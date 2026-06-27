use clap::Args;
use crate::commands::role_injector::RoleInjector;
use latte_agent_core::workspace::WorkspaceManager;

#[derive(Args, Debug)]
pub struct InjectCmd {
    #[arg(long)]
    pub task_id: String,
    #[arg(long)]
    pub role: String,
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
        RoleInjector::queue_for(&worktree_root, &self.role, &self.message)?;
        println!("queued for role '{}' at <worktree>/.latte/inject/{}.txt", self.role, self.role);
        Ok(())
    }
}
