//! 工具管理：发现所有可用工具 + 持久化启用状态。
//!
//! 工具发现分两层：
//! 1. **builtin_tool_packages**：`create_tool_manager()` + `register_package(builtin_*)`
//!    拿到所有 `git.*` / `exec` / `read` / `write` 等 Rust 内置工具。
//! 2. **register_*_tool 调用点**：轻量文本扫描 `latte-agent-core/src/**/*.rs`
//!    找 `register_*_tool` 标识符（如 `register_delegate_tool`、`register_workflow_tool`），
//!    这些是 controller 在运行时动态注册的工具。
//!    （原实现用 TreeSitterEngine 对整个 crate 建代码图，单次 >60s，
//!    只为提取两个函数名——文本匹配毫秒级，结果等价。）
//!
//! 启用状态用 `.latte/tools.yaml`（项目）和 `~/.latte/tools.yaml`（全局）存：
//! 仅记录用户**关闭**的工具（默认全开）。项目优先于全局。
//!
//! 枚举结果缓存由服务启动时的预热线程填充（见 `lib.rs` 的 `spawn`），
//! 预热完成前 HTTP 请求返回硬编码 fallback 列表，保证首次请求也立即响应。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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
        self.reload();
        if enabled {
            self.disabled.remove(id);
        } else {
            self.disabled.insert(id.to_string());
        }
        let mut project = ToolsState::load(&self.project_path);
        if enabled {
            project.disabled.remove(id);
        } else {
            project.disabled.insert(id.to_string());
        }
        let _ = &mut ToolsState::load(&self.global_path);
        project.save(&self.project_path)?;
        self.reload();
        Ok(enabled)
    }

    /// 列出当前状态：仅返回 disabled 集合（用于调试/状态查询）。
    pub fn disabled_ids(&self) -> impl Iterator<Item = &str> {
        self.disabled.iter().map(|s| s.as_str())
    }
}

// ─── 枚举缓存 ────────────────────────────────────────────────────────
//
// 预热（服务启动时 spawn）填充此缓存。预热完成前 HTTP 请求返回 fallback。
// 预热完成后 `set_enumerate_cache` 被调用，后续请求走快速路径。

static ENUMERATE_CACHE: std::sync::RwLock<Option<Vec<ToolEntry>>> = std::sync::RwLock::new(None);

/// 设置缓存（预热完成后调用）。
pub fn set_enumerate_cache(tools: Vec<ToolEntry>) {
    if let Ok(mut cache) = ENUMERATE_CACHE.write() {
        *cache = Some(tools);
    }
}

/// 清空缓存（测试用）。
pub fn clear_cache() {
    if let Ok(mut cache) = ENUMERATE_CACHE.write() {
        *cache = None;
    }
}

/// 枚举所有可用工具（含 builtin + dynamic + 已知别名）。
///
/// 不做 enable/disable 过滤；调用方用 `ToolsStore::is_enabled` 自行判断。
/// 结果由服务启动时的预热线程填充，预热完成前返回硬编码 fallback 列表，
/// 保证第一次 HTTP 请求也立即返回（毫秒级）。
pub async fn enumerate() -> Result<Vec<ToolEntry>> {
    // 快速路径：缓存已就绪
    if let Ok(cache) = ENUMERATE_CACHE.read() {
        if let Some(tools) = &*cache {
            return Ok(tools.clone());
        }
    }
    // 缓存未就绪（预热尚未完成），返回 fallback 列表
    Ok(fallback_tools())
}

/// 硬编码的核心工具回退列表（`enumerate()` 预热未完成时使用）。
fn fallback_tools() -> Vec<ToolEntry> {
    vec![
        ToolEntry { id: "read".into(), kind: "builtin".into(), description: "读取文件或目录内容".into(), enabled: true, registered_by: None },
        ToolEntry { id: "write".into(), kind: "builtin".into(), description: "写入或覆盖文件".into(), enabled: true, registered_by: None },
        ToolEntry { id: "edit".into(), kind: "builtin".into(), description: "对文件做精确的替换/插入/删除编辑".into(), enabled: true, registered_by: None },
        ToolEntry { id: "exec".into(), kind: "builtin".into(), description: "执行 shell 命令".into(), enabled: true, registered_by: None },
        ToolEntry { id: "bash".into(), kind: "package_alias".into(), description: "exec alias".into(), enabled: true, registered_by: None },
        ToolEntry { id: "git".into(), kind: "package_alias".into(), description: "git.* (package alias)".into(), enabled: true, registered_by: None },
        ToolEntry { id: "search".into(), kind: "builtin".into(), description: "搜索文件内容（正则 + glob）".into(), enabled: true, registered_by: None },
        ToolEntry { id: "grep".into(), kind: "builtin".into(), description: "全局正则搜索".into(), enabled: true, registered_by: None },
        ToolEntry { id: "delegate".into(), kind: "dynamic".into(), description: "delegate tool registered by controller".into(), enabled: true, registered_by: None },
        ToolEntry { id: "workflow".into(), kind: "dynamic".into(), description: "workflow tool registered by controller".into(), enabled: true, registered_by: None },
        ToolEntry { id: "mcp".into(), kind: "package_alias".into(), description: "mcp_connect (package alias)".into(), enabled: true, registered_by: None },
    ]
}

/// 完整枚举（含 TreeSitter 解析），由预热线程调用，结果写入 `ENUMERATE_CACHE`。
pub(crate) async fn enumerate_inner() -> Result<Vec<ToolEntry>> {
    use latte_rs_agent_tools::prelude::*;

    let mgr = create_tool_manager();
    for p in builtin_tool_packages() {
        mgr.register_package(p).await
            .map_err(|e| anyhow!("register builtin package: {e}"))?;
    }

    let mut by_id: BTreeMap<String, ToolEntry> = BTreeMap::new();

    // 1. builtin packages 注册的工具
    for tool_id in mgr.get_tool_names() {
        let short = tool_id
            .rsplit_once('.')
            .map(|(_, s)| s.to_string())
            .unwrap_or_else(|| tool_id.clone());
        let description = format!("builtin tool: {tool_id}");
        by_id.entry(short.clone()).or_insert(ToolEntry {
            id: short,
            kind: "builtin".into(),
            description,
            enabled: true,
            registered_by: None,
        });
    }

    // 2. controller 动态 register 的 tool entries
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

    // 3. 已知包级别别名
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

    // 4. delegate / workflow
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

/// Scan `<root>/**/*.rs` for `register_*_tool` identifier-shaped tokens.
///
/// 轻量文本扫描：`dynamic_registrations()` 原来用 latte-rs-graph 的
/// TreeSitterEngine 全量建图 + SQLite，单次 >60s；本函数与建图返回的
/// 标识符集合等价，毫秒级。两个调用方（`enumerate_inner` + role-graph
/// 端点）共用，避免把代码图重新拉一遍。
///
/// 匹配规则：
/// - 文件后缀必须是 `.rs`；
/// - 取字面 `register_` 之后连续的 `[A-Za-z0-9_]+`；
/// - 至少一个非空前缀字符（裸 `register_` 丢弃）；
/// - 必须以 `_tool` 结尾（`register_foo`、`register_foo_tool123` 不计）。
///
/// 返回值是去重 + 排序后的 `Vec<String>`（`BTreeSet` 序），保证调用方
/// 拿到稳定列表，便于 snapshot test 与对照。
pub(crate) async fn scan_register_tool_fns(root: &Path) -> Vec<String> {
    let mut found: BTreeSet<String> = BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let mut rest = text.as_str();
            while let Some(pos) = rest.find("register_") {
                let name: String = rest[pos..]
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if name.len() > "register_".len() && name.ends_with("_tool") {
                    found.insert(name);
                }
                rest = &rest[pos + "register_".len()..];
            }
        }
    }
    found.into_iter().collect()
}

/// 扫描 `latte-agent-core/src/**/*.rs` 找 `register_*_tool` 标识符。
///
/// 路径由 `CARGO_MANIFEST_DIR` 解析到 `latte-agent-ui-server` 的同级
/// crate，与仓库目录布局绑定（与 `tools.rs` 旧实现的硬编码路径一致，
/// 保持调用方零修改）。
async fn dynamic_registrations() -> Vec<String> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let project_root = manifest
        .parent()
        .map(|p| p.join("latte-agent-core"))
        .unwrap_or_else(|| manifest.clone());
    scan_register_tool_fns(&project_root.join("src")).await
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

    /// `scan_register_tool_fns` 必须挑出所有 `register_*_tool` 标识符、
    /// 去重、跳过非 `.rs` 文件与无 `_tool` 后缀的 identifier，且对裸
    /// `register_` 字面量不计入。
    #[tokio::test]
    async fn scan_register_tool_fns_finds_matching_identifiers() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();

        // 命中：两个标准 register_*_tool 函数定义。
        std::fs::write(
            src.join("controller.rs"),
            "fn register_delegate_tool() {}\n\
             fn register_workflow_tool() {}\n",
        )
        .unwrap();

        // 子目录里的命中 + 严格不命中混在一起：
        // - `register_extra_tool` 命中（以 `_tool` 结尾）
        // - `register_no_match` 不命中（不以 `_tool` 结尾）
        // - `register__tool` 命中（空前缀但仍是合法 identifier）
        // - 字符串字面量 `register_in_string_tool` 也命中（文本扫描不过滤词法）
        let nested = src.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            nested.join("extra.rs"),
            "fn register_extra_tool() {}\n\
             fn register_no_match() {}   // does NOT end with _tool\n\
             fn register__tool() {}      // empty prefix but valid identifier\n\
             let s = \"register_in_string_tool\"; // string literals count too\n",
        )
        .unwrap();

        // 不应被扫描：非 .rs 文件。
        std::fs::write(src.join("readme.md"), "fn register_ignored_tool() {}").unwrap();
        // .rs 但不含 `_tool` 结尾。
        std::fs::write(src.join("helpers.rs"), "fn register_helper() {}\n").unwrap();

        let mut found = scan_register_tool_fns(&src).await;
        found.sort();

        assert_eq!(
            found,
            vec![
                "register__tool".to_string(),
                "register_delegate_tool".to_string(),
                "register_extra_tool".to_string(),
                "register_in_string_tool".to_string(),
                "register_workflow_tool".to_string(),
            ]
        );
        // `register_no_match` / `register_helper` 不在结果里（双重保险）：
        assert!(!found.iter().any(|n| n == "register_no_match"));
        assert!(!found.iter().any(|n| n == "register_helper"));
    }

    /// 不存在的根目录 / 空目录 → 空结果（不能 panic、不能误报）。
    #[tokio::test]
    async fn scan_register_tool_fns_handles_missing_and_empty_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(scan_register_tool_fns(&tmp.path().join("does/not/exist")).await.is_empty());
        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(scan_register_tool_fns(&empty).await.is_empty());
    }

    #[tokio::test]
    async fn enumerate_returns_known_ids() {
        // enumerate() 现在返回 fallback 列表（缓存未填充），
        // 需要测试 enumerate_inner() 来验证完整枚举。
        let tools = enumerate_inner().await.expect("enumerate_inner");
        let ids: Vec<&str> = tools.iter().map(|t| t.id.as_str()).collect();
        assert!(ids.contains(&"read"), "read missing: {ids:?}");
        assert!(ids.contains(&"write"), "write missing: {ids:?}");
        assert!(ids.contains(&"git"), "git alias missing: {ids:?}");
        assert!(ids.contains(&"bash"), "bash alias missing: {ids:?}");
        assert!(tools.iter().any(|t| t.id == "exec" || t.id == "search"),
                "expected exec/search: {ids:?}");
    }
}