//! CliRenderer: 把 ChatRenderer 事件转成带 ANSI 颜色的终端输出。
//!
//! 维持原 chat.rs 里 println!/print! 的格式（status 用 [brackets]、角色 turn 用 box 渲染），
//! 但把渲染逻辑从业务循环中抽出。任何 run_hil_repl/run_hil_chat 的消费者只依赖
//! ChatRenderer trait，未来切到 JSON 渲染器、Slack 渲染器时不需要改业务代码。

use std::io::Write;
use std::sync::Arc;

use async_trait::async_trait;
use latte_agent_core::controller::RoleInfo;
use latte_agent_core::renderer::{ChatEventMetadata, ChatRenderer};

use super::style;

/// 写到 `Box<dyn Write>` 的 helper：避免每个 impl 都重复 stdout 句柄 + flush 逻辑。
pub type SharedWriter = Arc<parking_lot::Mutex<Box<dyn Write + Send>>>;

pub fn stdout_writer() -> SharedWriter {
    Arc::new(parking_lot::Mutex::new(Box::new(std::io::stdout())))
}

pub struct CliRenderer {
    out: SharedWriter,
}

impl CliRenderer {
    pub fn new(out: SharedWriter) -> Self {
        Self { out }
    }

    pub fn stdout() -> Self {
        Self::new(stdout_writer())
    }

    fn write(&self, line: impl AsRef<str>) {
        let mut guard = self.out.lock();
        let _ = writeln!(guard, "{}", line.as_ref());
        let _ = guard.flush();
    }

    fn write_raw(&self, raw: impl AsRef<str>) {
        let mut guard = self.out.lock();
        let _ = write!(guard, "{}", raw.as_ref());
        let _ = guard.flush();
    }
}

#[async_trait]
impl ChatRenderer for CliRenderer {
    async fn on_role_turn(&self, meta: &ChatEventMetadata) {
        // 沿用 chat.rs 里 `println!("[{} round {}: ok, {} chars]", ...)` 的格式：
        // 角色 turn 在单角色 REPL 里直接打印整段 content
        self.write(style::paint(
            style::FG_CYAN,
            &format!("[{}] ({} chars)", meta.role_id, meta.content.len()),
        ));
        self.write(&meta.content);
    }

    async fn on_status(&self, message: &str) {
        self.write(message);
    }

    fn render_prompt(&self, icon: &str, role_id: &str, model_id: &str) -> String {
        style::render_prompt(icon, role_id, model_id)
    }

    async fn on_paused(&self, reason: &str) {
        self.write(style::paint_bold(style::FG_YELLOW, &format!("[paused: {}]", reason)));
    }

    async fn on_resumed(&self) {
        self.write(style::paint(style::FG_GREEN, "[resumed]"));
    }

    async fn on_round_started(&self, round: u32) {
        self.write(format!("[round {}: started]", round));
    }

    async fn on_round_ended(&self, round: u32) {
        self.write(format!("[round {}: ended]", round));
    }

    async fn on_done(&self) {
        self.write(style::paint(style::FG_GREEN, "[done]"));
    }

    async fn on_error(&self, message: &str) {
        self.write(style::paint_bold(
            style::FG_RED,
            &format!("[error: {}]", message),
        ));
    }

    async fn on_role_list(&self, roles: &[RoleInfo]) {
        self.write(format!("[roles: {}]", roles.len()));
        for r in roles {
            self.write(format!("  {} — {}", r.id, r.name));
        }
    }

    async fn on_context_cleared(&self) {
        self.write("[context cleared]");
    }

    async fn on_session_info(&self, task_id: &str, state: &str, turn: u32) {
        self.write(format!("[session: {}, state: {}, turn: {}]", task_id, state, turn));
    }

    async fn on_tool_use(&self, role_id: &str, tool_name: &str, args: &str) {
        self.write(format!(
            "[tool: {}] {} invoked `{}` with: {}",
            role_id, role_id, tool_name, args
        ));
    }

    async fn on_tool_result(&self, role_id: &str, tool_name: &str, result: &str) {
        self.write(format!("[tool: {}] {} `{}` -> {}", role_id, role_id, tool_name, result));
    }
}

#[cfg(test)]
mod tests;