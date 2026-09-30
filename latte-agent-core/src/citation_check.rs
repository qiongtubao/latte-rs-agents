//! 行号引用校验：产出里写的 `path:line` 必须落在**本作用域实际观察过**的行内。
//!
//! # 动机（实测 2026-09-10 jemalloc 会话 `ui-…-6`，任务 LAT-113）
//!
//! `require_symbols_resolvable` 那道门禁**放行了**这份产出——因为它只验证
//! 一件事：反引号里的标识符在仓库里按词边界出现过。`bit_util.h`、`ffs_zu`、
//! `lg_floor` 全都真实存在，所以它按设计不可能拦。
//!
//! 然后 reviewer 判了 `VERDICT: FAIL`，B1 条列出：
//!
//! | 函数 | `#error` 真实行号 | 文档标的 | 文档标的真实内容 |
//! |---|---|---|---|
//! | `ffs_zu`  | 296 | 296 | ✅ |
//! | `fls_zu`  | 309 | 310 | ❌ 310 是 `#endif` |
//! | `ffs_u64` | 321 | 322 | ❌ 322 是 `#endif` |
//! | `fls_u64` | 332 | 333 | ❌ 333 是 `#endif` |
//! | `ffs_u32` | 341 | 342 | ❌ 342 是 `#endif` |
//! | `fls_u32` | 351 | 354 | ❌ 354 是函数结尾 `}` |
//! | `lg_floor`| （无）| 399 | ❌ 整个函数没有 `#error` |
//!
//! 7 个数字 6 个错。后续 programmer 那 8 次 `edit` 失败，全是在修这些引用。
//!
//! 名字对、**关于名字的断言**是编的——这是符号门禁的结构性盲区：它验证的是
//! 一个 bit（字符串存在），不验证任何 claim。本模块补的就是最容易机械验证的
//! 那一类 claim：行号。
//!
//! # 判据
//!
//! 对每条 `path:line` 引用：
//!
//! 1. 用 [`crate::edit_anchor::observed_lines_for_cited_path`] 在本作用域
//!    （session + role）找该文件的行级来源；
//! 2. 拿不到 → **放行**（见下「三态」）；
//! 3. 拿到了，但引用的行不在其中 → 拦，并把那些行的**真实内容**回给模型。
//!
//! # 三态：为什么「查不到」必须放行
//!
//! 台账有容量上限、按 LRU 淘汰，所以「查不到」既可能是没读过、也可能是读过
//! 但被挤出去了；还可能是同名文件歧义、或只有 tag 没有行级来源。把这些都当
//! 「没看过」去拒绝，会把大量合法产出误杀。所以只拦**确定看过这个文件、但
//! 引用的行落在观察范围之外**这一种——这一种是硬错误，没有申辩空间。
//!
//! 对齐 oh-my-pi 的 `Snapshot.seen_lines: Option<BTreeSet<u32>>`：`None`
//! 表示 provenance not recorded，检查降级而非拒绝。
//!
//! # 报错形态
//!
//! 抄 oh-my-pi 的 `UnseenLinesReveal`：不只说「你没看过这几行」，而是把那几
//! 行的真实内容**内联进报错**。模型可以原地改对，不必再花一轮 `read`。

use std::collections::BTreeSet;

/// 报错里最多列几条问题引用（多了模型读不完，也撑爆 prompt）。
const MAX_REPORTED: usize = 8;

/// 每条问题引用最多 reveal 几行真实内容。
const MAX_REVEAL_LINES: usize = 6;

/// reveal 时单行最多保留多少字符（对齐 oh-my-pi 的
/// `SEEN_LINE_REVEAL_MAX_COLUMNS`，只是更保守）。
const MAX_REVEAL_COLUMNS: usize = 200;

/// 从产出里解析出的一条行号引用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Citation {
    /// 引用里写的路径（可能是 basename 或短相对路径，原样保留）。
    pub path: String,
    /// 引用到的行号（1-based，已展开区间与逗号列表）。
    pub lines: Vec<u32>,
    /// 原文片段，用于报错定位。
    pub raw: String,
}

/// 路径 token 允许的字符。
fn is_path_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '\\' | '+')
}

/// 计算文本里所有反引号 span 的字节区间。
///
/// 逗号列表形态（`` `bit_util.h:296, 310, 322` ``）只在反引号内解析：散文里
/// 的 `foo.rs:12`，然后另起一句提到「3 个宏」——那个 3 不能被当成行号。反引号
/// 是模型标注「这是一处代码位置」的一致信号，把多值解析限制在里面能几乎消掉
/// 这类误读。
fn backtick_spans(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut open: Option<usize> = None;
    for (i, b) in bytes.iter().enumerate() {
        if *b != b'`' {
            continue;
        }
        match open {
            None => open = Some(i + 1),
            Some(start) => {
                if i > start {
                    spans.push((start, i));
                }
                open = None;
            }
        }
    }
    spans
}

fn in_span(spans: &[(usize, usize)], idx: usize) -> bool {
    spans.iter().any(|(s, e)| idx >= *s && idx < *e)
}

/// 解析产出里的行号引用。
///
/// 认四种形态（都取自 jemalloc 实例）：
/// - `src/jemalloc.c:2745`
/// - `jemalloc_internal_inlines_c.h:258-338`
/// - `` `bit_util.h:296, 310, 322` ``（反引号内的逗号列表）
/// - `` `bit_util.h:343、352` ``（中文顿号）
///
/// 刻意**不认**的：`foo::bar`（Rust 路径）、`http://x:80`（URL）、`12:30`
/// （时间，没有扩展名）。宁可漏解析，也不要把无关数字当行号去拦人。
pub fn parse_citations(text: &str) -> Vec<Citation> {
    let spans = backtick_spans(text);
    let bytes = text.as_bytes();
    let mut out: Vec<Citation> = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b':' {
            i += 1;
            continue;
        }
        // 冒号后必须紧跟数字
        if !matches!(bytes.get(i + 1), Some(c) if c.is_ascii_digit()) {
            i += 1;
            continue;
        }
        // `::` 排除（Rust 路径 / C++ 作用域）
        if i > 0 && bytes[i - 1] == b':' {
            i += 1;
            continue;
        }
        // 往左收路径 token
        let mut start = i;
        while start > 0 {
            let prev = bytes[start - 1] as char;
            if is_path_char(prev) {
                start -= 1;
            } else {
                break;
            }
        }
        let raw_path = &text[start..i];
        if !is_plausible_path(raw_path) {
            i += 1;
            continue;
        }
        // 往右读行号列表
        let multi = in_span(&spans, i);
        let (lines, end) = parse_line_list(text, i + 1, multi);
        if lines.is_empty() {
            i += 1;
            continue;
        }
        out.push(Citation {
            path: raw_path.to_string(),
            lines,
            raw: text[start..end].to_string(),
        });
        i = end.max(i + 1);
    }
    out
}

/// 路径 token 是否像个文件路径。
fn is_plausible_path(tok: &str) -> bool {
    if tok.len() < 3 || tok.len() > 300 {
        return false;
    }
    // 必须有扩展名：`.` 后面跟 1..=12 个字母数字，且这是最后一段
    let Some(dot) = tok.rfind('.') else {
        return false;
    };
    let ext = &tok[dot + 1..];
    if ext.is_empty() || ext.len() > 12 || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        return false;
    }
    // 纯数字扩展名（版本号 `1.2`）不算路径
    if ext.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    // URL：`https://host:8080` 里往左收到的 token 是 `//host`（scheme 在
    // 前一个冒号处就被截断了），所以要按「以 // 开头」来认，而不是找 scheme。
    if tok.starts_with("//") || tok.starts_with("\\\\") {
        return false;
    }
    // 扩展名前得有名字
    !tok[..dot].is_empty()
}

/// 从 `at` 开始读行号：`N`、`N-M`，`multi` 时还继续读 `, K` / `、K`。
fn parse_line_list(text: &str, at: usize, multi: bool) -> (Vec<u32>, usize) {
    let bytes = text.as_bytes();
    let mut lines: Vec<u32> = Vec::new();
    let mut pos = at;

    let read_num = |p: &mut usize| -> Option<u32> {
        let s = *p;
        while *p < bytes.len() && bytes[*p].is_ascii_digit() {
            *p += 1;
        }
        if *p == s || *p - s > 9 {
            return None;
        }
        text[s..*p].parse::<u32>().ok().filter(|n| *n > 0)
    };

    let Some(first) = read_num(&mut pos) else {
        return (lines, at);
    };
    // 区间形态 `N-M`
    if bytes.get(pos) == Some(&b'-') {
        let mut probe = pos + 1;
        if let Some(end) = read_num(&mut probe) {
            if end >= first && end - first <= 5_000 {
                for n in first..=end {
                    lines.push(n);
                }
                pos = probe;
                return (lines, pos);
            }
        }
    }
    lines.push(first);
    if !multi {
        return (lines, pos);
    }
    // 反引号内的逗号/顿号列表
    loop {
        let mut probe = pos;
        // 分隔符：`,` / `、`（UTF-8 三字节）+ 可选空格
        if bytes.get(probe) == Some(&b',') {
            probe += 1;
        } else if text[probe..].starts_with('、') {
            probe += '、'.len_utf8();
        } else {
            break;
        }
        while bytes.get(probe) == Some(&b' ') {
            probe += 1;
        }
        match read_num(&mut probe) {
            Some(n) => {
                lines.push(n);
                pos = probe;
            }
            None => break,
        }
        if lines.len() > 64 {
            break;
        }
    }
    (lines, pos)
}

/// 取 `path` 第 `lines` 行的真实内容，用于 reveal。
///
/// 从**磁盘**读而不是从台账读：reveal 的目的是告诉模型「那儿实际是什么」，
/// 磁盘上的当前内容才是 ground truth（台账里存的是它当初看到的样子，用它
/// reveal 等于用错误的依据纠正错误）。
fn reveal_lines(cwd: &std::path::Path, path: &str, lines: &[u32]) -> Vec<(u32, String)> {
    let full = cwd.join(path);
    let Ok(text) = std::fs::read_to_string(&full) else {
        return Vec::new();
    };
    let all: Vec<&str> = text.split('\n').collect();
    lines
        .iter()
        .take(MAX_REVEAL_LINES)
        .filter_map(|n| {
            let idx = (*n as usize).checked_sub(1)?;
            let raw = all.get(idx)?;
            let shown: String = if raw.chars().count() > MAX_REVEAL_COLUMNS {
                raw.chars().take(MAX_REVEAL_COLUMNS).collect::<String>() + "…"
            } else {
                (*raw).to_string()
            };
            Some((*n, shown))
        })
        .collect()
}

/// 校验产出里的行号引用。`Ok(())` = 放行。
///
/// `Err` 的文案直接作为给模型的验收批注使用。
pub fn check_citations_observed(
    output: &str,
    scope: &crate::edit_anchor::Scope,
    cwd: &std::path::Path,
) -> Result<(), String> {
    let citations = parse_citations(output);
    if citations.is_empty() {
        return Ok(());
    }
    // 同一文件的多条引用合并，报错更紧凑
    let mut problems: Vec<(String, Vec<u32>)> = Vec::new();
    for c in &citations {
        let Some((ledger_path, seen)) =
            crate::edit_anchor::observed_lines_for_cited_path(scope, &c.path)
        else {
            continue; // 三态：不能判定 → 放行
        };
        let unseen: Vec<u32> = c
            .lines
            .iter()
            .copied()
            .filter(|n| !seen.contains(n))
            .collect();
        if unseen.is_empty() {
            continue;
        }
        match problems.iter_mut().find(|(p, _)| *p == ledger_path) {
            Some((_, acc)) => acc.extend(unseen),
            None => problems.push((ledger_path, unseen)),
        }
    }
    if problems.is_empty() {
        return Ok(());
    }
    Err(render_problems(&problems, cwd))
}

fn render_problems(problems: &[(String, Vec<u32>)], cwd: &std::path::Path) -> String {
    let total: usize = problems.iter().map(|(_, l)| l.len()).sum();
    let mut msg = format!(
        "产出里有 {total} 处行号引用落在**本次没有实际读到过的行**上（这些文件你\
         读过，但引用的行不在读到的范围内——凭记忆或推算写行号，实测最容易错）。\
         下面直接给出那些行的真实内容，请据此改正引用，或先 `read` 对应范围再写：\n"
    );
    let mut shown = 0usize;
    for (path, lines) in problems {
        let mut uniq: Vec<u32> = lines.clone();
        uniq.sort_unstable();
        uniq.dedup();
        let revealed = reveal_lines(cwd, path, &uniq);
        let list: Vec<String> = uniq.iter().take(12).map(|n| n.to_string()).collect();
        msg.push_str(&format!("\n  {path} 未读到的行：{}\n", list.join(", ")));
        for (n, text) in revealed {
            msg.push_str(&format!("    {n}: {text}\n"));
            shown += 1;
            if shown >= MAX_REPORTED * MAX_REVEAL_LINES {
                break;
            }
        }
        if shown >= MAX_REPORTED * MAX_REVEAL_LINES {
            msg.push_str("    …（其余略）\n");
            break;
        }
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths_lines(text: &str) -> Vec<(String, Vec<u32>)> {
        parse_citations(text)
            .into_iter()
            .map(|c| (c.path, c.lines))
            .collect()
    }

    /// 四种形态都取自 jemalloc 实际产出。
    #[test]
    fn parses_the_four_real_world_citation_shapes() {
        // 单行
        assert_eq!(
            paths_lines("配置收口见 src/jemalloc.c:2745 处。"),
            vec![("src/jemalloc.c".to_string(), vec![2745])]
        );
        // 区间
        assert_eq!(
            paths_lines("快路径 `jemalloc_internal_inlines_c.h:258-260`"),
            vec![(
                "jemalloc_internal_inlines_c.h".to_string(),
                vec![258, 259, 260]
            )]
        );
        // 反引号内的逗号列表（LAT-113 的 B1 就是这个形态）
        assert_eq!(
            paths_lines("见 `bit_util.h:296, 310, 322`"),
            vec![("bit_util.h".to_string(), vec![296, 310, 322])]
        );
        // 中文顿号
        assert_eq!(
            paths_lines("末尾（`bit_util.h:343、352`）"),
            vec![("bit_util.h".to_string(), vec![343, 352])]
        );
    }

    /// 刻意不认的形态：宁可漏解析，也不要拿无关数字去拦人。
    #[test]
    fn does_not_misread_non_citations() {
        for text in [
            "std::collections::HashMap 里的 foo::bar:12", // `::`
            "见 https://example.com:8080/x",              // URL 端口
            "会议在 12:30 开始",                          // 时间（无扩展名）
            "版本 1.2:3 不是路径",                        // 纯数字扩展名
            "变量 x:1",                                   // 太短、无扩展名
        ] {
            assert!(
                parse_citations(text).is_empty(),
                "不该解析出引用: {text} -> {:?}",
                parse_citations(text)
            );
        }
    }

    /// 逗号列表只在反引号内生效：散文里的逗号后面的数字不是行号。
    #[test]
    fn comma_lists_only_inside_backticks() {
        // 反引号外：只取第一个数字，后面的 3 不算行号
        let got = paths_lines("详见 src/a.c:10, 3 个宏另见别处");
        assert_eq!(got, vec![("src/a.c".to_string(), vec![10])]);
    }

    /// 区间过大时不展开（防一条引用生成上万行号）。
    #[test]
    fn absurd_ranges_are_not_expanded() {
        let got = parse_citations("`a.c:1-99999`");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].lines, vec![1], "超大区间退化成单行，不展开");
    }

    use crate::edit_anchor::{record_tool_result, Scope};
    use serde_json::json;

    /// 台账里没有这个文件 → **放行**。
    ///
    /// 「查不到」既可能是没读过、也可能是读过但被 LRU 淘汰了，还可能是同名
    /// 歧义。当成「没看过」去拒绝会大量误杀，所以只能放行。
    #[test]
    fn unknown_files_are_let_through() {
        let scope = Scope::new("sess-cite-unknown", "tech_writer");
        let out = "机制见 `include/foo/never_read.h:296, 310`";
        assert!(
            check_citations_observed(out, &scope, std::path::Path::new(".")).is_ok(),
            "台账无记录时必须放行"
        );
    }

    /// 读过的行照常放行。
    #[test]
    fn citations_within_observed_lines_pass() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let file = dir.path().join("bit_util.h");
        std::fs::write(
            &file,
            (1..=20).map(|n| format!("line{n}\n")).collect::<String>(),
        )
        .expect("write");
        let scope = Scope::new("sess-cite-ok", "tech_writer");
        // 读了 1-20 行（无行范围 = 全文）
        record_tool_result(
            &scope,
            "read",
            &json!({"path": file.to_string_lossy(), "content":
                (1..=20).map(|n| format!("line{n}\n")).collect::<String>(),
                "tag": "AAAA"})
            .to_string(),
        );
        let out = "见 `bit_util.h:5, 7`";
        assert!(
            check_citations_observed(out, &scope, dir.path()).is_ok(),
            "引用落在读过的范围内应放行"
        );
    }

    /// LAT-113 的真实回归：读了 296 那一段，却引用了 310/322 —— 必须拦，
    /// 并且报错里带出那些行的**真实内容**。
    #[test]
    fn lat113_wrong_line_numbers_are_caught_with_reveal() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let file = dir.path().join("bit_util.h");
        // 造一份形似的文件：296 是 #error，310/322 是 #endif
        let mut lines: Vec<String> = (1..=400).map(|n| format!("/* filler {n} */")).collect();
        lines[295] = "#  error No implementation for ffs_zu()".to_string();
        lines[309] = "#endif".to_string();
        lines[321] = "#endif".to_string();
        std::fs::write(&file, lines.join("\n")).expect("write");

        let scope = Scope::new("sess-cite-lat113", "tech_writer");
        // 只读了 290-300 这一段
        record_tool_result(
            &scope,
            "read",
            &json!({
                "path": file.to_string_lossy(),
                "content": lines[289..300].join("\n"),
                "startLine": 290,
                "tag": "AAAA"
            })
            .to_string(),
        );

        let out = "字宽派发的 `#error` 链见 `bit_util.h:296, 310, 322`。";
        let err = check_citations_observed(out, &scope, dir.path())
            .expect_err("引用了没读到的 310/322，必须拦");

        assert!(
            err.contains("310") && err.contains("322"),
            "应点名问题行: {err}"
        );
        assert!(
            !err.contains("未读到的行：296"),
            "296 读过，不该被点名: {err}"
        );
        // reveal：把真实内容给出来，模型可以原地改对
        assert!(
            err.contains("#endif"),
            "报错必须带出那些行的真实内容（reveal）: {err}"
        );
        assert!(err.contains("read"), "应给出可执行动作: {err}");
    }

    /// 同名文件歧义 → 不判（放行）。
    #[test]
    fn ambiguous_basenames_are_not_judged() {
        let scope = Scope::new("sess-cite-ambig", "programmer");
        for p in ["/tmp/ambig-a/mod.rs", "/tmp/ambig-b/mod.rs"] {
            record_tool_result(
                &scope,
                "read",
                &json!({"path": p, "content": "a\nb\n", "tag": "AAAA"}).to_string(),
            );
        }
        let out = "见 `mod.rs:99`";
        assert!(
            check_citations_observed(out, &scope, std::path::Path::new(".")).is_ok(),
            "两个同名文件时无法确定说的是哪个，必须放行"
        );
    }
}
