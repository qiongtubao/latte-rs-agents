//! `latte-agent checkpoint` — manage explicit checkpoints for a task.
//!
//! Subcommands:
//!   create   — snapshot the worktree as a new checkpoint
//!   list     — print the manifest entries
//!   rollback — reset the worktree to a previous checkpoint id

use clap::{Args, Subcommand};
use latte_agent_core::checkpoint::{CheckpointEngine, RollbackMode};
use latte_agent_core::trace::{NullSink, TraceSink};
use latte_agent_core::workspace::WorkspaceManager;

/// Top-level checkpoint command. Its `action` field selects between
/// `create` / `list` / `rollback`.
#[derive(Args, Debug)]
pub struct CheckpointCmd {
    #[command(subcommand)]
    pub action: CheckpointAction,
}

/// Subcommand variants for `latte-agent checkpoint`.
#[derive(Subcommand, Debug)]
pub enum CheckpointAction {
    /// Create an explicit checkpoint of the worktree's current state.
    Create {
        #[arg(long)]
        task_id: String,
    },
    /// List checkpoints for a task.
    List {
        #[arg(long)]
        task_id: String,
    },
    /// Roll back to a specific checkpoint. v1 only supports `code`
    /// mode (the worktree is reset; `plan.md` and the trace JSONL
    /// are preserved per spec §三.3 "撤销代码,保留讨论").
    Rollback {
        #[arg(long)]
        task_id: String,
        #[arg(long)]
        id: u32,
    },
}

impl CheckpointCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let cwd = std::env::current_dir()?;
        let repo_root = WorkspaceManager::resolve_repo_root(&cwd)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        match self.action {
            CheckpointAction::Create { task_id } => {
                let (mut engine, _) = open_engine(&repo_root, &task_id)?;
                let cp = engine
                    .record_write("explicit", &serde_json::json!({}))
                    .map_err(anyhow::Error::from)?;
                let short = short_sha(&cp.git_commit);
                println!("created checkpoint {} (commit {})", cp.id, short);
            }
            CheckpointAction::List { task_id } => {
                let (engine, _) = open_engine(&repo_root, &task_id)?;
                let manifest = engine.storage_dir().join("manifest.json");
                if !manifest.exists() {
                    println!("(no checkpoints)");
                    return Ok(());
                }
                for line in std::fs::read_to_string(&manifest)?.lines() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let cp: latte_agent_core::checkpoint::Checkpoint =
                        serde_json::from_str(line)?;
                    println!(
                        "#{:<4} {} {} {}",
                        cp.id,
                        cp.created_at,
                        short_sha(&cp.git_commit),
                        format!("{:?}", cp.trigger)
                    );
                }
            }
            CheckpointAction::Rollback { task_id, id } => {
                let (engine, _) = open_engine(&repo_root, &task_id)?;
                let cp = engine.rollback(id, RollbackMode::Code).map_err(anyhow::Error::from)?;
                println!("rolled back to #{} (commit {})", cp.id, short_sha(&cp.git_commit));
            }
        }
        Ok(())
    }
}

/// Build a `CheckpointEngine` rooted at the task's worktree and
/// storage dir. Verifies the worktree exists so we fail fast with a
/// clear error before opening git.
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

/// First 12 chars of a sha, or the whole thing if shorter. Display
/// only; commits are 40 chars so this almost always yields the
/// short form.
fn short_sha(s: &str) -> &str {
    &s[..12.min(s.len())]
}
