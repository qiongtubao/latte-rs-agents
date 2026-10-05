//! # latte-agent-core
//!
//! Core agent runtime for multi-role discussion systems.
//!
//! ## Architecture
//!
//! ```text
//! AgentConfig (TOML) ──▶ ModelResolver ──▶ Agent ──▶ AgentRunner
//!        │                    │                │            │
//!   roles + models      tier→model       client+tools   run_turn()
//! ```
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use latte_agent_core::prelude::*;
//! use latte_agent_core::{ModelResolver, ModelTier};
//! use latte_ai::models::{Message, Role};
//!
//! # async fn example() -> std::result::Result<(), Box<dyn std::error::Error>> {
//! # let config = AgentConfig::load(".latte/agents")?;
//! let resolver = ModelResolver::from_config(&config)?;
//!
//! let template = config.roles.get("pm").unwrap();
//! let role = template.resolve(&GenerateParams::default()).await?;
//! let model = resolver.resolve(&role.id, ModelTier::Standard)?;
//! let agent = Agent::new("pm".into(), role, model, GenerateParams::default())?;
//!
//! let mut runner = AgentRunner::new(agent);
//! let response = runner.run_turn(
//!     &[Message::user("Features for MVP?")],
//!     None,
//! ).await?;
//! println!("{}", response);
//! # Ok(())
//! # }
//! ```

pub mod advisor_monitor;
pub mod agent;
pub mod billing;
pub mod bridge;
pub mod checkpoint;
pub mod choice;
pub mod citation_check;
pub mod code_graph_index;
pub mod code_graph_viz;
pub mod compaction;
pub mod config;
pub mod context;
pub mod controller;
pub mod dispatch_ledger;
pub mod doc_graph_tools;
pub mod edit_anchor;
pub mod error;
pub mod event_json;
pub mod global_config;
pub mod goal;
pub mod ground_truth;
pub mod hooks;
pub mod image_gen;
pub mod learn_doc;
pub mod inject_queue;
pub mod model_resolver;
pub mod pause_gate;
/// 阻塞 ask 挂起项的跨进程落盘（服务器重启后用户回答仍能驱动续跑）。
pub mod pending_ask;
pub mod prompts;
pub mod renderer;
pub mod role;
pub mod scheduler;
pub mod session;
pub mod session_store;
pub mod staging;
pub mod step_journal;
pub mod sub_cancel;
pub mod subsession;
mod subsession_disk;
pub mod supervisor;
pub mod task_worktree;
pub mod todo;
pub mod tool_docs;
pub mod tool_fix;
pub mod trace;
pub mod workflow;
pub mod workspace;
pub use config::{AgentConfig, ConfigLayer};
pub use context::{ConversationContext, Importance};
pub use error::{AgentError, AgentResult};
pub use model_resolver::{ModelResolver, ModelTier};
pub use role::{Role, RoleCategory, RoleTemplate};

/// Convenience re-exports.
pub mod prelude {
    pub use crate::advisor_monitor::{
        AdvisorMonitor, AdvisorMonitorConfig, AdvisorReviewEngine, AdvisorReviewMode, DetectorKind,
        Verdict,
    };
    pub use crate::agent::{Agent, AgentParams, AgentRunner, ModelClient, WaitPolicy};
    pub use crate::checkpoint::{Checkpoint, CheckpointError, CheckpointTrigger, RollbackMode};
    pub use crate::config::{AgentConfig, ConfigLayer};
    pub use crate::context::{ConversationContext, Importance};
    pub use crate::error::AgentResult;
    pub use crate::role::{Role, RoleCategory};
    pub use crate::subsession::SubsessionStore;
    pub use crate::trace::DiffSummary;
    pub use crate::trace::{MemorySink, NullSink, TraceEvent, TraceSink};
    pub use crate::workspace::{
        Blackboard, MergeMode, WorkspaceError, WorkspaceState, WorktreeSpec,
    };
    pub use latte_ai::prelude::*;
}

#[cfg(test)]
pub(crate) mod test_util {
    use std::sync::{Mutex, OnceLock};
    /// Process-wide mutex serialising `LATTE_HOME` env-var access
    /// across all unit tests in the crate. `cargo test` runs tests on
    /// multiple threads; env vars are process-wide, so concurrent
    /// test threads setting `LATTE_HOME` clobber each other.
    pub(crate) static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
}
