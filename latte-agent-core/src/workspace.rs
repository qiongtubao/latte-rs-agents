//! WorkspaceManager: a git worktree per task, with a plan.md blackboard.
//!
//! See `docs/superpowers/specs/2026-06-27-blackboard-worktree-sandbox-design.md` §4.3-§5.

use std::path::{Path, PathBuf};
use std::fs;
use std::process::Command;
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
    /// `git rev-parse --show-toplevel` fails or returns empty.
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

        std::fs::create_dir_all(worktree_root.parent().unwrap())?;

        git_cmd(
            &repo_root,
            &["worktree", "add", "-b", &branch_name, worktree_root.to_str().unwrap(), &base_branch],
        )?;

        // Drop a .gitignore so the worktree's own .latte/ (checkpoints,
        // inject queue) doesn't get committed into its branch.
        let gi = worktree_root.join(".gitignore");
        std::fs::write(&gi, ".latte/\n")?;

        let blackboard = Blackboard::new(blackboard_path.clone());
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