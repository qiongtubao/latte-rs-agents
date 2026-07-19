//! Self-Loop 支撑：spawn 本地 node 子进程跑 Playwright + screencap 的
//! AI 自调试闭环。协议层入口在 `crate::api`（start/stop/subscribe），
//! HTTP SSE 壳在 `crate::handlers`。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

#[derive(Default)]
pub(crate) struct SelfLoopState {
    /// Latest self-loop progress event sender (broadcast for multiple SSE subscribers).
    progress: parking_lot::Mutex<Option<broadcast::Sender<SelfLoopEvent>>>,
}

impl SelfLoopState {
    /// start 时登记新一轮的事件 sender。
    pub(crate) fn set_progress(&self, tx: broadcast::Sender<SelfLoopEvent>) {
        *self.progress.lock() = Some(tx);
    }

    /// stop：清掉 sender（已有 receiver 会因 channel 关闭而结束）。
    pub(crate) fn clear(&self) {
        *self.progress.lock() = None;
    }

    /// 订阅进度事件。没有在跑的 self-loop 时返回一个 1-容量空
    /// channel 的 receiver —— 前端发 start 后会重新订阅。
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<SelfLoopEvent> {
        let guard = self.progress.lock();
        match guard.as_ref() {
            Some(t) => t.subscribe(),
            None => broadcast::channel::<SelfLoopEvent>(1).1,
        }
    }
}

/// Self-loop 进度事件（SSE / Tauri `ui:self_loop_event` 的载荷）。
/// `kind`: "started" | "iteration" | "log" | "screenshot" | "done" | "error"
#[derive(Serialize, Clone, Deserialize)]
pub struct SelfLoopEvent {
    pub kind: String,
    pub iteration: u32,
    pub message: String,
    /// base64 PNG（仅 kind == "screenshot"）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<String>,
    /// 自由扩展字段
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    pub timestamp_unix_ms: u64,
}

pub(crate) async fn run_self_loop_node(
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

/// 定位 `latte-agent-cli/ui/self-loop` 目录：以本 crate 的
/// `CARGO_MANIFEST_DIR` 向上一级锚定 workspace 根，再 fallback 到相对
/// cwd 的路径。
pub(crate) fn self_loop_dir() -> Result<PathBuf, String> {
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
    Err(format!("self-loop dir not found: tried {}", first.display()))
}
