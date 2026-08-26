//! Build a "role × tool" code-graph for the running project.
//!
//! The graph is built by combining two sources of truth:
//! 1. **Code scan** (`scan_register_tool_fns`): text-scans `project_root`
//!    for `register_*_tool` function identifiers. These are the code-level
//!    evidence that a role's allowed_tools list gets wired into a working
//!    ToolManager.
//!    (Previously this used `latte-rs-graph` + a SQLite-backed
//!    TreeSitterEngine build that took >60s per request; the text
//!    scan returns the same identifier set in milliseconds.)
//! 2. **Config graph** (TOML loader): reads `.latte/agents.d/*.toml` and
//!    their `tools = [...]` declarations to determine which tools each
//!    role declared it can use.
//!
//! Output is a `RoleGraph` JSON:
//!   {
//!     nodes: [{ id, kind: "Role"|"Tool"|"ToolRegistration", label }],
//!     edges: [{ source, target, kind: "USES_TOOL"|"REGISTERED_BY", }],
//!     stats: { roles: 8, tools: 5, registrations: 6 }
//!   }
//!
//! This powers the `latte-agent ui` Graph panel — the HTTP endpoint
//! `/api/role-graph` serves this JSON.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{anyhow, Result};
use latte_agent_core::config::AgentConfig;
use serde::{Deserialize, Serialize};

use crate::tools::scan_register_tool_fns;

/// JSON wire shape returned to the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleGraph {
    pub nodes: Vec<RoleGraphNode>,
    pub edges: Vec<RoleGraphEdge>,
    pub stats: RoleGraphStats,
    pub project_root: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleGraphNode {
    pub id: String,
    /// "Role" | "Tool" | "ToolRegistration"
    pub kind: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleGraphEdge {
    pub source: String,
    pub target: String,
    /// "USES_TOOL" | "REGISTERED_BY"
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RoleGraphStats {
    pub roles: usize,
    pub tools: usize,
    pub registrations: usize,
}

/// Load the merged agent config for the role graph.
///
/// 与原 CLI `config_layer::load(agents, models, 空 CliOverrides)` 对
/// **roles** 的语义等价：`AgentConfig::load_with_global` 已经合并项目
/// `agents.d` + 全局 `~/.latte/agents.d` + 内建角色，这里再把
/// `models.d` 里可能定义的角色并进来。原 loader 里的 GlobalConfig
/// 模型合并（`merge_into_project`）只改写 `models` 不动 `roles`，
/// 而本模块只读 `roles`（name / model_tier / tools / prompt_file），
/// 所以从略。
fn load_agent_config(agents_dir: &Path, models_dir: &Path) -> Result<AgentConfig> {
    let agents = agents_dir
        .to_str()
        .ok_or_else(|| anyhow!("agents path contains non-utf8"))?;
    let mut cfg = AgentConfig::load_with_global(Some(agents))
        .map_err(|e| anyhow!("load agent config: {e}"))?;
    if models_dir.exists() {
        let models = models_dir
            .to_str()
            .ok_or_else(|| anyhow!("models path contains non-utf8"))?;
        let part =
            AgentConfig::load(models).map_err(|e| anyhow!("load models config: {e}"))?;
        cfg.roles.extend(part.roles);
    }
    Ok(cfg)
}

/// Build the role-graph for a project rooted at `cwd`.
///
/// - Loads `.latte/agents.d/*.toml` via the agent config loader.
/// - Text-scans `project_root` for `register_*_tool` function names —
///   these are the code-level call sites where roles' allowed_tools
///   get wired into a working ToolManager.
/// - Combines the two: every (role, tool_name) pair declared in TOML
///   becomes a `Role -[USES_TOOL]-> Tool` edge. Every `register_*_tool`
///   function becomes a `ToolRegistration` node pointing at `(CWD)`.
pub async fn build(cwd: &Path, project_root: &Path) -> Result<RoleGraph> {
    // 1. Load agent config from `.latte/agents.d/`.
    let agents_cfg_dir = cwd.join(".latte/agents.d");
    let models_cfg_dir = cwd.join(".latte/models.d");
    let cfg = load_agent_config(&agents_cfg_dir, &models_cfg_dir)?;

    // 2. Scan `project_root` for `register_*_tool` identifiers. The
    //    previous implementation built a full TreeSitter code graph over
    //    a SQLite DB in $TMPDIR (~60s per request). The text scan
    //    returns the same identifier set in milliseconds.
    let register_fns = scan_register_tool_fns(project_root).await;

    // 3. Combine config + code.
    let mut nodes: Vec<RoleGraphNode> = Vec::new();
    let mut edges: Vec<RoleGraphEdge> = Vec::new();
    let mut seen_role: BTreeSet<String> = BTreeSet::new();
    let mut seen_tool: BTreeSet<String> = BTreeSet::new();

    for (role_id, role) in &cfg.roles {
        let role_node_id = format!("role:{role_id}");
        seen_role.insert(role_id.clone());
        let label = if role.name.is_empty() {
            role_id.clone()
        } else {
            role.name.clone()
        };
        let detail = format!(
            "tier={} · tools={} · prompt={}",
            role.model_tier,
            role.tools.len(),
            role.prompt_file.as_deref().unwrap_or("?"),
        );
        nodes.push(RoleGraphNode {
            id: role_node_id.clone(),
            kind: "Role".into(),
            label,
            detail: Some(detail),
        });

        for t in &role.tools {
            let tool_node_id = format!("tool:{t}");
            if seen_tool.insert(t.clone()) {
                nodes.push(RoleGraphNode {
                    id: tool_node_id.clone(),
                    kind: "Tool".into(),
                    label: t.clone(),
                    detail: None,
                });
            }
            edges.push(RoleGraphEdge {
                source: role_node_id.clone(),
                target: tool_node_id,
                kind: "USES_TOOL".into(),
            });
        }
    }

    for r in &register_fns {
        let reg_id = format!("reg:{r}");
        nodes.push(RoleGraphNode {
            id: reg_id.clone(),
            kind: "ToolRegistration".into(),
            label: r.clone(),
            detail: Some("latte-agent-core controller register_*_tool call site".into()),
        });
        edges.push(RoleGraphEdge {
            source: reg_id,
            target: "(CWD)".into(),
            kind: "REGISTERED_BY".into(),
        });
    }

    Ok(RoleGraph {
        stats: RoleGraphStats {
            roles: seen_role.len(),
            tools: seen_tool.len(),
            registrations: register_fns.len(),
        },
        nodes,
        edges,
        project_root: project_root.display().to_string(),
    })
}

/// A deterministic palette used by the UI to color nodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodePalette {
    pub role: String,
    pub tool: String,
    pub registration: String,
}

pub fn palette() -> NodePalette {
    NodePalette {
        role: "#4f46e5".into(),
        tool: "#0d9488".into(),
        registration: "#b45309".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 写一个最小角色 TOML 到 `<dir>/<id>.toml`：必填字段
    /// id/name/category/model_tier，tools 可选。
    fn write_role(dir: &Path, id: &str, name: &str, tools: &[&str]) {
        std::fs::create_dir_all(dir).unwrap();
        let tools_toml = if tools.is_empty() {
            String::new()
        } else {
            let items: Vec<String> = tools.iter().map(|t| format!("\"{t}\"")).collect();
            format!("tools = [{}]\n", items.join(", "))
        };
        let content = format!(
            "[roles.{id}]\n\
             id = \"{id}\"\n\
             name = \"{name}\"\n\
             category = \"management\"\n\
             model_tier = \"standard\"\n\
             {tools_toml}",
        );
        std::fs::write(dir.join(format!("{id}.toml")), content).unwrap();
    }

    /// 完整构建路径：角色 TOML + 项目根目录里的 register_*_tool 源文件
    /// → stats 与节点/边符合预期；无 `_tool` 后缀的 identifier 不应被
    /// 计为注册节点。
    #[tokio::test]
    async fn build_collects_roles_tools_and_registrations() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        let agents_d = cwd.join(".latte/agents.d");
        write_role(&agents_d, "pm", "Product Manager", &["read", "search"]);
        write_role(&agents_d, "dev", "Software Engineer", &["read", "write", "bash"]);

        // Fake latte-agent-core source tree.
        let core_src = cwd.join("latte-agent-core/src");
        std::fs::create_dir_all(&core_src).unwrap();
        std::fs::write(
            core_src.join("controller.rs"),
            "fn register_delegate_tool() {}\n\
             fn register_workflow_tool() {}\n\
             fn register_unrelated() {}\n",
        )
        .unwrap();

        let g = build(cwd, &core_src).await.expect("build");

        assert_eq!(g.stats.roles, 2);
        assert_eq!(g.stats.tools, 4); // read, search, write, bash
        assert_eq!(g.stats.registrations, 2); // delegate + workflow (no _tool excluded)

        // ToolRegistration 节点是 scan 出来的两个函数名。
        let reg_labels: Vec<&str> = g
            .nodes
            .iter()
            .filter(|n| n.kind == "ToolRegistration")
            .map(|n| n.label.as_str())
            .collect();
        assert!(reg_labels.contains(&"register_delegate_tool"));
        assert!(reg_labels.contains(&"register_workflow_tool"));
        // 没有 `_tool` 后缀的不计。
        assert!(!reg_labels.contains(&"register_unrelated"));

        // USES_TOOL 边数 = pm 2 + dev 3 = 5。
        let uses_edges = g.edges.iter().filter(|e| e.kind == "USES_TOOL").count();
        assert_eq!(uses_edges, 5);

        // 每个 ToolRegistration 都有一行 REGISTERED_BY 指向 (CWD)。
        let reg_edges = g.edges.iter().filter(|e| e.kind == "REGISTERED_BY").count();
        assert_eq!(reg_edges, 2);

        // 每个 role 的 detail 至少有 tier 信息。
        let role_with_detail = g
            .nodes
            .iter()
            .filter(|n| n.kind == "Role")
            .any(|n| n.detail.as_deref().unwrap_or("").contains("tier="));
        assert!(role_with_detail);
    }

    /// 项目根目录不存在 → 不能 panic，registrations 为空；roles 走
    /// `AgentConfig::load_with_global` 的内建兜底（11 个 built-ins），
    /// 只要 build() 正常返回就视为通过。
    #[tokio::test]
    async fn build_handles_missing_project_root() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        // 空 .latte/agents.d：路径存在，load_with_global 不报错。
        std::fs::create_dir_all(cwd.join(".latte/agents.d")).unwrap();
        let missing = cwd.join("does/not/exist");
        let g = build(cwd, &missing).await.expect("build");
        assert_eq!(g.stats.registrations, 0);
        // built-in 角色兜底 → 至少有 1 个 Role 节点。
        assert!(g.stats.roles > 0);
    }
}