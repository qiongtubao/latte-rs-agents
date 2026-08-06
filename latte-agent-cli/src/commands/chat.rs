//! `latte-agent chat` — single-role REPL.
//!
//! Runs an interactive chat session with one agent (selected by role id
//! or falling back to "manager"). Built-in slash commands switch roles,
//! switch model tier, clear context, and save/load sessions.

use std::io::{self, BufRead, BufWriter, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::time::timeout as tokio_timeout;
use latte_agent_core::session::SessionManager;

/// Max concurrent `delegate` tool calls per manager session.
const DEFAULT_DELEGATE_CONCURRENCY: usize = 4;
/// Per-specialist wall-clock timeout in seconds.
const DEFAULT_DELEGATE_TIMEOUT_SECS: u64 = 300;

use clap::Args;
use latte_agent_core::agent::{Agent, AgentRunner};
use latte_agent_core::config::AgentConfig;
use latte_agent_core::model_resolver::{ModelResolver, ModelTier};
use latte_agent_core::scheduler::{plan_md_slice_for, RoundScheduler};
use latte_agent_core::supervisor::{Supervisor, SupervisorConfig};
use latte_ai::models::{Message, Role as MsgRole};
use latte_ai::params::GenerateParams;

use super::config_layer::{self, CliOverrides};
use super::style;
type AnyResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// Start an interactive REPL chat session with a single agent.
#[derive(Args, Debug)]
pub struct ChatCmd {
    #[arg(short, long)]
    pub role: Option<String>,

    /// Initial model tier: premium | standard | budget. Short form: `-t`.
    /// When omitted, the role's `model_tier` from its TOML config is
    /// used. This lets reviewer roles (sanity/architecture/security)
    /// ship with their own tier defaults instead of forcing every
    /// role through `standard`.
    #[arg(short = 't', long)]
    pub tier: Option<String>,

    /// Pin the chat to a specific model id (overrides the tier-based
    /// resolution). The model must exist in the merged catalog
    /// (project + global `~/.latte/models.yaml`); useful when you want
    /// to force a known-working model without editing the project
    /// config. Short form: `-m`.
    ///
    /// Example: `latte-agent chat -m deepseek-v4-flash`
    #[arg(short = 'm', long = "model-id", value_name = "ID")]
    pub model_id: Option<String>,

    /// Resume from a previously saved session file (written by
    /// /save <path> or by the auto-save on turn failure).
    #[arg(long, value_name = "PATH")]
    pub resume: Option<String>,

    /// Path to agents config (file or directory).
    #[arg(long, default_value = ".latte/agents.d")]
    pub agents_config: String,

    /// Path to models config.
    #[arg(long, default_value = ".latte/models.d")]
    pub models_config: String,

    /// Override the api_key for a model. With `--model`, only that model is
    /// touched; otherwise every model that has an empty api_key is filled.
    #[arg(long, value_name = "KEY")]
    pub api_key: Option<String>,

    /// Restrict `--api-key` to a single model id.
    #[arg(long, value_name = "ID", requires = "api_key")]
    pub model: Option<String>,

    /// Per-model field override, repeatable: `id.field=value`. Examples:
    /// `--model-override claude-sonnet-4-20250514.api_key=sk-...`
    /// `--model-override deepseek-chat.base_url=http://localhost:11434`
    #[arg(long = "model-override", value_name = "ID.FIELD=VALUE")]
    pub model_overrides: Vec<String>,

    /// Enable full-chain observability (TraceSink + HookChain).
    /// Emits TraceEvents to stdout AND writes a complete trace to
    /// `~/.latte/traces/<session-id>.jsonl`. Default is enabled so
    /// the full request/response pipeline is visible during chat;
    /// pass `--debug false` to suppress per-turn event logs and keep
    /// only the always-on `~/.latte/sessions/<id>.idx` metadata index.
    #[arg(long, default_value_t = true)]
    pub debug: bool,

    /// Format for the on-stdout debug stream when `--debug` is set.
    /// `auto` (default) picks pretty on a tty and jsonl when piped.
    #[arg(long, value_enum, default_value_t = super::debug::DebugFormat::Auto)]
    pub debug_format: super::debug::DebugFormat,

    /// Comma-separated list of built-in hook names to register for
    /// this session. Available: `redact_pii`, `enforce_tool_allowlist`,
    /// `require_tool_call`.
    #[arg(long, value_delimiter = ',', default_value = "")]
    pub debug_hooks: Vec<String>,

    /// Filter the on-stdout `--debug` stream to a comma-separated list
    /// of `TraceEvent` variant names (e.g. `ToolExec,HookFired,ModelCall`).
    /// Default is `all` (no filter — every event lands on stdout).
    /// The JSONL trace and session index stay unfiltered regardless.
    #[arg(long, value_name = "NAMES|all", default_value = "all")]
    pub debug_events: String,

    /// Opt out of the always-on metadata index. Useful on shared
    /// machines where the file's contents are sensitive.
    #[arg(long)]
    pub no_session_index: bool,

    /// Task ID for the HIL blackboard session. When set, the chat
    /// runs in worktree mode and the REPL supports `/pause` + `@<role>`.
    /// When absent, the legacy single-role REPL is used.
    #[arg(long, value_name = "ID")]
    pub task_id: Option<String>,

    /// Comma-separated list of role ids participating in the
    /// HIL session. Defaults to "manager" (i.e. only the manager).
    /// Used together with `--task-id` and `--initial-prompt`.
    #[arg(long, value_delimiter = ',', default_value = "manager")]
    pub roles: Vec<String>,

    /// Initial prompt written to plan.md and used as the manager's
    /// first user message. Required when `--task-id` is given and no
    /// existing session JSON is found; ignored otherwise.
    #[arg(long)]
    pub initial_prompt: Option<String>,

    /// Maximum round-robin rounds (default 10). 0 = manager-dispatch
    /// only (v1 compat).
    #[arg(long, default_value_t = 10)]
    pub max_rounds: u32,

    /// Session-level token budget for the Supervisor. 0 disables
    /// the token trigger (dead-loop still active).
    #[arg(long, default_value_t = 50_000)]
    pub session_token_budget: u32,

    /// Disable registration of the `ask_human` tool for non-manager
    /// roles. Escape hatch for users who don't want pauses.
    #[arg(long)]
    pub no_ask_human: bool,

    /// Output format: cli (ANSI colors, default) or json (JSON Lines).
    /// JSON format matches the TS ChatProtocolMessage interface.
    #[arg(long, value_enum, default_value_t = super::debug::OutputFormat::Cli)]
    pub output: super::debug::OutputFormat,
}
impl ChatCmd {
    pub async fn run(&self) -> AnyResult {
        // HIL blackboard mode: when --task-id is given, drive the
        // session from the SessionManager JSON instead of the
        // legacy single-role REPL.
        if let Some(task_id) = &self.task_id {
            return run_hil_chat(
                self,
                task_id.clone(),
                self.roles.clone(),
                self.initial_prompt.clone(),
                self.max_rounds,
                self.session_token_budget,
                self.no_ask_human,
                self.output.renderer().as_ref(),
            )
            .await;
        }
        // Per-session log file (also echoed to stderr). Always on —
        // the whole point of `-m` + the resolver work is to debug
        // "why did this fall through to that model", and a single
        // tail-friendly file beats a heap of `tracing::debug!` lines
        // the user has to set RUST_LOG=trace to see.
        let log = super::chatlog::ChatLog::open();
        if let Some(p) = log.path() {
            eprintln!("[chatlog] writing session events to {}", p.display());
        }
        // Derive the canonical session_id from the ChatLog file stem
        // (`chat-YYYYMMDD-HHMMSS-<pid>` without the `.log` suffix).
        // This is what spec §9 calls the "log ↔ idx ↔ jsonl correlate"
        // anchor — every TraceMeta emitted by the runner carries this
        // id, the per-session index sits at `~/.latte/sessions/<id>.idx`,
        // the trace JSONL sits at `~/.latte/traces/<id>.jsonl`, and the
        // chat log sits at `~/.latte/logs/<id>.log`. Fall back to a
        // PID-based id if the log couldn't be opened (e.g. the logs
        // directory is read-only) so the trace files are still useful.
        let session_id: String = log
            .path()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()))
            .unwrap_or_else(|| format!("chat-{}", std::process::id()));
        log.info(
            "session start",
            &[
                ("role", self.role.clone().unwrap_or_else(|| "manager".into())),
                ("tier", self.tier.clone().unwrap_or_else(|| "auto".into())),
                ("model_id", self.model_id.clone().unwrap_or_default()),
                ("session_id", session_id.clone()),
            ],
        );

        let cli = build_cli_overrides(self)?;
        let resolved = config_layer::load(
            Some(&self.agents_config),
            Some(&self.models_config),
            cli,
        )
        .map_err(|e| {
            log.error("config load failed", &[("error", e.to_string())]);
            format!("failed to load configuration: {}", e)
        })?;
        // Emit the "config loaded" block BEFORE moving the fields
        // out of `resolved`. The block lists every file the loader
        // touched, then the per-role / per-model reverse map so the
        // operator can see "this role came from this file" without
        // digging through the agent TOML.
        emit_config_loaded(&log, &resolved.sources);
        let merged = resolved.config;
        let resolver = resolved.resolver;
        let default_params = GenerateParams::default();
        let initial_role = self.role.clone().unwrap_or_else(|| "manager".into());
        // Tier resolution: explicit --tier flag wins, otherwise the
        // role's `model_tier` from its TOML config. This is the
        // fix for "reviewer_sanity stays at standard instead of
        // budget" — without this, every role defaulted to standard
        // regardless of what the role said.
        let initial_tier = if let Some(t) = &self.tier {
            parse_tier(t)?
        } else {
            let template = merged.roles.get(&initial_role).ok_or_else(|| {
                format!("role '{}' not found in config", initial_role)
            })?;
            parse_tier(&template.model_tier)?
        };
        let initial_primary = self.model_id.as_deref();
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

        // Build the initial runner.
        let (runner, role_id) = match build_runner(
            &merged,
            &resolver,
            &default_params,
            &initial_role,
            initial_tier,
            initial_primary,
            &debug_flags,
            // Legacy single-role REPL — no HIL session.
            None,
            &std::env::current_dir()?,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                log.error("build_runner failed", &[
                    ("role", initial_role.clone()),
                    ("tier", initial_tier.label().to_string()),
                    ("model_id", initial_primary.unwrap_or("").to_string()),
                    ("error", e.to_string()),
                ]);
                return Err(e);
            }
        };
        log.info(
            "runner built",
            &[
                ("role", role_id.clone()),
                ("tier", initial_tier.label().to_string()),
                ("model_id", initial_primary.unwrap_or("").to_string()),
                ("session_id", session_id.clone()),
            ],
        );
        // Emit SessionStart trace event now that the runner is
        // fully built. The sink + session_id were wired via
        // build_runner earlier. `tier` is the initial_tier label
        // (premium | standard | budget) so the trace carries the
        // same model-tier metadata the chat log records.
        runner.emit_session_start(&initial_tier.label());
        let mut session = ChatSession {
            merged,
            resolver,
            default_params: default_params.clone(),
            runner,
            role_id,
            tier: initial_tier,
            primary_id: self.model_id.clone(),
            log: Some(log),
            last_response: None,
            turn_count: 0,
            debug_flags: debug_flags.clone(),
            renderer: self.output.renderer(),
        };
        // If --resume was passed, load the saved history into the
        // session context before the user starts chatting.
        if let Some(path) = &self.resume {
            let history = load_session(path)
                .map_err(|e| format!("failed to load resume file '{}': {}", path, e))?;
            session.load_history(history);
            session.renderer.on_status(&format!("[resume] loaded {} messages from {}", session.runner.context().messages().len(), path)).await;
        }
        if !io::stdout().is_terminal() {
            // Non-interactive: read one message from stdin and reply once.
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            let input = input.trim_end_matches(['\n', '\r']);
            if !input.is_empty() {
                session.turn(input).await?;
                if let Some(resp) = &session.last_response {
                    println!("{}", resp);
                }
            }
            return Ok(());
        }

        let stdin = io::stdin();
        let mut stdout = io::stdout();
        let term_width = style::terminal_width();
        loop {
            let model_id = session.primary_model_id();
            let role_icon = session.role_icon();
            // Prompt: `👔 manager · deepseek-v4-flash › ` (colorized when TTY).
            print!("{}", style::render_prompt(role_icon, &session.role_id, model_id));
            stdout.flush()?;

            let mut line = String::new();
            let n = stdin.lock().read_line(&mut line)?;
            if n == 0 {
                println!();
                break;
            }

            if line.trim().is_empty() {
                continue;
            }
            if line.starts_with('/') {
                if session.handle_command(&line).await? {
                    break;
                }
                continue;
            }
            let usage_before = session.runner.total_usage().clone();
            if let Err(e) = session.turn(&line).await {
                // Auto-save on turn failure so the user can resume
                // with `latte-agent chat --resume <path>` after the
                let saved = session.save_to_default();
                eprintln!("\n[auto-save] turn failed: {}", e);
                match saved {
                    Ok(path) => eprintln!(
                        "[auto-save] session saved to {}\n[auto-save] resume later: latte-agent chat --resume {}",
                        path.display(), path.display()
                    ),
                    Err(save_err) => eprintln!(
                        "[auto-save] could not save session: {}",
                        save_err
                    ),
                }
                return Err(e.into());
            }
            // Render the response after a successful turn. The
            // non-tty path above already does this; the tty REPL
            // was missing it, so the synthesis was stored in
            // `last_response` but never displayed. The user would
            // only see log entries (truncated to 400 chars for
            // preview) and the next prompt.
            if let Some(resp) = &session.last_response {
                if std::io::stdout().is_terminal() {
                    let body = style::render_response_box(
                        &session.role_icon(),
                        &session.role_id,
                        resp,
                        style::terminal_width(),
                    );
                    print!("{}", body);
                } else {
                    println!("{}", resp);
                }
                stdout.flush()?;
            }
            let usage_after = session.runner.total_usage();
            let in_delta = usage_after.input_tokens - usage_before.input_tokens;
            let out_delta = usage_after.output_tokens - usage_before.output_tokens;
        }
        Ok(())
    }
}

struct ChatSession {
    merged: AgentConfig,
    resolver: ModelResolver,
    default_params: GenerateParams,
    runner: AgentRunner,
    role_id: String,
    tier: ModelTier,
    /// Pinned model id from `-m` / `--model-id`. When `Some`, it is
    /// used as the chain head on every rebuild (e.g. after `/role`
    /// or `/model` switches), overriding tier-based resolution.
    primary_id: Option<String>,
    /// Per-session event log. Held as `Option` so the type is
    /// still constructible in unit tests that don't write a file.
    log: Option<super::chatlog::ChatLog>,
    /// Number of `run_turn` invocations. Fed to `SessionEnd`
    /// so the trace carries the total turn count.
    turn_count: u32,
    last_response: Option<String>,
    debug_flags: super::DebugFlags,
    /// 渲染器：把事件转成 CLI 文本或 JSON Lines 输出。
    /// 调用方决定使用哪个具体实现。
    renderer: Box<dyn latte_agent_core::renderer::ChatRenderer>,
}

impl ChatSession {
    fn new(
        merged: AgentConfig,
        resolver: ModelResolver,
        default_params: GenerateParams,
        runner: AgentRunner,
        role_id: String,
        tier: ModelTier,
        primary_id: Option<String>,
        debug_flags: super::DebugFlags,
        renderer: Box<dyn latte_agent_core::renderer::ChatRenderer>,
    ) -> Self {
        Self {
            merged,
            resolver,
            default_params,
            runner,
            role_id,
            tier,
            primary_id,
            log: None,
            last_response: None,
            turn_count: 0,
            debug_flags,
            renderer,
        }
    }

    /// Look up the role's display icon (emoji) from the merged config.
    /// Falls back to a generic 🤖 if the role is not configured.
    fn role_icon(&self) -> &'static str {
        self.merged
            .roles
            .get(&self.role_id)
            .map(|t| {
                // Icon is owned by the template, but we only need a
                // short-lived display — leak a &'static str. In practice
                // icons are 1-4 bytes of UTF-8 + emoji so the leak is tiny.
                Box::leak(t.icon.clone().into_boxed_str()) as &'static str
            })
            .unwrap_or("🤖")
    }

    /// Return the chain's primary model id (head) or "?" if empty.
    fn primary_model_id(&self) -> &str {
        self.runner
            .agent()
            .model_chain
            .first()
            .map(|mc| mc.model.id.as_str())
            .unwrap_or("?")
    }

    /// Inject previously-saved conversation history into the agent's
    /// context (does NOT re-emit the system prompt; the agent re-derives
    /// that per turn). Used by --resume to restore a session that was
    /// saved by  or by the auto-save on turn failure.
    fn load_history(&mut self, msgs: Vec<Message>) {
        self.runner.context_mut().extend(msgs);
    }

    /// Auto-save the current conversation history to a timestamped
    /// file under ~/.latte/chat-saves/. Called by the REPL when a
    /// turn fails so the user can resume with --resume.
    fn save_to_default(&self) -> Result<std::path::PathBuf, String> {
        let dir = default_save_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("create {}: {}", dir.display(), e))?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = dir.join(format!("chat-{}-{}.jsonl", self.role_id, stamp));
        let msgs: Vec<Message> = self.runner.context().messages().to_vec();
        save_session(path.to_str().unwrap(), &msgs)
            .map_err(|e| format!("write {}: {}", path.display(), e))?;
        Ok(path)
    }

    async fn turn(&mut self, user_input: &str) -> AnyResult {
        self.turn_count += 1;
        let chain_ids: Vec<String> = self
            .runner
            .agent()
            .model_chain
            .iter()
            .map(|mc| mc.model.id.clone())
            .collect();
        if let Some(log) = &self.log {
            log.info(
                "turn request",
                &[
                    ("role", self.role_id.clone()),
                    ("tier", self.tier.label().to_string()),
                    ("chain", chain_ids.join(" → ")),
                    ("input", truncate(user_input, 400)),
                ],
            );
        }
        let msgs = vec![Message {
            role: MsgRole::User,
            content: vec![latte_ai::models::ContentPart::text(user_input)],
            tool_call_id: None,
            tool_calls: None
        }];
        // Show a braille spinner while the model is generating. The
        // spinner writes to stderr with `\r` so it doesn't fight
        // stdout on a TTY; piped (non-TTY) runs get no decoration.
        let spinner = style::Spinner::start("thinking…");
        let result = self.runner.run_turn(&msgs, None).await;
        spinner.stop();
        match result {
            Ok(response) => {
                if let Some(log) = &self.log {
                    log.info(
                        "turn response",
                        &[
                            ("role", self.role_id.clone()),
                            ("tier", self.tier.label().to_string()),
                            ("len", response.len().to_string()),
                            ("preview", truncate(&response, 400)),
                        ],
                    );
                }
                self.last_response = Some(response);
                Ok(())
            }
            Err(e) => {
                let msg = e.to_string();
                if let Some(log) = &self.log {
                    log.error(
                        "turn failed",
                        &[
                            ("role", self.role_id.clone()),
                            ("tier", self.tier.label().to_string()),
                            ("chain", chain_ids.join(" → ")),
                            ("error", msg.clone()),
                        ],
                    );
                }
                Err(e.into())
            }
        }
    }

    async fn handle_command(&mut self, line: &str) -> AnyResult<bool> {
        // Returns `Ok(true)` when the REPL should exit.
        let mut parts = line.split_whitespace();
        let cmd = parts.next().unwrap_or("");
        let rest: Vec<&str> = parts.collect();

        match cmd {
            "/exit" | "/quit" => return Ok(true),
            "/help" => {
                print_help();
            }
            "/roles" => {
                let mut ids: Vec<&String> = self.merged.roles.keys().collect();
                ids.sort();
                println!("Available roles ({}):", ids.len());
                for id in ids {
                    if let Some(tpl) = self.merged.roles.get(id) {
                        println!(
                            "  {} \u{2014} {} [{}]",
                            id, tpl.name, tpl.model_tier
                        );
                    }
                }
            }
            "/role" => {
                let Some(id) = rest.first() else {
                    println!("usage: /role <id>");
                    return Ok(false);
                };
                let history: Vec<Message> =
                    self.runner.context().messages().to_vec();
                let new_tier = self.tier;
                match build_runner(
                    &self.merged,
                    &self.resolver,
                    &self.default_params,
                    id,
                    new_tier,
                    self.primary_id.as_deref(),
                    &self.debug_flags,
                    // Legacy /role switch — no HIL session.
                    None,
                    &std::env::current_dir()?,
                )
                .await
                {
                    Ok((mut runner, rid)) => {
                        for m in history {
                            runner.context_mut().push(m);
                        }
                        let chain_ids: Vec<String> = runner
                            .agent()
                            .model_chain
                            .iter()
                            .map(|mc| mc.model.id.clone())
                            .collect();
                        self.runner = runner;
                        self.role_id = rid.clone();
                        println!(
                            "Switched to role '{}' (tier {})",
                            rid,
                            self.tier.label()
                        );
                        if let Some(log) = &self.log {
                            log.info(
                                "switch role",
                                &[
                                    ("role", rid.clone()),
                                    ("tier", self.tier.label().to_string()),
                                    ("chain", chain_ids.join(" → ")),
                                ],
                            );
                        }
                    }
                    Err(e) => {
                        if let Some(log) = &self.log {
                            log.error("switch role failed", &[("error", e.to_string())]);
                        }
                        println!("error: {}", e)
                    }
                }
            }
            "/model" => {
                let Some(t) = rest.first() else {
                    println!("usage: /model <premium|standard|budget>");
                    return Ok(false);
                };
                match parse_tier(t) {
                    Ok(new_tier) => {
                        let role_id = self.role_id.clone();
                        let history: Vec<Message> = self.runner.context().messages().to_vec();
                        match build_runner(
                            &self.merged,
                            &self.resolver,
                            &self.default_params,
                            &role_id,
                            new_tier,
                            self.primary_id.as_deref(),
                            &self.debug_flags,
                            // Legacy /model switch — no HIL session.
                            None,
                            &std::env::current_dir()?,
                        )
                        .await
                        {
                            Ok((mut runner, _)) => {
                                for m in history {
                                    runner.context_mut().push(m);
                                }
                                let chain_ids: Vec<String> = runner
                                    .agent()
                                    .model_chain
                                    .iter()
                                    .map(|mc| mc.model.id.clone())
                                    .collect();
                                self.runner = runner;
                                self.tier = new_tier;
                                println!("Switched to tier {}", self.tier.label());
                                if let Some(log) = &self.log {
                                    log.info(
                                        "switch tier",
                                        &[
                                            ("role", self.role_id.clone()),
                                            ("tier", new_tier.label().to_string()),
                                            ("chain", chain_ids.join(" → ")),
                                        ],
                                    );
                                }
                            }
                            Err(e) => {
                                if let Some(log) = &self.log {
                                    log.error(
                                        "switch tier failed",
                                        &[("error", e.to_string())],
                                    );
                                }
                                println!("error: {}", e)
                            }
                        }
                    }
                     Err(e) => {
                        if let Some(log) = &self.log {
                            log.warn("invalid tier", &[("input", t.to_string())]);
                            log.error("invalid tier", &[("error", e.to_string())]);
                        }
                        println!("error: {}", e);
                    }
                }
            }
            "/clear" => {
                self.runner.context_mut().clear();
                self.last_response = None;
                println!("Context cleared.");
            }
            "/save" => {
                let Some(path) = rest.first() else {
                    println!("usage: /save <file>");
                    return Ok(false);
                };
                let n = self.runner.context().messages().len();
                save_session(path, self.runner.context().messages())?;
                println!("Saved {} messages to {}", n, path);
            }
            "/load" => {
                let Some(path) = rest.first() else {
                    println!("usage: /load <file>");
                    return Ok(false);
                };
                let msgs = load_session(path)?;
                self.runner.context_mut().clear();
                for m in msgs {
                    self.runner.context_mut().push(m);
                }
                println!(
                    "Loaded {} messages from {}",
                    self.runner.context().messages().len(),
                    path
                );
            }
            "/history" => {
                let msgs = self.runner.context().messages();
                if msgs.is_empty() {
                    println!("(no messages)");
                } else {
                    for (i, m) in msgs.iter().enumerate() {
                        let preview: String = m.as_text().chars().take(80).collect();
                        println!("{:>3} [{:?}] {}", i, m.role, preview);
                    }
                }
            }
            "/status" => {
                let chain: Vec<String> = self
                    .runner
                    .agent()
                    .model_chain
                    .iter()
                    .map(|mc| mc.model.id.clone())
                    .collect();
                let primary = chain.first().map(String::as_str).unwrap_or("?");
                let usage = self.runner.total_usage();
                let ctx_msgs = self.runner.context().messages().len();
                println!(
                    "role    : {}\n\
                     tier    : {}\n\
                     model   : {}\n\
                     chain   : {}\n\
                     tokens  : ↑{} in ↓{} out (session)\n\
                     context : {} messages",
                    self.role_id,
                    self.tier.label(),
                    primary,
                    if chain.is_empty() {
                        "(empty)".to_string()
                    } else {
                        chain.join(" → ")
                    },
                    usage.input_tokens,
                    usage.output_tokens,
                    ctx_msgs,
                );
            }
            "/tools" => {
                let role = self.merged.roles.get(&self.role_id);
                let tools: Vec<String> = role
                    .map(|r| r.tools.clone())
                    .unwrap_or_default();
                if tools.is_empty() {
                    println!("role '{}' has no tools configured", self.role_id);
                } else {
                    println!("role '{}' tools ({}):", self.role_id, tools.len());
                    for t in &tools {
                        println!("  - {}", t);
                    }
                    println!(
                        "\nFormat: <tool_call>{} {{\"arg\": \"value\"}}</tool_call>",
                        tools.first().map(String::as_str).unwrap_or("name")
                    );
                }
            }
            other => {
                println!("unknown command: {} (try /help)", other);
            }
        }
        Ok(false)
    }
}

/// Build a fresh `AgentRunner` for the named role at the given tier.
/// If `primary_id` is `Some`, the named model becomes the head of the
/// chain (skipping tier resolution) — used by the `-m` / `--model-id`
/// CLI flag. Returns the runner and the canonical role id.
async fn build_runner(
    merged: &AgentConfig,
    resolver: &ModelResolver,
    default_params: &GenerateParams,
    role_id: &str,
    tier: ModelTier,
    primary_id: Option<&str>,
    debug_flags: &super::DebugFlags,
    // Optional session arc forwarded from `run_hil_chat` so the
    // per-specialist `ask_human` tool can pause the shared
    // SessionManager when a specialist needs human input. The
    // legacy single-role REPL paths pass `None` — `ask_human` is
    // a HIL-only feature (manager doesn't need it; it delegates
    // instead).
    session: Option<Arc<tokio::sync::Mutex<latte_agent_core::session::SessionManager>>>,
    // Working directory the runner should treat as ground truth for
    // system-prompt env injection (matches the process's actual cwd).
    cwd: &std::path::Path,
) -> AnyResult<(AgentRunner, String)> {
    let template = merged
        .roles
        .get(role_id)
        .ok_or_else(|| format!("role '{}' not found", role_id))?
        .clone();
    let mut role = template.resolve(default_params).await?;
     let models = if let Some(id) = primary_id {
        // `-m` flag: pin the head of the chain to a specific model
        // (matched by `id` first, then by `name` case-insensitive, so
        // `-m DeepSeek-v4-flash` and `-m deepseek-v4-flash` both
        // work). If the model is unknown or has no `api_key`, surface
        // the error here — never fall through to a different model
        // silently, since the operator asked for this one explicitly.
        let mut chain = Vec::with_capacity(1 + role.model_chain.len());
        chain.push(resolver.resolve_id_or_name(id).map_err(|e| {
            format!("{}", e)
        })?);
        // Append the role's declared fallback chain (which already
        // includes any global-only models injected by
        // `config_layer::load`).
        for cid in &role.model_chain {
            if chain.iter().any(|m| m.name == *cid) {
                continue;
            }
            if let Ok(m) = resolver.build_model(cid) {
                chain.push(m);
            }
        }
        chain
    } else {
        resolver.resolve_chain(&role.id, tier, &role.model_chain)?
    };
    if models.is_empty() {
        return Err(format!(
            "no usable model for role '{}' (tier {})",
            role_id,
            tier.label()
        )
        .into());
    }
    if !role.allowed_tools.is_empty() {
        // Append a tool-usage section to the system prompt so the
        // model knows it can call tools and how. The model emits
        // `<tool_call>name {"arg": ...}</tool_call>` and `AgentRunner`
        // parses that to drive `tool_manager.execute`.
        let mut prompt = tool_usage_prompt(&role.allowed_tools);
        // If the role can dispatch subtasks to specialist agents,
        // describe the delegate tool so the model knows to use it.
        if role_id == "manager" {
            prompt.push_str(&DELEGATE_TOOL_HINT);
        }
        prompt.push_str(&latte_agent_core::ground_truth::ground_truth_block(cwd));
        role.system_prompt.push_str(&prompt);
    } else {
        // No tools but still inject the env so the model sees real cwd
        // / host and doesn't hallucinate paths on direct-answer turns.
        role.system_prompt.push_str(
            &latte_agent_core::ground_truth::ground_truth_block(cwd),
        );
    }
    let agent = Agent::new_with_chain(
        role_id.to_string(),
        role.clone(),
        models,
        default_params.clone(),
    )?;
    // Use the canonical session_id (from the ChatLog file stem in chat,
    // synthesized for discuss) — NOT the role_id. The role_id is what
    // each runner does, but the session_id is what groups every
    // TraceMeta emitted in this session so chat log / trace JSONL /
    // session idx all share the same stem per spec §9.
    let sink = super::build_debug_sink(&debug_flags.session_id, debug_flags);
    let hooks = super::build_debug_hooks(debug_flags);
    // with_session_id flows into every TraceMeta the runner emits —
    // without this the emitted events default to `session_id = ""`
    // and the JsonlSink / IndexSink records can't be cross-referenced
    // back to the chat log or session directory.
    let with_session = |r: AgentRunner| r.with_session_id(debug_flags.session_id.clone());
    let runner = if !role.allowed_tools.is_empty() {
        let tm = build_tool_manager(&role.allowed_tools).await
            .map_err(|e| format!("tool setup failed: {}", e))?;
        // Register the delegate tool so the manager can dispatch
        // subtasks to specialist agents.
        register_delegate_tool(
            &tm,
            Arc::new(merged.clone()),
            Arc::new(resolver.clone()),
            default_params.clone(),
            Arc::new(Semaphore::new(
                std::env::var("LATTE_AGENT_DELEGATE_CONCURRENCY")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(DEFAULT_DELEGATE_CONCURRENCY)
            )),
            // Per-model timeout is resolved inside the closure after
            // the specialist's model chain is known (model.timeout_secs
            // > env > DEFAULT_DELEGATE_TIMEOUT_SECS). The env value
            // is captured once here since the env doesn't change at
            // runtime.
            std::env::var("LATTE_AGENT_DELEGATE_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok()),
            // Pass the manager's real sink through — without this every
            // specialist's HookFired / ToolExec / TurnEnd event was
            // emitted to NullSink and never reached stdout / jsonl /
            // the session index. The ScopedSink inside the delegate
            // handler wraps this so specialist events get the right
            // `role` field for the trace consumer.
            Arc::clone(&sink),
        )
        .await
        .map_err(|e| format!("delegate tool setup failed: {}", e))?;
        // Register the `workflow` tool for roles that declare it
        // (manager). The controller/UI path already did this; the REPL
        // path was missing it, so a REPL manager could never run named
        // workflows even though its prompt told it to.
        if role.allowed_tools.iter().any(|t| t == "workflow") {
            register_workflow_tool(
                &tm,
                Arc::new(merged.clone()),
                Arc::new(resolver.clone()),
                default_params.clone(),
                cwd,
                role_id,
            )
            .await
            .map_err(|e| format!("workflow tool setup failed: {}", e))?;
        }
        // HIL v1.1 phase 6: wire the `ask_human` tool for every
        // non-manager role. The tool pauses the shared session
        // when a specialist needs clarification, surfacing the
        // question to the human via the REPL. The manager doesn't
        // need `ask_human` (it delegates to specialists instead),
        // and legacy single-role REPL paths pass `session = None`
        // so this block is a no-op outside HIL.
        if role_id != "manager" {
            if let Some(session_arc) = session.clone() {
                register_ask_human_tool(&tm, session_arc, role_id.to_string());
            }
        }
        with_session(
            AgentRunner::new_with_tools(agent, tm, 0)
                .with_sink(Arc::clone(&sink))
                .with_hooks(Arc::clone(&hooks))
                .with_role(role_id)
        )
    } else {
        with_session(
            AgentRunner::new(agent)
                .with_sink(Arc::clone(&sink))
                .with_hooks(Arc::clone(&hooks))
                .with_role(role_id)
        )
    };
    Ok((runner, role_id.to_string()))
}

/// Build a tool manager that exposes only the names in `allowed`.
/// Must run inside an async context (we are called from
/// `build_runner`, which is invoked from `#[tokio::main]`).
/// `ToolManager::register_package` is async, so we `await` directly
/// rather than spinning up a second runtime.
pub async fn build_tool_manager(
    allowed: &[String],
) -> Result<Arc<dyn latte_rs_agent_tools::types::ToolManager>, Box<dyn std::error::Error>> {
    use latte_rs_agent_tools::prelude::*;
     let mgr = create_tool_manager();
     for p in builtin_tool_packages() {
         mgr.register_package(p)
            .await
            .map_err(|e| format!("register_package: {}", e))?;
    }
    // Allowlist filter: short name or namespaced id.
    let mut keep: std::collections::HashSet<String> = allowed
        .iter()
        .flat_map(|s| vec![s.to_lowercase(), s.clone()])
        .collect();
    // Alias mapping: configs use friendly names ("bash") but builtin
    // tools register under different short names ("shell.exec",
    // "shell.spawn"). Map the friendly name to the real one so
    // specialist configs don't need to know internal tool names.
    for alias in &["bash"] {
        if keep.contains(*alias) || keep.contains(&alias.to_lowercase()) {
            keep.insert("exec".to_string());
        }
    }
    for tool_id in mgr.get_tool_names() {
        let short = tool_id
            .rsplit_once('.')
            .map(|(_, s)| s.to_string())
            .unwrap_or_else(|| tool_id.clone());
        if !(keep.contains(&short) || keep.contains(&tool_id)) {
            mgr.unregister(&tool_id);
        }
    }
    Ok(mgr)
}

/// Register the `workflow` tool so roles that declare it (manager) can
/// run named multi-role workflows from the REPL. Mirrors the controller
/// path (`latte_agent_core::controller::register_workflow_tool`), but
/// the REPL has no UI event consumer, so the ChatEvents emitted during
/// the run are dropped — the workflow summary comes back as the tool
/// result.
async fn register_workflow_tool(
    tm: &Arc<dyn latte_rs_agent_tools::types::ToolManager>,
    merged: Arc<AgentConfig>,
    resolver: Arc<ModelResolver>,
    default_params: GenerateParams,
    cwd: &Path,
    role_id: &str,
) -> AnyResult {
    use latte_rs_agent_tools::types::{PropertyType, Tool, ToolInputProperty, ToolInputSchema};

    let available = latte_agent_core::workflow::list_workflows(cwd);
    let available_text = if available.is_empty() {
        "none found in .latte/workflows.d".to_string()
    } else {
        available
            .iter()
            .map(|(n, d)| {
                if d.is_empty() { n.clone() } else { format!("{n} — {d}") }
            })
            .collect::<Vec<_>>()
            .join("; ")
    };

    let input_schema = ToolInputSchema {
        schema_type: latte_rs_agent_tools::types::SchemaType,
        properties: vec![
            ("name".into(), ToolInputProperty {
                property_type: PropertyType::String,
                description: Some(format!("Workflow name. Available: {available_text}")),
                enum_values: None,
                minimum: None,
                maximum: None,
                min_length: None,
                max_length: None,
            }),
            ("topic".into(), ToolInputProperty {
                property_type: PropertyType::String,
                description: Some("The task/topic the workflow should work on".into()),
                enum_values: None,
                minimum: None,
                maximum: None,
                min_length: None,
                max_length: None,
            }),
        ]
        .into_iter()
        .collect(),
        required: Some(vec!["name".into(), "topic".into()]),
        ..Default::default()
    };

    let cwd_owned = cwd.to_path_buf();
    let handler: latte_rs_agent_tools::types::SharedToolHandler =
        std::sync::Arc::new(move |input: serde_json::Value, _ctx| {
            let merged = Arc::clone(&merged);
            let resolver = Arc::clone(&resolver);
            let default_params = default_params.clone();
            let cwd = cwd_owned.clone();
            Box::pin(async move {
                let tool_err = |msg: String| latte_rs_agent_tools::error::ToolError::Other(msg);
                let name = input
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| tool_err("missing 'name' field".into()))?
                    .to_string();
                let topic = input
                    .get("topic")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| tool_err("missing 'topic' field".into()))?
                    .to_string();
                let wf = latte_agent_core::workflow::load_workflow(&name, &cwd).map_err(|e| {
                    let available = latte_agent_core::workflow::list_workflows(&cwd)
                        .iter()
                        .map(|(n, _)| n.clone())
                        .collect::<Vec<_>>()
                        .join(", ");
                    tool_err(format!("{e}. available workflows: {available}"))
                })?;
                // No UI subscriber in the REPL: keep the receiver alive
                // for the duration of the run and drop it afterwards.
                let (event_tx, _rx) = tokio::sync::broadcast::channel(64);
                let ctx = latte_agent_core::workflow::WorkflowRunContext {
                    merged,
                    resolver,
                    default_params,
                    cwd,
                    event_tx,
                    cancel_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    agent_pause_gate: None, // CLI REPL workflow：无 session agent gate
                    depth: 0,
                };
                latte_agent_core::workflow::run_workflow(&wf, &topic, &ctx)
                    .await
                    .map(serde_json::Value::String)
                    .map_err(tool_err)
            })
        });

    let tool = Tool::builder(
        "workflow".to_string(),
        format!("Run a named multi-role workflow. Available workflows: {available_text}"),
        input_schema,
        handler,
    )
    .build();

    tm.register(tool, Some(role_id));
    Ok(())
}

/// Register a `delegate` tool on the tool manager. The tool lets the
/// manager agent dispatch subtasks to specialist roles (programmer,
/// architect, reviewer, etc.) and receive their responses.
///
/// `env_timeout_secs` is the `LATTE_AGENT_DELEGATE_TIMEOUT_SECS` value
/// resolved once at the call site (env doesn't change at runtime);
/// the per-call timeout is then resolved as
/// `model.timeout_secs > env_timeout_secs > DEFAULT_DELEGATE_TIMEOUT_SECS`
/// — see the closure below. Default 60s.
// =====================================================================
// HIL v1 Phase 6 — checkpoint wire-up verification marker
//
// The HIL v1 spec §4.8 asserts that specialist writes issued from the
// delegate closure land in the same WorktreeSpec::worktree_root shared
// by WorkspaceManager::create (run.rs) and CheckpointEngine::new
// (run.rs). Both paths resolve through WorktreeSpec::derive_paths, so
// a single source of truth binds them. The manager tm passed into
// register_delegate_tool below is built by build_tool_manager (line
// ~899) as a vanilla create_tool_manager() carrying builtin packages
// filtered by role.allowed_tools — it is NOT wrapped by
// WorkspaceManager. The fresh specialist_tm built inside the closure
// is also not wrapped. v1 therefore relies on the convention that
// latte-agent chat --task-id X is launched from the repo root, so
// writes land in the worktree's branch directory by way of
// git-worktree-link semantics. Checkpoints are explicit-only in v1
// (no ToolManager::execute hook wraps record_write); they are
// produced by latte-agent run at init and latte-agent checkpoint
// create on demand. Auto-checkpoint-on-write is deferred.
// =====================================================================
async fn register_delegate_tool(
    tm: &Arc<dyn latte_rs_agent_tools::types::ToolManager>,
    merged: Arc<AgentConfig>,
    resolver: Arc<ModelResolver>,
    default_params: GenerateParams,
    delegate_sem: Arc<Semaphore>,
    env_timeout_secs: Option<u64>,
    delegate_sink: Arc<dyn latte_agent_core::trace::TraceSink>,
) -> AnyResult {
    use latte_rs_agent_tools::types::{PropertyType, Tool, ToolInputProperty, ToolInputSchema};

    let input_schema = ToolInputSchema {
        schema_type: latte_rs_agent_tools::types::SchemaType,
        properties: vec![
            ("role".into(), ToolInputProperty {
                property_type: PropertyType::String,
                description: Some(
                    "Specialist role id: programmer, architect, reviewer, tester, security, devops, designer, tech_writer, pm"
                        .into(),
                ),
                enum_values: None,
                minimum: None,
                maximum: None,
                min_length: None,
                max_length: None,
            }),
            ("task".into(), ToolInputProperty {
                property_type: PropertyType::String,
                description: Some(
                    "Natural-language task for the specialist to perform".into(),
                ),
                enum_values: None,
                minimum: None,
                maximum: None,
                min_length: None,
                max_length: None,
            }),
        ]
        .into_iter()
        .collect(),
        required: Some(vec!["role".into(), "task".into()]),
        ..Default::default()
    };
    let handler: latte_rs_agent_tools::types::SharedToolHandler =
        std::sync::Arc::new(move |input: serde_json::Value, _ctx| {
            let merged = Arc::clone(&merged);
            let resolver = Arc::clone(&resolver);
            let default_params = default_params.clone();
            let sem = Arc::clone(&delegate_sem);
            let env_timeout = env_timeout_secs;
            let sink = Arc::clone(&delegate_sink);
            Box::pin(async move {
                let tool_err = |msg: String| latte_rs_agent_tools::error::ToolError::Other(msg);
                let role_id = input
                    .get("role")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| tool_err("missing 'role' field".into()))?
                    .to_string();
                let task = input
                    .get("task")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| tool_err("missing 'task' field".into()))?
                    .to_string();
                // Surface the dispatch to the user. The chat log
                // records these as structured events, but the user
                // running interactively needs to see *something*
                // happen during the spinner — a 60s specialist
                // run otherwise feels like a hang. Use eprintln
                // so we don't fight the spinner on stdout.
                // We print the dispatch *after* resolving the
                // specialist's model chain so the header line
                // carries the model id + tier the user is about
                // to wait on.
                let dispatch_start = std::time::Instant::now();

                // Resolve the role and build a temporary AgentRunner.
                let template = merged
                    .roles
                    .get(&role_id)
                    .ok_or_else(|| {
                        tool_err(format!("role '{}' not found in config", role_id))
                    })?
                    .clone();
                let role = template.resolve(&default_params).await.map_err(|e| {
                    tool_err(format!("failed to resolve role '{}': {}", role_id, e))
                })?;
                let tier = role.default_model_tier;
                let models = resolver
                    .resolve_chain(&role.id, tier, &role.model_chain)
                    .map_err(|e| {
                        tool_err(format!("no model for role '{}': {}", role_id, e))
                })?;
                // Resolve the per-specialist wall-clock timeout. Order:
                //   1. `model.timeout_secs` from the model catalog (per-model
                //      override — e.g. glm-5.2 sets 120s because it's slow
                //      on 8-step tasks)
                //   2. `LATTE_AGENT_DELEGATE_TIMEOUT_SECS` (env, captured
                //      once at startup as `env_timeout`)
                //   3. `DEFAULT_DELEGATE_TIMEOUT_SECS` (60s)
                let timeout_s = resolver
                    .get_def(&models[0].id)
                    .and_then(|d| d.timeout_secs)
                    .or(env_timeout)
                    .unwrap_or(DEFAULT_DELEGATE_TIMEOUT_SECS);
                // Print the dispatch header now that we know which
                // model the specialist will use. Multi-line task
                // is shown verbatim so the user can see what was
                // actually sent to the specialist.
                eprintln!(
                    "  → delegating to {} (model={}, tier={})",
                    role_id, models[0].id, tier.label(),
                );
                eprintln!("    task:");
                for line in task.lines() {
                    eprintln!("      | {}", line);
                }
                let agent = Agent::new_with_chain(
                    role_id.clone(),
                    role.clone(),
                    models,
                    default_params.clone(),
                )
                .map_err(|e| {
                    tool_err(format!("failed to create agent for '{}': {}", role_id, e))
                })?;
                // (e.g. programmer has read/write/bash/search). Without
                // this the specialist answers "I have no file access" —
                // the bug we hit in late June 2026 where the manager
                // delegated but the programmer refused to read any code.
                let specialist_tm = if role.allowed_tools.is_empty() {
                    None
                } else {
                    match build_tool_manager(&role.allowed_tools).await {
                        Ok(tm) => Some(tm),
                        Err(e) => {
                            return Err(tool_err(format!(
                                "tool setup for '{}' failed: {}",
                                role_id, e
                            )));
                        }
                    }
                };
                let scoped_sink = Arc::new(latte_agent_core::trace::ScopedSink::new(
                    Arc::clone(&sink),
                    role_id.clone(),
                ));
                let mut runner = match specialist_tm {
                    // unlimited tool rounds — model decides when it's done.
                    // LoopDetector in agent.rs trips on actual stuck patterns.
                    Some(tm) => AgentRunner::new_with_tools(agent, tm, 0)
                        .with_sink(scoped_sink.clone())
                        .with_role(role_id.clone()),
                    None => AgentRunner::new(agent)
                        .with_sink(scoped_sink.clone())
                        .with_role(role_id.clone()),
                };
                let msgs = vec![Message {
                    role: MsgRole::User,
                    content: vec![latte_ai::models::ContentPart::text(task)],
                    tool_call_id: None,
            tool_calls: None
                }];
                // specialists are already running).
                let _permit = sem.acquire().await.map_err(|_| {
                    tool_err("delegate pool shut down".into())
                })?;
                // Run the specialist with a wall-clock timeout.
                let response = match tokio_timeout(
                    Duration::from_secs(timeout_s),
                    runner.run_turn(&msgs, None),
                ).await {
                    Ok(Ok(resp)) => resp,
                    Ok(Err(e)) => {
                        eprintln!("  ← {} failed: {}", role_id, e);
                        return Err(tool_err(format!(
                            "delegate to '{}' failed: {}", role_id, e
                        )));
                    }
                    Err(_) => {
                        eprintln!("  ← {} timed out after {}s", role_id, timeout_s);
                        return Err(tool_err(format!(
                            "delegate to '{}' timed out after {}s",
                            role_id, timeout_s
                        )));
                    }
                };
                drop(_permit);
                let elapsed = dispatch_start.elapsed();
                eprintln!(
                    "  ← {} returned ({} chars in {:.1}s)",
                    role_id,
                    response.len(),
                    elapsed.as_secs_f64(),
                );
                Ok(serde_json::json!({
                    "role": role_id,
                    "response": response,
                }))
            })
        });

    let tool = Tool::builder(
        "delegate".to_string(),
        "Delegate a subtask to a specialist agent. The specialist will analyze and respond, then return results to you for synthesis.".to_string(),
        input_schema,
        handler,
    )
    .build();

    tm.register(tool, Some("manager"));
    Ok(())
}

/// Build the [`CliOverrides`] from a [`ChatCmd`]. Parses each
/// `--model-override id.field=value` into the flat tuple form the loader
/// expects; a malformed entry is an error.
fn build_cli_overrides(cmd: &ChatCmd) -> Result<CliOverrides, String> {
    let mut out = CliOverrides {
        api_key: cmd.api_key.clone(),
        api_key_target: cmd.model.clone(),
        field_overrides: Vec::with_capacity(cmd.model_overrides.len()),
    };
    for raw in &cmd.model_overrides {
        let (id, field, value) = parse_model_override(raw)
            .map_err(|e| format!("invalid --model-override '{}': {}", raw, e))?;
        out.field_overrides.push((id, field, value));
    }
    Ok(out)
}

pub(crate) fn parse_model_override(s: &str) -> Result<(String, String, String), String> {
    let (id_field, value) = s
        .split_once('=')
        .ok_or_else(|| format!("expected ID.FIELD=VALUE, got '{}'", s))?;
    let (id, field) = id_field
        .split_once('.')
        .ok_or_else(|| format!("expected ID.FIELD, got '{}'", id_field))?;
    if id.is_empty() || field.is_empty() {
        return Err(format!("empty id or field in '{}'", s));
    }
    Ok((id.to_string(), field.to_string(), value.to_string()))
}
pub(crate) fn tool_usage_prompt(allowed: &[String]) -> String {
    // Map friendly config aliases to real builtin tool names. Configs
    // use "bash" because that's what most operators know, but the
    // builtin package registers it as "shell.exec". Without this map
    // the system prompt lies to the model about what's available and
    // every `tool_callbash` fails with "tool not found".
    let real_names: Vec<String> = allowed
        .iter()
        .map(|s| match s.as_str() {
            "bash" => "exec".to_string(),
            other => other.to_string(),
        })
        .collect();
    let tool_list = real_names.join(", ");
    format!(
        r#"

## Tool calling protocol

When you need to use a tool, emit EXACTLY this format on its own line
(no markdown, no code fences, no backticks — the raw markers below are
parsed verbatim by the host):

<tool_call>NAME {{"arg": "value"}}</tool_call>

Rules you MUST follow:
  1. Start with the literal text <tool_call> (no spaces, no backticks).
  2. Then the tool name and a single space, then a JSON object of args.
  3. Close with the literal text </tool_call>.
  4. Do NOT wrap the line in markdown code blocks (```...```) or indent
     it as a code block — the parser will not see the markers.
  5. You may emit multiple <tool_call> lines in one response; the host
     runs them and feeds results back as the next turn.
  6. When you have enough information to answer, respond in plain text
     with NO <tool_call> block and the loop ends.

Allowed tool names for this role: {tool_list}.

Examples (raw, copy the format exactly):

<tool_call>list {{"path": "."}}</tool_call>
<tool_call>read {{"path": "README.md"}}</tool_call>
<tool_call>search {{"path": "src", "pattern": "TODO", "max_results": 20}}</tool_call>
"#
    )
}

/// System-prompt hint describing the `delegate` tool, appended for
/// the manager role so it knows it can dispatch subtasks to
/// specialist agents.
const DELEGATE_TOOL_HINT: &str = r#"

### Delegating to specialists

For substantive tasks you SHOULD fan out to specialists and
synthesize. Available specialist roles:
- `programmer` — code analysis, reading code, understanding
- `architect` — module structure, dependency graph, design overview
- `reviewer_sanity` — fast fact-check: file references, syntax,
  internal consistency. Uses read + list only.
- `reviewer_architecture` — design check: module deps, interfaces,
  circular imports. Uses read + search + list.
- `reviewer_security` — deep audit: security, perf, edge cases.
  Uses read + search + list.
(Plus the older roles: `reviewer`, `tester`, `security`, `devops`,
`designer`, `tech_writer`, `pm` — for the discuss workflow.)

### Layered review protocol

After you receive a specialist's report, run a layered review
before finalizing your synthesis. This is the defense against
hallucinations and the "stuck model" failure mode:

1. **ALWAYS dispatch to `reviewer_sanity` first.** It's cheap
   (budget model, read-only). It catches file-reference
   hallucinations, syntax errors, and internal contradictions.
   A 5400-char report from a single specialist is NOT trustworthy
   without a sanity pass.

2. **If `reviewer_sanity` returns VERDICT: FAIL:**
   - DO NOT synthesize the report as-is.
   - Dispatch to `programmer` with the specific issues found,
     ask for a fix.
   - After programmer returns, re-run `reviewer_sanity`.
   - Only proceed to step 4 if sanity now passes.

3. **If `reviewer_sanity` returns VERDICT: WARN:**
   - You may either fix minor issues yourself (you have no tools,
     so just mention them in your final answer) or dispatch to
     `programmer` for a fix.
   - If the warnings are about the report's claims (not just style),
     dispatch to programmer.

4. **If `reviewer_sanity` returns VERDICT: PASS but you suspect a
   design issue** (e.g., the report contradicts the project's
   known architecture, mentions a new module, changes a public
   interface, or touches multiple files in ways that suggest a
   layer violation):
   - Dispatch to `reviewer_architecture` for a deeper check.
   - Only proceed if architecture also returns PASS or WARN.

5. **If the task touches auth, user data, network, file I/O on
   user-controlled paths, or process execution:**
   - Dispatch to `reviewer_security` after `reviewer_architecture`
     passes (or directly after sanity if you skipped architecture).
   - This is the deep audit. It's expensive on purpose.

6. **ONLY after all relevant review layers pass, synthesize your
   final answer.** Cite each reviewer ("per reviewer_sanity: ...,
   per reviewer_architecture: ..., per reviewer_security: ...").

#### When you answer directly without delegating

You are allowed to answer directly (without using `delegate`) when the
task is small enough that a specialist dispatch would be overkill —
e.g. a one-line explanation, a quick definition, a small code snippet,
a single-file edit. Use your judgement.

BUT: when you choose to answer directly, your response MUST start with
a `## Why no delegation` section that briefly explains why the task
doesn't warrant a specialist dispatch. Format:

    ## Why no delegation
    <one or two sentences: what kind of task this is, and why a
    specialist round-trip would be unnecessary overhead>

    <then your actual answer>

This gives the user visibility into your routing decision. If the
reasoning is wrong, the user can correct you and re-prompt. Do NOT
skip this section — a direct answer with no justification will be
treated as a routing error.
"#;
fn print_help() {
    println!(
        "Commands:\n\
         /role <id>           Switch to a different role (preserves history)\n\
         /model <tier>        Switch model tier: premium | standard | budget\n\
         /roles               List available roles\n\
         /status              Show current model, chain, tokens, context\n\
         /tools               List available tools for this role\n\
         /clear               Clear conversation history\n\
         /history             Show recent messages\n\
         /save <file>         Save conversation to a JSONL file\n\
         /load <file>         Load conversation from a JSONL file\n\
         /help                Show this help\n\
         /exit | /quit        Exit the chat"
    );
}


/// Truncate a string for log display, keeping the head and adding an
/// ellipsis when trimmed. Used so we don't dump multi-kilobyte model
/// responses into a log file.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    let mut out = String::with_capacity(end + 8);
    out.push_str(&s[..end]);
    out.push_str(" …[+");
    out.push_str(&(s.len() - end).to_string());
    out.push_str("B]");
    out
}

/// Emit the "config loaded" block to the chat log. The block has:
///
/// - a one-line header with summary counts (so `grep`/`rg` on the
///   log file can land on the right event without parsing the body)
/// - the project + global file paths that the loader actually
///   consulted, with `(none)` markers when a layer is empty
/// - the per-role reverse map (`role_id <- file`)
/// - the per-model reverse map (`model_id <- file`)
///
/// Duplicate entries (project overrides global) are kept in load
/// order — the **first** entry per id is the effective source.
/// This matches `config_layer::ResolvedSources` semantics, so
/// "what would the loader actually use" can be derived by taking
/// the head of each list.
fn emit_config_loaded(log: &super::chatlog::ChatLog, sources: &config_layer::ResolvedSources) {
    let mut body: Vec<String> = Vec::new();

    body.push(format!(
        "project_agents = {}",
        sources
            .project_agents
            .as_deref()
            .unwrap_or("(none)")
    ));
    body.push(format!(
        "project_models = {}",
        sources
            .project_models
            .as_deref()
            .unwrap_or("(none)")
    ));
    body.push(format!(
        "global[{}]",
        sources.global.len()
    ));
    if sources.global.is_empty() {
        body.push("  (none found; checked ~/.latte/models.{yaml,yml,toml} and ~/.latte/models.d/)".into());
    } else {
        for src in &sources.global {
            body.push(format!("  {src}"));
        }
    }

    if !sources.cli_overrides.is_empty() {
        body.push(format!("cli_overrides[{}]", sources.cli_overrides.len()));
        for ov in &sources.cli_overrides {
            body.push(format!("  {ov}"));
        }
    }

    body.push(format!(
        "roles[{}] -> file",
        sources.role_files.len()
    ));
    if sources.role_files.is_empty() {
        body.push("  (no role declared in any scanned file; using built-in defaults)".into());
    } else {
        for (id, files) in &sources.role_files {
            body.push(format!("  {id} <- {}", files.join(", ")));
        }
    }

    body.push(format!(
        "models[{}] -> file",
        sources.model_files.len()
    ));
    if sources.model_files.is_empty() {
        body.push("  (no model id extracted from any scanned file; YAML global models listed above)".into());
    } else {
        for (id, files) in &sources.model_files {
            body.push(format!("  {id} <- {}", files.join(", ")));
        }
    }

    let header = [
        (
            "roles",
            sources.role_files.len().to_string(),
        ),
        (
            "models",
            sources.model_files.len().to_string(),
        ),
        (
            "global",
            sources.global.len().to_string(),
        ),
    ];
    log.block("info", "config loaded", &header, &body);
}
fn parse_tier(s: &str) -> AnyResult<ModelTier> {
    ModelTier::parse(s).map_err(|e| -> Box<dyn std::error::Error> { e.into() })
}

fn save_session(path: &str, msgs: &[Message]) -> AnyResult {
    let file = std::fs::File::create(path)?;
    let mut writer = BufWriter::new(file);
    for m in msgs {
        let line = serde_json::to_string(m)?;
        writeln!(writer, "{}", line)?;
    }
    Ok(())
}

fn load_session(path: &str) -> AnyResult<Vec<Message>> {
    let content = std::fs::read_to_string(path)?;
    let mut msgs = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let m: Message = serde_json::from_str(line)?;
        msgs.push(m);
    }
    Ok(msgs)
}

/// Default directory for auto-saved sessions (used when a turn fails
/// and the user didn't specify a path). Created on demand.
fn default_save_dir() -> std::path::PathBuf {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    home.join(".latte").join("chat-saves")
}

#[cfg(test)]
mod tests {
    use super::*;
    use latte_ai::models::Role as MsgRole;

    fn msg(role: MsgRole, content: &str) -> Message {
        match role {
            MsgRole::System => Message::system(content),
            MsgRole::User => Message::user(content),
            MsgRole::Assistant => Message::assistant(content),
            MsgRole::Tool => Message::tool_result("call_test", content),
        }
    }

    #[test]
    fn session_round_trip() {
        let path = std::env::temp_dir().join("latte_chat_session_test.jsonl");
        let _ = std::fs::remove_file(&path);

        let original = vec![
            msg(MsgRole::System, "you are helpful"),
            msg(MsgRole::User, "hi"),
            msg(MsgRole::Assistant, "hello"),
        ];
        save_session(path.to_str().unwrap(), &original).unwrap();
        let restored = load_session(path.to_str().unwrap()).unwrap();
        assert_eq!(restored.len(), 3);
        assert_eq!(restored[0].as_text(), "you are helpful");
        assert_eq!(restored[2].as_text(), "hello");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_session_skips_blank_lines() {
        let path = std::env::temp_dir().join("latte_chat_session_blank.jsonl");
        let _ = std::fs::remove_file(&path);

        let body = "{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"a\"}]}\n\n{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"b\"}]}\n";
        std::fs::write(&path, body).unwrap();
        let msgs = load_session(path.to_str().unwrap()).unwrap();
        assert_eq!(msgs.len(), 2);

        let _ = std::fs::remove_file(&path);
    }
}

/// Drop is the canonical place to emit the trailing `SessionEnd`
/// trace event: it covers EOF in the REPL, the non-tty stdin
/// flush path, and any early error return without requiring the
/// caller to remember a teardown call. `emit_session_end` takes
/// `&self`, so a `&mut self` Drop impl is fine — we just read
/// the accumulated turn count and the runner's `total_usage`.
impl Drop for ChatSession {
    fn drop(&mut self) {
        self.runner.emit_session_end(self.turn_count);
    }
}


// =========================================================================
// HIL blackboard chat (Task 3.2): drive a multi-role session via
// SessionManager JSON when --task-id is given. The plain `chat` flow above
// is untouched for users who don't pass --task-id.
// =========================================================================

async fn run_hil_chat(
    cmd: &ChatCmd,
    task_id: String,
    roles: Vec<String>,
    initial_prompt: Option<String>,
    max_rounds: u32,
    session_token_budget: u32,
    no_ask_human: bool,
    renderer: &dyn latte_agent_core::renderer::ChatRenderer,
) -> AnyResult {
    use latte_agent_core::session::SessionManager;

    // 1. Resolve worktree root
    let cwd = std::env::current_dir()?;
    let repo_root = latte_agent_core::workspace::WorkspaceManager::resolve_repo_root(&cwd)
        .map_err(|e| format!("{}", e))?;
    let worktree_root = repo_root.join(".latte").join("worktrees").join(&task_id);
    if !worktree_root.exists() {
        return Err(format!(
            "worktree for task '{}' not found at {}. Run `latte-agent run --task-id {}` first.",
            task_id, worktree_root.display(), task_id
        ).into());
    }

    // 2. Open or create the SessionManager
    let mut mgr = SessionManager::new(&task_id, worktree_root.clone(), roles.clone());
    if mgr.session_path().exists() {
        let raw = std::fs::read_to_string(mgr.session_path())?;
        let record: latte_agent_core::session::SessionRecord = serde_json::from_str(&raw)?;
        mgr = SessionManager::from_record(record, worktree_root.clone());
        renderer.on_session_info(&mgr.record().task_id, &format!("{:?}", mgr.state()), mgr.record().current_turn).await;
        if mgr.state() == latte_agent_core::session::SessionState::Paused {
            mgr.resume()?;
            renderer.on_resumed().await;
            renderer.on_status(&format!("session timestamp: {}", mgr.record().updated_at)).await;
        }
    } else {
        // Fresh session: require --initial-prompt
        let prompt = match initial_prompt {
            Some(p) => p,
            None => return Err(format!(
                "no session for task '{}' — pass --initial-prompt to start one",
                task_id
            ).into()),
        };
        let bb = latte_agent_core::workspace::Blackboard::new(worktree_root.join("plan.md"));
        bb.write(&format!("# Task: {}\n\n## Initial prompt\n\n{}\n", task_id, prompt))?;
        mgr.persist()?;
        renderer.on_session_info(&task_id, "Created", 0).await;
        renderer.on_status(&format!("roles: {}", roles.join(", "))).await;
    }
    // 3. Build the shared `Arc<Mutex<SessionManager>>` (HIL v1.1 phase 6).
    // The `ask_human` tool handler needs a shared reference to the
    // session so it can pause + emit + return an error. The REPL
    // also needs the same manager, so both the REPL and (eventually,
    // in phase 7) the per-role build_runner calls share the same arc.
    let session_arc: Arc<tokio::sync::Mutex<latte_agent_core::session::SessionManager>> =
        Arc::new(tokio::sync::Mutex::new(mgr));

    // Attach a JsonlSink + IndexSink fanout so the SessionManager's
    // emit_round_started / emit_round_ended / emit_ask_human_event
    // methods actually land in the trace JSONL (HIL v1.2 bug fix).
    let latte_home: PathBuf = std::env::var_os("LATTE_HOME")
        .map(PathBuf::from)
        // v1: non-empty
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| {
            let cwd = std::env::current_dir().ok()?;
            let cand = cwd.join(".latte");
            if cand.exists() { Some(cand) } else { None }
        })
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".latte")))
        .unwrap_or_else(|| PathBuf::from(".latte"));
    let trace_path = latte_home.join("traces").join(format!("hil-{}.jsonl", task_id));
    let index_path = latte_home.join("sessions").join(format!("{}.idx", task_id));
    let jsonl_sink: Arc<dyn latte_agent_core::trace::TraceSink> = Arc::new(
        latte_agent_core::trace::JsonlSink::new(trace_path),
    );
    let index_sink: Arc<dyn latte_agent_core::trace::TraceSink> = Arc::new(
        latte_agent_core::trace::IndexSink::new(index_path),
    );
    let fanout: Arc<dyn latte_agent_core::trace::TraceSink> = Arc::new(
        latte_agent_core::trace::FanoutSink::new(vec![jsonl_sink, index_sink]),
    );
    let sink_arc = session_arc.clone();
    tokio::task::spawn_blocking(move || {
        sink_arc.blocking_lock().with_sink(fanout);
    })
    .await
    .map_err(|e| format!("with_sink join error: {}", e))?;

    // Transition Created → Running (idempotent for Resumed/Running).
    // Must run on a blocking thread because tokio::sync::Mutex's
    // `blocking_lock` panics from within the runtime (same pattern
    // as RoundScheduler::new below).
    let start_arc = session_arc.clone();
    tokio::task::spawn_blocking(move || {
        start_arc.blocking_lock().start().ok();
    })
    .await
    .map_err(|e| format!("start join error: {}", e))?;

    // 4. Build the round-robin scheduler (HIL v1.1 phase 7).
    // `RoundScheduler::new` uses `blocking_lock` internally to read the
    // role order from the manager, so we must run it on a blocking
    // thread to avoid `Cannot block the current thread from within a
    // runtime` panics. The scheduler is then reconfigured for the
    // user-supplied `max_rounds` and `session_token_budget`.
    let scheduler_arc = session_arc.clone();
    let mut scheduler = tokio::task::spawn_blocking(move || {
        RoundScheduler::new(scheduler_arc)
    })
    .await
    .map_err(|e| format!("scheduler join error: {}", e))??;
    scheduler.max_rounds = max_rounds;
    scheduler.supervisor = Supervisor::new(SupervisorConfig {
        session_token_budget,
        dead_loop_window: 3,
    });

    // Load agent + model config so we can build a per-role
    // `AgentRunner` for each role in `scheduler.order`. The runners
    // are reused across rounds (v1.2 keeps them alive instead of
    // rebuilding per round) and the REPL drives each role's
    // `run_turn` directly. v1.1's `no_ask_human` flag suppresses
    // the per-specialist `ask_human` tool registration when set.
    let cli = build_cli_overrides(cmd)?;
    let resolved = config_layer::load(
        Some(&cmd.agents_config),
        Some(&cmd.models_config),
        cli,
    )
    .map_err(|e| format!("failed to load configuration: {}", e))?;
    let merged = resolved.config;
    let resolver = resolved.resolver;
    let default_params = GenerateParams::default();
    let session_id = format!("hil-{}", task_id);
    let debug_flags = super::DebugFlags {
        debug: cmd.debug,
        debug_format: cmd.debug_format,
        debug_hooks: cmd.debug_hooks.clone(),
        no_session_index: cmd.no_session_index,
        debug_events: if cmd.debug_events.eq_ignore_ascii_case("all") {
            None
        } else {
            Some(cmd.debug_events.clone())
        },
        session_id: session_id.clone(),
    };
    // Build one `AgentRunner` per role, in the scheduler's order.
    // `no_ask_human` is a global kill switch that also passes
    // `None` for non-manager roles (so the `ask_human` tool is
    // not registered). Manager always uses `None` (it delegates
    // instead of asking).
    let mut runners: Vec<(String, AgentRunner)> = Vec::with_capacity(scheduler.order.len());
    for role_id in scheduler.order.clone() {
        let tier_str = if let Some(t) = &cmd.tier {
            t.clone()
        } else {
            merged
                .roles
                .get(&role_id)
                .map(|r| r.model_tier.clone())
                .unwrap_or_else(|| "standard".to_string())
        };
        let tier = parse_tier(&tier_str)?;
        let session_arg = if no_ask_human || role_id == "manager" {
            None
        } else {
            Some(session_arc.clone())
        };
        let (mut runner, _canonical_id) = build_runner(
            &merged,
            &resolver,
            &default_params,
            &role_id,
            tier,
            cmd.model_id.as_deref(),
            &debug_flags,
            session_arg,
            &std::env::current_dir()?,
        )
        .await?;
        // Wire the per-role inject queue drain path. With
        // `with_inject_worktree_root` set, `AgentRunner::run_turn`
        // will internally prepend queued `[INJECTED]` messages to
        // its in-memory context. The REPL also still writes the
        // inject to `role_history.messages` so the next round's
        // replay sees it.
        runner = runner.with_inject_worktree_root(worktree_root.clone());
        runners.push((role_id, runner));
    }

    run_hil_repl(session_arc, runners, scheduler, no_ask_human, renderer).await
}

async fn run_hil_repl(
    session_arc: std::sync::Arc<tokio::sync::Mutex<latte_agent_core::session::SessionManager>>,
    mut runners: Vec<(String, AgentRunner)>,
    mut scheduler: RoundScheduler,
    _no_ask_human: bool,
    renderer: &dyn latte_agent_core::renderer::ChatRenderer,
) -> AnyResult {
    use crate::commands::repl::{parse_repl_line, ReplInput};
    use latte_agent_core::session::SessionState;
    use latte_ai::models::{Message, Role as MsgRole};

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    print!("> ");
    use std::io::Write;
    stdout.flush()?;

    // Drive a stub round-robin loop (HIL v1.1 phase 7). Each round:
    //   1. read one line of user input (slash command or role inject or manager input)
    //   2. emit RoundStarted
    //   3. iterate roles in scheduler.order:
    //        - drain `.latte/inject/<role>.txt` into the role's history
    //        - slice plan.md for the role and append as a synthetic user message
    //        - advance_turn
    //        - emit the stub line (actual LLM call deferred to a later phase)
    //        - feed the Supervisor; pause the session if it triggers
    //   4. emit RoundEnded and advance_turn again
    'rounds: for round_num in 1..=scheduler.max_rounds {
        // === v1.3a full: ask_human resume ===
        // If the session was paused by ask_human (the previous round's
        // tool call triggered pause_with_reason), the human's reply
        // resumes the session. Read the human's input from stdin, then
        // call resume_with_message(role_id, reply) which appends a
        // synthetic user message to the role's history and transitions
        // Paused -> Resumed. Then call start() to transition
        // Resumed -> Running so the rest of this round's role loop
        // (and the inner `state() != Running` check) proceeds normally.
        {
            let mgr = session_arc.lock().await;
            if mgr.state() == SessionState::Paused {
                if let Some(role_id) = parse_ask_human_role(&mgr) {
                    // Pause_reason format: "ask_human: <role> asked: <question>"
                    let question = parse_ask_human_question(&mgr);
                    renderer.on_status(&format!("[ask_human] {} asked: {}", role_id, question)).await;
                    renderer.on_status("[ask_human] Type your reply and press Enter to resume the session:").await;
                    print!("> ");
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                    drop(mgr);  // release lock before blocking on stdin

                    let mut reply_line = String::new();
                    let n = stdin.lock().read_line(&mut reply_line)?;
                    if n == 0 {
                        break 'rounds; // EOF
                    }
                    let reply = reply_line.trim().to_string();
                    let reply = if reply.is_empty() {
                        // Empty reply: still resume but with a placeholder
                        // so the round can continue.
                        "(no reply)".to_string()
                    } else {
                        reply
                    };

                    let mut mgr = session_arc.lock().await;
                    mgr.resume_with_message(&role_id, &reply)?;
                    // Resumed -> Running so the per-role `state() != Running`
                    // check below lets the round proceed.
                    let _ = mgr.start();
                    renderer.on_status(&format!("[ask_human] session resumed with reply: {} chars", reply.len())).await;
                    // Fall through: the for round_num loop continues. The
                    // session is now Running, and on the next iteration
                    // mgr.state() != Paused, so the rest of the loop runs.
                } else {
                    // Paused but not from ask_human (e.g. /pause or supervisor).
                    // Treat as a manual-pause; the operator must /quit or
                    renderer.on_status("[session is Paused — type /quit to exit, or any input to continue from the manager turn]").await;
                    print!("> ");
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                    drop(mgr);

                    let mut reply_line = String::new();
                    let n = stdin.lock().read_line(&mut reply_line)?;
                    if n == 0 {
                        break 'rounds;
                    }
                    let trimmed = reply_line.trim();
                    if trimmed == "/quit" || trimmed == "q" {
                        let mut mgr = session_arc.lock().await;
                        let _ = mgr.resume();
                        mgr.mark_done()?;
                        break 'rounds;
                    }
                    // Any other input: just resume (the input is discarded
                    // for this v1.3a; future versions may route it).
                    let mut mgr = session_arc.lock().await;
                    let _ = mgr.resume();
                }
            }
        }

        // Read one line of user input per round.
        let mut line = String::new();
        let n = stdin.lock().read_line(&mut line)?;
        if n == 0 {
            break 'rounds; // EOF
        }
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            match parse_repl_line(trimmed) {
                Ok(ReplInput::Empty) => {},
                Ok(ReplInput::Cmd { name, arg }) if name == "pause" => {
                    let mut mgr = session_arc.lock().await;
                    match arg {
                        Some(role_id) => {
                            // Per-role pause (HIL v1.4): flag one role and
                            // keep the session running for the others.
                            if !mgr.record().roles.iter().any(|r| r.role_id == role_id) {
                                renderer.on_error(&format!("[error: unknown role '{}']", role_id)).await;
                            } else {
                                mgr.pause_role(&role_id, &format!("user /pause {}", role_id))?;
                                renderer.on_status(&format!("[role paused: {}]", role_id)).await;
                            }
                        }
                        None => {
                            // Global pause — halts the whole session.
                            mgr.pause("user /pause")?;
                            renderer.on_status("[session paused]").await;
                            break 'rounds;
                        }
                    }
                }
                Ok(ReplInput::Cmd { name, arg }) if name == "resume" => {
                    let mut mgr = session_arc.lock().await;
                    match arg {
                        Some(role_id) => {
                            // Per-role resume (HIL v1.4).
                            if !mgr.record().roles.iter().any(|r| r.role_id == role_id) {
                                renderer.on_error(&format!("[error: unknown role '{}']", role_id)).await;
                            } else if mgr.is_role_paused(&role_id) {
                                mgr.resume_role(&role_id)?;
                                renderer.on_status(&format!("[role resumed: {}]", role_id)).await;
                            } else {
                                renderer.on_status(&format!("[role '{}' is not paused]", role_id)).await;
                            }
                        }
                        None => {
                            let paused = mgr.paused_roles();
                            if paused.is_empty() {
                                renderer.on_status("[no individually-paused roles — use /resume <role>]").await;
                            } else {
                                renderer.on_status(&format!("[paused roles: {} — use /resume <role>]", paused.join(", "))).await;
                            }
                        }
                    }
                }
                Ok(ReplInput::Cmd { name, arg: _ }) if name == "quit" => {
                    let mut mgr = session_arc.lock().await;
                    // If the session was paused (e.g. by the supervisor
                    // mid-round), resume first so the Paused -> Done
                    // transition is legal. The state machine forbids
                    // Paused -> Done directly.
                    if mgr.state() == SessionState::Paused {
                        let _ = mgr.resume();
                    }
                    mgr.mark_done()?;
                    break 'rounds;
                }
                Ok(ReplInput::Cmd { name, arg: _ }) if name == "roles" => {
                    let mgr = session_arc.lock().await;
                    let paused = mgr.paused_roles();
                    if paused.is_empty() {
                        renderer.on_status(&format!("[roles: {}]", scheduler.order.join(", "))).await;
                    } else {
                        renderer.on_status(&format!("[roles: {} | paused: {}]", scheduler.order.join(", "), paused.join(", "))).await;
                    }
                }
                Ok(ReplInput::Cmd { name, arg: _ }) if name == "rounds" => {
                    let mgr = session_arc.lock().await;
                    renderer.on_status(&format!("[round: {} / {}]", mgr.record().current_turn, scheduler.max_rounds)).await;
                }
                Ok(ReplInput::Cmd { name, arg: _ }) => {
                    renderer.on_status(&format!("[unknown /{} — known: pause [role], resume [role], quit, roles, rounds]", name)).await;
                }
                Ok(ReplInput::RoleInject { role_id, message }) => {
                    let mgr = session_arc.lock().await;
                    if !mgr.record().roles.iter().any(|r| r.role_id == role_id) {
                        renderer.on_error(&format!("[error: unknown role '{}']", role_id)).await;
                    } else {
                        queue_inject(mgr.worktree_root(), &role_id, &message)?;
                        renderer.on_status(&format!("[{} queue: +1 message]", role_id)).await;
                    }
                }
                Ok(ReplInput::ManagerInput { message }) => {
                    let mut mgr = session_arc.lock().await;
                    mgr.append_to_role("manager", Message::user(message.clone()))?;
                    renderer.on_status(&format!("[manager turn enqueued: {} chars]", message.len())).await;
                }
                Err(e) => renderer.on_error(&format!("[parse error: {:?}]", e)).await,
            }
        }

        // Round started
        {
            let mgr = session_arc.lock().await;
            mgr.emit_round_started(round_num, &scheduler.order);
        }

        for role_id in scheduler.order.clone() {
            // Drain inject queue + append plan slice for the role. The
            // actual LLM call per role is intentionally deferred —
            // see plan Phase 7 step 4.
            {
                let mut mgr = session_arc.lock().await;
                // The session is "active" only when Running. Created
                // and Resumed are auto-transitioned to Running by
                // `SessionManager::start()` at session creation
                // time (HIL v1.3). Any other state (Paused / Done /
                // Failed) skips the round.
                if mgr.state() != SessionState::Running {
                    renderer.on_status(&format!("[session not running — current state: {:?}]", mgr.state())).await;
                    continue 'rounds;
                }
                // Per-role pause (HIL v1.4): skip only this role's turn
                // while the rest of the round proceeds. Orthogonal to
                // the global state check above — `continue` (this role)
                // rather than `continue 'rounds` (whole round).
                if mgr.is_role_paused(&role_id) {
                    renderer.on_status(&format!("[{} round {}: skipped — role paused]", role_id, round_num)).await;
                    continue;
                }
                let queue_path = mgr.worktree_root().join(".latte").join("inject").join(format!("{}.txt", role_id));
                if queue_path.exists() {
                    if let Ok(content) = std::fs::read_to_string(&queue_path) {
                        let synth = Message {
                            role: MsgRole::User,
                            content: vec![latte_ai::models::ContentPart::text(format!("[INJECTED]\n{}", content))],
                            tool_call_id: None,
                            tool_calls: None
                        };
                        let _ = std::fs::remove_file(&queue_path);
                    }
                }
                // Slice plan.md for this role (H2-tagged section + initial-prompt).
                let plan_slice = plan_md_slice_for(&mgr.record().plan_md, &role_id);
                if !plan_slice.is_empty() {
                    let synth = Message {
                        role: MsgRole::User,
                            content: vec![latte_ai::models::ContentPart::text(format!("[PLAN SLICE]\n{}", plan_slice))],
                            tool_call_id: None,
                            tool_calls: None
                    };
                    mgr.append_to_role(&role_id, synth).ok();
                }
                mgr.advance_turn().ok();
            }

            // Find the runner for this role. The Vec is built once
            // in `run_hil_chat` and lives for the whole REPL
            // session, so a missing entry is a programming error.
            let runner = runners
                .iter_mut()
                .find(|(id, _)| id == &role_id)
                .map(|(_, r)| r)
                .expect("runner for role should exist");

            // Replay the role's persistent history into the
            // runner's `ConversationContext`. The runner was built
            // empty in `run_hil_chat`; on every round we clear and
            // copy the latest `role_history(role_id)` into the
            // context so the model sees all prior turns (including
            // any specialist output from earlier in the same round
            // that the manager needs to read). The synthetic
            // `[INJECTED]` and `[PLAN SLICE]` messages added above
            // are already in `role_history`, so they replay along
            // with everything else.
            {
                let mgr = session_arc.lock().await;
                let history = mgr.role_history(&role_id);
                let ctx = runner.context_mut();
                ctx.clear();
                for m in history {
                    ctx.push(m);
                }
            }

            // Drive the LLM. We pass no new messages — everything
            // the model needs is in the replayed context.
            renderer.on_status(&format!("[{} round {}: calling LLM...]", role_id, round_num)).await;
            let turn_result = runner.run_turn(&[], None).await;
            let new_assistant_text = match turn_result {
                Ok(text) => {
                    renderer.on_status(&format!("[{} round {}: ok, {} chars]", role_id, round_num, text.len())).await;
                    text
                }
                Err(e) => {
                    // Don't pause on a transient LLM error — the
                    // supervisor's dead-loop window will catch
                    // repeated failures via the decision_kind
                    // signal below. We emit the error so the
                    // operator sees what happened, and leave the
                    // assistant message empty so the role history
                    // doesn't grow on a hard failure.
                    renderer.on_error(&format!("[{} round {}: error: {}]", role_id, round_num, e)).await;
                    String::new()
                }
            };

            // Persist the assistant turn back to the
            // SessionManager so the next round's replay sees it.
            // Seed the supervisor with `runner.last_decision_kind()`
            // (text vs tool_call vs delegate vs ask_human) instead
            // of the v1.1 stub's raw text dump.
            {
                let mut mgr = session_arc.lock().await;
                if !new_assistant_text.is_empty() {
                    let assistant_msg = Message {
                        role: MsgRole::Assistant,
                        content: vec![latte_ai::models::ContentPart::text(new_assistant_text)],
                        tool_call_id: None,
            tool_calls: None
                    };
                    if let Err(e) = mgr.append_to_role(&role_id, assistant_msg) {
                        renderer.on_error(&format!("[{} round {}: failed to append assistant turn: {}]", role_id, round_num, e)).await;
                    }
                }
            }
            let decision_kind = runner.last_decision_kind();
            let pause_reason = scheduler.supervisor.observe(&role_id, 100, &decision_kind);
            if let Some(reason) = pause_reason {
                {
                    let mut mgr = session_arc.lock().await;
                    let _ = mgr.pause_with_reason(&reason);
                }
                let mgr = session_arc.lock().await;
                mgr.emit_round_ended(round_num);
                renderer.on_status(&format!("[supervisor pause: {}]", reason)).await;
                continue 'rounds;
            }
        }

        // Round ended
        {
            let mut mgr = session_arc.lock().await;
            mgr.emit_round_ended(round_num);
            mgr.advance_turn().ok();
        }
        print!("> ");
        stdout.flush()?;
    }

    Ok(())
}

fn queue_inject(
    worktree_root: &Path,
    role_id: &str,
    message: &str,
) -> std::io::Result<()> {
    let dir = worktree_root.join(".latte").join("inject");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.txt", role_id));
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(f, "{}", message)?;
    Ok(())
}


// =====================================================================
// ask_human tool registration (HIL v1.1 phase 6)
// =====================================================================
//
// `ask_human` is registered as a per-specialist tool (NOT for manager).
// The tool does not return a value at the call site; instead, the
// session auto-transitions to `Paused` with `pause_reason = "ask_human:
// <role> asked: <question>"`, emits a `TraceEvent::AskHuman` (if a sink
// is attached — no-op otherwise), and the REPL driver surfaces the
// question to the human. The human's reply resumes the session and the
// next round (or the current round's remaining roles) re-invokes the
// target role's `run_turn` with the human's reply in its history.
pub fn register_ask_human_tool(
    tm: &Arc<dyn latte_rs_agent_tools::types::ToolManager>,
    session: std::sync::Arc<tokio::sync::Mutex<latte_agent_core::session::SessionManager>>,
    role_id: String,
) {
    use latte_rs_agent_tools::error::ToolError;
    use latte_rs_agent_tools::types::{
        PropertyType, SharedToolHandler, Tool, ToolExecutionContext, ToolInputProperty,
        ToolInputSchema,
    };

    let input_schema = ToolInputSchema {
        schema_type: latte_rs_agent_tools::types::SchemaType,
        properties: vec![(
            "question".into(),
            ToolInputProperty {
                property_type: PropertyType::String,
                description: Some(
                    "Free-form question to surface to the human. The session pauses with this question; the human's reply resumes the session and is appended to the calling role's history.".into(),
                ),
                enum_values: None,
                minimum: None,
                maximum: None,
                min_length: None,
                max_length: None,
            },
        )]
        .into_iter()
        .collect(),
        required: Some(vec!["question".into()]),
        ..Default::default()
    };

    // The handler is async (BoxFuture). We use `tokio::sync::Mutex::blocking_lock`
    // inside the future because tool handlers run on the agent's blocking
    // dispatch thread (see `register_delegate_tool` for the same pattern with
    // `Arc<Semaphore>` — synchronous locks inside a BoxFuture are fine here
    // since the future itself is invoked from a `spawn_blocking` context by
    // `ToolManager::execute`).
    let role_for_handler = role_id.clone();
    let session_for_handler = session.clone();
    let handler: SharedToolHandler = std::sync::Arc::new(
        move |input: serde_json::Value, _ctx: ToolExecutionContext| {
            let role_for_closure = role_for_handler.clone();
            let session_for_closure = session_for_handler.clone();
            Box::pin(async move {
                let question = input
                    .get("question")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let reason = format!("ask_human: {} asked: {}", role_for_closure, question);
                let mut mgr = session_for_closure.lock().await;
                if let Err(e) = mgr.pause_with_reason(&reason) {
                    eprintln!("[ask_human] pause_with_reason failed: {}", e);
                }
                mgr.emit_ask_human_event(&role_for_closure, &question);
                Err(ToolError::Other(format!(
                    "session paused: ask_human from {}",
                    role_for_closure
                )))
            })
        },
    );

    let tool = Tool::builder(
        "ask_human".to_string(),
        "Ask the human a question and pause the session. The session transitions to Paused with reason `ask_human: <role> asked: <question>`, a TraceEvent::AskHuman is emitted, and the human's reply resumes the session and is appended to the calling role's history.".to_string(),
        input_schema,
        handler,
    )
    .build();

    // Register the tool on the manager's tool registry. The second argument
    // is an optional package namespace — we pass `Some("specialist")` to
    // mirror how `register_delegate_tool` tags the manager-only tool with
    // `Some("manager")`, so trace consumers can filter by namespace.
    tm.register(tool, Some("specialist"));
}

/// Parse the role_id out of a pause_reason of the form
/// "ask_human: <role_id> asked: <question>". Returns None if the
/// pause_reason does not match this format.
fn parse_ask_human_role(mgr: &SessionManager) -> Option<String> {
    let reason = mgr.record().pause_reason.as_deref()?;
    let after = reason.strip_prefix("ask_human: ")?;
    let role_end = after.find(" asked: ")?;
    Some(after[..role_end].to_string())
}

/// Parse the question out of a pause_reason of the form
/// "ask_human: <role_id> asked: <question>". Returns "" if the
/// pause_reason does not match.
fn parse_ask_human_question(mgr: &SessionManager) -> String {
    let Some(reason) = mgr.record().pause_reason.as_deref() else {
        return String::new();
    };
    let Some(after) = reason.strip_prefix("ask_human: ") else {
        return String::new();
    };
    match after.find(" asked: ") {
        Some(idx) => after[idx + " asked: ".len()..].to_string(),
        None => String::new(),
    }
}
