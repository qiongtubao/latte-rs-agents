//! Trace event types and sinks for full-chain observability.
//!
//! See `docs/superpowers/specs/2026-06-25-latte-agent-debug-observability-design.md`
//! for the design.

use std::path::PathBuf;
use serde::Serialize;

/// Per-event metadata. Carried on every `TraceEvent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
}

/// Trace event emitted by the agent runtime. Only the minimal skeleton
/// variants are defined here; the full 9-variant enum populates in Task 4.
#[derive(Debug, Clone, Serialize)]
pub enum TraceEvent {
    SessionStart {
        meta: TraceMeta,
        tier: String,
        model_chain: Vec<String>,
        allowed_tools: Vec<String>,
    },
    SessionEnd {
        meta: TraceMeta,
        total_turns: u32,
        total_input: u32,
        total_output: u32,
        total_thinking: u32,
    },
    // ... more variants added in later tasks; SessionEnd is enough for Task 1.
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
            TraceEvent::SessionStart { meta, .. } | TraceEvent::SessionEnd { meta, .. } => meta,
        }
    }
    pub fn variant_name(&self) -> &'static str {
        match self {
            TraceEvent::SessionStart { .. } => "SessionStart",
            TraceEvent::SessionEnd { .. } => "SessionEnd",
        }
    }
    /// Multi-line body for the StdoutSink pretty form. For Task 3
    /// only the 2 existing variants are populated; Task 4 will add
    /// arms for the other 7 variants.
    pub fn body_for_pretty(&self) -> String {
        match self {
            TraceEvent::SessionStart { tier, model_chain, allowed_tools, .. } =>
                format!("tier={} model_chain={:?} tools={:?}", tier, model_chain, allowed_tools),
            TraceEvent::SessionEnd { total_turns, total_input, total_output, total_thinking, .. } =>
                format!("turns={} in={} out={} think={}", total_turns, total_input, total_output, total_thinking),
        }
    }
    /// Metadata-only projection for IndexSink. Returns None for
    /// events that have no indexable information. For Task 3 only
    /// the 2 existing variants are populated; Task 4 will add
    /// arms for the other 7 variants.
    pub fn to_index_line(&self) -> Option<IndexLine> {
        let meta = self.meta();
        Some(match self {
            TraceEvent::SessionStart { tier, model_chain, allowed_tools, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "SessionStart".into(),
                model_id: None, latency_ms: None, tokens_in: None, tokens_out: None, tokens_think: None,
                detail: format!("tier={} chain_len={} tools={}", tier, model_chain.len(), allowed_tools.len()),
            },
            TraceEvent::SessionEnd { total_turns, total_input, total_output, total_thinking, .. } => IndexLine {
                turn: meta.turn, ts: meta.ts.clone(), role: meta.role.clone(),
                kind: "SessionEnd".into(),
                model_id: None, latency_ms: None, tokens_in: Some(*total_input), tokens_out: Some(*total_output), tokens_think: Some(*total_thinking),
                detail: format!("turns={}", total_turns),
            },
        })
    }
}

#[derive(Debug, serde::Serialize, Clone)]
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

/// Writes a human-readable multi-line pretty form to a `Write` impl.
/// `use_color` is set by the caller based on `IsTerminal`.
pub struct StdoutSink {
    writer: parking_lot::Mutex<Box<dyn std::io::Write + Send>>,
    #[allow(dead_code)] // wired up in a later task when the color path lands
    use_color: bool,
}

impl StdoutSink {
    pub fn new(use_color: bool) -> Self {
        Self { writer: parking_lot::Mutex::new(Box::new(std::io::stdout())), use_color }
    }
    pub fn with_writer(w: impl std::io::Write + Send + 'static, use_color: bool) -> Self {
        Self { writer: parking_lot::Mutex::new(Box::new(w)), use_color }
    }
}

impl TraceSink for StdoutSink {
    fn emit(&self, event: TraceEvent) {
        use std::io::Write;
        let mut guard = self.writer.lock();
        let _ = writeln!(guard, "─── turn {} · role={} · session={} · {} ───",
            event.meta().turn, event.meta().role, event.meta().session_id, event.meta().ts);
        let _ = writeln!(guard, "[{}]", event.variant_name());
        let body = event.body_for_pretty();
        for line in body.lines() {
            let _ = writeln!(guard, "  {}", line);
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
        if let Some(parent) = path.parent() { std::fs::create_dir_all(parent).ok(); }
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
            let _ = writeln!(guard, "{}", json);
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
        for s in &self.sinks {
            s.emit(event.clone());
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
            TraceEvent::SessionStart { meta, .. } | TraceEvent::SessionEnd { meta, .. } => {
                meta.role = self.role.clone();
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
    fn index_sink_strips_payload_fields() {
        let dir = std::env::temp_dir().join(format!("latte-test-idx-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.idx");
        let sink = IndexSink::new(path.clone());
        let mut meta = TraceMeta::test_default();
        meta.role = "manager".into();
        // Note: ModelRawOut is NOT yet a variant (Task 4). For Task 3,
        // use SessionEnd. The point is to confirm the index file does
        // NOT include any raw content fields. We'll verify this in Task 4.
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
}
