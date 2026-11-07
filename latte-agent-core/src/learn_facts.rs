//! 学习文档里「同一个宏 / 常量在不同地方写成不同数值」的机械检测。
//!
//! # 动机（实测 LAT-110 jemalloc，会话 ui-75536）
//!
//! plan 阶段给每个知识点写了一个 `example`，其中编了两个没有核实的数：`SC_NBINS=39`、
//! `SC_NPSIZES≈194`（真实是 36 和 199）。第 0 节 / 第 3 节的作者老老实实对着源码推导，算出 36；
//! 总览、k1 章末总结却沿用了 plan 里的 39。同一份文档里 `SC_NBINS` 同时是 36 和 39，
//! 小白试学读完 5 章没发现，引擎也没有任何一道门看「全文数值是否自洽」。
//!
//! # 判据
//!
//! 只看**能机械判定**的那一种写法：正文（不含代码块）里 `大写宏名 = 数字`（`=`、`＝`、`≈`、`为`、`等于`
//! 等连接词，中间允许反引号 / 星号 / 空格）。同一个宏名出现两个以上不同的数值就是矛盾。
//!
//! 刻意保守，宁可漏判：
//! - 宏名必须带下划线（`SC_NBINS`），`CPU`、`ASCII` 这类大写词不算；
//! - 数字后面紧跟运算符（`4 * 8`、`2^62`、`1 << 3`）是算式不是结果，不算；
//! - 带 `K` / `KB` / `MB` 的按 1024 进位归一，`16 KB` 与 `16384` 是同一个值；
//! - 整行带「若 / 如果 / 假设 / 改成 / 改后 / 代入 / 推测 / 反例 / arm / 16K 页」这类条件、假设、反例的措辞，不算；
//! - `X == 0` 是比较（`#if` 守卫）不是陈述，不算；`= 4 − 3` 后面接运算符是算式，不算；
//! - 「⚠️ 待确认」注记是转述别人发现的错误，不算。
//!
//! 引擎不判哪个数对（对不对要看源码或推导），只指出矛盾、把少数派当作可疑处交给辅导员核对：
//! - 辅导员改了（FIXED）→ 引擎重扫，矛盾还在就不认；
//! - 辅导员核对后说不是矛盾（WONTFIX，如「把 `SC_LG_NGROUP` 改成 1」的反事实推演）→ 采信，同一组数值不再提。
//!
//! 正文讲设计取舍时同一个宏出现多个值是常态（替代方案、误改后果），所以机械规则一定会有误报，
//! 最后的裁决权在读过源码的辅导员手里，而不是引擎。

use std::collections::BTreeMap;

/// 一处「宏 = 数」的陈述。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub name: String,
    pub value: u64,
    /// 文档文件（相对学习工作区）。
    pub file: String,
    /// 1 起。
    pub line: usize,
    /// 原文里的这一小段（`SC_NBINS = 39`），用来在文档里定位。
    pub span: String,
}

/// 同一个宏有多个数值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub name: String,
    /// 按「出现次数多、位置靠前」排序：第一组当作多数派，其余是可疑的。
    pub groups: Vec<(u64, Vec<Claim>)>,
}

impl Conflict {
    /// 这组矛盾的标识：宏名 + 出现过的全部数值。辅导员核对后认定不是矛盾（反事实推演等）的，
    /// 同一组数值下次不再提；数值变了（改了一部分）就是新的矛盾。
    pub fn signature(&self) -> String {
        let mut vs: Vec<u64> = self.groups.iter().map(|(v, _)| *v).collect();
        vs.sort_unstable();
        format!(
            "numeric:{}|{}",
            self.name,
            vs.iter().map(u64::to_string).collect::<Vec<_>>().join(",")
        )
    }

    /// 可疑的陈述：非第一组的全部；第一、二组一样多就分不出多数派，全部都算（由辅导员对着源码裁决）。
    pub fn suspects(&self) -> Vec<&Claim> {
        let tie = self.groups.len() >= 2 && self.groups[0].1.len() == self.groups[1].1.len();
        self.groups
            .iter()
            .skip(usize::from(!tie))
            .flat_map(|(_, c)| c.iter())
            .collect()
    }

    /// 给辅导员看的一段话。
    pub fn describe(&self) -> String {
        let fmt_group = |(v, cs): &(u64, Vec<Claim>)| {
            let places: Vec<String> = cs
                .iter()
                .take(3)
                .map(|c| format!("{} 第 {} 行「{}」", c.file, c.line, c.span))
                .collect();
            format!(
                "{v}（{} 处：{}{}）",
                cs.len(),
                places.join("；"),
                if cs.len() > 3 { "；…" } else { "" }
            )
        };
        format!(
            "`{}` 在文档里写成了互相矛盾的数值：{}。对照源码（宏定义、`#define` 的展开）或把公式代入重新算一遍，确定哪个对，\
             把全文所有写法改成一致——包括换了说法的同一个数（如「前 N 个档位」「共 N 档」）。不要只在旁边加说明",
            self.name,
            self.groups.iter().map(fmt_group).collect::<Vec<_>>().join(" 与 ")
        )
    }
}

/// 整行带这些措辞：在讲条件 / 假设 / 反例 / 别的配置，里面的数不代表「这个宏的值」。
const EXEMPT_WORDS: &[&str] = &[
    "若",
    "如果",
    "假设",
    "假如",
    "换成",
    "改成",
    "改为",
    "改到",
    "改后",
    "改动",
    "代入",
    "推测",
    "反例",
    "误以为",
    "误写",
    "非法",
    "不支持",
    "arm",
    "ARM",
    "16K 页",
    "16KB 页",
    "16 KB 页",
    "64K 页",
    "又如",
    "比如",
    "例如",
];

/// 连接词：宏名之后、数字之前。
// 注意没有 `==`：那是 `#if X == 0` 这类条件表达式（比较），不是在陈述它的值。
const LINKS: &[&str] = &[
    "＝",
    "=",
    "≈",
    "≃",
    "约等于",
    "等于",
    "约为",
    "恰为",
    "固定为",
    "为",
    "是",
];

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn is_macro_name(tok: &str) -> bool {
    tok.chars().count() >= 4
        && tok.contains('_')
        && tok.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && tok
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// 从 `s` 开头解析一个整数（可带 `,` `_` 分隔、`0x` 十六进制、`K`/`KB`/`MB` 单位）。
/// 后面紧跟运算符 / 字母数字（表达式或别的标识符）返回 None。
fn parse_value(s: &str) -> Option<(u64, usize)> {
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    let hex = b.len() > 2 && b[0] == '0' && (b[1] == 'x' || b[1] == 'X');
    let mut digits = String::new();
    if hex {
        i = 2;
        while i < b.len() && b[i].is_ascii_hexdigit() {
            digits.push(b[i]);
            i += 1;
        }
    } else {
        while i < b.len()
            && (b[i].is_ascii_digit()
                || ((b[i] == ',' || b[i] == '_')
                    && b.get(i + 1).is_some_and(|c| c.is_ascii_digit())))
        {
            if b[i].is_ascii_digit() {
                digits.push(b[i]);
            }
            i += 1;
        }
    }
    if digits.is_empty() || digits.len() > 18 {
        return None;
    }
    // 小数不当整数。
    if b.get(i) == Some(&'.') && b.get(i + 1).is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    let mut value = if hex {
        u64::from_str_radix(&digits, 16).ok()?
    } else {
        digits.parse::<u64>().ok()?
    };
    let mut j = i;
    while b.get(j) == Some(&' ') {
        j += 1;
    }
    let rest: String = b[j..].iter().take(4).collect();
    let unit = [
        ("KiB", 1024u64),
        ("KB", 1024),
        ("MiB", 1 << 20),
        ("MB", 1 << 20),
        ("K", 1024),
    ]
    .into_iter()
    .find(|(u, _)| rest.starts_with(u));
    if let Some((u, mul)) = unit {
        let after = b.get(j + u.len()).copied();
        if after.is_none_or(|c| !c.is_ascii_alphanumeric()) {
            value = value.checked_mul(mul)?;
            i = j + u.len();
        }
    }
    // 紧跟运算符或标识符字符：是算式 / 别的东西，不是结果。
    let mut k = i;
    while b.get(k) == Some(&' ') {
        k += 1;
    }
    match b.get(k) {
        Some(c)
            if "*×x+-−–—/÷^<>%·".contains(*c)
                || (c.is_ascii_alphabetic() && k == i)
                || *c == '_' =>
        {
            None
        }
        _ => Some((value, i)),
    }
}

/// 一个文件里的全部陈述。
pub fn extract_claims(file: &str, text: &str) -> Vec<Claim> {
    let mut out = Vec::new();
    let mut fence: Option<String> = None;
    for (n, line) in text.lines().enumerate() {
        let t = line.trim_start();
        if let Some(f) = &fence {
            if t.starts_with(f.as_str()) {
                fence = None;
            }
            continue;
        }
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = Some(t[..3].to_string());
            continue;
        }
        // 「试学提问」小节是引擎转述读者的疑问，里面引着被指出的错误原文，不代表文档的立场。
        if crate::learn_cite::is_pending_note(t)
            || t.starts_with("#### 试学提问")
            || EXEMPT_WORDS.iter().any(|w| line.contains(w))
        {
            continue;
        }
        let chars: Vec<(usize, char)> = line.char_indices().collect();
        let mut ci = 0;
        while ci < chars.len() {
            let (bi, c) = chars[ci];
            if !c.is_ascii_uppercase() || (ci > 0 && is_ident_char(chars[ci - 1].1)) {
                ci += 1;
                continue;
            }
            let mut cj = ci;
            while cj < chars.len() && is_ident_char(chars[cj].1) {
                cj += 1;
            }
            let end_b = chars.get(cj).map_or(line.len(), |x| x.0);
            let tok = &line[bi..end_b];
            ci = cj.max(ci + 1);
            if !is_macro_name(tok) {
                continue;
            }
            // `NGROUP+LG_QUANTUM=6`：等号右边是整个算式的结果，不是这个宏自己的值。
            {
                let mut before = line[..bi].chars().rev();
                let pre = before.next();
                let pre2 = before.next();
                let pre_trimmed = line[..bi].trim_end().chars().last();
                let in_expr = match pre {
                    _ if pre_trimmed.is_some_and(|p| "+&|^<>~/".contains(p)) => true,
                    // `-`、`*` 紧贴着前一个标识符 / 右括号才是运算符（列表符号、加粗记号前面是空白或行首）。
                    Some(p) if p == '-' || p == '*' => {
                        pre2.is_some_and(|q| is_ident_char(q) || q == ')')
                    }
                    _ => false,
                };
                if in_expr {
                    continue;
                }
            }
            // 宏名后的装饰（反引号、星号、空格）→ 连接词 → 装饰 → 数字
            let rest = &line[end_b..];
            let skip_deco = |s: &str| {
                s.trim_start_matches(|c: char| c == '`' || c == '*' || c == ' ' || c == '\u{3000}')
                    .to_string()
            };
            let after_name = skip_deco(rest);
            let Some(link) = LINKS.iter().find(|l| after_name.starts_with(**l)) else {
                continue;
            };
            let after_link = skip_deco(&after_name[link.len()..]);
            // 「约」「大约」之类的修饰
            let after_link = after_link
                .trim_start_matches("约")
                .trim_start_matches("大约");
            let after_link = skip_deco(after_link);
            let Some((value, used)) = parse_value(&after_link) else {
                continue;
            };
            // 原文片段：从宏名到数字末尾。
            let consumed_tail = after_link.chars().take(used).collect::<String>();
            let span_start = bi;
            let tail_start = line.len() - after_link.len();
            let span_end = tail_start + consumed_tail.len();
            out.push(Claim {
                name: tok.to_string(),
                value,
                file: file.to_string(),
                line: n + 1,
                span: line[span_start..span_end].to_string(),
            });
        }
    }
    out
}

/// 全部文件里自相矛盾的宏。`files` 是 (文件名, 全文)，顺序即文档顺序。
pub fn conflicts(files: &[(String, String)]) -> Vec<Conflict> {
    let mut by_name: BTreeMap<String, Vec<(usize, Claim)>> = BTreeMap::new();
    let mut pos = 0usize;
    for (f, t) in files {
        for c in extract_claims(f, t) {
            by_name.entry(c.name.clone()).or_default().push((pos, c));
            pos += 1;
        }
    }
    let mut out = Vec::new();
    for (name, claims) in by_name {
        let mut groups: BTreeMap<u64, Vec<(usize, Claim)>> = BTreeMap::new();
        for (p, c) in claims {
            groups.entry(c.value).or_default().push((p, c));
        }
        if groups.len() < 2 {
            continue;
        }
        let mut gs: Vec<(u64, usize, Vec<Claim>)> = groups
            .into_iter()
            .map(|(v, cs)| (v, cs[0].0, cs.into_iter().map(|(_, c)| c).collect()))
            .collect();
        // 次数多的在前；次数一样，先出现的在前（先出现的多半是对着代码推导出来的那一处）。
        gs.sort_by(|a, b| b.2.len().cmp(&a.2.len()).then(a.1.cmp(&b.1)));
        out.push(Conflict {
            name,
            groups: gs.into_iter().map(|(v, _, cs)| (v, cs)).collect(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(name: &str, text: &str) -> (String, String) {
        (name.to_string(), text.to_string())
    }

    #[test]
    fn extracts_macro_equals_number_in_prose_forms() {
        let t = "`SC_NBINS = 36`\n**SC_NPSIZES** ≈ 199 档\nSC_NSIZES 为 232\n`SC_LARGE_MINCLASS` = 16 KB\nSC_LG_MAX_LOOKUP＝12";
        let c = extract_claims("a.md", t);
        let got: Vec<(&str, u64)> = c.iter().map(|c| (c.name.as_str(), c.value)).collect();
        assert_eq!(
            got,
            vec![
                ("SC_NBINS", 36),
                ("SC_NPSIZES", 199),
                ("SC_NSIZES", 232),
                ("SC_LARGE_MINCLASS", 16384),
                ("SC_LG_MAX_LOOKUP", 12)
            ]
        );
        assert_eq!(c[0].span, "SC_NBINS = 36");
        assert_eq!(c[0].line, 1);
    }

    #[test]
    fn expressions_code_blocks_notes_and_conditions_are_ignored() {
        let t = "#### 试学提问：SC_NBINS = 35 是不是写错了\nSC_NBINS = 4 * 9\nSC_NSIZES = 1 << 3\nSC_LG_X = 2^62\nCPU = 4\nSC_ABC 为 small 档数\n\
                 若换成 16K 页，SC_NBINS = 44\n> ⚠️ 待确认（试学提问 n1）：SC_NBINS = 39\n```c\n#define SC_NBINS = 99\n```\nSC_OK = 7";
        let c = extract_claims("a.md", t);
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!((c[0].name.as_str(), c[0].value), ("SC_OK", 7));
        // 算式的结果不记在最后一个宏头上
        assert!(
            extract_claims("a.md", "NGROUP+LG_QUANTUM=6，SC_LG_NGROUP + LG_QUANTUM = 6").is_empty()
        );
        // 比较、减法算式、反事实
        let cf = "#if SC_LG_TINY_MIN == 0 守卫\nSC_NTINY = LG_QUANTUM − SC_LG_TINY_MIN = 4 − 3 = 1\n改后（`SC_LG_NGROUP = 1`）\n代入 SC_LG_NGROUP = 1 时";
        assert!(
            extract_claims("a.md", cf).is_empty(),
            "{:?}",
            extract_claims("a.md", cf)
        );
    }

    /// jemalloc 实测：36 写了多处、39 写了一处 → 39 是可疑的。
    #[test]
    fn contradictory_values_are_reported_with_majority_first() {
        let files = vec![
            f(
                "sections/00_overview.md",
                "总览：SC_NSIZES = 232，SC_NBINS 为 39",
            ),
            f(
                "sections/k1/0_logic.md",
                "推导 SC_NBINS = 36\n再算 `SC_NBINS` = 36",
            ),
            f(
                "sections/k1/9_summary.md",
                "本章小结 `SC_NBINS = 39`\nSC_NSIZES = 232",
            ),
            f("sections/k2/3_solutions.md", "SC_NBINS=36"),
        ];
        let cs = conflicts(&files);
        assert_eq!(cs.len(), 1, "{cs:?}");
        let c = &cs[0];
        assert_eq!(c.name, "SC_NBINS");
        assert_eq!(c.groups[0].0, 36);
        assert_eq!(c.groups[0].1.len(), 3);
        let sus = c.suspects();
        assert_eq!(sus.len(), 2, "{sus:?}");
        assert!(sus.iter().all(|s| s.value == 39));
        let d = c.describe();
        assert!(
            d.contains("36（3 处")
                && d.contains("39（2 处")
                && d.contains("sections/k1/9_summary.md 第 1 行"),
            "{d}"
        );
        assert!(d.contains("前 N 个档位"), "要提醒换了说法的同一个数: {d}");
        assert_eq!(c.signature(), "numeric:SC_NBINS|36,39");
    }

    #[test]
    fn consistent_values_and_unit_equivalents_do_not_conflict() {
        let files = vec![
            f("a.md", "SC_LARGE_MINCLASS = 16384\nSC_NSIZES = 232"),
            f(
                "b.md",
                "`SC_LARGE_MINCLASS` = **16 KB**\nSC_NSIZES 等于 232",
            ),
        ];
        assert!(conflicts(&files).is_empty());
    }

    #[test]
    fn ties_put_the_earlier_value_first() {
        let files = vec![f("a.md", "SC_X_Y = 1"), f("b.md", "SC_X_Y = 2")];
        let cs = conflicts(&files);
        assert_eq!(cs[0].groups[0].0, 1);
        // 一样多：分不出多数派，两边都是可疑的
        assert_eq!(cs[0].suspects().len(), 2);
        let three = vec![f("a.md", "SC_X_Y = 1\nSC_X_Y = 1"), f("b.md", "SC_X_Y = 2")];
        let c3 = conflicts(&three);
        assert_eq!(c3[0].suspects().len(), 1);
        assert_eq!(c3[0].suspects()[0].value, 2);
    }

    /// 手动探针：`LEARN_FACTS_DIR=<学习工作区> cargo test -- --ignored probe_real_docs --nocapture`。
    #[test]
    #[ignore]
    fn probe_real_docs() {
        let Ok(dir) = std::env::var("LEARN_FACTS_DIR") else {
            return;
        };
        let base = std::path::Path::new(&dir);
        let mut files: Vec<(String, String)> = crate::learn_novice::walk(&base.join("sections"))
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "md"))
            .filter_map(|p| {
                let rel = p.strip_prefix(base).ok()?.to_string_lossy().into_owned();
                let top_level_chapter =
                    rel.matches('/').count() == 1 && rel != "sections/00_overview.md";
                (!top_level_chapter).then(|| (rel, std::fs::read_to_string(&p).unwrap_or_default()))
            })
            .collect();
        files.sort();
        for c in conflicts(&files) {
            println!("{}", c.describe());
        }
    }
}
