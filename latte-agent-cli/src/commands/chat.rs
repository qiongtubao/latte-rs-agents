//! `latte-agent chat` — single-role REPL.
//!
//! Runs an interactive chat session with one agent (selected by role id
//! or falling back to "manager"). Built-in slash commands switch roles,
//! switch model tier, clear context, and save/load sessions.

use std::io::{self, BufRead, BufWriter, IsTerminal, Write};
use std::sync::Arc;

use clap::Args;
use latte_agent_core::agent::{Agent, AgentRunner};
use latte_agent_core::config::AgentConfig;
use latte_agent_core::model_resolver::{ModelResolver, ModelTier};
use latte_ai::models::{Message, Role as MsgRole};
use latte_ai::params::GenerateParams;

use super::config_layer::{self, CliOverrides};
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

    /// Path to agents config (file or directory).
    #[arg(long, default_value = ".latte/agents")]
    pub agents_config: String,

    /// Path to models config.
    #[arg(long, default_value = ".latte/models.toml")]
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

        let mut session = ChatSession::new(
            merged,
            resolver,
            default_params,
            runner,
            role_id,
            initial_tier,
            self.model_id.clone(),
        );
        session.log = Some(log);
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
        loop {
            print!("{}> ", session.role_id);
            stdout.flush()?;

            let mut line = String::new();
            let n = stdin.lock().read_line(&mut line)?;
            if n == 0 {
                println!();
                break;
            }
            let line = line.trim_end_matches(['\n', '\r']).to_string();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('/') {
                if session.handle_command(&line).await? {
                    break;
                }
                continue;
            }
            session.turn(&line).await?;
            if let Some(resp) = &session.last_response {
                println!("\n{}\n", resp);
            }
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
        }
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
        match self.runner.run_turn(&msgs, None).await {
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
                        let history: Vec<Message> =
                            self.runner.context().messages().to_vec();
                        match build_runner(
                            &self.merged,
                            &self.resolver,
                            &self.default_params,
                            &role_id,
                            new_tier,
                            self.primary_id.as_deref(),
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
        role.system_prompt
            .push_str(&tool_usage_prompt(&role.allowed_tools));
    }
    let agent = Agent::new_with_chain(
        role_id.to_string(),
        role.clone(),
        models,
        default_params.clone(),
    )?;
    let runner = if !role.allowed_tools.is_empty() {
        let tm = build_tool_manager(&role.allowed_tools).await
            .map_err(|e| format!("tool setup failed: {}", e))?;
        AgentRunner::new_with_tools(agent, tm, 8)
    } else {
        AgentRunner::new(agent)
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
    let keep: std::collections::HashSet<String> = allowed
        .iter()
        .flat_map(|s| vec![s.to_lowercase(), s.clone()])
        .collect();
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

fn print_help() {
    println!(
        "Commands:\n\
         /role <id>           Switch to a different role (preserves history)\n\
         /model <tier>        Switch model tier: premium | standard | budget\n\
         /roles               List available roles\n\
         /clear               Clear conversation history\n\
         /history             Show recent messages\n\
         /save <file>         Save conversation to a JSONL file\n\
         /load <file>         Load conversation from a JSONL file\n\
         /help                Show this help\n\
         /exit | /quit        Exit the chat"
    );
}

/// Build the "Tool usage" section appended to a role's system
/// prompt when the role has `allowed_tools` set. Teaches the model
/// the `<tool_call>name args</tool_call>` text protocol that
/// `AgentRunner` parses, and lists exactly the tools this role is
/// permitted to call (so it doesn't waste a turn trying a name that
/// doesn't exist).
pub(crate) fn tool_usage_prompt(allowed: &[String]) -> String {
    let tool_list = allowed.join(", ");
    format!(
        r#"

## Tool usage

You have access to these tools (and only these): {tool_list}. To call
one, emit a single line in EXACTLY this format:

    <tool_call><name> {{<json-args>}}</tool_call>

Use whichever name is in your allowed list. Examples for `read`,
`list`, `search`:

    <tool_call>list {{"path": "."}}</tool_call>
    <tool_call>read {{"path": "README.md"}}</tool_call>
    <tool_call>search {{"path": "src", "pattern": "TODO", "max_results": 20}}</tool_call>

After you emit one or more `<tool_call>` lines, the system will run
them and feed the results back to you as a new turn. You can call
multiple tools in one response. Once you have enough information to
answer the user, respond in plain text (no `<tool_call>` block) and
the loop will end.
"#
    )
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