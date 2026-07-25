//! Model test endpoint —— UI 上「测试」按钮的后端。
//!
//! 三种模式：
//!   - `connectivity`：GET `<base_url>/v1/models`，检查连通性 + 凭证
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
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TestMode {
    #[default]
    Connectivity,
    AiClient,
    Http,
}

#[derive(Debug, Deserialize)]
pub struct TestModelRequest {
    #[serde(flatten)]
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
    let probe_path = req
        .probe_path
        .clone()
        .unwrap_or_else(|| "/v1/models".to_string());
    let url = format!(
        "{}{}",
        req.def.base_url.trim_end_matches('/'),
        probe_path
    );
    let client = match reqwest_client() {
        Ok(c) => c,
        Err(e) => return err_response(started, e),
    };
    let resp = match client
        .get(&url)
        .bearer_auth(&req.def.api_key)
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
    let params = latte_ai::params::GenerateParams {
        max_tokens: Some(1024),
        ..Default::default()
    };
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
        },
        Err(e) => TestModelResponse {
            ok: false,
            mode: String::new(),
            latency_ms: latency,
            response: None,
            error: Some(format!("{e}")),
            status: None,
            available_models: None,
        },
    }
}

async fn test_via_raw_http(req: &TestModelRequest, started: Instant) -> TestModelResponse {
    let client = match reqwest_client() {
        Ok(c) => c,
        Err(e) => return err_response(started, e),
    };
    let url = format!(
        "{}{}",
        req.def.base_url.trim_end_matches('/'),
        if req.def.api.eq_ignore_ascii_case("anthropic") {
            "/v1/messages"
        } else {
            "/v1/chat/completions"
        }
    );
    let model_id = if req.def.name.is_empty() {
        "test-model"
    } else {
        req.def.name.as_str()
    };
    let body = if req.def.api.eq_ignore_ascii_case("anthropic") {
        serde_json::json!({
            "model": model_id,
            "max_tokens": 1024,
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
        .bearer_auth(&req.def.api_key)
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
        api_key: def.api_key.clone(),
        context_window: def.context_window,
        max_tokens: def.max_tokens,
        supports_thinking: def.supports_thinking,
        supports_vision: def.supports_vision,
        cost_per_million_input: def.cost_per_million_input.unwrap_or(0.0),
        cost_per_million_output: def.cost_per_million_output.unwrap_or(0.0),
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
    }
}
