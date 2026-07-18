//! HTTP handlers：/api/* 全部路由（self-loop 除外，见 `self_loop` 模块）。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use futures_util::stream::Stream;
use latte_agent_core::config::AgentConfig;
use latte_agent_core::controller::RoleInfo;
use latte_agent_core::event_json::chat_event_to_frontend_json;
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::sessions::{create_session_handle, SessionHandle};
use crate::{unix_ts_millis, AppState};

pub(crate) async fn health() -> &'static str {
    "ok"
}

/// Returned by `GET /api/session?id=...` and `POST /api/sessions`.
/// Frontend uses `session_id` as its localStorage key.
#[derive(Serialize)]
pub(crate) struct SessionInfo {
    session_id: String,
    role: String,
    model: Option<String>,
    tier: String,
    /// All sessions the caller could switch to (sidebar).
    available_sessions: Vec<SessionSummary>,
    available_roles: Vec<RoleInfo>,
}

/// Lightweight session entry for the sidebar — returned by
/// `GET /api/sessions` and embedded in `SessionInfo`.
#[derive(Serialize)]
pub(crate) struct SessionSummary {
    session_id: String,
    /// First user message, truncated, used as the human label.
    preview: String,
    /// User-assigned display name (POST /api/session/label). `None`
    /// until the user renames the session.
    label: Option<String>,
    initial_role: String,
    created_at_unix_ms: u64,
    last_activity_unix_ms: u64,
}

pub(crate) async fn list_sessions(State(state): State<AppState>) -> Json<Vec<SessionSummary>> {
    let map = state.sessions.read();
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
    Json(out)
}

pub(crate) async fn create_session(
    State(state): State<AppState>,
    // Empty body extractor — POST /api/sessions just allocates a session.
    _body: axum::Json<serde_json::Value>,
) -> Result<Json<SessionInfo>, (StatusCode, String)> {
    let session_id =
        format!("ui-{}-{}", std::process::id(), unix_ts_millis());
    let handle = create_session_handle(
        session_id.clone(),
        &state.initial_role,
        &state.merged,
        &state.resolver,
        &state.cwd,
        None,
        None,
        256,
        &state.subsession_store,
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("spawn controller: {e}"),
        )
    })?;
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
        available_roles: build_role_info(&state.merged.read()),
    };
    state.sessions.write().insert(h.session_id.clone(), h);
    Ok(Json(resp))
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
///
async fn resolve_session(
    state: &AppState,
    body_id: Option<&str>,
    query_id: Option<&str>,
) -> Result<Arc<SessionHandle>, (StatusCode, String)> {
    let id = body_id
        .or(query_id)
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "missing session_id".into()))?;
    let map = state.sessions.read();
    map.get(id)
        .cloned()
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("session_id {id:?} not found"),
            )
        })
}

pub(crate) async fn get_session(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<
        std::collections::HashMap<String, String>,
    >,
) -> Result<Json<SessionInfo>, (StatusCode, String)> {
    let id = params
        .get("id")
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "missing id".into()))?;
    let h: Arc<SessionHandle> = {
        let map = state.sessions.read();
        map.get(id).cloned()
    }
    .ok_or_else(|| (StatusCode::NOT_FOUND, format!("session {id} unknown")))?;
    let label = h.label.lock().clone();
    Ok(Json(SessionInfo {
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
        available_roles: build_role_info(&state.merged.read()),
    }))
}

/// GET /api/session/history?id=... — return the archived ChatEvent log
/// for a session so the UI can restore chat contents after a switch.
pub(crate) async fn get_session_history(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<
        std::collections::HashMap<String, String>,
    >,
) -> Result<Json<Vec<serde_json::Value>>, (StatusCode, String)> {
    let id = params
        .get("id")
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "missing id".into()))?;
    let h: Arc<SessionHandle> = {
        let map = state.sessions.read();
        map.get(id).cloned()
    }
    .ok_or_else(|| (StatusCode::NOT_FOUND, format!("session {id} unknown")))?;
    let events: Vec<serde_json::Value> = h
        .event_log
        .read()
        .iter()
        .filter_map(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .collect();
    Ok(Json(events))
}

/// DELETE /api/sessions?id=... — remove a session and stop its
/// controller. The UI switches to another session (or creates a new
/// one) before calling this when deleting the active session.
pub(crate) async fn delete_session(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<
        std::collections::HashMap<String, String>,
    >,
) -> Result<StatusCode, (StatusCode, String)> {
    let id = params
        .get("id")
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "missing id".into()))?;
    let removed = state.sessions.write().remove(id);
    match removed {
        Some(h) => {
            h.controller.abort().await;
            Ok(StatusCode::OK)
        }
        None => Err((
            StatusCode::NOT_FOUND,
            format!("session {id} unknown"),
        )),
    }
}

#[derive(Deserialize)]
pub(crate) struct LabelRequest {
    session_id: String,
    label: String,
}

/// POST /api/session/label — rename a session. An empty/whitespace
/// label clears the custom name and falls back to the preview.
pub(crate) async fn set_session_label(
    State(state): State<AppState>,
    Json(req): Json<LabelRequest>,
) -> StatusCode {
    let Ok(h) = resolve_session(&state, Some(req.session_id.as_str()), None).await else {
        return StatusCode::NOT_FOUND;
    };
    let trimmed = req.label.trim();
    *h.label.lock() = if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.chars().take(60).collect())
    };
    h.touch();
    StatusCode::OK
}

pub(crate) async fn list_roles(State(state): State<AppState>) -> Json<Vec<RoleInfo>> {
    Json(build_role_info(&state.merged.read()))
}

pub(crate) fn build_role_info(cfg: &AgentConfig) -> Vec<RoleInfo> {
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
pub(crate) struct RoleConfigEntry {
    id: String,
    name: String,
    category: String,
    icon: String,
    model_tier: String,
    model_chain: Vec<String>,
    temperature: Option<f64>,
    tools: Vec<String>,
    skills: Vec<String>,
    prompt_file: Option<String>,
    /// 当前生效的 system prompt 文本（编辑器里直接改它）。
    prompt: String,
}

#[derive(Serialize)]
pub(crate) struct RolesConfigResponse {
    roles: Vec<RoleConfigEntry>,
    available_tools: Vec<String>,
    tiers: Vec<String>,
}

/// 读取角色当前 prompt：优先 `prompt_file`（相对 state.cwd），读不到
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
async fn enumerate_available_tools() -> Result<Vec<String>, (StatusCode, String)> {
    use latte_rs_agent_tools::prelude::*;
    let mgr = create_tool_manager();
    for p in builtin_tool_packages() {
        mgr.register_package(p).await.map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("register_package: {e}"),
            )
        })?;
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

pub(crate) async fn get_roles_config(
    State(state): State<AppState>,
) -> Result<Json<RolesConfigResponse>, (StatusCode, String)> {
    let roles = {
        let cfg = state.merged.read();
        let mut ids: Vec<&String> = cfg.roles.keys().collect();
        ids.sort();
        ids.into_iter()
            .filter_map(|id| cfg.roles.get(id))
            .map(|tpl| role_config_entry(&state.cwd, tpl))
            .collect()
    };
    let available_tools = enumerate_available_tools().await?;
    Ok(Json(RolesConfigResponse {
        roles,
        available_tools,
        tiers: vec!["premium".into(), "standard".into(), "budget".into()],
    }))
}

/// `POST /api/roles/config` 的请求体。`category` / `prompt_file` /
/// `skills` 不在其中 —— 保存时保持原值不变。
#[derive(Deserialize)]
pub(crate) struct SaveRoleConfigRequest {
    id: String,
    name: String,
    icon: String,
    model_tier: String,
    #[serde(default)]
    model_chain: Vec<String>,
    temperature: Option<f64>,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    prompt: String,
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

pub(crate) async fn save_role_config(
    State(state): State<AppState>,
    Json(req): Json<SaveRoleConfigRequest>,
) -> Result<Json<RoleConfigEntry>, (StatusCode, String)> {
    let tpl = {
        let cfg = state.merged.read();
        cfg.roles.get(&req.id).cloned()
    };
    let tpl = tpl.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("role {:?} not found", req.id),
        )
    })?;

    // 1. 重写 .latte/agents.d/<id>.toml。
    let dir = agents_config_dir(&state.agents_config);
    std::fs::create_dir_all(&dir).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("create {}: {e}", dir.display()),
        )
    })?;
    let path = dir.join(format!("{}.toml", req.id));
    write_role_toml(&path, &tpl, &req)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    // 2. prompt 非空 → 写到该角色的 prompt_file（None → prompts/<id>.md）。
    if !req.prompt.is_empty() {
        let rel = tpl
            .prompt_file
            .clone()
            .unwrap_or_else(|| format!("prompts/{}.md", req.id));
        let p = state.cwd.join(&rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("create {}: {e}", parent.display()),
                )
            })?;
        }
        std::fs::write(&p, &req.prompt).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("write {}: {e}", p.display()),
            )
        })?;
    }

    // 3. 更新内存中的 merged —— 新 session 即刻生效。
    let entry = {
        let mut cfg = state.merged.write();
        match cfg.roles.get_mut(&req.id) {
            Some(t) => {
                t.name = req.name.clone();
                t.icon = req.icon.clone();
                t.model_tier = req.model_tier.clone();
                t.model_chain = req.model_chain.clone();
                t.temperature = req.temperature;
                t.tools = req.tools.clone();
                role_config_entry(&state.cwd, t)
            }
            None => {
                return Err((
                    StatusCode::NOT_FOUND,
                    format!("role {:?} not found", req.id),
                ))
            }
        }
    };
    Ok(Json(entry))
}

#[derive(Deserialize)]
pub(crate) struct SendRequest {
    /// First field so `session_id` presence in body doesn't surprise
    /// legacy clients; the frontend always sends both.
    #[serde(default)]
    session_id: Option<String>,
    message: String,
}

pub(crate) async fn chat_send(
    State(state): State<AppState>,
    Json(req): Json<SendRequest>,
) -> StatusCode {
    let h = match resolve_session(
        &state,
        req.session_id.as_deref(),
        None,
    )
    .await
    {
        Ok(h) => h,
        Err(_) => return StatusCode::NOT_FOUND,
    };
    h.touch();
    // First-message preview for the sidebar entry. Cheap + bounded.
    if h.first_user_msg.lock().is_none() {
        let preview = req.message.chars().take(80).collect::<String>();
        *h.first_user_msg.lock() = Some(preview);
    }
    h.controller.submit_input(&req.message).await;
    StatusCode::ACCEPTED
}

#[derive(Deserialize)]
pub(crate) struct CommandRequest {
    #[serde(default)]
    session_id: Option<String>,
    command: String,
}

pub(crate) async fn chat_command(
    State(state): State<AppState>,
    Json(req): Json<CommandRequest>,
) -> StatusCode {
    let Ok(h) = resolve_session(&state, req.session_id.as_deref(), None).await else {
        return StatusCode::NOT_FOUND;
    };
    h.touch();
    h.controller.submit_input(&req.command).await;
    StatusCode::ACCEPTED
}

#[derive(Deserialize)]
pub(crate) struct SwitchRoleRequest {
    #[serde(default)]
    session_id: Option<String>,
    role_id: String,
}

pub(crate) async fn switch_role(
    State(state): State<AppState>,
    Json(req): Json<SwitchRoleRequest>,
) -> StatusCode {
    let Ok(h) = resolve_session(&state, req.session_id.as_deref(), None).await else {
        return StatusCode::NOT_FOUND;
    };
    h.touch();
    h.controller.switch_role(&req.role_id).await;
    StatusCode::ACCEPTED
}

pub(crate) async fn events_sse(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<
        std::collections::HashMap<String, String>,
    >,
) -> Result<
    Sse<impl Stream<Item = Result<Event, axum::Error>>>,
    (StatusCode, String),
> {
    let id = params
        .get("id")
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "missing ?id=".into()))?;
    let map = state.sessions.read();
    let h = map
        .get(id)
        .cloned()
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("session {id} not found"),
            )
        })?;
    // Each SessionHandle owns its own ChatController (and therefore
    // its own broadcast channel). Subscribing to `h.controller`
    // guarantees the SSE stream sees only events for THIS tab — the
    // server is no longer broadcasting the single shared controller's
    // events to every connected tab.
    drop(map);
    let rx = h.controller.subscribe();
    let stream = BroadcastStream::new(rx).map(|item| match item {
        Ok(ev) => match chat_event_to_frontend_json(&ev) {
            Ok(json) => Ok(Event::default()
                .event("chat_event")
                .data(json)),
            Err(e) => Ok(Event::default()
                .event("error")
                .data(format!("chat_event convert failed: {}", e))),
        },
        Err(e) => Ok(Event::default()
            .event("error")
            .data(format!("broadcast lag: {}", e))),
    });
    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new().interval(std::time::Duration::from_secs(15)),
    ))
}

// ─── Trace API ────────────────────────────────────────────────────

#[derive(Serialize)]
pub(crate) struct TraceSummary {
    session_id: String,
    path: String,
    size_bytes: u64,
    modified_unix: u64,
}

pub(crate) async fn list_traces() -> Json<Vec<TraceSummary>> {
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

pub(crate) async fn read_trace(
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

fn latte_home() -> PathBuf {
    std::env::var_os("LATTE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".latte")))
        .unwrap_or_else(|| PathBuf::from(".latte"))
}

/// GET /api/role-graph — build + serve the "role × tool" code-graph.
///
/// Combines the project's `.latte/agents.d/*.toml` (which lists each
/// role's declared tools) with a TreeSitterEngine scan of the project
/// root for `register_*_tool` call-sites. The result is JSON-friendly
/// and consumed by the UI's Graph panel.
pub(crate) async fn role_graph_get(
    State(state): State<AppState>,
) -> Result<Json<crate::role_graph::RoleGraph>, (StatusCode, String)> {
    match crate::role_graph::build(&state.cwd, &state.cwd).await {
        Ok(g) => Ok(Json(g)),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

/// GET /api/subsessions?id=<sub_id> — fetch the subsession transcript.
pub(crate) async fn get_subsession(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<
        std::collections::HashMap<String, String>,
    >,
) -> Result<Json<Vec<serde_json::Value>>, (StatusCode, String)> {
    let id = params
        .get("id")
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "missing id".into()))?;
    let snapshot = state.subsession_store.snapshot_any(id);
    match snapshot {
        Some(events) => {
            let vals: Vec<serde_json::Value> = events
                .into_iter()
                .filter_map(|e| serde_json::to_value(e).ok())
                .collect();
            Ok(Json(vals))
        }
        None => Ok(Json(vec![])),
    }
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
