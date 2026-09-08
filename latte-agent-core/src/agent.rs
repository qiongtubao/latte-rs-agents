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
use latte_ai::models::{Completion, ContentPart, Message, Role, StreamEvent, TokenUsage};
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
//   5. 终态（成功或失败）都以 `tool_result` 喂回 messages——native 协议
//      要求每个 tool_call_id 都有对应 tool 消息闭环，缺一条下游 API
//      直接 400（"tool_calls must be followed by tool messages"），
//      整个会话卡死。防死循环靠 LoopDetector，不靠断链。
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
    /// 工具名 model 写了我们不认识的名字（registry 扁平名之后的
    /// 名字与配置层一致，一般不该发生）—— alias 后备也没救回来。
    ToolNotFound { tried_aliases: Vec<String> },
    /// PreTool / PostTool hook 主动拒绝（一般是 EnforceToolAllowlist
    /// 之类的策略 hook）。这是有意的，不该 retry。
    HookAborted { hook: String, reason: String },
    /// `tm.execute()` 抛错 —— 网络 5xx / 模型返回奇怪结构 / 反序列化
    /// 失败 / 业务错误。重试一次可能好。
    Execution { reason: String },
    /// 永久性执行错误（文件不存在、缺必填参数、路径非法、权限拒绝
    /// 等）——原样重试必然再失败：不 retry，但要喂回 model 让它换
    /// 路径/换参数。（日志事故：ENOENT、Tool not found 这类错误被
    /// 原样重试一次再失败，96 次 ToolRetry 0 次恢复。）
    PermanentExec { reason: String },
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
            Self::PermanentExec { .. } => "PermanentExec",
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
            Self::PermanentExec { reason } => write!(f, "permanent error: {reason}"),
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
            // 名字错 / hook 故意拒绝 / 永久性执行错误 → 再试也不会好
            ToolCallErrorKind::ToolNotFound { .. }
            | ToolCallErrorKind::HookAborted { .. }
            | ToolCallErrorKind::PermanentExec { .. } => false,
        }
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
    /// 最近一次请求**实际由哪个模型接的**（`model_chain` 下标）。
    ///
    /// 为什么不能靠 `model_chain.first()` 推：trace 里的 `ModelCall` /
    /// `ModelCallSlow` 原来就是这么算的，于是发生 fallback 后仍然报链首模型
    /// —— 实测一次 `glm-5.3 → MiniMax-M3` 回退后，三条 `ModelCall` 全标着
    /// `glm-5.3`，等于把「换没换成模型」这件事从日志里抹掉了。也不能靠
    /// 「链上第一个 available」反推：冷却窗口会过期，回头看时链首又变可用了。
    ///
    /// `chat` / `chat_stream` 选定模型时写入，只用于观测。
    served_index: Arc<std::sync::atomic::AtomicUsize>,
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
            // Arc 共享：克隆出来的 Agent 与原件指向同一次"实际服务模型"
            // 记录，避免 clone 之后观测值退回默认的链首。
            served_index: Arc::clone(&self.served_index),
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
            served_index: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
    }

    /// 热替换整条 model chain（UI 保存模型配置后由
    /// `AgentRunner::maybe_reload_models` 调用）。重建所有 client
    /// （api_key/base_url 烘焙在 client 里，必须重建才能生效），
    /// 同步 `client`/`model_id` 快捷字段；cooldown 随旧 client 丢弃
    /// （新配置重新计冷却，语义正确——换 key 后旧冷却不该沿用）。
    /// 会话 context 不受影响。
    pub fn reload_models(&mut self, models: Vec<latte_ai::models::Model>) -> AgentResult<()> {
        if models.is_empty() {
            return Err(AgentError::InvalidParam(
                "model chain must contain at least one model".into(),
            ));
        }
        let model_chain: Vec<ModelClient> = models
            .into_iter()
            .map(ModelClient::new)
            .collect::<AgentResult<Vec<_>>>()?;
        self.model_id = model_chain[0].model.id.clone();
        self.client = model_chain[0].client.clone();
        self.model_chain = model_chain;
        Ok(())
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
        let mut failures: Vec<(String, String)> = Vec::with_capacity(self.model_chain.len());

        for mc in &self.model_chain {
            if !mc.is_available() {
                continue;
            }
            match mc.client.chat(messages, p).await {
                Ok(c) if is_empty_completion(&c) => {
                    // 空 completion（无文本、无 tool_calls）是厂商侧抖动
                    // （glm/deepseek 系偶发）：原样上交会让下游把空串当
                    // 结论（日志事故：architect 空返回 → workflow 直接判死）。
                    // 按模型局部失败处理：短冷却后跟链走。
                    tried.push(mc.model.id.clone());
                    failures.push((
                        mc.model.id.clone(),
                        "empty completion (no content, no tool_calls)".into(),
                    ));
                    mc.set_cooldown(Duration::from_secs(5));
                }
                Ok(c) => {
                    self.note_served(mc);
                    return Ok(c);
                }
                Err(e) => {
                    tried.push(mc.model.id.clone());
                    failures.push((mc.model.id.clone(), brief_model_error(&e)));
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
                        Ok(c) if !is_empty_completion(&c) => return Ok(c),
                        Ok(_) => {
                            // 空 completion 同样按失败处理（见链式遍历分支）。
                            mc.set_cooldown(Duration::from_secs(5));
                            tried.push(mc.model.id.clone());
                            failures.push((
                                mc.model.id.clone(),
                                "empty completion (no content, no tool_calls)".into(),
                            ));
                        }
                        Err(e) => {
                            // Refresh its cooldown if retryable.
                            if let Some(cd) = cooldown_for_error(&e) {
                                mc.set_cooldown(cd);
                            }
                            tried.push(mc.model.id.clone());
                            failures.push((mc.model.id.clone(), brief_model_error(&e)));
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
                    failures,
                    next_retry_in: new_earliest,
                });
            }
        }

        Err(AgentError::ModelsUnavailable {
            tried,
            failures,
            next_retry_in: earliest,
        })
    }

    /// 流式聊天完成请求。返回 [`StreamEvent`] 通道，调用方逐事件消费。
    ///
    /// 与 [`Agent::chat`] 的区别：`chat` 内部把 Delta 塌缩成一个 `Completion`；
    /// `chat_stream` 把 Delta 通道直接交给调用方，让 UI 逐 token 渲染。
    ///
    /// model chain fallback 只在**连接建立阶段**生效（`chat_stream` 返回 Err）。
    /// 一旦 rx 交给调用方，后续错误以 `StreamEvent::Error` / `HttpError` 推送。
    /// 不做 `WaitPolicy::WaitAndRetry`：流式下重试会重复发请求、产生不连贯 Delta。
    pub async fn chat_stream(
        &self,
        messages: &[Message],
        params: Option<&GenerateParams>,
    ) -> AgentResult<tokio::sync::mpsc::Receiver<StreamEvent>> {
        let p = params.unwrap_or(&self.params);
        let mut tried: Vec<String> = Vec::with_capacity(self.model_chain.len());
        let mut failures: Vec<(String, String)> = Vec::with_capacity(self.model_chain.len());

        for mc in &self.model_chain {
            if !mc.is_available() {
                continue;
            }
            match mc.client.chat_stream(messages, p).await {
                Ok(rx) => {
                    self.note_served(mc);
                    return Ok(rx);
                }
                Err(e) => {
                    tried.push(mc.model.id.clone());
                    failures.push((mc.model.id.clone(), brief_model_error(&e)));
                    if let Some(cd) = cooldown_for_error(&e) {
                        mc.set_cooldown(cd);
                    } else {
                        return Err(e.into());
                    }
                }
            }
        }

        Err(AgentError::ModelsUnavailable {
            tried,
            failures,
            next_retry_in: self
                .model_chain
                .iter()
                .filter_map(|mc| mc.cooldown_remaining())
                .min(),
        })
    }

    /// 记下这次请求由链上哪个模型接的（仅观测用，见 `served_index`）。
    fn note_served(&self, mc: &ModelClient) {
        if let Some(i) = self
            .model_chain
            .iter()
            .position(|m| m.model.id == mc.model.id)
        {
            self.served_index
                .store(i, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// 最近一次请求**实际由哪个模型**接的 id。没发过请求时返回链首。
    ///
    /// trace 里报模型 id 一律走这里，别再用 `model_chain.first()` —— 那样
    /// fallback 之后日志会继续标着链首模型，等于把换模型这件事抹掉。
    pub fn served_model_id(&self) -> String {
        let i = self.served_index.load(std::sync::atomic::Ordering::Relaxed);
        self.model_chain
            .get(i)
            .or_else(|| self.model_chain.first())
            .map(|mc| mc.model.id.clone())
            .unwrap_or_default()
    }

    /// 把**当前正在服务这一路流**的模型置入冷却，返回它的 id。
    ///
    /// 用途：连接**已经建立成功**、但第一个事件就是 `HttpError` / `Error`
    /// 的场景（实测 glm-5.3 中转站的 400）。不冷却的话下一次重开流还会选中
    /// 同一个模型，永远换不掉，模型链形同虚设。
    pub fn cooldown_serving_model(&self, cd: Duration) -> Option<String> {
        let i = self.served_index.load(std::sync::atomic::Ordering::Relaxed);
        let mc = self.model_chain.get(i).or_else(|| self.model_chain.first())?;
        mc.set_cooldown(cd);
        Some(mc.model.id.clone())
    }

    /// 链上下一个仍可用的模型 id（跳过 `skip_id`），只用于 trace 展示。
    pub fn next_available_model_after(&self, skip_id: &str) -> Option<String> {
        self.model_chain
            .iter()
            .filter(|mc| mc.model.id != skip_id && mc.is_available())
            .map(|mc| mc.model.id.clone())
            .next()
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
/// Returns `None` only for local errors that switching models cannot
/// plausibly bypass (config, serialization). Retryable vendor-side
/// failures — rate limits, 4xx/5xx, transport errors, auth failures,
/// stream timeouts — get a cooldown and the chain falls through.
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

/// `stop_reason` 是否表示"输出撞上长度上限"（各家命名不同：OpenAI 系
/// `length`、Anthropic 系 `max_tokens`、部分兼容端点用
/// `max_output_tokens` / `output_limit`）。
///
/// 只在这一处集中判定：截断的后果是 tool_calls 的 arguments 半截，必须
/// 与"模型写错 JSON 格式"区分开——前者原样重试必然再撞一次。
fn is_length_truncation(stop_reason: &str) -> bool {
    const MARKERS: &[&str] = &["length", "max_tokens", "max_output_tokens", "output_limit"];
    let s = stop_reason.trim().to_ascii_lowercase();
    MARKERS.iter().any(|m| s == *m)
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

/// Ensure every tool call in a single assistant response has a unique,
/// non-empty `id`.
///
/// Native function-calling pairs each `tool_result` back to its call by
/// `tool_call_id`. Some providers hand parallel calls the *same* id (or
/// an empty one); left as-is, multiple results collapse onto one id and
/// the model only sees the last — e.g. two `delegate` calls to the same
/// role would lose all but the final specialist's return. We rewrite any
/// empty/duplicate id to a synthesized `call_<n>`, keeping the first
/// occurrence of each distinct id. Applied *after* dedupe so genuinely
/// identical calls are already collapsed.
fn ensure_unique_tool_call_ids(calls: &mut [latte_ai::models::ToolCall]) {
    use std::collections::HashSet;
    let mut seen: HashSet<String> = HashSet::new();
    for (i, tc) in calls.iter_mut().enumerate() {
        let needs_new = tc.id.trim().is_empty() || seen.contains(&tc.id);
        if needs_new {
            let mut n = i;
            let mut candidate = format!("call_{n}");
            while seen.contains(&candidate) {
                n += 1;
                candidate = format!("call_{n}");
            }
            tc.id = candidate;
        }
        seen.insert(tc.id.clone());
    }
}
// ─── LoopDetector ─────────────────────────────────────────────────────────

/// Detects when a model is stuck calling the same tool with the
/// same args repeatedly, and breaks the tool-call loop.
///
/// The "model is stuck" failure mode: the model emits the same
/// tool call on every round, gets the same result, but can't break
/// out of the pattern on its own.
///
/// **这是 deadline-only 模式下唯一的自动刹车。** 轮次上限已退化为 soft
/// warning，`deadline` 只在 delegate / workflow step 上设置（交互式 chat
/// 不设，按设计交给人工 ⏸ 或 advisor 叫停），所以没有别的机制会兜住死
/// 循环。探测器由 `run_turn` 在**整个 turn 内共用一个实例**，streak 跨
/// round 累积；放进 round 循环里就等于关掉它（模型一轮通常只发一个调
/// 用，streak 永远是 1）。
///
/// 判定键是 (tool_name, args_json) 的**逐字节相同**。两条规则并行：
///
/// 1. **连击**：连续 [`LOOP_STREAK_THRESHOLD`] 次同一个 key → 熔断。
/// 2. **打转**：窗口（[`LOOP_WINDOW_SIZE`] 次调用）填满后，若其中只有
///    ≤ [`LOOP_CYCLE_MAX_DISTINCT`] 种 key、且每种都出现 ≥
///    [`LOOP_CYCLE_MIN_EACH`] 次 → 熔断。
///
/// 只有规则 1 时存在一个洞：模型在两个调用之间来回跳（`read A` →
/// `search B` → `read A` → `search B` …），每次都与上一次不同，`streak`
/// 恒为 1，永远不熔断。而交互式 chat 既没有轮次上限也不设 `deadline`，
/// 于是无限空转。规则 2 就是补这个洞——`history` 窗口此前只被
/// `.last()` 读过，维护了却不用，现在真正派上用场。
///
/// 两条规则都要求参数**逐字节相同**，所以「改参数后重试」这种正常进展
/// 不会被误杀。规则 2 额外要求「每种 key 都重复出现」，用来区分真打转和
/// 「一串相同调用里夹了一个孤立的别的调用」——后者是被打断的连击，交给
/// 规则 1 判，不该按环处理。
#[derive(Default)]
struct LoopDetector {
    /// 最近 [`LOOP_WINDOW_SIZE`] 次 (tool_name, args_json)。规则 1 只看
    /// 末尾，规则 2 看整个窗口。
    history: Vec<(String, String)>,
    /// Count of consecutive identical tool calls seen at the tail.
    streak: usize,
}

const LOOP_WINDOW_SIZE: usize = 8;
const LOOP_STREAK_THRESHOLD: usize = 5;

/// 判「打转」时窗口内最多允许几种不同调用。2 = 只在两个调用之间来回跳。
const LOOP_CYCLE_MAX_DISTINCT: usize = 2;

/// 判「打转」时每种调用至少要出现几次。
///
/// 这一条把「7 次 read A 中间夹 1 次 search」排除在外：那是被打断的连击
/// （规则 1 的辖区），不是环。少了它，`loop_detector_resets_on_different_call`
/// 那种「换个调用清零连击」的正常语义会被误判成打转。
const LOOP_CYCLE_MIN_EACH: usize = 2;

/// 「同一工具连续被确定性校验拒绝」的两级阈值。
///
/// 与 [`LOOP_STREAK_THRESHOLD`] 分工：`LoopDetector` 只认 (工具, args)
/// **逐字节相同**的重复；而模型常常每轮改一点参数再撞同一类校验
/// （实测事故：manager 连着 3 轮调 `plan`，每轮重写 8 个任务
/// 40+ 条 paths，每轮烧 100~150s），那种模式只有这个只看
/// (工具, 失败类别) 的计数器能抓。
///
/// 提成模块级常量是为了让实现和测试共用同一个真值来源——这两个数字
/// 曾经是 `run_turn` 里的局部 `const`，被从 3/5 调到 5/8 时测试没跟着
/// 改，于是测试长期失败却没人发现。
const PERMANENT_NUDGE_AT: usize = 5;
const PERMANENT_BREAK_AT: usize = 8;

impl LoopDetector {
    /// Record a tool call and decide whether to continue or break.
    ///
    /// `tool_name` is the name emitted by the model (e.g. `"read"`,
    /// `"list"` — registry 扁平化后与配置层名字一致).
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
        // 规则 2：窗口填满后判「打转」。放在 push 之后，这样刚好第
        // LOOP_WINDOW_SIZE 次调用就能判。
        if self.history.len() >= LOOP_WINDOW_SIZE {
            if let Some(reason) = self.cycle_reason() {
                return LoopDecision::Break(reason);
            }
        }
        LoopDecision::Continue
    }

    /// 窗口是否构成「在少数几个调用之间来回打转」。
    ///
    /// 判据：窗口内不同 key ≤ [`LOOP_CYCLE_MAX_DISTINCT`] 种，且**每种**
    /// 都出现 ≥ [`LOOP_CYCLE_MIN_EACH`] 次。后半条不可省——否则「一串相同
    /// 调用里夹了一个孤立的别的调用」也会被判成环。
    fn cycle_reason(&self) -> Option<String> {
        // 窗口只有 8 项，线性统计比建 HashMap 更省。
        let mut counts: Vec<(&(String, String), usize)> = Vec::new();
        for k in &self.history {
            match counts.iter_mut().find(|(key, _)| *key == k) {
                Some((_, c)) => *c += 1,
                None => counts.push((k, 1)),
            }
        }
        if counts.len() > LOOP_CYCLE_MAX_DISTINCT {
            return None;
        }
        if counts.iter().any(|(_, c)| *c < LOOP_CYCLE_MIN_EACH) {
            return None;
        }
        let cycle = counts
            .iter()
            .map(|((tool, args), c)| {
                format!("{tool}({}) ×{c}", truncate_tool_summary(args, 60))
            })
            .collect::<Vec<_>>()
            .join(" ↔ ");
        Some(format!(
            "最近 {} 次工具调用只在 {} 种调用之间来回打转：{cycle}。\
             参数逐次完全相同，模型在原地绕圈而非取得进展。\
             Breaking out so the user can intervene.",
            self.history.len(),
            counts.len(),
        ))
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

/// 模型底层错误的紧凑摘要（≤120 字符）：塞进
/// `AgentError::ModelsUnavailable.failures`，让"all models
/// unavailable"能直接看出每个模型死于什么（限流/鉴权/网络）。
fn brief_model_error(e: &AiError) -> String {
    const CAP: usize = 120;
    let s = e.to_string();
    if s.chars().count() <= CAP {
        return s;
    }
    let cut: String = s.chars().take(CAP).collect();
    format!("{cut}…")
}

fn cooldown_for_error(e: &AiError) -> Option<Duration> {    match e {
        // Vendor told us how long to wait — respect it (floor 1s).
        AiError::RateLimited { retry_after, .. } => {
            let secs = retry_after.max(1.0);
            Some(Duration::from_secs_f64(secs))
        }
        // HTTP status codes from the upstream provider.
        AiError::Api { status, .. } => match *status {
            429 => Some(Duration::from_secs(60)),
            500..=599 => Some(Duration::from_secs(30)),
            // 402 Payment Required / 余额不足：在**本次会话内不会自愈**
            // ——充值是人的动作，不是等一会儿就好。5s 冷却意味着每轮
            // 失败切换都要再白烧它一次：实测 jemalloc 那次 61 分钟的
            // 会话里，`deepseek-v4-flash: 402 Insufficient Balance`
            // 出现在每一次全链失败的日志里，每次都占掉一整个往返。
            //
            // 给一个远超正常会话时长的冷却，等价于"本次会话内摘掉它"，
            // 但不需要改配置、也不会永久禁用（新进程重新试一次）。
            402 => Some(Duration::from_secs(24 * 3600)),
            // 4xx (other than 429/402) usually means "this model id doesn't
            // exist on this vendor" or "bad request payload for this
            // model". Walking the fallback chain is still worth it
            // because the next model may have a different id format or
            // accept the payload. Short cooldown to avoid hammering a
            // broken model.
            400..=499 => Some(Duration::from_secs(60)),
            // Anything else (3xx redirects we don't auto-follow, 6xx
            // exotic) — also retryable, very long cooldown.
            _ => Some(Duration::from_secs(30)),
        },
        AiError::Http(_) => Some(Duration::from_secs(10)),
        // 认证失败：本系统里每个模型有**各自的 key/厂商**（deepseek
        // 官方、kimi、minimax、glm 中转……），一个模型 token 失效并不
        // 意味着整链都坏——glm 中转 key 过期时 deepseek 官方可能健
        // 在。短冷却让链落到下一个模型；若全链都 auth 失败，最终以
        // ModelsUnavailable 报错，信息同样明确。
        AiError::Auth(_) => Some(Duration::from_secs(5)),
        // 流式超时/中断（TTFB 首事件超时、idle 超时、连接中断）：多为
        // 厂商侧卡顿或 payload 过大处理慢，换链上下一个模型很可能立刻
        // 能跑。短冷却后跟链走；全链都超时才以 ModelsUnavailable 进入
        // 「自动暂停 → 用户继续 → 重试」路径，而不是整 turn 直接硬失败。
        AiError::Stream(_) => Some(Duration::from_secs(10)),
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
    /// Exact persisted transcript for the session that owns this runner.
    /// The embedding runtime binds it; tools never select by id or mtime.
    current_session_file: Option<std::path::PathBuf>,
    /// 本轮 turn 实际执行成功的工具调用次数。D6 ToolCallEcho
    /// 需要这个数判断"response 含 `<read>` 但 tool_use_count=0"
    /// 的回声模式。run_turn 结束时设置，run_turn_gated 据此
    /// 跑 check_response_gates。
    /// pub(crate) so workflow.rs can read it for advisor return review
    /// (tool-call evidence prevents hallucinated "fabricated answers" verdict).
    pub(crate) last_turn_tool_count: usize,
    /// 本轮 turn 实际工具调用摘要（工具名 + 结果前 200 字符），
    /// 供 advisor delegate-return 审查核实「工具确实执行了对应任务」。
    /// 在 `run_turn` 内与 `last_turn_tool_count` 同步收集；
    /// `run_turn_gated` 重试时会被 `run_turn` 开头重置。
    /// `take_last_turn_tool_summary()` 读取并清空。
    pub(crate) last_turn_tool_summaries: Vec<String>,
    /// Pre-persistence gate 配置。None = 不 gate（向后兼容老调用方）。
    /// Some(_) = run_turn 自动按 GateConfig 跑 D5/D6 检查，命中
    /// 时通过 advisor_hints 队列注入 hint 并触发同 turn 重跑
    /// （最多 config.max_retries 次）。
    gate_config: Option<crate::advisor_monitor::GateConfig>,
    /// Advisor v3 pause gate（intervene 暂停门）。Some(_) 时
    /// `run_turn` 在每个 tool-round 边界（drain advisor hint 的
    /// 同一位置）调 `wait_if_requested()`：monitor 判 Intervene
    /// 置位后 runner 挂起等用户拍板。只装到 watched role 的主
    /// runner（driver 单角色 loop）；delegate specialist 不带。
    pause_gate: Option<crate::advisor_monitor::AdvisorPauseGate>,
    /// Session-level 暂停门（用户按 ⏸ 触发），与 advisor `pause_gate` 正交。
    agent_pause_gate: Option<std::sync::Arc<crate::pause_gate::AgentPauseGate>>,
    /// 流式模式开关（运行时可切换）。`load(true)` 时 `run_turn` 用
    /// `Agent::chat_stream` 消费 Delta 事件，UI 逐 token 渲染；
    /// `None` / `load(false)` 时走非流式 `Agent::chat`（默认）。
    stream_mode: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// 模型热更新源：Some(_) 时 `run_turn` 入口与「模型不可用暂停→
    /// 恢复重试」点会比对 resolver 代际，配置变了就用
    /// `Agent::reload_models` 重建 model chain——UI 保存模型配置后
    /// 运行中的 session 不用重启即可生效。None = 不热更新（测试/
    /// 嵌入方自建 runner 的默认）。
    model_source: Option<RunnerModelSource>,
    // 强制 native function-calling：tool schema 经 GenerateParams.tools
    // 下发，模型返回结构化 `completion.tool_calls`。文本 `<tool_call>`
    // 协议已移除，不再有降级路径——provider 必须支持 OpenAI/Anthropic
    // `tools` 字段。tool_choice 取自 `agent.params.tool_choice`（默认 Auto）。
    /// 循环内 deadline（对齐 oh-my-pi）：绝对墙钟时刻。每个 model call
    /// 前检查：超时时优雅退出、返回 partial 产出，而不是被外层 tokio
    /// timeout abort（后者会丢失所有未 flush 的 partial 文本）。
    /// `None` = 不启用循环内 deadline（向后兼容：外层 tokio timeout 仍兜底）。
    deadline: Option<std::time::Instant>,
}

/// `AgentRunner` 的模型热更新源：记录链是从哪个 resolver + 解析参数
/// 来的，以及构建时的配置代际。
#[derive(Clone)]
struct RunnerModelSource {
    resolver: std::sync::Arc<crate::model_resolver::ModelResolver>,
    tier: crate::model_resolver::ModelTier,
    chain_ids: Vec<String>,
    applied_generation: u64,
}

/// 把 ToolError 归类到 `ToolCallErrorKind`。
/// 调用方（retry loop）只关心"能不能重试"：
/// - `ToolError::ToolNotFound` → ToolNotFound（永久，不重试）；
/// - 消息匹配参数/路径类永久模式（ENOENT、缺必填、非法路径等）→
///   PermanentExec（不重试，但喂回 model 换路径）；
/// - 其余 ToolExecution 视为瞬时执行错误 → Execution（重试一次）。
fn classify_tool_execution_error(
    e: &latte_rs_agent_tools::error::ToolError,
) -> ToolCallErrorKind {
    if let ToolError::ToolNotFound { name, .. } = e {
        return ToolCallErrorKind::ToolNotFound {
            tried_aliases: vec![name.clone()],
        };
    }
    // 工具管理器的 wall-clock 熔断。此前没有这一条：`ToolTimeout` 的
    // Display（"Tool execution timeout: …"）不匹配任何 PERMANENT 模式，
    // 于是被归成 `Execution`，而 `ToolCallErrorKind::Timeout` 是个**从未
    // 被构造过**的死 variant。后果是超时和普通执行错误共用一条重试路径，
    // 看不出区别、也没法给超时单独定预算。
    if let ToolError::ToolTimeout { .. } = e {
        return ToolCallErrorKind::Timeout;
    }
    // 参数校验失败：确定性输入错误，同参数重试必然拿到同一份拒绝。
    // 判不可重试，把错误喂回模型让它改参数。
    //
    // 此前没有这一条，靠下面 PERMANENT 那张**消息子串黑名单**兜：
    // schema 层的「缺必填参数」恰好命中 "is required" 所以侥幸正确，
    // 而 handler 内部的校验只要文案没撞上关键词就漏网。实测
    // 2026-08-26 会话：code_graph 的「无法从 path 推断语言，请显式传
    // lang」一个词都没命中 → 归成 Execution → 同参数自动重试 2 次，
    // 每次都拿到同一份报错。黑名单要靠人穷举中文文案，每加一个工具
    // 漏一次；改判类型。
    if let ToolError::Validation { .. } = e {
        return ToolCallErrorKind::PermanentExec {
            reason: e.to_string(),
        };
    }
    let msg = e.to_string();
    const PERMANENT: &[&str] = &[
        "No such file or directory",
        "Path not found",
        "Not a file",
        "is required",
        "must be",
        "must not",
        "invalid",
        "Permission denied",
        "系统运行时内部文件",
        // plan 工具的确定性输入校验失败：原样重试只会再次被同一
        // 份错误清单拒绝；应把错误喂回 model，让它修正 paths。
        "paths 范围重叠",
        "第一级目录在仓库里不存在",
        "疑似幻觉路径",
        "tasks[",
        // plan 工具的 workflow/task_type 名提交侧校验：同为确定性
        // 输入错误（实测 2026-09 jemalloc 会话：task_type:"learning"
        // 被同参数自动重试 2 次，错误才喂回模型）。
        "unknown workflow",
        "unknown task_type",
        // code_graph dependency preflight/query spawn failures are deterministic
        // until the local ast-grep installation changes. Retrying the same call
        // immediately only burns a tool round-trip.
        "code_graph backend unavailable",
        "ast-grep query failed",
        "backend protocol failure",
        // ask_human 的"错误返回"是设计好的控制流（暂停会话），不是
        // 执行失败——重试只会重复 pause + 重复发 AskHuman trace 事件。
        "session paused",
    ];
    if PERMANENT.iter().any(|p| msg.contains(p)) {
        ToolCallErrorKind::PermanentExec { reason: msg }
    } else {
        ToolCallErrorKind::Execution { reason: msg }
    }
}

/// `ToolNotFound` 回填给模型的错误文本：区分「工具真的不存在」与
/// 「工具存在但本角色/本 step 未授权」。
///
/// 动机（实测实锤）：programmer 调 `edit` 拿到的是 `Tool not found:
/// edit`。但 `edit` 其实**注册了**（`EditToolsPackage` 在
/// `builtin_tool_packages()` 里），只是被角色 allowlist 过滤掉了。这条
/// 报错把「未授权」说成「不存在」，模型于是判定该能力不存在、改用
/// bash 绕路，而正确出路是 `request_tool` 申请。
///
/// 提示必须短：`tool_result` 回填有 256 字节截断，长文本会被砍掉尾部。
fn tool_not_found_detail(name: &str, raw_detail: String) -> String {
    let short = short_tool_name(name);
    if crate::controller::full_tool_pool().contains_key(short) {
        return format!(
            "工具 `{short}` 存在但本角色/本步骤未授权。要用它请先调 \
             request_tool(tool_name=\"{short}\", reason=...) 申请；\
             或改用已授权工具完成同一目的。"
        );
    }
    raw_detail
}

/// Short (namespace-stripped) tool name, e.g. `manager.delegate` → `delegate`.
/// 扁平化后内建工具注册名已无点号（恒等）；保留剥前缀逻辑用于 delegate
/// 检测（`short_tool_name(&tc.name) == "delegate"`）及 namespace 遗留兼容。
fn short_tool_name(name: &str) -> &str {
    name.rsplit_once('.').map(|(_, s)| s).unwrap_or(name)
}
/// advisor delegate-return 审查的动机是「judge based on evidence」——
/// 工具摘要太长会挤占审查上下文（还有 32K 字符的结果预算），
/// 单条截断即可。返回的字符串以 `…[+N]` 结尾标记丢弃的字符数。
fn truncate_tool_summary(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        text.to_string()
    } else {
        let head: String = text.chars().take(max).collect();
        format!("{head}…[+{}]", count - max)
    }
}

/// 模型不可用自动暂停后，最多连续自动重试几次；用尽转人工。
///
/// 5 次配合 [`auto_pause_backoff`] 的退避序列约覆盖 4 分钟——足够扛过
/// 常见的厂商限流窗口和本机网络抖动（那次实测实际只需 9s），
/// 又不至于在真·配置错误上磨太久。
/// env `LATTE_AGENT_AUTO_PAUSE_MAX_RETRIES` 可覆盖，0 = 不自动重试
/// （退回「立刻等人工 ▶」的旧行为）。
const DEFAULT_AUTO_PAUSE_MAX_RETRIES: u32 = 5;

fn auto_pause_max_retries() -> u32 {
    std::env::var("LATTE_AGENT_AUTO_PAUSE_MAX_RETRIES")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(DEFAULT_AUTO_PAUSE_MAX_RETRIES)
}

/// 自动暂停的退避上限：再长就不如交给人。
const AUTO_PAUSE_BACKOFF_CAP: Duration = Duration::from_secs(120);

/// 第 `attempt` 次（0-based）自动重试前等多久。
///
/// 以厂商给的 cooldown（`ModelsUnavailable::next_retry_in`）为基线做
/// 二进制指数退避并封顶：cooldown 是「最早那个模型退出冷却」的时刻，
/// 照它重试一次是对的，但如果重试完还是全挂，说明问题比单个模型的
/// 冷却更大（本机网络、全域限流），这时必须拉长间隔而不是原地
/// 空转。下界 1s，避免 cooldown 为 0 时变成忙等。
fn auto_pause_backoff(attempt: u32, cooldown: Duration) -> Duration {
    let base = cooldown.max(Duration::from_secs(1));
    let factor = 1u32 << attempt.min(6); // 1,2,4,…,64
    base.saturating_mul(factor).min(AUTO_PAUSE_BACKOFF_CAP)
}

/// 失败列表是否**全部**为「请求没送出去」类（连接/DNS/代理/连接超时）。
///
/// 多个厂商、多个域名同时连不上，几乎必然是本机网络或代理的问题，而不
/// 是「所有模型都挂了」。空列表返回 false —— 没有证据不下结论。
fn failures_are_all_transport(failures: &[(String, String)]) -> bool {
    !failures.is_empty()
        && failures
            .iter()
            .all(|(_, err)| err.contains(latte_ai::error::TRANSPORT_FAILURE_MARKER))
}

/// `ModelsUnavailable` 的人类可读摘要：tried/failures + 最近可重试
/// 时间。给自动暂停的 Paused 事件原因用。
fn model_failures_summary(e: &AgentError) -> String {
    match e {
        AgentError::ModelsUnavailable {
            failures,
            next_retry_in,
            ..
        } => {
            let f = failures
                .iter()
                .map(|(id, err)| format!("{id}: {err}"))
                .collect::<Vec<_>>()
                .join("; ");
            // 全链都连不上 → 先把结论说出来。否则这段摘要会以
            // 「all models unavailable + 4 个模型名」开头，读起来像
            // 配额/鉴权问题，实际是本机网络断了。
            let f = if failures_are_all_transport(failures) {
                format!(
                    "疑似本机网络/代理故障：{} 个模型的请求都没能送出（连接失败），\
                     并非厂商侧不可用——请先检查网络、代理与 DNS。原始失败：{f}",
                    failures.len()
                )
            } else {
                f
            };
            match next_retry_in {
                Some(d) => format!("{f}（{}s 后可自动重试）", d.as_secs()),
                None => f,
            }
        }
        other => other.to_string(),
    }
}

/// 失败列表里是否包含「上下文超出模型窗口」类错误。这类失败靠原样
/// 重试无法恢复（payload 不变），需要裁剪待发消息或换更大上下文的模型。
fn failures_include_context_overflow(e: &AgentError) -> bool {
    const PATTERNS: &[&str] = &[
        "context window",
        "context length",
        "maximum context",
        "prompt is too long",
        "too many tokens",
        "request too large",
    ];
    match e {
        AgentError::ModelsUnavailable { failures, .. } => failures.iter().any(|(_, err)| {
            let low = err.to_lowercase();
            PATTERNS.iter().any(|p| low.contains(p))
        }),
        _ => false,
    }
}

/// 回填给模型的**工具错误**详情上限（字节）。
///
/// 曾经是 256，且是纯截头。两个后果都实测到了：
/// - `plan` 的 paths 冲突清单 5855 字 / 45 对被砍到只剩开头，模型看不全
///   就修不动（代码里为此加了 `assert!(detail.len() <= 256)` 来迁就）；
/// - `code_graph` 的 "可用 kind：…" 这类**修复指引在尾部**，正好被砍掉。
///
/// 截头之所以最坏：错误的「是什么」在头部，「怎么改」在尾部，砍尾等于
/// 把唯一可操作的信息扔了，模型只能原样重试——一次白跑的模型往返比几 KB
/// 上下文贵几个数量级。所以改成 2KB + 中段省略（见
/// [`compact_middle_out`]），头尾都保住。
const MAX_TOOL_ERROR_BYTES: usize = 2048;

/// 中段省略式压缩：超限时保留头 60% + 尾 40%，中间换成一行标记。
///
/// 与"截断"的区别是**不丢两端**：错误的分类在头部、修复指引在尾部，
/// 两头都得留。标记里带上省略字节数，模型知道自己看的是压缩过的。
/// `max_bytes` 是压缩后正文的预算（标记本身不计入，它是常数级）。
fn compact_middle_out(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let head_budget = max_bytes * 6 / 10;
    let tail_budget = max_bytes - head_budget;
    let head = crate::trace::utf8_safe_prefix(text, head_budget);
    let tail = crate::trace::utf8_safe_suffix(text, tail_budget);
    let elided = text.len().saturating_sub(head.len() + tail.len());
    format!("{head}\n…[中段省略 {elided} 字节，共 {} 字节]…\n{tail}", text.len())
}

/// [`compact_middle_out`] 的字符版（预算以字符而非字节计）。
fn compact_middle_out_chars(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    let head_budget = max_chars * 6 / 10;
    let tail_budget = max_chars - head_budget;
    let head: String = text.chars().take(head_budget).collect();
    let tail: String = text
        .chars()
        .skip(total.saturating_sub(tail_budget))
        .collect();
    let elided = total - head_budget - tail_budget;
    format!("{head}\n…[中段省略 {elided} 字符，原 {total} 字符]…\n{tail}")
}

/// 单条待发消息的文本上限（字符）。成功的工具结果原样回填、不设上限；
/// 一旦因此撑爆模型上下文，恢复重试前靠它把超长消息压成头 + 尾 + 标记。
const OVERSIZED_MSG_CHARS: usize = 8_000;

/// 一个 turn 内**保留逐字原文**的最近工具结果条数。更早的结果压成
/// 头 + 尾 + 标记（[`STALE_TOOL_RESULT_CHARS`]）。
const CONTEXT_KEEP_RECENT_TOOL_RESULTS: usize = 6;

/// 被判定为"陈旧"的工具结果压缩后的字符预算。留头留尾而不是换成一句
/// 占位符：模型要能从头部认出"这个文件我已经读过了"，否则会重新读一遍，
/// 省下的往返又还回去了。
///
/// 默认取值偏保守（900 = 头 540 + 尾 360）：压得越狠省得越多，但模型
/// 认不出读过什么就会重读，一次重读的代价是**一整轮往返**（含重传全部
/// 历史），比多留几百字符贵得多。`LATTE_AGENT_CONTEXT_STALE_CHARS` 可调。
const STALE_TOOL_RESULT_CHARS: usize = 900;

/// 陈旧工具结果**低于**这个字符数就不动它——压缩的收益还不够抵消
/// 标记本身的开销，而且小结果多是简短的成功/失败回执，留着最有用。
/// 运行时门槛是 `context_stale_chars() * 4 / 3`（至少要能省下 1/3）；
/// 本常量是默认预算下的取值（900 * 4/3 = 1200）。
const STALE_TOOL_RESULT_FLOOR: usize = 1_200;

/// 主动压缩：`LATTE_AGENT_CONTEXT_COMPACT` 设 `0`/`false`/`no`/`off` 关闭
/// （默认开）。关掉即退回"每轮重传全部工具结果原文"的旧行为。
fn context_compact_enabled() -> bool {
    !matches!(
        std::env::var("LATTE_AGENT_CONTEXT_COMPACT")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "0" | "false" | "no" | "off"
    )
}

/// 保留逐字的最近工具结果条数，`LATTE_AGENT_CONTEXT_KEEP_RECENT` 可调，
/// 钳在 `1..=64`（0 会把当轮刚拿到的结果也压掉，模型立刻失去判断依据）。
fn context_keep_recent() -> usize {
    std::env::var("LATTE_AGENT_CONTEXT_KEEP_RECENT")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(CONTEXT_KEEP_RECENT_TOOL_RESULTS)
        .clamp(1, 64)
}

/// 陈旧工具结果的压缩预算（字符），`LATTE_AGENT_CONTEXT_STALE_CHARS` 可调，
/// 钳在 `200..=8000`。调小省得更多但模型更容易认不出读过什么而重读；
/// 调大更安全但收益递减。
fn context_stale_chars() -> usize {
    std::env::var("LATTE_AGENT_CONTEXT_STALE_CHARS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(STALE_TOOL_RESULT_CHARS)
        .clamp(200, 8_000)
}

/// 把本轮待发列表里**陈旧的**工具结果压成头 + 尾 + 标记，返回被压缩的
/// 条数。只动 `Role::Tool` 消息的文本，**绝不删消息**。
///
/// # 为什么需要它
///
/// 工具循环每一轮都把"到目前为止的全部对话"重新发一遍，于是第 N 轮要
/// 重传前 N-1 轮所有工具结果的原文。实测（2026-09-07 jemalloc 会话）：
/// 一个 reviewer 子会话 70 次工具调用，返回内容合计只有 188,498 字符
/// （≈47K token），而那一个 turn 消耗了 **1,795,762 输入 token** —— 38 倍。
/// 整个会话 22.19M 输入 token 里，工具返回的唯一内容仅 ≈0.97M，放大 23 倍。
/// 真正付钱的不是"读得多"，是"读完反复重传"。
///
/// # 为什么只压不删
///
/// 每个 `tool_call` 必须有 id 对得上的 `tool_result`，删一条就破坏配对、
/// 请求直接被 provider 拒。压缩文本不动结构，与既有的
/// [`slim_oversized_messages`]（超窗恢复路径）同一套做法。
///
/// # 为什么留头留尾
///
/// 换成"[已折叠]"这类占位符会让模型认不出自己读过什么，转头重新读一遍
/// ——省下的往返又还回去了，还可能撞上死循环熔断。留头能认出文件/符号，
/// 留尾能保住 `read` 的结构摘要 footer、`code_graph` 的"怎么收窄"提示、
/// 批量读的 `failed` 清单这些出口信息。
///
/// 工具结果本就不入持久 context（见 [`slim_oversized_messages`]），因此
/// 压缩只影响这一次待发 payload，会话记录与 trace 落盘均不受影响。
fn compact_stale_tool_results(messages: &mut [Message], keep_recent: usize) -> usize {
    let budget = context_stale_chars();
    // 至少要能省下 1/3 才值得动它（预算的 4/3）。默认 900 → 门槛 1200，
    // 与 [`STALE_TOOL_RESULT_FLOOR`] 一致。
    let floor = budget * 4 / 3;
    // 先定位所有工具结果的下标，末尾 keep_recent 条豁免。
    let tool_idx: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == Role::Tool)
        .map(|(i, _)| i)
        .collect();
    if tool_idx.len() <= keep_recent {
        return 0;
    }
    let stale = &tool_idx[..tool_idx.len() - keep_recent];
    let mut compacted = 0usize;
    for &i in stale {
        let mut touched = false;
        for part in messages[i].content.iter_mut() {
            if let ContentPart::Text { text } = part {
                if text.chars().count() > floor {
                    *text = compact_middle_out_chars(text, budget);
                    touched = true;
                }
            }
        }
        if touched {
            compacted += 1;
        }
    }
    compacted
}

/// 原地裁剪消息列表中超过 [`OVERSIZED_MSG_CHARS`] 的文本 part（system
/// 消息除外），返回被裁剪的消息条数。只作用于本 turn 的待发
/// payload——工具结果本就不入持久 context，会话记录不受损。
fn slim_oversized_messages(messages: &mut [Message]) -> usize {
    let mut slimmed = 0;
    for m in messages.iter_mut() {
        if m.role == Role::System {
            continue;
        }
        let mut touched = false;
        for part in m.content.iter_mut() {
            if let ContentPart::Text { text } = part {
                let n = text.chars().count();
                if n > OVERSIZED_MSG_CHARS {
                    // 同样中段省略而不是截头：这条路径专门处理超大工具结果，
                    // 而结果的结论/note 往往在末尾（read 的结构摘要 footer、
                    // code_graph 的"怎么收窄"、批量读的 failed 清单）。
                    // 砍尾会让恢复重试拿到一份没有出口信息的残缺结果。
                    *text = compact_middle_out_chars(text, OVERSIZED_MSG_CHARS);
                    touched = true;
                }
            }
        }
        if touched {
            slimmed += 1;
        }
    }
    slimmed
}

/// 慢模型调用提示阈值：非流式 `chat` 超过该时长未返回时，向 UI 发
/// 一次「仍在等待」状态（见 run_turn 非流式分支）。默认 120s；
/// 测试可用 `LATTE_AGENT_SLOW_CALL_NOTICE_SECS` 调小。
fn slow_model_call_notice() -> Duration {
    std::env::var("LATTE_AGENT_SLOW_CALL_NOTICE_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(120))
}

/// Session 暂停门的 park 上限：超过该时长自动恢复继续。默认 `None`
/// （不限）——用户按 ⏸ 是显式意图，自动恢复会违背意图；无人值守
/// 场景（CI / 长跑）用 `LATTE_AGENT_PAUSE_MAX_PARK_SECS` 配上限。
fn max_pause_park() -> Option<Duration> {
    std::env::var("LATTE_AGENT_PAUSE_MAX_PARK_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .map(Duration::from_secs)
}

/// 空 completion 判定：无有效文本且无任何 tool_calls。只有文本为空
/// 但带 tool_calls 是正常的 tool round，不算空。空 completion 是厂商
/// 侧抖动（空响应入历史会导致后续请求 400，见 run_turn 尾部注释），
/// 调用方应把它当模型局部失败走 fallback，而不是当成功结果上交。
fn is_empty_completion(c: &Completion) -> bool {
    c.tool_calls.is_empty() && crate::controller::is_empty_output(&c.content)
}

/// 最终答复尾部带悬挂工具标记的判定。native function-calling 下，
/// `</parameter>`/`</function>`/`<tool_call>` 这类文本协议标记只会是
/// 模型泄漏的垃圾（glm-5.2 实锤：答复以 `</parameter> </function>`
/// 收尾、正文断在半句）。只查尾部 400 字节窗口——正文里合法的
/// XML/代码示例不触发；为兼容多字节字符，窗口起点对齐 char boundary。
fn has_dangling_tool_markup_tail(text: &str) -> bool {
    const MARKERS: [&str; 5] = [
        "</parameter",
        "</function",
        "<function",
        "<tool_call",
        "</tool_call",
    ];
    let mut idx = text.len().saturating_sub(400);
    while idx < text.len() && !text.is_char_boundary(idx) {
        idx += 1;
    }
    let tail = &text[idx..];
    MARKERS.iter().any(|m| tail.contains(m))
}

/// Whether `delegate` tool calls emitted in the *same* model response
/// may run concurrently. **Default off** — the agent's tool loop stays
/// strictly serial unless `LATTE_AGENT_DELEGATE_PARALLEL` is set to a
/// truthy value (`1` / `true` / `yes` / `on`, case-insensitive).
///
/// When on, a batch of ≥2 delegate calls fans out via `tokio::task::
/// JoinSet`; the actual specialist concurrency is still capped by the
/// `Semaphore` inside the delegate tool handler (see
/// `register_delegate_tool`, `LATTE_AGENT_DELEGATE_CONCURRENCY`). Non-
/// delegate tools (file writes, bash, …) always run serially so this
/// can't introduce write races.
fn delegate_parallel_enabled() -> bool {
    std::env::var("LATTE_AGENT_DELEGATE_PARALLEL")
        .ok()
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
}

/// 一条响应里允许多个 tool_call 的偏好，下发给 OpenAI 协议的
/// `parallel_tool_calls`（Anthropic 默认就允许并行，无需也不会下发）。
///
/// `LATTE_AGENT_PARALLEL_TOOL_CALLS` 三态：
/// - 未设 / 真值（`1`/`true`/`yes`/`on`）→ `Some(true)`，显式要求可并行；
/// - 假值（`0`/`false`/`no`/`off`）→ `Some(false)`，强制每轮最多一个工具调用
///   （排查"并发引入的问题"时用）；
/// - `omit` → `None`，字段完全不下发。留这一档是因为并非所有 OpenAI 兼容
///   端点都认它（litellm #22637：Bedrock Converse 在 Claude 4.5+ 上收到
///   这个字段直接失败）。遇到这种端点用 `omit` 退回供应商默认。
///
/// 注意：**开了也不保证模型会批量发**。实测 186 轮里 185 轮
/// 只发 1 个调用，而当时这个字段根本没下发、走的是供应商默认 `true` ——
/// 也就是说协议层一直是允许的，模型只是不肯。真正让它批量的是角色 prompt
/// 里的硬要求 + `read` 的 `paths` 批量入口。
fn parallel_tool_calls_pref() -> Option<bool> {
    match std::env::var("LATTE_AGENT_PARALLEL_TOOL_CALLS") {
        Err(_) => Some(true),
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            match v.as_str() {
                "omit" => None,
                "0" | "false" | "no" | "off" => Some(false),
                _ => Some(true),
            }
        }
    }
}

/// 可以在**同一响应内并发执行**的只读工具（短名，不含 namespace）。
///
/// 入选标准只有一条：**执行它不会改变任何进程外状态**。`read` 只读文件、
/// `code_graph` 只解析源码出符号表，两者都没有写路径，因此同一轮里的多个
/// 此类调用彼此没有顺序依赖，串行执行纯属浪费墙钟时间。
///
/// 反面例子（永远不进这张表）：`bash`（可以写文件 / 起进程）、`write` /
/// `edit`（改文件）、`delegate`（子 agent 会写文件，另有
/// `LATTE_AGENT_DELEGATE_PARALLEL` 单独管）。
///
/// 实测会话（`.latte/ui-sessions/…/programmer-*.jsonl`）的实测数据是
/// 这张表存在的理由：186 轮里 185 轮只发 1 个工具调用，185 次工具的真实
/// I/O 合计 18s，却付了 577s 模型往返。批量发起后如果还串行执行，收益会
/// 被工具侧的串行 I/O 吃掉一部分——这里把它并发掉。
///
/// **为什么不直接读 `Tool::concurrency_safe`**：那个字段的默认值是 `true`，
/// 而 MCP 服务器注册进来的工具、TOML 配出来的工具都可能没显式声明——信它
/// 就等于让一个会写文件的外部工具默认获得并发资格。并发这件事应当
/// deny-by-default，所以这里用显式白名单，`concurrency_safe` 只作为工具
/// 自描述的文档。要加 `search` / `find` 这类同样无副作用的工具，往这张表
/// 里加一项即可。
const READONLY_PARALLEL_TOOLS: &[&str] = &["read", "code_graph"];

/// `name` 是否属于 [`READONLY_PARALLEL_TOOLS`]（按短名比较，容忍
/// `namespace.read` 这类遗留全名）。
fn is_readonly_parallel_tool(name: &str) -> bool {
    READONLY_PARALLEL_TOOLS.contains(&short_tool_name(name))
}

/// 同一响应内的多个只读工具调用是否并发执行。**默认开**——只读工具没有
/// 写路径，并发不会引入竞态。设 `LATTE_AGENT_READONLY_PARALLEL` 为
/// `0`/`false`/`no`/`off` 可退回严格串行（排查问题时的逃生口）。
///
/// 与 `LATTE_AGENT_DELEGATE_PARALLEL`（默认关）的区别：delegate 会派生
/// 能写文件的子 agent，并发有真实风险，所以默认关；只读工具没有这个问题。
pub(crate) fn readonly_parallel_enabled() -> bool {
    std::env::var("LATTE_AGENT_READONLY_PARALLEL")
        .ok()
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "false" | "no" | "off")
        })
        .unwrap_or(true)
}

/// 只读并发的 in-flight 上限，`LATTE_AGENT_READONLY_PARALLEL_MAX` 覆盖，
/// 默认 8，钳在 `1..=32`。
///
/// 需要上限是因为 `code_graph` 会跑 tree-sitter 解析（CPU 密集），模型
/// 一轮批量发 30 个调用时全部同时起会把 CPU 打满、反而变慢；`read` 侧
/// 也避免一次撑爆 fd。上限为 1 等价于串行。
pub(crate) fn readonly_parallel_max() -> usize {
    std::env::var("LATTE_AGENT_READONLY_PARALLEL_MAX")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(8)
        .clamp(1, 32)
}

/// 把一轮的工具调用切成「可并发的只读段」与「屏障调用」。
///
/// 返回**连续**只读调用的区间 `[start, end)`，且只返回长度 ≥2 的区间
/// （长度 1 并发没有意义，走原串行路径少一次 spawn）。
///
/// **必须连续**是这里唯一的正确性要求：任何非只读调用（`bash`、`write`、
/// `edit`、`delegate`…）都是屏障，它前后的只读调用不会被合并到同一段。
/// 这样 `bash "echo x > f"` → `read f` 仍然严格按模型给出的顺序执行，
/// read 一定看得到那次写入。段内调用彼此只读，顺序无关，可以任意并发。
fn readonly_parallel_runs(calls: &[ParsedCall]) -> Vec<(usize, usize)> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < calls.len() {
        if !is_readonly_parallel_tool(&calls[i].name) {
            i += 1;
            continue;
        }
        let start = i;
        while i < calls.len() && is_readonly_parallel_tool(&calls[i].name) {
            i += 1;
        }
        if i - start >= 2 {
            runs.push((start, i));
        }
    }
    runs
}

/// 会**阻塞等人作答**的工具（短名）。
///
/// `ask` 在 workflow step / delegate 子代理里是阻塞语义，且刻意不设超时
/// （见 `controller::register_ask_tool` 的 `ORCHESTRATION_TOOL_TIMEOUT_SECS`
/// 注释）——没人点弹窗它就永远不返回。`ask_human` 直接把 session 暂停。
const HUMAN_BLOCKING_TOOLS: &[&str] = &["ask", "ask_human"];

/// **派发型**工具（短名）：把活交给子代理/子流水线跑完再返回。
///
/// 同一批里的多个派发调用彼此独立——引擎本来就允许它们并发执行
/// （`LATTE_AGENT_DELEGATE_PARALLEL`），所以也允许**重排**。这张表存在的
/// 唯一用途是给 [`human_blocking_last_order`] 画一条保守的适用边界：只有
/// 整批都是派发调用时才敢动顺序。
const DISPATCH_TOOLS: &[&str] = &["workflow", "delegate"];

fn is_human_blocking_tool(name: &str) -> bool {
    HUMAN_BLOCKING_TOOLS.contains(&short_tool_name(name))
}

fn is_dispatch_tool(name: &str) -> bool {
    DISPATCH_TOOLS.contains(&short_tool_name(name))
}

/// 一个 `workflow` 工具调用是否会**在中途停下来等人作答**。
///
/// 按名字加载 workflow 定义，看有没有哪一步能调 `ask`：`tools` 里列了、
/// `require_tools` / `require_tools_any` 要求了、或 `tool_call_limits` 给它
/// 定了次数上限——任一即算。加载不到（名字错、文件不存在）按"不阻塞"处理：
/// 这个判断只用来决定执行顺序，猜错的代价是退回现状，不能因为猜不出就报错。
///
/// 为什么不能只看工具名：manager 发的是 `workflow{name:"learn_loop"}`，
/// 阻塞发生在它内部第三步的 `ask` 上，调用点本身完全看不出来。
fn workflow_call_blocks_on_human(args_json: &str, cwd: Option<&std::path::Path>) -> bool {
    let name = match serde_json::from_str::<serde_json::Value>(args_json) {
        Ok(v) => match v.get("name").and_then(|n| n.as_str()) {
            Some(n) => n.to_string(),
            None => return false,
        },
        Err(_) => return false,
    };
    let cwd = match cwd {
        Some(c) => c.to_path_buf(),
        None => std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
    };
    let wf = match crate::workflow::load_workflow(&name, &cwd) {
        Ok(wf) => wf,
        Err(_) => return false,
    };
    wf.steps.iter().any(|s| {
        s.tools.iter().any(|t| is_human_blocking_tool(t))
            || s.require_tools.iter().any(|t| is_human_blocking_tool(t))
            || s.require_tools_any.iter().any(|t| is_human_blocking_tool(t))
            || s.tool_call_limits.keys().any(|t| is_human_blocking_tool(t))
    })
}

/// 把一批 tool_call 里**会阻塞等人**的调用排到最后，返回执行顺序；
/// `None` = 按模型给出的原顺序执行（现状，绝大多数批次走这条）。
///
/// ── 堵的是哪次事故 ────────────────────────────────────────────
/// 实测（jemalloc 会话 `ui-87189-1788067528852-0`，事件 220/221）：manager
/// 在同一条响应里发了两个 `workflow` 调用 —— `learn_loop`（学习，第三步
/// 弹窗出题）和 `implementation_plan`（用户点名的"拆分任务"）。工具循环
/// 严格按下标串行，`learn_loop` 跑到 quiz 步调 `ask` 就挂在那儿等人点弹窗
/// （无超时），于是 `implementation_plan` 连一次 `ToolExec` 都没有 ——
/// 用户明确要的产出被一个人机交互无限期押在队列里。advisor 当场就发现了
/// 「任务清单至今没有任何产出型分派动作」，但 manager 正卡在弹窗里收不到。
///
/// 换成"阻塞调用最后跑"，同一批里的其他派发照样先产出，弹窗照样弹，
/// 只是顺序反过来 —— 没有任何能力被牺牲。
///
/// ── 为什么重排是安全的 ────────────────────────────────────────
/// 同一批调用的参数在模型产出时**就已经全部定死**，后一个调用的入参不可能
/// 依赖前一个调用的结果（那需要另起一轮），所以批内不存在数据依赖。剩下的
/// 只有副作用可见性（`bash` 写文件 → `read` 读它），因此这里画了一条很保守
/// 的边界：**整批调用全是派发型（`workflow`/`delegate`）或阻塞型时才重排**，
/// 批里出现任何 `read`/`write`/`bash`/`edit` 就整批不动。派发之间彼此独立，
/// 引擎本来就允许它们并发（`LATTE_AGENT_DELEGATE_PARALLEL`）。
///
/// 记账（死循环探测、`tool_result` 回填）仍严格按**原始下标**顺序进行，
/// 模型看到的消息序列与不重排时逐字节一致；重排只改变"谁先真正执行"。
fn human_blocking_last_order(
    calls: &[ParsedCall],
    cwd: Option<&std::path::Path>,
) -> Option<Vec<usize>> {
    if calls.len() < 2 {
        return None;
    }
    let mut blocking: Vec<usize> = Vec::new();
    let mut rest: Vec<usize> = Vec::new();
    for (i, c) in calls.iter().enumerate() {
        let blocks = if is_human_blocking_tool(&c.name) {
            true
        } else if short_tool_name(&c.name) == "workflow" {
            workflow_call_blocks_on_human(&c.args, cwd)
        } else {
            false
        };
        if blocks {
            blocking.push(i);
        } else {
            // 保守边界：非阻塞侧只允许派发型调用被提到前面。
            if !is_dispatch_tool(&c.name) {
                return None;
            }
            rest.push(i);
        }
    }
    if blocking.is_empty() || rest.is_empty() {
        return None;
    }
    let order: Vec<usize> = rest.into_iter().chain(blocking).collect();
    // 已经是原顺序（阻塞调用本来就在最后）→ 不必走重排路径。
    if order.iter().copied().eq(0..calls.len()) {
        return None;
    }
    Some(order)
}

/// 「阻塞调用排最后」是否生效。**默认开**——它不牺牲任何能力，只改执行
/// 顺序。设 `LATTE_AGENT_BLOCKING_CALLS_LAST` 为 `0`/`false`/`no`/`off`
/// 退回严格按模型下标串行（排查问题时的逃生口）。
fn blocking_calls_last_enabled() -> bool {
    std::env::var("LATTE_AGENT_BLOCKING_CALLS_LAST")
        .ok()
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "false" | "no" | "off")
        })
        .unwrap_or(true)
}

/// Outcome of running a single tool call through the full pipeline
/// (parse → PreToolHook → execute+retry → PostToolHook). Returned by
/// [`run_one_tool_call`] so the caller can apply the `&mut self`
/// bookkeeping (loop detection, success count, message append) in the
/// original call order — even when calls executed concurrently.
struct OneCallResult {
    /// `tool_call_id` to pair the result back to its assistant call.
    id: String,
    /// Terminal result: `Ok(result_str)` or `Err((kind, detail))`.
    outcome: Result<String, (ToolCallErrorKind, String)>,
    /// `(name, args_json)` recorded per execute attempt, in order.
    /// Replayed into the shared `LoopDetector` by the caller so loop
    /// detection keeps working without sharing `&mut` state across
    /// concurrent tasks.
    loop_records: Vec<(String, String)>,
    /// `true` if at least one execute returned `Ok` (feeds
    /// `last_turn_tool_count`).
    executed_ok: bool,
}

fn tool_execution_metadata(
    cwd: Option<&std::path::Path>,
    current_session_file: Option<&std::path::Path>,
) -> Option<serde_json::Value> {
    if cwd.is_none() && current_session_file.is_none() {
        return None;
    }
    let mut metadata = serde_json::Map::new();
    if let Some(cwd) = cwd {
        metadata.insert("cwd".into(), cwd.display().to_string().into());
    }
    if let Some(path) = current_session_file {
        metadata.insert("current_session_file".into(), path.display().to_string().into());
    }
    Some(serde_json::Value::Object(metadata))
}

/// Run one tool call end-to-end without touching `&mut self`, so it can
/// be `tokio::spawn`ed for concurrent delegate execution. All inputs
/// are owned/`Arc` clones. Mirrors the serial inline pipeline in
/// [`AgentRunner::run_turn`]; the two must stay in sync.
#[allow(clippy::too_many_arguments)]
async fn run_one_tool_call(
    tm: Arc<dyn latte_rs_agent_tools::types::ToolManager>,
    hooks: Arc<crate::hooks::HookChain>,
    sink: Arc<dyn crate::trace::TraceSink>,
    retry_policy: Arc<dyn RetryPolicy>,
    cwd: Option<std::path::PathBuf>,
    current_session_file: Option<std::path::PathBuf>,
    meta: crate::trace::TraceMeta,
    tc: ParsedCall,
) -> OneCallResult {
    use crate::trace::{ToolStatus, TraceEvent};
    let resolved_name = tc.name.clone();
    let full_name = if tm.has(&resolved_name) {
        resolved_name.clone()
    } else {
        tm.get_tool_names()
            .into_iter()
            .find(|n| short_tool_name(n) == resolved_name.as_str())
            .unwrap_or_else(|| resolved_name.clone())
    };

    let mut attempt: u32 = 0;
    let max_attempts: u32 = 2;
    let mut final_outcome: Result<String, (ToolCallErrorKind, String)> = Err((
        ToolCallErrorKind::ToolNotFound { tried_aliases: vec![] },
        "init".into(),
    ));
    let mut loop_records: Vec<(String, String)> = Vec::new();
    let mut executed_ok = false;

    while attempt < max_attempts {
        attempt += 1;
        // 1. parse args
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
        let input = match &cwd {
            Some(c) => resolve_tool_input_against_cwd(input, c),
            None => input,
        };
        // 类型方言纠正：必须在 tm.execute 之前——工具管理器先跑
        // schema 校验，handler 里的宽松解析根本碰不到。
        let input = match tm.get_tool(&full_name) {
            Some(t) => coerce_tool_input_to_schema(input, &t.input_schema),
            None => input,
        };
        // 行号自愈（anchor）：edit 派发前用「本 agent 读过什么」的台账自动
        // 补 expect/tag，模型无需配合。工具层据此原子校验：行号漂了唯一
        // 重定位，定位不了就拒绝——绝不静默改错位置。见 `crate::edit_anchor`。
        let input = {
            let mut healed = input;
            let notes = crate::edit_anchor::heal_edit_input(&resolved_name, &mut healed);
            for note in notes {
                log_hook_fire(&note, crate::trace::HookPoint::PreTool, "anchor");
            }
            healed
        };

        // 2. PreToolHook
        let mut mutable_input = input;
        let pre_aborted: Option<String> = {
            let mut pre_ctx = crate::hooks::PreToolCtx {
                name: &resolved_name,
                args: &mut mutable_input,
            };
            let outcome = hooks.run_pre_tool(&mut pre_ctx, |hook_name, point, kind| {
                sink.emit(TraceEvent::HookFired {
                    meta: meta.refreshed(),
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
        let mut ctx =
            latte_rs_agent_tools::types::ToolExecutionContext::fresh(&resolved_name, 1);
        ctx.metadata = tool_execution_metadata(cwd.as_deref(), current_session_file.as_deref());
        let tool_start = Instant::now();
        let exec_result = tm.execute(&full_name, input.clone(), Some(ctx)).await;
        let tool_latency = tool_start.elapsed().as_millis() as u64;
        let args_json =
            serde_json::to_string(&input).unwrap_or_else(|_| tc.args.clone());
        loop_records.push((tc.name.clone(), args_json.clone()));

        match exec_result {
            Ok(result) => {
                executed_ok = true;
                // 记进 anchor 台账：read 存「模型看到的内容 + tag」，edit 只刷新
                // tag，write 作废。给后续 edit 的行号自愈提供基准。
                if let Ok(raw) = serde_json::to_string(&result) {
                    crate::edit_anchor::record_tool_result(&resolved_name, &raw);
                }
                // 5. PostToolHook
                let mut result_str = serde_json::to_string_pretty(&result)
                    .unwrap_or_else(|_| format!("{:?}", result));
                let post_aborted: Option<String> = {
                    let mut post_ctx = crate::hooks::PostToolCtx {
                        name: &resolved_name,
                        result: &mut result_str,
                    };
                    let outcome = hooks.run_post_tool(&mut post_ctx, |hook_name, point, kind| {
                        sink.emit(TraceEvent::HookFired {
                            meta: meta.refreshed(),
                            hook_name: hook_name.to_string(),
                            point,
                            outcome_kind: kind.to_string(),
                        });
                        log_hook_fire(hook_name, point, kind);
                    });
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
                sink.emit(TraceEvent::ToolExec {
                    meta: meta.refreshed(),
                    name: tc.name.clone(),
                    args_json: args_json.clone(),
                    latency_ms: tool_latency,
                    status: ToolStatus::Ok(
                        serde_json::to_string(&result).unwrap_or_default(),
                    ),
                });
                final_outcome = Ok(result_str);
                break;
            }
            Err(e) => {
                let kind = classify_tool_execution_error(&e);
                let detail = match &kind {
                    ToolCallErrorKind::ToolNotFound { .. } => {
                        tool_not_found_detail(&tc.name, e.to_string())
                    }
                    _ => e.to_string(),
                };
                sink.emit(TraceEvent::ToolExec {
                    meta: meta.refreshed(),
                    name: tc.name.clone(),
                    args_json: args_json.clone(),
                    latency_ms: tool_latency,
                    status: ToolStatus::Err(detail.clone()),
                });
                final_outcome = Err((kind, detail));
            }
        }

        // Retry decision (only reached on execute failure; Ok breaks above).
        let current_kind = match &final_outcome {
            Ok(_) => break,
            Err((k, _)) => k.clone(),
        };
        let retrying = retry_policy.retryable(&current_kind) && attempt < max_attempts;
        sink.emit(TraceEvent::ToolRetry {
            meta: meta.refreshed(),
            name: tc.name.clone(),
            attempt,
            kind: current_kind.label().to_string(),
            reason: current_kind.to_string(),
            recovered: false,
        });
        if retrying {
            continue;
        } else {
            break;
        }
    }

    OneCallResult {
        id: tc.id.clone(),
        outcome: final_outcome,
        loop_records,
        executed_ok,
    }
}


impl AgentRunner {
    /// Create a new runner for an agent (no tools).
    pub fn new(agent: Agent) -> Self {
        Self {
            agent,
            context: ConversationContext::default(),
            tool_manager: None,
            total_usage: TokenUsage::default(),
            sink: Arc::new(crate::trace::NullSink),
            hooks: Arc::new(crate::hooks::HookChain::empty()),
            retry_policy: Arc::new(DefaultRetryPolicy),
            role_id: String::new(),
            session_id: String::new(),
            advisor_hints: None,
            cwd: None,
            current_session_file: None,
            last_turn_tool_count: 0,
            last_turn_tool_summaries: Vec::new(),
            gate_config: None,
            pause_gate: None,
            agent_pause_gate: None,
            stream_mode: None,
            model_source: None,
            deadline: None,
        }
    }
    /// 构造带工具的 runner。
    ///
    /// 曾有第三个参数 `max_tool_rounds`（轮次上限）。deadline-only 重构
    /// 把硬上限拿掉后，该值只是被存进字段、再没人读，是纯粹的死参数
    /// （CLI 三处都传 0——按旧文档那意思是「关闭工具调用」，而工具照常
    /// 工作，正好印证它已被忽略）。现在循环的自动刹车只有
    /// [`LoopDetector`]（同参数死循环），超时与叫停交给
    /// `set_deadline` / 人工 ⏸ / advisor。
    pub fn new_with_tools(agent: Agent, tool_manager: Arc<dyn latte_rs_agent_tools::types::ToolManager>) -> Self {
        Self {
            agent,
            context: ConversationContext::default(),
            tool_manager: Some(tool_manager),
            total_usage: TokenUsage::default(),
            sink: Arc::new(crate::trace::NullSink),
            hooks: Arc::new(crate::hooks::HookChain::empty()),
            retry_policy: Arc::new(DefaultRetryPolicy),
            role_id: String::new(),
            session_id: String::new(),
            advisor_hints: None,
            cwd: None,
            current_session_file: None,
            last_turn_tool_count: 0,
            last_turn_tool_summaries: Vec::new(),
            gate_config: None,
            pause_gate: None,
            agent_pause_gate: None,
            stream_mode: None,
            model_source: None,
            deadline: None,
        }
    }
    pub fn with_context(agent: Agent, context: ConversationContext) -> Self {
        Self {
            agent,
            context,
            tool_manager: None,
            total_usage: TokenUsage::default(),
            sink: Arc::new(crate::trace::NullSink),
            hooks: Arc::new(crate::hooks::HookChain::empty()),
            retry_policy: Arc::new(DefaultRetryPolicy),
            role_id: "default".to_string(),
            session_id: String::new(),
            advisor_hints: None,
            cwd: None,
            current_session_file: None,
            last_turn_tool_count: 0,
            last_turn_tool_summaries: Vec::new(),
            gate_config: None,
            pause_gate: None,
            agent_pause_gate: None,
            stream_mode: None,
            model_source: None,
            deadline: None,
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

    /// Attach the advisor v3 pause gate（intervene 暂停门）. The same
    /// handle is held by the `ChatController`（monitor 经
    /// `request_pause()` 置位；`submit_input` resolve）；本 runner
    /// 在每个 tool-round 边界 `wait_if_requested()`。只应装到
    pub fn with_pause_gate(mut self, gate: crate::advisor_monitor::AdvisorPauseGate) -> Self {
        self.pause_gate = Some(gate);
        self
    }
    /// Attach session-level pause gate（用户按 ⏸ 触发），与 [`with_pause_gate`]
    /// （advisor monitor 的细粒度干预门）正交共存。
    /// Clone `Arc<AgentPauseGate>`：同一 session 的 main driver + subagent
    /// + tool runner 共享同一 gate —— 用户按暂停时全部一起冻结。
    pub fn with_agent_pause_gate(
        mut self,
        gate: std::sync::Arc<crate::pause_gate::AgentPauseGate>,
    ) -> Self {
        self.agent_pause_gate = Some(gate);
        self
    }

    /// 模型全链不可用（`ModelsUnavailable`）时的自动暂停。
    ///
    /// 挂了 session 暂停门 → engage 门（带原因，controller 的
    /// on_change listener 会广播 `ChatEvent::Paused`，UI 弹出
    /// 「已暂停 + ▶ 继续」）。恢复后返回 true，调用方重试模型调用。
    /// 无门 → false，调用方原样抛错。
    ///
    /// 语义对齐用户手动 ⏸：in-flight 的本次 model call 已经失败
    /// 落定，重试从下一次调用开始，不打断任何进行中的流。
    ///
    /// ## 退避自动重试
    ///
    /// `attempt`（0-based）是本次模型调用已经因同一原因暂停过几次，
    /// 由调用方的重试循环维护。
    ///
    /// 此前这里无条件 `wait_until_resumed(None)` —— 无上限死等人点
    /// ▶。但事件文本写的是「9s 后可自动重试」，`ModelsUnavailable`
    /// 也带着 `next_retry_in`，两边都在承诺自动重试，代码却没做。
    /// 实测会话：22:06:27 三家 provider 同时连接失败
    /// （本机网络抖动），reviewer 就地 park 到死，84 分钟后整条
    /// workflow 被预算判超支中止。一次 9 秒的抖动毁掉一次完整运行。
    ///
    /// 现在：厂商 cooldown（`next_retry_in`）有值 → 按退避自动恢复重
    /// 试，人仍可随时点 ▶ 抢先继续；退避次数用尽、或 cooldown 为
    /// `None`（失败不可重试，如鉴权错误——重试一万次也是 401）→ 退回
    /// 无上限 park 等人工。
    async fn pause_wait_model_unavailable(&self, e: &AgentError, attempt: u32) -> bool {
        let Some(gate) = self.agent_pause_gate.clone() else {
            return false;
        };
        let summary = model_failures_summary(e);
        let cooldown = match e {
            AgentError::ModelsUnavailable { next_retry_in, .. } => *next_retry_in,
            _ => None,
        };
        let max_retries = auto_pause_max_retries();
        // 自动重试的两个前提：厂商给了 cooldown（说明失败可重试），
        // 且退避次数还没用尽。
        let auto_after = cooldown
            .filter(|_| attempt < max_retries)
            .map(|d| auto_pause_backoff(attempt, d));

        let hint = if failures_include_context_overflow(e) {
            "检测到上下文超出模型窗口——原样重试必败。点 ▶ 继续将自动裁剪本轮待发消息中的超长内容（多为工具结果）后重试；会话记录不受影响"
                .to_string()
        } else if let Some(d) = auto_after {
            format!(
                "{}s 后自动重试（第 {}/{} 次），也可点 ▶ 立即继续",
                d.as_secs(),
                attempt + 1,
                max_retries
            )
        } else if cooldown.is_none() {
            "失败不可自动重试（多为鉴权/配置问题，非限流）——请修正后点 ▶ 继续".to_string()
        } else {
            format!("已连续自动重试 {max_retries} 次仍不可用，转人工——点 ▶ 继续会再试一次")
        };
        gate.pause_with_reason(format!("模型不可用（{summary}），已自动暂停——{hint}"));
        self.sink.emit(crate::trace::TraceEvent::SessionPaused {
            meta: crate::trace::TraceMeta::now(0, &self.role_id, ""),
            task_id: String::new(),
            reason: format!("models unavailable (attempt {attempt}): {summary}"),
            turn: 0,
        });

        let resumed = match auto_after {
            // 退避窗口内人点了 ▶ 就走人工路径；没人管则超时自恢复。
            // 注意 gate 必须由我们显式 resume：wait 超时不会解除门，
            // 而门还 engaged 的话后续 park 点会再次卡住。
            Some(d) => {
                match tokio::time::timeout(d, gate.wait_until_resumed(None)).await {
                    Ok(r) => matches!(r, crate::pause_gate::WaitResult::Ok),
                    Err(_) => {
                        gate.resume();
                        true
                    }
                }
            }
            None => matches!(
                gate.wait_until_resumed(None).await,
                crate::pause_gate::WaitResult::Ok
            ),
        };
        self.sink.emit(crate::trace::TraceEvent::SessionResumed {
            meta: crate::trace::TraceMeta::now(0, &self.role_id, ""),
            task_id: String::new(),
            turn: 0,
        });
        resumed
    }

    /// Session 门 park 期间的"仍在等待"提醒间隔。
    const PARK_NOTICE_INTERVAL: Duration = Duration::from_secs(120);

    /// 统一的 session 暂停 park：进出各发一次 trace 事件，park 期间
    /// 定期 warn，可选上限防死等。
    ///
    /// 动机（实测会话）：session 门的 3 个 park 点
    /// （turn 入口 / model call 前 / tool 启动前）此前都直接
    /// `wait_until_resumed(None)`，park 期间 trace 里**一个事件都没有**。
    /// 那条会话 67 分钟里有 37 分钟是完整空洞（13:03:33 → 13:40:49 所有
    /// 角色同时静默），事后无法区分"用户暂停 / 厂商限流 / 死锁"——排查
    /// 只能靠猜。
    ///
    /// `site` 写进 reason，用来区分停在哪个边界。
    /// `LATTE_AGENT_PAUSE_MAX_PARK_SECS`（默认 0 = 不限）给一个防死等
    /// 上限：用户按 ⏸ 是显式意图，默认不自动恢复；无人值守场景可用
    /// 这个 env 配上限。
    async fn park_if_paused(&self, site: &str) {
        let Some(gate) = self.agent_pause_gate.clone() else {
            return;
        };
        if !gate.is_paused() {
            return;
        }
        let detail = gate.pause_reason().unwrap_or_default();
        let reason = if detail.is_empty() {
            format!("pause gate parked at {site}")
        } else {
            format!("pause gate parked at {site}: {detail}")
        };
        self.sink.emit(crate::trace::TraceEvent::SessionPaused {
            meta: crate::trace::TraceMeta::now(0, &self.role_id, self.session_id.clone()),
            task_id: String::new(),
            reason,
            turn: 0,
        });
        let started = Instant::now();
        let cap = max_pause_park();
        loop {
            match tokio::time::timeout(Self::PARK_NOTICE_INTERVAL, gate.wait_until_resumed(None))
                .await
            {
                Ok(_) => break,
                Err(_) => {
                    let waited = started.elapsed().as_secs();
                    tracing::warn!(
                        "session pause gate: {} 在 {site} 已 park {waited}s，仍在等待恢复",
                        self.role_id
                    );
                    if let Some(cap) = cap {
                        if started.elapsed() >= cap {
                            gate.resume();
                            tracing::warn!(
                                "session pause gate: 超过上限 {}s，自动恢复继续",
                                cap.as_secs()
                            );
                            break;
                        }
                    }
                }
            }
        }
        self.sink.emit(crate::trace::TraceEvent::SessionResumed {
            meta: crate::trace::TraceMeta::now(0, &self.role_id, self.session_id.clone()),
            task_id: String::new(),
            turn: 0,
        });
    }

    /// 把一次 [`run_one_tool_call`] 的结果落到 `&mut self` 与消息历史上。
    ///
    /// 并发执行路径（delegate 扇出、只读工具扇出）都必须**按模型给出的
    /// 原始调用顺序**逐个调用它，这样死循环探测、成功计数、`tool_result`
    /// 追加、`PermanentExec` 连击熔断的顺序语义与串行路径完全一致——并发
    /// 只改变「工具什么时候真正执行」，不改变「模型看到什么」。
    ///
    /// - `tool_name`：模型给出的调用名（`ParsedCall::name`）。不从
    ///   `loop_records` 里取，因为参数 JSON 非法时压根没进 execute，
    ///   `loop_records` 会是空的。
    /// - `permanent_streak`：`(工具名, 连续被确定性校验拒绝的次数)`，跨
    ///   调用累积，语义见串行路径。
    #[allow(clippy::too_many_arguments)]
    fn apply_call_result(
        &mut self,
        r: OneCallResult,
        tool_name: &str,
        messages: &mut Vec<Message>,
        loop_detector: &mut LoopDetector,
        permanent_streak: &mut (String, usize),
        final_response: &str,
        last_substantive_response: &str,
    ) -> Result<(), AgentError> {
        // 1. 死循环探测：回放并发任务里攒下的 (name, args) 记录。
        for (name, args_json) in &r.loop_records {
            if let LoopDecision::Break(reason) = loop_detector.record(name, args_json) {
                return Err(AgentError::ToolLoopDetected {
                    tool: name.clone(),
                    reason,
                    partial: if last_substantive_response.is_empty() {
                        final_response.to_string()
                    } else {
                        last_substantive_response.to_string()
                    },
                });
            }
        }
        // 2. 成功计数 + 工具摘要（供 advisor 审查用）。
        if r.executed_ok {
            self.last_turn_tool_count += 1;
            if let (Some((name, args)), Ok(result_str)) = (r.loop_records.first(), &r.outcome) {
                self.last_turn_tool_summaries.push(format!(
                    "{} {} → {}",
                    name,
                    truncate_tool_summary(args, 160),
                    truncate_tool_summary(result_str, 240),
                ));
            }
        }
        // 3. 回填 tool_result（成功失败都必须回填，否则 native 协议下
        //    下一轮请求 400，见串行路径的长注释）。
        match r.outcome {
            Ok(result_str) => {
                if permanent_streak.0 == tool_name {
                    *permanent_streak = (String::new(), 0);
                }
                messages.push(Message::tool_result(r.id, result_str));
            }
            Err((kind, detail)) => {
                // 中段省略而非截头：错误分类在头、修复指引在尾，两头都要留。
                let compacted = compact_middle_out(&detail, MAX_TOOL_ERROR_BYTES);
                messages.push(Message::tool_result(r.id, compacted));
                if matches!(kind, ToolCallErrorKind::PermanentExec { .. }) {
                    if permanent_streak.0 == tool_name {
                        permanent_streak.1 += 1;
                    } else {
                        *permanent_streak = (tool_name.to_string(), 1);
                    }
                    if permanent_streak.1 >= PERMANENT_BREAK_AT {
                        return Err(AgentError::ToolLoopDetected {
                            tool: tool_name.to_string(),
                            reason: format!(
                                "'{}' 连续 {} 次因输入校验被拒（每次参数都不同），\
                                 模型无法自行修正，停止重试",
                                tool_name, permanent_streak.1
                            ),
                            partial: if last_substantive_response.is_empty() {
                                final_response.to_string()
                            } else {
                                last_substantive_response.to_string()
                            },
                        });
                    }
                    if permanent_streak.1 == PERMANENT_NUDGE_AT {
                        messages.push(Message::user(format!(
                            "⚠️ `{}` 已连续 {} 次因输入校验失败被拒。不要再用同一个\
                             工具反复试：要么换一条路径完成任务，要么把当前进展和\
                             卡点直接讲给用户。",
                            tool_name, permanent_streak.1
                        )));
                    }
                } else if permanent_streak.0 == tool_name {
                    *permanent_streak = (String::new(), 0);
                }
            }
        }
        Ok(())
    }

    /// Advisor 暂停门的 park，同样补 trace 事件。门自带 600s 自动恢复
    /// （见 [`crate::advisor_monitor::AdvisorPauseGate`]），这里只负责
    /// 可观测性。
    async fn park_if_advisor_requested(&self) {
        let Some(gate) = self.pause_gate.clone() else {
            return;
        };
        if !gate.is_requested() {
            return;
        }
        self.sink.emit(crate::trace::TraceEvent::SessionPaused {
            meta: crate::trace::TraceMeta::now(0, &self.role_id, self.session_id.clone()),
            task_id: String::new(),
            reason: "advisor pause gate: 等待用户拍板".to_string(),
            turn: 0,
        });
        gate.wait_if_requested().await;
        self.sink.emit(crate::trace::TraceEvent::SessionResumed {
            meta: crate::trace::TraceMeta::now(0, &self.role_id, self.session_id.clone()),
            task_id: String::new(),
            turn: 0,
        });
    }

    /// 设置流式模式开关（运行时可切换）。
    ///
    /// `stream_mode.load(true)` 时 `run_turn` 走 `Agent::chat_stream`，
    /// 逐 Delta 发 `TraceEvent::ModelDelta`，UI 逐 token 渲染。
    /// `load(false)` 或未设置时走非流式 `Agent::chat`（默认）。
    /// 用 `Arc<AtomicBool>` 让 UI/session 层实时切换无需重建 runner。
    pub fn with_stream_mode(mut self, stream_mode: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.stream_mode = Some(stream_mode);
        self
    }

    /// 挂上模型热更新源。`chain_ids` 应为构建本 runner 时使用的
    /// `role.model_chain`（解析参数原样保留，重载时按新配置重新解析）。
    pub fn with_model_hot_reload(
        mut self,
        resolver: std::sync::Arc<crate::model_resolver::ModelResolver>,
        tier: crate::model_resolver::ModelTier,
        chain_ids: Vec<String>,
    ) -> Self {
        let applied_generation = resolver.generation();
        self.model_source = Some(RunnerModelSource {
            resolver,
            tier,
            chain_ids,
            applied_generation,
        });
        self
    }

    /// 配置代际变了就重新解析并替换 model chain。调用点：`run_turn`
    /// 入口、「模型不可用暂停 → 用户 ▶ 恢复」的重试前——后者正是
    /// 「模型挂了 → 用户在 UI 改配置 → 点继续」的救命路径。解析失败
    /// 或空链时保留旧链（配置可能处于中间态），代际照样推进避免每个
    /// turn 重复解析。
    fn maybe_reload_models(&mut self) {
        let Some(src) = self.model_source.as_ref() else {
            return;
        };
        let gen = src.resolver.generation();
        if gen == src.applied_generation {
            return;
        }
        let resolver = src.resolver.clone();
        // 角色编辑器保存的模型指派优先于 runner 构建时的快照——
        // 「角色模型修改 → 保存 → 已加载 session 热生效」走这里。
        let (chain_ids, tier) = match resolver.role_model_assignment(&self.role_id) {
            Some((chain, t)) => (
                if chain.is_empty() {
                    src.chain_ids.clone()
                } else {
                    chain
                },
                t.unwrap_or(src.tier),
            ),
            None => (src.chain_ids.clone(), src.tier),
        };
        match resolver.resolve_chain(&self.role_id, tier, &chain_ids) {
            Ok(models) if !models.is_empty() => {
                let ids: Vec<String> = models.iter().map(|m| m.id.clone()).collect();
                match self.agent.reload_models(models) {
                    Ok(()) => {
                        tracing::info!(
                            role = %self.role_id,
                            chain = ?ids,
                            "model config changed (gen {gen}), model chain reloaded"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(role = %self.role_id, "model chain reload failed: {e}; keeping old chain");
                    }
                }
            }
            _ => {
                tracing::warn!(
                    role = %self.role_id,
                    "model config changed (gen {gen}) but re-resolve yielded no usable model; keeping old chain"
                );
            }
        }
        if let Some(src) = self.model_source.as_mut() {
            src.applied_generation = gen;
        }
    }

    /// 当前是否处于流式模式。`stream_mode` 未设置或 `load(false)` 时返回 false。
    fn is_stream_mode(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.stream_mode.as_ref().map_or(false, |m| m.load(Ordering::SeqCst))
    }

    /// 流上报错时决定「冷却当前模型 + 重开流」还是「把错误上交」。
    ///
    /// `Some(_)` = 决定重开（调用方 `continue 'stream_attempt`）；
    /// `None` = 不能重开，调用方把错误上交。
    ///
    /// 三个不重开的条件：
    /// 1. **已经有内容吐给用户**（`streamed_any`）—— 重开会把这段重复输出，
    ///    比报一个错更糟；
    /// 2. 错误不可通过换模型规避（`cooldown_for_error` 返回 `None`，即本地
    ///    配置/序列化类错误）；
    /// 3. 已经换过 `max` 次 —— 防止「链上每个模型都在连接后立刻报错」时打转。
    fn try_stream_fallback(
        &self,
        err: &AiError,
        streamed_any: bool,
        fallbacks: &mut u8,
        max: u8,
        meta: &crate::trace::TraceMeta,
    ) -> Option<Option<String>> {
        if streamed_any || *fallbacks >= max {
            return None;
        }
        let cd = cooldown_for_error(err)?;
        let from = self.agent.cooldown_serving_model(cd)?;
        let to = self.agent.next_available_model_after(&from);
        *fallbacks += 1;
        self.sink.emit(crate::trace::TraceEvent::FallbackTriggered {
            meta: meta.refreshed(),
            from_model: from,
            to_model: to.clone(),
            reason: crate::trace::FallbackReason::RetryableFailure,
            cause_kind: Some(crate::trace::ModelErrorKind::from(err)),
        });
        Some(to)
    }

    /// 测试用：是否挂了模型热更新源（workflow / delegate 路径的接线
    /// 回归测试据此断言，不漏挂）。
    #[cfg(test)]
    pub(crate) fn has_model_hot_reload(&self) -> bool {
        self.model_source.is_some()
    }
    /// 测试用：是否挂了真实 trace sink（默认的 `NullSink` 不算）。
    ///
    /// 同上的接线回归用途：`build_runner` 的无工具分支曾把 `runner_sink`
    /// 造好却忘了 `with_sink`，编译器只报 `unused variable`，而后果是该类
    /// 角色的 trace 既不落子会话日志、也不广播给 UI。
    #[cfg(test)]
    pub(crate) fn has_trace_sink(&self) -> bool {
        !self.sink.is_null()
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

    /// Set the absolute wall-clock deadline for the tool loop (对齐 oh-my-pi
    /// `config.deadline`). 超时时循环优雅退出、返回 partial 产出。
    pub fn set_deadline(&mut self, deadline: std::time::Instant) {
        self.deadline = Some(deadline);
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
    /// 读取本轮工具调用摘要并清空内部缓冲区。
    /// 供 workflow.rs 的 delegate-return 审查收集工具执行证据，
    /// 让 advisor 能看到 ask 工具返回的用户答案，而不是仅凭计数做判断。
    pub fn take_last_turn_tool_summary(&mut self) -> String {
        if self.last_turn_tool_summaries.is_empty() {
            return format!("[本 step 工具调用数: {}]", self.last_turn_tool_count);
        }
        let out = self.last_turn_tool_summaries.join("\n");
        self.last_turn_tool_summaries.clear();
        out
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
        // Session-level 暂停：用户按 ⏸ 时 run_turn 入口 park。
        self.park_if_paused("turn entry").await;
        // 模型配置热更新：turn 边界比对配置代际，变了就重建 model chain。
        self.maybe_reload_models();
        // 注：注入队列（`.latte/inject/<role>.txt`）**不在这里** drain。
        // 真正生效的是外层循环——CLI HIL（`chat.rs`）与 controller 多角色
        // （`controller.rs`）在调用 run_turn **之前**就读文件、
        // `append_to_role` 写进角色历史、并 `remove_file`。等执行到这里
        // 文件已不存在。`AgentRunner::drain_inject_queue` 是第三份重复
        // 实现，从未接线也不该接（见其文档注释）。
        // Advisor monitor: drain pending hints into the context
        // *before* the working message list is built below, so the
        // first model call of this turn already sees them.
        self.drain_advisor_hints();

        // 一旦 turn 已开始，就立即把调用方输入提交到持久 context，而不
        // 等模型成功返回。`run_turn_cancellable` 通过 drop in-flight future
        // 取消；模型/API/hook 也可能在下面任一点返回 Err。若在函数尾才
        // push，这些路径会让 UI 已显示、事件已落盘的用户消息从下一轮
        // prompt 消失。先提交同时保证 future 被取消时输入仍然保留。
        //
        // advisor hint 必须先于本轮输入（保持原有 prompt 顺序）；成功
        // 路径末尾不再重复 push。`run_turn_gated` 的重试传空 slice，因而
        // 同一用户输入也只提交一次。
        for msg in new_messages {
            self.context.push(msg.clone());
        }

        // Pre-persistence gate: 每次 turn 开始清零 tool 计数器，
        // run_turn 内部每次成功执行一个工具就 +1；run_turn 结束时
        // run_turn_gated 据此跑 D6 ToolCallEcho 检查。
        self.last_turn_tool_count = 0;
        self.last_turn_tool_summaries.clear();
        use crate::trace::{ParsedCall, ParseDiag, ToolStatus, TraceEvent, TraceMeta};
        let turn_start = Instant::now();
        // wall-clock 起点：与 `turn_start`（单调）配对，用来暴露进程被
        // 挂起（系统睡眠）的时长——见 TurnEnd 发射处。
        let turn_start_wall = std::time::SystemTime::now();
        let meta = TraceMeta::now(0, self.role_id.clone(), self.session_id.clone());
        let default_vars = serde_json::json!({});
        let vars = system_vars.unwrap_or(&default_vars);

        let sys_msg = self.agent.system_message(vars)?;
        let system_rendered = sys_msg.as_text();
        let mut messages: Vec<Message> = Vec::new();
        messages.push(sys_msg);
        // 本轮输入已在上方提交进 context；这里直接复制完整上下文，不能
        // 再 extend new_messages，否则发给模型的 user message 会重复。
        messages.extend_from_slice(self.context.messages());

        // Run PreCall hooks before the prompt is built — this lets hooks
        // like `redact_pii` mutate the outgoing message list in-place so
        // the redacted version is what the model actually sees. Each
        // fired hook emits a HookFired TraceEvent so the trace stream
        // records the redaction (and tests can assert on it).
        {
            let mut pre_call_ctx = crate::hooks::PreCallCtx { messages: &mut messages };
            let outcome = self.hooks.run_pre_call(&mut pre_call_ctx, |hook_name, point, kind| {
                self.sink.emit(TraceEvent::HookFired {
                    meta: meta.refreshed(),
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
        // 模型 id 一律在**发出请求之后**经 `self.agent.served_model_id()` 取，
        // 不在这里预先算一个链首值。原来这里算 `model_chain.first()` 供
        // `ModelCall` / `ModelCallSlow` 使用，于是 fallback 换过模型之后日志
        // 仍标着链首 —— 实测一次 `glm-5.3 → MiniMax-M3` 回退后三条
        // `ModelCall` 全标 `glm-5.3`，把"换没换成"从日志里抹掉了。
        let est_input_tokens = (messages.iter().map(|m| m.as_text().len()).sum::<usize>() / 4) as u32;
        self.sink.emit(TraceEvent::PromptBuilt {
            meta: meta.refreshed(),
            system_rendered,
            // 保持原有语义：history_len 不含本轮 new_messages。
            history_len: self.context.messages().len().saturating_sub(n_new),
            user_input: user_input.clone(),
            est_input_tokens,
        });

        // 工具轮次没有硬性上限（对齐 oh-my-pi deadline-only 模式）。
        // 循环终止靠：
        //   (1) 模型返回无 tool_calls → break；
        //   (2) `self.deadline`（`set_deadline` 设置，仅 delegate /
        //       workflow step 两条路径会设；交互式 chat 不设，超时按设计
        //       交给人工 ⏸ 或 advisor 叫停）；
        //   (3) LoopDetector —— 同一工具 + 逐字节相同参数连续
        //       LOOP_STREAK_THRESHOLD 次即熔断，是**唯一始终生效**的自动
        //       刹车；
        //   (4) permanent_streak —— 同一工具连续被确定性校验拒绝
        //       （参数每次都变，LoopDetector 抓不到）。
        //
        // 曾经的第 5 条是 `max_tool_rounds` 硬上限。deadline-only 重构把它
        // 拿掉后，那个字段只写不读、setter 零调用、`MaxToolRoundsExceeded`
        // 再没人构造，已连同参数一起删除。若之后要加回 soft warning，
        // 重新引入即可，别再留一个不生效的字段冒充刹车。

        let mut final_response = String::new();
        // 循环被熔断中止时要交出去的 partial 产出。不能直接用
        // `final_response`：它每轮被覆盖，而最后一轮常常是「只有 tool_call、
        // 没有正文」的一轮，那样 partial 会是空串，前面几十轮写好的正文
        // 白丢。这里只在正文非空时更新，保留「最后一段有实质内容的回复」。
        let mut last_substantive_response = String::new();
        let mut total_input: u32 = 0;
        let mut total_output: u32 = 0;
        let mut total_thinking: u32 = 0;
        // 悬挂工具标记的卫生重试计数（每 turn 至多 1 次）。
        let mut markup_retried: u8 = 0;
        // 正文撞长度上限后的重试次数（同 markup_retried：只重试一次，
        // 避免"每次都被截断"变成死循环）。
        let mut length_retried: u8 = 0;

        // 死循环探测器：**整个 turn 共用一个**，streak 跨 round 累积。
        //
        // 这是 deadline-only 模式下唯一的自动刹车。轮次上限
        // 已随死代码删除、`deadline` 只在
        // delegate / workflow step 两条路径上设置，交互式 chat 不设——
        // 按设计，超时与叫停交给人工 ⏸ 或 advisor 角色。所以「模型连续
        // 拿同一条命令+同一份参数反复调工具」必须由这里兜住，别处没有
        // 兜底了。
        //
        // 为什么必须放在循环外：模型通常一轮只发一个工具调用，探测器若
        // 每轮新建，streak 永远是 1，跨轮死循环一次都测不到——这正是
        // 之前的状态（构造语句原本在 `loop` 内第一行）。
        //
        // 与 2026-07-10 那次「manager 连着 3 轮 delegate 同一任务被误杀」
        // 不冲突：判定键是 (工具名, **完全相同**的 args_json)，合法重试
        // 每次都会改参数（重写任务描述），一改就 `streak = 1` 重新计数；
        // 且阈值当时是 3，现在是 `LOOP_STREAK_THRESHOLD = 5`。真正连续
        // 5 次逐字节相同的调用，就是死循环而不是进展。
        //
        // 注：同一 response 内的重复调用由 `dedupe_tool_calls` 先折叠；
        // 跨 round 的「manager 反复改派同类任务」（参数每次都变）仍归
        // supervisor 的 `dead_loop_window = 3` 管，两者互补不重叠。
        let mut loop_detector = LoopDetector::default();
        // 跨 round 的「同一工具连续确定性失败」熔断计数：
        // (工具名, 连续 PermanentExec 次数)。`LoopDetector` 只认参数
        // **完全相同**的重复调用，而模型每次都会改一点参数再撞同一个
        // 校验（实测事故：manager 连着 3 轮调 plan，每轮重写
        // 8 个任务 40+ 条 paths，前两次分别撞 paths 重叠和 paths 类型
        // 错误，每轮烧 100~150s 模型时间），所以需要一个只看
        // (工具, 失败类别) 的计数器。
        let mut permanent_streak: (String, usize) = (String::new(), 0);
        let mut round: usize = 0;
        loop {
            // Deadline 检查（对齐 oh-my-pi `isDeadlineExceeded`）：
            // 每个 model call 前检查，超时时优雅退出、返回已有 partial。
            if let Some(dl) = self.deadline {
                if std::time::Instant::now() >= dl {
                    tracing::info!(round, "deadline exceeded, returning partial output");
                    if !last_substantive_response.is_empty() {
                        final_response = last_substantive_response.clone();
                    }
                    break;
                }
            }

            // Advisor monitor (tool-round boundary): hints that landed
            // while the previous round's tools were executing are
            // appended to the working message list, so the model call
            // below — mid tool loop — already carries the correction.
            // (`drain_advisor_hints` also records them in the context;
            // round 0 is covered by the turn-start drain above.)
            if round > 0 {
                // Advisor v3 pause gate：monitor 判 Intervene 置位后，
                // 本 runner 在此挂起，直到用户拍板（下一条用户输入
                // resolve）或 gate 超时自动恢复。恢复后照常 drain
                // hint——advisor 的纠正提示与用户的拍板决定一起进入
                // 下一次模型调用。
                if self.pause_gate.is_some() {
                    self.park_if_advisor_requested().await;
                }
                // Session-level 暂停（用户按 ⏸）：每个 model call 之前
                // park —— 与 entry + tool-exec 边界一起，构成"任何
                // 状态都能暂停"的完整覆盖。in-flight model stream
                // 跑完，下个 round 才停。
                self.park_if_paused("model call boundary").await;
                for hint in self.drain_advisor_hints() {
                    messages.push(Message::user(format!("🦉 advisor 监察：\n{hint}")));
                }
            }
            // 2a. Emit ModelCall + ModelRawOut after agent.chat()
            //
            // 发请求前主动压缩陈旧工具结果：工具循环每轮都重传全部历史，
            // 第 N 轮要把前 N-1 轮的工具结果原文再发一遍。实测一个
            // reviewer 子会话 70 次调用、返回内容合计 47K token，那个 turn
            // 却烧了 1.79M 输入 token（38 倍）。只压文本不删消息，保住
            // tool_call ↔ tool_result 的 id 配对。
            if round > 0 && context_compact_enabled() {
                let n = compact_stale_tool_results(&mut messages, context_keep_recent());
                if n > 0 {
                    tracing::debug!(
                        round,
                        compacted = n,
                        "compacted stale tool results before model call"
                    );
                }
            }
            let chat_start = Instant::now();
            // native function-calling：tool schema 经 GenerateParams.tools 下发。
            // 文本 `<tool_call>` 协议已移除，不再有降级路径。
            // tool_choice 取自 agent.params（默认 Auto，可经 with_tool_choice 覆盖）。
            let chat_params: Option<GenerateParams> = self.tool_manager.as_ref().map(|tm| {
                let mut p = self.agent.params.clone();
                p.tools = build_tool_schemas(tm);
                // 显式声明「允许一条响应里发多个工具调用」。调用方已经
                // 明确设过就尊重它，不覆盖。
                if p.parallel_tool_calls.is_none() {
                    p.parallel_tool_calls = parallel_tool_calls_pref();
                }
                p
            });
            let completion = if self.is_stream_mode() {
                // Stream 模式：逐 Delta 消费，发 ModelDelta trace 让 UI 逐 token 渲染。
                // Done 携带完整 tool_calls + usage，组装成 Completion 后下游工具循环零改动。
                // 模型全链不可用 → 自动暂停 session 门，等用户「继续」后重试。
                // 空 completion（无文本、无 tool_calls）多为厂商抖动：流式在连接
                // 建立后无法链式 fallback，拿到空 Done 时整流重试，上限 2 次。
                let mut empty_retries = 0u8;
                // 模型全链不可用导致的自动暂停次数（同一次 model call
                // 内累计），驱动 `pause_wait_model_unavailable` 的退避。
                let mut unavail_attempt = 0u32;
                // 「连接建立成功、但流上还没吐出任何内容就报错」时的换模型次数。
                //
                // 修的 bug：`chat_stream` 的链式 fallback 只覆盖**连接建立
                // 阶段**（返回 Err）。厂商在连接成功之后才吐 400 的话，错误
                // 是以 `StreamEvent::HttpError` 进 channel 的，原来直接
                // `return Err` —— 整条模型链一次都没被咨询，turn 当场判死。
                // 实测（`chat -r programmer`，glm-5.3 中转站）：把一条工具校验
                // 错误喂回去之后厂商回 400，链上还有 MiniMax-M3 /
                // deepseek-v4-flash / deepseek-v4-pro 三个健康模型，全没轮到。
                let mut stream_fallbacks = 0u8;
                const MAX_STREAM_FALLBACKS: u8 = 3;
                'stream_attempt: loop {
                    let mut rx = loop {
                        match self.agent.chat_stream(&messages, chat_params.as_ref()).await {
                            Ok(rx) => break rx,
                            Err(e @ AgentError::ModelsUnavailable { .. }) => {
                                let overflow = failures_include_context_overflow(&e);
                                if !self
                                    .pause_wait_model_unavailable(&e, unavail_attempt)
                                    .await
                                {
                                    return Err(e);
                                }
                                unavail_attempt = unavail_attempt.saturating_add(1);
                                // 用户在暂停期间可能已在 UI 改了模型配置：
                                // 重试前先热更新 chain。
                                self.maybe_reload_models();
                                if overflow {
                                    // 上下文超窗：同一 payload 重试必败，
                                    // 先瘦身本轮待发消息（只影响待发列表，
                                    // 不动持久 context）。
                                    slim_oversized_messages(&mut messages);
                                }
                            }
                            Err(e) => return Err(e),
                        }
                    };
                    let mut streamed_any = false;
                    let c = loop {
                        match rx.recv().await {
                            Some(StreamEvent::Delta { content, .. }) => {
                                let delta_text: String = content.iter()
                                    .filter_map(|p| match p {
                                        ContentPart::Text { text } => Some(text.as_str()),
                                        _ => None,
                                    })
                                    .collect::<Vec<_>>()
                                    .join("");
                                if !delta_text.is_empty() {
                                    // 一旦有内容吐给用户，就不能再整流重开了
                                    // ——重开会把这段重复输出一遍。
                                    streamed_any = true;
                                    self.sink.emit(TraceEvent::ModelDelta {
                                        meta: meta.refreshed(),
                                        delta: delta_text,
                                    });
                                }
                            }
                            Some(StreamEvent::Done { content, tool_calls, usage, stop_reason }) => {
                                let text = content.iter()
                                    .filter_map(|p| match p {
                                        ContentPart::Text { text } => Some(text.as_str()),
                                        _ => None,
                                    })
                                    .collect::<Vec<_>>()
                                    .join("");
                                break Completion {
                                    content: text,
                                    content_parts: content,
                                    tool_calls,
                                    stop_reason: if stop_reason.is_empty() { "stop".into() } else { stop_reason },
                                    usage,
                                };
                            }
                            Some(StreamEvent::Error(e)) => {
                                let err = AiError::Stream(e);
                                if let Some(next) = self.try_stream_fallback(
                                    &err,
                                    streamed_any,
                                    &mut stream_fallbacks,
                                    MAX_STREAM_FALLBACKS,
                                    &meta,
                                ) {
                                    let _ = next;
                                    continue 'stream_attempt;
                                }
                                return Err(AgentError::from(err));
                            }
                            Some(StreamEvent::HttpError { status, message }) => {
                                let err = AiError::Api { status, message };
                                if let Some(next) = self.try_stream_fallback(
                                    &err,
                                    streamed_any,
                                    &mut stream_fallbacks,
                                    MAX_STREAM_FALLBACKS,
                                    &meta,
                                ) {
                                    let _ = next;
                                    continue 'stream_attempt;
                                }
                                return Err(AgentError::from(err));
                            }
                            None => {
                                return Err(AgentError::from(AiError::Stream(
                                    "stream closed before Done".into(),
                                )));
                            }
                        }
                    };
                    if is_empty_completion(&c) && empty_retries < 2 {
                        empty_retries += 1;
                        continue 'stream_attempt;
                    }
                    break 'stream_attempt c;
                }
            } else {
                // 非流式模式（默认）：chat() 内部走流式传输 + idle watchdog，对外返回完整 Completion。
                // 模型全链不可用 → 自动暂停 session 门，退避自动重试 / 等用户「继续」。
                let mut unavail_attempt = 0u32;
                loop {
                    // 慢调用可见性：非流式下生成过程没有任何中间事件，
                    // 慢速 trickle 在 UI 上等同卡死（日志事故：glm 大
                    // 上下文慢生成 8 分钟，叠加 advisor 暂停弹窗，看起来
                    // 像假暂停）。超过阈值未返回发一次 ModelCallSlow，
                    // 然后继续等——提示不是超时，不影响等待本身。
                    let r = {
                        let chat = self.agent.chat(
                            &messages,
                            chat_params.as_ref(),
                            WaitPolicy::WaitAndRetry,
                        );
                        tokio::pin!(chat);
                        let mut noticed = false;
                        loop {
                            match tokio::time::timeout(slow_model_call_notice(), &mut chat).await
                            {
                                Ok(r) => break r,
                                Err(_) => {
                                    if !noticed {
                                        noticed = true;
                                        self.sink.emit(TraceEvent::ModelCallSlow {
                                            meta: meta.refreshed(),
                                            model_id: self.agent.served_model_id(),
                                            elapsed_secs: slow_model_call_notice().as_secs(),
                                        });
                                    }
                                }
                            }
                        }
                    };
                    match r {
                        Ok(c) => break c,
                        Err(e @ AgentError::ModelsUnavailable { .. }) => {
                            let overflow = failures_include_context_overflow(&e);
                            if !self
                                .pause_wait_model_unavailable(&e, unavail_attempt)
                                .await
                            {
                                return Err(e);
                            }
                            unavail_attempt = unavail_attempt.saturating_add(1);
                            // 用户在暂停期间可能已在 UI 改了模型配置：
                            // 重试前先热更新 chain。
                            self.maybe_reload_models();
                            if overflow {
                                // 上下文超窗：同一 payload 重试必败，
                                // 先瘦身本轮待发消息（只影响待发列表，
                                // 不动持久 context）。
                                slim_oversized_messages(&mut messages);
                            }
                        }
                        Err(e) => return Err(e),
                    }
                }
            };
            let latency_ms = chat_start.elapsed().as_millis() as u64;

            self.total_usage.input_tokens += completion.usage.input_tokens;
            self.total_usage.output_tokens += completion.usage.output_tokens;
            self.total_usage.thinking_tokens += completion.usage.thinking_tokens;
            total_input += completion.usage.input_tokens;
            total_output += completion.usage.output_tokens;
            total_thinking += completion.usage.thinking_tokens;

            final_response = completion.content.clone();
            if !final_response.trim().is_empty() {
                last_substantive_response = final_response.clone();
            }

            self.sink.emit(TraceEvent::ModelCall {
                meta: meta.refreshed(),
                model_id: self.agent.served_model_id(),
                params_json: serde_json::to_string(&self.agent.params).unwrap_or_default(),
                latency_ms,
                finish_reason: completion.stop_reason.clone(),
            });
            // 输出被长度上限截断：native tool_calls 的 arguments 会是半截
            // JSON，下游 `from_str` 报 "EOF while parsing a string"，模型
            // 看到的却是一句不知所以的 `invalid JSON`，于是原样重发再撞
            // 一次。实测会话里 manager 连撞两次（finish_reason=length，
            // args 断在 436 / 12081 字符），第三次靠删掉几乎所有 subtasks
            // 才挤进去——拆分粒度是被这条静默截断吃掉的，不是判断失误。
            // 记一个 flag，MalformedArgs 时把真实原因和可执行对策讲清。
            let output_truncated = is_length_truncation(&completion.stop_reason);
            self.sink.emit(TraceEvent::ModelRawOut {
                meta: meta.refreshed(),
                raw_content: final_response.clone(),
            });

            // 2b. Run PostResponseHook
            {
                let mut ctx = crate::hooks::PostResponseCtx { raw: &final_response };
                let outcome = self.hooks.run_post_response(&mut ctx, |hook_name, point, kind| {
                    self.sink.emit(TraceEvent::HookFired {
                        meta: meta.refreshed(),
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
            let mut tool_calls: Vec<latte_ai::models::ToolCall> =
                dedupe_native_tool_calls(completion.tool_calls.clone());
            // Guarantee unique, non-empty ids so each tool_result pairs
            // back to its own call. Without this, parallel calls that
            // share an id (e.g. two delegates to the same role) collapse
            // to just the last result on the round-trip to the model.
            ensure_unique_tool_call_ids(&mut tool_calls);

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
                meta: meta.refreshed(),
                raw_in: final_response.clone(),
                parsed: parsed_calls.clone(),
                diagnostics: ParseDiag {
                    opens_found: tool_calls.len() as u32,
                    closes_matched: tool_calls.len() as u32,
                    unmatched_opens: Vec::new(),
                },
            });

            if tool_calls.is_empty() {
                // 产出被长度上限截断、且这一轮**没有工具调用**——也就是
                // 这半截正文就要作为最终答复返回了。
                //
                // 修的 bug（实测实锤：2026-09-07 jemalloc 会话
                // `wf-implementation_plan-1788718876052941`）：architect 在
                // tasks_json 步撞上 8192 token 上限，`finish_reason=length`，
                // 正文停在 `…large（大对象直连 extent）、` —— 半个 JSON。
                // 引擎不看 finish_reason，把它当正常产出穿给下游终审；
                // 终审连着三轮 REJECT，理由都是「JSON 在 T2c 处被截断」，
                // max_iterations 耗尽后整条 20 分钟的流水线判 failed。
                // 更糟的是**返工无效**：gate 跳回 breakdown 重拆，而反馈里
                // 只说「JSON 不完整」、没说「你被长度上限掐断了」，于是
                // 模型又写一份同样长的清单、又被掐断，三轮空转。
                //
                // 对策与上面的悬挂标记同形：重试一次，并且把**真实原因和
                // 可执行对策**讲明（不是「重写一遍」，而是「压缩到能装下」）。
                // 仍被截断则照收，但在正文末尾留一条显式标记，让人、
                // 下游评审与 workflow 契约都能看见——最坏情况下也不再是
                // 静默采纳。
                if length_retried == 0 && output_truncated {
                    length_retried += 1;
                    tracing::warn!(
                        stop_reason = %completion.stop_reason,
                        chars = final_response.chars().count(),
                        "final response hit the output length limit; retrying once with a compression instruction"
                    );
                    messages.push(Message::assistant(final_response.clone()));
                    messages.push(Message::user(format!(
                        "你上一条回复撞上了输出长度上限（finish_reason={}），\
                         正文在 {} 字符处被强行截断、内容不完整（结构化产出会是半截 \
                         JSON，下游无法使用）。\n\n\
                         不要原样重写——同样的长度必然再次被截断。请压缩到能一次装下：\
                         减少条目数量（宁少勿缺）、精简每条的长文本字段、\
                         去掉解释性前言与重复表述，只保留要求的那部分内容。\
                         若信息量确实装不下一条回复，先输出**完整且自洽**的一部分，\
                         并在结尾明确说明还缺哪些、需要后续补充。",
                        completion.stop_reason,
                        final_response.chars().count()
                    )));
                    continue;
                }
                if output_truncated {
                    // 重试后仍被截断：照收，但绝不静默。
                    final_response.push_str(&format!(
                        "\n\n⚠️ [产出被截断] 本回复撞上模型输出长度上限\
                         （finish_reason={}，已重试 1 次仍未装下），\
                         上面的内容**不完整**，结构化数据（JSON/清单）很可能缺尾。\
                         下游请勿当作完整产出使用。",
                        completion.stop_reason
                    ));
                    tracing::warn!(
                        stop_reason = %completion.stop_reason,
                        "final response still truncated after retry; annotated the payload"
                    );
                }
                // 产出卫生：最终答复尾部带悬挂工具标记（native
                // function-calling 下 `</parameter>`/`</function>` 等
                // 只会是模型泄漏的垃圾，且正文常断在半句——glm-5.2 在
                // 实测现场的实锤形态）。重试一次让模型重出完整
                // 答复，省一轮 advisor 打回；重试仍带标记则照收（避免
                // 死循环）。
                if markup_retried == 0 && has_dangling_tool_markup_tail(&final_response) {
                    markup_retried += 1;
                    tracing::warn!(
                        "final response ends with dangling tool markup; retrying once"
                    );
                    messages.push(Message::assistant(final_response.clone()));
                    messages.push(Message::user(
                        "你上一条回复的尾部带有未完成的工具调用标记（如 </parameter>/</function>），回复疑似被截断。请重新输出完整答复；需要调用工具时用 native function-calling，正文里不要输出任何工具调用标记。"
                            .to_string(),
                    ));
                    continue;
                }
                break;
            }

            // 3b. Run PostParseHook (can mutate parsed calls)
            let mut post_parse_calls: Vec<ParsedCall> = parsed_calls;
            {
                let mut ctx = crate::hooks::PostParseCtx { parsed: &mut post_parse_calls };
                let outcome = self.hooks.run_post_parse(&mut ctx, |hook_name, point, kind| {
                    self.sink.emit(TraceEvent::HookFired {
                        meta: meta.refreshed(),
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

                // ── Concurrent delegate dispatch (opt-in) ──────────────
                //
                // When `LATTE_AGENT_DELEGATE_PARALLEL` is on AND this
                // response batches ≥2 `delegate` calls, run those
                // delegates concurrently on a `JoinSet` while any
                // non-delegate calls in the same batch run serially
                // inline. Results are collected by original index so the
                // `&mut self` bookkeeping (loop detection, success count,
                // tool_result append) happens in call order — identical
                // to the serial path. Everything is capped downstream by
                // the delegate tool's own `Semaphore`.
                //
                // Default off → falls through to the original strictly
                // serial loop below (unchanged behavior).
                let delegate_positions: Vec<usize> = post_parse_calls
                    .iter()
                    .enumerate()
                    .filter(|(_, tc)| short_tool_name(&tc.name) == "delegate")
                    .map(|(i, _)| i)
                    .collect();

                if delegate_parallel_enabled() && delegate_positions.len() >= 2 {
                    use tokio::task::JoinSet;
                    let is_delegate: std::collections::HashSet<usize> =
                        delegate_positions.iter().copied().collect();
                    let mut results: Vec<Option<OneCallResult>> =
                        (0..post_parse_calls.len()).map(|_| None).collect();

                    // Pass 1: spawn all delegate calls.
                    let mut set: JoinSet<(usize, OneCallResult)> = JoinSet::new();
                    for (i, tc) in post_parse_calls.iter().enumerate() {
                        if !is_delegate.contains(&i) {
                            continue;
                        }
                        let tm_c = tm.clone();
                        let hooks_c = self.hooks.clone();
                        let sink_c = self.sink.clone();
                        let rp_c = self.retry_policy.clone();
                        let cwd_c = self.cwd.clone();
                        let current_session_file_c = self.current_session_file.clone();
                        let meta_c = meta.clone();
                        let tc_c = tc.clone();
                        set.spawn(async move {
                            (
                                i,
                                run_one_tool_call(
                                    tm_c, hooks_c, sink_c, rp_c, cwd_c,
                                    current_session_file_c, meta_c, tc_c,
                                )
                                .await,
                            )
                        });
                    }

                    // Pass 2: run non-delegate calls inline (overlaps
                    // with the spawned delegates).
                    for (i, tc) in post_parse_calls.iter().enumerate() {
                        if is_delegate.contains(&i) {
                            continue;
                        }
                        results[i] = Some(
                            run_one_tool_call(
                                tm.clone(),
                                self.hooks.clone(),
                                self.sink.clone(),
                                self.retry_policy.clone(),
                                self.cwd.clone(),
                                self.current_session_file.clone(),
                                meta.clone(),
                                tc.clone(),
                            )
                            .await,
                        );
                    }

                    // Pass 3: join delegates.
                    while let Some(joined) = set.join_next().await {
                        match joined {
                            Ok((i, r)) => results[i] = Some(r),
                            Err(e) => {
                                return Err(AgentError::Tool(format!(
                                    "delegate task join failed: {e}"
                                )))
                            }
                        }
                    }

                    // Pass 4: apply bookkeeping + append tool_results in
                    // original call order.
                    for (i, slot) in results.into_iter().enumerate() {
                        let r = match slot {
                            Some(r) => r,
                            None => continue,
                        };
                        let name = post_parse_calls[i].name.clone();
                        self.apply_call_result(
                            r,
                            &name,
                            &mut messages,
                            &mut loop_detector,
                            &mut permanent_streak,
                            &final_response,
                            &last_substantive_response,
                        )?;
                    }

                    round += 1;
                    continue;
                }

                // ── 阻塞人机交互的调用排到最后（默认开）─────────────
                //
                // 同一批里如果既有"会停下来等人点弹窗"的调用（`ask` /
                // 内部会 `ask` 的 workflow），又有别的派发调用，先把不等人
                // 的跑完，再去等人。见 [`human_blocking_last_order`]：那里
                // 记了这条路径堵的实测事故（`implementation_plan` 被
                // `learn_loop` 的弹窗无限期押在队列里，一次都没执行）。
                //
                // 重排只改执行时机；记账与 `tool_result` 回填仍按原始下标
                // 顺序，模型看到的消息序列与不重排时完全一致。
                let blocking_last_order = if blocking_calls_last_enabled() {
                    human_blocking_last_order(&post_parse_calls, self.cwd.as_deref())
                } else {
                    None
                };

                if let Some(exec_order) = blocking_last_order {
                    let names: Vec<&str> = exec_order
                        .iter()
                        .map(|&i| short_tool_name(&post_parse_calls[i].name))
                        .collect();
                    tracing::info!(
                        "blocking-last reorder: executing {} calls as {:?} ({:?})",
                        exec_order.len(),
                        exec_order,
                        names,
                    );
                    log_hook_fire(
                        "blocking_calls_last",
                        crate::trace::HookPoint::PostParse,
                        "reorder",
                    );

                    let mut results: Vec<Option<OneCallResult>> =
                        (0..post_parse_calls.len()).map(|_| None).collect();
                    for &i in &exec_order {
                        self.park_if_paused("tool exec boundary").await;
                        results[i] = Some(
                            run_one_tool_call(
                                tm.clone(),
                                self.hooks.clone(),
                                self.sink.clone(),
                                self.retry_policy.clone(),
                                self.cwd.clone(),
                                self.current_session_file.clone(),
                                meta.clone(),
                                post_parse_calls[i].clone(),
                            )
                            .await,
                        );
                    }

                    // 按原始调用顺序记账 + 回填 tool_result。
                    for (i, slot) in results.into_iter().enumerate() {
                        let r = match slot {
                            Some(r) => r,
                            None => continue,
                        };
                        let name = post_parse_calls[i].name.clone();
                        self.apply_call_result(
                            r,
                            &name,
                            &mut messages,
                            &mut loop_detector,
                            &mut permanent_streak,
                            &final_response,
                            &last_substantive_response,
                        )?;
                    }

                    round += 1;
                    continue;
                }

                // ── Concurrent read-only fan-out ───────────────────────
                //
                // 同一响应里**连续**的多个只读工具调用（`read` /
                // `code_graph`，见 [`READONLY_PARALLEL_TOOLS`]）并发执行，
                // 其余调用保持原地串行。默认开，`LATTE_AGENT_READONLY_
                // PARALLEL=0` 退回串行。
                //
                // 为什么安全：段内全是无副作用的读，顺序无关；任何可能
                // 写状态的调用都是屏障（见 [`readonly_parallel_runs`]），
                // 不会跨屏障重排。记账仍按原始顺序走
                // [`AgentRunner::apply_call_result`]，模型看到的
                // `tool_result` 序列与串行路径逐字节一致。
                //
                // 为什么值得：实测会话里 programmer 一轮只发 1 个
                // 调用、185 次工具 I/O 只占 18s，模型往返却烧掉 577s。
                // 让模型批量发之后，工具侧不能再成为新的串行瓶颈——
                // 一段 8 个 read 并发跑，墙钟从 8×latency 降到 ≈1×。
                let readonly_runs = if readonly_parallel_enabled() {
                    readonly_parallel_runs(&post_parse_calls)
                } else {
                    Vec::new()
                };

                if !readonly_runs.is_empty() {
                    use tokio::sync::Semaphore;
                    use tokio::task::JoinSet;

                    let mut results: Vec<Option<OneCallResult>> =
                        (0..post_parse_calls.len()).map(|_| None).collect();
                    let sem = Arc::new(Semaphore::new(readonly_parallel_max()));
                    // 段起点 → 段终点，便于顺序遍历时跳段。
                    let run_start: std::collections::HashMap<usize, usize> =
                        readonly_runs.iter().copied().collect();

                    let mut i = 0usize;
                    while i < post_parse_calls.len() {
                        if let Some(&end) = run_start.get(&i) {
                            // 并发段：spawn 段内全部只读调用后一起 join。
                            // park 点放在段边界（与串行路径的「in-flight
                            // tool 跑完才停」语义一致）。
                            self.park_if_paused("tool exec boundary").await;
                            tracing::debug!(
                                "readonly fan-out: {} calls concurrently (cap {})",
                                end - i,
                                readonly_parallel_max(),
                            );
                            let mut set: JoinSet<(usize, OneCallResult)> = JoinSet::new();
                            for (idx, tc) in post_parse_calls
                                .iter()
                                .enumerate()
                                .take(end)
                                .skip(i)
                            {
                                let tm_c = tm.clone();
                                let hooks_c = self.hooks.clone();
                                let sink_c = self.sink.clone();
                                let rp_c = self.retry_policy.clone();
                                let cwd_c = self.cwd.clone();
                                let current_session_file_c = self.current_session_file.clone();
                                let meta_c = meta.clone();
                                let tc_c = tc.clone();
                                let sem_c = sem.clone();
                                set.spawn(async move {
                                    // permit 在 task 内获取：spawn 不阻塞，
                                    // 实际 in-flight 数由 semaphore 钳住。
                                    let _permit = sem_c.acquire().await;
                                    (
                                        idx,
                                        run_one_tool_call(
                                            tm_c, hooks_c, sink_c, rp_c, cwd_c,
                                            current_session_file_c, meta_c, tc_c,
                                        )
                                        .await,
                                    )
                                });
                            }
                            while let Some(joined) = set.join_next().await {
                                match joined {
                                    Ok((idx, r)) => results[idx] = Some(r),
                                    Err(e) => {
                                        return Err(AgentError::Tool(format!(
                                            "readonly tool task join failed: {e}"
                                        )))
                                    }
                                }
                            }
                            i = end;
                        } else {
                            // 屏障 / 单个只读调用：原地串行。
                            self.park_if_paused("tool exec boundary").await;
                            results[i] = Some(
                                run_one_tool_call(
                                    tm.clone(),
                                    self.hooks.clone(),
                                    self.sink.clone(),
                                    self.retry_policy.clone(),
                                    self.cwd.clone(),
                                    self.current_session_file.clone(),
                                    meta.clone(),
                                    post_parse_calls[i].clone(),
                                )
                                .await,
                            );
                            i += 1;
                        }
                    }

                    // 按原始调用顺序记账。
                    for (idx, slot) in results.into_iter().enumerate() {
                        let r = match slot {
                            Some(r) => r,
                            None => continue,
                        };
                        let name = post_parse_calls[idx].name.clone();
                        self.apply_call_result(
                            r,
                            &name,
                            &mut messages,
                            &mut loop_detector,
                            &mut permanent_streak,
                            &final_response,
                            &last_substantive_response,
                        )?;
                    }

                    round += 1;
                    continue;
                }

                // 逐个执行工具调用
                for tc in &post_parse_calls {
                    // 工具名已是扁平规范名（registry 注册名 == 模型看到的
                    // 名字），直接查找，不再有别名反向映射。保留短名兜底仅为
                    // namespace 遗留工具兼容。
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
                    // 重试 ≠ 重新问 model。本层自动 reparse / reexecute；
                    // 终态无论成败都会以 tool_result 喂回 model（协议闭环
                    // 要求），死循环防护由 LoopDetector 承担。
                    let policy = self.retry_policy.clone();
                    let mut attempt: u32 = 0;
                    let max_attempts: u32 = 2;
                    // 终止态：Ok(result_str) or Err((kind, detail_str))
                    let mut final_outcome: Result<String, (ToolCallErrorKind, String)> =
                        Err((ToolCallErrorKind::ToolNotFound { tried_aliases: vec![] }, "init".into()));

                    while attempt < max_attempts {
                        attempt += 1;
                        // Session-level 暂停：tool 启动前 park（in-flight
                        // tool 跑完才停，下一个 tool 启动前才看 gate）。
                        self.park_if_paused("tool exec boundary").await;
                        // 1. parse args。native 协议下模型输出的是合法 JSON；
                        // 若 latte-ai 层解析失败，arguments_raw 保留原始坏串，
                        // 这里 from_str 会失败并归类为 MalformedArgs。
                        let input: serde_json::Value = match serde_json::from_str(&tc.args) {
                            Ok(v) => v,
                            Err(e) => {
                                // 截断导致的坏 JSON 与"模型写错格式"是两
                                // 回事：前者重发同一份必然再撞，必须让模型
                                // 缩小单次提交量（分批 / 精简字段），而不是
                                // 原样重试。
                                let detail = if output_truncated {
                                    format!(
                                        "工具参数被模型输出长度上限截断（finish_reason=length，\
                                         收到 {} 字节不完整 JSON）：{e}。不要原样重发——\
                                         必须缩小单次提交量：分批调用（每批 3-5 项）\
                                         或精简每项的长文本字段。",
                                        tc.args.len()
                                    )
                                } else {
                                    format!("invalid JSON: {e}")
                                };
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
                        // 类型方言纠正：必须在 tm.execute 之前——工具
                        // 管理器先跑 schema 校验，handler 里的宽松解析
                        // 根本碰不到。
                        let input = match tm.get_tool(&full_name) {
                            Some(t) => coerce_tool_input_to_schema(input, &t.input_schema),
                            None => input,
                        };
                        // 行号自愈（anchor）：见 `crate::edit_anchor` 与
                        // run_one_tool_call 里的同款处理。
                        let input = {
                            let mut healed = input;
                            let notes =
                                crate::edit_anchor::heal_edit_input(&resolved_name, &mut healed);
                            for note in notes {
                                log_hook_fire(
                                    &note,
                                    crate::trace::HookPoint::PreTool,
                                    "anchor",
                                );
                            }
                            healed
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
                                    meta: meta.refreshed(),
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
                        ctx.metadata = tool_execution_metadata(
                            self.cwd.as_deref(),
                            self.current_session_file.as_deref(),
                        );
                        let tool_start = Instant::now();
                        let exec_result = tm.execute(&full_name, input.clone(), Some(ctx)).await;
                        let tool_latency = tool_start.elapsed().as_millis() as u64;
                        let args_json = serde_json::to_string(&input)
                            .unwrap_or_else(|_| tc.args.clone());
                        // 4. Loop detection（per-args 哈希，与 retry 无关）
                        if let LoopDecision::Break(reason) = loop_detector.record(&tc.name, &args_json) {
                            // 带出已产出正文：熔断是「模型卡住了」，不是
                            // 「什么都没做出来」。上层 workflow / delegate
                            // 靠这份 partial 降级采纳，丢了它就等于把前面
                            // 几十轮的成果一起扔掉。
                            return Err(AgentError::ToolLoopDetected {
                                tool: tc.name.clone(),
                                reason,
                                partial: if last_substantive_response.is_empty() {
                                    final_response.clone()
                                } else {
                                    last_substantive_response.clone()
                                },
                            });
                        }
                        match exec_result {
                            Ok(result) => {
                                // Pre-persistence gate: D6 需要 tool
                                // 实际执行计数。失败的工具不计入。
                                self.last_turn_tool_count += 1;
                                // 记进 anchor 台账（见 crate::edit_anchor）。
                                if let Ok(raw) = serde_json::to_string(&result) {
                                    crate::edit_anchor::record_tool_result(&resolved_name, &raw);
                                }
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
                                                meta: meta.refreshed(),
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
                                    meta: meta.refreshed(),
                                    name: tc.name.clone(),
                                    args_json: args_json.clone(),
                                    latency_ms: tool_latency,
                                    status: ToolStatus::Ok(
                                        serde_json::to_string(&result).unwrap_or_default()
                                    ),
                                });
                                self.last_turn_tool_summaries.push(format!(
                                    "{} {} → {}",
                                    tc.name,
                                    truncate_tool_summary(&args_json, 160),
                                    truncate_tool_summary(&result_str, 240),
                                ));
                                final_outcome = Ok(result_str);
                                break;
                            }
                            Err(e) => {
                                let kind = classify_tool_execution_error(&e);
                                let detail = match &kind {
                                    ToolCallErrorKind::ToolNotFound { .. } => {
                                        tool_not_found_detail(&tc.name, e.to_string())
                                    }
                                    _ => e.to_string(),
                                };
                                self.sink.emit(TraceEvent::ToolExec {
                                    meta: meta.refreshed(),
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
                                meta: meta.refreshed(),
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
                                meta: meta.refreshed(),
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
                            if permanent_streak.0 == tc.name {
                                permanent_streak = (String::new(), 0);
                            }
                            messages.push(Message::tool_result(tc.id.clone(), result_str));
                        }
                        Err((kind, detail)) => {
                            // 成功失败都必须回填 tool_result：native 协议要求
                            // 每个 tool_call_id 都有对应 tool 消息，缺一条
                            // deepseek 系 API 下一轮直接 400（"tool_calls
                            // must be followed by tool messages"），整个会话
                            // 卡死（日志事故：architect 调了未授权的
                            // bash，ToolNotFound 不回填 → 历史破损 → 模型
                            // 链全灭 → 会话永久暂停）。防"模型看自己错误
                            // 输出循环恶化"靠 LoopDetector，不靠断链。
                            //
                            // 压缩错误消息：长 payload（write/edit 类的大内容）
                            // 原样 echo 回模型会变成巨大 tool_result。但**中段
                            // 省略**而不是截头 —— 错误分类在头、修复指引在尾，
                            // 砍尾等于把唯一可操作的信息扔了（见
                            // MAX_TOOL_ERROR_BYTES 的文档）。
                            let compacted =
                                compact_middle_out(&detail, MAX_TOOL_ERROR_BYTES);
                            messages.push(Message::tool_result(tc.id.clone(), compacted));
                            // 熔断：同一工具连续被确定性校验拒绝。
                            // NUDGE_AT 次 → 追加一条硬指令，让模型停手
                            // 换路径（此时它已经证明自己修不好这份输入）；
                            // BREAK_AT 次 → 判死循环退出，避免整个 turn
                            // 烧在一个工具的输入格式上。
                            if matches!(kind, ToolCallErrorKind::PermanentExec { .. }) {
                                if permanent_streak.0 == tc.name {
                                    permanent_streak.1 += 1;
                                } else {
                                    permanent_streak = (tc.name.clone(), 1);
                                }
                                const NUDGE_AT: usize = PERMANENT_NUDGE_AT;
                                const BREAK_AT: usize = PERMANENT_BREAK_AT;
                                if permanent_streak.1 >= BREAK_AT {
                                    return Err(AgentError::ToolLoopDetected {
                                        tool: tc.name.clone(),
                                        reason: format!(
                                            "'{}' 连续 {} 次因输入校验被拒（每次参数都不同），\
                                             模型无法自行修正，停止重试",
                                            tc.name, permanent_streak.1
                                        ),
                                        partial: if last_substantive_response.is_empty() {
                                            final_response.clone()
                                        } else {
                                            last_substantive_response.clone()
                                        },
                                    });
                                }
                                if permanent_streak.1 == NUDGE_AT {
                                    messages.push(Message::user(format!(
                                        "⚠️ `{}` 已连续 {} 次因输入校验失败被拒。不要再用同一个\
                                         工具反复试：要么换一条路径完成任务，要么把当前进展和\
                                         卡点直接讲给用户。",
                                        tc.name, permanent_streak.1
                                    )));
                                }
                            } else if permanent_streak.0 == tc.name {
                                permanent_streak = (String::new(), 0);
                            }
                        }
                    }
                }
            }

            round += 1;
        }

        // 5. Emit TurnEnd
        //
        // elapsed_ms 用 **wall-clock**：`Instant` 在 macOS 上不含系统
        // 睡眠时间，只用它会让 turn 看起来比实际短得多（实测
        // 2026-08-26 会话：manager 的 turn 真实跨度 67 分钟，单调时钟
        // 只有 28.7 分钟——差的 38 分钟是合盖睡眠。当时 TurnEnd 报的是
        // 单调值，于是"trace 里没有任何事件的 38 分钟空洞"既没有事件
        // 也没有时长佐证，只能靠翻 pmset 日志才定位）。
        // 单调值同时保留在下面的 suspended 判定里：两者的差就是进程
        // 被挂起（睡眠 / SIGSTOP）的时长，超过 60s 就 warn 出来。
        let mono_elapsed = turn_start.elapsed();
        let wall_elapsed = turn_start_wall
            .elapsed()
            .unwrap_or(mono_elapsed)
            .max(mono_elapsed);
        let suspended = wall_elapsed.saturating_sub(mono_elapsed);
        if suspended.as_secs() >= 60 {
            tracing::warn!(
                "turn 期间进程被挂起约 {}s（系统睡眠/SIGSTOP）：wall {}s vs 运行 {}s——\
                 trace 上这段时间不会有任何事件，属正常现象",
                suspended.as_secs(),
                wall_elapsed.as_secs(),
                mono_elapsed.as_secs(),
            );
        }
        let elapsed_ms = wall_elapsed.as_millis() as u64;
        self.sink.emit(TraceEvent::TurnEnd {
            // 结束时刻的 meta —— 此前复用 turn 起始 meta，TurnEnd.ts
            // 指向 turn 开头，时间线上看像是 turn 刚开始就结束了。
            meta: meta.refreshed(),
            total_input,
            total_output,
            total_thinking,
            elapsed_ms,
        });

        // Store successful assistant output in context. 本轮输入已在
        // 模型调用前提交，不能在这里再次 push。
        // 空响应不入 context：部分 API（deepseek 系）对 text 为空的
        // assistant 消息直接 400（"text content is empty"）——一旦入
        // context，后续每个请求都带着它，这个 turn 就永久卡死。
        // （日志事故：reviewer 空响应入 context 后 gate 重试全 400，
        // 整个 workflow 被拖垮。）
        if !final_response.trim().is_empty() {
            self.context.push(Message::assistant(final_response.clone()));
        }

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

    /// Bind the exact persisted transcript exposed as `session://current`.
    pub fn with_current_session_file(mut self, path: std::path::PathBuf) -> Self {
        self.current_session_file = Some(path);
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
/// 名字），不再有 friendly_tool_name / namespace 别名转换。
/// `tool_choice` 不在此设置，取自 `agent.params.tool_choice`（默认 Auto）。
fn normalize_model_schema_node(schema: &mut serde_json::Value) {
    let Some(node) = schema.as_object_mut() else {
        return;
    };

    let schema_type = node
        .get("type")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let min = node.remove("min_length");
    let max = node.remove("max_length");
    match schema_type.as_deref() {
        Some("array") => {
            if let Some(min) = min {
                node.insert("minItems".into(), min);
            }
            if let Some(max) = max {
                node.insert("maxItems".into(), max);
            }
        }
        Some("string") => {
            if let Some(min) = min {
                node.insert("minLength".into(), min);
            }
            if let Some(max) = max {
                node.insert("maxLength".into(), max);
            }
        }
        _ => {}
    }

    if let Some(properties) = node
        .get_mut("properties")
        .and_then(serde_json::Value::as_object_mut)
    {
        for property in properties.values_mut() {
            normalize_model_schema_node(property);
        }
    }
    // `$defs`（自递归 schema 的共享定义表，如 plan 工具的 planTask）同样
    // 需要递归归一化——$ref 目标的内部节点不走 properties/items 路径。
    if let Some(defs) = node
        .get_mut("$defs")
        .and_then(serde_json::Value::as_object_mut)
    {
        for def in defs.values_mut() {
            normalize_model_schema_node(def);
        }
    }
    for key in [
        "items",
        "additionalProperties",
        "contains",
        "propertyNames",
        "not",
        "if",
        "then",
        "else",
    ] {
        if let Some(child) = node.get_mut(key) {
            normalize_model_schema_node(child);
        }
    }
    for key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
        if let Some(children) = node.get_mut(key).and_then(serde_json::Value::as_array_mut) {
            for child in children {
                normalize_model_schema_node(child);
            }
        }
    }
}

fn model_facing_tool_schema(
    definition: &latte_rs_agent_tools::types::ToolDefinition,
) -> serde_json::Value {
    let mut schema = serde_json::to_value(&definition.input_schema)
        .unwrap_or_else(|_| serde_json::json!({}));
    normalize_model_schema_node(&mut schema);
    schema
}

fn build_tool_schemas(
    tm: &Arc<dyn latte_rs_agent_tools::types::ToolManager>,
) -> Vec<latte_ai::models::Tool> {
    tm.get_tool_definitions()
        .iter()
        .map(|definition| latte_ai::models::Tool {
            name: definition.name.clone(),
            description: Some(definition.description.clone()),
            parameters: model_facing_tool_schema(definition),
            strict: None,
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
    // 批量 read 的 `paths` 数组同样逐项改写。不做的话会出现「单路径按 cwd
    // 解析、批量路径不解析」的不对称——工具侧 `resolve_tool_path` 虽然也会
    // 兜一层 ctx.cwd，但 trace 里记的 args_json 就不一致了，排查时看到两种
    // 形状的路径。
    if let Some(Value::Array(items)) = obj.get_mut("paths") {
        for item in items.iter_mut() {
            if let Some(s) = item.as_str() {
                if is_relative_fs_path(s) {
                    *item = Value::String(cwd.join(s).display().to_string());
                }
            }
        }
    }
    if obj.contains_key("command") {
        obj.entry("cwd".to_string())
            .or_insert_with(|| Value::String(cwd.display().to_string()));
    }
    input
}

/// 按工具自己的 input schema 把模型的「类型方言」纠正回声明类型。
///
/// 动机：工具管理器在 handler **之前**跑 `validate_input`，类型不符
/// 直接 `ToolError::Validation`，handler 里写多宽松的解析都碰不到。
/// 而模型（尤其非头部模型）把 boolean 发成 `"true"`、把候选项数组包
/// 进 `{"item":[...]}` 是高频错误——一次就废掉整个工具调用，
/// `classify_tool_execution_error` 还会把它归成不可重试。
///
/// 实测实锤两例：
/// - tutor 的 `ask` 发 `"recommended":"true"` → 提问失败，选择框没弹
/// - programmer 的 `ask` 发 `{"options":{"item":[…]},"multiSelect":"false"}`
///
/// 只做**保守、由 schema 驱动**的两条纠正，绝不猜测未声明的字段：
/// 1. schema 声明 Boolean，而值是可无歧义识别的字符串/0-1 数字
///    （见 [`crate::controller::lenient_bool`]）→ 换成真 bool。
/// 2. schema 声明 Array，而值是对象且其中**恰好一个**字段是数组
///    → 解开那一层。多于一个数组时不猜，保持原样让校验照常报错。
///
/// 识别不了的值原样透传 —— 校验该报的错还是会报，模型仍能拿到反馈。
pub(crate) fn coerce_tool_input_to_schema(
    mut input: serde_json::Value,
    schema: &latte_rs_agent_tools::types::ToolInputSchema,
) -> serde_json::Value {
    use latte_rs_agent_tools::types::PropertyType;
    use serde_json::Value;

    let Some(obj) = input.as_object_mut() else {
        return input;
    };
    for (key, value) in obj.iter_mut() {
        let Some(prop) = schema.properties.get(key) else {
            continue;
        };
        match prop.property_type {
            PropertyType::Boolean if !value.is_boolean() => {
                if let Some(b) = crate::controller::lenient_bool(value) {
                    *value = Value::Bool(b);
                }
            }
            PropertyType::Array => {
                if let Value::Object(map) = value {
                    let mut arrays = map.values().filter(|v| v.is_array());
                    if let (Some(a), None) = (arrays.next(), arrays.next()) {
                        let unwrapped = a.clone();
                        *value = unwrapped;
                    }
                }
            }
            _ => {}
        }
    }

    // 3. 声明 String 的字段收到了**字符串数组**，而 schema 里存在同名复数
    //    字段且声明为 Array → 把值搬过去。
    //
    //    唯一的落点是 `read`：schema 同时有 `path: String` 与
    //    `paths: Array`，模型很容易写成 `path: ["a","b"]`。校验器没有
    //    union 类型，这会被判 Validation 失败 → 归为不可重试 → 白烧一次
    //    往返。而本次改造的全部目的就是**省往返**，在这里纠正比让模型
    //    自己发现划算。
    //
    //    仍然是 schema 驱动、不猜测：必须同时满足「K 声明 String」+
    //    「K+s 声明 Array」+「值是全字符串数组」+「K+s 尚未出现」。
    let mut moves: Vec<(String, String)> = Vec::new();
    for (key, value) in obj.iter() {
        let Some(prop) = schema.properties.get(key) else {
            continue;
        };
        if !matches!(prop.property_type, PropertyType::String) {
            continue;
        }
        let Some(items) = value.as_array() else { continue };
        if items.is_empty() || !items.iter().all(|v| v.is_string()) {
            continue;
        }
        let plural = format!("{key}s");
        let plural_is_array = schema
            .properties
            .get(&plural)
            .is_some_and(|p| matches!(p.property_type, PropertyType::Array));
        if plural_is_array && !obj.contains_key(&plural) {
            moves.push((key.clone(), plural));
        }
    }
    for (from, to) in moves {
        if let Some(v) = obj.remove(&from) {
            obj.insert(to, v);
        }
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
    // 字符串化的 JSON 数组不是路径：模型想批量读时会把
    // `["a.c:1-40", "b.h"]` 整个塞进 `path`（实测 architect / MiniMax-M3）。
    // 若在这里被当成相对路径拼上 cwd，就变成
    // `/Users/…/jemalloc/["a.c:1-40", …]` —— 开头的 `[` 不在首位了，
    // 下游 `normalize_read_path_args` 的"字符串化数组 → paths"归一化
    // 认不出它，于是报一个莫名的 "No such file or directory"，模型看不懂
    // 也改不对，白烧一整轮往返。
    if s.trim_start().starts_with('[') {
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

    #[test]
    fn tool_metadata_uses_exact_bound_session_file() {
        let cwd = std::path::Path::new("/workspace");
        let a = std::path::Path::new("/workspace/.latte/ui-sessions/a.jsonl");
        let b = std::path::Path::new("/workspace/.latte/ui-sessions/b.jsonl");
        let meta_a = tool_execution_metadata(Some(cwd), Some(a)).unwrap();
        let meta_b = tool_execution_metadata(Some(cwd), Some(b)).unwrap();
        assert_eq!(meta_a["cwd"], "/workspace");
        assert_eq!(meta_a["current_session_file"], a.to_string_lossy().as_ref());
        assert_eq!(meta_b["current_session_file"], b.to_string_lossy().as_ref());
        assert_ne!(meta_a["current_session_file"], meta_b["current_session_file"]);
        assert!(tool_execution_metadata(None, None).is_none());
    }

    /// 悬挂工具标记只命中尾部窗口：glm 实锤形态（正文断在半句、
    /// 以 </parameter></function> 收尾）触发；正文里的合法 XML 示例
    /// 与干净答复不触发。
    #[test]
    fn dangling_tool_markup_tail_detection() {
        // 实测实锤样本形态：标记在末尾
        let bad = "折中：同一函数内累计≥80，中间无≥5行纯代码的段落</parameter> </function>";
        assert!(has_dangling_tool_markup_tail(bad));
        let bad2 = "好的，我来处理<tool_call>";
        assert!(has_dangling_tool_markup_tail(bad2));
        // 干净答复
        assert!(!has_dangling_tool_markup_tail("这是一段完整的中文答复，没有任何标记。"));
        // 标记在正文前部（尾部窗口之外）→ 不误伤
        let mut mid = "示例：`</function>` 是闭合标记。\n".to_string();
        mid.push_str(&"后续正文。".repeat(100));
        assert!(!has_dangling_tool_markup_tail(&mid));
        // 空串 / 多字节字符边界不 panic
        assert!(!has_dangling_tool_markup_tail(""));
        let cjk = "汉字".repeat(300);
        assert!(!has_dangling_tool_markup_tail(&cjk));
    }

    /// 永久性错误（工具不存在 / ENOENT / 参数非法 / plan 校验失败）不重试；
    /// 瞬时错误重试一次。所有错误终态都以 tool_result 回填（协议闭环）。
    #[test]
    fn permanent_tool_errors_are_not_retried() {
        let policy = DefaultRetryPolicy;
        // 工具不存在 → ToolNotFound，不重试
        let kind = classify_tool_execution_error(&ToolError::tool_not_found("bash"));
        assert!(matches!(kind, ToolCallErrorKind::ToolNotFound { .. }));

        // 回填给模型的文本要区分「未授权」与「不存在」：
        // `edit` 在 builtin 池里（EditToolsPackage），只是被角色 allowlist
        // 过滤掉了——报 "Tool not found" 会让模型判定该能力不存在、改用
        // bash 绕路，而正确出路是 request_tool（实测实锤）。
        let detail = tool_not_found_detail("edit", "Tool not found: edit".into());
        assert!(detail.contains("未授权"), "detail = {detail}");
        assert!(detail.contains("request_tool"), "detail = {detail}");
        // 提示仍应短小：虽然 tool_result 的上限已从 256 提到
        // MAX_TOOL_ERROR_BYTES（且改成中段省略），一条"换个路径"的硬指令
        // 没有理由写长——短提示更容易被照做。
        assert!(
            detail.len() <= MAX_TOOL_ERROR_BYTES,
            "提示过长: {} 字节",
            detail.len()
        );

        // 真的不存在的工具名 → 保留原始报错，不编造未授权的说法。
        let raw = "Tool not found: definitely_not_a_tool".to_string();
        let detail = tool_not_found_detail("definitely_not_a_tool", raw.clone());
        assert_eq!(detail, raw);
        assert!(!policy.retryable(&kind));
        // ENOENT → PermanentExec，不重试
        let kind = classify_tool_execution_error(&ToolError::other(
            "stat: No such file or directory (os error 2)",
        ));
        assert!(matches!(kind, ToolCallErrorKind::PermanentExec { .. }));
        assert!(!policy.retryable(&kind));
        // 缺必填参数 → PermanentExec
        let kind = classify_tool_execution_error(&ToolError::other("path is required"));
        assert!(!policy.retryable(&kind));
        // plan 路径重叠 → PermanentExec，不能同参数重试
        let kind = classify_tool_execution_error(&ToolError::other(
            "以下任务的 paths 范围重叠，请调整使各任务范围互不重叠",
        ));
        assert!(matches!(kind, ToolCallErrorKind::PermanentExec { .. }));
        assert!(!policy.retryable(&kind));
        // plan 幻觉路径 → PermanentExec
        let kind = classify_tool_execution_error(&ToolError::other(
            "疑似幻觉路径，请用 read/search 核实后再提交",
        ));
        assert!(matches!(kind, ToolCallErrorKind::PermanentExec { .. }));
        assert!(!policy.retryable(&kind));
        // ask_human 的暂停控制流 → PermanentExec（重试会重复发 AskHuman 事件）
        let kind = classify_tool_execution_error(&ToolError::other(
            "session paused: ask_human from programmer",
        ));
        assert!(matches!(kind, ToolCallErrorKind::PermanentExec { .. }));
        assert!(!policy.retryable(&kind));
        // code_graph backend/query/protocol errors are deterministic for the
        // same invocation and must not be executed twice automatically.
        for message in [
            "code_graph backend unavailable: ast-grep missing",
            "ast-grep query failed (exit=8): invalid rule",
            "ast-grep backend protocol failure, not 0 matches",
        ] {
            let kind = classify_tool_execution_error(&ToolError::other(message));
            assert!(
                matches!(kind, ToolCallErrorKind::PermanentExec { .. }),
                "{message}"
            );
            assert!(!policy.retryable(&kind), "{message}");
        }
        // 普通执行错误（网络/5xx 类）→ Execution，重试一次
        let kind = classify_tool_execution_error(&ToolError::other("connection reset by peer"));
        assert!(matches!(kind, ToolCallErrorKind::Execution { .. }));
        assert!(policy.retryable(&kind));
    }

    /// 工具管理器熔断必须归到 `Timeout`，不能混进 `Execution`。
    /// 回归防线：`ToolCallErrorKind::Timeout` 曾经是个从未被构造过的死
    /// variant——超时和普通执行错误共用一条路径，trace 上看不出区别，
    /// 也没法给超时单独定策略。
    #[test]
    fn tool_manager_timeout_classifies_as_timeout_kind() {
        let policy = DefaultRetryPolicy;
        let kind = classify_tool_execution_error(&ToolError::timeout(
            "workflow",
            std::time::Duration::from_secs(1500),
        ));
        assert_eq!(kind, ToolCallErrorKind::Timeout, "必须归到 Timeout");
        assert_eq!(kind.label(), "Timeout", "trace 上要能看出是超时");
        assert!(policy.retryable(&kind), "工具级超时仍可重试一次");
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
            omit_max_tokens: false,
            max_tokens_field: Default::default(),
            supports_thinking: false,
            supports_vision: false,
            cost_per_million_input: 0.0,
            cost_per_million_output: 0.0,
            timeout_secs: None,
        }
    }

    // ── resolve_tool_input_against_cwd / is_relative_fs_path ─────────────

    /// 字符串化的 JSON 数组**不是**相对路径：模型想批量读时会把
    /// `["a.c:1-40","b.h"]` 整个塞进 `path`。若被当成相对路径拼上 cwd，
    /// 就变成 `/Users/…/proj/["a.c:1-40",…]` —— 开头的 `[` 不在首位，
    /// 下游 `normalize_read_path_args` 的"字符串化数组 → paths"归一化认不
    /// 出它，于是报一个莫名的 not-found，模型看不懂也改不对。
    ///
    /// 回归 2026-09-07 jemalloc 会话（architect / MiniMax-M3）实测形态：
    /// `path='/Users/zhouguodong/Documents/github/jemalloc/["/Users/…/a.md:1-40", …]'`
    #[test]
    fn json_array_in_path_is_not_treated_as_a_relative_path() {
        assert!(!is_relative_fs_path(r#"["a.c:1-40", "b.h"]"#));
        assert!(!is_relative_fs_path(r#"  ["a.c"]"#), "前置空白也要认");
        // 仍然是相对路径的形状不受影响
        assert!(is_relative_fs_path("src/foo.rs"));
        assert!(is_relative_fs_path("a.c"));
        // 方括号出现在中间不算（真实文件名可以带方括号）
        assert!(is_relative_fs_path("docs/note[1].md"));

        let cwd = std::path::Path::new("/tmp/proj");
        let out = resolve_tool_input_against_cwd(
            serde_json::json!({ "path": r#"["a.c", "b.h"]"# }),
            cwd,
        );
        assert_eq!(
            out["path"].as_str().unwrap(),
            r#"["a.c", "b.h"]"#,
            "不得被拼上 cwd，否则下游归一化认不出"
        );
    }

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

    /// 402（余额不足）必须**长冷却**，不能按普通 4xx 的 5s 处理。
    ///
    /// 余额不足在本次会话内不会自愈——充值是人的动作。5s 冷却意味着每轮
    /// 失败切换都要再白烧它一次：实测 jemalloc 那次 61 分钟的会话里，
    /// `deepseek-v4-flash: 402 Insufficient Balance` 出现在每一次全链
    /// 失败的日志里，每次都占掉一整个往返。
    #[test]
    fn test_cooldown_for_402_is_effectively_session_long() {
        let d = cooldown_for_error(&AiError::Api {
            status: 402,
            message: "Insufficient Balance".into(),
        })
        .expect("402 应有冷却");
        // 远超任何正常会话时长；但不是永久禁用（新进程重新试一次）。
        assert!(
            d >= Duration::from_secs(3600),
            "402 的冷却应远超会话时长，实际 {d:?}"
        );
        // 对照：其余 4xx 仍是短冷却（可能是模型 id 写错，下一个模型
        // 换个 id 格式就能成，值得快速重试）。
        for status in [400u16, 401, 403, 404, 422] {
            let other = cooldown_for_error(&AiError::Api {
                status,
                message: "bad".into(),
            });
            assert!(
                other.is_some_and(|o| o < Duration::from_secs(600)),
                "status {status} 不该跟 402 一样长冷却"
            );
        }
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
    fn test_cooldown_for_4xx_other_than_429_is_60s() {
        // 400/401/403/404：模型 id 不存在或 payload 不被该模型接受，
        // 60s 冷却后链落到下一个模型（不同 id / 不同厂商）。cd92331
        // 把 4xx 从 5s 提到 60s（流式 400 沿模型链 fallback），本测试
        // 对齐现行语义。
        for status in [400u16, 401, 403, 404] {
            let d = cooldown_for_error(&AiError::Api {
                status,
                message: "bad request".into(),
            });
            assert_eq!(d, Some(Duration::from_secs(60)), "status={status}");
        }
    }

    #[test]
    fn test_cooldown_for_auth_error_falls_through_quickly() {
        // 模型各自的 key 失效是模型局部问题：短冷却后链落到下一个
        // 模型，而不是整链陪葬（glm 中转 key 过期 ≠ deepseek 也坏）。
        let d = cooldown_for_error(&AiError::Auth("bad key".into()));
        assert_eq!(d, Some(Duration::from_secs(5)));
    }
    #[test]
    fn test_cooldown_for_stream_error_falls_through_chain() {
        // TTFB 首事件超时 / idle 超时多为厂商侧卡顿：应冷却后跟链走，
        // 而不是作为「本地不可规避错误」直接硬失败整个 turn。
        let d = cooldown_for_error(&AiError::Stream("等待首个事件超时（100s 无响应）".into()));
        assert_eq!(d, Some(Duration::from_secs(10)));
    }

    // ─── context overflow 检测与待发消息裁剪 ───────────────────────────

    #[test]
    fn test_context_overflow_detection() {
        let overflow = AgentError::ModelsUnavailable {
            tried: vec!["m1".into()],
            failures: vec![
                ("m1".into(), "API error: 400 - invalid params, context window exceeds limit".into()),
            ],
            next_retry_in: None,
        };
        assert!(failures_include_context_overflow(&overflow));

        let plain_5xx = AgentError::ModelsUnavailable {
            tried: vec!["m1".into()],
            failures: vec![("m1".into(), "API error: 502 - Bad Gateway".into())],
            next_retry_in: None,
        };
        assert!(!failures_include_context_overflow(&plain_5xx));

        let other = AgentError::InvalidParam("context window".into());
        assert!(!failures_include_context_overflow(&other));
    }

    #[test]
    fn test_slim_oversized_messages() {
        use latte_ai::models::Role as MsgRole;
        let mut msgs = vec![
            Message::system("sys".repeat(OVERSIZED_MSG_CHARS * 2)),
            Message::user("short"),
            Message::tool_result("t1", "x".repeat(OVERSIZED_MSG_CHARS * 3)),
        ];
        let n = slim_oversized_messages(&mut msgs);
        assert_eq!(n, 1, "只有超长 tool 消息应被裁剪");
        // system 消息不动
        assert!(msgs[0].as_text().chars().count() > OVERSIZED_MSG_CHARS);
        assert_eq!(msgs[0].role, MsgRole::System);
        // 短消息不动
        assert_eq!(msgs[1].as_text(), "short");
        // 超长消息压到上限附近并带标记。压缩是**中段省略**：头尾都必须
        // 还在（工具结果的结论/note 在末尾，砍尾等于把出口信息扔了）。
        let slimmed = msgs[2].as_text();
        assert!(
            slimmed.chars().count() <= OVERSIZED_MSG_CHARS + 60,
            "{} 字符",
            slimmed.chars().count()
        );
        assert!(slimmed.contains("中段省略"), "{slimmed}");
        let (head, tail) = slimmed.split_once("\n…[中段省略").expect("应有中段标记");
        assert_eq!(head.chars().count(), OVERSIZED_MSG_CHARS * 6 / 10, "头部预算 60%");
        let tail = tail.split_once("]…\n").expect("标记应闭合").1;
        assert_eq!(tail.chars().count(), OVERSIZED_MSG_CHARS * 4 / 10, "尾部预算 40%");
    }

    // ─── compact_stale_tool_results tests ───────────────────────────
    //
    // 回归 2026-09-07 jemalloc 会话的 token 放大：工具循环每轮重传全部
    // 历史，一个 reviewer 子会话 70 次调用（返回内容合计 ≈47K token）
    // 烧掉 1.79M 输入 token；整场 22.19M 里唯一内容仅 ≈0.97M（23 倍）。

    /// 只压陈旧的、只压大的；最近 N 条与小结果一律不动。
    #[test]
    fn compact_stale_tool_results_keeps_recent_and_small_verbatim() {
        use latte_ai::models::Role as MsgRole;
        let big = "A".repeat(STALE_TOOL_RESULT_FLOOR * 3);
        let small = "ok".repeat(10);
        let mut msgs = vec![
            Message::system("sys"),
            Message::user("go"),
            Message::tool_result("t1", &big), // 陈旧 + 大 → 压
            Message::tool_result("t2", &small), // 陈旧但小 → 不动
            Message::tool_result("t3", &big), // 陈旧 + 大 → 压
            Message::tool_result("t4", &big), // 最近 2 条 → 不动
            Message::tool_result("t5", &big),
        ];
        let n = compact_stale_tool_results(&mut msgs, 2);
        assert_eq!(n, 2, "只有 t1/t3 该被压缩");

        assert_eq!(msgs[0].as_text(), "sys", "system 不动");
        assert_eq!(msgs[1].as_text(), "go", "user 不动");
        assert!(msgs[2].as_text().contains("中段省略"), "t1 应被压缩");
        assert!(
            msgs[2].as_text().chars().count() <= STALE_TOOL_RESULT_CHARS + 60,
            "{}",
            msgs[2].as_text().chars().count()
        );
        assert_eq!(msgs[3].as_text(), small, "小结果保持逐字");
        assert!(msgs[4].as_text().contains("中段省略"), "t3 应被压缩");
        assert_eq!(msgs[5].as_text(), big, "最近 2 条保持逐字");
        assert_eq!(msgs[6].as_text(), big, "最近 2 条保持逐字");
        // 消息条数与角色顺序不变 —— tool_call ↔ tool_result 的配对不能破。
        assert_eq!(msgs.len(), 7);
        assert_eq!(
            msgs.iter().filter(|m| m.role == MsgRole::Tool).count(),
            5,
            "绝不删消息，否则 provider 因 tool_call 无配对结果直接拒请求"
        );
    }

    /// 压缩必须**留头留尾**：换成占位符会让模型认不出读过什么，转头
    /// 重读一遍，省下的往返又还回去。
    #[test]
    fn compact_stale_tool_results_preserves_head_and_tail() {
        let body = format!(
            "src/jemalloc.c:2743 起是 je_malloc 的快路径{}结构摘要：共 4476 行，顶层 87 个函数",
            "填充".repeat(STALE_TOOL_RESULT_FLOOR)
        );
        let mut msgs = vec![
            Message::tool_result("t1", &body),
            Message::tool_result("t2", "recent"),
        ];
        assert_eq!(compact_stale_tool_results(&mut msgs, 1), 1);
        let out = msgs[0].as_text();
        assert!(
            out.starts_with("src/jemalloc.c:2743"),
            "头部要留，模型才认得出这是哪份产出: {}",
            out.chars().take(60).collect::<String>()
        );
        assert!(
            out.ends_with("结构摘要：共 4476 行，顶层 87 个函数"),
            "尾部要留（read 的结构摘要 / code_graph 的收窄提示都在末尾）: {}",
            out.chars().rev().take(40).collect::<String>()
        );
    }

    /// 幂等：已压过的消息再压一次不变（工具循环每轮都会调它）。
    #[test]
    fn compact_stale_tool_results_is_idempotent() {
        let mut msgs = vec![
            Message::tool_result("t1", "B".repeat(STALE_TOOL_RESULT_FLOOR * 5)),
            Message::tool_result("t2", "recent"),
        ];
        assert_eq!(compact_stale_tool_results(&mut msgs, 1), 1);
        let once = msgs[0].as_text().to_string();
        assert_eq!(
            compact_stale_tool_results(&mut msgs, 1),
            0,
            "第二次不该再压（已在预算内）"
        );
        assert_eq!(msgs[0].as_text(), once);
    }

    /// 工具结果条数不超过 keep_recent 时什么都不做。
    #[test]
    fn compact_stale_tool_results_noop_when_under_threshold() {
        let big = "C".repeat(STALE_TOOL_RESULT_FLOOR * 3);
        let mut msgs = vec![
            Message::user("go"),
            Message::tool_result("t1", &big),
            Message::tool_result("t2", &big),
        ];
        assert_eq!(compact_stale_tool_results(&mut msgs, 6), 0);
        assert_eq!(msgs[1].as_text(), big);
        assert_eq!(msgs[2].as_text(), big);
    }

    /// UTF-8 安全：压缩点落在多字节字符中间不能 panic / 产生乱码。
    #[test]
    fn compact_stale_tool_results_is_utf8_safe() {
        let body = "中文内容😀".repeat(STALE_TOOL_RESULT_FLOOR);
        let mut msgs = vec![
            Message::tool_result("t1", &body),
            Message::tool_result("t2", "recent"),
        ];
        assert_eq!(compact_stale_tool_results(&mut msgs, 1), 1);
        let out = msgs[0].as_text();
        assert!(out.is_char_boundary(0));
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
        assert!(out.contains("中段省略"));
    }

    /// 量化收益：按实测会话的形状（70 次调用、返回内容合计 188,498 字符）
    /// 模拟一个 turn 的累计重传量，确认压缩确实把放大倍数打下来。
    #[test]
    fn compact_stale_tool_results_cuts_resend_volume() {
        // 实测：70 次工具调用 / 188,498 字符 → 平均每条 ≈2693 字符。
        const CALLS: usize = 70;
        const PER_RESULT: usize = 2693;

        // 模拟工具循环：第 r 轮发出的 payload = 前 r 条工具结果之和。
        // 累计重传量 = Σ(r=1..CALLS) payload(r)。
        let resend_total = |compact: bool| -> usize {
            let mut msgs: Vec<Message> = vec![Message::user("go")];
            let mut total = 0usize;
            for i in 0..CALLS {
                msgs.push(Message::tool_result(
                    format!("t{i}"),
                    "x".repeat(PER_RESULT),
                ));
                if compact {
                    compact_stale_tool_results(&mut msgs, CONTEXT_KEEP_RECENT_TOOL_RESULTS);
                }
                total += msgs.iter().map(|m| m.as_text().chars().count()).sum::<usize>();
            }
            total
        };

        let before = resend_total(false);
        let after = resend_total(true);
        let unique = CALLS * PER_RESULT;
        println!(
            "唯一内容 {unique} 字符；累计重传 压缩前 {before} / 压缩后 {after}\
             （放大 {:.1}x → {:.1}x，省 {:.0}%）",
            before as f64 / unique as f64,
            after as f64 / unique as f64,
            100.0 * (1.0 - after as f64 / before as f64),
        );
        // 缺省参数（keep_recent=6 / budget=900）下实测省 ~55%：陈旧的每条
        // 2693 → 900 字符，最近 6 条保持逐字，早期轮次本来就没多少可压。
        // 这里钉一条保守下界，避免将来把预算调大到收益消失还没人发现。
        assert!(
            after * 2 < before,
            "压缩后累计重传量应至少腰斩：before={before} after={after}"
        );
        // 压缩前的放大倍数应与实测量级吻合（那个子会话是 38x），确认这个
        // 模拟模型没跑偏。
        assert!(
            before / unique >= 20,
            "模拟的放大倍数 {}x 与实测量级不符",
            before / unique
        );
    }

    /// 端到端：压缩必须真的作用在**发出去的请求体**上，而不只是函数
    /// 单测通过。让模型连发多轮工具调用，然后检查最后一次请求里早期的
    /// 工具结果已被压缩、最近几条仍逐字。
    #[tokio::test]
    async fn stale_tool_results_are_compacted_in_outgoing_requests() {
        use latte_rs_agent_tools::prelude::create_tool_manager;
        use latte_rs_agent_tools::types::{
            SchemaType, SharedToolHandler, Tool, ToolInputSchema, ToolManager as _,
        };
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        // 每条工具结果都远超压缩门槛，便于断言。
        const BIG: usize = STALE_TOOL_RESULT_FLOOR * 2;
        const ROUNDS: usize = 10;

        // 固定返回一大段文本的工具。
        let make_tm = || {
            let handler: SharedToolHandler = Arc::new(move |_input, _ctx| {
                Box::pin(async move { Ok(serde_json::json!({ "text": "Z".repeat(BIG) })) })
            });
            let schema = ToolInputSchema {
                schema_type: SchemaType,
                properties: Default::default(),
                required: None,
                additional_properties: None,
                defs: None,
            };
            let tm = create_tool_manager();
            tm.register(
                Tool::builder("echo_big", "返回一大段文本", schema, handler).build(),
                None,
            );
            tm
        };

        // 前 ROUNDS 次响应各发一个工具调用，最后一次不带工具调用收尾。
        let make_server = |rounds: usize| async move {
            let server = wiremock::MockServer::start().await;
            for i in 0..rounds {
                server
                    .register(
                        Mock::given(method("POST"))
                            .and(path("/chat/completions"))
                            .respond_with(ResponseTemplate::new(200).set_body_string(
                                openai_completion_body(
                                    "",
                                    vec![serde_json::json!({
                                        "id": format!("call_{i}"),
                                        "type": "function",
                                        "function": {
                                            "name": "echo_big",
                                            "arguments": format!("{{\"n\":{i}}}")
                                        }
                                    })],
                                ),
                            ))
                            .up_to_n_times(1),
                    )
                    .await;
            }
            server
                .register(
                    Mock::given(method("POST"))
                        .and(path("/chat/completions"))
                        .respond_with(
                            ResponseTemplate::new(200)
                                .set_body_string(openai_completion_body("收尾", vec![])),
                        ),
                )
                .await;
            server
        };

        std::env::remove_var("LATTE_AGENT_CONTEXT_COMPACT");
        std::env::remove_var("LATTE_AGENT_CONTEXT_KEEP_RECENT");
        std::env::remove_var("LATTE_AGENT_CONTEXT_STALE_CHARS");

        let server = make_server(ROUNDS).await;
        let agent = Agent::new_with_chain(
            "compact-e2e".into(),
            test_role(),
            vec![model_at(&server, "stub")],
            GenerateParams::default(),
        )
        .unwrap();
        let mut runner = AgentRunner::new_with_tools(agent, make_tm());
        let _ = runner.run_turn(&[Message::user("连续读")], None).await.unwrap();

        let reqs = server.received_requests().await.expect("mock 应记录请求");
        assert!(reqs.len() > CONTEXT_KEEP_RECENT_TOOL_RESULTS, "轮数要够才压得到: {}", reqs.len());
        let last = String::from_utf8_lossy(&reqs[reqs.len() - 1].body).to_string();

        assert!(
            last.contains("中段省略"),
            "最后一次请求应含被压缩的陈旧工具结果"
        );
        let verbatim = last.matches(&"Z".repeat(BIG)).count();
        assert!(
            verbatim <= CONTEXT_KEEP_RECENT_TOOL_RESULTS,
            "逐字保留的大块应不超过 keep_recent={}，实际 {verbatim}",
            CONTEXT_KEEP_RECENT_TOOL_RESULTS
        );
        assert!(verbatim >= 1, "最近的结果必须逐字保留，实际 {verbatim}");

        // 关掉开关后行为回退：所有结果逐字重传，一处压缩都不该有。
        std::env::set_var("LATTE_AGENT_CONTEXT_COMPACT", "0");
        let server2 = make_server(ROUNDS).await;
        let agent2 = Agent::new_with_chain(
            "compact-off".into(),
            test_role(),
            vec![model_at(&server2, "stub")],
            GenerateParams::default(),
        )
        .unwrap();
        let mut runner2 = AgentRunner::new_with_tools(agent2, make_tm());
        let _ = runner2.run_turn(&[Message::user("连续读")], None).await.unwrap();
        let reqs2 = server2.received_requests().await.unwrap();
        let last2 = String::from_utf8_lossy(&reqs2[reqs2.len() - 1].body).to_string();
        assert!(!last2.contains("中段省略"), "关闭开关后不应有任何压缩");
        assert!(
            last2.matches(&"Z".repeat(BIG)).count() > CONTEXT_KEEP_RECENT_TOOL_RESULTS,
            "关闭后所有结果都该逐字重传"
        );
        std::env::remove_var("LATTE_AGENT_CONTEXT_COMPACT");
    }

    /// 开关：`LATTE_AGENT_CONTEXT_COMPACT=0` 关闭，`KEEP_RECENT` 可调且钳位。
    #[test]
    fn context_compact_env_knobs() {
        // 该测试改进程级环境变量，与其他用例串行跑（同一测试内自己清理）。
        std::env::remove_var("LATTE_AGENT_CONTEXT_COMPACT");
        assert!(context_compact_enabled(), "缺省应为开");
        for off in ["0", "false", "no", "off", "OFF", " off "] {
            std::env::set_var("LATTE_AGENT_CONTEXT_COMPACT", off);
            assert!(!context_compact_enabled(), "{off:?} 应关闭");
        }
        std::env::set_var("LATTE_AGENT_CONTEXT_COMPACT", "1");
        assert!(context_compact_enabled());
        std::env::remove_var("LATTE_AGENT_CONTEXT_COMPACT");

        std::env::remove_var("LATTE_AGENT_CONTEXT_KEEP_RECENT");
        assert_eq!(context_keep_recent(), CONTEXT_KEEP_RECENT_TOOL_RESULTS);
        std::env::set_var("LATTE_AGENT_CONTEXT_KEEP_RECENT", "3");
        assert_eq!(context_keep_recent(), 3);
        // 0 会把当轮刚拿到的结果也压掉 → 钳到 1
        std::env::set_var("LATTE_AGENT_CONTEXT_KEEP_RECENT", "0");
        assert_eq!(context_keep_recent(), 1);
        std::env::set_var("LATTE_AGENT_CONTEXT_KEEP_RECENT", "999");
        assert_eq!(context_keep_recent(), 64);
        std::env::set_var("LATTE_AGENT_CONTEXT_KEEP_RECENT", "不是数字");
        assert_eq!(context_keep_recent(), CONTEXT_KEEP_RECENT_TOOL_RESULTS);
        std::env::remove_var("LATTE_AGENT_CONTEXT_KEEP_RECENT");

        std::env::remove_var("LATTE_AGENT_CONTEXT_STALE_CHARS");
        assert_eq!(context_stale_chars(), STALE_TOOL_RESULT_CHARS);
        // 默认预算下的压缩门槛应与常量一致（900 * 4/3 = 1200）
        assert_eq!(context_stale_chars() * 4 / 3, STALE_TOOL_RESULT_FLOOR);
        std::env::set_var("LATTE_AGENT_CONTEXT_STALE_CHARS", "400");
        assert_eq!(context_stale_chars(), 400);
        std::env::set_var("LATTE_AGENT_CONTEXT_STALE_CHARS", "10");
        assert_eq!(context_stale_chars(), 200, "下钳到 200");
        std::env::set_var("LATTE_AGENT_CONTEXT_STALE_CHARS", "99999");
        assert_eq!(context_stale_chars(), 8_000, "上钳到 8000");
        std::env::remove_var("LATTE_AGENT_CONTEXT_STALE_CHARS");
    }

    /// 工具错误详情：中段省略而非截头。回归实测事故——旧实现 256 字节
    /// 截头，把 `plan` 的冲突清单（5855 字 / 45 对）和 `code_graph` 的
    /// "可用 kind：…" 修复指引都砍在尾部，模型看不全就修不动。
    #[test]
    fn tool_error_detail_is_compacted_middle_out_not_head_truncated() {
        // 短错误原样透传。
        let short = "code_graph: 不支持的 kind='fn'。可用 kind：function, struct";
        assert_eq!(compact_middle_out(short, MAX_TOOL_ERROR_BYTES), short);

        // 长错误：头（错误分类）与尾（修复指引）都必须留下。
        let head_marker = "PLAN_REJECTED: paths 重叠";
        let tail_marker = "修法：把 involved_paths 改成 paths";
        let long = format!("{head_marker}{}{tail_marker}", "冲突对; ".repeat(2000));
        assert!(long.len() > MAX_TOOL_ERROR_BYTES * 2, "样本要够长");
        let out = compact_middle_out(&long, MAX_TOOL_ERROR_BYTES);
        assert!(out.starts_with(head_marker), "头部（错误分类）必须保留: {out:.80}");
        assert!(out.ends_with(tail_marker), "尾部（修复指引）必须保留 —— 这正是旧实现丢掉的");
        assert!(out.contains("中段省略"), "必须告知模型内容被压缩过");
        // 预算：正文 ≤ 上限，标记是常数级开销。
        assert!(
            out.len() <= MAX_TOOL_ERROR_BYTES + 64,
            "压缩后 {} 字节超预算",
            out.len()
        );
        // 新上限至少要装得下那 5855 字节的实测样本的头尾两端。
        assert!(MAX_TOOL_ERROR_BYTES >= 2048, "256 字节的旧上限已被实测证伪");
    }

    /// 多字节字符不得被切坏（头尾两端都要对齐字符边界）。
    #[test]
    fn compact_middle_out_is_utf8_safe() {
        let text = "中".repeat(4000); // 每字 3 字节
        let out = compact_middle_out(&text, 1000);
        assert!(out.contains("中段省略"));
        assert!(out.starts_with('中'), "{out:.20}");
        assert!(out.ends_with('中'));
        // 能正常按字符迭代 = 没有非法字节序列。
        assert!(out.chars().count() > 0);
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

    /// 构造 OpenAI streaming SSE 响应体（多个 delta chunk + [DONE]）。
    /// `chunks` 按序拼成独立 SSE event，模拟模型逐 token 输出。
    fn openai_sse_body(chunks: &[&str]) -> String {
        let mut out = String::new();
        for c in chunks {
            out.push_str("data: ");
            out.push_str(&serde_json::json!({
                "id": "chatcmpl-test",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "test",
                "choices": [{
                    "index": 0,
                    "delta": { "content": c },
                    "finish_reason": None::<String>
                }]
            }).to_string());
            out.push_str("\n\n");
        }
        // 末尾 chunk：finish_reason + usage
        out.push_str("data: ");
        out.push_str(&serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "test",
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
        }).to_string());
        out.push_str("\n\n");
        out.push_str("data: [DONE]\n\n");
        out
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
            omit_max_tokens: false,
            max_tokens_field: Default::default(),
            supports_thinking: false,
            supports_vision: false,
            cost_per_million_input: 0.0,
            cost_per_million_output: 0.0,
            timeout_secs: None,
        }
    }

    /// 端到端：`run_turn` 真的把**模型目录里配的** `max_tokens` 发到线上。
    ///
    /// 这条钉的是 2026-09-07 jemalloc 会话那个 bug 的完整链路：目录里
    /// `max_tokens = 384000`，而 529 次请求下发的全是 `None` ——
    /// `build_openai_request` 直接透传 `Option` + `skip_serializing_if`，
    /// 字段整个被省略，于是上限由厂商默认值说话，architect 撞上 8192 截断、
    /// 半个 JSON 穿给下游，整条 20 分钟流水线判 failed。
    ///
    /// 只测 `effective_max_tokens` 不够 —— 那个 bug 恰恰不在决策逻辑里，
    /// 而在「决策出来的值有没有真的进请求体」。所以这里从 `run_turn` 入口
    /// 打到 mock server 收到的字节。
    #[tokio::test]
    async fn run_turn_sends_the_configured_max_tokens_on_the_wire() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .set_body_string(openai_completion_body("好", vec![])),
                    ),
            )
            .await;

        // 目录配 384000 —— 实测配置里的真实值
        let mut model = model_at(&server, "stub");
        model.max_tokens = 384_000;
        model.context_window = 1_000_000;

        let agent = Agent::new_with_chain(
            "max-tokens-e2e".into(),
            test_role(),
            vec![model],
            GenerateParams::default(),
        )
        .unwrap();
        let mut runner = AgentRunner::new(agent);
        runner.run_turn(&[Message::user("你好")], None).await.unwrap();

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1, "应恰好一次请求");
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(
            body.get("max_tokens").and_then(|v| v.as_u64()),
            Some(384_000),
            "目录里配的上限必须原样出现在请求体里（曾经这里是 None）: {body}"
        );
    }

    /// 目录没配（`0`）时该字段**完全不出现**，而不是发 `null` 或发一个
    /// 我们替使用者拍的数 —— 由厂商默认值说话。
    #[tokio::test]
    async fn run_turn_omits_max_tokens_when_catalog_leaves_it_unset() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .set_body_string(openai_completion_body("好", vec![])),
                    ),
            )
            .await;

        let mut model = model_at(&server, "stub");
        model.max_tokens = 0;

        let agent = Agent::new_with_chain(
            "max-tokens-unset".into(),
            test_role(),
            vec![model],
            GenerateParams::default(),
        )
        .unwrap();
        let mut runner = AgentRunner::new(agent);
        runner.run_turn(&[Message::user("你好")], None).await.unwrap();

        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert!(
            body.get("max_tokens").is_none(),
            "未配置时应省略字段，不能发 null: {body}"
        );
    }

    /// 模型热更新：resolver 代际变了以后，maybe_reload_models 重建
    /// model chain（run_turn 入口与「模型不可用暂停→恢复」重试前都调）。
    #[test]
    fn hot_reload_swaps_model_chain_on_generation_bump() {
        use crate::config::{AgentConfig, ModelCatalog, ModelDef};
        fn cfg_with(id: &str) -> AgentConfig {
            AgentConfig {
                advisor: Default::default(),
                models: ModelCatalog {
                    models: vec![ModelDef {
                        name: id.into(),
                        api: "openai".into(),
                        provider: "test".into(),
                        base_url: "http://localhost:1".into(),
                        api_key: "k".into(),
                        context_window: 32000,
                        max_tokens: 4096,
                        omit_max_tokens: false,
                        max_tokens_field: Default::default(),
                        supports_thinking: false,
                        supports_vision: false,
                        supports_image_generation: false,
                        cost_per_million_input: Some(0.0),
                        cost_per_million_output: Some(0.0),
                        tier: None,
                        timeout_secs: None,
                    }],
                    tiers: None,
                    role_tiers: None,
                },
                roles: Default::default(),
            }
        }
        let resolver = std::sync::Arc::new(
            crate::model_resolver::ModelResolver::from_config(&cfg_with("m-old")).unwrap(),
        );
        let agent = Agent::new_with_chain(
            "t".into(),
            test_role(),
            vec![test_model()],
            GenerateParams::default(),
        )
        .unwrap();
        let mut runner = AgentRunner::new(agent).with_model_hot_reload(
            resolver.clone(),
            ModelTier::Standard,
            vec!["m-old".to_string()],
        );
        // 代际未变：不动（test_model 的 id 与 m-old 不同，正好用来区分
        // 「没重建」和「重建了」）。
        runner.maybe_reload_models();
        assert_eq!(runner.agent.model_id, "test-model");
        // 代际变了：按新配置重新解析（chain 里的 m-old 已不存在 →
        // 回退 tier 解析到 m-new），链重建。
        resolver.reload_from_config(&cfg_with("m-new")).unwrap();
        runner.maybe_reload_models();
        assert_eq!(runner.agent.model_id, "m-new");
        // 再次调用：代际已同步，不重复重建。
        runner.maybe_reload_models();
        assert_eq!(runner.agent.model_id, "m-new");
    }

    /// 角色编辑器保存新 model_chain：resolver 快照里的角色指派优先于
    /// runner 构建时固化的 chain_ids。
    #[test]
    fn hot_reload_prefers_role_assignment_from_resolver() {
        use crate::config::{AgentConfig, ModelCatalog, ModelDef};
        fn model_def(id: &str) -> ModelDef {
            ModelDef {
                name: id.into(),
                api: "openai".into(),
                provider: "test".into(),
                base_url: "http://localhost:1".into(),
                api_key: "k".into(),
                context_window: 32000,
                max_tokens: 4096,
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
        fn cfg(model_id: &str, role_chain: Vec<&str>) -> AgentConfig {
            let mut roles = std::collections::HashMap::new();
            roles.insert(
                "t".to_string(),
                crate::role::RoleTemplate {
                    id: "t".into(),
                    name: "t".into(),
                    category: "engineering".into(),
                    model_tier: "standard".into(),
                    model_chain: role_chain.into_iter().map(|s| s.to_string()).collect(),
                    prompt_file: None,
                    temperature: None,
                    tools: vec![],
                    icon: String::new(),
                    skills: vec![],
                    code_paths: vec![],
                    description: String::new(),
                },
            );
            AgentConfig {
                advisor: Default::default(),
                models: ModelCatalog {
                    models: vec![model_def(model_id)],
                    tiers: None,
                    role_tiers: None,
                },
                roles,
            }
        }
        let resolver = std::sync::Arc::new(
            crate::model_resolver::ModelResolver::from_config(&cfg("m-old", vec!["m-old"]))
                .unwrap(),
        );
        let agent = Agent::new_with_chain(
            "t".into(),
            test_role(),
            vec![test_model()],
            GenerateParams::default(),
        )
        .unwrap();
        let mut runner = AgentRunner::new(agent).with_model_hot_reload(
            resolver.clone(),
            ModelTier::Standard,
            vec!["m-old".to_string()],
        );
        // 角色编辑器保存：chain 换成 m-new。runner 的 chain_ids 还是
        // 构建时的 ["m-old"]，但 reload 必须采用 resolver 里的新指派。
        resolver
            .reload_from_config(&cfg("m-new", vec!["m-new"]))
            .unwrap();
        runner.maybe_reload_models();
        assert_eq!(runner.agent.model_id, "m-new");
    }

    /// 模型全链不可用 + 挂了 session 暂停门 → 自动暂停（带原因），
    /// 用户「继续」（resume）后返回 true 让调用方重试。
    #[tokio::test]
    async fn models_unavailable_auto_pauses_and_resumes_for_retry() {
        let agent = Agent::new_with_chain(
            "a".into(),
            test_role(),
            vec![test_model()],
            GenerateParams::default(),
        )
        .unwrap();
        let gate = crate::pause_gate::AgentPauseGate::new("session");
        let runner = AgentRunner::new(agent).with_agent_pause_gate(gate.clone());
        let err = AgentError::ModelsUnavailable {
            tried: vec!["m1".into()],
            failures: vec![("m1".into(), "429 rate limited".into())],
            next_retry_in: None,
        };

        // 80ms 后模拟用户点「继续」。
        let g2 = gate.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            g2.resume();
        });
        let retry = runner.pause_wait_model_unavailable(&err, 0).await;
        assert!(retry, "resume 后应返回 true（重试）");
        assert!(!gate.is_paused(), "resume 后门已放开");
    }

    /// 无暂停门 → 不等人，直接 false（调用方原样抛错，保持旧行为）。
    #[tokio::test]
    async fn models_unavailable_without_gate_fails_fast() {
        let agent = Agent::new_with_chain(
            "a".into(),
            test_role(),
            vec![test_model()],
            GenerateParams::default(),
        )
        .unwrap();
        let runner = AgentRunner::new(agent);
        let err = AgentError::ModelsUnavailable {
            tried: vec!["m1".into()],
            failures: vec![],
            next_retry_in: None,
        };
        assert!(!runner.pause_wait_model_unavailable(&err, 0).await);
    }

    /// 自动暂停期间原因可读（Paused 事件的数据源）。
    #[tokio::test]
    async fn models_unavailable_pause_carries_reason() {
        let agent = Agent::new_with_chain(
            "a".into(),
            test_role(),
            vec![test_model()],
            GenerateParams::default(),
        )
        .unwrap();
        let gate = crate::pause_gate::AgentPauseGate::new("session");
        let runner = AgentRunner::new(agent).with_agent_pause_gate(gate.clone());
        let err = AgentError::ModelsUnavailable {
            tried: vec!["m1".into()],
            failures: vec![("m1".into(), "500 oops".into())],
            next_retry_in: None,
        };
        let g2 = gate.clone();
        tokio::spawn(async move {
            // 等 gate 被 engage 后断言原因，再 resume 放行。
            while !g2.is_paused() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let reason = g2.pause_reason().unwrap_or_default();
            assert!(reason.contains("模型不可用"), "{reason}");
            assert!(reason.contains("m1"), "{reason}");
            g2.resume();
        });
        assert!(runner.pause_wait_model_unavailable(&err, 0).await);
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

    /// 空 completion（200 但无文本、无 tool_calls）按模型局部失败处理：
    /// 短冷却 + 跟链走，而不是把空串当成功结果上交（日志事故：architect
    /// 空返回 → workflow 直接判死）。
    #[tokio::test]
    async fn test_chat_falls_back_on_empty_completion() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let primary = wiremock::MockServer::start().await;
        primary
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .set_body_string(openai_completion_body("", vec![])),
                    ),
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
            .chat(&[Message::user("hi")], None, WaitPolicy::NoWait)
            .await
            .expect("空 completion 应 fallback 到第二模型");
        assert_eq!(resp.content, "fallback won");
        assert!(
            !agent.model_chain[0].is_available(),
            "返回空 completion 的模型应被短冷却"
        );
    }

    /// is_empty_completion：空文本 + 无 tool_calls 才算空；带 tool_calls
    /// 的空文本是正常的 tool round。
    #[test]
    fn test_is_empty_completion() {
        use latte_ai::models::ToolCall;
        let empty = Completion {
            content: String::new(),
            content_parts: vec![],
            tool_calls: vec![],
            stop_reason: "stop".into(),
            usage: TokenUsage::default(),
        };
        assert!(is_empty_completion(&empty));

        let tool_round = Completion {
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "read".into(),
                arguments: serde_json::json!({}),
                arguments_raw: None,
                arguments_parse_error: None,
            }],
            ..empty
        };
        assert!(!is_empty_completion(&tool_round));
    }

    /// 慢调用提示：非流式 chat 超过阈值未返回 → sink 恰好收到一次
    /// ModelCallSlow；调用本身不被打断，慢响应最终正常返回。
    #[tokio::test]
    async fn test_slow_model_call_emits_notice() {
        use crate::trace::{TraceEvent, TraceSink};
        use std::sync::Arc;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        struct LocalVecSink(parking_lot::Mutex<Vec<TraceEvent>>);
        impl TraceSink for LocalVecSink {
            fn emit(&self, e: TraceEvent) {
                self.0.lock().push(e);
            }
        }

        // 阈值缩到 1s（进程级 env，需串行）。
        let _guard = crate::test_util::ENV_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap();
        std::env::set_var("LATTE_AGENT_SLOW_CALL_NOTICE_SECS", "1");

        let s = wiremock::MockServer::start().await;
        s.register(
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_delay(Duration::from_millis(2500))
                        .set_body_string(openai_completion_body("slow reply", vec![])),
                ),
        )
        .await;

        let agent = Agent::new_with_chain(
            "a".into(),
            test_role(),
            vec![model_at(&s, "slow-model")],
            GenerateParams::default(),
        )
        .unwrap();
        let sink = Arc::new(LocalVecSink(parking_lot::Mutex::new(vec![])));
        let mut runner = AgentRunner::new(agent).with_sink(sink.clone() as Arc<dyn TraceSink>);

        let out = runner
            .run_turn(&[Message::user("hi")], None)
            .await
            .unwrap();
        std::env::remove_var("LATTE_AGENT_SLOW_CALL_NOTICE_SECS");
        drop(_guard);

        assert_eq!(out, "slow reply");
        let events = sink.0.lock();
        let slows = events
            .iter()
            .filter(|e| matches!(e, TraceEvent::ModelCallSlow { .. }))
            .count();
        assert_eq!(slows, 1, "应恰好发一次慢调用提示，events: {}", events.len());
    }

    /// 本轮 user 输入在模型调用前提交：成功时 prompt/context 都只能出现
    /// 一次，不能因「提前提交 + 旧尾部提交」或额外 extend 而重复。
    #[tokio::test]
    async fn turn_input_is_committed_once_on_success() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(
                        openai_completion_body("success-output", vec![]),
                    )),
            )
            .await;
        let agent = Agent::new_with_chain(
            "success".into(),
            test_role(),
            vec![model_at(&server, "stub")],
            GenerateParams::default(),
        )
        .unwrap();
        let mut runner = AgentRunner::new(agent);

        let out = runner
            .run_turn(&[Message::user("success-input")], None)
            .await
            .unwrap();
        assert_eq!(out, "success-output");
        let history = runner.context().messages();
        assert_eq!(history.len(), 2, "user + assistant, each exactly once");
        assert_eq!(history[0].as_text(), "success-input");
        assert_eq!(history[1].as_text(), "success-output");

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body = String::from_utf8_lossy(&requests[0].body);
        assert_eq!(
            body.matches("success-input").count(),
            1,
            "model prompt must not duplicate the just-committed input: {body}"
        );
    }

    /// HTTP/model 失败也必须保留用户已经提交的输入，供下一轮模型看到。
    #[tokio::test]
    async fn turn_input_survives_model_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized")),
            )
            .await;
        let agent = Agent::new_with_chain(
            "error".into(),
            test_role(),
            vec![model_at(&server, "stub")],
            GenerateParams::default(),
        )
        .unwrap();
        let mut runner = AgentRunner::new(agent);

        let result = runner
            .run_turn(&[Message::user("input-before-model-error")], None)
            .await;
        assert!(result.is_err(), "401 must fail the turn");
        let history = runner.context().messages();
        assert_eq!(history.len(), 1, "failed turn must retain only its user input");
        assert_eq!(history[0].as_text(), "input-before-model-error");
    }

    /// controller 的当前-turn 取消会 drop `run_turn` future；drop 后输入仍
    /// 必须留在 runner context，不能随 in-flight 请求一起消失。
    #[tokio::test]
    async fn turn_input_survives_future_cancellation() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .set_delay(Duration::from_secs(5))
                            .set_body_string(openai_completion_body("too late", vec![])),
                    ),
            )
            .await;
        let agent = Agent::new_with_chain(
            "cancel".into(),
            test_role(),
            vec![model_at(&server, "stub")],
            GenerateParams::default(),
        )
        .unwrap();
        let mut runner = AgentRunner::new(agent);

        let input = [Message::user("input-before-cancel")];
        let mut future = Box::pin(runner.run_turn(&input, None));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), future.as_mut())
                .await
                .is_err(),
            "mock turn should still be in flight"
        );
        drop(future);

        let history = runner.context().messages();
        assert_eq!(history.len(), 1, "cancelled turn must retain its user input");
        assert_eq!(history[0].as_text(), "input-before-cancel");
    }

    /// 同一工具连续被确定性校验拒绝 → 先追加硬指令，再熔断。
    ///
    /// 回归实测事故：manager 连着 3 轮调 `plan`，每轮
    /// 都改一点参数再撞同一类校验（`LoopDetector` 只认参数完全相同的
    /// 重复调用，所以毫无反应），每轮烧 100~150s 模型时间。
    #[tokio::test]
    async fn consecutive_permanent_tool_failures_break_the_loop() {
        use latte_rs_agent_tools::prelude::create_tool_manager;
        use latte_rs_agent_tools::types::{
            SchemaType, SharedToolHandler, Tool, ToolInputSchema, ToolManager as _,
        };
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        // 永远以"确定性输入错误"失败的工具（"is required" 命中
        // classify_tool_execution_error 的 PERMANENT 列表）。
        let handler: SharedToolHandler = Arc::new(move |_input, _ctx| {
            Box::pin(async move {
                Err(latte_rs_agent_tools::error::ToolError::other(
                    "title is required",
                ))
            })
        });
        let schema = ToolInputSchema {
            schema_type: SchemaType,
            properties: Default::default(),
            required: None,
            additional_properties: None,
            defs: None,
        };
        let tm = create_tool_manager();
        tm.register(
            Tool::builder("plan", "test plan", schema, handler).build(),
            None,
        );

        let server = wiremock::MockServer::start().await;
        // **每轮参数都不同** —— 这是 permanent_streak 存在的理由，也是
        // 本测试与死循环熔断的分工：`LoopDetector` 只认逐字节相同的重复
        // 调用，参数一变它就清零，于是「每轮改一点参数、反复撞同一类
        // 校验」这种烧钱模式只有 permanent_streak 能抓。
        //
        // 桩必须变参数，否则 LoopDetector 会在第 5 次先行熔断，
        // permanent_streak 的 BREAK_AT(8) 永远走不到。
        for i in 0..(PERMANENT_BREAK_AT + 1) {
            server
                .register(
                    Mock::given(method("POST"))
                        .and(path("/chat/completions"))
                        .respond_with(ResponseTemplate::new(200).set_body_string(
                            openai_completion_body(
                                "",
                                vec![serde_json::json!({
                                    "id": format!("call_plan_{i}"),
                                    "type": "function",
                                    "function": {
                                        "name": "plan",
                                        // 参数每轮不同，但都会撞同一个
                                        // "title is required" 校验。
                                        "arguments": format!("{{\"seq\":{i}}}")
                                    }
                                })],
                            ),
                        ))
                        .up_to_n_times(1),
                )
                .await;
        }

        let agent = Agent::new_with_chain(
            "t".into(),
            test_role(),
            vec![model_at(&server, "stub")],
            GenerateParams::default(),
        )
        .unwrap();
        // 轮次上限已退化为 soft warning，这里给 0（不限），让
        // permanent_streak 成为唯一出口。
        let mut runner = AgentRunner::new_with_tools(agent, tm);

        let err = runner
            .run_turn(&[Message::user("go")], None)
            .await
            .expect_err("连续确定性失败应熔断");
        match err {
            AgentError::ToolLoopDetected { tool, reason, .. } => {
                assert_eq!(tool, "plan");
                assert!(
                    reason.contains(&format!("连续 {PERMANENT_BREAK_AT} 次")),
                    "应报 permanent_streak 的熔断（而非同参数死循环）: {reason}"
                );
                assert!(
                    reason.contains("每次参数都不同"),
                    "要说明是变参数撞同一校验: {reason}"
                );
            }
            other => panic!("应判 ToolLoopDetected，实际 {other:?}"),
        }

        // 第 NUDGE_AT 次失败后的下一个请求必须带上"别再调这个工具"的硬指令。
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(
            reqs.len(),
            PERMANENT_BREAK_AT,
            "{PERMANENT_BREAK_AT} 次失败 = {PERMANENT_BREAK_AT} 个模型请求"
        );
        let nudged = String::from_utf8_lossy(&reqs[PERMANENT_NUDGE_AT].body);
        assert!(
            nudged.contains(&format!("已连续 {PERMANENT_NUDGE_AT} 次因输入校验失败被拒")),
            "第 {} 个请求应带熔断前的硬指令: {nudged}",
            PERMANENT_NUDGE_AT + 1
        );
        let body2 = String::from_utf8_lossy(&reqs[1].body);
        assert!(
            !body2.contains("已连续"),
            "第 2 个请求还没到 3 次，不该有指令"
        );
    }

    /// session 暂停门 park 必须在 trace 上留痕（进 SessionPaused、出
    /// SessionResumed）。
    ///
    /// 回归实测会话：3 个 park 点都是裸
    /// `wait_until_resumed(None)`，park 期间 trace 一个事件都没有，
    /// 事后无法区分"暂停 / 限流 / 死锁"。
    #[tokio::test]
    async fn park_emits_session_paused_and_resumed() {
        use crate::trace::{TraceEvent, TraceSink};

        struct VecSink(parking_lot::Mutex<Vec<TraceEvent>>);
        impl TraceSink for VecSink {
            fn emit(&self, e: TraceEvent) {
                self.0.lock().push(e);
            }
        }

        let server = wiremock::MockServer::start().await;
        let agent = Agent::new_with_chain(
            "t".into(),
            test_role(),
            vec![model_at(&server, "stub")],
            GenerateParams::default(),
        )
        .unwrap();
        let sink = Arc::new(VecSink(parking_lot::Mutex::new(vec![])));
        let gate = crate::pause_gate::AgentPauseGate::new("test");
        let runner = AgentRunner::new(agent)
            .with_sink(sink.clone() as Arc<dyn TraceSink>)
            .with_agent_pause_gate(gate.clone());

        // 未暂停 → 不产生任何事件，也不阻塞。
        runner.park_if_paused("noop").await;
        assert!(sink.0.lock().is_empty(), "没暂停就不该发事件");

        gate.pause_with_reason("用户按了 ⏸");
        let g = gate.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            g.resume();
        });
        runner.park_if_paused("tool exec boundary").await;

        let events = sink.0.lock();
        let paused = events.iter().find_map(|e| match e {
            TraceEvent::SessionPaused { reason, .. } => Some(reason.clone()),
            _ => None,
        });
        let reason = paused.expect("应发 SessionPaused");
        assert!(reason.contains("tool exec boundary"), "reason: {reason}");
        assert!(reason.contains("用户按了 ⏸"), "应带上门的暂停原因: {reason}");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TraceEvent::SessionResumed { .. })),
            "恢复后应发 SessionResumed"
        );
    }

    /// TurnEnd 必须用结束时刻的 meta（此前复用 turn 起始 meta，时间线上
    /// 看起来 turn 刚开始就结束了），且 elapsed_ms 是 wall-clock。
    #[tokio::test]
    async fn turn_end_uses_end_timestamp() {
        use crate::trace::{TraceEvent, TraceSink};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        struct VecSink(parking_lot::Mutex<Vec<TraceEvent>>);
        impl TraceSink for VecSink {
            fn emit(&self, e: TraceEvent) {
                self.0.lock().push(e);
            }
        }

        let server = wiremock::MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .set_delay(Duration::from_millis(1200))
                            .set_body_string(openai_completion_body("done", vec![])),
                    ),
            )
            .await;
        let agent = Agent::new_with_chain(
            "t".into(),
            test_role(),
            vec![model_at(&server, "stub")],
            GenerateParams::default(),
        )
        .unwrap();
        let sink = Arc::new(VecSink(parking_lot::Mutex::new(vec![])));
        let mut runner = AgentRunner::new(agent).with_sink(sink.clone() as Arc<dyn TraceSink>);
        runner.run_turn(&[Message::user("hi")], None).await.unwrap();

        let events = sink.0.lock();
        let built = events
            .iter()
            .find_map(|e| match e {
                TraceEvent::PromptBuilt { meta, .. } => Some(meta.ts.clone()),
                _ => None,
            })
            .expect("PromptBuilt");
        let (end_ts, elapsed) = events
            .iter()
            .find_map(|e| match e {
                TraceEvent::TurnEnd { meta, elapsed_ms, .. } => {
                    Some((meta.ts.clone(), *elapsed_ms))
                }
                _ => None,
            })
            .expect("TurnEnd");
        assert!(
            end_ts >= built,
            "TurnEnd.ts({end_ts}) 不能早于 PromptBuilt.ts({built})"
        );
        assert!(elapsed >= 1200, "elapsed_ms 应覆盖真实耗时，实际 {elapsed}");
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
            AgentError::ModelsUnavailable {
                tried,
                failures,
                next_retry_in,
            } => {
                assert_eq!(tried, vec!["m1".to_string(), "m2".to_string()]);
                assert!(next_retry_in.is_some(), "should report a retry window");
                // 每个失败模型都带底层错误摘要（可据此确诊限流/鉴权）
                assert_eq!(failures.len(), 2);
                assert_eq!(failures[0].0, "m1");
                assert!(!failures[0].1.is_empty());
                assert_eq!(failures[1].0, "m2");
            }
            other => panic!("expected ModelsUnavailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_chat_falls_through_to_next_model_on_auth_error() {
        // Auth errors DO walk the chain: each model has its own
        // key/vendor, so one model's bad token says nothing about the
        // next model's health (glm relay key expired ≠ deepseek key
        // bad). The dead model is cooled down and the chain continues.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let s1 = wiremock::MockServer::start().await;
        // wiremock returns 401, which latte-ai surfaces as Auth.
        s1.register(
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized")),
        )
        .await;
        let s2 = wiremock::MockServer::start().await;
        s2.register(
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .respond_with(ResponseTemplate::new(200).set_body_string(
                    openai_completion_body("reached via fallback", vec![]),
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

        let completion = agent
            .chat(&[Message::user("hi")], None, WaitPolicy::NoWait)
            .await
            .expect("chain should fall through to the healthy model");
        assert!(completion.content.contains("reached via fallback"));
        // m1 got exactly one attempt, then the chain moved on.
        assert_eq!(s1.received_requests().await.unwrap().len(), 1);
        assert_eq!(s2.received_requests().await.unwrap().len(), 1);
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

    // ─── Stream 模式测试 ──────────────────────────────────────────────
    //
    // stream 模式：runner 装了 with_stream_mode(true) 时，run_turn 走
    // Agent::chat_stream 消费 SSE Delta 事件，逐 chunk 发 TraceEvent::ModelDelta，
    // Done 时组装完整 Completion。此测试用 wiremock 返回 SSE 流验证：
    //   1. run_turn 返回拼好的完整文本（跨多个 delta chunk）。
    //   2. sink 收到 ≥1 个 ModelDelta 增量事件。
    //   3. non-stream（默认）时 sink 收到 0 个 ModelDelta（走 chat() 聚合路径）。
    /// 流式下「连接建立成功、但流上第一个事件就是 HTTP 400」必须沿模型链
    /// 换到下一个模型，而不是当场判死整个 turn。
    ///
    /// 回归的真实故障（`chat -r programmer`，glm-5.3 中转站）：把一条工具
    /// 校验错误喂回去之后厂商回 400，链上还有三个健康模型，却一个都没轮到，
    /// turn 直接以 `AI client error: API error: 400` 失败。根因是
    /// `chat_stream` 的链式 fallback 只覆盖**连接建立阶段**（返回 Err），
    /// 连接成功后到达的 `StreamEvent::HttpError` 走的是 `return Err` 死路。
    #[tokio::test]
    async fn stream_http_error_before_any_delta_falls_back_to_next_model() {
        use crate::trace::{FallbackReason, TraceEvent, TraceSink};
        use std::sync::Arc;
        #[derive(Clone)]
        struct VecSink(Arc<parking_lot::Mutex<Vec<TraceEvent>>>);
        impl TraceSink for VecSink {
            fn emit(&self, e: TraceEvent) {
                self.0.lock().push(e);
            }
        }
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        // 坏模型：**200 建连**不行——要让 latte-ai 走到 HttpError 分支，
        // 得让首个响应就是非 2xx；它会先试非流式，也 400，然后把
        // HttpError 推进 channel。
        let broken = wiremock::MockServer::start().await;
        broken
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(400).set_body_string(
                        r#"{"error":{"message":"请求参数值或格式不受支持","type":"invalid_request_error"}}"#,
                    )),
            )
            .await;
        // 健康模型：正常吐 SSE。
        let healthy = wiremock::MockServer::start().await;
        healthy
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .insert_header("Content-Type", "text/event-stream")
                            .set_body_string(openai_sse_body(&["recovered"])),
                    ),
            )
            .await;

        let sink = Arc::new(VecSink(Arc::new(parking_lot::Mutex::new(vec![]))));
        let agent = Agent::new_with_chain(
            "fallback-test".into(),
            test_role(),
            vec![
                model_at(&broken, "broken-400"),
                model_at(&healthy, "healthy-model"),
            ],
            GenerateParams::default(),
        )
        .unwrap();
        let mut runner = AgentRunner::new(agent)
            .with_sink(sink.clone() as Arc<dyn TraceSink>)
            .with_role("fallback-test".to_string())
            .with_stream_mode(Arc::new(std::sync::atomic::AtomicBool::new(true)));

        let resp = runner
            .run_turn(&[Message::user("hi")], None)
            .await
            .expect("400 之后必须换模型跑通，而不是整 turn 判死");
        assert_eq!(resp, "recovered", "应拿到链上第二个模型的输出");

        let events = sink.0.lock().clone();
        let fb = events
            .iter()
            .find_map(|e| match e {
                TraceEvent::FallbackTriggered {
                    from_model,
                    to_model,
                    reason,
                    ..
                } => Some((from_model.clone(), to_model.clone(), *reason)),
                _ => None,
            })
            .expect("必须留下 FallbackTriggered 痕迹，否则换模型是黑盒");
        assert_eq!(fb.0, "broken-400", "from 应是报 400 的那个");
        assert_eq!(fb.1.as_deref(), Some("healthy-model"), "to 应是链上下一个");
        assert_eq!(fb.2, FallbackReason::RetryableFailure);

        // `ModelCall` 必须报**实际服务**的模型，不能报链首。
        // 原来它取 `model_chain.first()`，fallback 之后仍标 broken-400，
        // 等于把"换没换成模型"从日志里抹掉——本次会话就被它误导过。
        let called: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                TraceEvent::ModelCall { model_id, .. } => Some(model_id.clone()),
                _ => None,
            })
            .collect();
        assert!(
            called.iter().all(|m| m == "healthy-model"),
            "ModelCall 应报实际服务的 healthy-model，实际: {called:?}"
        );
    }

    #[tokio::test]
    async fn stream_mode_run_turn_emits_deltas_and_assembles() {
        use crate::trace::{TraceEvent, TraceSink};
        use std::sync::Arc;
        #[derive(Clone)]
        struct StreamVecSink(Arc<parking_lot::Mutex<Vec<TraceEvent>>>);
        impl TraceSink for StreamVecSink {
            fn emit(&self, e: TraceEvent) {
                self.0.lock().push(e);
            }
        }
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let s = wiremock::MockServer::start().await;
        s.register(
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .respond_with(ResponseTemplate::new(200)
                    .insert_header("Content-Type", "text/event-stream")
                    .set_body_string(openai_sse_body(&["Hello", ", ", "stream", " mode!"])),
                ),
        )
        .await;

        let sink = Arc::new(StreamVecSink(Arc::new(parking_lot::Mutex::new(vec![]))));
        let role = test_role();
        let agent = Agent::new_with_chain(
            "stream-test".into(),
            role,
            vec![model_at(&s, "stream-model")],
            GenerateParams::default(),
        ).unwrap();
        let mut runner = AgentRunner::new(agent)
            .with_sink(sink.clone() as Arc<dyn TraceSink>)
            .with_role("stream-test".to_string())
            .with_stream_mode(Arc::new(std::sync::atomic::AtomicBool::new(true)));

        let resp = runner
            .run_turn(&[Message::user("hi")], None)
            .await
            .expect("stream run_turn should succeed");
        assert_eq!(resp, "Hello, stream mode!", "stream 模式应拼出完整文本");

        // stream 模式应发出 ≥1 个 ModelDelta 增量事件。
        let events = sink.0.lock().clone();
        let delta_count = events
            .iter()
            .filter(|e| matches!(e, TraceEvent::ModelDelta { .. }))
            .count();
        assert!(delta_count > 0, "stream 模式应发出 ModelDelta，但收到 {delta_count} 个");

        // non-stream（默认，不装 stream_mode）时不应发 ModelDelta，走 chat() 聚合。
        let s2 = wiremock::MockServer::start().await;
        s2.register(
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .respond_with(ResponseTemplate::new(200)
                    .set_body_string(openai_completion_body("plain", vec![])),
                ),
        )
        .await;
        let sink2 = Arc::new(StreamVecSink(Arc::new(parking_lot::Mutex::new(vec![]))));
        let agent2 = Agent::new_with_chain(
            "nostream-test".into(),
            test_role(),
            vec![model_at(&s2, "nostream-model")],
            GenerateParams::default(),
        ).unwrap();
        let mut runner2 = AgentRunner::new(agent2)
            .with_sink(sink2.clone() as Arc<dyn TraceSink>)
            .with_role("nostream-test".to_string());
        let resp2 = runner2
            .run_turn(&[Message::user("hi")], None)
            .await
            .expect("non-stream run_turn should succeed");
        assert_eq!(resp2, "plain", "non-stream 模式返回完整文本");
        let events2 = sink2.0.lock().clone();
        assert!(
            !events2.iter().any(|e| matches!(e, TraceEvent::ModelDelta { .. })),
            "non-stream 模式不应发 ModelDelta（走 chat() 聚合路径）"
        );
    }
    // The detector is the agent's safety net against the "model is
    // stuck" failure mode: it watches for repeated identical tool
    // calls and breaks the loop early. These tests pin down the
    // threshold (3 consecutive identical calls) and the reset
    // behaviour (a different call resets the streak counter).
    #[test]
    fn loop_detector_breaks_on_repeated_calls() {
        let mut d = LoopDetector::default();
        // 前 4 次同样的调用还够不上阈值（LOOP_STREAK_THRESHOLD = 5）。
        for _ in 0..4 {
            assert!(matches!(
                d.record("read", "{\"path\":\"a.rs\"}"),
                LoopDecision::Continue
            ));
        }
        // 第 5 次同样的调用触发。reason 要点名肇事工具，用户才能据此处理。
        let decision = d.record("read", "{\"path\":\"a.rs\"}");
        match decision {
            LoopDecision::Break(reason) => {
                assert!(reason.contains("read"),
                    "break reason should name the offending tool, got: {reason}");
            }
            LoopDecision::Continue => panic!("expected Break on 5th identical call"),
        }
    }

    #[test]
    fn loop_detector_resets_on_different_call() {
        let mut d = LoopDetector::default();
        // 先攒一段接近阈值的连击。
        for _ in 0..4 {
            assert!(matches!(
                d.record("read", "{\"path\":\"a.rs\"}"),
                LoopDecision::Continue
            ));
        }
        // 换一个调用把连击清零 —— 这正是「改了参数就是有进展」的语义。
        assert!(matches!(d.record("search", "{\"path\":\".\"}"), LoopDecision::Continue));
        // 重新计数：接下来 4 次仍在阈值之下。
        for _ in 0..4 {
            assert!(matches!(
                d.record("read", "{\"path\":\"a.rs\"}"),
                LoopDecision::Continue
            ));
        }
        // 清零后的第 5 次连续调用才触发。
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
            defs: None,
        };
        let tool = Tool::builder("ping", "test ping", schema, handler).build();
        let tm = create_tool_manager();
        tm.register(tool, None);

        // 桩模型：前 3 次返回同一个 tool call，让 turn 在工具轮次里
        // 打乒乓（这样才有 round 边界可观察）；第 4 次返回纯文本，
        // turn 自然收尾。
        //
        // 终止方式为什么不靠轮次上限：`max_tool_rounds` 已改为
        // deadline-only 模式下的 soft warning，不再硬中止（见 run_turn
        // 里的说明），`MaxToolRoundsExceeded` 也不再被构造。本测试要验
        // 的是「hint 在工具循环中途就下发到模型」，终止手段无关紧要，
        // 所以改用响应序列——比挂 deadline 更确定，也不留超时抖动。
        //
        // 注意 LoopDetector 是**每轮**新建的（见 run_turn 里 2026-07-10
        // 的修复），跨轮重复不会被它熔断，所以这里必须由桩自己收尾。
        let server = wiremock::MockServer::start().await;
        // 先挂 tool_call 桩：wiremock 中**先挂载的优先匹配**，这条
        // up_to_n_times(3) 耗尽后才落到后面的收尾 mock。
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
                    ))
                    .up_to_n_times(3),
            )
            .await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(
                        openai_completion_body("收尾产出", vec![]),
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
        let mut runner = AgentRunner::new_with_tools(agent, tm).with_advisor_hints(hints);

        let out = runner
            .run_turn(&[Message::user("go")], None)
            .await
            .expect("桩模型第 4 次返回纯文本，turn 应正常收尾");
        assert_eq!(out.trim(), "收尾产出", "最终产出应来自收尾那一轮");

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

    /// 回归防线：**循环被自动刹车中止时，模型已产出的正文必须作为
    /// `partial` 交出来。**
    ///
    /// 事故背景（实测）：estimate 步跑了 105 轮，最后一条回复是完整
    /// 的验证结论，但中止错误当时只带一个轮次数字，上层拿不到任何产出，
    /// 于是 step 失败 → 嵌套 workflow 失败 → 父 workflow 失败，3 小时
    /// 零产出。partial 是 workflow / delegate 两条降级路径的唯一输入。
    ///
    /// 本测试原先断言 `MaxToolRoundsExceeded`。deadline-only 模式下轮次
    /// 上限已退化为 soft warning、该错误不再被构造，实际生效的刹车是
    /// **同参数死循环熔断**（`ToolLoopDetected`）——所以防线必须挪到它
    /// 身上，否则「唯一会触发的刹车恰好不带 partial」，事故原样复发。
    #[tokio::test]
    async fn tool_loop_error_carries_partial_output() {
        use latte_rs_agent_tools::prelude::create_tool_manager;
        use latte_rs_agent_tools::types::{
            SchemaType, SharedToolHandler, Tool, ToolInputSchema, ToolManager as _,
        };
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let handler: SharedToolHandler = Arc::new(move |_input, _ctx| {
            Box::pin(async move { Ok(serde_json::json!({ "ok": true })) })
        });
        let schema = ToolInputSchema {
            schema_type: SchemaType,
            properties: Default::default(),
            required: None,
            additional_properties: None,
            defs: None,
        };
        let tm = create_tool_manager();
        tm.register(
            Tool::builder("ping", "test ping", schema, handler).build(),
            None,
        );

        // 每轮都「有正文 + 有完全相同的 tool_call」——正是事故里的形态：
        // 模型边写结论边反复调同一个工具，最终被死循环熔断。
        const PROSE: &str = "refill_produced=65 已核对，语义符合预期";
        let server = wiremock::MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(
                        openai_completion_body(
                            PROSE,
                            vec![serde_json::json!({
                                "id": "call_ping",
                                "type": "function",
                                "function": { "name": "ping", "arguments": "{}" }
                            })],
                        ),
                    )),
            )
            .await;

        let agent = Agent::new_with_chain(
            "t".into(),
            test_role(),
            vec![model_at(&server, "stub")],
            GenerateParams::default(),
        )
        .unwrap();
        let mut runner = AgentRunner::new_with_tools(agent, tm);

        let err = runner
            .run_turn(&[Message::user("go")], None)
            .await
            .expect_err("同参数反复调用应被死循环熔断");
        match &err {
            AgentError::ToolLoopDetected {
                tool,
                reason,
                partial,
            } => {
                assert_eq!(tool, "ping");
                assert!(
                    reason.contains("identical args"),
                    "原因要说明是同参数重复: {reason}"
                );
                assert!(
                    partial.contains(PROSE),
                    "partial 必须携带模型已产出的正文，实际: {partial:?}"
                );
            }
            other => panic!("期望 ToolLoopDetected，实际 {other:?}"),
        }
        // Display 只报规模、不倒正文（错误串会回填给模型）。
        let msg = err.to_string();
        assert!(msg.contains("tool loop detected"), "msg = {msg}");
        assert!(msg.contains("可降级采纳"), "msg 应提示可降级: {msg}");
        assert!(!msg.contains(PROSE), "Display 不应内联正文: {msg}");
    }

    /// 核心保证：**跨 round 的同参数死循环必须被抓到。**
    ///
    /// deadline-only 模式下这是唯一的自动刹车——轮次上限已退化为 soft
    /// warning，`deadline` 只在 delegate / workflow step 上设置（交互式
    /// chat 不设，超时与叫停按设计交给人工 ⏸ 或 advisor）。
    ///
    /// 回归对象：`LoopDetector` 曾被构造在 round 循环**内部**，每轮重置。
    /// 模型一轮通常只发一个工具调用，于是 streak 永远是 1，跨轮死循环
    /// 一次都测不到——刹车形同虚设，turn 会无限空转（本测试在修复前会
    /// 一直打 wiremock 直到外层超时）。
    ///
    /// 断言「模型每轮发一个完全相同的调用」这一最常见形态：连续
    /// `LOOP_STREAK_THRESHOLD`(5) 轮后熔断，且点名肇事工具。
    #[tokio::test]
    async fn identical_tool_call_across_rounds_trips_loop_detector() {
        use latte_rs_agent_tools::prelude::create_tool_manager;
        use latte_rs_agent_tools::types::{
            SchemaType, SharedToolHandler, Tool, ToolInputSchema, ToolManager as _,
        };
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let handler: SharedToolHandler = Arc::new(move |_input, _ctx| {
            Box::pin(async move { Ok(serde_json::json!({ "ok": true })) })
        });
        let schema = ToolInputSchema {
            schema_type: SchemaType,
            properties: Default::default(),
            required: None,
            additional_properties: None,
            defs: None,
        };
        let tm = create_tool_manager();
        tm.register(
            Tool::builder("spin", "always same", schema, handler).build(),
            None,
        );

        // 每轮**一个**调用、参数逐字节相同 —— 探测器若按轮重置就永远
        // 抓不到（streak 恒为 1）。
        let server = wiremock::MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(
                        openai_completion_body(
                            "",
                            vec![serde_json::json!({
                                "id": "call_spin",
                                "type": "function",
                                "function": { "name": "spin", "arguments": "{\"k\":1}" }
                            })],
                        ),
                    )),
            )
            .await;

        let agent = Agent::new_with_chain(
            "t".into(),
            test_role(),
            vec![model_at(&server, "stub")],
            GenerateParams::default(),
        )
        .unwrap();
        // 注意：不设 deadline、轮次上限给 0（=不限），刻意让死循环
        // 探测器成为唯一出口。
        let mut runner = AgentRunner::new_with_tools(agent, tm);

        let err = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            runner.run_turn(&[Message::user("go")], None),
        )
        .await
        .expect("必须由死循环熔断收尾，而不是无限空转到外层超时")
        .expect_err("同参数跨轮重复应被熔断");

        match &err {
            AgentError::ToolLoopDetected { tool, reason, .. } => {
                assert_eq!(tool, "spin", "要点名肇事工具");
                assert!(
                    reason.contains("5 times in a row"),
                    "应在第 5 次连续相同调用时熔断: {reason}"
                );
            }
            other => panic!("期望 ToolLoopDetected，实际 {other:?}"),
        }
        // 熔断要快：5 轮就够，不该把几十轮 model call 都烧掉。
        let n = server.received_requests().await.unwrap().len();
        assert!(
            (5..=7).contains(&n),
            "应在 ~5 轮内熔断，实际发了 {n} 次 model call"
        );
    }

    /// 反向防线：参数**每次都变**的连续调用不是死循环，不得误杀。
    ///
    /// 对应 2026-07-10 那次事故：manager 连着几轮 delegate「同一类」任务
    /// （每次重写任务描述）被当成死循环打断。判定键含 args_json，改参数
    /// 就该重新计数——这条测试把该语义钉住，防止有人为了「更严格」把
    /// 判定放宽成只看工具名。
    #[tokio::test]
    async fn varying_args_across_rounds_do_not_trip_loop_detector() {
        use latte_rs_agent_tools::prelude::create_tool_manager;
        use latte_rs_agent_tools::types::{
            SchemaType, SharedToolHandler, Tool, ToolInputSchema, ToolManager as _,
        };
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let handler: SharedToolHandler = Arc::new(move |_input, _ctx| {
            Box::pin(async move { Ok(serde_json::json!({ "ok": true })) })
        });
        let schema = ToolInputSchema {
            schema_type: SchemaType,
            properties: Default::default(),
            required: None,
            additional_properties: None,
            defs: None,
        };
        let tm = create_tool_manager();
        tm.register(
            Tool::builder("step", "varying", schema, handler).build(),
            None,
        );

        let server = wiremock::MockServer::start().await;
        // 6 轮，每轮参数都不同（> 阈值 5），最后一轮返回纯文本收尾。
        for i in 0..6 {
            server
                .register(
                    Mock::given(method("POST"))
                        .and(path("/chat/completions"))
                        .respond_with(ResponseTemplate::new(200).set_body_string(
                            openai_completion_body(
                                "",
                                vec![serde_json::json!({
                                    "id": format!("call_{i}"),
                                    "type": "function",
                                    "function": {
                                        "name": "step",
                                        "arguments": format!("{{\"i\":{i}}}")
                                    }
                                })],
                            ),
                        ))
                        .up_to_n_times(1),
                )
                .await;
        }
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(
                        openai_completion_body("按步骤走完了", vec![]),
                    )),
            )
            .await;

        let agent = Agent::new_with_chain(
            "t".into(),
            test_role(),
            vec![model_at(&server, "stub")],
            GenerateParams::default(),
        )
        .unwrap();
        let mut runner = AgentRunner::new_with_tools(agent, tm);

        let out = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            runner.run_turn(&[Message::user("go")], None),
        )
        .await
        .expect("不该挂住")
        .expect("参数每轮都变，不是死循环，不得熔断");
        assert_eq!(out.trim(), "按步骤走完了");
    }

    /// 核心保证（规则 2）：**A-B-A-B 交替打转必须被抓到。**
    ///
    /// 只有连击规则时这里是个洞：每次调用都与上一次不同 → `streak` 恒为
    /// 1 → 永不熔断。而交互式 chat 既无轮次上限也不设 deadline，模型这么
    /// 跳就是无限空转。
    #[test]
    fn loop_detector_breaks_on_alternating_cycle() {
        let mut d = LoopDetector::default();
        let a = ("read", r#"{"path":"a.rs"}"#);
        let b = ("search", r#"{"pattern":"foo"}"#);
        // 前 7 次都不该触发：连击恒为 1，窗口也还没填满。
        for i in 0..7 {
            let (t, args) = if i % 2 == 0 { a } else { b };
            assert!(
                matches!(d.record(t, args), LoopDecision::Continue),
                "第 {i} 次不该熔断（连击恒为 1，窗口未满）"
            );
        }
        // 第 8 次填满窗口：只有 2 种 key、每种都重复出现 → 判打转。
        let (t, args) = a;
        match d.record(t, args) {
            LoopDecision::Break(reason) => {
                assert!(reason.contains("来回打转"), "reason: {reason}");
                assert!(reason.contains("read"), "要点出参与打转的工具: {reason}");
                assert!(reason.contains("search"), "两个工具都要点出: {reason}");
            }
            LoopDecision::Continue => panic!("窗口填满后应判打转"),
        }
    }

    /// 反向防线一：窗口里夹一个**孤立**的别的调用，不算打转。
    ///
    /// 这是被打断的连击（规则 1 的辖区），不是环。少了
    /// `LOOP_CYCLE_MIN_EACH` 这条，`loop_detector_resets_on_different_call`
    /// 描述的正常语义会被规则 2 误杀。
    #[test]
    fn loop_detector_isolated_odd_call_is_not_a_cycle() {
        let mut d = LoopDetector::default();
        // read×4（连击到 4，未达 5）→ search×1 清零 → read×3。
        // 窗口 8 项里 read×7 + search×1：search 只出现 1 次，不构成环。
        for _ in 0..4 {
            assert!(matches!(
                d.record("read", r#"{"path":"a.rs"}"#),
                LoopDecision::Continue
            ));
        }
        assert!(matches!(
            d.record("search", r#"{"pattern":"x"}"#),
            LoopDecision::Continue
        ));
        for i in 0..3 {
            assert!(
                matches!(d.record("read", r#"{"path":"a.rs"}"#), LoopDecision::Continue),
                "第 {i} 次 read 不该被判打转（search 只出现 1 次）"
            );
        }
    }

    /// 反向防线二：「读 → 改 → 读验证」这类交替是**正常进展**，不得误杀。
    ///
    /// 关键在 edit 的参数每次都不同（写入的内容在变），窗口里的 key 种类
    /// 因此远超 2，规则 2 不会触发。
    #[test]
    fn loop_detector_read_edit_verify_cycle_is_progress() {
        let mut d = LoopDetector::default();
        for i in 0..4 {
            // 读同一个文件（参数相同）——改完要复核，这是正常的。
            assert!(
                matches!(d.record("read", r#"{"path":"a.rs"}"#), LoopDecision::Continue),
                "第 {i} 轮 read 不该熔断"
            );
            // 每次写入不同内容 → 不同 key。
            assert!(
                matches!(
                    d.record("edit", &format!(r#"{{"path":"a.rs","content":"v{i}"}}"#)),
                    LoopDecision::Continue
                ),
                "第 {i} 轮 edit 不该熔断"
            );
        }
    }

    /// 端到端：模型跨 round 在两个工具之间来回跳 → 由打转规则收尾。
    ///
    /// 不设 deadline、不设轮次上限，刻意让死循环熔断成为唯一出口。
    #[tokio::test]
    async fn alternating_tool_calls_across_rounds_trip_loop_detector() {
        use latte_rs_agent_tools::prelude::create_tool_manager;
        use latte_rs_agent_tools::types::{
            SchemaType, SharedToolHandler, Tool, ToolInputSchema, ToolManager as _,
        };
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let tm = create_tool_manager();
        for name in ["ping", "pong"] {
            let handler: SharedToolHandler = Arc::new(move |_input, _ctx| {
                Box::pin(async move { Ok(serde_json::json!({ "ok": true })) })
            });
            tm.register(
                Tool::builder(
                    name,
                    "alt",
                    ToolInputSchema {
                        schema_type: SchemaType,
                        properties: Default::default(),
                        required: None,
                        additional_properties: None,
                        defs: None,
                    },
                    handler,
                )
                .build(),
                None,
            );
        }

        // 桩模型交替返回 ping / pong，参数恒定 —— 连击规则抓不到。
        let server = wiremock::MockServer::start().await;
        for i in 0..(LOOP_WINDOW_SIZE + 2) {
            let name = if i % 2 == 0 { "ping" } else { "pong" };
            server
                .register(
                    Mock::given(method("POST"))
                        .and(path("/chat/completions"))
                        .respond_with(ResponseTemplate::new(200).set_body_string(
                            openai_completion_body(
                                "",
                                vec![serde_json::json!({
                                    "id": format!("call_{i}"),
                                    "type": "function",
                                    "function": { "name": name, "arguments": "{}" }
                                })],
                            ),
                        ))
                        .up_to_n_times(1),
                )
                .await;
        }

        let agent = Agent::new_with_chain(
            "t".into(),
            test_role(),
            vec![model_at(&server, "stub")],
            GenerateParams::default(),
        )
        .unwrap();
        let mut runner = AgentRunner::new_with_tools(agent, tm);

        let err = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            runner.run_turn(&[Message::user("go")], None),
        )
        .await
        .expect("必须由打转熔断收尾，而不是无限空转到外层超时")
        .expect_err("交替调用应被判打转");

        match &err {
            AgentError::ToolLoopDetected { reason, .. } => {
                assert!(reason.contains("来回打转"), "reason: {reason}");
            }
            other => panic!("期望 ToolLoopDetected，实际 {other:?}"),
        }
        // 窗口 8 就该收手，不能烧到十几轮。
        let n = server.received_requests().await.unwrap().len();
        assert!(
            (LOOP_WINDOW_SIZE..=LOOP_WINDOW_SIZE + 2).contains(&n),
            "应在窗口填满时熔断，实际发了 {n} 次 model call"
        );
    }


    /// v3 pause gate：runner 在 tool-round 边界挂起，直到拍板
    /// （resolve）才继续。模拟链路：工具 handler 扮演 monitor 置位
    /// pause gate → 下一轮边界 runner 挂起 → 外部任务（扮演用户
    /// 输入）resolve → turn 继续跑完。
    #[tokio::test]
    async fn pause_gate_suspends_tool_loop_until_resolved() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        use latte_rs_agent_tools::prelude::create_tool_manager;
        use latte_rs_agent_tools::types::{
            SchemaType, SharedToolHandler, Tool, ToolInputSchema, ToolManager as _,
        };

        let gate = crate::advisor_monitor::AdvisorPauseGate::with_timeout(
            std::time::Duration::from_secs(30),
        );

        // 工具 handler 扮演 advisor monitor：首次执行时请求暂停
        // （只置位一次——否则恢复后每轮工具又置位，turn 会再次挂起）。
        let gate_in_tool = gate.clone();
        let requested_once = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handler: SharedToolHandler = Arc::new(move |_input, _ctx| {
            let gate = gate_in_tool.clone();
            let requested_once = requested_once.clone();
            Box::pin(async move {
                if !requested_once.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    gate.request();
                }
                Ok(serde_json::json!({ "ok": true }))
            })
        });
        let schema = ToolInputSchema {
            schema_type: SchemaType,
            properties: Default::default(),
            required: None,
            additional_properties: None,
            defs: None,
        };
        let tool = Tool::builder("ping", "test ping", schema, handler).build();
        let tm = create_tool_manager();
        tm.register(tool, None);

        // 恒定返回同一 tool call，turn 在工具循环里打乒乓；若 pause
        // gate 不生效，turn 会迅速撞死循环熔断（同参数连续 5 次）。
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
        let mut runner = AgentRunner::new_with_tools(agent, tm).with_pause_gate(gate.clone());

        let turn = tokio::spawn(async move { runner.run_turn(&[Message::user("go")], None).await });

        // 等 pause 置位（第一轮工具执行后）。
        for _ in 0..100 {
            if gate.is_requested() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(gate.is_requested(), "tool handler requested the pause");
        // runner 应在 round-1 边界挂起：给足够时间它也不该完成
        // （没有 gate 时 4 轮乒乓远早于 300ms 撞上限）。
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(!turn.is_finished(), "runner suspended at the tool-round boundary");
        assert!(
            server.received_requests().await.unwrap().len() == 1,
            "no second model call while paused: {}",
            server.received_requests().await.unwrap().len()
        );

        // 用户拍板（继续）→ resolve → turn 恢复并继续跑工具循环，
        // 最终由死循环探测器收尾：桩模型每轮都发**逐字节相同**的
        // `ping {}`，连续 5 次即熔断。
        //
        // 这里原本断言 `MaxToolRoundsExceeded { rounds: 4 }`。轮次上限
        // 在 deadline-only 模式下已退化为 soft warning、该错误也不再被
        // 构造，所以改断言死循环熔断——它现在是这条路径上唯一的自动
        // 刹车（本测试没设 deadline，也没有人工 ⏸）。
        gate.resolve();
        let err = tokio::time::timeout(std::time::Duration::from_secs(10), turn)
            .await
            .expect("turn resumes after resolve")
            .unwrap()
            .expect_err("identical repeated tool calls trip the loop detector");
        match err {
            AgentError::ToolLoopDetected { tool, reason, .. } => {
                assert_eq!(tool, "ping", "熔断要点名肇事工具");
                assert!(
                    reason.contains("identical args"),
                    "原因要说明是同参数重复: {reason}"
                );
            }
            other => panic!("期望 ToolLoopDetected，实际 {other:?}"),
        }
        assert!(
            server.received_requests().await.unwrap().len() >= 2,
            "tool loop continued after resume"
        );
    }

    /// Session-level pause gate（用户按 ⏸ 触发）：run_turn 入口
    /// park、tool 启动前 park。测试：pause 后 run_turn 永远不
    /// 完成；resolve 后才走完。
    #[tokio::test]
    async fn agent_pause_gate_suspends_run_turn_entry_until_resumed() {
        use crate::pause_gate::AgentPauseGate;
        use latte_rs_agent_tools::prelude::create_tool_manager;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_completion_body("hi", vec![])))
            .mount(&server)
            .await;
        let role = test_role();
        let agent = Agent::new_with_chain(
            "t".into(),
            role,
            vec![model_at(&server, "stub")],
            GenerateParams::default(),
        )
        .unwrap();
        let tm = create_tool_manager();
        let gate = AgentPauseGate::new("test");
        // Pre-pause（模拟用户先按 ⏸ 后才发消息）。
        gate.pause();
        let mut runner =
            AgentRunner::new_with_tools(agent, tm).with_agent_pause_gate(gate.clone());
        let turn = tokio::spawn(async move { runner.run_turn(&[Message::user("go")], None).await });
        // 200ms 后仍不完成（gate 在 park）。
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(!turn.is_finished(), "run_turn parked at entry on pre-paused gate");
        // resolve → 跑完。
        gate.resume();
        let r = tokio::time::timeout(std::time::Duration::from_secs(5), turn)
            .await
            .expect("turn resumes after gate resume")
            .expect("turn join ok");
        assert_eq!(r.unwrap(), "hi");
    }

    /// 用户在 tool exec 期间按 ⏸：当前 tool 跑完（in-flight 不打断），
    /// 下一个 model call 之前在 gate 处 park；resume 后继续。
    #[tokio::test]
    async fn agent_pause_gate_lets_in_flight_tool_finish() {
        use crate::pause_gate::AgentPauseGate;
        use latte_rs_agent_tools::prelude::create_tool_manager;
        use latte_rs_agent_tools::types::{
            SchemaType, SharedToolHandler, Tool, ToolInputSchema, ToolManager as _,
        };
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let gate = AgentPauseGate::new("test");
        // 工具 handler 扮演"用户按暂停"：第一个 tool 执行时 engage
        // gate（in-flight tool 已启动、会跑完），turn 在下一轮
        // model call 边界 park。
        let gate_in_tool = gate.clone();
        let requested_once = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handler: SharedToolHandler = Arc::new(move |_input, _ctx| {
            let gate = gate_in_tool.clone();
            let once = requested_once.clone();
            Box::pin(async move {
                if !once.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    gate.pause();
                }
                Ok(serde_json::json!({ "ok": true }))
            })
        });
        let schema = ToolInputSchema {
            schema_type: SchemaType,
            properties: Default::default(),
            required: None,
            additional_properties: None,
            defs: None,
        };
        let tool = Tool::builder("ping", "test ping", schema, handler).build();
        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_completion_body(
                "",
                vec![serde_json::json!({
                    "id": "call_ping",
                    "type": "function",
                    "function": {"name": "ping", "arguments": "{}"}
                })],
            )))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_completion_body("done", vec![])))
            .mount(&server)
            .await;
        let tm = create_tool_manager();
        tm.register(tool, None);
        let role = test_role();
        let agent = Agent::new_with_chain(
            "t".into(),
            role,
            vec![model_at(&server, "stub")],
            GenerateParams::default(),
        )
        .unwrap();
        let mut runner =
            AgentRunner::new_with_tools(agent, tm).with_agent_pause_gate(gate.clone());
        let turn = tokio::spawn(async move { runner.run_turn(&[Message::user("go")], None).await });
        // 第一个 tool 已启动、in-flight 会跑完，但 tool 内部 engage
        // 了 gate → 下个 model call 边界 park。
        for _ in 0..100 {
            if gate.is_paused() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(gate.is_paused(), "tool handler engaged the gate");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(!turn.is_finished(), "in-flight tool 跑完，但下个 model call park");
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "in-flight tool 跑完但没有第二次模型请求（parked）"
        );
        // Resume → 第二次模型请求 → 返回 text。
        gate.resume();
        let r = tokio::time::timeout(std::time::Duration::from_secs(5), turn)
            .await
            .expect("turn resumes")
            .expect("turn join ok")
            .expect("run ok");
        assert_eq!(r, "done");
        assert!(
            server.received_requests().await.unwrap().len() >= 2,
            "resume 后跑了第二次模型请求"
        );
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
            defs: None,
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
        let mut runner = AgentRunner::new_with_tools(agent, tm);

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
        // 协议闭环：MalformedArgs 也以 tool 消息回填 call_ping，否则
        // assistant 的 tool_calls 没有对应 tool 消息，下游 API 直接 400。
        assert!(
            body2.contains("\"tool_call_id\":\"call_ping\""),
            "MalformedArgs must still close the tool_call_id loop: {body2}"
        );
        assert!(
            body2.contains("invalid JSON"),
            "error detail should be fed back so the model stops retrying: {body2}"
        );
    }

    // ─── run_turn_gated：advisor pre-persistence gate 重试链路 ──────

    /// Gate 命中 → hint 注入 → 重跑通过 = 纠偏成功，流程正常继续。
    #[tokio::test]
    async fn run_turn_gated_retries_with_hint_then_passes() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        // Round 0（先注册先匹配，只生效一次）：短输出，撞 D5
        // （4 chars < 阈值 50，且不是明确的短 ack）。
        struct FirstOnly(std::sync::atomic::AtomicUsize);
        impl wiremock::Match for FirstOnly {
            fn matches(&self, _req: &wiremock::Request) -> bool {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0
            }
        }
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .and(FirstOnly(std::sync::atomic::AtomicUsize::new(0)))
                    .respond_with(ResponseTemplate::new(200)
                        .set_body_string(openai_completion_body("TODO", vec![]))),
            )
            .await;
        // 兜底：纠偏后的正常长回答。
        let good = "这是纠偏后的完整回答：逐条说明结论、依据和后续步骤，长度远超五十字符的 D5 阈值。";
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200)
                        .set_body_string(openai_completion_body(good, vec![]))),
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
        let hints = Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let mut runner = AgentRunner::new(agent)
            .with_advisor_hints(hints.clone())
            .with_gate_config(crate::advisor_monitor::GateConfig::default());

        let resp = runner
            .run_turn_gated(&[Message::user("go")], None)
            .await
            .expect("gate retry should pass on the corrected response");
        assert_eq!(resp, good, "纠偏后的回答被接受，流程正常继续（继续语义）");

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 2, "1 failed attempt + 1 retry: {}", reqs.len());
        let body2 = String::from_utf8_lossy(&reqs[1].body);
        assert!(
            body2.contains("[advisor gate D5]"),
            "retry request carries the gate annotation hint: {body2}"
        );
        assert!(hints.lock().is_empty(), "hint drained into the retry context");
    }

    /// Gate 重试耗尽 → AgentError::AdvisorTerminated 向上传播。
    #[tokio::test]
    async fn run_turn_gated_exhausts_retries_raises_advisor_terminated() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        // 所有响应都撞 D5：模型屡教不改。
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200)
                        .set_body_string(openai_completion_body("TODO", vec![]))),
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
        let hints = Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let mut runner = AgentRunner::new(agent)
            .with_advisor_hints(hints)
            .with_gate_config(crate::advisor_monitor::GateConfig::default());

        let err = runner
            .run_turn_gated(&[Message::user("go")], None)
            .await
            .expect_err("retries exhausted must raise AdvisorTerminated");
        match err {
            AgentError::AdvisorTerminated { reason, detector } => {
                assert_eq!(detector, "D5", "detector label propagates");
                assert!(reason.contains("D5"), "reason names the detector: {reason}");
            }
            other => panic!("expected AdvisorTerminated, got {other:?}"),
        }
        // 默认 max_retries=2：首次 + 2 次重试 = 3 次模型调用。
        let n = server.received_requests().await.unwrap().len();
        assert_eq!(n, 3, "1 initial + max_retries(2) retries, got {n}");
    }

    /// 未装 gate_config 时 run_turn_gated 等价 run_turn（向后兼容：
    /// 未启用 advisor 的场景行为不变，短输出照样接受、不重试）。
    #[tokio::test]
    async fn run_turn_gated_without_config_behaves_like_run_turn() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200)
                        .set_body_string(openai_completion_body("TODO", vec![]))),
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
        let mut runner = AgentRunner::new(agent); // no with_gate_config

        let resp = runner
            .run_turn_gated(&[Message::user("go")], None)
            .await
            .expect("no gate → plain run_turn behavior");
        assert_eq!(resp, "TODO", "short output accepted without a gate");
        let n = server.received_requests().await.unwrap().len();
        assert_eq!(n, 1, "no gate → no retry, got {n}");
    }

    #[test]
    fn short_tool_name_strips_namespace() {
        assert_eq!(short_tool_name("delegate"), "delegate");
        assert_eq!(short_tool_name("manager.delegate"), "delegate");
        assert_eq!(short_tool_name("a.b.delegate"), "delegate");
        assert_eq!(short_tool_name("read"), "read");
    }

    /// End-to-end proof that a human-blocking call can no longer strand
    /// its siblings: 一轮里 `workflow{interactive}`（内部会 `ask`）排在
    /// `workflow{plain}` 前面，引擎必须先跑 plain。
    ///
    /// 构造成"interactive 的 handler 一直等到 plain 跑完才返回"——所以
    /// 在**修复前**的严格按下标串行下这个测试会直接死等超时（正是实测事故：
    /// `implementation_plan` 被 `learn_loop` 的弹窗永久押在队列里）。
    #[tokio::test]
    async fn blocking_workflow_call_does_not_strand_its_sibling() {
        use latte_rs_agent_tools::prelude::create_tool_manager;
        use latte_rs_agent_tools::types::{
            SchemaType, SharedToolHandler, Tool, ToolInputSchema, ToolManager as _,
        };
        use std::time::Duration;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        // workflow 定义 fixture：interactive 步声明了 ask，plain 没有。
        let dir = tempfile::tempdir().expect("tempdir");
        let wf_dir = dir.path().join(".latte").join("workflows.d");
        std::fs::create_dir_all(&wf_dir).expect("mkdir");
        std::fs::write(
            wf_dir.join("interactive.toml"),
            "name = \"interactive\"\ndescription = \"d\"\n[[steps]]\nid = \"quiz\"\nrole = \"tutor\"\ntask = \"t\"\ntools = [\"ask\"]\n",
        )
        .unwrap();
        std::fs::write(
            wf_dir.join("plain.toml"),
            "name = \"plain\"\ndescription = \"d\"\n[[steps]]\nid = \"plan\"\nrole = \"pm\"\ntask = \"t\"\ntools = [\"write\"]\n",
        )
        .unwrap();

        // plain 跑完才 notify；interactive 的 handler 等这个通知。
        let plain_done = Arc::new(tokio::sync::Notify::new());
        let order: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let plain_done_h = plain_done.clone();
        let order_h = order.clone();
        let handler: SharedToolHandler = Arc::new(move |input, _ctx| {
            let plain_done = plain_done_h.clone();
            let order = order_h.clone();
            let name = input
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
            Box::pin(async move {
                if name == "interactive" {
                    // 模拟"等人点弹窗"：这里等的是 plain 完成的信号。
                    plain_done.notified().await;
                } else {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    plain_done.notify_one();
                }
                order.lock().push(name.clone());
                Ok(serde_json::json!({ "ok": name }))
            })
        });
        let tool = Tool::builder(
            "workflow",
            "test workflow",
            ToolInputSchema {
                schema_type: SchemaType,
                properties: Default::default(),
                required: None,
                additional_properties: None,
                defs: None,
            },
            handler,
        )
        .build();
        let tm = create_tool_manager();
        tm.register(tool, None);

        let server = wiremock::MockServer::start().await;
        struct FirstOnly(std::sync::atomic::AtomicUsize);
        impl wiremock::Match for FirstOnly {
            fn matches(&self, _req: &wiremock::Request) -> bool {
                self.0
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    == 0
            }
        }
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .and(FirstOnly(std::sync::atomic::AtomicUsize::new(0)))
                    .respond_with(ResponseTemplate::new(200).set_body_string(
                        openai_completion_body(
                            "",
                            vec![
                                serde_json::json!({
                                    "id": "call_i",
                                    "type": "function",
                                    "function": {
                                        "name": "workflow",
                                        "arguments": "{\"name\":\"interactive\",\"topic\":\"learn\"}"
                                    }
                                }),
                                serde_json::json!({
                                    "id": "call_p",
                                    "type": "function",
                                    "function": {
                                        "name": "workflow",
                                        "arguments": "{\"name\":\"plain\",\"topic\":\"split tasks\"}"
                                    }
                                }),
                            ],
                        ),
                    )),
            )
            .await;
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

        let agent = Agent::new_with_chain(
            "mgr".into(),
            test_role(),
            vec![model_at(&server, "stub")],
            GenerateParams::default(),
        )
        .unwrap();
        let mut runner = AgentRunner::new_with_tools(agent, tm).with_cwd(dir.path().to_path_buf());
        let resp = tokio::time::timeout(
            Duration::from_secs(10),
            runner.run_turn(&[Message::user("go")], None),
        )
        .await
        .expect("阻塞调用排到最后后本轮必须能收尾——超时说明兄弟调用又被押住了")
        .expect("turn completes");
        assert_eq!(resp, "done");
        assert_eq!(
            *order.lock(),
            vec!["plain".to_string(), "interactive".to_string()],
            "不等人的派发必须先跑完，等人的排最后"
        );
    }

    /// End-to-end proof of the opt-in delegate concurrency:
    ///   - `LATTE_AGENT_DELEGATE_PARALLEL` unset → the two delegate
    ///     calls in one response run **serially** (max observed
    ///     concurrency = 1) — the default, unchanged behavior.
    ///   - flag on → they run **concurrently** (max observed
    ///     concurrency = 2).
    /// Both phases must still feed back both tool_results and finish
    /// with the round-1 "done" reply. Env is mutated only by this test.
    #[tokio::test]
    async fn delegate_batch_runs_serial_by_default_concurrent_when_enabled() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        use latte_rs_agent_tools::prelude::create_tool_manager;
        use latte_rs_agent_tools::types::{
            SchemaType, SharedToolHandler, Tool, ToolInputSchema, ToolManager as _,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        // Runs one turn where round 0 emits two `delegate` calls and
        // round 1 replies "done". Returns the max concurrency the tool
        // handler observed. Each phase gets its own server / tm /
        // counters so wiremock's one-shot matcher is fresh.
        async fn run_phase() -> usize {
            let active = Arc::new(AtomicUsize::new(0));
            let max_seen = Arc::new(AtomicUsize::new(0));
            let total = Arc::new(AtomicUsize::new(0));
            let active_h = active.clone();
            let max_h = max_seen.clone();
            let total_h = total.clone();
            let handler: SharedToolHandler = Arc::new(move |_input, _ctx| {
                let active = active_h.clone();
                let max_seen = max_h.clone();
                let total = total_h.clone();
                Box::pin(async move {
                    let cur = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_seen.fetch_max(cur, Ordering::SeqCst);
                    // Hold the "slot" long enough that a concurrent
                    // sibling overlaps; short enough to keep the test fast.
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    total.fetch_add(1, Ordering::SeqCst);
                    Ok(serde_json::json!({ "ok": true }))
                })
            });
            let schema = ToolInputSchema {
                schema_type: SchemaType,
                properties: Default::default(),
                required: None,
                additional_properties: None,
                defs: None,
            };
            let tool = Tool::builder("delegate", "test delegate", schema, handler).build();
            let tm = create_tool_manager();
            tm.register(tool, None);

            let server = wiremock::MockServer::start().await;
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
                                "",
                                vec![
                                    serde_json::json!({
                                        "id": "call_a",
                                        "type": "function",
                                        "function": {
                                            "name": "delegate",
                                            "arguments": "{\"role\":\"programmer\",\"task\":\"a\"}"
                                        }
                                    }),
                                    serde_json::json!({
                                        "id": "call_b",
                                        "type": "function",
                                        "function": {
                                            "name": "delegate",
                                            "arguments": "{\"role\":\"architect\",\"task\":\"b\"}"
                                        }
                                    }),
                                ],
                            ),
                        )),
                )
                .await;
            server
                .register(
                    Mock::given(method("POST"))
                        .and(path("/chat/completions"))
                        .respond_with(ResponseTemplate::new(200).set_body_string(
                            openai_completion_body("done", vec![]),
                        )),
                )
                .await;

            let role = test_role();
            let agent = Agent::new_with_chain(
                "mgr".into(),
                role,
                vec![model_at(&server, "stub")],
                GenerateParams::default(),
            )
            .unwrap();
            let mut runner = AgentRunner::new_with_tools(agent, tm);
            let resp = runner
                .run_turn(&[Message::user("go")], None)
                .await
                .expect("turn completes");
            assert_eq!(resp, "done");
            assert_eq!(total.load(Ordering::SeqCst), 2, "both delegates must run");
            max_seen.load(Ordering::SeqCst)
        }

        // Phase 1: default (flag unset) → serial.
        std::env::remove_var("LATTE_AGENT_DELEGATE_PARALLEL");
        let serial_max = run_phase().await;
        assert_eq!(serial_max, 1, "default must be serial (max concurrency 1)");

        // Phase 2: flag on → concurrent.
        std::env::set_var("LATTE_AGENT_DELEGATE_PARALLEL", "1");
        let parallel_max = run_phase().await;
        std::env::remove_var("LATTE_AGENT_DELEGATE_PARALLEL");
        assert_eq!(parallel_max, 2, "flag on must run the two delegates concurrently");
    }

    /// [`human_blocking_last_order`]：会等人点弹窗的调用必须排到最后，
    /// 且只在整批都是派发调用时才敢重排。
    ///
    /// 回归防线（jemalloc 会话 ui-87189-1788067528852-0，事件 220/221）：
    /// manager 同一轮发了 `workflow{learn_loop}` + `workflow{implementation_plan}`，
    /// 前者第三步 `ask` 弹窗挂住不返回（无超时），后者一次都没执行 ——
    /// 用户点名要的"拆分任务"被一个人机交互无限期押在队列里。
    #[test]
    fn human_blocking_calls_are_reordered_last() {
        fn c(name: &str, args: &str) -> ParsedCall {
            ParsedCall { id: String::new(), name: name.into(), args: args.into() }
        }
        // workflow 定义 fixture：interactive 步里有 ask，plain 没有。
        let dir = tempfile::tempdir().expect("tempdir");
        let wf_dir = dir.path().join(".latte").join("workflows.d");
        std::fs::create_dir_all(&wf_dir).expect("mkdir workflows.d");
        std::fs::write(
            wf_dir.join("interactive.toml"),
            r#"
name = "interactive"
description = "有弹窗的流水线"
[[steps]]
id = "quiz"
role = "tutor"
task = "出题"
tools = ["read", "write", "ask"]
require_tools = ["ask"]
"#,
        )
        .expect("write interactive.toml");
        std::fs::write(
            wf_dir.join("plain.toml"),
            r#"
name = "plain"
description = "无弹窗的流水线"
[[steps]]
id = "plan"
role = "pm"
task = "拆任务"
tools = ["read", "write"]
"#,
        )
        .expect("write plain.toml");
        let cwd = Some(dir.path());

        // 事故原样重放：交互式 workflow 在前 → 必须换到最后。
        assert_eq!(
            human_blocking_last_order(
                &[
                    c("workflow", r#"{"name":"interactive","topic":"x"}"#),
                    c("workflow", r#"{"name":"plain","topic":"y"}"#),
                ],
                cwd,
            ),
            Some(vec![1, 0]),
            "内部会 ask 的 workflow 必须排到最后"
        );

        // 已经在最后 → 不必重排（走原有串行路径，少一层包装）。
        assert_eq!(
            human_blocking_last_order(
                &[
                    c("workflow", r#"{"name":"plain","topic":"y"}"#),
                    c("workflow", r#"{"name":"interactive","topic":"x"}"#),
                ],
                cwd,
            ),
            None
        );

        // 裸 ask 调用同样算阻塞，派发调用先跑。
        assert_eq!(
            human_blocking_last_order(
                &[c("ask", "{}"), c("delegate", r#"{"role":"programmer"}"#)],
                cwd,
            ),
            Some(vec![1, 0])
        );

        // 保守边界：批里出现非派发调用（read/write/bash/edit）→ 整批不动。
        // 副作用可见性（bash 写 → read 读）绝不能被顺序调整破坏。
        for other in ["read", "write", "bash", "edit"] {
            assert_eq!(
                human_blocking_last_order(
                    &[
                        c("workflow", r#"{"name":"interactive","topic":"x"}"#),
                        c(other, "{}"),
                    ],
                    cwd,
                ),
                None,
                "批里有 {other} 时不许重排"
            );
        }

        // 没有阻塞调用 / 全是阻塞调用 / 单个调用 → 都不重排。
        assert_eq!(
            human_blocking_last_order(
                &[
                    c("workflow", r#"{"name":"plain","topic":"a"}"#),
                    c("workflow", r#"{"name":"plain","topic":"b"}"#),
                ],
                cwd,
            ),
            None
        );
        assert_eq!(
            human_blocking_last_order(
                &[
                    c("workflow", r#"{"name":"interactive","topic":"a"}"#),
                    c("ask", "{}"),
                ],
                cwd,
            ),
            None
        );
        assert_eq!(
            human_blocking_last_order(&[c("ask", "{}")], cwd),
            None
        );

        // 名字加载不到的 workflow 按"不阻塞"处理：判断只影响顺序，
        // 猜不出时退回现状，绝不因此报错。
        assert_eq!(
            human_blocking_last_order(
                &[
                    c("workflow", r#"{"name":"nope","topic":"a"}"#),
                    c("workflow", r#"{"name":"plain","topic":"b"}"#),
                ],
                cwd,
            ),
            None
        );

        // 三个调用：交互式夹在中间 → 其余保持相对顺序，它去最后。
        assert_eq!(
            human_blocking_last_order(
                &[
                    c("delegate", r#"{"role":"architect"}"#),
                    c("workflow", r#"{"name":"interactive","topic":"x"}"#),
                    c("workflow", r#"{"name":"plain","topic":"y"}"#),
                ],
                cwd,
            ),
            Some(vec![0, 2, 1])
        );
    }

    /// 逃生口：`LATTE_AGENT_BLOCKING_CALLS_LAST=0` 退回严格按下标串行。
    #[test]
    fn blocking_calls_last_env_escape_hatch() {
        std::env::remove_var("LATTE_AGENT_BLOCKING_CALLS_LAST");
        assert!(blocking_calls_last_enabled(), "默认开");
        for off in ["0", "false", "no", "off", "OFF"] {
            std::env::set_var("LATTE_AGENT_BLOCKING_CALLS_LAST", off);
            assert!(!blocking_calls_last_enabled(), "{off} 应关掉重排");
        }
        std::env::set_var("LATTE_AGENT_BLOCKING_CALLS_LAST", "1");
        assert!(blocking_calls_last_enabled());
        std::env::remove_var("LATTE_AGENT_BLOCKING_CALLS_LAST");
    }

    /// [`readonly_parallel_runs`] 的分段语义：
    ///   - 只返回长度 ≥2 的**连续**只读区间；
    ///   - 任何非只读工具都是屏障，前后不合并（保证
    ///     `bash 写文件 → read 读它` 的顺序不被重排）。
    #[test]
    fn readonly_parallel_runs_splits_on_write_barriers() {
        fn c(name: &str) -> ParsedCall {
            ParsedCall { id: String::new(), name: name.into(), args: "{}".into() }
        }
        // 全是只读 → 一整段。
        assert_eq!(
            readonly_parallel_runs(&[c("read"), c("code_graph"), c("read")]),
            vec![(0, 3)]
        );
        // 单个只读不成段（并发无意义）。
        assert_eq!(readonly_parallel_runs(&[c("read")]), vec![]);
        // bash 是屏障：两侧各 1 个 read，都不成段 → 全串行。
        assert_eq!(
            readonly_parallel_runs(&[c("read"), c("bash"), c("read")]),
            vec![]
        );
        // 屏障两侧各 2 个 read → 两段，互不跨越屏障。
        assert_eq!(
            readonly_parallel_runs(&[
                c("read"),
                c("read"),
                c("write"),
                c("code_graph"),
                c("read"),
            ]),
            vec![(0, 2), (3, 5)]
        );
        // delegate / edit 不属于只读表。
        assert_eq!(
            readonly_parallel_runs(&[c("delegate"), c("delegate"), c("edit")]),
            vec![]
        );
        // 带 namespace 的遗留全名按短名识别。
        assert_eq!(
            readonly_parallel_runs(&[c("fs.read"), c("graph.code_graph")]),
            vec![(0, 2)]
        );
    }

    /// End-to-end proof of the read-only fan-out:
    ///   - 默认（`LATTE_AGENT_READONLY_PARALLEL` unset）→ 一轮里的 3 个
    ///     `read` **并发**执行（观测到的最大并发 = 3）；
    ///   - `LATTE_AGENT_READONLY_PARALLEL=0` → 退回**串行**（最大并发 1）。
    /// 两种情况都必须回填 3 条 tool_result 并以第 1 轮的 "done" 收尾。
    #[tokio::test]
    async fn readonly_batch_runs_concurrently_by_default_serial_when_disabled() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        use latte_rs_agent_tools::prelude::create_tool_manager;
        use latte_rs_agent_tools::types::{
            SchemaType, SharedToolHandler, Tool, ToolInputSchema, ToolManager as _,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        async fn run_phase() -> usize {
            let active = Arc::new(AtomicUsize::new(0));
            let max_seen = Arc::new(AtomicUsize::new(0));
            let total = Arc::new(AtomicUsize::new(0));
            let active_h = active.clone();
            let max_h = max_seen.clone();
            let total_h = total.clone();
            let handler: SharedToolHandler = Arc::new(move |_input, _ctx| {
                let active = active_h.clone();
                let max_seen = max_h.clone();
                let total = total_h.clone();
                Box::pin(async move {
                    let cur = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_seen.fetch_max(cur, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    total.fetch_add(1, Ordering::SeqCst);
                    Ok(serde_json::json!({ "ok": true }))
                })
            });
            let schema = ToolInputSchema {
                schema_type: SchemaType,
                properties: Default::default(),
                required: None,
                additional_properties: None,
                defs: None,
            };
            // 名字必须是 `read` —— 只读并发靠 READONLY_PARALLEL_TOOLS 白名单
            // 识别，不是靠「工具看起来只读」猜的。
            let tool = Tool::builder("read", "test read", schema, handler).build();
            let tm = create_tool_manager();
            tm.register(tool, None);

            let server = wiremock::MockServer::start().await;
            struct FirstOnly(std::sync::atomic::AtomicUsize);
            impl wiremock::Match for FirstOnly {
                fn matches(&self, _req: &wiremock::Request) -> bool {
                    self.0.fetch_add(1, Ordering::SeqCst) == 0
                }
            }
            let call = |id: &str, p: &str| {
                serde_json::json!({
                    "id": id,
                    "type": "function",
                    "function": { "name": "read", "arguments": format!("{{\"path\":\"{p}\"}}") }
                })
            };
            server
                .register(
                    Mock::given(method("POST"))
                        .and(path("/chat/completions"))
                        .and(FirstOnly(std::sync::atomic::AtomicUsize::new(0)))
                        .respond_with(ResponseTemplate::new(200).set_body_string(
                            openai_completion_body(
                                "",
                                vec![
                                    call("call_a", "/tmp/a"),
                                    call("call_b", "/tmp/b"),
                                    call("call_c", "/tmp/c"),
                                ],
                            ),
                        )),
                )
                .await;
            server
                .register(
                    Mock::given(method("POST"))
                        .and(path("/chat/completions"))
                        .respond_with(ResponseTemplate::new(200).set_body_string(
                            openai_completion_body("done", vec![]),
                        )),
                )
                .await;

            let role = test_role();
            let agent = Agent::new_with_chain(
                "prog".into(),
                role,
                vec![model_at(&server, "stub")],
                GenerateParams::default(),
            )
            .unwrap();
            let mut runner = AgentRunner::new_with_tools(agent, tm);
            let resp = runner
                .run_turn(&[Message::user("go")], None)
                .await
                .expect("turn completes");
            assert_eq!(resp, "done");
            assert_eq!(total.load(Ordering::SeqCst), 3, "all three reads must run");
            assert_eq!(
                runner.last_turn_tool_count, 3,
                "记账必须与串行路径一致：3 次成功计数"
            );
            max_seen.load(Ordering::SeqCst)
        }

        // Phase 1: 默认 → 并发。
        std::env::remove_var("LATTE_AGENT_READONLY_PARALLEL");
        let parallel_max = run_phase().await;
        assert_eq!(parallel_max, 3, "默认应并发执行同一轮的 3 个 read");

        // Phase 2: 显式关闭 → 串行。
        std::env::set_var("LATTE_AGENT_READONLY_PARALLEL", "0");
        let serial_max = run_phase().await;
        std::env::remove_var("LATTE_AGENT_READONLY_PARALLEL");
        assert_eq!(serial_max, 1, "关闭后必须退回串行（最大并发 1）");
    }

    /// 批量读的 `paths` 数组也要按 cwd 改写，否则会出现「单路径按 cwd
    /// 解析、批量路径不解析」的不对称。绝对路径 / URL / `~` 一律不动。
    #[test]
    fn cwd_rewrite_covers_paths_array() {
        let cwd = std::path::Path::new("/work/repo");
        let out = resolve_tool_input_against_cwd(
            serde_json::json!({"paths":["src/a.rs","/abs/b.rs","https://x/y","~/c.rs"]}),
            cwd,
        );
        let arr = out["paths"].as_array().unwrap();
        assert_eq!(arr[0], "/work/repo/src/a.rs", "相对路径要拼 cwd");
        assert_eq!(arr[1], "/abs/b.rs", "绝对路径不动");
        assert_eq!(arr[2], "https://x/y", "URL 不动");
        assert_eq!(arr[3], "~/c.rs", "~ 不动");
    }

    /// 模型把批量路径误写进单数字段（`path: ["a","b"]`）时自动搬到 `paths`。
    ///
    /// 校验器没有 union 类型，不纠正就是 Validation 失败 → 归为不可重试 →
    /// 白烧一次往返，而本次改造的全部目的就是省往返。规则仍是 schema 驱动：
    /// 必须「K 声明 String」+「K+s 声明 Array」+「值是全字符串数组」+
    /// 「K+s 尚未出现」四条同时成立。
    #[test]
    fn coerce_moves_string_array_to_plural_array_field() {
        use latte_rs_agent_tools::types::{
            PropertyType, SchemaType, ToolInputProperty, ToolInputSchema,
        };
        fn p(ty: PropertyType) -> ToolInputProperty {
            ToolInputProperty { property_type: ty,
            description: None,
            enum_values: None,
            minimum: None,
            maximum: None,
            min_length: None,
            max_length: None, items: None, properties: None, required: None, additional_properties: None, ref_: None, }
        }
        let mut props = std::collections::BTreeMap::new();
        props.insert("path".to_string(), p(PropertyType::String));
        props.insert("paths".to_string(), p(PropertyType::Array));
        let schema = ToolInputSchema {
            schema_type: SchemaType,
            properties: props,
            required: None,
            additional_properties: None,
            defs: None,
        };

        // 搬移：path 收到字符串数组。
        let out = coerce_tool_input_to_schema(
            serde_json::json!({"path":["a.rs","b.rs"]}),
            &schema,
        );
        assert!(out.get("path").is_none(), "原字段应被搬走");
        assert_eq!(out["paths"], serde_json::json!(["a.rs", "b.rs"]));

        // 正常单路径不动。
        let out = coerce_tool_input_to_schema(serde_json::json!({"path":"a.rs"}), &schema);
        assert_eq!(out["path"], "a.rs");
        assert!(out.get("paths").is_none());

        // paths 已存在 → 不覆盖（不猜模型意图）。
        let out = coerce_tool_input_to_schema(
            serde_json::json!({"path":["a.rs"],"paths":["keep.rs"]}),
            &schema,
        );
        assert_eq!(out["paths"], serde_json::json!(["keep.rs"]));
        assert_eq!(out["path"], serde_json::json!(["a.rs"]));

        // 数组里有非字符串 → 不动，让校验照常报错。
        let out = coerce_tool_input_to_schema(serde_json::json!({"path":["a.rs",1]}), &schema);
        assert!(out.get("paths").is_none());
    }

    /// `parallel_tool_calls` 必须真的落到 wire 上（默认 `true`），并且
    /// `LATTE_AGENT_PARALLEL_TOOL_CALLS=omit` 能让它整条消失。
    ///
    /// 这条是路径 A 的核心断言：在实测会话里这个字段**根本没下发**，
    /// 走的是供应商默认，所以"模型不并行"从来不是被我们关掉的。加上它是
    /// 为了把意图显式化，并给不认这个字段的端点留 `omit` 逃生口。
    #[tokio::test]
    async fn parallel_tool_calls_lands_on_wire_and_can_be_omitted() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        use latte_rs_agent_tools::prelude::create_tool_manager;
        use latte_rs_agent_tools::types::{
            SchemaType, SharedToolHandler, Tool, ToolInputSchema, ToolManager as _,
        };

        /// 跑一轮（无工具调用直接收尾），返回请求体里 parallel_tool_calls 的值。
        async fn body_flag() -> Option<serde_json::Value> {
            let handler: SharedToolHandler =
                Arc::new(move |_i, _c| Box::pin(async move { Ok(serde_json::json!({})) }));
            let schema = ToolInputSchema {
                schema_type: SchemaType,
                properties: Default::default(),
                required: None,
                additional_properties: None,
                defs: None,
            };
            let tm = create_tool_manager();
            tm.register(Tool::builder("read", "t", schema, handler).build(), None);

            let server = wiremock::MockServer::start().await;
            server
                .register(
                    Mock::given(method("POST"))
                        .and(path("/chat/completions"))
                        .respond_with(ResponseTemplate::new(200).set_body_string(
                            openai_completion_body("done", vec![]),
                        )),
                )
                .await;
            let agent = Agent::new_with_chain(
                "prog".into(),
                test_role(),
                vec![model_at(&server, "stub")],
                GenerateParams::default(),
            )
            .unwrap();
            let mut runner = AgentRunner::new_with_tools(agent, tm);
            runner.run_turn(&[Message::user("go")], None).await.unwrap();
            let reqs = server.received_requests().await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
            // 顺带确认工具确实下发了——没工具时该字段本就不该出现。
            assert!(body["tools"].as_array().is_some_and(|a| !a.is_empty()));
            body.get("parallel_tool_calls").cloned()
        }

        std::env::remove_var("LATTE_AGENT_PARALLEL_TOOL_CALLS");
        assert_eq!(body_flag().await, Some(serde_json::json!(true)), "默认应显式下发 true");

        std::env::set_var("LATTE_AGENT_PARALLEL_TOOL_CALLS", "off");
        assert_eq!(body_flag().await, Some(serde_json::json!(false)), "关掉应下发 false");

        std::env::set_var("LATTE_AGENT_PARALLEL_TOOL_CALLS", "omit");
        assert_eq!(body_flag().await, None, "omit 时字段必须完全不出现");
        std::env::remove_var("LATTE_AGENT_PARALLEL_TOOL_CALLS");
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

    // ─── is_length_truncation tests ─────────────────────────────────
    //
    // 回归 jemalloc plan 事故：manager 提交 24 个任务的 plan 清单，两次
    // 撞 finish_reason=length（args 断在 436 / 12081 字符），下游只报
    // 一句 `invalid JSON`，模型据此原样重发再撞一次；第三次靠删掉几乎
    // 所有 subtasks 才挤进去，拆分粒度被这条静默截断吃掉。截断必须与
    // "模型写错 JSON 格式"区分开，回执里给出"分批/精简"的可执行对策。

    #[test]
    fn length_truncation_recognizes_provider_markers() {
        for s in ["length", "max_tokens", "max_output_tokens", "output_limit"] {
            assert!(is_length_truncation(s), "{s} 必须判为长度截断");
        }
    }

    #[test]
    fn length_truncation_is_case_and_space_insensitive() {
        assert!(is_length_truncation("  LENGTH "), "大小写/空白不应影响判定");
        assert!(is_length_truncation("Max_Tokens"));
    }

    #[test]
    fn length_truncation_rejects_normal_stop_reasons() {
        for s in ["stop", "tool_calls", "end_turn", "", "stop_sequence", "content_filter"] {
            assert!(!is_length_truncation(s), "{s:?} 不是长度截断");
        }
    }

    /// 构造一个 `finish_reason` 可指定的 OpenAI 非流式响应体。
    fn openai_completion_body_with_finish(content: &str, finish_reason: &str) -> String {
        serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 0,
            "model": "test",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": content },
                "finish_reason": finish_reason
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
        })
        .to_string()
    }

    /// 正文撞长度上限（`finish_reason=length`、无 tool_calls）必须**重试
    /// 一次**并带上压缩指令，而不是把半截产出当正常答复返回。
    ///
    /// 回归 2026-09-07 jemalloc 会话：architect 在 tasks_json 步撞 8192
    /// token 上限，半个 JSON 被静默采纳穿给终审，gate 连着三轮 REJECT
    /// （理由都是「JSON 被截断」），max_iterations 耗尽后整条流水线 failed。
    #[tokio::test]
    async fn length_truncated_final_response_is_retried_once() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        // 第一次返回被截断的半截 JSON，第二次返回完整产出。
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(
                        openai_completion_body_with_finish(
                            r#"{"tasks":[{"title":"T0","description":"半截"#,
                            "length",
                        ),
                    ))
                    .up_to_n_times(1),
            )
            .await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(
                        openai_completion_body_with_finish(r#"{"tasks":[]}"#, "stop"),
                    )),
            )
            .await;

        let agent = Agent::new_with_chain(
            "trunc".into(),
            test_role(),
            vec![model_at(&server, "stub")],
            GenerateParams::default(),
        )
        .unwrap();
        let mut runner = AgentRunner::new(agent);
        let out = runner.run_turn(&[Message::user("出任务清单")], None).await.unwrap();

        assert_eq!(
            out, r#"{"tasks":[]}"#,
            "应采纳重试后的完整产出，而不是第一次的半截 JSON"
        );
        assert!(
            !out.contains("半截"),
            "被截断的产出不该出现在最终答复里: {out}"
        );
        // 重试的追问必须讲清真实原因与对策（否则模型只会再写一份同样长
        // 的）。它进的是工具循环内的工作消息表、不落 runner context，所以
        // 检查**真正发出去的第二个请求**。
        let reqs = server.received_requests().await.expect("mock 应记录请求");
        assert_eq!(reqs.len(), 2, "应恰好重试一次（共 2 次模型调用）");
        let second = String::from_utf8_lossy(&reqs[1].body).to_string();
        assert!(
            second.contains("输出长度上限"),
            "第二次请求应带讲明截断原因的追问: {second}"
        );
        assert!(second.contains("finish_reason=length"), "{second}");
        assert!(
            second.contains("不要原样重写"),
            "追问必须给出「压缩」而非「重写」的对策: {second}"
        );
        assert!(
            second.contains("半截"),
            "被截断的那份产出要作为 assistant 消息回喂，模型才知道自己写到哪断了: {second}"
        );
    }

    /// 重试后仍被截断：照收（不死循环），但必须打上显式标记——最坏情况
    /// 下也不能是静默采纳。
    #[tokio::test]
    async fn persistently_truncated_response_is_annotated_not_silent() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(
                        openai_completion_body_with_finish("永远装不下的长清单开头", "length"),
                    )),
            )
            .await;

        let agent = Agent::new_with_chain(
            "trunc2".into(),
            test_role(),
            vec![model_at(&server, "stub")],
            GenerateParams::default(),
        )
        .unwrap();
        let mut runner = AgentRunner::new(agent);
        let out = runner.run_turn(&[Message::user("出任务清单")], None).await.unwrap();

        assert!(
            out.contains("⚠️ [产出被截断]"),
            "连续截断必须留下显式标记，实际: {out}"
        );
        assert!(out.contains("finish_reason=length"), "标记要带真实原因: {out}");
        assert!(
            out.contains("永远装不下的长清单开头"),
            "已产出的内容仍要保留（降级采纳，不是丢弃）: {out}"
        );
    }

    // ─── ensure_unique_tool_call_ids tests ──────────────────────────
    //
    // 回归：manager 并行委派同一角色的不同任务时，若 provider 给这些
    // 并行调用相同/空的 id，两条 tool_result 会撞在同一 tool_call_id
    // 上，模型只看到最后一个 → "只处理最后一个" bug。id 唯一化确保
    // 每个返回都能配回自己的调用。

    fn t_id(id: &str, name: &str, args: serde_json::Value) -> latte_ai::models::ToolCall {
        latte_ai::models::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args,
            arguments_raw: None,
            arguments_parse_error: None,
        }
    }

    #[test]
    fn unique_ids_fixes_empty_ids_on_parallel_same_role_delegates() {
        // 两次委派 programmer，不同任务，provider 给了空 id。
        let mut calls = vec![
            t_id("", "delegate", serde_json::json!({"role":"programmer","task":"A"})),
            t_id("", "delegate", serde_json::json!({"role":"programmer","task":"B"})),
        ];
        // dedupe 不会合并（args 不同），随后 id 唯一化。
        let mut deduped = dedupe_native_tool_calls(calls.drain(..).collect());
        assert_eq!(deduped.len(), 2, "不同任务不应被去重");
        ensure_unique_tool_call_ids(&mut deduped);
        assert!(!deduped[0].id.trim().is_empty());
        assert!(!deduped[1].id.trim().is_empty());
        assert_ne!(deduped[0].id, deduped[1].id, "两个调用必须拿到不同 id");
    }

    #[test]
    fn unique_ids_rewrites_duplicate_ids_keeping_first() {
        let mut calls = vec![
            t_id("dup", "delegate", serde_json::json!({"task":"A"})),
            t_id("dup", "delegate", serde_json::json!({"task":"B"})),
        ];
        ensure_unique_tool_call_ids(&mut calls);
        assert_eq!(calls[0].id, "dup", "首个保留原 id");
        assert_ne!(calls[1].id, "dup", "重复的第二个应被改写");
    }

    #[test]
    fn unique_ids_leaves_distinct_ids_untouched() {
        let mut calls = vec![
            t_id("call_a", "delegate", serde_json::json!({"task":"A"})),
            t_id("call_b", "delegate", serde_json::json!({"task":"B"})),
        ];
        ensure_unique_tool_call_ids(&mut calls);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[1].id, "call_b");
    }

    #[test]
    fn unique_ids_handles_synthesized_collision() {
        // 已有一个真实 id 恰好等于合成候选 "call_1"，空 id 的那个
        // 必须跳过它另取，不能撞车。
        let mut calls = vec![
            t_id("", "delegate", serde_json::json!({"task":"A"})),   // idx 0 → call_0
            t_id("call_1", "delegate", serde_json::json!({"task":"B"})), // 保留
            t_id("", "delegate", serde_json::json!({"task":"C"})),   // idx 2 → call_2 (不撞 call_1)
        ];
        ensure_unique_tool_call_ids(&mut calls);
        let ids: std::collections::HashSet<_> = calls.iter().map(|c| c.id.clone()).collect();
        assert_eq!(ids.len(), 3, "三个 id 全部唯一: {:?}", calls.iter().map(|c| &c.id).collect::<Vec<_>>());
        assert_eq!(calls[1].id, "call_1");
    }

    // ─── LoopDetector 报数修正测试 ───────────────────────────────────
    //
    // 历史 bug：触发 Break 时错误消息把次数多算 1（`self.streak + 1`）。
    // 修正后报的次数必须与实际连续调用次数一致。

    #[test]
    fn loop_detector_break_message_count_matches_actual_call() {
        let mut d = LoopDetector::default();
        let payload = r#"{"role":"programmer","task":"pwd"}"#;
        // 阈值 = LOOP_STREAK_THRESHOLD(5)，前 4 次不触发。
        for _ in 0..4 {
            let _ = d.record("delegate", payload);
        }
        let decision = d.record("delegate", payload);
        match decision {
            LoopDecision::Break(reason) => {
                assert!(
                    reason.contains("5 times in a row"),
                    "第 5 次触发应报 '5 times in a row'，实际：{reason}"
                );
                assert!(
                    !reason.contains("6 times"),
                    "不应出现 '6 times'（off-by-one），实际：{reason}"
                );
            }
            LoopDecision::Continue => panic!("第 5 次相同调用应该触发 Break"),
        }
    }

    /// 契约：**一个 detector 跨 round 共用，streak 必须累积**。
    ///
    /// 这条测试原先钉的是相反的契约（「每轮一个全新 detector」，
    /// 2026-07-10 的修复）。那个设计在 deadline-only 模式下等于关掉刹车：
    /// 模型一轮通常只发一个工具调用，每轮重置就意味着 streak 恒为 1，
    /// 跨轮死循环永远测不到，而轮次上限已退化为 soft warning、交互式
    /// chat 也不设 deadline —— 没有任何东西会兜住。
    ///
    /// 误杀合法重试的顾虑由判定键解决：键含 args_json，manager 每轮重写
    /// 任务描述就会清零（见 `loop_detector_resets_on_different_call`）；
    /// 真正逐字节相同地重复 5 次，就是死循环。
    #[test]
    fn loop_detector_accumulates_across_rounds() {
        let payload = r#"{"role":"programmer","task":"pwd"}"#;
        // 模拟 run_turn：detector 建在 round 循环**外**，每轮吐一个
        // 完全相同的调用。
        let mut d = LoopDetector::default();
        for round in 0..4 {
            assert!(
                matches!(d.record("delegate", payload), LoopDecision::Continue),
                "第 {round} 轮还不该触发（阈值 5）"
            );
        }
        assert!(
            matches!(d.record("delegate", payload), LoopDecision::Break(_)),
            "跨 4 轮累积后，第 5 次相同调用必须熔断"
        );
    }
    #[test]
    fn model_schema_recurses_without_tool_or_field_name_special_cases() {
        use latte_rs_agent_tools::types::{
            PropertyType, SchemaType, ToolAdditionalProperties, ToolDefinition,
            ToolInputProperty, ToolInputSchema,
        };

        fn property(property_type: PropertyType) -> ToolInputProperty {
            ToolInputProperty {
                property_type,
                description: None,
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
            }
        }

        let mut tags = property(PropertyType::Array)
            .with_items(property(PropertyType::Integer));
        tags.min_length = Some(2);
        tags.max_length = Some(4);
        let item = property(PropertyType::Object).with_object(
            [("tags".into(), tags)].into_iter().collect(),
            Some(vec!["tags".into()]),
            Some(ToolAdditionalProperties::Boolean(false)),
        );
        let mut entries = property(PropertyType::Array).with_items(item);
        entries.min_length = Some(1);
        let definition = ToolDefinition {
            name: "not_read".into(),
            description: "recursive test".into(),
            input_schema: ToolInputSchema {
                schema_type: SchemaType,
                properties: [("entries".into(), entries)].into_iter().collect(),
                required: Some(vec!["entries".into()]),
                additional_properties: Some(false),
                defs: None,
            },
            strict: None,
        };

        let wire = model_facing_tool_schema(&definition);
        let entries = &wire["properties"]["entries"];
        assert_eq!(entries["minItems"], 1);
        assert_eq!(entries["items"]["type"], "object");
        assert_eq!(entries["items"]["required"], serde_json::json!(["tags"]));
        assert_eq!(entries["items"]["additionalProperties"], false);
        let tags = &entries["items"]["properties"]["tags"];
        assert_eq!(tags["items"]["type"], "integer");
        assert_eq!(tags["minItems"], 2);
        assert_eq!(tags["maxItems"], 4);
        assert!(wire.to_string().find("min_length").is_none());
        assert!(wire.to_string().find("max_length").is_none());
    }

    #[tokio::test]
    async fn all_builtin_array_schemas_have_items() {
        fn inspect(node: &serde_json::Value, path: &str, missing: &mut Vec<String>) {
            let Some(object) = node.as_object() else {
                return;
            };
            if object.get("type").and_then(|value| value.as_str()) == Some("array")
                && !object.contains_key("items")
            {
                missing.push(path.to_string());
            }
            if let Some(properties) = object.get("properties").and_then(|value| value.as_object()) {
                for (name, child) in properties {
                    inspect(child, &format!("{path}.properties.{name}"), missing);
                }
            }
            for key in ["items", "additionalProperties"] {
                if let Some(child) = object.get(key) {
                    inspect(child, &format!("{path}.{key}"), missing);
                }
            }
        }

        let names: Vec<String> = crate::controller::full_tool_pool()
            .keys()
            .cloned()
            .collect();
        let manager = crate::controller::build_tool_manager(&names)
            .await
            .expect("build all tools");
        let schemas = build_tool_schemas(&manager);
        let mut missing = Vec::new();
        for tool in &schemas {
            inspect(&tool.parameters, &tool.name, &mut missing);
        }
        assert!(missing.is_empty(), "array schemas missing items: {missing:?}");
    }

    /// 工具 description 和 schema 必须进入真实 provider 请求，而不只是留在本地 registry。
    #[tokio::test]
    async fn tool_description_lands_on_openai_wire() {
        use latte_rs_agent_tools::prelude::create_tool_manager;
        use latte_rs_agent_tools::types::{
            PropertyType, SchemaType, SharedToolHandler, Tool, ToolInputProperty,
            ToolInputSchema, ToolManager as _,
        };
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        const SENTINEL: &str = "BATCH_INDEPENDENT_READS_WITH_PATHS";
        const PATHS_DESCRIPTION: &str = "batch paths field description";
        let handler: SharedToolHandler =
            Arc::new(move |_i, _c| Box::pin(async move { Ok(serde_json::json!({})) }));
        let mut schema = ToolInputSchema {
            schema_type: SchemaType,
            properties: Default::default(),
            required: None,
            additional_properties: None,
            defs: None,
        };
        schema.properties.insert(
            "paths".into(),
            ToolInputProperty { property_type: PropertyType::Array,
            description: Some(PATHS_DESCRIPTION.into()),
            enum_values: None,
            minimum: None,
            maximum: None,
            min_length: Some(1),
            max_length: Some(10),
            items: Some(Box::new(ToolInputProperty {
                property_type: PropertyType::String,
                description: None,
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
            })), properties: None, required: None, additional_properties: None, ref_: None, },
        );
        let tm = create_tool_manager();
        tm.register(Tool::builder("read", SENTINEL, schema, handler).build(), None);

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
        let agent = Agent::new_with_chain(
            "prog".into(),
            test_role(),
            vec![model_at(&server, "stub")],
            GenerateParams::default(),
        )
        .unwrap();
        let mut runner = AgentRunner::new_with_tools(agent, tm);
        runner.run_turn(&[Message::user("go")], None).await.unwrap();

        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        let description = body["tools"][0]["function"]["description"]
            .as_str()
            .expect("tool description on wire");
        assert_eq!(description, SENTINEL);
        let paths = &body["tools"][0]["function"]["parameters"]["properties"]["paths"];
        assert_eq!(paths["type"], "array");
        assert_eq!(paths["description"], PATHS_DESCRIPTION);
        assert_eq!(paths["minItems"], 1);
        assert_eq!(paths["maxItems"], 10);
        assert_eq!(paths["items"]["type"], "string");
    }

    #[tokio::test]
    async fn recursive_tool_schema_is_compatible_across_provider_wires() {
        use latte_rs_agent_tools::prelude::create_tool_manager;
        use latte_rs_agent_tools::types::{
            PropertyType, SchemaType, SharedToolHandler, Tool, ToolAdditionalProperties,
            ToolInputProperty, ToolInputSchema, ToolManager as _,
        };
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        fn property(property_type: PropertyType, description: &str) -> ToolInputProperty {
            ToolInputProperty {
                property_type,
                description: (!description.is_empty()).then(|| description.to_string()),
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
            }
        }

        fn recursive_schema_fixture() -> ToolInputSchema {
            let mut pattern = property(PropertyType::String, "Search pattern.");
            pattern.min_length = Some(2);
            pattern.max_length = Some(64);

            let mut mode = property(PropertyType::String, "Match mode.");
            mode.enum_values = Some(vec!["literal".into(), "regex".into()]);

            let mut limit = property(PropertyType::Integer, "Maximum matches.");
            limit.minimum = Some(1.0);
            limit.maximum = Some(100.0);

            let mut metadata_value = property(PropertyType::String, "Metadata value.");
            metadata_value.min_length = Some(1);
            metadata_value.max_length = Some(32);
            let metadata = property(PropertyType::Object, "Typed metadata map.")
                .with_additional_properties(metadata_value);

            let item = property(PropertyType::Object, "One query.").with_object(
                std::collections::BTreeMap::from([
                    ("limit".into(), limit),
                    ("metadata".into(), metadata),
                    ("mode".into(), mode),
                    ("pattern".into(), pattern),
                ]),
                Some(vec!["pattern".into()]),
                Some(ToolAdditionalProperties::Boolean(false)),
            );
            let mut queries = property(PropertyType::Array, "Queries to execute.")
                .with_items(item);
            queries.min_length = Some(2);
            queries.max_length = Some(4);

            ToolInputSchema {
                schema_type: SchemaType,
                properties: std::collections::BTreeMap::from([("queries".into(), queries)]),
                required: Some(vec!["queries".into()]),
                additional_properties: Some(false),
                defs: None,
            }
        }

        fn wire_tool_name(tool: &serde_json::Value) -> Option<&str> {
            tool.get("function")
                .and_then(|function| function.get("name"))
                .or_else(|| tool.get("name"))
                .and_then(serde_json::Value::as_str)
        }

        fn has_tool(body: &serde_json::Value, name: &str) -> bool {
            body["tools"]
                .as_array()
                .is_some_and(|tools| tools.iter().any(|tool| wire_tool_name(tool) == Some(name)))
        }

        async fn capture(
            api: latte_ai::models::ApiType,
            provider: &str,
            base_suffix: &str,
            request_path: &str,
        ) -> (serde_json::Value, serde_json::Value) {
            let server = wiremock::MockServer::start().await;
            let response = match api {
                latte_ai::models::ApiType::OpenAiCompletions => ResponseTemplate::new(200)
                    .set_body_string(openai_completion_body("done", vec![])),
                latte_ai::models::ApiType::AnthropicMessages => ResponseTemplate::new(200)
                    .set_body_raw(
                        concat!(
                            "event: content_block_delta\n",
                            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\n",
                            "event: message_delta\n",
                            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}\n\n",
                            "event: message_stop\n",
                            "data: {\"type\":\"message_stop\"}\n\n"
                        ),
                        "text/event-stream",
                    ),
            };
            server
                .register(
                    Mock::given(method("POST"))
                        .and(path(request_path))
                        .respond_with(response),
                )
                .await;

            let model = Model {
                id: format!("{provider}-schema-test"),
                name: format!("{provider} schema test"),
                api,
                provider: provider.into(),
                base_url: format!("{}{base_suffix}", server.uri()),
                api_key: "test-key".into(),
                context_window: 32_000,
                max_tokens: 4_096,
                omit_max_tokens: false,
                max_tokens_field: Default::default(),
                supports_thinking: false,
                supports_vision: false,
                cost_per_million_input: 0.0,
                cost_per_million_output: 0.0,
                timeout_secs: None,
            };
            let tm = create_tool_manager();
            let handler: SharedToolHandler = Arc::new(move |_input, _ctx| {
                Box::pin(async move { Ok(serde_json::json!({})) })
            });
            tm.register(
                Tool::builder(
                    "static_schema_matrix",
                    "Static recursive provider schema fixture",
                    recursive_schema_fixture(),
                    handler.clone(),
                )
                .build(),
                None,
            );

            let agent = Agent::new_with_chain(
                "provider-schema".into(),
                test_role(),
                vec![model],
                GenerateParams::default(),
            )
            .unwrap();
            let mut runner = AgentRunner::new_with_tools(agent, tm.clone());

            // First turn sees only the tool present when the runner was created.
            runner.run_turn(&[Message::user("static")], None).await.unwrap();

            // Register between turns: the next turn must rebuild model-facing schemas
            // and send this dynamically granted tool through the same provider path.
            tm.register(
                Tool::builder(
                    "dynamic_schema_matrix",
                    "Dynamic recursive provider schema fixture",
                    recursive_schema_fixture(),
                    handler,
                )
                .build(),
                None,
            );

            runner.run_turn(&[Message::user("dynamic")], None).await.unwrap();
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 2, "{provider} request count");
            let first: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
            assert!(
                has_tool(&first, "static_schema_matrix"),
                "{provider} first turn keeps the static tool"
            );
            assert!(
                !has_tool(&first, "dynamic_schema_matrix"),
                "{provider} first turn must precede dynamic registration"
            );

            let second: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
            assert!(
                has_tool(&second, "static_schema_matrix"),
                "{provider} second turn keeps the static tool"
            );
            assert!(
                has_tool(&second, "dynamic_schema_matrix"),
                "{provider} second turn picks up the dynamic tool"
            );
            (first, second)
        }

        fn find_tool<'a>(body: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
            body["tools"]
                .as_array()
                .expect("tools array")
                .iter()
                .find(|tool| wire_tool_name(tool) == Some(name))
                .unwrap_or_else(|| panic!("tool {name} on provider wire"))
        }

        fn assert_structure(schema: &serde_json::Value) {
            let queries = &schema["properties"]["queries"];
            let item = &queries["items"];
            assert_eq!(schema["type"], "object");
            assert_eq!(schema["required"], serde_json::json!(["queries"]));
            assert_eq!(schema["additionalProperties"], false);
            assert_eq!(queries["type"], "array");
            assert_eq!(item["type"], "object");
            assert_eq!(item["required"], serde_json::json!(["pattern"]));
            assert_eq!(item["additionalProperties"], false);
            assert_eq!(
                item["properties"]["mode"]["enum"],
                serde_json::json!(["literal", "regex"])
            );
            assert_eq!(
                item["properties"]["metadata"]["additionalProperties"]["type"],
                "string"
            );
        }

        fn assert_standard_schema(schema: &serde_json::Value) {
            assert_structure(schema);
            let queries = &schema["properties"]["queries"];
            let properties = &queries["items"]["properties"];
            assert_eq!(queries["minItems"], 2);
            assert_eq!(queries["maxItems"], 4);
            assert_eq!(properties["pattern"]["minLength"], 2);
            assert_eq!(properties["pattern"]["maxLength"], 64);
            assert_eq!(properties["limit"]["minimum"], 1.0);
            assert_eq!(properties["limit"]["maximum"], 100.0);
            assert_eq!(
                properties["metadata"]["additionalProperties"]["minLength"],
                1
            );
            assert_eq!(
                properties["metadata"]["additionalProperties"]["maxLength"],
                32
            );
        }

        fn assert_anthropic_schema(schema: &serde_json::Value) {
            assert_structure(schema);
            let queries = &schema["properties"]["queries"];
            let properties = &queries["items"]["properties"];
            let metadata_value = &properties["metadata"]["additionalProperties"];
            assert!(queries.get("minItems").is_none());
            assert!(queries.get("maxItems").is_none());
            assert!(queries["description"].as_str().unwrap().contains("minItems"));
            assert!(properties["pattern"].get("minLength").is_none());
            assert!(properties["pattern"].get("maxLength").is_none());
            assert!(properties["limit"].get("minimum").is_none());
            assert!(properties["limit"].get("maximum").is_none());
            assert!(metadata_value.get("minLength").is_none());
            assert!(metadata_value.get("maxLength").is_none());
        }

        let (openai_first, openai) = capture(
            latte_ai::models::ApiType::OpenAiCompletions,
            "openai",
            "",
            "/chat/completions",
        )
        .await;
        let (gemini_first, gemini) = capture(
            latte_ai::models::ApiType::OpenAiCompletions,
            "google",
            "/v1beta/openai",
            "/v1beta/openai/chat/completions",
        )
        .await;
        let (anthropic_first, anthropic) = capture(
            latte_ai::models::ApiType::AnthropicMessages,
            "anthropic",
            "",
            "/v1/messages",
        )
        .await;

        let openai_first_static = find_tool(&openai_first, "static_schema_matrix");
        let gemini_first_static = find_tool(&gemini_first, "static_schema_matrix");
        let anthropic_first_static = find_tool(&anthropic_first, "static_schema_matrix");

        let openai_static = find_tool(&openai, "static_schema_matrix");
        let openai_dynamic = find_tool(&openai, "dynamic_schema_matrix");
        let gemini_static = find_tool(&gemini, "static_schema_matrix");
        let gemini_dynamic = find_tool(&gemini, "dynamic_schema_matrix");
        let anthropic_static = find_tool(&anthropic, "static_schema_matrix");
        let anthropic_dynamic = find_tool(&anthropic, "dynamic_schema_matrix");

        let openai_first_static_schema = &openai_first_static["function"]["parameters"];
        let gemini_first_static_schema = &gemini_first_static["function"]["parameters"];
        let anthropic_first_static_schema = &anthropic_first_static["input_schema"];

        let openai_static_schema = &openai_static["function"]["parameters"];
        let openai_dynamic_schema = &openai_dynamic["function"]["parameters"];
        let gemini_static_schema = &gemini_static["function"]["parameters"];
        let gemini_dynamic_schema = &gemini_dynamic["function"]["parameters"];
        let anthropic_static_schema = &anthropic_static["input_schema"];
        let anthropic_dynamic_schema = &anthropic_dynamic["input_schema"];

        for schema in [
            openai_first_static_schema,
            openai_static_schema,
            openai_dynamic_schema,
            gemini_first_static_schema,
            gemini_static_schema,
            gemini_dynamic_schema,
        ] {
            assert_standard_schema(schema);
        }
        assert_eq!(
            openai_first_static_schema, openai_static_schema,
            "OpenAI static schema is stable across turns"
        );
        assert_eq!(
            gemini_first_static_schema, gemini_static_schema,
            "Gemini-compatible static schema is stable across turns"
        );
        assert_eq!(
            openai_static_schema, openai_dynamic_schema,
            "static and dynamic tools share the OpenAI schema path"
        );
        assert_eq!(
            gemini_static_schema, gemini_dynamic_schema,
            "static and dynamic tools share the Gemini-compatible schema path"
        );
        assert_eq!(
            openai_static_schema, gemini_static_schema,
            "Gemini compatibility uses OpenAI tool wire"
        );

        assert_anthropic_schema(anthropic_first_static_schema);
        assert_anthropic_schema(anthropic_static_schema);
        assert_anthropic_schema(anthropic_dynamic_schema);
        assert_eq!(
            anthropic_first_static_schema, anthropic_static_schema,
            "Anthropic static schema is stable across turns"
        );
        assert_eq!(
            anthropic_static_schema, anthropic_dynamic_schema,
            "static and dynamic tools share the Anthropic schema path"
        );
        for tool in [
            anthropic_first_static,
            anthropic_static,
            anthropic_dynamic,
        ] {
            assert!(tool.get("parameters").is_none());
        }
    }

    /// build_tool_schemas：配置层扁平名（bash/read/search）与 registry
    /// 注册名一致（latte-rs-agent-tools 已扁平化命名空间，不再有点号前缀）。
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
    }
}
