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
/// Screenshot testing skill
pub const SCREENSHOT_SKILL: &str = include_str!("../../prompts/screenshot_skill.md");
/// Engineering Manager
pub const MANAGER: &str = include_str!("../../prompts/manager.md");
/// MCP Agent
pub const MCP_AGENT: &str = include_str!("../../prompts/mcp_agent.md");
/// Senior Advisor / final reviewer
pub const ADVISOR: &str = include_str!("../../prompts/advisor.md");

/// Look up a skill by name from built-in prompts.
pub fn for_skill(name: &str) -> Option<&'static str> {
    match name {
        "screenshot_skill" => Some(SCREENSHOT_SKILL),
        _ => None,
    }
}

/// Look up a default role prompt by its id (e.g. `"pm"`, `"architect"`).
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
        "mcp_agent" => MCP_AGENT,
        "advisor" => ADVISOR,
        _ => return None,
    })
}
/// Default role template factory for built-in roles. Returns `Some`
/// only for the 10 hard-coded roles below; callers use this when no
/// project config and no global config is present so the binary can
/// still serve its built-in defaults.
///
/// The role metadata (id, name, category, model_tier, tools, icon)
/// is duplicated from `.latte/agents/<id>.toml` because we want the
/// binary to work without that directory existing on disk. Keep the
/// two in sync when changing role metadata.
pub fn template_for(id: &str) -> Option<crate::role::RoleTemplate> {
    use crate::role::RoleTemplate;
    Some(match id {
        "pm" => RoleTemplate {
            id: "pm".into(),
            name: "Product Manager".into(),
            category: "planning".into(),
            model_tier: "standard".into(),
            model_chain: vec![],
            prompt_file: None,
            temperature: Some(0.7),
            tools: vec!["read".into(), "list".into(), "search".into()],
            icon: "📋".into(),
            skills: vec![],
        },
        "architect" => RoleTemplate {
            id: "architect".into(),
            name: "System Architect".into(),
            category: "planning".into(),
            model_tier: "premium".into(),
            model_chain: vec![],
            prompt_file: None,
            temperature: Some(0.5),
            tools: vec!["read".into(), "list".into(), "search".into()],
            icon: "🏗️".into(),
            skills: vec![],
        },
        "programmer" => RoleTemplate {
            id: "programmer".into(),
            name: "Software Engineer".into(),
            category: "execution".into(),
            model_tier: "budget".into(),
            model_chain: vec![],
            prompt_file: None,
            temperature: Some(0.3),
            tools: vec![
                "read".into(),
                "write".into(),
                "bash".into(),
                "search".into(),
            ],
            icon: "💻".into(),
            skills: vec![],
        },
        "tester" => RoleTemplate {
            id: "tester".into(),
            name: "QA Engineer".into(),
            category: "verification".into(),
            model_tier: "budget".into(),
            model_chain: vec![],
            prompt_file: None,
            temperature: Some(0.4),
            tools: vec![
                "read".into(),
                "list".into(),
                "bash".into(),
                "search".into(),
            ],
            icon: "🧪".into(),
            skills: vec![],
        },
        "reviewer" => RoleTemplate {
            id: "reviewer".into(),
            name: "Code Reviewer".into(),
            category: "verification".into(),
            model_tier: "standard".into(),
            model_chain: vec![],
            prompt_file: None,
            temperature: Some(0.4),
            tools: vec!["read".into(), "list".into(), "search".into()],
            icon: "🔍".into(),
            skills: vec![],
        },
        "devops" => RoleTemplate {
            id: "devops".into(),
            name: "DevOps Engineer".into(),
            category: "execution".into(),
            model_tier: "budget".into(),
            model_chain: vec![],
            prompt_file: None,
            temperature: Some(0.3),
            tools: vec!["read".into(), "bash".into(), "write".into()],
            icon: "🚀".into(),
            skills: vec![],
        },
        "security" => RoleTemplate {
            id: "security".into(),
            name: "Security Auditor".into(),
            category: "verification".into(),
            model_tier: "standard".into(),
            model_chain: vec![],
            prompt_file: None,
            temperature: Some(0.4),
            tools: vec!["read".into(), "list".into(), "search".into()],
            icon: "🛡️".into(),
            skills: vec![],
        },
        "designer" => RoleTemplate {
            id: "designer".into(),
            name: "UI/UX Designer".into(),
            category: "planning".into(),
            model_tier: "standard".into(),
            model_chain: vec![],
            prompt_file: None,
            temperature: Some(0.7),
            tools: vec!["read".into()],
            icon: "🎨".into(),
            skills: vec![],
        },
        "tech_writer" => RoleTemplate {
            id: "tech_writer".into(),
            name: "Technical Writer".into(),
            category: "execution".into(),
            model_tier: "budget".into(),
            model_chain: vec![],
            prompt_file: None,
            temperature: Some(0.5),
            tools: vec!["read".into(), "write".into()],
            icon: "📝".into(),
            skills: vec![],
        },
        "manager" => RoleTemplate {
            id: "manager".into(),
            name: "Engineering Manager".into(),
            category: "planning".into(),
            model_tier: "premium".into(),
            model_chain: vec![],
            prompt_file: None,
            temperature: Some(0.5),
            tools: vec!["delegate".into(), "workflow".into()],
            icon: "👔".into(),
            skills: vec![],
        },
        "mcp_agent" => RoleTemplate {
            id: "mcp_agent".into(),
            name: "MCP Agent".into(),
            category: "execution".into(),
            model_tier: "budget".into(),
            model_chain: vec![],
            prompt_file: None,
            temperature: Some(0.3),
            tools: vec!["mcp".into(), "bash".into(), "read".into(), "list".into()],
            icon: "🔌".into(),
            skills: vec![],
        },
        "advisor" => RoleTemplate {
            id: "advisor".into(),
            name: "Senior Advisor".into(),
            category: "verification".into(),
            model_tier: "premium".into(),
            model_chain: vec![],
            prompt_file: None,
            temperature: Some(0.4),
            tools: vec!["read".into(), "list".into(), "search".into()],
            icon: "🦉".into(),
            skills: vec![],
        },
        _ => return None,
    })
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Every role declared by `prompts::template_for` below must
    /// have a matching prompt here. Adding a role upstream
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
            "advisor",
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
