//! learn_loop 学习文档的引擎侧工具（零模型调用）：
//!
//! 1. **工作区位置**：学习过程中的全部中间产物（plan/ledger/sections/draft/
//!    code/img）落在**会话目录**下 `.latte/sessions/<sid>/learn/<slug>/`，
//!    不再直接写进项目共享的 `.latte/learn/`。只有 merge 步把定稿发布到
//!    `docs/learn/<name>.md`（`name` 去掉看板任务编号前缀）。
//! 2. **相对链接改基准**：章节文件写在 `sections/`、`sections/<id>/` 下，
//!    链接相对它们自己的目录；引擎把它们拼进上层文件时必须改写，否则
//!    `draft.md` 里的 `[k1](k1.md)` 指向不存在的 `<slug>/k1.md`。
//! 3. **memlayout 内存布局图**：mermaid 画不出「一页 4 KiB 怎么切成 8 B
//!    格子、哪个结构体字段指向哪一格」。writer 写 ```memlayout 文本块，
//!    引擎校验并渲染成 SVG，装配时替换成图片链接。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

// ─── 1. 工作区位置 ────────────────────────────────────────────────

/// 没有会话 id（独立 run、单测）时的旧位置。
pub const LEGACY_LEARN_ROOT: &str = ".latte/learn";

fn session_id_safe(sid: &str) -> bool {
    !sid.is_empty()
        && sid.len() <= 128
        && !sid.starts_with('.')
        && sid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !sid.contains("..")
}

/// 学习工作区根（相对项目 cwd）。有会话 → `.latte/sessions/<sid>/learn`。
pub fn learn_root_rel(session_id: Option<&str>) -> String {
    match session_id.map(str::trim).filter(|s| session_id_safe(s)) {
        Some(sid) => format!(".latte/sessions/{sid}/learn"),
        None => LEGACY_LEARN_ROOT.to_string(),
    }
}

/// `{{learn_root}}` 变量是否是引擎会产出的形状（防模板变量被篡改成任意路径）。
pub fn learn_root_rel_ok(rel: &str) -> bool {
    if rel == LEGACY_LEARN_ROOT {
        return true;
    }
    let parts: Vec<&str> = rel.split('/').collect();
    parts.len() == 4
        && parts[0] == ".latte"
        && parts[1] == "sessions"
        && session_id_safe(parts[2])
        && parts[3] == "learn"
}

/// 所有可能存放学习目录的根（旧位置 + 各会话），供跨会话找回进度。
pub fn all_learn_roots(cwd: &Path) -> Vec<PathBuf> {
    let mut roots = vec![cwd.join(LEGACY_LEARN_ROOT)];
    if let Ok(entries) = std::fs::read_dir(cwd.join(".latte").join("sessions")) {
        for e in entries.flatten() {
            let p = e.path().join("learn");
            if p.is_dir() {
                roots.push(p);
            }
        }
    }
    roots
}

/// 定稿文件名：去掉看板任务编号前缀（`lat-110-jemalloc-size-class` →
/// `jemalloc-size-class`）。工作区 slug 保留前缀——重跑靠它找回同一目录；
/// 交付给项目的文档按主题命名，不按任务编号。
pub fn publish_name(slug: &str) -> String {
    let slug = slug.trim();
    let b = slug.as_bytes();
    let letters = b.iter().take_while(|c| c.is_ascii_alphabetic()).count();
    if (1..=10).contains(&letters) && b.get(letters) == Some(&b'-') {
        let digits = b[letters + 1..]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .count();
        let end = letters + 1 + digits;
        if digits > 0 && b.get(end) == Some(&b'-') {
            let rest = &slug[end + 1..];
            if rest.len() >= 3 {
                return rest.to_string();
            }
        }
    }
    slug.to_string()
}

/// 递归拷贝目录（跳过 `skip` 里的名字）。目标已存在的文件被覆盖。
pub fn copy_dir(src: &Path, dst: &Path, skip: &[&str]) -> std::io::Result<usize> {
    std::fs::create_dir_all(dst)?;
    let mut n = 0;
    for e in std::fs::read_dir(src)?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if skip.iter().any(|s| *s == name) || name.ends_with(".tmp") {
            continue;
        }
        let (from, to) = (e.path(), dst.join(&name));
        if from.is_dir() {
            n += copy_dir(&from, &to, skip)?;
        } else if from.is_file() {
            std::fs::copy(&from, &to)?;
            n += 1;
        }
    }
    Ok(n)
}

// ─── 2. 相对链接改基准 ────────────────────────────────────────────

/// 本地相对目标（不是 URL / 绝对路径 / 锚点 / data:）。
fn is_local_target(t: &str) -> bool {
    let t = t.trim();
    if t.is_empty() || t.starts_with('#') || t.starts_with('/') || t.starts_with("//") {
        return false;
    }
    // scheme:（http:, mailto:, data: …）；Windows 盘符不在考虑范围。
    if let Some(i) = t.find(':') {
        let scheme = &t[..i];
        if !scheme.is_empty()
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        {
            return false;
        }
    }
    true
}

/// 拼接并折叠 `.` / `..`。越过根的 `..` 保留。
pub fn normalize_join(dir: &str, rel: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in dir.split('/').chain(rel.split('/')) {
        match seg {
            "" | "." => {}
            ".." if out.last().is_some_and(|l| *l != "..") => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    out.join("/")
}

/// `target`（相对 base 的规范路径）表达成相对 `from_dir` 的路径。
pub fn relative_to(target: &str, from_dir: &str) -> String {
    let t: Vec<&str> = target.split('/').filter(|s| !s.is_empty()).collect();
    let f: Vec<&str> = from_dir.split('/').filter(|s| !s.is_empty()).collect();
    let common = t.iter().zip(&f).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<&str> = std::iter::repeat_n("..", f.len() - common).collect();
    parts.extend(&t[common..]);
    if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    }
}

/// 扫描 markdown 正文里的 `](目标)`（链接与图片），对本地相对目标调用
/// `f(路径, 片段)`；`f` 返回 Some(新路径) 时替换（片段原样接回）。
/// 跳过围栏代码块与行内代码——代码示例里的 `](x)` 不是链接。
pub fn rewrite_local_links(text: &str, f: &mut dyn FnMut(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut fence: Option<(char, usize)> = None;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let fence_char = trimmed.chars().next().filter(|c| *c == '`' || *c == '~');
        if let Some(c) = fence_char {
            let run = trimmed.chars().take_while(|x| *x == c).count();
            if run >= 3 {
                match fence {
                    None => fence = Some((c, run)),
                    Some((fc, fr)) if fc == c && run >= fr && trimmed[run..].trim().is_empty() => {
                        fence = None
                    }
                    _ => {}
                }
                out.push_str(line);
                continue;
            }
        }
        if fence.is_some() {
            out.push_str(line);
            continue;
        }
        out.push_str(&rewrite_line(line, f));
    }
    out
}

fn rewrite_line(line: &str, f: &mut dyn FnMut(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while !rest.is_empty() {
        // 行内代码：原样拷贝到匹配的反引号串。
        let tick = rest.find('`');
        let link = rest.find("](");
        match (tick, link) {
            (Some(t), l) if l.is_none_or(|l| t < l) => {
                out.push_str(&rest[..t]);
                let after = &rest[t..];
                let run = after.chars().take_while(|c| *c == '`').count();
                let closer = "`".repeat(run);
                match after[run..].find(&closer) {
                    Some(end) => {
                        let stop = run + end + run;
                        out.push_str(&after[..stop]);
                        rest = &after[stop..];
                    }
                    None => {
                        out.push_str(after);
                        rest = "";
                    }
                }
            }
            (_, Some(l)) => {
                out.push_str(&rest[..l + 2]);
                let after = &rest[l + 2..];
                // `](<带空格的路径>)` 的 `)` 在 `>` 之后找。
                let close = match after.strip_prefix('<').and_then(|a| a.find('>')) {
                    Some(e) => after[e + 2..].find(')').map(|c| e + 2 + c),
                    None => after.find(')'),
                };
                let Some(close) = close else {
                    out.push_str(after);
                    rest = "";
                    continue;
                };
                let inner = &after[..close];
                // 目标 + 可选标题（`path "title"`）。
                let (target, tail) = match inner.trim_start().find(char::is_whitespace) {
                    Some(sp) if !inner.trim_start().starts_with('<') => {
                        let t = inner.trim_start();
                        (&t[..sp], &t[sp..])
                    }
                    _ => (inner.trim(), ""),
                };
                let (bare, angle) = match target.strip_prefix('<').and_then(|t| t.strip_suffix('>')) {
                    Some(t) => (t, true),
                    None => (target, false),
                };
                let replaced = if is_local_target(bare) {
                    let (path, frag) = match bare.find(['#', '?']) {
                        Some(i) => (&bare[..i], &bare[i..]),
                        None => (bare, ""),
                    };
                    (!path.is_empty())
                        .then(|| f(path))
                        .flatten()
                        .map(|p| if angle { format!("<{p}{frag}>") } else { format!("{p}{frag}") })
                } else {
                    None
                };
                match replaced {
                    Some(new_target) => {
                        out.push_str(&new_target);
                        out.push_str(tail);
                    }
                    None => out.push_str(inner),
                }
                out.push(')');
                rest = &after[close + 1..];
            }
            _ => {
                out.push_str(rest);
                rest = "";
            }
        }
    }
    out
}

/// 工作区里的路径是否存在（`sections/k1.md` 尚未装配、只有 `sections/k1/`
/// 分节目录时也算存在——装配顺序不该决定链接对不对）。
pub fn learn_path_exists(base: &Path, rel: &str) -> bool {
    let p = base.join(rel);
    p.exists() || (rel.ends_with(".md") && p.with_extension("").is_dir())
}

/// 把写在 `src_dir`（相对工作区 base）下的正文搬到 `out_dir` 时改写相对链接。
///
/// 解析顺序：先按 `src_dir` 解析；不存在时按 `fallbacks`（如 `sections`，
/// 章节互链约定写 `k2.md`）再试；都不存在保持按 `src_dir` 解析的结果。
pub fn rebase_links(
    text: &str,
    base: &Path,
    src_dir: &str,
    out_dir: &str,
    fallbacks: &[&str],
) -> String {
    if src_dir == out_dir && fallbacks.is_empty() {
        return text.to_string();
    }
    rewrite_local_links(text, &mut |path| {
        let primary = normalize_join(src_dir, path);
        let resolved = if learn_path_exists(base, &primary) {
            primary
        } else {
            fallbacks
                .iter()
                .map(|d| normalize_join(d, path))
                .find(|c| learn_path_exists(base, c))
                .unwrap_or(primary)
        };
        let new = relative_to(&resolved, out_dir);
        (new != path).then_some(new)
    })
}

// ─── 3. memlayout 内存布局图 ──────────────────────────────────────

const ML_MAX_LANES: usize = 6;
const ML_MAX_ITEMS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellKind {
    Data,
    Used,
    Free,
    Meta,
    Pad,
}

impl CellKind {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "data" => Self::Data,
            "used" => Self::Used,
            "free" => Self::Free,
            "meta" => Self::Meta,
            "pad" => Self::Pad,
            _ => return None,
        })
    }
    fn fill(self) -> &'static str {
        match self {
            Self::Data => "#e8eef7",
            Self::Used => "#fbd9b4",
            Self::Free => "#cdeccf",
            Self::Meta => "#c9dcf5",
            Self::Pad => "#eeeeee",
        }
    }
    fn legend(self) -> &'static str {
        match self {
            Self::Data => "数据",
            Self::Used => "已分配",
            Self::Free => "空闲",
            Self::Meta => "元数据/字段",
            Self::Pad => "填充/未用",
        }
    }
}

#[derive(Debug, Clone)]
enum Item {
    Cell {
        id: String,
        size: u64,
        kind: CellKind,
        label: String,
        offset: Option<u64>,
    },
    Gap {
        size: Option<u64>,
        label: String,
    },
}

#[derive(Debug, Clone)]
struct Lane {
    id: String,
    label: String,
    items: Vec<Item>,
}

#[derive(Debug, Clone)]
struct Ptr {
    from: (usize, usize),
    to: (usize, usize),
    label: String,
}

#[derive(Debug, Clone)]
pub struct MemLayout {
    title: String,
    lanes: Vec<Lane>,
    ptrs: Vec<Ptr>,
}

/// `8` / `8B` / `4K` / `4KiB` / `4KB` / `2M` / `0x1000`。
fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return u64::from_str_radix(&hex.replace('_', ""), 16).ok();
    }
    let digits = s.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let n: u64 = s[..digits].parse().ok()?;
    let mult = match s[digits..].to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        _ => return None,
    };
    n.checked_mul(mult)
}

fn fmt_size(n: u64) -> String {
    for (shift, unit) in [(30, "GiB"), (20, "MiB"), (10, "KiB")] {
        if n >= 1 << shift && n % (1 << shift) == 0 {
            return format!("{} {unit}", n >> shift);
        }
    }
    format!("{n} B")
}

/// 去掉一层成对引号。
fn unquote(s: &str) -> String {
    let t = s.trim();
    for q in ['"', '\'', '「'] {
        let close = if q == '「' { '」' } else { q };
        if t.len() >= 2 && t.starts_with(q) && t.ends_with(close) {
            return t[q.len_utf8()..t.len() - close.len_utf8()].trim().to_string();
        }
    }
    t.to_string()
}

fn ident_ok(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// 语法（每行一条指令，`#` 开头为注释）：
///
/// ```text
/// title 一个 4 KiB slab 页切成 512 个 8 B region
/// lane page "slab 页（4 KiB）"
/// cell r0 8 used "region[0]"
/// cell r1 8 free "region[1]"
/// gap 4072 "region[2..510]"
/// cell r511 8 used "region[511]" @4088
/// lane meta "edata_t（slab 元数据）"
/// cell addr 8 meta "e_addr"
/// cell bm 64 meta "bitmap"
/// ptr meta.addr -> page.r0 "指向页首"
/// ```
pub fn parse_memlayout(src: &str) -> Result<MemLayout, Vec<String>> {
    let mut errs = Vec::new();
    let mut title = String::new();
    let mut lanes: Vec<Lane> = Vec::new();
    let mut raw_ptrs: Vec<(usize, String, String, String)> = Vec::new();
    for (i, raw) in src.lines().enumerate() {
        let ln = i + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
            continue;
        }
        let (kw, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim();
        match kw {
            "title" => title = unquote(rest),
            "lane" => {
                let (id, label) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                if !ident_ok(id) {
                    errs.push(format!("第 {ln} 行 lane 的 id「{id}」只能用字母数字下划线"));
                    continue;
                }
                if lanes.iter().any(|l| l.id == id) {
                    errs.push(format!("第 {ln} 行 lane id「{id}」重复"));
                }
                let label = unquote(label);
                lanes.push(Lane {
                    id: id.to_string(),
                    label: if label.is_empty() { id.to_string() } else { label },
                    items: Vec::new(),
                });
            }
            "cell" => {
                let Some(lane) = lanes.last_mut() else {
                    errs.push(format!("第 {ln} 行 cell 之前要先写 lane"));
                    continue;
                };
                let mut toks = rest.splitn(3, char::is_whitespace);
                let id = toks.next().unwrap_or("");
                let size_tok = toks.next().unwrap_or("");
                let mut tail = toks.next().unwrap_or("").trim().to_string();
                if !ident_ok(id) {
                    errs.push(format!("第 {ln} 行 cell 的 id「{id}」只能用字母数字下划线"));
                    continue;
                }
                let Some(size) = parse_size(size_tok) else {
                    errs.push(format!(
                        "第 {ln} 行 cell {id} 的大小「{size_tok}」不合法（写 8 / 64B / 4K / 0x1000）"
                    ));
                    continue;
                };
                let mut kind = CellKind::Data;
                let (first, more) = {
                    let (f, m) = tail.split_once(char::is_whitespace).unwrap_or((tail.as_str(), ""));
                    (f.to_string(), m.trim().to_string())
                };
                if let Some(k) = CellKind::parse(&first) {
                    kind = k;
                    tail = more;
                }
                let mut offset = None;
                if let Some(at) = tail.rfind(" @").map(|i| i + 1).or_else(|| tail.starts_with('@').then_some(0)) {
                    match parse_size(&tail[at + 1..]) {
                        Some(o) => {
                            offset = Some(o);
                            tail = tail[..at].trim().to_string();
                        }
                        None => errs.push(format!("第 {ln} 行 cell {id} 的偏移「{}」不合法", &tail[at..])),
                    }
                }
                if lane.items.iter().any(|it| matches!(it, Item::Cell { id: x, .. } if x == id)) {
                    errs.push(format!("第 {ln} 行 cell id「{}.{id}」重复", lane.id));
                }
                let label = unquote(&tail);
                lane.items.push(Item::Cell {
                    id: id.to_string(),
                    size,
                    kind,
                    label: if label.is_empty() { id.to_string() } else { label },
                    offset,
                });
            }
            "gap" => {
                let Some(lane) = lanes.last_mut() else {
                    errs.push(format!("第 {ln} 行 gap 之前要先写 lane"));
                    continue;
                };
                let (first, more) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                let (size, label) = match parse_size(first) {
                    Some(s) => (Some(s), unquote(more)),
                    None => (None, unquote(rest)),
                };
                lane.items.push(Item::Gap {
                    size,
                    label: if label.is_empty() { "…".into() } else { label },
                });
            }
            "ptr" => {
                let Some((from, after)) = rest.split_once("->") else {
                    errs.push(format!("第 {ln} 行 ptr 要写成 `ptr 行.格 -> 行.格 \"说明\"`"));
                    continue;
                };
                let after = after.trim();
                let (to, label) = after.split_once(char::is_whitespace).unwrap_or((after, ""));
                raw_ptrs.push((ln, from.trim().to_string(), to.trim().to_string(), unquote(label)));
            }
            other => errs.push(format!(
                "第 {ln} 行未知指令「{other}」（只认 title / lane / cell / gap / ptr）"
            )),
        }
    }
    if lanes.is_empty() {
        errs.push("没有任何 lane（一行内存 = 一个 lane）".into());
    }
    if lanes.len() > ML_MAX_LANES {
        errs.push(format!("lane 有 {} 个，最多 {ML_MAX_LANES} 个：拆成两张图", lanes.len()));
    }
    for l in &lanes {
        if !l.items.iter().any(|it| matches!(it, Item::Cell { .. })) {
            errs.push(format!("lane {} 没有 cell", l.id));
        }
        if l.items.len() > ML_MAX_ITEMS {
            errs.push(format!(
                "lane {} 有 {} 个格子，最多 {ML_MAX_ITEMS} 个：重复的格子用 `gap <大小> \"说明\"` 省略",
                l.id,
                l.items.len()
            ));
        }
    }
    let resolve = |r: &str| -> Option<(usize, usize)> {
        let (lane, cell) = r.split_once('.')?;
        let li = lanes.iter().position(|l| l.id == lane)?;
        let ci = lanes[li]
            .items
            .iter()
            .position(|it| matches!(it, Item::Cell { id, .. } if id == cell))?;
        Some((li, ci))
    };
    let mut ptrs = Vec::new();
    for (ln, from, to, label) in raw_ptrs {
        match (resolve(&from), resolve(&to)) {
            (Some(f), Some(t)) => ptrs.push(Ptr { from: f, to: t, label }),
            (f, t) => {
                for (r, ok) in [(&from, f.is_some()), (&to, t.is_some())] {
                    if !ok {
                        errs.push(format!("第 {ln} 行 ptr 端点「{r}」不存在（写成 lane.cell）"));
                    }
                }
            }
        }
    }
    if errs.is_empty() {
        Ok(MemLayout { title, lanes, ptrs })
    } else {
        Err(errs)
    }
}

/// 偏移连续性（写作期检查，渲染不拦）：见函数体注释。
fn memlayout_offset_problems(ml: &MemLayout) -> Vec<String> {
    // 偏移连续性：`@偏移` 只许用来**核对**位置，不许跳过或重叠——跳过的
    // 字节在图上不可见（实测：tab[2] 之后直接 `@8`，中间 5 字节凭空消失），
    // 重叠说明大小算错了。要省略就写 `gap`。
    let mut errs = Vec::new();
    for l in &ml.lanes {
        let mut cursor: Option<u64> = Some(0);
        for it in &l.items {
            match it {
                Item::Cell { id, size, offset, .. } => {
                    if let (Some(o), Some(c)) = (offset, cursor) {
                        if *o > c {
                            errs.push(format!(
                                "lane {} 的 {id} 标了 @{o}，但前面的格子只排到 +{c}：中间 {} B 用 `gap {} \"说明\"` 写出来",
                                l.id, o - c, o - c
                            ));
                        } else if *o < c {
                            errs.push(format!(
                                "lane {} 的 {id} 标了 @{o}，但前面的格子已排到 +{c}：大小加起来超了 {} B，检查前面各格的大小",
                                l.id, c - o
                            ));
                        }
                    }
                    cursor = offset.or(cursor).and_then(|c| c.checked_add(*size));
                }
                Item::Gap { size, .. } => {
                    cursor = match (cursor, size) {
                        (Some(c), Some(s)) => c.checked_add(*s),
                        _ => None,
                    };
                }
            }
        }
    }
    errs
}

/// 文本里「= 28 KiB」「＝ 4096 B」这类**声明的大小**：只认 `=` / `＝` 后面紧跟
/// 「数字 + 单位」。返回 (原文, 字节数)。
///
/// 故意收得很窄：图的标题/行说明里**写了等号**就是在断言「这一行总共这么大」，
/// 可以拿去和格子之和对账；没有等号的「14 KiB 对象」是在描述单个格子，不是总数。
/// 小数（`1.5 KiB`）不认——宁可漏判，不把别的数字误读成总数。
fn declared_sizes(text: &str) -> Vec<(String, u64)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '=' && chars[i] != '＝' {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }
        let ds = j;
        while j < chars.len() && chars[j].is_ascii_digit() {
            j += 1;
        }
        if j == ds || (j < chars.len() && chars[j] == '.') {
            i += 1;
            continue;
        }
        let digits: String = chars[ds..j].iter().collect();
        while j < chars.len() && chars[j] == ' ' {
            j += 1;
        }
        let us = j;
        while j < chars.len() && chars[j].is_ascii_alphabetic() {
            j += 1;
        }
        let mut unit: String = chars[us..j].iter().collect::<String>().to_ascii_lowercase();
        if unit.is_empty() && chars.get(j..j + 2) == Some(&['字', '节'][..]) {
            unit = "b".into();
            j += 2;
        }
        let mult: u64 = match unit.as_str() {
            "b" | "byte" | "bytes" => 1,
            "k" | "kb" | "kib" => 1 << 10,
            "m" | "mb" | "mib" => 1 << 20,
            "g" | "gb" | "gib" => 1 << 30,
            _ => {
                i += 1;
                continue;
            }
        };
        if let Some(bytes) = digits.parse::<u64>().ok().and_then(|n| n.checked_mul(mult)) {
            let raw: String = chars[ds..j].iter().collect();
            out.push((raw.trim().to_string(), bytes));
        }
        i = j.max(i + 1);
    }
    out
}

/// 一行的总字节数；有未标大小的 `gap` 就算不出来。
fn lane_total(l: &Lane) -> Option<u64> {
    l.items.iter().try_fold(0u64, |acc, it| match it {
        Item::Cell { size, .. } => acc.checked_add(*size),
        Item::Gap { size: Some(s), .. } => acc.checked_add(*s),
        Item::Gap { size: None, .. } => None,
    })
}

/// 图自己的说法 vs 图里的格子：写作期检查，拦**图文不一致**。
///
/// 实测事故（jemalloc size class 学习文档）：总览里的 slab 布局图，行说明写着
/// 「7 个 4KB 页 = 28 KiB」，格子却是「1 KiB 头部元数据 + 14 KiB + 14 KiB」=
/// 29 KiB——头部是凭空多画的（jemalloc 的 slab 元数据在带外的 `edata_t` 里，
/// 数据页就是 `nregs × reg_size` 一整块）。语法和 `@偏移` 检查都过了，因为格子
/// 自己加起来是连续的；只有拿说明里声明的总数去对账才露馅。两类检查：
///
/// 1. **声明的总大小必须等于格子之和**：行说明里的 `= N KiB` 对这一行负责；
///    标题里的 `= N KiB` 至少要等于某一行的总长。
/// 2. **数据格子和元数据格子画在同一行 = 断言「元数据内嵌在数据区里」**。
///    这是最容易画错的地方（分配器常把元数据放带外的描述符里），图上看不出
///    对错，所以要求行说明里给出支撑它的源码出处 `file:line`，或者把元数据
///    拆成独立的 lane、用 `ptr` 指过去。
fn memlayout_claim_problems(ml: &MemLayout) -> Vec<String> {
    let mut errs = Vec::new();
    for l in &ml.lanes {
        let total = lane_total(l);
        if let Some(total) = total {
            for (raw, bytes) in declared_sizes(&l.label) {
                if bytes != total {
                    let (more, d) = if total > bytes { ("多", total - bytes) } else { ("少", bytes - total) };
                    errs.push(format!(
                        "lane {} 的说明写「= {raw}」（{bytes} B），但它的格子加起来是 {total} B（{}），{more}了 {d} B：\
                         先回源码确认这块内存里到底有哪些格子——多出来/缺少的那部分是不是想当然画上去的（如并不内嵌的头部/元数据）？\
                         再改说明或改格子",
                        l.id,
                        fmt_size(total),
                    ));
                }
            }
        }
        let kinds = |pred: fn(CellKind) -> bool| {
            l.items.iter().any(|it| matches!(it, Item::Cell { kind, .. } if pred(*kind)))
        };
        let has_meta = kinds(|k| matches!(k, CellKind::Meta));
        let has_data = kinds(|k| matches!(k, CellKind::Used | CellKind::Free | CellKind::Data));
        if has_meta && has_data && !has_file_line(&l.label) {
            errs.push(format!(
                "lane {} 把元数据格子（meta）和数据格子（used/free/data）画在同一行，等于断言「元数据内嵌在数据区里」，\
                 却没给出处：在 lane 说明里写出支撑它的源码位置 `file:line`（如地址是怎么算出来的那一行）；\
                 如果元数据其实在别的结构里（带外描述符），把它拆成独立 lane，用 `ptr` 指向数据格",
                l.id
            ));
        }
    }
    let totals: Vec<u64> = ml.lanes.iter().filter_map(lane_total).collect();
    if totals.len() == ml.lanes.len() && !totals.is_empty() {
        for (raw, bytes) in declared_sizes(&ml.title) {
            if !totals.contains(&bytes) {
                errs.push(format!(
                    "标题写「= {raw}」（{bytes} B），但没有任何一行的格子加起来等于它（各行总长：{}）：标题和格子对不上",
                    totals.iter().map(|t| fmt_size(*t)).collect::<Vec<_>>().join("、")
                ));
            }
        }
    }
    errs
}

/// 文本里有没有 `name.ext:行号` 形式的出处（`src/arena.c:236`、`sz.h:191-197`）。
fn has_file_line(text: &str) -> bool {
    let b = text.as_bytes();
    (0..b.len()).any(|i| {
        b[i] == b':'
            && i > 0
            && b.get(i + 1).is_some_and(|c| c.is_ascii_digit())
            && text[..i]
                .rsplit(|c: char| c.is_whitespace() || "（(`,，".contains(c))
                .next()
                .is_some_and(|w| {
                    w.rsplit_once('.')
                        .is_some_and(|(stem, ext)| !stem.is_empty() && !ext.is_empty() && ext.chars().all(|c| c.is_ascii_alphanumeric()))
                })
    })
}

/// 正文里的 ```memlayout 块：(块起始行号, 源码, 原文切片的字节区间)。
fn memlayout_spans(text: &str) -> Vec<(usize, String, std::ops::Range<usize>)> {
    let mut out = Vec::new();
    let mut pos = 0;
    let mut open: Option<(usize, usize, usize, String)> = None; // (行号, 起始字节, 围栏长, 源)
    let mut other_fence: Option<usize> = None;
    for (i, line) in text.split_inclusive('\n').enumerate() {
        let start = pos;
        pos += line.len();
        let t = line.trim();
        let run = t.chars().take_while(|c| *c == '`').count();
        if let Some((ln, s, fl, ref mut src)) = open {
            if run >= fl && t[run..].trim().is_empty() {
                out.push((ln, std::mem::take(src), s..pos));
                open = None;
            } else {
                src.push_str(line);
            }
            continue;
        }
        if let Some(fl) = other_fence {
            if run >= fl && t[run..].trim().is_empty() {
                other_fence = None;
            }
            continue;
        }
        if run >= 3 {
            if t[run..].trim().eq_ignore_ascii_case("memlayout") {
                open = Some((i + 1, start, run, String::new()));
            } else {
                other_fence = Some(run);
            }
        }
    }
    out
}

pub fn memlayout_blocks(text: &str) -> usize {
    memlayout_spans(text).len()
}

/// 每个 memlayout 块的语法问题（写作阶段检查用，带回给 writer 重试）。
pub fn lint_memlayout_blocks(text: &str) -> Vec<String> {
    memlayout_spans(text)
        .into_iter()
        .filter_map(|(ln, src, _)| {
            let errs = match parse_memlayout(&src) {
                Ok(ml) => {
                    let mut e = memlayout_offset_problems(&ml);
                    e.extend(memlayout_claim_problems(&ml));
                    e
                }
                Err(e) => e,
            };
            (!errs.is_empty())
                .then(|| format!("第 {ln} 行起的 memlayout 图不合法：{}", errs.join("；")))
        })
        .collect()
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 估算文字宽度（12px 字号：CJK ≈ 12，ASCII ≈ 7）。
fn text_width(s: &str, px: f64) -> f64 {
    s.chars()
        .map(|c| if c.is_ascii() { px * 0.58 } else { px })
        .sum()
}

fn fit_text(s: &str, px: f64, max_w: f64) -> String {
    if text_width(s, px) <= max_w {
        return s.to_string();
    }
    let mut out = String::new();
    for c in s.chars() {
        let next = format!("{out}{c}…");
        if text_width(&next, px) > max_w {
            break;
        }
        out.push(c);
    }
    format!("{out}…")
}

struct CellBox {
    x: f64,
    w: f64,
}

/// 渲染成独立 SVG（白底、自带字体回退，可直接 `<img>` 或 GitHub 预览）。
pub fn render_memlayout_svg(ml: &MemLayout) -> String {
    const W: f64 = 900.0;
    const MARGIN: f64 = 24.0;
    const CELL_H: f64 = 46.0;
    const LANE_LABEL_H: f64 = 22.0;
    const OFFSET_H: f64 = 18.0;
    const LANE_GAP: f64 = 64.0;
    let title_h = if ml.title.is_empty() { 8.0 } else { 36.0 };
    let lane_h = LANE_LABEL_H + CELL_H + OFFSET_H;
    let kinds: Vec<CellKind> = {
        let mut seen = Vec::new();
        for l in &ml.lanes {
            for it in &l.items {
                if let Item::Cell { kind, .. } = it {
                    if !seen.contains(kind) {
                        seen.push(*kind);
                    }
                }
            }
        }
        seen
    };
    let legend_h = 30.0;
    let h = title_h + ml.lanes.len() as f64 * lane_h + (ml.lanes.len().saturating_sub(1)) as f64 * LANE_GAP
        + legend_h
        + MARGIN;

    let mut s = String::new();
    s.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{W}\" height=\"{h:.0}\" viewBox=\"0 0 {W} {h:.0}\" \
         font-family=\"-apple-system,'PingFang SC','Microsoft YaHei','Noto Sans CJK SC',sans-serif\">\n"
    ));
    s.push_str("<defs><marker id=\"ah\" viewBox=\"0 0 10 10\" refX=\"9\" refY=\"5\" markerWidth=\"7\" markerHeight=\"7\" orient=\"auto-start-reverse\"><path d=\"M0,0 L10,5 L0,10 z\" fill=\"#c0392b\"/></marker></defs>\n");
    s.push_str(&format!("<rect width=\"{W}\" height=\"{h:.0}\" fill=\"#ffffff\"/>\n"));
    if !ml.title.is_empty() {
        s.push_str(&format!(
            "<text x=\"{MARGIN}\" y=\"24\" font-size=\"15\" font-weight=\"600\" fill=\"#222\">{}</text>\n",
            xml_escape(&fit_text(&ml.title, 15.0, W - 2.0 * MARGIN))
        ));
    }

    // 每个 lane 的格子坐标：宽度按 log2(大小) 加权，gap 固定权重，铺满整行。
    let avail = W - 2.0 * MARGIN;
    let mut boxes: Vec<Vec<CellBox>> = Vec::new();
    let lane_y = |li: usize| title_h + li as f64 * (lane_h + LANE_GAP);
    for lane in &ml.lanes {
        let weights: Vec<f64> = lane
            .items
            .iter()
            .map(|it| match it {
                Item::Cell { size, .. } => 1.0 + ((*size).max(1) as f64).log2() / 4.0,
                Item::Gap { .. } => 2.2,
            })
            .collect();
        let total: f64 = weights.iter().sum();
        let mut x = MARGIN;
        let row = weights
            .iter()
            .map(|w| {
                let width = avail * w / total;
                let b = CellBox { x, w: width };
                x += width;
                b
            })
            .collect();
        boxes.push(row);
    }

    for (li, lane) in ml.lanes.iter().enumerate() {
        let y0 = lane_y(li);
        let cy = y0 + LANE_LABEL_H;
        s.push_str(&format!(
            "<text x=\"{MARGIN}\" y=\"{:.1}\" font-size=\"13\" font-weight=\"600\" fill=\"#333\">{}</text>\n",
            y0 + 15.0,
            xml_escape(&fit_text(&lane.label, 13.0, avail))
        ));
        let mut cursor: Option<u64> = Some(0);
        for (ii, it) in lane.items.iter().enumerate() {
            let b = &boxes[li][ii];
            match it {
                Item::Cell { size, kind, label, offset, .. } => {
                    if let Some(o) = offset {
                        cursor = Some(*o);
                    }
                    s.push_str(&format!(
                        "<rect x=\"{:.1}\" y=\"{cy:.1}\" width=\"{:.1}\" height=\"{CELL_H}\" fill=\"{}\" stroke=\"#555\" stroke-width=\"1\"/>\n",
                        b.x, b.w, kind.fill()
                    ));
                    s.push_str(&format!(
                        "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"12\" text-anchor=\"middle\" fill=\"#222\">{}</text>\n",
                        b.x + b.w / 2.0,
                        cy + 20.0,
                        xml_escape(&fit_text(label, 12.0, b.w - 6.0))
                    ));
                    s.push_str(&format!(
                        "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"10\" text-anchor=\"middle\" fill=\"#666\">{}</text>\n",
                        b.x + b.w / 2.0,
                        cy + 36.0,
                        xml_escape(&fit_text(&fmt_size(*size), 10.0, b.w - 4.0))
                    ));
                    if let Some(c) = cursor {
                        s.push_str(&format!(
                            "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"10\" fill=\"#888\">+{c}</text>\n",
                            b.x + 2.0,
                            cy + CELL_H + 13.0
                        ));
                    }
                    cursor = cursor.and_then(|c| c.checked_add(*size));
                }
                Item::Gap { size, label } => {
                    s.push_str(&format!(
                        "<rect x=\"{:.1}\" y=\"{cy:.1}\" width=\"{:.1}\" height=\"{CELL_H}\" fill=\"#fafafa\" stroke=\"#999\" stroke-dasharray=\"4 3\"/>\n",
                        b.x, b.w
                    ));
                    s.push_str(&format!(
                        "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"11\" text-anchor=\"middle\" fill=\"#555\">{}</text>\n",
                        b.x + b.w / 2.0,
                        cy + 20.0,
                        xml_escape(&fit_text(label, 11.0, b.w - 6.0))
                    ));
                    if let Some(sz) = size {
                        s.push_str(&format!(
                            "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"10\" text-anchor=\"middle\" fill=\"#888\">… {} …</text>\n",
                            b.x + b.w / 2.0,
                            cy + 36.0,
                            xml_escape(&fmt_size(*sz))
                        ));
                    }
                    cursor = match (cursor, size) {
                        (Some(c), Some(sz)) => c.checked_add(*sz),
                        _ => None,
                    };
                }
            }
        }
        if let (Some(c), Some(last)) = (cursor, boxes[li].last()) {
            s.push_str(&format!(
                "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"10\" text-anchor=\"end\" fill=\"#888\">+{c}</text>\n",
                last.x + last.w,
                cy + CELL_H + 13.0
            ));
        }
    }

    // 指针：源格 → 目标格。跨行走竖向贝塞尔；同一行从格子上方拱过去。
    for (pi, p) in ml.ptrs.iter().enumerate() {
        let (fb, tb) = (&boxes[p.from.0][p.from.1], &boxes[p.to.0][p.to.1]);
        let (fy0, ty0) = (lane_y(p.from.0) + LANE_LABEL_H, lane_y(p.to.0) + LANE_LABEL_H);
        let fx = fb.x + fb.w / 2.0 + (pi % 3) as f64 * 4.0 - 4.0;
        let tx = tb.x + tb.w / 2.0;
        let (d, lx, ly) = if p.from.0 == p.to.0 {
            let y = fy0;
            let top = y - LANE_LABEL_H + 2.0;
            (
                format!("M{fx:.1},{y:.1} C{fx:.1},{top:.1} {tx:.1},{top:.1} {tx:.1},{y:.1}"),
                (fx + tx) / 2.0,
                top + 4.0,
            )
        } else if p.from.0 < p.to.0 {
            let (y1, y2) = (fy0 + CELL_H, ty0);
            let my = (y1 + y2) / 2.0;
            (
                format!("M{fx:.1},{y1:.1} C{fx:.1},{my:.1} {tx:.1},{my:.1} {tx:.1},{y2:.1}"),
                (fx + tx) / 2.0,
                my - 3.0 + (pi % 2) as f64 * 12.0,
            )
        } else {
            let (y1, y2) = (fy0, ty0 + CELL_H);
            let my = (y1 + y2) / 2.0;
            (
                format!("M{fx:.1},{y1:.1} C{fx:.1},{my:.1} {tx:.1},{my:.1} {tx:.1},{y2:.1}"),
                (fx + tx) / 2.0,
                my - 3.0 + (pi % 2) as f64 * 12.0,
            )
        };
        s.push_str(&format!(
            "<circle cx=\"{fx:.1}\" cy=\"{:.1}\" r=\"3\" fill=\"#c0392b\"/>\n",
            if p.from.0 > p.to.0 || p.from.0 == p.to.0 { fy0 } else { fy0 + CELL_H }
        ));
        s.push_str(&format!(
            "<path d=\"{d}\" fill=\"none\" stroke=\"#c0392b\" stroke-width=\"1.6\" marker-end=\"url(#ah)\"/>\n"
        ));
        if !p.label.is_empty() {
            let label = fit_text(&p.label, 11.0, 260.0);
            let w = text_width(&label, 11.0) + 8.0;
            s.push_str(&format!(
                "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{w:.1}\" height=\"15\" rx=\"3\" fill=\"#fff\" fill-opacity=\"0.92\" stroke=\"#e6b0aa\"/>\n",
                lx - w / 2.0,
                ly - 11.0
            ));
            s.push_str(&format!(
                "<text x=\"{lx:.1}\" y=\"{ly:.1}\" font-size=\"11\" text-anchor=\"middle\" fill=\"#922b21\">{}</text>\n",
                xml_escape(&label)
            ));
        }
    }

    // 图例。
    let mut lx = MARGIN;
    let ly = h - MARGIN - 4.0;
    for k in kinds {
        s.push_str(&format!(
            "<rect x=\"{lx:.1}\" y=\"{:.1}\" width=\"14\" height=\"12\" fill=\"{}\" stroke=\"#555\"/>\n",
            ly - 10.0,
            k.fill()
        ));
        s.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{ly:.1}\" font-size=\"11\" fill=\"#444\">{}</text>\n",
            lx + 18.0,
            k.legend()
        ));
        lx += 24.0 + text_width(k.legend(), 11.0) + 14.0;
    }
    if !ml.ptrs.is_empty() {
        s.push_str(&format!(
            "<path d=\"M{lx:.1},{:.1} L{:.1},{:.1}\" stroke=\"#c0392b\" stroke-width=\"1.6\" marker-end=\"url(#ah)\"/>\n",
            ly - 4.0,
            lx + 26.0,
            ly - 4.0
        ));
        s.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{ly:.1}\" font-size=\"11\" fill=\"#444\">指针 / 地址引用</text>\n",
            lx + 32.0
        ));
    }
    s.push_str("</svg>\n");
    s
}

fn fnv64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// 把合法的 ```memlayout 块渲染成 `img_dir/ml-<hash>.svg`，正文里换成
/// `![标题](<img_rel>/ml-<hash>.svg)`。不合法的块原样保留（写作阶段已拦过；
/// 漏网的至少还能当文本读）。文件名取源码哈希：内容不变就不重写。
pub fn render_memlayout_blocks(text: &str, img_dir: &Path, img_rel: &str) -> String {
    let spans = memlayout_spans(text);
    if spans.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for (_, src, range) in spans {
        let Ok(ml) = parse_memlayout(&src) else {
            continue;
        };
        let name = format!("ml-{:016x}.svg", fnv64(&src));
        let path = img_dir.join(&name);
        if !path.is_file() {
            if std::fs::create_dir_all(img_dir).is_err() {
                continue;
            }
            if std::fs::write(&path, render_memlayout_svg(&ml)).is_err() {
                continue;
            }
        }
        out.push_str(&text[last..range.start]);
        let alt = if ml.title.is_empty() { "内存布局图".to_string() } else { ml.title.replace(['[', ']'], "") };
        let rel = if img_rel.is_empty() { name } else { format!("{img_rel}/{name}") };
        out.push_str(&format!("![{alt}]({rel})\n"));
        last = range.end;
    }
    out.push_str(&text[last..]);
    out
}

// ─── 4. 定稿发布 ──────────────────────────────────────────────────

/// 资源目录里的标记文件：有它才允许清空重建（不误删用户自己的同名目录）。
const ASSET_MARKER: &str = ".latte-learn-assets";

/// 可发布的工作区子目录。
const PUBLISH_DIRS: &[&str] = &["sections", "code", "img"];

/// 本地链接目标（相对 `dir`）规范化成相对工作区 base 的路径；只认
/// `sections/` `code/` `img/` 下真实存在的文件。
fn publishable_target(base: &Path, dir: &str, path: &str) -> Option<String> {
    let p = normalize_join(dir, path);
    let top = p.split('/').next().unwrap_or("");
    (PUBLISH_DIRS.contains(&top) && base.join(&p).is_file()).then_some(p)
}

/// 定稿正文（链接相对工作区 base）发布到 `docs/learn/`：
/// 只拷**被引用到的文件及其传递闭包**（章节 md 里再链到的章节/图/副本）到
/// `docs/learn/<name>/`，链接改指 `<name>/…`。不整目录拷：工作区跨会话沿用，
/// 里面留着旧 plan 的章节（实测 LAT-110：plan 从 7 点变 4 点后 k5–k7、appendix
/// 还在 sections/ 里），整目录拷会把它们一起发布出去。
/// 返回 (改写后的正文, 拷贝的文件数)。
pub fn publish_assets(
    body: &str,
    base: &Path,
    docs_dir: &Path,
    name: &str,
) -> std::io::Result<(String, usize)> {
    let mut files: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let text = rewrite_local_links(body, &mut |path| {
        let p = publishable_target(base, "", path)?;
        files.insert(p.clone());
        Some(format!("{name}/{p}"))
    });
    // 传递闭包：被发布的 md 里的相对链接（相对它自己的目录）。
    let mut queue: Vec<String> = files.iter().filter(|f| f.ends_with(".md")).cloned().collect();
    while let Some(md) = queue.pop() {
        let Ok(src) = std::fs::read_to_string(base.join(&md)) else {
            continue;
        };
        let dir = md.rsplit_once('/').map_or("", |(d, _)| d).to_string();
        rewrite_local_links(&src, &mut |path| {
            if let Some(p) = publishable_target(base, &dir, path) {
                if files.insert(p.clone()) && p.ends_with(".md") {
                    queue.push(p);
                }
            }
            None
        });
    }
    if files.is_empty() {
        return Ok((text, 0));
    }
    let assets = docs_dir.join(name);
    if assets.join(ASSET_MARKER).is_file() {
        let _ = std::fs::remove_dir_all(&assets);
    }
    std::fs::create_dir_all(&assets)?;
    std::fs::write(
        assets.join(ASSET_MARKER),
        "learn_loop 定稿的资源目录（章节、注释副本、布局图），每次定稿整体重建。\n",
    )?;
    for f in &files {
        let dst = assets.join(f);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(base.join(f), dst)?;
    }
    Ok((text, files.len()))
}

/// 删掉 `img/` 下不再被任何文档引用的 `ml-*.svg`（memlayout 改过一次就会留下
/// 旧哈希的图）。只动引擎生成的 `ml-` 前缀文件，别的图不碰。
pub fn prune_memlayout_images(base: &Path, referenced_by: &[&str]) -> usize {
    let Ok(entries) = std::fs::read_dir(base.join("img")) else {
        return 0;
    };
    let mut n = 0;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with("ml-")
            && name.ends_with(".svg")
            && !referenced_by.iter().any(|t| t.contains(&name))
            && std::fs::remove_file(e.path()).is_ok()
        {
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn learn_root_uses_session_dir_and_rejects_unsafe_ids() {
        assert_eq!(learn_root_rel(Some("ui-1-2-0")), ".latte/sessions/ui-1-2-0/learn");
        assert_eq!(learn_root_rel(None), ".latte/learn");
        assert_eq!(learn_root_rel(Some("")), ".latte/learn");
        assert_eq!(learn_root_rel(Some("../x")), ".latte/learn");
        assert_eq!(learn_root_rel(Some("a/b")), ".latte/learn");
        assert!(learn_root_rel_ok(".latte/sessions/ui-1/learn"));
        assert!(learn_root_rel_ok(".latte/learn"));
        assert!(!learn_root_rel_ok(".latte/sessions/../learn"));
        assert!(!learn_root_rel_ok("docs"));
    }

    #[test]
    fn publish_name_strips_task_prefix_only() {
        assert_eq!(publish_name("lat-110-jemalloc-size-class"), "jemalloc-size-class");
        assert_eq!(publish_name("rust-ownership"), "rust-ownership");
        assert_eq!(publish_name("lat-110"), "lat-110");
        assert_eq!(publish_name("lat-110-ab"), "lat-110-ab");
        assert_eq!(publish_name("x86-64-abi"), "x86-64-abi");
    }

    #[test]
    fn relative_path_math() {
        assert_eq!(normalize_join("sections/k1", "../k2.md"), "sections/k2.md");
        assert_eq!(normalize_join("", "../x"), "../x");
        assert_eq!(relative_to("sections/k2.md", ""), "sections/k2.md");
        assert_eq!(relative_to("img/a.svg", "sections"), "../img/a.svg");
        assert_eq!(relative_to("sections/k2.md", "sections"), "k2.md");
    }

    #[test]
    fn rewrite_skips_code_urls_and_anchors() {
        let text = "见 [k1](k1.md#x) 与 ![图](a.png \"t\")\n`[no](k1.md)` [web](https://a.b/c.md) [top](#h)\n```md\n[code](k1.md)\n```\n";
        let out = rewrite_local_links(text, &mut |p| Some(format!("S/{p}")));
        assert!(out.contains("[k1](S/k1.md#x)"), "{out}");
        assert!(out.contains("![图](S/a.png \"t\")"), "{out}");
        assert!(out.contains("`[no](k1.md)`"), "{out}");
        assert!(out.contains("(https://a.b/c.md)"), "{out}");
        assert!(out.contains("[top](#h)"), "{out}");
        assert!(out.contains("[code](k1.md)\n```"), "{out}");
    }

    #[test]
    fn rebase_prefers_existing_target_then_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        std::fs::create_dir_all(base.join("sections/k1")).unwrap();
        std::fs::create_dir_all(base.join("sections/k2")).unwrap();
        // k1/9_summary.md 写 `(k2.md)`（约定指章节文件）：sections/k1/k2.md 不存在，
        // 落到 sections/k2.md（只有分节目录也算存在）。
        let out = rebase_links("[下一章](k2.md)", base, "sections/k1", "", &["sections"]);
        assert_eq!(out, "[下一章](sections/k2.md)");
        let out = rebase_links("[k1](k1.md)", base, "sections", "", &[]);
        assert_eq!(out, "[k1](sections/k1.md)");
        // 不存在的目标按源目录解析。
        let out = rebase_links("[x](nope.md)", base, "sections", "", &[]);
        assert_eq!(out, "[x](sections/nope.md)");
    }

    const SLAB: &str = r#"
title 一个 4 KiB slab 页切成 512 个 8 B region
lane page "slab 页（4 KiB）"
cell r0 8 used "region[0]"
cell r1 8 free "region[1]"
gap 4072 "region[2..510]"
cell r511 8 used "region[511]" @4088
lane meta "edata_t（slab 元数据）"
cell addr 8 meta "e_addr"
cell nfree 4 meta "nfree=255"
cell bm 64 meta "bitmap[8]"
ptr meta.addr -> page.r0 "e_addr = 页首地址"
ptr meta.bm -> page.r1 "bit1=1 → 空闲"
"#;

    #[test]
    fn memlayout_parses_and_renders_svg() {
        let ml = parse_memlayout(SLAB).expect("parse");
        assert_eq!(ml.lanes.len(), 2);
        assert_eq!(ml.ptrs.len(), 2);
        let svg = render_memlayout_svg(&ml);
        assert!(svg.starts_with("<svg"), "{svg}");
        assert!(svg.contains("region[511]"));
        assert!(svg.contains("+4088"), "explicit offset label");
        assert!(svg.contains("+4096"), "lane end offset");
        assert!(svg.contains("4072 B"));
        assert!(svg.contains("e_addr = 页首地址"));
        assert_eq!(svg.matches("marker-end=").count(), 3, "2 ptrs + legend arrow");
    }

    #[test]
    fn memlayout_lint_reports_actionable_errors() {
        let bad = "```memlayout\ncell a 8\nlane p x\ncell b 8q\nptr p.b -> q.z\nfoo\n```\n";
        let errs = lint_memlayout_blocks(bad);
        assert_eq!(errs.len(), 1);
        let e = &errs[0];
        assert!(e.contains("先写 lane"), "{e}");
        assert!(e.contains("大小「8q」"), "{e}");
        assert!(e.contains("端点「p.b」") && e.contains("端点「q.z」"), "{e}");
        assert!(e.contains("未知指令「foo」"), "{e}");
        assert!(lint_memlayout_blocks(&format!("```memlayout{SLAB}```\n")).is_empty());
        let jump = "```memlayout\nlane t\ncell a 1\ncell b 1 @8\ncell c 1 @1\n```\n";
        let e = lint_memlayout_blocks(jump).join("\n");
        assert!(e.contains("中间 7 B") && e.contains("gap 7"), "{e}");
        assert!(e.contains("超了 8 B"), "{e}");
    }

    /// 实测事故原样复刻：行说明写「= 28 KiB」，格子却多画了 1 KiB「头部位图元数据」。
    const SLAB_WITH_FAKE_HEADER: &str = r#"
lane slab "Small 14 KiB 对象 Slab (7 个 4KB 页 = 28 KiB)"
cell hdr 1K meta "slab 头部位图元数据"
cell s0 14K used "14 KiB 对象槽位 0"
cell s1 14K free "14 KiB 对象槽位 1"
"#;

    #[test]
    fn memlayout_claims_catch_invented_slab_header() {
        let e = lint_memlayout_blocks(&format!("```memlayout{SLAB_WITH_FAKE_HEADER}```\n")).join("\n");
        assert!(e.contains("= 28 KiB") && e.contains("29696 B") && e.contains("多了 1024 B"), "{e}");
        // 同一行混着 meta 和数据格子，又没给出处 → 也要被点名。
        assert!(e.contains("元数据内嵌在数据区里"), "{e}");
    }

    #[test]
    fn memlayout_claims_accept_correct_slab_layouts() {
        // 修正后的画法：数据区就是 nregs × reg_size，元数据是独立 lane，用 ptr 指过去。
        let good = r#"
title 14 KiB 档的 slab：7 页 = 28 KiB = 28672 B
lane slab "14 KiB slab（7 个连续 4 KiB 物理页 = 28 KiB）"
cell s0 14K used "槽位 0"
cell s1 14K free "槽位 1"
lane edata "edata_t（带外元数据，不占 slab 数据页）"
cell addr 8 meta "e_addr"
cell bm 8 meta "bitmap"
ptr edata.addr -> slab.s0 "指向 slab 基址"
"#;
        assert_eq!(lint_memlayout_blocks(&format!("```memlayout{good}```\n")), Vec::<String>::new());
        // 确实内嵌时：给出处就放行。
        let inline = r#"
lane chunk "glibc chunk（malloc.c:1050 头部内嵌 size 字段）"
cell hdr 16 meta "prev_size+size"
cell body 48 used "用户数据"
"#;
        assert_eq!(lint_memlayout_blocks(&format!("```memlayout{inline}```\n")), Vec::<String>::new());
        // 没有等号的「14 KiB 对象」是单格描述，不是总数，不对账。
        let desc = "lane p \"每格 14 KiB\"\ncell a 1K used \"a\"\n";
        assert!(lint_memlayout_blocks(&format!("```memlayout\n{desc}```\n")).is_empty());
        // 有未标大小的 gap → 总长算不出来，不乱报。
        let gap = "lane p \"一页 = 4 KiB\"\ncell a 8 used \"a\"\ngap \"中间省略\"\n";
        assert!(lint_memlayout_blocks(&format!("```memlayout\n{gap}```\n")).is_empty());
    }

    #[test]
    fn memlayout_title_claim_must_match_some_lane() {
        let bad = "title 一页 = 8 KiB\nlane p \"页\"\ncell a 4K data \"a\"\n";
        let e = lint_memlayout_blocks(&format!("```memlayout\n{bad}```\n")).join("\n");
        assert!(e.contains("标题写「= 8 KiB」") && e.contains("4 KiB"), "{e}");
    }

    #[test]
    fn declared_sizes_only_reads_equals_with_unit() {
        let got = |s: &str| declared_sizes(s).into_iter().map(|(_, b)| b).collect::<Vec<_>>();
        assert_eq!(got("7 页 = 28 KiB = 28672 B"), vec![28672, 28672]);
        assert_eq!(got("4KB页＝4096字节"), vec![4096]);
        assert!(got("nregs = 4，reg_size = 1024").is_empty(), "没有单位不认");
        assert!(got("= 1.5 KiB").is_empty(), "小数不认");
        assert!(got("14 KiB 对象").is_empty(), "没有等号不认");
    }

    #[test]
    fn has_file_line_recognises_citations() {
        assert!(has_file_line("slab（src/arena.c:236）"));
        assert!(has_file_line("见 sz.h:191-197"));
        assert!(!has_file_line("时间 12:30 到 bin.h 之间"));
        assert!(!has_file_line("比例 1:2"));
    }

    #[test]
    fn memlayout_blocks_render_to_image_links() {
        let dir = tempfile::tempdir().unwrap();
        let text = format!("前文\n\n```memlayout{SLAB}```\n\n后文\n```c\n```memlayout\n```\n");
        let out = render_memlayout_blocks(&text, &dir.path().join("img"), "../img");
        assert!(out.contains("![一个 4 KiB slab 页切成 512 个 8 B region](../img/ml-"), "{out}");
        assert!(out.contains("前文") && out.contains("后文"));
        assert!(!out.contains("lane page"), "{out}");
        let svgs: Vec<_> = std::fs::read_dir(dir.path().join("img")).unwrap().collect();
        assert_eq!(svgs.len(), 1);
        assert_eq!(memlayout_blocks(&text), 1, "c 代码块里的 ```memlayout 不算");
    }

    #[test]
    fn publish_copies_referenced_assets_and_rewrites_links() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("ws");
        std::fs::create_dir_all(base.join("sections")).unwrap();
        std::fs::create_dir_all(base.join("img")).unwrap();
        std::fs::create_dir_all(base.join("code")).unwrap();
        std::fs::write(base.join("sections/k1.md"), "![a](../img/a.svg)").unwrap();
        std::fs::write(base.join("sections/feedback.md"), "MODE: FULL").unwrap();
        std::fs::write(base.join("img/a.svg"), "<svg/>").unwrap();
        std::fs::write(base.join("code/x.c"), "int x;").unwrap();
        std::fs::write(base.join("sections/k9.md"), "旧 plan 留下的章").unwrap();
        std::fs::write(base.join("img/old.svg"), "<svg/>").unwrap();
        let docs = dir.path().join("docs/learn");
        let body = "[k1](sections/k1.md) ![a](img/a.svg) [src](src/sz.c)";
        let (out, n) = publish_assets(body, &base, &docs, "topic").unwrap();
        assert_eq!(out, "[k1](topic/sections/k1.md) ![a](topic/img/a.svg) [src](src/sz.c)");
        assert_eq!(n, 2, "只拷引用闭包");
        assert!(docs.join("topic/sections/k1.md").is_file());
        assert!(!docs.join("topic/sections/feedback.md").exists());
        assert!(docs.join("topic/img/a.svg").is_file());
        assert!(!docs.join("topic/code/x.c").exists(), "没被引用的副本不发布");
        // 章节里再链到的东西随章节一起发布（传递闭包），旧章不发布。
        std::fs::write(base.join("sections/k1.md"), "![a](../img/a.svg) [x](../code/x.c)").unwrap();
        let (_, n) = publish_assets("[k1](sections/k1.md)", &base, &docs, "topic").unwrap();
        assert_eq!(n, 3);
        assert!(docs.join("topic/code/x.c").is_file());
        assert!(!docs.join("topic/sections/k9.md").exists());
        assert!(!docs.join("topic/img/old.svg").exists());
    }
}

