//! Model resolution: tier → concrete model mapping.
//!
//! Mirrors gsd-core's three-layer model assignment:
//!   Layer 1: model-catalog.json → Agent → Model Profile Mapping
//!   Layer 2: model-resolver.cts  → Config-driven model ID resolution
//!   Layer 3: runtime tier defaults → Profile tier → concrete model ID

use latte_ai::models::Model;
use serde::{Deserialize, Serialize};

use crate::config::{AgentConfig, ModelDef};
use crate::error::{AgentError, AgentResult};

/// Three-tier model assignment per agent (mirrors gsd-core quality/balanced/budget).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTier {
    /// Best available model (cf. opus).
    Premium,
    /// Best-balanced model (cf. sonnet).
    Standard,
    /// Fast/cheap model (cf. haiku).
    Budget,
}

impl ModelTier {
    /// Parse from string (case-insensitive).
    pub fn parse(s: &str) -> AgentResult<Self> {
        match s.to_lowercase().as_str() {
            "premium" | "golden" | "quality" | "opus" => Ok(Self::Premium),
            "standard" | "balanced" | "sonnet" => Ok(Self::Standard),
            "budget" | "haiku" | "fast" => Ok(Self::Budget),
            other => Err(AgentError::Config(format!(
                "unknown model tier: '{}' (expected premium/standard/budget)",
                other
            ))),
        }
    }

    /// Dynamic escalation on retry: budget → standard → premium.
    pub fn escalate(self) -> Option<Self> {
        match self {
            Self::Budget => Some(Self::Standard),
            Self::Standard => Some(Self::Premium),
            Self::Premium => None, // already at max
        }
    }

    /// Human-readable label.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Premium => "premium",
            Self::Standard => "standard",
            Self::Budget => "budget",
        }
    }
}

/// Maps role + tier → concrete Model.
///
/// Resolution order:
/// 1. Per-role tier override in config (`role_tiers.<role>.<tier>`)
/// 2. Global tier default (`tiers.<tier>`)
/// 3. Model's own `tier` field in catalog
pub struct ModelResolver {
    /// All known models indexed by id.
    models: std::collections::HashMap<String, ModelDef>,
    /// Global tier → model_id mapping.
    tier_defaults: std::collections::HashMap<ModelTier, String>,
    /// Per-role tier overrides: role_id → (tier → model_id).
    role_tiers: std::collections::HashMap<String, std::collections::HashMap<ModelTier, String>>,
}

impl ModelResolver {
    /// Build a resolver from an `AgentConfig`.
    pub fn from_config(config: &AgentConfig) -> AgentResult<Self> {
        let models: std::collections::HashMap<String, ModelDef> = config
            .models
            .models
            .iter()
            .map(|m| (m.id.clone(), m.clone()))
            .collect();

        let tier_defaults = config
            .models
            .tiers
            .as_ref()
            .map(|tiers| {
                let mut map = std::collections::HashMap::new();
                for (tier_str, model_id) in tiers.iter() {
                    let tier = ModelTier::parse(tier_str)?;
                    map.insert(tier, model_id.clone());
                }
                Ok::<_, AgentError>(map)
            })
            .unwrap_or_else(|| Ok(std::collections::HashMap::new()))?;

        let role_tiers = config
            .models
            .role_tiers
            .as_ref()
            .map(|rt| {
                let mut outer = std::collections::HashMap::new();
                for (role_id, tier_map) in rt.iter() {
                    let mut inner = std::collections::HashMap::new();
                    for (tier_str, model_id) in tier_map.iter() {
                        let tier = ModelTier::parse(tier_str)?;
                        inner.insert(tier, model_id.clone());
                    }
                    outer.insert(role_id.clone(), inner);
                }
                Ok::<_, AgentError>(outer)
            })
            .unwrap_or_else(|| Ok(std::collections::HashMap::new()))?;

        Ok(Self {
            models,
            tier_defaults,
            role_tiers,
        })
    }

    /// Resolve a concrete `Model` for a given role and tier.
    ///
    /// Lookup order: role_tiers override → tier_defaults → model's own tier field.
    pub fn resolve(&self, role_id: &str, tier: ModelTier) -> AgentResult<Model> {
        // 1. Check per-role tier override
        if let Some(role_map) = self.role_tiers.get(role_id) {
            if let Some(model_id) = role_map.get(&tier) {
                return self.build_model(model_id);
            }
        }

        // 2. Check global tier defaults
        if let Some(model_id) = self.tier_defaults.get(&tier) {
            return self.build_model(model_id);
        }

        // 3. Find first model matching this tier in the catalog
        let matching = self
            .models
            .values()
            .find(|m| {
                m.tier
                    .as_ref()
                    .map(|t| ModelTier::parse(t).ok() == Some(tier))
                    .unwrap_or(false)
            });

        if let Some(def) = matching {
            return self.build_model(&def.id);
        }

        // 4. Fallback: return first model in catalog
        if let Some(def) = self.models.values().next() {
            return self.build_model(&def.id);
        }

        Err(AgentError::ModelResolutionFailed {
            role: role_id.into(),
            tier: tier.label().into(),
            reason: "no models in catalog".into(),
        })
    }

    /// Escalate tier and resolve the escalated model. Returns `None` if already at max.
    pub fn resolve_escalated(
        &self,
        role_id: &str,
        current: ModelTier,
    ) -> Option<(ModelTier, AgentResult<Model>)> {
        let escalated = current.escalate()?;
        Some((escalated, self.resolve(role_id, escalated)))
    }

    /// List all known model tiers for a role.
    pub fn available_tiers(&self, role_id: &str) -> Vec<ModelTier> {
        let mut tiers = std::collections::BTreeSet::new();

        // From role_tiers
        if let Some(role_map) = self.role_tiers.get(role_id) {
            tiers.extend(role_map.keys().copied());
        }
        // From global tier defaults
        tiers.extend(self.tier_defaults.keys().copied());
        // From model tier fields
        for def in self.models.values() {
            if let Some(t) = &def.tier {
                if let Ok(tier) = ModelTier::parse(t) {
                    tiers.insert(tier);
                }
            }
        }

        tiers.into_iter().collect()
    }

    fn build_model(&self, model_id: &str) -> AgentResult<Model> {
        let def = self
            .models
            .get(model_id)
            .ok_or_else(|| AgentError::ModelNotFound(model_id.into()))?;

        let api_type = match def.api.as_str() {
            "openai" | "openai-completions" => latte_ai::models::ApiType::OpenAiCompletions,
            "anthropic" | "anthropic-messages" => latte_ai::models::ApiType::AnthropicMessages,
            other => {
                return Err(AgentError::Config(format!(
                    "unsupported API type '{}' for model '{}'",
                    other, def.id
                )))
            }
        };

        Ok(Model {
            id: def.id.clone(),
            name: def.name.clone(),
            api: api_type,
            provider: def.provider.clone(),
            base_url: def.base_url.clone(),
            api_key: resolve_env_vars(&def.api_key),
            context_window: def.context_window,
            max_tokens: def.max_tokens,
            supports_thinking: def.supports_thinking,
            cost_per_million_input: def.cost_per_million_input.unwrap_or(0.0),
            cost_per_million_output: def.cost_per_million_output.unwrap_or(0.0),
        })
    }
}

/// Resolve `${ENV_VAR}` placeholders in a string.
fn resolve_env_vars(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '$' && chars.peek() == Some(&'{') {
            chars.next(); // skip '{'
            let mut var_name = String::new();
            for ch in chars.by_ref() {
                if ch == '}' {
                    break;
                }
                var_name.push(ch);
            }
            let value = std::env::var(&var_name).unwrap_or_default();
            result.push_str(&value);
        } else {
            result.push(c);
        }
    }

    result
}

impl std::fmt::Debug for ModelResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelResolver")
            .field("models", &self.models.len())
            .field("tier_defaults", &self.tier_defaults)
            .field("role_tiers", &self.role_tiers)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelDef;

    fn test_model_def(id: &str, tier: Option<&str>) -> ModelDef {
        ModelDef {
            id: id.into(),
            name: format!("Model {}", id),
            api: "openai".into(),
            provider: "test".into(),
            base_url: "http://localhost".into(),
            api_key: "test-key".into(),
            context_window: 32000,
            max_tokens: 4096,
            supports_thinking: false,
            cost_per_million_input: Some(0.0),
            cost_per_million_output: Some(0.0),
            tier: tier.map(|s| s.into()),
        }
    }

    #[test]
    fn test_model_tier_parse() {
        assert_eq!(ModelTier::parse("premium").unwrap(), ModelTier::Premium);
        assert_eq!(ModelTier::parse("standard").unwrap(), ModelTier::Standard);
        assert_eq!(ModelTier::parse("budget").unwrap(), ModelTier::Budget);
        assert_eq!(ModelTier::parse("golden").unwrap(), ModelTier::Premium);
        assert_eq!(ModelTier::parse("sonnet").unwrap(), ModelTier::Standard);
        assert_eq!(ModelTier::parse("haiku").unwrap(), ModelTier::Budget);
        assert!(ModelTier::parse("unknown").is_err());
    }

    #[test]
    fn test_tier_escalate() {
        assert_eq!(ModelTier::Budget.escalate(), Some(ModelTier::Standard));
        assert_eq!(ModelTier::Standard.escalate(), Some(ModelTier::Premium));
        assert_eq!(ModelTier::Premium.escalate(), None);
    }

    #[test]
    fn test_resolve_via_tier_defaults() {
        let config = AgentConfig {
            models: crate::config::ModelCatalog {
                models: vec![
                    test_model_def("premium-model", Some("premium")),
                    test_model_def("standard-model", Some("standard")),
                    test_model_def("budget-model", Some("budget")),
                ],
                tiers: Some(
                    [
                        ("premium".into(), "premium-model".into()),
                        ("standard".into(), "standard-model".into()),
                        ("budget".into(), "budget-model".into()),
                    ]
                    .into_iter()
                    .collect(),
                ),
                role_tiers: None,
            },
            roles: Default::default(),
        };

        let resolver = ModelResolver::from_config(&config).unwrap();

        let model = resolver.resolve("any_role", ModelTier::Standard).unwrap();
        assert_eq!(model.id, "standard-model");
    }

    #[test]
    fn test_resolve_fallback() {
        let config = AgentConfig {
            models: crate::config::ModelCatalog {
                models: vec![test_model_def("only-model", None)],
                tiers: None,
                role_tiers: None,
            },
            roles: Default::default(),
        };

        let resolver = ModelResolver::from_config(&config).unwrap();
        let model = resolver.resolve("any_role", ModelTier::Budget).unwrap();
        assert_eq!(model.id, "only-model");
    }

    #[test]
    fn test_env_var_resolution() {
        std::env::set_var("TEST_KEY", "resolved-value");
        assert_eq!(resolve_env_vars("${TEST_KEY}"), "resolved-value");
        assert_eq!(resolve_env_vars("prefix_${TEST_KEY}_suffix"), "prefix_resolved-value_suffix");
        assert_eq!(resolve_env_vars("${NONEXISTENT}"), "");
        std::env::remove_var("TEST_KEY");
    }
}
