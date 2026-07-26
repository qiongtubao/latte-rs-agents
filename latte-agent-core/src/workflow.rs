//! Minimal workflow definition + loader for the manager's `workflow` tool.
//!
//! This is a deliberately small subset of `latte-agent-orchestrator`'s
//! `DiscussionWorkflow`: `latte-agent-core` cannot depend on the
//! orchestrator crate (the orchestrator depends on core — a cycle),
//! so the manager-facing workflow tool parses the same TOML format
//! here. Fields we don't need (hooks, contracts, consensus) are
//! ignored but tolerated.
//!
//! Resolution order for `load(name)`:
//!   1. `<project>/.latte/workflows.d/<name>.toml`
//!   2. `$LATTE_HOME/workflows.d/<name>.toml` (or `~/.latte/workflows.d/`)

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use latte_ai::models::Message;
use latte_ai::params::GenerateParams;
use tokio::sync::broadcast;

use crate::agent::{Agent, AgentRunner};
use crate::config::AgentConfig;
use crate::controller::{build_tool_manager, ChatEvent};
use crate::model_resolver::ModelResolver;

/// One parsed workflow file.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkflowDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub max_rounds: Option<usize>,
    #[serde(default)]
    pub steps: Vec<WorkflowStepDef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkflowStepDef {
    pub id: String,
    #[serde(default)]
    pub description: String,
    /// New single-role task form. `speakers` remains accepted for legacy workflows.
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub task: String,
    #[serde(default)]
    pub speakers: Vec<String>,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub output_key: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub max_retries: u32,
    #[serde(default)]
    pub loop_until: Option<String>,
    #[serde(default)]
    pub max_iterations: Option<usize>,
}

impl WorkflowStepDef {
    pub fn roles(&self) -> Vec<String> {
        if let Some(role) = &self.role { vec![role.clone()] } else { self.speakers.clone() }
    }

    pub fn task_text(&self) -> &str {
        if self.task.is_empty() { &self.prompt } else { &self.task }
    }
}

impl WorkflowDef {
    pub fn effective_max_rounds(&self) -> usize {
        self.max_rounds.unwrap_or(1).max(1)
    }

    pub fn speaker_roles(&self) -> Vec<String> {
        let mut out = Vec::new();
        for step in &self.steps {
            for role in step.roles() {
                if !out.contains(&role) { out.push(role); }
            }
        }
        out
    }

    pub fn render_task(
        &self,
        step: &WorkflowStepDef,
        vars: &std::collections::HashMap<String, String>,
    ) -> String {
        let mut out = step.task_text().to_string();
        for (key, value) in vars {
            out = out.replace(&format!("{{{{{key}}}}}"), value);
        }
        out
    }

    pub fn render_prompt(
        &self,
        step: &WorkflowStepDef,
        vars: &std::collections::HashMap<String, String>,
    ) -> String {
        self.render_task(step, vars)
    }
}

fn workflows_dirs(project_cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![project_cwd.join(".latte").join("workflows.d")];
    let home = std::env::var_os("LATTE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".latte")));
    if let Some(h) = home {
        dirs.push(h.join("workflows.d"));
    }
    dirs
}

/// Load a workflow by name from the project `.latte/workflows.d/`
/// (preferred) or the global `$LATTE_HOME/workflows.d/`.
pub fn load_workflow(name: &str, project_cwd: &Path) -> Result<WorkflowDef, String> {
    for dir in workflows_dirs(project_cwd) {
        let path = dir.join(format!("{name}.toml"));
        if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let wf: WorkflowDef = toml::from_str(&raw)
                .map_err(|e| format!("invalid workflow {}: {e}", path.display()))?;
            if wf.steps.is_empty() {
                return Err(format!("workflow '{name}' has no steps"));
            }
            return Ok(wf);
        }
    }
    Err(format!(
        "workflow '{name}' not found in {}",
        workflows_dirs(project_cwd)
            .iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// List available workflows (name + description) for hints/errors.
pub fn list_workflows(project_cwd: &Path) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for dir in workflows_dirs(project_cwd) {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("toml") {
                continue;
            }
            let name = match path.file_stem().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if out.iter().any(|(n, _)| *n == name) {
                continue; // project copy shadows global
            }
            let desc = std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| toml::from_str::<WorkflowDef>(&raw).ok())
                .map(|wf| wf.description)
                .unwrap_or_default();
            out.push((name, desc));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

// ─── Reusable workflow runner ─────────────────────────────────────
//
// The engine behind the manager's `workflow` tool (see
// `controller::register_workflow_tool`, now a thin wrapper over
// [`run_workflow`]) and the UI server's workflow test-run endpoint.
// Semantics: one persistent `AgentRunner` per speaker role, rounds ×
// steps × speakers loop, `{{var}}` substitution (`topic` + step
// `output_key`s), progress streamed as
// WorkflowStarted/Step/Turn/Finished events on `event_tx`.

/// Everything a workflow run needs from its host (controller tool
/// handler, UI server test-run endpoint, ...).
pub struct WorkflowRunContext {
    /// Merged agent config (roles). Cloned per run by the host.
    pub merged: Arc<AgentConfig>,
    pub resolver: Arc<ModelResolver>,
    pub default_params: GenerateParams,
    pub cwd: PathBuf,
    pub event_tx: broadcast::Sender<ChatEvent>,
    pub cancel_flag: Arc<AtomicBool>,
}

/// Run a loaded workflow to completion. Returns the last speaker's
/// output on success; on cancel/failure emits `WorkflowFinished` with
/// the matching status and returns the summary as `Err`.
pub async fn run_workflow(
    wf: &WorkflowDef,
    topic: &str,
    ctx: &WorkflowRunContext,
) -> Result<String, String> {
    let name = wf.name.clone();
    let wf_id = format!(
        "wf-{}-{}",
        name,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros())
            .unwrap_or(0)
    );
    let _ = ctx.event_tx.send(ChatEvent::WorkflowStarted {
        name: name.clone(),
        topic: topic.to_string(),
        wf_id: wf_id.clone(),
    });

    // One runner per scheduled role; advisor is an internal monitor only.
    let mut runners: HashMap<String, AgentRunner> = HashMap::new();
    for role_id in wf.speaker_roles() {
        if role_id == "advisor" {
            return Err("advisor is monitor-only; use reviewer for workflow tasks".into());
        }
        let template = ctx
            .merged
            .roles
            .get(&role_id)
            .ok_or_else(|| format!("role '{role_id}' not found in config"))?
            .clone();
        let role = template
            .resolve(&ctx.default_params)
            .await
            .map_err(|e| format!("resolve role '{role_id}': {e}"))?;
        let models = ctx
            .resolver
            .resolve_chain(&role.id, role.default_model_tier, &role.model_chain)
            .map_err(|e| format!("no model for role '{role_id}': {e}"))?;
        let agent = Agent::new_with_chain(role_id.clone(), role.clone(), models, ctx.default_params.clone())
            .map_err(|e| format!("create agent '{role_id}': {e}"))?;
        let runner = if role.allowed_tools.is_empty() {
            AgentRunner::new(agent)
        } else {
            let rtm = build_tool_manager(&role.allowed_tools)
                .await
                .map_err(|e| format!("tools for '{role_id}': {e}"))?;
            AgentRunner::new_with_tools(agent, rtm, 0)
        };
        runners.insert(role_id.clone(), runner.with_role(role_id).with_cwd(ctx.cwd.clone()));
    }

    let mut vars: HashMap<String, String> = HashMap::new();
    vars.insert("topic".into(), topic.to_string());
    let total = wf.steps.len();
    let mut last_output = String::new();
    let mut status = "ok";
    let mut error_msg = String::new();

    'rounds: for round in 0..wf.effective_max_rounds() {
        for (idx, step) in wf.steps.iter().enumerate() {
            if ctx.cancel_flag.load(Ordering::SeqCst) {
                status = "cancelled";
                break 'rounds;
            }
            let _ = ctx.event_tx.send(ChatEvent::WorkflowStep {
                wf_id: wf_id.clone(),
                step_id: step.id.clone(),
                description: step.description.clone(),
                index: idx + 1,
                total,
            });
            let mut step_transcript = String::new();
            for speaker in step.roles() {
                if speaker == "advisor" {
                    status = "failed";
                    error_msg = "advisor is monitor-only; use reviewer".into();
                    break 'rounds;
                }
                if ctx.cancel_flag.load(Ordering::SeqCst) {
                    status = "cancelled";
                    break 'rounds;
                }
                let runner = match runners.get_mut(&speaker) {
                    Some(r) => r,
                    None => {
                        status = "failed";
                        error_msg = format!("role '{speaker}' not instantiated");
                        break 'rounds;
                    }
                };
                let mut step_vars = vars.clone();
                step_vars.insert("step_id".into(), step.id.clone());
                step_vars.insert("speaker".into(), speaker.clone());
                let base_prompt = wf.render_task(step, &step_vars);
                let prompt = if step_transcript.is_empty() {
                    base_prompt
                } else {
                    format!("{base_prompt}\n\n--- Preceding discussion in this step ---\n{step_transcript}")
                };
                match runner
                    .run_turn(
                        &[Message::user(prompt)],
                        None,
                    )
                    .await
                {
                    Ok(response) => {
                        let _ = ctx.event_tx.send(ChatEvent::WorkflowTurn {
                            wf_id: wf_id.clone(),
                            step_id: step.id.clone(),
                            role_id: speaker.clone(),
                            content: response.clone(),
                            round,
                        });
                        step_transcript.push_str(&format!("[{speaker}]: {response}\n"));
                        last_output = response;
                    }
                    Err(e) => {
                        status = "failed";
                        error_msg = format!("step '{}' speaker '{}': {e}", step.id, speaker);
                        break 'rounds;
                    }
                }
            }
            if let Some(key) = &step.output_key {
                vars.insert(key.clone(), last_output.clone());
            }
        }
    }

    match status {
        "ok" => {
            let _ = ctx.event_tx.send(ChatEvent::WorkflowFinished {
                name,
                wf_id,
                status: "ok".into(),
                summary: last_output.clone(),
            });
            Ok(last_output)
        }
        s => {
            let summary = if s == "cancelled" {
                "workflow cancelled by user".to_string()
            } else {
                error_msg
            };
            let _ = ctx.event_tx.send(ChatEvent::WorkflowFinished {
                name,
                wf_id,
                status: s.into(),
                summary: summary.clone(),
            });
            Err(summary)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_workflow() {
        let raw = r#"
name = "demo"
description = "d"
max_rounds = 2
[[steps]]
id = "a"
speakers = ["pm"]
prompt = "do {{topic}}"
[[steps]]
id = "b"
speakers = ["architect", "advisor"]
prompt = "review"
"#;
        let wf: WorkflowDef = toml::from_str(raw).unwrap();
        assert_eq!(wf.name, "demo");
        assert_eq!(wf.effective_max_rounds(), 2);
        assert_eq!(wf.speaker_roles(), vec!["pm", "architect", "advisor"]);
        let vars = std::collections::HashMap::from([("topic".to_string(), "X".to_string())]);
        assert_eq!(wf.render_prompt(&wf.steps[0], &vars), "do X");
    }
}

#[cfg(test)]
mod task_schema_tests {
    use super::*;

    #[test]
    fn single_role_task_supports_dependencies_and_loop() {
        let raw = r#"
name = "custom"
[[steps]]
id = "implement"
role = "programmer"
task = "implement {{topic}}"
depends_on = ["tests"]
max_retries = 2
loop_until = "tests_pass"
max_iterations = 3
"#;
        let wf: WorkflowDef = toml::from_str(raw).unwrap();
        let step = &wf.steps[0];
        assert_eq!(step.roles(), vec!["programmer"]);
        assert_eq!(step.task_text(), "implement {{topic}}");
        assert_eq!(step.depends_on, vec!["tests"]);
        assert_eq!(step.max_retries, 2);
        assert_eq!(step.max_iterations, Some(3));
    }

    #[test]
    fn legacy_speakers_and_prompt_remain_supported() {
        let raw = r#"
name = "legacy"
[[steps]]
id = "review"
speakers = ["reviewer"]
prompt = "review it"
"#;
        let wf: WorkflowDef = toml::from_str(raw).unwrap();
        assert_eq!(wf.steps[0].roles(), vec!["reviewer"]);
        assert_eq!(wf.steps[0].task_text(), "review it");
    }
}
