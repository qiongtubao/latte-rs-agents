//! ANSI styling helpers for the chat CLI.
//!
//! Keeps the chat REPL readable without pulling in a full TUI library.
//! All helpers are no-ops when stdout is not a terminal (so piped output
//! stays clean for scripts).

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Reset all attributes.
pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
// ANSI 调色板：成套定义，按需取用；缺哪个都会让后续配色不一致。
#[allow(dead_code)]
pub const DIM: &str = "\x1b[2m";
pub const ITALIC: &str = "\x1b[3m";
pub const UNDERLINE: &str = "\x1b[4m";

// Foreground colors.
pub const FG_RED: &str = "\x1b[31m";
pub const FG_GREEN: &str = "\x1b[32m";
pub const FG_YELLOW: &str = "\x1b[33m";
pub const FG_BLUE: &str = "\x1b[34m";
pub const FG_MAGENTA: &str = "\x1b[35m";
pub const FG_CYAN: &str = "\x1b[36m";
pub const FG_WHITE: &str = "\x1b[37m";
pub const FG_BRIGHT_BLACK: &str = "\x1b[90m";
pub const FG_BRIGHT_CYAN: &str = "\x1b[96m";
pub const FG_BRIGHT_MAGENTA: &str = "\x1b[95m";

/// Box-drawing characters (rounded).
pub const BOX_TL: &str = "╭";
pub const BOX_TR: &str = "╮";
pub const BOX_BL: &str = "╰";
pub const BOX_BR: &str = "╯";
pub const BOX_H: &str = "─";
pub const BOX_V: &str = "│";

/// Braille-dot spinner frames (10-cycle).
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Global counter so multiple concurrent spinners don't all show the same frame.
static SPINNER_TICK: AtomicU64 = AtomicU64::new(0);

/// True when stdout is a TTY. Used to gate ANSI escapes.
pub fn is_tty() -> bool {
    std::io::stdout().is_terminal()
}

/// Colorize `text` with `code` only when stdout is a TTY.
pub fn paint(code: &str, text: &str) -> String {
    if is_tty() {
        format!("{code}{text}{RESET}")
    } else {
        text.to_string()
    }
}

/// Bold + colorize.
pub fn paint_bold(code: &str, text: &str) -> String {
    if is_tty() {
        format!("{BOLD}{code}{text}{RESET}")
    } else {
        text.to_string()
    }
}

/// Render the chat prompt: `👔 manager · deepseek-v4-flash › `.
pub fn render_prompt(role_icon: &str, role_id: &str, model_id: &str) -> String {
    let role = paint_bold(FG_BRIGHT_MAGENTA, role_id);
    let model = paint(FG_BRIGHT_CYAN, model_id);
    let sep = paint(FG_BRIGHT_BLACK, " · ");
    let arrow = paint_bold(FG_CYAN, "› ");
    format!("{role_icon} {role}{sep}{model} {arrow}")
}

/// Render a single box-drawing border line for the response box.
fn render_hline(width: usize) -> String {
    let inner = BOX_H.repeat(width.saturating_sub(2));
    format!("{BOX_TL}{inner}{BOX_TR}")
}

/// Render the closing line of a response box.
fn render_bottom_line(width: usize) -> String {
    let inner = BOX_H.repeat(width.saturating_sub(2));
    format!("{BOX_BL}{inner}{BOX_BR}")
}

/// Wrap a response string in a rounded box with a colored left border.
/// The first line gets a small accent marker (e.g. "👔 manager") so
/// the reader can tell which role produced the message.
pub fn render_response_box(role_icon: &str, role_id: &str, body: &str, width: usize) -> String {
    if !is_tty() {
        return body.to_string();
    }
    let w = width.clamp(40, 160);
    let accent = format!("{role_icon} {role_id}");
    let accent = paint_bold(FG_BRIGHT_MAGENTA, &accent);
    let top = paint_bold(FG_BRIGHT_BLACK, &render_hline(w));
    let bottom = paint_bold(FG_BRIGHT_BLACK, &render_bottom_line(w));
    let border = paint_bold(FG_BRIGHT_BLACK, BOX_V);
    let indent = " "; // 1-space indent inside the box
    let inner_w = w.saturating_sub(4); // border + indent + indent
    let mut out = String::new();
    out.push_str(&format!("{accent}\n"));
    out.push_str(&top);
    out.push('\n');
    for line in body.lines() {
        for wrapped in wrap_line(line, inner_w) {
            out.push_str(&border);
            out.push_str(indent);
            out.push_str(&wrapped);
            out.push('\n');
        }
    }
    out.push_str(&bottom);
    out
}

/// Naive word-wrap: break on whitespace, fall back to hard-wrap on long
/// tokens. Respects ANSI escape codes (won't break mid-sequence).
fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![line.to_string()];
    }
    if line.is_empty() {
        return vec![String::new()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    for token in split_tokens(line) {
        let token_width = visible_width(&token);
        if current.is_empty() {
            current.push_str(&token);
            current_width = token_width;
        } else if current_width + 1 + token_width <= width {
            current.push(' ');
            current.push_str(&token);
            current_width += 1 + token_width;
        } else {
            out.push(std::mem::take(&mut current));
            current.push_str(&token);
            current_width = token_width;
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Split a line into tokens, preserving ANSI escape sequences as their own
/// zero-width tokens so wrapping doesn't break them.
fn split_tokens(line: &str) -> Vec<String> {
    // Token = either an ANSI escape sequence ([..m) or a maximal
    // run of non-whitespace, non-escape chars. Whitespace is a
    // word boundary: it gets dropped (wrap_line inserts the join
    // space) and the current word token is flushed first. This
    // is what `wrap_line` needs to actually wrap — without the
    // whitespace split, the entire line would be one giant token
    // and width>line-width would never trigger a break.
    let mut tokens = Vec::new();
    let mut buf = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if !buf.is_empty() {
                tokens.push(std::mem::take(&mut buf));
            }
            buf.push(c);
            for nc in chars.by_ref() {
                buf.push(nc);
                if nc == 'm' {
                    break;
                }
            }
            tokens.push(std::mem::take(&mut buf));
        } else if c.is_whitespace() {
            if !buf.is_empty() {
                tokens.push(std::mem::take(&mut buf));
            }
            // whitespace itself is dropped — wrap_line joins
            // adjacent tokens with a single space.
        } else {
            buf.push(c);
        }
    }
    if !buf.is_empty() {
        tokens.push(buf);
    }
    tokens
}

/// Visible (display) width of a string, ignoring ANSI escapes.
fn visible_width(s: &str) -> usize {
    let mut w = 0;
    let mut in_escape = false;
    for c in s.chars() {
        if c == '\x1b' {
            in_escape = true;
            continue;
        }
        if in_escape {
            if c == 'm' {
                in_escape = false;
            }
            continue;
        }
        w += if is_wide(c) { 2 } else { 1 };
    }
    w
}

fn is_wide(c: char) -> bool {
    let cp = c as u32;
    (0x1100..=0x115F).contains(&cp)
        || (0x2E80..=0x303E).contains(&cp)
        || (0x3041..=0x33FF).contains(&cp)
        || (0x3400..=0x4DBF).contains(&cp)
        || (0x4E00..=0x9FFF).contains(&cp)
        || (0xA000..=0xA4CF).contains(&cp)
        || (0xAC00..=0xD7A3).contains(&cp)
        || (0xF900..=0xFAFF).contains(&cp)
        || (0xFE30..=0xFE4F).contains(&cp)
        || (0xFF00..=0xFF60).contains(&cp)
        || (0xFFE0..=0xFFE6).contains(&cp)
}

/// Render the per-turn token status line.
pub fn render_status_line(model_id: &str, in_delta: u32, out_delta: u32, total_in: u32, total_out: u32) -> String {
    let model = paint(FG_BRIGHT_CYAN, model_id);
    let up = paint(FG_GREEN, "↑");
    let down = paint(FG_YELLOW, "↓");
    let sep = paint(FG_BRIGHT_BLACK, " · ");
    let total = paint(FG_BRIGHT_BLACK, "session total");
    format!(
        "[{model}] {up}{in_delta} in {down}{out_delta} out {sep}{total} {up}{total_in} {down}{total_out}"
    )
}

/// One-shot spinner for "thinking…" feedback.
pub struct Spinner {
    label: String,
    stop_flag: std::sync::Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    started: Instant,
}

impl Spinner {
    /// Start a spinner with the given label (e.g. "thinking…").
    /// No-op when stdout is not a TTY.
    pub fn start(label: &str) -> Self {
        let started = Instant::now();
        if !is_tty() {
            return Self {
                label: label.to_string(),
                stop_flag: std::sync::Arc::new(AtomicBool::new(false)),
                handle: None,
                started,
            };
        }
        let stop_flag = std::sync::Arc::new(AtomicBool::new(false));
        let stop_flag_clone = std::sync::Arc::clone(&stop_flag);
        let label_clone = label.to_string();
        let handle = std::thread::spawn(move || {
            let mut stderr = std::io::stderr();
            let mut i: usize = 0;
            while !stop_flag_clone.load(Ordering::Relaxed) {
                let frame = SPINNER[i % SPINNER.len()];
                let painted_frame = paint_bold(FG_CYAN, frame);
                let label_painted = paint(FG_BRIGHT_BLACK, &label_clone);
                let _ = write!(stderr, "\r\x1b[K{painted_frame} {label_painted}");
                let _ = stderr.flush();
                i = i.wrapping_add(1);
                std::thread::sleep(Duration::from_millis(80));
            }
            let _ = write!(stderr, "\r\x1b[K");
            let _ = stderr.flush();
        });
        Self {
            label: label.to_string(),
            stop_flag,
            handle: Some(handle),
            started,
        }
    }

    /// Stop the spinner and clear its line. Consumes self so it can't
    /// be called twice.
    pub fn stop(mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }

    /// Elapsed time since the spinner started.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

/// Pick a spinner frame for one-off use.
pub fn spinner_frame() -> &'static str {
    let i = SPINNER_TICK.fetch_add(1, Ordering::Relaxed) as usize;
    SPINNER[i % SPINNER.len()]
}

/// Render the inline error block: a red box with the error text.
pub fn render_error_block(title: &str, body: &str, width: usize) -> String {
    if !is_tty() {
        return format!("error: {title}\n{body}\n");
    }
    let w = width.clamp(40, 160);
    let title_str = format!("error: {title}");
    let title_painted = paint_bold(FG_RED, &title_str);
    let top = paint_bold(FG_RED, &render_hline(w));
    let bottom = paint_bold(FG_RED, &render_bottom_line(w));
    let border = paint_bold(FG_RED, BOX_V);
    let indent = " ";
    let inner_w = w.saturating_sub(4);
    let mut out = String::new();
    out.push_str(&format!("{title_painted}\n"));
    out.push_str(&top);
    out.push('\n');
    for line in body.lines() {
        for wrapped in wrap_line(line, inner_w) {
            out.push_str(&border);
            out.push_str(indent);
            out.push_str(&wrapped);
            out.push('\n');
        }
    }
    out.push_str(&bottom);
    out
}

/// Get the terminal width; falls back to 100 if not a TTY.
pub fn terminal_width() -> usize {
    if !is_tty() {
        return 100;
    }
    std::env::var("COLUMNS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n >= 40)
        .unwrap_or(100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_short_line_passes_through() {
        let lines = wrap_line("hello world", 80);
        assert_eq!(lines, vec!["hello world".to_string()]);
    }

    #[test]
    fn wrap_breaks_long_line() {
        let lines = wrap_line("aa bb cc dd ee ff gg hh ii jj", 10);
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|l| visible_width(l) <= 10));
    }

    #[test]
    fn wrap_preserves_ansi_sequences() {
        let input = format!("{BOLD}hello{RESET} world this is a long line");
        let lines = wrap_line(&input, 10);
        // ANSI sequences should not be split mid-escape.
        for line in &lines {
            assert!(!line.contains("\x1b[1") || line.ends_with("\x1b[0m") || line.contains("\x1b[0m"));
        }
    }

    #[test]
    fn visible_width_skips_ansi() {
        let s = format!("{BOLD}hi{RESET}");
        assert_eq!(visible_width(&s), 2);
    }

    #[test]
    fn prompt_renders_without_tty() {
        // is_tty() returns false in test process, so no ANSI codes.
        let p = render_prompt("👔", "manager", "deepseek-v4-flash");
        assert!(p.contains("manager"));
        assert!(p.contains("deepseek-v4-flash"));
        assert!(p.contains("›"));
    }

    #[test]
    fn status_line_includes_totals() {
        let s = render_status_line("deepseek-v4-flash", 100, 50, 1000, 500);
        assert!(s.contains("100 in"));
        assert!(s.contains("50 out"));
        assert!(s.contains("1000"));
        assert!(s.contains("500"));
    }
}
