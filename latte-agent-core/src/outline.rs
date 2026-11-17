//! 大纲文档（outline doc）：任务树的第二权威。
//!
//! 为什么需要这个模块：看板负责"谁在做什么"（任务树 + 状态），大纲 Markdown 负责
//! "为什么这么做"（结构 + 人手写的叙述）。两者靠 `<!-- node:<task id> -->` 锚点对齐，
//! 而**只有本模块懂这个文件的格式**——其它模块一律通过 [`parse_outline`] /
//! [`render_outline`] 进出，不许自己拼这个文件的字符串，否则格式一改就到处漏改。
//!
//! 格式（设计文档 §5）：第一个 `<!-- node: -->` 之前的全部内容是人手写的前言，
//! 除行尾规范化外机器一个字都不许动（原样存进 [`OutlineDoc::preamble`]）。
//! 机器只碰四类受控区块：
//! `section:body`、`learn-links`、`children`，以及 node 锚点本身。
//!
//! 解析为什么是"按行扫描"而不是正则：不能新增依赖；这个格式的标记独占一行、结构
//! 简单，按行扫描足够，而且能顺手给出**出错行号**——手改大纲写崩时行号比"格式错误"
//! 有用得多。
//!
//! # 契约：渲染是**规范化**，不是无损往返
//!
//! `render_outline(parse_outline(x))` 对**规范输入**逐字节相同（规范样例见下面的
//! `FIXTURE` 与 `canonical_fixture_round_trips_byte_exactly`），一般输入则会被
//! 规范化：机器保证"内容不丢"，但不保证"排版不变"。为什么不追每个空格：为了
//! 每一处随意空白都往返，`OutlineNode` 里就得塞满排版字段，结构体被噪声占满而
//! 真正要紧的（id、标题、正文）反而看不清。
//!
//! 会被规范化掉的只有这几类，别的一概不动：
//!
//! - 区块之间、标题前后的空行：不保留（正文必须显式写在 `section:body` 里）；
//! - 受控标记与 node 锚点的行首缩进：统一去掉；
//! - 锚点内的空白：`<!-- node:X  needs_split -->` → `<!-- node:X needs_split -->`；
//! - 行尾序列：CRLF → LF（正文和前言的内容行除换行符外一字不动）；
//! - 空的 `children` 区块：渲染时丢弃（内容为空的标记没有信息量）。

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

const NODE_PREFIX: &str = "<!-- node:";
const BODY_START: &str = "<!-- section:body:start -->";
const BODY_END: &str = "<!-- section:body:end -->";
const LEARN_START: &str = "<!-- learn-links:start -->";
const LEARN_END: &str = "<!-- learn-links:end -->";
const CHILDREN_START: &str = "<!-- children:start -->";
const CHILDREN_END: &str = "<!-- children:end -->";

/// 大纲树上的一个节点。
///
/// 节点与看板任务的唯一纽带是 [`OutlineNode::id`]；导入前它是 `tmp:<标题>`，
/// 导入完成后被换成真实 task id（见 [`rebind_tmp_ids`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineNode {
    /// 导入前为 `tmp:<title>`，导入后为真实 task id。
    pub id: String,
    pub title: String,
    pub needs_split: bool,
    /// 标题行原文（含 `### ` 前缀）。`None` = 锚点后没有标题行（根节点常见）。
    ///
    /// 保留原文是为了排版信息不丢：标题的 `#` 层级、奇怪的空白都按原样写回。
    pub heading_raw: Option<String>,
    /// 标题层级（`#` 的个数）：`### 标题` → 3；没有标题行时为 0。
    ///
    /// 为什么显式存一份而不是每次从 [`OutlineNode::heading_raw`] 数：
    /// 之后要在大纲里插入子节点（Task 7）时需要"父层级 + 1"这种算术，
    /// 让调用方去解析标题文本既脆又容易数错。
    pub heading_level: u8,
    /// `section:body` 区块内的正文，机器同步时原样保留。
    ///
    /// 内容含结尾换行（区块非空时必然是 `...\n`），这样渲染时可以直接拼回去。
    pub body: String,
    /// 该节点是否带 `section:body` 区块——没有就**不要**在 render 时凭空加。
    pub has_body_block: bool,
    /// `learn-links` 区块内的原始行（不含换行符）。区块为空时这里是空数组。
    pub learn_links: Vec<String>,
    pub has_learn_links_block: bool,
    pub children: Vec<OutlineNode>,
}

impl OutlineNode {
    /// 子节点构造器：层级由调用方给出，避免从标题文本反推。
    ///
    /// `heading_level` 是 `#` 的个数（1..=6 是 Markdown 的常规范围，0 按 1 处理）。
    ///
    /// **为什么 body / learn-links 区块默认存在（内容为空）**：大纲生成器
    /// （outline_writer 模板）写出来的节点永远带这两个区块，随后的手工编辑只是
    /// 往里填字。构造器跟着模板走，生成的文件才和模板逐字节同构；反过来"没有区块
    /// 就省掉"会让同一棵树因为"谁构造的"而渲染出两种形状，diff 全是噪声。
    pub fn new_child(
        id: String,
        title: String,
        needs_split: bool,
        heading_level: u8,
    ) -> OutlineNode {
        let level = heading_level.max(1);
        OutlineNode {
            id,
            title: title.clone(),
            needs_split,
            heading_raw: Some(make_heading(level, &title)),
            heading_level: level,
            body: String::new(),
            has_body_block: true,
            learn_links: Vec::new(),
            has_learn_links_block: true,
            children: Vec::new(),
        }
    }
}

/// 整份大纲文档。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineDoc {
    pub task_id: String,
    /// 根任务的标题。
    ///
    /// **文档里展示的标题不归它管**：那是 [`OutlineDoc::preamble`] 里人手写的
    /// `# 总纲` 一级标题，机器从不改写。根节点没有自己的标题行
    /// （[`OutlineNode::heading_raw`] 为 `None`）时，[`OutlineNode::title`] 也只是
    /// 结构元数据——大纲 → 看板的同步靠它，但文件里没有它对应的那一行文字。
    pub title: String,
    /// 第一个 `<!-- node: -->` 之前的全部原文（frontmatter、文档标题、人手写的前言）。
    ///
    /// 以换行结尾（锚点在第 0 字节时为空串），渲染时直接拼在最前面；行尾已按
    /// 契约规范化为 LF。
    pub preamble: String,
    pub root: OutlineNode,
}

/// 看板任务的精简视图：让 core 不依赖 ui-server 的类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskBrief {
    pub id: String,
    pub title: String,
    pub parent_id: Option<String>,
    pub needs_split: bool,
}

/// 受控区块标记（不含 node 锚点，锚点单独识别因为它带 id）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    BodyStart,
    BodyEnd,
    LearnStart,
    LearnEnd,
    ChildrenStart,
    ChildrenEnd,
}

impl Marker {
    /// 这一行是不是受控标记（容忍行首尾空白，但不容忍行内夹杂文字）。
    fn of(line: &str) -> Option<Marker> {
        match line.trim() {
            BODY_START => Some(Marker::BodyStart),
            BODY_END => Some(Marker::BodyEnd),
            LEARN_START => Some(Marker::LearnStart),
            LEARN_END => Some(Marker::LearnEnd),
            CHILDREN_START => Some(Marker::ChildrenStart),
            CHILDREN_END => Some(Marker::ChildrenEnd),
            _ => None,
        }
    }

    /// 标记的原文，用于错误信息里点名"是哪一个标记出了问题"。
    fn text(self) -> &'static str {
        match self {
            Marker::BodyStart => BODY_START,
            Marker::BodyEnd => BODY_END,
            Marker::LearnStart => LEARN_START,
            Marker::LearnEnd => LEARN_END,
            Marker::ChildrenStart => CHILDREN_START,
            Marker::ChildrenEnd => CHILDREN_END,
        }
    }
}

/// 一行是不是 node 锚点（`<!-- node:<id> [needs_split] -->`）。
fn is_anchor(line: &str) -> bool {
    let t = line.trim();
    t.starts_with(NODE_PREFIX) && t.ends_with("-->")
}

/// 解析锚点行，返回 `(id, needs_split)`。调用方已确认 [`is_anchor`]。
fn parse_anchor(line: &str) -> (String, bool) {
    let t = line.trim();
    // 去掉 `<!-- node:` 前缀和结尾的 `-->`。
    let inner = &t[NODE_PREFIX.len()..t.len() - 3];
    let inner = inner.trim();
    match inner.strip_suffix("needs_split") {
        Some(head) if head.is_empty() || head.ends_with(char::is_whitespace) => {
            (head.trim_end().to_string(), true)
        }
        _ => (inner.to_string(), false),
    }
}

/// 标题行里 `#` 的个数（`### 标题` → 3）。没有 `#` 前缀的行返回 0。
fn heading_level_of(line: &str) -> u8 {
    let n = line.trim_start().chars().take_while(|c| *c == '#').count();
    n.min(u8::MAX as usize) as u8
}

/// 由层级与标题文字拼出规范标题行（`3, "模块一"` → `### 模块一`）。
fn make_heading(level: u8, title: &str) -> String {
    format!("{} {}", "#".repeat(level.max(1) as usize), title)
}

/// 把 `### 模块一` 之类的标题行取成纯标题文字。
///
/// 只吃掉 `#` 连串和紧跟其后的**一个**空格：标题自身以 `#` 开头时
/// （`### ## 小节` → `## 小节`）必须保住那几个 `#`，否则文字被吃掉一半。
fn heading_title(line: &str) -> String {
    let rest = line.trim_start().trim_start_matches('#');
    let rest = rest.strip_prefix(' ').unwrap_or(rest);
    rest.trim_end().to_string()
}

/// 找第一个 node 锚点行的**起始字节偏移**；它之前的一切都是前言。
fn first_anchor_offset(md: &str) -> Option<usize> {
    let mut off = 0usize;
    for line in md.split('\n') {
        if is_anchor(line) {
            return Some(off);
        }
        off += line.len() + 1; // +1 = 被 split 吃掉的那个 '\n'
    }
    None
}

/// 解析 frontmatter，返回 `(task_id, title)`。
///
/// 手写 `key: value` 扫描而不是引入 serde_yaml：frontmatter 只有两三个标量键，
/// 用 YAML 解析器反而会把"`title: 总纲` 里的冒号"这类琐碎差异变成神秘的报错。
fn parse_frontmatter(md: &str) -> Result<(String, String), String> {
    let mut lines = md.split('\n');
    let first = lines.next().unwrap_or("");
    if first.trim() != "---" {
        return Err("第 1 行: 大纲文档必须以 `---` frontmatter 开头".to_string());
    }
    let mut task_id: Option<String> = None;
    let mut title: Option<String> = None;
    let mut closed = false;
    let mut lineno = 1usize;
    // 缺键时报"结尾 `---` 那一行"：出错位置就是这个键本该出现的地方。
    let mut close_lineno = 1usize;
    for line in lines {
        lineno += 1;
        let t = line.trim();
        if t == "---" {
            closed = true;
            close_lineno = lineno;
            break;
        }
        if t.is_empty() {
            continue;
        }
        let Some((key, value)) = t.split_once(':') else {
            return Err(format!(
                "第 {lineno} 行: frontmatter 无法解析（不是 `key: value`）：{t}"
            ));
        };
        let (key, value) = (key.trim(), unquote(value.trim()));
        match key {
            "task_id" => task_id = Some(value),
            "title" => title = Some(value),
            "schema" => {
                if value != "1" {
                    return Err(format!(
                        "第 {lineno} 行: 不支持的 outline schema `{value}`（本代码只认识 1）"
                    ));
                }
            }
            // 未知键直接跳过：以后加字段不该让旧版本炸。
            _ => {}
        }
    }
    if !closed {
        return Err(format!(
            "第 {lineno} 行: frontmatter 未闭合：找不到结尾的 `---`"
        ));
    }
    let task_id =
        task_id.ok_or_else(|| format!("第 {close_lineno} 行: frontmatter 缺少 `task_id`"))?;
    let title = title.ok_or_else(|| format!("第 {close_lineno} 行: frontmatter 缺少 `title`"))?;
    Ok((task_id, title))
}

/// 去掉成对的引号（`title: "总纲"` 与 `title: 总纲` 等价）。
fn unquote(s: &str) -> String {
    for q in ['"', '\''] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

/// 行扫描解析器：游标 + 行号基准，只为给出人话错误信息。
struct Parser<'a> {
    lines: Vec<&'a str>,
    i: usize,
    /// `lines[0]` 在整份文档里的 1-based 行号。
    base_line: usize,
    /// 已经出现过的 node id → 它第一次出现的行下标（局部下标，报错时换算行号）。
    /// id 是节点身份，重复即歧义，必须在解析阶段就拦下来。
    seen: HashMap<String, usize>,
}

impl<'a> Parser<'a> {
    fn line_no(&self, i: usize) -> usize {
        self.base_line + i
    }

    fn err(&self, i: usize, msg: impl AsRef<str>) -> String {
        format!("第 {} 行: {}", self.line_no(i), msg.as_ref())
    }

    fn skip_blank(&mut self) {
        while self.i < self.lines.len() && self.lines[self.i].trim().is_empty() {
            self.i += 1;
        }
    }

    /// 解析一个节点（游标必须停在 node 锚点上）。
    ///
    /// 返回时游标停在"不属于本节点"的行上：兄弟锚点、上层 `children:end`，或 EOF。
    fn parse_node(&mut self) -> Result<OutlineNode, String> {
        let anchor_i = self.i;
        if anchor_i >= self.lines.len() || !is_anchor(self.lines[anchor_i]) {
            return Err(self.err(anchor_i, "期望 `<!-- node:... -->` 锚点"));
        }
        let (id, needs_split) = parse_anchor(self.lines[anchor_i]);
        if id.is_empty() {
            return Err(self.err(anchor_i, "node 锚点缺少 task id"));
        }
        self.i = anchor_i + 1;

        // id 是节点身份：第二次出现意味着"两个节点自认为同一个"，同步和重绑定
        // 都会认错人，所以宁可在解析时炸掉。
        if let Some(first) = self.seen.get(&id) {
            return Err(self.err(
                anchor_i,
                format!(
                    "重复的 node id `{id}`：第 {} 行已经出现过",
                    self.line_no(*first)
                ),
            ));
        }
        self.seen.insert(id.clone(), anchor_i);

        let mut node = OutlineNode {
            id,
            title: String::new(),
            needs_split,
            heading_raw: None,
            heading_level: 0,
            body: String::new(),
            has_body_block: false,
            learn_links: Vec::new(),
            has_learn_links_block: false,
            children: Vec::new(),
        };

        // 标题只可能是"锚点之后、任何区块开始之前"的第一个内容行：
        // 渲染时标题永远落在所有区块前面，所以只有这个位置的 `#` 行能往返；
        // 接受别处的 `#` 行会让 render(parse(x)) 把标题挪位置。
        let mut j = self.i;
        while j < self.lines.len() && self.lines[j].trim().is_empty() {
            j += 1;
        }
        if j < self.lines.len() && Marker::of(self.lines[j]).is_none() {
            let cand = self.lines[j];
            if cand.trim_start().starts_with('#') {
                node.heading_raw = Some(cand.to_string());
                node.heading_level = heading_level_of(cand);
                node.title = heading_title(cand);
                self.i = j + 1;
            }
        }

        // 空 children 区块会被丢弃（见 render_node），但仍然不该重复出现。
        let mut saw_children_block = false;
        loop {
            // 区块之间的空行不保留（正文必须显式写进 body 区块）。
            self.skip_blank();
            if self.i >= self.lines.len() {
                break;
            }
            let cur = self.lines[self.i];

            if let Some(marker) = Marker::of(cur) {
                match marker {
                    Marker::BodyStart => {
                        if node.has_body_block {
                            return Err(self.err(self.i, "重复的 `section:body` 区块"));
                        }
                        let start_i = self.i;
                        let (inner, next) =
                            self.collect_block(start_i, BODY_START, BODY_END, "section:body")?;
                        node.body = join_as_body(&inner);
                        node.has_body_block = true;
                        self.i = next;
                    }
                    Marker::LearnStart => {
                        if node.has_learn_links_block {
                            return Err(self.err(self.i, "重复的 `learn-links` 区块"));
                        }
                        let start_i = self.i;
                        let (inner, next) =
                            self.collect_block(start_i, LEARN_START, LEARN_END, "learn-links")?;
                        node.learn_links = inner;
                        node.has_learn_links_block = true;
                        self.i = next;
                    }
                    Marker::ChildrenStart => {
                        if saw_children_block {
                            return Err(self.err(self.i, "重复的 `children` 区块"));
                        }
                        saw_children_block = true;
                        let start_i = self.i;
                        node.children = self.parse_children(start_i)?;
                    }
                    Marker::ChildrenEnd => {
                        // 本节点的区块都结束了：这个 `children:end` 属于外面那个
                        // children 区块（"最近的开着的父节点"由这个游标位置决定）。
                        break;
                    }
                    Marker::BodyEnd => {
                        return Err(self.err(self.i, "多余的 `<!-- section:body:end -->`"));
                    }
                    Marker::LearnEnd => {
                        return Err(self.err(self.i, "多余的 `<!-- learn-links:end -->`"));
                    }
                }
                continue;
            }

            if is_anchor(cur) {
                // 兄弟锚点：交回给外层的 children 循环。
                break;
            }

            if cur.trim_start().starts_with('#') {
                // 标题已经收过（见前面那个唯一合法位置）——区块之后再冒出来的
                // `#` 行在这个格式里无家可归：不能当标题（渲染会把它挪到区块前，
                // 往返就变了），也不能当正文（它没有 body 区块）。
                return Err(self.err(
                    self.i,
                    format!(
                        "标题行出现在区块之后：一个节点只能有一条紧跟锚点的标题行，其余标题文字必须写进 `<!-- section:body:start -->` 区块：{}",
                        cur.trim()
                    ),
                ));
            }

            return Err(self.err(
                self.i,
                format!(
                    "节点 `{}` 内这一行既不是锚点也不是区块标记；正文必须写在 `<!-- section:body:start -->` 区块里：{}",
                    node.id,
                    cur.trim()
                ),
            ));
        }

        Ok(node)
    }

    /// 解析 children 区块（游标停在 `children:start` 上）。
    fn parse_children(&mut self, start_i: usize) -> Result<Vec<OutlineNode>, String> {
        self.i = start_i + 1;
        let mut children = Vec::new();
        loop {
            self.skip_blank();
            if self.i >= self.lines.len() {
                return Err(self.err(
                    start_i,
                    "`<!-- children:start -->` 未闭合：找不到 `<!-- children:end -->`",
                ));
            }
            let cur = self.lines[self.i];
            if Marker::of(cur) == Some(Marker::ChildrenEnd) {
                self.i += 1;
                return Ok(children);
            }
            if is_anchor(cur) {
                children.push(self.parse_node()?);
                continue;
            }
            if Marker::of(cur).is_some() {
                return Err(self.err(self.i, "children 区块里出现了不该有的区块标记"));
            }
            return Err(self.err(
                self.i,
                format!(
                    "children 区块里出现非锚点内容（子节点必须以 `<!-- node:... -->` 开头）：{}",
                    cur.trim()
                ),
            ));
        }
    }

    /// 收集一个区块的内部原始行，返回 `(内部行, 结束标记之后的游标)`。
    ///
    /// 区块内部只允许本区块自己的结束标记：混进别的受控标记说明结构写崩了
    /// （某个 `start` 忘了配对），当正文吞下去会把它永久藏起来。
    fn collect_block(
        &self,
        start_i: usize,
        start_marker: &str,
        end_marker: &str,
        name: &str,
    ) -> Result<(Vec<String>, usize), String> {
        let mut inner = Vec::new();
        let mut k = start_i + 1;
        while k < self.lines.len() {
            let t = self.lines[k].trim();
            if t == end_marker {
                return Ok((inner, k + 1));
            }
            if let Some(marker) = Marker::of(self.lines[k]) {
                return Err(self.err(
                    k,
                    format!(
                        "`{name}` 区块里出现了 `{}`：区块内部只允许正文和 `{end_marker}`（开始标记在第 {} 行）",
                        marker.text(),
                        self.line_no(start_i)
                    ),
                ));
            }
            inner.push(self.lines[k].to_string());
            k += 1;
        }
        Err(self.err(
            start_i,
            format!("`{start_marker}` 未闭合：找不到 `{end_marker}`"),
        ))
    }
}

/// 把区块内部行拼成正文文本：非空时保留结尾换行，渲染时才能原样拼回。
fn join_as_body(inner: &[String]) -> String {
    if inner.is_empty() {
        return String::new();
    }
    let mut s = inner.join("\n");
    s.push('\n');
    s
}

/// 解析大纲文档。
///
/// 出错时给出具体行号：手改大纲写崩了，行号比"格式错误"能救命。
/// CRLF 输入先规范化为 LF（见模块级"规范化"清单），因此渲染结果里不会再有 `\r`。
pub fn parse_outline(md: &str) -> Result<OutlineDoc, String> {
    // 行尾先统一：后面所有偏移、行号、正文拼接都按 LF 算，渲染时自然不再出现
    // '\r'。不逐行 trim：正文里的 '\r' 属于内容，只有行尾序列才该被规范化。
    let normalized: Cow<'_, str> = if md.contains('\r') {
        Cow::Owned(md.replace("\r\n", "\n"))
    } else {
        Cow::Borrowed(md)
    };
    let md: &str = normalized.as_ref();

    let (task_id, title) = parse_frontmatter(md)?;

    let anchor_off = first_anchor_offset(md)
        .ok_or_else(|| "找不到 `<!-- node:... -->` 锚点：大纲至少要有根节点".to_string())?;
    let preamble = md[..anchor_off].to_string();

    // 前言里出现区块标记 = 第一个锚点其实"嵌"在一个没有父锚点的区块里，
    // 归属无法确定。这种歧义必须报错，不能猜。
    for (idx, line) in preamble.split('\n').enumerate() {
        if let Some(marker) = Marker::of(line) {
            return Err(format!(
                "第 {} 行: 根锚点之前出现了区块标记 `{}`，第一个 node 锚点归属不明",
                idx + 1,
                marker.text()
            ));
        }
    }

    let base_line = md[..anchor_off].matches('\n').count() + 1;
    let mut parser = Parser {
        lines: md[anchor_off..].split('\n').collect(),
        i: 0,
        base_line,
        seen: HashMap::new(),
    };
    let root = parser.parse_node()?;

    // 根节点之外还能剩下的只有文件末尾的空行（`split('\n')` 的产物，渲染时
    // 已经由最后一行的换行覆盖）。再冒出锚点或标记就说明结构归属不明，
    // 例如第二个根锚点、多余的 `children:end`。
    parser.skip_blank();
    if parser.i < parser.lines.len() {
        let line = parser.lines[parser.i];
        return Err(format!(
            "第 {} 行: 根节点 `{}` 已经结束，这里又出现了 `{}`，无法确定它属于谁",
            parser.line_no(parser.i),
            root.id,
            line.trim()
        ));
    }

    Ok(OutlineDoc {
        task_id,
        title,
        preamble,
        root,
    })
}

/// 渲染大纲文档。
///
/// 对**规范输入**（见模块级的规范化清单与 `FIXTURE`）逐字节还原原文；其它输入
/// 输出的是同一棵树规范化后的形状，内容不丢、排版可能变。
pub fn render_outline(doc: &OutlineDoc) -> String {
    let mut out = String::new();
    out.push_str(&doc.preamble);
    render_node(&doc.root, &mut out);
    out
}

fn render_node(node: &OutlineNode, out: &mut String) {
    out.push_str(NODE_PREFIX);
    out.push_str(&node.id);
    if node.needs_split {
        out.push_str(" needs_split");
    }
    out.push_str(" -->\n");

    if let Some(heading) = &node.heading_raw {
        out.push_str(heading);
        out.push('\n');
    }
    // 只在区块原本存在时输出：凭空加一个空的 body 区块会让文件越来越吵，
    // 也让"人手写的结构"被机器改写。
    if node.has_body_block {
        out.push_str(BODY_START);
        out.push('\n');
        out.push_str(&node.body);
        out.push_str(BODY_END);
        out.push('\n');
    }
    if node.has_learn_links_block {
        out.push_str(LEARN_START);
        out.push('\n');
        for link in &node.learn_links {
            out.push_str(link);
            out.push('\n');
        }
        out.push_str(LEARN_END);
        out.push('\n');
    }
    // 只在有子节点时输出 children 区块：内容为空的标记对读者零价值，留着只会
    // 让文件越来越吵，所以解析出的空区块在渲染时被丢掉（规范化）。
    if !node.children.is_empty() {
        out.push_str(CHILDREN_START);
        out.push('\n');
        for child in &node.children {
            render_node(child, out);
        }
        out.push_str(CHILDREN_END);
        out.push('\n');
    }
}

/// 把 `tmp:<标题>` 锚点换成真实 task id；返回**仍然没绑上**的标题
/// （去重、保持出现顺序）。调用方拿到非空结果应当中止导入。
///
/// # 调用约束：绑定之后必须再查一次 id 唯一性
///
/// 本函数只改 id，不负责唯一性：两个不同的 `tmp:<标题>` 可能被映射到同一个真实
/// id（导入时同名任务被建了两遍），也可能撞上大纲里本来就没加 `tmp:` 前缀的 id。
/// 所以**调用方 MUST 在绑定后调用 [`validate_unique_ids`]**，不是 `Ok` 就中止
/// 导入——否则后续同步会把两个节点当成同一个任务，静默改错地方。
pub fn rebind_tmp_ids(doc: &mut OutlineDoc, title_to_id: &HashMap<String, String>) -> Vec<String> {
    let mut missing: Vec<String> = Vec::new();
    rebind_node(&mut doc.root, title_to_id, &mut missing);
    missing
}

fn rebind_node(
    node: &mut OutlineNode,
    title_to_id: &HashMap<String, String>,
    missing: &mut Vec<String>,
) {
    let key = node.id.strip_prefix("tmp:").map(|s| s.trim().to_string());
    if let Some(key) = key {
        match title_to_id.get(&key) {
            Some(real) => node.id = real.clone(),
            None => {
                // 去重但保持顺序：同一个临时标题出现两次时，人只需要看到一次。
                if !missing.iter().any(|m| m == &key) {
                    missing.push(key);
                }
            }
        }
    }
    for child in &mut node.children {
        rebind_node(child, title_to_id, missing);
    }
}

/// 看板 → 大纲：把看板侧的 title/needs_split 落到文档；结构冲突（父不一致、
/// 文档有而看板无的节点）报错。反向（大纲 → 看板）由 ui-server 的 sync 端点负责。
///
/// 手写内容（`body`、`learn_links`）一个字都不碰；大纲里有而看板没有的节点
/// 直接报错——静默丢弃一个带手写正文的节点是不可接受的损失。
/// 标题行只改文字、保留各自原有的 `#` 层级（层级是排版，看板不管排版）。
pub fn apply_board_fields(doc: &mut OutlineDoc, tasks: &[TaskBrief]) -> Result<(), String> {
    let index: HashMap<&str, &TaskBrief> = tasks.iter().map(|t| (t.id.as_str(), t)).collect();
    // 先整体校验再改：中途失败不该留下"改了一半"的文档。
    // id 唯一性也在这里兜住——身份都不成立时，谈标题是不是最新的没有意义。
    validate_unique_ids(doc)?;
    check_node(&doc.root, None, &index)?;
    apply_node(&mut doc.root, &index);
    Ok(())
}

/// 校验整棵树里 node id 不重复。
///
/// 解析时已经查过一遍（那时能给出两处行号）；这个入口给"内存里构造/改过 id
/// 之后"的调用方用——[`rebind_tmp_ids`] 之后就必查一次。
pub fn validate_unique_ids(doc: &OutlineDoc) -> Result<(), String> {
    let mut seen: HashSet<&str> = HashSet::new();
    check_unique_ids(&doc.root, &mut seen)
}

fn check_unique_ids<'a>(node: &'a OutlineNode, seen: &mut HashSet<&'a str>) -> Result<(), String> {
    if !seen.insert(node.id.as_str()) {
        return Err(format!(
            "重复的 node id `{}`：同一份大纲里 id 必须唯一（否则同步/重绑定会认错节点）",
            node.id
        ));
    }
    for child in &node.children {
        check_unique_ids(child, seen)?;
    }
    Ok(())
}

fn check_node(
    node: &OutlineNode,
    parent: Option<&str>,
    index: &HashMap<&str, &TaskBrief>,
) -> Result<(), String> {
    let Some(task) = index.get(node.id.as_str()) else {
        return Err(format!(
            "大纲节点 `{}`（{}）在看板任务里不存在；不会静默丢弃它的手写正文",
            node.id, node.title
        ));
    };
    match (parent, task.parent_id.as_deref()) {
        (None, None) => {}
        (None, Some(task_parent)) => {
            return Err(format!(
                "大纲根节点 `{}` 在看板里 parent_id = `{task_parent}`，两边树位置不一致",
                node.id
            ));
        }
        (Some(outline_parent), Some(task_parent)) if outline_parent == task_parent => {}
        (Some(outline_parent), Some(task_parent)) => {
            return Err(format!(
                "节点 `{}` 的父节点不一致：大纲是 `{outline_parent}`，看板是 `{task_parent}`",
                node.id
            ));
        }
        (Some(outline_parent), None) => {
            return Err(format!(
                "节点 `{}` 在大纲里挂在 `{outline_parent}` 下，但看板里没有 parent_id",
                node.id
            ));
        }
    }
    for child in &node.children {
        check_node(child, Some(&node.id), index)?;
    }
    Ok(())
}

fn apply_node(node: &mut OutlineNode, index: &HashMap<&str, &TaskBrief>) {
    if let Some(task) = index.get(node.id.as_str()) {
        if node.title != task.title {
            // 标题是共享字段：文件里那行标题也得跟着变，否则渲染出来的文档会
            // 和看板互相打架。只保留原有的 `#` 层级（那是排版），文字用看板的。
            if let Some(raw) = node.heading_raw.as_deref() {
                let level = if node.heading_level > 0 {
                    node.heading_level
                } else {
                    heading_level_of(raw)
                };
                node.heading_level = level;
                node.heading_raw = Some(make_heading(level, &task.title));
            }
            // 根节点、以及锚点后没写标题行的节点，`heading_raw` 为 `None`：
            // 这时 `title` 只是结构元数据，**展示用**的文档标题在前言/frontmatter
            // 里（人手写的 `# 总纲`），这里只更新元数据，一个字的文档标题都不改。
            node.title = task.title.clone();
        }
        node.needs_split = task.needs_split;
    }
    // body / learn_links 属于人手写的内容，这里刻意不碰。
    for child in &mut node.children {
        apply_node(child, index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 设计文档 §5 的规范样例：`render(parse(x)) == x` 必须逐字节成立。
    /// 非规范输入走的是"规范化"契约（见模块文档），不要求逐字节。
    const FIXTURE: &str = r#"---
schema: 1
task_id: LAT-100
title: 总纲
---
# 总纲

人手写的前言，不能被机器改。

<!-- node:LAT-100 -->
<!-- section:body:start -->
## 概述
正文一段。
<!-- section:body:end -->
<!-- children:start -->
<!-- node:tmp:模块一 needs_split -->
### 模块一
<!-- section:body:start -->
子正文。
<!-- section:body:end -->
<!-- learn-links:start -->
<!-- learn-links:end -->
<!-- children:end -->
"#;

    #[test]
    fn canonical_fixture_round_trips_byte_exactly() {
        let doc = parse_outline(FIXTURE).expect("规范样例必须能解析");

        assert_eq!(doc.task_id, "LAT-100");
        assert_eq!(doc.title, "总纲");
        assert!(doc.preamble.ends_with("人手写的前言，不能被机器改。\n\n"));

        assert_eq!(doc.root.id, "LAT-100");
        assert_eq!(doc.root.title, "");
        assert_eq!(doc.root.heading_raw, None, "根节点不该有标题行");
        assert_eq!(doc.root.heading_level, 0);
        assert!(doc.root.has_body_block);
        assert_eq!(doc.root.body, "## 概述\n正文一段。\n");
        assert!(
            !doc.root.has_learn_links_block,
            "根节点没有 learn-links 区块"
        );
        assert_eq!(doc.root.children.len(), 1);

        let child = &doc.root.children[0];
        assert_eq!(child.id, "tmp:模块一");
        assert_eq!(child.title, "模块一");
        assert!(child.needs_split);
        assert_eq!(child.heading_raw.as_deref(), Some("### 模块一"));
        assert_eq!(child.heading_level, 3);
        assert_eq!(child.body, "子正文。\n");
        assert!(child.has_learn_links_block);
        assert!(child.learn_links.is_empty());
        assert!(child.children.is_empty());

        let rendered = render_outline(&doc);
        assert_eq!(
            rendered, FIXTURE,
            "\n渲染结果必须与原文逐字节相同\n{rendered:#?}"
        );
    }

    #[test]
    fn rebind_tmp_ids_replaces_all_and_reports_missing() {
        let md = "\
---
schema: 1
task_id: LAT-1
title: T
---
<!-- node:tmp:A -->
<!-- children:start -->
<!-- node:tmp:B -->
<!-- children:end -->
";
        let mut doc = parse_outline(md).expect("解析失败");
        let mut map = HashMap::new();
        map.insert("A".to_string(), "LAT-9".to_string());

        let missing = rebind_tmp_ids(&mut doc, &map);

        assert_eq!(doc.root.id, "LAT-9");
        assert_eq!(doc.root.children[0].id, "tmp:B");
        assert_eq!(missing, vec!["B".to_string()]);
        // 没有绑上的节点原文照旧（id 不合法不影响结构）。
        assert_eq!(render_outline(&doc), md.replace("tmp:A", "LAT-9"));
    }

    #[test]
    fn root_without_heading_round_trips() {
        let md = "\
---
schema: 1
task_id: LAT-1
title: T
---
<!-- node:LAT-1 -->
<!-- section:body:start -->
正文
<!-- section:body:end -->
";
        let doc = parse_outline(md).expect("解析失败");
        assert_eq!(doc.root.heading_raw, None);
        assert_eq!(doc.root.title, "");
        assert_eq!(doc.root.body, "正文\n");
        assert_eq!(render_outline(&doc), md);
    }

    #[test]
    fn node_without_body_or_learn_links_round_trips() {
        let md = "\
---
schema: 1
task_id: LAT-2
title: T
---
<!-- node:LAT-2 -->
### 根
<!-- children:start -->
<!-- node:LAT-3 -->
### 子
<!-- children:end -->
";
        let doc = parse_outline(md).expect("解析失败");
        assert_eq!(doc.root.children.len(), 1);
        assert!(!doc.root.has_body_block, "没写 body 区块就不该被解析出来");
        assert!(!doc.root.has_learn_links_block);
        let child = &doc.root.children[0];
        assert_eq!(child.title, "子");
        assert!(!child.has_body_block);
        assert!(!child.has_learn_links_block);
        assert!(child.children.is_empty());

        let rendered = render_outline(&doc);
        assert!(
            !rendered.contains("section:body") && !rendered.contains("learn-links"),
            "render 不许凭空造区块：\n{rendered}"
        );
        assert_eq!(rendered, md);
    }

    #[test]
    fn parse_rejects_unclosed_block() {
        let md = "\
---
schema: 1
task_id: LAT-5
title: T
---
<!-- node:LAT-5 -->
<!-- section:body:start -->
正文
";
        let err = parse_outline(md).expect_err("未闭合的 body 区块必须报错");
        assert!(
            err.contains("section:body:end"),
            "错误信息要指出缺哪个标记：{err}"
        );
        assert!(err.contains("第 7 行"), "错误信息要带行号：{err}");
    }

    #[test]
    fn parse_rejects_heading_after_block() {
        let md = "\
---
schema: 1
task_id: LAT-1
title: T
---
<!-- node:LAT-1 -->
<!-- section:body:start -->
正文
<!-- section:body:end -->
### 迟到的标题
";
        let err = parse_outline(md).expect_err("区块之后的标题行没有合法归属，必须报错");
        assert!(
            err.contains("标题") && err.contains("第 10 行"),
            "错误信息要点明标题位置并带行号：{err}"
        );
    }

    #[test]
    fn parse_rejects_foreign_marker_inside_body() {
        let md = "\
---
schema: 1
task_id: LAT-1
title: T
---
<!-- node:LAT-1 -->
<!-- section:body:start -->
正文
<!-- learn-links:start -->
<!-- section:body:end -->
";
        let err = parse_outline(md).expect_err("body 区块里的外来标记必须报错");
        assert!(
            err.contains("learn-links:start") && err.contains("第 9 行"),
            "错误信息要点名标记并带行号：{err}"
        );
    }

    #[test]
    fn leaf_with_empty_children_block_renders_without_it() {
        let md = "\
---
schema: 1
task_id: LAT-7
title: T
---
<!-- node:LAT-7 -->
### 叶子
<!-- children:start -->
<!-- children:end -->
";
        let doc = parse_outline(md).expect("解析失败");
        assert!(doc.root.children.is_empty(), "空 children 区块解析出空数组");

        let rendered = render_outline(&doc);
        assert!(
            !rendered.contains("children:"),
            "空的 children 区块不该被渲染出来：\n{rendered}"
        );
        assert_eq!(
            rendered,
            "\
---
schema: 1
task_id: LAT-7
title: T
---
<!-- node:LAT-7 -->
### 叶子
",
            "丢弃空区块后其余内容照旧"
        );
    }

    #[test]
    fn apply_board_fields_updates_title_keeps_body() {
        let md = "\
---
schema: 1
task_id: LAT-1
title: T
---
<!-- node:LAT-1 -->
### 旧标题
<!-- section:body:start -->
手写正文，机器别动。
<!-- section:body:end -->
<!-- learn-links:start -->
- [设计](design.md)
<!-- learn-links:end -->
<!-- children:start -->
<!-- node:LAT-2 -->
### 子节点
<!-- children:end -->
";
        let mut doc = parse_outline(md).expect("解析失败");
        let tasks = vec![
            TaskBrief {
                id: "LAT-1".to_string(),
                title: "新标题".to_string(),
                parent_id: None,
                needs_split: true,
            },
            TaskBrief {
                id: "LAT-2".to_string(),
                title: "子节点".to_string(),
                parent_id: Some("LAT-1".to_string()),
                needs_split: false,
            },
        ];

        apply_board_fields(&mut doc, &tasks).expect("同步失败");

        assert_eq!(doc.root.title, "新标题");
        assert!(doc.root.needs_split);
        assert_eq!(
            doc.root.heading_raw.as_deref(),
            Some("### 新标题"),
            "文件里的标题行也要跟着看板走（保留原有 # 层级）"
        );
        assert_eq!(doc.root.body, "手写正文，机器别动。\n", "body 不许被改");
        assert_eq!(
            doc.root.learn_links,
            vec!["- [设计](design.md)".to_string()]
        );
        assert_eq!(doc.root.children[0].title, "子节点");
        assert!(!doc.root.children[0].needs_split);
    }

    #[test]
    fn apply_board_fields_errors_on_unknown_node() {
        let md = "\
---
schema: 1
task_id: LAT-1
title: T
---
<!-- node:LAT-1 -->
### 根
<!-- children:start -->
<!-- node:LAT-404 -->
### 看板上没有的节点
<!-- children:end -->
";
        let mut doc = parse_outline(md).expect("解析失败");
        let tasks = vec![TaskBrief {
            id: "LAT-1".to_string(),
            title: "根".to_string(),
            parent_id: None,
            needs_split: false,
        }];

        let err = apply_board_fields(&mut doc, &tasks).expect_err("缺失节点必须报错");
        assert!(err.contains("LAT-404"), "错误信息要点名 id：{err}");
    }

    #[test]
    fn crlf_input_is_normalized_to_lf() {
        let crlf = FIXTURE.replace('\n', "\r\n");
        let doc = parse_outline(&crlf).expect("CRLF 输入必须能解析");
        let rendered = render_outline(&doc);
        assert!(
            !rendered.contains('\r'),
            "渲染结果里不许残留 CR：{rendered:?}"
        );
        assert_eq!(rendered, FIXTURE, "规范化后应当与 LF 版本完全一致");
        assert_eq!(
            doc.root,
            parse_outline(FIXTURE).expect("解析失败").root,
            "两种行尾解析出的树必须相同"
        );
    }

    #[test]
    fn rejects_duplicate_node_ids() {
        let md = "\
---
schema: 1
task_id: LAT-1
title: T
---
<!-- node:LAT-1 -->
<!-- children:start -->
<!-- node:LAT-1 -->
<!-- children:end -->
";
        let err = parse_outline(md).expect_err("重复 id 必须报错");
        assert!(err.contains("LAT-1"), "错误信息要点名 id：{err}");
        assert!(
            err.contains("第 8 行") && err.contains("第 6 行"),
            "错误信息要带两处行号（重复处与首次出现处）：{err}"
        );
    }

    #[test]
    fn rebind_then_validate_catches_collision() {
        let md = "\
---
schema: 1
task_id: LAT-1
title: T
---
<!-- node:tmp:A -->
<!-- children:start -->
<!-- node:tmp:B -->
<!-- children:end -->
";
        let mut doc = parse_outline(md).expect("解析失败");
        // 两个不同的临时标题被映射到同一个真实 id：导入建重复任务时就是这样。
        let mut map = HashMap::new();
        map.insert("A".to_string(), "LAT-9".to_string());
        map.insert("B".to_string(), "LAT-9".to_string());

        let missing = rebind_tmp_ids(&mut doc, &map);
        assert!(missing.is_empty(), "两个都绑上了：{missing:?}");

        let err = validate_unique_ids(&doc).expect_err("重绑定后的撞 id 必须被拦住");
        assert!(err.contains("LAT-9"), "错误信息要点名冲突的 id：{err}");
    }

    #[test]
    fn new_child_renders_valid_node() {
        let node = OutlineNode::new_child("LAT-9".to_string(), "新模块".to_string(), true, 3);
        let doc = OutlineDoc {
            task_id: "LAT-1".to_string(),
            title: "T".to_string(),
            preamble: "---\nschema: 1\ntask_id: LAT-1\ntitle: T\n---\n".to_string(),
            root: node.clone(),
        };

        let rendered = render_outline(&doc);
        assert!(
            rendered.contains("### 新模块"),
            "层级要写进标题行：{rendered}"
        );
        assert!(rendered.contains("needs_split"), "标记要带上：{rendered}");

        let reparsed = parse_outline(&rendered).expect("构造出来的节点必须能被解析回来");
        assert_eq!(reparsed.root, node, "渲染再解析必须得到同一个节点");
        assert_eq!(reparsed.root.heading_level, 3);
        assert!(validate_unique_ids(&reparsed).is_ok());
    }

    #[test]
    fn title_starting_with_hash_survives_round_trip() {
        let md = "\
---
schema: 1
task_id: LAT-1
title: T
---
<!-- node:LAT-1 -->
### ## 二级井号标题
<!-- children:start -->
<!-- node:LAT-2 -->
#### #tag 开头的标题
<!-- children:end -->
";
        let doc = parse_outline(md).expect("解析失败");
        assert_eq!(doc.root.title, "## 二级井号标题", "只吃掉一个空格后的井号");
        assert_eq!(doc.root.heading_level, 3);
        let child = &doc.root.children[0];
        assert_eq!(child.title, "#tag 开头的标题");
        assert_eq!(child.heading_level, 4);
        assert_eq!(render_outline(&doc), md);
    }

    #[test]
    fn deep_nesting_round_trips() {
        let md = "\
---
schema: 1
task_id: LAT-1
title: 深层
---
<!-- node:LAT-1 -->
<!-- section:body:start -->
根正文
<!-- section:body:end -->
<!-- children:start -->
<!-- node:LAT-2 -->
## 第一章
<!-- section:body:start -->
一章正文
<!-- section:body:end -->
<!-- children:start -->
<!-- node:LAT-3 -->
### 第一节
<!-- section:body:start -->
一节正文
<!-- section:body:end -->
<!-- children:start -->
<!-- node:LAT-4 -->
#### 子目
<!-- section:body:start -->
子目正文
<!-- section:body:end -->
<!-- children:end -->
<!-- children:end -->
<!-- children:end -->
";
        let doc = parse_outline(md).expect("解析失败");

        assert_eq!(doc.root.heading_raw, None);
        assert_eq!(doc.root.heading_level, 0, "没有标题行就没有层级");
        assert_eq!(doc.root.body, "根正文\n");

        let l1 = &doc.root.children[0];
        assert_eq!((l1.title.as_str(), l1.heading_level), ("第一章", 2));
        assert_eq!(l1.body, "一章正文\n");

        let l2 = &l1.children[0];
        assert_eq!((l2.title.as_str(), l2.heading_level), ("第一节", 3));
        assert_eq!(l2.body, "一节正文\n");

        let l3 = &l2.children[0];
        assert_eq!((l3.title.as_str(), l3.heading_level), ("子目", 4));
        assert_eq!(l3.body, "子目正文\n");
        assert!(l3.children.is_empty());

        assert_eq!(render_outline(&doc), md);
    }

    #[test]
    fn apply_board_fields_preserves_levels() {
        let md = "\
---
schema: 1
task_id: LAT-1
title: 总纲
---
# 总纲

<!-- node:LAT-1 -->
<!-- section:body:start -->
根正文
<!-- section:body:end -->
<!-- children:start -->
<!-- node:LAT-2 -->
### 旧一
<!-- section:body:start -->
一章正文
<!-- section:body:end -->
<!-- children:start -->
<!-- node:LAT-3 -->
##### 旧二
<!-- section:body:start -->
二节正文
<!-- section:body:end -->
<!-- children:end -->
<!-- children:end -->
";
        let mut doc = parse_outline(md).expect("解析失败");

        let good = vec![
            TaskBrief {
                id: "LAT-1".to_string(),
                title: "新总纲".to_string(),
                parent_id: None,
                needs_split: false,
            },
            TaskBrief {
                id: "LAT-2".to_string(),
                title: "新一".to_string(),
                parent_id: Some("LAT-1".to_string()),
                needs_split: false,
            },
            TaskBrief {
                id: "LAT-3".to_string(),
                title: "新二".to_string(),
                parent_id: Some("LAT-2".to_string()),
                needs_split: false,
            },
        ];

        // 父不一致：整体中止，一个字段都不许改。
        let mut bad = good.clone();
        bad[2].parent_id = Some("LAT-99".to_string());
        let snapshot = doc.clone();
        let err = apply_board_fields(&mut doc, &bad).expect_err("父不一致必须报错");
        assert!(err.contains("LAT-3") && err.contains("LAT-99"), "{err}");
        assert_eq!(doc, snapshot, "冲突时不能留下改了一半的文档");

        apply_board_fields(&mut doc, &good).expect("同步失败");

        // 文档标题（前言里人手写的 `# 总纲`）不归同步管。
        assert_eq!(doc.title, "总纲");
        assert!(doc.preamble.contains("# 总纲"));

        assert_eq!(doc.root.heading_raw, None, "根节点不该被塞上标题行");
        assert_eq!(doc.root.title, "新总纲");

        let l1 = &doc.root.children[0];
        assert_eq!(l1.heading_raw.as_deref(), Some("### 新一"));
        assert_eq!(l1.heading_level, 3);
        let l2 = &l1.children[0];
        assert_eq!(l2.heading_raw.as_deref(), Some("##### 新二"));
        assert_eq!(l2.heading_level, 5);

        // 正文一个字都不许动。
        assert_eq!(doc.root.body, "根正文\n");
        assert_eq!(l1.body, "一章正文\n");
        assert_eq!(l2.body, "二节正文\n");

        let rendered = render_outline(&doc);
        assert!(rendered.contains("##### 新二"), "{rendered}");
    }
}
