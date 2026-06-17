//! Role system: role definitions, categories, and built-in role constructors.
//!
//! Mirrors gsd-core's agent definition pattern: each role has an identity (system prompt),
//! a default model tier, and allowed tools.

use latte_ai::params::GenerateParams;
use serde::{Deserialize, Serialize};

/// Classification of a role in the development lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleCategory {
    /// Planning roles: PM, Architect, Manager.
    Planning,
    /// Execution roles: Programmer, DevOps, Tech Writer.
    Execution,
    /// Verification roles: Tester, Reviewer, Security.
    Verification,
    /// General discussion / all-purpose.
    Discussion,
}

impl RoleCategory {
    /// Parse from string (case-insensitive, snake_case or kebab-case).
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().replace('-', "_").as_str() {
            "planning" => Some(Self::Planning),
            "execution" => Some(Self::Execution),
            "verification" => Some(Self::Verification),
            "discussion" => Some(Self::Discussion),
            _ => None,
        }
    }
}

/// A role template — defines the identity, behavior, and defaults for an agent role.
///
/// Loaded from TOML config + markdown prompt files. Supports `{{variable}}` template
/// substitution via handlebars.
#[derive(Debug, Clone)]
pub struct Role {
    /// Unique identifier (e.g. "pm", "programmer", "tester").
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Lifecycle category.
    pub category: RoleCategory,
    /// System prompt template (handlebars). Rendered with context variables.
    pub system_prompt: String,
    /// Default model tier for this role (resolves to the primary model).
    pub default_model_tier: crate::model_resolver::ModelTier,
    /// Fallback model chain, in priority order (highest first).
    ///
    /// When the primary model (resolved from `default_model_tier`) is
    /// unavailable (rate-limited, 5xx, etc.), the agent walks this list and
    /// tries each model in order. Empty = no fallback beyond the primary.
    pub model_chain: Vec<String>,
    /// Default generation params (temperature, top_p, etc.).
    pub default_params: GenerateParams,
    /// Tools this role typically uses (tool names).
    pub allowed_tools: Vec<String>,
    /// Display icon/emoji.
    pub icon: String,
}

impl Role {
    /// Render the system prompt with the given context variables.
    pub fn render_prompt(
        &self,
        vars: &serde_json::Value,
    ) -> Result<String, crate::error::AgentError> {
        let reg = new_handlebars();
        reg.render_template(&self.system_prompt, vars)
            .map_err(|e| crate::error::AgentError::Template(self.id.clone(), e))
    }

    /// Render the system prompt with string variables.
    pub fn render_prompt_str(
        &self,
        vars: &std::collections::HashMap<String, String>,
    ) -> Result<String, crate::error::AgentError> {
        let json = serde_json::to_value(vars).unwrap_or_default();
        self.render_prompt(&json)
    }
}

/// Lightweight, serializable role template for TOML config.
///
/// Used as an intermediate step: loaded from TOML, then converted to `Role`
/// by resolving the prompt file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleTemplate {
    pub id: String,
    pub name: String,
    pub category: String,
    pub model_tier: String,
    /// Fallback model chain in priority order (highest priority first).
    /// TOML example: `model_chain = ["gpt-4o", "deepseek-chat", "claude-3-haiku"]`.
    #[serde(default)]
    pub model_chain: Vec<String>,
    pub prompt_file: Option<String>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub icon: String,
}

impl RoleTemplate {
    /// Resolve this template into a full `Role` by loading the prompt file.
    pub async fn resolve(
        &self,
        default_params: &GenerateParams,
    ) -> Result<Role, crate::error::AgentError> {
        let category = RoleCategory::parse(&self.category)
            .ok_or_else(|| {
                crate::error::AgentError::Config(format!(
                    "unknown role category '{}' for role '{}'",
                    self.category, self.id
                ))
            })?;

        let model_tier = crate::model_resolver::ModelTier::parse(&self.model_tier)?;

        let mut params = default_params.clone();
        if let Some(t) = self.temperature {
            params.temperature = Some(t);
        }

        let system_prompt = match &self.prompt_file {
            Some(path) => tokio::fs::read_to_string(path).await.map_err(|e| {
                crate::error::AgentError::Config(format!(
                    "cannot read prompt file '{}' for role '{}': {}",
                    path, self.id, e
                ))
            })?,
            None => format!(
                "You are a {}. Respond in character as a {}.\nYour task: {{topic}}",
                self.name, self.name
            ),
        };

        Ok(Role {
            id: self.id.clone(),
            name: self.name.clone(),
            category,
            system_prompt,
            default_model_tier: model_tier,
            model_chain: self.model_chain.clone(),
            default_params: params,
            allowed_tools: self.tools.clone(),
            icon: if self.icon.is_empty() {
                default_icon(&self.id)
            } else {
                self.icon.clone()
            },
        })
    }
}

fn default_icon(role_id: &str) -> String {
    match role_id {
        "pm" => "📋".into(),
        "architect" => "🏗️".into(),
        "programmer" => "💻".into(),
        "tester" => "🧪".into(),
        "reviewer" => "🔍".into(),
        "devops" => "🚀".into(),
        "security" => "🛡️".into(),
        "designer" => "🎨".into(),
        "tech_writer" => "📝".into(),
        "manager" => "👔".into(),
        _ => "🤖".into(),
    }
}

/// Create a handlebars registry with default settings (no escaping for markdown).
fn new_handlebars() -> handlebars::Handlebars<'static> {
    let mut reg = handlebars::Handlebars::new();
    reg.register_escape_fn(handlebars::no_escape);
    reg.set_strict_mode(true);
    reg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_role_category_parse() {
        assert_eq!(RoleCategory::parse("planning"), Some(RoleCategory::Planning));
        assert_eq!(RoleCategory::parse("Execution"), Some(RoleCategory::Execution));
        assert_eq!(RoleCategory::parse("verification"), Some(RoleCategory::Verification));
        assert_eq!(RoleCategory::parse("discussion"), Some(RoleCategory::Discussion));
        assert_eq!(RoleCategory::parse("unknown"), None);
    }

    #[test]
    fn test_default_icon() {
        assert_eq!(default_icon("pm"), "📋");
        assert_eq!(default_icon("unknown_role"), "🤖");
    }

    #[test]
    fn test_render_prompt() {
        let role = Role {
            id: "test".into(),
            name: "Test".into(),
            category: RoleCategory::Discussion,
            system_prompt: "You are a {{role_name}}. Topic: {{topic}}".into(),
            default_model_tier: crate::model_resolver::ModelTier::Standard,
            model_chain: vec![],
            default_params: GenerateParams::default(),
            allowed_tools: vec![],
            icon: "🧪".into(),
        };

        let vars: std::collections::HashMap<String, String> = [
            ("role_name".into(), "Tester".into()),
            ("topic".into(), "Login flow".into()),
        ]
        .into_iter()
        .collect();

        let rendered = role.render_prompt_str(&vars).unwrap();
        assert!(rendered.contains("Tester"));
        assert!(rendered.contains("Login flow"));
    }
}
