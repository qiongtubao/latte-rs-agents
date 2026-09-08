//! Global (`~/.latte/`) model configuration loader.
//!
//! Resolution layers for an `api_key` / `base_url` (and any other `ModelDef`
//! field), in increasing priority:
//!
//! 1. **Global** — `~/.latte/models.yaml`, `~/.latte/models.toml`, or every
//!    `*.yaml`/`*.toml` under `~/.latte/models.d/`. Optional.
//! 2. **Project** — the model catalog passed to the CLI
//!    (typically .latte/models.toml + .latte/agents/).
//! 3. **CLI overrides** — `--api-key <key>`, `--model-override k=v`.
//!
//! Within each layer a model is identified by its `id`; later sources replace
//! earlier ones for matching fields, and models present in the later layer
//! but missing from the earlier one are appended.
//!
//! Two YAML/TOML schemas are accepted in the **router-style** form
//! (used by `latte-rs-model-router/models.toml` and the existing
//! `~/.latte/models.yaml`):
//!
//! ```yaml
//! models:
//!   - id: deepseek-v4-flash
//!     name: DeepSeek-v4-flash
//!     api: openai
//!     ...
//! ```
//!
//! and the **project-style** form (used by `config/models.toml`):
//!
//! ```toml
//! [models]
//! tiers = { standard = "claude-sonnet-4-20250514" }
//!
//! [[models.models]]
//! id = "claude-sonnet-4-20250514"
//! ...
//! ```
//!
//! Both are normalised into [`GlobalConfig`] on load.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{AgentConfig, ModelCatalog, ModelDef};
use crate::error::{AgentError, AgentResult};

/// Default directory holding global model configuration.
pub const GLOBAL_CONFIG_DIR: &str = ".latte";

/// Global model configuration. Equivalent in shape to the project-side
/// [`ModelCatalog`](crate::config::ModelCatalog), but with the additional
/// ability to be parsed from a **router-style** document (bare model list
/// without the `[models]` wrapper).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GlobalConfig {
    #[serde(default)]
    pub models: Vec<ModelDef>,
    #[serde(default)]
    pub tiers: Option<HashMap<String, String>>,
    #[serde(default)]
    pub role_tiers: Option<HashMap<String, HashMap<String, String>>>,
}

/// Router-style document: a bare list of `ModelDef`.
///
/// This is the schema used by `latte-rs-model-router/models.toml` and
/// matches the user's existing `~/.latte/models.yaml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RouterStyleDoc {
    #[serde(default)]
    models: Vec<ModelDef>,
    #[serde(default)]
    tiers: Option<HashMap<String, String>>,
    #[serde(default)]
    role_tiers: Option<HashMap<String, HashMap<String, String>>>,
}

impl GlobalConfig {
    /// Locate the global config directory (`$LATTE_HOME`, falling back to
    /// `$HOME/.latte/`).
    pub fn global_dir() -> Option<PathBuf> {
        if let Ok(p) = std::env::var("LATTE_HOME") {
            if !p.is_empty() {
                return Some(PathBuf::from(p));
            }
        }
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(GLOBAL_CONFIG_DIR))
    }

    /// Default file paths the loader tries, in order. The first one that
    /// exists wins; missing files are silently skipped.
    pub fn default_candidates() -> Vec<PathBuf> {
        let Some(dir) = Self::global_dir() else {
            return Vec::new();
        };
        vec![
            dir.join("models.yaml"),
            dir.join("models.yml"),
            dir.join("models.toml"),
        ]
    }

    /// Load the global config from the default search path.
    ///
    /// Returns `Ok(default)` (empty) when nothing exists — global config is
    /// optional. Errors only when a file exists but fails to parse.
    pub fn load_default() -> AgentResult<Self> {
        let mut merged = Self::default();
        let Some(dir) = Self::global_dir() else {
            return Ok(merged);
        };

        // 1) Try the canonical single-file candidates first.
        for candidate in Self::default_candidates() {
            if candidate.exists() {
                let part = Self::load_file(&candidate)?;
                merged.merge_from(&part);
            }
        }

        // 2) Then layer in everything under ~/.latte/models.d/ (allows users
        //    to split models across files, e.g. one per vendor).
        let models_d = dir.join("models.d");
        if models_d.is_dir() {
            let mut entries: Vec<PathBuf> = std::fs::read_dir(&models_d)
                .map_err(|e| {
                    AgentError::Config(format!(
                        "cannot read '{}': {}",
                        models_d.display(),
                        e
                    ))
                })?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.is_file()
                        && matches!(
                            p.extension().and_then(|s| s.to_str()),
                            Some("yaml") | Some("yml") | Some("toml")
                        )
                })
                .collect();
            entries.sort();
            for entry in &entries {
                let part = Self::load_file(entry)?;
                merged.merge_from(&part);
            }
        }

        Ok(merged)
    }

    /// Load a single config file. Auto-detects YAML vs TOML by extension and
    /// accepts both the project-style and router-style schemas for YAML/TOML.
    pub fn load_file(path: &Path) -> AgentResult<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            AgentError::Config(format!(
                "cannot read global config '{}': {}",
                path.display(),
                e
            ))
        })?;

        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("");

        match ext {
            "yaml" | "yml" => Self::parse_yaml(&content),
            "toml" => Self::parse_toml(&content),
            other => Err(AgentError::Config(format!(
                "unsupported global config extension '.{}' (use .yaml, .yml, or .toml)",
                other
            ))),
        }
    }

    /// Try both router-style and project-style YAML parsers; the first one
    /// that successfully produces a non-empty result wins. If both succeed
    /// but the router-style returns nothing, fall through to project-style.
    fn parse_yaml(content: &str) -> AgentResult<Self> {
        // Router-style is unambiguous (`models: - id: ...`). Try it first.
        if let Ok(doc) = serde_yaml::from_str::<RouterStyleDoc>(content) {
            if !doc.models.is_empty() || doc.tiers.is_some() || doc.role_tiers.is_some() {
                return Ok(Self {
                    models: doc.models,
                    tiers: doc.tiers,
                    role_tiers: doc.role_tiers,
                });
            }
        }
        // Otherwise treat as project-style (the user can also put
        // `[[models.models]]` in YAML if they want the nested form).
        let cfg: AgentConfig = serde_yaml::from_str(content).map_err(|e| {
            AgentError::Config(format!(
                "invalid YAML in global config (neither router-style nor \
                 project-style): {}",
                e
            ))
        })?;
        Ok(Self {
            models: cfg.models.models,
            tiers: cfg.models.tiers,
            role_tiers: cfg.models.role_tiers,
        })
    }

    fn parse_toml(content: &str) -> AgentResult<Self> {
        // Same dual-schema strategy as YAML.
        if let Ok(doc) = toml::from_str::<RouterStyleDoc>(content) {
            if !doc.models.is_empty() || doc.tiers.is_some() || doc.role_tiers.is_some() {
                return Ok(Self {
                    models: doc.models,
                    tiers: doc.tiers,
                    role_tiers: doc.role_tiers,
                });
            }
        }
        let cfg: AgentConfig = toml::from_str(content).map_err(|e| {
            AgentError::Config(format!(
                "invalid TOML in global config (neither router-style nor \
                 project-style): {}",
                e
            ))
        })?;
        Ok(Self {
            models: cfg.models.models,
            tiers: cfg.models.tiers,
            role_tiers: cfg.models.role_tiers,
        })
    }

    /// Apply a partial global config onto this one. Later wins per `id`
    /// (models) and per key (tier maps).
    pub fn merge_from(&mut self, other: &Self) {
        // Accumulate models across files: same id → fill unset fields,
        // new id → append. Then layer tier maps on top.
        merge_models(&mut self.models, &other.models);
        append_models(&mut self.models, &other.models);
        merge_tier_map(self.tiers.get_or_insert_with(Default::default), &other.tiers);
    }

    /// Merge this global config into a project `AgentConfig`, then return
    /// the result. **Project wins** per model id and per tier key; the
    /// global config is used to **fill empty fields** the project left
    /// blank (typically `api_key` set to `${ENV_VAR}` with the env unset).
    pub fn merge_into_project(&self, project: &AgentConfig) -> AgentConfig {
        let mut models = project.models.models.clone();
        merge_models(&mut models, &self.models);
        // Append global-only models (e.g. `deepseek-v4-flash`) so the
        // resolver can find them when walking the role's fallback
        // chain. The caller (`config_layer::load`) computes the same
        // set *before* calling this so it can inject the ids into
        // every role's `model_chain` — keeping the catalog and the
        // chain in sync.
        append_models(&mut models, &self.models);
        let mut tiers: HashMap<String, String> = self.tiers.clone().unwrap_or_default();
        if let Some(p) = &project.models.tiers {
            for (k, v) in p {
                tiers.insert(k.clone(), v.clone());
            }
        }
        let tiers_opt = if tiers.is_empty() { None } else { Some(tiers) };

        let mut role_tiers: HashMap<String, HashMap<String, String>> =
            self.role_tiers.clone().unwrap_or_default();
        if let Some(p) = &project.models.role_tiers {
            for (role, tier_map) in p {
                let entry = role_tiers
                    .entry(role.clone())
                    .or_insert_with(Default::default);
                for (k, v) in tier_map {
                    entry.insert(k.clone(), v.clone());
                }
            }
        }
        let role_tiers_opt = if role_tiers.is_empty() {
            None
        } else {
            Some(role_tiers)
        };

        let mut out = project.clone();
        out.models = ModelCatalog {
            models,
            tiers: tiers_opt,
            role_tiers: role_tiers_opt,
        };
        out
    }

    /// Apply one or more `id.field=value` overrides onto a model catalog.
    /// The override keys mirror the [`ModelDef`] field names
    /// (`api_key`, `base_url`, `name`, `tier`, `max_tokens`, ...).
    pub fn apply_field_overrides(
        catalog: &mut ModelCatalog,
        overrides: &[(String, String, String)], // (model_id, field, value)
    ) -> AgentResult<()> {
        for (model_id, field, value) in overrides {
            let model = catalog
                .models
                .iter_mut()
                .find(|m| &m.name == model_id)
                .ok_or_else(|| {
                    AgentError::Config(format!(
                        "CLI override targets unknown model '{}'",
                        model_id
                    ))
                })?;
            apply_field_override(model, field, value)?;
        }
        Ok(())
    }
}

fn apply_field_override(model: &mut ModelDef, field: &str, value: &str) -> AgentResult<()> {
    match field {
        "api_key" => model.api_key = value.to_string(),
        "base_url" => model.base_url = value.to_string(),
        "name" => model.name = value.to_string(),
        "api" => model.api = value.to_string(),
        "provider" => model.provider = value.to_string(),
        "tier" => model.tier = Some(value.to_string()),
        "max_tokens" => {
            model.max_tokens = value.parse().map_err(|e| {
                AgentError::Config(format!("invalid max_tokens '{}': {}", value, e))
            })?
        }
        "context_window" => {
            model.context_window = value.parse().map_err(|e| {
                AgentError::Config(format!("invalid context_window '{}': {}", value, e))
            })?
        }
        other => {
            return Err(AgentError::Config(format!(
                "unknown override field '{}' for model override",
                other
            )))
        }
    }
    Ok(())
}

fn merge_models(dst: &mut Vec<ModelDef>, src: &[ModelDef]) {
    // First pass: id-based merge. dst wins on identity, src fills only
    // fields dst left unset (empty or `${VAR}` placeholder). New src
    // models (no matching id) are NOT appended: this is the conservative
    // "no surprise new models" semantic needed by `merge_into_project`.
    // Callers that want to accumulate (e.g. `merge_from` reading
    // multiple global config files) use [`append_models`] instead.
    for new_model in src {
        if let Some(existing) = dst.iter_mut().find(|m| m.provider == new_model.provider && m.name == new_model.name) {
            if is_unset(&existing.api_key) && !is_unset(&new_model.api_key) {
                existing.api_key = new_model.api_key.clone();
            }
            if is_unset(&existing.base_url) && !is_unset(&new_model.base_url) {
                existing.base_url = new_model.base_url.clone();
            }
        }
    }

    // Second pass: heuristic — for any dst model whose api_key is still
    // unset, look for a src model with the same `provider` AND
    // `base_url` and copy the key. This handles the common case where a
    // project declares a model under one name (e.g. `deepseek-chat`)
    // and the global config has a differently-named entry for the same
    // vendor (e.g. `deepseek-v4-flash`). It is best-effort and never
    // overrides a real dst value.
    for existing in dst.iter_mut() {
        if !is_unset(&existing.api_key) {
            continue;
        }
        let donors: Vec<_> = src
            .iter()
            .filter(|m| m.provider == existing.provider && m.base_url == existing.base_url)
            .collect();
        for d in donors {
            if !is_unset(&d.api_key) {
                existing.api_key = d.api_key.clone();
            }
        }
    }
}

/// Append every model in `src` whose `id` is not already in `dst`. Used
/// by `merge_from` to accumulate models across multiple global config
/// files (single-file + `models.d/*.yaml`) without losing any.
fn append_models(dst: &mut Vec<ModelDef>, src: &[ModelDef]) {
    for new_model in src {
        if !dst.iter().any(|m| m.provider == new_model.provider && m.name == new_model.name) {
            dst.push(new_model.clone());
        }
    }
}
/// True when a field is effectively "not configured": empty,
/// whitespace-only, or still a literal `${ENV_VAR}` placeholder whose
/// variable may or may not resolve at runtime. The placeholder check
/// means a global config can fill the gap when the env var is missing.
fn is_unset(s: &str) -> bool {
    let t = s.trim();
    t.is_empty() || t.starts_with("${")
}

fn merge_tier_map(dst: &mut HashMap<String, String>, src: &Option<HashMap<String, String>>) {
    let Some(src) = src else { return };
    for (k, v) in src {
        dst.insert(k.clone(), v.clone());
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(name: &str, content: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "latte_global_config_{}_{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_file(&p);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        p
    }

    #[test]
    fn parses_router_style_yaml() {
        let path = write_tmp(
            "router.yaml",
            r#"
models:
  - name: deepseek-v4-flash
    api: openai
    provider: deepseek
    base_url: https://api.deepseek.com
    api_key: sk-direct
    context_window: 1000000
    max_tokens: 384000
    tier: budget
"#,
        );
        let cfg = GlobalConfig::load_file(&path).unwrap();
        assert_eq!(cfg.models.len(), 1);
        assert_eq!(cfg.models[0].name, "deepseek-v4-flash");
        assert_eq!(cfg.models[0].api_key, "sk-direct");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn parses_router_style_toml() {
        let path = write_tmp(
            "router.toml",
            r#"
[[models]]
name = "m1"
api = "openai"
provider = "p"
base_url = "http://x"
api_key = "k"
context_window = 1000
max_tokens = 256
"#,
        );
        let cfg = GlobalConfig::load_file(&path).unwrap();
        assert_eq!(cfg.models.len(), 1);
        assert_eq!(cfg.models[0].name, "m1");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn parses_project_style_toml() {
        let path = write_tmp(
            "proj.toml",
            r#"
[models]
tiers = { standard = "claude-sonnet" }

[[models.models]]
name = "claude-sonnet"
api = "anthropic"
provider = "anthropic"
base_url = "https://api.anthropic.com"
api_key = "k"
context_window = 200000
max_tokens = 8192
"#,
        );
        let cfg = GlobalConfig::load_file(&path).unwrap();
        assert_eq!(cfg.models.len(), 1);
        let tiers = cfg.tiers.expect("tiers present");
        assert_eq!(tiers.get("standard").map(String::as_str), Some("claude-sonnet"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn parses_project_style_yaml() {
        let path = write_tmp(
            "proj.yaml",
            r#"
models:
  models:
    - name: y1
      api: openai
      provider: p
      base_url: http://x
      api_key: k
      context_window: 1000
      max_tokens: 256
  tiers:
    budget: y1
"#,
        );
        let cfg = GlobalConfig::load_file(&path).unwrap();
        assert_eq!(cfg.models.len(), 1);
        assert_eq!(cfg.models[0].name, "y1");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn merge_into_project_fills_missing_api_key() {
        // Project model declares only a ${ENV} placeholder that resolves to
        // empty in the test env; the global has a real key. After merge the
        // project model gets the real key.
        let project = AgentConfig {
            advisor: Default::default(),
            models: ModelCatalog {
                models: vec![ModelDef {
                    name: "shared".into(),
                    api: "openai".into(),
                    provider: "p".into(),
                    base_url: "http://x".into(),
                    api_key: "${DEFINITELY_UNSET_OVERRIDE_TEST}".into(),
                    context_window: 1000,
                    max_tokens: 256,
                    supports_thinking: false,
                    supports_vision: false,
                    omit_max_tokens: false,
                    max_tokens_field: Default::default(),
                    supports_image_generation: false,
                    cost_per_million_input: None,
                    cost_per_million_output: None,
                    tier: Some("budget".into()),
                    timeout_secs: None,
                }],
                tiers: None,
                role_tiers: None,
            },
            roles: Default::default(),
        };
        let global = GlobalConfig {
            models: vec![ModelDef {
                name: "shared".into(),
                api: "openai".into(),
                provider: "p".into(),
                base_url: "http://x".into(),
                api_key: "real-key-from-global".into(),
                context_window: 1000,
                max_tokens: 256,
                supports_thinking: false,
                supports_vision: false,
                omit_max_tokens: false,
                max_tokens_field: Default::default(),
                supports_image_generation: false,
                cost_per_million_input: None,
                cost_per_million_output: None,
                tier: Some("budget".into()),
                timeout_secs: None,
            }],
            tiers: None,
            role_tiers: None,
        };
        std::env::remove_var("DEFINITELY_UNSET_OVERRIDE_TEST");
        let merged = global.merge_into_project(&project);
        let m = &merged.models.models[0];
        assert_eq!(m.api_key, "real-key-from-global");
    }

    #[test]
    fn merge_into_project_keeps_existing_api_key() {
        // Project already has a real key — global must not clobber it.
        let project = AgentConfig {
            advisor: Default::default(),
            models: ModelCatalog {
                models: vec![ModelDef {
                    name: "shared".into(),
                    api: "openai".into(),
                    provider: "p".into(),
                    base_url: "http://x".into(),
                    api_key: "project-key".into(),
                    context_window: 1000,
                    max_tokens: 256,
                    supports_thinking: false,
                    supports_vision: false,
                    omit_max_tokens: false,
                    max_tokens_field: Default::default(),
                    supports_image_generation: false,
                    cost_per_million_input: None,
                    cost_per_million_output: None,
                    tier: Some("budget".into()),
                    timeout_secs: None,
                }],
                tiers: None,
                role_tiers: None,
            },
            roles: Default::default(),
        };
        let global = GlobalConfig {
            models: vec![ModelDef {
                name: "shared".into(),
                api: "openai".into(),
                provider: "p".into(),
                base_url: "http://x".into(),
                api_key: "global-key".into(),
                context_window: 1000,
                max_tokens: 256,
                supports_thinking: false,
                supports_vision: false,
                omit_max_tokens: false,
                max_tokens_field: Default::default(),
                supports_image_generation: false,
                cost_per_million_input: None,
                cost_per_million_output: None,
                tier: Some("budget".into()),
                timeout_secs: None,
            }],
            tiers: None,
            role_tiers: None,
        };
        let merged = global.merge_into_project(&project);
        assert_eq!(merged.models.models[0].api_key, "project-key");
    }

    #[test]
    fn merge_into_project_heuristic_same_provider_fills_unset_key() {
        // Project model has a `${VAR}` placeholder; global has a
        // differently-named model with the same provider + base_url.
        // The heuristic pass should copy the key over.
        std::env::remove_var("DEFINITELY_UNSET_OVERRIDE_TEST");
        let project = AgentConfig {
            advisor: Default::default(),
            models: ModelCatalog {
                models: vec![ModelDef {
                    name: "deepseek-chat".into(),
                    api: "openai".into(),
                    provider: "deepseek".into(),
                    base_url: "https://api.deepseek.com".into(),
                    api_key: "${DEFINITELY_UNSET_OVERRIDE_TEST}".into(),
                    context_window: 65536,
                    max_tokens: 8192,
                    supports_thinking: false,
                    supports_vision: false,
                    omit_max_tokens: false,
                    max_tokens_field: Default::default(),
                    supports_image_generation: false,
                    cost_per_million_input: None,
                    cost_per_million_output: None,
                    tier: Some("budget".into()),
                    timeout_secs: None,
                }],
                tiers: None,
                role_tiers: None,
            },
            roles: Default::default(),
        };
        let global = GlobalConfig {
            models: vec![ModelDef {
                name: "deepseek-v4-flash".into(),
                api: "openai".into(),
                provider: "deepseek".into(),
                base_url: "https://api.deepseek.com".into(),
                api_key: "sk-from-global".into(),
                context_window: 1000000,
                max_tokens: 384000,
                supports_thinking: false,
                supports_vision: false,
                omit_max_tokens: false,
                max_tokens_field: Default::default(),
                supports_image_generation: false,
                cost_per_million_input: None,
                cost_per_million_output: None,
                tier: Some("budget".into()),
                timeout_secs: None,
            }],
            tiers: None,
            role_tiers: None,
        };
        let merged = global.merge_into_project(&project);
        assert_eq!(merged.models.models[0].api_key, "sk-from-global");
    }

    #[test]
    fn field_override_changes_api_key() {
        let mut catalog = ModelCatalog {
            models: vec![ModelDef {
                name: "m".into(),
                api: "openai".into(),
                provider: "p".into(),
                base_url: "http://x".into(),
                api_key: "old".into(),
                context_window: 1000,
                max_tokens: 256,
                supports_thinking: false,
                supports_vision: false,
                omit_max_tokens: false,
                max_tokens_field: Default::default(),
                supports_image_generation: false,
                cost_per_million_input: None,
                cost_per_million_output: None,
                tier: None,
                timeout_secs: None,
            }],
            tiers: None,
            role_tiers: None,
        };
        GlobalConfig::apply_field_overrides(
            &mut catalog,
            &[("m".into(), "api_key".into(), "new".into())],
        )
        .unwrap();
        assert_eq!(catalog.models[0].api_key, "new");
    }

    #[test]
    fn field_override_unknown_model_errors() {
        let mut catalog = ModelCatalog::default();
        let err = GlobalConfig::apply_field_overrides(
            &mut catalog,
            &[("nope".into(), "api_key".into(), "k".into())],
        )
        .unwrap_err();
        assert!(format!("{err}").contains("unknown model 'nope'"));
    }

    #[test]
    fn field_override_unknown_field_errors() {
        let mut catalog = ModelCatalog {
            models: vec![ModelDef {
                name: "m".into(),
                api: "openai".into(),
                provider: "p".into(),
                base_url: "http://x".into(),
                api_key: "k".into(),
                context_window: 1000,
                max_tokens: 256,
                supports_thinking: false,
                supports_vision: false,
                omit_max_tokens: false,
                max_tokens_field: Default::default(),
                supports_image_generation: false,
                cost_per_million_input: None,
                cost_per_million_output: None,
                tier: None,
                timeout_secs: None,
            }],
            tiers: None,
            role_tiers: None,
        };
        let err = GlobalConfig::apply_field_overrides(
            &mut catalog,
            &[("m".into(), "frobnicate".into(), "v".into())],
        )
        .unwrap_err();
        assert!(format!("{err}").contains("unknown override field 'frobnicate'"));
    }
}

