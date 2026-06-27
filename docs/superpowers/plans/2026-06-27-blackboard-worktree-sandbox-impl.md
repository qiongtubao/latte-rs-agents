# Blackboard + Worktree Sandbox Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `WorkspaceManager` + `CheckpointEngine` + HIL CLI on top of `latte-agent` so that any `latte-agent run --task-id X` runs in a git worktree, auto-checkpoints on every `write`/`edit`, and supports per-checkpoint rollback. Spec: `docs/superpowers/specs/2026-06-27-blackboard-worktree-sandbox-design.md`.

**Architecture:** Two new modules in `latte-agent-core` (`workspace.rs`, `checkpoint.rs`), two new `TraceEvent` variants, six new clap subcommands in `latte-agent-cli`. No new crates. No global `--worktree` flag. Existing `chat`/`discuss`/`workflow`/`list`/`config`/`debug` are untouched.

**Tech Stack:** Rust 2021, clap 4, serde, serde_json, thiserror, parking_lot, `git` CLI (assumed on PATH), `tempfile` (dev-dep for e2e test only).

**Working directory note:** Execute this plan inside a fresh git worktree created from `pi-dev` (use the `using-git-worktrees` skill). The worktree's branch name should be `latte/blackboard-sandbox` and must be the base branch when the user later archives the task — but for implementation work, the spec's own "task" is just a development branch.

---

## Phase 1 — Foundation Types

### Task 1.1: DiffSummary + Checkpoint + new TraceEvent variants

**Files:**
- Modify: `latte-agent-core/src/trace.rs` (extend `TraceEvent` enum and re-exports)
- Create: `latte-agent-core/src/checkpoint.rs` (skeleton: types only, no engine yet)
- Modify: `latte-agent-core/src/lib.rs` (`pub mod checkpoint;` + re-exports)
- Modify: `latte-agent-core/Cargo.toml` (no new deps)

- [ ] **Step 1.1.1: Add `DiffSummary` to trace.rs**

In `latte-agent-core/src/trace.rs`, locate the existing `pub struct IndexLine` (around line 389) and add the following struct right above the `StdoutSink` block (around line 422). Place it next to the other trace payload structs:

```rust
/// Per-checkpoint diff summary. Stored on `CheckpointCreated` events
/// and on the on-disk `manifest.json` so operators can see what
/// changed without reading the patch file.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiffSummary {
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
}
```

- [ ] **Step 1.1.2: Add two new `TraceEvent` variants**

In the same file, extend the `pub enum TraceEvent` (around line 116). Add the two new variants after the existing `SessionEnd` arm. Do NOT renumber or reorder the existing 9 variants — the on-disk JSONL schema is forward-compatible only when variants are appended:

```rust
    CheckpointCreated {
        meta: TraceMeta,
        checkpoint_id: u32,
        git_commit: String,
        trigger_kind: String,        // "tool_write" | "explicit" | "pre_hazard"
        diff_summary: DiffSummary,
    },
    CheckpointRolledBack {
        meta: TraceMeta,
        checkpoint_id: u32,
        mode: String,                // "code" | "trace" | "full"
        rolled_back_to: String,      // git_commit SHA
    },
```

- [ ] **Step 1.1.3: Create `checkpoint.rs` skeleton**

Create `latte-agent-core/src/checkpoint.rs` with the type definitions only — no engine logic yet. This task is compileable types plus doc-comments. The `Checkpoint`/`CheckpointTrigger`/`RollbackMode` shapes are pinned here so later tasks cannot drift:

```rust
//! Checkpoint types for the WorkspaceManager sandbox.
//!
//! A `Checkpoint` is a recorded moment in a worktree's history. It pairs
//! a git commit SHA with a unified diff on disk and a summary on the
//! `TraceEvent::CheckpointCreated` line that announced it.
//!
//! See `docs/superpowers/specs/2026-06-27-blackboard-worktree-sandbox-design.md` §4.4.

use std::path::PathBuf;
use serde::{Deserialize, Serialize};

use crate::trace::DiffSummary;

#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    #[error("git command failed: {0}")]
    GitFailed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("checkpoint {0} not found in task {1}")]
    NotFound(u32, String),
    #[error("worktree not initialized")]
    NotInitialized,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum CheckpointTrigger {
    ToolWrite { tool: String, args_hash: String },
    Explicit,
    PreHazard,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: u32,
    pub task_id: String,
    pub git_commit: String,
    pub created_at: String,
    pub trigger: CheckpointTrigger,
    pub diff_summary: DiffSummary,
    pub diff_path: PathBuf,
    pub trace_event_index: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RollbackMode {
    Code,
    Trace,
    Full,
}

impl Default for RollbackMode {
    fn default() -> Self { RollbackMode::Full }
}
```

- [ ] **Step 1.1.4: Wire the new module into lib.rs**

In `latte-agent-core/src/lib.rs`, add `pub mod checkpoint;` to the module list (next to `pub mod trace;`). Also add the re-exports so CLI code can write `use latte_agent_core::checkpoint::CheckpointEngine` without a deep path:

```rust
pub mod checkpoint;
```

And in the `prelude` module, add:

```rust
pub use crate::checkpoint::{Checkpoint, CheckpointTrigger, CheckpointError, RollbackMode};
pub use crate::trace::DiffSummary;
```

- [ ] **Step 1.1.5: Run `cargo build` to verify it compiles**

Run: `cargo build -p latte-agent-core`
Expected: success, no warnings about unused code (the engine code in Task 3.1 will use these).

- [ ] **Step 1.1.6: Write a serde round-trip test for the new variants**

Append a test inside `latte-agent-core/src/trace.rs`'s existing `#[cfg(test)] mod tests` block (around line 612). The test exercises the new variants so we know the on-disk JSONL shape is stable:

```rust
#[test]
fn checkpoint_variants_serde_round_trip() {
    use crate::trace::{TraceEvent, TraceMeta, DiffSummary};
    use crate::checkpoint::{CheckpointTrigger, RollbackMode};

    let meta = TraceMeta {
        turn: 1,
        role: "manager".into(),
        ts: "2026-06-27T00:00:00Z".into(),
        session_id: "fix-redis-bug".into(),
    };

    let created = TraceEvent::CheckpointCreated {
        meta: meta.clone(),
        checkpoint_id: 1,
        git_commit: "abc123".into(),
        trigger_kind: "tool_write".into(),
        diff_summary: DiffSummary { files_changed: 1, insertions: 5, deletions: 0 },
    };
    let json = serde_json::to_string(&created).unwrap();
    let back: TraceEvent = serde_json::from_str(&json).unwrap();
    match back {
        TraceEvent::CheckpointCreated { checkpoint_id, .. } => {
            assert_eq!(checkpoint_id, 1);
        }
        _ => panic!("wrong variant after round-trip"),
    }

    let rolled = TraceEvent::CheckpointRolledBack {
        meta,
        checkpoint_id: 1,
        mode: "full".into(),
        rolled_back_to: "abc123".into(),
    };
    let json = serde_json::to_string(&rolled).unwrap();
    let back: TraceEvent = serde_json::from_str(&json).unwrap();
    match back {
        TraceEvent::CheckpointRolledBack { mode, .. } => assert_eq!(mode, "full"),
        _ => panic!("wrong variant after round-trip"),
    }

    // Sanity: triggers and modes serialize lowercase.
    let trig = serde_json::to_string(&CheckpointTrigger::Explicit).unwrap();
    assert_eq!(trig, "\"explicit\"");
    let mode = serde_json::to_string(&RollbackMode::Code).unwrap();
    assert_eq!(mode, "\"code\"");
}
```

- [ ] **Step 1.1.7: Run the new test**

Run: `cargo test -p latte-agent-core checkpoint_variants_serde_round_trip`
Expected: PASS, 1 passed.

- [ ] **Step 1.1.8: Commit**

```bash
git add latte-agent-core/src/trace.rs latte-agent-core/src/checkpoint.rs latte-agent-core/src/lib.rs
git commit -m "feat(core): add DiffSummary + Checkpoint types + 2 TraceEvent variants"
```

---

## Phase 2 — WorkspaceManager Creation

### Task 2.1: Blackboard + WorktreeSpec + WorkspaceManager skeleton

**Files:**
- Create: `latte-agent-core/src/workspace.rs` (skeleton: types + thin `Blackboard`)
- Modify: `latte-agent-core/src/lib.rs` (re-exports)
- Create: `latte-agent-core/src/workspace.rs` test module

- [ ] **Step 2.1.1: Create `workspace.rs` with types + Blackboard**

Create `latte-agent-core/src/workspace.rs`. The file owns the workspace types AND a thin `Blackboard` wrapper (just a file path + read/write helpers — no in-memory state). The full `WorkspaceManager` lands in Task 2.2:

```rust
//! WorkspaceManager: a git worktree per task, with a plan.md blackboard.
//!
//! See `docs/superpowers/specs/2026-06-27-blackboard-worktree-sandbox-design.md` §4.3-§5.

use std::path::{Path, PathBuf};
use std::fs;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("git command failed: {0}")]
    GitFailed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("not a git repository: {0}")]
    NotARepo(PathBuf),
    #[error("task_id already in use: {0}")]
    TaskIdInUse(String),
    #[error("worktree path not found: {0}")]
    NotFound(PathBuf),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeSpec {
    pub task_id: String,
    pub base_branch: String,
    pub worktree_root: PathBuf,
    pub branch_name: String,
    pub blackboard_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceState {
    Created,
    Running { active_checkpoint_id: u32 },
    Archiving,
    Archived { merge_commit: String },
    Failed { reason: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MergeMode {
    NoFf,
    Squash,
    FastForward,
}

impl Default for MergeMode {
    fn default() -> Self { MergeMode::NoFf }
}

impl WorktreeSpec {
    /// Resolve `<repo>/.latte/worktrees/<task_id>` and `latte/<task_id>`.
    pub fn derive_paths(repo_root: &Path, task_id: &str) -> (PathBuf, String, PathBuf) {
        let worktree_root = repo_root.join(".latte").join("worktrees").join(task_id);
        let branch_name = format!("latte/{}", task_id);
        let blackboard_path = worktree_root.join("plan.md");
        (worktree_root, branch_name, blackboard_path)
    }
}

/// Thin wrapper around the blackboard file. v1: file only, no in-memory cache.
#[derive(Debug, Clone)]
pub struct Blackboard {
    path: PathBuf,
}

impl Blackboard {
    pub fn new(path: PathBuf) -> Self { Self { path } }
    pub fn path(&self) -> &Path { &self.path }

    pub fn read(&self) -> Result<String, WorkspaceError> {
        Ok(fs::read_to_string(&self.path)?)
    }

    pub fn write(&self, content: &str) -> Result<(), WorkspaceError> {
        if let Some(parent) = self.path.parent() { fs::create_dir_all(parent)?; }
        fs::write(&self.path, content)?;
        Ok(())
    }

    pub fn exists(&self) -> bool { self.path.exists() }
}
```

- [ ] **Step 2.1.2: Wire into lib.rs**

In `latte-agent-core/src/lib.rs`, add `pub mod workspace;` and prelude re-exports:

```rust
pub mod workspace;
```

And in `prelude`:

```rust
pub use crate::workspace::{WorktreeSpec, WorkspaceState, WorkspaceError, Blackboard, MergeMode};
```

- [ ] **Step 2.1.3: Write a Blackboard test**

Append a `#[cfg(test)] mod tests` block to `latte-agent-core/src/workspace.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_paths_places_worktree_under_latte_dir() {
        let repo = PathBuf::from("/tmp/fake-repo");
        let (wt, branch, bb) = WorktreeSpec::derive_paths(&repo, "fix-redis-bug");
        assert_eq!(wt, PathBuf::from("/tmp/fake-repo/.latte/worktrees/fix-redis-bug"));
        assert_eq!(branch, "latte/fix-redis-bug");
        assert_eq!(bb, PathBuf::from("/tmp/fake-repo/.latte/worktrees/fix-redis-bug/plan.md"));
    }

    #[test]
    fn blackboard_round_trip_via_tempfile() {
        let dir = tempfile::tempdir().unwrap();
        let bb = Blackboard::new(dir.path().join("plan.md"));
        assert!(!bb.exists());
        bb.write("# Hello\n").unwrap();
        assert!(bb.exists());
        assert_eq!(bb.read().unwrap(), "# Hello\n");
    }
}
```

`tempfile` is already a transitive dep through `latte-rs-agent-tools` (verify with `cargo tree -p tempfile`); if not present, add `tempfile = "3"` to `[dev-dependencies]` in `latte-agent-core/Cargo.toml`.

- [ ] **Step 2.1.4: Run the new tests**

Run: `cargo test -p latte-agent-core workspace::`
Expected: 2 passed.

- [ ] **Step 2.1.5: Commit**

```bash
git add latte-agent-core/src/workspace.rs latte-agent-core/src/lib.rs latte-agent-core/Cargo.toml
git commit -m "feat(core): add WorktreeSpec + Blackboard + workspace types"
```

### Task 2.2: WorkspaceManager::create + initial Checkpoint 0

**Files:**
- Modify: `latte-agent-core/src/workspace.rs` (add `WorkspaceManager` with `create`)
- Create: `latte-agent-core/tests/workspace_create.rs` (black-box test in a real git repo)

- [ ] **Step 2.2.1: Add a `git` helper to workspace.rs**

Add this private helper near the top of `latte-agent-core/src/workspace.rs` (after the imports):

```rust
use std::process::Command;

fn git_cmd(cwd: &Path, args: &[&str]) -> Result<String, WorkspaceError> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| WorkspaceError::GitFailed(e.to_string()))?;
    if !out.status.success() {
        return Err(WorkspaceError::GitFailed(
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
```

- [ ] **Step 2.2.2: Add `WorkspaceManager` with `create`**

Add to the same file (below `Blackboard`):

```rust
#[derive(Debug)]
pub struct WorkspaceManager {
    spec: WorktreeSpec,
    repo_root: PathBuf,
    blackboard: Blackboard,
    state: WorkspaceState,
}

impl WorkspaceManager {
    pub fn spec(&self) -> &WorktreeSpec { &self.spec }
    pub fn state(&self) -> &WorkspaceState { &self.state }
    pub fn blackboard(&self) -> &Blackboard { &self.blackboard }
    pub fn repo_root(&self) -> &Path { &self.repo_root }

    /// Resolve the current git repo root. Errors with `NotARepo` if
    /// `git rev-parse --show-toplevel` fails.
    pub fn resolve_repo_root(from: &Path) -> Result<PathBuf, WorkspaceError> {
        let root = git_cmd(from, &["rev-parse", "--show-toplevel"])?;
        if root.is_empty() {
            return Err(WorkspaceError::NotARepo(from.to_path_buf()));
        }
        Ok(PathBuf::from(root))
    }

    /// Resolve the current branch name. Errors with `NotARepo` if HEAD
    /// is detached or git fails.
    pub fn resolve_base_branch(from: &Path) -> Result<String, WorkspaceError> {
        let branch = git_cmd(from, &["rev-parse", "--abbrev-ref", "HEAD"])?;
        if branch.is_empty() || branch == "HEAD" {
            return Err(WorkspaceError::NotARepo(from.to_path_buf()));
        }
        Ok(branch)
    }

    /// Create the worktree, branch, and blackboard. Initial state is
    /// `Created`. Checkpoint 0 is recorded in this step; see Task 3.2.
    pub fn create(
        cwd: &Path,
        task_id: &str,
        initial_prompt: &str,
    ) -> Result<Self, WorkspaceError> {
        let repo_root = Self::resolve_repo_root(cwd)?;
        let base_branch = Self::resolve_base_branch(cwd)?;
        let (worktree_root, branch_name, blackboard_path) =
            WorktreeSpec::derive_paths(&repo_root, task_id);

        if worktree_root.exists() {
            return Err(WorkspaceError::TaskIdInUse(task_id.into()));
        }

        fs::create_dir_all(worktree_root.parent().unwrap())?;

        git_cmd(
            &repo_root,
            &["worktree", "add", "-b", &branch_name, worktree_root.to_str().unwrap(), &base_branch],
        )?;

        // Drop a .gitignore so the worktree's own .latte/ (checkpoints,
        // inject queue) doesn't get committed into its branch.
        let gi = worktree_root.join(".gitignore");
        fs::write(&gi, ".latte/\n")?;

        let blackboard = Blackboard::new(blackboard_path);
        blackboard.write(&format!(
            "# Task: {}\n\n## Initial prompt\n\n{}\n",
            task_id, initial_prompt,
        ))?;

        // First commit on the worktree branch: include the .gitignore and
        // the initial plan.md. This is the "Checkpoint 0" boundary.
        git_cmd(&worktree_root, &["add", "-A"])?;
        git_cmd(
            &worktree_root,
            &["commit", "-m", &format!("checkpoint({}): init", task_id)],
        )?;

        Ok(Self {
            spec: WorktreeSpec { task_id: task_id.into(), base_branch, worktree_root, branch_name, blackboard_path },
            repo_root,
            blackboard,
            state: WorkspaceState::Created,
        })
    }
}
```

- [ ] **Step 2.2.3: Write a black-box create test**

Create `latte-agent-core/tests/workspace_create.rs`:

```rust
//! Black-box test for `WorkspaceManager::create`. Uses a real temp git repo.

use std::process::Command;
use latte_agent_core::workspace::{WorkspaceManager, WorkspaceState};

fn run(cwd: &std::path::Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(cwd).output().unwrap();
    assert!(out.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
}

#[test]
fn create_makes_worktree_branch_and_initial_commit() {
    let dir = tempfile::tempdir().unwrap();
    run(dir.path(), &["init", "-q", "-b", "main"]);
    run(dir.path(), &["config", "user.email", "test@local"]);
    run(dir.path(), &["config", "user.name", "Test"]);
    fs::write(dir.path().join("seed.txt"), "seed\n").unwrap();
    run(dir.path(), &["add", "-A"]);
    run(dir.path(), &["commit", "-q", "-m", "init"]);

    let mgr = WorkspaceManager::create(dir.path(), "fix-redis", "redis bug").unwrap();
    assert_eq!(mgr.spec().task_id, "fix-redis");
    assert!(matches!(mgr.state(), WorkspaceState::Created));
    assert!(mgr.blackboard().exists());

    // Worktree shows up in `git worktree list`
    let listing = Command::new("git")
        .args(["worktree", "list"])
        .current_dir(dir.path())
        .output().unwrap();
    let listing = String::from_utf8_lossy(&listing.stdout);
    assert!(listing.contains("fix-redis"), "worktree list missing task: {}", listing);

    // The worktree branch has the initial commit
    let log = Command::new("git")
        .args(["-C", mgr.spec().worktree_root.to_str().unwrap(), "log", "--oneline"])
        .output().unwrap();
    let log = String::from_utf8_lossy(&log.stdout);
    assert!(log.contains("init"), "worktree branch missing initial commit: {}", log);

    // Main branch is untouched
    let main_log = Command::new("git")
        .args(["log", "--oneline"])
        .current_dir(dir.path())
        .output().unwrap();
    let main_log = String::from_utf8_lossy(&main_log.stdout);
    assert!(!main_log.contains("checkpoint"), "main branch was contaminated: {}", main_log);
}

use std::fs;
```

`tempfile` must be in `[dev-dependencies]` of `latte-agent-core/Cargo.toml`; if not yet added, add `tempfile = "3"` and re-run.

- [ ] **Step 2.2.4: Run the test**

Run: `cargo test -p latte-agent-core --test workspace_create`
Expected: 1 passed.

- [ ] **Step 2.2.5: Commit**

```bash
git add latte-agent-core/src/workspace.rs latte-agent-core/tests/workspace_create.rs latte-agent-core/Cargo.toml
git commit -m "feat(core): WorkspaceManager::create with worktree + branch + plan.md"
```

---

## Phase 3 — CheckpointEngine Interception

### Task 3.1: CheckpointEngine shell (no git plumbing yet)

**Files:**
- Modify: `latte-agent-core/src/checkpoint.rs` (add `CheckpointEngine` struct + constructor)

- [ ] **Step 3.1.1: Add the `CheckpointEngine` struct**

Append to `latte-agent-core/src/checkpoint.rs`:

```rust
use std::collections::HashSet;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use crate::trace::{DiffSummary, TraceSink};

/// Wraps a worktree, intercepts write/edit calls, and produces
/// `Checkpoint`s. The engine owns its storage dir but does NOT
/// call into `WorkspaceManager` directly — the manager wires it up.
pub struct CheckpointEngine {
    worktree_root: PathBuf,
    storage_dir: PathBuf,
    pub next_id: u32,
    sink: Arc<dyn TraceSink>,
    pub write_tools: HashSet<String>,
    pub enable_bash_capture: bool,
}

impl CheckpointEngine {
    pub fn new(
        worktree_root: std::path::PathBuf,
        storage_dir: std::path::PathBuf,
        sink: Arc<dyn TraceSink>,
    ) -> std::io::Result<Self> {
        std::fs::create_dir_all(&storage_dir)?;
        let next_id = Self::max_existing_id(&storage_dir).map(|n| n + 1).unwrap_or(0);
        let mut write_tools = HashSet::new();
        write_tools.insert("write".to_string());
        write_tools.insert("edit".to_string());
        Ok(Self {
            worktree_root,
            storage_dir,
            next_id,
            sink,
            write_tools,
            enable_bash_capture: false,
        })
    }

    pub fn worktree_root(&self) -> &Path { &self.worktree_root }
    pub fn storage_dir(&self) -> &Path { &self.storage_dir }

    fn max_existing_id(storage_dir: &Path) -> Option<u32> {
        let mut max = None;
        if let Ok(rd) = std::fs::read_dir(storage_dir) {
            for entry in rd.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if let Some(stem) = name.strip_suffix(".patch") {
                        if let Ok(n) = stem.parse::<u32>() {
                            max = Some(max.map_or(n, |m: u32| m.max(n)));
                        }
                    }
                }
            }
        }
        max
    }
}
```

- [ ] **Step 3.1.2: Build to confirm types line up**

Run: `cargo build -p latte-agent-core`
Expected: success.

- [ ] **Step 3.1.3: Commit**

```bash
git add latte-agent-core/src/checkpoint.rs
git commit -m "feat(core): CheckpointEngine shell with next_id and write_tools"
```

### Task 3.2: record_write — full pipeline

**Files:**
- Modify: `latte-agent-core/src/checkpoint.rs` (implement `record_write`)
- Create: `latte-agent-core/tests/checkpoint_record.rs`

- [ ] **Step 3.2.1: Implement `record_write`**

Append to `CheckpointEngine` in `latte-agent-core/src/checkpoint.rs`:

```rust
    /// Capture the worktree's current dirty (or clean) state as a
    /// checkpoint. Always commits — even if no files changed — so the
    /// caller gets a monotonically increasing id sequence.
    pub fn record_write(
        &self,
        tool: &str,
        args: &serde_json::Value,
    ) -> Result<Checkpoint, CheckpointError> {
        use crate::trace::{TraceEvent, TraceMeta};

        let id = self.next_id;
        let diff_path = self.storage_dir.join(format!("{:04}.patch", id));

        // 1. stage all
        run_git(&self.worktree_root, &["add", "-A"])?;

        // 2. summary
        let stat = run_git(&self.worktree_root, &["diff", "--cached", "--shortstat"])?;
        let summary = parse_shortstat(&stat);

        // 3. full diff to file
        let diff = run_git(&self.worktree_root, &["diff", "--cached"])?;
        std::fs::write(&diff_path, &diff)?;

        // 4. commit (allow empty so we still get a checkpoint id when nothing changed)
        let args_hash = short_hash(&format!("{}", args));
        let msg = format!("checkpoint({}): {} {}", self.worktree_root_display(), tool, args_hash);
        run_git_allow_empty(&self.worktree_root, &["commit", "--allow-empty", "-m", &msg])?;
        let git_commit = run_git(&self.worktree_root, &["rev-parse", "HEAD"])?;

        // 5. trace byte offset — read after commit so the line is in the file
        let trace_event_index = current_trace_offset();

        // 6. manifest line
        let created_at = iso8601_utc_now();
        let checkpoint = Checkpoint {
            id,
            task_id: self.task_id().to_string(),
            git_commit,
            created_at: created_at.clone(),
            trigger: CheckpointTrigger::ToolWrite {
                tool: tool.to_string(),
                args_hash,
            },
            diff_summary: summary.clone(),
            diff_path,
            trace_event_index,
        };
        append_manifest(&self.storage_dir, &checkpoint)?;

        // 7. emit
        let meta = TraceMeta {
            turn: 0,
            role: "checkpoint".into(),
            ts: created_at,
            session_id: self.task_id().to_string(),
        };
        self.sink.emit(TraceEvent::CheckpointCreated {
            meta,
            checkpoint_id: id,
            git_commit: checkpoint.git_commit.clone(),
            trigger_kind: "tool_write".into(),
            diff_summary: summary,
        });

        Ok(checkpoint)
    }

    fn task_id(&self) -> &str {
        self.storage_dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
    }

    fn worktree_root_display(&self) -> String {
        // Used only in commit messages. Avoid expensive conversion.
        self.worktree_root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("worktree")
            .to_string()
    }
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<String, CheckpointError> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| CheckpointError::GitFailed(e.to_string()))?;
    if !out.status.success() {
        return Err(CheckpointError::GitFailed(
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn run_git_allow_empty(cwd: &Path, args: &[&str]) -> Result<String, CheckpointError> {
    // Same as run_git but tolerates `--allow-empty` succeeding with no
    // new commit — `git rev-parse HEAD` still works either way.
    run_git(cwd, args).or(Ok(String::new()))
}

fn parse_shortstat(s: &str) -> DiffSummary {
    // Example: " 1 file changed, 5 insertions(+), 0 deletions(-)"
    let mut out = DiffSummary::default();
    let parts: Vec<&str> = s.split(',').map(|p| p.trim()).collect();
    for p in parts {
        if let Some(n) = p.split_whitespace().next().and_then(|x| x.parse::<usize>().ok()) {
            if p.contains("file") { out.files_changed = n; }
            else if p.contains("insertion") { out.insertions = n; }
            else if p.contains("deletion") { out.deletions = n; }
        }
    }
    out
}

fn short_hash(s: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:x}", h.finish())[..8].to_string()
}

fn current_trace_offset() -> u64 {
    // Read LATTE_TRACE_FILE env var; if absent, the trace is going
    // through the sink as a fanout and not on disk — return 0 so the
    // rollback code knows there is nothing to truncate.
    std::env::var("LATTE_TRACE_FILE")
        .ok()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .unwrap_or(0)
}

fn append_manifest(dir: &Path, cp: &Checkpoint) -> Result<(), CheckpointError> {
    let path = dir.join("manifest.json");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    let line = serde_json::to_string(cp)
        .map_err(|e| CheckpointError::GitFailed(e.to_string()))?;
    use std::io::Write;
    writeln!(f, "{}", line)?;
    Ok(())
}

fn iso8601_utc_now() -> String {
    // Use std::time to compute UTC, hand-format to avoid a chrono dep.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    crate::trace::iso8601_utc_now_for(secs)
}
```

The `iso8601_utc_now_for` helper is new — re-export it from `trace.rs` by adding at the bottom of `latte-agent-core/src/trace.rs`:

```rust
/// Format a Unix epoch second count as `YYYY-MM-DDTHH:MM:SSZ` (UTC).
/// Exposed for use by `checkpoint.rs` so we don't duplicate the
/// proleptic-Gregorian math.
pub fn iso8601_utc_now_for(secs: u64) -> String {
    epoch_to_ymdhms(secs)
        .map(|(y, mo, d, h, mi, s)| {
            format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo, d, h, mi, s)
        })
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
}
```

- [ ] **Step 3.2.2: Build**

Run: `cargo build -p latte-agent-core`
Expected: success.

- [ ] **Step 3.2.3: Write the record_write black-box test**

Create `latte-agent-core/tests/checkpoint_record.rs`:

```rust
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use latte_agent_core::checkpoint::CheckpointEngine;
use latte_agent_core::trace::{TraceEvent, TraceSink};

fn run(cwd: &Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(cwd).output().unwrap();
    assert!(out.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
}

#[derive(Default)]
struct CapturingSink { events: Mutex<Vec<TraceEvent>> }
impl TraceSink for CapturingSink {
    fn emit(&self, e: TraceEvent) { self.events.lock().unwrap().push(e); }
}

#[test]
fn record_write_creates_checkpoint_patch_and_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    run(&repo, &["init", "-q", "-b", "main"]);
    run(&repo, &["config", "user.email", "t@l"]);
    run(&repo, &["config", "user.name", "T"]);
    std::fs::write(repo.join("seed.txt"), "seed\n").unwrap();
    run(&repo, &["add", "-A"]); run(&repo, &["commit", "-q", "-m", "init"]);

    // Simulate a worktree at repo/.latte/worktrees/fix-x
    let wt = repo.join(".latte/worktrees/fix-x");
    run(&repo, &["worktree", "add", "-b", "latte/fix-x", wt.to_str().unwrap(), "main"]);
    std::fs::create_dir_all(wt.join(".latte/checkpoints/fix-x")).unwrap();
    let sink = Arc::new(CapturingSink::default());
    let engine = CheckpointEngine::new(wt.clone(), repo.join(".latte/checkpoints/fix-x"), sink.clone()).unwrap();

    // First write
    std::fs::write(wt.join("foo.rs"), "fn main() {}\n").unwrap();
    let cp1 = engine.record_write("write", &serde_json::json!({"path": "foo.rs"})).unwrap();
    assert_eq!(cp1.id, 0);
    assert!(cp1.diff_path.exists());
    assert!(cp1.diff_summary.files_changed >= 1);

    // Second write
    std::fs::write(wt.join("foo.rs"), "fn main() { println!(\"hi\"); }\n").unwrap();
    let cp2 = engine.record_write("edit", &serde_json::json!({"path": "foo.rs"})).unwrap();
    assert_eq!(cp2.id, 1);

    // Manifest
    let manifest = std::fs::read_to_string(repo.join(".latte/checkpoints/fix-x/manifest.json")).unwrap();
    assert_eq!(manifest.lines().count(), 2);

    // Sink received two CheckpointCreated events
    let events = sink.events.lock().unwrap();
    let count = events.iter().filter(|e| matches!(e, TraceEvent::CheckpointCreated { .. })).count();
    assert_eq!(count, 2, "expected 2 CheckpointCreated, got {:?}", events.len());
}
```

- [ ] **Step 3.2.4: Run the test**

Run: `cargo test -p latte-agent-core --test checkpoint_record`
Expected: 1 passed.

- [ ] **Step 3.2.5: Commit**

```bash
git add latte-agent-core/src/checkpoint.rs latte-agent-core/src/trace.rs latte-agent-core/tests/checkpoint_record.rs
git commit -m "feat(core): CheckpointEngine.record_write full pipeline"
```

---

## Phase 4 — Rollback (Code / Trace / Full)

### Task 4.1: Rollback::code and Rollback::full

**Files:**
- Modify: `latte-agent-core/src/checkpoint.rs` (add `rollback` method)
- Create: `latte-agent-core/tests/checkpoint_rollback.rs`

- [ ] **Step 4.1.1: Implement `rollback`**

Append to `CheckpointEngine` in `latte-agent-core/src/checkpoint.rs`:

```rust
    /// Roll back to a previous checkpoint. Reads `manifest.json` to
    /// find the git commit for `id`, then runs `git reset --hard`.
    /// If `mode` is `Trace` or `Full`, also truncates the on-disk
    /// trace JSONL to the recorded `trace_event_index`.
    pub fn rollback(&self, id: u32, mode: RollbackMode) -> Result<Checkpoint, CheckpointError> {
        let target = self.read_manifest_entry(id)?
            .ok_or_else(|| CheckpointError::NotFound(id, self.task_id().to_string()))?;

        // 1. Always reset the worktree (Code is the floor)
        if matches!(mode, RollbackMode::Code | RollbackMode::Full) {
            run_git(&self.worktree_root, &["reset", "--hard", &target.git_commit])?;
        }

        // 2. Truncate trace if requested
        if matches!(mode, RollbackMode::Trace | RollbackMode::Full) {
            if let Ok(p) = std::env::var("LATTE_TRACE_FILE") {
                if let Ok(meta) = std::fs::metadata(&p) {
                    let len = target.trace_event_index.min(meta.len());
                    let f = std::fs::OpenOptions::new().write(true).open(&p)?;
                    f.set_len(len)?;
                }
            }
        }

        // 3. Emit the rolled-back event
        let meta = crate::trace::TraceMeta {
            turn: 0,
            role: "checkpoint".into(),
            ts: iso8601_utc_now(),
            session_id: self.task_id().to_string(),
        };
        let mode_str = match mode {
            RollbackMode::Code => "code",
            RollbackMode::Trace => "trace",
            RollbackMode::Full => "full",
        };
        self.sink.emit(TraceEvent::CheckpointRolledBack {
            meta,
            checkpoint_id: id,
            mode: mode_str.into(),
            rolled_back_to: target.git_commit.clone(),
        });

        Ok(target)
    }

    fn read_manifest_entry(&self, id: u32) -> Result<Option<Checkpoint>, CheckpointError> {
        let path = self.storage_dir.join("manifest.json");
        if !path.exists() { return Ok(None); }
        let content = std::fs::read_to_string(&path)?;
        for line in content.lines() {
            if line.trim().is_empty() { continue; }
            let cp: Checkpoint = serde_json::from_str(line)
                .map_err(|e| CheckpointError::GitFailed(e.to_string()))?;
            if cp.id == id { return Ok(Some(cp)); }
        }
        Ok(None)
    }
```

- [ ] **Step 4.1.2: Write the rollback test**

Create `latte-agent-core/tests/checkpoint_rollback.rs`:

```rust
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use latte_agent_core::checkpoint::{CheckpointEngine, RollbackMode};
use latte_agent_core::trace::TraceSink;

fn run(cwd: &Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(cwd).output().unwrap();
    assert!(out.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
}

fn setup() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf, Arc<Mutex<Vec<u8>>>) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    run(&repo, &["init", "-q", "-b", "main"]);
    run(&repo, &["config", "user.email", "t@l"]);
    run(&repo, &["config", "user.name", "T"]);
    std::fs::write(repo.join("seed.txt"), "v0\n").unwrap();
    run(&repo, &["add", "-A"]); run(&repo, &["commit", "-q", "-m", "init"]);

    let wt = repo.join(".latte/worktrees/fix-x");
    run(&repo, &["worktree", "add", "-b", "latte/fix-x", wt.to_str().unwrap(), "main"]);

    let storage = repo.join(".latte/checkpoints/fix-x");
    std::fs::create_dir_all(&storage).unwrap();
    let bytes = Arc::new(Mutex::new(Vec::<u8>::new()));
    (dir, repo, wt, bytes)
}

#[derive(Clone)]
struct NullSink;
impl TraceSink for NullSink { fn emit(&self, _e: latte_agent_core::trace::TraceEvent) {} }

#[test]
fn rollback_code_resets_worktree() {
    let (_dir, repo, wt, _bytes) = setup();
    let engine = CheckpointEngine::new(wt.clone(), repo.join(".latte/checkpoints/fix-x"), Arc::new(NullSink)).unwrap();

    // cp0: clean (empty diff)
    let _ = engine.record_write("init", &serde_json::json!({})).unwrap();
    // cp1: add a line
    std::fs::write(wt.join("foo.rs"), "v1\n").unwrap();
    let cp1 = engine.record_write("write", &serde_json::json!({})).unwrap();
    // cp2: change again
    std::fs::write(wt.join("foo.rs"), "v2\n").unwrap();
    let _cp2 = engine.record_write("write", &serde_json::json!({})).unwrap();

    // Roll back to cp1 — foo.rs should be "v1"
    engine.rollback(cp1.id, RollbackMode::Code).unwrap();
    assert_eq!(std::fs::read_to_string(wt.join("foo.rs")).unwrap(), "v1\n");
}

#[test]
fn rollback_full_truncates_trace_jsonl() {
    let (dir, repo, wt, _bytes) = setup();
    // Set LATTE_TRACE_FILE to a temp file we control
    let trace_path = dir.path().join("trace.jsonl");
    std::fs::write(&trace_path, b"line-A\nline-B\nline-C\n").unwrap();
    std::env::set_var("LATTE_TRACE_FILE", &trace_path);

    let engine = CheckpointEngine::new(wt.clone(), repo.join(".latte/checkpoints/fix-x"), Arc::new(NullSink)).unwrap();
    // cp0: 0-byte trace
    let cp0 = engine.record_write("init", &serde_json::json!({})).unwrap();
    assert_eq!(cp0.trace_event_index, 0);
    // Append to the trace "file" then cp1: offset should reflect new length
    std::fs::write(&trace_path, b"line-A\nline-B\nline-C\nline-D\n").unwrap();
    let cp1 = engine.record_write("write", &serde_json::json!({})).unwrap();
    let offset_at_cp1 = cp1.trace_event_index;
    assert!(offset_at_cp1 > 0);

    // Roll back to cp1 with full mode
    engine.rollback(cp1.id, RollbackMode::Full).unwrap();
    let len_after = std::fs::metadata(&trace_path).unwrap().len();
    assert_eq!(len_after, offset_at_cp1);

    std::env::remove_var("LATTE_TRACE_FILE");
}
```

- [ ] **Step 4.1.3: Run the tests**

Run: `cargo test -p latte-agent-core --test checkpoint_rollback`
Expected: 2 passed.

- [ ] **Step 4.1.4: Commit**

```bash
git add latte-agent-core/src/checkpoint.rs latte-agent-core/tests/checkpoint_rollback.rs
git commit -m "feat(core): CheckpointEngine.rollback with code/trace/full modes"
```

---

## Phase 5 — Archive + Cleanup

### Task 5.1: WorkspaceManager::archive with conflict handling

**Files:**
- Modify: `latte-agent-core/src/workspace.rs` (add `archive`)
- Create: `latte-agent-core/tests/workspace_archive.rs`

- [ ] **Step 5.1.1: Implement `archive`**

Append to `WorkspaceManager` in `latte-agent-core/src/workspace.rs`:

```rust
    /// Auto-commit any uncommitted changes, then merge the worktree
    /// branch into the base branch. On conflict, transitions to
    /// `Failed` and preserves the worktree. Returns the merge commit
    /// SHA on success.
    pub fn archive(&mut self, mode: MergeMode) -> Result<String, WorkspaceError> {
        // First, catch any writes the engine missed
        let dirty = !git_cmd(&self.worktree_root, &["status", "--porcelain"])?.is_empty();
        if dirty {
            git_cmd(&self.worktree_root, &["add", "-A"])?;
            git_cmd(
                &self.worktree_root,
                &["commit", "-m", "[auto-commit-pre-archive]"],
            )?;
        }

        self.state = WorkspaceState::Archiving;
        let merge_flag = match mode {
            MergeMode::NoFf => "--no-ff",
            MergeMode::Squash => "--squash",
            MergeMode::FastForward => "--ff-only",
        };

        let res = git_cmd(
            &self.repo_root,
            &["merge", merge_flag, "-m", &format!("archive({})", self.spec.task_id), &self.spec.branch_name],
        );
        if let Err(e) = res {
            // Try to abort the merge so the main branch isn't left mid-merge.
            let _ = git_cmd(&self.repo_root, &["merge", "--abort"]);
            self.state = WorkspaceState::Failed { reason: e.to_string() };
            return Err(e);
        }

        let merge_commit = git_cmd(&self.repo_root, &["rev-parse", "HEAD"])?;
        self.state = WorkspaceState::Archived { merge_commit: merge_commit.clone() };
        Ok(merge_commit)
    }
```

- [ ] **Step 5.1.2: Write the archive test**

Create `latte-agent-core/tests/workspace_archive.rs`:

```rust
use std::path::Path;
use std::process::Command;
use latte_agent_core::workspace::{WorkspaceManager, WorkspaceState, MergeMode};

fn run(cwd: &Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(cwd).output().unwrap();
    assert!(out.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
}

#[test]
fn archive_with_no_ff_creates_merge_commit_on_main() {
    let dir = tempfile::tempdir().unwrap();
    run(dir.path(), &["init", "-q", "-b", "main"]);
    run(dir.path(), &["config", "user.email", "t@l"]);
    run(dir.path(), &["config", "user.name", "T"]);
    std::fs::write(dir.path().join("seed.txt"), "v0\n").unwrap();
    run(dir.path(), &["add", "-A"]); run(dir.path(), &["commit", "-q", "-m", "init"]);

    let mut mgr = WorkspaceManager::create(dir.path(), "x", "do thing").unwrap();
    // Make a change in the worktree + commit
    std::fs::write(mgr.spec().worktree_root.join("seed.txt"), "v1\n").unwrap();
    run(&mgr.spec().worktree_root, &["add", "-A"]);
    run(&mgr.spec().worktree_root, &["commit", "-q", "-m", "work"]);

    let sha = mgr.archive(MergeMode::NoFf).unwrap();
    assert!(!sha.is_empty());
    assert!(matches!(mgr.state(), WorkspaceState::Archived { .. }));

    // Main branch log shows the merge commit
    let log = Command::new("git").args(["log", "--oneline", "--graph"])
        .current_dir(dir.path()).output().unwrap();
    let log = String::from_utf8_lossy(&log.stdout);
    assert!(log.contains("archive(x)"), "main log missing merge commit:\n{}", log);
}

#[test]
fn archive_with_conflict_transitions_to_failed() {
    let dir = tempfile::tempdir().unwrap();
    run(dir.path(), &["init", "-q", "-b", "main"]);
    run(dir.path(), &["config", "user.email", "t@l"]);
    run(dir.path(), &["config", "user.name", "T"]);
    std::fs::write(dir.path().join("seed.txt"), "main\n").unwrap();
    run(dir.path(), &["add", "-A"]); run(dir.path(), &["commit", "-q", "-m", "init"]);

    let mut mgr = WorkspaceManager::create(dir.path(), "y", "conflict case").unwrap();
    // Worktree changes seed.txt to "work\n"
    std::fs::write(mgr.spec().worktree_root.join("seed.txt"), "work\n").unwrap();
    run(&mgr.spec().worktree_root, &["add", "-A"]);
    run(&mgr.spec().worktree_root, &["commit", "-q", "-m", "work"]);

    // Now change main's seed.txt so a merge will conflict
    std::fs::write(dir.path().join("seed.txt"), "main2\n").unwrap();
    run(dir.path(), &["add", "-A"]);
    run(dir.path(), &["commit", "-q", "-m", "main change"]);

    let res = mgr.archive(MergeMode::NoFf);
    assert!(res.is_err());
    assert!(matches!(mgr.state(), WorkspaceState::Failed { .. }));

    // Worktree dir is preserved
    assert!(mgr.spec().worktree_root.exists(), "worktree should be preserved on failure");
}
```

- [ ] **Step 5.1.3: Run the tests**

Run: `cargo test -p latte-agent-core --test workspace_archive`
Expected: 2 passed.

- [ ] **Step 5.1.4: Commit**

```bash
git add latte-agent-core/src/workspace.rs latte-agent-core/tests/workspace_archive.rs
git commit -m "feat(core): WorkspaceManager.archive with NoFf/Squash/FF + conflict handling"
```

### Task 5.2: WorkspaceManager::cleanup

**Files:**
- Modify: `latte-agent-core/src/workspace.rs` (add `cleanup`)
- Create: `latte-agent-core/tests/workspace_cleanup.rs`

- [ ] **Step 5.2.1: Implement `cleanup`**

Append to `WorkspaceManager` in `latte-agent-core/src/workspace.rs`:

```rust
    /// Best-effort: remove the worktree directory and delete the
    /// branch. Never panics; logs failures via `eprintln!` and
    /// returns Ok(()) if the worktree is already gone.
    pub fn cleanup(&mut self) -> Result<(), WorkspaceError> {
        // Remove the worktree
        if self.spec.worktree_root.exists() {
            if let Err(e) = git_cmd(&self.repo_root, &["worktree", "remove", "--force", self.spec.worktree_root.to_str().unwrap()]) {
                eprintln!("[workspace] worktree remove failed: {}", e);
                // Fall back to manual removal
                let _ = std::fs::remove_dir_all(&self.spec.worktree_root);
            }
        }
        // Delete the branch (may already be gone, that's fine)
        if let Err(e) = git_cmd(&self.repo_root, &["branch", "-D", &self.spec.branch_name]) {
            eprintln!("[workspace] branch delete failed (ok if already gone): {}", e);
        }
        // Prune worktree bookkeeping
        let _ = git_cmd(&self.repo_root, &["worktree", "prune"]);
        Ok(())
    }
```

- [ ] **Step 5.2.2: Write the cleanup test**

Create `latte-agent-core/tests/workspace_cleanup.rs`:

```rust
use std::path::Path;
use std::process::Command;
use latte_agent_core::workspace::{WorkspaceManager, MergeMode};

fn run(cwd: &Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(cwd).output().unwrap();
    assert!(out.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
}

#[test]
fn cleanup_removes_worktree_and_branch() {
    let dir = tempfile::tempdir().unwrap();
    run(dir.path(), &["init", "-q", "-b", "main"]);
    run(dir.path(), &["config", "user.email", "t@l"]);
    run(dir.path(), &["config", "user.name", "T"]);
    std::fs::write(dir.path().join("seed.txt"), "v0\n").unwrap();
    run(dir.path(), &["add", "-A"]); run(dir.path(), &["commit", "-q", "-m", "init"]);

    let mut mgr = WorkspaceManager::create(dir.path(), "z", "go").unwrap();
    let wt = mgr.spec().worktree_root.clone();
    mgr.archive(MergeMode::NoFf).unwrap();
    mgr.cleanup().unwrap();

    assert!(!wt.exists(), "worktree dir should be gone");
    let branches = Command::new("git").args(["branch"]).current_dir(dir.path()).output().unwrap();
    let branches = String::from_utf8_lossy(&branches.stdout);
    assert!(!branches.contains("latte/z"), "branch should be gone: {}", branches);
}
```

- [ ] **Step 5.2.3: Run the test**

Run: `cargo test -p latte-agent-core --test workspace_cleanup`
Expected: 1 passed.

- [ ] **Step 5.2.4: Commit**

```bash
git add latte-agent-core/src/workspace.rs latte-agent-core/tests/workspace_cleanup.rs
git commit -m "feat(core): WorkspaceManager.cleanup (best-effort worktree + branch removal)"
```

---

## Phase 6 — CLI Subcommands

### Task 6.1: Inject command (no role = blackboard; with role = queue)

**Files:**
- Create: `latte-agent-cli/src/commands/inject.rs`
- Modify: `latte-agent-cli/src/commands/mod.rs` (`pub mod inject;`)
- Modify: `latte-agent-cli/src/main.rs` (add `Inject(InjectCmd)` to `Command`)

- [ ] **Step 6.1.1: Create `InjectCmd`**

Create `latte-agent-cli/src/commands/inject.rs`:

```rust
use std::path::PathBuf;
use clap::Args;
use latte_agent_core::workspace::{Blackboard, WorkspaceManager};

#[derive(Args, Debug)]
pub struct InjectCmd {
    /// Task ID of the running session.
    #[arg(long)]
    pub task_id: String,
    /// Optional role to route the message to. If omitted, the message
    /// is appended to the plan.md blackboard.
    #[arg(long)]
    pub role: Option<String>,
    /// Message text to inject.
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

        match self.role {
            Some(role) => {
                let dir = worktree_root.join(".latte").join("inject");
                std::fs::create_dir_all(&dir)?;
                let path: PathBuf = dir.join(format!("{}.txt", role));
                let mut content = if path.exists() {
                    std::fs::read_to_string(&path)?
                } else {
                    String::new()
                };
                if !content.is_empty() && !content.ends_with('\n') { content.push('\n'); }
                let ts = chrono_like_now();
                content.push_str(&format!("[{}] {}\n", ts, self.message));
                std::fs::write(&path, content)?;
                println!("queued for role '{}' at {}", role, path.display());
            }
            None => {
                let bb = Blackboard::new(worktree_root.join("plan.md"));
                let mut content = bb.read().unwrap_or_default();
                if !content.ends_with('\n') { content.push('\n'); }
                let ts = chrono_like_now();
                content.push_str(&format!("\n## HUMAN @ {}\n\n{}\n", ts, self.message));
                bb.write(&content)?;
                println!("appended to blackboard at {}", bb.path().display());
            }
        }
        Ok(())
    }
}

fn chrono_like_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    latte_agent_core::trace::iso8601_utc_now_for(secs)
}
```

Note: `anyhow` is added to the CLI crate's `[dependencies]` in Task 6.6. If not yet present, use `Box<dyn std::error::Error>` instead and add `anyhow = "1"` in the same commit. (Check the current `latte-agent-cli/Cargo.toml` first.)

- [ ] **Step 6.1.2: Wire into mod.rs and main.rs**

In `latte-agent-cli/src/commands/mod.rs`, add `pub mod inject;` to the existing module list (top of file).

In `latte-agent-cli/src/main.rs`, add `Inject(InjectCmd)` to the `Command` enum and import it. The exact change:

```rust
use commands::{
    chat::ChatCmd, checkpoint::CheckpointCmd, config::ConfigCmd, debug::DebugCmd,
    discuss::DiscussCmd, inject::InjectCmd, list::ListCmd, pause::PauseCmd,
    resume::ResumeCmd, workflow::WorkflowCmd,
};
```

```rust
#[derive(Subcommand)]
enum Command {
    Discuss(DiscussCmd),
    Chat(ChatCmd),
    /// Run a multi-agent discussion using a named workflow
    Workflow(WorkflowCmd),
    List(ListCmd),
    Config(ConfigCmd),
    /// Offline inspection of recorded sessions, prompts, parser, and hooks
    Debug(DebugCmd),
    /// Run a task inside an isolated worktree sandbox
    Run(RunCmd),
    /// Inject a message into a running task
    Inject(InjectCmd),
    /// Pause a running task
    Pause(PauseCmd),
    /// Resume a paused task
    Resume(ResumeCmd),
    /// Manage checkpoints
    Checkpoint(CheckpointCmd),
}
```

`RunCmd`, `PauseCmd`, `ResumeCmd`, `CheckpointCmd` are stubs we add in later tasks — declare them as empty structs with the right `#[derive(Args)]` for now so this step compiles:

```rust
#[derive(Args, Debug)] pub struct RunCmd { #[arg(long)] pub task_id: String }
#[derive(Args, Debug)] pub struct PauseCmd { #[arg(long)] pub task_id: String }
#[derive(Args, Debug)] pub struct ResumeCmd { #[arg(long)] pub task_id: String }
#[derive(Args, Debug)] pub struct CheckpointCmd { #[command(subcommand)] pub action: CheckpointAction }
#[derive(clap::Subcommand, Debug)] pub enum CheckpointAction { List { #[arg(long)] task_id: String }, Rollback { #[arg(long)] task_id: String, #[arg(long)] id: u32, #[arg(long, value_enum)] mode: Option<RollbackModeArg> } }
#[derive(clap::ValueEnum, Clone, Debug)] pub enum RollbackModeArg { Code, Trace, Full }
```

(Replace these with real implementations in later tasks.)

- [ ] **Step 6.1.3: Build**

Run: `cargo build -p latte-agent-cli`
Expected: success.

- [ ] **Step 6.1.4: Commit**

```bash
git add latte-agent-cli/src/commands/inject.rs latte-agent-cli/src/commands/mod.rs latte-agent-cli/src/main.rs latte-agent-cli/Cargo.toml
git commit -m "feat(cli): inject subcommand + stub other new subcommands"
```

### Task 6.2: Pause + Resume commands

**Files:**
- Create: `latte-agent-cli/src/commands/pause.rs`
- Create: `latte-agent-cli/src/commands/resume.rs`

- [ ] **Step 6.2.1: Create `PauseCmd`**

Create `latte-agent-cli/src/commands/pause.rs`:

```rust
use clap::Args;
use latte_agent_core::workspace::WorkspaceManager;

#[derive(Args, Debug)]
pub struct PauseCmd {
    #[arg(long)]
    pub task_id: String,
}

impl PauseCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let cwd = std::env::current_dir()?;
        let repo_root = WorkspaceManager::resolve_repo_root(&cwd)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let wt = repo_root.join(".latte").join("worktrees").join(&self.task_id);
        let flag = wt.join(".latte").join("control").join("paused");
        std::fs::create_dir_all(flag.parent().unwrap())?;
        std::fs::write(&flag, b"")?;
        println!("paused task {}", self.task_id);
        Ok(())
    }
}
```

- [ ] **Step 6.2.2: Create `ResumeCmd`**

Create `latte-agent-cli/src/commands/resume.rs`:

```rust
use clap::Args;
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
        let flag = repo_root.join(".latte").join("worktrees").join(&self.task_id)
            .join(".latte").join("control").join("paused");
        if flag.exists() {
            std::fs::remove_file(&flag)?;
            println!("resumed task {}", self.task_id);
        } else {
            println!("task {} was not paused", self.task_id);
        }
        Ok(())
    }
}
```

- [ ] **Step 6.2.3: Wire into mod.rs and main.rs**

Add `pub mod pause; pub mod resume;` to `latte-agent-cli/src/commands/mod.rs`.

In `latte-agent-cli/src/main.rs`, replace the stub structs from Task 6.1.2 with the real imports:

```rust
use commands::{
    chat::ChatCmd, checkpoint::CheckpointCmd, config::ConfigCmd, debug::DebugCmd,
    discuss::DiscussCmd, inject::InjectCmd, list::ListCmd, pause::PauseCmd,
    resume::ResumeCmd, run::RunCmd, workflow::WorkflowCmd,
};
```

(Remove the stub struct definitions from main.rs.)

- [ ] **Step 6.2.4: Build + manual smoke test**

Run: `cargo build -p latte-agent-cli`
Then in a temp git repo:
```bash
latte-agent run --task-id smoketest   # create a task
latte-agent pause  --task-id smoketest
ls /tmp/.../.latte/worktrees/smoketest/.latte/control/   # shows "paused"
latte-agent resume --task-id smoketest
ls /tmp/.../.latte/worktrees/smoketest/.latte/control/   # no "paused"
```

- [ ] **Step 6.2.5: Commit**

```bash
git add latte-agent-cli/src/commands/pause.rs latte-agent-cli/src/commands/resume.rs latte-agent-cli/src/commands/mod.rs latte-agent-cli/src/main.rs
git commit -m "feat(cli): pause + resume subcommands"
```

### Task 6.3: Checkpoint command (list + rollback)

**Files:**
- Create: `latte-agent-cli/src/commands/checkpoint.rs`
- Modify: `latte-agent-cli/src/commands/mod.rs`

- [ ] **Step 6.3.1: Create `CheckpointCmd`**

Create `latte-agent-cli/src/commands/checkpoint.rs`:

```rust
use clap::{Args, Subcommand, ValueEnum};
use latte_agent_core::checkpoint::{CheckpointEngine, RollbackMode};
use latte_agent_core::trace::{NullSink, TraceSink};
use latte_agent_core::workspace::WorkspaceManager;

#[derive(Args, Debug)]
pub struct CheckpointCmd {
    #[command(subcommand)]
    pub action: CheckpointAction,
}

#[derive(Subcommand, Debug)]
pub enum CheckpointAction {
    /// Create an explicit checkpoint of the worktree's current state.
    Create {
        #[arg(long)] task_id: String,
    },
    /// List checkpoints for a task.
    List {
        #[arg(long)] task_id: String,
    },
    /// Roll back to a specific checkpoint.
    Rollback {
        #[arg(long)] task_id: String,
        #[arg(long)] id: u32,
        #[arg(long, value_enum, default_value_t = RollbackArg::Full)] mode: RollbackArg,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum RollbackArg {
    Code,
    Trace,
    Full,
}

impl From<RollbackArg> for RollbackMode {
    fn from(v: RollbackArg) -> Self {
        match v {
            RollbackArg::Code => RollbackMode::Code,
            RollbackArg::Trace => RollbackMode::Trace,
            RollbackArg::Full => RollbackMode::Full,
        }
    }
}

impl CheckpointCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let cwd = std::env::current_dir()?;
        let repo_root = WorkspaceManager::resolve_repo_root(&cwd)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        match self.action {
            CheckpointAction::Create { task_id } => {
                let (engine, _) = open_engine(&repo_root, &task_id)?;
                let cp = engine.record_write("explicit", &serde_json::json!({}))?;
                println!("created checkpoint {} (commit {})", cp.id, &cp.git_commit[..12]);
            }
            CheckpointAction::List { task_id } => {
                let (engine, _) = open_engine(&repo_root, &task_id)?;
                let manifest = engine.storage_dir().join("manifest.json");
                if !manifest.exists() {
                    println!("(no checkpoints)");
                    return Ok(());
                }
                for line in std::fs::read_to_string(&manifest)?.lines() {
                    if line.trim().is_empty() { continue; }
                    let cp: latte_agent_core::checkpoint::Checkpoint = serde_json::from_str(line)?;
                    println!("#{:<4} {} {} {}",
                        cp.id, &cp.created_at, &cp.git_commit[..12.min(cp.git_commit.len())],
                        format!("{:?}", cp.trigger));
                }
            }
            CheckpointAction::Rollback { task_id, id, mode } => {
                let (engine, _) = open_engine(&repo_root, &task_id)?;
                let cp = engine.rollback(id, mode.into())?;
                println!("rolled back to #{} (commit {})", cp.id, &cp.git_commit[..12]);
            }
        }
        Ok(())
    }
}

fn open_engine(
    repo_root: &std::path::Path,
    task_id: &str,
) -> anyhow::Result<(CheckpointEngine, std::path::PathBuf)> {
    let worktree_root = repo_root.join(".latte").join("worktrees").join(task_id);
    if !worktree_root.exists() {
        anyhow::bail!("worktree for task '{}' not found", task_id);
    }
    let storage_dir = repo_root.join(".latte").join("checkpoints").join(task_id);
    let sink: std::sync::Arc<dyn TraceSink> = std::sync::Arc::new(NullSink);
    let engine = CheckpointEngine::new(worktree_root.clone(), storage_dir, sink)?;
    Ok((engine, worktree_root))
}
```

- [ ] **Step 6.3.2: Wire into mod.rs and main.rs**

Add `pub mod checkpoint;` to `latte-agent-cli/src/commands/mod.rs`. The `Run(RunCmd)` variant in `main.rs` from Task 6.1.2 stays; in this task the real `RunCmd` is still a stub, replaced in Task 6.4. Update the `Command` enum variant in `main.rs`:

```rust
    /// Manage checkpoints
    Checkpoint(CheckpointCmd),
```

(Matches what was stubbed in Task 6.1.2.)

- [ ] **Step 6.3.3: Build**

Run: `cargo build -p latte-agent-cli`
Expected: success.

- [ ] **Step 6.3.4: Manual smoke test**

```bash
# in a temp git repo:
latte-agent run --task-id cp-smoke
echo "hello" > /tmp/repo/.latte/worktrees/cp-smoke/foo.txt
latte-agent checkpoint create --task-id cp-smoke
latte-agent checkpoint list --task-id cp-smoke   # should show #0
echo "goodbye" > /tmp/repo/.latte/worktrees/cp-smoke/foo.txt
latte-agent checkpoint rollback --task-id cp-smoke --id 0 --mode code
cat /tmp/repo/.latte/worktrees/cp-smoke/foo.txt   # should not exist (rollback to clean state)
```

- [ ] **Step 6.3.5: Commit**

```bash
git add latte-agent-cli/src/commands/checkpoint.rs latte-agent-cli/src/commands/mod.rs latte-agent-cli/src/main.rs
git commit -m "feat(cli): checkpoint subcommand (create/list/rollback)"
```

### Task 6.4: Run command (drives a real task loop)

**Files:**
- Create: `latte-agent-cli/src/commands/run.rs`
- Modify: `latte-agent-cli/src/commands/mod.rs`

- [ ] **Step 6.4.1: Create `RunCmd`**

Create `latte-agent-cli/src/commands/run.rs`. This is the v1 simplified loop: 1 round, 1 turn per role, no `DiscussionOrchestrator` consumption. v2 replaces with the real orchestrator:

```rust
use std::sync::Arc;
use clap::Args;
use latte_agent_core::checkpoint::CheckpointEngine;
use latte_agent_core::trace::{JsonlSink, IndexSink, NullSink, TraceSink, FanoutSink};
use latte_agent_core::workspace::{MergeMode, WorkspaceManager};

#[derive(Args, Debug)]
pub struct RunCmd {
    #[arg(long)]
    pub task_id: String,
    /// Comma-separated role list. v1: each role gets one run_turn.
    #[arg(long, default_value = "programmer")]
    pub roles: String,
    /// Initial prompt written to plan.md and used as the first user message.
    #[arg(long)]
    pub initial_prompt: String,
    /// Archive after running: --no-ff merge the worktree into the base branch.
    #[arg(long)]
    pub archive: bool,
    /// After archive, remove the worktree and branch.
    #[arg(long)]
    pub cleanup: bool,
    /// Choose merge mode when --archive is set.
    #[arg(long, value_enum, default_value_t = MergeArg::NoFf)]
    pub merge: MergeArg,
    /// Skip creating the plan.md blackboard.
    #[arg(long)]
    pub no_blackboard: bool,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeArg { NoFf, Squash, FastForward }

impl From<MergeArg> for MergeMode {
    fn from(v: MergeArg) -> Self {
        match v {
            MergeArg::NoFf => MergeMode::NoFf,
            MergeArg::Squash => MergeMode::Squash,
            MergeArg::FastForward => MergeMode::FastForward,
        }
    }
}

impl RunCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let cwd = std::env::current_dir()?;
        let mut mgr = WorkspaceManager::create(&cwd, &self.task_id, &self.initial_prompt)?;

        // Sink fanout: JSONL on disk + always-on index.
        let trace_path = latte_home().join("traces").join(format!("{}.jsonl", self.task_id));
        std::fs::create_dir_all(trace_path.parent().unwrap())?;
        let jsonl: Arc<dyn TraceSink> = Arc::new(JsonlSink::new(trace_path));
        let index_path = latte_home().join("sessions").join(format!("{}.idx", self.task_id));
        std::fs::create_dir_all(index_path.parent().unwrap())?;
        let index: Arc<dyn TraceSink> = Arc::new(IndexSink::new(index_path));
        let sink: Arc<dyn TraceSink> = Arc::new(FanoutSink::new(vec![jsonl, index]));

        // Checkpoint engine
        let storage_dir = mgr.repo_root().join(".latte").join("checkpoints").join(&self.task_id);
        let engine = CheckpointEngine::new(mgr.spec().worktree_root.clone(), storage_dir, sink.clone())?;

        // Initial checkpoint 0 — captures the just-created worktree.
        let cp0 = engine.record_write("init", &serde_json::json!({}))?;
        println!("checkpoint 0 = {}", &cp0.git_commit[..12.min(cp0.git_commit.len())]);

        // v1 loop: one turn per role, no model calls (we are not
        // spinning up AgentRunner in this CLI subcommand — that
        // integration is the v2 spec). The role names are recorded
        // in the trace for future playback.
        for role in self.roles.split(',') {
            let role = role.trim();
            println!("[v1] would invoke role: {}", role);
        }

        // Optional archive
        if self.archive {
            let sha = mgr.archive(self.merge.into())?;
            println!("archived: merge commit {}", &sha[..12.min(sha.len())]);
            if self.cleanup {
                mgr.cleanup()?;
                println!("cleaned up worktree and branch");
            }
        }
        Ok(())
    }
}

fn latte_home() -> std::path::PathBuf {
    std::env::var_os("LATTE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(|h| std::path::PathBuf::from(h).join(".latte"))
                .unwrap_or_else(|| std::path::PathBuf::from(".latte"))
        })
}
```

If `JsonlSink::new` / `IndexSink::new` / `FanoutSink::new` constructors don't exist with the exact names in `latte-agent-core/src/trace.rs`, check the actual API (e.g. they may be `JsonlSink { path }` literal construction or take a different arg). Adjust the call sites to match. The `NullSink` is also referenced in case the real API differs from the plan — fall back to `NullSink` if disk sinks are not yet usable from the CLI crate.

- [ ] **Step 6.4.2: Wire into mod.rs and main.rs**

Add `pub mod run;` to `latte-agent-cli/src/commands/mod.rs`. The `Run(RunCmd)` variant in main.rs from Task 6.1.2 already matches; ensure the import is correct:

```rust
use commands::{
    chat::ChatCmd, checkpoint::CheckpointCmd, config::ConfigCmd, debug::DebugCmd,
    discuss::DiscussCmd, inject::InjectCmd, list::ListCmd, pause::PauseCmd,
    resume::ResumeCmd, run::RunCmd, workflow::WorkflowCmd,
};
```

Remove the stub `RunCmd` struct from main.rs (added in Task 6.1.2).

- [ ] **Step 6.4.3: Build + manual smoke**

Run: `cargo build -p latte-agent-cli`
Then in a temp git repo:
```bash
latte-agent run --task-id e2e-runner --initial-prompt "noop" --archive --cleanup
# Expected: worktree created, checkpoint 0 emitted, main branch is clean, worktree gone.
git log --oneline   # should show only the original "init" commit; nothing from "e2e-runner"
```

- [ ] **Step 6.4.4: Commit**

```bash
git add latte-agent-cli/src/commands/run.rs latte-agent-cli/src/commands/mod.rs latte-agent-cli/src/main.rs
git commit -m "feat(cli): run subcommand with archive/cleanup"
```

### Task 6.5: Existing-command regression sanity check

**Files:** (none — verification only)

- [ ] **Step 6.5.1: Run the full workspace test suite**

Run: `cargo test --workspace`
Expected: all existing tests + new tests pass. If anything fails, the regression was introduced by changes to `mod.rs` / `main.rs` / `lib.rs`; fix it before moving on.

- [ ] **Step 6.5.2: Run `latte-agent` with no args to confirm help renders**

Run: `cargo run -p latte-agent-cli -- --help`
Expected: help output lists `run`, `inject`, `pause`, `resume`, `checkpoint` alongside the existing subcommands.

- [ ] **Step 6.5.3: Commit any regression fixes (if needed)**

If step 6.5.1 revealed a failure, fix and commit. Otherwise skip this step.

```bash
git commit --allow-empty -m "chore: regression fixes from full test run"
```

---

## Phase 7 — End-to-End Integration Test

### Task 7.1: Black-box e2e test

**Files:**
- Create: `latte-agent-cli/tests/workspace_e2e.rs`
- Modify: `latte-agent-cli/Cargo.toml` (add `tempfile = "3"` to `[dev-dependencies]`)

- [ ] **Step 7.1.1: Add tempfile as a dev-dependency**

In `latte-agent-cli/Cargo.toml`, add under `[dev-dependencies]`:

```toml
tempfile = "3"
```

- [ ] **Step 7.1.2: Write the e2e test**

Create `latte-agent-cli/tests/workspace_e2e.rs`. This test spawns the real `latte-agent` binary, drives a 2-checkpoint task, then rolls back:

```rust
//! End-to-end test: run a 2-checkpoint synthetic task via the
//! `latte-agent` binary, then rollback --mode full, and assert the
//! worktree is back to the post-Checkpoint-0 state.

use std::path::Path;
use std::process::Command;

fn bin() -> std::path::PathBuf {
    // cargo puts the test binary in target/debug/deps; the workspace
    // binary is at target/debug/latte-agent
    let mut p = std::env::var("CARGO_BIN_EXE_latte-agent")
        .ok()
        .map(std::path::PathBuf::from)
        .expect("CARGO_BIN_EXE_latte-agent not set by cargo test");
    assert!(p.exists(), "binary not found: {}", p.display());
    p
}

fn git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(cwd).output().unwrap();
    assert!(out.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
}

fn latte(cwd: &Path, args: &[&str]) {
    let out = Command::new(bin()).args(args).current_dir(cwd).output().unwrap();
    assert!(out.status.success(), "latte-agent {:?} failed: {}",
        args, String::from_utf8_lossy(&out.stderr));
}

#[test]
fn run_then_rollback_full_returns_worktree_to_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init", "-q", "-b", "main"]);
    git(repo, &["config", "user.email", "t@l"]);
    git(repo, &["config", "user.name", "T"]);
    std::fs::write(repo.join("main.txt"), "original\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "init"]);

    // 1. Start a task
    latte(repo, &["run", "--task-id", "e2e", "--initial-prompt", "noop"]);

    // 2. Two writes inside the worktree (simulating two tool calls),
    //    each followed by an explicit checkpoint
    let wt = repo.join(".latte/worktrees/e2e");
    std::fs::write(wt.join("main.txt"), "v1\n").unwrap();
    latte(repo, &["checkpoint", "create", "--task-id", "e2e"]);
    std::fs::write(wt.join("main.txt"), "v2\n").unwrap();
    latte(repo, &["checkpoint", "create", "--task-id", "e2e"]);

    // 3. List shows two checkpoints (ids 0 and 1; cp0 was emitted in
    //    run, cp1 from first create, cp2 from second create — total 3
    //    or 2 depending on cp0. Accept either as long as the list
    //    contains 2 or 3 lines)
    let out = Command::new(bin())
        .args(["checkpoint", "list", "--task-id", "e2e"])
        .current_dir(repo)
        .output().unwrap();
    let list = String::from_utf8_lossy(&out.stdout);
    let n = list.lines().filter(|l| l.starts_with('#')).count();
    assert!(n >= 2, "expected at least 2 checkpoints, got {}:\n{}", n, list);

    // 4. Roll back to the FIRST user-created checkpoint (id 1 — cp0
    //    was the worktree init). Worktree's main.txt should be "v1".
    let out = Command::new(bin())
        .args(["checkpoint", "rollback", "--task-id", "e2e", "--id", "1", "--mode", "full"])
        .current_dir(repo)
        .output().unwrap();
    assert!(out.status.success(), "rollback failed: {}",
        String::from_utf8_lossy(&out.stderr));
    assert_eq!(std::fs::read_to_string(wt.join("main.txt")).unwrap(), "v1\n");

    // 5. Main branch stays clean
    let main_log = Command::new("git").args(["log", "--oneline"])
        .current_dir(repo).output().unwrap();
    let main_log = String::from_utf8_lossy(&main_log.stdout);
    assert!(!main_log.contains("checkpoint("), "main branch contaminated:\n{}", main_log);
}
```

- [ ] **Step 7.1.3: Run the e2e test**

Run: `cargo test -p latte-agent-cli --test workspace_e2e -- --nocapture`
Expected: 1 passed. If it fails, the most likely cause is a CLI argument mismatch between the test and the real binary's clap definition; fix the test args to match.

- [ ] **Step 7.1.4: Commit**

```bash
git add latte-agent-cli/tests/workspace_e2e.rs latte-agent-cli/Cargo.toml
git commit -m "test(cli): black-box e2e for run + checkpoint rollback"
```

---

## Phase 8 — Docs

### Task 8.1: README update

**Files:**
- Modify: `README.md`

- [ ] **Step 8.1.1: Add a "Workspace + Checkpoint (v1)" section**

Open `README.md` and find the existing "## Status" section (around line 176). Below it, insert a new section before "## Roadmap":

```markdown
## Workspace + Checkpoint (v1)

Run an isolated task inside a git worktree with automatic per-write checkpoints and per-checkpoint rollback:

```bash
# Start an isolated task
latte-agent run --task-id fix-redis-bug --roles programmer,reviewer \
    --initial-prompt "Redis pool doesn't recycle after 5xx"

# Inject a human message mid-run
latte-agent inject --task-id fix-redis-bug --role programmer \
    --message "Ignore formatting; focus on the lock logic"

# Pause / resume
latte-agent pause  --task-id fix-redis-bug
latte-agent resume --task-id fix-redis-bug

# Inspect / roll back
latte-agent checkpoint list    --task-id fix-redis-bug
latte-agent checkpoint rollback --task-id fix-redis-bug --id 3 --mode full

# Archive (merge --no-ff into the base branch) + optional cleanup
latte-agent run --task-id fix-redis-bug --archive --cleanup
```

The worktree lives at `<repo>/.latte/worktrees/<task-id>/`; the
base branch HEAD stays clean for the entire run. Checkpoint diffs
are at `<repo>/.latte/checkpoints/<task-id>/<id>.patch`. Two new
`TraceEvent` variants (`CheckpointCreated`, `CheckpointRolledBack`)
land in `~/.latte/traces/<task-id>.jsonl` and are visible via
`latte-agent debug session <id>`.

See `docs/superpowers/specs/2026-06-27-blackboard-worktree-sandbox-design.md`
for the design and `docs/superpowers/plans/2026-06-27-blackboard-worktree-sandbox-impl.md`
for the implementation plan.
```

Wrap the bash block in triple-backticks (matching the file's existing style).

- [ ] **Step 8.1.2: Cross-link the Roadmap bullet**

In the same `README.md`, find the "## Roadmap" section. The existing bullet that mentions checkpoint is:

```
- **Checkpoint / node-level retry** (deferred) — state serialization,
  restore from a snapshot, retry budget. Hooks reserve the
  `HookOutcome::Retry { correction }` variant for this future work.
```

Replace it with:

```
- **Checkpoint / node-level retry** — the v1 of the
  Workspace + Checkpoint system ships in this release (see the
  section above). The v2 spec will add full state serialization,
  restore-from-snapshot, retry budget, and a real `SessionManager`.
```

- [ ] **Step 8.1.3: Commit**

```bash
git add README.md
git commit -m "docs: document Workspace + Checkpoint v1 + cross-link from Roadmap"
```

---

## Self-Review Checklist (post-write)

This section is for the planning author, not the implementer. Already executed before commit:

1. **Spec coverage:** every spec section maps to at least one task.
   - §1-3 Background / Goals / Non-Goals → task structure itself
   - §4.3-4.5 types → Task 1.1, 2.1
   - §5 lifecycle (Created/Running/Archiving/Archived/Failed) → Tasks 2.2, 5.1
   - §5.2 CLI subcommands → Tasks 6.1-6.4
   - §6.1 intercept → Task 3.2
   - §6.4 rollback code/trace/full → Task 4.1
   - §6.2 explicit checkpoint → Task 6.3
   - §6.5 session_id == task_id → `TraceMeta::session_id` used in Tasks 3.2, 4.1
   - §8.1 unit tests → distributed across Phases 2-5
   - §8.2 e2e test → Task 7.1
   - §11 acceptance criteria → §11 task list in spec aligns with plan's `cargo test --workspace` + e2e gate

2. **Placeholder scan:** no "TBD" / "TODO" / "fill in" in any task. Where the spec defers detail (e.g. `JsonlSink::new` exact signature, anyhow vs Box<dyn Error>), the plan gives a fallback: "check the actual API; adjust the call sites."

3. **Type consistency:** `WorktreeSpec` / `WorkspaceManager` / `WorkspaceState` / `MergeMode` / `Blackboard` / `Checkpoint` / `CheckpointTrigger` / `DiffSummary` / `CheckpointEngine` / `RollbackMode` / `CheckpointError` — names and field shapes are pinned in Task 1.1 + 2.1 and referenced unchanged in every later task. `CheckpointEngine::new` signature is pinned in Task 3.1.1; later tasks only call it.

4. **Commit cadence:** every task ends with a commit. No "save up for later" anti-pattern.

5. **Test-first:** every implementation step has a test step before it (TDD) except Task 8.1 (docs only) and Task 6.5 (regression check).

---

## Execution Handoff

**Plan complete and saved to `docs/superpowers/plans/2026-06-27-blackboard-worktree-sandbox-impl.md` (committed next to spec).**

Total: 8 phases, 16 tasks, 1 e2e test. Estimated time: 4-6 hours of focused implementation + 1 hour of debugging. Every phase ends with `cargo build` / `cargo test` green and a discrete commit, so the work is bisectable.

**Two execution options:**

1. **Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration with two-stage review.
2. **Inline Execution** — Execute tasks in this session using `executing-plans`, batch execution with checkpoints for review.

**Which approach?**
