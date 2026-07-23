//! Trace event types and sinks for full-chain observability.
//!
//! See `docs/superpowers/specs/2026-06-25-latte-agent-debug-observability-design.md`
//! for the design.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use latte_ai::error::AiError;

/// 统一的错误分类标签。把 `latte_ai::error::AiError`、`AgentError`、
/// `reqwest::Error` 等异构错误源投影成一个稳定的小枚举，让 trace.jsonl
/// 上的 `ModelCallFailed.error_kind`、`ChatEvent::Error.kind`、CLI/UI
/// 错误展示都能基于 **结构化** 分类做事（着色、统计、自动建议），
/// 不再依赖 `format!("{e}")` 解析。
///
/// 设计原则：
/// - 标签 **有限且互斥**，每个 kind 都有清晰的处置策略（"可以重试"
///   / "应该换模型" / "用户需要修配置"）
/// - 不携带可执行的元信息（不暴露 api_key、URL 含 key 等敏感字段）
/// - 错误原文仍放在 `error_message: String` 里供离线诊断
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]  // no Eq: f64 fields
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelErrorKind {
    /// HTTP 请求整体超时（reqwest `is_timeout()`）。
    Timeout,
    /// TCP/TLS 连接失败（reqwest `is_connect()`）— 不是超时但同样卡死。
    ConnectFailed,
    /// HTTP 429 / vendor `Retry-After`。`retry_after_secs` 是 vendor
    /// 建议等的时间（秒）；0 表示 vendor 没给。
    RateLimited { retry_after_secs: f64 },
    /// 任意 HTTP 状态码（4xx/5xx 中除已分类的）。
    Http { status: u16, message: String },
    /// 鉴权失败（401/403）。需要用户修 api_key。
    Auth,
    /// 模型不存在（404）。需要用户修 model id。
    ModelNotFound,
    /// SSE / 流式连接异常。
    Stream { message: String },
    /// 响应体解析失败。
    Serde { message: String },
    /// 配置错误（api_key 空、base_url 拼写错等）。
    Config { message: String },
    /// 模型在 cooldown 窗口内，跳过本次调用。
    /// 这是**跳过的原因**，不是 chat 调用本身失败。
    CooldownHit { cooldown_remaining_secs: f64 },
    /// 其他 / 未分类 / vendor 私有错误信息。
    Other { message: String },
}

impl ModelErrorKind {
    /// 稳定的小写标签，便于日志 grep / 统计。
    pub fn label(&self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::ConnectFailed => "connect_failed",
            Self::RateLimited { .. } => "rate_limited",
            Self::Http { .. } => "http_error",
            Self::Auth => "auth",
            Self::ModelNotFound => "model_not_found",
            Self::Stream { .. } => "stream",
            Self::Serde { .. } => "serde",
            Self::Config { .. } => "config",
            Self::CooldownHit { .. } => "cooldown_hit",
            Self::Other { .. } => "other",
        }
    }

    /// 该错误是否建议直接重试同一个模型（即"非可重试"语义，
    /// fall back 到下一个模型）。
    pub fn is_retryable(&self) -> bool {
        match self {
            // Auth / 404 / serde / config 这些永久错：重试同一个会
            // 100% 失败，应该立刻 fallback。
            Self::Auth | Self::ModelNotFound | Self::Serde { .. } | Self::Config { .. } => false,
            _ => true,
        }
    }

    /// 用户侧建议的简短描述（不是修复指引，只标"问题类型"）。
    pub fn user_hint(&self) -> &'static str {
        match self {
            Self::Timeout => "model took too long to respond",
            Self::ConnectFailed => "couldn't reach the model endpoint",
            Self::RateLimited { .. } => "rate limited by vendor",
            Self::Http { status, .. } => match status {
                &s if s >= 500 => "vendor server error",
                _ => "request rejected by vendor",
            },
            Self::Auth => "auth failed — check api_key",
            Self::ModelNotFound => "model id not recognized by vendor",
            Self::Stream { .. } => "response stream interrupted",
            Self::Serde { .. } => "vendor returned unparseable response",
            Self::Config { .. } => "local config error",
            Self::CooldownHit { .. } => "model still on cooldown from earlier failure",
            Self::Other { .. } => "uncategorized error",
        }
    }
}

/// 把 `AiError` 投影成 `ModelErrorKind`。
///
/// 关键设计点：
/// - `AiError::Http(reqwest::Error)` 内部其实封装了 timeout / connect /
///   其它 — 用 `reqwest::Error::is_timeout` / `is_connect` 区分，否则
///   会全部坍缩到 "Other" 标签里（这正是当前 `chat_openai` 路径已经踩过的坑）。
/// - 4xx/5xx HTTP 错误（`AiError::Api`）也单独分类，因为"404"和"503"
///   排查方向截然不同。
impl From<&AiError> for ModelErrorKind {
    fn from(e: &AiError) -> Self {
        match e {
            AiError::Http(req) => {
                if req.is_timeout() {
                    Self::Timeout
                } else if req.is_connect() {
                    Self::ConnectFailed
                } else {
                    Self::Other { message: format!("http: {req}") }
                }
            }
            AiError::Api { status, message } => match *status {
                401 | 403 => Self::Auth,
                404 => Self::ModelNotFound,
                429 => Self::RateLimited { retry_after_secs: 0.0 },
                other => Self::Http { status: other, message: message.clone() },
            }
            AiError::RateLimited { retry_after, .. } =>
                Self::RateLimited { retry_after_secs: *retry_after },
            AiError::Auth(_) => Self::Auth,
            AiError::ModelNotFound(_) => Self::ModelNotFound,
            AiError::Stream(s) => Self::Stream { message: s.clone() },
            AiError::Serde(err) => Self::Serde { message: err.to_string() },
            AiError::Config(s) => Self::Config { message: s.clone() },
            AiError::UnsupportedProvider(s) => Self::Config { message: format!("unsupported provider: {s}") },
            AiError::Io(err) => Self::Other { message: format!("io: {err}") },
            AiError::Other(s) => Self::Other { message: s.clone() },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceMeta {
    pub turn: u32,
    pub role: String, // "manager", "programmer", ...
    pub ts: String,   // ISO8601 UTC
    pub session_id: String,
}

impl TraceMeta {
    #[cfg(test)]
    pub fn test_default() -> Self {
        Self {
            turn: 0,
            role: "test".into(),
            ts: "1970-01-01T00:00:00Z".into(),
            session_id: "test-session".into(),
        }
    }

    /// Build a `TraceMeta` with the current UTC timestamp.
    /// Used by the agent runtime so emitted events carry a usable
    /// ISO8601 `ts` for timeline display (e.g. `latte-agent debug
    /// trace`). The `turn` is supplied by the caller because the
    /// runner increments it turn-by-turn.
    pub fn now(turn: u32, role: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            turn,
            role: role.into(),
            ts: iso8601_utc_now(),
            session_id: session_id.into(),
        }
    }
}

/// Format the current UTC time as `YYYY-MM-DDTHH:MM:SSZ`. Hand-rolled
/// (no `chrono` dep) so the trace module stays stdlib + serde +
/// parking_lot. Good enough for `latte-agent debug trace` timeline
/// display; not designed for sub-second precision or timezone math.
pub(crate) fn iso8601_utc_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = epoch_to_ymdhms(secs);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo, d, h, mi, s)
}

/// Convert a Unix-epoch second count to a `(year, month, day, h, m, s)`
/// tuple in UTC. Proleptic-Gregorian, correct for every timestamp we
/// care about in 2026.
fn epoch_to_ymdhms(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let s = (secs % 60) as u32;
    let mins = (secs / 60) as u32;
    let mi = mins % 60;
    let hours = mins / 60;
    let h = hours % 24;
    let mut days = (hours / 24) as i64;
    let mut year = 1970i64;
    loop {
        let leap = is_leap(year);
        let dy = if leap { 366 } else { 365 };
        if days >= dy {
            days -= dy;
            year += 1;
        } else {
            break;
        }
    }
    let month_lens = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 0usize;
    while month < 12 {
        let ml = if month == 1 && is_leap(year) { 29 } else { month_lens[month] };
        if days >= ml {
            days -= ml;
            month += 1;
        } else {
            break;
        }
    }
    (year as u32, month as u32 + 1, days as u32 + 1, h, mi, s)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// payload that varies per variant. 9 variants cover the full
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TraceEvent {
    SessionStart {
        meta: TraceMeta,
        tier: String,
        model_chain: Vec<String>,
        allowed_tools: Vec<String>,
    },
    PromptBuilt {
        meta: TraceMeta,
        system_rendered: String,
        history_len: usize,
        user_input: String,
        est_input_tokens: u32,
    },
    ModelCall {
        meta: TraceMeta,
        model_id: String,
        params_json: String,
        latency_ms: u64,
        finish_reason: String,
    },
    ModelRawOut {
        meta: TraceMeta,
        raw_content: String,
    },
    ParseToolCalls {
        meta: TraceMeta,
        raw_in: String,
        parsed: Vec<ParsedCall>,
        diagnostics: ParseDiag,
    },
    ToolExec {
        meta: TraceMeta,
        name: String,
        args_json: String,
        latency_ms: u64,
        status: ToolStatus,
    },
    ToolRetry {
        meta: TraceMeta,
        name: String,
        attempt: u32,
        kind: String,
        reason: String,
        recovered: bool,
    },
    HookFired {
        meta: TraceMeta,
        hook_name: String,
        point: HookPoint,
        outcome_kind: String,
    },
    TurnEnd {
        meta: TraceMeta,
        total_input: u32,
        total_output: u32,
        total_thinking: u32,
        elapsed_ms: u64,
    },
    SessionEnd {
        meta: TraceMeta,
        total_turns: u32,
        total_input: u32,
        total_output: u32,
        total_thinking: u32,
    },
    SessionStarted {
        meta: TraceMeta,
        task_id: String,
        roles: Vec<String>,
        initial_prompt: String,
    },
    SessionPaused {
        meta: TraceMeta,
        task_id: String,
        reason: String,
        turn: u32,
    },
    SessionResumed {
        meta: TraceMeta,
        task_id: String,
        turn: u32,
    },
    RoleInjected {
        meta: TraceMeta,
        task_id: String,
        target_role: String,
        message_preview: String,
    },
    CheckpointCreated {
        meta: TraceMeta,
        checkpoint_id: u32,
        git_commit: String,
        trigger_kind: String,        // "tool_write" | "explicit" | "pre_hazard"
        diff_summary: DiffSummary,
    },
    CheckpointRolledBack {
        meta: TraceMeta,
        checkpoint_id: u32,
        mode: String,                // "code" | "trace" | "full"
        rolled_back_to: String,      // git_commit SHA
    },
    AskHuman {
        meta: TraceMeta,
        task_id: String,
        role: String,
        question: String,
    },
    RoundStarted {
        meta: TraceMeta,
        task_id: String,
        round: u32,
        roles: Vec<String>,
    },
    RoundEnded {
        meta: TraceMeta,
        task_id: String,
        round: u32,
    },
    /// 模型调用失败。这是 trace 黑洞修复的核心：之前失败路径只
    /// 是 trace 流上一个**空洞**，现在每次失败都留一条结构化
    /// 记录，让 `~/.latte/traces/<id>.jsonl` 能离线追查"超时 /
    /// 限流 / 404"等不同原因。
    ///
    /// 注意：即便上层走了 fallback 链命中了下一个模型，每次失败
    /// 都会单独 emit 一条 `ModelCallFailed`，最终 `ModelCall`
    /// （成功）也会再 emit 一次。所以"成功 + N 次失败"会留下
    /// 完整链条。
    ModelCallFailed {
        meta: TraceMeta,
        /// 调用的模型 id（catalog 中的）
        model_id: String,
        /// 第几次尝试（0-based，对应 `model_chain` 中的位置）
        attempt_index: u32,
        /// 从发起请求到收到错误响应的毫秒数
        latency_ms: u64,
        /// 错误分类（统一标签，给前端/统计用）
        error_kind: ModelErrorKind,
        /// 错误原文（截断到 800 字符；含 url/body 等诊断信息，
        /// **不含 api_key** —— 见 ModelErrorKind 设计原则）
        error_message: String,
        /// 如果设置了 cooldown，这是 cooldown 秒数；None 表示不可重试
        cooldown_secs: Option<u32>,
    },
    /// 某模型进入 cooldown 窗口。
    ///
    /// 与 `ModelCallFailed.cooldown_secs` 互补：前者记录**触发的
    /// 一次性事件**（"模型 X 因为 5xx 进入 30s cooldown"），后者记
    /// 录**结果**（"调用失败 / 设了 30s cooldown"）。让 trace 上
    /// 时间线可以反推 N 秒前模型 X 进入了什么状态的 cooldown。
    ModelCooldownEntered {
        meta: TraceMeta,
        model_id: String,
        cooldown_secs: u32,
        /// Unix epoch 秒 — 客户端无需自行计算"还剩多久 cooldown"
        cooldown_until_unix_secs: u64,
        /// 触发进入 cooldown 的错误分类
        trigger_kind: ModelErrorKind,
    },
    /// 跳过当前模型，准备试下一个（fallback）。`reason` 是结构
    /// 化标签（不是 message），方便统计"为什么跳过"。
    FallbackTriggered {
        meta: TraceMeta,
        from_model: String,
        /// `None` 表示 fallback 链已走到尽头、整体失败
        to_model: Option<String>,
        reason: FallbackReason,
        /// 触发这次 fallback 的错误分类；如果是 `OnCooldown` 跳过则
        /// 为 `Some(ModelErrorKind::CooldownHit { ... })`
        cause_kind: Option<ModelErrorKind>,
    },
}

/// 为什么跳过当前模型、试下一个。**枚举**值，不是字符串 ——
/// 让 trace 消费方能基于语义做聚合统计（"今天 30 次 OnCooldown,
/// 5 次 RateLimited"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum FallbackReason {
    /// 当前模型在 cooldown 窗口内
    OnCooldown,
    /// 当前模型失败且错误可重试 → 下一个模型接手
    RetryableFailure,
    /// 链已经走到尽头 → 整体失败（`ModelsUnavailable`）
    NoMoreModels,
    /// `WaitAndRetry` 策略：sleep 过后再次尝试 head 模型，第二次
    /// 仍失败 → 进入 NoMoreModels
    RetryExhausted,
}

/// `Agent::chat()` 内部在每个"不可重试 / 进入 cooldown / 跳 fallback"事件点
/// 上调用的回调。实现类是 `TraceEventProgressSink`（默认实现：把回调翻成
/// 上面 3 个 `TraceEvent` 变体），让 trace 黑洞被堵上又**不**让 `Agent`
/// 直接依赖 `TraceSink`（trace 是 agent 的可选观察层，agent 本身不关心
/// 任何"事件"）。
///
/// 设计点：
/// - 每个回调都是 `&self` + 不可变（事件已经被构造好），没有"agent 等
pub trait ChatProgressSink: Send + Sync {
    fn on_model_call_failed(&self, meta: &TraceMeta, info: ModelCallAttemptInfo);
    /// 进入 cooldown：失败可重试 + 对应时长。与 on_model_call_failed
    /// 是先后两个事件，先触发的失败、再 cooldown，不要漏发任何一个
    fn on_model_cooldown_set(&self, meta: &TraceMeta, info: ModelCooldownInfo);
    /// 跳过当前模型试下一个：包括 OnCooldown 跳过和 Retryable 跳过
    fn on_fallback_triggered(&self, meta: &TraceMeta, info: FallbackInfo);
}

/// 单次失败：模型返回了任何错误（5xx / 404 / timeout / 限流...）。
#[derive(Debug, Clone)]
pub struct ModelCallAttemptInfo {
    pub model_id: String,
    /// 第几个 model_chain（0-based）
    pub attempt_index: u32,
    /// 调用到失败经过的毫秒数
    pub latency_ms: u64,
    pub error_kind: ModelErrorKind,
    /// 错误原文（截断到 800 字符）
    pub error_message: String,
    /// Some(N) = 失败可重试，已设置 cooldown N 秒；None = 不可重试
    pub cooldown_secs: Option<u32>,
    /// 接下来要试的模型 id（fallback）。链尾为 None
    pub next_model_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModelCooldownInfo {
    pub model_id: String,
    pub cooldown_secs: u32,
    /// Unix epoch 秒。客户端无需自行算剩余时间
    pub cooldown_until_unix_secs: u64,
    /// 触发该 cooldown 的错误分类
    pub trigger_kind: ModelErrorKind,
}

#[derive(Debug, Clone)]
pub struct FallbackInfo {
    pub from_model: String,
    pub to_model: Option<String>,
    pub reason: FallbackReason,
    pub cause_kind: Option<ModelErrorKind>,
}
/// `ChatProgressSink` 的默认实现：把每次回调一对一地翻译成上面 3 个
/// `TraceEvent` 变体。让 runner 只需要把 `self.sink` 包成这个，
/// 不必自己写 if/else 分流。
///
/// `meta` 是一次 chat 的"运行时上下文"（turn / role / session_id），
/// 由 runner 构造一次后复用。`Agent::chat_with_progress` 的每个事件
/// 点都拿同一个 meta，让 trace 上同一 turn 的失败事件能被 grep 到。
/// `ChatProgressSink` 的默认实现：把每次回调一对一地翻译成上面 3 个
/// `TraceEvent` 变体。让 runner 只需要把 `self.sink` 包成这个，
/// 不必自己写 if/else 分流。
///
/// 与 trait 同步要求：每个回调都接受 `meta: &TraceMeta`，因为 `Agent`
/// 内部不知道 caller 的 turn/role/session_id，由 caller（runner）注入。
pub struct TraceEventProgressSink {
    /// 底层 trace sink。meta 不在此处保存 ——
    /// `Agent::chat_with_progress` 每次回调把 meta 作为参数传入，
    /// sink 透传给 TraceEvent。
    inner: std::sync::Arc<dyn TraceSink>,
}

impl TraceEventProgressSink {
    pub fn new(inner: std::sync::Arc<dyn TraceSink>) -> Self {
        Self { inner }
    }
}

impl ChatProgressSink for TraceEventProgressSink {
    fn on_model_call_failed(&self, meta: &TraceMeta, info: ModelCallAttemptInfo) {
        let msg = if info.error_message.len() > 800 {
            format!("{}…", &info.error_message[..800])
        } else {
            info.error_message.clone()
        };
        self.inner.emit(TraceEvent::ModelCallFailed {
            meta: meta.clone(),
            model_id: info.model_id.clone(),
            attempt_index: info.attempt_index,
            latency_ms: info.latency_ms,
            error_kind: info.error_kind.clone(),
            error_message: msg,
            cooldown_secs: info.cooldown_secs,
        });
    }
    fn on_model_cooldown_set(&self, meta: &TraceMeta, info: ModelCooldownInfo) {
        self.inner.emit(TraceEvent::ModelCooldownEntered {
            meta: meta.clone(),
            model_id: info.model_id.clone(),
            cooldown_secs: info.cooldown_secs,
            cooldown_until_unix_secs: info.cooldown_until_unix_secs,
            trigger_kind: info.trigger_kind.clone(),
        });
    }
    fn on_fallback_triggered(&self, meta: &TraceMeta, info: FallbackInfo) {
        self.inner.emit(TraceEvent::FallbackTriggered {
            meta: meta.clone(),
            from_model: info.from_model.clone(),
            to_model: info.to_model.clone(),
            reason: info.reason,
            cause_kind: info.cause_kind.clone(),
        });
    }
}

/// No-op sink。`Agent::chat_with_progress(.., None, ..)` 用，对
/// 那些不需要 trace 的 caller（tests / cold-path）零开销。
impl ChatProgressSink for () {
    fn on_model_call_failed(&self, _meta: &TraceMeta, _info: ModelCallAttemptInfo) {}
    fn on_model_cooldown_set(&self, _meta: &TraceMeta, _info: ModelCooldownInfo) {}
    fn on_fallback_triggered(&self, _meta: &TraceMeta, _info: FallbackInfo) {}
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParsedCall {
    pub name: String,
    pub args: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParseDiag {
    pub opens_found: u32,
    pub closes_matched: u32,
    pub unmatched_opens: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ToolStatus {
    Ok(String),
    Err(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookPoint {
    PreCall,
    PostResponse,
    PostParse,
    PreTool,
    PostTool,
}

/// Receiver of trace events. Implementations must be `Send + Sync` so they
/// can be shared across async tasks.
pub trait TraceSink: Send + Sync {
    fn emit(&self, event: TraceEvent);
}

/// Default sink. `emit` is `#[inline]` empty; compiler eliminates the call.
#[derive(Default, Clone, Copy)]
pub struct NullSink;
impl TraceSink for NullSink {
    #[inline]
    fn emit(&self, _event: TraceEvent) {}
}

/// In-memory sink. Accumulates every `emit` into a `Vec<TraceEvent>`
/// shared via `Arc<Mutex<_>>`. Used by the controller to record the
/// full transcript of a per-task subsession so the UI can fetch it
/// later via `/api/sessions/{id}/subsessions/{sub_id}`. Cheap to clone
/// the handle so the runner, the controller, and HTTP handlers can
/// each inspect the same buffer.
#[derive(Default)]
pub struct MemorySink {
    events: parking_lot::Mutex<Vec<TraceEvent>>,
}
impl MemorySink {
    pub fn new() -> Self {
        Self {
            events: parking_lot::Mutex::new(Vec::new()),
        }
    }
    /// Returns a clone of the event log in the order they were emitted.
    pub fn snapshot(&self) -> Vec<TraceEvent> {
        self.events.lock().clone()
    }
    /// Number of captured events so far.
    pub fn len(&self) -> usize {
        self.events.lock().len()
    }
}
impl TraceSink for MemorySink {
    fn emit(&self, event: TraceEvent) {
        self.events.lock().push(event);
    }
}
/// Writes each event as one JSON object per line. Thread-safe.
///
/// Multiple `JsonlSink` instances pointing at the same `path` share a
/// single underlying `BufWriter<File>` keyed by that path. This is
/// load-bearing: callers (e.g. `build_debug_sink` invoked once per role)
/// construct several sinks per session, and without sharing the writers
/// each `BufWriter` flushes independently — losing `\n` separators
/// between events written by different sinks. The trace file then
/// becomes un-parseable as line-delimited JSON.
pub struct JsonlSink {
    inner: Arc<parking_lot::Mutex<std::io::BufWriter<std::fs::File>>>,
    path: PathBuf,
}

impl JsonlSink {
    pub fn new(path: PathBuf) -> Self {
        Self {
            inner: shared_writer_for(path.clone()),
            path,
        }
    }
    pub fn path(&self) -> &PathBuf { &self.path }
}
/// Per-process registry of `Weak<Mutex<BufWriter<File>>>` keyed by path.
/// `JsonlSink::new` looks up the existing writer here; if absent, it
/// creates a fresh one and inserts it. Sinks own the writer via a
/// strong `Arc`; the registry only keeps a weak handle so it doesn't
/// keep the writer alive past the last sink's drop.
///
/// Without sharing the writer, multiple `JsonlSink` instances opening
/// the same path each hold an independent `BufWriter`, and `\n`
/// boundaries between events emitted by different sinks get lost.
fn shared_writer_for(
    path: PathBuf,
) -> Arc<parking_lot::Mutex<std::io::BufWriter<std::fs::File>>> {
    use std::collections::HashMap;
    use std::sync::{LazyLock, Weak};
    static REGISTRY: LazyLock<
        parking_lot::Mutex<HashMap<PathBuf, Weak<parking_lot::Mutex<std::io::BufWriter<std::fs::File>>>>>
    > = LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));
    {
        let mut map = REGISTRY.lock();
        // Prune stale weak refs and look up the live one.
        map.retain(|_, w| w.strong_count() > 0);
        if let Some(weak) = map.get(&path) {
            if let Some(arc) = weak.upgrade() {
                return arc;
            }
        }
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .expect("JsonlSink: failed to open trace file");
    let writer = Arc::new(parking_lot::Mutex::new(std::io::BufWriter::new(file)));
    REGISTRY.lock().insert(path, Arc::downgrade(&writer));
    writer
}

impl TraceSink for JsonlSink {
    fn emit(&self, event: TraceEvent) {
        use std::io::Write;
        let mut guard = self.inner.lock();
        let json = serde_json::to_string(&event)
            .expect("TraceEvent serialization");
        writeln!(guard, "{}", json).expect("JsonlSink write");
    }
}

impl TraceEvent {
    pub fn meta(&self) -> &TraceMeta {
        match self {
            TraceEvent::SessionStart { meta, .. }
            | TraceEvent::PromptBuilt { meta, .. }
            | TraceEvent::ModelCall { meta, .. }
            | TraceEvent::ModelRawOut { meta, .. }
            | TraceEvent::ParseToolCalls { meta, .. }
            | TraceEvent::ToolExec { meta, .. }
            | TraceEvent::ToolRetry { meta, .. }
            | TraceEvent::HookFired { meta, .. }
            | TraceEvent::TurnEnd { meta, .. }
            | TraceEvent::SessionEnd { meta, .. }
            | TraceEvent::SessionStarted { meta, .. }
            | TraceEvent::SessionPaused { meta, .. }
            | TraceEvent::SessionResumed { meta, .. }
            | TraceEvent::RoleInjected { meta, .. }
            | TraceEvent::AskHuman { meta, .. }
            | TraceEvent::RoundStarted { meta, .. }
            | TraceEvent::RoundEnded { meta, .. }
            | TraceEvent::CheckpointCreated { meta, .. }
            | TraceEvent::CheckpointRolledBack { meta, .. }
            | TraceEvent::ModelCallFailed { meta, .. }
            | TraceEvent::ModelCooldownEntered { meta, .. }
            | TraceEvent::FallbackTriggered { meta, .. } => meta,
        }
    }
    pub fn variant_name(&self) -> &'static str {
        match self {
            TraceEvent::SessionStart { .. } => "SessionStart",
            TraceEvent::PromptBuilt { .. } => "PromptBuilt",
            TraceEvent::ModelCall { .. } => "ModelCall",
            TraceEvent::ModelRawOut { .. } => "ModelRawOut",
            TraceEvent::ParseToolCalls { .. } => "ParseToolCalls",
            TraceEvent::ToolExec { .. } => "ToolExec",
            TraceEvent::ToolRetry { .. } => "ToolRetry",
            TraceEvent::HookFired { .. } => "HookFired",
            TraceEvent::TurnEnd { .. } => "TurnEnd",
            TraceEvent::SessionEnd { .. } => "SessionEnd",
            TraceEvent::SessionStarted { .. } => "SessionStarted",
            TraceEvent::SessionPaused { .. } => "SessionPaused",
            TraceEvent::SessionResumed { .. } => "SessionResumed",
            TraceEvent::RoleInjected { .. } => "RoleInjected",
            TraceEvent::AskHuman { .. } => "AskHuman",
            TraceEvent::RoundStarted { .. } => "RoundStarted",
            TraceEvent::RoundEnded { .. } => "RoundEnded",
            TraceEvent::CheckpointCreated { .. } => "CheckpointCreated",
            TraceEvent::CheckpointRolledBack { .. } => "CheckpointRolledBack",
            TraceEvent::ModelCallFailed { .. } => "ModelCallFailed",
            TraceEvent::ModelCooldownEntered { .. } => "ModelCooldownEntered",
            TraceEvent::FallbackTriggered { .. } => "FallbackTriggered",
        }
    }
    pub fn body_for_pretty(&self) -> String {
        match self {
            TraceEvent::SessionStart { tier, model_chain, allowed_tools, .. } =>
                format!("tier={} model_chain={:?} tools={:?}", tier, model_chain, allowed_tools),
            TraceEvent::PromptBuilt { est_input_tokens, history_len, user_input, .. } =>
                format!("est_input={} history_len={} user_input={}",
                    est_input_tokens, history_len, user_input),
            TraceEvent::ModelCall { model_id, latency_ms, finish_reason, .. } =>
                format!("model={} latency={}ms finish={}", model_id, latency_ms, finish_reason),
            TraceEvent::ModelRawOut { raw_content, .. } =>
                format!("{}", raw_content),
            TraceEvent::ParseToolCalls { parsed, diagnostics, .. } =>
                format!("parsed={} opens={} matched={} unmatched={}",
                    parsed.len(), diagnostics.opens_found, diagnostics.closes_matched, diagnostics.unmatched_opens.len()),
            TraceEvent::ToolExec { name, args_json, latency_ms, status, .. } => {
                let result_str = match status {
                    ToolStatus::Ok(s) => format!("ok: {}", s),
                    ToolStatus::Err(s) => format!("err: {}", s),
                };
                format!("name={}\n  args:  {}\n  result: {}\n  latency: {}ms",
                    name, args_json, result_str, latency_ms)
            }
            TraceEvent::ToolRetry { name, attempt, kind, reason, recovered, .. } =>
                format!("name={} attempt={} kind={} recovered={} reason={}",
                    name, attempt, kind, recovered, reason),
            TraceEvent::HookFired { hook_name, point, outcome_kind, .. } =>
                format!("{} {:?} {}", hook_name, point, outcome_kind),
            TraceEvent::TurnEnd { total_input, total_output, total_thinking, elapsed_ms, .. } =>
                format!("in={} out={} think={} elapsed={}ms",
                    total_input, total_output, total_thinking, elapsed_ms),
            TraceEvent::SessionEnd { total_turns, total_input, total_output, total_thinking, .. } =>
                format!("turns={} in={} out={} think={}", total_turns, total_input, total_output, total_thinking),
            TraceEvent::SessionStarted { task_id, roles, .. } =>
                format!("task={} roles={}", task_id, roles.join(",")),
            TraceEvent::SessionPaused { task_id, reason, turn, .. } =>
                format!("task={} turn={} reason={}", task_id, turn, reason),
            TraceEvent::SessionResumed { task_id, turn, .. } =>
                format!("task={} turn={}", task_id, turn),
            TraceEvent::RoleInjected { task_id, target_role, message_preview, .. } =>
                format!("task={} -> {} preview={:?}", task_id, target_role, message_preview),
            TraceEvent::AskHuman { task_id, role, question, .. } =>
                format!("task={} role={} question={:?}", task_id, role, question),
            TraceEvent::RoundStarted { task_id, round, roles, .. } =>
                format!("task={} round={} roles=[{}]", task_id, round, roles.join(",")),
            TraceEvent::RoundEnded { task_id, round, .. } =>
                format!("task={} round={}", task_id, round),
            TraceEvent::CheckpointCreated { checkpoint_id, git_commit, trigger_kind, diff_summary, .. } =>
                format!("id={} commit={} trigger={} files={} +{}/-{}",
                    checkpoint_id, git_commit, trigger_kind,
                    diff_summary.files_changed, diff_summary.insertions, diff_summary.deletions),
            TraceEvent::CheckpointRolledBack { checkpoint_id, mode, rolled_back_to, .. } =>
                format!("id={} mode={} -> {}", checkpoint_id, mode, rolled_back_to),
            TraceEvent::ModelCallFailed { model_id, attempt_index, latency_ms, error_kind, cooldown_secs, error_message, .. } => {
                let cooldown_str = match cooldown_secs {
                    Some(s) => format!("{}s", s),
                    None => "n/a".into(),
                };
                // 截断避免 trace 行无界增长；保留尾部 … 标记
                let msg = if error_message.len() > 200 {
                    format!("{}…", &error_message[..200])
                } else {
                    error_message.clone()
                };
                format!("model={} attempt={} latency={}ms kind={} cooldown={} message={:?}",
                    model_id, attempt_index, latency_ms, error_kind.label(),
                    cooldown_str, msg)
            }
            TraceEvent::ModelCooldownEntered { model_id, cooldown_secs, trigger_kind, .. } =>
                format!("model={} cooldown={}s trigger={} hint=\"{}\"",
                    model_id, cooldown_secs, trigger_kind.label(), trigger_kind.user_hint()),
            TraceEvent::FallbackTriggered { from_model, to_model, reason, cause_kind, .. } => {
                let cause = cause_kind.as_ref().map(|k| format!("cause={}", k.label())).unwrap_or_default();
                format!("from={} -> to={:?} reason={:?} {}", from_model, to_model, reason, cause)
            }
         }
    }
    /// Metadata-only projection for IndexSink. Returns None for
    /// events that have no indexable information.
    pub fn to_index_line(&self) -> Option<IndexLine> {
        let meta = self.meta();
        Some(match self {
            TraceEvent::SessionStart { tier, model_chain, allowed_tools, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "SessionStart".into(),
                model_id: None, latency_ms: None, tokens_in: None, tokens_out: None, tokens_think: None,
                detail: format!("tier={} chain_len={} tools={}", tier, model_chain.len(), allowed_tools.len()),
            },
            TraceEvent::PromptBuilt { est_input_tokens, history_len, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "PromptBuilt".into(),
                model_id: None, latency_ms: None,
                tokens_in: Some(*est_input_tokens), tokens_out: None, tokens_think: None,
                detail: format!("history_len={}", history_len),
            },
            TraceEvent::ModelCall { model_id, latency_ms, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "ModelCall".into(),
                model_id: Some(model_id.clone()), latency_ms: Some(*latency_ms),
                tokens_in: None, tokens_out: None, tokens_think: None,
                detail: String::new(),
            },
            TraceEvent::ModelRawOut { .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "ModelRawOut".into(),
                model_id: None, latency_ms: None, tokens_in: None, tokens_out: None, tokens_think: None,
                detail: String::new(),
            },
            TraceEvent::ParseToolCalls { parsed, diagnostics, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "ParseToolCalls".into(),
                model_id: None, latency_ms: None, tokens_in: None, tokens_out: None, tokens_think: None,
                detail: format!("parsed={} unmatched={}", parsed.len(), diagnostics.unmatched_opens.len()),
            },
            TraceEvent::ToolExec { name, latency_ms, status, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "ToolExec".into(),
                model_id: None, latency_ms: Some(*latency_ms), tokens_in: None, tokens_out: None, tokens_think: None,
                detail: format!("name={} status={}", name, match status { ToolStatus::Ok(_) => "ok", ToolStatus::Err(_) => "err" }),
            },
            TraceEvent::ToolRetry { name, attempt, kind, recovered, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "ToolRetry".into(),
                model_id: None, latency_ms: None, tokens_in: None, tokens_out: None, tokens_think: None,
                detail: format!("name={} attempt={} kind={} recovered={}", name, attempt, kind, recovered),
            },
            TraceEvent::HookFired { hook_name, point, outcome_kind, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "HookFired".into(),
                model_id: None, latency_ms: None, tokens_in: None, tokens_out: None, tokens_think: None,
                detail: format!("hook={} point={:?} outcome={}", hook_name, point, outcome_kind),
            },
            TraceEvent::TurnEnd { total_input, total_output, total_thinking, elapsed_ms, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "TurnEnd".into(),
                model_id: None, latency_ms: Some(*elapsed_ms),
                tokens_in: Some(*total_input), tokens_out: Some(*total_output), tokens_think: Some(*total_thinking),
                detail: String::new(),
            },
            TraceEvent::SessionEnd { total_turns, total_input, total_output, total_thinking, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "SessionEnd".into(),
                model_id: None, latency_ms: None,
                tokens_in: Some(*total_input), tokens_out: Some(*total_output), tokens_think: Some(*total_thinking),
                detail: format!("turns={}", total_turns),
            },
            TraceEvent::SessionStarted { task_id, roles, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "SessionStarted".into(),
                model_id: None, latency_ms: None, tokens_in: None, tokens_out: None, tokens_think: None,
                detail: format!("task={} roles={}", task_id, roles.len()),
            },
            TraceEvent::SessionPaused { task_id, reason, turn, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "SessionPaused".into(),
                model_id: None, latency_ms: None, tokens_in: None, tokens_out: None, tokens_think: None,
                detail: format!("task={} turn={} reason={}", task_id, turn, reason),
            },
            TraceEvent::SessionResumed { task_id, turn, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "SessionResumed".into(),
                model_id: None, latency_ms: None, tokens_in: None, tokens_out: None, tokens_think: None,
                detail: format!("task={} turn={}", task_id, turn),
            },
            TraceEvent::RoleInjected { task_id, target_role, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "RoleInjected".into(),
                model_id: None, latency_ms: None, tokens_in: None, tokens_out: None, tokens_think: None,
                detail: format!("task={} target={}", task_id, target_role),
            },
            TraceEvent::AskHuman { task_id, role, question, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "AskHuman".into(),
                model_id: None, latency_ms: None, tokens_in: None, tokens_out: None, tokens_think: None,
                detail: format!("task={} role={} q_len={}", task_id, role, question.len()),
            },
            TraceEvent::RoundStarted { task_id, round, roles, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "RoundStarted".into(),
                model_id: None, latency_ms: None, tokens_in: None, tokens_out: None, tokens_think: None,
                detail: format!("task={} round={} roles={}", task_id, round, roles.len()),
            },
            TraceEvent::RoundEnded { task_id, round, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "RoundEnded".into(),
                model_id: None, latency_ms: None, tokens_in: None, tokens_out: None, tokens_think: None,
                detail: format!("task={} round={}", task_id, round),
            },
            TraceEvent::CheckpointCreated { checkpoint_id, git_commit, diff_summary, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "CheckpointCreated".into(),
                model_id: None, latency_ms: None, tokens_in: None, tokens_out: None, tokens_think: None,
                detail: format!("id={} commit={} files={} +{}/-{}",
                    checkpoint_id, git_commit,
                    diff_summary.files_changed, diff_summary.insertions, diff_summary.deletions),
            },
            TraceEvent::CheckpointRolledBack { checkpoint_id, mode, rolled_back_to, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "CheckpointRolledBack".into(),
                model_id: None, latency_ms: None, tokens_in: None, tokens_out: None, tokens_think: None,
                detail: format!("id={} mode={} -> {}", checkpoint_id, mode, rolled_back_to),
            },
            TraceEvent::ModelCallFailed { model_id, attempt_index, latency_ms, error_kind, cooldown_secs, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "ModelCallFailed".into(),
                model_id: Some(model_id.clone()), latency_ms: Some(*latency_ms),
                tokens_in: None, tokens_out: None, tokens_think: None,
                detail: format!("attempt={} kind={} hint=\"{}\" cooldown={}",
                    attempt_index, error_kind.label(), error_kind.user_hint(),
                    match cooldown_secs { Some(s) => format!("{}s", s), None => "n/a".into() }),
            },
            TraceEvent::ModelCooldownEntered { model_id, cooldown_secs, trigger_kind, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "ModelCooldownEntered".into(),
                model_id: Some(model_id.clone()), latency_ms: None,
                tokens_in: None, tokens_out: None, tokens_think: None,
                detail: format!("{}s trigger={}", cooldown_secs, trigger_kind.label()),
            },
            TraceEvent::FallbackTriggered { from_model, to_model, reason, cause_kind, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "FallbackTriggered".into(),
                model_id: Some(from_model.clone()), latency_ms: None,
                tokens_in: None, tokens_out: None, tokens_think: None,
                detail: format!("-> {:?} reason={:?} cause={}",
                    to_model, reason,
                    cause_kind.as_ref().map(|k| k.label()).unwrap_or("none")),
            },
         })
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct IndexLine {
    pub turn: u32,
    pub ts: String,
    pub role: String,
    pub kind: String,
    pub model_id: Option<String>,
    pub latency_ms: Option<u64>,
    pub tokens_in: Option<u32>,
    pub tokens_out: Option<u32>,
    pub tokens_think: Option<u32>,
    pub detail: String,
}

/// Per-checkpoint diff summary. Stored on `CheckpointCreated` events
/// and on the on-disk `manifest.json` so operators can see what
/// changed without reading the patch file.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiffSummary {
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
}

/// Writes a human-readable multi-line pretty form (default) or
/// raw JSONL (one event per line) to a `Write` impl. The mode is
/// chosen at construction so the format is stable for the lifetime
/// of the sink — output never flips mid-stream.
///
/// `pretty=true` is the default: multi-line, optionally colorized
/// when `use_color` is also true and the writer is a TTY. This is
/// what `latte-agent chat --debug` uses.
///
/// `pretty=false` is the JSONL mode: one `serde_json::to_string`
/// per event, newline-terminated. This is what
/// `latte-agent chat --debug --debug-format jsonl` selects so
/// downstream tools (jq, ripgrep, the future `latte-agent debug
/// replay`) can read the stream.
pub struct StdoutSink {
    writer: parking_lot::Mutex<Box<dyn std::io::Write + Send>>,
    pretty: bool,
    #[allow(dead_code)] // wired up in a later task when the color path lands
    use_color: bool,
}

impl StdoutSink {
    /// Construct a pretty sink bound to stdout. `use_color` is
    /// honored when the writer is a TTY.
    pub fn new(use_color: bool) -> Self {
        Self { writer: parking_lot::Mutex::new(Box::new(std::io::stdout())), pretty: true, use_color }
    }
    /// Construct a pretty sink with a caller-supplied writer.
    pub fn with_writer(w: impl std::io::Write + Send + 'static, use_color: bool) -> Self {
        Self { writer: parking_lot::Mutex::new(Box::new(w)), pretty: true, use_color }
    }
    /// Construct a JSONL sink bound to stdout. Each emitted event
    /// is one JSON object per line; `use_color` is ignored.
    pub fn new_jsonl() -> Self {
        Self { writer: parking_lot::Mutex::new(Box::new(std::io::stdout())), pretty: false, use_color: false }
    }
    /// Construct a JSONL sink with a caller-supplied writer.
    pub fn with_writer_jsonl(w: impl std::io::Write + Send + 'static) -> Self {
        Self { writer: parking_lot::Mutex::new(Box::new(w)), pretty: false, use_color: false }
    }
}

impl TraceSink for StdoutSink {
    fn emit(&self, event: TraceEvent) {
        use std::io::Write;
        let mut guard = self.writer.lock();
        if self.pretty {
            let _ = writeln!(guard, "─── turn {} · role={} · session={} · {} ───",
                event.meta().turn, event.meta().role, event.meta().session_id, event.meta().ts);
            let _ = writeln!(guard, "[{}]", event.variant_name());
            let body = event.body_for_pretty();
            for line in body.lines() {
                let _ = writeln!(guard, "  {}", line);
            }
        } else {
            let json = serde_json::to_string(&event).expect("TraceEvent serialization");
            let _ = writeln!(guard, "{}", json);
        }
        let _ = guard.flush();
    }
}

/// Always-on metadata-only index. Strips content fields to keep the
/// index small and safe for always-on writing.
pub struct IndexSink {
    inner: parking_lot::Mutex<std::io::BufWriter<std::fs::File>>,
    path: PathBuf,
}

impl IndexSink {
    pub fn new(path: PathBuf) -> Self {
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                panic!("IndexSink: failed to create parent dir {}: {}", parent.display(), e);
            }
        }
        let file = std::fs::OpenOptions::new()
            .create(true).append(true).open(&path)
            .expect("IndexSink open");
        Self { inner: parking_lot::Mutex::new(std::io::BufWriter::new(file)), path }
    }
    pub fn path(&self) -> &PathBuf { &self.path }
}

impl TraceSink for IndexSink {
    fn emit(&self, event: TraceEvent) {
        use std::io::Write;
        let mut guard = self.inner.lock();
        let index = event.to_index_line();
        if let Some(line) = index {
            let json = serde_json::to_string(&line).expect("index serialization");
writeln!(guard, "{}", json).expect("IndexSink write");
        }
    }
}

/// Routes a single emit to N child sinks in order. Each child runs in
/// the caller's thread (no async/spawning); sink implementations are
/// expected to be cheap or internally-thread-safe.
pub struct FanoutSink { sinks: Vec<std::sync::Arc<dyn TraceSink>> }

impl FanoutSink {
    pub fn new(sinks: Vec<std::sync::Arc<dyn TraceSink>>) -> Self { Self { sinks } }
    pub fn push(&mut self, s: std::sync::Arc<dyn TraceSink>) { self.sinks.push(s); }
}

impl TraceSink for FanoutSink {
    fn emit(&self, event: TraceEvent) {
        use std::panic::{catch_unwind, AssertUnwindSafe};
        for s in &self.sinks {
            // Per spec §14: isolate child panics so one bad sink doesn't
            // drop the events the other sinks were supposed to see.
            // AssertUnwindSafe is required because `TraceSink` carries no
            // UnwindSafe guarantees; the children own their own state.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                s.emit(event.clone());
            }));
        }
    }
}

/// Wraps another sink and overrides the `role` field on every event.
/// Used by `register_delegate_tool` to tag specialist events with the
/// specialist's role id without the caller having to remember.
pub struct ScopedSink {
    inner: std::sync::Arc<dyn TraceSink>,
    role: String,
}

impl ScopedSink {
    pub fn new(inner: std::sync::Arc<dyn TraceSink>, role: String) -> Self { Self { inner, role } }
}

impl TraceSink for ScopedSink {
    fn emit(&self, mut event: TraceEvent) {
        match &mut event {
            TraceEvent::SessionStart { meta, .. }
            | TraceEvent::PromptBuilt { meta, .. }
            | TraceEvent::ModelCall { meta, .. }
            | TraceEvent::ModelRawOut { meta, .. }
            | TraceEvent::ParseToolCalls { meta, .. }
            | TraceEvent::ToolExec { meta, .. }
            | TraceEvent::ToolRetry { meta, .. }
            | TraceEvent::HookFired { meta, .. }
            | TraceEvent::TurnEnd { meta, .. }
            | TraceEvent::SessionEnd { meta, .. }
            | TraceEvent::SessionStarted { meta, .. }
            | TraceEvent::SessionPaused { meta, .. }
            | TraceEvent::SessionResumed { meta, .. }
            | TraceEvent::RoleInjected { meta, .. }
            | TraceEvent::AskHuman { meta, .. }
            | TraceEvent::RoundStarted { meta, .. }
            | TraceEvent::RoundEnded { meta, .. }
            | TraceEvent::CheckpointCreated { meta, .. }
            | TraceEvent::CheckpointRolledBack { meta, .. }
            | TraceEvent::ModelCallFailed { meta, .. }
            | TraceEvent::ModelCooldownEntered { meta, .. }
            | TraceEvent::FallbackTriggered { meta, .. } => {
                meta.role = self.role.clone();
            }
        }
        self.inner.emit(event);
    }
}

/// Drops events whose `variant_name()` isn't in the allow-list. Used by
/// the `--debug-events` CLI flag to filter the on-stdout debug stream
/// down to a subset of `TraceEvent` variants (e.g. `ToolExec,HookFired`).
/// The JsonlSink / IndexSink stay unfiltered — operators still want
/// the full trace on disk; the filter is only for the live stdout view.
/// `allow: "all"` (or empty) disables filtering; any other comma-separated
/// string is treated as variant names. Comparison is exact-match against
/// `TraceEvent::variant_name()` (e.g. `"ToolExec"`, `"HookFired"`,
/// `"ModelCall"`).
pub struct FilterSink {
    inner: std::sync::Arc<dyn TraceSink>,
    /// Set of allowed variant names. `None` means "all" (no filter);
    /// `Some(empty)` means "match nothing" (filter out everything).
    allowed: Option<std::collections::HashSet<String>>,
}

impl FilterSink {
    /// Build a `FilterSink` from a comma-separated list. `"all"` or
    /// `""` means no filtering (every event passes through). Otherwise the
    /// string is split on `,` and each token is treated as a variant
    /// name. Whitespace around tokens is trimmed.
    ///
    /// Unknown variant names pass through silently — the filter is
    /// best-effort and shouldn't fail the program if a user typo'd
    /// `ToolExecution` instead of `ToolExec`. Operators will simply
    /// see fewer events than they expected, which is recoverable.
    pub fn new(inner: std::sync::Arc<dyn TraceSink>, filter: &str) -> Self {
        let trimmed = filter.trim();
        let allowed = if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("all") {
            None
        } else {
            Some(
                trimmed
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
            )
        };
        Self { inner, allowed }
    }
}

impl TraceSink for FilterSink {
    fn emit(&self, event: TraceEvent) {
        if let Some(allowed) = &self.allowed {
            // `variant_name()` returns `&'static str`; `HashSet<String>`
            // lookups accept `&str` via `Borrow`, so no allocation here.
            if !allowed.contains(event.variant_name()) {
                return;
            }
        }
        self.inner.emit(event);
    }
}

/// Format a Unix epoch second count as `YYYY-MM-DDTHH:MM:SSZ` (UTC).
/// Exposed for use by `checkpoint.rs` so we don't duplicate the
/// proleptic-Gregorian math already in this module.
pub fn iso8601_utc_now_for(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = epoch_to_ymdhms(secs);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo, d, h, mi, s)
}
#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    // VecSink test helper used across all trace tests
    pub struct VecSink(pub Mutex<Vec<TraceEvent>>);
    impl TraceSink for VecSink {
        fn emit(&self, e: TraceEvent) {
            self.0.lock().push(e);
        }
    }
    impl VecSink {
        pub fn drain(&self) -> Vec<TraceEvent> {
            std::mem::take(&mut *self.0.lock())
        }
    }

    #[test]
    fn null_sink_emits_without_panic() {
        let s = NullSink;
        s.emit(TraceEvent::SessionEnd {
            meta: TraceMeta::test_default(),
            total_turns: 1,
            total_input: 10,
            total_output: 20,
            total_thinking: 0,
        });
    }

    #[test]
    fn vec_sink_captures() {
        let s = VecSink(Mutex::new(vec![]));
        s.emit(TraceEvent::SessionEnd {
            meta: TraceMeta::test_default(),
            total_turns: 2,
            total_input: 100,
            total_output: 200,
            total_thinking: 5,
        });
        s.emit(TraceEvent::SessionEnd {
            meta: TraceMeta::test_default(),
            total_turns: 3,
            total_input: 110,
            total_output: 210,
            total_thinking: 6,
        });
        assert_eq!(s.drain().len(), 2);
    }

    #[test]
    fn jsonl_sink_writes_one_line_per_event() {
        let dir = std::env::temp_dir().join(format!("latte-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trace.jsonl");
        let sink = JsonlSink::new(path.clone());
        sink.emit(TraceEvent::SessionEnd {
            meta: TraceMeta::test_default(),
            total_turns: 1, total_input: 10, total_output: 20, total_thinking: 0,
        });
        sink.emit(TraceEvent::SessionEnd {
            meta: TraceMeta::test_default(),
            total_turns: 2, total_input: 100, total_output: 200, total_thinking: 5,
        });
        drop(sink);  // flush BufWriter
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "expected 2 JSONL lines, got: {:?}", content);
        // Each line must be valid JSON with a "SessionEnd" key
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(v.get("SessionEnd").is_some(), "line missing SessionEnd: {}", line);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Regression: 之前 `ModelErrorKind::Other(String)` 是 tagged newtype variant，
    /// `#[serde(tag = "kind")]` 模式不支持 newtype，导致 `JsonlSink::emit`
    /// 在 `serde_json::to_string` 处 panic（"cannot serialize tagged newtype
    /// variant ... containing a string"），整条 trace 写不进去。
    /// 同样的坑也存在于 Stream/Serde/Config（newtype variants），全
    /// 改成 `{ message: String }` 后必须都能 round-trip。
    /// 这个测试同时覆盖了"kind 字段是 snake_case" 和 "message 字段被保留"。
    #[test]
    fn model_error_kind_struct_variants_serialize() {
        let cases = vec![
            (ModelErrorKind::Timeout, r#"{"kind":"timeout"}"#),
            (ModelErrorKind::ConnectFailed, r#"{"kind":"connect_failed"}"#),
            (ModelErrorKind::Auth, r#"{"kind":"auth"}"#),
            (ModelErrorKind::ModelNotFound, r#"{"kind":"model_not_found"}"#),
            (
                ModelErrorKind::RateLimited { retry_after_secs: 1.5 },
                r#"{"kind":"rate_limited","retry_after_secs":1.5}"#,
            ),
            (
                ModelErrorKind::Http { status: 503, message: "down".into() },
                r#"{"kind":"http","status":503,"message":"down"}"#,
            ),
            (
                ModelErrorKind::Stream { message: "sse died".into() },
                r#"{"kind":"stream","message":"sse died"}"#,
            ),
            (
                ModelErrorKind::Serde { message: "bad json".into() },
                r#"{"kind":"serde","message":"bad json"}"#,
            ),
            (
                ModelErrorKind::Config { message: "no api_key".into() },
                r#"{"kind":"config","message":"no api_key"}"#,
            ),
            (
                ModelErrorKind::CooldownHit { cooldown_remaining_secs: 10.0 },
                r#"{"kind":"cooldown_hit","cooldown_remaining_secs":10.0}"#,
            ),
            (
                ModelErrorKind::Other { message: "http: error decoding response body".into() },
                r#"{"kind":"other","message":"http: error decoding response body"}"#,
            ),
        ];
        for (kind, expected_json) in cases {
            let json = serde_json::to_string(&kind)
                .unwrap_or_else(|e| panic!("serialize {:?} failed: {}", kind, e));
            // 精确匹配 JSON 输出：确保字段名是 snake_case 且 message 保留，
            // 不允许 silently 改成其他表示。
            assert_eq!(json, expected_json, "unexpected JSON for {:?}", kind);
            // 反过来也要能解出来：保证反序列化路径不会因为字段缺失而 panic。
            let back: ModelErrorKind = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("deserialize {} failed: {}", json, e));
            assert_eq!(back, kind, "round-trip mismatch for {:?}", kind);
        }
    }

    /// 直接打 JsonlSink：保证 "Other" 事件能完整写进文件，
    /// 不再让 trace 写盘 panic 蔓延到上层。
    #[test]
    fn jsonl_sink_handles_other_kind_without_panic() {
        let dir = std::env::temp_dir().join(format!("latte-test-other-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trace.jsonl");
        let sink = JsonlSink::new(path.clone());
        sink.emit(TraceEvent::ModelCallFailed {
            meta: TraceMeta::test_default(),
            model_id: "MiniMax-M3".into(),
            attempt_index: 0,
            latency_ms: 18936,
            error_kind: ModelErrorKind::Other {
                message: "HTTP error: error decoding response body".into(),
            },
            error_message: "HTTP error: error decoding response body".into(),
            cooldown_secs: Some(10),
        });
        drop(sink);
        let content = std::fs::read_to_string(&path).unwrap();
        let line = content.lines().next().expect("trace file empty");
        let v: serde_json::Value = serde_json::from_str(line)
 .unwrap_or_else(|e| panic!("trace jsonl unreadable: {} -- line: {}", e, line));
        // 关键不变量：error_kind.kind 字段是 "other"，message 字段保留原文，
        // 防止 someone 重新把 Other 改回 newtype variant 又 panic 一次。
        assert_eq!(v["ModelCallFailed"]["error_kind"]["kind"], "other");
        assert_eq!(
            v["ModelCallFailed"]["error_kind"]["message"],
            "HTTP error: error decoding response body"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn jsonl_sink_concurrent_emits_do_not_interleave() {
        // Characterization test: the struct doc claims concurrent emit() calls
        // don't interleave bytes. If the Mutex<BufWriter> ever regressed (e.g.
        // switched to per-call open/append), serde_json::from_str on each line
        // would fail because mid-line byte mixing produces invalid JSON.
        let dir = std::env::temp_dir().join(format!("latte-test-concurrent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trace-concurrent.jsonl");
        let sink = std::sync::Arc::new(JsonlSink::new(path.clone()));

        let mut handles = Vec::new();
        for thread_idx in 0u32..4 {
            let sink = std::sync::Arc::clone(&sink);
            handles.push(std::thread::spawn(move || {
                for _ in 0..25 {
                    sink.emit(TraceEvent::SessionEnd {
                        meta: TraceMeta::test_default(),
                        total_turns: thread_idx,
                        total_input: 0,
                        total_output: 0,
                        total_thinking: 0,
                    });
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        drop(sink);

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 100, "expected 100 JSONL lines, got: {}", lines.len());

        // Each line must be valid JSON whose SessionEnd.total_turns matches
        // the originating thread index. Any byte-mixing between threads
        // would either fail JSON parsing or yield a total_turns value that
        // does not match any single thread index.
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("line not valid JSON (byte interleaving?): {} -- {}", line, e));
            let se = v.get("SessionEnd")
                .unwrap_or_else(|| panic!("line missing SessionEnd: {}", line));
            let total_turns = se.get("total_turns")
                .and_then(|x| x.as_u64())
                .expect("total_turns missing or not u64");
            assert!(
                total_turns < 4,
                "total_turns={} from a thread index 0..=3 — line is corrupt: {}",
                total_turns, line
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn jsonl_sink_appends_to_existing_file() {
        // Characterization test: append-mode is load-bearing for re-running
        // debug sessions. JsonlSink::new must NOT truncate prior content.
        let dir = std::env::temp_dir().join(format!("latte-test-append-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trace-append.jsonl");

        // Pre-existing line written via std::fs::write (independent of sink).
        std::fs::write(&path, "{\"pre\":1}\n").unwrap();

        let sink = JsonlSink::new(path.clone());
        sink.emit(TraceEvent::SessionEnd {
            meta: TraceMeta::test_default(),
            total_turns: 7,
            total_input: 0,
            total_output: 0,
            total_thinking: 0,
        });
        drop(sink);

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "expected 2 lines (1 prior + 1 new), got: {:?}", content);
        // The new (second) line must be the SessionEnd event we emitted.
        let v: serde_json::Value = serde_json::from_str(lines[1])
            .expect("second line not valid JSON");
        assert!(v.get("SessionEnd").is_some(), "second line missing SessionEnd: {}", lines[1]);
        std::fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn stdout_sink_writes_to_writer() {
        let buf = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
        let writer = StdoutSinkWriter(buf.clone());
        let sink = StdoutSink::with_writer(writer, false /* no color */);
        sink.emit(TraceEvent::SessionEnd {
            meta: TraceMeta::test_default(),
            total_turns: 1, total_input: 10, total_output: 20, total_thinking: 0,
        });
        let out = String::from_utf8(buf.lock().clone()).unwrap();
        assert!(out.contains("SessionEnd"), "missing variant: {}", out);
        assert!(out.contains("test-session"), "missing session_id: {}", out);
    }

    /// Test-only writer adapter so we can capture stdout in tests.
    pub struct StdoutSinkWriter(pub std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);
    impl std::io::Write for StdoutSinkWriter {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }

    #[test]
    fn index_sink_writes_session_end_to_disk() {
        // TODO(Task 4): re-add "strips payload" assertion against a
        // content-bearing variant (e.g. ModelRawOut) once that variant
        // lands. The current 2 variants (SessionStart, SessionEnd) carry
        // no payload fields, so this test only verifies on-disk write.
        let dir = std::env::temp_dir().join(format!("latte-test-idx-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.idx");
        let sink = IndexSink::new(path.clone());
        let mut meta = TraceMeta::test_default();
        meta.role = "manager".into();
        sink.emit(TraceEvent::SessionEnd {
            meta,
            total_turns: 1, total_input: 10, total_output: 20, total_thinking: 0,
        });
        drop(sink);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("SessionEnd"));
        assert!(content.contains("\"role\":\"manager\""));
        std::fs::remove_dir_all(&dir).ok();
    }

    struct PanickingSink;
    impl TraceSink for PanickingSink {
        fn emit(&self, _event: TraceEvent) { panic!("intentional"); }
    }

    #[test]
    fn fanout_sink_isolates_child_panic() {
        use std::sync::Arc;
        // Order matters: panicking sink FIRST, normal sink SECOND.
        // Without catch_unwind, the second sink would never receive
        // the event because the test process would unwind through it.
        let ok = Arc::new(VecSink(parking_lot::Mutex::new(vec![])));
        let bad: Arc<dyn TraceSink> = Arc::new(PanickingSink);
        let fan = FanoutSink::new(vec![bad, ok.clone()]);
        fan.emit(TraceEvent::SessionEnd {
            meta: TraceMeta::test_default(),
            total_turns: 1, total_input: 1, total_output: 1, total_thinking: 0,
        });
        assert_eq!(
            ok.0.lock().len(),
            1,
            "sibling sink must still receive event after sibling panics",
        );
    }

    #[test]
    fn fanout_sink_routes_to_all_children() {
        let s1 = std::sync::Arc::new(VecSink(parking_lot::Mutex::new(vec![])));
        let s2 = std::sync::Arc::new(VecSink(parking_lot::Mutex::new(vec![])));
        let fan = FanoutSink::new(vec![s1.clone(), s2.clone()]);
        fan.emit(TraceEvent::SessionEnd {
            meta: TraceMeta::test_default(),
            total_turns: 1, total_input: 1, total_output: 1, total_thinking: 0,
        });
        assert_eq!(s1.0.lock().len(), 1);
        assert_eq!(s2.0.lock().len(), 1);
    }

    #[test]
    fn scoped_sink_overrides_role() {
        // Use a fresh local VecSink; the test-only VecSink has pub inner field
        let inner = std::sync::Arc::new(VecSink(parking_lot::Mutex::new(vec![])));
        let scoped = ScopedSink::new(inner.clone(), "programmer".into());
        let mut meta = TraceMeta::test_default();
        meta.role = "WRONG".into();
        scoped.emit(TraceEvent::SessionEnd {
            meta,
            total_turns: 1, total_input: 1, total_output: 1, total_thinking: 0,
        });
        let captured = inner.0.lock();
        assert_eq!(captured.len(), 1);
        match &captured[0] {
            TraceEvent::SessionEnd { meta, .. } => {
                assert_eq!(meta.role, "programmer", "scoped sink did not override role");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn trace_event_variants_serialize_roundtrip() {
        // Task 4: all 9 variants must survive JSON round-trip. Each
        // arm constructs the variant with realistic-looking data,
        // serializes via serde_json, parses back, and asserts the
        // discriminant (variant name) is preserved.
        let mut meta = TraceMeta::test_default();
        meta.role = "tester".into();
        let events: Vec<TraceEvent> = vec![
            TraceEvent::SessionStart {
                meta: meta.clone(),
                tier: "standard".into(),
                model_chain: vec!["glm-5.2".into(), "deepseek-v4-flash".into()],
                allowed_tools: vec!["read".into(), "bash".into()],
            },
            TraceEvent::PromptBuilt {
                meta: meta.clone(),
                system_rendered: "you are a tester".into(),
                history_len: 3,
                user_input: "analyze chat.rs".into(),
                est_input_tokens: 1234,
            },
            TraceEvent::ModelCall {
                meta: meta.clone(),
                model_id: "glm-5.2".into(),
                params_json: r#"{"temperature":0.5}"#.into(),
                latency_ms: 2741,
                finish_reason: "stop".into(),
            },
            TraceEvent::ModelRawOut {
                meta: meta.clone(),
                raw_content: "<tool_callexec> {\"command\": \"pwd\"}</tool_call>".into(),
            },
            TraceEvent::ParseToolCalls {
                meta: meta.clone(),
                raw_in: "<tool_callexec> {\"command\": \"pwd\"}</tool_call>".into(),
                parsed: vec![ParsedCall { name: "exec".into(), args: "{}".into() }],
                diagnostics: ParseDiag { opens_found: 1, closes_matched: 1, unmatched_opens: vec![] },
            },
            TraceEvent::ToolExec {
                meta: meta.clone(),
                name: "exec".into(),
                args_json: r#"{"command":"pwd"}"#.into(),
                latency_ms: 50,
                status: ToolStatus::Ok("/Users/zhouguodong".into()),
            },
            TraceEvent::HookFired {
                meta: meta.clone(),
                hook_name: "redact_pii".into(),
                point: HookPoint::PreCall,
                outcome_kind: "mutate".into(),
            },
            TraceEvent::TurnEnd {
                meta: meta.clone(),
                total_input: 100, total_output: 200, total_thinking: 0, elapsed_ms: 5000,
            },
            TraceEvent::SessionEnd {
                meta,
                total_turns: 3, total_input: 300, total_output: 600, total_thinking: 10,
            },
        ];
        assert_eq!(events.len(), 9);
        for e in &events {
            let json = serde_json::to_string(e).expect("serialize");
            let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
            // Each variant serializes as a single-key object whose key
            // is the variant name. Verify the key matches variant_name().
            let key = v.as_object().expect("object").keys().next().expect("one key").clone();
            assert_eq!(key, e.variant_name(), "variant_name mismatch for {}", key);
        }
    }

    #[test]
    fn checkpoint_variants_serde_round_trip() {
        use crate::trace::{TraceEvent, TraceMeta, DiffSummary};
        use crate::checkpoint::{CheckpointTrigger, RollbackMode};

        let meta = TraceMeta {
            turn: 1,
            role: "manager".into(),
            ts: "2026-06-27T00:00:00Z".into(),
            session_id: "fix-redis-bug".into(),
        };

        let created = TraceEvent::CheckpointCreated {
            meta: meta.clone(),
            checkpoint_id: 1,
            git_commit: "abc123".into(),
            trigger_kind: "tool_write".into(),
            diff_summary: DiffSummary { files_changed: 1, insertions: 5, deletions: 0 },
        };
        let json = serde_json::to_string(&created).unwrap();
        let back: TraceEvent = serde_json::from_str(&json).unwrap();
        match back {
            TraceEvent::CheckpointCreated { checkpoint_id, .. } => {
                assert_eq!(checkpoint_id, 1);
            }
            _ => panic!("wrong variant after round-trip"),
        }

        let rolled = TraceEvent::CheckpointRolledBack {
            meta,
            checkpoint_id: 1,
            mode: "full".into(),
            rolled_back_to: "abc123".into(),
        };
        let json = serde_json::to_string(&rolled).unwrap();
        let back: TraceEvent = serde_json::from_str(&json).unwrap();
        match back {
            TraceEvent::CheckpointRolledBack { mode, .. } => assert_eq!(mode, "full"),
            _ => panic!("wrong variant after round-trip"),
        }

        // Sanity: triggers and modes serialize lowercase.
        let trig = serde_json::to_string(&CheckpointTrigger::Explicit).unwrap();
        assert_eq!(trig, "\"explicit\"");
        let mode = serde_json::to_string(&RollbackMode::Code).unwrap();
        assert_eq!(mode, "\"code\"");
    }
}
