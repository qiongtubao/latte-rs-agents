//! e2e 共享工具：本地模型 mock + 完全隔离的配置。
//!
//! # 为什么需要
//!
//! CLI 侧的 e2e 原来不提供模型配置，于是走全局 `~/.latte/models.yaml`
//! 打**真实 API**。这带来两类问题：
//!
//! 1. **不确定**：断言的其实是模型行为。实测 `hil_v11_e2e` 6 次跑 2 次
//!    失败（一次是 reviewer 自主调 `ask_human` 把 session 暂停了），
//!    `hil_v12_e2e` 则稳定失败在 "programmer has no messages"。它们本
//!    该测的是调度与历史回写，不是模型听不听话。
//! 2. **慢**：单个测试 27–270 秒，全套跑一遍要十几分钟。
//!
//! # 隔离要点
//!
//! `LATTE_HOME` 必须指向临时目录。否则全局层里"独有的模型 id"会被
//! `push` 进**每个角色**的 `model_chain`（见 `config_layer.rs` 里
//! "global-only ids are also appended"），mock 失败时会 fallback 到真实
//! API，测试又变回不确定。

use std::path::Path;

/// 起一个本地 OpenAI 兼容 mock。返回 `(server, base_url)`；
/// **server 必须持有到测试结束**，drop 即关闭。
pub async fn start_model_mock(reply: &str) -> (wiremock::MockServer, String) {
    start_model_mock_delayed(reply, std::time::Duration::ZERO).await
}

/// 同上，但每次响应前先等 `delay`。
///
/// 用于需要"turn 正在跑"这个窗口的测试（比如验证 `/pause` 能否在 turn
/// 中途生效）——mock 秒回的话 turn 早就结束了，测试拿不到那个窗口。
pub async fn start_model_mock_delayed(
    reply: &str,
    delay: std::time::Duration,
) -> (wiremock::MockServer, String) {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let server = wiremock::MockServer::start().await;
    let body = serde_json::json!({
        "id": "chatcmpl-e2e",
        "object": "chat.completion",
        "created": 0,
        "model": "e2e-stub",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": reply },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
    })
    .to_string();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(body)
                .set_delay(delay),
        )
        .mount(&server)
        .await;
    let uri = server.uri();
    (server, uri)
}

/// 在 `home` 下写一份只含 mock 模型的配置，三个 tier 都指向它。
pub fn write_isolated_home(home: &Path, base_url: &str) {
    let models_d = home.join("models.d");
    std::fs::create_dir_all(&models_d).expect("create models.d");
    std::fs::write(
        models_d.join("stub.toml"),
        format!(
            r#"[[models]]
id = "e2e-stub"
name = "e2e-stub"
api = "openai"
provider = "e2e"
base_url = "{base_url}"
api_key = "test-key"
context_window = 32000
max_tokens = 4096
supports_thinking = false
tier = "premium"

[models.tiers]
premium = "e2e-stub"
standard = "e2e-stub"
budget = "e2e-stub"
"#
        ),
    )
    .expect("write stub.toml");
}
