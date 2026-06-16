//! Error types for the orchestrator.

use thiserror::Error;

/// Result alias for orchestrator operations.
pub type OrchResult<T> = std::result::Result<T, OrchError>;

/// Orchestrator errors.
#[derive(Error, Debug)]
pub enum OrchError {
    /// Agent runtime error.
    #[error("agent error: {0}")]
    Agent(#[from] latte_agent_core::error::AgentError),

    /// An agent referenced in a workflow is not registered.
    #[error("agent not found: '{0}' — available: {1:?}")]
    AgentNotFound(String, Vec<String>),

    /// Workflow configuration is invalid.
    #[error("invalid workflow: {0}")]
    InvalidWorkflow(String),

    /// Consensus requires a moderator but none was configured.
    #[error("consensus method requires a moderator, but none configured")]
    ModeratorRequired,

    /// Discussion exceeded max rounds without consensus.
    #[error("max rounds ({0}) exceeded without consensus")]
    MaxRoundsExceeded(usize),

    /// Max turns exceeded.
    #[error("max turns ({0}) exceeded")]
    MaxTurnsExceeded(usize),

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Config error.
    #[error("config error: {0}")]
    Config(String),
}
