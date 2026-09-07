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
    /// 产出最多字符数。用于「本步产出必须是一个短标识」这类形状约束——
    /// learn_loop 的 plan 步产出直接被下游当路径片段用
    /// （`.latte/learn/{{plan_out}}/plan.json`），一旦模型在 slug 前后
    /// 多写一句话，下游拼出来的就是一条不存在的路径。靠 prompt 说
    /// "不要包含其他内容"拦不住，这里给它一个机械上限。
    #[serde(default)]
    pub max_chars: Option<usize>,
    /// 禁止出现的子串（占位符等），命中即不合格。
    #[serde(default)]
    pub forbid: Vec<String>,
    /// 必须全部出现的子串。
    #[serde(default)]
    pub require: Vec<String>,
    /// 必须**至少出现一个**的子串（OR 语义）。`require` 是 AND，表达
    /// 不了「二选一必须选一个」——learn_loop 的 quiz 步末行只能是
    /// `STATUS_ALL_DONE` 或 `STATUS_CONTINUE`，两者都不写时循环控制就
    /// 失去依据（引擎按"不含 loop_until"判返工，等于把"讲完了"误当成
    /// "还没讲完"）。空 = 不校验。
    #[serde(default)]
    pub require_any: Vec<String>,
    /// 产出**最后一个非空行**必须以其中之一开头（OR 语义）。空 = 不校验。
    ///
    /// 为什么单独查末行而不是用 `require`/`require_any` 的子串检查：
    /// 下游步骤是按「末行」取值的（quiz 从 teach 产出的末行取
    /// `TEACH_DONE <id>` 决定考哪个知识点），而子串检查只要正文里任何
    /// 位置出现过就算过——模型把标记写在中间、末行是一句总结时，契约
    /// 通过而下游解析失败，表现为"讲了 k1 却考 k2"。
    #[serde(default)]
    pub last_line_prefix_any: Vec<String>,
}

/// 校验本步是否真的调用过 `required` 里的工具。
///
/// `summary` 是 runner 收集的工具摘要，每行形如 `"{name} {args} → {result}"`
/// （见 `AgentRunner::last_turn_tool_summaries`），取首个空白分隔 token 作为
/// 工具名，并按短名（去 namespace 前缀）比较。
///
/// 与 [`check_output_contract`] 的分工：那个查模型**说了什么**，这个查它
/// **做了什么**。模型能把没做的事说得像做过（learn_loop quiz 编造判分结果的
/// 实测事故），所以凡是"必须真的执行某个动作"的步骤都该用这条。
fn check_required_tools(required: &[String], summary: &str) -> Result<(), String> {
    if required.is_empty() {
        return Ok(());
    }
    let called = called_tool_names(summary);
    let missing: Vec<&str> = required
        .iter()
        .map(|r| short_tool_name(r))
        .filter(|r| !called.contains(r))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "本步要求实际调用工具 [{}]，但本轮没有调用（实际调用了 [{}]）。\
             不要只在正文里描述结果——必须真的把工具调起来。",
            missing.join(", "),
            if called.is_empty() { "无".to_string() } else { called.join(", ") },
        ))
    }
}

/// 工具短名：去掉 `namespace.` 前缀。
fn short_tool_name(n: &str) -> &str {
    n.rsplit_once('.').map(|(_, s)| s).unwrap_or(n)
}

/// 从 runner 的工具摘要里取出实际调用过的工具短名（按调用顺序，含重复）。
///
/// 跳过占位行：一次工具都没调时 runner 给的是 `"[本 step 工具调用数: 0]"`，
/// 按空白切会得到 `[本` 这种伪工具名，混进"实际调用了"清单里既误导人、
/// 又可能让某个叫 `[本` 的必需工具意外通过（单测抓到过这个）。真实摘要行
/// 永远以工具名开头。
fn called_tool_names(summary: &str) -> Vec<&str> {
    summary
        .lines()
        .filter(|l| !l.trim_start().starts_with('['))
        .filter_map(|s| s.split_whitespace().next())
        .map(short_tool_name)
        .collect()
}

/// 校验本步是否**至少调用过一个** `any_of` 里的工具（OR 语义）。
///
/// 与 [`check_required_tools`]（AND）的分工：有些"动作"有多条合法实现
/// 路径，AND 会把它们全变成必须。learn_loop 的 plan 步要求"取证不可
/// 跳过"，而取证既可以 `read` 也可以 `code_graph` 也可以 `search`，
/// 写成 `require_tools` 就等于逼它三个都调一遍。
///
/// 为什么这条必须由引擎查：plan 步实测事故——它一次源码都没读
/// （全部工具调用就是 2 次 `write`），5 道题里 3 道的"正确答案"是幻觉
/// （不存在的 `init` 回调、`MALLOCX_FAIL_NOWAIT`、写错文件的版本宏）。
/// 学员照这种题去理解代码，学到的是假知识——比不学更糟。
fn check_required_tools_any(any_of: &[String], summary: &str) -> Result<(), String> {
    if any_of.is_empty() {
        return Ok(());
    }
    let called = called_tool_names(summary);
    let wanted: Vec<&str> = any_of.iter().map(|r| short_tool_name(r)).collect();
    if wanted.iter().any(|w| called.contains(w)) {
        Ok(())
    } else {
        Err(format!(
            "本步要求至少调用 [{}] 之中的一个工具，但本轮一个都没调用\
             （实际调用了 [{}]）。产出必须建立在真实取证之上，不能凭记忆写。",
            wanted.join(" / "),
            if called.is_empty() { "无".to_string() } else { called.join(", ") },
        ))
    }
}

/// 校验单个工具在本步的调用次数上限（`工具短名 → 最大次数`）。
///
/// 为什么需要它：`require_tools` 只能查"有没有调过"，查不出"调了几次"。
/// learn_loop 的 quiz 步语义是「恰好弹一次窗」——多弹一次就是同一个知识点
/// 连考两遍、账本按最后一次覆盖，学员的答题记录被悄悄丢掉一条；上限设 0
/// 则可表达"本步禁止调用该工具"（比从 `tools` 白名单里摘掉更精确：白名单
/// 是"拿不到工具"，上限 0 是"拿得到但不许用"，批注也能说清为什么）。
fn check_tool_call_limits(
    limits: &std::collections::BTreeMap<String, usize>,
    summary: &str,
) -> Result<(), String> {
    if limits.is_empty() {
        return Ok(());
    }
    let called = called_tool_names(summary);
    for (name, max) in limits {
        let short = short_tool_name(name);
        let n = called.iter().filter(|c| **c == short).count();
        if n > *max {
            return Err(if *max == 0 {
                format!("本步禁止调用工具 `{short}`，但本轮调用了 {n} 次。")
            } else {
                format!(
                    "本步允许调用工具 `{short}` 最多 {max} 次，但本轮调用了 {n} 次。\
                     多余的调用会让本步的记账依据变得不唯一，请只保留必要的那一次。"
                )
            });
        }
    }
    Ok(())
}

/// 本步的**工具使用行为**总校验：必调（AND）→ 至少调一个（OR）→ 次数上限。
///
/// 三条都查"它做了什么"，与 [`check_output_contract`]（查"它说了什么"）
/// 互补。顺序按批注的指导性排：完全没调 > 没取证 > 调多了。
fn check_tool_usage(step: &WorkflowStepDef, summary: &str) -> Result<(), String> {
    check_required_tools(&step.require_tools, summary)?;
    check_required_tools_any(&step.require_tools_any, summary)?;
    check_tool_call_limits(&step.tool_call_limits, summary)
}


/// 把一个 step 内多个 speaker 的产出聚合成该 step 的输出。
///
/// # 修的 bug
///
/// 原来是 `last_output = response`，每个 speaker 覆盖一次，于是
/// `output_key` 只绑到**最后一个** speaker 的产出——前面所有 speaker 的
/// 内容只进 `step_transcript`（step 内可见），**从不传给下游**。
///
/// 实测实锤（jemalloc 会话 `wf-design_brainstorm-1788084798861063`）：
/// `evaluate` 步派了 reviewer / security / devops，三份产出分别 4556 /
/// 12346 / 5131 字符，而 `evaluation` 只拿到 devops 的 5131 字符。
/// security 那 12346 字里**包含 3 项 P0 安全阻断**，从未到达下游
/// synthesize。这不是性能问题，是数据丢失。
///
/// # 聚合形式
///
/// 单 speaker（绝大多数 step）时**逐字返回原产出**，不加任何包装——
/// 否则会改变所有既有 workflow 的下游输入。多 speaker 时按声明顺序
/// 拼接，并加 `## [role]` 分隔，让下游能分辨谁说了什么。
fn aggregate_step_output(parts: &[(String, String)]) -> String {
    match parts {
        [] => String::new(),
        [(_, only)] => only.clone(),
        many => many
            .iter()
            .map(|(role, out)| format!("## [{role}]\n\n{out}"))
            .collect::<Vec<_>>()
            .join("\n\n---\n\n"),
    }
}

/// 校验产出是否满足契约。按 min_chars → max_chars → forbid → require →
/// require_any → last_line_prefix_any 顺序检查，第一个违规即返回中文原因
/// （措辞可直接作为给模型的验收批注）。空契约（全默认）恒 Ok。
fn check_output_contract(contract: &OutputContract, output: &str) -> Result<(), String> {
    if let Some(min) = contract.min_chars {
        let n = output.chars().count();
        if n < min {
            return Err(format!("产出过短：{n} 字符，少于要求的 {min} 字符"));
        }
    }
    if let Some(max) = contract.max_chars {
        let n = output.chars().count();
        if n > max {
            return Err(format!(
                "产出过长：{n} 字符，超过要求的 {max} 字符上限。\
                 本步的产出会被下游直接取用，只输出要求的那部分内容，不要加说明或前言。"
            ));
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
    if !contract.require_any.is_empty()
        && !contract.require_any.iter().any(|p| output.contains(p.as_str()))
    {
        return Err(format!(
            "产出必须包含 [{}] 之中的至少一个，但一个都没出现。",
            contract.require_any.join(" / ")
        ));
    }
    if !contract.last_line_prefix_any.is_empty() {
        let last = output
            .lines()
            .rev()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        if !contract
            .last_line_prefix_any
            .iter()
            .any(|p| last.starts_with(p.as_str()))
        {
            return Err(format!(
                "产出的最后一行必须以 [{}] 之中的一个开头，实际末行是「{}」。\
                 下游步骤按末行取值，写在正文中间不算。",
                contract.last_line_prefix_any.join(" / "),
                output_excerpt(last, 80),
            ));
        }
    }
    Ok(())
}

// ─── plan tasks 结构化 patch（require_plan_tasks / patch_from / submit_plan_from） ───
//
// 设计血缘：oh-my-pi 的 hashline 验证了「小操作集 + 锚点 + 机械校验 +
// 失败重试」在 LLM 编辑场景可行；这里把同一模式从「文本行」平移到
// 「任务字段」——锚点是标题子串，校验复用 plan 工具的同一套机械判据，
// 失败走 output_contract 同一条批注重试链。

/// 扫描文本中的 ``` 代码块，返回（整段字节范围, info 串, 内容）。
fn fenced_code_blocks(text: &str) -> Vec<(std::ops::Range<usize>, String, String)> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let line_start = offset;
        offset += line.len() + 1; // +1 for '\n'（最后一行无换行也无妨：span 只用于切片）
        let trimmed = line.trim_start();
        if !trimmed.starts_with("```") {
            continue;
        }
        let info = trimmed.trim_start_matches('`').trim().to_string();
        let content_start = offset;
        let mut content_end = offset;
        let mut closed_end = None;
        for line in lines.by_ref() {
            let ls = offset;
            offset += line.len() + 1;
            if line.trim_start().starts_with("```") {
                closed_end = Some(offset.min(text.len()));
                content_end = ls;
                break;
            }
            content_end = offset.min(text.len());
        }
        let end = closed_end.unwrap_or_else(|| text.len());
        out.push((line_start..end, info, text[content_start..content_end].to_string()));
    }
    out
}

/// 解析 fence 内容为 JSON。模型常把闭合 ``` 写在内容最后一行的行尾
/// （`…} ``` ` 不换行），按行扫描会把它吞进内容导致解析失败——先原样
/// 解析，失败则剥掉行尾的闭合反引号再试一次。
fn parse_fence_json(content: &str) -> Option<serde_json::Value> {
    if let Ok(v) = serde_json::from_str(content) {
        return Some(v);
    }
    let stripped = content.trim_end().trim_end_matches('`').trim_end();
    serde_json::from_str(stripped).ok()
}

/// 从文本中定位**唯一**含 `"tasks"` 数组的 ```json fence，返回
/// (fence 整段范围, tasks 数组的 Value)。0 个 = 模型没给结构化清单；
/// 多个 = 草案里混了示例 JSON，「哪份是 canonical」无从判断——两种
/// 都是机械错误，带指导文案走批注重试。
fn find_tasks_fence(text: &str) -> Result<(std::ops::Range<usize>, serde_json::Value), String> {
    let mut hits: Vec<(std::ops::Range<usize>, serde_json::Value)> = Vec::new();
    for (span, _info, content) in fenced_code_blocks(text) {
        let Some(v) = parse_fence_json(&content) else {
            continue;
        };
        if let Some(tasks) = v.get("tasks").filter(|t| t.is_array()) {
            hits.push((span, tasks.clone()));
        }
    }
    match hits.len() {
        0 => Err("产出中没有可解析的、含 \"tasks\" 数组的 ```json 代码块。\
                  请把任务清单放进一个 ```json fenced 代码块，顶层结构为 {\"tasks\": [...]}"
            .into()),
        1 => Ok(hits.pop().expect("len == 1")),
        n => Err(format!(
            "产出里有 {n} 个含 \"tasks\" 数组的 json 代码块，无法判断哪份是正式清单。\
             请只保留一个（示例/草稿请删除或改成非 tasks 结构）。"
        )),
    }
}

/// `require_plan_tasks` 的校验：抽清单 fence + 跑 plan 工具的同一套
/// 机械校验。措辞直接作为给模型的验收批注。
fn check_plan_tasks_output(output: &str, cwd: &std::path::Path) -> Result<(), String> {
    let (_, tasks) = find_tasks_fence(output)?;
    crate::controller::parse_and_validate_plan_tasks(&tasks, cwd)
        .map(|_| ())
        .map_err(|e| format!("任务清单未通过机械校验：{e}"))
}

/// `require_symbols_resolvable` 的校验：产出里反引号包裹的代码符号必须
/// 都能在仓库源码里回指到。判据、豁免与报错措辞都在
/// [`latte_rs_agent_tools::utils::symbol_check`]。
fn check_symbols_output(output: &str, cwd: &std::path::Path) -> Result<(), String> {
    latte_rs_agent_tools::utils::symbol_check::check_symbols_resolvable(output, cwd)
}

/// `patch_from` 的一条修改操作：把标题包含 `task` 子串的唯一任务的
/// `field` 字段整体替换为 `set`。操作集故意保持最小（只有整体替换
/// 一种语义）——字段级 set 已足够表达评审修正，更细的数组合并/删除
/// 语义只会放大模型出错面。
#[derive(Debug, serde::Deserialize)]
struct TaskPatchOp {
    task: String,
    field: String,
    set: serde_json::Value,
}

/// 从 patch 步产出里解析 ops：一个 ```json fence 的 `{"ops":[...]}`。
fn parse_task_patch_ops(text: &str) -> Result<(Vec<TaskPatchOp>, std::ops::Range<usize>), String> {
    let mut hits = Vec::new();
    for (span, _info, content) in fenced_code_blocks(text) {
        let Some(v) = parse_fence_json(&content) else {
            continue;
        };
        if v.get("ops").is_some_and(|o| o.is_array()) {
            hits.push((span, v["ops"].clone()));
        }
    }
    let (span, ops_val) = match hits.len() {
        0 => return Err("产出中没有含 \"ops\" 数组的 ```json 代码块。\
                         本步是定向修补：只输出修订说明 + 一个 ```json 代码块 \
                         {\"ops\": [{\"task\": \"<标题唯一子串>\", \"field\": \"<字段名>\", \"set\": <新值>}]}；\
                         没有需要修改的字段时输出 {\"ops\": []}。"
            .into()),
        1 => hits.pop().expect("len == 1"),
        n => {
            return Err(format!(
                "产出里有 {n} 个含 \"ops\" 数组的 json 代码块，请只保留一个。"
            ))
        }
    };
    let ops: Vec<TaskPatchOp> = serde_json::from_value(ops_val)
        .map_err(|e| format!("ops 解析失败：{e}（每条 op 需要 task / field / set 三个字段）"))?;
    Ok((ops, span))
}

/// patch 可写字段白名单与值类型校验。subtasks 不支持（嵌套层的锚定
/// 语义靠标题子串表达不清，真需要时走 draft 重做）。
fn check_patch_field(field: &str, value: &serde_json::Value) -> Result<(), String> {
    let ok = match field {
        "title" | "description" => value.is_string(),
        "priority" => value.is_i64() || value.is_u64() || value.is_null(),
        "labels" | "paths" => value
            .as_array()
            .is_some_and(|a| a.iter().all(|x| x.is_string())),
        "task_type" | "workflow" => value.is_string() || value.is_null(),
        _ => {
            return Err(format!(
                "field '{field}' 不可 patch（允许：title / description / priority / labels / task_type / workflow / paths）"
            ))
        }
    };
    if !ok {
        return Err(format!("field '{field}' 的值类型不对（收到 {value}）"));
    }
    Ok(())
}

/// 把 patch 步产出（修订说明 + ops）机械应用到上游草案，返回打了补丁
/// 的完整草案。ops 为空 = 无修订，草案原样返回。
fn apply_task_patch(
    draft: &str,
    response: &str,
    cwd: &std::path::Path,
) -> Result<String, String> {
    let (ops, ops_span) = parse_task_patch_ops(response)?;
    if ops.is_empty() {
        return Ok(draft.to_string());
    }
    let (span, tasks_val) = find_tasks_fence(draft)?;
    let mut tasks: Vec<serde_json::Value> = tasks_val
        .as_array()
        .expect("find_tasks_fence 只返回数组")
        .clone();
    let mut titles: Vec<String> = tasks
        .iter()
        .map(|t| {
            t.get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        })
        .collect();
    for (i, op) in ops.iter().enumerate() {
        check_patch_field(&op.field, &op.set).map_err(|e| format!("ops[{i}]：{e}"))?;
        let matched: Vec<usize> = titles
            .iter()
            .enumerate()
            .filter(|(_, title)| title.contains(op.task.as_str()))
            .map(|(idx, _)| idx)
            .collect();
        let idx = match matched.len() {
            1 => matched[0],
            0 => {
                return Err(format!(
                    "ops[{i}]：没有标题包含「{}」的任务。可选标题：{}",
                    op.task,
                    titles.join("；")
                ))
            }
            n => {
                return Err(format!(
                    "ops[{i}]：「{}」匹配到 {n} 个任务（{}），请用更长的标题子串唯一定位。",
                    op.task,
                    matched
                        .iter()
                        .map(|&j| titles[j].as_str())
                        .collect::<Vec<_>>()
                        .join("；")
                ))
            }
        };
        let obj = tasks[idx]
            .as_object_mut()
            .ok_or_else(|| format!("ops[{i}]：tasks[{idx}] 不是对象"))?;
        // no-op 守卫：新值与现值相同 = 白转一圈还以为自己改了
        // （hashline 把 byte-identical edit 判为错误是同一道理）。
        let old = obj.get(&op.field).cloned().unwrap_or(serde_json::Value::Null);
        if old == op.set {
            return Err(format!(
                "ops[{i}]：字段 '{}' 的新值与现值相同（无实际修改）。若无需修改请从 ops 里移除该条。",
                op.field
            ));
        }
        obj.insert(op.field.clone(), op.set.clone());
        // title 被改时刷新定位表，后续 op 按新标题匹配。
        if op.field == "title" {
            titles[idx] = tasks[idx]
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
        }
    }
    // 补丁后重跑 plan 机械校验：修补最容易引入新重叠（task_refine
    // 实录：revise 把 src/arena.c 补进已持有它的任务，凑成第 14 处
    // 重叠，整单被 plan 工具拒绝）。
    let patched = crate::controller::parse_and_validate_plan_tasks(
        &serde_json::Value::Array(tasks),
        cwd,
    )
    .map_err(|e| format!("补丁后的清单未通过机械校验：{e}"))?;
    let new_fence = format!(
        "```json\n{}\n```",
        serde_json::to_string_pretty(&serde_json::json!({ "tasks": patched }))
            .expect("PlanTask 序列化不会失败")
    );
    let mut out = String::with_capacity(draft.len() + response.len());
    out.push_str(&draft[..span.start]);
    out.push_str(&new_fence);
    out.push_str(&draft[span.end..]);
    // 修订说明 = 模型产出里去掉 ops fence 的部分，附在草案末尾给人看。
    let notes = format!("{}{}", &response[..ops_span.start], &response[ops_span.end..]);
    let notes = notes.trim();
    if !notes.is_empty() {
        out.push_str("\n\n---\n\n### 本次修订\n\n");
        out.push_str(notes);
        out.push('\n');
    }
    Ok(out)
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

/// `require_plan_submit` 的判据：plan 工具提交成功即把私有 `PlanStage`
/// 推到 `PendingApproval`；机械校验（paths 存在性 / paths 重叠）整单
/// 拒绝时不改状态，停在 `Normal`。
fn plan_submitted(stage: Option<&crate::controller::SharedPlanStage>) -> bool {
    stage
        .map(|s| {
            matches!(
                &*s.read(),
                crate::controller::PlanStage::PendingApproval { .. }
            )
        })
        .unwrap_or(false)
}

/// 未成功提交 plan 时拼进重试 prompt 的批注。
///
/// 必须把**上次产出整段**带过去：每次尝试都是全新 subagent（无跨次
/// 记忆），plan 工具的报错只存在于上一次的对话里，不带过去模型就只知道
/// 「你失败了」、不知道失败在哪个 path 上。
fn plan_submit_retry_annotation(prev_output: &str) -> String {
    format!(
        "上次**没有成功提交 plan 提案** —— plan 工具没有产生待批准清单，\
         任务一个都没进看板，这一步等于白跑。\
         最常见的原因是 plan 的机械校验（paths 第一级目录不存在 / paths 范围重叠）\
         整单拒绝后，改口输出了一份文字报告代替提交。\
         请按下面这份上次产出里的工具报错修正 **paths 与 labels**（重叠就删掉下游任务 paths\
         里的上游交付物路径、改在验收标准里文字引用；或给纯阅读 / 纯消费型任务的 labels\
         追加「只读」），然后**重新调用一次 plan 工具**提交完整清单。\
         禁止用文字报告、方案表、请示代替提交。\n\n--- 上次产出 ---\n{}",
        output_excerpt(prev_output, 2000)
    )
}

/// 契约重试耗尽后的语义兜底裁决。纯字符串契约分不清「格式不合格
/// 的坏产出」和「语义正确但没写约定标记的好产出」（实测实锤：
/// gate 的合法 REJECT 缺「VERDICT: PASS」字样，被契约当成格式错误
/// 判死）。耗尽前过一道 advisor 返回审查：
/// - verdict ok → 带批注放行（下游与人都能看到契约被语义覆盖）；
/// - warn/intervene/terminate/超时/无引擎 → `None`，维持原失败。
async fn contract_last_resort_review(
    review_engine: &Option<Arc<crate::advisor_monitor::AdvisorReviewEngine>>,
    event_tx: &broadcast::Sender<ChatEvent>,
    // 本 workflow 的 topic = 该 step 的「主会话主题」。
    main_topic: &str,
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
        main_topic,
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

/// 返工环耗尽前的最后一道 advisor 语义复核。
///
/// 机械判据（`loop_until` 子串匹配）分不清两件事：**草案真有阻断缺陷**，
/// 和**评审在挑不影响下游的小瑕疵**。而耗尽即判死会把整条流水线连同前面
/// 几十轮成果一起作废——实测实录：`task_refine` 的 gate 两轮
/// `VERDICT: REVISE`（理由是行号偏差）撞满 `max_iterations=2` → workflow
/// Failed → `submit` 步永不执行 → `plan` 工具从不被调用 → 「添加子任务」
/// 弹窗彻底消失，用户只看到「没反应」。
///
/// 因此耗尽前把**被返工的那份产出**（`loop_back_to` 目标 step 的 output）
/// 连同**尚未消化的评审意见**交给 advisor 做语义审查：
/// - `Ok` → 判定实质合格，放行进入下游（带 Status 说明，可追溯）；
/// - 其他裁决 / 无 advisor 引擎 / 拿不到产出 → `false`，维持原失败。
///
/// 与「无条件降级放行」的关键区别：**必须 advisor 主动判定合格**才放行。
/// 真有实质缺陷时它会给出非 Ok 裁决，流程照旧失败——不是放水。
/// advisor 未启用时行为与历史完全一致（直接失败）。
async fn loop_exhausted_last_resort_review(
    review_engine: &Option<Arc<crate::advisor_monitor::AdvisorReviewEngine>>,
    event_tx: &broadcast::Sender<ChatEvent>,
    main_topic: &str,
    step_id: &str,
    cond: &str,
    iterations: usize,
    artifact: &str,
    objection: &str,
) -> bool {
    let Some(engine) = review_engine.as_ref() else {
        return false;
    };
    if artifact.trim().is_empty() {
        return false;
    }
    let task = format!(
        "评审返工环已迭代 {iterations} 次仍未满足「{cond}」，即将判整条流程失败。\
         请对下面这份产出做最后一次语义复核，判断它**是否实质合格、可以进入下游**。\n\n\
         【尚未消化的评审意见】\n{objection}\n\n\
         判定标准：\n\
         - 合格（ok）：剩余意见都属于不影响下游使用的小瑕疵——措辞、格式、\
           行号/引用偏差、命名风格等；产出的主体结论与结构是可用的。\n\
         - 不合格：存在实质缺陷——内容事实错误、关键目标遗漏、自相矛盾、\
           或下游据此无法执行。\n\
         只做这一个判断，不要重写产出。"
    );
    let (_reviewed, verdict) = crate::controller::gate_delegate_return(
        engine,
        event_tx,
        main_topic,
        "advisor",
        "", // 此处不注入角色职责全文，审查以「产出 + 未消化意见」为基准
        &task,
        artifact.to_string(),
        "", // 无工具调用数据
    )
    .await;
    let Some(v) = verdict else {
        return false;
    };
    if v.verdict != crate::advisor_monitor::Verdict::Ok {
        return false;
    }
    let _ = event_tx.send(ChatEvent::Status {
        message: format!(
            "⚠️ step '{step_id}' 返工环迭代 {iterations} 次未满足「{cond}」，\
             经 advisor 语义复核判定产出实质合格，放行进入下游"
        ),
    });
    true
}

/// 可选 speaker 的关键词匹配范围。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchScope {
    /// 只看 workflow 的 `topic`（默认）。topic 是用户诉求的原文，
    /// 跨 step 稳定、不含上游产出，误命中概率最低。
    #[default]
    Topic,
    /// 看渲染后的任务全文（topic + 上游 `{{var}}` 注入的产出）。
    /// 命中面更广但更容易被上游文本里偶然出现的词带偏——只在
    /// 「相关性只能从上游产出里判断」时才用。
    Task,
}

/// 可选 speaker：按关键词命中情况决定这一步**是否真的把它派出去**。
///
/// 为什么需要它：多 speaker 的 step 是固定阵容（`speakers` 写死），
/// 一个 step 只渲染一份 prompt 广播给所有 speaker，角色分工全靠 prompt
/// 里一句话。任务与某个角色无关时（实测实锤：给
/// 「编排 jemalloc 源码学习路径」这种没有界面的任务派 `designer`），
/// 该角色只有两条路——交元讨论，或越权替别人干活——两条都会被 advisor
/// 的返回审查判 intervene 并触发重做，而重做也救不回来（角色本身就
/// 不该来）。实测代价：designer + devops 两个分派合计约 55 万
/// input tokens 与 500s 墙钟全部作废。
///
/// 判定放在**派发前**且**零模型调用**：这是静态的模板选型问题，
/// 用静态手段解——不花 token、结果确定、resume 后可复现。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptionalSpeaker {
    /// 角色 id。不得与本 step 的无条件 speaker 重复。
    pub role: String,
    /// 命中其中**任意一个**关键词才派（OR 语义）。空 = 不设正向条件。
    #[serde(default)]
    pub when_any: Vec<String>,
    /// 命中其中**任意一个**关键词就不派（否决优先于 `when_any`）。
    #[serde(default)]
    pub unless_any: Vec<String>,
    /// 关键词匹配范围，默认 `topic`。
    #[serde(default)]
    pub match_scope: MatchScope,
}

/// 被跳过的可选 speaker 及原因（用于事件与日志，便于回查
/// 「这一步为什么少了个人」）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedSpeaker {
    pub role: String,
    pub reason: String,
}

/// 关键词命中判定（大小写不敏感）。
///
/// 纯 ASCII 字母数字的关键词按**词边界**匹配，非 ASCII（中文等）按
/// 子串匹配。原因：`"ui"` 用裸子串会命中 `build` / `guide` /
/// `require` / `quick`，一个 CI 构建话题就能把 designer 拉进来，
/// 可选角色等于没做——这类静默误命中比漏命中更难查。
fn keyword_hit(haystack_lower: &str, needle: &str) -> bool {
    let needle = needle.trim().to_lowercase();
    if needle.is_empty() {
        return false;
    }
    let ascii_word = needle.chars().all(|c| c.is_ascii_alphanumeric());
    if !ascii_word {
        return haystack_lower.contains(&needle);
    }
    let bytes = haystack_lower.as_bytes();
    let nlen = needle.len();
    let mut from = 0usize;
    while let Some(rel) = haystack_lower[from..].find(&needle) {
        let start = from + rel;
        let end = start + nlen;
        let left_ok = start == 0 || !is_word_byte(bytes[start - 1]);
        let right_ok = end >= bytes.len() || !is_word_byte(bytes[end]);
        if left_ok && right_ok {
            return true;
        }
        from = start + 1;
        if from >= haystack_lower.len() {
            break;
        }
    }
    false
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
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
    /// 可选 speaker：只在关键词命中时才派（见 [`OptionalSpeaker`]）。
    /// 与无条件 `speakers` 并列，派发顺序排在无条件 speaker 之后。
    ///
    /// ```toml
    /// [[steps]]
    /// id = "brainstorm"
    /// speakers = ["architect", "programmer"]
    /// [[steps.optional_speakers]]
    /// role = "designer"
    /// when_any = ["ui", "ux", "界面", "前端", "交互"]
    /// ```
    #[serde(default)]
    pub optional_speakers: Vec<OptionalSpeaker>,
    /// **step 级**条件执行：命中其中任意一个关键词才跑这一步。
    ///
    /// 与 [`OptionalSpeaker`] 的分工：后者在**一个 step 内**按需增删
    /// speaker；这里是整步的开关。把多 speaker 步拆成同 wave 的多个
    /// 单 speaker 步（为了并发）之后，"按需派某个角色"就只能用它表达
    /// ——`optional_speakers` 是 step 内的机制，拆开后没有载体。
    ///
    /// 空 = 无条件执行（默认，既有 workflow 不受影响）。
    #[serde(default)]
    pub when_any: Vec<String>,
    /// 命中其中任意一个关键词就**跳过**这一步（否决优先于 `when_any`）。
    #[serde(default)]
    pub unless_any: Vec<String>,
    /// 关键词匹配范围，默认 `topic`。
    #[serde(default)]
    pub match_scope: MatchScope,
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
    /// 本步**必须实际调用过**的工具（短名）。没调用即不合格，走
    /// `output_contract` 同一条重试链（复用 `max_retries`）。
    ///
    /// 为什么需要它而不是把要求写进 prompt 或 `output_contract`：
    /// 两者都只能检查模型**说了什么**，而模型可以把没做的事说得像做过。
    /// learn_loop 的 quiz 步实测事故——它没调 `ask`，直接拿上一轮 prompt
    /// 里出现过的旧答案编了一段「判分结果：答错 / 你选的是：split」，
    /// `output_contract.require` 的子串检查完全满足，学员却连题都没看到，
    /// 而账本被写成了"答错两次"。只有核对**真实的工具调用记录**才拦得住。
    #[serde(default)]
    pub require_tools: Vec<String>,
    /// 本步必须**至少调用过其中一个**工具（OR 语义，短名）。同一条重试链。
    ///
    /// 用于"这个动作有多条合法实现路径"的场合：`require_tools` 是 AND，
    /// 写上 `["read","code_graph","search"]` 等于逼模型三个都调一遍；而
    /// 真实要求只是"必须取证"。见 [`check_required_tools_any`] 里记录的
    /// plan 步零取证事故。
    #[serde(default)]
    pub require_tools_any: Vec<String>,
    /// 本步单个工具的调用次数上限：`工具短名 → 最大次数`。同一条重试链。
    ///
    /// `require_tools` 只能查"有没有调过"，查不出"调了几次"。需要表达
    /// 「恰好一次」时两者搭配使用（`require_tools = ["ask"]` +
    /// `tool_call_limits = { ask = 1 }`）——learn_loop 的 quiz 步就是这个
    /// 语义：多弹一次窗就是同一个知识点连考两遍、账本按最后一次覆盖，
    /// 学员的答题记录被悄悄丢掉一条。上限写 0 = 本步禁止调用该工具。
    #[serde(default)]
    pub tool_call_limits: std::collections::BTreeMap<String, usize>,
    /// 本 step 必须**成功提交一份 plan 提案**才算合格（与
    /// `output_contract` 同一条重试链，复用 `max_retries`）。
    ///
    /// 为什么需要它：plan 工具的机械校验（paths 存在性 / paths 重叠）
    /// 整单拒绝后，模型往往改口输出一份「已被拒绝，请指示走 A/B/C」的
    /// 回退报告——这份正文字数、格式全部合格，纯字符串契约看不出问题，
    /// 于是 step 通过、workflow 报 `ok`，而**零任务入库**（实测
    /// 某次实测会话：14 处 paths 重叠被拒，六个子任务
    /// 一个都没进看板，会话看起来正常完成）。
    ///
    /// 判据取 plan 工具的私有 `PlanStage`：提交成功即置
    /// `PendingApproval`，机械校验失败则停在 `Normal`。不合格时把工具
    /// 的原始报错拼进重试 prompt，让模型知道该修什么。
    #[serde(default)]
    pub require_plan_submit: bool,
    /// 本步产出必须包含一个 ```json fence 的 `{"tasks":[...]}`，且该
    /// 清单必须通过 plan 工具的**同一套**机械校验（`controller.rs` 的
    /// `parse_and_validate_plan_tasks`：路径存在性、字符串层重叠、
    /// workflow 名、task_type 名）。不合格走 `output_contract` 同一条
    /// 批注重试链（复用 `max_retries`）。
    ///
    /// 为什么需要它：task_refine 的 paths 重叠自查长期靠「模型自觉画
    /// 自查表」，实测多次漏判、清单带病走到 submit 才被 plan 工具整单
    /// 拒绝（前期几十轮评审全部白跑）。把校验前移到产出草案的 step，
    /// 自查从「说的」变成「算的」。
    #[serde(default)]
    pub require_plan_tasks: bool,
    /// 本步产出里**反引号包裹**的代码符号必须都能在仓库源码里按词边界回指到，
    /// 查不到的走 `output_contract` 同一条批注重试链（复用 `max_retries`）。
    /// 判据与豁免见 `latte_rs_agent_tools::utils::symbol_check`。
    ///
    /// 为什么需要它（实测 2026-09 jemalloc 会话）：architect 产出的学习规划里
    /// 131 个反引号标识符有 **74 个（56%）全仓搜不到**，且成体系地是该项目
    /// 3.x/4.x 的旧术语（`arena_bin_malloc_hard`、`bin->runcur`、
    /// `chunk_alloc_mmap`…）——5.x 早把 chunk→extent、run→slab 改了名，但
    /// 训练语料里旧版本的解析文章远多于新版本。这些名字彼此自洽、读起来
    /// 非常专业，人类评审与 LLM 终审都没看出来（那次 verdict 反而表扬了
    /// 「取证覆盖度」）。
    ///
    /// 与 `require_tools_any`（查它调没调工具）、`require_plan_tasks`（查
    /// 路径存在性）的分工：那两条管不到「文件对、函数名假」这一类。按来源
    /// 归因那 74 个，**39 个来自模型压根没打开过的文件**——任何工具侧改进
    /// 都碰不到这部分，只能在产出侧做机械回指。
    ///
    /// 与 oh-my-pi 的 hashline seen-line guard 同源：那边是「编辑不许落在
    /// read 没显示过的行上」，这边把同一条 provenance 思路从「行」平移到
    /// 「符号」。
    #[serde(default)]
    pub require_symbols_resolvable: bool,
    /// patch 模式：本步 speaker 不再重出整份清单，只输出「修订说明 +
    /// 一个 ```json fence 的 `{"ops":[{"task":"<标题唯一子串>",
    /// "field":"<字段名>","set":<新值>}]}`」。引擎把 ops 机械应用到
    /// 指定 output_key 的上游草案（必须含 tasks JSON fence）上：
    /// 定位失败（0 或 >1 匹配）、字段名/类型非法、补丁后清单过不了
    /// plan 机械校验，都走批注重试链。空 `ops: []` = 无需修订，草案
    /// 原样成为本步产出。成功时本步产出 = 打了补丁的完整草案（下游
    /// 拿到的仍是全量文本，只有模型那一跳是增量）。
    ///
    /// 为什么需要它（实测实录）：跨 step 信息只有 `{{output_key}}` 纯
    /// 文本替换，没有 patch 通道，revise 类 step 只能「输出完整修订
    /// 版」——实测一次 task_refine 运行里同一份清单被模型完整输出
    /// 3 次（draft → 全量重出 → 转写成 plan args），revise 一跳花了
    /// 3884 output token 只改 2 个字段。
    #[serde(default)]
    pub patch_from: Option<String>,
    /// 机械提交步：本 step **不派 speaker、零模型调用**——引擎直接从
    /// 指定 output_key 的上游产出里抽 tasks JSON fence，调用 plan 工具
    /// 的共享实现（`controller::submit_plan_proposal`：同一套校验 +
    /// PlanProposed 广播 + 弹窗落盘 + PlanStage 置位）提交给用户勾选。
    /// 与 `role`/`speakers` 互斥（validate 期拦截）。校验失败 = step
    /// 失败（上游 `require_plan_tasks`/`patch_from` 已两道硬校验，
    /// 理论上不可达；真失败则响亮报错，不静默）。
    ///
    /// 为什么需要它（实测实录）：submit 步此前是「模型把草案再抄一遍
    /// 成 plan args」——实测一跳 in=18001/out=3311/158s，且转写过程
    /// 本身曾引入增删改漂移（`task_refine.toml` 头部实录）。
    #[serde(default)]
    pub submit_plan_from: Option<String>,
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
    /// 流水线（实测实锤：唯一跑到 gate 的那次运行，44 分钟零产出）。
    /// 熔断词表改为按 step 显式声明，引擎不再私藏 magic string。
    #[serde(default)]
    pub loop_abort_on: Option<String>,
    /// 「不可放行」标记：返工环**迭代耗尽**时，若产出包含该子串，
    /// 跳过 advisor 语义复核直接判 workflow 失败。与 `loop_abort_on`
    /// 的区别：abort 是首次命中即终局（不给返工机会）；本字段允许
    /// 正常返工，只是耗尽那一刻不许被复核放行。
    ///
    /// 为什么需要它（实测实锤：2026-09 jemalloc implementation_plan
    /// 会话）：gate 三轮均输出 `VERDICT: REJECT`（阻断问题带 file:line
    /// 证据），耗尽点 advisor 复核的却是 `loop_back_to` 目标的产出
    /// （breakdown 计划文本——本身没病，病在 tasks JSON），误判「实质
    /// 合格」放行，run 以 `ok` 收尾、summary 却是 REJECT 原文——
    /// 失败被吞成成功，三道带证据的阻断意见被一次复核否决。
    /// REJECT（存在阻断问题/实测错误）与 REVISE（小瑕疵返工）必须
    /// 区别对待：后者才是 advisor 复核放行的设计场景。
    #[serde(default)]
    pub loop_no_release_on: Option<String>,
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
    /// 被逐层压缩的结论，想核对也无从核对（实测实锤：双评审共
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

    /// 全部**声明过**的角色（无条件 + 可选），供 validate 的角色存在性
    /// 检查、UI 列举、`speaker_roles()` 用。不代表本次真的会派。
    pub fn declared_roles(&self) -> Vec<String> {
        let mut out = self.roles();
        for opt in &self.optional_speakers {
            if !out.contains(&opt.role) {
                out.push(opt.role.clone());
            }
        }
        out
    }

    /// 本次真正要派的角色 + 被跳过的可选角色（含原因）。
    ///
    /// `topic` 取 workflow 入口写入 vars 的 `topic`（嵌套 step 取当前层
    /// 的 topic）；`task_text` 取渲染后的任务全文。无条件 speaker 永远
    /// 保留——validate 已保证至少有一个，因此返回值不会为空。
    pub fn active_roles(&self, topic: &str, task_text: &str) -> (Vec<String>, Vec<SkippedSpeaker>) {
        let mut active = self.roles();
        let mut skipped = Vec::new();
        if self.optional_speakers.is_empty() {
            return (active, skipped);
        }
        let topic_lower = topic.to_lowercase();
        let task_lower = task_text.to_lowercase();
        for opt in &self.optional_speakers {
            if active.contains(&opt.role) {
                continue;
            }
            let hay = match opt.match_scope {
                MatchScope::Topic => &topic_lower,
                MatchScope::Task => &task_lower,
            };
            let scope = match opt.match_scope {
                MatchScope::Topic => "topic",
                MatchScope::Task => "task",
            };
            if let Some(veto) = opt.unless_any.iter().find(|k| keyword_hit(hay, k)) {
                skipped.push(SkippedSpeaker {
                    role: opt.role.clone(),
                    reason: format!("{scope} 命中排除词 '{veto}'"),
                });
                continue;
            }
            if opt.when_any.is_empty() || opt.when_any.iter().any(|k| keyword_hit(hay, k)) {
                active.push(opt.role.clone());
            } else {
                skipped.push(SkippedSpeaker {
                    role: opt.role.clone(),
                    reason: format!("{scope} 未命中 {:?}", opt.when_any),
                });
            }
        }
        (active, skipped)
    }

    /// 本 step 这次要不要跑。返回 `(是否执行, 跳过原因)`。
    ///
    /// 跳过的 step 的 `output_key` 会被绑成**空串**（见调用点）——不绑的话
    /// 下游 prompt 里的 `{{key}}` 会原样留着，模型看到一个字面占位符。
    pub fn is_enabled(&self, topic: &str, task_text: &str) -> (bool, Option<String>) {
        if self.when_any.is_empty() && self.unless_any.is_empty() {
            return (true, None);
        }
        let hay = match self.match_scope {
            MatchScope::Topic => topic.to_lowercase(),
            MatchScope::Task => task_text.to_lowercase(),
        };
        let scope = match self.match_scope {
            MatchScope::Topic => "topic",
            MatchScope::Task => "task",
        };
        if let Some(veto) = self.unless_any.iter().find(|k| keyword_hit(&hay, k)) {
            return (false, Some(format!("{scope} 命中排除词 '{veto}'")));
        }
        if self.when_any.is_empty() || self.when_any.iter().any(|k| keyword_hit(&hay, k)) {
            (true, None)
        } else {
            (
                false,
                Some(format!("{scope} 未命中 {:?}", self.when_any)),
            )
        }
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
            for role in step.declared_roles() {
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
            // step 级条件的形状校验。空关键词永不命中 = 静默失效。
            for kw in step.when_any.iter().chain(step.unless_any.iter()) {
                if kw.trim().is_empty() {
                    return Err(format!(
                        "step '{}': when_any/unless_any 的关键词不能为空串（永不命中）",
                        step.id
                    ));
                }
            }
            // 条件跳过的 step 若绑了 output_key，下游会拿到空串——这是
            // 设计如此（见 `is_enabled` 的说明），但**没有** output_key
            // 的条件 step 等于"跳过了也没人知道"，通常是配置写漏了。
            if (!step.when_any.is_empty() || !step.unless_any.is_empty())
                && step.output_key.is_none()
                && step.workflow.is_none()
            {
                return Err(format!(
                    "step '{}': 有 when_any/unless_any 时必须声明 output_key\
                     （否则跳过与执行对下游没有任何可观察差别，通常是配置写漏）",
                    step.id
                ));
            }
            // 可选 speaker 的形状校验。这些错法全都是「静默失效」——
            // 条件恒真等于白写、role 拼错等于永远不派、全员可选等于
            // step 可能零产出——配置期直接拦掉。
            if !step.optional_speakers.is_empty() {
                if step.workflow.is_some() {
                    return Err(format!(
                        "step '{}': `optional_speakers` 与嵌套 workflow 互斥（嵌套 step 没有 speaker）",
                        step.id
                    ));
                }
                let unconditional = step.roles();
                if unconditional.is_empty() {
                    return Err(format!(
                        "step '{}': 有 `optional_speakers` 时必须至少保留一个无条件 speaker \
                         （否则关键词全不命中时本 step 零 speaker、零产出）",
                        step.id
                    ));
                }
                let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
                for opt in &step.optional_speakers {
                    if opt.role.trim().is_empty() {
                        return Err(format!(
                            "step '{}': optional_speakers 的 role 不能为空",
                            step.id
                        ));
                    }
                    if unconditional.contains(&opt.role) {
                        return Err(format!(
                            "step '{}': optional_speakers 的 '{}' 已经是无条件 speaker，\
                             条件永远不生效（要么从 speakers 里移除，要么删掉这条）",
                            step.id, opt.role
                        ));
                    }
                    if !seen.insert(opt.role.as_str()) {
                        return Err(format!(
                            "step '{}': optional_speakers 里 '{}' 重复声明",
                            step.id, opt.role
                        ));
                    }
                    if opt.when_any.is_empty() && opt.unless_any.is_empty() {
                        return Err(format!(
                            "step '{}': optional_speakers['{}'] 的 when_any 与 unless_any 全空，\
                             条件恒真等于无条件 speaker——请写条件或直接放进 speakers",
                            step.id, opt.role
                        ));
                    }
                    for kw in opt.when_any.iter().chain(opt.unless_any.iter()) {
                        if kw.trim().is_empty() {
                            return Err(format!(
                                "step '{}': optional_speakers['{}'] 的关键词不能为空串（永不命中）",
                                step.id, opt.role
                            ));
                        }
                    }
                }
            }
            if step.output_from.is_some() && step.workflow.is_none() {                return Err(format!(
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
            // 工具行为字段的形状校验。空工具名会被 `short_tool_name`
            // 归一成空串、永远匹配不上任何调用，于是 `require_tools_any`
            // 恒失败、`tool_call_limits` 恒通过——两种都是静默失效，
            // 与"写了却不生效"是同一类事故。直接拒。
            for t in &step.require_tools_any {
                if t.trim().is_empty() {
                    return Err(format!(
                        "step '{}': require_tools_any 里的工具名不能为空",
                        step.id
                    ));
                }
            }
            for name in step.tool_call_limits.keys() {
                if name.trim().is_empty() {
                    return Err(format!(
                        "step '{}': tool_call_limits 的工具名不能为空",
                        step.id
                    ));
                }
            }
            // min/max 反了的话没有任何产出能通过，且报错会指向 min（先查
            // min），排查方向完全错。配置期就拦掉。
            if let (Some(min), Some(max)) =
                (step.output_contract.min_chars, step.output_contract.max_chars)
            {
                if min > max {
                    return Err(format!(
                        "step '{}': output_contract.min_chars ({min}) 大于 max_chars ({max})，\
                         没有任何产出能同时满足",
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
            // loop_no_release_on 与 loop_abort_on 同规则：只在返工环耗尽
            // 判定里生效，没有 loop_until 必然是笔误；含 loop_until 标记
            // 则永远先命中放行，成死代码。
            if let Some(marker) = &step.loop_no_release_on {
                if step.loop_until.is_none() {
                    return Err(format!(
                        "step '{}': `loop_no_release_on` 需要配合 `loop_until`（没有返工环时不存在耗尽放行）",
                        step.id
                    ));
                }
                if marker.trim().is_empty() {
                    return Err(format!(
                        "step '{}': loop_no_release_on must not be empty",
                        step.id
                    ));
                }
                if let Some(cond) = &step.loop_until {
                    if marker.contains(cond.as_str()) {
                        return Err(format!(
                            "step '{}': loop_no_release_on '{marker}' 包含 loop_until '{cond}'，\
                             永远不会触发（放行条件先命中）",
                            step.id
                        ));
                    }
                }
            }
            if step.loop_until.is_some() && uses_dag {
                // DAG 返工环（实测实锤：gate 的合法 REJECT 被契约
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
            // patch / 机械提交字段的形状校验（三者都只在串行引擎实现，
            // DAG 用到即报配置错误，不静默忽略）。
            if (step.require_plan_tasks || step.patch_from.is_some()
                || step.submit_plan_from.is_some())
                && uses_dag
            {
                return Err(format!(
                    "step '{}': require_plan_tasks / patch_from / submit_plan_from \
                     目前仅支持串行引擎（本 workflow 有 step 声明了 depends_on）",
                    step.id
                ));
            }
            // patch_from / submit_plan_from 必须指向**更早** step 的
            // output_key——指向自己或未来 step 在串行执行下永远拿到空串，
            // 是静默失效类错误。
            for (attr, src) in [
                ("patch_from", step.patch_from.as_deref()),
                ("submit_plan_from", step.submit_plan_from.as_deref()),
            ] {
                let Some(src) = src else { continue };
                if src.trim().is_empty() {
                    return Err(format!("step '{}': {attr} must not be empty", step.id));
                }
                let own_idx = self.steps.iter().position(|s| s.id == step.id);
                let ok = own_idx.is_some_and(|i| {
                    self.steps[..i]
                        .iter()
                        .any(|s| s.output_key.as_deref() == Some(src))
                });
                if !ok {
                    return Err(format!(
                        "step '{}': {attr} '{src}' 必须指向更早 step 的 output_key",
                        step.id
                    ));
                }
            }
            if step.submit_plan_from.is_some()
                && (step.role.is_some() || !step.speakers.is_empty())
            {
                return Err(format!(
                    "step '{}': `submit_plan_from` 是零模型调用的机械步，与 role/speakers 互斥",
                    step.id
                ));
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

/// 调研类流水线：产出只是**背景/事实/决策要点**，本身不是可交付物。
///
/// 之所以要单独识别它们：实测会话里 manager 连着跑了两轮 `explore`
/// 又追加两个「通读 / 解剖」delegate，四轮全在调研，用户点名要的
/// 任务清单一个都没产出（`.latte/tasks/board.json` 里 0 任务）。
///
/// **注意这里不设轮次上限。** 轮数是错的度量：大仓库分模块探三轮是
/// 健康的，原地把同一件事再探一遍才是病态的——两者轮数可能相同。
/// 按轮数刹车就是 `LATTE_AGENT_WORKFLOW_BUDGET_PER_UNIT_SECS`
/// （已移除）那个错误的翻版：只看计数、分不清"任务本身重"与"在空转"。
/// 判据只有一条二值事实：**用户点名的交付物有没有被产出**
/// （见 [`named_deliverables`]）。
///
/// `learn` / `write_doc` / `update_docs` **不在**此列：它们会落盘真实
/// 产物（`docs/learn/<slug>.md` 等），对"讲讲原理"这类诉求本身就是
/// 交付物。
pub fn is_research_workflow(name: &str) -> bool {
    matches!(name, "explore" | "design_brainstorm")
}

/// 规划类流水线：产出是可导入任务看板的任务清单（交付物）。
pub fn is_planning_workflow(name: &str) -> bool {
    matches!(
        name,
        "implementation_plan" | "task_refine" | "design_and_plan" | "feature_design"
    )
}

/// 交付物表：`(标签, 诉求里的关键词, 产出它该跑的流程, 能产出它的流程名)`。
///
/// 单一真源——`named_deliverables` / `deliverable_route` /
/// `deliverables_produced_by` 都读它，避免"识别出了交付物却指错流程"、
/// 或者"某个流程被算作关掉了它其实产不出的交付物"这两类不同步的错。
///
/// 第 3 项是给人看的路由说明（可以带括号注解），第 4 项是给引擎比对的
/// **纯流程名**列表。两者分开，否则记账只能去 substring 匹配一句人话。
const DELIVERABLES: [(&str, &[&str], &str, &[&str]); 4] = [
    (
        "任务清单",
        &["拆分任务", "拆任务", "任务清单", "任务列表", "看板", "排任务"],
        "implementation_plan（拿到清单后用 plan 提交给用户勾选导入看板）",
        &["implementation_plan", "task_refine", "design_and_plan", "feature_design"],
    ),
    (
        "计划",
        &["计划", "规划", "路线图", "roadmap"],
        "implementation_plan",
        &["implementation_plan", "task_refine", "design_and_plan", "feature_design"],
    ),
    (
        "教程 / 学习材料",
        &["教程", "讲讲", "学习", "入门", "科普"],
        "learn（会落盘 docs/learn/<slug>.md）",
        &["learn", "learn_loop"],
    ),
    (
        "文档",
        &["文档", "README", "写一篇", "报告"],
        "write_doc",
        &["write_doc", "update_docs"],
    ),
];

/// 从用户诉求里识别**点名的交付物**。
///
/// 只匹配用户明确说出来的产出物名词。诉求里没有任何交付物名词时返回
/// 空——那种情况下"只调研不产出"是完全正常的（「这块代码怎么回事」的
/// 答案就是调研本身），不该提醒、不该报警。
///
/// 这里刻意**没有**相似度阈值一类的可调参数。曾试过用字符 bigram
/// 相似度检测「把上一轮 topic 换几个词再探一遍」，实测在真实样本上
/// 分不开：那两次重复的 explore（jaccard 0.179 / containment 0.429）
/// 与一对确实不同的 topic（0.128 / 0.490）区间交叠，任何阈值都是
/// 拍脑袋，且会误杀正当的分模块深入。判据因此收敛成一个二值事实：
/// **用户点名的交付物有没有被产出**，不需要任何数字。
pub fn named_deliverables(user_text: &str) -> Vec<&'static str> {
    DELIVERABLES
        .iter()
        .filter(|(_, keys, _, _)| keys.iter().any(|k| user_text.contains(k)))
        .map(|(label, _, _, _)| *label)
        .collect()
}

/// 这个流程**能关掉**哪几个交付物标签。
///
/// 为什么必须按标签逐项记账、而不是记一个"产出过交付物"的布尔：用户
/// 一句话里点名两三样东西是常态（实测：「安排学习计划 + 拆分任务」同时
/// 命中`任务清单` / `计划` / `教程 / 学习材料` 三个标签）。布尔记账下，
/// 只要跑了 `learn`，缺口整体关闭——欠着的任务清单从此再没有任何机制
/// 会提起它。实测会话正是这样：manager 在正文里写明还要跑
/// `implementation_plan`，`learn` 一启动缺口就关了，看板至今 0 个任务。
pub fn deliverables_produced_by(workflow_name: &str) -> Vec<&'static str> {
    DELIVERABLES
        .iter()
        .filter(|(_, _, _, by)| by.contains(&workflow_name))
        .map(|(label, _, _, _)| *label)
        .collect()
}

/// `plan` 工具提交成功关掉的交付物标签：任务清单类。
///
/// 提交清单给用户勾选导入看板，是"任务清单 / 计划"这两个标签的终点
/// 动作——比跑规划流程更硬（流程只是产出草案，提交才进看板）。
pub fn deliverables_produced_by_plan_submit() -> Vec<&'static str> {
    DELIVERABLES
        .iter()
        .filter(|(_, _, _, by)| by.contains(&"implementation_plan"))
        .map(|(label, _, _, _)| *label)
        .collect()
}

/// 交付物 → 该跑哪个流程。未知标签回退到 `write_doc`（只可能来自
/// 调用方手写标签，正常路径的标签都来自 [`DELIVERABLES`]）。
fn deliverable_route(label: &str) -> &'static str {
    DELIVERABLES
        .iter()
        .find(|(l, _, _, _)| *l == label)
        .map(|(_, _, route, _)| *route)
        .unwrap_or("write_doc")
}

/// 调研类流水线跑完、且用户诉求里点名过交付物时，追加给 manager 的
/// 提醒。
///
/// **不限制任何东西**：不数轮次、不设阈值、不阻断。它只做一件事——
/// 把「用户还欠着什么」和「该跑哪个流程」摆在 manager 眼前。想接着
/// 分模块深入完全可以，只是别忘了欠着的东西。
///
/// 诉求里没点名交付物时调用方不会走到这里（`named` 为空），所以
/// 「这块代码怎么回事」这类纯咨询不会收到任何提醒。
pub fn deliverable_reminder(named: &[&str]) -> String {
    let routes: Vec<String> = named
        .iter()
        .map(|d| format!("  - **{d}** → 跑 `{}`", deliverable_route(d)))
        .collect();
    format!(
        "\n\n---\n\
         📌 **交付物提醒（系统注入，非专家结论）**：本轮是**调研**，产出是背景结论，不是交付物。\
         用户在诉求里点名要的东西还欠着：\n{}\n\n\
         这不限制你继续调研——分模块深入是正当的。但每再探一轮，请先说得出\
         **这一轮补的是哪一个具体问题**；说不出来（只是「再摸一遍结构」）就说明已有结论\
         足够动手，该转去产出上面欠着的东西了。\n\
         注意：把「通读 XXX 产出解剖报告」派成 delegate，与再跑一轮调研流程是同一件事。",
        routes.join("\n")
    )
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
    /// 也没有任何机制被唤醒，于是永久卡死（实测现场：
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
        // 嵌套 run 不自设预算（墙钟机制已废弃）：防挂死由下层 idle
        // 超时统一兜住。
        let (last, keyed) =
            run_workflow_inner(&wf, &topic, &nested_ctx, None).await?;
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
        /// 本 run 所属的 UI session。plan 工具的直提门禁靠它判断
        /// 「本 session 是否跑过 workflow」（扫 workflow-runs/ 各
        /// checkpoint 的首行）。旧 checkpoint 无此字段，serde
        /// default 兼容。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
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
    /// previous run's checkpoint. `session_id` 写进 meta 首行，供
    /// plan 工具的直提门禁按 session 查询「是否跑过 workflow」。
    fn new(cwd: &Path, wf_id: &str, workflow_name: &str, topic: &str, resumed: usize, session_id: Option<String>) -> Self {
        append_checkpoint(
            cwd,
            wf_id,
            &CheckpointRecord::Meta {
                wf_id: wf_id.to_string(),
                workflow_name: workflow_name.to_string(),
                topic: topic.to_string(),
                started_at: now_secs(),
                session_id,
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
    run_workflow_inner(wf, topic, ctx, None)
        .await
        .map(|(out, _)| out)
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
    run_workflow_inner(wf, topic, ctx, Some(resume_wf_id))
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
        ctx.session_id.clone(),
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

    // ── 不设 workflow 层 wall-clock 预算（deadline-only 已废弃）─────
    // 曾经这里按「分派单元数 × 每单元 420s」推算一个墙钟上限，跑满即
    // 中止整条流程。它的意图是防「流水线真卡死没人叫停」，但用错了
    // 信号维度：只看总时长，无法区分「后端卡死」和「任务本身就重」，
    // 于是持续在吐 token、完全健康的重任务（explore 单步产
    // 40KB 报告）被反复误杀，还诱导 resume 二次空等。
    //
    // 真正的「防挂死」由更精准的下层机制承担，无需这一层冗余墙钟：
    //   1. 单次模型调用的 TTFB/idle 流式超时（agent.rs）—— 后端零字节
    //      响应时秒级发现，冷却后沿模型链换下一个模型，全链都哑才升级
    //      为 ModelsUnavailable → 自动暂停等用户，全程不丢已有成果；
    //   2. 死循环熔断（连击 / 打转）—— 抓「空转」这个正交的真信号。
    // 合法长任务只要一直在产出就想跑多久跑多久。
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
    // 墙钟已移除：直接跑到引擎自然收尾（正常完成 / 用户取消 /
    // step 失败）。真卡死由下层 idle 超时 + 自动暂停接管，不在此中止。
    let outcome = engine.await;

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
    // plan 工具的 PlanStage 句柄：`Some` 时用调用方给的（调用方据此判断
    // plan 是否提交成功，见 `WorkflowStepDef::require_plan_submit`），
    // `None` 时自建私有句柄。
    plan_stage: Option<crate::controller::SharedPlanStage>,
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
        // delegate 工具，阶段门无人消费——调用方没给句柄时给一个私有的
        // 即可（plan 提案仍会广播 PlanProposed 事件，只是不驱动
        // delegate 门禁）。调用方给了句柄（require_plan_submit 的 step）
        // 则用它，让引擎能在分派结束后读到提交结果。
        if role.allowed_tools.iter().any(|t| t == "plan") {
            let plan_stage: crate::controller::SharedPlanStage = plan_stage
                .clone()
                .unwrap_or_else(|| {
                    Arc::new(parking_lot::RwLock::new(crate::controller::PlanStage::Normal))
                });
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
        // Tool not found（日志实锤：manager 在 decide 步调
        // ask 失败，选择框永远没弹出）。
        // 注意：子代理没有"下一轮"，ask 必须是阻塞模式——挂起等用户
        // 在弹框回答，答案经 /api/chat/choice-answer 直达本工具结果
        // （日志实锤：fire-and-forget 的"结束本轮等回答"语义
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
        // runner 的自动刹车只有死循环熔断；wall-clock 靠下面的
        // set_deadline。
        AgentRunner::new_with_tools(agent, rtm)
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
    /// 本 workflow 的 topic —— 对一个 step 来说，「主会话主题」就是整条
    /// workflow 的主题（嵌套时是当前这层的 topic，它本身就派生自父级）。
    /// 委派返回审查拿它当「是否符合预期」的准绳。
    main_topic: String,
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
    /// plan 工具的 PlanStage 句柄。`Some` 时由**引擎**持有（而不是
    /// `build_role_runner` 自建私有句柄），这样本次分派结束后引擎能读到
    /// 「plan 到底提交成功了没有」——`WorkflowStepDef::require_plan_submit`
    /// 的判据。`None` = 引擎不关心，runner 自建私有句柄（现状）。
    plan_stage: Option<crate::controller::SharedPlanStage>,
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
        // 本 workflow 的 topic（= 该 step 的「主会话主题」）。
        main_topic: String,
    ) -> Self {
        Self {
            speaker,
            step_id: step_id.to_string(),
            prompt,
            main_topic,
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
            // 默认不接管；需要 require_plan_submit 的 step 由引擎在
            // 构造后写入自己的句柄。
            plan_stage: None,
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
/// 循环被熔断中止时的 partial 降级判定。
///
/// 返回 `Some((notice, adopted))` 表示可以降级采纳：`notice` 发给 UI
/// （告知这一步是截断产出），`adopted` 是带截断标注、写进 vars 穿给下游
/// 的正文。返回 `None` 表示 partial 无实质内容，按失败处理。
///
/// 为什么必须降级而不是判死：「撞上限」≠「零产出」。实测实锤：
/// estimate 步跑了 105 轮、131 次工具调用，最后一条回复是完整的验证
/// 结论，却因为 step 判失败 → 嵌套 workflow 失败 → 父 workflow 失败，
/// 3 小时成果全丢。
///
/// 为什么要加截断标注：下游 step（评审、拍板）必须知道这份输入没经过
/// 作者收尾，否则会把半成品当终稿评审。
fn degrade_partial_on_break(
    speaker: &str,
    step_id: &str,
    cause: &str,
    partial: &str,
) -> Option<(String, String)> {
    let stripped = crate::controller::strip_think_blocks(partial);
    if crate::controller::is_empty_output(&stripped) {
        return None;
    }
    // `strip_think_blocks` 有个刻意的兜底：剥完为空时返回**原文**
    // （controller.rs:126-128），于是「只有思维链、没有正式回答」的
    // partial 剥完仍非空。正常收尾的回复走这条兜底无妨，但降级采纳不行
    // ——把裸思维链写进 vars 穿给下游评审，比判失败更糟。用「剥完仍带
    // <think>」判定兜底已触发。
    if stripped.contains("<think>") {
        return None;
    }
    let notice = format!(
        "⚠️ {speaker} 在 step '{step_id}' 因{cause}被中止，\
         已降级采纳其未收尾产出（{} 字符）——下游请把它当\"未完成的中间结论\"看待，\
         不要视为终稿。",
        stripped.chars().count()
    );
    let adopted = format!(
        "{stripped}\n\n---\n[本产出因{cause}被截断，未经作者收尾]"
    );
    Some((notice, adopted))
}

/// [`run_step_speaker`] 的产出：模型正文 + 本轮**真实发生过**的工具调用摘要。
///
/// 带出 `tools` 是为了让引擎能校验 `require_tools`——只看正文无法区分
/// "调用了 ask" 和 "编了一段像调用过 ask 的话"。
struct SpeakerOut {
    response: String,
    /// runner 收集的工具摘要，每行形如 `"{name} {args} → {result}"`
    /// （见 `AgentRunner::take_last_turn_tool_summary`）。一次工具都没调时
    /// 是 `"[本 step 工具调用数: 0]"` 这样的占位串，不含任何工具名。
    tools: String,
}

async fn run_step_speaker(inp: SpeakerDispatch) -> Result<SpeakerOut, StepFail> {

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

    // 2-4. 执行 + advisor 返回审查的返工环：返回被判 intervene/terminate
    //    时带【上轮审查反馈】返工——advisor 的「打回」不再只是批注
    //    （实测实锤：reviewer 空转被 advisor 抓到、hint 要求重做，
    //    流水线却照流不误）。返工分两种：remedy=patch（默认）续作复用
    //    同一 runner 的 context 修补；remedy=restart 才废弃重建。
    //    上限可配（AdvisorMonitorConfig::
    //    review_settings.return_max_redo，经 engine 注入），默认 1、
    //    硬上限 MAX_RETURN_REDO。
    let max_redo = inp
        .review_engine
        .as_ref()
        .map(|e| e.return_max_redo())
        .unwrap_or(crate::advisor_monitor::DEFAULT_RETURN_MAX_REDO);
    let mut prompt_for_turn = inp.prompt.clone();
    let mut redo: u8 = 0;
    // 续作句柄：advisor 打回判 remedy=patch 时**复用同一 runner** 追加
    // 一轮修补 turn——它的 context 里保留着全部已完成的探索与工具取证
    // （AgentRunner::run_turn 把每轮 user/assistant 消息持久进 context，
    // 见 agent.rs），修订只需一两次模型调用；而不是整轮重跑（实测实锤：
    // 46 万 input tokens 的探索型分派被 intervene 打回后全部作废重烧）。
    // 仅 remedy=restart（产出基于虚构/方向全错，续作不如重来）时清空
    // 此槽位，下一次循环重建全新 runner。
    let mut held_runner: Option<AgentRunner> = None;
    let mut role_responsibilities = String::new();
    let result: Result<SpeakerOut, StepFail> = loop {
        // 2. Runner：patch 续作复用 held_runner；首发与 restart 重建
        //    （全新 subagent，无跨次记忆）+ sink + gate。
        let mut runner = match held_runner.take() {
            Some(r) => r,
            None => {
                let (mut r, resp) = match build_role_runner(
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
                    inp.plan_stage.clone(),
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
                role_responsibilities = resp;
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
                    r = r.with_sink(specialist_sink);
                }
                if let Some(gate) = inp.advisor_gate.clone() {
                    r = r.with_gate_config(gate);
                }
                // intervene 暂停门也装到专家 runner：运行中判 Intervene 时在
                // tool-round 边界 park，直到用户拍板（或超时自动恢复）。
                if let Some(gate) = &inp.advisor_pause {
                    r = r.with_pause_gate(gate.clone());
                }
                r
            }
        };
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
        // 循环内 deadline（对齐 oh-my-pi）：在 runner 被 move 进 spawn 之前设好。
        let step_timeout_s = crate::controller::specialist_timeout_secs(None);
        runner.set_deadline(std::time::Instant::now() + std::time::Duration::from_secs(step_timeout_s));
        let mut run_handle = tokio::spawn(async move {
            let result = runner.run_turn_gated(&[Message::user(prompt)], None).await;
            let tool_count = runner.last_turn_tool_count;
            let tool_summary = runner.take_last_turn_tool_summary();
            // 失败路径也带出 tool_count / summary：死循环熔断的降级采纳
            // 需要它们（被熔断的 step 恰恰是工具用得最多的 step，摘要对
            // 下游最有价值）。
            // runner 一并带回：成功路径下 advisor 判 patch 续作时复用
            // 它的 context（见 held_runner），不必重建。
            (runner, result, tool_count, tool_summary)
        });
        // 外层 future 被 drop（上层取消 / 用户终止）时连带 abort 这个
        // spawn 出去的分派。`JoinHandle` 自己 drop **不取消**任务，光靠
        // "drop 引擎 future" 并不能停掉 specialist：否则被取消后 architect
        // 子会话会变成孤儿任务继续写，token 白烧且产出没人再要。
        let _abort_on_drop = crate::sub_cancel::AbortOnDrop(run_handle.abort_handle());
        // 熔断：wall-clock 超时（对齐 controller delegate；被 500ms
        // 轮询分支重建的 sleep 永远不响，必须在循环外 pin 住）。
        // 超时走 StepFail::Failed → 引擎按 max_retries 重试/失败冒泡，
        // 而不是无限挂起（实测事故：estimate 步挂 24min）。
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
                        Ok((returned, Ok(response), tool_count, tool_summary)) => {
                            // runner 放回槽位：成功产出若被 advisor 判
                            // patch 续作，下一轮复用它的 context。
                            held_runner = Some(returned);
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
                        Ok((returned, Err(e), tool_count, tool_summary)) => {
                            // 失败路径不复用 runner：context 可能带着半截
                            // 脏 turn；redo 环对 Err 也只会 break 出去，
                            // 这里直接丢弃。
                            drop(returned);
                            // ── 死循环熔断的降级采纳 ──────────────────
                            //
                            // 「撞上限」≠「零产出」。此前这里把
                            // 死循环熔断要走降级采纳，不能一律判
                            // StepFail::Failed —— 否则 step 失败 → 嵌套
                            // workflow 失败 → 父 workflow 失败，模型已写好
                            // 的正文全丢（实测实锤：estimate 步 105 轮、
                            // 131 次工具调用，最后一条回复是完整结论，整条
                            // design_and_plan 仍被判死，3 小时零产出）。
                            //
                            // 有实质 partial 就带截断标注采纳，让下游 step
                            // 拿到证据继续跑；partial 为空才按失败处理。
                            if let crate::error::AgentError::ToolLoopDetected {
                                tool,
                                partial,
                                ..
                            } = &e
                            {
                                if let Some((notice, adopted)) = degrade_partial_on_break(
                                    &speaker,
                                    &inp.step_id,
                                    &format!("检测到工具死循环（'{tool}' 反复调用）"),
                                    partial,
                                ) {
                                    let _ = inp
                                        .event_tx
                                        .send(ChatEvent::Status { message: notice });
                                    attempt = Ok((adopted, tool_count, tool_summary));
                                    break;
                                }
                            }
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
                // 哪些工具（实测实锤：interview step 用 ask 收齐答案
                // 后输出 user_profile，advisor 看不到 ask 记录→误判伪造）。
                // 摘要由 runner 收集（take_last_turn_tool_summary），
                // 含工具名 + 参数 + 结果，不再是裸计数。
                let (annotated, verdict) = crate::controller::gate_delegate_return(
                    engine,
                    &inp.event_tx,
                    &inp.main_topic,

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
                    // 处置分流：patch（默认）复用 held_runner 在同一
                    // subsession 续作修补；restart 清空槽位，下一次循环
                    // 重建全新 subagent（仅在 advisor 判定续作不如重做时）。
                    let restart =
                        matches!(v.remedy, crate::advisor_monitor::Remedy::Restart);
                    if restart {
                        held_runner = None;
                    }
                    let action = if restart { "重做" } else { "续作修补" };
                    let _ = inp.event_tx.send(ChatEvent::Status {
                        message: format!(
                            "↩ advisor 判定 {speaker} 的返回未达标，带审查意见{action}（第 {redo}/{max_redo} 次）"
                        ),
                    });
                    // 与下一次派发的 RoleStarted 配平。
                    let _ = inp.event_tx.send(ChatEvent::RoleFinished {
                        role_id: speaker.clone(),
                        detail: format!("advisor intervene：带审查意见{action}"),
                        sub_id: sub_id.clone(),
                    });
                    let feedback = if v.hint.is_empty() {
                        v.reason.clone()
                    } else {
                        format!("{}
处理建议：{}", v.reason, v.hint)
                    };
                    prompt_for_turn = if restart {
                        // 全新 subagent 没有上下文：重发原始任务 + 反馈。
                        format!("{}

【上轮审查反馈】
{}", inp.prompt, feedback)
                    } else {
                        // 续作：原始任务与上轮产出已在 runner context 里，
                        // 只发反馈 + 修补指令——重发原始任务会让模型误以为
                        // 要推倒重来。
                        format!(
                            "【审查打回·续作】你的上一版产出未通过返回审查。\n\n【上轮审查反馈】\n{feedback}\n\n\
                             请在**已有产出与已完成取证**的基础上修订后重新作答——\
                             不要重复已做过的探索/读取，只补必要的新取证。\
                             若审查意见不成立，说明理由后给出修订版。"
                        )
                    };
                    continue;
                }
                break Ok(SpeakerOut { response: annotated, tools: tool_summary });
            }
            (other, _) => {
                break other.map(|(r, _, t)| SpeakerOut { response: r, tools: t })
            }
        }
    };

    // 5. 收尾事件（RoleTurn 由引擎的 WorkflowTurn 承担，不重复发）。
    match &result {
        Ok(out) => {
            let response = &out.response;
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
            // step 级条件：不命中就整步跳过。output_key 绑空串——不绑的话
            // 下游 prompt 里的 `{{key}}` 会原样留着，模型看到字面占位符。
            {
                let mut probe = vars.clone();
                probe.insert("step_id".into(), step.id.clone());
                let task_text = wf.render_task(step, &probe);
                let topic_s = vars.get("topic").map(|s| s.as_str()).unwrap_or("");
                if let (false, reason) = step.is_enabled(topic_s, &task_text) {
                    let why = reason.unwrap_or_default();
                    let _ = ctx.event_tx.send(ChatEvent::Status {
                        message: format!(
                            "[workflow '{}'] 跳过 step '{}'（{why}）",
                            wf.name, step.id
                        ),
                    });
                    if let Some(key) = &step.output_key {
                        vars.insert(key.clone(), String::new());
                        keyed.insert(key.clone(), String::new());
                    }
                    ckpt.record_step(&step.id, step.output_key.as_deref(), "");
                    idx += 1;
                    continue;
                }
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
            // 机械提交步（submit_plan_from）：零模型调用——引擎直接从
            // 上游产出抽 tasks JSON，走 plan 工具的共享实现提交。
            // 校验失败 = step 失败：上游 require_plan_tasks / patch_from
            // 已两道硬校验，这里失败说明有未知路径，响亮报错不静默。
            if let Some(src_key) = &step.submit_plan_from {
                let src = vars.get(src_key).cloned().unwrap_or_default();
                let result = find_tasks_fence(&src).and_then(|(_, tasks)| {
                    crate::controller::submit_plan_proposal(
                        &serde_json::json!({ "tasks": tasks }),
                        // 本步没有 speaker；role_id 只用于 plan_id 与
                        // 前端展示，用 workflow 名标识来源。
                        &format!("workflow:{}", wf.name),
                        &Arc::new(parking_lot::RwLock::new(
                            crate::controller::PlanStage::Normal,
                        )),
                        &ctx.cwd,
                        ctx.session_id.as_deref().unwrap_or(""),
                        &ctx.event_tx,
                    )
                });
                match result {
                    Ok(summary) => {
                        let _ = ctx.event_tx.send(ChatEvent::WorkflowTurn {
                            wf_id: wf_id.to_string(),
                            step_id: step.id.clone(),
                            role_id: format!("workflow:{}", wf.name),
                            content: summary.clone(),
                            round,
                        });
                        last_output = summary;
                        if let Some(key) = &step.output_key {
                            vars.insert(key.clone(), last_output.clone());
                            keyed.insert(key.clone(), last_output.clone());
                        }
                        ckpt.record_step(&step.id, step.output_key.as_deref(), &last_output);
                        idx += 1;
                        continue;
                    }
                    Err(e) => {
                        return WfOutcome::Failed(format!(
                            "step '{}' 机械提交 plan 失败：{e}",
                            step.id
                        ))
                    }
                }
            }
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
            // 可选 speaker 过滤：派发前按 topic/任务文本决定阵容，零模型
            // 调用。跳过的角色发一条 Status，UI 上能看到「这一步为什么
            // 少了个人」。
            let (active_speakers, skipped_speakers) = {
                let mut probe_vars = vars.clone();
                probe_vars.insert("step_id".into(), step.id.clone());
                let task_text = wf.render_task(step, &probe_vars);
                step.active_roles(vars.get("topic").map(|s| s.as_str()).unwrap_or(""), &task_text)
            };
            for s in &skipped_speakers {
                let _ = ctx.event_tx.send(ChatEvent::Status {
                    message: format!(
                        "[workflow '{}' step '{}'] 跳过可选角色 {}（{}）",
                        wf.name, step.id, s.role, s.reason
                    ),
                });
            }
            // **每次进入 step 都重置**：loop_until 返工会重入同一个 step，
            // 声明在 workflow 层会把上一轮的产出也累进来（实测：既有测试
            // `serial_loop_rework_then_pass` 里同时出现 VERDICT: FAIL 与
            // VERDICT: PASS 两轮内容）。
            let mut speaker_outputs: Vec<(String, String)> = Vec::new();
            for speaker in active_speakers {
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
                    // require_plan_submit 的 step：每次尝试都用一个新的
                    // PlanStage，避免上一次尝试的成功状态污染本次判定。
                    let plan_stage: Option<crate::controller::SharedPlanStage> =
                        if step.require_plan_submit {
                            Some(Arc::new(parking_lot::RwLock::new(
                                crate::controller::PlanStage::Normal,
                            )))
                        } else {
                            None
                        };
                    let mut dispatch = SpeakerDispatch::from_ctx(
                        ctx,
                        review_engine.clone(),
                        wf_id,
                        &step.id,
                        speaker.clone(),
                        prompt.clone(),
                        step.tools.clone(),
                        answer_log.clone(),
                        topic.to_string(),
                    );
                    dispatch.plan_stage = plan_stage.clone();
                    let (mut response, tools_called) = match run_step_speaker(dispatch).await {
                        Ok(o) => (o.response, o.tools),
                        Err(StepFail::Cancelled) => return WfOutcome::Cancelled,
                        Err(StepFail::Failed(e)) => {
                            return WfOutcome::Failed(format!(
                                "step '{}' speaker '{}': {e}",
                                step.id, speaker
                            ))
                        }
                    };
                    // plan 提交硬校验：**不走 advisor 语义兜底**。
                    // 「零任务入库」没有语义解释空间——放行只会让
                    // workflow 继续报 ok（实测
                    // 实测实录）。
                    if step.require_plan_submit && !plan_submitted(plan_stage.as_ref()) {
                        if attempt >= step.max_retries {
                            return WfOutcome::Failed(format!(
                                "step '{}' speaker '{}': 本步要求成功提交 plan 提案，\
                                 重试 {attempt} 次后仍未提交成功（任务未进看板）\
                                 ；最后一次产出摘要：{}",
                                step.id,
                                speaker,
                                output_excerpt(&response, 1200)
                            ));
                        }
                        attempt += 1;
                        tracing::warn!(
                            step = %step.id,
                            speaker = %speaker,
                            attempt,
                            "workflow step did not submit a plan proposal; retrying"
                        );
                        let annotation = plan_submit_retry_annotation(&response);
                        step_transcript.push_str("[验收批注]: 未成功提交 plan 提案，已要求重提\n");
                        prompt = format!("{full_prompt}\n\n{annotation}");
                        continue;
                    }
                    // 工具使用行为（require_tools / require_tools_any / tool_call_limits）：
            // 查它**做了什么**（output_contract 只能查它说了什么）。
                    // 放在 output_contract 之前——「根本没调工具」比「正文缺字样」更根本，
                    // 批注也更有指导性。
                    if let Err(reason) = check_tool_usage(step, &tools_called) {
                        if attempt >= step.max_retries {
                            return WfOutcome::Failed(format!(
                                "step '{}' speaker '{}': {reason}（已重试 {attempt} 次）",
                                step.id, speaker
                            ));
                        }
                        attempt += 1;
                        tracing::warn!(
                            step = %step.id, speaker = %speaker, attempt,
                            "workflow step skipped a required tool; retrying"
                        );
                        step_transcript.push_str("[验收批注]: 未调用必需工具，已要求重做\n");
                        prompt = format!("{full_prompt}\n\n[验收批注]\n{reason}");
                        continue;
                    }
                    if let Err(reason) = check_output_contract(&step.output_contract, &response) {
                        if attempt >= step.max_retries {
                            // 重试耗尽：判死前过一道 advisor 语义兜底
                            // （字符串契约分不清格式错误与合法但无标记
                            // 的产出——实测实锤 gate REJECT 判死）。
                            match contract_last_resort_review(
                                &review_engine,
                                &ctx.event_tx,
                                topic,
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
                    // 符号可解析性（require_symbols_resolvable）：产出里反引号
                    // 包裹的代码符号必须能在仓库里回指到。放在清单硬校验之前
                    // ——「引用的函数根本不存在」比「JSON 字段不齐」更根本，
                    // 而且这条挡的正是 require_tools_any 挡不住的那部分：
                    // 工具调了、但对着没打开过的文件凭记忆写符号名。
                    if step.require_symbols_resolvable {
                        if let Err(reason) = check_symbols_output(&response, &ctx.cwd) {
                            if attempt >= step.max_retries {
                                return WfOutcome::Failed(format!(
                                    "step '{}' speaker '{}': {reason}（已重试 {attempt} 次）",
                                    step.id, speaker
                                ));
                            }
                            attempt += 1;
                            tracing::warn!(
                                step = %step.id, speaker = %speaker, attempt,
                                "workflow step referenced unresolvable symbols; retrying"
                            );
                            step_transcript
                                .push_str("[验收批注]: 引用了仓库中不存在的代码符号，已要求核对后重做\n");
                            prompt = format!("{full_prompt}\n\n[验收批注]\n{reason}");
                            continue;
                        }
                    }
                    // 清单 JSON 硬校验（require_plan_tasks）：与 plan 工具
                    // 同一套机械判据——paths 重叠自查从「模型自觉画表」变成
                    // 「引擎算的」（实测模型自查多次漏判、带病入库）。
                    if step.require_plan_tasks {
                        if let Err(reason) = check_plan_tasks_output(&response, &ctx.cwd) {
                            if attempt >= step.max_retries {
                                return WfOutcome::Failed(format!(
                                    "step '{}' speaker '{}': {reason}（已重试 {attempt} 次）\
                                     ；不合格产出摘要：{}",
                                    step.id,
                                    speaker,
                                    output_excerpt(&response, 1200)
                                ));
                            }
                            attempt += 1;
                            tracing::warn!(
                                step = %step.id, speaker = %speaker, attempt,
                                reason = %reason,
                                "workflow step output failed plan-tasks check; retrying"
                            );
                            let annotation =
                                format!("上次产出未通过验收：{reason}。请修正后重新产出完整结果。");
                            step_transcript.push_str(&format!("[验收批注]: {annotation}\n"));
                            prompt = format!("{full_prompt}\n\n{annotation}");
                            continue;
                        }
                    }
                    // patch 模式（patch_from）：产出是「修订说明 + ops」，
                    // 引擎机械打到上游草案上，本步产出 = 打过补丁的完整草案
                    // （下游拿到的仍是全量文本，只有模型那一跳是增量）。
                    if let Some(src_key) = &step.patch_from {
                        let draft = vars.get(src_key).cloned().unwrap_or_default();
                        match apply_task_patch(&draft, &response, &ctx.cwd) {
                            Ok(patched) => response = patched,
                            Err(reason) => {
                                if attempt >= step.max_retries {
                                    return WfOutcome::Failed(format!(
                                        "step '{}' speaker '{}': patch 应用失败\
                                         （已重试 {attempt} 次）：{reason}\
                                         ；不合格产出摘要：{}",
                                        step.id,
                                        speaker,
                                        output_excerpt(&response, 1200)
                                    ));
                                }
                                attempt += 1;
                                tracing::warn!(
                                    step = %step.id, speaker = %speaker, attempt,
                                    reason = %reason,
                                    "workflow step patch failed; retrying"
                                );
                                let annotation = format!(
                                    "上次产出的 patch 未通过机械校验：{reason}。\
                                     请修正 ops 后重新输出（只输出修订说明 + 一个 \
                                     ```json ops 代码块，不要重出完整清单）。"
                                );
                                step_transcript.push_str(&format!("[验收批注]: {annotation}\n"));
                                prompt = format!("{full_prompt}\n\n{annotation}");
                                continue;
                            }
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
                    // 收集而非覆盖：见 `aggregate_step_output`。
                    speaker_outputs.push((speaker.clone(), response));
                    break;
                }
            }
            // 聚合本 step 全部 speaker 的产出。单 speaker 时逐字等于原
            // 产出，所以既有 workflow 的下游输入不变。
            last_output = aggregate_step_output(&speaker_outputs);
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
                        // 「不可放行」标记：gate 已给出带证据的硬拒绝
                        // （如 VERDICT: REJECT），不许 advisor 复核把
                        // 失败吞成成功，直接判死（见 loop_no_release_on）。
                        if let Some(marker) = &step.loop_no_release_on {
                            if last_output.contains(marker.as_str()) {
                                return WfOutcome::Failed(format!(
                                    "step '{}' 循环条件「{cond}」在 {count} 次迭代后仍未满足\
                                     （已达 max_iterations={max}），产出含不可放行标记「{marker}」\
                                     （跳过 advisor 复核），最后一次产出摘要：{}",
                                    step.id,
                                    last_output.chars().take(200).collect::<String>()
                                ));
                            }
                        }
                        // 判死前最后一道 advisor 语义复核：把「被返工的那份
                        // 产出」+「未消化的评审意见」交给 advisor，判定实质
                        // 合格则放行进入下游（见 loop_exhausted_last_resort_review）。
                        let target_id =
                            step.loop_back_to.as_deref().unwrap_or(step.id.as_str());
                        let artifact = wf
                            .steps
                            .iter()
                            .find(|s| s.id == target_id)
                            .and_then(|s| s.output_key.as_ref())
                            .and_then(|k| vars.get(k).cloned())
                            .unwrap_or_default();
                        if loop_exhausted_last_resort_review(
                            &review_engine,
                            &ctx.event_tx,
                            topic,
                            &step.id,
                            cond,
                            count,
                            &artifact,
                            &last_output,
                        )
                        .await
                        {
                            idx += 1;
                            continue;
                        }
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
    // 本 step 内各 speaker 的产出，按声明顺序。step 收尾时聚合成
    // `last_output`（见 `aggregate_step_output`）。
    let mut speaker_outputs: Vec<(String, String)> = Vec::new();
    // 可选 speaker 过滤（同串行引擎，见 WorkflowStepDef::active_roles）。
    let (active_speakers, skipped_speakers) = {
        let mut probe_vars = inp.vars.clone();
        probe_vars.insert("step_id".into(), step.id.clone());
        let task_text = inp.wf.render_task(step, &probe_vars);
        step.active_roles(
            inp.vars.get("topic").map(|s| s.as_str()).unwrap_or(""),
            &task_text,
        )
    };
    for s in &skipped_speakers {
        let _ = inp.event_tx.send(ChatEvent::Status {
            message: format!(
                "[workflow '{}' step '{}'] 跳过可选角色 {}（{}）",
                inp.wf.name, step.id, s.role, s.reason
            ),
        });
    }
    for speaker in active_speakers {
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
            // 同串行引擎：require_plan_submit 的 step 每次尝试新建
            // PlanStage，引擎据此判断 plan 是否真的提交成功。
            let plan_stage: Option<crate::controller::SharedPlanStage> =
                if step.require_plan_submit {
                    Some(Arc::new(parking_lot::RwLock::new(
                        crate::controller::PlanStage::Normal,
                    )))
                } else {
                    None
                };
            let dispatch = SpeakerDispatch {
                speaker: speaker.clone(),
                step_id: step.id.clone(),
                prompt: prompt.clone(),
                // topic 由 run_workflow_{serial,dag} 在入口写入 vars，
                // 嵌套 step 取到的是当前这层的 topic。
                main_topic: inp.vars.get("topic").cloned().unwrap_or_default(),
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
                plan_stage: plan_stage.clone(),
            };
            let (mut response, tools_called) = match run_step_speaker(dispatch).await {
                Ok(o) => (o.response, o.tools),
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
            // plan 提交硬校验：不走 advisor 语义兜底（同串行引擎）。
            if step.require_plan_submit && !plan_submitted(plan_stage.as_ref()) {
                if attempt >= step.max_retries {
                    return Err(StepFail::Failed(format!(
                        "step '{}' speaker '{}': 本步要求成功提交 plan 提案，\
                         重试 {attempt} 次后仍未提交成功（任务未进看板）\
                         ；最后一次产出摘要：{}",
                        step.id,
                        speaker,
                        output_excerpt(&response, 1200)
                    )));
                }
                attempt += 1;
                tracing::warn!(
                    step = %step.id,
                    speaker = %speaker,
                    attempt,
                    "workflow step did not submit a plan proposal; retrying"
                );
                let annotation = plan_submit_retry_annotation(&response);
                step_transcript.push_str("[验收批注]: 未成功提交 plan 提案，已要求重提\n");
                prompt = format!("{full_prompt}\n\n{annotation}");
                continue;
            }
            // 工具使用行为（require_tools / require_tools_any / tool_call_limits）：
            // 查它**做了什么**（output_contract 只能查它说了什么）。
            // 放在 output_contract 之前——「根本没调工具」比「正文缺字样」更根本，
            // 批注也更有指导性。
            if let Err(reason) = check_tool_usage(step, &tools_called) {
                if attempt >= step.max_retries {
                    return Err(StepFail::Failed(format!(
                        "step '{}' speaker '{}': {reason}（已重试 {attempt} 次）",
                        step.id, speaker
                    )));
                }
                attempt += 1;
                tracing::warn!(
                    step = %step.id, speaker = %speaker, attempt,
                    "workflow step skipped a required tool; retrying"
                );
                step_transcript.push_str("[验收批注]: 未调用必需工具，已要求重做\n");
                prompt = format!("{full_prompt}\n\n[验收批注]\n{reason}");
                continue;
            }
            // 符号可解析性（require_symbols_resolvable）：与串行引擎同语义。
            // 这条是纯产出检查、无跨步状态，所以 DAG 引擎也支持（不像
            // require_plan_tasks / patch_from 那样只能串行）。
            if step.require_symbols_resolvable {
                if let Err(reason) = check_symbols_output(&response, &inp.cwd) {
                    if attempt >= step.max_retries {
                        return Err(StepFail::Failed(format!(
                            "step '{}' speaker '{}': {reason}（已重试 {attempt} 次）",
                            step.id, speaker
                        )));
                    }
                    attempt += 1;
                    tracing::warn!(
                        step = %step.id, speaker = %speaker, attempt,
                        "workflow step referenced unresolvable symbols; retrying"
                    );
                    step_transcript
                        .push_str("[验收批注]: 引用了仓库中不存在的代码符号，已要求核对后重做\n");
                    prompt = format!("{full_prompt}\n\n[验收批注]\n{reason}");
                    continue;
                }
            }
            if let Err(reason) = check_output_contract(&step.output_contract, &response) {
                if attempt >= step.max_retries {
                    // 重试耗尽：判死前过一道 advisor 语义兜底（同串行
                    // 引擎；实测实锤 gate REJECT 被契约判死）。
                    match contract_last_resort_review(
                        &inp.review_engine,
                        &inp.event_tx,
                        &inp.vars.get("topic").cloned().unwrap_or_default(),
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
            // 收集而非覆盖：见 `aggregate_step_output`。
            speaker_outputs.push((speaker.clone(), response));
            break;
        }
    }
    // 同串行引擎：聚合本 step 全部 speaker 的产出。
    last_output = aggregate_step_output(&speaker_outputs);
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
                // step 级条件（同串行引擎）：不命中就不派发，output_key
                // 绑空串，下游照常推进（skip 不阻塞 depends_on）。
                {
                    let step = &wf.steps[idx];
                    let mut probe = vars.clone();
                    probe.insert("step_id".into(), step.id.clone());
                    let task_text = wf.render_task(step, &probe);
                    let topic_s = vars.get("topic").map(|s| s.as_str()).unwrap_or("");
                    if let (false, reason) = step.is_enabled(topic_s, &task_text) {
                        let why = reason.unwrap_or_default();
                        let _ = ctx.event_tx.send(ChatEvent::Status {
                            message: format!(
                                "[workflow '{}'] 跳过 step '{}'（{why}）",
                                wf.name, step.id
                            ),
                        });
                        if let Some(key) = &step.output_key {
                            vars.insert(key.clone(), String::new());
                            keyed.insert(key.clone(), String::new());
                        }
                        ckpt.record_step(&step.id, step.output_key.as_deref(), "");
                        continue;
                    }
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
            // 3、硬上限 10，耗尽才 failed——实测实锤：gate 的
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
                    // 「不可放行」标记（与串行引擎同款）：硬拒绝不被
                    // advisor 复核放行（见 loop_no_release_on）。
                    if let Some(marker) = &step.loop_no_release_on {
                        if out.contains(marker.as_str()) {
                            return WfOutcome::Failed(format!(
                                "step '{}' 循环条件「{cond}」在 {count} 次迭代后仍未满足\
                                 （已达 max_iterations={max}），产出含不可放行标记「{marker}」\
                                 （跳过 advisor 复核），最后一次产出摘要：{}",
                                step.id,
                                out.chars().take(200).collect::<String>()
                            ));
                        }
                    }
                    // 判死前最后一道 advisor 语义复核（与串行引擎同款）：
                    // 把「被返工的那份产出」+「未消化的评审意见」交 advisor，
                    // 判定实质合格则免除本 step 的返工要求，让 wave 正常推进。
                    // DAG 下 loop_back_to 必填且指向更早 wave，故直接取它。
                    // 实际暴露面：design_and_plan 既是 DAG 又配了返工环
                    // （loop_until = "VERDICT: PASS", max_iterations = 3）。
                    let target_id =
                        step.loop_back_to.clone().unwrap_or_else(|| step.id.clone());
                    let artifact = wf
                        .steps
                        .iter()
                        .find(|s| s.id == target_id)
                        .and_then(|s| s.output_key.as_ref())
                        .and_then(|k| vars.get(k).cloned())
                        .unwrap_or_default();
                    if loop_exhausted_last_resort_review(
                        &review_engine,
                        &ctx.event_tx,
                        topic,
                        &step.id,
                        cond,
                        count,
                        &artifact,
                        out,
                    )
                    .await
                    {
                        continue;
                    }
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
    fn research_and_planning_workflows_are_disjoint() {
        // explore 的产出是"决策背景要点"——调研，不是交付物。
        assert!(is_research_workflow("explore"));
        assert!(is_research_workflow("design_brainstorm"));
        // 规划类产出可导入看板的任务清单。
        assert!(is_planning_workflow("implementation_plan"));
        assert!(is_planning_workflow("task_refine"));
        // 两类互斥：任何名字不能同时算调研和交付物。
        for n in [
            "explore",
            "design_brainstorm",
            "implementation_plan",
            "task_refine",
            "design_and_plan",
            "feature_design",
        ] {
            assert!(
                !(is_research_workflow(n) && is_planning_workflow(n)),
                "{n} classified as both"
            );
        }
    }

    #[test]
    fn learn_and_doc_workflows_are_not_research() {
        // learn / write_doc 会落盘真实产物（docs/learn/<slug>.md），
        // 对"讲讲原理"这类诉求本身就是交付物——不能被重复调研判定误伤。
        for n in ["learn", "learn_loop", "write_doc", "update_docs"] {
            assert!(!is_research_workflow(n), "{n} must not count as research");
        }
    }

    /// 实测那条诉求点名了两个交付物，必须都识别出来。
    #[test]
    fn named_deliverables_recognizes_the_observed_request() {
        let named = named_deliverables("查看代码  安排学习计划  拆分任务（从浅到深学习）");
        assert!(named.contains(&"任务清单"), "{named:?}");
        assert!(named.contains(&"计划"), "{named:?}");
        assert!(named.contains(&"教程 / 学习材料"), "{named:?}");
    }

    /// 不误报的底线：纯咨询类问句没有交付物，调研本身就是答案。
    #[test]
    fn pure_consultation_names_no_deliverable() {
        for q in [
            "这个函数为什么会崩？",
            "arena 和 extent 之间的锁顺序是怎么回事，有什么坑",
            "帮我看下 free_fastpath 的边界条件",
        ] {
            assert!(named_deliverables(q).is_empty(), "误判有交付物: {q}");
        }
    }

    #[test]
    fn reminder_names_what_is_owed_and_which_workflow() {
        let named = named_deliverables("帮我拆分任务");
        let r = deliverable_reminder(&named);
        assert!(r.contains("任务清单"), "{r}");
        // 光说"你还欠着"没用，必须点名该跑哪个流程。
        assert!(r.contains("implementation_plan"), "{r}");
        assert!(r.contains("plan"), "{r}");
        // 明确声明不是限制，避免 manager 把它读成禁令而停止正当调研。
        assert!(r.contains("不限制你继续调研"), "{r}");
        // 变相再探一轮也要点名。
        assert!(r.contains("解剖报告"), "{r}");
    }

    #[test]
    fn every_recognized_deliverable_has_a_route() {
        // 单一真源自检：识别得出的每个标签都必须能查到流程，
        // 否则会出现"知道用户要什么、却指不出该跑哪个流程"。
        for (label, keys, route, produced_by) in DELIVERABLES {
            assert!(!route.is_empty(), "{label} 缺少流程");
            assert_eq!(deliverable_route(label), route, "{label} 路由不一致");
            for k in keys {
                assert!(
                    named_deliverables(k).contains(&label),
                    "关键词 {k} 未能识别出 {label}"
                );
            }
            // `produced_by` 是记账用的**纯流程名**列表：不能为空（否则这个
            // 交付物永远关不掉、缺口提醒会一直响），且每个名字都必须真的
            // 存在于路由说明里（防止两列漂移——记账关掉了一个交付物，
            // 而提醒还在让 manager 去跑另一个流程）。
            assert!(!produced_by.is_empty(), "{label} 没有任何流程能产出它");
            assert!(
                produced_by.iter().any(|n| route.contains(n)),
                "{label} 的 produced_by {produced_by:?} 与路由说明 `{route}` 不一致"
            );
        }
    }

    /// `produced_by` 里的规划类流程必须与 [`is_planning_workflow`] 完全一致。
    ///
    /// 两处各写一份名单必然漂移：加了新的规划流程只改一处，另一处就静默
    /// 失效（记账那侧漂移的后果是交付物缺口永远关不掉或永远不响）。
    #[test]
    fn planning_workflows_agree_between_predicate_and_table() {
        let from_table = deliverables_produced_by("implementation_plan");
        assert!(
            from_table.contains(&"任务清单") && from_table.contains(&"计划"),
            "{from_table:?}"
        );
        for name in ["implementation_plan", "task_refine", "design_and_plan", "feature_design"] {
            assert!(is_planning_workflow(name), "{name} 应是规划流程");
            assert!(
                deliverables_produced_by(name).contains(&"任务清单"),
                "{name} 是规划流程，必须能关掉「任务清单」"
            );
        }
    }

    /// `learn` 关不掉「任务清单」——本次修复的核心断言。
    ///
    /// 旧实现把"产出过交付物"记成一个布尔，于是一句「安排学习计划 +
    /// 拆分任务」的诉求里，跑一个 `learn` 就把任务清单也一起算交付了。
    #[test]
    fn learn_does_not_close_the_task_list_deliverable() {
        let closes = deliverables_produced_by("learn");
        assert!(closes.contains(&"教程 / 学习材料"), "{closes:?}");
        assert!(!closes.contains(&"任务清单"), "learn 产不出任务清单: {closes:?}");
        assert!(!closes.contains(&"计划"), "learn 产不出任务清单/计划: {closes:?}");
        // learn_loop 同理（它多了知识点拆解，但拆的是知识点不是工程任务）。
        let closes = deliverables_produced_by("learn_loop");
        assert!(closes.contains(&"教程 / 学习材料"), "{closes:?}");
        assert!(!closes.contains(&"任务清单"), "{closes:?}");
        // 反向：调研类流程什么都关不掉。
        assert!(deliverables_produced_by("explore").is_empty());
    }

    #[test]
    fn reminder_routes_learning_requests_to_learn() {
        let named = named_deliverables("给我讲讲 jemalloc 的原理，想入门");
        let r = deliverable_reminder(&named);
        assert!(r.contains("learn"), "{r}");
        assert!(r.contains("docs/learn/"), "{r}");
    }

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

    // ─── 可选 speaker ────────────────────────────────────────────

    const OPT_WF: &str = r#"
name = "brain"
[[steps]]
id = "brainstorm"
speakers = ["architect", "programmer"]
prompt = "主题：{{topic}}\n\n背景：{{context}}"
[[steps.optional_speakers]]
role = "designer"
when_any = ["ui", "界面", "前端"]
"#;

    fn vars_with(topic: &str, context: &str) -> std::collections::HashMap<String, String> {
        std::collections::HashMap::from([
            ("topic".to_string(), topic.to_string()),
            ("context".to_string(), context.to_string()),
        ])
    }

    #[test]
    fn optional_speaker_skipped_when_topic_has_no_ui() {
        let wf: WorkflowDef = toml::from_str(OPT_WF).unwrap();
        wf.validate().unwrap();
        // 声明过的角色仍然全都列出来（validate 的角色存在性检查、UI
        // 列举要看得到可选角色）。
        assert_eq!(wf.speaker_roles(), vec!["architect", "programmer", "designer"]);

        // 实测那次的 topic：编排 jemalloc 源码学习路径，没有界面。
        let (active, skipped) = wf.steps[0].active_roles(
            "为当前仓库 jemalloc 编排「从浅到深学习」的学习路径与任务清单",
            "",
        );
        assert_eq!(active, vec!["architect", "programmer"], "designer 不该被派");
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].role, "designer");
        assert!(skipped[0].reason.contains("topic 未命中"), "{:?}", skipped[0]);
    }

    #[test]
    fn optional_speaker_included_when_topic_has_ui() {
        let wf: WorkflowDef = toml::from_str(OPT_WF).unwrap();
        for topic in ["重做设置页的 UI", "改前端布局", "give the CLI a UI"] {
            let (active, skipped) = wf.steps[0].active_roles(topic, "");
            assert_eq!(
                active,
                vec!["architect", "programmer", "designer"],
                "topic={topic} 应该拉上 designer"
            );
            assert!(skipped.is_empty(), "topic={topic}");
        }
    }

    #[test]
    fn ascii_keyword_matches_on_word_boundary_only() {
        // "ui" 裸子串会命中 build / guide / require / quick —— 一个纯
        // 构建话题就能把 designer 拉进来，可选角色等于没做。
        let wf: WorkflowDef = toml::from_str(OPT_WF).unwrap();
        for topic in [
            "fix the build pipeline",
            "写一份 style guide",
            "requirements 收敛",
            "quick fix",
            "GUI_TOOLKIT 常量改名", // 下划线也算词内字符
        ] {
            let (active, _) = wf.steps[0].active_roles(topic, "");
            assert_eq!(
                active,
                vec!["architect", "programmer"],
                "topic={topic} 不该误命中 ui"
            );
        }
        // 中文关键词按子串匹配（无词边界概念）。
        let (active, _) = wf.steps[0].active_roles("重做界面交互", "");
        assert!(active.contains(&"designer".to_string()));
        // 大小写不敏感。
        let (active, _) = wf.steps[0].active_roles("redesign the UI", "");
        assert!(active.contains(&"designer".to_string()));
    }

    #[test]
    fn optional_speaker_unless_any_vetoes() {
        let raw = r#"
name = "brain"
[[steps]]
id = "s"
speakers = ["architect"]
prompt = "x"
[[steps.optional_speakers]]
role = "designer"
when_any = ["ui"]
unless_any = ["cli only"]
"#;
        let wf: WorkflowDef = toml::from_str(raw).unwrap();
        wf.validate().unwrap();
        // 否决优先于 when_any：两个条件都命中时不派。
        let (active, skipped) = wf.steps[0].active_roles("build a ui, cli only", "");
        assert_eq!(active, vec!["architect"]);
        assert!(skipped[0].reason.contains("命中排除词"), "{:?}", skipped[0]);
    }

    #[test]
    fn optional_speaker_task_scope_sees_upstream_vars() {
        let raw = OPT_WF.replace(
            "when_any = [\"ui\", \"界面\", \"前端\"]",
            "when_any = [\"ui\", \"界面\", \"前端\"]\nmatch_scope = \"task\"",
        );
        let wf: WorkflowDef = toml::from_str(&raw).unwrap();
        wf.validate().unwrap();
        let vars = vars_with("做个功能", "上游结论：需要新增一个前端页面");
        let task = wf.render_task(&wf.steps[0], &vars);
        // topic 里没有界面词，但上游产出里有 —— task 范围能看到。
        let (active_topic, _) = wf.steps[0].active_roles("做个功能", "");
        assert_eq!(active_topic, vec!["architect", "programmer"]);
        let (active_task, _) = wf.steps[0].active_roles("做个功能", &task);
        assert!(active_task.contains(&"designer".to_string()));
    }

    #[test]
    fn optional_speaker_validation_rejects_silent_misconfig() {
        let base = |extra: &str| {
            format!(
                r#"
name = "brain"
[[steps]]
id = "s"
{extra}
"#
            )
        };
        // 1. 全员可选 → 关键词全不命中时 step 零 speaker、零产出。
        let raw = base(
            "prompt = \"x\"\n[[steps.optional_speakers]]\nrole = \"designer\"\nwhen_any = [\"ui\"]",
        );
        let wf: WorkflowDef = toml::from_str(&raw).unwrap();
        assert!(wf
            .validate()
            .unwrap_err()
            .contains("至少保留一个无条件 speaker"));

        // 2. 已经是无条件 speaker → 条件永远不生效。
        let raw = base(
            "speakers = [\"designer\"]\nprompt = \"x\"\n\
             [[steps.optional_speakers]]\nrole = \"designer\"\nwhen_any = [\"ui\"]",
        );
        let wf: WorkflowDef = toml::from_str(&raw).unwrap();
        assert!(wf.validate().unwrap_err().contains("已经是无条件 speaker"));

        // 3. 条件全空 → 恒真，等于无条件 speaker。
        let raw = base(
            "speakers = [\"architect\"]\nprompt = \"x\"\n\
             [[steps.optional_speakers]]\nrole = \"designer\"",
        );
        let wf: WorkflowDef = toml::from_str(&raw).unwrap();
        assert!(wf.validate().unwrap_err().contains("条件恒真"));

        // 4. 空关键词 → 永不命中。
        let raw = base(
            "speakers = [\"architect\"]\nprompt = \"x\"\n\
             [[steps.optional_speakers]]\nrole = \"designer\"\nwhen_any = [\"  \"]",
        );
        let wf: WorkflowDef = toml::from_str(&raw).unwrap();
        assert!(wf.validate().unwrap_err().contains("不能为空串"));

        // 5. 同一角色重复声明。
        let raw = base(
            "speakers = [\"architect\"]\nprompt = \"x\"\n\
             [[steps.optional_speakers]]\nrole = \"designer\"\nwhen_any = [\"ui\"]\n\
             [[steps.optional_speakers]]\nrole = \"designer\"\nwhen_any = [\"ux\"]",
        );
        let wf: WorkflowDef = toml::from_str(&raw).unwrap();
        assert!(wf.validate().unwrap_err().contains("重复声明"));

        // 6. 与嵌套 workflow 互斥。
        let raw = base(
            "workflow = \"explore\"\n\
             [[steps.optional_speakers]]\nrole = \"designer\"\nwhen_any = [\"ui\"]",
        );
        let wf: WorkflowDef = toml::from_str(&raw).unwrap();
        assert!(wf.validate().unwrap_err().contains("与嵌套 workflow 互斥"));
    }

    #[test]
    fn steps_without_optional_speakers_are_unchanged() {
        // 回归：没写 optional_speakers 的 step 阵容逐字不变。
        let raw = r#"
name = "legacy"
[[steps]]
id = "a"
speakers = ["pm", "architect", "designer"]
prompt = "x"
"#;
        let wf: WorkflowDef = toml::from_str(raw).unwrap();
        wf.validate().unwrap();
        let (active, skipped) = wf.steps[0].active_roles("任何 topic", "任何 task");
        assert_eq!(active, vec!["pm", "architect", "designer"]);
        assert!(skipped.is_empty());
        assert_eq!(wf.steps[0].declared_roles(), active);
    }

    /// 同一 wave 里并发跑的 step，只要**有两个以上**，就必须全是只读
    /// （`read`/`search`/`code_graph`）——否则并发写文件有真实竞态。
    ///
    /// 这条扫全部内置 workflow，是并发重排的安全网：以后有人给某个并发
    /// step 加了 `write`/`bash`，这里会直接失败。
    ///
    /// 例外：只有一个 step 的 wave 不算并发；嵌套 workflow step 的工具由
    /// 子流程自己的 step 决定，这里不管。
    ///
    /// 豁免名单里的 step 是**经核实写不同路径**的：`init_project` 的
    /// 每个 `overlay_*` 步各写 `prompts/overlays/<role>.md`，路径互不重叠，
    /// 并发无竞态。加豁免必须先核实这一点。
    #[test]
    fn concurrent_steps_in_a_wave_are_read_only() {
        const READ_ONLY: &[&str] = &["read", "search", "code_graph"];
        /// (workflow 名, step id 前缀) —— 写各自独立路径，已核实无竞态。
        const EXEMPT: &[(&str, &str)] = &[("init_project", "overlay_")];
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../config/workflows");
        let entries = std::fs::read_dir(&dir).expect("config/workflows");
        let mut checked = 0usize;
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("toml") {
                continue;
            }
            let raw = std::fs::read_to_string(&path).unwrap();
            let Ok(wf) = toml::from_str::<WorkflowDef>(&raw) else { continue };
            if !wf.uses_dependency_dag() {
                continue;
            }
            let Ok(waves) = compute_waves(&wf.steps) else { continue };
            for wave in &waves {
                // 只看真正会并发的（同 wave 有 ≥2 个非嵌套 step）。
                let concurrent: Vec<&WorkflowStepDef> = wave
                    .iter()
                    .map(|&i| &wf.steps[i])
                    .filter(|s| s.workflow.is_none())
                    .collect();
                if concurrent.len() < 2 {
                    continue;
                }
                for st in concurrent {
                    if EXEMPT
                        .iter()
                        .any(|(w, p)| *w == wf.name && st.id.starts_with(p))
                    {
                        continue;
                    }
                    checked += 1;
                    assert!(
                        !st.tools.is_empty(),
                        "{}: step '{}' 与同 wave 的其他步并发，必须显式声明只读工具集\
                         （空 = 角色全集，可能含 write/bash）",
                        wf.name,
                        st.id
                    );
                    for t in &st.tools {
                        assert!(
                            READ_ONLY.contains(&t.as_str()),
                            "{}: step '{}' 并发却声明了非只读工具 '{t}'——并发写文件有竞态",
                            wf.name,
                            st.id
                        );
                    }
                }
            }
        }
        assert!(checked > 0, "应至少扫到一组并发 step，实际 0 组（扫描逻辑可能失效）");
    }

    /// 仓库里有**两份** workflow 副本：`config/workflows/`（权威）与
    /// `.latte/workflows.d/`（项目级，会遮蔽前者）。改了权威版忘了同步，
    /// 运行时加载的仍是旧版——实测踩过两次：第一次是 `optional_speakers`
    /// 白改了，第二次是本次的并发重排"没生效"（实际跑的是旧文件）。
    ///
    /// 这条测试只钉住 step 拓扑一致，不逐字比对（两份的注释可以不同）。
    #[test]
    fn workflow_copies_do_not_drift() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        for name in ["design_brainstorm", "design_and_plan", "explore"] {
            let authoritative = root.join(format!("config/workflows/{name}.toml"));
            let project = root.join(format!(".latte/workflows.d/{name}.toml"));
            let (Ok(a), Ok(b)) = (
                std::fs::read_to_string(&authoritative),
                std::fs::read_to_string(&project),
            ) else {
                continue; // 某一份不存在就不比
            };
            let ids = |raw: &str| -> Vec<String> {
                toml::from_str::<WorkflowDef>(raw)
                    .map(|w| w.steps.iter().map(|s| s.id.clone()).collect())
                    .unwrap_or_default()
            };
            assert_eq!(
                ids(&a),
                ids(&b),
                "workflow '{name}' 的两份副本 step 拓扑不一致：\
                 config/workflows/ 是权威版，改完必须同步到 .latte/workflows.d/\
                 （否则运行时加载的是后者，改动静默失效）"
            );
        }
    }

    /// 重排后的 `design_brainstorm` 必须：走 DAG、脑暴三步同 wave、
    /// 评审三步同 wave，且 designer/devops 对无界面无部署的任务被跳过。
    #[test]
    fn shipped_design_brainstorm_runs_fanout_concurrently() {
        let raw = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../config/workflows/design_brainstorm.toml"),
        )
        .expect("内置 design_brainstorm.toml");
        let wf: WorkflowDef = toml::from_str(&raw).unwrap();
        wf.validate().unwrap();
        wf.validate_dag().unwrap();
        assert!(wf.uses_dependency_dag(), "必须走 DAG 引擎才能并发");

        let waves = compute_waves(&wf.steps).expect("waves");
        let name_of = |i: usize| wf.steps[i].id.clone();
        let wave_names: Vec<Vec<String>> = waves
            .iter()
            .map(|w| w.iter().map(|&i| name_of(i)).collect())
            .collect();

        // 脑暴三步必须落在同一个 wave（否则还是串行）。
        let brainstorm_wave = wave_names
            .iter()
            .find(|w| w.iter().any(|n| n == "brainstorm_arch"))
            .expect("找不到脑暴 wave");
        for n in ["brainstorm_arch", "brainstorm_prog", "brainstorm_design"] {
            assert!(
                brainstorm_wave.iter().any(|x| x == n),
                "{n} 应与其他脑暴步同 wave，实际分组: {wave_names:?}"
            );
        }
        // 评审三步同理。
        let eval_wave = wave_names
            .iter()
            .find(|w| w.iter().any(|n| n == "eval_review"))
            .expect("找不到评审 wave");
        for n in ["eval_review", "eval_security", "eval_devops"] {
            assert!(
                eval_wave.iter().any(|x| x == n),
                "{n} 应与其他评审步同 wave，实际分组: {wave_names:?}"
            );
        }
        // 评审步必须是只读（并发写文件有竞态）。
        for id in ["eval_review", "eval_security", "eval_devops"] {
            let st = wf.steps.iter().find(|s| s.id == id).unwrap();
            assert!(!st.tools.is_empty(), "{id} 必须显式声明只读工具集");
            for t in &st.tools {
                assert!(
                    matches!(t.as_str(), "read" | "search" | "code_graph"),
                    "{id} 声明了非只读工具 '{t}'，并发会有写竞态"
                );
            }
        }
        // 条件角色：无界面无部署的任务应跳过 designer 与 devops。
        let topic = "为当前仓库 jemalloc 编排「从浅到深学习」的学习路径与任务清单";
        for id in ["brainstorm_design", "eval_devops"] {
            let st = wf.steps.iter().find(|s| s.id == id).unwrap();
            assert!(!st.is_enabled(topic, "").0, "{id} 对该 topic 应被跳过");
        }
        // 反向：涉及界面 + 部署的任务两者都在场。
        let ui_topic = "重做控制台的前端界面，并接入 CI 自动部署";
        for id in ["brainstorm_design", "eval_devops"] {
            let st = wf.steps.iter().find(|s| s.id == id).unwrap();
            assert!(st.is_enabled(ui_topic, "").0, "{id} 对该 topic 应执行");
        }
    }

    /// `optional_speakers`（step 内按需增删 speaker）本身仍要有覆盖——
    /// 内置的 design_brainstorm 现在改用 **step 级**条件了，所以这条改用
    /// 一份内联配置来验，不再依赖那个文件的具体形态。
    #[test]
    fn optional_speakers_still_gate_within_a_step() {
        let wf: WorkflowDef = toml::from_str(
            r#"
name = "opt_in_step"
[[steps]]
id = "fan"
speakers = ["architect", "programmer"]
task = "x"
output_key = "ideas"
[[steps.optional_speakers]]
role = "designer"
when_any = ["ui", "界面"]
"#,
        )
        .unwrap();
        wf.validate().unwrap();
        let topic = "为 jemalloc 编排学习路径";
        let (active, skipped) = wf.steps[0].active_roles(topic, "");
        assert_eq!(active, vec!["architect", "programmer"]);
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].role, "designer");
        let (active, _) = wf.steps[0].active_roles("重做界面", "");
        assert!(active.contains(&"designer".to_string()));
    }


    const COND_WF: &str = r#"
name = "cond"
[[steps]]
id = "always"
role = "pm"
task = "无条件"
output_key = "a"
[[steps]]
id = "only_ui"
role = "designer"
task = "只在有界面时跑"
output_key = "b"
when_any = ["ui", "界面"]
"#;

    #[test]
    fn step_level_condition_gates_the_whole_step() {
        let wf: WorkflowDef = toml::from_str(COND_WF).unwrap();
        wf.validate().unwrap();
        // 无条件的步永远跑。
        assert_eq!(wf.steps[0].is_enabled("任何 topic", ""), (true, None));
        // 有条件的步：命中才跑。
        let (on, why) = wf.steps[1].is_enabled("重做设置页的界面", "");
        assert!(on && why.is_none());
        let (off, why) = wf.steps[1].is_enabled("为 jemalloc 编排学习路径", "");
        assert!(!off);
        assert!(why.unwrap().contains("未命中"));
    }

    #[test]
    fn step_condition_unless_any_vetoes() {
        let raw = COND_WF.replace(
            "when_any = [\"ui\", \"界面\"]",
            "when_any = [\"ui\"]\nunless_any = [\"cli only\"]",
        );
        let wf: WorkflowDef = toml::from_str(&raw).unwrap();
        wf.validate().unwrap();
        // 否决优先。
        let (on, why) = wf.steps[1].is_enabled("build a ui, cli only", "");
        assert!(!on);
        assert!(why.unwrap().contains("命中排除词"));
    }

    #[test]
    fn step_condition_validation_rejects_silent_misconfig() {
        // 空关键词永不命中。
        let raw = COND_WF.replace("when_any = [\"ui\", \"界面\"]", "when_any = [\"  \"]");
        let wf: WorkflowDef = toml::from_str(&raw).unwrap();
        assert!(wf.validate().unwrap_err().contains("不能为空串"));

        // 条件 step 没有 output_key → 跳过与执行对下游没有可观察差别。
        let raw = COND_WF.replace("output_key = \"b\"\n", "");
        let wf: WorkflowDef = toml::from_str(&raw).unwrap();
        assert!(wf
            .validate()
            .unwrap_err()
            .contains("必须声明 output_key"));
    }

    #[test]
    fn steps_without_conditions_are_unchanged() {
        // 回归：既有 workflow 一个字都没改，行为必须不变。
        let raw = r#"
name = "legacy"
[[steps]]
id = "a"
role = "pm"
task = "x"
"#;
        let wf: WorkflowDef = toml::from_str(raw).unwrap();
        wf.validate().unwrap();
        assert_eq!(wf.steps[0].is_enabled("", ""), (true, None));
    }

    // ─── 多 speaker 产出聚合 ──────────────────────────────────────

    #[test]
    fn single_speaker_output_is_passed_through_verbatim() {
        // 绝大多数 step 是单 speaker。加任何包装都会改变所有既有
        // workflow 的下游输入，所以必须逐字返回。
        let parts = vec![("architect".to_string(), "方案 A\n第二行".to_string())];
        assert_eq!(aggregate_step_output(&parts), "方案 A\n第二行");
        // 空（speaker 全被 optional 过滤掉是不可能的，但别 panic）。
        assert_eq!(aggregate_step_output(&[]), "");
    }

    #[test]
    fn multi_speaker_output_keeps_every_speaker() {
        // 修的 bug：原来 `last_output = response` 逐个覆盖，`output_key`
        // 只绑最后一个 speaker 的产出。实测（jemalloc 会话
        // wf-design_brainstorm-1788084798861063）：`evaluate` 步的
        // reviewer / security / devops 三份产出 4556 / 12346 / 5131
        // 字符，而 `evaluation` 只拿到 devops 的 5131 字符——security
        // 那 12346 字里的 3 项 P0 安全阻断从未到达下游。
        let parts = vec![
            ("reviewer".to_string(), "可维护性意见".to_string()),
            ("security".to_string(), "P0 阻断三项".to_string()),
            ("devops".to_string(), "运维成本意见".to_string()),
        ];
        let out = aggregate_step_output(&parts);
        for (role, body) in &parts {
            assert!(out.contains(body), "{role} 的产出丢了：{out}");
            assert!(out.contains(&format!("## [{role}]")), "{role} 缺少归属标记");
        }
        // 顺序 = 声明顺序，便于下游与 transcript 对照。
        let (i, j, k) = (
            out.find("可维护性意见").unwrap(),
            out.find("P0 阻断三项").unwrap(),
            out.find("运维成本意见").unwrap(),
        );
        assert!(i < j && j < k, "应按声明顺序拼接");
    }

    #[test]
    fn nested_workflow_step_validation() {        let ok = r#"
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

    /// `check_required_tools`：查步骤**做了什么**，而不是它说了什么。
    ///
    /// 动机是 learn_loop quiz 的实测事故：它没调 `ask`，却拿上一轮 prompt 里
    /// 出现过的旧答案编了一段「判分结果：答错 / 你选的是：split」，
    /// `output_contract` 的子串检查完全满足——学员连题都没看到，账本却被写成
    /// "答错两次、不再重教"。只有核对真实工具调用记录才拦得住这种。
    #[test]
    fn required_tools_checks_actual_calls_not_claims() {
        let need = vec!["ask".to_string()];
        // 真的调过 → 放行。摘要格式是 runner 的 "{name} {args} → {result}"。
        let called = "read {\"path\":\"a\"} → ok\nask {\"question\":\"…\"} → 我的选择：X";
        assert!(check_required_tools(&need, called).is_ok());

        // 一次工具都没调（runner 给的是占位串，不含工具名）→ 拦下。
        let none = "[本 step 工具调用数: 0]";
        let err = check_required_tools(&need, none).unwrap_err();
        assert!(err.contains("ask"), "批注要点名缺哪个工具: {err}");
        assert!(err.contains("无"), "没调任何工具时要说明: {err}");

        // 调了别的工具但没调 ask → 拦下，并回显实际调了什么。
        let other = "read {\"path\":\"plan.json\"} → ok\nwrite {\"path\":\"ledger.json\"} → ok";
        let err = check_required_tools(&need, other).unwrap_err();
        assert!(err.contains("ask") && err.contains("read"), "{err}");

        // 空要求恒放行（绝大多数 step 不设这个契约）。
        assert!(check_required_tools(&[], none).is_ok());

        // 带 namespace 的全名按短名比较（两侧都归一化）。
        assert!(check_required_tools(&need, "tutor.ask {} → x").is_ok());
        assert!(check_required_tools(&[
            "ns.ask".to_string()
        ], "ask {} → x").is_ok());
    }

    /// `check_required_tools_any`：OR 语义——"必须取证"有多条合法路径。
    ///
    /// 动机是 learn_loop plan 步的零取证事故（全部工具调用就是 2 次
    /// `write`，5 道题里 3 道的正确答案是幻觉）。取证走 read / code_graph /
    /// search 都算，用 AND 的 `require_tools` 会逼它三个都调一遍。
    #[test]
    fn require_tools_any_is_or_semantics() {
        let any = vec!["read".to_string(), "code_graph".to_string(), "search".to_string()];

        // 命中任意一个即通过。
        assert!(check_required_tools_any(&any, "code_graph {} → ok").is_ok());
        assert!(check_required_tools_any(&any, "search {} → ok").is_ok());

        // 一个都没命中 → 报错，且把实际调用了什么写进批注（好让模型知道
        // 自己干了什么、缺什么）。
        let err = check_required_tools_any(&any, "write {} → ok").unwrap_err();
        assert!(err.contains("至少调用"), "批注要说清是 OR 语义：{err}");
        assert!(err.contains("write"), "批注要回显实际调用了什么：{err}");

        // 一次工具都没调：占位行 `[本 step 工具调用数: 0]` 不能被当成工具名。
        let err = check_required_tools_any(&any, "[本 step 工具调用数: 0]").unwrap_err();
        assert!(err.contains("无"), "零调用应显示\"无\"而不是伪工具名：{err}");

        // 空声明 = 不校验。
        assert!(check_required_tools_any(&[], "[本 step 工具调用数: 0]").is_ok());

        // namespace 归一化（两侧都按短名比）。
        assert!(check_required_tools_any(&any, "tutor.read {} → ok").is_ok());
    }

    /// `check_tool_call_limits`：`require_tools` 查"有没有调过"，这条查
    /// "调了几次"。learn_loop quiz 的语义是「恰好弹一次窗」——弹两次就是
    /// 同一知识点连考两遍、账本按最后一次覆盖，丢掉一条作答记录。
    #[test]
    fn tool_call_limits_counts_calls() {
        let mut limits = std::collections::BTreeMap::new();
        limits.insert("ask".to_string(), 1usize);

        // 恰好一次 → 通过；零次也通过（"有没有调过"由 require_tools 管，
        // 两个字段职责不重叠）。
        assert!(check_tool_call_limits(&limits, "ask {} → A").is_ok());
        assert!(check_tool_call_limits(&limits, "read {} → x").is_ok());

        // 两次 → 超限。
        let err = check_tool_call_limits(&limits, "ask {} → A\nask {} → B").unwrap_err();
        assert!(err.contains("最多 1 次"), "批注要说清上限：{err}");
        assert!(err.contains("2 次"), "批注要说清实际次数：{err}");

        // 上限 0 = 本步禁止调用该工具（比从 tools 白名单摘掉更精确：
        // 白名单是"拿不到"，上限 0 是"拿得到但不许用"，批注能说清原因）。
        let mut forbid = std::collections::BTreeMap::new();
        forbid.insert("write".to_string(), 0usize);
        assert!(check_tool_call_limits(&forbid, "read {} → x").is_ok());
        let err = check_tool_call_limits(&forbid, "write {} → ok").unwrap_err();
        assert!(err.contains("禁止调用"), "上限 0 的批注措辞应是禁止：{err}");

        // namespace 归一化 + 空声明恒通过。
        assert!(check_tool_call_limits(&limits, "tutor.ask {} → A").is_ok());
        let dup = "tutor.ask {} → A\nask {} → B";
        assert!(check_tool_call_limits(&limits, dup).is_err(), "短名归一后应算 2 次");
        assert!(check_tool_call_limits(&Default::default(), dup).is_ok());
    }

    /// `check_tool_usage`：三条工具行为按"批注指导性"排序——完全没调 >
    /// 没取证 > 调多了。顺序错了模型会先去修次要问题。
    #[test]
    fn tool_usage_checks_run_in_priority_order() {
        let step: WorkflowStepDef = toml::from_str(
            "id = \"quiz\"\nrole = \"tutor\"\ntask = \"t\"\n\
             require_tools = [\"ask\"]\nrequire_tools_any = [\"read\"]\n\
             tool_call_limits = { ask = 1 }",
        )
        .expect("valid step TOML");

        // 三条全违反时，先报"没调 ask"（最根本）。
        let err = check_tool_usage(&step, "[本 step 工具调用数: 0]").unwrap_err();
        assert!(err.contains("要求实际调用工具"), "应先报缺必需工具：{err}");

        // 补上 ask 后，改报"没取证"。
        let err = check_tool_usage(&step, "ask {} → A").unwrap_err();
        assert!(err.contains("至少调用"), "应接着报缺取证：{err}");

        // 再补上 read，但 ask 弹了两次 → 报次数超限。
        let err = check_tool_usage(&step, "read {} → x\nask {} → A\nask {} → B").unwrap_err();
        assert!(err.contains("最多 1 次"), "应最后报次数超限：{err}");

        // 全满足。
        assert!(check_tool_usage(&step, "read {} → x\nask {} → A").is_ok());
    }

    /// `output_contract.max_chars` / `require_any` / `last_line_prefix_any`。
    #[test]
    fn output_contract_shape_checks() {
        // max_chars：learn_loop plan 步的产出被下游当路径片段拼接
        // （`.latte/learn/{{plan_out}}/plan.json`），多写一句话就拼出
        // 一条不存在的路径。
        let c = OutputContract { max_chars: Some(10), ..Default::default() };
        assert!(check_output_contract(&c, "rust-owner").is_ok());
        let err = check_output_contract(&c, "好的，slug 是 rust-ownership").unwrap_err();
        assert!(err.contains("产出过长"), "{err}");

        // require_any：OR 语义。quiz 末行只能是两个状态标记之一，
        // `require`（AND）表达不了"二选一"。
        let c = OutputContract {
            require_any: vec!["STATUS_ALL_DONE".to_string(), "STATUS_CONTINUE".to_string()],
            ..Default::default()
        };
        assert!(check_output_contract(&c, "…\nSTATUS_CONTINUE").is_ok());
        let err = check_output_contract(&c, "讲完了").unwrap_err();
        assert!(err.contains("至少一个"), "{err}");

        // last_line_prefix_any：下游按**末行**取值，子串检查拦不住
        // "标记写在正文中间、末行是句总结"（表现为讲了 k1 却考 k2）。
        let c = OutputContract {
            last_line_prefix_any: vec!["TEACH_DONE".to_string()],
            ..Default::default()
        };
        assert!(check_output_contract(&c, "讲解…\nTEACH_DONE k1").is_ok());
        // 末尾空行/缩进不影响（取最后一个非空行并 trim）。
        assert!(check_output_contract(&c, "讲解…\n  TEACH_DONE k1  \n\n").is_ok());
        // 标记在中间、末行是总结 → 必须拦下。这正是子串检查放过的那种。
        let err = check_output_contract(&c, "TEACH_DONE k1\n以上就是本节内容。").unwrap_err();
        assert!(err.contains("最后一行"), "{err}");
        assert!(err.contains("以上就是本节内容。"), "批注要回显实际末行：{err}");

        // 空契约恒通过。
        let c = OutputContract::default();
        assert!(check_output_contract(&c, "随便写").is_ok());
    }

    /// `validate`：新字段的形状错误必须在**配置期**拦下，而不是运行到
    /// 一半才静默失效。空工具名会被 `short_tool_name` 归一成空串、永远
    /// 匹配不上任何调用，于是 `require_tools_any` 恒失败、
    /// `tool_call_limits` 恒通过——两种都是"写了却不生效"。
    #[test]
    fn validate_rejects_malformed_behavior_fields() {
        let base = |extra: &str| {
            format!(
                "name = \"w\"\n[[steps]]\nid = \"s\"\nrole = \"tutor\"\ntask = \"t\"\n{extra}"
            )
        };

        let wf: WorkflowDef = toml::from_str(&base("require_tools_any = [\"\"]")).unwrap();
        let err = wf.validate().unwrap_err();
        assert!(err.contains("require_tools_any"), "{err}");

        let wf: WorkflowDef =
            toml::from_str(&base("tool_call_limits = { \"\" = 1 }")).unwrap();
        let err = wf.validate().unwrap_err();
        assert!(err.contains("tool_call_limits"), "{err}");

        // min > max：没有任何产出能同时满足，而先查 min 会让报错指向
        // "产出过短"，排查方向完全错。
        let wf: WorkflowDef = toml::from_str(&base(
            "[steps.output_contract]\nmin_chars = 100\nmax_chars = 10",
        ))
        .unwrap();
        let err = wf.validate().unwrap_err();
        assert!(err.contains("min_chars") && err.contains("max_chars"), "{err}");

        // 合法配置照常通过。
        let wf: WorkflowDef = toml::from_str(&base(
            "require_tools_any = [\"read\"]\ntool_call_limits = { ask = 1 }\n\
             [steps.output_contract]\nmin_chars = 10\nmax_chars = 100",
        ))
        .unwrap();
        wf.validate().expect("合法配置应通过");
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
    /// learn_loop：交互式学习循环——plan（取证+拆解+出题+写账本）→ teach
    /// （只讲解）→ quiz（只出题判分，未学完 loop_back_to teach）→ report。
    ///
    /// 这个测试锁的是实测会话暴露的三个缺陷的修复：
    ///   1. **答案泄漏**：旧设计要求把正确答案标成 options 里的
    ///      `recommended`，而该字段在前端渲染成字面的「✓ 推荐」徽标
    ///      （`chat_impl.ts`）——等于把答案印在题面上。现在答案只存
    ///      plan.json 的 `answer` 字段，workflow 明文禁止传 recommended。
    ///   2. **先考后教**：旧设计把讲解与出题塞进同一个 step，实测模型把
    ///      讲解写进 <think> 块、可见正文为空就直接 ask 弹窗阻塞 28.9 分钟，
    ///      学员看到一道没有课的考题。拆成 teach / quiz 两步，讲解一定先
    ///      落到主 session。
    ///   3. **凭记忆出题**：旧设计 plan 步全部工具调用是 2 次 write（0 次读
    ///      源码），5 道题里 3 道的"正确答案"是幻觉。现在 plan 步必须先取证、
    ///      每个知识点带 evidence（file:line）。
    #[test]
    fn learn_loop_workflow_structure() {
        let raw = include_str!("../../config/workflows/learn_loop.toml");
        let wf: WorkflowDef = toml::from_str(raw).expect("valid TOML");
        wf.validate().expect("learn_loop should validate");

        // prompt 文本断言的归一化：去掉反引号与 markdown 强调符。
        //
        // 为什么需要它：这些断言的目的是"某条禁令还在"，而不是"某句中文
        // 一字不改"。旧版直接 contains 整句（如「禁止出现 `recommended`」），
        // 结果每次精简 prompt 排版都会红一片测试，却没多挡住任何缺陷。
        fn norm(s: &str) -> String {
            s.replace(['`', '*'], "")
        }
        // `keyword` 所在行是否带否定词 —— 断言"这条禁令还在"，措辞自由。
        fn banned(text: &str, keyword: &str) -> bool {
            let t = norm(text);
            t.lines().any(|l| {
                l.contains(keyword)
                    && ["禁止", "不许", "不要", "不得", "不加", "别"]
                        .iter()
                        .any(|n| l.contains(n))
            })
        }

        let ids: Vec<&str> = wf.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["plan", "teach", "quiz", "report"]);

        let plan = &wf.steps[0];
        let teach = &wf.steps[1];
        let quiz = &wf.steps[2];
        let report = &wf.steps[3];
        for s in [plan, teach, quiz, report] {
            assert_eq!(s.roles(), vec!["tutor"]);
        }

        // ── 缺陷 3：plan 必须先取证再出题 ──
        assert!(plan.task_text().contains("code_graph"), "plan 必须要求用 code_graph 取证");
        assert!(plan.task_text().contains("evidence"), "plan 必须要求记 evidence 出处");
        assert!(plan.task_text().contains("ledger.json"));

        // ── 缺陷 2：讲解与出题分属两步，循环跨步回跳 ──
        // 循环标记只挂在 quiz 上：teach 不负责循环控制，也不许出题。
        assert!(teach.loop_until.is_none(), "teach 不该是循环步");
        assert_eq!(quiz.loop_until.as_deref(), Some("STATUS_ALL_DONE"));
        assert_eq!(
            quiz.loop_back_to.as_deref(),
            Some("teach"),
            "quiz 未学完必须跳回 teach 讲下一个知识点"
        );
        // 引擎硬上限是 10（unwrap_or(3).min(10)），配更大是自欺。
        assert_eq!(quiz.max_iterations, Some(10));
        assert!(quiz.task_text().contains("STATUS_CONTINUE"));
        assert!(quiz.task_text().contains("STATUS_ALL_DONE"));
        // teach 必须禁止出题——否则就退化回"讲解写进 think 块 + 直接弹窗"。
        //
        // 这条从"明文禁止"改成了"拿不到工具"：旧版靠 prompt 里的祈使句
        // 「禁止调用 `ask`」，那是劝告，模型可以不听且违反了没人知道。
        // 现在 teach 的 `tools` 白名单里根本没有 ask（也没有 write），
        // 引擎在构建 runner 时就把它们过滤掉了（`effective_step_tools`）。
        assert!(
            !teach.tools.is_empty(),
            "teach 必须声明 tools 白名单，否则拿到角色全集（含 ask/write）"
        );
        assert!(
            !teach.tools.iter().any(|t| t == "ask"),
            "teach 的 tools 白名单里不能有 ask——禁出题要靠工具隔离，不是 prompt 劝告"
        );
        assert!(
            !teach.tools.iter().any(|t| t == "write"),
            "teach 的 tools 白名单里不能有 write——账本只由 quiz 步更新"
        );
        assert!(!teach.task_text().contains("STATUS_ALL_DONE"), "循环标记不该出现在 teach");

        // ── 缺陷 1：答案键不得走 recommended ──
        assert!(
            banned(plan.task_text(), "recommended"),
            "plan 必须禁止把答案标成 recommended"
        );
        assert!(
            banned(quiz.task_text(), "recommended"),
            "quiz 调 ask 时必须禁止补 recommended"
        );
        assert!(plan.task_text().contains("\"answer\""), "答案应放独立的 answer 字段");

        // ── 缺陷 4（首次实跑发现）：答案位置偏置 ──
        // 实测 plan 生成的 5 道题答案全在第 1 位、label 退化成 A/B/C/D，
        // 学员靠位置就能全对——等于换了个通道泄漏答案。
        // 位置打散已从 plan（靠模型自觉，实测 5 题里 3 题答案仍在第 1 位）
        // 移交给 ask 的 shuffle（程序洗牌）——plan 这边只需说明"位置不用操心"。
        assert!(
            norm(plan.task_text()).contains("洗牌") || plan.task_text().contains("shuffle"),
            "plan 应说明位置由 ask 洗牌解决，不再要求模型自己打散"
        );
        assert!(
            banned(plan.task_text(), "A/B/C/D"),
            "plan 必须禁止用纯字母序号当 label"
        );

        // ── 缺陷 5（首次实跑发现）：teach 讲 k1、quiz 考 k2 ──
        // 两步各自按 ledger 状态推断"当前知识点"，fail 状态下推断分岔：
        // teach 重讲 k1，quiz 跳到 k2 出题，学员被突袭且 k1 永不推进。
        // 修法是单一事实来源——teach 声明 id，quiz 沿用。
        assert!(
            teach.task_text().contains("TEACH_DONE <知识点id>"),
            "teach 末行必须声明它讲的是哪个知识点"
        );
        assert!(
            quiz.task_text().contains("{{teach_out}}"),
            "quiz 必须从 teach 的输出里取知识点 id，不能自己推断"
        );
        assert_eq!(
            teach.output_key.as_deref(),
            Some("teach_out"),
            "quiz 插值 {{teach_out}} 依赖这个 output_key"
        );

        // ── 缺陷 6（第二次实跑发现）：quiz 跳过出题直接判 weak ──
        // 实测：k1 状态是 fail（只答错过一次），quiz 脑补成"已经答错两次"，
        // 没调 ask 就把它标成 weak，学员连重考机会都没有。光靠 prompt 说
        // "必须先出题"拦不住，所以用 output_contract 在引擎层设门槛：
        // 没拿到用户答案就写不出这两个字样，契约不满足会带批注重试。
        for pat in ["判分结果", "你选的是"] {
            assert!(
                quiz.output_contract.require.iter().any(|r| r == pat),
                "quiz 的 output_contract 缺少必需子串 {pat}"
            );
        }
        // 但子串检查会被编造绕过（实测：模型拿上一轮的旧答案编了一段判分），
        // 所以还必须有 require_tools 在引擎层核对真实工具调用记录。
        assert!(
            quiz.require_tools.iter().any(|t| t == "ask"),
            "quiz 必须声明 require_tools=[\"ask\"]，否则模型可以编造判分结果"
        );
        assert!(
            teach.require_tools.is_empty(),
            "teach 不该要求调用工具（它的 tools 白名单里没有 ask/write）"
        );
        // 「恰好一次」的后半句：require_tools 只查"有没有调过"，查不出
        // "调了几次"。弹两次窗 = 同一知识点连考两遍、账本按最后一次覆盖，
        // 学员的作答记录被悄悄丢掉一条。旧版这条只写在 prompt 里
        //（「本步必须恰好调用一次 `ask`」），是劝告；现在由引擎计数。
        assert_eq!(
            quiz.tool_call_limits.get("ask"),
            Some(&1),
            "quiz 必须声明 tool_call_limits={{ask=1}}，否则\"恰好一次\"只是句劝告"
        );
        // 答完题必须真的落账本。旧版只查了 ask：答完不写 ledger.json 也能
        // 通过，于是同一个知识点被反复考，循环靠 max_iterations 撞满才结束。
        assert!(
            quiz.require_tools.iter().any(|t| t == "write"),
            "quiz 必须声明 require_tools 含 write，否则判完分可以不落账本"
        );
        // 末行契约：下游/引擎按末行取循环状态。用 require 子串检查不够——
        // 标记写在正文中间时子串检查照样通过，而循环控制拿不到依据。
        assert!(
            quiz.output_contract.last_line_prefix_any.iter().any(|p| p == "STATUS_ALL_DONE")
                && quiz
                    .output_contract
                    .last_line_prefix_any
                    .iter()
                    .any(|p| p == "STATUS_CONTINUE"),
            "quiz 的末行必须被契约钉成两个循环状态标记之一"
        );
        assert!(
            teach
                .output_contract
                .last_line_prefix_any
                .iter()
                .any(|p| p == "TEACH_DONE"),
            "teach 的末行必须被契约钉成 TEACH_DONE——quiz 靠它决定考哪个知识点"
        );
        // 堵「讲解全写进 <think> 块、可见正文为空」：那次 1282 字符的讲解
        // 学员一个字也没看到。正文长度是引擎唯一能机械核对的"有没有真讲"。
        assert!(
            teach.output_contract.min_chars.unwrap_or(0) >= 300,
            "teach 必须有 min_chars 下限，否则空正文也能通过"
        );
        // plan 取证：用 _any（OR）而不是 require_tools（AND）——取证走
        // read / code_graph / search 都算，AND 会逼它三个都调一遍。
        assert!(
            !plan.require_tools_any.is_empty()
                && plan
                    .require_tools_any
                    .iter()
                    .all(|t| ["read", "code_graph", "search"].contains(&t.as_str())),
            "plan 必须声明 require_tools_any 覆盖取证工具，否则可以零取证凭记忆出题"
        );
        assert!(
            plan.require_tools.iter().any(|t| t == "write"),
            "plan 必须真把 plan.json / ledger.json 落盘"
        );
        // plan 的产出被下游当路径片段拼接，必须只是 slug 本身。
        assert!(
            plan.output_contract.max_chars.is_some_and(|m| m <= 60),
            "plan 必须有 max_chars 上限，否则多写一句话就把下游路径拼坏"
        );
        assert!(
            plan.output_contract.forbid.iter().any(|f| f == " ")
                && plan.output_contract.forbid.iter().any(|f| f == "/"),
            "plan 的 slug 不能带空格或斜杠"
        );
        // report 只读：改账本会掩盖真实掌握情况。
        assert_eq!(report.tools, vec!["read"], "report 必须是只读步");
        // 缺陷 7（第四次实跑发现）：模型执行四状态机不可靠——第一次答错就
        // 直接写 weak，跳过重考档。账本改成只存 wrong/passed 两个事实，
        // 状态由读取方推导，模型不再做状态推理。
        // 缺陷 8（第五次实跑发现，根因在 ask 工具而不在 workflow）：
        // 重考弹的是同一道题，被 ask 的"同题复用旧答案"保护静默拦掉，
        // 学员的重考机会被工具层吞掉。quiz 必须显式要求 allow_repeat。
        assert!(
            quiz.task_text().contains("allow_repeat: true"),
            "quiz 必须要求传 allow_repeat=true，否则重考会被 ask 复用旧答案吞掉"
        );
        // 缺陷 9/10（收尾优化）：位置偏置交给程序洗牌；测验必须关掉超时兜底
        // （否则会凭空造出一条学员作答记录，成绩账本全是假的）。
        assert!(
            quiz.task_text().contains("shuffle: true"),
            "quiz 必须要求 ask 打乱选项顺序"
        );
        assert!(
            quiz.task_text().contains("auto_answer: false"),
            "quiz 必须关掉超时兜底"
        );
        assert!(
            norm(quiz.task_text()).contains("绝不按位置") && quiz.task_text().contains("label"),
            "洗牌后判分必须按 label 文本匹配"
        );
        assert!(
            norm(quiz.task_text()).contains("wrong 只能 +1"),
            "quiz 必须把状态推进降级成 wrong+1，不许它自己判状态"
        );
        assert!(
            plan.task_text().contains("\"wrong\":0") && plan.task_text().contains("\"passed\":false"),
            "初始账本必须是 wrong/passed 两个事实，不是状态名"
        );
        assert!(
            teach.task_text().contains("passed == false 且 wrong < 2"),
            "teach 选题必须按 wrong/passed 推导"
        );
        assert!(
            report.task_text().contains("wrong >= 2") && report.task_text().contains("passed == true"),
            "report 必须按 wrong/passed 推导掌握情况"
        );

        // report：结尾收报告，把 fail/weak/pending 都算未通过。
        assert!(report.task_text().contains("ledger.json"));
    }

    /// tutor 角色必须有取证工具，否则 learn_loop 里"基于源码出题"的要求
    /// 根本执行不了——实测会话就是这么编出 3 道幻觉题的（当时白名单
    /// 只有 read/write/ask）。
    #[test]
    fn tutor_role_has_evidence_tools() {
        let raw = include_str!("../../config/agents/tutor.toml");
        let cfg: toml::Value = toml::from_str(raw).expect("valid TOML");
        let tools = cfg["roles"]["tutor"]["tools"]
            .as_array()
            .expect("tutor.tools must be an array")
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>();
        for t in ["read", "code_graph", "search", "write", "ask"] {
            assert!(tools.contains(&t), "tutor 缺少工具 {t}：{tools:?}");
        }
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
            "step 'design' missing output_key=\"design\" — 下游 plan step 靠这个 key 注入设计正文"
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

    /// `AbortOnDrop` 必须真的取消 spawn 出去的任务：`JoinHandle` 自身
    /// drop 不取消，预算超支时孤儿 specialist 会继续烧 token。
    #[tokio::test]
    async fn abort_on_drop_cancels_spawned_task() {
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f = flag.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            f.store(true, Ordering::SeqCst);
        });
        {
            let _guard = crate::sub_cancel::AbortOnDrop(handle.abort_handle());
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        } // guard drop → abort
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert!(
            !flag.load(Ordering::SeqCst),
            "任务应在守卫 drop 时被取消，不该跑完"
        );
        assert!(handle.await.is_err(), "被 abort 的任务 join 应返回 JoinError");
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

    /// `loop_no_release_on` 与 `loop_abort_on` 同规则：没有 loop_until
    /// 是死配置，必须拒绝。
    #[test]
    fn loop_no_release_on_without_loop_until_rejected() {
        let wf = wf_from(
            r#"
name = "no_release_orphan"
[[steps]]
id = "a"
role = "pm"
task = "t"
loop_no_release_on = "VERDICT: REJECT"
"#,
        );
        let err = wf.validate().expect_err("孤立的 loop_no_release_on 必须报错");
        assert!(err.contains("loop_no_release_on"), "错误应点名字段: {err}");
        assert!(err.contains("loop_until"), "错误应说明依赖: {err}");
    }

    /// 不可放行标记若包含放行条件，永远先命中放行，成死代码——拒绝。
    #[test]
    fn loop_no_release_on_containing_loop_until_rejected() {
        let wf = wf_from(
            r#"
name = "no_release_shadowed"
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
loop_no_release_on = "VERDICT: REJECT"
"#,
        );
        let err = wf.validate().expect_err("被放行条件遮蔽的不可放行标记必须报错");
        assert!(err.contains("永远不会触发"), "错误应解释原因: {err}");
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
mod max_rounds_degrade_tests {
    use super::degrade_partial_on_break;

    /// 有实质 partial → 降级采纳，且必须带截断标注（下游不能把半成品
    /// 当终稿）。
    #[test]
    fn substantive_partial_is_adopted_with_truncation_notice() {
        let partial = "## 工期估算\n\n实现复杂度中等，预计 6 小时。风险：inline 热路径。";
        let (notice, adopted) =
            degrade_partial_on_break("programmer", "estimate", "达到工具轮次上限（100 轮）", partial)
                .expect("有实质内容应降级采纳");

        // 通知要点名 speaker / step / 轮次，运维才能定位是哪一步被截断。
        assert!(notice.contains("programmer"), "{notice}");
        assert!(notice.contains("estimate"), "{notice}");
        assert!(notice.contains("100"), "{notice}");

        // 采纳的正文保留原内容，并追加截断标注。
        assert!(adopted.contains("工期估算"), "{adopted}");
        assert!(adopted.contains("预计 6 小时"), "{adopted}");
        assert!(adopted.contains("未经作者收尾"), "缺截断标注: {adopted}");
        assert!(adopted.contains("100"), "{adopted}");
    }

    /// `<think>` 块要被剥掉——降级采纳的是正式回答，不是思维链。
    #[test]
    fn think_blocks_are_stripped_before_adoption() {
        let partial = "<think>先读 tcache.c 再决定</think>\n\n结论：可行，约 6 小时。";
        let (_, adopted) = degrade_partial_on_break("programmer", "estimate", "达到工具轮次上限（100 轮）", partial)
            .expect("剥掉 think 后仍有正文");
        assert!(!adopted.contains("先读 tcache.c"), "think 未剥净: {adopted}");
        assert!(adopted.contains("结论：可行"), "{adopted}");
    }

    /// partial 为空 / 只有 think / 只有空白 → 不降级，交回失败路径。
    /// 否则会把空串写进 vars 穿给下游，比失败更糟。
    #[test]
    fn empty_partial_is_not_adopted() {
        for partial in ["", "   \n\t ", "<think>只想了想，没写结论</think>"] {
            assert!(
                degrade_partial_on_break("programmer", "estimate", "达到工具轮次上限（100 轮）", partial).is_none(),
                "空 partial 不应降级采纳: {partial:?}"
            );
        }
    }
}

#[cfg(test)]
mod contract_tests {    use super::*;

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

    /// 验收：task_refine 是「草案 → 评审 → 终审门 → important 定向修补
    /// → plan 提交」的五步评审流。gate 只输出裁决，revise 负责把评审的
    /// important 修正项落回草案，submit 消费**修订后**的草案。
    ///
    /// 为什么 submit 不能再消费 `{{draft}}`（实测实录）：
    /// gate 放行门槛是「无 blocking」，important 不阻断；但只要 submit
    /// 提交的是第一版 draft、且 gate 被禁止重写草案，任何 important
    /// 修正项就都没有回写通道，reviewer 写着「必须修正后再提交」的问题
    /// 100% 带病入库。
    #[test]
    fn task_refine_is_reviewed_five_step_flow() {
        let raw = include_str!("../../config/workflows/task_refine.toml");
        let wf: WorkflowDef = toml::from_str(raw).expect("valid TOML");
        wf.validate().expect("task_refine should validate");

        let ids: Vec<&str> = wf.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["refine", "review", "gate", "revise", "submit"]);

        let refine = &wf.steps[0];
        assert_eq!(refine.output_key.as_deref(), Some("draft"));
        assert_eq!(refine.output_contract.min_chars, Some(200));
        assert!(
            refine.task_text().contains("禁止调用 plan"),
            "refine 步必须禁止调 plan（草案先过评审）"
        );
        assert!(
            refine.require_plan_tasks,
            "refine 步必须声明 require_plan_tasks（清单 JSON 由引擎机械校验）"
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

        let revise = &wf.steps[3];
        assert_eq!(revise.roles(), &["task_planner".to_string()]);
        assert_eq!(revise.output_key.as_deref(), Some("final_draft"));
        assert_eq!(
            revise.patch_from.as_deref(),
            Some("draft"),
            "revise 步必须是 patch 模式：模型只输出 ops，引擎机械打到 draft 上"
        );
        assert!(
            revise.task_text().contains("{{draft}}") && revise.task_text().contains("{{verdict}}"),
            "revise 步必须同时拿到原草案与评审结论才能定向修补"
        );
        assert!(
            revise.task_text().contains("只改被点名的地方"),
            "revise 是定向修补，不是重新拆分"
        );
        assert!(
            revise.task_text().contains("禁止重出完整清单"),
            "revise 必须明写禁止重出完整清单（patch 的意义就是省掉全文重出）"
        );
        assert!(
            !revise.tools.iter().any(|t| t == "plan"),
            "revise 步不放行 plan——修补产物仍要走 submit 提交"
        );

        let submit = &wf.steps[4];
        assert_eq!(
            submit.submit_plan_from.as_deref(),
            Some("final_draft"),
            "submit 步必须消费修订后的草案，否则 important 修正项被丢弃"
        );
        assert!(
            submit.roles().is_empty() && !submit.task_text().contains("{{draft}}"),
            "submit 是零模型调用的机械步（无 speaker、不消费第一版 draft）"
        );
        assert!(
            !submit.require_plan_submit,
            "引擎直提后 require_plan_submit 退役（提交动作由引擎完成，不存在「没提交」）"
        );

        // step 级工具过滤：refine/revise 步硬性摘掉 plan（引擎层 enforce，
        // 不靠 prompt 自觉），submit 步是机械步（无 speaker、无工具）。
        // code_graph 在列：拆分依据改用「文件+符号名」锚点后，符号
        // **存在性**是唯一硬要求，refine 需要它来自证。
        assert_eq!(refine.tools, vec!["read", "search", "code_graph"]);
        assert_eq!(revise.tools, vec!["read", "search", "code_graph"]);
        assert!(
            !refine.tools.iter().any(|t| t == "plan"),
            "refine 步绝不能放行 plan——草案必须先过 review/gate"
        );
        assert!(review.tools.is_empty());
        assert!(gate.tools.is_empty());
        assert!(submit.tools.is_empty());
    }

    /// P0 回归（实测会话 实锤）：paths 规则
    /// 必须按 plan 工具的**真实判据**写 —— 路径字符串层、不区分读写 ——
    /// 并且必须在 prompt 里明写「只读」豁免。
    ///
    /// 事故链：旧规则同时要求「paths 必须包含验收标准点名的所有文件」和
    /// 「共读同一文件时改成消费上游子任务的交付物」，而"消费上游交付物"
    /// 本身就要把上游 doc 写进自己的 paths，正好踩中前一条禁令 →
    /// 6 个链式子任务撞出 14 处重叠 → plan 整单拒绝 → 零任务入库。
    /// 而 `is_readonly_plan_task` 的豁免只写在 plan 工具的 input_schema
    /// description 里，refine/revise 又用 `tools =` 摘掉了 plan，写草案的
    /// 角色从来看不到这个出口。
    #[test]
    fn task_refine_paths_rules_match_plan_tool_semantics() {
        for path in [
            "../config/workflows/task_refine.toml",
            "../.latte/workflows.d/task_refine.toml",
        ] {
            let raw = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
            let wf: WorkflowDef = toml::from_str(&raw).unwrap();
            wf.validate().unwrap_or_else(|e| panic!("{path}: {e}"));
            let step = |id: &str| {
                wf.steps
                    .iter()
                    .find(|s| s.id == id)
                    .unwrap_or_else(|| panic!("{path}: 缺 step '{id}'"))
            };
            let refine = step("refine");
            let review = step("review");
            let revise = step("revise");
            let submit = step("submit");

            // ① 重叠判据必须写明是路径字符串层、不区分读写。
            for (id, text) in [
                ("refine", refine.task_text()),
                ("review", review.task_text()),
            ] {
                assert!(
                    text.contains("不区分读") && text.contains("字符串"),
                    "{path}: {id} 步必须写明重叠判据是路径字符串层、不区分读写"
                );
            }
            // ② 「只读」豁免必须在写草案/打补丁的步骤里明写（这些步骤看不到
            //    plan 工具的 schema）。submit 是零模型调用的机械步，
            //    没有 prompt，不在此列。
            for (id, text) in [
                ("refine", refine.task_text()),
                ("review", review.task_text()),
                ("revise", revise.task_text()),
            ] {
                assert!(
                    text.contains("只读"),
                    "{path}: {id} 步必须明写「只读」label 豁免"
                );
            }
            // ③ 旧的自相矛盾表述不得回归。
            assert!(
                !refine.task_text().contains("纯合成型子任务（只消费上游交付物）写它要产出的文档目录"),
                "{path}: refine 不得回退到旧的「隐式重叠」表述"
            );
            assert!(
                !revise
                    .task_text()
                    .contains("补进去若与别的\n    子任务重叠"),
                "{path}: revise 不得回退到「先补再说」的旧改法"
            );
            // ④ revise 补 paths 前必须先查该文件是否已被别的任务持有
            //    （事故里第 14 处重叠正是 revise 照 important 直接补出来的）。
            assert!(
                revise.task_text().contains("补之前先查"),
                "{path}: revise 必须要求补 paths 前先查重"
            );
            // ⑤ 提交由引擎机械完成（submit_plan_from），重叠/存在性校验在
            //    refine（require_plan_tasks）与 revise patch 后各跑一遍——
            //    「模型受限自愈 + require_plan_submit 兜底」已随模型转写步
            //    一起退役。
            assert!(
                refine.require_plan_tasks,
                "{path}: refine 必须声明 require_plan_tasks（清单机械校验前置到产出时）"
            );
            assert_eq!(
                revise.patch_from.as_deref(),
                Some("draft"),
                "{path}: revise 必须是 patch 模式（补丁后引擎重跑机械校验）"
            );
            assert_eq!(
                submit.submit_plan_from.as_deref(),
                Some("final_draft"),
                "{path}: submit 必须是引擎机械提交步（submit_plan_from）"
            );
        }
    }

    /// P0 回归（实测实锤）：gate 的通过门槛必须是「无 blocking」，
    /// 而不是「无 blocking/important」——后者让 reviewer 的行号偏差类
    /// important 反复触发 REVISE，撞满 max_iterations 后整条 workflow
    /// Failed，`submit` 步永远执行不到 → plan 工具从不被调用 →
    /// 「添加子任务」弹窗彻底消失（session ui-88486-…-2 实录）。
    #[test]
    fn task_refine_gate_blocks_only_on_blocking() {
        for path in [
            "../config/workflows/task_refine.toml",
            "../.latte/workflows.d/task_refine.toml",
        ] {
            let raw = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
            let wf: WorkflowDef = toml::from_str(&raw).unwrap();
            let gate = wf.steps.iter().find(|s| s.id == "gate").expect("gate step");
            let review = wf.steps.iter().find(|s| s.id == "review").expect("review step");
            let refine = wf.steps.iter().find(|s| s.id == "refine").expect("refine step");

            assert!(
                gate.prompt.contains("没有 blocking 问题"),
                "{path}: gate 的 ACCEPT 条件必须是「无 blocking」"
            );
            assert!(
                !gate.prompt.contains("无 blocking/important 问题"),
                "{path}: important 不得再作为阻断条件（这是弹窗消失的根因）"
            );
            assert!(
                gate.prompt.contains("行号偏差是 nit"),
                "{path}: gate 必须显式声明行号偏差不构成 REVISE 理由"
            );
            // 评审步要有严重度分级，且把行号归入 nit。
            assert!(
                review.prompt.contains("blocking") && review.prompt.contains("nit"),
                "{path}: review 步必须给出严重度定义"
            );
            assert!(
                review.prompt.contains("行号偏差"),
                "{path}: review 步必须把行号偏差明确归为 nit"
            );
            // 草案锚点改用符号名，不再用裸行号当验收依据。
            assert!(
                refine.prompt.contains("不要用裸行号"),
                "{path}: refine 步必须要求符号级锚点"
            );
            // 三值裁决与返工环仍在（放宽门槛不等于取消评审）。
            assert_eq!(gate.loop_until.as_deref(), Some("VERDICT: ACCEPT"));
            assert_eq!(gate.loop_back_to.as_deref(), Some("refine"));
            assert_eq!(gate.loop_abort_on.as_deref(), Some("VERDICT: REJECT"));
            assert!(
                wf.steps.iter().any(|s| s.id == "submit"),
                "{path}: submit 步必须存在（它才是调 plan 弹窗的那一步）"
            );
            // important 必须有回写通道：gate 不阻断 important，所以
            // 只要 submit 提交的是第一版 draft，important 就永远丢。
            let revise = wf
                .steps
                .iter()
                .find(|s| s.id == "revise")
                .unwrap_or_else(|| panic!("{path}: 必须有 revise 步来落实 important 修正项"));
            assert_eq!(revise.output_key.as_deref(), Some("final_draft"), "{path}");
            let submit = wf.steps.iter().find(|s| s.id == "submit").expect("submit");
            assert_eq!(
                submit.submit_plan_from.as_deref(),
                Some("final_draft"),
                "{path}: submit 必须消费修订后的草案（submit_plan_from = \"final_draft\"）"
            );
            // gate 不得再宣称「用户会在导入弹窗里看到 important」——
            // PlanProposed 事件只带 tasks，弹窗不展示评审结论（虚假免责
            // 会让 gate 心安理得地把带病草案放行）。
            assert!(
                !gate.prompt.contains("导入弹窗里看到"),
                "{path}: 导入弹窗并不展示评审结论，不能靠这个说法免责"
            );
        }
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
    /// 流水线，`loop_until`/`loop_back_to` 变回死配置（实测实锤：
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
    /// 实测实锤：评审如实 REJECT（3 条实证阻断）导致 50 分钟
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
    /// 实测实锤：gate 如实 REJECT（extent 状态数、LG_QUANTUM 等
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
                description: String::new(),
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
    /// 重放同一个必挂请求（实测里 tester 卡 400 的实锤路径）。
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
            None,
        )
        .await
        .expect("worker runner");
        assert!(
            runner.has_model_hot_reload(),
            "workflow step runner must hot-reload model chain on resume"
        );
    }

    // ─── 可选 speaker：引擎级（真跑 run_workflow，数真实模型请求）──
    //
    // 上面 `mod tests` 里的测试只覆盖 `active_roles()` 这个判定函数；
    // 这几条覆盖的是**两个引擎真的把它接上了**——串行与 DAG 是两处
    // 独立的扇出点，漏接一处就是「配置写了但不生效」的静默失效。

    /// 多角色测试配置（同 tier 同 mock 端点，靠 role_id 区分谁被派）。
    fn test_config_roles_at(base_url: &str, role_ids: &[&str]) -> Arc<AgentConfig> {
        let base = test_config_at(base_url);
        let mut roles = HashMap::new();
        for id in role_ids {
            roles.insert(
                id.to_string(),
                RoleTemplate {
                    id: (*id).into(),
                    name: (*id).into(),
                    category: "execution".into(),
                    model_tier: "premium".into(),
                    model_chain: vec![],
                    prompt_file: None,
                    temperature: None,
                    tools: vec![],
                    icon: String::new(),
                    skills: vec![],
                    code_paths: vec![],
                    description: String::new(),
                },
            );
        }
        Arc::new(AgentConfig {
            advisor: Default::default(),
            models: base.models.clone(),
            roles,
        })
    }

    /// 排干事件流：返回 (发过言的 role_id 顺序, Status 消息).
    fn drain_turns_and_status(
        rx: &mut broadcast::Receiver<ChatEvent>,
    ) -> (Vec<String>, Vec<String>) {
        let mut turns = Vec::new();
        let mut status = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            match ev {
                ChatEvent::WorkflowTurn { role_id, .. } => turns.push(role_id),
                ChatEvent::Status { message } => status.push(message),
                _ => {}
            }
        }
        (turns, status)
    }

    /// 串行引擎（无 depends_on）。
    fn optional_speaker_wf_serial() -> WorkflowDef {
        toml::from_str(
            r#"
name = "opt_serial"
[[steps]]
id = "brainstorm"
speakers = ["arch", "prog"]
task = "脑暴：{{topic}}"
output_key = "ideas"
[[steps.optional_speakers]]
role = "design"
when_any = ["ui", "界面"]
"#,
        )
        .expect("valid TOML")
    }

    /// DAG 引擎（有 depends_on → 走 waves 调度，另一处扇出点）。
    fn optional_speaker_wf_dag() -> WorkflowDef {
        toml::from_str(
            r#"
name = "opt_dag"
[[steps]]
id = "seed"
role = "arch"
task = "起个头：{{topic}}"
output_key = "seed"
[[steps]]
id = "brainstorm"
speakers = ["arch", "prog"]
depends_on = ["seed"]
task = "脑暴：{{topic}} / {{seed}}"
output_key = "ideas"
[[steps.optional_speakers]]
role = "design"
when_any = ["ui", "界面"]
"#,
        )
        .expect("valid TOML")
    }

    async fn mock_server_always_ok() -> wiremock::MockServer {
        let server = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "这是一份足够详实的产出，覆盖方案概述与取舍依据，没有占位内容。",
            )))
            .mount(&server)
            .await;
        server
    }

    /// 串行引擎：topic 无界面词 → designer 一次模型请求都不该发生。
    #[tokio::test]
    async fn serial_engine_skips_optional_speaker_when_topic_misses() {
        let server = mock_server_always_ok().await;
        let (ctx, mut rx) =
            test_ctx(test_config_roles_at(&server.uri(), &["arch", "prog", "design"]));
        run_workflow(
            &optional_speaker_wf_serial(),
            "为 jemalloc 编排从浅到深的学习路径与任务清单",
            &ctx,
        )
        .await
        .expect("workflow 应成功");

        let (turns, status) = drain_turns_and_status(&mut rx);
        assert_eq!(turns, vec!["arch", "prog"], "designer 不该发言");
        // 真实模型请求数是最硬的证据：没派就不会有第 3 个请求。
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "只应有 2 个 speaker 各一次请求"
        );
        assert!(
            status.iter().any(|m| m.contains("跳过可选角色 design")),
            "应发出跳过原因的 Status: {status:?}"
        );
    }

    /// 串行引擎：topic 有界面词 → designer 正常参与（第 3 个请求）。
    #[tokio::test]
    async fn serial_engine_dispatches_optional_speaker_when_topic_hits() {
        let server = mock_server_always_ok().await;
        let (ctx, mut rx) =
            test_ctx(test_config_roles_at(&server.uri(), &["arch", "prog", "design"]));
        run_workflow(&optional_speaker_wf_serial(), "重做设置页的界面", &ctx)
            .await
            .expect("workflow 应成功");

        let (turns, status) = drain_turns_and_status(&mut rx);
        assert_eq!(turns, vec!["arch", "prog", "design"], "designer 应在场");
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
        assert!(
            !status.iter().any(|m| m.contains("跳过可选角色")),
            "命中时不应发跳过 Status: {status:?}"
        );
    }

    /// DAG 引擎是**另一处**扇出点，必须单独覆盖——只接串行那处的话
    /// 带 depends_on 的 workflow（design_and_plan 就是）完全不生效。
    #[tokio::test]
    async fn dag_engine_skips_optional_speaker_when_topic_misses() {
        let server = mock_server_always_ok().await;
        let wf = optional_speaker_wf_dag();
        assert!(wf.uses_dependency_dag(), "本用例必须走 DAG 引擎");
        let (ctx, mut rx) =
            test_ctx(test_config_roles_at(&server.uri(), &["arch", "prog", "design"]));
        run_workflow(&wf, "为 jemalloc 编排学习路径", &ctx)
            .await
            .expect("workflow 应成功");

        let (turns, status) = drain_turns_and_status(&mut rx);
        // seed(arch) + brainstorm(arch, prog)，无 design。
        assert_eq!(turns, vec!["arch", "arch", "prog"], "designer 不该发言");
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            3,
            "seed 1 次 + brainstorm 2 个 speaker"
        );
        assert!(
            status.iter().any(|m| m.contains("跳过可选角色 design")),
            "DAG 引擎也要发跳过 Status: {status:?}"
        );
    }

    /// DAG 引擎命中时正常参与（第 4 个请求）。
    #[tokio::test]
    async fn dag_engine_dispatches_optional_speaker_when_topic_hits() {
        let server = mock_server_always_ok().await;
        let (ctx, mut rx) =
            test_ctx(test_config_roles_at(&server.uri(), &["arch", "prog", "design"]));
        run_workflow(&optional_speaker_wf_dag(), "把 UI 重构一遍", &ctx)
            .await
            .expect("workflow 应成功");

        let (turns, _) = drain_turns_and_status(&mut rx);
        assert_eq!(turns, vec!["arch", "arch", "prog", "design"]);
        assert_eq!(server.received_requests().await.unwrap().len(), 4);
    }

    /// 端到端：多 speaker step 的**每一份**产出都必须流到下游 step。
    ///
    /// 这是 `aggregate_step_output` 的真正目的。原来 `output_key` 只绑
    /// 最后一个 speaker 的产出，下游拿到的是残缺输入——实测 jemalloc
    /// 会话里 security 的 12346 字（含 3 项 P0 安全阻断）就是这么丢的。
    /// 产出契约必须作用在**单个 speaker 的产出**上，不能作用在聚合结果上。
    ///
    /// 我加聚合时的衍生风险：若契约校验聚合后的文本，多 speaker 步的契约
    /// 语义就变了——比如 `learn.toml` 的 `verify` 步要求产出含
    /// `KIND`/`PASS`/`FAIL`，聚合后只要**任一** speaker 提到就算过，
    /// 而原意是**每个** speaker 都要给出裁决。
    ///
    /// 实现上校验发生在 `response` 上、聚合在其后，所以语义没变。这条
    /// 测试把这个顺序钉住。
    #[tokio::test]
    async fn contract_is_checked_per_speaker_not_on_aggregate() {
        let server = wiremock::MockServer::start().await;
        // 第一个 speaker 的产出**不含** VERDICT（违反契约），第二个含。
        // 若契约作用在聚合结果上，第一个的违规会被第二个"救回来"，
        // 于是只有 2 次请求；作用在单个产出上则第一个要重试，共 3 次。
        let bad = "这是一段足够长的产出但缺少必需标记，用于触发契约重试与批注回填机制。";
        let good = "这是一段足够长的产出。VERDICT: PASS";
        // wiremock 是**先注册先匹配**：限次的违约 mock 必须注册在兜底
        // 之前，否则兜底会吃掉所有请求、违约永不发生（我第一版就这么错的，
        // 实测请求数 2 而非 3）。
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(bad)))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        // 兜底：合格产出。
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(good)))
            .mount(&server)
            .await;

        let wf: WorkflowDef = toml::from_str(
            r#"
name = "per_speaker_contract"
[[steps]]
id = "fan"
speakers = ["a", "b"]
task = "各自给裁决"
output_key = "verdicts"
max_retries = 1

[steps.output_contract]
require = ["VERDICT:"]
"#,
        )
        .unwrap();
        wf.validate().unwrap();

        let (ctx, _rx) = test_ctx(test_config_roles_at(&server.uri(), &["a", "b"]));
        let out = run_workflow(&wf, "测试", &ctx).await.expect("应成功");
        let n = server.received_requests().await.unwrap().len();
        assert_eq!(
            n, 3,
            "speaker a 的首次产出违约应触发重试（1 首发 + 1 重试 + 1 个 b），\
             实际 {n} 次——若为 2 说明契约被改成校验聚合结果了"
        );
        // 聚合结果里两个 speaker 都在。
        assert!(out.contains("## [a]") && out.contains("## [b]"), "{out}");
    }

    #[tokio::test]
    async fn multi_speaker_step_feeds_all_outputs_downstream() {
        let server = wiremock::MockServer::start().await;
        // 所有调用都回同一段话：足够长以通过默认契约，内容里带 speaker
        // 无关的固定串，便于数出现次数。
        let body = "这是一份足够详实的产出，覆盖方案概述与取舍依据，没有占位内容。MARKER";
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(body)))
            .mount(&server)
            .await;

        let wf: WorkflowDef = toml::from_str(
            r#"
name = "agg_demo"
[[steps]]
id = "fan"
speakers = ["a", "b", "c"]
task = "各自发表意见"
output_key = "opinions"

[[steps]]
id = "join"
role = "a"
task = "下面是全部意见，请汇总：\n{{opinions}}"
"#,
        )
        .unwrap();
        wf.validate().unwrap();

        let (ctx, _rx) = test_ctx(test_config_roles_at(&server.uri(), &["a", "b", "c"]));
        run_workflow(&wf, "测试主题", &ctx).await.expect("workflow 应成功");

        // 最后一次请求是 join 步，它的 prompt 里应含三份产出。
        let requests = server.received_requests().await.unwrap();
        let last = String::from_utf8_lossy(&requests.last().unwrap().body);
        assert_eq!(
            last.matches("MARKER").count(),
            3,
            "join 步应看到全部 3 份产出，实际 {} 份：\n{}",
            last.matches("MARKER").count(),
            &last[last.len().saturating_sub(600)..]
        );
        for role in ["a", "b", "c"] {
            assert!(
                last.contains(&format!("## [{role}]")),
                "join 步的输入应带 {role} 的归属标记"
            );
        }
    }

    fn count_workflow_turns(rx: &mut broadcast::Receiver<ChatEvent>) -> usize {        let mut n = 0;        while let Ok(ev) = rx.try_recv() {
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

    /// P0 回归（实测会话 实锤）：
    /// `require_plan_submit` 的 step 只产出文字、没成功提交 plan 时，
    /// 必须重试并最终判 step 失败 —— 不能像事故里那样让「plan 被工具
    /// 整单拒绝 → 改口输出一份请示报告」通过验收，把零任务入库的运行
    /// 报成 `status ok`。
    ///
    /// 同时验证重试 prompt 带上了「上次产出」（每次尝试都是全新
    /// subagent，工具报错只存在于上一次的产出里，不带过去模型不知道
    /// 该修哪个 path）。
    #[tokio::test]
    async fn serial_require_plan_submit_fails_without_plan_proposal() {
        let server = wiremock::MockServer::start().await;
        // 模型每次都只回文字（模拟「plan 被拒 → 改口写请示报告」）。
        let excuse = "plan 工具报 paths 范围重叠 14 处，我没有修改任务内容，请指示走 A / B / C 方案。";
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(excuse)))
            .mount(&server)
            .await;

        let raw = r#"
name = "plan_submit_demo"
[[steps]]
id = "submit"
role = "worker"
task = "把清单用 plan 工具提交"
max_retries = 1
require_plan_submit = true
"#;
        let wf: WorkflowDef = toml::from_str(raw).expect("valid TOML");
        assert!(wf.steps[0].require_plan_submit, "字段必须从 TOML 解析出来");

        let (ctx, mut rx) = test_ctx(test_config_at(&server.uri()));
        let err = run_workflow(&wf, "测试主题", &ctx)
            .await
            .expect_err("没提交 plan 必须判失败，不能报 ok");
        assert!(err.contains("submit"), "错误含 step id: {err}");
        assert!(err.contains("plan 提案"), "错误说清是 plan 没提交: {err}");

        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.len(),
            2,
            "首发 + max_retries=1 次重试: {}",
            requests.len()
        );
        let second = String::from_utf8_lossy(&requests[1].body);
        assert!(
            second.contains("没有成功提交 plan 提案"),
            "重试 prompt 带批注: {second}"
        );
        assert!(
            second.contains("paths 范围重叠 14 处"),
            "重试 prompt 必须回带上次产出里的工具报错: {second}"
        );
        assert_eq!(
            count_workflow_turns(&mut rx),
            0,
            "未提交成功的产出不发 WorkflowTurn"
        );
    }

    /// advisor 返回审查判 intervene（未写 remedy → 默认 patch）→ 同一
    /// runner **续作**修补：第二次请求必须带着首轮的完整上下文（含 v1
    /// 产出），而不是全新 subagent 重跑（实测实锤：探索型分派被 intervene
    /// 打回后整轮重跑，几十万 input tokens 作废）。
    #[tokio::test]
    async fn advisor_intervene_triggers_patch_continuation_with_feedback() {
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
        assert!(redo_req.contains("【上轮审查反馈】"), "续作 prompt 带反馈批注: {redo_req}");
        assert!(redo_req.contains("重写并紧扣任务"), "批注含 advisor hint: {redo_req}");
        // 续作的判据：同一 runner 的 context 保留了首轮产出——全新
        // subagent 重跑的请求里不会有它。
        assert!(
            redo_req.contains("v1 产出"),
            "续作请求必须带首轮上下文（含 v1 产出）: {redo_req}"
        );

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
                ChatEvent::Status { message } if message.contains("带审查意见续作修补")
            )),
            "应有续作修补 Status 事件: {events:?}"
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

    /// advisor 显式判 remedy: restart → 废弃本轮产出，**重建全新 runner**
    /// 重跑：重做请求带原始任务 + 反馈，但**不含**首轮上下文（与 patch
    /// 续作相反——续作请求里能查到首轮产出，见上上个测试）。
    #[tokio::test]
    async fn advisor_intervene_remedy_restart_rebuilds_fresh_runner() {
        let server = wiremock::MockServer::start().await;
        let v1 = "v1 产出：这份内容足够长，肯定超过五十个字符的短输出门禁阈值，不含工具回声。";
        let v2 = "v2 产出：已按 restart 重做，同样超过五十个字符的短输出门禁阈值，不含工具回声。";
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
                "verdict: intervene\nreason: 内容基于虚构，续作不可信\nhint: 推倒重来\nremedy: restart",
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
name = "restart_test"
description = "advisor intervene restart"

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
            .expect("restart 重做后应成功");
        assert_eq!(out, v2, "最终产出必须是重做版");

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 4, "worker×2 + advisor×2: {}", requests.len());
        let redo_req = String::from_utf8_lossy(&requests[2].body);
        assert!(redo_req.contains("【上轮审查反馈】"), "重做 prompt 带反馈批注: {redo_req}");
        assert!(
            !redo_req.contains("v1 产出"),
            "restart 必须是全新上下文，不得携带首轮产出: {redo_req}"
        );

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
            "restart 应发「重做」Status 事件: {events:?}"
        );
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

    /// P2 回归（实测实录）：gate 返工环耗尽时，advisor 语义
    /// 复核判 ok → 放行进入下游 `submit`，弹窗得以出现。
    ///
    /// 修复前：gate 两轮 REVISE 撞满 max_iterations=2 → 整条 workflow
    /// Failed → submit 永不执行 → plan 从不被调 → 「添加子任务」弹窗消失。
    /// 注意这不是「无条件放水」：必须 advisor 主动判 ok（见下一个用例）。
    #[tokio::test]
    async fn loop_exhausted_advisor_ok_reaches_downstream() {
        let server = wiremock::MockServer::start().await;
        let draft = "拆分草案：子任务一覆盖构建系统认知，子任务二覆盖目录结构认知，各自可独立验收。";
        // refine 产出草案（会被返工重跑，故不限次数）。
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("产出拆分草案"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(draft)))
            .mount(&server)
            .await;
        // gate 永远 REVISE（模拟 reviewer 反复挑行号偏差）。
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("放行判定"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "VERDICT: REVISE 行号引用仍有偏差，若干处 file:line 与实际位置差了一两行，其余内容无异议。",
            )))
            .mount(&server)
            .await;
        // advisor 语义复核：判 ok（剩余意见只是行号小瑕疵）。
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("最后一次语义复核"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "verdict: ok\nreason:\nhint:",
            )))
            .mount(&server)
            .await;
        // 下游 submit。
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("提交清单"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "已提交完整子任务清单：共两条子任务，均带验收标准与 paths 范围，等待用户在导入弹窗中确认。",
            )))
            .mount(&server)
            .await;

        let wf: WorkflowDef = toml::from_str(
            r#"
name = "loop_rescue_demo"
[[steps]]
id = "refine"
role = "worker"
task = "产出拆分草案 {{topic}}"
output_key = "draft"
[[steps]]
id = "gate"
role = "worker"
task = "放行判定 {{draft}}"
output_key = "approved"
loop_until = "VERDICT: ACCEPT"
loop_back_to = "refine"
max_iterations = 2
[[steps]]
id = "submit"
role = "worker"
task = "提交清单 {{draft}}"
"#,
        )
        .unwrap();
        let mut ctx = test_ctx(test_config_at(&server.uri())).0;
        ctx.advisor_gate = Some(crate::advisor_monitor::GateConfig::default());
        let out = run_workflow(&wf, "主题", &ctx)
            .await
            .expect("advisor 判 ok 应放行到 submit");
        assert!(
            out.contains("已提交完整子任务清单"),
            "必须真的执行到 submit 步并拿到它的产出: {out}"
        );

        let bodies: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).to_string())
            .collect();
        // 语义复核只在**耗尽那一刻**问一次，不是每轮都问（否则等于取消返工）。
        // 注意不能按 step 任务文本计数：advisor 的返回审查请求也内嵌任务原文。
        assert_eq!(
            bodies.iter().filter(|b| b.contains("最后一次语义复核")).count(),
            1,
            "advisor 语义复核应恰好在耗尽点被问一次"
        );
        assert!(
            bodies.iter().any(|b| b.contains("提交清单")),
            "submit 步必须真的被派发"
        );
        // 返工环仍然有界（没有放开成无限重试）。
        assert!(
            bodies.len() < 20,
            "请求总数应受 max_iterations 约束，实际 {}",
            bodies.len()
        );
    }

    /// 对照用例：advisor 判 intervene（草案真有实质缺陷）→ 维持原失败。
    /// 证明这条兜底不是「无条件放水」。
    #[tokio::test]
    async fn loop_exhausted_advisor_intervene_still_fails() {
        let server = wiremock::MockServer::start().await;
        let draft = "拆分草案：子任务一覆盖构建系统认知，子任务二覆盖目录结构认知，各自可独立验收。";
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("产出拆分草案"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(draft)))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("放行判定"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "VERDICT: REVISE 子任务引用了不存在的符号",
            )))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("最后一次语义复核"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "verdict: intervene\nreason: 引用的符号不存在，属实质缺陷\nhint: 重新核验符号",
            )))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("提交清单"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body("不该到这里")))
            .mount(&server)
            .await;

        let wf: WorkflowDef = toml::from_str(
            r#"
name = "loop_rescue_deny"
[[steps]]
id = "refine"
role = "worker"
task = "产出拆分草案 {{topic}}"
output_key = "draft"
[[steps]]
id = "gate"
role = "worker"
task = "放行判定 {{draft}}"
output_key = "approved"
loop_until = "VERDICT: ACCEPT"
loop_back_to = "refine"
max_iterations = 2
[[steps]]
id = "submit"
role = "worker"
task = "提交清单 {{draft}}"
"#,
        )
        .unwrap();
        let mut ctx = test_ctx(test_config_at(&server.uri())).0;
        ctx.advisor_gate = Some(crate::advisor_monitor::GateConfig::default());
        let err = run_workflow(&wf, "主题", &ctx)
            .await
            .expect_err("advisor 判 intervene 应维持失败");
        assert!(err.contains("循环条件"), "应报循环条件未满足: {err}");

        let bodies: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).to_string())
            .collect();
        assert_eq!(
            bodies.iter().filter(|b| b.contains("提交清单")).count(),
            0,
            "实质缺陷时绝不能放行到 submit"
        );
    }

    /// 回归（实测实锤：2026-09 jemalloc implementation_plan 会话）：
    /// gate 三轮均输出 `VERDICT: REJECT`（带阻断证据的硬拒绝），耗尽点
    /// advisor 复核的是 loop_back_to 目标的产出（breakdown 计划——本身
    /// 没病），误判「实质合格」放行，run 以 ok 收尾、summary 却是
    /// REJECT 原文——失败被吞成成功。
    ///
    /// `loop_no_release_on` 后：含不可放行标记的产出在耗尽点**不问
    /// advisor** 直接判死。REVISE 类小瑕疵（不含标记）仍走复核放行
    /// （见 loop_exhausted_advisor_ok_reaches_downstream）。
    #[tokio::test]
    async fn loop_exhausted_reject_marker_is_not_released() {
        let server = wiremock::MockServer::start().await;
        let draft = "拆分草案：子任务一覆盖构建系统认知，子任务二覆盖目录结构认知，各自可独立验收。";
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("产出拆分草案"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(draft)))
            .mount(&server)
            .await;
        // gate 永远 REJECT（阻断问题，带证据）。
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("放行判定"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "VERDICT: REJECT 任务清单缺失 id/depends_on 字段，依赖拓扑全丢（B1/B2 阻断）。",
            )))
            .mount(&server)
            .await;
        // advisor 若被问到会判 ok——但含 REJECT 标记时根本不该问它。
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("最后一次语义复核"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "verdict: ok\nreason:\nhint:",
            )))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("提交清单"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body("不该到这里")))
            .mount(&server)
            .await;

        let wf: WorkflowDef = toml::from_str(
            r#"
name = "loop_no_release_demo"
[[steps]]
id = "refine"
role = "worker"
task = "产出拆分草案 {{topic}}"
output_key = "draft"
[[steps]]
id = "gate"
role = "worker"
task = "放行判定 {{draft}}"
output_key = "approved"
loop_until = "VERDICT: PASS"
loop_back_to = "refine"
loop_no_release_on = "VERDICT: REJECT"
max_iterations = 2
[[steps]]
id = "submit"
role = "worker"
task = "提交清单 {{draft}}"
"#,
        )
        .unwrap();
        let mut ctx = test_ctx(test_config_at(&server.uri())).0;
        ctx.advisor_gate = Some(crate::advisor_monitor::GateConfig::default());
        let err = run_workflow(&wf, "主题", &ctx)
            .await
            .expect_err("REJECT 硬拒绝在耗尽点必须判死，不许复核放行");
        assert!(err.contains("循环条件"), "应报循环条件未满足: {err}");
        assert!(
            err.contains("不可放行标记"),
            "失败消息应说明是不可放行标记触发的: {err}"
        );

        let bodies: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).to_string())
            .collect();
        assert_eq!(
            bodies.iter().filter(|b| b.contains("最后一次语义复核")).count(),
            0,
            "含 REJECT 标记时绝不能问 advisor（问了就可能被放行）"
        );
        assert_eq!(
            bodies.iter().filter(|b| b.contains("提交清单")).count(),
            0,
            "REJECT 硬拒绝绝不能放行到 submit"
        );
    }

    /// DAG 侧兜底（与串行同款）：返工环耗尽 → advisor 判 ok → 免除本
    /// step 的返工要求，wave 正常推进到下游。
    ///
    /// 实际暴露面：`design_and_plan` 既走 DAG（有 depends_on）又配了返工环
    /// （`loop_until = "VERDICT: PASS"`, `max_iterations = 3`），耗尽即判死
    /// 会把已完成 wave 的成果一起丢掉。
    #[tokio::test]
    async fn dag_loop_exhausted_advisor_ok_reaches_downstream() {
        let server = wiremock::MockServer::start().await;
        let design = "设计稿：分三层落地，接口与数据结构均已给出，边界条件与回滚路径写明，可直接进入实现。";
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("产出设计稿"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(design)))
            .mount(&server)
            .await;
        // gate 永远 REVISE（>50 字节以避开 advisor D5 短输出门禁）。
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("放行判定"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "VERDICT: REVISE 仍有若干引用位置与实际行号差了一两行，其它部分没有异议。",
            )))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("最后一次语义复核"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "verdict: ok\nreason:\nhint:",
            )))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("编写实现"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "实现已完成：按设计稿落地全部三层，补齐单元测试与边界用例，回归通过。",
            )))
            .mount(&server)
            .await;

        let wf: WorkflowDef = toml::from_str(
            r#"
name = "dag_loop_rescue"
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
[[steps]]
id = "impl"
role = "worker"
task = "编写实现 {{design}}"
depends_on = ["gate"]
"#,
        )
        .unwrap();
        let mut ctx = test_ctx(test_config_at(&server.uri())).0;
        ctx.advisor_gate = Some(crate::advisor_monitor::GateConfig::default());
        let out = run_workflow(&wf, "主题", &ctx)
            .await
            .expect("advisor 判 ok 应放行到下游 wave");
        assert!(out.contains("实现已完成"), "必须真的执行到下游 impl 步: {out}");

        let bodies: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).to_string())
            .collect();
        assert_eq!(
            bodies.iter().filter(|b| b.contains("最后一次语义复核")).count(),
            1,
            "语义复核只在耗尽点问一次"
        );
        assert!(
            bodies.iter().any(|b| b.contains("编写实现")),
            "下游 wave 必须被派发"
        );
    }

    /// DAG 侧对照：advisor 判 intervene → 维持原失败，下游不得执行。
    #[tokio::test]
    async fn dag_loop_exhausted_advisor_intervene_still_fails() {
        let server = wiremock::MockServer::start().await;
        let design = "设计稿：分三层落地，接口与数据结构均已给出，边界条件与回滚路径写明，可直接进入实现。";
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("产出设计稿"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(design)))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("放行判定"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "VERDICT: REVISE 设计稿遗漏了并发路径下的一致性处理，属实质缺陷，必须返工。",
            )))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("最后一次语义复核"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                "verdict: intervene\nreason: 关键路径遗漏，属实质缺陷\nhint: 补并发一致性设计",
            )))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("编写实现"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body("不该到这里")))
            .mount(&server)
            .await;

        let wf: WorkflowDef = toml::from_str(
            r#"
name = "dag_loop_rescue_deny"
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
[[steps]]
id = "impl"
role = "worker"
task = "编写实现 {{design}}"
depends_on = ["gate"]
"#,
        )
        .unwrap();
        let mut ctx = test_ctx(test_config_at(&server.uri())).0;
        ctx.advisor_gate = Some(crate::advisor_monitor::GateConfig::default());
        let err = run_workflow(&wf, "主题", &ctx)
            .await
            .expect_err("advisor 判 intervene 应维持失败");
        assert!(err.contains("循环条件"), "应报循环条件未满足: {err}");

        let bodies: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).to_string())
            .collect();
        assert_eq!(
            bodies.iter().filter(|b| b.contains("编写实现")).count(),
            0,
            "实质缺陷时下游 wave 绝不能被派发"
        );
    }

    /// C3 兜底（串行）：契约重试耗尽 → advisor 语义审查判 ok →
    /// 带批注放行。实测实锤：gate 的合法 REJECT 没写契约要求的
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
                description: String::new(),
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
    /// REJECT 的场景里 manager 要能直接看到 REJECT 理由（实测现场：
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
    /// 注意本例用的是 REVISE。真正的实测事故词表是 REJECT，
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

    /// 事故回放（真实词表）：design_and_plan / implementation_plan
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
        let ckpt = CheckpointLog::new(cwd, "wf-ans-1", "design_and_plan", "主题", 0, None);
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
        let _ckpt = CheckpointLog::new(cwd, "wf-ans-2", "design_and_plan", "主题", 0, None);
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
        let _ckpt = CheckpointLog::new(cwd, "wf-carry-1", "design_and_plan", "主题", 0, None);
        AnswerLog::new(cwd, "wf-carry-1", None, None, Default::default())
            .record("tutor", "时间预算？", "10h+");
        let first = load_checkpoint(cwd, "wf-carry-1").expect("第一份 checkpoint");

        // 模拟 run_workflow_inner 的 resume 抄写块（Step + Answer）。
        let _ckpt2 = CheckpointLog::new(cwd, "wf-carry-2", "design_and_plan", "主题", 0, None);
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
        let _ckpt = CheckpointLog::new(cwd, "wf-orphan-1", "design_and_plan", "主题", 0, None);
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
    /// 永久卡死（实测现场：`design_and_plan` → `req_review` →
    /// `requirements_review` 的 `decide` 弹出选择题，答了也没用）。
    #[test]
    fn nested_answer_lands_in_both_own_and_root_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let _root = CheckpointLog::new(cwd, "wf-root-1", "design_and_plan", "主题", 0, None);
        let _child = CheckpointLog::new(cwd, "wf-child-1", "requirements_review", "子主题", 0, None);

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
        let _ckpt = CheckpointLog::new(cwd, "wf-solo-1", "design_and_plan", "主题", 0, None);
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
        let _root = CheckpointLog::new(cwd, "wf-root-2", "design_and_plan", "主题", 0, None);
        let _old_child = CheckpointLog::new(cwd, "wf-child-2a", "requirements_review", "子", 0, None);

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
        let _ckpt = CheckpointLog::new(cwd, "wf-ans-4", "wf", "t", 0, None);
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

        let out = run_workflow(&wf, "学某个 C 项目", &ctx).await;
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
    /// 方案本体。回归实测现场：design_and_plan 的 {{design}}
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
    /// 回归实测现场：评审只收到被逐层压缩的 proposal，事实基线
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
            description: String::new(),
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

    // ─── plan tasks 结构化 patch（require_plan_tasks / patch_from / submit_plan_from） ───

    /// 带 src/ 与 docs/ 的临时目录：paths 机械校验需要真实存在的路径。
    fn patch_test_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wf-patch-test-{}", tag));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        dir
    }

    const PATCH_DRAFT: &str = r#"草案说明文字。

```json
{"tasks": [
  {"title": "T1 实现", "description": "做 T1", "priority": 2, "paths": ["src"]},
  {"title": "T2 文档", "description": "写文档", "labels": ["只读"], "paths": ["docs"]}
]}
```
"#;

    #[test]
    fn find_tasks_fence_requires_exactly_one_tasks_block() {
        // 恰好一个 → 拿到 tasks 数组
        let (_span, tasks) = find_tasks_fence(PATCH_DRAFT).expect("单 fence 应解析");
        assert_eq!(tasks.as_array().unwrap().len(), 2);
        // 没有 → 机械错误
        let err = find_tasks_fence("纯文字，没有清单").unwrap_err();
        assert!(err.contains("tasks"), "{err}");
        // 两个 → 拒绝（哪份是 canonical 无从判断）
        let two = format!("{PATCH_DRAFT}\n{PATCH_DRAFT}");
        let err = find_tasks_fence(&two).unwrap_err();
        assert!(err.contains("2 个"), "{err}");
    }

    #[test]
    fn apply_task_patch_happy_path() {
        let dir = patch_test_dir("happy");
        let resp = "把 T1 的优先级提到 1。\n\n```json\n{\"ops\": [{\"task\": \"T1\", \"field\": \"priority\", \"set\": 1}]}\n```\n";
        let out = apply_task_patch(PATCH_DRAFT, resp, &dir).expect("patch 应成功");
        // 补丁后的 fence 里是结构化的新值
        let (_span, tasks) = find_tasks_fence(&out).expect("产出仍含唯一清单");
        assert_eq!(tasks[0]["priority"], serde_json::json!(1));
        assert_eq!(tasks[1]["title"], "T2 文档");
        // 修订说明（ops fence 之外的文字）附在末尾
        assert!(out.contains("本次修订"), "{out}");
        assert!(out.contains("把 T1 的优先级提到 1"), "{out}");
        // 草案正文保留
        assert!(out.contains("草案说明文字"), "{out}");
    }

    #[test]
    fn apply_task_patch_empty_ops_passes_draft_through() {
        let dir = patch_test_dir("empty");
        let resp = "无 important 项，草案未改动。\n\n```json\n{\"ops\": []}\n```\n";
        let out = apply_task_patch(PATCH_DRAFT, resp, &dir).expect("空 ops 应放行");
        assert_eq!(out, PATCH_DRAFT);
    }

    #[test]
    fn apply_task_patch_unknown_task_lists_titles() {
        let dir = patch_test_dir("unknown");
        let resp = "```json\n{\"ops\": [{\"task\": \"T9\", \"field\": \"priority\", \"set\": 1}]}\n```\n";
        let err = apply_task_patch(PATCH_DRAFT, resp, &dir).unwrap_err();
        assert!(err.contains("T9") && err.contains("T1 实现"), "{err}");
    }

    #[test]
    fn apply_task_patch_ambiguous_task_rejected() {
        let dir = patch_test_dir("ambiguous");
        // 「T」同时匹配 T1/T2
        let resp = "```json\n{\"ops\": [{\"task\": \"T\", \"field\": \"priority\", \"set\": 1}]}\n```\n";
        let err = apply_task_patch(PATCH_DRAFT, resp, &dir).unwrap_err();
        assert!(err.contains("匹配到 2 个任务"), "{err}");
    }

    #[test]
    fn apply_task_patch_unknown_field_rejected() {
        let dir = patch_test_dir("field");
        let resp = "```json\n{\"ops\": [{\"task\": \"T1\", \"field\": \"owner\", \"set\": \"x\"}]}\n```\n";
        let err = apply_task_patch(PATCH_DRAFT, resp, &dir).unwrap_err();
        assert!(err.contains("owner") && err.contains("paths"), "{err}");
    }

    #[test]
    fn apply_task_patch_type_mismatch_rejected() {
        let dir = patch_test_dir("type");
        let resp = "```json\n{\"ops\": [{\"task\": \"T1\", \"field\": \"title\", \"set\": 5}]}\n```\n";
        let err = apply_task_patch(PATCH_DRAFT, resp, &dir).unwrap_err();
        assert!(err.contains("类型"), "{err}");
    }

    #[test]
    fn apply_task_patch_noop_guard() {
        let dir = patch_test_dir("noop");
        // priority 已是 2，再 set 2 = 无实际修改
        let resp = "```json\n{\"ops\": [{\"task\": \"T1\", \"field\": \"priority\", \"set\": 2}]}\n```\n";
        let err = apply_task_patch(PATCH_DRAFT, resp, &dir).unwrap_err();
        assert!(err.contains("无实际修改"), "{err}");
    }

    /// 补丁后重跑 plan 机械校验：把 docs 改到 T1 持有的 src 上制造
    /// 字符串层重叠（T2 无只读豁免时）——必须被拦下（实录：revise
    /// 直接补 paths 曾把清单从「可提交」推成「整单被拒」）。
    #[test]
    fn parse_ops_tolerates_closing_fence_on_content_line() {
        // 实测 glm-5.3：闭合 ``` 写在 JSON 最后一行的行尾，不换行。
        let resp = "修订说明。\n\n```json\n{\"ops\": [{\"task\": \"T1\", \"field\": \"priority\", \"set\": 1}]} ```\n";
        let (ops, _span) = parse_task_patch_ops(resp).expect("行尾闭合应被容忍");
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].task, "T1");
    }

    #[test]
    fn apply_task_patch_revalidates_overlap() {
        let dir = patch_test_dir("overlap");
        let draft = r#"```json
{"tasks": [
  {"title": "T1 实现", "paths": ["src"]},
  {"title": "T2 工具", "paths": ["docs"]}
]}
```"#;
        let resp = "```json\n{\"ops\": [{\"task\": \"T2\", \"field\": \"paths\", \"set\": [\"docs\", \"src\"]}]}\n```\n";
        let err = apply_task_patch(draft, resp, &dir).unwrap_err();
        assert!(err.contains("机械校验"), "{err}");
    }

    #[test]
    fn check_plan_tasks_output_runs_plan_validation() {
        let dir = patch_test_dir("contract");
        assert!(check_plan_tasks_output(PATCH_DRAFT, &dir).is_ok());
        // 幻觉路径 → 机械校验打回
        let bad = "```json\n{\"tasks\": [{\"title\": \"T\", \"paths\": [\"nonexistent_dir_xyz\"]}]}\n```";
        let err = check_plan_tasks_output(bad, &dir).unwrap_err();
        assert!(err.contains("机械校验"), "{err}");
    }

    /// `require_symbols_resolvable` 的引擎侧接线：产出里查不到的符号要被
    /// 打回，真实符号要放行。判据细节在
    /// `latte_rs_agent_tools::utils::symbol_check` 自己的测试里。
    ///
    /// 回归 2026-09 jemalloc 会话：`require_tools_any`（调没调工具）与
    /// `require_plan_tasks`（路径存在性）都管不到「文件对、函数名假」，
    /// 而那 74 个幻觉符号里 39 个出自模型压根没打开过的文件。
    #[test]
    fn check_symbols_output_rejects_unresolvable_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/arena.c"),
            "void\narena_slab_reg_alloc(bin_t *bin) {\n\t(void)bin;\n}\n",
        )
        .unwrap();

        // 真实符号 → 放行。
        assert!(
            check_symbols_output("主线索是 `arena_slab_reg_alloc`。", root).is_ok(),
            "真实符号不该被拦"
        );
        // 旧版本符号 → 打回，且批注里点名并给出相近真实符号。
        let err = check_symbols_output("主线索是 `arena_bin_malloc_hard`。", root)
            .expect_err("查不到的符号必须被拦");
        assert!(err.contains("arena_bin_malloc_hard"), "{err}");
        assert!(err.contains("code_graph"), "批注要指路到正确工具：{err}");
    }

    /// 该字段能从 step TOML 解析（默认 false = 不校验）。
    #[test]
    fn require_symbols_resolvable_parses_from_step_toml() {
        let on: WorkflowStepDef = toml::from_str(
            "id = \"s\"\nrole = \"worker\"\ntask = \"t\"\nrequire_symbols_resolvable = true",
        )
        .expect("parse");
        assert!(on.require_symbols_resolvable);
        let off: WorkflowStepDef =
            toml::from_str("id = \"s\"\nrole = \"worker\"\ntask = \"t\"").expect("parse");
        assert!(!off.require_symbols_resolvable, "默认不校验");
    }

    #[test]
    fn validate_patch_fields_shape() {
        // patch_from 必须指向更早 step 的 output_key
        let bad_later = r#"
name = "w"
[[steps]]
id = "a"
role = "worker"
task = "t"
patch_from = "draft"

[[steps]]
id = "b"
role = "worker"
task = "t"
output_key = "draft"
"#;
        let wf: WorkflowDef = toml::from_str(bad_later).unwrap();
        let err = wf.validate().unwrap_err();
        assert!(err.contains("patch_from") && err.contains("更早"), "{err}");

        // submit_plan_from 与 speakers 互斥
        let bad_speakers = r#"
name = "w"
[[steps]]
id = "a"
role = "worker"
task = "t"
output_key = "draft"

[[steps]]
id = "b"
speakers = ["worker"]
submit_plan_from = "draft"
"#;
        let wf: WorkflowDef = toml::from_str(bad_speakers).unwrap();
        let err = wf.validate().unwrap_err();
        assert!(err.contains("互斥"), "{err}");

        // DAG 下禁用
        let dag = r#"
name = "w"
[[steps]]
id = "a"
role = "worker"
task = "t"
output_key = "draft"

[[steps]]
id = "b"
role = "worker"
task = "t"
depends_on = ["a"]
patch_from = "draft"
"#;
        let wf: WorkflowDef = toml::from_str(dag).unwrap();
        let err = wf.validate().unwrap_err();
        assert!(err.contains("串行"), "{err}");

        // 合法形态通过
        let ok = r#"
name = "w"
[[steps]]
id = "a"
role = "worker"
task = "t"
output_key = "draft"
require_plan_tasks = true

[[steps]]
id = "b"
role = "worker"
task = "t"
output_key = "final_draft"
patch_from = "draft"

[[steps]]
id = "c"
submit_plan_from = "final_draft"
"#;
        let wf: WorkflowDef = toml::from_str(ok).unwrap();
        wf.validate().expect("合法 patch 流程应通过校验");
    }

    /// 端到端（wiremock 假模型）：refine 出结构化草案 → revise 只输出
    /// ops → submit 零模型调用机械提交。断言：模型请求只有 2 次
    /// （submit 不调模型）、PlanProposed 里是打过补丁的清单。
    #[tokio::test]
    async fn serial_patch_revise_and_mechanical_submit() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        let draft_resp = "草案说明。\n\n```json\n{\"tasks\": [{\"title\": \"T1 实现\", \"description\": \"做 T1\", \"priority\": 2, \"paths\": [\"src\"]}, {\"title\": \"T2 文档\", \"description\": \"写文档\", \"labels\": [\"只读\"], \"paths\": [\"docs\"]}]}\n```\n";
        let ops_resp = "把 T1 的优先级提到 1。\n\n```json\n{\"ops\": [{\"task\": \"T1\", \"field\": \"priority\", \"set\": 1}]}\n```\n";
        // wiremock 后注册先匹配：revise 的匹配器（含「请修补」）先注册
        // 也行——两个匹配器互斥（请求体不会同时含「请拆分」和「请修补」）。
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_string_contains("请修补"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(ops_resp)))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(draft_resp)))
            .mount(&server)
            .await;

        let wf: WorkflowDef = toml::from_str(
            r#"
name = "patch_flow"
[[steps]]
id = "refine"
role = "reviewer"
task = "请拆分"
output_key = "draft"
require_plan_tasks = true

[[steps]]
id = "revise"
role = "reviewer"
task = "请修补 {{draft}}"
output_key = "final_draft"
patch_from = "draft"

[[steps]]
id = "submit"
submit_plan_from = "final_draft"
"#,
        )
        .unwrap();
        wf.validate().unwrap();

        let (ctx, mut rx) = test_ctx(test_config_at(&server.uri()));
        // paths 机械校验需要真实目录
        let dir = patch_test_dir("e2e");
        let ctx = WorkflowRunContext { cwd: dir, ..ctx };
        let out = run_workflow(&wf, "测试主题", &ctx).await.expect("workflow 应成功");
        assert!(out.contains("已提交 2 个任务候选"), "{out}");

        // submit 是机械步：模型请求只有 refine + revise 两次。
        let n = server.received_requests().await.unwrap().len();
        assert_eq!(n, 2, "submit 不应产生模型调用，实际 {n} 次");

        // PlanProposed 里是打过补丁的清单（T1 priority 已被 patch 成 1）。
        let mut proposed = None;
        while let Ok(ev) = rx.try_recv() {
            if let ChatEvent::PlanProposed { tasks, .. } = ev {
                proposed = Some(tasks);
            }
        }
        let tasks = proposed.expect("应发出 PlanProposed 事件");
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].title, "T1 实现");
        assert_eq!(tasks[0].priority, Some(1), "patch 应已生效");
        assert_eq!(tasks[1].labels, vec!["只读".to_string()]);
    }
}
