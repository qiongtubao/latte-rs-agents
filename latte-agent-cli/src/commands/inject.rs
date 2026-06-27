//! `latte-agent inject` — append a message to a running task.
//!
//! With `--role`, the message is queued for that role (per-role
//! append-only file under `<wt>/.latte/inject/<role>.txt`). Without
//! `--role`, the message is appended to the worktree's `plan.md`
//! blackboard under a timestamped `## HUMAN @ <ts>` heading.

use std::path::PathBuf;

use clap::Args;
use latte_agent_core::workspace::{Blackboard, WorkspaceManager};

/// Inject a message into a running task.
#[derive(Args, Debug)]
pub struct InjectCmd {
    /// Task id whose worktree we should inject into.
    #[arg(long)]
    pub task_id: String,
    /// If set, append to this role's queue; otherwise append to plan.md.
    #[arg(long)]
    pub role: Option<String>,
    /// Message body.
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
            anyhow::bail!(
                "worktree for task '{}' not found at {}",
                self.task_id,
                worktree_root.display()
            );
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
                if !content.is_empty() && !content.ends_with('\n') {
                    content.push('\n');
                }
                let ts = now_iso();
                content.push_str(&format!("[{}] {}\n", ts, self.message));
                std::fs::write(&path, content)?;
                println!("queued for role '{}' at {}", role, path.display());
            }
            None => {
                let bb = Blackboard::new(worktree_root.join("plan.md"));
                let mut content = bb.read().unwrap_or_default();
                if !content.is_empty() && !content.ends_with('\n') {
                    content.push('\n');
                }
                let ts = now_iso();
                content.push_str(&format!("\n## HUMAN @ {}\n\n{}\n", ts, self.message));
                bb.write(&content)?;
                println!("appended to blackboard at {}", bb.path().display());
            }
        }
        Ok(())
    }
}

fn now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    latte_agent_core::trace::iso8601_utc_now_for(secs)
}