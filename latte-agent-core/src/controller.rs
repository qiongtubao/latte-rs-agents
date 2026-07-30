//! ChatController: event-driven chat session driver.
//!
//! Replaces the CLI's stdin-based REPL (`run_hil_chat` + `run_hil_repl` +
//! `ChatCmd::run()`) with an async task that reads from an `mpsc` channel
//! and emits events via a `broadcast` channel.
//!
//! Two modes:
//! - **Single-role** (`roles.len() == 1`): one `AgentRunner`, loop of
//!   `submit_input → run_turn → RoleTurn`. Supports slash commands
//!   (`/role`, `/model`, `/clear`, `/save`, etc.).
//! - **Multi-role** (`roles.len() > 1`): full round-robin HIL mode with
//!   `RoundScheduler`, inject queue, plan slice, supervisor pause.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use latte_ai::models::{Message, Role as MsgRole};
use latte_ai::params::GenerateParams;
use latte_rs_agent_tools::types::{PropertyType, ToolInputProperty};
use tokio::sync::{broadcast, mpsc, Mutex};

use crate::advisor_monitor::AdvisorMonitorConfig;
use crate::agent::{Agent, AgentRunner};
use crate::config::AgentConfig;
use crate::error::AgentError;
use crate::model_resolver::{ModelResolver, ModelTier};
use crate::scheduler::{plan_md_slice_for, RoundScheduler};
use crate::session::{SessionManager, SessionRecord, SessionState};
use crate::subsession::SubsessionStore;
use crate::supervisor::{Supervisor, SupervisorConfig};
use crate::trace::{FanoutSink, ModelErrorKind};
use crate::workspace::WorkspaceManager;
use crate::AgentResult;

/// 把 `AgentError` 投影成 `ModelErrorKind`。`Controller` 端
/// 收到 `turn failed: {AgentError}` 时，把 e 转成 kind 写进
/// `ChatEvent::Error.kind`，让消费方拿到结构化分类。
///
/// 设计原则：**保真**优先于**聚合**——不同 AgentError 投影到
/// 不同 ModelErrorKind，缺口的走 `Other`。后续可以扩展 matches。
fn agent_error_to_kind(e: &AgentError) -> ModelErrorKind {
    match e {
        // Config 系：user 可修
        AgentError::Config(s) => ModelErrorKind::Config { message: s.clone() },
        AgentError::Template(role, _) => ModelErrorKind::Config { message: format!("template render: {role}") },
        AgentError::TemplateNotFound(p) => ModelErrorKind::Config { message: format!("template not found: {p}") },
        AgentError::RoleNotFound(r) => ModelErrorKind::Config { message: format!("role not found: {r}") },
        AgentError::ModelNotFound(m) => ModelErrorKind::Config { message: format!("model not found: {m}") },
        AgentError::ModelResolutionFailed { role, tier, reason } => {
            ModelErrorKind::Config { message: format!("resolution failed for {role}/{tier}: {reason}") }
        }
        AgentError::InvalidParam(s) => ModelErrorKind::Config { message: s.clone() },
        // AI client：转发 helper 的分类
        AgentError::AiClient(ai_err) => ModelErrorKind::from(ai_err),
        // 工具 / 上层
        AgentError::Tool(s) => ModelErrorKind::Other { message: format!("tool: {s}") },
        AgentError::TokenBudgetExceeded { .. } => ModelErrorKind::Config { message: "token budget exceeded".into() },
        AgentError::MaxToolRoundsExceeded(_) => ModelErrorKind::Other { message: "max tool rounds exceeded".into() },
        AgentError::ToolLoopDetected { tool, .. } => ModelErrorKind::Other { message: format!("tool loop: {tool}") },
        AgentError::Orchestration(s) => ModelErrorKind::Other { message: format!("orchestration: {s}") },
        AgentError::ModelsUnavailable { tried, .. } => {
            // 告诉前端这是"全部模型都不可用"，已不是单点错误
            ModelErrorKind::Other { message: format!("all models unavailable (tried {})", tried.len()) }
        }
        AgentError::HookAborted { hook, .. } => ModelErrorKind::Other { message: format!("hook aborted: {hook}") },
        AgentError::Io(io) => ModelErrorKind::Other { message: format!("io: {io}") },
        // Advisor 终止（gate D5/D6 重试耗尽 或 LLM 复审返回 Terminate）。
        AgentError::AdvisorTerminated { reason, detector } => {
            ModelErrorKind::Other { message: format!("advisor terminated ({}): {}", detector, reason) }
        }
    }
}

/// 把任意 `AgentError` + 上下文 prefix 打包成 `ChatEvent::Error`，
/// 让 controller 端的 5 处 Error 构造点（turn failed / switch role /
/// switch model 等）走同一投影，UI/CLI 拿到一致的 `kind`。
/// `sub_id` 在错误来自某个 delegate subsession 时填入，让 UI
/// 右键「查看日志」能跳到具体 subagent 过程。
fn error_event(e: &AgentError, prefix: &str, sub_id: Option<String>) -> ChatEvent {
    ChatEvent::Error {
        kind: Some(agent_error_to_kind(e)),
        message: format!("{prefix}: {e}"),
        sub_id,
    }
}
fn truncate_event_text(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    format!("{}...[+{}B]", &text[..end], text.len() - end)
}

struct ChatEventTraceSink {
    event_tx: broadcast::Sender<ChatEvent>,
}

impl crate::trace::TraceSink for ChatEventTraceSink {
    fn emit(&self, event: crate::trace::TraceEvent) {
        match event {
            crate::trace::TraceEvent::ParseToolCalls { meta, parsed, .. } => {
                for call in parsed {
                    let _ = self.event_tx.send(ChatEvent::ToolUse {
                        role_id: meta.role.clone(),
                        tool_name: call.name,
                        args: truncate_event_text(&call.args, 1_200),
                    });
                }
            }
            crate::trace::TraceEvent::ToolExec {
                meta,
                name,
                status,
                ..
            } => match status {
                crate::trace::ToolStatus::Ok(result) => {
                    let _ = self.event_tx.send(ChatEvent::ToolResult {
                        role_id: meta.role,
                        tool_name: name,
                        result: truncate_event_text(&result, 1_500),
                    });
                }
                crate::trace::ToolStatus::Err(error) => {
                    let _ = self.event_tx.send(ChatEvent::ToolError {
                        role_id: meta.role,
                        tool_name: name,
                        error: truncate_event_text(&error, 1_500),
                    });
                }
            },
            _ => {}
        }
    }
}

// ─── Events ──────────────────────────────────────────────────────

/// Event emitted by the ChatController driver loop.
///
/// ## JSON wire format
///
/// Default serde representation (externally-tagged) is used:
/// `{"Status":{"message":"..."}}`. This is the canonical contract
/// consumed by `latte-code-editor` and the `chat --output json` JSONL
/// stream; the variant name is the outer object key.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum ChatEvent {
    /// Error message.
    ///
    /// `kind` 是结构化的错误分类（ModelErrorKind）—— 让 CLI / UI /
    /// 桌面端能基于**分类**做事（着色、自动建议、统计）。`message`
    /// 保留为人类可读原文。`kind` 是 `Option`，让旧 consumer 反
    /// 序列化旧 JSON（缺 kind 字段）时不报错；新构造点应填。
    ///
    /// `sub_id` (optional) 链接到产生此错误的 delegate subsession：
    /// UI 右键「查看日志」即可定位到该 subagent 的完整工具调用/
    /// bash 日志，而不是只看主 turn。主 turn 自身的错误保持 `None`。
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<ModelErrorKind>,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sub_id: Option<String>,
    },
    /// `sub_id` (optional) links this turn to a specific delegate subsession.
    RoleTurn {
        role_id: String,
        content: String,
        is_complete: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sub_id: Option<String>,
    },
    /// Informational / status message (replaces `eprintln!` / `println!`).
    Status { message: String },
    /// 用户发出的一条消息（send 路径；`/` 开头的斜杠命令不产生此事件）。
    /// 回放（UI 的 event_log / ui-sessions 落盘）靠它恢复用户气泡——
    /// 此前全部变体都是 agent 侧产出，回放里看不到用户自己发的内容。
    UserMessage { text: String },
    /// Prompt indicator (replaces `"👔 manager · deepseek-v4-flash › "`).
    Prompt {
        icon: String,
        role_id: String,
        model_id: String,
    },
    /// Session paused (by user or supervisor).
    Paused { reason: String },
    /// Session resumed.
    Resumed,
    /// Round started (multi-role mode).
    RoundStarted { round: u32 },
    /// Round ended (multi-role mode).
    RoundEnded { round: u32 },
    /// A role has started working.
    RoleStarted { role_id: String, detail: String },
    /// A role has finished working.
    RoleFinished { role_id: String, detail: String },
    /// Session finished (aborted or quit).
    Done,
    RoleList {
        roles: Vec<RoleInfo>,
    },
    /// Context cleared (response to `/clear`).
    ContextCleared,
    /// Startup info: session identity + initial state.
    SessionInfo {
        task_id: String,
        state: String,
        turn: u32,
        roles: Vec<RoleInfo>,
    },
    /// Tool usage (for display in the editor UI).
    ToolUse {
        role_id: String,
        tool_name: String,
        args: String,
    },
    /// Tool result (for display in the editor UI).
    ToolResult {
        role_id: String,
        tool_name: String,
        result: String,
    },
    /// Manager decided to delegate a subtask to a specialist role.
    /// Emitted before the specialist runner starts so the UI can
    /// show "manager → programmer" without waiting for the
    /// (potentially multi-minute) specialist turn to complete.
    /// `sub_id` is a unique id for the subsession; the UI can
    /// `GET /api/sessions/{id}/subsessions/{sub_id}` to fetch the
    /// specialist's full internal transcript (tool calls, intermediate
    /// replies) — main chat only sees the final summary.
    DelegateStarted {
        from_role: String,
        to_role: String,
        task: String,
        sub_id: String,
    },
    /// Specialist returned (or failed/timeout). `status` is one of
    /// `"ok" | "failed" | "timeout" | "cancelled"`. `summary` is the
    /// specialist's last assistant turn on success, or the error
    /// message on failure — suitable for showing in the activity
    /// stream and for the manager to consume as the tool result.
    /// `sub_id` matches the `DelegateStarted.sub_id` so the UI can
    /// look up the full subsession transcript via `/api/sessions/...`.
    DelegateFinished {
        from_role: String,
        to_role: String,
        status: String,
        summary: String,
        sub_id: String,
    },
    ToolError {
        role_id: String,
        tool_name: String,
        error: String,
    },
    /// Manager triggered a multi-role workflow (设计/plan/TDD/文档/图谱…).
    /// `wf_id` links all events of this run; the UI groups them.
    WorkflowStarted {
        name: String,
        topic: String,
        wf_id: String,
    },
    /// A workflow step is about to run (`index`/`total` are 1-based).
    WorkflowStep {
        wf_id: String,
        step_id: String,
        description: String,
        index: usize,
        total: usize,
    },
    /// One speaker's completed turn inside a workflow step.
    WorkflowTurn {
        wf_id: String,
        step_id: String,
        role_id: String,
        content: String,
        round: usize,
    },
    /// Workflow run finished. `status`: "ok" | "failed" | "cancelled".
    /// `summary` is the last turn's output (or the error message).
    WorkflowFinished {
        name: String,
        wf_id: String,
        status: String,
        summary: String,
    },
    /// 角色调用 generate_image 生成的图片。path 是 UI server 的
    /// `/api/images/<file>` URL（web UI 直接渲染 <img>）。
    ImageGenerated { role_id: String, path: String, prompt: String },
    /// 角色调用 `plan` 工具提交的任务候选清单。manager 在
    /// `implementation_plan` workflow 跑完（或手持一份具体任务清单）
    /// 后调 `plan`，把结构化任务交给用户在弹窗里勾选导入任务看板。
    /// `plan_id` 唯一标识本次提案，供 UI 去重与右键补救重开弹窗。
    /// `tasks` 与 `POST /api/tasks/import` 的 `ImportTask`（tasks.rs）
    /// 同构——前端选中后直接透传导入接口，无需文本解析。
    PlanProposed {
        role_id: String,
        plan_id: String,
        tasks: Vec<PlanTask>,
    },
    /// Turn soft-timeout warning: the current turn has been running
    /// longer than the configured soft timeout but is still alive.
    /// The UI uses this to surface a "继续等待 / 终止当前任务" prompt
    /// instead of silently killing the turn. The hard kill fires at
    /// `hard_timeout_secs` if the user does not act.
    ///
    /// `role_id` identifies which role's turn is over-budget; for the
    /// main single-role loop this is the active role, for multi-role
    /// it is the role currently speaking. `sub_id` is set when the
    /// over-budget turn is a delegate subsession, so the UI's
    /// "查看日志" action can jump straight to the subagent transcript.
    TimeoutWarning {
        role_id: String,
        elapsed_secs: u64,
        soft_timeout_secs: u64,
        hard_timeout_secs: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sub_id: Option<String>,
    },

    /// Advisor 终止了一次 turn（**不**广播 `RoleTurn`，坏答案不
    /// 落盘）。两类触发源：
    /// 1. Pre-persistence gate（D5/D6）重试 `max_retries` 次仍命中
    ///    → runner raise `AgentError::AdvisorTerminated`，
    ///    `detector: Some("D6")` 等具体标签
    /// 2. LLM 复审返回 `Verdict::Terminate`（语义层判定问题严重
    ///    到需要停下让用户接管），`detector: Some("LLM")`
    ///
    /// UI 必须把它显示成"对话已暂停，请查看 advisor 反馈后继续"
    /// 的明确状态；用户输入新消息后 controller 正常处理。runner
    /// 不会自杀，driver 只是回到 `input_rx.recv()` 等用户输入。
    /// `sub_id` 在 sub-session 终止时填上。
    AdvisorTerminated {
        role_id: String,
        reason: String,
        /// 触发的 detector 标签（"D5" / "D6" / "LLM" / "D3" / "D4"）；
        /// `None` 表示来源未明确（例如 LLM 复审失败但降级返回时）
        detector: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sub_id: Option<String>,
    },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RoleInfo {
    pub id: String,
    pub name: String,
    pub icon: String,
}
/// `plan` 工具提交的单个任务候选。字段与后端 `ImportTask`
/// （`latte-agent-ui-server/src/tasks.rs`）及前端 `ImportTask`
/// （`api.ts`）同构——前端勾选后可直接透传 `POST /api/tasks/import`。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PlanTask {
    /// 任务标题，一句话，必填非空。
    pub title: String,
    /// 做什么 + 验收标准。空则不序列化（前端视作缺省）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// 优先级 1-4（1 最高）。空则不序列化。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
    /// 标签数组。空则不序列化。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// 执行该任务的 workflow 名（tdd_development/bug_triage/update_docs）。
    /// 轻量任务可空。空则不序列化。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// 子任务，同构，最多一层。空则不序列化。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subtasks: Vec<PlanTask>,
}

// ─── Controller input ────────────────────────────────────────────

enum ControllerInput {
    Input(String),
    Pause,
    Resume,
    SwitchRole(String),
    SwitchModel(ModelTier),
    Abort,
    /// Cancel only the in-flight turn (the one currently waiting on
    /// the LLM). Distinct from `Abort`, which tears down the entire
    /// session. The UI sends this when the user picks "终止当前任务"
    /// from a `TimeoutWarning` prompt. The driver flips a per-turn
    /// flag, the run_turn future is dropped, and the session loop
    /// moves on to the next user input.
    CancelTurn,
    /// Advisor monitor correction hint. The driver pushes it into the
    /// current runners' shared in-memory hint queue (see
    /// `AgentRunner::with_advisor_hints`); it is NOT a new user
    /// question and never triggers a turn on its own.
    ///
    /// Note: `ChatController::advisor_hint` pushes into the shared
    /// queue *directly* (synchronous, mid-turn delivery); this
    /// variant exists so embedders that only hold the input channel
    /// can still route hints through the driver.
    #[allow(dead_code)] // constructed by embedders holding the raw input channel
    AdvisorHint(String),
}

// ─── Configuration ───────────────────────────────────────────────

/// Configuration for creating a ChatController.
pub struct ControllerConfig {
    /// `Some(id)` → multi-role HIL mode on existing worktree.
    /// `None` → single-role mode (no SessionManager).
    pub task_id: Option<String>,
    /// Role IDs. `len() == 1` → single-role, `> 1` → multi-role HIL.
    pub roles: Vec<String>,
    /// Initial prompt (used only when creating a fresh HIL session).
    pub initial_prompt: Option<String>,
    /// Maximum rounds (multi-role mode). Default 10.
    pub max_rounds: u32,
    /// Session token budget for Supervisor. 0 = disabled.
    pub session_token_budget: u32,
    /// Merged agent config (models + roles).
    pub agent_config: Arc<AgentConfig>,
    /// Model resolver (built from merged config).
    pub model_resolver: Arc<ModelResolver>,
    /// Default generation params.
    pub default_params: GenerateParams,
    /// Pinned model id (overrides tier-based resolution).
    pub primary_model_id: Option<String>,
    /// Initial model tier override.
    pub initial_tier: Option<ModelTier>,
    /// Pre-existing conversation history to seed the runner with
    /// *before* the driver loop processes its first input. Used by
    /// `controller_runtime::start` to resume a previously-paused
    /// chat session from the SessionStore: the editor reads the
    /// stored `ChatEvent` mirror, converts to `latte_ai::models::Message`,
    /// and threads them through here so the next `run_turn` sees the
    /// full prior context.
    ///
    /// Empty in the fresh-session path. Order matches the original
    /// chat (oldest first). The driver does not re-emit these as
    /// `RoleTurn` events — the editor's `useChatStore` already
    /// has them locally.
    pub initial_history: Vec<latte_ai::models::Message>,
    /// Current working directory (for worktree resolution).
    pub cwd: PathBuf,
    /// UI session id（= UI 侧 SessionHandle.session_id），用于
    /// subsession_store 的落盘 key。**不要**用 `cwd` 凑数 —— cwd 是
    /// 绝对路径，作为文件目录名是非法且不稳定的。
    /// 不传：默认 empty（兼容老 caller；subsession 仍能跑但 cwd 路径
    /// 会被 disk sink 拒绝并降级到内存）。
    pub session_id: String,
    /// Shared store for per-task subsessions. Each delegate tool call
    /// allocates one entry under `(cwd_session, sub_id)` and stores
    /// the specialist's full TraceEvent stream so the UI can fetch it
    /// for "show contents". Owned by the caller (e.g. UiCmd::run);
    /// a single Arc is shared across all controllers regardless of
    /// tab/session, so GC works uniformly per process.
    pub subsession_store: Arc<SubsessionStore>,
    /// Advisor monitor (旁路监察者) settings. Default: enabled with
    /// `OnAnomaly` LLM review. The session creator (e.g. ui-server's
    /// `create_session_handle`) spawns the monitor when enabled.
    pub advisor_monitor: AdvisorMonitorConfig,
}

// ─── Controller ──────────────────────────────────────────────────

/// Event-driven chat session controller.
pub struct ChatController {
    input_tx: tokio::sync::Mutex<Option<mpsc::UnboundedSender<ControllerInput>>>,
    event_tx: broadcast::Sender<ChatEvent>,
    cancel_flag: Arc<AtomicBool>,
    pause_requested: Arc<AtomicBool>,
    /// Per-turn cancellation flag, distinct from `cancel_flag`
    /// (which aborts the whole session). The driver arms it before
    /// awaiting `run_turn` and clears it after the turn resolves; the
    /// UI sends `CancelTurn` to flip it. Lets the user abort just the
    /// currently-running turn (e.g. from a `TimeoutWarning` prompt)
    /// without losing the rest of the session.
    turn_cancel_flag: Arc<AtomicBool>,
    /// Shared advisor hint queue. The producer side is
    /// `advisor_hint()` (called by the AdvisorMonitor); the consumer
    /// side is every runner the driver builds
    /// (`AgentRunner::with_advisor_hints`), drained at turn start and
    /// at every tool-round boundary. Direct push (not the mpsc input
    /// channel) is what makes *mid-turn* delivery possible: in
    /// single-role mode the driver awaits `run_turn` and would not
    /// see an mpsc message until the turn finished.
    advisor_hints: Arc<parking_lot::Mutex<std::collections::VecDeque<String>>>,
    /// Most recent non-command user input, recorded by
    /// `submit_input`. The AdvisorMonitor reads it to give the LLM
    /// review the user's current question (user input is not part of
    /// the `ChatEvent` broadcast stream).
    last_user_input: Arc<parking_lot::Mutex<String>>,
}

impl ChatController {
    /// Create a new controller. Does NOT start the driver loop — call
    /// `spawn()` to begin.
    pub fn new(event_capacity: usize) -> Self {
        let (event_tx, _) = broadcast::channel(event_capacity);
        Self {
            input_tx: tokio::sync::Mutex::new(None),
            event_tx,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            pause_requested: Arc::new(AtomicBool::new(false)),
            turn_cancel_flag: Arc::new(AtomicBool::new(false)),
            advisor_hints: Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new())),
            last_user_input: Arc::new(parking_lot::Mutex::new(String::new())),
        }
    }

    /// Start the driver loop. Returns a receiver that gets all events.
    pub async fn spawn(
        &self,
        config: ControllerConfig,
    ) -> broadcast::Receiver<ChatEvent> {
        let (new_tx, input_rx) = mpsc::unbounded_channel();
        let _old = std::mem::replace(&mut *self.input_tx.lock().await, Some(new_tx));
        drop(_old);

        let event_tx = self.event_tx.clone();
        let cancel_flag = self.cancel_flag.clone();
        let pause_flag = self.pause_requested.clone();
        let turn_cancel_flag = self.turn_cancel_flag.clone();
        let advisor_hints = self.advisor_hints.clone();

        tokio::spawn(async move {
            run_driver(
                config,
                input_rx,
                &event_tx,
                cancel_flag,
                pause_flag,
                turn_cancel_flag,
                advisor_hints,
            )
            .await;
        });

        self.event_tx.subscribe()
    }

    /// Submit a line of user input.
    pub async fn submit_input(&self, text: &str) {
        // Record the latest genuine question for the advisor monitor's
        // LLM review input (slash commands are not questions).
        if !text.trim_start().starts_with('/') {
            *self.last_user_input.lock() = text.to_string();
        }
        if let Some(tx) = self.input_tx.lock().await.as_ref() {
            let _ = tx.send(ControllerInput::Input(text.to_string()));
        }
    }

    /// Push an advisor correction hint into the shared runner hint
    /// queue. Synchronous, lock-only — safe to call while a turn is
    /// running; the current runner drains it at the next tool-round
    /// boundary (or turn start when idle). Never triggers a turn.
    pub fn advisor_hint(&self, text: &str) {
        const MAX_PENDING_HINTS: usize = 16;
        let mut q = self.advisor_hints.lock();
        // Bound the backlog so a pathological monitor can't grow the
        // queue without limit; oldest hints are the least relevant.
        while q.len() >= MAX_PENDING_HINTS {
            q.pop_front();
        }
        q.push_back(text.to_string());
    }

    /// The shared advisor hint queue (driver wiring + tests).
    pub fn advisor_hint_queue(
        &self,
    ) -> Arc<parking_lot::Mutex<std::collections::VecDeque<String>>> {
        self.advisor_hints.clone()
    }

    /// The most recent user question recorded by `submit_input`.
    pub fn last_user_input(&self) -> String {
        self.last_user_input.lock().clone()
    }

    /// Clone of the event broadcast sender. Used by the AdvisorMonitor
    /// to publish its own `RoleTurn { role_id: "advisor" }` bubbles
    /// (channel B) into the same stream the UI consumes.
    pub fn event_sender(&self) -> broadcast::Sender<ChatEvent> {
        self.event_tx.clone()
    }

    /// Request pause after the current round completes.
    pub async fn pause(&self) {
        if let Some(tx) = self.input_tx.lock().await.as_ref() {
            let _ = tx.send(ControllerInput::Pause);
        }
    }

    /// Resume from paused state.
    pub async fn resume(&self) {
        self.pause_requested.store(false, Ordering::SeqCst);
        if let Some(tx) = self.input_tx.lock().await.as_ref() {
            let _ = tx.send(ControllerInput::Resume);
        }
    }

    /// Switch to a different role (single-role mode only).
    pub async fn switch_role(&self, role_id: &str) {
        if let Some(tx) = self.input_tx.lock().await.as_ref() {
            let _ = tx.send(ControllerInput::SwitchRole(role_id.to_string()));
        }
    }

    /// Switch model tier (single-role mode only).
    pub async fn switch_model(&self, tier: ModelTier) {
        if let Some(tx) = self.input_tx.lock().await.as_ref() {
            let _ = tx.send(ControllerInput::SwitchModel(tier));
        }
    }

    /// Abort the session immediately.
    pub async fn abort(&self) {
        self.cancel_flag.store(true, Ordering::SeqCst);
        if let Some(tx) = self.input_tx.lock().await.as_ref() {
            let _ = tx.send(ControllerInput::Abort);
        }
    }
    /// Cancel only the in-flight turn (the one currently awaiting
    /// the LLM). Distinct from `abort`, which tears down the whole
    /// session. The driver flips `turn_cancel_flag`, aborts the
    /// `run_turn` JoinHandle, and the session loop moves on to
    /// the next user input. UI sends this when the user picks
    /// "终止当前任务" from a `TimeoutWarning` prompt.
    pub async fn cancel_turn(&self) {
        self.turn_cancel_flag.store(true, Ordering::SeqCst);
        // Also send CancelTurn input so that loops blocked on
        // input_rx.recv() (multi-role, single-role while idle)
        // see the signal and can break out.
        if let Some(tx) = self.input_tx.lock().await.as_ref() {
            let _ = tx.send(ControllerInput::CancelTurn);
        }
    }

    /// Get a subscriber that receives all future events.
    pub fn subscribe(&self) -> broadcast::Receiver<ChatEvent> {
        self.event_tx.subscribe()
    }
}

// ─── Wave config types ────────────────────────────────────────────

/// Workflow step configuration, used for wave dependency analysis.
/// Mirrors the gsd-core workflow step concept with optional input/output
/// contract tracking via `FileContractConfig`.
#[derive(Debug, Clone)]
pub struct WorkflowStepConfig {
    pub id: String,
    pub speakers: Vec<String>,
    pub prompt: String,
    pub hooks: Vec<super::config::StepHookConfig>,
    pub contract: Option<FileContractConfig>,
}

impl ContractAccess for WorkflowStepConfig {
    fn contract_input(&self) -> Option<&str> {
        self.contract.as_ref().and_then(|c| c.input.as_deref())
    }
    fn contract_output(&self) -> Option<&str> {
        self.contract.as_ref().and_then(|c| c.output.as_deref())
    }
}

/// File-based contract between workflow steps. `input` declares a file
/// this step reads (making it dependent on the step that produces it);
/// `output` declares a file this step produces (making later steps
/// dependent on it).
#[derive(Debug, Clone)]
pub struct FileContractConfig {
    pub input: Option<String>,
    pub output: Option<String>,
    pub placeholder: Option<String>,
    pub extract_prompt: Option<String>,
    pub required: bool,
}

/// Trait for types that can be used in wave dependency analysis.
/// Implemented by any step type with optional input/output contracts.
pub trait ContractAccess {
    fn contract_input(&self) -> Option<&str>;
    fn contract_output(&self) -> Option<&str>;
}

/// A group of workflow steps that execute concurrently within a single
/// wave. Steps in the same wave have no inter-dependency on contract
/// outputs.
#[derive(Debug, Clone)]
pub struct Wave {
    pub index: usize,
    pub steps: Vec<usize>,
}

/// Result of wave analysis: an ordered sequence of waves.
#[derive(Debug, Clone, Default)]
pub struct WavePlan {
    pub waves: Vec<Wave>,
}

/// Compute the wave execution plan for a list of workflow steps.
/// Steps that produce no output (contract_output is None) are treated
/// as terminal steps — they can depend on prior outputs but no other
/// step depends on them.
pub fn compute_waves<S: ContractAccess>(steps: &[S]) -> WavePlan {
    if steps.is_empty() {
        return WavePlan::default();
    }

    // Build a map: output file → step index
    let output_to_step: std::collections::HashMap<&str, usize> = steps
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.contract_output().map(|path| (path, i)))
        .collect();

    // Compute in-degree (number of dependencies) for each step
    let mut in_degree: Vec<usize> = vec![0; steps.len()];
    let mut depends_on: Vec<Vec<usize>> = vec![vec![]; steps.len()];

    for (i, step) in steps.iter().enumerate() {
        if let Some(input) = step.contract_input() {
            if let Some(&producer) = output_to_step.get(input) {
                in_degree[i] += 1;
                depends_on[i].push(producer);
            }
        }
    }

    // Kahn's algorithm: topological sort into waves
    let mut plan = WavePlan::default();
    let mut remaining: std::collections::VecDeque<usize> = steps
        .iter()
        .enumerate()
        .filter(|(i, _)| in_degree[*i] == 0)
        .map(|(i, _)| i)
        .collect();

    let mut visited = 0;

    while !remaining.is_empty() {
        let wave_steps: Vec<usize> = remaining.drain(..).collect();
        visited += wave_steps.len();
        plan.waves.push(Wave { index: plan.waves.len(), steps: wave_steps });

        for &wave_idx in plan.waves.last().unwrap().steps.iter() {
            for (next_idx, deps) in depends_on.iter().enumerate() {
                if deps.contains(&wave_idx) {
                    in_degree[next_idx] = in_degree[next_idx].saturating_sub(1);
                    if in_degree[next_idx] == 0 && !remaining.contains(&next_idx) {
                        remaining.push_back(next_idx);
                    }
                }
            }
        }
    }

    if visited < steps.len() {
        let orphans: Vec<usize> = (0..steps.len())
            .filter(|i| in_degree[*i] > 0)
            .collect();
        plan.waves.push(Wave { index: plan.waves.len(), steps: orphans });
    }

    plan
}





/// Run a turn with cancellation support. Polls `run_turn` at 500ms ticks
/// and checks `cancel_flag` (session abort) + `turn_cancel_flag` (current
/// turn cancel). On cancellation the in-flight LLM call is dropped via
/// the future's Drop impl. Returns `Ok(response)` or `Err(AgentError)`.
/// No timeout — the user must cancel explicitly.
async fn run_turn_cancellable(
    runner: &mut AgentRunner,
    msgs: &[Message],
    cancel_flag: &AtomicBool,
    turn_cancel_flag: &AtomicBool,
) -> Result<String, AgentError> {
    turn_cancel_flag.store(false, Ordering::SeqCst);
    let mut fut = Box::pin(runner.run_turn(msgs, None));
    let result: Result<String, &'static str> = loop {
        tokio::select! {
            biased;
            _ = tokio::time::sleep(Duration::from_millis(500)) => {
                if cancel_flag.load(Ordering::SeqCst) {
                    break Err("session_cancelled");
                }
                if turn_cancel_flag.load(Ordering::SeqCst) {
                    break Err("turn_cancelled");
                }
            }
            r = fut.as_mut() => {
                break r.map(|s| s).map_err(|_| "turn_failed");
            }
        }
    };
    drop(fut);
    match result {
        Ok(resp) => Ok(resp),
        Err("session_cancelled") => Err(AgentError::Orchestration("session cancelled by user".into())),
        Err("turn_cancelled") => Err(AgentError::Orchestration("turn cancelled by user".into())),
        Err("turn_failed") => Err(AgentError::Orchestration("turn failed".into())),
        Err(e) => Err(AgentError::Orchestration(format!("turn error: {e}"))),
    }
}

async fn run_driver(
    config: ControllerConfig,
    mut input_rx: mpsc::UnboundedReceiver<ControllerInput>,
    event_tx: &broadcast::Sender<ChatEvent>,
    cancel_flag: Arc<AtomicBool>,
    pause_flag: Arc<AtomicBool>,
    turn_cancel_flag: Arc<AtomicBool>,
    advisor_hints: Arc<parking_lot::Mutex<std::collections::VecDeque<String>>>,
) {
    let is_multi = config.roles.len() > 1 || config.task_id.is_some();

    if is_multi {
        run_multi_role_loop(
            config,
            &mut input_rx,
            event_tx,
            cancel_flag,
            &*pause_flag,
            turn_cancel_flag,
            advisor_hints,
        )
        .await;
    } else {
        run_single_role_loop(
            config,
            &mut input_rx,
            event_tx,
            cancel_flag,
            turn_cancel_flag,
            advisor_hints,
        )
        .await;
    }

    let _ = event_tx.send(ChatEvent::Done);
}

// ─── Multi-role HIL mode ─────────────────────────────────────────

async fn run_multi_role_loop(
    config: ControllerConfig,
    input_rx: &mut mpsc::UnboundedReceiver<ControllerInput>,
    event_tx: &broadcast::Sender<ChatEvent>,
    cancel_flag: Arc<AtomicBool>,
    pause_flag: &AtomicBool,
    turn_cancel_flag: Arc<AtomicBool>,
    advisor_hints: Arc<parking_lot::Mutex<std::collections::VecDeque<String>>>,
) {
    // Resolve worktree root
    let repo_root = match WorkspaceManager::resolve_repo_root(&config.cwd) {
        Ok(root) => root,
        Err(e) => {
            let _ = event_tx.send(ChatEvent::Error {
                            kind: None,
                            message: format!("无法解析仓库根目录: {e}"),
                            sub_id: None,
                        });
            return;
        }
    };
    let task_id = match &config.task_id {
        Some(id) => id.clone(),
        None => {
            let _ = event_tx.send(ChatEvent::Error {
                            kind: None,
                            message: "multi-role 模式需要 task_id".into(),
                            sub_id: None,
                        });
            return;
        }
    };
    let worktree_root = repo_root.join(".latte").join("worktrees").join(&task_id);

    // Load or create SessionManager
    let mut session_mgr =
        SessionManager::new(&task_id, worktree_root.clone(), config.roles.clone());
    if session_mgr.session_path().exists() {
        let raw = match std::fs::read_to_string(session_mgr.session_path()) {
            Ok(r) => r,
            Err(e) => {
                let _ = event_tx.send(ChatEvent::Error {
                                kind: None,
                                message: format!("读取 session 文件失败: {e}"),
                                sub_id: None,
                            });
                return;
            }
        };
        let record: SessionRecord = match serde_json::from_str(&raw) {
            Ok(r) => r,
            Err(e) => {
                let _ = event_tx.send(ChatEvent::Error {
                                kind: None,
                                message: format!("解析 session JSON 失败: {e}"),
                                sub_id: None,
                            });
                return;
            }
        };
        session_mgr = SessionManager::from_record(record, worktree_root.clone());
        let _ = event_tx.send(ChatEvent::Status {
            message: format!(
                "[session: {}, state: {:?}, turn: {}]",
                session_mgr.record().task_id,
                session_mgr.state(),
                session_mgr.record().current_turn
            ),
        });
        if session_mgr.state() == SessionState::Paused {
            if let Err(e) = session_mgr.resume() {
                let _ = event_tx.send(ChatEvent::Error {
                                kind: None,
                                message: format!("resume 失败: {e}"),
                                sub_id: None,
                            });
                return;
            }
            let _ = event_tx.send(ChatEvent::Status {
                message: format!("[RESUMED at {}]", session_mgr.record().updated_at),
            });
        }
    } else {
        let prompt = match &config.initial_prompt {
            Some(p) => p.clone(),
            None => {
                let _ = event_tx.send(ChatEvent::Error {
                                kind: None,
                                message: format!("task '{task_id}' 无 session，需要 initial_prompt"),
                                sub_id: None,
                            });
                return;
            }
        };
        let bb = crate::workspace::Blackboard::new(worktree_root.join("plan.md"));
        if let Err(e) = bb.write(&format!("# {task_id}\n\n{prompt}")) {
            let _ = event_tx.send(ChatEvent::Error {
                            kind: None,
                            message: format!("写入 plan.md 失败: {e}"),
                            sub_id: None,
                        });
            return;
        }
        if let Err(e) = session_mgr.persist() {
            let _ = event_tx.send(ChatEvent::Error {
                            kind: None,
                            message: format!("持久化 session 失败: {e}"),
                            sub_id: None,
                        });
            return;
        }
        let _ = event_tx.send(ChatEvent::Status {
            message: format!("[session: {task_id}, state: Created, turn: 0]"),
        });
        let _ = event_tx.send(ChatEvent::Status {
            message: format!("[roles: {}]", config.roles.join(", ")),
        });
    }

    // Build shared session Arc
    let session_arc: Arc<Mutex<SessionManager>> = Arc::new(Mutex::new(session_mgr));

    // Build RoundScheduler
    let scheduler_arc = session_arc.clone();
    let mut scheduler = match tokio::task::spawn_blocking(move || {
        RoundScheduler::new(scheduler_arc)
    })
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            let _ = event_tx.send(ChatEvent::Error {
                            kind: None,
                            message: format!("scheduler init error: {e}"),
                            sub_id: None,
                        });
            return;
        }
        Err(e) => {
            let _ = event_tx.send(ChatEvent::Error {
                            kind: None,
                            message: format!("scheduler join error: {e}"),
                            sub_id: None,
                        });
            return;
        }
    };
    scheduler.max_rounds = config.max_rounds;
    scheduler.supervisor = Supervisor::new(SupervisorConfig {
        session_token_budget: config.session_token_budget,
        dead_loop_window: 3,
    });

    let order = scheduler.order.clone();

    // Emit session info
    {
        let mgr = session_arc.lock().await;
        let roles_info: Vec<RoleInfo> = mgr
            .record()
            .roles
            .iter()
            .map(|r| RoleInfo {
                id: r.role_id.clone(),
                name: r.role_id.clone(),
                icon: role_icon(&r.role_id),
            })
            .collect();
        let _ = event_tx.send(ChatEvent::SessionInfo {
            task_id: task_id.clone(),
            state: format!("{:?}", mgr.state()),
            turn: mgr.record().current_turn,
            roles: roles_info,
        });
    }

    // Build per-role AgentRunners
    let merged = &config.agent_config;
    let resolver = &config.model_resolver;
    let default_params = &config.default_params;
    let mut runners: Vec<(String, AgentRunner)> = Vec::with_capacity(order.len());

    for role_id in &order {
        let tier_str = config
            .initial_tier
            .as_ref()
            .map(|t| t.label().to_string())
            .or_else(|| {
                merged
                    .roles
                    .get(role_id)
                    .map(|r| r.model_tier.clone())
            })
            .unwrap_or_else(|| "standard".to_string());
        let tier = ModelTier::parse(&tier_str).unwrap_or(ModelTier::Standard);

        match build_runner(
            merged,
            resolver,
            default_params,
            role_id,
            tier,
            config.primary_model_id.as_deref(),
            Some(session_arc.clone()),
            event_tx,
            &config.cwd,
            config.subsession_store.clone(),
            &config.session_id,
            cancel_flag.clone(),
            turn_cancel_flag.clone(),
        )
        .await
        {
            Ok((mut runner, _canonical_id)) => {
                runner = runner.with_inject_worktree_root(worktree_root.clone());
                runner = runner.with_advisor_hints(advisor_hints.clone());
                runners.push((role_id.clone(), runner));
            }
            Err(e) => {
                let _ = event_tx.send(ChatEvent::Error {
                                kind: None,
                                message: format!("构建 role '{role_id}' runner 失败: {e}"),
                                sub_id: None,
                            });
                return;
            }
        }
    }

    // ─── Round loop ────────────────────────────────────────────
    'rounds: for round_num in 1..=scheduler.max_rounds {
        if cancel_flag.load(Ordering::SeqCst) {
            break 'rounds;
        }

        // Wait for user input
        let line = loop {
            if cancel_flag.load(Ordering::SeqCst) {
                break 'rounds;
            }
            match input_rx.recv().await {
                Some(ControllerInput::Input(text)) => {
                    let trimmed = text.trim().to_string();
                    if !trimmed.is_empty() {
                        break trimmed;
                    }
                }
                Some(ControllerInput::Pause) => {
                    let mut mgr = session_arc.lock().await;
                    let _ = mgr.pause("user /pause");
                    let _ = event_tx.send(ChatEvent::Paused {
                        reason: "用户暂停".into(),
                    });
                    break 'rounds;
                }
                Some(ControllerInput::Abort) => break 'rounds,
                // 取消当前 turn：仅打断正在跑的 run_turn，不退出
                // 整个 round 循环。turn_cancel_flag 会被 run_turn
                // 的 select! 循环检测到，丢弃该轮结果；下一轮用户
                // 输入照常接收。
                Some(ControllerInput::CancelTurn) => {
                    turn_cancel_flag.store(true, Ordering::SeqCst);
                }
                Some(ControllerInput::Resume) => {}
                Some(ControllerInput::AdvisorHint(text)) => {
                    advisor_hints.lock().push_back(text);
                }
                Some(ControllerInput::SwitchRole(_)) | Some(ControllerInput::SwitchModel(_)) => {
                    let _ = event_tx.send(ChatEvent::Status {
                        message: "multi-role 模式不支持 /role 或 /model 命令".into(),
                    });
                }
                None => break 'rounds,
            }
        };

        // Process line: slash command, @role, or manager input
        if line.starts_with('/') {
            match line.as_str() {
                "/exit" | "/quit" => break 'rounds,
                "/pause" => {
                    let mut mgr = session_arc.lock().await;
                    let _ = mgr.pause("user /pause");
                    let _ = event_tx.send(ChatEvent::Paused {
                        reason: "用户暂停".into(),
                    });
                    break 'rounds;
                }
                "/roles" => {
                    let mut roles_info: Vec<RoleInfo> = Vec::new();
                    for id in &order {
                        let icon = merged
                            .roles
                            .get(id)
                            .map(|r| r.icon.clone())
                            .unwrap_or_else(|| role_icon(id));
                        roles_info.push(RoleInfo {
                            id: id.clone(),
                            name: id.clone(),
                            icon,
                        });
                    }
                    let _ = event_tx.send(ChatEvent::RoleList { roles: roles_info });
                    continue 'rounds;
                }
                line if line.starts_with("/rounds") => {
                    let mgr = session_arc.lock().await;
                    let _ = event_tx.send(ChatEvent::Status {
                        message: format!(
                            "[round: {} / {}]",
                            mgr.record().current_turn,
                            scheduler.max_rounds
                        ),
                    });
                    continue 'rounds;
                }
                cmd => {
                    let parts: Vec<&str> = line.splitn(2, ' ').collect();
                    let topic = parts.get(1).unwrap_or(&"").trim();
                    match run_workflow_command(cmd, topic, &config, &event_tx, cancel_flag.clone()).await {
                        Ok(Some(summary)) => {
                            let _ = event_tx.send(ChatEvent::Status {
                                message: format!("Workflow '{cmd}' 完成。结果已交给 manager 处理。"),
                            });
                            let inject_dir = worktree_root.join(".latte").join("inject");
                            let _ = std::fs::create_dir_all(&inject_dir);
                            let inject_path = inject_dir.join("manager.txt");
                            let mgr_text = format!(
                                "Workflow '{cmd}' 已完成。用户请求：{topic}\n\n结果：\n{summary}\n\n请根据结果给出结论或下一步。"
                            );
                            let _ = std::fs::write(&inject_path, &mgr_text);
                            continue 'rounds;
                        }
                        Ok(None) => {
                            let _ = event_tx.send(ChatEvent::Status {
                                message: format!(
                                    "[unknown {cmd} — known: pause resume quit roles rounds, plus any workflow command]"
                                ),
                            });
                            continue 'rounds;
                        }
                        Err(e) => {
                            let _ = event_tx.send(ChatEvent::Status {
                                message: format!("Workflow '{cmd}' 失败: {e}"),
                            });
                            continue 'rounds;
                        }
                    }
                }
            }
        }

        // @role message injection
        if let Some(rest) = line.strip_prefix('@') {
            if let Some((role_id, message)) = rest.split_once(' ') {
                let role_id = role_id.trim();
                let message = message.trim();
                if order.iter().any(|r| r == role_id) {
                    let inject_dir = worktree_root.join(".latte").join("inject");
                    let _ = std::fs::create_dir_all(&inject_dir);
                    let inject_path = inject_dir.join(format!("{role_id}.txt"));
                    if let Ok(existing) = std::fs::read_to_string(&inject_path) {
                        let combined = format!("{existing}\n{message}");
                        let _ = std::fs::write(&inject_path, combined);
                    } else {
                        let _ = std::fs::write(&inject_path, message);
                    }
                    let _ = event_tx.send(ChatEvent::Status {
                        message: format!("[{role_id} queue: +1 message]"),
                    });
                } else {
                    let _ = event_tx.send(ChatEvent::Status {
                        message: format!("[error: unknown role '{role_id}']"),
                    });
                }
            } else {
                let _ = event_tx.send(ChatEvent::Status {
                    message: "[parse error: malformed @role line]".into(),
                });
            }
            continue 'rounds;
        }

        // Plain text = manager input
        {
            let mut mgr = session_arc.lock().await;
            mgr.append_to_role(
                "manager",
                Message::user(line.clone()),
            )
            .ok();
            let _ = event_tx.send(ChatEvent::Status {
                message: format!("[manager turn enqueued: {} chars]", line.len()),
            });
        }

        // ─── Execute round ─────────────────────────────────────
        let _ = event_tx.send(ChatEvent::RoundStarted { round: round_num });

        {
            let mgr = session_arc.lock().await;
            mgr.emit_round_started(round_num, &order);
        }

        for role_id in &order {
            if cancel_flag.load(Ordering::SeqCst) {
                break 'rounds;
            }

            {
                let mut mgr = session_arc.lock().await;
                if !matches!(
                    mgr.state(),
                    SessionState::Created | SessionState::Running | SessionState::Resumed
                ) {
                    let _ = event_tx.send(ChatEvent::Status {
                        message: format!(
                            "[session not running — current state: {:?}]",
                            mgr.state()
                        ),
                    });
                    continue 'rounds;
                }

                // Drain inject queue
                let inject_path = mgr
                    .worktree_root()
                    .join(".latte")
                    .join("inject")
                    .join(format!("{role_id}.txt"));
                if inject_path.exists() {
                    if let Ok(content) = std::fs::read_to_string(&inject_path) {
                        if !content.trim().is_empty() {
                            let synth = Message::user(format!("[INJECTED]\n{}", content));
                            mgr.append_to_role(role_id, synth).ok();
                        }
                        let _ = std::fs::remove_file(&inject_path);
                    }
                }
                // Slice plan.md for this role
                let plan_slice = plan_md_slice_for(&mgr.record().plan_md, role_id);
                if !plan_slice.is_empty() {
                    let synth = Message::user(format!("[PLAN SLICE]\n{}", plan_slice));
                    mgr.append_to_role(role_id, synth).ok();
                }
                mgr.advance_turn().ok();
            }

            // Find the runner for this role
            let runner = match runners.iter_mut().find(|(id, _)| id == role_id) {
                Some((_, r)) => r,
                None => {
                    let _ = event_tx.send(ChatEvent::Status {
                        message: format!("[error: no runner for role '{role_id}']"),
                    });
                    continue;
                }
            };

            // Replay role history into runner context
            {
                let mgr = session_arc.lock().await;
                let history = mgr.role_history(role_id);
                let ctx = runner.context_mut();
                ctx.clear();
                for m in history {
                    ctx.push(m);
                }
            }

            let _ = event_tx.send(ChatEvent::Status {
                message: format!("[{role_id} round {round_num}: calling LLM...]"),
            });
            let _ = event_tx.send(ChatEvent::RoleStarted {
                role_id: role_id.clone(),
                detail: format!("round {round_num}: calling LLM"),
            });

            let new_assistant_text = match run_turn_cancellable(
                runner,
                &[],
                &cancel_flag,
                &turn_cancel_flag,
            ).await {
                Ok(text) => {
                    let _ = event_tx.send(ChatEvent::RoleFinished {
                        role_id: role_id.clone(),
                        detail: format!("round {round_num}: ok, {} chars", text.len()),
                    });
                    text
                }
                Err(e) => {
                    let _ = event_tx.send(ChatEvent::Status {
                        message: format!("[{role_id} round {round_num}: error: {e}]"),
                    });
                    let _ = event_tx.send(ChatEvent::RoleFinished {
                        role_id: role_id.clone(),
                        detail: format!("round {round_num}: error: {e}"),
                    });
                    String::new()
                }
            };

            if !new_assistant_text.is_empty() {
                let _ = event_tx.send(ChatEvent::Status {
                    message: format!("[{role_id} round {round_num}: ok, {} chars]", new_assistant_text.len()),
                });

                let _ = event_tx.send(ChatEvent::RoleTurn {
                    role_id: role_id.clone(),
                    content: new_assistant_text.clone(),
                    is_complete: true,
                    sub_id: None,
                });

                let mut mgr = session_arc.lock().await;
                let assistant_msg = Message::assistant(new_assistant_text);
                if let Err(e) = mgr.append_to_role(role_id, assistant_msg) {
                    let _ = event_tx.send(ChatEvent::Status {
                        message: format!("[{role_id} round {round_num}: failed to append: {e}]"),
                    });
                }
                drop(mgr);
            }

            // ─── Context threshold check ──────────────────────────────
            // Check accumulated token usage against the session token
            // budget. Uses the runner's actual `total_usage` (not a
            // hardcoded placeholder). If the budget is exceeded, the
            // supervisor triggers an auto-pause and we skip remaining
            // roles in this round.
            let usage = runner.total_usage();
            let tokens_used = usage.input_tokens + usage.output_tokens + usage.thinking_tokens;
            let decision_kind = runner.last_decision_kind();
            let pause_reason = scheduler
                .supervisor
                .observe(role_id, tokens_used, &decision_kind);
            if let Some(reason) = pause_reason {
                let mut mgr = session_arc.lock().await;
                let _ = mgr.pause_with_reason(&reason);
                mgr.emit_round_ended(round_num);
                let _ = event_tx.send(ChatEvent::Paused {
                    reason: reason.clone(),
                });
                let _ = event_tx.send(ChatEvent::Status {
                    message: format!("[supervisor pause: {reason}]"),
                });
                continue 'rounds;
            }
        }


        // Round ended

        {
            let mut mgr = session_arc.lock().await;
            mgr.emit_round_ended(round_num);
            mgr.advance_turn().ok();
        }
        let _ = event_tx.send(ChatEvent::RoundEnded { round: round_num });

        // Check pause flag
        if pause_flag.load(Ordering::SeqCst) {
            {
                let mut mgr = session_arc.lock().await;
                let _ = mgr.pause("用户请求暂停");
            }
            let _ = event_tx.send(ChatEvent::Paused {
                reason: "用户请求暂停".into(),
            });
            // Pause-wait loop: only accept Resume or Abort
            'pause: loop {
                if cancel_flag.load(Ordering::SeqCst) {
                    break 'rounds;
                }
                match input_rx.recv().await {
                    Some(ControllerInput::Resume) => {
                        let mut mgr = session_arc.lock().await;
                        if mgr.state() == SessionState::Paused {
                            let _ = mgr.resume();
                        }
                        pause_flag.store(false, Ordering::SeqCst);
                        let _ = event_tx.send(ChatEvent::Resumed);
                        break 'pause;
                    }
                    Some(ControllerInput::Abort) | None => break 'rounds,
                    Some(ControllerInput::AdvisorHint(text)) => {
                        // Paused: park the hint; the runner drains it
                        // when the session resumes.
                        advisor_hints.lock().push_back(text);
                    }
                    _ => {}
                }
            }
        }
    }

    // Cleanup
    {
        let mut mgr = session_arc.lock().await;
        if mgr.state() == SessionState::Paused {
            let _ = mgr.resume();
        }
        let _ = mgr.mark_done();
    }
}

// ─── Single-role mode ────────────────────────────────────────────

async fn run_single_role_loop(
    config: ControllerConfig,
    input_rx: &mut mpsc::UnboundedReceiver<ControllerInput>,
    event_tx: &broadcast::Sender<ChatEvent>,
    cancel_flag: Arc<AtomicBool>,
    turn_cancel_flag: Arc<AtomicBool>,
    advisor_hints: Arc<parking_lot::Mutex<std::collections::VecDeque<String>>>,
) {
    let merged = &config.agent_config;
    let resolver = &config.model_resolver;
    let default_params = &config.default_params;
    let role_id = config.roles.first().cloned().unwrap_or_else(|| "manager".into());

    let tier = config
        .initial_tier
        .or_else(|| {
            merged
                .roles
                .get(&role_id)
                .and_then(|r| ModelTier::parse(&r.model_tier).ok())
        })
        .unwrap_or(ModelTier::Standard);

    let (runner, canonical_id) = match build_runner(
        merged,
        resolver,
        default_params,
        &role_id,
        tier,
        config.primary_model_id.as_deref(),
        None,
        event_tx,
        &config.cwd,
        config.subsession_store.clone(),
        &config.session_id,
        cancel_flag.clone(),
        turn_cancel_flag.clone(),
    )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let _ = event_tx.send(ChatEvent::Error {
                            kind: None,
                            message: format!("构建 runner 失败: {e}"),
                            sub_id: None,
                        });
            return;
        }
    };
    let mut runner = runner.with_advisor_hints(advisor_hints.clone());

    // Seed the runner with any pre-existing history (e.g. when
    // resuming a paused session from the SessionStore). Done
    // before the first turn runs so the controller has full prior
    // context. Tokens are re-counted lazily; budget enforcement
    // happens on the next turn's start.
    for msg in &config.initial_history {
        runner.context_mut().push(msg.clone());
    }
    let mut current_role = canonical_id;
    let mut current_tier = tier;
    let current_primary = config.primary_model_id.clone();

    let icon = merged
        .roles
        .get(&current_role)
        .map(|r| r.icon.clone())
        .unwrap_or_else(|| role_icon(&current_role));
    let model_id = runner
        .agent()
        .model_chain
        .first()
        .map(|mc| mc.model.id.clone())
        .unwrap_or_else(|| "?".to_string());

    let _ = event_tx.send(ChatEvent::Prompt {
        icon: icon.clone(),
        role_id: current_role.clone(),
        model_id: model_id.clone(),
    });

    let _ = event_tx.send(ChatEvent::SessionInfo {
        task_id: config.task_id.clone().unwrap_or_else(|| "single-role".into()),
        state: "Running".into(),
        turn: 0,
        roles: vec![RoleInfo {
            id: current_role.clone(),
            name: current_role.clone(),
            icon: icon.clone(),
        }],
    });

    loop {
        if cancel_flag.load(Ordering::SeqCst) {
            break;
        }

        tokio::select! {
            input = input_rx.recv() => {
                match input {
                    None => break,
                    Some(ControllerInput::Input(text)) => {
                        let trimmed = text.trim().to_string();
                        if trimmed.is_empty() {
                        }

                        if trimmed.starts_with('/') {
                            let parts: Vec<&str> = trimmed.splitn(2, ' ').collect();
                            let cmd = parts[0];
                            match cmd {
                                "/exit" | "/quit" => break,
                                "/help" => {
                                    let _ = event_tx.send(ChatEvent::Status {
                                        message: "Commands: /role <id>  /model <tier>  /roles  /clear  /status  /tools  /history  /save <file>  /load <file>  /help  /exit".into(),
                                    });
                                }
                                "/roles" => {
                                    let ids: Vec<&String> = merged.roles.keys().collect();
                                    let roles_info: Vec<RoleInfo> = ids.iter().map(|id| {
                                        let tpl = merged.roles.get(*id).unwrap();
                                        RoleInfo { id: (*id).clone(), name: tpl.name.clone(), icon: tpl.icon.clone() }
                                    }).collect();
                                    let _ = event_tx.send(ChatEvent::RoleList { roles: roles_info });
                                }
                                "/role" => {
                                    let Some(new_role) = parts.get(1) else {
                                        let _ = event_tx.send(ChatEvent::Status { message: "usage: /role <id>".into() });
                                        continue;
                                    };
                                    let history: Vec<Message> = runner.context().messages().to_vec();
                                    match build_runner(merged, resolver, default_params, new_role, current_tier, current_primary.as_deref(), None, event_tx, &config.cwd, config.subsession_store.clone(), &config.session_id, cancel_flag.clone(), turn_cancel_flag.clone()).await {
                                        Ok((mut new_runner, rid)) => {
                                            for m in history { new_runner.context_mut().push(m); }
                                            runner = new_runner.with_advisor_hints(advisor_hints.clone());
                                            current_role = rid.clone();
                                            let mid = runner.agent().model_chain.first().map(|mc| mc.model.id.clone()).unwrap_or_else(|| "?".into());
                                            let ico = merged.roles.get(&current_role).map(|r| r.icon.clone()).unwrap_or_else(|| role_icon(&current_role));
                                            let _ = event_tx.send(ChatEvent::Status { message: format!("Switched to role '{rid}' (tier {})", current_tier.label()) });
                                            let _ = event_tx.send(ChatEvent::Prompt { icon: ico, role_id: current_role.clone(), model_id: mid });
                                        }
                                        Err(e) => { let _ = event_tx.send(error_event(&e, "switch role failed", None)); }
                                    }
                                }
                                "/model" => {
                                    let Some(t) = parts.get(1) else {
                                        let _ = event_tx.send(ChatEvent::Status { message: "usage: /model <premium|standard|budget>".into() });
                                        continue;
                                    };
                                    match ModelTier::parse(t) {
                                        Ok(new_tier) => {
                                            let role = current_role.clone();
                                            let history: Vec<Message> = runner.context().messages().to_vec();
                                            match build_runner(merged, resolver, default_params, &role, new_tier, current_primary.as_deref(), None, event_tx, &config.cwd, config.subsession_store.clone(), &config.session_id, cancel_flag.clone(), turn_cancel_flag.clone()).await {
                                                Ok((mut new_runner, _)) => {
                                                    for m in history { new_runner.context_mut().push(m); }
                                                    runner = new_runner.with_advisor_hints(advisor_hints.clone());
                                                    current_tier = new_tier;
                                                    let _ = event_tx.send(ChatEvent::Status { message: format!("Switched to tier {}", new_tier.label()) });
                                                }
                                                Err(e) => { let _ = event_tx.send(error_event(&e, "switch model failed", None)); }
                                            }
                                        }
                                        Err(e) => { let _ = event_tx.send(ChatEvent::Status { message: format!("invalid tier: {e}") }); }
                                    }
                                }
                                "/history" => {
                                    let msgs = runner.context().messages();
                                    let _ = event_tx.send(ChatEvent::Status { message: format!("history ({} messages):", msgs.len()) });
                                    for (i, m) in msgs.iter().enumerate() {
                                        let preview = if m.content.len() > 200 { format!("{}...", &m.as_text()[..200]) } else { m.as_text() };
                                        let _ = event_tx.send(ChatEvent::Status { message: format!("  {i} [{:?}] {preview}", m.role) });
                                    }
                                }
                                cmd => {
                                    let topic = parts.get(1).unwrap_or(&"").trim();
                                    match run_workflow_command(cmd, topic, &config, &event_tx, cancel_flag.clone()).await {
                                        Ok(Some(summary)) => {
                                            let _ = event_tx.send(ChatEvent::Status {
                                                message: format!("Workflow '{cmd}' 完成。\n{summary}"),
                                            });
                                        }
                                        Ok(None) => {
                                            let _ = event_tx.send(ChatEvent::Status {
                                                message: format!(
                                                    "[unknown {cmd} — known: role model roles clear status tools history save load help exit quit, plus any workflow command]"
                                                ),
                                            });
                                        }
                                        Err(e) => {
                                            let _ = event_tx.send(ChatEvent::Status {
                                                message: format!("Workflow '{cmd}' 失败: {e}"),
                                            });
                                        }
                                    }
                                }
                            }
                            continue;
                        }

                        let _ = event_tx.send(ChatEvent::Status { message: format!("[calling LLM for role '{current_role}'...]") });
                        let _ = event_tx.send(ChatEvent::UserMessage { text: trimmed.clone() });
                        let _ = event_tx.send(ChatEvent::RoleStarted {
                            role_id: current_role.clone(),
                            detail: "calling LLM".into(),
                        });
let usage_before = runner.total_usage().clone();
                        // Run the turn with cancellation support (no
                        // auto-timeout — user hits the stop button).
                        let user_msg = [Message::user(trimmed.clone())];
                        let turn_result = run_turn_cancellable(
                            &mut runner,
                            &user_msg,
                            &cancel_flag,
                            &turn_cancel_flag,
                        )
                        .await;
                        match turn_result {
                            Ok(response) => {
                                let mid = runner.agent().model_chain.first().map(|mc| mc.model.id.clone()).unwrap_or_else(|| "?".into());
                                let ico = merged.roles.get(&current_role).map(|r| r.icon.clone()).unwrap_or_else(|| role_icon(&current_role));
                                let usage_after = runner.total_usage();
                                let in_delta = usage_after.input_tokens - usage_before.input_tokens;
                                let out_delta = usage_after.output_tokens - usage_before.output_tokens;
                                let _ = event_tx.send(ChatEvent::RoleTurn { role_id: current_role.clone(), content: response.clone(), is_complete: true, sub_id: None });
                                let _ = event_tx.send(ChatEvent::Status { message: format!("[{current_role} · {mid} · tokens: +{in_delta} in / +{out_delta} out]") });
                                let _ = event_tx.send(ChatEvent::RoleFinished {
                                    role_id: current_role.clone(),
                                    detail: format!("ok, {} chars", response.len()),
                                });
                                let _ = event_tx.send(ChatEvent::Prompt { icon: ico, role_id: current_role.clone(), model_id: mid });
                            }
                            Err(e) => {
                                let _ = event_tx.send(ChatEvent::RoleFinished {
                                    role_id: current_role.clone(),
                                    detail: format!("error: {e}"),
                                });
                                let _ = event_tx.send(error_event(&e, "turn failed", None));
                            }
                        }
                    }
                    Some(ControllerInput::SwitchRole(new_role)) => {
                        let history: Vec<Message> = runner.context().messages().to_vec();
                        match build_runner(merged, resolver, default_params, &new_role, current_tier, current_primary.as_deref(), None, event_tx, &config.cwd, config.subsession_store.clone(), &config.session_id, cancel_flag.clone(), turn_cancel_flag.clone()).await {
                            Ok((mut new_runner, rid)) => {
                                for m in history { new_runner.context_mut().push(m); }
                                runner = new_runner.with_advisor_hints(advisor_hints.clone());
                                current_role = rid.clone();
                                let mid = runner.agent().model_chain.first().map(|mc| mc.model.id.clone()).unwrap_or_else(|| "?".into());
                                let ico = merged.roles.get(&current_role).map(|r| r.icon.clone()).unwrap_or_else(|| role_icon(&current_role));
                                let _ = event_tx.send(ChatEvent::Status { message: format!("Switched to role '{rid}' (tier {})", current_tier.label()) });
                                let _ = event_tx.send(ChatEvent::Prompt { icon: ico, role_id: current_role.clone(), model_id: mid });
                            }
                            Err(e) => { let _ = event_tx.send(error_event(&e, "switch role failed", None)); }
                        }
                    }
                    Some(ControllerInput::SwitchModel(new_tier)) => {
                        let history: Vec<Message> = runner.context().messages().to_vec();
                        match build_runner(merged, resolver, default_params, &current_role, new_tier, current_primary.as_deref(), None, event_tx, &config.cwd, config.subsession_store.clone(), &config.session_id, cancel_flag.clone(), turn_cancel_flag.clone()).await {
                            Ok((mut new_runner, _)) => {
                                for m in history { new_runner.context_mut().push(m); }
                                runner = new_runner.with_advisor_hints(advisor_hints.clone());
                                current_tier = new_tier;
                                let _ = event_tx.send(ChatEvent::Status { message: format!("Switched to tier {}", new_tier.label()) });
                            }
                            Err(e) => { let _ = event_tx.send(error_event(&e, "switch model failed", None)); }
                        }
                    }
                    Some(ControllerInput::Pause) => {
                        let _ = event_tx.send(ChatEvent::Paused { reason: "用户暂停".into() });
                    }
                    Some(ControllerInput::Resume) => {
                        let _ = event_tx.send(ChatEvent::Resumed);
                    }
                    Some(ControllerInput::AdvisorHint(text)) => {
                        // Idle-side delivery: park the hint in the
                        // shared queue; the runner drains it at the
                        // next turn start. (Mid-turn delivery happens
                        // via the direct `advisor_hint()` push, not
                        // this channel.)
                        advisor_hints.lock().push_back(text);
                    }
                    // 取消当前 turn：仅打断正在跑的 run_turn，不退出
                    // 整个 session。run_single_role_loop 的 select! 循环
                    // 会检测 turn_cancel_flag 并在下一个 500ms tick
                    // 丢弃 run_turn future。
                    Some(ControllerInput::CancelTurn) => {
                        turn_cancel_flag.store(true, Ordering::SeqCst);
                    }
                    Some(ControllerInput::Abort) | None => break,
                }
            }
        }
    }
}

async fn build_runner(
    merged: &AgentConfig,
    resolver: &ModelResolver,
    default_params: &GenerateParams,
    role_id: &str,
    tier: ModelTier,
    primary_id: Option<&str>,
    session: Option<Arc<Mutex<SessionManager>>>,
    event_tx: &broadcast::Sender<ChatEvent>,
    cwd: &Path,
    subsession_store: Arc<SubsessionStore>,
    // UI session id —— 见 [`ControllerConfig::session_id`]。透传给
    // `register_delegate_tool`，最终到 `subsession_store.create()`。
    // 空串 → 旧行为（用 cwd 当 key，落盘会被拒绝）。
    session_id: &str,
    cancel_flag: Arc<AtomicBool>,
    turn_cancel_flag: Arc<AtomicBool>,
) -> AgentResult<(AgentRunner, String)> {
    let template = merged
        .roles
        .get(role_id)
        .ok_or_else(|| AgentError::RoleNotFound(role_id.to_string()))?
        .clone();
    let role = template.resolve(default_params).await?;
    let models = if let Some(id) = primary_id {
        let mut chain = Vec::new();
        chain.push(resolver.resolve_id_or_name(id)?);
        for cid in &role.model_chain {
            if chain.iter().any(|m| m.id == *cid) {
                continue;
            }
            if let Ok(m) = resolver.build_model(cid) {
                chain.push(m);
            }
        }
        chain
    } else {
        resolver.resolve_chain(&role.id, tier, &role.model_chain)?
    };
    if models.is_empty() {
        return Err(AgentError::ModelsUnavailable {
            tried: vec![role_id.to_string()],
            next_retry_in: None,
        });
    }
    let mut role = role;

    // Append the tool-call protocol + (for manager) the delegate hints.
    let mut prompt = String::new();
    if !role.allowed_tools.is_empty() {
        prompt.push_str(&tool_usage_prompt(&role.allowed_tools));
        // Any role with "delegate" in its tools gets the delegation capability
        if role.allowed_tools.iter().any(|t| t == "delegate") {
            prompt.push_str(&delegate_tool_hint(merged));
        }
        // Any role with "workflow" in its tools gets the workflow hint.
        if role.allowed_tools.iter().any(|t| t == "workflow") {
            prompt.push_str(&workflow_tool_hint(cwd));
        }
    }
    // Every role — tools or no tools — gets the system ground truth
    // (cwd, host, time). When the user asks about runtime state
    // ("current directory?", "pwd?", …) the model sees real values
    // instead of guessing from training data. This is structural
    // ground-truth injection: we *give* the model the fact, we don't
    // *forbid* hallucinations.
    prompt.push_str(&crate::ground_truth::ground_truth_block(cwd));
    role.system_prompt.push_str(&prompt);

    let agent = Agent::new_with_chain(role_id.to_string(), role.clone(), models, default_params.clone())?;

    if !role.allowed_tools.is_empty() {
        let tm = build_tool_manager(&role.allowed_tools).await
            .map_err(|e| AgentError::Tool(format!("build tool manager '{role_id}': {e}")))?;
        // Any role with "delegate" in allowed_tools gets the delegate tool registered,
        // enabling nested subsessions (roleA delegates to roleB, roleB can also delegate)
        if role.allowed_tools.iter().any(|t| t == "delegate") {
            // 必须是真 session_id —— subsession_store 用它当落盘目录名。
            // 之前这里用 `cwd.to_string_lossy()` 凑数，cwd 含 `/` 会被
            // 路径安全检查拒绝，整路 subagent 退化到内存。
            let sid = session_id.to_string();
            register_delegate_tool(
                &tm,
                merged,
                resolver,
                default_params.clone(),
                event_tx.clone(),
                cwd.to_path_buf(),
                subsession_store.clone(),
                sid,
                cancel_flag.clone(),
                turn_cancel_flag.clone(),
            )
            .await
            .map_err(|e| AgentError::Tool(format!("register delegate: {e}")))?;
        }
        // Any role with "workflow" in allowed_tools gets the workflow
        // tool: trigger a named multi-role workflow by name + topic.
        if role.allowed_tools.iter().any(|t| t == "workflow") {
            register_workflow_tool(
                &tm,
                merged,
                resolver,
                default_params.clone(),
                event_tx.clone(),
                cwd.to_path_buf(),
                cancel_flag.clone(),
                turn_cancel_flag.clone(),
            )
            .await
            .map_err(|e| AgentError::Tool(format!("register workflow: {e}")))?;
        }
        // Any role with "generate_image" in allowed_tools gets the image
        // generation tool (OpenAI-compatible /v1/images/generations).
        if role.allowed_tools.iter().any(|t| t == "generate_image") {
            crate::image_gen::register_generate_image_tool(
                &tm,
                merged,
                event_tx.clone(),
                cwd.to_path_buf(),
                role_id.to_string(),
            )
            .map_err(|e| AgentError::Tool(format!("register generate_image: {e}")))?;
        }
        // Any role with "plan" in allowed_tools gets the plan tool:
        // 把结构化任务清单提交给用户在弹窗里勾选导入任务看板。
        // manager 在 implementation_plan workflow 跑完后调它。
        if role.allowed_tools.iter().any(|t| t == "plan") {
            register_plan_tool(&tm, event_tx.clone(), role_id.to_string())
                .map_err(|e| AgentError::Tool(format!("register plan: {e}")))?;
        }

        // ── 给主 runner 分配 subsession sink（manager / 任何角色通用） ──
        let subsession_sink: Option<Arc<dyn crate::trace::TraceSink>> = if session_id.is_empty() {
            None
        } else {
            let (_sub_id, sink) = subsession_store.create(session_id, role_id);
            Some(sink)
        };
        let mut runner = AgentRunner::new_with_tools(agent, tm, 16)
            .with_role(role_id)
            .with_cwd(cwd.to_path_buf());
        if let Some(sink) = subsession_sink.as_ref() {
            runner = runner.with_sink(sink.clone());
        }
        Ok((runner, role_id.to_string()))
    } else {
        // ── 给主 runner 分配 subsession sink（同上） ──
        let subsession_sink: Option<Arc<dyn crate::trace::TraceSink>> = if session_id.is_empty() {
            None
        } else {
            let (_sub_id, sink) = subsession_store.create(session_id, role_id);
            Some(sink)
        };
        let mut runner = AgentRunner::new(agent)
            .with_role(role_id)
            .with_cwd(cwd.to_path_buf());
        if let Some(sink) = subsession_sink.as_ref() {
            runner = runner.with_sink(sink.clone());
        }
        Ok((runner, role_id.to_string()))
    }
}


pub(crate) async fn build_tool_manager(
    allowed: &[String],
) -> Result<Arc<dyn latte_rs_agent_tools::types::ToolManager>, Box<dyn std::error::Error + Send + Sync>> {
    use latte_rs_agent_tools::prelude::*;
    let mgr = create_tool_manager();
    for p in builtin_tool_packages() {
        mgr.register_package(p).await
            .map_err(|e| format!("register_package: {e}"))?;
    }
    // allowed 里的名字现在是扁平规范名（bash/read/edit/...），直接与
    // registry 注册名匹配，不再有 bash->shell.exec 之类的别名反向映射。
    let mut keep: std::collections::HashSet<String> = allowed
        .iter()
        .flat_map(|s| vec![s.to_lowercase(), s.clone()])
        .collect();
    // 配置层向后兼容映射：旧配置名 → 新扁平规范名。用户已有的
    // `~/.latte/agents.d/*.toml` 里可能用 "exec"（现为 "bash"）、
    // "diff"（现为 "git_diff"）、"playwright_screenshot"（现为 "screenshot"）
    let compat_map: std::collections::HashMap<&str, &str> = [
        ("exec", "bash"),
        ("playwright_screenshot", "screenshot"),
        // 旧 namespace 短名映射：git 系列现在用 "git_diff"/"git_log"/等。
        ("diff", "git_diff"),
        ("status", "git_status"),
        ("log", "git_log"),
        ("branch", "git_branch"),
        ("commit", "git_commit"),
        ("add", "git_add"),
    ].into_iter().collect();
    for (old, new) in &compat_map {
        if keep.contains(*old) {
            keep.insert(new.to_string());
        }
    }
    // mcp 是配置层分组别名（一个名字展开成 3 个 mcp_* 工具），不是工具名别名。
    if keep.contains("mcp") || keep.contains("mcp_connect") {
        keep.insert("mcp_connect".to_string());
        keep.insert("mcp_list".to_string());
        keep.insert("mcp_call".to_string());
    }
    // playwright 同理：展开成 screenshot + playwright_script。
    if keep.contains("playwright") {
        keep.insert("screenshot".to_string());
        keep.insert("playwright_script".to_string());
    }
    // Register code_graph tool if allowed
    if keep.contains("code_graph") || keep.contains("code-graph") {
        let cg = code_graph_tool();
        mgr.register(cg, None);
    }
    // 工具名已是扁平（无点号），short == tool_id；保留 rsplit_once 兜底
    // 仅为兼容可能遗留的 namespace 工具。
    for tool_id in mgr.get_tool_names() {
        let short = tool_id
            .rsplit_once('.')
            .map(|(_, s)| s.to_string())
            .unwrap_or_else(|| tool_id.clone());
        if !(keep.contains(&short) || keep.contains(&tool_id)) {
            mgr.unregister(&tool_id);
        }
    }
    Ok(mgr)
}

fn code_graph_tool() -> latte_rs_agent_tools::types::Tool {
    use latte_rs_agent_tools::types::*;
    use latte_rs_agent_tools::error::ToolError;
    use std::sync::Arc;
    use serde_json::json;

    fn prop(ty: PropertyType, desc: &str) -> ToolInputProperty {
        ToolInputProperty {
            property_type: ty,
            description: Some(desc.into()),
            enum_values: None, minimum: None, maximum: None, min_length: None, max_length: None,
        }
    }
    fn optional_schema(props: Vec<(&str, PropertyType, &str)>) -> ToolInputSchema {
        let mut p = std::collections::BTreeMap::new();
        for (name, ty, desc) in props {
            p.insert(name.to_string(), prop(ty, desc));
        }
        ToolInputSchema {
            schema_type: SchemaType::default(),
            properties: p,
            required: None,
            additional_properties: None,
        }
    }

    let handler: SharedToolHandler = Arc::new(|input: serde_json::Value, _ctx| {
        Box::pin(async move {
            let pattern = input.get("pattern")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::other("pattern is required"))?;
            let path = input.get("path")
                .and_then(|v| v.as_str())
                .unwrap_or(".");
            let kind = input.get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("function");
            let query = match kind {
                "function" => format!("fn $NAME($$$PARAMS) -> $RET {{ $$$BODY }}"),
                "struct" => format!("struct $NAME {{ $$$FIELDS }}"),
                "class" => format!("class $NAME {{ $$$BODY }}"),
                "import" => format!("import {{ $$$IMPORTS }} from \"$SRC\""),
                "interface" => format!("interface $NAME {{ $$$BODY }}"),
                "trait" => format!("trait $NAME {{ $$$BODY }}"),
                "impl" => format!("impl $NAME {{ $$$BODY }}"),
                "call" => format!("$CALLEE($$$ARGS)"),
                _ => pattern.to_string(),
            };
            let output = tokio::process::Command::new("sg")
                .args(["-p", &query, path])
                .output().await
                .map_err(|e| ToolError::execution_str("code_graph", format!("sg failed: {e}")))?;
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let mut result = serde_json::Map::new();
            result.insert("matches".into(), serde_json::Value::String(stdout));
            if !stderr.is_empty() {
                result.insert("stderr".into(), serde_json::Value::String(stderr));
            }
            Ok(serde_json::Value::Object(result))
        })
    });
    let schema = optional_schema(vec![
        ("pattern", PropertyType::String, "AST 模式，如 fn $NAME($$$PARAMS) -> $RET { $$$BODY }"),
        ("path", PropertyType::String, "搜索路径，默认当前目录"),
        ("kind", PropertyType::String, "查询类型: function/struct/class/import/interface/trait/impl/call"),
    ]);
    Tool::builder("code_graph", "用 AST 模式查询代码结构，比 grep 更精准。支持多种结构类型查询", schema, handler)
        .timeout(std::time::Duration::from_secs(30))
        .build()
}

pub(crate) fn tool_usage_prompt(allowed: &[String]) -> String {
    let names_str = allowed.join(", ");
    format!(
        r#"
## Tools

You have access to the following tools: {names_str}.
Call tools via the native function-calling interface (the request `tools`
field carries each tool's name, description, and JSON schema). Do NOT emit
`<tool_call>` text blocks -- they are no longer parsed. Inspect each tool
result and continue until the task is done.
"#
    )
}

/// 当前配置里的角色花名册：`id(Name)` 排序拼接。delegate 工具的
/// description/schema 和系统提示共用这一份 —— 角色编辑器新建/改名的
/// 角色下个 session 自动出现，不靠静态列表。
pub(crate) fn role_roster_text(merged: &AgentConfig) -> String {
    let mut entries: Vec<String> = merged
        .roles
        .values()
        .map(|t| {
            if t.name.is_empty() || t.name == t.id {
                t.id.clone()
            } else {
                format!("{}({})", t.id, t.name)
            }
        })
        .collect();
    entries.sort();
    entries.join(", ")
}

/// delegate 工具提示：可用专家列表来自 `merged.roles` 动态生成
/// （与角色编辑器同源），不再硬编码 7 个基础角色。
fn delegate_tool_hint(merged: &AgentConfig) -> String {
    let roster = role_roster_text(merged);
    let roster_line = if roster.is_empty() {
        "（当前配置里没有任何角色）".to_string()
    } else {
        roster
    };
    format!(
        r#"
### 关键规则：你必须使用 delegate 工具

你**必须**使用 `delegate` 工具来完成任务，**绝不能**直接输出计划而不执行。

正确的流程：
1. 分析任务 → 决定需要哪些专家
2. **立即**调用 delegate 工具派发任务
3. 等专家返回结果
4. 综合所有结果输出最终答案

可用专家（来自角色配置，含角色编辑器新建的自定义角色）：{roster_line}

失败升级：工具调用连续失败、专家报错且原因不明、或需要在多个方案间取舍时 → 派 advisor 诊断；诊断清楚之前不要直接回答用户，更不要重复回答旧问题。

**错误流程（禁止）：**
- 只输出计划而不调用 delegate ← 这是最常见的错误！不要这样做！
- 自己分析而不派发给专家

用 native function-calling 调 delegate 工具（参数 role + task）。
"#
    )
}

/// workflow 工具提示：动态列出 `.latte/workflows.d`（项目 + 全局）里
/// 实际可用的 workflow —— UI 管理界面新建的自定义流程会出现在这里，
/// 模型不需要靠猜。清单为空时退化成不带列表的通用提示。
fn workflow_tool_hint(cwd: &std::path::Path) -> String {
    let available = crate::workflow::list_workflows(cwd);
    let list = if available.is_empty() {
        "（当前 .latte/workflows.d 里没有可用 workflow，只能 delegate）\n".to_string()
    } else {
        available
            .iter()
            .map(|(n, d)| {
                if d.is_empty() {
                    format!("- `{n}`\n")
                } else {
                    format!("- `{n}` — {d}\n")
                }
            })
            .collect()
    };
    format!(
        r#"
### 你还可以用 workflow 工具触发多角色工作流

当任务适合**固定的多角色流水线**时，调用 `workflow` 而不是逐个 delegate。
当前可用的 workflow（含 UI 管理界面新建的自定义流程）：

{list}
用 native function-calling 调 workflow 工具（参数 name + topic）。

判断标准（分派前先想流程）：
- 单点问题（读代码、改文件、审查某个具体实现）→ delegate
- 需要多个角色按固定流程协作的完整任务 → workflow，从上面清单里选最贴合的
- 调用 workflow 前先用一句话说明：选哪个、为什么、预期拿到什么结论
- workflow 会跑完整条流水线并把结论返回给你；你综合后再回复用户。
"#
    )
}
/// 注册 `plan` 工具：把结构化任务清单提交给用户在弹窗里勾选导入
/// 任务看板。manager 在 `implementation_plan` workflow 跑完（或手持
/// 一份具体任务清单）后调它。与 `register_generate_image_tool` 同构
/// （schema + 捕获 event_tx 的 SharedToolHandler），但更简单--不
/// 调模型、不读写文件，只校验 tasks 并广播 `PlanProposed` 事件。
///
/// 工具返回立即（fire-and-forget）：发完事件就回"已提交 N 个候选"，
/// LLM 的 turn 结束；弹窗是纯 UI 侧异步行为，用户何时导入都行。
/// 若用户误关弹窗，可右键 PlanProposed 消息选「导入任务看板」补救
/// （右键读消息上存的结构化 tasks，不靠文本解析）。
pub(crate) fn register_plan_tool(
    tm: &Arc<dyn latte_rs_agent_tools::types::ToolManager>,
    event_tx: broadcast::Sender<ChatEvent>,
    role_id: String,
) -> Result<(), Box<dyn std::error::Error>> {
    use latte_rs_agent_tools::types::{
        PropertyType, SchemaType, SharedToolHandler, Tool, ToolInputProperty, ToolInputSchema,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    // 进程级单调序号，保证 plan_id 全局唯一。
    static PLAN_SEQ: AtomicU64 = AtomicU64::new(0);

    // tasks 是数组；ToolInputProperty 无嵌套 items schema，故用描述
    // 把每项结构讲清（title/description/priority/labels/workflow/
    // subtasks）。LLM 按描述产出，handler 逐项 serde 解析 + 校验。
    let input_schema = ToolInputSchema {
        schema_type: SchemaType,
        properties: vec![
            ("tasks".into(), ToolInputProperty {
                property_type: PropertyType::Array,
                description: Some(
                    "任务候选清单。每项是对象：{title(必填,一句话), description(做什么+验收标准), priority(1-4,1最高), labels(字符串数组), workflow(执行该任务的workflow名:tdd_development/bug_triage/update_docs;轻量任务可空), subtasks(同构数组,最多一层)}. 调用本工具后任务会出现在用户弹窗里供勾选导入任务看板，不要再以 Markdown 列表输出任务。".into()
                ),
                enum_values: None, minimum: None, maximum: None, min_length: None, max_length: None,
            }),
        ].into_iter().collect(),
        required: Some(vec!["tasks".into()]),
        ..Default::default()
    };

    let handler_role_id = role_id.clone();
    let handler: SharedToolHandler = Arc::new(move |input: serde_json::Value, _ctx| {
        let event_tx = event_tx.clone();
        let role_id = handler_role_id.clone();
        Box::pin(async move {
            let tool_err = |msg: String| latte_rs_agent_tools::error::ToolError::Other(msg);

            let tasks_val = input
                .get("tasks")
                .ok_or_else(|| tool_err("missing 'tasks' field".into()))?;
            let tasks_arr = tasks_val
                .as_array()
                .ok_or_else(|| tool_err("'tasks' must be an array".into()))?;
            if tasks_arr.is_empty() {
                return Err(tool_err("'tasks' must not be empty".into()));
            }
            // 逐项解析 + 校验 title 非空（与后端 import_tasks 的校验对齐）。
            let mut tasks: Vec<PlanTask> = Vec::with_capacity(tasks_arr.len());
            for (i, t) in tasks_arr.iter().enumerate() {
                let pt: PlanTask = serde_json::from_value(t.clone())
                    .map_err(|e| tool_err(format!("tasks[{i}] invalid: {e}")))?;
                if pt.title.trim().is_empty() {
                    return Err(tool_err(format!("tasks[{i}].title must not be empty")));
                }
                tasks.push(pt);
            }

            let plan_id = format!(
                "plan-{}-{}",
                role_id,
                PLAN_SEQ.fetch_add(1, Ordering::Relaxed)
            );
            let n = tasks.len();
            let _ = event_tx.send(ChatEvent::PlanProposed {
                role_id: role_id.clone(),
                plan_id: plan_id.clone(),
                tasks,
            });
            Ok(serde_json::Value::String(format!(
                "已提交 {n} 个任务候选给用户选择（plan_id={plan_id}）。请在弹窗中勾选要导入任务看板的项；若弹窗已关闭，可右键本条消息选「导入任务看板」补救。"
            )))
        })
    });

    let tool = Tool::builder(
        "plan".to_string(),
        "把一份结构化任务清单提交给用户，用户在弹窗里勾选后导入任务看板（backlog）。用于 implementation_plan workflow 跑完或手持具体任务清单时把任务交给看板。参数 tasks 是任务对象数组。".to_string(),
        input_schema,
        handler,
    )
    .build();

    tm.register(tool, Some(&role_id));
    Ok(())
}
async fn register_delegate_tool(
    tm: &Arc<dyn latte_rs_agent_tools::types::ToolManager>,
    merged: &AgentConfig,
    resolver: &ModelResolver,
    default_params: GenerateParams,
    event_tx: broadcast::Sender<ChatEvent>,
    cwd: PathBuf,
    subsession_store: Arc<SubsessionStore>,
    session_id: String,
    cancel_flag: Arc<AtomicBool>,
    turn_cancel_flag: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    use latte_rs_agent_tools::types::{SchemaType, SharedToolHandler, Tool};
    use tokio::sync::Semaphore;

    // 注册时把 merged.roles 的角色花名册写进 description/schema ——
    // 角色编辑器新建/改名的角色对模型立刻可见（与系统提示同源：
    // role_roster_text）。
    let roster = role_roster_text(merged);

    let input_schema = latte_rs_agent_tools::types::ToolInputSchema {
        schema_type: SchemaType,
        properties: vec![
            ("role".into(), ToolInputProperty {
                property_type: PropertyType::String,
                description: Some(format!("Specialist role id. Available roles: {roster}")),
                enum_values: None,
                minimum: None,
                maximum: None,
                min_length: None,
                max_length: None,
            }),
            ("task".into(), ToolInputProperty {
                property_type: PropertyType::String,
                description: Some("Natural-language task for the specialist".into()),
                enum_values: None,
                minimum: None,
                maximum: None,
                min_length: None,
                max_length: None,
            }),
        ]
        .into_iter()
        .collect(),
        required: Some(vec!["role".into(), "task".into()]),
        ..Default::default()
    };

    let sem = Arc::new(Semaphore::new(8));
    let merged_owned = Arc::new(merged.clone());
    let resolver_owned = Arc::new(resolver.clone());
    let cancel_flag_owned = Arc::clone(&cancel_flag);
    let turn_cancel_flag_owned = turn_cancel_flag.clone();
    let handler: SharedToolHandler = Arc::new(move |input: serde_json::Value, _ctx| {
        let merged = Arc::clone(&merged_owned);
        let resolver = Arc::clone(&resolver_owned);
        let default_params = default_params.clone();
        let event_tx = event_tx.clone();
        let cwd = cwd.clone();
        let sem = Arc::clone(&sem);
        let cancel_flag = Arc::clone(&cancel_flag_owned);
        let turn_cancel_flag = Arc::clone(&turn_cancel_flag_owned);
        let subsession_store = subsession_store.clone();
        let session_id = session_id.clone();
        Box::pin(async move {
            let tool_err = |msg: String| latte_rs_agent_tools::error::ToolError::Other(msg);

            let role_id = input
                .get("role")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tool_err("missing 'role' field".into()))?
                .to_string();
            let task = input
                .get("task")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tool_err("missing 'task' field".into()))?
                .to_string();

            // 2. Allocate a subsession so the specialist's full event
            //    log is captured. The main chat SSE stream sees only
            //    DelegateStarted/Finished (the summary); the UI can
            //    right-click on those events and fetch the
            //    transcript via /api/sessions/.../subsessions/{sub_id}.
            let (sub_id, sub_sink) =
                subsession_store.create(session_id.as_str(), &role_id);
            let _ = event_tx.send(ChatEvent::DelegateStarted {
                from_role: "manager".into(),
                to_role: role_id.clone(),
                task: task.clone(),
                sub_id: sub_id.clone(),
            });
            // Fan out ONLY to the subsession memory sink: the
            // `sub_sink` is `Arc<dyn TraceSink>` returning from
            // `SubsessionStore::create` —— 内部已 fanout 到
            // MemorySink（实时读源）+ DiskSink（落盘备份，主
            // session 删除时联删）。这里**不要**再包一层
            // FanoutSink，否则事件会被写两份到内存。
            let template = merged
                .roles
                .get(&role_id)
                .ok_or_else(|| {
                    tool_err(format!(
                        "role '{}' not found in config. available roles: {}",
                        role_id,
                        role_roster_text(&merged)
                    ))
                })?
                .clone();
            let role = template.resolve(&default_params).await.map_err(|e| {
                tool_err(format!("failed to resolve role '{}': {}", role_id, e))
            })?;
            let tier = role.default_model_tier;
            let models = resolver
                .resolve_chain(&role.id, tier, &role.model_chain)
                .map_err(|e| {
                    tool_err(format!("no model for role '{}': {}", role_id, e))
                })?;

            // 4. Build the specialist runner. We give it a tool
            //    manager iff the role's `allowed_tools` is non-empty;
            //    otherwise the agent answers "I have no file access"
            //    (the bug we hit when the manager delegated but the
            //    programmer refused to read any code).
            let specialist_tm = if role.allowed_tools.is_empty() {
                None
            } else {
                match build_tool_manager(&role.allowed_tools).await {
                    Ok(tm) => Some(tm),
                    Err(e) => {
                        let summary = format!("tool setup for '{}' failed: {}", role_id, e);
                        let _ = event_tx.send(ChatEvent::DelegateFinished {
                            from_role: "manager".into(),
                            to_role: role_id.clone(),
                            status: "failed".into(),
                            summary: summary.clone(),
                            sub_id: sub_id.clone(),
                        });
                        return Err(tool_err(summary));
                    }
                }
            };
            let agent = Agent::new_with_chain(
                role_id.clone(),
                role.clone(),
                models,
                default_params.clone(),
            )
            .map_err(|e| {
                let summary = format!("failed to create agent for '{}': {}", role_id, e);
                let _ = event_tx.send(ChatEvent::DelegateFinished {
                    from_role: "manager".into(),
                    to_role: role_id.clone(),
                    status: "failed".into(),
                    summary: summary.clone(),
                    sub_id: sub_id.clone(),
                });
                tool_err(summary)
            })?;
            // `max_tool_rounds = 0` → unlimited; the agent decides
            // when it's done. LoopDetector in agent.rs trips on
            // actually stuck patterns.
            // Wire the workspace cwd so the specialist's path-aware
            // tools (`shell.exec`, `file.read`, …) chdir into the
            // workspace the user opened — not the Tauri process
            // cwd. See `AgentRunner::with_cwd` for the contract.
            let mut runner = match specialist_tm {
                Some(tm) => AgentRunner::new_with_tools(agent, tm, 0),
                None => AgentRunner::new(agent),
            };
            runner = runner
                .with_role(role_id.clone())
                .with_cwd(cwd.clone())
                .with_sink(sub_sink.clone());

            // Emit RoleStarted so the UI shows the specialist is working
            let task_clone = task.clone();
            let role_id_clone = role_id.clone();
            let event_tx_clone = event_tx.clone();
            let _ = event_tx_clone.send(ChatEvent::RoleStarted {
                role_id: role_id_clone.clone(),
                detail: format!("delegated: {}", task_clone),
            });

            // 5. Run the specialist. No wall-clock timeout — the
            //    subagent runs until completion or explicit cancellation
            //    via the session's cancel_flag. Check periodically.
            let _permit = sem.acquire().await.map_err(|_| {
                tool_err("delegate pool shut down".into())
            })?;
            // Move runner + messages into a spawned task so we can
            // cancel it from the select! loop. The task owns everything.
            let task_content = task.clone();
            let mut run_handle = tokio::spawn(async move {
                runner.run_turn(&[Message::user(task_content)], None).await
            });
            let result: Result<String, latte_rs_agent_tools::error::ToolError>;
            loop {
                tokio::select! {
                    r = &mut run_handle => {
                        match r {
                            Ok(Ok(response)) => { result = Ok(response); break; }
                            Ok(Err(e)) => {
                                result = Err(tool_err(format!("subagent failed: {e}")));
                                break;
                            }
                            Err(e) => {
                                result = Err(tool_err(format!("task join failed: {e}")));
                                break;
                            }
                        }
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                        if cancel_flag.load(Ordering::SeqCst) {
                            run_handle.abort();
                            let _ = event_tx.send(ChatEvent::RoleFinished {
                                role_id: role_id.clone(),
                                detail: "cancelled by user".into(),
                            });
                            let summary = String::from("delegate cancelled by user");
                            let _ = event_tx.send(ChatEvent::DelegateFinished {
                                from_role: "manager".into(),
                                to_role: role_id.clone(),
                                status: "cancelled".into(),
                                summary: summary.clone(),
                                sub_id: sub_id.clone(),
                            });
                            return Err(tool_err(summary));
                        }
                        if turn_cancel_flag.load(Ordering::SeqCst) {
                            run_handle.abort();
                            let _ = event_tx.send(ChatEvent::RoleFinished {
                                role_id: role_id.clone(),
                                detail: "cancelled by user".into(),
                            });
                            let summary = String::from("delegate cancelled by user");
                            let _ = event_tx.send(ChatEvent::DelegateFinished {
                                from_role: "manager".into(),
                                to_role: role_id.clone(),
                                status: "cancelled".into(),
                                summary: summary.clone(),
                                sub_id: sub_id.clone(),
                            });
                            return Err(tool_err(summary));
                        }
                    }
                }
            }
            let run_result = result;

            // 6. Emit specialist's RoleTurn + RoleFinished so the UI
            //    can show the subagent reply with a reference back to
            //    the DelegateStarted message (@role task).
            match &run_result {
                Ok(response) => {
                    // 成功时 sub_sink 已由 runner 的 with_sink 写入全部事件，
                    // 不再重复 emit RoleFinished —— 需要的话在 TurnEnd 里已有足够信息。
                    let _ = event_tx.send(ChatEvent::RoleTurn {
                        role_id: role_id.clone(),
                        content: response.clone(),
                        is_complete: true,
                        sub_id: Some(sub_id.clone()),
                    });
                    let _ = event_tx.send(ChatEvent::RoleFinished {
                        role_id: role_id.clone(),
                        detail: format!("ok, {} chars", response.len()),
                    });
                }
                Err(e) => {
                    // 失败时 sub_sink 中的 trace 在 TurnEnd 前就断了（agent.chat?.await
                    // 提前返回），导致右键「查看日志」看不到失败原因。
                    // 这里写入 TurnEnd 作为 trace 终点事件，让子会话日志有明确结尾。
                    sub_sink.emit(crate::trace::TraceEvent::TurnEnd {
                        meta: crate::trace::TraceMeta::now(0, &role_id, &session_id),
                        total_input: 0,
                        total_output: 0,
                        total_thinking: 0,
                        elapsed_ms: 0,
                    });
                    let _ = event_tx.send(ChatEvent::RoleFinished {
                        role_id: role_id.clone(),
                        detail: format!("error: {}", e),
                    });
                    let _ = event_tx.send(ChatEvent::Error {
                        kind: Some(crate::trace::ModelErrorKind::Other {
                            message: format!("delegate {role_id} failed: {e}"),
                        }),
                        message: format!("delegate {role_id} failed: {e}"),
                        sub_id: Some(sub_id.clone()),
                    });
                }
            }

            // 7. Emit DelegateFinished and return result.
            match run_result {
                Ok(response) => {
                    let _ = event_tx.send(ChatEvent::DelegateFinished {
                        from_role: "manager".into(),
                        to_role: role_id.clone(),
                        status: "ok".into(),
                        summary: response.clone(),
                        sub_id: sub_id.clone(),
                    });
                    Ok(serde_json::Value::String(response))
                }
                Err(e) => {
                    let summary = format!("delegate to '{}' failed: {}", role_id, e);
                    let _ = event_tx.send(ChatEvent::DelegateFinished {
                        from_role: "manager".into(),
                        to_role: role_id.clone(),
                        status: "failed".into(),
                        summary: summary.clone(),
                        sub_id: sub_id.clone(),
                    });
                    Err(tool_err(summary))
                }
            }
        })
    });

    let tool = Tool::builder(
        "delegate".to_string(),
        format!("Delegate a subtask to a specialist agent. Available roles: {roster}"),
        input_schema,
        handler,
    )
    .build();

    tm.register(tool, Some("manager"));
    Ok(())
}

/// Register the `workflow` tool: the manager can trigger a named
/// multi-role workflow (设计/plan/TDD/文档/图谱…). The handler is a thin
/// shell: parse name/topic → `load_workflow` → [`crate::workflow::run_workflow`]
/// (the engine lives in `crate::workflow` so the UI server can reuse it).
///
/// `latte-agent-core` implements its own tiny engine (see
/// `crate::workflow`) instead of reusing `latte-agent-orchestrator`,
/// because the orchestrator depends on core — a dependency cycle.
async fn register_workflow_tool(
    tm: &Arc<dyn latte_rs_agent_tools::types::ToolManager>,
    merged: &AgentConfig,
    resolver: &ModelResolver,
    default_params: GenerateParams,
    event_tx: broadcast::Sender<ChatEvent>,
    cwd: PathBuf,
    cancel_flag: Arc<AtomicBool>,
    _turn_cancel_flag: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    use latte_rs_agent_tools::types::{SchemaType, SharedToolHandler, Tool};

    // 注册时动态枚举 .latte/workflows.d（项目 + 全局）里的可用
    // workflow，写进 schema/description —— UI 管理界面新建的自定义
    // workflow 对模型立刻可见，不再靠静态样例列表。
    let available = crate::workflow::list_workflows(&cwd);
    let available_text = if available.is_empty() {
        "none found in .latte/workflows.d".to_string()
    } else {
        available
            .iter()
            .map(|(n, d)| {
                if d.is_empty() { n.clone() } else { format!("{n} — {d}") }
            })
            .collect::<Vec<_>>()
            .join("; ")
    };

    let input_schema = latte_rs_agent_tools::types::ToolInputSchema {
        schema_type: SchemaType,
        properties: vec![
            ("name".into(), ToolInputProperty {
                property_type: PropertyType::String,
                description: Some(format!("Workflow name. Available: {available_text}")),
                enum_values: None,
                minimum: None,
                maximum: None,
                min_length: None,
                max_length: None,
            }),
            ("topic".into(), ToolInputProperty {
                property_type: PropertyType::String,
                description: Some("The task/topic the workflow should work on".into()),
                enum_values: None,
                minimum: None,
                maximum: None,
                min_length: None,
                max_length: None,
            }),
        ]
        .into_iter()
        .collect(),
        required: Some(vec!["name".into(), "topic".into()]),
        ..Default::default()
    };

    let merged_owned = Arc::new(merged.clone());
    let resolver_owned = Arc::new(resolver.clone());

    let handler: SharedToolHandler = Arc::new(move |input: serde_json::Value, _ctx| {
        let merged = Arc::clone(&merged_owned);
        let resolver = Arc::clone(&resolver_owned);
        let default_params = default_params.clone();
        let event_tx = event_tx.clone();
        let cwd = cwd.clone();
        let cancel_flag = Arc::clone(&cancel_flag);
        Box::pin(async move {
            let tool_err = |msg: String| latte_rs_agent_tools::error::ToolError::Other(msg);

            let name = input
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tool_err("missing 'name' field".into()))?
                .to_string();
            let topic = input
                .get("topic")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tool_err("missing 'topic' field".into()))?
                .to_string();

            let wf = crate::workflow::load_workflow(&name, &cwd).map_err(|e| {
                let available = crate::workflow::list_workflows(&cwd)
                    .iter()
                    .map(|(n, _)| n.clone())
                    .collect::<Vec<_>>()
                    .join(", ");
                tool_err(format!("{e}. available workflows: {available}"))
            })?;

            let ctx = crate::workflow::WorkflowRunContext {
                merged,
                resolver,
                default_params,
                cwd,
                event_tx,
                cancel_flag,
            };
            crate::workflow::run_workflow(&wf, &topic, &ctx)
                .await
                .map(serde_json::Value::String)
                .map_err(tool_err)
        })
    });

    let tool = Tool::builder(
        "workflow".to_string(),
        format!("Run a named multi-role workflow. Available workflows: {available_text}"),
        input_schema,
        handler,
    )
    .build();

    tm.register(tool, Some("manager"));
    Ok(())
}

/// Try to run a workflow by slash command (e.g. "/plan xxx").
/// Returns `Ok(Some(summary))` if a workflow was found and ran,
/// `Ok(None)` if no workflow matched the command (not an error),
/// or `Err(msg)` if the workflow failed.
async fn run_workflow_command(
    cmd: &str,
    topic: &str,
    config: &ControllerConfig,
    event_tx: &broadcast::Sender<ChatEvent>,
    cancel_flag: Arc<AtomicBool>,
) -> Result<Option<String>, String> {
    let wf = match crate::workflow::load_workflow_by_command(cmd, &config.cwd) {
        Ok(w) => w,
        Err(_) => return Ok(None), // no workflow registered for this command
    };

    let ctx = crate::workflow::WorkflowRunContext {
        merged: config.agent_config.clone(),
        resolver: config.model_resolver.clone(),
        default_params: config.default_params.clone(),
        cwd: config.cwd.clone(),
        event_tx: event_tx.clone(),
        cancel_flag,
    };

    let summary = crate::workflow::run_workflow(&wf, topic, &ctx).await?;
    Ok(Some(summary))
}

fn role_icon(role_id: &str) -> String {
    match role_id {
        "manager" => "🧠",
        "programmer" => "💻",
        "architect" => "🏗️",
        "reviewer" | "reviewer_sanity" => "🔍",
        "reviewer_architecture" => "📐",
        "reviewer_security" => "🔒",
        "tester" => "🧪",
        "security" => "🛡️",
        "devops" => "⚙️",
        "designer" => "🎨",
        "tech_writer" => "📝",
        "pm" => "👔",
        _ => "🤖",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegate_hint_lists_roles_from_config() {
        let mut cfg = AgentConfig::default();
        for (id, name) in [("programmer", "Software Engineer"), ("my_custom_role", "")] {
            cfg.roles.insert(
                id.to_string(),
                crate::role::RoleTemplate {
                    id: id.into(),
                    name: name.into(),
                    category: "engineering".into(),
                    model_tier: "standard".into(),
                    model_chain: vec![],
                    prompt_file: None,
                    temperature: None,
                    tools: vec![],
                    icon: String::new(),
                    skills: vec![],
            code_paths: vec![],
                },
            );
        }
        let hint = delegate_tool_hint(&cfg);
        assert!(hint.contains("programmer(Software Engineer)"), "hint: {hint}");
        assert!(hint.contains("my_custom_role"), "hint: {hint}");
        // 花名册排序：my_custom_role 在 programmer 前
        let roster = role_roster_text(&cfg);
        assert!(roster.find("my_custom_role").unwrap() < roster.find("programmer").unwrap());
    }

    #[test]
    fn workflow_hint_lists_available_workflows() {
        let dir = tempfile::tempdir().unwrap();
        let wf_dir = dir.path().join(".latte").join("workflows.d");
        std::fs::create_dir_all(&wf_dir).unwrap();
        std::fs::write(
            wf_dir.join("my_custom.toml"),
            "name = \"my_custom\"\ndescription = \"自定义流程\"\n[[steps]]\nid = \"a\"\nspeakers = [\"tester\"]\nprompt = \"{{topic}}\"\n",
        )
        .unwrap();
        let hint = workflow_tool_hint(dir.path());
        assert!(hint.contains("`my_custom` — 自定义流程"), "hint: {hint}");
        assert!(hint.contains("分派前先想流程"), "hint: {hint}");
    }

    #[test]
    fn workflow_hint_empty_dir_degrades_gracefully() {
        // list_workflows 会回退到全局 $LATTE_HOME/workflows.d —— 用
        // ENV_LOCK + 空的 LATTE_HOME 隔离本机全局目录。
        let guard = crate::test_util::ENV_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap();
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("LATTE_HOME");
        std::env::set_var("LATTE_HOME", home.path());
        let hint = workflow_tool_hint(project.path());
        match prev {
            Some(v) => std::env::set_var("LATTE_HOME", v),
            None => std::env::remove_var("LATTE_HOME"),
        }
        drop(guard);
        assert!(hint.contains("没有可用 workflow"), "hint: {hint}");
    }

    #[test]
    fn no_steps_returns_empty_plan() {
        let plan = compute_waves::<WorkflowStepConfig>(&[]);
        assert!(plan.waves.is_empty());
    }

    #[test]
    fn single_step_one_wave() {
        let steps = vec![WorkflowStepConfig {
            id: "s1".into(),
            speakers: vec!["a".into()],
            prompt: "test".into(),
            hooks: vec![],
            contract: None,
        }];
        let plan = compute_waves(&steps);
        assert_eq!(plan.waves.len(), 1);
        assert_eq!(plan.waves[0].steps, vec![0]);
    }

    #[test]
    fn two_independent_steps_one_wave() {
        let steps = vec![
            WorkflowStepConfig {
                id: "s1".into(),
                speakers: vec!["a".into()],
                prompt: "test".into(),
                hooks: vec![],
                contract: Some(FileContractConfig {
                    input: None,
                    output: Some("out1.md".into()),
                    placeholder: None,
                    extract_prompt: None,
                    required: true,
                }),
            },
            WorkflowStepConfig {
                id: "s2".into(),
                speakers: vec!["b".into()],
                prompt: "test".into(),
                hooks: vec![],
                contract: Some(FileContractConfig {
                    input: None,
                    output: Some("out2.md".into()),
                    placeholder: None,
                    extract_prompt: None,
                    required: true,
                }),
            },
        ];
        let plan = compute_waves(&steps);
        // Two independent producers → same wave
        assert_eq!(plan.waves.len(), 1);
        assert!(plan.waves[0].steps.contains(&0));
        assert!(plan.waves[0].steps.contains(&1));
    }

    #[test]
    fn dependent_steps_two_waves() {
        let steps = vec![
            WorkflowStepConfig {
                id: "research".into(),
                speakers: vec!["arch".into()],
                prompt: "research".into(),
                hooks: vec![],
                contract: Some(FileContractConfig {
                    input: None,
                    output: Some("summary.md".into()),
                    placeholder: None,
                    extract_prompt: None,
                    required: true,
                }),
            },
            WorkflowStepConfig {
                id: "plan".into(),
                speakers: vec!["pm".into()],
                prompt: "plan: {{research_summary}}".into(),
                hooks: vec![],
                contract: Some(FileContractConfig {
                    input: Some("summary.md".into()),
                    output: Some("plan.md".into()),
                    placeholder: Some("research_summary".into()),
                    extract_prompt: None,
                    required: true,
                }),
            },
        ];
        let plan = compute_waves(&steps);
        assert_eq!(plan.waves.len(), 2, "research and plan should be in separate waves");
        assert_eq!(plan.waves[0].steps, vec![0], "wave 0 should be research");
        assert_eq!(plan.waves[1].steps, vec![1], "wave 1 should be plan");
    }

    #[test]
    fn diamond_dependency() {
        // research → review → plan
        //         → test  → plan (test also depends on research)
        let steps = vec![
            WorkflowStepConfig {
                id: "research".into(),
                speakers: vec!["r".into()],
                prompt: "r".into(),
                hooks: vec![],
                contract: Some(FileContractConfig {
                    input: None,
                    output: Some("out.md".into()),
                    placeholder: None,
                    extract_prompt: None,
                    required: true,
                }),
            },
            WorkflowStepConfig {
                id: "review".into(),
                speakers: vec!["v".into()],
                prompt: "v".into(),
                hooks: vec![],
                contract: Some(FileContractConfig {
                    input: Some("out.md".into()),
                    output: Some("review.md".into()),
                    placeholder: None,
                    extract_prompt: None,
                    required: true,
                }),
            },
            WorkflowStepConfig {
                id: "test".into(),
                speakers: vec!["t".into()],
                prompt: "t".into(),
                hooks: vec![],
                contract: Some(FileContractConfig {
                    input: Some("out.md".into()),
                    output: Some("test.md".into()),
                    placeholder: None,
                    extract_prompt: None,
                    required: true,
                }),
            },
            WorkflowStepConfig {
                id: "plan".into(),
                speakers: vec!["p".into()],
                prompt: "p".into(),
                hooks: vec![],
                contract: Some(FileContractConfig {
                    input: Some("review.md".into()),
                    output: None,
                    placeholder: None,
                    extract_prompt: None,
                    required: true,
                }),
            },
        ];
        let plan = compute_waves(&steps);
        assert_eq!(plan.waves.len(), 3, "diamond should be 3 waves");
        assert_eq!(plan.waves[0].steps, vec![0], "wave 0 should be research");
        // Wave 1: review + test (both depend on research, independent of each other)
        assert_eq!(plan.waves[1].steps.len(), 2, "wave 1 should have 2 parallel steps");
        assert!(plan.waves[1].steps.contains(&1));
        assert!(plan.waves[1].steps.contains(&2));
        assert_eq!(plan.waves[2].steps, vec![3], "wave 2 should be plan");
    }
    // ─── Delegate event schema contract ─────────────────────────
    //
    // The frontend (`src/api/chat.ts:81-82`) types the
    // `DelegateStarted` / `DelegateFinished` variants with
    // snake_case field names (`from_role`, `to_role`, `task`,
    // `status`, `summary`) and PascalCase variant tags. These
    // tests pin the serialization shape so a careless `rename_all`
    // on the enum or a field rename won't silently break the
    // CLI / latte-code-editor chat module / future CLI frontends.

    #[test]
    fn delegate_started_serializes_to_frontend_shape() {
        let event = ChatEvent::DelegateStarted {
            from_role: "manager".into(),
            to_role: "programmer".into(),
            task: "read the project structure".into(),
            sub_id: "programmer-sub-1".into(),
        };
        let json = serde_json::to_value(&event).expect("serialize");
        // PascalCase variant tag → must match the TS discriminated
        // union key in `src/api/chat.ts`.
        assert!(json.get("DelegateStarted").is_some(), "missing variant tag");
        // snake_case fields → must match the TS field names.
        assert_eq!(json["DelegateStarted"]["from_role"], "manager");
        assert_eq!(json["DelegateStarted"]["to_role"], "programmer");
        assert_eq!(
            json["DelegateStarted"]["task"],
            "read the project structure"
        );
        assert_eq!(json["DelegateStarted"]["sub_id"], "programmer-sub-1");
    }

    #[test]
    fn delegate_finished_serializes_to_frontend_shape() {
        let event = ChatEvent::DelegateFinished {
            from_role: "manager".into(),
            to_role: "programmer".into(),
            status: "ok".into(),
            summary: "found 3 files".into(),
            sub_id: "programmer-sub-1".into(),
        };
        let json = serde_json::to_value(&event).expect("serialize");
        assert!(json.get("DelegateFinished").is_some(), "missing variant tag");
        assert_eq!(json["DelegateFinished"]["from_role"], "manager");
        assert_eq!(json["DelegateFinished"]["to_role"], "programmer");
        assert_eq!(json["DelegateFinished"]["status"], "ok");
        assert_eq!(json["DelegateFinished"]["summary"], "found 3 files");
        assert_eq!(json["DelegateFinished"]["sub_id"], "programmer-sub-1");
    }
    #[test]
    fn plan_proposed_serializes_to_frontend_shape() {
        // 验证 PlanProposed 事件经 chat_event_to_frontend_json 转成
        // 前端 internally-tagged 形态：{"type":"PlanProposed",...}，
        // 字段 snake_case；tasks 里空字段被 skip_serializing_if 省略。
        let event = ChatEvent::PlanProposed {
            role_id: "manager".into(),
            plan_id: "plan-manager-0".into(),
            tasks: vec![
                PlanTask {
                    title: "实现 ringbuf 核心读写".into(),
                    description: "覆盖并发读写路径".into(),
                    priority: Some(1),
                    labels: vec!["core".into()],
                    workflow: Some("tdd_development".into()),
                    subtasks: vec![],
                },
                PlanTask {
                    title: "补测试".into(),
                    description: String::new(),
                    priority: None,
                    labels: vec![],
                    workflow: None,
                    subtasks: vec![],
                },
            ],
        };
        let wire = crate::event_json::chat_event_to_frontend_json(&event).expect("frontend json");
        let v: serde_json::Value = serde_json::from_str(&wire).expect("parse wire");
        // internally-tagged discriminator（与 api.ts 的 union key 对齐）。
        assert_eq!(v["type"], "PlanProposed");
        assert_eq!(v["role_id"], "manager");
        assert_eq!(v["plan_id"], "plan-manager-0");
        let tasks = v["tasks"].as_array().expect("tasks array");
        assert_eq!(tasks.len(), 2);
        // 第一项：全字段序列化。
        assert_eq!(tasks[0]["title"], "实现 ringbuf 核心读写");
        assert_eq!(tasks[0]["priority"], 1);
        assert_eq!(tasks[0]["workflow"], "tdd_development");
        // 第二项：空字段被 skip_serializing_if 省略（title 必留）。
        assert_eq!(tasks[1]["title"], "补测试");
        assert!(tasks[1].get("description").is_none(), "空 description 应省略");
        assert!(tasks[1].get("priority").is_none(), "None priority 应省略");
        assert!(tasks[1].get("labels").is_none(), "空 labels 应省略");
    }

    #[test]
    fn delegate_finished_carries_failure_status() {
        // The handler emits `status = "failed" | "timeout"` in error
        // paths. Pin both values so a typo here doesn't ship.
        let failed = ChatEvent::DelegateFinished {
            from_role: "manager".into(),
            to_role: "programmer".into(),
            status: "failed".into(),
            summary: "delegate to 'programmer' failed: model unavailable".into(),
            sub_id: "programmer-sub-2".into(),
        };
        let json = serde_json::to_value(&failed).unwrap();
        assert_eq!(json["DelegateFinished"]["status"], "failed");
        assert_eq!(json["DelegateFinished"]["sub_id"], "programmer-sub-2");

        let timed_out = ChatEvent::DelegateFinished {
            from_role: "manager".into(),
            to_role: "programmer".into(),
            status: "timeout".into(),
            summary: "delegate to 'programmer' timed out after 60s".into(),
            sub_id: "programmer-sub-3".into(),
        };
        let json = serde_json::to_value(&timed_out).unwrap();
        assert_eq!(json["DelegateFinished"]["status"], "timeout");
        assert_eq!(json["DelegateFinished"]["sub_id"], "programmer-sub-3");
    }

    #[test]
    fn delegate_event_roundtrip_through_workspace_wrapper() {
        // Mirrors the Tauri runtime's `WorkspaceChatEvent` wrap
        // (see `src-tauri/src/chat_panel/types.rs:8` and
        // `controller_runtime.rs:88-95`): the controller event
        // lands inside `{ workspaceId, event: <ChatEvent> }` over
        // the Tauri `chat:event` channel. Both layers must agree
        // on the shape or the frontend never sees the dispatch.
        let inner = ChatEvent::DelegateStarted {
            from_role: "manager".into(),
            to_role: "programmer".into(),
            task: "ping".into(),
            sub_id: "programmer-sub-4".into(),
        };
        let wrapped = serde_json::json!({
            "workspaceId": "ws-1",
            "event": inner,
        });
        let parsed: ChatEvent =
            serde_json::from_value(wrapped["event"].clone()).expect("parse");
        match parsed {
            ChatEvent::DelegateStarted { from_role, to_role, task, sub_id } => {
                assert_eq!(from_role, "manager");
                assert_eq!(to_role, "programmer");
                assert_eq!(task, "ping");
                assert_eq!(sub_id, "programmer-sub-4");
            }
            other => panic!("expected DelegateStarted, got {other:?}"),
        }
    }
    // ─── TimeoutWarning wire shape ─────────────────────────────
    //
    // The soft-timeout path emits this when a turn has been
    // running past `soft_timeout_secs` but the user has not yet
    // decided whether to cancel. The frontend surfaces a
    // "继续等待 / 终止" prompt; the backend's hard-kill at
    // `hard_timeout_secs` is the safety net. Pin the field names
    // so a rename here doesn't silently break the UI.
    #[test]
    fn timeout_warning_serializes_to_frontend_shape() {
        let event = ChatEvent::TimeoutWarning {
            role_id: "programmer".into(),
            elapsed_secs: 130,
            soft_timeout_secs: 120,
            hard_timeout_secs: 360,
            sub_id: None,
        };
        let json = serde_json::to_value(&event).expect("serialize");
        assert!(json.get("TimeoutWarning").is_some(), "missing variant tag");
        assert_eq!(json["TimeoutWarning"]["role_id"], "programmer");
        assert_eq!(json["TimeoutWarning"]["elapsed_secs"], 130);
        assert_eq!(json["TimeoutWarning"]["soft_timeout_secs"], 120);
        assert_eq!(json["TimeoutWarning"]["hard_timeout_secs"], 360);
        // sub_id is None → must be skipped (not serialized as null)
        // to keep wire shape lean.
        assert!(
            json["TimeoutWarning"].get("sub_id").is_none(),
            "sub_id must be skipped when None"
        );
    }

    #[test]
    fn timeout_warning_carries_sub_id_for_delegate_subsession() {
        // When a *delegate* subsession is the one over budget, the
        // UI's "查看日志" action needs the sub_id to land on the
        // subagent transcript directly.
        let event = ChatEvent::TimeoutWarning {
            role_id: "programmer".into(),
            elapsed_secs: 150,
            soft_timeout_secs: 120,
            hard_timeout_secs: 360,
            sub_id: Some("programmer-sub-99".into()),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["TimeoutWarning"]["sub_id"], "programmer-sub-99");
    }

    // ─── AdvisorHint driver plumbing ──────────────────────────────
    //
    // Driver-level test for the advisor injection channel (design
    // §7): a hint pushed while the driver is idle must (a) NOT
    // trigger a turn on its own, and (b) ride along with the next
    // user input's context as a synthetic `🦉 advisor 监察：` message.

    #[tokio::test]
    async fn advisor_hint_enters_context_without_triggering_turn() {
        use crate::config::{ModelCatalog, ModelDef};
        use crate::role::RoleTemplate;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(
                        serde_json::json!({
                            "id": "chatcmpl-test",
                            "object": "chat.completion",
                            "created": 0,
                            "model": "test",
                            "choices": [{
                                "index": 0,
                                "message": { "role": "assistant", "content": "plain reply" },
                                "finish_reason": "stop"
                            }],
                            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
                        })
                        .to_string(),
                    )),
            )
            .await;

        let agent_config = Arc::new(AgentConfig {
            models: ModelCatalog {
                models: vec![ModelDef {
                    name: "stub-standard".into(),
                    api: "openai".into(),
                    provider: "test".into(),
                    base_url: server.uri(),
                    api_key: "test-key".into(),
                    context_window: 32000,
                    max_tokens: 4096,
                    supports_thinking: false,
                    supports_vision: false,
                    supports_image_generation: false,
                    cost_per_million_input: None,
                    cost_per_million_output: None,
                    tier: Some("standard".into()),
                    timeout_secs: None,
                }],
                tiers: None,
                role_tiers: None,
            },
            roles: [(
                "manager".to_string(),
                RoleTemplate {
                    id: "manager".into(),
                    name: "manager".into(),
                    category: "planning".into(),
                    model_tier: "standard".into(),
                    model_chain: vec![],
                    prompt_file: None,
                    temperature: None,
                    tools: vec![],
                    icon: "👔".into(),
                    skills: vec![],
            code_paths: vec![],
                },
            )]
            .into_iter()
            .collect(),
        });
        let resolver = Arc::new(ModelResolver::from_config(&agent_config).unwrap());
        let dir = tempfile::tempdir().unwrap();

        let cfg = ControllerConfig {
            task_id: None,
            roles: vec!["manager".to_string()],
            initial_prompt: None,
            max_rounds: 0,
            session_token_budget: 0,
            agent_config,
            model_resolver: resolver,
            default_params: GenerateParams::default(),
            primary_model_id: None,
            initial_tier: None,
            initial_history: vec![],
            cwd: dir.path().to_path_buf(),
            subsession_store: Arc::new(crate::subsession::SubsessionStore::new()),
            advisor_monitor: AdvisorMonitorConfig::default(),
            session_id: String::new(),
        };

        async fn wait_request_count(server: &wiremock::MockServer, n: usize) {
            for _ in 0..150 {
                let got = server.received_requests().await.map(|r| r.len()).unwrap_or(0);
                if got >= n {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            panic!("timed out waiting for {n} model requests");
        }

        let controller = ChatController::new(64);
        let _rx = controller.spawn(cfg).await;

        // Turn 1 runs normally.
        controller.submit_input("第一问").await;
        wait_request_count(&server, 1).await;

        // A hint pushed while idle must not trigger a new turn.
        controller.advisor_hint("改用 src/lib.rs 路径");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "advisor hint must not trigger a turn on its own"
        );

        // The hint rides along with the next user input's context.
        controller.submit_input("第二问").await;
        wait_request_count(&server, 2).await;
        let reqs = server.received_requests().await.unwrap();
        let body2 = String::from_utf8_lossy(&reqs[1].body);
        assert!(
            body2.contains("🦉 advisor 监察"),
            "second turn's request carries the hint: {body2}"
        );
        assert!(body2.contains("改用 src/lib.rs 路径"), "hint text: {body2}");
        // …and the user question was recorded for the monitor.
        assert_eq!(controller.last_user_input(), "第二问");

        controller.abort().await;
    }
    // ─── Soft-timeout warning plumbing ───────────────────────────
    //
    // 验证 soft timeout → TimeoutWarning + turn 仍能完成的契约。
    // Mock server 2s 响应 + model.timeout_secs=1s → 第 1s 触发软
    // 警告，第 2s 拿到响应跑完；硬超时 3s 永远不到。
    #[tokio::test]
    async fn cancellable_turn_can_be_cancelled() {
        use crate::config::{ModelCatalog, ModelDef};
        use crate::role::RoleTemplate;
        use std::time::Duration;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .set_delay(Duration::from_millis(5_000))
                            .set_body_string(
                                serde_json::json!({
                                    "id": "chatcmpl-test",
                                    "object": "chat.completion",
                                    "created": 0,
                                    "model": "test",
                                    "choices": [{
                                        "index": 0,
                                        "message": { "role": "assistant", "content": "slow reply" },
                                        "finish_reason": "stop"
                                    }],
                                    "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
                                })
                                .to_string(),
                            ),
                    ),
            )
            .await;

        let agent_config = Arc::new(AgentConfig {
            models: ModelCatalog {
                models: vec![ModelDef {
                    name: "stub-slow".into(),
                    api: "openai".into(),
                    provider: "test".into(),
                    base_url: server.uri(),
                    api_key: "test-key".into(),
                    context_window: 32000,
                    max_tokens: 4096,
                    supports_thinking: false,
                    supports_vision: false,
                    supports_image_generation: false,
                    cost_per_million_input: None,
                    cost_per_million_output: None,
                    tier: Some("standard".into()),
                    timeout_secs: Some(1),
                }],
                tiers: None,
                role_tiers: None,
            },
            roles: [(
                "programmer".to_string(),
                RoleTemplate {
                    id: "programmer".into(),
                    name: "programmer".into(),
                    category: "execution".into(),
                    model_tier: "standard".into(),
                    model_chain: vec![],
                    prompt_file: None,
                    temperature: None,
                    tools: vec![],
                    icon: "💻".into(),
                    skills: vec![],
            code_paths: vec![],
                },
            )]
            .into_iter()
            .collect(),
        });
        let resolver = Arc::new(ModelResolver::from_config(&agent_config).unwrap());
        let dir = tempfile::tempdir().unwrap();

        let cfg = ControllerConfig {
            task_id: None,
            roles: vec!["programmer".to_string()],
            initial_prompt: None,
            max_rounds: 0,
            session_token_budget: 0,
            agent_config,
            model_resolver: resolver,
            default_params: GenerateParams::default(),
            primary_model_id: None,
            initial_tier: None,
            initial_history: vec![],
            cwd: dir.path().to_path_buf(),
            subsession_store: Arc::new(crate::subsession::SubsessionStore::new()),
            advisor_monitor: AdvisorMonitorConfig::default(),
            session_id: String::new(),
        };

        let controller = ChatController::new(64);
        let mut rx = controller.spawn(cfg).await;

        // 发一条慢请求（5s 响应），然后在 1s 后 cancel_turn。
        controller.submit_input("请分析这个慢请求").await;
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        controller.cancel_turn().await;

        // 收事件：期待 Error 事件描述 turn cancelled，而不是
        // 看到 "slow reply" 的 RoleTurn。
        let mut saw_cancelled = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
                Ok(Ok(ChatEvent::RoleTurn { content, .. })) => {
                    panic!("turn should be cancelled, not complete: {content}");
                }
                Ok(Ok(ChatEvent::Error { message, .. })) => {
                    if message.contains("cancelled") {
                        saw_cancelled = true;
                        break;
                    }
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => panic!("recv error: {e}"),
                Err(_) => continue,
            }
        }
        assert!(saw_cancelled, "turn must be cancelled by cancel_turn()");

        controller.abort().await;
    }

    // ─── UserMessage 事件 ─────────────────────────────────────────
    //
    // 回放里要能看到用户自己发的内容：driver 收到非斜杠 Input 时先广播
    // UserMessage 再跑 turn；斜杠命令（/roles 等）不产生该事件。

    #[tokio::test]
    async fn user_message_emitted_for_input_but_not_slash_command() {
        use crate::config::{ModelCatalog, ModelDef};
        use crate::role::RoleTemplate;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(
                        serde_json::json!({
                            "id": "chatcmpl-test",
                            "object": "chat.completion",
                            "created": 0,
                            "model": "test",
                            "choices": [{
                                "index": 0,
                                "message": { "role": "assistant", "content": "plain reply" },
                                "finish_reason": "stop"
                            }],
                            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
                        })
                        .to_string(),
                    )),
            )
            .await;

        let agent_config = Arc::new(AgentConfig {
            models: ModelCatalog {
                models: vec![ModelDef {
                    name: "stub-standard".into(),
                    api: "openai".into(),
                    provider: "test".into(),
                    base_url: server.uri(),
                    api_key: "test-key".into(),
                    context_window: 32000,
                    max_tokens: 4096,
                    supports_thinking: false,
                    supports_vision: false,
                    supports_image_generation: false,
                    cost_per_million_input: None,
                    cost_per_million_output: None,
                    tier: Some("standard".into()),
                    timeout_secs: None,
                }],
                tiers: None,
                role_tiers: None,
            },
            roles: [(
                "manager".to_string(),
                RoleTemplate {
                    id: "manager".into(),
                    name: "manager".into(),
                    category: "planning".into(),
                    model_tier: "standard".into(),
                    model_chain: vec![],
                    prompt_file: None,
                    temperature: None,
                    tools: vec![],
                    icon: "👔".into(),
                    skills: vec![],
            code_paths: vec![],
                },
            )]
            .into_iter()
            .collect(),
        });
        let resolver = Arc::new(ModelResolver::from_config(&agent_config).unwrap());
        let dir = tempfile::tempdir().unwrap();

        let cfg = ControllerConfig {
            task_id: None,
            roles: vec!["manager".to_string()],
            initial_prompt: None,
            max_rounds: 0,
            session_token_budget: 0,
            agent_config,
            model_resolver: resolver,
            default_params: GenerateParams::default(),
            primary_model_id: None,
            initial_tier: None,
            initial_history: vec![],
            cwd: dir.path().to_path_buf(),
            subsession_store: Arc::new(crate::subsession::SubsessionStore::new()),
            advisor_monitor: AdvisorMonitorConfig::default(),
            session_id: String::new(),
        };

        async fn next_event(rx: &mut tokio::sync::broadcast::Receiver<ChatEvent>) -> ChatEvent {
            tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
                .await
                .expect("event timeout")
                .expect("recv")
        }

        let controller = ChatController::new(64);
        let mut rx = controller.spawn(cfg).await;

        // 1) 普通输入 → 必须先出现 UserMessage（且 text 与输入一致），
        //    且它先于本 turn 的 RoleTurn。
        controller.submit_input("你好，世界").await;
        let mut saw_user_message = false;
        loop {
            match next_event(&mut rx).await {
                ChatEvent::UserMessage { text } => {
                    assert_eq!(text, "你好，世界");
                    saw_user_message = true;
                }
                ChatEvent::RoleTurn { .. } => break,
                _ => {}
            }
        }
        assert!(saw_user_message, "send 路径必须广播 UserMessage");

        // 2) 斜杠命令 → RoleList 返回，但绝不能出现 UserMessage。
        controller.submit_input("/roles").await;
        loop {
            match next_event(&mut rx).await {
                um @ ChatEvent::UserMessage { .. } => {
                    panic!("slash command must not emit UserMessage: {um:?}")
                }
                ChatEvent::RoleList { .. } => break,
                _ => {}
            }
        }

        controller.abort().await;
    }

    /// 锁定 bash 工具的 schema 契约：allowed "bash" 直接保留 "bash" 工具
    /// （扁平命名，无 alias），且接受 {command, cwd}（cwd 由
    /// resolve_tool_input_against_cwd 注入）。
    #[tokio::test]
    async fn bash_tool_kept_and_accepts_cwd() {
        let mgr = build_tool_manager(&["read".into(), "write".into(), "bash".into(), "search".into()])
            .await
            .expect("build_tool_manager");
        // 扁平命名：allowed "bash" 直接保留 "bash" 工具。
        let names: Vec<String> = mgr.get_tool_names();
        assert!(names.contains(&"bash".to_string()), "bash 应被保留: {names:?}");
        // bash 必须接受 {command, cwd}。
        let args = serde_json::json!({"command":"pwd","cwd":"/tmp"});
        let r = mgr.execute("bash", args, None).await;
        assert!(r.is_ok(), "bash 应接受 {{command,cwd}}，却失败: {:?}", r.err());
        // eval 不在 allowed 里，被过滤掉；扁平命名下 bash/eval 不再碰撞。
        assert!(!names.contains(&"eval".to_string()), "eval 不应被保留（不在 allowed）: {names:?}");
    }
}
