//! `latte-agent debug <subcommand>` — offline inspection of recorded
//! sessions, prompts, parser, and hooks. See spec §8.2.
//!
//! All subcommands are read-only against the trace store and never
//! call any model API. That's the safety property that makes
//! `debug replay` useful: iterate on hook strategies against real
//! recorded prompts without burning tokens.

use std::sync::Arc;

use clap::{Args, Subcommand, ValueEnum};
use latte_agent_core::trace::TraceEvent;

use super::config_layer::{self, CliOverrides};
use super::trace_store;
/// and `Jsonl` otherwise, matching the spec's "pretty if tty, jsonl
/// otherwise" default.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DebugFormat {
    #[default]
    Auto,
    Pretty,
    Jsonl,
}

impl DebugFormat {
    /// Resolve `Auto` to a concrete choice based on stdout TTY-ness.
    /// `is_tty` should be the result of `std::io::stdout().is_terminal()`.
    pub fn resolve(self, is_tty: bool) -> DebugFormat {
        match self {
            DebugFormat::Auto => {
                if is_tty { DebugFormat::Pretty } else { DebugFormat::Jsonl }
            }
            other => other,
        }
    }
}

/// 输出格式选择：CLI（ANSI 彩色终端）或 JSON Lines（与前端 TS 协议层对齐）。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    #[default]
    Cli,
    Json,
}

use latte_agent_core::renderer::ChatRenderer;

impl OutputFormat {
    /// 根据输出格式创建对应的渲染器。
    pub fn renderer(&self) -> Box<dyn ChatRenderer> {
        match self {
            OutputFormat::Json => Box::new(super::json_renderer::JsonRenderer::stdout()),
            OutputFormat::Cli => Box::new(super::cli_renderer::CliRenderer::stdout()),
        }
    }
}


/// `latte-agent debug` subcommand set. See spec §8.2.
///
/// Implemented as a struct with a nested `DebugAction` enum so the
/// outer `Command` enum in `main.rs` can hold it as a variant —
/// clap 4 requires the inner subcommand layer to be a struct field
/// (`#[command(subcommand)] action: DebugAction`) when nesting.
#[derive(Args, Debug, Clone)]
pub struct DebugCmd {
    #[command(subcommand)]
    pub action: DebugAction,
}

/// The seven sub-subcommands listed in spec §8.2.
#[derive(Subcommand, Debug, Clone)]
pub enum DebugAction {
    /// Build the real system prompt + history skeleton for a role.
    Prompt {
        /// Role id (e.g. "manager", "programmer").
        #[arg(long)]
        role: String,
        /// Model tier: premium | standard | budget. Default: standard.
        #[arg(long, default_value = "standard")]
        tier: String,
        /// Optional sample user input appended as the last message.
        #[arg(long)]
        input: Option<String>,
        /// Path to agents config (file or directory).
        #[arg(long, default_value = ".latte/agents.d")]
        agents_config: String,
        /// Path to models config.
        #[arg(long, default_value = ".latte/models.d")]
        models_config: String,
    },
    /// Print a recorded session's events, optionally filtered by kind.
    Session {
        /// Session id (e.g. `chat-20260625-054204-73546`).
        id: String,
        /// Filter to one event kind: `ModelCall`, `ToolExec`, `HookFired`, etc.
        #[arg(long, value_name = "KIND")]
        kind: Option<String>,
    },
    /// List every session in `~/.latte/{traces,sessions,logs}/`.
    Sessions,
    /// Aggregate input/output/thinking by turn and role for a session.
    Tokens {
        /// Session id.
        id: String,
    },
    /// One-line-per-event timeline of a session.
    Trace {
        /// Session id.
        id: String,
    },
    /// Re-run parse (and optionally hooks) against stored ModelRawOut events.
    Replay {
        /// Session id.
        id: String,
        /// Parser variant. Only `default` is implemented in v1.
        #[arg(long, value_name = "V")]
        parser: Option<String>,
        /// Comma-separated hooks to re-run against each ModelRawOut.
        #[arg(long, value_delimiter = ',', value_name = "NAMES")]
        hook: Vec<String>,
    },
}

impl DebugCmd {
    pub async fn run(&self) -> Result<(), Box<dyn std::error::Error>> {
        match &self.action {
            DebugAction::Prompt { role, tier, input, agents_config, models_config } => {
                run_prompt(role, tier, input.as_deref(), agents_config, models_config).await
            }
            DebugAction::Session { id, kind } => run_session(id, kind.as_deref()),
            DebugAction::Sessions => run_sessions(),
            DebugAction::Tokens { id } => run_tokens(id),
            DebugAction::Trace { id } => run_trace(id),
            DebugAction::Replay { id, parser, hook } => {
                run_replay(id, parser.as_deref(), hook)
            }
        }
    }
}

// ─── Subcommand implementations ───────────────────────────────────────────


async fn run_prompt(
    role: &str,
    tier: &str,
    input: Option<&str>,
    agents_config: &str,
    models_config: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use latte_ai::params::GenerateParams;

    let resolved = config_layer::load(
        Some(agents_config),
        Some(models_config),
        CliOverrides::default(),
    )?;
    let template = resolved
        .config
        .roles
        .get(role)
        .ok_or_else(|| format!("role '{}' not found in config", role))?;
    let role_def = template.resolve(&GenerateParams::default()).await?;
    let parsed_tier = latte_agent_core::model_resolver::ModelTier::parse(tier)
        .map_err(|e| format!("invalid tier '{}': {}", tier, e))?;
    let vars = serde_json::json!({});
    let system_content = role_def.render_prompt(&vars)?;
    let history_len = 0;
    let est_input_tokens = (system_content.len() + input.unwrap_or("").len()) / 4;
    println!("=== debug prompt: role={}, tier={} ===", role, parsed_tier.label());
    println!("prompt_file:    {}", template.prompt_file.as_deref().unwrap_or("<inline>"));
    println!("allowed_tools:  {:?}", role_def.allowed_tools);
    println!("model_tier:     {}", parsed_tier.label());
    println!("history_len:    {}", history_len);
    println!("est_input_tok:  {}", est_input_tokens);
    println!("\n--- system_prompt ({} chars) ---\n{}",
        system_content.len(), system_content);
    if let Some(inp) = input {
        println!("\n--- sample user_input ---\n{}", inp);
    }
    Ok(())
}

fn run_session(id: &str, kind: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    if !trace_store::is_valid_id(id) {
        return Err(format!("invalid session id '{}'", id).into());
    }
    let events = trace_store::load_trace(id)
        .or_else(|_| {
            // Fall back to the index when there's no full trace
            // (sessions run without --debug still have a .idx).
            let idx = trace_store::load_index(id)?;
            Ok::<_, trace_store::TraceStoreError>(
                idx.into_iter().map(|l| l.to_trace_event_placeholder()).collect()
            )
        })?;
    let events = trace_store::filter_by_kind(events, kind);
    println!("=== session {} ===  {} event(s){}",
        id, events.len(),
        kind.map(|k| format!(" (kind={})", k)).unwrap_or_default());
    for ev in &events {
        println!("\n[{}] turn={} role={} ts={}",
            ev.variant_name(), ev.meta().turn, ev.meta().role, ev.meta().ts);
        println!("  {}", ev.body_for_pretty().replace('\n', "\n  "));
    }
    Ok(())
}

fn run_sessions() -> Result<(), Box<dyn std::error::Error>> {
    let sessions = trace_store::list_sessions();
    if sessions.is_empty() {
        println!("(no sessions found under {})",
            trace_store::latte_home()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<no latte home>".to_string()));
        return Ok(());
    }
    println!("{:<40} {:<6} {:<6} {:<6} {}", "ID", "LOG", "IDX", "JSONL", "BYTES");
    println!("{}", "-".repeat(72));
    for s in &sessions {
        println!("{:<40} {:<6} {:<6} {:<6} {}",
            s.id,
            yes_no(s.has_log),
            yes_no(s.has_index),
            yes_no(s.has_trace),
            s.size_bytes);
    }
    println!("\n{} session(s)", sessions.len());
    Ok(())
}

fn run_tokens(id: &str) -> Result<(), Box<dyn std::error::Error>> {
    if !trace_store::is_valid_id(id) {
        return Err(format!("invalid session id '{}'", id).into());
    }
    let events = trace_store::load_trace(id)
        .or_else(|_| {
            let idx = trace_store::load_index(id)?;
            Ok::<_, trace_store::TraceStoreError>(
                idx.into_iter().map(|l| l.to_trace_event_placeholder()).collect()
            )
        })?;
    let agg = trace_store::aggregate_tokens(&events);
    println!("=== tokens: session {} ===", id);
    println!("{:<6} {:<12} {:>10} {:>10} {:>10}", "TURN", "ROLE", "IN", "OUT", "THINK");
    println!("{}", "-".repeat(54));
    for row in &agg.per_turn {
        println!("{:<6} {:<12} {:>10} {:>10} {:>10}",
            row.turn, row.role, row.input, row.output, row.thinking);
    }
    println!("{}", "-".repeat(54));
    println!("{:<6} {:<12} {:>10} {:>10} {:>10}",
        "", "TOTAL", agg.total_input, agg.total_output, agg.total_thinking);
    Ok(())
}

fn run_trace(id: &str) -> Result<(), Box<dyn std::error::Error>> {
    if !trace_store::is_valid_id(id) {
        return Err(format!("invalid session id '{}'", id).into());
    }
    let events = trace_store::load_trace(id)?;
    let timeline = trace_store::format_timeline(&events);
    if timeline.is_empty() {
        println!("(no events in {})", id);
    } else {
        print!("{}", timeline);
    }
    Ok(())
}

fn run_replay(
    id: &str,
    parser: Option<&str>,
    hooks: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    use latte_agent_core::hooks::builtin::{
        EnforceToolAllowlist, RedactPii, RequireToolCall,
    };
    use latte_agent_core::hooks::HookChain;
    use latte_agent_core::hooks::Hook;
    use std::sync::Arc;

    if !trace_store::is_valid_id(id) {
        return Err(format!("invalid session id '{}'", id).into());
    }
    let parser_variant = parser.unwrap_or("default");
    if parser_variant != "default" {
        return Err(format!("parser variant '{}' not implemented in v1 (only 'default')", parser_variant).into());
    }
    let events = trace_store::load_trace(id)?;

    // Build a hook chain from the requested names.
    let resolved_hooks: Vec<Arc<dyn Hook>> = hooks.iter()
        .filter_map(|raw| resolve_hook_name(raw))
        .collect();

    let mut tool_call_count = 0usize;
    let mut hook_runs = 0usize;
    let mut hook_aborts = 0usize;
    for ev in &events {
        if let TraceEvent::ParseToolCalls { parsed, .. } = ev {
            // native protocol：ParseToolCalls 的 parsed 就是实际的工具调用列表。
            tool_call_count += 1;
            // Run hooks against the parsed calls (best-effort).
            for h in &resolved_hooks {
                if h.name() == "enforce_tool_allowlist" {
                    let chain = HookChain::empty()
                        .push(Arc::new(EnforceToolAllowlist::from(vec!["read", "search", "bash", "write", "delegate"])));
                    let mut p = parsed.clone();
                    let mut ctx = latte_agent_core::hooks::PostParseCtx { parsed: &mut p };
                    let outcome = chain.run_post_parse(&mut ctx, |_, _, _| {});
                    hook_runs += 1;
                    if matches!(outcome, latte_agent_core::hooks::HookOutcome::Abort { .. }) {
                        hook_aborts += 1;
                    }
                } else if h.name() == "require_tool_call" {
                    // require_tool_call 需要原始 model raw out，无 tool_calls 时跳过。
                }
            }
        }
        // redact_pii hook：需要 PreCall 级别的 raw text 才能运行。
        // native 下 ModelRawOut 不再有 <tool_call> 文本，但 PreCall
        // hook 仍然可用；这里留空因为 run_replay 没有完整的 message 上下文。
    }
    println!("=== replay: session {} (native tool calls, hooks=[{}]) ===",
        id, hooks.join(","));
    println!("ParseToolCalls events: {}", tool_call_count);
    println!("Hook runs:            {} (aborts: {})", hook_runs, hook_aborts);
    Ok(())
}

// ─── helpers ─────────────────────────────────────────────────────────────

fn yes_no(b: bool) -> &'static str { if b { "yes" } else { "no" } }
fn pct(num: usize, denom: usize) -> usize {
    if denom == 0 { 0 } else { (num * 100) / denom }
}
/// Map a CLI hook name to a built-in hook instance. Returns `None`
/// for unknown names so `debug replay` reports them but continues
/// running the rest.
pub fn resolve_hook_name(name: &str) -> Option<Arc<dyn latte_agent_core::hooks::Hook>> {
    use latte_agent_core::hooks::builtin::{
        EnforceToolAllowlist, RedactPii, RequireToolCall,
    };
    match name.trim() {
        "redact_pii" => Some(Arc::new(RedactPii)),
        "enforce_tool_allowlist" => Some(Arc::new(EnforceToolAllowlist::from(
            vec!["read", "search", "exec", "write", "delegate"],
        ))),
        "require_tool_call" => Some(Arc::new(RequireToolCall::default())),
        other if other.starts_with("no:") => None, // explicit disable, no-op
        other => {
            eprintln!("debug replay: unknown hook name '{}' (skipped)", other);
            None
        }
    }
}

/// Apply a comma-separated `--debug-hooks` list. Names not in the
/// built-in table are reported on stderr and skipped.
pub fn debug_hook_names_to_arcs(
    raw: &str,
) -> Vec<Arc<dyn latte_agent_core::hooks::Hook>> {
    raw.split(',').filter_map(|s| resolve_hook_name(s)).collect()
}

/// `IndexLine` doesn't carry the full event payload, so when the
/// caller falls back to the index (no `--debug` was used), we
/// synthesize a placeholder event with the metadata. This is enough
/// for `debug session` to list the kinds/turns/roles, even though
/// the body is empty.
trait IndexLineExt {
    fn to_trace_event_placeholder(self) -> TraceEvent;
}
impl IndexLineExt for latte_agent_core::trace::IndexLine {
    fn to_trace_event_placeholder(self) -> TraceEvent {
        use latte_agent_core::trace::{HookPoint, ToolStatus, TraceEvent, TraceMeta};
        let meta = TraceMeta {
            turn: self.turn, role: self.role, ts: self.ts, session_id: String::new(),
        };
        match self.kind.as_str() {
            "SessionStart" => TraceEvent::SessionStart {
                meta, tier: String::new(), model_chain: Vec::new(), allowed_tools: Vec::new(),
            },
            "PromptBuilt" => TraceEvent::PromptBuilt {
                meta, system_rendered: String::new(), history_len: 0,
                user_input: String::new(), est_input_tokens: self.tokens_in.unwrap_or(0),
            },
            "ModelCall" => TraceEvent::ModelCall {
                meta, model_id: self.model_id.unwrap_or_default(),
                params_json: String::new(), latency_ms: self.latency_ms.unwrap_or(0),
                finish_reason: String::new(),
            },
            "ModelRawOut" => TraceEvent::ModelRawOut { meta, raw_content: String::new() },
            "ParseToolCalls" => TraceEvent::ParseToolCalls {
                meta, raw_in: String::new(), parsed: Vec::new(),
                diagnostics: Default::default(),
            },
            "ToolExec" => TraceEvent::ToolExec {
                meta, name: self.detail.clone(), args_json: String::new(),
                latency_ms: self.latency_ms.unwrap_or(0),
                status: ToolStatus::Ok(String::new()),
            },
            "HookFired" => TraceEvent::HookFired {
                meta, hook_name: String::new(), point: HookPoint::PreCall,
                outcome_kind: self.detail.clone(),
            },
            "TurnEnd" => TraceEvent::TurnEnd {
                meta,
                total_input: self.tokens_in.unwrap_or(0),
                total_output: self.tokens_out.unwrap_or(0),
                total_thinking: self.tokens_think.unwrap_or(0),
                elapsed_ms: self.latency_ms.unwrap_or(0),
            },
            "SessionEnd" => TraceEvent::SessionEnd {
                meta,
                total_turns: self.turn,
                total_input: self.tokens_in.unwrap_or(0),
                total_output: self.tokens_out.unwrap_or(0),
                total_thinking: self.tokens_think.unwrap_or(0),
            },
            // Unknown variant name in the index — degrade to a
            // ModelRawOut placeholder so the kind still appears in
            // the session listing.
            _ => TraceEvent::ModelRawOut { meta, raw_content: format!("<unknown kind: {}>", self.kind) },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_with_two_canonical_calls() {
        let text = r#"Some prose
<tool_call>read {"path": "src/main.rs"}</tool_call>
middle
<tool_call>search {"pattern": "TODO"}</tool_call>
end"#;
        let (parsed, diag) = parse_tool_calls(text);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name, "read");
        assert_eq!(diag.opens_found, 2);
        assert_eq!(diag.closes_matched, 2);
        assert!(diag.unmatched_opens.is_empty());
    }

    #[test]
    fn parse_reports_unmatched_when_open_has_no_close() {
        let text = r#"<tool_call>read {"path":"x"}</tool_call>
<tool_call>list {"path":"."}
<no close here>"#;
        let (parsed, diag) = parse_tool_calls(text);
        assert_eq!(parsed.len(), 1, "only one well-formed call expected");
        assert_eq!(diag.opens_found, 2);
        assert_eq!(diag.closes_matched, 1);
        assert_eq!(diag.unmatched_opens.len(), 1, "second open has no close");
        assert!(diag.unmatched_opens[0].starts_with("<tool_call"));
    }

    #[test]
    fn debug_format_resolves_auto() {
        assert_eq!(DebugFormat::Auto.resolve(true), DebugFormat::Pretty);
        assert_eq!(DebugFormat::Auto.resolve(false), DebugFormat::Jsonl);
        assert_eq!(DebugFormat::Pretty.resolve(false), DebugFormat::Pretty);
        assert_eq!(DebugFormat::Jsonl.resolve(true), DebugFormat::Jsonl);
    }

    #[test]
    fn debug_hooks_parses_known_and_unknown() {
        let arcs = debug_hook_names_to_arcs("redact_pii,enforce_tool_allowlist,bogus_hook");
        assert_eq!(arcs.len(), 2, "bogus_hook should be skipped");
        assert_eq!(arcs[0].name(), "redact_pii");
        assert_eq!(arcs[1].name(), "enforce_tool_allowlist");
    }

    #[test]
    fn debug_hooks_resolves_all_three_builtins() {
        let arcs = debug_hook_names_to_arcs("redact_pii,enforce_tool_allowlist,require_tool_call");
        let names: Vec<&str> = arcs.iter().map(|h| h.name()).collect();
        assert_eq!(names, vec!["redact_pii", "enforce_tool_allowlist", "require_tool_call"]);
    }
}
