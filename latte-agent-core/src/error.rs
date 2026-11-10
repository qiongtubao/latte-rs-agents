//! Error types for the agent runtime.

use thiserror::Error;

/// Result alias used throughout the agent crates.
pub type AgentResult<T> = std::result::Result<T, AgentError>;

/// `ToolLoopDetected` 的 Display 后缀：只报 partial 的规模，不把
/// 正文塞进错误串（错误串会被回填给模型/上报给 manager，正文另有降级
/// 采纳路径，见 `workflow.rs` 的 partial 降级分支）。
/// 标注 `budget` 是「分派准入线」而不是 workflow 硬上限。
fn fmt_dispatch_ceiling(dispatch_ceiling: bool) -> String {
    if dispatch_ceiling {
        "（首次分派准入线，已为校验返修预留额度；不是 workflow 硬上限）".to_string()
    } else {
        String::new()
    }
}

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

    /// Shared run/session token budget exceeded. All runners attached to the
    /// same meter stop before their next model request.
    ///
    /// `used` 是**这一发请求的估算落点**（已用量 + 在途预占 + 本请求估算），
    /// 不是「已花掉多少」；`budget` 是撞到的那条线——可能是 workflow 硬上限，
    /// 也可能是首次分派留了返修额度后的准入 ceiling。两者的区别要写进文案：
    /// 实测 `已用完（764742/800000）` 里 800000 其实是
    /// `1,000,000 × (1 − 20% 返修预留)` 的分派 ceiling，而用户按它当成
    /// 「1M 预算还剩 35K」，读不出该调哪个旋钮。
    #[error(
        "token budget exceeded: this request would reach {used}, limit {budget}{}{}",
        fmt_dispatch_ceiling(*dispatch_ceiling),
        fmt_partial(partial)
    )]
    TokenBudgetExceeded {
        used: u64,
        budget: u64,
        /// `budget` 是「首次分派准入线」而非 workflow 硬上限。
        ///
        /// 两者同形不同源：准入线 = `硬上限 × (1 − 返修预留比例)`。不标出来，
        /// 用户会把 800000 当成 1M 预算的剩余量（实测事故就是这样）。
        dispatch_ceiling: bool,
        partial: String,
    },

    /// 一个 workflow step 在发送下一次模型调用前发现实际待发 prompt 已
    /// 超出该 step 的硬预算。与 session token budget 不同：这是防止
    /// 单个工具循环把 read/tool-result 历史不断重传的上下文边界；workflow
    /// 可选择丢弃 runner 并从受控文件契约重新启动。
    #[error("prompt budget exceeded: estimated {estimated}, budget {budget}")]
    PromptBudgetExceeded { estimated: usize, budget: usize },

    /// A tool loop was detected: the model called the same tool with
    /// the same arguments `LOOP_STREAK_THRESHOLD+` times in a row,
    /// indicating it is stuck.
    ///
    /// deadline-only 模式下这是唯一始终生效的自动刹车（轮次上限连同
    /// `MaxToolRoundsExceeded` 已删除，`deadline` 只在 delegate / workflow
    /// step 上设置），所以它必须把已产出的正文带出来——熔断时丢掉模型已
    /// 写好的结论，就是实测「105 轮跑完、3 小时零产出」那次事故的
    /// 成因。上层据此降级采纳（见 `workflow.rs` 的 partial 降级分支与
    /// `controller.rs` 的 delegate 回喂）。
    #[error("tool loop detected: {tool} — {reason}{}", fmt_partial(partial))]
    ToolLoopDetected {
        tool: String,
        reason: String,
        /// 熔断时模型已产出的正文（可能为空）。
        partial: String,
    },
    #[error("request budget exceeded: {requests} model requests (budget {budget}){}", last_tool.as_ref().map(|t| format!(", last tool: {t}")).unwrap_or_default())]
    /// 本 turn 的模型请求次数用完了请求预算（见 `AgentRunner::set_request_budget`）。
    /// 达到软预算时 runner 已注入过「立即收尾」提示；仍未收尾、到硬上限才报这个。
    /// 与 `DeadlineExceeded` 同样带 `partial`，调用方按截断降级采纳。
    RequestBudgetExceeded {
        requests: usize,
        budget: usize,
        last_tool: Option<String>,
        partial: String,
    },
    /// 本 turn 将要启动的一批工具调用会超过硬上限。检查发生在任何调用
    /// spawn/execute 之前，因此 `calls` 是已经启动的数量，第 N+1 个工具
    /// 不会产生副作用。
    #[error("tool call budget exceeded: started {calls}, limit {limit}{}", last_tool.as_ref().map(|t| format!(", last tool: {t}")).unwrap_or_default())]
    ToolCallBudgetExceeded {
        calls: usize,
        limit: usize,
        last_tool: Option<String>,
        partial: String,
    },
    /// Wall-clock deadline exceeded before completion.
    #[error("deadline exceeded after {elapsed_secs}s ({tool_rounds} tool rounds){}", last_tool.as_ref().map(|t| format!(", last tool: {t}")).unwrap_or_default())]
    DeadlineExceeded {
        elapsed_secs: u64,
        tool_rounds: usize,
        last_tool: Option<String>,
        /// 截止时模型已产出的最后一段正文（可能为空）。
        ///
        /// 截止截断**一律**报这个错误，不再把 partial 当成正常回复返回：
        /// 截止往往发生在工具循环中途，最后一段正文是「再补读三个片段……」
        /// 这类过程性旁白，当成 `Ok` 交出去，下游契约校验会拿它报一个
        /// 与真实原因（超时）毫不相干的错误（实测：annotate 步 904s 截断，
        /// 被判「缺少 code/」重试）。调用方据此自行决定降级采纳并标注截断。
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

#[cfg(test)]
mod budget_message_tests {
    use super::*;

    /// 报错必须让人分得清「撞的是哪条线」。
    ///
    /// 实测事故原文是 `已用完（764742/800000）`：800000 其实是
    /// `1,000,000 × (1 − 20% 返修预留)` 的**首次分派准入线**，而用户按
    /// 它读成「1M 预算还剩 35K」，既算不出还剩多少，也不知道该调
    /// `workflow_limit` 还是该缩小取证范围。
    #[test]
    fn dispatch_ceiling_is_labelled_distinctly_from_hard_limit() {
        let ceiling_hit = AgentError::TokenBudgetExceeded {
            used: 764_742,
            budget: 800_000,
            dispatch_ceiling: true,
            partial: String::new(),
        };
        let text = ceiling_hit.to_string();
        assert!(text.contains("764742"), "{text}");
        assert!(text.contains("800000"), "{text}");
        assert!(
            text.contains("首次分派准入线"),
            "必须点明这是准入线：{text}"
        );
        assert!(
            text.contains("不是 workflow 硬上限"),
            "必须排除「这就是总预算」的误读：{text}"
        );

        let hard_hit = AgentError::TokenBudgetExceeded {
            used: 1_000_100,
            budget: 1_000_000,
            dispatch_ceiling: false,
            partial: String::new(),
        };
        let text = hard_hit.to_string();
        assert!(
            !text.contains("准入线"),
            "撞的是硬上限时不该冒充准入线：{text}"
        );
        assert!(text.contains("1000000"), "{text}");
    }

    /// 有 partial 时仍要带出「可降级采纳」提示——这是整步不被判死的依据。
    #[test]
    fn partial_hint_survives_the_ceiling_label() {
        let err = AgentError::TokenBudgetExceeded {
            used: 900,
            budget: 800,
            dispatch_ceiling: true,
            partial: "已经写好的结论".into(),
        };
        let text = err.to_string();
        assert!(text.contains("首次分派准入线"), "{text}");
        assert!(text.contains("可降级采纳"), "partial 提示丢了：{text}");
    }
}
