pub mod chat;
pub mod chatlog;
pub mod config;
pub mod config_layer;
pub mod debug;
pub mod discuss;
pub mod list;
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
        let fanout = Arc::new(latte_agent_core::trace::FanoutSink::new(vec![
            jsonl,
            stdout,
        ]));
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
}

fn latte_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".latte")
}
