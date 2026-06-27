use clap::Args;
use latte_agent_core::session::{SessionManager, SessionState};
use latte_agent_core::workspace::WorkspaceManager;

#[derive(Args, Debug)]
pub struct PauseCmd {
    #[arg(long)]
    pub task_id: String,
    /// Pause reason; written to the session JSON.
    #[arg(long, default_value = "external: latte-agent pause")]
    pub reason: String,
}

impl PauseCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let cwd = std::env::current_dir()?;
        let repo_root = WorkspaceManager::resolve_repo_root(&cwd)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let worktree_root = repo_root.join(".latte").join("worktrees").join(&self.task_id);
        if !worktree_root.exists() {
            anyhow::bail!("worktree for task '{}' not found at {}", self.task_id, worktree_root.display());
        }
        let session = find_latest_session(&worktree_root, &self.task_id)?;
        let mut mgr = SessionManager::from_record(session, worktree_root);
        match mgr.state() {
            SessionState::Running | SessionState::Resumed | SessionState::Created => {
                mgr.pause(&self.reason)?;
                println!("paused task {} (reason: {})", self.task_id, self.reason);
                Ok(())
            }
            other => {
                anyhow::bail!("task {} is in state {:?}, cannot pause", self.task_id, other);
            }
        }
    }
}

fn find_latest_session(
    worktree_root: &std::path::Path,
    task_id: &str,
) -> anyhow::Result<latte_agent_core::session::SessionRecord> {
    let sessions_dir = worktree_root.join(".latte").join("sessions");
    if !sessions_dir.exists() {
        anyhow::bail!("no session found for task '{}' (sessions dir does not exist)", task_id);
    }
    let mut best: Option<(std::time::SystemTime, latte_agent_core::session::SessionRecord)> = None;
    for entry in std::fs::read_dir(&sessions_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") { continue; }
        let modified = entry.metadata()?.modified()?;
        let raw = std::fs::read_to_string(&path)?;
        let record: latte_agent_core::session::SessionRecord = serde_json::from_str(&raw)?;
        if record.task_id != task_id { continue; }
        match &best {
            Some((t, _)) if *t >= modified => {}
            _ => best = Some((modified, record)),
        }
    }
    best.map(|(_, r)| r).ok_or_else(|| anyhow::anyhow!("no session JSON for task '{}'", task_id))
}
