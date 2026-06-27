//! `latte-agent pause` — pause a running task by writing a sentinel
//! file under `<wt>/.latte/control/paused`. The running loop notices
//! the file and stops issuing new role turns; in-flight work is left
//! to complete.

use clap::Args;
use latte_agent_core::workspace::WorkspaceManager;

/// Pause a running task.
#[derive(Args, Debug)]
pub struct PauseCmd {
    /// Task id whose worktree to pause.
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