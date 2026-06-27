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
//! use latte_ai::models::{Message, Role};
//!
//! # async fn example() -> std::result::Result<(), Box<dyn std::error::Error>> {
//! let config = AgentConfig::load(".latte/agents.toml")?;
//! let resolver = ModelResolver::from_config(&config)?;
//!
//! let template = config.roles.get("pm").unwrap();
//! let role = template.resolve(&GenerateParams::default()).await?;
//! let model = resolver.resolve(&role.id, ModelTier::Standard)?;
//! let agent = Agent::new("pm".into(), role, model, GenerateParams::default())?;
//!
//! let mut runner = AgentRunner::new(agent);
//! let response = runner.run_turn(
//!     &[Message { role: Role::User, content: "Features for MVP?".to_string() }],
//!     None,
//! ).await?;
//! println!("{}", response);
//! # Ok(())
//! # }
//! ```

pub mod agent;
pub mod checkpoint;
pub mod config;
pub mod context;
pub mod error;
pub mod global_config;
pub mod hooks;
pub mod model_resolver;
pub mod prompts;
pub mod role;
pub mod scheduler;
pub mod session;
pub mod supervisor;
pub mod trace;
pub mod workspace;

pub use agent::{Agent, AgentRunner, AgentParams, ModelClient, WaitPolicy};
pub use config::AgentConfig;
pub use context::{ConversationContext, Importance};
pub use error::{AgentError, AgentResult};
pub use model_resolver::{ModelResolver, ModelTier};
pub use role::{Role, RoleCategory, RoleTemplate};

/// Convenience re-exports.
pub mod prelude {
    pub use crate::agent::{Agent, AgentRunner, AgentParams, ModelClient, WaitPolicy};
    pub use crate::checkpoint::{Checkpoint, CheckpointError, CheckpointTrigger, RollbackMode};
    pub use crate::config::AgentConfig;
    pub use crate::context::{ConversationContext, Importance};
    pub use crate::error::AgentResult;
    pub use crate::model_resolver::{ModelResolver, ModelTier};
    pub use crate::role::{Role, RoleCategory};
    pub use crate::scheduler::plan_md_slice_for;
    pub use crate::session::{RoleHistory, SessionError, SessionManager, SessionRecord, SessionState};
    pub use crate::supervisor::{Supervisor, SupervisorConfig};
    pub use crate::trace::DiffSummary;
    pub use crate::workspace::{Blackboard, MergeMode, WorktreeSpec, WorkspaceError, WorkspaceState};
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
