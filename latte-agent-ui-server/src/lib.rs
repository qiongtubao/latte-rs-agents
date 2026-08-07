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
//!     // Tauri 内嵌：注入编辑器工作区根，agent 的工具调用（read/write/bash）
//!     // 与 prompt_file 等相对路径都相对它解析；CLI 传 None（= 进程 cwd）。
//!     cwd: Some(std::path::PathBuf::from("/path/to/user/workspace")),
//!     agents_config: ".latte/agents.d".into(),
//! })
//! .await?;
//! println!("UI at http://{}/", handle.addr);
//! # Ok(())
//! # }
//! ```

pub mod api;
mod handlers;
pub mod role_graph;
mod self_loop;
mod sessions;
mod models;
pub mod tasks;
pub mod tools;
pub mod workflows;
mod test;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::routing::{get, post, put};
use axum::Router;
use latte_agent_core::config::AgentConfig;
use latte_agent_core::model_resolver::{ModelResolver, ModelTier};

use handlers::*;
use sessions::SessionMap;

/// [`UiBackend`] 的构造配置（= [`UiServerConfig`] 去掉 `bind` /
/// `static_dir` 这两个 HTTP 关切）。配置加载（agents/models TOML 三层
/// 合并）是调用方的责任：CLI 用 `commands::config_layer::load`，编辑
/// 器用 `controller_runtime::load_cli_like_agent_config`。
#[derive(Clone)]
pub struct UiBackendConfig {
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
    /// agent 工作目录：每个 session 的 `ControllerConfig.cwd`（工具调用、
    /// `prompt_file` 等相对路径都相对它解析），也用于 role graph 与角色
    /// 编辑器读 prompt。`None` = 进程当前目录（CLI 现状）；Tauri 内嵌
    /// 必须显式传用户工作区根，否则 agent 会落到 `app_data_dir` 之类
    /// 的宿主进程 cwd。
    pub cwd: Option<PathBuf>,
    /// agents 配置路径原值（文件或目录），角色编辑器保存时用它定位
    /// `.latte/agents.d/<id>.toml`。
    pub agents_config: String,
}

/// 一个工作区一份的 UI 后端容器：per-tab SessionMap（每 session 一个
/// ChatController + event_log 环形缓冲）+ 共享配置/resolver/subsession
/// store/self-loop 状态。协议无关，HTTP server（[`spawn`]）与 Tauri
/// 适配器（latte-code-editor `chat_panel/ui_adapter.rs`）共用。
///
/// 编辑器的典型用法（按工作区 get-or-spawn，配合双重检查锁）：
///
/// ```text
/// map<workspace_root, Arc<UiBackend>>
/// get-or-spawn(key):
///   let (cfg, resolver) = load_cli_like_agent_config(root/.latte/agents.d, ...)?;
///   let backend = UiBackend::new(UiBackendConfig { agent_config: cfg, model_resolver: resolver,
///                     role: None, tier: None, model_id: None,
///                     cwd: Some(root), agents_config: ".../agents.d".into() })?;
///   let sid = backend.bootstrap_default_session().await?;
///   // 事件转发：api::subscribe_session(&backend, &sid) → emit
/// ```
///
/// 之后所有操作走 [`crate::api`]（sessions/roles/chat/traces/
/// subsessions/role_graph/self_loop + 事件订阅）。
#[derive(Clone)]
pub struct UiBackend {
    /// One entry per browser tab. Each `SessionHandle` owns its own
    /// ChatController + ChatEvent broadcast channel, so chats across
    /// tabs don't pollute each other.
    pub(crate) sessions: Arc<SessionMap>,
    /// RwLock：角色编辑器（POST /api/roles/config）保存后直接改写内存
    /// 配置，新 session 立即用新配置，无需重启 server。
    pub(crate) merged: Arc<parking_lot::RwLock<AgentConfig>>,
    pub(crate) resolver: Arc<ModelResolver>,
    pub(crate) cwd: PathBuf,
    pub(crate) initial_role: String,
    pub(crate) initial_tier: Option<ModelTier>,
    pub(crate) primary_model_id: Option<String>,
    pub(crate) self_loop: Arc<self_loop::SelfLoopState>,
    /// Workflow 测试运行的全局状态（一个 backend 同时只允许一个 run）；
    /// HTTP 走 `/api/workflows/run*`，事件走 SSE。
    pub(crate) workflow_run: Arc<workflows::WorkflowRunState>,
    /// Process-wide store of per-task subsession event logs. Each
    /// delegate call allocates an entry; the UI's right-click →
    /// "show contents" reads from this same store via
    /// `/api/subsessions?id=<sub_id>`. Shared across all tabs so a
    /// tab-A subsession can never accidentally read tab-B's events
    /// (the (session_id, sub_id) key keeps them apart even when the
    /// store is process-wide).
    ///
    /// 两层存储：内存（活读）+ 磁盘（`with_persistence` 启用）。磁盘文件
    /// 落 `<cwd>/.latte/ui-sessions/<sid>/<sub_id>.jsonl`，主 session 删
    /// 时联删；服务器重启后磁盘回查仍能看到历史 subagent 过程（排查用）。
    pub(crate) subsession_store: Arc<latte_agent_core::subsession::SubsessionStore>,
    /// agents 配置路径原值（文件或目录），角色编辑器保存时
    /// 用它定位 `.latte/agents.d/<id>.toml`。
    pub(crate) agents_config: String,
    /// 任务看板：`<cwd>/.latte/tasks/` 的内存索引 + 原子写
    /// （见 [`crate::tasks`]）。HTTP 路由与 scheduler 共用。
    pub(crate) tasks: Arc<parking_lot::RwLock<tasks::TaskStore>>,
}

impl UiBackend {
    /// 构造容器：先扫描 `<cwd>/.latte/ui-sessions/` 恢复落盘 session
    /// （元数据 + event_log 载入，**不** spawn controller——首个
    /// chat_send/subscribe 懒 spawn）；不新建任何 session，新建走
    /// [`UiBackend::bootstrap_default_session`] / [`api::create_session`]。
    pub fn new(config: UiBackendConfig) -> anyhow::Result<Self> {
        let UiBackendConfig {
            agent_config,
            model_resolver,
            role,
            tier,
            model_id,
            cwd,
            agents_config,
        } = config;

        let merged: Arc<parking_lot::RwLock<AgentConfig>> =
            Arc::new(parking_lot::RwLock::new(agent_config));
        // 决定初始 role 和 tier。
        let initial_role = role.unwrap_or_else(|| "manager".to_string());
        let initial_tier: Option<ModelTier> = tier.or_else(|| {
            merged
                .read()
                .roles
                .get(&initial_role)
                .map(|tpl| ModelTier::parse(&tpl.model_tier).unwrap_or(ModelTier::Standard))
        });
        // cwd：None = 进程当前目录（CLI 现状）；Some(dir) = 调用方注入
        // 的工作区根（Tauri 内嵌场景）。
        let cwd = cwd.map(Ok).unwrap_or_else(std::env::current_dir)?;

        let backend = Self {
            sessions: Arc::new(parking_lot::RwLock::new(std::collections::HashMap::new())),
            merged,
            resolver: Arc::new(model_resolver),
            cwd: cwd.clone(),
            initial_role,
            initial_tier,
            primary_model_id: model_id,
            self_loop: Arc::new(self_loop::SelfLoopState::default()),
            workflow_run: Arc::new(workflows::WorkflowRunState::default()),
            // subagent 落盘用：与 ui-sessions/*.jsonl 共用 `<cwd>/.latte/ui-sessions`
            // 作为根，subagent 文件落 `<sid>/<sub_id>.jsonl`，主 session 删时联删。
            // with_persistence 启动时一次扫盘建 sub_id→session_id 索引。
            subsession_store: Arc::new(latte_agent_core::subsession::SubsessionStore::with_persistence(
                cwd.join(".latte").join("ui-sessions"),
            )),
            agents_config,
            tasks: Arc::new(parking_lot::RwLock::new(tasks::TaskStore::load(&cwd)?)),
        };
        // 清理上次残留的 `.latte/tmp/`（重启时确保不遗留空 session 文件）。
        sessions::clean_tmp(&cwd);

        // 恢复落盘的 ui-sessions（`<cwd>/.latte/ui-sessions/*.jsonl`）：
        // 元数据 + event_log 载入内存，list/get/history 立即可用；不
        // spawn controller（首个 chat_send/subscribe 懒 spawn）。
        let restore_base = sessions::SessionSpawnParams {
            merged: backend.merged.clone(),
            resolver: backend.resolver.clone(),
            cwd: backend.cwd.clone(),
            primary_model_id: None,
            initial_tier: None,
            subsession_store: backend.subsession_store.clone(),
            initial_history: Vec::new(),
        };
        for h in sessions::restore_sessions(&backend.cwd, &restore_base) {
            backend
                .sessions
                .write()
                .insert(h.session_id.clone(), Arc::new(h));
        }

        Ok(backend)
    }

    /// Bootstrap ONE default session tied to "launch" time, so old
    /// single-tab clients that don't yet create their own session can
    /// still get/history/subscribe with the default id. ControllerConfig
    /// 保持单角色模式（原 UiCmd::run 的关键洞察）：chat 命令本质就是
    /// 单角色（manager），由 manager 通过 `delegate` 工具去调其他
    /// specialist；用户输入只到 manager 一人手里。
    ///
    /// 返回新 session 的 id。失败（controller spawn 失败）时不留半成品。
    pub async fn bootstrap_default_session(&self) -> Result<String, String> {
        let session_id =
            format!("ui-{}-{}", std::process::id(), unix_ts_millis());
        let handle = sessions::create_session_handle(
            session_id.clone(),
            &self.initial_role,
            &self.merged,
            &self.resolver,
            &self.cwd,
            self.primary_model_id.clone(),
            self.initial_tier.clone(),
            &self.subsession_store,
        )
        .await?;
        self.sessions
            .write()
            .insert(session_id.clone(), Arc::new(handle));
        Ok(session_id)
    }
}

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
    /// agent 工作目录：每个 session 的 `ControllerConfig.cwd`（工具调用、
    /// `prompt_file` 等相对路径都相对它解析），也用于 `/api/role-graph`
    /// 与角色编辑器读 prompt。`None` = 进程当前目录（CLI 现状）；
    /// Tauri 内嵌必须显式传用户工作区根，否则 agent 会落到
    /// `app_data_dir` 之类的宿主进程 cwd。
    pub cwd: Option<PathBuf>,
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
/// per-tab SessionMap（不建默认 session，前端 `ensureSession` 会
/// 通过 `POST /api/sessions` 创建）→ 解析静态目录 → build_router →
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
        cwd,
        agents_config,
    } = config;

    let backend = UiBackend::new(UiBackendConfig {
        agent_config,
        model_resolver,
        role,
        tier,
        model_id,
        cwd,
        agents_config,
    })?;
    // 不再 bootstrap 默认 session——前端 `ensureSession` 会调用
    // `createSession` 创建唯一 session，避免每次页面加载多出来一个
    // 空白 session。

    // 在 bind HTTP 之前先完成工具枚举，确保服务启动后所有 HTTP 请求
    // 都能立即拿到完整的工具列表（而非 fallback）。枚举本身已是毫秒级
    // （dynamic_registrations 从 TreeSitter 全量建图改为文本扫描），
    // 阻塞 bind 无感；失败时仍有 fallback 兜底。
    match crate::tools::enumerate_inner().await {
        Ok(list) => {
            crate::tools::set_enumerate_cache(list.clone());
            eprintln!("[boot] tools enumeration: {} tools", list.len());
        }
        Err(e) => eprintln!("[boot] tools enumeration failed: {e}"),
    }

    let static_dir = resolve_static_dir(static_dir);

    let state = AppState {
        backend: Arc::new(backend),
        static_dir: static_dir.clone(),
    };
    let app = build_router(state.clone());

    // 任务看板 scheduler（§4）：启动时先立即扫一遍（补发关机期间
    // 错过的排期），之后每 5s 扫描到期任务并派发给 manager。
    tokio::spawn(tasks::scheduler_loop(state.backend.clone()));

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

/// axum 侧状态：协议无关逻辑全在 [`UiBackend`]（`crate::api` 直接操作
/// 它）；axum 只多出 `static_dir`（静态文件是 HTTP  serving 的关切，
/// Tauri 适配器没有也不需要）。
#[derive(Clone)]
pub(crate) struct AppState {
    backend: Arc<UiBackend>,
    static_dir: Option<PathBuf>,
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
        .route("/sessions/fork", post(fork_session))
        .route("/session", get(get_session))
        .route("/session/label", post(set_session_label))
        .route("/session/history", get(get_session_history))
        .route(
            "/roles/config",
            get(get_roles_config).post(save_role_config),
        )
        .route("/chat/send", post(chat_send))
        .route("/chat/command", post(chat_command))
        .route("/chat/role", post(switch_role))
        .route("/chat/cancel-turn", post(chat_cancel_turn))
        .route("/chat/abort", post(chat_abort))
        .route("/chat/pause", post(chat_pause))
        .route("/chat/resume", post(chat_resume))
        .route("/chat/pause-session", post(chat_pause_session))
        .route("/chat/resume-session", post(chat_resume_session))
        .route("/chat/resume-role", post(chat_resume_role))
        .route("/chat/stream-mode", post(chat_stream_mode))
        // SSE event stream for the main chat. UI subscribes via
        // `EventSource('/api/events?id=<session>')` and drains
        // events as they arrive.
        .route("/events", get(events_sse))
        .route("/traces", get(list_traces))
        .route("/self-loop/start", post(self_loop_start))
        .route("/self-loop/events", get(self_loop_events_sse))
        .route("/self-loop/stop", post(self_loop_stop))
        .route("/role-graph", get(role_graph_get))
        .route("/subsessions", get(get_subsession))
        // 模型管理面板：列出合并后的 catalog（项目层覆盖后）；
        // 数据源是 `UiBackend.merged`（已经走过 `config_layer::load`
        .route("/models", get(list_models).post(create_model))
        .route(
            "/models/:key",
            axum::routing::patch(update_model).delete(delete_model),
        )
        .route("/models/test", axum::routing::post(test_model))
        .route("/models/:key/capabilities", get(model_capabilities))
        // 模型 TOML 源文件编辑
        .route("/models/:key/toml", get(get_model_toml).put(put_model_toml))
        // 工具管理
        .route("/tools", get(list_tools))
        .route("/tools/test", axum::routing::post(test_tool))
        .route("/tools/:id/toggle", axum::routing::post(toggle_tool))
        // 角色管理（新建/删除/测试）
        .route("/roles", get(list_roles).post(create_role))
        .route("/roles/test", axum::routing::post(test_role))
        .route("/roles/:id", axum::routing::delete(delete_role))
        // 角色 TOML 源文件编辑
        .route("/roles/:id/toml", get(get_role_toml).put(put_role_toml))
        // 任务看板（docs/task-board-design.md §6）
        .route("/tasks", get(list_tasks).post(create_task))
        .route("/tasks/import", post(import_tasks))
        .route("/tasks/dispatch-ready", post(dispatch_ready))
        .route(
            "/tasks/:id",
            get(get_task)
                .patch(update_task)
                .delete(delete_task),
        )
        .route("/tasks/:id/dispatch", post(dispatch_task))
        .route("/tasks/:id/abort", post(abort_task))
        .route("/tasks/:id/report", post(report_task))
        // 工作流管理（编辑器 CRUD + 校验 + 测试运行）
        .route("/workflows", get(list_workflows_h).post(create_workflow_h))
        .route("/workflows/validate", post(validate_workflow_h))
        .route("/workflows/run", post(workflow_run_start_h))
        .route("/workflows/run/events", get(workflow_run_events_sse))
        .route("/workflows/run/stop", post(workflow_run_stop_h))
        .route(
            "/workflows/:name",
            get(get_workflow_h).put(update_workflow_h).delete(delete_workflow_h),
        )
        .route(
            "/workflows/:name/toml",
            get(get_workflow_toml_h).put(put_workflow_toml_h),
        )
        .route("/images", post(upload_image))
        .route("/images/:file", get(get_image));
    let mut app = Router::new()
        .route("/health", get(health))
        .nest("/api", api);
    if let Some(dir) = &state.static_dir {
        let serve = tower_http::services::ServeDir::new(dir);
        app = app.fallback_service(serve);
    }

    app.with_state(state)
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
    ///
    /// 同时验证 `UiServerConfig::cwd` 注入：放一个只在临时工作区根里
    /// 存在的 `prompts/manager.md`，`/api/roles/config` 读角色 prompt
    /// 走的是 `AppState.cwd`（= 注入的 cwd），命中即证明生效。
    #[tokio::test]
    async fn spawn_binds_ephemeral_port_and_serves() {
        // 注入的工作区根：agent 相对路径（prompt_file 等）应相对它解析。
        let ws_root = std::env::temp_dir().join(format!(
            "ui-server-cwd-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(ws_root.join("prompts")).expect("mkdir prompts");
        const MARKER: &str = "CUSTOM PROMPT FROM INJECTED CWD 注入工作区";
        std::fs::write(ws_root.join("prompts").join("manager.md"), MARKER)
            .expect("write prompt");

        let mut agent_config = AgentConfig::default();
        agent_config.roles.insert(
            "manager".to_string(),
            latte_agent_core::role::RoleTemplate {
                id: "manager".into(),
                name: "Manager".into(),
                category: "management".into(),
                model_tier: "standard".into(),
                model_chain: vec![],
                prompt_file: Some("prompts/manager.md".into()),
                temperature: None,
                tools: vec![],
                icon: "[m]".into(),
                skills: vec![],
            code_paths: vec![],
            },
        );
        let resolver = ModelResolver::from_config(&agent_config).expect("resolver");
        let handle = spawn(UiServerConfig {
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            static_dir: Some(PathBuf::from("/nonexistent/path/1234")),
            agent_config,
            model_resolver: resolver,
            role: None,
            tier: None,
            model_id: None,
            cwd: Some(ws_root.clone()),
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

        // /api/sessions（无默认 session，前端 `ensureSession` 才会创建）
        let (status, body) = http_get(&format!("http://127.0.0.1:{}/api/sessions", port)).await;
        assert_eq!(status, 200, "/api/sessions body: {}", body);
        let v: serde_json::Value = serde_json::from_str(&body).expect("sessions json");
        assert_eq!(v.as_array().map(|a| a.len()), Some(0));

        // /api/roles
        let (status, body) = http_get(&format!("http://127.0.0.1:{}/api/roles", port)).await;
        assert_eq!(status, 200, "/api/roles body: {}", body);
        let v: serde_json::Value = serde_json::from_str(&body).expect("roles json");
        assert!(v.is_array());

        // /api/traces (没有 traces 目录时返回空数组而不是 500)
        let (status, _) = http_get(&format!("http://127.0.0.1:{}/api/traces", port)).await;
        assert_eq!(status, 200);

        // /api/roles/config：角色 prompt 必须读自注入的 cwd
        // （<ws_root>/prompts/manager.md），证明 UiServerConfig::cwd 生效。
        let (status, body) =
            http_get(&format!("http://127.0.0.1:{}/api/roles/config", port)).await;
        assert_eq!(status, 200, "/api/roles/config body: {}", body);
        let v: serde_json::Value = serde_json::from_str(&body).expect("roles config json");
        let prompt = v["roles"]
            .as_array()
            .and_then(|roles| roles.iter().find(|r| r["id"] == "manager"))
            .and_then(|r| r["prompt"].as_str())
            .expect("manager entry with prompt");
        assert_eq!(prompt, MARKER, "prompt 应来自注入的 cwd，而非进程 cwd");

        handle.shutdown().await;
        let _ = std::fs::remove_dir_all(&ws_root);
    }

    /// ui-sessions 落盘端到端：backend A（tmp cwd）建 session、发一条
    /// 真实用户消息（模型指向死端口，turn 必失败但 UserMessage 已先
    /// 广播）→ drop A → 新 backend B（同 cwd）恢复：list 可见且带
    /// restored/label/preview，history 与 A 逐字节一致（含
    /// UserMessage）→ delete 后文件消失。
    #[tokio::test]
    async fn persistence_restore_list_history_label_and_delete() {
        use latte_agent_core::config::{AgentConfig, ModelCatalog, ModelDef};
        use latte_agent_core::role::RoleTemplate;

        let ws = std::env::temp_dir().join(format!(
            "ui-server-persist-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(&ws).expect("mkdir ws");

        // 一个"能 build_runner、但 turn 必失败"的配置：模型存在但
        // base_url 是死端口（连接立即被拒）。这样 UserMessage 与后续
        // Error 都会进 event_log 并落盘。
        let make_config = || AgentConfig {
            models: ModelCatalog {
                models: vec![ModelDef {
                    name: "dead-model".into(),
                    api: "openai".into(),
                    provider: "test".into(),
                    base_url: "http://127.0.0.1:1".into(),
                    api_key: "k".into(),
                    context_window: 32000,
                    max_tokens: 4096,
                    supports_thinking: false,
                    supports_vision: false,
                    supports_image_generation: false,
                    cost_per_million_input: None,
                    cost_per_million_output: None,
                    tier: Some("standard".into()),
                    timeout_secs: Some(2),
                }],
                tiers: None,
                role_tiers: None,
            },
            roles: [(
                "manager".to_string(),
                RoleTemplate {
                    id: "manager".into(),
                    name: "Manager".into(),
                    category: "planning".into(),
                    model_tier: "standard".into(),
                    model_chain: vec![],
                    prompt_file: None,
                    temperature: None,
                    tools: vec![],
                    icon: "[m]".into(),
                    skills: vec![],
            code_paths: vec![],
                },
            )]
            .into_iter()
            .collect(),
        };
        let make_backend = || {
            let agent_config = make_config();
            let resolver = ModelResolver::from_config(&agent_config).expect("resolver");
            UiBackend::new(UiBackendConfig {
                agent_config,
                model_resolver: resolver,
                role: None,
                tier: None,
                model_id: None,
                cwd: Some(ws.clone()),
                agents_config: ".latte/agents.d".into(),
            })
        };

        // ── 进程 A：建 session → chat_send → set_label ──
        let backend_a = make_backend().expect("backend A");
        let info = crate::api::create_session(&backend_a)
            .await
            .expect("create session");
        let sid = info.session_id.clone();
        crate::api::chat_send(&backend_a, Some(&sid), "你好，持久化")
            .await
            .expect("chat send");
        // 等 turn 结束（Error 事件落进 event_log）且内容稳定。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let history_a = loop {
            let h = crate::api::session_history(&backend_a, &sid).expect("history poll");
            let has_user = h.iter().any(|v| v["type"] == "UserMessage");
            let has_turn_end = h
                .iter()
                .any(|v| v["type"] == "Error" || v["type"] == "RoleTurn");
            if has_user && has_turn_end {
                // 再稳一拍，等 archiver tee 完落盘。
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                let h2 = crate::api::session_history(&backend_a, &sid).expect("history settle");
                if h2.len() == h.len() {
                    break h2;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for turn events: {h:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };
        crate::api::set_session_label(&backend_a, &sid, "测试会话").expect("set label");
        // label 覆盖行也是异步无关的同步写，这里已是落盘状态。
        let file = ws
            .join(".latte")
            .join("ui-sessions")
            .join(format!("{sid}.jsonl"));
        assert!(file.exists(), "session 文件应已落盘: {}", file.display());
        drop(backend_a); // 模拟进程退出

        // ── 进程 B：同 cwd 新 backend → 恢复 ──
        let backend_b = make_backend().expect("backend B");
        let list = crate::api::list_sessions(&backend_b);
        assert_eq!(list.len(), 1, "恢复后应只有这一个 session");
        assert_eq!(list[0].session_id, sid);
        assert!(list[0].restored, "恢复的 session 必须带 restored 标记");
        assert_eq!(list[0].label.as_deref(), Some("测试会话"), "label 应保留");
        assert_eq!(
            list[0].preview, "你好，持久化",
            "preview 应从第一条 UserMessage 推导"
        );
        let history_b = crate::api::session_history(&backend_b, &sid).expect("history B");
        // `Prompt`/`SessionInfo` 是初始化事件，发文消息的空 session 不
        // 落盘。恢复后 history_b 不含 init 事件；history_a 含（内存全量）。
        // 断言：过滤掉 init 事件后两侧一致。
        let history_a_no_init: Vec<&serde_json::Value> = history_a
            .iter()
            .filter(|v| v["type"] != "Prompt" && v["type"] != "SessionInfo")
            .collect();
        let history_b_refs: Vec<&serde_json::Value> = history_b.iter().collect();
        assert_eq!(
            serde_json::to_string(&history_a_no_init).unwrap(),
            serde_json::to_string(&history_b_refs).unwrap(),
            "history 恢复后（不含 init 事件）必须逐字节一致"
        );
        assert!(
            history_b
                .iter()
                .any(|v| v["type"] == "UserMessage" && v["text"] == "你好，持久化"),
            "回放里必须含 UserMessage: {history_b:?}"
        );

        // ── delete → 文件消失 ──
        crate::api::delete_session(&backend_b, &sid)
            .await
            .expect("delete");
        assert!(crate::api::list_sessions(&backend_b).is_empty());
        assert!(!file.exists(), "delete 后落盘文件应删除");
        let _ = std::fs::remove_dir_all(&ws);
    }

    /// subagent 落盘跨 backend 重启：backend A create 一个 subagent
    /// 事件，drop A；新 backend B（同 cwd）启动后通过 `get_subsession`
    /// API 仍能拿到事件（磁盘回查路径）。
    ///
    /// 直接用 `subsession_store.create` 写事件，不走真实 chat 流——
    /// 这条路径只验证"落盘 → 重启 → 读回"三步端到端正确，模型/LLM
    /// 与它无关。
    #[tokio::test]
    async fn subsession_persists_across_backend_restart() {
        use latte_agent_core::subsession::SubsessionStore;
        use latte_agent_core::trace::{ToolStatus, TraceEvent, TraceMeta, TraceSink};

        let ws = std::env::temp_dir().join(format!(
            "ui-server-subs-persist-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(&ws).expect("mkdir ws");

        let make_backend = || {
            let resolver = ModelResolver::from_config(
                &latte_agent_core::config::AgentConfig::default(),
            )
            .expect("resolver");
            UiBackend::new(UiBackendConfig {
                agent_config: latte_agent_core::config::AgentConfig::default(),
                model_resolver: resolver,
                role: None,
                tier: None,
                model_id: None,
                cwd: Some(ws.clone()),
                agents_config: ".latte/agents.d".into(),
            })
            .expect("backend")
        };

        // 进程 A：建一个 subagent，发 3 个事件
        let backend_a = make_backend();
        let (sub_id, sink) = backend_a
            .subsession_store
            .create("sid-A", "programmer");
        for i in 0..3 {
            sink.emit(TraceEvent::ToolExec {
                meta: TraceMeta::now(i, "programmer", "sid-A"),
                name: "file.read".into(),
                args_json: "{}".into(),
                latency_ms: 3,
                status: ToolStatus::Ok("ok".into()),
            });
        }
        drop(sink);
        // 文件在 cwd/.latte/ui-sessions/sid-A/<sub_id>.jsonl
        let file = ws
            .join(".latte")
            .join("ui-sessions")
            .join("sid-A")
            .join(format!("{sub_id}.jsonl"));
        assert!(file.exists(), "subagent 落盘文件应存在: {}", file.display());
        drop(backend_a);

        // 进程 B：重启模拟，get_subsession API 应能拿回事件
        let backend_b = make_backend();
        // 内存空（重启后 in-memory cache 也清）
        assert_eq!(backend_b.subsession_store.len(), 0);
        // 但磁盘索引已建
        assert_eq!(backend_b.subsession_store.persisted_index_size(), 1);
        // API 路径读（先内存 miss → 回查磁盘）
        let events = crate::api::get_subsession(&backend_b, &sub_id, 0);
        assert_eq!(events.len(), 3, "重启后从磁盘读出 3 个事件");
        for (i, ev) in events.iter().enumerate() {
            assert_eq!(
                ev["ToolExec"]["meta"]["turn"].as_u64(),
                Some(i as u64),
                "事件内容应完整保序"
            );
        }

        // 直接调 store 也能读（绕过 API）
        let direct = backend_b
            .subsession_store
            .read_persisted(&sub_id)
            .expect("read_persisted");
        assert_eq!(direct.len(), 3);

        let _ = std::fs::remove_dir_all(&ws);
    }

    /// 删主 session 联删 subagent 落盘：建一个真 session（进
    /// SessionMap）+ 它的 subagent（落盘），调 `delete_session`，
    /// 验证 subagent 目录被清。
    #[tokio::test]
    async fn delete_session_cascades_to_subsessions() {
        use latte_agent_core::trace::{ToolStatus, TraceEvent, TraceMeta, TraceSink};

        let ws = std::env::temp_dir().join(format!(
            "ui-server-subs-delete-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(&ws).expect("mkdir ws");

        let resolver = ModelResolver::from_config(
            &latte_agent_core::config::AgentConfig::default(),
        )
        .expect("resolver");
        let backend = UiBackend::new(UiBackendConfig {
            agent_config: latte_agent_core::config::AgentConfig::default(),
            model_resolver: resolver,
            role: None,
            tier: None,
            model_id: None,
            cwd: Some(ws.clone()),
            agents_config: ".latte/agents.d".into(),
        })
        .expect("backend");


        // 建一个真 session（进 SessionMap，delete 不会 404）
        let info = crate::api::create_session(&backend).await.expect("create session");
        let sid = info.session_id;

        // 给它两个 subagent
        let (sub_a, sink_a) = backend.subsession_store.create(&sid, "programmer");
        sink_a.emit(TraceEvent::ToolExec {
            meta: TraceMeta::now(0, "programmer", &sid),
            name: "file.read".into(),
            args_json: "{}".into(),
            latency_ms: 1,
            status: ToolStatus::Ok("ok".into()),
        });
        let (sub_b, sink_b) = backend.subsession_store.create(&sid, "reviewer");
        sink_b.emit(TraceEvent::ToolExec {
            meta: TraceMeta::now(0, "reviewer", &sid),
            name: "file.read".into(),
            args_json: "{}".into(),
            latency_ms: 1,
            status: ToolStatus::Ok("ok".into()),
        });
        drop(sink_a);
        drop(sink_b);

        // 落盘文件应都在
        let session_dir = ws.join(".latte").join("ui-sessions").join(&sid);
        assert!(session_dir.exists(), "session 目录应已建: {}", session_dir.display());
        assert!(session_dir.join(format!("{sub_a}.jsonl")).exists());
        assert!(session_dir.join(format!("{sub_b}.jsonl")).exists());
        // 2 个手动创建的 subagent + 1 个 advisor monitor 的 subsession
        // （spawn_controller 里 advisor engine 自动创建的）。
        assert!(backend.subsession_store.persisted_index_size() >= 2);

        // 删主 session
        crate::api::delete_session(&backend, &sid).await.expect("delete");

        // 联删：session 整目录 + 索引 + 内存 entry 全清
        assert!(!session_dir.exists(), "session 目录应被联删");
        assert_eq!(backend.subsession_store.persisted_index_size(), 0);
        assert_eq!(backend.subsession_store.len(), 0);
        assert!(backend.subsession_store.read_persisted(&sub_a).is_none());
        assert!(backend.subsession_store.read_persisted(&sub_b).is_none());

        let _ = std::fs::remove_dir_all(&ws);
    }
}
