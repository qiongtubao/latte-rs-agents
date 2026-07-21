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
#[derive(Clone)]
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
    ///
    /// Lookup order: role_tiers override → tier_defaults → model's own tier
    /// field → first model in catalog. If the chosen model has no
    /// `api_key` configured (empty after env-var resolution), walks the
    /// same priority order for an alternative whose `api_key` *is* set,
    /// so a stale `${ENV_VAR}` placeholder that the env never filled
    /// does not silently kill the role. Returns `Err` only when no
    /// candidate has a key.
    pub fn resolve(&self, role_id: &str, tier: ModelTier) -> AgentResult<Model> {
        let candidates: Vec<String> = self.candidates_for(role_id, tier);
        let mut missing_env: Vec<String> = Vec::new();

        for model_id in &candidates {
            if let Ok(m) = self.build_model(model_id) {
                if !m.api_key.trim().is_empty() {
                    return Ok(m);
                }
            }
            // Collect which env var this model needs.
            if let Some(def) = self.models.get(model_id) {
                if let Some(var) = extract_env_var(&def.api_key) {
                    if !missing_env.contains(&var) {
                        missing_env.push(var);
                    }
                }
            }
        }

        let reason = if missing_env.is_empty() {
            format!(
                "no api_key configured for candidates: {}. Set an api_key in config or via --api-key",
                candidates.join(", ")
            )
        } else {
            format!(
                "no api_key configured for candidates: {}. Set environment variable(s): {}",
                candidates.join(", "),
                missing_env.join(" ")
            )
        };

        Err(AgentError::ModelResolutionFailed {
            role: role_id.into(),
            tier: tier.label().into(),
            reason,
        })
    }

    /// Returns every model id that could satisfy this (role, tier) in
    /// priority order: per-role override → global tier default → catalog
    /// match on `tier` field → first model in catalog.
    fn candidates_for(&self, role_id: &str, tier: ModelTier) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut push = |id: &str| {
            if seen.insert(id.to_string()) {
                out.push(id.to_string());
            }
        };
        if let Some(role_map) = self.role_tiers.get(role_id) {
            if let Some(id) = role_map.get(&tier) {
                push(id);
            }
        }
        if let Some(id) = self.tier_defaults.get(&tier) {
            push(id);
        }
        if let Some(def) = self.models.values().find(|m| {
            m.tier
                .as_ref()
                .and_then(|t| ModelTier::parse(t).ok())
                == Some(tier)
        }) {
            push(&def.id);
        }
        if let Some(def) = self.models.values().next() {
            push(&def.id);
        }
        out
    }

    /// Resolve a primary model plus a priority-ordered fallback chain.
    ///
    /// The head of the returned chain is the *first* model in the
    /// tier-resolution order that has an `api_key` set; subsequent
    /// entries are `chain_ids` (deduped) where the model also has a
    /// key. Models whose `api_key` is empty are skipped, so the
    /// Resolve candidate models in priority order.
    ///
    /// When `chain_ids` is non-empty, the **first chain model** is used as
    /// the primary candidate (instead of the tier-based default from
    /// `[role_tiers]` / `[tiers]`).  Remaining chain ids are appended as
    /// fallbacks.
    ///
    /// When `chain_ids` is empty, falls back to the tier-based resolution.
    pub fn resolve_chain(
        &self,
        role_id: &str,
        tier: ModelTier,
        chain_ids: &[String],
    ) -> AgentResult<Vec<Model>> {
        let mut out: Vec<Model> = Vec::with_capacity(1 + chain_ids.len());

        if !chain_ids.is_empty() {
            // model_chain is set: use chain[0] as primary,
            // then append remaining chain ids as fallbacks.
            for id in chain_ids {
                if out.iter().any(|m| &m.id == id) {
                    continue;
                }
                if let Ok(m) = self.build_model(id) {
                    if !m.api_key.trim().is_empty() {
                        out.push(m);
                    }
                }
            }
        }

        // No chain or chain models all failed: fall back to tier-based resolve.
        if out.is_empty() {
            if let Ok(primary) = self.resolve(role_id, tier) {
                out.push(primary);
            }
        }

        // Also append tier-based candidates that aren't already in the chain,
        // as additional fallbacks (only when chain was used).
        if !chain_ids.is_empty() {
            if let Ok(primary) = self.resolve(role_id, tier) {
                if !out.iter().any(|m| &m.id == &primary.id) {
                    out.push(primary);
                }
            }
        }

        // Last-resort safety net: walk the full catalog for any model
        // with a valid `api_key`.
        if out.is_empty() {
            for def in self.models.values() {
                if let Ok(m) = self.build_model(&def.id) {
                    if !m.api_key.trim().is_empty() {
                        out.push(m);
                        break;
                    }
                }
            }
        }
        if out.is_empty() {
            return Err(AgentError::ModelResolutionFailed {
                role: role_id.into(),
                tier: tier.label().into(),
                reason: "no candidate with a configured api_key".into(),
            });
        }
        Ok(out)
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

    /// Resolve a model by **id or display name**. `id` is matched
    /// exactly; `name` is matched case-insensitively. The returned
    /// `Model` is the same as `build_model(id)` for the resolved id.
    /// On miss, the error message lists every known `(id, name)` so
    /// the operator can see what they meant.
    pub fn resolve_id_or_name(&self, query: &str) -> AgentResult<Model> {
        // 1) Exact id hit.
        if self.models.contains_key(query) {
            return self.build_model(query);
        }
        // 2) Case-insensitive name match.
        let q = query.to_lowercase();
        let mut hits: Vec<&str> = self
            .models
            .values()
            .filter(|m| m.name.to_lowercase() == q)
            .map(|m| m.id.as_str())
            .collect();
        if hits.len() == 1 {
            return self.build_model(&hits.remove(0));
        }
        if hits.len() > 1 {
            return Err(AgentError::Config(format!(
                "--model-id {} matches multiple models by name: {}",
                query,
                hits.join(", ")
            )));
        }
        // 3) Miss — build a helpful error.
        let known: Vec<String> = self
            .models
            .values()
            .map(|m| format!("{} ({})", m.id, m.name))
            .collect();
        Err(AgentError::Config(format!(
            "--model-id {}: no such id or display name. Available: [{}]",
            query,
            known.join(", ")
        )))
    }

     pub fn build_model(&self, model_id: &str) -> AgentResult<Model> {
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
        // Resolve `${ENV_VAR}` placeholders. Unset variables become the
        // empty string: that's the "not configured here" marker. The
        // three-layer merge (global → project → CLI) decides whether an
        // empty `api_key` stays empty (and the chat call later fails with
        // a clear `Config` error from `AiClient::check_api_key`).
        let api_key = resolve_env_vars(&def.api_key);

        Ok(Model {
            id: def.id.clone(),
            name: def.name.clone(),
            api: api_type,
            provider: def.provider.clone(),
            base_url: def.base_url.clone(),
            api_key,
            context_window: def.context_window,
            max_tokens: def.max_tokens,
            supports_thinking: def.supports_thinking,
            supports_vision: def.supports_vision,
            cost_per_million_input: def.cost_per_million_input.unwrap_or(0.0),
            cost_per_million_output: def.cost_per_million_output.unwrap_or(0.0),
        })
    }
    /// Look up the raw `ModelDef` for a model id, returning the catalog
    /// entry unchanged. Used by callers that need fields `Model` doesn't
    /// surface (e.g. `timeout_secs` for the `delegate` tool's
    /// per-model timeout budget). `Model` is a `latte_ai` value with a
    /// fixed schema, so we can't add fields there without modifying a
    /// foreign crate; this lookup lets the CLI pull per-model config
    /// out of the catalog without round-tripping through `Model`.
    pub fn get_def(&self, model_id: &str) -> Option<&ModelDef> {
        self.models.get(model_id)
    }
}

/// Resolve `${ENV_VAR}` placeholders in a string.
///
/// Unset variables become the empty string (the "not configured" marker).
/// The caller is responsible for deciding whether an empty result is
/// acceptable in context; for `api_key` the three-layer config merge
/// resolves that and `AiClient::check_api_key` produces a final error
/// if all layers were blank.
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
            match std::env::var(&var_name) {
                Ok(value) => result.push_str(&value),
                // Unset variable → empty string (not-configured marker).
                Err(_) => {}
            }
        } else {
            result.push(c);
        }
    }

    result
}

/// Extract the first `${ENV_VAR}` name from an api_key string.
/// Returns `None` if the string contains no `${VAR}` placeholder.
fn extract_env_var(s: &str) -> Option<String> {
    let start = s.find("${")?;
    let rest = &s[start + 2..];
    let end = rest.find('}')?;
    let var = &rest[..end];
    if var.is_empty() { None } else { Some(var.to_string()) }
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
            supports_vision: false,
            cost_per_million_input: Some(0.0),
            cost_per_million_output: Some(0.0),
            tier: tier.map(|s| s.into()),
            timeout_secs: None,
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
    fn test_resolve_id_or_name_by_exact_id() {
        let mut def = test_model_def("deepseek-v4-flash", None);
        def.name = "DeepSeek-v4-flash".into();
        let config = AgentConfig {
            models: crate::config::ModelCatalog {
                models: vec![def],
                tiers: None,
                role_tiers: None,
            },
            roles: Default::default(),
        };
        let resolver = ModelResolver::from_config(&config).unwrap();
        let m = resolver.resolve_id_or_name("deepseek-v4-flash").unwrap();
        assert_eq!(m.id, "deepseek-v4-flash");
    }

    #[test]
    fn test_resolve_id_or_name_by_case_insensitive_name() {
        let mut def = test_model_def("deepseek-v4-flash", None);
        def.name = "DeepSeek-v4-flash".into();
        let config = AgentConfig {
            models: crate::config::ModelCatalog {
                models: vec![def],
                tiers: None,
                role_tiers: None,
            },
            roles: Default::default(),
        };
        let resolver = ModelResolver::from_config(&config).unwrap();
        // Display name with original casing should match.
        let m = resolver.resolve_id_or_name("DeepSeek-v4-flash").unwrap();
        assert_eq!(m.id, "deepseek-v4-flash");
        // And a lowercased form should also match.
        let m = resolver.resolve_id_or_name("deepseek-v4-flash").unwrap();
        assert_eq!(m.id, "deepseek-v4-flash");
    }

    #[test]
    fn test_resolve_id_or_name_miss_lists_available() {
        let mut def = test_model_def("real-id", None);
        def.name = "Real Name".into();
        let config = AgentConfig {
            models: crate::config::ModelCatalog {
                models: vec![def],
                tiers: None,
                role_tiers: None,
            },
            roles: Default::default(),
        };
        let resolver = ModelResolver::from_config(&config).unwrap();
        let err = resolver
            .resolve_id_or_name("nope")
            .expect_err("should miss");
        let msg = format!("{}", err);
        assert!(msg.contains("nope"), "msg should contain query: {msg}");
        assert!(msg.contains("real-id"), "msg should list real id: {msg}");
        assert!(msg.contains("Real Name"), "msg should list real name: {msg}");
    }
    fn test_env_var_resolution() {
        std::env::set_var("TEST_KEY", "resolved-value");
        assert_eq!(resolve_env_vars("${TEST_KEY}"), "resolved-value");
        assert_eq!(
            resolve_env_vars("prefix_${TEST_KEY}_suffix"),
            "prefix_resolved-value_suffix"
        );
        // Unset env var → empty string (not-configured marker); the
        // three-layer merge decides whether the empty key is acceptable.
        std::env::remove_var("TEST_KEY");
        assert_eq!(resolve_env_vars("${DEFINITELY_UNSET}"), "");
    }
    // ─── resolve_chain tests ───────────────────────────────────────────

    fn catalog_with_three() -> AgentConfig {
        AgentConfig {
            models: crate::config::ModelCatalog {
                models: vec![
                    test_model_def("premium", Some("premium")),
                    test_model_def("standard", Some("standard")),
                    test_model_def("budget", Some("budget")),
                ],
                tiers: Some(
                    [
                        ("premium".into(), "premium".into()),
                        ("standard".into(), "standard".into()),
                        ("budget".into(), "budget".into()),
                    ]
                    .into_iter()
                    .collect(),
                ),
                role_tiers: None,
            },
            roles: Default::default(),
        }
    }

    #[test]
    fn test_resolve_chain_returns_primary_first() {
        let resolver = ModelResolver::from_config(&catalog_with_three()).unwrap();
        // chain[0] is the primary, tier-based primary appended as fallback.
        let chain = resolver
            .resolve_chain("any", ModelTier::Premium, &["standard".into()])
            .unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].id, "standard");
        assert_eq!(chain[1].id, "premium");
    }

    #[test]
    fn test_resolve_chain_preserves_order() {
        let resolver = ModelResolver::from_config(&catalog_with_three()).unwrap();
        // chain[0] becomes primary, then chain[1], then tier-based fallback.
        let chain = resolver
            .resolve_chain("any", ModelTier::Premium, &["budget".into(), "standard".into()])
            .unwrap();
        assert_eq!(
            chain.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["budget", "standard", "premium"],
            "chain order must be preserved, tier-based fallback appended"
        );
    }

    #[test]
    fn test_resolve_chain_dedups_primary() {
        // If the chain list also names the primary, the primary
        // must not appear twice.
        let resolver = ModelResolver::from_config(&catalog_with_three()).unwrap();
        let chain = resolver
            .resolve_chain("any", ModelTier::Premium, &["premium".into(), "budget".into()])
            .unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].id, "premium");
        assert_eq!(chain[1].id, "budget");
    }

    #[test]
    fn test_resolve_chain_drops_unknown_ids() {
        // Unknown ids in the chain are silently skipped — the agent
        // shouldn't fail just because a config listed a typo.
        let resolver = ModelResolver::from_config(&catalog_with_three()).unwrap();
        let chain = resolver
            .resolve_chain(
                "any",
                ModelTier::Premium,
                &["nonexistent".into(), "budget".into()],
            )
            .unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].id, "budget");
        assert_eq!(chain[1].id, "premium");
    }

    #[test]
    fn test_resolve_chain_with_empty_chain_returns_only_primary() {
        let resolver = ModelResolver::from_config(&catalog_with_three()).unwrap();
        let chain = resolver
            .resolve_chain("any", ModelTier::Premium, &[])
            .unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].id, "premium");
    }

    #[test]
    fn test_resolve_chain_errors_when_primary_unresolvable() {
        // With an empty catalog, the primary itself can't be resolved —
        // resolve_chain must surface that error rather than silently
        // returning an empty chain.
        let empty = AgentConfig::default();
        let resolver = ModelResolver::from_config(&empty).unwrap();
        let err = resolver
            .resolve_chain("any", ModelTier::Premium, &["fallback".into()])
            .expect_err("empty catalog should fail primary resolution");
        assert!(matches!(err, AgentError::ModelResolutionFailed { .. }));
    }
}
