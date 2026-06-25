//! `latte-agent chat` — single-role REPL.
//!
//! Runs an interactive chat session with one agent (selected by role id
//! or falling back to "manager"). Built-in slash commands switch roles,
//! switch model tier, clear context, and save/load sessions.

use std::io::{self, BufRead, BufWriter, IsTerminal, Write};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::time::timeout as tokio_timeout;

/// Max concurrent `delegate` tool calls per manager session.
const DEFAULT_DELEGATE_CONCURRENCY: usize = 4;
/// Per-specialist wall-clock timeout in seconds.
const DEFAULT_DELEGATE_TIMEOUT_SECS: u64 = 60;

use clap::Args;
use latte_agent_core::agent::{Agent, AgentRunner};
use latte_agent_core::config::AgentConfig;
use latte_agent_core::model_resolver::{ModelResolver, ModelTier};
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
    #[arg(short = 't', long, default_value = "standard")]
    pub tier: String,

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
    #[arg(long, default_value = "config/agents")]
    pub agents_config: String,

    /// Path to models config.
    #[arg(long, default_value = "config/models.toml")]
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
    /// `~/.latte/traces/<session-id>.jsonl`. Without this flag,
    /// only the always-on `~/.latte/sessions/<id>.idx` metadata
    /// index is written.
    #[arg(long)]
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

    /// Opt out of the always-on metadata index. Useful on shared
    /// machines where the file's contents are sensitive.
    #[arg(long)]
    pub no_session_index: bool,
}
impl ChatCmd {
    pub async fn run(&self) -> AnyResult {
        // Per-session log file (also echoed to stderr). Always on —
        // the whole point of `-m` + the resolver work is to debug
        // "why did this fall through to that model", and a single
        // tail-friendly file beats a heap of `tracing::debug!` lines
        // the user has to set RUST_LOG=trace to see.
        let log = super::chatlog::ChatLog::open();
        if let Some(p) = log.path() {
            eprintln!("[chatlog] writing session events to {}", p.display());
        }
        log.info(
            "session start",
            &[
                ("role", self.role.clone().unwrap_or_else(|| "manager".into())),
                ("tier", self.tier.clone()),
                ("model_id", self.model_id.clone().unwrap_or_default()),
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
        let merged = resolved.config;
        let resolver = resolved.resolver;
        let default_params = GenerateParams::default();

        let initial_role = self.role.clone().unwrap_or_else(|| "manager".into());
        let initial_tier = parse_tier(&self.tier)?;
        let initial_primary = self.model_id.as_deref();

        // Build the initial runner.
        let (runner, role_id) = match build_runner(
            &merged,
            &resolver,
            &default_params,
            &initial_role,
            initial_tier,
            initial_primary,
            &super::DebugFlags { debug: self.debug, debug_format: self.debug_format, debug_hooks: self.debug_hooks.clone(), no_session_index: self.no_session_index },
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
            ],
        );
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
            debug_flags: super::DebugFlags {
                debug: self.debug,
                debug_format: self.debug_format,
                debug_hooks: self.debug_hooks.clone(),
                no_session_index: self.no_session_index,
            },
        };
        // If --resume was passed, load the saved history into the
        // session context before the user starts chatting.
        if let Some(path) = &self.resume {
            let history = load_session(path)
                .map_err(|e| format!("failed to load resume file '{}': {}", path, e))?;
            session.load_history(history);
            eprintln!("[resume] loaded {} messages from {}", session.runner.context().messages().len(), path);
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
    last_response: Option<String>,
    debug_flags: super::DebugFlags,
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
            debug_flags,
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
            content: user_input.to_string(),
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
                        let preview: String = m.content.chars().take(80).collect();
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
) -> AnyResult<(AgentRunner, String)> {
    let template = merged
        .roles
        .get(role_id)
        .ok_or_else(|| format!("role '{}' not found", role_id))?
        .clone();
    let role = template.resolve(default_params).await?;
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
            if chain.iter().any(|m| m.id == *cid) {
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
    let mut role = role;

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

        role.system_prompt.push_str(&prompt);
    }
    let agent = Agent::new_with_chain(
        role_id.to_string(),
        role.clone(),
        models,
        default_params.clone(),
    )?;
    let sink = super::build_debug_sink(&role_id, debug_flags);
    let hooks = super::build_debug_hooks(debug_flags);
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
            std::env::var("LATTE_AGENT_DELEGATE_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_DELEGATE_TIMEOUT_SECS),
            Arc::new(latte_agent_core::trace::NullSink),
        )
        .await
        .map_err(|e| format!("delegate tool setup failed: {}", e))?;
        AgentRunner::new_with_tools(agent, tm, 16)
            .with_sink(Arc::clone(&sink))
            .with_hooks(Arc::clone(&hooks))
            .with_role(role_id)
    } else {
        AgentRunner::new(agent)
            .with_sink(Arc::clone(&sink))
            .with_hooks(Arc::clone(&hooks))
            .with_role(role_id)
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

/// Register a `delegate` tool on the tool manager. The tool lets the
/// manager agent dispatch subtasks to specialist roles (programmer,
/// architect, reviewer, etc.) and receive their responses.
async fn register_delegate_tool(
    tm: &Arc<dyn latte_rs_agent_tools::types::ToolManager>,
    merged: Arc<AgentConfig>,
    resolver: Arc<ModelResolver>,
    default_params: GenerateParams,
    delegate_sem: Arc<Semaphore>,
    delegate_timeout_secs: u64,
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
            let timeout_s = delegate_timeout_secs;
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
                let agent = Agent::new_with_chain(
                    role_id.clone(),
                    role.clone(),
                    models,
                    default_params.clone(),
                )
                .map_err(|e| {
                    tool_err(format!("failed to create agent for '{}': {}", role_id, e))
                })?;
                // Give the specialist the tools its role template allows
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
                    Some(tm) => AgentRunner::new_with_tools(agent, tm, 8)
                        .with_sink(scoped_sink.clone())
                        .with_role(role_id.clone()),
                    None => AgentRunner::new(agent)
                        .with_sink(scoped_sink.clone())
                        .with_role(role_id.clone()),
                };
                let msgs = vec![Message {
                    role: MsgRole::User,
                    content: task,
                }];
                // Acquire concurrency permit (blocks if too many
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
                        return Err(tool_err(format!(
                            "delegate to '{}' failed: {}", role_id, e
                        )));
                    }
                    Err(_) => {
                        return Err(tool_err(format!(
                            "delegate to '{}' timed out after {}s",
                            role_id, timeout_s
                        )));
                    }
                };
                drop(_permit);
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

You also have access to a `delegate` tool that dispatches a subtask
to a specialist agent and returns the result. Use it for substantive
code analysis or work that a specialist is best at.

<tool_call>delegate {"role": "programmer", "task": "Read src/agent.rs and summarize the AgentRunner::run_turn flow"}</tool_call>

Available specialist roles: programmer, architect, reviewer, tester,
security, devops, designer, tech_writer, pm.

When to delegate:
- Deep code analysis → programmer
- Architecture / design review → architect
- Code quality review → reviewer
- Testing strategy / bug analysis → tester
- Security audit → security

You can call `delegate` multiple times in parallel (in one response
with multiple `<tool_call>` blocks) to fan out independent subtasks.
Synthesize the results into a coherent answer.
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
        Message {
            role,
            content: content.to_string(),
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
        assert_eq!(restored[0].content, "you are helpful");
        assert_eq!(restored[2].content, "hello");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_session_skips_blank_lines() {
        let path = std::env::temp_dir().join("latte_chat_session_blank.jsonl");
        let _ = std::fs::remove_file(&path);

        let body = "{\"role\":\"user\",\"content\":\"a\"}\n\n{\"role\":\"assistant\",\"content\":\"b\"}\n";
        std::fs::write(&path, body).unwrap();
        let msgs = load_session(path.to_str().unwrap()).unwrap();
        assert_eq!(msgs.len(), 2);

        let _ = std::fs::remove_file(&path);
    }
}