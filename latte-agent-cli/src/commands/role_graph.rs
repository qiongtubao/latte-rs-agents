//! Build a "role × tool" code-graph for the running project.
//!
//! The graph is built by combining two sources of truth:
//! 1. **Code graph** (TreeSitterEngine): scans `latte-agent-core/src/` for
//!    function definitions. We look specifically for `register_*_tool(...)`
//!    call sites — these are the prose locations where a role's
//!    allowed_tools list gets wired into a working ToolManager.
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

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{anyhow, Result};
use latte_rs_graph::prelude::*;
use serde::{Deserialize, Serialize};

use super::config_layer;

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

/// Build the role-graph for a project rooted at `cwd`.
///
/// - Loads `.latte/agents.d/*.toml` via the CLI's config loader.
/// - Uses TreeSitterEngine to scan `project_root` (the latte-agents repo)
///   for `register_*_tool` call sites — these are the code-level evidence
///   that a role wires its tools in.
/// - Combines the two: every (role, tool_name) pair declared in TOML
///   becomes a `Role -[USES_TOOL]-> Tool` edge. Every `register_*_tool`
///   function becomes a `ToolRegistration` node.
pub async fn build(cwd: &Path, project_root: &Path) -> Result<RoleGraph> {
    // 1. Load agent config from `.latte/agents.d/`.
    let agents_cfg_dir = cwd.join(".latte/agents.d");
    let models_cfg_dir = cwd.join(".latte/models.d");
    let resolved = config_layer::load(
        Some(agents_cfg_dir.to_str().ok_or_else(|| {
            anyhow!("agents path contains non-utf8")
        })?),
        Some(models_cfg_dir.to_str().ok_or_else(|| {
            anyhow!("models path contains non-utf8")
        })?),
        config_layer::CliOverrides {
            api_key: None,
            api_key_target: None,
            field_overrides: vec![],
        },
    )
    .map_err(|e| anyhow!("load agent config: {e}"))?;
    let cfg = resolved.config;

    // 2. Build the code graph via TreeSitterEngine over a SQLite DB in
    //    $TMPDIR. TreeSitterEngine currently requires SqliteStorage.
    let db_path = std::env::temp_dir().join("latte-graph-build.db");
    let storage = SqliteStorage::open(&db_path)
        .map_err(|e| anyhow!("open graph.db: {e}"))?;
    let engine = TreeSitterEngine::new(storage);
    engine
        .build(project_root, &BuildOptions::default())
        .await
        .map_err(|e| anyhow!("graph build {}: {e}", project_root.display()))?;
    let graph_data = engine
        .graph_data()
        .await
        .map_err(|e| anyhow!("graph_data: {e}"))?;

    // 3. Pull `register_*_tool` symbols from the graph.
    let mut registrations: Vec<ToolRegistration> = Vec::new();
    for n in &graph_data.nodes {
        let nm = n.name.as_str();
        if nm.starts_with("register_") && nm.ends_with("_tool") {
            registrations.push(ToolRegistration {
                function: nm.to_string(),
            });
        }
    }

    // 4. Combine config + code.
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

    for r in &registrations {
        let reg_id = format!("reg:{}", r.function);
        nodes.push(RoleGraphNode {
            id: reg_id.clone(),
            kind: "ToolRegistration".into(),
            label: r.function.clone(),
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
            registrations: registrations.len(),
        },
        nodes,
        edges,
        project_root: project_root.display().to_string(),
    })
}

#[derive(Debug, Clone)]
struct ToolRegistration {
    function: String,
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

#[allow(dead_code)]
fn _btreemap_keep<T>(v: BTreeMap<String, T>) -> usize {
    v.len()
}
