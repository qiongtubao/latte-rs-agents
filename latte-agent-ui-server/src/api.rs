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
use latte_agent_core::workflow::{load_workflow, run_workflow, WorkflowRunContext};
use latte_ai::params::GenerateParams;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::role_graph::RoleGraph;
use crate::sessions::{create_session_handle, SessionHandle};
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
    let session_id =
        format!("ui-{}-{}", std::process::id(), crate::unix_ts_millis());
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
        }],
        available_roles: build_role_info(&b.merged.read()),
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
        }],
        available_roles: build_role_info(&b.merged.read()),
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

#[derive(Serialize)]
pub struct RolesConfigResponse {
    pub roles: Vec<RoleConfigEntry>,
    pub available_tools: Vec<String>,
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
}

pub async fn get_roles_config(b: &UiBackend) -> Result<RolesConfigResponse, ApiError> {
    // 表单数据与源文件 tab 同源：先从磁盘重载，再读内存。
    reload_roles_from_disk(b);
    let agents_dir = agents_config_dir(&b.cwd, &b.agents_config);
    let roles = {
        let cfg = b.merged.read();
        let mut ids: Vec<&String> = cfg.roles.keys().collect();
        ids.sort();
        ids.into_iter()
            .filter_map(|id| cfg.roles.get(id))
            .map(|tpl| role_config_entry(b, tpl))
            .collect()
    };
    let available_tools = enumerate_available_tools().await?;
    Ok(RolesConfigResponse {
        roles,
        available_tools,
        tiers: vec!["premium".into(), "standard".into(), "budget".into()],
        workspace_path: b.cwd.display().to_string(),
        agents_config_path: agents_dir.display().to_string(),
        sessions_path: crate::sessions::SessionPersist::dir_for(&b.cwd).display().to_string(),
    })
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
pub fn get_subsession(b: &UiBackend, id: &str) -> Vec<serde_json::Value> {
    match b.subsession_store.snapshot_any(id) {
        Some(events) => events
            .into_iter()
            .filter_map(|e| serde_json::to_value(e).ok())
            .collect(),
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

#[cfg(test)]
mod tests {
    use super::*;

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

/// PATCH 请求体：把 `def` 整体写回（partial update 未实现）。
/// `target`: "project"（默认，写到 `<cwd>/.latte/models.d/`）或 "global"
/// （写到 `~/.latte/models.d/`）。两个写盘目录分别对应 UI 上的
/// "保存到项目" / "保存到全局" 两个按钮。
#[derive(Deserialize)]
pub struct UpdateModelRequest {
    #[serde(default)]
    pub target: String,
    #[serde(flatten)]
    pub def: ModelDef,
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
    // sources + paths 是两个 map，但 keys 一致，所以可以合并成
    // `key -> (source, path)` 的单 map。sources 用 into_iter() 消费；
    // paths 用 clone() 保留给下面 lookup。
    let source_of: BTreeMap<String, (crate::models::ModelSource, PathBuf)> =
        on_disk.sources.into_iter()
            .map(|(k, src)| {
                let path = on_disk.paths.get(&k).cloned().unwrap_or_default();
                (k, (src, path))
            })
            .collect();
    let models: Vec<ModelWithSource> = cfg
        .models
        .models
        .iter()
        .map(|def| {
            let key = if def.name.contains('/') {
                def.name.clone()
            } else {
                format!("{}/{}", def.provider, def.name)
            };
            // disk 扫描告诉我们这个 model 是 project / global、落在哪个文件。
            // 没扫到说明只在内存 catalog 里（新加但还没保存）。
            let (source, path) = source_of
                .get(&key)
                .cloned()
                .map(|(s, p)| (source_label(s).to_string(), p.display().to_string()))
                .unwrap_or_else(|| ("catalog".to_string(), String::new()));
            ModelWithSource {
                key,
                source,
                file_path: path,
                def: def.clone(),
            }
        })
        .collect();
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

/// `PATCH /api/models/:key` —— 把一个已存在的 model 写回磁盘（项目目录
/// `<cwd>/.latte/models.d/<provider>__<id>.toml`），同时就地更新内存
/// catalog 让后续 chat 立刻看到新值。
///
/// 写入策略：项目目录优先（与 `ModelsState::load` 的"项目覆盖全局"语义
/// 对称）。如果旧文件在全局，保存时会落到项目目录，相当于把全局 model
/// 提升到项目层 —— 这是符合直觉的"修改并本地化"操作。
pub fn update_model(
    b: &UiBackend,
    key: &str,
    target: &str,
    def: ModelDef,
) -> Result<ModelWithSource, ApiError> {
    crate::models::validate(&def)
        .map_err(|e| ApiError::bad_request(format!("validate: {e}")))?;
    let new_key = format!("{}/{}", def.provider, def.name);
    if new_key != key {
        return Err(ApiError::bad_request(format!(
            "composite_key 不能改：原 key={key:?}, 新 key={new_key:?}。请用 PATCH 不带改 key，或先 DELETE 再 POST。"
        )));
    }
    // target: "project" (默认，写到 `<cwd>/.latte/models.d/`) 或 "global"
    // （写到 `~/.latte/models.d/`，与 GlobalConfig::load_default 同源）。
    // 这两个按钮（保存到项目 / 保存到全局）共用一条路由，目标由 request
    // body 的 `target` 字段决定。
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
    // 写盘：保持与项目层一致（单文件 flat ModelDef TOML）。同一目录多次
    // 保存时 `key_to_filename` 会覆盖同名文件（`open(..., O_CREAT|O_TRUNC)`
    // 语义），无需额外删除。
    let path = crate::models::ModelsState::write_project(&write_dir, key, &def)
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
    Ok(ModelWithSource {
        key: key.to_string(),
        source: source_label.to_string(),
        file_path: path.display().to_string(),
        def,
    })
}

/// `DELETE /api/models/:key` — 删除 model 文件（项目目录 + 全局目录）
/// 并从内存 catalog 移除。两个目录都尝试删除；至少一个找到才算成功。
pub fn delete_model(b: &UiBackend, key: &str) -> Result<(), ApiError> {
    let project_dir = b.cwd.join(".latte/models.d");
    let global_dir = crate::models::ModelsState::global_models_dir();
    let deleted_project = crate::models::ModelsState::delete_project(&project_dir, key)
        .map_err(|e| ApiError::internal(format!("delete project model: {e}")))?;
    let deleted_global = crate::models::ModelsState::delete_project(&global_dir, key)
        .map_err(|e| ApiError::internal(format!("delete global model: {e}")))?;
    if !deleted_project && !deleted_global {
        return Err(ApiError::not_found(format!("model {key:?} not found")));
    }
    // 从内存 catalog 移除（按 composite_key 匹配 provider/name）。
    {
        let mut cfg = b.merged.write();
        cfg.models.models.retain(|m| {
            format!("{}/{}", m.provider, m.name) != key
        });
    }
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
    // 写盘：与 update_model 一致，始终用 write_project（传入已解析的目录）。
    let path = crate::models::ModelsState::write_project(&write_dir, &key, &req.def)
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
    Ok(())
}
