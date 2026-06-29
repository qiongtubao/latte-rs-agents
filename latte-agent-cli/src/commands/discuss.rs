//! `latte-agent discuss` — run a multi-agent discussion.

use std::collections::HashMap;

use clap::Args;
use latte_agent_core::agent::{Agent, AgentRunner};
use latte_agent_orchestrator::orchestrator::{DiscussionConfig, DiscussionOrchestrator};
use latte_agent_orchestrator::ConsensusMethod;
use latte_agent_orchestrator::DiscussionWorkflow;
use latte_ai::params::GenerateParams;

use super::config_layer::{self, CliOverrides};

type AnyResult = Result<(), Box<dyn std::error::Error>>;

/// Run a multi-agent discussion.
#[derive(Args, Debug)]
pub struct DiscussCmd {
    /// Discussion topic.
    #[arg(short, long)]
    pub topic: String,

    /// Comma-separated list of roles (e.g. "pm,architect,programmer").
    #[arg(short, long, value_delimiter = ',')]
    pub roles: Vec<String>,

    /// Workflow name to use (from discussion.toml).
    #[arg(short, long)]
    pub workflow: Option<String>,

    /// Path to agents config TOML (file or directory).
    #[arg(long, default_value = ".latte/agents.d")]
    pub agents_config: String,

    /// Path to models config TOML.
    #[arg(long, default_value = ".latte/models.d")]
    pub models_config: String,

    /// Path to discussion workflow TOML (file or directory).
    #[arg(long, default_value = ".latte/workflows.d")]
    pub discussion_config: String,

    /// Maximum discussion rounds.
    #[arg(long)]
    pub max_rounds: Option<usize>,

    /// Consensus method: majority, none, weighted.
    #[arg(long, default_value = "none")]
    pub consensus: String,

    /// Override the api_key for a model. With `--model`, only that model is
    /// touched; otherwise every model that has an empty api_key is filled.
    #[arg(long, value_name = "KEY")]
    pub api_key: Option<String>,

    /// Restrict `--api-key` to a single model id.
    #[arg(long, value_name = "ID", requires = "api_key")]
    pub model: Option<String>,

    /// Per-model field override, repeatable: `id.field=value`.
    #[arg(long = "model-override", value_name = "ID.FIELD=VALUE")]
    pub model_overrides: Vec<String>,

    /// Enable full-chain observability (see `latte-agent chat --debug`).
    #[arg(long)]
    pub debug: bool,

    /// Format for the on-stdout debug stream.
    #[arg(long, value_enum, default_value_t = super::debug::DebugFormat::Auto)]
    pub debug_format: super::debug::DebugFormat,

    /// Comma-separated list of built-in hook names.
    #[arg(long, value_delimiter = ',', default_value = "")]
    pub debug_hooks: Vec<String>,

    /// Filter the on-stdout `--debug` stream to a comma-separated list
    /// of `TraceEvent` variant names. Default is `all` (no filter).
    #[arg(long, value_name = "NAMES|all", default_value = "all")]
    pub debug_events: String,

    /// Opt out of the always-on metadata index.
    #[arg(long)]
    pub no_session_index: bool,
}

impl DiscussCmd {
    pub async fn run(&self) -> AnyResult {
        println!("=== latte-agent discussion ===");
        println!("Topic: {}", self.topic);
        println!("Roles: {:?}", self.roles);
        println!();
        // 1. Three-layer config: global (~/.latte/) ← project (CLI flags) ← CLI overrides.
        let cli = build_cli_overrides(self)?;
        let resolved = config_layer::load(
            Some(&self.agents_config),
            Some(&self.models_config),
            cli,
        )
        .map_err(|e| format!("failed to load configuration: {}", e))?;
        let merged = resolved.config;
        let resolver = resolved.resolver;

        // 2. Resolve roles and create agents
        let default_params = GenerateParams::default();
        let mut agents: HashMap<String, AgentRunner> = HashMap::new();

        // Synthesize a session_id for this discussion. Discuss has no
        // per-session ChatLog file to anchor on (it's not the REPL path),
        // so we mint a `discuss-YYYYMMDD-HHMMSS-PID` id that follows
        // the same shape as the chat session ids and so lines up with
        // `~/.latte/sessions/<id>.idx` and `~/.latte/traces/<id>.jsonl`.
        // One id for the whole discussion, shared across every role's
        // runner, so all events from the discussion correlate.
        let session_id: String = format!(
            "discuss-{}-{}",
            chat_timestamp_compact(),
            std::process::id(),
        );
        let debug_flags = super::DebugFlags {
            debug: self.debug,
            debug_format: self.debug_format,
            debug_hooks: self.debug_hooks.clone(),
            no_session_index: self.no_session_index,
            debug_events: if self.debug_events.eq_ignore_ascii_case("all") {
                None
            } else {
                Some(self.debug_events.clone())
            },
            session_id: session_id.clone(),
        };

        for role_name in &self.roles {
            let template = merged
                .roles
                .get(role_name)
                .ok_or_else(|| format!("role '{}' not found in config", role_name))?;
            let role = template.resolve(&default_params).await?;
            let tier = role.default_model_tier;
            // Resolve the full model chain (primary + fallbacks) so the
            // agent can walk the priority list when the primary is
            // rate-limited or otherwise unavailable.
            let models = resolver.resolve_chain(&role.id, tier, &role.model_chain)?;
            let model_id = models[0].id.clone();

            // Pass the orchestrator's session_id, not the role_name, so
            // all events from this discussion land in the same trace
            // file and session index.
            let sink = super::build_debug_sink(&debug_flags.session_id, &debug_flags);
            let hooks = super::build_debug_hooks(&debug_flags);
            let agent = Agent::new_with_chain(role_name.clone(), role, models, default_params.clone())?;
            let runner = AgentRunner::new(agent)
                .with_sink(sink)
                .with_hooks(hooks)
                .with_role(role_name.clone())
                .with_session_id(debug_flags.session_id.clone());
            // Emit SessionStart trace event for this role. Each role
            // gets its own SessionStart so the trace can be filtered
            // by role via `latte-agent debug trace --role <name>`.
            runner.emit_session_start(&tier.label());

            agents.insert(role_name.clone(), runner);
            println!("  {} ({}) → {}", role_name, template.model_tier, model_id);
        }
        println!();

        // 4. Load or build workflow
        let workflow = if let Some(ref wf_name) = self.workflow {
            self.load_named_workflow(wf_name)?
        } else {
            self.build_default_workflow()
        };

        // 5. Create orchestrator config
        let mut variables = HashMap::new();
        variables.insert("topic".into(), self.topic.clone());

        let consensus = ConsensusMethod::parse(&self.consensus)
            .unwrap_or(ConsensusMethod::NoConsensus);

        let config = DiscussionConfig {
            workflow,
            consensus,
            max_rounds: self.max_rounds,
            context_token_budget: 32000,
            variables,
        };

        // 6. Run discussion
        println!("Starting discussion ({} round(s))...\n", config.effective_max_rounds());

        let mut orchestrator = DiscussionOrchestrator::new(agents, config)?;
        let result = orchestrator.run().await?;
        // Emit SessionEnd for every role. The orchestrator kept the
        // runners (see orchestrator.agents()); we use the total
        // round count as a stand-in for per-role turn count.
        // Operators can re-derive per-role turn counts from the
        // TurnEnd events in the trace.
        let rounds = result.rounds.len() as u32;
        for (_role, runner) in orchestrator.agents() {
            runner.emit_session_end(rounds);
        }

        // 7. Print results
        println!("\n=== Discussion Complete ===");
        println!("Rounds: {}", result.rounds.len());
        println!("Consensus: {}", if result.consensus_reached { "YES" } else { "NO" });
        println!(
            "Tokens: {} in / {} out",
            result.total_usage.input_tokens, result.total_usage.output_tokens
        );
        println!();
        println!("{}", orchestrator.transcript());

        Ok(())
    }

    fn build_default_workflow(&self) -> DiscussionWorkflow {
        let steps = self
            .roles
            .iter()
            .enumerate()
            .map(|(i, role)| latte_agent_orchestrator::WorkflowStep {
                id: format!("step_{}", i),
                description: format!("{} speaks", role),
                speakers: vec![role.clone()],
                prompt: format!(
                    "You are discussing: {{topic}}. Share your perspective as a {}.",
                    role
                ),
                contract: None,
                hooks: vec![],
                output_key: None,
            })
            .collect();

        DiscussionWorkflow {
            name: "default".into(),
            description: "Auto-generated from roles".into(),
            steps,
            max_rounds: 1,
            context_token_budget: 32000,
        }
    }

    fn load_named_workflow(&self, name: &str) -> Result<DiscussionWorkflow, Box<dyn std::error::Error>> {
        let registry = latte_agent_orchestrator::WorkflowRegistry::load_with_global(Some(&self.discussion_config))
            .map_err(|e| format!("failed to load workflows: {}", e))?;
        registry.resolve(Some(name)).map_err(|e| e.into())
    }
}

fn build_cli_overrides(cmd: &DiscussCmd) -> Result<CliOverrides, String> {
    let mut out = CliOverrides {
        api_key: cmd.api_key.clone(),
        api_key_target: cmd.model.clone(),
        field_overrides: Vec::with_capacity(cmd.model_overrides.len()),
    };
    for raw in &cmd.model_overrides {
        let (id, field, value) = super::chat::parse_model_override(raw)
            .map_err(|e| format!("invalid --model-override '{}': {}", raw, e))?;
        out.field_overrides.push((id, field, value));
    }
    Ok(out)
}

/// Format the current UTC time as `YYYYMMDD-HHMMSS` (compact form, no
/// separators). Used to mint the discuss session_id so it matches the
/// shape of the chat log's file stem. We avoid pulling chrono in; this
/// is good enough for trace-file naming and is sortable
/// lexicographically. Mirrors `chatlog::iso_local_date_time` so the
/// two commands produce visually similar timestamps.
fn chat_timestamp_compact() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = epoch_to_ymdhms(secs);
    format!("{:04}{:02}{:02}-{:02}{:02}{:02}", y, mo, d, h, mi, s)
}

/// Tiny proleptic-Gregorian epoch-to-calendar converter. Lifted
/// verbatim from `chatlog::epoch_to_ymdhms` because discuss doesn't
/// open a chat log file (so it can't borrow it from there) and the
/// logic is small enough to duplicate.
fn epoch_to_ymdhms(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let s = (secs % 60) as u32;
    let mins = (secs / 60) as u32;
    let mi = mins % 60;
    let hours = mins / 60;
    let h = hours % 24;
    let mut days = (hours / 24) as i64;
    let mut year = 1970i64;
    loop {
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        let dy = if leap { 366 } else { 365 };
        if days >= dy {
            days -= dy;
            year += 1;
        } else {
            break;
        }
    }
    let month_lens = [31, 28, 31, 30, 31, 30, 31, 31, 31, 30, 31, 30];
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

#[cfg(test)]
mod tests {
    #[test]
    fn epoch_zero_is_1970_01_01_00_00_00() {
        assert_eq!(super::epoch_to_ymdhms(0), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn epoch_one_day_is_1970_01_02() {
        assert_eq!(super::epoch_to_ymdhms(86_400), (1970, 1, 2, 0, 0, 0));
    }

    #[test]
    fn epoch_one_year_is_1971() {
        // 1970 is not a leap year; 365 * 86400 = 31_536_000.
        assert_eq!(super::epoch_to_ymdhms(31_536_000), (1971, 1, 1, 0, 0, 0));
    }

    #[test]
    fn leap_year_handling() {
        // 1972 is a leap year; 1971-01-01 + 365d = 1972-01-01 (we went
        // through the leap day, so it should still be Jan 1).
        assert_eq!(super::epoch_to_ymdhms(63_072_000), (1972, 1, 1, 0, 0, 0));
    }

    #[test]
    fn chat_timestamp_compact_has_expected_shape() {
        let s = super::chat_timestamp_compact();
        assert_eq!(s.len(), 15); // YYYYMMDD-HHMMSS
        assert_eq!(s.as_bytes()[8], b'-');
        for (i, c) in s.chars().enumerate() {
            if i == 8 { continue; }
            assert!(c.is_ascii_digit(), "expected digit at position {} in {}", i, s);
        }
    }
}
