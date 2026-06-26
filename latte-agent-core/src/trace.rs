//! Trace event types and sinks for full-chain observability.
//!
//! See `docs/superpowers/specs/2026-06-25-latte-agent-debug-observability-design.md`
//! for the design.

use std::path::PathBuf;
use serde::{Deserialize, Serialize};

/// Per-event metadata. Carried on every `TraceEvent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceMeta {
    pub turn: u32,
    pub role: String, // "manager", "programmer", ...
    pub ts: String,   // ISO8601 UTC
    pub session_id: String,
}

impl TraceMeta {
    #[cfg(test)]
    pub fn test_default() -> Self {
        Self {
            turn: 0,
            role: "test".into(),
            ts: "1970-01-01T00:00:00Z".into(),
            session_id: "test-session".into(),
        }
    }

    /// Build a `TraceMeta` with the current UTC timestamp.
    /// Used by the agent runtime so emitted events carry a usable
    /// ISO8601 `ts` for timeline display (e.g. `latte-agent debug
    /// trace`). The `turn` is supplied by the caller because the
    /// runner increments it turn-by-turn.
    pub fn now(turn: u32, role: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            turn,
            role: role.into(),
            ts: iso8601_utc_now(),
            session_id: session_id.into(),
        }
    }
}

/// Format the current UTC time as `YYYY-MM-DDTHH:MM:SSZ`. Hand-rolled
/// (no `chrono` dep) so the trace module stays stdlib + serde +
/// parking_lot. Good enough for `latte-agent debug trace` timeline
/// display; not designed for sub-second precision or timezone math.
fn iso8601_utc_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = epoch_to_ymdhms(secs);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo, d, h, mi, s)
}

/// Convert a Unix-epoch second count to a `(year, month, day, h, m, s)`
/// tuple in UTC. Proleptic-Gregorian, correct for every timestamp we
/// care about in 2026.
fn epoch_to_ymdhms(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let s = (secs % 60) as u32;
    let mins = (secs / 60) as u32;
    let mi = mins % 60;
    let hours = mins / 60;
    let h = hours % 24;
    let mut days = (hours / 24) as i64;
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
    (year as u32, month as u32 + 1, days as u32 + 1, h, mi, s)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Truncate a string to `max` chars, appending a `+NB` indicator
/// when trimmed. Used by `body_for_pretty` so ToolExec args and
/// results stay readable but operators can see how much was cut.
/// Operates on char boundaries (safe for non-ASCII) and on the
/// raw string — for JSON we just count bytes since pretty-print
/// is for human eyes, not for re-parsing.
fn truncate_for_pretty(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}…[+{}B]", &s[..end], s.len() - end)
    }
}
/// payload that varies per variant. 9 variants cover the full
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TraceEvent {
    SessionStart {
        meta: TraceMeta,
        tier: String,
        model_chain: Vec<String>,
        allowed_tools: Vec<String>,
    },
    PromptBuilt {
        meta: TraceMeta,
        system_rendered: String,
        history_len: usize,
        user_input: String,
        est_input_tokens: u32,
    },
    ModelCall {
        meta: TraceMeta,
        model_id: String,
        params_json: String,
        latency_ms: u64,
        finish_reason: String,
    },
    ModelRawOut {
        meta: TraceMeta,
        raw_content: String,
    },
    ParseToolCalls {
        meta: TraceMeta,
        raw_in: String,
        parsed: Vec<ParsedCall>,
        diagnostics: ParseDiag,
    },
    ToolExec {
        meta: TraceMeta,
        name: String,
        args_json: String,
        latency_ms: u64,
        status: ToolStatus,
    },
    HookFired {
        meta: TraceMeta,
        hook_name: String,
        point: HookPoint,
        outcome_kind: String,
    },
    TurnEnd {
        meta: TraceMeta,
        total_input: u32,
        total_output: u32,
        total_thinking: u32,
        elapsed_ms: u64,
    },
    SessionEnd {
        meta: TraceMeta,
        total_turns: u32,
        total_input: u32,
        total_output: u32,
        total_thinking: u32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParsedCall {
    pub name: String,
    pub args: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParseDiag {
    pub opens_found: u32,
    pub closes_matched: u32,
    pub unmatched_opens: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ToolStatus {
    Ok(String),
    Err(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookPoint {
    PreCall,
    PostResponse,
    PostParse,
    PreTool,
    PostTool,
}

/// Receiver of trace events. Implementations must be `Send + Sync` so they
/// can be shared across async tasks.
pub trait TraceSink: Send + Sync {
    fn emit(&self, event: TraceEvent);
}

/// Default sink. `emit` is `#[inline]` empty; compiler eliminates the call.
#[derive(Default, Clone, Copy)]
pub struct NullSink;
impl TraceSink for NullSink {
    #[inline]
    fn emit(&self, _event: TraceEvent) {}
}

/// Writes each event as one JSON object per line. Thread-safe; uses
/// an internal Mutex<BufWriter> so concurrent emit() calls don't
/// interleave bytes.
pub struct JsonlSink {
    inner: parking_lot::Mutex<std::io::BufWriter<std::fs::File>>,
    path: PathBuf,
}

impl JsonlSink {
    pub fn new(path: PathBuf) -> Self {
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                panic!("JsonlSink: failed to create parent dir {}: {}", parent.display(), e);
            }
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("JsonlSink: failed to open trace file");
        Self {
            inner: parking_lot::Mutex::new(std::io::BufWriter::new(file)),
            path,
        }
    }
    pub fn path(&self) -> &PathBuf { &self.path }
}

impl TraceSink for JsonlSink {
    fn emit(&self, event: TraceEvent) {
        use std::io::Write;
        let mut guard = self.inner.lock();
        let json = serde_json::to_string(&event)
            .expect("TraceEvent serialization");
        writeln!(guard, "{}", json).expect("JsonlSink write");
    }
}

impl TraceEvent {
    pub fn meta(&self) -> &TraceMeta {
        match self {
            TraceEvent::SessionStart { meta, .. }
            | TraceEvent::PromptBuilt { meta, .. }
            | TraceEvent::ModelCall { meta, .. }
            | TraceEvent::ModelRawOut { meta, .. }
            | TraceEvent::ParseToolCalls { meta, .. }
            | TraceEvent::ToolExec { meta, .. }
            | TraceEvent::HookFired { meta, .. }
            | TraceEvent::TurnEnd { meta, .. }
            | TraceEvent::SessionEnd { meta, .. } => meta,
        }
    }
    pub fn variant_name(&self) -> &'static str {
        match self {
            TraceEvent::SessionStart { .. } => "SessionStart",
            TraceEvent::PromptBuilt { .. } => "PromptBuilt",
            TraceEvent::ModelCall { .. } => "ModelCall",
            TraceEvent::ModelRawOut { .. } => "ModelRawOut",
            TraceEvent::ParseToolCalls { .. } => "ParseToolCalls",
            TraceEvent::ToolExec { .. } => "ToolExec",
            TraceEvent::HookFired { .. } => "HookFired",
            TraceEvent::TurnEnd { .. } => "TurnEnd",
            TraceEvent::SessionEnd { .. } => "SessionEnd",
        }
    }
    pub fn body_for_pretty(&self) -> String {
        match self {
            TraceEvent::SessionStart { tier, model_chain, allowed_tools, .. } =>
                format!("tier={} model_chain={:?} tools={:?}", tier, model_chain, allowed_tools),
            TraceEvent::PromptBuilt { est_input_tokens, history_len, user_input, .. } =>
                format!("est_input={} history_len={} user_input={:?}",
                    est_input_tokens, history_len,
                    if user_input.len() > 60 { format!("{}…", &user_input[..60]) } else { user_input.clone() }),
            TraceEvent::ModelCall { model_id, latency_ms, finish_reason, .. } =>
                format!("model={} latency={}ms finish={}", model_id, latency_ms, finish_reason),
            TraceEvent::ModelRawOut { raw_content, .. } =>
                format!("{} chars: {}", raw_content.len(),
                    if raw_content.len() > 80 { format!("{}…", &raw_content[..80]) } else { raw_content.clone() }),
            TraceEvent::ParseToolCalls { parsed, diagnostics, .. } =>
                format!("parsed={} opens={} matched={} unmatched={}",
                    parsed.len(), diagnostics.opens_found, diagnostics.closes_matched, diagnostics.unmatched_opens.len()),
            TraceEvent::ToolExec { name, args_json, latency_ms, status, .. } => {
                // Show args + result so the operator can see what
                // the model asked the tool to do and what came
                // back. Both are truncated to keep the pretty
                // stream readable; the JSONL sink keeps the full
                // event for post-mortem.
                let args_short = truncate_for_pretty(args_json, 120);
                let result_short = match status {
                    ToolStatus::Ok(s) => format!("ok({} chars): {}", s.len(), truncate_for_pretty(s, 200)),
                    ToolStatus::Err(s) => format!("err({} chars): {}", s.len(), truncate_for_pretty(s, 200)),
                };
                format!("name={}\n  args:  {}\n  result: {}\n  latency: {}ms",
                    name, args_short, result_short, latency_ms)
            }
            TraceEvent::HookFired { hook_name, point, outcome_kind, .. } =>
                format!("{} {:?} {}", hook_name, point, outcome_kind),
            TraceEvent::TurnEnd { total_input, total_output, total_thinking, elapsed_ms, .. } =>
                format!("in={} out={} think={} elapsed={}ms",
                    total_input, total_output, total_thinking, elapsed_ms),
            TraceEvent::SessionEnd { total_turns, total_input, total_output, total_thinking, .. } =>
                format!("turns={} in={} out={} think={}", total_turns, total_input, total_output, total_thinking),
        }
    }
    /// Metadata-only projection for IndexSink. Returns None for
    /// events that have no indexable information.
    pub fn to_index_line(&self) -> Option<IndexLine> {
        let meta = self.meta();
        Some(match self {
            TraceEvent::SessionStart { tier, model_chain, allowed_tools, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "SessionStart".into(),
                model_id: None, latency_ms: None, tokens_in: None, tokens_out: None, tokens_think: None,
                detail: format!("tier={} chain_len={} tools={}", tier, model_chain.len(), allowed_tools.len()),
            },
            TraceEvent::PromptBuilt { est_input_tokens, history_len, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "PromptBuilt".into(),
                model_id: None, latency_ms: None,
                tokens_in: Some(*est_input_tokens), tokens_out: None, tokens_think: None,
                detail: format!("history_len={}", history_len),
            },
            TraceEvent::ModelCall { model_id, latency_ms, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "ModelCall".into(),
                model_id: Some(model_id.clone()), latency_ms: Some(*latency_ms),
                tokens_in: None, tokens_out: None, tokens_think: None,
                detail: String::new(),
            },
            TraceEvent::ModelRawOut { .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "ModelRawOut".into(),
                model_id: None, latency_ms: None, tokens_in: None, tokens_out: None, tokens_think: None,
                detail: String::new(),
            },
            TraceEvent::ParseToolCalls { parsed, diagnostics, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "ParseToolCalls".into(),
                model_id: None, latency_ms: None, tokens_in: None, tokens_out: None, tokens_think: None,
                detail: format!("parsed={} unmatched={}", parsed.len(), diagnostics.unmatched_opens.len()),
            },
            TraceEvent::ToolExec { name, latency_ms, status, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "ToolExec".into(),
                model_id: None, latency_ms: Some(*latency_ms), tokens_in: None, tokens_out: None, tokens_think: None,
                detail: format!("name={} status={}", name, match status { ToolStatus::Ok(_) => "ok", ToolStatus::Err(_) => "err" }),
            },
            TraceEvent::HookFired { hook_name, point, outcome_kind, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "HookFired".into(),
                model_id: None, latency_ms: None, tokens_in: None, tokens_out: None, tokens_think: None,
                detail: format!("hook={} point={:?} outcome={}", hook_name, point, outcome_kind),
            },
            TraceEvent::TurnEnd { total_input, total_output, total_thinking, elapsed_ms, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "TurnEnd".into(),
                model_id: None, latency_ms: Some(*elapsed_ms),
                tokens_in: Some(*total_input), tokens_out: Some(*total_output), tokens_think: Some(*total_thinking),
                detail: String::new(),
            },
            TraceEvent::SessionEnd { total_turns, total_input, total_output, total_thinking, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "SessionEnd".into(),
                model_id: None, latency_ms: None,
                tokens_in: Some(*total_input), tokens_out: Some(*total_output), tokens_think: Some(*total_thinking),
                detail: format!("turns={}", total_turns),
            },
        })
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct IndexLine {
    pub turn: u32,
    pub ts: String,
    pub role: String,
    pub kind: String,
    pub model_id: Option<String>,
    pub latency_ms: Option<u64>,
    pub tokens_in: Option<u32>,
    pub tokens_out: Option<u32>,
    pub tokens_think: Option<u32>,
    pub detail: String,
}

/// Writes a human-readable multi-line pretty form (default) or
/// raw JSONL (one event per line) to a `Write` impl. The mode is
/// chosen at construction so the format is stable for the lifetime
/// of the sink — output never flips mid-stream.
///
/// `pretty=true` is the default: multi-line, optionally colorized
/// when `use_color` is also true and the writer is a TTY. This is
/// what `latte-agent chat --debug` uses.
///
/// `pretty=false` is the JSONL mode: one `serde_json::to_string`
/// per event, newline-terminated. This is what
/// `latte-agent chat --debug --debug-format jsonl` selects so
/// downstream tools (jq, ripgrep, the future `latte-agent debug
/// replay`) can read the stream.
pub struct StdoutSink {
    writer: parking_lot::Mutex<Box<dyn std::io::Write + Send>>,
    pretty: bool,
    #[allow(dead_code)] // wired up in a later task when the color path lands
    use_color: bool,
}

impl StdoutSink {
    /// Construct a pretty sink bound to stdout. `use_color` is
    /// honored when the writer is a TTY.
    pub fn new(use_color: bool) -> Self {
        Self { writer: parking_lot::Mutex::new(Box::new(std::io::stdout())), pretty: true, use_color }
    }
    /// Construct a pretty sink with a caller-supplied writer.
    pub fn with_writer(w: impl std::io::Write + Send + 'static, use_color: bool) -> Self {
        Self { writer: parking_lot::Mutex::new(Box::new(w)), pretty: true, use_color }
    }
    /// Construct a JSONL sink bound to stdout. Each emitted event
    /// is one JSON object per line; `use_color` is ignored.
    pub fn new_jsonl() -> Self {
        Self { writer: parking_lot::Mutex::new(Box::new(std::io::stdout())), pretty: false, use_color: false }
    }
    /// Construct a JSONL sink with a caller-supplied writer.
    pub fn with_writer_jsonl(w: impl std::io::Write + Send + 'static) -> Self {
        Self { writer: parking_lot::Mutex::new(Box::new(w)), pretty: false, use_color: false }
    }
}

impl TraceSink for StdoutSink {
    fn emit(&self, event: TraceEvent) {
        use std::io::Write;
        let mut guard = self.writer.lock();
        if self.pretty {
            let _ = writeln!(guard, "─── turn {} · role={} · session={} · {} ───",
                event.meta().turn, event.meta().role, event.meta().session_id, event.meta().ts);
            let _ = writeln!(guard, "[{}]", event.variant_name());
            let body = event.body_for_pretty();
            for line in body.lines() {
                let _ = writeln!(guard, "  {}", line);
            }
        } else {
            let json = serde_json::to_string(&event).expect("TraceEvent serialization");
            let _ = writeln!(guard, "{}", json);
        }
        let _ = guard.flush();
    }
}

/// Always-on metadata-only index. Strips content fields to keep the
/// index small and safe for always-on writing.
pub struct IndexSink {
    inner: parking_lot::Mutex<std::io::BufWriter<std::fs::File>>,
    path: PathBuf,
}

impl IndexSink {
    pub fn new(path: PathBuf) -> Self {
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                panic!("IndexSink: failed to create parent dir {}: {}", parent.display(), e);
            }
        }
        let file = std::fs::OpenOptions::new()
            .create(true).append(true).open(&path)
            .expect("IndexSink open");
        Self { inner: parking_lot::Mutex::new(std::io::BufWriter::new(file)), path }
    }
    pub fn path(&self) -> &PathBuf { &self.path }
}

impl TraceSink for IndexSink {
    fn emit(&self, event: TraceEvent) {
        use std::io::Write;
        let mut guard = self.inner.lock();
        let index = event.to_index_line();
        if let Some(line) = index {
            let json = serde_json::to_string(&line).expect("index serialization");
writeln!(guard, "{}", json).expect("IndexSink write");
        }
    }
}

/// Routes a single emit to N child sinks in order. Each child runs in
/// the caller's thread (no async/spawning); sink implementations are
/// expected to be cheap or internally-thread-safe.
pub struct FanoutSink { sinks: Vec<std::sync::Arc<dyn TraceSink>> }

impl FanoutSink {
    pub fn new(sinks: Vec<std::sync::Arc<dyn TraceSink>>) -> Self { Self { sinks } }
    pub fn push(&mut self, s: std::sync::Arc<dyn TraceSink>) { self.sinks.push(s); }
    pub fn children(&self) -> &[std::sync::Arc<dyn TraceSink>] { &self.sinks }
}

impl TraceSink for FanoutSink {
    fn emit(&self, event: TraceEvent) {
        use std::panic::{catch_unwind, AssertUnwindSafe};
        for s in &self.sinks {
            // Per spec §14: isolate child panics so one bad sink doesn't
            // drop the events the other sinks were supposed to see.
            // AssertUnwindSafe is required because `TraceSink` carries no
            // UnwindSafe guarantees; the children own their own state.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                s.emit(event.clone());
            }));
        }
    }
}

/// Wraps another sink and overrides the `role` field on every event.
/// Used by `register_delegate_tool` to tag specialist events with the
/// specialist's role id without the caller having to remember.
pub struct ScopedSink {
    inner: std::sync::Arc<dyn TraceSink>,
    role: String,
}

impl ScopedSink {
    pub fn new(inner: std::sync::Arc<dyn TraceSink>, role: String) -> Self { Self { inner, role } }
}

impl TraceSink for ScopedSink {
    fn emit(&self, mut event: TraceEvent) {
        match &mut event {
            TraceEvent::SessionStart { meta, .. }
            | TraceEvent::PromptBuilt { meta, .. }
            | TraceEvent::ModelCall { meta, .. }
            | TraceEvent::ModelRawOut { meta, .. }
            | TraceEvent::ParseToolCalls { meta, .. }
            | TraceEvent::ToolExec { meta, .. }
            | TraceEvent::HookFired { meta, .. }
            | TraceEvent::TurnEnd { meta, .. }
            | TraceEvent::SessionEnd { meta, .. } => {
                meta.role = self.role.clone();
            }
        }
        self.inner.emit(event);
    }
}

/// Drops events whose `variant_name()` isn't in the allow-list. Used by
/// the `--debug-events` CLI flag to filter the on-stdout debug stream
/// down to a subset of `TraceEvent` variants (e.g. `ToolExec,HookFired`).
/// The JsonlSink / IndexSink stay unfiltered — operators still want
/// the full trace on disk; the filter is only for the live stdout view.
/// `allow: "all"` (or empty) disables filtering; any other comma-separated
/// string is treated as variant names. Comparison is exact-match against
/// `TraceEvent::variant_name()` (e.g. `"ToolExec"`, `"HookFired"`,
/// `"ModelCall"`).
pub struct FilterSink {
    inner: std::sync::Arc<dyn TraceSink>,
    /// Set of allowed variant names. `None` means "all" (no filter);
    /// `Some(empty)` means "match nothing" (filter out everything).
    allowed: Option<std::collections::HashSet<String>>,
}

impl FilterSink {
    /// Build a `FilterSink` from a comma-separated list. `"all"` or
    /// `""` means no filtering (every event passes through). Otherwise the
    /// string is split on `,` and each token is treated as a variant
    /// name. Whitespace around tokens is trimmed.
    ///
    /// Unknown variant names pass through silently — the filter is
    /// best-effort and shouldn't fail the program if a user typo'd
    /// `ToolExecution` instead of `ToolExec`. Operators will simply
    /// see fewer events than they expected, which is recoverable.
    pub fn new(inner: std::sync::Arc<dyn TraceSink>, filter: &str) -> Self {
        let trimmed = filter.trim();
        let allowed = if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("all") {
            None
        } else {
            Some(
                trimmed
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
            )
        };
        Self { inner, allowed }
    }
}

impl TraceSink for FilterSink {
    fn emit(&self, event: TraceEvent) {
        if let Some(allowed) = &self.allowed {
            // `variant_name()` returns `&'static str`; `HashSet<String>`
            // lookups accept `&str` via `Borrow`, so no allocation here.
            if !allowed.contains(event.variant_name()) {
                return;
            }
        }
        self.inner.emit(event);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    // VecSink test helper used across all trace tests
    pub struct VecSink(pub Mutex<Vec<TraceEvent>>);
    impl TraceSink for VecSink {
        fn emit(&self, e: TraceEvent) {
            self.0.lock().push(e);
        }
    }
    impl VecSink {
        pub fn drain(&self) -> Vec<TraceEvent> {
            std::mem::take(&mut *self.0.lock())
        }
    }

    #[test]
    fn null_sink_emits_without_panic() {
        let s = NullSink;
        s.emit(TraceEvent::SessionEnd {
            meta: TraceMeta::test_default(),
            total_turns: 1,
            total_input: 10,
            total_output: 20,
            total_thinking: 0,
        });
    }

    #[test]
    fn vec_sink_captures() {
        let s = VecSink(Mutex::new(vec![]));
        s.emit(TraceEvent::SessionEnd {
            meta: TraceMeta::test_default(),
            total_turns: 2,
            total_input: 100,
            total_output: 200,
            total_thinking: 5,
        });
        s.emit(TraceEvent::SessionEnd {
            meta: TraceMeta::test_default(),
            total_turns: 3,
            total_input: 110,
            total_output: 210,
            total_thinking: 6,
        });
        assert_eq!(s.drain().len(), 2);
    }

    #[test]
    fn jsonl_sink_writes_one_line_per_event() {
        let dir = std::env::temp_dir().join(format!("latte-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trace.jsonl");
        let sink = JsonlSink::new(path.clone());
        sink.emit(TraceEvent::SessionEnd {
            meta: TraceMeta::test_default(),
            total_turns: 1, total_input: 10, total_output: 20, total_thinking: 0,
        });
        sink.emit(TraceEvent::SessionEnd {
            meta: TraceMeta::test_default(),
            total_turns: 2, total_input: 100, total_output: 200, total_thinking: 5,
        });
        drop(sink);  // flush BufWriter
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "expected 2 JSONL lines, got: {:?}", content);
        // Each line must be valid JSON with a "SessionEnd" key
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(v.get("SessionEnd").is_some(), "line missing SessionEnd: {}", line);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn jsonl_sink_concurrent_emits_do_not_interleave() {
        // Characterization test: the struct doc claims concurrent emit() calls
        // don't interleave bytes. If the Mutex<BufWriter> ever regressed (e.g.
        // switched to per-call open/append), serde_json::from_str on each line
        // would fail because mid-line byte mixing produces invalid JSON.
        let dir = std::env::temp_dir().join(format!("latte-test-concurrent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trace-concurrent.jsonl");
        let sink = std::sync::Arc::new(JsonlSink::new(path.clone()));

        let mut handles = Vec::new();
        for thread_idx in 0u32..4 {
            let sink = std::sync::Arc::clone(&sink);
            handles.push(std::thread::spawn(move || {
                for _ in 0..25 {
                    sink.emit(TraceEvent::SessionEnd {
                        meta: TraceMeta::test_default(),
                        total_turns: thread_idx,
                        total_input: 0,
                        total_output: 0,
                        total_thinking: 0,
                    });
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        drop(sink);

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 100, "expected 100 JSONL lines, got: {}", lines.len());

        // Each line must be valid JSON whose SessionEnd.total_turns matches
        // the originating thread index. Any byte-mixing between threads
        // would either fail JSON parsing or yield a total_turns value that
        // does not match any single thread index.
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("line not valid JSON (byte interleaving?): {} -- {}", line, e));
            let se = v.get("SessionEnd")
                .unwrap_or_else(|| panic!("line missing SessionEnd: {}", line));
            let total_turns = se.get("total_turns")
                .and_then(|x| x.as_u64())
                .expect("total_turns missing or not u64");
            assert!(
                total_turns < 4,
                "total_turns={} from a thread index 0..=3 — line is corrupt: {}",
                total_turns, line
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn jsonl_sink_appends_to_existing_file() {
        // Characterization test: append-mode is load-bearing for re-running
        // debug sessions. JsonlSink::new must NOT truncate prior content.
        let dir = std::env::temp_dir().join(format!("latte-test-append-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trace-append.jsonl");

        // Pre-existing line written via std::fs::write (independent of sink).
        std::fs::write(&path, "{\"pre\":1}\n").unwrap();

        let sink = JsonlSink::new(path.clone());
        sink.emit(TraceEvent::SessionEnd {
            meta: TraceMeta::test_default(),
            total_turns: 7,
            total_input: 0,
            total_output: 0,
            total_thinking: 0,
        });
        drop(sink);

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "expected 2 lines (1 prior + 1 new), got: {:?}", content);
        // The new (second) line must be the SessionEnd event we emitted.
        let v: serde_json::Value = serde_json::from_str(lines[1])
            .expect("second line not valid JSON");
        assert!(v.get("SessionEnd").is_some(), "second line missing SessionEnd: {}", lines[1]);
        std::fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn stdout_sink_writes_to_writer() {
        let buf = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
        let writer = StdoutSinkWriter(buf.clone());
        let sink = StdoutSink::with_writer(writer, false /* no color */);
        sink.emit(TraceEvent::SessionEnd {
            meta: TraceMeta::test_default(),
            total_turns: 1, total_input: 10, total_output: 20, total_thinking: 0,
        });
        let out = String::from_utf8(buf.lock().clone()).unwrap();
        assert!(out.contains("SessionEnd"), "missing variant: {}", out);
        assert!(out.contains("test-session"), "missing session_id: {}", out);
    }

    /// Test-only writer adapter so we can capture stdout in tests.
    pub struct StdoutSinkWriter(pub std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);
    impl std::io::Write for StdoutSinkWriter {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }

    #[test]
    fn index_sink_writes_session_end_to_disk() {
        // TODO(Task 4): re-add "strips payload" assertion against a
        // content-bearing variant (e.g. ModelRawOut) once that variant
        // lands. The current 2 variants (SessionStart, SessionEnd) carry
        // no payload fields, so this test only verifies on-disk write.
        let dir = std::env::temp_dir().join(format!("latte-test-idx-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.idx");
        let sink = IndexSink::new(path.clone());
        let mut meta = TraceMeta::test_default();
        meta.role = "manager".into();
        sink.emit(TraceEvent::SessionEnd {
            meta,
            total_turns: 1, total_input: 10, total_output: 20, total_thinking: 0,
        });
        drop(sink);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("SessionEnd"));
        assert!(content.contains("\"role\":\"manager\""));
        std::fs::remove_dir_all(&dir).ok();
    }

    struct PanickingSink;
    impl TraceSink for PanickingSink {
        fn emit(&self, _event: TraceEvent) { panic!("intentional"); }
    }

    #[test]
    fn fanout_sink_isolates_child_panic() {
        use std::sync::Arc;
        // Order matters: panicking sink FIRST, normal sink SECOND.
        // Without catch_unwind, the second sink would never receive
        // the event because the test process would unwind through it.
        let ok = Arc::new(VecSink(parking_lot::Mutex::new(vec![])));
        let bad: Arc<dyn TraceSink> = Arc::new(PanickingSink);
        let fan = FanoutSink::new(vec![bad, ok.clone()]);
        fan.emit(TraceEvent::SessionEnd {
            meta: TraceMeta::test_default(),
            total_turns: 1, total_input: 1, total_output: 1, total_thinking: 0,
        });
        assert_eq!(
            ok.0.lock().len(),
            1,
            "sibling sink must still receive event after sibling panics",
        );
    }

    #[test]
    fn fanout_sink_routes_to_all_children() {
        let s1 = std::sync::Arc::new(VecSink(parking_lot::Mutex::new(vec![])));
        let s2 = std::sync::Arc::new(VecSink(parking_lot::Mutex::new(vec![])));
        let fan = FanoutSink::new(vec![s1.clone(), s2.clone()]);
        fan.emit(TraceEvent::SessionEnd {
            meta: TraceMeta::test_default(),
            total_turns: 1, total_input: 1, total_output: 1, total_thinking: 0,
        });
        assert_eq!(s1.0.lock().len(), 1);
        assert_eq!(s2.0.lock().len(), 1);
    }

    #[test]
    fn scoped_sink_overrides_role() {
        // Use a fresh local VecSink; the test-only VecSink has pub inner field
        let inner = std::sync::Arc::new(VecSink(parking_lot::Mutex::new(vec![])));
        let scoped = ScopedSink::new(inner.clone(), "programmer".into());
        let mut meta = TraceMeta::test_default();
        meta.role = "WRONG".into();
        scoped.emit(TraceEvent::SessionEnd {
            meta,
            total_turns: 1, total_input: 1, total_output: 1, total_thinking: 0,
        });
        let captured = inner.0.lock();
        assert_eq!(captured.len(), 1);
        match &captured[0] {
            TraceEvent::SessionEnd { meta, .. } => {
                assert_eq!(meta.role, "programmer", "scoped sink did not override role");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn trace_event_variants_serialize_roundtrip() {
        // Task 4: all 9 variants must survive JSON round-trip. Each
        // arm constructs the variant with realistic-looking data,
        // serializes via serde_json, parses back, and asserts the
        // discriminant (variant name) is preserved.
        let mut meta = TraceMeta::test_default();
        meta.role = "tester".into();
        let events: Vec<TraceEvent> = vec![
            TraceEvent::SessionStart {
                meta: meta.clone(),
                tier: "standard".into(),
                model_chain: vec!["glm-5.2".into(), "deepseek-v4-flash".into()],
                allowed_tools: vec!["read".into(), "bash".into()],
            },
            TraceEvent::PromptBuilt {
                meta: meta.clone(),
                system_rendered: "you are a tester".into(),
                history_len: 3,
                user_input: "analyze chat.rs".into(),
                est_input_tokens: 1234,
            },
            TraceEvent::ModelCall {
                meta: meta.clone(),
                model_id: "glm-5.2".into(),
                params_json: r#"{"temperature":0.5}"#.into(),
                latency_ms: 2741,
                finish_reason: "stop".into(),
            },
            TraceEvent::ModelRawOut {
                meta: meta.clone(),
                raw_content: "<tool_callexec> {\"command\": \"pwd\"}</tool_call>".into(),
            },
            TraceEvent::ParseToolCalls {
                meta: meta.clone(),
                raw_in: "<tool_callexec> {\"command\": \"pwd\"}</tool_call>".into(),
                parsed: vec![ParsedCall { name: "exec".into(), args: "{}".into() }],
                diagnostics: ParseDiag { opens_found: 1, closes_matched: 1, unmatched_opens: vec![] },
            },
            TraceEvent::ToolExec {
                meta: meta.clone(),
                name: "exec".into(),
                args_json: r#"{"command":"pwd"}"#.into(),
                latency_ms: 50,
                status: ToolStatus::Ok("/Users/zhouguodong".into()),
            },
            TraceEvent::HookFired {
                meta: meta.clone(),
                hook_name: "redact_pii".into(),
                point: HookPoint::PreCall,
                outcome_kind: "mutate".into(),
            },
            TraceEvent::TurnEnd {
                meta: meta.clone(),
                total_input: 100, total_output: 200, total_thinking: 0, elapsed_ms: 5000,
            },
            TraceEvent::SessionEnd {
                meta,
                total_turns: 3, total_input: 300, total_output: 600, total_thinking: 10,
            },
        ];
        assert_eq!(events.len(), 9);
        for e in &events {
            let json = serde_json::to_string(e).expect("serialize");
            let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
            // Each variant serializes as a single-key object whose key
            // is the variant name. Verify the key matches variant_name().
            let key = v.as_object().expect("object").keys().next().expect("one key").clone();
            assert_eq!(key, e.variant_name(), "variant_name mismatch for {}", key);
        }
    }
}
