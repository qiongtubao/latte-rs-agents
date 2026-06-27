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
