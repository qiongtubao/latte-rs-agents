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

/// "用户主动中止本轮"的哨兵错误串。REPL 主循环据此跳过 auto-save 与
/// 退出，只是回到提示符——Ctrl-C 停一轮不该终结整个会话。
const TURN_CANCELLED: &str = "__latte_turn_cancelled__";

/// `println!` 的分离模式替代品。参数与 `println!` 完全一致，便于原地替换。
macro_rules! ui_println {
    () => { ui_out("") };
    ($($arg:tt)*) => { ui_out(&format!($($arg)*)) };
}


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
        // Advisor 监察：旁路订阅会话事件流，异常时经 hint 注入纠偏
        // （通道 A）+ 广播 🦉 气泡（通道 B）。此前只有 UI 会拉起它，
        // chat 的顶层 turn 完全没有监察。
        // 状态行的耗时刷新（每秒）。分离模式下显示「在干什么 · 干了多久」。
        spawn_ui_status_ticker();
        ensure_cli_advisor_monitor(
            &merged,
            &resolver,
            default_params.clone(),
            &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            &initial_role,
        );
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

        let _term_width = style::terminal_width();
        // 输入/显示分离：TTY 下历史推进 scrollback、状态行与输入行固定
        // 在底部；非 TTY（管道 / 重定向 / e2e）`enter()` 返回 None，
        // 全部调用点退回纯文本路径，输出逐字节不变。
        *split_ui().lock() = super::split_screen::SplitScreen::enter("› ");
        // 守卫必须是局部变量：static 永不 drop，靠 `Drop for SplitScreen`
        // 恢复终端是不成立的（实机抓到 `?2004l` 从未下发）。
        let _ui_guard = UiGuard;
        // 立刻拉起常驻按键线程：输入行必须在**第一个 turn 期间**就已经
        // 活着，不能等到第一次 read_user_line 才起——否则第一轮等待时
        // 用户打的字仍然看不见。
        spawn_input_thread();
        loop {
            let model_id = session.primary_model_id();
            let role_icon = session.role_icon();
            // Prompt: `👔 manager · deepseek-v4-flash › ` (colorized when TTY).
            let prompt = style::render_prompt(role_icon, &session.role_id, model_id);
            let Some(line) = read_user_line(&prompt).await? else {
                break;
            };

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
            // advisor 宿主接入：① 记录本轮用户输入（审查 prompt 的
            // 「主诉求」基准）；② 取走上一轮 monitor 注入的纠正提示，
            // 拼进本轮输入——与 controller 的 hint 队列（通道 A）等价。
            let host = cli_advisor_host();
            *host.last_input.lock() = line.trim().to_string();
            let line = {
                let hints: Vec<String> = std::mem::take(&mut *host.hints.lock());
                if hints.is_empty() {
                    line.clone()
                } else {
                    for h in &hints {
                        ui_emit(&format!("🦉 advisor: {h}"));
                    }
                    format!("{line}\n\n【advisor 纠正提示】\n{}", hints.join("\n"))
                }
            };
            if let Err(e) = session.turn(&line).await {
                // 用户 Ctrl-C 中止：回到提示符，不 auto-save、不退出。
                if e.to_string().contains(TURN_CANCELLED) {
                    continue;
                }
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
                    // 必须走 ui_emit：分离模式下直接 print! 会覆盖在
                    // 底部的状态行/输入行上。非 TTY 时 ui_emit 退回
                    // eprintln，但 TTY 判断已经保证走不到那条分支。
                    ui_emit(body.trim_end_matches('\n'));
                } else {
                    // 非 TTY：保持原样 println 到 stdout（e2e 断言它）。
                    println!("{}", resp);
                    io::stdout().flush()?;
                }
            }
            let usage_after = session.runner.total_usage();
            let _in_delta = usage_after.input_tokens - usage_before.input_tokens;
            let _out_delta = usage_after.output_tokens - usage_before.output_tokens;
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
        // 生成期间显示进度。两条路：
        //
        // - **分离模式**：走状态行（我们自己重绘、位置固定）。独立
        //   spinner 在这里是**有害**的——它往 stderr 写 `\r\x1b[K`，
        //   会把光标拉回行首并清行，正好擦掉底部的输入行。原注释说的
        //   "用 `\r` 所以不会打架"只对旧的交错式输出成立。
        // - **非分离模式**（管道 / 非 TTY）：保持原来的 braille spinner。
        let split = split_ui().lock().is_some();
        let spinner = if split {
            ui_set_activity(Some("thinking…"));
            None
        } else {
            Some(style::Spinner::start("thinking…"))
        };
        // 可取消地跑：Ctrl-C（turn 进行中）置 `turn_cancel_flag`，这里
        // 轮询到就 abort 掉 join handle。与 UI 的 driver 同一模式
        // （`controller.rs` 里也是 spawn + 轮询 + `run_handle.abort()`），
        // 因为 `AgentRunner` 本身没有取消入口，只能靠丢弃 future 中止。
        //
        // 分离模式才启用：非 TTY（管道 / e2e）没有 Ctrl-C 来源，直接
        // await 保持原路径逐字节不变。
        let result = if split {
            turn_cancel_flag().store(false, std::sync::atomic::Ordering::SeqCst);
            turn_in_flight().store(true, std::sync::atomic::Ordering::SeqCst);
            // runner 是 `&mut self` 借用，移不进 `tokio::spawn`，所以用
            // `select!` 与取消轮询竞速：取消胜出时 `run_turn` 的 future
            // 被丢弃，等价于 abort（在途 HTTP 与工具循环一起停）。
            let outcome = {
                let cancel_watch = async {
                    loop {
                        if turn_cancel_flag().load(std::sync::atomic::Ordering::SeqCst) {
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(120)).await;
                    }
                };
                tokio::select! {
                    r = self.runner.run_turn(&msgs, None) => Some(r),
                    _ = cancel_watch => None,
                }
            };
            turn_in_flight().store(false, std::sync::atomic::Ordering::SeqCst);
            match outcome {
                Some(r) => r,
                None => {
                    ui_emit("⛔ 本轮已中止（Ctrl-C）");
                    ui_set_activity(None);
                    // 用哨兵串标识"用户主动中止"，与真正的 turn 失败区分。
                    // 不区分的话：REPL 主循环拿到 Err 会 auto-save +
                    // 打印 `turn failed` + **退出整个会话**——而用户
                    // 按 Ctrl-C 只想停这一轮（实测复现）。
                    return Err(TURN_CANCELLED.into());
                }
            }
        } else {
            self.runner.run_turn(&msgs, None).await
        };
        match spinner {
            Some(sp) => sp.stop(),
            None => ui_set_activity(None),
        }
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
                // 攒成一段再输出：分离模式下每次输出都要清+重画整个
                // viewport，逐行发 51 个角色就是 51 次重绘（实测
                // `/roles` 制造了 900+ 个重绘段）。攒批后只有 1 次。
                let mut buf = format!("Available roles ({}):", ids.len());
                for id in ids {
                    if let Some(tpl) = self.merged.roles.get(id) {
                        buf.push_str(&format!(
                            "\n  {} \u{2014} {} [{}]",
                            id, tpl.name, tpl.model_tier
                        ));
                    }
                }
                ui_out(&buf);
            }
            "/role" => {
                let Some(id) = rest.first() else {
                    ui_println!("usage: /role <id>");
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
                        ui_println!(
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
                        ui_println!("error: {}", e)
                    }
                }
            }
            "/model" => {
                let Some(t) = rest.first() else {
                    ui_println!("usage: /model <premium|standard|budget>");
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
                                ui_println!("Switched to tier {}", self.tier.label());
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
                                ui_println!("error: {}", e)
                            }
                        }
                    }
                     Err(e) => {
                        if let Some(log) = &self.log {
                            log.warn("invalid tier", &[("input", t.to_string())]);
                            log.error("invalid tier", &[("error", e.to_string())]);
                        }
                        ui_println!("error: {}", e);
                    }
                }
            }
            "/clear" => {
                self.runner.context_mut().clear();
                self.last_response = None;
                // advisor 宿主状态也要清：`last_user_input` 是审查 prompt
                // 的「主诉求」基准，残留会让清空后的第一次审查拿**上一个
                // 话题**当准绳；未消费的 hint 会串到新话题上。
                {
                    let host = cli_advisor_host();
                    host.last_input.lock().clear();
                    host.hints.lock().clear();
                }
                ui_println!("Context cleared.");
            }
            "/save" => {
                let Some(path) = rest.first() else {
                    ui_println!("usage: /save <file>");
                    return Ok(false);
                };
                let n = self.runner.context().messages().len();
                save_session(path, self.runner.context().messages())?;
                ui_println!("Saved {} messages to {}", n, path);
            }
            "/load" => {
                let Some(path) = rest.first() else {
                    ui_println!("usage: /load <file>");
                    return Ok(false);
                };
                let msgs = load_session(path)?;
                self.runner.context_mut().clear();
                for m in msgs {
                    self.runner.context_mut().push(m);
                }
                ui_println!(
                    "Loaded {} messages from {}",
                    self.runner.context().messages().len(),
                    path
                );
            }
            "/history" => {
                let msgs = self.runner.context().messages();
                if msgs.is_empty() {
                    ui_println!("(no messages)");
                } else {
                    // 同上：长会话里 /history 会有几十行。
                    let mut buf = String::new();
                    for (i, m) in msgs.iter().enumerate() {
                        let preview: String = m.as_text().chars().take(80).collect();
                        if !buf.is_empty() {
                            buf.push('\n');
                        }
                        buf.push_str(&format!("{:>3} [{:?}] {}", i, m.role, preview));
                    }
                    ui_out(&buf);
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
                ui_println!(
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
                    ui_println!("role '{}' has no tools configured", self.role_id);
                } else {
                    // 同 /roles：攒批，避免逐行重绘。
                    let mut buf =
                        format!("role '{}' tools ({}):", self.role_id, tools.len());
                    for t in &tools {
                        buf.push_str(&format!("\n  - {t}"));
                    }
                    ui_out(&buf);
                    ui_println!(
                        "\nFormat: <tool_call>{} {{\"arg\": \"value\"}}</tool_call>",
                        tools.first().map(String::as_str).unwrap_or("name")
                    );
                }
            }
            other => {
                ui_println!("unknown command: {} (try /help)", other);
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
        //
        // 门槛对齐 UI：controller 只给 `allowed_tools` 里声明了
        // `delegate` 的角色注册。CLI 此前是**无条件**注册——任何角色
        // （programmer / reviewer …）都能派活给别人，绕过了角色配置里
        // 刻意收紧的权限边界，而同一个角色在 UI 里根本没有这个工具。
        if role.allowed_tools.iter().any(|t| t == "delegate") {
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
        }
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
        // 顶层 `ask`（选择题）对齐 UI：controller 路径给每个声明了
        // `ask` 的角色注册 core 版 ask 工具，REPL 路径此前完全没有
        // ——同一个角色（tutor / manager）在 UI 里能弹选择题，在 chat
        // 里调 `ask` 直接 ToolNotFound。
        //
        // 用 fire-and-forget（`blocking = None`），与 controller 顶层
        // turn 的语义一致：结束本轮、答案作为下一条 user 消息回喂。
        // 阻塞版只属于 workflow / delegate 的子代理（它们等得起）。
        // 落盘归属 `(cwd, session_id)` 同样对齐：进程重启后
        // `.latte/pending-asks/` 能把待办弹框补回来。
        if role.allowed_tools.iter().any(|t| t == "ask") {
            latte_agent_core::controller::register_ask_tool(
                &tm,
                cli_session_event_tx().clone(),
                role_id.to_string(),
                None,
                Some((cwd.to_path_buf(), cli_session_id())),
            )
            .map_err(|e| format!("ask tool setup failed: {}", e))?;
        }
        // `plan` / `task_report` 对齐 UI：controller 给声明了它们的角色
        // （manager 两个都声明了）注册，REPL 路径此前完全没有——manager
        // 的 prompt 教它用 `plan` 提交任务候选、用 `task_report` 回报看板
        // 结果，在 chat 里调用却是 ToolNotFound。
        if role.allowed_tools.iter().any(|t| t == "plan") {
            latte_agent_core::controller::register_plan_tool(
                &tm,
                cli_session_event_tx().clone(),
                role_id.to_string(),
                Arc::new(parking_lot::RwLock::new(
                    latte_agent_core::controller::PlanStage::Normal,
                )),
                cwd,
                cli_session_id(),
            )
            .map_err(|e| format!("plan tool setup failed: {}", e))?;
        }
        if role.allowed_tools.iter().any(|t| t == "task_report") {
            latte_agent_core::controller::register_task_report_tool(
                &tm,
                cli_session_event_tx().clone(),
                role_id.to_string(),
            )
            .map_err(|e| format!("task_report tool setup failed: {}", e))?;
        }
        // `generate_image` 对齐 UI（已是 pub，无需改可见性）。
        if role.allowed_tools.iter().any(|t| t == "generate_image") {
            latte_agent_core::image_gen::register_generate_image_tool(
                &tm,
                merged,
                cli_session_event_tx().clone(),
                cwd.to_path_buf(),
                role_id.to_string(),
            )
            .map_err(|e| format!("generate_image tool setup failed: {}", e))?;
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
            AgentRunner::new_with_tools(agent, tm)
                .with_sink(cli_top_level_sink(&sink))
                .with_hooks(Arc::clone(&hooks))
                .with_role(role_id)
        )
    } else {
        with_session(
            AgentRunner::new(agent)
                .with_sink(cli_top_level_sink(&sink))
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
    // Allowlist filter: builtin tools register under flat names
    // ("bash", "read", …) identical to the config names, so the
    // config entries match the registry directly.
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

/// 拉起 CLI 的 advisor 监察（幂等，进程内只跑一份）。
///
/// 与 UI（`sessions.rs`）同构：`AdvisorReviewEngine` + watchdog 笔记 +
/// review 设置 + 给 advisor 自己分配一个 subsession sink（每次 review
/// 的模型往返落 `<cli-sessions>/<sid>/advisor-<micros>.jsonl`）。
/// 配置关掉 advisor 时直接不拉。
fn ensure_cli_advisor_monitor(
    merged: &AgentConfig,
    resolver: &ModelResolver,
    default_params: GenerateParams,
    cwd: &Path,
    watched_role: &str,
) {
    use latte_agent_core::advisor_monitor::{
        AdvisorMonitor, AdvisorMonitorConfig, AdvisorReviewEngine,
    };
    static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if STARTED.get().is_some() {
        return;
    }
    let cfg = AdvisorMonitorConfig {
        enabled: merged.advisor.enabled(),
        ..AdvisorMonitorConfig::default()
    };
    if !cfg.enabled {
        return;
    }
    let _ = STARTED.set(());
    let engine = AdvisorReviewEngine::new(
        Arc::new(merged.clone()),
        Arc::new(resolver.clone()),
        default_params,
    )
    .with_watchdog_notes(cwd.to_path_buf(), cfg.watchdog_notes)
    .with_review_settings(cfg.review_settings);
    let (_sub_id, sink) = cli_subsession_store().create(&cli_session_id(), "advisor");
    let engine = engine.with_subsession_sink(sink);
    AdvisorMonitor::spawn_on_host(
        cli_advisor_host(),
        cfg,
        engine,
        watched_role.to_string(),
    );
}

/// 顶层 runner 的 sink：既有的 trace sink（CliRenderer / jsonl / index）
/// **并上** `ChatEventTraceSink`。
///
/// 后者把 TraceSink 事件转成 `ChatEvent` 送进会话级通道，供 advisor
/// monitor 消费。这样接的好处是渲染路径完全不动——`CliRenderer` 照旧
/// 从 TraceSink 拿事件，monitor 从 broadcast 拿，谁也不用改。
fn cli_top_level_sink(
    base: &Arc<dyn latte_agent_core::trace::TraceSink>,
) -> Arc<dyn latte_agent_core::trace::TraceSink> {
    Arc::new(latte_agent_core::trace::FanOutSink::new(vec![
        Arc::clone(base),
        Arc::new(latte_agent_core::controller::ChatEventTraceSink {
            event_tx: cli_session_event_tx().clone(),
            // 顶层角色不属于任何 subsession。
            sub_id: None,
        }),
    ]))
}

/// CLI REPL 作为 advisor 监察的宿主。
///
/// 修的是：`AdvisorMonitor` 原来只由 `ChatController::spawn` 拉起，而
/// chat 不走 controller，于是顶层 turn 完全没有 advisor 监察——
/// intervene/warn 气泡、派单路由审查、以及全部确定性检测器（D3 文件
/// 不存在连击、D8 串行 delegate…）在 chat 里一个都不跑，同一个会话
/// 在 UI 里跑就有。
///
/// 事件来源不需要改 REPL 的渲染路径：`ChatEventTraceSink` 挂在顶层
/// runner 的 sink fanout 上，把 TraceSink 事件转成 ChatEvent 送进会话
/// 级通道，`CliRenderer` 照旧从 TraceSink 渲染，两者互不干扰。
struct CliAdvisorHost {
    /// monitor 注入的纠正提示。REPL 主循环在下一轮取走，拼进用户输入前。
    hints: Arc<parking_lot::Mutex<Vec<String>>>,
    /// 最近一条用户输入，作为「主诉求」喂给审查 prompt。
    last_input: Arc<parking_lot::Mutex<String>>,
    /// terminate 裁决时置位；REPL 在轮次边界检查。
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

impl latte_agent_core::advisor_monitor::AdvisorHost for CliAdvisorHost {
    fn subscribe(
        &self,
    ) -> tokio::sync::broadcast::Receiver<latte_agent_core::controller::ChatEvent> {
        cli_session_event_tx().subscribe()
    }
    fn event_sender(
        &self,
    ) -> tokio::sync::broadcast::Sender<latte_agent_core::controller::ChatEvent> {
        cli_session_event_tx().clone()
    }
    fn advisor_hint(&self, text: &str) {
        self.hints.lock().push(text.to_string());
    }
    fn last_user_input(&self) -> String {
        self.last_input.lock().clone()
    }
    fn request_cancel_turn(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        // CLI 没有 controller 的 input 通道要唤醒：置标志即可，
        // REPL 在轮次边界读它。
        self.cancel
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async {})
    }
}

/// 进程级单例：REPL 主循环与 monitor 共享同一份 hint / 输入 / 取消状态。
fn cli_advisor_host() -> Arc<CliAdvisorHost> {
    static HOST: std::sync::OnceLock<Arc<CliAdvisorHost>> = std::sync::OnceLock::new();
    HOST.get_or_init(|| {
        Arc::new(CliAdvisorHost {
            hints: Arc::new(parking_lot::Mutex::new(Vec::new())),
            last_input: Arc::new(parking_lot::Mutex::new(String::new())),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    })
    .clone()
}

/// 进程级共享的分离式屏幕。
///
/// 必须共享：REPL 用它读输入，事件消费者用它把历史行推进 scrollback，
/// ask 选择器两样都用。如果消费者绕过它直接 `eprintln!`，输出会覆盖在
/// 输入行上——"分离"就白做了。
///
/// `None` = 非 TTY（管道 / 重定向 / e2e），全部调用点退回纯文本路径。
fn split_ui() -> &'static parking_lot::Mutex<Option<super::split_screen::SplitScreen>> {
    static UI: std::sync::OnceLock<parking_lot::Mutex<Option<super::split_screen::SplitScreen>>> =
        std::sync::OnceLock::new();
    UI.get_or_init(|| parking_lot::Mutex::new(None))
}

/// 当前活动的简报 + 起始时刻，供状态行显示"在干什么 · 干了多久"。
///
/// 分开存而不是把耗时直接写进 `ui_status`：耗时要每秒变，但事件只在
/// step/delegate 边界到达。所以事件只更新"在干什么"，另有一个定时任务
/// 负责把秒数刷上去。
fn ui_activity() -> &'static parking_lot::Mutex<Option<(String, std::time::Instant)>> {
    static A: std::sync::OnceLock<parking_lot::Mutex<Option<(String, std::time::Instant)>>> =
        std::sync::OnceLock::new();
    A.get_or_init(|| parking_lot::Mutex::new(None))
}

/// 设置当前活动（`None` = 空闲，清空状态行）。
fn ui_set_activity(label: Option<&str>) {
    let mut g = ui_activity().lock();
    match label {
        Some(l) => *g = Some((l.to_string(), std::time::Instant::now())),
        None => {
            *g = None;
            drop(g);
            ui_status("");
        }
    }
}

/// 起一个每秒刷状态行的任务（幂等，进程内只跑一份）。
///
/// 只在分离模式下有意义：非 TTY 时 `ui_status` 是空操作，任务空转但
/// 无副作用（每秒一次，可忽略）。
fn spawn_ui_status_ticker() {
    static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let snapshot = ui_activity().lock().clone();
            if let Some((label, started)) = snapshot {
                let secs = started.elapsed().as_secs();
                ui_status(&format!("⚙ {label} · {secs}s"));
            }
        }
    });
}

/// 退出分离模式并恢复终端（幂等）。
///
/// **必须显式调用**：`SplitScreen` 存在 `static` 里，而 Rust 的 static
/// 永不 drop——靠 `Drop for SplitScreen` 恢复终端是不成立的。不调它的
/// 后果是 `latte-agent chat` 退出后用户的终端留在 raw mode（无回显，
/// 只能盲敲 `reset`）+ bracketed paste 未关。实机在伪终端里抓到过：
/// `?2004h` 有、`?2004l` 没有。
fn ui_leave() {
    if let Some(mut ui) = split_ui().lock().take() {
        ui.leave();
    }
}

/// 作用域守卫：REPL 无论正常退出、`?` 早退还是 panic，都恢复终端。
///
/// 是**局部变量**而非 static，所以 Drop 一定会跑——这正是 `static` 做
/// 不到的那一点。
struct UiGuard;

impl Drop for UiGuard {
    fn drop(&mut self) {
        ui_leave();
    }
}

/// 当前是否有 turn 在跑。输入线程据此决定 Ctrl-C 的语义：
/// 跑着 → 中止本轮；空闲 → 只清输入行。
fn turn_in_flight() -> &'static std::sync::atomic::AtomicBool {
    static F: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    &F
}

/// 本轮取消请求。Ctrl-C（turn 进行中）置位，`turn()` 轮询到即 abort。
fn turn_cancel_flag() -> &'static std::sync::atomic::AtomicBool {
    static F: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    &F
}

/// 交互输出（斜杠命令的回显、响应正文）：分离模式走历史区，非 TTY
/// 退回 **stdout** 的 `println!`。
///
/// 与 [`ui_emit`] 的区别只在降级目标：后台事件原本是 `eprintln!`（stderr），
/// 命令输出原本是 `println!`（stdout），而 CLI 侧 e2e 断言的是 stdout。
/// 混用会让 `/roles`、`/history` 这类输出跑到 stderr 上，测试看不见。
fn ui_out(text: &str) {
    let mut g = split_ui().lock();
    match g.as_mut() {
        Some(ui) => ui.emit(text),
        None => println!("{text}"),
    }
}

/// 把一行输出交给分离式屏幕；非 TTY 时退回 `eprintln!`。
///
/// 所有后台事件的输出都必须走这里，不要直接 `eprintln!`。
fn ui_emit(text: &str) {
    let mut g = split_ui().lock();
    match g.as_mut() {
        Some(ui) => ui.emit(text),
        None => eprintln!("{text}"),
    }
}

/// 更新状态行（仅分离模式有效；非 TTY 时静默丢弃——它是易失信息，
/// 塞进被 grep 的 stdout 只会污染 e2e 断言）。
fn ui_status(text: &str) {
    if let Some(ui) = split_ui().lock().as_mut() {
        ui.set_status(text);
    }
}

/// 取首行并按**字符**（非字节）截断，用于事件行的简报。
/// 按字节切会把多字节字符切一半 panic —— 任务描述基本都是中文。
fn first_line_brief(s: &str, max_chars: usize) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    if line.chars().count() <= max_chars {
        return line.to_string();
    }
    let head: String = line.chars().take(max_chars).collect();
    format!("{head}…")
}

/// CLI REPL 的**会话级**事件通道，对齐 UI 的"每 session 一个 broadcast"。
///
/// `latte-agent chat` 一个进程就是一个会话，所以进程级单例与 UI 的
/// per-session 通道语义等价。首次取用时顺带把终端事件消费者拉起来
/// （渲染进度 + 让阻塞式 `ask` 可回答）。
///
/// 为什么不再每次 workflow 建一个通道：`choice::register` /
/// `register_prompt` 会把 sender 克隆存进挂起表，未回答的挂起项让通道
/// 永不关闭。此前 per-run 通道要靠"有界等待 + abort"兜底才不死锁；
/// 会话级通道活到进程结束，根本不需要收尾，那类死锁不可能发生。
fn cli_session_event_tx()
-> &'static tokio::sync::broadcast::Sender<latte_agent_core::controller::ChatEvent> {
    static TX: std::sync::OnceLock<
        tokio::sync::broadcast::Sender<latte_agent_core::controller::ChatEvent>,
    > = std::sync::OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = tokio::sync::broadcast::channel(256);
        // 消费者随进程存活；REPL 退出即进程退出，无需回收。
        spawn_cli_workflow_event_consumer_with(rx, terminal_ask_answerer());
        tx
    })
}

/// CLI 会话 id：`ask` 的落盘补发（`.latte/pending-asks/`）按
/// `(cwd, session_id)` 归属，UI 侧用真实 session id，CLI 用进程 id
/// 保证同一次运行内稳定、跨运行不串。
fn cli_session_id() -> String {
    format!("cli-{}", std::process::id())
}

/// CLI REPL 的子会话存储，对齐 UI 的 `b.subsession_store`。
///
/// UI 落 `<cwd>/.latte/ui-sessions/`；CLI 落 `<cwd>/.latte/cli-sessions/`
/// ——刻意分开：`ui-sessions` 是 UI 的 session 索引根，混进 CLI 的记录
/// 会让 UI 侧列出不存在的会话。两边都能被 `latte-agent debug` 系列读到。
///
/// 没有这个的后果（原状）：CLI 跑 workflow 时每个 specialist 的完整
/// 子会话日志（prompt / 模型往返 / 工具调用）**一条都不落盘**，
/// `latte-agent debug session|trace|tokens` 对 CLI 的 workflow 全瞎，
/// 而同一个 workflow 在 UI 里跑就有完整记录。
fn cli_subsession_store() -> Arc<latte_agent_core::subsession::SubsessionStore> {
    static STORE: std::sync::OnceLock<Arc<latte_agent_core::subsession::SubsessionStore>> =
        std::sync::OnceLock::new();
    STORE
        .get_or_init(|| {
            let base = std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".latte")
                .join("cli-sessions");
            Arc::new(latte_agent_core::subsession::SubsessionStore::with_persistence(base))
        })
        .clone()
}

/// 建 CLI REPL 用的 [`WorkflowRunContext`]，挂在会话级事件通道上。
fn cli_workflow_ctx(
    merged: Arc<AgentConfig>,
    resolver: Arc<ModelResolver>,
    default_params: GenerateParams,
    cwd: PathBuf,
) -> latte_agent_core::workflow::WorkflowRunContext {
    cli_workflow_ctx_on(
        merged,
        resolver,
        default_params,
        cwd,
        cli_session_event_tx().clone(),
    )
}

fn cli_workflow_ctx_on(
    merged: Arc<AgentConfig>,
    resolver: Arc<ModelResolver>,
    default_params: GenerateParams,
    cwd: PathBuf,
    event_tx: tokio::sync::broadcast::Sender<latte_agent_core::controller::ChatEvent>,
) -> latte_agent_core::workflow::WorkflowRunContext {
    let merged_for_advisor = merged.clone();
    latte_agent_core::workflow::WorkflowRunContext {
        merged,
        resolver,
        default_params,
        cwd,
        event_tx,
        cancel_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        turn_cancel_flag: None,
        agent_pause_gate: None, // CLI REPL workflow：无 session agent gate
        depth: 0,
        // 顶层 run：自己就是嵌套链的根。
        root_wf_id: None,
        // 对齐 UI（tasks.rs 的 workflow ctx）：分派建 subsession、过
        // advisor gate。原状是两个都 None，导致同一个 workflow 在 chat
        // 里跑**没有子会话日志、也不过 advisor 的返回审查**——UI 里
        // 会被 intervene 打回重做的产出，chat 里直接放行。
        subsession_store: Some(cli_subsession_store()),
        session_id: Some(cli_session_id()),
        advisor_gate: latte_agent_core::advisor_monitor::AdvisorMonitorConfig {
            enabled: merged_for_advisor.advisor.enabled(),
            ..latte_agent_core::advisor_monitor::AdvisorMonitorConfig::default()
        }
        .runner_gate(),
        // intervene 暂停门（v4 起休眠，UI 侧也常为 None）：CLI 无
        // session 级门，保持 None 与 UI 的休眠语义一致。
        advisor_pause: None,
        staging: None,
    }
}

/// 一道待回答的选择题交给"回答器"的入参。抽出来是为了让事件消费循环
/// 可测——终端实现读 stdin，测试注入预设答案。
struct AskRequest {
    role_id: String,
    question: String,
    multi: bool,
    options: Vec<latte_agent_core::controller::ChoiceOption>,
}

/// 回答器：拿到一道题给出答案，`None` = 无法作答（stdin EOF 等）。
type AskAnswerer = Arc<
    dyn Fn(AskRequest) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>>
        + Send
        + Sync,
>;

/// 终端回答器：渲染编号选项并读 stdin。
fn terminal_ask_answerer() -> AskAnswerer {
    Arc::new(|req: AskRequest| {
        Box::pin(async move {
            prompt_choice_on_terminal(&req.role_id, &req.question, req.multi, &req.options).await
        })
    })
}

/// 订阅 workflow 的事件通道，把阻塞式 `ask` 变成**终端里可回答**的选择器。
///
/// 修的 bug：`register_workflow_tool` 此前把 receiver 绑到 `_rx` 就不管了
/// （"REPL 无 UI 消费者，事件丢弃"）。但 workflow step 注册的 `ask` 是
/// **阻塞版**（`workflow.rs` 里的 `AskBlocking`），而 controller 侧写明
/// 「无限等待用户」——超时路径已移除。两件事凑在一起的后果：
///
/// - `ChoiceRequested` 广播进空通道，用户在终端**什么都看不到**；
/// - 工具调用挂在 oneshot 上永不返回，而唯一能 `choice::resolve` 的入口
///   是 ui-server 的 HTTP 端点，终端里根本不存在；
/// - 于是 `latte-agent chat -r manager` 一旦跑到带 `ask` 的 step
///   （`design_and_plan` 第一步的 tutor 就是），整个会话静默永久挂死，
///   只能 Ctrl-C，本轮进度全丢。
///
/// 现在：订阅同一个通道，阻塞式 ask 在终端渲染编号选项并读 stdin，
/// 答完调 `choice::resolve` 把答案交回挂起的工具调用。
///
/// 读 stdin 是安全的：REPL 主循环此刻正 `await` 在 `session.turn()` 上，
/// 只有 turn 结束后才会回去读下一行输入，不存在两处争抢 stdin。
///
/// 任务在所有 sender 被 drop（workflow 跑完）后自行退出。入口只有
/// [`cli_workflow_ctx`]——不要在别处裸建 workflow 通道。
fn spawn_cli_workflow_event_consumer_with(
    mut rx: tokio::sync::broadcast::Receiver<latte_agent_core::controller::ChatEvent>,
    answerer: AskAnswerer,
) -> tokio::task::JoinHandle<()> {
    use latte_agent_core::controller::ChatEvent;
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ChatEvent::ChoiceRequested {
                    role_id,
                    choice_id,
                    question,
                    multi,
                    wait,
                    options,
                    ..
                }) => {
                    if !wait {
                        // fire-and-forget：提问方没在等，回答会作为下一条
                        // user 消息回喂。终端只提示，不阻塞。
                        ui_emit(&format!(
                            "❓ [{role_id}] {question}（{} 个选项，非阻塞：可在下一轮直接回答）",
                            options.len()
                        ));
                        continue;
                    }
                    let req = AskRequest { role_id, question, multi, options };
                    let answer = match answerer(req).await {
                        Some(a) => a,
                        None => {
                            // stdin EOF / 读失败：不能静默——挂起的工具
                            // 会永远等下去。取消这次 ask，让工具报错退出，
                            // 至少把控制权还给用户。
                            ui_emit("[ask] stdin 不可用，取消本次提问");
                            latte_agent_core::choice::cancel(&choice_id);
                            continue;
                        }
                    };
                    if !latte_agent_core::choice::resolve(&choice_id, answer) {
                        ui_emit(&format!(
                            "[ask] 答案未送达（choice_id={choice_id} 已被取消或已回答）"
                        ));
                    }
                }
                // 进度可见性：没有这几行，用户无法区分「在等我回答」和
                // 「还在干活」——这正是原来那个静默挂死难查的一半原因。
                Ok(ChatEvent::WorkflowStep { step_id, index, total, role_id, .. }) => {
                    // step 进度同时进历史与状态行：历史留痕，状态行给
                    // "现在在哪一步"的常驻可见性（分离模式的主要收益）。
                    ui_emit(&format!("  [step {index}/{total}] {step_id} → {role_id}"));
                    ui_set_activity(Some(&format!("step {index}/{total} · {step_id} · {role_id}")));
                }
                Ok(ChatEvent::WorkflowFinished { name, status, .. }) => {
                    ui_emit(&format!("  [workflow {name}] {status}"));
                    ui_set_activity(None);
                }
                // advisor 的裁决气泡（monitor 的通道 B）。UI 里是一个
                // 🦉 卡片，chat 里进历史区——原来这条事件在 CLI 完全
                // 不可见，用户看不到 advisor 判了什么。
                Ok(ChatEvent::RoleTurn { role_id, content, .. })
                    if role_id == "advisor" =>
                {
                    ui_emit(&format!("🦉 {}", content.trim()));
                }
                Ok(ChatEvent::DelegateStarted { to_role, task, .. }) => {
                    ui_emit(&format!("  → delegate {to_role}: {}", first_line_brief(&task, 90)));
                    ui_set_activity(Some(&format!("delegate → {to_role}")));
                }
                Ok(ChatEvent::DelegateFinished { to_role, status, summary, .. }) => {
                    ui_emit(&format!(
                        "  ← delegate {to_role} {status} ({} chars)",
                        summary.chars().count()
                    ));
                    ui_set_activity(None);
                }
                // ── 以下几条原来 CLI 一条都不处理，事件广播进空气 ──
                //
                // `Paused` 最严重：模型全链哑掉（如 glm-5.3 报 400 +
                // MiniMax 流式空闲超时 + deepseek 余额不足）时后端自动
                // 暂停等人，而 chat 里用户**看不到任何提示**，只觉得
                // 卡死了。UI 侧这条会弹暂停条 + ▶ 按钮。
                Ok(ChatEvent::Paused { reason }) => {
                    ui_emit(&format!("⏸ 已自动暂停：{}", reason.trim()));
                    ui_set_activity(Some("已暂停，等待恢复"));
                }
                Ok(ChatEvent::Resumed) => {
                    ui_emit("▶ 已恢复");
                    ui_set_activity(None);
                }
                // 模型/后端错误。原来只在 trace 里能看到，正文无提示。
                Ok(ChatEvent::Error { message, .. }) => {
                    ui_emit(&format!("⚠️ {}", first_line_brief(&message, 160)));
                }
                // 工具失败：D3 这类检测器靠它，用户也该看见。
                Ok(ChatEvent::ToolError { role_id, tool_name, error, .. }) => {
                    ui_emit(&format!(
                        "  ✗ [{role_id}] {tool_name}: {}",
                        first_line_brief(&error, 140)
                    ));
                }
                // advisor 判 terminate 时中止本轮——不提示的话用户只看到
                // 一个突然结束的轮次。
                Ok(ChatEvent::AdvisorTerminated { role_id, reason, detector, .. }) => {
                    let det = detector.unwrap_or_else(|| "?".into());
                    ui_emit(&format!("🛑 advisor 中止 [{role_id} · {det}]: {}", reason.trim()));
                    ui_set_activity(None);
                }
                // 软超时：轮次还活着，但预算已超——UI 会转成"继续/取消"提示。
                Ok(ChatEvent::TimeoutWarning {
                    role_id, elapsed_secs, soft_timeout_secs, hard_timeout_secs, ..
                }) => {
                    ui_emit(&format!(
                        "⏳ [{role_id}] 已跑 {elapsed_secs}s（软超时 {soft_timeout_secs}s，硬中止 {hard_timeout_secs}s）"
                    ));
                }
                // `plan` 工具提交的任务候选。CLI 没有勾选弹窗，但至少要
                // 让用户知道产出了什么、去哪看——否则 manager 调了 plan
                // 而用户毫无感知（我刚把这个工具接进 CLI）。
                Ok(ChatEvent::PlanProposed { role_id, plan_id, tasks }) => {
                    ui_emit(&format!(
                        "📋 [{role_id}] 提交 {} 个任务候选（{plan_id}）；在 UI 弹窗勾选导入，或看 .latte/tasks/",
                        tasks.len()
                    ));
                }
                // `task_report` 工具的回报（同上，CLI 侧新接的工具）。
                Ok(ChatEvent::TaskReport { task_id, result, summary, .. }) => {
                    ui_emit(&format!(
                        "📊 task_report {task_id} {result}: {}",
                        first_line_brief(&summary, 120)
                    ));
                }
                Ok(_) => {}
                // Lagged：事件产出快于消费，丢了几条无所谓——ask 是
                // 阻塞的，不会因为丢事件而漏掉（丢了也还挂着）。但要
                // 提示，否则用户不知道自己漏看了进度。
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    ui_emit(&format!("  [事件流跳过 {n} 条]"));
                }
                // Closed：所有 sender 都 drop 了 = workflow 结束。
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// 在终端渲染一道选择题并读取回答。返回 `None` 表示 stdin 不可用。
///
/// 输入约定（对齐 oh-my-pi `ask` 的可用性，不追求它的富渲染）：
/// - 输入序号选择；`multi` 时可用逗号/空格分隔多个序号；
/// - 输入任意非序号文本 = 自由作答（等价于 oh-my-pi 的
///   `Other (type your own)`）；
/// - 空行重问——不做「超时自动选推荐项」，因为 latte 的 `ChoiceOption`
///   没有 `recommended` 字段，无从判断默认项。
async fn prompt_choice_on_terminal(
    role_id: &str,
    question: &str,
    multi: bool,
    options: &[latte_agent_core::controller::ChoiceOption],
) -> Option<String> {
    let mut rendered = String::new();
    rendered.push_str(&format!("\n❓ [{role_id}] {question}\n"));
    for (i, opt) in options.iter().enumerate() {
        rendered.push_str(&format!("  {}) {}\n", i + 1, opt.label));
        if !opt.description.is_empty() {
            rendered.push_str(&format!("     {}\n", opt.description));
        }
    }
    let hint = if multi {
        "多选，逗号或空格分隔序号"
    } else {
        "输入序号"
    };
    rendered.push_str(&format!("  （{hint}；也可直接输入自己的答案）\n"));

    let labels: Vec<String> = options.iter().map(|o| o.label.clone()).collect();
    loop {
        // 题面进历史区（scrollback），回答走底部输入行——分离模式下
        // 二者不会互相覆盖。非 TTY 时 ui_emit / read_user_line 各自
        // 退回 stderr + read_line，行为与改造前一致。
        ui_emit(rendered.trim_end_matches('\n'));
        let line = read_user_line("answer › ").await.ok().flatten()?;

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // 先试解析成序号；解析不出来就当自由作答原样回传。
        let picked = parse_choice_indices(trimmed, labels.len(), multi);
        return Some(match picked {
            Some(idx) => idx
                .into_iter()
                .map(|i| labels[i].clone())
                .collect::<Vec<_>>()
                .join(", "),
            None => trimmed.to_string(),
        });
    }
}

/// 把 `"2"` / `"1,3"` / `"1 3"` 解析成 0-based 下标。
///
/// 返回 `None` = 不是合法的序号表达式（调用方按自由作答处理）。单选时
/// 给多个序号也返回 `None`——那是用户意图不明，按自由文本原样交给模型
/// 比悄悄取第一个更诚实。
fn parse_choice_indices(input: &str, n: usize, multi: bool) -> Option<Vec<usize>> {
    if n == 0 {
        return None;
    }
    let parts: Vec<&str> = input
        .split([',', '，', ' ', '\t'])
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() || (!multi && parts.len() > 1) {
        return None;
    }
    let mut out = Vec::with_capacity(parts.len());
    for p in parts {
        let k: usize = p.parse().ok()?;
        if k == 0 || k > n {
            return None;
        }
        let idx = k - 1;
        if !out.contains(&idx) {
            out.push(idx);
        }
    }
    Some(out)
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
                // ctx 挂在会话级事件通道上：阻塞式 `ask` 在终端可回答，
                // 否则 workflow 跑到带 ask 的 step 就静默永久挂死
                // （见 `cli_session_event_tx`）。通道活到进程结束，
                // 这里不需要任何收尾。
                let ctx = cli_workflow_ctx(merged, resolver, default_params, cwd);
                latte_agent_core::workflow::run_workflow(&wf, &topic, &ctx)
                    .await
                    .map(serde_json::Value::String)
                    .map_err(|e| {
                        // 与 controller 的 workflow 工具一致：失败附善后
                        // 指令，防止 agent 拿到裸错误后静默结束。
                        tool_err(format!(
                            "{e}\n\n请善后：能修复的修复后用上面的 wf_id 以 resume 参数续跑，\
                             或换路径重做；处理完向用户汇报结果，不要静默结束。"
                        ))
                    })
            })
        });

    let tool = Tool::builder(
        "workflow".to_string(),
        format!("Run a named multi-role workflow. Available workflows: {available_text}"),
        input_schema,
        handler,
    )
    // 见 ORCHESTRATION_TOOL_TIMEOUT_SECS：这里此前漏配 `.timeout()`，
    // 于是吃工具管理器 1500s(25min) 默认熔断 —— 比子流水线真实时长紧
    // 得多，正是「超时倒挂」：引擎还在跑，工具层已经熔断，step 变孤儿
    // 任务。防挂死改由单次模型调用的 idle/TTFB 流式超时按「活性」兜底
    // （见 agent.rs），不再靠工具层这道墙钟。
    .timeout(std::time::Duration::from_secs(
        latte_agent_core::controller::ORCHESTRATION_TOOL_TIMEOUT_SECS,
    ))
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
                // 子会话 + 事件对齐 UI（controller.rs 的 delegate 路径）：
                // 此前 CLI 的 delegate 是个纯黑盒——不发任何 ChatEvent、
                // 不建 subsession，于是 `latte-agent debug session|trace`
                // 读不到专家的往返，终端上也看不到"派了谁、派了什么"。
                // workflow 路径已经有这两样（子会话日志会落盘），
                // delegate 路径是最后一处遗漏。
                let (sub_id, sub_sink) =
                    cli_subsession_store().create(&cli_session_id(), &role_id);
                let _ = cli_session_event_tx().send(
                    latte_agent_core::controller::ChatEvent::DelegateStarted {
                        from_role: "manager".into(),
                        to_role: role_id.clone(),
                        task: task.clone(),
                        sub_id: sub_id.clone(),
                        wf_id: None,
                    },
                );
                // sub_sink 内部已 fanout 到 MemorySink + DiskSink，
                // 不要再包一层 FanoutSink（会把事件写两份进内存）。
                // 这里与既有的 scoped_sink 并列：前者给子会话日志，
                // 后者给 REPL 的 stdout/jsonl 追踪。
                let scoped_sink = Arc::new(latte_agent_core::trace::ScopedSink::new(
                    Arc::clone(&sink),
                    role_id.clone(),
                ));
                let specialist_sink: Arc<dyn latte_agent_core::trace::TraceSink> =
                    Arc::new(latte_agent_core::trace::FanOutSink::new(vec![
                        scoped_sink.clone(),
                        sub_sink,
                    ]));
                let mut runner = match specialist_tm {
                    // unlimited tool rounds — model decides when it's done.
                    // LoopDetector in agent.rs trips on actual stuck patterns.
                    Some(tm) => AgentRunner::new_with_tools(agent, tm)
                        .with_sink(specialist_sink.clone())
                        .with_role(role_id.clone()),
                    None => AgentRunner::new(agent)
                        .with_sink(specialist_sink.clone())
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
                        // 失败路径也要补 DelegateFinished——否则消费端的
                        // 分派记录永远停在「执行中」（UI 侧同样约定）。
                        let _ = cli_session_event_tx().send(
                            latte_agent_core::controller::ChatEvent::DelegateFinished {
                                from_role: "manager".into(),
                                to_role: role_id.clone(),
                                status: "failed".into(),
                                summary: e.to_string(),
                                sub_id: sub_id.clone(),
                                wf_id: None,
                            },
                        );
                        return Err(tool_err(format!(
                            "delegate to '{}' failed: {}", role_id, e
                        )));
                    }
                    Err(_) => {
                        eprintln!("  ← {} timed out after {}s", role_id, timeout_s);
                        let _ = cli_session_event_tx().send(
                            latte_agent_core::controller::ChatEvent::DelegateFinished {
                                from_role: "manager".into(),
                                to_role: role_id.clone(),
                                status: "timeout".into(),
                                summary: format!("{timeout_s}s wall-clock timeout"),
                                sub_id: sub_id.clone(),
                                wf_id: None,
                            },
                        );
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
                let _ = cli_session_event_tx().send(
                    latte_agent_core::controller::ChatEvent::DelegateFinished {
                        from_role: "manager".into(),
                        to_role: role_id.clone(),
                        status: "ok".into(),
                        summary: response.clone(),
                        sub_id: sub_id.clone(),
                        wf_id: None,
                    },
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
pub(crate) fn tool_usage_prompt(_allowed: &[String]) -> String {
    // 不在 prompt 里枚举工具名，也不再教 `<tool_call>` 文本协议：
    // 可用工具完全由请求 `tools` 字段的 schema 决定，文本协议已被
    // 原生 function-calling 取代（见 latte-agent-core agent.rs）。
    r#"

## Tools

Call tools via the native function-calling interface (the request `tools`
field carries each tool's name, description, and JSON schema). Do NOT emit
`<tool_call>` text blocks -- they are no longer parsed. Inspect each tool
result and continue until the task is done.
"#
    .to_string()
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
    for (lineno, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let m: Message = match serde_json::from_str(line) {
            Ok(m) => m,
            Err(e) => {
                // 旧格式存档：content 是裸 string（latte-ai 把 Message.content
                // 改成 Vec<ContentPart> 之前 save_session 写出的文件）。
                // 包一层 Text part 兼容载入，否则老存档永远 resume 不了。
                #[derive(serde::Deserialize)]
                struct LegacyMessage {
                    role: MsgRole,
                    content: String,
                }
                let legacy: LegacyMessage = serde_json::from_str(line)
                    .map_err(|_| format!("{} line {}: {e}", path, lineno + 1))?;
                Message {
                    role: legacy.role,
                    content: vec![latte_ai::models::ContentPart::text(&legacy.content)],
                    tool_call_id: None,
                    tool_calls: None,
                }
            }
        };
        msgs.push(m);
    }
    Ok(msgs)
}

/// 常驻输入任务提交上来的一条输入。
enum InputEvent {
    Line(String),
    /// Ctrl-D（空行）或终端关闭。
    Eof,
}

/// 输入队列：常驻按键线程 → 消费方（REPL 主循环 / ask 选择器）。
///
/// 为什么要队列：`read_user_line` 原来只在两个 turn **之间**被调用，
/// turn 期间没有任何东西在读 stdin——用户在等待的几十分钟里看不到自己
/// 打的字，也没法预先排下一条消息。实测实锤：turn 中打的字在日志里的
/// 位置晚于 `TurnEnd`，说明回显发生在 turn 结束之后。
///
/// 改成常驻读取后：输入行始终活着，turn 期间提交的行排队、turn 结束后
/// 依次消费——与 UI 的「发送框始终可用」语义一致。
///
/// receiver 用 `tokio::sync::Mutex` 包住：同一时刻只允许一个消费方等行。
/// REPL 只在 turn 之间等行，而 `ask` 只在 turn 之内出现，两者不重叠；
/// 加锁是把这个前提显式化，避免将来有人在 idle 期发 ask 时静默抢走
/// 用户本要发给 REPL 的那一行。
#[allow(clippy::type_complexity)]
fn input_queue() -> &'static (
    tokio::sync::mpsc::UnboundedSender<InputEvent>,
    tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<InputEvent>>,
) {
    static Q: std::sync::OnceLock<(
        tokio::sync::mpsc::UnboundedSender<InputEvent>,
        tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<InputEvent>>,
    )> = std::sync::OnceLock::new();
    Q.get_or_init(|| {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (tx, tokio::sync::Mutex::new(rx))
    })
}

/// 起常驻按键线程（幂等）。只在分离模式下有意义。
///
/// 用**专用 OS 线程**而不是 tokio 任务：终端读取是阻塞 IO，放在
/// 线程里可以直接同步持锁（`parking_lot`），不必在 await 上跨锁；
/// 也不会占用执行器的 worker。
fn spawn_input_thread() {
    use super::split_screen::{apply_key, KeyOutcome};
    static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    let tx = input_queue().0.clone();
    std::thread::Builder::new()
        .name("latte-input".into())
        .spawn(move || {
            loop {
                // 读事件（阻塞）。终端关闭时 read 报错 → 报 EOF 收尾。
                let ev = match crossterm::event::read() {
                    Ok(ev) => ev,
                    Err(_) => {
                        let _ = tx.send(InputEvent::Eof);
                        return;
                    }
                };
                let key = match ev {
                    crossterm::event::Event::Key(k)
                        if k.kind == crossterm::event::KeyEventKind::Press =>
                    {
                        k
                    }
                    // 粘贴：整段一次插入，换行折成空格。
                    crossterm::event::Event::Paste(text) => {
                        let cleaned = super::split_screen::normalize_pasted(&text);
                        if !cleaned.is_empty() {
                            if let Some(ui) = split_ui().lock().as_mut() {
                                ui.buf.insert_str(&cleaned);
                                ui.redraw();
                            }
                        }
                        continue;
                    }
                    // 尺寸变化：重画 viewport（历史已在 scrollback，由终端重排）。
                    crossterm::event::Event::Resize(_, _) => {
                        if let Some(ui) = split_ui().lock().as_mut() {
                            ui.redraw();
                        }
                        continue;
                    }
                    _ => continue,
                };
                // 持锁只在这一小段内，且不跨越任何 await。
                let mut g = split_ui().lock();
                let Some(ui) = g.as_mut() else {
                    // 分离模式已退出（/quit 后）：线程收尾。
                    return;
                };
                match apply_key(&mut ui.buf, key) {
                    KeyOutcome::Submit(line) => {
                        ui.history.push(&line);
                        let prompt = ui.prompt_text().to_string();
                        ui.redraw();
                        drop(g);
                        // 提交的行也进历史区，否则回车后它就消失了。
                        ui_emit(&format!("{prompt}{line}"));
                        if tx.send(InputEvent::Line(line)).is_err() {
                            return;
                        }
                    }
                    KeyOutcome::Eof => {
                        drop(g);
                        let _ = tx.send(InputEvent::Eof);
                        return;
                    }
                    // Ctrl-C 分两级，对齐终端习惯（curl / 多数 REPL）：
                    // turn 进行中 → 中止本轮（相当于 UI 的
                    // `/chat/cancel-turn`）；空闲 → 只清输入行。
                    //
                    // 只清行不中止是反直觉的：用户在一个跑了十几分钟的
                    // turn 里按 Ctrl-C，期望的是"停下来"。
                    KeyOutcome::Interrupt => {
                        ui.redraw();
                        drop(g);
                        if turn_in_flight().load(std::sync::atomic::Ordering::SeqCst) {
                            turn_cancel_flag().store(true, std::sync::atomic::Ordering::SeqCst);
                            ui_emit("^C 正在中止本轮…");
                        } else {
                            ui_emit("^C");
                        }
                    }
                    KeyOutcome::Redraw => ui.redraw(),
                    KeyOutcome::HistoryPrev => {
                        let cur = ui.buf.text();
                        if let Some(prev) = ui.history.prev(&cur) {
                            ui.buf.set(&prev);
                            ui.redraw();
                        }
                    }
                    KeyOutcome::HistoryNext => {
                        if let Some(next) = ui.history.next() {
                            ui.buf.set(&next);
                            ui.redraw();
                        }
                    }
                    KeyOutcome::Ignored => {}
                }
            }
        })
        .ok();
}

/// 读一行用户输入。
///
/// 分离模式（TTY）走 `SplitScreen` 的按键循环；非 TTY 走原来的
/// `print! + read_line`，**逐字节不变**——CLI 侧 e2e 全都 grep stdout。
///
/// 返回 `None` = EOF（Ctrl-D 或管道读完），调用方退出 REPL。
async fn read_user_line(prompt: &str) -> io::Result<Option<String>> {
    // 非分离模式：原路径，逐字节不变（CLI 侧 e2e 全都 grep stdout）。
    if split_ui().lock().is_none() {
        let mut out = io::stdout();
        print!("{prompt}");
        out.flush()?;
        let mut line = String::new();
        let n = io::stdin().lock().read_line(&mut line)?;
        if n == 0 {
            println!();
            return Ok(None);
        }
        return Ok(Some(line));
    }
    // 分离模式：提示符交给常驻按键线程渲染，这里只等队列里的行。
    // 输入行因此在 turn 期间也是活的——用户能看到自己打的字、能预先
    // 排下一条消息（改造前这段时间输入行是死的）。
    if let Some(ui) = split_ui().lock().as_mut() {
        ui.set_prompt(prompt.to_string());
    }
    spawn_input_thread();
    let mut rx = input_queue().1.lock().await;
    match rx.recv().await {
        Some(InputEvent::Line(line)) => Ok(Some(line)),
        Some(InputEvent::Eof) | None => Ok(None),
    }
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

    // ─── CLI 阻塞式 ask（修复静默挂死）────────────────────────────

    fn opt(label: &str, desc: &str) -> latte_agent_core::controller::ChoiceOption {
        let mut o = latte_agent_core::controller::ChoiceOption::default();
        o.label = label.into();
        o.description = desc.into();
        o
    }

    fn choice_event(
        choice_id: &str,
        wait: bool,
        multi: bool,
    ) -> latte_agent_core::controller::ChatEvent {
        latte_agent_core::controller::ChatEvent::ChoiceRequested {
            role_id: "tutor".into(),
            choice_id: choice_id.into(),
            question: "你的目标是什么？".into(),
            multi,
            layout: String::new(),
            allow_upload: false,
            wait,
            options: vec![opt("搞懂代码机制", "读源码"), opt("做调优", "调参数")],
        }
    }

    /// 预设答案的回答器（替代读 stdin）。
    fn canned_answerer(answer: Option<&'static str>) -> AskAnswerer {
        Arc::new(move |_req: AskRequest| {
            Box::pin(async move { answer.map(|s| s.to_string()) })
        })
    }

    /// 核心回归：阻塞式 ask 必须被终端消费者回答，工具调用得以继续。
    ///
    /// 修复前这里会永久挂起——事件广播进无人订阅的通道，而唯一能
    /// `choice::resolve` 的入口在 ui-server 的 HTTP 端点里。
    #[tokio::test]
    async fn blocking_ask_gets_answered_from_cli_consumer() {
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        let task = spawn_cli_workflow_event_consumer_with(rx, canned_answerer(Some("做调优")));

        let id = "choice-cli-test-1";
        let ev = choice_event(id, true, false);
        // 顺序同真实 ask 工具：先 register 再广播。
        let waiter = latte_agent_core::choice::register(id, ev.clone(), tx.clone());
        tx.send(ev).unwrap();

        // 修复前这个 await 永不返回，靠 timeout 把「挂死」变成断言失败。
        let answer = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("阻塞 ask 必须在 5s 内拿到答案（挂死回归）")
            .expect("通道不该被关闭");
        assert_eq!(answer, "做调优");

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    /// stdin 不可用（EOF）→ 必须 `cancel` 让等待方立刻拿到 RecvError
    /// 退出，而不是继续挂着。挂死的另一半：修复得别引入新的挂死。
    #[tokio::test]
    async fn unanswerable_ask_is_cancelled_not_hung() {
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        let task = spawn_cli_workflow_event_consumer_with(rx, canned_answerer(None));

        let id = "choice-cli-test-2";
        let ev = choice_event(id, true, false);
        let waiter = latte_agent_core::choice::register(id, ev.clone(), tx.clone());
        tx.send(ev).unwrap();

        let outcome = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("cancel 后等待方必须立刻返回，不能挂着");
        assert!(outcome.is_err(), "cancel 应让 oneshot 关闭而非投递答案");

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    /// fire-and-forget（`wait=false`）不该被消费者回答——它的答案走
    /// 「下一条 user 消息」那条路，抢答会让挂起表状态错乱。
    #[tokio::test]
    async fn non_blocking_ask_is_not_resolved_by_consumer() {
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        let task = spawn_cli_workflow_event_consumer_with(rx, canned_answerer(Some("不该用到")));

        let id = "choice-cli-test-3";
        let ev = choice_event(id, false, false);
        let waiter = latte_agent_core::choice::register(id, ev.clone(), tx.clone());
        tx.send(ev).unwrap();

        // 给消费者足够时间处理完事件，再确认没人动过这个挂起项。
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !waiter.is_terminated(),
            "wait=false 的 ask 不该被终端消费者回答"
        );
        latte_agent_core::choice::cancel(id);

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    /// 通道关闭（workflow 跑完，sender 全 drop）→ 消费任务必须退出，
    /// 否则每跑一次 workflow 泄漏一个常驻 task。
    #[tokio::test]
    async fn consumer_exits_when_channel_closes() {
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        let task = spawn_cli_workflow_event_consumer_with(rx, canned_answerer(Some("x")));
        drop(tx);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("sender drop 后消费任务必须退出")
            .expect("任务不该 panic");
    }

    /// 覆盖**接线本身**：`cli_workflow_ctx_on` 造出来的 ctx，它的
    /// `event_tx` 必须真的有消费者在听。上面几条测试直接调
    /// `spawn_..._with`，所以即使构造入口把 receiver 丢了也照样绿——
    /// 这条堵的就是那个缺口（原 bug 恰恰只错在构造入口）。
    #[tokio::test]
    async fn cli_workflow_ctx_wires_the_ask_consumer() {
        let cfg = Arc::new(AgentConfig {
            advisor: Default::default(),
            models: latte_agent_core::config::ModelCatalog {
                models: vec![latte_agent_core::config::ModelDef {
                    name: "Test".into(),
                    api: "openai".into(),
                    provider: "test".into(),
                    base_url: "http://127.0.0.1:1".into(),
                    api_key: "k".into(),
                    context_window: 8192,
                    max_tokens: 1024,
                    supports_thinking: false,
                    supports_vision: false,
                    supports_image_generation: false,
                    cost_per_million_input: None,
                    cost_per_million_output: None,
                    tier: Some("premium".into()),
                    timeout_secs: None,
                }],
                tiers: None,
                role_tiers: None,
            },
            roles: Default::default(),
        });
        let resolver = Arc::new(ModelResolver::from_config(&cfg).unwrap());
        // 用测试自建的通道 + 预设答案回答器，避免碰进程级单例
        // （单例的消费者会去读真实 stdin）。
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        let consumer =
            spawn_cli_workflow_event_consumer_with(rx, canned_answerer(Some("搞懂代码机制")));
        let ctx = cli_workflow_ctx_on(
            cfg,
            resolver,
            GenerateParams::default(),
            std::env::temp_dir(),
            tx,
        );
        // session_id 必须带上：`ask` 的落盘补发按 (cwd, session_id) 归属，
        // UI 侧一直有，CLI 侧此前是 None。
        assert!(ctx.session_id.is_some(), "CLI workflow ctx 必须带 session_id");

        let id = "choice-cli-ctx-1";
        let ev = choice_event(id, true, false);
        let waiter = latte_agent_core::choice::register(id, ev.clone(), ctx.event_tx.clone());
        ctx.event_tx.send(ev).unwrap();

        let answer = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("ctx 的事件通道必须有消费者在听（接线回归）")
            .expect("通道不该被关闭");
        assert_eq!(answer, "搞懂代码机制");

        drop(ctx);
        consumer.abort();
    }

    /// 防回归：CLI 必须为 UI(controller) 支持的**每一个**编排工具都
    /// 提供注册路径。此前 CLI 只接了 `workflow`/`delegate`，`ask`/`plan`/
    /// `task_report`/`generate_image` 全缺——manager 声明了前三个，在 UI
    /// 里能用、在 chat 里调用直接 ToolNotFound。
    ///
    /// 这条测试用源码扫描而非运行时探测：注册发生在 `build_runner` 深处，
    /// 需要真实模型配置才能构造，而"少接一个工具"是纯静态的漏接。
    #[test]
    fn cli_registers_every_orchestration_tool_the_ui_does() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let controller = std::fs::read_to_string(
            root.join("latte-agent-core/src/controller.rs"),
        )
        .expect("controller.rs");
        let chat = std::fs::read_to_string(
            root.join("latte-agent-cli/src/commands/chat.rs"),
        )
        .expect("chat.rs");

        // UI 侧的真源：controller 里所有 `allowed_tools ... t == "X"` 分支。
        let mut ui_tools: Vec<String> = Vec::new();
        for line in controller.lines() {
            if !line.contains("allowed_tools") || !line.contains("t == \"") {
                continue;
            }
            if let Some(rest) = line.split("t == \"").nth(1) {
                if let Some(name) = rest.split('"').next() {
                    if !ui_tools.iter().any(|t| t == name) {
                        ui_tools.push(name.to_string());
                    }
                }
            }
        }
        assert!(
            ui_tools.len() >= 6,
            "应扫到 UI 的全部编排工具，实际: {ui_tools:?}"
        );

        for tool in &ui_tools {
            let guard = format!("t == \"{tool}\"");
            assert!(
                chat.contains(&guard),
                "CLI chat 缺少 `{tool}` 的注册分支（UI 有）。\
                 UI 支持的编排工具: {ui_tools:?}"
            );
        }
    }

    /// 防回归：CLI 的 workflow ctx 必须带上 subsession_store 与
    /// advisor_gate。原状两个都是 None——同一个 workflow 在 chat 里跑
    /// 没有子会话日志、也不过 advisor 的返回审查（UI 里会被 intervene
    /// 打回重做的产出，chat 里直接放行）。
    #[tokio::test]
    async fn cli_workflow_ctx_matches_ui_observability_wiring() {
        let cfg = Arc::new(AgentConfig {
            advisor: Default::default(),
            models: latte_agent_core::config::ModelCatalog {
                models: vec![latte_agent_core::config::ModelDef {
                    name: "Test".into(),
                    api: "openai".into(),
                    provider: "test".into(),
                    base_url: "http://127.0.0.1:1".into(),
                    api_key: "k".into(),
                    context_window: 8192,
                    max_tokens: 1024,
                    supports_thinking: false,
                    supports_vision: false,
                    supports_image_generation: false,
                    cost_per_million_input: None,
                    cost_per_million_output: None,
                    tier: Some("premium".into()),
                    timeout_secs: None,
                }],
                tiers: None,
                role_tiers: None,
            },
            roles: Default::default(),
        });
        let advisor_on = cfg.advisor.enabled();
        let resolver = Arc::new(ModelResolver::from_config(&cfg).unwrap());
        let (tx, _rx) = tokio::sync::broadcast::channel(4);
        let ctx = cli_workflow_ctx_on(
            cfg,
            resolver,
            GenerateParams::default(),
            std::env::temp_dir(),
            tx,
        );
        assert!(
            ctx.subsession_store.is_some(),
            "CLI workflow ctx 必须带 subsession_store（对齐 UI 的 debug 可观测）"
        );
        assert!(ctx.session_id.is_some(), "必须带 session_id（ask 落盘归属）");
        // advisor_gate 跟随配置开关，与 UI 的 `advisor.enabled()` 同源。
        assert_eq!(
            ctx.advisor_gate.is_some(),
            advisor_on,
            "advisor_gate 必须与配置里的 advisor.enabled() 一致"
        );
    }

    /// 防回归：CLI 的 delegate 必须建 subsession + 发
    /// DelegateStarted/Finished。原状是纯黑盒（0 处 ChatEvent、
    /// 0 处 subsession），`latte-agent debug session|trace` 读不到
    /// 专家的往返，终端上也看不到派了谁——而 UI 侧两样都有。
    ///
    /// 同样用源码扫描：注册在闭包深处，运行时探测要真实模型。
    #[test]
    fn cli_delegate_is_observable_like_the_ui() {
        let chat = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src/commands/chat.rs"),
        )
        .expect("chat.rs");
        let body = chat
            .split("async fn register_delegate_tool")
            .nth(1)
            .expect("register_delegate_tool 应存在");
        // 边界收到本函数结尾的 `tm.register(...)` 为止——否则会把测试
        // 自身的源码也算进匹配（实测：DelegateFinished 数出 4 而非 3）。
        let body = body
            .split("tm.register(tool, Some(\"manager\"))")
            .next()
            .expect("delegate 函数应以 tm.register 收尾");

        assert!(
            body.contains("cli_subsession_store()"),
            "delegate 必须给 specialist 建 subsession（对齐 UI）"
        );
        assert!(
            body.contains("ChatEvent::DelegateStarted"),
            "delegate 必须发 DelegateStarted"
        );
        // 三条出口（ok / failed / timeout）都要销账，否则消费端的分派
        // 记录永远停在「执行中」。
        assert_eq!(
            body.matches("ChatEvent::DelegateFinished").count(),
            3,
            "ok / failed / timeout 三条出口都必须发 DelegateFinished"
        );
        for status in ["\"ok\"", "\"failed\"", "\"timeout\""] {
            assert!(
                body.contains(status),
                "DelegateFinished 缺少 status={status} 的出口"
            );
        }
    }

    #[test]
    fn brief_truncation_is_char_safe_for_cjk() {
        // 按字节切会把多字节字符切一半 panic——任务描述基本都是中文。
        let cjk = "为当前仓库编排从浅到深的学习路径与任务清单";
        let out = first_line_brief(cjk, 5);
        assert_eq!(out, "为当前仓库…");
        // 多行只取首行。
        assert_eq!(first_line_brief("第一行\n第二行", 90), "第一行");
        // 不足上限时原样返回、不加省略号。
        assert_eq!(first_line_brief("短", 90), "短");
        // 空输入不 panic。
        assert_eq!(first_line_brief("", 10), "");
        // 边界：恰好等于上限不截断。
        assert_eq!(first_line_brief("一二三", 3), "一二三");
    }

    /// 防回归：CLI 的 advisor 宿主必须真的能驱动 monitor。
    ///
    /// 这条不是源码扫描——它真跑 `spawn_on_host`，喂一条 ToolError 事件
    /// （D3 检测器的输入），断言 monitor 把纠正提示注入了宿主的 hint
    /// 队列。原状是 monitor 只由 `ChatController::spawn` 拉起，chat 的
    /// 顶层 turn 一条检测器都不跑。
    #[tokio::test]
    async fn cli_advisor_host_drives_the_monitor() {
        use latte_agent_core::advisor_monitor::AdvisorHost;
        let host = cli_advisor_host();
        host.hints.lock().clear();
        // trait 方法必须接到会话级通道上——接错了 monitor 收不到事件。
        let mut rx = AdvisorHost::subscribe(&*host);
        AdvisorHost::event_sender(&*host)
            .send(latte_agent_core::controller::ChatEvent::Status {
                message: "probe".into(),
            })
            .expect("宿主的 event_sender 必须与 subscribe 同一个通道");
        let got = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("应收到自己发的事件")
            .expect("通道不该关闭");
        assert!(
            matches!(
                got,
                latte_agent_core::controller::ChatEvent::Status { .. }
            ),
            "subscribe/event_sender 必须成对指向同一通道"
        );

        // hint 注入 → REPL 会在下一轮取走。
        AdvisorHost::advisor_hint(&*host, "换个路径");
        assert_eq!(host.hints.lock().as_slice(), ["换个路径"]);

        // last_user_input 是审查 prompt 的「主诉求」基准。
        *host.last_input.lock() = "原始诉求".into();
        assert_eq!(AdvisorHost::last_user_input(&*host), "原始诉求");

        // terminate 裁决置取消标志（CLI 无 controller 的 input 通道要唤醒）。
        host.cancel
            .store(false, std::sync::atomic::Ordering::SeqCst);
        AdvisorHost::request_cancel_turn(&*host).await;
        assert!(
            host.cancel.load(std::sync::atomic::Ordering::SeqCst),
            "request_cancel_turn 必须置位取消标志"
        );
        host.hints.lock().clear();
    }

    /// 防回归：顶层 runner 的 sink 必须并上 `ChatEventTraceSink`，
    /// 否则 monitor 收不到顶层 turn 的任何事件（检测器全瞎），
    /// 而 REPL 看起来一切正常——是最难发现的那类漏接。
    #[test]
    fn top_level_sink_feeds_the_event_channel() {
        let full = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/commands/chat.rs"),
        )
        .expect("chat.rs");
        // 先切掉 test 模块再扫：否则本测试自己的字面量会被算进匹配
        // （任何搜索串都出现在这段源码里，实测恒定多数出 1）。
        let chat = full
            .split("#[cfg(test)]")
            .next()
            .expect("非测试部分");
        let body = chat
            .split("fn cli_top_level_sink(")
            .nth(1)
            .expect("cli_top_level_sink 应存在");
        let body = body.split("\n}").next().unwrap_or(body);
        assert!(
            body.contains("ChatEventTraceSink"),
            "顶层 sink 必须并上 ChatEventTraceSink（advisor monitor 的事件源）"
        );
        // 两个分支都必须用它，不能只改一个。
        assert_eq!(
            // 匹配含 `.with_sink(` 的完整调用：只数真实接线，
            // 不会把本测试源码里的裸名算进去（实测数出 3 而非 2）。
            chat.matches(".with_sink(cli_top_level_sink(&sink))").count(),
            2,
            "带工具/无工具两个 runner 分支都要用 cli_top_level_sink"
        );
    }

    /// 防回归：`ensure_cli_advisor_monitor` 必须真的拉起 monitor。
    ///
    /// 判据用 subsession 计数：函数内部会给 advisor 分配一个子会话
    /// （每次 review 的模型往返落 `advisor-<micros>.jsonl`），拉起了就
    /// 一定 +1。不用"检查日志文件是否存在"——`create()` 在没有事件写入
    /// 时不落文件，实机验证时我就是被这点误导过一次。
    #[tokio::test]
    async fn ensure_cli_advisor_monitor_actually_spawns() {
        let cfg = AgentConfig {
            advisor: Default::default(),
            models: latte_agent_core::config::ModelCatalog {
                models: vec![latte_agent_core::config::ModelDef {
                    name: "Test".into(),
                    api: "openai".into(),
                    provider: "test".into(),
                    base_url: "http://127.0.0.1:1".into(),
                    api_key: "k".into(),
                    context_window: 8192,
                    max_tokens: 1024,
                    supports_thinking: false,
                    supports_vision: false,
                    supports_image_generation: false,
                    cost_per_million_input: None,
                    cost_per_million_output: None,
                    tier: Some("premium".into()),
                    timeout_secs: None,
                }],
                tiers: None,
                role_tiers: None,
            },
            roles: Default::default(),
        };
        // 默认配置下 advisor 是开的——这本身就是要锁住的前提：
        // 关掉它等于 chat 又回到"没有监察"。
        assert!(
            cfg.advisor.enabled(),
            "advisor 默认应开启（enabled() 缺省 true）"
        );
        let resolver = ModelResolver::from_config(&cfg).unwrap();
        let before = cli_subsession_store().len();
        ensure_cli_advisor_monitor(
            &cfg,
            &resolver,
            GenerateParams::default(),
            &std::env::temp_dir(),
            "manager",
        );
        assert_eq!(
            cli_subsession_store().len(),
            before + 1,
            "monitor 应已拉起并为 advisor 分配子会话"
        );
        // 幂等：进程内只跑一份，重复调用不再新增。
        ensure_cli_advisor_monitor(
            &cfg,
            &resolver,
            GenerateParams::default(),
            &std::env::temp_dir(),
            "manager",
        );
        assert_eq!(
            cli_subsession_store().len(),
            before + 1,
            "重复调用必须幂等（否则每次切角色都多拉一个 monitor）"
        );
    }

    /// 防回归：REPL 必须用**局部**守卫恢复终端，不能依赖
    /// `Drop for SplitScreen`。
    ///
    /// `SplitScreen` 存在 `static` 里，而 Rust 的 static 永不 drop——
    /// 少了这个守卫，`latte-agent chat` 退出后用户终端留在 raw mode
    /// （无回显，只能盲敲 `reset`）。实机在伪终端里抓到过：`?2004h`
    /// 下发了、`?2004l` 从来没有。
    #[test]
    fn repl_installs_a_local_guard_to_restore_the_terminal() {
        let full = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/commands/chat.rs"),
        )
        .expect("chat.rs");
        // 扫描前切掉 test 模块，否则本测试的字面量会被算进匹配。
        let src = full.split("#[cfg(test)]").next().expect("非测试部分");
        assert!(
            src.contains("let _ui_guard = UiGuard;"),
            "REPL 必须安装 UiGuard；static 里的 SplitScreen 不会被 drop"
        );
        // 守卫本身要真的做恢复动作。
        let guard_impl = src
            .split("impl Drop for UiGuard")
            .nth(1)
            .expect("UiGuard 应实现 Drop");
        let guard_impl = guard_impl.split("\n}").next().unwrap_or(guard_impl);
        assert!(
            guard_impl.contains("ui_leave()"),
            "UiGuard::drop 必须调 ui_leave() 恢复终端"
        );
    }

    /// 防回归：对用户**可见性关键**的事件，CLI 消费者必须都有分支。
    ///
    /// 原状是 CLI 只处理 6 个事件，其余广播进空气。最严重的是 `Paused`：
    /// 模型全链哑掉（glm-5.3 报 400 + MiniMax 流式空闲超时 + deepseek
    /// 余额不足）时后端自动暂停等人，而 chat 里用户看不到任何提示，
    /// 只觉得卡死了——UI 侧这条会弹暂停条 + ▶ 按钮。
    ///
    /// 这里不要求"全部 39 个变体都处理"（RoundStarted 之类是多角色模式
    /// 专用，CLI 单角色路径用不到），只钉住这份清单。
    #[test]
    fn cli_consumer_handles_user_visible_events() {
        let full = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/commands/chat.rs"),
        )
        .expect("chat.rs");
        // 扫描前切掉 test 模块，否则本测试的字面量会被算进匹配。
        let src = full.split("#[cfg(test)]").next().expect("非测试部分");
        let body = src
            .split("fn spawn_cli_workflow_event_consumer_with")
            .nth(1)
            .expect("消费者函数应存在");
        // 边界收到函数结尾（`})` + `}` 收尾的 spawn 块）之前的下一个顶层 fn。
        let body = body.split("\nfn ").next().unwrap_or(body);

        for ev in [
            // 会话状态：不提示就等于"卡死了"。
            "Paused",
            "Resumed",
            // 失败可见性。
            "Error",
            "ToolError",
            "AdvisorTerminated",
            "TimeoutWarning",
            // 我们刚接进 CLI 的两个工具，产出必须让用户知道。
            "PlanProposed",
            "TaskReport",
            // 既有的。
            "ChoiceRequested",
            "WorkflowStep",
            "WorkflowFinished",
            "DelegateStarted",
            "DelegateFinished",
            "RoleTurn",
        ] {
            assert!(
                body.contains(&format!("ChatEvent::{ev}")),
                "CLI 事件消费者缺少 `{ev}` 分支（UI 侧对用户可见）"
            );
        }
    }

    /// `Paused` 必须落到状态行上（而不仅是历史里一行）。
    ///
    /// 用状态而非输出做判据：`ui_emit` 在非 TTY 下写 stderr，测试里不好
    /// 捕获；`ui_activity()` 是进程内可读的，能确定性断言。
    ///
    /// 为什么这条重要：模型全链哑掉时后端自动暂停等人，用户若看不到
    /// "已暂停"就只会以为卡死了。实机没法稳定复现（要让所有 provider
    /// 同时失败），所以用合成事件锁住。
    #[tokio::test]
    async fn paused_event_surfaces_in_the_status_line() {
        let (tx, rx) = tokio::sync::broadcast::channel(8);
        let task = spawn_cli_workflow_event_consumer_with(rx, canned_answerer(None));
        ui_set_activity(None);

        tx.send(latte_agent_core::controller::ChatEvent::Paused {
            reason: "模型不可用（glm-5.3: 400; MiniMax-M3: 流式空闲超时）".into(),
        })
        .unwrap();
        // 等消费者处理（事件是异步的）。
        let mut label = None;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if let Some((l, _)) = ui_activity().lock().clone() {
                label = Some(l);
                break;
            }
        }
        assert_eq!(
            label.as_deref(),
            Some("已暂停，等待恢复"),
            "Paused 必须反映到状态行，否则用户只觉得卡死了"
        );

        tx.send(latte_agent_core::controller::ChatEvent::Resumed).unwrap();
        let mut cleared = false;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if ui_activity().lock().is_none() {
                cleared = true;
                break;
            }
        }
        assert!(cleared, "Resumed 必须清掉暂停状态");

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    /// `/clear` 必须连 advisor 宿主状态一起清。
    ///
    /// `last_user_input` 是审查 prompt 的「主诉求」基准，残留会让清空后
    /// 的第一次审查拿**上一个话题**当准绳；未消费的 hint 会串到新话题上。
    #[test]
    fn clear_command_also_resets_advisor_host_state() {
        let full = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/commands/chat.rs"),
        )
        .expect("chat.rs");
        let src = full.split("#[cfg(test)]").next().expect("非测试部分");
        let body = src
            .split("\"/clear\" => {")
            .nth(1)
            .expect("/clear 分支应存在");
        let body = body.split("\n            \"").next().unwrap_or(body);
        assert!(
            body.contains("last_input.lock().clear()"),
            "/clear 必须清 advisor 的 last_user_input（审查基准）"
        );
        assert!(
            body.contains("hints.lock().clear()"),
            "/clear 必须清未消费的 advisor hint，否则会串到新话题"
        );
    }

    #[test]
    fn choice_index_parsing() {
        // 单选：合法序号。
        assert_eq!(parse_choice_indices("2", 3, false), Some(vec![1]));
        // 多选：逗号 / 空格 / 中文逗号都认，去重。
        assert_eq!(parse_choice_indices("1,3", 3, true), Some(vec![0, 2]));
        assert_eq!(parse_choice_indices("1 3", 3, true), Some(vec![0, 2]));
        assert_eq!(parse_choice_indices("1，3", 3, true), Some(vec![0, 2]));
        assert_eq!(parse_choice_indices("2,2", 3, true), Some(vec![1]));
        // 越界 / 0 / 非数字 → None（按自由作答处理，原样交给模型）。
        assert_eq!(parse_choice_indices("4", 3, false), None);
        assert_eq!(parse_choice_indices("0", 3, false), None);
        assert_eq!(parse_choice_indices("我想先看代码", 3, false), None);
        // 单选给多个序号 → None：意图不明，不偷偷取第一个。
        assert_eq!(parse_choice_indices("1,2", 3, false), None);
        // 零选项的 ask（模型发歪了）不该 panic。
        assert_eq!(parse_choice_indices("1", 0, false), None);
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
                    // RoundStarted 已在本轮开头发出：被暂停/终止打断的
                    // 轮次也要补 RoundEnded，保持 trace 里两者成对
                    // （v1.1 契约：每轮结束一次 + 每次中途暂停一次——
                    // ask_human 在 turn 内 pause 会话后走的就是这条路，
                    // 此前漏发导致 hil_v11_trace_events_e2e 失败）。
                    mgr.emit_round_ended(round_num);
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
                // 统一实现见 latte_agent_core::inject_queue。旧的内联版本**不检查
                // 空内容**，空队列文件会注入一条只有 [INJECTED] 头、没有正文的
                // 消息（controller 那份内联版检查了，两边行为不一致）。
                if let Some(content) = latte_agent_core::inject_queue::drain(
                    mgr.worktree_root(),
                    &role_id,
                ) {
                    let synth = Message {
                        role: MsgRole::User,
                        content: vec![latte_ai::models::ContentPart::text(
                            latte_agent_core::inject_queue::format_injected(&content),
                        )],
                        tool_call_id: None,
                        tool_calls: None,
                    };
                    mgr.append_to_role(&role_id, synth).ok();
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
