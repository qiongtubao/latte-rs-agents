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

use crate::agent::{Agent, AgentRunner};
use crate::config::AgentConfig;
use crate::error::AgentError;
use crate::model_resolver::{ModelResolver, ModelTier};
use crate::scheduler::{plan_md_slice_for, RoundScheduler};
use crate::session::{SessionManager, SessionRecord, SessionState};
use crate::supervisor::{Supervisor, SupervisorConfig};
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
/// (`{type:"Status",...}`) at the transport boundary. Do **not** add
/// `#[serde(tag = "type")]` here — it would break the existing
/// Tauri / CLI consumers.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum ChatEvent {
    /// One role's completed turn.
    RoleTurn {
        role_id: String,
        content: String,
        is_complete: bool,
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
    DelegateStarted {
        from_role: String,
        to_role: String,
        task: String,
    },
    /// Specialist returned (or failed/timeout). `status` is one of
    /// `"ok" | "failed" | "timeout" | "cancelled"`. `summary` is the
    /// specialist's last assistant turn on success, or the error
    /// message on failure — suitable for showing in the activity
    /// stream and for the manager to consume as the tool result.
    DelegateFinished {
        from_role: String,
        to_role: String,
        status: String,
        summary: String,
    },
    /// Tool execution failed.
    ToolError {
        role_id: String,
        tool_name: String,
        error: String,
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
}

// ─── Controller ──────────────────────────────────────────────────

/// Event-driven chat session controller.
pub struct ChatController {
    input_tx: tokio::sync::Mutex<Option<mpsc::UnboundedSender<ControllerInput>>>,
    event_tx: broadcast::Sender<ChatEvent>,
    cancel_flag: Arc<AtomicBool>,
    pause_requested: Arc<AtomicBool>,
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

        tokio::spawn(async move {
            run_driver(config, input_rx, &event_tx, &cancel_flag, &pause_flag).await;
        });

        self.event_tx.subscribe()
    }

    /// Submit a line of user input.
    pub async fn submit_input(&self, text: &str) {
        if let Some(tx) = self.input_tx.lock().await.as_ref() {
            let _ = tx.send(ControllerInput::Input(text.to_string()));
        }
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
    cancel_flag: &AtomicBool,
    pause_flag: &AtomicBool,
) {
    let is_multi = config.roles.len() > 1 || config.task_id.is_some();

    if is_multi {
        run_multi_role_loop(config, &mut input_rx, event_tx, cancel_flag, pause_flag).await;
    } else {
        run_single_role_loop(config, &mut input_rx, event_tx, cancel_flag).await;
    }

    let _ = event_tx.send(ChatEvent::Done);
}

// ─── Multi-role HIL mode ─────────────────────────────────────────

async fn run_multi_role_loop(
    config: ControllerConfig,
    input_rx: &mut mpsc::UnboundedReceiver<ControllerInput>,
    event_tx: &broadcast::Sender<ChatEvent>,
    cancel_flag: &AtomicBool,
    pause_flag: &AtomicBool,
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
        )
        .await
        {
            Ok((mut runner, _canonical_id)) => {
                runner = runner.with_inject_worktree_root(worktree_root.clone());
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
    cancel_flag: &AtomicBool,
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

    let (mut runner, canonical_id) = match build_runner(
        merged,
        resolver,
        default_params,
        &role_id,
        tier,
        config.primary_model_id.as_deref(),
        None,
        event_tx,
        &config.cwd,
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
                    Some(ControllerInput::Input(text)) => {
                        let trimmed = text.trim().to_string();
                        if trimmed.is_empty() {
                            continue;
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
                                    match build_runner(merged, resolver, default_params, new_role, current_tier, current_primary.as_deref(), None, event_tx, &config.cwd).await {
                                        Ok((mut new_runner, rid)) => {
                                            for m in history { new_runner.context_mut().push(m); }
                                            runner = new_runner;
                                            current_role = rid.clone();
                                            let _ = event_tx.send(ChatEvent::Status {
                                                message: format!("Switched to role '{rid}' (tier {})", current_tier.label()),
                                            });
                                            let mid = runner.agent().model_chain.first().map(|mc| mc.model.id.clone()).unwrap_or_else(|| "?".into());
                                            let ico = merged.roles.get(&current_role).map(|r| r.icon.clone()).unwrap_or_else(|| role_icon(&current_role));
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
                                            match build_runner(merged, resolver, default_params, &role, new_tier, current_primary.as_deref(), None, event_tx, &config.cwd).await {
                                                Ok((mut new_runner, _)) => {
                                                    for m in history { new_runner.context_mut().push(m); }
                                                    runner = new_runner;
                                                    current_tier = new_tier;
                                                    let _ = event_tx.send(ChatEvent::Status { message: format!("Switched to tier {}", new_tier.label()) });
                                                }
                                                Err(e) => { let _ = event_tx.send(ChatEvent::Error { message: format!("switch model failed: {e}") }); }
                                            }
                                        }
                                        Err(e) => { let _ = event_tx.send(ChatEvent::Error { message: format!("invalid tier: {e}") }); }
                                    }
                                }
                                "/clear" => {
                                    runner.context_mut().clear();
                                    let _ = event_tx.send(ChatEvent::ContextCleared);
                                }
                                "/status" => {
                                    let _mid = runner.agent().model_chain.first().map(|mc| mc.model.id.clone()).unwrap_or_else(|| "?".into());
                                    let chain_ids: Vec<String> = runner.agent().model_chain.iter().map(|mc| mc.model.id.clone()).collect();
                                    let usage = runner.total_usage();
                                    let _ = event_tx.send(ChatEvent::Status {
                                        message: format!("role: {} | tier: {} | chain: {} | tokens: in={} out={} ctx={}",
                                            current_role, current_tier.label(), chain_ids.join(" → "),
                                            usage.input_tokens, usage.output_tokens, runner.context().messages().len()),
                                    });
                                }
                                "/tools" => {
                                    let allowed = &runner.agent().role.allowed_tools;
                                    let _ = event_tx.send(ChatEvent::Status {
                                        message: if allowed.is_empty() { "no tools configured for this role".into() } else { format!("allowed tools: {}", allowed.join(", ")) },
                                    });
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
                                let _ = event_tx.send(ChatEvent::RoleTurn { role_id: current_role.clone(), content: response.clone(), is_complete: true });
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
                        match build_runner(merged, resolver, default_params, &new_role, current_tier, current_primary.as_deref(), None, event_tx, &config.cwd).await {
                            Ok((mut new_runner, rid)) => {
                                for m in history { new_runner.context_mut().push(m); }
                                runner = new_runner;
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
                        match build_runner(merged, resolver, default_params, &current_role, new_tier, current_primary.as_deref(), None, event_tx, &config.cwd).await {
                            Ok((mut new_runner, _)) => {
                                for m in history { new_runner.context_mut().push(m); }
                                runner = new_runner;
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
                    Some(ControllerInput::Abort) | None => break,
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(3600)) => {}
        }
    }
}

// ─── Runner builder ──────────────────────────────────────────────

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

    if !role.allowed_tools.is_empty() {
        let mut prompt = tool_usage_prompt(&role.allowed_tools);
        if role_id == "manager" {
            prompt.push_str(DELEGATE_TOOL_HINT);
        }
        role.system_prompt.push_str(&prompt);
    }

    let agent = Agent::new_with_chain(role_id.to_string(), role.clone(), models, default_params.clone())?;

    if !role.allowed_tools.is_empty() {
        let tm = build_tool_manager(&role.allowed_tools).await
            .map_err(|e| AgentError::Tool(format!("build tool manager '{role_id}': {e}")))?;
        if role_id == "manager" {
            register_delegate_tool(
                &tm,
                merged,
                resolver,
                default_params.clone(),
                event_tx.clone(),
                cwd.to_path_buf(),
            )
            .await
            .map_err(|e| AgentError::Tool(format!("register delegate: {e}")))?;
        }
        if role_id != "manager" {
            if let Some(session_arc) = session {
                register_ask_human_tool(&tm, session_arc, role_id.to_string());
            }
        }
        // The runner is wired with the workspace's cwd so path-aware
        // tools (`shell.exec`, `file.read`, …) chdir into the
        // workspace the user opened — not `src-tauri/` (the Tauri
        // process cwd). See `AgentRunner::with_cwd` + the
        // `ToolExecutionContext.metadata.cwd` thread in `agent.rs`.
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
    for alias in &["bash"] {
        if keep.contains(*alias) || keep.contains(&alias.to_lowercase()) {
            keep.insert("exec".to_string());
        }
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

fn tool_usage_prompt(allowed: &[String]) -> String {
    let real_names: Vec<String> = allowed
        .iter()
        .map(|s| match s.as_str() {
            "bash" => "exec".to_string(),
            other => other.to_string(),
        })
        .collect();
    let tool_list = real_names.join(", ");
    format!(
        r#"
## Tool calling protocol

When you need to use a tool, emit EXACTLY this format on its own line
(no markdown, no code fences, no backticks — the raw markers below are
parsed verbatim by the host):

<tool_call>NAME {{"arg": "value"}}</tool_call>

Rules you MUST follow:
  1. Start with the literal text <tool_call> (no spaces, no backticks).
  2. Then the tool name and a single space, then a JSON object of args.
  3. Close with the literal text </tool_call>.
  4. Do NOT wrap the line in markdown code blocks (```...```) or indent
     it as a code block — the parser will not see the markers.
  5. You may emit multiple <tool_call> lines in one response; the host
     runs them and feeds results back as the next turn.
  6. When you have enough information to answer, respond in plain text
     with NO <tool_call> block and the loop ends.

Allowed tool names for this role: {tool_list}.

Examples (raw, copy the format exactly):

<tool_call>read {{"path": "README.md"}}</tool_call>
<tool_call>search {{"path": "src", "pattern": "TODO", "max_results": 20}}</tool_call>
"#
    )
}

const DELEGATE_TOOL_HINT: &str = r#"

### Delegating to specialists

For substantive tasks you SHOULD fan out to specialists and
synthesize. Available specialist roles:
- `programmer` — code analysis, reading code, understanding
- `architect` — module structure, dependency graph, design overview
- (plus the roles from your config)

### Layered review protocol

After you receive a specialist's report, run a layered review
before finalizing your synthesis.
"#;

async fn register_delegate_tool(
    tm: &Arc<dyn latte_rs_agent_tools::types::ToolManager>,
    merged: &AgentConfig,
    resolver: &ModelResolver,
    default_params: GenerateParams,
    event_tx: broadcast::Sender<ChatEvent>,
    cwd: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    use latte_rs_agent_tools::error::ToolError;
    use latte_rs_agent_tools::types::{SchemaType, SharedToolHandler, Tool};
    use std::time::Duration;
    use tokio::sync::Semaphore;
    use tokio::time::timeout as tokio_timeout;

    let input_schema = latte_rs_agent_tools::types::ToolInputSchema {
        schema_type: SchemaType,
        properties: vec![
            (
                "role".into(),
                ToolInputProperty {
                    property_type: PropertyType::String,
                    description: Some("Specialist role id".into()),
                    enum_values: None,
                    minimum: None,
                    maximum: None,
                    min_length: None,
                    max_length: None,
                },
            ),
            (
                "task".into(),
                ToolInputProperty {
                    property_type: PropertyType::String,
                    description: Some("Natural-language task for the specialist".into()),
                    enum_values: None,
                    minimum: None,
                    maximum: None,
                    min_length: None,
                    max_length: None,
                },
            ),
        ]
        .into_iter()
        .collect(),
        required: Some(vec!["role".into(), "task".into()]),
        ..Default::default()
    };

    // Per-process concurrency cap for parallel specialist dispatches.
    // 8 matches the CLI's `chat.rs` default — high enough to overlap
    // several reads/searches, low enough to stay under most providers'
    // per-key rate limit.
    let sem = Arc::new(Semaphore::new(8));
    // Per-specialist wall-clock timeout. Resolution order:
    //   1. `model.timeout_secs` (per-model override from models.yaml)
    //   2. LATTE_AGENT_DELEGATE_TIMEOUT_SECS env var
    //   3. 60s default
    let env_timeout_secs = std::env::var("LATTE_AGENT_DELEGATE_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok());
    const DEFAULT_DELEGATE_TIMEOUT_SECS: u64 = 60;

    // Wrap the borrowed `&AgentConfig` in an Arc so the handler
    // closure can own its own handle (`Send + 'static` requirement
    // for the boxed future the tool manager drives).
    let merged = Arc::new(merged.clone());
    let resolver = Arc::new(resolver.clone());

    let handler: SharedToolHandler = Arc::new(move |input: serde_json::Value, _ctx| {
        let merged = Arc::clone(&merged);
        let resolver = Arc::clone(&resolver);
        let default_params = default_params.clone();
        let event_tx = event_tx.clone();
        let cwd = cwd.clone();
        let sem = Arc::clone(&sem);
        let env_timeout = env_timeout_secs;
        Box::pin(async move {
            let tool_err = |msg: String| ToolError::Other(msg);

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

            // 2. Emit DelegateStarted so the UI shows the dispatch
            //    immediately — the specialist turn can take a minute
            //    and the user needs feedback it has begun.
            let _ = event_tx.send(ChatEvent::DelegateStarted {
                from_role: "manager".into(),
                to_role: role_id.clone(),
                task: task.clone(),
            });

            // 3. Resolve the specialist's role config + model chain.
            //    Mirrors the CLI's `chat.rs:1098-1114` flow.
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

            // 4. Per-specialist wall-clock timeout (model override →
            //    env → 60s default). Same precedence as the CLI.
            let timeout_s = resolver
                .get_def(&models[0].id)
                .and_then(|d| d.timeout_secs)
                .or(env_timeout)
                .unwrap_or(DEFAULT_DELEGATE_TIMEOUT_SECS);

            // 5. Build the specialist runner. We give it a tool
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
                .with_cwd(cwd.clone());

            // 6. Run the specialist turn under a wall-clock timeout,
            //    gated by a concurrency semaphore so the manager
            //    doesn't fan out unbounded parallel specialists.
            let _permit = sem.acquire().await.map_err(|_| {
                tool_err("delegate pool shut down".into())
            })?;
            let msgs = vec![Message {
                role: MsgRole::User,
                content: task.clone(),
            }];
            let run_result = tokio_timeout(
                Duration::from_secs(timeout_s),
                runner.run_turn(&msgs, None),
            )
            .await;

            // 7. Emit DelegateFinished in all terminal states and
            //    return the response (or error) to the manager as
            //    the tool result. The manager sees this on its next
            //    turn as a regular `tool_result` message.
            match run_result {
                Ok(Ok(response)) => {
                    let _ = event_tx.send(ChatEvent::DelegateFinished {
                        from_role: "manager".into(),
                        to_role: role_id.clone(),
                        status: "ok".into(),
                        summary: response.clone(),
                    });
                    // The tool result must be a `serde_json::Value`;
                    // wrap the response string. Manager sees it as
                    // the tool's return value on its next turn.
                    Ok(serde_json::Value::String(response))
                }
                Ok(Err(e)) => {
                    let summary = format!("delegate to '{}' failed: {}", role_id, e);
                    let _ = event_tx.send(ChatEvent::DelegateFinished {
                        from_role: "manager".into(),
                        to_role: role_id.clone(),
                        status: "failed".into(),
                        summary: summary.clone(),
                    });
                    Err(tool_err(summary))
                }
                Err(_elapsed) => {
                    let summary = format!(
                        "delegate to '{}' timed out after {}s",
                        role_id, timeout_s
                    );
                    let _ = event_tx.send(ChatEvent::DelegateFinished {
                        from_role: "manager".into(),
                        to_role: role_id.clone(),
                        status: "timeout".into(),
                        summary: summary.clone(),
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
    }

    #[test]
    fn delegate_finished_serializes_to_frontend_shape() {
        let event = ChatEvent::DelegateFinished {
            from_role: "manager".into(),
            to_role: "programmer".into(),
            status: "ok".into(),
            summary: "found 3 files".into(),
        };
        let json = serde_json::to_value(&event).expect("serialize");
        assert!(json.get("DelegateFinished").is_some(), "missing variant tag");
        assert_eq!(json["DelegateFinished"]["from_role"], "manager");
        assert_eq!(json["DelegateFinished"]["to_role"], "programmer");
        assert_eq!(json["DelegateFinished"]["status"], "ok");
        assert_eq!(json["DelegateFinished"]["summary"], "found 3 files");
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
        };
        let json = serde_json::to_value(&failed).unwrap();
        assert_eq!(json["DelegateFinished"]["status"], "failed");

        let timed_out = ChatEvent::DelegateFinished {
            from_role: "manager".into(),
            to_role: "programmer".into(),
            status: "timeout".into(),
            summary: "delegate to 'programmer' timed out after 60s".into(),
        };
        let json = serde_json::to_value(&timed_out).unwrap();
        assert_eq!(json["DelegateFinished"]["status"], "timeout");
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
        };
        // `WorkspaceChatEvent` is `pub` re-exported in `super` —
        // construct it inline to validate the public serialization
        // surface that the Tauri runtime emits.
        let wrapped = serde_json::json!({
            "workspaceId": "ws-1",
            "event": inner,
        });
        let parsed: ChatEvent =
            serde_json::from_value(wrapped["event"].clone()).expect("parse");
        match parsed {
            ChatEvent::DelegateStarted { from_role, to_role, task } => {
                assert_eq!(from_role, "manager");
                assert_eq!(to_role, "programmer");
                assert_eq!(task, "ping");
            }
            other => panic!("expected DelegateStarted, got {other:?}"),
        }
    }

}