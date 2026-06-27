//! Checkpoint types for the WorkspaceManager sandbox.
//!
//! A `Checkpoint` is a recorded moment in a worktree's history. It pairs
//! a git commit SHA with a unified diff on disk and a summary on the
//! `TraceEvent::CheckpointCreated` line that announced it.
//!
//! See `docs/superpowers/specs/2026-06-27-blackboard-worktree-sandbox-design.md` §4.4.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use std::path::PathBuf;
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
