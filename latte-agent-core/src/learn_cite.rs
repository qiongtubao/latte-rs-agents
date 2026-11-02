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
    /// 代码块里带 `N:` 行号前缀的源码摘录，大部分行在所标的位置附近找不到（不是行号错位，是内容对不上）。
    Excerpt { detail: String },
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
            FindingKind::Excerpt { detail } => format!(
                "带行号的代码摘录（出处 `{}`）与源码对不上：{}",
                self.cite, detail
            ),
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
/// 引用区间这么长就当作「指一个区域」，不按其中是否出现某个符号来判对错。
const LONG_RANGE: usize = 40;

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

/// [`parse_cite`] 的对外版本（取证核对要从一行文字里抽出全部出处）。
pub fn parse_cite_pub(content: &str) -> Option<(String, Vec<(usize, usize)>)> {
    parse_cite(content)
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
/// 试学收尾时引擎插在原文处的待确认注记（`> ⚠️ 待确认（试学提问 n1）：…`）。
pub fn is_pending_note(trimmed_line: &str) -> bool {
    trimmed_line.starts_with("> ⚠️ 待确认")
}

/// 去掉所有待确认注记行：它们引用的是被指出有误的出处，不参与出处核对 / 对账。
pub fn without_pending_notes(text: &str) -> String {
    text.split('\n')
        .filter(|l| !is_pending_note(l.trim_start()))
        .collect::<Vec<_>>()
        .join("\n")
}

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
        // 「⚠️ 待确认」注记转述的是小白发现的**错误**出处（"文档写 sc.c:462-473，实际不是"），
        // 原样保留：自动改成"正确"行号会让注记自相矛盾，也不该当成文档的出处去核对。
        if is_pending_note(t) {
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
            // 引的是一大段（≥ LONG_RANGE 行）、符号不在里面也不在附近：这是在指一个区域，符号只是顺带
            // 提到。把整段区域改成符号的定义行等于把出处改坏（实测 `sc_data_init`（`sc.h:1-245`）
            // 被改成 `sc.h:348`，245 行的概览变成了一行）。不判、不改。
            if ranges.len() == 1 && ranges[0].1 - ranges[0].0 + 1 >= LONG_RANGE && near.is_none() {
                continue;
            }
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
    // 代码块里带 `N:` 前缀的源码摘录：行号和内容逐行对源码，错位的由引擎改正。
    findings.extend(fix_numbered_excerpts(&mut out_lines, idx));
    (out_lines.join("\n"), findings)
}

/// 摘录行号向前后找同一行内容的范围（行）。
const EXCERPT_WINDOW: usize = 8;

/// 去掉注释和空白后的代码行；注释行、空行得到空串。
fn code_key(l: &str) -> String {
    let mut s = l.to_string();
    while let (Some(a), Some(b)) = (s.find("/*"), s.find("*/")) {
        if b < a {
            break;
        }
        s.replace_range(a..b + 2, "");
    }
    if let Some(a) = s.find("//") {
        s.truncate(a);
    }
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// 太短的行（`}`、`{`、`};`）在源码里到处都是，单凭它定位不了行号，跟随相邻的定位行。
fn is_weak_key(k: &str) -> bool {
    k.chars().count() <= 3
}

/// 一行 `N: 代码`：返回 (下标起点, 数字, 内容)。
fn numbered_line(line: &str) -> Option<(usize, usize, &str)> {
    let t = line.trim_start();
    let off = line.len() - t.len();
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > 6 {
        return None;
    }
    let rest = t[digits.len()..].strip_prefix(':')?;
    let rest = rest.strip_prefix(' ').or_else(|| rest.strip_prefix('\t')).unwrap_or(rest);
    Some((off, digits.parse().ok()?, rest))
}

/// 一行文字里的 `文件:行号` 出处（第一个能解析到仓库文件的）：(原文 token, 文件, 区间)。
fn header_cite(line: &str, idx: &mut SourceIndex) -> Option<(String, String, (usize, usize))> {
    for tok in line.split(|c: char| !(c.is_ascii_alphanumeric() || "_./+-:".contains(c))) {
        let tok = tok.trim_matches(|c| c == '.' || c == ':' || c == '-');
        let Some((path, ranges)) = parse_cite(tok) else { continue };
        if ranges.len() != 1 {
            continue;
        }
        if let Some(rel) = idx.resolve(&path) {
            return Some((tok.to_string(), rel, ranges[0]));
        }
    }
    None
}

/// 核对并改正代码块里的 `N: 代码` 摘录。
///
/// # 动机（实测 LAT-110 jemalloc，会话 ui-75536）
///
/// 写手把源码摘进代码块、逐行标上行号。引擎只核对正文里的「符号（文件:行号）」，不看代码块里的
/// 行号前缀；16 块带行号摘录里 6 块错了：`sz.h` 的 `sz_size2index_compute` 实际在 155 行，整块标成
/// 156；`arena.c` 少抄了一行注释却保持连续编号，后面的行号全部错位。模型数行号靠心算，没法保证。
///
/// # 判据
///
/// 块里至少两行是 `N: ` 开头（占非空行八成以上），块头（块内头两行或块前三行）能解析出
/// `文件:行号`。对每一行有实际内容的代码（去掉注释、空白后超过 3 个字符），在标注行号 ±8 行里
/// 找同样内容：
/// - 标的位置就对 → 不动；
/// - 附近别处有（多处时取离「上一行的偏移」最近的）→ 改成真实行号；
/// - 空行、注释行、`}` 这类太短的行 → 跟随相邻定位行的偏移；
/// - 找不到 → 不改它的内容，只给它套上相邻行的偏移。有效行里一半以上、且至少 3 行找不到时，
///   这块摘录不是源码（改写、拼凑）→ 判错交给写手。
///
/// 块头里的 `文件:a-b` 起止若等于改正前的首尾行号，同步改成改正后的。
fn fix_numbered_excerpts(lines: &mut [String], idx: &mut SourceIndex) -> Vec<Finding> {
    let mut findings = Vec::new();
    // 先切出所有围栏块：(开围栏行下标, 闭围栏行下标)
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    let mut open: Option<(usize, String)> = None;
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim_start();
        match &open {
            None => {
                if t.starts_with("```") || t.starts_with("~~~") {
                    open = Some((i, t[..3].to_string()));
                }
            }
            Some((start, f)) => {
                if t.starts_with(f.as_str()) && t.trim_end() == f.as_str() {
                    blocks.push((*start, i));
                    open = None;
                }
            }
        }
    }
    for (open_i, close_i) in blocks {
        let body = open_i + 1..close_i;
        let nums: Vec<(usize, usize, usize, String)> = body
            .clone()
            .filter_map(|i| numbered_line(&lines[i]).map(|(off, n, rest)| (i, off, n, rest.to_string())))
            .collect();
        // 块内头两行若是不带行号的注释（`/* 文件:行号 */`），是块头，不计入「非空行」。
        let is_head_comment = |i: usize| {
            let t = lines[i].trim_start();
            numbered_line(&lines[i]).is_none() && (t.starts_with("//") || t.starts_with("/*") || t.starts_with('#'))
        };
        let nonblank = body
            .clone()
            .filter(|&i| !lines[i].trim().is_empty())
            .filter(|&i| !(i < open_i + 3 && is_head_comment(i)))
            .count();
        if nums.len() < 2 || nums.len() * 5 < nonblank * 4 {
            continue;
        }
        // 块头：块内头两行（非行号行）、块前三个非空行（近的优先）。
        let mut head_lines: Vec<usize> = body.clone().filter(|&i| numbered_line(&lines[i]).is_none()).take(2).collect();
        head_lines.extend((0..open_i).rev().filter(|&i| !lines[i].trim().is_empty()).take(3));
        let Some((hl, (token, rel, _))) = head_lines
            .iter()
            .find_map(|&i| header_cite(&lines[i], idx).map(|h| (i, h)))
        else {
            continue;
        };
        let Some(src) = idx.lines(&rel) else { continue };
        let first_claim = nums[0].2;
        let last_claim = nums[nums.len() - 1].2;
        // 逐行定位：Some(真实行号) / None（待跟随）/ 找不到
        #[derive(Clone, Copy, PartialEq)]
        enum Loc {
            At(usize),
            Follow,
            Missing,
        }
        let mut locs: Vec<Loc> = Vec::with_capacity(nums.len());
        let mut prev_delta: Option<i64> = None;
        let (mut strong, mut missing) = (0usize, 0usize);
        let mut missing_lines: Vec<(usize, String, i64)> = Vec::new();
        for (_, _, n, text) in &nums {
            let key = code_key(text);
            if key.is_empty() || is_weak_key(&key) {
                locs.push(Loc::Follow);
                continue;
            }
            strong += 1;
            let at = |m: usize| m >= 1 && m <= src.len() && code_key(&src[m - 1]) == key;
            let want = *n as i64 + prev_delta.unwrap_or(0);
            // 标的位置（含上一行的偏移）就对 → 不动
            let found = if want >= 1 && at(want as usize) {
                Some(want as usize)
            } else if at(*n) {
                Some(*n)
            } else {
                let lo = n.saturating_sub(EXCERPT_WINDOW).max(1);
                let hi = (n + EXCERPT_WINDOW).min(src.len());
                let near = (lo..=hi)
                    .filter(|&m| at(m))
                    .min_by_key(|&m| ((m as i64 - want).abs(), m));
                // 附近没有：写手压缩摘录（省略了一大段）时行号会漂得更远。整个文件里
                // 这一行内容只出现一次，就能确定是它。
                near.or_else(|| {
                    let mut hits = (1..=src.len()).filter(|&m| at(m));
                    let first = hits.next()?;
                    hits.next().is_none().then_some(first)
                })
            };
            // 一条语句被写手折成两行（`... >> lg_delta)) &` / `((ZU(1) ...`）：每一半都是源码的一段，
            // 不是改写。把附近源码的字符串拼起来看这一行是不是其中一段。
            let is_fragment = found.is_none() && {
                let lo = n.saturating_sub(EXCERPT_WINDOW * 3).max(1);
                let hi = (n + EXCERPT_WINDOW * 3).min(src.len());
                let joined: String = (lo..=hi).map(|m| code_key(&src[m - 1])).collect();
                joined.contains(&key)
            };
            if is_fragment {
                locs.push(Loc::Follow);
                continue;
            }
            match found {
                Some(m) => {
                    prev_delta = Some(m as i64 - *n as i64);
                    locs.push(Loc::At(m));
                }
                None => {
                    missing += 1;
                    missing_lines.push((*n, text.trim().to_string(), prev_delta.unwrap_or(0)));
                    locs.push(Loc::Missing);
                }
            }
        }
        if strong == 0 {
            continue;
        }
        let cite_claimed = format!("{rel}:{first_claim}-{last_claim}");
        // 带行号的摘录是在说「源码这一行就是这样」。两行以上对不上，说明写手改写了代码
        // （把 `lg_base` 抄成 `lg_tiny_min`、拼进源码里没有的语句），读者会把它当原文。
        if missing >= 2 {
            let shown: Vec<String> = missing_lines
                .iter()
                .take(3)
                .map(|(n, text, d)| {
                    let pos = (*n as i64 + d).clamp(1, src.len() as i64) as usize;
                    // 附近和这一行开头最像的源码行（改写多半只改了后半截），没有像的就按位置。
                    let key = code_key(text);
                    let prefix = |m: usize| code_key(&src[m - 1]).chars().zip(key.chars()).take_while(|(a, b)| a == b).count();
                    let lo = pos.saturating_sub(EXCERPT_WINDOW * 2).max(1);
                    let hi = (pos + EXCERPT_WINDOW * 2).min(src.len());
                    let at = (lo..=hi)
                        .max_by_key(|&m| (prefix(m), std::cmp::Reverse((m as i64 - pos as i64).abs())))
                        .filter(|&m| prefix(m) >= 6)
                        .unwrap_or(pos);
                    format!(
                        "第 {n} 行「{}」（源码 {at} 行是「{}」）",
                        clip(text, 60),
                        clip(src[at - 1].trim(), 60)
                    )
                })
                .collect();
            findings.push(Finding {
                doc_line: nums[0].0 + 1,
                symbol: "代码摘录".into(),
                cite: cite_claimed,
                kind: FindingKind::Excerpt {
                    detail: format!(
                        "{strong} 行有效代码里 {missing} 行在 `{rel}` 里找不到：{}{}。带行号的摘录必须是源码原文（可以省略几行，不要改写、拼接）；\
                         要改写就去掉行号前缀，当示意代码写",
                        shown.join("；"),
                        if missing > 3 { "；…" } else { "" }
                    ),
                },
            });
            continue;
        }
        // 把待跟随的行补上：跟随前一个已定位行的偏移，没有就跟后一个。
        let mut delta_at: Vec<Option<i64>> = locs
            .iter()
            .zip(&nums)
            .map(|(l, (_, _, n, _))| if let Loc::At(m) = l { Some(*m as i64 - *n as i64) } else { None })
            .collect();
        let mut carry: Option<i64> = None;
        for d in delta_at.iter_mut() {
            match d {
                Some(v) => carry = Some(*v),
                None => *d = carry,
            }
        }
        let mut carry: Option<i64> = None;
        for d in delta_at.iter_mut().rev() {
            match d {
                Some(v) => carry = Some(*v),
                None => *d = carry,
            }
        }
        let new_nums: Vec<i64> = nums
            .iter()
            .zip(&delta_at)
            .map(|((_, _, n, _), d)| *n as i64 + d.unwrap_or(0))
            .collect();
        if new_nums.iter().zip(nums.iter()).all(|(nn, (_, _, n, _))| *nn == *n as i64) {
            continue;
        }
        // 定位到的行，新行号必须严格递增且落在文件内，否则宁可不动。写手自己加的注释行
        // （源码里没有）只是跟随邻行，可以和邻行同号。
        let located: Vec<i64> = locs
            .iter()
            .zip(&new_nums)
            .filter(|(l, _)| matches!(l, Loc::At(_)))
            .map(|(_, v)| *v)
            .collect();
        if located.windows(2).any(|w| w[1] <= w[0]) || new_nums.iter().any(|&v| v < 1 || v as usize > src.len()) {
            continue;
        }
        for ((i, off, _, _), nn) in nums.iter().zip(&new_nums) {
            let l = &lines[*i];
            let t = l.trim_start();
            let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
            lines[*i] = format!("{}{}{}", &l[..*off], nn, &t[digits..]);
        }
        let (new_first, new_last) = (new_nums[0] as usize, new_nums[new_nums.len() - 1] as usize);
        // 块头的起止行号同步（只改等于旧首尾的那一端）。
        {
            let (ha, hb) = parse_cite(&token).map(|(_, r)| r[0]).unwrap_or((0, 0));
            let na = if ha == first_claim { new_first } else { ha };
            let nb = if hb == last_claim { new_last } else { hb };
            if (na, nb) != (ha, hb) {
                if let Some((path, _)) = token.rsplit_once(':') {
                    let new_token = if na == nb { format!("{path}:{na}") } else { format!("{path}:{na}-{nb}") };
                    lines[hl] = lines[hl].replacen(&token, &new_token, 1);
                }
            }
        }
        findings.push(Finding {
            doc_line: nums[0].0 + 1,
            symbol: "代码摘录行号".into(),
            cite: cite_claimed,
            kind: FindingKind::Fixed { new_cite: format!("{rel}:{new_first}-{new_last}") },
        });
    }
    findings
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

    /// 待确认注记里转述的错误出处不被"自动修正"，也不算文档的出处。
    #[test]
    fn pending_notes_are_left_alone() {
        let dir = repo();
        let mut idx = SourceIndex::new(dir.path());
        let note = "> ⚠️ 待确认（试学提问 n1）：`SC_LG_MAX_LOOKUP`（`sc.h:3`）行号不对";
        let (fixed, f) = check_text(note, &mut idx);
        assert_eq!(fixed, note, "注记原样保留");
        assert!(f.is_empty());
        let doc = format!("正文\n{note}\n尾");
        assert_eq!(without_pending_notes(&doc), "正文\n尾");
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

    const SZ_H: &str = "/* sz.h */\n\
\n\
static inline szind_t\n\
sz_size2index_compute(size_t size) {\n\
\tif (unlikely(size > MAX)) {\n\
\t\treturn NSIZES;\n\
\t}\n\
\n\
\tif (size == 0) {\n\
\t\treturn 0;\n\
\t}\n\
\t/* comment */\n\
\tszind_t x = lg_floor(size);\n\
\treturn x;\n\
}\n";

    fn repo_with_sz() -> (tempfile::TempDir, SourceIndex) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("include")).unwrap();
        std::fs::write(dir.path().join("include/sz.h"), SZ_H).unwrap();
        let idx = SourceIndex::new(dir.path());
        (dir, idx)
    }

    /// 整块行号都差 1：按内容改成真实行号，块头的起止行号同步。
    #[test]
    fn numbered_excerpt_shifted_block_is_renumbered_with_header() {
        let (_d, mut idx) = repo_with_sz();
        let doc = "```c\n\
/* include/sz.h:4-8 */\n\
4: static inline szind_t                  // 静态内联\n\
5: sz_size2index_compute(size_t size) {  // 入口\n\
6: \tif (unlikely(size > MAX)) {           // 超限\n\
7: \t\treturn NSIZES;                    // 哨兵\n\
8: \t}\n\
```\n";
        let (fixed, f) = check_text(doc, &mut idx);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].is_fixed(), "{f:?}");
        assert!(fixed.contains("/* include/sz.h:3-7 */"), "块头起止同步: {fixed}");
        assert!(fixed.contains("\n3: static inline szind_t") && fixed.contains("\n4: sz_size2index_compute"), "{fixed}");
        assert!(fixed.contains("\n7: \t}"), "短行 `}}` 跟随相邻行的偏移: {fixed}");
        // 改完再核对：干净
        assert!(check_text(&fixed, &mut idx).1.is_empty());
    }

    /// 少抄了一行注释却保持连续编号：偏移在块中间变化，逐行改，不是整块平移。
    #[test]
    fn numbered_excerpt_with_skipped_line_gets_per_line_numbers() {
        let (_d, mut idx) = repo_with_sz();
        let doc = "出处 include/sz.h:9-13\n\n```c\n\
9: \tif (size == 0) {\n\
10: \t\treturn 0;\n\
11: \t}\n\
12: \tszind_t x = lg_floor(size);\n\
13: \treturn x;\n\
```\n";
        let (fixed, f) = check_text(doc, &mut idx);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(fixed.contains("\n13: \tszind_t x = lg_floor(size);"), "跳过注释行后的行号: {fixed}");
        assert!(fixed.contains("\n14: \treturn x;"), "{fixed}");
        assert!(fixed.contains("\n9: \tif (size == 0) {") && fixed.contains("\n11: \t}"), "前面的行不动: {fixed}");
        assert!(fixed.contains("include/sz.h:9-14"), "块前一行的出处同步: {fixed}");
    }

    /// 引一大段区域、符号只是顺带提到：不改不判（实测 `sc_data_init`（`sc.h:1-245`）被改成 `sc.h:348`）。
    #[test]
    fn long_region_cite_with_symbol_mentioned_elsewhere_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut src = String::new();
        for i in 1..=300 {
            src.push_str(&format!("int filler_{i};\n"));
        }
        src.push_str("void sc_data_init(void);\n");
        std::fs::write(dir.path().join("sc.h"), src).unwrap();
        let mut idx = SourceIndex::new(dir.path());
        let doc = "概览见 `sc_data_init`（`sc.h:1-245`）整个头文件。\n";
        let (fixed, f) = check_text(doc, &mut idx);
        assert!(f.is_empty() && fixed == doc, "{f:?}\n{fixed}");
        // 短区间照旧会被订正
        let short = "入口 `sc_data_init`（`sc.h:10-12`）。\n";
        let (fixed, f) = check_text(short, &mut idx);
        assert!(f.len() == 1 && f[0].is_fixed() && fixed.contains("sc.h:301"), "{fixed}");
    }

    /// 写手在摘录里自己加了源码里没有的注释行（也编了号）：其后的行号整体差 2，只有真实代码行定位行号，
    /// 加出来的注释行跟随邻行，不因为同号就整块放弃。
    #[test]
    fn numbered_excerpt_with_writer_added_comment_line_is_still_fixed() {
        let (_d, mut idx) = repo_with_sz();
        let doc = "```c\n/* include/sz.h:5-12 */\n\
5: \tif (unlikely(size > MAX)) {\n\
6: \t\treturn NSIZES;\n\
7: \t}\n\
8:                   // 写手加的说明行（源码里没有）\n\
9: \tif (size == 0) {\n\
10: \t\treturn 0;\n\
11: \t}\n\
12: \tszind_t x = lg_floor(size);\n\
```\n";
        let (fixed, f) = check_text(doc, &mut idx);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].is_fixed(), "{f:?}");
        assert!(fixed.contains("\n5: \tif (unlikely") && fixed.contains("\n9: \tif (size == 0)"), "{fixed}");
        assert!(fixed.contains("\n8: \tif (size == 0)") == false, "{fixed}");
    }

    /// 一条语句折成两行：每一半都是源码的一段，不算改写；改写（换了变量名）才算。
    #[test]
    fn numbered_excerpt_wrapped_statement_is_not_a_rewrite_but_changed_code_is() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.c"),
            "int f(void) {\n\tint mod = (((size - 1) & mask) >> shift) &\n\t    ((1 << NG) - 1);\n\tint lg_delta = lg_base;\n\treturn mod;\n}\n",
        )
        .unwrap();
        let mut idx = SourceIndex::new(dir.path());
        let wrapped = "a.c:1-5\n```c\n1: int f(void) {\n2: \tint mod = (((size - 1) & mask) >> shift)\n3: \t    & ((1 << NG) - 1);\n4: \tint lg_delta = lg_base;\n```\n";
        assert!(check_text(wrapped, &mut idx).1.is_empty(), "折行不判错");
        let rewritten = "a.c:1-5\n```c\n1: int f(void) {\n2: \tint mod = compute_mod(size, mask);\n3: \tint lg_delta = lg_tiny_min;\n4: \treturn mod;\n```\n";
        let f = check_text(rewritten, &mut idx).1;
        assert!(f.len() == 1 && matches!(f[0].kind, FindingKind::Excerpt { .. }), "{f:?}");
        assert!(f[0].describe().contains("lg_tiny_min") && f[0].describe().contains("lg_base"), "要带上源码真实内容: {}", f[0].describe());
    }

    /// 压缩摘录（省略了一大段），行号漂得比窗口远：内容在文件里唯一就能认出来。
    #[test]
    fn numbered_excerpt_far_drift_is_resolved_by_unique_content() {
        let dir = tempfile::tempdir().unwrap();
        let mut src = String::new();
        for i in 1..=60 {
            src.push_str(&format!("int line_{i}_marker = {i};\n"));
        }
        std::fs::write(dir.path().join("b.c"), src).unwrap();
        let mut idx = SourceIndex::new(dir.path());
        let doc = "b.c:1-60\n```c\n10: int line_10_marker = 10;\n11: int line_11_marker = 11;\n12: int line_42_marker = 42;\n13: int line_43_marker = 43;\n```\n";
        let (fixed, f) = check_text(doc, &mut idx);
        assert!(f.len() == 1 && f[0].is_fixed(), "{f:?}");
        assert!(fixed.contains("\n42: int line_42_marker") && fixed.contains("\n43: int line_43_marker"), "{fixed}");
        assert!(fixed.contains("\n10: int line_10_marker"), "{fixed}");
    }

    /// 行号本来就对：一个字不动，也不产生结论。
    #[test]
    fn numbered_excerpt_correct_block_is_untouched() {
        let (_d, mut idx) = repo_with_sz();
        let doc = "```c\n// include/sz.h:3-7\n3: static inline szind_t\n4: sz_size2index_compute(size_t size) {\n5: \tif (unlikely(size > MAX)) {\n```\n";
        let (fixed, f) = check_text(doc, &mut idx);
        assert!(f.is_empty() && fixed == doc, "{f:?}\n{fixed}");
    }

    /// 摘录内容根本不是源码（改写、拼凑）：不改，判错交给写手。
    #[test]
    fn numbered_excerpt_not_from_source_is_wrong() {
        let (_d, mut idx) = repo_with_sz();
        let doc = "```c\n// include/sz.h:3-9\n\
3: if (a_made_up_condition_one) {\n\
4: \tdo_something_invented(alpha);\n\
5: \tdo_another_invented_thing(beta);\n\
6: \treturn gamma_value_invented;\n\
```\n";
        let (fixed, f) = check_text(doc, &mut idx);
        assert_eq!(f.len(), 1);
        assert!(matches!(f[0].kind, FindingKind::Excerpt { .. }) && !f[0].is_fixed(), "{f:?}");
        assert!(f[0].describe().contains("找不到"), "{}", f[0].describe());
        assert!(fixed.contains("3: if (a_made_up_condition_one)"), "判错的不改动: {fixed}");
    }

    /// 没有块头（解析不到文件）、或不是带行号的块：不判不改。
    #[test]
    fn numbered_excerpt_without_header_or_numbers_is_ignored() {
        let (_d, mut idx) = repo_with_sz();
        let no_header = "```c\n4: static inline szind_t\n5: sz_size2index_compute(size_t size) {\n```\n";
        assert!(check_text(no_header, &mut idx).1.is_empty());
        let plain = "include/sz.h:3-5\n```c\nstatic inline szind_t\nsz_size2index_compute(size_t size) {\n```\n";
        assert!(check_text(plain, &mut idx).1.is_empty());
        // 一行里 `default: foo` 之类不是行号前缀
        let sw = "include/sz.h:3-5\n```c\nswitch (x) {\ncase 1: f(); break;\ndefault: g();\n}\n```\n";
        assert!(check_text(sw, &mut idx).1.is_empty());
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
            let (fixed_text, findings) = check_text(&text, &mut idx);
            if std::env::var("LEARN_CITE_WRITE").is_ok() && fixed_text != text {
                let _ = std::fs::write(p.with_extension("fixed"), fixed_text);
            }
            for f in findings {
                if f.is_fixed() { fixed += 1 } else { wrong += 1 }
                println!("{}:{} {}", p.display(), f.doc_line, f.describe());
            }
        }
        println!("fixed={fixed} wrong={wrong}");
    }
}
