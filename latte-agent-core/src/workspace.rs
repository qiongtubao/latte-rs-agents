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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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