//! 小白试学：学员读之前，先让一个「小白」角色把文档和代码学一遍、提问题，
//! 由辅导员纠正、小白复查。
//!
//! # 动机（实测 LAT-110 jemalloc，会话 ui-5976）
//!
//! 学员读到的 k2 写着「比 16 KiB 大一律走 large」，同一章又写「small 最大档
//! 14 KiB」——照前一句推，申请 16384 字节会走 small，与源码相反。还有几十处
//! `符号（文件:行号）` 对不上。写手自己写、自己检查，看不出自己的盲点；
//! learn_loop 的写文档流程里也没有任何审读步骤。
//!
//! # 分工（流程与核实由引擎做，模型只做「读懂/改对」）
//!
//! - 引擎：试学前先跑出处核对（[`crate::learn_cite`]）；按章节内容哈希决定这次要学
//!   哪几章；出不带答案的试学卷；核实问题清单（摘的原文必须逐字在文档里、依据的
//!   行号必须真实存在）；按选项机械判分，答错自动成为一条问题；按轮次收尾，
//!   复查两轮仍未解决的在文档里标「⚠️ 待确认」。
//! - 小白（novice 角色）：只读文档、代码副本和源码，看不到 plan.json（答案在里面）。
//! - 辅导员（tutor）：逐条 `FIXED` / `WONTFIX`，只能改章节和代码副本的注释。
//!
//! 本模块只放纯逻辑（文件进、文件出），调度与校验门禁在 `workflow.rs`。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 工作区下的试学目录。
pub const DIR: &str = "novice";
/// 复查轮数上限（辅导员改 → 小白复查 算一轮）。
pub const MAX_ROUNDS: u32 = 2;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Issue {
    pub id: String,
    /// doc / code / split / cite / quiz
    pub kind: String,
    /// 知识点 id（`overview` = 总览）。
    pub chapter: String,
    /// 原文所在文件（相对学习工作区）。
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub quote: String,
    pub problem: String,
    #[serde(default)]
    pub evidence: String,
    /// novice / quiz / engine
    pub source: String,
    /// open / fixed / wontfix / resolved / pending
    pub status: String,
    #[serde(default)]
    pub fix_note: String,
    #[serde(default)]
    pub recheck_note: String,
    /// quiz：小白选的、正确答案。
    #[serde(default)]
    pub choice: String,
    #[serde(default)]
    pub answer: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    /// 知识点 id → 上次试学结束时的章节内容哈希。
    #[serde(default)]
    pub studied: BTreeMap<String, String>,
    /// 本轮试学已复查了几轮。
    #[serde(default)]
    pub round: u32,
    /// 本轮试学的章节。
    #[serde(default)]
    pub scope: Vec<String>,
}

fn dir(base: &Path) -> PathBuf {
    base.join(DIR)
}

pub fn load_state(base: &Path) -> State {
    std::fs::read_to_string(dir(base).join("state.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_state(base: &Path, st: &State) -> Result<(), String> {
    std::fs::create_dir_all(dir(base)).map_err(|e| e.to_string())?;
    std::fs::write(
        dir(base).join("state.json"),
        serde_json::to_string_pretty(st).unwrap_or_default(),
    )
    .map_err(|e| e.to_string())
}

pub fn load_issues(base: &Path) -> Vec<Issue> {
    std::fs::read_to_string(dir(base).join("issues.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_issues(base: &Path, issues: &[Issue]) -> Result<(), String> {
    std::fs::create_dir_all(dir(base)).map_err(|e| e.to_string())?;
    std::fs::write(
        dir(base).join("issues.json"),
        serde_json::to_string_pretty(issues).unwrap_or_default(),
    )
    .map_err(|e| e.to_string())
}

fn plan(base: &Path) -> serde_json::Value {
    std::fs::read_to_string(base.join("plan.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(serde_json::Value::Null)
}

fn points(plan: &serde_json::Value) -> Vec<serde_json::Value> {
    plan.get("knowledge_points")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

fn str_of<'a>(v: &'a serde_json::Value, k: &str) -> &'a str {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("")
}

/// 一章的全部正文文件（分节目录优先，没有就用整章文件）。
pub fn chapter_files(base: &Path, id: &str) -> Vec<PathBuf> {
    let parts = base.join("sections").join(id);
    let mut files: Vec<PathBuf> = std::fs::read_dir(&parts)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .collect();
    files.sort();
    if files.is_empty() {
        let whole = base.join("sections").join(format!("{id}.md"));
        if whole.is_file() {
            files.push(whole);
        }
    }
    files
}

fn fnv(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

pub fn chapter_hash(base: &Path, id: &str) -> String {
    let mut all = String::new();
    for f in chapter_files(base, id) {
        all.push_str(&std::fs::read_to_string(f).unwrap_or_default());
        all.push('\u{0}');
    }
    fnv(&all)
}

/// 这次要试学的章：内容自上次试学后变过的（第一次 = 全部）。
pub fn study_scope(base: &Path) -> Vec<String> {
    let st = load_state(base);
    points(&plan(base))
        .iter()
        .map(|p| str_of(p, "id").to_string())
        .filter(|id| !id.is_empty() && !chapter_files(base, id).is_empty())
        .filter(|id| st.studied.get(id) != Some(&chapter_hash(base, id)))
        .collect()
}

/// 试学卷：题面 + 选项（只给 label，不给讲解；顺序固定洗一次，不给答案）。
fn quiz_lines(point: &serde_json::Value, salt: u64) -> String {
    let mut labels: Vec<String> = point
        .get("options")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .map(|o| str_of(o, "label").to_string())
        .filter(|l| !l.is_empty())
        .collect();
    // 正确项常被写在第一个：固定洗一次，免得小白靠位置猜。
    let seed = fnv(&format!("{}{salt}", str_of(point, "id")));
    let mut x = u64::from_str_radix(&seed, 16).unwrap_or(1) | 1;
    for i in (1..labels.len()).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        labels.swap(i, (x % (i as u64 + 1)) as usize);
    }
    let mut out = format!("- 题 `{}`：{}\n", str_of(point, "id"), str_of(point, "question"));
    for l in labels {
        out.push_str(&format!("  - {l}\n"));
    }
    out
}

fn rel(base: &Path, p: &Path) -> String {
    p.strip_prefix(base).unwrap_or(p).to_string_lossy().replace('\\', "/")
}

/// 写试学说明 `novice/brief.md`，重置本轮问题与轮次。
pub fn write_study_brief(base: &Path, base_rel: &str, scope: &[String], engine_issues: Vec<Issue>) -> Result<(), String> {
    let plan = plan(base);
    let mut out = String::from("# 试学说明（引擎生成）\n\n## 要学的章节（按顺序）\n");
    let overview = base.join("sections/00_overview.md");
    if overview.is_file() {
        out.push_str(&format!("- 总览 `overview`：`{base_rel}/sections/00_overview.md`\n"));
    }
    for p in points(&plan) {
        let id = str_of(&p, "id");
        if !scope.iter().any(|s| s == id) {
            continue;
        }
        out.push_str(&format!("- `{id}` {}：", str_of(&p, "title")));
        let files: Vec<String> = chapter_files(base, id)
            .iter()
            .map(|f| format!("`{base_rel}/{}`", rel(base, f)))
            .collect();
        out.push_str(&files.join("、"));
        out.push('\n');
    }
    let code: Vec<String> = std::fs::read_dir(base.join("code"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .map(|p| format!("- `{base_rel}/{}`", rel(base, &p)))
        .collect();
    if !code.is_empty() {
        out.push_str("\n## 注释版代码副本\n");
        out.push_str(&code.join("\n"));
        out.push('\n');
    }
    out.push_str("\n## 试学卷（学完按文档作答；答案不在这里）\n");
    for p in points(&plan) {
        if scope.iter().any(|s| s == str_of(&p, "id")) {
            out.push_str(&quiz_lines(&p, 0));
        }
    }
    if !engine_issues.is_empty() {
        out.push_str("\n## 引擎已发现、不用你再报的问题\n");
        for i in &engine_issues {
            out.push_str(&format!("- {}\n", i.problem));
        }
    }
    std::fs::create_dir_all(dir(base)).map_err(|e| e.to_string())?;
    std::fs::write(dir(base).join("brief.md"), out).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(dir(base).join("report.json"));
    let _ = std::fs::remove_file(dir(base).join("recheck.json"));
    save_issues(base, &engine_issues)?;
    let mut st = load_state(base);
    st.round = 0;
    st.scope = scope.to_vec();
    save_state(base, &st)
}

/// 去掉空白与 markdown 记号后比较（模型摘录时常丢 `**`、反引号）。
fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace() && !"`*_>#|".contains(*c))
        .collect()
}

/// 摘录原文在哪个文件里（相对工作区）。找不到返回 None。
pub fn locate_quote(base: &Path, quote: &str) -> Option<String> {
    let q = norm(quote);
    if q.chars().count() < 6 {
        return None;
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    for e in walk(&base.join("sections")) {
        candidates.push(e);
    }
    for e in walk(&base.join("code")) {
        candidates.push(e);
    }
    // 分节文件优先（改的是它们；整章文件是装配产物）。
    candidates.sort_by_key(|p| (p.parent() == Some(&base.join("sections")), p.clone()));
    candidates
        .into_iter()
        .find(|p| std::fs::read_to_string(p).is_ok_and(|t| norm(&t).contains(&q)))
        .map(|p| rel(base, &p))
}

pub fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        if p.is_dir() {
            for e in std::fs::read_dir(&p).into_iter().flatten().flatten() {
                stack.push(e.path());
            }
        } else if p.is_file() {
            out.push(p);
        }
    }
    out.sort();
    out
}

/// 文件路径 → 所属章节 id。
pub fn chapter_of(file: &str) -> String {
    let parts: Vec<&str> = file.split('/').collect();
    match parts.as_slice() {
        ["sections", "00_overview.md"] => "overview".into(),
        ["sections", ch, _] => (*ch).to_string(),
        ["sections", f] => f.trim_end_matches(".md").to_string(),
        _ => String::new(),
    }
}

fn label_eq(a: &str, b: &str) -> bool {
    norm(a) == norm(b)
}

#[derive(Debug, Default)]
pub struct Ingested {
    pub issues: Vec<Issue>,
    /// 被丢弃的条目与原因。
    pub dropped: Vec<String>,
    pub wrong_answers: usize,
}

/// 读 `novice/report.json`，核实后写 `issues.json`。`Err` = 报告本身不合格（要重写）。
pub fn ingest_report(base: &Path, evidence_ok: &dyn Fn(&str) -> bool) -> Result<Ingested, String> {
    let raw = std::fs::read_to_string(dir(base).join("report.json"))
        .map_err(|_| format!("没有写 {DIR}/report.json"))?;
    let report: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("{DIR}/report.json 不是合法 JSON：{e}"))?;
    let st = load_state(base);
    let plan = plan(base);
    let mut issues = load_issues(base); // 引擎预置的（出处核对修不了的）
    let mut out = Ingested::default();
    let answers = report.get("answers").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let mut missing = Vec::new();
    for p in points(&plan) {
        let id = str_of(&p, "id");
        if !st.scope.iter().any(|s| s == id) {
            continue;
        }
        let Some(a) = answers.iter().find(|a| str_of(a, "id") == id) else {
            missing.push(id.to_string());
            continue;
        };
        let choice = str_of(a, "choice");
        let answer = str_of(&p, "answer");
        if label_eq(choice, answer) {
            continue;
        }
        out.wrong_answers += 1;
        let basis = str_of(a, "basis");
        let file = locate_quote(base, basis).unwrap_or_default();
        let chapter = if file.is_empty() { id.to_string() } else { chapter_of(&file) };
        issues.push(Issue {
            kind: "quiz".into(),
            chapter: if chapter.is_empty() { id.to_string() } else { chapter },
            file: file.clone(),
            quote: if file.is_empty() { String::new() } else { basis.to_string() },
            problem: format!(
                "照文档作答「{}」选了「{choice}」，与源码不符（正确：「{answer}」）。{}",
                str_of(&p, "question"),
                if file.is_empty() { "依据的那句没在文档里找到。".to_string() } else { format!("依据的原文：「{basis}」") }
            ),
            source: "quiz".into(),
            status: "open".into(),
            choice: choice.to_string(),
            answer: answer.to_string(),
            evidence: str_of(&p, "evidence").to_string(),
            ..Default::default()
        });
    }
    if !missing.is_empty() {
        return Err(format!(
            "{DIR}/report.json 的 answers 缺了这些题：{}（每题都要答，choice 抄选项原文）",
            missing.join("、")
        ));
    }
    for it in report.get("issues").and_then(|v| v.as_array()).into_iter().flatten() {
        let kind = match str_of(it, "kind") {
            "code" => "code",
            "split" => "split",
            _ => "doc",
        };
        let problem = str_of(it, "problem").trim().to_string();
        if problem.is_empty() {
            out.dropped.push("一条问题没写 problem".into());
            continue;
        }
        let quote = str_of(it, "quote").trim().to_string();
        let evidence = str_of(it, "evidence").trim().to_string();
        if !evidence.is_empty() && !evidence_ok(&evidence) {
            out.dropped.push(format!("「{}」依据 `{evidence}` 的行号不存在", clip(&problem, 40)));
            continue;
        }
        let (file, chapter) = if kind == "split" && quote.is_empty() {
            (String::new(), str_of(it, "chapter").to_string())
        } else {
            match locate_quote(base, &quote) {
                Some(f) => {
                    let ch = chapter_of(&f);
                    let ch = if ch.is_empty() { str_of(it, "chapter").to_string() } else { ch };
                    (f, ch)
                }
                None => {
                    out.dropped.push(format!("「{}」摘的原文在文档/代码副本里找不到", clip(&problem, 40)));
                    continue;
                }
            }
        };
        issues.push(Issue {
            kind: kind.into(),
            chapter,
            file,
            quote,
            problem,
            evidence,
            source: "novice".into(),
            status: "open".into(),
            ..Default::default()
        });
    }
    for (n, i) in issues.iter_mut().enumerate() {
        i.id = format!("n{}", n + 1);
    }
    save_issues(base, &issues)?;
    out.issues = issues;
    Ok(out)
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        s.chars().take(n).collect::<String>() + "…"
    } else {
        s.to_string()
    }
}

/// 交给辅导员改的问题（拆分类不在这里改——改知识点划分要重排整份 plan，交给学员定）。
pub fn fixable(issues: &[Issue]) -> Vec<&Issue> {
    issues.iter().filter(|i| i.status == "open" && i.kind != "split").collect()
}

/// 写 `novice/fix_brief.md`。没有要改的返回 false。
pub fn write_fix_brief(base: &Path, base_rel: &str) -> Result<bool, String> {
    let issues = load_issues(base);
    let todo = fixable(&issues);
    if todo.is_empty() {
        return Ok(false);
    }
    let mut out = String::from("# 试学问题（引擎核实过，逐条处理）\n\n");
    for i in todo {
        out.push_str(&format!("## {}（{}，章节 {}）\n", i.id, i.kind, i.chapter));
        if !i.file.is_empty() {
            out.push_str(&format!("- 文件：`{base_rel}/{}`\n", i.file));
        }
        if !i.quote.is_empty() {
            out.push_str(&format!("- 原文：「{}」\n", i.quote));
        }
        out.push_str(&format!("- 问题：{}\n", i.problem));
        if !i.evidence.is_empty() {
            out.push_str(&format!("- 依据：`{}`\n", i.evidence));
        }
        if !i.recheck_note.is_empty() {
            out.push_str(&format!("- 上轮你的处理：{}\n- 小白复查后仍不满意：{}\n", i.fix_note, i.recheck_note));
        }
        out.push('\n');
    }
    std::fs::write(dir(base).join("fix_brief.md"), out).map_err(|e| e.to_string())?;
    Ok(true)
}

/// 解析辅导员逐条回应：`FIXED n1: …` / `WONTFIX n2: …`。返回没回应的 id。
pub fn apply_fix_response(base: &Path, response: &str, broken_files: &[String]) -> Result<Vec<String>, String> {
    let mut issues = load_issues(base);
    let mut replies: BTreeMap<String, (bool, String)> = BTreeMap::new();
    for line in response.lines() {
        let l = line.trim().trim_start_matches(['-', '*', ' ']);
        for (tag, fixed) in [("FIXED", true), ("WONTFIX", false)] {
            if let Some(rest) = l.strip_prefix(tag) {
                let rest = rest.trim_start();
                let id: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
                let note = rest[id.len()..].trim_start_matches([':', '：', ' ']).trim().to_string();
                if !id.is_empty() {
                    replies.insert(id, (fixed, note));
                }
            }
        }
    }
    let mut silent = Vec::new();
    for i in issues.iter_mut().filter(|i| i.status == "open" && i.kind != "split") {
        if broken_files.contains(&i.file) {
            i.fix_note = "改动没通过引擎检查（删了原有出处或整段推倒），已还原".into();
            continue;
        }
        match replies.get(&i.id) {
            Some((true, note)) => {
                i.status = "fixed".into();
                i.fix_note = note.clone();
            }
            Some((false, note)) => {
                i.status = "wontfix".into();
                i.fix_note = note.clone();
            }
            None => silent.push(i.id.clone()),
        }
    }
    save_issues(base, &issues)?;
    Ok(silent)
}

/// 写 `novice/recheck_brief.md`。没有可复查的返回 false。
pub fn write_recheck_brief(base: &Path, base_rel: &str) -> Result<bool, String> {
    let issues = load_issues(base);
    let st = load_state(base);
    let plan = plan(base);
    let todo: Vec<&Issue> = issues.iter().filter(|i| i.status == "fixed" || i.status == "wontfix").collect();
    if todo.is_empty() {
        return Ok(false);
    }
    let mut out = String::from("# 复查（只看下面这些条目和改过的地方）\n\n");
    for i in &todo {
        out.push_str(&format!("## {}（{}）\n- 你上次的问题：{}\n", i.id, i.kind, i.problem));
        if !i.file.is_empty() {
            out.push_str(&format!("- 改在：`{base_rel}/{}`\n", i.file));
        }
        out.push_str(&format!(
            "- 辅导员：{}{}\n",
            if i.status == "fixed" { "已改。" } else { "不改，理由：" },
            i.fix_note
        ));
        if i.kind == "quiz" {
            if let Some(p) = points(&plan).iter().find(|p| i.problem.contains(str_of(p, "question")) && !str_of(p, "question").is_empty()) {
                out.push_str("- 重新作答：\n");
                out.push_str(&quiz_lines(p, u64::from(st.round) + 1).replace("- 题", "  - 题"));
            }
        }
        out.push('\n');
    }
    std::fs::write(dir(base).join("recheck_brief.md"), out).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(dir(base).join("recheck.json"));
    Ok(true)
}

/// 读 `novice/recheck.json`，更新状态。返回 (仍未解决数, 本轮是否收尾)。
pub fn ingest_recheck(base: &Path) -> Result<(usize, bool), String> {
    let raw = std::fs::read_to_string(dir(base).join("recheck.json"))
        .map_err(|_| format!("没有写 {DIR}/recheck.json"))?;
    let v: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("{DIR}/recheck.json 不是合法 JSON：{e}"))?;
    let plan = plan(base);
    let mut issues = load_issues(base);
    let verdicts = v.get("issues").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    let answers = v.get("answers").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    let mut missing = Vec::new();
    for i in issues.iter_mut().filter(|i| i.status == "fixed" || i.status == "wontfix") {
        let verdict = verdicts.iter().find(|x| str_of(x, "id") == i.id);
        let resolved = if i.kind == "quiz" {
            // 答对才算解决：不采信「我觉得改好了」。
            let qid = points(&plan)
                .iter()
                .find(|p| i.problem.contains(str_of(p, "question")) && !str_of(p, "question").is_empty())
                .map(|p| str_of(p, "id").to_string())
                .unwrap_or_default();
            match answers.iter().find(|a| str_of(a, "id") == qid) {
                Some(a) => label_eq(str_of(a, "choice"), &i.answer),
                None => {
                    missing.push(i.id.clone());
                    continue;
                }
            }
        } else {
            match verdict.and_then(|x| x.get("resolved")).and_then(|x| x.as_bool()) {
                Some(b) => b,
                None => {
                    missing.push(i.id.clone());
                    continue;
                }
            }
        };
        let note = verdict.map(|x| str_of(x, "note").to_string()).unwrap_or_default();
        if resolved {
            i.status = "resolved".into();
            i.recheck_note = note;
        } else {
            i.status = "open".into();
            i.recheck_note = if note.is_empty() { "复查仍没看懂".into() } else { note };
        }
    }
    if !missing.is_empty() {
        return Err(format!(
            "{DIR}/recheck.json 缺了这些条目的结论：{}（每条写 resolved；答题类写 answers）",
            missing.join("、")
        ));
    }
    save_issues(base, &issues)?;
    let mut st = load_state(base);
    st.round += 1;
    save_state(base, &st)?;
    let open = issues.iter().filter(|i| i.status == "open" && i.kind != "split").count();
    Ok((open, open == 0 || st.round >= MAX_ROUNDS))
}

/// 收尾：未解决的在文档原文处标「⚠️ 待确认」，已解决的看不懂类问题进 FAQ，
/// 写 `pending.md`，记下本轮试学过的章节哈希。返回待确认清单。
pub fn finalize(base: &Path) -> Result<Vec<Issue>, String> {
    let mut issues = load_issues(base);
    let mut pending = Vec::new();
    for i in issues.iter_mut() {
        if i.status == "open" || (i.kind == "split" && i.status != "resolved") {
            i.status = "pending".into();
            mark_pending(base, i);
            pending.push(i.clone());
        } else if i.status == "resolved" && i.kind == "doc" && !i.fix_note.is_empty() && !i.chapter.is_empty() && i.chapter != "overview" {
            let faq = base.join("sections").join(&i.chapter).join("8_faq.md");
            if faq.is_file() {
                let mut t = std::fs::read_to_string(&faq).unwrap_or_default();
                if !t.contains(&i.problem) {
                    t.push_str(&format!("\n\n#### 试学提问：{}\n\n{}\n", i.problem, i.fix_note));
                    let _ = std::fs::write(&faq, t);
                }
            }
        }
    }
    save_issues(base, &issues)?;
    let mut md = String::from("# 试学后仍待确认的问题\n\n");
    for i in &pending {
        md.push_str(&format!("- {}（{}，章节 {}）{}\n", i.id, i.kind, i.chapter, i.problem));
    }
    std::fs::write(dir(base).join("pending.md"), md).map_err(|e| e.to_string())?;
    let mut st = load_state(base);
    for id in st.scope.clone() {
        st.studied.insert(id.clone(), chapter_hash(base, &id));
    }
    save_state(base, &st)?;
    Ok(pending)
}

/// 在原文所在段落后插一条「⚠️ 待确认」；找不到原文就追加到该章总结。
fn mark_pending(base: &Path, i: &Issue) {
    let note = format!("> ⚠️ 待确认（试学提问 {}）：{}", i.id, i.problem.replace('\n', " "));
    let target = if i.file.is_empty() {
        base.join("sections").join(&i.chapter).join("9_summary.md")
    } else {
        base.join(&i.file)
    };
    let Ok(text) = std::fs::read_to_string(&target) else {
        return;
    };
    if text.contains(&note) {
        return;
    }
    let q = norm(&i.quote);
    let lines: Vec<&str> = text.split('\n').collect();
    let hit = (!q.is_empty())
        .then(|| {
            let first: String = q.chars().take(12).collect();
            lines.iter().position(|l| norm(l).contains(&first))
        })
        .flatten();
    let new = match hit {
        Some(at) => {
            let end = (at..lines.len()).find(|&j| lines[j].trim().is_empty()).unwrap_or(lines.len());
            let mut v: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
            v.insert(end, String::new());
            v.insert(end + 1, note);
            v.join("\n")
        }
        None => format!("{}\n\n{note}\n", text.trim_end()),
    };
    let _ = std::fs::write(target, new);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let b = d.path();
        std::fs::create_dir_all(b.join("sections/k2")).unwrap();
        std::fs::create_dir_all(b.join("code")).unwrap();
        std::fs::write(
            b.join("plan.json"),
            serde_json::json!({"knowledge_points": [{
                "id": "k2", "title": "边界", "question": "申请恰好 16384 字节会怎样？",
                "options": [{"label": "作为 Large 直接分整页"}, {"label": "作为 Small 走 slab 分配"}],
                "answer": "作为 Large 直接分整页", "evidence": "sc.h:267"
            }]})
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            b.join("sections/k2/1_background.md"),
            "请求落在 `[16 B, 16 KiB]`：比 16 KiB 大一律走 large。\n\n下一段讲具体的分档数字。\n",
        )
        .unwrap();
        std::fs::write(b.join("sections/k2/8_faq.md"), "#### 旧问题\n答。\n").unwrap();
        std::fs::write(b.join("code/k2-sc.h"), "#define X 1 // 注释\n").unwrap();
        d
    }

    #[test]
    fn scope_is_content_hash_based() {
        let d = ws();
        assert_eq!(study_scope(d.path()), vec!["k2"]);
        write_study_brief(d.path(), "L", &["k2".into()], vec![]).unwrap();
        let brief = std::fs::read_to_string(d.path().join("novice/brief.md")).unwrap();
        assert!(brief.contains("16384") && brief.contains("作为 Small 走 slab 分配"));
        assert!(!brief.contains("answer"), "卷子里没有答案");
        finalize(d.path()).unwrap();
        assert!(study_scope(d.path()).is_empty(), "学过且没改 → 不再学");
        std::fs::write(d.path().join("sections/k2/1_background.md"), "改了").unwrap();
        assert_eq!(study_scope(d.path()), vec!["k2"]);
    }

    #[test]
    fn report_is_verified_and_wrong_answer_becomes_issue() {
        let d = ws();
        write_study_brief(d.path(), "L", &["k2".into()], vec![]).unwrap();
        // 没答题 → 报告不合格
        std::fs::write(d.path().join("novice/report.json"), r#"{"answers":[],"issues":[]}"#).unwrap();
        assert!(ingest_report(d.path(), &|_| true).unwrap_err().contains("k2"));
        std::fs::write(
            d.path().join("novice/report.json"),
            serde_json::json!({
                "answers": [{"id": "k2", "choice": "作为 Small 走 slab 分配", "basis": "比 16 KiB 大一律走 large"}],
                "issues": [
                    {"kind": "doc", "quote": "比 16 KiB 大一律走 large", "problem": "和 small 最大 14 KiB 矛盾", "evidence": "sc.h:267"},
                    {"kind": "doc", "quote": "文档里根本没有这句话", "problem": "编的"},
                    {"kind": "doc", "quote": "下一段。下一段", "problem": "行号假", "evidence": "sc.h:99999"},
                    {"kind": "code", "quote": "#define X 1", "problem": "注释不对"},
                    {"kind": "split", "chapter": "k2", "problem": "k2 和 k1 重叠"}
                ]
            })
            .to_string(),
        )
        .unwrap();
        let ok = |e: &str| !e.contains("99999");
        let got = ingest_report(d.path(), &ok).unwrap();
        assert_eq!(got.wrong_answers, 1);
        assert_eq!(got.dropped.len(), 2, "{:?}", got.dropped);
        let kinds: Vec<&str> = got.issues.iter().map(|i| i.kind.as_str()).collect();
        assert_eq!(kinds, vec!["quiz", "doc", "code", "split"]);
        assert_eq!(got.issues[0].file, "sections/k2/1_background.md");
        assert_eq!(got.issues[0].chapter, "k2");
        assert_eq!(got.issues[2].file, "code/k2-sc.h");
        assert_eq!(fixable(&got.issues).len(), 3, "拆分类不交给辅导员改");
    }

    #[test]
    fn fix_recheck_and_finalize() {
        let d = ws();
        write_study_brief(d.path(), "L", &["k2".into()], vec![]).unwrap();
        std::fs::write(
            d.path().join("novice/report.json"),
            serde_json::json!({
                "answers": [{"id": "k2", "choice": "作为 Small 走 slab 分配", "basis": "比 16 KiB 大一律走 large"}],
                "issues": [{"kind": "doc", "quote": "比 16 KiB 大一律走 large", "problem": "边界没讲清"},
                           {"kind": "doc", "quote": "下一段讲具体的分档数字。", "problem": "这段看不懂"}]
            })
            .to_string(),
        )
        .unwrap();
        ingest_report(d.path(), &|_| true).unwrap();
        assert!(write_fix_brief(d.path(), "L").unwrap());
        let silent = apply_fix_response(d.path(), "FIXED n1: 改成 ≥ 16 KiB\nFIXED n2: 边界改了\n- FIXED n3: 补了解释\n", &[]).unwrap();
        assert!(silent.is_empty());
        assert!(write_recheck_brief(d.path(), "L").unwrap());
        let rb = std::fs::read_to_string(d.path().join("novice/recheck_brief.md")).unwrap();
        assert!(rb.contains("重新作答") && rb.contains("16384"), "{rb}");
        // 小白说 n1（答题）解决了，但重答仍错 → 不采信；n2、n3 解决。
        std::fs::write(
            d.path().join("novice/recheck.json"),
            serde_json::json!({"issues": [{"id": "n1", "resolved": true}, {"id": "n2", "resolved": true}, {"id": "n3", "resolved": true}],
                               "answers": [{"id": "k2", "choice": "作为 Small 走 slab 分配"}]})
            .to_string(),
        )
        .unwrap();
        assert_eq!(ingest_recheck(d.path()).unwrap(), (1, false));
        // 第二轮：仍错 → 轮次用完收尾。
        apply_fix_response(d.path(), "FIXED n1: 再改", &[]).unwrap();
        write_recheck_brief(d.path(), "L").unwrap();
        std::fs::write(
            d.path().join("novice/recheck.json"),
            r#"{"issues":[{"id":"n1","resolved":false,"note":"还是矛盾"}],"answers":[{"id":"k2","choice":"作为 Small 走 slab 分配"}]}"#,
        )
        .unwrap();
        assert_eq!(ingest_recheck(d.path()).unwrap(), (1, true));
        let pending = finalize(d.path()).unwrap();
        assert_eq!(pending.len(), 1);
        let bg = std::fs::read_to_string(d.path().join("sections/k2/1_background.md")).unwrap();
        assert!(bg.contains("⚠️ 待确认（试学提问 n1）"), "{bg}");
        let pos_note = bg.find("⚠️").unwrap();
        assert!(pos_note < bg.find("下一段").unwrap(), "标在原文段落后面: {bg}");
        let faq = std::fs::read_to_string(d.path().join("sections/k2/8_faq.md")).unwrap();
        assert!(faq.contains("试学提问：这段看不懂") && faq.contains("补了解释"), "{faq}");
    }

    #[test]
    fn broken_file_keeps_issue_open() {
        let d = ws();
        write_study_brief(d.path(), "L", &["k2".into()], vec![]).unwrap();
        std::fs::write(
            d.path().join("novice/report.json"),
            serde_json::json!({"answers": [{"id": "k2", "choice": "作为 Large 直接分整页"}],
                               "issues": [{"kind": "doc", "quote": "下一段讲具体的分档数字。", "problem": "不懂"}]})
            .to_string(),
        )
        .unwrap();
        let got = ingest_report(d.path(), &|_| true).unwrap();
        assert_eq!(got.wrong_answers, 0);
        apply_fix_response(d.path(), "FIXED n1: 改了", &["sections/k2/1_background.md".into()]).unwrap();
        assert_eq!(load_issues(d.path())[0].status, "open");
        assert!(!write_recheck_brief(d.path(), "L").unwrap());
    }
}
