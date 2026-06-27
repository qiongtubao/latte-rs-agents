//! `latte-agent run` — run a task inside an isolated worktree sandbox.
//!
//! v1: creates the worktree, records an initial checkpoint, prints
//! one "would invoke role" line per role, and optionally archives
//! the worktree back into the base branch. Real model integration
//! lands in the v2 spec.

use std::sync::Arc;

use clap::Args;
use latte_agent_core::checkpoint::CheckpointEngine;
use latte_agent_core::trace::{FanoutSink, IndexSink, JsonlSink, NullSink, TraceSink};
use latte_agent_core::workspace::{MergeMode, WorkspaceManager};

/// Run a task inside an isolated worktree sandbox.
#[derive(Args, Debug)]
pub struct RunCmd {
    /// Task id. Becomes both the worktree dir name and the branch name.
    #[arg(long)]
    pub task_id: String,
    /// Comma-separated role list. v1: each role gets one run_turn.
    #[arg(long, default_value = "programmer")]
    pub roles: String,
    /// Initial prompt written to plan.md and used as the first user message.
    #[arg(long)]
    pub initial_prompt: String,
    /// Archive after running: --no-ff merge the worktree into the base branch.
    #[arg(long)]
    pub archive: bool,
    /// After archive, remove the worktree and branch.
    #[arg(long)]
    pub cleanup: bool,
    /// Choose merge mode when --archive is set.
    #[arg(long, value_enum, default_value_t = MergeArg::NoFf)]
    pub merge: MergeArg,
    /// Skip creating the plan.md blackboard.
    #[arg(long)]
    pub no_blackboard: bool,
}

/// CLI mirror of `MergeMode`. clap wants its own `ValueEnum`.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeArg {
    NoFf,
    Squash,
    FastForward,
}

impl From<MergeArg> for MergeMode {
    fn from(v: MergeArg) -> Self {
        match v {
            MergeArg::NoFf => MergeMode::NoFf,
            MergeArg::Squash => MergeMode::Squash,
            MergeArg::FastForward => MergeMode::FastForward,
        }
    }
}

impl RunCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let cwd = std::env::current_dir()?;
        let mut mgr = WorkspaceManager::create(&cwd, &self.task_id, &self.initial_prompt)?;

        // Sink fanout: JSONL on disk + always-on index. If those fail
        // (e.g. disk full), fall back to NullSink rather than aborting
        // the run.
        let sink: Arc<dyn TraceSink> = build_sink(&self.task_id)
            .unwrap_or_else(|_| Arc::new(NullSink));

        // Checkpoint engine
        let storage_dir = mgr
            .repo_root()
            .join(".latte")
            .join("checkpoints")
            .join(&self.task_id);
        let mut engine = CheckpointEngine::new(
            mgr.spec().worktree_root.clone(),
            storage_dir,
            sink.clone(),
        )?;

        // Initial checkpoint 0 — captures the just-created worktree.
        let cp0 = engine
            .record_write("init", &serde_json::json!({}))
            .map_err(anyhow::Error::from)?;
        let short = short_sha(&cp0.git_commit);
        println!("checkpoint 0 = {}", short);

        // v1 loop: one "would invoke" line per role. Real model
        // integration lands in the v2 spec.
        for role in self.roles.split(',') {
            println!("[v1] would invoke role: {}", role.trim());
        }

        // Optional archive
        if self.archive {
            let sha = mgr.archive(self.merge.into())?;
            let short = short_sha(&sha);
            println!("archived: merge commit {}", short);
            if self.cleanup {
                mgr.cleanup()?;
                println!("cleaned up worktree and branch");
            }
        }
        Ok(())
    }
}

/// Build the trace sink fanout. Respects `LATTE_HOME` for parity with
/// the rest of the CLI; falls back to `$HOME/.latte` and finally to a
/// relative `.latte` if neither is set.
fn build_sink(task_id: &str) -> anyhow::Result<Arc<dyn TraceSink>> {
    let home = latte_home();
    let jsonl_path = home.join("traces").join(format!("{}.jsonl", task_id));
    std::fs::create_dir_all(jsonl_path.parent().unwrap())?;
    let index_path = home.join("sessions").join(format!("{}.idx", task_id));
    std::fs::create_dir_all(index_path.parent().unwrap())?;
    let jsonl: Arc<dyn TraceSink> = Arc::new(JsonlSink::new(jsonl_path));
    let index: Arc<dyn TraceSink> = Arc::new(IndexSink::new(index_path));
    Ok(Arc::new(FanoutSink::new(vec![jsonl, index])))
}

/// Resolve the LATTE_HOME root for sink output. Mirrors the
/// `latte_home()` helpers elsewhere in the CLI so all per-task
/// trace/index files land in the same tree.
fn latte_home() -> std::path::PathBuf {
    std::env::var_os("LATTE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(|h| std::path::PathBuf::from(h).join(".latte"))
                .unwrap_or_else(|| std::path::PathBuf::from(".latte"))
        })
}

fn short_sha(s: &str) -> &str {
    &s[..12.min(s.len())]
}