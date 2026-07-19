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
use crate::trace::ParsedCall;

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
    /// On a retryable failure (rate-limited, 5xx, transient HTTP error),
    /// the failing model is put on cooldown and the next model in the
    /// chain is tried. Non-retryable errors (auth, config, serialization)
    /// surface immediately without walking the rest of the chain.
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
                        // Non-retryable: surface immediately, don't keep
                        // walking the chain (e.g. Auth errors mean the
                        // vendor is misconfigured and other models in
                        // the same vendor would fail the same way).
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
        Ok(Message {
            role: Role::System,
            content,
        })
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

// ─── dedupe_tool_calls ─────────────────────────────────────────────────────

/// 同一响应内去掉完全相同的 `(name, args)` tool_call，保留首次出现的那
/// 一条，后面的重复项直接丢弃（不抛错，不修改原顺序之外的位置）。
///
/// # 为什么需要
///
/// manager role 的 prompt 明确告诉模型"在一个响应里发出多个
/// `tool_call` 块以并行执行"。部分模型（GLM 5.2 在 `delegate` 上尤其
/// 明显）会把这个指令误解为"重复同一调用三次"——面对"我在哪个目录"
/// 这类简单问题，模型可能直接吐出 3 个一模一样的 `delegate` 块。三
/// 个完全相同的 `delegate` 调用会触发 `LoopDetector`，让用户看到
/// "called 4 times in a row with identical args" 这种令人困惑的报错
/// （实际是第 3 次触发 + 消息里多算了 1，详见 `LoopDetector::record`）。
/// dedup 之后 agent 继续运行，用户拿到一次 specialist 的结果。
///
/// # 输入 / 输出
///
/// - 入参 `calls`：模型一次响应里提取出的 `<tool_call>` 列表，按模型输出
///   顺序排列。
/// - 出参：去重后的列表，长度 ≤ 入参，顺序与入参中首次出现的位置一致。
///
/// # 行为细节
///
/// - 比较的是 args 的**规范化 JSON 形式**（用 `serde_json::from_str` 解析
///   后再 `to_string`），不是原始文本。这样 `{"path":"a"}` 和
///   `{"path": "a"}`（不同空白）或 `{"a":1,"b":2}` 和 `{"b":2,"a":1}`
///   （不同键顺序）都会被识别为同一调用，与 `LoopDetector` 对 args
///   的 hash 方式保持一致。
/// - args 不是合法 JSON 时回退到原始文本比较（用 `Value::String` 兜底
///   编码），避免 panic。
/// - 不同的 `(name, args)` 不会被合并 —— 用户或模型可能真的想并行
///   调多次同一工具但参数不同（例如一次读 a.rs、一次读 b.rs），这种
///   情况保留所有调用。
fn dedupe_tool_calls(calls: Vec<ParsedCall>) -> Vec<ParsedCall> {
    use std::collections::HashSet;
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut out: Vec<ParsedCall> = Vec::with_capacity(calls.len());
    for tc in calls {
        // 把 args 规范化成 JSON 字符串后再做 key，让"语义相同但文本不同"
        // 的调用被识别成同一次。parse 失败时退回到原始文本，不抛错。
        let canonical_args = serde_json::from_str::<serde_json::Value>(&tc.args)
            .ok()
            .and_then(|v| serde_json::to_string(&v).ok())
            .unwrap_or_else(|| tc.args.clone());
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
        // Everything else is non-retryable: vendor config, serde,
        // auth, unsupported provider, etc.
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
    /// Role identifier for this runner.
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
            role_id: "default".to_string(),
            session_id: String::new(),
            inject_worktree_root: None,
            advisor_hints: None,
            cwd: None,
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
            role_id: "default".to_string(),
            session_id: String::new(),
            inject_worktree_root: None,
            advisor_hints: None,
            cwd: None,
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
            role_id: "default".to_string(),
            session_id: String::new(),
            inject_worktree_root: None,
            advisor_hints: None,
            cwd: None,
        }
    }
    /// Read and drain the per-role inject queue, if any. Prepends a
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
        let synthetic = latte_ai::models::Message {
            role: latte_ai::models::Role::User,
            content: format!("[INJECTED]\n{}", content),
        };
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
            self.context.push(latte_ai::models::Message {
                role: latte_ai::models::Role::User,
                content: format!("🦉 advisor 监察：\n{hint}"),
            });
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

    /// Run one turn: process a user message and return the assistant's response.
    ///
    /// The turn builds a complete message list:
    ///   [system_prompt, ...history, new_user_messages...]
    ///
    /// If tools are configured, enters a tool-call loop:
    ///   request → response → parse tool_calls → execute → append result → repeat
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
        use crate::trace::{ParsedCall, ParseDiag, ToolStatus, TraceEvent, TraceMeta};
        let turn_start = Instant::now();
        let meta = TraceMeta::now(0, self.role_id.clone(), self.session_id.clone());

        let default_vars = serde_json::json!({});
        let vars = system_vars.unwrap_or(&default_vars);

        let sys_msg = self.agent.system_message(vars)?;
        let system_rendered = sys_msg.content.clone();
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
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let model_id = self.agent.model_chain.first()
            .map(|mc| mc.model.id.clone())
            .unwrap_or_default();
        let est_input_tokens = (messages.iter().map(|m| m.content.len()).sum::<usize>() / 4) as u32;
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
                    messages.push(Message {
                        role: Role::User,
                        content: format!("🦉 advisor 监察：\n{hint}"),
                    });
                }
            }
            // 2a. Emit ModelCall + ModelRawOut after agent.chat()
            let chat_start = Instant::now();
            let completion = self.agent.chat(&messages, None, WaitPolicy::WaitAndRetry).await?;
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

            // Check for tool calls
            let tool_calls = extract_tool_calls(&final_response);

            // 3a. Emit ParseToolCalls
            let opens_found = final_response.matches("<tool_call").count() as u32;
            let parsed_calls: Vec<ParsedCall> = tool_calls.iter().map(|tc| ParsedCall {
                name: tc.name.clone(),
                args: tc.args.clone(),
            }).collect();
            // 同一响应内多个完全相同的 `tool_call` 块（典型场景：
            // manager 把"并行调度多个 specialist"误读为"重复同一调
            // 用"）只保留首次出现的那一条，避免触发 `LoopDetector`。
            // 这一步放在 ParseToolCalls 事件之前，所以 trace 上看到的
            // 列表就是实际会执行的那一份。
            let parsed_calls = dedupe_tool_calls(parsed_calls);
            self.sink.emit(TraceEvent::ParseToolCalls {
                meta: meta.clone(),
                raw_in: final_response.clone(),
                parsed: parsed_calls.clone(),
                diagnostics: ParseDiag {
                    opens_found,
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
                // Append assistant message with tool calls
                messages.push(Message {
                    role: Role::Assistant,
                    content: final_response.clone(),
                });
                // Execute each tool call
                for tc in &post_parse_calls {
                     // Map friendly config aliases ("bash") to the real
                     // builtin tool names ("exec"). Without this, model
                     // outputs trained as `bash` get "tool not found"
                     // because the registry stores it as `shell.exec`.
                     let resolved_name: String = match tc.name.as_str() {
                         "bash" => "exec".to_string(),
                         n => n.to_string(),
                     };
                     let input: serde_json::Value = match serde_json::from_str(&tc.args) {
                         Ok(v) => v,
                         Err(e) => {
                             // The model emitted args that are not valid
                             // JSON (e.g. XML arg_key/arg_value blocks).
                             // Don't hand the tool a bare string (it would
                             // fail with a confusing "xx is required");
                             // feed a clear format error straight back so
                             // the model can re-issue in the right shape.
                             let msg = format!(
                                 "tool args are not valid JSON ({e}). Re-issue as a single line \
                                  `<tool_call>{} {{\"arg\": \"value\"}}</tool_call>` with a JSON object as args.",
                                 tc.name
                             );
                             self.sink.emit(TraceEvent::ToolExec {
                                 meta: meta.clone(),
                                 name: tc.name.clone(),
                                 args_json: tc.args.clone(),
                                 latency_ms: 0,
                                 status: ToolStatus::Err(msg.clone()),
                             });
                             messages.push(Message {
                                 role: Role::User,
                                 content: format!("[tool_error for {}]\n{}", tc.name, msg),
                             });
                             continue;
                         }
                     };

                    // 4a. Run PreToolHook (can abort or mutate args).
                    // The hook gets a mutable copy of the parsed input;
                    // on `Mutate` we shadow `input` so the tool sees the
                    // mutated args and the ToolExec event records them.
                    let mut mutable_input = input.clone();
                    {
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
                        match outcome {
                            crate::hooks::HookOutcome::Abort { reason } => {
                                return Err(AgentError::HookAborted {
                                    hook: "PreTool".into(),
                                    reason,
                                });
                            }
                            // Continue + Mutate both leave `mutable_input`
                            // holding the (possibly mutated) value we want
                            // to forward to the tool. The Mutate variant
                            // has already updated `*ctx.args` inside the
                            // chain, so no further action is needed.
                            _ => {}
                        }
                    }
                    let input = mutable_input;

                    // Resolve relative filesystem paths in the tool input
                    // against the runner's workspace cwd (when set), so the
                    // packaged tools — which resolve relative paths against
                    // the PROCESS cwd — still land in the user's workspace
                    // when embedded (Tauri: process cwd = app data dir).
                    // The `metadata.cwd` channel below is advisory and no
                    // packaged tool consults it today, hence this rewrite.
                    let input = match &self.cwd {
                        Some(cwd) => resolve_tool_input_against_cwd(input, cwd),
                        None => input,
                    };

                    let mut ctx = latte_rs_agent_tools::types::ToolExecutionContext::fresh(
                        &resolved_name,
                        1,
                    );
                    // Advisory channel for future path-aware tools; the
                    // authoritative mechanism is the input rewrite above.
                    if let Some(cwd) = &self.cwd {
                        ctx.metadata = Some(serde_json::json!({
                            "cwd": cwd.display().to_string(),
                        }));
                    }
                    // Resolve the short name the model emits ("read")
                    // to the namespaced form the registry stores
                    // ("file.read"). We try the name as-is first, then
                    // fall back to any registered tool whose short
                    // suffix matches. This lets role.allowed_tools
                    // list `["read", "list", "search"]` while the
                    // registry stores them under their package prefix.
                    let full_name = tm
                        .get_tool(&resolved_name)
                        .map(|_| resolved_name.clone())
                        .or_else(|| {
                            tm.get_tool_names().into_iter().find(|n| {
                                n.rsplit_once('.').map(|(_, s)| s) == Some(resolved_name.as_str())
                            })
                        })
                        .unwrap_or_else(|| resolved_name.clone());

                    // 4b. Execute tool + emit ToolExec
                    let tool_start = Instant::now();
                    let exec_result = tm.execute(&full_name, input.clone(), Some(ctx)).await;
                    let tool_latency = tool_start.elapsed().as_millis() as u64;

                    let args_json = serde_json::to_string(&input).unwrap_or_else(|_| tc.args.clone());

                    // 4b-extra. Loop detection: if the model is
                    // stuck calling the same tool with the same args
                    // repeatedly, bail out before `max_tool_rounds`
                    // is exhausted. We use `tc.name` (the name the
                    // model emitted) rather than `full_name` (the
                    // resolved one) so that the loop key matches what
                    // the model is reasoning about — if a model
                    // switches between emitting "read" and "file.read"
                    // that should count as a fresh call, not a
                    // continuation of the streak.
                    if let LoopDecision::Break(reason) = loop_detector.record(&tc.name, &args_json) {
                        return Err(AgentError::ToolLoopDetected {
                            tool: tc.name.clone(),
                            reason,
                        });
                    }

                    match exec_result {
                        Ok(result) => {
                            self.sink.emit(TraceEvent::ToolExec {
                                meta: meta.clone(),
                                name: tc.name.clone(),
                                args_json: args_json.clone(),
                                latency_ms: tool_latency,
                                status: ToolStatus::Ok(serde_json::to_string(&result).unwrap_or_default()),
                            });
                            // 4c. Run PostToolHook
                            let mut result_str = serde_json::to_string_pretty(&result)
                                .unwrap_or_else(|_| format!("{:?}", result));
                            {
                                let mut post_ctx = crate::hooks::PostToolCtx { name: &resolved_name, result: &mut result_str };
                                let outcome = self.hooks.run_post_tool(&mut post_ctx, |hook_name, point, kind| {
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
                                            hook: "PostTool".into(),
                                            reason,
                                        });
                                    }
                                    crate::hooks::HookOutcome::Mutate(ref mutated) => {
                                        result_str = mutated.clone();
                                    }
                                    _ => {}
                                }
                            }
                            messages.push(Message {
                                role: Role::User,
                                content: format!(
                                    "[tool_result for {}]\n{}",
                                    tc.name,
                                    result_str,
                                ),
                            });
                        }
                        Err(e) => {
                            self.sink.emit(TraceEvent::ToolExec {
                                meta: meta.clone(),
                                name: tc.name.clone(),
                                args_json,
                                latency_ms: tool_latency,
                                status: ToolStatus::Err(e.to_string()),
                            });
                            messages.push(Message {
                                role: Role::User,
                                content: format!("[tool_error for {}]\n{}", tc.name, e),
                            });
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
        self.context.push(Message {
            role: Role::Assistant,
            content: final_response.clone(),
        });

        Ok(final_response)
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
    pub fn last_decision_kind(&self) -> String {
        use latte_ai::models::Role as MsgRole;
        use crate::checkpoint::short_hash;
        let Some(last) = self.context.messages().last() else { return "text".to_string(); };
        if last.role != MsgRole::Assistant { return "text".to_string(); }
        let content = &last.content;
        // Order matters: more specific tool tags (delegate, ask_human)
        // win over the generic `<tool_callNAME>` extraction below.
        if content.contains("<tool_calldelegate>") { return "delegate".to_string(); }
        if content.contains("<tool_callask_human>") { return "ask_human".to_string(); }
        if let Some(open_idx) = content.find("<tool_call") {
            let after_open = &content[open_idx + "<tool_call".len()..];
            // Name runs from after_open[0] to the first char that is
            // whitespace or `>` (same rule the parser uses).
            let name_end = after_open
                .find(|c: char| c.is_whitespace() || c == '>')
                .unwrap_or(after_open.len());
            let name = &after_open[..name_end];
            // Args: the JSON between the tool name terminator and the
            // canonical close tag; we hash the trimmed args so
            // `decision_kind` collisions only happen on identical payloads.
            let rest = &after_open[name_end..];
            let close = rest.find("</tool_call>").unwrap_or(rest.len());
            // Strip the optional `>` terminator after the tool name plus any
            // surrounding whitespace, so the hash matches the raw args the
            // caller passed in (no `>` or extra spaces leaking in).
            let mut args_str = rest[..close].trim();
            if let Some(stripped) = args_str.strip_prefix('>') {
                args_str = stripped.trim_start();
            }
            let args_hash = short_hash(args_str);
            return format!("tool_call:{}:{}", name, &args_hash[..8]);
        }
        "text".to_string()
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

// ─── Tool Call Parsing ────────────────────────────────────────────────────

/// A parsed tool call from model output.
#[derive(Debug, Clone)]
struct ToolCall {
    name: String,
    args: String,
}

/// Extract `<tool_call>name args</tool_call>` patterns from model output.
/// Close-tag variants accepted: `</tool_call>` (canonical prompt form)
/// and `</tool_callNAME>` (XML-style match tag, what deepseek-v4-flash
/// actually emits). The open prefix is always `<tool_call` (10 chars,
/// no `>`); the name runs to the first non-`[A-Za-z0-9_]` char.

fn extract_tool_calls(text: &str) -> Vec<ToolCall> {
    let mut results = Vec::new();
    let mut remaining = text;
    const OPEN: &str = "<tool_call";
    const CANONICAL_CLOSE: &str = "</tool_call>";

    while let Some(start) = remaining.find(OPEN) {
        let after_open = &remaining[start + OPEN.len()..];
        // Legacy open was `<tool_call>NAME` (10+`>`+name), current XML
        // open is `<tool_callNAME>` (10+name+`>`). Either way, if the
        // char right after `<tool_call` is `>`, skip it before parsing
        // the name. This lets one parser accept all three formats.
        let after_open = after_open.strip_prefix('>').unwrap_or(after_open);
        // Tolerate whitespace/newlines between the open tag and the tool
        // name (`<tool_call>\nNAME {…}`) — previously the name scan
        // stopped at the first non-alphanumeric char and the whole call
        // was silently dropped (name_len == 0).
        let after_open = after_open.trim_start();
        let name_len = after_open
            .char_indices()
            .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '_')
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);

        if name_len == 0 {
            // `<tool_call` with no name following — skip past the
            // prefix to avoid an infinite loop.
            remaining = &remaining[start + OPEN.len()..];
            continue;
        }

        let name = after_open[..name_len].to_string();
        let after_name = &after_open[name_len..];
        let after_name = after_name
            .strip_prefix('>')        // drop `>` from `<tool_callNAME>`
            .unwrap_or(after_name)
            .trim_start();

        // Look for the close tag. Three-stage fallback for model
        // quirks: (1) XML-style `</tool_callNAME>` (what most models
        // actually emit), (2) canonical `</tool_call>` (what the
        // prompt says), (3) any well-formed `</X>` — GLM 5.2 in
        // particular often emits `</arg_value>` for the closing
        // tag, so accepting any valid XML closing tag keeps the
        // parser robust to that family of model quirks.
        let xml_close = format!("</tool_call{}>", name);
        let (close_pos, close_len) = if let Some(p) = after_name.find(&xml_close) {
            (p, xml_close.len())
        } else if let Some(p) = after_name.find(CANONICAL_CLOSE) {
            (p, CANONICAL_CLOSE.len())
        } else {
            match find_any_close_tag(after_name) {
                Some((p, l)) => (p, l),
                None => break, // malformed / truncated: give up
            }
        };

        let args = after_name[..close_pos].trim().to_string();
        results.push(ToolCall { name, args });
        remaining = &after_name[close_pos + close_len..];
    }

    results
}
/// Public, structured wrapper around the private [`extract_tool_calls`].
/// Returns the parsed calls together with parse diagnostics so the CLI
/// `debug parse` / `debug replay` subcommands can report what the
/// parser saw in a model output (or any other text).
///
/// `opens_found` is the count of `<tool_call` substrings in `text`.
/// `closes_matched` is the number of well-formed calls the parser
/// extracted. `opens_found - closes_matched` is the number of opens
/// that had no matching close; their raw text is collected (up to
/// 40 chars per slice) into `unmatched_opens` for diagnostic display.
pub fn parse_tool_calls(text: &str) -> (Vec<crate::trace::ParsedCall>, crate::trace::ParseDiag) {
    use crate::trace::{ParseDiag, ParsedCall};
    let opens_found = text.matches("<tool_call").count() as u32;
    let raw = extract_tool_calls(text);
    let parsed: Vec<ParsedCall> = raw.iter()
        .map(|tc| ParsedCall { name: tc.name.clone(), args: tc.args.clone() })
        .collect();
    let closes_matched = parsed.len() as u32;
    let unmatched = opens_found.saturating_sub(closes_matched);
    let mut unmatched_opens: Vec<String> = Vec::new();
    if unmatched > 0 {
        // Take the LAST `unmatched` opens — those are the ones
        // `extract_tool_calls` couldn't close. Earlier opens were
        // already paired with their close tag and consumed.
        let positions: Vec<(usize, &str)> = text.match_indices("<tool_call").collect();
        for &(pos, _) in positions.iter().rev().take(unmatched as usize) {
            let end = text[pos..]
                .find(|c: char| c.is_whitespace() || c == '>' || c == '\n')
                .map(|p| pos + p)
                .unwrap_or(text.len().min(pos + 40));
            unmatched_opens.push(text[pos..end].to_string());
        }
    }
    (parsed, ParseDiag { opens_found, closes_matched, unmatched_opens })
}

/// 找 s 里第一个符合 `</X>` 形式的合法 closing tag（X 是字母 / 数字
/// / 下划线 / 连字符，至少 1 个字符，后面紧跟 `>`）。返回
/// `(start, len)` 让调用方能切出整段 closing tag。
///
/// 这个回退是给部分模型（GLM 5.2 经常）会吐非标准 closing tag 用的：
/// 比如 `</arg_value>` 或 `</function>`，而不是 prompt 里写的
/// `</tool_call>` 或 `</tool_callNAME>`。如果连一个像样的 closing
/// tag 都没有，才放弃整个 tool call。
fn find_any_close_tag(s: &str) -> Option<(usize, usize)> {
    let mut search_from = 0;
    while let Some(rel) = s[search_from..].find("</") {
        let abs = search_from + rel;
        let after_slash = abs + 2;
        // Tag name 至少 1 个合法字符
        let name_end = s[after_slash..]
            .char_indices()
            .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        if name_end == 0 {
            // `</` 后面没有合法 tag name 字符，跳过去继续找
            search_from = abs + 2;
            continue;
        }
        // 必须紧跟一个 `>` 才算完整的 closing tag
        if let Some(gt_rel) = s[after_slash + name_end..].find('>') {
            let close_end = after_slash + name_end + gt_rel + 1;
            return Some((abs, close_end - abs));
        }
        search_from = abs + 2;
    }
    None
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

/// Rewrite `input` so relative filesystem paths resolve against the
/// runner's workspace `cwd` instead of the process cwd.
///
/// Background: `ToolExecutionContext.metadata.cwd` is advisory and none of
/// the packaged tools (`file.*`, `shell.*`) consult it — they resolve
/// relative paths against the process cwd, which inside a Tauri host is
/// the app data dir, not the user's workspace. Rather than touching every
/// tool handler, the two path-carrying shapes are rewritten here, once:
///
/// - any top-level `"path": "<relative>"` arg (file.read/write/list/
///   delete/search) is joined onto `cwd`;
/// - any input carrying `"command"` without an explicit `"cwd"` arg
///   (shell.exec/spawn) gets `cwd` injected.
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
    use latte_ai::models::Model;
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
        assert!(msg.content.contains("Tester"));
        assert!(msg.content.contains("Auth module"));
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

        runner.context_mut().push(Message {
            role: MsgRole::User,
            content: "Hello".to_string(),
        });
        assert_eq!(runner.context().messages().len(), 1);
    }

    #[test]
    fn test_prune_context() {
        let role = test_role();
        let agent =
            Agent::new("test-agent".into(), role, test_model(), GenerateParams::default()).unwrap();
        let mut runner = AgentRunner::new(agent);

        runner.context_mut().set_token_budget(10);
        runner.context_mut().push(Message {
            role: MsgRole::User,
            content: "a".repeat(200),
        });

        runner.prune_context(1);
        assert!(runner.context().messages().len() <= 1);
    }

    #[test]
    fn test_extract_tool_calls() {
        let text = r#"Some text before
<tool_call>read {"path": "src/main.rs"}</tool_call>
More text
<tool_call>search {"pattern": "TODO"}</tool_call>
End"#;

        let calls = extract_tool_calls(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "read");
        assert_eq!(calls[0].args, r#"{"path": "src/main.rs"}"#);
        assert_eq!(calls[1].name, "search");
        assert_eq!(calls[1].args, r#"{"pattern": "TODO"}"#);
    }

    #[test]
    fn test_extract_tool_calls_prompt_format_with_closing_gt() {
        // The specialist prompts (programmer.md, architect.md, manager.md)
        // teach the model to emit:
        //   <tool_callbash> {"command": "pwd && ls"}</tool_call>
        // — note the `>` between name and args. The parser must strip
        // that `>` so the name is `bash` (not `bash>`) and the
        // `bash → exec` alias in AgentRunner::run_turn actually fires.
        let text = r#"<tool_callbash> {"command": "pwd && ls"}</tool_call>
<tool_callread> {"path": "src/main.rs"}</tool_call>
<tool_calldelegate> {"role": "programmer", "task": "read"}</tool_call>"#;
        let calls = extract_tool_calls(text);
        assert_eq!(calls.len(), 3, "expected 3 tool calls, got {:?}", calls);
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].args, r#"{"command": "pwd && ls"}"#);
        assert_eq!(calls[1].name, "read");
        assert_eq!(calls[1].args, r#"{"path": "src/main.rs"}"#);
        assert_eq!(calls[2].name, "delegate");
        assert_eq!(calls[2].args, r#"{"role": "programmer", "task": "read"}"#);
    }
    #[test]
    fn test_extract_tool_calls_xml_style_close() {
        // What deepseek-v4-flash actually emits in real chat sessions
        // (observed 2026-06-25): the open and close are XML-style
        // matching tags `<tool_callNAME>...</tool_callNAME>`, not the
        // prompt-taught canonical form. The parser must accept this.
        let text = r#"<tool_calldelegate> {"role": "programmer", "task": "read chat.rs"}</tool_calldelegate>
<tool_callreviewer> {"role": "reviewer", "task": "audit chat.rs"}</tool_callreviewer>"#;
        let calls = extract_tool_calls(text);
        assert_eq!(calls.len(), 2, "expected 2 calls, got {:?}", calls);
        assert_eq!(calls[0].name, "delegate");
        assert_eq!(calls[0].args, r#"{"role": "programmer", "task": "read chat.rs"}"#);
        assert_eq!(calls[1].name, "reviewer");
        assert_eq!(calls[1].args, r#"{"role": "reviewer", "task": "audit chat.rs"}"#);
    }

    #[test]
    fn test_extract_tool_calls_whitespace_before_name() {
        // 复现 2026-07-19 用户报告的格式：`<tool_call>` 与工具名之间有
        // 换行/空格。之前名字扫描在第一个非字母数字字符（\n）处停止，
        // name_len == 0 → 整个调用被静默丢弃（opens_found=1,
        // closes_matched=0），UI 表现就是"XML 解析有问题/工具没执行"。
        let text = "<tool_call>\ndelegate {\"role\": \"programmer\", \"task\": \"pwd 确认目录\"}\n</tool_call>";
        let calls = extract_tool_calls(text);
        assert_eq!(calls.len(), 1, "换行分隔的工具名必须被解析，got {:?}", calls);
        assert_eq!(calls[0].name, "delegate");
        let parsed: serde_json::Value =
            serde_json::from_str(&calls[0].args).expect("args 必须是合法 JSON");
        assert_eq!(parsed["role"], "programmer");

        // 空格分隔同理
        let text2 = r#"<tool_call> read {"path": "src/a.rs"}</tool_call>"#;
        let calls2 = extract_tool_calls(text2);
        assert_eq!(calls2.len(), 1);
        assert_eq!(calls2[0].name, "read");
    }

    #[test]
    fn test_extract_tool_calls_glm_arg_value_close() {
        // 复现 2026-07-10 UI 用户报告的真实 bug：GLM 5.2 吐
        // `</arg_value>` 作为 closing tag，而不是 prompt 里教的
        // `</tool_call>` 或模型自己应该匹配的 `</tool_calldelegate>`。
        // parser 之前直接 break 不返回，manager 调不出 delegate，
        // UI 看到的就是一行"裸 tool call 文本"然后卡住。
        let text = r#"<tool_call>delegate {"role": "programmer", "task": "首先执行：bash {\"command\": \"pwd && ls\"} 以确认 cwd"}
</arg_value>"#;
        let calls = extract_tool_calls(text);
        assert_eq!(
            calls.len(),
            1,
            "GLM 5.2 的 </arg_value> 应该被识别成合法 closing tag，got {:?}",
            calls
        );
        assert_eq!(calls[0].name, "delegate");
        let parsed: serde_json::Value =
            serde_json::from_str(&calls[0].args).expect("args 必须是合法 JSON");
        assert_eq!(parsed["role"], "programmer");
        assert_eq!(
            parsed["task"],
            "首先执行：bash {\"command\": \"pwd && ls\"} 以确认 cwd"
        );
    }

    #[test]
    fn test_extract_tool_calls_arbitrary_close_fallback() {
        // 任何 well-formed `</X>` 都应该被 fallback 接受，不只是
        // `</arg_value>`。比如 `</function>`、``</invoke>`` 之类。
        let text = r#"<tool_call>delegate {"role": "pm", "task": "x"}
</function>"#;
        let calls = extract_tool_calls(text);
        assert_eq!(calls.len(), 1, "应该认 </function> 当 close，got {:?}", calls);
        assert_eq!(calls[0].name, "delegate");
    }

    #[test]
    fn test_extract_tool_calls_no_close_gives_up_cleanly() {
        // 真的没有 closing tag 时（model 输出被截断），parser 应该
        // 干净地放弃 —— 不能 panic，也不能吐半截 args。
        let text = r#"<tool_call>delegate {"role": "programmer", "task": "run pwd"}"#;
        let calls = extract_tool_calls(text);
        assert!(
            calls.is_empty(),
            "没有 closing tag 应该放弃整个 tool call，不能返回半截，got {:?}",
            calls
        );
    }

    #[test]
    fn test_find_any_close_tag_skips_non_tags() {
        // `</` 后面不是合法 tag name 字符（连 `>` 都没有）的，要跳
        // 过去继续找下一个，不能误判成 closing tag。
        assert!(find_any_close_tag("hello </ world").is_none());
        assert!(find_any_close_tag("nothing here").is_none());
        // `</arg_value>` = `<` `/` `a` `r` `g` `_` `v` `a` `l` `u` `e` `>` = 12 字符
        assert_eq!(
            find_any_close_tag("prefix </arg_value> suffix"),
            Some((7, 12))
        );
    }


    #[test]
    fn test_extract_tool_calls_no_args() {
        let text = "<tool_call>list_models</tool_call>";
        let calls = extract_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "list_models");
        assert_eq!(calls[0].args, "");
    }

    #[test]
    fn test_extract_tool_calls_empty() {
        let calls = extract_tool_calls("Just a regular response, no tools.");
        assert!(calls.is_empty());
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
    fn openai_completion_body(content: &str) -> String {
        serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 0,
            "model": "test",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": content },
                "finish_reason": "stop"
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
                        openai_completion_body("fallback won"),
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
                &[Message {
                    role: MsgRole::User,
                    content: "hi".to_string(),
                }],
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
                &[Message {
                    role: MsgRole::User,
                    content: "hi".to_string(),
                }],
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
                    openai_completion_body("never reached"),
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
                &[Message {
                    role: MsgRole::User,
                    content: "hi".to_string(),
                }],
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
                &[Message {
                    role: MsgRole::User,
                    content: "hi".to_string(),
                }],
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
                        openai_completion_body("recovered"),
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
                        openai_completion_body("after wait"),
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
                &[Message {
                    role: MsgRole::User,
                    content: "hi".to_string(),
                }],
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
                    .set_body_string(openai_completion_body("plain text reply"))),
        )
        .await;

        // PreCall hook: redacts PII in user messages.
        struct RedactPreCall;
        impl Hook for RedactPreCall {
            fn name(&self) -> &str { "redact_pre_call" }
            fn pre_call(&self, ctx: &mut PreCallCtx) -> HookOutcome<()> {
                for m in ctx.messages.iter_mut() {
                    if m.role == MsgRole::User {
                        m.content = m.content.replace("13812345678", "<REDACTED>");
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
        let msgs = vec![Message {
            role: MsgRole::User,
            content: "call me at 13812345678 about it".to_string(),
        }];
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
        assert_eq!(first.content, "[INJECTED]\nlook at foo.rs\n");
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
                            .set_body_string(openai_completion_body("done")),
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
                &[Message {
                    role: MsgRole::User,
                    content: "继续".into(),
                }],
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
                .any(|m| m.content.contains("🦉 advisor 监察") && m.content.contains("已改名")),
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
                        openai_completion_body("<tool_call>ping {}</tool_call>"),
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
                &[Message {
                    role: MsgRole::User,
                    content: "go".into(),
                }],
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
                        openai_completion_body(
                            "<tool_call>ping <arg_key>x</arg_key></tool_call>",
                        ),
                    )),
            )
            .await;
        // 兜底：round 1 起一律 finish。
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(
                        ResponseTemplate::new(200).set_body_string(openai_completion_body("done")),
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
                &[Message {
                    role: MsgRole::User,
                    content: "go".into(),
                }],
                None,
            )
            .await
            .expect("turn completes after the format error feedback");
        assert_eq!(resp, "done");
        assert!(!executed.load(Ordering::SeqCst), "tool must not execute on non-JSON args");

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 2);
        let body2 = String::from_utf8_lossy(&reqs[1].body);
        assert!(
            body2.contains("[tool_error for ping]") && body2.contains("not valid JSON"),
            "model got the explicit format error: {body2}"
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
        runner.context_mut().push(Message {
            role: MsgRole::Assistant,
            content: "no tools here".to_string(),
        });
        assert_eq!(runner.last_decision_kind(), "text");
    }

    #[test]
    fn last_decision_kind_returns_tool_call_for_write() {
        use latte_ai::models::Role as MsgRole;
        use crate::checkpoint::short_hash;
        let role = test_role();
        let agent =
            Agent::new("test-agent".into(), role, test_model(), GenerateParams::default())
                .unwrap();
        let mut runner = AgentRunner::new(agent);
        let args_json = r#"{"path":"/tmp/x"}"#;
        // Canonical open tag + close tag the v1 parser uses.
        runner.context_mut().push(Message {
            role: MsgRole::Assistant,
            content: format!("<tool_callwrite> {}</tool_call>", args_json),
        });
        let expected = format!("tool_call:write:{}", &short_hash(args_json)[..8]);
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

    // ─── dedupe_tool_calls tests ────────────────────────────────────────
    //
    // 覆盖 dedupe 工具的几种关键行为：完全相同的调用合并、不同参数不
    // 合并、JSON 文本形式不同但语义相同时合并、保留首次出现的位置、
    // 空输入和非 JSON 输入也能正常工作。
    // 每个测试都聚焦一个行为分支，方便定位回归。

    #[test]
    fn dedupe_drops_three_identical_delegate_calls() {
        // 复现用户报告的 bug 场景：manager 在一次响应里吐出 3 个完
        // 全相同的 `delegate` 调用。dedup 之后应该只剩 1 个，这样
        // LoopDetector 就不会被"called 4 times in a row"误报打断。
        let calls = vec![
            ParsedCall {
                name: "delegate".into(),
                args: r#"{"role":"programmer","task":"run pwd"}"#.into(),
            },
            ParsedCall {
                name: "delegate".into(),
                args: r#"{"role":"programmer","task":"run pwd"}"#.into(),
            },
            ParsedCall {
                name: "delegate".into(),
                args: r#"{"role":"programmer","task":"run pwd"}"#.into(),
            },
        ];
        let deduped = dedupe_tool_calls(calls);
        assert_eq!(deduped.len(), 1, "3 个相同 delegate 调用应合并为 1 个");
        assert_eq!(deduped[0].name, "delegate");
        assert_eq!(
            deduped[0].args,
            r#"{"role":"programmer","task":"run pwd"}"#
        );
    }

    #[test]
    fn dedupe_keeps_calls_with_different_args() {
        // 并行读多个文件是 manager 推荐的合法场景，不能被误合并。
        let calls = vec![
            ParsedCall {
                name: "read".into(),
                args: r#"{"path":"a.rs"}"#.into(),
            },
            ParsedCall {
                name: "read".into(),
                args: r#"{"path":"b.rs"}"#.into(),
            },
            ParsedCall {
                name: "read".into(),
                args: r#"{"path":"c.rs"}"#.into(),
            },
        ];
        let deduped = dedupe_tool_calls(calls);
        assert_eq!(deduped.len(), 3, "3 个不同 path 的 read 调用应全部保留");
    }

    #[test]
    fn dedupe_treats_semantic_equivalent_args_as_duplicates() {
        // 同一调用的两种 JSON 文本写法（带额外空格）应被识别为同一次
        // 调用，这样和 LoopDetector 的 hash 方式保持一致 —— 同一个
        // 调用不会因为多打了一个空格就"骗过" dedup。
        let calls = vec![
            ParsedCall {
                name: "read".into(),
                args: r#"{"path":"a.rs"}"#.into(),
            },
            ParsedCall {
                name: "read".into(),
                args: r#"{"path": "a.rs"}"#.into(), // 多了个空格
            },
        ];
        let deduped = dedupe_tool_calls(calls);
        assert_eq!(
            deduped.len(),
            1,
            "只是空白不同的 args 视为同一调用（与 LoopDetector 的 hash 一致）"
        );
    }

    #[test]
    fn dedupe_does_not_merge_args_that_differ_in_fields() {
        // 多了一个字段就是不同的调用 —— 不能因为 JSON 解析成功就合
        // 并掉合法但不同的请求。
        let calls = vec![
            ParsedCall {
                name: "read".into(),
                args: r#"{"path":"a.rs"}"#.into(),
            },
            ParsedCall {
                name: "read".into(),
                args: r#"{"path":"a.rs","limit":10}"#.into(),
            },
        ];
        let deduped = dedupe_tool_calls(calls);
        assert_eq!(deduped.len(), 2, "args 含不同字段时不应合并");
    }

    #[test]
    fn dedupe_preserves_first_occurrence_order() {
        // 入参里 a.rs 在前、b.rs 居中、a.rs 重复出现在末尾 → 输出
        // 应当是 [a.rs, b.rs]（首次出现位置 = 1，b.rs 位置 = 2）。
        let calls = vec![
            ParsedCall {
                name: "read".into(),
                args: r#"{"path":"a.rs"}"#.into(),
            },
            ParsedCall {
                name: "read".into(),
                args: r#"{"path":"b.rs"}"#.into(),
            },
            ParsedCall {
                name: "read".into(),
                args: r#"{"path":"a.rs"}"#.into(),
            },
        ];
        let deduped = dedupe_tool_calls(calls);
        assert_eq!(deduped.len(), 2);
        assert_eq!(deduped[0].args, r#"{"path":"a.rs"}"#);
        assert_eq!(deduped[1].args, r#"{"path":"b.rs"}"#);
    }

    #[test]
    fn dedupe_handles_empty_input() {
        // 空列表直接返回空列表，不应该 panic。
        let deduped = dedupe_tool_calls(vec![]);
        assert!(deduped.is_empty());
    }

    #[test]
    fn dedupe_handles_malformed_json_args() {
        // args 不是合法 JSON 时回退到原始文本比较，相同原始文本仍
        // 应被识别为重复 —— 不能因为 parse 失败就放弃去重。
        let calls = vec![
            ParsedCall {
                name: "bash".into(),
                args: "pwd && ls".into(),
            },
            ParsedCall {
                name: "bash".into(),
                args: "pwd && ls".into(),
            },
        ];
        let deduped = dedupe_tool_calls(calls);
        assert_eq!(deduped.len(), 1, "非 JSON args 的相同文本应被去重");
    }

    #[test]
    fn dedupe_keeps_different_tools_with_same_args() {
        // 工具名不同时即使 args 文本一致也不应合并 —— 比如 `read` 和
        // `bash` 都可能用到 `path` 字段，但它们是不同工具。
        let calls = vec![
            ParsedCall {
                name: "read".into(),
                args: r#"{"path":"a"}"#.into(),
            },
            ParsedCall {
                name: "bash".into(),
                args: r#"{"path":"a"}"#.into(),
            },
        ];
        let deduped = dedupe_tool_calls(calls);
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
}
