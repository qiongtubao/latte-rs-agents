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
}
