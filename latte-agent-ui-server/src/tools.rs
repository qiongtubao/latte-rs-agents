//! 工具管理：发现所有可用工具 + 持久化启用状态。
//!
//! 工具目录的**唯一事实来源**是 `latte_agent_core::tool_docs`（分类表、
//! 别名表、动态工具目录、外部 MCP 目录）。这里只负责把它们汇总成一张
//! UI 能直接渲染的表，外加 tools crate 的 builtin registry 快照。
//!
//! 四类工具（[`latte_agent_core::tool_docs::ToolKind`]）：
//! 1. **builtin** —— `create_tool_manager()` + `register_package(builtin_*)`
//!    拿到的全部 Rust 内置工具，外加 core 单独 register 的 `code_graph`。
//!    描述取 `Tool::description`，即模型随 schema 读到的那一行。
//! 2. **dynamic** —— controller 运行时注册的（`delegate` / `workflow` /
//!    `ask` / 4 个 `doc_*` …），取 core 的 `DYNAMIC_TOOL_CATALOG`。
//! 3. **package_alias** —— 配置层别名（`tools = ["mcp"]`），取 `TOOL_ALIAS_GROUPS`。
//! 4. **mcp** —— 连上外部 MCP server 后发现的工具，取 `tool_docs::mcp_tools()`。
//!    这些在 core 里会被注册成一等工具，所以为它们写的文档同样会下发给模型。
//!
//! 启用状态用 `.latte/tools.yaml`（项目）和 `~/.latte/tools.yaml`（全局）存：
//! 仅记录用户**关闭**的工具（默认全开）。项目优先于全局。
//!
//! 枚举结果缓存由服务启动时的预热线程填充（见 `lib.rs` 的 `spawn`），
//! 预热完成前 HTTP 请求返回硬编码 fallback 列表，保证首次请求也立即响应。
//! MCP 工具是运行时才出现的，所以每次 `enumerate()` 都会在缓存之上叠加
//! 一遍当前 MCP 目录（见 [`enumerate`]）。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use latte_agent_core::tool_docs;

/// 单个工具描述，UI 渲染与后端 lookup 共享。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolEntry {
    /// 工具 ID（注册名，例如 `bash`、`read`、`delegate`、`stub_echo`）。
    /// `.latte/tools.d/<id>.md` 的文件名就是它。
    pub id: String,
    /// UI 展示用的短标签（`code_graph` → `Code Graph`）。
    #[serde(default)]
    pub label: String,
    /// 工具分组：`builtin` | `dynamic` | `package_alias` | `mcp`。
    #[serde(default)]
    pub kind: String,
    /// 完整描述：模型随 schema 读到的那份（不含 `.latte/tools.d` 追加内容）。
    #[serde(default)]
    pub description: String,
    /// 一句话简介（`description` 的首行/首句，见 `tool_docs::split_brief_and_detail`）。
    #[serde(default)]
    pub brief: String,
    /// 是否启用（默认 true）。**仅影响本面板展示**，运行时可用工具由角色配置决定。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 是否已经有项目/全局 md 文档。
    #[serde(default)]
    pub has_doc: bool,
    /// 文档来源：`project` | `global` | `none`。
    #[serde(default)]
    pub doc_source: String,
    /// 注册点：dynamic 工具为函数名，mcp 工具为 server 命令。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registered_by: Option<String>,
}

fn default_true() -> bool { true }

/// `code_graph` → `Code Graph`，`bash` → `Bash`。参考 oh-my-pi 每个工具都带
/// 一个 `label` 供 UI 展示（它那边是手写的，这里按注册名推导，够用且不会漂移）。
pub(crate) fn label_for(id: &str) -> String {
    id.split(['_', '-', '.'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

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

/// 枚举所有可用工具（builtin + dynamic + 别名 + 外部 MCP）。
///
/// 不做 enable/disable 过滤；调用方用 `ToolsStore::is_enabled` 自行判断。
/// builtin/dynamic/别名三类由启动预热线程算好放进缓存（预热完成前返回
/// fallback 列表，保证首个请求毫秒级返回）；**MCP 工具是运行时才出现的**，
/// 所以每次调用都在缓存之上叠一遍当前 MCP 目录。
pub async fn enumerate() -> Result<Vec<ToolEntry>> {
    let cached = ENUMERATE_CACHE
        .read()
        .ok()
        .and_then(|cache| cache.clone())
        .unwrap_or_else(fallback_tools);
    Ok(merge_mcp_tools(cached))
}

/// 把当前 MCP 目录并进工具表。已在表里的 id 不覆盖（内置同名优先）。
fn merge_mcp_tools(mut tools: Vec<ToolEntry>) -> Vec<ToolEntry> {
    let known: BTreeSet<String> = tools.iter().map(|t| t.id.clone()).collect();
    for mcp in tool_docs::mcp_tools() {
        if known.contains(&mcp.name) {
            continue;
        }
        let description = if mcp.description.trim().is_empty() {
            format!("外部 MCP 工具（来自 `{}`）。", mcp.server)
        } else {
            mcp.description.clone()
        };
        tools.push(ToolEntry {
            label: label_for(&mcp.name),
            brief: tool_docs::split_brief_and_detail(&description).0,
            id: mcp.name.clone(),
            kind: tool_docs::ToolKind::Mcp.as_str().into(),
            description,
            enabled: true,
            has_doc: false,
            doc_source: tool_docs::DocSource::Missing.as_str().into(),
            registered_by: Some(mcp.server.clone()),
        });
    }
    tools.sort_by(|a, b| a.id.cmp(&b.id));
    tools
}

/// 给每个条目补上「磁盘上有没有文档」。UI 用它把「已写过文档」标出来，
/// 也让「无文档」的工具能被筛出来集中补齐。
pub fn annotate_doc_state(cwd: &Path, tools: &mut [ToolEntry]) {
    for tool in tools.iter_mut() {
        let doc = tool_docs::resolve_doc(cwd, &tool.id);
        tool.has_doc = !doc.raw.is_empty();
        tool.doc_source = doc.source.as_str().to_string();
    }
}

/// 硬编码的核心工具回退列表（`enumerate()` 预热未完成时使用）。
///
/// 只列最常用的几个，且描述用人话写——预热（毫秒级到秒级）完成后会被
/// `enumerate_inner()` 的真实描述整表替换。
fn fallback_tools() -> Vec<ToolEntry> {
    [
        ("read", "builtin", "读取文件或目录内容"),
        ("write", "builtin", "写入或覆盖文件"),
        ("edit", "builtin", "对文件做精确的替换/插入/删除编辑"),
        ("bash", "builtin", "执行 shell 命令"),
        ("search", "builtin", "搜索文件内容（正则 + glob）"),
        ("grep", "builtin", "AST 结构化代码搜索"),
        ("code_graph", "builtin", "结构化代码导航（按 AST 节点类型查函数/调用点）"),
        ("delegate", "dynamic", "把独立子任务派发给专业角色"),
        ("workflow", "dynamic", "启动多步骤结构化流程"),
    ]
    .into_iter()
    .map(|(id, kind, description)| ToolEntry {
        id: id.into(),
        label: label_for(id),
        kind: kind.into(),
        brief: description.into(),
        description: description.into(),
        enabled: true,
        has_doc: false,
        doc_source: "none".into(),
        registered_by: None,
    })
    .collect()
}

/// 完整枚举，由预热线程调用，结果写入 `ENUMERATE_CACHE`。
pub(crate) async fn enumerate_inner() -> Result<Vec<ToolEntry>> {
    let mut by_id: BTreeMap<String, ToolEntry> = BTreeMap::new();
    let mut push = |id: String, kind: tool_docs::ToolKind, description: String, registered_by: Option<String>| {
        by_id.entry(id.clone()).or_insert(ToolEntry {
            label: label_for(&id),
            brief: tool_docs::split_brief_and_detail(&description).0,
            id,
            kind: kind.as_str().to_string(),
            description,
            enabled: true,
            has_doc: false,
            doc_source: tool_docs::DocSource::Missing.as_str().to_string(),
            registered_by,
        });
    };

    // 1. builtin packages + core 单独注册的 `code_graph`，描述取 core 给出的
    //    **基线描述**——即模型随 schema 真正读到的那一行（含 `read` 的批量
    //    契约改写）。
    //
    //    这里之前自己 `create_tool_manager()` + `register_package`，于是
    //    `read` 显示的是 tools crate 的原始描述（讲 `path` 单目标），而模型
    //    看到的是 `add_batch_read_contract` 改写后的版本（讲 `paths` 批量）
    //    ——面板又一次显示了模型看不到的东西。更早的版本甚至填的是
    //    `"builtin tool: {id}"` 占位串。
    let bases = latte_agent_core::controller::builtin_tool_base_descriptions_at(Path::new("."))
        .await
        .map_err(|e| anyhow!("enumerate builtin tools: {e}"))?;
    for (tool_id, description) in bases {
        let short = tool_id
            .rsplit_once('.')
            .map(|(_, s)| s.to_string())
            .unwrap_or_else(|| tool_id.clone());
        let description = if description.trim().is_empty() {
            format!("builtin tool: {tool_id}")
        } else {
            description
        };
        push(short, tool_docs::ToolKind::Builtin, description, None);
    }

    // 2. controller 运行时注册的动态工具。取 core 导出的权威清单，而不是
    //    文本扫 `register_*_tool` 标识符——后者会漏 `register_doc_graph_tools`
    //    （复数，注册 4 个 doc_* 工具），也会把 `register_request_tool` 误
    //    推成 `request`（真名是 `request_tool`，文档文件名依赖它）。
    for (id, description) in tool_docs::DYNAMIC_TOOL_CATALOG {
        push(
            (*id).to_string(),
            tool_docs::ToolKind::Dynamic,
            (*description).to_string(),
            None,
        );
    }

    // 3. 配置层别名：`agents.toml` 里写 `tools = ["mcp"]` 会展开成组内
    //    多个注册名。别名本身不是模型可见的工具名，但为它写的文档会被
    //    core 的 `tool_docs::resolve_doc` 回退命中、下发给组内每个工具。
    for (alias, members) in tool_docs::TOOL_ALIAS_GROUPS {
        push(
            (*alias).to_string(),
            tool_docs::ToolKind::PackageAlias,
            format!(
                "配置别名（展开为 {}，为它写的文档对组内所有工具生效）",
                members.join(" / ")
            ),
            None,
        );
    }

    Ok(by_id.into_values().collect())
}

/// Scan `<root>/**/*.rs` for `register_*_tool` identifier-shaped tokens.
///
/// 轻量文本扫描：原实现用 latte-rs-graph 的 TreeSitterEngine 全量建图 +
/// SQLite，单次 >60s；本函数与建图返回的标识符集合等价，毫秒级。
///
/// 仅 role-graph 端点使用——它要的是「哪些代码位置注册了工具」这层证据。
/// **不要**再用它推导工具 id：函数名到注册名之间没有可靠映射
/// （`register_doc_graph_tools` 注册 4 个 `doc_*` 工具，
/// `register_request_tool` 的注册名是 `request_tool` 而非 `request`），
/// 工具清单一律走 `latte_agent_core::controller::DYNAMIC_TOOL_CATALOG`。
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// 面板列表必须覆盖运行时真会注册的工具。历史 bug：
    /// - `code_graph` 不在任何 builtin package 里（core 单独 register），面板漏了它；
    /// - 动态工具靠文本扫 `register_*_tool` 推导 id，漏掉
    ///   `register_doc_graph_tools`（复数）注册的 4 个 `doc_*` 工具，
    ///   并把 `register_request_tool` 误推成 `request`（真名 `request_tool`）。
    /// 三者都让已存在的 `.latte/tools.d/<id>.md` 在面板里点不到。
    #[tokio::test]
    async fn enumerate_covers_runtime_registered_tools() {
        let tools = enumerate_inner().await.expect("enumerate_inner");
        let ids: BTreeSet<&str> = tools.iter().map(|t| t.id.as_str()).collect();
        for expected in [
            "code_graph",
            "doc_graph_scan",
            "doc_graph_context",
            "doc_index",
            "doc_write",
            "request_tool",
            "delegate",
            "workflow",
            "ask",
            "plan",
            "task_report",
            "generate_image",
            "playwright",
        ] {
            assert!(ids.contains(expected), "{expected} missing from panel: {ids:?}");
        }
        // `request` 是旧的 `register_*_tool` → id 推导留下的幽灵条目：
        // 运行时没有这个工具名，`request.md` 永远不会被读到。
        assert!(!ids.contains("request"), "phantom `request` entry resurfaced: {ids:?}");
    }

    /// 描述必须是模型 schema 里真读到的那一行，不能是 `builtin tool: x`
    /// 占位串——`get_tool_doc` 在文档没写 `<!-- SUMMARY -->` 时把它当简介
    /// 回退，占位串会一路顶到详情弹层的「简介」区。
    #[tokio::test]
    async fn enumerate_uses_real_tool_descriptions() {
        let tools = enumerate_inner().await.expect("enumerate_inner");
        let placeholders: Vec<&str> = tools
            .iter()
            .filter(|t| t.description.starts_with("builtin tool:")
                || t.description.starts_with("dynamic tool registered via"))
            .map(|t| t.id.as_str())
            .collect();
        assert!(placeholders.is_empty(), "placeholder descriptions: {placeholders:?}");

        let read = tools.iter().find(|t| t.id == "read").expect("read");
        assert!(read.description.len() > "builtin tool: read".len(),
                "read description looks synthetic: {:?}", read.description);
    }

    /// `read` 的描述必须是**运行时改写后**的那份（讲 `paths` 批量入口），
    /// 不是 tools crate 的原始描述（讲 `path` 单目标）。
    ///
    /// 回归：枚举以前自己 `create_tool_manager()` + `register_package`，绕过了
    /// core 的 `add_batch_read_contract`——它会把 `read` 的描述整段换掉。于是
    /// 面板上显示的是一份模型早就看不到的描述，「简介」也跟着错。
    #[tokio::test]
    async fn enumerate_reflects_runtime_description_rewrites() {
        let tools = enumerate_inner().await.expect("enumerate_inner");
        let read = tools.iter().find(|t| t.id == "read").expect("read");
        assert!(
            read.description.contains("paths"),
            "read 的描述没反映批量契约改写: {:?}",
            read.description
        );
        // 简介 = 改写后描述的首句，且首句本身就要讲「能并发批量读」——
        // 模型在选工具那一刻只看得到这一句。
        for keyword in ["并发", "批量", "paths"] {
            let target: &str = if keyword == "paths" { &read.description } else { &read.brief };
            assert!(target.contains(keyword), "缺「{keyword}」: brief={:?}", read.brief);
        }
        assert!(
            read.brief.len() < read.description.len(),
            "简介应该只是首句，不是整段描述: {:?}",
            read.brief
        );
    }
}
