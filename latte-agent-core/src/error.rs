//! Error types for the agent runtime.

use thiserror::Error;

/// Result alias used throughout the agent crates.
pub type AgentResult<T> = std::result::Result<T, AgentError>;

/// Agent runtime errors.
#[derive(Error, Debug)]
pub enum AgentError {
    /// Configuration loading or parsing failed.
    #[error("config error: {0}")]
    Config(String),

    /// A role referenced in config was not found.
    #[error("role not found: {0}")]
    RoleNotFound(String),

    /// A model referenced in config was not found.
    #[error("model not found: {0}")]
    ModelNotFound(String),

    /// Model tier resolution failed.
    #[error("cannot resolve model for role '{role}' at tier '{tier}': {reason}")]
    ModelResolutionFailed {
        role: String,
        tier: String,
        reason: String,
    },

    /// Template rendering failed.
    #[error("template render error for '{0}': {1}")]
    Template(String, #[source] handlebars::RenderError),

    /// Template file not found.
    #[error("prompt template not found: {0}")]
    TemplateNotFound(String),

    /// AI client error.
    #[error("AI client error: {0}")]
    AiClient(#[from] latte_ai::error::AiError),

    /// Tool execution error.
    #[error("tool error: {0}")]
    Tool(String),

    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Token budget exceeded.
    #[error("token budget exceeded: used {used}, budget {budget}")]
    TokenBudgetExceeded { used: usize, budget: usize },

    /// Max tool-call rounds exceeded.
    #[error("max tool rounds ({0}) exceeded")]
    MaxToolRoundsExceeded(usize),
    /// A tool loop was detected: the model called the same tool with
    /// the same arguments `LOOP_STREAK_THRESHOLD+` times in a row,
    /// indicating it is stuck. We break out before
    /// `max_tool_rounds` is exhausted so the user can intervene.
    #[error("tool loop detected: {tool} — {reason}")]
    ToolLoopDetected { tool: String, reason: String },

    /// Invalid parameter.
    #[error("invalid parameter: {0}")]
    InvalidParam(String),

    /// Orchestration error (for phase 2+).
    #[error("orchestration error: {0}")]
    Orchestration(String),

    /// All models in the role's fallback chain are unavailable
    /// (rate-limited / 5xx / cooldown).
    ///
    /// `tried`: model_ids attempted in priority order (skipping
    ///          already-on-cooldown entries).
    /// `next_retry_in`: how long until the **earliest** model in the chain
    ///                  exits cooldown. `None` if no model has a future
    ///                  cooldown (i.e. the failures were non-retryable but
    ///                  were swallowed by the fallback loop — caller's hint
    #[error("all models unavailable (tried: {tried:?}); next retry in {next_retry_in:?}")]
    ModelsUnavailable {
        tried: Vec<String>,
        next_retry_in: Option<std::time::Duration>,
    },

    /// A lifecycle hook aborted execution.
    #[error("hook '{hook}' aborted: {reason}")]
    HookAborted { hook: String, reason: String },
}
