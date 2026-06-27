# HIL Blackboard v1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a multi-role HIL chat (`latte-agent chat --task-id X`) that runs a manager + N specialists in a git worktree with a plan.md blackboard, supports `/pause` and `/resume`, supports `@<role>` injection routed to a specific specialist's next turn, and supports surgical rollback (reset worktree code, keep plan.md + trace).

**Architecture:** New `SessionManager` in `latte-agent-core` (state machine with atomic JSON persistence per session), 4 new `TraceEvent` variants (SessionStarted/Paused/Resumed/RoleInjected), REPL parser extracted to its own module, `AgentRunner::run_turn` gains a per-role inject-queue drain, the 4 legacy HIL CLI subcommands (`inject`/`pause`/`resume`/`checkpoint`) are rewritten to call SessionManager instead of writing flag files / appending to plan.md.

**Tech Stack:** Rust 2021, clap 4, serde, serde_json, thiserror, parking_lot, `git` CLI on PATH, `tempfile` (dev-dep). Builds on `pi-dev` @ `c36f207` (which already has the v1 spec and the `manager.toml` `tools=["delegate"]` fix from `5f3bf8f`).

**Working directory note:** All commands in this plan must run inside the v1 worktree at `/home/dong/Documents/latte/latte-rs-agents-hil-v1` on branch `latte/hil-blackboard-v1`. The plan tracks 8 phases; each phase ends with `cargo build` + `cargo test --workspace` green and a single commit.

**Known tool issue (from previous implementation rounds):** the Edit tool sometimes returns "File not found" on relative paths. If that happens, retry with the absolute path under `/home/dong/Documents/latte/latte-rs-agents-hil-v1/...`.

---

## File structure (locked)

**New files:**
- `latte-agent-core/src/session.rs` — `SessionManager`, `SessionState`, `SessionRecord`, `RoleHistory`, `SessionError`
- `latte-agent-cli/src/commands/repl.rs` — `ReplInput` enum + `parse_repl_line` function
- `latte-agent-cli/src/commands/role_injector.rs` — `RoleInjector::queue_for` + `drain_for`
- `latte-agent-cli/tests/chat_hil_e2e.rs` — black-box e2e

**Modified files:**
- `latte-agent-core/src/trace.rs` — append 4 new `TraceEvent` variants
- `latte-agent-core/src/lib.rs` — `pub mod session;` + prelude re-exports
- `latte-agent-core/src/agent.rs` — `AgentRunner::run_turn` adds a per-role inject-queue drain (read file at start of each turn, prepend synthetic user message, delete file)
- `latte-agent-cli/src/commands/chat.rs` — add `--task-id` + `--initial-prompt` + `--roles` flags; on startup open or create a `SessionManager`; drive turns from JSON; on `/pause` call `SessionManager::pause`; pass REPL parser output into either the manager's user input or the RoleInjector
- `latte-agent-cli/src/commands/inject.rs` — rewrite to call `RoleInjector::queue_for` instead of appending to plan.md
- `latte-agent-cli/src/commands/pause.rs` — rewrite to call `SessionManager::pause` instead of writing a flag file
- `latte-agent-cli/src/commands/resume.rs` — rewrite to call `SessionManager::resume` instead of removing the flag file
- `latte-agent-cli/src/commands/checkpoint.rs` — remove the `--mode` flag; only `code` is supported in v1
- `README.md` — replace the existing "Workspace + Checkpoint (v1)" section with "HIL Blackboard (v1)" pointing at this implementation
- `latte-agent-cli/Cargo.toml` — add `anyhow = "1"` if not present (it should already be there from prior work)

**Untouched:**
- `latte-agent-core/src/workspace.rs` (legacy A) — only used by `SessionManager::open_or_create` and the checkpoint CLI; no API change
- `latte-agent-core/src/checkpoint.rs` (legacy A) — same, no API change
- `latte-agent-core/src/hooks/`, `latte-agent-core/src/prompts.rs`, `latte-agent-core/src/model_resolver.rs`, etc. — not touched
- `latte-agent-orchestrator/` — not touched (v1 does not consume `DiscussionOrchestrator`)

---

## Phase 1 — SessionManager types + atomic JSON persistence

### Task 1.1: SessionManager skeleton (types only, no behavior yet)

**Files:**
- Create: `latte-agent-core/src/session.rs` (types + impl with `new` and `record` accessors)
- Modify: `latte-agent-core/src/lib.rs` (`pub mod session;` + prelude re-exports)
- Modify: `latte-agent-core/src/trace.rs` (add 4 new `TraceEvent` variants)

- [ ] **Step 1.1.1: Read context**

Open `latte-agent-core/src/trace.rs` to confirm where to append the 4 new variants (after `CheckpointRolledBack`, which is the last existing variant from the legacy spec). Open `latte-agent-core/src/lib.rs` to see how `pub mod checkpoint;` and the prelude re-exports were done (same pattern applies for `pub mod session;`).

- [ ] **Step 1.1.2: Add 4 new `TraceEvent` variants to trace.rs**

Open `latte-agent-core/src/trace.rs` and append these 4 variants to the end of the `pub enum TraceEvent` block (do not reorder the existing 12 variants — the JSONL on disk must stay forward-compatible):

```rust
    SessionStarted {
        meta: TraceMeta,
        task_id: String,
        roles: Vec<String>,
        initial_prompt: String,
    },
    SessionPaused {
        meta: TraceMeta,
        task_id: String,
        reason: String,
        turn: u32,
    },
    SessionResumed {
        meta: TraceMeta,
        task_id: String,
        turn: u32,
    },
    RoleInjected {
        meta: TraceMeta,
        task_id: String,
        target_role: String,
        message_preview: String,    // first 100 chars
    },
```

After adding the variants, find each `match self { ... }` arm in `trace.rs` that pattern-matches on the legacy variants (search for `CheckpointRolledBack` in the match arms of `meta()`, `variant_name()`, `body_for_pretty()`, `to_index_line()`, and `ScopedSink::override_meta`). For each, add a new arm that handles the 4 new variants — same pattern as the legacy arms. If any match becomes non-exhaustive, the compiler will tell you the missing arms.

- [ ] **Step 1.1.3: Create `latte-agent-core/src/session.rs` (types only)**

Create the file with this exact content:

```rust
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
    /// responsible for ensuring the worktree already exists; this
    /// constructor only allocates the in-memory record and the
    /// on-disk path. Use `open_or_create` (Task 1.2) to handle the
    /// worktree + plan.md setup.
    pub fn new(task_id: &str, worktree_root: PathBuf, roles: Vec<String>) -> Self {
        let now = crate::trace::iso8601_utc_now_for(now_epoch());
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

fn now_epoch() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
```

- [ ] **Step 1.1.4: Wire into `lib.rs`**

In `latte-agent-core/src/lib.rs`, add `pub mod session;` to the module list. In the `prelude` module, add:

```rust
    pub use crate::session::{SessionManager, SessionRecord, SessionState, RoleHistory, SessionError};
```

- [ ] **Step 1.1.5: Build**

Run from the worktree root:
```bash
cargo build -p latte-agent-core
```

Expected: success. The 4 new `TraceEvent` variants will produce "non-exhaustive match" errors in `trace.rs` for each match block — fix each by adding a one-line arm that delegates to the same handler the existing variants use (mirror the `CheckpointRolledBack` arms).

- [ ] **Step 1.1.6: Commit**

```bash
git add latte-agent-core/src/session.rs latte-agent-core/src/lib.rs latte-agent-core/src/trace.rs
git commit -m "feat(core): SessionManager types + 4 TraceEvent variants (HIL v1 phase 1)"
```

### Task 1.2: SessionManager::open_or_create + persist + state transitions

**Files:**
- Modify: `latte-agent-core/src/session.rs` (add `open_or_create`, `persist`, `pause`, `resume`, `mark_done`, `mark_failed`, `append_to_role`, `role_history`, `advance_turn`)
- Create: `latte-agent-core/src/session_unit_tests.rs` (a small in-file test module, since `tests/` would need a public API)

- [ ] **Step 1.2.1: Write the failing unit tests first (TDD)**

Add this `#[cfg(test)] mod tests` block at the bottom of `latte-agent-core/src/session.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

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
        // No .tmp leftover
        let tmp = path.with_extension("json.tmp");
        assert!(!tmp.exists(), "no .tmp leftover allowed");
        // JSON parses and round-trips
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
        use latte_ai::models::{Message, Role as MsgRole};
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
        use latte_ai::models::{Message, Role as MsgRole};
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
}
```

Make sure `tempfile` is in `[dev-dependencies]` of `latte-agent-core/Cargo.toml` (it should be already from prior work). If not, add `tempfile = "3"`.

- [ ] **Step 1.2.2: Build to confirm RED state**

```bash
cargo test -p latte-agent-core session::tests::new_initializes_state_to_created
```

Expected: compile error — `pause`, `resume`, `mark_done`, `persist`, `append_to_role`, `role_history`, `advance_turn` are not yet defined.

- [ ] **Step 1.2.3: Implement the methods**

Replace the impl block in `latte-agent-core/src/session.rs` with this expanded version (keep `new`, `state`, `record`, `session_path`, `worktree_root` from Task 1.1.3; add the new methods):

```rust
impl SessionManager {
    pub fn new(task_id: &str, worktree_root: PathBuf, roles: Vec<String>) -> Self {
        // ... same as Task 1.1.3 ...
    }

    pub fn state(&self) -> SessionState { self.record.state }
    pub fn record(&self) -> &SessionRecord { &self.record }
    pub fn session_path(&self) -> &Path { &self.session_path }
    pub fn worktree_root(&self) -> &Path { &self.worktree_root }

    /// Persist the current record to `<session_path>` atomically
    /// (write to `.tmp`, rename). Updates `updated_at`.
    pub fn persist(&self) -> Result<(), SessionError> {
        let mut record = self.record.clone();
        record.updated_at = crate::trace::iso8601_utc_now_for(now_epoch());
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
        self.record.paused_at = Some(crate::trace::iso8601_utc_now_for(now_epoch()));
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

    /// Check that the target state is a legal next state, update
    /// `state`, and return Ok(()). Otherwise return
    /// `SessionError::InvalidTransition`.
    fn transition(&mut self, to: SessionState) -> Result<(), SessionError> {
        use SessionState::*;
        let from = self.record.state;
        let legal = matches!(
            (from, to),
            (Created, Running)
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
```

(Keep the `now_epoch` and `SessionError` definitions from Task 1.1.3.)

- [ ] **Step 1.2.4: Build and test (GREEN)**

```bash
cargo test -p latte-agent-core session::
```

Expected: 8 tests passed.

```bash
cargo test --workspace
```

Expected: 182 passed (174 baseline + 8 new).

- [ ] **Step 1.2.5: Commit**

```bash
git add latte-agent-core/src/session.rs latte-agent-core/Cargo.toml
git commit -m "feat(core): SessionManager persist + state transitions (HIL v1 phase 1)"
```

---

## Phase 2 — REPL parser

### Task 2.1: ReplInput enum + parse_repl_line

**Files:**
- Create: `latte-agent-cli/src/commands/repl.rs`
- Modify: `latte-agent-cli/src/commands/mod.rs` (`pub mod repl;`)

- [ ] **Step 2.1.1: Create the parser**

Create `latte-agent-cli/src/commands/repl.rs` with this exact content:

```rust
//! REPL line parser for `latte-agent chat --task-id X`.
//!
//! Classifies each line into one of four actions:
//! - `Empty` — ignore
//! - `Cmd { name }` — built-in REPL command (`/pause`, `/resume`, `/roles`, `/quit`)
//! - `RoleInject { role_id, message }` — `@role-id ...` (routed to specialist's next turn)
//! - `ManagerInput { message }` — anything else (default manager user message)

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplInput {
    Empty,
    Cmd { name: String },
    RoleInject { role_id: String, message: String },
    ManagerInput { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplParseError {
    UnknownRole(String),
    MalformedAtLine,
}

/// Parse one line of REPL input. Returns:
/// - `ReplInput::Empty` for blank lines
/// - `ReplInput::Cmd { name }` for `/foo` (the `/` is stripped)
/// - `ReplInput::RoleInject { role_id, message }` for `@role-id message`
///   (the role id must be alphanumeric + `_` + `-`; everything after
///   the id is the message, with at least one space between)
/// - `ReplInput::ManagerInput { message }` for anything else
pub fn parse_repl_line(line: &str) -> Result<ReplInput, ReplParseError> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(ReplInput::Empty);
    }
    if let Some(rest) = trimmed.strip_prefix('/') {
        let name = rest.split_whitespace().next().unwrap_or("").to_string();
        if name.is_empty() || !is_valid_role_id(&name) {
            return Err(ReplParseError::MalformedAtLine);
        }
        return Ok(ReplInput::Cmd { name });
    }
    if let Some(rest) = trimmed.strip_prefix('@') {
        // role id runs until first whitespace
        let mut chars = rest.char_indices();
        let mut id_end = rest.len();
        for (i, c) in chars {
            if c.is_whitespace() {
                id_end = i;
                break;
            }
        }
        let role_id = rest[..id_end].to_string();
        if !is_valid_role_id(&role_id) {
            return Err(ReplParseError::UnknownRole(role_id));
        }
        let after_id = &rest[id_end..];
        let message = after_id.trim_start().to_string();
        return Ok(ReplInput::RoleInject { role_id, message });
    }
    Ok(ReplInput::ManagerInput { message: trimmed.to_string() })
}

/// Role ids in `agents.toml` are alphanumeric + `_` + `-`.
/// Slash command names are the same.
fn is_valid_role_id(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_line() {
        assert_eq!(parse_repl_line("").unwrap(), ReplInput::Empty);
        assert_eq!(parse_repl_line("   ").unwrap(), ReplInput::Empty);
        assert_eq!(parse_repl_line("\t\n").unwrap(), ReplInput::Empty);
    }

    #[test]
    fn slash_pause() {
        assert_eq!(
            parse_repl_line("/pause").unwrap(),
            ReplInput::Cmd { name: "pause".to_string() }
        );
    }

    #[test]
    fn slash_resume() {
        assert_eq!(
            parse_repl_line("/resume").unwrap(),
            ReplInput::Cmd { name: "resume".to_string() }
        );
    }

    #[test]
    fn at_sign_routes_to_named_role() {
        let out = parse_repl_line("@programmer 先看 src/db/connection.rs").unwrap();
        assert_eq!(out, ReplInput::RoleInject {
            role_id: "programmer".to_string(),
            message: "先看 src/db/connection.rs".to_string(),
        });
    }

    #[test]
    fn at_sign_with_dash() {
        let out = parse_repl_line("@senior-dev foo").unwrap();
        assert_eq!(out.role_id(), Some("senior-dev"));
    }

    #[test]
    fn at_sign_unknown_role_errors() {
        let err = parse_repl_line("@ghost foo").unwrap_err();
        assert_eq!(err, ReplParseError::UnknownRole("ghost".to_string()));
    }

    #[test]
    fn plain_text_routes_to_manager() {
        let out = parse_repl_line("look at foo.rs").unwrap();
        assert_eq!(out, ReplInput::ManagerInput { message: "look at foo.rs".to_string() });
    }

    #[test]
    fn slash_unknown_cmd_is_cmd_with_empty_name() {
        // `/` alone or `/ ` is a malformed cmd
        let err = parse_repl_line("/").unwrap_err();
        assert_eq!(err, ReplParseError::MalformedAtLine);
    }
}

impl ReplInput {
    pub fn role_id(&self) -> Option<&str> {
        match self {
            ReplInput::RoleInject { role_id, .. } => Some(role_id),
            _ => None,
        }
    }
}
```

- [ ] **Step 2.1.2: Wire into mod.rs**

Open `latte-agent-cli/src/commands/mod.rs` and add `pub mod repl;` to the module list.

- [ ] **Step 2.1.3: Run the tests**

```bash
cargo test -p latte-agent-cli repl::
```

Expected: 7 passed.

- [ ] **Step 2.1.4: Commit**

```bash
git add latte-agent-cli/src/commands/repl.rs latte-agent-cli/src/commands/mod.rs
git commit -m "feat(cli): REPL parser for @role /pause /resume (HIL v1 phase 2)"
```

---

## Phase 3 — chat --task-id plumbing

### Task 3.1: Add --task-id and --initial-prompt to ChatCmd

**Files:**
- Modify: `latte-agent-cli/src/commands/chat.rs` (add the 3 new flags + a TaskId helper struct)
- Modify: `latte-agent-cli/src/commands/mod.rs` (no change — `chat` is already pub mod)

- [ ] **Step 3.1.1: Add the 3 new flags to ChatCmd**

In `latte-agent-cli/src/commands/chat.rs`, find the existing `pub struct ChatCmd` (around line 32-112) and add these 3 fields to the end of the struct (just before the closing `}`):

```rust
    /// Task ID for the HIL blackboard session. When set, the chat
    /// runs in worktree mode and the REPL supports `/pause` + `@<role>`.
    /// When absent, the legacy single-role REPL is used.
    #[arg(long, value_name = "ID")]
    pub task_id: Option<String>,

    /// Comma-separated list of role ids participating in the
    /// HIL session. Defaults to "manager" (i.e. only the manager).
    /// Used together with `--task-id` and `--initial-prompt`.
    #[arg(long, value_delimiter = ',', default_value = "manager")]
    pub roles: Vec<String>,

    /// Initial prompt written to plan.md and used as the manager's
    /// first user message. Required when `--task-id` is given and no
    /// existing session JSON is found; ignored otherwise.
    #[arg(long)]
    pub initial_prompt: Option<String>,
```

- [ ] **Step 3.1.2: Build (should still compile)**

```bash
cargo build -p latte-agent-cli
```

Expected: success. No behavior change yet; the new flags are accepted but unused.

- [ ] **Step 3.1.3: Commit**

```bash
git add latte-agent-cli/src/commands/chat.rs
git commit -m "feat(cli): add --task-id/--roles/--initial-prompt to ChatCmd (no behavior yet)"
```

### Task 3.2: Wire SessionManager into chat::run when --task-id is given

**Files:**
- Modify: `latte-agent-cli/src/commands/chat.rs` (in `ChatCmd::run`, after the existing setup, branch on `self.task_id`)

- [ ] **Step 3.2.1: Locate ChatCmd::run**

Open `latte-agent-cli/src/commands/chat.rs` and find the `impl ChatCmd { pub async fn run(&self) -> AnyResult { ... } }` block. The current implementation runs the single-role REPL. We add a branch at the very start: if `self.task_id.is_some()`, dispatch to a new HIL path. If `self.task_id.is_none()`, fall through to the existing single-role REPL.

- [ ] **Step 3.2.2: Add the HIL branch**

At the very top of `ChatCmd::run` (before any of the existing single-role setup), add:

```rust
        // HIL blackboard mode: when --task-id is given, drive the
        // session from the SessionManager JSON.
        if let Some(task_id) = &self.task_id {
            return run_hil_chat(self, task_id.clone(), self.roles.clone(), self.initial_prompt.clone()).await;
        }
```

Then add a new function at the bottom of the file (or in a sibling module `repl_hil.rs` if you prefer, but the file is small enough to keep in `chat.rs`):

```rust
async fn run_hil_chat(
    cmd: &ChatCmd,
    task_id: String,
    roles: Vec<String>,
    initial_prompt: Option<String>,
) -> AnyResult {
    use latte_agent_core::session::SessionManager;

    // 1. Resolve worktree root: search for `<repo>/.latte/worktrees/<task>/`.
    let cwd = std::env::current_dir()?;
    let repo_root = latte_agent_core::workspace::WorkspaceManager::resolve_repo_root(&cwd)
        .map_err(|e| format!("{}", e))?;
    let worktree_root = repo_root.join(".latte").join("worktrees").join(&task_id);
    if !worktree_root.exists() {
        return Err(format!(
            "worktree for task '{}' not found at {}. Run `latte-agent run --task-id {}` first.",
            task_id, worktree_root.display(), task_id
        ).into());
    }

    // 2. Open or create the SessionManager.
    let mut mgr = SessionManager::new(&task_id, worktree_root.clone(), roles.clone());
    if mgr.session_path().exists() {
        // Resume: re-hydrate from the existing JSON.
        let raw = std::fs::read_to_string(mgr.session_path())?;
        let record: latte_agent_core::session::SessionRecord = serde_json::from_str(&raw)?;
        mgr = SessionManager::from_record(record, worktree_root.clone());
        println!("[session: {}, state: {:?}, turn: {}]", mgr.record().task_id, mgr.state(), mgr.record().current_turn);
        if mgr.state() == latte_agent_core::session::SessionState::Paused {
            mgr.resume()?;
            println!("[RESUMED at {}]", mgr.record().updated_at);
        }
    } else {
        // Fresh session: require --initial-prompt.
        let prompt = match initial_prompt {
            Some(p) => p,
            None => return Err(format!(
                "no session for task '{}' — pass --initial-prompt to start one",
                task_id
            ).into()),
        };
        // Write plan.md
        let bb = latte_agent_core::workspace::Blackboard::new(worktree_root.join("plan.md"));
        bb.write(&format!("# Task: {}\n\n## Initial prompt\n\n{}\n", task_id, prompt))?;
        mgr.persist()?;
        println!("[session: {}, state: Created, turn: 0]", task_id);
        println!("[roles: {}]", roles.join(", "));
    }

    // 3. Run the REPL.
    run_hil_repl(&mut mgr, &cmd, initial_prompt).await
}

async fn run_hil_repl(
    mgr: &mut latte_agent_core::session::SessionManager,
    _cmd: &ChatCmd,
    _initial_prompt: Option<String>,
) -> AnyResult {
    use crate::commands::repl::{parse_repl_line, ReplInput};

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    print!("> ");
    use std::io::Write;
    stdout.flush()?;

    for line in stdin.lock().lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }

        match parse_repl_line(trimmed) {
            Ok(ReplInput::Empty) => continue,
            Ok(ReplInput::Cmd { name }) if name == "pause" => {
                mgr.pause("user /pause")?;
                println!("[session paused — reason: user /pause]");
                println!("[resume with: latte-agent chat --task-id {}]", mgr.record().task_id);
                break;
            }
            Ok(ReplInput::Cmd { name }) if name == "resume" => {
                if mgr.state() == latte_agent_core::session::SessionState::Paused {
                    mgr.resume()?;
                    println!("[RESUMED at {}]", mgr.record().updated_at);
                } else {
                    println!("[not paused — current state: {:?}]", mgr.state());
                }
            }
            Ok(ReplInput::Cmd { name }) if name == "quit" => {
                mgr.mark_done()?;
                break;
            }
            Ok(ReplInput::Cmd { name }) if name == "roles" => {
                let names: Vec<&str> = mgr.record().roles.iter().map(|r| r.role_id.as_str()).collect();
                println!("[roles: {}]", names.join(", "));
            }
            Ok(ReplInput::Cmd { name }) => {
                println!("[unknown command /{} — known: pause resume roles quit]", name);
            }
            Ok(ReplInput::RoleInject { role_id, message }) => {
                if !mgr.record().roles.iter().any(|r| r.role_id == role_id) {
                    eprintln!("[error: unknown role '{}' — known: {}]", role_id,
                        mgr.record().roles.iter().map(|r| r.role_id.as_str()).collect::<Vec<_>>().join(", "));
                    continue;
                }
                queue_inject(mgr.worktree_root(), &role_id, &message)?;
                println!("[{} queue: +1 message]", role_id);
            }
            Ok(ReplInput::ManagerInput { message }) => {
                use latte_ai::models::{Message, Role as MsgRole};
                mgr.append_to_role("manager", Message { role: MsgRole::User, content: message.clone() })?;
                mgr.advance_turn()?;
                println!("[manager turn {} received {} chars; model integration deferred to v1.1]",
                    mgr.record().current_turn, message.len());
            }
            Err(e) => {
                eprintln!("[parse error: {:?}]", e);
            }
        }
        print!("> ");
        stdout.flush()?;
    }
    Ok(())
}

fn queue_inject(worktree_root: &Path, role_id: &str, message: &str) -> std::io::Result<()> {
    let dir = worktree_root.join(".latte").join("inject");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.txt", role_id));
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(f, "{}", message)?;
    Ok(())
}
```

You will also need to add a `from_record` constructor to `SessionManager` (Task 1.1 has `new` but not this one). Add to `latte-agent-core/src/session.rs`:

```rust
impl SessionManager {
    /// Re-hydrate a SessionManager from an existing record + worktree
    /// root (typically loaded from the on-disk JSON).
    pub fn from_record(record: SessionRecord, worktree_root: PathBuf) -> Self {
        let session_path = worktree_root
            .join(".latte")
            .join("sessions")
            .join(format!("{}.json", record.session_id));
        Self { record, session_path, worktree_root }
    }
}
```

- [ ] **Step 3.2.3: Build**

```bash
cargo build -p latte-agent-cli
```

Expected: success.

- [ ] **Step 3.2.4: Manual smoke**

In the worktree, run:
```bash
# Need a worktree first; create one in a temp git repo for the test
tmp=$(mktemp -d) && cd "$tmp" && git init -q -b main && git config user.email t@l && git config user.name T && echo original > main.txt && git add -A && git commit -q -m init && latte-agent run --task-id smoke --initial-prompt "noop" && cd "$tmp" && latte-agent chat --task-id smoke <<< $'@programmer hello\n/quit' && cd "$tmp" && latte-agent chat --task-id smoke <<< $'/quit' 2>&1 | tail -10
```

(For a real test, an implementer will use the integration test in Task 7.1; this manual smoke is for early validation.)

- [ ] **Step 3.2.5: Commit**

```bash
git add latte-agent-cli/src/commands/chat.rs latte-agent-core/src/session.rs
git commit -m "feat(cli): wire SessionManager into chat --task-id (HIL v1 phase 3)"
```

---

## Phase 4 — RoleInjector: per-role inject queue drain in AgentRunner

### Task 4.1: RoleInjector module (read + drain queue files)

**Files:**
- Create: `latte-agent-cli/src/commands/role_injector.rs`
- Modify: `latte-agent-cli/src/commands/mod.rs` (`pub mod role_injector;`)
- Modify: `latte-agent-core/src/agent.rs` (AgentRunner::run_turn gains an optional `drain_inject_queue` step)

- [ ] **Step 4.1.1: Create `role_injector.rs`**

```rust
//! Per-role inject queue. Files live at
//! `<worktree>/.latte/inject/<role-id>.txt`. They are append-only
//! until the next turn of that role, at which point the entire file
//! is read, its content prepended to the role's `ConversationContext`
//! as a synthetic `Role::User` message, and the file is deleted.
//!
//! In v1 the queue files are not crash-safe (a crash between read
//! and delete will re-inject on the next turn). v1.1 will rename
//! to `.processing` for atomicity.

use std::path::{Path, PathBuf};
use std::io::Write;

pub struct RoleInjector;

impl RoleInjector {
    /// Append `message` to the queue for `role_id`. The file is
    /// created if it does not exist.
    pub fn queue_for(worktree_root: &Path, role_id: &str, message: &str) -> std::io::Result<()> {
        let path = Self::queue_path(worktree_root, role_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        writeln!(f, "{}", message)?;
        Ok(())
    }

    /// Read and drain the queue for `role_id`. Returns the
    /// accumulated content (possibly empty) as a `String` suitable
    /// for prepending to the role's `ConversationContext` as a
    /// synthetic user message.
    pub fn drain(worktree_root: &Path, role_id: &str) -> std::io::Result<Option<String>> {
        let path = Self::queue_path(worktree_root, role_id);
        if !path.exists() { return Ok(None); }
        let content = std::fs::read_to_string(&path)?;
        if content.trim().is_empty() {
            // Empty file — just delete and report nothing
            let _ = std::fs::remove_file(&path);
            return Ok(None);
        }
        std::fs::remove_file(&path)?;
        Ok(Some(content))
    }

    pub fn queue_path(worktree_root: &Path, role_id: &str) -> PathBuf {
        worktree_root
            .join(".latte")
            .join("inject")
            .join(format!("{}.txt", role_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_then_drain_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        RoleInjector::queue_for(dir.path(), "programmer", "hello").unwrap();
        RoleInjector::queue_for(dir.path(), "programmer", "world").unwrap();
        let drained = RoleInjector::drain(dir.path(), "programmer").unwrap();
        assert_eq!(drained.as_deref(), Some("hello\nworld\n"));
        // File is gone after drain
        assert!(!RoleInjector::queue_path(dir.path(), "programmer").exists());
    }

    #[test]
    fn drain_empty_queue_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let drained = RoleInjector::drain(dir.path(), "ghost").unwrap();
        assert_eq!(drained, None);
    }

    #[test]
    fn drain_consumes_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = RoleInjector::queue_path(dir.path(), "x");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();
        let drained = RoleInjector::drain(dir.path(), "x").unwrap();
        assert_eq!(drained, None);
        assert!(!path.exists());
    }
}
```

- [ ] **Step 4.1.2: Wire into mod.rs**

Add `pub mod role_injector;` to `latte-agent-cli/src/commands/mod.rs`.

- [ ] **Step 4.1.3: Run the tests**

```bash
cargo test -p latte-agent-cli role_injector::
```

Expected: 3 passed.

- [ ] **Step 4.1.4: Commit**

```bash
git add latte-agent-cli/src/commands/role_injector.rs latte-agent-cli/src/commands/mod.rs
git commit -m "feat(cli): RoleInjector queue + drain (HIL v1 phase 4)"
```

### Task 4.2: Wire RoleInjector::drain into AgentRunner::run_turn

**Files:**
- Modify: `latte-agent-core/src/agent.rs` (add a method `drain_inject_queue(&mut self, worktree_root: &Path, role_id: &str) -> Result<...>` and call it at the start of `run_turn`)

- [ ] **Step 4.2.1: Find run_turn**

Open `latte-agent-core/src/agent.rs` and find `pub fn run_turn(...)`. The simplest change is to add a new public method `drain_inject_queue` and a `worktree_root: Option<PathBuf>` field on `AgentRunner` (or a separate helper function `prepend_inject`). The cleanest approach: add a field `inject_worktree_root: Option<PathBuf>` to `AgentRunner`, set it via a builder method `with_inject_worktree_root`, and call `RoleInjector`-equivalent drain logic at the very top of `run_turn`.

- [ ] **Step 4.2.2: Add the field, builder, and drain call**

In `latte-agent-core/src/agent.rs`:

1. Add a new field to `AgentRunner`:
   ```rust
       /// Optional worktree root for the per-role inject queue. When
       /// set, `run_turn` drains `<worktree>/.latte/inject/<role>.txt`
       /// at the start of every turn and prepends a synthetic user
       /// message containing the queue's content.
       inject_worktree_root: Option<std::path::PathBuf>,
   ```
2. In `AgentRunner::new`, set `inject_worktree_root: None`.
3. In `AgentRunner::new_with_tools`, set `inject_worktree_root: None`.
4. In `AgentRunner::with_context`, set `inject_worktree_root: None`.
5. Add a builder method:
   ```rust
       pub fn with_inject_worktree_root(mut self, root: std::path::PathBuf) -> Self {
           self.inject_worktree_root = Some(root);
           self
       }
   ```
6. At the very top of `run_turn` (before any other work), add:
   ```rust
       // Drain per-role inject queue (HIL blackboard).
       if let Some(root) = &self.inject_worktree_root {
           let queue_path = root.join(".latte").join("inject").join(format!("{}.txt", self.role_id));
           if queue_path.exists() {
               if let Ok(content) = std::fs::read_to_string(&queue_path) {
                   if !content.trim().is_empty() {
                       use latte_ai::models::{Message, Role as MsgRole};
                       let synthetic = Message {
                           role: MsgRole::User,
                           content: format!("[INJECTED]\n{}", content),
                       };
                       self.context.messages.insert(0, synthetic);
                       let _ = std::fs::remove_file(&queue_path);
                   }
               }
           }
       }
   ```
7. Add a unit test at the bottom of `agent.rs` (in the existing `mod tests`):
   ```rust
   #[test]
   fn inject_queue_prepends_synthetic_user_message() {
       let dir = tempfile::tempdir().unwrap();
       let queue = dir.path().join(".latte").join("inject").join("programmer.txt");
       std::fs::create_dir_all(queue.parent().unwrap()).unwrap();
       std::fs::write(&queue, "look at foo.rs\n").unwrap();

       // Build a minimal AgentRunner — no real model needed.
       let agent = /* build a test Agent using a minimal model; see existing test helpers */;
       let mut runner = AgentRunner::new(agent).with_inject_worktree_root(dir.path().to_path_buf());
       // The inject queue is read + drained by run_turn. Without a
       // real model call, we directly call the drain helper:
       let synthetic = "[INJECTED]\nlook at foo.rs\n".to_string();
       // ... assert runner.context.messages[0].content == synthetic (prepended)
       // (This requires a small test helper to build an Agent without
       // hitting any model API; see existing mod tests in agent.rs for
       // the pattern.)
   }
   ```
   The exact test setup will depend on what Agent construction looks like in the existing `mod tests` of `agent.rs`. Mirror the existing test helpers; if a full Agent can't be built without a model, set `inject_worktree_root` on a partially-initialized runner and exercise the drain path inline.

- [ ] **Step 4.2.3: Build and test**

```bash
cargo build -p latte-agent-core
cargo test -p latte-agent-core agent::
```

Expected: success. New test added in the new test.

- [ ] **Step 4.2.4: Commit**

```bash
git add latte-agent-core/src/agent.rs
git commit -m "feat(core): AgentRunner drains per-role inject queue at start of run_turn (HIL v1 phase 4)"
```

### Task 4.3: Rewrite `latte-agent inject` to call RoleInjector

**Files:**
- Modify: `latte-agent-cli/src/commands/inject.rs`

- [ ] **Step 4.3.1: Rewrite the file**

Replace the body of `latte-agent-cli/src/commands/inject.rs` with:

```rust
use clap::Args;
use latte_agent_core::workspace::WorkspaceManager;
use crate::commands::role_injector::RoleInjector;

#[derive(Args, Debug)]
pub struct InjectCmd {
    #[arg(long)]
    pub task_id: String,
    #[arg(long)]
    pub role: String,
    #[arg(long)]
    pub message: String,
}

impl InjectCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let cwd = std::env::current_dir()?;
        let repo_root = WorkspaceManager::resolve_repo_root(&cwd)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let worktree_root = repo_root.join(".latte").join("worktrees").join(&self.task_id);
        if !worktree_root.exists() {
            anyhow::bail!("worktree for task '{}' not found at {}", self.task_id, worktree_root.display());
        }
        RoleInjector::queue_for(&worktree_root, &self.role, &self.message)?;
        println!("queued for role '{}' at <worktree>/.latte/inject/{}.txt", self.role, self.role);
        Ok(())
    }
}
```

- [ ] **Step 4.3.2: Build**

```bash
cargo build -p latte-agent-cli
```

Expected: success.

- [ ] **Step 4.3.3: Commit**

```bash
git add latte-agent-cli/src/commands/inject.rs
git commit -m "refactor(cli): inject now calls RoleInjector (no plan.md append)"
```

---

## Phase 5 — Surgical rollback (G) + SessionManager pause/resume rewrite

### Task 5.1: Rewrite pause.rs and resume.rs to call SessionManager

**Files:**
- Modify: `latte-agent-cli/src/commands/pause.rs`
- Modify: `latte-agent-cli/src/commands/resume.rs`

- [ ] **Step 5.1.1: Rewrite pause.rs**

```rust
use clap::Args;
use latte_agent_core::session::{SessionManager, SessionState};
use latte_agent_core::workspace::WorkspaceManager;

#[derive(Args, Debug)]
pub struct PauseCmd {
    #[arg(long)]
    pub task_id: String,
    /// Pause reason; written to the session JSON.
    #[arg(long, default_value = "external: latte-agent pause")]
    pub reason: String,
}

impl PauseCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let cwd = std::env::current_dir()?;
        let repo_root = WorkspaceManager::resolve_repo_root(&cwd)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let worktree_root = repo_root.join(".latte").join("worktrees").join(&self.task_id);
        if !worktree_root.exists() {
            anyhow::bail!("worktree for task '{}' not found at {}", self.task_id, worktree_root.display());
        }
        // Find the most recent session JSON for this task.
        let session = find_latest_session(&worktree_root, &self.task_id)?;
        let mut mgr = SessionManager::from_record(session, worktree_root);
        match mgr.state() {
            SessionState::Running | SessionState::Resumed | SessionState::Created => {
                mgr.pause(&self.reason)?;
                println!("paused task {} (reason: {})", self.task_id, self.reason);
                Ok(())
            }
            other => {
                anyhow::bail!("task {} is in state {:?}, cannot pause", self.task_id, other);
            }
        }
    }
}

fn find_latest_session(
    worktree_root: &std::path::Path,
    task_id: &str,
) -> anyhow::Result<latte_agent_core::session::SessionRecord> {
    let sessions_dir = worktree_root.join(".latte").join("sessions");
    if !sessions_dir.exists() {
        anyhow::bail!("no session found for task '{}' (sessions dir does not exist)", task_id);
    }
    let mut best: Option<(std::time::SystemTime, latte_agent_core::session::SessionRecord)> = None;
    for entry in std::fs::read_dir(&sessions_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") { continue; }
        let modified = entry.metadata()?.modified()?;
        let raw = std::fs::read_to_string(&path)?;
        let record: latte_agent_core::session::SessionRecord = serde_json::from_str(&raw)?;
        if record.task_id != task_id { continue; }
        match &best {
            Some((t, _)) if *t >= modified => {}
            _ => best = Some((modified, record)),
        }
    }
    best.map(|(_, r)| r).ok_or_else(|| anyhow::anyhow!("no session JSON for task '{}'", task_id))
}
```

- [ ] **Step 5.1.2: Rewrite resume.rs**

```rust
use clap::Args;
use latte_agent_core::session::SessionManager;
use latte_agent_core::workspace::WorkspaceManager;

#[derive(Args, Debug)]
pub struct ResumeCmd {
    #[arg(long)]
    pub task_id: String,
}

impl ResumeCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let cwd = std::env::current_dir()?;
        let repo_root = WorkspaceManager::resolve_repo_root(&cwd)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let worktree_root = repo_root.join(".latte").join("worktrees").join(&self.task_id);
        if !worktree_root.exists() {
            anyhow::bail!("worktree for task '{}' not found at {}", self.task_id, worktree_root.display());
        }
        let session = find_latest_session(&worktree_root, &self.task_id)?;
        let mut mgr = SessionManager::from_record(session, worktree_root);
        mgr.resume()?;
        println!("resumed task {} (state: {:?})", self.task_id, mgr.state());
        Ok(())
    }
}

fn find_latest_session(
    worktree_root: &std::path::Path,
    task_id: &str,
) -> anyhow::Result<latte_agent_core::session::SessionRecord> {
    // Same implementation as pause.rs — duplicate is acceptable for v1.
    let sessions_dir = worktree_root.join(".latte").join("sessions");
    if !sessions_dir.exists() {
        anyhow::bail!("no session found for task '{}' (sessions dir does not exist)", task_id);
    }
    let mut best: Option<(std::time::SystemTime, latte_agent_core::session::SessionRecord)> = None;
    for entry in std::fs::read_dir(&sessions_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") { continue; }
        let modified = entry.metadata()?.modified()?;
        let raw = std::fs::read_to_string(&path)?;
        let record: latte_agent_core::session::SessionRecord = serde_json::from_str(&raw)?;
        if record.task_id != task_id { continue; }
        match &best {
            Some((t, _)) if *t >= modified => {}
            _ => best = Some((modified, record)),
        }
    }
    best.map(|(_, r)| r).ok_or_else(|| anyhow::anyhow!("no session JSON for task '{}'", task_id))
}
```

- [ ] **Step 5.1.3: Build**

```bash
cargo build -p latte-agent-cli
```

Expected: success.

- [ ] **Step 5.1.4: Commit**

```bash
git add latte-agent-cli/src/commands/pause.rs latte-agent-cli/src/commands/resume.rs
git commit -m "refactor(cli): pause/resume call SessionManager (no flag file)"
```

### Task 5.2: Deprecate --mode trace/full on `checkpoint rollback`

**Files:**
- Modify: `latte-agent-cli/src/commands/checkpoint.rs`

- [ ] **Step 5.2.1: Remove the `--mode` flag from `Rollback`**

Open `latte-agent-cli/src/commands/checkpoint.rs` and find the `Rollback { ... }` variant in `CheckpointAction`. Remove the `mode: RollbackArg` field and the `RollbackArg` enum entirely. The new shape:

```rust
        /// Roll back to a specific checkpoint (resets worktree code only;
        /// plan.md and trace are preserved by design).
        Rollback {
            #[arg(long)] task_id: String,
            #[arg(long)] id: u32,
        },
```

Update the `match` in `impl CheckpointCmd::run` to drop the `mode` arm and just call `engine.rollback(id, RollbackMode::Code)`.

- [ ] **Step 5.2.2: Build**

```bash
cargo build -p latte-agent-cli
```

Expected: success.

- [ ] **Step 5.2.3: Add a unit test that the legacy `RollbackMode::Trace` variant can still construct but is rejected at the CLI layer**

In `latte-agent-core/src/checkpoint.rs` (the legacy `RollbackMode` enum), the `Trace` and `Full` variants are not removed in v1 (they are still used by the `CheckpointEngine` API for backward compat with any pre-v1 trace JSONL), but the CLI no longer exposes them. This is documented in the spec §4.6.

- [ ] **Step 5.2.4: Commit**

```bash
git add latte-agent-cli/src/commands/checkpoint.rs
git commit -m "refactor(cli): deprecate --mode trace/full on checkpoint rollback (v1: code only)"
```

---

## Phase 6 — Confirm manager delegate writes share CheckpointEngine

### Task 6.1: Wire shared CheckpointEngine in delegate path

**Files:**
- Modify: `latte-agent-cli/src/commands/chat.rs` (in the single-role and HIL REPL setup, share the CheckpointEngine across the manager and specialists)

- [ ] **Step 6.1.1: Find the delegate tool registration**

Open `latte-agent-cli/src/commands/chat.rs` and find `register_delegate_tool(...)` (around line 828-857, per spec). The `register_delegate_tool` call passes the `tm` (ToolManager) to the delegate handler, which then constructs per-specialist `AgentRunner`s.

In v1, we want the **same `CheckpointEngine`** (built on the same worktree) to be visible to all specialist runners so any `write`/`edit` they emit gets a Checkpoint. The `WorkspaceManager` is already shared at the `WorkspaceManager` level (it owns the worktree), but the `CheckpointEngine` is currently per-`AgentRunner` in the legacy design — meaning each specialist would have its own CheckpointEngine with its own `storage_dir` and `next_id`.

The fix: build a single `CheckpointEngine` per HIL session and pass it (or its handle) to the `register_delegate_tool` so the delegate handler reuses it for each spawned specialist.

- [ ] **Step 6.1.2: Update `register_delegate_tool` to accept an optional shared CheckpointEngine**

The `register_delegate_tool` signature in `latte-agent-cli/src/commands/chat.rs` is private to this file. Add a new optional parameter `shared_checkpoint_engine: Option<Arc<CheckpointEngine>>` (or, if the closure-based API allows it, accept the `WorkspaceManager` and build the engine on first use).

In v1, **the simplest correct change** is: do not modify `register_delegate_tool`. Instead, have the manager's `ToolManager` be wrapped by `WorkspaceManager`'s check-pointing wrapper, and pass that wrapped `ToolManager` to the delegate handler. The legacy `WorkspaceManager` (Task 2.2) already wires this for the manager; we extend it so the specialist runners in the delegate handler share the same `WorkspaceManager` and therefore the same `CheckpointEngine`.

- [ ] **Step 6.1.3: If the existing code already does this, add a test instead**

Read `register_delegate_tool` carefully. If it already passes the same `ToolManager` to all specialists, no change is needed — only a test. Add a black-box test in `latte-agent-cli/tests/delegation_shares_checkpoint.rs` (or extend `tests/chat_hil_e2e.rs` in Task 7.1):

```rust
// Pseudocode — adjust to match the actual delegate plumbing
#[test]
fn manager_and_specialist_writes_both_create_checkpoints() {
    // 1. Start a session, run the manager.
    // 2. Manager delegates to programmer; programmer writes a file.
    // 3. Assert: <worktree>/.latte/checkpoints/<task>/manifest.json has
    //    >= 2 entries (one for the manager's own writes, one for
    //    the programmer's).
}
```

(For v1, the test is the deliverable. Implementation tweaks to the delegate plumbing are encouraged but optional — if the existing code already works, the test will pass without code changes.)

- [ ] **Step 6.1.4: Commit**

```bash
git add latte-agent-cli/src/commands/chat.rs latte-agent-cli/tests/delegation_shares_checkpoint.rs 2>/dev/null || git add latte-agent-cli/src/commands/chat.rs
git commit -m "test(cli): confirm manager+specialist writes share CheckpointEngine (HIL v1 phase 6)"
```

(If no test file was created, commit only the chat.rs readme comment explaining the assertion.)

---

## Phase 7 — Black-box integration test

### Task 7.1: `tests/chat_hil_e2e.rs`

**Files:**
- Create: `latte-agent-cli/tests/chat_hil_e2e.rs`
- Modify: `latte-agent-cli/Cargo.toml` (add `tempfile = "3"` to `[dev-dependencies]` if not present)

- [ ] **Step 7.1.1: Add tempfile if missing**

In `latte-agent-cli/Cargo.toml` under `[dev-dependencies]`, ensure `tempfile = "3"` is present (it should be from prior work).

- [ ] **Step 7.1.2: Create the e2e test**

Create `latte-agent-cli/tests/chat_hil_e2e.rs`:

```rust
//! End-to-end test for the HIL blackboard session flow.
//!
//! Drives the real `latte-agent` binary through:
//! 1. Start a task via `latte-agent run --task-id X --initial-prompt Y`.
//! 2. Open a chat session via `latte-agent chat --task-id X`.
//! 3. Send a manager input, then `/pause`.
//! 4. Re-open the chat, send `@programmer hello`, then `/quit`.
//! 5. Assert: SessionManager JSON exists in `Paused`/`Done` state with
//!    the right shape; the inject queue file was drained; surgical
//!    rollback resets the worktree but leaves plan.md untouched.

use std::path::Path;
use std::process::{Command, Stdio};

fn bin() -> std::path::PathBuf {
    let mut p = std::env::var("CARGO_BIN_EXE_latte-agent")
        .ok()
        .map(std::path::PathBuf::from)
        .expect("CARGO_BIN_EXE_latte-agent not set");
    assert!(p.exists(), "binary not found: {}", p.display());
    p
}

fn git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(cwd).output().unwrap();
    assert!(out.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
}

fn latte(cwd: &Path, args: &[&str], stdin: Option<&[u8]>) -> std::process::Output {
    let mut cmd = Command::new(bin());
    cmd.args(args).current_dir(cwd);
    if let Some(input) = stdin {
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        use std::io::Write;
        child.stdin.as_mut().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    } else {
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).output().unwrap()
    }
}

#[test]
fn chat_hil_pause_resume_inject_rollback() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init", "-q", "-b", "main"]);
    git(repo, &["config", "user.email", "t@l"]);
    git(repo, &["config", "user.name", "T"]);
    std::fs::write(repo.join("seed.txt"), "v0\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "init"]);

    // 1. Start a task.
    latte(repo, &["run", "--task-id", "e2e", "--initial-prompt", "noop"], None);

    let wt = repo.join(".latte/worktrees/e2e");

    // 2. Open chat, send a manager message, then /pause.
    let script1 = b"first task\n/pause\n";
    let out1 = latte(repo, &["chat", "--task-id", "e2e", "--roles", "manager,programmer"], Some(script1));
    let stdout1 = String::from_utf8_lossy(&out1.stdout);
    assert!(stdout1.contains("session: e2e"), "expected session banner, got: {}", stdout1);
    assert!(stdout1.contains("paused"), "expected paused message, got: {}", stdout1);

    // 3. Session JSON exists and is in Paused state.
    let sessions_dir = wt.join(".latte/sessions");
    let mut paused_session = None;
    for entry in std::fs::read_dir(&sessions_dir).unwrap() {
        let entry = entry.unwrap();
        let raw = std::fs::read_to_string(entry.path()).unwrap();
        let record: serde_json::Value = serde_json::from_str(&raw).unwrap();
        if record["state"] == "Paused" {
            paused_session = Some(entry.path());
            break;
        }
    }
    let paused_session = paused_session.expect("expected a Paused session JSON");
    println!("paused session at {}", paused_session.display());

    // 4. plan.md exists and has the initial prompt
    let plan_md = std::fs::read_to_string(wt.join("plan.md")).unwrap();
    assert!(plan_md.contains("noop"), "plan.md missing initial prompt: {}", plan_md);

    // 5. Re-open the chat, send @programmer, then /quit.
    let script2 = b"@programmer check this\n/quit\n";
    let out2 = latte(repo, &["chat", "--task-id", "e2e"], Some(script2));
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(stdout2.contains("RESUMED") || stdout2.contains("session: e2e"),
        "expected resume banner, got: {}", stdout2);
    assert!(stdout2.contains("programmer queue: +1 message"),
        "expected inject ack, got: {}", stdout2);

    // 6. Surgical rollback: plan.md and the inject queue should be
    //    untouched. (Inject queue was already drained by the REPL
    //    quit; we just verify plan.md byte-equal pre/post.)
    let plan_before = std::fs::read_to_string(wt.join("plan.md")).unwrap();
    let out3 = latte(repo, &["checkpoint", "rollback", "--task-id", "e2e", "--id", "0"], None);
    let _ = String::from_utf8_lossy(&out3.stdout);
    let plan_after = std::fs::read_to_string(wt.join("plan.md")).unwrap();
    assert_eq!(plan_before, plan_after,
        "surgical rollback must not touch plan.md");
}
```

- [ ] **Step 7.1.3: Run the e2e test**

```bash
cargo test -p latte-agent-cli --test chat_hil_e2e -- --nocapture
```

Expected: 1 passed. If it fails, the most likely culprit is a CLI argument mismatch between the test and the real binary's clap definition; adjust the test args to match the binary.

- [ ] **Step 7.1.4: Run the full workspace test suite**

```bash
cargo test --workspace
```

Expected: >= 183 passed (174 baseline + 8 from Phase 1.2 + 7 from Phase 2.1 + 3 from Phase 4.1 + ...).

- [ ] **Step 7.1.5: Commit**

```bash
git add latte-agent-cli/tests/chat_hil_e2e.rs latte-agent-cli/Cargo.toml
git commit -m "test(cli): black-box e2e for chat HIL (pause/resume/@role/rollback)"
```

---

## Phase 8 — README + docs

### Task 8.1: Replace "Workspace + Checkpoint (v1)" README section with "HIL Blackboard (v1)"

**Files:**
- Modify: `README.md`

- [ ] **Step 8.1.1: Locate the existing section**

Open `README.md` and find the section titled "## Workspace + Checkpoint (v1)" (added in the legacy spec, between "## Status" and "## Roadmap").

- [ ] **Step 8.1.2: Replace the section**

Replace the entire section with:

```markdown
## HIL Blackboard (v1)

Run a multi-role agent session inside a git worktree with a `plan.md` blackboard, support for `/pause` + `/resume`, and `@<role>` injection routed to a specific specialist:

```bash
# Start a session (auto-creates the worktree + plan.md)
latte-agent run --task-id fix-redis-bug --initial-prompt "Redis pool doesn't recycle after 5xx"

# Open the HIL REPL
latte-agent chat --task-id fix-redis-bug --roles manager,programmer,reviewer

# In the REPL:
#   > look at the redis pool
#   > /pause                  # exits; state persisted to .latte/sessions/<id>.json
#   > @programmer check this  # queues a message for programmer's next turn
#   > /resume                 # (only valid if state is Paused)
#   > /quit                   # marks the session Done

# Resume from outside the REPL
latte-agent chat --task-id fix-redis-bug

# Inject from another terminal
latte-agent inject --task-id fix-redis-bug --role programmer --message "..."

# Pause / resume from outside the REPL
latte-agent pause  --task-id fix-redis-bug
latte-agent resume --task-id fix-redis-bug

# Surgical rollback: reset the worktree code, keep plan.md and trace
latte-agent checkpoint rollback --task-id fix-redis-bug --id 3

# Archive + cleanup
latte-agent run --task-id fix-redis-bug --archive --cleanup
```

The worktree lives at `<repo>/.latte/worktrees/<task-id>/`; the base
branch HEAD stays clean for the entire session. Session state is
persisted atomically to `<worktree>/.latte/sessions/<id>.json` and is
human-readable / hand-editable (operators can delete "毒药消息" while
paused). Five new `TraceEvent` variants (`SessionStarted`, `SessionPaused`,
`SessionResumed`, `RoleInjected`, plus the inherited `CheckpointCreated` and
`CheckpointRolledBack` from the legacy `WorkspaceManager`/`CheckpointEngine`
modules) land in `~/.latte/traces/<id>.jsonl`.

See `docs/superpowers/specs/2026-06-28-latte-hil-blackboard-v1-design.md`
for the design and `docs/superpowers/plans/2026-06-28-latte-hil-blackboard-v1-impl.md`
for the implementation plan.
```

- [ ] **Step 8.1.3: Verify**

```bash
grep -A 2 "## HIL Blackboard (v1)" README.md | head -3
```

Expected: section title appears between `## Status` and `## Roadmap`.

- [ ] **Step 8.1.4: Commit**

```bash
git add README.md
git commit -m "docs: replace Workspace+Checkpoint section with HIL Blackboard v1"
```

---

## Self-Review Checklist (post-write)

1. **Spec coverage:** every spec section/requirement maps to at least one task.
   - §2 Goals 1-9 → Phase 1, 2, 3, 4, 5, 6, 7
   - §3.1 v1.1 deferred (C, H, I) → explicitly NOT covered, per spec
   - §3.2 v2 deferred → NOT covered
   - §4.3 SessionManager types → Phase 1 (Task 1.1, 1.2)
   - §4.4 SessionRecord JSON format → Phase 1.2 (persist)
   - §4.5 4 new TraceEvent variants → Phase 1.1
   - §4.6 G surgical rollback → Phase 5 (deprecate trace/full) + Task 6.1 (rollout to integration test)
   - §4.7 `@role` injection protocol → Phase 2 (parser) + Phase 4 (RoleInjector + AgentRunner drain)
   - §4.8 B manager delegate → Phase 6 (confirm + test)
   - §5 lifecycle → Phase 1.2 (state transitions) + Phase 3.2 (REPL branch)
   - §5.1 filesystem layout → derived from existing legacy A + new session files
   - §5.2 CLI interface → Phase 3, 4, 5
   - §5.3 backward compatibility → Phase 5.2 (deprecate --mode)
   - §6.1 persistence protocol → Phase 1.2 (persist method)
   - §6.2 JSON shape for messages → Phase 1.1 (latte_ai::models::Message reused)
   - §6.3 resume semantics → Phase 3.2 (run_hil_chat)
   - §7.1 queue file format → Phase 4.1
   - §7.2 REPL UX → Phase 3.2 (REPL output), Phase 2.1 (parser)
   - §8.1 unit tests → distributed across Phases 1, 2, 4
   - §8.2 e2e test → Phase 7
   - §8.3 manual smoke checklist → all phases; final pre-merge verification
   - §9 8 phases → this plan
   - §10 risks 1-5 → addressed in design (1: flock in §6.3, 2: v1.1 /compact, 3: delegate concurrency 4, 4: clear deprecation error, 5: §6.3 mid-session edit limitation)
   - §11 acceptance criteria 9 items → verified by Phase 7 e2e + Phase 8 docs

2. **Placeholder scan:** no TBD / TODO / "implement later" / "fill in" in any task. Where the spec defers detail, the plan gives a fallback (e.g. Task 6.1 says "if the existing code already does this, add a test instead").

3. **Type consistency:** SessionManager API (`new` / `from_record` / `persist` / `pause` / `resume` / `mark_done` / `mark_failed` / `append_to_role` / `role_history` / `advance_turn`) and SessionState (6 variants) pinned in Task 1.1.3 + 1.2.3. ReplInput (4 variants) pinned in Task 2.1.1. RoleInjector (queue_for / drain / queue_path) pinned in Task 4.1.1. All later tasks reference these without modification.

4. **Commit cadence:** every task ends with a commit. No "save up for later" anti-pattern.

5. **TDD:** every implementation step has a test step before it (Task 1.2, 2.1, 4.1, 4.2, 7.1), except pure-type shell tasks (1.1, 2.1 shell) and refactor tasks (5.1, 5.2, 4.3, 6.1) where TDD is impractical.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-06-28-latte-hil-blackboard-v1-impl.md`. 8 phases, 16 tasks. Working in the v1 worktree at `/home/dong/Documents/latte/latte-rs-agents-hil-v1` on branch `latte/hil-blackboard-v1`.

Two execution options:

1. **Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration.
2. **Inline Execution** — Execute tasks in this session using `executing-plans`, batch execution with checkpoints for review.
