//! Discussion workflow: defines the structure of a multi-agent discussion.
//!
//! Mirrors gsd-core's workflow .md files pattern: a workflow is a sequence of
//! steps, each specifying which agents speak and with what prompt.

use serde::{Deserialize, Serialize};

use crate::error::OrchResult;

/// A complete discussion workflow: a sequence of steps with hooks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscussionWorkflow {
    /// Workflow name (e.g. "code_review", "requirements_review").
    pub name: String,
    /// Human-readable description.
    #[serde(default)]
    pub description: String,
    /// Sequence of steps.
    pub steps: Vec<WorkflowStep>,
    /// Maximum discussion rounds.
    #[serde(default = "default_max_rounds")]
    pub max_rounds: usize,
    /// Token budget for shared context (0 = unlimited).
    #[serde(default)]
    pub context_token_budget: usize,
}

fn default_max_rounds() -> usize {
    3
}

impl Default for DiscussionWorkflow {
    fn default() -> Self {
        Self {
            name: "default".into(),
            description: String::new(),
            steps: Vec::new(),
            max_rounds: 3,
            context_token_budget: 0,
        }
    }
}

/// A single step within a discussion workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStep {
    /// Step identifier (e.g. "present", "review", "decide").
    pub id: String,
    /// Human-readable description.
    #[serde(default)]
    pub description: String,
    /// Agents that speak in this step, in order.
    pub speakers: Vec<String>,
    /// Prompt template for this step. Supports `{{variable}}` substitution.
    pub prompt: String,
    /// Hooks to inject before/after this step.
    #[serde(default)]
    pub hooks: Vec<StepHook>,
    /// If set, the combined output of this step is stored under this key
    /// for use in subsequent steps.
    #[serde(default)]
    pub output_key: Option<String>,
    /// Optional file contract for wave dependency analysis. `input`
    /// declares a file this step reads (making it dependent on the
    /// step that produces it); `output` declares a file this step
    /// produces (making subsequent steps dependent on it).
    #[serde(default)]
    pub contract: Option<FileContract>,
}

/// File-based contract between workflow steps for dependency tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileContract {
    /// File path this step consumes as input.
    #[serde(default)]
    pub input: Option<String>,
    /// File path this step produces as output.
    #[serde(default)]
    pub output: Option<String>,
}

/// A hook that runs before or after a workflow step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepHook {
    /// Hook kind.
    pub kind: HookKind,
    /// Agent source for content injection (only for Contribution).
    #[serde(default)]
    pub source: Option<String>,
    /// Template for the hook content.
    #[serde(default)]
    pub template: Option<String>,
    /// Message for gating hooks (shown when check fails).
    #[serde(default)]
    pub message: Option<String>,
}

/// Types of step hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookKind {
    /// Inject content before the step begins.
    Contribution,
    /// Blocking validation check (must pass to proceed).
    Gate,
}

impl DiscussionWorkflow {
    /// Load a workflow from a TOML string.
    pub fn parse(toml_str: &str) -> Result<Self, crate::error::OrchError> {
        toml::from_str(toml_str)
            .map_err(|e| crate::error::OrchError::Config(format!("invalid workflow TOML: {}", e)))
    }

    /// Validate the workflow: check that all referenced agents exist.
    pub fn validate(&self, available_agents: &[String]) -> Result<(), crate::error::OrchError> {
        for step in &self.steps {
            for speaker in &step.speakers {
                if !available_agents.contains(speaker) {
                    return Err(crate::error::OrchError::AgentNotFound(
                        speaker.clone(),
                        available_agents.to_vec(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Build the prompt for a step by substituting variables.
    pub fn build_step_prompt(
        &self,
        step: &WorkflowStep,
        vars: &std::collections::HashMap<String, String>,
    ) -> String {
        let mut prompt = step.prompt.clone();
        for (key, value) in vars {
            prompt = prompt.replace(&format!("{{{{{}}}}}", key), value);
        }
        prompt
    }
}

/// A collection of discussion workflows loaded from one source.
///
/// Source layout supported by [`WorkflowRegistry::load`]:
///   * **Single file** — legacy `discussion.toml` with
///     `[default_workflow]` and `[workflows.<id>]` sections.
///   * **Directory** — one `*.toml` file per workflow. Each file is a
///     single `DiscussionWorkflow`. The file's `name` field is the
///     canonical id; duplicate names are rejected.
#[derive(Debug, Clone, Default)]
pub struct WorkflowRegistry {
    /// Optional default workflow (used when caller asks for "default").
    pub default: Option<DiscussionWorkflow>,
    /// Named workflows keyed by their `name` field.
    pub workflows: std::collections::HashMap<String, DiscussionWorkflow>,
}

impl WorkflowRegistry {
    /// Load a registry from a path. Detects whether `path` is a file or
    /// directory and dispatches accordingly.
    pub fn load(path: &str) -> OrchResult<Self> {
        let meta = std::fs::metadata(path).map_err(|e| {
            crate::error::OrchError::Config(format!(
                "cannot stat workflow path '{}': {}",
                path, e
            ))
        })?;

        if meta.is_dir() {
            Self::load_dir(path)
        } else {
            Self::load_file(path)
        }
    }
    /// Load project + global workflow registries and merge them.
    ///
    /// Both layers use the same layout via [`ConfigLayer`]:
    /// - Project: `ConfigLayer::Project.workflows_dir()` (`./.latte/workflows/`)
    /// - Global:  `ConfigLayer::Global.workflows_dir()` (`$LATTE_HOME/workflows.d/` or `~/.latte/workflows.d/`)
    ///
    /// On id collision, **project-wins**: the project's workflow
    /// replaces the global one. The `default` slot is set from
    /// whichever layer has a `default.toml` / `name = "default"`
    /// workflow — if both do, the project's wins. When neither is
    /// present, returns an empty registry.
    ///
    /// `project_path` — optional override for the project workflows dir.
    /// When `None`, defaults to [`ConfigLayer::Project.workflows_dir()`].
    pub fn load_with_global(project_path: Option<&str>) -> OrchResult<Self> {
        let mut merged = Self::default();

        // 1) Project layer: use explicit path or fallback to ConfigLayer.
        let project_dir: Option<String> = project_path
            .filter(|p| std::fs::metadata(p).is_ok())
            .map(|s| s.to_string())
            .or_else(|| {
                latte_agent_core::config::ConfigLayer::Project
                    .workflows_dir()
                    .filter(|d| d.is_dir())
                    .and_then(|d| d.to_str().map(|s| s.to_string()))
            });
        if let Some(path) = project_dir {
            if let Ok(part) = Self::load(&path) {
                if part.default.is_some() {
                    merged.default = part.default;
                }
                for (id, wf) in part.workflows {
                    merged.workflows.insert(id, wf);
                }
            }
        }

        // 2) Global layer: always uses ConfigLayer::Global.
        if let Some(wf_dir) = latte_agent_core::config::ConfigLayer::Global.workflows_dir() {
            if wf_dir.is_dir() {
                if let Ok(global_part) = Self::load(wf_dir.to_str().unwrap()) {
                    if merged.default.is_none() {
                        merged.default = global_part.default;
                    }
                    for (id, wf) in global_part.workflows {
                        merged.workflows.entry(id).or_insert(wf);
                    }
                }
            }
        }

        Ok(merged)
    }
    /// Load every `*.toml` in `dir` (non-recursive). Each file is parsed
    /// as a single `DiscussionWorkflow`. A file named `default.toml` (or
    /// whose `name` field is "default") is also stored in the default slot.
    fn load_dir(dir: &str) -> OrchResult<Self> {
        let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
            .map_err(|e| {
                crate::error::OrchError::Config(format!(
                    "cannot read dir '{}': {}",
                    dir, e
                ))
            })?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.extension().and_then(|s| s.to_str()) == Some("toml")
            })
            .collect();
        entries.sort();

        let mut registry = Self::default();
        for entry in &entries {
            let content = std::fs::read_to_string(entry)?;
            let wf = DiscussionWorkflow::parse(&content)?;
            let id = wf.name.clone();
            if registry.workflows.contains_key(&id) {
                return Err(crate::error::OrchError::Config(format!(
                    "duplicate workflow '{}' in {}",
                    id,
                    entry.display()
                )));
            }
            // File named `default.toml` (or matching the workflow's own
            // `name` of "default") is promoted to the default slot.
            let is_default = entry.file_stem().and_then(|s| s.to_str()) == Some("default")
                || id == "default";
            if is_default {
                registry.default = Some(wf.clone());
            }
            registry.workflows.insert(id, wf);
        }
        Ok(registry)
    }

    /// Load the legacy single-file format (`discussion.toml`) with
    /// `[default_workflow]` and `[workflows.<id>]` sections.
    fn load_file(path: &str) -> OrchResult<Self> {
        let content = std::fs::read_to_string(path)?;

        #[derive(serde::Deserialize, Default)]
        #[serde(default)]
        struct WorkflowsFile {
            default_workflow: Option<DiscussionWorkflow>,
            workflows: std::collections::HashMap<String, DiscussionWorkflow>,
        }

        let wf_file: WorkflowsFile = toml::from_str(&content).map_err(|e| {
            crate::error::OrchError::Config(format!("invalid workflow TOML: {}", e))
        })?;
        Ok(Self {
            default: wf_file.default_workflow,
            workflows: wf_file.workflows,
        })
    }

    /// Resolve a workflow by name. Returns the named workflow if present,
    /// otherwise the default. Errors if neither is available.
    pub fn resolve(&self, name: Option<&str>) -> OrchResult<DiscussionWorkflow> {
        if let Some(n) = name {
            if let Some(wf) = self.workflows.get(n) {
                return Ok(wf.clone());
            }
            if n == "default" {
                return self
                    .default
                    .clone()
                    .ok_or_else(|| crate::error::OrchError::Config(
                        "no default workflow available".into(),
                    ));
            }
            return Err(crate::error::OrchError::Config(format!(
                "workflow '{}' not found. Available: {:?}",
                n,
                self.workflows.keys().collect::<Vec<_>>()
            )));
        }
        self.default
            .clone()
            .ok_or_else(|| crate::error::OrchError::Config(
                "no workflow specified and no default configured".into(),
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_workflow() {
        let toml = r#"
name = "code_review"
description = "Multi-perspective code review"
max_rounds = 1

[[steps]]
id = "explain"
speakers = ["programmer"]
prompt = "Explain the changes at {{code_path}}"

[[steps]]
id = "review"
speakers = ["reviewer", "tester"]
prompt = "Review the changes described above."
"#;

        let wf = DiscussionWorkflow::parse(toml).unwrap();
        assert_eq!(wf.name, "code_review");
        assert_eq!(wf.steps.len(), 2);
        assert_eq!(wf.steps[0].speakers, vec!["programmer"]);
        assert_eq!(wf.steps[1].speakers, vec!["reviewer", "tester"]);
    }

    #[test]
    fn test_validate_missing_agent() {
        let wf = DiscussionWorkflow {
            name: "test".into(),
            steps: vec![WorkflowStep {
                id: "s1".into(),
                description: String::new(),
                speakers: vec!["missing_agent".into()],
                prompt: "test".into(),
                hooks: vec![],
                output_key: None,
                contract: None,
            }],
            ..Default::default()
        };

        let err = wf.validate(&["pm".into(), "dev".into()]).unwrap_err();
        assert!(matches!(err, crate::error::OrchError::AgentNotFound(_, _)));
    }

    #[test]
    fn test_build_step_prompt() {
        let wf = DiscussionWorkflow::default();
        let step = WorkflowStep {
            id: "s1".into(),
            description: String::new(),
            speakers: vec!["pm".into()],
            prompt: "Topic: {{topic}}, Sprint: {{sprint}}".into(),
            hooks: vec![],
            output_key: None,
            contract: None,
        };

        let mut vars = std::collections::HashMap::new();
        vars.insert("topic".into(), "Auth".into());
        vars.insert("sprint".into(), "S3".into());

        let prompt = wf.build_step_prompt(&step, &vars);
        assert_eq!(prompt, "Topic: Auth, Sprint: S3");
    }

    #[test]
    fn test_registry_load_dir() {
        let tmp = std::env::temp_dir().join("latte_wf_registry_dir");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        std::fs::write(
            tmp.join("code_review.toml"),
            r#"
name = "code_review"
description = "Multi-perspective review"

[[steps]]
id = "explain"
speakers = ["programmer"]
prompt = "Explain."
"#,
        )
        .unwrap();
        std::fs::write(
            tmp.join("bug_triage.toml"),
            r#"
name = "bug_triage"
description = "Triage"

[[steps]]
id = "describe"
speakers = ["tester"]
prompt = "Describe."
"#,
        )
        .unwrap();
        std::fs::write(tmp.join("README.md"), "ignored").unwrap();

        let reg = WorkflowRegistry::load(tmp.to_str().unwrap()).unwrap();
        assert_eq!(reg.workflows.len(), 2);
        assert!(reg.workflows.contains_key("code_review"));
        assert!(reg.workflows.contains_key("bug_triage"));

        let wf = reg.resolve(Some("code_review")).unwrap();
        assert_eq!(wf.name, "code_review");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_registry_load_legacy_file() {
        let tmp = std::env::temp_dir().join("latte_wf_registry_file.toml");
        let _ = std::fs::remove_file(&tmp);
        std::fs::write(
            &tmp,
            r#"
[default_workflow]
name = "std"
description = "Standard"
max_rounds = 2

[[default_workflow.steps]]
id = "s1"
speakers = ["pm"]
prompt = "Hi"

[workflows.code_review]
name = "code_review"
description = "Review"
max_rounds = 1

[[workflows.code_review.steps]]
id = "s1"
speakers = ["reviewer"]
prompt = "Review."
"#,
        )
        .unwrap();

        let reg = WorkflowRegistry::load(tmp.to_str().unwrap()).unwrap();
        assert_eq!(reg.workflows.len(), 1);
        assert!(reg.default.is_some());
        assert_eq!(reg.default.as_ref().unwrap().name, "std");

        let named = reg.resolve(Some("code_review")).unwrap();
        assert_eq!(named.name, "code_review");
        let default = reg.resolve(Some("default")).unwrap();
        assert_eq!(default.name, "std");

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_registry_resolve_missing() {
        let reg = WorkflowRegistry::default();
        let err = reg.resolve(Some("nope")).unwrap_err();
        assert!(format!("{}", err).contains("not found"));
    }

    #[test]
    fn test_registry_dir_duplicate_workflow() {
        let tmp = std::env::temp_dir().join("latte_wf_registry_dup");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let body = r#"
name = "same"

[[steps]]
id = "s1"
speakers = ["pm"]
prompt = "Hi"
"#;
        std::fs::write(tmp.join("a.toml"), body).unwrap();
        std::fs::write(tmp.join("b.toml"), body).unwrap();

        let err = WorkflowRegistry::load(tmp.to_str().unwrap()).unwrap_err();
        assert!(format!("{}", err).contains("duplicate workflow 'same'"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── load_with_global ────────────────────────────────────────────────

    /// Helper: redirect `LATTE_HOME` to a temp dir for the duration of a
    /// test, restoring the previous value afterwards. Takes a global
    /// mutex so concurrent test threads don't clobber each other's
    /// `LATTE_HOME` (env vars are process-wide, but cargo test runs
    /// each `#[test]` on a separate thread).
    fn with_latte_home<F: FnOnce(&std::path::Path)>(f: F) {
        use std::sync::Mutex;
        static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
        let _guard = LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();

        let tmp = std::env::temp_dir().join(format!(
            "latte_wf_test_home_{}_{:?}",
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
        // No global layer; project workflows only.
        let tmp = std::env::temp_dir().join("latte_wf_lwg_project_only");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(
            tmp.join("code_review.toml"),
            r#"
name = "code_review"
description = "Project"
[[steps]]
id = "s1"
speakers = ["reviewer"]
prompt = "Review"
"#,
        )
        .unwrap();
        with_latte_home(|_home| {
            let reg = WorkflowRegistry::load_with_global(Some(tmp.to_str().unwrap())).unwrap();
            assert_eq!(reg.workflows.len(), 1);
            assert!(reg.workflows.contains_key("code_review"));
            assert_eq!(
                reg.workflows["code_review"].description,
                "Project"
            );
        });
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_load_with_global_fills_missing() {
        // Project has `code_review`; global has `bug_triage`. Both end
        // up in the registry, project-wins on collision.
        let project = std::env::temp_dir().join("latte_wf_lwg_fill");
        let _ = std::fs::remove_dir_all(&project);
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("code_review.toml"),
            r#"
name = "code_review"
description = "From_project"
[[steps]]
id = "s1"
speakers = ["reviewer"]
prompt = "Review"
"#,
        )
        .unwrap();
        with_latte_home(|home| {
            let wf_dir = home.join("workflows.d");
            std::fs::create_dir_all(&wf_dir).unwrap();
            std::fs::write(
                wf_dir.join("bug_triage.toml"),
                r#"
name = "bug_triage"
description = "From_global"
[[steps]]
id = "s1"
speakers = ["tester"]
prompt = "Triage"
"#,
            )
            .unwrap();
            let reg =
                WorkflowRegistry::load_with_global(Some(project.to_str().unwrap())).unwrap();
            assert_eq!(reg.workflows.len(), 2);
            assert!(reg.workflows.contains_key("code_review"));
            assert!(reg.workflows.contains_key("bug_triage"));
            assert_eq!(reg.workflows["code_review"].description, "From_project");
            assert_eq!(reg.workflows["bug_triage"].description, "From_global");
        });
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn test_load_with_global_only_global() {
        // No project path; global layer supplies everything.
        with_latte_home(|home| {
            let wf_dir = home.join("workflows.d");
            std::fs::create_dir_all(&wf_dir).unwrap();
            std::fs::write(
                wf_dir.join("default.toml"),
                r#"
name = "default"
description = "From_global_only"
[[steps]]
id = "s1"
speakers = ["pm"]
prompt = "Hi"
"#,
            )
            .unwrap();
            let reg = WorkflowRegistry::load_with_global(None).unwrap();
            assert_eq!(reg.workflows.len(), 1);
            assert!(reg.workflows.contains_key("default"));
            assert!(reg.default.is_some());
            assert_eq!(reg.default.as_ref().unwrap().description, "From_global_only");
        });
    }
}