//! `latte-agent test` — 一句话模型快速验证。
//!
//! 解决两个问题：
//! 1. 排查 "这个模型为什么不能用" 时不想起一个 REPL。
//! 2. `chat -m` 走完整 REPL（turn / cooldown / session / trace），
//!    一次失败后 cooldown 会污染后续调用，定位非常痛苦。
//!
//! 这里**绕过 `Agent` 的 cooldown / fallback 链**直连 `AiClient`，
//! 不开 session、不写 trace，立刻返回 vendor 真实响应 / 错误。
//! `--raw` 模式更进一步跳过 `AiClient`，裸跑 reqwest 把 vendor 原始
//! 字节序列打到屏幕上 —— 用来定位 "是 vendor / 网络问题，还是
//! `AiClient` 集成层 bug"。
//!
//! 用法：
//!   latte-agent test -m deepseek-v4-flash 'hi'
//!   latte-agent test -m MiniMax-M3 'summarize this' --system 'You are concise.'
//!   latte-agent test -m some-model 'hi' --api-key sk-xxx
//!   latte-agent test --raw -m MiniMax-M3 'hi'    # 跳过 AiClient，原始 HTTP 调试
//!
//! 退出码：
//!   0 — 成功（raw 模式下：拿到任何 HTTP 响应都算 0）
//!   1 — 模型调用失败（auth / 4xx / 5xx / 解码失败 / 网络错误）
//!   2 — 参数 / 配置 / 模型未找到
use clap::Args;
use latte_agent_core::trace::ModelErrorKind;
use latte_ai::client::AiClient;
use latte_ai::error::AiError;
use latte_ai::models::{Completion, Message};
use latte_ai::params::GenerateParams;

use super::config_layer::{self, CliOverrides};

/// 一句话调用一个模型，输出响应或错误后立即退出。
///
/// 与 `chat -m` 的区别：
/// - **不进入 REPL**：跑完一次就退出，方便脚本化 (`latte-agent test ... && echo ok`)。
/// - **不写 session / trace / chatlog**：避免一次失败污染后续调用。
/// - **绕过 `Agent` 的 cooldown / fallback 链**：直接调 `AiClient::chat`，
///   调用者拿到的是 vendor 的真实错误（不是 "all models unavailable"）。
/// - **不暴露 role / tier / 系统级配置**：纯单点测试。
#[derive(Args, Debug)]
pub struct TestCmd {
    /// 要测试的模型 id 或 display name（与 `chat -m` 完全一致，
    /// 走同一个 `ModelResolver::resolve_id_or_name`）。
    #[arg(short = 'm', long = "model", value_name = "ID")]
    pub model: String,

    /// 发送给模型的用户消息（必填，单条）。
    /// 设计成 positional 而不是 `--prompt`，因为 `latte-agent test
    /// <model> 'hi'` 这种短形式在排查时最常用，少敲两个键。
    #[arg(value_name = "PROMPT")]
    pub prompt: String,

    /// 可选：覆盖 system prompt。默认空（让模型按 vendor 默认行为响应）。
    #[arg(long, value_name = "TEXT")]
    pub system: Option<String>,

    /// 可选：覆盖该模型的 `api_key`（与 `chat --api-key` 走同一覆盖路径）。
    #[arg(long, value_name = "KEY")]
    pub api_key: Option<String>,

    /// 可选：覆盖 `base_url`（排查 vendor proxy 时常用）。
    #[arg(long, value_name = "URL")]
    pub base_url: Option<String>,

    /// **跳过 `AiClient`，直接用 reqwest 调一次**。
    ///
    /// 用 `--raw` 时：
    /// - 不走 `AiClient::check_api_key` / JSON 解析 / 错误分类
    /// - 不写 trace / 不挂 cooldown
    /// - 拿到任何 HTTP 响应（即便 4xx/5xx）都算"成功"，返回码固定 0
    /// - **stdout** 是 vendor 原始 body（不解析），**stderr** 是 status / headers / 调试信息
    ///
    /// 用途：定位"是 vendor 集成层（`AiClient` 解析）的 bug，还是 vendor / 网络本身的问题"。
    /// 典型场景：`error decoding response body` 这种 — 我们看 body 到底返回了什么。
    #[arg(long)]
    pub raw: bool,

    /// Path to agents config (file or directory). 与 `chat --agents-config` 同义。
    #[arg(long, value_name = "PATH", default_value = ".latte/agents.d")]
    pub agents_config: String,

    /// Path to models config (file or directory). 与 `chat --models-config` 同义。
    #[arg(long, value_name = "PATH", default_value = ".latte/models.d")]
    pub models_config: String,
}

impl TestCmd {
    /// 跑测试。**调用 `std::process::exit` 退出进程，不返回**。
    /// 退出码通过 [`TestFailure`] 分类：
    ///   - 0  — 成功
    ///   - 1  — 模型调用失败（auth / 4xx / 5xx / 解码失败等）
    ///   - 2  — 参数 / 配置 / 模型未找到
    /// 这里不返回 `ExitCode` 是因为 stable `ExitCode` 是 opaque 的
    /// （没法抽出整数），把 exit 收在 `run_inner` 内部最直接。
    pub async fn run(&self) -> ! {
        let code = match self.run_inner().await {
            Ok(()) => 0,
            Err(TestFailure::Usage(msg)) => {
                eprintln!("[test] argument error: {msg}");
                2
            }
            Err(TestFailure::Model(msg)) => {
                eprintln!("[test] model call failed: {msg}");
                1
            }
        };
        std::process::exit(code);
    }

    async fn run_inner(&self) -> Result<(), TestFailure> {
        if self.prompt.trim().is_empty() {
            return Err(TestFailure::Usage("prompt is empty".into()));
        }
        // 1) 与 `chat` 共享同一个三段合并配置：CLI > project > global。
        //    这样 `latte-agent test -m MiniMax-M3 'hi'` 走的就是 `chat -m MiniMax-M3`
        //    实际用的同一份模型定义 / api_key / base_url。
        let cli = CliOverrides {
            api_key: self.api_key.clone(),
            api_key_target: Some(self.model.clone()),
            // base_url 通过 field_overrides 注入：
            //   self.model + "base_url" + <url>
            field_overrides: self
                .base_url
                .as_ref()
                .map(|u| vec![(self.model.clone(), "base_url".to_string(), u.clone())])
                .unwrap_or_default(),
        };
        let resolved = config_layer::load(
            Some(&self.agents_config),
            Some(&self.models_config),
            cli,
        )
        .map_err(|e| TestFailure::Usage(format!("failed to load config: {e}")))?;

        // 2) 通过 resolver 拿到一个真正能跑的 Model（含解析后的 env vars、api_key、base_url）。
        //    `resolve_id_or_name` 同时支持精确 id 和大小写不敏感的 display name，
        //    与 `chat -m` 的解析语义完全一致。
        let model = resolved
            .resolver
            .resolve_id_or_name(&self.model)
            .map_err(|e| TestFailure::Usage(format!("model resolution failed: {e}")))?;

        // 3) 分派：--raw 跳过 AiClient 直接打 reqwest；
        //    否则走 AiClient（解析 + 错误分类 + 友好输出）。
        if self.raw {
            self.run_raw(&model).await
        } else {
            self.run_ai_client(&model).await
        }
    }

    /// **裸 HTTP 路径**（`--raw`）：直接拿 model 的 base_url + api_key，
    /// 用 reqwest POST 一次，把请求 / 响应的所有字节原样打到 stderr / stdout。
    /// 不分类、不解析、不挂 cooldown。**唯一目的：让"vendor 实际返回什么"
    /// 出现在屏幕上**。
    ///
    /// stdout 是 raw body（可被 `latte-agent test --raw ... | xxd | head` pipe），
    /// stderr 是请求行 / 响应行 / headers。**拿到任何 HTTP 响应都算成功
    /// （exit 0）**；只有连接失败 / DNS / 主动拒绝才 exit 1。
    async fn run_raw(&self, model: &latte_ai::models::Model) -> Result<(), TestFailure> {
        // URL：和 AiClient 走 OpenAI 路径时一样的拼法（base_url + "/chat/completions"）。
        // 注意：默认 base_url 形如 `https://api.minimaxi.com/v1`，
        // 所以拼出来是 `https://api.minimaxi.com/v1/chat/completions`。
        let url = format!(
            "{}/chat/completions",
            model.base_url.trim_end_matches('/')
        );

        // 极简 body：model + 单条 user message + max_tokens=16。
        // 不带 tools / temperature / stream —— 这些都可能让 vendor
        // 返回不同的 body 形态，我们要测的是"最朴素请求 vendor 怎么回"。
        let body = serde_json::json!({
            "model": model.id,
            "messages": [{"role": "user", "content": self.prompt}],
            "max_tokens": 16,
        });
        let body_str = serde_json::to_string(&body)
            .map_err(|e| TestFailure::Usage(format!("serialize body: {e}")))?;

        // key 脱敏输出（前 4 + 后 4 字符，中间 *），
        // 让用户能确认"传的是不是我以为的那个 key"而不泄漏完整密钥到日志。
        let key_for_log = redact_key(&model.api_key);

        eprintln!("--- REQUEST ---");
        eprintln!("POST {}", url);
        eprintln!("Authorization: Bearer {}", key_for_log);
        eprintln!("Content-Type: application/json");
        eprintln!("\n{}\n", body_str);

        // 30s 超时，比 AiClient 默认的 300s 短得多 —— 排查场景下不想等。
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| TestFailure::Usage(format!("build reqwest client: {e}")))?;

        let resp = client
            .post(&url)
            .header("Authorization", format!("Bearer {}", model.api_key))
            .header("Content-Type", "application/json")
            .body(body_str)
            .send()
            .await
            .map_err(|e| TestFailure::Model(format!("raw request failed: {e}")))?;

        let status = resp.status();
        let headers = resp.headers().clone();

        eprintln!("--- RESPONSE ---");
        eprintln!("HTTP {} {}", status.as_u16(), status.canonical_reason().unwrap_or(""));
        for (k, v) in headers.iter() {
            eprintln!(
                "{}: {}",
                k,
                v.to_str().unwrap_or("<binary header value>")
            );
        }
        eprintln!();

        // body 读成 raw bytes，不解析。**这是 --raw 的全部价值**：
        // 让"vendor 实际返回的字节序列"出现在 stdout 上，不被 AiClient 吃掉。
        let body_bytes = resp
            .bytes()
            .await
            .map_err(|e| TestFailure::Model(format!("raw body read failed: {e}")))?;

        use std::io::Write;
        std::io::stdout().write_all(&body_bytes).ok();
        std::io::stdout().flush().ok();
        eprintln!("\n[body: {} bytes]", body_bytes.len());

        // 拿到响应就是成功。HTTP 4xx/5xx 仍然 exit 0 —— 那是 vendor 的答案，不是我们的失败。
        Ok(())
    }

    /// 走 `AiClient` 的常规路径（解析 + 错误分类 + 友好输出）。
    /// 之前用 `Agent::chat_with_progress(WaitPolicy::NoWait)` 时，单 model 失败后
    /// 会把这个模型标 cooldown，然后 chain 走完发现没下一个，**返回
    /// `ModelsUnavailable` 把真正的错误吞掉了** —— 用户看到的是
    /// "all models unavailable" 而不是 vendor 给的真实错误。
    /// test 的语义是"裸调一个模型拿真实响应 / 错误"，不是"走 fallback 链"，
    /// 所以应该直连 `AiClient`。
    async fn run_ai_client(&self, model: &latte_ai::models::Model) -> Result<(), TestFailure> {
        let client = AiClient::new(model.clone())
            .map_err(|e| TestFailure::Usage(format!("failed to build client: {e}")))?;

        // 构造消息列表。系统提示词由用户在 `--system` 显式提供时才会发；
        // 默认不发送任何 system，让模型按 vendor 默认行为响应。
        let mut messages: Vec<Message> = Vec::new();
        if let Some(sys) = &self.system {
            messages.push(Message::system(sys.clone()));
        }
        messages.push(Message::user(self.prompt.clone()));
        let params = GenerateParams::default();

        // 发起调用。错误是 vendor 返回的 `AiError`，不经 Agent 包装。
        let completion = client
            .chat(&messages, &params)
            .await
            .map_err(|e| TestFailure::Model(format_ai_error(&e)))?;

        print_success(&model.id, &completion);
        Ok(())
    }
}

/// 失败分类。`Usage` 是用户该修的（参数 / 配置 / 模型 id），
/// `Model` 是 vendor 该修的（auth / 网络 / 解析）。分类后
/// exit code 区分（2 vs 1），方便脚本化判断。
#[derive(Debug)]
enum TestFailure {
    Usage(String),
    Model(String),
}

/// 把 vendor 返回的 `AiError` 投影成单行可读消息，并把 `ModelErrorKind`
/// 的 label / user_hint 也带上 —— 这是 `latte-agent test` 给排查者
/// 的最大价值：直接告诉调用方 "这是 vendor 5xx" 还是 "api_key 错"，
/// 省得自己去翻 trace.jsonl。
fn format_ai_error(e: &AiError) -> String {
    let kind = ModelErrorKind::from(e);
    format!(
        "[{}/{}] {}: {}",
        kind.label(),
        kind.user_hint(),
        kind.variant_name(),
        e
    )
}

/// 把 api_key 脱敏成 `abcd********wxyz` 形式用于日志：
/// 让用户能确认"传的是不是我以为的那个 key"而不泄漏完整密钥。
/// 短于 12 字符的 key（基本不可能）整串脱敏。
fn redact_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() < 12 {
        return "***".to_string();
    }
    let head: String = chars.iter().take(4).collect();
    let tail: String = chars.iter().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{}********{}", head, tail)
}

/// 给 `ModelErrorKind` 的人类可读 variant 名（错误日志中指出具体变体）。
/// 不暴露内部字段，只暴露名字。
trait ModelErrorKindExt {
    fn variant_name(&self) -> &'static str;
}

impl ModelErrorKindExt for ModelErrorKind {
    fn variant_name(&self) -> &'static str {
        match self {
            Self::Timeout => "Timeout",
            Self::ConnectFailed => "ConnectFailed",
            Self::RateLimited { .. } => "RateLimited",
            Self::Http { .. } => "Http",
            Self::Auth => "Auth",
            Self::ModelNotFound => "ModelNotFound",
            Self::Stream { .. } => "Stream",
            Self::Serde { .. } => "Serde",
            Self::Config { .. } => "Config",
            Self::CooldownHit { .. } => "CooldownHit",
            Self::Other { .. } => "Other",
        }
    }
}

/// 输出成功结果。
///
/// 格式刻意简单（两行），方便脚本化：
///   ```
///   <content>
///   --- tokens: ↑N ↓M (K thinking) | model: <id> | stop: <reason>
///   ```
fn print_success(model_id: &str, c: &Completion) {
    println!("{}", c.content);
    eprintln!(
        "--- tokens: {} | model: {} | stop: {}",
        c.usage, model_id, c.stop_reason
    );
}

// ─── 单测 ──────────────────────────────────────────────────────────────
//
// 重点覆盖：
// 1. 错误消息分类正确（auth / 4xx / 5xx / 不可用 → 不同 label）。
// 2. 成功输出格式稳定。
// 3. 失败分类：空白 prompt → Usage。
//
// 实际的 HTTP 调用没法在单测里跑（需要 mock server），所以这里只
// 测 **纯函数** 的可测试部分。集成测试（带真模型）留给 e2e。

#[cfg(test)]
mod tests {
    use super::*;
    use latte_ai::error::AiError;
    use latte_ai::models::TokenUsage;

    /// 纯函数：把 `AiError` 翻译成单行可读消息，且带 kind label。
    /// 关键不变量：auth / 404 / 5xx / body-decode 必须能被 grep 到（便于脚本化分流）。
    #[test]
    fn format_ai_error_classifies_auth() {
        let e = AiError::Auth("bad key".into());
        let msg = format_ai_error(&e);
        assert!(msg.contains("auth"), "expected 'auth' label, got: {msg}");
        assert!(msg.contains("check api_key"), "expected user_hint, got: {msg}");
    }

    #[test]
    fn format_ai_error_classifies_404() {
        let e = AiError::ModelNotFound("gpt-99".into());
        let msg = format_ai_error(&e);
        assert!(
            msg.contains("model_not_found"),
            "expected 'model_not_found' label, got: {msg}"
        );
    }

    #[test]
    fn format_ai_error_classifies_429_with_retry_after() {
        let e = AiError::RateLimited {
            retry_after: 30.0,
            message: "rate limit hit".into(),
        };
        let msg = format_ai_error(&e);
        assert!(msg.contains("rate_limited"), "got: {msg}");
    }

    /// HTTP 5xx 应走 `http_error` 标签（不是 `other`），
    /// 这正是 trace.rs `From<&AiError>` 的关键分类。
    #[test]
    fn format_ai_error_classifies_5xx_as_http() {
        let e = AiError::Api {
            status: 503,
            message: "down".into(),
        };
        let msg = format_ai_error(&e);
        assert!(msg.contains("http_error"), "expected http_error, got: {msg}");
    }

    /// **直接复现用户当前 hit 的 case**：
    /// `AiError::Http(reqwest::Error)` 的 body-decode 失败既不是 timeout
    /// 也不是 connect，被分到 `Other`，message 字段必须保留 vendor 原文
    /// （"error decoding response body"）让排查者能立刻定位 vendor 兼容性问题。
    ///
    /// 实际从 reqwest::Error 到 `AiError::Other { message: format!("http: {req}") }`
    /// 的转换在 `latte_agent_core::trace::From<&AiError>`，那个转换是另
    /// 一个 crate 的责任。这里只测 `format_ai_error` 对 `Other` 的显示：
    /// **必须保留 message 字段**（不能像 `expect("...")` 那样吞掉）。
    #[test]
    fn format_ai_error_preserves_vendor_message_in_other_kind() {
        // 模拟 vendor body-decode 失败经过 From<&AiError> 后的形态
        let e = AiError::Other("http: error decoding response body".into());
        let msg = format_ai_error(&e);
        assert!(msg.contains("other"), "expected 'other' label, got: {msg}");
        // 关键不变量：vendor 原始信息不能丢
        assert!(
            msg.contains("error decoding response body"),
            "vendor message lost: {msg}"
        );
    }

    /// `TokenUsage` Display 格式必须稳定：grep 脚本会按 `↑N ↓M` 切分。
    /// 改这个 Display 就是改外部契约，必须被这个测试卡住。
    #[test]
    fn success_line_includes_usage_and_model_id() {
        let c = Completion {
            content: "hello".into(),
            stop_reason: "end_turn".into(),
            usage: TokenUsage {
                input_tokens: 5,
                output_tokens: 7,
                thinking_tokens: 0,
            },
        };
        assert_eq!(format!("{}", c.usage), "↑5 ↓7");
    }

    /// `run_inner` 自身是个 async fn，需要一个能跑 await 的 host。
    /// 这里只验"空 prompt"这一个**不依赖网络**的早期错误路径。
    /// 加载 config / 真调模型 / 5xx → 1 这类路径留给 e2e。
    #[test]
    fn empty_prompt_is_usage_error() {
        let cmd = TestCmd {
            model: "x".into(),
            prompt: "   ".into(),
            system: None,
            api_key: None,
            base_url: None,
            raw: false,
            agents_config: ".latte/agents.d".into(),
            models_config: ".latte/models.d".into(),
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let res = rt.block_on(cmd.run_inner());
        match res {
            Err(TestFailure::Usage(msg)) => assert!(msg.contains("prompt")),
            other => panic!("expected Usage error, got: {other:?}"),
        }
    }

    /// 保证 `ModelErrorKind::variant_name` 对所有变体都返回非空静态串——
    /// 防止谁新加一个变体却忘了在这里 match。编译期 `match` 是穷尽的就够了，
    /// 但这里加运行期断言能再兜一道。
    #[test]
    fn variant_name_is_non_empty_for_every_kind() {
        let kinds = [
            ModelErrorKind::Timeout,
            ModelErrorKind::ConnectFailed,
            ModelErrorKind::RateLimited { retry_after_secs: 0.0 },
            ModelErrorKind::Http {
                status: 500,
                message: "x".into(),
            },
            ModelErrorKind::Auth,
            ModelErrorKind::ModelNotFound,
            ModelErrorKind::Stream { message: "x".into() },
            ModelErrorKind::Serde { message: "x".into() },
            ModelErrorKind::Config { message: "x".into() },
            ModelErrorKind::CooldownHit {
                cooldown_remaining_secs: 0.0,
            },
            ModelErrorKind::Other { message: "x".into() },
        ];
        for k in &kinds {
            assert!(!k.variant_name().is_empty(), "{:?} has empty name", k);
        }
    }

    /// `--raw` 模式下 key 脱敏：必须保留前 4 / 后 4 字符以便核对，
    /// 中间用 `********` 盖住。
    /// 这是把 api_key 打到 stderr 的安全契约，**改这个函数就是改安全策略**。
    #[test]
    fn redact_key_long_enough_shows_head_and_tail() {
        // 22 字符：head 取前 4 = "sk-a"，tail 取末 4 = "Zdef"，
        // 中间 14 字符盖 `********`。
        let redacted = redact_key("sk-abc1234567890XYZdef");
        assert_eq!(redacted, "sk-a********Zdef");
    }

    /// 短于 12 字符的 key（异常情况）整串 `***`，不能泄漏任何字符。
    #[test]
    fn redact_key_short_string_fully_hidden() {
        assert_eq!(redact_key("short"), "***");
        assert_eq!(redact_key(""), "***");
    }

    /// 12 字符（边界）：刚好够 4 + 4 + 4 个 `*`。
    #[test]
    fn redact_key_exact_twelve_chars() {
        let redacted = redact_key("1234567890ab");
        assert_eq!(redacted, "1234********90ab");
    }
}
