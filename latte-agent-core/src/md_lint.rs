//! 写文档时的 markdown 结构门禁（mermaid 之外的、会让页面直接坏掉的问题）。
//!
//! 每一条都来自 jemalloc 学习文档的实测扫描（近两天写出的 245 份 md）：
//! - **代码围栏没闭合**：后面整篇都被吞进代码块；
//! - **表格行没收尾**：单元格里写了换行（列表、长句），这一行没以 `|` 结尾，
//!   后半截从表格里掉出来（lat-113 的位字段表）；表格行列数和表头不一致同理；
//! - **模型思考泄漏**：`<think>…</think>` 原样写进正文（输出被截断后续写时最常见）；
//! - **本地图片不存在**：`![](img/x.png)` 指向没有的文件，页面上是一个破图。
//!
//! 只做「确定会渲染坏」的判断，不评价写得好不好。

/// 一段 markdown 的结构问题。`dir` 是文档所在目录（解析相对图片路径用）；
/// None 时跳过图片存在性检查。
pub fn lint(text: &str, dir: Option<&std::path::Path>) -> Vec<String> {
    let mut problems = Vec::new();
    let mut fence: Option<(usize, char, usize)> = None; // (行号, 字符, 长度)
                                                        // 表格状态：(表头行号, 列数)
    let mut table: Option<(usize, usize)> = None;
    let mut prev_table_row_open: Option<usize> = None;
    let mut think_reported = false;
    // 正在报告的一段「没有表头的表格行」（只报第一行）。
    let mut orphan_run = false;
    let lines: Vec<&str> = text.lines().collect();
    for (i, raw) in lines.iter().enumerate() {
        let ln = i + 1;
        let t = raw.trim();
        let ch = t.chars().next().unwrap_or(' ');
        if ch == '`' || ch == '~' {
            let run = t.chars().take_while(|&c| c == ch).count();
            if run >= 3 {
                match fence {
                    None => {
                        fence = Some((ln, ch, run));
                        table = None;
                        prev_table_row_open = None;
                        continue;
                    }
                    Some((_, fc, fl)) if fc == ch && run >= fl && t[run..].trim().is_empty() => {
                        fence = None;
                        continue;
                    }
                    _ => {}
                }
            }
        }
        if fence.is_some() {
            continue;
        }
        if !think_reported && (t.contains("<think>") || t.contains("</think>")) {
            problems.push(format!(
                "第 {ln} 行有模型思考内容 `<think>` 漏进了正文，删掉这段"
            ));
            think_reported = true;
        }
        if let Some(open) = prev_table_row_open.take() {
            // 上一行表格行没以 `|` 收尾，这一行又不是表格行：单元格里换行了。
            if !t.starts_with('|') && !t.is_empty() {
                problems.push(format!(
                    "第 {open} 行的表格行没有以 `|` 结尾，下一行的文字会掉出表格：单元格内不能换行，要换行写 `<br/>`"
                ));
            }
        }
        if t.starts_with('|') {
            let cols = table_cols(t);
            match table {
                None => {
                    // 表头 + 分隔行才算表格。
                    let next = lines.get(i + 1).map(|l| l.trim()).unwrap_or("");
                    if is_separator(next) {
                        table = Some((ln, cols));
                    } else if cols >= 3 && !is_separator(t) && !orphan_run {
                        // 没有表头的表格行：表格被中间插进的段落截断了（实测
                        // lat-113：位字段表中间插了一段 PAC/HPA 解释，后半截表格
                        // 成了一串 `| … |` 纯文本）。
                        problems.push(format!(
                            "第 {ln} 行起是表格行，但前面没有表头和 `|---|` 分隔行：表格被中间插入的文字打断了，把说明挪到表格前后"
                        ));
                        orphan_run = true;
                    }
                }
                Some((head, n)) => {
                    if !is_separator(t) && cols != n && t.ends_with('|') {
                        problems.push(format!(
                            "第 {ln} 行表格有 {cols} 列，表头（第 {head} 行）是 {n} 列：单元格里的 `|` 要写成 `\\|`"
                        ));
                    }
                }
            }
            if table.is_some() && !t.ends_with('|') {
                prev_table_row_open = Some(ln);
            }
            continue;
        }
        table = None;
        orphan_run = false;
        if let Some(dir) = dir {
            for target in local_images(t) {
                let path = target.split(['#', '?']).next().unwrap_or("");
                if !path.is_empty() && !dir.join(path).exists() {
                    problems.push(format!("第 {ln} 行的图片 `{target}` 不存在"));
                }
            }
        }
    }
    if let Some((ln, c, n)) = fence {
        problems.push(format!(
            "第 {ln} 行开始的代码块（{}）没有闭合，后面整篇都会被当成代码",
            std::iter::repeat_n(c, n).collect::<String>()
        ));
    }
    problems
}

/// 表格行的列数（忽略行内代码与转义的 `\|`）。
///
/// 行内代码按 CommonMark 的规则配对：N 个连续反引号开头，到下一段**恰好 N 个**连续反引号
/// 结束，中间原样不解析；找不到配对的反引号就是普通字符。早先逐个反引号翻转"在代码里"开关，
/// 单元格里出现 `` `line="```rust"` `` 这种（单反引号包着三连反引号）会让奇偶错位，后面的
/// `|` 全被当成代码里的字符，列数算成 1 列，白白把合法表格打回去（实测：写 markdown 围栏
/// 相关主题时，逐轮推演表整张被判坏，重试用完只能带着"坏表格"放行）。
fn table_cols(row: &str) -> usize {
    let chars: Vec<char> = row
        .trim()
        .trim_start_matches('|')
        .trim_end_matches('|')
        .chars()
        .collect();
    let run_len = |from: usize| chars[from..].iter().take_while(|c| **c == '`').count();
    let mut n = 0;
    let mut prev = ' ';
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '`' {
            let open = run_len(i);
            // 找下一段长度恰好等于 open 的反引号。
            let mut k = i + open;
            let mut close_end = None;
            while k < chars.len() {
                if chars[k] == '`' {
                    let m = run_len(k);
                    if m == open {
                        close_end = Some(k + m);
                        break;
                    }
                    k += m;
                } else {
                    k += 1;
                }
            }
            i = close_end.unwrap_or(i + open);
            prev = '`';
            continue;
        }
        if c == '|' && prev != '\\' {
            n += 1;
        }
        prev = c;
        i += 1;
    }
    n + 1
}

fn is_separator(row: &str) -> bool {
    let r = row.trim();
    r.starts_with('|') && r.contains('-') && r.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

/// 一行里的本地图片目标（不含 http(s)/data/绝对路径）。
fn local_images(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(p) = rest.find("![") {
        rest = &rest[p + 2..];
        let Some(close) = rest.find("](") else { break };
        let after = &rest[close + 2..];
        let Some(end) = after.find(')') else { break };
        let target = after[..end]
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string();
        rest = &after[end..];
        if target.is_empty()
            || target.contains("://")
            || target.starts_with("data:")
            || target.starts_with('/')
        {
            continue;
        }
        out.push(target);
    }
    out
}

/// 发布后的文档里指不到文件的本地链接与图片：`root` 是入口 md，连同它链到的
/// 本地 md（传递闭包）一起查。返回「文件: 目标」。
pub fn dead_links(root: &std::path::Path) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut queue = vec![root.to_path_buf()];
    let mut dead = Vec::new();
    while let Some(md) = queue.pop() {
        if !seen.insert(md.clone()) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&md) else {
            continue;
        };
        let dir = md.parent().map(|d| d.to_path_buf()).unwrap_or_default();
        let mut in_fence = false;
        for line in text.lines() {
            if line.trim_start().starts_with("```") {
                in_fence = !in_fence;
                continue;
            }
            if in_fence {
                continue;
            }
            for target in link_targets(line) {
                let path = target.split(['#', '?']).next().unwrap_or("");
                if path.is_empty() {
                    continue;
                }
                let p = dir.join(path);
                if !p.exists() {
                    let name = md
                        .strip_prefix(root.parent().unwrap_or(&dir))
                        .unwrap_or(&md);
                    dead.push(format!("{}: {target}", name.display()));
                } else if path.ends_with(".md") {
                    queue.push(p);
                }
            }
        }
    }
    dead
}

/// 一行里的本地链接与图片目标。
fn link_targets(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(p) = rest.find("](") {
        let after = &rest[p + 2..];
        let Some(end) = after.find(')') else { break };
        let target = after[..end]
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string();
        rest = &after[end..];
        if target.is_empty()
            || target.contains("://")
            || target.starts_with('#')
            || target.starts_with("mailto:")
            || target.starts_with("data:")
            || target.starts_with('/')
        {
            continue;
        }
        out.push(target);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unclosed_fence() {
        let p = lint("正文\n```c\nint x;\n", None);
        assert!(
            p[0].contains("第 2 行") && p[0].contains("没有闭合"),
            "{p:?}"
        );
        assert!(lint("```c\nint x;\n```\n````md\n```\n````\n", None).is_empty());
    }

    /// lat-113 位字段表的原形：单元格里起了列表，行没收尾。
    #[test]
    fn table_row_broken_by_newline() {
        let doc = "| 位 | 字段 | 含义 |\n|---|---|---|\n| `z` | `zeroed` | 是否全 0 |\n| `i` | `szind` | 下标。四个宏不要混淆：\n- SC_NSIZES\n- SC_NBINS |\n\n后文\n";
        let p = lint(doc, None);
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].contains("第 4 行") && p[0].contains("<br/>"), "{p:?}");
    }

    #[test]
    fn table_column_mismatch_ignores_code_and_escapes() {
        let ok = "| a | b |\n|---|---|\n| `x|y` | 1 \\| 2 |\n";
        assert!(lint(ok, None).is_empty(), "{:?}", lint(ok, None));
        let bad = "| a | b |\n|---|---|\n| x | y | z |\n";
        assert!(lint(bad, None)[0].contains("3 列"), "{:?}", lint(bad, None));
        // 不是表格（没有分隔行）的 `|` 开头行不查。
        assert!(lint("| 只是一行竖线开头的字\n下一行\n", None).is_empty());
    }

    /// lat-113 原形：表格中间插了一段说明，后半截表格没有表头。
    /// 单反引号包着三连反引号的单元格不能打乱列数（逐个反引号翻转开关会错位成 1 列）。
    #[test]
    fn table_cols_pairs_backtick_runs_like_commonmark() {
        assert_eq!(table_cols("| 1 | `line=\"```rust\"` | 否 |"), 3);
        assert_eq!(table_cols("| 4 | ```` ``` ```` | 是 | x |"), 4);
        assert_eq!(table_cols("| `a` | `b` |"), 2);
        // 没有配对的反引号是普通字符，后面的 | 照常分列
        assert_eq!(table_cols("| it`s | b | c |"), 3);
        // 转义的 \| 不分列；真正多出来的 | 仍然会被算出来
        assert_eq!(table_cols("| a \\| b | c |"), 2);
        assert_eq!(table_cols("| a | b | c |"), 3);
        let md = "| 轮次 | 变量 | 动作 |\n|---|---|---|\n| 1 | `line=\"```rust\"` | 翻转 |\n";
        assert!(lint(md, None).is_empty(), "{:?}", lint(md, None));
    }

    #[test]
    fn table_split_by_inserted_paragraph() {
        let doc = "| 位 | 字段 | 含义 |\n|---|---|---|\n| `a` | `arena` | 所属 arena |\n\n**PAC** = 经典页分配器。\n| `z` | `zeroed` | 是否全 0 |\n| `g` | `guarded` | 是否挂 guard |\n\n正文\n";
        let p = lint(doc, None);
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].contains("第 6 行") && p[0].contains("打断"), "{p:?}");
        // 两列的 `| a | b |` 单行（如引用式排版）不报。
        assert!(lint("| 注意 | 这里 |\n", None).is_empty());
    }

    #[test]
    fn think_leak_reported_once() {
        let p = lint("a\n</think>\n\n<think>继续写</think>\n", None);
        assert_eq!(p.len(), 1);
        assert!(p[0].contains("<think>"));
        assert!(
            lint("```\n<think>\n```\n", None).is_empty(),
            "代码块里的不算"
        );
    }

    #[test]
    fn missing_local_image() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("img")).unwrap();
        std::fs::write(dir.path().join("img/a.svg"), "<svg/>").unwrap();
        let doc = "![有](img/a.svg) ![没有](img/b.svg) ![远程](https://x/y.png)\n";
        let p = lint(doc, Some(dir.path()));
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].contains("img/b.svg"));
    }

    #[test]
    fn dead_links_follow_local_md_closure() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::create_dir_all(d.join("doc/sections")).unwrap();
        std::fs::create_dir_all(d.join("doc/img")).unwrap();
        std::fs::write(d.join("doc/img/ok.svg"), "<svg/>").unwrap();
        std::fs::write(
            d.join("doc.md"),
            "[一](doc/sections/k1.md) [外](https://a.b) [锚](#x)\n",
        )
        .unwrap();
        std::fs::write(
            d.join("doc/sections/k1.md"),
            "![好](../img/ok.svg) ![坏](../img/gone.svg) [下一章](k6.md)\n```\n[代码里](nope.md)\n```\n",
        )
        .unwrap();
        let dead = dead_links(&d.join("doc.md"));
        assert_eq!(dead.len(), 2, "{dead:?}");
        assert!(
            dead.iter().any(|x| x.ends_with("../img/gone.svg")),
            "{dead:?}"
        );
        assert!(dead.iter().any(|x| x.ends_with("k6.md")), "{dead:?}");
    }
}
