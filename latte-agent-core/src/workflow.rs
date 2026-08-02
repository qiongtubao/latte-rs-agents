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
use crate::controller::{build_tool_manager, register_plan_tool, ChatEvent};
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
    pub command: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub max_rounds: Option<usize>,
    #[serde(default)]
    pub steps: Vec<WorkflowStepDef>,
}

impl WorkflowDef {
    /// Check if the workflow has a command and it matches the given input.
    pub fn matches_command(&self, cmd: &str) -> bool {
        self.command.as_deref() == Some(cmd)
    }
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
    /// Ids of steps that must complete before this one runs.
    ///
    /// Declaring `depends_on` on **any** step switches the whole
    /// workflow from implicit file-order serial execution to the
    /// dependency-DAG scheduler: steps run in waves, and steps sharing
    /// a wave (no unsatisfied deps between them) run **concurrently**.
    /// Steps with the same `depends_on` fan out in parallel; a step
    /// listing several deps fans in (waits for all of them).
    ///
    /// Leave empty on every step (the default) to keep the legacy
    /// serial semantics — existing workflows are unaffected.
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub max_retries: u32,
    #[serde(default)]
    pub loop_until: Option<String>,
    #[serde(default)]
    pub max_iterations: Option<usize>,
    /// Nest another workflow as this step: the named workflow runs with
    /// the rendered `task` as its topic (empty `task` passes the parent
    /// topic through unchanged), and its final output binds to this
    /// step's `output_key` when set (omit the key to discard the
    /// output — normal for composition). Mutually exclusive with
    /// `role`/`speakers` — see [`WorkflowDef::validate`]. Combine
    /// several nested steps with `depends_on` to compose workflows
    /// serially (chain) or in parallel (same wave). Nesting depth is
    /// capped at [`MAX_WORKFLOW_DEPTH`] to prevent cycles.
    #[serde(default)]
    pub workflow: Option<String>,
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
            if let Some(nested) = &step.workflow {
                if nested.trim().is_empty() {
                    return Err(format!(
                        "step '{}': workflow name must not be empty",
                        step.id
                    ));
                }
                if nested == &self.name {
                    return Err(format!(
                        "step '{}': workflow '{}' must not nest itself",
                        step.id, self.name
                    ));
                }
                if step.role.is_some() || !step.speakers.is_empty() {
                    return Err(format!(
                        "step '{}': `workflow` (nested) is mutually exclusive with role/speakers",
                        step.id
                    ));
                }
            }
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

    /// Whether this workflow uses explicit `depends_on` on any step.
    /// When true, [`run_workflow`] switches from implicit file-order
    /// serial execution to the dependency-DAG scheduler (independent
    /// steps run concurrently; `depends_on` edges serialize). When
    /// false, behavior is the legacy serial file-order loop so existing
    /// workflows are byte-for-byte unchanged.
    pub fn uses_dependency_dag(&self) -> bool {
        self.steps.iter().any(|s| !s.depends_on.is_empty())
    }

    /// Validate the `depends_on` graph before running in DAG mode:
    /// step ids are unique, every `depends_on` names an existing step,
    /// no step depends on itself, and there are no cycles. Called from
    /// [`run_workflow`] when [`Self::uses_dependency_dag`] is true.
    pub fn validate_dag(&self) -> Result<(), String> {
        use std::collections::HashSet;
        let mut ids: HashSet<&str> = HashSet::new();
        for step in &self.steps {
            if !ids.insert(step.id.as_str()) {
                return Err(format!("duplicate step id '{}'", step.id));
            }
        }
        for step in &self.steps {
            for dep in &step.depends_on {
                if dep == &step.id {
                    return Err(format!("step '{}' depends on itself", step.id));
                }
                if !ids.contains(dep.as_str()) {
                    return Err(format!(
                        "step '{}' depends_on unknown step '{}'",
                        step.id, dep
                    ));
                }
            }
        }
        // `compute_waves` returns Err on any unsatisfiable / cyclic graph.
        compute_waves(&self.steps).map(|_| ())
    }
}

/// Group steps into dependency "waves" for concurrent execution.
///
/// Wave 0 = all steps whose `depends_on` is empty (or already satisfied).
/// Wave N = steps whose deps are all in waves `< N`. Steps within a
/// wave have no ordering constraint between them and run concurrently;
/// waves themselves run in order. File order is preserved within a wave
/// for deterministic event/output ordering.
///
/// Returns `Err` if the graph is cyclic or references a missing step
/// (a wave comes up empty while steps remain).
fn compute_waves(steps: &[WorkflowStepDef]) -> Result<Vec<Vec<usize>>, String> {
    let id_to_idx: HashMap<&str, usize> = steps
        .iter()
        .enumerate()
        .map(|(i, s)| (s.id.as_str(), i))
        .collect();
    let mut done = vec![false; steps.len()];
    let mut remaining = steps.len();
    let mut waves: Vec<Vec<usize>> = Vec::new();
    while remaining > 0 {
        let mut wave = Vec::new();
        for (i, s) in steps.iter().enumerate() {
            if done[i] {
                continue;
            }
            let ready = s.depends_on.iter().all(|dep| {
                id_to_idx
                    .get(dep.as_str())
                    .map(|&j| done[j])
                    .unwrap_or(false)
            });
            if ready {
                wave.push(i);
            }
        }
        if wave.is_empty() {
            return Err(
                "workflow has a dependency cycle or unsatisfiable depends_on".into(),
            );
        }
        for &i in &wave {
            done[i] = true;
        }
        remaining -= wave.len();
        waves.push(wave);
    }
    Ok(waves)
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

/// List workflows that have a `command` field set, returning (command, name) pairs.
pub fn list_workflow_commands(project_cwd: &Path) -> Vec<(String, String)> {
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
            if out.iter().any(|(_, n)| *n == name) {
                continue;
            }
            let raw = match std::fs::read_to_string(&path) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let wf: WorkflowDef = match toml::from_str(&raw) {
                Ok(w) => w,
                Err(_) => continue,
            };
            if let Some(cmd) = wf.command {
                out.push((cmd, name));
            }
        }
    }
    out.sort();
    out
}

/// Load a workflow by its command (e.g. "/plan").
pub fn load_workflow_by_command(cmd: &str, project_cwd: &Path) -> Result<WorkflowDef, String> {
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
            let raw = match std::fs::read_to_string(&path) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let wf: WorkflowDef = match toml::from_str(&raw) {
                Ok(w) => w,
                Err(_) => continue,
            };
            if wf.matches_command(cmd) {
                wf.validate()?;
                if wf.steps.is_empty() {
                    return Err(format!("workflow '{}' (cmd {cmd}) has no steps", wf.name));
                }
                return Ok(wf);
            }
        }
    }
    Err(format!("no workflow found with command '{cmd}'"))
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
#[derive(Clone)]
pub struct WorkflowRunContext {
    /// Merged agent config (roles). Cloned per run by the host.
    pub merged: Arc<AgentConfig>,
    pub resolver: Arc<ModelResolver>,
    pub default_params: GenerateParams,
    pub cwd: PathBuf,
    pub event_tx: broadcast::Sender<ChatEvent>,
    pub cancel_flag: Arc<AtomicBool>,
    /// Nesting depth: 0 for a top-level run, +1 per nested workflow
    /// step. Guarded against [`MAX_WORKFLOW_DEPTH`] to stop cycles.
    pub depth: u8,
}

/// Maximum nesting depth for workflow steps that invoke another
/// workflow (`workflow = "..."` on a step). Deeper nesting is rejected
/// with an error — almost always a cycle or a design mistake.
pub const MAX_WORKFLOW_DEPTH: u8 = 3;

/// Run a nested workflow step: load the named workflow and run it with
/// `topic`, sharing the parent's config / event channel / cancel flag.
/// Events of the nested run stream under their own wf_id.
///
/// This is deliberately a **plain fn** returning a boxed `'static`
/// future, not an `async fn`: nested steps re-enter `run_workflow`
/// from inside both engines, and an `async fn` here would make the
/// engines' opaque return types depend on `run_workflow`'s own opaque
/// type — a cycle the compiler rejects. The explicit boxed type severs
/// that dependency.
fn run_nested_workflow(
    name: String,
    topic: String,
    ctx: WorkflowRunContext,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send>> {
    Box::pin(async move {
        if ctx.depth >= MAX_WORKFLOW_DEPTH {
            return Err(format!(
                "nested workflow '{name}' exceeds max depth {MAX_WORKFLOW_DEPTH} (cycle?)"
            ));
        }
        let wf = load_workflow(&name, &ctx.cwd).map_err(|e| {
            let available = list_workflows(&ctx.cwd)
                .iter()
                .map(|(n, _)| n.clone())
                .collect::<Vec<_>>()
                .join(", ");
            format!("nested workflow '{name}': {e}. available workflows: {available}")
        })?;
        let nested_ctx = WorkflowRunContext {
            merged: ctx.merged.clone(),
            resolver: ctx.resolver.clone(),
            default_params: ctx.default_params.clone(),
            cwd: ctx.cwd.clone(),
            event_tx: ctx.event_tx.clone(),
            cancel_flag: ctx.cancel_flag.clone(),
            depth: ctx.depth + 1,
        };
        run_workflow(&wf, &topic, &nested_ctx).await
    })
}

/// Internal outcome of a workflow engine (serial or DAG), before the
/// `WorkflowFinished` event is emitted by [`run_workflow`].
enum WfOutcome {
    Ok(String),
    Cancelled,
    Failed(String),
}

/// Max concurrent steps within a single dependency wave. Independent
/// steps fan out via `tokio::task::JoinSet`; this caps how many
/// specialists run at once so a wide wave can't exhaust models /
/// sockets. Override with `LATTE_WORKFLOW_CONCURRENCY` (default 4).
fn workflow_concurrency() -> usize {
    std::env::var("LATTE_WORKFLOW_CONCURRENCY")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(4)
}

/// Run a loaded workflow to completion. Returns the last step's output
/// on success; on cancel/failure emits `WorkflowFinished` with the
/// matching status and returns the summary as `Err`.
///
/// Dispatches to one of two engines:
///   - **serial** (default): steps run in file order with one
///     persistent `AgentRunner` per role (conversation continuity
///     across steps/rounds). Used when no step declares `depends_on`.
///   - **DAG** (opt-in): when any step declares `depends_on`, steps run
///     in dependency waves — independent steps concurrently, dependent
///     steps serialized. Each step gets a fresh runner; data flows via
///     `{{output_key}}` vars.
pub async fn run_workflow(
    wf: &WorkflowDef,
    topic: &str,
    ctx: &WorkflowRunContext,
) -> Result<String, String> {
    wf.validate()?;
    let uses_dag = wf.uses_dependency_dag();
    if uses_dag {
        wf.validate_dag()?;
    }
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

    let outcome = if uses_dag {
        run_workflow_dag(wf, topic, ctx, &wf_id).await
    } else {
        run_workflow_serial(wf, topic, ctx, &wf_id).await
    };

    match outcome {
        WfOutcome::Ok(last_output) => {
            let _ = ctx.event_tx.send(ChatEvent::WorkflowFinished {
                name,
                wf_id,
                status: "ok".into(),
                summary: last_output.clone(),
            });
            Ok(last_output)
        }
        WfOutcome::Cancelled => {
            let summary = "workflow cancelled by user".to_string();
            let _ = ctx.event_tx.send(ChatEvent::WorkflowFinished {
                name,
                wf_id,
                status: "cancelled".into(),
                summary: summary.clone(),
            });
            Err(summary)
        }
        WfOutcome::Failed(msg) => {
            let _ = ctx.event_tx.send(ChatEvent::WorkflowFinished {
                name,
                wf_id,
                status: "failed".into(),
                summary: msg.clone(),
            });
            Err(msg)
        }
    }
}

/// Build a fresh `AgentRunner` for one role, wired exactly like the
/// chat/controller `build_runner`: tool-capable roles get the tool-use
/// protocol prompt + ground-truth block, plan tool registered when
/// allowed. Shared by the serial engine (one per role) and the DAG
/// engine (one per step invocation). `advisor` is rejected (monitor
/// only).
async fn build_role_runner(
    role_id: &str,
    merged: &Arc<AgentConfig>,
    resolver: &Arc<ModelResolver>,
    default_params: &GenerateParams,
    cwd: &Path,
    event_tx: &broadcast::Sender<ChatEvent>,
) -> Result<AgentRunner, String> {
    if role_id == "advisor" {
        return Err("advisor is monitor-only; use reviewer for workflow tasks".into());
    }
    let template = merged
        .roles
        .get(role_id)
        .ok_or_else(|| {
            format!(
                "role '{role_id}' not found in config. available roles: {}",
                crate::controller::role_roster_text(merged)
            )
        })?
        .clone();
    let mut role = template
        .resolve(default_params)
        .await
        .map_err(|e| format!("resolve role '{role_id}': {e}"))?;
    // 与 chat 的 build_runner 一致：带工具的角色必须拿到工具调用协议
    // 提示 + 系统 ground truth（cwd 等），否则模型不知道该用工具，
    // 会回答"我没有文件访问权限"。
    if !role.allowed_tools.is_empty() {
        role.system_prompt
            .push_str(&crate::controller::tool_usage_prompt(&role.allowed_tools));
    }
    role.system_prompt
        .push_str(&crate::ground_truth::ground_truth_block(cwd));
    let models = resolver
        .resolve_chain(&role.id, role.default_model_tier, &role.model_chain)
        .map_err(|e| format!("no model for role '{role_id}': {e}"))?;
    let agent = Agent::new_with_chain(
        role_id.to_string(),
        role.clone(),
        models,
        default_params.clone(),
    )
    .map_err(|e| format!("create agent '{role_id}': {e}"))?;
    let runner = if role.allowed_tools.is_empty() {
        AgentRunner::new(agent)
    } else {
        let rtm = build_tool_manager(&role.allowed_tools)
            .await
            .map_err(|e| format!("tools for '{role_id}': {e}"))?;
        // 注册 plan 工具：角色有"plan"时，注册 tool 使其在 LLM 可见
        // （与 controller::build_runner 对齐）
        if role.allowed_tools.iter().any(|t| t == "plan") {
            register_plan_tool(&rtm, event_tx.clone(), role_id.to_string())
                .map_err(|e| format!("register plan for '{role_id}': {e}"))?;
        }
        AgentRunner::new_with_tools(agent, rtm, 0)
    };
    Ok(runner.with_role(role_id.to_string()).with_cwd(cwd.to_path_buf()))
}

/// Legacy serial engine: file-order steps, one persistent runner per
/// role (conversation continuity across steps/rounds). Behavior is
/// unchanged from before the DAG scheduler was introduced.
async fn run_workflow_serial(
    wf: &WorkflowDef,
    topic: &str,
    ctx: &WorkflowRunContext,
    wf_id: &str,
) -> WfOutcome {
    // One runner per scheduled role; advisor is an internal monitor only.
    let mut runners: HashMap<String, AgentRunner> = HashMap::new();
    for role_id in wf.speaker_roles() {
        match build_role_runner(
            &role_id,
            &ctx.merged,
            &ctx.resolver,
            &ctx.default_params,
            &ctx.cwd,
            &ctx.event_tx,
        )
        .await
        {
            Ok(runner) => {
                runners.insert(role_id, runner);
            }
            Err(e) => return WfOutcome::Failed(e),
        }
    }

    let mut vars: HashMap<String, String> = HashMap::new();
    vars.insert("topic".into(), topic.to_string());
    let total = wf.steps.len();
    let mut last_output = String::new();

    for round in 0..wf.effective_max_rounds() {
        for (idx, step) in wf.steps.iter().enumerate() {
            if ctx.cancel_flag.load(Ordering::SeqCst) {
                return WfOutcome::Cancelled;
            }
            let first_role = step.roles().first().cloned().unwrap_or_default();
            let _ = ctx.event_tx.send(ChatEvent::WorkflowStep {
                wf_id: wf_id.to_string(),
                step_id: step.id.clone(),
                description: step.description.clone(),
                index: idx + 1,
                total,
                role_id: first_role,
                task: step.task_text().to_string(),
            });
            let mut step_transcript = String::new();
            // Nested workflow step: run the named workflow with the
            // rendered task as its topic; bind its final output.
            if let Some(nested_name) = step.workflow.clone() {
                let mut step_vars = vars.clone();
                step_vars.insert("step_id".into(), step.id.clone());
                // 空 task = 组合场景：把父 workflow 的 topic 原样透传。
                let nested_topic = if step.task_text().trim().is_empty() {
                    vars.get("topic").cloned().unwrap_or_default()
                } else {
                    wf.render_task(step, &step_vars)
                };
                match run_nested_workflow(nested_name.clone(), nested_topic, ctx.clone()).await {
                    Ok(output) => {
                        let _ = ctx.event_tx.send(ChatEvent::WorkflowTurn {
                            wf_id: wf_id.to_string(),
                            step_id: step.id.clone(),
                            role_id: format!("workflow:{nested_name}"),
                            content: crate::controller::strip_think_blocks(&output),
                            round,
                        });
                        last_output = output;
                    }
                    Err(e) => {
                        return WfOutcome::Failed(format!(
                            "step '{}' nested workflow '{nested_name}': {e}",
                            step.id
                        ))
                    }
                }
                if let Some(key) = &step.output_key {
                    vars.insert(key.clone(), last_output.clone());
                }
                continue;
            }
            for speaker in step.roles() {
                if ctx.cancel_flag.load(Ordering::SeqCst) {
                    return WfOutcome::Cancelled;
                }
                let runner = match runners.get_mut(&speaker) {
                    Some(r) => r,
                    None => {
                        return WfOutcome::Failed(format!(
                            "role '{speaker}' not instantiated"
                        ))
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
                match runner.run_turn(&[Message::user(prompt)], None).await {
                    Ok(response) => {
                        // 事件里剥离 <think>（主 session 展示用）；
                        // step_transcript / last_output 保留原文供后续
                        // speaker 与最终总结使用。
                        let _ = ctx.event_tx.send(ChatEvent::WorkflowTurn {
                            wf_id: wf_id.to_string(),
                            step_id: step.id.clone(),
                            role_id: speaker.clone(),
                            content: crate::controller::strip_think_blocks(&response),
                            round,
                        });
                        step_transcript.push_str(&format!("[{speaker}]: {response}\n"));
                        last_output = response;
                    }
                    Err(e) => {
                        return WfOutcome::Failed(format!(
                            "step '{}' speaker '{}': {e}",
                            step.id, speaker
                        ))
                    }
                }
            }
            if let Some(key) = &step.output_key {
                vars.insert(key.clone(), last_output.clone());
            }
        }
    }

    WfOutcome::Ok(last_output)
}

/// One step failure signal from a spawned DAG task.
enum StepFail {
    Cancelled,
    Failed(String),
}

/// Everything a single DAG step task owns (spawned onto a `JoinSet`,
/// so all fields are `Send + 'static` clones of the run context).
struct DagStepInput {
    wf: Arc<WorkflowDef>,
    step_idx: usize,
    total: usize,
    round: usize,
    /// Snapshot of shared `vars` taken before the wave started. Steps
    /// in the same wave are independent, so they all read the same
    /// pre-wave snapshot; outputs merge back only after the wave joins.
    vars: HashMap<String, String>,
    wf_id: String,
    merged: Arc<AgentConfig>,
    resolver: Arc<ModelResolver>,
    default_params: GenerateParams,
    cwd: PathBuf,
    event_tx: broadcast::Sender<ChatEvent>,
    cancel_flag: Arc<AtomicBool>,
    depth: u8,
}

/// Run one DAG step (all its speakers, serially) with a fresh runner
/// per speaker. Returns `(step_id, output_key, last_output)` on success.
async fn run_dag_step(inp: DagStepInput) -> Result<(String, Option<String>, String), StepFail> {
    let step = &inp.wf.steps[inp.step_idx];
    if inp.cancel_flag.load(Ordering::SeqCst) {
        return Err(StepFail::Cancelled);
    }
    let first_role = step.roles().first().cloned().unwrap_or_default();
    let _ = inp.event_tx.send(ChatEvent::WorkflowStep {
        wf_id: inp.wf_id.clone(),
        step_id: step.id.clone(),
        description: step.description.clone(),
        index: inp.step_idx + 1,
        total: inp.total,
        role_id: first_role,
        task: step.task_text().to_string(),
    });

    // Nested workflow step: run the named workflow with the rendered
    // task as its topic; bind its final output to output_key.
    if let Some(nested_name) = &step.workflow {
        let mut step_vars = inp.vars.clone();
        step_vars.insert("step_id".into(), step.id.clone());
        // 空 task = 组合场景：把父 workflow 的 topic 原样透传。
        let nested_topic = if step.task_text().trim().is_empty() {
            inp.vars.get("topic").cloned().unwrap_or_default()
        } else {
            inp.wf.render_task(step, &step_vars)
        };
        let ctx = WorkflowRunContext {
            merged: inp.merged.clone(),
            resolver: inp.resolver.clone(),
            default_params: inp.default_params.clone(),
            cwd: inp.cwd.clone(),
            event_tx: inp.event_tx.clone(),
            cancel_flag: inp.cancel_flag.clone(),
            depth: inp.depth,
        };
        let output = run_nested_workflow(nested_name.clone(), nested_topic, ctx)
            .await
            .map_err(|e| {
                StepFail::Failed(format!(
                    "step '{}' nested workflow '{nested_name}': {e}",
                    step.id
                ))
            })?;
        let _ = inp.event_tx.send(ChatEvent::WorkflowTurn {
            wf_id: inp.wf_id.clone(),
            step_id: step.id.clone(),
            role_id: format!("workflow:{nested_name}"),
            content: crate::controller::strip_think_blocks(&output),
            round: inp.round,
        });
        return Ok((step.id.clone(), step.output_key.clone(), output));
    }

    let mut step_transcript = String::new();
    let mut last_output = String::new();
    for speaker in step.roles() {
        if inp.cancel_flag.load(Ordering::SeqCst) {
            return Err(StepFail::Cancelled);
        }
        let mut runner = build_role_runner(
            &speaker,
            &inp.merged,
            &inp.resolver,
            &inp.default_params,
            &inp.cwd,
            &inp.event_tx,
        )
        .await
        .map_err(StepFail::Failed)?;

        let mut step_vars = inp.vars.clone();
        step_vars.insert("step_id".into(), step.id.clone());
        step_vars.insert("speaker".into(), speaker.clone());
        let base_prompt = inp.wf.render_task(step, &step_vars);
        let prompt = if step_transcript.is_empty() {
            base_prompt
        } else {
            format!("{base_prompt}\n\n--- Preceding discussion in this step ---\n{step_transcript}")
        };
        match runner.run_turn(&[Message::user(prompt)], None).await {
            Ok(response) => {
                let _ = inp.event_tx.send(ChatEvent::WorkflowTurn {
                    wf_id: inp.wf_id.clone(),
                    step_id: step.id.clone(),
                    role_id: speaker.clone(),
                    content: crate::controller::strip_think_blocks(&response),
                    round: inp.round,
                });
                step_transcript.push_str(&format!("[{speaker}]: {response}\n"));
                last_output = response;
            }
            Err(e) => {
                return Err(StepFail::Failed(format!(
                    "step '{}' speaker '{}': {e}",
                    step.id, speaker
                )))
            }
        }
    }
    Ok((step.id.clone(), step.output_key.clone(), last_output))
}

/// DAG engine: schedule steps into dependency waves and run each wave's
/// steps concurrently. Independent steps overlap; `depends_on` edges
/// serialize. Each step gets a fresh runner (no shared conversation
/// state), so data flows only through `{{output_key}}` vars — which is
/// exactly how workflows already thread information between steps.
async fn run_workflow_dag(
    wf: &WorkflowDef,
    topic: &str,
    ctx: &WorkflowRunContext,
    wf_id: &str,
) -> WfOutcome {
    use tokio::sync::Semaphore;
    use tokio::task::JoinSet;

    let waves = match compute_waves(&wf.steps) {
        Ok(w) => w,
        Err(e) => return WfOutcome::Failed(e),
    };
    let wf_arc = Arc::new(wf.clone());
    let total = wf.steps.len();
    let sem = Arc::new(Semaphore::new(workflow_concurrency()));

    let mut vars: HashMap<String, String> = HashMap::new();
    vars.insert("topic".into(), topic.to_string());
    // step_id -> last_output, used to resolve the final return value.
    let mut outputs: HashMap<String, String> = HashMap::new();

    for round in 0..wf.effective_max_rounds() {
        for wave in &waves {
            if ctx.cancel_flag.load(Ordering::SeqCst) {
                return WfOutcome::Cancelled;
            }
            let mut set: JoinSet<Result<(String, Option<String>, String), StepFail>> =
                JoinSet::new();
            for &idx in wave {
                let inp = DagStepInput {
                    wf: wf_arc.clone(),
                    step_idx: idx,
                    total,
                    round,
                    vars: vars.clone(),
                    wf_id: wf_id.to_string(),
                    merged: ctx.merged.clone(),
                    resolver: ctx.resolver.clone(),
                    default_params: ctx.default_params.clone(),
                    cwd: ctx.cwd.clone(),
                    event_tx: ctx.event_tx.clone(),
                    cancel_flag: ctx.cancel_flag.clone(),
                    depth: ctx.depth,
                };
                let sem = sem.clone();
                set.spawn(async move {
                    // Hold a permit for the whole step so a wide wave
                    // can't launch more than `workflow_concurrency()`
                    // specialists at once. Semaphore is never closed,
                    // so `acquire_owned` only errors on shutdown — treat
                    // that as "run anyway" rather than fail the step.
                    let _permit = sem.acquire_owned().await.ok();
                    run_dag_step(inp).await
                });
            }

            // Collect the whole wave; merge outputs into `vars` only
            // after every step in the wave has finished (they were all
            // independent and read the same pre-wave snapshot).
            let mut wave_updates: Vec<(Option<String>, String)> = Vec::new();
            while let Some(joined) = set.join_next().await {
                match joined {
                    Ok(Ok((step_id, output_key, out))) => {
                        outputs.insert(step_id, out.clone());
                        wave_updates.push((output_key, out));
                    }
                    Ok(Err(StepFail::Cancelled)) => {
                        set.abort_all();
                        return WfOutcome::Cancelled;
                    }
                    Ok(Err(StepFail::Failed(msg))) => {
                        set.abort_all();
                        return WfOutcome::Failed(msg);
                    }
                    Err(join_err) => {
                        set.abort_all();
                        return WfOutcome::Failed(format!(
                            "workflow step task failed: {join_err}"
                        ));
                    }
                }
            }
            for (output_key, out) in wave_updates {
                if let Some(key) = output_key {
                    vars.insert(key, out);
                }
            }
        }
    }

    // Final output = the file-order-last step's output (the natural
    // "result" of the workflow, matching serial-mode semantics).
    let final_out = wf
        .steps
        .last()
        .and_then(|s| outputs.get(&s.id))
        .cloned()
        .unwrap_or_default();
    WfOutcome::Ok(final_out)
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

    #[test]
    fn nested_workflow_step_validation() {
        let ok = r#"
name = "outer"
[[steps]]
id = "explore"
workflow = "explore"
task = "probe {{topic}}"
output_key = "exploration"
"#;
        let wf: WorkflowDef = toml::from_str(ok).unwrap();
        wf.validate().unwrap();
        assert_eq!(wf.steps[0].workflow.as_deref(), Some("explore"));

        // 缺 output_key → 允许（组合场景下产出可丢弃）
        let missing_key = ok.replace("output_key = \"exploration\"\n", "");
        let wf: WorkflowDef = toml::from_str(&missing_key).unwrap();
        wf.validate().unwrap();

        // 与 role/speakers 互斥
        let with_role = ok.replace(
            "workflow = \"explore\"",
            "workflow = \"explore\"\nrole = \"pm\"",
        );
        let wf: WorkflowDef = toml::from_str(&with_role).unwrap();
        assert!(wf.validate().unwrap_err().contains("mutually exclusive"));

        // 自嵌套 → 拒绝
        let self_nest = ok.replace("workflow = \"explore\"", "workflow = \"outer\"");
        let wf: WorkflowDef = toml::from_str(&self_nest).unwrap();
        assert!(wf.validate().unwrap_err().contains("must not nest itself"));
    }

    #[test]
    fn init_project_workflow_parses_and_validates() {
        let raw = include_str!("../../config/workflows/init_project.toml");
        let wf: WorkflowDef = toml::from_str(raw).expect("valid TOML");
        wf.validate().expect("init_project should validate");
        wf.validate_dag().expect("init_project DAG should validate");
        assert!(wf.uses_dependency_dag());
        // 第一步是嵌套 explore workflow
        assert_eq!(wf.steps[0].workflow.as_deref(), Some("explore"));
        // project_md 依赖 explore 的产出
        let pmd = wf.steps.iter().find(|s| s.id == "project_md").unwrap();
        assert!(pmd.depends_on.iter().any(|d| d == "explore"));
        // verify 依赖全部 10 个 overlay step
        let verify = wf.steps.iter().find(|s| s.id == "verify").unwrap();
        assert_eq!(verify.depends_on.len(), 10);
        // 全部 overlay step 都依赖 project_md
        for s in wf.steps.iter().filter(|s| s.id.starts_with("overlay_")) {
            assert!(
                s.depends_on.iter().any(|d| d == "project_md"),
                "{} must depend on project_md",
                s.id
            );
        }
    }

    #[test]
    fn design_and_plan_composes_workflows_serial_and_parallel() {
        let raw = include_str!("../../config/workflows/design_and_plan.toml");
        let wf: WorkflowDef = toml::from_str(raw).expect("valid TOML");
        wf.validate().expect("design_and_plan should validate");
        wf.validate_dag().expect("design_and_plan DAG should validate");

        // 波浪调度：串行链分层、并行组同层
        let waves = compute_waves(&wf.steps).unwrap();
        let wave_of = |id: &str| {
            waves
                .iter()
                .position(|w| w.iter().any(|&i| wf.steps[i].id == id))
                .unwrap()
        };
        // 串行链 explore → brainstorm → plan 各占一层
        assert!(wave_of("explore") < wave_of("brainstorm"));
        assert!(wave_of("brainstorm") < wave_of("plan"));
        // 并行组同一层
        assert_eq!(wave_of("req_review"), wave_of("code_review"));
        assert!(wave_of("brainstorm") < wave_of("req_review"));
    }

    /// 验收：feature_design.toml 的 design step 必须含有 output_key="design"。
    /// 使用 include_str! 直接引用真文件，确保 TOML 编辑后测试立即红。
    #[test]
    fn feature_design_design_step_has_output_key() {        let raw = include_str!("../../config/workflows/feature_design.toml");
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
mod dag_tests {
    use super::*;

    fn wf_from(raw: &str) -> WorkflowDef {
        toml::from_str(raw).expect("valid TOML")
    }

    /// No `depends_on` anywhere → serial mode (DAG scheduler off).
    #[test]
    fn no_depends_on_is_not_dag() {
        let wf = wf_from(
            r#"
name = "serial"
[[steps]]
id = "a"
role = "pm"
task = "t"
[[steps]]
id = "b"
role = "architect"
task = "t"
"#,
        );
        assert!(!wf.uses_dependency_dag());
        // Serial mode = single wave per file order handled elsewhere;
        // compute_waves on no-deps puts everything in wave 0.
        let waves = compute_waves(&wf.steps).unwrap();
        assert_eq!(waves, vec![vec![0, 1]]);
    }

    /// A step declaring `depends_on` flips the workflow into DAG mode.
    #[test]
    fn any_depends_on_enables_dag() {
        let wf = wf_from(
            r#"
name = "dag"
[[steps]]
id = "a"
role = "pm"
task = "t"
[[steps]]
id = "b"
role = "architect"
task = "t"
depends_on = ["a"]
"#,
        );
        assert!(wf.uses_dependency_dag());
        wf.validate_dag().expect("valid dag");
    }

    /// Linear chain a→b→c produces one step per wave (fully serial).
    #[test]
    fn linear_chain_is_serial_waves() {
        let wf = wf_from(
            r#"
name = "chain"
[[steps]]
id = "a"
role = "pm"
task = "t"
[[steps]]
id = "b"
role = "architect"
task = "t"
depends_on = ["a"]
[[steps]]
id = "c"
role = "reviewer"
task = "t"
depends_on = ["b"]
"#,
        );
        let waves = compute_waves(&wf.steps).unwrap();
        assert_eq!(waves, vec![vec![0], vec![1], vec![2]]);
    }

    /// Fan-out: b and c both depend on a → they share wave 1 (concurrent).
    #[test]
    fn fan_out_shares_a_wave() {
        let wf = wf_from(
            r#"
name = "fanout"
[[steps]]
id = "a"
role = "pm"
task = "t"
[[steps]]
id = "b"
role = "architect"
task = "t"
depends_on = ["a"]
[[steps]]
id = "c"
role = "reviewer"
task = "t"
depends_on = ["a"]
"#,
        );
        let waves = compute_waves(&wf.steps).unwrap();
        assert_eq!(waves, vec![vec![0], vec![1, 2]]);
    }

    /// Fan-in: d waits for both b and c (from parallel wave) → own wave.
    #[test]
    fn fan_in_waits_for_all_deps() {
        let wf = wf_from(
            r#"
name = "diamond"
[[steps]]
id = "a"
role = "pm"
task = "t"
[[steps]]
id = "b"
role = "architect"
task = "t"
depends_on = ["a"]
[[steps]]
id = "c"
role = "reviewer"
task = "t"
depends_on = ["a"]
[[steps]]
id = "d"
role = "programmer"
task = "t"
depends_on = ["b", "c"]
"#,
        );
        let waves = compute_waves(&wf.steps).unwrap();
        assert_eq!(waves, vec![vec![0], vec![1, 2], vec![3]]);
    }

    #[test]
    fn cycle_is_rejected() {
        let wf = wf_from(
            r#"
name = "cyclic"
[[steps]]
id = "a"
role = "pm"
task = "t"
depends_on = ["b"]
[[steps]]
id = "b"
role = "architect"
task = "t"
depends_on = ["a"]
"#,
        );
        let err = wf.validate_dag().expect_err("cycle must be rejected");
        assert!(err.contains("cycle"), "got: {err}");
    }

    #[test]
    fn unknown_dependency_is_rejected() {
        let wf = wf_from(
            r#"
name = "bad-dep"
[[steps]]
id = "a"
role = "pm"
task = "t"
depends_on = ["ghost"]
"#,
        );
        let err = wf.validate_dag().expect_err("unknown dep must be rejected");
        assert!(err.contains("ghost"), "got: {err}");
    }

    #[test]
    fn self_dependency_is_rejected() {
        let wf = wf_from(
            r#"
name = "self"
[[steps]]
id = "a"
role = "pm"
task = "t"
depends_on = ["a"]
"#,
        );
        let err = wf.validate_dag().expect_err("self dep must be rejected");
        assert!(err.contains("itself"), "got: {err}");
    }

    #[test]
    fn duplicate_step_id_is_rejected() {
        let wf = wf_from(
            r#"
name = "dup"
[[steps]]
id = "a"
role = "pm"
task = "t"
[[steps]]
id = "a"
role = "architect"
task = "t"
depends_on = []
"#,
        );
        // Force DAG validation regardless of depends_on presence.
        let err = wf.validate_dag().expect_err("duplicate id must be rejected");
        assert!(err.contains("duplicate"), "got: {err}");
    }

    #[test]
    fn concurrency_env_override() {
        // Default when unset/invalid.
        std::env::remove_var("LATTE_WORKFLOW_CONCURRENCY");
        assert_eq!(workflow_concurrency(), 4);
        std::env::set_var("LATTE_WORKFLOW_CONCURRENCY", "7");
        assert_eq!(workflow_concurrency(), 7);
        std::env::set_var("LATTE_WORKFLOW_CONCURRENCY", "0");
        assert_eq!(workflow_concurrency(), 4, "0 falls back to default");
        std::env::remove_var("LATTE_WORKFLOW_CONCURRENCY");
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
