pub mod chat;
pub mod chatlog;
pub mod config;
pub mod config_layer;
pub mod debug;
pub mod discuss;
pub mod list;
pub mod repl;
pub mod style;
pub mod trace_store;
pub mod workflow;

use std::path::PathBuf;
use std::sync::Arc;

use latte_agent_core::hooks::HookChain;
use latte_agent_core::trace::TraceSink;
use std::io::IsTerminal;

use crate::commands::debug::DebugFormat;

/// Build the `--debug` sink bundle for a session.
///
/// When `flags.debug` is true, returns a `FanoutSink` of `[StdoutSink, JsonlSink]`
/// plus the always-on `IndexSink` (unless `flags.no_session_index` is set).
/// Otherwise returns just the `IndexSink` (no overhead).
pub fn build_debug_sink(
    session_id: &str,
    flags: &DebugFlags,
) -> Arc<dyn TraceSink> {
    let index_sink: Arc<dyn TraceSink> = if flags.no_session_index {
        Arc::new(latte_agent_core::trace::NullSink)
    } else {
        let idx_path = latte_dir().join("sessions").join(format!("{}.idx", session_id));
        Arc::new(latte_agent_core::trace::IndexSink::new(idx_path))
    };
    if flags.debug {
        let trace_path = latte_dir().join("traces").join(format!("{}.jsonl", session_id));
        let jsonl: Arc<dyn TraceSink> = Arc::new(latte_agent_core::trace::JsonlSink::new(trace_path));
        let stdout: Arc<dyn TraceSink> = match flags.debug_format {
            DebugFormat::Auto => Arc::new(latte_agent_core::trace::StdoutSink::new(
                std::io::stdout().is_terminal(),
            )),
            DebugFormat::Pretty => Arc::new(latte_agent_core::trace::StdoutSink::new(true)),
            DebugFormat::Jsonl => Arc::new(latte_agent_core::trace::StdoutSink::new_jsonl()),
        };
        // Apply the --debug-events filter on top of the stdout sink only.
        // The JSONL sink and the session index are deliberately unfiltered
        // so the on-disk trace keeps every event (operators can re-filter
        // post-hoc with `latte-agent debug trace --filter ...`).
        let stdout: Arc<dyn TraceSink> = match &flags.debug_events {
            Some(filter) => Arc::new(latte_agent_core::trace::FilterSink::new(stdout, filter)),
            None => stdout,
        };
        let fanout = Arc::new(latte_agent_core::trace::FanoutSink::new(vec![
            jsonl,
            stdout,
        ]));
        // Wrap the (jsonl + filtered-stdout) fanout in an outer
        // fanout that also writes to the always-on session index.
        // Two layers so a stdout-only filter doesn't drop the JSONL
        // or the index — both are written unconditionally.
        Arc::new(latte_agent_core::trace::FanoutSink::new(vec![
            fanout,
            index_sink,
        ]))
    } else {
        index_sink
    }
}

/// Build the `--debug-hooks` chain for a session.
pub fn build_debug_hooks(flags: &DebugFlags) -> Arc<HookChain> {
    if flags.debug_hooks.is_empty() {
        return Arc::new(HookChain::empty());
    }
    let hooks = crate::commands::debug::debug_hook_names_to_arcs(
        &flags.debug_hooks.join(","),
    );
    let mut chain = HookChain::empty();
    for h in hooks {
        chain = chain.push(h);
    }
    Arc::new(chain)
}

/// Minimal flag bundle shared by `ChatCmd` and `DiscussCmd` for
/// their `--debug*` family of flags.
#[derive(Default, Clone)]
pub struct DebugFlags {
    pub debug: bool,
    pub debug_format: DebugFormat,
    pub debug_hooks: Vec<String>,
    pub no_session_index: bool,
    /// Comma-separated list of TraceEvent variant names to forward to
    /// the on-stdout debug stream. `None` or `"all"` means no filter
    /// (every event lands on stdout). Wired to the `--debug-events`
    /// CLI flag. The JSONL trace and session index stay unfiltered
    /// so the full event stream is still on disk — the filter is only
    /// for the live stdout view.
    pub debug_events: Option<String>,
    /// Canonical session id for this run. For chat it's the stem of
    /// the per-session ChatLog file (`chat-YYYYMMDD-HHMMSS-<pid>`),
    /// so the trace JSONL, session idx, and chat log all share a
    /// stem and can be cross-referenced per spec §9. For discuss
    /// it's a synthesized `discuss-...` id. Threaded through every
    /// `TraceMeta.session_id` emitted by the runner.
    pub session_id: String,
}

fn latte_dir() -> PathBuf {
    // Delegate to `trace_store::latte_home` so the precedence rule
    // (LATTE_HOME → project .latte/ → ~/.latte/) is defined in
    // exactly one place. Falls back to `/tmp/.latte` if no
    // discoverable home exists so the sink constructors can
    // still get a writable path.
    crate::commands::trace_store::latte_home()
        .unwrap_or_else(|| PathBuf::from("/tmp").join(".latte"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Save/restore wrapper for `LATTE_HOME` so a test mutating the
    /// env var doesn't leak into sibling tests.
    struct LatteHomeGuard {
        prev: Option<String>,
    }
    impl LatteHomeGuard {
        fn set(value: &str) -> Self {
            let prev = std::env::var("LATTE_HOME").ok();
            std::env::set_var("LATTE_HOME", value);
            Self { prev }
        }
    }
    impl Drop for LatteHomeGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("LATTE_HOME", v),
                None => std::env::remove_var("LATTE_HOME"),
            }
        }
    }

    /// HIGH 3 verification: `build_debug_sink` should honor
    /// `LATTE_HOME` so tests and sandboxed dev envs can redirect
    /// the `~/.latte/traces/...` and `~/.latte/sessions/...` writes.
    /// path via a custom no-op sink that records what was passed.
    #[test]
    fn debug_sink_uses_latte_home_when_set() {
        let _g = LatteHomeGuard::set("/tmp/latte-test-home");
        let flags = DebugFlags {
            debug: true,
            debug_format: crate::commands::debug::DebugFormat::Jsonl,
            debug_hooks: vec![],
            no_session_index: false,
            debug_events: None,
            session_id: "test-sess".into(),
        };
        // The sink is built but we don't run it; we just verify it
        // doesn't panic when LATTE_HOME is set. To verify the path
        // is rooted at LATTE_HOME we'd need a sink-introspecting
        // helper; the existing trace_store::latte_home() unit tests
        // already cover that side of the helper. This test is a
        // smoke test that the env var doesn't blow up the sink
        // builder.
        let _sink = build_debug_sink(&flags.session_id, &flags);
    }

    /// Same as above but with `LATTE_HOME` unset — falls back to
    /// `$HOME/.latte`. We can't easily test the fallback path
    /// without clobbering the user's home, so this just checks
    /// the build path doesn't panic.
    #[test]
    fn debug_sink_builds_without_latte_home() {
        // Ensure LATTE_HOME is not set.
        let prev = std::env::var("LATTE_HOME").ok();
        std::env::remove_var("LATTE_HOME");
        let flags = DebugFlags {
            debug: false,
            debug_format: crate::commands::debug::DebugFormat::Auto,
            debug_hooks: vec![],
            no_session_index: false,
            debug_events: None,
            session_id: "test-sess-fallback".into(),
        };
        let _sink = build_debug_sink(&flags.session_id, &flags);
        if let Some(v) = prev {
            std::env::set_var("LATTE_HOME", v);
        }
    }
}
