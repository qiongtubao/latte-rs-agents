//! SessionManager: per-session state machine for HIL blackboard chat.
//!
//! A `SessionRecord` is the single source of truth for one HIL session.
//! It is persisted atomically (write to `.tmp`, then rename) under
//! `<worktree>/.latte/sessions/<session_id>.json` and is human-readable
//! so operators can hand-edit it (per spec §三.3 "外科手术式回滚").
//!
//! See `docs/superpowers/specs/2026-06-28-latte-hil-blackboard-v1-design.md` §4.3.

use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// State machine for a single HIL session.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SessionState {
    Created,
    Running,
    Paused,
    Resumed,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleHistory {
    pub role_id: String,
    pub messages: Vec<latte_ai::models::Message>,
    pub last_turn: u32,
}

/// Per-session persisted record. Lives at
/// `<worktree>/.latte/sessions/<session_id>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_id: String,
    pub task_id: String,
    pub state: SessionState,
    pub plan_md: String,
    pub active_checkpoint_id: u32,
    pub current_turn: u32,
    pub roles: Vec<RoleHistory>,
    pub paused_at: Option<String>,
    pub pause_reason: Option<String>,
    pub started_at: String,
    pub updated_at: String,
}

pub struct SessionManager {
    record: SessionRecord,
    session_path: PathBuf,
    worktree_root: PathBuf,
}

impl SessionManager {
    /// Create a new SessionManager for the given task. The caller is
    /// responsible for ensuring the worktree already exists.
    pub fn new(task_id: &str, worktree_root: PathBuf, roles: Vec<String>) -> Self {
        let now = crate::trace::iso8601_utc_now();
        let session_id = format!("{}-{}", task_id, now.replace(':', "-"));
        let session_path = worktree_root
            .join(".latte")
            .join("sessions")
            .join(format!("{}.json", session_id));
        let roles = roles
            .into_iter()
            .map(|r| RoleHistory { role_id: r, messages: Vec::new(), last_turn: 0 })
            .collect();
        Self {
            record: SessionRecord {
                session_id,
                task_id: task_id.to_string(),
                state: SessionState::Created,
                plan_md: String::new(),
                active_checkpoint_id: 0,
                current_turn: 0,
                roles,
                paused_at: None,
                pause_reason: None,
                started_at: now.clone(),
                updated_at: now,
            },
            session_path,
            worktree_root,
        }
    }

    pub fn state(&self) -> SessionState { self.record.state }
    pub fn record(&self) -> &SessionRecord { &self.record }
    pub fn session_path(&self) -> &Path { &self.session_path }
    pub fn worktree_root(&self) -> &Path { &self.worktree_root }
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid state transition from {from:?} to {to:?}")]
    InvalidTransition { from: SessionState, to: SessionState },
    #[error("not a git repository: {0}")]
    NotARepo(PathBuf),
    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("role '{0}' is not part of this session")]
    UnknownRole(String),
}