//! Configuration types: agent roles, model catalog, TOML load/save.
//!
//! Mirrors gsd-core's `model-catalog.json` + `config-defaults.manifest.json` pattern,
//! but in TOML (Rust-native config format).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::error::{AgentError, AgentResult};
use crate::role::RoleTemplate;
use std::path::PathBuf;

/// Which layer of the layered config system we are loading from.
///
/// Both layers use the same directory layout: `agents/` for per-role
/// TOML files, `workflows/` for per-workflow TOML files, etc.
/// The only difference is the root directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigLayer {
    /// Project layer — `.latte/` under the current working directory.
    Project,
    /// Global layer — `$LATTE_HOME/` or `~/.latte/`.
    Global,
}

impl ConfigLayer {
    /// Return the root directory for this layer.
    ///
    /// - `Project` → `./.latte`
    /// - `Global` → `$LATTE_HOME` or `$HOME/.latte`
    pub fn root_dir(&self) -> Option<PathBuf> {
        match self {
            ConfigLayer::Project => Some(PathBuf::from(".latte")),
            ConfigLayer::Global => crate::global_config::GlobalConfig::global_dir(),
        }
    }

    /// Return the agents directory for this layer.
    ///
    /// The two layers use different layouts by convention:
    /// - **Project** prefers `.latte/agents/` (no `.d` suffix — the
    ///   layout this project ships), and falls back to
    ///   `.latte/agents.d/` for projects that follow the global
    ///   convention. The first directory that exists wins; if
    ///   neither exists the `.d/` form is returned so callers can
    ///   decide what to do with `None`.
    /// - **Global** always uses `agents.d/`.
    pub fn agents_dir(&self) -> Option<PathBuf> {
        match self {
            ConfigLayer::Project => {
                let root = self.root_dir()?;
                let no_d = root.join("agents");
                if no_d.is_dir() {
                    return Some(no_d);
                }
                Some(root.join("agents.d"))
            }
            ConfigLayer::Global => self.root_dir().map(|d| d.join("agents.d")),
        }
    }

    /// Return the workflows directory for this layer.
    /// Both project and global layers use `workflows.d/`.
    pub fn workflows_dir(&self) -> Option<PathBuf> {
        self.root_dir().map(|d| d.join("workflows.d"))
    }

    /// Return the logs root directory for this layer.
    pub fn logs_dir(&self) -> Option<PathBuf> {
        self.root_dir().map(|d| d.join("logs"))
    }

    /// Return the logs/agents directory for this layer.
    pub fn log_agents_dir(&self) -> Option<PathBuf> {
        self.root_dir().map(|d| d.join("logs").join("agents"))
    }
}

/// Merge `src` roles/tiers into `dst` using **field-level** semantics:
/// - New role ids from `src` are inserted as-is.
/// - Existing role ids get only **empty fields** filled from `src`
///   (`model_chain`, `icon`, `temperature`). Name, category,
///   model_tier, prompt_file, and tools stay with the project's value.
fn merge_global_into(dst: &mut HashMap<String, RoleTemplate>, src: &HashMap<String, RoleTemplate>) {
    for (id, role) in src {
        match dst.entry(id.clone()) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(role.clone());
            }
            std::collections::hash_map::Entry::Occupied(mut e) => {
                let existing = e.get_mut();
                // model_chain: global fills in if project has none.
                if existing.model_chain.is_empty() && !role.model_chain.is_empty() {
                    existing.model_chain = role.model_chain.clone();
                }
                // icon: global fills in if project has default empty.
                if existing.icon.is_empty() && !role.icon.is_empty() {
                    existing.icon = role.icon.clone();
                }
                // temperature: global fills in if project omitted it.
                if existing.temperature.is_none() && role.temperature.is_some() {
                    existing.temperature = role.temperature;
                }
            }
        }
    }
}

fn merge_models_or_insert(dst: &mut Vec<ModelDef>, src: &[ModelDef]) {
    for m in src {
        if !dst.iter().any(|e| e.id == m.id) {
            dst.push(m.clone());
        }
    }
}

fn merge_tiers_or_insert(dst: &mut Option<HashMap<String, String>>, src: &Option<HashMap<String, String>>) {
    if let Some(s) = src {
        let d = dst.get_or_insert_with(Default::default);
        for (k, v) in s {
            d.entry(k.clone()).or_insert(v.clone());
        }
    }
}

fn merge_role_tiers_or_insert(
    dst: &mut Option<HashMap<String, HashMap<String, String>>>,
    src: &Option<HashMap<String, HashMap<String, String>>>,
) {
    if let Some(s) = src {
        let d = dst.get_or_insert_with(Default::default);
        for (role, tier_map) in s {
            let entry = d.entry(role.clone()).or_insert_with(Default::default);
            for (k, v) in tier_map {
                entry.entry(k.clone()).or_insert(v.clone());
            }
        }
    }
}

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

/// Configuration for which hooks a role uses. Each variant selects a
/// built-in hook by kind and optionally provides its parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookKindConfig {
    /// No hook (default).
    None,
    /// Redact PII from outgoing messages.
    RedactPii,
    /// Enforce a tool allowlist (abort on disallowed calls).
    EnforceToolAllowlist,
    /// Require at least `min_words` in the model response.
    RequireToolCall {
        min_words: usize,
    },
    /// Monitor context token usage; abort or warn when approaching
    /// the configured budget.
    ContextMonitor {
        /// Soft threshold (0.0–1.0 of budget). Emit warning above this.
        warn_at: f64,
        /// Hard threshold (0.0–1.0 of budget). Abort above this.
        abort_at: f64,
    },
}

impl Default for HookKindConfig {
    fn default() -> Self { Self::None }
}

/// A single hook applied at a specific point in the workflow step
/// lifecycle. Mirrors the gsd-core `StepHook` concept.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepHookConfig {
    /// Which hook kind to install.
    pub kind: HookKindConfig,
    /// Optional role filter — only apply when the current role matches.
    /// Empty string or "*" means all roles.
    #[serde(default)]
    pub role_filter: String,
 }

impl AgentConfig {
    /// Load config from a path. If `path` is a directory, merge every
    /// `*.toml` inside (models and roles are concatenated). If it is a
    /// file, load that file directly.
    ///
    /// Directory mode supports the layout:
    ///   .latte/agents/
    ///     pm.toml
    ///     architect.toml
    ///     ...
    ///   .latte/workflows/
    ///     code_review.toml
    ///     ...
    pub fn load(path: &str) -> AgentResult<Self> {
        let meta = std::fs::metadata(path).map_err(|e| {
            AgentError::Config(format!("cannot stat config path '{}': {}", path, e))
        })?;

        if meta.is_dir() {
            return Self::load_dir(path);
        }

        let content = std::fs::read_to_string(path).map_err(|e| {
            AgentError::Config(format!("cannot read config file '{}': {}", path, e))
        })?;
        Self::parse(&content)
    }
    /// Load project + global configs and merge them.
    ///
    /// The two layers use different layouts via [`ConfigLayer`]:
    /// - Project: `ConfigLayer::Project.agents_dir()` — prefers
    ///   `./.latte/agents/`, falls back to `./.latte/agents.d/`.
    /// - Global:  `ConfigLayer::Global.agents_dir()` (`$LATTE_HOME/agents.d/` or `~/.latte/agents.d/`)
    ///
    /// **Project-wins** per role id: the project's version replaces
    /// the global one. The global layer only contributes ids the
    /// project did not declare.
    ///
    /// `project_path` — optional override for the project agents dir.
    /// When `None`, defaults to [`ConfigLayer::Project.agents_dir()`].
    ///
    /// Returns `Ok(default)` if neither layer has anything.
    pub fn load_with_global(project_path: Option<&str>) -> AgentResult<Self> {
        let mut merged = Self::default();

        // 1) Project layer: use explicit path or fallback to ConfigLayer.
        let project_dir: Option<String> = project_path
            .filter(|p| std::fs::metadata(p).is_ok())
            .map(|s| s.to_string())
            .or_else(|| {
                ConfigLayer::Project
                    .agents_dir()
                    .filter(|d| d.is_dir())
                    .and_then(|d| d.to_str().map(|s| s.to_string()))
            });
        if let Some(path) = project_dir {
            if let Ok(part) = Self::load(&path) {
                merged.roles.extend(part.roles);
                for m in part.models.models {
                    if !merged.models.models.iter().any(|e| e.id == m.id) {
                        merged.models.models.push(m);
                    }
                }
                if let Some(t) = part.models.tiers {
                    merged
                        .models
                        .tiers
                        .get_or_insert_with(Default::default)
                        .extend(t);
                }
                if let Some(rt) = part.models.role_tiers {
                    let dst = merged
                        .models
                        .role_tiers
                        .get_or_insert_with(Default::default);
                    for (role, tier_map) in rt {
                        dst.entry(role).or_insert_with(Default::default).extend(tier_map);
                    }
                }
            }
        }

        // 2) Global layer: always uses ConfigLayer::Global.
        if let Some(agents_dir) = ConfigLayer::Global.agents_dir() {
            if agents_dir.is_dir() {
                if let Ok(global_part) = Self::load(agents_dir.to_str().unwrap()) {
                    merge_global_into(&mut merged.roles, &global_part.roles);
                    merge_models_or_insert(&mut merged.models.models, &global_part.models.models);
                    merge_tiers_or_insert(&mut merged.models.tiers, &global_part.models.tiers);
                    merge_role_tiers_or_insert(&mut merged.models.role_tiers, &global_part.models.role_tiers);
                }
            }
        }

        // 3) Built-in role fallback.
        if merged.roles.is_empty() {
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
                if let Some(tmpl) = crate::prompts::template_for(id) {
                    merged.roles.insert(id.to_string(), tmpl);
                }
            }
        }

        Ok(merged)
    }

    /// Load every `*.toml` file in `dir` (non-recursive) and merge the
    /// results. A `roles.<id>` key collision across files is an error so
    /// users get immediate feedback on duplicate role definitions.
    fn load_dir(dir: &str) -> AgentResult<Self> {
        let mut merged = Self::default();
        let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
            .map_err(|e| AgentError::Config(format!("cannot read dir '{}': {}", dir, e)))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.extension().and_then(|s| s.to_str()) == Some("toml")
            })
            .collect();
        // Deterministic merge order.
        entries.sort();

        for entry in &entries {
            let content = std::fs::read_to_string(entry).map_err(|e| {
                AgentError::Config(format!(
                    "cannot read '{}': {}",
                    entry.display(),
                    e
                ))
            })?;
            let part = Self::parse(&content)?;

            // Roles: detect duplicate IDs.
            for id in part.roles.keys() {
                if merged.roles.contains_key(id) {
                    return Err(AgentError::Config(format!(
                        "duplicate role '{}' in {}",
                        id,
                        entry.display()
                    )));
                }
            }
            merged.roles.extend(part.roles);

            // Models: append.
            merged.models.models.extend(part.models.models);
            // Tier maps: merge shallowly (last writer wins per key).
            if let Some(tiers) = part.models.tiers {
                merged.models.tiers.get_or_insert_with(Default::default).extend(tiers);
            }
            if let Some(rt) = part.models.role_tiers {
                let dst = merged
                    .models
                    .role_tiers
                    .get_or_insert_with(Default::default);
                for (role, tier_map) in rt {
                    dst.entry(role).or_insert_with(Default::default).extend(tier_map);
                }
            }
        }

        Ok(merged)
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
/// optional `tier`, `supports_thinking`, and `timeout_secs` fields.
///
/// `timeout_secs` sets an optional per-turn wall-clock budget. Resolution
/// order:
/// 1. `model.timeout_secs` from this field (per-model override)
/// 2. Controller-specific env var (`LATTE_AGENT_TURN_TIMEOUT_SECS` or
///    `LATTE_AGENT_DELEGATE_TIMEOUT_SECS`)
/// 3. No controller-level timeout
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
    /// Whether the model can accept image inputs. Defaults to `false`
    /// so existing configs work without changes.
    #[serde(default)]
    pub supports_vision: bool,
    /// Cost per million input tokens (USD). Optional for local models.
    #[serde(default)]
    pub cost_per_million_input: Option<f64>,
    /// Cost per million output tokens (USD). Optional for local models.
    #[serde(default)]
    pub cost_per_million_output: Option<f64>,
    /// Which tier this model belongs to (for auto-resolution without explicit tier map).
    #[serde(default)]
    pub tier: Option<String>,
    /// Per-specialist wall-clock timeout (seconds) for the `delegate` tool.
    /// Default: 60s if not set; can be overridden per-model and via
    /// `LATTE_AGENT_DELEGATE_TIMEOUT_SECS`. Used by slow models (e.g. GLM 5.2
    /// on 8-step tasks) that need a longer budget than the global default.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
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
    #[test]
    fn test_load_dir_merges_role_files() {
        let tmp = std::env::temp_dir().join("latte_agent_test_dir_merge");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        std::fs::write(
            tmp.join("pm.toml"),
            r#"
[roles.pm]
id = "pm"
name = "PM"
category = "planning"
model_tier = "standard"
icon = "P"
"#,
        )
        .unwrap();
        std::fs::write(
            tmp.join("architect.toml"),
            r#"
[roles.architect]
id = "architect"
name = "Arch"
category = "planning"
model_tier = "premium"
icon = "A"
"#,
        )
        .unwrap();
        // Non-TOML files must be ignored.
        std::fs::write(tmp.join("README.md"), "ignore me").unwrap();

        let cfg = AgentConfig::load(tmp.to_str().unwrap()).unwrap();
        assert_eq!(cfg.roles.len(), 2);
        assert!(cfg.roles.contains_key("pm"));
        assert!(cfg.roles.contains_key("architect"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_load_dir_detects_duplicate_role() {
        let tmp = std::env::temp_dir().join("latte_agent_test_dir_dup");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let body = r#"
[roles.pm]
id = "pm"
name = "PM"
category = "planning"
model_tier = "standard"
"#;
        std::fs::write(tmp.join("a.toml"), body).unwrap();
        std::fs::write(tmp.join("b.toml"), body).unwrap();

        let err = AgentConfig::load(tmp.to_str().unwrap()).unwrap_err();
        assert!(format!("{}", err).contains("duplicate role 'pm'"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── load_with_global ────────────────────────────────────────────────

    /// Helper: redirect `LATTE_HOME` to a temp dir for the duration of a
    /// test, restoring the previous value afterwards. Takes a global
    /// mutex so concurrent test threads don't clobber each other's
    /// `LATTE_HOME` (env vars are process-wide, but cargo test runs
    /// each `#[test]` on a separate thread).
    fn with_latte_home<F: FnOnce(&std::path::Path)>(f: F) {
        let _guard = crate::test_util::ENV_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap();


        let tmp = std::env::temp_dir().join(format!(
            "latte_agent_test_home_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let prev = std::env::var("LATTE_HOME").ok();
        std::env::set_var("LATTE_HOME", &tmp);
        f(&tmp);
        match prev {
            Some(v) => std::env::set_var("LATTE_HOME", v),
            None => std::env::remove_var("LATTE_HOME"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_load_with_global_only_project() {
        // No global layer: project roles only.
        let tmp = std::env::temp_dir().join("latte_agent_test_lwg_project_only");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(
            tmp.join("pm.toml"),
            r#"
[roles.pm]
id = "pm"
name = "PM"
category = "planning"
model_tier = "standard"
icon = "P"
"#,
        )
        .unwrap();

        with_latte_home(|_home| {
            let cfg = AgentConfig::load_with_global(Some(tmp.to_str().unwrap())).unwrap();
            assert_eq!(cfg.roles.len(), 1);
            assert!(cfg.roles.contains_key("pm"));
        });
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_load_with_global_fills_missing_from_global() {
        // Project defines `pm`; global defines `architect`. Both should
        // appear in the merged result, with project-wins on collision.
        let project = std::env::temp_dir().join("latte_agent_test_lwg_project_fill");
        let _ = std::fs::remove_dir_all(&project);
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("pm.toml"),
            r#"
[roles.pm]
id = "pm"
name = "PM_from_project"
category = "planning"
model_tier = "standard"
icon = "P"
"#,
        )
        .unwrap();

        with_latte_home(|home| {
            let agents_dir = home.join("agents.d");
            std::fs::create_dir_all(&agents_dir).unwrap();
            std::fs::write(
                agents_dir.join("architect.toml"),
                r#"
[roles.architect]
id = "architect"
name = "Arch_from_global"
category = "planning"
model_tier = "premium"
icon = "A"
"#,
            )
            .unwrap();

            let cfg =
                AgentConfig::load_with_global(Some(project.to_str().unwrap())).unwrap();
            assert_eq!(cfg.roles.len(), 2);
            assert!(cfg.roles.contains_key("pm"));
            assert!(cfg.roles.contains_key("architect"));
            // Project-wins: PM came from project.
            assert_eq!(cfg.roles["pm"].name, "PM_from_project");
            // Architect came from global.
            assert_eq!(cfg.roles["architect"].name, "Arch_from_global");
        });
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn test_load_with_global_project_wins_on_collision() {
        // Same id `pm` in both layers: project version wins.
        let project = std::env::temp_dir().join("latte_agent_test_lwg_collision");
        let _ = std::fs::remove_dir_all(&project);
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("pm.toml"),
            r#"
[roles.pm]
id = "pm"
name = "PM_project"
category = "planning"
model_tier = "standard"
icon = "P"
"#,
        )
        .unwrap();

        with_latte_home(|home| {
            let agents_dir = home.join("agents.d");
            std::fs::create_dir_all(&agents_dir).unwrap();
            std::fs::write(
                agents_dir.join("pm.toml"),
                r#"
[roles.pm]
id = "pm"
name = "PM_global"
category = "planning"
model_tier = "standard"
icon = "G"
"#,
            )
            .unwrap();

            let cfg =
                AgentConfig::load_with_global(Some(project.to_str().unwrap())).unwrap();
            assert_eq!(cfg.roles.len(), 1);
            assert_eq!(cfg.roles["pm"].name, "PM_project");
            assert_eq!(cfg.roles["pm"].icon, "P");
        });
        let _ = std::fs::remove_dir_all(&project);
    }
    // ── load_with_global: built-in role fallback ──────────────────────

    #[test]
    fn test_load_with_global_falls_back_to_builtin_roles() {
        // No project, no global: only built-ins. We redirect
        // LATTE_HOME to a temp dir with no `agents.d/`, so the
        // global layer is empty, and the project path is a
        // nonexistent path so the project layer is also empty.
        with_latte_home(|_home| {
            let cfg = AgentConfig::load_with_global(Some(
                "/tmp/__definitely_nonexistent_for_builtin_test__",
            ))
            .unwrap();
            // We expect at minimum the 10 built-ins.
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
                    cfg.roles.contains_key(id),
                    "missing built-in role '{id}'"
                );
            }
        });
    }

    #[test]
    fn test_load_with_global_does_not_override_existing_with_builtin() {
        // Project defines `pm`; the built-in should NOT clobber it
        // (project-wins is the invariant). Built-ins are only used
        // when no layer contributes any role.
        let tmp = std::env::temp_dir().join("latte_agent_test_builtin_no_override");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(
            tmp.join("pm.toml"),
            r#"
[roles.pm]
id = "pm"
name = "PM_project_custom"
category = "planning"
model_tier = "standard"
icon = "P"
"#,
        )
        .unwrap();
        with_latte_home(|_home| {
            let cfg = AgentConfig::load_with_global(Some(tmp.to_str().unwrap())).unwrap();
            assert_eq!(cfg.roles["pm"].name, "PM_project_custom");
        });
        let _ = std::fs::remove_dir_all(&tmp);
    }


    #[test]
    fn test_load_with_global_only_global() {
        with_latte_home(|home| {
            let agents_dir = home.join("agents.d");
            std::fs::create_dir_all(&agents_dir).unwrap();
            std::fs::write(
                agents_dir.join("pm.toml"),
                r#"
[roles.pm]
id = "pm"
name = "PM_global_only"
category = "planning"
model_tier = "standard"
icon = "G"
"#,
            )
            .unwrap();

            let cfg = AgentConfig::load_with_global(None).unwrap();
            assert_eq!(cfg.roles.len(), 1);
            assert_eq!(cfg.roles["pm"].name, "PM_global_only");
        });
    }

    // ── agents_dir layout fallback (.latte/agents vs .latte/agents.d) ─

    /// Project `agents/` (no `.d`) is the layout this project ships.
    /// When that directory exists, `ConfigLayer::Project.agents_dir()`
    /// must return it directly so `load_with_global(None)` picks the
    /// roles up automatically.
    #[test]
    fn test_project_agents_dir_prefers_no_d_layout() {
        // Serialise on the same env lock `with_latte_home` uses, so
        // our `set_current_dir` + `LATTE_HOME` mutations can't race
        // with the other load_with_global tests.
        let _env_guard = crate::test_util::ENV_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap();

        let project = std::env::temp_dir().join("latte_agent_test_no_d_layout");
        let _ = std::fs::remove_dir_all(&project);
        let agents = project.join(".latte").join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("quick.toml"),
            r#"
[roles.quick]
id = "quick"
name = "Quick"
category = "execution"
model_tier = "budget"
icon = "Q"
"#,
        )
        .unwrap();

        // Cwd guard — restores cwd on Drop, even if assertions panic.
        let prev_cwd = std::env::current_dir().unwrap();
        struct CwdGuard(std::path::PathBuf);
        impl Drop for CwdGuard {
            fn drop(&mut self) {
                let _ = std::env::set_current_dir(&self.0);
            }
        }
        let _cwd_guard = CwdGuard(prev_cwd);
        std::env::set_current_dir(&project).unwrap();

        let resolved = ConfigLayer::Project.agents_dir()
            .expect("project agents_dir should resolve");
        assert!(
            resolved.ends_with("agents"),
            "expected no-`.d` layout, got {}",
            resolved.display()
        );
        assert!(resolved.is_dir(), "{} should be a directory", resolved.display());

        let _ = std::fs::remove_dir_all(&project);
    }

    /// When only `.latte/agents.d/` exists, `agents_dir()` must still
    /// return that path so legacy projects keep working.
    #[test]
    fn test_project_agents_dir_falls_back_to_d_layout() {
        let _env_guard = crate::test_util::ENV_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap();

        let project = std::env::temp_dir().join("latte_agent_test_d_layout_only");
        let _ = std::fs::remove_dir_all(&project);
        let agents_d = project.join(".latte").join("agents.d");
        std::fs::create_dir_all(&agents_d).unwrap();
        std::fs::write(
            agents_d.join("pm.toml"),
            r#"
[roles.pm]
id = "pm"
name = "PM"
category = "planning"
model_tier = "standard"
icon = "P"
"#,
        )
        .unwrap();

        let prev_cwd = std::env::current_dir().unwrap();
        struct CwdGuard(std::path::PathBuf);
        impl Drop for CwdGuard {
            fn drop(&mut self) {
                let _ = std::env::set_current_dir(&self.0);
            }
        }
        let _cwd_guard = CwdGuard(prev_cwd);
        std::env::set_current_dir(&project).unwrap();

        let resolved = ConfigLayer::Project.agents_dir()
            .expect("project agents_dir should resolve");
        assert!(
            resolved.ends_with("agents.d"),
            "expected `.d` fallback, got {}",
            resolved.display()
        );
        assert!(resolved.is_dir(), "{} should be a directory", resolved.display());

        let _ = std::fs::remove_dir_all(&project);
    }

    /// End-to-end: with `.latte/agents/quick.toml` present, calling
    /// `load_with_global(None)` from the project root must surface the
    /// `quick` role — this is what `latte-agent chat -r quick` relies on.
    #[test]
    fn test_load_with_global_picks_up_no_d_project_layout() {
        let _env_guard = crate::test_util::ENV_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap();

        let project = std::env::temp_dir().join("latte_agent_test_no_d_load");
        let _ = std::fs::remove_dir_all(&project);
        let agents = project.join(".latte").join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("quick.toml"),
            r#"
[roles.quick]
id = "quick"
name = "Quick"
category = "execution"
model_tier = "budget"
icon = "Q"
"#,
        )
        .unwrap();

        let prev_cwd = std::env::current_dir().unwrap();
        struct CwdGuard(std::path::PathBuf);
        impl Drop for CwdGuard {
            fn drop(&mut self) {
                let _ = std::env::set_current_dir(&self.0);
            }
        }
        let _cwd_guard = CwdGuard(prev_cwd);
        std::env::set_current_dir(&project).unwrap();

        // `None` forces the loader to use `ConfigLayer::Project.agents_dir()`.
        let cfg = AgentConfig::load_with_global(None).unwrap();
        assert!(
            cfg.roles.contains_key("quick"),
            "expected `quick` role from .latte/agents/ to be loaded; got: {:?}",
            cfg.roles.keys().collect::<Vec<_>>()
        );
        assert_eq!(cfg.roles["quick"].name, "Quick");
        assert_eq!(cfg.roles["quick"].icon, "Q");

        let _ = std::fs::remove_dir_all(&project);
    }
}
