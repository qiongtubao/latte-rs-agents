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

/// Names that the runner injects into `vars` itself (`topic` from the
/// user-provided topic, `step_id` / `speaker` per step). Declaring any
/// of these as a step's `output_key` would silently overwrite the
/// reserved value, corrupting downstream `{{…}}` substitutions.
pub const RESERVED_OUTPUT_KEYS: &[&str] = &["topic", "step_id", "speaker"];

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
#[serde(deny_unknown_fields)]
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
    /// Key under which this step's last output is stored in the shared
    /// `vars` map for downstream `{{key}}` substitution. Must be
    /// non-empty and not collide with [`RESERVED_OUTPUT_KEYS`] — see
    /// [`WorkflowDef::validate`].
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

    /// Validate `output_key` values before the workflow runs.
    /// Rejects empty keys and keys that collide with
    /// [`RESERVED_OUTPUT_KEYS`], which would silently corrupt
    /// shared `vars`. Called at the start of [`run_workflow`]
    /// and in [`load_workflow`] so that invalid TOML is caught
    /// at parse time rather than during execution.
    pub fn validate(&self) -> Result<(), String> {
        for step in &self.steps {
            if let Some(key) = &step.output_key {
                if key.is_empty() {
                    return Err(format!(
                        "step '{}': output_key must not be empty",
                        step.id
                    ));
                }
                if RESERVED_OUTPUT_KEYS.contains(&key.as_str()) {
                    return Err(format!(
                        "step '{}': output_key '{key}' is reserved \
                         (used by the runner for topic / step context); \
                         pick a non-reserved name",
                        step.id
                    ));
                }
            }
        }
        Ok(())
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
            wf.validate()?;
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
    wf.validate()?;
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
            .ok_or_else(|| {
                format!(
                    "role '{role_id}' not found in config. available roles: {}",
                    crate::controller::role_roster_text(&ctx.merged)
                )
            })?
            .clone();
        let role = template
            .resolve(&ctx.default_params)
            .await
            .map_err(|e| format!("resolve role '{role_id}': {e}"))?;
        // 与 chat 的 build_runner 一致：带工具的角色必须拿到工具调用协议
        // 提示 + 系统 ground truth（cwd 等），否则模型不知道该用工具，
        // 会回答"我没有文件访问权限"。
        let mut role = role;
        if !role.allowed_tools.is_empty() {
            role.system_prompt
                .push_str(&crate::controller::tool_usage_prompt(&role.allowed_tools));
        }
        role.system_prompt
            .push_str(&crate::ground_truth::ground_truth_block(&ctx.cwd));
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

    /// 验收：feature_design.toml 的 design step 必须含有 output_key="design"。
    /// 使用 include_str! 直接引用真文件，确保 TOML 编辑后测试立即红。
    #[test]
    fn feature_design_design_step_has_output_key() {
        let raw = include_str!("../../config/workflows/feature_design.toml");
        let wf: WorkflowDef = toml::from_str(raw).expect("valid TOML");
        let design = wf.steps.iter().find(|s| s.id == "design")
            .expect("step 'design' must exist");
        assert_eq!(
            design.output_key,
            Some("design".to_string()),
            "step 'design' missing output_key=\"design\" — required by LAT-106"
        );
    }

    /// 验收：所有 step 的 prompt 中不包含"上面的"模糊引用。
    /// 表驱动：未来新增 step 或 forbidden phrase 时无需改测试逻辑。
    #[test]
    fn no_vague_references_in_prompts() {
        let raw = include_str!("../../config/workflows/feature_design.toml");
        let wf: WorkflowDef = toml::from_str(raw).expect("valid TOML");
        let forbidden = ["上面的"];
        for step in &wf.steps {
            for phrase in &forbidden {
                assert!(
                    !step.prompt.contains(phrase),
                    "step '{}' prompt contains forbidden vague reference '{}': {}",
                    step.id, phrase, step.prompt
                );
            }
        }
    }

    /// 集成行为验证：design step 的 prompt 渲染时正确注入 {{requirements}} 和 {{topic}}。
    #[test]
    fn design_step_prompt_renders_with_requirements_var() {
        let raw = include_str!("../../config/workflows/feature_design.toml");
        let wf: WorkflowDef = toml::from_str(raw).expect("valid TOML");
        let design = wf.steps.iter().find(|s| s.id == "design").unwrap();
        let mut vars = HashMap::new();
        vars.insert("topic".into(), "AI 助手".into());
        vars.insert("requirements".into(), "用户能创建笔记".into());
        let rendered = wf.render_prompt(design, &vars);
        assert!(rendered.contains("用户能创建笔记"), "must inject {{requirements}}: {rendered}");
        assert!(rendered.contains("AI 助手"), "must inject {{topic}}: {rendered}");
        // 确保没有残留的未替换模板语法
        assert!(!rendered.contains("{{{"), "no unsubstituted template vars: {rendered}");
    }

    /// advisor_verdict 的 prompt 必须引用 {{requirements}} 和 {{design}} 两个 output_key。
    #[test]
    fn advisor_verdict_prompt_uses_named_vars() {
        let raw = include_str!("../../config/workflows/feature_design.toml");
        let wf: WorkflowDef = toml::from_str(raw).expect("valid TOML");
        let verdict = wf.steps.iter().find(|s| s.id == "advisor_verdict")
            .expect("step 'advisor_verdict' must exist");
        assert!(
            verdict.prompt.contains("{{requirements}}"),
            "advisor_verdict prompt must reference {{requirements}}"
        );
        assert!(
            verdict.prompt.contains("{{design}}"),
            "advisor_verdict prompt must reference {{design}} (from step 'design' output_key)"
        );
    }

    /// Reserved `output_key` names (`topic`, `step_id`, `speaker`) must be rejected.
    #[test]
    fn reject_reserved_output_key() {
        for reserved in ["topic", "step_id", "speaker"] {
            let raw = format!(
                r#"
name = "bad"
[[steps]]
id = "s"
speakers = ["pm"]
prompt = "do {{}}"
output_key = "{reserved}"
"#
            );
            let wf: WorkflowDef = toml::from_str(&raw).unwrap();
            let err = wf.validate().expect_err("reserved key must be rejected");
            assert!(err.contains("reserved"), "got: {err}");
        }
    }

    /// Empty `output_key` must be rejected.
    #[test]
    fn reject_empty_output_key() {
        let raw = r#"
name = "bad"
[[steps]]
id = "s"
speakers = ["pm"]
prompt = "p"
output_key = ""
"#;
        let wf: WorkflowDef = toml::from_str(raw).expect("valid TOML");
        let err = wf.validate().expect_err("empty key must be rejected");
        assert!(err.contains("empty"), "got: {err}");
    }

    /// Normal non-reserved, non-empty keys pass validation.
    #[test]
    fn validate_accepts_legitimate_output_keys() {
        let raw = r#"
name = "ok"
[[steps]]
id = "design"
speakers = ["architect"]
prompt = "do {{requirements}}"
output_key = "design"
[[steps]]
id = "review"
speakers = ["reviewer"]
prompt = "review {{design}}"
"#;
        let wf: WorkflowDef = toml::from_str(raw).unwrap();
        wf.validate().expect("non-reserved, non-empty keys must validate");
    }

    /// Typo `ouput_key` (missing 't') must be rejected at parse time by
    /// `#[serde(deny_unknown_fields)]`, not silently treated as missing.
    #[test]
    fn reject_typo_ouput_key() {
        let raw = r#"
name = "typo"
[[steps]]
id = "s"
speakers = ["pm"]
prompt = "p"
ouput_key = "design"
"#;
        let result: Result<WorkflowDef, _> = toml::from_str(raw);
        assert!(
            result.is_err(),
            "typo 'ouput_key' must be rejected at parse time"
        );
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("ouput_key"), "error should mention the unknown field: {msg}");
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
