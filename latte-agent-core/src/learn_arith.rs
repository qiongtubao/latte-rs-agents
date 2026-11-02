//! 学习文档里「算式 = 结果」的机械核对。
//!
//! # 动机
//!
//! 学习文档到处是推演：`4 * (62 - 6 + 1) - 1 = 227`、`(16 - 8) / 16 = 50%`、`2^62 + 4 × 2^60 = 2^63`。
//! 模型心算会错，而读者最容易当真的就是这种带等号的数字。引擎能把算式抠出来自己算一遍。
//!
//! # 判据
//!
//! 正文（不含代码块）里一段由数字、运算符、括号和 `=` 组成的连续片段，把 `=` 两边各自求值，
//! 不相等就是错。运算符：`+ - − * × ÷ / ^ << >>`、LaTeX 的 `\times \cdot \div \ll \gg`、
//! 上标数字（`2⁶²`）。优先级按 C：`^` > `* /` > `+ -` > `<< >>`。
//!
//! 刻意保守，宁可漏判：
//! - 片段前后挨着标识符（`SC_NGROUP * 57 - 1 = 227`、`= 36B`）是含变量的表达式，只截一段会算错，整段不判；
//! - 片段后紧跟英文单位（`16 * 1024 = 16 KB`）不判；
//! - `≈` 不是 `=`，不判；`<=`、`>=`、`!=`、`==` 不是等号；
//! - 小数按右边的位数四舍五入再比；整数右边、左边除不尽时，取整后相等也算对（C 整数除法）；
//! - 含「≠ / 不等于 / 算错 / 误写」的行不判（在讲错误写法）。

use crate::learn_novice::Issue;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Tok {
    Num(f64),
    Op(&'static str),
    Lp,
    Rp,
    Eq,
    Ident,
    Other,
}

#[derive(Debug, Clone, Copy)]
struct Span {
    tok: Tok,
    start: usize,
    end: usize,
}

const SUPERSCRIPT: &str = "⁰¹²³⁴⁵⁶⁷⁸⁹";

fn superscript_digit(c: char) -> Option<u32> {
    SUPERSCRIPT.chars().position(|s| s == c).map(|p| p as u32)
}

fn tokenize(line: &str) -> Vec<Span> {
    let b: Vec<(usize, char)> = line.char_indices().collect();
    let mut out: Vec<Span> = Vec::new();
    let mut i = 0;
    let mut brace_stack: Vec<bool> = Vec::new();
    let at = |k: usize| b.get(k).map(|x| x.1);
    let pos = |k: usize| b.get(k).map_or(line.len(), |x| x.0);
    let push = |out: &mut Vec<Span>, tok: Tok, from: usize, to: usize| out.push(Span { tok, start: pos(from), end: pos(to) });
    while i < b.len() {
        let c = b[i].1;
        if c == '*' && at(i + 1) == Some('*') {
            // 加粗标记 `**`：透明，不当乘方。
            i += 2;
            continue;
        }
        if c.is_whitespace() || c == '$' || c == '`' {
            i += 1;
            continue;
        }
        if c.is_ascii_digit() || (c == '.' && at(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            let mut j = i;
            let mut text = String::new();
            while j < b.len() {
                let d = b[j].1;
                if d.is_ascii_digit() || (d == '.' && at(j + 1).is_some_and(|x| x.is_ascii_digit())) {
                    text.push(d);
                    j += 1;
                } else if d == ','
                    && (1..=3).all(|k| at(j + k).is_some_and(|x| x.is_ascii_digit()))
                    && !at(j + 4).is_some_and(|x| x.is_ascii_digit())
                {
                    j += 1; // 千分位 16,384
                } else {
                    break;
                }
            }
            match text.parse::<f64>() {
                Ok(v) => push(&mut out, Tok::Num(v), i, j),
                Err(_) => push(&mut out, Tok::Other, i, j),
            }
            i = j;
            continue;
        }
        if let Some(d) = superscript_digit(c) {
            let mut v = d;
            let mut j = i + 1;
            while let Some(d2) = at(j).and_then(superscript_digit) {
                v = v * 10 + d2;
                j += 1;
            }
            push(&mut out, Tok::Op("^"), i, i + 1);
            push(&mut out, Tok::Num(f64::from(v)), i, j);
            i = j;
            continue;
        }
        if (c.is_ascii_alphabetic()) || c == '_' {
            let mut j = i;
            while at(j).is_some_and(|x| x.is_ascii_alphanumeric() || x == '_') {
                j += 1;
            }
            push(&mut out, Tok::Ident, i, j);
            i = j;
            continue;
        }
        if c == '\\' {
            let mut j = i + 1;
            while at(j).is_some_and(|x| x.is_ascii_alphabetic()) {
                j += 1;
            }
            if j == i + 1 {
                if at(i + 1) == Some('%') {
                    push(&mut out, Tok::Other, i, i + 2);
                } // 其余 `\,` `\;` 之类单字符排版命令：透明。
                i += 2;
                continue;
            }
            let name: String = (i + 1..j).filter_map(at).collect();
            let tok = match name.as_str() {
                "times" | "cdot" => Tok::Op("*"),
                "div" => Tok::Op("/"),
                "ll" => Tok::Op("<<"),
                "gg" => Tok::Op(">>"),
                "left" | "right" => {
                    i = j;
                    continue;
                }
                _ => Tok::Ident,
            };
            push(&mut out, tok, i, j);
            i = j;
            continue;
        }
        if (c == '<' && at(i + 1) == Some('<')) || (c == '>' && at(i + 1) == Some('>')) {
            push(&mut out, Tok::Op(if c == '<' { "<<" } else { ">>" }), i, i + 2);
            i += 2;
            continue;
        }
        if c == '=' {
            let prev = i.checked_sub(1).and_then(at);
            if prev.is_some_and(|p| "<>!=:".contains(p)) || at(i + 1) == Some('=') {
                // `<=` `>=` `!=` `==` `:=`：不是等式。
                let skip = if at(i + 1) == Some('=') { 2 } else { 1 };
                push(&mut out, Tok::Other, i, i + skip);
                i += skip;
                continue;
            }
            push(&mut out, Tok::Eq, i, i + 1);
            i += 1;
            continue;
        }
        if c == '^' && at(i + 1) == Some('{') {
            push(&mut out, Tok::Op("^"), i, i + 1);
            push(&mut out, Tok::Lp, i + 1, i + 2);
            brace_stack.push(true);
            i += 2;
            continue;
        }
        if c == '{' {
            brace_stack.push(false);
            push(&mut out, Tok::Other, i, i + 1);
            i += 1;
            continue;
        }
        if c == '}' {
            let tok = if brace_stack.pop() == Some(true) { Tok::Rp } else { Tok::Other };
            push(&mut out, tok, i, i + 1);
            i += 1;
            continue;
        }
        let tok = match c {
            '＝' => Tok::Eq,
            '+' => Tok::Op("+"),
            '-' | '−' | '–' => Tok::Op("-"),
            '*' | '×' | '·' => Tok::Op("*"),
            '/' | '÷' => Tok::Op("/"),
            '^' => Tok::Op("^"),
            // 位运算 / 取反：本检查不求值，当作「含变量的表达式」的屏障。
            '~' | '!' | '&' | '|' => Tok::Ident,
            '(' | '（' => Tok::Lp,
            ')' | '）' => Tok::Rp,
            _ => Tok::Other,
        };
        push(&mut out, tok, i, i + 1);
        i += 1;
    }
    out
}

/// 递归下降求值。`None` = 语法不对 / 除零。
struct Eval<'a> {
    toks: &'a [Tok],
    pos: usize,
}

impl Eval<'_> {
    fn peek(&self) -> Option<Tok> {
        self.toks.get(self.pos).copied()
    }
    fn shift(&mut self) -> Option<f64> {
        let mut v = self.add()?;
        while let Some(Tok::Op(o @ ("<<" | ">>"))) = self.peek() {
            self.pos += 1;
            let r = self.add()?;
            if r < 0.0 || r.fract() != 0.0 || r > 120.0 {
                return None;
            }
            let f = 2f64.powi(r as i32);
            v = if o == "<<" { v * f } else { (v / f).floor() };
        }
        Some(v)
    }
    fn add(&mut self) -> Option<f64> {
        let mut v = self.mul()?;
        while let Some(Tok::Op(o @ ("+" | "-"))) = self.peek() {
            self.pos += 1;
            let r = self.mul()?;
            v = if o == "+" { v + r } else { v - r };
        }
        Some(v)
    }
    fn mul(&mut self) -> Option<f64> {
        let mut v = self.pow()?;
        while let Some(Tok::Op(o @ ("*" | "/"))) = self.peek() {
            self.pos += 1;
            let r = self.pow()?;
            if o == "/" {
                if r == 0.0 {
                    return None;
                }
                v /= r;
            } else {
                v *= r;
            }
        }
        Some(v)
    }
    fn pow(&mut self) -> Option<f64> {
        let base = self.unary()?;
        if let Some(Tok::Op("^")) = self.peek() {
            self.pos += 1;
            let e = self.pow()?;
            if e.abs() > 300.0 {
                return None;
            }
            return Some(base.powf(e));
        }
        Some(base)
    }
    fn unary(&mut self) -> Option<f64> {
        if let Some(Tok::Op("-")) = self.peek() {
            self.pos += 1;
            return Some(-self.unary()?);
        }
        self.atom()
    }
    fn atom(&mut self) -> Option<f64> {
        match self.peek()? {
            Tok::Num(v) => {
                self.pos += 1;
                Some(v)
            }
            Tok::Lp => {
                self.pos += 1;
                let v = self.shift()?;
                if self.peek() == Some(Tok::Rp) {
                    self.pos += 1;
                    Some(v)
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

fn eval(toks: &[Tok]) -> Option<f64> {
    if toks.is_empty() {
        return None;
    }
    let mut e = Eval { toks, pos: 0 };
    let v = e.shift()?;
    (e.pos == toks.len() && v.is_finite()).then_some(v)
}

fn decimals_of(text: &str) -> usize {
    text.rsplit_once('.').map_or(0, |(_, d)| d.chars().take_while(char::is_ascii_digit).count())
}

fn equal(lhs: f64, rhs: f64, rhs_text: &str) -> bool {
    let d = decimals_of(rhs_text);
    if d > 0 {
        let m = 10f64.powi(d as i32);
        return (lhs * m).round() == (rhs * m).round();
    }
    if lhs.fract() != 0.0 && rhs.fract() == 0.0 {
        // C 整数除法：向下取整或四舍五入相等都算对。
        return lhs.floor() == rhs || lhs.round() == rhs;
    }
    let scale = lhs.abs().max(rhs.abs());
    (lhs - rhs).abs() <= if scale < 1e12 { 1e-9 } else { scale * 1e-12 }
}

/// 一处不成立的算式。
#[derive(Debug, Clone, PartialEq)]
pub struct Mismatch {
    pub file: String,
    pub line: usize,
    /// 原文里这段算式（含等号两边）。
    pub expr: String,
    /// 等号左边算出来的值，和右边写的数。
    pub computed: f64,
    pub written: f64,
}

fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v:.4}").trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

impl Mismatch {
    pub fn describe(&self) -> String {
        format!(
            "{} 第 {} 行的算式「{}」不成立：等号左边算出来是 {}，写的是 {}",
            self.file,
            self.line,
            self.expr,
            fmt_num(self.computed),
            fmt_num(self.written)
        )
    }

    pub fn to_issue(&self) -> Issue {
        Issue {
            kind: "doc".into(),
            chapter: crate::learn_novice::chapter_of(&self.file),
            file: self.file.clone(),
            quote: self.expr.clone(),
            problem: format!(
                "{}。请重新算一遍：是结果抄错了，还是算式里的某个数 / 运算写错了？对照源码里的取值，把算式和得数改成一致",
                self.describe()
            ),
            source: "engine".into(),
            status: "open".into(),
            check: self.check(),
            ..Default::default()
        }
    }

    /// 稳定标识：同一段算式文字。
    pub fn check(&self) -> String {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in self.expr.bytes().filter(|b| !b.is_ascii_whitespace()) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("arith:{h:016x}")
    }
}

const EXEMPT: &[&str] = &["≠", "不等于", "算错", "误写", "错写", "写成了"];

fn runnable(t: Tok) -> bool {
    matches!(t, Tok::Num(_) | Tok::Op(_) | Tok::Lp | Tok::Rp | Tok::Eq)
}

/// 一个文件里全部不成立的算式。
pub fn scan(file: &str, text: &str) -> Vec<Mismatch> {
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
        if crate::learn_cite::is_pending_note(t) || t.starts_with("#### 试学提问") || EXEMPT.iter().any(|w| line.contains(w)) {
            continue;
        }
        if !line.contains('=') && !line.contains('＝') {
            continue;
        }
        let spans = tokenize(line);
        let mut i = 0;
        while i < spans.len() {
            if !runnable(spans[i].tok) {
                i += 1;
                continue;
            }
            let mut j = i;
            while j < spans.len() && runnable(spans[j].tok) {
                j += 1;
            }
            let run = &spans[i..j];
            // 片段前后挨着标识符：含变量的表达式只截了一段。
            let prev_ident = i > 0 && spans[i - 1].tok == Tok::Ident;
            let next_ident = j < spans.len() && spans[j].tok == Tok::Ident;
            let starts_ok = matches!(run[0].tok, Tok::Num(_) | Tok::Lp | Tok::Eq);
            let mut parts: Vec<Vec<Span>> = vec![Vec::new()];
            for s in run {
                if s.tok == Tok::Eq {
                    parts.push(Vec::new());
                } else {
                    parts.last_mut().unwrap().push(*s);
                }
            }
            if parts.len() >= 2 && starts_ok && !next_ident {
                // 最左一段的左边是变量（`X = 3 + 4 = 7` 里的 `= 3 + 4 = 7`）时，只核对后面各段之间。
                let first = usize::from(prev_ident || run[0].tok == Tok::Eq);
                for w in first..parts.len() - 1 {
                    let (a, b) = (&parts[w], &parts[w + 1]);
                    let (Some(fa), Some(fb), Some(lb)) = (a.first(), b.first(), b.last()) else { continue };
                    let ta: Vec<Tok> = a.iter().map(|s| s.tok).collect();
                    let tb: Vec<Tok> = b.iter().map(|s| s.tok).collect();
                    // 两边至少有一边是真算式（有运算符）；纯 `a = b` 交给别的检查。
                    let has_op = |t: &[Tok]| t.iter().any(|x| matches!(x, Tok::Op(_)));
                    if !has_op(&ta) && !has_op(&tb) {
                        continue;
                    }
                    let (Some(mut va), Some(vb)) = (eval(&ta), eval(&tb)) else { continue };
                    // 右边是百分数（`= 49.2%`、`= 49.2\%`）：左边的比值乘 100。
                    let rhs_end_pct = w + 2 == parts.len() && j < spans.len() && line[spans[j].start..spans[j].end].ends_with('%');
                    if rhs_end_pct {
                        va *= 100.0;
                    }
                    if !equal(va, vb, &line[fb.start..lb.end]) {
                        out.push(Mismatch {
                            file: file.to_string(),
                            line: n + 1,
                            expr: line[fa.start..lb.end].trim().to_string(),
                            computed: va,
                            written: vb,
                        });
                    }
                }
            }
            i = j;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bad(t: &str) -> Vec<Mismatch> {
        scan("a.md", t)
    }

    #[test]
    fn correct_arithmetic_passes_in_all_notations() {
        for t in [
            "4 * (62 - 6 + 1) - 1 = 227",
            "1 + 4 + 227 = 232",
            "$57 \\times 4 = 228$",
            "$2^{62} + 4 \\times 2^{60} = 2^{63}$",
            "2⁶² + 4 × 2⁶⁰ = 2⁶³",
            "5 + 32 − 1 = 36（共 36 档）",
            "16384 / 16 = 1024 档",
            "(1 << 3) * 8 = 64",
            "4096 >> 3 = 512，512 + 1 = 513",
            "(16 - 8) / 16 = 0.5",
            "127 / 384 = 0.3307 说明",
            "16,384 = 4 * 4096",
            "100 / 3 = 33",
            "**4 * 8 = 32**",
            "(128 - 65) / 128 = 49.2%",
            "$1/5 = 20\\%$",
            "占 (32−17)/32 = 46.88 %",
        ] {
            assert!(bad(t).is_empty(), "{t}: {:?}", bad(t));
        }
    }

    #[test]
    fn wrong_arithmetic_is_reported_with_computed_value() {
        let m = bad("所以 4 * (62 - 6 + 1) - 1 = 228 档");
        assert_eq!(m.len(), 1, "{m:?}");
        assert_eq!((m[0].computed, m[0].written), (227.0, 228.0));
        assert_eq!(m[0].expr, "4 * (62 - 6 + 1) - 1 = 228");
        assert!(m[0].describe().contains("算出来是 227，写的是 228"), "{}", m[0].describe());
        // 链式：只报错的那一段
        let c = bad("5 + 32 - 1 = 36 = 6 * 7");
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(c[0].written, 42.0);
        // 小数按右边的位数比
        assert_eq!(bad("127 / 384 = 0.33").len(), 0);
        assert_eq!(bad("127 / 384 = 0.34").len(), 1);
        // LaTeX 与上标
        assert_eq!(bad("$57 \\times 4 = 238$").len(), 1);
        assert_eq!(bad("2⁶² + 2⁶² = 2⁶⁴").len(), 1);
        // 百分数：比值乘 100 之后再比
        assert_eq!(bad("(128 - 65) / 128 = 49.3%").len(), 1);
    }

    #[test]
    fn expressions_with_variables_units_and_non_equalities_are_left_alone() {
        for t in [
            "SC_NGROUP * 57 - 1 = 227",        // 含变量，截出来的 `57 - 1` 不是完整表达式
            "SC_NSIZES = SC_NTINY + 4 + 3 = 8", // 变量在前
            "16 * 1024 = 16 KB",                // 单位
            "x <= 3 + 4",
            "a >= 5 - 1",
            "1 + 1 ≈ 3",
            "(size + 7) >> 3 = idx",
            "if (n == 3 + 4)",
            "这是错的：1 + 1 ≠ 3，不等于 3",
            "不对，1 + 1 = 3 是算错的写法",
            "SC_NGROUP - 1 = 3",                // 变量后的减号是二元运算
            "~(0-1) = ~(-1) = 0",              // 按位取反不求值
            "x & (8 - 1) = 7",
        ] {
            assert!(bad(t).is_empty(), "{t}: {:?}", bad(t));
        }
        // 代码块不看
        assert!(bad("```c\nint x = 1 + 1 = 3;\n```\n").is_empty());
        // 待确认注记不看
        assert!(bad("> ⚠️ 待确认（试学提问 n1）：2 * 3 = 7").is_empty());
    }

    #[test]
    fn mismatch_becomes_a_checkable_engine_issue() {
        let m = &bad("共 4 * 57 = 238 档")[0];
        let i = m.to_issue();
        assert_eq!((i.kind.as_str(), i.source.as_str(), i.quote.as_str()), ("doc", "engine", "4 * 57 = 238"));
        assert!(i.check.starts_with("arith:") && i.evidence.is_empty(), "{i:?}");
        // 同一段算式文字，标识稳定
        assert_eq!(i.check, bad("再说一次 4 * 57 = 238。")[0].check());
    }

    /// 手动探针：`LEARN_ARITH_DIR=<学习工作区> cargo test -- --ignored probe_real_docs --nocapture`。
    #[test]
    #[ignore]
    fn probe_real_docs() {
        let Ok(dir) = std::env::var("LEARN_ARITH_DIR") else { return };
        let base = std::path::Path::new(&dir);
        let mut n = 0;
        for p in crate::learn_novice::walk(&base.join("sections")) {
            if p.extension().is_none_or(|e| e != "md") {
                continue;
            }
            let rel = p.strip_prefix(base).unwrap().to_string_lossy().into_owned();
            if rel.matches('/').count() == 1 && rel != "sections/00_overview.md" {
                continue;
            }
            for m in scan(&rel, &std::fs::read_to_string(&p).unwrap_or_default()) {
                n += 1;
                println!("{}", m.describe());
            }
        }
        println!("mismatches={n}");
    }
}
