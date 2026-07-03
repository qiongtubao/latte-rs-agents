//! JsonRenderer: 把 ChatRenderer 事件转成 JSON Lines 输出到 stdout。
//!
//! 与前端 TS 协议层 (src/chat/types.ts ChatProtocolMessage) 字段对齐：
//! 每行一个 JSON 对象，包含 type 字段（事件类型）+ 数据负载。
//! 适合通过 stdio pipe 给外部消费者（Tauri-side 渲染 / Slack bot / Web）使用。
//!
//! 协议格式示例：
//! ```text
//! {"type":"role_turn","roleId":"pm","content":"...","isComplete":true}
//! {"type":"status","message":"[round 1: started]"}
//! {"type":"done"}
//! ```

use async_trait::async_trait;
use latte_agent_core::controller::RoleInfo;
use latte_agent_core::renderer::{ChatEventMetadata, ChatRenderer};
use serde::Serialize;

use super::cli_renderer::SharedWriter;

pub struct JsonRenderer {
    out: SharedWriter,
}

impl JsonRenderer {
    pub fn new(out: SharedWriter) -> Self {
        Self { out }
    }

    pub fn stdout() -> Self {
        Self::new(super::cli_renderer::stdout_writer())
    }

    fn emit<E: Serialize>(&self, event: E) {
        // 每行一个 JSON：上游消费者用 line-delimited JSON 解析
        let mut guard = self.out.lock();
        if let Ok(json) = serde_json::to_string(&event) {
            let _ = writeln!(guard, "{}", json);
            let _ = guard.flush();
        }
    }
}

// ─── 事件 payload 类型 ───────────────────────────────
// 字段名采用 camelCase 序列化，与前端 ChatProtocolMessage 对齐
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Event<'a> {
    RoleTurn {
        role_id: &'a str,
        content: &'a str,
        is_complete: bool,
    },
    Status {
        message: &'a str,
    },
    Paused {
        reason: &'a str,
    },
    Resumed,
    RoundStarted {
        round: u32,
    },
    RoundEnded {
        round: u32,
    },
    Done,
    Error {
        message: &'a str,
    },
    RoleList {
        roles: &'a [RoleInfo],
    },
    ContextCleared,
    SessionInfo {
        task_id: &'a str,
        state: &'a str,
        turn: u32,
    },
    ToolUse {
        role_id: &'a str,
        tool_name: &'a str,
        args: &'a str,
    },
    ToolResult {
        role_id: &'a str,
        tool_name: &'a str,
        result: &'a str,
    },
}

#[async_trait]
impl ChatRenderer for JsonRenderer {
    async fn on_role_turn(&self, meta: &ChatEventMetadata) {
        self.emit(Event::RoleTurn {
            role_id: &meta.role_id,
            content: &meta.content,
            is_complete: meta.is_complete,
        });
    }

    async fn on_status(&self, message: &str) {
        self.emit(Event::Status { message });
    }

    async fn on_paused(&self, reason: &str) {
        self.emit(Event::Paused { reason });
    }

    async fn on_resumed(&self) {
        self.emit(Event::Resumed);
    }

    async fn on_round_started(&self, round: u32) {
        self.emit(Event::RoundStarted { round });
    }

    async fn on_round_ended(&self, round: u32) {
        self.emit(Event::RoundEnded { round });
    }

    async fn on_done(&self) {
        self.emit(Event::Done);
    }

    async fn on_error(&self, message: &str) {
        self.emit(Event::Error { message });
    }

    async fn on_role_list(&self, roles: &[RoleInfo]) {
        self.emit(Event::RoleList { roles });
    }

    async fn on_context_cleared(&self) {
        self.emit(Event::ContextCleared);
    }

    async fn on_session_info(&self, task_id: &str, state: &str, turn: u32) {
        self.emit(Event::SessionInfo { task_id, state, turn });
    }

    async fn on_tool_use(&self, role_id: &str, tool_name: &str, args: &str) {
        self.emit(Event::ToolUse { role_id, tool_name, args });
    }

    async fn on_tool_result(&self, role_id: &str, tool_name: &str, result: &str) {
        self.emit(Event::ToolResult { role_id, tool_name, result });
    }
}