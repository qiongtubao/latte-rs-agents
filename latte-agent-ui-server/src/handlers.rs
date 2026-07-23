//! HTTP 薄壳：Query/Json/Path 参数提取 → 调 `crate::api`（协议无关），
//! `ApiError` 映射回 `(StatusCode, String)`。两个 SSE 流（chat events /
//! self-loop events）是 HTTP 特有的，保留在本文件。
//!
//! 路由行为与薄壳化之前一致：状态码经 `ApiError::status` 保真，
//! 202 ACCEPTED / 404 / 400 等语义不变。

use std::collections::HashMap;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use futures_util::stream::Stream;
use latte_agent_core::event_json::chat_event_to_frontend_json;
use serde::Deserialize;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::api::{self, SaveRoleConfigRequest};
use crate::AppState;

pub(crate) async fn health() -> &'static str {
    "ok"
}

// ─── Sessions ─────────────────────────────────────────────────────

pub(crate) async fn list_sessions(
    State(state): State<AppState>,
) -> Json<Vec<api::SessionSummary>> {
    Json(api::list_sessions(&state.backend))
}

pub(crate) async fn create_session(
    State(state): State<AppState>,
    // Empty body extractor — POST /api/sessions just allocates a session.
    _body: axum::Json<serde_json::Value>,
) -> Result<Json<api::SessionInfo>, (StatusCode, String)> {
    api::create_session(&state.backend)
        .await
        .map(Json)
        .map_err(Into::into)
}

pub(crate) async fn get_session(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Result<Json<api::SessionInfo>, (StatusCode, String)> {
    let id = params
        .get("id")
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "missing id".into()))?;
    api::get_session(&state.backend, id)
        .map(Json)
        .map_err(Into::into)
}

/// GET /api/session/history?id=... — return the archived ChatEvent log
/// for a session so the UI can restore chat contents after a switch.
pub(crate) async fn get_session_history(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Result<Json<Vec<serde_json::Value>>, (StatusCode, String)> {
    let id = params
        .get("id")
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "missing id".into()))?;
    api::session_history(&state.backend, id)
        .map(Json)
        .map_err(Into::into)
}

/// DELETE /api/sessions?id=... — remove a session and stop its
/// controller. The UI switches to another session (or creates a new
/// one) before calling this when deleting the active session.
pub(crate) async fn delete_session(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Result<StatusCode, (StatusCode, String)> {
    let id = params
        .get("id")
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "missing id".into()))?;
    api::delete_session(&state.backend, id)
        .await
        .map(|_| StatusCode::OK)
        .map_err(Into::into)
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
    match api::set_session_label(&state.backend, &req.session_id, &req.label) {
        Ok(()) => StatusCode::OK,
        Err(_) => StatusCode::NOT_FOUND,
    }
}

// ─── Roles ────────────────────────────────────────────────────────

pub(crate) async fn list_roles(
    State(state): State<AppState>,
) -> Json<Vec<latte_agent_core::controller::RoleInfo>> {
    Json(api::list_roles(&state.backend))
}

pub(crate) async fn get_roles_config(
    State(state): State<AppState>,
) -> Result<Json<api::RolesConfigResponse>, (StatusCode, String)> {
    api::get_roles_config(&state.backend)
        .await
        .map(Json)
        .map_err(Into::into)
}

pub(crate) async fn save_role_config(
    State(state): State<AppState>,
    Json(req): Json<SaveRoleConfigRequest>,
) -> Result<Json<api::RoleConfigEntry>, (StatusCode, String)> {
    api::save_role_config(&state.backend, req)
        .map(Json)
        .map_err(Into::into)
}

// ─── Chat ─────────────────────────────────────────────────────────

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
    match api::chat_send(&state.backend, req.session_id.as_deref(), &req.message).await {
        Ok(()) => StatusCode::ACCEPTED,
        Err(_) => StatusCode::NOT_FOUND,
    }
}
/// Only carries `session_id`, used for endpoints that need no other params.
#[derive(Deserialize)]
pub(crate) struct SessionOnlyRequest {
    #[serde(default)]
    session_id: Option<String>,
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
    match api::chat_command(&state.backend, req.session_id.as_deref(), &req.command).await {
        Ok(()) => StatusCode::ACCEPTED,
        Err(_) => StatusCode::NOT_FOUND,
    }
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
    match api::chat_switch_role(&state.backend, req.session_id.as_deref(), &req.role_id).await {
        Ok(()) => StatusCode::ACCEPTED,
        Err(_) => StatusCode::NOT_FOUND,
    }
}

pub(crate) async fn chat_cancel_turn(
    State(state): State<AppState>,
    Json(req): Json<SessionOnlyRequest>,
) -> StatusCode {
    match api::chat_cancel_turn(&state.backend, req.session_id.as_deref()).await {
        Ok(()) => StatusCode::OK,
        Err(_) => StatusCode::NOT_FOUND,
    }
}

/// `POST /api/chat/abort` — 终止整个 session，body 只需 session_id。
pub(crate) async fn chat_abort(
    State(state): State<AppState>,
    Json(req): Json<SessionOnlyRequest>,
) -> StatusCode {
    match api::chat_abort(&state.backend, req.session_id.as_deref()).await {
        Ok(()) => StatusCode::OK,
        Err(_) => StatusCode::NOT_FOUND,
    }
}
// ─── SSE（HTTP 特有） ─────────────────────────────────────────────

pub(crate) async fn events_sse(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Result<
    Sse<impl Stream<Item = Result<Event, axum::Error>>>,
    (StatusCode, String),
> {
    let id = params
        .get("id")
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "missing ?id=".into()))?;
    let rx = api::subscribe_session(&state.backend, id).await.map_err(|e| {
        (
            StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            // events_sse 原来的错误消息带 "session ... not found"（不带
            // session_id 前缀的 debug 格式），api 的是 "session_id {:?}
            // not found"。保持路由行为不变：这里重写消息。
            match e.status {
                404 => format!("session {id} not found"),
                _ => e.message,
            },
        )
    })?;
    // Each SessionHandle owns its own ChatController (and therefore
    // its own broadcast channel). Subscribing guarantees the SSE
    // stream sees only events for THIS tab — the server is no longer
    // broadcasting the single shared controller's events to every
    // connected tab.
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

// ─── Traces ───────────────────────────────────────────────────────

pub(crate) async fn list_traces() -> Json<Vec<api::TraceSummary>> {
    Json(api::list_traces())
}

pub(crate) async fn read_trace(
    axum::extract::Path(session_id): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    api::read_trace(&session_id).map(Json).map_err(Into::into)
}

// ─── Role Graph / Subsessions ─────────────────────────────────────

/// GET /api/role-graph — build + serve the "role × tool" code-graph.
pub(crate) async fn role_graph_get(
    State(state): State<AppState>,
) -> Result<Json<crate::role_graph::RoleGraph>, (StatusCode, String)> {
    api::role_graph(&state.backend)
        .await
        .map(Json)
        .map_err(Into::into)
}

/// GET /api/subsessions?id=<sub_id> — fetch the subsession transcript.
pub(crate) async fn get_subsession(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Result<Json<Vec<serde_json::Value>>, (StatusCode, String)> {
    let id = params
        .get("id")
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "missing id".into()))?;
    Ok(Json(api::get_subsession(&state.backend, id)))
}

// ─── Self-Loop（start/stop 是薄壳；events 是 SSE） ────────────────

#[derive(Deserialize)]
pub(crate) struct SelfLoopStartRequest {
    /// 给 AI 的任务描述（如"fix the chat panel layout"）。
    task: String,
    /// 最大迭代轮数（防止死循环）。默认 5。
    #[serde(default)]
    max_iterations: Option<u32>,
}

pub(crate) async fn self_loop_start(
    State(state): State<AppState>,
    Json(req): Json<SelfLoopStartRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    api::self_loop_start(&state.backend, req.task, req.max_iterations)
        .await
        .map(|(resp, _rx)| Json(resp))
        .map_err(Into::into)
}

pub(crate) async fn self_loop_events_sse(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, axum::Error>>> {
    let rx = api::self_loop_subscribe(&state.backend);
    let stream = BroadcastStream::new(rx).map(|item| match item {
        Ok(ev) => Ok(Event::default()
            .event("self_loop_event")
            .data(serde_json::to_string(&ev).unwrap_or_else(|_| "{}".into()))),
        Err(_) => Ok(Event::default().event("ping").data("")),
    });
    Sse::new(stream).keep_alive(
        KeepAlive::new().interval(std::time::Duration::from_secs(10)),
    )
}

pub(crate) async fn self_loop_stop(State(state): State<AppState>) -> StatusCode {
    api::self_loop_stop(&state.backend);
    StatusCode::OK
}

// ─── Logs ────────────────────────────────────────────────────────

/// `GET /api/logs` — 列出或读取 ui-sessions 日志。
/// 参数：`?file=<name>&tail=30`
pub(crate) async fn get_logs(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Result<Json<Vec<api::LogEntry>>, (StatusCode, String)> {
    let file = params.get("file").map(|s| s.as_str());
    let tail = params.get("tail").and_then(|s| s.parse::<usize>().ok());
    api::list_logs(&state.backend, file, tail)
        .map(Json)
        .map_err(|e| (StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), e.message))
}
