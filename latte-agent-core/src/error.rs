//! Error types for the agent runtime.

use thiserror::Error;

/// Result alias used throughout the agent crates.
pub type AgentResult<T> = std::result::Result<T, AgentError>;

/// `ToolLoopDetected` 的 Display 后缀：只报 partial 的规模，不把
/// 正文塞进错误串（错误串会被回填给模型/上报给 manager，正文另有降级
/// 采纳路径，见 `workflow.rs` 的 partial 降级分支）。
fn fmt_partial(partial: &str) -> String {
    if partial.trim().is_empty() {
        return String::new();
    }
    format!(
        "（已产出 {} 字符未收尾，可降级采纳）",
        partial.chars().count()
    )
}

/// Agent runtime errors.
#[derive(Error, Debug)]
pub enum AgentError {
    /// Configuration loading or parsing failed.
    #[error("config error: {0}")]
    Config(String),

    /// A role referenced in config was not found.
    #[error("role not found: {0}")]
    RoleNotFound(String),

    /// A model referenced in config was not found.
    #[error("model not found: {0}")]
    ModelNotFound(String),

    /// Model tier resolution failed.
    #[error("cannot resolve model for role '{role}' at tier '{tier}': {reason}")]
    ModelResolutionFailed {
        role: String,
        tier: String,
        reason: String,
    },

    /// Template rendering failed.
    #[error("template render error for '{0}': {1}")]
    Template(String, #[source] handlebars::RenderError),

    /// Template file not found.
    #[error("prompt template not found: {0}")]
    TemplateNotFound(String),

    /// AI client error.
    #[error("AI client error: {0}")]
    AiClient(#[from] latte_ai::error::AiError),

    /// Tool execution error.
    #[error("tool error: {0}")]
    Tool(String),

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Token budget exceeded.
    #[error("token budget exceeded: used {used}, budget {budget}")]
    TokenBudgetExceeded { used: usize, budget: usize },

    /// A tool loop was detected: the model called the same tool with
    /// the same arguments `LOOP_STREAK_THRESHOLD+` times in a row,
    /// indicating it is stuck.
    ///
    /// deadline-only 模式下这是唯一始终生效的自动刹车（轮次上限连同
    /// `MaxToolRoundsExceeded` 已删除，`deadline` 只在 delegate / workflow
    /// step 上设置），所以它必须把已产出的正文带出来——熔断时丢掉模型已
    /// 写好的结论，就是 jemalloc「105 轮跑完、3 小时零产出」那次事故的
    /// 成因。上层据此降级采纳（见 `workflow.rs` 的 partial 降级分支与
    /// `controller.rs` 的 delegate 回喂）。
    #[error("tool loop detected: {tool} — {reason}{}", fmt_partial(partial))]
    ToolLoopDetected {
        tool: String,
        reason: String,
        /// 熔断时模型已产出的正文（可能为空）。
        partial: String,
    },

    /// Invalid parameter.
    #[error("invalid parameter: {0}")]
    InvalidParam(String),

    /// Orchestration error (for phase 2+).
    #[error("orchestration error: {0}")]
    Orchestration(String),

    /// All models in the role's fallback chain are unavailable
    /// (rate-limited / 5xx / cooldown).
    ///
    /// `tried`: model_ids attempted in priority order (skipping
    ///          already-on-cooldown entries).
    /// `next_retry_in`: how long until the **earliest** model in the chain
    ///                  exits cooldown. `None` if no model has a future
    ///                  cooldown (i.e. the failures were non-retryable but
    ///                  were swallowed by the fallback loop — caller's hint
    #[error("all models unavailable (tried: {tried:?}); failures: {failures:?}; next retry in {next_retry_in:?}")]
    ModelsUnavailable {
        tried: Vec<String>,
        /// 每个失败模型的底层错误摘要（model_id, 截断后的错误），
        /// 让"all models unavailable"不再是黑盒——限流/鉴权/网络
        /// 问题可以直接从错误消息与会话日志里确诊。
        failures: Vec<(String, String)>,
        next_retry_in: Option<std::time::Duration>,
    },
    /// A lifecycle hook aborted execution.
    #[error("hook '{hook}' aborted: {reason}")]
    HookAborted { hook: String, reason: String },

    /// Pre-persistence gate 已重试 `max_retries` 次仍判定当前
    /// turn 不合格（如 D5/D6 一直命中），advisor 选择**终止本次
    /// turn 而非强制落盘坏答案**。`reason` 是给 UI 展示的终止
    /// 原因，`detector` 是触发的 detector 标签（D1-D6）。
    ///
    /// 与 `HookAborted` 的区别：终止后 controller **不会**广播
    /// `RoleTurn` 给 UI（坏答案不落盘），改为发
    /// `ChatEvent::AdvisorTerminated` 让用户看到原因并继续对话。
    #[error("advisor terminated: {reason} (detector: {detector})")]
    AdvisorTerminated { reason: String, detector: String },
}
