//! Three-layer config loader used by every CLI subcommand.
//!
//! Precedence (highest → lowest):
//!
//! 1. CLI overrides (`--api-key`, `--model-override id.field=value`).
//! 2. Project config (project `.latte/agents.d/` + `.latte/models.d/`).
//! 3. Global config (`~/.latte/models.{yaml,toml}` or `~/.latte/models.d/*`).
//!
//! Within a layer, identical `model.id` entries merge by **filling empty
//! fields** (so a project model with a real `api_key` is not clobbered by
//! a global entry that only fills `base_url`).
//!
//! Returns an `AgentConfig` whose `models` field already reflects all three
//! layers, plus a [`Resolved`] view exposing the merged `ModelResolver` and
//! the locations that actually contributed data — useful for `config show`.

use latte_agent_core::config::AgentConfig;
use latte_agent_core::error::AgentResult;
use latte_agent_core::global_config::GlobalConfig;
use latte_agent_core::model_resolver::ModelResolver;

/// Sources of `api_key` etc. that the operator can use.
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    /// `--api-key <KEY>` — sets the api_key of every model that currently
    /// has an empty `api_key`. If `--model` is also given, restricts the
    /// fill to that single model id; otherwise all models are touched.
    pub api_key: Option<String>,
    /// `--model <id>` — selects which model the `--api-key` flag targets.
    /// Without `--model`, `--api-key` fills every blank model.
    pub api_key_target: Option<String>,
    /// One or more `--model-override id.field=value` (repeatable). Higher
    /// precision than `--api-key`: can set `base_url`, `tier`, `max_tokens`,
    /// etc. on a specific model.
    pub field_overrides: Vec<(String, String, String)>,
}

impl CliOverrides {
    /// Turn the structured `api_key` + `api_key_target` into a flat list of
    /// `(id, field, value)` overrides compatible with
    /// [`GlobalConfig::apply_field_overrides`].
    ///
    /// Because the list of model ids isn't known until the configs are
    /// loaded, the caller must resolve the target list and pass it in.
    pub fn into_field_overrides(
        self,
        all_model_ids: &[String],
    ) -> Vec<(String, String, String)> {
        let mut out = self.field_overrides;
        if let Some(key) = self.api_key {
            let targets: Vec<&str> = match &self.api_key_target {
                Some(id) => vec![id.as_str()],
                None => all_model_ids.iter().map(String::as_str).collect(),
            };
            for id in targets {
                out.push((id.to_string(), "api_key".to_string(), key.clone()));
            }
        }
        out
    }
}

/// Locations that contributed to the merged config (for `config show`
/// and for the chat log's "config loaded" block).
#[derive(Debug, Clone, Default)]
pub struct ResolvedSources {
    pub global: Vec<String>,
    pub project_models: Option<String>,
    pub project_agents: Option<String>,
    pub cli_overrides: Vec<String>,
    /// Reverse map: `role_id -> [file, ...]`. Every file that declared
    /// the role under `[roles.<id>]` is recorded in load order, so the
    /// operator can see where a given role came from — useful when a
    /// project overrides a global role.
    pub role_files: std::collections::BTreeMap<String, Vec<String>>,
    /// Reverse map: `model_id -> [file, ...]`. Same semantics as
    /// `role_files` but for the model catalog (`[models]`,
    /// `[[models.models]]`, or router-style top-level `models:` list).
    pub model_files: std::collections::BTreeMap<String, Vec<String>>,
}

/// Final, fully-merged configuration along with the resolver and the
/// sources that contributed to it.
pub struct Resolved {
    pub config: AgentConfig,
    pub resolver: ModelResolver,
    pub sources: ResolvedSources,
    pub discussion: DiscussionConfig,
}

/// Load the merged config and build a `ModelResolver` from it.
///
/// `project_agents` and `project_models` are paths (file or directory) as
/// accepted by `AgentConfig::load`. Pass `None` to skip the project
/// layer entirely (the global layer `~/.latte/agents.d/` is still tried).
/// A missing project path is silently skipped via
/// [`AgentConfig::load_with_global`].
///
/// The CLI overrides are applied last and on top of everything.

/// 项目讨论配置，从 `.latte/discussion.toml` 自动加载。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct DiscussionConfig {
    pub default_workflow: Option<WorkflowDef>,
    #[serde(default)]
    pub default_roles: Vec<String>,
    #[serde(default)]
    pub steps: Vec<StepDef>,
    #[serde(default)]
    pub role_hierarchy: std::collections::HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct WorkflowDef {
    pub name: String,
    pub description: Option<String>,
    pub max_rounds: Option<u32>,
    pub context_token_budget: Option<u32>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct StepDef {
    pub id: String,
    pub speakers: Vec<String>,
    pub prompt: String,
}

/// 自动加载项目目录下的 `.latte/discussion.toml`。
pub fn load_discussion_config(project_dir: &std::path::Path) -> DiscussionConfig {
    let path = project_dir.join(".latte/discussion.toml");
    if path.exists() {
        match std::fs::read_to_string(&path) {
            Ok(content) => match toml::from_str(&content) {
                Ok(cfg) => {
                    eprintln!("[config] loaded discussion config from {}", path.display());
                    return cfg;
                }
                Err(e) => eprintln!("[config] failed to parse {}: {e}", path.display()),
            },
            Err(e) => eprintln!("[config] failed to read {}: {e}", path.display()),
        }
    }
    DiscussionConfig::default()
}

 pub fn load(
     project_agents: Option<&str>,
     project_models: Option<&str>,
     cli: CliOverrides,
 ) -> AgentResult<Resolved> {
     // Layer 3 (lowest): global ~/.latte/models.{yaml,toml} + ~/.latte/models.d/
     let global = GlobalConfig::load_default()?;
     let mut sources = ResolvedSources {
         global: GlobalConfig::default_candidates()
             .into_iter()
             .filter(|p| p.exists())
             .map(|p| p.display().to_string())
             .collect(),
         ..Default::default()
     };
     if let Some(dir) = GlobalConfig::global_dir() {
         let d = dir.join("models.d");
         if d.is_dir() {
             if let Ok(read) = std::fs::read_dir(&d) {
                 for entry in read.flatten() {
                     let p = entry.path();
                     if p.is_file() {
                         sources.global.push(p.display().to_string());
                     }
                 }
             }
         }
     }
    // Per-file reverse map: every agent file under project/global
    // agents.d → list of role ids declared. Populated by
    // `scan_role_files_in_dir` below. Project wins over global, but
    // both are recorded so the operator can see "this role was
    // overridden in the project" at a glance.
    if let Some(agents) = project_agents {
        if std::fs::metadata(agents).is_ok() {
            scan_role_files_in_dir(std::path::Path::new(agents), &mut sources.role_files);
        }
    }
    if let Some(dir) = GlobalConfig::global_dir() {
        let d = dir.join("agents.d");
        if d.is_dir() {
            scan_role_files_in_dir(&d, &mut sources.role_files);
        }
    }
     // Layer 2: project (agents + models) merged with the global
     // `~/.latte/agents.d/` directory via `load_with_global` (project-wins
     // per role / model id; global fills in anything the project did not
     // declare). A missing project path is silently skipped so a bare
     // `~/.latte/agents.d` setup can drive the system end-to-end.
     //
     // Check if the project agents path exists first; if not, pass None
     // so load_with_global cleanly falls back to global + built-in roles
     // without producing a misleading error in the log.
     let agents_path_exists = project_agents
         .map(|p| std::fs::metadata(p).is_ok())
         .unwrap_or(false);
     let active_agents = if agents_path_exists { project_agents } else { None };
     let mut project_cfg = AgentConfig::load_with_global(active_agents)?;
     if let Some(path) = project_agents {
         if agents_path_exists {
             sources.project_agents = Some(path.to_string());
         }
     }
     // Models: project wins, but the global `models.{yaml,toml}` /
     // `models.d/*.yaml` is still merged on top below. We do NOT call
     // `load_with_global` for models here because the global *models*
     // layer is merged in a separate step (`global.merge_into_project`)
     // that already implements id-based field-filling semantics.
     if let Some(path) = project_models {
         if std::fs::metadata(path).is_ok() {
             let part = AgentConfig::load(path)?;
             merge_into(&mut project_cfg, &part);
             sources.project_models = Some(path.to_string());
            // Track per-model origin for the model file the user
            // pointed at. When the path is a directory (default
            // `.latte/models.d`), we walk the dir below; when it's
            // a single file, we just scan that file.
            let p = std::path::Path::new(path);
            if p.is_dir() {
                scan_model_files_in_dir(p, &mut sources.model_files);
            } else if p.is_file() {
                scan_model_file(p, &mut sources.model_files);
            }
         }
     }
    // Global models: same scan for the layered global locations
    // already enumerated into `sources.global`. We re-walk the
    // directory tree (cheap; just metadata) so the YAML files
    // contribute model ids too.
    for src in &sources.global {
        let p = std::path::Path::new(src);
        if p.is_file() {
            scan_model_file(p, &mut sources.model_files);
        }
    }

    // Compute the set of model ids that the global config contributed
    // exclusively. These are appended to every role's `model_chain`
    // so that a role whose primary model has no configured credentials
    // falls through to a model the operator actually configured in
    // `~/.latte/models.yaml`. The same ids are also appended to the
    // merged catalog below so the resolver can find them.
    let project_ids: std::collections::HashSet<&str> = project_cfg
        .models
        .models
        .iter()
        .map(|m| m.name.as_str())
        .collect();
    let extra_ids: Vec<String> = global
        .models
        .iter()
        .map(|m| m.name.as_str())
        .filter(|id| !project_ids.contains(id))
        .map(str::to_string)
        .collect();

    // Merge global UNDER project (project wins per id); the same
    // global-only ids are also appended to the merged catalog so the
    // resolver can find them when walking the chain.
    let mut merged = global.merge_into_project(&project_cfg);
    if !extra_ids.is_empty() {
        for template in merged.roles.values_mut() {
            for id in &extra_ids {
                if !template.model_chain.iter().any(|c| c == id) {
                    template.model_chain.push(id.clone());
                }
            }
        }
    }

    let resolver = ModelResolver::from_config(&merged)?;

    // Load project discussion config
    let discussion = load_discussion_config(project_agents.map(|p| std::path::Path::new(p).parent().unwrap_or(std::path::Path::new("."))).unwrap_or(std::path::Path::new(".")));

    Ok(Resolved {
        config: merged,
        resolver,
        sources,
        discussion,
    })
}

/// Append `src`'s models, roles, and tier maps into `dst`.
/// ids are the caller's problem (checked before this is called).
fn merge_into(dst: &mut AgentConfig, src: &AgentConfig) {
    dst.roles.extend(src.roles.iter().map(|(k, v)| (k.clone(), v.clone())));
    dst.models.models.extend(src.models.models.iter().cloned());
    if let Some(t) = &src.models.tiers {
        dst.models
            .tiers
            .get_or_insert_with(Default::default)
            .extend(t.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    if let Some(rt) = &src.models.role_tiers {
        let dst_rt = dst
            .models
            .role_tiers
            .get_or_insert_with(Default::default);
        for (role, tier_map) in rt {
            dst_rt
                .entry(role.clone())
                .or_insert_with(Default::default)
                .extend(tier_map.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
    }
}


/// Walk an agents directory and record every role id found in each
/// `.toml` file into the `role_files` reverse map. Files are scanned
/// in sorted order so the recorded list is deterministic across
/// runs; project entries are scanned before global ones by the
/// caller, so the **first** entry per role id is the effective
/// source (later entries still recorded for audit, not stripped).
///
/// Note: only TOML is scanned. The chat log already lists every
/// contributing file under `sources.global` / `sources.project_agents`,
/// so a missing per-file reverse map for an unrecognised format
/// (e.g. a future YAML agent file) is still visible — the operator
/// can correlate by hand.
fn scan_role_files_in_dir(
    dir: &std::path::Path,
    role_files: &mut std::collections::BTreeMap<String, Vec<String>>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension().and_then(|s| s.to_str()) == Some("toml")
        })
        .collect();
    paths.sort();
    for path in paths {
        scan_role_file(&path, role_files);
    }
}

fn scan_role_file(
    path: &std::path::Path,
    role_files: &mut std::collections::BTreeMap<String, Vec<String>>,
) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(value) = content.parse::<toml::Value>() else {
        return;
    };
    let Some(roles) = value.get("roles").and_then(|v| v.as_table()) else {
        return;
    };
    let path_str = path.display().to_string();
    for (id, _) in roles {
        role_files.entry(id.clone()).or_default().push(path_str.clone());
    }
}

/// Walk a models directory and record every model id found in each
/// TOML / YAML file into the `model_files` reverse map. YAML files
/// in the global layer (`~/.latte/models.d/*.yaml`) are listed in
/// `sources.global` but the per-model id extraction is skipped
/// there (YAML parsing lives in `latte-agent-core`, and duplicating
/// `serde_yaml` in the CLI just for log cosmetics is not worth it).
fn scan_model_files_in_dir(
    dir: &std::path::Path,
    model_files: &mut std::collections::BTreeMap<String, Vec<String>>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && matches!(
                    p.extension().and_then(|s| s.to_str()),
                    Some("toml") | Some("yaml") | Some("yml")
                )
        })
        .collect();
    paths.sort();
    for path in paths {
        scan_model_file(&path, model_files);
    }
}

/// Scan a single model file. TOML files: extract `[[models.models]]`
/// array entries (project-style) AND top-level `models` array
/// (router-style). YAML/other: no-op (YAML scan skipped, see
/// `scan_model_files_in_dir`).
fn scan_model_file(
    path: &std::path::Path,
    model_files: &mut std::collections::BTreeMap<String, Vec<String>>,
) {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if ext != "toml" {
        return;
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(value) = content.parse::<toml::Value>() else {
        return;
    };
    let path_str = path.display().to_string();

    // Project-style: `[[models.models]]` array.
    if let Some(models) = value
        .get("models")
        .and_then(|m| m.get("models"))
        .and_then(|v| v.as_array())
    {
        for entry in models {
            if let Some(id) = model_entry_key(entry) {
                model_files
                    .entry(id)
                    .or_default()
                    .push(path_str.clone());
            }
        }
    }

    // Router-style: top-level `[[models]]` array (also accepted by
    // GlobalConfig::parse_toml for the global layer).
    if let Some(models) = value.get("models").and_then(|v| v.as_array()) {
        for entry in models {
            if let Some(id) = model_entry_key(entry) {
                model_files
                    .entry(id)
                    .or_default()
                    .push(path_str.clone());
            }
        }
    }
}

/// 返回模型来源索引使用的规范键。
///
/// 新配置以 `provider/model_name` 作为模型唯一标识；保留 `id` 回退以便
/// 日志扫描旧版项目配置时仍能显示来源，而不影响运行时模型解析。
fn model_entry_key(entry: &toml::Value) -> Option<String> {
    let table = entry.as_table()?;
    match (
        table.get("provider").and_then(toml::Value::as_str),
        table.get("model_name").and_then(toml::Value::as_str),
    ) {
        (Some(provider), Some(model_name))
            if !provider.is_empty() && !model_name.is_empty() =>
        {
            Some(format!("{provider}/{model_name}"))
        }
        _ => table
            .get("id")
            .and_then(toml::Value::as_str)
            .map(str::to_owned),
    }
}
/// Redact a value that looks like a secret: keep the first 4 and last 2
/// characters, replace the middle with `***`. Short values become `***`.
fn redact(s: &str) -> String {
    if s.len() <= 8 {
        return "***".to_string();
    }
    let head = &s[..4];
    let tail = &s[s.len() - 2..];
    format!("{head}***{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_router_models_by_composite_key() {
        // 验证全局层 [[models]] 使用 model_name 时能登记 provider/model_name 来源。
        let path = std::env::temp_dir().join(format!(
            "latte_config_layer_model_{}.toml",
            std::process::id()
        ));
        std::fs::write(
            &path,
            r#"
[[models]]
name = "MiniMax-M3"
model_name = "MiniMax-M3"
api = "openai"
provider = "minimax"
base_url = "https://api.minimaxi.com/v1"
api_key = "test-key"
context_window = 1000000
max_tokens = 384000
"#,
        )
        .unwrap();

        let mut model_files = std::collections::BTreeMap::new();
        scan_model_file(&path, &mut model_files);

        assert_eq!(model_files.len(), 1);
        assert_eq!(model_files["minimax/MiniMax-M3"], vec![path.display().to_string()]);
        let _ = std::fs::remove_file(path);
    }
}
