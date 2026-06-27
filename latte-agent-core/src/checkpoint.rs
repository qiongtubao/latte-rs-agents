//! Checkpoint types for the WorkspaceManager sandbox.
//!
//! A `Checkpoint` is a recorded moment in a worktree's history. It pairs
//! a git commit SHA with a unified diff on disk and a summary on the
//! `TraceEvent::CheckpointCreated` line that announced it.
//!
//! See `docs/superpowers/specs/2026-06-27-blackboard-worktree-sandbox-design.md` §4.4.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::trace::{DiffSummary, TraceSink};

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
#[serde(rename_all = "snake_case")]
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

/// Wraps a worktree, intercepts write/edit calls, and produces
/// `Checkpoint`s. The engine owns its storage dir but does NOT
/// call into `WorkspaceManager` directly — the manager wires it up.
#[allow(dead_code)] // `sink`, `write_tools`, `enable_bash_capture`, and `next_id` are read by `record_write`/`rollback` in Tasks 3.2 & 4.1
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


impl CheckpointEngine {
    /// Capture the worktree's current dirty (or clean) state as a
    /// checkpoint. Always commits — even if no files changed — so the
    /// caller gets a monotonically increasing id sequence.
    pub fn record_write(
        &mut self,
        tool: &str,
        args: &serde_json::Value,
    ) -> Result<Checkpoint, CheckpointError> {
        use crate::trace::{TraceEvent, TraceMeta};

        let id = self.next_id;
        self.next_id = id + 1;
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
        let msg = format!(
            "checkpoint({}): {} {}",
            self.worktree_root_display(),
            tool,
            args_hash
        );
        run_git_allow_empty(&self.worktree_root, &["commit", "--allow-empty", "-m", &msg])?;
        let git_commit = run_git(&self.worktree_root, &["rev-parse", "HEAD"])?;

        // 5. trace byte offset — read after commit so the line is in the file
        let trace_event_index = current_trace_offset();

        // 6. manifest line
        let created_at = iso8601_utc_now_now();
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
    run_git(cwd, args).or(Ok(String::new()))
}

fn parse_shortstat(s: &str) -> DiffSummary {
    // Example: " 1 file changed, 5 insertions(+), 0 deletions(-)"
    let mut out = DiffSummary::default();
    let parts: Vec<&str> = s.split(',').map(|p| p.trim()).collect();
    for p in parts {
        if let Some(n) = p
            .split_whitespace()
            .next()
            .and_then(|x| x.parse::<usize>().ok())
        {
            if p.contains("file") {
                out.files_changed = n;
            } else if p.contains("insertion") {
                out.insertions = n;
            } else if p.contains("deletion") {
                out.deletions = n;
            }
        }
    }
    out
}

pub fn short_hash(s: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    let hex = format!("{:x}", h.finish());
    hex[..8].to_string()
}

fn current_trace_offset() -> u64 {
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
    let line = serde_json::to_string(cp).map_err(|e| CheckpointError::GitFailed(e.to_string()))?;
    use std::io::Write;
    writeln!(f, "{}", line)?;
    Ok(())
}

fn iso8601_utc_now_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    crate::trace::iso8601_utc_now_for(secs)
}

impl CheckpointEngine {
    /// Roll back to a previous checkpoint. Reads `manifest.json` to
    /// find the git commit for `id`, then runs `git reset --hard`.
    /// If `mode` is `Trace` or `Full`, also truncates the on-disk
    /// trace JSONL to the recorded `trace_event_index`.
    pub fn rollback(
        &self,
        id: u32,
        mode: RollbackMode,
    ) -> Result<Checkpoint, CheckpointError> {
        use crate::trace::TraceEvent;
        let target = self
            .read_manifest_entry(id)?
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
            ts: iso8601_utc_now_now(),
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
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)?;
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let cp: Checkpoint = serde_json::from_str(line)
                .map_err(|e| CheckpointError::GitFailed(e.to_string()))?;
            if cp.id == id {
                return Ok(Some(cp));
            }
        }
        Ok(None)
    }
}
