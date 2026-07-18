//! `latte-agent-ui-server` — 可复用的 Web UI server crate。
//!
//! 起一个 axum HTTP server，把 `ChatController` 的事件流通过 SSE
//! 推给浏览器，并把浏览器发回的消息通过 mpsc 喂给 ChatController。
//!
//! 架构:
//!   - 前端 (latte-agent-cli/ui/, Vite + React): ChatPanel / TracePanel / SelfLoop
//!   - 后端 (本 crate): axum，代理 ChatController
//!   - SelfLoop (latte-agent-cli/ui/self-loop/): Playwright + screencap 闭环
//!
//! 设计目标:
//!   - 完全复用 `latte_agent_core::controller::ChatController`（事件驱动），
//!     不复制 chat.rs 的 stdin-REPL 循环。
//!   - 可嵌入：CLI（`latte-agent ui`）只是薄壳；Tauri 应用
//!     （latte-code-editor，docs/ui-embedding-design.md §6 阶段 0）在进程内
//!     以 `bind: 127.0.0.1:0` 调用 [`spawn`]，iframe 指向
//!     返回句柄里的实际地址。
//!   - 事件 JSON 格式（契约 C2）由
//!     `latte_agent_core::event_json::chat_event_to_frontend_json`
//!     单点实现，axum 与未来 Tauri 适配器共用。
//!
//! 最小用法（编辑器内嵌）:
//!
//! ```no_run
//! # async fn demo() -> anyhow::Result<()> {
//! let handle = latte_agent_ui_server::spawn(latte_agent_ui_server::UiServerConfig {
//!     bind: "127.0.0.1:0".parse()?,
//!     static_dir: Some(std::path::PathBuf::from("path/to/ui/dist")),
//!     agent_config: latte_agent_core::config::AgentConfig::default(),
//!     model_resolver: latte_agent_core::model_resolver::ModelResolver::from_config(
//!         &latte_agent_core::config::AgentConfig::default(),
//!     )?,
//!     role: None,
//!     tier: None,
//!     model_id: None,
//!     agents_config: ".latte/agents.d".into(),
//! })
//! .await?;
//! println!("UI at http://{}/", handle.addr);
//! # Ok(())
//! # }
//! ```

mod handlers;
mod role_graph;
mod self_loop;
mod sessions;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use latte_agent_core::config::AgentConfig;
use latte_agent_core::model_resolver::{ModelResolver, ModelTier};

use handlers::*;
use sessions::SessionMap;

/// UI server 启动配置。
///
/// 配置加载（agents/models TOML 三层合并）是调用方的责任：CLI 用
/// `commands::config_layer::load`，编辑器用自己的加载路径，然后把结果
/// 塞进 `agent_config` / `model_resolver`。
pub struct UiServerConfig {
    /// 监听地址。CLI 传 `0.0.0.0:4567`；编辑器内嵌传 `127.0.0.1:0`
    /// （随机端口，实际端口从 [`UiServerHandle::addr`] 取）。
    pub bind: SocketAddr,
    /// 静态前端目录（vite build 产物 `dist/`）。`Some` 但路径不存在 →
    /// 不挂静态路由；`None` → 走内置候选逻辑（workspace 内
    /// `latte-agent-cli/ui/dist`，见 [`resolve_static_dir`]）。
    pub static_dir: Option<PathBuf>,
    /// 合并后的 agent 配置（roles + models）。
    pub agent_config: AgentConfig,
    /// 与 `agent_config` 配套的 model resolver。
    pub model_resolver: ModelResolver,
    /// 初始角色 id（单角色模式），`None` → `"manager"`。
    pub role: Option<String>,
    /// 初始模型 tier；`None` → 从初始角色的 `model_tier` 模板推导。
    pub tier: Option<ModelTier>,
    /// 固定模型 id（覆盖 tier 解析），同 `chat --model-id`。
    pub model_id: Option<String>,
    /// agents 配置路径原值（文件或目录），角色编辑器
    /// （POST /api/roles/config）保存时用它定位 `.latte/agents.d/<id>.toml`。
    pub agents_config: String,
}

/// 运行中的 server 句柄。drop 不会停 server——显式调
/// [`UiServerHandle::shutdown`] 或让 [`UiServerHandle::wait`] 跑到进程结束。
pub struct UiServerHandle {
    /// 实际监听地址（`bind` 端口为 0 时是 OS 分配的真实端口）。
    pub addr: SocketAddr,
    /// 实际生效的静态目录（`None` → 只有 API 路由，没有前端产物）。
    pub static_dir: Option<PathBuf>,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    server_task: tokio::task::JoinHandle<()>,
}

impl UiServerHandle {
    /// 阻塞到 server 退出（CLI 场景：一直运行直到进程被杀）。
    pub async fn wait(self) {
        let _ = self.server_task.await;
    }

    /// 触发优雅停机并等待 server 退出。
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let _ = self.server_task.await;
    }
}

/// 起 UI server：bind 后立即返回，实际服务在后台 tokio task 里跑。
///
/// 流程同原 `UiCmd::run` 的 axum 部分：决定初始 role/tier → 建
/// per-tab SessionMap + 默认 session → 解析静态目录 → build_router →
/// bind → serve（带优雅停机）。
pub async fn spawn(config: UiServerConfig) -> anyhow::Result<UiServerHandle> {
    let UiServerConfig {
        bind,
        static_dir,
        agent_config,
        model_resolver,
        role,
        tier,
        model_id,
        agents_config,
    } = config;

    let merged: Arc<parking_lot::RwLock<AgentConfig>> =
        Arc::new(parking_lot::RwLock::new(agent_config));
    let resolver: Arc<ModelResolver> = Arc::new(model_resolver);

    // 决定初始 role 和 tier。
    let initial_role = role.unwrap_or_else(|| "manager".to_string());
    let initial_tier: Option<ModelTier> = tier.or_else(|| {
        merged
            .read()
            .roles
            .get(&initial_role)
            .map(|tpl| ModelTier::parse(&tpl.model_tier).unwrap_or(ModelTier::Standard))
    });

    // ControllerConfig 保持单角色模式（原 UiCmd::run 的关键洞察）：
    // chat 命令本质就是单角色（manager），让 manager 通过 `delegate`
    // 工具去调其他 specialist；用户输入只到 manager 一人手里。
    let cwd = std::env::current_dir()?;

    // Build a per-tab session map. Each browser tab will get its
    // own ChatController on first POST /api/sessions so that
    // concurrent tabs do not see each other's events.
    //
    // We also bootstrap ONE default session tied to the launch
    // time, so old single-tab clients that don't yet POST
    // /api/sessions can still /api/session?id=<default> and route
    // traffic.
    let sessions: Arc<SessionMap> =
        Arc::new(parking_lot::RwLock::new(std::collections::HashMap::new()));
    let default_session_id =
        format!("ui-{}-{}", std::process::id(), unix_ts_millis());
    // Process-wide subsession store (used before sessions).
    let subsession_store: Arc<latte_agent_core::subsession::SubsessionStore> =
        Arc::new(latte_agent_core::subsession::SubsessionStore::new());
    let default_handle = sessions::create_session_handle(
        default_session_id.clone(),
        &initial_role,
        &merged,
        &resolver,
        &cwd,
        model_id,
        initial_tier,
        256,
        &subsession_store,
    )
    .await
    .map_err(|e| anyhow::anyhow!("spawn default session controller: {e}"))?;

    // Also bootstrap a default session so early HTTP calls that
    // don't create their own still work.
    sessions
        .write()
        .insert(default_session_id.clone(), Arc::new(default_handle));
    let static_dir = resolve_static_dir(static_dir);

    let state = AppState {
        sessions,
        merged,
        resolver,
        cwd,
        initial_role,
        static_dir: static_dir.clone(),
        self_loop: Arc::new(self_loop::SelfLoopState::default()),
        subsession_store,
        agents_config,
    };
    let app = build_router(state);

    // 先 bind 再返回：端口 0 时把 OS 分配的真实端口带给调用方。
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let addr = listener.local_addr()?;
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server_task = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await;
    });

    Ok(UiServerHandle {
        addr,
        static_dir,
        shutdown_tx: Some(shutdown_tx),
        server_task,
    })
}

// ─── State ────────────────────────────────────────────────────────

#[derive(Clone)]
pub(crate) struct AppState {
    /// One entry per browser tab. Each `SessionHandle` owns its own
    /// ChatController + ChatEvent broadcast channel, so chats across
    /// tabs don't pollute each other.
    sessions: Arc<SessionMap>,
    /// RwLock：角色编辑器（POST /api/roles/config）保存后直接改写内存
    /// 配置，新 session 立即用新配置，无需重启 server。
    merged: Arc<parking_lot::RwLock<AgentConfig>>,
    resolver: Arc<ModelResolver>,
    cwd: PathBuf,
    initial_role: String,
    static_dir: Option<PathBuf>,
    self_loop: Arc<self_loop::SelfLoopState>,
    /// Process-wide store of per-task subsession event logs. Each
    /// delegate call allocates an entry; the UI's right-click →
    /// "show contents" reads from this same store via
    /// `/api/sessions/{id}/subsessions/{sub_id}`. Shared across all
    /// tabs so a tab-A subsession can never accidentally read
    /// tab-B's events (the (session_id, sub_id) key keeps them apart
    /// even when the store is process-wide).
    subsession_store: Arc<latte_agent_core::subsession::SubsessionStore>,
    /// agents 配置路径原值（文件或目录），角色编辑器保存时
    /// 用它定位 `.latte/agents.d/<id>.toml`。
    agents_config: String,
}

pub(crate) fn unix_ts_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn build_router(state: AppState) -> Router {
    // /health 在根路径（liveness probe），其它走 /api
    //
    // Multi-session: tabs call POST /api/sessions on first mount to
    // create their own session_id; thereafter every chat/event call
    // carries `session_id` so the server stays tab-scoped.
    let api = Router::new()
        .route(
            "/sessions",
            get(list_sessions).post(create_session).delete(delete_session),
        )
        .route("/session", get(get_session))
        .route("/session/label", post(set_session_label))
        .route("/session/history", get(get_session_history))
        .route("/roles", get(list_roles))
        .route(
            "/roles/config",
            get(get_roles_config).post(save_role_config),
        )
        .route("/chat/send", post(chat_send))
        .route("/chat/command", post(chat_command))
        .route("/chat/role", post(switch_role))
        .route("/events", get(events_sse))
        .route("/traces", get(list_traces))
        .route("/traces/:session_id", get(read_trace))
        .route("/self-loop/start", post(self_loop::self_loop_start))
        .route("/self-loop/events", get(self_loop::self_loop_events_sse))
        .route("/self-loop/stop", post(self_loop::self_loop_stop))
        .route("/role-graph", get(role_graph_get))
        .route("/subsessions", get(get_subsession))
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

/// 决定静态前端目录。
///
/// 显式 `Some(dir)`：存在则用，不存在则不挂静态路由（语义同原
/// `UiCmd --static-dir`）。`None` 时按候选列表探测：
///   1. `<workspace 根>/latte-agent-cli/ui/dist`（以本 crate 的
///      `CARGO_MANIFEST_DIR` 向上一级锚定，不依赖调用方 cwd）；
///   2. 相对当前 cwd 的 `latte-agent-cli/ui/dist`。
///
/// 编辑器内嵌应显式传 `Some(dist 路径)`，不要依赖候选逻辑。
fn resolve_static_dir(explicit: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return if p.exists() { Some(p) } else { None };
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut candidates = Vec::with_capacity(2);
    if let Some(ws_root) = manifest_dir.parent() {
        candidates.push(ws_root.join("latte-agent-cli").join("ui").join("dist"));
    }
    candidates.push(PathBuf::from("latte-agent-cli/ui/dist"));
    candidates.into_iter().find(|p| p.exists())
}

// ─── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_static_dir_handles_missing() {
        // 显式指定不存在的路径 → None（不挂静态路由，也不回退候选）。
        assert!(resolve_static_dir(Some(PathBuf::from(
            "/nonexistent/path/1234"
        )))
        .is_none());
    }

    /// 端口 0 语义 + 端到端冒烟：`bind: 127.0.0.1:0` 时
    /// `UiServerHandle::addr` 必须是 OS 分配的真实端口，且
    /// `/health`、`/api/sessions`、`/api/roles`、`/api/traces`
    /// 都正常响应。编辑器内嵌（阶段 0）依赖这个行为。
    #[tokio::test]
    async fn spawn_binds_ephemeral_port_and_serves() {
        let agent_config = AgentConfig::default();
        let resolver = ModelResolver::from_config(&agent_config).expect("resolver");
        let handle = spawn(UiServerConfig {
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            static_dir: Some(PathBuf::from("/nonexistent/path/1234")),
            agent_config,
            model_resolver: resolver,
            role: None,
            tier: None,
            model_id: None,
            agents_config: ".latte/agents.d".into(),
        })
        .await
        .expect("spawn");

        // 端口 0 → 真实端口。
        assert_ne!(handle.addr.port(), 0, "port 0 must resolve to a real port");
        assert_eq!(handle.addr.ip(), std::net::IpAddr::from([127, 0, 0, 1]));
        // 显式给了不存在的 static_dir → 不挂静态路由。
        assert!(handle.static_dir.is_none());
        let port = handle.addr.port();

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

        // /api/sessions（spawn 自带一个默认 session）
        let (status, body) = http_get(&format!("http://127.0.0.1:{}/api/sessions", port)).await;
        assert_eq!(status, 200, "/api/sessions body: {}", body);
        let v: serde_json::Value = serde_json::from_str(&body).expect("sessions json");
        assert_eq!(v.as_array().map(|a| a.len()), Some(1));

        // /api/roles
        let (status, body) = http_get(&format!("http://127.0.0.1:{}/api/roles", port)).await;
        assert_eq!(status, 200, "/api/roles body: {}", body);
        let v: serde_json::Value = serde_json::from_str(&body).expect("roles json");
        assert!(v.is_array());

        // /api/traces (没有 traces 目录时返回空数组而不是 500)
        let (status, _) = http_get(&format!("http://127.0.0.1:{}/api/traces", port)).await;
        assert_eq!(status, 200);

        handle.shutdown().await;
    }
}
