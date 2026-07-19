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
use crate::trace::FanoutSink;
use crate::workspace::WorkspaceManager;
use crate::AgentResult;
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
///
/// Other frontends (e.g. `latte-agent ui`) translate the externally-
/// tagged shape into their own preferred discriminated-union form
/// (`{type:"Status",...}`) at the transport boundary — the single
/// implementation of that translation is
/// [`crate::event_json::chat_event_to_frontend_json`] (contract C2),
/// shared by the axum UI server and the Tauri adapter. Do **not** add
/// `#[serde(tag = "type")]` here — it would break the existing
/// Tauri / CLI consumers.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum ChatEvent {
    /// One role's completed turn.
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
    /// Error message.
    Error { message: String },
    /// List of available roles (response to `/roles`).
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
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RoleInfo {
    pub id: String,
    pub name: String,
    pub icon: String,
}

// ─── Controller input ────────────────────────────────────────────

enum ControllerInput {
    Input(String),
    Pause,
    Resume,
    SwitchRole(String),
    SwitchModel(ModelTier),
    Abort,
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
        let advisor_hints = self.advisor_hints.clone();

        tokio::spawn(async move {
            run_driver(config, input_rx, &event_tx, cancel_flag, pause_flag, advisor_hints).await;
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





async fn run_driver(
    config: ControllerConfig,
    mut input_rx: mpsc::UnboundedReceiver<ControllerInput>,
    event_tx: &broadcast::Sender<ChatEvent>,
    cancel_flag: Arc<AtomicBool>,
    pause_flag: Arc<AtomicBool>,
    advisor_hints: Arc<parking_lot::Mutex<std::collections::VecDeque<String>>>,
) {
    let is_multi = config.roles.len() > 1 || config.task_id.is_some();

    if is_multi {
        run_multi_role_loop(config, &mut input_rx, event_tx, cancel_flag, &*pause_flag, advisor_hints).await;
    } else {
        run_single_role_loop(config, &mut input_rx, event_tx, cancel_flag, advisor_hints).await;
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
    advisor_hints: Arc<parking_lot::Mutex<std::collections::VecDeque<String>>>,
) {
    // Resolve worktree root
    let repo_root = match WorkspaceManager::resolve_repo_root(&config.cwd) {
        Ok(root) => root,
        Err(e) => {
            let _ = event_tx.send(ChatEvent::Error {
                message: format!("无法解析仓库根目录: {e}"),
            });
            return;
        }
    };
    let task_id = match &config.task_id {
        Some(id) => id.clone(),
        None => {
            let _ = event_tx.send(ChatEvent::Error {
                message: "multi-role 模式需要 task_id".into(),
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
                    message: format!("读取 session 文件失败: {e}"),
                });
                return;
            }
        };
        let record: SessionRecord = match serde_json::from_str(&raw) {
            Ok(r) => r,
            Err(e) => {
                let _ = event_tx.send(ChatEvent::Error {
                    message: format!("解析 session JSON 失败: {e}"),
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
                    message: format!("resume 失败: {e}"),
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
                    message: format!("task '{task_id}' 无 session，需要 initial_prompt"),
                });
                return;
            }
        };
        let bb = crate::workspace::Blackboard::new(worktree_root.join("plan.md"));
        if let Err(e) = bb.write(&format!(
            "# Task: {task_id}\n\n## Initial prompt\n\n{prompt}\n"
        )) {
            let _ = event_tx.send(ChatEvent::Error {
                message: format!("写入 plan.md 失败: {e}"),
            });
            return;
        }
        if let Err(e) = session_mgr.persist() {
            let _ = event_tx.send(ChatEvent::Error {
                message: format!("持久化 session 失败: {e}"),
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
                message: format!("scheduler init error: {e}"),
            });
            return;
        }
        Err(e) => {
            let _ = event_tx.send(ChatEvent::Error {
                message: format!("scheduler join error: {e}"),
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
            cancel_flag.clone(),
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
                    message: format!("构建 role '{role_id}' runner 失败: {e}"),
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
                _ => {
                    let parts: Vec<&str> = line.splitn(2, ' ').collect();
                    let cmd = parts[0];
                    let _ = event_tx.send(ChatEvent::Status {
                        message: format!(
                            "[unknown /{cmd} — known: pause resume quit roles rounds]"
                        ),
                    });
                    continue 'rounds;
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
                Message {
                    role: MsgRole::User,
                    content: line.clone(),
                },
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
                            let synth = Message {
                                role: MsgRole::User,
                                content: format!("[INJECTED]\n{}", content),
                            };
                            mgr.append_to_role(role_id, synth).ok();
                        }
                        let _ = std::fs::remove_file(&inject_path);
                    }
                }
                // Slice plan.md for this role
                let plan_slice = plan_md_slice_for(&mgr.record().plan_md, role_id);
                if !plan_slice.is_empty() {
                    let synth = Message {
                        role: MsgRole::User,
                        content: format!("[PLAN SLICE]\n{}", plan_slice),
                    };
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

            let new_assistant_text = match runner.run_turn(&[], None).await {
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
                let assistant_msg = Message {
                    role: MsgRole::Assistant,
                    content: new_assistant_text,
                };
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
            cancel_flag.clone(),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let _ = event_tx.send(ChatEvent::Error {
                message: format!("构建 runner 失败: {e}"),
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
                                    match build_runner(merged, resolver, default_params, new_role, current_tier, current_primary.as_deref(), None, event_tx, &config.cwd, config.subsession_store.clone(), cancel_flag.clone()).await {
                                        Ok((mut new_runner, rid)) => {
                                            for m in history { new_runner.context_mut().push(m); }
                                            runner = new_runner.with_advisor_hints(advisor_hints.clone());
                                            current_role = rid.clone();
                                            let mid = runner.agent().model_chain.first().map(|mc| mc.model.id.clone()).unwrap_or_else(|| "?".into());
                                            let ico = merged.roles.get(&current_role).map(|r| r.icon.clone()).unwrap_or_else(|| role_icon(&current_role));
                                            let _ = event_tx.send(ChatEvent::Status { message: format!("Switched to role '{rid}' (tier {})", current_tier.label()) });
                                            let _ = event_tx.send(ChatEvent::Prompt { icon: ico, role_id: current_role.clone(), model_id: mid });
                                        }
                                        Err(e) => { let _ = event_tx.send(ChatEvent::Error { message: format!("switch role failed: {e}") }); }
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
                                            match build_runner(merged, resolver, default_params, &role, new_tier, current_primary.as_deref(), None, event_tx, &config.cwd, config.subsession_store.clone(), cancel_flag.clone()).await {
                                                Ok((mut new_runner, _)) => {
                                                    for m in history { new_runner.context_mut().push(m); }
                                                    runner = new_runner.with_advisor_hints(advisor_hints.clone());
                                                    current_tier = new_tier;
                                                    let _ = event_tx.send(ChatEvent::Status { message: format!("Switched to tier {}", new_tier.label()) });
                                                }
                                                Err(e) => { let _ = event_tx.send(ChatEvent::Error { message: format!("switch model failed: {e}") }); }
                                            }
                                        }
                                        Err(e) => { let _ = event_tx.send(ChatEvent::Status { message: format!("invalid tier: {e}") }); }
                                    }
                                }
                                "/history" => {
                                    let msgs = runner.context().messages();
                                    let _ = event_tx.send(ChatEvent::Status { message: format!("history ({} messages):", msgs.len()) });
                                    for (i, m) in msgs.iter().enumerate() {
                                        let preview = if m.content.len() > 200 { format!("{}...", &m.content[..200]) } else { m.content.clone() };
                                        let _ = event_tx.send(ChatEvent::Status { message: format!("  {i} [{:?}] {preview}", m.role) });
                                    }
                                }
                                _ => {
                                    let _ = event_tx.send(ChatEvent::Status { message: format!("[unknown /{cmd} — known: role model roles clear status tools history save load help exit quit]") });
                                }
                            }
                            continue;
                        }
                    
                        // Run the turn
                        let _ = event_tx.send(ChatEvent::Status { message: format!("[calling LLM for role '{current_role}'...]") });
                        let _ = event_tx.send(ChatEvent::RoleStarted {
                            role_id: current_role.clone(),
                            detail: "calling LLM".into(),
                        });
                        let usage_before = runner.total_usage().clone();
                        let turn_timeout_secs = active_model_timeout_secs(
                            resolver,
                            runner.agent().model_chain.first().map(|mc| mc.model.id.as_str()),
                            "LATTE_AGENT_TURN_TIMEOUT_SECS",
                        );
                        let turn_result = if let Some(timeout_secs) = turn_timeout_secs {
                            match tokio::time::timeout(
                                Duration::from_secs(timeout_secs),
                                runner.run_turn(&[Message { role: MsgRole::User, content: trimmed }], None),
                            ).await {
                                Ok(result) => result,
                                Err(_) => {
                                    let _ = event_tx.send(ChatEvent::RoleFinished {
                                        role_id: current_role.clone(),
                                        detail: format!("timeout after {timeout_secs}s"),
                                    });
                                    let _ = event_tx.send(ChatEvent::Error {
                                        message: format!("turn timed out after {timeout_secs}s"),
                                    });
                                    continue;
                                }
                            }
                        } else {
                            runner.run_turn(&[Message { role: MsgRole::User, content: trimmed }], None).await
                        };
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
                                let _ = event_tx.send(ChatEvent::Error { message: format!("turn failed: {e}") });
                            }
                        }
                    }
                    Some(ControllerInput::SwitchRole(new_role)) => {
                        let history: Vec<Message> = runner.context().messages().to_vec();
                        match build_runner(merged, resolver, default_params, &new_role, current_tier, current_primary.as_deref(), None, event_tx, &config.cwd, config.subsession_store.clone(), cancel_flag.clone()).await {
                            Ok((mut new_runner, rid)) => {
                                for m in history { new_runner.context_mut().push(m); }
                                runner = new_runner.with_advisor_hints(advisor_hints.clone());
                                current_role = rid.clone();
                                let mid = runner.agent().model_chain.first().map(|mc| mc.model.id.clone()).unwrap_or_else(|| "?".into());
                                let ico = merged.roles.get(&current_role).map(|r| r.icon.clone()).unwrap_or_else(|| role_icon(&current_role));
                                let _ = event_tx.send(ChatEvent::Status { message: format!("Switched to role '{rid}' (tier {})", current_tier.label()) });
                                let _ = event_tx.send(ChatEvent::Prompt { icon: ico, role_id: current_role.clone(), model_id: mid });
                            }
                            Err(e) => { let _ = event_tx.send(ChatEvent::Error { message: format!("switch role failed: {e}") }); }
                        }
                    }
                    Some(ControllerInput::SwitchModel(new_tier)) => {
                        let history: Vec<Message> = runner.context().messages().to_vec();
                        match build_runner(merged, resolver, default_params, &current_role, new_tier, current_primary.as_deref(), None, event_tx, &config.cwd, config.subsession_store.clone(), cancel_flag.clone()).await {
                            Ok((mut new_runner, _)) => {
                                for m in history { new_runner.context_mut().push(m); }
                                runner = new_runner.with_advisor_hints(advisor_hints.clone());
                                current_tier = new_tier;
                                let _ = event_tx.send(ChatEvent::Status { message: format!("Switched to tier {}", new_tier.label()) });
                            }
                            Err(e) => { let _ = event_tx.send(ChatEvent::Error { message: format!("switch model failed: {e}") }); }
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
    cancel_flag: Arc<AtomicBool>,
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
            prompt.push_str(DELEGATE_TOOL_HINT);
        }
        // Any role with "workflow" in its tools gets the workflow hint.
        if role.allowed_tools.iter().any(|t| t == "workflow") {
            prompt.push_str(WORKFLOW_TOOL_HINT);
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
            let sid = cwd.to_string_lossy().into_owned();
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
            )
            .await
            .map_err(|e| AgentError::Tool(format!("register workflow: {e}")))?;
        }
        Ok((
            AgentRunner::new_with_tools(agent, tm, 16)
                .with_role(role_id)
                .with_cwd(cwd.to_path_buf()),
            role_id.to_string(),
        ))
    } else {
        Ok((
            AgentRunner::new(agent)
                .with_role(role_id)
                .with_cwd(cwd.to_path_buf()),
            role_id.to_string(),
        ))
    }
}

async fn build_tool_manager(
    allowed: &[String],
) -> Result<Arc<dyn latte_rs_agent_tools::types::ToolManager>, Box<dyn std::error::Error + Send + Sync>> {
    use latte_rs_agent_tools::prelude::*;
    let mgr = create_tool_manager();
    for p in builtin_tool_packages() {
        mgr.register_package(p).await
            .map_err(|e| format!("register_package: {e}"))?;
    }
    let mut keep: std::collections::HashSet<String> = allowed
        .iter()
        .flat_map(|s| vec![s.to_lowercase(), s.clone()])
        .collect();
    // bash → exec alias
    for alias in &["bash"] {
        if keep.contains(*alias) || keep.contains(&alias.to_lowercase()) {
            keep.insert("exec".to_string());
        }
    }
    // mcp → mcp_* tool aliases
    if keep.contains("mcp") || keep.contains("mcp_connect") {
        keep.insert("mcp_connect".to_string());
        keep.insert("mcp_list".to_string());
        keep.insert("mcp_call".to_string());
    }
    // Register code_graph tool if allowed
    if keep.contains("code_graph") || keep.contains("code-graph") {
        let cg = code_graph_tool();
        mgr.register(cg, None);
    }
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

fn tool_usage_prompt(allowed: &[String]) -> String {
    let real_names: Vec<String> = allowed.iter()
        .map(|s| match s.as_str() {
            "bash" => "exec".to_string(),
            "playwright" => "playwright_screenshot".to_string(),
            "mcp" => "mcp_connect".to_string(),
            other => other.to_string(),
        })
        .collect();
    let names_str = real_names.join(", ");
    format!(
        r#"
## Tool calling protocol

You have access to the following tools: {names_str}.
When you call a tool you MUST output a single line of this exact XML form:

<tool_call>NAME {{"arg": "value"}}</tool_call>

Examples:

<tool_call>read {{"path": "README.md"}}</tool_call>
<tool_call>search {{"path": "src", "pattern": "TODO", "max_results": 20}}</tool_call>
"#
    )
}

const DELEGATE_TOOL_HINT: &str = r#"
### 关键规则：你必须使用 delegate 工具

你**必须**使用 `delegate` 工具来完成任务，**绝不能**直接输出计划而不执行。

正确的流程：
1. 分析任务 → 决定需要哪些专家
2. **立即**调用 delegate 工具派发任务
3. 等专家返回结果
4. 综合所有结果输出最终答案

可用专家：programmer(读代码), architect(架构), reviewer(审查), tester(测试), security(安全), designer(设计), advisor(资深顾问：失败诊断/根因分析/方案裁决)

失败升级：工具调用连续失败、专家报错且原因不明、或需要在多个方案间取舍时 → 派 advisor 诊断；诊断清楚之前不要直接回答用户，更不要重复回答旧问题。

**错误流程（禁止）：**
- 只输出计划而不调用 delegate ← 这是最常见的错误！不要这样做！
- 自己分析而不派发给专家

调用格式：<tool_call>delegate {"role": "programmer", "task": "读取 src/main.ts 的内容"}</tool_call>
"#;

const WORKFLOW_TOOL_HINT: &str = r#"
### 你还可以用 workflow 工具触发多角色工作流

当任务适合**固定的多角色流水线**时，调用 `workflow` 而不是逐个 delegate：

- 设计功能 / 方案讨论 → `feature_design`（PM → 架构 → 终审顾问）
- 实现计划 / plan → `implementation_plan`（架构 → 工程 → 终审顾问）
- TDD 开发 → `tdd_development`（测试先行 → 实现 → 验证 → 终审顾问）
- 更新文档 → `update_docs`（分析变更 → 写文档 → 终审顾问）
- 更新图谱 → `update_graph`（结构分析 → 更新图谱 → 终审顾问）

调用格式：<tool_call>workflow {"name": "feature_design", "topic": "为 UI 增加 session 管理"}</tool_call>

判断标准：
- 单点问题（读代码、改文件、审查某个具体实现）→ delegate
- 需要多个角色按固定流程协作的完整任务（设计/plan/TDD/文档/图谱）→ workflow
- workflow 会跑完整条流水线并把终审结论返回给你；你综合后再回复用户。
"#;
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
) -> Result<(), Box<dyn std::error::Error>> {
    use latte_rs_agent_tools::types::{SchemaType, SharedToolHandler, Tool};
    use tokio::sync::Semaphore;

    let input_schema = latte_rs_agent_tools::types::ToolInputSchema {
        schema_type: SchemaType,
        properties: vec![
            ("role".into(), ToolInputProperty {
                property_type: PropertyType::String,
                description: Some("Specialist role id".into()),
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

    let handler: SharedToolHandler = Arc::new(move |input: serde_json::Value, _ctx| {
        let merged = Arc::clone(&merged_owned);
        let resolver = Arc::clone(&resolver_owned);
        let default_params = default_params.clone();
        let event_tx = event_tx.clone();
        let cwd = cwd.clone();
        let sem = Arc::clone(&sem);
        let cancel_flag = Arc::clone(&cancel_flag_owned);
        let subsession_store = subsession_store.clone();
        let session_id = session_id.clone();
        Box::pin(async move {
            let tool_err = |msg: String| latte_rs_agent_tools::error::ToolError::Other(msg);

            // 1. Parse { role, task } from tool input.
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
            // specialist's tool calls are part of the subagent
            // transcript (viewable via "📋 详情" / 右键 → 查看
            // subagent 过程), not of the main chat stream.
            let subsession_fanout = FanoutSink::new(vec![
                sub_sink.clone() as Arc<dyn crate::trace::TraceSink>,
            ]);
            let template = merged
                .roles
                .get(&role_id)
                .ok_or_else(|| {
                    tool_err(format!("role '{}' not found in config", role_id))
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
                .with_sink(std::sync::Arc::new(subsession_fanout));

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
                runner.run_turn(&[Message {
                    role: MsgRole::User,
                    content: task_content,
                }], None).await
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
                    }
                }
            }
            let run_result = result;

            // 6. Emit specialist's RoleTurn + RoleFinished so the UI
            //    can show the subagent reply with a reference back to
            //    the DelegateStarted message (@role task).
            match &run_result {
                Ok(response) => {
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
                    let _ = event_tx.send(ChatEvent::RoleFinished {
                        role_id: role_id.clone(),
                        detail: format!("error: {}", e),
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
        "Delegate a subtask to a specialist agent.".to_string(),
        input_schema,
        handler,
    )
    .build();

    tm.register(tool, Some("manager"));
    Ok(())
}

/// Register the `workflow` tool: the manager can trigger a named
/// multi-role workflow (设计/plan/TDD/文档/图谱…). Each step's speakers
/// run as specialist turns with persistent per-role runners, so later
/// speakers see the earlier discussion in the same run. Progress is
/// streamed as WorkflowStarted/Step/Turn/Finished events.
///
/// `latte-agent-core` implements its own tiny engine here (see
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
) -> Result<(), Box<dyn std::error::Error>> {
    use latte_rs_agent_tools::types::{SchemaType, SharedToolHandler, Tool};

    let input_schema = latte_rs_agent_tools::types::ToolInputSchema {
        schema_type: SchemaType,
        properties: vec![
            ("name".into(), ToolInputProperty {
                property_type: PropertyType::String,
                description: Some("Workflow name, e.g. feature_design / implementation_plan / tdd_development / update_docs / update_graph".into()),
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

            let wf_id = format!(
                "wf-{}-{}",
                name,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_micros())
                    .unwrap_or(0)
            );
            let _ = event_tx.send(ChatEvent::WorkflowStarted {
                name: name.clone(),
                topic: topic.clone(),
                wf_id: wf_id.clone(),
            });

            // One runner per speaker role, persistent across steps so
            // each role sees the preceding discussion in its context.
            let mut runners: std::collections::HashMap<String, AgentRunner> =
                std::collections::HashMap::new();
            for role_id in wf.speaker_roles() {
                let template = merged
                    .roles
                    .get(&role_id)
                    .ok_or_else(|| tool_err(format!("role '{role_id}' not found in config")))?
                    .clone();
                let role = template
                    .resolve(&default_params)
                    .await
                    .map_err(|e| tool_err(format!("resolve role '{role_id}': {e}")))?;
                let tier = role.default_model_tier;
                let models = resolver
                    .resolve_chain(&role.id, tier, &role.model_chain)
                    .map_err(|e| tool_err(format!("no model for role '{role_id}': {e}")))?;
                let agent = Agent::new_with_chain(
                    role_id.clone(),
                    role.clone(),
                    models,
                    default_params.clone(),
                )
                .map_err(|e| tool_err(format!("create agent '{role_id}': {e}")))?;
                let runner = if role.allowed_tools.is_empty() {
                    AgentRunner::new(agent)
                } else {
                    let rtm = build_tool_manager(&role.allowed_tools)
                        .await
                        .map_err(|e| tool_err(format!("tools for '{role_id}': {e}")))?;
                    AgentRunner::new_with_tools(agent, rtm, 0)
                };
                runners.insert(
                    role_id.clone(),
                    runner.with_role(role_id).with_cwd(cwd.clone()),
                );
            }

            let mut vars: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            vars.insert("topic".into(), topic.clone());
            let total = wf.steps.len();
            let mut last_output = String::new();
            let mut status = "ok";
            let mut error_msg = String::new();

            'rounds: for round in 0..wf.effective_max_rounds() {
                for (idx, step) in wf.steps.iter().enumerate() {
                    if cancel_flag.load(Ordering::SeqCst) {
                        status = "cancelled";
                        break 'rounds;
                    }
                    let _ = event_tx.send(ChatEvent::WorkflowStep {
                        wf_id: wf_id.clone(),
                        step_id: step.id.clone(),
                        description: step.description.clone(),
                        index: idx + 1,
                        total,
                    });
                    let mut step_transcript = String::new();
                    for speaker in &step.speakers {
                        if cancel_flag.load(Ordering::SeqCst) {
                            status = "cancelled";
                            break 'rounds;
                        }
                        let runner = match runners.get_mut(speaker) {
                            Some(r) => r,
                            None => {
                                status = "failed";
                                error_msg = format!("role '{speaker}' not instantiated");
                                break 'rounds;
                            }
                        };
                        let mut step_vars = vars.clone();
                        step_vars.insert("step_id".into(), step.id.clone());
                        step_vars.insert("speaker".into(), speaker.clone());
                        let base_prompt = wf.render_prompt(step, &step_vars);
                        let prompt = if step_transcript.is_empty() {
                            base_prompt
                        } else {
                            format!(
                                "{base_prompt}\n\n--- Preceding discussion in this step ---\n{step_transcript}"
                            )
                        };
                        match runner
                            .run_turn(
                                &[Message {
                                    role: MsgRole::User,
                                    content: prompt,
                                }],
                                None,
                            )
                            .await
                        {
                            Ok(response) => {
                                let _ = event_tx.send(ChatEvent::WorkflowTurn {
                                    wf_id: wf_id.clone(),
                                    step_id: step.id.clone(),
                                    role_id: speaker.clone(),
                                    content: response.clone(),
                                    round,
                                });
                                step_transcript
                                    .push_str(&format!("[{speaker}]: {response}\n"));
                                last_output = response;
                            }
                            Err(e) => {
                                status = "failed";
                                error_msg =
                                    format!("step '{}' speaker '{}': {e}", step.id, speaker);
                                break 'rounds;
                            }
                        }
                    }
                    if let Some(key) = &step.output_key {
                        vars.insert(key.clone(), last_output.clone());
                    }
                }
            }

            match status {
                "ok" => {
                    let _ = event_tx.send(ChatEvent::WorkflowFinished {
                        name,
                        wf_id,
                        status: "ok".into(),
                        summary: last_output.clone(),
                    });
                    Ok(serde_json::Value::String(last_output))
                }
                s => {
                    let summary = if s == "cancelled" {
                        "workflow cancelled by user".to_string()
                    } else {
                        error_msg
                    };
                    let _ = event_tx.send(ChatEvent::WorkflowFinished {
                        name,
                        wf_id,
                        status: s.into(),
                        summary: summary.clone(),
                    });
                    Err(tool_err(summary))
                }
            }
        })
    });

    let tool = Tool::builder(
        "workflow".to_string(),
        "Run a named multi-role workflow (feature_design, implementation_plan, tdd_development, update_docs, update_graph).".to_string(),
        input_schema,
        handler,
    )
    .build();

    tm.register(tool, Some("manager"));
    Ok(())
}

fn active_model_timeout_secs(
    resolver: &ModelResolver,
    model_id: Option<&str>,
    env_var: &str,
) -> Option<u64> {
    model_id
        .and_then(|id| resolver.get_def(id))
        .and_then(|d| d.timeout_secs)
        .or_else(|| std::env::var(env_var).ok().and_then(|s| s.parse().ok()))
}

#[allow(unused_variables)]
fn register_ask_human_tool(
    tm: &Arc<dyn latte_rs_agent_tools::types::ToolManager>,
    session: Arc<Mutex<SessionManager>>,
    role_id: String,
) {
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
                    id: "stub-standard".into(),
                    name: "Stub".into(),
                    api: "openai".into(),
                    provider: "test".into(),
                    base_url: server.uri(),
                    api_key: "test-key".into(),
                    context_window: 32000,
                    max_tokens: 4096,
                    supports_thinking: false,
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
                    name: "Manager".into(),
                    category: "planning".into(),
                    model_tier: "standard".into(),
                    model_chain: vec![],
                    prompt_file: None,
                    temperature: None,
                    tools: vec![],
                    icon: "👔".into(),
                    skills: vec![],
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

}