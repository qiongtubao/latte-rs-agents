//! `latte-agent resume` — remove the paused sentinel so the running
//! loop resumes issuing role turns. Idempotent: if the task was not
//! paused, this just prints a notice.

use clap::Args;
use latte_agent_core::workspace::WorkspaceManager;

/// Resume a paused task.
#[derive(Args, Debug)]
pub struct ResumeCmd {
    /// Task id whose worktree to resume.
    #[arg(long)]
    pub task_id: String,
}

impl ResumeCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let cwd = std::env::current_dir()?;
        let repo_root = WorkspaceManager::resolve_repo_root(&cwd)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let flag = repo_root
            .join(".latte")
            .join("worktrees")
            .join(&self.task_id)
            .join(".latte")
            .join("control")
            .join("paused");
        if flag.exists() {
            std::fs::remove_file(&flag)?;
            println!("resumed task {}", self.task_id);
        } else {
            println!("task {} was not paused", self.task_id);
        }
        Ok(())
    }
}