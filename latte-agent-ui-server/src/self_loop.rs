//! Self-Loop API：spawn 本地 node 子进程跑 Playwright + screencap 的
//! AI 自调试闭环，进度事件经 SSE 推给前端。

use std::path::PathBuf;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use futures_util::stream::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::AppState;

#[derive(Default)]
pub(crate) struct SelfLoopState {
    /// Latest self-loop progress event sender (broadcast for multiple SSE subscribers).
    progress: parking_lot::Mutex<Option<broadcast::Sender<SelfLoopEvent>>>,
}

#[derive(Deserialize)]
pub(crate) struct SelfLoopStartRequest {
    /// 给 AI 的任务描述（如"fix the chat panel layout"）。
    task: String,
    /// 最大迭代轮数（防止死循环）。默认 5。
    #[serde(default)]
    max_iterations: Option<u32>,
}

#[derive(Serialize, Clone, Deserialize)]
pub(crate) struct SelfLoopEvent {
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

pub(crate) async fn self_loop_start(
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

pub(crate) async fn self_loop_events_sse(
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
    Sse::new(stream).keep_alive(
        KeepAlive::new().interval(std::time::Duration::from_secs(10)),
    )
}

pub(crate) async fn self_loop_stop(State(state): State<AppState>) -> StatusCode {
    let mut guard = state.self_loop.progress.lock();
    *guard = None;
    StatusCode::OK
}

/// 定位 `latte-agent-cli/ui/self-loop` 目录：以本 crate 的
/// `CARGO_MANIFEST_DIR` 向上一级锚定 workspace 根，再 fallback 到相对
/// cwd 的路径。
fn self_loop_dir() -> Result<PathBuf, (StatusCode, String)> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut candidates = Vec::with_capacity(2);
    if let Some(ws_root) = manifest_dir.parent() {
        candidates.push(ws_root.join("latte-agent-cli").join("ui").join("self-loop"));
    }
    candidates.push(PathBuf::from("latte-agent-cli/ui/self-loop"));
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
