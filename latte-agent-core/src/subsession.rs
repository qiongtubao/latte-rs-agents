//! Server-side store for per-task subsession event logs.
//!
//! Each `DelegateStarted/Finished` carries a `sub_id` that points back
//! to one of these. The HTTP API exposes them via
//! `GET /api/sessions/{session_id}/subsessions/{sub_id}`.
//!
//! Storage is process-local (in-memory `parking_lot::Mutex<Vec<Arc<MemorySink>>>`)
//! keyed by `(session_id, sub_id)`. We don't persist to disk because the
//! lat agent `debug trace` directory already has JSONL files if a
//! future user wants long-term archive; for the immediate
//! "right-click → show contents" use case, in-memory is correct.
//!
//! The map can grow unbounded; a periodic cleaner sweeps sub_ids
//! older than `MAX_AGE` whenever `lookup` is called. This is cheap
//! and bounded — see `sweep_locked`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::trace::{MemorySink, TraceEvent};

/// How long a subsession's events are kept before GC reclaims them.
/// Long enough that the user can copy/paste from the viewer before
/// the entry disappears; short enough that a long-running server
/// doesn't OOM. 1 hour matches typical debugging flow.
pub const MAX_AGE: Duration = Duration::from_secs(60 * 60);

/// Compound key: one tab's worth of subsessions.
pub type SessionId = String;
pub type SubId = String;

#[derive(Default)]
pub struct SubsessionStore {
    /// (session, sub) → event log.
    inner: Mutex<Vec<((SessionId, SubId), Entry)>>,
}

struct Entry {
    sink: Arc<MemorySink>,
    created_at: Instant,
}

impl SubsessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate a fresh subsession for `(session_id, role_name)` and
    /// return the memory sink the runner should emit to + the
    /// generated sub_id the chat event should carry.
    pub fn create(&self, session_id: &str, role_name: &str) -> (SubId, Arc<MemorySink>) {
        let sub_id = format!(
            "{}-{}",
            role_name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_micros())
                .unwrap_or(0)
        );
        let sink = Arc::new(MemorySink::new());
        self.inner.lock().push((
            (session_id.to_string(), sub_id.clone()),
            Entry {
                sink: sink.clone(),
                created_at: Instant::now(),
            },
        ));
        (sub_id, sink)
    }

    /// Snapshot the captured events. None if the sub_id is unknown
    /// (already GC'd, or never existed).
    pub fn snapshot(&self, session_id: &str, sub_id: &str) -> Option<Vec<TraceEvent>> {
        self.sweep();
        let key = (session_id.to_string(), sub_id.to_string());
        let mut g = self.inner.lock();
        g.iter_mut()
            .find(|(k, _)| k == &key)
            .map(|(_, e)| e.sink.snapshot())
    }

    /// Lazy sweep: drop any sub-session entries older than `MAX_AGE`.
    /// Called opportunistically before each `snapshot`; O(n) but n is
    /// bounded by user activity, not chat volume.
    fn sweep(&self) {
        let now = Instant::now();
        let mut g = self.inner.lock();
        g.retain(|(_, entry)| now.duration_since(entry.created_at) < MAX_AGE);
    }

    /// Number of live entries (handy for tests / debug endpoints).
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }
}
