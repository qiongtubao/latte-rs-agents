//! 协议无关的 UI 后端 API（契约 C1 的 Rust 半区）。
//!
//! 这里集中了原 REST handlers 里与 HTTP 无关的全部逻辑：axum
//! （`crate::handlers`）只是参数提取 + 状态码映射的薄壳；Tauri 适配器
//! （latte-code-editor `chat_panel/ui_adapter.rs`）直接调用本模块，
//! 经 `ui_*` 命令与 `ui:chat_event` / `ui:self_loop_event` 事件驱动同
//! 一个前端。路由语义（含状态码）经 [`ApiError::status`] 保真。
//!
//! 所有函数都操作 [`crate::UiBackend`]（一个工作区一个容器）。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use latte_agent_core::config::AgentConfig;
use latte_agent_core::controller::{ChatEvent, RoleInfo};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::role_graph::RoleGraph;
use crate::sessions::{create_session_handle, SessionHandle};
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
        256,
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

/// 删除 session 并停掉它的 controller。UI 在删除当前活跃 session
/// 之前会先切到别的 session（或新建一个）。
pub async fn delete_session(b: &UiBackend, id: &str) -> Result<(), ApiError> {
    let removed = b.sessions.write().remove(id);
    match removed {
        Some(h) => {
            h.controller.abort().await;
            Ok(())
        }
        None => Err(ApiError::not_found(format!("session {id} unknown"))),
    }
}

/// 重命名 session。空/纯空白 label 清除自定义名，回退到 preview。
pub fn set_session_label(b: &UiBackend, session_id: &str, label: &str) -> Result<(), ApiError> {
    let h = resolve_session(b, Some(session_id))?;
    let trimmed = label.trim();
    *h.label.lock() = if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.chars().take(60).collect())
    };
    h.touch();
    Ok(())
}

/// 订阅某 session 的 ChatEvent broadcast（只拿得到订阅之后的事件）。
/// Tauri 适配器用它把事件转发成 `ui:chat_event`；HTTP 的 SSE handler
/// 走的是同一通道。事件转前端 JSON 用
/// `latte_agent_core::event_json::chat_event_to_frontend_json`（契约 C2）。
pub fn subscribe_session(
    b: &UiBackend,
    id: &str,
) -> Result<broadcast::Receiver<ChatEvent>, ApiError> {
    let h = resolve_session(b, Some(id))?;
    Ok(h.controller.subscribe())
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
    pub prompt_file: Option<String>,
    /// 当前生效的 system prompt 文本（编辑器里直接改它）。
    pub prompt: String,
}

#[derive(Serialize)]
pub struct RolesConfigResponse {
    pub roles: Vec<RoleConfigEntry>,
    pub available_tools: Vec<String>,
    pub tiers: Vec<String>,
}

/// 读取角色当前 prompt：优先 `prompt_file`（相对 backend cwd），读不到
/// 或 None 时回退内嵌兜底 prompt，再不行空串。
fn read_role_prompt(
    cwd: &std::path::Path,
    tpl: &latte_agent_core::role::RoleTemplate,
) -> String {
    if let Some(f) = &tpl.prompt_file {
        if let Ok(s) = std::fs::read_to_string(cwd.join(f)) {
            return s;
        }
    }
    latte_agent_core::prompts::for_role(&tpl.id)
        .map(str::to_string)
        .unwrap_or_default()
}

fn role_config_entry(
    cwd: &std::path::Path,
    tpl: &latte_agent_core::role::RoleTemplate,
) -> RoleConfigEntry {
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
        prompt_file: tpl.prompt_file.clone(),
        prompt: read_role_prompt(cwd, tpl),
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
    Ok(names.into_iter().collect())
}

pub async fn get_roles_config(b: &UiBackend) -> Result<RolesConfigResponse, ApiError> {
    let roles = {
        let cfg = b.merged.read();
        let mut ids: Vec<&String> = cfg.roles.keys().collect();
        ids.sort();
        ids.into_iter()
            .filter_map(|id| cfg.roles.get(id))
            .map(|tpl| role_config_entry(&b.cwd, tpl))
            .collect()
    };
    let available_tools = enumerate_available_tools().await?;
    Ok(RolesConfigResponse {
        roles,
        available_tools,
        tiers: vec!["premium".into(), "standard".into(), "budget".into()],
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
    #[serde(default)]
    pub prompt: String,
}

/// 由 agents 配置路径（文件或目录）定位角色 TOML 所在目录；
/// 指向文件时取其父目录，空值兜底 `.latte/agents.d`。
fn agents_config_dir(agents_config: &str) -> PathBuf {
    let p = PathBuf::from(agents_config);
    if p.is_file() {
        return p
            .parent()
            .map(|d| d.to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".latte/agents.d"));
    }
    if agents_config.trim().is_empty() {
        return PathBuf::from(".latte/agents.d");
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
    let mut doc: DocumentMut = match std::fs::read_to_string(path) {
        Ok(content) => content
            .parse()
            .map_err(|e| format!("parse {}: {e}", path.display()))?,
        Err(_) => DocumentMut::new(),
    };
    let role_exists = doc
        .get("roles")
        .and_then(|r| r.get(req.id.as_str()))
        .and_then(|r| r.as_table())
        .is_some();
    if role_exists {
        let t = doc["roles"][req.id.as_str()].as_table_mut().unwrap();
        t["name"] = value(req.name.clone());
        t["icon"] = value(req.icon.clone());
        t["model_tier"] = value(req.model_tier.clone());
        t["model_chain"] = value(str_array(&req.model_chain));
        t["tools"] = value(str_array(&req.tools));
        match req.temperature {
            Some(temp) => {
                t["temperature"] = value(temp);
            }
            None => {
                t.remove("temperature");
            }
        }
    } else {
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
        if !tpl.skills.is_empty() {
            t["skills"] = value(str_array(&tpl.skills));
        }
        doc["roles"][req.id.as_str()] = Item::Table(t);
    }
    std::fs::write(path, doc.to_string())
        .map_err(|e| format!("write {}: {e}", path.display()))
}

/// 保存角色配置：重写 agents.d TOML → 写 prompt_file → 更新内存配置
/// （新 session 即刻生效）。三步全量落盘，与 HTTP 版语义一致。
pub fn save_role_config(
    b: &UiBackend,
    req: SaveRoleConfigRequest,
) -> Result<RoleConfigEntry, ApiError> {
    let tpl = {
        let cfg = b.merged.read();
        cfg.roles.get(&req.id).cloned()
    };
    let tpl = tpl.ok_or_else(|| ApiError::not_found(format!("role {:?} not found", req.id)))?;

    // 1. 重写 .latte/agents.d/<id>.toml。
    let dir = agents_config_dir(&b.agents_config);
    std::fs::create_dir_all(&dir)
        .map_err(|e| ApiError::internal(format!("create {}: {e}", dir.display())))?;
    let path = dir.join(format!("{}.toml", req.id));
    write_role_toml(&path, &tpl, &req).map_err(ApiError::internal)?;

    // 2. prompt 非空 → 写到该角色的 prompt_file（None → prompts/<id>.md）。
    if !req.prompt.is_empty() {
        let rel = tpl
            .prompt_file
            .clone()
            .unwrap_or_else(|| format!("prompts/{}.md", req.id));
        let p = b.cwd.join(&rel);
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
                role_config_entry(&b.cwd, t)
            }
            None => {
                return Err(ApiError::not_found(format!("role {:?} not found", req.id)))
            }
        }
    };
    Ok(entry)
}

// ─── Chat ─────────────────────────────────────────────────────────

/// `POST /api/chat/send`：向 session 的 controller 提交一条用户消息。
/// 立即返回（"已受理"语义）；产出走 [`subscribe_session`] 的事件流。
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
    h.controller.submit_input(message).await;
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
    h.controller.submit_input(command).await;
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
    h.controller.switch_role(role_id).await;
    Ok(())
}

// ─── Traces ───────────────────────────────────────────────────────

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_role_info_is_sorted() {
        let cfg = AgentConfig::default();
        let v = build_role_info(&cfg);
        assert!(v.is_empty());
    }
}
