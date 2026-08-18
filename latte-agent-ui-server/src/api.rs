//! 协议无关的 UI 后端 API（契约 C1 的 Rust 半区）。
//!
//! 这里集中了原 REST handlers 里与 HTTP 无关的全部逻辑：axum
//! （`crate::handlers`）只是参数提取 + 状态码映射的薄壳；Tauri 适配器
//! （latte-code-editor `chat_panel/ui_adapter.rs`）直接调用本模块，
//! 经 `ui_*` 命令与 `ui:chat_event` / `ui:self_loop_event` 事件驱动同
//! 一个前端。路由语义（含状态码）经 [`ApiError::status`] 保真。
//!
//! 所有函数都操作 [`crate::UiBackend`]（一个工作区一个容器）。

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use latte_agent_core::config::{AgentConfig, ConfigLayer, ModelDef};
use latte_agent_core::controller::{ChatEvent, RoleInfo};
use latte_agent_core::workflow::{load_checkpoint, load_workflow, run_workflow, run_workflow_resume, WorkflowRunContext};
use latte_ai::params::GenerateParams;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::role_graph::RoleGraph;
use crate::sessions::{create_forked_handle, create_session_handle, SessionHandle};
use crate::workflows::{self, ValidateResponse, WorkflowDetail, WorkflowForm, WorkflowSummary};
use crate::UiBackend;

pub use crate::self_loop::SelfLoopEvent;

// ─── Error ────────────────────────────────────────────────────────

/// 协议无关错误：`status` 保留 HTTP 语义（axum 壳映射回 StatusCode），
/// `message` 为人类可读原因。Tauri 侧一般只取 `message`。
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

impl ApiError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self { status: 400, message: message.into() }
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self { status: 404, message: message.into() }
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self { status: 500, message: message.into() }
    }
}

impl From<ApiError> for (axum::http::StatusCode, String) {
    fn from(e: ApiError) -> Self {
        (
            axum::http::StatusCode::from_u16(e.status)
                .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR),
            e.message,
        )
    }
}

// ─── Sessions ─────────────────────────────────────────────────────

/// `GET /api/session?id=...` 与 `POST /api/sessions` 的返回。
/// 前端把 `session_id` 当 localStorage key 用。
#[derive(Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub role: String,
    pub model: Option<String>,
    pub tier: String,
    /// All sessions the caller could switch to (sidebar).
    pub available_sessions: Vec<SessionSummary>,
    pub available_roles: Vec<RoleInfo>,
    /// true = 当前 session 处于用户按 ⏸ 的暂停状态。
    #[serde(default)]
    pub is_paused: bool,
}

/// 侧栏轻量条目 — `GET /api/sessions` 返回，也内嵌在 [`SessionInfo`]。
#[derive(Serialize)]
pub struct SessionSummary {
    pub session_id: String,
    /// First user message, truncated, used as the human label.
    pub preview: String,
    /// User-assigned display name (POST /api/session/label). `None`
    /// until the user renames the session.
    pub label: Option<String>,
    pub initial_role: String,
    pub created_at_unix_ms: u64,
    pub last_activity_unix_ms: u64,
    /// true = 本 session 是从 ui-sessions 落盘恢复的（可见历史早于
    /// 本次进程启动；agent 上下文从空开始，首个 chat/subscribe 时
    /// 懒 spawn controller）。
    pub restored: bool,
    /// true = 该 session 处于用户按 ⏸ 的暂停状态（未暂停 / controller
    /// 尚未 spawn / 侧栏未显示暂停 badge）。
    #[serde(default)]
    pub is_paused: bool,
}

pub fn list_sessions(b: &UiBackend) -> Vec<SessionSummary> {
    let map = b.sessions.read();
    let now = Instant::now();
    let mut out: Vec<SessionSummary> = map
        .values()
        .map(|h| SessionSummary {
            session_id: h.session_id.clone(),
            preview: h
                .first_user_msg
                .lock()
                .clone()
                .unwrap_or_else(|| "(no user message yet)".into()),
            label: h.label.lock().clone(),
            initial_role: h.initial_role.clone(),
            created_at_unix_ms: millis_from_now(now, h.created_at),
            last_activity_unix_ms: millis_from_now(now, *h.last_activity.lock()),
            restored: h.restored,
            is_paused: h.try_controller().map(|c| c.is_session_paused()).unwrap_or(false),
        })
        .collect();
    // last_activity_unix_ms 存的是"距上次活动的毫秒数"（age），越小越
    // 新。按 age 升序排，最近活跃的 session 排最前。
    out.sort_by(|a, b| a.last_activity_unix_ms.cmp(&b.last_activity_unix_ms));
    out
}

/// 分配一个新 session（含专属 ChatController）。调用方（HTTP 壳 /
/// Tauri 适配器）拿到返回后通常还要接事件转发（[`subscribe_session`]）。
pub async fn create_session(b: &UiBackend) -> Result<SessionInfo, ApiError> {
    // 批量派发等场景同一毫秒可能建多个 session：加进程内序号兜底唯一性。
    static SESSION_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let session_id = format!(
        "ui-{}-{}-{}",
        std::process::id(),
        crate::unix_ts_millis(),
        SESSION_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let handle = create_session_handle(
        session_id.clone(),
        &b.initial_role,
        &b.merged,
        &b.resolver,
        &b.cwd,
        None,
        None,
        &b.subsession_store,
    )
    .await
    .map_err(|e| ApiError::internal(format!("spawn controller: {e}")))?;
    let h = Arc::new(handle);
    let resp = SessionInfo {
        session_id: h.session_id.clone(),
        role: h.initial_role.clone(),
        model: None,
        tier: "auto".into(),
        available_sessions: vec![SessionSummary {
            session_id: h.session_id.clone(),
            preview: "(new)".into(),
            label: None,
            initial_role: h.initial_role.clone(),
            created_at_unix_ms: 0,
            last_activity_unix_ms: 0,
            restored: h.restored,
            is_paused: false,
        }],
        available_roles: build_role_info(&b.merged.read()),
        is_paused: false,
    };
    b.sessions.write().insert(h.session_id.clone(), h);
    Ok(resp)
}

/// `POST /api/sessions/fork` — 从某个 session 的对话某一点分叉出一个
/// 新 session。`events` 是源 session 可见历史的前缀（前端 JSON 的
/// ChatEvent，含被右键那条消息为止），既作为新 session 的可见历史
/// （写内存 event_log + 落盘），又用来重建 agent 上下文
/// （UserMessage → user、完成的 RoleTurn → assistant），让分叉出来的
/// 会话记得分叉点之前的讨论。用于"探索项目后分叉去做不同功能"。
pub async fn fork_session(
    b: &UiBackend,
    source_session_id: &str,
    events: Vec<serde_json::Value>,
) -> Result<SessionInfo, ApiError> {
    // 源 session 必须存在（拿 initial_role + label 派生）。
    let src = resolve_session(b, Some(source_session_id))?;
    let initial_role = src.initial_role.clone();

    // 可见历史：每个事件序列化成一行（与 event_log 落盘格式一致）。
    let event_lines: Vec<String> = events.iter().map(|v| v.to_string()).collect();

    // agent 上下文重建：只取用户/助手的文本回合（工具/委派/工作流
    // 事件对单角色续聊的上下文价值有限，从简）。
    let mut initial_history: Vec<latte_ai::models::Message> = Vec::new();
    for v in &events {
        match v.get("type").and_then(|t| t.as_str()) {
            Some("UserMessage") => {
                if let Some(t) = v.get("text").and_then(|x| x.as_str()) {
                    if !t.is_empty() {
                        initial_history.push(latte_ai::models::Message::user(t));
                    }
                }
            }
            Some("RoleTurn") => {
                let complete = v
                    .get("is_complete")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(true);
                if complete {
                    if let Some(c) = v.get("content").and_then(|x| x.as_str()) {
                        if !c.is_empty() {
                            initial_history.push(latte_ai::models::Message::assistant(c));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // 侧栏预览：第一条 UserMessage 的前 80 字符（与 chat_send 一致）。
    let first_user_msg = events.iter().find_map(|v| {
        if v.get("type").and_then(|t| t.as_str()) != Some("UserMessage") {
            return None;
        }
        let t = v.get("text").and_then(|x| x.as_str())?;
        Some(t.chars().take(80).collect::<String>())
    });

    // 标签：🍴 + 源 session 的标签/预览，方便在侧栏识别分叉。
    let src_base = src
        .label
        .lock()
        .clone()
        .or_else(|| src.first_user_msg.lock().clone())
        .unwrap_or_else(|| initial_role.clone());
    let label = Some(format!("🍴 {}", src_base.chars().take(40).collect::<String>()));

    let new_id = format!("ui-{}-{}", std::process::id(), crate::unix_ts_millis());
    let handle = create_forked_handle(
        new_id.clone(),
        &initial_role,
        &b.merged,
        &b.resolver,
        &b.cwd,
        &b.subsession_store,
        event_lines,
        initial_history,
        first_user_msg.clone(),
        label.clone(),
    )
    .await
    .map_err(|e| ApiError::internal(format!("fork spawn controller: {e}")))?;

    let h = Arc::new(handle);
    let resp = SessionInfo {
        session_id: h.session_id.clone(),
        role: h.initial_role.clone(),
        model: None,
        tier: "auto".into(),
        available_sessions: vec![SessionSummary {
            session_id: h.session_id.clone(),
            preview: first_user_msg.unwrap_or_default(),
            label,
            initial_role: h.initial_role.clone(),
            created_at_unix_ms: 0,
            last_activity_unix_ms: 0,
            restored: false,
            is_paused: false,
        }],
        available_roles: build_role_info(&b.merged.read()),
        is_paused: h.try_controller().map(|c| c.is_session_paused()).unwrap_or(false),
    };
    b.sessions.write().insert(h.session_id.clone(), h);
    Ok(resp)
}

/// Helper: turns `now - earlier` into a unix-millis duration, useful
fn millis_from_now(now: Instant, earlier: Instant) -> u64 {
    // `Duration` doesn't expose `.map`. Compute ms in one shot and
    // saturate on the unwrap (`Instant::duration_since` only fails
    // when `earlier > now` because the system clock jumped back).
    let d = now.saturating_duration_since(earlier);
    d.as_millis() as u64
}

/// Lookup the SessionHandle for a request, or return 404.
fn resolve_session(
    b: &UiBackend,
    session_id: Option<&str>,
) -> Result<Arc<SessionHandle>, ApiError> {
    let id = session_id.ok_or_else(|| ApiError::bad_request("missing session_id"))?;
    let map = b.sessions.read();
    map.get(id)
        .cloned()
        .ok_or_else(|| ApiError::not_found(format!("session_id {id:?} not found")))
}

pub fn get_session(b: &UiBackend, id: &str) -> Result<SessionInfo, ApiError> {
    let h: Arc<SessionHandle> = {
        let map = b.sessions.read();
        map.get(id).cloned()
    }
    .ok_or_else(|| ApiError::not_found(format!("session {id} unknown")))?;
    let label = h.label.lock().clone();
    Ok(SessionInfo {
        session_id: h.session_id.clone(),
        role: h.initial_role.clone(),
        model: None,
        tier: "auto".into(),
        available_sessions: vec![SessionSummary {
            session_id: h.session_id.clone(),
            preview: {
                let g = h.first_user_msg.lock();
                g.clone().unwrap_or_default()
            },
            label,
            initial_role: h.initial_role.clone(),
            created_at_unix_ms: millis_from_now(Instant::now(), h.created_at),
            last_activity_unix_ms: {
                let g = h.last_activity.lock();
                millis_from_now(Instant::now(), *g)
            },
            restored: h.restored,
            is_paused: h.try_controller().map(|c| c.is_session_paused()).unwrap_or(false),
        }],
        available_roles: build_role_info(&b.merged.read()),
        is_paused: h.try_controller().map(|c| c.is_session_paused()).unwrap_or(false),
    })
}

/// `GET /api/session/history?id=...` — 回放归档的 ChatEvent 前端 JSON
/// （event_log 环形缓冲），切 tab 回来时恢复聊天内容用。
pub fn session_history(b: &UiBackend, id: &str) -> Result<Vec<serde_json::Value>, ApiError> {
    let h: Arc<SessionHandle> = {
        let map = b.sessions.read();
        map.get(id).cloned()
    }
    .ok_or_else(|| ApiError::not_found(format!("session {id} unknown")))?;
    let events: Vec<serde_json::Value> = h
        .event_log
        .read()
        .iter()
        .filter_map(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .collect();
    Ok(events)
}

/// 删除 session：停掉它的 controller（恢复未激活的 no-op）并删
/// 落盘文件。UI 在删除当前活跃 session 之前会先切到别的 session
/// （或新建一个）。
pub async fn delete_session(b: &UiBackend, id: &str) -> Result<(), ApiError> {
    let removed = b.sessions.write().remove(id);
    match removed {
        Some(h) => {
            h.abort_if_spawned().await;
            h.delete_files();
            // 联删 subagent 落盘文件（与主 session 文件同生命周期）：
            // `<ui-sessions>/<sid>/` 整目录 rm + 索引清掉 + 内存
            // entries 清掉。最佳努力，错误已在 store 内部 warn。
            b.subsession_store.delete_for_session(id);
            Ok(())
        }
        None => Err(ApiError::not_found(format!("session {id} unknown"))),
    }
}

/// 重命名 session。空/纯空白 label 清除自定义名，回退到 preview。
/// 同时尾加一条 meta 覆盖行落盘（恢复时最后一条 meta 生效）。
pub fn set_session_label(b: &UiBackend, session_id: &str, label: &str) -> Result<(), ApiError> {
    let h = resolve_session(b, Some(session_id))?;
    let trimmed = label.trim();
    let new_label = if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.chars().take(60).collect())
    };
    h.set_label(new_label);
    h.touch();
    Ok(())
}

/// 订阅某 session 的 ChatEvent broadcast（只拿得到订阅之后的事件；
/// 恢复 session 在此时懒 spawn controller）。Tauri 适配器用它把事件
/// 转发成 `ui:chat_event`；HTTP 的 SSE handler 走的是同一通道。事件
/// 转前端 JSON 用
/// `latte_agent_core::event_json::chat_event_to_frontend_json`（契约 C2）。
pub async fn subscribe_session(
    b: &UiBackend,
    id: &str,
) -> Result<broadcast::Receiver<ChatEvent>, ApiError> {
    let h = resolve_session(b, Some(id))?;
    let controller = h
        .controller_or_spawn()
        .await
        .map_err(|e| ApiError::internal(format!("spawn controller: {e}")))?;
    Ok(controller.subscribe())
}

/// 拿 session controller 的事件 broadcast **sender**（懒 spawn 同
/// [`subscribe_session`]）。任务看板把绑定 workflow 的 run 直接跑在
/// session 的事件流上：`run_workflow` 的 WorkflowStarted/Step/Turn/
/// Finished 经它进入该 session 的 SSE 与 event_log 归档。
pub async fn session_event_sender(
    b: &UiBackend,
    id: &str,
) -> Result<broadcast::Sender<ChatEvent>, ApiError> {
    let h = resolve_session(b, Some(id))?;
    let controller = h
        .controller_or_spawn()
        .await
        .map_err(|e| ApiError::internal(format!("spawn controller: {e}")))?;
    Ok(controller.event_sender())
}

/// 拿 session controller 的 session-level 暂停门（`Arc<AgentPauseGate>`）。
/// 任务看板把绑定 workflow 的 run 也用**同一个** gate —— 用户 ⏸ 时
/// 看板派的 workflow 也一起停；▶ 一起恢复。
pub async fn session_pause_gate(
    b: &UiBackend,
    id: &str,
) -> Result<Arc<latte_agent_core::pause_gate::AgentPauseGate>, ApiError> {
    let h = resolve_session(b, Some(id))?;
    let controller = h
        .controller_or_spawn()
        .await
        .map_err(|e| ApiError::internal(format!("spawn controller: {e}")))?;
    Ok(controller.session_pause_gate())
}

/// 拿 session controller 的 advisor intervene 暂停门
/// （`AdvisorPauseGate`）。任务看板/续跑的 workflow 分派共用同一个
/// gate——advisor 判 Intervene「等待用户拍板」时，workflow 流水线
/// 也一起 park（此前只对 watched role 的主 runner 生效）。
pub async fn session_advisor_pause_gate(
    b: &UiBackend,
    id: &str,
) -> Result<latte_agent_core::advisor_monitor::AdvisorPauseGate, ApiError> {
    let h = resolve_session(b, Some(id))?;
    let controller = h
        .controller_or_spawn()
        .await
        .map_err(|e| ApiError::internal(format!("spawn controller: {e}")))?;
    Ok(controller.advisor_pause_gate())
}
// ─── Roles ────────────────────────────────────────────────────────

pub fn list_roles(b: &UiBackend) -> Vec<RoleInfo> {
    build_role_info(&b.merged.read())
}

pub fn build_role_info(cfg: &AgentConfig) -> Vec<RoleInfo> {
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

// ─── Role Config API（角色编辑器） ────────────────────────────────

/// `GET /api/roles/config` 返回的单个角色条目。
#[derive(Serialize)]
pub struct RoleConfigEntry {
    pub id: String,
    pub name: String,
    pub category: String,
    pub icon: String,
    pub model_tier: String,
    pub model_chain: Vec<String>,
    pub temperature: Option<f64>,
    pub tools: Vec<String>,
    pub skills: Vec<String>,
    /// 领域代码/文档路径（注入角色系统提示，见 core `RoleTemplate::code_paths`）。
    pub code_paths: Vec<String>,
    pub prompt_file: Option<String>,
    pub prompt_path: Option<String>,
    pub config_path: String,
    pub prompt: String,
}

/// `GET /api/roles/config` 同时返回的可用模型清单，给角色编辑器的
/// 「模型链」下拉框使用 —— 避免用户手敲 model id（容易打错或记错）。
///
/// 数据源：合并后的 `cfg.models.models`（已经走过项目 + 全局两层合并，
/// 与 `GET /api/models` 同源）。`source` 字段供 UI 在 option 标签里
/// 显示「项目 / 全局 / catalog」标签，让用户知道这条记录是否在磁盘上。
#[derive(Serialize, Clone, Debug)]
pub struct AvailableModel {
    /// 模型 id（同时是 API 请求里的 `model` 字段，也是 `model_chain`
    /// 里实际写入的字符串）。注意：后端 `ModelDef.name` 的别名是
    /// `model_name`，与此刻的 `name` 字段语义一致。
    pub name: String,
    /// 厂商标识，与 `name` 一起组成 `provider/name` 全键。
    pub provider: String,
    /// "project" / "global" / "catalog"：UI 显示用标签。
    pub source: String,
    /// 复合键 `provider/name`。同名多条（项目层 + 全局层各一个文件）
    /// 时 UI 靠 `key + source` 区分选项；写入 `model_chain` 的仍是
    /// `name`（协议限制：model_chain 只存 model id）。
    pub key: String,
}

#[derive(Serialize)]
pub struct RolesConfigResponse {
    pub roles: Vec<RoleConfigEntry>,
    pub available_tools: Vec<String>,
    /// 合并后的可用模型清单（按 provider 排序后按 name 排序），供
    /// 角色编辑器的「模型链」下拉框使用。
    pub available_models: Vec<AvailableModel>,
    pub tiers: Vec<String>,
    pub workspace_path: String,
    pub agents_config_path: String,
    pub sessions_path: String,
}

/// 全局配置根目录（`$LATTE_HOME` 或 `~/.latte`）。None = 无 HOME。
fn global_root_dir() -> Option<PathBuf> {
    latte_agent_core::global_config::GlobalConfig::global_dir()
}

/// 全局 agents.d 目录。
fn global_agents_dir() -> PathBuf {
    ConfigLayer::Global
        .agents_dir()
        .unwrap_or_else(|| global_root_dir().unwrap_or_default().join("agents.d"))
}

/// 文件存在且包含 `[roles.<role_id>]` 表（`roles = {}` 之类的空壳不算）。
fn role_file_has_section(path: &std::path::Path, role_id: &str) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|content| content.parse::<toml_edit::DocumentMut>().ok())
        .map(|doc| {
            doc.get("roles")
                .and_then(|r| r.get(role_id))
                .and_then(|r| r.as_table())
                .is_some()
        })
        .unwrap_or(false)
}

/// 角色 TOML 的有效路径（显示 / 读取 / 写入共用，保证三处一致）：
/// 1. 项目 agents 目录下的文件含 `[roles.<id>]` 节 → 项目路径；
/// 2. 否则 → 全局 `agents.d/<id>.toml`（文件可能尚不存在，此时即保存目标，
///    写入时创建）。
/// 返回 `(路径, 是否项目层)`。
fn resolve_role_toml_path(
    project_agents_dir: &std::path::Path,
    global_agents_dir: &std::path::Path,
    role_id: &str,
) -> (PathBuf, bool) {
    let project_path = project_agents_dir.join(format!("{role_id}.toml"));
    if role_file_has_section(&project_path, role_id) {
        return (project_path, true);
    }
    (global_agents_dir.join(format!("{role_id}.toml")), false)
}

/// 角色 prompt 的有效路径，镜像 runtime `RoleTemplate::resolve` 的读取顺序：
/// 1. `<cwd>/<prompt_file>` 存在 → 项目 prompt；
/// 2. `<global_root>/prompts.d/<basename>` 存在 → 全局覆盖；
/// 3. 都不存在 → 保存目标：项目角色走路径 1，全局角色走路径 2。
/// 绝对路径 / `~` 前缀 / 含 `..` 的路径不做全局回退（与 runtime 一致），
/// 直接按字面路径使用。
fn resolve_prompt_path(
    cwd: &std::path::Path,
    global_root: Option<&std::path::Path>,
    prompt_file: &str,
    project_role: bool,
) -> PathBuf {
    let direct = cwd.join(prompt_file);
    let skip_global = prompt_file.starts_with('/')
        || prompt_file.starts_with('~')
        || prompt_file.contains("..");
    if skip_global {
        return direct;
    }
    if direct.is_file() {
        return direct;
    }
    let global = global_root.and_then(|root| {
        std::path::Path::new(prompt_file)
            .file_name()
            .map(|b| root.join("prompts.d").join(b))
    });
    if let Some(g) = &global {
        if g.is_file() {
            return g.clone();
        }
    }
    match (project_role, global) {
        (true, _) => direct,
        (false, Some(g)) => g,
        (false, None) => direct,
    }
}

/// 读取角色当前 prompt，镜像 runtime 的解析顺序：`cwd.join(prompt_file)`
/// → 全局 `prompts.d/<basename>` → 内嵌兜底 prompt → 空串。
fn read_role_prompt(
    cwd: &std::path::Path,
    global_root: Option<&std::path::Path>,
    tpl: &latte_agent_core::role::RoleTemplate,
) -> String {
    if let Some(file) = &tpl.prompt_file {
        let skip_global =
            file.starts_with('/') || file.starts_with('~') || file.contains("..");
        if let Ok(content) = std::fs::read_to_string(cwd.join(file)) {
            return content;
        }
        if !skip_global {
            if let Some(basename) = std::path::Path::new(file).file_name() {
                if let Some(root) = global_root {
                    if let Ok(content) =
                        std::fs::read_to_string(root.join("prompts.d").join(basename))
                    {
                        return content;
                    }
                }
            }
        }
    }
    latte_agent_core::prompts::for_role(&tpl.id)
        .map(str::to_string)
        .unwrap_or_default()
}

fn role_config_entry(
    b: &UiBackend,
    tpl: &latte_agent_core::role::RoleTemplate,
) -> RoleConfigEntry {
    let project_dir = agents_config_dir(&b.cwd, &b.agents_config);
    let (toml_path, project_role) =
        resolve_role_toml_path(&project_dir, &global_agents_dir(), &tpl.id);
    let global_root = global_root_dir();
    let prompt_path = tpl.prompt_file.as_ref().map(|file| {
        resolve_prompt_path(&b.cwd, global_root.as_deref(), file, project_role)
            .display()
            .to_string()
    });
    RoleConfigEntry {
        id: tpl.id.clone(),
        name: tpl.name.clone(),
        category: tpl.category.clone(),
        icon: tpl.icon.clone(),
        model_tier: tpl.model_tier.clone(),
        model_chain: tpl.model_chain.clone(),
        temperature: tpl.temperature,
        tools: tpl.tools.clone(),
        skills: tpl.skills.clone(),
        code_paths: tpl.code_paths.clone(),
        prompt_file: tpl.prompt_file.clone(),
        prompt_path,
        config_path: toml_path.display().to_string(),
        prompt: read_role_prompt(&b.cwd, global_root.as_deref(), tpl),
    }
}

/// 枚举全部可用工具的 short name：注册全部 builtin packages 后取
/// `get_tool_names()` 的最后一段，去重排序，再补上 `delegate` 和
/// `workflow`（这两个由 controller 动态注册，不在 builtin packages 里）。
async fn enumerate_available_tools() -> Result<Vec<String>, ApiError> {
    use latte_rs_agent_tools::prelude::*;
    let mgr = create_tool_manager();
    for p in builtin_tool_packages() {
        mgr.register_package(p)
            .await
            .map_err(|e| ApiError::internal(format!("register_package: {e}")))?;
    }
    let mut names: std::collections::BTreeSet<String> = mgr
        .get_tool_names()
        .into_iter()
        .map(|n| n.rsplit('.').next().unwrap_or(&n).to_string())
        .collect();
    names.insert("delegate".to_string());
    names.insert("workflow".to_string());
    names.insert("generate_image".to_string());
    // code_graph 由 controller 按角色配置动态注册（controller.rs
    // code_graph_tool()），不在 builtin packages——不补进枚举的话角色
    // 编辑器永远看不到它，形成「没配上就不注册、不注册就配不上」的死锁。
    names.insert("code_graph".to_string());
    Ok(names.into_iter().collect())
}

/// 从磁盘重新加载角色分层配置，整体替换内存中的 roles。
///
/// 背景：merged 只在 server 启动时加载一次，而角色 TOML 可能被外部
/// 编辑器改动——角色编辑器「表单编辑」读内存、「源文件编辑」读磁盘，
/// 两边会不一致。每次 `GET /api/roles/config` 前调用本函数，让表单
/// 始终反映磁盘上的有效配置（之后新 session 也直接用新配置）。
///
/// 分层语义与 core `load_with_global` / `merge_global_into` 一致：
/// 项目层优先；全局层只贡献项目未声明的 id，并为已声明的角色补空字段
/// （`model_chain` / `icon` / `temperature`）。磁盘两层都没有任何角色
/// 时保持内存现状（嵌入式/测试注入的纯内存配置不被清空）。
fn reload_roles_from_disk(b: &UiBackend) {
    reload_roles_from_disk_with(b, &global_agents_dir());
}

fn reload_roles_from_disk_with(b: &UiBackend, global_dir: &Path) {
    use latte_agent_core::role::RoleTemplate;
    let mut roles: std::collections::HashMap<String, RoleTemplate> = Default::default();
    // 项目层：agents_config 原值（相对 b.cwd 解析），文件或目录模式均可。
    let raw = PathBuf::from(&b.agents_config);
    let abs = if raw.is_absolute() { raw } else { b.cwd.join(raw) };
    if std::fs::metadata(&abs).is_ok() {
        if let Some(p) = abs.to_str() {
            if let Ok(part) = AgentConfig::load(p) {
                roles.extend(part.roles);
            }
        }
    }
    // 全局层：项目未声明的 id 直接插入；已声明的只补空字段。
    if global_dir.is_dir() {
        if let Some(g) = global_dir.to_str().and_then(|p| AgentConfig::load(p).ok()) {
            for (id, role) in g.roles {
                match roles.entry(id) {
                    std::collections::hash_map::Entry::Vacant(e) => {
                        e.insert(role);
                    }
                    std::collections::hash_map::Entry::Occupied(mut e) => {
                        let existing = e.get_mut();
                        if existing.model_chain.is_empty() && !role.model_chain.is_empty() {
                            existing.model_chain = role.model_chain;
                        }
                        if existing.icon.is_empty() && !role.icon.is_empty() {
                            existing.icon = role.icon;
                        }
                        if existing.temperature.is_none() && role.temperature.is_some() {
                            existing.temperature = role.temperature;
                        }
                    }
                }
            }
        }
    }
    if roles.is_empty() {
        return;
    }
    b.merged.write().roles = roles;
    // 磁盘上的角色编辑（外部改文件）同样触发热替换，运行中的
    // session / workflow 分派用上新模型指派。
    hot_reload_resolver(b);
}

pub async fn get_roles_config(b: &UiBackend) -> Result<RolesConfigResponse, ApiError> {
    // 表单数据与源文件 tab 同源：先从磁盘重载，再读内存。
    reload_roles_from_disk(b);
    let agents_dir = agents_config_dir(&b.cwd, &b.agents_config);
    let (roles, available_models) = {
        let cfg = b.merged.read();
        let mut ids: Vec<&String> = cfg.roles.keys().collect();
        ids.sort();
        let roles: Vec<RoleConfigEntry> = ids
            .into_iter()
            .filter_map(|id| cfg.roles.get(id))
            .map(|tpl| role_config_entry(b, tpl))
            .collect();
        let available_models = enumerate_available_models(b);
        (roles, available_models)
    };
    let available_tools = enumerate_available_tools().await?;
    Ok(RolesConfigResponse {
        roles,
        available_models,
        available_tools,
        tiers: vec!["premium".into(), "standard".into(), "budget".into()],
        workspace_path: b.cwd.display().to_string(),
        agents_config_path: agents_dir.display().to_string(),
        sessions_path: crate::sessions::SessionPersist::dir_for(&b.cwd).display().to_string(),
    })
}

/// 枚举可用模型 —— 给角色编辑器的「模型链」下拉框使用。
///
/// 数据源与 [`list_models`] **完全同源**：磁盘扫描的 `ModelsState::entries`
/// （不去重，每个 model 文件一条记录）+ 内存 catalog 里未落盘的补充。
/// 这样角色编辑器看到的模型集合与模型管理面板一致 —— 同一
/// `provider/name` 在项目层与全局层各有一个文件时，两边都列出两条，
/// 且都带各自的 source 标签。
///
/// 排序：provider → name → source → 无 path，保证 UI 下拉框顺序稳定。
fn enumerate_available_models(b: &UiBackend) -> Vec<AvailableModel> {
    let cfg = b.merged.read();
    let project_dir = b.cwd.join(".latte/models.d");
    let on_disk = crate::models::ModelsState::load(&project_dir, &global_dir_fallback())
        .unwrap_or_default();
    let mut out: Vec<AvailableModel> = Vec::new();
    let mut seen_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (key, recs) in &on_disk.entries {
        for (src, _path, def) in recs {
            out.push(AvailableModel {
                name: def.name.clone(),
                provider: def.provider.clone(),
                source: source_label(*src).to_string(),
                key: key.clone(),
            });
            seen_keys.insert(key.clone());
        }
    }
    // 内存 catalog 里有但磁盘上不存在的（catalog 源）追加在末尾。
    for def in &cfg.models.models {
        let key = if def.name.contains('/') {
            def.name.clone()
        } else {
            format!("{}/{}", def.provider, def.name)
        };
        if seen_keys.contains(&key) {
            continue;
        }
        out.push(AvailableModel {
            name: def.name.clone(),
            provider: def.provider.clone(),
            source: "catalog".to_string(),
            key: key.clone(),
        });
    }
    out.sort_by(|a, b| {
        a.provider
            .cmp(&b.provider)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.source.cmp(&b.source))
    });
    out
}

/// `POST /api/roles/config` 的请求体；Tauri `ui_roles_config_save` 的
/// `config` 参数也是它。`category` / `prompt_file` / `skills` 不在
/// 其中 —— 保存时保持原值不变。
#[derive(Deserialize)]
pub struct SaveRoleConfigRequest {
    pub id: String,
    pub name: String,
    pub icon: String,
    pub model_tier: String,
    #[serde(default)]
    pub model_chain: Vec<String>,
    pub temperature: Option<f64>,
    #[serde(default)]
    pub tools: Vec<String>,
    /// 领域代码/文档路径（空数组 = 清空）。
    #[serde(default)]
    pub code_paths: Vec<String>,
    #[serde(default)]
    pub prompt: String,
}
fn agents_config_dir(cwd: &std::path::Path, agents_config: &str) -> PathBuf {
    let raw = PathBuf::from(agents_config);
    let p = if raw.is_absolute() { raw } else { cwd.join(raw) };
    if p.is_file() {
        return p.parent().map(Path::to_path_buf).unwrap_or_else(|| cwd.join(".latte/agents.d"));
    }
    if agents_config.trim().is_empty() {
        return cwd.join(".latte/agents.d");
    }
    p
}

fn str_array(items: &[String]) -> toml_edit::Array {
    items.iter().map(String::as_str).collect()
}

/// 重写 `.latte/agents.d/<id>.toml`。已有文件用 toml_edit 原地改值，
/// 保持原有字段顺序/格式；新文件按项目现有风格（参考 manager.toml）
/// 排字段顺序。`category` / `prompt_file` / `skills` 保持原值。
fn write_role_toml(
    path: &std::path::Path,
    tpl: &latte_agent_core::role::RoleTemplate,
    req: &SaveRoleConfigRequest,
) -> Result<(), String> {
    use toml_edit::{value, DocumentMut, Item, Table};
    let content = std::fs::read_to_string(path).ok();
    let mut doc: DocumentMut = match &content {
        Some(c) => c.parse().map_err(|e| format!("parse {}: {e}", path.display()))?,
        None => DocumentMut::new(),
    };
    let role_exists = doc
        .get("roles")
        .and_then(|r| r.get(req.id.as_str()))
        .and_then(|r| r.as_table())
        .is_some();

    // 如果 roles 存在但 role_exists 为 false（如 `roles = {}` inline table），
    // toml_edit 无法向 inline table 追加 key，需要重建 roles 表。
    let roles_is_inline = doc.get("roles").map_or(false, |r| !r.is_table());

    if role_exists && !roles_is_inline {
        let t = doc["roles"][req.id.as_str()].as_table_mut().unwrap();
        t["name"] = value(req.name.clone());
        t["icon"] = value(req.icon.clone());
        t["model_tier"] = value(req.model_tier.clone());
        t["model_chain"] = value(str_array(&req.model_chain));
        t["tools"] = value(str_array(&req.tools));
        t["code_paths"] = value(str_array(&req.code_paths));
        match req.temperature {
            Some(temp) => {
                t["temperature"] = value(temp);
            }
            None => {
                t.remove("temperature");
            }
        }
    } else {
        // toml_edit 无法覆盖已有的 inline table key（如 `roles = {}`），
        // 需要先删除再重建。
        doc.remove("roles");
        let mut roles = Table::new();
        let mut t = Table::new();
        t["id"] = value(req.id.clone());
        t["name"] = value(req.name.clone());
        t["category"] = value(tpl.category.clone());
        t["model_tier"] = value(req.model_tier.clone());
        if let Some(pf) = &tpl.prompt_file {
            t["prompt_file"] = value(pf.clone());
        }
        if let Some(temp) = req.temperature {
            t["temperature"] = value(temp);
        }
        if !req.model_chain.is_empty() {
            t["model_chain"] = value(str_array(&req.model_chain));
        }
        t["icon"] = value(req.icon.clone());
        t["tools"] = value(str_array(&req.tools));
        if !req.code_paths.is_empty() {
            t["code_paths"] = value(str_array(&req.code_paths));
        }
        if !tpl.skills.is_empty() {
            t["skills"] = value(str_array(&tpl.skills));
        }
        roles[req.id.as_str()] = Item::Table(t);
        doc["roles"] = Item::Table(roles);
    }
    let output = doc.to_string();
    std::fs::write(path, &output)
        .map_err(|e| format!("write {}: {e}", path.display()))
}

/// 保存角色配置：重写角色 TOML → 写 prompt 文件 → 更新内存配置
/// （新 session 即刻生效）。三步全量落盘，与 HTTP 版语义一致。
///
/// 写入路径由 [`resolve_role_toml_path`] / [`resolve_prompt_path`] 决定，
/// 与 GET 展示的有效路径一致：项目层已有该角色 → 写项目；否则写全局。
/// prompt 写入 runtime 实际读取的位置（项目 `<cwd>/<prompt_file>` 或全局
/// `~/.latte/prompts.d/<basename>`）。
pub fn save_role_config(
    b: &UiBackend,
    req: SaveRoleConfigRequest,
) -> Result<RoleConfigEntry, ApiError> {
    let tpl = {
        let cfg = b.merged.read();
        cfg.roles.get(&req.id).cloned()
    };
    let tpl = tpl.ok_or_else(|| ApiError::not_found(format!("role {:?} not found", req.id)))?;

    // 1. 确定写入路径并重写 TOML（项目已有该角色 → 项目，否则 → 全局）
    let project_dir = agents_config_dir(&b.cwd, &b.agents_config);
    let (path, project_role) =
        resolve_role_toml_path(&project_dir, &global_agents_dir(), &req.id);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| ApiError::internal(format!("create {}: {e}", dir.display())))?;
    }
    write_role_toml(&path, &tpl, &req).map_err(ApiError::internal)?;

    // 2. prompt 非空 → 写到 runtime 实际读取的有效路径（项目
    // `<cwd>/<prompt_file>` 或全局 `prompts.d/<basename>`；prompt_file
    // 为 None 时用 `prompts/<id>.md`）。
    if !req.prompt.is_empty() {
        let rel = tpl
            .prompt_file
            .clone()
            .unwrap_or_else(|| format!("prompts/{}.md", req.id));
        let p = resolve_prompt_path(&b.cwd, global_root_dir().as_deref(), &rel, project_role);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ApiError::internal(format!("create {}: {e}", parent.display())))?;
        }
        std::fs::write(&p, &req.prompt)
            .map_err(|e| ApiError::internal(format!("write {}: {e}", p.display())))?;
    }

    // 3. 更新内存中的 merged —— 新 session 即刻生效。
    let entry = {
        let mut cfg = b.merged.write();
        match cfg.roles.get_mut(&req.id) {
            Some(t) => {
                t.name = req.name.clone();
                t.icon = req.icon.clone();
                t.model_tier = req.model_tier.clone();
                t.model_chain = req.model_chain.clone();
                t.temperature = req.temperature;
                t.tools = req.tools.clone();
                t.code_paths = req.code_paths.clone();
                role_config_entry(b, t)
            }
            None => {
                return Err(ApiError::not_found(format!("role {:?} not found", req.id)))
            }
        }
    };
    // 角色模型指派（model_chain/model_tier）热生效：已加载 session
    // 的 runner 在下一个 turn 边界 / 暂停恢复重试时自动换链。
    hot_reload_resolver(b);
    Ok(entry)
}

/// `POST /api/roles` — 创建新角色：生成默认 RoleTemplate，写入
/// `<agents.d>/<role_id>.toml`，并加入内存配置。
pub fn create_role(b: &UiBackend, role_id: &str, role_name: &str) -> Result<RoleConfigEntry, ApiError> {
    // 检查是否已存在
    {
        let cfg = b.merged.read();
        if cfg.roles.contains_key(role_id) {
            return Err(ApiError::bad_request(format!("role {:?} already exists", role_id)));
        }
    }
    let tpl = latte_agent_core::role::RoleTemplate {
        id: role_id.to_string(),
        name: role_name.to_string(),
        category: "custom".to_string(),
        model_tier: "standard".to_string(),
        model_chain: vec![],
        prompt_file: None,
        temperature: None,
        tools: vec![],
        icon: String::new(),
        skills: vec![],
            code_paths: vec![],
    };
    let agents_dir = agents_config_dir(&b.cwd, &b.agents_config);
    std::fs::create_dir_all(&agents_dir)
        .map_err(|e| ApiError::internal(format!("create {}: {e}", agents_dir.display())))?;
    let path = agents_dir.join(format!("{}.toml", role_id));
    // 序列化为 toml：`[roles.<role_id>]` 节。
    {
        use toml_edit::{value, DocumentMut, Table, Item};
        let mut doc = DocumentMut::new();
        let mut roles = Table::new();
        let mut t = Table::new();
        t["id"] = value(role_id.to_string());
        t["name"] = value(role_name.to_string());
        t["category"] = value("custom");
        t["model_tier"] = value("standard");
        t["icon"] = value("");
        t["model_chain"] = value(toml_edit::Array::new());
        t["tools"] = value(toml_edit::Array::new());
        roles[role_id] = Item::Table(t);
        doc["roles"] = Item::Table(roles);
        std::fs::write(&path, doc.to_string())
            .map_err(|e| ApiError::internal(format!("write {}: {e}", path.display())))?;
    }
    // 加入内存配置
    {
        let mut cfg = b.merged.write();
        cfg.roles.insert(role_id.to_string(), tpl.clone());
    }
    hot_reload_resolver(b);
    Ok(role_config_entry(b, &tpl))
}

/// `DELETE /api/roles/:id` — 删除角色：移除 agents.d 文件（项目 +
/// 全局目录）并从内存配置移除。文件不存在但内存中有角色时仍可删除。
pub fn delete_role(b: &UiBackend, role_id: &str) -> Result<(), ApiError> {
    // 检查内存中是否存在
    {
        let cfg = b.merged.read();
        if !cfg.roles.contains_key(role_id) {
            return Err(ApiError::not_found(format!("role {:?} not found", role_id)));
        }
    }
    // 尝试从项目目录和全局目录删除 toml 文件
    let agents_dir = agents_config_dir(&b.cwd, &b.agents_config);
    let global_dir = global_agents_dir();
    for dir in &[&agents_dir, &global_dir] {
        let path = dir.join(format!("{}.toml", role_id));
        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|e| ApiError::internal(format!("remove {}: {e}", path.display())))?;
        }
    }
    // 从内存配置移除
    {
        let mut cfg = b.merged.write();
        cfg.roles.remove(role_id);
    }
    hot_reload_resolver(b);
    Ok(())
}

// ─── Chat ─────────────────────────────────────────────────────────

/// `POST /api/chat/send`：向 session 的 controller 提交一条用户消息
/// （恢复 session 此时懒 spawn controller）。立即返回（"已受理"语
/// 义）；产出走 [`subscribe_session`] 的事件流。
pub async fn chat_send(
    b: &UiBackend,
    session_id: Option<&str>,
    message: &str,
) -> Result<(), ApiError> {
    let h = resolve_session(b, session_id)?;
    h.touch();
    // First-message preview for the sidebar entry. Cheap + bounded.
    if h.first_user_msg.lock().is_none() {
        let preview = message.chars().take(80).collect::<String>();
        *h.first_user_msg.lock() = Some(preview);
    }
    let controller = h
        .controller_or_spawn()
        .await
        .map_err(|e| ApiError::internal(format!("spawn controller: {e}")))?;
    controller.submit_input(message).await;
    Ok(())
}

/// `POST /api/chat/command`：特殊命令（/clear 等），与 send 同通道。
pub async fn chat_command(
    b: &UiBackend,
    session_id: Option<&str>,
    command: &str,
) -> Result<(), ApiError> {
    let h = resolve_session(b, session_id)?;
    h.touch();
    let controller = h
        .controller_or_spawn()
        .await
        .map_err(|e| ApiError::internal(format!("spawn controller: {e}")))?;
    controller.submit_input(command).await;
    Ok(())
}

/// `POST /api/chat/role`：切换 session 的活动角色。
pub async fn chat_switch_role(
    b: &UiBackend,
    session_id: Option<&str>,
    role_id: &str,
) -> Result<(), ApiError> {
    let h = resolve_session(b, session_id)?;
    h.touch();
    let controller = h
        .controller_or_spawn()
        .await
        .map_err(|e| ApiError::internal(format!("spawn controller: {e}")))?;
    controller.switch_role(role_id).await;
    Ok(())
}

/// `POST /api/chat/cancel-turn`：只打断当前正在跑的 turn（用户从
/// `TimeoutWarning` 弹窗里点了「终止当前任务」）。不退出 session，
/// 不删落盘文件；下一次用户输入照常接收。
///
/// 与 `DELETE /api/sessions` 不同：后者是整个 session abort + 落盘
/// 删除（`chat_controller_abort` 语义），这里是 per-turn cancel。
/// 名字上的区分很重要 —— cancel 暗示"这个 turn 不要了，但 session
pub async fn chat_cancel_turn(
    b: &UiBackend,
    session_id: Option<&str>,
) -> Result<(), ApiError> {
    let h = resolve_session(b, session_id)?;
    h.touch();
    if let Some(controller) = h.try_controller() {
        controller.cancel_turn().await;
        Ok(())
    } else {
        Ok(())
    }
}

/// `POST /api/chat/abort` — 终止整个 session（所有 in-flight
/// subagent / workflow / multi-role 全部停止）。不删 session，
/// 归档事件保留。
pub async fn chat_abort(
    b: &UiBackend,
    session_id: Option<&str>,
) -> Result<(), ApiError> {
    let h = resolve_session(b, session_id)?;
    h.touch();
    let controller = h
        .controller_or_spawn()
        .await
        .map_err(|e| ApiError::internal(format!("spawn controller: {e}")))?;
    controller.abort().await;
    // 连带取消该 session 事件流上正在跑的 workflow（如 resume 续
    // 跑的）——它不走 controller 的 cancel_flag，有自己的句柄。
    if let Some(flag) = b.session_workflows.write().remove(h.session_id.as_str()) {
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(())
}

/// `POST /api/chat/pause` — 暂停当前 session。会话保留，正在等待
/// 的下一个 turn 会被挂起，直到收到 resume。若 controller 尚未
/// spawn（磁盘恢复但未激活），无操作。
pub async fn chat_pause(
    b: &UiBackend,
    session_id: Option<&str>,
) -> Result<(), ApiError> {
    let h = resolve_session(b, session_id)?;
    h.touch();
    if let Some(controller) = h.try_controller() {
        controller.pause().await;
    }
    Ok(())
}

/// `POST /api/chat/resume` — 恢复被 `chat_pause` 暂停的 session。
/// 若 controller 尚未 spawn，无操作。
pub async fn chat_resume(
    b: &UiBackend,
    session_id: Option<&str>,
) -> Result<(), ApiError> {
    let h = resolve_session(b, session_id)?;
    h.touch();
    if let Some(controller) = h.try_controller() {
        controller.resume().await;
    }
    Ok(())
}

/// `POST /api/chat/pause-session` — 用户按 ⏸ 触发全 session 冻结。
pub async fn chat_pause_session(
    b: &UiBackend,
    session_id: Option<&str>,
) -> Result<(), ApiError> {
    let h = resolve_session(b, session_id)?;
    h.touch();
    if let Some(controller) = h.try_controller() {
        controller.pause_session();
    }
    Ok(())
}

/// `POST /api/chat/resume-session` — 与 [`chat_pause_session`] 配对。
///
/// 重启兜底：gate 并未暂停（session 从磁盘恢复、controller 未 spawn
/// 或新 gate 从未 engage）时，旧进程的 workflow task 已随进程消亡，
/// resume 本身是空操作 —— 此时 ▶ 的语义升级为「从 checkpoint 续跑
/// 中断的 workflow」，并补发一条 `Resumed` 同步 UI 暂停态（replay
/// 会把历史 Paused 恢复成 isPaused=true，而本进程 gate 未 engage，
/// 不会有 listener 发 Resumed）。
pub async fn chat_resume_session(
    b: &UiBackend,
    session_id: Option<&str>,
) -> Result<(), ApiError> {
    let h = resolve_session(b, session_id)?;
    h.touch();
    let resumed_live = match h.try_controller() {
        Some(controller) => controller.resume_session().is_some(),
        None => false,
    };
    if resumed_live {
        return Ok(());
    }
    let controller = h
        .controller_or_spawn()
        .await
        .map_err(|e| ApiError::internal(format!("spawn controller: {e}")))?;
    let _ = controller.event_sender().send(ChatEvent::Resumed);
    // spawn 时已从 event_log seed 最近可续跑的 workflow；有才续跑，
    // 没有则只是解除 UI 暂停态（幂等 no-op）。404（checkpoint 已删）
    // 与 409（已在跑，用户连点）都按成功处理。
    if controller.last_failed_workflow().is_some() {
        match spawn_workflow_resume(b, &h.session_id, None, None).await {
            Ok(_) => {}
            Err(e) if e.status == 404 || e.status == 409 => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// `POST /api/chat/stream-mode` -- 运行时切换 stream/non-stream 模式。
///
/// 设置 `SessionHandle.stream_mode` 的 `Arc<AtomicBool>`，AgentRunner
/// 的 `run_turn` 在下一次模型调用时读取该值决定走 stream 还是 non-stream 路径。
pub async fn set_stream_mode(
    b: &UiBackend,
    session_id: Option<&str>,
    stream: bool,
) -> Result<(), ApiError> {
    let h = resolve_session(b, session_id)?;
    h.touch();
    h.stream_mode.store(stream, std::sync::atomic::Ordering::SeqCst);
    Ok(())
}

/// `POST /api/chat/pause-role` — 单独暂停一个角色（多角色 HIL v1.4）。
/// 与整会话的 [`chat_pause`] 正交：被暂停的角色在每轮里被 scheduler
/// 跳过，其余角色照常推进，全局 `SessionState` 不变。底层复用
/// controller 的 `/pause <role>` 输入通道；controller 校验角色是否
/// 存在后，会回发结构化的 `RolePaused` ChatEvent 驱动前端标记。
pub async fn chat_pause_role(
    b: &UiBackend,
    session_id: Option<&str>,
    role_id: &str,
) -> Result<(), ApiError> {
    let h = resolve_session(b, session_id)?;
    h.touch();
    let controller = h
        .controller_or_spawn()
        .await
        .map_err(|e| ApiError::internal(format!("spawn controller: {e}")))?;
    controller.submit_input(&format!("/pause {role_id}")).await;
    Ok(())
}

/// `POST /api/chat/resume-role` — 恢复被 [`chat_pause_role`] 单独暂停
/// 的角色。controller 校验角色处于暂停态后回发 `RoleResumed`
/// ChatEvent。与整会话的 [`chat_resume`] 正交。
pub async fn chat_resume_role(
    b: &UiBackend,
    session_id: Option<&str>,
    role_id: &str,
) -> Result<(), ApiError> {
    let h = resolve_session(b, session_id)?;
    h.touch();
    let controller = h
        .controller_or_spawn()
        .await
        .map_err(|e| ApiError::internal(format!("spawn controller: {e}")))?;
    controller.submit_input(&format!("/resume {role_id}")).await;
    Ok(())
}

#[derive(Serialize)]
pub struct TraceSummary {
    pub session_id: String,
    pub path: String,
    pub size_bytes: u64,
    pub modified_unix: u64,
}

/// `GET /api/traces` — 列出 `$LATTE_HOME/traces` 下全部 .jsonl。
/// 注意锚点是 `LATTE_HOME`/`HOME` 而非 backend cwd。
pub fn list_traces() -> Vec<TraceSummary> {
    let dir = latte_home().join("traces");
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return out,
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
    out
}

/// `GET /api/traces/:session_id` — 读单个 trace 文件为
/// `{ "session_id", "events": [...] }`。
pub fn read_trace(session_id: &str) -> Result<serde_json::Value, ApiError> {
    let path = latte_home().join("traces").join(format!("{}.jsonl", session_id));
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| ApiError::not_found(format!("trace not found: {e}")))?;
    let mut events = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            events.push(v);
        }
    }
    Ok(serde_json::json!({
        "session_id": session_id,
        "events": events,
    }))
}

fn latte_home() -> PathBuf {
    std::env::var_os("LATTE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".latte")))
        .unwrap_or_else(|| PathBuf::from(".latte"))
}

// ─── Subsessions ──────────────────────────────────────────────────

/// `GET /api/subsessions?id=<sub_id>` — 取 delegate 子会话的完整
/// transcript（主聊天只见 summary）。不存在时返回空数组。
///
/// 可选参数 `limit=N`（默认 0 = 全部）。`limit>0` 时走尾部有限读取
/// `read_persisted_last_n`，只返回最近 N 个事件，避免 2.7GB 级超大
/// subsession 文件炸内存 / 超时。
///
/// 读路径：先内存（活读，1h 内）→ 内存 miss 再回查磁盘
///（`subsession_store.read_persisted`）。
pub fn get_subsession(b: &UiBackend, id: &str, limit: usize) -> Vec<serde_json::Value> {
    let result = b.subsession_store.snapshot_any(id).map(|evs| (evs, false)).or_else(|| {
        if limit > 0 {
            b.subsession_store
                .read_persisted_last_n(id, limit, 5 * 1024 * 1024)
        } else {
            b.subsession_store.read_persisted(id).map(|evs| (evs, false))
        }
    });
    match result {
        Some((events, truncated)) => {
            let mut v: Vec<serde_json::Value> = events
                .into_iter()
                .filter_map(|e| serde_json::to_value(e).ok())
                .collect();
            if truncated {
                v.push(serde_json::json!({
                    "__truncated__": true,
                    "limit": limit,
                    "note": format!("事件超过显示上限（{}），仅显示最后 {limit} 条。传 limit=0 获取全部。", limit)
                }));
            }
            v
        }
        None => vec![],
    }
}

// ─── Role Graph ───────────────────────────────────────────────────

/// `GET /api/role-graph` — 基于 backend cwd 构建 "role × tool" 图谱。
pub async fn role_graph(b: &UiBackend) -> Result<RoleGraph, ApiError> {
    crate::role_graph::build(&b.cwd, &b.cwd)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))
}

// ─── Self-Loop ────────────────────────────────────────────────────

/// `POST /api/self-loop/start`：校验 → 建 broadcast → spawn node 子
/// 进程。返回 `(响应 JSON, 事件订阅)`：Tauri 适配器需要订阅句柄在
/// spawn 前就绪（避免丢 "started" 事件）；HTTP 壳只用响应 JSON，
/// 事件经 `GET /api/self-loop/events`（[`self_loop_subscribe`]）。
pub async fn self_loop_start(
    b: &UiBackend,
    task: String,
    max_iterations: Option<u32>,
) -> Result<(serde_json::Value, broadcast::Receiver<SelfLoopEvent>), ApiError> {
    let max = max_iterations.unwrap_or(5);
    if task.trim().is_empty() {
        return Err(ApiError::bad_request("task is required"));
    }

    // 起 broadcast channel。
    let (tx, rx) = broadcast::channel::<SelfLoopEvent>(64);
    b.self_loop.set_progress(tx.clone());

    // 找 self-loop 脚本。
    let self_loop_dir = crate::self_loop::self_loop_dir().map_err(ApiError::not_found)?;
    if !self_loop_dir.exists() {
        return Err(ApiError::not_found(format!(
            "self-loop runner not found at {}. Run `pnpm --dir latte-agent-cli/ui install` first.",
            self_loop_dir.display()
        )));
    }

    // spawn node 子进程跑 self-loop/runner.ts。
    let task_clone = task.clone();
    let self_loop_dir_clone = self_loop_dir.clone();
    tokio::spawn(async move {
        crate::self_loop::run_self_loop_node(task_clone, max, self_loop_dir_clone, tx).await;
    });

    Ok((
        serde_json::json!({
            "started": true,
            "task": task,
            "max_iterations": max,
        }),
        rx,
    ))
}

/// `POST /api/self-loop/stop`。
pub fn self_loop_stop(b: &UiBackend) {
    b.self_loop.clear();
}

/// 订阅 self-loop 进度事件。没有在跑的 self-loop 时返回一个 1-容量
/// 空 channel 的 receiver（前端 start 后重新订阅）。
pub fn self_loop_subscribe(b: &UiBackend) -> broadcast::Receiver<SelfLoopEvent> {
    b.self_loop.subscribe()
}

// ─── Workflows ────────────────────────────────────────────────────

/// `GET /api/workflows`。
pub fn list_workflows(b: &UiBackend) -> Result<Vec<WorkflowSummary>, ApiError> {
    Ok(workflows::list(&b.cwd))
}

/// `GET /api/workflows/:name`。
pub fn get_workflow(b: &UiBackend, name: &str) -> Result<WorkflowDetail, ApiError> {
    workflows::get(&b.cwd, name).map_err(ApiError::not_found)
}

/// 校验表单；errors 非空 → 400（消息为 join 后的全部错误）。
fn validate_form_or_400(b: &UiBackend, form: &WorkflowForm) -> Result<(), ApiError> {
    let v = workflows::validate(form, &b.merged.read());
    if v.ok {
        Ok(())
    } else {
        Err(ApiError::bad_request(v.errors.join("; ")))
    }
}

/// `POST /api/workflows`：先校验，重名（项目或全局）→ 409。
pub fn create_workflow(b: &UiBackend, form: WorkflowForm) -> Result<WorkflowDetail, ApiError> {
    validate_form_or_400(b, &form)?;
    if workflows::exists(&b.cwd, &form.name) {
        return Err(ApiError {
            status: 409,
            message: format!("workflow '{}' already exists", form.name),
        });
    }
    workflows::create(&b.cwd, &form).map_err(ApiError::internal)
}

/// `PUT /api/workflows/:name`：先校验；项目副本原地更新，只有全局
/// 副本时生成项目遮蔽副本；`form.name != name` 即重命名。
pub fn update_workflow(
    b: &UiBackend,
    name: &str,
    form: WorkflowForm,
) -> Result<WorkflowDetail, ApiError> {
    validate_form_or_400(b, &form)?;
    if !workflows::exists(&b.cwd, name) {
        return Err(ApiError::not_found(format!("workflow '{name}' not found")));
    }
    workflows::update(&b.cwd, name, &form).map_err(ApiError::internal)
}

/// `DELETE /api/workflows/:name`：只删项目副本；只有全局副本 → 400。
pub fn delete_workflow(b: &UiBackend, name: &str) -> Result<(), ApiError> {
    workflows::delete(&b.cwd, name).map_err(|e| {
        if e.contains("not found") {
            ApiError::not_found(e)
        } else {
            ApiError::bad_request(e)
        }
    })
}

/// `POST /api/workflows/validate`：只校验，不落盘。
pub fn validate_workflow_form(b: &UiBackend, form: &WorkflowForm) -> ValidateResponse {
    workflows::validate(form, &b.merged.read())
}

/// `GET /api/workflows/:name/toml` — 读取 workflow TOML 源文件原始内容。
pub fn get_workflow_toml(b: &UiBackend, name: &str) -> Result<String, ApiError> {
    get_workflow(b, name).map(|d| d.raw_toml)
}

/// `PUT /api/workflows/:name/toml` — 直接写入 workflow TOML 源文件。
/// 项目副本存在 → 写项目；只有全局副本 → 在项目层生成遮蔽副本。
pub fn put_workflow_toml(b: &UiBackend, name: &str, raw: &str) -> Result<(), ApiError> {
    // 校验 TOML 可解析且 name 匹配
    let doc: toml_edit::DocumentMut = raw
        .parse()
        .map_err(|e| ApiError::bad_request(format!("TOML 解析失败: {e}")))?;
    if doc.get("name").and_then(|v| v.as_str()) != Some(name) {
        return Err(ApiError::bad_request(format!(
            "TOML 中 name 为 {:?}，与请求名称 {name:?} 不匹配",
            doc.get("name").and_then(|v| v.as_str()).unwrap_or("?"),
        )));
    }
    if !workflows::exists(&b.cwd, name) {
        return Err(ApiError::not_found(format!("workflow '{name}' not found")));
    }
    let dir = workflows::project_dir(&b.cwd);
    std::fs::create_dir_all(&dir)
        .map_err(|e| ApiError::internal(format!("create {}: {e}", dir.display())))?;
    let path = dir.join(format!("{name}.toml"));
    std::fs::write(&path, raw)
        .map_err(|e| ApiError::internal(format!("write {}: {e}", path.display())))?;
    Ok(())
}

/// `POST /api/workflows/run` 的请求体。`name`（跑已保存的 workflow）
/// 与 `workflow`（跑编辑器里未保存的表单）必须且只能给一个。
#[derive(Debug, Deserialize)]
pub struct WorkflowRunRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub workflow: Option<WorkflowForm>,
    pub topic: String,
    /// 额外的 `{{var}}` 替换（在 `topic`/output_key 之外，运行前直接
    /// 替换到各 step 的 prompt/task 文本里）。
    #[serde(default)]
    pub vars: Option<HashMap<String, String>>,
}

/// `POST /api/workflows/run`：校验 → 建 broadcast + cancel flag →
/// spawn `run_workflow`。返回 `(响应 JSON, 事件订阅)`，语义同
/// [`self_loop_start`]：Tauri 适配器需要订阅句柄在 spawn 前就绪，
/// HTTP 壳只用响应 JSON，事件走 `GET /api/workflows/run/events`。
pub fn workflow_run_start(
    b: &UiBackend,
    req: WorkflowRunRequest,
) -> Result<(serde_json::Value, broadcast::Receiver<ChatEvent>), ApiError> {
    if req.name.is_some() == req.workflow.is_some() {
        return Err(ApiError::bad_request(
            "exactly one of 'name' or 'workflow' is required",
        ));
    }
    if req.topic.trim().is_empty() {
        return Err(ApiError::bad_request("topic is required"));
    }

    // 解析 def：内联表单 → 直接构造（允许测试未保存的编辑）；
    // name → 从 workflows.d 加载。两条路都先跑 validate。
    let (wf, form) = match (&req.name, &req.workflow) {
        (Some(name), None) => {
            let wf = load_workflow(name, &b.cwd).map_err(ApiError::not_found)?;
            let form = workflows::form_from_def(&wf);
            (wf, form)
        }
        (None, Some(form)) => (workflows::def_from_form(form), form.clone()),
        _ => unreachable!(),
    };
    validate_form_or_400(b, &form)?;

    if b.workflow_run.is_active() {
        return Err(ApiError {
            status: 409,
            message: "a workflow test run is already active".into(),
        });
    }

    let (tx, rx) = broadcast::channel::<ChatEvent>(64);
    let cancel = Arc::new(AtomicBool::new(false));
    let run_id = format!(
        "wfui-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros())
            .unwrap_or(0)
    );
    b.workflow_run.start(tx.clone(), cancel.clone(), run_id.clone());

    // 与 sessions.rs 的 ControllerConfig 一致：default_params 用
    // GenerateParams::default()（session spawn 路径同款，也是
    // controller 里 workflow 工具收到的值）。
    let state = b.workflow_run.clone();
    let merged = Arc::new(b.merged.read().clone());
    let resolver = b.resolver.clone();
    let cwd = b.cwd.clone();
    let topic = req.topic.clone();
    let vars = req.vars.clone();
    let wf_name = wf.name.clone();
    tokio::spawn(async move {
        let mut wf = wf;
        // vars 预替换：直接改各 step 的 prompt/task 文本（engine 只做
        // topic/output_key 替换，不扩展它的签名）。
        if let Some(vars) = vars {
            for step in &mut wf.steps {
                for (k, v) in &vars {
                    step.prompt = step.prompt.replace(&format!("{{{{{k}}}}}"), v);
                    step.task = step.task.replace(&format!("{{{{{k}}}}}"), v);
                }
            }
        }
        // engine 发到内部 channel，forwarder 经 state.broadcast 转发：
        // 同一把锁内记录回放缓冲 + 广播，晚连接的 SSE 客户端能补到
        // 开头的事件（见 WorkflowRunState::broadcast）。
        let (tx_inner, mut rx_inner) = broadcast::channel::<ChatEvent>(64);
        let fwd_state = state.clone();
        let forwarder = tokio::spawn(async move {
            while let Ok(ev) = rx_inner.recv().await {
                fwd_state.broadcast(ev);
            }
        });
        let ctx = WorkflowRunContext {
            merged,
            resolver,
            default_params: GenerateParams::default(),
            cwd,
            event_tx: tx_inner,
            cancel_flag: cancel,
            agent_pause_gate: None, // 独立测试 run：无 session gate
            depth: 0,
            // 独立测试 run 无 session：不建 subsession、不走 advisor。
            subsession_store: None,
            session_id: None,
            advisor_gate: None,
            advisor_pause: None,
        };
        let _ = run_workflow(&wf, &topic, &ctx).await;
        // engine 返回后丢掉 tx_inner 关闭内部 channel，forwarder 排空后退出。
        drop(ctx);
        let _ = forwarder.await;
        state.clear();
    });

    Ok((
        serde_json::json!({
            "started": true,
            "run_id": run_id,
            "name": wf_name,
        }),
        rx,
    ))
}

/// `POST /api/workflows/run/stop`。
pub fn workflow_run_stop(b: &UiBackend) {
    b.workflow_run.cancel();
}

/// 订阅测试运行事件并带回放缓冲（订阅前已发出的事件）。没有在跑的
/// run 时返回空 history + 1-容量空 channel 的 receiver。
pub fn workflow_run_subscribe(b: &UiBackend) -> (Vec<ChatEvent>, broadcast::Receiver<ChatEvent>) {
    b.workflow_run.subscribe_with_history()
}

/// `POST /api/workflows/resume` 的请求体。
#[derive(Debug, Deserialize)]
pub struct WorkflowResumeRequest {
    /// 哪个 session 的事件流上跑续跑（事件回该 session 的 SSE / event_log）。
    pub session_id: String,
    /// 显式指定失败 run 的 checkpoint wf_id。缺省 → 读 controller 的
    /// `last_failed_workflow` 快照（冷启动 session 由 sessions.rs 从
    /// event_log 反向扫描 seed），即「续跑最近一次失败」。
    #[serde(default)]
    pub wf_id: Option<String>,
    /// 可选 topic 覆盖；空/缺省回退到 checkpoint 里的 topic。
    #[serde(default)]
    pub topic: Option<String>,
}

/// `POST /api/workflows/resume`：校验 → 注册 cancel 句柄 → spawn
/// `run_workflow_resume`。与 [`workflow_run_start`] 不同，续跑复用
/// **session 的** event broadcast（不是独立测试 run 的 channel），
/// 前端无需新建 SSE 连接，事件自然出现在聊天面板。
pub async fn workflow_resume(
    b: &UiBackend,
    req: WorkflowResumeRequest,
) -> Result<serde_json::Value, ApiError> {
    spawn_workflow_resume(b, &req.session_id, req.wf_id, req.topic).await
}

/// 续跑共享实现：`workflow_resume` 与 `chat_resume_session` 的
/// 重启兜底共用。`wf_id` 缺省 = 该 session 最近可续跑的 workflow
/// （实时事件流由 controller 内部订阅者维护；冷启动由 sessions.rs
/// 从 event_log seed，含「中断未完成」的 run）。
async fn spawn_workflow_resume(
    b: &UiBackend,
    session_id: &str,
    wf_id: Option<String>,
    topic: Option<String>,
) -> Result<serde_json::Value, ApiError> {
    let h = resolve_session(b, Some(session_id))?;
    let controller = h
        .controller_or_spawn()
        .await
        .map_err(|e| ApiError::internal(format!("spawn controller: {e}")))?;

    // 续跑目标：显式 wf_id 优先；缺省 = 该 session 最近可续跑的
    // workflow。
    let wf_id = match wf_id {
        Some(id) => id,
        None => {
            controller
                .last_failed_workflow()
                .ok_or_else(|| {
                    ApiError::not_found("this session has no failed workflow to resume")
                })?
                .wf_id
        }
    };

    // at-most-one-per-session guard：同一 session 事件流上同时跑两个
    // workflow 会让 WorkflowStarted/Step/Finished 事件交错无法分辨。
    // 先于 checkpoint 校验注册：409（已在跑）优先于 404（目标无效）。
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut runs = b.session_workflows.write();
        if let Some(flag) = runs.get(session_id) {
            if !flag.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(ApiError {
                    status: 409,
                    message: "a workflow is already running on this session".into(),
                });
            }
        }
        runs.insert(session_id.to_string(), cancel.clone());
    }

    // 启动前校验：checkpoint 存在（load_checkpoint 内含 wf_id 安全
    // 检查：拒绝空 / 含路径分隔符 / `..`），workflow 定义可加载。
    // 校验失败要释放刚注册的 guard，否则会永久堵住这个 session。
    let validated = (|| -> Result<_, ApiError> {
        let ckpt = load_checkpoint(&b.cwd, &wf_id).map_err(ApiError::not_found)?;
        let wf = load_workflow(&ckpt.workflow_name, &b.cwd).map_err(ApiError::not_found)?;
        Ok(wf)
    })();
    let wf = match validated {
        Ok(wf) => wf,
        Err(e) => {
            b.session_workflows.write().remove(session_id);
            return Err(e);
        }
    };

    let ctx = WorkflowRunContext {
        merged: Arc::new(b.merged.read().clone()),
        resolver: b.resolver.clone(),
        default_params: GenerateParams::default(),
        cwd: b.cwd.clone(),
        event_tx: controller.event_sender(),
        cancel_flag: cancel,
        // 与任务看板派发的 run 同款：用户 ⏸ 时续跑也一起冻结。
        agent_pause_gate: Some(controller.session_pause_gate()),
        depth: 0,
        // 续跑跑在真实 session 事件流上：分派建 subsession（UI 右键
        // 可查日志）、过 advisor gate + 返回审查，与 manager 的
        // delegate / workflow 工具一致。
        subsession_store: Some(b.subsession_store.clone()),
        session_id: Some(session_id.to_string()),
        advisor_gate: latte_agent_core::advisor_monitor::AdvisorMonitorConfig::default()
            .runner_gate(),
        advisor_pause: Some(controller.advisor_pause_gate()),
    };
    let wf_name = wf.name.clone();
    let resp_wf_id = wf_id.clone();
    let topic = topic.unwrap_or_default();
    let runs = b.session_workflows.clone();
    let sid = session_id.to_string();
    tokio::spawn(async move {
        let _ = run_workflow_resume(&wf, &topic, &ctx, &wf_id).await;
        // run 结束（无论成败）释放 guard；失败事件里的新 wf_id 可再续跑。
        runs.write().remove(&sid);
    });

    Ok(serde_json::json!({
        "started": true,
        "wf_id": resp_wf_id,
        "name": wf_name,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 进程级 mutex，串行化测试里对 `LATTE_HOME` / `HOME` 的修改。
    /// `cargo test` 多线程跑测试，env 变量是进程级的，不锁会互相踩。
    static ENV_LOCK: std::sync::LazyLock<parking_lot::Mutex<()>> =
        std::sync::LazyLock::new(|| parking_lot::Mutex::new(()));

    /// 把 `LATTE_HOME` 重定向到临时目录跑 `f`，结束后恢复原值。
    /// 防止测试环境真实的 `~/.latte/models.d` 全局层污染结果。
    fn with_isolated_home<T>(f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock();
        let home = tempfile::tempdir().unwrap();
        let prev = std::env::var("LATTE_HOME").ok();
        std::env::set_var("LATTE_HOME", home.path());
        let result = f();
        match prev {
            Some(v) => std::env::set_var("LATTE_HOME", v),
            None => std::env::remove_var("LATTE_HOME"),
        }
        result
    }

    #[test]
    fn build_role_info_is_sorted() {
        let cfg = AgentConfig::default();
        let v = build_role_info(&cfg);
        assert!(v.is_empty());
    }

    /// 写一个 `[roles.<id>]` TOML 文件到 `<dir>/<id>.toml`。
    fn write_role_file(dir: &std::path::Path, id: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("{id}.toml"));
        std::fs::write(&path, format!("[roles.{id}]\nid = \"{id}\"\nname = \"N\"\n")).unwrap();
        path
    }

    #[test]
    fn resolve_role_toml_prefers_project_when_section_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project_agents");
        let global = tmp.path().join("global_agents");
        let pp = write_role_file(&project, "pm");
        let gp = write_role_file(&global, "pm");
        let (path, is_project) = resolve_role_toml_path(&project, &global, "pm");
        assert!(is_project);
        assert_eq!(path, pp);
        // 项目文件存在但不含 [roles.pm] 节 → 落全局
        std::fs::write(&pp, "roles = {}\n").unwrap();
        let (path, is_project) = resolve_role_toml_path(&project, &global, "pm");
        assert!(!is_project);
        assert_eq!(path, gp);
        // 两边都没有 → 全局保存目标
        let (path, is_project) = resolve_role_toml_path(&project, &global, "ghost");
        assert!(!is_project);
        assert_eq!(path, global.join("ghost.toml"));
    }

    #[test]
    fn resolve_prompt_path_mirrors_runtime_order() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("ws");
        let global_root = tmp.path().join("global");
        // 1. 项目 prompt 存在 → 项目路径
        std::fs::create_dir_all(cwd.join("prompts")).unwrap();
        let project_prompt = cwd.join("prompts/pm.md");
        std::fs::write(&project_prompt, "project").unwrap();
        assert_eq!(
            resolve_prompt_path(&cwd, Some(&global_root), "prompts/pm.md", false),
            project_prompt
        );
        // 2. 项目没有、全局 prompts.d 有 → 全局路径（即使角色在项目层）
        std::fs::remove_file(&project_prompt).unwrap();
        std::fs::create_dir_all(global_root.join("prompts.d")).unwrap();
        let global_prompt = global_root.join("prompts.d/pm.md");
        std::fs::write(&global_prompt, "global").unwrap();
        assert_eq!(
            resolve_prompt_path(&cwd, Some(&global_root), "prompts/pm.md", true),
            global_prompt
        );
        // 3. 都没有 → 项目角色给项目保存目标，全局角色给全局保存目标
        std::fs::remove_file(&global_prompt).unwrap();
        assert_eq!(
            resolve_prompt_path(&cwd, Some(&global_root), "prompts/pm.md", true),
            project_prompt
        );
        assert_eq!(
            resolve_prompt_path(&cwd, Some(&global_root), "prompts/pm.md", false),
            global_prompt
        );
        // 4. 绝对路径不做全局回退
        let abs = tmp.path().join("abs.md");
        assert_eq!(
            resolve_prompt_path(&cwd, Some(&global_root), abs.to_str().unwrap(), false),
            abs
        );
    }

    #[test]
    fn read_role_prompt_falls_back_to_global_prompts_d() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("ws");
        let global_root = tmp.path().join("global");
        std::fs::create_dir_all(global_root.join("prompts.d")).unwrap();
        std::fs::write(global_root.join("prompts.d/custom_role.md"), "global prompt").unwrap();
        let tpl = latte_agent_core::role::RoleTemplate {
            id: "custom_role".into(),
            name: "Custom".into(),
            category: "custom".into(),
            model_tier: "standard".into(),
            model_chain: vec![],
            prompt_file: Some("prompts/custom_role.md".into()),
            temperature: None,
            tools: vec![],
            icon: String::new(),
            skills: vec![],
            code_paths: vec![],
        };
        // cwd 下没有文件 → 读全局 prompts.d
        assert_eq!(
            read_role_prompt(&cwd, Some(&global_root), &tpl),
            "global prompt"
        );
        // cwd 下有了 → 项目优先
        std::fs::create_dir_all(cwd.join("prompts")).unwrap();
        std::fs::write(cwd.join("prompts/custom_role.md"), "project prompt").unwrap();
        assert_eq!(
            read_role_prompt(&cwd, Some(&global_root), &tpl),
            "project prompt"
        );
    }

    /// 构造一个只含注入角色（无磁盘层）的测试 backend。
    fn test_backend(cwd: &Path) -> UiBackend {
        let mut cfg = AgentConfig::default();
        cfg.roles.insert(
            "reload_ghost_9f3b".to_string(),
            latte_agent_core::role::RoleTemplate {
                id: "reload_ghost_9f3b".into(),
                name: "Ghost".into(),
                category: "custom".into(),
                model_tier: "standard".into(),
                model_chain: vec!["stale-model".into()],
                prompt_file: None,
                temperature: None,
                tools: vec![],
                icon: String::new(),
                skills: vec![],
            code_paths: vec![],
            },
        );
        let resolver = latte_agent_core::model_resolver::ModelResolver::from_config(&cfg)
            .expect("resolver");
        UiBackend::new(crate::UiBackendConfig {
            agent_config: cfg,
            model_resolver: resolver,
            role: None,
            tier: None,
            model_id: None,
            cwd: Some(cwd.to_path_buf()),
            agents_config: ".latte/agents.d".into(),
        })
        .expect("backend")
    }

    #[test]
    fn reload_roles_picks_up_external_file_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("ws");
        let agents = cwd.join(".latte/agents.d");
        std::fs::create_dir_all(&agents).unwrap();
        let role_file = agents.join("pm.toml");
        std::fs::write(
            &role_file,
            "[roles.pm]\nid = \"pm\"\nname = \"PM\"\ncategory = \"management\"\nmodel_tier = \"standard\"\nmodel_chain = [\"a\", \"b\"]\n",
        )
        .unwrap();
        let b = test_backend(&cwd);
        let no_global = tmp.path().join("no-such-global");

        // 首次重载：磁盘角色替换内存注入角色
        reload_roles_from_disk_with(&b, &no_global);
        {
            let cfg = b.merged.read();
            assert_eq!(cfg.roles["pm"].model_chain, vec!["a", "b"]);
            assert!(!cfg.roles.contains_key("reload_ghost_9f3b"));
        }
        // 外部编辑器改了文件 → 再次重载后表单数据随之更新
        std::fs::write(
            &role_file,
            "[roles.pm]\nid = \"pm\"\nname = \"PM\"\ncategory = \"management\"\nmodel_tier = \"standard\"\nmodel_chain = [\"c\"]\n",
        )
        .unwrap();
        reload_roles_from_disk_with(&b, &no_global);
        assert_eq!(b.merged.read().roles["pm"].model_chain, vec!["c"]);
    }

    #[test]
    fn reload_roles_keeps_memory_when_no_disk_layers() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("ws");
        std::fs::create_dir_all(&cwd).unwrap();
        let b = test_backend(&cwd);
        reload_roles_from_disk_with(&b, &tmp.path().join("no-such-global"));
        // 磁盘两层都没有角色 → 内存配置原样保留
        assert!(b.merged.read().roles.contains_key("reload_ghost_9f3b"));
    }

    #[test]
    fn reload_roles_global_fills_empty_project_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("ws");
        let agents = cwd.join(".latte/agents.d");
        std::fs::create_dir_all(&agents).unwrap();
        // 项目层有 pm 但 model_chain 为空
        std::fs::write(
            agents.join("pm.toml"),
            "[roles.pm]\nid = \"pm\"\nname = \"PM\"\ncategory = \"management\"\nmodel_tier = \"standard\"\n",
        )
        .unwrap();
        // 全局层同 id 带 model_chain → 应补齐
        let global = tmp.path().join("global");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(
            global.join("pm.toml"),
            "[roles.pm]\nid = \"pm\"\nname = \"PM Global\"\ncategory = \"management\"\nmodel_tier = \"standard\"\nmodel_chain = [\"g1\"]\nicon = \"📋\"\n",
        )
        .unwrap();
        let b = test_backend(&cwd);
        reload_roles_from_disk_with(&b, &global);
        let cfg = b.merged.read();
        let pm = &cfg.roles["pm"];
        assert_eq!(pm.model_chain, vec!["g1"]);
        assert_eq!(pm.icon, "📋");
        // 项目层已声明的字段不被全局覆盖
        assert_eq!(pm.name, "PM");
    }
    /// 角色编辑器接收的 `available_models` 应来自合并后的 catalog，
    /// 排序按 (provider, name)，来源标签按磁盘扫描结果标注。
    /// 用隔离的 LATTE_HOME，避免测试环境真实的 `~/.latte/models.d`
    /// 全局层污染结果。
    #[test]
    fn enumerate_available_models_returns_sorted_merged_catalog() {
        with_isolated_home(|| {
            let tmp = tempfile::tempdir().unwrap();
            let cwd = tmp.path().join("ws");
            std::fs::create_dir_all(&cwd).unwrap();
            let b = test_backend(&cwd);
            // 注入三个 catalog model（无磁盘文件 → source = "catalog"）。
            let new_models = vec![
                ModelDef {
                    name: "zeta".into(),
                    api: "openai".into(),
                    provider: "openai".into(),
                    base_url: "https://x".into(),
                    api_key: "k".into(),
                    context_window: 1,
                    max_tokens: 1,
                    supports_thinking: false,
                    supports_vision: false,
                    supports_image_generation: false,
                    cost_per_million_input: None,
                    cost_per_million_output: None,
                    tier: None,
                    timeout_secs: None,
                },
                ModelDef {
                    name: "alpha".into(),
                    api: "anthropic".into(),
                    provider: "anthropic".into(),
                    base_url: "https://x".into(),
                    api_key: "k".into(),
                    context_window: 1,
                    max_tokens: 1,
                    supports_thinking: false,
                    supports_vision: false,
                    supports_image_generation: false,
                    cost_per_million_input: None,
                    cost_per_million_output: None,
                    tier: None,
                    timeout_secs: None,
                },
                ModelDef {
                    name: "beta".into(),
                    api: "openai".into(),
                    provider: "openai".into(),
                    base_url: "https://x".into(),
                    api_key: "k".into(),
                    context_window: 1,
                    max_tokens: 1,
                    supports_thinking: false,
                    supports_vision: false,
                    supports_image_generation: false,
                    cost_per_million_input: None,
                    cost_per_million_output: None,
                    tier: None,
                    timeout_secs: None,
                },
            ];
            b.merged.write().models.models = new_models;
            let out = enumerate_available_models(&b);
            // 按 provider 后按 name 排序：anthropic/alpha 在最前（provider 字典序）
            // openai 的 alpha (beta) 和 zeta 按字母序紧随其后。
            let names: Vec<&str> = out.iter().map(|m| m.name.as_str()).collect();
            assert_eq!(names, vec!["alpha", "beta", "zeta"]);
            // 全部 source = "catalog"（无磁盘文件），key = provider/name。
            for m in &out {
                assert_eq!(m.source, "catalog", "{:?}", m);
                assert_eq!(m.key, format!("{}/{}", m.provider, m.name));
            }
        });
    }

    /// 项目 `.latte/models.d/<provider>__<id>.toml` 存在时，对应 model
    /// 的 source 标签应为 "project"。用隔离的 LATTE_HOME 避免真实
    /// 全局层污染。
    #[test]
    fn enumerate_available_models_labels_disk_source_project() {
        with_isolated_home(|| {
            let tmp = tempfile::tempdir().unwrap();
            let cwd = tmp.path().join("ws");
            std::fs::create_dir_all(cwd.join(".latte/models.d")).unwrap();
            let project_path = cwd.join(".latte/models.d/openai__alpha.toml");
            std::fs::write(
                &project_path,
                r#"name = "alpha"
api = "openai"
provider = "openai"
base_url = "https://x"
api_key = "k"
context_window = 1
max_tokens = 1
"#,
            )
            .unwrap();
            let b = test_backend(&cwd);
            b.merged.write().models.models = vec![ModelDef {
                name: "alpha".into(),
                api: "openai".into(),
                provider: "openai".into(),
                base_url: "https://x".into(),
                api_key: "k".into(),
                context_window: 1,
                max_tokens: 1,
                supports_thinking: false,
                supports_vision: false,
                supports_image_generation: false,
                cost_per_million_input: None,
                cost_per_million_output: None,
                tier: None,
                timeout_secs: None,
            }];
            let out = enumerate_available_models(&b);
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].name, "alpha");
            assert_eq!(out[0].provider, "openai");
            assert_eq!(out[0].source, "project");
            assert_eq!(out[0].key, "openai/alpha");
        });
    }
}

// ─── Logs ────────────────────────────────────────────────────────

/// 日志条目：一行 ui-session 日志文件的内容。
#[derive(Serialize)]
pub struct LogEntry {
    /// 日志文件名（如 `ui-536900-1784574560320.jsonl`）
    pub file: String,
    /// 日志文件大小（字节）
    pub size: u64,
    /// 日志文件修改时间（unix 毫秒）
    pub modified: u64,
    /// 日志内容行（最多返回 last_n 行）
    pub lines: Vec<String>,
}

/// `GET /api/logs` — 列出 `<cwd>/.latte/ui-sessions/` 下的日志文件。
/// 支持 `?file=<name>&tail=30` 参数读取指定文件尾部。
pub fn list_logs(b: &UiBackend, file: Option<&str>, tail: Option<usize>) -> Result<Vec<LogEntry>, ApiError> {
    let sessions_dir = b.cwd.join(".latte").join("ui-sessions");
    if !sessions_dir.exists() {
        return Ok(vec![]);
    }
    let tail_n = tail.unwrap_or(20);
    let mut entries: Vec<LogEntry> = Vec::new();

    let mut dir_entries: Vec<_> = match std::fs::read_dir(&sessions_dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(_) => return Ok(vec![]),
    };
    dir_entries.sort_by_key(|e| e.path().to_string_lossy().to_string());

    for entry in dir_entries {
        let path = entry.path();
        if path.extension().map_or(true, |ext| ext != "jsonl") {
            continue;
        }
        let file_name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };

        // 如果指定了具体文件，只读取那个文件
        if let Some(target) = file {
            if file_name != target {
                continue;
            }
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let all_lines: Vec<&str> = content.lines().collect();
            let total = all_lines.len();
            let start = if total > tail_n { total - tail_n } else { 0 };
            let lines: Vec<String> = all_lines[start..]
                .iter()
                .enumerate()
                .map(|(i, l)| {
                    let line_num = start + i + 1;
                    format!("{line_num}: {l}")
                })
                .collect();
            let modified = meta.modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            return Ok(vec![LogEntry {
                file: file_name,
                size: meta.len(),
                modified,
                lines,
            }]);
        }

        // 否则只列出文件元信息（不读内容）
        let modified = meta.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        entries.push(LogEntry {
            file: file_name,
            size: meta.len(),
            modified,
            lines: vec![],
        });
    }

    Ok(entries)
}


// ─── Tools（工具管理） ─────────────────────────────────────────────

/// `GET /api/tools` 的返回：所有工具及其启用/禁用状态。
#[derive(Serialize)]
pub struct ToolsListResponse {
    pub tools: Vec<crate::tools::ToolEntry>,
    /// 当前被禁用的工具 ID 列表（方便 UI 快速判断全选状态）。
    pub disabled: Vec<String>,
}

/// `GET /api/tools` — 列出所有可用工具，标记每个工具的启用/禁用状态。
/// `enumerate()` 总是立即返回（预热完成前返回 fallback 列表），不阻塞。
pub async fn list_tools(b: &UiBackend) -> Result<ToolsListResponse, ApiError> {
    let mut tools = crate::tools::enumerate()
        .await
        .map_err(|e| ApiError::internal(format!("enumerate tools: {e}")))?;
    let store = crate::tools::ToolsStore::new(&b.cwd);
    for tool in &mut tools {
        tool.enabled = store.is_enabled(&tool.id);
    }
    let disabled: Vec<String> = tools.iter()
        .filter(|t| !t.enabled)
        .map(|t| t.id.clone())
        .collect();
    Ok(ToolsListResponse { tools, disabled })
}

/// `POST /api/tools/:id/toggle` — 切换工具的启用/禁用状态。
/// 返回切换后的 enabled 值。
pub fn toggle_tool(b: &UiBackend, id: &str, enabled: bool) -> Result<bool, ApiError> {
    let mut store = crate::tools::ToolsStore::new(&b.cwd);
    store
        .set_enabled(id, enabled)
        .map_err(|e| ApiError::internal(format!("toggle tool: {e}")))
}
// ─── Models（模型 CRUD） ──────────────────────────────────────────
//
// 读源：`UiBackend.merged: Arc<RwLock<AgentConfig>>` —— 由
// `config_layer::load` / `load_cli_like_agent_config` 在 `UiBackend::new`
// 之前已合并完项目 + 全局 catalog（含 `~/.latte/models.d/*.toml` 按厂商
// 拆分的所有 model）。这里直接序列化出去，不再二次扫描磁盘，避免
// `GlobalConfig::load_default` 与本端扫描逻辑出现分歧。
// （历史：早期 UI server 自己扫 `models.d/` 写过一份
// `ModelsState` + `load_dir`，与 `GlobalConfig` 的 [[models]] 数组 / 顶层
// `models:` 语义不一致，导致用户文件存在但 UI 显示"暂无 model"——
// 详见 git log 中那段 `[[models]]` 数组回归测试。）

/// `GET /api/models` 返回：合并后所有 model + 来源 + tier 映射 + 路径提示。
#[derive(Serialize)]
pub struct ModelsListResponse {
    pub models: Vec<ModelWithSource>,
    pub tiers: BTreeMap<String, String>,
    pub project_models_dir: String,
    pub global_models_dir: String,
}

/// 单条 model + 来源标识（用于 UI 区分"项目覆盖"与"全局继承"）。
#[derive(Serialize)]
pub struct ModelWithSource {
    /// composite_key = `provider/model_name`，UI 表里当主键。
    pub key: String,
    /// `"project"` / `"global"`：来自项目 `.latte/models.d/` 还是
    /// 全局 `~/.latte/models.d/`。项目覆盖同名时取 `"project"`。
    pub source: String,
    /// 空字符串表示该 model 只在内存 catalog 里（新加但还没保存）。
    /// UI 用它显示「文件位置」与定位保存目标。
    pub file_path: String,
    /// 透传 ModelDef 全部字段（name / api / provider / ...）。
    #[serde(flatten)]
    pub def: ModelDef,
}

/// PATCH 请求体（partial update）：只把 body 里**出现**的字段合并进已有
/// model，缺省字段保持磁盘/catalog 现值不变 —— 语义对齐 oh-my-pi
/// `writeMCPConfigFile` 的 `{ ...existing, ...updates }` 合并 + 原子写。
/// UI 现在既可以整表提交（改完点保存，所有字段都在），也可以只 PATCH
/// 单个字段（如仅改 `base_url` / `api_key`）而不必回传其它字段。
///
/// `target`: "project"（默认，写到 `<cwd>/.latte/models.d/`）或 "global"
/// （写到 `~/.latte/models.d/`）。两个写盘目录分别对应 UI 上的
/// "保存到项目" / "保存到全局" 两个按钮。
///
/// 说明：可选字段（cost / tier / timeout_secs）只支持“设新值”，不支持通过
/// PATCH 显式清空回 `null`（缺省即保持原值）。需要清空这些字段时走整表
/// 重写：`POST /api/models`。
#[derive(Debug, Default, Deserialize)]
pub struct UpdateModelRequest {
    #[serde(default)]
    pub target: String,
    #[serde(flatten)]
    pub patch: ModelPatch,
}

/// 单个 model 的部分字段补丁：全字段 `Option`，`Some` 覆盖、`None` 保持。
/// 字段名 / alias 与 [`ModelDef`] 一一对应，所以整表提交也能无损落进来。
#[derive(Debug, Default, Deserialize)]
pub struct ModelPatch {
    #[serde(default, alias = "model_name")]
    pub name: Option<String>,
    #[serde(default)]
    pub api: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub supports_thinking: Option<bool>,
    #[serde(default)]
    pub supports_vision: Option<bool>,
    #[serde(default)]
    pub supports_image_generation: Option<bool>,
    #[serde(default)]
    pub cost_per_million_input: Option<f64>,
    #[serde(default)]
    pub cost_per_million_output: Option<f64>,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

impl ModelPatch {
    /// 把补丁里 `Some` 的字段合并进 `def`（`None` 字段保持不变）。
    /// 对应 oh-my-pi `{ ...existing, ...updates }`：只覆盖显式给出的键。
    fn apply_to(self, def: &mut ModelDef) {
        if let Some(v) = self.name {
            def.name = v;
        }
        if let Some(v) = self.api {
            def.api = v;
        }
        if let Some(v) = self.provider {
            def.provider = v;
        }
        if let Some(v) = self.base_url {
            def.base_url = v;
        }
        if let Some(v) = self.api_key {
            def.api_key = v;
        }
        if let Some(v) = self.context_window {
            def.context_window = v;
        }
        if let Some(v) = self.max_tokens {
            def.max_tokens = v;
        }
        if let Some(v) = self.supports_thinking {
            def.supports_thinking = v;
        }
        if let Some(v) = self.supports_vision {
            def.supports_vision = v;
        }
        if let Some(v) = self.supports_image_generation {
            def.supports_image_generation = v;
        }
        if let Some(v) = self.cost_per_million_input {
            def.cost_per_million_input = Some(v);
        }
        if let Some(v) = self.cost_per_million_output {
            def.cost_per_million_output = Some(v);
        }
        if let Some(v) = self.tier {
            def.tier = Some(v);
        }
        if let Some(v) = self.timeout_secs {
            def.timeout_secs = Some(v);
        }
    }
}

/// `POST /api/models` 的请求体：新建 model 并写盘。
/// `target`: "project"（默认，写到 `<cwd>/.latte/models.d/`）或 "global"
/// （写到 `~/.latte/models.d/`），与 UpdateModelRequest 语义一致。
#[derive(Deserialize)]
pub struct CreateModelRequest {
    #[serde(default)]
    pub target: String,
    #[serde(flatten)]
    pub def: ModelDef,
}
pub fn list_models(b: &UiBackend) -> Result<ModelsListResponse, ApiError> {
    let cfg = b.merged.read();
    // 项目目录 = `<cwd>/.latte/models.d`，与 cli `chat --models-d` 默认值一致。
    let project_dir = b.cwd.join(".latte/models.d");
    // 合并后的 catalog 已经是项目层合并 + 全局 `~/.latte/models.d/*.toml`
    // 全量按厂商拆分文件（见 `GlobalConfig::load_default`）。这里
    // 再独立扫一次磁盘拿 *实际文件路径* —— `agent_config` 里不带
    // 文件位置信息，只能用 `ModelsState::load` 反查每个 model 实际
    // 落在哪个 .toml 文件里。两个扫描都很小（<10 个文件），开销可
    // 忽略；为了正确性值得做。
    let on_disk =
        crate::models::ModelsState::load(&project_dir, &global_dir_fallback())
            .map_err(|e| ApiError::internal(format!("scan models.d: {e}")))?;
    // 磁盘上每个 model 文件一条记录（不去重）：同一 `provider/name`
    // 在项目层与全局层各有一个文件时，两条都展示（UI 能区分编辑）。
    // 内存 catalog 里但磁盘上不存在的（catalog 源）追加在末尾。
    let mut seen_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut models: Vec<ModelWithSource> = Vec::new();
    for (key, recs) in &on_disk.entries {
        for (src, path, def) in recs {
            models.push(ModelWithSource {
                key: key.clone(),
                source: source_label(*src).to_string(),
                file_path: path.display().to_string(),
                def: def.clone(),
            });
            seen_keys.insert(key.clone());
        }
    }
    for def in &cfg.models.models {
        let key = if def.name.contains('/') {
            def.name.clone()
        } else {
            format!("{}/{}", def.provider, def.name)
        };
        if seen_keys.contains(&key) {
            continue;
        }
        models.push(ModelWithSource {
            key,
            source: "catalog".to_string(),
            file_path: String::new(),
            def: def.clone(),
        });
    }
    // 稳定排序：provider → name → source → path，保证 UI 下拉框顺序稳定。
    models.sort_by(|a, b| {
        (&a.def.provider, &a.def.name, &a.source, &a.file_path)
            .cmp(&(&b.def.provider, &b.def.name, &b.source, &b.file_path))
    });
    let tiers: BTreeMap<String, String> = cfg
        .models
        .tiers
        .as_ref()
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    let global_dir = global_dir_fallback();
    Ok(ModelsListResponse {
        models,
        tiers,
        project_models_dir: project_dir.display().to_string(),
        global_models_dir: global_dir.display().to_string(),
    })
}


/// 全局目录 `<HOME>/.latte/models.d/` 的解析逻辑，集中到这里避免
/// `list_models` 内嵌三层 Option 链。
fn global_dir_fallback() -> PathBuf {
    latte_agent_core::config::ConfigLayer::Global
        .root_dir()
        .map(|d| d.join("models.d"))
        .unwrap_or_else(|| {
            std::env::var("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_default()
                .join(".latte/models.d")
        })
}

/// `ModelSource` 枚举 → UI 字符串。`#[serde(rename_all = "snake_case")]` 已经
/// 在 `ModelSource` 上标注，直接 `as_str` 不到；自己手动映射。
fn source_label(s: crate::models::ModelSource) -> &'static str {
    match s {
        crate::models::ModelSource::Project => "project",
        crate::models::ModelSource::Global => "global",
    }
}

/// `PATCH /api/models/:key` —— **部分更新**一个已存在的 model：以 catalog
/// 里的现值为基准，只覆盖补丁里显式给出的字段，再写回磁盘（项目目录
/// 模型/角色配置变更（models 的 create/update/delete/put_toml、roles
/// 的保存/新建/删除/磁盘重载）落盘并更新 `merged` 后调用：把新配置
/// 热替换进共享 `ModelResolver`（代际 +1）。之后新 session、下一个
/// workflow step 分派、下一次 delegate、长存 runner 的 turn 边界
/// 自查都会自动用新配置——无需重启 server。
fn hot_reload_resolver(b: &UiBackend) {
    let cfg = b.merged.read().clone();
    if let Err(e) = b.resolver.reload_from_config(&cfg) {
        eprintln!("[models] reload resolver after config save failed: {e}");
    }
}

/// `<cwd>/.latte/models.d/<provider>__<id>.toml`），同时就地更新内存
/// catalog 让后续 chat 立刻看到新值。
///
/// 合并语义参考 oh-my-pi 的 `updateMCPServer`：先读现有配置，`{ ...existing,
/// ...updates }` 合并，再原子写回（写 `.tmp` 后 `rename`，见
/// [`ModelsState::write_project`]）。
///
/// 只能改**已存在**的 model（不在 catalog 里返回 404）；新建走
/// `POST /api/models`。
///
/// 写入策略：由 request 的 `target` 决定写到项目层还是全局层；在目标层
/// 内优先**写回该 model 实际所在的文件**（例如全局 `glm.toml` 厂商拆分
/// 文件只更新对应条目，保留同文件其它 model），目标层没有此 key 的
/// 文件时才新建 `<provider>__<id>.toml`。见
/// [`ModelsState::write_to_layer`]。旧文件只在全局而 target=project 时，
/// 会落到项目目录，相当于把全局 model 提升到项目层。
pub fn update_model(
    b: &UiBackend,
    key: &str,
    target: &str,
    patch: ModelPatch,
) -> Result<ModelWithSource, ApiError> {
    // target: "project" (默认，写到 `<cwd>/.latte/models.d/`) 或 "global"
    // （写到 `~/.latte/models.d/`，与 GlobalConfig::load_default 同源）。
    // 这两个按钮（保存到项目 / 保存到全局）共用一条路由，目标由 request
    // body 的 `target` 字段决定。先解析 target，磁盘回退时要按层挑基准值。
    let (write_dir, source_label) = match target {
        "global" => (
            crate::models::ModelsState::global_models_dir(),
            "global",
        ),
        "project" | "" => (b.cwd.join(".latte/models.d"), "project"),
        other => {
            return Err(ApiError::bad_request(format!(
                "unknown target {:?}（仅 project / global）",
                other
            )));
        }
    };
    // 以 catalog 现值为基准（catalog 已是项目层合并 + 全局的最终生效值）。
    // 内存 catalog 是 server 启动/上次保存时的快照，磁盘上后加的文件
    // （如用户手动写的 `~/.latte/models.d/glm.toml`）可能不在里面 ——
    // 此时回退到磁盘扫描（与 `GET /api/models` 列表同源），按目标层
    // 优先取该层的记录作基准。都找不到才 404，让 UI 走 POST 新建。
    let mut def = {
        let cfg = b.merged.read();
        cfg.models
            .models
            .iter()
            .find(|m| format!("{}/{}", m.provider, m.name) == key)
            .cloned()
    };
    if def.is_none() {
        let project_dir = b.cwd.join(".latte/models.d");
        let on_disk =
            crate::models::ModelsState::load(&project_dir, &global_dir_fallback())
                .map_err(|e| ApiError::internal(format!("scan models.d: {e}")))?;
        let prefer = if source_label == "global" {
            crate::models::ModelSource::Global
        } else {
            crate::models::ModelSource::Project
        };
        def = on_disk.entries.get(key).and_then(|recs| {
            recs.iter()
                .find(|(s, _, _)| *s == prefer)
                .or(recs.first())
                .map(|(_, _, d)| d.clone())
        });
    }
    let mut def = def.ok_or_else(|| {
        ApiError::not_found(format!(
            "model {key:?} 不在 catalog 里；PATCH 只能改已存在的 model，新建请用 POST /api/models"
        ))
    })?;
    // 合并补丁：只覆盖显式给出的字段（None 保持原值）。
    patch.apply_to(&mut def);
    // 合并后再整体校验，保证落盘的一定是一份完整合法的 ModelDef。
    crate::models::validate(&def)
        .map_err(|e| ApiError::bad_request(format!("validate: {e}")))?;
    let new_key = format!("{}/{}", def.provider, def.name);
    if new_key != key {
        return Err(ApiError::bad_request(format!(
            "composite_key 不能改：原 key={key:?}, 新 key={new_key:?}。请用 PATCH 不带改 key，或先 DELETE 再 POST。"
        )));
    }
    // 写盘：优先写回该层里 model 实际所在的文件（如全局 `glm.toml`
    // 厂商拆分文件只更新对应条目）；该层没有此 key 的文件时才新建
    // `<provider>__<id>.toml`。见 [`ModelsState::write_to_layer`]。
    let path = crate::models::ModelsState::write_to_layer(&write_dir, key, &def)
        .map_err(|e| ApiError::internal(format!("write: {e}")))?;
    // 更新内存 catalog：新值覆盖；若原 model 是从其它目录继承来的，
    // 这里也追加进 catalog（写到哪里就在内存里出现一份）。
    {
        let mut cfg = b.merged.write();
        if let Some(slot) = cfg.models.models.iter_mut().find(|m| {
            format!("{}/{}", m.provider, m.name) == key
        }) {
            *slot = def.clone();
        } else {
            cfg.models.models.push(def.clone());
        }
    }
    hot_reload_resolver(b);
    Ok(ModelWithSource {
        key: key.to_string(),
        source: source_label.to_string(),
        file_path: path.display().to_string(),
        def,
    })
}

/// `DELETE /api/models/:key?source=project|global` — 删除 model 文件并从
/// 内存 catalog 移除。
///
/// `source` 精确指定删除哪一层：同一 `provider/name` 在项目层与全局层
/// 可能各有一个文件（同名多条），只删指定层的那份，另一层保留。
/// 缺省时向后兼容旧行为：项目目录优先，只删项目层（不碰全局）——
/// 全局模型如果项目层没有同名文件，则删全局那份。
pub fn delete_model(b: &UiBackend, key: &str, source: Option<&str>) -> Result<(), ApiError> {
    let project_dir = b.cwd.join(".latte/models.d");
    let global_dir = crate::models::ModelsState::global_models_dir();
    // 按 source 精确删一层；source 缺失时兼容旧语义：
    // 先试项目，项目没有同名文件再试全局。
    let mut deleted_project = false;
    let mut deleted_global = false;
    match source {
        Some("project") => {
            deleted_project = crate::models::ModelsState::delete_project(&project_dir, key)
                .map_err(|e| ApiError::internal(format!("delete project model: {e}")))?;
        }
        Some("global") => {
            deleted_global = crate::models::ModelsState::delete_project(&global_dir, key)
                .map_err(|e| ApiError::internal(format!("delete global model: {e}")))?;
        }
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "unknown source {:?}（仅 project / global）",
                other
            )));
        }
        None => {
            deleted_project = crate::models::ModelsState::delete_project(&project_dir, key)
                .map_err(|e| ApiError::internal(format!("delete project model: {e}")))?;
            if !deleted_project {
                deleted_global = crate::models::ModelsState::delete_project(&global_dir, key)
                    .map_err(|e| ApiError::internal(format!("delete global model: {e}")))?;
            }
        }
    }
    if !deleted_project && !deleted_global {
        return Err(ApiError::not_found(format!("model {key:?} not found")));
    }
    // 从内存 catalog 移除：删项目层时移除 catalog 里的同名条目（项目层
    // 是覆盖源，删掉后该 key 不应再以项目配置存在）；删全局层且项目层
    // 无同名文件时同样移除。同名多条的其余记录（另一层文件）不在此
    // 处处理 —— 磁盘扫描（`list_models` 的 entries）会如实反映剩余文件。
    {
        let mut cfg = b.merged.write();
        cfg.models.models.retain(|m| {
            format!("{}/{}", m.provider, m.name) != key
        });
    }
    hot_reload_resolver(b);
    Ok(())
}

/// `POST /api/models` — 创建新 model，写盘到项目或全局目录并加入内存 catalog。
/// 同名 key 已存在时覆盖旧值（与 update_model 的 append-or-overwrite 语义一致）。
pub fn create_model(b: &UiBackend, req: CreateModelRequest) -> Result<ModelWithSource, ApiError> {
    crate::models::validate(&req.def)
        .map_err(|e| ApiError::bad_request(format!("validate: {e}")))?;
    let key = format!("{}/{}", req.def.provider, req.def.name);
    let (write_dir, src_label) = match req.target.as_str() {
        "global" => (
            crate::models::ModelsState::global_models_dir(),
            "global",
        ),
        "project" | "" => (b.cwd.join(".latte/models.d"), "project"),
        other => {
            return Err(ApiError::bad_request(format!(
                "unknown target {:?}（仅 project / global）",
                other
            )));
        }
    };
    // 写盘：与 update_model 一致 —— 该层已有包含此 key 的文件就原地
    // 更新，否则新建 `<provider>__<id>.toml`。
    let path = crate::models::ModelsState::write_to_layer(&write_dir, &key, &req.def)
        .map_err(|e| ApiError::internal(format!("write: {e}")))?;
    // 加入内存 catalog：同名 key 覆盖；不存在则追加。
    {
        let mut cfg = b.merged.write();
        if let Some(slot) = cfg.models.models.iter_mut().find(|m| {
            format!("{}/{}", m.provider, m.name) == key
        }) {
            *slot = req.def.clone();
        } else {
            cfg.models.models.push(req.def.clone());
        }
    }
    hot_reload_resolver(b);
    Ok(ModelWithSource {
        key,
        source: src_label.to_string(),
        file_path: path.display().to_string(),
        def: req.def,
    })
}

#[cfg(test)]
mod list_models_tests {

    #[test]
    fn key_shape_is_provider_over_id() {
        // 不构造 UiBackend，直接验证 key 格式化逻辑：
        //   - id 含 `/` 时直接当 key（兼容 anthropic/claude-opus-... 这种预分割 id）
        //   - 否则补 `provider/id`
        let id_with_slash = "anthropic/claude-opus-4-20250514";
        let id_plain = "deepseek-v4-pro";
        let provider = "deepseek";

        let k1 = if id_with_slash.contains('/') {
            id_with_slash.to_string()
        } else {
            format!("{provider}/{id_with_slash}")
        };
        assert_eq!(k1, "anthropic/claude-opus-4-20250514");

        let k2 = if id_plain.contains('/') {
            id_plain.to_string()
        } else {
            format!("{provider}/{id_plain}")
        };
        assert_eq!(k2, "deepseek/deepseek-v4-pro");
    }
}

#[cfg(test)]
mod model_patch_tests {
    use super::*;

    fn base_def() -> ModelDef {
        ModelDef {
            name: "gpt-4o".into(),
            api: "openai".into(),
            provider: "openai".into(),
            base_url: "https://api.openai.com".into(),
            api_key: "sk-old".into(),
            context_window: 128_000,
            max_tokens: 4096,
            supports_thinking: false,
            supports_vision: true,
            supports_image_generation: false,
            cost_per_million_input: Some(2.5),
            cost_per_million_output: Some(10.0),
            tier: Some("standard".into()),
            timeout_secs: Some(60),
        }
    }

    #[test]
    fn patch_only_touches_present_fields() {
        // 只 PATCH base_url + api_key，其它字段必须原样保留。
        let mut def = base_def();
        let patch = ModelPatch {
            base_url: Some("https://proxy.local".into()),
            api_key: Some("sk-new".into()),
            ..Default::default()
        };
        patch.apply_to(&mut def);

        assert_eq!(def.base_url, "https://proxy.local");
        assert_eq!(def.api_key, "sk-new");
        // 未提及的字段保持不变。
        assert_eq!(def.name, "gpt-4o");
        assert_eq!(def.context_window, 128_000);
        assert_eq!(def.max_tokens, 4096);
        assert_eq!(def.supports_vision, true);
        assert_eq!(def.cost_per_million_input, Some(2.5));
        assert_eq!(def.tier.as_deref(), Some("standard"));
        assert_eq!(def.timeout_secs, Some(60));
    }

    #[test]
    fn empty_patch_is_a_noop() {
        let mut def = base_def();
        let before = def.clone();
        ModelPatch::default().apply_to(&mut def);
        assert_eq!(def.base_url, before.base_url);
        assert_eq!(def.api_key, before.api_key);
        assert_eq!(def.timeout_secs, before.timeout_secs);
    }

    #[test]
    fn full_form_submit_deserializes_via_flatten() {
        // 整表提交（所有字段 + target）必须仍能落进 UpdateModelRequest。
        let json = serde_json::json!({
            "target": "project",
            "model_name": "gpt-4o",
            "api": "openai",
            "provider": "openai",
            "base_url": "https://api.openai.com",
            "api_key": "sk-x",
            "context_window": 128000,
            "max_tokens": 4096,
            "supports_vision": true,
            "tier": "standard",
            "timeout_secs": 90
        });
        let req: UpdateModelRequest = serde_json::from_value(json).expect("deserialize");
        assert_eq!(req.target, "project");
        // `model_name` alias 落到 name。
        assert_eq!(req.patch.name.as_deref(), Some("gpt-4o"));
        assert_eq!(req.patch.timeout_secs, Some(90));

        // 应用到一个 base，验证整表提交等价于全字段覆盖。
        let mut def = base_def();
        def.name = "old".into();
        req.patch.apply_to(&mut def);
        assert_eq!(def.name, "gpt-4o");
        assert_eq!(def.timeout_secs, Some(90));
    }

    #[test]
    fn partial_patch_body_only_has_changed_field() {
        // 真·partial：body 里只有一个字段。
        let json = serde_json::json!({ "base_url": "https://only.this" });
        let req: UpdateModelRequest = serde_json::from_value(json).expect("deserialize");
        assert_eq!(req.target, ""); // 缺省 target
        assert_eq!(req.patch.base_url.as_deref(), Some("https://only.this"));
        assert!(req.patch.api_key.is_none());
        assert!(req.patch.name.is_none());
    }
}

/// `GET /api/roles/:id/toml` — 读取角色 TOML 源文件原始内容。
/// 路径走 [`resolve_role_toml_path`]：项目层有该角色读项目文件，否则读全局文件。
pub fn get_role_toml(b: &UiBackend, role_id: &str) -> Result<String, ApiError> {
    let project_dir = agents_config_dir(&b.cwd, &b.agents_config);
    let (path, _) = resolve_role_toml_path(&project_dir, &global_agents_dir(), role_id);
    if !path.exists() {
        return Err(ApiError::not_found(format!(
            "role {role_id:?} toml not found (looked at {})",
            path.display()
        )));
    }
    std::fs::read_to_string(&path)
        .map_err(|e| ApiError::internal(format!("read {}: {e}", path.display())))
}

/// `PUT /api/roles/:id/toml` — 直接写入角色 TOML 源文件原始内容。
/// 写入后更新内存中的 merged 配置，使新 session 生效。
pub fn put_role_toml(b: &UiBackend, role_id: &str, raw: &str) -> Result<(), ApiError> {
    // 1. 校验 TOML 可解析
    let doc: toml_edit::DocumentMut = raw
        .parse()
        .map_err(|e| ApiError::bad_request(format!("TOML 解析失败: {e}")))?;
    // 校验存在 [roles.<id>] 节
    let role_table = doc
        .get("roles")
        .and_then(|r| r.get(role_id))
        .and_then(|r| r.as_table())
        .ok_or_else(|| {
            ApiError::bad_request(format!("TOML 中缺少 [roles.{role_id}] 节"))
        })?;
    // 可选：校验 id 字段匹配
    if let Some(v) = role_table.get("id") {
        if v.as_str() != Some(role_id) {
            return Err(ApiError::bad_request(format!(
                "TOML 中 id 字段为 {:?}，与请求角色 {role_id:?} 不匹配",
                v.as_str().unwrap_or("?"),
            )));
        }
    }

    // 2. 确定写入路径（与 GET / 表单保存共用同一套有效路径解析）
    let project_dir = agents_config_dir(&b.cwd, &b.agents_config);
    let (path, _) = resolve_role_toml_path(&project_dir, &global_agents_dir(), role_id);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| ApiError::internal(format!("create {}: {e}", dir.display())))?;
    }
    std::fs::write(&path, raw)
        .map_err(|e| ApiError::internal(format!("write {}: {e}", path.display())))?;

    let file_cfg = latte_agent_core::config::AgentConfig::load(
        path.to_str().ok_or_else(|| ApiError::internal("path not UTF-8".to_string()))?,
    )
    .map_err(|e| ApiError::internal(format!("reload {role_id}: {e}")))?;
    if let Some(tpl) = file_cfg.roles.into_iter().next().map(|(_, v)| v) {
        let mut cfg = b.merged.write();
        cfg.roles.insert(role_id.to_string(), tpl);
    } else {
        return Err(ApiError::bad_request(format!(
            "TOML 中未找到角色 {role_id:?}"
        )));
    }
    hot_reload_resolver(b);
    Ok(())
}

// ─── Models TOML 源文件编辑 ──────────────────────────────────────

/// `GET /api/models/:key/toml` — 读取模型 TOML 源文件原始内容。
/// 路径复用 `list_models` 中的 `ModelsState::load` 扫描结果，先找到
/// 文件路径再读取。只在内存中（catalog）未落盘的 model 返回 404。
pub fn get_model_toml(b: &UiBackend, key: &str) -> Result<String, ApiError> {
    let project_dir = b.cwd.join(".latte/models.d");
    let global_dir = global_dir_fallback();
    let on_disk =
        crate::models::ModelsState::load(&project_dir, &global_dir)
            .map_err(|e| ApiError::internal(format!("scan models.d: {e}")))?;
    let path = on_disk.paths.get(key).cloned().unwrap_or_default();
    if path.as_os_str().is_empty() || !path.exists() {
        return Err(ApiError::not_found(format!(
            "model {key:?} toml not found on disk"
        )));
    }
    std::fs::read_to_string(&path)
        .map_err(|e| ApiError::internal(format!("read {}: {e}", path.display())))
}

/// `PUT /api/models/:key/toml` — 直接写入模型 TOML 源文件原始内容。
/// 先校验 TOML 可解析且 key 匹配，然后写回原文件并更新内存 catalog。
pub fn put_model_toml(b: &UiBackend, key: &str, raw: &str) -> Result<(), ApiError> {
    // 1. 校验 TOML 可解析，且 provider/name 与请求 key 匹配
    let _doc: toml_edit::DocumentMut = raw
        .parse()
        .map_err(|e| ApiError::bad_request(format!("TOML 解析失败: {e}")))?;
    let def: ModelDef = toml::from_str(raw)
        .map_err(|e| ApiError::bad_request(format!("TOML 不是合法的 ModelDef: {e}")))?;
    let file_key = format!("{}/{}", def.provider, def.name);
    if file_key != key {
        return Err(ApiError::bad_request(format!(
            "TOML 中的 provider/name ({file_key:?}) 与请求 key ({key:?}) 不匹配"
        )));
    }

    // 2. 找到当前 model 文件路径
    let project_dir = b.cwd.join(".latte/models.d");
    let global_dir = global_dir_fallback();
    let on_disk =
        crate::models::ModelsState::load(&project_dir, &global_dir)
            .map_err(|e| ApiError::internal(format!("scan models.d: {e}")))?;
    let path = on_disk.paths.get(key).cloned().unwrap_or_default();
    if path.as_os_str().is_empty() || !path.exists() {
        return Err(ApiError::not_found(format!(
            "model {key:?} toml not found on disk"
        )));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| ApiError::internal(format!("create {}: {e}", dir.display())))?;
    }
    std::fs::write(&path, raw)
        .map_err(|e| ApiError::internal(format!("write {}: {e}", path.display())))?;

    // 3. 更新内存 catalog
    {
        let mut cfg = b.merged.write();
        if let Some(slot) = cfg.models.models.iter_mut().find(|m| {
            format!("{}/{}", m.provider, m.name) == key
        }) {
            *slot = def;
        } else {
            cfg.models.models.push(def);
        }
    }
    hot_reload_resolver(b);
    Ok(())
}


#[cfg(test)]
mod workflow_resume_tests {
    use super::*;

    /// 与 tasks.rs 测试同款：临时 cwd + 默认空配置构造 UiBackend。
    fn make_backend(dir: &std::path::Path) -> UiBackend {
        let cfg = latte_agent_core::AgentConfig::default();
        let resolver = latte_agent_core::ModelResolver::from_config(&cfg).expect("resolver");
        UiBackend::new(crate::UiBackendConfig {
            agent_config: cfg,
            model_resolver: resolver,
            role: None,
            tier: None,
            model_id: None,
            cwd: Some(dir.to_path_buf()),
            agents_config: ".latte/agents.d".into(),
        })
        .expect("backend")
    }

    /// 建一个 session 并插进 backend 的 session map，返回 session_id。
    async fn add_session(b: &UiBackend, sid: &str) -> String {
        let h = crate::sessions::create_session_handle(
            sid.into(),
            "manager",
            &b.merged,
            &b.resolver,
            &b.cwd,
            None,
            None,
            &b.subsession_store,
        )
        .await
        .expect("session handle");
        b.sessions.write().insert(sid.to_string(), Arc::new(h));
        sid.to_string()
    }

    /// 写一份合法 checkpoint（meta + 一个已完成 step）到
    /// `<cwd>/.latte/workflow-runs/<wf_id>.jsonl`。
    fn write_checkpoint(cwd: &std::path::Path, wf_id: &str, workflow_name: &str) {
        let dir = cwd.join(".latte/workflow-runs");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{wf_id}.jsonl")),
            format!(
                "{{\"type\":\"meta\",\"wf_id\":\"{wf_id}\",\"workflow_name\":\"{workflow_name}\",\"topic\":\"t\",\"started_at\":1}}\n\
                 {{\"type\":\"step\",\"wf_id\":\"{wf_id}\",\"workflow_name\":\"{workflow_name}\",\"topic\":\"t\",\"step_id\":\"s1\",\"output_key\":\"o1\",\"output\":\"done\",\"finished_at\":2}}\n"
            ),
        )
        .unwrap();
    }

    /// 写一份最小 workflow 定义到 `<cwd>/.latte/workflows.d/<name>.toml`。
    fn write_workflow(cwd: &std::path::Path, name: &str) {
        let dir = cwd.join(".latte/workflows.d");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{name}.toml")),
            format!(
                "name = \"{name}\"\nmax_rounds = 1\n\n[[steps]]\nid = \"s1\"\nspeakers = [\"manager\"]\nprompt = \"hi {{{{topic}}}}\"\n"
            ),
        )
        .unwrap();
    }

    /// session 不存在 → 404。
    #[tokio::test]
    async fn resume_unknown_session_404() {
        let dir = tempfile::tempdir().unwrap();
        let b = make_backend(dir.path());
        let err = workflow_resume(&b, WorkflowResumeRequest {
            session_id: "ui-ghost".into(),
            wf_id: Some("wf-x".into()),
            topic: None,
        })
        .await
        .unwrap_err();
        assert_eq!(err.status, 404);
    }

    /// wf_id 缺省且 session 没有失败快照 → 404。
    #[tokio::test]
    async fn resume_no_failed_workflow_404() {
        let dir = tempfile::tempdir().unwrap();
        let b = make_backend(dir.path());
        let sid = add_session(&b, "ui-s1").await;
        let err = workflow_resume(&b, WorkflowResumeRequest {
            session_id: sid,
            wf_id: None,
            topic: None,
        })
        .await
        .unwrap_err();
        assert_eq!(err.status, 404);
        assert!(err.message.contains("no failed workflow"));
    }

    /// 显式 wf_id 含路径穿越 → 404（load_checkpoint 的安全检查）。
    #[tokio::test]
    async fn resume_invalid_wf_id_404() {
        let dir = tempfile::tempdir().unwrap();
        let b = make_backend(dir.path());
        let sid = add_session(&b, "ui-s1").await;
        let err = workflow_resume(&b, WorkflowResumeRequest {
            session_id: sid,
            wf_id: Some("../etc/passwd".into()),
            topic: None,
        })
        .await
        .unwrap_err();
        assert_eq!(err.status, 404);
    }

    /// checkpoint 文件不存在 → 404。
    #[tokio::test]
    async fn resume_missing_checkpoint_404() {
        let dir = tempfile::tempdir().unwrap();
        let b = make_backend(dir.path());
        let sid = add_session(&b, "ui-s1").await;
        let err = workflow_resume(&b, WorkflowResumeRequest {
            session_id: sid,
            wf_id: Some("wf-ghost".into()),
            topic: None,
        })
        .await
        .unwrap_err();
        assert_eq!(err.status, 404);
    }

    /// checkpoint 有、workflow 定义没有 → 404。
    #[tokio::test]
    async fn resume_missing_workflow_def_404() {
        let dir = tempfile::tempdir().unwrap();
        let b = make_backend(dir.path());
        let sid = add_session(&b, "ui-s1").await;
        write_checkpoint(dir.path(), "wf-a", "learn-ghost");
        let err = workflow_resume(&b, WorkflowResumeRequest {
            session_id: sid,
            wf_id: Some("wf-a".into()),
            topic: None,
        })
        .await
        .unwrap_err();
        assert_eq!(err.status, 404);
    }

    /// 同 session 已有在跑的 workflow（guard flag 未置位）→ 409。
    #[tokio::test]
    async fn resume_conflict_409() {
        let dir = tempfile::tempdir().unwrap();
        let b = make_backend(dir.path());
        let sid = add_session(&b, "ui-s1").await;
        b.session_workflows
            .write()
            .insert(sid.clone(), Arc::new(AtomicBool::new(false)));
        let err = workflow_resume(&b, WorkflowResumeRequest {
            session_id: sid,
            wf_id: Some("wf-a".into()),
            topic: None,
        })
        .await
        .unwrap_err();
        assert_eq!(err.status, 409);
    }

    /// 完整启动路径：checkpoint + workflow 定义齐全 → started=true，
    /// 响应带回 checkpoint 的 wf_id 与 workflow 名；guard 已注册。
    /// （spawn 出去的 run 没有可用模型会随即失败，不影响响应断言。）
    #[tokio::test]
    async fn resume_starts_and_registers_guard() {
        let dir = tempfile::tempdir().unwrap();
        let b = make_backend(dir.path());
        let sid = add_session(&b, "ui-s1").await;
        write_checkpoint(dir.path(), "wf-a", "learn");
        write_workflow(dir.path(), "learn");
        let resp = workflow_resume(&b, WorkflowResumeRequest {
            session_id: sid.clone(),
            wf_id: Some("wf-a".into()),
            topic: None,
        })
        .await
        .expect("resume should start");
        assert_eq!(resp["started"], serde_json::json!(true));
        assert_eq!(resp["wf_id"], serde_json::json!("wf-a"));
        assert_eq!(resp["name"], serde_json::json!("learn"));
        assert!(
            b.session_workflows.read().contains_key(&sid),
            "guard 应在 spawn 前注册"
        );
    }

    /// wf_id 缺省 → 回退到 controller 的 last_failed_workflow 快照。
    #[tokio::test]
    async fn resume_falls_back_to_last_failed_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let b = make_backend(dir.path());
        let sid = add_session(&b, "ui-s1").await;
        write_checkpoint(dir.path(), "wf-snap", "learn");
        write_workflow(dir.path(), "learn");
        let h = resolve_session(&b, Some(&sid)).unwrap();
        h.try_controller()
            .expect("spawned")
            .set_last_failed_workflow(latte_agent_core::controller::FailedWorkflow {
                name: "learn".into(),
                wf_id: "wf-snap".into(),
                summary: "boom".into(),
                failed_at_unix_ms: 1,
            });
        let resp = workflow_resume(&b, WorkflowResumeRequest {
            session_id: sid,
            wf_id: None,
            topic: None,
        })
        .await
        .expect("resume via snapshot should start");
        assert_eq!(resp["wf_id"], serde_json::json!("wf-snap"));
    }

    /// ▶ 重启兜底：gate 未暂停（模拟 server 重启后新 spawn 的
    /// controller，旧 workflow task 已消亡），但有可续跑的快照 →
    /// chat_resume_session 从 checkpoint 拉起续跑。
    #[tokio::test]
    async fn chat_resume_session_falls_back_to_workflow_resume() {
        let dir = tempfile::tempdir().unwrap();
        let b = make_backend(dir.path());
        let sid = add_session(&b, "ui-s1").await;
        write_checkpoint(dir.path(), "wf-paused", "learn");
        write_workflow(dir.path(), "learn");
        let h = resolve_session(&b, Some(&sid)).unwrap();
        h.try_controller()
            .expect("spawned")
            .set_last_failed_workflow(latte_agent_core::controller::FailedWorkflow {
                name: "learn".into(),
                wf_id: "wf-paused".into(),
                summary: String::new(),
                failed_at_unix_ms: 0,
            });
        chat_resume_session(&b, Some(&sid)).await.expect("resume ok");
        assert!(
            b.session_workflows.read().contains_key(&sid),
            "兜底应注册续跑 guard（workflow 从 checkpoint 拉起）"
        );
    }

    /// gate 真的暂停着（进程内暂停）→ 走 live resume 释放 gate，
    /// 不得重复触发 checkpoint 续跑（否则同一会话跑两份 workflow）。
    #[tokio::test]
    async fn chat_resume_session_live_pause_does_not_spawn_resume() {
        let dir = tempfile::tempdir().unwrap();
        let b = make_backend(dir.path());
        let sid = add_session(&b, "ui-s1").await;
        write_checkpoint(dir.path(), "wf-paused", "learn");
        write_workflow(dir.path(), "learn");
        let h = resolve_session(&b, Some(&sid)).unwrap();
        let c = h.try_controller().expect("spawned");
        c.set_last_failed_workflow(latte_agent_core::controller::FailedWorkflow {
            name: "learn".into(),
            wf_id: "wf-paused".into(),
            summary: String::new(),
            failed_at_unix_ms: 0,
        });
        c.pause_session();
        chat_resume_session(&b, Some(&sid)).await.expect("resume ok");
        assert!(
            b.session_workflows.read().get(&sid).is_none(),
            "live 恢复只释放 gate，不该再起一份续跑"
        );
    }

    /// chat_abort 连带取消该 session 事件流上的 workflow：flag 置位 +
    /// 从 guard map 移除。
    #[tokio::test]
    async fn chat_abort_cancels_session_workflow() {
        let dir = tempfile::tempdir().unwrap();
        let b = make_backend(dir.path());
        let sid = add_session(&b, "ui-s1").await;
        let flag = Arc::new(AtomicBool::new(false));
        b.session_workflows.write().insert(sid.clone(), flag.clone());
        chat_abort(&b, Some(&sid)).await.expect("abort");
        assert!(flag.load(std::sync::atomic::Ordering::Relaxed));
        assert!(b.session_workflows.read().is_empty());
    }
}

#[cfg(test)]
mod hot_reload_tests {
    use super::*;

    fn make_backend(dir: &std::path::Path) -> UiBackend {
        let cfg = latte_agent_core::AgentConfig::default();
        let resolver = latte_agent_core::ModelResolver::from_config(&cfg).expect("resolver");
        UiBackend::new(crate::UiBackendConfig {
            agent_config: cfg,
            model_resolver: resolver,
            role: None,
            tier: None,
            model_id: None,
            cwd: Some(dir.to_path_buf()),
            agents_config: ".latte/agents.d".into(),
        })
        .expect("backend")
    }

    /// 角色编辑器保存（create/save）必须推进共享 resolver 代际并让
    /// 角色模型指派即刻可读——运行中 session 的热重载依赖这个信号。
    #[test]
    fn role_save_bumps_resolver_generation() {
        let dir = tempfile::tempdir().unwrap();
        let b = make_backend(dir.path());
        let g0 = b.resolver.generation();

        create_role(&b, "architect", "Architect").expect("create role");
        assert_eq!(b.resolver.generation(), g0 + 1, "create_role 推进代际");

        save_role_config(&b, SaveRoleConfigRequest {
            id: "architect".into(),
            name: "Architect".into(),
            icon: String::new(),
            model_tier: "premium".into(),
            model_chain: vec!["m1".into()],
            temperature: None,
            tools: vec![],
            code_paths: vec![],
            prompt: String::new(),
        })
        .expect("save role");
        assert_eq!(b.resolver.generation(), g0 + 2, "save_role_config 推进代际");
        let (chain, tier) = b
            .resolver
            .role_model_assignment("architect")
            .expect("assignment");
        assert_eq!(chain, vec!["m1".to_string()]);
        assert_eq!(
            tier,
            Some(latte_agent_core::model_resolver::ModelTier::Premium)
        );
    }

    /// 模型保存（create_model）推进代际（回归：此前只改 merged 不动
    /// resolver，新值对任何 session 都不生效）。
    #[test]
    fn model_save_bumps_resolver_generation() {
        let dir = tempfile::tempdir().unwrap();
        let b = make_backend(dir.path());
        let g0 = b.resolver.generation();
        create_model(&b, CreateModelRequest {
            target: "project".into(),
            def: ModelDef {
                name: "m1".into(),
                api: "openai".into(),
                provider: "test".into(),
                base_url: "http://localhost:1".into(),
                api_key: "k".into(),
                context_window: 32000,
                max_tokens: 4096,
                supports_thinking: false,
                supports_vision: false,
                supports_image_generation: false,
                cost_per_million_input: None,
                cost_per_million_output: None,
                tier: None,
                timeout_secs: None,
            },
        })
        .expect("create model");
        assert_eq!(b.resolver.generation(), g0 + 1);
        assert!(b.resolver.build_model("m1").is_ok(), "新模型即刻可解析");
    }

    /// 回归：model 只在磁盘上（server 启动后才写入的文件，如用户手动
    /// 编辑的 `models.d/glm.toml`），内存 catalog 是旧快照没有它 ——
    /// PATCH 必须回退磁盘扫描取基准值（与 `GET /api/models` 列表同源），
    /// 而不是直接 404。
    #[test]
    fn update_model_falls_back_to_disk_scan() {
        let dir = tempfile::tempdir().unwrap();
        let b = make_backend(dir.path());
        // 磁盘上放一个 model 文件，内存 catalog 里没有它。
        let models_d = dir.path().join(".latte/models.d");
        std::fs::create_dir_all(&models_d).unwrap();
        std::fs::write(
            models_d.join("diskonly__m1.toml"),
            "name = \"m1\"\n\
             api = \"openai\"\n\
             provider = \"diskonly\"\n\
             base_url = \"http://localhost:1\"\n\
             api_key = \"sk-old\"\n\
             context_window = 32000\n\
             max_tokens = 4096\n",
        )
        .unwrap();

        let out = update_model(&b, "diskonly/m1", "project", ModelPatch {
            api_key: Some("sk-new".into()),
            ..Default::default()
        })
        .expect("PATCH 应回退磁盘扫描而不是 404");

        // 补丁字段生效，未打补丁的字段保留磁盘现值。
        assert_eq!(out.def.api_key, "sk-new");
        assert_eq!(out.def.context_window, 32000);
        // 落盘回原文件，而不是新建别的文件。
        assert!(out.file_path.ends_with("diskonly__m1.toml"));
        // 内存 catalog 同步补上，后续 PATCH 直接命中。
        let cfg = b.merged.read();
        assert!(cfg
            .models
            .models
            .iter()
            .any(|m| m.provider == "diskonly" && m.name == "m1" && m.api_key == "sk-new"));
    }
}
