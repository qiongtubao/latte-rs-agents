use clap::Args;
use latte_agent_core::session::SessionManager;
use latte_agent_core::workspace::WorkspaceManager;

#[derive(Args, Debug)]
pub struct ResumeCmd {
    #[arg(long)]
    pub task_id: String,
    /// Optional role id. When set, resumes only that individually-paused
    /// role (leaving the global session state untouched). When absent,
    /// resumes the whole session.
    #[arg(long)]
    pub role: Option<String>,
}

impl ResumeCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let cwd = std::env::current_dir()?;
        let repo_root =
            WorkspaceManager::resolve_repo_root(&cwd).map_err(|e| anyhow::anyhow!("{}", e))?;
        let worktree_root = repo_root
            .join(".latte")
            .join("worktrees")
            .join(&self.task_id);
        if !worktree_root.exists() {
            anyhow::bail!(
                "worktree for task '{}' not found at {}",
                self.task_id,
                worktree_root.display()
            );
        }
        let session = find_latest_session(&worktree_root, &self.task_id)?;
        let mut mgr = SessionManager::from_record(session, worktree_root);

        if let Some(role_id) = &self.role {
            if !mgr.record().roles.iter().any(|r| &r.role_id == role_id) {
                anyhow::bail!("role '{}' is not part of task '{}'", role_id, self.task_id);
            }
            if !mgr.is_role_paused(role_id) {
                anyhow::bail!(
                    "role '{}' of task '{}' is not paused",
                    role_id,
                    self.task_id
                );
            }
            mgr.resume_role(role_id)?;
            println!("resumed role {} of task {}", role_id, self.task_id);
            return Ok(());
        }

        mgr.resume()?;
        println!("resumed task {} (state: {:?})", self.task_id, mgr.state());
        Ok(())
    }
}

fn find_latest_session(
    worktree_root: &std::path::Path,
    task_id: &str,
) -> anyhow::Result<latte_agent_core::session::SessionRecord> {
    latte_agent_core::session::SessionManager::find_latest_record(worktree_root, task_id)
        .ok_or_else(|| anyhow::anyhow!("no session found for task '{}'", task_id))
}
