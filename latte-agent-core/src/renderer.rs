//! ChatRenderer trait: 解耦 chat 事件的处理逻辑与具体的展示方式。
//!
//! 任何实现了 ChatRenderer 的类型（CLI 终端输出 / JSON Lines / Tauri 事件转发）
//! 都共用同一套事件派发接口。run_hil_repl 等消费者只依赖此 trait 而不依赖具体渲染器。
//!
//! # 设计原则
//!
//! - 每个事件类型一个方法，不搞统一的 en/disable，让 impl 能按需忽略不关心的事件
//! - `render_prompt` 是同步的（返回格式化后的字符串），方便 REPL 提示行展示
//! - `on_*` 方法都是 async，因为某些渲染器（如 Tauri 转发）需要异步

use crate::controller::ChatEvent;

/// 角色执行数据（对应 TS 侧 `ChatProtocolMessage` 的结构化字段）
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatEventMetadata {
    pub role_id: String,
    pub content: String,
    pub is_complete: bool,
}

/// 渲染器接口：每个后端事件对应一个方法。
///
/// 默认实现是空操作——impl 只需要 override 自己关心的事件。
#[allow(unused_variables)]
#[async_trait::async_trait]
pub trait ChatRenderer: Send + Sync {
    /// 角色完成了一轮产出。
    async fn on_role_turn(&self, meta: &ChatEventMetadata) {}

    /// 状态/信息性消息。
    async fn on_status(&self, message: &str) {}

    /// 当前提示行（角色·模型）。
    fn render_prompt(&self, icon: &str, role_id: &str, model_id: &str) -> String {
        format!("{} {} · {}", icon, role_id, model_id)
    }

    /// 会话暂停。
    async fn on_paused(&self, reason: &str) {}

    /// 会话恢复。
    async fn on_resumed(&self) {}

    /// 新的一轮开始。
    async fn on_round_started(&self, round: u32) {}

    /// 一轮结束。
    async fn on_round_ended(&self, round: u32) {}

    /// 会话完成。
    async fn on_done(&self) {}

    /// 错误消息。
    async fn on_error(&self, message: &str) {}

    /// 角色列表（响应 `/roles`）。
    async fn on_role_list(&self, roles: &[crate::controller::RoleInfo]) {}

    /// 上下文清空。
    async fn on_context_cleared(&self) {}

    /// 启动信息：session 身份 + 初始状态。
    async fn on_session_info(&self, task_id: &str, state: &str, turn: u32) {}

    /// 工具使用。
    async fn on_tool_use(&self, role_id: &str, tool_name: &str, args: &str) {}

    /// 工具结果。
    async fn on_tool_result(&self, role_id: &str, tool_name: &str, result: &str) {}

    /// 便捷方法：把一个完整的 `ChatEvent` 派发到对应的方法。
    async fn dispatch_event(&self, event: &ChatEvent) {
        match event {
            ChatEvent::RoleTurn { role_id, content, is_complete, .. } => {
                self.on_role_turn(&ChatEventMetadata {
                    role_id: role_id.clone(),
                    content: content.clone(),
                    is_complete: *is_complete,
                })
                .await;
            }
            ChatEvent::Status { message } => self.on_status(message).await,
            ChatEvent::UserMessage { .. } => {
                // 用户输入在 CLI 里本来就是本地 echo，无需渲染器再画
                // 一遍；该事件主要供 UI 回放（event_log / 落盘）恢复
                // 用户气泡。
            }
            ChatEvent::Prompt { .. } => {
                // prompt 是同步格式化的事件，没有专门的 on_prompt 方法
            }
            ChatEvent::Paused { reason } => self.on_paused(reason).await,
            ChatEvent::Resumed => self.on_resumed().await,
            ChatEvent::RoundStarted { round } => self.on_round_started(*round).await,
            ChatEvent::RoundEnded { round } => self.on_round_ended(*round).await,
            ChatEvent::RoleStarted { role_id, detail } => {
                self.on_status(&format!("{role_id} started: {detail}")).await;
            }
            ChatEvent::RoleFinished { role_id, detail } => {
                self.on_status(&format!("{role_id} finished: {detail}")).await;
            }
            ChatEvent::DelegateStarted { from_role, to_role, task, .. } => {
                self.on_status(&format!("{from_role} delegated to {to_role}: {task}")).await;
            }
            ChatEvent::DelegateFinished { from_role, to_role, status, summary, .. } => {
                self.on_status(&format!("{from_role} delegate {to_role} {status}: {summary}")).await;
            }
            ChatEvent::Done => self.on_done().await,
            ChatEvent::Error { message, .. } => self.on_error(message).await,
            ChatEvent::RoleList { roles } => self.on_role_list(roles).await,
            ChatEvent::ContextCleared => self.on_context_cleared().await,
            ChatEvent::SessionInfo { task_id, state, turn, .. } => {
                self.on_session_info(task_id, state, *turn).await;
            }
            ChatEvent::ToolUse { role_id, tool_name, args } => {
                self.on_tool_use(role_id, tool_name, args).await;
            }
            ChatEvent::ToolResult { role_id, tool_name, result } => {
                self.on_tool_result(role_id, tool_name, result).await;
            }
            ChatEvent::ToolError { role_id, tool_name, error } => {
                self.on_tool_result(role_id, tool_name, error).await;
            }
            ChatEvent::WorkflowStarted { name, topic, .. } => {
                self.on_status(&format!("workflow {name} started: {topic}")).await;
            }
            ChatEvent::WorkflowStep { step_id, index, total, .. } => {
                self.on_status(&format!("workflow step {index}/{total}: {step_id}")).await;
            }
            ChatEvent::WorkflowTurn { role_id, content, .. } => {
                self.on_role_turn(&ChatEventMetadata {
                    role_id: role_id.clone(),
                    content: content.clone(),
                    is_complete: true,
                })
                .await;
            }
            ChatEvent::WorkflowFinished { name, status, summary, .. } => {
                self.on_status(&format!("workflow {name} {status}: {summary}")).await;
            }
        }
    }
}

/// 多渲染器聚合：把所有事件广播给所有子渲染器。
/// 常见用法：CLI 终端输出 + 持久化 trace 日志。
pub struct FanoutRenderer {
    renderers: Vec<Box<dyn ChatRenderer>>,
}

impl FanoutRenderer {
    pub fn new(renderers: Vec<Box<dyn ChatRenderer>>) -> Self {
        Self { renderers }
    }
}

#[async_trait::async_trait]
impl ChatRenderer for FanoutRenderer {
    async fn on_role_turn(&self, meta: &ChatEventMetadata) {
        for r in &self.renderers {
            r.on_role_turn(meta).await;
        }
    }
    async fn on_status(&self, message: &str) {
        for r in &self.renderers {
            r.on_status(message).await;
        }
    }
    async fn on_paused(&self, reason: &str) {
        for r in &self.renderers {
            r.on_paused(reason).await;
        }
    }
    async fn on_resumed(&self) {
        for r in &self.renderers {
            r.on_resumed().await;
        }
    }
    async fn on_round_started(&self, round: u32) {
        for r in &self.renderers {
            r.on_round_started(round).await;
        }
    }
    async fn on_round_ended(&self, round: u32) {
        for r in &self.renderers {
            r.on_round_ended(round).await;
        }
    }
    async fn on_done(&self) {
        for r in &self.renderers {
            r.on_done().await;
        }
    }
    async fn on_error(&self, message: &str) {
        for r in &self.renderers {
            r.on_error(message).await;
        }
    }
    async fn on_role_list(&self, roles: &[crate::controller::RoleInfo]) {
        for r in &self.renderers {
            r.on_role_list(roles).await;
        }
    }
    async fn on_context_cleared(&self) {
        for r in &self.renderers {
            r.on_context_cleared().await;
        }
    }
    async fn on_session_info(&self, task_id: &str, state: &str, turn: u32) {
        for r in &self.renderers {
            r.on_session_info(task_id, state, turn).await;
        }
    }
    async fn on_tool_use(&self, role_id: &str, tool_name: &str, args: &str) {
        for r in &self.renderers {
            r.on_tool_use(role_id, tool_name, args).await;
        }
    }
    async fn on_tool_result(&self, role_id: &str, tool_name: &str, result: &str) {
        for r in &self.renderers {
            r.on_tool_result(role_id, tool_name, result).await;
        }
    }
    fn render_prompt(&self, icon: &str, role_id: &str, model_id: &str) -> String {
        if let Some(r) = self.renderers.first() {
            r.render_prompt(icon, role_id, model_id)
        } else {
            format!("{} {} · {}", icon, role_id, model_id)
        }
    }
}