//! Model test endpoint —— UI 上「测试」按钮的后端。
//!
//! 三种模式：
//!   - `connectivity`：GET `<base_url>/models`（base_url 不含版本段时自动
//!     补 `/v1`），检查连通性 + 凭证
//!   - `aiclient`：走 `latte_ai::client::AiClient::chat`，贴近生产语义
//!   - `http`：用 `reqwest` 直接 POST，绕过 AiClient 的中间处理
//!
//! 图片支持：图片输入（base64 data URL 列表）走 user content multimodal
//! blocks。图片生成：标准 chat completion 不支持，模块只做能力探测。
//!
//! 响应统一 `{ok, mode, latency_ms, response?, error?, status?, available_models?}`。
//! 这条路径不走 `ChatController`，不维护 history。

use std::time::Instant;

use latte_agent_core::config::ModelDef;
use latte_agent_core::model_resolver::resolve_env_vars;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TestMode {
    #[default]
    Connectivity,
    /// 前端发送的是 `aiclient`（无下划线），alias 保持兼容。
    #[serde(alias = "aiclient")]
    AiClient,
    Http,
}

#[derive(Debug, Deserialize)]
pub struct TestModelRequest {
    pub def: ModelDef,
    #[serde(default)]
    pub mode: TestMode,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub images: Vec<String>,
    #[serde(default)]
    pub probe_path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TestModelResponse {
    pub ok: bool,
    pub mode: String,
    pub latency_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_models: Option<Vec<String>>,
    /// 本次实际下发的输出上限（`None` = 该字段未下发，由厂商默认值决定）。
    ///
    /// 存在的理由：`max_tokens` 配错时厂商回 4xx，而在这个字段出现之前
    /// 测试面板写死 `Some(1024)`，于是**永远测不出配置里的值有问题** ——
    /// 面板显示「通了」，真实 chat 用配置值照样 400。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens_sent: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct ModelCapabilities {
    pub supports_image_input: bool,
    pub supports_image_generation: bool,
    pub latency_ms: u64,
}

pub async fn run_test(req: TestModelRequest) -> TestModelResponse {
    let started = Instant::now();
    let (label, mut resp) = match req.mode {
        TestMode::Connectivity => ("connectivity", test_connectivity(&req, started).await),
        TestMode::AiClient => ("aiclient", test_via_aiclient(&req, started).await),
        TestMode::Http => ("http", test_via_raw_http(&req, started).await),
    };
    resp.mode = label.to_string();
    resp
}

async fn test_connectivity(req: &TestModelRequest, started: Instant) -> TestModelResponse {
    let base = req.def.base_url.trim_end_matches('/');
    // 显式 probe_path 按字面拼接（用户明确意图）；默认走智能拼接，
    // base_url 已含 /v1 时不再重复。
    let url = match &req.probe_path {
        Some(p) => format!("{base}{p}"),
        None => join_endpoint(base, "/models"),
    };
    let client = match reqwest_client() {
        Ok(c) => c,
        Err(e) => return err_response(started, e),
    };
    let resp = match client
        .get(&url)
        .bearer_auth(resolve_env_vars(&req.def.api_key))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return err_response(started, e),
    };
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let available_models = if status.is_success() {
        extract_model_list(&body)
    } else {
        None
    };
    let body_excerpt = excerpt(&body, 500);
    if status.is_success() {
        TestModelResponse {
            ok: true,
            mode: String::new(),
            latency_ms: started.elapsed().as_millis() as u64,
            response: Some(format!("{status}（{body_excerpt}）")),
            error: None,
            status: Some(status.as_u16()),
            available_models,
            max_tokens_sent: None,
        }
    } else {
        TestModelResponse {
            ok: false,
            mode: String::new(),
            latency_ms: started.elapsed().as_millis() as u64,
            response: Some(body_excerpt),
            error: Some(format!("HTTP {status}")),
            status: Some(status.as_u16()),
            available_models,
            max_tokens_sent: None,
        }
    }
}

async fn test_via_aiclient(req: &TestModelRequest, started: Instant) -> TestModelResponse {
    let model = match build_latte_ai_model(&req.def) {
        Ok(m) => m,
        Err(e) => return err_response(started, e),
    };
    let client = match latte_ai::client::AiClient::new(model) {
        Ok(c) => c,
        Err(e) => return err_response(started, e),
    };
    let user_msg = match build_user_message(&req.prompt, &req.images) {
        Ok(m) => m,
        Err(e) => return err_response(started, e),
    };
    let history: Vec<latte_ai::models::Message> = vec![user_msg];
    // 关键：**不显式给 max_tokens**，让 AiClient 按生产路径解析
    // （目录值 → 原样下发）。写死一个测试专用的小值等于绕开被测对象。
    let params = latte_ai::params::GenerateParams::default();
    let sent = client.resolved_max_tokens();
    let result = client.chat(&history, &params).await;
    let latency = started.elapsed().as_millis() as u64;
    match result {
        Ok(text) => TestModelResponse {
            ok: true,
            mode: String::new(),
            latency_ms: latency,
            response: Some(text.content),
            error: None,
            status: Some(200),
            available_models: None,
            max_tokens_sent: sent,
        },
        Err(e) => TestModelResponse {
            ok: false,
            mode: String::new(),
            latency_ms: latency,
            response: None,
            error: Some(format!("{e}")),
            status: None,
            available_models: None,
            max_tokens_sent: sent,
        },
    }
}

async fn test_via_raw_http(req: &TestModelRequest, started: Instant) -> TestModelResponse {
    let client = match reqwest_client() {
        Ok(c) => c,
        Err(e) => return err_response(started, e),
    };
    let base = req.def.base_url.trim_end_matches('/');
    let url = if req.def.api.eq_ignore_ascii_case("anthropic") {
        join_endpoint(base, "/messages")
    } else {
        join_endpoint(base, "/chat/completions")
    };
    let model_id = if req.def.name.is_empty() {
        "test-model"
    } else {
        req.def.name.as_str()
    };
    // 用**配置里的**上限，而不是测试专用的小值 —— 目的正是让配错的值在
    // 这里就暴露。0 视为未配置。
    let raw_cap = (req.def.max_tokens > 0).then_some(req.def.max_tokens);
    let is_anthropic = req.def.api.eq_ignore_ascii_case("anthropic");
    // Anthropic 必填，没配就兜 4096；OpenAI 没配则整个不发该字段。
    let reported_cap = if is_anthropic { Some(raw_cap.unwrap_or(4_096)) } else { raw_cap };
    let body = if is_anthropic {
        serde_json::json!({
            // Anthropic 要求必填；用配置值，配错了这次测试就该失败。
            "model": model_id,
            "max_tokens": raw_cap.unwrap_or(4_096),
            "messages": [{
                "role": "user",
                "content": build_anthropic_content(&req.prompt, &req.images),
            }],
        })
    } else {
        serde_json::json!({
            "model": model_id,
            "messages": [{
                "role": "user",
                "content": build_openai_content(&req.prompt, &req.images),
            }],
        })
    };
    let resp = match client
        .post(&url)
        .bearer_auth(resolve_env_vars(&req.def.api_key))
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return err_response(started, e),
    };
    let status = resp.status();
    let raw = resp.text().await.unwrap_or_default();
    let latency = started.elapsed().as_millis() as u64;
    if status.is_success() {
        let text = extract_chat_text(&raw, req.def.api.as_str());
        TestModelResponse {
            ok: true,
            mode: String::new(),
            latency_ms: latency,
            response: Some(text),
            error: None,
            status: Some(status.as_u16()),
            available_models: None,
            max_tokens_sent: reported_cap,
        }
    } else {
        TestModelResponse {
            ok: false,
            mode: String::new(),
            latency_ms: latency,
            response: Some(excerpt(&raw, 500)),
            error: Some(format!("HTTP {status}")),
            status: Some(status.as_u16()),
            available_models: None,
            max_tokens_sent: reported_cap,
        }
    }
}

pub async fn probe_capabilities(req: &TestModelRequest) -> ModelCapabilities {
    let started = Instant::now();
    let supports_image_input = req.def.supports_vision
        || matches!(
            req.def.name.to_lowercase().as_str(),
            "gpt-4-vision-preview"
                | "gpt-4o"
                | "gpt-4o-mini"
                | "claude-3-opus"
                | "claude-3-sonnet"
                | "claude-3-haiku"
                | "claude-3-5-sonnet"
                | "claude-3-5-haiku"
                | "gemini-1.5-pro"
                | "gemini-1.5-flash"
                | "qwen-vl-max"
        );
    let supports_image_generation = matches!(
        req.def.name.to_lowercase().as_str(),
        "dall-e-3" | "dall-e-2" | "stable-diffusion" | "midjourney" | "imagen-3"
    );
    ModelCapabilities {
        supports_image_input,
        supports_image_generation,
        latency_ms: started.elapsed().as_millis() as u64,
    }
}

// ─── helpers ───────────────────────────────────────────────────

/// 拼接 API endpoint：base_url 的最后一段已是版本号（`v1`、`v1beta`、
/// `v2`…）时直接拼资源路径，否则补 `/v1` 再拼。兼容两种配置习惯：
/// `https://api.deepseek.com` 与 `https://yuanyuaicloud.cn/v1`。
fn join_endpoint(base_url: &str, resource: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let last = base.rsplit('/').next().unwrap_or("");
    let has_version = {
        let l = last.to_ascii_lowercase();
        l.len() > 1 && l.starts_with('v') && l.as_bytes()[1].is_ascii_digit()
    };
    if has_version {
        format!("{base}{resource}")
    } else {
        format!("{base}/v1{resource}")
    }
}

fn reqwest_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(anyhow::Error::from)
}

fn build_latte_ai_model(def: &ModelDef) -> anyhow::Result<latte_ai::models::Model> {
    use latte_ai::models::{ApiType, Model};
    let api = match def.api.to_lowercase().as_str() {
        "openai" | "openai-completions" => ApiType::OpenAiCompletions,
        "anthropic" | "anthropic-messages" => ApiType::AnthropicMessages,
        other => {
            return Err(anyhow::anyhow!(
                "unsupported api type '{}'（仅 openai / anthropic）",
                other
            ));
        }
    };
    Ok(Model {
        id: def.name.clone(),
        name: def.name.clone(),
        api,
        provider: def.provider.clone(),
        base_url: def.base_url.clone(),
        api_key: resolve_env_vars(&def.api_key),
        context_window: def.context_window,
        max_tokens: def.max_tokens,
        omit_max_tokens: def.omit_max_tokens,
        max_tokens_field: def.max_tokens_field,
        supports_thinking: def.supports_thinking,
        supports_vision: def.supports_vision,
        cost_per_million_input: def.cost_per_million_input.unwrap_or(0.0),
        cost_per_million_output: def.cost_per_million_output.unwrap_or(0.0),
        timeout_secs: def.timeout_secs,
    })
}

fn build_user_message(
    text: &str,
    images: &[String],
) -> anyhow::Result<latte_ai::models::Message> {
    use latte_ai::models::{ContentPart, Message, Role};
    if images.is_empty() {
        return Ok(Message::text(Role::User, text));
    }
    let mut parts = Vec::with_capacity(1 + images.len());
    if !text.is_empty() {
        parts.push(ContentPart::text(text));
    }
    for img in images {
        let (media_type, b64) = parse_data_url(img)?;
        let bytes = base64_decode(&b64)?;
        parts.push(ContentPart::image(media_type, bytes));
    }
    Ok(Message {
        role: Role::User,
        content: parts,
        tool_call_id: None,
        tool_calls: None,
    })
}

fn build_openai_content(text: &str, images: &[String]) -> serde_json::Value {
    if images.is_empty() {
        return serde_json::Value::String(text.to_string());
    }
    let mut parts: Vec<serde_json::Value> = Vec::with_capacity(1 + images.len());
    if !text.is_empty() {
        parts.push(serde_json::json!({"type": "text", "text": text}));
    }
    for img in images {
        parts.push(serde_json::json!({
            "type": "image_url",
            "image_url": {"url": img},
        }));
    }
    serde_json::Value::Array(parts)
}

fn build_anthropic_content(text: &str, images: &[String]) -> serde_json::Value {
    if images.is_empty() {
        return serde_json::Value::String(text.to_string());
    }
    let mut parts: Vec<serde_json::Value> = Vec::with_capacity(1 + images.len());
    if !text.is_empty() {
        parts.push(serde_json::json!({"type": "text", "text": text}));
    }
    for img in images {
        if let Some((media_type, b64)) = parse_data_url_loose(img) {
            parts.push(serde_json::json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": media_type,
                    "data": b64,
                },
            }));
        } else {
            parts.push(serde_json::json!({"type": "text", "text": img}));
        }
    }
    serde_json::Value::Array(parts)
}

fn parse_data_url(s: &str) -> anyhow::Result<(String, String)> {
    let s = s.trim();
    let rest = s.strip_prefix("data:").ok_or_else(|| {
        anyhow::anyhow!("image must be a data URL (data:image/...;base64,...)")
    })?;
    let (media_type, data) = rest
        .split_once(";base64,")
        .ok_or_else(|| anyhow::anyhow!("data URL missing `;base64,` separator"))?;
    Ok((media_type.to_string(), data.to_string()))
}

fn parse_data_url_loose(s: &str) -> Option<(String, String)> {
    parse_data_url(s).ok()
}

fn base64_decode(s: &str) -> anyhow::Result<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| anyhow::anyhow!("invalid base64: {e}"))
}

fn extract_model_list(body: &str) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let arr = v.get("data")?.as_array()?;
    let ids: Vec<String> = arr
        .iter()
        .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(String::from))
        .collect();
    if ids.is_empty() { None } else { Some(ids) }
}

fn extract_chat_text(body: &str, api: &str) -> String {
    if api.eq_ignore_ascii_case("anthropic") {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
            if let Some(text) = v
                .get("content")
                .and_then(|c| c.as_array())
                .and_then(|arr| arr.first())
                .and_then(|p| p.get("text"))
                .and_then(|t| t.as_str())
            {
                return text.to_string();
            }
        }
    } else if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(text) = v
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|t| t.as_str())
        {
            return text.to_string();
        }
    }
    excerpt(body, 1000)
}

fn excerpt(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

fn err_response<E: std::fmt::Display>(started: Instant, e: E) -> TestModelResponse {
    TestModelResponse {
        ok: false,
        mode: String::new(),
        latency_ms: started.elapsed().as_millis() as u64,
        response: None,
        error: Some(format!("{e}")),
        status: None,
        available_models: None,
        // 这条路径是「请求都没发出去」（构造/网络失败），没有下发值可报。
        max_tokens_sent: None,
    }
}

// ─── Tool test ─────────────────────────────────────────────────────

/// `POST /api/tools/test` 的请求体。
#[derive(Debug, Deserialize)]
pub struct TestToolRequest {
    /// 工具 ID（如 `read`、`exec`、`write` 等）。
    pub tool_id: String,
    /// 测试参数（JSON 格式）。不同工具预期不同形状：
    /// - `read`：`{"path": "Cargo.toml"}`
    /// - `exec`：`{"command": "echo hello"}`
    /// - `write`：`{"path": "/tmp/test.txt", "content": "hello"}`
    pub args: serde_json::Value,
}

/// `POST /api/tools/test` 的响应体。
#[derive(Debug, Serialize)]
pub struct TestToolResponse {
    pub ok: bool,
    pub tool_id: String,
    pub latency_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 运行一次工具测试：**用运行时那套 tool manager**（builtin + `code_graph`
/// + 文档增强 + MCP 代理），执行指定工具，返回结果。不走 ChatController，
/// 不维护 context。
///
/// 特意复用 `controller::build_tool_manager_at` 而不是自己 `create_tool_manager()`
/// + `register_package`：后者少了 `code_graph`、少了 `.latte/tools.d` 增强、
/// 也少了 `mcp_connect` 的包装，于是「面板里测通了」不等于「模型那边能用」。
/// 顺带让面板的「测试」按钮能真正用来连 MCP server —— 连上之后
/// `GET /api/tools` 立刻能列出外部工具。
pub async fn run_tool_test(req: TestToolRequest, cwd: &std::path::Path) -> TestToolResponse {
    use latte_rs_agent_tools::types::ToolManager as _;
    let started = Instant::now();
    let tool_id = &req.tool_id;

    // 1. 建 manager：allowed 给「当前枚举出来的全部工具 + 别名」，等价于不过滤。
    let mut allowed: Vec<String> = crate::tools::enumerate()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|t| t.id)
        .collect();
    allowed.push(tool_id.clone());
    for (alias, members) in latte_agent_core::tool_docs::TOOL_ALIAS_GROUPS {
        allowed.push((*alias).to_string());
        allowed.extend(members.iter().map(|m| (*m).to_string()));
    }
    let mgr = match latte_agent_core::controller::build_tool_manager_at(cwd, &allowed).await {
        Ok(mgr) => mgr,
        Err(e) => {
            return TestToolResponse {
                ok: false,
                tool_id: tool_id.clone(),
                latency_ms: started.elapsed().as_millis() as u64,
                response: None,
                error: Some(format!("构建 tool manager 失败: {e}")),
            }
        }
    };

    // 2. 别名解析：bash → exec（tools crate 里 shell 包的注册名）
    let resolved = match tool_id.as_str() {
        "bash" if mgr.get_tool("bash").is_none() => "exec".to_string(),
        id => id.to_string(),
    };

    // 3. 查找工具
    let full_name = mgr
        .get_tool(&resolved)
        .map(|_| resolved.clone())
        .or_else(|| {
            mgr.get_tool_names().into_iter().find(|n| {
                n.rsplit_once('.').map(|(_, s)| s) == Some(resolved.as_str())
            })
        });

    let Some(full_name) = full_name else {
        let all = mgr.get_tool_names();
        let err = format!("未找到工具「{tool_id}」。可用工具: {}", all.join(", "));
        return TestToolResponse {
            ok: false,
            tool_id: tool_id.clone(),
            latency_ms: started.elapsed().as_millis() as u64,
            response: None,
            error: Some(err),
        };
    };

    // 4. 构造 test context
    let mut ctx = latte_rs_agent_tools::types::ToolExecutionContext::fresh(&full_name, 1);
    ctx.metadata = Some(serde_json::json!({
        "cwd": cwd.display().to_string(),
    }));

    // 5. 执行
    match mgr.execute(&full_name, req.args, Some(ctx)).await {
        Ok(val) => {
            let text = serde_json::to_string_pretty(&val).unwrap_or_else(|_| format!("{val:?}"));
            TestToolResponse {
                ok: true,
                tool_id: tool_id.clone(),
                latency_ms: started.elapsed().as_millis() as u64,
                response: Some(text),
                error: None,
            }
        }
        Err(e) => TestToolResponse {
            ok: false,
            tool_id: tool_id.clone(),
            latency_ms: started.elapsed().as_millis() as u64,
            response: None,
            error: Some(format!("{e}")),
        },
    }
}
// ─── Role test ─────────────────────────────────────────────────────

/// `POST /api/roles/test` 的请求体。
#[derive(Deserialize)]
pub struct TestRoleRequest {
    /// 角色 ID。
    pub role_id: String,
    /// 角色配置（完整的角色定义）。
    pub config: crate::api::SaveRoleConfigRequest,
}

/// `POST /api/roles/test` 的响应体。
#[derive(Debug, Serialize)]
pub struct TestRoleResponse {
    pub ok: bool,
    pub role_id: String,
    pub latency_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
pub async fn run_role_test(
    req: TestRoleRequest,
    resolver: &latte_agent_core::model_resolver::ModelResolver,
    _cwd: &std::path::Path,
) -> TestRoleResponse {
    let started = Instant::now();
    let role_id = req.role_id.clone();
    let config = req.config;

    // 1. 选择 model：优先用 model_chain 的第一个，否则按 tier 解析
    let model_id = config
        .model_chain
        .first()
        .cloned()
        .unwrap_or_default();
    let tier = latte_agent_core::model_resolver::ModelTier::parse(&config.model_tier)
        .unwrap_or(latte_agent_core::model_resolver::ModelTier::Standard);

    let resolved = if !model_id.is_empty() {
        resolver.resolve_id_or_name(&model_id)
    } else {
        resolver.resolve(&role_id, tier)
    };

    let resolved = match resolved {
        Ok(m) => m,
        Err(e) => {
            return TestRoleResponse {
                ok: false,
                role_id,
                latency_ms: started.elapsed().as_millis() as u64,
                response: None,
                error: Some(format!("无法解析 model: {e}")),
            };
        }
    };

    // 2. 构造 AiClient（直接传入 Model）
    let client = match latte_ai::client::AiClient::new(resolved) {
        Ok(client) => client,
        Err(e) => {
            return TestRoleResponse {
                ok: false,
                role_id,
                latency_ms: started.elapsed().as_millis() as u64,
                response: None,
                error: Some(format!("创建 AiClient 失败: {e}")),
            };
        }
    };

    // 3. 发一条测试消息
    let messages = vec![
        latte_ai::models::Message::system(config.prompt.clone()),
        latte_ai::models::Message::user("你好，请用一句话介绍你自己。"),
    ];
    let params = latte_ai::params::GenerateParams {
        temperature: config.temperature,
        max_tokens: Some(150),
        ..Default::default()
    };

    match client.chat(&messages, &params).await {
        Ok(response) => {
            let text = if response.content.is_empty() {
                "（无返回内容）".to_string()
            } else {
                response.content
            };
            TestRoleResponse {
                ok: true,
                role_id,
                latency_ms: started.elapsed().as_millis() as u64,
                response: Some(excerpt(&text, 500)),
                error: None,
            }
        }
        Err(e) => TestRoleResponse {
            ok: false,
            role_id,
            latency_ms: started.elapsed().as_millis() as u64,
            response: None,
            error: Some(format!("{e}")),
        },
    }
}

// ─── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn def_at(uri: &str, api: &str, max_tokens: u32) -> ModelDef {
        ModelDef {
            name: "probe".into(),
            api: api.into(),
            provider: "test".into(),
            base_url: uri.into(),
            api_key: "k".into(),
            context_window: 1_000_000,
            max_tokens,
            omit_max_tokens: false,
            max_tokens_field: Default::default(),
            supports_thinking: false,
            supports_vision: false,
            supports_image_generation: false,
            cost_per_million_input: Some(0.0),
            cost_per_million_output: Some(0.0),
            tier: None,
            timeout_secs: None,
        }
    }

    /// 「测试模型」必须用**配置里的**输出上限发请求，并把该值报回前端。
    ///
    /// 回归：这个面板以前在 aiclient / http 两个模式里都写死
    /// `max_tokens: 1024`。后果是配置里那个值**从来没被测到过** —— 面板显示
    /// 「✓ 通过」，真实 chat 用配置值照样可能被厂商 4xx 拒。既然现在配置值
    /// 是原样下发的（没有天花板兜着了），这个面板就必须测真实值才有意义。
    #[tokio::test]
    async fn model_test_uses_the_configured_cap_and_reports_it() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(
                        r#"{"id":"c","object":"chat.completion","created":0,"model":"probe",
                            "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},
                            "finish_reason":"stop"}],
                            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
                    )),
            )
            .await;

        let resp = run_test(TestModelRequest {
            def: def_at(&server.uri(), "openai", 384_000),
            mode: TestMode::AiClient,
            prompt: "hi".into(),
            images: Vec::new(),
            probe_path: None,
        })
        .await;

        assert!(resp.ok, "测试应通过: {resp:?}");
        assert_eq!(
            resp.max_tokens_sent,
            Some(384_000),
            "必须把实际下发值报回前端，否则用户无从判断上限配得合不合理"
        );

        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(
            body.get("max_tokens").and_then(|v| v.as_u64()),
            Some(384_000),
            "出站请求必须带配置值，不能是写死的 1024: {body}"
        );
    }

    /// 目录没配（`0`）时，OpenAI 模式不下发该字段，报回的也是 `None`
    /// —— 前端据此显示「用厂商默认」，而不是显示一个假的数。
    #[tokio::test]
    async fn model_test_reports_none_when_cap_unset() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(
                        r#"{"id":"c","object":"chat.completion","created":0,"model":"probe",
                            "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},
                            "finish_reason":"stop"}],
                            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
                    )),
            )
            .await;

        let resp = run_test(TestModelRequest {
            def: def_at(&server.uri(), "openai", 0),
            mode: TestMode::AiClient,
            prompt: "hi".into(),
            images: Vec::new(),
            probe_path: None,
        })
        .await;

        assert!(resp.ok, "测试应通过: {resp:?}");
        assert_eq!(resp.max_tokens_sent, None, "未配置就不该报一个数");

        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert!(body.get("max_tokens").is_none(), "未配置应省略字段: {body}");
    }

    #[test]
    fn join_endpoint_avoids_double_version_segment() {
        // base_url 已含 /v1 → 不重复
        assert_eq!(
            join_endpoint("https://yuanyuaicloud.cn/v1", "/chat/completions"),
            "https://yuanyuaicloud.cn/v1/chat/completions"
        );
        assert_eq!(
            join_endpoint("https://api.kimi.com/coding/v1", "/models"),
            "https://api.kimi.com/coding/v1/models"
        );
        // base_url 不含版本段 → 补 /v1
        assert_eq!(
            join_endpoint("https://api.deepseek.com", "/chat/completions"),
            "https://api.deepseek.com/v1/chat/completions"
        );
        assert_eq!(
            join_endpoint("https://api.anthropic.com", "/messages"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            join_endpoint("http://localhost:11434", "/models"),
            "http://localhost:11434/v1/models"
        );
        // 尾部斜杠 / 其它版本号形式
        assert_eq!(
            join_endpoint("https://example.com/v2/", "/models"),
            "https://example.com/v2/models"
        );
        // 非版本号结尾路径段不误判
        assert_eq!(
            join_endpoint("https://example.com/api", "/models"),
            "https://example.com/api/v1/models"
        );
    }

    #[test]
    fn test_mode_deserializes_frontend_spellings() {
        let m: TestMode = serde_json::from_str("\"aiclient\"").unwrap();
        assert!(matches!(m, TestMode::AiClient));
        let m: TestMode = serde_json::from_str("\"ai_client\"").unwrap();
        assert!(matches!(m, TestMode::AiClient));
        let m: TestMode = serde_json::from_str("\"http\"").unwrap();
        assert!(matches!(m, TestMode::Http));
        let m: TestMode = serde_json::from_str("\"connectivity\"").unwrap();
        assert!(matches!(m, TestMode::Connectivity));
    }
}
