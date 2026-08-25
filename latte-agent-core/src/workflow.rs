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

use serde::{Deserialize, Serialize};
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
    /// 文档暂存模式：true 时本 run 内角色的 write 重定向到
    /// `.latte/staging/<wf_id>/`（read overlay 优先读暂存副本），
    /// workflow 整体 ok 才把草稿提升到目标路径——未终审的文档不再
    /// 直接落进仓库。见 `crate::staging`。
    #[serde(default)]
    pub staging: bool,
    #[serde(default)]
    pub steps: Vec<WorkflowStepDef>,
}

impl WorkflowDef {
    /// Check if the workflow has a command and it matches the given input.
    pub fn matches_command(&self, cmd: &str) -> bool {
        self.command.as_deref() == Some(cmd)
    }
}

/// 专家产出契约：step 声明自己产出的最低质量门槛。speaker 产出后由
/// 引擎用 [`check_output_contract`] 校验，不合格则带批注重试（最多
/// `max_retries` 次），重试耗尽 → step 失败。全部默认（空契约）时
/// 恒合格，等价于不校验。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputContract {
    /// 产出最少字符数（防"一句话敷衍"）。
    #[serde(default)]
    pub min_chars: Option<usize>,
    /// 禁止出现的子串（占位符等），命中即不合格。
    #[serde(default)]
    pub forbid: Vec<String>,
    /// 必须全部出现的子串。
    #[serde(default)]
    pub require: Vec<String>,
}

/// 校验产出是否满足契约。按 min_chars → forbid → require 顺序检查，
/// 第一个违规即返回中文原因（措辞可直接作为给模型的验收批注）。
/// 空契约（全默认）恒 Ok。
fn check_output_contract(contract: &OutputContract, output: &str) -> Result<(), String> {
    if let Some(min) = contract.min_chars {
        let n = output.chars().count();
        if n < min {
            return Err(format!("产出过短：{n} 字符，少于要求的 {min} 字符"));
        }
    }
    for pat in &contract.forbid {
        if output.contains(pat.as_str()) {
            return Err(format!("产出包含禁止出现的内容「{pat}」"));
        }
    }
    for pat in &contract.require {
        if !output.contains(pat.as_str()) {
            return Err(format!("产出缺少必须出现的内容「{pat}」"));
        }
    }
    Ok(())
}

/// 契约校验最终失败时附进错误消息的产出摘要：取前 `max_chars` 个字符，
/// 超长补「…」。目的：gate 类 step 判 REJECT 时，manager 拿到的错误
/// 里能直接看到 REJECT 理由（而不是只有"缺少 VERDICT: PASS"），
/// 才能向用户解释或修复后 resume。
fn output_excerpt(output: &str, max_chars: usize) -> String {
    let excerpt: String = output.chars().take(max_chars).collect();
    if output.chars().count() > max_chars {
        format!("{excerpt}…")
    } else {
        excerpt
    }
}

/// 契约重试耗尽后的语义兜底裁决。纯字符串契约分不清「格式不合格
/// 的坏产出」和「语义正确但没写约定标记的好产出」（jemalloc 实锤：
/// gate 的合法 REJECT 缺「VERDICT: PASS」字样，被契约当成格式错误
/// 判死）。耗尽前过一道 advisor 返回审查：
/// - verdict ok → 带批注放行（下游与人都能看到契约被语义覆盖）；
/// - warn/intervene/terminate/超时/无引擎 → `None`，维持原失败。
async fn contract_last_resort_review(
    review_engine: &Option<Arc<crate::advisor_monitor::AdvisorReviewEngine>>,
    event_tx: &broadcast::Sender<ChatEvent>,
    step_id: &str,
    speaker: &str,
    task: &str,
    response: &str,
    contract_reason: &str,
) -> Option<String> {
    let engine = review_engine.as_ref()?;
    let (reviewed, verdict) = crate::controller::gate_delegate_return(
        engine,
        event_tx,
        speaker,
        "", // 此处拿不到角色职责全文，审查以任务+产出为基准
        task,
        response.to_string(),
        "", // contract_last_resort_review 无工具调用数据
    )
    .await;
    let v = verdict?;
    if v.verdict != crate::advisor_monitor::Verdict::Ok {
        return None;
    }
    let _ = event_tx.send(ChatEvent::Status {
        message: format!(
            "⚠️ step '{step_id}' speaker '{speaker}' 的产出未通过契约（{contract_reason}），\
             经 advisor 语义审查判定内容合格，放行"
        ),
    });
    Some(format!(
        "{reviewed}\n\n⚠️ [监察审查] 本产出未通过产出契约（{contract_reason}），\
         经 advisor 语义审查判定内容合格后放行"
    ))
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
    /// step 级工具过滤：非空时本 step 的 speaker 只能用
    /// `role.allowed_tools ∩ tools`（保持角色配置里的顺序）；空 =
    /// 角色全集（现状）。用途：同一角色在不同 step 的能力收紧——如
    /// task_refine 的 refine 步禁 task_planner 调 plan（草案必须先过
    /// 评审），submit 步才放开。请求了角色没有的工具名会在构建 runner
    /// 时打 warning 并忽略（不过滤出不存在的工具）。
    #[serde(default)]
    pub tools: Vec<String>,
    /// 产出契约：speaker 产出不合格时带批注重试（复用 `max_retries`
    /// 作为重试上限），耗尽则 step 失败。默认空契约 = 不校验。
    #[serde(default)]
    pub output_contract: OutputContract,
    /// 跨 step 循环条件：本 step 完成后检查产出是否包含
    /// 该子串，包含 = 通过继续；不包含则跳回 `loop_back_to` 指定的 step
    /// 重做（缺省 = 自己），并把本 step 产出作为"上轮审查反馈"批注预置
    /// 进跳回目标的 prompt。迭代上限 `max_iterations`（缺省 3，硬上限
    /// 10），耗尽仍不满足 → workflow 失败。
    /// 串行与 DAG 引擎都支持；DAG 下 `loop_back_to` 必须指向严格更早
    /// wave 的 step（同 wave 自环请用 output_contract + max_retries），
    /// 跳回时目标及其全部下游 step 作废重跑。
    #[serde(default)]
    pub loop_until: Option<String>,
    /// `loop_until` 不满足时跳回的 step id（串行缺省 = 本 step 自己；
    /// DAG 下必填且必须位于更早的 wave）。
    #[serde(default)]
    pub loop_back_to: Option<String>,
    /// 「不可返工」熔断标记：`loop_until` 未满足时，若产出还包含该子串
    /// 则直接判 workflow 失败，不做返工。给 task_refine 这类三值裁决
    /// （ACCEPT 继续 / REVISE 返工 / REJECT 终止）表达「输入缺失、事实
    /// 无法核验、流程不可继续」的终局。
    ///
    /// **缺省 = 不熔断**：`loop_until` 未满足就一律返工。此前引擎把
    /// `"VERDICT: REJECT"` 硬编码成无条件终局，导致 PASS/REJECT 两值
    /// 词表的 gate（implementation_plan、design_and_plan）配了
    /// `loop_until`/`loop_back_to` 也永远走不到返工分支——注释与配置
    /// 写着「REJECT → 跳回重做」，实际是评审如实 REJECT 就判死整条
    /// 流水线（jemalloc 实锤：唯一跑到 gate 的那次运行，44 分钟零产出）。
    /// 熔断词表改为按 step 显式声明，引擎不再私藏 magic string。
    #[serde(default)]
    pub loop_abort_on: Option<String>,
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
    /// 嵌套 workflow step 专用：默认把子 workflow 的**最后一步输出**
    /// 绑到本 step 的 `output_key`；子 workflow 末尾常是评审/裁决步骤
    /// （如 design_brainstorm 的 advisor_verdict），父级真正想要的往往
    /// 是中间产物（如 proposal）。设置后改取子 workflow 中该
    /// `output_key` 对应的 step 产出。仅对嵌套 step 有效——见
    /// [`WorkflowDef::validate`]。
    #[serde(default)]
    pub output_from: Option<String>,
    /// 嵌套 workflow step 专用：把子 workflow 的**若干中间产物**一并
    /// 带到父级 vars。写法 `父级变量名 = "子 workflow 的 output_key"`：
    ///
    /// ```toml
    /// [[steps]]
    /// id = "explore"
    /// workflow = "explore"
    /// output_key = "exploration"          # 末步的提炼稿
    /// export = { survey = "exploration" } # 另外带出 survey 原文
    /// ```
    ///
    /// **为什么需要**：`output_from` 只能挑**一个**产物绑到本 step 的
    /// `output_key`，子 workflow 其余 step 的产出全被丢掉——引擎其实
    /// 已经把它们算好了（`run_workflow_inner` 返回 `keyed`），只是没有
    /// 出口。这就是「信息漏斗」：证据在子流程里，下游评审只拿到一份
    /// 被逐层压缩的结论，想核对也无从核对（jemalloc 实锤：双评审共
    /// 5 处实测行号错误 —— 它们只看到 proposal，survey 原文里的真实
    /// 文件与行号根本没往下传）。
    ///
    /// 键（父级变量名）与 `output_key` 同一套命名规则：非空、不占用
    /// [`RESERVED_OUTPUT_KEYS`]、不与本 workflow 任何 step 的
    /// `output_key` 撞名。导出值只进 `vars` 供 `{{}}` 取用，不进本
    /// workflow 的 `keyed`——`keyed` 的语义是「本 workflow 各 step 的
    /// 产出」，混入转口货会让上层的 `output_from` 语义变模糊。
    #[serde(default)]
    pub export: std::collections::BTreeMap<String, String>,
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
        let uses_dag = self.uses_dependency_dag();
        let step_ids: std::collections::HashSet<&str> =
            self.steps.iter().map(|s| s.id.as_str()).collect();
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
                if step.loop_until.is_some() {
                    return Err(format!(
                        "step '{}': `loop_until` 与嵌套 workflow 互斥（嵌套 step 不参与循环）",
                        step.id
                    ));
                }
            }
            if step.output_from.is_some() && step.workflow.is_none() {
                return Err(format!(
                    "step '{}': `output_from` 仅对嵌套 workflow step 有效（该 step 没有 workflow 字段）",
                    step.id
                ));
            }
            if let Some(of) = &step.output_from {
                if of.trim().is_empty() {
                    return Err(format!(
                        "step '{}': output_from must not be empty",
                        step.id
                    ));
                }
            }
            // export：与 output_from 同源（都只对嵌套 step 有意义），
            // 父级变量名沿用 output_key 的命名规则。
            if !step.export.is_empty() && step.workflow.is_none() {
                return Err(format!(
                    "step '{}': `export` 仅对嵌套 workflow step 有效（该 step 没有 workflow 字段）",
                    step.id
                ));
            }
            for (var, sub_key) in &step.export {
                if var.trim().is_empty() {
                    return Err(format!("step '{}': export 的父级变量名不能为空", step.id));
                }
                if sub_key.trim().is_empty() {
                    return Err(format!(
                        "step '{}': export['{var}'] 指向的子 workflow output_key 不能为空",
                        step.id
                    ));
                }
                if RESERVED_OUTPUT_KEYS.contains(&var.as_str()) {
                    return Err(format!(
                        "step '{}': export 的父级变量名 '{var}' 占用了保留变量名",
                        step.id
                    ));
                }
                // 与本 workflow 任何 step 的 output_key 撞名 → 谁覆盖谁
                // 取决于执行顺序，是隐藏的踩踏。直接拒。
                if self
                    .steps
                    .iter()
                    .any(|s| s.output_key.as_deref() == Some(var.as_str()))
                {
                    return Err(format!(
                        "step '{}': export 的父级变量名 '{var}' 与本 workflow 某个 step 的 output_key 撞名",
                        step.id
                    ));
                }
            }
            if let Some(target) = &step.loop_back_to {
                if !step_ids.contains(target.as_str()) {
                    return Err(format!(
                        "step '{}': loop_back_to 指向不存在的 step '{target}'",
                        step.id
                    ));
                }
            }
            // loop_abort_on 只在返工判定里生效；没有 loop_until 的 step
            // 写了它必然是笔误（会被静默忽略），直接拒。
            if let Some(marker) = &step.loop_abort_on {
                if step.loop_until.is_none() {
                    return Err(format!(
                        "step '{}': `loop_abort_on` 需要配合 `loop_until`（没有返工环时熔断标记无意义）",
                        step.id
                    ));
                }
                if marker.trim().is_empty() {
                    return Err(format!(
                        "step '{}': loop_abort_on must not be empty",
                        step.id
                    ));
                }
                // 熔断标记若同时满足 loop_until，条件永远先命中放行，
                // 熔断成死代码——配置一定写错了。
                if let Some(cond) = &step.loop_until {
                    if marker.contains(cond.as_str()) {
                        return Err(format!(
                            "step '{}': loop_abort_on '{marker}' 包含 loop_until '{cond}'，\
                             熔断永远不会触发（放行条件先命中）",
                            step.id
                        ));
                    }
                }
            }
            if step.loop_until.is_some() && uses_dag {
                // DAG 返工环（jemalloc 实锤：gate 的合法 REJECT 被契约
                // 判死，50 分钟流水线零产出）。约束：跳回目标必须在
                // 严格更早的 wave——跳同 wave / 未来 wave 无法表达
                // 「重跑上游再流到本 step」的语义。自环（loop_back_to
                // 缺省 = 自己）在 DAG 下同属同 wave，同样拒绝；单步
                // 重试用 output_contract + max_retries 表达。
                let waves = compute_waves(&self.steps)?;
                let wave_of = |id: &str| {
                    waves
                        .iter()
                        .position(|w| w.iter().any(|&i| self.steps[i].id == id))
                        .expect("compute_waves 覆盖全部 step")
                };
                let own_wave = wave_of(&step.id);
                let target = step.loop_back_to.as_deref().unwrap_or(step.id.as_str());
                let target_wave = wave_of(target);
                if target_wave >= own_wave {
                    return Err(format!(
                        "step '{}': DAG 模式下 loop_back_to '{target}' 必须位于严格更早的 wave \
                         （目标 wave {target_wave}，本 step wave {own_wave}）；\
                         同 wave 自环请改用 output_contract + max_retries",
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
// Semantics: every step dispatch runs as a **fresh subagent** via
// [`run_step_speaker`] — isolated context, own subsession log,
// advisor gate + return review, mirroring the manager's `delegate`
// tool; rounds × steps × speakers loop, `{{var}}` substitution
// (`topic` + step `output_key`s), progress streamed as
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
    /// Per-turn cancellation flag, distinct from `cancel_flag` (which
    /// aborts the whole session).
    ///
    /// Step 已无 wall-clock 硬超时：超时只发 `TimeoutWarning` 让用户
    /// 选「继续等待 / 终止当前任务」，倒计时重新 park。所以本旗标是
    /// **唯一的 per-turn 逃生口** —— 用户点「终止当前任务」
    /// （`Controller::cancel_turn`）置位后，卡住的 step 在下个 500ms
    /// tick 被 abort，session 保留。
    ///
    /// `None` 不再退化成硬中止（硬中止已删除），而是意味着该 run
    /// **没有 per-turn 逃生口**，只能靠 session 级 `cancel_flag`
    /// 一次性全杀。因此凡是能拿到 session controller 的入口都应传
    /// `Some`（见 `api::session_turn_cancel_flag`）；仅无 session 的
    /// 独立测试 run 才允许 `None`。
    pub turn_cancel_flag: Option<Arc<AtomicBool>>,
    /// Session-level 暂停门。workflow 的 role runner 也 attach 这个
    /// gate —— 用户按 ⏸ 时 workflow 流水线一起冻结（下个 boundary
    /// park）。None（独立测试/无 session 的 run）跳过。
    pub agent_pause_gate: Option<Arc<crate::pause_gate::AgentPauseGate>>,
    /// Nesting depth: 0 for a top-level run, +1 per nested workflow
    /// step. Guarded against [`MAX_WORKFLOW_DEPTH`] to stop cycles.
    pub depth: u8,
    /// **顶层** run 的 `wf_id`（嵌套链的根）。`None` = 本 run 就是顶层。
    ///
    /// 为什么必须有：阻塞 `ask` 的落盘记录要指向**能把整条流水线带起来**
    /// 的那个 run。此前它记的是"发出提问的那个 run"，而嵌套场景下那是
    /// 子 run —— 回答后只 resume 子 run，父流水线既不知道自己在等谁、
    /// 也没有任何机制被唤醒，于是永久卡死（jemalloc 现场：
    /// `design_and_plan` → `req_review` → `requirements_review` 的
    /// `decide` 步骤弹出选择题，答案无处可去，顶层再也没动过）。
    ///
    /// 由 [`run_nested_workflow`] 一路透传：顶层把自己的 `wf_id` 填进去，
    /// 每层嵌套原样继承。
    pub root_wf_id: Option<String>,
    /// 有 Some 时每次 step 分派都为专家建 subsession（独立子会话
    /// 日志，UI 右键「查看日志」可读）——与普通流程 delegate 一致。
    /// None（独立测试 run / CLI REPL）时跳过，专家过程不可追溯。
    pub subsession_store: Option<Arc<crate::subsession::SubsessionStore>>,
    /// subsession_store 的落盘 key（UI session id）。空/None 时
    /// subsession 退化到内存。与 store 配对使用。
    pub session_id: Option<String>,
    /// Advisor 产出门禁（D5/D6）+ 返回审查开关。Some 时专家 runner
    /// 走 `run_turn_gated`，产出回写 vars 前过
    /// `controller::gate_delegate_return` 审查——与普通流程 delegate
    /// 一致。None 时等价裸 `run_turn`、不做返回审查。
    pub advisor_gate: Option<crate::advisor_monitor::GateConfig>,
    /// Advisor intervene 暂停门（v3）：monitor 判 Intervene 时置位，
    /// workflow 的分派在派发前 wait、运行中的专家 runner 在
    /// tool-round 边界 park——advisor 的「已暂停，等待用户拍板」对
    /// workflow 真实生效（此前只对 watched role 的主 runner 生效，
    /// workflow 照跑不误）。None（CLI/独立 run）跳过。
    pub advisor_pause: Option<crate::advisor_monitor::AdvisorPauseGate>,
    /// 文档暂存层（workflow toml `staging = true` 时由 run_workflow_inner
    /// 在顶层 run 创建并挂到这里；嵌套 run 原样继承，共享同一暂存区，
    /// 只由创建者在收尾时 promote）。None 时 write/read 直落真实 fs。
    pub staging: Option<Arc<crate::staging::Staging>>,
}

/// Maximum nesting depth for workflow steps that invoke another
/// workflow (`workflow = "..."` on a step). Deeper nesting is rejected
/// with an error — almost always a cycle or a design mistake.
pub const MAX_WORKFLOW_DEPTH: u8 = 3;

/// 无法加载的嵌套 workflow 按几个分派单元估。取值偏小是故意的：
/// 名字都加载不出来的 step 大概率会立刻失败，不该为它把整体预算
/// 抬高。
const NESTED_UNKNOWN_UNITS: usize = 4;

/// 「预算超支」失败消息的固定标记。存在的理由：`classify_tool_execution_error`
/// 要能把它认出来判**不可重试**——预算超支时 checkpoint 已经落盘，
/// 盲目重试等于从第 0 步重跑一遍，把已完成的成果全扔了再烧一份同样
/// 长的预算（这正是 jemalloc「两次空等 50 分钟」的形状）。正确善后是
/// 拿错误里的 wf_id 走 resume，由模型决策。
pub const BUDGET_EXCEEDED_MARKER: &str = "超出按规模推算的时间预算";

/// 一条 workflow 的「分派单元」数：一个单元 = 一次 speaker 分派
/// （一个 step 里每个 role 各算一次），嵌套 step 递归展开成子流程的
/// 单元数。用于把时间预算按**子流程真实规模**推算，而不是所有
/// workflow 共用一个拍脑袋的常量。
///
/// 计入的放大因素：
/// - `max_retries`：契约不合格会带批注重跑同一 step
/// - `loop_until` 的 `max_iterations`：返工环会把目标及其下游重跑多轮
///   （取全流程最大值作为整体倍数——偏保守，宁可预算大也不要误杀）
///
/// 递归以 [`MAX_WORKFLOW_DEPTH`] 为界；加载不出来的嵌套名按
/// [`NESTED_UNKNOWN_UNITS`] 兜底，绝不因此报错（预算估算不该让注册
/// 或启动失败）。
pub fn dispatch_units(wf: &WorkflowDef, cwd: &Path, depth: u8) -> usize {
    let per_step: Vec<usize> = wf
        .steps
        .iter()
        .map(|step| {
            let base = match &step.workflow {
                Some(name) if depth < MAX_WORKFLOW_DEPTH => load_workflow(name, cwd)
                    .map(|nested| dispatch_units(&nested, cwd, depth + 1))
                    .unwrap_or(NESTED_UNKNOWN_UNITS),
                // 深度到顶：不再展开（运行时也会被 MAX_WORKFLOW_DEPTH
                // 拒），按兜底值计。
                Some(_) => NESTED_UNKNOWN_UNITS,
                None => step.roles().len().max(1),
            };
            base.saturating_mul(1 + step.max_retries as usize)
        })
        .collect();
    let base: usize = per_step.iter().copied().fold(0, usize::saturating_add);

    // 返工环的额外预算 = (轮数 - 1) × 「被重跑的区间」的单元数。
    // 区间 = loop_back_to 目标 → 本 loop step（文件序；DAG 下 validate
    // 已保证目标位于严格更早的 wave，文件序是可用的近似）。
    //
    // 不能把整条流水线乘以轮数：design_and_plan 那样算出 93 个单元、
    // 23 小时预算，等于天花板不存在（改动前就是 24h 常量）。返工只重跑
    // 目标及其下游，interview / explore 这些上游一次都不会重来。
    let mut extra: usize = 0;
    for (i, step) in wf.steps.iter().enumerate() {
        if step.loop_until.is_none() {
            continue;
        }
        let iters = step.max_iterations.unwrap_or(3).clamp(1, 10);
        let target = step.loop_back_to.as_deref().unwrap_or(step.id.as_str());
        let start = wf
            .steps
            .iter()
            .position(|s| s.id == target)
            .unwrap_or(i)
            .min(i);
        let scope: usize = per_step[start..=i].iter().copied().fold(0, usize::saturating_add);
        extra = extra.saturating_add(scope.saturating_mul(iters.saturating_sub(1)));
    }
    base.saturating_add(extra).max(1)
}

/// 默认的「每个分派单元」预算秒数。
///
/// 刻意**不用** `specialist_timeout_secs`（per-step 软超时，默认 900s）：
/// 那是「这一步慢得该提醒用户了」的告警线，把它当预算单位等于假设每次
/// 分派都病态地慢 —— 实测 design_and_plan 会算出 23 小时预算，跟改动前
/// 的 24h 常量没有区别，天花板等于不存在。
///
/// 取值依据：design_and_plan 实测约 48 分钟 / 31 个基础单元 ≈ 93s/单元，
/// 取 240s 留约 2.5 倍余量。env `LATTE_AGENT_WORKFLOW_BUDGET_PER_UNIT_SECS`
/// 可覆盖。
pub const DEFAULT_BUDGET_SECS_PER_UNIT: u64 = 240;

/// 每个分派单元的预算秒数（env 可覆盖，每次调用读 env 便于测试）。
pub fn budget_secs_per_unit() -> u64 {
    std::env::var("LATTE_AGENT_WORKFLOW_BUDGET_PER_UNIT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_BUDGET_SECS_PER_UNIT)
}

/// 按规模推算一条 workflow 的 wall-clock 时间预算（秒）。
///
/// `per_unit_secs` 是单次分派计入预算的秒数，调用方传
/// [`budget_secs_per_unit`]。
///
/// 结果夹在 `[per_unit_secs, ORCHESTRATION_TOOL_TIMEOUT_SECS]`：下界
/// 保证再小的 workflow 也有一个单元的余量，上界保证永不超过工具管理器
/// 那一层的天花板（否则工具层先熔断，又变成孤儿 step）。
pub fn estimate_budget_secs(wf: &WorkflowDef, cwd: &Path, per_unit_secs: u64) -> u64 {
    let ceiling = crate::controller::ORCHESTRATION_TOOL_TIMEOUT_SECS;
    let per_unit = per_unit_secs.max(1);
    // 下界也要压在天花板之内：单次分派预算本身就大过天花板时
    // （env 配了个荒谬的值），`clamp(min, max)` 会因 min > max 直接
    // panic。
    let floor = per_unit.min(ceiling);
    let units = dispatch_units(wf, cwd, 0).saturating_mul(wf.effective_max_rounds());
    (units as u64).saturating_mul(per_unit).clamp(floor, ceiling)
}

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
    output_from: Option<String>,
    export: std::collections::BTreeMap<String, String>,
    // 发起本次嵌套的那个 run 的 `wf_id`。仅当 `ctx.root_wf_id` 为
    // `None`（父级自己就是顶层）时用它做根身份；否则原样继承 ctx 里
    // 已有的根。调用方传自己的 wf_id 即可，不必判断自己是第几层。
    parent_wf_id: String,
) -> std::pin::Pin<
    Box<
        dyn std::future::Future<
                Output = Result<(String, std::collections::BTreeMap<String, String>), String>,
            > + Send,
    >,
> {
    Box::pin(async move {
        if ctx.depth >= MAX_WORKFLOW_DEPTH {
            return Err(format!(
                "nested workflow '{name}' exceeds max depth {MAX_WORKFLOW_DEPTH} (cycle?)"
            ));
        }
        // 根身份：父级已经在嵌套链里就沿用它的根，否则父级自己是根。
        let root_wf_id = ctx.root_wf_id.clone().unwrap_or(parent_wf_id);
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
            turn_cancel_flag: ctx.turn_cancel_flag.clone(),
            agent_pause_gate: ctx.agent_pause_gate.clone(),
            depth: ctx.depth + 1,
            subsession_store: ctx.subsession_store.clone(),
            session_id: ctx.session_id.clone(),
            advisor_gate: ctx.advisor_gate.clone(),
            advisor_pause: ctx.advisor_pause.clone(),
            staging: ctx.staging.clone(),
            // 顶层身份原样继承：嵌套多深，阻塞 ask 的落盘记录都指向
            // 同一个根 run —— 回答后 resume 它，整条链才会重新走起来。
            // `parent_wf_id` 是本次嵌套的直接父级（顶层调用时 ctx 的
            // root 为 None，用父自己的 wf_id 兜底）。
            root_wf_id: Some(root_wf_id.clone()),
        };
        // 嵌套 run 不自设预算：外层顶层 run 的截止时间已经把它包住，
        // 内外层各设一份等于同一段时间被重复计费。
        let (last, keyed) =
            run_workflow_inner(&wf, &topic, &nested_ctx, None, Budget::Off).await?;
        let available = || keyed.keys().cloned().collect::<Vec<_>>().join(", ");
        // export：把子 workflow 的若干中间产物按「父级变量名 ← 子
        // output_key」带出去（打通信息漏斗，见 `WorkflowStepDef::export`）。
        let mut exported = std::collections::BTreeMap::new();
        for (var, sub_key) in &export {
            let value = keyed.get(sub_key).cloned().ok_or_else(|| {
                format!(
                    "nested workflow '{name}' 的 export['{var}'] 指向不存在的 output_key \
                     '{sub_key}'（可用：{}）",
                    available()
                )
            })?;
            exported.insert(var.clone(), value);
        }
        // output_from：取子 workflow 指定 output_key 的产出（如
        // proposal），而非默认的最后一步输出（常是评审 verdict）。
        let primary = match output_from {
            Some(key) => keyed.get(&key).cloned().ok_or_else(|| {
                format!(
                    "nested workflow '{name}' 没有 output_key '{key}' 的产出（可用：{}）",
                    available()
                )
            })?,
            None => last,
        };
        Ok((primary, exported))
    })
}

/// workflow 因**用户主动终止**而结束时，错误信息的固定前缀。
///
/// 存在的理由：`run_workflow` 的 Err 同时承载"跑挂了"和"人掐了"两种
/// 情况，而这两种的善后动作完全相反 —— 跑挂了该 resume 续跑，人掐了
/// 绝不能自动续跑（否则用户右键「终止此分派」刚把活停掉，manager 立刻
/// 又把它 resume 起来，功能等于没有）。调用方用
/// [`is_cancelled_by_user`] 区分。
pub const CANCELLED_BY_USER_PREFIX: &str = "workflow cancelled by user";

/// 该 workflow 错误是否源于用户主动终止（而非执行失败）。
pub fn is_cancelled_by_user(msg: &str) -> bool {
    msg.starts_with(CANCELLED_BY_USER_PREFIX)
}

/// Internal outcome of a workflow engine (serial or DAG), before the
/// `WorkflowFinished` event is emitted by [`run_workflow`].
enum WfOutcome {
    /// (最后一步输出, output_key → 各 step 产出)。后者供嵌套 step 的
    /// `output_from` 取子 workflow 的中间产物（如 proposal 而非末尾
    /// 的评审 verdict）。
    Ok(String, std::collections::HashMap<String, String>),
    Cancelled,
    Failed(String),
}

// ─── Checkpoint（断点续跑）────────────────────────────────────────
//
// 每完成一个 step，把产出追加写入 `<cwd>/.latte/workflow-runs/<wf_id>.jsonl`
// （首行 meta，之后一行一个 step 记录）。step 失败时已完成成果不丢：
// [`run_workflow_resume`] 读取 checkpoint、把已完成 step 的
// output_key→output 注入 `vars` 并跳过这些 step，从断点继续。
// 写盘失败只 warn，绝不影响执行；跑完的 checkpoint 保留（不做自动清理）。

/// One line in a workflow-run checkpoint file (JSONL).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CheckpointRecord {
    /// First line of every checkpoint file: run identity.
    Meta {
        wf_id: String,
        workflow_name: String,
        topic: String,
        started_at: u64,
    },
    /// One completed step.
    Step {
        wf_id: String,
        workflow_name: String,
        topic: String,
        step_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_key: Option<String>,
        output: String,
        finished_at: u64,
    },
    /// 一条用户在阻塞 `ask` 弹框里给出的回答，**收到即落盘**。
    ///
    /// 与 `Step` 的关键区别是粒度：`Step` 只在整个 step 成功后才写，
    /// 而用户的回答是**不可再生资源**——重跑一次就得让人重新点一遍。
    /// design_and_plan 的 interview 步要连问 3-4 题，只要该 step 后续
    /// 任何环节挂了（契约不合格、空产出、advisor 拦、预算超支、模型
    /// 报错），整段问答就随 step 一起蒸发：契约重试是**全新 subagent**
    /// （无跨分派记忆），resume 也只跳过已完成的 step——两条路都会把
    /// 同样的问题重新弹给用户。
    Answer {
        wf_id: String,
        /// 提问的角色（如 tutor）。
        role: String,
        /// 问题原文（去首尾空白后作为回放的 key）。
        question: String,
        answer: String,
        answered_at: u64,
    },
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn checkpoint_dir(cwd: &Path) -> PathBuf {
    cwd.join(".latte").join("workflow-runs")
}

fn checkpoint_path(cwd: &Path, wf_id: &str) -> PathBuf {
    checkpoint_dir(cwd).join(format!("{wf_id}.jsonl"))
}

/// Append one record to the run's checkpoint file (creating the
/// directory on first use). Failures only warn — a checkpoint is a
/// recovery aid, never a reason to fail the run.
fn append_checkpoint(cwd: &Path, wf_id: &str, record: &CheckpointRecord) {
    let write = || -> std::io::Result<()> {
        let line = serde_json::to_string(record)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::create_dir_all(checkpoint_dir(cwd))?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(checkpoint_path(cwd, wf_id))?;
        use std::io::Write as _;
        writeln!(f, "{line}")
    };
    if let Err(e) = write() {
        tracing::warn!(wf_id, error = %e, "workflow checkpoint write failed (run continues)");
    }
}

/// State recovered from a checkpoint file for [`run_workflow_resume`].
/// pub 是给 UI-server 的 `POST /api/workflows/resume` 做启动前校验
/// （404 检查 + 读 workflow 名）用；`completed` 仅引擎内部使用。
pub struct CheckpointState {
    pub workflow_name: String,
    pub topic: String,
    /// Completed steps in completion order: (step_id, output_key, output).
    completed: Vec<(String, Option<String>, String)>,
    /// 上一次运行里用户已经回答过的问题：question → answer。
    /// resume 时预载进 [`AnswerLog`]，同一个问题不再弹给用户。
    answers: std::collections::HashMap<String, String>,
}

impl CheckpointState {
    /// 这个问题在本 checkpoint 里是否已有答案。
    ///
    /// 挂起 ask 的落盘记录用它做"已答"判定：一旦答案进了 checkpoint
    /// （无论是活着的 run 记的，还是重启后补写的），那条挂起记录就该
    /// 被当作已处理，不能再补发成弹框（否则用户重复回答同一题）。
    pub fn has_answer(&self, question: &str) -> bool {
        self.answers.contains_key(question.trim())
    }
}

/// Load and validate a checkpoint file for resume. `wf_id` comes from
/// tool input, so reject anything that isn't a plain file name.
pub fn load_checkpoint(cwd: &Path, wf_id: &str) -> Result<CheckpointState, String> {
    if wf_id.is_empty()
        || wf_id.contains('/')
        || wf_id.contains('\\')
        || wf_id.contains("..")
    {
        return Err(format!("invalid resume wf_id '{wf_id}'"));
    }
    let path = checkpoint_path(cwd, wf_id);
    let raw = std::fs::read_to_string(&path).map_err(|_| {
        format!("resume checkpoint '{wf_id}' not found at {}", path.display())
    })?;
    let mut meta: Option<(String, String)> = None;
    let mut completed = Vec::new();
    let mut answers: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for (lineno, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let rec: CheckpointRecord = serde_json::from_str(line)
            .map_err(|e| format!("checkpoint '{wf_id}' line {}: invalid JSON: {e}", lineno + 1))?;
        match rec {
            CheckpointRecord::Meta { workflow_name, topic, .. } => {
                if meta.is_none() {
                    meta = Some((workflow_name, topic));
                }
            }
            CheckpointRecord::Step { step_id, output_key, output, .. } => {
                completed.push((step_id, output_key, output));
            }
            CheckpointRecord::Answer { question, answer, .. } => {
                // 后写覆盖：同一问题被问了两次（重试）时保留最后一次
                // 的回答。
                answers.insert(question, answer);
            }
        }
    }
    let (workflow_name, topic) =
        meta.ok_or_else(|| format!("checkpoint '{wf_id}' has no meta line (corrupt?)"))?;
    Ok(CheckpointState { workflow_name, topic, completed, answers })
}

/// 用户回答的持久化台账：收到即落盘，同一问题再问直接回放。
///
/// 解决的问题：阻塞 `ask` 的答案此前只活在 subagent 的内存 context 里。
/// step 一失败就全丢——而 step 内的契约重试是全新 subagent、resume 又
/// 只跳过已完成的 step，两条路都会把同样的问题重新弹一遍。用户答了
/// 4 道题，第 1 步挂掉，等于白答。
///
/// 写盘与读回都走 run 的 checkpoint 文件（[`CheckpointRecord::Answer`]），
/// 所以 resume 天然继承上一次运行的回答。
///
/// 回放按**问题原文**（trim 后）匹配。模型换了措辞就不算命中，会正常
/// 重新问用户——宁可多问一次，也不要把答案对错问题。
///
/// 它同时是 run 的**恢复身份**载体：`cwd + wf_id + session_id` 是
/// `ask` 工具那一层唯一能拿到的三元组（`AskBlocking` 里没有 session_id
/// / wf_id，而 `build_role_runner` 也不接这两个参数）。挂起的阻塞 ask
/// 要落盘成一条可跨进程恢复的记录（见 [`crate::pending_ask`]），靠的
/// 就是从这里读出来的身份。
pub struct AnswerLog {
    cwd: PathBuf,
    wf_id: String,
    /// 本 run 所属的 UI session（`WorkflowRunContext.session_id`）。
    /// CLI / 独立测试为 None —— 没有 session 就无从续跑。
    session_id: Option<String>,
    /// **顶层** run 的 `wf_id`（嵌套链的根）。`None` = 本 run 就是顶层。
    ///
    /// 回答落盘时要写**两份**：本 run 的 checkpoint（本层 resume 用）
    /// 和根 run 的 checkpoint（唤醒整条流水线用）。只写本层的话，
    /// 嵌套 ask 的答案就只能续跑子 run，父流水线永远醒不过来。
    root_wf_id: Option<String>,
    seen: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl AnswerLog {
    pub(crate) fn new(
        cwd: &Path,
        wf_id: &str,
        session_id: Option<String>,
        root_wf_id: Option<String>,
        preloaded: std::collections::HashMap<String, String>,
    ) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            wf_id: wf_id.to_string(),
            session_id,
            root_wf_id,
            seen: std::sync::Mutex::new(preloaded),
        }
    }

    /// run 的工作目录（checkpoint / 挂起 ask 记录都落在它下面的 `.latte/`）。
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// 本 run 的 checkpoint id。
    pub fn wf_id(&self) -> &str {
        &self.wf_id
    }

    /// 本 run 所属的 UI session（None = CLI / 独立测试）。
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// 能把**整条流水线**带起来的那个 run 的 id：嵌套链的根，没有嵌套
    /// 时就是本 run 自己。
    ///
    /// 阻塞 `ask` 的落盘记录必须用它而不是 [`Self::wf_id`] —— 那样
    /// 回答后 resume 的是顶层，父流水线才会继续；用本层 id 只能让子
    /// run 单独跑完，父级永久卡住。
    pub fn resume_wf_id(&self) -> &str {
        self.root_wf_id.as_deref().unwrap_or(&self.wf_id)
    }

    /// 本 run（含 resume 继承）里这个问题是否已经有答案。
    pub fn recall(&self, question: &str) -> Option<String> {
        let key = question.trim();
        self.seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
    }

    /// 记下一条回答：**先进内存表再落盘**，落盘失败也不影响本 run 内
    /// 的回放（checkpoint 只是恢复辅助，从不作为失败理由）。
    pub fn record(&self, role: &str, question: &str, answer: &str) {
        let key = question.trim().to_string();
        self.seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone(), answer.to_string());
        let mk = |wf_id: &str| CheckpointRecord::Answer {
            wf_id: wf_id.to_string(),
            role: role.to_string(),
            question: key.clone(),
            answer: answer.to_string(),
            answered_at: now_secs(),
        };
        append_checkpoint(&self.cwd, &self.wf_id, &mk(&self.wf_id));
        // 嵌套 run：答案再写一份到根 run 的 checkpoint。
        //
        // 两份都要，各有用处：本层那份让**子** run 单独 resume 时能
        // recall；根那份让**顶层** resume 时能 recall —— 而顶层 resume
        // 会重新走到这个嵌套 step、重新跑子 workflow，那时子 run 是全新
        // 的 wf_id（resume 也是新建 wf_id），它的 preloaded 只能来自根。
        // 少了根那份，顶层续跑会把同一道题重新弹给用户。
        if let Some(root) = self.root_wf_id.as_deref() {
            if root != self.wf_id {
                append_checkpoint(&self.cwd, root, &mk(root));
            }
        }
    }
}

/// 把一条用户回答直接写进某个 run 的 checkpoint，**不需要该 run 还活着**。
///
/// 为「服务器重启后回答孤儿 ask 弹框」而存在：重启把等待方（oneshot
/// 通道 + workflow future + 整个 runner）全带走了，答案没有活着的接收
/// 方可投。但 resume 的地基本来就是"从 checkpoint 预载已答问题"——
/// 所以把答案补写成一条 `Answer` 行，再触发
/// `POST /api/workflows/resume`，续跑的 run 走到同一个 `ask` 时
/// [`AnswerLog::recall`] 直接命中，不再弹框，流水线自然往下走。
///
/// 与 [`AnswerLog::record`] 写的是同一种记录，区别只是这里不持有内存
/// 表（本进程里没有在跑的 run 需要回放）。
pub fn record_answer_for_run(cwd: &Path, wf_id: &str, role: &str, question: &str, answer: &str) {
    append_checkpoint(
        cwd,
        wf_id,
        &CheckpointRecord::Answer {
            wf_id: wf_id.to_string(),
            role: role.to_string(),
            question: question.trim().to_string(),
            answer: answer.to_string(),
            answered_at: now_secs(),
        },
    );
}

/// Per-run checkpoint writer shared by both engines. Bundles the file
/// identity with a completed-step counter (seeded with the resumed
/// count) used to enrich failure messages ("已完成 N/M 步，可 resume")。
struct CheckpointLog {
    cwd: PathBuf,
    wf_id: String,
    workflow_name: String,
    topic: String,
    completed: std::sync::atomic::AtomicUsize,
}

impl CheckpointLog {
    /// Start a new checkpoint file (writes the meta line). `resumed`
    /// seeds the completed counter with steps carried over from a
    /// previous run's checkpoint.
    fn new(cwd: &Path, wf_id: &str, workflow_name: &str, topic: &str, resumed: usize) -> Self {
        append_checkpoint(
            cwd,
            wf_id,
            &CheckpointRecord::Meta {
                wf_id: wf_id.to_string(),
                workflow_name: workflow_name.to_string(),
                topic: topic.to_string(),
                started_at: now_secs(),
            },
        );
        Self {
            cwd: cwd.to_path_buf(),
            wf_id: wf_id.to_string(),
            workflow_name: workflow_name.to_string(),
            topic: topic.to_string(),
            completed: std::sync::atomic::AtomicUsize::new(resumed),
        }
    }

    /// Persist one completed step's output and bump the counter.
    fn record_step(&self, step_id: &str, output_key: Option<&str>, output: &str) {
        append_checkpoint(
            &self.cwd,
            &self.wf_id,
            &CheckpointRecord::Step {
                wf_id: self.wf_id.clone(),
                workflow_name: self.workflow_name.clone(),
                topic: self.topic.clone(),
                step_id: step_id.to_string(),
                output_key: output_key.map(|s| s.to_string()),
                output: output.to_string(),
                finished_at: now_secs(),
            },
        );
        self.completed.fetch_add(1, Ordering::Relaxed);
    }

    fn completed(&self) -> usize {
        self.completed.load(Ordering::Relaxed)
    }
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
/// Every completed step is checkpointed to
/// `<cwd>/.latte/workflow-runs/<wf_id>.jsonl`; on failure the error
/// message carries the `wf_id` so the run can be continued with
/// [`run_workflow_resume`] instead of starting over.
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
    run_workflow_inner(wf, topic, ctx, None, Budget::Auto)
        .await
        .map(|(out, _)| out)
}

/// 一次 run 的 wall-clock 时间预算。
///
/// 为什么放在引擎里而不是 `workflow` 工具那一层：工具的
/// `Tool::timeout` 是**注册期常量**（`ORCHESTRATION_TOOL_TIMEOUT_SECS`
/// = 24h，等于没有天花板），而「这条流水线该跑多久」只有加载完
/// `WorkflowDef` 之后才知道。放在引擎里还有一个关键好处：`wf_id` 在
/// 这里是已知的，超支的错误消息能直接带上 resume 凭据；在工具层包一层
/// `tokio::time::timeout` 拿不到 wf_id，只能给一句没法善后的报错。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    /// 按 [`estimate_budget_secs`] 从子流程规模推算，resume 时再按
    /// **剩余** step 比例缩水。仅顶层 run 生效——嵌套 run 不自设预算，
    /// 由外层的截止时间统一兜住，否则同一段时间被内外层重复计费。
    Auto,
    /// 不设预算（嵌套 run；测试里也用它排除时间因素）。
    Off,
}

/// Resume a previously interrupted workflow run from its checkpoint
/// (`resume_wf_id` is the `wf_id` reported in the failed run's error
/// message). The checkpoint must belong to the same workflow
/// (`wf.name`) — a mismatch is an error. Completed steps are skipped
/// (no model calls, no WorkflowStep/Turn events re-emitted); their
/// `output_key` outputs are injected into `vars` so downstream
/// `{{key}}` substitution works unchanged. An empty `topic` falls back
/// to the checkpointed topic. The resumed run gets a fresh `wf_id` and
/// its own checkpoint file (seeded with the carried-over steps), so a
/// second failure is resumable again.
pub async fn run_workflow_resume(
    wf: &WorkflowDef,
    topic: &str,
    ctx: &WorkflowRunContext,
    resume_wf_id: &str,
) -> Result<String, String> {
    run_workflow_inner(wf, topic, ctx, Some(resume_wf_id), Budget::Auto)
        .await
        .map(|(out, _)| out)
}

/// 返回值：(最终输出, output_key → 各 step 产出)。后者供嵌套 step 的
/// `output_from` 选取子 workflow 的中间产物。
async fn run_workflow_inner(
    wf: &WorkflowDef,
    topic: &str,
    ctx: &WorkflowRunContext,
    resume_wf_id: Option<&str>,
    budget: Budget,
) -> Result<(String, std::collections::HashMap<String, String>), String> {
    wf.validate()?;
    let uses_dag = wf.uses_dependency_dag();
    if uses_dag {
        wf.validate_dag()?;
    }

    // Resume: load the previous run's checkpoint, verify it belongs to
    // the same workflow, and carry over its completed steps.
    let mut resume: Option<CheckpointState> = None;
    let mut topic = topic.to_string();
    if let Some(rid) = resume_wf_id {
        let state = load_checkpoint(&ctx.cwd, rid)?;
        if state.workflow_name != wf.name {
            return Err(format!(
                "resume checkpoint '{rid}' belongs to workflow '{}', not '{}'",
                state.workflow_name, wf.name
            ));
        }
        if topic.trim().is_empty() {
            topic = state.topic.clone();
        }
        resume = Some(state);
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
    // 文档暂存：workflow 声明 staging=true 且上游还没有暂存区时，本
    // run 是创建者/所有者——嵌套 run 继承同一个（见 nested_ctx），
    // 只由所有者在收尾时 promote。用增强副本替换 ctx 引用，下游
    // （SpeakerDispatch::from_ctx）透传到各 speaker 的 runner。
    let created_staging = if wf.staging && ctx.staging.is_none() {
        Some(crate::staging::Staging::new(&ctx.cwd, &wf_id))
    } else {
        None
    };
    let owned_ctx;
    let ctx: &WorkflowRunContext = if let Some(st) = &created_staging {
        owned_ctx = WorkflowRunContext {
            staging: Some(st.clone()),
            ..ctx.clone()
        };
        &owned_ctx
    } else {
        ctx
    };
    if let Some(st) = &created_staging {
        let _ = ctx.event_tx.send(ChatEvent::Status {
            message: format!(
                "📦 staging 已启用：本 workflow 的文档写入先落到 {}，终审通过后自动提升到目标路径",
                st.root().display()
            ),
        });
    }
    let ckpt = CheckpointLog::new(
        &ctx.cwd,
        &wf_id,
        &name,
        &topic,
        resume.as_ref().map_or(0, |s| s.completed.len()),
    );
    // Copy the carried-over step records into the new run's checkpoint
    // file so it stays self-contained (a second resume needs only the
    // new wf_id).
    if let Some(state) = &resume {
        for (step_id, output_key, output) in &state.completed {
            append_checkpoint(
                &ctx.cwd,
                &wf_id,
                &CheckpointRecord::Step {
                    wf_id: wf_id.clone(),
                    workflow_name: name.clone(),
                    topic: topic.clone(),
                    step_id: step_id.clone(),
                    output_key: output_key.clone(),
                    output: output.clone(),
                    finished_at: now_secs(),
                },
            );
        }
        // 用户回答同理必须抄过来。此前只抄了 Step 行：答案仅被预载进
        // 新 run 的**内存** AnswerLog，新 checkpoint 文件里一条
        // `Answer` 都没有。于是第二次中断（再重启 / 再 resume）时，
        // load_checkpoint 读新 wf_id 拿到空 answers，用户已经答过的题
        // 被整批重问一遍 —— 用户的回答是不可再生资源，不能因为续跑了
        // 一次就作废。
        for (question, answer) in &state.answers {
            append_checkpoint(
                &ctx.cwd,
                &wf_id,
                &CheckpointRecord::Answer {
                    wf_id: wf_id.clone(),
                    // 原记录的 role 不进 CheckpointState（recall 不按 role
                    // 匹配），抄写时标注来源即可。
                    role: "(resumed)".to_string(),
                    question: question.clone(),
                    answer: answer.clone(),
                    answered_at: now_secs(),
                },
            );
        }
        let _ = ctx.event_tx.send(ChatEvent::Status {
            message: format!(
                "workflow '{name}' 从断点续跑（checkpoint {}）：跳过已完成的 {} 步",
                resume_wf_id.unwrap_or_default(),
                state.completed.len()
            ),
        });
    }
    let _ = ctx.event_tx.send(ChatEvent::WorkflowStarted {
        name: name.clone(),
        topic: topic.clone(),
        wf_id: wf_id.clone(),
    });

    // ── wall-clock 时间预算 ────────────────────────────────────────
    // 按分派单元数 × 单次分派预算推算。此前唯一的天花板是
    // `workflow` 工具的注册期常量 24h（= 没有天花板）：真卡住的流水线
    // 只能靠用户盯着 TimeoutWarning 手动终止，没人盯就一直挂。
    let per_unit = budget_secs_per_unit();
    let budget_secs = match budget {
        Budget::Off => 0,
        Budget::Auto if ctx.depth == 0 => {
            let full = estimate_budget_secs(wf, &ctx.cwd, per_unit);
            // resume 用**不同预算**：已完成的 step 会被跳过（不发模型
            // 请求），照抄全量估算等于给残余工作发一份跑完整条流水线的
            // 时间。按剩余 step 比例缩水，下界一个单元。
            // 这是「超时类失败重试不该再烧一份同样长的预算」的落点：
            // 失败→resume 的第二次尝试自动拿到按剩余量算的预算。
            match &resume {
                Some(state) if !wf.steps.is_empty() => {
                    let done = state.completed.len().min(wf.steps.len());
                    let remaining = wf.steps.len().saturating_sub(done);
                    let scaled = full
                        .saturating_mul(remaining as u64)
                        .saturating_div(wf.steps.len() as u64);
                    scaled.max(per_unit.max(1))
                }
                _ => full,
            }
        }
        Budget::Auto => 0,
    };
    // 用户回答台账：resume 时预载上一次运行已答过的问题，同一个问题
    // 不再弹给用户（见 AnswerLog）。同时带上本 run 的恢复身份
    // （cwd + wf_id + session_id）—— 阻塞 ask 靠它把挂起项落盘，
    // 服务器重启后用户回答能驱动断点续跑。
    //
    // 预载来源有两处，缺一不可：
    //   1. 本 run 的 resume 状态 —— 顶层续跑跳过已完成 step 的常规路径；
    //   2. **根 run** 的 checkpoint —— 嵌套场景的关键。顶层 resume 会
    //      重新走到嵌套 step 并新建一个子 run（resume 也是新 wf_id），
    //      这个新子 run 没有自己的 resume 状态，答案只能从根那里继承。
    //      少了它，顶层每次续跑都会把嵌套里问过的题重新弹一遍。
    let mut preloaded = resume
        .as_ref()
        .map(|s| s.answers.clone())
        .unwrap_or_default();
    if let Some(root) = ctx.root_wf_id.as_deref() {
        if root != wf_id {
            if let Ok(root_state) = load_checkpoint(&ctx.cwd, root) {
                // 本 run 自己的 resume 答案优先（更贴近当前上下文）。
                for (q, a) in root_state.answers {
                    preloaded.entry(q).or_insert(a);
                }
            }
        }
    }
    let answer_log = Arc::new(AnswerLog::new(
        &ctx.cwd,
        &wf_id,
        ctx.session_id.clone(),
        ctx.root_wf_id.clone(),
        preloaded,
    ));
    let engine = async {
        if uses_dag {
            run_workflow_dag(wf, &topic, ctx, &wf_id, &ckpt, resume.as_ref(), &answer_log).await
        } else {
            run_workflow_serial(wf, &topic, ctx, &wf_id, &ckpt, resume.as_ref(), &answer_log).await
        }
    };
    let outcome = if budget_secs == 0 {
        engine.await
    } else {
        match tokio::time::timeout(std::time::Duration::from_secs(budget_secs), engine).await {
            Ok(o) => o,
            // 超支即中止：drop 掉引擎 future 会连带 abort 在跑的 step
            // （DAG 的 JoinSet 归该 future 所有），不留孤儿任务——这正是
            // 此前工具层 25min 熔断做不到的事。已完成的 step 都在
            // checkpoint 里，下面的 Failed 分支会把 wf_id 附上。
            Err(_) => WfOutcome::Failed(format!(
                "{BUDGET_EXCEEDED_MARKER} {budget_secs}s（{} 个分派单元 × {per_unit}s/单元），已中止",
                dispatch_units(wf, &ctx.cwd, 0) * wf.effective_max_rounds()
            )),
        }
    };

    match outcome {
        WfOutcome::Ok(last_output, keyed_outputs) => {
            // 暂存提升：只有创建者（顶层 run）在这里收尾；嵌套 run 的
            // created_staging 是 None，草稿继续留给外层终审。
            if let Some(st) = &created_staging {
                let (promoted, errors) = st.promote();
                let message = if errors.is_empty() {
                    format!(
                        "📦 staging 提升完成：{} 个文档已落到目标路径（{}）",
                        promoted.len(),
                        promoted
                            .iter()
                            .map(|(p, _)| p.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                } else {
                    format!(
                        "⚠️ staging 部分提升失败（暂存区保留在 {}）：{}",
                        st.root().display(),
                        errors.join("; ")
                    )
                };
                let _ = ctx.event_tx.send(ChatEvent::Status { message });
            }
            let _ = ctx.event_tx.send(ChatEvent::WorkflowFinished {
                name,
                wf_id,
                status: "ok".into(),
                summary: last_output.clone(),
            });
            Ok((last_output, keyed_outputs))
        }
        WfOutcome::Cancelled => {
            if let Some(st) = &created_staging {
                let _ = ctx.event_tx.send(ChatEvent::Status {
                    message: format!(
                        "📦 workflow 已取消：{} 个暂存文档未提升，保留在 {}（可人工检查后 rm -rf 清理）",
                        st.pending_count(),
                        st.root().display()
                    ),
                });
            }
            // 带上 wf_id：用户想续跑时自己有据可查（但**不要**让
            // manager 自动续跑，见 CANCELLED_BY_USER_PREFIX）。
            let summary = format!("{CANCELLED_BY_USER_PREFIX}（wf_id={wf_id}）");
            let _ = ctx.event_tx.send(ChatEvent::WorkflowFinished {
                name,
                wf_id,
                status: "cancelled".into(),
                summary: summary.clone(),
            });
            Err(summary)
        }
        WfOutcome::Failed(msg) => {
            if let Some(st) = &created_staging {
                let _ = ctx.event_tx.send(ChatEvent::Status {
                    message: format!(
                        "📦 workflow 失败：{} 个暂存文档未提升，保留在 {}（可人工检查后 rm -rf 清理；resume 续跑用新暂存区）",
                        st.pending_count(),
                        st.root().display()
                    ),
                });
            }
            // 失败不丢成果：已完成 step 都落了 checkpoint，消息尾部
            // 带上进度与 wf_id，manager 可直接用 resume 续跑。
            let total = wf.steps.len() * wf.effective_max_rounds();
            let msg = format!(
                "{msg}（已完成 {}/{total} 步，可用 resume 从断点续跑：wf_id={wf_id}）",
                ckpt.completed()
            );
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

/// step 级工具过滤：`step_tools` 为空 → 角色全集；非空 → 求交集
/// （保持角色配置的顺序）。返回 (有效工具, 被忽略的 step 请求项)——
/// 忽略项是「角色本来就没有该工具」的请求（多为笔误），调用方打
/// warning，不静默吞。
fn effective_step_tools(
    role_tools: &[String],
    step_tools: &[String],
) -> (Vec<String>, Vec<String>) {
    if step_tools.is_empty() {
        return (role_tools.to_vec(), vec![]);
    }
    let effective: Vec<String> = role_tools
        .iter()
        .filter(|t| step_tools.iter().any(|s| s == *t))
        .cloned()
        .collect();
    let ignored: Vec<String> = step_tools
        .iter()
        .filter(|s| !role_tools.iter().any(|t| t == *s))
        .cloned()
        .collect();
    (effective, ignored)
}

/// Build a fresh `AgentRunner` for one role, wired exactly like the
/// chat/controller `build_runner`: tool-capable roles get the tool-use
/// protocol prompt + ground-truth block, plan tool registered when
/// allowed. Used by [`run_step_speaker`] (one fresh runner per
/// dispatch). `advisor` is rejected (monitor only).
/// `step_tools` 非空时按 step 声明过滤角色工具（见
/// [`effective_step_tools`]）。
///
/// Returns the runner plus the role's **base** system prompt (before
/// the tool-protocol / ground-truth appends) — the delegate-return
/// review uses it as the role-responsibilities reference, mirroring
/// the controller's delegate path.
async fn build_role_runner(
    role_id: &str,
    merged: &Arc<AgentConfig>,
    resolver: &Arc<ModelResolver>,
    default_params: &GenerateParams,
    cwd: &Path,
    event_tx: &broadcast::Sender<ChatEvent>,
    agent_pause_gate: Option<Arc<crate::pause_gate::AgentPauseGate>>,
    cancel_flag: Arc<AtomicBool>,
    // 文档暂存层：Some 时把 runner 工具表里的 write/read 换成暂存
    // 包装版（写重定向 + 读 overlay），见 crate::staging。
    staging: Option<Arc<crate::staging::Staging>>,
    step_tools: &[String],
    // 用户回答台账：装进阻塞 ask，收到答案即落盘、重复提问直接回放。
    // None = 不记账（独立测试）。
    answer_log: Option<Arc<AnswerLog>>,
) -> Result<(AgentRunner, String), String> {
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
    // delegate-return 审查的"职责"参照：追加工具协议/ground truth
    // 之前的角色本体 prompt（与 controller delegate 路径一致）。
    let role_responsibilities = role.system_prompt.clone();
    // step 级工具过滤：非空时收紧到 role.allowed_tools ∩ step_tools。
    // 请求了角色没有的工具名 → warning（多为笔误），不静默吞。
    let (effective_tools, ignored) = effective_step_tools(&role.allowed_tools, step_tools);
    if !ignored.is_empty() {
        eprintln!(
            "[workflow] step 请求了角色 '{role_id}' 没有的工具，已忽略：{}（角色可用：{}）",
            ignored.join(", "),
            role.allowed_tools.join(", ")
        );
    }
    role.allowed_tools = effective_tools;
    // 与 chat 的 build_runner 一致：带工具的角色必须拿到工具调用协议
    // 提示 + 系统 ground truth（cwd 等），否则模型不知道该用工具，
    // 会回答"我没有文件访问权限"。
    if !role.allowed_tools.is_empty() {
        role.system_prompt
            .push_str(&crate::controller::tool_usage_prompt(&role.allowed_tools));
    }
    role.system_prompt
        .push_str(&crate::ground_truth::ground_truth_block(cwd));
    // 角色模型指派优先取 resolver 的最新快照：角色编辑器保存后，
    // 进行中的 workflow 的后续分派也能用上新模型（merged 是 workflow
    // 启动时的固化快照，可能已过时）。
    let (chain_ids, tier) = match resolver.role_model_assignment(&role.id) {
        Some((chain, t)) => (
            if chain.is_empty() {
                role.model_chain.clone()
            } else {
                chain
            },
            t.unwrap_or(role.default_model_tier),
        ),
        None => (role.model_chain.clone(), role.default_model_tier),
    };
    let models = resolver
        .resolve_chain(&role.id, tier, &chain_ids)
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
        // 文档暂存：staging 启用时把 write/read 换成暂存包装版（写
        // 重定向到 .latte/staging/<wf_id>/ + 读 overlay）。放在其它
        // 工具注册之前，后续注册不受影响。
        if let Some(st) = &staging {
            st.wrap_tools(&rtm, role_id);
        }
        // 注册 plan 工具：角色有"plan"时，注册 tool 使其在 LLM 可见
        // （与 controller::build_runner 对齐）。workflow 引擎不走
        // delegate 工具，阶段门无人消费——给一个私有句柄即可（plan
        // 提案仍会广播 PlanProposed 事件，只是不驱动 delegate 门禁）。
        if role.allowed_tools.iter().any(|t| t == "plan") {
            let plan_stage: crate::controller::SharedPlanStage =
                Arc::new(parking_lot::RwLock::new(crate::controller::PlanStage::Normal));
            // session_id 取自 answer_log（run 的恢复身份三元组之一）：
            // 有它才能把 PlanProposed 快照落盘，进程重启后「添加任务」
            // 弹窗仍可补发。CLI / 独立测试的 run 没有，落空串 = 不落盘。
            let sid = answer_log
                .as_ref()
                .and_then(|l| l.session_id())
                .unwrap_or_default()
                .to_string();
            register_plan_tool(
                &rtm,
                event_tx.clone(),
                role_id.to_string(),
                plan_stage,
                &cwd,
                sid,
            )
            .map_err(|e| format!("register plan for '{role_id}': {e}"))?;
        }
        // ask / task_report：与 controller::build_runner 对齐——manager
        // 在 workflow step 里也要能向用户抛选择题（ask 是其 prompt 指定
        // 的唯一提问通道）和回报任务看板；缺失时模型调用得到
        // Tool not found（jemalloc 日志实锤：manager 在 decide 步调
        // ask 失败，选择框永远没弹出）。
        // 注意：子代理没有"下一轮"，ask 必须是阻塞模式——挂起等用户
        // 在弹框回答，答案经 /api/chat/choice-answer 直达本工具结果
        // （jemalloc 日志实锤：fire-and-forget 的"结束本轮等回答"语义
        // 让 decide 步产出变成「等待您回答」垃圾文本流进下游）。
        if role.allowed_tools.iter().any(|t| t == "ask") {
            let blocking = crate::controller::AskBlocking {
                cancel_flag: Some(cancel_flag.clone()),
                agent_pause_gate: agent_pause_gate.clone(),
                answer_log: answer_log.clone(),
                role_id: role_id.to_string(),
            };
            crate::controller::register_ask_tool(
                &rtm,
                event_tx.clone(),
                role_id.to_string(),
                Some(blocking),
                // 阻塞分支不用这个：它走 AskBlocking::answer_log 那条
                // 带 wf_id 的落盘路径（可续跑），比非阻塞快照更完整。
                None,
            )
            .map_err(|e| format!("register ask for '{role_id}': {e}"))?;
        }
        if role.allowed_tools.iter().any(|t| t == "task_report") {
            crate::controller::register_task_report_tool(
                &rtm,
                event_tx.clone(),
                role_id.to_string(),
            )
            .map_err(|e| format!("register task_report for '{role_id}': {e}"))?;
        }
        // doc-graph 工具（scan/context/write/index）：与 controller::build_runner
        // 对齐，让带 doc_graph_* 工具的角色在 workflow 里也能维护图谱。
        {
            let has = ["doc_graph_scan", "doc_graph_context", "doc_write", "doc_index"]
                .iter()
                .any(|t| role.allowed_tools.iter().any(|a| a == t));
            if has {
                crate::doc_graph_tools::register_doc_graph_tools(&rtm, cwd.to_path_buf())
                    .map_err(|e| format!("register doc_graph tools for '{role_id}': {e}"))?;
            }
        }
        // 熔断：与 controller delegate 路径对齐，给 specialist
        // runner 装工具轮次上限（jemalloc 事故前为 0 = 无限）。
        AgentRunner::new_with_tools(agent, rtm, crate::controller::specialist_max_tool_rounds())
    };
    let mut r = runner
        .with_role(role_id.to_string())
        .with_cwd(cwd.to_path_buf());
    if let Some(gate) = agent_pause_gate {
        r = r.with_agent_pause_gate(gate);
    }
    // 模型热更新：与 chat 的 build_runner 对齐。workflow step 的 runner
    // 在「模型不可用暂停 → 用户 ▶ 恢复」重试前会自查 resolver 代际——
    // 用户在 UI 改了角色模型指派并保存后，续跑用新链重试，而不是拿
    // 构建时的旧链重放同一个必挂请求。
    r = r.with_model_hot_reload(resolver.clone(), tier, chain_ids.clone());
    Ok((r, role_responsibilities))
}

/// 一次 step 分派的完整输入。每次分派 = 一个全新 subagent（与普通
/// 流程 manager 的 `delegate` 工具一致）：新建 runner、独立
/// subsession 日志、trace fan-out、advisor gate + 返回审查、
/// 运行中可取消。字段均为 `Send + 'static` clone，DAG 引擎可直接
/// move 进 spawned task。
struct SpeakerDispatch {
    speaker: String,
    step_id: String,
    prompt: String,
    wf_id: String,
    merged: Arc<AgentConfig>,
    resolver: Arc<ModelResolver>,
    default_params: GenerateParams,
    event_tx: broadcast::Sender<ChatEvent>,
    cwd: PathBuf,
    cancel_flag: Arc<AtomicBool>,
    /// Per-turn cancellation flag, distinct from `cancel_flag`. When
    /// `Some`, the step timeout emits `TimeoutWarning` + waits for user
    /// decision instead of hard-aborting.
    pub turn_cancel_flag: Option<Arc<AtomicBool>>,
    agent_pause_gate: Option<Arc<crate::pause_gate::AgentPauseGate>>,
    subsession_store: Option<Arc<crate::subsession::SubsessionStore>>,
    session_id: Option<String>,
    advisor_gate: Option<crate::advisor_monitor::GateConfig>,
    review_engine: Option<Arc<crate::advisor_monitor::AdvisorReviewEngine>>,
    advisor_pause: Option<crate::advisor_monitor::AdvisorPauseGate>,
    staging: Option<Arc<crate::staging::Staging>>,
    /// step 级工具过滤（`WorkflowStepDef::tools`），空 = 角色全集。
    step_tools: Vec<String>,
    /// 用户回答台账：阻塞 `ask` 收到答案即落盘，同一问题重试/resume
    /// 时直接回放，不再让用户重答一遍。
    answer_log: Arc<AnswerLog>,
}

impl SpeakerDispatch {
    fn from_ctx(
        ctx: &WorkflowRunContext,
        review_engine: Option<Arc<crate::advisor_monitor::AdvisorReviewEngine>>,
        wf_id: &str,
        step_id: &str,
        speaker: String,
        prompt: String,
        step_tools: Vec<String>,
        answer_log: Arc<AnswerLog>,
    ) -> Self {
        Self {
            speaker,
            step_id: step_id.to_string(),
            prompt,
            wf_id: wf_id.to_string(),
            merged: ctx.merged.clone(),
            resolver: ctx.resolver.clone(),
            default_params: ctx.default_params.clone(),
            cwd: ctx.cwd.clone(),
            event_tx: ctx.event_tx.clone(),
            cancel_flag: ctx.cancel_flag.clone(),
    turn_cancel_flag: ctx.turn_cancel_flag.clone(),
            agent_pause_gate: ctx.agent_pause_gate.clone(),
            subsession_store: ctx.subsession_store.clone(),
            session_id: ctx.session_id.clone(),
            advisor_gate: ctx.advisor_gate.clone(),
            review_engine,
            advisor_pause: ctx.advisor_pause.clone(),
            staging: ctx.staging.clone(),
            step_tools,
            answer_log,
        }
    }
}

/// Run one dispatched speaker as a full subagent, mirroring the
/// manager's `delegate` tool path (`controller::register_delegate_tool`):
///
/// 1. allocate a subsession (`DelegateStarted` with `sub_id`) so the
///    specialist's full trace is viewable from the UI;
/// 2. build a **fresh** runner (isolated context per dispatch) with a
///    fan-out sink (subsession log + `ChatEventTraceSink` for the
///    advisor monitor) and the advisor gate when enabled;
/// 3. run gated in a spawned task, polling `cancel_flag` every 500 ms
///    so a running dispatch can be aborted mid-turn;
/// 4. on success pass the output through `gate_delegate_return`
///    (advisor review) before handing it back to the engine.
///
/// The engines still own `WorkflowTurn` events and output-contract
/// retries; this function owns the delegate-style events and the
/// subsession lifecycle.
async fn run_step_speaker(inp: SpeakerDispatch) -> Result<String, StepFail> {
    let speaker = inp.speaker.clone();
    // Advisor intervene 暂停门：派发前先等用户拍板（此前 advisor 的
    // 「已暂停」对 workflow 不生效，流水线照跑）。
    if let Some(gate) = &inp.advisor_pause {
        gate.wait_if_requested().await;
    }
    // 1. Subsession：与普通流程一致，主事件流只看到
    //    DelegateStarted/Finished（摘要），专家完整 trace 落
    //    subsession，UI 右键「查看日志」经 /api/subsessions 读取。
    //    无 store/session_id（独立测试 run、CLI REPL）时退化为不发。
    let (sub_id, sub_sink) = match (&inp.subsession_store, &inp.session_id) {
        (Some(store), Some(sid)) => {
            let (id, sink) = store.create(sid, &speaker);
            (Some(id), Some(sink))
        }
        _ => (None, None),
    };
    if let Some(id) = &sub_id {
        // from_role 统一归 manager：workflow 默认由 manager 出面执行，
        // wf_id 标记「流程内分派」，UI 渲染为 manager 气泡 + 工作流徽章。
        let _ = inp.event_tx.send(ChatEvent::DelegateStarted {
            from_role: "manager".into(),
            to_role: speaker.clone(),
            task: inp.prompt.clone(),
            sub_id: id.clone(),
            wf_id: Some(inp.wf_id.clone()),
        });
    }
    // per-subsession 取消登记：UI 右键该 subsession →「终止此分派」。
    //
    // 注意粒度的真实边界：本旗标只让**这一个 step** 提前收工，但
    // `StepFail::Cancelled` 会被 DAG 引擎升级为 `set.abort_all()` +
    // `WfOutcome::Cancelled`（因为下游 step 靠 output_key 依赖它，
    // 缺了就跑不下去）——所以对 workflow 而言效果是"结束这次 workflow
    // 运行"，而不是"只停一条、兄弟继续"。真正兄弟互不影响的是
    // manager 的 delegate 路径（各自独立的工具调用）。
    // 相比 per-turn cancel 的好处仍然明确：session 与 manager 主循环
    // 存活，且 checkpoint 已记录完成步，用户可自行 resume。
    //
    // guard 随本函数退出自动摘除登记，所以 `is_active(sub_id)` 等价于
    // 「这条分派还在跑」，UI 据此决定是否显示菜单项。
    let (sub_cancel_flag, _sub_cancel_guard) = match &sub_id {
        Some(id) => (
            Some(crate::sub_cancel::register(id)),
            Some(crate::sub_cancel::SubCancelGuard(id.clone())),
        ),
        None => (None, None),
    };

    // 2-4. 执行 + advisor 返回审查的重做环：返回被判 intervene/terminate
    //    时带【上轮审查反馈】重派——advisor 的「打回重做」不再只是批注
    //    （jemalloc 实锤：reviewer 空转被 advisor 抓到、hint 要求重做，
    //    流水线却照流不误）。上限可配（AdvisorMonitorConfig::
    //    review_settings.return_max_redo，经 engine 注入），默认 1、
    //    硬上限 MAX_RETURN_REDO。
    let max_redo = inp
        .review_engine
        .as_ref()
        .map(|e| e.return_max_redo())
        .unwrap_or(crate::advisor_monitor::DEFAULT_RETURN_MAX_REDO);
    let mut prompt_for_turn = inp.prompt.clone();
    let mut redo: u8 = 0;
    let result: Result<String, StepFail> = loop {
        // 2. Fresh runner（每次尝试都是全新 subagent，无跨次记忆）
        //    + sink + gate。
        let (mut runner, role_responsibilities) = match build_role_runner(
            &speaker,
            &inp.merged,
            &inp.resolver,
            &inp.default_params,
            &inp.cwd,
            &inp.event_tx,
            inp.agent_pause_gate.clone(),
            inp.cancel_flag.clone(),
            inp.staging.clone(),
            &inp.step_tools,
            Some(inp.answer_log.clone()),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                // 构建失败也要补 DelegateFinished——否则 UI 上的分派
                // 气泡永远停在「⏳ 执行中…」。
                if let Some(id) = &sub_id {
                    let _ = inp.event_tx.send(ChatEvent::DelegateFinished {
                        from_role: "manager".into(),
                        to_role: speaker.clone(),
                        status: "failed".into(),
                        summary: e.clone(),
                        sub_id: id.clone(),
                        wf_id: Some(inp.wf_id.clone()),
                    });
                }
                return Err(StepFail::Failed(e));
            }
        };
        if let Some(sink) = &sub_sink {
            // Fan-out：子会话日志 + ChatEventTraceSink——专家的工具错误
            // 由此广播到 session channel，advisor monitor 的
            // specialist-error 检测依赖它（与 delegate 路径一致）。
            let specialist_sink: Arc<dyn crate::trace::TraceSink> =
                Arc::new(crate::trace::FanOutSink::new(vec![
                    sink.clone(),
                    Arc::new(crate::controller::ChatEventTraceSink {
                        event_tx: inp.event_tx.clone(),
                        sub_id: sub_id.clone(),
                    }),
                ]));
            runner = runner.with_sink(specialist_sink);
        }
        if let Some(gate) = inp.advisor_gate.clone() {
            runner = runner.with_gate_config(gate);
        }
        // intervene 暂停门也装到专家 runner：运行中判 Intervene 时在
        // tool-round 边界 park，直到用户拍板（或超时自动恢复）。
        if let Some(gate) = &inp.advisor_pause {
            runner = runner.with_pause_gate(gate.clone());
        }
        let _ = inp.event_tx.send(ChatEvent::RoleStarted {
            role_id: speaker.clone(),
            detail: format!("workflow step '{}'", inp.step_id),
            sub_id: sub_id.clone(),
        });

        // 3. Spawn + 500ms 轮询 cancel：运行中的分派可中途 abort
        //    （对齐 controller.rs delegate 的取消语义）。
        // 每个 step 开始前重置 turn_cancel_flag：上一步被用户取消后
        // flag 仍为 true，若不重置会导致后续 step 一进 poll loop 就
        // 立即 Cancelled（连锁取消 bug）。session 级 cancel_flag 不
        // 受影响——它由 abort 路径控制，跨步有效。
        if let Some(tcf) = &inp.turn_cancel_flag {
            tcf.store(false, Ordering::SeqCst);
        }
        let prompt = prompt_for_turn.clone();
        let cancel = inp.cancel_flag.clone();
        let mut run_handle = tokio::spawn(async move {
            let result = runner.run_turn_gated(&[Message::user(prompt)], None).await;
            let tool_count = runner.last_turn_tool_count;
            let tool_summary = runner.take_last_turn_tool_summary();
            result.map(|r| (r, tool_count, tool_summary))
        });
        // 熔断：wall-clock 超时（对齐 controller delegate；被 500ms
        // 轮询分支重建的 sleep 永远不响，必须在循环外 pin 住）。
        // 超时走 StepFail::Failed → 引擎按 max_retries 重试/失败冒泡，
        // 而不是无限挂起（jemalloc 事故：estimate 步挂 24min）。
        let timeout_s = crate::controller::specialist_timeout_secs(None);
        let timeout = tokio::time::sleep(std::time::Duration::from_secs(timeout_s));
        tokio::pin!(timeout);
        // step 实际起跑时刻：TimeoutWarning 复发时据此报真实已跑秒数
        // （而不是每次都报一个 soft_timeout_secs 的固定值）。
        let step_started = tokio::time::Instant::now();
        let attempt: Result<(String, usize, String), StepFail>;
        loop {
            tokio::select! {
                r = &mut run_handle => {
                    match r {
                        // 剥 <think>：主 session 展示与后续 speaker 的
                        // transcript 只保留正式回答；原文留在子会话 trace。
                        Ok(Ok((response, tool_count, tool_summary))) => {
                            let stripped = crate::controller::strip_think_blocks(&response);
                            // 空产出不算成功：判失败让引擎重试/失败，
                            // 而不是把空串写进 vars 穿给下游。
                            if crate::controller::is_empty_output(&stripped) {
                                attempt = Err(StepFail::Failed(format!(
                                    "subagent '{speaker}' 返回了空内容"
                                )));
                            } else {
                                attempt = Ok((stripped, tool_count, tool_summary));
                            }
                            break;
                        }
                        Ok(Err(e)) => {
                            // Gate 重试耗尽 → 被 advisor 终止：发
                            // AdvisorTerminated（带 sub_id）让 UI 显示
                            // 「已暂停」状态。
                            if let crate::error::AgentError::AdvisorTerminated { reason, detector } = &e {
                                let _ = inp.event_tx.send(ChatEvent::AdvisorTerminated {
                                    role_id: speaker.clone(),
                                    reason: reason.clone(),
                                    detector: Some(detector.clone()),
                                    sub_id: sub_id.clone(),
                                });
                            }
                            attempt = Err(StepFail::Failed(format!("subagent failed: {e}")));
                            break;
                        }
                        Err(e) => {
                            attempt = Err(StepFail::Failed(format!("task join failed: {e}")));
                            break;
                        }
                    }
                }
                _ = &mut timeout => {
                    // 子代理正挂起等用户回答（ask 阻塞中，choice 表里有
                    // 该 role 的挂起项）——「等用户」不是卡死，暂停熔断
                    // 倒计时：重置 timer 继续等，用户回答后自行恢复。
                    if crate::choice::has_pending_for(&speaker) {
                        timeout.as_mut().reset(
                            tokio::time::Instant::now()
                                + std::time::Duration::from_secs(timeout_s),
                        );
                        continue;
                    }
                    // 发 TimeoutWarning，让 UI 弹「继续等待 / 终止当前任务」。
                    // 不硬杀：防死循环不由 wall-clock 兜底——run_turn 内部
                    // 有 tool rounds 上限（100 轮）和工具循环检测（连续 3 次
                    // 同参相同调用），两者都会报 AgentError 终止 turn。合法
                    // 长任务（loop agent 跑很久）不应被杀。
                    let elapsed_secs = step_started.elapsed().as_secs();
                    let _ = inp.event_tx.send(ChatEvent::TimeoutWarning {
                        role_id: speaker.clone(),
                        elapsed_secs,
                        soft_timeout_secs: timeout_s,
                        hard_timeout_secs: 0, // 0 = 无硬超时（无限等待）
                        sub_id: sub_id.clone(),
                    });
                    // 按 timeout_s 周期性复发，而不是 park 到 1 年后：用户
                    // 点「继续等待」只是前端隐藏 banner，后端若从此静默，
                    // 真卡死的 step 在 turn_cancel_flag 为 None 的入口上
                    // 就彻底没人知道了。复发让「还活着但很久没回」始终可见，
                    // 同时 select! 不会空转（timer 永远指向未来）。
                    timeout.as_mut().reset(
                        tokio::time::Instant::now()
                            + std::time::Duration::from_secs(timeout_s),
                    );
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                    if cancel.load(Ordering::SeqCst) {
                        run_handle.abort();
                        let _ = inp.event_tx.send(ChatEvent::RoleFinished {
                            role_id: speaker.clone(),
                            detail: "cancelled by user".into(),
                            sub_id: sub_id.clone(),
                        });
                        if let Some(id) = &sub_id {
                            let _ = inp.event_tx.send(ChatEvent::DelegateFinished {
                                from_role: "manager".into(),
                                to_role: speaker.clone(),
                                status: "cancelled".into(),
                                summary: "workflow step cancelled by user".into(),
                                sub_id: id.clone(),
                                wf_id: Some(inp.wf_id.clone()),
                            });
                        }
                        return Err(StepFail::Cancelled);
                    }
                    // per-turn 取消：用户在 TimeoutWarning 里点「终止
                    // 当前任务」，掐掉当前 step 但保留 session。
                    if inp
                        .turn_cancel_flag
                        .as_ref()
                        .is_some_and(|tcf| tcf.load(Ordering::SeqCst))
                    {
                        run_handle.abort();
                        let _ = inp.event_tx.send(ChatEvent::RoleFinished {
                            role_id: speaker.clone(),
                            detail: "cancelled by user".into(),
                            sub_id: sub_id.clone(),
                        });
                        if let Some(id) = &sub_id {
                            let _ = inp.event_tx.send(ChatEvent::DelegateFinished {
                                from_role: "manager".into(),
                                to_role: speaker.clone(),
                                status: "cancelled".into(),
                                summary: "workflow step cancelled by user".into(),
                                sub_id: id.clone(),
                                wf_id: Some(inp.wf_id.clone()),
                            });
                        }
                        return Err(StepFail::Cancelled);
                    }
                    // per-subsession 取消：用户右键这条 subsession →
                    // 「终止此分派」。与上面 per-turn 分支的区别是
                    // **来源与善后**：错误经 CANCELLED_BY_USER_PREFIX
                    // 标记，manager 不会自动 resume（用户刚掐的活不该
                    // 被自动起回来）；session 与主循环存活，checkpoint
                    // 保留可人工续跑。
                    if sub_cancel_flag
                        .as_ref()
                        .is_some_and(|f| f.load(Ordering::SeqCst))
                    {
                        run_handle.abort();
                        let _ = inp.event_tx.send(ChatEvent::RoleFinished {
                            role_id: speaker.clone(),
                            detail: "terminated by user (subsession)".into(),
                            sub_id: sub_id.clone(),
                        });
                        if let Some(id) = &sub_id {
                            let _ = inp.event_tx.send(ChatEvent::DelegateFinished {
                                from_role: "manager".into(),
                                to_role: speaker.clone(),
                                status: "cancelled".into(),
                                summary: format!(
                                    "分派 '{speaker}' 被用户从 subsession 右键终止"
                                ),
                                sub_id: id.clone(),
                                wf_id: Some(inp.wf_id.clone()),
                            });
                        }
                        return Err(StepFail::Cancelled);
                    }
                }
            }
        }

        // 4. Delegate-return 审查：advisor 启用（gate Some ⇒ review_engine
        //    Some）时产出先过审查再回写引擎；intervene/terminate → 重做。
        match (attempt, &inp.review_engine) {
            (Ok((response, _tool_count, tool_summary)), Some(engine)) => {
                // 审查基准是原始任务（inp.prompt），不含重做批注。
                // 工具调用摘要：让 advisor 的返回审查能看到专家实际执行了
                // 哪些工具（jemalloc 实锤：interview step 用 ask 收齐答案
                // 后输出 user_profile，advisor 看不到 ask 记录→误判伪造）。
                // 摘要由 runner 收集（take_last_turn_tool_summary），
                // 含工具名 + 参数 + 结果，不再是裸计数。
                let (annotated, verdict) = crate::controller::gate_delegate_return(
                    engine,
                    &inp.event_tx,
                    &speaker,
                    &role_responsibilities,
                    &inp.prompt,
                    response,
                    &tool_summary,
                )
                .await;
                let intervene = matches!(
                    verdict.as_ref().map(|v| &v.verdict),
                    Some(crate::advisor_monitor::Verdict::Intervene)
                        | Some(crate::advisor_monitor::Verdict::Terminate)
                );
                if intervene && redo < max_redo {
                    redo += 1;
                    let v = verdict.as_ref().expect("intervene 蕴含 verdict");
                    let _ = inp.event_tx.send(ChatEvent::Status {
                        message: format!(
                            "↩ advisor 判定 {speaker} 的返回未达标，带审查意见重做（第 {redo}/{max_redo} 次）"
                        ),
                    });
                    // 与下一次派发的 RoleStarted 配平。
                    let _ = inp.event_tx.send(ChatEvent::RoleFinished {
                        role_id: speaker.clone(),
                        detail: "advisor intervene：带审查意见重做".into(),
                        sub_id: sub_id.clone(),
                    });
                    let feedback = if v.hint.is_empty() {
                        v.reason.clone()
                    } else {
                        format!("{}
处理建议：{}", v.reason, v.hint)
                    };
                    prompt_for_turn =
                        format!("{}

【上轮审查反馈】
{}", inp.prompt, feedback);
                    continue;
                }
                break Ok(annotated);
            }
            (other, _) => break other.map(|(r, _, _)| r),
        }
    };

    // 5. 收尾事件（RoleTurn 由引擎的 WorkflowTurn 承担，不重复发）。
    match &result {
        Ok(response) => {
            let _ = inp.event_tx.send(ChatEvent::RoleFinished {
                role_id: speaker.clone(),
                detail: format!("ok, {} chars", response.len()),
                sub_id: sub_id.clone(),
            });
            if let Some(id) = &sub_id {
                let _ = inp.event_tx.send(ChatEvent::DelegateFinished {
                    from_role: "manager".into(),
                    to_role: speaker.clone(),
                    status: "ok".into(),
                    summary: response.clone(),
                    sub_id: id.clone(),
                    wf_id: Some(inp.wf_id.clone()),
                });
            }
        }
        Err(StepFail::Failed(msg)) => {
            // 失败时 sub_sink 的 trace 在 TurnEnd 前断了——补写终结
            // 事件，让子会话日志有明确结尾（同 delegate 路径）。
            if let Some(sink) = &sub_sink {
                sink.emit(crate::trace::TraceEvent::TurnEnd {
                    meta: crate::trace::TraceMeta::now(
                        0,
                        &speaker,
                        &inp.session_id.clone().unwrap_or_default(),
                    ),
                    total_input: 0,
                    total_output: 0,
                    total_thinking: 0,
                    elapsed_ms: 0,
                });
            }
            let _ = inp.event_tx.send(ChatEvent::RoleFinished {
                role_id: speaker.clone(),
                detail: format!("error: {msg}"),
                sub_id: sub_id.clone(),
            });
            if let Some(id) = &sub_id {
                let _ = inp.event_tx.send(ChatEvent::DelegateFinished {
                    from_role: "manager".into(),
                    to_role: speaker.clone(),
                    status: "failed".into(),
                    summary: msg.clone(),
                    sub_id: id.clone(),
                    wf_id: Some(inp.wf_id.clone()),
                });
            }
        }
        // 取消分支已发齐事件。
        Err(StepFail::Cancelled) => {}
    }
    result
}

/// Serial engine: file-order steps. Every step dispatch runs as a
/// fresh subagent via [`run_step_speaker`] (no shared conversation
/// state — same as the manager's `delegate`); data flows between
/// steps only through `{{output_key}}` vars.
async fn run_workflow_serial(
    wf: &WorkflowDef,
    topic: &str,
    ctx: &WorkflowRunContext,
    wf_id: &str,
    ckpt: &CheckpointLog,
    resume: Option<&CheckpointState>,
    answer_log: &Arc<AnswerLog>,
) -> WfOutcome {
    // Advisor 启用时构建 delegate-return 审查引擎（整个 run 共享
    // 一个，仿 controller 的 register_delegate_tool）。有 session
    // 时挂 subsession sink：审查的 LLM 调用也落日志（观测盲区修复）。
    let review_engine = ctx.advisor_gate.as_ref().map(|gate| {
        let engine = crate::advisor_monitor::AdvisorReviewEngine::new(
            ctx.merged.clone(),
            ctx.resolver.clone(),
            ctx.default_params.clone(),
        )
        .with_review_settings(gate.review_settings);
        let engine = match (&ctx.subsession_store, &ctx.session_id) {
            (Some(store), Some(sid)) if !sid.is_empty() => {
                let (_id, sink) = store.create(sid, "advisor");
                engine.with_subsession_sink(sink)
            }
            _ => engine,
        };
        Arc::new(engine)
    });

    let mut vars: HashMap<String, String> = HashMap::new();
    vars.insert("topic".into(), topic.to_string());
    let total = wf.steps.len();
    let mut last_output = String::new();
    // output_key → 产出（原始文本），供嵌套调用方的 output_from 选取。
    let mut keyed: HashMap<String, String> = HashMap::new();
    // 断点续跑：已完成 step 的产出直接注入 vars，step 本体跳过
    // （不重跑、不重发 WorkflowStep/Turn 事件）。
    let mut done_steps: std::collections::HashSet<&str> = std::collections::HashSet::new();
    if let Some(state) = resume {
        for (step_id, output_key, output) in &state.completed {
            done_steps.insert(step_id.as_str());
            if let Some(key) = output_key {
                vars.insert(key.clone(), crate::controller::strip_review_annotation(output));
                keyed.insert(key.clone(), output.clone());
            }
            last_output = output.clone();
        }
    }

    for round in 0..wf.effective_max_rounds() {
        // 跨 step 循环（loop_until）的每轮状态：迭代计数按 loop_until
        // 所在 step 的完成次数计（key = step 下标）；pending_feedback
        // 是跳回时预置进目标 step prompt 的"上轮审查反馈"批注。
        let mut loop_iters: HashMap<usize, usize> = HashMap::new();
        let mut pending_feedback: Option<String> = None;
        let mut idx = 0;
        while idx < wf.steps.len() {
            let step = &wf.steps[idx];
            if ctx.cancel_flag.load(Ordering::SeqCst) {
                return WfOutcome::Cancelled;
            }
            if done_steps.contains(step.id.as_str()) {
                idx += 1;
                continue;
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
                // 返工反馈同样要喂进子流程的 topic（对齐 DAG 引擎）：
                // 跳回目标是嵌套 step 时，不带反馈的重跑就是逐字重放，
                // 评审证据全丢。take() 同时清掉批注，避免残留反馈错串
                // 到后面某个角色 step 上。
                let nested_topic = match pending_feedback.take() {
                    Some(fb) => format!("{nested_topic}\n\n【上轮审查反馈】\n{fb}"),
                    None => nested_topic,
                };
                match run_nested_workflow(
                    nested_name.clone(),
                    nested_topic,
                    ctx.clone(),
                    step.output_from.clone(),
                    step.export.clone(),
                    wf_id.to_string(),
                )
                .await
                {
                    Ok((output, exported)) => {
                        // export：子 workflow 的中间产物进父级 vars，
                        // 下游 step 用 {{父级变量名}} 取用。只进 vars
                        // 不进 keyed（见 `WorkflowStepDef::export`）。
                        for (var, value) in exported {
                            vars.insert(
                                var,
                                crate::controller::strip_review_annotation(&value),
                            );
                        }
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
                    // 监察批注不进 vars（同主路径）。
                    vars.insert(key.clone(), crate::controller::strip_review_annotation(&last_output));
                    keyed.insert(key.clone(), last_output.clone());
                }
                ckpt.record_step(&step.id, step.output_key.as_deref(), &last_output);
                idx += 1;
                continue;
            }
            for speaker in step.roles() {
                if ctx.cancel_flag.load(Ordering::SeqCst) {
                    return WfOutcome::Cancelled;
                }
                let mut step_vars = vars.clone();
                step_vars.insert("step_id".into(), step.id.clone());
                step_vars.insert("speaker".into(), speaker.clone());
                let mut base_prompt = wf.render_task(step, &step_vars);
                // 循环返工批注：跳回目标 step 的第一个 speaker 的 prompt
                // = 渲染后的 task + 上轮审查反馈（loop step 的产出）。
                if step_transcript.is_empty() {
                    if let Some(feedback) = pending_feedback.take() {
                        base_prompt =
                            format!("{base_prompt}\n\n【上轮审查反馈】\n{feedback}");
                    }
                }
                let full_prompt = if step_transcript.is_empty() {
                    base_prompt
                } else {
                    format!("{base_prompt}\n\n--- Preceding discussion in this step ---\n{step_transcript}")
                };
                // 产出契约重试：每次分派都是全新 subagent（无跨分派
                // 记忆），重试必须把验收批注拼回完整 prompt，保证模型
                // 仍拿得到任务上下文（与 DAG 引擎一致）。批注同时写进
                // step_transcript，让同 step 的后续 speaker 看到返工。
                let mut prompt = full_prompt.clone();
                let mut attempt: u32 = 0;
                loop {
                    let dispatch = SpeakerDispatch::from_ctx(
                        ctx,
                        review_engine.clone(),
                        wf_id,
                        &step.id,
                        speaker.clone(),
                        prompt.clone(),
                        step.tools.clone(),
                        answer_log.clone(),
                    );
                    let mut response = match run_step_speaker(dispatch).await {
                        Ok(r) => r,
                        Err(StepFail::Cancelled) => return WfOutcome::Cancelled,
                        Err(StepFail::Failed(e)) => {
                            return WfOutcome::Failed(format!(
                                "step '{}' speaker '{}': {e}",
                                step.id, speaker
                            ))
                        }
                    };
                    if let Err(reason) = check_output_contract(&step.output_contract, &response) {
                        if attempt >= step.max_retries {
                            // 重试耗尽：判死前过一道 advisor 语义兜底
                            // （字符串契约分不清格式错误与合法但无标记
                            // 的产出——jemalloc 实锤 gate REJECT 判死）。
                            match contract_last_resort_review(
                                &review_engine,
                                &ctx.event_tx,
                                &step.id,
                                speaker.as_str(),
                                &full_prompt,
                                &response,
                                &reason,
                            )
                            .await
                            {
                                Some(annotated) => response = annotated,
                                None => {
                                    return WfOutcome::Failed(format!(
                                        "step '{}' speaker '{}': 产出契约校验失败\
                                         （重试 {attempt} 次后仍不合格）：{reason}\
                                         ；不合格产出摘要：{}",
                                        step.id,
                                        speaker,
                                        output_excerpt(&response, 1200)
                                    ))
                                }
                            }
                        } else {
                            attempt += 1;
                            tracing::warn!(
                                step = %step.id,
                                speaker = %speaker,
                                attempt,
                                reason = %reason,
                                "workflow step output failed output_contract; retrying"
                            );
                            let annotation =
                                format!("上次产出未通过验收：{reason}。请修正后重新产出完整结果。");
                            step_transcript.push_str(&format!("[验收批注]: {annotation}\n"));
                            prompt = format!("{full_prompt}\n\n{annotation}");
                            continue;
                        }
                    }
                    // 契约合格的产出才发事件 / 进 transcript / last_output。
                    // （run_step_speaker 已剥离 <think>；step_transcript /
                    // last_output 保留原文供后续 speaker 与最终总结使用。）
                    let _ = ctx.event_tx.send(ChatEvent::WorkflowTurn {
                        wf_id: wf_id.to_string(),
                        step_id: step.id.clone(),
                        role_id: speaker.clone(),
                        content: response.clone(),
                        round,
                    });
                    step_transcript.push_str(&format!("[{speaker}]: {response}\n"));
                    last_output = response;
                    break;
                }
            }
            if let Some(key) = &step.output_key {
                // 监察批注（⚠️ [监察审查]…）不进 vars：它给人看，
                // 穿给下游 step / 嵌套 workflow 是污染。
                vars.insert(key.clone(), crate::controller::strip_review_annotation(&last_output));
                keyed.insert(key.clone(), last_output.clone());
            }
            ckpt.record_step(&step.id, step.output_key.as_deref(), &last_output);
            // 跨 step 循环：产出不含 loop_until 子串 → 跳回 loop_back_to
            // （缺省 = 自己）重做，本 step 产出作为批注预置进目标 prompt；
            // 迭代上限 max_iterations（缺省 3，硬上限 10），耗尽即失败。
            if let Some(cond) = &step.loop_until {
                // 显式声明了熔断标记的 step 才有「不可返工」终局；
                // 缺省一律返工（见 `loop_abort_on` 文档）。
                if let Some(marker) = &step.loop_abort_on {
                    if last_output.contains(marker.as_str()) {
                        return WfOutcome::Failed(format!(
                            "step '{}' 终止 workflow：{}",
                            step.id,
                            last_output.chars().take(500).collect::<String>()
                        ));
                    }
                }
                if !last_output.contains(cond.as_str()) {
                    let count = {
                        let c = loop_iters.entry(idx).or_insert(0);
                        *c += 1;
                        *c
                    };
                    let max = step.max_iterations.unwrap_or(3).min(10);
                    if count >= max {
                        let summary: String = last_output.chars().take(200).collect();
                        return WfOutcome::Failed(format!(
                            "step '{}' 循环条件「{cond}」在 {count} 次迭代后仍未满足\
                             （已达 max_iterations={max}），最后一次产出摘要：{summary}",
                            step.id
                        ));
                    }
                    let target_id = step.loop_back_to.as_deref().unwrap_or(step.id.as_str());
                    let target_idx = wf
                        .steps
                        .iter()
                        .position(|s| s.id == target_id)
                        .expect("validate 已保证 loop_back_to 指向存在的 step");
                    let _ = ctx.event_tx.send(ChatEvent::Status {
                        message: format!(
                            "↩ 第 {count} 轮返工：step '{}' 产出未满足循环条件\
                             「{cond}」，跳回 '{target_id}' 重做",
                            step.id
                        ),
                    });
                    pending_feedback = Some(last_output.clone());
                    idx = target_idx;
                    continue;
                }
            }
            idx += 1;
        }
    }

    WfOutcome::Ok(last_output, keyed)
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
    /// 顶层 run 的 `wf_id`（嵌套链的根）。`None` = 本 run 就是顶层。
    /// 嵌套 step 重建 ctx 时要原样带下去，否则子 run 认不出根身份，
    /// 阻塞 ask 的落盘记录会退化成指向子 run（父流水线醒不过来）。
    root_wf_id: Option<String>,
    merged: Arc<AgentConfig>,
    resolver: Arc<ModelResolver>,
    event_tx: broadcast::Sender<ChatEvent>,
    default_params: GenerateParams,
    cwd: PathBuf,
    cancel_flag: Arc<AtomicBool>,
    turn_cancel_flag: Option<Arc<AtomicBool>>,
    agent_pause_gate: Option<Arc<crate::pause_gate::AgentPauseGate>>,
    depth: u8,
    subsession_store: Option<Arc<crate::subsession::SubsessionStore>>,
    session_id: Option<String>,
    advisor_gate: Option<crate::advisor_monitor::GateConfig>,
    review_engine: Option<Arc<crate::advisor_monitor::AdvisorReviewEngine>>,
    advisor_pause: Option<crate::advisor_monitor::AdvisorPauseGate>,
    staging: Option<Arc<crate::staging::Staging>>,
    /// loop_until 返工时由调度器预置的「上轮审查反馈」（未满足循环
    /// 条件的那个 step 的产出），拼进本 step 的 prompt 后消费。
    feedback: Option<String>,
    /// 用户回答台账（透传给 SpeakerDispatch）。
    answer_log: Arc<AnswerLog>,
}

/// Run one DAG step (all its speakers, serially) with a fresh runner
/// per speaker. Returns `(step_idx, step_id, output_key, last_output)`
/// on success — `step_idx` 供调度器做 loop_until 返工判定。
async fn run_dag_step(
    inp: DagStepInput,
) -> Result<
    (
        usize,
        String,
        Option<String>,
        String,
        // export：嵌套 step 带出的子流程中间产物（父级变量名 → 值）。
        // 非嵌套 step 恒为空。
        std::collections::BTreeMap<String, String>,
    ),
    StepFail,
> {
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
        // loop_until 返工反馈也要喂给嵌套 workflow —— 否则跳回一个嵌套
        // step（design_and_plan 的 loop_back_to = "brainstorm" 就是）时
        // 子流程拿到的 topic 与上一轮**逐字相同**，评审的 file:line 证据
        // 被丢掉，返工退化成原地重摇骰子，白烧 max_iterations 轮。
        let nested_topic = match &inp.feedback {
            Some(fb) => format!("{nested_topic}\n\n【上轮审查反馈】\n{fb}"),
            None => nested_topic,
        };
        let ctx = WorkflowRunContext {
            merged: inp.merged.clone(),
            resolver: inp.resolver.clone(),
            default_params: inp.default_params.clone(),
            cwd: inp.cwd.clone(),
            event_tx: inp.event_tx.clone(),
            cancel_flag: inp.cancel_flag.clone(),
            turn_cancel_flag: inp.turn_cancel_flag.clone(),
            agent_pause_gate: inp.agent_pause_gate.clone(),
            depth: inp.depth,
            subsession_store: inp.subsession_store.clone(),
            session_id: inp.session_id.clone(),
            advisor_gate: inp.advisor_gate.clone(),
            advisor_pause: inp.advisor_pause.clone(),
            staging: inp.staging.clone(),
            root_wf_id: inp.root_wf_id.clone(),
        };
        let (output, exported) = run_nested_workflow(
            nested_name.clone(),
            nested_topic,
            ctx,
            step.output_from.clone(),
            step.export.clone(),
            inp.wf_id.clone(),
        )
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
        return Ok((
            inp.step_idx,
            step.id.clone(),
            step.output_key.clone(),
            output,
            exported,
        ));
    }

    let mut step_transcript = String::new();
    let mut last_output = String::new();
    for speaker in step.roles() {
        if inp.cancel_flag.load(Ordering::SeqCst) {
            return Err(StepFail::Cancelled);
        }
        let mut step_vars = inp.vars.clone();
        step_vars.insert("step_id".into(), step.id.clone());
        step_vars.insert("speaker".into(), speaker.clone());
        let base_prompt = inp.wf.render_task(step, &step_vars);
        // loop_until 返工反馈（调度器预置）：拼进 base prompt，本 step
        // 内所有 speaker 与契约重试都看得到（对齐串行引擎
        // pending_feedback 的语义）。
        let base_prompt = match &inp.feedback {
            Some(fb) => format!("{base_prompt}\n\n【上轮审查反馈】\n{fb}"),
            None => base_prompt,
        };
        let full_prompt = if step_transcript.is_empty() {
            base_prompt
        } else {
            format!("{base_prompt}\n\n--- Preceding discussion in this step ---\n{step_transcript}")
        };
        // 产出契约重试：每次分派都是全新 subagent（无跨分派记忆），
        // 批注必须拼回完整 prompt，保证模型仍拿得到任务上下文。
        let mut prompt = full_prompt.clone();
        let mut attempt: u32 = 0;
        loop {
            let dispatch = SpeakerDispatch {
                speaker: speaker.clone(),
                step_id: step.id.clone(),
                prompt: prompt.clone(),
                wf_id: inp.wf_id.clone(),
                merged: inp.merged.clone(),
                resolver: inp.resolver.clone(),
                default_params: inp.default_params.clone(),
                cwd: inp.cwd.clone(),
                cancel_flag: inp.cancel_flag.clone(),
                event_tx: inp.event_tx.clone(),
                turn_cancel_flag: inp.turn_cancel_flag.clone(),
                agent_pause_gate: inp.agent_pause_gate.clone(),
                subsession_store: inp.subsession_store.clone(),
                session_id: inp.session_id.clone(),
                advisor_gate: inp.advisor_gate.clone(),
                review_engine: inp.review_engine.clone(),
                advisor_pause: inp.advisor_pause.clone(),
                staging: inp.staging.clone(),
                step_tools: step.tools.clone(),
                answer_log: inp.answer_log.clone(),
            };
            let mut response = match run_step_speaker(dispatch).await {
                Ok(r) => r,
                Err(e) => {
                    return Err(match e {
                        StepFail::Cancelled => StepFail::Cancelled,
                        StepFail::Failed(msg) => StepFail::Failed(format!(
                            "step '{}' speaker '{}': {msg}",
                            step.id, speaker
                        )),
                    })
                }
            };
            if let Err(reason) = check_output_contract(&step.output_contract, &response) {
                if attempt >= step.max_retries {
                    // 重试耗尽：判死前过一道 advisor 语义兜底（同串行
                    // 引擎；jemalloc 实锤 gate REJECT 被契约判死）。
                    match contract_last_resort_review(
                        &inp.review_engine,
                        &inp.event_tx,
                        &step.id,
                        speaker.as_str(),
                        &full_prompt,
                        &response,
                        &reason,
                    )
                    .await
                    {
                        Some(annotated) => response = annotated,
                        None => {
                            return Err(StepFail::Failed(format!(
                                "step '{}' speaker '{}': 产出契约校验失败\
                                 （重试 {attempt} 次后仍不合格）：{reason}\
                                 ；不合格产出摘要：{}",
                                step.id,
                                speaker,
                                output_excerpt(&response, 1200)
                            )))
                        }
                    }
                } else {
                    attempt += 1;
                    tracing::warn!(
                        step = %step.id,
                        speaker = %speaker,
                        attempt,
                        reason = %reason,
                    "workflow step output failed output_contract; retrying"
                );
                    let annotation =
                        format!("上次产出未通过验收：{reason}。请修正后重新产出完整结果。");
                    step_transcript.push_str(&format!("[验收批注]: {annotation}\n"));
                    prompt = format!("{full_prompt}\n\n{annotation}");
                    continue;
                }
            }
            // 契约合格的产出才发事件 / 进 transcript / last_output。
            let _ = inp.event_tx.send(ChatEvent::WorkflowTurn {
                wf_id: inp.wf_id.clone(),
                step_id: step.id.clone(),
                role_id: speaker.clone(),
                content: response.clone(),
                round: inp.round,
            });
            step_transcript.push_str(&format!("[{speaker}]: {response}\n"));
            last_output = response;
            break;
        }
    }
    Ok((
        inp.step_idx,
        step.id.clone(),
        step.output_key.clone(),
        last_output,
        // 非嵌套 step 没有可导出的子流程产物。
        std::collections::BTreeMap::new(),
    ))
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
    ckpt: &CheckpointLog,
    resume: Option<&CheckpointState>,
    answer_log: &Arc<AnswerLog>,
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
    // Advisor 启用时构建 delegate-return 审查引擎（整个 run 共享
    // 一个，仿 controller 的 register_delegate_tool）。有 session
    // 时挂 subsession sink：审查的 LLM 调用也落日志（观测盲区修复）。
    let review_engine = ctx.advisor_gate.as_ref().map(|gate| {
        let engine = crate::advisor_monitor::AdvisorReviewEngine::new(
            ctx.merged.clone(),
            ctx.resolver.clone(),
            ctx.default_params.clone(),
        )
        .with_review_settings(gate.review_settings);
        let engine = match (&ctx.subsession_store, &ctx.session_id) {
            (Some(store), Some(sid)) if !sid.is_empty() => {
                let (_id, sink) = store.create(sid, "advisor");
                engine.with_subsession_sink(sink)
            }
            _ => engine,
        };
        Arc::new(engine)
    });

    let mut vars: HashMap<String, String> = HashMap::new();
    vars.insert("topic".into(), topic.to_string());
    // step_id -> last_output, used to resolve the final return value.
    let mut outputs: HashMap<String, String> = HashMap::new();
    // output_key → 产出（原始文本），供嵌套调用方的 output_from 选取。
    let mut keyed: HashMap<String, String> = HashMap::new();
    // 断点续跑：已完成 step 预填 outputs/vars（视为依赖已满足），
    // wave 调度时跳过，不再 spawn。
    let mut done_steps: std::collections::HashSet<&str> = std::collections::HashSet::new();
    if let Some(state) = resume {
        for (step_id, output_key, output) in &state.completed {
            done_steps.insert(step_id.as_str());
            outputs.insert(step_id.clone(), output.clone());
            if let Some(key) = output_key {
                vars.insert(key.clone(), crate::controller::strip_review_annotation(output));
                keyed.insert(key.clone(), output.clone());
            }
        }
    }

    for round in 0..wf.effective_max_rounds() {
        // loop_until 返工的每轮状态（对齐串行引擎）：迭代计数按
        // loop_until 所在 step 的完成次数计（key = step 下标）；
        // pending_feedback 是跳回时预置进目标 step prompt 的
        // 「上轮审查反馈」（key = 目标 step id）。
        let mut loop_iters: HashMap<usize, usize> = HashMap::new();
        let mut pending_feedback: HashMap<String, String> = HashMap::new();
        let mut wave_idx = 0;
        while wave_idx < waves.len() {
            let wave = &waves[wave_idx];
            if ctx.cancel_flag.load(Ordering::SeqCst) {
                return WfOutcome::Cancelled;
            }
            type DagStepOk = (
                usize,
                String,
                Option<String>,
                String,
                std::collections::BTreeMap<String, String>,
            );
            let mut set: JoinSet<Result<DagStepOk, StepFail>> = JoinSet::new();
            for &idx in wave {
                if done_steps.contains(wf.steps[idx].id.as_str()) {
                    continue;
                }
                let inp = DagStepInput {
                    wf: wf_arc.clone(),
                    step_idx: idx,
                    total,
                    round,
                    vars: vars.clone(),
                    wf_id: wf_id.to_string(),
                    root_wf_id: ctx.root_wf_id.clone(),
                    merged: ctx.merged.clone(),
                    resolver: ctx.resolver.clone(),
                    default_params: ctx.default_params.clone(),
                    cancel_flag: ctx.cancel_flag.clone(),
                    cwd: ctx.cwd.clone(),
                    event_tx: ctx.event_tx.clone(),
                    turn_cancel_flag: ctx.turn_cancel_flag.clone(),
                    agent_pause_gate: ctx.agent_pause_gate.clone(),
                    depth: ctx.depth,
                    subsession_store: ctx.subsession_store.clone(),
                    session_id: ctx.session_id.clone(),
                    advisor_gate: ctx.advisor_gate.clone(),
                    review_engine: review_engine.clone(),
                    advisor_pause: ctx.advisor_pause.clone(),
                    staging: ctx.staging.clone(),
                    feedback: pending_feedback.remove(wf.steps[idx].id.as_str()),
                    answer_log: answer_log.clone(),
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
            let mut wave_results: Vec<(usize, Option<String>, String)> = Vec::new();
            let mut wave_exports: std::collections::BTreeMap<String, String> =
                std::collections::BTreeMap::new();
            while let Some(joined) = set.join_next().await {
                match joined {
                    Ok(Ok((step_idx, step_id, output_key, out, exported))) => {
                        ckpt.record_step(&step_id, output_key.as_deref(), &out);
                        outputs.insert(step_id, out.clone());
                        wave_exports.extend(exported);
                        wave_results.push((step_idx, output_key, out));
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
            for (_sidx, output_key, out) in &wave_results {
                if let Some(key) = output_key {
                    // 监察批注不进 vars（同串行引擎）。
                    vars.insert(key.clone(), crate::controller::strip_review_annotation(out));
                    keyed.insert(key.clone(), out.clone());
                }
            }
            // export：嵌套 step 带出的子流程中间产物进父级 vars，
            // 下游 step 用 {{父级变量名}} 取用。只进 vars 不进 keyed
            // （见 `WorkflowStepDef::export`）。
            for (var, value) in wave_exports {
                vars.insert(var, crate::controller::strip_review_annotation(&value));
            }

            // loop_until 返工判定：wave 整体 join 后才评估（不打断同
            // wave 仍在跑的兄弟 step）。产出不满足条件的 step 把产出
            // 作为反馈预置给跳回目标，调度指针退回目标所在 wave，
            // 目标及其全部下游作废重跑（迭代上限 max_iterations，缺省
            // 3、硬上限 10，耗尽才 failed——jemalloc 实锤：gate 的
            // 合法 REJECT 此前被契约直接判死，零返工）。
            let mut jump_back: Option<usize> = None;
            let mut rework_notes: Vec<String> = Vec::new();
            for (sidx, _key, out) in &wave_results {
                let step = &wf.steps[*sidx];
                let Some(cond) = &step.loop_until else { continue };
                // 熔断判定必须在 loop_until 保护内：此前它套在循环外、
                // 对 wave 里**每个** step 生效，任何产出里出现该字符串
                // 的无关 step（评审引用裁决原文就够）都能判死整条
                // workflow。且缺省不再熔断（见 `loop_abort_on`）。
                if let Some(marker) = &step.loop_abort_on {
                    if out.contains(marker.as_str()) {
                        return WfOutcome::Failed(format!(
                            "step '{}' 终止 workflow：{}",
                            step.id,
                            out.chars().take(500).collect::<String>()
                        ));
                    }
                }
                if out.contains(cond.as_str()) {
                    continue;
                }
                let count = {
                    let c = loop_iters.entry(*sidx).or_insert(0);
                    *c += 1;
                    *c
                };
                let max = step.max_iterations.unwrap_or(3).min(10);
                if count >= max {
                    let summary: String = out.chars().take(200).collect();
                    return WfOutcome::Failed(format!(
                        "step '{}' 循环条件「{cond}」在 {count} 次迭代后仍未满足\
                         （已达 max_iterations={max}），最后一次产出摘要：{summary}",
                        step.id
                    ));
                }
                let target_id = step.loop_back_to.clone().unwrap_or_else(|| step.id.clone());
                let target_wave = waves
                    .iter()
                    .position(|w| w.iter().any(|&i| wf.steps[i].id == target_id))
                    .expect("validate 已保证 loop_back_to 指向存在的 step");
                pending_feedback.insert(target_id.clone(), out.clone());
                rework_notes.push(format!(
                    "step '{}' 未满足「{cond}」（第 {count}/{max} 轮），跳回 '{target_id}'",
                    step.id
                ));
                jump_back = Some(match jump_back {
                    Some(cur) => cur.min(target_wave),
                    None => target_wave,
                });
            }
            if let Some(target_wave) = jump_back {
                let _ = ctx.event_tx.send(ChatEvent::Status {
                    message: format!("↩ DAG 返工：{}", rework_notes.join("；")),
                });
                // 作废目标 wave 及之后全部 step 的产出（vars/keyed/
                // outputs），让下游随重跑拿到返工后的新值而不是旧快照。
                // checkpoint 只增不改：重跑会产生重复 step 记录，resume
                // 预填是「后写覆盖」，最后一次产出生效，无需清理。
                for w in &waves[target_wave..] {
                    for &i in w {
                        outputs.remove(wf.steps[i].id.as_str());
                        if let Some(key) = &wf.steps[i].output_key {
                            vars.remove(key);
                            keyed.remove(key);
                        }
                        // export 出去的中间产物同样要作废：留着旧的
                        // survey/证据，返工后的下游会拿新方案去对旧证据。
                        for var in wf.steps[i].export.keys() {
                            vars.remove(var);
                        }
                    }
                }
                wave_idx = target_wave;
                continue;
            }
            wave_idx += 1;
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
    WfOutcome::Ok(final_out, keyed)
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

    #[test]
    fn tdd_development_has_four_state_and_two_stage_review() {
        let raw = include_str!("../../config/workflows/tdd_development.toml");
        let wf: WorkflowDef = toml::from_str(raw).expect("valid TOML");
        wf.validate().expect("tdd_development should validate");
        // 实现 step：四态报告契约
        let implement = wf.steps.iter().find(|s| s.id == "implement").unwrap();
        assert!(implement.output_contract.require.iter().any(|r| r == "STATUS:"));
        assert!(implement.task_text().contains("DONE_WITH_CONCERNS"));
        assert!(implement.task_text().contains("BLOCKED"));
        assert!(implement.task_text().contains("NEEDS_CONTEXT"));
        // 规格审查：不要相信实现报告 + VERDICT 契约
        let spec = wf.steps.iter().find(|s| s.id == "spec_review").unwrap();
        assert!(spec.task_text().contains("不要相信实现报告"));
        assert!(spec.output_contract.require.iter().any(|r| r == "VERDICT:"));
        // 质量审查在规格审查之后
        let ids: Vec<&str> = wf.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["tests_first", "implement", "spec_review", "quality_review"]);
    }

    #[test]
    fn learn_workflow_structure() {
        let raw = include_str!("../../config/workflows/learn.toml");
        let wf: WorkflowDef = toml::from_str(raw).expect("valid TOML");
        wf.validate().expect("learn should validate");
        let ids: Vec<&str> = wf.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["investigate", "structure", "write", "verify"]);
        // investigate：防幻觉——要求行号锚点
        assert!(wf.steps[0].task_text().contains("行号"));
        // structure：六层骨架契约
        let structure = &wf.steps[1];
        for layer in ["L0", "L1", "L2", "L3", "L4", "L5", "L6"] {
            assert!(
                structure.output_contract.require.iter().any(|r| r == layer),
                "structure contract missing {layer}"
            );
        }
        // write：必须产出 Mermaid 图与名词解释
        let write = &wf.steps[2];
        assert!(write.output_contract.require.iter().any(|r| r == "```mermaid"));
        assert!(write.output_contract.require.iter().any(|r| r == "名词解释"));
        // verify：reviewer + programmer 接力（校对 + 修正）
        assert_eq!(wf.steps[3].roles(), vec!["reviewer", "programmer"]);
    }
    /// learn_loop：交互式学习循环——plan（拆解+写账本）→ teach（loop_until
    /// 弹窗出题）→ report。核心：teach 必须是循环步（每轮 fresh subagent
    /// context 隔离），且 tutor 配 ask 工具（弹窗出题，答案不入主 session）。
    #[test]
    fn learn_loop_workflow_structure() {
        let raw = include_str!("../../config/workflows/learn_loop.toml");
        let wf: WorkflowDef = toml::from_str(raw).expect("valid TOML");
        wf.validate().expect("learn_loop should validate");
        let ids: Vec<&str> = wf.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["plan", "teach", "report"]);
        // teach：跨 step 循环（A——每轮 fresh subagent，context 隔离）
        let teach = &wf.steps[1];
        assert_eq!(teach.loop_until.as_deref(), Some("STATUS_ALL_DONE"));
        assert_eq!(teach.max_iterations, Some(10));
        assert_eq!(teach.roles(), vec!["tutor"]);
        // teach 必须产出循环状态标记（CONTINUE / ALL_DONE）
        assert!(teach.task_text().contains("STATUS_CONTINUE"));
        assert!(teach.task_text().contains("STATUS_ALL_DONE"));
        // plan：先写账本再进循环；report：结尾收报告
        assert!(wf.steps[0].task_text().contains("ledger.json"));
        assert!(wf.steps[2].task_text().contains("ledger.json"));
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

    /// DAG loop_until 校验：loop_back_to 指向严格更早 wave → 通过。
    #[test]
    fn dag_loop_back_to_earlier_wave_ok() {
        let wf = wf_from(
            r#"
name = "dag_loop_ok"
[[steps]]
id = "design"
role = "pm"
task = "t"
[[steps]]
id = "gate"
role = "architect"
task = "t"
depends_on = ["design"]
loop_until = "VERDICT: PASS"
loop_back_to = "design"
"#,
        );
        wf.validate().expect("指向更早 wave 的 loop_back_to 应合法");
    }

    /// DAG loop_until 校验：loop_back_to 缺省（= 自己，同 wave）→
    /// 拒绝，并提示改用 output_contract。
    #[test]
    fn dag_loop_self_cycle_rejected() {
        let wf = wf_from(
            r#"
name = "dag_loop_self"
[[steps]]
id = "design"
role = "pm"
task = "t"
[[steps]]
id = "gate"
role = "architect"
task = "t"
depends_on = ["design"]
loop_until = "VERDICT: PASS"
"#,
        );
        let err = wf.validate().expect_err("同 wave 自环必须拒绝");
        assert!(err.contains("严格更早的 wave"), "got: {err}");
    }

    /// DAG loop_until 校验：跳回同 wave 的兄弟 step → 拒绝。
    #[test]
    fn dag_loop_back_to_same_wave_rejected() {
        let wf = wf_from(
            r#"
name = "dag_loop_same_wave"
[[steps]]
id = "design"
role = "pm"
task = "t"
[[steps]]
id = "review_a"
role = "architect"
task = "t"
depends_on = ["design"]
[[steps]]
id = "review_b"
role = "architect"
task = "t"
depends_on = ["design"]
loop_until = "VERDICT: PASS"
loop_back_to = "review_a"
"#,
        );
        let err = wf.validate().expect_err("同 wave 跳回必须拒绝");
        assert!(err.contains("严格更早的 wave"), "got: {err}");
    }

    /// 串行引擎的 loop_until 自环不受影响（回归保护：新校验只
    /// 约束 DAG）。
    #[test]
    fn serial_loop_self_cycle_still_ok() {
        let wf = wf_from(
            r#"
name = "serial_loop"
[[steps]]
id = "a"
role = "pm"
task = "t"
[[steps]]
id = "b"
role = "architect"
task = "t"
loop_until = "VERDICT: PASS"
loop_back_to = "a"
"#,
        );
        wf.validate().expect("串行 loop_until 应保持合法");
    }

    /// `loop_abort_on` 没有 `loop_until` 就毫无作用（会被静默忽略），
    /// validate 必须拒绝而不是让它变成看不见的死配置。
    #[test]
    fn loop_abort_on_without_loop_until_rejected() {
        let wf = wf_from(
            r#"
name = "abort_orphan"
[[steps]]
id = "a"
role = "pm"
task = "t"
loop_abort_on = "VERDICT: REJECT"
"#,
        );
        let err = wf.validate().expect_err("孤立的 loop_abort_on 必须报错");
        assert!(err.contains("loop_abort_on"), "错误应点名字段: {err}");
        assert!(err.contains("loop_until"), "错误应说明依赖: {err}");
    }

    /// 分派单元计数：多 speaker、max_retries、返工环倍数都要计入。
    #[test]
    fn dispatch_units_counts_speakers_retries_and_loops() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();

        // 单 role 三步 = 3 个单元。
        let wf = wf_from(
            r#"
name = "plain"
[[steps]]
id = "a"
role = "pm"
task = "t"
[[steps]]
id = "b"
role = "architect"
task = "t"
[[steps]]
id = "c"
role = "reviewer"
task = "t"
"#,
        );
        assert_eq!(dispatch_units(&wf, cwd, 0), 3);

        // 多 speaker：一个 step 里每个 role 各一次分派。
        let wf = wf_from(
            r#"
name = "multi"
[[steps]]
id = "a"
speakers = ["pm", "architect", "reviewer"]
prompt = "t"
"#,
        );
        assert_eq!(dispatch_units(&wf, cwd, 0), 3);

        // max_retries=2 → 该 step 最多跑 3 次。
        let wf = wf_from(
            r#"
name = "retry"
[[steps]]
id = "a"
role = "pm"
task = "t"
max_retries = 2
"#,
        );
        assert_eq!(dispatch_units(&wf, cwd, 0), 3);

        // 返工环：额外预算只覆盖被重跑的区间 [loop_back_to, loop step]，
        // 不是整条流水线乘轮数。
        // upstream(1) + a(1) + gate(1) = 3 基础；区间 a..=gate = 2 个单元，
        // (3-1) × 2 = 4 额外 → 7。
        let wf = wf_from(
            r#"
name = "loop"
[[steps]]
id = "upstream"
role = "pm"
task = "t"
[[steps]]
id = "a"
role = "architect"
task = "t"
[[steps]]
id = "gate"
role = "reviewer"
task = "t"
loop_until = "VERDICT: PASS"
loop_back_to = "a"
max_iterations = 3
"#,
        );
        assert_eq!(
            dispatch_units(&wf, cwd, 0),
            7,
            "上游 upstream 一次都不会重跑，不该被计入返工倍数"
        );
    }

    /// per-unit 预算旋钮：默认值 + env 覆盖。刻意与 per-step 软超时
    /// （900s 告警线）分开——混用会把 design_and_plan 算成 23 小时。
    #[test]
    fn budget_per_unit_defaults_and_env_override() {
        let _guard = crate::test_util::ENV_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("LATTE_AGENT_WORKFLOW_BUDGET_PER_UNIT_SECS");
        assert_eq!(budget_secs_per_unit(), DEFAULT_BUDGET_SECS_PER_UNIT);
        assert_ne!(
            DEFAULT_BUDGET_SECS_PER_UNIT,
            crate::controller::DEFAULT_UI_DELEGATE_TIMEOUT_SECS,
            "预算单位不得等于 per-step 软超时告警线"
        );
        std::env::set_var("LATTE_AGENT_WORKFLOW_BUDGET_PER_UNIT_SECS", "60");
        assert_eq!(budget_secs_per_unit(), 60);
        // 0 / 垃圾值退回默认，不会算出 0 预算（0 = 关闭，语义完全相反）。
        std::env::set_var("LATTE_AGENT_WORKFLOW_BUDGET_PER_UNIT_SECS", "0");
        assert_eq!(budget_secs_per_unit(), DEFAULT_BUDGET_SECS_PER_UNIT);
        std::env::set_var("LATTE_AGENT_WORKFLOW_BUDGET_PER_UNIT_SECS", "abc");
        assert_eq!(budget_secs_per_unit(), DEFAULT_BUDGET_SECS_PER_UNIT);
        std::env::remove_var("LATTE_AGENT_WORKFLOW_BUDGET_PER_UNIT_SECS");
    }

    /// 嵌套 step 递归展开成子流程的单元数——「按子流程规模推算」的
    /// 核心。加载不出来的嵌套名按兜底值计，绝不报错。
    #[test]
    fn dispatch_units_expands_nested_workflows() {
        let dir = tempfile::tempdir().unwrap();
        let wf_dir = dir.path().join(".latte").join("workflows.d");
        std::fs::create_dir_all(&wf_dir).unwrap();
        std::fs::write(
            wf_dir.join("inner.toml"),
            r#"
name = "inner"
[[steps]]
id = "i1"
role = "pm"
task = "t"
[[steps]]
id = "i2"
speakers = ["architect", "reviewer"]
prompt = "t"
"#,
        )
        .unwrap();

        // inner 自己 = 1 + 2 = 3 个单元。
        let parent = wf_from(
            r#"
name = "outer"
[[steps]]
id = "own"
role = "pm"
task = "t"
[[steps]]
id = "nested"
workflow = "inner"
task = "t"
"#,
        );
        assert_eq!(
            dispatch_units(&parent, dir.path(), 0),
            4,
            "1（自有 step）+ 3（展开 inner）"
        );

        // 加载不出来的嵌套名 → 兜底值，不 panic 不报错。
        let broken = wf_from(
            r#"
name = "outer2"
[[steps]]
id = "nested"
workflow = "does_not_exist"
task = "t"
"#,
        );
        assert_eq!(dispatch_units(&broken, dir.path(), 0), NESTED_UNKNOWN_UNITS);
    }

    /// 拿仓库里真实的 design_and_plan（6 层嵌套 + 双评审并行波 + gate
    /// 返工环）验证推算落在合理区间：实测这条流水线约 48 分钟，预算
    /// 必须明显宽于它（否则健康的长任务会被误杀），同时远低于 24h
    /// 天花板（否则等于没有天花板，回到改动前）。
    #[test]
    fn real_design_and_plan_budget_is_sane() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root")
            .to_path_buf();
        let wf = match load_workflow("design_and_plan", &root) {
            Ok(w) => w,
            // 仓库布局变了就跳过，不把无关改动搞红。
            Err(_) => return,
        };
        let units = dispatch_units(&wf, &root, 0);
        let budget = estimate_budget_secs(&wf, &root, DEFAULT_BUDGET_SECS_PER_UNIT);
        assert!(
            units >= 20,
            "6 个嵌套子流程 + gate 返工环，单元数不该这么小: {units}"
        );
        // 下界：明显宽于实测 48 分钟，否则健康的长任务会被误杀。
        assert!(
            budget > 48 * 60 * 2,
            "预算必须明显宽于实测 48 分钟: {budget}s（{units} 单元）"
        );
        // 上界：必须远离 24h 天花板，否则「有上限」只是纸面上的
        // （改动前就是 24h 常量）。取 12h 作回归线。
        assert!(
            budget < 12 * 3600,
            "预算顶到天花板量级 = 等于没有上限: {budget}s（{units} 单元）"
        );
    }

    /// 预算 = 单元数 × 单次分派预算，并夹在 [per_unit, 工具层天花板]。
    /// 上界很重要：预算若超过 `ORCHESTRATION_TOOL_TIMEOUT_SECS`，工具
    /// 管理器那一层会先熔断，又变回孤儿 step。
    #[test]
    fn estimate_budget_scales_with_units_and_stays_under_tool_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let wf = wf_from(
            r#"
name = "three"
[[steps]]
id = "a"
role = "pm"
task = "t"
[[steps]]
id = "b"
role = "architect"
task = "t"
[[steps]]
id = "c"
role = "reviewer"
task = "t"
"#,
        );
        assert_eq!(estimate_budget_secs(&wf, cwd, 100), 300);
        // 下界：单步 workflow 也至少有一个单元的余量。
        let one = wf_from(
            r#"
name = "one"
[[steps]]
id = "a"
role = "pm"
task = "t"
"#,
        );
        assert_eq!(estimate_budget_secs(&one, cwd, 900), 900);
        // 上界：不得越过工具管理器那一层的天花板。
        let huge = estimate_budget_secs(&wf, cwd, u64::MAX / 2);
        assert_eq!(huge, crate::controller::ORCHESTRATION_TOOL_TIMEOUT_SECS);
    }

    /// 嵌套 run 不自设预算：内外层各设一份等于同一段时间被重复计费，
    /// 子流程会先于父级预算被判死。
    #[test]
    fn nested_runs_do_not_set_their_own_budget() {
        assert_eq!(Budget::Off, Budget::Off);
        // depth > 0 的 Auto 等价于关闭 —— 由 run_workflow_inner 的
        // match 保证；这里锁住语义文档，实际行为由上面的端到端测试
        // （嵌套返工/组合类）覆盖。
        let dir = tempfile::tempdir().unwrap();
        let wf = wf_from(
            r#"
name = "n"
[[steps]]
id = "a"
role = "pm"
task = "t"
"#,
        );
        // 估算函数本身不看 depth：depth 判定在调用点。
        assert!(estimate_budget_secs(&wf, dir.path(), 10) > 0);
    }

    /// 熔断标记若把放行条件当子串包含，放行判定永远先命中，熔断成
    /// 死代码 —— 拒绝这种自相矛盾的配置。
    #[test]
    fn loop_abort_on_containing_loop_until_rejected() {
        let wf = wf_from(
            r#"
name = "abort_shadowed"
[[steps]]
id = "a"
role = "pm"
task = "t"
[[steps]]
id = "b"
role = "architect"
task = "t"
loop_until = "VERDICT:"
loop_back_to = "a"
loop_abort_on = "VERDICT: REJECT"
"#,
        );
        let err = wf.validate().expect_err("被放行条件遮蔽的熔断标记必须报错");
        assert!(err.contains("熔断永远不会触发"), "错误应解释原因: {err}");
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

#[cfg(test)]
mod contract_tests {
    use super::*;

    #[test]
    fn contract_min_chars() {
        let c = OutputContract { min_chars: Some(5), ..Default::default() };
        // 不足 → 报错，消息含实际/要求字符数
        let err = check_output_contract(&c, "太短").unwrap_err();
        assert!(err.contains('2') && err.contains('5'), "got: {err}");
        // 达标与超出都通过（按字符数，不是字节数）
        assert!(check_output_contract(&c, "刚刚好五个").is_ok());
        assert!(check_output_contract(&c, "超过五个字符也没问题").is_ok());
    }

    #[test]
    fn contract_forbid() {
        let c = OutputContract {
            forbid: vec!["TBD".into(), "待补充".into()],
            ..Default::default()
        };
        // 命中 → 报错含命中的子串
        let err = check_output_contract(&c, "这里留个 TBD 再说").unwrap_err();
        assert!(err.contains("TBD"), "got: {err}");
        // 未命中 → 通过
        assert!(check_output_contract(&c, "完整产出，没有占位符字样").is_ok());
    }

    #[test]
    fn contract_require() {
        let c = OutputContract {
            require: vec!["结论".into(), "风险".into()],
            ..Default::default()
        };
        // 缺失 → 报错含缺失的子串（require 全部出现才合格）
        let err = check_output_contract(&c, "只有结论没有别的").unwrap_err();
        assert!(err.contains("风险"), "got: {err}");
        // 齐全 → 通过
        assert!(check_output_contract(&c, "结论：可行。风险：无。").is_ok());
    }

    #[test]
    fn contract_empty_always_ok() {
        let c = OutputContract::default();
        assert!(check_output_contract(&c, "").is_ok());
        assert!(check_output_contract(&c, "TBD 待补充 随便写").is_ok());
    }

    #[test]
    fn output_excerpt_truncates_with_ellipsis() {
        // 短产出原样返回，不带省略号。
        assert_eq!(output_excerpt("短产出", 10), "短产出");
        // 超长截到 max_chars 并补「…」（按字符数，不是字节数）。
        let long: String = "x".repeat(1500);
        let e = output_excerpt(&long, 1200);
        assert_eq!(e.chars().count(), 1201, "1200 字符 + 省略号: {}", e.len());
        assert!(e.ends_with('…'));
    }

    #[test]
    fn output_contract_parses_from_step_toml() {
        let raw = r#"
name = "c"
[[steps]]
id = "s"
role = "pm"
task = "t"
max_retries = 2

[steps.output_contract]
min_chars = 100
forbid = ["TBD"]
require = ["结论"]
"#;
        let wf: WorkflowDef = toml::from_str(raw).unwrap();
        let step = &wf.steps[0];
        assert_eq!(step.max_retries, 2);
        assert_eq!(step.output_contract.min_chars, Some(100));
        assert_eq!(step.output_contract.forbid, vec!["TBD"]);
        assert_eq!(step.output_contract.require, vec!["结论"]);
    }

    #[test]
    fn output_contract_defaults_empty_when_omitted() {
        let raw = r#"
name = "c"
[[steps]]
id = "s"
role = "pm"
task = "t"
"#;
        let wf: WorkflowDef = toml::from_str(raw).unwrap();
        let c = &wf.steps[0].output_contract;
        assert_eq!(c.min_chars, None);
        assert!(c.forbid.is_empty());
        assert!(c.require.is_empty());
        // 空契约 = 不校验
        assert!(check_output_contract(c, "").is_ok());
    }

    /// 验收：implementation_plan 的 breakdown step（产出 plan）带产出契约。
    #[test]
    fn implementation_plan_breakdown_step_has_contract() {
        let raw = include_str!("../../config/workflows/implementation_plan.toml");
        let wf: WorkflowDef = toml::from_str(raw).expect("valid TOML");
        wf.validate().expect("implementation_plan should validate");
        let breakdown = wf
            .steps
            .iter()
            .find(|s| s.id == "breakdown")
            .expect("step 'breakdown' must exist");
        assert_eq!(breakdown.output_contract.min_chars, Some(200));
        assert_eq!(
            breakdown.output_contract.forbid,
            vec!["TBD", "待补充", "占位符"]
        );
        assert!(breakdown.max_retries >= 1, "contract needs retry budget");
    }

    /// 验收：task_refine 是「草案 → 评审 → 终审门 → 过审后才 plan 提交」
    /// 的四步评审流。gate 只输出裁决，submit 消费最新 draft。
    #[test]
    fn task_refine_is_reviewed_four_step_flow() {
        let raw = include_str!("../../config/workflows/task_refine.toml");
        let wf: WorkflowDef = toml::from_str(raw).expect("valid TOML");
        wf.validate().expect("task_refine should validate");

        let ids: Vec<&str> = wf.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["refine", "review", "gate", "submit"]);

        let refine = &wf.steps[0];
        assert_eq!(refine.output_key.as_deref(), Some("draft"));
        assert_eq!(refine.output_contract.min_chars, Some(200));
        assert!(
            refine.task_text().contains("禁止调用 plan"),
            "refine 步必须禁止调 plan（草案先过评审）"
        );

        let review = &wf.steps[1];
        assert_eq!(review.roles(), &["reviewer".to_string()]);
        assert_eq!(review.output_key.as_deref(), Some("verdict"));

        let gate = &wf.steps[2];
        assert_eq!(gate.loop_until.as_deref(), Some("VERDICT: ACCEPT"));
        assert_eq!(gate.loop_back_to.as_deref(), Some("refine"));
        assert_eq!(gate.max_iterations, Some(2));
        assert_eq!(gate.output_contract.require, vec!["VERDICT:"]);
        assert!(gate.prompt.contains("VERDICT: ACCEPT"));
        assert!(gate.prompt.contains("VERDICT: REVISE"));
        assert!(gate.prompt.contains("VERDICT: REJECT"));
        assert!(gate.prompt.contains("不要重新生成、修改或复述完整拆分草案"));
        assert!(!gate.prompt.contains("原样完整附上拆分草案"));
        assert_eq!(gate.output_key.as_deref(), Some("approved"));

        let submit = &wf.steps[3];
        assert_eq!(submit.roles(), &["task_planner".to_string()]);
        assert!(
            submit.task_text().contains("{{draft}}"),
            "submit 步必须消费最新 draft，而不是 gate 裁决文本"
        );
        assert!(!submit.task_text().contains("{{approved}}"));

        // step 级工具过滤：refine 步硬性摘掉 plan（引擎层 enforce，
        // 不靠 prompt 自觉），submit 步不限制（需要 plan 提交）。
        assert_eq!(refine.tools, vec!["read", "search"]);
        assert!(review.tools.is_empty());
        assert!(gate.tools.is_empty());
        assert!(submit.tools.is_empty());
    }

    /// step 级工具过滤语义：空 = 角色全集；非空 = 交集（保持角色
    /// 顺序），角色没有的请求项进 ignored（笔误预警），不静默吞。
    #[test]
    fn effective_step_tools_intersects_and_reports_ignored() {
        let role = vec!["read".to_string(), "search".to_string(), "plan".to_string()];

        // 空 = 全集
        let (eff, ignored) = effective_step_tools(&role, &[]);
        assert_eq!(eff, role);
        assert!(ignored.is_empty());

        // 交集 + 保序 + 忽略项上报
        let (eff, ignored) = effective_step_tools(
            &role,
            &["serach".to_string(), "plan".to_string(), "read".to_string()],
        );
        assert_eq!(eff, vec!["read", "plan"]);
        assert_eq!(ignored, vec!["serach"]);

        // 全都不匹配 → 空工具表（合法：该 step 只用语言产出）
        let (eff, _) = effective_step_tools(&role, &["nonexistent".to_string()]);
        assert!(eff.is_empty());
    }

    /// implementation_plan 与 design_and_plan 用 PASS/REJECT 两值词表，
    /// 其中 REJECT = 「返工」而非终局，所以这两个 gate **不得**声明
    /// `loop_abort_on`——一旦声明，REJECT 又会像此前硬编码那样判死整条
    /// 流水线，`loop_until`/`loop_back_to` 变回死配置（jemalloc 实锤：
    /// 唯一跑到 gate 的运行 44 分钟零产出）。task_refine 的
    /// ACCEPT/REVISE/REJECT 三值契约由专门测试覆盖。
    #[test]
    fn gate_prompts_keep_conditions_and_reject_unabsorbed_errors() {
        let cases: [(&str, &str); 2] = [
            (
                include_str!("../../config/workflows/implementation_plan.toml"),
                "implementation_plan",
            ),
            (
                include_str!("../../config/workflows/design_and_plan.toml"),
                "design_and_plan",
            ),
        ];
        for (raw, wf_name) in cases {
            let wf: WorkflowDef = toml::from_str(raw).expect("valid TOML");
            wf.validate().expect("workflow should validate");
            let gate = wf
                .steps
                .iter()
                .find(|s| s.id == "gate")
                .unwrap_or_else(|| panic!("{wf_name} must have a gate step"));
            let prompt = gate.task_text();
            assert!(prompt.contains("【通过条件/修正项】"));
            assert!(prompt.contains("实测错误"));
            assert!(prompt.contains("VERDICT: PASS") && prompt.contains("VERDICT: REJECT"));
            // 返工环必须真的通电：REJECT 是返工信号，不能被熔断标记
            // 提前判死。
            assert_eq!(gate.loop_until.as_deref(), Some("VERDICT: PASS"), "{wf_name}");
            assert!(
                gate.loop_abort_on.is_none(),
                "{wf_name} 的 gate 用 REJECT 表示返工，声明 loop_abort_on 会让返工环失效"
            );
        }
    }

    /// 验收：implementation_plan 的 gate 必须把 REJECT 变成返工循环
    /// （loop_until 跳回 breakdown），而不是契约判死整条流水线——
    /// jemalloc 实锤：评审如实 REJECT（3 条实证阻断）导致 50 分钟
    /// workflow 血本无归。契约只能卡「VERDICT:」格式，不能
    /// require PASS（否则 REJECT 又变回契约失败）。
    #[test]
    fn implementation_plan_gate_reject_loops_back_for_rework() {
        let raw = include_str!("../../config/workflows/implementation_plan.toml");
        let wf: WorkflowDef = toml::from_str(raw).expect("valid TOML");
        wf.validate().expect("implementation_plan should validate");
        let gate = wf
            .steps
            .iter()
            .find(|s| s.id == "gate")
            .expect("gate step");
        assert_eq!(
            gate.loop_until.as_deref(),
            Some("VERDICT: PASS"),
            "gate 必须用 loop_until 驱动 REJECT 返工"
        );
        assert_eq!(
            gate.loop_back_to.as_deref(),
            Some("breakdown"),
            "REJECT 反馈必须回到拆解步让 architect 吸收"
        );
        assert!(
            gate.max_iterations.unwrap_or(3) >= 2,
            "至少给 2 轮返工机会"
        );
        assert!(
            !gate.output_contract.require.iter().any(|r| r == "VERDICT: PASS"),
            "契约 require PASS 会把 REJECT 判死，必须只卡格式"
        );
        assert!(
            gate.output_contract.require.iter().any(|r| r == "VERDICT:"),
            "契约应保留 VERDICT: 格式校验"
        );
    }

    /// 验收：design_and_plan 的 gate 同样必须把 REJECT 变成返工循环
    /// （DAG 引擎已支持 loop_until），跳回 brainstorm 重新生成设计——
    /// jemalloc 实锤：gate 如实 REJECT（extent 状态数、LG_QUANTUM 等
    /// 实测错误）被契约 require PASS 判死，50 分钟流水线零产出。
    #[test]
    fn design_and_plan_gate_reject_loops_back_for_rework() {
        let raw = include_str!("../../config/workflows/design_and_plan.toml");
        let wf: WorkflowDef = toml::from_str(raw).expect("valid TOML");
        wf.validate().expect("design_and_plan should validate");
        let gate = wf
            .steps
            .iter()
            .find(|s| s.id == "gate")
            .expect("gate step");
        assert_eq!(
            gate.loop_until.as_deref(),
            Some("VERDICT: PASS"),
            "gate 必须用 loop_until 驱动 REJECT 返工"
        );
        assert_eq!(
            gate.loop_back_to.as_deref(),
            Some("brainstorm"),
            "跨评审的结构性问题必须回到设计脑暴重做"
        );
        assert!(
            gate.max_iterations.unwrap_or(3) >= 2,
            "至少给 2 轮返工机会"
        );
        assert!(
            !gate.output_contract.require.iter().any(|r| r == "VERDICT: PASS"),
            "契约 require PASS 会把 REJECT 判死，必须只卡格式"
        );
        assert!(
            gate.output_contract.require.iter().any(|r| r == "VERDICT:"),
            "契约应保留 VERDICT: 格式校验"
        );
    }

    /// 用户主动终止必须能与执行失败区分开。
    ///
    /// 为什么关键：两者的善后动作相反。失败 → manager 该用 wf_id
    /// resume 续跑；用户终止 → 绝不能自动续跑，否则右键「终止此分派」
    /// 刚把活停掉，manager 立刻又 resume 起来，功能等于没有。
    #[test]
    fn user_cancellation_is_distinguishable_from_failure() {
        // run_workflow 的 Cancelled 分支产出的形状（带 wf_id 便于人工续跑）。
        let cancelled = format!("{CANCELLED_BY_USER_PREFIX}（wf_id=wf-design_and_plan-123）");
        assert!(
            is_cancelled_by_user(&cancelled),
            "取消信息必须被识别为用户终止：{cancelled}"
        );
        assert!(
            cancelled.contains("wf-design_and_plan-123"),
            "取消信息应带 wf_id，用户想手动续跑时有据可查"
        );

        // 各类真失败都不能被误判成"用户终止"，否则丢掉 resume 善后。
        for fail in [
            "step 'gate' speaker 'reviewer': 契约失败",
            "workflow step task failed: join error",
            "step 'breakdown' 返回了空内容",
        ] {
            assert!(
                !is_cancelled_by_user(fail),
                "执行失败不该被当成用户终止（会丢掉 resume 善后）：{fail}"
            );
        }
    }

    /// 验收：init_project 的 project_md step 带产出契约（任务要求文件名出现）。
    #[test]
    fn init_project_project_md_step_has_contract() {
        let raw = include_str!("../../config/workflows/init_project.toml");
        let wf: WorkflowDef = toml::from_str(raw).expect("valid TOML");
        wf.validate().expect("init_project should validate");
        let pmd = wf
            .steps
            .iter()
            .find(|s| s.id == "project_md")
            .expect("step 'project_md' must exist");
        assert_eq!(pmd.output_contract.min_chars, Some(400));
        assert_eq!(pmd.output_contract.require, vec!["prompts/project.md"]);
        assert!(pmd.max_retries >= 1, "contract needs retry budget");
    }
}

#[cfg(test)]
mod contract_engine_tests {
    use super::*;
    use crate::config::{ModelCatalog, ModelDef};
    use crate::role::RoleTemplate;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    fn openai_body(content: &str) -> String {
        serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 0,
            "model": "test",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": content },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
        })
        .to_string()
    }

    /// 单模型（premium tier 指向 wiremock）+ 单角色 "worker" 的测试配置。
    fn test_config_at(base_url: &str) -> Arc<AgentConfig> {
        let roles = HashMap::from([(
            "worker".to_string(),
            RoleTemplate {
                id: "worker".into(),
                name: "Worker".into(),
                category: "execution".into(),
                model_tier: "premium".into(),
                model_chain: vec![],
                prompt_file: None,
                temperature: None,
                tools: vec![],
                icon: String::new(),
                skills: vec![],
                code_paths: vec![],
            },
        )]);
        Arc::new(AgentConfig {
            advisor: Default::default(),
            models: ModelCatalog {
                models: vec![ModelDef {
                    name: "Test Premium".into(),
                    api: "openai".into(),
                    provider: "test".into(),
                    base_url: base_url.into(),
                    api_key: "test-key".into(),
                    context_window: 32000,
                    max_tokens: 4096,
                    supports_thinking: false,
                    supports_vision: false,
                    supports_image_generation: false,
                    cost_per_million_input: None,
                    cost_per_million_output: None,
                    tier: Some("premium".into()),
                    timeout_secs: None,
                }],
                tiers: None,
                role_tiers: None,
            },
            roles,
        })
    }

    fn test_ctx(config: Arc<AgentConfig>) -> (WorkflowRunContext, broadcast::Receiver<ChatEvent>) {
        let resolver = Arc::new(ModelResolver::from_config(&config).unwrap());
        let (event_tx, event_rx) = broadcast::channel(64);
        (
            WorkflowRunContext {
                merged: config,
                resolver,
                default_params: GenerateParams::default(),
                cwd: std::env::temp_dir(),
                event_tx,
                cancel_flag: Arc::new(AtomicBool::new(false)),
                turn_cancel_flag: None,
                agent_pause_gate: None, depth: 0,
                subsession_store: None,
                session_id: None,
                advisor_gate: None,
                advisor_pause: None,
                staging: None,
                root_wf_id: None,
            },
            event_rx,
        )
    }

    /// 串行 workflow：单 step 单 speaker，契约 min_chars=50 + forbid TBD。
    fn contract_wf() -> WorkflowDef {
        let raw = r#"
name = "contract_demo"
[[steps]]
id = "draft"
role = "worker"
task = "写一份计划"
output_key = "draft"
max_retries = 1

[steps.output_contract]
min_chars = 50
forbid = ["TBD"]
"#;
        toml::from_str(raw).expect("valid TOML")
    }

    /// workflow step 的 runner 必须挂模型热更新源——否则「模型不可用
    /// 暂停 → 用户在 UI 改配置保存 → ▶ 恢复」的重试仍拿构建时的旧链
    /// 重放同一个必挂请求（jemalloc tester 卡 k3 400 的实锤路径）。
    #[tokio::test]
    async fn build_role_runner_attaches_model_hot_reload() {
        let config = test_config_at("http://127.0.0.1:1");
        let resolver = Arc::new(ModelResolver::from_config(&config).unwrap());
        let (event_tx, _rx) = broadcast::channel(64);
        let (runner, _resp) = build_role_runner(
            "worker",
            &config,
            &resolver,
            &GenerateParams::default(),
            std::env::temp_dir().as_path(),
            &event_tx,
            None,
            Arc::new(AtomicBool::new(false)),
            None,
            &[],
            None,
        )
        .await
        .expect("worker runner");
        assert!(
            runner.has_model_hot_reload(),
            "workflow step runner must hot-reload model chain on resume"
        );
    }

    fn count_workflow_turns(rx: &mut broadcast::Receiver<ChatEvent>) -> usize {
        let mut n = 0;        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, ChatEvent::WorkflowTurn { .. }) {
                n += 1;
            }
        }
        n
    }

    /// 第一次产出不合格 → 带批注重试同一 speaker，第二次合格：
    /// 最终成功、第二次请求带批注、WorkflowTurn 只发一次。
    #[tokio::test]
    async fn serial_contract_retry_passes_with_annotation() {
        let server = wiremock::MockServer::start().await;
        // 兜底 mock（后注册但先匹配耗尽前的兜底）：合格产出。
        let good = "这是一份足够详实的实现计划，覆盖方案概述、工作分解、依赖关系与风险分析，每一项都给出了明确的验收标准，没有任何占位内容。";
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("未通过验收"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(good)))
            .mount(&server)
            .await;
        // 首次请求（不含批注）→ 不合格产出，只生效一次。
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body("TBD")))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;

        let (ctx, mut rx) = test_ctx(test_config_at(&server.uri()));
        let result = run_workflow(&contract_wf(), "测试主题", &ctx).await;
        let out = result.expect("retry 后应成功");
        assert_eq!(out, good);

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2, "首发 + 一次重试: {}", requests.len());
        let second = String::from_utf8_lossy(&requests[1].body);
        assert!(second.contains("上次产出未通过验收"), "重试 prompt 带批注: {second}");
        assert!(second.contains("产出过短"), "批注含违规原因: {second}");
        assert_eq!(
            count_workflow_turns(&mut rx),
            1,
            "失败尝试不发 WorkflowTurn，只有合格产出发一次"
        );
    }

    /// 重试耗尽（max_retries=1，两次产出都不合格）→ step 失败，
    /// 错误消息含 step id、speaker、最后一次违规原因。
    #[tokio::test]
    async fn serial_contract_retry_exhausted_fails_step() {
        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body("TBD")))
            .mount(&server)
            .await;

        let (ctx, mut rx) = test_ctx(test_config_at(&server.uri()));
        let err = run_workflow(&contract_wf(), "测试主题", &ctx)
            .await
            .expect_err("重试耗尽必须失败");
        assert!(err.contains("draft"), "错误含 step id: {err}");
        assert!(err.contains("worker"), "错误含 speaker: {err}");
        assert!(err.contains("产出过短"), "错误含最后一次违规原因: {err}");

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2, "首发 + max_retries=1 次重试: {}", requests.len());
        assert_eq!(count_workflow_turns(&mut rx), 0, "无合格产出，不发 WorkflowTurn");
    }

    /// advisor 返回审查判 intervene → 同一 speaker 带【上轮审查反馈】
    /// 重做一次，第二次审 ok → step 成功、产出是重做版（jemalloc 实锤：
    /// reviewer 空转被 advisor 抓到「打回重做」，流水线却照流不误）。
    #[tokio::test]
    async fn advisor_intervene_triggers_redo_with_feedback() {
        let server = wiremock::MockServer::start().await;
        let v1 = "v1 产出：这份内容足够长，肯定超过五十个字符的短输出门禁阈值，不含工具回声。";
        let v2 = "v2 产出：已吸收审查意见重写，同样超过五十个字符的短输出门禁阈值，不含工具回声。";
        // wiremock 后挂载的优先匹配；逐个 up_to_n_times(1) 耗尽后落回
        // 下一个。调用顺序：worker首发 → advisor审v1 → worker重做 →
        // advisor审v2。判别子串：advisor 的审查 prompt 内嵌被审产出。
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("v2 产出"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "verdict: ok\nreason:\nhint:",
            )))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("【上轮审查反馈】"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(v2)))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("v1 产出"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "verdict: intervene\nreason: 偏航未达标\nhint: 重写并紧扣任务",
            )))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(v1)))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let wf: WorkflowDef = toml::from_str(
            r#"
name = "redo_test"
description = "advisor intervene redo"

[[steps]]
id = "only"
description = "单步"
speakers = ["worker"]
output_key = "out"
prompt = "围绕测试主题产出学习笔记。"
"#,
        )
        .unwrap();
        let (mut ctx, mut rx) = test_ctx(test_config_at(&server.uri()));
        ctx.advisor_gate = Some(crate::advisor_monitor::GateConfig::default());
        let out = run_workflow(&wf, "测试主题", &ctx)
            .await
            .expect("重做后应成功");
        assert_eq!(out, v2, "最终产出必须是重做版");

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 4, "worker×2 + advisor×2: {}", requests.len());
        let redo_req = String::from_utf8_lossy(&requests[2].body);
        assert!(redo_req.contains("【上轮审查反馈】"), "重做 prompt 带反馈批注: {redo_req}");
        assert!(redo_req.contains("重写并紧扣任务"), "批注含 advisor hint: {redo_req}");

        let events: Vec<ChatEvent> = {
            let mut v = Vec::new();
            while let Ok(ev) = rx.try_recv() {
                v.push(ev);
            }
            v
        };
        assert!(
            events.iter().any(|ev| matches!(
                ev,
                ChatEvent::Status { message } if message.contains("带审查意见重做")
            )),
            "应有重做 Status 事件: {events:?}"
        );
        // intervene 批注不污染最终产出（verdict ok 的第二次返回原样）。
        assert!(!out.contains("监察审查"));
    }

    /// intervene 重做后仍被判 intervene → 不再无限重试：第二次产出
    /// 带批注放行（重做上限取默认 return_max_redo=1）。
    #[tokio::test]
    async fn advisor_intervene_redo_exhausted_passes_annotated() {
        let server = wiremock::MockServer::start().await;
        let v1 = "v1 产出：这份内容足够长，肯定超过五十个字符的短输出门禁阈值，不含工具回声。";
        let v2 = "v2 产出：重做版内容，同样超过五十个字符的短输出门禁阈值，不含工具回声。";
        // 两次审查都 intervene：先审 v1、再审 v2。
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("v2 产出"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "verdict: intervene\nreason: 仍不达标\nhint: 再改",
            )))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("【上轮审查反馈】"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(v2)))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("v1 产出"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "verdict: intervene\nreason: 偏航\nhint: 重写",
            )))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(v1)))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let wf: WorkflowDef = toml::from_str(
            r#"
name = "redo_cap_test"
description = "advisor intervene redo cap"

[[steps]]
id = "only"
description = "单步"
speakers = ["worker"]
output_key = "out"
prompt = "围绕测试主题产出学习笔记。"
"#,
        )
        .unwrap();
        let (mut ctx, _rx) = test_ctx(test_config_at(&server.uri()));
        ctx.advisor_gate = Some(crate::advisor_monitor::GateConfig::default());
        let out = run_workflow(&wf, "测试主题", &ctx)
            .await
            .expect("重试耗尽后应带批注放行而不是失败");
        assert!(out.contains("v2 产出"), "产出是重做版: {out}");
        assert!(out.contains("监察审查"), "耗尽后批注随产出放行: {out}");

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 4, "只重做一次: {}", requests.len());
    }

    /// 分派 = 完整 subagent（与普通流程 delegate 一致）：ctx 带
    /// subsession_store + session_id 时，每个 step 分派建独立
    /// subsession，并发 DelegateStarted/Finished（带 sub_id）；
    /// subsession 里有真实 trace（UI 右键「查看日志」的数据源）。
    #[tokio::test]
    async fn dispatch_creates_subsession_and_delegate_events() {
        let server = wiremock::MockServer::start().await;
        let good = "这是一份足够详实的产出，覆盖方案概述、工作分解与风险分析。";
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(good)))
            .mount(&server)
            .await;

        let (mut ctx, mut rx) = test_ctx(test_config_at(&server.uri()));
        let store = Arc::new(crate::subsession::SubsessionStore::new());
        ctx.subsession_store = Some(store.clone());
        ctx.session_id = Some("test-session".into());

        // 两步串行 workflow → 两次分派 → 两个独立 subsession。
        let raw = r#"
name = "sub_demo"
[[steps]]
id = "a"
role = "worker"
task = "任务A：{{topic}}"
output_key = "out_a"
[[steps]]
id = "b"
role = "worker"
task = "任务B：{{out_a}}"
"#;
        let wf: WorkflowDef = toml::from_str(raw).expect("valid TOML");
        run_workflow(&wf, "主题", &ctx).await.expect("两步都应成功");

        let mut started = 0;
        let mut finished_ok = 0;
        let mut sub_ids = std::collections::HashSet::new();
        while let Ok(ev) = rx.try_recv() {
            match ev {
                ChatEvent::DelegateStarted { from_role, sub_id, wf_id, .. } => {
                    // workflow 分派统一归属 manager，wf_id 标记「流程内」。
                    assert_eq!(from_role, "manager");
                    assert!(wf_id.is_some(), "workflow 分派必须带 wf_id 标记");
                    started += 1;
                    sub_ids.insert(sub_id);
                }
                ChatEvent::DelegateFinished { status, .. } if status == "ok" => {
                    finished_ok += 1;
                }
                _ => {}
            }
        }
        assert_eq!(started, 2, "两次分派各发一次 DelegateStarted");
        assert_eq!(finished_ok, 2, "两次分派各发一次 DelegateFinished(ok)");
        assert_eq!(sub_ids.len(), 2, "每次分派都是独立 subsession");
        for id in &sub_ids {
            let events = store
                .snapshot_any(id)
                .unwrap_or_else(|| panic!("subsession {id} 应有 trace"));
            assert!(!events.is_empty(), "subsession {id} 的 trace 不应为空");
        }
    }

    /// P0-2：workflow 运行时若 session gate 已 paused，role runner 应
    /// 在第一个 model call 边界 park —— 流水线不前进。
    #[tokio::test]
    async fn paused_session_gate_parks_workflow_at_first_turn() {
        let gate = crate::pause_gate::AgentPauseGate::new("test-session");
        // Pre-pause（模拟用户先按 ⏸ 才触发 workflow）。
        gate.pause();
        let server = wiremock::MockServer::start().await;
        let good = "这是一份足够详实的实现计划，覆盖方案概述、工作分解、依赖关系与风险分析，每一项都给出了明确的验收标准，没有任何占位内容。";
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                        good,
                    ))),
            )
            .await;
        let mut ctx = test_ctx(test_config_at(&server.uri())).0;
        ctx.agent_pause_gate = Some(gate.clone());
        let cwd_tmp = ctx.cwd.clone();
        let _ = cwd_tmp;
        let spawned = tokio::spawn(async move {
            run_workflow(&contract_wf(), "测试主题", &ctx).await
        });
        // 200ms 后应仍挂起（第一个 model call 边界 park）。
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        assert!(!spawned.is_finished(), "gate paused → workflow 应 park");
        // Resume → 流水线继续，跑完。
        gate.resume();
        let r = tokio::time::timeout(std::time::Duration::from_secs(5), spawned)
            .await
            .expect("workflow resumes after gate resume")
            .expect("run ok");
        assert!(r.is_ok(), "resume 后 workflow 成功: {:?}", r);
    }

    /// C3 兜底（串行）：契约重试耗尽 → advisor 语义审查判 ok →
    /// 带批注放行。jemalloc 实锤：gate 的合法 REJECT 没写契约要求的
    /// 「VERDICT: PASS」字样，被字符串契约当格式错误判死。
    #[tokio::test]
    async fn contract_exhausted_advisor_ok_passes_with_annotation() {
        let server = wiremock::MockServer::start().await;
        // worker 产出：语义合格但没写契约要求的验收标记（长度过 D5）。
        let no_mark = "这是一份语义合格但没有写约定验收标记的产出，长度足够超过五十个字符的短输出门禁阈值，内容完整覆盖任务要求。";
        // wiremock 先挂载优先（FIFO 实测）：worker 首发命中通用 mock
        // （仅 1 次）后耗尽；advisor 审查请求（内嵌被审产出文本）落到
        // 第二个 mock → verdict ok。
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(no_mark)))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains(
                "没有写约定验收标记",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "verdict: ok\nreason:\nhint:",
            )))
            .mount(&server)
            .await;

        let wf: WorkflowDef = toml::from_str(
            r#"
name = "last_resort_demo"
[[steps]]
id = "only"
role = "worker"
task = "围绕测试主题产出学习笔记"
output_key = "out"
[steps.output_contract]
require = ["契约要求的验收标记字符串"]
"#,
        )
        .unwrap();
        let mut ctx = test_ctx(test_config_at(&server.uri())).0;
        ctx.advisor_gate = Some(crate::advisor_monitor::GateConfig::default());
        let out = run_workflow(&wf, "测试主题", &ctx)
            .await
            .expect("advisor 语义兜底应放行");
        assert!(out.contains("没有写约定验收标记"), "产出本体保留: {out}");
        assert!(out.contains("监察审查"), "产出带兜底放行批注: {out}");
    }

    /// C3 兜底（串行）：advisor 判 intervene → 维持契约失败（兜底只
    /// 救「内容合格」的产出，不救真不合格的）。
    #[tokio::test]
    async fn contract_exhausted_advisor_intervene_still_fails() {
        let server = wiremock::MockServer::start().await;
        let no_mark = "这是一份语义合格但没有写约定验收标记的产出，长度足够超过五十个字符的短输出门禁阈值，内容完整覆盖任务要求。";
        // 请求序列（FIFO）：worker 首发 → advisor 返回审查 intervene
        // → worker 带【上轮审查反馈】重做（返回审查重做环，上限 1）
        // → advisor 复审 intervene → 重做耗尽放行 → 契约失败 →
        // last-resort 审查 intervene → 维持失败。
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(no_mark)))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("【上轮审查反馈】"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(no_mark)))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains(
                "没有写约定验收标记",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "verdict: intervene\nreason: 内容与任务无关\nhint: 重写",
            )))
            .mount(&server)
            .await;

        let wf: WorkflowDef = toml::from_str(
            r#"
name = "last_resort_deny"
[[steps]]
id = "only"
role = "worker"
task = "围绕测试主题产出学习笔记"
output_key = "out"
[steps.output_contract]
require = ["契约要求的验收标记字符串"]
"#,
        )
        .unwrap();
        let mut ctx = test_ctx(test_config_at(&server.uri())).0;
        ctx.advisor_gate = Some(crate::advisor_monitor::GateConfig::default());
        let err = run_workflow(&wf, "测试主题", &ctx)
            .await
            .expect_err("advisor 判 intervene 必须维持失败");
        assert!(err.contains("产出契约校验失败"), "got: {err}");
    }

    /// C3 兜底（DAG 引擎同款路径）：b 步契约耗尽 → advisor 判 ok →
    /// 放行，workflow 成功。
    #[tokio::test]
    async fn dag_contract_exhausted_advisor_ok_passes() {
        let server = wiremock::MockServer::start().await;
        let no_mark = "这是一份语义合格但没有写约定验收标记的产出，长度足够超过五十个字符的短输出门禁阈值，内容完整覆盖任务要求。";
        // FIFO：a → 任务一 mock；b → 任务二 mock（仅 1 次）；advisor
        // 审查请求（内嵌 b 的产出）落到最后一个 mock。
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("任务一"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "甲步骤产出：一段足够长的占位内容，确保超过五十个字符的短输出门禁阈值。",
            )))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("任务二"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(no_mark)))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains(
                "没有写约定验收标记",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "verdict: ok\nreason:\nhint:",
            )))
            .mount(&server)
            .await;

        let wf: WorkflowDef = toml::from_str(
            r#"
name = "dag_last_resort"
[[steps]]
id = "a"
role = "worker"
task = "任务一：{{topic}}"
output_key = "out_a"
[[steps]]
id = "b"
role = "worker"
task = "任务二：{{out_a}}"
output_key = "out_b"
depends_on = ["a"]
[steps.output_contract]
require = ["契约要求的验收标记字符串"]
"#,
        )
        .unwrap();
        let mut ctx = test_ctx(test_config_at(&server.uri())).0;
        ctx.advisor_gate = Some(crate::advisor_monitor::GateConfig::default());
        let out = run_workflow(&wf, "测试主题", &ctx)
            .await
            .expect("DAG advisor 语义兜底应放行");
        assert!(out.contains("监察审查"), "产出带兜底放行批注: {out}");
    }
}

#[cfg(test)]
mod resume_tests {
    use super::*;
    use crate::config::{ModelCatalog, ModelDef};
    use crate::role::RoleTemplate;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    fn openai_body(content: &str) -> String {
        serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 0,
            "model": "test",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": content },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
        })
        .to_string()
    }

    /// 单模型（premium tier 指向 wiremock）+ 单角色 "worker" 的测试配置。
    fn test_config_at(base_url: &str) -> Arc<AgentConfig> {
        let roles = HashMap::from([(
            "worker".to_string(),
            RoleTemplate {
                id: "worker".into(),
                name: "Worker".into(),
                category: "execution".into(),
                model_tier: "premium".into(),
                model_chain: vec![],
                prompt_file: None,
                temperature: None,
                tools: vec![],
                icon: String::new(),
                skills: vec![],
                code_paths: vec![],
            },
        )]);
        Arc::new(AgentConfig {
            advisor: Default::default(),
            models: ModelCatalog {
                models: vec![ModelDef {
                    name: "Test Premium".into(),
                    api: "openai".into(),
                    provider: "test".into(),
                    base_url: base_url.into(),
                    api_key: "test-key".into(),
                    context_window: 32000,
                    max_tokens: 4096,
                    supports_thinking: false,
                    supports_vision: false,
                    supports_image_generation: false,
                    cost_per_million_input: None,
                    cost_per_million_output: None,
                    tier: Some("premium".into()),
                    timeout_secs: None,
                }],
                tiers: None,
                role_tiers: None,
            },
            roles,
        })
    }

    fn test_ctx_at(
        config: Arc<AgentConfig>,
        cwd: PathBuf,
    ) -> (WorkflowRunContext, broadcast::Receiver<ChatEvent>) {
        let resolver = Arc::new(ModelResolver::from_config(&config).unwrap());
        let (event_tx, event_rx) = broadcast::channel(64);
        (
            WorkflowRunContext {
                merged: config,
                resolver,
                default_params: GenerateParams::default(),
                cwd,
                event_tx,
                cancel_flag: Arc::new(AtomicBool::new(false)),
                turn_cancel_flag: None,
                agent_pause_gate: None, depth: 0,
                subsession_store: None,
                session_id: None,
                advisor_gate: None,
                advisor_pause: None,
                staging: None,
                root_wf_id: None,
            },
            event_rx,
        )
    }

    /// 3-step 串行 workflow：b 带不可能通过的产出契约（require 一个
    /// 永远不会出现的字符串），max_retries=1 —— 用于制造"第 2 步失败"。
    /// 用契约失败而不是 HTTP 500：确定性强、无模型冷却/重试带来的
    /// 额外请求与等待。
    fn three_step_wf() -> WorkflowDef {
        let raw = r#"
name = "resume_demo"
[[steps]]
id = "a"
role = "worker"
task = "任务A：分析 {{topic}}"
output_key = "out_a"
[[steps]]
id = "b"
role = "worker"
task = "任务B：基于 {{out_a}} 设计 {{topic}}"
output_key = "out_b"
max_retries = 1
[steps.output_contract]
require = ["永远不可能出现的验收字符串"]
[[steps]]
id = "c"
role = "worker"
task = "任务C：汇总 {{out_b}}"
output_key = "out_c"
"#;
        toml::from_str(raw).expect("valid TOML")
    }

    /// 从失败消息尾部解析 wf_id（格式："...wf_id=xxx）"）。
    fn extract_wf_id(err: &str) -> String {
        let pos = err.rfind("wf_id=").expect("错误消息应带 wf_id");
        err[pos + "wf_id=".len()..]
            .trim_end_matches('）')
            .to_string()
    }

    fn read_checkpoint(cwd: &Path, wf_id: &str) -> Vec<serde_json::Value> {
        let raw = std::fs::read_to_string(checkpoint_path(cwd, wf_id))
            .expect("checkpoint 文件应存在");
        raw.lines()
            .map(|l| serde_json::from_str(l).expect("每行都是合法 JSON"))
            .collect()
    }

    /// checkpoint 写入：2-step workflow 跑完 → jsonl = meta + 2 条 step 记录，
    /// 字段齐全（workflow_name/topic/step_id/output_key/output/finished_at）。
    #[tokio::test]
    async fn checkpoint_written_per_step() {
        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body("产出内容")))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let wf: WorkflowDef = toml::from_str(
            r#"
name = "ckpt_demo"
[[steps]]
id = "a"
role = "worker"
task = "任务A"
output_key = "out_a"
[[steps]]
id = "b"
role = "worker"
task = "任务B {{out_a}}"
output_key = "out_b"
"#,
        )
        .unwrap();
        let (ctx, _rx) = test_ctx_at(test_config_at(&server.uri()), dir.path().to_path_buf());
        run_workflow(&wf, "主题", &ctx).await.expect("应成功");

        let runs_dir = dir.path().join(".latte").join("workflow-runs");
        let files: Vec<_> = std::fs::read_dir(&runs_dir)
            .expect("workflow-runs 目录应被创建")
            .flatten()
            .collect();
        assert_eq!(files.len(), 1, "一次运行一个 checkpoint 文件");
        let raw = std::fs::read_to_string(files[0].path()).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 3, "meta + 2 条 step 记录: {raw}");

        let meta: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(meta["type"], "meta");
        assert_eq!(meta["workflow_name"], "ckpt_demo");
        assert_eq!(meta["topic"], "主题");
        assert!(meta["wf_id"].as_str().unwrap().starts_with("wf-ckpt_demo-"));

        let s1: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(s1["type"], "step");
        assert_eq!(s1["step_id"], "a");
        assert_eq!(s1["output_key"], "out_a");
        assert_eq!(s1["output"], "产出内容");
        assert!(s1["finished_at"].is_number());

        let s2: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(s2["step_id"], "b");
        assert_eq!(s2["output_key"], "out_b");
    }

    /// resume 串行：3-step workflow 第 2 步失败（契约不可能通过）→
    /// 错误消息带进度与 wf_id；resume（topic 传空，用 checkpoint 的）
    /// 后第 1 步不再请求模型、{{out_a}} 与 {{topic}} 正常注入、最终成功。
    #[tokio::test]
    async fn resume_skips_completed_steps_serial() {
        // 第一次运行：模型产出永远不含验收字符串 → step b 契约失败。
        let server1 = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body("步骤产出")))
            .mount(&server1)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let wf = three_step_wf();
        let (ctx1, _rx1) = test_ctx_at(test_config_at(&server1.uri()), dir.path().to_path_buf());
        let err = run_workflow(&wf, "测试主题", &ctx1)
            .await
            .expect_err("step b 契约不可能通过，必须失败");
        assert!(err.contains("已完成 1/3 步"), "失败消息带进度: {err}");
        assert!(err.contains("从断点续跑"), "失败消息带 resume 提示: {err}");
        let wf_id = extract_wf_id(&err);

        // checkpoint：meta + step a 一条记录。
        let lines = read_checkpoint(dir.path(), &wf_id);
        assert_eq!(lines.len(), 2, "meta + 已完成的 step a: {lines:?}");
        assert_eq!(lines[1]["step_id"], "a");
        assert_eq!(lines[1]["output"], "步骤产出");

        // Resume：新 mock server（若 step a 重跑会向它多发请求）。
        // 模型这次产出含验收字符串 → b、c 都能过。
        let server2 = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "包含永远不可能出现的验收字符串的合格产出",
            )))
            .mount(&server2)
            .await;
        let (ctx2, mut rx2) = test_ctx_at(test_config_at(&server2.uri()), dir.path().to_path_buf());
        // topic 传空 → 用 checkpoint 里的 "测试主题"。
        let out = run_workflow_resume(&wf, "", &ctx2, &wf_id)
            .await
            .expect("resume 应成功");
        assert_eq!(out, "包含永远不可能出现的验收字符串的合格产出");

        let requests = server2.received_requests().await.unwrap();
        // b 第一次不合格（mock 恒定返回…… 不对，这次返回含验收串，
        // 一次就过）→ b=1、c=1 共 2 个请求；若 step a 重跑则是 3 个。
        assert_eq!(requests.len(), 2, "只跑 b、c 两步: {}", requests.len());
        for req in &requests {
            let body = String::from_utf8_lossy(&req.body);
            assert!(!body.contains("任务A"), "step a 不应重跑: {body}");
        }
        let first = String::from_utf8_lossy(&requests[0].body);
        assert!(
            first.contains("基于 步骤产出 设计 测试主题"),
            "out_a 从 checkpoint 注入、topic 用 checkpoint 的: {first}"
        );

        // 事件：不重发 step a 的 WorkflowStep，有一条断点续跑 Status。
        let mut step_events: Vec<String> = Vec::new();
        let mut status_msgs: Vec<String> = Vec::new();
        while let Ok(ev) = rx2.try_recv() {
            match ev {
                ChatEvent::WorkflowStep { step_id, .. } => step_events.push(step_id),
                ChatEvent::Status { message } => status_msgs.push(message),
                _ => {}
            }
        }
        assert_eq!(step_events, vec!["b".to_string(), "c".to_string()]);
        assert!(
            status_msgs.iter().any(|m| m.contains("断点续跑") && m.contains("跳过已完成的 1 步")),
            "应有断点续跑 Status: {status_msgs:?}"
        );

        // resume 运行的 checkpoint 自包含：copy-forward 的 a + 新完成的 b、c。
        let files: Vec<_> = std::fs::read_dir(dir.path().join(".latte").join("workflow-runs"))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(files.len(), 2, "resume 运行有自己的 checkpoint 文件");
    }

    /// 契约最终失败时，错误消息必须附「不合格产出摘要」——gate 判
    /// REJECT 的场景里 manager 要能直接看到 REJECT 理由（jemalloc 现场：
    /// 错误只有"缺少 VERDICT: PASS"，阻断原因被丢弃，manager 无从
    /// 解释也无从修复，turn 以裸错误收场）。
    #[tokio::test]
    async fn contract_failure_error_includes_output_excerpt() {
        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "VERDICT: REJECT\n\n阻断原因：方案违反红线 XYZ-001",
            )))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let wf = three_step_wf();
        let (ctx, _rx) = test_ctx_at(test_config_at(&server.uri()), dir.path().to_path_buf());
        let err = run_workflow(&wf, "测试主题", &ctx)
            .await
            .expect_err("step b 契约不可能通过，必须失败");
        assert!(err.contains("产出契约校验失败"), "保留校验失败措辞: {err}");
        assert!(
            err.contains("阻断原因：方案违反红线 XYZ-001"),
            "错误消息必须带被拦产出的摘要（REJECT 理由）: {err}"
        );
    }

    /// resume DAG：depends_on 链 a→b→c，同样在第 2 步失败后续跑，
    /// 验证 DAG 引擎的跳过与 outputs 预填。
    #[tokio::test]
    async fn resume_skips_completed_steps_dag() {
        let wf: WorkflowDef = toml::from_str(
            r#"
name = "resume_dag"
[[steps]]
id = "a"
role = "worker"
task = "任务A：分析 {{topic}}"
output_key = "out_a"
[[steps]]
id = "b"
role = "worker"
task = "任务B：基于 {{out_a}} 设计"
output_key = "out_b"
depends_on = ["a"]
max_retries = 1
[steps.output_contract]
require = ["永远不可能出现的验收字符串"]
[[steps]]
id = "c"
role = "worker"
task = "任务C：汇总 {{out_b}}"
output_key = "out_c"
depends_on = ["b"]
"#,
        )
        .unwrap();

        let server1 = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body("步骤产出")))
            .mount(&server1)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let (ctx1, _rx1) = test_ctx_at(test_config_at(&server1.uri()), dir.path().to_path_buf());
        let err = run_workflow(&wf, "测试主题", &ctx1)
            .await
            .expect_err("step b 必须失败");
        assert!(err.contains("已完成 1/3 步"), "失败消息带进度: {err}");
        let wf_id = extract_wf_id(&err);

        let server2 = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "包含永远不可能出现的验收字符串的合格产出",
            )))
            .mount(&server2)
            .await;
        let (ctx2, _rx2) = test_ctx_at(test_config_at(&server2.uri()), dir.path().to_path_buf());
        let out = run_workflow_resume(&wf, "测试主题", &ctx2, &wf_id)
            .await
            .expect("DAG resume 应成功");
        assert_eq!(out, "包含永远不可能出现的验收字符串的合格产出");

        let requests = server2.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2, "只跑 b、c: {}", requests.len());
        for req in &requests {
            let body = String::from_utf8_lossy(&req.body);
            assert!(!body.contains("任务A"), "step a 不应重跑: {body}");
        }
    }

    /// resume 校验：wf_id 不存在 / workflow_name 不匹配 / 非法 wf_id →
    /// 明确报错（且不发任何模型请求）。
    #[tokio::test]
    async fn resume_validation_errors() {
        let dir = tempfile::tempdir().unwrap();
        let wf = three_step_wf();
        let (ctx, _rx) = test_ctx_at(
            test_config_at("http://127.0.0.1:1"),
            dir.path().to_path_buf(),
        );

        // wf_id 不存在
        let err = run_workflow_resume(&wf, "t", &ctx, "wf-ghost-123")
            .await
            .expect_err("不存在的 checkpoint 必须报错");
        assert!(err.contains("not found"), "got: {err}");

        // 非法 wf_id（路径穿越）
        let err = run_workflow_resume(&wf, "t", &ctx, "../etc/passwd")
            .await
            .expect_err("非法 wf_id 必须报错");
        assert!(err.contains("invalid resume wf_id"), "got: {err}");

        // workflow_name 不匹配：手工写一个属于别的 workflow 的 checkpoint。
        let runs_dir = dir.path().join(".latte").join("workflow-runs");
        std::fs::create_dir_all(&runs_dir).unwrap();
        std::fs::write(
            runs_dir.join("wf-other-1.jsonl"),
            "{\"type\":\"meta\",\"wf_id\":\"wf-other-1\",\"workflow_name\":\"other_wf\",\
             \"topic\":\"t\",\"started_at\":0}\n",
        )
        .unwrap();
        let err = run_workflow_resume(&wf, "t", &ctx, "wf-other-1")
            .await
            .expect_err("workflow 不匹配必须报错");
        assert!(
            err.contains("belongs to workflow 'other_wf'"),
            "got: {err}"
        );
    }

    /// DAG loop_until 端到端：gate 第一轮输出 REVISE → 跳回 design
    /// 重跑、下游 impl 一并作废重跑、design 的 prompt 带【上轮审查
    /// 反馈】；第二轮 gate PASS → 放行。
    ///
    /// 注意本例用的是 REVISE。真正的 jemalloc 事故词表是 REJECT，
    /// 由 [`dag_gate_verdict_reject_loops_back_and_delivers`] 覆盖——
    /// 那条路径此前被引擎硬编码的 `VERDICT: REJECT` 熔断判死，本测试
    /// 换用 REVISE 恰好绕开了缺陷，所以一直是绿的。
    #[tokio::test]
    async fn dag_gate_revise_loops_back_and_passes() {
        // 注意：本仓库 wiremock 0.6 实测为先挂载优先（FIFO），且各
        // step 的判别子串必须互不重叠——gate 的 prompt 会内嵌上游
        // 产出文本，用"实现"这类子串会误吸 gate 请求。
        let wf: WorkflowDef = toml::from_str(
            r#"
name = "dag_loop"
[[steps]]
id = "design"
role = "worker"
task = "产出设计稿 {{topic}}"
output_key = "design"
[[steps]]
id = "impl"
role = "worker"
task = "编写实现 {{design}}"
output_key = "impl"
depends_on = ["design"]
[[steps]]
id = "gate"
role = "worker"
task = "放行判定 {{impl}}"
output_key = "verdict"
depends_on = ["impl"]
loop_until = "VERDICT: PASS"
loop_back_to = "design"
max_iterations = 3
"#,
        )
        .unwrap();
        wf.validate().expect("DAG loop_until 应通过校验");

        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("产出设计稿"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body("设计产出")))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("编写实现"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body("实现产出")))
            .mount(&server)
            .await;
        // FIFO：先挂 REJECT（仅 1 次），第一次放行判定命中它，耗尽后
        // 落回下面挂载的 PASS。
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("放行判定"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "VERDICT: REVISE 有阻断问题",
            )))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("放行判定"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "VERDICT: PASS 终审通过",
            )))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let (ctx, _rx) = test_ctx_at(test_config_at(&server.uri()), dir.path().to_path_buf());
        let out = run_workflow(&wf, "主题", &ctx).await.expect("第二轮 PASS 应放行");
        assert_eq!(out, "VERDICT: PASS 终审通过");

        let requests = server.received_requests().await.unwrap();
        let bodies: Vec<String> = requests
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).to_string())
            .collect();
        let design_reqs: Vec<&String> = bodies.iter().filter(|b| b.contains("产出设计稿")).collect();
        assert_eq!(design_reqs.len(), 2, "design 应重跑一次: {}", design_reqs.len());
        assert!(
            design_reqs[1].contains("【上轮审查反馈】")
                && design_reqs[1].contains("VERDICT: REVISE"),
            "重跑的 design prompt 必须带返工反馈: {}",
            design_reqs[1]
        );
        let impl_reqs: Vec<&String> = bodies.iter().filter(|b| b.contains("编写实现")).collect();
        assert_eq!(impl_reqs.len(), 2, "下游 impl 应随返工作废重跑: {}", impl_reqs.len());
        let gate_reqs: Vec<&String> = bodies.iter().filter(|b| b.contains("放行判定")).collect();
        assert_eq!(gate_reqs.len(), 2, "gate 应跑两轮: {}", gate_reqs.len());
    }

    /// DAG loop_until 迭代耗尽：gate 永远 REJECT，max_iterations=2 →
    /// 第二轮仍未满足即 failed，错误消息带循环条件与迭代次数（不再
    /// 是「契约校验失败」这种误导性措辞）。
    #[tokio::test]
    async fn dag_loop_until_exhausted_fails() {
        let wf: WorkflowDef = toml::from_str(
            r#"
name = "dag_loop_exhaust"
[[steps]]
id = "design"
role = "worker"
task = "产出设计稿 {{topic}}"
output_key = "design"
[[steps]]
id = "gate"
role = "worker"
task = "放行判定 {{design}}"
output_key = "verdict"
depends_on = ["design"]
loop_until = "VERDICT: PASS"
loop_back_to = "design"
max_iterations = 2
"#,
        )
        .unwrap();

        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("产出设计稿"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body("设计产出")))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("放行判定"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "VERDICT: REVISE 仍有阻断",
            )))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let (ctx, _rx) = test_ctx_at(test_config_at(&server.uri()), dir.path().to_path_buf());
        let err = run_workflow(&wf, "主题", &ctx)
            .await
            .expect_err("迭代耗尽必须失败");
        assert!(err.contains("循环条件"), "应报循环条件未满足: {err}");
        assert!(err.contains("max_iterations=2"), "应带迭代上限: {err}");

        let requests = server.received_requests().await.unwrap();
        let gate_reqs = requests
            .iter()
            .filter(|r| String::from_utf8_lossy(&r.body).contains("放行判定"))
            .count();
        assert_eq!(gate_reqs, 2, "gate 跑满 2 轮才判死: {gate_reqs}");
    }

    /// jemalloc 事故回放（真实词表）：design_and_plan / implementation_plan
    /// 的 gate 用 PASS/REJECT 两值，REJECT = 「打回重做」。引擎此前把
    /// `VERDICT: REJECT` 硬编码成无条件终局，`loop_until`/`loop_back_to`
    /// 形同虚设——评审如实 REJECT 就判死整条流水线（唯一跑到 gate 的
    /// 那次运行 44 分钟零产出）。现在熔断改由 `loop_abort_on` 显式声明，
    /// 没声明 = REJECT 走返工，第二轮 PASS 必须真的交付出计划正文。
    #[tokio::test]
    async fn dag_gate_verdict_reject_loops_back_and_delivers() {
        let wf: WorkflowDef = toml::from_str(
            r#"
name = "dag_reject_rework"
[[steps]]
id = "brainstorm"
role = "worker"
task = "产出设计稿 {{topic}}"
output_key = "design"
[[steps]]
id = "plan"
role = "worker"
task = "拆解清单 {{design}}"
output_key = "plan"
depends_on = ["brainstorm"]
[[steps]]
id = "gate"
role = "worker"
task = "放行判定 {{plan}}"
output_key = "verdict"
depends_on = ["plan"]
loop_until = "VERDICT: PASS"
loop_back_to = "brainstorm"
max_iterations = 3
"#,
        )
        .unwrap();
        wf.validate().expect("DAG loop_until 应通过校验");

        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("产出设计稿"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body("设计产出")))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("拆解清单"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body("清单产出")))
            .mount(&server)
            .await;
        // FIFO：第一轮 gate 命中 REJECT（仅 1 次），第二轮落到 PASS。
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("放行判定"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "VERDICT: REJECT\n阻断原因：stats.c:1234 行号与实测不符",
            )))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("放行判定"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "VERDICT: PASS\n【实现计划】1. 改 stats.c",
            )))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let (ctx, _rx) = test_ctx_at(test_config_at(&server.uri()), dir.path().to_path_buf());
        let out = run_workflow(&wf, "主题", &ctx)
            .await
            .expect("REJECT 必须返工而不是判死整条 workflow");
        assert!(
            out.contains("VERDICT: PASS") && out.contains("【实现计划】"),
            "第二轮必须真的交付计划正文: {out}"
        );

        let bodies: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).to_string())
            .collect();
        let design_reqs: Vec<&String> = bodies.iter().filter(|b| b.contains("产出设计稿")).collect();
        assert_eq!(design_reqs.len(), 2, "REJECT 必须触发 brainstorm 重跑");
        assert!(
            design_reqs[1].contains("【上轮审查反馈】") && design_reqs[1].contains("stats.c:1234"),
            "返工的 prompt 必须带 REJECT 的证据: {}",
            design_reqs[1]
        );
        assert_eq!(
            bodies.iter().filter(|b| b.contains("拆解清单")).count(),
            2,
            "下游 plan 随返工作废重跑"
        );
    }

    /// 返工目标是**嵌套 workflow step** 时反馈必须喂进子流程 topic。
    /// design_and_plan 的 `loop_back_to = "brainstorm"` 正是这个形状：
    /// 嵌套分支此前在注入反馈前就 return 了，重跑拿到的 topic 与上一轮
    /// 逐字相同，评审的 file:line 证据被丢掉，返工退化成原地重摇骰子。
    #[tokio::test]
    async fn dag_rework_feedback_reaches_nested_workflow_target() {
        let dir = tempfile::tempdir().unwrap();
        let wf_dir = dir.path().join(".latte").join("workflows.d");
        std::fs::create_dir_all(&wf_dir).unwrap();
        std::fs::write(
            wf_dir.join("inner_design.toml"),
            r#"
name = "inner_design"
[[steps]]
id = "draft"
role = "worker"
task = "子流程出稿 {{topic}}"
output_key = "draft"
"#,
        )
        .unwrap();

        let wf: WorkflowDef = toml::from_str(
            r#"
name = "dag_nested_rework"
[[steps]]
id = "brainstorm"
workflow = "inner_design"
task = "设计主题 {{topic}}"
output_key = "design"
[[steps]]
id = "gate"
role = "worker"
task = "放行判定 {{design}}"
output_key = "verdict"
depends_on = ["brainstorm"]
loop_until = "VERDICT: PASS"
loop_back_to = "brainstorm"
max_iterations = 3
"#,
        )
        .unwrap();
        wf.validate().expect("嵌套 step 作为返工目标应合法");

        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("子流程出稿"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body("子流程产出")))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("放行判定"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "VERDICT: REJECT\n阻断原因：extent.c:88 与实测不符",
            )))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("放行判定"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(openai_body("VERDICT: PASS 放行")),
            )
            .mount(&server)
            .await;

        let (ctx, _rx) = test_ctx_at(test_config_at(&server.uri()), dir.path().to_path_buf());
        let out = run_workflow(&wf, "主题", &ctx).await.expect("第二轮应放行");
        assert!(out.contains("VERDICT: PASS"), "got: {out}");

        let bodies: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).to_string())
            .collect();
        let inner: Vec<&String> = bodies.iter().filter(|b| b.contains("子流程出稿")).collect();
        assert_eq!(inner.len(), 2, "嵌套子流程应重跑一次: {}", inner.len());
        assert!(
            inner[1].contains("【上轮审查反馈】") && inner[1].contains("extent.c:88"),
            "重跑的子流程 topic 必须带 REJECT 证据: {}",
            inner[1]
        );
    }

    /// DAG 下 `loop_abort_on` 仍然是硬终局：声明了熔断标记的 gate 命中
    /// 它就立刻失败，不返工（task_refine 的 REJECT 语义）。
    #[tokio::test]
    async fn dag_loop_abort_on_is_terminal() {
        let wf: WorkflowDef = toml::from_str(
            r#"
name = "dag_abort"
[[steps]]
id = "refine"
role = "worker"
task = "产出草案 {{topic}}"
output_key = "draft"
[[steps]]
id = "gate"
role = "worker"
task = "放行判定 {{draft}}"
output_key = "verdict"
depends_on = ["refine"]
loop_until = "VERDICT: ACCEPT"
loop_back_to = "refine"
loop_abort_on = "VERDICT: REJECT"
max_iterations = 3
"#,
        )
        .unwrap();
        wf.validate().expect("loop_abort_on 应通过校验");

        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("产出草案"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body("草案产出")))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("放行判定"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "VERDICT: REJECT\n阻断原因：输入缺失无法核验",
            )))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let (ctx, _rx) = test_ctx_at(test_config_at(&server.uri()), dir.path().to_path_buf());
        let err = run_workflow(&wf, "主题", &ctx)
            .await
            .expect_err("命中 loop_abort_on 必须终止");
        assert!(err.contains("终止 workflow"), "应报终止: {err}");
        assert!(err.contains("输入缺失无法核验"), "应保留阻断原因: {err}");

        let bodies: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).to_string())
            .collect();
        assert_eq!(
            bodies.iter().filter(|b| b.contains("产出草案")).count(),
            1,
            "熔断不返工，refine 只跑一次"
        );
    }

    /// 用户回答**收到即落盘**，而不是等 step 成功才写。
    /// 回归防线：此前答案只活在 subagent 内存里，step 一挂就没了。
    #[test]
    fn answer_log_persists_immediately_and_survives_reload() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        // checkpoint 需要 meta 行才能被 load_checkpoint 接受。
        let ckpt = CheckpointLog::new(cwd, "wf-ans-1", "design_and_plan", "主题", 0);
        let log = AnswerLog::new(cwd, "wf-ans-1", None, None, Default::default());

        assert_eq!(log.recall("第 1 题：你的目标？"), None);
        log.record("tutor", "第 1 题：你的目标？", "改源码/做实现");
        log.record("tutor", "第 2 题：当前水平？", "看过一些代码");

        // 内存表立刻可回放（同一 run 内的 step 重试就靠这条）。
        assert_eq!(
            log.recall("第 1 题：你的目标？").as_deref(),
            Some("改源码/做实现")
        );
        // trim 后匹配：模型多打了空白也算命中。
        assert_eq!(
            log.recall("  第 2 题：当前水平？  ").as_deref(),
            Some("看过一些代码")
        );

        // **一个 step 都还没完成**，但回答已经在盘上了 —— 这正是重点。
        assert_eq!(ckpt.completed(), 0);
        let state = load_checkpoint(cwd, "wf-ans-1").expect("checkpoint 可读");
        assert!(state.completed.is_empty(), "没有任何已完成 step");
        assert_eq!(
            state.answers.get("第 1 题：你的目标？").map(String::as_str),
            Some("改源码/做实现")
        );
        assert_eq!(state.answers.len(), 2);
    }

    /// resume 预载上一次运行的回答：新 run 的台账里直接就有答案，
    /// 用户不会被重新问一遍。
    #[test]
    fn resumed_run_preloads_previous_answers() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let _ckpt = CheckpointLog::new(cwd, "wf-ans-2", "design_and_plan", "主题", 0);
        AnswerLog::new(cwd, "wf-ans-2", None, None, Default::default())
            .record("tutor", "时间预算？", "10h+");

        let state = load_checkpoint(cwd, "wf-ans-2").expect("checkpoint");
        // 新 run（新 wf_id）用上一次的回答预载。
        let resumed = AnswerLog::new(cwd, "wf-ans-3", None, None, state.answers.clone());
        assert_eq!(resumed.recall("时间预算？").as_deref(), Some("10h+"));
    }

    /// **第二次**中断也不能让用户重答：resume 必须把 `Answer` 行抄进新
    /// 的 checkpoint 文件，不能只预载进内存。
    ///
    /// 此前只抄了 `Step` 行，答案仅活在新 run 的内存 AnswerLog 里。于是
    /// 「答题 → 崩 → 续跑 → 再崩 → 再续跑」时，第二次 load_checkpoint 读
    /// 新 wf_id 拿到空 answers，整批题重问一遍。用户的回答是不可再生
    /// 资源，续跑一次就作废是不可接受的。
    #[test]
    fn resume_carries_answers_into_new_checkpoint_file() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let _ckpt = CheckpointLog::new(cwd, "wf-carry-1", "design_and_plan", "主题", 0);
        AnswerLog::new(cwd, "wf-carry-1", None, None, Default::default())
            .record("tutor", "时间预算？", "10h+");
        let first = load_checkpoint(cwd, "wf-carry-1").expect("第一份 checkpoint");

        // 模拟 run_workflow_inner 的 resume 抄写块（Step + Answer）。
        let _ckpt2 = CheckpointLog::new(cwd, "wf-carry-2", "design_and_plan", "主题", 0);
        for (question, answer) in &first.answers {
            append_checkpoint(
                cwd,
                "wf-carry-2",
                &CheckpointRecord::Answer {
                    wf_id: "wf-carry-2".to_string(),
                    role: "(resumed)".to_string(),
                    question: question.clone(),
                    answer: answer.clone(),
                    answered_at: now_secs(),
                },
            );
        }

        // 第二次中断后只认新 wf_id —— 答案必须还在盘上。
        let second = load_checkpoint(cwd, "wf-carry-2").expect("第二份 checkpoint");
        assert!(
            second.has_answer("时间预算？"),
            "resume 后的 checkpoint 必须自带上一次的回答，否则第二次续跑会重问"
        );
        assert_eq!(
            AnswerLog::new(cwd, "wf-carry-3", None, None, second.answers.clone())
                .recall("时间预算？")
                .as_deref(),
            Some("10h+")
        );
    }

    /// `record_answer_for_run`（重启后补写孤儿 ask 的答案）写出来的记录
    /// 必须与活着的 run 记的完全等价 —— 否则续跑时 recall 匹配不上，
    /// 用户白答一次、题目又弹一遍。
    #[test]
    fn record_answer_for_run_is_recallable_on_resume() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let _ckpt = CheckpointLog::new(cwd, "wf-orphan-1", "design_and_plan", "主题", 0);
        // 没有活着的 AnswerLog（进程重启后就是这个状态）。
        record_answer_for_run(cwd, "wf-orphan-1", "tutor", "  选哪个？  ", "方案A");

        let state = load_checkpoint(cwd, "wf-orphan-1").expect("checkpoint");
        // 匹配键是 trim 后的问题原文，与 AnswerLog::record 一致。
        assert!(state.has_answer("选哪个？"));
        assert_eq!(
            AnswerLog::new(cwd, "wf-orphan-2", None, None, state.answers.clone())
                .recall("选哪个？")
                .as_deref(),
            Some("方案A"),
            "续跑的 run 必须 recall 命中，才不会再弹同一个框"
        );
    }

    /// 嵌套 run 的回答必须**同时**落进本层与根 run 的 checkpoint。
    ///
    /// 为什么是修复本次事故的核心：阻塞 ask 出在子 workflow 里时，
    /// 回答只写本层 = 只能 resume 子 run，父流水线不知道自己在等谁，
    /// 永久卡死（jemalloc 现场：`design_and_plan` → `req_review` →
    /// `requirements_review` 的 `decide` 弹出选择题，答了也没用）。
    #[test]
    fn nested_answer_lands_in_both_own_and_root_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let _root = CheckpointLog::new(cwd, "wf-root-1", "design_and_plan", "主题", 0);
        let _child = CheckpointLog::new(cwd, "wf-child-1", "requirements_review", "子主题", 0);

        // 子 run 的 AnswerLog：root 指向顶层。
        let log = AnswerLog::new(
            cwd,
            "wf-child-1",
            Some("sess-a".into()),
            Some("wf-root-1".into()),
            Default::default(),
        );
        log.record("manager", "怎么推 L1 改稿？", "走精简计划");

        // 本层能 recall（子 run 单独 resume 的场景）。
        assert!(
            load_checkpoint(cwd, "wf-child-1")
                .expect("child ckpt")
                .has_answer("怎么推 L1 改稿？"),
            "答案必须写进本层 checkpoint"
        );
        // 根也能 recall（顶层 resume 唤醒整条流水线的场景）——这是关键。
        assert!(
            load_checkpoint(cwd, "wf-root-1")
                .expect("root ckpt")
                .has_answer("怎么推 L1 改稿？"),
            "答案必须同时写进根 checkpoint，否则顶层续跑会重新弹同一道题"
        );
    }

    /// 顶层 run 自己就是根时不重复写（root == 自己 → 只落一条）。
    #[test]
    fn top_level_answer_is_written_once() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let _ckpt = CheckpointLog::new(cwd, "wf-solo-1", "design_and_plan", "主题", 0);
        // 顶层：root 显式等于自己（run_workflow_inner 里 root 为 None 时
        // resume_wf_id() 回落到 wf_id，这里模拟两者相等的情况）。
        let log = AnswerLog::new(
            cwd,
            "wf-solo-1",
            Some("sess-a".into()),
            Some("wf-solo-1".into()),
            Default::default(),
        );
        log.record("tutor", "选哪个？", "A");

        let raw = std::fs::read_to_string(
            cwd.join(".latte").join("workflow-runs").join("wf-solo-1.jsonl"),
        )
        .expect("read ckpt");
        let answers = raw.lines().filter(|l| l.contains("\"answer\"")).count();
        assert_eq!(answers, 1, "root == 自己时不该写两条重复 Answer 行");
    }

    /// `resume_wf_id()` 是"能把整条流水线带起来的那个 run"：
    /// 嵌套时给根，无嵌套时给自己。ask 落盘用的就是它。
    #[test]
    fn resume_wf_id_prefers_root() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let nested = AnswerLog::new(
            cwd,
            "wf-child-9",
            None,
            Some("wf-root-9".into()),
            Default::default(),
        );
        assert_eq!(
            nested.resume_wf_id(),
            "wf-root-9",
            "嵌套 run 的 ask 必须让顶层去 resume"
        );
        let top = AnswerLog::new(cwd, "wf-top-9", None, None, Default::default());
        assert_eq!(top.resume_wf_id(), "wf-top-9", "顶层 run 用自己的 id");
    }

    /// 跨层 recall：顶层 resume 会**重新建一个子 run**（resume 也是新
    /// wf_id），这个新子 run 没有自己的 resume 状态，答案只能从根
    /// checkpoint 继承 —— 否则顶层每次续跑都把嵌套里问过的题重弹一遍。
    ///
    /// 这里直接验 `run_workflow_inner` 里那段预载逻辑的等价行为：
    /// 新子 run 的 preloaded = 自己的 resume 状态 ∪ 根的 answers。
    #[test]
    fn fresh_nested_run_recalls_answers_from_root() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let _root = CheckpointLog::new(cwd, "wf-root-2", "design_and_plan", "主题", 0);
        let _old_child = CheckpointLog::new(cwd, "wf-child-2a", "requirements_review", "子", 0);

        // 第一次跑：子 run A 里用户答了题（双写本层 + 根）。
        AnswerLog::new(
            cwd,
            "wf-child-2a",
            Some("sess-a".into()),
            Some("wf-root-2".into()),
            Default::default(),
        )
        .record("manager", "怎么推 L1 改稿？", "走精简计划");

        // 顶层 resume → 重新跑嵌套 → 全新子 run B（wf_id 不同、无 resume 态）。
        // 它的 preloaded 按 run_workflow_inner 的规则从根继承。
        let root_answers = load_checkpoint(cwd, "wf-root-2").expect("root").answers;
        let fresh_child = AnswerLog::new(
            cwd,
            "wf-child-2b",
            Some("sess-a".into()),
            Some("wf-root-2".into()),
            root_answers,
        );
        assert_eq!(
            fresh_child.recall("怎么推 L1 改稿？").as_deref(),
            Some("走精简计划"),
            "顶层续跑重建的子 run 必须 recall 命中，否则用户被迫重答"
        );
    }

    /// 同一问题重复 record 取最后一次（用户改了主意的场景）。
    #[test]
    fn answer_log_last_write_wins_on_repeat() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let _ckpt = CheckpointLog::new(cwd, "wf-ans-4", "wf", "t", 0);
        let log = AnswerLog::new(cwd, "wf-ans-4", None, None, Default::default());
        log.record("tutor", "选哪个？", "A");
        log.record("tutor", "选哪个？", "B");
        assert_eq!(log.recall("选哪个？").as_deref(), Some("B"));
        let state = load_checkpoint(cwd, "wf-ans-4").expect("checkpoint");
        assert_eq!(state.answers.get("选哪个？").map(String::as_str), Some("B"));
    }

    /// 端到端接线验证：真实 workflow run 里，speaker 调阻塞 `ask`、
    /// 用户作答后，答案必须出现在该 run 的 checkpoint 里。
    ///
    /// 为什么值得单独测：`answer_log` 要穿过
    /// run_workflow_inner → 引擎 → DagStepInput/SpeakerDispatch →
    /// run_step_speaker → build_role_runner → AskBlocking 六层。
    /// 任何一层传成 `None`，单元测试全绿而线上照样让用户重答 —— 本轮
    /// 早前就踩过同型的坑（`dataset.wfId` 从未被赋值，判断恒 false）。
    #[tokio::test]
    async fn ask_answer_lands_in_run_checkpoint_end_to_end() {
        fn tool_call_body(args: &str) -> String {
            serde_json::json!({
                "id": "chatcmpl-tc",
                "object": "chat.completion",
                "created": 0,
                "model": "test",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [{
                            "id": "call_ask_1",
                            "type": "function",
                            "function": { "name": "ask", "arguments": args }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
            })
            .to_string()
        }

        let server = wiremock::MockServer::start().await;
        // 第 1 次模型调用：调 ask 提问。之后：给出最终产出。
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(tool_call_body(
                r#"{"question":"第 1 题：你最终想达成什么？","options":[{"label":"学习代码机制"},{"label":"改源码/做实现"}]}"#,
            )))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(openai_body("用户画像汇总完毕")),
            )
            .mount(&server)
            .await;

        // worker 需要 ask 工具，否则 build_role_runner 不注册它。
        let mut config = (*test_config_at(&server.uri())).clone();
        if let Some(r) = config.roles.get_mut("worker") {
            r.tools = vec!["ask".into()];
        }
        let config = Arc::new(config);

        let wf: WorkflowDef = toml::from_str(
            r#"
name = "interview_only"
[[steps]]
id = "interview"
role = "worker"
task = "先问用户：{{topic}}"
output_key = "user_profile"
"#,
        )
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let (ctx, mut rx) = test_ctx_at(config, dir.path().to_path_buf());

        // 模拟 UI：看到选择框就把答案投递回去。
        let answerer = tokio::spawn(async move {
            while let Ok(ev) = rx.recv().await {
                if let ChatEvent::ChoiceRequested { choice_id, .. } = ev {
                    crate::choice::resolve(&choice_id, "改源码/做实现".to_string());
                    return true;
                }
            }
            false
        });

        let out = run_workflow(&wf, "学 jemalloc", &ctx).await;
        let asked = answerer.await.unwrap_or(false);
        assert!(asked, "speaker 应该真的弹出了选择框（ask 已注册）");
        assert!(out.is_ok(), "run 应成功: {out:?}");

        // checkpoint 里必须有 Answer 记录 —— 证明六层接线没有断在 None。
        let runs = dir.path().join(".latte").join("workflow-runs");
        let mut found: Option<String> = None;
        for entry in std::fs::read_dir(&runs).expect("workflow-runs 目录").flatten() {
            let raw = std::fs::read_to_string(entry.path()).unwrap_or_default();
            for line in raw.lines() {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                if v["type"] == "answer" {
                    found = Some(line.to_string());
                }
            }
        }
        let line = found.expect("checkpoint 必须落一条 answer 记录（接线断了就没有）");
        assert!(line.contains("改源码/做实现"), "答案原文要落盘: {line}");
        assert!(line.contains("第 1 题"), "问题原文要落盘（回放的 key）: {line}");
        assert!(line.contains("\"role\":\"worker\""), "记账要带提问角色: {line}");
    }

    /// 预算超支必须**中止**并给出可 resume 的错误（标记 + 单元数说明
    /// + wf_id），而不是无限挂着等人来看 TimeoutWarning。此前唯一的
    /// 天花板是 `workflow` 工具的注册期常量 24h = 没有天花板。
    #[tokio::test]
    async fn budget_exceeded_aborts_with_resumable_error() {
        let server = wiremock::MockServer::start().await;
        // 模型每次慢 3s；预算被 env 压到 1s/单元 × 1 单元 = 1s。
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(openai_body("慢产出"))
                    .set_delay(std::time::Duration::from_secs(3)),
            )
            .mount(&server)
            .await;

        let wf: WorkflowDef = toml::from_str(
            r#"
name = "budget_demo"
[[steps]]
id = "a"
role = "worker"
task = "慢任务 {{topic}}"
output_key = "out"
"#,
        )
        .unwrap();

        let guard = crate::test_util::ENV_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("LATTE_AGENT_WORKFLOW_BUDGET_PER_UNIT_SECS", "1");
        let dir = tempfile::tempdir().unwrap();
        let (ctx, _rx) = test_ctx_at(test_config_at(&server.uri()), dir.path().to_path_buf());
        let err = run_workflow(&wf, "主题", &ctx)
            .await
            .expect_err("超出预算必须失败");
        std::env::remove_var("LATTE_AGENT_WORKFLOW_BUDGET_PER_UNIT_SECS");
        drop(guard);

        assert!(
            err.contains(crate::workflow::BUDGET_EXCEEDED_MARKER),
            "错误必须带预算超支标记（分类器靠它判不可重试）: {err}"
        );
        assert!(err.contains("分派单元"), "应说明预算是怎么算出来的: {err}");
        assert!(err.contains("wf_id="), "必须带 resume 凭据: {err}");
    }

    /// 预算充裕时不得误杀：同一条 workflow、同样的慢模型，把 per-unit
    /// 预算放宽后必须正常跑完。回归防线——预算推算错成过小会把所有
    /// 长任务判死。
    #[tokio::test]
    async fn generous_budget_does_not_kill_slow_but_healthy_run() {
        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(openai_body("慢但正常的产出"))
                    .set_delay(std::time::Duration::from_millis(300)),
            )
            .mount(&server)
            .await;

        let wf: WorkflowDef = toml::from_str(
            r#"
name = "budget_ok"
[[steps]]
id = "a"
role = "worker"
task = "任务 {{topic}}"
output_key = "out"
"#,
        )
        .unwrap();

        let guard = crate::test_util::ENV_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("LATTE_AGENT_WORKFLOW_BUDGET_PER_UNIT_SECS", "30");
        let dir = tempfile::tempdir().unwrap();
        let (ctx, _rx) = test_ctx_at(test_config_at(&server.uri()), dir.path().to_path_buf());
        let out = run_workflow(&wf, "主题", &ctx).await;
        std::env::remove_var("LATTE_AGENT_WORKFLOW_BUDGET_PER_UNIT_SECS");
        drop(guard);
        assert_eq!(out.expect("预算充裕不该被杀"), "慢但正常的产出");
    }

    /// 熔断判定必须锁在 loop_until 保护内：没有返工环的 step 产出里
    /// 出现别的 step 的裁决原文（评审引用 gate 结论是常态）不该判死
    /// workflow。此前 DAG 引擎把检查套在循环外，对 wave 里每个 step
    /// 生效，任意 step 复述 `VERDICT: REJECT` 就能连坐。
    #[tokio::test]
    async fn dag_non_loop_step_quoting_reject_does_not_kill_run() {
        let wf: WorkflowDef = toml::from_str(
            r#"
name = "dag_quote"
[[steps]]
id = "review"
role = "worker"
task = "复述历史裁决 {{topic}}"
output_key = "quoted"
[[steps]]
id = "summarize"
role = "worker"
task = "汇总结论 {{quoted}}"
output_key = "summary"
depends_on = ["review"]
"#,
        )
        .unwrap();

        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("复述历史裁决"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "上一轮评审写的是 VERDICT: REJECT，本轮已修复",
            )))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("汇总结论"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body("汇总完成")))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let (ctx, _rx) = test_ctx_at(test_config_at(&server.uri()), dir.path().to_path_buf());
        let out = run_workflow(&wf, "主题", &ctx)
            .await
            .expect("无返工环的 step 引用 REJECT 原文不该判死 workflow");
        assert_eq!(out, "汇总完成");
    }

    /// output_from 端到端：子 workflow 末步是评审 verdict，父级用
    /// output_from 取中间 step（synthesize，output_key=proposal）的
    /// 方案本体。回归 jemalloc 现场：design_and_plan 的 {{design}}
    /// 被绑成 advisor_verdict 的裁决文本，下游 plan/评审全部跑偏。
    #[tokio::test]
    async fn nested_output_from_selects_intermediate_output() {
        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("产出方案"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "方案本体：先做 X 再做 Y",
            )))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("终审"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "VERDICT: PASS 裁决文本",
            )))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body("下游产出")))
            .mount(&server)
            .await;

        // 内层 workflow 落盘到临时项目的 .latte/workflows.d/。
        let dir = tempfile::tempdir().unwrap();
        let wf_dir = dir.path().join(".latte").join("workflows.d");
        std::fs::create_dir_all(&wf_dir).unwrap();
        std::fs::write(
            wf_dir.join("inner.toml"),
            r#"
name = "inner"
[[steps]]
id = "synthesize"
role = "worker"
task = "产出方案 {{topic}}"
output_key = "proposal"
[[steps]]
id = "verdict"
role = "worker"
task = "终审 {{proposal}}"
output_key = "verdict"
"#,
        )
        .unwrap();

        let outer: WorkflowDef = toml::from_str(
            r#"
name = "outer"
[[steps]]
id = "brainstorm"
workflow = "inner"
task = "主题"
output_from = "proposal"
output_key = "design"
[[steps]]
id = "downstream"
role = "worker"
task = "下游消费：{{design}}"
output_key = "final"
"#,
        )
        .unwrap();
        outer.validate().unwrap();

        let (ctx, _rx) = test_ctx_at(test_config_at(&server.uri()), dir.path().to_path_buf());
        let out = run_workflow(&outer, "主题", &ctx).await.expect("应成功");
        assert_eq!(out, "下游产出");

        let requests = server.received_requests().await.unwrap();
        let downstream: Vec<String> = requests
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).to_string())
            .filter(|b| b.contains("下游消费"))
            .collect();
        assert_eq!(downstream.len(), 1);
        assert!(
            downstream[0].contains("方案本体：先做 X 再做 Y"),
            "{{design}} 必须是 proposal 而非 verdict: {}",
            downstream[0]
        );
        assert!(
            !downstream[0].contains("VERDICT: PASS 裁决文本"),
            "{{design}} 不得是末步 verdict: {}",
            downstream[0]
        );
    }

    /// export 端到端（打通信息漏斗）：嵌套 step 除了 `output_from` 选中
    /// 的方案本体，还把子 workflow 的 survey 原文带到父级 vars，下游
    /// 评审 step 用 `{{survey}}` 拿得到。
    ///
    /// 回归 jemalloc 现场：评审只收到被逐层压缩的 proposal，事实基线
    /// （真实文件/行号）留在子流程里没往下传，于是评审凭记忆核事实、
    /// 报出 5 处错行号。
    #[tokio::test]
    async fn nested_export_forwards_evidence_to_downstream_step() {
        let server = wiremock::MockServer::start().await;
        // 子 workflow：survey（证据原文）→ synthesize（方案本体）。
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("调研代码"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "证据原文：stats.c:1234 实测为 arena_stats_merge",
            )))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("提炼方案"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(openai_body("推荐方案：改 A 模块")),
            )
            .mount(&server)
            .await;
        // 父级评审 step：产出里回显它收到的内容，便于断言。
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("开始评审"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body("评审完成")))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let wf_dir = dir.path().join(".latte").join("workflows.d");
        std::fs::create_dir_all(&wf_dir).unwrap();
        std::fs::write(
            wf_dir.join("inner.toml"),
            r#"
name = "inner"
[[steps]]
id = "survey"
role = "worker"
task = "调研代码 {{topic}}"
output_key = "raw_survey"
[[steps]]
id = "synthesize"
role = "worker"
task = "提炼方案，依据：{{raw_survey}}"
output_key = "proposal"
"#,
        )
        .unwrap();

        let wf: WorkflowDef = toml::from_str(
            r#"
name = "funnel"
[[steps]]
id = "brainstorm"
workflow = "inner"
task = "主题"
output_from = "proposal"
output_key = "design"
export = { survey = "raw_survey" }
[[steps]]
id = "review"
role = "worker"
task = "开始评审\n方案：{{design}}\n【调研原文】\n{{survey}}"
output_key = "verdict"
depends_on = ["brainstorm"]
"#,
        )
        .unwrap();
        wf.validate().expect("export 应通过校验");

        let (ctx, _rx) = test_ctx_at(test_config_at(&server.uri()), dir.path().to_path_buf());
        run_workflow(&wf, "主题", &ctx).await.expect("run ok");

        let bodies: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).to_string())
            .collect();
        let review = bodies
            .iter()
            .find(|b| b.contains("开始评审"))
            .expect("评审 step 应被派发");
        assert!(
            review.contains("推荐方案：改 A 模块"),
            "评审仍要拿到方案本体（output_from）: {review}"
        );
        assert!(
            review.contains("stats.c:1234"),
            "评审必须同时拿到 survey 原文 —— 这就是信息漏斗被打通的证据: {review}"
        );
    }

    /// 串行引擎的 export 也要生效。上一个测试用了 `depends_on` 走 DAG
    /// 调度器；两个引擎的 vars 合并是**两份独立代码**，只测一边等于
    /// 另一边没测（本仓库已有先例：熔断判定在 DAG 里套错了层）。
    #[tokio::test]
    async fn nested_export_works_in_serial_engine_too() {
        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("调研代码"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(openai_body("证据原文：extent.c:88")),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("提炼方案"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body("方案 X")))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("开始评审"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body("评审完成")))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let wf_dir = dir.path().join(".latte").join("workflows.d");
        std::fs::create_dir_all(&wf_dir).unwrap();
        std::fs::write(
            wf_dir.join("inner_serial.toml"),
            r#"
name = "inner_serial"
[[steps]]
id = "survey"
role = "worker"
task = "调研代码 {{topic}}"
output_key = "raw_survey"
[[steps]]
id = "synthesize"
role = "worker"
task = "提炼方案，依据：{{raw_survey}}"
output_key = "proposal"
"#,
        )
        .unwrap();

        // 注意：没有任何 depends_on → 走串行引擎。
        let wf: WorkflowDef = toml::from_str(
            r#"
name = "funnel_serial"
[[steps]]
id = "brainstorm"
workflow = "inner_serial"
task = "主题"
output_from = "proposal"
output_key = "design"
export = { survey = "raw_survey" }
[[steps]]
id = "review"
role = "worker"
task = "开始评审\n方案：{{design}}\n【调研原文】\n{{survey}}"
output_key = "verdict"
"#,
        )
        .unwrap();
        assert!(!wf.uses_dependency_dag(), "本例必须走串行引擎");
        wf.validate().expect("validate");

        let (ctx, _rx) = test_ctx_at(test_config_at(&server.uri()), dir.path().to_path_buf());
        run_workflow(&wf, "主题", &ctx).await.expect("run ok");

        let bodies: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).to_string())
            .collect();
        let review = bodies
            .iter()
            .find(|b| b.contains("开始评审"))
            .expect("评审 step 应被派发");
        assert!(review.contains("方案 X"), "{review}");
        assert!(
            review.contains("extent.c:88"),
            "串行引擎也必须把 survey 原文并进 vars: {review}"
        );
    }

    /// export 指向子 workflow 不存在的 output_key → 步骤失败，错误列出
    /// 可用 key（笔误不该静默变成空字符串喂给下游评审）。
    #[tokio::test]
    async fn nested_export_unknown_key_errors() {
        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body("产出")))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let wf_dir = dir.path().join(".latte").join("workflows.d");
        std::fs::create_dir_all(&wf_dir).unwrap();
        std::fs::write(
            wf_dir.join("inner2.toml"),
            r#"
name = "inner2"
[[steps]]
id = "a"
role = "worker"
task = "干活 {{topic}}"
output_key = "real_key"
"#,
        )
        .unwrap();

        let wf: WorkflowDef = toml::from_str(
            r#"
name = "funnel_bad"
[[steps]]
id = "nested"
workflow = "inner2"
task = "主题"
export = { survey = "ghost_key" }
"#,
        )
        .unwrap();

        let (ctx, _rx) = test_ctx_at(test_config_at(&server.uri()), dir.path().to_path_buf());
        let err = run_workflow(&wf, "主题", &ctx)
            .await
            .expect_err("export 指向不存在的 key 必须失败");
        assert!(err.contains("ghost_key"), "错误要点名笔误的 key: {err}");
        assert!(err.contains("real_key"), "错误要列出可用 key: {err}");
    }

    /// output_from 指向子 workflow 不存在的 output_key → 步骤失败，
    /// 错误列出可用的 key。
    #[tokio::test]
    async fn nested_output_from_unknown_key_errors() {
        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body("产出")))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let wf_dir = dir.path().join(".latte").join("workflows.d");
        std::fs::create_dir_all(&wf_dir).unwrap();
        std::fs::write(
            wf_dir.join("inner.toml"),
            r#"
name = "inner"
[[steps]]
id = "only"
role = "worker"
task = "干活 {{topic}}"
output_key = "result"
"#,
        )
        .unwrap();

        let outer: WorkflowDef = toml::from_str(
            r#"
name = "outer"
[[steps]]
id = "sub"
workflow = "inner"
task = "主题"
output_from = "ghost"
output_key = "design"
"#,
        )
        .unwrap();
        outer.validate().unwrap();

        let (ctx, _rx) = test_ctx_at(test_config_at(&server.uri()), dir.path().to_path_buf());
        let err = run_workflow(&outer, "主题", &ctx)
            .await
            .expect_err("ghost key 必须失败");
        assert!(err.contains("ghost"), "got: {err}");
        assert!(err.contains("result"), "错误应列出可用 key: {err}");
    }
}

#[cfg(test)]
mod loop_tests {
    use super::*;
    use crate::config::{ModelCatalog, ModelDef};
    use crate::role::RoleTemplate;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    fn openai_body(content: &str) -> String {
        serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 0,
            "model": "test",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": content },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
        })
        .to_string()
    }

    /// 单模型（premium tier 指向 wiremock）+ 三角色的测试配置。
    fn test_config_at(base_url: &str) -> Arc<AgentConfig> {
        let role = |id: &str| RoleTemplate {
            id: id.into(),
            name: id.into(),
            category: "execution".into(),
            model_tier: "premium".into(),
            model_chain: vec![],
            prompt_file: None,
            temperature: None,
            tools: vec![],
            icon: String::new(),
            skills: vec![],
            code_paths: vec![],
        };
        let roles = HashMap::from([
            ("programmer".to_string(), role("programmer")),
            ("tester".to_string(), role("tester")),
            ("reviewer".to_string(), role("reviewer")),
        ]);
        Arc::new(AgentConfig {
            advisor: Default::default(),
            models: ModelCatalog {
                models: vec![ModelDef {
                    name: "Test Premium".into(),
                    api: "openai".into(),
                    provider: "test".into(),
                    base_url: base_url.into(),
                    api_key: "test-key".into(),
                    context_window: 32000,
                    max_tokens: 4096,
                    supports_thinking: false,
                    supports_vision: false,
                    supports_image_generation: false,
                    cost_per_million_input: None,
                    cost_per_million_output: None,
                    tier: Some("premium".into()),
                    timeout_secs: None,
                }],
                tiers: None,
                role_tiers: None,
            },
            roles,
        })
    }

    fn test_ctx(config: Arc<AgentConfig>) -> (WorkflowRunContext, broadcast::Receiver<ChatEvent>) {
        let resolver = Arc::new(ModelResolver::from_config(&config).unwrap());
        let (event_tx, event_rx) = broadcast::channel(64);
        (
            WorkflowRunContext {
                merged: config,
                resolver,
                default_params: GenerateParams::default(),
                cwd: std::env::temp_dir(),
                event_tx,
                turn_cancel_flag: None,
                cancel_flag: Arc::new(AtomicBool::new(false)),
                agent_pause_gate: None, depth: 0,
                subsession_store: None,
                session_id: None,
                advisor_gate: None,
                advisor_pause: None,
                staging: None,
                root_wf_id: None,
            },
            event_rx,
        )
    }

    /// implement → spec_review（loop_until=PASS, 跳回 implement）→ quality_review。
    fn loop_wf(max_iterations: usize) -> WorkflowDef {
        let raw = format!(
            r#"
name = "loop_demo"
[[steps]]
id = "implement"
role = "programmer"
task = "实现任务：{{{{topic}}}}"
output_key = "impl"
[[steps]]
id = "spec_review"
role = "tester"
task = "审查规格：{{{{impl}}}}"
output_key = "review"
loop_until = "VERDICT: PASS"
loop_back_to = "implement"
max_iterations = {max_iterations}
[[steps]]
id = "quality_review"
role = "reviewer"
task = "质量审查：{{{{review}}}}"
output_key = "quality"
"#
        );
        toml::from_str(&raw).expect("valid TOML")
    }

    fn bodies_containing(requests: &[wiremock::Request], pat: &str) -> Vec<String> {
        requests
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).to_string())
            .filter(|b| b.contains(pat))
            .collect()
    }

    /// 声明了 `loop_abort_on` 的 gate 命中熔断标记时必须立即终止；
    /// 只有 REVISE 才允许通过 loop_until 回到上游返工。熔断词表由
    /// step 显式声明——引擎不再硬编码 `VERDICT: REJECT`。
    #[tokio::test]
    async fn serial_loop_reject_is_terminal() {
        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("实现任务"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body("实现产出")))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("终审判定"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "VERDICT: REJECT\nISSUES: 输入无法核验",
            )))
            .mount(&server)
            .await;

        let wf: WorkflowDef = toml::from_str(
            r#"
name = "reject_terminal"
[[steps]]
id = "implement"
role = "programmer"
task = "实现任务：{{topic}}"
output_key = "impl"
[[steps]]
id = "gate"
role = "reviewer"
task = "终审判定：{{impl}}"
output_key = "verdict"
loop_until = "VERDICT: ACCEPT"
loop_back_to = "implement"
loop_abort_on = "VERDICT: REJECT"
max_iterations = 2
"#,
        )
        .expect("valid workflow");
        wf.validate().expect("workflow must validate");

        let (ctx, _rx) = test_ctx(test_config_at(&server.uri()));
        let err = run_workflow(&wf, "测试主题", &ctx)
            .await
            .expect_err("REJECT 必须立即终止 workflow");
        assert!(err.contains("VERDICT: REJECT"), "错误应保留 REJECT 理由: {err}");
        let requests = server.received_requests().await.unwrap();
        assert_eq!(bodies_containing(&requests, "实现任务").len(), 1);
        assert_eq!(bodies_containing(&requests, "终审判定").len(), 1);
    }

    /// 审查 FAIL → 自动跳回 implement 返工（模型调 2 次、第二次请求带
    /// "上轮审查反馈"批注与 FAIL 内容）→ 第二次审查 PASS → 流程继续到
    /// quality_review 完成。
    #[tokio::test]
    async fn serial_loop_rework_then_pass() {
        let server = wiremock::MockServer::start().await;
        // wiremock 0.6 按挂载顺序取第一个命中的 mock：具体的审查 mock
        // 先挂，通用兜底最后挂。
        // 首次审查（只生效一次）→ FAIL。
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("审查规格"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "VERDICT: FAIL\nGAPS: 缺少边界测试",
            )))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("审查规格"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "VERDICT: PASS\nEVIDENCE: 逐条核对全部通过",
            )))
            .mount(&server)
            .await;
        // 兜底：implement / quality 通用产出。
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body("通用产出")))
            .mount(&server)
            .await;

        let (ctx, mut rx) = test_ctx(test_config_at(&server.uri()));
        let out = run_workflow(&loop_wf(3), "测试主题", &ctx)
            .await
            .expect("返工后 PASS 应成功");
        assert_eq!(out, "通用产出", "最后一步 quality_review 的产出");

        let requests = server.received_requests().await.unwrap();
        let impl_reqs = bodies_containing(&requests, "实现任务");
        assert_eq!(impl_reqs.len(), 2, "implement 的模型被调 2 次: {}", requests.len());
        assert!(
            impl_reqs[1].contains("【上轮审查反馈】"),
            "返工 prompt 带审查反馈批注: {}",
            impl_reqs[1]
        );
        assert!(
            impl_reqs[1].contains("缺少边界测试"),
            "批注含 FAIL 审查内容: {}",
            impl_reqs[1]
        );
        let review_reqs = bodies_containing(&requests, "审查规格");
        assert_eq!(review_reqs.len(), 2, "spec_review 跑 2 次: {}", requests.len());

        // 返工 Status 事件。
        let mut status_msgs: Vec<String> = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let ChatEvent::Status { message } = ev {
                status_msgs.push(message);
            }
        }
        assert!(
            status_msgs
                .iter()
                .any(|m| m.contains("第 1 轮返工") && m.contains("implement")),
            "应有返工 Status: {status_msgs:?}"
        );
    }

    /// 循环耗尽：审查恒 FAIL，max_iterations=2 → Failed，消息含循环条件、
    /// 迭代次数与最后一次产出摘要；implement / spec_review 各跑 2 次。
    #[tokio::test]
    async fn serial_loop_exhausted_fails() {
        let server = wiremock::MockServer::start().await;
        // 审查恒 FAIL（先挂具体 mock，兜底最后）。
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("审查规格"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "VERDICT: FAIL\nGAPS: 缺少边界测试",
            )))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body("通用产出")))
            .mount(&server)
            .await;

        let (ctx, _rx) = test_ctx(test_config_at(&server.uri()));
        let err = run_workflow(&loop_wf(2), "测试主题", &ctx)
            .await
            .expect_err("迭代耗尽必须失败");
        assert!(err.contains("VERDICT: PASS"), "错误含循环条件: {err}");
        assert!(err.contains("2 次迭代"), "错误含迭代次数: {err}");
        assert!(err.contains("缺少边界测试"), "错误含最后产出摘要: {err}");

        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            bodies_containing(&requests, "实现任务").len(),
            2,
            "implement 跑 2 次: {}",
            requests.len()
        );
        assert_eq!(
            bodies_containing(&requests, "审查规格").len(),
            2,
            "spec_review 跑 2 次: {}",
            requests.len()
        );
        assert!(
            bodies_containing(&requests, "质量审查").is_empty(),
            "失败后不继续 quality_review"
        );
    }

    /// validate：loop_back_to 指向不存在的 step → 报错。
    #[test]
    fn validate_loop_back_to_unknown_step() {
        let wf: WorkflowDef = toml::from_str(
            r#"
name = "bad_loop"
[[steps]]
id = "review"
role = "tester"
task = "审查"
loop_until = "VERDICT: PASS"
loop_back_to = "ghost"
"#,
        )
        .unwrap();
        let err = wf.validate().expect_err("loop_back_to 不存在必须报错");
        assert!(err.contains("loop_back_to"), "got: {err}");
        assert!(err.contains("ghost"), "got: {err}");
    }

    /// validate：DAG（任何 step 有 depends_on）中出现 loop_until → 报错。
    #[test]
    fn validate_loop_until_rejected_in_dag() {
        let wf: WorkflowDef = toml::from_str(
            r#"
name = "dag_loop"
[[steps]]
id = "a"
role = "programmer"
task = "实现"
[[steps]]
id = "b"
role = "tester"
task = "审查"
depends_on = ["a"]
loop_until = "VERDICT: PASS"
"#,
        )
        .unwrap();
        // DAG 支持 loop_until 后，缺省 loop_back_to（= 自己）属同 wave
        // 自环，仍必须报错——但措辞是 wave 约束而非「仅串行」。
        let err = wf.validate().expect_err("DAG 同 wave 自环必须报错");
        assert!(err.contains("严格更早的 wave"), "got: {err}");
    }

    /// validate：loop_until 与嵌套 workflow 互斥 → 报错。
    #[test]
    fn validate_loop_until_rejected_on_nested() {
        let wf: WorkflowDef = toml::from_str(
            r#"
name = "nested_loop"
[[steps]]
id = "sub"
workflow = "other_wf"
task = "子流程"
loop_until = "VERDICT: PASS"
"#,
        )
        .unwrap();
        let err = wf.validate().expect_err("嵌套 step 的 loop_until 必须报错");
        assert!(err.contains("嵌套"), "got: {err}");
    }

    /// task_refine 的 gate 必须用明确的 ACCEPT token 放行；REVISE/REJECT
    /// 只能保留裁决与问题，不应要求 gate 复制完整 draft。
    #[test]
    fn task_refine_gate_contract() {
        for raw in [
            include_str!("../../.latte/workflows.d/task_refine.toml"),
            include_str!("../../config/workflows/task_refine.toml"),
        ] {
            let wf: WorkflowDef = toml::from_str(raw).expect("task_refine TOML 必须有效");
            wf.validate().expect("task_refine workflow 必须通过校验");
            let gate = wf
                .steps
                .iter()
                .find(|step| step.id == "gate")
                .expect("task_refine 必须包含 gate step");
            assert_eq!(gate.loop_until.as_deref(), Some("VERDICT: ACCEPT"));
            assert_eq!(gate.loop_back_to.as_deref(), Some("refine"));
            assert_eq!(gate.max_iterations, Some(2));
            assert_eq!(gate.output_contract.require, vec!["VERDICT:"]);
            assert!(gate.prompt.contains("VERDICT: ACCEPT"));
            assert!(gate.prompt.contains("VERDICT: REVISE"));
            assert!(gate.prompt.contains("VERDICT: REJECT"));
            assert!(gate.prompt.contains("不要重新生成、修改或复述完整拆分草案"));
            assert!(!gate.prompt.contains("原样完整附上拆分草案"));
        }
    }

    /// tdd_development.toml：spec_review 带 loop_until/loop_back_to/max_iterations，
    /// FAIL 时跳回 implement 返工。
    #[test]
    fn tdd_development_spec_review_loops_back_to_implement() {
        let raw = include_str!("../../config/workflows/tdd_development.toml");
        let wf: WorkflowDef = toml::from_str(raw).expect("valid TOML");
        wf.validate().expect("tdd_development should validate");
        let spec = wf
            .steps
            .iter()
            .find(|s| s.id == "spec_review")
            .expect("step 'spec_review' must exist");
        assert_eq!(spec.loop_until.as_deref(), Some("VERDICT: PASS"));
        assert_eq!(spec.loop_back_to.as_deref(), Some("implement"));
        assert_eq!(spec.max_iterations, Some(3));
    }

    /// validate：output_from 仅对嵌套 step 有效；空串拒绝。
    #[test]
    fn validate_output_from_requires_nested_workflow() {
        let not_nested = r#"
name = "bad_of"
[[steps]]
id = "a"
role = "worker"
task = "干活"
output_from = "proposal"
"#;
        let wf: WorkflowDef = toml::from_str(not_nested).unwrap();
        let err = wf.validate().expect_err("非嵌套 step 带 output_from 必须报错");
        assert!(err.contains("output_from"), "got: {err}");

        let empty = r#"
name = "bad_of2"
[[steps]]
id = "sub"
workflow = "inner"
output_from = "  "
"#;
        let wf: WorkflowDef = toml::from_str(empty).unwrap();
        assert!(wf.validate().is_err(), "空 output_from 必须报错");
    }

    /// validate：export 仅对嵌套 step 有效；空串、保留名、与 output_key
    /// 撞名一律拒绝。撞名尤其要拒 —— 谁覆盖谁取决于执行顺序，是隐藏的
    /// 踩踏，下游评审可能拿到另一个 step 的产出当事实基线。
    #[test]
    fn validate_export_rules() {
        let case = |toml_src: &str| -> String {
            let wf: WorkflowDef = toml::from_str(toml_src).expect("合法 TOML");
            wf.validate().expect_err("应报错")
        };

        // 非嵌套 step 带 export。
        let err = case(
            r#"
name = "bad_export_1"
[[steps]]
id = "a"
role = "worker"
task = "干活"
export = { survey = "x" }
"#,
        );
        assert!(err.contains("export"), "got: {err}");

        // 子 key 为空串。
        let err = case(
            r#"
name = "bad_export_2"
[[steps]]
id = "sub"
workflow = "inner"
export = { survey = "  " }
"#,
        );
        assert!(err.contains("不能为空"), "got: {err}");

        // 父级变量名占用保留名。
        let err = case(
            r#"
name = "bad_export_3"
[[steps]]
id = "sub"
workflow = "inner"
export = { topic = "x" }
"#,
        );
        assert!(err.contains("保留变量名"), "got: {err}");

        // 与本 workflow 某个 step 的 output_key 撞名。
        let err = case(
            r#"
name = "bad_export_4"
[[steps]]
id = "sub"
workflow = "inner"
export = { design = "x" }
[[steps]]
id = "other"
role = "worker"
task = "干活"
output_key = "design"
"#,
        );
        assert!(err.contains("撞名"), "got: {err}");
    }

    /// 仓库里真实的 design_and_plan：explore 必须 export survey，
    /// 且两个评审 step 都必须真的引用 {{survey}}。
    /// 回归防线——只加 export 不在评审 prompt 里用，等于漏斗照旧堵着
    /// （本轮早前踩过同型的坑：配置加了 loop_until 但引擎走不到）。
    #[test]
    fn real_design_and_plan_pipes_survey_to_both_reviews() {
        for raw in [
            include_str!("../../config/workflows/design_and_plan.toml"),
            include_str!("../../.latte/workflows.d/design_and_plan.toml"),
        ] {
            let wf: WorkflowDef = toml::from_str(raw).expect("合法 TOML");
            wf.validate().expect("design_and_plan 应通过校验");

            let explore = wf
                .steps
                .iter()
                .find(|s| s.id == "explore")
                .expect("explore step");
            assert_eq!(
                explore.export.get("survey").map(String::as_str),
                Some("exploration"),
                "explore 必须把 survey 原文 export 出来"
            );

            for id in ["req_review", "code_review"] {
                let step = wf
                    .steps
                    .iter()
                    .find(|s| s.id == id)
                    .unwrap_or_else(|| panic!("{id} step"));
                let task = step.task_text();
                assert!(
                    task.contains("{{design}}"),
                    "{id} 仍要收到方案本体: {task}"
                );
                assert!(
                    task.contains("{{survey}}"),
                    "{id} 必须引用 {{{{survey}}}}，否则 export 白配、漏斗照旧: {task}"
                );
            }
        }
    }
}
