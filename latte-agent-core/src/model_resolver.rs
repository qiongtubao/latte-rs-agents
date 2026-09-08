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
///
/// 内部状态是 `Arc<Snapshot>` 整体替换（`reload_from_config`）：UI 保存
/// 模型配置后换入新快照，正在解析的调用方仍持旧快照完成当次解析，
/// 下一次解析自动看到新值——运行中的 session 无需重启即可热更新模型
/// 配置。读路径只克隆一次 Arc，没有锁递归问题。
#[derive(Clone)]
pub struct ModelResolver {
    inner: std::sync::Arc<parking_lot::RwLock<std::sync::Arc<ResolverSnapshot>>>,
    /// 配置代际：`reload_from_config` 每次 +1。长存 runner 据此判断
    /// 要不要重建自己的 model chain（见 `AgentRunner::maybe_reload_models`）。
    generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

/// 解析器内部状态的一次性快照。
#[derive(Debug, Clone)]
struct ResolverSnapshot {
    /// All known models indexed by id.
    models: std::collections::HashMap<String, ModelDef>,
    /// Global tier → model_id mapping.
    tier_defaults: std::collections::HashMap<ModelTier, String>,
    /// Per-role tier overrides: role_id → (tier → model_id).
    role_tiers: std::collections::HashMap<String, std::collections::HashMap<ModelTier, String>>,
    /// 角色编辑器保存的 model_chain（`[roles.<id>].model_chain`）。
    /// 放进 resolver 快照是为了让运行中的 runner / workflow 分派在
    /// 「角色模型修改 → 保存」后热生效——它们都持有 resolver，而
    /// AgentConfig 在 session/workflow 启动时已固化快照。
    role_model_chains: std::collections::HashMap<String, Vec<String>>,
    /// 角色编辑器保存的 model_tier（解析失败的项跳过，调用方回退
    /// 到自己构建时的 tier）。
    role_model_tiers: std::collections::HashMap<String, ModelTier>,
}

impl ResolverSnapshot {
    fn from_config(config: &AgentConfig) -> AgentResult<Self> {
        let models: std::collections::HashMap<String, ModelDef> = config
            .models
            .models
            .iter()
            .map(|m| (m.name.clone(), m.clone()))
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

        let role_model_chains = config
            .roles
            .iter()
            .map(|(id, tpl)| (id.clone(), tpl.model_chain.clone()))
            .collect();
        let role_model_tiers = config
            .roles
            .iter()
            .filter_map(|(id, tpl)| {
                ModelTier::parse(&tpl.model_tier)
                    .ok()
                    .map(|t| (id.clone(), t))
            })
            .collect();

        Ok(Self {
            models,
            tier_defaults,
            role_tiers,
            role_model_chains,
            role_model_tiers,
        })
    }
}

impl ModelResolver {
    /// Build a resolver from an `AgentConfig`.
    pub fn from_config(config: &AgentConfig) -> AgentResult<Self> {
        Ok(Self {
            inner: std::sync::Arc::new(parking_lot::RwLock::new(std::sync::Arc::new(
                ResolverSnapshot::from_config(config)?,
            ))),
            generation: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        })
    }

    /// 用热更新后的 config 整体替换内部快照，代际 +1。已持有旧快照的
    /// 解析不受影响；下一次解析（新 session / 下一个 workflow step
    /// 分派 / 下一次 delegate / 长存 runner 的 turn 边界自查）自动
    /// 生效。
    pub fn reload_from_config(&self, config: &AgentConfig) -> AgentResult<()> {
        let snap = ResolverSnapshot::from_config(config)?;
        *self.inner.write() = std::sync::Arc::new(snap);
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// 当前配置代际（每次 `reload_from_config` 递增）。
    pub fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 拿当前快照（O(1) Arc clone，拿完即放锁）。
    fn snapshot(&self) -> std::sync::Arc<ResolverSnapshot> {
        self.inner.read().clone()
    }

    /// 见 [`ResolverSnapshot::resolve`]。
    pub fn resolve(&self, role_id: &str, tier: ModelTier) -> AgentResult<Model> {
        self.snapshot().resolve(role_id, tier)
    }

    /// 见 [`ResolverSnapshot::resolve_chain`]。
    pub fn resolve_chain(
        &self,
        role_id: &str,
        tier: ModelTier,
        chain_ids: &[String],
    ) -> AgentResult<Vec<Model>> {
        self.snapshot().resolve_chain(role_id, tier, chain_ids)
    }

    /// 见 [`ResolverSnapshot::available_tiers`]。
    pub fn available_tiers(&self, role_id: &str) -> Vec<ModelTier> {
        self.snapshot().available_tiers(role_id)
    }

    /// 见 [`ResolverSnapshot::resolve_id_or_name`]。
    pub fn resolve_id_or_name(&self, query: &str) -> AgentResult<Model> {
        self.snapshot().resolve_id_or_name(query)
    }

    /// 见 [`ResolverSnapshot::build_model`]。
    pub fn build_model(&self, model_id: &str) -> AgentResult<Model> {
        self.snapshot().build_model(model_id)
    }

    /// 见 [`ResolverSnapshot::get_def`]（返回克隆值——快照随时可能被
    /// 热替换，借用无法安全返回）。
    pub fn get_def(&self, model_id: &str) -> Option<ModelDef> {
        self.snapshot().get_def(model_id).cloned()
    }

    /// 角色的模型指派（model_chain + model_tier），来自最新配置快照。
    /// 角色编辑器保存后，运行中的 runner（turn 边界自查）与 workflow
    /// 分派（每次分派重新解析）据此热生效。角色未知 → None。
    pub fn role_model_assignment(
        &self,
        role_id: &str,
    ) -> Option<(Vec<String>, Option<ModelTier>)> {
        let snap = self.snapshot();
        snap.role_model_chains
            .get(role_id)
            .map(|chain| (chain.clone(), snap.role_model_tiers.get(role_id).copied()))
    }
}

impl ResolverSnapshot {
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
            push(&def.name);
        }
        if let Some(def) = self.models.values().next() {
            push(&def.name);
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
                if out.iter().any(|m| &m.name == id) {
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
                if !out.iter().any(|m| &m.name == &primary.name) {
                    out.push(primary);
                }
            }
        }

        // Last-resort safety net: walk the full catalog for any model
        // with a valid `api_key`.
        if out.is_empty() {
            for def in self.models.values() {
                if let Ok(m) = self.build_model(&def.name) {
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
            .map(|m| m.name.as_str())
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
            .map(|m| m.name.clone())
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
                    other, def.name
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
            id: def.name.clone(),
            name: def.name.clone(),
            api: api_type,
            provider: def.provider.clone(),
            base_url: def.base_url.clone(),
            api_key,
            context_window: def.context_window,
            max_tokens: def.max_tokens,
            omit_max_tokens: def.omit_max_tokens,
            max_tokens_field: def.max_tokens_field,
            supports_thinking: def.supports_thinking,
            supports_vision: def.supports_vision,
            cost_per_million_input: def.cost_per_million_input.unwrap_or(0.0),
            cost_per_million_output: def.cost_per_million_output.unwrap_or(0.0),
            timeout_secs: def.timeout_secs,
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
pub fn resolve_env_vars(s: &str) -> String {
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
        let snap = self.snapshot();
        f.debug_struct("ModelResolver")
            .field("models", &snap.models.len())
            .field("tier_defaults", &snap.tier_defaults)
            .field("role_tiers", &snap.role_tiers)
            .field("generation", &self.generation())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelDef;

    fn test_model_def(id: &str, tier: Option<&str>) -> ModelDef {
        // 用 `id` 作为 API 标识（即 `name` 字段），display name 用 human-friendly 形式。
        ModelDef {
            name: id.into(),
            api: "openai".into(),
            provider: "test".into(),
            base_url: "http://localhost".into(),
            api_key: "test-key".into(),
            context_window: 32000,
            max_tokens: 4096,
            omit_max_tokens: false,
            max_tokens_field: Default::default(),
            supports_thinking: false,
            supports_vision: false,
            supports_image_generation: false,
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
            advisor: Default::default(),
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
        assert_eq!(model.name, "standard-model");
    }

    #[test]
    fn test_resolve_fallback() {
        let config = AgentConfig {
            advisor: Default::default(),
            models: crate::config::ModelCatalog {
                models: vec![test_model_def("only-model", None)],
                tiers: None,
                role_tiers: None,
            },
            roles: Default::default(),
        };

        let resolver = ModelResolver::from_config(&config).unwrap();
        let model = resolver.resolve("any_role", ModelTier::Budget).unwrap();
        assert_eq!(model.name, "only-model");
    }

    #[test]
    fn test_resolve_id_or_name_by_exact_id() {
        let mut def = test_model_def("deepseek-v4-flash", None);
        def.name = "DeepSeek-v4-flash".into();
        let config = AgentConfig {
            advisor: Default::default(),
            models: crate::config::ModelCatalog {
                models: vec![def],
                tiers: None,
                role_tiers: None,
            },
            roles: Default::default(),
        };
        let resolver = ModelResolver::from_config(&config).unwrap();
        let m = resolver.resolve_id_or_name("deepseek-v4-flash").unwrap();
        assert_eq!(m.name, "DeepSeek-v4-flash");
        let _ = m;
    }
    #[test]
    fn test_resolve_id_or_name_by_case_insensitive_name() {
        let mut def = test_model_def("deepseek-v4-flash", None);
        def.name = "DeepSeek-v4-flash".into();
        let config = AgentConfig {
            advisor: Default::default(),
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
        assert_eq!(m.name, "DeepSeek-v4-flash");
        // And a lowercased form should also match.
        let m = resolver.resolve_id_or_name("deepseek-v4-flash").unwrap();
        assert_eq!(m.name, "DeepSeek-v4-flash");
    }

    #[test]
    fn test_resolve_id_or_name_miss_lists_available() {
        let mut def = test_model_def("real-id", None);
        def.name = "Real Name".into();
        let config = AgentConfig {
            advisor: Default::default(),
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

    // ─── 热更新（reload_from_config / generation） ─────────────────────

    fn catalog_with(models: Vec<ModelDef>) -> AgentConfig {
        AgentConfig {
            advisor: Default::default(),
            models: crate::config::ModelCatalog {
                models,
                tiers: None,
                role_tiers: None,
            },
            roles: Default::default(),
        }
    }

    /// reload 后代际递增，且后续解析看到新配置（改 api_key 场景：
    /// 旧 resolver 对象不用替换，内容热更新）。
    #[test]
    fn reload_from_config_bumps_generation_and_swaps_snapshot() {
        let resolver =
            ModelResolver::from_config(&catalog_with(vec![test_model_def("m1", None)])).unwrap();
        assert_eq!(resolver.generation(), 0);
        assert_eq!(resolver.build_model("m1").unwrap().api_key, "test-key");

        let mut def = test_model_def("m1", None);
        def.api_key = "new-key".into();
        resolver
            .reload_from_config(&catalog_with(vec![def]))
            .unwrap();
        assert_eq!(resolver.generation(), 1);
        assert_eq!(resolver.build_model("m1").unwrap().api_key, "new-key");

        // clone 共享内部状态：ui-server 各处持有的 resolver clone
        // 必须看到同一次热更新。
        let clone = resolver.clone();
        assert_eq!(clone.generation(), 1);
        assert_eq!(clone.build_model("m1").unwrap().api_key, "new-key");
    }

    /// reload 出非法配置（坏 tier 字符串）→ 报错且旧快照保留。
    #[test]
    fn reload_from_config_rejects_bad_config_and_keeps_old_snapshot() {
        let resolver =
            ModelResolver::from_config(&catalog_with(vec![test_model_def("m1", None)])).unwrap();
        let mut bad = catalog_with(vec![test_model_def("m1", None)]);
        bad.models.tiers = Some(
            [("not-a-tier".to_string(), "m1".to_string())]
                .into_iter()
                .collect(),
        );
        assert!(resolver.reload_from_config(&bad).is_err());
        assert_eq!(resolver.generation(), 0, "失败不推进代际");
        assert!(resolver.build_model("m1").is_ok(), "旧快照保留");
    }

    fn role_tpl(id: &str, tier: &str, chain: Vec<&str>) -> crate::role::RoleTemplate {
        crate::role::RoleTemplate {
            id: id.into(),
            name: id.into(),
            category: "engineering".into(),
            model_tier: tier.into(),
            model_chain: chain.into_iter().map(|s| s.to_string()).collect(),
            prompt_file: None,
            temperature: None,
            tools: vec![],
            icon: String::new(),
            skills: vec![],
            code_paths: vec![],
            description: String::new(),
        }
    }

    /// 角色模型指派进快照并随 reload 热更新（角色编辑器保存场景）。
    #[test]
    fn role_model_assignment_hot_updates_on_reload() {
        let mut cfg = catalog_with(vec![test_model_def("m1", None)]);
        cfg.roles.insert("architect".into(), role_tpl("architect", "standard", vec!["m1"]));
        let resolver = ModelResolver::from_config(&cfg).unwrap();
        let (chain, tier) = resolver.role_model_assignment("architect").unwrap();
        assert_eq!(chain, vec!["m1".to_string()]);
        assert_eq!(tier, Some(ModelTier::Standard));
        assert!(resolver.role_model_assignment("ghost").is_none());

        // 角色编辑器保存：换 chain + tier → reload 后读到新值。
        cfg.roles.insert("architect".into(), role_tpl("architect", "premium", vec![]));
        resolver.reload_from_config(&cfg).unwrap();
        let (chain, tier) = resolver.role_model_assignment("architect").unwrap();
        assert!(chain.is_empty(), "空 chain 原样透出（调用方回退旧链）");
        assert_eq!(tier, Some(ModelTier::Premium));
    }

    // ─── resolve_chain tests ───────────────────────────────────────────

    fn catalog_with_three() -> AgentConfig {
        AgentConfig {
            advisor: Default::default(),
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
        assert_eq!(chain[0].name, "standard");
        assert_eq!(chain[1].name, "premium");
    }

    #[test]
    fn test_resolve_chain_preserves_order() {
        let resolver = ModelResolver::from_config(&catalog_with_three()).unwrap();
        // chain[0] becomes primary, then chain[1], then tier-based fallback.
        let chain = resolver
            .resolve_chain("any", ModelTier::Premium, &["budget".into(), "standard".into()])
            .unwrap();
        assert_eq!(
            chain.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
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
        assert_eq!(chain[0].name, "premium");
        assert_eq!(chain[1].name, "budget");
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
        assert_eq!(chain[0].name, "budget");
        assert_eq!(chain[1].name, "premium");
    }

    #[test]
    fn test_resolve_chain_with_empty_chain_returns_only_primary() {
        let resolver = ModelResolver::from_config(&catalog_with_three()).unwrap();
        let chain = resolver
            .resolve_chain("any", ModelTier::Premium, &[])
            .unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].name, "premium");
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
