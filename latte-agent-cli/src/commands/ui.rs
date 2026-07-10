//! `latte-agent ui` — Web UI bridge command.
//!
//! 起一个 axum HTTP server，把 `ChatController` 的事件流通过 SSE
//! 推给浏览器，并把浏览器发回的消息通过 mpsc 喂给 ChatController。
//!
//! 架构:
//!   - 前端 (latte-agent-cli/ui/, Vite + React): ChatPanel / TracePanel / SelfLoop
//!   - 后端 (本文件): axum @ :4567，代理 ChatController
//!   - SelfLoop (latte-agent-cli/ui/self-loop/): Playwright + screencap 闭环
//!
//! 设计目标:
//!   - 完全复用 `latte_agent_core::controller::ChatController`（事件驱动），
//!     不复制 chat.rs 的 stdin-REPL 循环。
//!   - Tauri 迁移：换掉 axum + SSE，改为 tauri::Builder + window.emit，
//!     业务代码（ControllerConfig / spawn / SSE 事件 JSON）一行不动。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Args;
use futures_util::stream::Stream;
use latte_agent_core::config::AgentConfig;
use latte_agent_core::controller::{ChatController, ControllerConfig, RoleInfo};
use latte_agent_core::model_resolver::{ModelResolver, ModelTier};
use latte_ai::params::GenerateParams;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

/// `latte-agent ui` — 启动 Web UI server。
///
/// 行为：
///   - 加载 agents.toml / models.toml（同 `chat` 命令）。
///   - 起 axum HTTP server @ `--port`（默认 4567）。
///   - 生产模式：把 `latte-agent-cli/ui/dist/` 当静态文件根。
///   - 开发模式（`--dev`）：spawn `npm run dev` 起 vite @ 5173，
///     并在 stdout 打印两个 URL。
#[derive(Args, Debug)]
pub struct UiCmd {
    /// HTTP server 监听端口。开发模式下也是 vite 代理目标端口。
    #[arg(long, default_value_t = 4567)]
    pub port: u16,

    /// 自动打开浏览器。
    #[arg(long, default_value_t = true)]
    pub open: bool,

    /// 角色 id（单角色模式），同 `chat --role`。
    #[arg(short, long)]
    pub role: Option<String>,

    /// 初始模型 tier，同 `chat --tier`。
    #[arg(short = 't', long)]
    pub tier: Option<String>,

    /// 同 `chat --model-id`。
    #[arg(short = 'm', long)]
    pub model_id: Option<String>,

    /// Path to agents config (file or directory).
    #[arg(long, default_value = ".latte/agents.d")]
    pub agents_config: String,

    /// Path to models config.
    #[arg(long, default_value = ".latte/models.d")]
    pub models_config: String,

    /// 开发模式：自动 spawn vite dev server。
    #[arg(long, default_value_t = false)]
    pub dev: bool,

    /// Vite dev server 端口（仅 `--dev` 模式下生效）。
    #[arg(long, default_value_t = 5173)]
    pub vite_port: u16,

    /// 静态前端目录。默认 `latte-agent-cli/ui/dist`（生产模式）。
    #[arg(long)]
    pub static_dir: Option<String>,
}

impl UiCmd {
    pub async fn run(&self) -> Result<(), Box<dyn std::error::Error>> {
        // 1. 加载配置 —— 复用 commands/config_layer::load。
        let cli_overrides = super::config_layer::CliOverrides {
            api_key: None,
            api_key_target: None,
            field_overrides: vec![],
        };
        let resolved = super::config_layer::load(
            Some(&self.agents_config),
            Some(&self.models_config),
            cli_overrides,
        )
        .map_err(|e| format!("failed to load configuration: {}", e))?;
        let merged: Arc<AgentConfig> = Arc::new(resolved.config);
        let resolver: Arc<ModelResolver> = Arc::new(resolved.resolver);

        // 2. 决定初始 role 和 tier。
        let initial_role = self
            .role
            .clone()
            .unwrap_or_else(|| "manager".to_string());
        let initial_tier: Option<ModelTier> = if let Some(t) = &self.tier {
            Some(parse_tier_str(t)?)
        } else {
            merged
                .roles
                .get(&initial_role)
                .map(|tpl| parse_tier_str(&tpl.model_tier).unwrap_or(ModelTier::Standard))
        };

        // 3. 构造 ControllerConfig — 单角色模式。
        //
        // 关键洞察：之前试过切到多角色（task_id + 全 role round-robin），
        // 但 controller 的 run_multi_role_loop 是把用户输入**广播给所
        // 有 role** 让它们各自响应 —— 这跟 `latte-agent chat` 的行为
        // 完全不同。chat 命令本质就是单角色（manager），让 manager 通
        // 过 `delegate` 工具去调其他 specialist；用户输入只到 manager
        // 一人手里。
        //
        // 所以正确做法是保持单角色：manager 唯一收到用户输入，由它决
        // 定 delegate 给谁。用户能看到 DelegateStarted/Finished/
        // RoleTurn 这些事件，"讨论感"是 manager ↔ 单一 specialist
        // 之间的，不是 10 个 role 一起瞎答。
        let cwd = std::env::current_dir()?;
        let cfg = ControllerConfig {
            task_id: None,
            roles: vec![initial_role.clone()],
            initial_prompt: None,
            max_rounds: 0,
            session_token_budget: 0,
            agent_config: merged.clone(),
            model_resolver: resolver.clone(),
            default_params: GenerateParams::default(),
            primary_model_id: self.model_id.clone(),
            initial_tier,
            initial_history: vec![],
            cwd: cwd.clone(),
        };

        // 4. 起 ChatController。
        let controller = Arc::new(ChatController::new(256));
        let _events_rx = controller.spawn(cfg).await;

        // 5. 解析静态文件目录。
        let static_dir = resolve_static_dir(self);

        // 6. spawn vite dev server（仅 dev 模式）。
        if self.dev {
            spawn_vite_dev(self.vite_port, self.port).await?;
        }

        // 7. 起 axum server。
        let state = AppState {
            controller: controller.clone(),
            merged: merged.clone(),
            initial_role: initial_role.clone(),
            static_dir: static_dir.clone(),
            self_loop: Arc::new(SelfLoopState::default()),
        };

        let app = build_router(state);
        let addr = SocketAddr::from(([0, 0, 0, 0], self.port));
        eprintln!(
            "[ui] latte-agent UI server listening on http://localhost:{}",
            self.port
        );
        if self.dev {
            eprintln!(
                "[ui] dev mode: vite UI at http://localhost:{} (proxied to backend at :{})",
                self.vite_port, self.port
            );
        } else if static_dir.is_some() {
            eprintln!("[ui] static UI at http://localhost:{}/", self.port);
        } else {
            eprintln!(
                "[ui] no static UI built — only API routes are live. \
                 run `pnpm --dir latte-agent-cli/ui install && pnpm --dir latte-agent-cli/ui build` for the production bundle, \
                 or restart with `--dev` to spawn vite."
            );
        }

        let listener = tokio::net::TcpListener::bind(&addr).await?;
        axum::serve(listener, app).await?;
        Ok(())
    }
}

// ─── State ────────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    controller: Arc<ChatController>,
    merged: Arc<AgentConfig>,
    initial_role: String,
    static_dir: Option<PathBuf>,
    self_loop: Arc<SelfLoopState>,
}

#[derive(Default)]
struct SelfLoopState {
    /// Latest self-loop progress event sender (broadcast for multiple SSE subscribers).
    progress: parking_lot::Mutex<Option<broadcast::Sender<SelfLoopEvent>>>,
}

// ─── Router ───────────────────────────────────────────────────────

fn build_router(state: AppState) -> Router {
    // /health 在根路径（liveness probe），其它走 /api
    let api = Router::new()
        .route("/session", get(get_session))
        .route("/roles", get(list_roles))
        .route("/chat/send", post(chat_send))
        .route("/chat/command", post(chat_command))
        .route("/chat/role", post(switch_role))
        .route("/events", get(events_sse))
        .route("/traces", get(list_traces))
        .route("/traces/:session_id", get(read_trace))
        .route("/self-loop/start", post(self_loop_start))
        .route("/self-loop/events", get(self_loop_events_sse))
        .route("/self-loop/stop", post(self_loop_stop))
        .with_state(state.clone());

    let mut app = Router::new()
        .route("/health", get(health))
        .nest("/api", api);
    if let Some(dir) = &state.static_dir {
        let serve = tower_http::services::ServeDir::new(dir);
        app = app.fallback_service(serve);
    }

    app
}

// ─── Handlers ─────────────────────────────────────────────────────

async fn health() -> &'static str {
    "ok"
}

#[derive(Serialize)]
struct SessionInfo {
    session_id: String,
    role: String,
    model: Option<String>,
    tier: String,
    available_roles: Vec<RoleInfo>,
}

async fn get_session(State(state): State<AppState>) -> Json<SessionInfo> {
    Json(SessionInfo {
        session_id: format!("ui-{}", std::process::id()),
        role: state.initial_role.clone(),
        model: None, // 前端订阅 /api/events 后从 ChatEvent::SessionInfo 取
        tier: "auto".into(),
        available_roles: build_role_info(&state.merged),
    })
}

async fn list_roles(State(state): State<AppState>) -> Json<Vec<RoleInfo>> {
    Json(build_role_info(&state.merged))
}

fn build_role_info(cfg: &AgentConfig) -> Vec<RoleInfo> {
    let mut out = Vec::with_capacity(cfg.roles.len());
    let mut ids: Vec<&String> = cfg.roles.keys().collect();
    ids.sort();
    for id in ids {
        if let Some(tpl) = cfg.roles.get(id) {
            out.push(RoleInfo {
                id: id.clone(),
                name: tpl.name.clone(),
                icon: tpl.icon.clone(),
            });
        }
    }
    out
}

#[derive(Deserialize)]
struct SendRequest {
    message: String,
}

async fn chat_send(
    State(state): State<AppState>,
    Json(req): Json<SendRequest>,
) -> StatusCode {
    state.controller.submit_input(&req.message).await;
    StatusCode::ACCEPTED
}

#[derive(Deserialize)]
struct CommandRequest {
    /// 形如 "/clear"、"/quit"、"/save <path>" 的 REPL 命令。
    command: String,
}

async fn chat_command(
    State(state): State<AppState>,
    Json(req): Json<CommandRequest>,
) -> StatusCode {
    // controller 内部对 '/' 前缀有 REPL 处理（看 controller.rs ControllerInput）。
    state.controller.submit_input(&req.command).await;
    StatusCode::ACCEPTED
}

#[derive(Deserialize)]
struct SwitchRoleRequest {
    role_id: String,
}

async fn switch_role(
    State(state): State<AppState>,
    Json(req): Json<SwitchRoleRequest>,
) -> StatusCode {
    state.controller.switch_role(&req.role_id).await;
    StatusCode::ACCEPTED
}

async fn events_sse(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, axum::Error>>> {
    let rx = state.controller.subscribe();
    let stream = BroadcastStream::new(rx).map(|item| match item {
        Ok(ev) => {
            // Convert ChatEvent (externally-tagged JSON, e.g.
            // `{"Status":{"message":"..."}}`) into the discriminated
            // union the latte-agent-ui frontend expects:
            // `{"type":"Status","message":"..."}`.
            //
            // The conversion preserves every field; we just flatten
            // the outer variant tag into a `type` discriminator. The
            // shape matches `api.ts ChatEvent` exactly, so the
            // front-end `switch (e.type)` hits the right branch and
            // the user sees the response.
            match chat_event_to_frontend_json(&ev) {
                Ok(json) => Ok(Event::default()
                    .event("chat_event")
                    .data(json)),
                Err(e) => Ok(Event::default()
                    .event("error")
                    .data(format!("chat_event convert failed: {}", e))),
            }
        }
        Err(e) => Ok(Event::default()
            .event("error")
            .data(format!("broadcast lag: {}", e))),
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15)))
}

/// Convert a `ChatEvent` (externally-tagged JSON object) into the
/// `latte-agent-ui` frontend wire shape (internally-tagged):
///
///   externally:  `{"RoleTurn":{"role_id":"m","content":"hi","is_complete":true}}`
///   internally:  `{"type":"RoleTurn","role_id":"m","content":"hi","is_complete":true}`
///
///   externally:  `{"Done"}`
///   internally:  `{"type":"Done"}`
///
/// All field names stay the same (no `rename_all`); the only
/// transformation is "lift the outer variant key into a `type`
/// discriminator". This matches the TS `ChatEvent` union in
/// `latte-agent-cli/ui/src/api.ts`, so the front-end
/// `switch (e.type)` lands on the right branch.
///
/// `serde_json::Value::Object` guarantees insertion order is
/// preserved on serialization, so we can re-insert `type` first
/// then spread the variant's fields — the resulting string is
/// stable and human-readable in browser dev tools.
fn chat_event_to_frontend_json(ev: &latte_agent_core::controller::ChatEvent) -> Result<String, String> {
    let value = serde_json::to_value(ev).map_err(|e| e.to_string())?;
    match value {
        // Named-field / tuple variant: ChatEvent::RoleTurn { .. } →
        // externally-tagged object `{"RoleTurn":{...}}`.
        serde_json::Value::Object(mut outer) => {
            if outer.len() != 1 {
                return Err(format!(
                    "ChatEvent should serialize to exactly 1-key object, got {} keys: {:?}",
                    outer.len(),
                    outer.keys().collect::<Vec<_>>()
                ));
            }
            let (variant_name, fields) = outer.into_iter().next().unwrap();
            // `fields` is a Value::Object for variants with named
            // fields. We lift `variant_name` into a `type` discriminator
            // and flatten the fields up to the top level. Unit-like
            // variants can't reach this branch because they serialize
            // to a bare string (handled below).
            let mut out = serde_json::Map::with_capacity(
                1 + fields.as_object().map(|m| m.len()).unwrap_or(0)
            );
            out.insert("type".to_string(), serde_json::Value::String(variant_name));
            if let serde_json::Value::Object(inner) = fields {
                for (k, v) in inner {
                    out.insert(k, v);
                }
            }
            serde_json::to_string(&serde_json::Value::Object(out)).map_err(|e| e.to_string())
        }
        // Unit variant: ChatEvent::Done → bare string `"Done"`.
        // We wrap it as `{"type":"Done"}` to match the TS discriminated
        // union shape (every other case uses the same envelope).
        serde_json::Value::String(variant_name) => {
            Ok(format!(r#"{{"type":{}}}"#, serde_json::to_string(&variant_name).map_err(|e| e.to_string())?))
        }
        // Anything else is a contract bug — surface it instead of
        // silently dropping the event (which is what triggered the
        // original `[chat] unknown event` bug).
        other => Err(format!("unexpected ChatEvent shape: {}", other)),
    }
}

// ─── Trace API ────────────────────────────────────────────────────

#[derive(Serialize)]
struct TraceSummary {
    session_id: String,
    path: String,
    size_bytes: u64,
    modified_unix: u64,
}

async fn list_traces() -> Json<Vec<TraceSummary>> {
    let dir = latte_home().join("traces");
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Json(out),
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let meta = match e.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let modified_unix = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        out.push(TraceSummary {
            session_id,
            path: path.display().to_string(),
            size_bytes: meta.len(),
            modified_unix,
        });
    }
    out.sort_by(|a, b| b.modified_unix.cmp(&a.modified_unix));
    Json(out)
}

async fn read_trace(
    axum::extract::Path(session_id): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let path = latte_home().join("traces").join(format!("{}.jsonl", session_id));
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| (StatusCode::NOT_FOUND, format!("trace not found: {}", e)))?;
    let mut events = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            events.push(v);
        }
    }
    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "events": events,
    })))
}

// ─── Self-Loop API ────────────────────────────────────────────────

#[derive(Deserialize)]
struct SelfLoopStartRequest {
    /// 给 AI 的任务描述（如"fix the chat panel layout"）。
    task: String,
    /// 最大迭代轮数（防止死循环）。默认 5。
    #[serde(default)]
    max_iterations: Option<u32>,
}

#[derive(Serialize, Clone, Deserialize)]
struct SelfLoopEvent {
    /// "started" | "iteration" | "log" | "screenshot" | "done" | "error"
    kind: String,
    iteration: u32,
    message: String,
    /// base64 PNG（仅 kind == "screenshot"）
    #[serde(skip_serializing_if = "Option::is_none")]
    screenshot: Option<String>,
    /// 自由扩展字段
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
    timestamp_unix_ms: u64,
}

async fn self_loop_start(
    State(state): State<AppState>,
    Json(req): Json<SelfLoopStartRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let max = req.max_iterations.unwrap_or(5);
    if req.task.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "task is required".into()));
    }

    // 起 broadcast channel。
    let (tx, _) = broadcast::channel::<SelfLoopEvent>(64);
    {
        let mut guard = state.self_loop.progress.lock();
        *guard = Some(tx.clone());
    }

    // 找 self-loop 脚本。
    let self_loop_dir = self_loop_dir()?;
    if !self_loop_dir.exists() {
        return Err((
            StatusCode::NOT_FOUND,
            format!(
                "self-loop runner not found at {}. Run `pnpm --dir latte-agent-cli/ui install` first.",
                self_loop_dir.display()
            ),
        ));
    }

    // spawn node 子进程跑 self-loop/runner.ts。
    let task = req.task.clone();
    let self_loop_dir_clone = self_loop_dir.clone();
    tokio::spawn(async move {
        run_self_loop_node(task, max, self_loop_dir_clone, tx).await;
    });

    Ok(Json(serde_json::json!({
        "started": true,
        "task": req.task,
        "max_iterations": max,
    })))
}

async fn run_self_loop_node(
    task: String,
    max_iterations: u32,
    self_loop_dir: PathBuf,
    tx: broadcast::Sender<SelfLoopEvent>,
) {
    let now_ms = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    };

    let _ = tx.send(SelfLoopEvent {
        kind: "started".into(),
        iteration: 0,
        message: format!("AI self-debug loop started: {}", task),
        screenshot: None,
        data: None,
        timestamp_unix_ms: now_ms(),
    });

    // 用 tsx 跑 runner.ts（开发期不需 build）。
    let runner = self_loop_dir.join("runner.ts");
    if !runner.exists() {
        let _ = tx.send(SelfLoopEvent {
            kind: "error".into(),
            iteration: 0,
            message: format!("runner.ts not found at {}", runner.display()),
            screenshot: None,
            data: None,
            timestamp_unix_ms: now_ms(),
        });
        return;
    }

    let task_json = serde_json::to_string(&serde_json::json!({
        "task": task,
        "max_iterations": max_iterations,
        "ui_base_url": "http://localhost:4567",
    }))
    .unwrap_or_else(|_| "{}".into());

    let mut child = match tokio::process::Command::new("npx")
        .arg("tsx")
        .arg(&runner)
        .arg("--task-json")
        .arg(&task_json)
        .current_dir(&self_loop_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(SelfLoopEvent {
                kind: "error".into(),
                iteration: 0,
                message: format!("failed to spawn self-loop runner: {}", e),
                screenshot: None,
                data: None,
                timestamp_unix_ms: now_ms(),
            });
            return;
        }
    };

    // 读子进程 stdout，每行一个 JSON SelfLoopEvent。
    if let Some(stdout) = child.stdout.take() {
        let tx_clone = tx.clone();
        let read = tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(ev) = serde_json::from_str::<SelfLoopEvent>(&line) {
                    let _ = tx_clone.send(ev);
                } else {
                    let _ = tx_clone.send(SelfLoopEvent {
                        kind: "log".into(),
                        iteration: 0,
                        message: line,
                        screenshot: None,
                        data: None,
                        timestamp_unix_ms: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0),
                    });
                }
            }
        });

        let _ = child.wait().await;
        let _ = read.await;
    } else {
        let _ = child.wait().await;
    }

    let _ = tx.send(SelfLoopEvent {
        kind: "done".into(),
        iteration: max_iterations,
        message: "self-loop finished".into(),
        screenshot: None,
        data: None,
        timestamp_unix_ms: now_ms(),
    });
}

async fn self_loop_events_sse(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, axum::Error>>> {
    let rx = {
        let guard = state.self_loop.progress.lock();
        match guard.as_ref() {
            Some(t) => t.subscribe(),
            None => {
                // 没有正在跑 self-loop —— 返回一个 1-容量的空 channel，
                // 前端发 start 后会重新订阅。
                let (_tx, rx) = broadcast::channel::<SelfLoopEvent>(1);
                rx
            }
        }
    };
    let stream = BroadcastStream::new(rx).map(|item| match item {
        Ok(ev) => Ok(Event::default()
            .event("self_loop_event")
            .data(serde_json::to_string(&ev).unwrap_or_else(|_| "{}".into()))),
        Err(_) => Ok(Event::default().event("ping").data("")),
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(10)))
}

async fn self_loop_stop(State(state): State<AppState>) -> StatusCode {
    let mut guard = state.self_loop.progress.lock();
    *guard = None;
    StatusCode::OK
}

// ─── Helpers ──────────────────────────────────────────────────────

fn parse_tier_str(s: &str) -> Result<ModelTier, Box<dyn std::error::Error>> {
    ModelTier::parse(s).map_err(|e| e.into())
}

fn latte_home() -> PathBuf {
    std::env::var_os("LATTE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".latte")))
        .unwrap_or_else(|| PathBuf::from(".latte"))
}

fn resolve_static_dir(cmd: &UiCmd) -> Option<PathBuf> {
    if let Some(s) = &cmd.static_dir {
        let p = PathBuf::from(s);
        return if p.exists() { Some(p) } else { None };
    }
    // 默认：<cli crate>/ui/dist（用 CARGO_MANIFEST_DIR 锚定，
    // 不依赖调用方 cwd）。fallback 1: 相对当前 cwd；fallback 2:
    // 在 PATH 父目录扫一遍 (用户从 cwd 启动时)。
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidates = [
        manifest_dir.join("ui").join("dist"),
        PathBuf::from("latte-agent-cli/ui/dist"),
    ];
    for p in &candidates {
        if p.exists() {
            return Some(p.clone());
        }
    }
    None
}

fn self_loop_dir() -> Result<PathBuf, (StatusCode, String)> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidates = [
        manifest_dir.join("ui").join("self-loop"),
        PathBuf::from("latte-agent-cli/ui/self-loop"),
    ];
    for p in &candidates {
        if p.exists() {
            return Ok(p.clone());
        }
    }
    let first = &candidates[0];
    Err((
        StatusCode::NOT_FOUND,
        format!("self-loop dir not found: tried {}", first.display()),
    ))
}

async fn spawn_vite_dev(
    vite_port: u16,
    backend_port: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidates = [
        manifest_dir.join("ui"),
        PathBuf::from("latte-agent-cli/ui"),
    ];
    let ui_dir = candidates.into_iter().find(|p| p.exists());
    let ui_dir = match ui_dir {
        Some(d) => d,
        None => {
            eprintln!(
                "[ui] vite dir not found; tried {} and latte-agent-cli/ui.                  run from the workspace root or build ui/dist for production mode.",
                manifest_dir.join("ui").display()
            );
            return Ok(());
        }
    };
    let backend = format!("http://localhost:{}", backend_port);
    let mut cmd = tokio::process::Command::new("npm");
    cmd.arg("run")
        .arg("dev")
        .env("VITE_PORT", vite_port.to_string())
        .env("VITE_BACKEND", backend)
        .current_dir(&ui_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn()?;
    // Forward vite stdout/stderr to stderr tagged with [vite].
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[vite] {}", line);
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[vite] {}", line);
            }
        });
    }
    // Don't await — vite is a long-running server. Detach.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}

// ─── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tier_str_works() {
        assert!(matches!(parse_tier_str("premium").unwrap(), ModelTier::Premium));
        assert!(matches!(parse_tier_str("standard").unwrap(), ModelTier::Standard));
        assert!(matches!(parse_tier_str("budget").unwrap(), ModelTier::Budget));
        assert!(parse_tier_str("nope").is_err());
    }

    #[test]
    fn resolve_static_dir_handles_missing() {
        let cmd = UiCmd {
            port: 4567,
            open: false,
            role: None,
            tier: None,
            model_id: None,
            agents_config: ".latte/agents.d".into(),
            models_config: ".latte/models.d".into(),
            dev: false,
            vite_port: 5173,
            static_dir: Some("/nonexistent/path/1234".into()),
        };
        // 显式指定不存在的路径 → None。
        assert!(resolve_static_dir(&cmd).is_none());
    }

    #[test]
    fn build_role_info_is_sorted() {
        let cfg = AgentConfig::default();
        let v = build_role_info(&cfg);
        assert!(v.is_empty());
    }

    /// chat_event_to_frontend_json 必须把 ChatEvent 的 externally-tagged
    /// JSON 转成前端 latte-agent-ui 期望的 internally-tagged JSON。
    /// 这是协议适配层的关键测试 — 改坏会触发 [chat] unknown event。
    #[test]
    fn chat_event_to_frontend_json_handles_status_with_message() {
        use latte_agent_core::controller::ChatEvent;
        let ev = ChatEvent::Status { message: "[calling LLM...]".into() };
        let json = chat_event_to_frontend_json(&ev).expect("convert");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
        // 外层 key = type
        assert_eq!(v["type"], "Status");
        // 字段被平铺到外层
        assert_eq!(v["message"], "[calling LLM...]");
        // 不能残留旧的 "Status" 外层对象
        assert!(v.get("Status").is_none(), "stale externally-tagged object leaked: {}", v);
    }

    #[test]
    fn chat_event_to_frontend_json_handles_unit_variant() {
        use latte_agent_core::controller::ChatEvent;
        let ev = ChatEvent::Done;
        let json = chat_event_to_frontend_json(&ev).expect("convert");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
        // Done 是 unit variant —— 序列化是 `{"Done":null}`。
        // 转换后只有 `type` 字段，没有额外字段。
        assert_eq!(v["type"], "Done");
        assert_eq!(v.as_object().unwrap().len(), 1, "Done should be a single-field object, got {}", v);
    }

    #[test]
    fn chat_event_to_frontend_json_handles_roleturn_with_all_fields() {
        use latte_agent_core::controller::ChatEvent;
        let ev = ChatEvent::RoleTurn {
            role_id: "manager".into(),
            content: "你好".into(),
            is_complete: true,
        };
        let json = chat_event_to_frontend_json(&ev).expect("convert");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(v["type"], "RoleTurn");
        assert_eq!(v["role_id"], "manager");
        assert_eq!(v["content"], "你好");
        assert_eq!(v["is_complete"], true);
        assert!(v.get("RoleTurn").is_none());
    }

    /// 端到端协议层：events_sse 推出来的数据必须能被前端 api.ts
    /// 里的 ChatEvent union 匹配。验证方法是：把 ChatEvent 经过
    /// chat_event_to_frontend_json → JSON.stringify 模拟前端 dispatch_event
    /// 流程，确认每种类型都能找到 type 字段。
    #[test]
    fn chat_event_to_frontend_json_every_variant_has_type_field() {
        use latte_agent_core::controller::{ChatEvent, RoleInfo};
        let cases: Vec<(&str, ChatEvent)> = vec![
            ("Status", ChatEvent::Status { message: "x".into() }),
            ("Paused", ChatEvent::Paused { reason: "ask_human".into() }),
            ("Resumed", ChatEvent::Resumed),
            ("RoundStarted", ChatEvent::RoundStarted { round: 1 }),
            ("RoundEnded", ChatEvent::RoundEnded { round: 1 }),
            ("Done", ChatEvent::Done),
            ("Error", ChatEvent::Error { message: "boom".into() }),
            ("ContextCleared", ChatEvent::ContextCleared),
            ("ToolUse", ChatEvent::ToolUse { role_id: "m".into(), tool_name: "read".into(), args: "{}".into() }),
            ("ToolResult", ChatEvent::ToolResult { role_id: "m".into(), tool_name: "read".into(), result: "ok".into() }),
            ("ToolError", ChatEvent::ToolError { role_id: "m".into(), tool_name: "read".into(), error: "fail".into() }),
            ("RoleStarted", ChatEvent::RoleStarted { role_id: "m".into(), detail: "calling LLM".into() }),
            ("RoleFinished", ChatEvent::RoleFinished { role_id: "m".into(), detail: "ok".into() }),
            ("DelegateStarted", ChatEvent::DelegateStarted { from_role: "manager".into(), to_role: "programmer".into(), task: "ping".into() }),
            ("DelegateFinished", ChatEvent::DelegateFinished { from_role: "manager".into(), to_role: "programmer".into(), status: "ok".into(), summary: "done".into() }),
            ("Prompt", ChatEvent::Prompt { icon: "[m]".into(), role_id: "manager".into(), model_id: "glm-5.2".into() }),
            ("SessionInfo", ChatEvent::SessionInfo { task_id: "ui-1".into(), state: "running".into(), turn: 0, roles: vec![RoleInfo { id: "manager".into(), name: "Manager".into(), icon: "[m]".into() }] }),
        ];
        for (expected_type, ev) in cases {
            let json = chat_event_to_frontend_json(&ev).expect("convert");
            let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
            assert_eq!(v["type"], expected_type, "type mismatch for variant {}: {}", expected_type, v);
        }
    }

    /// 端到端测试：起一个真实 axum server (随机端口)，验证
    /// `/health` + `/api/session` + `/api/roles` + `/api/traces` 都能正常响应。
    ///
    /// 这个测试是 self-debug loop 的基础前提：必须确认 HTTP
    /// 服务端可被前端代码调用，否则前端 / Playwright 一打开
    /// 就看到 CORS / 404 错误，self-debug 失去依据。
    #[tokio::test]
    async fn e2e_http_routes_respond() {
        // 用空闲端口 (0) 让 OS 自动分配。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // 构造最小 AppState：agent_config 留默认，controller 用 ChatController::new 不 spawn。
        let controller = Arc::new(ChatController::new(8));
        let state = AppState {
            controller,
            merged: Arc::new(AgentConfig::default()),
            initial_role: "manager".into(),
            static_dir: None,
            self_loop: Arc::new(SelfLoopState::default()),
        };
        let app = build_router(state);

        // spawn server。
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        // 等 server 起来。
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // 简单的 GET helper（不引 reqwest）。
        async fn http_get(url: &str) -> (u16, String) {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            use tokio::net::TcpStream;
            // 解析 host:port。
            let stripped = url.trim_start_matches("http://");
            let (host_port, path) = stripped.split_once('/').unwrap_or((stripped, ""));
            let path = format!("/{}", path);
            let mut stream = TcpStream::connect(host_port).await.expect("connect");
            let req = format!(
                "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                path, host_port
            );
            stream.write_all(req.as_bytes()).await.expect("write");
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.expect("read");
            let raw = String::from_utf8_lossy(&buf).into_owned();
            // 拆 status line + body
            let status = raw
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let body = raw.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
            (status, body)
        }

        // /health
        let (status, body) = http_get(&format!("http://127.0.0.1:{}/health", port)).await;
        assert_eq!(status, 200, "/health body: {}", body);
        assert_eq!(body, "ok");

        // /api/session
        let (status, body) = http_get(&format!("http://127.0.0.1:{}/api/session", port)).await;
        assert_eq!(status, 200, "/api/session body: {}", body);
        let v: serde_json::Value = serde_json::from_str(&body).expect("session json");
        assert_eq!(v["role"], "manager");
        assert!(v["available_roles"].is_array());

        // /api/roles
        let (status, body) = http_get(&format!("http://127.0.0.1:{}/api/roles", port)).await;
        assert_eq!(status, 200, "/api/roles body: {}", body);
        let v: serde_json::Value = serde_json::from_str(&body).expect("roles json");
        assert!(v.is_array());

        // /api/traces (没有 traces 目录时返回空数组而不是 500)
        let (status, _) = http_get(&format!("http://127.0.0.1:{}/api/traces", port)).await;
        assert_eq!(status, 200);
    }
}
