//! 工具的**模型侧说明**：分类、磁盘文档、模板渲染、基线描述、活跃
//! `ToolManager` 热更新，以及外部 MCP 工具目录。
//!
//! # 为什么单独一个模块
//!
//! 「模型看到的工具说明」原来散在三处各写一份：`controller` 里拼接文档、
//! UI server 里另造一套枚举、`api.rs` 里再手抄一遍工具名清单。任何一处漂移
//! 都表现为「面板上看得到、模型读不到」或者反过来。本模块是唯一事实来源，
//! `controller` 与 UI server 都从这里取数据。
//!
//! # 一份说明的组成
//!
//! ```text
//! 模型看到的 tool.description
//!   = 基线描述（Tool::builder 里写的，编译进代码）
//!   + "\n\n"
//!   + 项目/全局 md（.latte/tools.d/<id>.md，模板渲染后）
//! ```
//!
//! 约定与 oh-my-pi 的 `prompts/tools/<tool>.md` 一致：**首行是一句话说明
//! 用途，其后才是 `<instruction>` / `<critical>` 之类细则**；md 里可以用
//! `{{#if hasEval}}…{{/if}}` 按当前会话真实可用的工具改写内容。
//!
//! # 分类（[`ToolKind`]）
//!
//! - `builtin` —— tools crate 的 builtin package + core 单独 register 的 `code_graph`；
//! - `dynamic` —— controller 运行时注册的（`delegate` / `workflow` / `ask` …），
//!   见 [`DYNAMIC_TOOL_CATALOG`]；
//! - `package_alias` —— 配置层别名（`tools = ["mcp"]`），见 [`TOOL_ALIAS_GROUPS`]；
//! - `mcp` —— 连上外部 MCP server 后发现的工具，见 [`mcp_tools`]。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, Weak};

use latte_rs_agent_tools::types::ToolManager;

// ─────────────────────────── 分类与目录 ───────────────────────────

/// 工具分类。序列化成 UI 直接用的短字符串。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    /// tools crate 的 builtin package，或 core 单独 register 的内置工具。
    Builtin,
    /// controller 运行时注册（handler 捕获了 runner / 模型客户端 / 工作目录）。
    Dynamic,
    /// 配置层别名，展开成组内多个注册名。
    PackageAlias,
    /// 外部 MCP server 发现的工具。
    Mcp,
}

impl ToolKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolKind::Builtin => "builtin",
            ToolKind::Dynamic => "dynamic",
            ToolKind::PackageAlias => "package_alias",
            ToolKind::Mcp => "mcp",
        }
    }
}

/// 每个工具的**一句话简介**上限（按 char 计）。超过就按句末标点断句。
pub const BRIEF_MAX_CHARS: usize = 60;

/// controller 运行时注册的工具，`(注册名, 一句话简介)`。
///
/// 这些没法在没有活跃会话的情况下枚举出来（handler 捕获了 runner、模型
/// 客户端、工作目录），所以只需要「列出来」的调用方（UI 工具面板、角色
/// 编辑器的工具选择器）从这张表取。注册本身仍在各 `register_*` 函数里，
/// 两边要一起改；**注册名必须逐字一致**——`.latte/tools.d/<name>.md` 的
/// 文件名就是按它查的。
///
/// `delegate` / `workflow` 的运行时描述是按会话生成的（内嵌当前角色与
/// workflow 清单），这里给的是稳定摘要而非运行时原文。
pub const DYNAMIC_TOOL_CATALOG: &[(&str, &str)] = &[
    ("ask", "向用户抛出一道选择题并弹出选择框（支持单选/多选、配图、图片网格、自定义上传）。仅用于需要用户拍板的取舍。"),
    ("delegate", "把独立子任务派发给专业角色（programmer / architect / reviewer …），完成后综合核对结果。运行时描述附带当前可用角色清单。"),
    ("doc_graph_context", "基于查询预读 doc-graph 知识图谱中相关文档块，返回 overview + index + pages。"),
    ("doc_graph_scan", "重建 doc-graph 知识图谱索引（扫描 .latte-review/docs/ 下所有 .md，更新 graph.json）。"),
    ("doc_index", "生成 doc-graph 文档索引（docs/index.md 目录表）。"),
    ("doc_write", "创建/更新一篇 doc-graph 文档：写 frontmatter + 正文到 .latte-review/docs/，重建图索引。"),
    ("generate_image", "生成一张图片（OpenAI 兼容图片接口），保存到 .latte/images/ 并在聊天中显示。"),
    ("plan", "把一份结构化任务清单提交给用户，用户勾选后导入任务看板（backlog）。"),
    ("request_tool", "向当前角色申请临时使用某个不在其可用工具列表里的工具，申请后自动批准。"),
    ("task_report", "向任务看板回报任务执行结果，把任务推进到 human_review 状态。"),
    ("workflow", "启动多步骤结构化流程（实施计划 / 测试开发 / 代码审查 …）。运行时描述附带当前可用 workflow 清单。"),
];

/// 配置层工具别名：角色写 `tools = ["mcp"]` 时展开成组内这些注册名。
///
/// 文档按**注册名**查找，所以为别名写的 md 需要 [`alias_of`] 回退才能生效
/// （`mcp.md` → `mcp_connect` / `mcp_list` / `mcp_call`）。
pub const TOOL_ALIAS_GROUPS: &[(&str, &[&str])] = &[
    ("git", &["git_status", "git_diff", "git_log", "git_add", "git_commit", "git_branch"]),
    ("mcp", &["mcp_connect", "mcp_list", "mcp_call"]),
    ("playwright", &["playwright_script", "screenshot"]),
];

/// 某个注册名所属的别名组（没有则 `None`）。
pub fn alias_of(name: &str) -> Option<&'static str> {
    TOOL_ALIAS_GROUPS
        .iter()
        .find(|(_, members)| members.contains(&name))
        .map(|(alias, _)| *alias)
}

/// 把一份说明拆成 `(一句话简介, 详情)`。
///
/// 首行优先（oh-my-pi 的 md 约定）；latte 的内置描述有不少写成一整行，
/// 首行仍超过 [`BRIEF_MAX_CHARS`] 时再按第一个句末标点断一次。整段找不到
/// 断点就全当简介，不做硬截断——半句话的简介比长简介更没用。
pub fn split_brief_and_detail(text: &str) -> (String, String) {
    let text = text.trim();
    if text.is_empty() {
        return (String::new(), String::new());
    }
    match text.split_once('\n') {
        Some((first, rest)) => {
            let first = first.trim();
            let rest = rest.trim();
            if first.chars().count() <= BRIEF_MAX_CHARS {
                return (first.to_string(), rest.to_string());
            }
            let (brief, tail) = split_first_sentence(first);
            let detail = match (tail.is_empty(), rest.is_empty()) {
                (true, _) => rest.to_string(),
                (false, true) => tail,
                (false, false) => format!("{tail}\n{rest}"),
            };
            (brief, detail)
        }
        None => split_first_sentence(text),
    }
}

/// 在第一个句末标点（中英文句号/问号/叹号/分号）之后断开。英文点号要求
/// 后面紧跟空白，避免在 `v1.2`、`e.g.` 这类地方断错。
fn split_first_sentence(text: &str) -> (String, String) {
    let mut split_at = None;
    for (idx, ch) in text.char_indices() {
        if matches!(ch, '。' | '！' | '？' | '；' | '.' | '!' | '?' | ';') {
            let end = idx + ch.len_utf8();
            if matches!(ch, '.' | '!' | '?' | ';')
                && !text[end..].is_empty()
                && !text[end..].starts_with(char::is_whitespace)
            {
                continue;
            }
            split_at = Some(end);
            break;
        }
    }
    match split_at {
        Some(end) => (text[..end].trim().to_string(), text[end..].trim().to_string()),
        None => (text.to_string(), String::new()),
    }
}

// ─────────────────────────── 磁盘文档 ───────────────────────────

/// 文档来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocSource {
    /// 项目 `<cwd>/.latte/tools.d/<id>.md`。
    Project,
    /// 全局 `$LATTE_HOME/tools.d/<id>.md`（默认 `~/.latte/tools.d`）。
    Global,
    /// 两层都没有。
    Missing,
}

impl DocSource {
    pub fn as_str(self) -> &'static str {
        match self {
            DocSource::Project => "project",
            DocSource::Global => "global",
            DocSource::Missing => "none",
        }
    }
}

/// 一次文档解析的结果。
#[derive(Debug, Clone)]
pub struct ResolvedDoc {
    /// md 原文（未渲染模板、未剥 SUMMARY/DETAILS 标记）。缺省为空串。
    pub raw: String,
    pub source: DocSource,
    /// 命中的文件路径；`Missing` 时给出**将要写入**的项目路径。
    pub path: PathBuf,
    /// 实际命中的文档 id：可能是别名（`mcp_call` 命中 `mcp.md` 时为 `mcp`）。
    pub matched_id: String,
}

/// 工具文档的项目相对路径。运行时读的、UI 写的都必须是这个路径。
pub fn doc_rel_path(id: &str) -> String {
    format!(".latte/tools.d/{id}.md")
}

/// 全局 `tools.d` 目录（`$LATTE_HOME/tools.d`，回退 `~/.latte/tools.d`）。
pub fn global_docs_dir() -> Option<PathBuf> {
    crate::global_config::GlobalConfig::global_dir().map(|dir| dir.join("tools.d"))
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

/// 解析工具文档：项目 → 全局 → 别名组（项目 → 全局）→ 无。
///
/// 别名回退让「按配置里写的名字」写的文档也能生效：角色配置里写
/// `tools = ["mcp"]`，用户自然把文档存成 `mcp.md`，而注册名是
/// `mcp_connect` / `mcp_list` / `mcp_call`。
pub fn resolve_doc(cwd: &Path, name: &str) -> ResolvedDoc {
    let mut candidates = vec![name.to_string()];
    if let Some(alias) = alias_of(name) {
        candidates.push(alias.to_string());
    }
    for id in &candidates {
        let project = cwd.join(doc_rel_path(id));
        if let Some(raw) = read_trimmed(&project) {
            return ResolvedDoc { raw, source: DocSource::Project, path: project, matched_id: id.clone() };
        }
        if let Some(global) = global_docs_dir().map(|dir| dir.join(format!("{id}.md"))) {
            if let Some(raw) = read_trimmed(&global) {
                return ResolvedDoc { raw, source: DocSource::Global, path: global, matched_id: id.clone() };
            }
        }
    }
    ResolvedDoc {
        raw: String::new(),
        source: DocSource::Missing,
        path: cwd.join(doc_rel_path(name)),
        matched_id: name.to_string(),
    }
}

/// 文档所在的配置层。与 models / roles 面板的分层语义一致：项目层覆盖全局层。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocLayer {
    Project,
    Global,
}

impl DocLayer {
    pub fn as_str(self) -> &'static str {
        match self {
            DocLayer::Project => "project",
            DocLayer::Global => "global",
        }
    }

    /// 解析 UI 传来的 target 字段；空串按项目层（与 models 的 `update_model` 一致）。
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "project" | "" => Some(DocLayer::Project),
            "global" => Some(DocLayer::Global),
            _ => None,
        }
    }
}

/// 某个工具在某一层的文档落点。`exists` 为假时 `path` 是**将要写入**的路径。
#[derive(Debug, Clone)]
pub struct DocLayerState {
    pub layer: DocLayer,
    pub exists: bool,
    pub path: PathBuf,
    /// 命中的文档 id：走别名回退时与查询的 id 不同（`mcp_call` → `mcp`）。
    pub matched_id: String,
}

/// 某一层里该工具（或其别名组）文档的路径 + 是否存在。
///
/// 与 [`resolve_doc`] 用同一套候选顺序（自身 id → 别名），这样 UI 显示的
/// 「哪层有文档」与运行时真正读的那份永远对得上。
pub fn doc_layer_state(cwd: &Path, id: &str, layer: DocLayer) -> DocLayerState {
    let mut candidates = vec![id.to_string()];
    if let Some(alias) = alias_of(id) {
        candidates.push(alias.to_string());
    }
    let dir = match layer {
        DocLayer::Project => Some(cwd.join(".latte").join("tools.d")),
        DocLayer::Global => global_docs_dir(),
    };
    if let Some(dir) = dir.as_ref() {
        for candidate in &candidates {
            let path = dir.join(format!("{candidate}.md"));
            if read_trimmed(&path).is_some() {
                return DocLayerState {
                    layer,
                    exists: true,
                    path,
                    matched_id: candidate.clone(),
                };
            }
        }
    }
    DocLayerState {
        layer,
        exists: false,
        // 全局目录解析不出来时（没有 HOME）给个相对路径占位，仅用于展示。
        path: dir
            .unwrap_or_else(|| PathBuf::from("tools.d"))
            .join(format!("{id}.md")),
        matched_id: id.to_string(),
    }
}

/// 两层的文档状态，项目层在前（覆盖顺序）。
pub fn doc_layers(cwd: &Path, id: &str) -> Vec<DocLayerState> {
    vec![
        doc_layer_state(cwd, id, DocLayer::Project),
        doc_layer_state(cwd, id, DocLayer::Global),
    ]
}

/// 写工具文档到指定层：项目 `<cwd>/.latte/tools.d/<id>.md` 或全局
/// `$LATTE_HOME/tools.d/<id>.md`（自动建目录）。
///
/// 与 models 的「保存到项目 / 保存到全局」同一套语义：目标层由调用方给定，
/// 不做隐式改写。写全局时若解析不到全局目录（无 `HOME`/`LATTE_HOME`）报错，
/// 而不是悄悄落到项目层——那会让用户以为改了全局其实没有。
pub fn write_doc_in(cwd: &Path, id: &str, layer: DocLayer, text: &str) -> std::io::Result<PathBuf> {
    let path = match layer {
        DocLayer::Project => cwd.join(doc_rel_path(id)),
        DocLayer::Global => global_docs_dir()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "无法定位全局配置目录（$LATTE_HOME / $HOME 都没有）",
                )
            })?
            .join(format!("{id}.md")),
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, text)?;
    Ok(path)
}

/// 删除指定层的工具文档。文件本来就不存在时按成功处理（幂等）。
/// 返回被删（或本就不存在）的路径。
pub fn remove_doc_in(cwd: &Path, id: &str, layer: DocLayer) -> std::io::Result<PathBuf> {
    let path = match layer {
        DocLayer::Project => cwd.join(doc_rel_path(id)),
        DocLayer::Global => global_docs_dir()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "无法定位全局配置目录（$LATTE_HOME / $HOME 都没有）",
                )
            })?
            .join(format!("{id}.md")),
    };
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(path),
        Err(e) => Err(e),
    }
}

/// 写工具文档到项目层（[`write_doc_in`] 的项目层快捷方式）。
pub fn write_doc(cwd: &Path, id: &str, text: &str) -> std::io::Result<PathBuf> {
    write_doc_in(cwd, id, DocLayer::Project, text)
}

/// 剥掉 `<!-- SUMMARY -->` / `<!-- DETAILS -->` 标记，拼成模型侧纯文本。
/// 无标记的文档原样返回。
pub fn strip_doc_markers(raw: &str) -> String {
    let (summary, content) = split_doc_markers(raw);
    match (summary.trim().is_empty(), content.trim().is_empty()) {
        (true, true) => raw.trim().to_string(),
        (true, false) => content.trim().to_string(),
        (false, true) => summary.trim().to_string(),
        (false, false) => format!("{}\n\n{}", summary.trim(), content.trim()),
    }
}

/// 取出 `SUMMARY` / `DETAILS` 标记内的两段；缺标记的那段为空串。
pub fn split_doc_markers(raw: &str) -> (String, String) {
    let between = |start: &str, end: &str| -> String {
        raw.find(start)
            .and_then(|s| {
                let body = s + start.len();
                raw[body..].find(end).map(|e| (body, body + e))
            })
            .map(|(begin, end_idx)| raw[begin..end_idx].to_string())
            .unwrap_or_default()
    };
    (
        between("<!-- SUMMARY -->", "<!-- /SUMMARY -->"),
        between("<!-- DETAILS -->", "<!-- /DETAILS -->"),
    )
}

/// 把简介 + 详情合并成带标记的 md（[`split_doc_markers`] 的逆运算）。
pub fn compose_doc(summary: &str, content: &str) -> String {
    let mut out = String::new();
    if !summary.trim().is_empty() {
        out.push_str("<!-- SUMMARY -->\n");
        out.push_str(summary.trim());
        out.push_str("\n<!-- /SUMMARY -->\n\n");
    }
    if !content.trim().is_empty() {
        out.push_str("<!-- DETAILS -->\n");
        out.push_str(content.trim());
        out.push_str("\n<!-- /DETAILS -->\n");
    }
    out
}

// ─────────────────────────── 模板渲染 ───────────────────────────

/// 模板上下文：变量（`{{CWD}}`）+ 布尔开关（`{{#if hasEval}}`）。
///
/// 键名大小写与下划线都不敏感（`hasEval` / `HAS_EVAL` / `has_eval` 等价），
/// 手写模板不该因为风格差异静默失效。
#[derive(Debug, Clone, Default)]
pub struct TemplateContext {
    vars: BTreeMap<String, String>,
    flags: BTreeMap<String, bool>,
}

fn norm_key(key: &str) -> String {
    key.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

impl TemplateContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_var(&mut self, key: &str, value: impl Into<String>) -> &mut Self {
        self.vars.insert(norm_key(key), value.into());
        self
    }

    pub fn set_flag(&mut self, key: &str, value: bool) -> &mut Self {
        self.flags.insert(norm_key(key), value);
        self
    }

    pub fn var(&self, key: &str) -> &str {
        self.vars.get(&norm_key(key)).map(String::as_str).unwrap_or("")
    }

    /// 未定义的开关按 `false`：模板写错工具名时走 `{{else}}` 分支，
    /// 而不是把两个分支都塞给模型。
    pub fn flag(&self, key: &str) -> bool {
        self.flags.get(&norm_key(key)).copied().unwrap_or(false)
    }

    /// 已定义的变量名（原始下划线风格，供 UI 提示可用变量）。
    pub fn var_names(&self) -> Vec<String> {
        self.vars.keys().cloned().collect()
    }

    /// 已定义的开关名。
    pub fn flag_names(&self) -> Vec<String> {
        self.flags.keys().cloned().collect()
    }

    /// 按「当前会话实际注册了哪些工具」构造上下文。
    ///
    /// 提供：
    /// - 开关 `has_<工具名>`（如 `has_eval` / `has_code_graph`）+ `has_mcp_tools`；
    /// - 开关 `is_windows` / `is_macos` / `is_linux`；
    /// - 变量 `cwd`、`os`、`tools`（逗号分隔清单）、`tool_count`。
    pub fn from_tool_names(cwd: &Path, names: &[String]) -> Self {
        let mut ctx = Self::new();
        for name in names {
            ctx.set_flag(&format!("has_{name}"), true);
        }
        for (alias, members) in TOOL_ALIAS_GROUPS {
            let any = members.iter().any(|m| names.iter().any(|n| n == m));
            ctx.set_flag(&format!("has_{alias}"), any);
        }
        ctx.set_flag("has_mcp_tools", !mcp_tools().is_empty());
        ctx.set_flag("is_windows", cfg!(target_os = "windows"));
        ctx.set_flag("is_macos", cfg!(target_os = "macos"));
        ctx.set_flag("is_linux", cfg!(target_os = "linux"));
        let mut sorted: Vec<&str> = names.iter().map(String::as_str).collect();
        sorted.sort_unstable();
        ctx.set_var("cwd", cwd.display().to_string());
        ctx.set_var("os", std::env::consts::OS);
        ctx.set_var("tools", sorted.join(", "));
        ctx.set_var("tool_count", names.len().to_string());
        ctx
    }
}

/// 文档里是否用到了模板语法（UI 据此提示「这份文档是模板」）。
pub fn uses_template_syntax(raw: &str) -> bool {
    raw.contains("{{")
}

enum Stop {
    Eof,
    Else,
    End,
}

/// 渲染工具文档模板。
///
/// 支持 `{{VAR}}`、`{{#if COND}}…{{else}}…{{/if}}`、
/// `{{#unless COND}}…{{else}}…{{/unless}}`，可嵌套。
/// 未知变量渲染成空串、未知开关按 `false`；未闭合的块按「到文末」处理，
/// 不 panic、不吞掉整份文档。
pub fn render_template(raw: &str, ctx: &TemplateContext) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut pos = 0usize;
    let _ = render_seq(raw, &mut pos, ctx, &mut out);
    out
}

fn render_seq(src: &str, pos: &mut usize, ctx: &TemplateContext, out: &mut String) -> Stop {
    loop {
        let Some(rel) = src[*pos..].find("{{") else {
            out.push_str(&src[*pos..]);
            *pos = src.len();
            return Stop::Eof;
        };
        let open = *pos + rel;
        out.push_str(&src[*pos..open]);
        let Some(close_rel) = src[open + 2..].find("}}") else {
            // 没有闭合的 `}}`：剩下的全按字面量输出。
            out.push_str(&src[open..]);
            *pos = src.len();
            return Stop::Eof;
        };
        let close = open + 2 + close_rel;
        let tag = src[open + 2..close].trim();
        *pos = close + 2;

        if let Some(cond) = tag.strip_prefix("#if ") {
            let taken = ctx.flag(cond.trim());
            render_block(src, pos, ctx, out, taken);
        } else if let Some(cond) = tag.strip_prefix("#unless ") {
            let taken = !ctx.flag(cond.trim());
            render_block(src, pos, ctx, out, taken);
        } else if tag == "else" {
            return Stop::Else;
        } else if tag == "/if" || tag == "/unless" {
            return Stop::End;
        } else {
            out.push_str(ctx.var(tag));
        }
    }
}

/// 渲染 `{{#if}}` / `{{#unless}}` 的两个分支，按 `taken` 选一个写出。
/// 未选中的分支照样解析（才能正确找到配对的 `{{/if}}`），只是丢弃结果。
fn render_block(src: &str, pos: &mut usize, ctx: &TemplateContext, out: &mut String, taken: bool) {
    let mut primary = String::new();
    let stop = render_seq(src, pos, ctx, &mut primary);
    let mut alternate = String::new();
    if matches!(stop, Stop::Else) {
        let _ = render_seq(src, pos, ctx, &mut alternate);
    }
    out.push_str(if taken { &primary } else { &alternate });
}

// ─────────────────────── 基线描述 + 有效描述 ───────────────────────

/// `注册名 → 基线描述`。基线 = `Tool::builder` 里写死的那份（含
/// `read` 的批量契约改写），**不含**任何 md 追加内容。
///
/// 有了它，「重新 enrich」就是幂等的纯函数 `基线 + 当前文档`：文档改过、
/// 甚至删掉之后再 enrich 也能得到正确结果。原来靠
/// `if !description.contains(doc)` 判重，文档一改就会把新旧两份都追加上去。
static BASE_DESCRIPTIONS: RwLock<Option<BTreeMap<String, String>>> = RwLock::new(None);

/// 取工具的基线描述；第一次见到某个工具时把 `current` 记下来。
pub fn base_description(name: &str, current: &str) -> String {
    if let Ok(guard) = BASE_DESCRIPTIONS.read() {
        if let Some(map) = guard.as_ref() {
            if let Some(base) = map.get(name) {
                return base.clone();
            }
        }
    }
    if let Ok(mut guard) = BASE_DESCRIPTIONS.write() {
        let map = guard.get_or_insert_with(BTreeMap::new);
        return map
            .entry(name.to_string())
            .or_insert_with(|| current.to_string())
            .clone();
    }
    current.to_string()
}

/// 清空基线描述表（测试用；生产路径不需要）。
pub fn clear_base_descriptions() {
    if let Ok(mut guard) = BASE_DESCRIPTIONS.write() {
        *guard = None;
    }
}

/// 组装模型最终看到的描述：`基线 + 渲染后的 md`（md 缺省时就是基线）。
///
/// md 的简介与基线首句**逐字相同时会被丢掉**：面板的编辑框会用当前显示的
/// 简介预填（否则点「编辑」看到空框，没法接着改），用户只改详情、原样保存
/// 简介是常态——不去重的话模型会连着读到两遍同一句话。
pub fn effective_description(base: &str, cwd: &Path, name: &str, ctx: &TemplateContext) -> String {
    let doc = resolve_doc(cwd, name);
    if doc.raw.is_empty() {
        return base.to_string();
    }
    let mut per_tool = ctx.clone();
    per_tool.set_var("tool", name);
    per_tool.set_var("doc_path", doc.path.display().to_string());
    let rendered = render_template(&doc.raw, &per_tool);
    let (doc_summary, doc_detail) = split_doc_markers(&rendered);
    let base_brief = split_brief_and_detail(base).0;
    let rendered = if !doc_summary.trim().is_empty() && doc_summary.trim() == base_brief.trim() {
        // 简介与基线首句重复 → 只下发详情部分。
        doc_detail.trim().to_string()
    } else {
        strip_doc_markers(&rendered)
    };
    let rendered = rendered.trim();
    if rendered.is_empty() {
        return base.to_string();
    }
    if base.trim().is_empty() {
        return rendered.to_string();
    }
    format!("{}\n\n{}", base.trim(), rendered)
}

// ─────────────────── 活跃 ToolManager 热更新 ───────────────────

/// 活跃 `ToolManager` 的弱引用表：`(工作目录, manager)`。
///
/// 文档在 UI 里改完就能生效，不必等下一个 session —— PUT 之后
/// [`for_each_live`] 会把每个还活着的 manager 重新 enrich 一遍。弱引用
/// 保证会话结束后条目自然失效，不会拖着 manager 不放。
static LIVE_MANAGERS: RwLock<Option<Vec<(PathBuf, Weak<dyn ToolManager>)>>> = RwLock::new(None);

/// 登记一个活跃 manager（`build_tool_manager_at` 里调用）。
pub fn register_live_manager(cwd: &Path, tm: &Arc<dyn ToolManager>) {
    if let Ok(mut guard) = LIVE_MANAGERS.write() {
        let list = guard.get_or_insert_with(Vec::new);
        list.retain(|(_, weak)| weak.strong_count() > 0);
        list.push((cwd.to_path_buf(), Arc::downgrade(tm)));
    }
}

/// 对每个还活着的 manager 执行 `f`，返回处理了多少个。顺带清理死引用。
pub fn for_each_live(mut f: impl FnMut(&Path, &Arc<dyn ToolManager>)) -> usize {
    let snapshot: Vec<(PathBuf, Arc<dyn ToolManager>)> = match LIVE_MANAGERS.write() {
        Ok(mut guard) => {
            let list = guard.get_or_insert_with(Vec::new);
            list.retain(|(_, weak)| weak.strong_count() > 0);
            list.iter()
                .filter_map(|(cwd, weak)| weak.upgrade().map(|tm| (cwd.clone(), tm)))
                .collect()
        }
        Err(_) => return 0,
    };
    for (cwd, tm) in &snapshot {
        f(cwd, tm);
    }
    snapshot.len()
}

/// 清空登记表（测试用）。
pub fn clear_live_managers() {
    if let Ok(mut guard) = LIVE_MANAGERS.write() {
        *guard = None;
    }
}

// ─────────────────────── 外部 MCP 工具目录 ───────────────────────

/// 一个来自外部 MCP server 的工具。
#[derive(Debug, Clone, serde::Serialize)]
pub struct McpTool {
    /// 注册名。与 MCP server 报的 `name` 一致（除非重名，见 [`record_mcp_tools`]）。
    pub name: String,
    /// MCP server 报的 description（可能为空）。
    pub description: String,
    /// 启动该 server 的命令，作为来源标识。
    pub server: String,
    /// server 在连接列表里的下标（`mcp_call` 的 `server_index`）。
    pub server_index: usize,
    /// MCP server 报的 `inputSchema`（原始 JSON Schema）。
    pub input_schema: serde_json::Value,
}

static MCP_CATALOG: RwLock<Option<Vec<McpTool>>> = RwLock::new(None);

/// 记录一次 `mcp_connect` 发现的工具。
///
/// **同一个 server 重连时先清掉它上一批工具**，否则重连一次就多出一份
/// `foo@1` / `foo@2`——工具面板会越连越脏，模型的 schema 里也会出现一堆
/// 只有下标不同的重名工具。
///
/// 跨 server 撞名则保留先到的那个，后到的带上 server 下标后缀（`read@1`）：
/// MCP 工具名会进模型 schema，撞上内置工具名（`read` / `write` …）会让路由
/// 变得不可预测。
pub fn record_mcp_tools(server: &str, server_index: usize, tools: Vec<McpTool>) -> Vec<McpTool> {
    let mut recorded = Vec::new();
    if let Ok(mut guard) = MCP_CATALOG.write() {
        let list = guard.get_or_insert_with(Vec::new);
        list.retain(|existing| existing.server != server);
        for mut tool in tools {
            tool.server = server.to_string();
            tool.server_index = server_index;
            if list.iter().any(|existing| existing.name == tool.name) {
                tool.name = format!("{}@{}", tool.name, server_index);
            }
            if list.iter().any(|existing| existing.name == tool.name) {
                continue;
            }
            list.push(tool.clone());
            recorded.push(tool);
        }
    }
    recorded
}

/// 当前已知的全部外部 MCP 工具。
pub fn mcp_tools() -> Vec<McpTool> {
    MCP_CATALOG
        .read()
        .ok()
        .and_then(|guard| guard.clone())
        .unwrap_or_default()
}

/// 忘掉某个 server 的工具（断开连接时用）。返回删掉的条数。
pub fn forget_mcp_server(server: &str) -> usize {
    if let Ok(mut guard) = MCP_CATALOG.write() {
        if let Some(list) = guard.as_mut() {
            let before = list.len();
            list.retain(|tool| tool.server != server);
            return before - list.len();
        }
    }
    0
}

/// 清空 MCP 目录（测试用）。
pub fn clear_mcp_catalog() {
    if let Ok(mut guard) = MCP_CATALOG.write() {
        *guard = None;
    }
}

/// 从 `mcp_connect` 的返回值里解析工具清单。
///
/// 形状（见 tools crate 的 `mcp_connect_tool`）：
/// `{"server": "<cmd>", "tools": [{"name":…, "description":…, "inputSchema":…}], …}`
/// 任何字段缺失都不致命——名字拿不到的条目直接跳过。
pub fn parse_mcp_connect_result(result: &serde_json::Value) -> Vec<McpTool> {
    result
        .get("tools")
        .and_then(|tools| tools.as_array())
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    let name = tool.get("name").and_then(|n| n.as_str())?.trim();
                    if name.is_empty() {
                        return None;
                    }
                    Some(McpTool {
                        name: name.to_string(),
                        description: tool
                            .get("description")
                            .and_then(|d| d.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        server: String::new(),
                        server_index: 0,
                        input_schema: tool
                            .get("inputSchema")
                            .or_else(|| tool.get("input_schema"))
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 已连接的 MCP server 列表（去重、保序）。
pub fn mcp_servers() -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for tool in mcp_tools() {
        if seen.insert(tool.server.clone()) {
            out.push(tool.server);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `LATTE_HOME` 是进程级环境变量，碰它的用例必须串行——而且必须和
    /// crate 里**其它**改这个变量的用例（如 `controller::tests::tool_doc_*`）
    /// 共用同一把锁，各自一把等于没锁。
    /// MCP 目录同样是进程级全局，碰它的用例也必须串行。
    fn mcp_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_util::ENV_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn brief_split_prefers_first_line_then_first_sentence() {
        let (b, d) = split_brief_and_detail("Reads files.\n\n<instruction>\n- x\n</instruction>");
        assert_eq!(b, "Reads files.");
        assert!(d.starts_with("<instruction>"));

        let (b, d) = split_brief_and_detail(
            "读取文件内容。支持行范围选择器：path:start-end、path:start+count、path:raw；不带行范围时回传结构摘要。",
        );
        assert_eq!(b, "读取文件内容。");
        assert!(d.starts_with("支持行范围选择器"));

        assert_eq!(split_brief_and_detail("列出目录内容"), ("列出目录内容".into(), String::new()));
        assert_eq!(
            split_brief_and_detail("Runs pi v1.2 scripts"),
            ("Runs pi v1.2 scripts".into(), String::new())
        );
        assert_eq!(split_brief_and_detail("  "), (String::new(), String::new()));
    }

    #[test]
    fn alias_lookup_is_reverse_of_groups() {
        assert_eq!(alias_of("mcp_call"), Some("mcp"));
        assert_eq!(alias_of("screenshot"), Some("playwright"));
        assert_eq!(alias_of("git_status"), Some("git"));
        assert_eq!(alias_of("read"), None);
    }

    #[test]
    fn template_substitutes_vars_and_branches() {
        let mut ctx = TemplateContext::new();
        ctx.set_var("cwd", "/tmp/x").set_flag("has_eval", true).set_flag("has_browser", false);

        assert_eq!(render_template("cwd={{CWD}}", &ctx), "cwd=/tmp/x");
        // 键名大小写/下划线不敏感
        assert_eq!(render_template("{{ cwd }}|{{Cwd}}", &ctx), "/tmp/x|/tmp/x");
        // 未知变量 → 空串
        assert_eq!(render_template("[{{nope}}]", &ctx), "[]");

        assert_eq!(render_template("{{#if hasEval}}yes{{else}}no{{/if}}", &ctx), "yes");
        assert_eq!(render_template("{{#if has_browser}}yes{{else}}no{{/if}}", &ctx), "no");
        // 未知开关按 false
        assert_eq!(render_template("{{#if hasNothing}}y{{else}}n{{/if}}", &ctx), "n");
        assert_eq!(render_template("{{#unless has_browser}}fallback{{/unless}}", &ctx), "fallback");
        // 嵌套
        assert_eq!(
            render_template("{{#if hasEval}}A{{#unless has_browser}}B{{/unless}}C{{/if}}", &ctx),
            "ABC"
        );
        // 未选中的分支不能泄漏到输出里
        assert_eq!(
            render_template("{{#if has_browser}}NO{{#if hasEval}}NEITHER{{/if}}{{else}}OK{{/if}}", &ctx),
            "OK"
        );
    }

    #[test]
    fn template_tolerates_malformed_input() {
        let ctx = TemplateContext::new();
        // 没闭合的 `}}` → 原样保留，不吞内容
        assert_eq!(render_template("a {{oops b", &ctx), "a {{oops b");
        // 没闭合的块 → 按到文末处理（条件为假时整段丢弃）
        assert_eq!(render_template("x{{#if nope}}tail", &ctx), "x");
        // 孤立的 `{{/if}}` 不能 panic
        assert_eq!(render_template("a{{/if}}b", &ctx), "a");
        assert!(uses_template_syntax("{{#if x}}"));
        assert!(!uses_template_syntax("plain text"));
    }

    #[test]
    fn context_from_tool_names_exposes_flags_and_vars() {
        let ctx = TemplateContext::from_tool_names(Path::new("/w"), &[
            "read".to_string(),
            "mcp_call".to_string(),
        ]);
        assert!(ctx.flag("has_read"));
        assert!(ctx.flag("hasRead"));
        assert!(!ctx.flag("has_eval"));
        // 别名开关跟着组内成员一起亮
        assert!(ctx.flag("has_mcp"));
        assert!(!ctx.flag("has_playwright"));
        assert_eq!(ctx.var("cwd"), "/w");
        assert_eq!(ctx.var("tool_count"), "2");
        assert_eq!(ctx.var("tools"), "mcp_call, read");
        assert!(ctx.flag_names().iter().any(|n| n == "hasread"));
    }

    #[test]
    fn doc_markers_round_trip() {
        let composed = compose_doc("简介一句话", "详情正文");
        let (s, c) = split_doc_markers(&composed);
        assert_eq!(s.trim(), "简介一句话");
        assert_eq!(c.trim(), "详情正文");
        // 模型侧看到的是剥掉标记后的纯文本
        assert_eq!(strip_doc_markers(&composed), "简介一句话\n\n详情正文");
        // 无标记文档原样返回
        assert_eq!(strip_doc_markers("plain doc"), "plain doc");
        // 只有简介
        assert_eq!(strip_doc_markers(&compose_doc("只有简介", "")), "只有简介");
    }

    #[test]
    fn resolve_doc_prefers_project_then_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        let dir = cwd.join(".latte/tools.d");
        std::fs::create_dir_all(&dir).unwrap();

        // 别名文档：mcp_call 自己没有 md 时命中 mcp.md
        std::fs::write(dir.join("mcp.md"), "mcp alias doc").unwrap();
        let doc = resolve_doc(cwd, "mcp_call");
        assert_eq!(doc.raw, "mcp alias doc");
        assert_eq!(doc.matched_id, "mcp");
        assert_eq!(doc.source, DocSource::Project);

        // 工具自己的 md 优先
        std::fs::write(dir.join("mcp_call.md"), "own doc").unwrap();
        let doc = resolve_doc(cwd, "mcp_call");
        assert_eq!(doc.raw, "own doc");
        assert_eq!(doc.matched_id, "mcp_call");

        // 全都没有 → Missing，path 指向将要写入的项目路径
        let doc = resolve_doc(cwd, "no_such_tool");
        assert_eq!(doc.source, DocSource::Missing);
        assert!(doc.raw.is_empty());
        assert!(doc.path.ends_with(".latte/tools.d/no_such_tool.md"));
    }

    /// 基线描述必须只记第一次：文档改了之后重新 enrich 得到
    /// `基线 + 新文档`，而不是 `基线 + 旧文档 + 新文档`。
    #[test]
    fn effective_description_rebuilds_from_base() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        let dir = cwd.join(".latte/tools.d");
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = TemplateContext::new();

        assert_eq!(effective_description("base", cwd, "zz_probe", &ctx), "base");

        std::fs::write(dir.join("zz_probe.md"), "v1 doc").unwrap();
        assert_eq!(effective_description("base", cwd, "zz_probe", &ctx), "base\n\nv1 doc");

        std::fs::write(dir.join("zz_probe.md"), "v2 doc").unwrap();
        let again = effective_description("base", cwd, "zz_probe", &ctx);
        assert_eq!(again, "base\n\nv2 doc");
        assert!(!again.contains("v1 doc"), "旧文档残留：{again}");

        std::fs::remove_file(dir.join("zz_probe.md")).unwrap();
        assert_eq!(effective_description("base", cwd, "zz_probe", &ctx), "base");
    }

    /// md 的简介与基线首句逐字相同时不重复下发（面板会用当前简介预填编辑框，
    /// 用户只改详情、原样保存简介是常态）；不同就照常拼上去。
    #[test]
    fn effective_description_drops_summary_identical_to_base_brief() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        let dir = cwd.join(".latte/tools.d");
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = TemplateContext::new();
        let base = "读取文件或目录，一次调用可并发批量读取。剩下的都是细则。";

        // 简介 == 基线首句 → 只下发详情
        std::fs::write(
            dir.join("zz_dup.md"),
            compose_doc("读取文件或目录，一次调用可并发批量读取。", "只看详情这段"),
        )
        .unwrap();
        let text = effective_description(base, cwd, "zz_dup", &ctx);
        assert!(text.contains("只看详情这段"), "{text}");
        assert_eq!(
            text.matches("一次调用可并发批量读取。").count(),
            1,
            "同一句话被下发了两遍: {text}"
        );

        // 简介被改过 → 两段都要
        std::fs::write(
            dir.join("zz_dup.md"),
            compose_doc("换了一句自己的简介。", "只看详情这段"),
        )
        .unwrap();
        let text = effective_description(base, cwd, "zz_dup", &ctx);
        assert!(text.contains("换了一句自己的简介。"), "{text}");
        assert!(text.contains("只看详情这段"), "{text}");

        // 只有重复简介、没写详情 → 退回纯基线，不留一段空白
        std::fs::write(
            dir.join("zz_dup.md"),
            compose_doc("读取文件或目录，一次调用可并发批量读取。", ""),
        )
        .unwrap();
        assert_eq!(effective_description(base, cwd, "zz_dup", &ctx), base);
    }

    /// 文档里的模板要按当前会话渲染，`{{TOOL}}` 拿到工具自己的名字。
    #[test]
    fn effective_description_renders_template() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        let dir = cwd.join(".latte/tools.d");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("zz_tpl.md"),
            "tool={{TOOL}}\n{{#if has_eval}}use eval{{else}}no eval{{/if}}",
        )
        .unwrap();

        let ctx = TemplateContext::from_tool_names(cwd, &["zz_tpl".into()]);
        let text = effective_description("base", cwd, "zz_tpl", &ctx);
        assert!(text.contains("tool=zz_tpl"), "{text}");
        assert!(text.contains("no eval") && !text.contains("use eval"), "{text}");

        let ctx = TemplateContext::from_tool_names(cwd, &["zz_tpl".into(), "eval".into()]);
        let text = effective_description("base", cwd, "zz_tpl", &ctx);
        assert!(text.contains("use eval") && !text.contains("no eval"), "{text}");
    }

    /// 分层落点：项目层优先、全局层次之，与 models / roles 面板同一套语义。
    /// `exists=false` 时 `path` 是**将要写入**的路径（UI 拿它显示落点）。
    #[test]
    fn doc_layers_report_both_layers_and_write_targets() {
        let _guard = env_lock();
        let project = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("LATTE_HOME");
        std::env::set_var("LATTE_HOME", global.path());
        let cwd = project.path();

        // 两层都没有
        let layers = doc_layers(cwd, "zz_layer");
        assert_eq!(layers.len(), 2);
        assert!(!layers[0].exists && !layers[1].exists);
        assert_eq!(layers[0].layer, DocLayer::Project);
        assert!(layers[0].path.ends_with(".latte/tools.d/zz_layer.md"));
        assert_eq!(layers[1].layer, DocLayer::Global);
        assert!(layers[1].path.starts_with(global.path()), "{:?}", layers[1].path);

        // 只写全局：全局命中，运行时也读全局
        let written = write_doc_in(cwd, "zz_layer", DocLayer::Global, "global body").unwrap();
        assert!(written.starts_with(global.path()));
        let layers = doc_layers(cwd, "zz_layer");
        assert!(!layers[0].exists && layers[1].exists);
        assert_eq!(resolve_doc(cwd, "zz_layer").source, DocSource::Global);

        // 再写项目：项目层覆盖，全局层仍在
        write_doc_in(cwd, "zz_layer", DocLayer::Project, "project body").unwrap();
        let layers = doc_layers(cwd, "zz_layer");
        assert!(layers[0].exists && layers[1].exists);
        let doc = resolve_doc(cwd, "zz_layer");
        assert_eq!(doc.source, DocSource::Project);
        assert_eq!(doc.raw, "project body");

        // 删项目层 → 回落到全局；删全局层 → 什么都不剩。删两次不报错（幂等）
        remove_doc_in(cwd, "zz_layer", DocLayer::Project).unwrap();
        assert_eq!(resolve_doc(cwd, "zz_layer").raw, "global body");
        remove_doc_in(cwd, "zz_layer", DocLayer::Global).unwrap();
        remove_doc_in(cwd, "zz_layer", DocLayer::Global).unwrap();
        assert_eq!(resolve_doc(cwd, "zz_layer").source, DocSource::Missing);

        // 别名组文档在哪层，layers 就要在哪层报命中
        write_doc_in(cwd, "mcp", DocLayer::Global, "alias body").unwrap();
        let layers = doc_layers(cwd, "mcp_call");
        assert!(!layers[0].exists, "项目层不该命中");
        assert!(layers[1].exists && layers[1].matched_id == "mcp", "{:?}", layers[1]);

        assert_eq!(DocLayer::parse("project"), Some(DocLayer::Project));
        assert_eq!(DocLayer::parse(""), Some(DocLayer::Project));
        assert_eq!(DocLayer::parse("global"), Some(DocLayer::Global));
        assert_eq!(DocLayer::parse("nope"), None);

        match previous {
            Some(value) => std::env::set_var("LATTE_HOME", value),
            None => std::env::remove_var("LATTE_HOME"),
        }
    }

    #[test]
    fn mcp_catalog_records_parses_and_dedups() {
        let _serial = mcp_lock();
        clear_mcp_catalog();
        let result = serde_json::json!({
            "server": "npx demo-server",
            "tools": [
                {"name": "search_docs", "description": "Search the docs", "inputSchema": {"type": "object"}},
                {"name": "", "description": "nameless is skipped"},
                {"name": "read", "description": "collides with a builtin name"},
            ],
            "status": "connected",
        });
        let parsed = parse_mcp_connect_result(&result);
        assert_eq!(parsed.len(), 2, "空名字条目必须被跳过: {parsed:?}");

        let recorded = record_mcp_tools("npx demo-server", 0, parsed);
        assert_eq!(recorded.len(), 2);
        let names: Vec<&str> = recorded.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["search_docs", "read"]);
        assert_eq!(recorded[0].server, "npx demo-server");
        assert_eq!(mcp_servers(), vec!["npx demo-server".to_string()]);

        // 第二个 server 报同名工具 → 带 server 下标后缀，不覆盖第一个
        let second = record_mcp_tools(
            "npx other",
            1,
            parse_mcp_connect_result(&serde_json::json!({"tools":[{"name":"search_docs"}]})),
        );
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].name, "search_docs@1");
        assert_eq!(mcp_tools().len(), 3);

        assert_eq!(forget_mcp_server("npx demo-server"), 2);
        assert_eq!(mcp_tools().len(), 1);
        clear_mcp_catalog();
    }

    /// 同一个 server 重连要**替换**它上一批工具，而不是累积出 `foo@1` / `foo@2`。
    #[test]
    fn reconnecting_same_server_replaces_its_tools() {
        let _serial = mcp_lock();
        clear_mcp_catalog();
        let payload = |names: &[&str]| {
            serde_json::json!({
                "tools": names.iter().map(|n| serde_json::json!({"name": n})).collect::<Vec<_>>()
            })
        };

        record_mcp_tools("srv-a", 0, parse_mcp_connect_result(&payload(&["alpha", "beta"])));
        assert_eq!(mcp_tools().len(), 2);

        // 重连同一个 server，这次它只报一个工具 → 目录里就该只剩这一个
        let again = record_mcp_tools("srv-a", 0, parse_mcp_connect_result(&payload(&["alpha"])));
        assert_eq!(again.len(), 1);
        let names: Vec<String> = mcp_tools().into_iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["alpha".to_string()], "重连后不该残留旧工具/后缀副本");

        // 另一个 server 报同名工具，仍然按下标加后缀区分
        record_mcp_tools("srv-b", 1, parse_mcp_connect_result(&payload(&["alpha"])));
        let names: Vec<String> = mcp_tools().into_iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["alpha".to_string(), "alpha@1".to_string()]);
        clear_mcp_catalog();
    }
}
