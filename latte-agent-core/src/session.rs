//! SessionManager: per-session state machine for HIL blackboard chat.
//!
//! A `SessionRecord` is the single source of truth for one HIL session.
//! It is persisted atomically (write to `.tmp`, then rename) under
//! `<worktree>/.latte/sessions/<session_id>.json` and is human-readable
//! so operators can hand-edit it (per spec §三.3 "外科手术式回滚").
//!
//! See `docs/superpowers/specs/2026-06-28-latte-hil-blackboard-v1-design.md` §4.3.

use std::path::{Path, PathBuf};
use std::sync::Arc;
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
    /// Optional sink for trace emission. Added in v1.1 so the
    /// scheduler can emit AskHuman / RoundStarted / RoundEnded events.
    /// When None, the emit_* methods are no-ops.
    sink: Option<Arc<dyn crate::trace::TraceSink>>,
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
            sink: None,
        }
    }

    pub fn state(&self) -> SessionState { self.record.state }
    pub fn record(&self) -> &SessionRecord { &self.record }
    pub fn session_path(&self) -> &Path { &self.session_path }
    pub fn worktree_root(&self) -> &Path { &self.worktree_root }

    /// Persist the current record to `<session_path>` atomically
    /// (write to `.tmp`, rename). Updates `updated_at`.
    pub fn persist(&self) -> Result<(), SessionError> {
        let mut record = self.record.clone();
        record.updated_at = crate::trace::iso8601_utc_now();
        let json = serde_json::to_string_pretty(&record)?;
        let tmp = self.session_path.with_extension("json.tmp");
        if let Some(parent) = tmp.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &self.session_path)?;
        Ok(())
    }

    /// Transition to Paused. Sets `paused_at` + `pause_reason`, persists.
    pub fn pause(&mut self, reason: &str) -> Result<(), SessionError> {
        self.transition(SessionState::Paused)?;
        self.record.paused_at = Some(crate::trace::iso8601_utc_now());
        self.record.pause_reason = Some(reason.to_string());
        self.persist()
    }

    /// Transition to Resumed (only legal from Paused). Clears
    /// `paused_at`, persists.
    pub fn resume(&mut self) -> Result<(), SessionError> {
        self.transition(SessionState::Resumed)?;
        self.record.paused_at = None;
        self.record.pause_reason = None;
        self.persist()
    }
    /// Set pause_reason and paused_at, transition to Paused, persist.
    /// Same logic as `pause` but takes a fully-formed reason (no
    /// internal "user /pause" prefix). Used by the supervisor and by
    /// ask_human.
    pub fn pause_with_reason(&mut self, reason: &str) -> Result<(), SessionError> {
        self.transition(SessionState::Paused)?;
        self.record.paused_at = Some(crate::trace::iso8601_utc_now());
        self.record.pause_reason = Some(reason.to_string());
        self.persist()
    }

    /// Resume from Paused, append a synthetic user message to the
    /// named role's history, and persist. The role's next turn will
    /// see the human's reply in its history.
    pub fn resume_with_message(
        &mut self,
        role_id: &str,
        message: &str,
    ) -> Result<(), SessionError> {
        self.transition(SessionState::Resumed)?;
        self.record.paused_at = None;
        self.record.pause_reason = None;
        let now = crate::trace::iso8601_utc_now();
        let synthetic = latte_ai::models::Message {
            role: latte_ai::models::Role::User,
            content: format!("[HUMAN @ {}]\n{}", now, message),
        };
        self.append_to_role(role_id, synthetic)?;
        Ok(())
    }

    pub fn mark_done(&mut self) -> Result<(), SessionError> {
        self.transition(SessionState::Done)?;
        self.persist()
    }

    pub fn mark_failed(&mut self, reason: &str) -> Result<(), SessionError> {
        self.transition(SessionState::Failed)?;
        self.record.pause_reason = Some(reason.to_string());
        self.persist()
    }

    /// Append a message to a role's history. Persists.
    pub fn append_to_role(&mut self, role_id: &str, msg: latte_ai::models::Message) -> Result<(), SessionError> {
        let role = self.record.roles.iter_mut()
            .find(|r| r.role_id == role_id)
            .ok_or_else(|| SessionError::UnknownRole(role_id.to_string()))?;
        role.messages.push(msg);
        self.persist()
    }

    /// Read a role's full history (empty Vec if role unknown).
    pub fn role_history(&self, role_id: &str) -> Vec<latte_ai::models::Message> {
        self.record.roles.iter()
            .find(|r| r.role_id == role_id)
            .map(|r| r.messages.clone())
            .unwrap_or_default()
    }

    /// Bump the current turn counter and persist.
    pub fn advance_turn(&mut self) -> Result<(), SessionError> {
        self.record.current_turn += 1;
        self.persist()
    }

    /// Re-hydrate a SessionManager from an existing record + worktree
    /// root (typically loaded from the on-disk JSON).
    pub fn from_record(record: SessionRecord, worktree_root: PathBuf) -> Self {
        let session_path = worktree_root
            .join(".latte")
            .join("sessions")
            .join(format!("{}.json", record.session_id));
        Self { record, session_path, worktree_root, sink: None }
    }

    /// Attach a trace sink. Used by the REPL driver in Phase 7 to
    /// emit session lifecycle events.
    pub fn with_sink(mut self, sink: Arc<dyn crate::trace::TraceSink>) -> Self {
        self.sink = Some(sink);
        self
    }

    pub fn sink(&self) -> Option<&Arc<dyn crate::trace::TraceSink>> {
        self.sink.as_ref()
    }

    /// Append a TraceEvent::AskHuman to the sink. No-op if no sink
    /// is attached (which is the case in unit tests and in v1 callers).
    pub fn emit_ask_human_event(&self, role: &str, question: &str) {
        if let Some(sink) = &self.sink {
            use crate::trace::{TraceEvent, TraceMeta};
            let meta = TraceMeta {
                turn: 0,
                role: role.to_string(),
                ts: crate::trace::iso8601_utc_now(),
                session_id: self.record.session_id.clone(),
            };
            sink.emit(TraceEvent::AskHuman {
                meta,
                task_id: self.record.task_id.clone(),
                role: role.to_string(),
                question: question.to_string(),
            });
        }
    }

    /// Emit RoundStarted to the sink. No-op if no sink.
    pub fn emit_round_started(&self, round: u32, roles: &[String]) {
        if let Some(sink) = &self.sink {
            use crate::trace::{TraceEvent, TraceMeta};
            let meta = TraceMeta {
                turn: 0,
                role: "scheduler".to_string(),
                ts: crate::trace::iso8601_utc_now(),
                session_id: self.record.session_id.clone(),
            };
            sink.emit(TraceEvent::RoundStarted {
                meta,
                task_id: self.record.task_id.clone(),
                round,
                roles: roles.to_vec(),
            });
        }
    }

    /// Emit RoundEnded to the sink. No-op if no sink.
    pub fn emit_round_ended(&self, round: u32) {
        if let Some(sink) = &self.sink {
            use crate::trace::{TraceEvent, TraceMeta};
            let meta = TraceMeta {
                turn: 0,
                role: "scheduler".to_string(),
                ts: crate::trace::iso8601_utc_now(),
                session_id: self.record.session_id.clone(),
            };
            sink.emit(TraceEvent::RoundEnded {
                meta,
                task_id: self.record.task_id.clone(),
                round,
            });
        }
    }

    /// Check that the target state is a legal next state, update
    /// `state`, and return Ok(()). Otherwise return
    /// `SessionError::InvalidTransition`.
    fn transition(&mut self, to: SessionState) -> Result<(), SessionError> {
        use SessionState::*;
        let from = self.record.state;
        let legal = matches!(
            (from, to),
            (Created, Running)
            | (Created, Paused)
            | (Created, Done)
            | (Running, Paused)
            | (Paused, Resumed)
            | (Resumed, Running)
            | (Running, Done)
            | (Resumed, Done)
            | (_, Failed)
        );
        if !legal {
            return Err(SessionError::InvalidTransition { from, to });
        }
        self.record.state = to;
        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use latte_ai::models::{Message, Role as MsgRole};

    fn make_mgr() -> (tempfile::TempDir, SessionManager) {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new(
            "test-task",
            dir.path().to_path_buf(),
            vec!["manager".into(), "programmer".into()],
        );
        (dir, mgr)
    }

    #[test]
    fn new_initializes_state_to_created() {
        let (_dir, mgr) = make_mgr();
        assert_eq!(mgr.state(), SessionState::Created);
        assert_eq!(mgr.record().task_id, "test-task");
        assert_eq!(mgr.record().roles.len(), 2);
        assert_eq!(mgr.record().roles[0].role_id, "manager");
    }

    #[test]
    fn persist_writes_atomic_json() {
        let (_dir, mgr) = make_mgr();
        mgr.persist().unwrap();
        let path = mgr.session_path();
        assert!(path.exists(), "session file should exist at {}", path.display());
        let tmp = path.with_extension("json.tmp");
        assert!(!tmp.exists(), "no .tmp leftover allowed");
        let raw = std::fs::read_to_string(path).unwrap();
        let parsed: SessionRecord = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.task_id, "test-task");
        assert_eq!(parsed.state, SessionState::Created);
    }

    #[test]
    fn pause_then_persist_round_trips() {
        let (_dir, mut mgr) = make_mgr();
        mgr.pause("test reason").unwrap();
        assert_eq!(mgr.state(), SessionState::Paused);
        mgr.persist().unwrap();
        let raw = std::fs::read_to_string(mgr.session_path()).unwrap();
        let parsed: SessionRecord = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.state, SessionState::Paused);
        assert_eq!(parsed.pause_reason.as_deref(), Some("test reason"));
        assert!(parsed.paused_at.is_some());
    }

    #[test]
    fn resume_from_paused_is_legal() {
        let (_dir, mut mgr) = make_mgr();
        mgr.pause("r").unwrap();
        mgr.resume().unwrap();
        assert_eq!(mgr.state(), SessionState::Resumed);
    }

    #[test]
    fn invalid_transition_resume_from_done() {
        let (_dir, mut mgr) = make_mgr();
        mgr.mark_done().unwrap();
        let res = mgr.resume();
        assert!(matches!(res, Err(SessionError::InvalidTransition { .. })));
    }

    #[test]
    fn append_to_role_grows_history() {
        let (_dir, mut mgr) = make_mgr();
        mgr.append_to_role("programmer", Message {
            role: MsgRole::User,
            content: "hello".into(),
        }).unwrap();
        let history = mgr.role_history("programmer");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].content, "hello");
    }

    #[test]
    fn unknown_role_returns_error() {
        let (_dir, mut mgr) = make_mgr();
        let res = mgr.append_to_role("ghost", Message {
            role: MsgRole::User,
            content: "x".into(),
        });
        assert!(matches!(res, Err(SessionError::UnknownRole(_))));
    }

    #[test]
    fn advance_turn_increments_counter() {
        let (_dir, mut mgr) = make_mgr();
        mgr.advance_turn().unwrap();
        assert_eq!(mgr.record().current_turn, 1);
        mgr.advance_turn().unwrap();
        assert_eq!(mgr.record().current_turn, 2);
    }
    #[test]
    fn pause_with_reason_sets_paused_at_and_reason() {
        let (_dir, mut mgr) = make_mgr();
        mgr.pause_with_reason("token budget exceeded (50001 / 50000)").unwrap();
        assert_eq!(mgr.state(), SessionState::Paused);
        assert_eq!(
            mgr.record().pause_reason.as_deref(),
            Some("token budget exceeded (50001 / 50000)")
        );
        assert!(mgr.record().paused_at.is_some());
    }

    #[test]
    fn resume_with_message_appends_synthetic_user_message() {
        let (_dir, mut mgr) = make_mgr();
        mgr.pause_with_reason("ask_human: programmer asked: how should I parse the CSV?").unwrap();
        mgr.resume_with_message("programmer", "use serde").unwrap();
        assert_eq!(mgr.state(), SessionState::Resumed);
        assert!(mgr.record().paused_at.is_none());
        let history = mgr.role_history("programmer");
        assert!(history.last().unwrap().content.contains("[HUMAN @"));
        assert!(history.last().unwrap().content.contains("use serde"));
    }
}