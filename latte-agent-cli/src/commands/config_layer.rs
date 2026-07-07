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

/// Locations that contributed to the merged config (for `config show`).
#[derive(Debug, Clone, Default)]
pub struct ResolvedSources {
    pub global: Vec<String>,
    pub project_models: Option<String>,
    pub project_agents: Option<String>,
    pub cli_overrides: Vec<String>,
}

/// Final, fully-merged configuration along with the resolver and the
/// sources that contributed to it.
pub struct Resolved {
    pub config: AgentConfig,
    pub resolver: ModelResolver,
    pub sources: ResolvedSources,
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
        }
    }

    // Layer 1 (highest): CLI overrides.
    let all_ids: Vec<String> = project_cfg.models.models.iter().map(|m| m.id.clone()).collect();
    let flat_overrides = cli.into_field_overrides(&all_ids);
    if !flat_overrides.is_empty() {
        for (id, field, value) in &flat_overrides {
            sources
                .cli_overrides
                .push(format!("{id}.{field}={}", redact(value)));
        }
        GlobalConfig::apply_field_overrides(&mut project_cfg.models, &flat_overrides)?;
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
        .map(|m| m.id.as_str())
        .collect();
    let extra_ids: Vec<String> = global
        .models
        .iter()
        .map(|m| m.id.as_str())
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

    Ok(Resolved {
        config: merged,
        resolver,
        sources,
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
