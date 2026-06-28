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

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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

// ─── Events ──────────────────────────────────────────────────────

/// Event emitted by the ChatController driver loop.
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
        // Replace the channel — take the old sender created by `new()`.
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

            let new_assistant_text = match runner.run_turn(&[], None).await {
                Ok(text) => text,
                Err(e) => {
                    let _ = event_tx.send(ChatEvent::Status {
                        message: format!("[{role_id} round {round_num}: error: {e}]"),
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

            // Check supervisor
            let decision_kind = runner.last_decision_kind();
            let pause_reason = scheduler
                .supervisor
                .observe(role_id, 100, &decision_kind);
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

    let mut current_role = canonical_id;
    let mut current_tier = tier;
    let mut current_primary = config.primary_model_id.clone();

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
                                    match build_runner(merged, resolver, default_params, new_role, current_tier, current_primary.as_deref(), None).await {
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
                                            match build_runner(merged, resolver, default_params, &role, new_tier, current_primary.as_deref(), None).await {
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
                                    let mid = runner.agent().model_chain.first().map(|mc| mc.model.id.clone()).unwrap_or_else(|| "?".into());
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
                        let usage_before = runner.total_usage().clone();
                        match runner.run_turn(&[Message { role: MsgRole::User, content: trimmed }], None).await {
                            Ok(response) => {
                                let mid = runner.agent().model_chain.first().map(|mc| mc.model.id.clone()).unwrap_or_else(|| "?".into());
                                let ico = merged.roles.get(&current_role).map(|r| r.icon.clone()).unwrap_or_else(|| role_icon(&current_role));
                                let usage_after = runner.total_usage();
                                let in_delta = usage_after.input_tokens - usage_before.input_tokens;
                                let out_delta = usage_after.output_tokens - usage_before.output_tokens;
                                let _ = event_tx.send(ChatEvent::RoleTurn { role_id: current_role.clone(), content: response.clone(), is_complete: true });
                                let _ = event_tx.send(ChatEvent::Status { message: format!("[{current_role} · {mid} · tokens: +{in_delta} in / +{out_delta} out]") });
                                let _ = event_tx.send(ChatEvent::Prompt { icon: ico, role_id: current_role.clone(), model_id: mid });
                            }
                            Err(e) => { let _ = event_tx.send(ChatEvent::Error { message: format!("turn failed: {e}") }); }
                        }
                    }
                    Some(ControllerInput::SwitchRole(new_role)) => {
                        let history: Vec<Message> = runner.context().messages().to_vec();
                        match build_runner(merged, resolver, default_params, &new_role, current_tier, current_primary.as_deref(), None).await {
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
                        match build_runner(merged, resolver, default_params, &current_role, new_tier, current_primary.as_deref(), None).await {
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
            register_delegate_tool(&tm, merged, resolver, default_params.clone())
                .await
                .map_err(|e| AgentError::Tool(format!("register delegate: {e}")))?;
        }
        if role_id != "manager" {
            if let Some(session_arc) = session {
                register_ask_human_tool(&tm, session_arc, role_id.to_string());
            }
        }
        Ok((AgentRunner::new_with_tools(agent, tm, 16)
            .with_role(role_id), role_id.to_string()))
    } else {
        Ok((AgentRunner::new(agent)
            .with_role(role_id), role_id.to_string()))
    }
}

// ─── Tool helpers ────────────────────────────────────────────────

async fn build_tool_manager(
    allowed: &[String],
) -> Result<Arc<dyn latte_rs_agent_tools::types::ToolManager>, Box<dyn std::error::Error>> {
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
    _merged: &AgentConfig,
    _resolver: &ModelResolver,
    _default_params: GenerateParams,
) -> Result<(), Box<dyn std::error::Error>> {
    use latte_rs_agent_tools::types::{SchemaType, Tool};

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

    let handler: latte_rs_agent_tools::types::SharedToolHandler =
        Arc::new(move |_input: serde_json::Value, _ctx| {
            Box::pin(async move {
                Err(latte_rs_agent_tools::error::ToolError::Other(
                    "delegate tool not fully implemented in controller mode yet".into(),
                ))
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