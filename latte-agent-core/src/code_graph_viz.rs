//! code_graph 的 mermaid 可视化：结构图与调用图。
//!
//! - **结构图**（[`structure_mermaid`]）：只读预建索引
//!   `.latte/code_graph/index.json`（定义类符号节点），按文件分组画
//!   flowchart subgraph。快，零外部进程。
//! - **调用图**（[`calls_mermaid`]）：索引提供函数定义跨度（caller 归属），
//!   现场 spawn `ast-grep` 扫 scope 内的调用点（边），按「同文件优先、
//!   scope 内唯一同名次之」解析 callee。绕过 code_graph 工具的
//!   MAX_MATCHES/字符封顶（那是给模型 token 经济的约束，画图要全量）。
//!
//! 两端共用的安全口径：mermaid label 只放行字母数字/下划线/常用分隔符/
//! CJK，id 用递增序号，图源不含任何未转义用户内容（前端 mermaid 以
//! securityLevel "strict" 渲染，双保险）。

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::code_graph_index::CodeGraphIndex;

/// 图规模封顶：再大就不可读，截断并在图源里注明。
const MAX_NODES: usize = 100;
const MAX_EDGES: usize = 150;

/// 一张生成好的 mermaid 图。
#[derive(Debug)]
pub struct MermaidGraph {
    /// mermaid 图源文本（flowchart）。
    pub mermaid: String,
    /// 是否因封顶被截断。
    pub truncated: bool,
}

/// 读取预建索引。索引缺失/版本不符时给可操作的错误提示。
fn load_index(cwd: &Path) -> Result<CodeGraphIndex, String> {
    let path = CodeGraphIndex::path(cwd);
    let raw = std::fs::read_to_string(&path)
        .map_err(|_| "代码索引尚未构建（.latte/code_graph/index.json 不存在）。UI 启动后会后台构建，稍候再试。".to_string())?;
    let idx: CodeGraphIndex = serde_json::from_str(&raw).map_err(|e| {
        format!("代码索引损坏：{e}。删除 .latte/code_graph/index.json 后等后台重建。")
    })?;
    if idx.version != CodeGraphIndex::CURRENT_VERSION {
        return Err("代码索引版本过旧，等后台重建完成后再试。".to_string());
    }
    Ok(idx)
}

/// 查询路径归一化：UI 传入的是相对仓库根的路径；去 `./` 与首尾 `/`，
/// 空串/`.` → 全仓。索引 key 都是相对路径，直接字符串前缀比较。
fn normalize_scope(path: &str) -> String {
    let cleaned = path.trim().trim_start_matches("./").trim_matches('/');
    if cleaned.is_empty() || cleaned == "." {
        ".".to_string()
    } else {
        cleaned.to_string()
    }
}

/// rel（索引里的相对文件路径）是否落在 scope 内。scope 可以是目录或单个文件。
fn in_scope(scope: &str, rel: &str) -> bool {
    scope == "." || rel == scope || rel.starts_with(&format!("{scope}/"))
}

/// mermaid label 清洗：白名单字符，其余折叠成空格，压平连续空白。
fn sanitize_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_alphanumeric() || matches!(c, '_' | '.' | ':' | ' ' | '-' | '#' | '/') {
            out.push(c);
        } else {
            out.push(' ');
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 从签名里粗略抽符号名（与 code_graph_index::name_of 同逻辑，那边是私有的，
/// 按模块自洽惯例复制一份）。
fn name_of(sig: &str) -> String {
    let before_paren = sig.split('(').next().unwrap_or(sig);
    before_paren
        .rsplit(|c: char| !(c.is_alphanumeric() || c == '_'))
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_string()
}

/// kind → 图上的短前缀。
fn kind_tag(kind: &str) -> &str {
    match kind {
        "function" => "fn",
        "method" => "method",
        "struct" => "struct",
        "class" => "class",
        "interface" => "interface",
        "enum" => "enum",
        "trait" => "trait",
        "type" => "type",
        other => other,
    }
}

/// 结构图：索引里的定义类符号按文件分 subgraph。
pub async fn structure_mermaid(cwd: &Path, path: &str) -> Result<MermaidGraph, String> {
    let idx = load_index(cwd)?;
    let scope = normalize_scope(path);

    let mut lines = vec!["flowchart TD".to_string()];
    let mut node_count = 0usize;
    let mut truncated = false;
    let mut sg = 0usize;
    let mut matched_files = 0usize;

    for (rel, entry) in &idx.files {
        if !in_scope(&scope, rel) {
            continue;
        }
        if entry.symbols.is_empty() {
            continue;
        }
        matched_files += 1;
        lines.push(format!("  subgraph sg{sg}[\"📄 {}\"]", sanitize_label(rel)));
        sg += 1;
        for sym in &entry.symbols {
            if node_count >= MAX_NODES {
                truncated = true;
                break;
            }
            let name = name_of(&sym.sig);
            let label = if name.is_empty() {
                sanitize_label(&sym.sig)
            } else {
                format!("{} {}", kind_tag(&sym.kind), sanitize_label(&name))
            };
            // label 为空（极端情况）时用位置兜底，保证节点可读。
            let label = if label.is_empty() {
                format!("L{}", sym.line)
            } else {
                label
            };
            lines.push(format!("    n{node_count}[\"{label}\"]"));
            node_count += 1;
        }
        lines.push("  end".to_string());
        if truncated {
            break;
        }
    }

    if matched_files == 0 {
        return Err(format!(
            "范围 `{path}` 内没有已索引的符号。确认路径相对仓库根（如 `src/` 或 `src/foo.rs`）。"
        ));
    }
    if truncated {
        lines.push(format!(
            "  %% 图被截断：只画了前 {MAX_NODES} 个符号，缩小 path 范围再看"
        ));
    }
    Ok(MermaidGraph {
        mermaid: lines.join("\n"),
        truncated,
    })
}

/// 语言 → call 表达式节点类型（CODE_GRAPH_KIND_TABLE 的 call 行子集，
/// 按模块自洽惯例复制）。
fn call_node_kind(lang: &str) -> Option<&'static str> {
    Some(match lang {
        "c" | "cpp" | "rust" | "go" | "typescript" | "javascript" | "java" => "call_expression",
        "python" => "call",
        _ => return None,
    })
}

/// ast-grep 一条匹配里画图要用的字段。
#[derive(Debug, Deserialize)]
struct AgMatch {
    file: String,
    text: String,
    range: AgRange,
}
#[derive(Debug, Deserialize)]
struct AgRange {
    start: AgPos,
}
#[derive(Debug, Deserialize)]
struct AgPos {
    line: u64,
}

/// 对 scope 内某语言跑一次 call 扫描（全量，无工具侧封顶）。
async fn scan_calls(cwd: &Path, scope: &str, lang: &str) -> Result<Vec<AgMatch>, String> {
    let kind = match call_node_kind(lang) {
        Some(k) => k,
        None => return Ok(Vec::new()),
    };
    let rule = format!("id: cg_viz\nlanguage: {lang}\nrule:\n  kind: {kind}\n");
    let scan_path = if scope == "." { "." } else { scope };
    let out = tokio::process::Command::new("ast-grep")
        .args(["scan", "--inline-rules", &rule, scan_path, "--json=compact"])
        .current_dir(cwd)
        .output()
        .await
        .map_err(|_| "调用图需要 ast-grep（未安装或不在 PATH）。结构图不依赖它。".to_string())?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.trim().is_empty() {
        return Ok(Vec::new());
    }
    Ok(serde_json::from_str::<Vec<AgMatch>>(stdout.trim()).unwrap_or_default())
}

/// caller 候选：一个函数符号的归属跨度。
#[derive(Debug, Clone)]
struct FnSpan {
    name: String,
    line: u64,
    end_line: u64,
}

/// 从调用点文本抽被调标识：`foo(a)` / `self.foo(a)` / `mod::foo(a)`
/// 都取 `foo`；抽不出（不是调用形态）返回 None。
fn callee_name(text: &str) -> Option<String> {
    if !text.contains('(') {
        return None;
    }
    let before_paren = text.split('(').next()?;
    let name = before_paren
        .rsplit(|c: char| !(c.is_alphanumeric() || c == '_'))
        .find(|s| !s.is_empty())?;
    Some(name.to_string())
}

/// 最内层归属：file 的函数跨度表里，包含 line 且跨度最小者。
fn find_caller<'a>(spans: &'a [FnSpan], line: u64) -> Option<&'a FnSpan> {
    spans
        .iter()
        .filter(|s| s.line <= line && line <= s.end_line.max(s.line))
        .min_by_key(|s| s.end_line.saturating_sub(s.line))
}

/// 调用图：scope 内函数之间的调用边。
pub async fn calls_mermaid(cwd: &Path, path: &str) -> Result<MermaidGraph, String> {
    let idx = load_index(cwd)?;
    let scope = normalize_scope(path);

    // caller 跨度表：file → 函数符号（定义类里的可调用体）。
    let mut fns_by_file: BTreeMap<String, Vec<FnSpan>> = BTreeMap::new();
    for (rel, entry) in &idx.files {
        if !in_scope(&scope, rel) {
            continue;
        }
        let spans: Vec<FnSpan> = entry
            .symbols
            .iter()
            .filter(|s| s.kind == "function" || s.kind == "method")
            .map(|s| FnSpan {
                name: name_of(&s.sig),
                line: s.line,
                end_line: s.end_line,
            })
            .filter(|s| !s.name.is_empty())
            .collect();
        if !spans.is_empty() {
            fns_by_file.insert(rel.clone(), spans);
        }
    }
    if fns_by_file.is_empty() {
        return Err(format!(
            "范围 `{path}` 内没有已索引的函数。确认路径相对仓库根（如 `src/` 或 `src/foo.rs`）。"
        ));
    }

    // scope 内出现的语言逐个扫调用点。
    let langs: BTreeSet<String> = idx
        .files
        .iter()
        .filter(|(rel, _)| in_scope(&scope, rel))
        .map(|(_, e)| e.lang.clone())
        .collect();

    // callee 解析表：file → name → ()；name → 全局唯一 file（用于跨文件解析）。
    let mut name_locations: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (rel, spans) in &fns_by_file {
        for s in spans {
            name_locations
                .entry(s.name.clone())
                .or_default()
                .push(rel.clone());
        }
    }

    let mut edges: BTreeSet<(String, String)> = BTreeSet::new();
    let mut truncated = false;
    'lang_loop: for lang in &langs {
        let matches = scan_calls(cwd, &scope, lang).await?;
        for m in matches {
            let file = m.file.trim_start_matches("./").to_string();
            let spans = match fns_by_file.get(&file) {
                Some(s) => s,
                None => continue, // 调用点落在没有函数的索引外文件
            };
            let caller = match find_caller(spans, m.range.start.line + 1) {
                Some(c) => c,
                None => continue, // 文件顶层调用（如懒初始化块），不入图
            };
            let callee = match callee_name(&m.text) {
                Some(c) => c,
                None => continue,
            };
            if callee == caller.name {
                continue; // 自递归边不入图（可读性）
            }
            // callee 归属：同文件同名 > scope 内唯一同名 > 丢弃（外部依赖
            // 如 println!/库函数，入图全是噪声）。
            let callee_file = if fns_by_file
                .get(&file)
                .map(|spans| spans.iter().any(|s| s.name == callee))
                .unwrap_or(false)
            {
                file.clone()
            } else {
                match name_locations.get(&callee) {
                    Some(locs) if locs.len() == 1 => locs[0].clone(),
                    _ => continue,
                }
            };
            edges.insert((
                format!("{file}::{}", caller.name),
                format!("{callee_file}::{callee}"),
            ));
            if edges.len() >= MAX_EDGES {
                truncated = true;
                break 'lang_loop;
            }
        }
    }

    if edges.is_empty() {
        return Err(format!(
            "范围 `{path}` 内没解析出调用边（调用点都不在已知函数体内，或被调函数都在 scope 外）。"
        ));
    }

    // 节点 = 边两端涉及的函数，按文件分 subgraph（与结构图同风格）。
    let mut node_files: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new(); // file → [(key, name)]
    for (a, b) in &edges {
        for key in [a, b] {
            let (file, name) = key.split_once("::").unwrap();
            let nodes = node_files.entry(file.to_string()).or_default();
            if !nodes.iter().any(|(k, _)| k == key) {
                nodes.push((key.clone(), name.to_string()));
            }
        }
    }

    let mut lines = vec!["flowchart LR".to_string()];
    let mut ids: BTreeMap<String, String> = BTreeMap::new();
    let mut node_count = 0usize;
    let mut sg = 0usize;
    for (file, nodes) in &node_files {
        if node_count + nodes.len() > MAX_NODES {
            truncated = true;
            break;
        }
        lines.push(format!(
            "  subgraph sg{sg}[\"📄 {}\"]",
            sanitize_label(file)
        ));
        sg += 1;
        for (key, name) in nodes {
            let id = format!("n{node_count}");
            node_count += 1;
            lines.push(format!("    {id}[\"fn {}\"]", sanitize_label(name)));
            ids.insert(key.clone(), id);
        }
        lines.push("  end".to_string());
    }
    for (a, b) in &edges {
        if let (Some(ia), Some(ib)) = (ids.get(a), ids.get(b)) {
            lines.push(format!("  {ia} --> {ib}"));
        }
    }
    if truncated {
        lines.push(format!(
            "  %% 图被截断：上限 {MAX_EDGES} 边 / {MAX_NODES} 节点，缩小 path 范围再看"
        ));
    }
    Ok(MermaidGraph {
        mermaid: lines.join("\n"),
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_index(dir: &Path, json: &str) {
        let p = CodeGraphIndex::path(dir);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, json).unwrap();
    }

    const IDX: &str = r#"{
        "version": 2,
        "built_at": 9999999999,
        "lang_counts": {"rust": 1},
        "files": {
            "src/a.rs": {"lang": "rust", "mtime": 1, "symbols": [
                {"kind": "function", "sig": "fn alpha()", "line": 1, "end_line": 10},
                {"kind": "function", "sig": "fn beta()", "line": 12, "end_line": 20},
                {"kind": "struct", "sig": "struct Thing", "line": 22, "end_line": 25}
            ]},
            "src/sub/b.rs": {"lang": "rust", "mtime": 1, "symbols": [
                {"kind": "function", "sig": "pub fn gamma(x: i32)", "line": 1, "end_line": 8}
            ]}
        }
    }"#;

    #[tokio::test]
    async fn structure_groups_symbols_by_file() {
        let dir = tempfile::tempdir().unwrap();
        write_index(dir.path(), IDX);
        let g = structure_mermaid(dir.path(), "src").await.unwrap();
        assert!(g.mermaid.starts_with("flowchart TD"));
        assert!(g.mermaid.contains("subgraph sg0[\"📄 src/a.rs\"]"));
        assert!(g.mermaid.contains("fn alpha"));
        assert!(g.mermaid.contains("struct Thing"));
        assert!(!g.truncated);
    }

    #[tokio::test]
    async fn structure_scope_file_and_dir() {
        let dir = tempfile::tempdir().unwrap();
        write_index(dir.path(), IDX);
        // 单文件 scope
        let g = structure_mermaid(dir.path(), "src/sub/b.rs").await.unwrap();
        assert!(g.mermaid.contains("fn gamma"));
        assert!(!g.mermaid.contains("fn alpha"));
        // 无命中范围 → 明确报错
        assert!(structure_mermaid(dir.path(), "nope/").await.is_err());
    }

    #[tokio::test]
    async fn structure_missing_index_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = structure_mermaid(dir.path(), ".").await.unwrap_err();
        assert!(err.contains("索引尚未构建"));
    }

    #[test]
    fn callee_name_extracts_last_ident() {
        assert_eq!(callee_name("foo(a, b)").as_deref(), Some("foo"));
        assert_eq!(callee_name("self.run(x)").as_deref(), Some("run"));
        assert_eq!(callee_name("crate::mod::deep(x)").as_deref(), Some("deep"));
        assert_eq!(callee_name("x = 1"), None);
    }

    #[test]
    fn find_caller_picks_innermost_span() {
        let spans = vec![
            FnSpan {
                name: "outer".into(),
                line: 1,
                end_line: 100,
            },
            FnSpan {
                name: "inner".into(),
                line: 10,
                end_line: 20,
            },
        ];
        assert_eq!(find_caller(&spans, 15).unwrap().name, "inner");
        assert_eq!(find_caller(&spans, 50).unwrap().name, "outer");
        assert!(find_caller(&spans, 200).is_none());
    }

    #[test]
    fn sanitize_label_strips_mermaid_breakers() {
        assert_eq!(sanitize_label("foo[\"bar\"]\nbaz"), "foo bar baz");
        assert_eq!(sanitize_label("src/lib.rs"), "src/lib.rs");
    }

    /// 真实 ast-grep 调用图 e2e：ast-grep 不在 PATH 时跳过（与
    /// controller 的 code_graph e2e 同门禁）。
    #[tokio::test]
    async fn calls_mermaid_real_ast_grep() {
        if tokio::process::Command::new("ast-grep")
            .arg("--version")
            .output()
            .await
            .is_err()
        {
            eprintln!("ast-grep not installed; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "fn helper() -> i32 { 1 }\nfn main_entry() { let _ = helper(); }\n",
        )
        .unwrap();
        write_index(
            dir.path(),
            r#"{
            "version": 2, "built_at": 9999999999, "lang_counts": {"rust": 1},
            "files": {"src/lib.rs": {"lang": "rust", "mtime": 1, "symbols": [
                {"kind": "function", "sig": "fn helper()", "line": 1, "end_line": 1},
                {"kind": "function", "sig": "fn main_entry()", "line": 2, "end_line": 2}
            ]}}
        }"#,
        );
        let g = calls_mermaid(dir.path(), "src").await.unwrap();
        assert!(g.mermaid.starts_with("flowchart LR"), "{}", g.mermaid);
        assert!(g.mermaid.contains("fn helper"), "{}", g.mermaid);
        assert!(g.mermaid.contains("fn main_entry"), "{}", g.mermaid);
        assert!(g.mermaid.contains("-->"), "{}", g.mermaid);
    }
}
