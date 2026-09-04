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

/// POST /api/sessions/fork — fork a new session from a prefix of a
/// source session's visible history (up to a right-clicked message).
#[derive(Deserialize)]
pub(crate) struct ForkRequest {
    source_session_id: String,
    /// Frontend-JSON ChatEvents from the source session, in order,
    /// up to and including the fork point.
    #[serde(default)]
    events: Vec<serde_json::Value>,
}

pub(crate) async fn fork_session(
    State(state): State<AppState>,
    Json(req): Json<ForkRequest>,
) -> Result<Json<api::SessionInfo>, (StatusCode, String)> {
    api::fork_session(&state.backend, &req.source_session_id, req.events)
        .await
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

/// `POST /api/chat/choice-answer` —— 阻塞中的 ask（workflow/delegate
/// 子代理，`ChoiceRequested.wait=true`）的回答直达通道。前端把用户在
/// 选择框里的答案 POST 到这里，经 core 的 choice 路由送达等待方；
/// 无匹配挂起项（已答/已超时/未知 id）时 404，前端据此降级为普通
/// 消息回喂（fire-and-forget 路径）。
#[derive(Deserialize)]
pub(crate) struct ChoiceAnswerRequest {
    choice_id: String,
    answer: String,
}

pub(crate) async fn chat_choice_answer(
    State(state): State<AppState>,
    Json(req): Json<ChoiceAnswerRequest>,
) -> StatusCode {
    use api::ChoiceAnswerOutcome as O;
    match api::deliver_choice_answer(&state.backend, &req.choice_id, req.answer).await {
        // 直达活着的等待方，或答案已落 checkpoint + 断点续跑已拉起。
        O::DeliveredLive | O::DeliveredViaResume => StatusCode::OK,
        // 既没有等待方也没有可恢复的落盘记录 → 前端降级成普通消息。
        O::NotFound => StatusCode::NOT_FOUND,
        // 记录有效但续跑起不来（workflow 定义被删等）。答案已经在
        // checkpoint 里了，不该让前端再把它当普通消息发一遍，所以不回
        // 404；回 500 让 UI 显示失败原因。
        O::ResumeFailed(e) => {
            eprintln!(
                "[ui-choice] {} 的答案已落 checkpoint，但续跑失败: {}",
                req.choice_id, e.message
            );
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// `POST /api/chat/prompt-dismiss` 的请求体。
#[derive(Deserialize)]
pub(crate) struct PromptDismissRequest {
    /// ask 的 `choice_id` 或 plan 的 `plan_id`。
    prompt_id: String,
}

/// POST /api/chat/prompt-dismiss —— 用户已处理某个非阻塞弹框
/// （提交了选择 / 跳过 / 导入了清单），从补发表销账，避免重连时
/// 弹出僵尸框。幂等：未命中也回 200（可能已被别的 tab 处理）。
pub(crate) async fn chat_prompt_dismiss(
    State(state): State<AppState>,
    Json(req): Json<PromptDismissRequest>,
) -> StatusCode {
    api::dismiss_prompt(&state.backend, &req.prompt_id);
    StatusCode::OK
}

/// GET /api/chat/pending-prompts?id=... —— 本 session 仍未被用户处理
/// 的弹框事件（阻塞 ask + 非阻塞 ask/plan + **跨进程孤儿 ask**），
/// 以前端事件 JSON 返回。
///
/// SSE 只在**新建连接**时补发挂起弹框。但补齐路径不止重连一条：
/// broadcast lag 时连接是活的（前端只做 history 全量重放），而
/// `clear() + replayEvents(history)` 会把弹框卡片连同未提交状态一起
/// 抹掉——history 里没有的（Lagged 掉、被 MAX_LOG 挤出去的）就永远
/// 回不来了。这个端点让前端在任何一次重放后显式把挂起弹框补上。
///
/// 服务器重启后内存表全空，这里返回的是盘上的孤儿 ask
/// （`.latte/pending-asks/`）——它们**可答**，答案会补写进 checkpoint
/// 并触发断点续跑（见 `api::deliver_choice_answer`）。
pub(crate) async fn chat_pending_prompts(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Result<Json<Vec<serde_json::Value>>, (StatusCode, String)> {
    let id = params
        .get("id")
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "missing ?id=".into()))?;
    api::pending_dialog_events_json(&state.backend, id)
        .await
        .map(Json)
        .map_err(Into::into)
}
/// Only carries `session_id`, used for endpoints that need no other params.
#[derive(Deserialize)]
pub(crate) struct SessionOnlyRequest {
    #[serde(default)]
    session_id: Option<String>,
}

/// `POST /api/chat/cancel-turn` 的请求体。
///
/// `sub_id` 缺省 → per-turn 取消（掐当前 turn，session 保留），与旧
/// 客户端完全兼容；`sub_id` 给出 → 只终止那一条分派，同一并行波里的
/// 兄弟分派继续跑（UI 右键 subsession →「终止此分派」）。
#[derive(Deserialize)]
pub(crate) struct CancelTurnRequest {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    sub_id: Option<String>,
}

/// 设置 session 的流式模式开关。
#[derive(Deserialize)]
pub(crate) struct StreamModeRequest {
    #[serde(default)]
    session_id: Option<String>,
    stream: bool,
}

/// `POST /api/chat/stream-mode` -- 运行时切换 stream/non-stream 模式。
pub(crate) async fn chat_stream_mode(
    State(state): State<AppState>,
    Json(req): Json<StreamModeRequest>,
) -> StatusCode {
    match api::set_stream_mode(&state.backend, req.session_id.as_deref(), req.stream).await {
        Ok(()) => StatusCode::OK,
        Err(_) => StatusCode::NOT_FOUND,
    }
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
    Json(req): Json<CancelTurnRequest>,
) -> StatusCode {
    // 带 sub_id → 只终止那一条分派；不带 → 老语义（per-turn 取消）。
    let result = match req.sub_id.as_deref() {
        Some(sub_id) if !sub_id.trim().is_empty() => {
            api::chat_cancel_subagent(&state.backend, req.session_id.as_deref(), sub_id).await
        }
        _ => api::chat_cancel_turn(&state.backend, req.session_id.as_deref()).await,
    };
    match result {
        Ok(()) => StatusCode::OK,
        // 404 = 该分派已结束（或 session 不存在），前端据此提示而不是
        // 报错——用户点终止时分派刚好跑完是正常竞态。
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

pub(crate) async fn chat_pause(
    State(state): State<AppState>,
    Json(req): Json<SessionOnlyRequest>,
) -> StatusCode {
    match api::chat_pause(&state.backend, req.session_id.as_deref()).await {
        Ok(()) => StatusCode::OK,
        Err(_) => StatusCode::NOT_FOUND,
    }
}

pub(crate) async fn chat_resume(
    State(state): State<AppState>,
    Json(req): Json<SessionOnlyRequest>,
) -> StatusCode {
    match api::chat_resume(&state.backend, req.session_id.as_deref()).await {
        Ok(()) => StatusCode::OK,
        Err(_) => StatusCode::NOT_FOUND,
    }
}

pub(crate) async fn chat_pause_session(
    State(state): State<AppState>,
    Json(req): Json<SessionOnlyRequest>,
) -> StatusCode {
    match api::chat_pause_session(&state.backend, req.session_id.as_deref()).await {
        Ok(()) => StatusCode::OK,
        Err(_) => StatusCode::NOT_FOUND,
    }
}

pub(crate) async fn chat_resume_session(
    State(state): State<AppState>,
    Json(req): Json<SessionOnlyRequest>,
) -> StatusCode {
    match api::chat_resume_session(&state.backend, req.session_id.as_deref()).await {
        Ok(()) => StatusCode::OK,
        Err(_) => StatusCode::NOT_FOUND,
    }
}
/// Body for per-role pause/resume: carries the target `role_id` plus the
/// usual optional `session_id`. Shared by both endpoints.
#[derive(Deserialize)]
pub(crate) struct RolePauseRequest {
    #[serde(default)]
    session_id: Option<String>,
    role_id: String,
}

pub(crate) async fn chat_pause_role(
    State(state): State<AppState>,
    Json(req): Json<RolePauseRequest>,
) -> StatusCode {
    match api::chat_pause_role(&state.backend, req.session_id.as_deref(), &req.role_id).await {
        Ok(()) => StatusCode::ACCEPTED,
        Err(_) => StatusCode::NOT_FOUND,
    }
}

pub(crate) async fn chat_resume_role(
    State(state): State<AppState>,
    Json(req): Json<RolePauseRequest>,
) -> StatusCode {
    match api::chat_resume_role(&state.backend, req.session_id.as_deref(), &req.role_id).await {
        Ok(()) => StatusCode::ACCEPTED,
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
    // 先订阅再取挂起弹框：反序会漏掉这两步之间新发出的弹框。
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
    //
    // 补发仍在等用户回答的 ask 弹框（见 `api::pending_dialog_events_json`）：
    // 只发给这条新连接，不重新广播，避免 archiver 二次归档。
    // 含**跨进程孤儿**：服务器重启后内存表全空，弹框只在盘上
    // （`.latte/pending-asks/`），这里把它补出来，用户回答会补写进
    // checkpoint 并触发断点续跑。
    let replay: Vec<Result<Event, axum::Error>> =
        api::pending_dialog_events_json(&state.backend, id)
            .await
            .unwrap_or_default()
            .iter()
            .filter_map(|v| serde_json::to_string(v).ok())
            .map(|json| Ok(Event::default().event("chat_event").data(json)))
            .collect();
    let replay = tokio_stream::iter(replay);
    let backend = state.backend.clone();
    let stream = BroadcastStream::new(rx).map(move |item| match item {
        Ok(ev) => {
            // P0-2: 工具层 task_report → ChatEvent::TaskReport →
            // ui-server 桥接 → POST /api/tasks/:id/report，
            // 把任务推进到 human_review（completed）或 todo（其他）。
            // 调用是 fire-and-forget，失败只 eprintln（与其它 archiver
            // 失败处理一致——不打断聊天）。
            if let latte_agent_core::controller::ChatEvent::TaskReport {
                task_id, summary, result, ..
            } = &ev
            {
                match crate::tasks::report_task(&backend, task_id, summary, result) {
                    Ok(_) => {}
                    Err(e) => eprintln!(
                        "[ui-sse] task_report bridge {task_id} failed: {}",
                        e.message
                    ),
                }
            }
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
    // JS 看不到 axum keep-alive 的注释心跳——前端看门狗需要真实事件
    // 来判断连接死活（半开连接时 EventSource 不报错也不收数据，会
    // 让 UI 永久停在旧状态；现场实锤）。每 15s 一个 ping
    // 命名事件，前端 >60s 无任何事件即主动断开重建。
    let ping = tokio_stream::wrappers::IntervalStream::new(tokio::time::interval(
        std::time::Duration::from_secs(15),
    ))
    .map(|_| Ok(Event::default().event("ping").data("1")));
    // replay 在前、live 在后：挂起弹框先到，之后是实时事件。
    let stream = replay.chain(stream).merge(ping);
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
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(100); // 默认只返回最近 100 条事件
    Ok(Json(api::get_subsession(&state.backend, id, limit)))
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

// ─── Workflows（CRUD/validate/run 是薄壳；run/events 是 SSE） ─────

pub(crate) async fn list_workflows_h(
    State(state): State<AppState>,
) -> Result<Json<Vec<crate::workflows::WorkflowSummary>>, (StatusCode, String)> {
    api::list_workflows(&state.backend)
        .map(Json)
        .map_err(Into::into)
}

pub(crate) async fn get_workflow_h(
    State(state): State<AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Result<Json<crate::workflows::WorkflowDetail>, (StatusCode, String)> {
    api::get_workflow(&state.backend, &name)
        .map(Json)
        .map_err(Into::into)
}

pub(crate) async fn create_workflow_h(
    State(state): State<AppState>,
    Json(form): Json<crate::workflows::WorkflowForm>,
) -> Result<Json<crate::workflows::WorkflowDetail>, (StatusCode, String)> {
    api::create_workflow(&state.backend, form)
        .map(Json)
        .map_err(Into::into)
}

pub(crate) async fn update_workflow_h(
    State(state): State<AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
    Json(form): Json<crate::workflows::WorkflowForm>,
) -> Result<Json<crate::workflows::WorkflowDetail>, (StatusCode, String)> {
    api::update_workflow(&state.backend, &name, form)
        .map(Json)
        .map_err(Into::into)
}

pub(crate) async fn delete_workflow_h(
    State(state): State<AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    api::delete_workflow(&state.backend, &name)
        .map(|_| StatusCode::OK)
        .map_err(Into::into)
}

pub(crate) async fn validate_workflow_h(
    State(state): State<AppState>,
    Json(form): Json<crate::workflows::WorkflowForm>,
) -> Json<crate::workflows::ValidateResponse> {
    Json(api::validate_workflow_form(&state.backend, &form))
}

pub(crate) async fn workflow_run_start_h(
    State(state): State<AppState>,
    Json(req): Json<api::WorkflowRunRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    api::workflow_run_start(&state.backend, req)
        .map(|(resp, _rx)| Json(resp))
        .map_err(Into::into)
}

pub(crate) async fn workflow_run_stop_h(State(state): State<AppState>) -> StatusCode {
    api::workflow_run_stop(&state.backend);
    StatusCode::OK
}

pub(crate) async fn workflow_resume_h(
    State(state): State<AppState>,
    Json(req): Json<api::WorkflowResumeRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    api::workflow_resume(&state.backend, req)
        .await
        .map(Json)
        .map_err(Into::into)
}

pub(crate) async fn get_workflow_toml_h(
    axum::extract::Path(name): axum::extract::Path<String>,
    State(state): State<AppState>,
) -> Result<String, (StatusCode, String)> {
    api::get_workflow_toml(&state.backend, &name)
        .map_err(|e| {
            (
                StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                e.message,
            )
        })
}

pub(crate) async fn put_workflow_toml_h(
    axum::extract::Path(name): axum::extract::Path<String>,
    State(state): State<AppState>,
    body: String,
) -> Result<StatusCode, (StatusCode, String)> {
    api::put_workflow_toml(&state.backend, &name, &body)
        .map(|_| StatusCode::OK)
        .map_err(|e| {
            (
                StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                e.message,
            )
        })
}

/// `GET /api/workflows/run/events` — 测试运行事件流，只转发
/// Workflow* 事件（Started/Step/Turn/Finished），JSON 由
/// `chat_event_to_frontend_json` 序列化（契约与 chat events 一致）。
/// 开头先补发回放缓冲（前端 POST run 之后才连 SSE，Started/Step
/// 可能已经在连接建立前发出）。
pub(crate) async fn workflow_run_events_sse(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, axum::Error>>> {
    use latte_agent_core::controller::ChatEvent;
    let (history, rx) = api::workflow_run_subscribe(&state.backend);
    fn to_sse(ev: ChatEvent) -> Option<Result<Event, axum::Error>> {
        if !matches!(
            ev,
            ChatEvent::WorkflowStarted { .. }
                | ChatEvent::WorkflowStep { .. }
                | ChatEvent::WorkflowTurn { .. }
                | ChatEvent::WorkflowFinished { .. }
        ) {
            return None;
        }
        Some(match chat_event_to_frontend_json(&ev) {
            Ok(json) => Ok(Event::default().event("chat_event").data(json)),
            Err(e) => Ok(Event::default()
                .event("error")
                .data(format!("chat_event convert failed: {}", e))),
        })
    }
    let replay = futures_util::stream::iter(history.into_iter().filter_map(to_sse));
    let live = BroadcastStream::new(rx).filter_map(|item| match item {
        Ok(ev) => to_sse(ev),
        Err(e) => Some(Ok(Event::default()
            .event("error")
            .data(format!("broadcast lag: {}", e)))),
    });
    Sse::new(replay.chain(live)).keep_alive(
        KeepAlive::new().interval(std::time::Duration::from_secs(15)),
    )
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

// ─── Models ─────────────────────────────────────────────────────

/// `GET /api/models` — 列出合并后的 model catalog。
///
/// 数据源：`UiBackend.merged: Arc<RwLock<AgentConfig>>`，已经走过
/// `config_layer::load` 的三层合并（含 `~/.latte/models.d/*.toml`
/// 按厂商拆分）。这里直接透传，不再二次扫描磁盘 —— 这样 UI 看到的
/// 列表与运行时 `ModelResolver` 实际能解析到的 model 集合保持一致。
pub(crate) async fn list_models(
    State(state): State<AppState>,
) -> Result<Json<api::ModelsListResponse>, (StatusCode, String)> {
    api::list_models(&state.backend)
        .map(Json)
        .map_err(|e| {
            (
                StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                e.message,
            )
        })
}

/// `PATCH /api/models/:key` —— 部分更新单个 model（partial update）。
///
/// URL `:key` 用 `provider/id` 形式，与 `list_models` 返回的 `key` 一致。
/// 请求 body 只需带**要改的字段**（缺省字段保持现值）；整表提交也兼容
/// （所有字段都在）。合并 + 原子写细节见 [`api::update_model`]。
pub(crate) async fn update_model(
    axum::extract::Path(key): axum::extract::Path<String>,
    State(state): State<AppState>,
    Json(req): Json<api::UpdateModelRequest>,
) -> Result<Json<api::ModelWithSource>, (StatusCode, String)> {
    api::update_model(&state.backend, &key, &req.target, req.patch)
        .map(Json)
        .map_err(|e| {
            (
                StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                e.message,
            )
        })
}

/// `POST /api/models/test` —— 跑一次连通性 / 简单 chat 测试。
///
/// 详见 `crate::test::run_test` 的 doc。body 是 `TestModelRequest`，
/// 完整 model 定义（不是 catalog 里的 —— 改完没保存的也能直接测）。
pub(crate) async fn test_model(
    Json(req): Json<crate::test::TestModelRequest>,
) -> Result<Json<crate::test::TestModelResponse>, (StatusCode, String)> {
    let resp = crate::test::run_test(req).await;
    Ok(Json(resp))
}

/// `GET /api/models/:key/capabilities` —— image input / image generation
/// 能力探测。当前是启发式（看 supports_vision + model id pattern），
/// 之后可以做一次真请求探测。
pub(crate) async fn model_capabilities(
    axum::extract::Path(key): axum::extract::Path<String>,
    State(state): State<AppState>,
) -> Result<Json<crate::test::ModelCapabilities>, (StatusCode, String)> {
    // key 是 `provider/name`；从 catalog 里找 def，找不到就回 404。
    let def = {
        let cfg = state.backend.merged.read();
        let (provider, id) = key
            .split_once('/')
            .map(|(p, n)| (p.to_string(), n.to_string()))
            .ok_or_else(|| (StatusCode::BAD_REQUEST, format!("bad key {key:?}")))?;
        cfg.models
            .models
            .iter()
            .find(|m| m.provider == provider && m.name == id)
            .cloned()
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    format!("model {key:?} not in catalog"),
                )
            })?
    };
    // 能力探测不依赖 mode / prompt —— 直接构造 request。
    let req = crate::test::TestModelRequest {
        def,
        mode: crate::test::TestMode::Connectivity,
        prompt: String::new(),
        images: Vec::new(),
        probe_path: None,
    };
    Ok(Json(crate::test::probe_capabilities(&req).await))
}

// ─── Models CRUD 扩展 ──────────────────────────────────────────────

/// `DELETE /api/models/:key?source=project|global` —— 删除指定层的 model
/// 文件并从内存 catalog 移除。`source` 缺省时向后兼容（项目优先）。
pub(crate) async fn delete_model(
    axum::extract::Path(key): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
    State(state): State<AppState>,
) -> Result<StatusCode, (StatusCode, String)> {
    let source = params.get("source").map(String::as_str);
    api::delete_model(&state.backend, &key, source)
        .map(|_| StatusCode::OK)
        .map_err(|e| {
            (
                StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                e.message,
            )
        })
}

/// `POST /api/models` —— 创建新 model 并写入项目目录 + 更新内存 catalog。
pub(crate) async fn create_model(
    State(state): State<AppState>,
    Json(req): Json<api::CreateModelRequest>,
) -> Result<Json<api::ModelWithSource>, (StatusCode, String)> {
    api::create_model(&state.backend, req)
        .map(Json)
        .map_err(|e| {
            (
                StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                e.message,
            )
        })
}

// ─── Models TOML 源文件编辑 ──────────────────────────────────────

/// `GET /api/models/:key/toml` — 读取模型 TOML 源文件原始内容。
pub(crate) async fn get_model_toml(
    axum::extract::Path(key): axum::extract::Path<String>,
    State(state): State<AppState>,
) -> Result<String, (StatusCode, String)> {
    api::get_model_toml(&state.backend, &key)
        .map_err(|e| {
            (
                StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                e.message,
            )
        })
}

/// `PUT /api/models/:key/toml` — 直接写入模型 TOML 源文件原始内容。
pub(crate) async fn put_model_toml(
    axum::extract::Path(key): axum::extract::Path<String>,
    State(state): State<AppState>,
    body: String,
) -> Result<StatusCode, (StatusCode, String)> {
    api::put_model_toml(&state.backend, &key, &body)
        .map(|_| StatusCode::OK)
        .map_err(|e| {
            (
                StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                e.message,
            )
        })
}

// ─── Tools ─────────────────────────────────────────────────────────

/// `GET /api/tools` —— 列出所有工具及其启用/禁用状态。
pub(crate) async fn list_tools(
    State(state): State<AppState>,
) -> Result<Json<api::ToolsListResponse>, (StatusCode, String)> {
    api::list_tools(&state.backend)
        .await
        .map(Json)
        .map_err(|e| {
            (
                StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                e.message,
            )
        })
}

/// `POST /api/tools/:id/toggle` —— 切换工具启用/禁用状态。
/// body: `{"enabled": true}` 或 `{"enabled": false}`。
#[derive(Deserialize)]
pub(crate) struct ToggleToolBody {
    pub enabled: bool,
}

pub(crate) async fn toggle_tool(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(state): State<AppState>,
    Json(body): Json<ToggleToolBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    api::toggle_tool(&state.backend, &id, body.enabled)
        .map(|_| StatusCode::OK)
        .map_err(|e| {
            (
                StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                e.message,
            )
        })
}

/// `POST /api/tools/test` —— 测试工具是否能正常执行。
#[derive(Deserialize)]
pub(crate) struct TestToolRequest {
    pub tool_id: String,
    pub args: serde_json::Value,
}

pub(crate) async fn test_tool(
    State(state): State<AppState>,
    Json(req): Json<TestToolRequest>,
) -> Json<crate::test::TestToolResponse> {
    let resp = crate::test::run_tool_test(
        crate::test::TestToolRequest {
            tool_id: req.tool_id,
            args: req.args,
        },
        &state.backend.cwd,
    )
    .await;
    Json(resp)
}

/// `GET /api/tools/:id/doc` —— 读取工具的模型侧 Markdown 文档 + 可编辑标志。
pub(crate) async fn get_tool_doc(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(state): State<AppState>,
) -> Result<Json<api::ToolDocResponse>, (StatusCode, String)> {
    api::get_tool_doc(&state.backend, &id)
        .await
        .map(Json)
        .map_err(|e| {
            (
                StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                e.message,
            )
        })
}

/// `PUT /api/tools/:id/doc` —— 写入工具的模型侧 Markdown 文档（body 为 JSON）。
///
/// 写 `<cwd>/.latte/tools.d/<id>.md`，并就地刷新活跃会话的工具描述；返回体
/// 里的 `refreshed_managers` 就是热更新到了几个会话（UI 拿它给用户回执）。
pub(crate) async fn put_tool_doc(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(state): State<AppState>,
    axum::extract::Json(body): axum::extract::Json<api::PutToolDocRequest>,
) -> Result<Json<api::PutToolDocResponse>, (StatusCode, String)> {
    api::put_tool_doc(&state.backend, &id, &body.summary, &body.content, &body.target)
        .await
        .map(Json)
        .map_err(|e| {
            (
                StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                e.message,
            )
        })
}

/// `DELETE /api/tools/:id/doc` —— 删掉项目工具文档，回到只有内置描述的状态。
pub(crate) async fn delete_tool_doc(
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<api::DeleteToolDocQuery>,
    State(state): State<AppState>,
) -> Result<Json<api::PutToolDocResponse>, (StatusCode, String)> {
    api::delete_tool_doc(&state.backend, &id, &query.target)
        .await
        .map(Json)
        .map_err(|e| {
            (
                StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                e.message,
            )
        })
}

/// `POST /api/roles/test` —— 测试角色配置是否能正常与模型对话。
#[derive(Deserialize)]
pub(crate) struct TestRoleBody {
    pub role_id: String,
    pub config: crate::api::SaveRoleConfigRequest,
}

pub(crate) async fn test_role(
    State(state): State<AppState>,
    Json(req): Json<TestRoleBody>,
) -> Json<crate::test::TestRoleResponse> {
    let resp = crate::test::run_role_test(
        crate::test::TestRoleRequest {
            role_id: req.role_id,
            config: req.config,
        },
        &state.backend.resolver,
        &state.backend.cwd,
    )
    .await;
    Json(resp)
}


/// `POST /api/roles` 请求体：role_id + role_name。
#[derive(Deserialize)]
pub(crate) struct CreateRoleBody {
    pub role_id: String,
    pub role_name: String,
}

/// `POST /api/roles` —— 新建角色（写入 agents.d 配置 + 更新内存）。
pub(crate) async fn create_role(
    State(state): State<AppState>,
    Json(req): Json<CreateRoleBody>,
) -> Result<Json<api::RoleConfigEntry>, (StatusCode, String)> {
    api::create_role(&state.backend, &req.role_id, &req.role_name)
        .map(Json)
        .map_err(|e| {
            (
                StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                e.message,
            )
        })
}

/// `DELETE /api/roles/:id` —— 删除角色（删 agents.d 文件 + 更新内存）。
pub(crate) async fn delete_role(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(state): State<AppState>,
) -> Result<StatusCode, (StatusCode, String)> {
    api::delete_role(&state.backend, &id)
        .map(|_| StatusCode::OK)
        .map_err(|e| {
            (
                StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                e.message,
            )
        })
}

/// `GET /api/roles/:id/toml` — 读取角色 TOML 源文件原始内容。
pub(crate) async fn get_role_toml(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(state): State<AppState>,
) -> Result<String, (StatusCode, String)> {
    api::get_role_toml(&state.backend, &id)
        .map_err(|e| {
            (
                StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                e.message,
            )
        })
}

/// `PUT /api/roles/:id/toml` — 直接写入角色 TOML 源文件原始内容。
pub(crate) async fn put_role_toml(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(state): State<AppState>,
    body: String,
) -> Result<StatusCode, (StatusCode, String)> {
    api::put_role_toml(&state.backend, &id, &body)
        .map(|_| StatusCode::OK)
        .map_err(|e| {
            (
                StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                e.message,
            )
        })
}

// ─── Advisor 全局开关 ────────────────────────────────────────────

/// `GET /api/advisor` — advisor 全局开关状态。
pub(crate) async fn get_advisor(State(state): State<AppState>) -> Json<api::AdvisorState> {
    Json(api::get_advisor(&state.backend))
}

/// `PUT /api/advisor` 请求体。
#[derive(serde::Deserialize)]
pub(crate) struct PutAdvisorRequest {
    enabled: bool,
}

/// `PUT /api/advisor` — 设置 advisor 全局开关（写全局层
/// `agents.d/advisor.toml`，内存 merged 同步更新，新 session 生效）。
pub(crate) async fn put_advisor(
    State(state): State<AppState>,
    Json(req): Json<PutAdvisorRequest>,
) -> Result<Json<api::AdvisorState>, (StatusCode, String)> {
    api::put_advisor(&state.backend, req.enabled)
        .map(Json)
        .map_err(|e| {
            (
                StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                e.message,
            )
        })
}

// ─── Tasks（任务看板，薄壳 → crate::tasks） ──────────────────────

/// `GET /api/tasks` —— 全量列表（含子任务聚合进度）。
pub(crate) async fn list_tasks(
    State(state): State<AppState>,
) -> Json<Vec<crate::tasks::TaskView>> {
    Json(crate::tasks::list_tasks(&state.backend))
}

/// `POST /api/tasks` —— 新建（可带 parent_id）。
pub(crate) async fn create_task(
    State(state): State<AppState>,
    Json(req): Json<crate::tasks::CreateTaskRequest>,
) -> Result<Json<crate::tasks::TaskView>, (StatusCode, String)> {
    crate::tasks::create_task(&state.backend, req)
        .map(Json)
        .map_err(Into::into)
}

/// `POST /api/tasks/import` —— 批量导入（含一层子任务）。
pub(crate) async fn import_tasks(
    State(state): State<AppState>,
    Json(req): Json<crate::tasks::ImportTasksRequest>,
) -> Result<Json<crate::tasks::ImportTasksResponse>, (StatusCode, String)> {
    crate::tasks::import_tasks(&state.backend, req)
        .await
        .map(Json)
        .map_err(Into::into)
}

/// `POST /api/tasks/dispatch-ready` —— 一键批量派发全部 todo 任务
/// （按优先级升序，遵守并发上限与同族/paths 互斥）。
pub(crate) async fn dispatch_ready(
    State(state): State<AppState>,
) -> Json<crate::tasks::DispatchReadyResponse> {
    Json(crate::tasks::dispatch_ready(&state.backend, "user", None).await)
}

/// `GET /api/tasks/:id` —— 详情。
pub(crate) async fn get_task(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(state): State<AppState>,
) -> Result<Json<crate::tasks::TaskView>, (StatusCode, String)> {
    crate::tasks::get_task(&state.backend, &id)
        .map(Json)
        .map_err(Into::into)
}

/// `PATCH /api/tasks/:id` —— 改标题/描述/优先级/状态/排期/标签。
pub(crate) async fn update_task(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(state): State<AppState>,
    Json(patch): Json<crate::tasks::TaskPatch>,
) -> Result<Json<crate::tasks::TaskView>, (StatusCode, String)> {
    crate::tasks::update_task(&state.backend, &id, patch)
        .map(Json)
        .map_err(Into::into)
}

/// `POST /api/tasks/:id/dispatch` — 立即执行（或 rework 重新派发）。
/// body 可选：`{ "mode": "redo" | "rework" }`，缺省 `"redo"`。
#[derive(Deserialize)]
pub(crate) struct DispatchTaskBody {
    #[serde(default)]
    pub mode: String,
}

pub(crate) async fn dispatch_task(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(state): State<AppState>,
    Json(body): Json<DispatchTaskBody>,
) -> Result<Json<crate::tasks::TaskView>, (StatusCode, String)> {
    let mode = if body.mode == "rework" { "rework" } else { "redo" };
    crate::tasks::dispatch_task(&state.backend, &id, "user", mode)
        .await
        .map(Json)
        .map_err(Into::into)
}

/// `POST /api/tasks/:id/refine` —— 拆分子任务：新建 session 跑
/// task_refine workflow，plan 提案导入后成为该任务的子任务。
pub(crate) async fn refine_task(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(state): State<AppState>,
) -> Result<Json<crate::tasks::RefineTaskResponse>, (StatusCode, String)> {
    crate::tasks::refine_task(&state.backend, &id)
        .await
        .map(Json)
        .map_err(Into::into)
}

/// `POST /api/tasks/:id/abort` —— 中止执行（走现有 chat/abort 机制）。
pub(crate) async fn abort_task(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(state): State<AppState>,
) -> Result<Json<crate::tasks::TaskView>, (StatusCode, String)> {
    crate::tasks::abort_task(&state.backend, &id)
        .await
        .map(Json)
        .map_err(Into::into)
}

/// `POST /api/tasks/:id/report` —— manager 回报完成（设计 §5.1 方案 A 的接口半区）。
pub(crate) async fn report_task(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(state): State<AppState>,
    Json(req): Json<crate::tasks::ReportTaskRequest>,
) -> Result<Json<crate::tasks::TaskView>, (StatusCode, String)> {
    crate::tasks::report_task(&state.backend, &id, &req.summary, &req.result)
        .map(Json)
        .map_err(Into::into)
}

/// `GET /api/task-types` —— 列出任务类型注册表（可配置，项目层覆盖）。
pub(crate) async fn list_task_types(
    State(state): State<AppState>,
) -> Json<Vec<crate::task_types::TaskTypeEntry>> {
    Json(crate::task_types::TaskTypeRegistry::load(&state.backend.cwd).entries())
}

/// `GET /api/task-types/:id` —— 单个类型详情
pub(crate) async fn get_task_type(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<crate::task_types::TaskTypeEntry>, (StatusCode, String)> {
    let reg = crate::task_types::TaskTypeRegistry::load(&state.backend.cwd);
    let e = reg.types.get(&id).cloned().ok_or_else(|| (StatusCode::NOT_FOUND, format!("task_type '{id}' 不存在")))?;
    Ok(Json(crate::task_types::TaskTypeEntry { id, def: e }))
}

/// `POST /api/task-types` —— 新建任务类型
pub(crate) async fn create_task_type(
    State(state): State<AppState>,
    Json(entry): Json<crate::task_types::TaskTypeEntry>,
) -> Result<Json<crate::task_types::TaskTypeEntry>, (StatusCode, String)> {
    crate::task_types::upsert_task_type(&state.backend.cwd, entry.clone())
        .map(|e| Json(e))
        .map_err(|m| (StatusCode::BAD_REQUEST, m))
}

/// `PUT /api/task-types/:id` —— 更新任务类型（id 来自路径，body 可含 def 字段）
pub(crate) async fn put_task_type(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(mut entry): Json<crate::task_types::TaskTypeEntry>,
) -> Result<Json<crate::task_types::TaskTypeEntry>, (StatusCode, String)> {
    // 路径 id 优先，若 body 带有不同 id 则以路径为准
    entry.id = id.clone();
    crate::task_types::upsert_task_type(&state.backend.cwd, entry.clone())
        .map(|e| Json(e))
        .map_err(|m| (StatusCode::BAD_REQUEST, m))
}

/// `DELETE /api/task-types/:id` —— 删除任务类型（仅项目层自定义可删）
pub(crate) async fn delete_task_type(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    crate::task_types::delete_task_type(&state.backend.cwd, &id)
        .map(|_| StatusCode::OK)
        .map_err(|m| {
            let code = if m.contains("不存在") { StatusCode::NOT_FOUND } else { StatusCode::BAD_REQUEST };
            (code, m)
        })
}

/// `DELETE /api/tasks/:id` —— 删除（文件移入 archive/）。
pub(crate) async fn delete_task(
    axum::extract::Path(id): axum::extract::Path<String>,
    State(state): State<AppState>,
) -> Result<StatusCode, (StatusCode, String)> {
    crate::tasks::delete_task(&state.backend, &id)
        .map(|_| StatusCode::OK)
        .map_err(Into::into)
}

// ─── Images（generate_image 工具产物，HTTP 特有：直接吐字节） ─────

/// `GET /api/images/:file` —— 伺服 `<cwd>/.latte/images/<file>`。
///
/// generate_image 工具把图落盘到该目录并通过
/// `ChatEvent::ImageGenerated { path: "/api/images/<file>" }` 通知 UI；
/// web UI 直接 `<img src>` 渲染。防目录穿越：拒绝任何含 `/`、`\`、
/// `..` 的文件名（路由参数本身不含 `/`，这里是纵深防御）。
pub(crate) async fn get_image(
    axum::extract::Path(file): axum::extract::Path<String>,
    State(state): State<AppState>,
) -> impl axum::response::IntoResponse {
    if file.contains('/') || file.contains('\\') || file.contains("..") {
        return Err((StatusCode::BAD_REQUEST, "invalid image name".to_string()));
    }
    let path = state.backend.cwd.join(".latte/images").join(&file);
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| (StatusCode::NOT_FOUND, "image not found".to_string()))?;
    let content_type = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "application/octet-stream",
    };
    Ok(([(axum::http::header::CONTENT_TYPE, content_type)], bytes))
}

/// `POST /api/images?ext=png` —— 用户在选择框里上传自定义图片时用。
///
/// Body 是**原始图片字节**（前端把 `File` 直接当 body 发，不做
/// base64，省一个依赖）。`ext` 查询参数给出扩展名（png/jpg/…）。
/// 落盘到 `<cwd>/.latte/images/upload-<ts>.<ext>`，返回
/// `{ "path": "/api/images/<file>" }` —— 与 `ImageGenerated.path`
/// 同构，前端拿到后既可 `<img src>` 预览，也可把 URL 放进选择结果
/// 回喂给模型。防穿越：ext 白名单校验，文件名由服务端生成。
pub(crate) async fn upload_image(
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if body.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "empty image body".to_string()));
    }
    // 上限 10 MiB，避免超大 body 打爆内存 / 磁盘。
    const MAX_BYTES: usize = 10 * 1024 * 1024;
    if body.len() > MAX_BYTES {
        return Err((StatusCode::PAYLOAD_TOO_LARGE, "image exceeds 10 MiB".to_string()));
    }
    let ext = params
        .get("ext")
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| "png".to_string());
    if !matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp") {
        return Err((StatusCode::BAD_REQUEST, format!("unsupported image ext: {ext}")));
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    let file = format!("upload-{ts}.{ext}");
    let dir = state.backend.cwd.join(".latte/images");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("mkdir images: {e}")))?;
    tokio::fs::write(dir.join(&file), &body)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("write image: {e}")))?;
    Ok(Json(serde_json::json!({ "path": format!("/api/images/{file}") })))
}
