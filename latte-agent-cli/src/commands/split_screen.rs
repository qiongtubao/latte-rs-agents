//! chat 的**输入/显示分离**：历史推进 scrollback，输入与状态固定在底部。
//!
//! # 为什么需要它
//!
//! 原来的 REPL 是 `println!` + `read_line` 直出：后台事件（`[step 1/2]`、
//! `→ delegate programmer`）与提示符竞争同一行，长任务时输出错乱；也没法
//! 显示常驻状态（当前 step / 已耗时），只能塞进滚动历史；`ask` 选择器更是
//! "打印完再读同一个流"，依赖「没有游离任务发阻塞 ask」这个不成文前提。
//!
//! # 取舍：不进 alt screen
//!
//! 与 oh-my-pi 的 `pi-tui` 同一取舍——**刻意不用 alt-screen 全屏重绘**
//! （ratatui 的默认模式）。alt screen 会让终端 scrollback 失效，用户翻不了
//! 历史；而 chat 的历史正是最有价值的部分。所以这里只做两件事：
//!
//! ```text
//! ┌─ scrollback（终端所有，不可变）────────────┐
//! │ …历史输出…                                  │
//! ├─ viewport（我们重绘的固定区）───────────────┤
//! │ ⚙ manager · glm-5.3 · step 1/2 · 12s        │  ← 状态行
//! │ › 用户正在输入的内容                        │  ← 输入行
//! └────────────────────────────────────────────┘
//! ```
//!
//! 推进历史 = 光标移到 viewport 顶 → 清到屏幕底 → 打印历史行（终端自然
//! 上滚）→ 重绘 viewport。
//!
//! # 启用条件
//!
//! **只在 stdout 与 stdin 都是 TTY 时启用**。管道 / 重定向走原来的纯文本
//! 路径，逐字节不变——CLI 侧的 e2e（`blackbox.rs`、`hil_v11_e2e`）全都
//! `grep` stdout 文本，这条边界是它们的安全网。

use std::io::IsTerminal;

/// 低于这个列数就认为"尺寸读取失败"而非真实窗口宽度。
const MIN_SANE_WIDTH: usize = 20;
/// 尺寸不可用时的退回宽度。
const FALLBACK_WIDTH: usize = 80;

/// 把终端上报的列数归一成可用宽度。抽成纯函数以便测试——这里的
/// off-by-one 会把输出打碎成"每字符一行"，而在真终端上未必复现。
pub fn sane_width(reported: Option<u16>) -> usize {
    match reported {
        Some(w) if w as usize >= MIN_SANE_WIDTH => w as usize,
        _ => FALLBACK_WIDTH,
    }
}

/// 一行输入的编辑缓冲。按 **char** 而非 byte 索引——任务描述基本都是
/// 中文，按字节移动光标会切碎多字节字符。
#[derive(Debug, Default, Clone)]
pub struct EditBuffer {
    chars: Vec<char>,
    /// 光标位置，取值 `0..=chars.len()`（末尾可插入）。
    cursor: usize,
}

impl EditBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// 光标的**字符**下标（渲染时要按显示宽度换算，见 `display_width`）。
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn insert(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            self.insert(c);
        }
    }

    /// 退格。返回是否真的删了字符（用于决定要不要重绘）。
    pub fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        self.chars.remove(self.cursor);
        true
    }

    /// Delete 键：删光标右侧。
    pub fn delete(&mut self) -> bool {
        if self.cursor >= self.chars.len() {
            return false;
        }
        self.chars.remove(self.cursor);
        true
    }

    pub fn left(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        true
    }

    pub fn right(&mut self) -> bool {
        if self.cursor >= self.chars.len() {
            return false;
        }
        self.cursor += 1;
        true
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.chars.len();
    }

    /// Ctrl-U：清空整行。
    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
    }

    /// Ctrl-W：删掉光标左侧的一个「词」（先吃空白，再吃非空白）。
    pub fn delete_word_left(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let mut end = self.cursor;
        while end > 0 && self.chars[end - 1].is_whitespace() {
            end -= 1;
        }
        while end > 0 && !self.chars[end - 1].is_whitespace() {
            end -= 1;
        }
        self.chars.drain(end..self.cursor);
        self.cursor = end;
        true
    }

    /// 取走整行内容并清空（提交时用）。
    pub fn take(&mut self) -> String {
        let out = self.text();
        self.clear();
        out
    }

    pub fn set(&mut self, s: &str) {
        self.chars = s.chars().collect();
        self.cursor = self.chars.len();
    }
}

/// 字符的终端显示宽度：CJK / 全角算 2 列，其余算 1。
///
/// 不引入 `unicode-width` 依赖——只覆盖实际会遇到的区段（中日韩、全角
/// 标点、常用 emoji）。宽度算错的后果只是光标偏移，不会数据出错。
pub fn char_width(c: char) -> usize {
    let cp = c as u32;
    let wide = matches!(cp,
        0x1100..=0x115F        // Hangul Jamo
        | 0x2E80..=0x303E      // CJK 部首、中日韩符号
        | 0x3041..=0x33FF      // 平假名/片假名/注音/兼容
        | 0x3400..=0x4DBF      // CJK 扩展 A
        | 0x4E00..=0x9FFF      // CJK 基本区
        | 0xA000..=0xA4CF      // 彝文
        | 0xAC00..=0xD7A3      // 谚文音节
        | 0xF900..=0xFAFF      // CJK 兼容表意
        | 0xFE30..=0xFE6F      // CJK 兼容形式
        | 0xFF00..=0xFF60      // 全角 ASCII / 标点
        | 0xFFE0..=0xFFE6      // 全角符号
        | 0x1F300..=0x1F64F    // 常用 emoji
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x2FFFD    // CJK 扩展 B+
        | 0x30000..=0x3FFFD
    );
    if wide {
        2
    } else if cp < 0x20 || cp == 0x7F {
        // 控制字符不该出现在编辑缓冲里；按 0 列算，避免影响光标。
        0
    } else {
        1
    }
}

/// 字符串的显示宽度，**跳过 ANSI 转义序列**。
///
/// 必须跳过：提示符是带色的（`style::render_prompt` 会插入
/// `\x1b[1m\x1b[36m…`），把转义字节当可见字符算会把宽度高估约 30 列。
/// 后果有两个，且都很隐蔽：
///
/// - 光标被移到错误的列（实测：输入 `ab` 后下发 `\x1b[60G`，实际应在
///   ~25 列，偏了 35 列）；
/// - 窄终端上 `avail = 宽度 - 提示符宽度` 被压到 1，宽字符（2 列）
///   永远放不进去，**输入内容完全渲染不出来**（40 列终端实测复现）。
pub fn display_width(s: &str) -> usize {
    let mut w = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            skip_ansi(&mut chars);
            continue;
        }
        w += char_width(c);
    }
    w
}

/// 吞掉一段 ANSI 转义序列（`\x1b` 已被消费）。
///
/// 覆盖两类实际会出现的形态：CSI（`\x1b[` … 字母收尾）与 OSC
/// （`\x1b]` … BEL 或 ST 收尾）。其余单字符转义吞掉下一个字符即可。
fn skip_ansi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.peek() {
        Some('[') => {
            chars.next();
            // CSI 参数字节 0x30–0x3F、中间字节 0x20–0x2F，字母/符号收尾。
            for c in chars.by_ref() {
                if !matches!(c, '0'..='9' | ';' | ':' | '?' | '<' | '=' | '>' | ' ' | '!'..='/') {
                    break;
                }
            }
        }
        Some(']') => {
            chars.next();
            while let Some(c) = chars.next() {
                if c == '\u{7}' {
                    break;
                }
                if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                    chars.next();
                    break;
                }
            }
        }
        Some(_) => {
            chars.next();
        }
        None => {}
    }
}

/// 把一行按终端宽度硬折行（不做单词边界处理——历史行里大量是路径、
/// JSON、行号，按词折反而更难读）。
///
/// `width == 0` 时原样返回单行，避免除零 / 死循环。
pub fn wrap_line(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut w = 0usize;
    for c in s.chars() {
        let cw = char_width(c);
        // 宽字符跨边界时提前折行，不允许半个字符留在行尾。
        if w + cw > width && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            w = 0;
        }
        cur.push(c);
        w += cw;
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

/// 输入行的可见窗口：行内容比终端宽时，横向滚动到光标可见。
///
/// 返回 `(可见文本, 光标在可见文本内的列偏移)`。
pub fn visible_window(text: &str, cursor_chars: usize, width: usize) -> (String, usize) {
    let chars: Vec<char> = text.chars().collect();
    let cursor_chars = cursor_chars.min(chars.len());
    if width == 0 {
        return (String::new(), 0);
    }
    // 光标之前的宽度；整行放得下就直接全显示。
    let total: usize = chars.iter().map(|c| char_width(*c)).sum();
    if total <= width {
        let col: usize = chars[..cursor_chars].iter().map(|c| char_width(*c)).sum();
        return (chars.iter().collect(), col);
    }
    // 放不下：以光标为右锚，向左取满 width（留 1 列给光标本身）。
    let budget = width.saturating_sub(1).max(1);
    let mut start = cursor_chars;
    let mut w = 0usize;
    while start > 0 {
        let cw = char_width(chars[start - 1]);
        if w + cw > budget {
            break;
        }
        w += cw;
        start -= 1;
    }
    let mut vis = String::new();
    let mut vw = 0usize;
    for &c in &chars[start..] {
        let cw = char_width(c);
        if vw + cw > width {
            break;
        }
        vis.push(c);
        vw += cw;
    }
    let col: usize = chars[start..cursor_chars].iter().map(|c| char_width(*c)).sum();
    (vis, col)
}

/// 分离式界面是否可用。
///
/// 两端都必须是 TTY：stdout 不是 TTY 说明输出被重定向（e2e / 管道），
/// stdin 不是 TTY 说明输入是脚本喂的——两种情况都必须退回纯文本路径，
/// 否则转义序列会污染被断言的输出、raw mode 也读不到脚本输入。
pub fn split_screen_available() -> bool {
    std::io::stdout().is_terminal() && std::io::stdin().is_terminal()
}

/// 从 viewport 底部（光标所在行）回到 viewport 顶部要上移几行。
///
/// 抽成纯函数是为了可测：这里的 off-by-one 会直接吃掉一行历史，而在
/// 真终端上肉眼很难发现。`0` 表示**不要下发** `MoveUp`——见
/// [`SplitScreen::clear_viewport`] 里关于 `\x1b[0A` 的说明。
pub fn viewport_move_up(drawn_rows: u16) -> u16 {
    // 画了 N 行时光标停在第 N 行，回顶要上移 N-1 行；0 或 1 行都不用移。
    drawn_rows.saturating_sub(1)
}

/// 把粘贴进来的文本规整成可以塞进单行输入的内容。
///
/// 为什么需要：粘贴多行内容时，若把每个 `\n` 当回车处理，一段 5 行的
/// 代码会被拆成 5 次提交、前 4 次都是残句发给模型。终端的 bracketed
/// paste 会把整段作为一个 `Event::Paste` 交过来，这里把换行折成空格，
/// 让它成为一条完整输入。
///
/// 制表符同样折成空格：raw mode 下 `\t` 会让终端跳到下一个制表位，
/// 使我们对光标列的计算与实际不符。
pub fn normalize_pasted(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_was_space = false;
    for c in text.chars() {
        let c = match c {
            '\r' | '\n' | '\t' => ' ',
            // 其余控制字符直接丢：它们不可见，却会进入发给模型的内容。
            c if (c as u32) < 0x20 => continue,
            c => c,
        };
        if c == ' ' {
            // 折叠连续空白，避免粘贴缩进代码后出现一长串空格。
            if last_was_space {
                continue;
            }
            last_was_space = true;
        } else {
            last_was_space = false;
        }
        out.push(c);
    }
    out.trim().to_string()
}

/// 输入历史：上下箭头调出前几条输入。
///
/// 抽成独立类型是为了可测——历史导航的边界（在最新一条按下、在最旧
/// 一条按上、导航中途改内容）是最容易写错的部分，而在真终端上只能靠
/// 手敲验证。
#[derive(Debug, Default)]
pub struct InputHistory {
    entries: Vec<String>,
    /// 游标：`None` = 停在"正在编辑的新行"，`Some(i)` = 停在
    /// `entries[i]`。
    cursor: Option<usize>,
    /// 开始向上翻之前正在编辑的内容，翻回底部时还原。
    stash: String,
}

impl InputHistory {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记一条提交过的输入。
    ///
    /// 空行与"与上一条完全相同"都不记——否则连按回车会把历史刷满，
    /// 上翻要按十几次才能越过重复项。
    pub fn push(&mut self, line: &str) {
        let line = line.trim_end_matches(['\n', '\r']);
        if line.trim().is_empty() {
            self.reset_cursor();
            return;
        }
        if self.entries.last().map(String::as_str) != Some(line) {
            self.entries.push(line.to_string());
        }
        self.reset_cursor();
    }

    fn reset_cursor(&mut self) {
        self.cursor = None;
        self.stash.clear();
    }

    /// 向上（更旧）。`current` 是当前编辑内容，用于首次上翻时暂存。
    /// 返回要填进输入行的内容；`None` = 已在最旧一条，不动。
    pub fn prev(&mut self, current: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let next = match self.cursor {
            None => {
                self.stash = current.to_string();
                self.entries.len() - 1
            }
            Some(0) => return None,
            Some(i) => i - 1,
        };
        self.cursor = Some(next);
        Some(self.entries[next].clone())
    }

    /// 向下（更新）。走到底部时还原开始上翻前暂存的内容。
    pub fn next(&mut self) -> Option<String> {
        match self.cursor {
            None => None,
            Some(i) if i + 1 < self.entries.len() => {
                self.cursor = Some(i + 1);
                Some(self.entries[i + 1].clone())
            }
            Some(_) => {
                self.cursor = None;
                Some(std::mem::take(&mut self.stash))
            }
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// 分离式屏幕：历史推进 scrollback，状态行 + 输入行固定在底部。
///
/// 所有输出都走 **stdout**，且**不进 alt screen**——历史行是靠"打印后
/// 终端自然上滚"进入 scrollback 的，用户照旧能翻历史。
///
/// 光标不变式：任何公开方法返回时，硬件光标都停在**输入行的光标位置**。
/// 这样用户按键的回显位置永远正确，也不需要在按键路径上重新定位。
pub struct SplitScreen {
    /// 状态行内容（不含换行）。
    status: String,
    /// 输入提示符，如 `"👔 manager › "`。
    prompt: String,
    /// 编辑缓冲。
    pub buf: EditBuffer,
    /// 输入历史（↑/↓）。
    pub history: InputHistory,
    /// 上一次绘制 viewport 占用的物理行数——重绘时要先清掉这么多行。
    drawn_rows: u16,
    /// 是否已进入 raw mode（Drop 时据此决定要不要恢复）。
    raw: bool,
}

impl SplitScreen {
    /// 进入分离模式。失败（非 TTY / raw mode 不可用）返回 `None`，
    /// 调用方退回纯文本路径。
    pub fn enter(prompt: impl Into<String>) -> Option<Self> {
        if !split_screen_available() {
            return None;
        }
        crossterm::terminal::enable_raw_mode().ok()?;
        // 开启 bracketed paste：粘贴的多行内容作为**一个** `Event::Paste`
        // 到达，而不是逐字符 + 若干个回车（那会把一段代码拆成多次提交，
        // 前几次都是残句发给模型）。终端不支持时静默失败，退化成逐字符。
        {
            use std::io::Write;
            let mut out = std::io::stdout();
            let _ = crossterm::execute!(out, crossterm::event::EnableBracketedPaste);
            let _ = out.flush();
        }
        let mut me = Self {
            status: String::new(),
            prompt: prompt.into(),
            buf: EditBuffer::new(),
            history: InputHistory::new(),
            drawn_rows: 0,
            raw: true,
        };
        me.redraw();
        Some(me)
    }

    /// 终端列数。
    ///
    /// 拿不到真实尺寸（未设 winsize 的 pty、部分终端启动瞬间会报 0）时
    /// 退回 80，而**不是**钳到 1——钳到 1 会把每个字符折成一行，输出
    /// 彻底不可读。实机在 `pty.openpty()` 下抓到过：`\x1b[36m` 被逐字符
    /// 拆成 6 行。阈值取 20：比这更窄的"尺寸"几乎肯定是读取失败而非
    /// 真实窗口。
    fn width(&self) -> usize {
        sane_width(crossterm::terminal::size().ok().map(|(w, _)| w))
    }

    /// 当前提示符。常驻按键线程提交一行时要用它拼历史行。
    pub fn prompt_text(&self) -> &str {
        &self.prompt
    }

    /// 更新提示符（切角色 / 换模型后）。
    pub fn set_prompt(&mut self, prompt: impl Into<String>) {
        self.prompt = prompt.into();
        self.redraw();
    }

    /// 更新状态行。内容不变时不重绘——避免高频事件把光标搅乱。
    pub fn set_status(&mut self, status: impl Into<String>) {
        let s = status.into();
        if s == self.status {
            return;
        }
        self.status = s;
        self.redraw();
    }

    /// 把若干行推进 scrollback（历史区），然后重绘 viewport。
    ///
    /// 这是历史与 viewport 分离的核心：先把光标移到 viewport 顶、清掉
    /// viewport 占的那几行，再打印历史行（终端自然上滚），最后重画。
    /// 不这么做的话，历史行会覆盖在输入行上。
    pub fn emit(&mut self, text: &str) {
        use std::io::Write;
        let w = self.width();
        let mut out = std::io::stdout();
        self.clear_viewport(&mut out);
        for logical in text.split('\n') {
            for line in wrap_line(logical, w) {
                // raw mode 下 `\n` 不含回车，必须显式 `\r\n`。
                let _ = write!(out, "{line}\r\n");
            }
        }
        let _ = out.flush();
        self.drawn_rows = 0;
        self.redraw();
    }

    /// 光标移到 viewport 顶并清到屏幕底。
    ///
    /// `MoveUp(0)` 必须**跳过而不是下发**：ECMA-48 规定 CUU 的参数为 0
    /// 时按 1 处理，所以 `\x1b[0A` 会真的上移一行、把上一行历史吃掉。
    /// 实机在伪终端里抓到过这条序列。
    fn clear_viewport(&self, out: &mut impl std::io::Write) {
        use crossterm::{cursor, terminal, QueueableCommand};
        let _ = out.queue(cursor::MoveToColumn(0));
        let up = viewport_move_up(self.drawn_rows);
        if up > 0 {
            let _ = out.queue(cursor::MoveUp(up));
        }
        let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
    }

    /// 重画 viewport（状态行 + 输入行），并把光标停在输入位置。
    pub fn redraw(&mut self) {
        use crossterm::{cursor, QueueableCommand};
        use std::io::Write;
        let w = self.width();
        let mut out = std::io::stdout();
        self.clear_viewport(&mut out);

        let mut rows: u16 = 0;
        if !self.status.is_empty() {
            // 状态行截断而非折行：它是易失信息，占满两行会挤掉历史。
            let (vis, _) = visible_window(&self.status, 0, w);
            let _ = write!(out, "\x1b[2m{vis}\x1b[0m\r\n");
            rows += 1;
        }
        // 输入行：提示符 + 可见窗口。
        let pw = display_width(&self.prompt);
        let avail = w.saturating_sub(pw).max(1);
        let (vis, col) = visible_window(&self.buf.text(), self.buf.cursor(), avail);
        let _ = write!(out, "{}{}", self.prompt, vis);
        rows += 1;

        // 光标回到输入列。
        let _ = out.queue(cursor::MoveToColumn((pw + col) as u16));
        let _ = out.flush();
        self.drawn_rows = rows;
    }

    /// 离开分离模式：恢复 raw mode，并把 viewport 留成一行干净输出。
    pub fn leave(&mut self) {
        use std::io::Write;
        if !self.raw {
            return;
        }
        let mut out = std::io::stdout();
        self.clear_viewport(&mut out);
        let _ = write!(out, "\r\n");
        let _ = out.flush();
        let _ = crossterm::execute!(out, crossterm::event::DisableBracketedPaste);
        let _ = crossterm::terminal::disable_raw_mode();
        self.raw = false;
    }
}

impl Drop for SplitScreen {
    fn drop(&mut self) {
        // panic / `?` 早退时也必须恢复 raw mode，否则用户的终端会卡在
        // 无回显状态（要盲敲 `reset`）。
        self.leave();
    }
}

/// 一次按键处理的结果。
#[derive(Debug, PartialEq, Eq)]
pub enum KeyOutcome {
    /// 缓冲变了，需要重绘。
    Redraw,
    /// 什么都没变（如在行首按左箭头），不必重绘。
    Ignored,
    /// 回车：提交这一行。
    Submit(String),
    /// Ctrl-C：放弃当前输入行（不退出）。
    Interrupt,
    /// Ctrl-D 且缓冲为空：EOF，退出会话。
    Eof,
    /// ↑：调出更旧的一条历史。
    HistoryPrev,
    /// ↓：调回更新的一条历史。
    HistoryNext,
}

/// 把一个 crossterm 按键事件应用到编辑缓冲。
///
/// 抽成纯函数（不碰终端）是为了可测——按键映射是最容易写错、又最难
/// 手工回归的部分（Ctrl-D 在空/非空缓冲下语义不同、Ctrl-C 不该退出等）。
pub fn apply_key(buf: &mut EditBuffer, key: crossterm::event::KeyEvent) -> KeyOutcome {
    use crossterm::event::{KeyCode, KeyModifiers};
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Enter => KeyOutcome::Submit(buf.take()),
        KeyCode::Backspace => {
            if buf.backspace() { KeyOutcome::Redraw } else { KeyOutcome::Ignored }
        }
        KeyCode::Delete => {
            if buf.delete() { KeyOutcome::Redraw } else { KeyOutcome::Ignored }
        }
        KeyCode::Left => if buf.left() { KeyOutcome::Redraw } else { KeyOutcome::Ignored },
        KeyCode::Right => if buf.right() { KeyOutcome::Redraw } else { KeyOutcome::Ignored },
        // ↑/↓ 不在这里改缓冲：历史状态归 `InputHistory`，由调用方套用，
        // 这样 apply_key 保持"只动缓冲"的纯粹性。
        KeyCode::Up => KeyOutcome::HistoryPrev,
        KeyCode::Down => KeyOutcome::HistoryNext,
        KeyCode::Home => {
            buf.home();
            KeyOutcome::Redraw
        }
        KeyCode::End => {
            buf.end();
            KeyOutcome::Redraw
        }
        // Ctrl-C：**只清当前行，不退出**。终端习惯如此，而且 chat 里
        // 一轮对话可能跑了几十分钟，误退出的代价远大于清行。
        KeyCode::Char('c') if ctrl => {
            buf.clear();
            KeyOutcome::Interrupt
        }
        // Ctrl-D：空行 = EOF 退出；非空 = 删光标右侧（同 readline）。
        KeyCode::Char('d') if ctrl => {
            if buf.is_empty() {
                KeyOutcome::Eof
            } else if buf.delete() {
                KeyOutcome::Redraw
            } else {
                KeyOutcome::Ignored
            }
        }
        KeyCode::Char('u') if ctrl => {
            buf.clear();
            KeyOutcome::Redraw
        }
        KeyCode::Char('w') if ctrl => {
            if buf.delete_word_left() { KeyOutcome::Redraw } else { KeyOutcome::Ignored }
        }
        KeyCode::Char('a') if ctrl => {
            buf.home();
            KeyOutcome::Redraw
        }
        KeyCode::Char('e') if ctrl => {
            buf.end();
            KeyOutcome::Redraw
        }
        // 普通字符。带 CONTROL 的其余组合一律忽略——落进缓冲会变成
        // 不可见控制字符，用户看不见却发给了模型。
        KeyCode::Char(c) if !ctrl => {
            buf.insert(c);
            KeyOutcome::Redraw
        }
        _ => KeyOutcome::Ignored,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_buffer_is_char_indexed_not_byte_indexed() {
        // 按字节索引会把多字节字符切碎——任务描述基本都是中文。
        let mut b = EditBuffer::new();
        b.insert_str("你好世界");
        assert_eq!(b.cursor(), 4, "光标按字符计数，不是 12 字节");
        assert!(b.backspace());
        assert_eq!(b.text(), "你好世");
        b.left();
        b.left();
        assert_eq!(b.cursor(), 1);
        b.insert('X');
        assert_eq!(b.text(), "你X好世");
    }

    #[test]
    fn edit_buffer_boundaries_do_not_panic() {
        let mut b = EditBuffer::new();
        // 空缓冲上的所有操作都必须安全返回 false，不能下标越界。
        assert!(!b.backspace());
        assert!(!b.delete());
        assert!(!b.left());
        assert!(!b.right());
        assert!(!b.delete_word_left());
        b.home();
        b.end();
        assert_eq!(b.text(), "");
        // 光标在末尾时 delete 无事发生；在开头时 backspace 无事发生。
        b.insert_str("ab");
        b.end();
        assert!(!b.delete());
        b.home();
        assert!(!b.backspace());
        assert!(b.delete());
        assert_eq!(b.text(), "b");
    }

    #[test]
    fn delete_word_left_eats_trailing_space_then_word() {
        let mut b = EditBuffer::new();
        b.insert_str("读一下 src/main.rs   ");
        assert!(b.delete_word_left());
        assert_eq!(b.text(), "读一下 ", "先吃掉尾部空白，再吃掉一个词");
        assert!(b.delete_word_left());
        assert_eq!(b.text(), "");
    }

    #[test]
    fn take_returns_and_clears() {
        let mut b = EditBuffer::new();
        b.insert_str("hello");
        assert_eq!(b.take(), "hello");
        assert!(b.is_empty());
        assert_eq!(b.cursor(), 0);
    }

    #[test]
    fn cjk_is_two_columns_ascii_is_one() {
        assert_eq!(char_width('a'), 1);
        assert_eq!(char_width('中'), 2);
        assert_eq!(char_width('，'), 2, "全角标点");
        assert_eq!(display_width("ab中"), 4);
        // 控制字符按 0 列，不影响光标定位。
        assert_eq!(char_width('\u{7}'), 0);
    }

    #[test]
    fn wrap_never_splits_a_wide_char_across_lines() {
        // 宽度 3 放不下两个中文（4 列），第二个必须整体挪到下一行。
        let out = wrap_line("中文", 3);
        assert_eq!(out, vec!["中", "文"]);
        // 恰好放得下时不折。
        assert_eq!(wrap_line("中文", 4), vec!["中文"]);
        // ASCII 常规折行。
        assert_eq!(wrap_line("abcde", 2), vec!["ab", "cd", "e"]);
    }

    #[test]
    fn wrap_degenerate_inputs_are_safe() {
        // width=0 不能除零 / 死循环。
        assert_eq!(wrap_line("abc", 0), vec!["abc"]);
        // 空串返回一行空串（否则调用方少画一行、viewport 高度算错）。
        assert_eq!(wrap_line("", 10), vec![""]);
    }

    #[test]
    fn visible_window_scrolls_to_keep_cursor_in_view() {
        // 短行：全显示，光标列 = 前缀宽度。
        let (vis, col) = visible_window("abc", 2, 10);
        assert_eq!((vis.as_str(), col), ("abc", 2));
        // 长行、光标在末尾：窗口右锚，能看到尾部。
        let (vis, col) = visible_window("abcdefghij", 10, 5);
        assert!(vis.ends_with('j'), "光标在末尾时必须看得到行尾: {vis}");
        assert!(col <= 5, "光标列不能超出窗口宽度: {col}");
        // 中文长行不切碎字符。
        let long: String = "中".repeat(20);
        let (vis, _) = visible_window(&long, 20, 5);
        assert!(vis.chars().all(|c| c == '中'));
        assert!(display_width(&vis) <= 5);
    }

    #[test]
    fn visible_window_zero_width_is_safe() {
        let (vis, col) = visible_window("abc", 1, 0);
        assert_eq!((vis.as_str(), col), ("", 0));
    }

    // ─── 按键映射 ────────────────────────────────────────────────

    fn key(c: char) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(c),
            crossterm::event::KeyModifiers::NONE,
        )
    }
    fn ctrl(c: char) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(c),
            crossterm::event::KeyModifiers::CONTROL,
        )
    }
    fn code(c: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(c, crossterm::event::KeyModifiers::NONE)
    }

    #[test]
    fn enter_submits_and_clears() {
        let mut b = EditBuffer::new();
        for c in "你好".chars() {
            apply_key(&mut b, key(c));
        }
        assert_eq!(
            apply_key(&mut b, code(crossterm::event::KeyCode::Enter)),
            KeyOutcome::Submit("你好".into())
        );
        assert!(b.is_empty(), "提交后缓冲必须清空");
    }

    #[test]
    fn ctrl_c_clears_the_line_but_does_not_exit() {
        // chat 里一轮对话可能几十分钟，Ctrl-C 误退出代价远大于清行。
        let mut b = EditBuffer::new();
        apply_key(&mut b, key('x'));
        assert_eq!(apply_key(&mut b, ctrl('c')), KeyOutcome::Interrupt);
        assert!(b.is_empty());
        assert_ne!(
            apply_key(&mut b, ctrl('c')),
            KeyOutcome::Eof,
            "Ctrl-C 永远不是 EOF"
        );
    }

    #[test]
    fn ctrl_d_is_eof_only_on_empty_line() {
        let mut b = EditBuffer::new();
        assert_eq!(apply_key(&mut b, ctrl('d')), KeyOutcome::Eof);
        // 非空时是「删光标右侧」（同 readline），不是退出。
        b.insert_str("ab");
        b.home();
        assert_eq!(apply_key(&mut b, ctrl('d')), KeyOutcome::Redraw);
        assert_eq!(b.text(), "b");
    }

    #[test]
    fn control_combos_never_leak_into_the_buffer() {
        // 落进缓冲会变成不可见控制字符：用户看不见，却发给了模型。
        let mut b = EditBuffer::new();
        for c in ['z', 'k', 'l', 'r'] {
            assert_eq!(apply_key(&mut b, ctrl(c)), KeyOutcome::Ignored);
        }
        assert!(b.is_empty(), "未映射的 Ctrl 组合不能进缓冲: {:?}", b.text());
    }

    #[test]
    fn editing_keys_are_wired() {
        let mut b = EditBuffer::new();
        b.insert_str("读一下 main.rs");
        assert_eq!(apply_key(&mut b, ctrl('w')), KeyOutcome::Redraw);
        assert_eq!(b.text(), "读一下 ");
        assert_eq!(apply_key(&mut b, ctrl('a')), KeyOutcome::Redraw);
        assert_eq!(b.cursor(), 0);
        assert_eq!(apply_key(&mut b, ctrl('e')), KeyOutcome::Redraw);
        assert_eq!(b.cursor(), 4);
        assert_eq!(apply_key(&mut b, ctrl('u')), KeyOutcome::Redraw);
        assert!(b.is_empty());
    }

    #[test]
    fn no_op_keys_report_ignored_so_we_skip_redraw() {
        // 每次按键都重绘会在长历史里造成可见闪烁；无变化必须报 Ignored。
        let mut b = EditBuffer::new();
        assert_eq!(
            apply_key(&mut b, code(crossterm::event::KeyCode::Left)),
            KeyOutcome::Ignored
        );
        assert_eq!(
            apply_key(&mut b, code(crossterm::event::KeyCode::Backspace)),
            KeyOutcome::Ignored
        );
        b.insert('a');
        assert_eq!(
            apply_key(&mut b, code(crossterm::event::KeyCode::Right)),
            KeyOutcome::Ignored,
            "光标已在末尾"
        );
    }

    // ─── 输入历史 ────────────────────────────────────────────────

    #[test]
    fn history_navigates_up_and_down() {
        let mut h = InputHistory::new();
        h.push("第一条");
        h.push("第二条");
        // 首次上翻取最新一条。
        assert_eq!(h.prev("正在写的").as_deref(), Some("第二条"));
        assert_eq!(h.prev("").as_deref(), Some("第一条"));
        // 到最旧一条后不再动（返回 None，调用方据此跳过重绘）。
        assert_eq!(h.prev(""), None);
        // 往下走回来。
        assert_eq!(h.next().as_deref(), Some("第二条"));
        // 走到底部时还原开始上翻前正在编辑的内容。
        assert_eq!(h.next().as_deref(), Some("正在写的"));
        // 已在底部，再往下没有动作。
        assert_eq!(h.next(), None);
    }

    #[test]
    fn history_skips_blank_and_consecutive_duplicates() {
        // 连按回车会把历史刷满，上翻要按十几次才越得过重复项。
        let mut h = InputHistory::new();
        h.push("");
        h.push("   ");
        h.push("\n");
        assert!(h.is_empty(), "空白行不该进历史");
        h.push("同一条");
        h.push("同一条");
        h.push("同一条");
        assert_eq!(h.len(), 1, "连续重复只记一条");
        h.push("另一条");
        h.push("同一条");
        assert_eq!(h.len(), 3, "非连续的重复要记（是真的又输入了一次）");
    }

    #[test]
    fn history_on_empty_store_is_a_no_op() {
        let mut h = InputHistory::new();
        assert_eq!(h.prev("正在写的"), None);
        assert_eq!(h.next(), None);
    }

    #[test]
    fn history_push_resets_navigation_cursor() {
        // 翻到一半直接提交后，游标必须回到底部——否则下次上翻会从
        // 上次停的位置继续，用户以为历史"乱跳"。
        let mut h = InputHistory::new();
        h.push("a");
        h.push("b");
        assert_eq!(h.prev("").as_deref(), Some("b"));
        assert_eq!(h.prev("").as_deref(), Some("a"));
        h.push("c");
        assert_eq!(h.prev("").as_deref(), Some("c"), "提交后应从最新一条重新开始");
    }

    #[test]
    fn arrow_keys_map_to_history_navigation() {
        let mut b = EditBuffer::new();
        assert_eq!(
            apply_key(&mut b, code(crossterm::event::KeyCode::Up)),
            KeyOutcome::HistoryPrev
        );
        assert_eq!(
            apply_key(&mut b, code(crossterm::event::KeyCode::Down)),
            KeyOutcome::HistoryNext
        );
        // ↑/↓ 不该动缓冲——历史状态归 InputHistory，由调用方套用。
        assert!(b.is_empty());
    }

    // ─── 粘贴规整 ────────────────────────────────────────────────

    #[test]
    fn pasted_multiline_becomes_one_input_line() {
        // 不折行的话，一段 5 行代码会被拆成 5 次提交，前 4 次都是残句。
        let src = "fn main() {\n    println!(\"hi\");\n}\n";
        assert_eq!(normalize_pasted(src), "fn main() { println!(\"hi\"); }");
    }

    #[test]
    fn pasted_text_collapses_whitespace_and_drops_control_chars() {
        // 缩进代码粘进来不该变成一长串空格。
        assert_eq!(normalize_pasted("a\n\n\n    b"), "a b");
        // 制表符折成空格：raw mode 下 \t 会跳到下一个制表位，
        // 让我们对光标列的计算与实际不符。
        assert_eq!(normalize_pasted("a\tb"), "a b");
        // 不可见控制字符直接丢，否则会进入发给模型的内容。
        assert_eq!(normalize_pasted("a\u{7}b\u{1b}c"), "abc");
        // 首尾空白裁掉。
        assert_eq!(normalize_pasted("  x  "), "x");
        // 全空白粘贴 → 空串，调用方据此跳过插入。
        assert_eq!(normalize_pasted("\n\t  \r\n"), "");
    }

    #[test]
    fn pasted_cjk_is_preserved(){
        assert_eq!(
            normalize_pasted("读一下\nsrc/main.rs"),
            "读一下 src/main.rs"
        );
    }

    #[test]
    fn degenerate_terminal_width_falls_back_not_clamps_to_one() {
        // pty.openpty() 不设 winsize 时终端报 0×0。钳到 1 会把每个字符
        // 折成一行——实机抓到过 `\x1b[36m` 被拆成 6 行。
        assert_eq!(sane_width(Some(0)), FALLBACK_WIDTH);
        assert_eq!(sane_width(None), FALLBACK_WIDTH);
        assert_eq!(sane_width(Some(1)), FALLBACK_WIDTH);
        assert_eq!(sane_width(Some(19)), FALLBACK_WIDTH, "低于阈值视为读取失败");
        // 真实窗口宽度照用。
        assert_eq!(sane_width(Some(20)), 20);
        assert_eq!(sane_width(Some(120)), 120);
    }

    #[test]
    fn display_width_skips_ansi_escapes() {
        // 提示符是带色的：把转义字节当可见字符算会把宽度高估约 30 列，
        // 导致光标错位（实测 `\x1b[60G` 而应在 ~25）、窄终端上输入内容
        // 完全渲染不出来（avail 被压到 1，宽字符放不进去）。
        assert_eq!(display_width("\u{1b}[1m\u{1b}[36mabc\u{1b}[0m"), 3);
        assert_eq!(display_width("\u{1b}[90m · \u{1b}[0m"), 3);
        // 真实提示符形态。
        let prompt = "\u{1b}[1m👔 \u{1b}[0m\u{1b}[95mmanager\u{1b}[0m\u{1b}[90m · \u{1b}[0m\u{1b}[96mglm-5.3\u{1b}[0m \u{1b}[1m\u{1b}[36m› \u{1b}[0m";
        // 👔(2)+空格(1)+manager(7)+" · "(3)+glm-5.3(7)+空格(1)+"› "(2) = 23
        assert_eq!(display_width(prompt), 23, "带色提示符的可见宽度");
        // OSC 序列（如设置窗口标题）同样不计宽度。
        assert_eq!(display_width("\u{1b}]0;title\u{7}x"), 1);
        // 纯文本行为不变。
        assert_eq!(display_width("ab中"), 4);
    }

    #[test]
    fn narrow_terminal_still_renders_wide_chars() {
        // 40 列终端 + 22 列提示符 → avail=18，应能显示 9 个中文。
        let prompt_w = 23usize;
        let avail = 40usize.saturating_sub(prompt_w).max(1);
        let text: String = "中".repeat(30);
        let (vis, col) = visible_window(&text, 30, avail);
        assert!(!vis.is_empty(), "窄终端下必须渲染出内容，实测曾为空");
        assert!(display_width(&vis) <= avail, "不能溢出可用宽度");
        assert!(col <= avail);
    }

    #[test]
    fn viewport_move_up_never_emits_zero() {
        // ECMA-48：CUU 参数为 0 时按 1 处理，所以 `\x1b[0A` 会真的上移
        // 一行、吃掉一行历史。实机在伪终端里抓到过这条序列。
        assert_eq!(viewport_move_up(0), 0, "没画过就不该移动");
        assert_eq!(viewport_move_up(1), 0, "只有输入行时光标已在顶，不能下发 MoveUp");
        assert_eq!(viewport_move_up(2), 1, "状态行 + 输入行 → 上移 1");
        assert_eq!(viewport_move_up(3), 2);
    }

    #[test]
    fn split_screen_is_disabled_when_not_a_tty() {
        // 这是现有 e2e 的安全网：它们都重定向 stdout，必须走纯文本路径。
        // 测试进程的 stdout 被 cargo 捕获，所以这里恒为 false。
        assert!(
            !split_screen_available(),
            "非 TTY 环境必须禁用分离模式，否则转义序列会污染被断言的输出"
        );
        assert!(
            SplitScreen::enter("› ").is_none(),
            "非 TTY 时 enter() 必须返回 None 让调用方降级"
        );
    }
}
