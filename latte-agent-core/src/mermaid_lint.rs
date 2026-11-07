//! 写文档时的 mermaid 语法门禁：**检查 + 引擎自动修正**。
//!
//! 学习文档（learn_loop / doc_review_loop）里的 mermaid 是模型手写的，实测
//! jemalloc 会话里 602 张不重复的图有 30 张在页面上渲染失败、退化成一段代码。
//! 失败几乎都是同一批**机械可修**的写法（节点文字含括号没加引号、subgraph
//! 标题带全角标点……），这类问题不该靠 prompt 叮嘱、也不该让 writer 白白重写
//! 一轮：引擎按规则直接改写图源、写回文件。只有修不了的（引号不成对、第一行
//! 不是图类型）才带回给 writer 重试。
//!
//! 规则不是凭印象写的：每一条都在真实浏览器 + mermaid 12 上逐条探测过
//! 「会失败 / 不会失败」（见各规则注释）。宁可漏判不误判——误判的代价是把
//! 本来能渲染的图改了样子。UI 侧 `ui/src/mermaid_repair.ts` 是同一套规则的
//! 渲染期兜底（给门禁上线前写出的旧文档用）。

/// 合法的图类型头（第一行第一个词）。
const KINDS: &[&str] = &[
    "flowchart",
    "graph",
    "sequenceDiagram",
    "stateDiagram",
    "classDiagram",
    "erDiagram",
    "gantt",
    "pie",
    "mindmap",
    "timeline",
    "journey",
    "quadrantChart",
    "gitGraph",
    "requirementDiagram",
    "sankey",
    "kanban",
    "block-beta",
    "packet-beta",
    "xychart-beta",
    "architecture-beta",
    "radar-beta",
];

/// 不加引号就会让 mermaid 解析失败的节点文字字符（实测：`A[f(x)]`、`A[a[1]]`、
/// `A[x{y}]`、`A[a|b]`、`A[@x]` 失败；`#` `;` `<` `->` `%` `*` `=` 全角括号/冒号都不失败）。
const NODE_SPECIAL: &[char] = &['(', ')', '[', ']', '{', '}', '|', '@'];
/// `-->|文字|` 连线文字同理（实测：括号、方括号、花括号、`@` 失败；`:` `#` `<` `>` `;` `&` `/` 不失败）。
const EDGE_SPECIAL: &[char] = &['(', ')', '[', ']', '{', '}', '@'];
/// 裸 subgraph 标题允许的标点（实测：`a-b`、`a+b`、`a: b`、`src/a.c`、`a.c` 通过；
/// 全角括号/逗号/顿号/冒号/句号、`→`、`<`、`@`、半角括号都失败）。字母数字（含汉字）总是允许。
const SUBGRAPH_TITLE_OK: &[char] = &[' ', '_', '-', '+', ':', '/', '.'];

const ARROWS: &[&str] = &["-.->", "-->", "==>"];

fn ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn quote(text: &str) -> String {
    format!("\"{}\"", text.trim().replace('"', "#quot;"))
}

fn closer(c: char) -> Option<char> {
    match c {
        '[' => Some(']'),
        '(' => Some(')'),
        '{' => Some('}'),
        _ => None,
    }
}

/// `prefix` 之后是不是节点位置（行首 / 箭头 / 连线文字 / `&` 之后）。
fn node_position(prefix: &str) -> bool {
    let t = prefix.trim_end();
    t.is_empty() || t.ends_with('>') || t.ends_with('|') || t.ends_with('&') || t.ends_with("---")
}

/// `subgraph 名字 [标题]`：名字部分只有允许的字符、标题部分已加引号或不含特殊
/// 字符——实测能渲染（`subgraph malloc 前台 [malloc 前台: szind 体系]`）。
fn bracket_title_ok(title: &str) -> bool {
    let Some(open) = title.find('[') else {
        return false;
    };
    if !title.ends_with(']') {
        return false;
    }
    let (name, inner) = (&title[..open], &title[open + 1..title.len() - 1]);
    name.chars()
        .all(|c| c.is_alphanumeric() || SUBGRAPH_TITLE_OK.contains(&c))
        && ((inner.starts_with('"') && inner.ends_with('"')) || !inner.contains(NODE_SPECIAL))
}

/// 一行 flowchart 语句的修正。问题描述推进 `notes`。
struct LineFix<'a> {
    notes: &'a mut Vec<String>,
}

impl LineFix<'_> {
    /// `"a" + "b"` 拼接 → `"ab"`（实测失败；引号内的字面 `\n` 实测能渲染，不动）。
    fn strings(&mut self, line: &str) -> String {
        let mut out = String::with_capacity(line.len());
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '"' {
                if let Some(end) = chars[i + 1..]
                    .iter()
                    .position(|&c| c == '"')
                    .map(|p| i + 1 + p)
                {
                    let inner: String = chars[i + 1..end].iter().collect();
                    out.push('"');
                    out.push_str(&inner);
                    // 紧跟 `+ "` 的拼接：吞掉 `" + "`，继续同一个字符串。
                    let mut j = end + 1;
                    while j < chars.len() && chars[j] == ' ' {
                        j += 1;
                    }
                    if chars.get(j) == Some(&'+') {
                        let mut k = j + 1;
                        while k < chars.len() && chars[k] == ' ' {
                            k += 1;
                        }
                        if chars.get(k) == Some(&'"') {
                            self.notes
                                .push("标签里用 `\"a\" + \"b\"` 拼字符串，mermaid 不支持".into());
                            i = k + 1;
                            // 把后半段接到同一个引号里：不输出闭引号，从后半段内容继续。
                            let rest_end = chars[i..].iter().position(|&c| c == '"').map(|p| i + p);
                            if let Some(re) = rest_end {
                                let more: String = chars[i..re].iter().collect();
                                out.push_str(&more);
                                out.push('"');
                                i = re + 1;
                                continue;
                            }
                        }
                    }
                    out.push('"');
                    i = end + 1;
                    continue;
                }
            }
            out.push(chars[i]);
            i += 1;
        }
        out
    }

    /// 带文字的连线混写（实测四种都失败）：
    ///   `A -- "x" .-> B`、`A -.-> "x" --> B` → `A -. "x" .-> B`
    ///   `A --> "x" --> B`                    → `A -- "x" --> B`
    ///   `A -.废弃的 "x" .-> B`               → `A -. "废弃的 x" .-> B`
    fn links(&mut self, line: &str) -> String {
        let mut s = line.to_string();
        // 前三种：一个引号串夹在两段箭头之间。
        let mut search = 0;
        while let Some(q) = s[search..].find('"').map(|p| search + p) {
            let Some(qe) = s[q + 1..].find('"').map(|p| q + 1 + p) else {
                break;
            };
            let before = s[..q].trim_end();
            let after = s[qe + 1..].trim_start();
            let label = &s[q..=qe];
            let rewrite = if before.ends_with("-.->") && after.starts_with("-->") {
                Some((before.len() - 4, "-.", ".->", 3))
            } else if before.ends_with("-->") && after.starts_with("-->") {
                Some((before.len() - 3, "--", "-->", 3))
            } else if before.ends_with("--") && !before.ends_with("<--") && after.starts_with(".->")
            {
                Some((before.len() - 2, "-.", ".->", 3))
            } else {
                None
            };
            if let Some((cut, l, r, skip)) = rewrite {
                self.notes.push(format!("连线文字 {label} 的箭头写法不合法，带文字的连线写成 `A -- \"文字\" --> B` 或 `A -. \"文字\" .-> B`"));
                let tail_start = s.len() - after.len() + skip;
                let head = format!("{} {l} {label} ", s[..cut].trim_end());
                search = head.len();
                s = format!("{head}{r}{}", &s[tail_start..]);
            } else {
                search = qe + 1;
            }
        }
        // 第四种：`-.` 后直接跟文字（不是 `-.->` / `-. `），文字里还夹着引号串。
        if let Some(p) = s
            .find("-.")
            .filter(|&p| s[..p].matches('"').count() % 2 == 0)
        {
            let rest = &s[p + 2..];
            let next = rest.chars().next().unwrap_or(' ');
            if next != '-' && next != ' ' {
                if let Some(end) = rest.find(".->").filter(|&e| rest[..e].contains('"')) {
                    let text: String = rest[..end]
                        .replace('"', " ")
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ");
                    self.notes.push(format!(
                        "虚线文字「{text}」写法不合法，写成 `A -. \"文字\" .-> B`"
                    ));
                    s = format!("{}-. {} .->{}", &s[..p], quote(&text), &rest[end + 3..]);
                }
            }
        }
        s
    }

    /// 两条连线挤在一行：`n0 --> n1 n1 --> n2`（实测失败；code_graph 生成的图
    /// 被模型抄写时最常见）。只认两端都是裸 id 的简单情形。
    fn split_edges(&mut self, line: &str, indent: &str) -> String {
        let toks: Vec<&str> = line.split_whitespace().collect();
        let is_id = |t: &str| !t.is_empty() && t.chars().all(ident_char);
        let is_arrow = |t: &str| ARROWS.contains(&t);
        if toks.len() >= 5
            && is_id(toks[0])
            && is_arrow(toks[1])
            && is_id(toks[2])
            && is_id(toks[3])
            && is_arrow(toks[4])
        {
            self.notes
                .push(format!("一行里写了两条连线：`{line}`，每条连线单独一行"));
            let first = toks[..3].join(" ");
            let rest = toks[3..].join(" ");
            return format!("{first}\n{indent}{}", self.split_edges(&rest, indent));
        }
        line.to_string()
    }

    /// `-->|文字|` 里含特殊字符却没加引号。
    fn edge_labels(&mut self, line: &str) -> String {
        // 只认引号外的 `|`：`A["a|b"]` 里的竖线是节点文字。
        let bars: Vec<usize> = {
            let mut in_q = false;
            line.char_indices()
                .filter_map(|(i, c)| {
                    if c == '"' {
                        in_q = !in_q;
                    }
                    (c == '|' && !in_q).then_some(i)
                })
                .collect()
        };
        let mut out = String::new();
        let mut last = 0;
        for pair in bars.chunks_exact(2) {
            let (a, b) = (pair[0], pair[1]);
            let text = &line[a + 1..b];
            out.push_str(&line[last..=a]);
            if !text.starts_with('"') && text.contains(EDGE_SPECIAL) {
                self.notes.push(format!(
                    "连线文字「{text}」含括号/@ 却没加双引号，写成 `-->|\"{text}\"|`"
                ));
                out.push_str(&quote(text));
            } else {
                out.push_str(text);
            }
            out.push('|');
            last = b + 1;
        }
        out.push_str(&line[last..]);
        out
    }

    /// 节点文字含特殊字符却没加引号：`A[f(x)]`、`D[arena->bins[i]]`、
    /// `A[[sub(x)]]`、`A([x(y)])`、`F{x(y)}`（实测全部失败）。
    fn node_labels(&mut self, line: &str) -> String {
        let chars: Vec<char> = line.chars().collect();
        let mut out = String::new();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c == '"' {
                let end = chars[i + 1..]
                    .iter()
                    .position(|&x| x == '"')
                    .map_or(chars.len(), |p| i + 2 + p);
                out.extend(&chars[i..end]);
                i = end;
                continue;
            }
            if c == '|' {
                // 连线文字整段跳过（edge_labels 已处理）。
                let end = chars[i + 1..]
                    .iter()
                    .position(|&x| x == '|')
                    .map_or(chars.len(), |p| i + 2 + p);
                out.extend(&chars[i..end]);
                i = end;
                continue;
            }
            // 节点只出现在行首、箭头 / `|文字|` / `&` 之后；`A -- f(x) --> B`
            // 里的 `f(x)` 是连线文字（实测能渲染），不能当节点改。
            let id_start =
                ident_char(c) && (i == 0 || !ident_char(chars[i - 1])) && node_position(&out);
            if !id_start {
                out.push(c);
                i += 1;
                continue;
            }
            let mut j = i;
            while j < chars.len() && ident_char(chars[j]) {
                j += 1;
            }
            out.extend(&chars[i..j]);
            let Some(close) = chars.get(j).and_then(|&o| closer(o)) else {
                i = j;
                continue;
            };
            let open = chars[j];
            // 外层括号配平。
            let mut depth = 0;
            let mut k = j;
            let mut in_q = false;
            while k < chars.len() {
                let x = chars[k];
                if x == '"' {
                    in_q = !in_q;
                } else if !in_q && x == open {
                    depth += 1;
                } else if !in_q && x == close {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                k += 1;
            }
            if k >= chars.len() {
                // 不配平：不臆造结构，原样输出剩余部分。
                out.extend(&chars[j..]);
                break;
            }
            let inner: String = chars[j + 1..k].iter().collect();
            // 复合形状 `[[ ]]` `([ ])` `[( )]` `(( ))` `{{ }}`：剥掉内层一对再看文字。
            let (pre, text, post) = match inner
                .chars()
                .next()
                .and_then(|f| closer(f).map(|cl| (f, cl)))
            {
                Some((f, cl)) if inner.len() >= 2 && inner.ends_with(cl) => (
                    f.to_string(),
                    inner[f.len_utf8()..inner.len() - cl.len_utf8()].to_string(),
                    cl.to_string(),
                ),
                _ => (String::new(), inner.clone(), String::new()),
            };
            let shape_ok = !text.starts_with('"')
                && !text.starts_with('/')
                && !text.starts_with('\\')
                && !text.starts_with('>');
            if shape_ok && !text.is_empty() && text.contains(NODE_SPECIAL) {
                self.notes.push(format!("节点文字「{}」含括号/竖线/@ 却没加双引号，写成 `{}{open}{pre}\"…\"{post}{close}`", text.trim(), chars[i..j].iter().collect::<String>()));
                out.push(open);
                out.push_str(&pre);
                out.push_str(&quote(&text));
                out.push_str(&post);
                out.push(close);
            } else {
                out.extend(&chars[j..=k]);
            }
            i = k + 1;
        }
        out
    }
}

/// 一张图的分析结果。
pub struct MermaidCheck {
    /// 引擎能机械修正的问题（修正后的图源在 `fixed`）。
    pub fixable: Vec<String>,
    /// 修不了、要带回给 writer 的问题。
    pub fatal: Vec<String>,
    /// 修正后的图源；没有可修的问题时为 None。
    pub fixed: Option<String>,
}

/// 检查一张图（围栏内的源码，不含 ``` 行）。
pub fn check(src: &str) -> MermaidCheck {
    let mut fixable = Vec::new();
    let mut fatal = Vec::new();
    let lines: Vec<&str> = src.lines().collect();
    let Some(first_idx) = lines.iter().position(|l| {
        let t = l.trim();
        !t.is_empty() && !t.starts_with("%%")
    }) else {
        fatal.push("图是空的".into());
        return MermaidCheck {
            fixable,
            fatal,
            fixed: None,
        };
    };
    let head = lines[first_idx].trim();
    let kind = head
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_end_matches(':');
    let known = head.starts_with("---")
        || KINDS
            .iter()
            .any(|k| kind == *k || kind.starts_with(&format!("{k}-")));
    let mut prefix = String::new();
    let is_flow = if known {
        kind == "flowchart" || kind == "graph"
    } else {
        // 第一行就是节点/连线语句（`A[…]`、`A --> B`）：缺了图类型头，补 flowchart TD。
        let id_len = head
            .chars()
            .take_while(|&c| c.is_ascii_alphanumeric() || c == '_')
            .count();
        let after = head[id_len..].trim_start();
        let stmt = id_len > 0
            && (after.starts_with(['[', '(', '{'])
                || ["-->", "---", "-.", "==>"]
                    .iter()
                    .any(|a| after.starts_with(a)));
        if !stmt {
            fatal.push(format!("第一行「{head}」不是图类型（应为 flowchart TD / sequenceDiagram / stateDiagram-v2 …）"));
            return MermaidCheck {
                fixable,
                fatal,
                fixed: None,
            };
        }
        fixable.push(format!("缺图类型头：第一行「{head}」前要有 `flowchart TD`"));
        prefix = "flowchart TD\n".into();
        true
    };

    let mut sg = 0;
    let mut out_lines = Vec::with_capacity(lines.len());
    for (idx, raw) in lines.iter().enumerate() {
        let indent_len = raw.len() - raw.trim_start().len();
        let (indent, body) = raw.split_at(indent_len);
        let body = body.trim_end();
        if (idx == first_idx && known) || body.is_empty() || body.starts_with("%%") {
            out_lines.push(raw.to_string());
            continue;
        }
        if body.matches('"').count() % 2 == 1 {
            fatal.push(format!("引号不成对：`{body}`"));
            out_lines.push(raw.to_string());
            continue;
        }
        if !is_flow {
            out_lines.push(raw.to_string());
            continue;
        }
        let first = body.split_whitespace().next().unwrap_or("");
        if [
            "end",
            "classDef",
            "class",
            "style",
            "linkStyle",
            "click",
            "direction",
        ]
        .contains(&first)
        {
            out_lines.push(raw.to_string());
            continue;
        }
        // 连线没有终点（`A -->` 结尾）：几乎总是输出被截断，修不了。
        if ["-->", "--", "-.->", "==>", "---", "-."]
            .iter()
            .any(|a| body.ends_with(a))
        {
            fatal.push(format!(
                "连线没有终点：`{body}`（图源可能被截断，补全这一行）"
            ));
            out_lines.push(raw.to_string());
            continue;
        }
        let mut fx = LineFix {
            notes: &mut fixable,
        };
        if first == "subgraph" {
            let title = body["subgraph".len()..].trim();
            let id_len = title
                .chars()
                .take_while(|&c| ident_char(c))
                .map(char::len_utf8)
                .sum::<usize>();
            // `id[标题]` / `id [标题]`（中间有空格也合法，实测通过）。
            let shape_at = id_len + (title.len() - id_len - title[id_len..].trim_start().len());
            let has_shape =
                id_len > 0 && title[shape_at..].starts_with('[') && title.ends_with(']');
            let line = if has_shape {
                // 按节点文字规则处理（统一去掉 id 与 `[` 之间的空格）。
                format!(
                    "subgraph {}",
                    fx.node_labels(&format!("{}{}", &title[..id_len], &title[shape_at..]))
                )
            } else if (title.starts_with('"') && title.ends_with('"'))
                || title
                    .chars()
                    .all(|c| c.is_alphanumeric() || SUBGRAPH_TITLE_OK.contains(&c))
                || bracket_title_ok(title)
            {
                format!("subgraph {title}")
            } else {
                sg += 1;
                fx.notes.push(format!("subgraph 标题「{title}」含全角标点/括号等，写成 `subgraph sg{sg}[\"{title}\"]`"));
                format!("subgraph sg{sg}[{}]", quote(title))
            };
            out_lines.push(format!("{indent}{line}"));
            continue;
        }
        let line = fx.strings(body);
        let line = fx.links(&line);
        let line = fx.edge_labels(&line);
        let line = fx.node_labels(&line);
        let line = fx.split_edges(&line, indent);
        out_lines.push(format!("{indent}{line}"));
    }
    let mut fixed = prefix + &out_lines.join("\n");
    if src.ends_with('\n') {
        fixed.push('\n');
    }
    let fixed = (!fixable.is_empty() && fixed != src).then_some(fixed);
    MermaidCheck {
        fixable,
        fatal,
        fixed,
    }
}

/// 一段 markdown 里每个 ```mermaid 围栏的 (源码起止字节, 源码)。未闭合的围栏到文末为止。
fn spans(text: &str) -> Vec<(std::ops::Range<usize>, String)> {
    let mut out = Vec::new();
    let mut pos = 0;
    let mut open: Option<(usize, usize)> = None; // (源码起点, 围栏长度)
    let mut other: Option<usize> = None;
    for line in text.split_inclusive('\n') {
        let start = pos;
        pos += line.len();
        let t = line.trim();
        let run = t.chars().take_while(|&c| c == '`').count();
        if let Some((s, fl)) = open {
            if run >= fl && t[run..].trim().is_empty() {
                out.push((s..start, text[s..start].to_string()));
                open = None;
            }
            continue;
        }
        if let Some(fl) = other {
            if run >= fl && t[run..].trim().is_empty() {
                other = None;
            }
            continue;
        }
        if run >= 3 {
            if t[run..].trim().split_whitespace().next() == Some("mermaid") {
                open = Some((pos, run));
            } else {
                other = Some(run);
            }
        }
    }
    if let Some((s, _)) = open {
        out.push((s..text.len(), text[s..].to_string()));
    }
    out
}

/// 文档里的 mermaid 图数。
pub fn count_blocks(text: &str) -> usize {
    spans(text).len()
}

/// 整篇文档的门禁结果。
pub struct DocCheck {
    /// 自动修正后的全文；没有修正时为 None。
    pub fixed: Option<String>,
    /// 引擎已修正的问题（只记录，不打回）。
    pub fixed_notes: Vec<String>,
    /// 修不了的问题（打回 writer）。
    pub fatal: Vec<String>,
}

/// 检查并修正整篇 markdown 里的 mermaid。修正后的图会再检查一遍：修正引入了
/// 新问题（理论上不该发生）就放弃这张图的修正，把原问题当成修不了的打回。
pub fn check_doc(text: &str) -> DocCheck {
    let mut fixed_text = String::with_capacity(text.len());
    let mut last = 0;
    let mut changed = false;
    let mut fixed_notes = Vec::new();
    let mut fatal = Vec::new();
    for (n, (range, src)) in spans(text).into_iter().enumerate() {
        let n = n + 1;
        let c = check(&src);
        fatal.extend(c.fatal.iter().map(|p| format!("第 {n} 张 mermaid 图：{p}")));
        let mut replacement = None;
        if let Some(fixed) = c.fixed {
            let again = check(&fixed);
            if again.fixable.is_empty() && again.fatal.len() <= c.fatal.len() {
                fixed_notes.extend(
                    c.fixable
                        .iter()
                        .map(|p| format!("第 {n} 张 mermaid 图：{p}")),
                );
                replacement = Some(fixed);
            } else {
                fatal.extend(
                    c.fixable
                        .iter()
                        .map(|p| format!("第 {n} 张 mermaid 图：{p}")),
                );
            }
        }
        fixed_text.push_str(&text[last..range.start]);
        match replacement {
            Some(r) => {
                fixed_text.push_str(&r);
                changed = true;
            }
            None => fixed_text.push_str(&src),
        }
        last = range.end;
    }
    fixed_text.push_str(&text[last..]);
    DocCheck {
        fixed: changed.then_some(fixed_text),
        fixed_notes,
        fatal,
    }
}

/// 门禁入口：读文件 → 能修的直接改写回文件 → 返回 (已修正的问题, 修不了的问题)。
/// 读不到文件时两者都为空（缺文件由调用方的其它检查报）。
pub fn autofix_file(path: &std::path::Path) -> (Vec<String>, Vec<String>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (Vec::new(), Vec::new());
    };
    let r = check_doc(&text);
    if let Some(fixed) = &r.fixed {
        if let Err(e) = std::fs::write(path, fixed) {
            tracing::warn!(path = %path.display(), error = %e, "mermaid 自动修正写回失败");
            let mut fatal = r.fatal;
            fatal.extend(r.fixed_notes);
            return (Vec::new(), fatal);
        }
        tracing::info!(path = %path.display(), fixes = r.fixed_notes.len(), "mermaid 语法已由引擎自动修正");
    }
    (r.fixed_notes, r.fatal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed(src: &str) -> String {
        let c = check(src);
        assert!(c.fatal.is_empty(), "{:?}", c.fatal);
        c.fixed.unwrap_or_else(|| panic!("应有修正：{src}"))
    }

    // 以下用例全部取自 jemalloc 会话里实测在页面上渲染失败的图源。

    #[test]
    fn quotes_node_labels_with_brackets() {
        let out = fixed("flowchart TD\n  A[前端调用 resolve_one(choice_id, q)] --> D[通过柔性数组 arena->bins[bin_ind] 定位]\n  R2[@x 5 个] --> A\n");
        assert!(
            out.contains("A[\"前端调用 resolve_one(choice_id, q)\"]"),
            "{out}"
        );
        assert!(
            out.contains("D[\"通过柔性数组 arena->bins[bin_ind] 定位\"]"),
            "{out}"
        );
        assert!(out.contains("R2[\"@x 5 个\"]"), "{out}");
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn compound_shapes_and_quoted_untouched() {
        let out = fixed("flowchart TD\n  A[\"f(x)\"] --> B[[sub(x)]]\n  C((圆)) --> E([x(y)])\n  F{x(y)} --> G[(db)]\n");
        assert!(out.contains("A[\"f(x)\"]"), "{out}");
        assert!(out.contains("B[[\"sub(x)\"]]"), "{out}");
        assert!(out.contains("C((圆))"), "{out}");
        assert!(out.contains("E([\"x(y)\"])"), "{out}");
        assert!(out.contains("F{\"x(y)\"}"), "{out}");
        assert!(out.contains("G[(db)]"), "{out}");
    }

    #[test]
    fn renderable_specials_are_not_flagged() {
        // 实测这些都能渲染：不报、不改。
        for src in [
            "flowchart TD\n  A[a->b] --> B[#x]\n  C[a < b; 50%] --> D{bin->cur 有空闲?}\n",
            "flowchart TD\n  A -->|a.c:4| B\n  A -->|是（快）| C\n  A -- f(x) --> D\n  A -- call f(g(x)) --> E\n  A[\"a|b(c)\"] --> F\n",
            "graph LR\n  subgraph sz_boot 一次填表\n    A\n  end\n  subgraph src/a.c\n    B\n  end\n  subgraph s1[阶段：x]\n    C\n  end\n",
            "sequenceDiagram\n  A->>B: 取锁(1)\n",
            "stateDiagram-v2\n  [*] --> Idle\n",
            "flowchart TD\n  A[\"x\"]:::hot --> B\n  classDef hot fill:#f00\n  style B fill:#f9f\n",
            "flowchart LR\n  subgraph malloc 前台 [malloc 前台: szind 体系]\n    A\n  end\n",
            "flowchart TD\n  subgraph TSD [\"线程 tsd_t\"]\n    A\n  end\n  A -.断言对账.-> B\n  B -. \"x\" .-> C\n",
        ] {
            let c = check(src);
            assert!(c.fixable.is_empty() && c.fatal.is_empty() && c.fixed.is_none(), "{src}: {:?} {:?}", c.fixable, c.fatal);
        }
    }

    #[test]
    fn quotes_edge_labels() {
        let out = fixed("flowchart TD\n  base -->|base_alloc_impl (src/base.c:485)| ed\n  R -->|@role line| I\n  A -->|有空闲(1)| G\n");
        assert!(
            out.contains("-->|\"base_alloc_impl (src/base.c:485)\"| ed"),
            "{out}"
        );
        assert!(out.contains("-->|\"@role line\"| I"), "{out}");
        assert!(out.contains("-->|\"有空闲(1)\"| G"), "{out}");
    }

    #[test]
    fn subgraph_titles_with_cjk_punctuation() {
        let out = fixed("graph LR\n  subgraph 面向用户（低频）\n    U\n  end\n  subgraph 阶段1：启动\n    V\n  end\n  subgraph FAST[\"入口 (x.h:1)\"]\n  end\n");
        assert!(out.contains("subgraph sg1[\"面向用户（低频）\"]"), "{out}");
        assert!(out.contains("subgraph sg2[\"阶段1：启动\"]"), "{out}");
        assert!(out.contains("subgraph FAST[\"入口 (x.h:1)\"]"), "{out}");
    }

    #[test]
    fn mixed_link_text_forms() {
        let out = fixed("flowchart TD\n  H2 -- \"bins[5]\" .-> P5\n  ED -.-> \"容纳\" --> BM\n  X --> \"维持\" --> Y\n  D1 -.废弃的 \"单全局堆\" .-> A1\n");
        assert!(out.contains("H2 -. \"bins[5]\" .-> P5"), "{out}");
        assert!(out.contains("ED -. \"容纳\" .-> BM"), "{out}");
        assert!(out.contains("X -- \"维持\" --> Y"), "{out}");
        assert!(out.contains("D1 -. \"废弃的 单全局堆\" .-> A1"), "{out}");
    }

    #[test]
    fn string_concat_and_literal_newline() {
        let out = fixed("flowchart LR\n  F1[\"size 参数\" + \"\\n寄存器传入\"] --> F2\n");
        assert!(out.contains("F1[\"size 参数\\n寄存器传入\"]"), "{out}");
    }

    #[test]
    fn splits_two_edges_on_one_line() {
        let out = fixed("flowchart LR\n  n0 --> n1 n1 --> n2\n  n2 --> n3\n");
        assert!(out.contains("  n0 --> n1\n  n1 --> n2\n"), "{out}");
    }

    #[test]
    fn missing_header_added_only_for_statements() {
        assert!(fixed("t3[\"sz_pind2sz_tab\"] --> t4\n").starts_with("flowchart TD\n"));
        let c = check("Hmm, this starts with a mermaid block --> x\n");
        assert!(c.fatal[0].contains("不是图类型") && c.fixed.is_none());
        assert!(check("").fatal[0].contains("空"));
    }

    #[test]
    fn dangling_arrow_is_fatal() {
        let c = check("flowchart LR\n  A --> B\n  T2 --\n");
        assert!(c.fatal[0].contains("没有终点"), "{:?}", c.fatal);
    }

    #[test]
    fn subgraph_id_space_bracket_title_normalized_when_bad() {
        let out = fixed("flowchart LR\n  subgraph M [malloc(前台)]\n  A\n  end\n");
        assert!(out.contains("subgraph M[\"malloc(前台)\"]"), "{out}");
    }

    #[test]
    fn unbalanced_quote_is_fatal() {
        let c = check("flowchart LR\n  A[\"未闭合] --> B\n");
        assert!(c.fatal[0].contains("引号不成对"), "{:?}", c.fatal);
    }

    #[test]
    fn check_doc_rewrites_only_mermaid_fences() {
        let doc = "# t\n\n```mermaid\nflowchart TD\n  A[f(x)] --> B\n```\n\n```c\nA[f(x)]\n```\n\n````md\n```mermaid\nA[g(y)]\n```\n````\n";
        let r = check_doc(doc);
        let out = r.fixed.expect("改了第一张图");
        assert!(out.contains("A[\"f(x)\"] --> B"), "{out}");
        assert!(out.contains("```c\nA[f(x)]\n```"), "代码块不动：{out}");
        assert!(out.contains("A[g(y)]"), "嵌在 md 示例里的围栏不动：{out}");
        assert_eq!(r.fixed_notes.len(), 1, "{:?}", r.fixed_notes);
        assert!(r.fatal.is_empty());
        assert_eq!(count_blocks(doc), 1);
        assert!(check_doc(&out).fixed.is_none(), "修正结果幂等");
    }

    #[test]
    fn autofix_file_writes_back() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.md");
        std::fs::write(
            &p,
            "```mermaid\nflowchart TD\n  subgraph 用户（低频）\n  A\n  end\n```\n",
        )
        .unwrap();
        let (fixed, fatal) = autofix_file(&p);
        assert_eq!(fixed.len(), 1);
        assert!(fatal.is_empty());
        assert!(std::fs::read_to_string(&p)
            .unwrap()
            .contains("subgraph sg1[\"用户（低频）\"]"));
        let (fixed, _) = autofix_file(&p);
        assert!(fixed.is_empty(), "第二次无事可做");
    }
}
