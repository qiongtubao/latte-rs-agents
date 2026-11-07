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
    /// `[compaction]` 配置段。放进快照与 role_model_chains 同理：运行中的
    /// runner 持有 resolver，而它构建时的 AgentConfig 已经固化——把这段
    /// 放这里，「UI 改压缩参数 → 保存 → 已加载 session 下一轮生效」才成立。
    compaction: crate::config::CompactionSection,
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
            compaction: config.compaction.clone(),
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
    /// 当前生效的 `[compaction]` 配置段（**实时**，随 `reload_from_config`
    /// 更新）。
    ///
    /// 运行中的 runner 通过它拿最新压缩参数，而不是用构建时固化的
    /// AgentConfig ——「UI 改参数 → 保存 → 已加载 session 下一轮生效」
    /// 就靠这条路（与 role_model_chains 同款设计）。
    pub fn compaction_section(&self) -> crate::config::CompactionSection {
        self.inner.read().compaction.clone()
    }

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

    /// 哪些角色的**实际模型链**（与 runner / workflow 分派同一套解析：
    /// model_chain 优先，否则按 tier）用到了 `models` 里的模型。
    /// 返回 `(角色 id, 命中的模型 id)`，按角色 id 排序。余额不足提示用：
    /// 告诉用户「这些角色正在用没钱的账户」。
    pub fn roles_using_models(
        &self,
        models: &std::collections::BTreeSet<String>,
    ) -> Vec<(String, Vec<String>)> {
        let snap = self.snapshot();
        let mut roles: Vec<&String> = snap.role_model_chains.keys().collect();
        roles.sort();
        roles
            .into_iter()
            .filter_map(|role| {
                let chain = snap
                    .role_model_chains
                    .get(role)
                    .cloned()
                    .unwrap_or_default();
                let tier = snap
                    .role_model_tiers
                    .get(role)
                    .copied()
                    .unwrap_or(ModelTier::Standard);
                let hit: Vec<String> = snap
                    .resolve_chain(role, tier, &chain)
                    .ok()?
                    .into_iter()
                    .map(|m| m.id)
                    .filter(|id| models.contains(id))
                    .collect();
                (!hit.is_empty()).then(|| (role.clone(), hit))
            })
            .collect()
    }

    /// 角色的模型指派（model_chain + model_tier），来自最新配置快照。
    /// 角色编辑器保存后，运行中的 runner（turn 边界自查）与 workflow
    /// 分派（每次分派重新解析）据此热生效。角色未知 → None。
    pub fn role_model_assignment(&self, role_id: &str) -> Option<(Vec<String>, Option<ModelTier>)> {
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
    /// priority order: per-role override → global tier default.
    ///
    /// 只返回**显式配置**的候选。没有任何 role_tiers / tiers 映射命中时
    /// 返回空——调用方据此报 `ModelResolutionFailed`，绝不凭 catalog
    /// 内容推测一个未被指定的模型（旧版会这么做，见函数体注释）。
    fn candidates_for(&self, role_id: &str, tier: ModelTier) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut push = |id: &str| {
            if seen.insert(id.to_string()) {
                out.push(id.to_string());
            }
        };
        // 只认**显式配置**的两级：per-role 覆盖 → 全局 tier 默认。
        // 二者都是用户在 `[models.role_tiers.<role>]` / `[models.tiers]`
        // 里亲手写的映射，属于"角色配置的模型"。
        if let Some(role_map) = self.role_tiers.get(role_id) {
            if let Some(id) = role_map.get(&tier) {
                push(id);
            }
        }
        if let Some(id) = self.tier_defaults.get(&tier) {
            push(id);
        }
        // 不做 catalog 推测兜底。此前这里还有两步：
        //   3) 找 catalog 里任意 `tier` 字段匹配的模型；
        //   4) 直接取 catalog 第一个模型。
        // 两步都在"角色没有任何显式映射"时凭空挑一个模型顶上，与
        // resolve_chain 里被删的 last-resort 安全网同性质——实测会捞到
        // 用户想弃用、只因 `tier = "budget"` 还留在模型目录里的 deepseek。
        // 去掉后：没有任何显式映射的 tier 解析为空候选，resolve 直接报
        // ModelResolutionFailed，而不是偷换一个没人指定的模型。
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

        // 不设兜底网：chain / tier 候选全不可用就直接失败。
        //
        // 角色声明的 model_chain（chain 为空时则是 `[role_tiers]`/`[tiers]`
        // 的 tier 映射）就是它对模型的**完整意图**。此前这里有一张
        // last-resort 安全网：chain 全哑时遍历整个 catalog 抓第一个带
        // api_key 的模型顶上。它违背声明意图——实测事故：jemalloc 会话里
        // advisor 的 chain 是 ["glm-5.3","MiniMax-M3"]，两个都 timeout 后
        // 安全网捞出了用户早已从所有角色里删掉、只剩在模型目录中的
        // deepseek，接着报 402。用户"全局角色都改成只用 glm/minimax 了，
        // 为什么还在调 deepseek"无从排查，因为模型根本不来自任何角色配置。
        //
        // 正确行为是中止并报 ModelResolutionFailed，让调用方按既有降级路径
        // 处理：advisor 静默降级、delegate/workflow step 采纳 partial 或
        // 失败善后、顶层 chat 自动暂停等人工。要彻底不用某个模型，把它从
        // 角色 chain / tier 映射里去掉即可——不必再去模型目录里删它。
        if out.is_empty() {
            return Err(AgentError::ModelResolutionFailed {
                role: role_id.into(),
                tier: tier.label().into(),
                reason: format!(
                    "角色配置的模型全部不可用（chain + tier 候选均无法构建或缺 api_key）；\
                     不启用 catalog 兜底，直接中止。请检查角色 model_chain / tier 映射里的模型是否可用。\
                     chain_ids={:?}",
                    chain_ids
                ),
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
        let known: Vec<String> = self.models.values().map(|m| m.name.clone()).collect();
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
            prompt_cache: def.prompt_cache,
            prompt_cache_key: def.prompt_cache_key,
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
    if var.is_empty() {
        None
    } else {
        Some(var.to_string())
    }
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
            prompt_cache: false,
            prompt_cache_key: None,
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
            compaction: Default::default(),
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
    fn test_resolve_without_explicit_mapping_fails_no_catalog_guess() {
        // 回归：catalog 里有模型（带 api_key），但角色没有任何显式的
        // tier 映射（role_tiers / tiers 都没命中），且 tier 字段也不匹配。
        // 旧行为会凭"catalog 第一个模型"或"tier 字段匹配"兜底挑一个；
        // 新契约是直接失败，绝不偷换一个用户没指定的模型。
        let config = AgentConfig {
            advisor: Default::default(),
            compaction: Default::default(),
            models: crate::config::ModelCatalog {
                // 只有一个 premium 模型，且无 tiers 映射。
                models: vec![test_model_def("some-model", Some("premium"))],
                tiers: None,
                role_tiers: None,
            },
            roles: Default::default(),
        };

        let resolver = ModelResolver::from_config(&config).unwrap();
        // 按 Budget 解析：没有 budget 映射、也不该拿 premium 模型兜底。
        let err = resolver
            .resolve("any_role", ModelTier::Budget)
            .expect_err("无显式映射必须失败，不得凭 catalog 推测");
        assert!(matches!(err, AgentError::ModelResolutionFailed { .. }));
    }

    #[test]
    fn test_resolve_chain_aborts_when_chain_unavailable_no_catalog_fallback() {
        // 核心回归：角色 chain 指向的模型全不可用（此处用未知 id 模拟
        // "构建失败/缺 key"），catalog 里却躺着一个带 key 的"弃用模型"
        // （相当于只剩在模型目录里的 deepseek）。必须中止，绝不捞它。
        let config = AgentConfig {
            advisor: Default::default(),
            compaction: Default::default(),
            models: crate::config::ModelCatalog {
                // 这个带 key 的模型既不在 chain 里、也没有任何 tier 映射
                // 指向它——它就是旧安全网会捞出来的 deepseek 替身。
                models: vec![test_model_def("unwanted-deepseek", Some("budget"))],
                tiers: None,
                role_tiers: None,
            },
            roles: Default::default(),
        };
        let resolver = ModelResolver::from_config(&config).unwrap();
        let err = resolver
            .resolve_chain(
                "advisor",
                ModelTier::Premium,
                &["glm-5.3".into(), "MiniMax-M3".into()],
            )
            .expect_err("chain 全不可用必须中止，不得兜底到 catalog 里的 unwanted-deepseek");
        match err {
            AgentError::ModelResolutionFailed { role, .. } => assert_eq!(role, "advisor"),
            other => panic!("期望 ModelResolutionFailed，实际 {other:?}"),
        }
    }

    #[test]
    fn test_resolve_id_or_name_by_exact_id() {
        let mut def = test_model_def("deepseek-v4-flash", None);
        def.name = "DeepSeek-v4-flash".into();
        let config = AgentConfig {
            advisor: Default::default(),
            compaction: Default::default(),
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
            compaction: Default::default(),
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
            compaction: Default::default(),
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
        assert!(
            msg.contains("Real Name"),
            "msg should list real name: {msg}"
        );
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
            compaction: Default::default(),
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

    /// 角色声明的 chain 就是全部意图：catalog 里躺着别的带 key 的模型
    /// （如只剩在 ~/.latte/models.d 里的 deepseek）不得进入解析结果；
    /// 声明的模型不可用时必须报错中止，而不是换成它。
    #[test]
    fn declared_chain_is_complete_intent_undeclared_catalog_models_never_used() {
        let mut glm = test_model_def("glm-5.3", None);
        glm.api_key = "glm-key".into();
        let mut ds = test_model_def("deepseek-v4-pro", None);
        ds.api_key = "ds-key".into();
        let cfg = catalog_with(vec![glm.clone(), ds.clone()]);
        let resolver = ModelResolver::from_config(&cfg).unwrap();

        // 声明的模型可用 → 只返回它，不掺 deepseek。
        let chain = resolver
            .resolve_chain("advisor", ModelTier::Premium, &["glm-5.3".into()])
            .unwrap();
        assert_eq!(
            chain.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            vec!["glm-5.3"],
            "未声明的 catalog 模型不得出现在解析结果里"
        );

        // 声明的模型没有 key → 报错中止，绝不降级到有 key 的 deepseek。
        glm.api_key = String::new();
        let cfg = catalog_with(vec![glm, ds]);
        let resolver = ModelResolver::from_config(&cfg).unwrap();
        let err = resolver
            .resolve_chain("advisor", ModelTier::Premium, &["glm-5.3".into()])
            .expect_err("声明的模型不可用必须中止，不得换成 catalog 里别的模型");
        assert!(matches!(err, AgentError::ModelResolutionFailed { .. }));
    }

    /// 角色模型指派进快照并随 reload 热更新（角色编辑器保存场景）。
    #[test]
    fn role_model_assignment_hot_updates_on_reload() {
        let mut cfg = catalog_with(vec![test_model_def("m1", None)]);
        cfg.roles.insert(
            "architect".into(),
            role_tpl("architect", "standard", vec!["m1"]),
        );
        let resolver = ModelResolver::from_config(&cfg).unwrap();
        let (chain, tier) = resolver.role_model_assignment("architect").unwrap();
        assert_eq!(chain, vec!["m1".to_string()]);
        assert_eq!(tier, Some(ModelTier::Standard));
        assert!(resolver.role_model_assignment("ghost").is_none());

        // 角色编辑器保存：换 chain + tier → reload 后读到新值。
        cfg.roles
            .insert("architect".into(), role_tpl("architect", "premium", vec![]));
        resolver.reload_from_config(&cfg).unwrap();
        let (chain, tier) = resolver.role_model_assignment("architect").unwrap();
        assert!(chain.is_empty(), "空 chain 原样透出（调用方回退旧链）");
        assert_eq!(tier, Some(ModelTier::Premium));
    }

    // ─── resolve_chain tests ───────────────────────────────────────────

    /// 动态生效：`reload_from_config` 之后，长存 resolver 交出的
    /// `[compaction]` 必须是**新值**。
    ///
    /// 这是「UI 改压缩参数 → 保存 → 已加载 session 下一轮生效」的直接
    /// 证明：运行中的 runner 持有 resolver，而它构建时的 AgentConfig 已经
    /// 固化——若配置段不随 resolver 更新，UI 上改完只能等重开会话。
    #[test]
    fn compaction_section_hot_reloads_with_the_resolver() {
        use crate::config::CompactionSection;
        let mut cfg = catalog_with_three();
        cfg.compaction = CompactionSection {
            threshold_ratio: Some(0.7),
            keep_recent_turns: Some(3),
            ..Default::default()
        };
        let resolver = ModelResolver::from_config(&cfg).unwrap();
        let before = resolver.compaction_section();
        assert_eq!(before.threshold_ratio, Some(0.7));
        assert_eq!(before.keep_recent_turns, Some(3));
        let gen_before = resolver.generation();

        // 模拟 UI 保存：改配置 → reload。
        cfg.compaction = CompactionSection {
            threshold_ratio: Some(0.9),
            keep_recent_turns: Some(8),
            enabled: Some(false),
            ..Default::default()
        };
        resolver.reload_from_config(&cfg).unwrap();

        let after = resolver.compaction_section();
        assert_eq!(after.threshold_ratio, Some(0.9), "reload 后必须读到新阈值");
        assert_eq!(after.keep_recent_turns, Some(8));
        assert_eq!(after.enabled, Some(false), "开关也要热生效");
        assert!(
            resolver.generation() > gen_before,
            "代际必须推进，长存 runner 据此感知变更"
        );
    }

    /// `prompt_cache_key`（缓存亲和键开关）同样要从 TOML 透传。
    ///
    /// 它与 `prompt_cache` 的默认值**相反**：不写 = 跟随全局默认（开），
    /// 因为它是 OpenAI 官方文档化的顶层参数，兼容风险远低于把 Anthropic
    /// 的 `cache_control` 塞进 OpenAI body。
    #[test]
    fn prompt_cache_key_flows_from_toml_to_model() {
        let base = r#"
name = "m"
api = "openai"
provider = "p"
base_url = "http://x"
api_key = "k"
context_window = 1000
max_tokens = 100
supports_thinking = false
"#;
        // 不写 → None（= 跟随全局默认）
        let def: crate::config::ModelDef = toml::from_str(base).expect("deserialize");
        assert_eq!(def.prompt_cache_key, None, "缺省必须是「未声明」");

        // 显式关 → 透传 Some(false)
        let def_off: crate::config::ModelDef = toml::from_str(&format!(
            "{base}prompt_cache_key = false
"
        ))
        .expect("deserialize");
        assert_eq!(def_off.prompt_cache_key, Some(false));

        let cfg = AgentConfig {
            advisor: Default::default(),
            compaction: Default::default(),
            models: crate::config::ModelCatalog {
                models: vec![def_off],
                tiers: None,
                role_tiers: None,
            },
            roles: Default::default(),
        };
        let resolver = ModelResolver::from_config(&cfg).unwrap();
        let model = resolver.resolve_id_or_name("m").expect("按 id 解析");
        assert_eq!(
            model.prompt_cache_key,
            Some(false),
            "模型级关闭必须一路传到 latte_ai::Model，否则配了也没用"
        );
    }

    /// prompt_cache 必须从 TOML 一路透传到 `latte_ai::Model`。
    ///
    /// 只测「决策逻辑」不够——这类字段的经典失效是**算对了但没进请求**
    /// （见 latte-ai 那次 max_tokens：目录配 384000、529 次请求全发 None）。
    /// 这里钉的是配置 → ModelDef → Model 这一段；wire 侧由 latte-ai 的
    /// openai_cache_breakpoints_* 测试覆盖。
    #[test]
    fn prompt_cache_flows_from_toml_to_model() {
        // 1. TOML 缺省 = false（不认 cache_control 的端点不会被打挂）。
        let def: crate::config::ModelDef = toml::from_str(
            r#"
name = "m"
api = "openai"
provider = "p"
base_url = "http://x"
api_key = "k"
context_window = 1000
max_tokens = 100
supports_thinking = false
"#,
        )
        .expect("ModelDef 应能在不写 prompt_cache 时反序列化");
        assert!(!def.prompt_cache, "缺省必须是关");

        // 2. 显式打开 → 透传到 Model。
        let def_on: crate::config::ModelDef = toml::from_str(
            r#"
name = "m"
api = "openai"
provider = "p"
base_url = "http://x"
api_key = "k"
context_window = 1000
max_tokens = 100
supports_thinking = false
prompt_cache = true
"#,
        )
        .expect("显式 prompt_cache 应能反序列化");
        assert!(def_on.prompt_cache);

        let cfg = AgentConfig {
            advisor: Default::default(),
            compaction: Default::default(),
            models: crate::config::ModelCatalog {
                models: vec![def_on],
                tiers: None,
                role_tiers: None,
            },
            roles: Default::default(),
        };
        let resolver = ModelResolver::from_config(&cfg).unwrap();
        let model = resolver.resolve_id_or_name("m").expect("按 id 解析");
        assert!(
            model.prompt_cache,
            "配置里开了 prompt_cache，解析出的 Model 必须带着它——否则断点永远不下发"
        );
    }

    fn catalog_with_three() -> AgentConfig {
        AgentConfig {
            advisor: Default::default(),
            compaction: Default::default(),
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
            .resolve_chain(
                "any",
                ModelTier::Premium,
                &["budget".into(), "standard".into()],
            )
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
            .resolve_chain(
                "any",
                ModelTier::Premium,
                &["premium".into(), "budget".into()],
            )
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
