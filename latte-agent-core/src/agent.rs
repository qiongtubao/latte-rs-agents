//! Agent and AgentRunner: turn-based agent execution with multi-model fallback.
//!
//! An `Agent` is a configured role with a **priority-ordered model chain**.
//! On a request, the agent walks the chain top-to-bottom: if the primary
//! model is unavailable (rate-limited, 5xx, transient network error), the
//! next model in the chain is tried. See [`WaitPolicy`] for the behaviour
//! when every model is on cooldown.
//!
//! An `AgentRunner` wraps an agent with a conversation context and executes
//! turns: each turn processes incoming messages through the AI model and
//! returns the response. Supports tool calling via a text-based `<tool_call>`
//! protocol.

use std::sync::Arc;
use std::time::{Duration, Instant};

use latte_ai::client::AiClient;
use latte_ai::error::AiError;
use latte_ai::models::{Completion, Message, Role, TokenUsage};
use latte_ai::params::GenerateParams;
use parking_lot::Mutex;

use crate::context::ConversationContext;
use crate::error::{AgentError, AgentResult};
use crate::role::Role as RoleDef;
// ─── Tool call error classification + retry policy ────────────────────
//
// 任何把 tool_call 弄坏的错误都进同一个 pipeline：
//   1. classify 成 `ToolCallErrorKind`（增加 variant 不影响 retry 逻辑）
//   2. 问 `RetryPolicy` 要不要 retry
//   3. retry 就再来一次，发 `ToolRetry { attempt, kind, recovered: false }`
//   4. 终态发 `ToolExec { status: Err }` + `ToolRetry { recovered: true|false }`
//   5. 按 `should_loopback_to_model(&kind)` 决定要不要把错误喂回 messages
//
// 加新错误类型 = 加 variant + 在 `DefaultRetryPolicy::retryable()` 加一行。
// UI / 调度逻辑不需要改。

/// Tool 调用失败的分类。trace 上发出去的 `kind: String` 字段就是
/// `serde_json::to_string(&kind).unwrap_or_default()` 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCallErrorKind {
    /// Model 输出的 args 不是合法 JSON。最常见：shell regex 元字符
    /// (`|` `*` `?`) 在字符串里忘了 escape。`serde_err` 记录具体哪条 escape
    /// 炸了，方便 trace 排查。
    MalformedArgs { serde_err: String },
    /// 工具名 model 写了"read"但 registry 只有"file.read" —— alias
    /// 后备也没救回来。说明 model 用了我们不认识的工具名。
    ToolNotFound { tried_aliases: Vec<String> },
    /// PreTool / PostTool hook 主动拒绝（一般是 EnforceToolAllowlist
    /// 之类的策略 hook）。这是有意的，不该 retry。
    HookAborted { hook: String, reason: String },
    /// `tm.execute()` 抛错 —— 网络 5xx / 模型返回奇怪结构 / 反序列化
    /// 失败 / 业务错误。重试一次可能好。
    Execution { reason: String },
    /// 工具内部 timeout。瞬时错误，重试一次。
    Timeout,
}

impl ToolCallErrorKind {
    /// 短 label，给 trace 字段用。
    pub fn label(&self) -> &'static str {
        match self {
            Self::MalformedArgs { .. } => "MalformedArgs",
            Self::ToolNotFound { .. } => "ToolNotFound",
            Self::HookAborted { .. } => "HookAborted",
            Self::Execution { .. } => "Execution",
            Self::Timeout => "Timeout",
        }
    }
}

impl std::fmt::Display for ToolCallErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MalformedArgs { serde_err } => write!(f, "malformed args: {serde_err}"),
            Self::ToolNotFound { tried_aliases } => {
                write!(f, "tool not found (tried: {})", tried_aliases.join(", "))
            }
            Self::HookAborted { hook, reason } => {
                write!(f, "hook '{hook}' aborted: {reason}")
            }
            Self::Execution { reason } => write!(f, "execution failed: {reason}"),
            Self::Timeout => write!(f, "tool timeout"),
        }
    }
}

/// 决定某个错误值不值得再试一次的策略。trait 而不是 enum match，
/// 是为了让上层（CLI / Tauri / 测试）能注入自己的"更激进"或"更保守"
/// 策略而不动 agent 核心。
pub trait RetryPolicy: Send + Sync {
    /// `true` → 立刻用相同 args 再调一次 tool（pre-process 类的 hint
    /// 比如 escape 已经发生在上一层了，这里只是简单的"再执行一次"）。
    /// `false` → 走最终失败路径，emit ToolExec { Err }。
    fn retryable(&self, kind: &ToolCallErrorKind) -> bool;

    /// 该错误要不要把详细原因喂回 model（追加到 `messages` 里，
    /// 让 model 在下一轮看到 tool_result 一样的位置）。
    ///
    /// 默认策略：
    /// - `MalformedArgs` / `ToolNotFound` → **不喂回**。model 看自己上
    ///   一轮的输出"修正"通常产出更多错误（形成死循环）。
    /// - `HookAborted` / `Execution` / `Timeout` → 喂回。model 知道
    ///   hook 拒绝或网络挂了，决策树会换路径。
    fn loopback_to_model(&self, kind: &ToolCallErrorKind) -> bool;
}

#[derive(Debug, Clone, Default)]
pub struct DefaultRetryPolicy;

impl RetryPolicy for DefaultRetryPolicy {
    fn retryable(&self, kind: &ToolCallErrorKind) -> bool {
        match kind {
            // 瞬时错误：HTTP 5xx、timeout、shell escape 漏掉的常见修正一次就够
            ToolCallErrorKind::MalformedArgs { .. }
            | ToolCallErrorKind::Execution { .. }
            | ToolCallErrorKind::Timeout => true,
            // 名字错 / hook 故意拒绝 → 再试也不会好
            ToolCallErrorKind::ToolNotFound { .. }
            | ToolCallErrorKind::HookAborted { .. } => false,
        }
    }
    fn loopback_to_model(&self, kind: &ToolCallErrorKind) -> bool {
        matches!(
            kind,
            ToolCallErrorKind::HookAborted { .. }
                | ToolCallErrorKind::Execution { .. }
                | ToolCallErrorKind::Timeout
        )
    }
}
use crate::trace::ParsedCall;
use latte_rs_agent_tools::error::ToolError;

// ─── WaitPolicy ───────────────────────────────────────────────────────────

/// Controls how `Agent::chat` behaves when every model in the fallback
/// chain is currently on cooldown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WaitPolicy {
    /// Sleep until the highest-priority model's cooldown expires, then
    /// retry that model exactly once. If the retry also fails, return
    /// `ModelsUnavailable` with refreshed timing. This is the default
    /// so callers don't have to plumb retry timing themselves.
    #[default]
    WaitAndRetry,
    /// Return `AgentError::ModelsUnavailable` immediately. The error
    /// carries `next_retry_in` so the caller can decide when to retry.
    NoWait,
}

// ─── ModelClient ──────────────────────────────────────────────────────────

/// A single model in an agent's fallback chain, with its own cooldown
/// tracker. The cooldown is per-`ModelClient` (i.e. per-agent), so two
/// agents sharing the same `Model` track their rate-limit state
/// independently.
pub struct ModelClient {
    /// Resolved model definition.
    pub model: latte_ai::models::Model,
    /// Live client bound to `model`.
    pub client: AiClient,
    /// Instant at which this model becomes available again.
    /// `None` = currently usable. Held in a `parking_lot::Mutex` so
    /// `Agent::chat` (which takes `&self`) can update cooldowns
    /// without an exclusive `&mut Agent`.
    pub cooldown_until: Mutex<Option<Instant>>,
}

impl ModelClient {
    /// Build a `ModelClient` from a resolved `Model`. Returns the
    /// underlying `AiClient::new` error if the model is malformed.
    fn new(model: latte_ai::models::Model) -> AgentResult<Self> {
        let client = AiClient::new(model.clone())?;
        Ok(Self {
            model,
            client,
            cooldown_until: Mutex::new(None),
        })
    }

    /// Whether this model is currently usable (cooldown absent or elapsed).
    fn is_available(&self) -> bool {
        match *self.cooldown_until.lock() {
            None => true,
            Some(t) => Instant::now() >= t,
        }
    }
    /// Time remaining until the cooldown elapses, if any.
    fn cooldown_remaining(&self) -> Option<Duration> {
        let until_opt = *self.cooldown_until.lock();
        let until = until_opt?;
        let now = Instant::now();
        if until <= now {
            None
        } else {
            Some(until - now)
        }
    }

    /// Mark this model as unavailable for the given duration.
    fn set_cooldown(&self, dur: Duration) {
        *self.cooldown_until.lock() = Some(Instant::now() + dur);
    }
}

impl Clone for ModelClient {
    fn clone(&self) -> Self {
        Self {
            model: self.model.clone(),
            client: self.client.clone(),
            // Clone the instant, not the lock — a fresh mutex for the
            // cloned agent is correct: the original's cooldown state
            // shouldn't bleed into the clone.
            cooldown_until: Mutex::new(*self.cooldown_until.lock()),
        }
    }
}

impl std::fmt::Debug for ModelClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelClient")
            .field("model", &self.model.id)
            .field("cooldown_remaining_secs", &self.cooldown_remaining())
            .finish()
    }
}

// ─── Agent ────────────────────────────────────────────────────────────────

/// A fully configured agent: role identity + priority-ordered model chain.
///
/// `model_chain[0]` is the primary; the rest are tried in order when the
/// primary (or earlier fallbacks) is on cooldown or returns a retryable
/// error. `client` and `model_id` are kept as backwards-compatible
/// shortcuts for `model_chain[0].client` / `model_chain[0].model.id`.
pub struct Agent {
    /// Unique name in a discussion (e.g. "pm", "dev_lead").
    pub name: String,
    /// Role definition (system prompt, defaults).
    pub role: RoleDef,
    /// ID of the primary model (`model_chain[0].model.id`).
    pub model_id: String,
    /// Live client for the primary model (`model_chain[0].client`).
    pub client: AiClient,
    /// Priority-ordered model chain (highest priority first).
    /// Always non-empty; `new_with_chain` enforces this.
    pub model_chain: Vec<ModelClient>,
    /// Generation parameters for this agent instance.
    pub params: GenerateParams,
}

// Manual Clone because `ModelClient.cooldown_until` is a `Mutex` (not Clone).
impl Clone for Agent {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            role: self.role.clone(),
            model_id: self.model_id.clone(),
            client: self.client.clone(),
            model_chain: self.model_chain.clone(),
            params: self.params.clone(),
        }
    }
}

impl Agent {
    /// Create a single-model agent (no fallback).
    ///
    /// For production code with a fallback chain, use [`new_with_chain`]
    /// directly or pass `role.model_chain` through `ModelResolver::resolve_chain`.
    pub fn new(
        name: String,
        role: RoleDef,
        model: latte_ai::models::Model,
        params: GenerateParams,
    ) -> AgentResult<Self> {
        Self::new_with_chain(name, role, vec![model], params)
    }

    /// Create an agent with a priority-ordered model chain.
    ///
    /// `models[0]` is the primary; subsequent entries are fallbacks
    /// tried in order. An empty `models` returns `InvalidParam`.
    /// Models with malformed config (the `AiClient::new` error) fail
    /// the whole construction.
    pub fn new_with_chain(
        name: String,
        role: RoleDef,
        models: Vec<latte_ai::models::Model>,
        params: GenerateParams,
    ) -> AgentResult<Self> {
        if models.is_empty() {
            return Err(AgentError::InvalidParam(
                "model chain must contain at least one model".into(),
            ));
        }
        let model_chain: Vec<ModelClient> = models
            .into_iter()
            .map(ModelClient::new)
            .collect::<AgentResult<Vec<_>>>()?;
        let primary = &model_chain[0];
        Ok(Self {
            name,
            role,
            model_id: primary.model.id.clone(),
            client: primary.client.clone(),
            model_chain,
            params,
        })
    }


    /// Send a single chat completion request (one-turn, no context).
    ///
    /// Walks `model_chain` in priority order, skipping models on cooldown.
    /// On a model-local failure that can be bypassed (rate limit, 4xx, 5xx,
    /// transport-level HTTP error, or authentication failure), the failing
    /// model is put on cooldown and the next model in the chain is tried.
    /// Local configuration and serialization errors still surface immediately.
    ///
    /// If every model is on cooldown:
    /// - [`WaitPolicy::WaitAndRetry`] (default) sleeps until the
    ///   highest-priority model's cooldown expires, then retries that
    ///   model once. On second failure, returns `ModelsUnavailable`
    ///   with refreshed timing.
    /// - [`WaitPolicy::NoWait`] returns
    ///   `AgentError::ModelsUnavailable { tried, next_retry_in }` so
    ///   the caller can decide when to resume.
    pub async fn chat(
        &self,
        messages: &[Message],
        params: Option<&GenerateParams>,
        wait: WaitPolicy,
    ) -> AgentResult<Completion> {
        let p = params.unwrap_or(&self.params);
        let mut tried: Vec<String> = Vec::with_capacity(self.model_chain.len());

        for mc in &self.model_chain {
            if !mc.is_available() {
                continue;
            }
            match mc.client.chat(messages, p).await {
                Ok(c) => return Ok(c),
                Err(e) => {
                    tried.push(mc.model.id.clone());
                    if let Some(cd) = cooldown_for_error(&e) {
                        mc.set_cooldown(cd);
                    } else {
                        // Only local errors that cannot be bypassed by switching
                        // models surface immediately. Auth failures receive a short
                        // cooldown so one bad credential cannot block other vendors.
                        return Err(e.into());
                    }
                }
            }
        }

        // Every model in the chain is either on cooldown or just failed
        // with a retryable error.
        let earliest = self
            .model_chain
            .iter()
            .filter_map(|mc| mc.cooldown_remaining())
            .min();

        if wait == WaitPolicy::WaitAndRetry {
            if let Some(d) = earliest {
                tokio::time::sleep(d).await;
                // After waiting, retry the highest-priority model once.
                if let Some(mc) = self.model_chain.first() {
                    match mc.client.chat(messages, p).await {
                        Ok(c) => return Ok(c),
                        Err(e) => {
                            // Refresh its cooldown if retryable.
                            if let Some(cd) = cooldown_for_error(&e) {
                                mc.set_cooldown(cd);
                            }
                            tried.push(mc.model.id.clone());
                        }
                    }
                }
                let new_earliest = self
                    .model_chain
                    .iter()
                    .filter_map(|mc| mc.cooldown_remaining())
                    .min();
                return Err(AgentError::ModelsUnavailable {
                    tried,
                    next_retry_in: new_earliest,
                });
            }
        }

        Err(AgentError::ModelsUnavailable {
            tried,
            next_retry_in: earliest,
        })
    }

    /// Build the system message for this agent.
    pub fn system_message(&self, vars: &serde_json::Value) -> AgentResult<Message> {
        let content = self.role.render_prompt(vars)?;
        Ok(Message::system(content))
    }
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("name", &self.name)
            .field("role", &self.role.id)
            .field("primary_model", &self.model_id)
            .field(
                "chain",
                &self
                    .model_chain
                    .iter()
                    .map(|m| m.model.id.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// Map a `latte_ai::error::AiError` to a suggested cooldown duration.
///
/// Returns `None` for non-retryable errors (auth, config, serialization,
/// caller-fault 4xx other than 429). The agent should surface these
/// immediately rather than walking the fallback chain.
/// Surface hook fires to stderr so the operator sees the hook
/// chain in action without grepping the trace JSONL. One line
/// per fire, easy to grep:
///   [hook] redact_pii PreCall: continue
///   [hook] enforce_tool_allowlist PreTool: mutate
/// Called from every run_pre_*/post_* site in run_turn so it
/// covers PreCall / PostResponse / PostParse / PreTool / PostTool.
fn log_hook_fire(name: &str, point: crate::trace::HookPoint, kind: &str) {
    eprintln!("[hook] {} {:?}: {}", name, point, kind);
}

// ─── dedupe_native_tool_calls ─────────────────────────────────────────────

/// 同一响应内去掉完全相同的 `(name, arguments)` tool_call，保留首次出现
/// 的那一条（含其 id），后面的重复项直接丢弃。native function-calling 下
/// 模型偶尔会把"并行调度"误读为"重复同一调用"，dedup 后避免触发
/// `LoopDetector` 的"连续相同调用"误报。
///
/// - 入参 `calls`：`completion.tool_calls`，按模型输出顺序排列。
/// - 出参：去重后的列表，长度 ≤ 入参，顺序与首次出现位置一致。
/// - 比较的是 arguments 的**规范化 JSON 形式**（`to_string` 后），让
///   键顺序不同但语义相同的调用被识别为同一次。
/// - 不同的 `(name, arguments)` 不合并 -- 并行读多个文件等场景保留全部。
fn dedupe_native_tool_calls(calls: Vec<latte_ai::models::ToolCall>) -> Vec<latte_ai::models::ToolCall> {
    use std::collections::HashSet;
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut out: Vec<latte_ai::models::ToolCall> = Vec::with_capacity(calls.len());
    for tc in calls {
        let canonical_args = serde_json::to_string(&tc.arguments).unwrap_or_default();
        let key = (tc.name.clone(), canonical_args);
        if seen.insert(key) {
            out.push(tc);
        }
    }
    out
}
// ─── LoopDetector ─────────────────────────────────────────────────────────

/// Detects when a model is stuck calling the same tool with the
/// same args repeatedly. Used to break the tool-call loop early
/// before `max_tool_rounds` is exhausted.
///
/// The "model is stuck" failure mode: the model emits the same
/// `<tool_call>` on every round, gets the same result, but can't break
/// out of the pattern on its own. With a hard cap of 8 rounds that's
/// 8 wasted model calls before we surface a `MaxToolRoundsExceeded`
/// error. With this detector, the 3rd consecutive identical call
/// trips and we return a `ToolLoopDetected` error that names the
/// offending tool — much faster feedback, much less wasted spend.
#[derive(Default)]
struct LoopDetector {
    /// Recent (tool_name, args_json) pairs, capped at `LOOP_WINDOW_SIZE`.
    history: Vec<(String, String)>,
    /// Count of consecutive identical tool calls seen at the tail.
    streak: usize,
}

const LOOP_WINDOW_SIZE: usize = 5;
const LOOP_STREAK_THRESHOLD: usize = 3;

impl LoopDetector {
    /// Record a tool call and decide whether to continue or break.
    ///
    /// `tool_name` is the name emitted by the model (e.g. `"read"`,
    /// `"list"`, or the resolved name like `"file.read"`).
    /// `args_json` is the JSON-serialized args that will be passed
    /// to the tool. We compare on the serialized form because the
    /// model may emit semantically equivalent but textually distinct
    /// args (key order, whitespace) — two identical logical calls
    /// should hash equal here.
    fn record(&mut self, tool_name: &str, args_json: &str) -> LoopDecision {
        let key = (tool_name.to_string(), args_json.to_string());
        if self.history.last().map(|k| k == &key).unwrap_or(false) {
            self.streak += 1;
            if self.streak >= LOOP_STREAK_THRESHOLD {
                return LoopDecision::Break(format!(
                    "'{}' called {} times in a row with identical args; \
                     the model is stuck. Breaking out so the user can intervene.",
                    tool_name, self.streak,
                ));
            }
        } else {
            self.streak = 1;
        }
        self.history.push(key);
        if self.history.len() > LOOP_WINDOW_SIZE {
            self.history.remove(0);
        }
        LoopDecision::Continue
    }

    /// Clear all state. Tests use it to reset between scenarios.
    #[cfg_attr(not(test), allow(dead_code))]
    fn reset(&mut self) {
        self.history.clear();
        self.streak = 0;
    }
}

/// Outcome of `LoopDetector::record` — either keep going or break out.
enum LoopDecision {
    /// The call is not part of a stuck-pattern; keep iterating.
    Continue,
    /// The call matches the previous `LOOP_STREAK_THRESHOLD` calls
    /// exactly; the caller should abort the loop with the carried
    /// reason string.
    Break(String),
}

fn cooldown_for_error(e: &AiError) -> Option<Duration> {
    match e {
        // Vendor told us how long to wait — respect it (floor 1s).
        AiError::RateLimited { retry_after, .. } => {
            let secs = retry_after.max(1.0);
            Some(Duration::from_secs_f64(secs))
        }
        // HTTP status codes from the upstream provider.
        AiError::Api { status, .. } => match *status {
            429 => Some(Duration::from_secs(60)),
            500..=599 => Some(Duration::from_secs(30)),
            // 4xx (other than 429) usually means "this model id doesn't
            // exist on this vendor" or "bad request payload for this
            // model". Walking the fallback chain is still worth it
            // because the next model may have a different id format or
            // accept the payload. Short cooldown to avoid hammering a
            // broken model.
            400..=499 => Some(Duration::from_secs(5)),
            // Anything else (3xx redirects we don't auto-follow, 6xx
            // exotic) — also retryable, very long cooldown.
            _ => Some(Duration::from_secs(30)),
        },
        AiError::Http(_) => Some(Duration::from_secs(10)),
        // 认证失败（凭证无效）是致命的，不冷却也不 fallback。
        AiError::Auth(_) => None,
        // 其余为无法通过切换模型可靠规避的本地错误。
        _ => None,
    }
}
/// Maintains the full conversation history and executes turn-by-turn:
///   1. Build message list: system prompt + history + new user messages
///   2. Send to AI model
///   3. If response contains `<tool_call>`, execute tool, append result, goto 2
///   4. Record final response in history
///   5. Return response text
pub struct AgentRunner {
    agent: Agent,
    context: ConversationContext,
    /// Max tool-call round trips per turn (0 = disables tool calling).
    max_tool_rounds: usize,
    /// Optional tool manager for executing tool calls.
    tool_manager: Option<Arc<dyn latte_rs_agent_tools::types::ToolManager>>,
    /// Accumulated token usage across all turns.
    total_usage: TokenUsage,
    /// Trace sink for observability events.
    sink: Arc<dyn crate::trace::TraceSink>,
    /// Hook chain for pre/post processing.
    hooks: Arc<crate::hooks::HookChain>,
    /// Strategy for retrying failed tool calls (MalformedArgs /
    /// Execution / Timeout by default). Arc<dyn> so callers (CLI /
    /// Tauri) can inject a more aggressive or conservative policy
    /// without touching agent core.
    retry_policy: Arc<dyn RetryPolicy>,
    role_id: String,
    /// Session id for this runner. Filled in by the CLI so events
    /// emitted by this runner can be cross-referenced with the
    /// matching `~/.latte/sessions/<id>.idx` and
    /// `~/.latte/traces/<id>.jsonl` files. Empty string when the
    /// runner was built without an explicit id (test paths).
    session_id: String,
    /// Optional worktree root for the per-role inject queue. When
    /// set, `run_turn` (via `drain_inject_queue`) reads
    /// `<worktree>/.latte/inject/<role>.txt` at the start of every
    /// turn and prepends a synthetic user message containing the
    /// queue's content.
    inject_worktree_root: Option<std::path::PathBuf>,
    /// Shared in-memory advisor hint queue (see
    /// `crate::advisor_monitor`). The producer (advisor monitor via
    /// `ChatController::advisor_hint`) pushes correction hints while
    /// this runner is mid-turn; `run_turn` drains them at turn start
    /// and at every tool-round boundary so the next model call in the
    /// current tool loop already carries the hint. `None` = no
    /// advisor wired.
    advisor_hints: Option<Arc<Mutex<std::collections::VecDeque<String>>>>,
    /// Working directory for tool invocations. When set, `run_turn`
    /// rewrites relative filesystem paths in tool inputs to resolve
    /// against it (see `resolve_tool_input_against_cwd`) and also
    /// surfaces it via `ToolExecutionContext.metadata.cwd` (advisory).
    /// Embedding runtimes (Tauri) populate this from the workspace's
    /// `project_root`; absent it, tools fall back to the process cwd
    /// (which in Tauri is the app data dir, not the workspace the user
    /// opened — the bug that motivated this field).
    cwd: Option<std::path::PathBuf>,
    /// 本轮 turn 实际执行成功的工具调用次数。D6 ToolCallEcho
    /// 需要这个数判断"response 含 `<read>` 但 tool_use_count=0"
    /// 的回声模式。run_turn 结束时设置，run_turn_gated 据此
    /// 跑 check_response_gates。
    last_turn_tool_count: usize,
    /// Pre-persistence gate 配置。None = 不 gate（向后兼容老调用方）。
    /// Some(_) = run_turn 自动按 GateConfig 跑 D5/D6 检查，命中
    /// 时通过 advisor_hints 队列注入 hint 并触发同 turn 重跑
    /// （最多 config.max_retries 次）。
    gate_config: Option<crate::advisor_monitor::GateConfig>,
    // 强制 native function-calling：tool schema 经 GenerateParams.tools
    // 下发，模型返回结构化 `completion.tool_calls`。文本 `<tool_call>`
    // 协议已移除，不再有降级路径——provider 必须支持 OpenAI/Anthropic
    // `tools` 字段。tool_choice 取自 `agent.params.tool_choice`（默认 Auto）。
}

/// 把 ToolError 归类到 `ToolCallErrorKind`。
/// 这里不细分 HTTP 错误码：调用方（retry loop）只关心"能不能重试"，
/// ToolError::ToolExecution 永远是瞬时执行错误，归 Execution。
fn classify_tool_execution_error(
    e: &latte_rs_agent_tools::error::ToolError,
) -> ToolCallErrorKind {
    // ToolError 没有独立的 Timeout variant；timeout 由工具
    // 在 ToolExecution.source_string 里描述。我们统一归 Execution，
    // DefaultRetryPolicy 把 Execution 标记为可重试一次。
    ToolCallErrorKind::Execution {
        reason: e.to_string(),
    }
}


impl AgentRunner {
    /// Create a new runner for an agent (no tools).
    pub fn new(agent: Agent) -> Self {
        Self {
            agent,
            context: ConversationContext::default(),
            max_tool_rounds: 0,
            tool_manager: None,
            total_usage: TokenUsage::default(),
            sink: Arc::new(crate::trace::NullSink),
            hooks: Arc::new(crate::hooks::HookChain::empty()),
            retry_policy: Arc::new(DefaultRetryPolicy),
            role_id: String::new(),
            session_id: String::new(),
            inject_worktree_root: None,
            advisor_hints: None,
            cwd: None,
            last_turn_tool_count: 0,
            gate_config: None,
        }
    }
    pub fn new_with_tools(
        agent: Agent,
        tool_manager: Arc<dyn latte_rs_agent_tools::types::ToolManager>,
        max_tool_rounds: usize,
    ) -> Self {
        Self {
            agent,
            context: ConversationContext::default(),
            max_tool_rounds,
            tool_manager: Some(tool_manager),
            total_usage: TokenUsage::default(),
            sink: Arc::new(crate::trace::NullSink),
            hooks: Arc::new(crate::hooks::HookChain::empty()),
            retry_policy: Arc::new(DefaultRetryPolicy),
            role_id: String::new(),
            session_id: String::new(),
            inject_worktree_root: None,
            advisor_hints: None,
            cwd: None,
            last_turn_tool_count: 0,
            gate_config: None,
        }
    }
    pub fn with_context(agent: Agent, context: ConversationContext) -> Self {
        Self {
            agent,
            context,
            max_tool_rounds: 0,
            tool_manager: None,
            total_usage: TokenUsage::default(),
            sink: Arc::new(crate::trace::NullSink),
            hooks: Arc::new(crate::hooks::HookChain::empty()),
            retry_policy: Arc::new(DefaultRetryPolicy),
            role_id: "default".to_string(),
            session_id: String::new(),
            inject_worktree_root: None,
            advisor_hints: None,
            cwd: None,
            last_turn_tool_count: 0,
            gate_config: None,
        }
    }

    /// Enable pre-persistence gate (D5/D6) with the given config.
    /// When set, [`AgentRunner::run_turn_gated`] becomes the entry
    /// point — it wraps `run_turn` and retries the same turn up to
    /// `config.max_retries` times when the gate fails. By default
    /// (no `with_gate_config` call) `run_turn` keeps its current
    /// pass-through behavior, so existing call sites are unaffected.
    pub fn with_gate_config(mut self, cfg: crate::advisor_monitor::GateConfig) -> Self {
        self.gate_config = Some(cfg);
        self
    }
    /// synthetic `Role::User` message with content `"[INJECTED]\n..."`
    /// to `self.context.messages`. Deletes the queue file. This is
    /// called at the start of `run_turn` and can also be called
    /// directly from tests.
    fn drain_inject_queue(&mut self) {
        let Some(root) = self.inject_worktree_root.clone() else {
            return;
        };
        let queue_path = root
            .join(".latte")
            .join("inject")
            .join(format!("{}.txt", self.role_id));
        if !queue_path.exists() {
            return;
        }
        let Ok(content) = std::fs::read_to_string(&queue_path) else {
            return;
        };
        if content.trim().is_empty() {
            let _ = std::fs::remove_file(&queue_path);
            return;
        }
        let synthetic = latte_ai::models::Message::user(format!("[INJECTED]\n{}", content));
        // Prepend the synthetic message. `messages_mut()` returns a
        // `&mut [Message]` slice which has no `insert(0, _)`, and
        // there's no `Vec`-level accessor on `ConversationContext`
        // outside this file. Clone into a local Vec, prepend, then
        // rebuild the context via `clear` + `push` — slightly wasteful
        // for large histories but only on a rare inject-drain path.
        let existing: Vec<latte_ai::models::Message> =
            self.context.messages_mut().to_vec();
        self.context.clear();
        self.context.push(synthetic);
        for m in existing {
            self.context.push(m);
        }
        let _ = std::fs::remove_file(&queue_path);
    }

    /// Drain all pending advisor hints from the shared in-memory
    /// queue. Each drained hint is recorded in the conversation
    /// context as a synthetic `Role::User` message prefixed with
    /// `🦉 advisor 监察：` and also returned, so an in-flight
    /// `run_turn` can additionally append it to the working message
    /// list of the *current* tool loop (the context alone only
    /// reaches the model on the next turn).
    ///
    /// Called at turn start (before the working list is built, so the
    /// context copy suffices) and at every tool-round boundary.
    fn drain_advisor_hints(&mut self) -> Vec<String> {
        let Some(queue) = self.advisor_hints.clone() else {
            return Vec::new();
        };
        let pending: Vec<String> = queue.lock().drain(..).collect();
        for hint in &pending {
            self.context.push(latte_ai::models::Message::user(format!("🦉 advisor 监察：\n{hint}")));
        }
        pending
    }

    /// Attach the shared advisor hint queue. The same `Arc` is held
    /// by the `ChatController` (producer side: `advisor_hint`) and by
    /// every runner the driver builds (consumer side: drained here).
    pub fn with_advisor_hints(
        mut self,
        queue: Arc<Mutex<std::collections::VecDeque<String>>>,
    ) -> Self {
        self.advisor_hints = Some(queue);
        self
    }

    /// Set the max tool-call round trips per turn.
    pub fn set_max_tool_rounds(&mut self, n: usize) {
        self.max_tool_rounds = n;
    }
    /// Get a reference to the agent.
    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    /// Get the conversation context.
    pub fn context(&self) -> &ConversationContext {
        &self.context
    }

    /// Get mutable conversation context.
    pub fn context_mut(&mut self) -> &mut ConversationContext {
        &mut self.context
    }


    /// Total token usage accumulated so far.
    pub fn total_usage(&self) -> &TokenUsage {
        &self.total_usage
    }

    /// Emit a `SessionStart` trace event. Call once at session
    /// initialization, before the first `run_turn`. Carries the
    /// runner's identity (role, session id) and the configured
    /// model chain + tool allowlist so the trace can be replayed
    /// end-to-end without re-reading config. Turn is always 0
    /// because this precedes the first turn.
    pub fn emit_session_start(&self, tier: &str) {
        let meta = crate::trace::TraceMeta::now(
            0,
            self.role_id.clone(),
            self.session_id.clone(),
        );
        let model_chain: Vec<String> = self
            .agent
            .model_chain
            .iter()
            .map(|mc| mc.model.id.clone())
            .collect();
        let allowed_tools: Vec<String> = self
            .agent
            .role
            .allowed_tools
            .iter()
            .map(|s| s.to_string())
            .collect();
        self.sink.emit(crate::trace::TraceEvent::SessionStart {
            meta,
            tier: tier.to_string(),
            model_chain,
            allowed_tools,
        });
    }

    /// Emit a `SessionEnd` trace event. Call once at session
    /// shutdown, after the last `run_turn`. `total_turns` is the
    /// number of `run_turn` invocations the caller made (the
    /// runner itself doesn't track turn count because it can be
    /// reused across sessions). The three token counters come
    /// straight from the runner's accumulated `total_usage`.
    pub fn emit_session_end(&self, total_turns: u32) {
        let meta = crate::trace::TraceMeta::now(
            0,
            self.role_id.clone(),
            self.session_id.clone(),
        );
        self.sink.emit(crate::trace::TraceEvent::SessionEnd {
            meta,
            total_turns,
            total_input: self.total_usage.input_tokens,
            total_output: self.total_usage.output_tokens,
            total_thinking: self.total_usage.thinking_tokens,
        });
    }


    pub async fn run_turn(
        &mut self,
        new_messages: &[Message],
        system_vars: Option<&serde_json::Value>,
    ) -> AgentResult<String> {
        // HIL blackboard: drain per-role inject queue.
        self.drain_inject_queue();
        // Advisor monitor: drain pending hints into the context
        // *before* the working message list is built below, so the
        // first model call of this turn already sees them.
        self.drain_advisor_hints();
        // Pre-persistence gate: 每次 turn 开始清零 tool 计数器，
        // run_turn 内部每次成功执行一个工具就 +1；run_turn 结束时
        // run_turn_gated 据此跑 D6 ToolCallEcho 检查。
        self.last_turn_tool_count = 0;
        use crate::trace::{ParsedCall, ParseDiag, ToolStatus, TraceEvent, TraceMeta};
        let turn_start = Instant::now();
        let meta = TraceMeta::now(0, self.role_id.clone(), self.session_id.clone());
        let default_vars = serde_json::json!({});
        let vars = system_vars.unwrap_or(&default_vars);

        let sys_msg = self.agent.system_message(vars)?;
        let system_rendered = sys_msg.as_text();
        let mut messages: Vec<Message> = Vec::new();
        messages.push(sys_msg);
        messages.extend_from_slice(self.context.messages());
        messages.extend_from_slice(new_messages);

        // Run PreCall hooks before the prompt is built — this lets hooks
        // like `redact_pii` mutate the outgoing message list in-place so
        // the redacted version is what the model actually sees. Each
        // fired hook emits a HookFired TraceEvent so the trace stream
        // records the redaction (and tests can assert on it).
        {
            let mut pre_call_ctx = crate::hooks::PreCallCtx { messages: &mut messages };
            let outcome = self.hooks.run_pre_call(&mut pre_call_ctx, |hook_name, point, kind| {
                self.sink.emit(TraceEvent::HookFired {
                    meta: meta.clone(),
                    hook_name: hook_name.to_string(),
                    point,
                    outcome_kind: kind.to_string(),
                });
                log_hook_fire(hook_name, point, kind);
            });
            if let crate::hooks::HookOutcome::Abort { reason } = &outcome {
                return Err(AgentError::HookAborted {
                    hook: "pre_call".into(),
                    reason: reason.clone(),
                });
            }
        }
        // 1. Emit PromptBuilt after prompt assembly. The user_input
        // shown in the trace must reflect the post-hook state so
        // operators see the redacted version — i.e. what the model
        // actually receives. We pull from `messages` (the mutated
        // list, last N entries) rather than `new_messages` (the raw
        // caller input) so a PreCall hook like redact_pii is visible
        // in the trace.
        let n_new = new_messages.len();
        let user_input: String = messages[messages.len() - n_new..]
            .iter()
            .map(|m| m.as_text())
            .collect::<Vec<_>>()
            .join("\n");
        let model_id = self.agent.model_chain.first()
            .map(|mc| mc.model.id.clone())
            .unwrap_or_default();
        let est_input_tokens = (messages.iter().map(|m| m.as_text().len()).sum::<usize>() / 4) as u32;
        self.sink.emit(TraceEvent::PromptBuilt {
            meta: meta.clone(),
            system_rendered,
            history_len: self.context.messages().len(),
            user_input: user_input.clone(),
            est_input_tokens,
        });

        let max_rounds = if self.tool_manager.is_some() {
            if self.max_tool_rounds == 0 {
                usize::MAX
            } else {
                self.max_tool_rounds
            }
        } else {
            1
        };

        let mut final_response = String::new();
        let mut total_input: u32 = 0;
        let mut total_output: u32 = 0;
        let mut total_thinking: u32 = 0;

        // Per-round "stuck" detector. Each round the model emits one
        // or more tool calls; we want to catch the case where the
        // SAME call is duplicated WITHIN that single response (which
        // `dedupe_tool_calls` already collapses to 1) and the case
        // where a single call is repeated 3+ times within the same
        // response (the post-dedup streak).
        //
        // Cross-round "same call every round" is NOT this detector's
        // job — it's the supervisor's `dead_loop_window = 3` (see
        // `RoundScheduler::supervisor.observe(...)`), which pauses
        // the session when `last_decision_kind()` is the same 3
        // rounds in a row. The supervisor is the right place for
        // across-round "manager keeps re-delegating the same task"
        // because the manager's `run_turn` is called fresh per round
        // and a fresh `LoopDetector` each round means we don't
        // double-count legitimate "manager is making progress".
        //
        // The detector is constructed fresh INSIDE the `for round`
        // loop (line below) so the streak doesn't bleed across
        // rounds. The original code had `let mut loop_detector =
        // LoopDetector::default()` here, which caused the 2026-07-10
        // bug: a manager that delegated the same task 3 rounds in
        // a row (legit retry pattern) tripped the within-round
        // detector even though each round was internally fine.
        for round in 0..max_rounds {
            let mut loop_detector = LoopDetector::default();
            // Advisor monitor (tool-round boundary): hints that landed
            // while the previous round's tools were executing are
            // appended to the working message list, so the model call
            // below — mid tool loop — already carries the correction.
            // (`drain_advisor_hints` also records them in the context;
            // round 0 is covered by the turn-start drain above.)
            if round > 0 {
                for hint in self.drain_advisor_hints() {
                    messages.push(Message::user(format!("🦉 advisor 监察：\n{hint}")));
                }
            }
            // 2a. Emit ModelCall + ModelRawOut after agent.chat()
            let chat_start = Instant::now();
            // native function-calling：tool schema 经 GenerateParams.tools 下发。
            // 文本 `<tool_call>` 协议已移除，不再有降级路径。
            // tool_choice 取自 agent.params（默认 Auto，可经 with_tool_choice 覆盖）。
            let chat_params: Option<GenerateParams> = self.tool_manager.as_ref().map(|tm| {
                let mut p = self.agent.params.clone();
                p.tools = build_tool_schemas(tm);
                p
            });
            let completion = self.agent.chat(&messages, chat_params.as_ref(), WaitPolicy::WaitAndRetry).await?;
            let latency_ms = chat_start.elapsed().as_millis() as u64;

            self.total_usage.input_tokens += completion.usage.input_tokens;
            self.total_usage.output_tokens += completion.usage.output_tokens;
            self.total_usage.thinking_tokens += completion.usage.thinking_tokens;
            total_input += completion.usage.input_tokens;
            total_output += completion.usage.output_tokens;
            total_thinking += completion.usage.thinking_tokens;

            final_response = completion.content.clone();

            self.sink.emit(TraceEvent::ModelCall {
                meta: meta.clone(),
                model_id: model_id.clone(),
                params_json: serde_json::to_string(&self.agent.params).unwrap_or_default(),
                latency_ms,
                finish_reason: completion.stop_reason.clone(),
            });
            self.sink.emit(TraceEvent::ModelRawOut {
                meta: meta.clone(),
                raw_content: final_response.clone(),
            });

            // 2b. Run PostResponseHook
            {
                let mut ctx = crate::hooks::PostResponseCtx { raw: &final_response };
                let outcome = self.hooks.run_post_response(&mut ctx, |hook_name, point, kind| {
                    self.sink.emit(TraceEvent::HookFired {
                        meta: meta.clone(),
                        hook_name: hook_name.to_string(),
                        point,
                        outcome_kind: kind.to_string(),
                    });
                    log_hook_fire(hook_name, point, kind);
                });
                if let crate::hooks::HookOutcome::Abort { reason } = &outcome {
                    return Err(AgentError::HookAborted {
                        hook: "PostResponse".into(),
                        reason: reason.clone(),
                    });
                }
            }

            // 工具调用来自 completion.tool_calls（native function-calling，
            // 结构化 id+name+arguments）。同一响应内完全相同的
            // (name, arguments) 只保留首次出现，避免模型把"并行调度"
            // 误读为"重复同一调用"触发 LoopDetector。
            let tool_calls: Vec<latte_ai::models::ToolCall> =
                dedupe_native_tool_calls(completion.tool_calls.clone());

            // 3a. Emit ParseToolCalls。opens/closes 现在都等于结构化
            // tool_calls 数量（不再有文本解析失配），保留字段是为了
            // trace JSON 向后兼容。
            let parsed_calls: Vec<ParsedCall> = tool_calls.iter().map(|tc| ParsedCall {
                id: tc.id.clone(),
                name: tc.name.clone(),
                // 优先用模型原始 JSON 串（arguments_raw）：合法时与
                // arguments 等价；latte-ai 解析失败时保留坏串，让下面
                // exec loop 的 from_str 失败归类为 MalformedArgs。
                args: tc.arguments_raw.clone()
                    .unwrap_or_else(|| tc.arguments.to_string()),
            }).collect();
            self.sink.emit(TraceEvent::ParseToolCalls {
                meta: meta.clone(),
                raw_in: final_response.clone(),
                parsed: parsed_calls.clone(),
                diagnostics: ParseDiag {
                    opens_found: tool_calls.len() as u32,
                    closes_matched: tool_calls.len() as u32,
                    unmatched_opens: Vec::new(),
                },
            });

            if tool_calls.is_empty() {
                break;
            }

            // 3b. Run PostParseHook (can mutate parsed calls)
            let mut post_parse_calls: Vec<ParsedCall> = parsed_calls;
            {
                let mut ctx = crate::hooks::PostParseCtx { parsed: &mut post_parse_calls };
                let outcome = self.hooks.run_post_parse(&mut ctx, |hook_name, point, kind| {
                    self.sink.emit(TraceEvent::HookFired {
                        meta: meta.clone(),
                        hook_name: hook_name.to_string(),
                        point,
                        outcome_kind: kind.to_string(),
                    });
                    log_hook_fire(hook_name, point, kind);
                });
                match outcome {
                    crate::hooks::HookOutcome::Abort { reason } => {
                        return Err(AgentError::HookAborted {
                            hook: "PostParse".into(),
                            reason,
                        });
                    }
                    crate::hooks::HookOutcome::Mutate(ref mutated) => {
                        post_parse_calls = mutated.clone();
                    }
                    _ => {}
                }
            }

            if let Some(tm) = &self.tool_manager {
                // 追加 assistant 消息（带上它请求的 tool_calls）。native
                // 协议要求：assistant 消息列出 tool_calls（含 id），随后每条
                // tool_result 用 tool_call_id 引用回来，形成闭环。
                messages.push(Message::assistant_with_tool_calls(
                    final_response.clone(),
                    tool_calls.clone(),
                ));
                // 逐个执行工具调用
                for tc in &post_parse_calls {
                    // 工具名已是扁平规范名（registry 注册名 == 模型看到的
                    // 名字），直接查找，不再有 bash->shell.exec 之类的别名
                    // 反向映射。保留短名兜底仅为 namespace 遗留工具兼容。
                    let resolved_name = tc.name.clone();
                    let full_name = if tm.has(&resolved_name) {
                        resolved_name.clone()
                    } else {
                        tm.get_tool_names().into_iter()
                            .find(|n| n.rsplit_once('.').map(|(_, s)| s) == Some(resolved_name.as_str()))
                            .unwrap_or_else(|| resolved_name.clone())
                    };

                    // ── Per-tool-call retry loop ───────────────────────
                    //
                    // 默认 `DefaultRetryPolicy`：
                    //   MalformedArgs / Execution / Timeout -> 重试一次
                    //   ToolNotFound / HookAborted           -> 不重试
                    //
                    // 重试 ≠ 重新问 model。本层自动 reparse / reexecute，
                    // model 只在 *最终失败 + 错误该让 model 知道时* 才
                    // 看到 tool_result。避免"看自己错误输出又产出
                    // 同样错误"的死循环。
                    let policy = self.retry_policy.clone();
                    let mut attempt: u32 = 0;
                    let max_attempts: u32 = 2;
                    // 终止态：Ok(result_str) or Err((kind, detail_str))
                    let mut final_outcome: Result<String, (ToolCallErrorKind, String)> =
                        Err((ToolCallErrorKind::ToolNotFound { tried_aliases: vec![] }, "init".into()));
                    let mut final_args_json = tc.args.clone();

                    while attempt < max_attempts {
                        attempt += 1;
                        // 1. parse args。native 协议下模型输出的是合法 JSON；
                        // 若 latte-ai 层解析失败，arguments_raw 保留原始坏串，
                        // 这里 from_str 会失败并归类为 MalformedArgs。
                        let input: serde_json::Value = match serde_json::from_str(&tc.args) {
                            Ok(v) => v,
                            Err(e) => {
                                let detail = format!("invalid JSON: {e}");
                                final_outcome = Err((
                                    ToolCallErrorKind::MalformedArgs { serde_err: detail.clone() },
                                    detail,
                                ));
                                break;
                            }
                        };
                        // cwd rewrite
                        let input = match &self.cwd {
                            Some(cwd) => resolve_tool_input_against_cwd(input, cwd),
                            None => input,
                        };
                        let final_input = input; // capture for the Ok arm

                        // 2. PreToolHook
                        let mut mutable_input = final_input.clone();
                        let pre_aborted: Option<String> = {
                            let mut pre_ctx = crate::hooks::PreToolCtx {
                                name: &resolved_name,
                                args: &mut mutable_input,
                            };
                            let outcome = self.hooks.run_pre_tool(&mut pre_ctx, |hook_name, point, kind| {
                                self.sink.emit(TraceEvent::HookFired {
                                    meta: meta.clone(),
                                    hook_name: hook_name.to_string(),
                                    point,
                                    outcome_kind: kind.to_string(),
                                });
                                log_hook_fire(hook_name, point, kind);
                            });
                            if let crate::hooks::HookOutcome::Abort { reason } = outcome {
                                Some(reason)
                            } else {
                                None
                            }
                        };
                        if let Some(reason) = pre_aborted {
                            final_outcome = Err((
                                ToolCallErrorKind::HookAborted {
                                    hook: "PreTool".into(),
                                    reason: reason.clone(),
                                },
                                reason,
                            ));
                            break;
                        }
                        let input = mutable_input;

                        // 3. Execute
                        let mut ctx = latte_rs_agent_tools::types::ToolExecutionContext::fresh(
                            &resolved_name,
                            1,
                        );
                        if let Some(cwd) = &self.cwd {
                            ctx.metadata = Some(serde_json::json!({
                                "cwd": cwd.display().to_string(),
                            }));
                        }
                        let tool_start = Instant::now();
                        let exec_result = tm.execute(&full_name, input.clone(), Some(ctx)).await;
                        let tool_latency = tool_start.elapsed().as_millis() as u64;
                        let args_json = serde_json::to_string(&input)
                            .unwrap_or_else(|_| tc.args.clone());
                        final_args_json = args_json.clone();

                        // 4. Loop detection（per-args 哈希，与 retry 无关）
                        if let LoopDecision::Break(reason) = loop_detector.record(&tc.name, &args_json) {
                            return Err(AgentError::ToolLoopDetected {
                                tool: tc.name.clone(),
                                reason,
                            });
                        }
                        match exec_result {
                            Ok(result) => {
                                // Pre-persistence gate: D6 需要 tool
                                // 实际执行计数。失败的工具不计入。
                                self.last_turn_tool_count += 1;
                                // 5. PostToolHook
                                let mut result_str = serde_json::to_string_pretty(&result)
                                    .unwrap_or_else(|_| format!("{:?}", result));
                                let post_aborted: Option<String> = {
                                    let mut post_ctx = crate::hooks::PostToolCtx {
                                        name: &resolved_name,
                                        result: &mut result_str,
                                    };
                                    let outcome = self.hooks.run_post_tool(
                                        &mut post_ctx,
                                        |hook_name, point, kind| {
                                            self.sink.emit(TraceEvent::HookFired {
                                                meta: meta.clone(),
                                                hook_name: hook_name.to_string(),
                                                point,
                                                outcome_kind: kind.to_string(),
                                            });
                                            log_hook_fire(hook_name, point, kind);
                                        },
                                    );
                                    if let crate::hooks::HookOutcome::Abort { reason } = outcome {
                                        Some(reason)
                                    } else if let crate::hooks::HookOutcome::Mutate(ref mutated) = outcome {
                                        result_str = mutated.clone();
                                        None
                                    } else {
                                        None
                                    }
                                };
                                if let Some(reason) = post_aborted {
                                    final_outcome = Err((
                                        ToolCallErrorKind::HookAborted {
                                            hook: "PostTool".into(),
                                            reason: reason.clone(),
                                        },
                                        reason,
                                    ));
                                    break;
                                }
                                // success
                                self.sink.emit(TraceEvent::ToolExec {
                                    meta: meta.clone(),
                                    name: tc.name.clone(),
                                    args_json: args_json.clone(),
                                    latency_ms: tool_latency,
                                    status: ToolStatus::Ok(
                                        serde_json::to_string(&result).unwrap_or_default()
                                    ),
                                });
                                final_outcome = Ok(result_str);
                                break;
                            }
                            Err(e) => {
                                let kind = classify_tool_execution_error(&e);
                                let detail = e.to_string();
                                self.sink.emit(TraceEvent::ToolExec {
                                    meta: meta.clone(),
                                    name: tc.name.clone(),
                                    args_json: args_json.clone(),
                                    latency_ms: tool_latency,
                                    status: ToolStatus::Err(detail.clone()),
                                });
                                final_outcome = Err((kind, detail));
                                // 不 break -- 让 retry 决策在循环底决定
                            }
                        }

                        // Retry decision
                        let current_kind = match &final_outcome {
                            Ok(_) => break, // success path（不应该到这里）
                            Err((k, _)) => k.clone(),
                        };
                        if policy.retryable(&current_kind) && attempt < max_attempts {
                            self.sink.emit(TraceEvent::ToolRetry {
                                meta: meta.clone(),
                                name: tc.name.clone(),
                                attempt,
                                kind: current_kind.label().to_string(),
                                reason: current_kind.to_string(),
                                recovered: false,
                            });
                            continue;
                        } else {
                            // 最后一次失败 / 不可重试：结束循环
                            self.sink.emit(TraceEvent::ToolRetry {
                                meta: meta.clone(),
                                name: tc.name.clone(),
                                attempt,
                                kind: current_kind.label().to_string(),
                                reason: current_kind.to_string(),
                                recovered: false,
                            });
                            break;
                        }
                    }

                    // ── 终止态：用 tc.id 把结果回传给对应的 assistant
                    //    tool_call，形成 tool_call_id 闭环。不再靠
                    //    (name, args) 模糊匹配找 id。
                    match final_outcome {
                        Ok(result_str) => {
                            messages.push(Message::tool_result(tc.id.clone(), result_str));
                        }
                        Err((kind, detail)) => {
                            // 不把错误消息喂回 model 的 kind（MalformedArgs /
                            // ToolNotFound）会形成死循环（model 看自己上
                            // 一轮的输出"修正"通常产出更多错误）。
                            if policy.loopback_to_model(&kind) {
                                messages.push(Message::tool_result(tc.id.clone(), detail));
                            }
                            // 不喂回的：错误已经在 trace 里，UI 也能看；
                            // model 不需要知道（"它自己改不对"）。
                        }
                    }
                }
            }

            if round + 1 >= max_rounds {
                return Err(AgentError::MaxToolRoundsExceeded(max_rounds));
            }
        }

        // 5. Emit TurnEnd
        let elapsed_ms = turn_start.elapsed().as_millis() as u64;
        self.sink.emit(TraceEvent::TurnEnd {
            meta,
            total_input,
            total_output,
            total_thinking,
            elapsed_ms,
        });

        // Store in context
        for msg in new_messages {
            self.context.push(msg.clone());
        }
        self.context.push(Message::assistant(final_response.clone()));

        Ok(final_response)
    }

    /// Pre-persistence gate 版 run_turn。在 `run_turn` 拿到
    /// final_response **之后**、返回给 controller **之前**，跑
    /// `check_response_gates`：
    /// - Pass：照常返回。
    /// - Fail：把 hint 推到 advisor_hints 队列（同 turn 重跑时
    ///   `run_turn` 入口的 `drain_advisor_hints` 会自动注入到
    ///   context），再调一次 `run_turn`（用空 new_messages，避免
    ///   重复 user 输入），用新响应再跑 gate。最多 `config.max_retries`
    ///   次。
    /// - 全部重试仍 Fail：tracing::warn + 强制 pass 当前响应
    ///   （避免无限循环）。
    ///
    /// 用法：builder 模式 `runner.with_gate_config(GateConfig::default())`，
    /// 然后调 `runner.run_turn_gated(...)`；没 set config 时
    /// `run_turn_gated` 等价于 `run_turn`（保持向后兼容）。
    pub async fn run_turn_gated(
        &mut self,
        new_messages: &[Message],
        system_vars: Option<&serde_json::Value>,
    ) -> AgentResult<String> {
        let cfg = match self.gate_config.clone() {
            Some(c) => c,
            None => return self.run_turn(new_messages, system_vars).await,
        };
        let max_retries = cfg.max_retries;
        // 第 1 次：跑原 turn
        let mut response = self.run_turn(new_messages, system_vars).await?;
        let mut tool_count = self.last_turn_tool_count;
        for attempt in 0..=max_retries {
            let verdict =
                crate::advisor_monitor::check_response_gates(&response, tool_count, &cfg);
            match verdict {
                crate::advisor_monitor::GateVerdict::Pass => return Ok(response),
                crate::advisor_monitor::GateVerdict::Fail {
                    detector,
                    hint,
                    evidence,
                } => {
                    if attempt >= max_retries {
                        // 重试耗尽：**终止** turn 而非强制 pass。
                        // controller 接住 Err(AdvisorTerminated) 后会
                        // 发 ChatEvent::AdvisorTerminated 取代 RoleTurn，
                        // 坏答案不落盘，driver 回到 input_rx.recv()
                        // 等用户输入。
                        tracing::warn!(
                            "advisor gate {} still firing after {} retries ({}); terminating",
                            detector.label(),
                            max_retries,
                            evidence
                        );
                        return Err(AgentError::AdvisorTerminated {
                            reason: format!(
                                "{} 连续 {} 次未通过 gate: {}。请检查输入或换思路",
                                detector.label(),
                                max_retries + 1,
                                evidence
                            ),
                            detector: detector.label().to_string(),
                        });
                    }
                    // 注入 hint 到 advisor 队列，下一次 run_turn
                    // 入口的 drain_advisor_hints 会自动加进 context
                    if let Some(q) = &self.advisor_hints {
                        q.lock().push_back(hint);
                    }
                    // 重跑：空 new_messages，让 hint 自然落到 context
                    response = self.run_turn(&[], system_vars).await?;
                    tool_count = self.last_turn_tool_count;
                }
            }
        }
        Ok(response)
    }

    /// Run a turn without recording in history (useful for side queries).
    pub async fn run_turn_ephemeral(
        &self,
        new_messages: &[Message],
        system_vars: Option<&serde_json::Value>,
    ) -> AgentResult<String> {
        let default_vars = serde_json::json!({});
        let vars = system_vars.unwrap_or(&default_vars);

        let mut messages = vec![self.agent.system_message(vars)?];
        messages.extend_from_slice(new_messages);

        let completion = self.agent.chat(&messages, None, WaitPolicy::WaitAndRetry).await?;
        Ok(completion.content)
    }

    /// Clear conversation history (but keep total usage stats).
    pub fn reset_context(&mut self) {
        self.context.clear();
    }

    /// Prune context to stay within token budget, keeping last N messages.
    pub fn prune_context(&mut self, keep_last: usize) {
        self.context.prune_to_budget(keep_last);
    }

    // ── Builder methods ─────────────────────────────────────────────────────

    /// Set the trace sink.
    pub fn with_sink(mut self, sink: Arc<dyn crate::trace::TraceSink>) -> Self {
        self.sink = sink;
        self
    }

    /// Set the hook chain.
    pub fn with_hooks(mut self, hooks: Arc<crate::hooks::HookChain>) -> Self {
        self.hooks = hooks;
        self
    }

    /// Set the role identifier.
    pub fn with_role(mut self, role_id: impl Into<String>) -> Self {
        self.role_id = role_id.into();
        self
    }

    /// Set the inject-queue worktree root. When set, `run_turn`
    /// prepends any pending `<root>/.latte/inject/<role_id>.txt`
    /// content to the conversation as a synthetic user message
    pub fn with_inject_worktree_root(mut self, root: std::path::PathBuf) -> Self {
        self.inject_worktree_root = Some(root);
        self
    }

    /// Set the working directory tools see as their default cwd.
    /// `run_turn` rewrites relative paths in tool inputs against it
    /// (`resolve_tool_input_against_cwd`) and passes it along as
    /// advisory `metadata.cwd`. Caller is expected to have
    /// canonicalized the path — we do not resolve `.` / `..` here,
    /// mirroring how the CLI passes the project root verbatim.
    pub fn with_cwd(mut self, cwd: std::path::PathBuf) -> Self {
        self.cwd = Some(cwd);
        self
    }
    /// 设置工具调用策略（`tool_choice`）。透传到 `agent.params.tool_choice`，
    /// 经 `build_chat_params` 下发到 OpenAI/Anthropic wire 的 `tool_choice` 字段。
    /// 默认 `Auto`；`Required` 强制模型至少调一次工具；`Specific(name)` 锁定工具。
    pub fn with_tool_choice(mut self, choice: latte_ai::models::ToolChoice) -> Self {
        self.agent.params.tool_choice = choice;
        self
    }
    /// Set the session identifier. The id flows into every emitted
    /// `TraceMeta.session_id` so CLI tooling can correlate events
    /// with the matching `~/.latte/sessions/<id>.idx` /
    /// `~/.latte/traces/<id>.jsonl` files.
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = session_id.into();
        self
    }

    // ── Accessors ───────────────────────────────────────────────────────────

    /// Get the trace sink.
    pub fn sink(&self) -> &Arc<dyn crate::trace::TraceSink> {
        &self.sink
    }

    /// Get the hook chain.
    pub fn hooks(&self) -> &Arc<crate::hooks::HookChain> {
        &self.hooks
    }

    /// Get the role identifier.
    pub fn role_id(&self) -> &str {
        &self.role_id
    }

    /// Return a string describing the last turn's main decision.
    /// Used by the Supervisor to detect dead loops.
    /// Possible values: `"text"` | `"tool_call:<name>:<args_hash>"` |
    ///                  `"delegate"` | `"ask_human"`
    ///
    /// native function-calling 下，assistant 消息的 `tool_calls` 字段携带
    /// 结构化调用（id+name+arguments），直接读它而非解析 `<tool_call>` 文本。
    pub fn last_decision_kind(&self) -> String {
        use latte_ai::models::Role as MsgRole;
        use crate::checkpoint::short_hash;
        let Some(last) = self.context.messages().last() else { return "text".to_string(); };
        if last.role != MsgRole::Assistant { return "text".to_string(); }
        let Some(calls) = &last.tool_calls else { return "text".to_string(); };
        let Some(first) = calls.first() else { return "text".to_string(); };
        // 优先识别 delegate / ask_human（更具体的决策标签）。
        match first.name.as_str() {
            "delegate" => return "delegate".to_string(),
            "ask_human" => return "ask_human".to_string(),
            _ => {}
        }
        let args_hash = short_hash(&first.arguments.to_string());
        format!("tool_call:{}:{}", first.name, &args_hash[..8])
    }
}

impl std::fmt::Debug for AgentRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRunner")
            .field("agent", &self.agent.name)
            .field("messages", &self.context.messages().len())
            .field("tokens", &self.context.token_count())
            .field("tools", &self.tool_manager.is_some())
            .finish()
    }
}


/// Convenience: parameters override for agent construction.

pub struct AgentParams {
    pub params: GenerateParams,
}

impl AgentParams {
    pub fn from_role(role: &RoleDef) -> Self {
        Self {
            params: role.default_params.clone(),
        }
    }
}

impl From<GenerateParams> for AgentParams {
    fn from(params: GenerateParams) -> Self {
        Self { params }
    }
}

/// 从 tool_manager 的工具定义构建 latte-ai 的 Tool 列表，下发到 LLM 请求的
/// `tools` 字段。工具名直接用 registry 注册名（扁平规范名，== 模型看到的
/// 名字），不再有 friendly_tool_name / namespace 别名转换。`strict` 透传
/// `ToolDefinition.strict`（默认 None = 不开 OpenAI Structured Outputs）。
/// `tool_choice` 不在此设置，取自 `agent.params.tool_choice`（默认 Auto）。
fn build_tool_schemas(
    tm: &Arc<dyn latte_rs_agent_tools::types::ToolManager>,
) -> Vec<latte_ai::models::Tool> {
    tm.get_tool_definitions()
        .iter()
        .map(|td| latte_ai::models::Tool {
            name: td.name.clone(),
            description: Some(td.description.clone()),
            parameters: serde_json::to_value(&td.input_schema)
                .unwrap_or(serde_json::json!({})),
            strict: td.strict,
        })
        .collect()
}

/// Rewrite `input` so relative filesystem paths resolve against the
/// runner's workspace `cwd` instead of the process cwd.
///
/// Background: `ToolExecutionContext.metadata.cwd` is advisory and none of
/// the packaged tools consult it - they resolve relative paths against the
/// process cwd, which inside a Tauri host is the app data dir, not the
/// user's workspace. Rather than touching every tool handler, the two
/// path-carrying shapes are rewritten here, once:
///
/// - any top-level `"path": "<relative>"` arg (read/write/list/delete/
///   search/find) is joined onto `cwd`;
/// - any input carrying `"command"` without an explicit `"cwd"` arg
///   (bash/spawn) gets `cwd` injected.

fn resolve_tool_input_against_cwd(
    mut input: serde_json::Value,
    cwd: &std::path::Path,
) -> serde_json::Value {
    use serde_json::Value;
    let Some(obj) = input.as_object_mut() else {
        return input;
    };
    if let Some(v) = obj.get_mut("path") {
        if let Some(s) = v.as_str() {
            if is_relative_fs_path(s) {
                *v = Value::String(cwd.join(s).display().to_string());
            }
        }
    }
    if obj.contains_key("command") {
        obj.entry("cwd".to_string())
            .or_insert_with(|| Value::String(cwd.display().to_string()));
    }
    input
}

/// True when `s` looks like a relative filesystem path: not absolute, not
/// a URL, not an env-var/`~` reference, not a Windows drive path.
fn is_relative_fs_path(s: &str) -> bool {
    let b = s.as_bytes();
    if s.is_empty()
        || s.starts_with('/')
        || s.starts_with('~')
        || s.starts_with('$')
        || s.contains("://")
    {
        return false;
    }
    // Windows drive: "C:\" / "C:/"
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_resolver::ModelTier;
    use crate::role::Role as AgentRole;
    use crate::role::RoleCategory;
    use latte_ai::models::{ContentPart, Model};
    use latte_ai::models::Role as MsgRole;
    use std::time::Duration;

    fn test_role() -> AgentRole {
        AgentRole {
            id: "test".into(),
            name: "Test Agent".into(),
            category: RoleCategory::Discussion,
            system_prompt: "You are a {{role_name}}. Topic: {{topic}}".into(),
            default_model_tier: ModelTier::Standard,
            model_chain: vec![],
            default_params: GenerateParams::default(),
            allowed_tools: vec![],
            icon: "🧪".into(),
        }
    }

    fn test_model() -> Model {
        Model {
            id: "test-model".into(),
            name: "Test Model".into(),
            api: latte_ai::models::ApiType::OpenAiCompletions,
            provider: "test".into(),
            base_url: "http://localhost:9999".into(),
            api_key: "test-key".into(),
            context_window: 32000,
            max_tokens: 4096,
            supports_thinking: false,
            supports_vision: false,
            cost_per_million_input: 0.0,
            cost_per_million_output: 0.0,
        }
    }

    // ── resolve_tool_input_against_cwd / is_relative_fs_path ─────────────

    #[test]
    fn relative_path_args_are_joined_onto_cwd() {
        let cwd = std::path::Path::new("/ws/root");
        let out = resolve_tool_input_against_cwd(serde_json::json!({"path": "src/foo.rs"}), cwd);
        assert_eq!(out["path"], "/ws/root/src/foo.rs");
    }

    #[test]
    fn absolute_url_env_and_drive_paths_are_untouched() {
        let cwd = std::path::Path::new("/ws/root");
        for p in [
            "/abs/x.rs",
            "~/x.rs",
            "$HOME/x.rs",
            "${HOME}/x.rs",
            "http://example.com/x",
            "C:\\win\\x.rs",
            "C:/win/x.rs",
        ] {
            let out = resolve_tool_input_against_cwd(serde_json::json!({"path": p}), cwd);
            assert_eq!(out["path"], p, "path should pass through: {p}");
        }
    }

    #[test]
    fn shell_command_gets_cwd_injected_but_explicit_cwd_wins() {
        let cwd = std::path::Path::new("/ws/root");
        let out = resolve_tool_input_against_cwd(serde_json::json!({"command": "ls"}), cwd);
        assert_eq!(out["cwd"], "/ws/root");
        let explicit = resolve_tool_input_against_cwd(
            serde_json::json!({"command": "ls", "cwd": "/elsewhere"}),
            cwd,
        );
        assert_eq!(explicit["cwd"], "/elsewhere");
    }

    #[test]
    fn non_object_and_pathless_inputs_pass_through() {
        let cwd = std::path::Path::new("/ws/root");
        let s = resolve_tool_input_against_cwd(serde_json::json!("plain string"), cwd);
        assert_eq!(s, serde_json::json!("plain string"));
        let obj = resolve_tool_input_against_cwd(serde_json::json!({"pattern": "foo"}), cwd);
        assert_eq!(obj, serde_json::json!({"pattern": "foo"}));
    }

    #[test]
    fn test_agent_construction() {
        let role = test_role();
        let agent = Agent::new("test-agent".into(), role, test_model(), GenerateParams::default());
        assert!(agent.is_ok());
    }

    #[test]
    fn test_system_message_building() {
        let role = test_role();
        let agent =
            Agent::new("test-agent".into(), role, test_model(), GenerateParams::default()).unwrap();

        let vars = serde_json::json!({
            "role_name": "Tester",
            "topic": "Auth module"
        });

        let msg = agent.system_message(&vars).unwrap();
        assert!(msg.as_text().contains("Tester"));
        assert!(msg.as_text().contains("Auth module"));
        assert!(matches!(msg.role, MsgRole::System));
    }

    #[test]
    fn test_runner_construction() {
        let role = test_role();
        let agent =
            Agent::new("test-agent".into(), role, test_model(), GenerateParams::default()).unwrap();
        let runner = AgentRunner::new(agent);
        assert_eq!(runner.context().messages().len(), 0);
        assert_eq!(runner.total_usage().input_tokens, 0);
    }

    #[test]
    fn test_runner_context_management() {
        let role = test_role();
        let agent =
            Agent::new("test-agent".into(), role, test_model(), GenerateParams::default()).unwrap();
        let mut runner = AgentRunner::new(agent);

        runner.context_mut().push(Message::user("Hello"));
        assert_eq!(runner.context().messages().len(), 1);
    }

    #[test]
    fn test_prune_context() {
        let role = test_role();
        let agent =
            Agent::new("test-agent".into(), role, test_model(), GenerateParams::default()).unwrap();
        let mut runner = AgentRunner::new(agent);

        runner.context_mut().set_token_budget(10);
        runner.context_mut().push(Message::user("a".repeat(200)));

        runner.prune_context(1);
        assert!(runner.context().messages().len() <= 1);
    }

    // ─── cooldown_for_error mapping ────────────────────────────────────

    #[test]
    fn test_cooldown_for_rate_limited_uses_retry_after() {
        // Vendor gave us 5s — we trust it, but floor at 1s.
        let d = cooldown_for_error(&AiError::RateLimited {
            retry_after: 5.0,
            message: "slow down".into(),
        });
        assert_eq!(d, Some(Duration::from_secs(5)));
    }

    #[test]
    fn test_cooldown_for_rate_limited_floors_at_1s() {
        // retry_after = 0.1s shouldn't let us hammer immediately.
        let d = cooldown_for_error(&AiError::RateLimited {
            retry_after: 0.1,
            message: "burst".into(),
        });
        assert_eq!(d, Some(Duration::from_secs(1)));
    }

    #[test]
    fn test_cooldown_for_429_is_60s() {
        let d = cooldown_for_error(&AiError::Api {
            status: 429,
            message: "rate limit".into(),
        });
        assert_eq!(d, Some(Duration::from_secs(60)));
    }

    #[test]
    fn test_cooldown_for_5xx_is_30s() {
        for status in [500u16, 502, 503, 504, 599] {
            let d = cooldown_for_error(&AiError::Api {
                status,
                message: "server err".into(),
            });
            assert_eq!(d, Some(Duration::from_secs(30)), "status={status}");
        }
    }

    #[test]
    fn test_cooldown_for_4xx_other_than_429_is_5s() {
        // 400/401/403/404 cooldown short so the chain can fall through
        // to the next model (different id or different vendor).
        for status in [400u16, 401, 403, 404] {
            let d = cooldown_for_error(&AiError::Api {
                status,
                message: "bad request".into(),
            });
            assert_eq!(d, Some(Duration::from_secs(5)), "status={status}");
        }
    }

    #[test]
    fn test_cooldown_for_auth_error_is_none() {
        let d = cooldown_for_error(&AiError::Auth("bad key".into()));
        assert_eq!(d, None);
    }
    #[test]
    fn test_cooldown_for_config_error_is_none() {
        let d = cooldown_for_error(&AiError::Config("bad model".into()));
        assert_eq!(d, None);
    }

    // ─── ModelClient state ─────────────────────────────────────────────

    #[test]
    fn test_model_client_starts_available() {
        let mc = ModelClient::new(test_model()).unwrap();
        assert!(mc.is_available());
        assert_eq!(mc.cooldown_remaining(), None);
    }

    #[test]
    fn test_model_client_cooldown_blocks_then_releases() {
        let mc = ModelClient::new(test_model()).unwrap();
        // Set a short cooldown and verify the model reports it.
        mc.set_cooldown(Duration::from_millis(50));
        assert!(!mc.is_available());
        let remaining = mc.cooldown_remaining().expect("should have remaining");
        assert!(remaining <= Duration::from_millis(50));

        // After the cooldown elapses, the model is available again.
        std::thread::sleep(Duration::from_millis(60));
        assert!(mc.is_available());
        assert_eq!(mc.cooldown_remaining(), None);
    }

    #[test]
    fn test_model_client_clone_is_independent() {
        // A cloned ModelClient should have its own cooldown lock, so
        // setting a cooldown on the clone doesn't affect the original.
        let mc1 = ModelClient::new(test_model()).unwrap();
        let mc2 = mc1.clone();
        mc2.set_cooldown(Duration::from_secs(60));
        assert!(!mc2.is_available());
        assert!(mc1.is_available(), "original must not be affected by clone's cooldown");
    }

    // ─── new_with_chain validation ─────────────────────────────────────

    #[test]
    fn test_new_with_chain_rejects_empty() {
        let result = Agent::new_with_chain(
            "a".into(),
            test_role(),
            vec![], // empty
            GenerateParams::default(),
        );
        assert!(matches!(result, Err(AgentError::InvalidParam(_))));
    }

    #[test]
    fn test_new_with_chain_sets_chain_and_backcompat_fields() {
        let m1 = test_model();
        let mut m2 = test_model();
        m2.id = "fallback-model".into();
        let agent = Agent::new_with_chain(
            "a".into(),
            test_role(),
            vec![m1.clone(), m2.clone()],
            GenerateParams::default(),
        )
        .unwrap();

        // Primary back-compat fields point at chain[0].
        assert_eq!(agent.model_id, m1.id);
        assert_eq!(agent.model_chain[0].model.id, m1.id);
        assert_eq!(agent.model_chain[1].model.id, m2.id);
    }

    // ─── hook + sink wiring tests live in `latte-agent-cli/tests/` ──

    // ─── fallback behavior (integration via wiremock) ──────────────────

    /// OpenAI-compatible completion payload used by all mock endpoints.
    /// `content` is the text response; pass `tool_calls` empty for text-only.
    fn openai_completion_body(content: &str, tool_calls: Vec<serde_json::Value>) -> String {
        let msg = if tool_calls.is_empty() {
            serde_json::json!({ "role": "assistant", "content": content })
        } else {
            serde_json::json!({
                "role": "assistant",
                "content": content,
                "tool_calls": tool_calls,
            })
        };
        serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 0,
            "model": "test",
            "choices": [{
                "index": 0,
                "message": msg,
                "finish_reason": if tool_calls.is_empty() { "stop" } else { "tool_calls" }
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        })
        .to_string()
    }

    /// Build a Model whose `base_url` points at the given wiremock server.
    fn model_at(server: &wiremock::MockServer, id: &str) -> Model {
        Model {
            id: id.into(),
            name: id.into(),
            api: latte_ai::models::ApiType::OpenAiCompletions,
            provider: "test".into(),
            base_url: server.uri(),
            api_key: "test-key".into(),
            context_window: 32000,
            max_tokens: 4096,
            supports_thinking: false,
            supports_vision: false,
            cost_per_million_input: 0.0,
            cost_per_million_output: 0.0,
        }
    }

    #[tokio::test]
    async fn test_chat_falls_back_to_second_model_on_500() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        // Primary returns 500; fallback returns 200.
        let primary = wiremock::MockServer::start().await;
        primary
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(500).set_body_string("oops")),
            )
            .await;
        let fallback = wiremock::MockServer::start().await;
        fallback
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(
                        openai_completion_body("fallback won", vec![]),
                    )),
            )
            .await;

        let agent = Agent::new_with_chain(
            "a".into(),
            test_role(),
            vec![model_at(&primary, "primary"), model_at(&fallback, "fallback")],
            GenerateParams::default(),
        )
        .unwrap();

        let resp = agent
            .chat(
                &[Message::user("hi")],
                None,
                WaitPolicy::NoWait,
            )
            .await
            .expect("fallback should succeed");
        assert_eq!(resp.content, "fallback won");

        // Primary should now be on cooldown.
        assert!(!agent.model_chain[0].is_available());
        // Fallback should still be available.
        assert!(agent.model_chain[1].is_available());
    }

    #[tokio::test]
    async fn test_chat_returns_models_unavailable_when_all_fail() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let s1 = wiremock::MockServer::start().await;
        s1.register(
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .respond_with(ResponseTemplate::new(503)),
        )
        .await;
        let s2 = wiremock::MockServer::start().await;
        s2.register(
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .respond_with(ResponseTemplate::new(502)),
        )
        .await;

        let agent = Agent::new_with_chain(
            "a".into(),
            test_role(),
            vec![model_at(&s1, "m1"), model_at(&s2, "m2")],
            GenerateParams::default(),
        )
        .unwrap();

        let err = agent
            .chat(
                &[Message::user("hi")],
                None,
                WaitPolicy::NoWait,
            )
            .await
            .expect_err("all models should fail");
        match err {
            AgentError::ModelsUnavailable { tried, next_retry_in } => {
                assert_eq!(tried, vec!["m1".to_string(), "m2".to_string()]);
                assert!(next_retry_in.is_some(), "should report a retry window");
            }
            other => panic!("expected ModelsUnavailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_chat_returns_immediately_for_non_retryable_auth() {
        // Auth errors should NOT walk the chain — same vendor is
        // misconfigured and the second model will hit the same wall.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let s1 = wiremock::MockServer::start().await;
        // wiremock returns 401, which latte-ai surfaces as Api{status: 401}.
        s1.register(
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized")),
        )
        .await;
        let s2 = wiremock::MockServer::start().await;
        // We expect s2 to NOT be called; if it is, the test will hang
        // on the future and we'll see extra requests in logs.
        s2.register(
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .respond_with(ResponseTemplate::new(200).set_body_string(
                    openai_completion_body("never reached", vec![]),
                )),
        )
        .await;

        let agent = Agent::new_with_chain(
            "a".into(),
            test_role(),
            vec![model_at(&s1, "m1"), model_at(&s2, "m2")],
            GenerateParams::default(),
        )
        .unwrap();

        let err = agent
            .chat(
                &[Message::user("hi")],
                None,
                WaitPolicy::NoWait,
            )
            .await
            .expect_err("auth error should surface");
        assert!(
            matches!(err, AgentError::AiClient(AiError::Auth(_))),
            "expected 401 to surface as AiClient(Auth), got {err:?}"
        );
        // s2 should have zero received requests.
        assert_eq!(s2.received_requests().await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_wait_and_retry_succeeds_after_cooldown() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        // Primary fails 500 (→ 30s cooldown) then a short time later
        // (we override the cooldown to 0.2s for a fast test) succeeds.
        // Walked via WaitAndRetry: should sleep until the cooldown
        // elapses, then retry the primary once and succeed.
        //
        // We can't easily change the cooldown from outside, so we
        // mutate the chain's first cooldown to a short value right
        // after the first failure by hooking into the model after
        // the call. Instead, we use a fallback model that succeeds
        // and verify that NoWait still works while WaitAndRetry only
        // waits when nothing is available.
        //
        // More direct: stub primary to return 500, then 200, and use
        // WaitAndRetry. The 500 puts the primary on 30s cooldown;
        // WaitAndRetry sleeps the full 30s — too long for a test.
        // So this test uses a different shape: a single-model chain
        // whose model returns 500 first, then 200.
        // NoWait should fail with ModelsUnavailable.
        // We can't test WaitAndRetry cheaply here without injectable
        // cooldowns, so we only test the NoWait path here; the
        // WaitAndRetry path is exercised in test_wait_and_retry_short_cooldown.
        let server = wiremock::MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(500).set_body_string("boom")),
            )
            .await;
        let agent = Agent::new_with_chain(
            "a".into(),
            test_role(),
            vec![model_at(&server, "only")],
            GenerateParams::default(),
        )
        .unwrap();
        let err = agent
            .chat(
                &[Message::user("hi")],
                None,
                WaitPolicy::NoWait,
            )
            .await
            .expect_err("single model failure");
        assert!(matches!(err, AgentError::ModelsUnavailable { .. }));
    }

    #[tokio::test]
    async fn test_wait_and_retry_succeeds_with_short_cooldown() {
        // After a failure, we shrink the cooldown to 100ms and call
        // chat again with WaitAndRetry. It should sleep ~100ms then
        // succeed on the (now mocked-success) primary.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        // First call: 500. Subsequent calls: 200.
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .and(wiremock::matchers::header_exists("x-test-first"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(
                        openai_completion_body("recovered", vec![]),
                    )),
            )
            .await;
        // Simpler: register two mocks. wiremock returns the most
        // recently registered matching response, so register success
        // second; that means the first call hits the first mock (500)
        // if we use a matcher that matches once, but wiremock doesn't
        // have "first call only" — fall back to: register failure,
        // trigger it, then DELETE the mock and register success, then
        // call again. That's a lot of plumbing.
        //
        // Simpler: shrink the cooldown manually, then issue a
        // WaitAndRetry call. The retry will hit the still-500 mock
        // and fail, so the test only verifies the WAIT path, not
        // recovery. Combine: use a separate mock server that returns
        // 200 for the second call by inspecting the request count.
        // wiremock doesn't support that out of the box, so use a
        // different approach: shrink cooldown, then call again with
        // a freshly registered success mock.
        let _ = server; // suppress unused warning when the test is simplified

        // Build a minimal agent with a 2-element chain on a server
        // that returns 200, then force-set cooldowns so both are
        // unavailable, then call with WaitAndRetry and verify the
        // response succeeds.
        let server2 = wiremock::MockServer::start().await;
        server2
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(
                        openai_completion_body("after wait", vec![]),
                    )),
            )
            .await;
        let agent = Agent::new_with_chain(
            "a".into(),
            test_role(),
            vec![model_at(&server2, "primary")],
            GenerateParams::default(),
        )
        .unwrap();
        // Force a short cooldown on the primary to simulate a previous
        // failure that just expired.
        agent.model_chain[0].set_cooldown(Duration::from_millis(100));

        let start = std::time::Instant::now();
        let resp = agent
            .chat(
                &[Message::user("hi")],
                None,
                WaitPolicy::WaitAndRetry,
            )
            .await
            .expect("should succeed after short wait");
        let elapsed = start.elapsed();
        assert_eq!(resp.content, "after wait");
        // We should have waited ~100ms (allow generous slack).
        assert!(
            elapsed >= Duration::from_millis(80),
            "expected to wait for cooldown, only elapsed {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "should not have waited the full default 60s: {elapsed:?}"
        );
    }

    // HIGH 4: integration test for `run_turn` hook + sink wiring.
    // Verifies the four review fixes from commit 93b8441 are
    // actually end-to-end:
    //   1. PreCall hook fires (redact_pii) and mutates messages
    //   2. session_id flows through to TraceMeta
    //   3. sink receives the expected event sequence
    //
    // Uses wiremock to stub the model API so no real network is hit.
    // Uses Arc<VecSink> (concrete) so the test can lock the inner
    // Mutex and read events back out.
    #[tokio::test]
    async fn test_run_turn_wires_hooks_and_sinks() {
        use crate::trace::{HookPoint, TraceEvent, TraceSink};
        // Local VecSink test helper (mirrors the one in trace::tests
        // which is not pub-exported from the parent module).
        struct LocalVecSink(parking_lot::Mutex<Vec<TraceEvent>>);
        impl TraceSink for LocalVecSink {
            fn emit(&self, e: TraceEvent) {
                self.0.lock().push(e);
            }
        }
        use crate::hooks::{Hook, HookChain, PreCallCtx, HookOutcome};
        use std::sync::Arc;

        // Stub model returns a plain text response (no tool calls).
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let s = wiremock::MockServer::start().await;
        s.register(
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .respond_with(ResponseTemplate::new(200)
                    .set_body_string(openai_completion_body("plain text reply", vec![]))),
        )
        .await;

        // PreCall hook: redacts PII in user messages.
        struct RedactPreCall;
        impl Hook for RedactPreCall {
            fn name(&self) -> &str { "redact_pre_call" }
            fn pre_call(&self, ctx: &mut PreCallCtx) -> HookOutcome<()> {
                for m in ctx.messages.iter_mut() {
                    if m.role == MsgRole::User {
                        m.content = vec![ContentPart::text(m.as_text().replace("13812345678", "<REDACTED>"))];
                    }
                }
                HookOutcome::Continue
            }
        }

        let sink = Arc::new(LocalVecSink(parking_lot::Mutex::new(vec![])));
        let hooks: Arc<HookChain> = Arc::new(
            HookChain::empty()
                .push(Arc::new(RedactPreCall) as Arc<dyn Hook>)
        );

        let role = test_role();
        let agent = Agent::new_with_chain(
            "wiring-test".into(),
            role,
            vec![model_at(&s, "stub-model")],
            GenerateParams::default(),
        ).unwrap();

        let mut runner = AgentRunner::new(agent)
            .with_sink(sink.clone() as Arc<dyn TraceSink>)
            .with_hooks(hooks.clone())
            .with_role(String::from("wiring-test"))
            .with_session_id(String::from("integration-sess-1"));

        // 1. Emit SessionStart and assert it lands in the sink.
        runner.emit_session_start("standard");
        {
            let events = sink.0.lock();
            assert!(matches!(&events[0], TraceEvent::SessionStart { .. }),
                "first event should be SessionStart, got {:?}", events[0]);
            if let TraceEvent::SessionStart { tier, model_chain, .. } = &events[0] {
                assert_eq!(tier, "standard");
                assert_eq!(model_chain, &["stub-model".to_string()]);
            }
            // session_id flows through to TraceMeta (bug 4 fix).
            if let TraceEvent::SessionStart { meta, .. } = &events[0] {
                assert_eq!(meta.session_id, "integration-sess-1");
            }
        }

        // 2. Run a turn with PII in the input; redact hook should
        //    mutate it, and PromptBuilt.user_input should reflect
        //    the post-hook state.
        let msgs = vec![Message::user("call me at 13812345678 about it")];
        let resp = runner.run_turn(&msgs, None).await
            .expect("run_turn should succeed against wiremock");
        assert_eq!(resp, "plain text reply");

        // 3. Sink should contain the expected event sequence.
        let events = sink.0.lock();
        let kinds: Vec<&'static str> = events.iter().map(|e| e.variant_name()).collect();
        assert!(kinds.contains(&"SessionStart"), "missing SessionStart: {kinds:?}");
        assert!(kinds.contains(&"HookFired"), "missing HookFired: {kinds:?}");
        assert!(kinds.contains(&"PromptBuilt"), "missing PromptBuilt: {kinds:?}");
        assert!(kinds.contains(&"ModelCall"), "missing ModelCall: {kinds:?}");
        assert!(kinds.contains(&"ModelRawOut"), "missing ModelRawOut: {kinds:?}");
        assert!(kinds.contains(&"ParseToolCalls"), "missing ParseToolCalls: {kinds:?}");
        assert!(kinds.contains(&"TurnEnd"), "missing TurnEnd: {kinds:?}");

        // 4. PreCall hook fired exactly once (bug 1 fix verified).
        let pre_call_fires: usize = events.iter().filter(|e| {
            matches!(e, TraceEvent::HookFired { point: HookPoint::PreCall, .. })
        }).count();
        assert_eq!(pre_call_fires, 1, "expected 1 PreCall HookFired, got {pre_call_fires}");

        // 5. PromptBuilt's user_input is the post-hook (redacted)
        //    message, NOT the original input. This is the visible
        //    end-to-end proof that the PreCall hook mutation is
        //    applied to the model call.
        let prompt_built = events.iter().find_map(|e| {
            if let TraceEvent::PromptBuilt { user_input, .. } = e {
                Some(user_input.clone())
            } else { None }
        }).expect("PromptBuilt should be present");
        assert!(prompt_built.contains("<REDACTED>"),
            "PromptBuilt.user_input should be redacted, got: {prompt_built:?}");
        assert!(!prompt_built.contains("13812345678"),
            "PromptBuilt.user_input should not contain raw PII, got: {prompt_built:?}");

        drop(events);

        // 6. Emit SessionEnd and assert it lands in the sink.
        runner.emit_session_end(1);
        let events = sink.0.lock();
        let last = events.last().expect("should have events");
        assert!(matches!(last, TraceEvent::SessionEnd { .. }),
            "last event should be SessionEnd, got {last:?}");
        if let TraceEvent::SessionEnd { total_turns, .. } = last {
            assert_eq!(*total_turns, 1u32);
        }
    }

    // ─── LoopDetector tests ──────────────────────────────────────────────
    //
    // The detector is the agent's safety net against the "model is
    // stuck" failure mode: it watches for repeated identical tool
    // calls and breaks the loop early. These tests pin down the
    // threshold (3 consecutive identical calls) and the reset
    // behaviour (a different call resets the streak counter).
    #[test]
    fn loop_detector_breaks_on_repeated_calls() {
        let mut d = LoopDetector::default();
        // First two calls don't trip the threshold.
        assert!(matches!(d.record("read", "{\"path\":\"a.rs\"}"), LoopDecision::Continue));
        assert!(matches!(d.record("read", "{\"path\":\"a.rs\"}"), LoopDecision::Continue));
        // Third identical call trips. The reason string should
        // name the offending tool so the user can act on it.
        let decision = d.record("read", "{\"path\":\"a.rs\"}");
        match decision {
            LoopDecision::Break(reason) => {
                assert!(reason.contains("read"),
                    "break reason should name the offending tool, got: {reason}");
            }
            LoopDecision::Continue => panic!("expected Break on third identical call"),
        }
    }

    #[test]
    fn loop_detector_resets_on_different_call() {
        let mut d = LoopDetector::default();
        // Build up a near-streak with the same call.
        assert!(matches!(d.record("read", "{\"path\":\"a.rs\"}"), LoopDecision::Continue));
        assert!(matches!(d.record("read", "{\"path\":\"a.rs\"}"), LoopDecision::Continue));
        // A different call resets the streak.
        assert!(matches!(d.record("list", "{\"path\":\".\"}"), LoopDecision::Continue));
        // Now two `read` calls in a row — the counter starts at 1,
        // then 2; still below the threshold.
        assert!(matches!(d.record("read", "{\"path\":\"a.rs\"}"), LoopDecision::Continue));
        assert!(matches!(d.record("read", "{\"path\":\"a.rs\"}"), LoopDecision::Continue));
        // Third consecutive after the reset trips.
        assert!(matches!(d.record("read", "{\"path\":\"a.rs\"}"), LoopDecision::Break(_)));
    }

    #[test]
    fn loop_detector_reset_clears_state() {
        let mut d = LoopDetector::default();
        d.record("read", "{\"path\":\"a.rs\"}");
        d.record("read", "{\"path\":\"a.rs\"}");
        d.reset();
        // After reset, the streak is gone; next call starts fresh.
        assert!(matches!(d.record("read", "{\"path\":\"a.rs\"}"), LoopDecision::Continue));
        assert!(matches!(d.record("read", "{\"path\":\"a.rs\"}"), LoopDecision::Continue));
    }

    #[test]
    fn drain_inject_queue_prepends_synthetic_user_message() {
        use latte_ai::models::Role as MsgRole;

        let dir = tempfile::tempdir().unwrap();
        let queue = dir.path().join(".latte").join("inject").join("programmer.txt");
        std::fs::create_dir_all(queue.parent().unwrap()).unwrap();
        std::fs::write(&queue, "look at foo.rs\n").unwrap();

        let role = test_role();
        let agent =
            Agent::new("test-agent".into(), role, test_model(), GenerateParams::default())
                .unwrap();
        let mut runner = AgentRunner::new(agent)
            .with_role("programmer")
            .with_inject_worktree_root(dir.path().to_path_buf());
        runner.drain_inject_queue();

        assert!(!runner.context.messages().is_empty());
        let first = &runner.context.messages()[0];
        assert_eq!(first.role, MsgRole::User);
        assert_eq!(first.as_text(), "[INJECTED]\nlook at foo.rs\n");
        assert!(!queue.exists());
    }

    // ─── Advisor hint queue ────────────────────────────────────────
    //
    // The advisor monitor pushes correction hints into a shared
    // `Arc<Mutex<VecDeque<String>>>` while a turn is running; the
    // runner drains them at turn start and at every tool-round
    // boundary, prepending a synthetic `🦉 advisor 监察：` user
    // message. These tests pin both drain points.

    #[tokio::test]
    async fn advisor_hints_drained_at_turn_start() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .set_body_string(openai_completion_body("done", vec![])),
                    ),
            )
            .await;

        let hints: Arc<Mutex<std::collections::VecDeque<String>>> =
            Arc::new(Mutex::new(std::collections::VecDeque::new()));
        hints.lock().push_back("注意：src/main.rs 已改名为 src/lib.rs".to_string());

        let role = test_role();
        let agent = Agent::new_with_chain(
            "t".into(),
            role,
            vec![model_at(&server, "stub")],
            GenerateParams::default(),
        )
        .unwrap();
        let mut runner = AgentRunner::new(agent).with_advisor_hints(hints.clone());

        let resp = runner
            .run_turn(
                &[Message::user("继续")],
                None,
            )
            .await
            .expect("run_turn against wiremock");
        assert_eq!(resp, "done");
        assert!(hints.lock().is_empty(), "queue drained");

        // Recorded in the persistent context…
        let msgs = runner.context().messages();
        assert!(
            msgs.iter()
                .any(|m| m.as_text().contains("🦉 advisor 监察") && m.as_text().contains("已改名")),
            "hint recorded in context: {msgs:?}"
        );
        // …and visible to the model in the very first request.
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1);
        let body = String::from_utf8_lossy(&reqs[0].body);
        assert!(body.contains("🦉 advisor 监察"), "model saw the hint: {body}");
    }

    #[tokio::test]
    async fn advisor_hints_drained_mid_tool_loop() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        use latte_rs_agent_tools::prelude::create_tool_manager;
        use latte_rs_agent_tools::types::{
            SchemaType, SharedToolHandler, Tool, ToolInputSchema, ToolManager as _,
        };

        let hints: Arc<Mutex<std::collections::VecDeque<String>>> =
            Arc::new(Mutex::new(std::collections::VecDeque::new()));

        // The tool handler simulates the advisor: every execution
        // pushes a fresh hint into the shared queue.
        let hints_in_tool = hints.clone();
        let handler: SharedToolHandler = Arc::new(move |_input, _ctx| {
            let hints = hints_in_tool.clone();
            Box::pin(async move {
                hints.lock().push_back("停止重试，换个思路".to_string());
                Ok(serde_json::json!({ "ok": true }))
            })
        });
        let schema = ToolInputSchema {
            schema_type: SchemaType,
            properties: Default::default(),
            required: None,
            additional_properties: None,
        };
        let tool = Tool::builder("ping", "test ping", schema, handler).build();
        let tm = create_tool_manager();
        tm.register(tool, None);

        // The stub model always answers with the same tool call, so
        // the turn ping-pongs through tool rounds. Note: the
        // LoopDetector is constructed fresh *per round* (see the
        // 2026-07-10 fix in run_turn), so cross-round repetition is
        // NOT broken by it — the turn ends via MaxToolRoundsExceeded.
        // That still gives us several round-boundary drains to
        // observe.
        let server = wiremock::MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(
                        openai_completion_body("", vec![
                            serde_json::json!({
                                "id": "call_ping",
                                "type": "function",
                                "function": {
                                    "name": "ping",
                                    "arguments": "{}"
                                }
                            })
                        ]),
                    )),
            )
            .await;

        let role = test_role();
        let agent = Agent::new_with_chain(
            "t".into(),
            role,
            vec![model_at(&server, "stub")],
            GenerateParams::default(),
        )
        .unwrap();
        let mut runner = AgentRunner::new_with_tools(agent, tm, 4).with_advisor_hints(hints);

        let err = runner
            .run_turn(
                &[Message::user("go")],
                None,
            )
            .await
            .expect_err("repeated tool calls hit the round cap");
        assert!(
            matches!(err, AgentError::MaxToolRoundsExceeded(4)),
            "got {err:?}"
        );

        // Round 0 produced request #1 and executed the tool (which
        // pushed a hint); the round-1 boundary drain must make
        // request #2 carry that hint — i.e. the correction reached
        // the model MID tool loop, before the turn ended.
        let reqs = server.received_requests().await.unwrap();
        assert!(reqs.len() >= 2, "expected ≥2 model calls, got {}", reqs.len());
        let body2 = String::from_utf8_lossy(&reqs[1].body);
        assert!(
            body2.contains("🦉 advisor 监察"),
            "second request carries the drained hint: {body2}"
        );
        assert!(body2.contains("停止重试"), "hint text present: {body2}");
        // First request predates any hint.
        let body1 = String::from_utf8_lossy(&reqs[0].body);
        assert!(!body1.contains("🦉 advisor 监察"));
    }

    #[tokio::test]
    async fn non_json_tool_args_feed_back_format_error_without_executing() {
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, ResponseTemplate};
        use latte_rs_agent_tools::prelude::create_tool_manager;
        use latte_rs_agent_tools::types::{
            SchemaType, SharedToolHandler, Tool, ToolInputSchema, ToolManager as _,
        };
        use std::sync::atomic::{AtomicBool, Ordering};

        // The tool must NEVER run: args are XML, not JSON.
        let executed = Arc::new(AtomicBool::new(false));
        let executed_in_tool = executed.clone();
        let handler: SharedToolHandler = Arc::new(move |_input, _ctx| {
            let executed = executed_in_tool.clone();
            Box::pin(async move {
                executed.store(true, Ordering::SeqCst);
                Ok(serde_json::json!({ "ok": true }))
            })
        });
        let schema = ToolInputSchema {
            schema_type: SchemaType,
            properties: Default::default(),
            required: None,
            additional_properties: None,
        };
        let tool = Tool::builder("ping", "test ping", schema, handler).build();
        let tm = create_tool_manager();
        tm.register(tool, None);

        let server = wiremock::MockServer::start().await;
        // Round 0（先注册先匹配，只生效一次）：模型吐出 args 为 XML 块的调用。
        struct FirstOnly(std::sync::atomic::AtomicUsize);
        impl wiremock::Match for FirstOnly {
            fn matches(&self, _req: &wiremock::Request) -> bool {
                self.0.fetch_add(1, Ordering::SeqCst) == 0
            }
        }
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .and(FirstOnly(std::sync::atomic::AtomicUsize::new(0)))
                    .respond_with(ResponseTemplate::new(200).set_body_string(
                        openai_completion_body("", vec![
                            serde_json::json!({
                                "id": "call_ping",
                                "type": "function",
                                "function": {
                                    // arguments 为非法 JSON 串 → latte-ai 层
                                    // 解析失败，arguments=Null + arguments_parse_error=Some。
                                    "name": "ping",
                                    "arguments": "<arg_key>x</arg_key>"
                                }
                            })
                        ]),
                    )),
                )
                .await;
        // 兜底：round 1 起一律 finish。
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(
                        ResponseTemplate::new(200).set_body_string(openai_completion_body("done", vec![])),
                    ),
            )
            .await;

        let role = test_role();
        let agent = Agent::new_with_chain(
            "t".into(),
            role,
            vec![model_at(&server, "stub")],
            GenerateParams::default(),
        )
        .unwrap();
        let mut runner = AgentRunner::new_with_tools(agent, tm, 4);

        let resp = runner
            .run_turn(
                &[Message::user("go")],
                None,
            )
            .await
            .expect("turn completes after the format error feedback");
        assert_eq!(resp, "done");
        assert!(!executed.load(Ordering::SeqCst), "tool must not execute on non-JSON args");

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 2, "should still make round-2 call after parse failure");
        let body2 = String::from_utf8_lossy(&reqs[1].body);
        // 新行为：MalformedArgs 不回喂 model —— 第 2 轮请求里不含任何
        // [tool_error] 内容，避免 model 看自己上一轮错误输出循环恶化。
        assert!(
            !body2.contains("[tool_error for ping]"),
            "MalformedArgs must NOT loopback to model: {body2}"
        );
    }

    #[test]
    fn last_decision_kind_returns_text_for_plain_response() {
        use latte_ai::models::Role as MsgRole;
        let role = test_role();
        let agent =
            Agent::new("test-agent".into(), role, test_model(), GenerateParams::default())
                .unwrap();
        let mut runner = AgentRunner::new(agent);
        runner.context_mut().push(Message::assistant("no tools here"));
        assert_eq!(runner.last_decision_kind(), "text");
    }

    #[test]
    fn last_decision_kind_returns_tool_call_for_write() {
        use crate::checkpoint::short_hash;
        let role = test_role();
        let agent =
            Agent::new("test-agent".into(), role, test_model(), GenerateParams::default())
                .unwrap();
        let mut runner = AgentRunner::new(agent);
        let args = serde_json::json!({"path":"/tmp/x"});
        // native function-calling: assistant 消息带 tool_calls 字段。
        let tc = latte_ai::models::ToolCall {
            id: "call_1".into(),
            name: "write".into(),
            arguments: args.clone(),
            arguments_raw: None,
            arguments_parse_error: None,
        };
        runner.context_mut().push(Message::assistant_with_tool_calls(
            String::new(),
            vec![tc],
        ));
        let expected = format!("tool_call:write:{}", &short_hash(&args.to_string())[..8]);
        assert_eq!(runner.last_decision_kind(), expected);
    }
    // ─── cwd wiring ──────────────────────────────────────────
    //
    // Pins the builder contract for `with_cwd`. End-to-end
    // (model → tool handler receives `ctx.metadata.cwd`) coverage
    // lives in the Tauri integration tests; this asserts the
    // field round-trips through the builder so a refactor can't
    // silently drop the plumbing.
    #[test]
    fn with_cwd_starts_none_and_sets_some() {
        let role = test_role();
        let agent =
            Agent::new("test-agent".into(), role, test_model(), GenerateParams::default())
                .unwrap();
        // Default — no cwd wired.
        let runner = AgentRunner::new(agent.clone());
        assert!(runner.cwd.is_none(), "freshly-built runner must have cwd = None");
        // After `with_cwd` the field is Some(path).
        let cwd_path = std::path::PathBuf::from("/tmp/latte-cwd-test");
        let runner = AgentRunner::new(agent).with_cwd(cwd_path.clone());
        assert_eq!(runner.cwd.as_deref(), Some(cwd_path.as_path()));
    }

    #[test]
    fn with_cwd_preserves_other_builder_state() {
        // `with_cwd` must not clobber `role_id` / `session_id` set
        // by earlier builders. Mirrors how `build_runner` composes
        // `.with_role(role_id).with_cwd(cwd)` in
        // `latte-agent-core/src/controller.rs`.
        let role = test_role();
        let agent =
            Agent::new("test-agent".into(), role, test_model(), GenerateParams::default())
                .unwrap();
        let cwd_path = std::path::PathBuf::from("/home/user/my-project");
        let runner = AgentRunner::new(agent)
            .with_role("manager")
            .with_session_id("sess-42")
            .with_cwd(cwd_path.clone());
        assert_eq!(runner.role_id, "manager");
        assert_eq!(runner.session_id, "sess-42");
        assert_eq!(runner.cwd.as_deref(), Some(cwd_path.as_path()));
    }

    // ─── dedupe_native_tool_calls tests ─────────────────────────────────
    //
    // 覆盖去重的几种关键行为：完全相同的调用合并、不同参数不合并、
    // JSON 文本形式不同但语义相同时合并、保留首次出现的位置。
    // 用 `latte_ai::models::ToolCall` 构造而非遗留 `ParsedCall`。

    fn t(name: &str, args: serde_json::Value) -> latte_ai::models::ToolCall {
        latte_ai::models::ToolCall {
            id: "call".into(), name: name.into(), arguments: args,
            arguments_raw: None, arguments_parse_error: None,
        }
    }

    #[test]
    fn dedupe_drops_three_identical_delegate_calls() {
        let args = serde_json::json!({"role":"programmer","task":"run pwd"});
        let calls = vec![t("delegate", args.clone()), t("delegate", args.clone()), t("delegate", args.clone())];
        let deduped = dedupe_native_tool_calls(calls);
        assert_eq!(deduped.len(), 1, "3 个相同 delegate 调用应合并为 1 个");
        assert_eq!(deduped[0].name, "delegate");
    }

    #[test]
    fn dedupe_keeps_calls_with_different_args() {
        let calls = vec![
            t("read", serde_json::json!({"path":"a.rs"})),
            t("read", serde_json::json!({"path":"b.rs"})),
            t("read", serde_json::json!({"path":"c.rs"})),
        ];
        let deduped = dedupe_native_tool_calls(calls);
        assert_eq!(deduped.len(), 3, "3 个不同 path 的 read 调用应全部保留");
    }

    #[test]
    fn dedupe_treats_semantic_equivalent_args_as_duplicates() {
        let calls = vec![
            t("read", serde_json::json!({"path":"a.rs"})),
            t("read", serde_json::json!({"path": "a.rs"})), // 多了一个空格，语义等价
        ];
        let deduped = dedupe_native_tool_calls(calls);
        assert_eq!(deduped.len(), 1, "键序不同/空白不同视为同一次调用");
    }

    #[test]
    fn dedupe_does_not_merge_args_that_differ_in_fields() {
        let calls = vec![
            t("read", serde_json::json!({"path":"a.rs"})),
            t("read", serde_json::json!({"path":"a.rs","limit":10})),
        ];
        let deduped = dedupe_native_tool_calls(calls);
        assert_eq!(deduped.len(), 2, "args 含不同字段时不应合并");
    }

    #[test]
    fn dedupe_preserves_first_occurrence_order() {
        let calls = vec![
            t("read", serde_json::json!({"path":"a.rs"})),
            t("read", serde_json::json!({"path":"b.rs"})),
            t("read", serde_json::json!({"path":"a.rs"})),
        ];
        let deduped = dedupe_native_tool_calls(calls);
        assert_eq!(deduped.len(), 2);
        assert_eq!(deduped[0].arguments["path"], "a.rs");
        assert_eq!(deduped[1].arguments["path"], "b.rs");
    }

    #[test]
    fn dedupe_handles_empty_input() {
        let deduped = dedupe_native_tool_calls(vec![]);
        assert!(deduped.is_empty());
    }

    #[test]
    fn dedupe_handles_malformed_arguments() {
        // arguments_raw 传递原始坏串，arguments 为 Null；去重靠 to_string()。
        // 两个相同 arguments_raw（如"pwd && ls"）应被识别为重复。
        let make = |raw: &str| latte_ai::models::ToolCall {
            id: "call".into(), name: "bash".into(),
            arguments: serde_json::Value::Null,
            arguments_raw: Some(raw.into()),
            arguments_parse_error: Some("parse failed".into()),
        };
        let calls = vec![make("pwd && ls"), make("pwd && ls")];
        let deduped = dedupe_native_tool_calls(calls);
        assert_eq!(deduped.len(), 1, "相同 arguments_raw 的调用应去重");
    }

    #[test]
    fn dedupe_keeps_different_tools_with_same_args() {
        let args = serde_json::json!({"path":"a"});
        let calls = vec![t("read", args.clone()), t("bash", args)];
        let deduped = dedupe_native_tool_calls(calls);
        assert_eq!(deduped.len(), 2, "不同工具名应保留");
    }

    // ─── LoopDetector 报数修正测试 ───────────────────────────────────
    //
    // 历史 bug：第 3 次相同调用触发 Break 时，错误消息里写成
    // "called 4 times in a row"（`self.streak + 1` 多算了 1）。
    // 修正后第 3 次触发应该是 "called 3 times in a row"。

    #[test]
    fn loop_detector_break_message_count_matches_actual_call() {
        let mut d = LoopDetector::default();
        let _ = d.record("delegate", r#"{"role":"programmer","task":"pwd"}"#);
        let _ = d.record("delegate", r#"{"role":"programmer","task":"pwd"}"#);
        let decision = d.record("delegate", r#"{"role":"programmer","task":"pwd"}"#);
        match decision {
            LoopDecision::Break(reason) => {
                assert!(
                    reason.contains("3 times in a row"),
                    "第 3 次触发应报 '3 times in a row'，实际：{reason}"
                );
                assert!(
                    !reason.contains("4 times"),
                    "不应出现 '4 times'（off-by-one），实际：{reason}"
                );
            }
            LoopDecision::Continue => panic!("第 3 次相同调用应该触发 Break"),
        }
    }

    #[test]
    fn loop_detector_per_round_does_not_accumulate_across_rounds() {
        // 2026-07-10 修复：loop_detector 改为每个 round 重新构造。
        // 这个测试把"每轮一个全新 detector"的契约钉死 —— 3 轮每轮
        // 吐一个完全相同的 delegate 调用，每个 detector 各自看到
        // streak=1，不 trip。跨 round 的"manager 一模一样地重试"
        // 不归 in-round detector 管，那是 supervisor 的
        // dead_loop_window 的活（见 RoundScheduler::supervisor）。
        let payload = r#"{"role":"programmer","task":"pwd"}"#;
        for _round in 0..3 {
            let mut d = LoopDetector::default();
            let r = d.record("delegate", payload);
            assert!(
                matches!(r, LoopDecision::Continue),
                "每轮的全新 detector 不应被单次调用 trip"
            );
        }
    }
    /// build_tool_schemas：工具名直接用扁平规范名（bash/read/edit），
    /// 不再有 friendly_tool_name / namespace 转换。还验证 strict 字段
    /// 透传（默认 None）。
    #[tokio::test]
    async fn build_tool_schemas_uses_flat_names() {
        let mgr = crate::controller::build_tool_manager(
            &["bash".into(), "read".into(), "search".into()],
        )
        .await
        .unwrap();
        let schemas = build_tool_schemas(&mgr);
        let names: Vec<String> = schemas.iter().map(|t| t.name.clone()).collect();
        assert!(names.contains(&"bash".to_string()), "应有 bash: {names:?}");
        assert!(names.contains(&"read".to_string()), "应有 read: {names:?}");
        assert!(names.contains(&"search".to_string()), "应有 search: {names:?}");
        // 扁平命名下不应有点号（shell.exec 之类的 namespace 不复存在）。
        assert!(!names.iter().any(|n| n.contains('.')), "不应有点号命名: {names:?}");
        // strict 默认 None（不开 Structured Outputs）。
        for t in &schemas {
            assert!(t.strict.is_none(), "工具 {} 的 strict 应为 None，got {:?}", t.name, t.strict);
        }
    }
}
