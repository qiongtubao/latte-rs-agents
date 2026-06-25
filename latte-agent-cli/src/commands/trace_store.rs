//! Read-only access to `~/.latte/{traces,sessions,logs}/` for the
//! `latte-agent debug` subcommand set. All paths are overridable via
//! the `LATTE_HOME` env var (used by tests) and fall back to
//! `~/.latte/` otherwise.
//!
//! Three sibling files share one session id (e.g.
//! `chat-20260625-054204-73546`):
//!
//!   - `~/.latte/logs/<id>.log`        — human chat log, always written
//!   - `~/.latte/sessions/<id>.idx`    — metadata-only index, always written
//!   - `~/.latte/traces/<id>.jsonl`    — full trace, only with `--debug`
//!
//! This module is the only place that knows the layout; the `debug`
//! subcommand set calls into it to list / load / aggregate.

use std::path::{Path, PathBuf};

use latte_agent_core::trace::{IndexLine, TraceEvent};

/// Per-session summary derived from one or more of the three sibling
/// files. `size_bytes` is the on-disk total across whichever files
/// exist (trace + index + log).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub has_log: bool,
    pub has_index: bool,
    pub has_trace: bool,
    pub size_bytes: u64,
}

/// Resolve the latte root directory. Returns `None` if no home is
/// discoverable (no `HOME` and no `LATTE_HOME`).
pub fn latte_home() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("LATTE_HOME") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    std::env::var_os("HOME").map(PathBuf::from).map(|h| h.join(".latte"))
}

pub fn logs_dir() -> Option<PathBuf> { latte_home().map(|h| h.join("logs")) }
pub fn sessions_dir() -> Option<PathBuf> { latte_home().map(|h| h.join("sessions")) }
pub fn traces_dir() -> Option<PathBuf> { latte_home().map(|h| h.join("traces")) }

/// Sanity-check that `id` is a plausible session id. Lenient by
/// design: rejects path traversal (`..`, `/`) and empty strings;
/// accepts anything that starts with an alphanumeric char.
pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && !id.contains('/')
        && !id.contains("..")
        && id.chars().next().map_or(false, |c| c.is_ascii_alphanumeric())
}

/// List every session seen across `logs/`, `sessions/`, and
/// `traces/`, merging the per-file flags into one record per id.
/// Returns an empty vector if the latte home doesn't exist yet
/// (e.g. right after a fresh install).
pub fn list_sessions() -> Vec<SessionSummary> {
    use std::collections::BTreeMap;

    let mut by_id: BTreeMap<String, SessionSummary> = BTreeMap::new();

    let mut mark = |dir: Option<PathBuf>, ext: &str, set: fn(&mut SessionSummary, bool)| {
        let Some(d) = dir else { return };
        let Ok(rd) = std::fs::read_dir(&d) else { return };
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.is_file() { continue; }
            let Some(this_ext) = path.extension().and_then(|s| s.to_str()) else { continue };
            if this_ext != ext { continue; }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
            let id = stem.to_string();
            let summary = by_id.entry(id.clone()).or_insert_with(|| SessionSummary {
                id, has_log: false, has_index: false, has_trace: false, size_bytes: 0,
            });
            set(summary, true);
            if let Ok(meta) = std::fs::metadata(&path) {
                summary.size_bytes += meta.len();
            }
        }
    };

    mark(logs_dir(),     "log",   |s, b| s.has_log = b);
    mark(sessions_dir(), "idx",   |s, b| s.has_index = b);
    mark(traces_dir(),   "jsonl", |s, b| s.has_trace = b);

    by_id.into_values().collect()
}

/// Path to a session's full-trace file (`~/.latte/traces/<id>.jsonl`).
pub fn trace_path(id: &str) -> Option<PathBuf> {
    traces_dir().map(|d| d.join(format!("{}.jsonl", id)))
}

/// Path to a session's index file (`~/.latte/sessions/<id>.idx`).
pub fn index_path(id: &str) -> Option<PathBuf> {
    sessions_dir().map(|d| d.join(format!("{}.idx", id)))
}

/// Path to a session's chat-log file (`~/.latte/logs/<id>.log`).
pub fn log_path(id: &str) -> Option<PathBuf> {
    logs_dir().map(|d| d.join(format!("{}.log", id)))
}

/// Errors from `load_*` and friends. `NotFound` is the typical case
/// when the user typed the wrong id.
#[derive(Debug)]
pub enum TraceStoreError {
    NotFound(PathBuf),
    Io(PathBuf, std::io::Error),
    Parse(PathBuf, serde_json::Error, usize),
}

impl std::fmt::Display for TraceStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TraceStoreError::NotFound(p) => write!(f, "not found: {}", p.display()),
            TraceStoreError::Io(p, e) => write!(f, "io error on {}: {}", p.display(), e),
            TraceStoreError::Parse(p, e, line) => write!(f, "parse error on {} line {}: {}", p.display(), line, e),
        }
    }
}

impl std::error::Error for TraceStoreError {}

/// Load every `TraceEvent` from a session's full-trace file. Missing
/// file → `NotFound`; bad JSON → `Parse(path, err, line_no)`.
pub fn load_trace(id: &str) -> Result<Vec<TraceEvent>, TraceStoreError> {
    let path = trace_path(id).ok_or_else(|| {
        TraceStoreError::NotFound(PathBuf::from(format!("<no traces dir for id '{}'>", id)))
    })?;
    if !path.exists() {
        return Err(TraceStoreError::NotFound(path));
    }
    let content = std::fs::read_to_string(&path).map_err(|e| TraceStoreError::Io(path.clone(), e))?;
    let mut events = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        if line.trim().is_empty() { continue; }
        let ev: TraceEvent = serde_json::from_str(line)
            .map_err(|e| TraceStoreError::Parse(path.clone(), e, idx + 1))?;
        events.push(ev);
    }
    Ok(events)
}

/// Load every `IndexLine` from a session's index file. Same error
/// semantics as `load_trace`.
pub fn load_index(id: &str) -> Result<Vec<IndexLine>, TraceStoreError> {
    let path = index_path(id).ok_or_else(|| {
        TraceStoreError::NotFound(PathBuf::from(format!("<no sessions dir for id '{}'>", id)))
    })?;
    if !path.exists() {
        return Err(TraceStoreError::NotFound(path));
    }
    let content = std::fs::read_to_string(&path).map_err(|e| TraceStoreError::Io(path.clone(), e))?;
    let mut lines = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        if line.trim().is_empty() { continue; }
        let l: IndexLine = serde_json::from_str(line)
            .map_err(|e| TraceStoreError::Parse(path.clone(), e, idx + 1))?;
        lines.push(l);
    }
    Ok(lines)
}

/// Token aggregate for the `debug tokens` view. `events` is a full
/// trace (from `load_trace`); we walk the events and pick out the
/// `TurnEnd` totals so the per-turn breakdown matches the on-disk
/// JSONL exactly.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TokenAggregate {
    pub total_input: u32,
    pub total_output: u32,
    pub total_thinking: u32,
    /// One entry per `(turn, role)`. Order preserved (BTreeMap).
    pub per_turn: Vec<TokenRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenRow {
    pub turn: u32,
    pub role: String,
    pub input: u32,
    pub output: u32,
    pub thinking: u32,
}

pub fn aggregate_tokens(events: &[TraceEvent]) -> TokenAggregate {
    use std::collections::BTreeMap;
    let mut per: BTreeMap<(u32, String), (u32, u32, u32)> = BTreeMap::new();
    let mut total = TokenAggregate::default();
    for ev in events {
        if let TraceEvent::TurnEnd { meta, total_input, total_output, total_thinking, .. } = ev {
            total.total_input += total_input;
            total.total_output += total_output;
            total.total_thinking += total_thinking;
            let key = (meta.turn, meta.role.clone());
            let entry = per.entry(key).or_insert((0, 0, 0));
            entry.0 += total_input;
            entry.1 += total_output;
            entry.2 += total_thinking;
        }
    }
    for ((turn, role), (input, output, thinking)) in per {
        total.per_turn.push(TokenRow { turn, role, input, output, thinking });
    }
    total
}

/// Filter events by `kind` (e.g. "ToolExec", "HookFired"). Used by
/// `debug session <id> --kind <k>`. Returns all events when `kind`
/// is `None`.
pub fn filter_by_kind(events: Vec<TraceEvent>, kind: Option<&str>) -> Vec<TraceEvent> {
    match kind {
        None => events,
        Some(k) => events.into_iter().filter(|e| e.variant_name() == k).collect(),
    }
}

/// Format a list of events as a one-line-per-event timeline. Each
/// line: `ts  turn=N  role=X  Kind  body`. The `body` is whatever
/// `TraceEvent::body_for_pretty` produces.
pub fn format_timeline(events: &[TraceEvent]) -> String {
    let mut out = String::new();
    for ev in events {
        out.push_str(&format!(
            "{ts}  turn={turn:<3}  role={role:<10}  {kind:<14}  {body}\n",
            ts = ev.meta().ts,
            turn = ev.meta().turn,
            role = ev.meta().role,
            kind = ev.variant_name(),
            body = ev.body_for_pretty(),
        ));
    }
    out
}

/// Resolve a session id from a chatlog's file path. The path looks
/// like `~/.latte/logs/chat-20260625-054204-73546.log`; we strip the
/// directory and `.log` suffix to recover the id. Returns `None`
/// if the path has no usable stem.
pub fn session_id_from_log_path(path: &Path) -> Option<String> {
    path.file_stem().and_then(|s| s.to_str()).map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_sessions_returns_empty_when_dirs_missing() {
        // Force a non-existent home for the test.
        let prev = std::env::var("LATTE_HOME").ok();
        std::env::set_var("LATTE_HOME", "/nonexistent/path/latte-test");
        let result = list_sessions();
        assert!(result.is_empty(), "expected empty list, got {:?}", result);
        // restore env
        match prev {
            Some(v) => std::env::set_var("LATTE_HOME", v),
            None => std::env::remove_var("LATTE_HOME"),
        }
    }

    #[test]
    fn list_sessions_finds_files_across_all_three_dirs() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("latte-trace-store-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let logs = dir.join("logs");
        let sessions = dir.join("sessions");
        let traces = dir.join("traces");
        fs::create_dir_all(&logs).unwrap();
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(&traces).unwrap();
        fs::write(logs.join("chat-A.log"), "log line\n").unwrap();
        fs::write(sessions.join("chat-A.idx"), "{\"turn\":0}\n").unwrap();
        fs::write(traces.join("chat-A.jsonl"), "{}\n").unwrap();
        // chat-B has only a log + trace (no index yet — common mid-session)
        fs::write(logs.join("chat-B.log"), "log line\n").unwrap();
        fs::write(traces.join("chat-B.jsonl"), "{}\n").unwrap();
        // chat-C is index-only (legacy migrated session)
        fs::write(sessions.join("chat-C.idx"), "{\"turn\":0}\n").unwrap();

        let prev = std::env::var("LATTE_HOME").ok();
        std::env::set_var("LATTE_HOME", &dir);
        let result = list_sessions();
        // restore env early so a panic in assertion still cleans up
        match prev.clone() {
            Some(v) => std::env::set_var("LATTE_HOME", v),
            None => std::env::remove_var("LATTE_HOME"),
        }

        assert_eq!(result.len(), 3, "expected 3 sessions, got {:?}", result);
        let by_id: std::collections::HashMap<String, SessionSummary> =
            result.into_iter().map(|s| (s.id.clone(), s)).collect();
        let a = by_id.get("chat-A").expect("chat-A missing");
        assert!(a.has_log && a.has_index && a.has_trace);
        let b = by_id.get("chat-B").expect("chat-B missing");
        assert!(b.has_log && b.has_trace && !b.has_index);
        let c = by_id.get("chat-C").expect("chat-C missing");
        assert!(!c.has_log && c.has_index && !c.has_trace);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_valid_id_rejects_traversal_and_empty() {
        assert!(is_valid_id("chat-20260625-054204-73546"));
        assert!(is_valid_id("discuss-20260625-x"));
        assert!(!is_valid_id(""));
        assert!(!is_valid_id("../etc/passwd"));
        assert!(!is_valid_id("foo/bar"));
        assert!(!is_valid_id("/abs"));
    }

    #[test]
    fn session_id_from_log_path_strips_dir_and_suffix() {
        let p = PathBuf::from("/home/u/.latte/logs/chat-20260625-054204-73546.log");
        assert_eq!(session_id_from_log_path(&p).as_deref(), Some("chat-20260625-054204-73546"));
    }

    #[test]
    fn load_trace_missing_file_returns_not_found() {
        let prev = std::env::var("LATTE_HOME").ok();
        std::env::set_var("LATTE_HOME", "/nonexistent/path/latte-test");
        let err = load_trace("chat-missing").unwrap_err();
        match err {
            TraceStoreError::NotFound(_) => {} // expected
            other => panic!("expected NotFound, got {:?}", other),
        }
        match prev {
            Some(v) => std::env::set_var("LATTE_HOME", v),
            None => std::env::remove_var("LATTE_HOME"),
        }
    }

    #[test]
    fn aggregate_tokens_sums_turn_end_rows() {
        use latte_agent_core::trace::{ToolStatus, TraceEvent, TraceMeta};
        let events = vec![
            TraceEvent::TurnEnd {
                meta: TraceMeta { turn: 0, role: "manager".into(), ts: "t".into(), session_id: "s".into() },
                total_input: 100, total_output: 200, total_thinking: 5, elapsed_ms: 1,
            },
            TraceEvent::TurnEnd {
                meta: TraceMeta { turn: 0, role: "manager".into(), ts: "t".into(), session_id: "s".into() },
                total_input: 50, total_output: 100, total_thinking: 0, elapsed_ms: 1,
            },
            TraceEvent::ToolExec {
                meta: TraceMeta { turn: 0, role: "manager".into(), ts: "t".into(), session_id: "s".into() },
                name: "read".into(), args_json: "{}".into(), latency_ms: 1,
                status: ToolStatus::Ok("ok".into()),
            },
            TraceEvent::TurnEnd {
                meta: TraceMeta { turn: 1, role: "programmer".into(), ts: "t".into(), session_id: "s".into() },
                total_input: 30, total_output: 60, total_thinking: 2, elapsed_ms: 1,
            },
        ];
        let agg = aggregate_tokens(&events);
        assert_eq!(agg.total_input, 180);
        assert_eq!(agg.total_output, 360);
        assert_eq!(agg.total_thinking, 7);
        assert_eq!(agg.per_turn.len(), 2);
        let m = agg.per_turn.iter().find(|r| r.role == "manager").unwrap();
        assert_eq!(m.input, 150);
        assert_eq!(m.output, 300);
        let p = agg.per_turn.iter().find(|r| r.role == "programmer").unwrap();
        assert_eq!(p.input, 30);
        assert_eq!(p.turn, 1);
    }

    #[test]
    fn filter_by_kind_keeps_matching_variant() {
        use latte_agent_core::trace::{HookPoint, TraceEvent, TraceMeta};
        let events = vec![
            TraceEvent::HookFired {
                meta: TraceMeta { turn: 0, role: "x".into(), ts: "t".into(), session_id: "s".into() },
                hook_name: "h".into(), point: HookPoint::PreCall, outcome_kind: "continue".into(),
            },
            TraceEvent::SessionEnd {
                meta: TraceMeta { turn: 0, role: "x".into(), ts: "t".into(), session_id: "s".into() },
                total_turns: 0, total_input: 0, total_output: 0, total_thinking: 0,
            },
        ];
        let h = filter_by_kind(events.clone(), Some("HookFired"));
        assert_eq!(h.len(), 1);
        let all = filter_by_kind(events, None);
        assert_eq!(all.len(), 2);
    }
}
