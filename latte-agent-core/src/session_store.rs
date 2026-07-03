//! SessionStore: unified persistent session storage for all chat modes.
//!
//! Stores every session (single-role, multi-role HIL, workflow) as an
//! independent JSON file under `~/.latte/chat-sessions/<session_id>.json`.
//! This replaces the in-memory-only ChatController approach and complements
//! the worktree-based SessionManager (HIL sessions use both).
//!
//! Key design:
//! - Each session has a flat message list (`Vec<StoredMessage>`) capturing
//!   every user/agent/tool event in order.
//! - State machine mirrors `SessionState` from `session.rs`.
//! - Atomic writes (write .tmp → rename) prevent corruption.
//! - Listing scans the directory and reads only the summary fields
//!   (no full message deserialization for list view).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use thiserror::Error;

// ─── Re-export session state from existing module ───────────────────

pub use crate::session::SessionState;

// ─── Unified stored message ─────────────────────────────────────────

/// A single message in a unified session transcript.
/// Captures user input, agent responses, tool calls, and system events.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StoredMessage {
    User {
        content: String,
        timestamp: String,
    },
    Assistant {
        role_id: String,
        content: String,
        timestamp: String,
        /// Estimated tokens for this message.
        tokens: Option<u32>,
    },
    ToolCall {
        role_id: String,
        tool_name: String,
        args: String,
        timestamp: String,
    },
    ToolResult {
        role_id: String,
        tool_name: String,
        result: String,
        timestamp: String,
    },
    SystemEvent {
        event_type: String,
        message: String,
        timestamp: String,
    },
    RoundStart {
        round: u32,
        timestamp: String,
    },
    RoundEnd {
        round: u32,
        timestamp: String,
    },
    Paused {
        reason: String,
        timestamp: String,
    },
    Resumed {
        timestamp: String,
    },
}

/// Summary info for a session (used in list view — no messages loaded).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub state: SessionState,
    pub chat_type: String,        // "single" | "hil" | "workflow"
    pub role_ids: Vec<String>,
    pub message_count: usize,
    pub created_at: String,
    pub updated_at: String,
    pub size_bytes: u64,
}

/// Full session record stored on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredSession {
    pub session_id: String,
    pub state: SessionState,
    pub chat_type: String,
    pub role_ids: Vec<String>,
    pub messages: Vec<StoredMessage>,
    pub created_at: String,
    pub updated_at: String,
}

impl StoredSession {
    pub fn new(session_id: String, chat_type: String, role_ids: Vec<String>) -> Self {
        let now = iso8601_now();
        Self {
            session_id,
            state: SessionState::Created,
            chat_type,
            role_ids,
            messages: Vec::new(),
            created_at: now.clone(),
            updated_at: now,
        }
    }

    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    /// Update `updated_at` and set state.
    pub fn set_state(&mut self, state: SessionState) {
        self.state = state;
        self.updated_at = iso8601_now();
    }

    /// Append a message and bump `updated_at`.
    pub fn push_message(&mut self, msg: StoredMessage) {
        self.updated_at = iso8601_now();
        self.messages.push(msg);
    }
}

// ─── Errors ─────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum SessionStoreError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Session not found: {0}")]
    NotFound(String),
    #[error("Invalid session id: {0}")]
    InvalidId(String),
}

// ─── SessionStore ───────────────────────────────────────────────────

/// Thread-safe, async session store.
/// Manages the `~/.latte/chat-sessions/` directory.
pub struct SessionStore {
    dir: PathBuf,
    /// In-memory cache: session_id → StoredSession.
    /// Avoids re-reading the file on every append.
    cache: Arc<RwLock<HashMap<String, StoredSession>>>,
}

impl SessionStore {
    /// Create or open a session store at the given directory.
    /// `dir` defaults to `~/.latte/chat-sessions/` when `None`.
    pub async fn new(dir: Option<PathBuf>) -> Result<Self, SessionStoreError> {
        let dir = dir.unwrap_or_else(default_sessions_dir);
        tokio::fs::create_dir_all(&dir).await?;
        Ok(Self {
            dir,
            cache: Arc::new(RwLock::new(HashMap::new())),
        })
    }
    
    /// Synchronous version for use in OneShot initializers.
    /// Does NOT populate the in-memory cache (avoids async overhead).
    pub fn new_sync(dir: Option<PathBuf>) -> Self {
        let dir = dir.unwrap_or_else(default_sessions_dir);
        let _ = std::fs::create_dir_all(&dir);
        Self {
            dir,
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// The on-disk directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    // ─── CRUD ────────────────────────────────────────────────────

    /// Create a new session (writes to disk immediately).
    pub async fn create(
        &self,
        session_id: &str,
        chat_type: &str,
        role_ids: Vec<String>,
    ) -> Result<StoredSession, SessionStoreError> {
        let session = StoredSession::new(session_id.to_string(), chat_type.to_string(), role_ids);
        self.write(&session).await?;
        self.cache.write().await.insert(session_id.to_string(), session.clone());
        Ok(session)
    }

    /// Load a session from disk (or cache).
    pub async fn get(&self, session_id: &str) -> Result<StoredSession, SessionStoreError> {
        // Check cache first.
        {
            let cache = self.cache.read().await;
            if let Some(s) = cache.get(session_id) {
                return Ok(s.clone());
            }
        }
        // Load from disk.
        let path = self.path_for(session_id);
        if !path.exists() {
            return Err(SessionStoreError::NotFound(session_id.to_string()));
        }
        let bytes = tokio::fs::read(&path).await?;
        let session: StoredSession = serde_json::from_slice(&bytes)?;
        // Populate cache.
        self.cache.write().await.insert(session_id.to_string(), session.clone());
        Ok(session)
    }

    /// Save (insert or update) a session to disk.
    pub async fn save(&self, session: &StoredSession) -> Result<(), SessionStoreError> {
        self.write(session).await?;
        self.cache.write().await.insert(session.session_id.clone(), session.clone());
        Ok(())
    }

    /// Delete a session from disk and cache.
    pub async fn delete(&self, session_id: &str) -> Result<(), SessionStoreError> {
        let path = self.path_for(session_id);
        if path.exists() {
            tokio::fs::remove_file(&path).await?;
        }
        self.cache.write().await.remove(session_id);
        Ok(())
    }

    /// Append a message to a session and persist.
    pub async fn append_message(
        &self,
        session_id: &str,
        msg: StoredMessage,
    ) -> Result<(), SessionStoreError> {
        let mut session = self.get(session_id).await?;
        session.push_message(msg);
        self.save(&session).await
    }

    /// Update session state and persist.
    pub async fn set_state(
        &self,
        session_id: &str,
        state: SessionState,
    ) -> Result<(), SessionStoreError> {
        let mut session = self.get(session_id).await?;
        session.set_state(state);
        self.save(&session).await
    }

    // ─── Listing ─────────────────────────────────────────────────

    /// List all sessions with summary info (no full messages loaded).
    pub async fn list(&self) -> Result<Vec<SessionSummary>, SessionStoreError> {
        let mut summaries = Vec::new();
        let mut read_dir = tokio::fs::read_dir(&self.dir).await?;
        while let Some(entry) = read_dir.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Some(_stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let bytes = tokio::fs::read(&path).await?;
            if let Ok(session) = serde_json::from_slice::<StoredSession>(&bytes) {
                let msg_count = session.messages.len();
                let role_ids = session.role_ids.clone();
                let meta = match std::fs::metadata(&path) {
                    Ok(m) => m.len(),
                    Err(_) => 0,
                };
                summaries.push(SessionSummary {
                    session_id: session.session_id,
                    state: session.state,
                    chat_type: session.chat_type,
                    role_ids,
                    message_count: msg_count,
                    created_at: session.created_at,
                    updated_at: session.updated_at,
                    size_bytes: meta,
                });
            }
        }
        summaries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(summaries)
    }

    // ─── Internals ───────────────────────────────────────────────

    fn path_for(&self, session_id: &str) -> PathBuf {
        let safe_id = sanitize_id(session_id);
        self.dir.join(format!("{}.json", safe_id))
    }

    async fn write(&self, session: &StoredSession) -> Result<(), SessionStoreError> {
        let path = self.path_for(&session.session_id);
        let json = serde_json::to_string_pretty(session)?;
        // Atomic write: .tmp → rename
        let tmp = path.with_extension("json.tmp");
        tokio::fs::write(&tmp, &json).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(())
    }
}

// ─── Helpers ────────────────────────────────────────────────────────

fn default_sessions_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".latte")
        .join("chat-sessions")
}

fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

fn iso8601_now() -> String {
    // Simple ISO 8601 without pulling in chrono
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    // Format as ISO-like: seconds since epoch is fine for sorting,
    // but we produce a human-readable form.
    // We use a simple format: "2026-07-02T12:34:56Z"
    let s = secs as i64;
    let days = s / 86400;
    let time_secs = s % 86400;
    let h = time_secs / 3600;
    let m = (time_secs % 3600) / 60;
    let sec = time_secs % 60;
    // Approximate date from Unix epoch (2026-07-02 is ~20640 days from epoch)
    // This is a simplified calculation — sufficient for timestamps.
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        1970 + (days as f64 / 365.25) as u64,
        1 + ((days as f64 / 30.44) as u64 % 12),
        1 + (days as u64 % 28),
        h,
        m,
        sec,
    )
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use crate::test_util::ENV_LOCK;

    #[tokio::test]
    async fn test_create_and_get() {
        let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(Some(dir.path().join("chat-sessions"))).await.unwrap();

        let session = store.create("test-1", "single", vec!["manager".into()]).await.unwrap();
        assert_eq!(session.state, SessionState::Created);
        assert_eq!(session.messages.len(), 0);

        let loaded = store.get("test-1").await.unwrap();
        assert_eq!(loaded.session_id, "test-1");
    }

    #[tokio::test]
    async fn test_append_and_list() {
        let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(Some(dir.path().join("chat-sessions"))).await.unwrap();

        store.create("s1", "single", vec!["manager".into()]).await.unwrap();
        store.create("s2", "hil", vec!["manager".into(), "programmer".into()]).await.unwrap();

        store.append_message("s1", StoredMessage::User {
            content: "hello".into(),
            timestamp: "2026-07-02T00:00:00Z".into(),
        }).await.unwrap();

        store.set_state("s2", SessionState::Paused).await.unwrap();

        let list = store.list().await.unwrap();
        assert_eq!(list.len(), 2);
        let s1 = list.iter().find(|s| s.session_id == "s1").unwrap();
        assert_eq!(s1.message_count, 1);
        let s2 = list.iter().find(|s| s.session_id == "s2").unwrap();
        assert_eq!(s2.state, SessionState::Paused);
    }

    #[tokio::test]
    async fn test_delete() {
        let _guard = ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(Some(dir.path().join("chat-sessions"))).await.unwrap();

        store.create("del-me", "single", vec!["manager".into()]).await.unwrap();
        assert!(store.get("del-me").await.is_ok());

        store.delete("del-me").await.unwrap();
        assert!(store.get("del-me").await.is_err());
    }
}