//! 学习文档里「`符号`（`文件:行号`）」的机械核对与自动修正。
//!
//! # 动机（实测 LAT-110 jemalloc，会话 ui-5976）
//!
//! 引擎此前对出处只查两件事：文件存在、行号不越界。于是
//! 「`SC_LG_MAX_LOOKUP`（`sc.h:247`）」照样放行——247 行真实存在，只是内容是
//! `#endif`，宏在 250 行。全文 310 条「符号 + 出处」里错了几十条，返工规则
//! 还要求「原有出处一条不许删」，错的行号就此被锁死。
//!
//! # 判据
//!
//! 只核对**能机械判定**的那一类：同一行里一个代码片段紧挨着一个出处
//! （「`X`（`f.h:12`）」「`X`，`f.h:12`」或「`f.h:12` 的 `X`」）。
//!
//! 1. 出处的文件解析到仓库里唯一的源文件，解析不到 → 不判；
//! 2. 引用的行里按词边界出现了 `X` → 对；
//! 3. 没出现，且 `X` 在该文件里**恰好有一处定义**（`#define`、函数/变量/类型定义）
//!    → 引擎把行号改成定义所在的行（原来是区间就改成定义的完整区间）；
//! 4. 没出现、定义有多处 → 判错，交给写手（报错里附真实行）；
//! 5. 文件里没有 `X` 的定义（`uint8_t`、`malloc` 这类只是被用到的名字）→ 不判。
//!
//! 第 5 条是刻意的保守：误判一条出处的代价（把对的行号改错）比漏判高。

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// 一条核对结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// 文档里的行号（1 起）。
    pub doc_line: usize,
    pub symbol: String,
    /// 原出处文本（如 `include/x/sc.h:247`）。
    pub cite: String,
    pub kind: FindingKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindingKind {
    /// 引擎已改成 `new_cite`。
    Fixed { new_cite: String },
    /// 修不了：定义有多处，或出处是多段区间。`defs` 是定义所在行（1 起）与原文。
    Wrong { file: String, defs: Vec<(usize, String)> },
}

impl Finding {
    pub fn is_fixed(&self) -> bool {
        matches!(self.kind, FindingKind::Fixed { .. })
    }

    /// 给人/模型看的一句话。
    pub fn describe(&self) -> String {
        match &self.kind {
            FindingKind::Fixed { new_cite } => {
                format!("`{}` 的出处 `{}` → `{}`", self.symbol, self.cite, new_cite)
            }
            FindingKind::Wrong { file, defs } => {
                let shown: Vec<String> = defs
                    .iter()
                    .take(4)
                    .map(|(n, l)| format!("{file}:{n} `{}`", clip(l.trim(), 80)))
                    .collect();
                format!(
                    "`{}` 标在 `{}`，但这几行里没有它；它在 {}",
                    self.symbol,
                    self.cite,
                    shown.join("、")
                )
            }
        }
    }
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        s.chars().take(n).collect::<String>() + "…"
    } else {
        s.to_string()
    }
}

/// 仓库源文件索引：把文档里的短路径（`sc.h`）解析成唯一的仓库路径。
pub struct SourceIndex {
    cwd: PathBuf,
    files: Option<Vec<String>>,
    cache: HashMap<String, Option<std::sync::Arc<Vec<String>>>>,
}

const SKIP_DIRS: &[&str] = &["target", "node_modules", "build", "dist", "out"];
const MAX_FILES: usize = 60_000;

impl SourceIndex {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            files: None,
            cache: HashMap::new(),
        }
    }

    fn all_files(&mut self) -> &[String] {
        if self.files.is_none() {
            let mut out = Vec::new();
            let mut stack = vec![PathBuf::new()];
            while let Some(rel) = stack.pop() {
                let Ok(rd) = std::fs::read_dir(self.cwd.join(&rel)) else {
                    continue;
                };
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if name.starts_with('.') {
                        continue;
                    }
                    let Ok(ft) = e.file_type() else { continue };
                    let child = rel.join(&name);
                    if ft.is_dir() {
                        if !SKIP_DIRS.contains(&name.as_str()) {
                            stack.push(child);
                        }
                    } else if ft.is_file() && out.len() < MAX_FILES {
                        out.push(child.to_string_lossy().replace('\\', "/"));
                    }
                }
            }
            self.files = Some(out);
        }
        self.files.as_deref().unwrap_or(&[])
    }

    /// 解析成仓库相对路径；找不到或有歧义返回 None。
    pub fn resolve(&mut self, cited: &str) -> Option<String> {
        let cited = cited.trim_start_matches("./");
        if cited.is_empty() || cited.split('/').any(|s| s == "..") || cited.starts_with('/') {
            return None;
        }
        if self.cwd.join(cited).is_file() {
            return Some(cited.to_string());
        }
        let suffix = format!("/{cited}");
        let mut hits = self.all_files().iter().filter(|f| f.ends_with(&suffix));
        let first = hits.next()?.clone();
        hits.next().is_none().then_some(first)
    }

    /// 文件按行切好（缓存）。
    pub fn lines(&mut self, rel: &str) -> Option<std::sync::Arc<Vec<String>>> {
        if let Some(c) = self.cache.get(rel) {
            return c.clone();
        }
        let v = std::fs::read_to_string(self.cwd.join(rel))
            .ok()
            .map(|t| std::sync::Arc::new(t.lines().map(str::to_string).collect::<Vec<_>>()));
        self.cache.insert(rel.to_string(), v.clone());
        v
    }
}

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// `line` 里是否按词边界出现 `sym`。
pub fn contains_word(line: &str, sym: &str) -> bool {
    word_positions(line, sym).next().is_some()
}

fn word_positions<'a>(line: &'a str, sym: &'a str) -> impl Iterator<Item = usize> + 'a {
    line.match_indices(sym).filter_map(move |(i, _)| {
        let before = line[..i].chars().next_back();
        let after = line[i + sym.len()..].chars().next();
        (!before.is_some_and(is_ident) && !after.is_some_and(is_ident)).then_some(i)
    })
}

/// 「数错了几行」的搜索半径。
const NEAR: usize = 12;

const DEF_KEYWORDS: &[&str] = &[
    "fn", "struct", "enum", "union", "trait", "type", "const", "static", "class", "def", "func",
    "interface", "typedef",
];

/// 这一行是不是 `sym` 的定义（或顶层声明）。
fn is_definition(line: &str, sym: &str) -> bool {
    let trimmed = line.trim_start();
    // #define SYM
    if let Some(rest) = trimmed.strip_prefix('#') {
        let rest = rest.trim_start();
        if let Some(rest) = rest.strip_prefix("define") {
            let rest = rest.trim_start();
            return rest.starts_with(sym)
                && !rest[sym.len()..].chars().next().is_some_and(is_ident);
        }
        return false;
    }
    for i in word_positions(line, sym) {
        let before = line[..i].trim_end();
        let prev_tok = before
            .rsplit(|c: char| !is_ident(c))
            .next()
            .unwrap_or("");
        // `static int` 这种：关键字后面紧跟的是类型名、行就此结束，不是定义。
        if DEF_KEYWORDS.contains(&prev_tok) && !line[i + sym.len()..].trim().is_empty() {
            return true;
        }
        // `} SYM;`（typedef struct 收尾）
        if before == "}" && line[i + sym.len()..].trim_start().starts_with(';') {
            return true;
        }
        // C 顶层：第 0 列起头的函数定义 / 变量定义 / 声明。
        let col0 = line.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
        if col0 && !before.contains('(') && !before.contains('=') {
            let after = line[i + sym.len()..].trim_start();
            if after.starts_with('(') || after.starts_with('[') || after.starts_with('=') || after.starts_with(';') {
                return true;
            }
        }
    }
    false
}

/// 定义的完整区间（1 起，闭区间）。
fn definition_extent(lines: &[String], idx: usize) -> (usize, usize) {
    let first = &lines[idx];
    let is_define = first.trim_start().starts_with('#');
    let paren_depth = |s: &str| -> i32 {
        s.chars().fold(0, |d, c| match c {
            '(' => d + 1,
            ')' => d - 1,
            _ => d,
        })
    };
    if is_define {
        let mut depth = paren_depth(first);
        let mut end = idx;
        while (lines[end].trim_end().ends_with('\\') || depth > 0) && end + 1 < lines.len() && end - idx < 60 {
            end += 1;
            depth += paren_depth(&lines[end]);
        }
        return (idx + 1, end + 1);
    }
    if first.trim_end().ends_with(';') {
        return (idx + 1, idx + 1);
    }
    // 函数 / 结构体：从这一行起找 `{`（参数可能跨多行，最多看 40 行），再配对到收尾的 `}`。
    let mut depth = 0i32;
    let mut opened = false;
    for (j, l) in lines.iter().enumerate().skip(idx).take(400) {
        if !opened && j > idx + 40 {
            break;
        }
        if !opened && l.trim_end().ends_with(';') && !l.contains('{') {
            break;
        }
        for c in l.chars() {
            match c {
                '{' => {
                    depth += 1;
                    opened = true;
                }
                '}' => depth -= 1,
                _ => {}
            }
        }
        if opened && depth <= 0 {
            return (idx + 1, j + 1);
        }
    }
    (idx + 1, idx + 1)
}

/// `path:12` / `path:12-48` / `path:1-3,7-9` → (path, 区间列表)。
fn parse_cite(content: &str) -> Option<(String, Vec<(usize, usize)>)> {
    let content = content.trim();
    let (path, ranges) = content.rsplit_once(':')?;
    let file = path.rsplit('/').next()?;
    let (stem, ext) = file.rsplit_once('.')?;
    if stem.is_empty() || ext.is_empty() || !ext.chars().all(|c| c.is_ascii_alphanumeric()) || path.contains("://") {
        return None;
    }
    if path.chars().any(|c| c.is_whitespace() || c == '`') {
        return None;
    }
    let mut out = Vec::new();
    for part in ranges.split(',') {
        let part = part.trim();
        let (a, b) = part.split_once('-').unwrap_or((part, part));
        let a: usize = a.trim().parse().ok()?;
        let b: usize = b.trim().parse().ok()?;
        if a == 0 || b < a {
            return None;
        }
        out.push((a, b));
    }
    (!out.is_empty()).then(|| (path.to_string(), out))
}

/// 代码片段里打头的标识符（`SC_NBINS = 36` → `SC_NBINS`，`sz_s2u()` → `sz_s2u`）。
fn leading_symbol(content: &str) -> Option<String> {
    let mut s = content.trim();
    for p in ["#define ", "#define\t", "struct ", "enum ", "union ", "typedef ", "fn ", "*", "&"] {
        if let Some(r) = s.strip_prefix(p) {
            s = r.trim_start();
        }
    }
    let sym: String = s.chars().take_while(|c| is_ident(*c)).collect();
    // 只核对像「项目里的名字」的标识符：带下划线/大写，或足够长。`int`、`size`
    // 这类关键字和普通词到处都是，按它判出处只会误改。
    let distinctive = sym.contains('_') || sym.chars().any(|c| c.is_ascii_uppercase()) || sym.chars().count() >= 8;
    let ok = distinctive
        && sym.chars().count() >= 3
        && sym.chars().next().is_some_and(|c| !c.is_ascii_digit())
        && !STOP_WORDS.contains(&sym.as_str());
    ok.then_some(sym)
}

const STOP_WORDS: &[&str] = &[
    "size_t", "ssize_t", "uint8_t", "uint16_t", "uint32_t", "uint64_t", "int8_t", "int16_t",
    "int32_t", "int64_t", "uintptr_t", "intptr_t", "ptrdiff_t", "unsigned", "NULL", "EINVAL",
    "ENOMEM", "UINT64_MAX", "SIZE_MAX", "SSIZE_MAX", "INT_MAX", "UINT_MAX",
];

/// 单反引号代码片段：(内容起点, 内容终点) 字节下标。
fn inline_spans(line: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            // 跳过 `` 双反引号片段
            if bytes.get(i + 1) == Some(&b'`') {
                let mut j = i;
                while j < bytes.len() && bytes[j] == b'`' {
                    j += 1;
                }
                i = j;
                continue;
            }
            if let Some(rel) = line[i + 1..].find('`') {
                out.push((i + 1, i + 1 + rel));
                i = i + 1 + rel + 1;
                continue;
            }
            break;
        }
        i += 1;
    }
    out
}

fn backward_gap_ok(gap: &str) -> bool {
    gap.chars().count() <= 4
        && gap
            .chars()
            .all(|c| c.is_whitespace() || "（(，,：:、=".contains(c))
}

fn forward_gap_ok(gap: &str) -> bool {
    matches!(gap.trim(), "的" | "里的" | "中的" | "处的")
}

/// 核对 `text`，返回修正后的全文与结论。
pub fn check_text(text: &str, idx: &mut SourceIndex) -> (String, Vec<Finding>) {
    let mut findings = Vec::new();
    let mut out_lines: Vec<String> = Vec::new();
    let mut fence: Option<String> = None;
    for (n, line) in text.split('\n').enumerate() {
        let t = line.trim_start();
        if let Some(f) = &fence {
            if t.starts_with(f.as_str()) {
                fence = None;
            }
            out_lines.push(line.to_string());
            continue;
        }
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = Some(t[..3].to_string());
            out_lines.push(line.to_string());
            continue;
        }
        let spans = inline_spans(line);
        // (内容起点, 内容终点, 新内容)
        let mut edits: Vec<(usize, usize, String)> = Vec::new();
        for (k, &(s, e)) in spans.iter().enumerate() {
            let content = &line[s..e];
            let Some((path, ranges)) = parse_cite(content) else {
                continue;
            };
            let symbol = k
                .checked_sub(1)
                .map(|p| spans[p])
                .filter(|&(_, pe)| backward_gap_ok(&line[pe + 1..s - 1]))
                .and_then(|(ps, pe)| leading_symbol(&line[ps..pe]))
                .or_else(|| {
                    spans
                        .get(k + 1)
                        .filter(|&&(ns, _)| forward_gap_ok(&line[e + 1..ns - 1]))
                        .and_then(|&(ns, ne)| leading_symbol(&line[ns..ne]))
                });
            let Some(symbol) = symbol else { continue };
            let Some(rel) = idx.resolve(&path) else { continue };
            let Some(lines) = idx.lines(&rel) else { continue };
            let in_range = ranges.iter().any(|&(a, b)| {
                lines
                    .iter()
                    .skip(a - 1)
                    .take(b + 1 - a)
                    .any(|l| contains_word(l, &symbol))
            });
            if in_range {
                continue;
            }
            let defs: Vec<usize> = lines
                .iter()
                .enumerate()
                .filter(|(_, l)| is_definition(l, &symbol))
                .map(|(i, _)| i)
                .collect();
            // 引的是定义体内部的一段（函数体里的几行）→ 对。
            let inside_def = defs.iter().any(|&d| {
                let (a, b) = definition_extent(&lines, d);
                ranges.iter().all(|&(ra, rb)| ra >= a && rb <= b)
            });
            if inside_def {
                continue;
            }
            let file_short = path.clone();
            let single = ranges.len() == 1 && ranges[0].0 == ranges[0].1;
            // 就在附近（±NEAR 行）出现过：多半是数错了几行，改到最近的那一处。
            let near = if ranges.len() == 1 {
                let (ra, rb) = ranges[0];
                lines
                    .iter()
                    .enumerate()
                    .filter(|(i, l)| {
                        let n = i + 1;
                        n + NEAR >= ra && n <= rb + NEAR && contains_word(l, &symbol)
                    })
                    .map(|(i, _)| i)
                    // 附近有定义就取定义（多数引用指的是定义），否则取最近的一处用法。
                    .min_by_key(|&i| {
                        let n = i + 1;
                        let dist = if n < ra { ra - n } else { n.saturating_sub(rb) };
                        (!defs.contains(&i), dist)
                    })
            } else {
                None
            };
            // 长区间里没出现这个名字：多半是在讲一整段逻辑、顺带提到它，只按用法平移
            // 会把整段挪走，不改；附近有定义才改。
            let short = ranges.len() == 1 && ranges[0].1 - ranges[0].0 < 6;
            let near = near.filter(|i| short || defs.contains(i));
            let target = match near {
                Some(i) => Some(i),
                None if defs.len() == 1 && ranges.len() == 1 => Some(defs[0]),
                None => None,
            };
            if defs.is_empty() && near.is_none() {
                continue;
            }
            if let Some(t) = target {
                let (a, b) = if defs.contains(&t) {
                    definition_extent(&lines, t)
                } else if single {
                    (t + 1, t + 1)
                } else {
                    // 用到的地方：区间整体平移到覆盖它，篇幅不变。
                    let (ra, rb) = ranges[0];
                    let n = t + 1;
                    if n < ra { (n, n + (rb - ra)) } else { (n - (rb - ra), n) }
                };
                let new_lines = if single || a == b { a.to_string() } else { format!("{a}-{b}") };
                let new_cite = format!("{path}:{new_lines}");
                edits.push((s, e, new_cite.clone()));
                findings.push(Finding {
                    doc_line: n + 1,
                    symbol,
                    cite: content.trim().to_string(),
                    kind: FindingKind::Fixed { new_cite },
                });
            } else {
                findings.push(Finding {
                    doc_line: n + 1,
                    symbol,
                    cite: content.trim().to_string(),
                    kind: FindingKind::Wrong {
                        file: file_short,
                        defs: defs.iter().map(|&i| (i + 1, lines[i].clone())).collect(),
                    },
                });
            }
        }
        let mut new_line = line.to_string();
        for (s, e, rep) in edits.into_iter().rev() {
            new_line.replace_range(s..e, &rep);
        }
        out_lines.push(new_line);
    }
    (out_lines.join("\n"), findings)
}

/// 核对并就地改写文件。返回全部结论（含已修正的）。
pub fn autofix_file(path: &Path, idx: &mut SourceIndex) -> Vec<Finding> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let (fixed, findings) = check_text(&text, idx);
    if fixed != text {
        let _ = std::fs::write(path, fixed);
    }
    findings
}

/// `text` 里核对不通过的出处原文（含引擎能修的）。返工门禁用：这些出处
/// 被改掉、删掉都不算「删了原有出处」。
pub fn wrong_citations(text: &str, idx: &mut SourceIndex) -> BTreeSet<String> {
    check_text(text, idx).1.into_iter().map(|f| f.cite).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SC_H: &str = "/* head */\n\
#if (SC_NBINS > 256)\n\
#  error \"Too many\"\n\
#endif\n\
\n\
/* The largest size class in the lookup table. */\n\
#define SC_LG_MAX_LOOKUP 12\n\
#define SC_LOOKUP_MAXCLASS (1 << SC_LG_MAX_LOOKUP)\n\
\n\
#define SC_NBINS (\t\t\t\t\n\
    SC_NTINY + SC_NPSEUDO\n\
    - 1)\n\
\n\
JEMALLOC_ALWAYS_INLINE szind_t\n\
sz_size2index(size_t size) {\n\
\tif (size) {\n\
\t\treturn 1;\n\
\t}\n\
\treturn 0;\n\
}\n\
#ifdef A\n\
#define DUP 1\n\
#else\n\
#define DUP 2\n\
#endif\n";

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("include/x")).unwrap();
        std::fs::write(dir.path().join("include/x/sc.h"), SC_H).unwrap();
        dir
    }

    #[test]
    fn wrong_line_with_unique_definition_is_fixed() {
        let dir = repo();
        let mut idx = SourceIndex::new(dir.path());
        // 回归 LAT-110：`SC_LG_MAX_LOOKUP`（`sc.h:247`）→ 宏实际在 250。
        let doc = "边界 `SC_LG_MAX_LOOKUP = 12`（`sc.h:3`）写死。\n守门 `SC_NBINS`（`include/x/sc.h:2-4`）。";
        let (fixed, f) = check_text(doc, &mut idx);
        assert!(fixed.contains("`SC_LG_MAX_LOOKUP = 12`（`sc.h:7`）"), "{fixed}");
        // SC_NBINS 在 2 行出现（#if），按词边界算在范围内 → 不动。
        assert!(fixed.contains("`include/x/sc.h:2-4`"), "{fixed}");
        assert_eq!(f.len(), 1);
        assert!(f[0].is_fixed());
    }

    #[test]
    fn range_is_replaced_by_full_definition_extent() {
        let dir = repo();
        let mut idx = SourceIndex::new(dir.path());
        let (fixed, _) = check_text("`SC_NBINS`（`sc.h:5-6`）\n`sz_size2index`，`sc.h:1-2`", &mut idx);
        assert!(fixed.contains("`sc.h:10-12`"), "多行宏按括号配对: {fixed}");
        assert!(fixed.contains("`sc.h:15-20`"), "函数到收尾的 }}: {fixed}");
    }

    #[test]
    fn forward_form_and_ambiguous_definitions() {
        let dir = repo();
        let mut idx = SourceIndex::new(dir.path());
        let (fixed, f) = check_text("见 `sc.h:1` 的 `SC_LOOKUP_MAXCLASS`；`DUP`（`sc.h:1`）", &mut idx);
        assert!(fixed.contains("`sc.h:8` 的"), "{fixed}");
        let wrong = f.iter().find(|x| x.symbol == "DUP").unwrap();
        assert!(!wrong.is_fixed(), "两处定义不猜");
        assert!(wrong.describe().contains("sc.h:22") && wrong.describe().contains("sc.h:24"), "{}", wrong.describe());
        assert!(fixed.contains("`DUP`（`sc.h:1`）"), "修不了的不动");
    }

    #[test]
    fn unknown_symbols_files_and_code_blocks_are_left_alone() {
        let dir = repo();
        let mut idx = SourceIndex::new(dir.path());
        let doc = "`uint8_t`（`sc.h:1`）\n`SC_NBINS`（`nope.h:1`）\n```c\n`SC_LG_MAX_LOOKUP`（`sc.h:1`）\n```\n`SC_NBINS = 36` 个档（`sc.h:1`）";
        let (fixed, f) = check_text(doc, &mut idx);
        assert_eq!(fixed, doc);
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn ambiguous_short_path_is_not_resolved() {
        let dir = repo();
        std::fs::create_dir_all(dir.path().join("other")).unwrap();
        std::fs::write(dir.path().join("other/sc.h"), "#define SC_LG_MAX_LOOKUP 1\n").unwrap();
        let mut idx = SourceIndex::new(dir.path());
        let (_, f) = check_text("`SC_LG_MAX_LOOKUP`（`sc.h:3`）", &mut idx);
        assert!(f.is_empty(), "两个 sc.h 不知道指哪个");
        let (fixed, _) = check_text("`SC_LG_MAX_LOOKUP`（`include/x/sc.h:3`）", &mut idx);
        assert!(fixed.contains("include/x/sc.h:7"));
    }

    #[test]
    fn wrong_citations_lists_original_tokens() {
        let dir = repo();
        let mut idx = SourceIndex::new(dir.path());
        let w = wrong_citations("`SC_LG_MAX_LOOKUP`（`sc.h:3`）、`SC_LOOKUP_MAXCLASS`（`sc.h:8`）", &mut idx);
        assert_eq!(w.into_iter().collect::<Vec<_>>(), vec!["sc.h:3".to_string()]);
    }

    /// 手动探针：`LEARN_CITE_REPO=<仓库> LEARN_CITE_DOCS=<目录> cargo test -- --ignored probe`。
    #[test]
    #[ignore]
    fn probe_real_docs() {
        let (Ok(repo), Ok(docs)) = (std::env::var("LEARN_CITE_REPO"), std::env::var("LEARN_CITE_DOCS")) else {
            return;
        };
        let mut idx = SourceIndex::new(Path::new(&repo));
        let mut stack = vec![PathBuf::from(&docs)];
        let (mut fixed, mut wrong) = (0, 0);
        while let Some(p) = stack.pop() {
            if p.is_dir() {
                for e in std::fs::read_dir(&p).unwrap().flatten() {
                    stack.push(e.path());
                }
                continue;
            }
            if p.extension().is_none_or(|e| e != "md") {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap();
            for f in check_text(&text, &mut idx).1 {
                if f.is_fixed() { fixed += 1 } else { wrong += 1 }
                println!("{}:{} {}", p.display(), f.doc_line, f.describe());
            }
        }
        println!("fixed={fixed} wrong={wrong}");
    }
}
