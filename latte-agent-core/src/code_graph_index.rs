//! Prebuilt code-graph symbol index.
//!
//! latte 的 `code_graph` 工具本身是**无索引**的 ast-grep 实时扫描：每次
//! 调用现场 spawn `ast-grep` 扫源码。对大仓库第一次「建
//! 地图」要冷扫全仓，几百毫秒~数秒。本模块在 **UI 启动时**后台预扫一遍
//! cwd 仓库的定义类符号（function/struct/class/type/enum/trait/interface），
//! 把 `文件 → 符号签名 + 行号` 落盘成 `.latte/code_graph/index.json`，让
//! 后续「先建地图」这类查询可以直接读索引、秒回，而不必冷扫。
//!
//! 设计要点：
//! - **只索引定义类符号**：call/import/decl 数量爆炸且不是「地图」需要的，
//!   排除在外（工具仍可现场 ast-grep 查这些）。
//! - **gitignore 感知**：直接用 `ast-grep` 自己的目录扫描（它遵守
//!   .gitignore），不自己 walk 文件。
//! - **增量**：记录上次构建时间 `built_at`；若仓库里所有源文件的 mtime
//!   都不晚于 `built_at`，直接跳过重建（no-op）。
//! - **失败即降级**：没装 ast-grep 或扫描出错 → 不写索引、不报致命错，
//!   工具照常走实时 ast-grep 路径。
//!
//! 索引**不是**工具正确性的前提，只是一个加速缓存；删掉 index.json 不影
//! 响任何功能。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 索引里收录的「定义类」语义 kind —— 构成一张代码地图的骨架。
/// 刻意排除 call/import/decl（数量大、非地图语义）。
const INDEXED_KINDS: &[&str] = &[
    "function",
    "method",
    "struct",
    "class",
    "type",
    "enum",
    "trait",
    "interface",
];

/// 扩展名 → ast-grep 语言名。与 `controller::code_graph_lang_from_ext`
/// 保持一致；这里复制一份是为了模块自洽、不跨模块暴露私有函数。
fn lang_from_ext(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "rs" => "rust",
        "go" => "go",
        "py" | "pyi" => "python",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "java" => "java",
        _ => return None,
    })
}

/// (语言, 语义 kind) → tree-sitter 节点类型。与 controller 的
/// CODE_GRAPH_KIND_TABLE 的定义类子集保持一致。
fn node_kinds(lang: &str, kind: &str) -> &'static [&'static str] {
    match (lang, kind) {
        ("c", "function") => &["function_definition"],
        ("c", "struct") => &["struct_specifier"],
        ("c", "type") => &["type_definition"],
        ("cpp", "function") => &["function_definition"],
        ("cpp", "struct") => &["struct_specifier"],
        ("cpp", "class") => &["class_specifier"],
        ("rust", "function") => &["function_item"],
        ("rust", "struct") => &["struct_item"],
        ("rust", "enum") => &["enum_item"],
        ("rust", "trait") => &["trait_item"],
        ("go", "function") => &["function_declaration", "method_declaration"],
        ("go", "method") => &["method_declaration"],
        ("go", "type") => &["type_declaration"],
        ("go", "struct") => &["struct_type"],
        ("go", "interface") => &["interface_type"],
        ("python", "function") => &["function_definition"],
        ("python", "class") => &["class_definition"],
        ("typescript", "function") => &["function_declaration", "method_definition"],
        ("typescript", "method") => &["method_definition"],
        ("typescript", "class") => &["class_declaration"],
        ("typescript", "interface") => &["interface_declaration"],
        ("javascript", "function") => &["function_declaration", "method_definition"],
        ("javascript", "method") => &["method_definition"],
        ("javascript", "class") => &["class_declaration"],
        ("java", "function") => &["method_declaration"],
        ("java", "method") => &["method_declaration"],
        ("java", "class") => &["class_declaration"],
        ("java", "interface") => &["interface_declaration"],
        _ => &[],
    }
}

/// 一条已索引的符号。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Symbol {
    pub kind: String,
    /// 单行签名（去掉函数体、参数压平）。
    pub sig: String,
    /// 1-based 起始行号。
    pub line: u64,
    /// 1-based 结束行号（定义体的最后一行，含）。
    ///
    /// 有它才能把「查到的函数」直接变成「读得到的函数」：`read` 的行范围
    /// 选择器要的是 `start-end`，只给 start 等于让模型自己猜函数有多长。
    /// 实测事故（task_planner）：code_graph 只回 `:3935`，模型猜了
    /// `3874-3950` 去读 `code_graph_build_rule`，正好把 match 的
    /// `Some(name)` 分支切掉，它只能回一句"body 部分被截断了"然后放弃。
    ///
    /// `default` 让旧索引（v1，无此字段）还能读；为 0 时视为未知，
    /// 回退成只报起始行。索引 version 已随之升到 2，正常会自动重建。
    #[serde(default)]
    pub end_line: u64,
}

impl Symbol {
    /// 模型侧的位置锚点：知道跨度时给 `start-end`（可直接粘进 `read`），
    /// 否则退回单个起始行。
    pub fn span(&self) -> String {
        if self.end_line > self.line {
            format!("{}-{}", self.line, self.end_line)
        } else {
            self.line.to_string()
        }
    }
}

/// 单文件的索引条目。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FileEntry {
    /// 该文件的语言。
    pub lang: String,
    /// 文件 mtime（unix 秒），用于增量判断。
    pub mtime: u64,
    pub symbols: Vec<Symbol>,
}

/// 落盘的索引结构。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CodeGraphIndex {
    pub version: u32,
    /// 构建完成时间（unix 秒）。
    pub built_at: u64,
    /// 出现过的语言 → 文件数，供快速概览。
    pub lang_counts: BTreeMap<String, usize>,
    /// 相对路径 → 文件条目。
    pub files: BTreeMap<String, FileEntry>,
}

impl CodeGraphIndex {
    /// v2 起每个 Symbol 带 `end_line`（定义体跨度）。v1 索引缺该字段，
    /// 只能报起始行，所以升版本强制重建而不是将就着用。
    pub const CURRENT_VERSION: u32 = 2;

    /// 索引文件路径：`<cwd>/.latte/code_graph/index.json`。
    pub fn path(cwd: &Path) -> PathBuf {
        cwd.join(".latte").join("code_graph").join("index.json")
    }

    /// 总符号数。
    pub fn symbol_count(&self) -> usize {
        self.files.values().map(|f| f.symbols.len()).sum()
    }
}

/// 把工具传入的 query `path` 归一化成「索引 key（相对仓库根）」的可比形式。
///
/// 索引里的文件 key 都是**相对仓库根**的路径（ast-grep 以 `current_dir(cwd)`
/// 运行，回传相对路径）。但模型经常传**绝对路径**（如
/// `/Users/.../repo/src/main.c`），旧实现只 `trim_start_matches("./")`，
/// 绝对路径原样带进 scope，永远匹配不上相对 key → 静默返回 0 命中，且因为
/// 「空清单也算命中」而**不会回退**到实时 ast-grep，等于 code_graph 对所有
/// 绝对路径查询完全失效。
///
/// 归一化规则：
/// - 先解析出仓库根：`cwd` 能 canonicalize 就用其绝对形态，否则用进程 cwd。
/// - `path` 若是绝对路径且落在仓库根下 → strip 掉根前缀，得到相对路径。
/// - 若绝对但不在仓库根下 → 返回 None（该 path 不属于本仓库，索引必然无它，
///   交给调用方回退实时扫描而不是谎报 0 命中）。
/// - 相对路径 → 去掉前导 `./` 与首尾 `/`。
/// - 空串或 `.` → `"."`（全仓范围）。
fn normalize_scope(cwd: &Path, path: &str) -> Option<String> {
    let p = Path::new(path);
    let rel_str = if p.is_absolute() {
        // 仓库根：优先 canonicalize（吃掉 `.`、软链、`..`），失败则退回原样。
        let root = std::fs::canonicalize(cwd)
            .or_else(|_| std::env::current_dir())
            .unwrap_or_else(|_| cwd.to_path_buf());
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        // 同样 canonicalize query path（文件可能不存在时 canonicalize 会失败，
        // 退回按字符串前缀 strip）。
        let abs = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        match abs.strip_prefix(&root) {
            Ok(rel) => rel.to_string_lossy().to_string(),
            Err(_) => return None, // 不在仓库根下 → 让调用方回退实时扫描
        }
    } else {
        path.trim_start_matches("./").to_string()
    };
    let cleaned = rel_str.trim_start_matches("./").trim_matches('/');
    if cleaned.is_empty() {
        Some(".".to_string())
    } else {
        Some(cleaned.to_string())
    }
}

/// 索引新鲜度判定：索引存在、版本匹配、且 `path` 覆盖到的文件都没在
/// 索引构建后被改动过。用于工具 fast-path 决定「能否信任索引」。
fn index_is_fresh_for(cwd: &Path, idx: &CodeGraphIndex, scope: &str) -> bool {
    if idx.version != CodeGraphIndex::CURRENT_VERSION {
        return false;
    }
    // 只校验落在 query scope 范围内的文件。scope="." 时校验全部。
    for (rel, _entry) in &idx.files {
        if scope != "." && !scope.is_empty() && !rel.starts_with(scope) {
            continue;
        }
        let mt = mtime_unix(&cwd.join(rel));
        if mt == 0 || mt > idx.built_at {
            return false; // 文件消失或被改 → 索引对这个范围不新鲜
        }
    }
    true
}

/// 工具 fast-path：从预建索引里查某个定义 kind 的签名清单。
///
/// 命中条件（由调用方保证）：kind 是定义类、无裸 pattern、mode=signatures。
/// 返回 `Some((lines, total))` 表示命中索引；`None` 表示索引不可用/不新鲜/
/// 该 path 无 lang 信息，调用方应回退到实时 ast-grep。
///
/// - `cwd`：进程工作目录（索引与源码相对它）。
/// - `path`：查询路径（文件或目录，`.` 表示全仓）。
/// - `kind`：定义类语义 kind。
/// - `name`：可选，按符号名子串（大小写不敏感）过滤。
/// - `max_matches` / `max_chars`：输出封顶，与工具保持一致。
pub fn query_signatures(
    cwd: &Path,
    path: &str,
    kind: &str,
    name: Option<&str>,
    max_matches: usize,
    max_chars: usize,
) -> Option<(Vec<String>, usize)> {
    // 只服务定义类 kind；call/import/decl 不进索引。
    if !INDEXED_KINDS.contains(&kind) {
        return None;
    }
    // 把 query path 归一化成索引 key（相对仓库根）的可比形式。绝对路径、
    // `./` 前缀、末尾 `/` 都在此吃掉；不在本仓库下的绝对路径返回 None，
    // 让调用方回退实时扫描而不是谎报 0 命中。
    let scope = normalize_scope(cwd, path)?;
    let idx_path = CodeGraphIndex::path(cwd);
    let idx: CodeGraphIndex = serde_json::from_str(&std::fs::read_to_string(idx_path).ok()?).ok()?;
    if !index_is_fresh_for(cwd, &idx, &scope) {
        return None;
    }
    let name_lc = name.map(|n| n.to_ascii_lowercase());

    // 收集命中：遍历范围内文件的目标 kind 符号。
    // 元组第 2 项是排序键（起始行），第 3 项是模型侧锚点（`start-end` 跨度）。
    let mut hits: Vec<(String, u64, String, String)> = Vec::new(); // (file, line, span, sig)
    for (rel, entry) in &idx.files {
        // 路径范围过滤：scope 是文件时精确匹配，是目录时前缀匹配。
        if scope != "." && !scope.is_empty() && !(rel == &scope || rel.starts_with(&format!("{scope}/"))) {
            continue;
        }
        for s in &entry.symbols {
            if s.kind != kind {
                continue;
            }
            if let Some(ref nlc) = name_lc {
                if !s.sig.to_ascii_lowercase().contains(nlc.as_str()) {
                    continue;
                }
            }
            hits.push((rel.clone(), s.line, s.span(), s.sig.clone()));
        }
    }
    // 索引里该范围/kind 一个都没有：可能是索引确实没有（也可能范围不含源码）。
    // 返回空清单也算命中（避免无谓回退再冷扫一遍得到同样的空结果）。
    hits.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    let total = hits.len();
    let mut lines = Vec::new();
    let mut chars = 0usize;
    for (file, _line, span, sig) in hits.into_iter().take(max_matches) {
        let entry = format!("{file}:{span}: {sig}");
        if chars + entry.len() > max_chars {
            break;
        }
        chars += entry.len();
        lines.push(entry);
    }
    Some((lines, total))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn mtime_unix(p: &Path) -> u64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// ast-grep 是否可用。
async fn ast_grep_available() -> bool {
    tokio::process::Command::new("ast-grep")
        .arg("--version")
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 单行签名：取匹配文本的第一「行」，砍掉函数体（`{` 起）与多余空白。
/// 与工具里 `code_graph_signature_of` 的意图一致（这里做保守实现）。
fn signature_of(text: &str) -> String {
    // 有定义体（`{`）时签名到 `{` 为止（可能跨多行的参数列表）；
    // 没有 `{`（如 C 结构体前置声明、type alias）时到第一个换行为止。
    let cut = match text.find('{') {
        Some(b) => b,
        None => text.find('\n').unwrap_or(text.len()),
    };
    let head = &text[..cut];
    // 参数里的换行压平成单空格。
    let flat: String = head.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = flat.trim_end_matches([';', ':']).trim();
    trimmed.to_string()
}

/// 从签名里粗略抽符号名（用于 `name` 过滤/展示）。抽不出就返回空串。
fn name_of(sig: &str) -> String {
    // 找第一个 `(` 之前的最后一个标识符（函数/方法）；否则取最后一个词。
    let before_paren = sig.split('(').next().unwrap_or(sig);
    let ident = before_paren
        .rsplit(|c: char| !(c.is_alphanumeric() || c == '_'))
        .find(|s| !s.is_empty())
        .unwrap_or("");
    ident.to_string()
}

/// ast-grep 一条 JSON 匹配里我们关心的字段。
#[derive(Deserialize)]
struct AgMatch {
    file: String,
    text: String,
    range: AgRange,
}
#[derive(Deserialize)]
struct AgRange {
    start: AgPos,
    end: AgPos,
}
#[derive(Deserialize)]
struct AgPos {
    line: u64,
}

/// 对某语言的某个 kind 跑一次 ast-grep scan，返回匹配列表。
async fn scan_kind(cwd: &Path, lang: &str, kind: &str) -> Vec<AgMatch> {
    let kinds = node_kinds(lang, kind);
    if kinds.is_empty() {
        return Vec::new();
    }
    // inline YAML 规则：单/多节点类型。
    let rule = if kinds.len() == 1 {
        format!("id: cg_idx\nlanguage: {lang}\nrule:\n  kind: {}\n", kinds[0])
    } else {
        let mut any = String::from("  any:\n");
        for k in kinds {
            any.push_str(&format!("  - kind: {k}\n"));
        }
        format!("id: cg_idx\nlanguage: {lang}\nrule:\n{any}")
    };
    let out = tokio::process::Command::new("ast-grep")
        .args(["scan", "--inline-rules", &rule, ".", "--json=compact"])
        .current_dir(cwd)
        .output()
        .await;
    let out = match out {
        Ok(o) if o.status.success() || !o.stdout.is_empty() => o,
        _ => return Vec::new(),
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.trim().is_empty() {
        return Vec::new();
    }
    serde_json::from_str::<Vec<AgMatch>>(stdout.trim()).unwrap_or_default()
}

/// 结果：构建做了什么。
#[derive(Debug, Clone, PartialEq)]
pub enum BuildOutcome {
    /// ast-grep 不可用，跳过。
    AstGrepMissing,
    /// 仓库自上次构建后无改动，跳过重建。
    UpToDate,
    /// 完成一次（重）构建。
    Built {
        files: usize,
        symbols: usize,
    },
}

/// 判断是否需要重建：任一源文件 mtime 晚于 `built_at`，或索引缺失/版本旧。
/// 用 ast-grep 的文件清单来枚举「源文件」，避免自己 walk + 处理 gitignore。
async fn needs_rebuild(cwd: &Path, existing: &Option<CodeGraphIndex>) -> bool {
    let idx = match existing {
        Some(i) if i.version == CodeGraphIndex::CURRENT_VERSION => i,
        _ => return true, // 无索引或版本不符 → 建
    };
    // 已知文件有任何一个 mtime 变新（或消失）→ 重建。
    //
    // 只看「索引里记录过的文件」，不做「全仓文件数对比」：后者需要重跑一次
    // 全量 scan（等于把增量的意义抵消掉），而且「含函数的文件数」和「含任意
    // 定义的文件数」基准不同，会导致每次都误判为需要重建。新增文件的收录
    // 放宽到「下一次真实改动触发重建」或重启时兜底——单个 UI 会话期间仓库
    // 凭空多出源文件的场景很少，代价可接受。
    for (rel, _entry) in &idx.files {
        let mt = mtime_unix(&cwd.join(rel));
        if mt == 0 {
            return true; // 文件消失
        }
        if mt > idx.built_at {
            return true; // 被改动
        }
    }
    false
}

/// 构建或增量更新索引。非致命：任何异常都返回 Ok 的降级 outcome / 保底不 panic。
pub async fn build_or_update(cwd: &Path) -> BuildOutcome {
    if !ast_grep_available().await {
        return BuildOutcome::AstGrepMissing;
    }
    let index_path = CodeGraphIndex::path(cwd);
    let existing: Option<CodeGraphIndex> = std::fs::read_to_string(&index_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());

    if !needs_rebuild(cwd, &existing).await {
        return BuildOutcome::UpToDate;
    }

    let mut files: BTreeMap<String, FileEntry> = BTreeMap::new();
    let mut lang_counts: BTreeMap<String, usize> = BTreeMap::new();

    for lang in [
        "c", "cpp", "rust", "go", "python", "typescript", "javascript", "java",
    ] {
        for kind in INDEXED_KINDS {
            // 该语言不支持这个 kind 时 node_kinds 为空，scan_kind 直接返回空。
            for m in scan_kind(cwd, lang, kind).await {
                // 只收该语言扩展名的文件（ast-grep 按扩展名推断语言，这里再兜一层）。
                let ext = Path::new(&m.file)
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|s| s.to_ascii_lowercase())
                    .unwrap_or_default();
                if lang_from_ext(&ext) != Some(lang) {
                    continue;
                }
                let sig = signature_of(&m.text);
                if sig.is_empty() {
                    continue;
                }
                let entry = files.entry(m.file.clone()).or_insert_with(|| FileEntry {
                    lang: lang.to_string(),
                    mtime: mtime_unix(&cwd.join(&m.file)),
                    symbols: Vec::new(),
                });
                let _ = name_of; // 名字由 sig 现算，无需存储（保留函数供将来 name 过滤用）
                entry.symbols.push(Symbol {
                    kind: (*kind).to_string(),
                    sig,
                    line: m.range.start.line + 1, // ast-grep 0-based → 1-based
                    end_line: m.range.end.line + 1,
                });
            }
        }
    }

    // 每文件符号去重 + 按行排序（多个 kind 扫描可能重复命中同一节点）。
    for entry in files.values_mut() {
        entry.symbols.sort_by(|a, b| a.line.cmp(&b.line).then(a.sig.cmp(&b.sig)));
        entry.symbols.dedup_by(|a, b| a.line == b.line && a.sig == b.sig);
        *lang_counts.entry(entry.lang.clone()).or_insert(0) += 1;
    }

    let index = CodeGraphIndex {
        version: CodeGraphIndex::CURRENT_VERSION,
        built_at: now_unix(),
        lang_counts,
        files,
    };
    let n_files = index.files.len();
    let n_syms = index.symbol_count();

    // 落盘（best-effort）。
    if let Some(dir) = index_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string(&index) {
        let _ = std::fs::write(&index_path, json);
    }

    BuildOutcome::Built {
        files: n_files,
        symbols: n_syms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_strips_body_and_flattens() {
        assert_eq!(
            signature_of("int add(int a,\n        int b) {\n return a+b; }"),
            "int add(int a, int b)"
        );
        assert_eq!(signature_of("struct foo {"), "struct foo");
    }

    #[test]
    fn name_of_extracts_ident() {
        assert_eq!(name_of("int add(int a, int b)"), "add");
        assert_eq!(name_of("pub fn scan_kind"), "scan_kind");
        assert_eq!(name_of("struct foo"), "foo");
    }

    #[test]
    fn index_path_is_under_dot_latte() {
        let p = CodeGraphIndex::path(Path::new("/tmp/proj"));
        assert!(p.ends_with(".latte/code_graph/index.json"));
    }

    #[test]
    fn lang_from_ext_maps_c_family() {
        assert_eq!(lang_from_ext("h"), Some("c"));
        assert_eq!(lang_from_ext("rs"), Some("rust"));
        assert_eq!(lang_from_ext("md"), None);
    }

    #[test]
    fn normalize_scope_handles_relative_and_dot() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(normalize_scope(&cwd, "."), Some(".".to_string()));
        assert_eq!(normalize_scope(&cwd, ""), Some(".".to_string()));
        assert_eq!(normalize_scope(&cwd, "./src/lib.c"), Some("src/lib.c".to_string()));
        assert_eq!(normalize_scope(&cwd, "src/lib.c"), Some("src/lib.c".to_string()));
        assert_eq!(normalize_scope(&cwd, "src/"), Some("src".to_string()));
    }

    #[test]
    fn normalize_scope_strips_absolute_repo_prefix() {
        // 绝对路径落在仓库根下 → strip 成相对 key。用真实临时目录，
        // 因为 normalize_scope 会 canonicalize（需要目录真实存在）。
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.c"), "int x;\n").unwrap();
        let abs = dir.path().join("src/lib.c");
        assert_eq!(
            normalize_scope(dir.path(), abs.to_str().unwrap()),
            Some("src/lib.c".to_string()),
            "absolute path under repo root must normalize to relative key"
        );
    }

    #[test]
    fn normalize_scope_rejects_path_outside_repo() {
        // 绝对路径不在仓库根下 → None（调用方回退实时扫描，不谎报 0 命中）。
        let repo = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join("foo.c"), "int y;\n").unwrap();
        let outside = other.path().join("foo.c");
        assert_eq!(
            normalize_scope(repo.path(), outside.to_str().unwrap()),
            None,
            "absolute path outside repo root must return None"
        );
    }

    #[tokio::test]
    async fn query_signatures_matches_absolute_path() {
        // 回归测试：模型传绝对路径时也应命中预建索引（旧实现会谎报 0）。
        if !ast_grep_available().await {
            eprintln!("ast-grep not installed; skipping absolute-path query test");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/lib.c"),
            "void *do_alloc(size_t size) { return 0; }\nvoid do_free(void *p) { (void)p; }\n",
        )
        .unwrap();
        let built = build_or_update(dir.path()).await;
        assert!(matches!(built, BuildOutcome::Built { .. }), "got {built:?}");

        // 相对路径命中（基线）。
        let (rel_lines, rel_total) =
            query_signatures(dir.path(), "src/lib.c", "function", None, 200, 12_000).unwrap();
        assert_eq!(rel_total, 2, "relative path baseline: {rel_lines:?}");

        // 绝对路径必须命中同样的结果（旧实现在这里返回 total=0）。
        let abs = dir.path().join("src/lib.c");
        let (abs_lines, abs_total) =
            query_signatures(dir.path(), abs.to_str().unwrap(), "function", None, 200, 12_000)
                .unwrap();
        assert_eq!(abs_total, 2, "absolute path must match same 2 functions: {abs_lines:?}");
        assert!(abs_lines.iter().any(|l| l.contains("do_alloc")));
        assert!(abs_lines.iter().any(|l| l.contains("do_free")));

        // 仓库外的绝对路径 → None（回退实时扫描）。
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("z.c"), "int z(void){return 0;}\n").unwrap();
        let zabs = outside.path().join("z.c");
        assert!(
            query_signatures(dir.path(), zabs.to_str().unwrap(), "function", None, 200, 12_000)
                .is_none(),
            "path outside repo must return None so caller falls back to live scan"
        );
    }

    #[tokio::test]
    async fn build_on_tiny_c_repo_produces_index() {
        // 需要本机有 ast-grep；没有则跳过断言（CI 环境可能没装）。
        if !ast_grep_available().await {
            eprintln!("ast-grep not installed; skipping e2e index test");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.c"),
            "int add(int a, int b) { return a+b; }\nstruct point { int x; int y; };\n",
        )
        .unwrap();

        let outcome = build_or_update(dir.path()).await;
        match outcome {
            BuildOutcome::Built { files, symbols } => {
                assert!(files >= 1, "expected >=1 file, got {files}");
                assert!(symbols >= 2, "expected >=2 symbols (add + point), got {symbols}");
            }
            other => panic!("expected Built, got {other:?}"),
        }

        // 索引落盘 + 可读回。
        let idx: CodeGraphIndex =
            serde_json::from_str(&std::fs::read_to_string(CodeGraphIndex::path(dir.path())).unwrap())
                .unwrap();
        assert_eq!(idx.version, CodeGraphIndex::CURRENT_VERSION);
        let syms: Vec<&Symbol> = idx.files.values().flat_map(|f| &f.symbols).collect();
        assert!(syms.iter().any(|s| s.sig.contains("add")), "index should contain add()");

        // 第二次立即再建：无改动 → UpToDate。
        let again = build_or_update(dir.path()).await;
        assert_eq!(again, BuildOutcome::UpToDate);
    }

    /// 索引路径同样必须回**跨度**。多行函数用 `start-end`，单行定义退化成
    /// 单个行号（`start-start` 对 read 没意义，也白占 token）。
    ///
    /// 回归 task_planner 实测事故，见 [`Symbol::end_line`] 的文档。
    #[tokio::test]
    async fn query_signatures_emits_span_for_multiline_definitions() {
        if !ast_grep_available().await {
            eprintln!("ast-grep not installed; skipping span test");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // one_liner 单行；spread 跨 4 行（2-5）。
        std::fs::write(
            dir.path().join("m.c"),
            "int one_liner(void) { return 1; }\nint spread(int a,\n           int b)\n{\n    return a + b;\n}\n",
        )
        .unwrap();
        assert!(matches!(
            build_or_update(dir.path()).await,
            BuildOutcome::Built { .. }
        ));

        let (lines, total) =
            query_signatures(dir.path(), ".", "function", None, 200, 12_000).unwrap();
        assert_eq!(total, 2, "expected 2 functions: {lines:?}");

        let spread = lines
            .iter()
            .find(|l| l.contains("spread"))
            .expect(&format!("缺 spread: {lines:?}"));
        assert!(
            spread.starts_with("m.c:2-6:"),
            "跨行函数必须回 start-end 跨度，实际 {spread:?}"
        );

        let one = lines
            .iter()
            .find(|l| l.contains("one_liner"))
            .expect(&format!("缺 one_liner: {lines:?}"));
        assert!(
            one.starts_with("m.c:1:"),
            "单行定义应退化成单行号（跨度无意义），实际 {one:?}"
        );
    }

    /// 旧索引（v1，Symbol 无 `end_line`）反序列化不得报错，且退化成单行号
    /// 而不是伪造一个 `start-0` 之类的坏跨度。
    #[test]
    fn v1_symbol_without_end_line_degrades_to_start_only() {
        let s: Symbol =
            serde_json::from_str(r#"{"kind":"function","sig":"int f(void)","line":42}"#)
                .expect("v1 Symbol 必须仍能反序列化");
        assert_eq!(s.end_line, 0, "缺字段应为 0（未知）");
        assert_eq!(s.span(), "42", "跨度未知时只报起始行");

        let s2 = Symbol { end_line: 50, ..s.clone() };
        assert_eq!(s2.span(), "42-50");
        // end == start（单行定义）同样退化，避免 `42-42` 这种冗余。
        let s3 = Symbol { end_line: 42, ..s };
        assert_eq!(s3.span(), "42");
    }

    #[tokio::test]
    async fn query_signatures_serves_from_prebuilt_index() {
        if !ast_grep_available().await {
            eprintln!("ast-grep not installed; skipping query fast-path test");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("m.c"),
            "int add(int a, int b) { return a+b; }\nint mul(int a, int b) { return a*b; }\nstruct pt { int x; };\n",
        )
        .unwrap();
        let built = build_or_update(dir.path()).await;
        assert!(matches!(built, BuildOutcome::Built { .. }));

        // 查所有函数：应命中 add + mul。
        let (lines, total) =
            query_signatures(dir.path(), ".", "function", None, 200, 12_000).unwrap();
        assert_eq!(total, 2, "expected 2 functions, got {total}: {lines:?}");
        assert!(lines.iter().any(|l| l.contains("add")));
        assert!(lines.iter().any(|l| l.contains("mul")));
        // 行号格式 `file:line: sig`。
        assert!(lines.iter().all(|l| l.contains("m.c:")));

        // name 过滤：只要 add。
        let (lines, total) =
            query_signatures(dir.path(), ".", "function", Some("add"), 200, 12_000).unwrap();
        assert_eq!(total, 1, "name filter should narrow to add: {lines:?}");

        // 非索引 kind（call）→ 不命中索引，返回 None（工具会回退实时扫描）。
        assert!(query_signatures(dir.path(), ".", "call", None, 200, 12_000).is_none());

        // 文件被改动后 → 索引对该范围不新鲜 → None（回退实时扫描）。
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(
            dir.path().join("m.c"),
            "int add(int a, int b) { return a+b; }\nint sub(int a, int b){return a-b;}\n",
        )
        .unwrap();
        assert!(
            query_signatures(dir.path(), ".", "function", None, 200, 12_000).is_none(),
            "stale index (file mtime > built_at) must return None"
        );
    }

    #[tokio::test]
    async fn rebuild_picks_up_code_modifications() {
        // 场景：代码被改动后再次 build_or_update，索引应反映新符号。
        if !ast_grep_available().await {
            eprintln!("ast-grep not installed; skipping modify-code test");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("s.c");
        std::fs::write(&file, "int one(void) { return 1; }\n").unwrap();

        // 首建：只有 one()。
        assert!(matches!(build_or_update(dir.path()).await, BuildOutcome::Built { .. }));
        let (_l, total) = query_signatures(dir.path(), ".", "function", None, 200, 12_000).unwrap();
        assert_eq!(total, 1);

        // 无改动再建 → UpToDate。
        assert_eq!(build_or_update(dir.path()).await, BuildOutcome::UpToDate);

        // 修改代码：新增一个函数。等 >1s 让 mtime 明确超过 built_at。
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(&file, "int one(void){return 1;}\nint two(void){return 2;}\n").unwrap();

        // 改动被检测 → 重建。
        let outcome = build_or_update(dir.path()).await;
        assert!(matches!(outcome, BuildOutcome::Built { .. }), "modified file must trigger rebuild, got {outcome:?}");

        // 索引反映新符号：two() 现在也在。
        let (lines, total) = query_signatures(dir.path(), ".", "function", None, 200, 12_000).unwrap();
        assert_eq!(total, 2, "rebuild should include the new function: {lines:?}");
        assert!(lines.iter().any(|l| l.contains("two")), "new symbol two() must be indexed");
    }
}
