//! Per-session chat log file.
//!
//! Writes a structured event log for each `latte-agent chat` invocation to
//! `~/.latte/logs/chat-YYYYMMDD-HHMMSS-<pid>.log` so the operator can
//! inspect what models were tried, what failed, and what the assistant
//! actually returned. The log is also echoed to stderr at the `info`
//! level via the `tracing` facade, so it shows up alongside the normal
//! REPL output.
//!
//! The on-disk format is line-oriented JSONL-ish: each line starts with
//! `[<iso8601-utc>] [<level>] <message>` followed by an optional
//! key=value payload. Plain text + structured fields, no external
//! dependency on a logging framework.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// A session-scoped log file. The `Drop` impl flushes any pending
/// writes; the mutex serializes concurrent writes from the REPL loop
/// and any background tasks.
pub struct ChatLog {
    file: Option<Mutex<std::fs::File>>,
    path: Option<PathBuf>,
}

impl ChatLog {
    /// Open a new log file in `~/.latte/logs/`. Returns an inert
    /// (no-op) log if the directory can't be created — chat
    /// shouldn't fail just because logging is unavailable.
    pub fn open() -> Self {
        let dir = match global_log_dir() {
            Some(d) => d,
            None => return Self { file: None, path: None },
        };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("chatlog: cannot create {}: {}", dir.display(), e);
            return Self { file: None, path: None };
        }
        let name = format!(
            "chat-{}-{}.log",
            iso_local_date_time(),
            std::process::id()
        );
        let path = dir.join(name);
        let file = match std::fs::File::create(&path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("chatlog: cannot create {}: {}", path.display(), e);
                return Self { file: None, path: None };
            }
        };
        Self {
            file: Some(Mutex::new(file)),
            path: Some(path),
        }
    }

    pub fn path(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }

    /// Log a structured event. `fields` is appended as `key=value`
    /// pairs (values are quoted when they contain whitespace).
    pub fn event(&self, level: &str, msg: &str, fields: &[(&str, String)]) {
        let stamp = iso_utc();
        let mut line = format!("[{}] [{}] {}", stamp, level, msg);
        for (k, v) in fields {
            let v_quoted = if v.contains(char::is_whitespace) {
                format!("\"{}\"", v.replace('"', "\\\""))
            } else {
                v.clone()
            };
            line.push(' ');
            line.push_str(k);
            line.push('=');
            line.push_str(&v_quoted);
        }
        line.push('\n');
        self.write(&line);
    }

    pub fn info(&self, msg: &str, fields: &[(&str, String)]) {
        self.event("info", msg, fields);
    }

    pub fn warn(&self, msg: &str, fields: &[(&str, String)]) {
        self.event("warn", msg, fields);
    }

    pub fn error(&self, msg: &str, fields: &[(&str, String)]) {
        self.event("error", msg, fields);
    }

    fn write(&self, line: &str) {
        if let Some(file) = &self.file {
            if let Ok(mut f) = file.lock() {
                let _ = f.write_all(line.as_bytes());
                let _ = f.flush();
            }
        }
        // Echo to stderr so the operator sees the same line on the
        // terminal — important when running interactively.
        eprint!("{}", line);
    }
}

fn global_log_dir() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("LATTE_LOG_DIR") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".latte").join("logs"))
}

fn iso_utc() -> String {
    // We don't pull in chrono; this format is good enough for log
    // grepping and is sortable lexicographically.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let (year, month, day, hour, min, sec) = epoch_to_ymdhms(secs);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hour, min, sec
    )
}

fn iso_local_date_time() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let (year, month, day, hour, min, sec) = epoch_to_ymdhms(secs);
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        year, month, day, hour, min, sec
    )
}

/// Convert a Unix-epoch second count to a `(year, month, day, h, m, s)`
/// tuple in UTC. Implements a tiny proleptic-Gregorian calendar;
/// correct for every timestamp we care about.
fn epoch_to_ymdhms(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let sec = (secs % 60) as u32;
    let mins = (secs / 60) as u32;
    let min = mins % 60;
    let hours = mins / 60;
    let hour = hours % 24;
    let mut days = (hours / 24) as i64;
    // 1970-01-01 is a Thursday (epoch day 0 = Thursday).
    let mut year = 1970i64;
    loop {
        let leap = is_leap(year);
        let dy = if leap { 366 } else { 365 };
        if days >= dy {
            days -= dy;
            year += 1;
        } else {
            break;
        }
    }
    let month_lens = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 0usize;
    while month < 12 {
        let ml = if month == 1 && is_leap(year) { 29 } else { month_lens[month] };
        if days >= ml {
            days -= ml;
            month += 1;
        } else {
            break;
        }
    }
    (year as u32, month as u32 + 1, days as u32 + 1, hour, min, sec)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}
