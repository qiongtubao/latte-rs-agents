//! Default role prompt templates, embedded at build time from the
//! workspace's `prompts/<id>.md` files via `include_str!`.
//!
//! Downstream consumers (e.g. `latte-code-editor`) read these
//! directly through `latte_agent_core::prompts::PM` etc. — no file
//! I/O, no copying of markdown into the consumer's tree. Source
//! of truth stays in `latte-rs-agents/prompts/`; updating a file
//! there flows downstream on the next `cargo build`.
//!
//! The `include_str!` paths are relative to this file
//! (`src/prompts.rs` → `../../prompts/<id>.md`).
//!
//! The content is rendered with handlebars at runtime by
//! `RoleTemplate::resolve()` / `AgentRunner::run_turn`; this module
//! just makes the raw markdown addressable as Rust constants.

/// Product Manager
pub const PM: &str = include_str!("../../prompts/pm.md");
/// System Architect
pub const ARCHITECT: &str = include_str!("../../prompts/architect.md");
/// Software Engineer
pub const PROGRAMMER: &str = include_str!("../../prompts/programmer.md");
/// QA Engineer
pub const TESTER: &str = include_str!("../../prompts/tester.md");
/// Code Reviewer
pub const REVIEWER: &str = include_str!("../../prompts/reviewer.md");
/// DevOps Engineer
pub const DEVOPS: &str = include_str!("../../prompts/devops.md");
/// Security Auditor
pub const SECURITY: &str = include_str!("../../prompts/security.md");
/// UI/UX Designer
pub const DESIGNER: &str = include_str!("../../prompts/designer.md");
/// Technical Writer
pub const TECH_WRITER: &str = include_str!("../../prompts/tech_writer.md");
/// Engineering Manager
pub const MANAGER: &str = include_str!("../../prompts/manager.md");

/// Look up a default role prompt by its id (e.g. `"pm"`, `"architect"`).
///
/// Returns `None` for unknown roles — callers should fall back to
/// a custom `prompt_file` path or an inline `prompt` string.
pub fn for_role(id: &str) -> Option<&'static str> {
    Some(match id {
        "pm" => PM,
        "architect" => ARCHITECT,
        "programmer" => PROGRAMMER,
        "tester" => TESTER,
        "reviewer" => REVIEWER,
        "devops" => DEVOPS,
        "security" => SECURITY,
        "designer" => DESIGNER,
        "tech_writer" => TECH_WRITER,
        "manager" => MANAGER,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every role defined in `latte-rs-agents/config/agents.toml`
    /// must have a matching prompt here. Adding a role upstream
    /// without adding the prompt constant will fail this test.
    #[test]
    fn all_default_roles_have_prompts() {
        for id in [
            "pm",
            "architect",
            "programmer",
            "tester",
            "reviewer",
            "devops",
            "security",
            "designer",
            "tech_writer",
            "manager",
        ] {
            assert!(
                for_role(id).is_some(),
                "missing prompt constant for role '{id}'"
            );
            assert!(
                !for_role(id).unwrap().is_empty(),
                "empty prompt for role '{id}'"
            );
        }
    }
}
