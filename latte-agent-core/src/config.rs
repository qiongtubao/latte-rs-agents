//! Configuration types: agent roles, model catalog, TOML load/save.
//!
//! Mirrors gsd-core's `model-catalog.json` + `config-defaults.manifest.json` pattern,
//! but in TOML (Rust-native config format).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::error::{AgentError, AgentResult};
use crate::role::RoleTemplate;

// ─── Top-level config ────────────────────────────────────────────────────

/// Top-level agent configuration, loaded from `agents.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentConfig {
    /// Model catalog with tier mappings.
    #[serde(default)]
    pub models: ModelCatalog,
    /// Role definitions.
    #[serde(default)]
    pub roles: HashMap<String, RoleTemplate>,
}

impl AgentConfig {
    /// Load config from a TOML file path.
    pub fn load(path: &str) -> AgentResult<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            AgentError::Config(format!("cannot read config file '{}': {}", path, e))
        })?;
        Self::parse(&content)
    }

    /// Parse config from TOML string.
    pub fn parse(content: &str) -> AgentResult<Self> {
        toml::from_str(content).map_err(|e| {
            AgentError::Config(format!("invalid TOML: {}", e))
        })
    }

    /// Save config to a TOML file path.
    pub fn save(&self, path: &str) -> AgentResult<()> {
        let content = toml::to_string_pretty(self).map_err(|e| {
            AgentError::Config(format!("serialization failed: {}", e))
        })?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Resolve all role templates into full `Role` definitions.
    pub async fn resolve_roles(
        &self,
        default_params: &latte_ai::params::GenerateParams,
    ) -> AgentResult<HashMap<String, crate::role::Role>> {
        let mut roles = HashMap::new();
        for (id, template) in &self.roles {
            let role = template.resolve(default_params).await?;
            roles.insert(id.clone(), role);
        }
        Ok(roles)
    }
}

// ─── Model catalog ───────────────────────────────────────────────────────

/// Model catalog with tier-to-model mappings.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelCatalog {
    /// All known model definitions.
    #[serde(default)]
    pub models: Vec<ModelDef>,
    /// Global tier → model_id mapping.
    #[serde(default)]
    pub tiers: Option<HashMap<String, String>>,
    /// Per-role tier overrides: role_id → (tier → model_id).
    #[serde(default)]
    pub role_tiers: Option<HashMap<String, HashMap<String, String>>>,
}

/// A single model definition in the catalog.
///
/// Compatible with `latte-rs-model-router/models.toml` format, extended with
/// optional `tier` and `supports_thinking` fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDef {
    /// Model identifier (e.g. "claude-sonnet-4-20250514").
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// API type: "openai" or "anthropic".
    pub api: String,
    /// Provider name.
    pub provider: String,
    /// Base URL for API requests.
    pub base_url: String,
    /// API key (supports `${ENV_VAR}` substitution).
    pub api_key: String,
    /// Context window size in tokens.
    pub context_window: u32,
    /// Maximum output tokens.
    pub max_tokens: u32,
    /// Whether the model supports thinking/reasoning.
    #[serde(default)]
    pub supports_thinking: bool,
    /// Cost per million input tokens (USD). Optional for local models.
    #[serde(default)]
    pub cost_per_million_input: Option<f64>,
    /// Cost per million output tokens (USD). Optional for local models.
    #[serde(default)]
    pub cost_per_million_output: Option<f64>,
    /// Which tier this model belongs to (for auto-resolution without explicit tier map).
    #[serde(default)]
    pub tier: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_config() {
        let toml = r#"
[models]
tiers = { premium = "claude-opus", standard = "claude-sonnet" }

[[models.models]]
id = "claude-opus"
name = "Claude Opus"
api = "anthropic"
provider = "anthropic"
base_url = "https://api.anthropic.com"
api_key = "test-key"
context_window = 200000
max_tokens = 8192

[[models.models]]
id = "claude-sonnet"
name = "Claude Sonnet"
api = "anthropic"
provider = "anthropic"
base_url = "https://api.anthropic.com"
api_key = "test-key"
context_window = 200000
max_tokens = 8192

[roles.pm]
id = "pm"
name = "Product Manager"
category = "planning"
model_tier = "standard"
icon = "📋"
"#;

        let config = AgentConfig::parse(toml).unwrap();
        assert_eq!(config.models.models.len(), 2);
        assert_eq!(config.roles.len(), 1);
    }

    #[test]
    fn test_parse_model_def() {
        let toml = r#"
id = "test-model"
name = "Test Model"
api = "openai"
provider = "test"
base_url = "http://localhost"
api_key = "key123"
context_window = 32000
max_tokens = 4096
supports_thinking = false
tier = "budget"
"#;

        let def: ModelDef = toml::from_str(toml).unwrap();
        assert_eq!(def.id, "test-model");
        assert_eq!(def.api, "openai");
        assert_eq!(def.tier, Some("budget".into()));
    }
}
