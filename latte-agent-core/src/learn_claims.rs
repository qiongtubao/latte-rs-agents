//! 学习文档里「带出处的断言」的取证核对：引擎把断言和它引的源码原文配好对，交给核对员判对错。
//!
//! # 动机（实测 LAT-110 jemalloc，会话 ui-75536）
//!
//! 引擎能机械核对的只有行号和宏数值，管不了「这句话对不对」：
//! - 把 `batch_alloc` 里的 `if (ind < SC_NBINS)` 说成「小对象 fast path 的分流」；
//! - 引 `large.c:28-30` 说「判空逻辑是 `usize == 0 ||…`」，源码里变量叫 `ausize`；
//! - 说「`1ULL << 63` 会溢出」，它是无符号数。
//!
//! 这些错误都带着出处，**证据就在引用的那几行里**。之前靠小白试学去发现：小白要自己找文件、
//! 自己读、自己对照，读完 5 章提出 0 个问题。召回率低的原因不在小白不努力，而在「取证」这一步
//! 全压在一个工具循环的 agent 身上。
//!
//! # 做法
//!
//! 取证由引擎做，判断才交给模型：
//! 1. 逐行扫描正文（不含代码块），凡是带 `文件:行号` 出处、且含数字或代码片段的行就是一条断言；
//! 2. 引擎把每条断言引用的源码行（带行号，前后各扩几行）原样配在旁边；
//! 3. 一批断言 + 证据放进一次**无工具、无历史**的模型调用，模型只回答「证据是否直接推翻了这句话」；
//! 4. 判错必须附 `source_quote`——从证据里**逐字**抄的一行。引擎核对这行真在证据里，核不上的判决丢弃。
//!    这道门挡住模型凭想象判错；
//! 5. 判决为错的变成引擎提出的问题，走辅导员纠正 → 复查的老路。
//!
//! 同一条断言（原文 + 证据都没变）只核一次，缓存在 `novice/fact_cache.json`，返工时只核改过的。

use crate::learn_cite::SourceIndex;
use crate::learn_novice::Issue;

/// 单条断言配的证据最多这么多行（超过取头尾各半）。
const MAX_EVIDENCE_LINES: usize = 60;
/// 出处区间前后各扩几行（单行出处多扩一点，读者引一行多半想说的是它所在的语句）。
const PAD: usize = 2;
const PAD_SINGLE: usize = 3;
/// 一批断言 + 证据的字符数上限。
pub const BATCH_CHARS: usize = 12_000;
/// 一组断言核对完的结果（见 `workflow::learn_fact_check`）。
#[derive(Debug, Default)]
pub struct GroupResult {
    /// 回答了、且没被判错的断言（进缓存，下次不再核）。
    pub checked: Vec<Claim>,
    /// 采信的判错。
    pub wrong: Vec<(Claim, Verdict)>,
    /// 被引擎丢弃的判决及原因。
    pub dropped: Vec<String>,
    /// 模型始终没给出可用结果的断言数。
    pub failed: usize,
}

/// 一次试学最多核对多少条断言（防文档异常大时烧爆）。
pub const MAX_CLAIMS: usize = 600;
/// 一次最多变成多少条问题交给辅导员。
pub const MAX_ISSUES: usize = 25;

/// 一条断言。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    /// 批内编号（`c1`…），由 [`number`] 赋值。
    pub id: String,
    /// 文档文件（相对学习工作区）。
    pub file: String,
    /// 文档里的行号（1 起）。
    pub line: usize,
    /// 这一行文字（已截断）。
    pub text: String,
    /// 引用（原 token，如 `src/sc.c:241-265`）。
    pub cites: Vec<String>,
    /// 渲染好的证据：每条引用一段，带行号。
    pub evidence: String,
}

impl Claim {
    /// 缓存键：原文 + 证据都没变就不必再核。
    pub fn key(&self) -> String {
        fnv(&format!("{}\u{0}{}", self.text, self.evidence))
    }

    fn size(&self) -> usize {
        self.text.chars().count() + self.evidence.chars().count() + 80
    }
}

fn fnv(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        s.chars().take(n).collect::<String>() + "…"
    } else {
        s.to_string()
    }
}

/// 一行里的全部 `文件:行号` 引用：(原 token, 文件, 区间)。
fn line_cites(line: &str, idx: &mut SourceIndex) -> Vec<(String, String, (usize, usize))> {
    let mut out: Vec<(String, String, (usize, usize))> = Vec::new();
    for tok in line.split(|c: char| !(c.is_ascii_alphanumeric() || "_./+-:".contains(c))) {
        let tok = tok.trim_matches(|c| c == '.' || c == ':' || c == '-');
        let Some((path, ranges)) = crate::learn_cite::parse_cite_pub(tok) else { continue };
        let Some(rel) = idx.resolve(&path) else { continue };
        let Some(&(a, b)) = ranges.first() else { continue };
        if !out.iter().any(|(t, ..)| t == tok) {
            out.push((tok.to_string(), rel, (a, b)));
        }
    }
    out
}

/// 去掉出处之后，这一行是不是在**断言**什么：含数字，或含像代码的行内片段。
/// 只写「详见 `sc.h:233`」这种纯指路的行没有可核对的内容。
fn is_assertive(line: &str, cite_tokens: &[String]) -> bool {
    let mut rest = line.to_string();
    for t in cite_tokens {
        rest = rest.replace(t.as_str(), " ");
    }
    if rest.chars().any(|c| c.is_ascii_digit()) {
        return true;
    }
    let mut in_code = false;
    let mut cur = String::new();
    for c in rest.chars() {
        if c == '`' {
            if in_code && cur.chars().any(|c| "()=<>[]&|+*/%!".contains(c)) {
                return true;
            }
            in_code = !in_code;
            cur.clear();
        } else if in_code {
            cur.push(c);
        }
    }
    false
}

fn render_evidence(rel: &str, token: &str, (a, b): (usize, usize), lines: &[String]) -> String {
    let pad = if a == b { PAD_SINGLE } else { PAD };
    let lo = a.saturating_sub(pad).max(1);
    let hi = (b + pad).min(lines.len());
    if lo > hi {
        return String::new();
    }
    let mut out = format!("【{token}】（{rel}）\n");
    let idxs: Vec<usize> = (lo..=hi).collect();
    let shown: Vec<Option<usize>> = if idxs.len() > MAX_EVIDENCE_LINES {
        let half = MAX_EVIDENCE_LINES / 2;
        idxs[..half].iter().map(|&n| Some(n)).chain([None]).chain(idxs[idxs.len() - half..].iter().map(|&n| Some(n))).collect()
    } else {
        idxs.iter().map(|&n| Some(n)).collect()
    };
    for n in shown {
        match n {
            Some(n) => out.push_str(&format!("{n}: {}\n", lines[n - 1].trim_end())),
            None => out.push_str("…（中间省略）\n"),
        }
    }
    out
}

/// 从一批文档里抽出全部可核对的断言。`files` 是 (相对工作区的文件名, 全文)，顺序即文档顺序。
pub fn extract(files: &[(String, String)], idx: &mut SourceIndex) -> Vec<Claim> {
    let mut out = Vec::new();
    for (file, text) in files {
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
            // 待确认注记转述的是别人指出的错误；试学提问小节同理。
            if crate::learn_cite::is_pending_note(t) || t.starts_with("#### 试学提问") {
                continue;
            }
            let cites = line_cites(line, idx);
            if cites.is_empty() {
                continue;
            }
            let tokens: Vec<String> = cites.iter().map(|(t, ..)| t.clone()).collect();
            if !is_assertive(line, &tokens) {
                continue;
            }
            let mut evidence = String::new();
            let mut used = Vec::new();
            for (tok, rel, range) in cites.iter().take(3) {
                let Some(lines) = idx.lines(rel) else { continue };
                let e = render_evidence(rel, tok, *range, &lines);
                if !e.is_empty() {
                    evidence.push_str(&e);
                    used.push(tok.clone());
                }
            }
            if used.is_empty() {
                continue;
            }
            out.push(Claim {
                id: String::new(),
                file: file.clone(),
                line: n + 1,
                text: clip(line.trim(), 700),
                cites: used,
                evidence,
            });
        }
    }
    // 完全相同的断言（同一句话 + 同样的证据）只核一次。
    let mut seen = std::collections::HashSet::new();
    out.retain(|c| seen.insert(c.key()));
    out
}

/// 按字符数切批。返回每批的下标。
pub fn batches(claims: &[Claim], max_chars: usize) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = Vec::new();
    let mut cur: Vec<usize> = Vec::new();
    let mut size = 0;
    for (i, c) in claims.iter().enumerate() {
        if !cur.is_empty() && size + c.size() > max_chars {
            out.push(std::mem::take(&mut cur));
            size = 0;
        }
        size += c.size();
        cur.push(i);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

pub const SYSTEM_PROMPT: &str = "你是源码事实核对员。下面给你若干条学习文档里的陈述，每条旁边附着它所引用的源码原文（带行号）。\
你的任务：判断**证据是否直接推翻了这句话里某个可核对的说法**。\n\n\
判 wrong 的情形（必须是证据里能直接看到的）：\n\
- 数字、常量值、行号对应的内容与证据不符；\n\
- 条件、运算符、比较方向、变量/函数名与证据不符（如文档说 `usize == 0`，源码里是 `ausize == 0`）；\n\
- 文档说某行/某函数「做了 X」，证据里它做的是 Y（如把批量分配里的取 nregs 说成快路径分流）；\n\
- 文档把代码「摘录」出来，但与证据里的原文不一致。\n\n\
不要判 wrong：\n\
- 证据没有覆盖这句话的说法（讲调用方、性能、设计动机、别的文件）——这种当作 ok，不要猜；\n\
- 措辞、翻译、详略、比喻上的差异；\n\
- 文档写的行号与那个符号 / 那行代码的真实位置只差 1-2 行（证据里它清楚可见，只是前一行是 `/* implicit */`、空行或注释），这是行号漂移不是内容错误，不判；只有符号 / 内容根本不在引用区间附近才判；\n\
- 你只是觉得文档「不够准确」但证据没有直接矛盾。\n\n\
输出：只输出一个 JSON 对象，不要任何别的文字（推理在心里做，不要写进回答）。只列判 wrong 的，没有就 `{\"verdicts\": []}`：\n\
{\"verdicts\": [{\"id\": \"c3\", \"source_quote\": \"从证据里逐字抄的一行（不含行号前缀）\", \"reason\": \"文档说了什么、源码实际是什么，一句话\"}]}\n\
`source_quote` 必须是证据里真实存在的一行原文；抄不出来就说明证据并不直接矛盾，不要判。";

/// 给 `ids`（claims 的下标）这一批构造用户消息，同时给批内的断言编号 `c1…`。
pub fn build_user_prompt(claims: &mut [Claim], ids: &[usize]) -> String {
    let mut out = String::from("逐条核对。\n\n");
    for (k, &i) in ids.iter().enumerate() {
        claims[i].id = format!("c{}", k + 1);
        let c = &claims[i];
        out.push_str(&format!(
            "### {}（{} 第 {} 行）\n文档原文：{}\n证据：\n{}\n",
            c.id, c.file, c.line, c.text, c.evidence
        ));
    }
    out
}

fn norm(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// 证据里去掉行号前缀后的纯代码。
fn evidence_plain(evidence: &str) -> String {
    evidence
        .lines()
        .filter(|l| !l.starts_with('【'))
        .map(|l| {
            let t = l.trim_start();
            let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
            if digits > 0 && t[digits..].starts_with(':') {
                t[digits + 1..].trim_start().to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 一条被引擎采信的判错。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// claims 里的下标。
    pub claim: usize,
    pub source_quote: String,
    pub reason: String,
}

/// 解析一批的回答。`ids` 与 [`build_user_prompt`] 同一批（`claims[i].id` 已赋值）。
/// 返回采信的判错和被丢弃的原因（引文不在证据里、id 不存在、格式不对）。
pub fn ingest(raw: &str, claims: &[Claim], ids: &[usize]) -> (Vec<Verdict>, Vec<String>) {
    let mut dropped = Vec::new();
    let (Some(a), Some(b)) = (raw.find('{'), raw.rfind('}')) else {
        return (Vec::new(), vec!["回答里没有 JSON".into()]);
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw[a..=b]) else {
        return (Vec::new(), vec!["回答不是合法 JSON".into()]);
    };
    let mut out = Vec::new();
    for item in v.get("verdicts").and_then(|x| x.as_array()).cloned().unwrap_or_default() {
        let id = item.get("id").and_then(|x| x.as_str()).unwrap_or("");
        let quote = item.get("source_quote").and_then(|x| x.as_str()).unwrap_or("").trim();
        let reason = item.get("reason").and_then(|x| x.as_str()).unwrap_or("").trim();
        let Some(&ci) = ids.iter().find(|&&i| claims[i].id == id) else {
            dropped.push(format!("{id}：批里没有这个编号"));
            continue;
        };
        // 引文去掉模型可能带上的行号前缀，再逐字比对证据。
        let q: String = quote
            .lines()
            .map(|l| {
                let t = l.trim_start();
                let d = t.chars().take_while(|c| c.is_ascii_digit()).count();
                if d > 0 && t[d..].starts_with(':') { t[d + 1..].trim_start() } else { t }
            })
            .collect::<Vec<_>>()
            .join("\n");
        if norm(&q).chars().count() < 6 || reason.is_empty() {
            dropped.push(format!("{id}：缺少有效的 source_quote 或 reason"));
            continue;
        }
        if !norm(&evidence_plain(&claims[ci].evidence)).contains(&norm(&q)) {
            dropped.push(format!("{id}：source_quote 不在证据里（模型凭想象判错）"));
            continue;
        }
        out.push(Verdict { claim: ci, source_quote: clip(quote, 200), reason: clip(reason, 240) });
    }
    (out, dropped)
}

/// 判错 → 引擎提出的问题。`evidence` 填第一条引用：辅导员回 FIXED 时，被指出的原文必须真的改掉。
pub fn to_issue(c: &Claim, v: &Verdict) -> Issue {
    Issue {
        kind: "doc".into(),
        chapter: crate::learn_novice::chapter_of(&c.file),
        file: c.file.clone(),
        quote: c.text.clone(),
        problem: format!(
            "核对员对照 `{}` 的源码，认为这句话有误：{}（源码原文：「{}」）",
            c.cites.join("`、`"),
            v.reason,
            v.source_quote
        ),
        evidence: c.cites.first().cloned().unwrap_or_default(),
        source: "engine".into(),
        status: "open".into(),
        check: format!("fact:{}", c.key()),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> (tempfile::TempDir, SourceIndex) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let mut src = String::new();
        for i in 1..=40 {
            src.push_str(&format!("int stmt_{i} = {i};\n"));
        }
        src.push_str("size_t ausize = sz_sa2u(usize, alignment);\nif (unlikely(ausize == 0 || ausize > SC_LARGE_MAXCLASS)) {\n\treturn NULL;\n}\n");
        std::fs::write(dir.path().join("src/large.c"), src).unwrap();
        let idx = SourceIndex::new(dir.path());
        (dir, idx)
    }

    fn files(t: &str) -> Vec<(String, String)> {
        vec![("sections/k1/3.md".to_string(), t.to_string())]
    }

    #[test]
    fn extracts_only_assertive_lines_with_resolvable_cites_outside_code_blocks() {
        let (_d, mut idx) = repo();
        let doc = "详见 `src/large.c:41`。\n\
                   判空逻辑是 `usize == 0 || usize > MAX`（`src/large.c:42`），第 41 行算 ausize。\n\
                   ```c\n// src/large.c:5 这里有 3 处\n```\n\
                   > ⚠️ 待确认（试学提问 n1）：src/large.c:42 写错了 7\n\
                   没有出处的断言：共 232 档。\n\
                   引了不存在的 `nope.c:3` 共 5 处。\n\
                   判空逻辑是 `usize == 0 || usize > MAX`（`src/large.c:42`），第 41 行算 ausize。\n";
        let c = extract(&files(doc), &mut idx);
        assert_eq!(c.len(), 1, "纯指路、代码块、待确认、无出处、解析不到、完全重复的都不算: {c:?}");
        assert_eq!(c[0].line, 2);
        assert_eq!(c[0].cites, vec!["src/large.c:42".to_string()]);
        // 单行出处前后各扩 3 行，带行号
        assert!(c[0].evidence.contains("39: int stmt_39 = 39;") && c[0].evidence.contains("42: if (unlikely(ausize == 0"), "{}", c[0].evidence);
        assert!(c[0].evidence.contains("44: }") && !c[0].evidence.contains("38:"), "前 3 行后 3 行，到文件尾为止: {}", c[0].evidence);
    }

    #[test]
    fn long_ranges_keep_head_and_tail() {
        let dir = tempfile::tempdir().unwrap();
        let src: String = (1..=120).map(|i| format!("int stmt_{i} = {i};\n")).collect();
        std::fs::write(dir.path().join("big.c"), src).unwrap();
        let mut idx = SourceIndex::new(dir.path());
        let doc = "整段逻辑共 100 行（`big.c:1-100`）。\n";
        let c = extract(&files(doc), &mut idx);
        let e = &c[0].evidence;
        assert!(e.contains("中间省略") && e.contains("\n1: int stmt_1") && e.contains("\n102: int stmt_102"), "{e}");
        assert!(!e.contains("\n50: "), "{e}");
    }

    #[test]
    fn batches_split_by_size_and_keep_order() {
        let mk = |n: usize| Claim { id: String::new(), file: "f".into(), line: n, text: "x".repeat(100), cites: vec![], evidence: "y".repeat(100) };
        let cs: Vec<Claim> = (0..7).map(mk).collect();
        let b = batches(&cs, 600);
        assert_eq!(b.iter().map(Vec::len).sum::<usize>(), 7);
        assert!(b.len() >= 2 && b[0][0] == 0 && b.concat() == (0..7).collect::<Vec<_>>());
        assert!(batches(&[], 600).is_empty());
    }

    /// 判错必须附逐字出自证据的引文；凭想象的、编号不存在的、格式不对的都丢弃。
    #[test]
    fn ingest_keeps_only_verdicts_whose_quote_is_in_the_evidence() {
        let (_d, mut idx) = repo();
        let doc = "判空逻辑是 `usize == 0 || usize > MAX`（`src/large.c:42`）\n\
                   变量 `stmt_7` 的值是 8（`src/large.c:7`）\n";
        let mut cs = extract(&files(doc), &mut idx);
        assert_eq!(cs.len(), 2);
        let ids: Vec<usize> = vec![0, 1];
        let _ = build_user_prompt(&mut cs, &ids);
        assert_eq!((cs[0].id.as_str(), cs[1].id.as_str()), ("c1", "c2"));
        let raw = r#"好的，结果如下：
{"verdicts": [
  {"id": "c1", "source_quote": "42: if (unlikely(ausize == 0 || ausize > SC_LARGE_MAXCLASS)) {", "reason": "文档写 usize，源码变量叫 ausize"},
  {"id": "c2", "source_quote": "int stmt_7 = 999;", "reason": "值不对"},
  {"id": "c9", "source_quote": "int stmt_7 = 7;", "reason": "编号不存在"},
  {"id": "c2", "source_quote": "x", "reason": ""}
]}"#;
        let (ok, dropped) = ingest(raw, &cs, &ids);
        assert_eq!(ok.len(), 1, "{ok:?} {dropped:?}");
        assert_eq!(ok[0].claim, 0);
        assert_eq!(dropped.len(), 3, "{dropped:?}");
        assert!(dropped.iter().any(|d| d.contains("不在证据里")) && dropped.iter().any(|d| d.contains("没有这个编号")));
        let issue = to_issue(&cs[0], &ok[0]);
        assert_eq!((issue.kind.as_str(), issue.source.as_str(), issue.chapter.as_str()), ("doc", "engine", "k1"));
        assert_eq!(issue.evidence, "src/large.c:42", "带依据：回 FIXED 时被指出的原文必须真的改掉");
        assert!(issue.problem.contains("ausize") && issue.check.starts_with("fact:"), "{issue:?}");
        // 空结果 / 非 JSON
        assert!(ingest(r#"{"verdicts": []}"#, &cs, &ids).0.is_empty());
        assert!(ingest("没有问题", &cs, &ids).1[0].contains("没有 JSON"));
    }

    #[test]
    fn cache_key_changes_when_text_or_evidence_changes() {
        let (_d, mut idx) = repo();
        let a = extract(&files("共 3 处（`src/large.c:5`）\n"), &mut idx);
        let b = extract(&files("共 4 处（`src/large.c:5`）\n"), &mut idx);
        let c = extract(&files("共 3 处（`src/large.c:6`）\n"), &mut idx);
        assert_ne!(a[0].key(), b[0].key());
        assert_ne!(a[0].key(), c[0].key());
        assert_eq!(a[0].key(), extract(&files("共 3 处（`src/large.c:5`）\n"), &mut idx)[0].key());
    }

    /// 手动探针：`LEARN_CLAIMS_DIR=<学习工作区> LEARN_CLAIMS_REPO=<仓库> cargo test -- --ignored probe_real_docs --nocapture`。
    #[test]
    #[ignore]
    fn probe_real_docs() {
        let (Ok(dir), Ok(repo)) = (std::env::var("LEARN_CLAIMS_DIR"), std::env::var("LEARN_CLAIMS_REPO")) else { return };
        let base = std::path::Path::new(&dir);
        let mut files: Vec<(String, String)> = crate::learn_novice::walk(&base.join("sections"))
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "md"))
            .filter_map(|p| {
                let rel = p.strip_prefix(base).ok()?.to_string_lossy().into_owned();
                let top = rel.matches('/').count() == 1 && rel != "sections/00_overview.md";
                (!top).then(|| (rel, std::fs::read_to_string(&p).unwrap_or_default()))
            })
            .collect();
        files.sort();
        let mut idx = SourceIndex::new(std::path::Path::new(&repo));
        let claims = extract(&files, &mut idx);
        let bs = batches(&claims, BATCH_CHARS);
        let chars: usize = claims.iter().map(Claim::size).sum();
        println!("claims={} batches={} total_chars={}", claims.len(), bs.len(), chars);
        if let Some(c) = claims.get(7) {
            println!("sample:\n{}\n{}", c.text, c.evidence);
        }
        if let Ok(out) = std::env::var("LEARN_CLAIMS_OUT") {
            let mut claims = claims;
            let _ = std::fs::create_dir_all(&out);
            let _ = std::fs::write(format!("{out}/system.txt"), SYSTEM_PROMPT);
            for (n, ids) in bs.iter().enumerate() {
                let p = build_user_prompt(&mut claims, ids);
                let _ = std::fs::write(format!("{out}/b{n:02}.txt"), p);
                // 证据表：id → 证据，给外部脚本核对 source_quote
                let ev: Vec<serde_json::Value> = ids.iter().map(|&i| serde_json::json!({"id": claims[i].id, "evidence": claims[i].evidence, "file": claims[i].file, "line": claims[i].line, "text": claims[i].text})).collect();
                let _ = std::fs::write(format!("{out}/b{n:02}.json"), serde_json::to_string(&ev).unwrap());
            }
        }
    }
}
