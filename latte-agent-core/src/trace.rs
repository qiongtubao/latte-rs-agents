//! Trace event types and sinks for full-chain observability.
//!
//! See `docs/superpowers/specs/2026-06-25-latte-agent-debug-observability-design.md`
//! for the design.

/// Per-event metadata. Carried on every `TraceEvent`.
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone)]
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
}
