//! 图片生成工具（`generate_image`）：designer 等角色通过 OpenAI 兼容的
//! `/v1/images/generations` 接口生图，落盘到 `<cwd>/.latte/images/`，
//! 并通过 `ChatEvent::ImageGenerated` 通知 UI 渲染 `<img>`。
//!
//! 模型选择：只使用 catalog 里 `supports_image_generation = true` 的
//! 模型（生图是独立 API，不是 chat completion，不能拿普通对话模型凑）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use latte_rs_agent_tools::types::ToolManager;
use serde_json::Value;

use crate::config::{AgentConfig, ModelDef};
use crate::controller::ChatEvent;

/// 没有配置生图模型时的中文引导。
const NO_IMAGE_MODEL_MSG: &str =
    "没有配置生图模型：请在模型管理中给 OpenAI 兼容的图片模型（如 cogview-3 / gpt-image-1）勾选 supports_image_generation";

/// 挑选用于生图的模型。
///
/// 顺序：
/// 1. `role_chain`（角色的 model_chain）里第一个支持生图的模型；
/// 2. 全局 fallback：catalog 里所有支持生图的模型按 name 排序取第一个；
/// 3. 都没有 → 中文引导错误。
pub fn pick_image_model(merged: &AgentConfig, role_chain: &[String]) -> Result<ModelDef, String> {
    for id in role_chain {
        if let Some(def) = merged.models.models.iter().find(|m| &m.name == id) {
            if def.supports_image_generation {
                return Ok(def.clone());
            }
        }
    }
    let mut capable: Vec<&ModelDef> = merged
        .models
        .models
        .iter()
        .filter(|m| m.supports_image_generation)
        .collect();
    capable.sort_by(|a, b| a.name.cmp(&b.name));
    capable
        .into_iter()
        .next()
        .cloned()
        .ok_or_else(|| NO_IMAGE_MODEL_MSG.to_string())
}

/// 生成文件名用的进程内计数器（配合时间戳避免同毫秒碰撞）。
static NAME_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `img-<millis_since_epoch>-<6 lowercase alnum>.png`。不引入 rand：
/// 时间戳 ^ 计数器 ^ pid 做一次 xorshift 混合后映射到 36 进制字母表。
fn image_file_name() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut x = millis
        ^ (NAME_COUNTER.fetch_add(1, Ordering::Relaxed).wrapping_mul(0x9E37_79B9_7F4A_7C15))
        ^ ((std::process::id() as u64) << 32);
    let mut suffix = String::with_capacity(6);
    for _ in 0..6 {
        // xorshift64*
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        let idx = (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 58) & 0x3F;
        let c = match idx % 36 {
            0..=9 => (b'0' + (idx % 36) as u8) as char,
            n => (b'a' + (n - 10) as u8) as char,
        };
        suffix.push(c);
    }
    format!("img-{millis}-{suffix}.png")
}

/// 调用 OpenAI 兼容的图片生成接口，把图片写到 `out_dir`，返回完整路径。
///
/// 仅支持 `def.api == "openai"`；其它 api 类型返回中文错误。
pub async fn generate_image(
    def: &ModelDef,
    prompt: &str,
    size: Option<&str>,
    out_dir: &Path,
) -> Result<PathBuf, String> {
    if def.api != "openai" {
        return Err(format!(
            "生图暂未支持 api 类型 '{}'，目前仅支持 openai 兼容接口",
            def.api
        ));
    }
    let api_key = crate::model_resolver::resolve_env_vars(&def.api_key);
    let url = format!(
        "{}/v1/images/generations",
        def.base_url.trim_end_matches('/')
    );
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| format!("创建 HTTP client 失败: {e}"))?;

    let body = serde_json::json!({
        "model": def.name,
        "prompt": prompt,
        "n": 1,
        "size": size.unwrap_or("1024x1024"),
        "response_format": "b64_json",
    });
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {api_key}"))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("生图请求失败: {e}"))?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("读取生图响应失败: {e}"))?;
    if !status.is_success() {
        let excerpt: String = text.chars().take(300).collect();
        return Err(format!("生图接口返回 {status}: {excerpt}"));
    }
    let json: Value =
        serde_json::from_str(&text).map_err(|e| format!("解析生图响应 JSON 失败: {e}"))?;
    let first = json
        .get("data")
        .and_then(|d| d.get(0))
        .ok_or_else(|| format!("生图响应缺少 data[0]: {}", &text[..text.len().min(300)]))?;

    // 优先 b64_json，否则退化为 url 再 GET 一次。
    let bytes: Vec<u8> = if let Some(b64) = first.get("b64_json").and_then(|v| v.as_str()) {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| format!("解码 b64_json 失败: {e}"))?
    } else if let Some(img_url) = first.get("url").and_then(|v| v.as_str()) {
        let img = client
            .get(img_url)
            .send()
            .await
            .map_err(|e| format!("下载生成的图片失败: {e}"))?;
        if !img.status().is_success() {
            return Err(format!("下载生成的图片失败: HTTP {}", img.status()));
        }
        img.bytes()
            .await
            .map_err(|e| format!("读取图片字节失败: {e}"))?
            .to_vec()
    } else {
        return Err(format!(
            "生图响应 data[0] 既没有 b64_json 也没有 url: {}",
            &text[..text.len().min(300)]
        ));
    };

    std::fs::create_dir_all(out_dir).map_err(|e| format!("创建图片目录失败: {e}"))?;
    let path = out_dir.join(image_file_name());
    std::fs::write(&path, &bytes).map_err(|e| format!("写入图片文件失败: {e}"))?;
    Ok(path)
}

/// 把 `generate_image` 工具注册到指定角色的 tool manager。
///
/// 与 controller 里的 `register_workflow_tool` 同构：schema + 捕获
/// 上下文的 `SharedToolHandler`。注册时就把角色的 model_chain 拷出来
/// （merged 会被 UI 的角色编辑器整体替换，闭包只认注册时的快照语义
/// 与其它工具一致——merged 本身按 Arc 共享）。
pub fn register_generate_image_tool(
    tm: &Arc<dyn ToolManager>,
    merged: &AgentConfig,
    event_tx: tokio::sync::broadcast::Sender<ChatEvent>,
    cwd: PathBuf,
    role_id: String,
) -> Result<(), Box<dyn std::error::Error>> {
    use latte_rs_agent_tools::types::{
        PropertyType, SchemaType, SharedToolHandler, Tool, ToolInputProperty, ToolInputSchema,
    };

    let role_chain: Vec<String> = merged
        .roles
        .get(&role_id)
        .map(|r| r.model_chain.clone())
        .unwrap_or_default();
    let merged_owned = Arc::new(merged.clone());

    let input_schema = ToolInputSchema {
        schema_type: SchemaType,
        properties: vec![
            ("prompt".into(), ToolInputProperty {
                property_type: PropertyType::String,
                description: Some("图片描述（画面内容、风格、构图）".into()),
                enum_values: None,
                minimum: None,
                maximum: None,
                min_length: None,
                max_length: None,
                items: None,
                properties: None,
                required: None,
                additional_properties: None,
                ref_: None,
            }),
            ("size".into(), ToolInputProperty {
                property_type: PropertyType::String,
                description: Some("尺寸，如 1024x1024".into()),
                enum_values: None,
                minimum: None,
                maximum: None,
                min_length: None,
                max_length: None,
                items: None,
                properties: None,
                required: None,
                additional_properties: None,
                ref_: None,
            }),
        ]
        .into_iter()
        .collect(),
        required: Some(vec!["prompt".into()]),
        ..Default::default()
    };

    let handler_role_id = role_id.clone();
    let handler: SharedToolHandler = Arc::new(move |input: Value, _ctx| {
        let merged = Arc::clone(&merged_owned);
        let role_chain = role_chain.clone();
        let event_tx = event_tx.clone();
        let out_dir = cwd.join(".latte/images");
        let role_id = handler_role_id.clone();
        Box::pin(async move {
            let tool_err = |msg: String| latte_rs_agent_tools::error::ToolError::Other(msg);

            let prompt = input
                .get("prompt")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tool_err("missing 'prompt' field".into()))?
                .to_string();
            let size = input
                .get("size")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            let def = pick_image_model(&merged, &role_chain).map_err(tool_err)?;
            let path = generate_image(&def, &prompt, size.as_deref(), &out_dir)
                .await
                .map_err(tool_err)?;
            let file = path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .ok_or_else(|| tool_err(format!("生成的图片路径无效: {}", path.display())))?;

            let _ = event_tx.send(ChatEvent::ImageGenerated {
                role_id: role_id.clone(),
                path: format!("/api/images/{file}"),
                prompt: prompt.clone(),
            });

            Ok(Value::String(format!(
                "图片已生成：/api/images/{file}（已保存 .latte/images/{file}）。请在回复中用 ![描述](/api/images/{file}) 引用展示它。"
            )))
        })
    });

    let tool = Tool::builder(
        "generate_image".to_string(),
        "生成一张图片（OpenAI 兼容图片接口），保存到 .latte/images/ 并在聊天中显示。参数 prompt 描述画面。"
            .to_string(),
        input_schema,
        handler,
    )
    // 生成后落盘到 .latte/images/，串行。
    .concurrency_safe(false)
    .build();

    tm.register(tool, Some(&role_id));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelCatalog;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn image_def(name: &str, base_url: &str) -> ModelDef {
        ModelDef {
            name: name.into(),
            api: "openai".into(),
            provider: "test".into(),
            base_url: base_url.into(),
            api_key: "test-key".into(),
            context_window: 32000,
            max_tokens: 4096,
            omit_max_tokens: false,
            max_tokens_field: Default::default(),
            supports_thinking: false,
            supports_vision: false,
            supports_image_generation: true,
            cost_per_million_input: None,
            cost_per_million_output: None,
            tier: None,
            timeout_secs: None,
        }
    }

    fn config_with(defs: Vec<ModelDef>) -> AgentConfig {
        let mut cfg = AgentConfig::default();
        cfg.models = ModelCatalog {
            models: defs,
            tiers: None,
            role_tiers: None,
        };
        cfg
    }

    /// 最小的合法 PNG 头几个字节（测试只校验字节透传，不校验是真图）。
    const PNG_BYTES: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

    #[tokio::test]
    async fn generate_image_b64_writes_exact_bytes() {
        let server = MockServer::start().await;
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(PNG_BYTES);
        Mock::given(method("POST"))
            .and(path("/v1/images/generations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{ "b64_json": b64 }]
            })))
            .mount(&server)
            .await;

        let def = image_def("gpt-image-1", &server.uri());
        let out_dir = tempfile::tempdir().unwrap();
        let path = generate_image(&def, "a cat", None, out_dir.path())
            .await
            .unwrap();

        assert!(path.exists());
        assert_eq!(std::fs::read(&path).unwrap(), PNG_BYTES);
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("img-"));
        assert!(name.ends_with(".png"));
        assert_eq!(path.parent().unwrap(), out_dir.path());
    }

    #[tokio::test]
    async fn generate_image_url_form_downloads_bytes() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/images/generations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{ "url": format!("{}/img.png", server.uri()) }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/img.png"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(PNG_BYTES.to_vec()))
            .mount(&server)
            .await;

        let def = image_def("gpt-image-1", &server.uri());
        let out_dir = tempfile::tempdir().unwrap();
        let path = generate_image(&def, "a dog", Some("512x512"), out_dir.path())
            .await
            .unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), PNG_BYTES);
    }

    #[tokio::test]
    async fn generate_image_non_2xx_err_contains_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/images/generations"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let def = image_def("gpt-image-1", &server.uri());
        let out_dir = tempfile::tempdir().unwrap();
        let err = generate_image(&def, "x", None, out_dir.path())
            .await
            .unwrap_err();
        assert!(err.contains("500"), "err: {err}");
    }

    #[tokio::test]
    async fn generate_image_rejects_non_openai_api() {
        let mut def = image_def("cogview-3", "http://localhost");
        def.api = "anthropic".into();
        let out_dir = tempfile::tempdir().unwrap();
        let err = generate_image(&def, "x", None, out_dir.path())
            .await
            .unwrap_err();
        assert!(err.contains("暂未支持"), "err: {err}");
        assert!(err.contains("anthropic"), "err: {err}");
    }

    #[test]
    fn pick_image_model_prefers_role_chain_order() {
        let mut chain_first = image_def("b-image", "http://x");
        let mut other = image_def("a-image", "http://x");
        // 全局排序上 a-image 在前，但 role_chain 指定了 b-image → chain 优先。
        chain_first.provider = "p1".into();
        other.provider = "p2".into();
        let cfg = config_with(vec![other, chain_first]);
        let picked = pick_image_model(&cfg, &["missing".into(), "b-image".into()]).unwrap();
        assert_eq!(picked.name, "b-image");
    }

    #[test]
    fn pick_image_model_falls_back_sorted_by_name() {
        let cfg = config_with(vec![
            image_def("z-image", "http://x"),
            image_def("a-image", "http://x"),
        ]);
        let picked = pick_image_model(&cfg, &[]).unwrap();
        assert_eq!(picked.name, "a-image");
    }

    #[test]
    fn pick_image_model_skips_non_capable_chain_models() {
        let mut chat_only = image_def("chat-model", "http://x");
        chat_only.supports_image_generation = false;
        let cfg = config_with(vec![chat_only, image_def("img-model", "http://x")]);
        let picked = pick_image_model(&cfg, &["chat-model".into()]).unwrap();
        assert_eq!(picked.name, "img-model");
    }

    #[test]
    fn pick_image_model_none_configured_gives_guidance() {
        let cfg = config_with(vec![]);
        let err = pick_image_model(&cfg, &[]).unwrap_err();
        assert!(err.contains("没有配置生图模型"), "err: {err}");
        assert!(err.contains("supports_image_generation"), "err: {err}");
    }
}
