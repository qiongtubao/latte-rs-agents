//! Default role prompt templates, embedded at build time from the
//! workspace's `prompts/<id>.md` files via `include_str!`.
//!
//! Downstream consumers (e.g. `latte-code-editor`) read these
//! directly through `latte_agent_core::prompts::PM` etc. — no file
//! I/O, no copying of markdown into the consumer's tree. Source
//! of truth stays in `latte-rs-agents/prompts/`; updating a file
//! there flows downstream on the next `cargo build`.
//!
//! The `include_str!` paths are relative to this file
//! (`src/prompts.rs` → `../../prompts/<id>.md`).
//!
//! The content is rendered with handlebars at runtime by
//! `RoleTemplate::resolve()` / `AgentRunner::run_turn`; this module
//! just makes the raw markdown addressable as Rust constants.

/// Product Manager
pub const PM: &str = include_str!("../../prompts/pm.md");
/// System Architect
pub const ARCHITECT: &str = include_str!("../../prompts/architect.md");
/// Software Engineer
pub const PROGRAMMER: &str = include_str!("../../prompts/programmer.md");
/// QA Engineer
pub const TESTER: &str = include_str!("../../prompts/tester.md");
/// Code Reviewer
pub const REVIEWER: &str = include_str!("../../prompts/reviewer.md");
/// DevOps Engineer
pub const DEVOPS: &str = include_str!("../../prompts/devops.md");
/// Security Auditor
pub const SECURITY: &str = include_str!("../../prompts/security.md");
/// UI/UX Designer
pub const DESIGNER: &str = include_str!("../../prompts/designer.md");
/// Technical Writer
pub const TECH_WRITER: &str = include_str!("../../prompts/tech_writer.md");
/// Screenshot testing skill
pub const SCREENSHOT_SKILL: &str = include_str!("../../prompts/screenshot_skill.md");
/// Engineering Manager
pub const MANAGER: &str = include_str!("../../prompts/manager.md");
/// MCP Agent
pub const MCP_AGENT: &str = include_str!("../../prompts/mcp_agent.md");
/// Senior Advisor / final reviewer
pub const ADVISOR: &str = include_str!("../../prompts/advisor.md");

/// Look up a skill by name from built-in prompts.
pub fn for_skill(name: &str) -> Option<&'static str> {
    match name {
        "screenshot_skill" => Some(SCREENSHOT_SKILL),
        _ => None,
    }
}

// ─── Tool-level prompts (oh-my-pi pattern) ─────────────────────────────
// Appended to tool descriptions at registration time. Guides the model
// on WHEN to use each tool, WHEN NOT TO, and critical constraints.
// Format follows `<instruction>` + `<critical>` convention.

pub mod tool_prompts {
    pub const CODE_GRAPH: &str = include_str!("../../prompts/tools/code_graph.md");
    pub const READ: &str = include_str!("../../prompts/tools/read.md");
    pub const SEARCH: &str = include_str!("../../prompts/tools/search.md");
    pub const BASH: &str = include_str!("../../prompts/tools/bash.md");
    pub const WRITE: &str = include_str!("../../prompts/tools/write.md");
    pub const EDIT: &str = include_str!("../../prompts/tools/edit.md");
    pub const DELEGATE: &str = include_str!("../../prompts/tools/delegate.md");
    pub const ASK: &str = include_str!("../../prompts/tools/ask.md");
    pub const WORKFLOW: &str = include_str!("../../prompts/tools/workflow.md");
    pub const PLAN: &str = include_str!("../../prompts/tools/plan.md");
    pub const TASK_REPORT: &str = include_str!("../../prompts/tools/task_report.md");
    pub const PLAYWRIGHT: &str = include_str!("../../prompts/tools/playwright.md");
    pub const MCP: &str = include_str!("../../prompts/tools/mcp.md");
    pub const GENERATE_IMAGE: &str = include_str!("../../prompts/tools/generate_image.md");
    pub const DOC_GRAPH_SCAN: &str = include_str!("../../prompts/tools/doc_graph_scan.md");
    pub const DOC_GRAPH_CONTEXT: &str = include_str!("../../prompts/tools/doc_graph_context.md");
    pub const DOC_WRITE: &str = include_str!("../../prompts/tools/doc_write.md");
    pub const DOC_INDEX: &str = include_str!("../../prompts/tools/doc_index.md");

    /// Look up a tool prompt by tool name. Returns `None` if no prompt exists.
    pub fn for_tool(name: &str) -> Option<&'static str> {
        Some(match name {
            "code_graph" | "code-graph" => CODE_GRAPH,
            "read" => READ,
            "search" => SEARCH,
            "bash" => BASH,
            "write" => WRITE,
            "edit" => EDIT,
            "delegate" => DELEGATE,
            "ask" => ASK,
            "workflow" => WORKFLOW,
            "plan" => PLAN,
            "task_report" => TASK_REPORT,
            "playwright" | "playwright_script" => PLAYWRIGHT,
            "mcp" | "mcp_connect" | "mcp_list" | "mcp_call" => MCP,
            "generate_image" => GENERATE_IMAGE,
            "doc_graph_scan" => DOC_GRAPH_SCAN,
            "doc_graph_context" => DOC_GRAPH_CONTEXT,
            "doc_write" => DOC_WRITE,
            "doc_index" => DOC_INDEX,
            _ => return None,
        })
    }
}

/// Look up a default role prompt by its id (e.g. `"pm"`, `"architect"`).
/// Look up a default role prompt by its id (e.g. `"pm"`, `"architect"`).
///
/// Returns `None` for unknown roles — callers should fall back to
/// a custom `prompt_file` path or an inline `prompt` string.
pub fn for_role(id: &str) -> Option<&'static str> {
    Some(match id {
        "pm" => PM,
        "architect" => ARCHITECT,
        "programmer" => PROGRAMMER,
        "tester" => TESTER,
        "reviewer" => REVIEWER,
        "devops" => DEVOPS,
        "security" => SECURITY,
        "designer" => DESIGNER,
        "tech_writer" => TECH_WRITER,
        "manager" => MANAGER,
        "mcp_agent" => MCP_AGENT,
        "advisor" => ADVISOR,
        _ => return None,
    })
}
/// Default role template factory for built-in roles. Returns `Some`
/// only for the 10 hard-coded roles below; callers use this when no
/// project config and no global config is present so the binary can
/// still serve its built-in defaults.
///
/// The role metadata (id, name, category, model_tier, tools, icon)
/// is duplicated from `.latte/agents/<id>.toml` because we want the
/// binary to work without that directory existing on disk. Keep the
/// two in sync when changing role metadata.
pub fn template_for(id: &str) -> Option<crate::role::RoleTemplate> {
    use crate::role::RoleTemplate;
    Some(match id {
        "pm" => RoleTemplate {
            id: "pm".into(),
            name: "Product Manager".into(),
            category: "planning".into(),
            model_tier: "standard".into(),
            model_chain: vec![],
            prompt_file: None,
            temperature: Some(0.7),
            tools: vec!["read".into(), "search".into()],
            icon: "📋".into(),
            skills: vec![],
            code_paths: vec![],
        },
        "architect" => RoleTemplate {
            id: "architect".into(),
            name: "System Architect".into(),
            category: "planning".into(),
            model_tier: "premium".into(),
            model_chain: vec![],
            prompt_file: None,
            temperature: Some(0.5),
            tools: vec!["read".into(), "search".into()],
            icon: "🏗️".into(),
            skills: vec![],
            code_paths: vec![],
        },
        "programmer" => RoleTemplate {
            id: "programmer".into(),
            name: "Software Engineer".into(),
            category: "execution".into(),
            model_tier: "budget".into(),
            model_chain: vec![],
            prompt_file: None,
            temperature: Some(0.3),
            tools: vec![
                "read".into(),
                "write".into(),
                "bash".into(),
                "search".into(),
            ],
            icon: "💻".into(),
            skills: vec![],
            code_paths: vec![],
        },
        "tester" => RoleTemplate {
            id: "tester".into(),
            name: "QA Engineer".into(),
            category: "verification".into(),
            model_tier: "budget".into(),
            model_chain: vec![],
            prompt_file: None,
            temperature: Some(0.4),
            tools: vec![
                "read".into(),
                "bash".into(),
                "search".into(),
            ],
            icon: "🧪".into(),
            skills: vec![],
            code_paths: vec![],
        },
        "reviewer" => RoleTemplate {
            id: "reviewer".into(),
            name: "Code Reviewer".into(),
            category: "verification".into(),
            model_tier: "standard".into(),
            model_chain: vec![],
            prompt_file: None,
            temperature: Some(0.4),
            tools: vec!["read".into(), "search".into()],
            icon: "🔍".into(),
            skills: vec![],
            code_paths: vec![],
        },
        "devops" => RoleTemplate {
            id: "devops".into(),
            name: "DevOps Engineer".into(),
            category: "execution".into(),
            model_tier: "budget".into(),
            model_chain: vec![],
            prompt_file: None,
            temperature: Some(0.3),
            tools: vec!["read".into(), "bash".into(), "write".into()],
            icon: "🚀".into(),
            skills: vec![],
            code_paths: vec![],
        },
        "security" => RoleTemplate {
            id: "security".into(),
            name: "Security Auditor".into(),
            category: "verification".into(),
            model_tier: "standard".into(),
            model_chain: vec![],
            prompt_file: None,
            temperature: Some(0.4),
            tools: vec!["read".into(), "search".into()],
            icon: "🛡️".into(),
            skills: vec![],
            code_paths: vec![],
        },
        "designer" => RoleTemplate {
            id: "designer".into(),
            name: "UI/UX Designer".into(),
            category: "planning".into(),
            model_tier: "standard".into(),
            model_chain: vec![],
            prompt_file: None,
            temperature: Some(0.7),
            tools: vec!["read".into()],
            icon: "🎨".into(),
            skills: vec![],
            code_paths: vec![],
        },
        "tech_writer" => RoleTemplate {
            id: "tech_writer".into(),
            name: "Technical Writer".into(),
            category: "execution".into(),
            model_tier: "budget".into(),
            model_chain: vec![],
            prompt_file: None,
            temperature: Some(0.5),
            tools: vec!["read".into(), "write".into()],
            icon: "📝".into(),
            skills: vec![],
            code_paths: vec![],
        },
        "manager" => RoleTemplate {
            id: "manager".into(),
            name: "Engineering Manager".into(),
            category: "planning".into(),
            model_tier: "premium".into(),
            model_chain: vec![],
            prompt_file: None,
            temperature: Some(0.5),
            tools: vec!["delegate".into(), "workflow".into(), "plan".into()],
            icon: "👔".into(),
            skills: vec![],
            code_paths: vec![],
        },
        "mcp_agent" => RoleTemplate {
            id: "mcp_agent".into(),
            name: "MCP Agent".into(),
            category: "execution".into(),
            model_tier: "budget".into(),
            model_chain: vec![],
            prompt_file: None,
            temperature: Some(0.3),
            tools: vec!["mcp".into(), "bash".into(), "read".into()],
            icon: "🔌".into(),
            skills: vec![],
            code_paths: vec![],
        },
        "advisor" => RoleTemplate {
            id: "advisor".into(),
            name: "Senior Advisor".into(),
            category: "verification".into(),
            model_tier: "premium".into(),
            model_chain: vec![],
            prompt_file: None,
            temperature: Some(0.4),
            tools: vec!["read".into(), "search".into()],
            icon: "🦉".into(),
            skills: vec![],
            code_paths: vec![],
        },
        _ => return None,
    })
}
#[cfg(test)]
mod tests {
    use super::*;

    /// 高产角色必须带**篇幅约束**。
    ///
    /// 起因：一次实测会话里 25 次「万字级」交付合计占掉 37% 的总模型
    /// 时间——单份 2.5 万字的文档要独占 254 秒，期间整条流水线在等。
    /// 而这些角色的 prompt 原来对篇幅**一个字都没提**。
    ///
    /// 刻意不用 `output_contract.max_chars` 强制：那条路超限即重试，
    /// 而重试是整份重新生成，硬加反而可能更慢。所以走 prompt 软约束。
    #[test]
    fn high_output_roles_declare_a_length_budget() {
        // 这些角色在实测里都产出过万字级交付。
        for id in [
            "architect",
            "programmer",
            "designer",
            "security",
            "reviewer",
            "devops",
            "pm",
        ] {
            let p = for_role(id).unwrap_or_else(|| panic!("missing prompt for '{id}'"));
            assert!(
                p.contains("## 篇幅"),
                "角色 '{id}' 的 prompt 缺少「篇幅」约束段"
            );
            // pm 沿用它自己的 300 字规则，其余给显式上限。
            if id == "pm" {
                assert!(
                    p.contains("300 字以内"),
                    "pm 应沿用既有的 300 字上限，而不是另立一个更宽的数值"
                );
            } else {
                assert!(
                    p.contains("默认上限"),
                    "角色 '{id}' 应给出明确的字数上限"
                );
            }
        }
    }

    /// Every role declared by `prompts::template_for` below must
    /// have a matching prompt here. Adding a role upstream
    /// without adding the prompt constant will fail this test.
    #[test]
    fn all_default_roles_have_prompts() {
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
            "advisor",
        ] {
            assert!(
                for_role(id).is_some(),
                "missing prompt constant for role '{id}'"
            );
            assert!(
                !for_role(id).unwrap().is_empty(),
                "empty prompt for role '{id}'"
            );
        }
    }

    /// manager 必须带「交付物收敛」门禁，且**不得**退化成拍脑袋的
    /// 轮次上限。
    ///
    /// 回归背景：一次实测会话里 manager 连跑 2 轮 `explore` + 2 个
    /// 「通读/解剖」委派（共 28 分钟、260 次工具调用），用户点名的
    /// 任务清单一个都没产出。但根因不是"探多了"——jemalloc 确实大。
    /// 根因是没人盯着交付物，所以门禁必须按交付物写，不能按轮数写
    /// （按计数刹车就是已移除的 WORKFLOW_BUDGET_PER_UNIT_SECS 那类
    /// 误杀健康长任务的错误）。
    #[test]
    fn manager_prompt_gates_on_deliverables_not_round_counts() {
        assert!(MANAGER.contains("交付物收敛"), "缺少门禁小节");
        assert!(MANAGER.contains("这里不限制调研轮数"), "必须明说不限轮数");
        assert!(MANAGER.contains("不许重复探索同一范围"), "缺少重复探索约束");
        // 选流程的判别依据必须是落点这条通则，而不是逐个流程列举的对照表
        // （个别化的表覆盖不到自定义流程，且与按话题选的说法互相矛盾）。
        assert!(MANAGER.contains("按交付物落点"), "缺少落点判别通则");
        assert!(MANAGER.contains("话题词不决定落点"), "缺少话题≠落点的澄清");
        assert!(
            !MANAGER.contains("学习/调研/规划类诉求"),
            "按话题选流程的说法必须清除"
        );
        // 「先不锁方向」不得成为回退调研的借口。
        assert!(MANAGER.contains("先不锁方向"), "未覆盖 punt-back 分支");
        // 硬性反向断言：不得出现任何轮次上限。
        assert!(
            !MANAGER.contains("调研最多 1 轮"),
            "轮次上限已被判定为错误设计，不得再出现"
        );
    }

    /// `ask` 必须禁止「没有下一步动作」的兜底选项。用户一旦选中它，
    /// manager 就失去收敛依据，实测直接退回又一轮调研。
    #[test]
    fn ask_prompt_forbids_punt_back_options() {
        let ask = tool_prompts::ASK;
        assert!(ask.contains("punt-back"), "未禁止 punt-back 选项");
        assert!(ask.contains("先不锁方向"), "未给出具体反例");
    }
}
