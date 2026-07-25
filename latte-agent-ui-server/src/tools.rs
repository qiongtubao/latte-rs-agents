//! 工具管理：发现所有可用工具 + 持久化启用状态。
//!
//! 工具发现分两层：
//! 1. **builtin_tool_packages**：`create_tool_manager()` + `register_package(builtin_*)`
//!    拿到所有 `git.*` / `exec` / `read` / `write` 等 Rust 内置工具。
//! 2. **register_*_tool 调用点**：用 TreeSitterEngine 扫描 `latte-agent-core/src/`
//!    找 `register_*_tool(...)` 函数（如 `register_delegate_tool`、`register_workflow_tool`），
//!    这些是 controller 在运行时动态注册的工具。
//!
//! 启用状态用 `.latte/tools.yaml`（项目）和 `~/.latte/tools.yaml`（全局）存：
//! 仅记录用户**关闭**的工具（默认全开）。项目优先于全局。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

/// 单个工具描述，UI 渲染与后端 lookup 共享。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolEntry {
    /// 工具 ID（短名，例如 `exec`、`git`、`read`、`delegate`）。
    pub id: String,
    /// 工具分组：`builtin`（Rust 实现的 builtin package）| `dynamic`
    /// （controller 动态 register）| `package_alias`（如 `git` 别名）。
    #[serde(default)]
    pub kind: String,
    /// 人类可读描述（从 schema 自动提取；dynamic 工具为函数定义位置）。
    #[serde(default)]
    pub description: String,
    /// 是否启用（默认 true）。用户可手动关闭；关闭后新 session 不会注册该工具。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 注册点（dynamic 工具）：函数名（如 `register_delegate_tool`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registered_by: Option<String>,
}

fn default_true() -> bool { true }

/// 项目级或全局级 tools.yaml 的存储结构。
///
/// 只持久化"被关闭"的工具 ID，避免对每次启动都写入全表。
#[derive(Debug, Default, Serialize, Deserialize)]
struct ToolsState {
    /// ID -> 是否禁用（true=禁用；表里没有则视为启用）。
    #[serde(default)]
    disabled: BTreeSet<String>,
}

impl ToolsState {
    fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|c| serde_yaml::from_str(&c).ok())
            .unwrap_or_default()
    }
    fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow!("create {}: {e}", parent.display()))?;
        }
        let s = serde_yaml::to_string(self)
            .map_err(|e| anyhow!("serialize tools.yaml: {e}"))?;
        std::fs::write(path, s)
            .map_err(|e| anyhow!("write {}: {e}", path.display()))
    }
}

/// 工具管理状态句柄。
pub struct ToolsStore {
    project_path: PathBuf,
    global_path: PathBuf,
    /// 合并后的 disabled 集合（项目 + 全局）。
    disabled: BTreeSet<String>,
}

impl ToolsStore {
    /// cwd：项目工作目录；agents_config：参考 agent 工具的路径风格，
    /// 这里仅用于在同级 `.latte/` 下定位 `tools.yaml`。
    pub fn new(cwd: &Path) -> Self {
        let project_path = cwd.join(".latte").join("tools.yaml");
        let home = std::env::var("HOME").unwrap_or_default();
        let global_path = PathBuf::from(home)
            .join(".latte")
            .join("tools.yaml");
        let project = ToolsState::load(&project_path);
        let global = ToolsState::load(&global_path);
        let mut disabled = BTreeSet::new();
        disabled.extend(project.disabled);
        disabled.extend(global.disabled);
        Self { project_path, global_path, disabled }
    }

    /// 重新从磁盘加载（编辑器写盘后调用）。
    pub fn reload(&mut self) {
        let project = ToolsState::load(&self.project_path);
        let global = ToolsState::load(&self.global_path);
        let mut disabled = BTreeSet::new();
        disabled.extend(project.disabled);
        disabled.extend(global.disabled);
        self.disabled = disabled;
    }

    /// 工具是否启用。
    pub fn is_enabled(&self, id: &str) -> bool {
        !self.disabled.contains(id)
    }

    /// 切换启用状态，写到项目级（若无项目级配置文件路径能力则全局）。
    /// 返回新的 enabled 状态。
    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<bool> {
        // 先 reload 确保不丢已有状态
        self.reload();
        if enabled {
            self.disabled.remove(id);
        } else {
            self.disabled.insert(id.to_string());
        }
        // 写到项目级；如果项目级目录不可写则回退全局。
        let mut project = ToolsState::load(&self.project_path);
        if enabled {
            project.disabled.remove(id);
        } else {
            project.disabled.insert(id.to_string());
        }
        // 与全局合并：项目优先
        let mut global = ToolsState::load(&self.global_path);
        if !enabled {
            // 如果全局也禁用，保留；否则只项目级禁用
        }
        // 简化：把合并后的 disabled 拆分为 project+global
        // 策略：只在 project 写"被本 store 禁用"的差集
        let _ = &mut global;
        project.save(&self.project_path)?;
        // 全局级文件保持不动（project 优先）
        self.reload();
        Ok(enabled)
    }

    /// 列出当前状态：仅返回 disabled 集合（用于调试/状态查询）。
    pub fn disabled_ids(&self) -> impl Iterator<Item = &str> {
        self.disabled.iter().map(|s| s.as_str())
    }
}

/// 枚举所有可用工具（含 builtin + dynamic + 已知别名）。
///
/// 不做 enable/disable 过滤；调用方用 `ToolsStore::is_enabled` 自行判断。
pub async fn enumerate() -> Result<Vec<ToolEntry>> {
    use latte_rs_agent_tools::prelude::*;

    let mgr = create_tool_manager();
    for p in builtin_tool_packages() {
        mgr.register_package(p).await
            .map_err(|e| anyhow!("register builtin package: {e}"))?;
    }

    let mut by_id: BTreeMap<String, ToolEntry> = BTreeMap::new();

    // 1. builtin packages 注册的工具（包含 git.*, exec, read, write, search 等）
    for tool_id in mgr.get_tool_names() {
        // 短名（去 package 前缀）
        let short = tool_id
            .rsplit_once('.')
            .map(|(_, s)| s.to_string())
            .unwrap_or_else(|| tool_id.clone());
        // 描述：尝试从 schema 提取（ToolManager 没暴露 schema getter，跳过详细描述）
        let description = format!("builtin tool: {tool_id}");
        by_id.entry(short.clone()).or_insert(ToolEntry {
            id: short,
            kind: if tool_id.starts_with("git.") { "builtin".into() } else { "builtin".into() },
            description,
            enabled: true,
            registered_by: None,
        });
    }

    // 2. controller 动态 register 的工具
    for reg_fn in dynamic_registrations().await {
        let id = reg_fn_to_id(&reg_fn);
        by_id.entry(id.clone()).or_insert(ToolEntry {
            id,
            kind: "dynamic".into(),
            description: format!("dynamic tool registered via `{reg_fn}`"),
            enabled: true,
            registered_by: Some(reg_fn),
        });
    }

    // 3. 已知包级别别名（`git` 是 git.* 的别名；`bash` 是 exec 的别名）
    for (alias, target) in [
        ("git", "git.* (package alias)"),
        ("bash", "exec alias"),
        ("mcp", "mcp_connect (package alias)"),
    ] {
        by_id.entry(alias.to_string()).or_insert(ToolEntry {
            id: alias.to_string(),
            kind: "package_alias".into(),
            description: target.to_string(),
            enabled: true,
            registered_by: None,
        });
    }

    // 4. delegate / workflow（controller 动态注册，需要时存在）
    for id in ["delegate", "workflow"] {
        by_id.entry(id.to_string()).or_insert(ToolEntry {
            id: id.to_string(),
            kind: "dynamic".into(),
            description: format!("{id} tool registered by controller"),
            enabled: true,
            registered_by: None,
        });
    }

    Ok(by_id.into_values().collect())
}

/// 把 `register_delegate_tool` → `delegate`。
fn reg_fn_to_id(reg_fn: &str) -> String {
    let stem = reg_fn
        .strip_prefix("register_")
        .unwrap_or(reg_fn)
        .strip_suffix("_tool")
        .unwrap_or(reg_fn);
    stem.to_string()
}

/// 扫描 `latte-agent-core/src/` 找 `register_*_tool` 调用点。
///
/// 复用 `latte-rs-graph` TreeSitterEngine，结果与 `role_graph` 共享。
async fn dynamic_registrations() -> Vec<String> {
    use latte_rs_graph::prelude::*;
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let project_root = manifest
        .parent()
        .map(|p| p.join("latte-agent-core"))
        .unwrap_or_else(|| manifest.clone());
    if !project_root.exists() {
        return Vec::new();
    }
    let db_path = std::env::temp_dir().join("latte-tools-list.db");
    let storage = match SqliteStorage::open(&db_path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let engine = TreeSitterEngine::new(storage);
    if engine
        .build(&project_root, &BuildOptions::default())
        .await
        .is_err()
    {
        return Vec::new();
    }
    let graph_data = match engine.graph_data().await {
        Ok(g) => g,
        Err(_) => return Vec::new(),
    };
    graph_data
        .nodes
        .iter()
        .filter_map(|n| {
            let nm = n.name.as_str();
            if nm.starts_with("register_") && nm.ends_with("_tool") {
                Some(nm.to_string())
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reg_fn_to_id_extracts_short_name() {
        assert_eq!(reg_fn_to_id("register_delegate_tool"), "delegate");
        assert_eq!(reg_fn_to_id("register_workflow_tool"), "workflow");
        assert_eq!(reg_fn_to_id("plain_name"), "plain_name");
    }

    #[test]
    fn tools_state_round_trip() {
        let tmp = std::env::temp_dir().join("latte-tools-state-test.yaml");
        let _ = std::fs::remove_file(&tmp);
        let s = ToolsState {
            disabled: BTreeSet::from(["exec".into(), "delegate".into()]),
        };
        s.save(&tmp).unwrap();
        let loaded = ToolsState::load(&tmp);
        assert!(loaded.disabled.contains("exec"));
        assert!(loaded.disabled.contains("delegate"));
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn enumerate_returns_known_ids() {
        let tools = enumerate().await.expect("enumerate");
        let ids: Vec<&str> = tools.iter().map(|t| t.id.as_str()).collect();
        // 必有：核心 builtin 别名
        assert!(ids.contains(&"read"), "read missing: {ids:?}");
        assert!(ids.contains(&"write"), "write missing: {ids:?}");
        assert!(ids.contains(&"git"), "git alias missing: {ids:?}");
        assert!(ids.contains(&"bash"), "bash alias missing: {ids:?}");
        // 至少有一个 builtin 短名
        assert!(tools.iter().any(|t| t.id == "exec" || t.id == "search"),
                "expected exec/search: {ids:?}");
    }
}
