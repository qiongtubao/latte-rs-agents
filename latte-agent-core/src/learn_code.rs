//! learn_loop 注释副本：**代码由引擎逐字拷，模型只插注释**。
//!
//! 背景（实测 LAT-110，session ui-16242-1791198640076-0）：annotate 步只给了
//! `read` + `write`，模型只能把整份副本（sc.c 300+ 行、sz.h 371 行）凭记忆重打
//! 一遍——`ptr_bits - 1` 写成 `- 2`、`sz_size2index_tab[]` 「补全」成带长度的
//! 声明、续行折法改了。校验如实打回，模型重打一遍换一批地方错，3 次 × 40 分钟
//! 用完，整条 learn_loop 死在第 2/9 步。
//!
//! 现在的流程：
//! 1. annotate 开跑前 [`write_skeletons`]：按 plan.json 的 evidence，同一源文件的
//!    区间（各带几行上下文）合并成一个副本，**逐字**写进 `code/`，文件头的出处、
//!    知识点由引擎写；`code/README.md` 索引也由引擎写。
//! 2. 模型只用 `edit` 往骨架里插注释。
//! 3. 收尾 [`reconcile_all`]：按行对齐模型版本与骨架（LCS），代码行一律换回骨架
//!    原文、模型的注释挂回原位、模型新造的代码行丢掉——对不上不打回，引擎当场修。
//!    唯一打回的情况是一条注释都没插（模型什么都没做）。

use std::collections::BTreeMap;
use std::path::Path;

/// 每段证据前后多带的上下文行数。
const CONTEXT: usize = 3;
/// 两段区间（含上下文）之间空档不超过这么多行就并成一段，省得满屏「省略 2 行」。
const MERGE_GAP: usize = 6;
/// 对齐表的上限（行 × 行）。超了就不对齐，直接用骨架（注释丢掉、记一条说明）。
const LCS_MAX_CELLS: usize = 4_000_000;

/// 一个源文件对应的副本骨架。
#[derive(Debug, Clone)]
pub struct Skeleton {
    /// `code/` 下的文件名。
    pub name: String,
    /// 源文件相对路径。
    pub source: String,
    /// 引用它的知识点 `(id, 标题)`。
    pub points: Vec<(String, String)>,
    /// 合并后的行区间（1-based，闭区间）。
    pub ranges: Vec<(usize, usize)>,
    /// 注释前缀（`//` / `#` / `--`）。
    pub comment: &'static str,
    /// 骨架全文。
    pub text: String,
}

/// 按扩展名选整行注释符号。
fn comment_prefix(path: &str) -> &'static str {
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "py" | "sh" | "bash" | "zsh" | "rb" | "pl" | "toml" | "yaml" | "yml" | "cmake" | "mk" | "conf" | "ini" | "r" => "#",
        "lua" | "sql" | "hs" => "--",
        _ if matches!(name, "Makefile" | "Dockerfile" | "CMakeLists.txt") => "#",
        _ => "//",
    }
}

/// `file:a-b` / `file:a` → (file, a, b)。
fn parse_evidence(part: &str) -> Option<(String, usize, usize)> {
    let (file, lines) = part.trim().rsplit_once(':')?;
    let (a, b) = lines.split_once('-').unwrap_or((lines, lines));
    let a: usize = a.trim().parse().ok()?;
    let b: usize = b.trim().parse().ok()?;
    (a >= 1 && b >= a).then(|| (file.trim().to_string(), a, b))
}

/// 由 plan.json 算出全部骨架（不写盘；同一 plan + 源码总是得到同一结果，
/// 收尾对齐时重算一次即可，不必另存）。
pub fn skeletons(cwd: &Path, plan: &serde_json::Value) -> Vec<Skeleton> {
    // 源文件 → (区间, 知识点)
    let mut by_file: BTreeMap<String, (Vec<(usize, usize)>, Vec<(String, String)>)> = BTreeMap::new();
    for point in plan["knowledge_points"].as_array().into_iter().flatten() {
        if crate::workflow::is_engine_supplement_point(point) {
            continue;
        }
        let id = point["id"].as_str().unwrap_or("").trim().to_string();
        let title = point["title"].as_str().unwrap_or("").trim().to_string();
        for part in crate::workflow::split_learn_evidence(point["evidence"].as_str().unwrap_or("")) {
            let Some((file, a, b)) = parse_evidence(&part) else { continue };
            if crate::workflow::safe_relative_source(cwd, &file).is_none() {
                continue;
            }
            let e = by_file.entry(file).or_default();
            e.0.push((a, b));
            if !id.is_empty() && !e.1.iter().any(|(i, _)| *i == id) {
                e.1.push((id.clone(), title.clone()));
            }
        }
    }
    let mut used_names: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for (file, (mut ranges, points)) in by_file {
        let Some(full) = crate::workflow::safe_relative_source(cwd, &file) else { continue };
        let Ok(src) = std::fs::read_to_string(&full) else { continue };
        let lines: Vec<&str> = src.lines().collect();
        if lines.is_empty() {
            continue;
        }
        // 加上下文、钳到文件范围、合并。
        ranges.sort();
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for (a, b) in ranges {
            let a = a.saturating_sub(CONTEXT).max(1);
            let b = (b + CONTEXT).min(lines.len());
            if a > lines.len() {
                continue;
            }
            match merged.last_mut() {
                Some(last) if a <= last.1 + MERGE_GAP + 1 => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        if merged.is_empty() {
            continue;
        }
        let comment = comment_prefix(&file);
        let base = file.rsplit('/').next().unwrap_or(&file).to_string();
        let ids: Vec<&str> = points.iter().map(|(i, _)| i.as_str()).take(4).collect();
        let mut name = if ids.is_empty() { base.clone() } else { format!("{}-{base}", ids.join("-")) };
        if used_names.contains(&name) {
            let parent = file.rsplit('/').nth(1).unwrap_or("src");
            name = format!("{parent}-{name}");
        }
        used_names.push(name.clone());

        let cites: Vec<String> = merged.iter().map(|(a, b)| format!("{file}:{a}-{b}")).collect();
        let bar = format!("{comment} {}", "=".repeat(60));
        let mut text = String::new();
        text.push_str(&format!("{bar}\n"));
        text.push_str(&format!("{comment} 学习副本——源文件未改动（代码由引擎逐字拷贝，行号可能随源码演进漂移）\n"));
        text.push_str(&format!("{comment} 原始出处：{}\n", cites.join("、")));
        for (id, title) in &points {
            text.push_str(&format!("{comment} 知识点 {id}：{title}\n"));
        }
        text.push_str(&format!("{bar}\n"));
        let mut prev_end = 0usize;
        for (a, b) in &merged {
            if *a > prev_end + 1 {
                let from = prev_end + 1;
                let to = a - 1;
                text.push_str(&format!("\n{comment} …（{file} 第 {from}–{to} 行省略）…\n\n"));
            }
            for l in &lines[a - 1..*b] {
                text.push_str(l);
                text.push('\n');
            }
            prev_end = *b;
        }
        if prev_end < lines.len() {
            text.push_str(&format!("\n{comment} …（{file} 第 {}–{} 行省略）…\n", prev_end + 1, lines.len()));
        }
        out.push(Skeleton { name, source: file, points, ranges: merged, comment, text });
    }
    out
}

fn readme(skels: &[Skeleton], unannotated: &[String]) -> String {
    let mut s = String::from(
        "# 注释版代码副本索引\n\n> 代码由引擎从源文件逐字拷贝，讲解注释由辅导员插入；源文件未改动。\n\n\
         | 知识点 | 副本 | 原始出处 |\n|---|---|---|\n",
    );
    for k in skels {
        let points = k
            .points
            .iter()
            .map(|(i, t)| if t.is_empty() { i.clone() } else { format!("{i} {t}") })
            .collect::<Vec<_>>()
            .join("<br/>");
        let mark = if unannotated.contains(&k.name) { "（未加注释）" } else { "" };
        let cites = k.ranges.iter().map(|(a, b)| format!("`{}:{a}-{b}`", k.source)).collect::<Vec<_>>().join("<br/>");
        s.push_str(&format!("| {points} | [{}]({}){mark} | {cites} |\n", k.name, k.name));
    }
    s
}

/// annotate 开跑前：写出全部骨架与索引。返回写出的副本数。
pub fn write_skeletons(cwd: &Path, base: &Path) -> Result<usize, String> {
    let plan = crate::workflow::read_learn_json(&base.join("plan.json"))
        .map_err(|e| format!("读 plan.json 失败：{e}"))?;
    let skels = skeletons(cwd, &plan);
    if skels.is_empty() {
        return Ok(0);
    }
    let code = base.join("code");
    std::fs::create_dir_all(&code).map_err(|e| format!("建 code/ 失败：{e}"))?;
    for k in &skels {
        std::fs::write(code.join(&k.name), &k.text).map_err(|e| format!("写 {} 失败：{e}", k.name))?;
    }
    std::fs::write(code.join("README.md"), readme(&skels, &[])).map_err(|e| format!("写 README 失败：{e}"))?;
    Ok(skels.len())
}

const COMMENT_STARTS: [&str; 7] = ["//", "#", "--", "/*", "*", ";", "<!--"];

fn is_comment(line: &str) -> bool {
    let t = line.trim_start();
    COMMENT_STARTS.iter().any(|m| t.starts_with(m))
}

fn norm(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 模型行 `m` 是不是骨架行 `s`（空白差异不算；行尾追加注释算同一行）。
/// 返回行尾注释（含前导空白），没有则空串。
fn matches_line<'a>(s: &str, m: &'a str) -> Option<&'a str> {
    let (ns, nm) = (norm(s), norm(m));
    if ns == nm {
        return Some("");
    }
    if ns.is_empty() || !nm.starts_with(&ns) {
        return None;
    }
    // 在原始模型行里找到代码结束的位置：逐字符跳过与骨架相同的非空白。
    // 切点放在最后一个匹配字符之后，保留注释前原有的空白（`s; // …`）。
    let mut it = ns.chars().filter(|c| !c.is_whitespace()).peekable();
    let mut cut = 0;
    for (i, c) in m.char_indices() {
        if it.peek().is_none() {
            break;
        }
        if c.is_whitespace() {
            continue;
        }
        match it.peek() {
            Some(&x) if x == c => {
                it.next();
                cut = i + c.len_utf8();
            }
            _ => return None,
        }
    }
    let tail = &m[cut..];
    is_comment(tail.trim_start()).then_some(tail)
}

/// 一份副本的对齐结果。
pub struct Reconciled {
    pub text: String,
    /// 被换回原文 / 补回的骨架行数。
    pub restored: usize,
    /// 丢掉的、模型新造的代码行数。
    pub dropped: usize,
    /// 模型插入的注释行数（整行注释 + 行尾注释）。
    pub comments: usize,
}

/// 按行对齐：骨架行全部按原文保留（带上模型的行尾注释），模型插的整行注释挂回
/// 原位，模型新造/改坏的代码行丢掉。
pub fn reconcile(skeleton: &str, model: &str) -> Reconciled {
    let s: Vec<&str> = skeleton.lines().collect();
    let m: Vec<&str> = model.lines().collect();
    if s.len().saturating_mul(m.len()) > LCS_MAX_CELLS {
        return Reconciled { text: skeleton.to_string(), restored: s.len(), dropped: 0, comments: 0 };
    }
    // LCS：dp[i][j] = s[i..] 与 m[j..] 的最长匹配。
    let (n, k) = (s.len(), m.len());
    let mut dp = vec![0u32; (n + 1) * (k + 1)];
    let at = |i: usize, j: usize| i * (k + 1) + j;
    for i in (0..n).rev() {
        for j in (0..k).rev() {
            dp[at(i, j)] = if matches_line(s[i], m[j]).is_some() {
                dp[at(i + 1, j + 1)] + 1
            } else {
                dp[at(i + 1, j)].max(dp[at(i, j + 1)])
            };
        }
    }
    let mut out: Vec<String> = Vec::with_capacity(n + k);
    let (mut restored, mut dropped, mut comments) = (0, 0, 0);
    let (mut i, mut j) = (0, 0);
    let mut in_block = false; // 模型插入的多行 /* … */ 注释
    let mut keep_model = |line: &str, out: &mut Vec<String>, in_block: &mut bool, dropped: &mut usize, comments: &mut usize| {
        let t = line.trim_start();
        let opens = t.starts_with("/*") && !t.contains("*/");
        if *in_block || is_comment(t) || t.is_empty() {
            out.push(line.to_string());
            if !t.is_empty() {
                *comments += 1;
            }
            if *in_block && t.contains("*/") {
                *in_block = false;
            } else if opens {
                *in_block = true;
            }
        } else {
            *dropped += 1;
        }
    };
    while i < n || j < k {
        if i < n && j < k {
            if let Some(tail) = matches_line(s[i], m[j]) {
                if dp[at(i, j)] == dp[at(i + 1, j + 1)] + 1 {
                    out.push(format!("{}{tail}", s[i]));
                    if !tail.is_empty() {
                        comments += 1;
                    }
                    i += 1;
                    j += 1;
                    continue;
                }
            }
            // 平局时：模型这一行是注释（或在它插的多行块注释里）就先收它——
            // 注释讲的是**下面**那行代码，得排在它前面；是改坏的代码就先放骨架行
            // （反正会被丢掉，位置无所谓）。
            let model_note = in_block || m[j].trim().is_empty() || is_comment(m[j]);
            let (skip_s, skip_m) = (dp[at(i + 1, j)], dp[at(i, j + 1)]);
            if skip_s > skip_m || (skip_s == skip_m && !model_note) {
                out.push(s[i].to_string());
                if !s[i].trim().is_empty() {
                    restored += 1;
                }
                i += 1;
            } else {
                keep_model(m[j], &mut out, &mut in_block, &mut dropped, &mut comments);
                j += 1;
            }
        } else if i < n {
            out.push(s[i].to_string());
            if !s[i].trim().is_empty() {
                restored += 1;
            }
            i += 1;
        } else {
            keep_model(m[j], &mut out, &mut in_block, &mut dropped, &mut comments);
            j += 1;
        }
    }
    let mut text = out.join("\n");
    text.push('\n');
    Reconciled { text, restored, dropped, comments }
}

/// annotate 收尾的结果。
pub struct AnnotateOutcome {
    /// 引擎做过的修正（给用户看的状态消息）。
    pub notes: Vec<String>,
    /// 一条注释都没有的副本。
    pub unannotated: Vec<String>,
    /// 副本总数。
    pub total: usize,
}

/// annotate 收尾：逐个副本与骨架对齐、写回，重写 README，清掉 `code/` 里不属于
/// 骨架的文件（挪进 quarantine）。
pub fn reconcile_all(cwd: &Path, base: &Path) -> Result<AnnotateOutcome, String> {
    let plan = crate::workflow::read_learn_json(&base.join("plan.json"))
        .map_err(|e| format!("读 plan.json 失败：{e}"))?;
    let skels = skeletons(cwd, &plan);
    let code = base.join("code");
    let mut notes = Vec::new();
    let mut unannotated = Vec::new();
    if skels.is_empty() {
        return Ok(AnnotateOutcome { notes, unannotated, total: 0 });
    }
    std::fs::create_dir_all(&code).map_err(|e| format!("建 code/ 失败：{e}"))?;
    for k in &skels {
        let path = code.join(&k.name);
        let model = std::fs::read_to_string(&path).unwrap_or_default();
        if model.trim().is_empty() {
            notes.push(format!("{}：副本被删或清空，已按原文重建", k.name));
        }
        let r = reconcile(&k.text, &model);
        if r.restored > 0 || r.dropped > 0 {
            notes.push(format!(
                "{}：还原 {} 行被改动/删掉的代码{}，注释保留",
                k.name,
                r.restored,
                if r.dropped > 0 { format!("、丢掉 {} 行新造的代码", r.dropped) } else { String::new() }
            ));
        }
        if r.text != model {
            std::fs::write(&path, &r.text).map_err(|e| format!("写回 {} 失败：{e}", k.name))?;
        }
        // 与骨架逐行对上的行（含骨架自带的头注释、源码里原有的注释）不计数，
        // `comments` 只数模型插进来的。
        if r.comments == 0 {
            unannotated.push(k.name.clone());
        }
    }
    // 模型自己另起的文件：不在索引里、出处也没人核对，挪走。
    let keep: Vec<&str> = skels.iter().map(|k| k.name.as_str()).collect();
    let extras: Vec<std::path::PathBuf> = std::fs::read_dir(&code)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n != "README.md" && !keep.contains(&n))
        })
        .collect();
    if !extras.is_empty() {
        let q = base.join("quarantine").join("annotate-extra");
        let _ = std::fs::create_dir_all(&q);
        for p in &extras {
            if let Some(n) = p.file_name() {
                let _ = std::fs::rename(p, q.join(n));
                notes.push(format!("{}：不是引擎生成的副本，已挪到 quarantine/annotate-extra/", n.to_string_lossy()));
            }
        }
    }
    std::fs::write(code.join("README.md"), readme(&skels, &unannotated))
        .map_err(|e| format!("写 README 失败：{e}"))?;
    Ok(AnnotateOutcome { notes, unannotated, total: skels.len() })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "fn helper() {}\n\nfn main() {\n    let s = String::from(\"hi\");\n    let t = s;\n    println!(\"{t}\");\n}\n\nfn later() {}\nfn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\nfn e() {}\nfn f() {}\nfn g() {}\nfn h() {}\nfn i() {}\nfn j() {}\nfn k() {}\nfn l() {}\nfn m() {}\nfn n() {}\nfn tail() { 1 }\n";

    fn setup(evidence: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/own.rs"), SRC).unwrap();
        let base = dir.path().join(".latte/learn/own");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(
            base.join("plan.json"),
            serde_json::json!({"knowledge_points": [
                {"id": "k1", "title": "所有权", "evidence": evidence},
                {"id": "k2", "title": "收尾", "evidence": "src/own.rs:24"},
                {"id": "k9", "title": "不存在的文件", "evidence": "src/nope.rs:1-2"}
            ]})
            .to_string(),
        )
        .unwrap();
        dir
    }

    fn base(d: &tempfile::TempDir) -> std::path::PathBuf {
        d.path().join(".latte/learn/own")
    }

    #[test]
    fn skeleton_is_verbatim_with_merged_ranges_and_header() {
        let d = setup("src/own.rs:4-5, 6");
        assert_eq!(write_skeletons(d.path(), &base(&d)).unwrap(), 1, "同一源文件合成一个副本，不存在的文件跳过");
        let code = base(&d).join("code");
        let text = std::fs::read_to_string(code.join("k1-k2-own.rs")).unwrap();
        assert!(text.contains("// 原始出处：src/own.rs:1-9、src/own.rs:21-24"), "{text}");
        assert!(text.contains("// 知识点 k1：所有权") && text.contains("// 知识点 k2：收尾"), "{text}");
        assert!(text.contains("    let t = s;\n"), "逐字");
        assert!(text.contains("// …（src/own.rs 第 10–20 行省略）…"), "{text}");
        assert!(!text.contains("fn b() {}"), "空档里的代码不拷");
        let readme = std::fs::read_to_string(code.join("README.md")).unwrap();
        assert!(readme.contains("[k1-k2-own.rs](k1-k2-own.rs)") && readme.contains("`src/own.rs:1-9`"), "{readme}");
    }

    #[test]
    fn reconcile_restores_code_and_keeps_comments() {
        let skel = "// 头\nfn main() {\n    let s = String::from(\"hi\");\n    let t = s;\n    println!(\"{t}\");\n}\n";
        // 模型：插了整行注释、行尾注释、一段多行块注释；改了一个值、删了一行、
        // 编了一行新代码、改了缩进。
        let model = "// 头\nfn main() {\n    // s 唯一拥有这块堆内存\n    let s = String::from(\"bye\");\n  let t = s; // move：所有权交给 t\n    /*\n     多行讲解\n    */\n    let u = t.clone();\n}\n";
        let r = reconcile(skel, model);
        assert_eq!(
            r.text,
            "// 头\nfn main() {\n    // s 唯一拥有这块堆内存\n    let s = String::from(\"hi\");\n    let t = s; // move：所有权交给 t\n    /*\n     多行讲解\n    */\n    println!(\"{t}\");\n}\n",
        );
        assert_eq!(r.restored, 2, "改坏的那行 + 删掉的那行");
        assert_eq!(r.dropped, 2, "改坏的原行 + 新造的那行");
        assert!(r.comments >= 5);
        let again = reconcile(skel, &r.text);
        assert_eq!(again.text, r.text, "幂等");
        assert_eq!((again.restored, again.dropped), (0, 0));
    }

    /// 实测 LAT-110 的折行：源码一行，模型折成两行。
    #[test]
    fn reconcile_fixes_rewrapped_line() {
        let skel = "\t\tsize_class(sc, lg_max_lookup, lg_page, lg_ngroup, index,\n\t\t    lg_base, lg_delta, ndelta);\n";
        let model = "\t\t// 生成一档\n\t\tsize_class(sc, lg_max_lookup, lg_page, lg_ngroup,\n\t\t    index, lg_base, lg_delta, ndelta);\n";
        let r = reconcile(skel, model);
        assert_eq!(r.text, format!("\t\t// 生成一档\n{skel}"));
    }

    #[test]
    fn reconcile_all_rebuilds_flags_unannotated_and_moves_extras() {
        let d = setup("src/own.rs:4-5");
        write_skeletons(d.path(), &base(&d)).unwrap();
        let code = base(&d).join("code");
        let name = "k1-k2-own.rs";
        // 没插任何注释
        let out = reconcile_all(d.path(), &base(&d)).unwrap();
        assert_eq!((out.total, out.unannotated.clone()), (1, vec![name.to_string()]));
        assert!(out.notes.is_empty(), "{:?}", out.notes);
        assert!(std::fs::read_to_string(code.join("README.md")).unwrap().contains("（未加注释）"));

        // 插了注释但改了代码，另起了一个文件
        let skel = std::fs::read_to_string(code.join(name)).unwrap();
        let edited = skel.replace("    let t = s;\n", "    // 交出所有权\n    let t = s.clone();\n");
        std::fs::write(code.join(name), edited).unwrap();
        std::fs::write(code.join("k1-mine.rs"), "fn mine() {}\n").unwrap();
        let out = reconcile_all(d.path(), &base(&d)).unwrap();
        assert!(out.unannotated.is_empty());
        let now = std::fs::read_to_string(code.join(name)).unwrap();
        assert!(now.contains("    // 交出所有权\n    let t = s;\n"), "{now}");
        assert!(!now.contains("clone"));
        assert!(out.notes.iter().any(|n| n.contains("还原 1 行")), "{:?}", out.notes);
        assert!(!code.join("k1-mine.rs").exists());
        assert!(base(&d).join("quarantine/annotate-extra/k1-mine.rs").is_file());

        // 副本被删：按原文重建
        std::fs::remove_file(code.join(name)).unwrap();
        let out = reconcile_all(d.path(), &base(&d)).unwrap();
        assert!(code.join(name).is_file());
        assert_eq!(out.unannotated, vec![name.to_string()]);
    }

    #[test]
    fn comment_prefix_by_extension() {
        assert_eq!(comment_prefix("src/a.c"), "//");
        assert_eq!(comment_prefix("x/y.py"), "#");
        assert_eq!(comment_prefix("Makefile"), "#");
        assert_eq!(comment_prefix("q.sql"), "--");
    }
}
