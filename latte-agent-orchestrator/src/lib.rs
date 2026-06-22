//! # latte-agent-orchestrator
//!
//! Multi-agent discussion orchestrator: coordinates multiple role-based agents
//! through structured discussion rounds with configurable workflows and consensus
//! mechanisms.

pub mod consensus;
pub mod error;
pub mod orchestrator;
pub mod round;
pub mod workflow;

pub use consensus::ConsensusMethod;
pub use orchestrator::DiscussionOrchestrator;
pub use round::{Round, Turn, TurnOrder, TurnRecord};
pub use workflow::{DiscussionWorkflow, StepHook, WorkflowRegistry, WorkflowStep};

/// Convenience re-exports.
pub mod prelude {
    pub use crate::consensus::ConsensusMethod;
    pub use crate::orchestrator::DiscussionOrchestrator;
    pub use crate::round::{Turn, TurnOrder, TurnRecord};
    pub use crate::workflow::{DiscussionWorkflow, WorkflowStep};
    pub use latte_agent_core::prelude::*;
}
