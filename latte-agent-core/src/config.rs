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
    /// Lookup order:
    /// 1. `project_path` if it exists (file or directory). Missing
    ///    path is silently skipped.
    /// 2. `~/.latte/agents.d/` (directory) if it exists.
    /// 3. `~/.latte/agents.toml` (single file) if it exists.
    ///
    /// Roles and models with the same `id` in both layers are
    /// **project-wins**: the project's version replaces the global
    /// one, so a project can override a default shipped globally. The
    /// global layer only contributes ids the project did not declare.
    ///
    /// Returns `Ok(default)` if neither layer has anything.
    pub fn load_with_global(project_path: Option<&str>) -> AgentResult<Self> {
        use crate::global_config::GlobalConfig;
        let mut merged = Self::default();

        // 1) Project path. A missing path is silently skipped so a
        //    bare `~/.latte/agents.d` setup can work with no project
        //    config at all.
        if let Some(path) = project_path {
            if std::fs::metadata(path).is_ok() {
                let part = Self::load(path)?;
                // Roles: project goes in unconditionally (project-wins
                // is implemented at merge-into-global time below).
                merged.roles.extend(part.roles);
                // Models: project goes in unconditionally; the global
                // model layer will be merged on top later if the
                // caller wants field-filling semantics, but here we
                // just preserve the union.
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

        // 2) Global layer. We try the canonical `$LATTE_HOME` (or
        //    `~/.latte/`) for an `agents.d/` directory and a legacy
        //    `agents.toml` single file. Both are optional.
        if let Some(global_dir) = GlobalConfig::global_dir() {
            // 2a) Directory of per-role toml files.
            let agents_dir = global_dir.join("agents.d");
            if std::fs::metadata(&agents_dir)
                .map(|m| m.is_dir())
                .unwrap_or(false)
            {
                let global_part = Self::load(agents_dir.to_str().unwrap())?;
                // Project-wins per role id.
                for (id, role) in global_part.roles {
                    merged.roles.entry(id).or_insert(role);
                }
                // Models: only add ids the project did not define.
                for m in global_part.models.models {
                    if !merged.models.models.iter().any(|e| e.id == m.id) {
                        merged.models.models.push(m);
                    }
                }
                if let Some(t) = global_part.models.tiers {
                    let dst = merged
                        .models
                        .tiers
                        .get_or_insert_with(Default::default);
                    for (k, v) in t {
                        dst.entry(k).or_insert(v);
                    }
                }
                if let Some(rt) = global_part.models.role_tiers {
                    let dst = merged
                        .models
                        .role_tiers
                        .get_or_insert_with(Default::default);
                    for (role, tier_map) in rt {
                        let entry = dst.entry(role).or_insert_with(Default::default);
                        for (k, v) in tier_map {
                            entry.entry(k).or_insert(v);
                        }
                    }
                }
            }
            // 2b) Legacy single-file ~/.latte/agents.toml.
            let single = global_dir.join("agents.toml");
            if single.is_file() {
                let global_part = Self::load(single.to_str().unwrap())?;
                for (id, role) in global_part.roles {
                    merged.roles.entry(id).or_insert(role);
                }
                for m in global_part.models.models {
                    if !merged.models.models.iter().any(|e| e.id == m.id) {
                        merged.models.models.push(m);
                    }
                }
                if let Some(t) = global_part.models.tiers {
                    let dst = merged
                        .models
                        .tiers
                        .get_or_insert_with(Default::default);
                    for (k, v) in t {
                        dst.entry(k).or_insert(v);
                    }
                }
                if let Some(rt) = global_part.models.role_tiers {
                    let dst = merged
                        .models
                        .role_tiers
                        .get_or_insert_with(Default::default);
                    for (role, tier_map) in rt {
                        let entry = dst.entry(role).or_insert_with(Default::default);
                        for (k, v) in tier_map {
                            entry.entry(k).or_insert(v);
                        }
                    }
                }
            }
        }

        // 3) Built-in role fallback. If neither the project nor the
        //    global layer declared any role, the binary still works
        //    using the 10 hard-coded defaults from `prompts.rs`.
        //    This makes `latte-agent` runnable on a fresh checkout
        //    with no config files at all.
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
        let cfg = AgentConfig::load_with_global(Some(tmp.to_str().unwrap())).unwrap();
        assert_eq!(cfg.roles["pm"].name, "PM_project_custom");
        let _ = std::fs::remove_dir_all(&tmp);
    }


    #[test]
    fn test_load_with_global_only_global() {
        // Project path is None; global layer supplies everything.
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
}
