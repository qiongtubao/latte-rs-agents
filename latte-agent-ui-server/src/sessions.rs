//! Per-tab session 状态：每个浏览器 tab 一个 `ChatController`，
//! 互不串事件。包含 event_log 环形缓冲（切 tab 回来时的 replay）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use latte_agent_core::advisor_monitor::{
    AdvisorMonitor, AdvisorMonitorConfig, AdvisorReviewEngine,
};
use latte_agent_core::config::AgentConfig;
use latte_agent_core::controller::{ChatController, ControllerConfig};
use latte_agent_core::event_json::chat_event_to_frontend_json;
use latte_agent_core::model_resolver::{ModelResolver, ModelTier};
use latte_ai::params::GenerateParams;

/// Per-tab state. Holds the controller and the broadcast channel the
/// SSE stream subscribes to. Two handles never share a controller, so
/// `/api/chat/send` on tab A is invisible to tab B's SSE stream.
pub(crate) struct SessionHandle {
    pub(crate) session_id: String,
    /// controller that runs the actual chat loop and owns history.
    pub(crate) controller: Arc<ChatController>,
    /// Preview copy of the first user message — used by `/api/sessions`
    /// to render a sidebar entry without re-reading the controller.
    pub(crate) first_user_msg: parking_lot::Mutex<Option<String>>,
    /// User-assigned display name (via POST /api/session/label). When
    /// set it takes precedence over `first_user_msg` in the sidebar.
    pub(crate) label: parking_lot::Mutex<Option<String>>,
    /// Frontend-JSON `ChatEvent`s captured for this session. `GET
    /// /api/session/history` replays these so the chat panel restores
    /// its content when the user switches back to this session.
    pub(crate) event_log: Arc<parking_lot::RwLock<Vec<String>>>,
    pub(crate) created_at: Instant,
    pub(crate) last_activity: Arc<parking_lot::Mutex<Instant>>,
    pub(crate) initial_role: String,
}

impl SessionHandle {
    pub(crate) fn touch(&self) {
        *self.last_activity.lock() = Instant::now();
    }
}

/// Process-wide map of active session IDs to their per-tab handle.
/// `parking_lot::RwLock` because reads (every chat/SSE request) dominate.
pub(crate) type SessionMap = parking_lot::RwLock<HashMap<String, Arc<SessionHandle>>>;

/// Construct a fresh `SessionHandle` whose controller is already
/// `spawn`ed and owns its own broadcast channel. The caller inserts
/// the handle into `SessionMap`; failure to spawn the controller is
/// propagated to the HTTP caller via a 500.
pub(crate) async fn create_session_handle(
    session_id: String,
    initial_role: &str,
    merged: &Arc<parking_lot::RwLock<AgentConfig>>,
    resolver: &Arc<ModelResolver>,
    cwd: &std::path::Path,
    primary_model_id: Option<String>,
    initial_tier: Option<ModelTier>,
    broadcast_capacity: usize,
    // Process-wide subsession store. The `ControllerConfig`
    // field is shared by reference; when the manager delegates
    // inside this controller, `register_delegate_tool` carves a
    // fresh `MemorySink` out of this store and emits the
    // specialist's full event log there for the UI to fetch.
    subsession_store: &Arc<latte_agent_core::subsession::SubsessionStore>,
) -> Result<SessionHandle, String> {
    let agent_config_snapshot = Arc::new(merged.read().clone());
    let default_params = GenerateParams::default();
    let advisor_monitor_cfg = AdvisorMonitorConfig::default();
    let cfg = ControllerConfig {
        task_id: None,
        roles: vec![initial_role.to_string()],
        initial_prompt: None,
        max_rounds: 0,
        session_token_budget: 0,
        // snapshot 一份当前配置：session 固化创建时刻的配置，
        // 之后的角色编辑只影响新建的 session。
        agent_config: agent_config_snapshot.clone(),
        model_resolver: resolver.clone(),
        default_params: default_params.clone(),
        primary_model_id,
        initial_tier,
        initial_history: vec![],
        cwd: cwd.to_path_buf(),
        subsession_store: subsession_store.clone(),
        advisor_monitor: advisor_monitor_cfg.clone(),
    };
    let controller = Arc::new(ChatController::new(broadcast_capacity));
    // `spawn` returns a broadcast::Receiver (events consumer); the
    // controller runs in the background. We don't keep the receiver
    // here — the per-tab SSE subscriber is what reads events.
    let _rx = controller.spawn(cfg).await;
    // Advisor 监察者：旁路订阅该 session 的事件流，发现异常时经
    // controller 的 hint 队列纠偏（通道 A）并广播 🦉 气泡（通道 B）。
    // 任务 detach 与下方 archive 任务同生命周期：ChatEvent::Done 或
    // controller drop 后自动退出。
    if advisor_monitor_cfg.enabled {
        let engine = AdvisorReviewEngine::new(
            agent_config_snapshot,
            resolver.clone(),
            default_params,
        )
        .with_watchdog_notes(cwd.to_path_buf(), advisor_monitor_cfg.watchdog_notes);
        AdvisorMonitor::spawn(
            controller.clone(),
            advisor_monitor_cfg,
            engine,
            initial_role.to_string(),
        );
    }
    // Archive every ChatEvent as frontend-shaped JSON so a tab that
    // switches away and back can restore the chat contents. Bounded
    // to MAX_LOG entries (oldest dropped) to keep memory flat.
    let event_log: Arc<parking_lot::RwLock<Vec<String>>> =
        Arc::new(parking_lot::RwLock::new(Vec::new()));
    {
        let log = event_log.clone();
        let mut archive_rx = controller.subscribe();
        tokio::spawn(async move {
            const MAX_LOG: usize = 5000;
            while let Ok(ev) = archive_rx.recv().await {
                if let Ok(json) = chat_event_to_frontend_json(&ev) {
                    let mut g = log.write();
                    if g.len() >= MAX_LOG {
                        g.remove(0);
                    }
                    g.push(json);
                }
            }
        });
    }
    let now = Instant::now();
    Ok(SessionHandle {
        session_id,
        controller,
        first_user_msg: parking_lot::Mutex::new(None),
        label: parking_lot::Mutex::new(None),
        event_log,
        created_at: now,
        last_activity: Arc::new(parking_lot::Mutex::new(now)),
        initial_role: initial_role.to_string(),
    })
}
