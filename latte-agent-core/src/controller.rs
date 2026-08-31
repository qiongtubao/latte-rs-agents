//! ChatController: event-driven chat session driver.
//!
//! Replaces the CLI's stdin-based REPL (`run_hil_chat` + `run_hil_repl` +
//! `ChatCmd::run()`) with an async task that reads from an `mpsc` channel
//! and emits events via a `broadcast` channel.
//!
//! Two modes:
//! - **Single-role** (`roles.len() == 1`): one `AgentRunner`, loop of
//!   `submit_input → run_turn → RoleTurn`. Supports slash commands
//!   (`/role`, `/model`, `/clear`, `/save`, etc.).
//! - **Multi-role** (`roles.len() > 1`): full round-robin HIL mode with
//!   `RoundScheduler`, inject queue, plan slice, supervisor pause.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use latte_ai::models::Message;
use latte_ai::params::GenerateParams;
use latte_rs_agent_tools::types::{PropertyType, ToolInputProperty};
use tokio::sync::{broadcast, mpsc, Mutex};

use crate::advisor_monitor::{AdvisorMonitorConfig, AdvisorPauseGate, AdvisorReviewEngine, GateConfig};
use crate::agent::{Agent, AgentRunner};
use crate::config::AgentConfig;
use crate::error::AgentError;
use crate::model_resolver::{ModelResolver, ModelTier};
use crate::scheduler::{plan_md_slice_for, RoundScheduler};
use crate::session::{SessionManager, SessionRecord, SessionState};
use crate::subsession::SubsessionStore;
use crate::supervisor::{Supervisor, SupervisorConfig};
use crate::trace::ModelErrorKind;
use crate::workspace::WorkspaceManager;
use crate::AgentResult;

/// 把 `AgentError` 投影成 `ModelErrorKind`。`Controller` 端
/// 收到 `turn failed: {AgentError}` 时，把 e 转成 kind 写进
/// `ChatEvent::Error.kind`，让消费方拿到结构化分类。
///
/// 设计原则：**保真**优先于**聚合**——不同 AgentError 投影到
/// 不同 ModelErrorKind，缺口的走 `Other`。后续可以扩展 matches。
fn agent_error_to_kind(e: &AgentError) -> ModelErrorKind {
    match e {
        // Config 系：user 可修
        AgentError::Config(s) => ModelErrorKind::Config { message: s.clone() },
        AgentError::Template(role, _) => ModelErrorKind::Config { message: format!("template render: {role}") },
        AgentError::TemplateNotFound(p) => ModelErrorKind::Config { message: format!("template not found: {p}") },
        AgentError::RoleNotFound(r) => ModelErrorKind::Config { message: format!("role not found: {r}") },
        AgentError::ModelNotFound(m) => ModelErrorKind::Config { message: format!("model not found: {m}") },
        AgentError::ModelResolutionFailed { role, tier, reason } => {
            ModelErrorKind::Config { message: format!("resolution failed for {role}/{tier}: {reason}") }
        }
        AgentError::InvalidParam(s) => ModelErrorKind::Config { message: s.clone() },
        // AI client：转发 helper 的分类
        AgentError::AiClient(ai_err) => ModelErrorKind::from(ai_err),
        // 工具 / 上层
        AgentError::Tool(s) => ModelErrorKind::Other { message: format!("tool: {s}") },
        AgentError::TokenBudgetExceeded { .. } => ModelErrorKind::Config { message: "token budget exceeded".into() },
        AgentError::ToolLoopDetected { tool, .. } => ModelErrorKind::Other { message: format!("tool loop: {tool}") },
        AgentError::Orchestration(s) => ModelErrorKind::Other { message: format!("orchestration: {s}") },
        AgentError::ModelsUnavailable { tried, .. } => {
            // 告诉前端这是"全部模型都不可用"，已不是单点错误
            ModelErrorKind::Other { message: format!("all models unavailable (tried {})", tried.len()) }
        }
        AgentError::HookAborted { hook, .. } => ModelErrorKind::Other { message: format!("hook aborted: {hook}") },
        AgentError::Io(io) => ModelErrorKind::Other { message: format!("io: {io}") },
        // Advisor 终止（gate D5/D6 重试耗尽 或 LLM 复审返回 Terminate）。
        AgentError::AdvisorTerminated { reason, detector } => {
            ModelErrorKind::Other { message: format!("advisor terminated ({}): {}", detector, reason) }
        }
    }
}

/// 把任意 `AgentError` + 上下文 prefix 打包成 `ChatEvent::Error`，
/// 让 controller 端的 5 处 Error 构造点（turn failed / switch role /
/// switch model 等）走同一投影，UI/CLI 拿到一致的 `kind`。
/// `sub_id` 在错误来自某个 delegate subsession 时填入，让 UI
/// 右键「查看日志」能跳到具体 subagent 过程。
fn error_event(e: &AgentError, prefix: &str, sub_id: Option<String>) -> ChatEvent {
    ChatEvent::Error {
        kind: Some(agent_error_to_kind(e)),
        message: format!("{prefix}: {e}"),
        sub_id,
    }
}
fn truncate_event_text(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    format!("{}...[+{}B]", &text[..end], text.len() - end)
}

/// Strip `<think>…</think>` reasoning blocks from content surfaced to
/// the main session (RoleTurn bubbles, delegate/workflow returns to the
/// manager). The raw content stays in the subsession trace logs; this
/// only affects what humans and the manager see. An unclosed block
/// (model truncated mid-reasoning) is dropped to end-of-string. If
/// stripping would leave nothing, the original is returned so callers
/// never receive an empty payload.
pub(crate) fn strip_think_blocks(content: &str) -> String {
    if !content.contains("<think>") {
        return content.to_string();
    }
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(start) = rest.find("<think>") {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + "<think>".len()..];
        match after_open.find("</think>") {
            Some(end) => rest = &after_open[end + "</think>".len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    let stripped = out.trim();
    if stripped.is_empty() {
        content.to_string()
    } else {
        stripped.to_string()
    }
}

/// 判定分派/subagent 产出是否为"空"：空串或只剩模板壳
/// （`<response></response>`）。空产出不能算成功——下游会把空串
/// 当结论穿线（日志事故：architect 空返回被判 ok 进了讨论记录）。
pub(crate) fn is_empty_output(s: &str) -> bool {
    let t = s.trim();
    t.is_empty() || t == "<response></response>"
}

/// 剥掉 `gate_delegate_return` 追加的监察批注尾段
/// （"\n\n---\n⚠️ [监察审查]…"）。批注给人和 manager 看；写进
/// workflow 的 vars / 嵌套 topic 会污染下游 prompt（日志事故：
/// 「打回重做」批注原样成为 3 个子 workflow 的输入 topic）。
pub(crate) fn strip_review_annotation(s: &str) -> String {
    match s.find("\n\n---\n⚠️ [监察审查]") {
        Some(idx) => s[..idx].to_string(),
        None => s.to_string(),
    }
}

/// `pub` 而非 `pub(crate)`：CLI REPL 要用它把顶层 turn 的 TraceSink
/// 事件转成 ChatEvent 喂给 advisor monitor，而不必改 REPL 的渲染路径。
pub struct ChatEventTraceSink {
    pub event_tx: broadcast::Sender<ChatEvent>,
    /// 工具事件归属的 subsession：delegate / workflow speaker 的
    /// runner 填真实 sub_id，主 session 角色填 `None`。没有它
    /// ToolUse/ToolResult 泄进主 session 后前端只能按 role_id 猜。
    pub sub_id: Option<String>,
}

impl crate::trace::TraceSink for ChatEventTraceSink {
    fn emit(&self, event: crate::trace::TraceEvent) {
        match event {
            crate::trace::TraceEvent::ParseToolCalls { meta, parsed, .. } => {
                for call in parsed {
                    let _ = self.event_tx.send(ChatEvent::ToolUse {
                        role_id: meta.role.clone(),
                        tool_name: call.name,
                        args: truncate_event_text(&call.args, 1_200),
                        sub_id: self.sub_id.clone(),
                    });
                }
            }
            crate::trace::TraceEvent::ModelDelta { meta, delta } => {
                if !delta.is_empty() {
                    let _ = self.event_tx.send(ChatEvent::RoleTurn {
                        role_id: meta.role,
                        content: delta,
                        is_complete: false,
                        // 必须带 sub_id（同 ToolUse/ToolResult 分支）。
                        // 此前写死 None：delegate/workflow 子代理的逐
                        // token 流以主角色气泡的身份出现，而它的
                        // ToolUse/ToolResult 却带 sub_id，同一段对话被
                        // 拆到两处；并行波里两个同 role 分派的 token 还
                        // 会交错进同一个气泡（前端按 role_id+sub_id 建
                        // 流式气泡，None 会让它们撞同一个 key）。
                        sub_id: self.sub_id.clone(),
                    });
                }
            }
            crate::trace::TraceEvent::ModelCallSlow { meta, model_id, elapsed_secs } => {
                let _ = self.event_tx.send(ChatEvent::Status {
                    message: format!(
                        "⏳ {} 的模型调用（{}）已超过 {}s 未返回——多为厂商慢速生成或大上下文，不是卡死，仍在等待",
                        meta.role, model_id, elapsed_secs
                    ),
                });
            }
            crate::trace::TraceEvent::ToolExec {
                meta,
                name,
                status,
                ..
            } => match status {
                crate::trace::ToolStatus::Ok(result) => {
                    let _ = self.event_tx.send(ChatEvent::ToolResult {
                        role_id: meta.role,
                        tool_name: name,
                        result: truncate_event_text(&result, 1_500),
                        sub_id: self.sub_id.clone(),
                    });
                }
                crate::trace::ToolStatus::Err(error) => {
                    let _ = self.event_tx.send(ChatEvent::ToolError {
                        role_id: meta.role,
                        tool_name: name,
                        error: truncate_event_text(&error, 1_500),
                        sub_id: self.sub_id.clone(),
                    });
                }
            },
            _ => {}
        }
    }
}

// ─── Events ──────────────────────────────────────────────────────

/// Event emitted by the ChatController driver loop.
///
/// ## JSON wire format
///
/// Default serde representation (externally-tagged) is used:
/// `{"Status":{"message":"..."}}`. This is the canonical contract
/// consumed by `latte-code-editor` and the `chat --output json` JSONL
/// stream; the variant name is the outer object key.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum ChatEvent {
    /// Error message.
    ///
    /// `kind` 是结构化的错误分类（ModelErrorKind）—— 让 CLI / UI /
    /// 桌面端能基于**分类**做事（着色、自动建议、统计）。`message`
    /// 保留为人类可读原文。`kind` 是 `Option`，让旧 consumer 反
    /// 序列化旧 JSON（缺 kind 字段）时不报错；新构造点应填。
    ///
    /// `sub_id` (optional) 链接到产生此错误的 delegate subsession：
    /// UI 右键「查看日志」即可定位到该 subagent 的完整工具调用/
    /// bash 日志，而不是只看主 turn。主 turn 自身的错误保持 `None`。
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<ModelErrorKind>,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sub_id: Option<String>,
    },
    /// `sub_id` (optional) links this turn to a specific delegate subsession.
    RoleTurn {
        role_id: String,
        content: String,
        is_complete: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sub_id: Option<String>,
    },
    /// Informational / status message (replaces `eprintln!` / `println!`).
    Status { message: String },
    /// 用户发出的一条消息（send 路径；`/` 开头的斜杠命令不产生此事件）。
    /// 回放（UI 的 event_log / ui-sessions 落盘）靠它恢复用户气泡——
    /// 此前全部变体都是 agent 侧产出，回放里看不到用户自己发的内容。
    UserMessage { text: String },
    /// Prompt indicator (replaces `"👔 manager · deepseek-v4-flash › "`).
    Prompt {
        icon: String,
        role_id: String,
        model_id: String,
    },
    /// Session paused (by user or supervisor).
    Paused { reason: String },
    /// Session resumed.
    Resumed,
    /// Round started (multi-role mode).
    RoundStarted { round: u32 },
    /// Round ended (multi-role mode).
    RoundEnded { round: u32 },
    /// A role has started working.
    /// `sub_id` 标识本次运行所属的 subsession（delegate / workflow
    /// speaker 路径填真实值；主 session 角色的 turn 为 `None`）。
    /// 同一 role 可被并行 workflow 步同时委派，前端按
    /// `(role_id, sub_id)` 配对起止事件，不能只用 role_id。
    RoleStarted { role_id: String, detail: String, sub_id: Option<String> },
    /// A role has finished working. `sub_id` 与对应的
    /// [`ChatEvent::RoleStarted`] 一致。
    RoleFinished { role_id: String, detail: String, sub_id: Option<String> },
    /// A single role was individually paused (HIL v1.4, multi-role
    /// mode). Distinct from `Paused`, which halts the whole session.
    /// The paused role is skipped each round until `RoleResumed`.
    /// Drives the UI's per-role paused badge / toggle button.
    RolePaused { role_id: String },
    /// A single individually-paused role was resumed. Counterpart to
    /// `RolePaused`.
    RoleResumed { role_id: String },
    /// Session finished (aborted or quit).
    Done,
    RoleList {
        roles: Vec<RoleInfo>,
    },
    /// Context cleared (response to `/clear`).
    ContextCleared,
    /// Startup info: session identity + initial state.
    SessionInfo {
        task_id: String,
        state: String,
        turn: u32,
        roles: Vec<RoleInfo>,
    },
    /// Tool usage (for display in the editor UI).
    /// `sub_id` 标识工具调用所属的 subsession（delegate / workflow
    /// speaker 路径填真实值；主 session 角色为 `None`）。前端据此把
    /// 工具事件精确归属到 subagent，而不是按 role_id 猜。
    ToolUse {
        role_id: String,
        tool_name: String,
        args: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sub_id: Option<String>,
    },
    /// Tool result (for display in the editor UI).
    /// `sub_id` 与对应的 [`ChatEvent::ToolUse`] 一致。
    ToolResult {
        role_id: String,
        tool_name: String,
        result: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sub_id: Option<String>,
    },
    /// Manager decided to delegate a subtask to a specialist role.
    /// Emitted before the specialist runner starts so the UI can
    /// show "manager → programmer" without waiting for the
    /// (potentially multi-minute) specialist turn to complete.
    /// `sub_id` is a unique id for the subsession; the UI can
    /// `GET /api/sessions/{id}/subsessions/{sub_id}` to fetch the
    /// specialist's full internal transcript (tool calls, intermediate
    /// replies) — main chat only sees the final summary.
    DelegateStarted {
        from_role: String,
        to_role: String,
        task: String,
        sub_id: String,
        /// Workflow marker; absent for ordinary manager delegation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wf_id: Option<String>,
    },
    /// Specialist returned to the dispatching manager/workflow.
    DelegateFinished {
        from_role: String,
        to_role: String,
        status: String,
        summary: String,
        sub_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wf_id: Option<String>,
    },
    ToolError {
        role_id: String,
        tool_name: String,
        error: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sub_id: Option<String>,
    },
    /// Manager triggered a multi-role workflow (设计/plan/TDD/文档/图谱…).
    /// `wf_id` links all events of this run; the UI groups them.
    WorkflowStarted {
        name: String,
        topic: String,
        wf_id: String,
    },
    /// A workflow step is about to run (`index`/`total` are 1-based).
    /// `role_id` 是该步骤的第一个 speaker（供前端显示 @role）。
    WorkflowStep {
        wf_id: String,
        step_id: String,
        description: String,
        index: usize,
        total: usize,
        /// 该步骤的第一个 speaker（前端渲染为 @role）。
        role_id: String,
        /// 该步骤的任务描述（前端显示为 manager 的指派文本）。
        task: String,
    },
    /// One speaker's completed turn inside a workflow step.
    WorkflowTurn {
        wf_id: String,
        step_id: String,
        role_id: String,
        content: String,
        round: usize,
    },
    /// Workflow run finished. `status`: "ok" | "failed" | "cancelled".
    /// `summary` is the last turn's output (or the error message).
    WorkflowFinished {
        name: String,
        wf_id: String,
        status: String,
        summary: String,
    },
    /// 角色调用 generate_image 生成的图片。path 是 UI server 的
    /// `/api/images/<file>` URL（web UI 直接渲染 <img>）。
    ImageGenerated { role_id: String, path: String, prompt: String },
    /// 角色调用 `plan` 工具提交的任务候选清单。manager 在
    /// `implementation_plan` workflow 跑完（或手持一份具体任务清单）
    /// 后调 `plan`，把结构化任务交给用户在弹窗里勾选导入任务看板。
    /// `plan_id` 唯一标识本次提案，供 UI 去重与右键补救重开弹窗。
    /// `tasks` 与 `POST /api/tasks/import` 的 `ImportTask`（tasks.rs）
    /// 同构——前端选中后直接透传导入接口，无需文本解析。
    PlanProposed {
        role_id: String,
        plan_id: String,
        tasks: Vec<PlanTask>,
    },
    /// 角色调用 `ask` 工具向用户抛出一道选择题（含可选图片 / 图片
    /// 网格 / 允许上传自定义图片）。会话本轮 turn 就此结束，前端用
    /// 本事件渲染选择弹框；用户提交后把选择结果作为下一条 user 消息
    /// （经 `/chat/send`）回喂给角色，模型据此继续。
    ///
    /// `choice_id` 唯一标识本次提问，供前端去重与右键补救重开弹窗。
    /// `multi` 为 true 时允许多选。`layout` = `"grid"` 时前端按图片
    /// 网格渲染（否则列表）。`allow_upload` 为 true 时弹框提供上传
    /// 自定义图片的入口（图片经 `POST /api/images` 落盘后以 URL 回传）。
    /// `wait` 为 true 时表示提问方（workflow/delegate 子代理）正阻塞
    /// 等待答案——前端必须把答案 POST 到 `/api/chat/choice-answer`
    /// 直达等待方，而不是作为新 user 消息另起一轮。
    ChoiceRequested {
        role_id: String,
        choice_id: String,
        question: String,
        #[serde(default)]
        multi: bool,
        /// `"list"`（默认）或 `"grid"`（图片网格选择）。
        #[serde(default, skip_serializing_if = "String::is_empty")]
        layout: String,
        #[serde(default)]
        allow_upload: bool,
        /// true = 提问方正阻塞等回答（走 choice-answer 端点回传）；
        /// false/缺省 = fire-and-forget（回答作为下一条 user 消息）。
        #[serde(default)]
        wait: bool,
        options: Vec<ChoiceOption>,
    },
    /// 角色调用 `task_report` 工具回报任务执行结果（任务看板闭环）。
    /// 广播后由 ui-server 侧 `events_sse` 订阅器拦截，调内部
    /// `POST /api/tasks/:id/report`，把任务推进到 human_review
    /// （completed）或 todo（aborted/failed/timeout）。
    ///
    /// `result` 是 `completed` / `aborted` / `failed` / `timeout` 之一，
    /// 与后端 `tasks::report_task` 的入参对齐。
    TaskReport {
        role_id: String,
        task_id: String,
        summary: String,
        result: String,
    },
    /// Turn soft-timeout warning: the current turn has been running
    /// longer than the configured soft timeout but is still alive.
    /// The UI uses this to surface a "继续等待 / 终止当前任务" prompt
    /// instead of silently killing the turn. The hard kill fires at
    /// `hard_timeout_secs` if the user does not act.
    ///
    /// `role_id` identifies which role's turn is over-budget; for the
    /// main single-role loop this is the active role, for multi-role
    /// it is the role currently speaking. `sub_id` is set when the
    /// over-budget turn is a delegate subsession, so the UI's
    /// "查看日志" action can jump straight to the subagent transcript.
    TimeoutWarning {
        role_id: String,
        elapsed_secs: u64,
        soft_timeout_secs: u64,
        hard_timeout_secs: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sub_id: Option<String>,
    },

    /// Advisor 终止了一次 turn（**不**广播 `RoleTurn`，坏答案不
    /// 落盘）。两类触发源：
    /// 1. Pre-persistence gate（D5/D6）重试 `max_retries` 次仍命中
    ///    → runner raise `AgentError::AdvisorTerminated`，
    ///    `detector: Some("D6")` 等具体标签
    /// 2. LLM 复审返回 `Verdict::Terminate`（语义层判定问题严重
    ///    到需要停下让用户接管），`detector: Some("LLM")`
    ///
    /// UI 必须把它显示成"对话已暂停，请查看 advisor 反馈后继续"
    /// 的明确状态；用户输入新消息后 controller 正常处理。runner
    /// 不会自杀，driver 只是回到 `input_rx.recv()` 等用户输入。
    /// `sub_id` 在 sub-session 终止时填上。
    AdvisorTerminated {
        role_id: String,
        reason: String,
        /// 触发的 detector 标签（"D5" / "D6" / "LLM" / "D3" / "D4"）；
        /// `None` 表示来源未明确（例如 LLM 复审失败但降级返回时）
        detector: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sub_id: Option<String>,
    },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RoleInfo {
    pub id: String,
    pub name: String,
    pub icon: String,
}
/// `plan` 工具提交的单个任务候选。字段与后端 `ImportTask`
/// （`latte-agent-ui-server/src/tasks.rs`）及前端 `ImportTask`
/// （`api.ts`）同构——前端勾选后可直接透传 `POST /api/tasks/import`。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PlanTask {
    /// 任务标题，一句话，必填非空。
    pub title: String,
    /// 做什么 + 验收标准。空则不序列化（前端视作缺省）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// 优先级 1-4（1 最高）。空则不序列化。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
    /// 标签数组。空则不序列化。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// 执行该任务的 workflow 名（tdd_development/bug_triage/update_docs）。
    /// 轻量任务可空。空则不序列化。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// 任务涉及的文件/目录前缀（相对项目根）：并行执行时范围重叠的
    /// 任务会被任务看板拒绝派发（409）。空则不序列化（视作未声明）。
    ///
    /// `alias` 收编模型高频写错的同义字段名。实测会话里 manager 连撞
    /// 两次输出长度上限后，第三次把字段名写成 `involved_paths`，serde
    /// 静默丢弃整个数组 —— 结果 24 个任务全部 `paths: []`，
    /// [`validate_plan_paths`] 的幻觉路径校验与重叠校验双双退化成空
    /// 操作，"因为写错字段名所以校验通过"。别名 + 未知键回执
    /// （见 `register_plan_tool` 的 `PLAN_TASK_KNOWN_KEYS`）是两道防线。
    #[serde(
        default,
        alias = "involved_paths",
        alias = "involved_files",
        alias = "affected_paths",
        alias = "affected_files",
        alias = "file_paths",
        alias = "files",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub paths: Vec<String>,
    /// 子任务，同构，最多一层。空则不序列化。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subtasks: Vec<PlanTask>,
}

/// plan 阶段门（oh-my-pi plan-mode 写门禁在 delegate 层的等价物）：
/// manager 调 `plan` 工具提交任务清单后、用户在弹窗导入任务看板前，
/// 禁止 delegate 派发实现类角色（programmer*/devops*）。
/// 每个 [`ChatController`] session 一份，经 [`SharedPlanStage`] 共享给
/// plan/delegate 工具 handler 与 driver。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanStage {
    /// 初始 / 新决策周期：不拦截任何 delegate。
    Normal,
    /// `plan` 工具已提交任务清单（记录 plan_id），等待用户在弹窗导入。
    /// 此状态下实现类角色的 delegate 调用被工具层拒绝。
    PendingApproval { plan_id: String },
    /// 用户已通过 `POST /api/tasks/import` 导入该 plan 的任务
    /// （语义等同 Normal，不再拦截；记下被批准的 plan_id）。
    Approved { plan_id: String },
}

impl Default for PlanStage {
    fn default() -> Self {
        PlanStage::Normal
    }
}

/// session 内共享的 plan 阶段门状态（controller / 工具 handler / driver
/// 三方读写，锁内无 await，parking_lot 即可）。
pub type SharedPlanStage = Arc<parking_lot::RwLock<PlanStage>>;

/// 实现类角色判定：`programmer` / `programmer_*` / `devops` / `devops_*`。
/// 分析/设计/审查类（pm、architect、designer、reviewer、tester、
/// security、tech_writer 及其细分）不在此列。
fn is_implementation_role(role_id: &str) -> bool {
    role_id == "programmer"
        || role_id.starts_with("programmer_")
        || role_id == "devops"
        || role_id.starts_with("devops_")
}

/// plan 阶段门检查：`PendingApproval` 且目标是实现类角色时返回拒绝
/// 消息（含 plan_id）；其余情况放行（`None`）。
fn plan_gate_rejection(stage: &SharedPlanStage, role_id: &str) -> Option<String> {
    let guard = stage.read();
    match &*guard {
        PlanStage::PendingApproval { plan_id } if is_implementation_role(role_id) => {
            Some(format!(
                "任务清单尚未获用户批准（plan_id={plan_id}），请等待用户在弹窗导入任务看板，或先询问用户确认后再派发实现类任务（'{role_id}' 属于实现类角色）。"
            ))
        }
        _ => None,
    }
}

/// 宽松布尔解析：模型（尤其非头部模型）经常把 JSON schema 里的
/// boolean 发成字符串 `"true"` / `"false"`，或 0/1 数字。严格
/// `serde_json` 会直接报 `invalid type: string "true", expected a
/// boolean`，被 `classify_tool_execution_error` 归成 PermanentExec
/// （消息含 "invalid"）→ 不重试，一次工具调用就此废掉。
///
/// 实测实锤：tutor 在 `interview` 步给 `recommended` 发了
/// `"true"`，ask 直接失败，弹框没弹出。
///
/// 接受：真 bool、`"true"/"false"/"yes"/"no"/"1"/"0"/"on"/"off"`
/// （忽略大小写与首尾空白）、数字 0/1。其余返回 `None`。
pub(crate) fn lenient_bool(v: &serde_json::Value) -> Option<bool> {
    match v {
        serde_json::Value::Bool(b) => Some(*b),
        serde_json::Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" | "on" => Some(true),
            "false" | "no" | "0" | "off" => Some(false),
            _ => None,
        },
        serde_json::Value::Number(n) => match n.as_u64() {
            Some(0) => Some(false),
            Some(1) => Some(true),
            _ => None,
        },
        _ => None,
    }
}

/// `question` 文案是否已经**向用户承诺了多选**。
///
/// 为什么需要：模型经常把多选意图只写进问题文字里，却忘了传 `multi`
/// 参数（实测实锤：tutor 发的是
/// `{"question":"你最想深入哪条线？（可多选）","options":[…]}`，
/// 完全没有 `multi` 键）。此时 `multi` 缺省 false，前端老老实实渲染
/// 单选——用户看着"（可多选）"却只能点一个，是纯粹的承诺违背。
///
/// 语义：先看否定标记（"不可多选"/"仅单选"/"只能选一个"…），命中则
/// 明确是单选；否则命中任一多选标记即为 true。`multi` 显式为 true 时
/// 调用方不会走到这里；显式 false 但文案写着"可多选"属于自相矛盾，
/// 以**用户看得见的那句文案**为准（见 `register_ask_tool`）。
pub(crate) fn question_implies_multi(question: &str) -> bool {
    let q = question.to_ascii_lowercase();
    // 否定标记优先：注意 "不可多选" 本身含有 "多选" 子串，不先排除
    // 会被正向标记误命中。
    const NEGATIVE: [&str; 6] = [
        "不可多选",
        "不能多选",
        "非多选",
        "仅单选",
        "只能单选",
        "只能选一",
    ];
    if NEGATIVE.iter().any(|m| q.contains(m)) {
        return false;
    }
    const POSITIVE: [&str; 10] = [
        "可多选",
        "可以多选",
        "支持多选",
        "多选",
        "可复选",
        "可勾选多项",
        "选多项",
        "可选多个",
        "multi-select",
        "select all that apply",
    ];
    POSITIVE.iter().any(|m| q.contains(m))
}

/// serde 适配器：给 `ChoiceOption::recommended` 提供宽松布尔语义。
/// 无法识别的形态**不报错**，退化成 `false` —— 一个推荐角标不值得
/// 让整次提问失败。
fn de_lenient_bool<'de, D>(d: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = <serde_json::Value as serde::Deserialize>::deserialize(d)?;
    Ok(lenient_bool(&v).unwrap_or(false))
}

/// serde 适配器：把「优点/缺点」这类清单字段宽松地读成 `Vec<String>`。
/// 模型对清单的写法五花八门：裸数组、换行/分号分隔的长字符串、
/// `[{"text": "..."}]` 对象数组。严格类型会让整次提问失败，而一条
/// 优缺点不值得废掉弹框——无法识别的形态一律退化成空清单。
fn de_string_list<'de, D>(d: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = <serde_json::Value as serde::Deserialize>::deserialize(d)?;
    Ok(lenient_string_list(&v))
}

/// 见 [`de_string_list`]。抽出成独立函数便于单测。
fn lenient_string_list(v: &serde_json::Value) -> Vec<String> {
    /// 单条文本：去掉行首的 `- ` / `* ` / `• ` 项目符号并 trim。
    fn norm(s: &str) -> Option<String> {
        let t = s.trim().trim_start_matches(['-', '*', '•']).trim();
        (!t.is_empty()).then(|| t.to_string())
    }
    match v {
        // 长字符串：按换行 / 分号（中英文）切分。
        serde_json::Value::String(s) => s
            .split(['\n', ';', '；'])
            .filter_map(norm)
            .collect(),
        serde_json::Value::Array(a) => a
            .iter()
            .filter_map(|item| match item {
                serde_json::Value::String(s) => norm(s),
                // `[{"text": "..."}]` / `{"point": "..."}` 之类：取第一个
                // 非空字符串字段（不猜键名，取到即可）。
                serde_json::Value::Object(map) => map
                    .values()
                    .filter_map(|x| x.as_str())
                    .find_map(norm),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// `ask` 工具的单个选项。前端 `ChoiceRequested` 弹框逐项渲染。
/// 字段与前端 `api.ts` 的 `ChoiceOption` 同构。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ChoiceOption {
    /// 选项显示标签，必填非空。
    #[serde(alias = "title", alias = "name", alias = "text")]
    pub label: String,
    /// 可选补充说明，显示在标签下方。空则不序列化。
    /// `desc` 别名：模型常这么简写（实测实锤：programmer 发的
    /// 每一项都是 `desc`，严格字段名下说明文字被静默丢弃）。
    #[serde(
        default,
        alias = "desc",
        alias = "detail",
        skip_serializing_if = "String::is_empty"
    )]
    pub description: String,
    /// 可选配图 URL（一般是 `/api/images/<file>`）。列表模式显示为
    /// 缩略图，`layout="grid"` 时显示为大图卡片。空则不序列化。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub image: String,
    /// 标记为推荐项；前端加「推荐」角标。默认 false。
    /// 宽松布尔（见 [`lenient_bool`]）：`"true"` 这类字符串照收。
    #[serde(
        default,
        deserialize_with = "de_lenient_bool",
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub recommended: bool,
    /// 优点清单。前端「详情」按钮展开后逐条渲染（✅ 列表）。
    /// 宽松解析（见 [`de_string_list`]）：数组 / 换行字符串 / 对象数组都收。
    #[serde(
        default,
        alias = "advantages",
        alias = "pro",
        alias = "benefits",
        alias = "upsides",
        deserialize_with = "de_string_list",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub pros: Vec<String>,
    /// 缺点 / 风险清单。前端「详情」面板渲染（⚠️ 列表）。
    #[serde(
        default,
        alias = "disadvantages",
        alias = "con",
        alias = "risks",
        alias = "drawbacks",
        alias = "downsides",
        deserialize_with = "de_string_list",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub cons: Vec<String>,
    /// 详情补充说明（多行自由文本），与 pros/cons 一起在「详情」
    /// 面板里显示。`description` 是列表里那一行短说明，别混用。
    #[serde(
        default,
        alias = "detail_text",
        alias = "rationale",
        alias = "notes",
        skip_serializing_if = "String::is_empty"
    )]
    pub details: String,
}

// ─── Controller input ────────────────────────────────────────────

enum ControllerInput {
    Input(String),
    Pause,
    Resume,
    /// Session-level resume used by the V2 pause gate endpoint. Unlike
    /// `Resume`, this also reaches the multi-role driver's persisted
    /// `SessionManager` state, but does not emit `Resumed`: the gate
    /// listener/API owns that single user-visible event.
    ResumeSession(tokio::sync::oneshot::Sender<bool>),
    SwitchRole(String),
    SwitchModel(ModelTier),
    Abort,
    /// Cancel only the in-flight turn (the one currently waiting on
    /// the LLM). Distinct from `Abort`, which tears down the entire
    /// session. The UI sends this when the user picks "终止当前任务"
    /// from a `TimeoutWarning` prompt. The driver flips a per-turn
    /// flag, the run_turn future is dropped, and the session loop
    /// moves on to the next user input.
    CancelTurn,
    /// Advisor monitor correction hint. The driver pushes it into the
    /// current runners' shared in-memory hint queue (see
    /// `AgentRunner::with_advisor_hints`); it is NOT a new user
    /// question and never triggers a turn on its own.
    ///
    /// Note: `ChatController::advisor_hint` pushes into the shared
    /// queue *directly* (synchronous, mid-turn delivery); this
    /// variant exists so embedders that only hold the input channel
    /// can still route hints through the driver.
    #[allow(dead_code)] // constructed by embedders holding the raw input channel
    AdvisorHint(String),
}

// ─── Configuration ───────────────────────────────────────────────

/// Configuration for creating a ChatController.
#[derive(Clone)]
pub struct ControllerConfig {
    /// `Some(id)` → multi-role HIL mode on existing worktree.
    /// `None` → single-role mode (no SessionManager).
    pub task_id: Option<String>,
    /// Role IDs. `len() == 1` → single-role, `> 1` → multi-role HIL.
    pub roles: Vec<String>,
    /// Initial prompt (used only when creating a fresh HIL session).
    pub initial_prompt: Option<String>,
    /// Maximum rounds (multi-role mode). Default 10.
    pub max_rounds: u32,
    /// Session token budget for Supervisor. 0 = disabled.
    pub session_token_budget: u32,
    /// Merged agent config (models + roles).
    pub agent_config: Arc<AgentConfig>,
    /// Model resolver (built from merged config).
    pub model_resolver: Arc<ModelResolver>,
    /// Default generation params.
    pub default_params: GenerateParams,
    /// Pinned model id (overrides tier-based resolution).
    pub primary_model_id: Option<String>,
    /// Initial model tier override.
    pub initial_tier: Option<ModelTier>,
    /// Pre-existing conversation history to seed the runner with
    /// *before* the driver loop processes its first input. Used by
    /// `controller_runtime::start` to resume a previously-paused
    /// chat session from the SessionStore: the editor reads the
    /// stored `ChatEvent` mirror, converts to `latte_ai::models::Message`,
    /// and threads them through here so the next `run_turn` sees the
    /// full prior context.
    ///
    /// Empty in the fresh-session path. Order matches the original
    /// chat (oldest first). The driver does not re-emit these as
    /// `RoleTurn` events — the editor's `useChatStore` already
    /// has them locally.
    pub initial_history: Vec<latte_ai::models::Message>,
    /// Current working directory (for worktree resolution).
    pub cwd: PathBuf,
    /// UI session id（= UI 侧 SessionHandle.session_id），用于
    /// subsession_store 的落盘 key。**不要**用 `cwd` 凑数 —— cwd 是
    /// 绝对路径，作为文件目录名是非法且不稳定的。
    /// 不传：默认 empty（兼容老 caller；subsession 仍能跑但 cwd 路径
    /// 会被 disk sink 拒绝并降级到内存）。
    pub session_id: String,
    /// Shared store for per-task subsessions. Each delegate tool call
    /// allocates one entry under `(cwd_session, sub_id)` and stores
    /// the specialist's full TraceEvent stream so the UI can fetch it
    /// for "show contents". Owned by the caller (e.g. UiCmd::run);
    /// a single Arc is shared across all controllers regardless of
    /// tab/session, so GC works uniformly per process.
    pub subsession_store: Arc<SubsessionStore>,
    /// Advisor monitor (旁路监察者) settings. Default: enabled with
    /// `OnAnomaly` LLM review. The session creator (e.g. ui-server's
    /// `create_session_handle`) spawns the monitor when enabled.
    pub advisor_monitor: AdvisorMonitorConfig,
    /// 流式模式开关（运行时可切换）。`Arc<AtomicBool>` 让 UI/session 层
    /// 实时切换无需重建 controller。`load(true)` 时 run_turn 走流式
    /// 逐 Delta 渲染；`load(false)` 时走非流式（默认）。
    pub stream_mode: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// 单 session 内 manager/delegate 工具累计调用上限。超过后
    /// delegate 工具直接返回 `ClientError(\"delegate limit reached\")`。
    /// 0 = 禁用。默认 12（实测 §perf/diagnose-latency.md §1：
    /// manager 一次响应中重复 4 次 +0 资源的 delegate 拉低 50% 时长）。
    /// env `LATTE_MAX_DELEGATES_PER_SESSION` 覆盖；进程内 UI / workflow
    /// 路径都可读。
    pub max_delegates_per_session: u32,
}

// ─── Controller ──────────────────────────────────────────────────

/// Result of a V2 session resume request. A command that is still pending
/// acknowledgement is treated as live: starting checkpoint fallback while
/// that command can still be consumed would run two execution paths at once.
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionResumeOutcome {
    pub gate_paused_for_ms: Option<u128>,
    pub driver_resumed: bool,
    pub driver_command_pending: bool,
}

impl SessionResumeOutcome {
    pub fn is_live(self) -> bool {
        self.gate_paused_for_ms.is_some()
            || self.driver_resumed
            || self.driver_command_pending
    }
}

/// Event-driven chat session controller.
pub struct ChatController {
    input_tx: tokio::sync::Mutex<Option<mpsc::UnboundedSender<ControllerInput>>>,
    event_tx: broadcast::Sender<ChatEvent>,
    cancel_flag: Arc<AtomicBool>,
    /// Per-turn cancellation flag, distinct from `cancel_flag`
    /// (which aborts the whole session). The driver arms it before
    /// awaiting `run_turn` and clears it after the turn resolves; the
    /// UI sends `CancelTurn` to flip it. Lets the user abort just the
    /// currently-running turn (e.g. from a `TimeoutWarning` prompt)
    /// without losing the rest of the session.
    turn_cancel_flag: Arc<AtomicBool>,
    /// Shared advisor hint queue. The producer side is
    /// `advisor_hint()` (called by the AdvisorMonitor); the consumer
    /// side is every runner the driver builds
    /// (`AgentRunner::with_advisor_hints`), drained at turn start and
    /// at every tool-round boundary. Direct push (not the mpsc input
    /// channel) is what makes *mid-turn* delivery possible: in
    /// single-role mode the driver awaits `run_turn` and would not
    /// see an mpsc message until the turn finished.
    advisor_hints: Arc<parking_lot::Mutex<std::collections::VecDeque<String>>>,
    /// Most recent non-command user input, recorded by
    /// `submit_input`（workflow slash 命令的 topic 由 driver 在
    /// `run_single_role_loop` 里补记）. The AdvisorMonitor reads it to give the LLM
    /// review the user's current question (user input is not part of
    /// the `ChatEvent` broadcast stream).
    last_user_input: Arc<parking_lot::Mutex<String>>,
    /// plan 阶段门（见 [`PlanStage`]）：plan 工具 handler 置
    /// `PendingApproval`，ui-server 的任务导入置 `Approved`，driver
    /// 收到下一条用户消息复位 `Normal`。
    plan_stage: SharedPlanStage,
    /// Advisor v3 pause gate（intervene 暂停门）：monitor 判
    /// `Verdict::Intervene` 时经 `request_pause()` 置位；driver 给
    /// watched role 的主 runner 装配同一句柄
    /// （`AgentRunner::with_pause_gate`），runner 在 tool-round
    /// 边界挂起；任何用户输入到达（`submit_input`）即 resolve。
    advisor_pause: AdvisorPauseGate,
    /// 最近一次失败的 workflow 快照 —— UI-server 的
    /// `/api/workflows/resume` 直接读这个字段判断是否可续跑。
    last_failed_workflow: Arc<parking_lot::RwLock<Option<FailedWorkflow>>>,
    /// Session-level 暂停门（用户按 ⏸ 触发）。`Arc` 让 main driver
    /// + delegate 出去的 specialist 共享同一个 gate —— 用户暂停时
    /// 全部一起冻结。
    agent_pause_gate: Arc<crate::pause_gate::AgentPauseGate>,
    /// 最近一次 spawn 的配置快照——abort 后 respawn 复用。
    last_config: tokio::sync::Mutex<Option<ControllerConfig>>,
}

/// 最近一次失败 workflow 的快照。`wf_id` 对应
/// `<cwd>/.latte/workflow-runs/<wf_id>.jsonl` checkpoint 文件。
#[derive(Debug, Clone)]
pub struct FailedWorkflow {
    pub name: String,
    pub wf_id: String,
    pub summary: String,
    pub failed_at_unix_ms: u64,
}

impl ChatController {
    /// Create a new controller. Does NOT start the driver loop — call
    /// `spawn()` to begin.
    pub fn new(event_capacity: usize) -> Self {
        let (event_tx, _) = broadcast::channel(event_capacity);
        let agent_pause_gate = crate::pause_gate::AgentPauseGate::new("session");
        // 暂停事件统一由 gate 的 on_change 发出：用户手动 ⏸/▶
        // （pause_session/resume_session）与 runner 的自动暂停（模型
        // 不可用）走同一事件通道，UI 只认 Paused/Resumed。
        {
            let tx = event_tx.clone();
            let gate = agent_pause_gate.clone();
            agent_pause_gate.on_change(move |paused| {
                if paused {
                    let reason = gate
                        .pause_reason()
                        .filter(|r| !r.is_empty())
                        .unwrap_or_else(|| "session 已暂停".into());
                    let _ = tx.send(ChatEvent::Paused { reason });
                } else {
                    let _ = tx.send(ChatEvent::Resumed);
                }
            });
        }
        Self {
            input_tx: tokio::sync::Mutex::new(None),
            event_tx,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            turn_cancel_flag: Arc::new(AtomicBool::new(false)),
            advisor_hints: Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new())),
            last_user_input: Arc::new(parking_lot::Mutex::new(String::new())),
            plan_stage: Arc::new(parking_lot::RwLock::new(PlanStage::Normal)),
            advisor_pause: AdvisorPauseGate::new(),
            last_failed_workflow: Arc::new(parking_lot::RwLock::new(None)),
            agent_pause_gate,
            last_config: tokio::sync::Mutex::new(None),
        }
    }

    /// Start the driver loop. Returns a receiver that gets all events.
    pub async fn spawn(
        &self,
        config: ControllerConfig,
    ) -> broadcast::Receiver<ChatEvent> {
        // Reset session-level cancel flag so a previously-aborted
        // session does not instantly kill the new driver loop.
        self.cancel_flag.store(false, Ordering::SeqCst);
        self.turn_cancel_flag.store(false, Ordering::SeqCst);

        // Store config snapshot for potential respawn after abort.
        *self.last_config.lock().await = Some(config.clone());

        let (new_tx, input_rx) = mpsc::unbounded_channel();
        let _old = std::mem::replace(&mut *self.input_tx.lock().await, Some(new_tx));
        drop(_old);

        let event_tx = self.event_tx.clone();
        let cancel_flag = self.cancel_flag.clone();
        let turn_cancel_flag = self.turn_cancel_flag.clone();
        let advisor_hints = self.advisor_hints.clone();
        let plan_stage = self.plan_stage.clone();
        let advisor_pause = self.advisor_pause.clone();
        let agent_pause_gate = self.agent_pause_gate.clone();
        let last_user_input = self.last_user_input.clone();

        // 内部订阅者：跟踪 WorkflowFinished 事件，维护
        // last_failed_workflow 状态。
        let state_slot = self.last_failed_workflow.clone();
        let mut state_rx = self.event_tx.subscribe();
        tokio::spawn(async move {
            loop {
                let ev = match state_rx.recv().await {
                    Ok(ev) => ev,
                    // Lagged 可恢复：跳过丢掉的那批继续跟踪。此前当成
                    // 终止条件——一次突发之后 last_failed_workflow 冻在
                    // 旧值，"继续"按钮要么续跑早已成功的 run，要么拒绝
                    // 续跑真失败的那个。
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                if let ChatEvent::WorkflowFinished { name, wf_id, status, summary } = ev {
                    let mut slot = state_slot.write();
                    if status == "ok" {
                        *slot = None;
                    } else if !wf_id.is_empty() {
                        use std::time::{SystemTime, UNIX_EPOCH};
                        *slot = Some(FailedWorkflow {
                            name,
                            wf_id,
                            summary,
                            failed_at_unix_ms: SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .map(|d| d.as_millis() as u64)
                                .unwrap_or(0),
                        });
                    }
                }
            }
        });

        tokio::spawn(async move {
            run_driver(
                config,
                input_rx,
                &event_tx,
                cancel_flag,
                turn_cancel_flag,
                advisor_hints,
                plan_stage,
                advisor_pause,
                agent_pause_gate,
                last_user_input,
            )
            .await;
        });

        self.event_tx.subscribe()
    }

    /// Submit a line of user input.
    pub async fn submit_input(&self, text: &str) {
        // Record the latest genuine question for the advisor monitor's
        // LLM review input (slash commands are not questions).
        if !text.trim_start().starts_with('/') {
            *self.last_user_input.lock() = text.to_string();
        }
        // Advisor v3 pause gate：暂停期间任何用户输入都算拍板（选择
        // 弹窗的回答也作为普通用户消息回传）——先 resolve 唤醒挂起
        // 的 runner；文本命中"终止/stop/取消/别继续"时同时软终止
        // 当前 turn（turn_cancel_flag 由 run_turn_cancellable 的
        // 500ms tick 看到，driver 丢掉 in-flight turn 回到等输入）。
        if self.advisor_pause_requested() {
            let lower = text.to_lowercase();
            let stop = ["终止", "stop", "取消", "别继续"]
                .iter()
                .any(|k| lower.contains(k));
            self.advisor_pause.resolve();
            if stop {
                self.cancel_turn().await;
            }
        }
        if let Some(tx) = self.input_tx.lock().await.as_ref() {
            if tx.send(ControllerInput::Input(text.to_string())).is_err() {
                eprintln!(
                    "[ChatController] submit_input: driver channel closed (driver dead after abort?)"
                );
            }
        }
    }

    /// Push an advisor correction hint into the shared runner hint
    /// queue. Synchronous, lock-only — safe to call while a turn is
    /// running; the current runner drains it at the next tool-round
    /// boundary (or turn start when idle). Never triggers a turn.
    pub fn advisor_hint(&self, text: &str) {
        const MAX_PENDING_HINTS: usize = 16;
        let mut q = self.advisor_hints.lock();
        // Bound the backlog so a pathological monitor can't grow the
        // queue without limit; oldest hints are the least relevant.
        while q.len() >= MAX_PENDING_HINTS {
            q.pop_front();
        }
        q.push_back(text.to_string());
    }

    /// The shared advisor hint queue (driver wiring + tests).
    pub fn advisor_hint_queue(
        &self,
    ) -> Arc<parking_lot::Mutex<std::collections::VecDeque<String>>> {
        self.advisor_hints.clone()
    }

    /// The most recent user question recorded by `submit_input`.
    pub fn last_user_input(&self) -> String {
        self.last_user_input.lock().clone()
    }

    /// Record what the user asked *without* submitting a chat message.
    /// Entry points that drive the session directly (task-board
    /// workflow dispatch runs the workflow on the session event stream
    /// without `submit_input`) use this so the advisor monitor's LLM
    /// review still sees the original request instead of a blank.
    pub fn record_user_input(&self, text: &str) {
        *self.last_user_input.lock() = text.to_string();
    }

    /// 当前 plan 阶段门状态（见 [`PlanStage`]）。
    pub fn plan_stage(&self) -> PlanStage {
        self.plan_stage.read().clone()
    }

    /// 设置 plan 阶段门状态。调用方：`POST /api/tasks/import`
    /// （置 `Approved`）；driver / 工具 handler 走内部共享句柄直接写。
    pub fn set_plan_stage(&self, stage: PlanStage) {
        *self.plan_stage.write() = stage;
    }

    /// Clone of the event broadcast sender. Used by the AdvisorMonitor
    /// to publish its own `RoleTurn { role_id: "advisor" }` bubbles
    /// (channel B) into the same stream the UI consumes.
    pub fn event_sender(&self) -> broadcast::Sender<ChatEvent> {
        self.event_tx.clone()
    }

    /// 返回当前 session 的暂停门共享句柄。
    pub fn session_pause_gate(&self) -> Arc<crate::pause_gate::AgentPauseGate> {
        self.agent_pause_gate.clone()
    }

    /// 返回 per-turn 取消旗标的共享句柄，供**脱离 turn 单独 spawn**
    /// 的 workflow run（UI 续跑、任务看板派发）挂进
    /// `WorkflowRunContext::turn_cancel_flag`。
    ///
    /// 为什么需要：workflow step 已无 wall-clock 硬超时（超时只发
    /// `TimeoutWarning` 让用户拍板），若这些 run 传 `None`，卡住的
    /// step 就只能靠 session 级 `cancel_flag`（一按全杀）收拾。挂上
    /// 本旗标后，用户点「终止当前任务」（`cancel_turn`）能精确掐掉
    /// 当前 step 而保留 session。
    pub fn session_turn_cancel_flag(&self) -> Arc<AtomicBool> {
        self.turn_cancel_flag.clone()
    }

    /// 当前 session 是否处于用户手动暂停状态。
    pub fn is_session_paused(&self) -> bool {
        self.agent_pause_gate.is_paused()
    }
    /// Request pause after the current round completes.
    pub async fn pause(&self) {
        if let Some(tx) = self.input_tx.lock().await.as_ref() {
            let _ = tx.send(ControllerInput::Pause);
        }
    }

    /// Resume from paused state.
    pub async fn resume(&self) {
        // 用户显式恢复也算对 advisor pause gate 拍板。
        self.advisor_pause.resolve();
        if let Some(tx) = self.input_tx.lock().await.as_ref() {
            let _ = tx.send(ControllerInput::Resume);
        }
    }

    /// Advisor v3 pause gate：请求暂停 watched role 的主 runner。
    /// 由 AdvisorMonitor 在 `Verdict::Intervene` 时调用；runner 在
    /// 下一个 tool-round 边界挂起，直到用户拍板（`submit_input` /
    /// `resume` → resolve）或 10 分钟超时自动恢复。
    pub fn request_pause(&self) {
        self.advisor_pause.request();
    }

    /// engage —— turn / tool / round 边界一起冻结。返回是否"新"
    /// 进入暂停（幂等）。Paused 事件由 gate 的 on_change listener
    /// 统一广播（见 `ChatController::new`），这里不再直发。
    pub fn pause_session(&self) -> bool {
        self.agent_pause_gate.pause_with_reason("用户暂停 session")
    }
    /// Session-level 恢复（与 [`pause_session`] 配对）。返回 gate、
    /// driver ack 与 pending 状态，供 API 安全地区分 live resume 和
    /// cold-start checkpoint fallback。
    pub async fn resume_session(&self) -> SessionResumeOutcome {
        // 与 `resume()` 对齐：用户点「继续」同样算对 advisor pause gate
        // 拍板，否则 advisor 暂停（只置 advisor 门，不 engage
        // agent_pause_gate）永远无法通过 resume-session 解除。
        self.advisor_pause.resolve();
        let paused_for = self.agent_pause_gate.resume();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let sent = if let Some(tx) = self.input_tx.lock().await.as_ref() {
            tx.send(ControllerInput::ResumeSession(ack_tx)).is_ok()
        } else {
            false
        };
        if paused_for.is_some() || !sent {
            return SessionResumeOutcome {
                gate_paused_for_ms: paused_for,
                driver_resumed: false,
                driver_command_pending: sent,
            };
        }
        match tokio::time::timeout(Duration::from_secs(1), ack_rx).await {
            Ok(Ok(driver_resumed)) => SessionResumeOutcome {
                gate_paused_for_ms: None,
                driver_resumed,
                driver_command_pending: false,
            },
            // Sender disappeared: the driver cannot consume this command, so
            // checkpoint fallback remains safe.
            Ok(Err(_)) => SessionResumeOutcome::default(),
            // Timeout is ambiguous, not a negative ack. The command remains
            // queued and can still be consumed after this API returns, so it
            // must suppress fallback to avoid concurrent execution.
            Err(_) => SessionResumeOutcome {
                gate_paused_for_ms: None,
                driver_resumed: false,
                driver_command_pending: true,
            },
        }
    }

    /// 当前是否有未拍板的 advisor 暂停请求（测试与嵌入方断言用）。
    ///
    /// 曾经叫 `pause_requested()` —— 与当时同名的 `pause_requested`
    /// 字段（多角色 loop 的暂停旗标，实际是死字段）指的根本不是一回事，
    /// 谁去读那段代码都会先被这个同名坑一次。字段已删，方法改名。
    pub fn advisor_pause_requested(&self) -> bool {
        self.advisor_pause.is_requested()
    }

    /// pause gate 共享句柄（driver 给主 runner 装配用）。
    pub fn advisor_pause_gate(&self) -> AdvisorPauseGate {
        self.advisor_pause.clone()
    }

    /// 当前 session 最近一次失败的 workflow 快照。详见
    /// [`FailedWorkflow`]。UI-server 的 `/api/workflows/resume`
    /// 直接读这个字段判断"该不该续跑"，不扫 event_log。
    pub fn last_failed_workflow(&self) -> Option<FailedWorkflow> {
        self.last_failed_workflow.read().clone()
    }

    /// 直接写入"最近失败 workflow"快照 —— 仅供恢复路径（cold-start
    /// seed）使用。实时事件流维护走 controller 内部订阅者。
    pub fn set_last_failed_workflow(&self, fw: FailedWorkflow) {
        *self.last_failed_workflow.write() = Some(fw);
    }

    /// Switch to a different role (single-role mode only).
    pub async fn switch_role(&self, role_id: &str) {
        if let Some(tx) = self.input_tx.lock().await.as_ref() {
            let _ = tx.send(ControllerInput::SwitchRole(role_id.to_string()));
        }
    }

    /// Switch model tier (single-role mode only).
    pub async fn switch_model(&self, tier: ModelTier) {
        if let Some(tx) = self.input_tx.lock().await.as_ref() {
            let _ = tx.send(ControllerInput::SwitchModel(tier));
        }
    }

    /// Abort the session immediately.
    pub async fn abort(&self) {
        self.cancel_flag.store(true, Ordering::SeqCst);
        if let Some(tx) = self.input_tx.lock().await.as_ref() {
            let _ = tx.send(ControllerInput::Abort);
        }
    }
    /// Cancel only the in-flight turn (the one currently awaiting
    /// the LLM). Distinct from `abort`, which tears down the whole
    /// session. The driver flips `turn_cancel_flag`, aborts the
    /// `run_turn` JoinHandle, and the session loop moves on to
    /// the next user input. UI sends this when the user picks
    /// "终止当前任务" from a `TimeoutWarning` prompt; the
    /// AdvisorMonitor calls it on `Verdict::Terminate`.
    pub async fn cancel_turn(&self) {
        self.turn_cancel_flag.store(true, Ordering::SeqCst);
        // Also send CancelTurn input so that loops blocked on
        // input_rx.recv() (multi-role, single-role while idle)
        // see the signal and can break out.
        if let Some(tx) = self.input_tx.lock().await.as_ref() {
            let _ = tx.send(ControllerInput::CancelTurn);
        }
    }

    /// Whether the per-turn cancel flag is currently set. Exposed
    /// for tests and for embedders asserting advisor-terminate
    /// behavior; the driver clears the flag at each turn entry.
    pub fn turn_cancel_requested(&self) -> bool {
        self.turn_cancel_flag.load(Ordering::SeqCst)
    }

    /// Get a subscriber that receives all future events.
    pub fn subscribe(&self) -> broadcast::Receiver<ChatEvent> {
        self.event_tx.subscribe()
    }

    /// Returns `true` when the driver task has exited (the input
    /// channel's receiver has been dropped). After `abort()` the
    /// driver breaks out of its loop and drops `input_rx`; subsequent
    /// sends via `input_tx` would silently fail. Callers (e.g.
    /// `sessions.rs`) should use this to decide whether to respawn.
    pub async fn is_driver_dead(&self) -> bool {
        let guard = self.input_tx.lock().await;
        match guard.as_ref() {
            None => true, // never spawned
            Some(tx) => tx.is_closed(),
        }
    }

    /// Respawn the driver loop after a previous `abort()` killed it.
    /// Reuses the stored `ControllerConfig` from the last `spawn()`.
    /// Returns `Ok(())` on success or `Err` if no config was stored
    /// (i.e. `spawn()` was never called).
    ///
    /// This preserves the `event_tx` broadcast channel (archiver /
    /// SSE subscribers stay connected) and resets the cancel flags.
    /// Chat history is NOT preserved in the new driver — but since
    /// single-role mode keeps history in `AgentRunner` (which dies
    /// with the driver), loss is expected and consistent with the
    /// design. The UI event_log (archiver) retains the visible
    /// history for display purposes.
    pub async fn respawn(&self) -> Result<(), String> {
        let cfg = self
            .last_config
            .lock()
            .await
            .clone()
            .ok_or_else(|| "no stored config; spawn() was never called".to_string())?;
        self.spawn(cfg).await;
        Ok(())
    }
}

// ─── Wave config types ────────────────────────────────────────────

/// Workflow step configuration, used for wave dependency analysis.
/// Mirrors the gsd-core workflow step concept with optional input/output
/// contract tracking via `FileContractConfig`.
#[derive(Debug, Clone)]
pub struct WorkflowStepConfig {
    pub id: String,
    pub speakers: Vec<String>,
    pub prompt: String,
    pub hooks: Vec<super::config::StepHookConfig>,
    pub contract: Option<FileContractConfig>,
}

impl ContractAccess for WorkflowStepConfig {
    fn contract_input(&self) -> Option<&str> {
        self.contract.as_ref().and_then(|c| c.input.as_deref())
    }
    fn contract_output(&self) -> Option<&str> {
        self.contract.as_ref().and_then(|c| c.output.as_deref())
    }
}

/// File-based contract between workflow steps. `input` declares a file
/// this step reads (making it dependent on the step that produces it);
/// `output` declares a file this step produces (making later steps
/// dependent on it).
#[derive(Debug, Clone)]
pub struct FileContractConfig {
    pub input: Option<String>,
    pub output: Option<String>,
    pub placeholder: Option<String>,
    pub extract_prompt: Option<String>,
    pub required: bool,
}

/// Trait for types that can be used in wave dependency analysis.
/// Implemented by any step type with optional input/output contracts.
pub trait ContractAccess {
    fn contract_input(&self) -> Option<&str>;
    fn contract_output(&self) -> Option<&str>;
}

/// A group of workflow steps that execute concurrently within a single
/// wave. Steps in the same wave have no inter-dependency on contract
/// outputs.
#[derive(Debug, Clone)]
pub struct Wave {
    pub index: usize,
    pub steps: Vec<usize>,
}

/// Result of wave analysis: an ordered sequence of waves.
#[derive(Debug, Clone, Default)]
pub struct WavePlan {
    pub waves: Vec<Wave>,
}

/// Compute the wave execution plan for a list of workflow steps.
/// Steps that produce no output (contract_output is None) are treated
/// as terminal steps — they can depend on prior outputs but no other
/// step depends on them.
pub fn compute_waves<S: ContractAccess>(steps: &[S]) -> WavePlan {
    if steps.is_empty() {
        return WavePlan::default();
    }

    // Build a map: output file → step index
    let output_to_step: std::collections::HashMap<&str, usize> = steps
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.contract_output().map(|path| (path, i)))
        .collect();

    // Compute in-degree (number of dependencies) for each step
    let mut in_degree: Vec<usize> = vec![0; steps.len()];
    let mut depends_on: Vec<Vec<usize>> = vec![vec![]; steps.len()];

    for (i, step) in steps.iter().enumerate() {
        if let Some(input) = step.contract_input() {
            if let Some(&producer) = output_to_step.get(input) {
                in_degree[i] += 1;
                depends_on[i].push(producer);
            }
        }
    }

    // Kahn's algorithm: topological sort into waves
    let mut plan = WavePlan::default();
    let mut remaining: std::collections::VecDeque<usize> = steps
        .iter()
        .enumerate()
        .filter(|(i, _)| in_degree[*i] == 0)
        .map(|(i, _)| i)
        .collect();

    let mut visited = 0;

    while !remaining.is_empty() {
        let wave_steps: Vec<usize> = remaining.drain(..).collect();
        visited += wave_steps.len();
        plan.waves.push(Wave { index: plan.waves.len(), steps: wave_steps });

        for &wave_idx in plan.waves.last().unwrap().steps.iter() {
            for (next_idx, deps) in depends_on.iter().enumerate() {
                if deps.contains(&wave_idx) {
                    in_degree[next_idx] = in_degree[next_idx].saturating_sub(1);
                    if in_degree[next_idx] == 0 && !remaining.contains(&next_idx) {
                        remaining.push_back(next_idx);
                    }
                }
            }
        }
    }

    if visited < steps.len() {
        let orphans: Vec<usize> = (0..steps.len())
            .filter(|i| in_degree[*i] > 0)
            .collect();
        plan.waves.push(Wave { index: plan.waves.len(), steps: orphans });
    }

    plan
}





/// Run a turn with cancellation support. Polls `run_turn_gated` at 500ms
/// ticks and checks `cancel_flag` (session abort) + `turn_cancel_flag`
/// (current turn cancel). On cancellation the in-flight LLM call is
/// dropped via the future's Drop impl. Returns `Ok(response)` or the
/// runner's real `Err(AgentError)` — in particular
/// `AgentError::AdvisorTerminated` from the pre-persistence gate
/// propagates unchanged so the driver can emit
/// `ChatEvent::AdvisorTerminated` instead of a generic failure.
/// No timeout — the user must cancel explicitly.
async fn run_turn_cancellable(
    runner: &mut AgentRunner,
    msgs: &[Message],
    cancel_flag: &AtomicBool,
    turn_cancel_flag: &AtomicBool,
) -> Result<String, AgentError> {
    turn_cancel_flag.store(false, Ordering::SeqCst);
    // run_turn_gated 在 runner 未装 gate_config 时等价于 run_turn，
    // 未启用 advisor 的场景行为不变。
    let mut fut = Box::pin(runner.run_turn_gated(msgs, None));
    enum TurnOutcome {
        Done(Result<String, AgentError>),
        SessionCancelled,
        TurnCancelled,
    }
    let outcome = loop {
        tokio::select! {
            biased;
            _ = tokio::time::sleep(Duration::from_millis(500)) => {
                if cancel_flag.load(Ordering::SeqCst) {
                    break TurnOutcome::SessionCancelled;
                }
                if turn_cancel_flag.load(Ordering::SeqCst) {
                    break TurnOutcome::TurnCancelled;
                }
            }
            r = fut.as_mut() => {
                break TurnOutcome::Done(r);
            }
        }
    };
    drop(fut);
    match outcome {
        TurnOutcome::Done(r) => r,
        TurnOutcome::SessionCancelled => {
            Err(AgentError::Orchestration("session cancelled by user".into()))
        }
        TurnOutcome::TurnCancelled => {
            Err(AgentError::Orchestration("turn cancelled by user".into()))
        }
    }
}

async fn run_driver(
    config: ControllerConfig,
    mut input_rx: mpsc::UnboundedReceiver<ControllerInput>,
    event_tx: &broadcast::Sender<ChatEvent>,
    cancel_flag: Arc<AtomicBool>,
    turn_cancel_flag: Arc<AtomicBool>,
    advisor_hints: Arc<parking_lot::Mutex<std::collections::VecDeque<String>>>,
    plan_stage: SharedPlanStage,
    advisor_pause: AdvisorPauseGate,
    agent_pause_gate: std::sync::Arc<crate::pause_gate::AgentPauseGate>,
    last_user_input: Arc<parking_lot::Mutex<String>>,
) {
    let is_multi = config.roles.len() > 1 || config.task_id.is_some();

    if is_multi {
        run_multi_role_loop(
            config,
            &mut input_rx,
            event_tx,
            cancel_flag,
            turn_cancel_flag,
            advisor_hints,
            plan_stage,
            advisor_pause,
            agent_pause_gate,
            last_user_input.clone(),
        )
        .await;
    } else {
        run_single_role_loop(
            config,
            &mut input_rx,
            event_tx,
            cancel_flag,
            turn_cancel_flag,
            advisor_hints,
            plan_stage,
            advisor_pause,
            agent_pause_gate,
            last_user_input,
        )
        .await;
    }

    let _ = event_tx.send(ChatEvent::Done);
}

// ─── Multi-role HIL mode ─────────────────────────────────────────

/// 多角色 round loop 的实际轮次上限。`0` = 不限（与单角色
/// `loop {}` 的语义一致）。
///
/// 为什么需要这层转换：round loop 写作 `1..=limit`，`limit == 0` 时
/// `1..=0` 是**空区间**，循环体一次都不跑 → `run_driver` 立刻返回 →
/// driver task 结束、`input_rx` 被 drop → 之后所有 `input_tx.send()`
/// 静默失败（调用点都是 `let _ = ...`），session 出生即哑，而
/// `/api/chat/send` 照样返回 202。ui-server 建 session 时传的正是
/// `max_rounds: 0`（`sessions.rs`），只因它同时是单角色配置才没踩到。
fn effective_round_limit(max_rounds: u32) -> u32 {
    if max_rounds == 0 {
        u32::MAX
    } else {
        max_rounds
    }
}

async fn run_multi_role_loop(
    config: ControllerConfig,
    input_rx: &mut mpsc::UnboundedReceiver<ControllerInput>,
    event_tx: &broadcast::Sender<ChatEvent>,
    cancel_flag: Arc<AtomicBool>,
    turn_cancel_flag: Arc<AtomicBool>,
    advisor_hints: Arc<parking_lot::Mutex<std::collections::VecDeque<String>>>,
    plan_stage: SharedPlanStage,
    advisor_pause: AdvisorPauseGate,
    agent_pause_gate: std::sync::Arc<crate::pause_gate::AgentPauseGate>,
    // 与 ChatController 共享的原始用户诉求句柄（委派返回审查的准绳）。
    last_user_input: Arc<parking_lot::Mutex<String>>,
) {
    let repo_root = match WorkspaceManager::resolve_repo_root(&config.cwd) {
        Ok(root) => root,
        Err(e) => {
            let _ = event_tx.send(ChatEvent::Error {
                            kind: None,
                            message: format!("无法解析仓库根目录: {e}"),
                            sub_id: None,
                        });
            return;
        }
    };
    let task_id = match &config.task_id {
        Some(id) => id.clone(),
        None => {
            let _ = event_tx.send(ChatEvent::Error {
                            kind: None,
                            message: "multi-role 模式需要 task_id".into(),
                            sub_id: None,
                        });
            return;
        }
    };
    let worktree_root = repo_root.join(".latte").join("worktrees").join(&task_id);

    // Load or create SessionManager
    let mut session_mgr =
        SessionManager::new(&task_id, worktree_root.clone(), config.roles.clone());
    if session_mgr.session_path().exists() {
        let raw = match std::fs::read_to_string(session_mgr.session_path()) {
            Ok(r) => r,
            Err(e) => {
                let _ = event_tx.send(ChatEvent::Error {
                                kind: None,
                                message: format!("读取 session 文件失败: {e}"),
                                sub_id: None,
                            });
                return;
            }
        };
        let record: SessionRecord = match serde_json::from_str(&raw) {
            Ok(r) => r,
            Err(e) => {
                let _ = event_tx.send(ChatEvent::Error {
                                kind: None,
                                message: format!("解析 session JSON 失败: {e}"),
                                sub_id: None,
                            });
                return;
            }
        };
        session_mgr = SessionManager::from_record(record, worktree_root.clone());
        let _ = event_tx.send(ChatEvent::Status {
            message: format!(
                "[session: {}, state: {:?}, turn: {}]",
                session_mgr.record().task_id,
                session_mgr.state(),
                session_mgr.record().current_turn
            ),
        });
        if session_mgr.state() == SessionState::Paused {
            if let Err(e) = session_mgr.resume() {
                let _ = event_tx.send(ChatEvent::Error {
                                kind: None,
                                message: format!("resume 失败: {e}"),
                                sub_id: None,
                            });
                return;
            }
            let _ = event_tx.send(ChatEvent::Status {
                message: format!("[RESUMED at {}]", session_mgr.record().updated_at),
            });
        }
    } else {
        let prompt = match &config.initial_prompt {
            Some(p) => p.clone(),
            None => {
                let _ = event_tx.send(ChatEvent::Error {
                                kind: None,
                                message: format!("task '{task_id}' 无 session，需要 initial_prompt"),
                                sub_id: None,
                            });
                return;
            }
        };
        let bb = crate::workspace::Blackboard::new(worktree_root.join("plan.md"));
        if let Err(e) = bb.write(&format!("# {task_id}\n\n{prompt}")) {
            let _ = event_tx.send(ChatEvent::Error {
                            kind: None,
                            message: format!("写入 plan.md 失败: {e}"),
                            sub_id: None,
                        });
            return;
        }
        if let Err(e) = session_mgr.persist() {
            let _ = event_tx.send(ChatEvent::Error {
                            kind: None,
                            message: format!("持久化 session 失败: {e}"),
                            sub_id: None,
                        });
            return;
        }
        let _ = event_tx.send(ChatEvent::Status {
            message: format!("[session: {task_id}, state: Created, turn: 0]"),
        });
        let _ = event_tx.send(ChatEvent::Status {
            message: format!("[roles: {}]", config.roles.join(", ")),
        });
    }

    // Build shared session Arc
    let session_arc: Arc<Mutex<SessionManager>> = Arc::new(Mutex::new(session_mgr));

    // Build RoundScheduler
    let scheduler_arc = session_arc.clone();
    let mut scheduler = match tokio::task::spawn_blocking(move || {
        RoundScheduler::new(scheduler_arc)
    })
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            let _ = event_tx.send(ChatEvent::Error {
                            kind: None,
                            message: format!("scheduler init error: {e}"),
                            sub_id: None,
                        });
            return;
        }
        Err(e) => {
            let _ = event_tx.send(ChatEvent::Error {
                            kind: None,
                            message: format!("scheduler join error: {e}"),
                            sub_id: None,
                        });
            return;
        }
    };
    scheduler.max_rounds = config.max_rounds;
    scheduler.supervisor = Supervisor::new(SupervisorConfig {
        session_token_budget: config.session_token_budget,
        dead_loop_window: 3,
    });

    let order = scheduler.order.clone();

    // Emit session info
    {
        let mgr = session_arc.lock().await;
        let roles_info: Vec<RoleInfo> = mgr
            .record()
            .roles
            .iter()
            .map(|r| RoleInfo {
                id: r.role_id.clone(),
                name: r.role_id.clone(),
                icon: role_icon(&r.role_id),
            })
            .collect();
        let _ = event_tx.send(ChatEvent::SessionInfo {
            task_id: task_id.clone(),
            state: format!("{:?}", mgr.state()),
            turn: mgr.record().current_turn,
            roles: roles_info,
        });
    }

    // Build per-role AgentRunners
    let merged = &config.agent_config;
    let resolver = &config.model_resolver;
    let default_params = &config.default_params;
    let mut runners: Vec<(String, AgentRunner)> = Vec::with_capacity(order.len());

    for role_id in &order {
        let tier_str = config
            .initial_tier
            .as_ref()
            .map(|t| t.label().to_string())
            .or_else(|| {
                merged
                    .roles
                    .get(role_id)
                    .map(|r| r.model_tier.clone())
            })
            .unwrap_or_else(|| "standard".to_string());
        let tier = ModelTier::parse(&tier_str).unwrap_or(ModelTier::Standard);

        match build_runner(
            merged,
            resolver,
            default_params,
            role_id,
            tier,
            config.primary_model_id.as_deref(),
            event_tx,
            &config.cwd,
            config.subsession_store.clone(),
            &config.session_id,
            cancel_flag.clone(),
            turn_cancel_flag.clone(),
            last_user_input.clone(),
            config.advisor_monitor.runner_gate(),
            advisor_pause.clone(),
            &plan_stage,
            agent_pause_gate.clone(),
            config.stream_mode.clone(),
        )
        .await
        {
            Ok((mut runner, _canonical_id)) => {
                runner = runner.with_advisor_hints(advisor_hints.clone());
                runners.push((role_id.clone(), runner));
            }
            Err(e) => {
                let _ = event_tx.send(ChatEvent::Error {
                                kind: None,
                                message: format!("构建 role '{role_id}' runner 失败: {e}"),
                                sub_id: None,
                            });
                return;
            }
        }
    }

    // ─── Round loop ────────────────────────────────────────────
    let round_limit = effective_round_limit(scheduler.max_rounds);
    'rounds: for round_num in 1..=round_limit {
        if cancel_flag.load(Ordering::SeqCst) {
            break 'rounds;
        }

        // Wait for user input
        let line = loop {
            if cancel_flag.load(Ordering::SeqCst) {
                break 'rounds;
            }
            match input_rx.recv().await {
                Some(ControllerInput::Input(text)) => {
                    let trimmed = text.trim().to_string();
                    if !trimmed.is_empty() {
                        break trimmed;
                    }
                }
                Some(ControllerInput::Pause) => {
                    {
                        let mut mgr = session_arc.lock().await;
                        let _ = mgr.pause("user /pause");
                    }
                    let _ = event_tx.send(ChatEvent::Paused {
                        reason: "用户暂停".into(),
                    });
                    // 原地挂起等 Resume，**不要** `break 'rounds`：
                    // break 会让 run_driver 返回、driver task 结束、
                    // input_rx 被 drop —— 之后所有 send 静默失败，
                    // session 永久变哑（/api/chat/send 仍回 202）。
                    // 暂停是可恢复状态，不是 session 终止。
                    loop {
                        if cancel_flag.load(Ordering::SeqCst) {
                            break 'rounds;
                        }
                        match input_rx.recv().await {
                            Some(ControllerInput::Resume) => {
                                {
                                    let mut mgr = session_arc.lock().await;
                                    let _ = mgr.resume();
                                }
                                let _ = event_tx.send(ChatEvent::Resumed);
                                break;
                            }
                            Some(ControllerInput::ResumeSession(ack)) => {
                                let resumed = {
                                    let mut mgr = session_arc.lock().await;
                                    mgr.resume().is_ok()
                                };
                                let _ = ack.send(resumed);
                                break;
                            }
                            Some(ControllerInput::Abort) | None => break 'rounds,
                            Some(ControllerInput::AdvisorHint(t)) => {
                                advisor_hints.lock().push_back(t);
                            }
                            // 暂停期间的其它输入（含重复 Pause）忽略。
                            _ => {}
                        }
                    }
                }
                Some(ControllerInput::Abort) => break 'rounds,
                // 取消当前 turn：仅打断正在跑的 run_turn，不退出
                // 整个 round 循环。turn_cancel_flag 会被 run_turn
                // 的 select! 循环检测到，丢弃该轮结果；下一轮用户
                // 输入照常接收。
                Some(ControllerInput::CancelTurn) => {
                    turn_cancel_flag.store(true, Ordering::SeqCst);
                }
                // ▶ 在 round 边界到达：如果 SessionRecord 还停在 Paused，
                // 这里必须真的 resume 它。
                //
                // 此前是空 arm（`=> {}`），于是 supervisor 的**自动**暂停
                // （预算超支 / dead-loop：`pause_with_reason` +
                // `continue 'rounds`）成了不可恢复状态：记录停在 Paused，
                // 之后每轮所有角色都被「session not running」跳过，用户
                // 按 ▶ 无效、发消息也无效 —— 会话永久空转（照收消息、照
                // 发 RoundStarted，但永远没产出）。用户手动 /pause 走上面
                // 那个原地 park 分支，不受影响；这里补的是自动暂停那条路。
                Some(ControllerInput::Resume) => {
                    let resumed = {
                        let mut mgr = session_arc.lock().await;
                        if mgr.state() == SessionState::Paused {
                            let _ = mgr.resume();
                            true
                        } else {
                            false
                        }
                    };
                    if resumed {
                        let _ = event_tx.send(ChatEvent::Resumed);
                    }
                }
                Some(ControllerInput::ResumeSession(ack)) => {
                    let resumed = {
                        let mut mgr = session_arc.lock().await;
                        if mgr.state() == SessionState::Paused {
                            let _ = mgr.resume();
                            true
                        } else {
                            false
                        }
                    };
                    let _ = ack.send(resumed);
                }
                Some(ControllerInput::AdvisorHint(text)) => {
                    advisor_hints.lock().push_back(text);
                }
                Some(ControllerInput::SwitchRole(_)) | Some(ControllerInput::SwitchModel(_)) => {
                    let _ = event_tx.send(ChatEvent::Status {
                        message: "multi-role 模式不支持 /role 或 /model 命令".into(),
                    });
                }
                None => break 'rounds,
            }
        };

        // Process line: slash command, @role, or manager input
        if line.starts_with('/') {
            match line.as_str() {
                "/exit" | "/quit" => break 'rounds,
                "/pause" => {
                    {
                        let mut mgr = session_arc.lock().await;
                        let _ = mgr.pause("user /pause");
                    }
                    let _ = event_tx.send(ChatEvent::Paused {
                        reason: "用户暂停".into(),
                    });
                    // 同上：暂停原地挂起等 Resume，不 break 'rounds。
                    // break 会 drop input_rx 让 session 永久变哑。
                    loop {
                        if cancel_flag.load(Ordering::SeqCst) {
                            break 'rounds;
                        }
                        match input_rx.recv().await {
                            Some(ControllerInput::Resume) => {
                                {
                                    let mut mgr = session_arc.lock().await;
                                    let _ = mgr.resume();
                                }
                                let _ = event_tx.send(ChatEvent::Resumed);
                                break;
                            }
                            Some(ControllerInput::ResumeSession(ack)) => {
                                let resumed = {
                                    let mut mgr = session_arc.lock().await;
                                    mgr.resume().is_ok()
                                };
                                let _ = ack.send(resumed);
                                break;
                            }
                            Some(ControllerInput::Abort) | None => break 'rounds,
                            Some(ControllerInput::AdvisorHint(t)) => {
                                advisor_hints.lock().push_back(t);
                            }
                            _ => {}
                        }
                    }
                    continue 'rounds;
                }
                // Per-role pause (HIL v1.4): flag one role and keep the
                // session running for the others. Orthogonal to the
                // global `/pause` above.
                line if line.starts_with("/pause ") => {
                    let role_id = line["/pause ".len()..].trim().to_string();
                    let mut mgr = session_arc.lock().await;
                    if !mgr.record().roles.iter().any(|r| r.role_id == role_id) {
                        let _ = event_tx.send(ChatEvent::Status {
                            message: format!("[error: unknown role '{role_id}']"),
                        });
                    } else {
                        let _ = mgr.pause_role(&role_id, &format!("user /pause {role_id}"));
                        let _ = event_tx.send(ChatEvent::RolePaused {
                            role_id: role_id.clone(),
                        });
                    }
                    continue 'rounds;
                }
                // Per-role resume (HIL v1.4): clear one role's pause flag.
                "/resume" => {
                    let mgr = session_arc.lock().await;
                    let paused = mgr.paused_roles();
                    if paused.is_empty() {
                        let _ = event_tx.send(ChatEvent::Status {
                            message: "[no individually-paused roles — use /resume <role>]".into(),
                        });
                    } else {
                        let _ = event_tx.send(ChatEvent::Status {
                            message: format!("[paused roles: {} — use /resume <role>]", paused.join(", ")),
                        });
                    }
                    continue 'rounds;
                }
                line if line.starts_with("/resume ") => {
                    let role_id = line["/resume ".len()..].trim().to_string();
                    let mut mgr = session_arc.lock().await;
                    if !mgr.record().roles.iter().any(|r| r.role_id == role_id) {
                        let _ = event_tx.send(ChatEvent::Status {
                            message: format!("[error: unknown role '{role_id}']"),
                        });
                    } else if mgr.is_role_paused(&role_id) {
                        let _ = mgr.resume_role(&role_id);
                        let _ = event_tx.send(ChatEvent::RoleResumed {
                            role_id: role_id.clone(),
                        });
                    } else {
                        let _ = event_tx.send(ChatEvent::Status {
                            message: format!("[role '{role_id}' is not paused]"),
                        });
                    }
                    continue 'rounds;
                }
                "/roles" => {
                    let mut roles_info: Vec<RoleInfo> = Vec::new();
                    for id in &order {
                        let icon = merged
                            .roles
                            .get(id)
                            .map(|r| r.icon.clone())
                            .unwrap_or_else(|| role_icon(id));
                        roles_info.push(RoleInfo {
                            id: id.clone(),
                            name: id.clone(),
                            icon,
                        });
                    }
                    let _ = event_tx.send(ChatEvent::RoleList { roles: roles_info });
                    continue 'rounds;
                }
                line if line.starts_with("/rounds") => {
                    let mgr = session_arc.lock().await;
                    let limit = if scheduler.max_rounds == 0 {
                        "∞".to_string()
                    } else {
                        scheduler.max_rounds.to_string()
                    };
                    let _ = event_tx.send(ChatEvent::Status {
                        message: format!("[round: {} / {}]", mgr.record().current_turn, limit),
                    });
                    continue 'rounds;
                }
                cmd => {
                    let parts: Vec<&str> = line.splitn(2, ' ').collect();
                    let topic = parts.get(1).unwrap_or(&"").trim();
                    match run_workflow_command(cmd, topic, &config, &event_tx, cancel_flag.clone(), agent_pause_gate.clone(), turn_cancel_flag.clone(), advisor_pause.clone()).await {
                        Ok(Some(summary)) => {
                            let _ = event_tx.send(ChatEvent::Status {
                                message: format!("Workflow '{cmd}' 完成。结果已交给 manager 处理。"),
                            });
                            let inject_dir = worktree_root.join(".latte").join("inject");
                            let _ = std::fs::create_dir_all(&inject_dir);
                            let inject_path = inject_dir.join("manager.txt");
                            let mgr_text = format!(
                                "Workflow '{cmd}' 已完成。用户请求：{topic}\n\n结果：\n{summary}\n\n请根据结果给出结论或下一步。"
                            );
                            let _ = std::fs::write(&inject_path, &mgr_text);
                            continue 'rounds;
                        }
                        Ok(None) => {
                            let _ = event_tx.send(ChatEvent::Status {
                                message: format!(
                                    "[unknown {cmd} — known: pause resume quit roles rounds, plus any workflow command]"
                                ),
                            });
                            continue 'rounds;
                        }
                        Err(e) => {
                            let _ = event_tx.send(ChatEvent::Status {
                                message: format!("Workflow '{cmd}' 失败: {e}"),
                            });
                            // 失败也要注入 manager 善后输入（与成功路径
                            // 同款 inject）。此前失败只发 Status 就回等
                            // 输入——advisor 的纠正 hint 悬在队列里无人
                            // 消费（实测实锤：gate 失败后 advisor
                            // 的「过度编排」intervene 悬空，50 分钟零产出）。
                            let hints: Vec<String> =
                                advisor_hints.lock().drain(..).collect();
                            let hints_text = if hints.is_empty() {
                                String::new()
                            } else {
                                format!("\n\nadvisor 监察建议：\n- {}", hints.join("\n- "))
                            };
                            let inject_dir = worktree_root.join(".latte").join("inject");
                            let _ = std::fs::create_dir_all(&inject_dir);
                            let inject_path = inject_dir.join("manager.txt");
                            let mgr_text = format!(
                                "Workflow '{cmd}' 失败。用户请求：{topic}\n\n错误：\n{e}{hints_text}\n\n请善后：能修复的修复后用错误消息里的 wf_id resume 续跑，或按 advisor 建议换路径重做；无法继续则向用户说明失败原因。"
                            );
                            let _ = std::fs::write(&inject_path, &mgr_text);
                            continue 'rounds;
                        }
                    }
                }
            }
        }

        // @role message injection
        if let Some(rest) = line.strip_prefix('@') {
            if let Some((role_id, message)) = rest.split_once(' ') {
                let role_id = role_id.trim();
                let message = message.trim();
                if order.iter().any(|r| r == role_id) {
                    let inject_dir = worktree_root.join(".latte").join("inject");
                    let _ = std::fs::create_dir_all(&inject_dir);
                    let inject_path = inject_dir.join(format!("{role_id}.txt"));
                    if let Ok(existing) = std::fs::read_to_string(&inject_path) {
                        let combined = format!("{existing}\n{message}");
                        let _ = std::fs::write(&inject_path, combined);
                    } else {
                        let _ = std::fs::write(&inject_path, message);
                    }
                    let _ = event_tx.send(ChatEvent::Status {
                        message: format!("[{role_id} queue: +1 message]"),
                    });
                } else {
                    let _ = event_tx.send(ChatEvent::Status {
                        message: format!("[error: unknown role '{role_id}']"),
                    });
                }
            } else {
                let _ = event_tx.send(ChatEvent::Status {
                    message: "[parse error: malformed @role line]".into(),
                });
            }
            continue 'rounds;
        }

        // Plain text = manager input
        {
            // plan 阶段门复位：新的用户消息 = 新的决策周期，上一轮
            // 未批准的 plan 不再约束 delegate（同时销账它的弹窗补发）。
            reset_plan_stage(&plan_stage);
            let mut mgr = session_arc.lock().await;
            mgr.append_to_role(
                "manager",
                Message::user(line.clone()),
            )
            .ok();
            let _ = event_tx.send(ChatEvent::Status {
                message: format!("[manager turn enqueued: {} chars]", line.len()),
            });
        }

        // ─── Execute round ─────────────────────────────────────
        let _ = event_tx.send(ChatEvent::RoundStarted { round: round_num });

        {
            let mgr = session_arc.lock().await;
            mgr.emit_round_started(round_num, &order);
        }

        for role_id in &order {
            if cancel_flag.load(Ordering::SeqCst) {
                break 'rounds;
            }

            {
                let mut mgr = session_arc.lock().await;
                if !matches!(
                    mgr.state(),
                    SessionState::Created | SessionState::Running | SessionState::Resumed
                ) {
                    let _ = event_tx.send(ChatEvent::Status {
                        message: format!(
                            "[session not running — current state: {:?}]",
                            mgr.state()
                        ),
                    });
                    continue 'rounds;
                }

                // Per-role pause (HIL v1.4): skip only this role's turn
                // while the rest of the round proceeds. Orthogonal to
                // the global state check above — `continue` (this role)
                // rather than `continue 'rounds` (whole round).
                if mgr.is_role_paused(role_id) {
                    let _ = event_tx.send(ChatEvent::Status {
                        message: format!(
                            "[{role_id} round {round_num}: skipped — role paused]"
                        ),
                    });
                    continue;
                }

                // Drain inject queue（统一实现见 crate::inject_queue —— 这段
                // 逻辑此前有四份拷贝且行为不一致）
                if let Some(content) =
                    crate::inject_queue::drain(mgr.worktree_root(), role_id)
                {
                    let synth = Message::user(crate::inject_queue::format_injected(&content));
                    mgr.append_to_role(role_id, synth).ok();
                }
                // Slice plan.md for this role
                let plan_slice = plan_md_slice_for(&mgr.record().plan_md, role_id);
                if !plan_slice.is_empty() {
                    let synth = Message::user(format!("[PLAN SLICE]\n{}", plan_slice));
                    mgr.append_to_role(role_id, synth).ok();
                }
                mgr.advance_turn().ok();
            }

            // Find the runner for this role
            let runner = match runners.iter_mut().find(|(id, _)| id == role_id) {
                Some((_, r)) => r,
                None => {
                    let _ = event_tx.send(ChatEvent::Status {
                        message: format!("[error: no runner for role '{role_id}']"),
                    });
                    continue;
                }
            };

            // Replay role history into runner context
            {
                let mgr = session_arc.lock().await;
                let history = mgr.role_history(role_id);
                let ctx = runner.context_mut();
                ctx.clear();
                for m in history {
                    ctx.push(m);
                }
            }

            let _ = event_tx.send(ChatEvent::Status {
                message: format!("[{role_id} round {round_num}: calling LLM...]"),
            });
            let _ = event_tx.send(ChatEvent::RoleStarted {
                role_id: role_id.clone(),
                detail: format!("round {round_num}: calling LLM"),
                sub_id: None,
            });

            // 本轮用量基线：`Supervisor::observe` 内部做 `+=`，所以这里
            // 必须传**本轮增量**，不能传 `runner.total_usage()`（那是该
            // runner 的累计值）。传累计值的后果是第 N 轮把前 N-1 轮重复
            // 加一次，总量按 O(N²) 膨胀 —— 名义 5 万预算会远早于 5 万就
            // 触发 "token budget exceeded"，角色越多、轮次越多越夸张。
            // 单角色路径一直是对的（usage_after - usage_before），这里
            // 与它对齐。
            let usage_before = runner.total_usage().clone();

            let new_assistant_text = match run_turn_cancellable(
                runner,
                &[],
                &cancel_flag,
                &turn_cancel_flag,
            ).await {
                Ok(text) => {
                    let _ = event_tx.send(ChatEvent::RoleFinished {
                        role_id: role_id.clone(),
                        detail: format!("round {round_num}: ok, {} chars", text.len()),
                        sub_id: None,
                    });
                    text
                }
                Err(e) => {
                    let _ = event_tx.send(ChatEvent::Status {
                        message: format!("[{role_id} round {round_num}: error: {e}]"),
                    });
                    let _ = event_tx.send(ChatEvent::RoleFinished {
                        role_id: role_id.clone(),
                        detail: format!("round {round_num}: error: {e}"),
                        sub_id: None,
                    });
                    // Advisor gate 重试耗尽：发 AdvisorTerminated，
                    // 本轮不产出 RoleTurn（String::new() 下方跳过）。
                    if let AgentError::AdvisorTerminated { reason, detector } = &e {
                        let _ = event_tx.send(ChatEvent::AdvisorTerminated {
                            role_id: role_id.clone(),
                            reason: reason.clone(),
                            detector: Some(detector.clone()),
                            sub_id: None,
                        });
                    }
                    String::new()
                }
            };

            if !new_assistant_text.is_empty() {
                let _ = event_tx.send(ChatEvent::Status {
                    message: format!("[{role_id} round {round_num}: ok, {} chars]", new_assistant_text.len()),
                });

                let _ = event_tx.send(ChatEvent::RoleTurn {
                    role_id: role_id.clone(),
                    content: strip_think_blocks(&new_assistant_text),
                    is_complete: true,
                    sub_id: None,
                });

                let mut mgr = session_arc.lock().await;
                let assistant_msg = Message::assistant(new_assistant_text);
                if let Err(e) = mgr.append_to_role(role_id, assistant_msg) {
                    let _ = event_tx.send(ChatEvent::Status {
                        message: format!("[{role_id} round {round_num}: failed to append: {e}]"),
                    });
                }
                drop(mgr);
            }

            // ─── Context threshold check ──────────────────────────────
            // 把**本轮增量**交给 supervisor（它内部 `+=` 累加）。见上面
            // `usage_before` 的注释：传累计值会让预算按 O(N²) 提前打爆。
            let usage_after = runner.total_usage();
            let tokens_used = (usage_after.input_tokens + usage_after.output_tokens
                + usage_after.thinking_tokens)
                .saturating_sub(
                    usage_before.input_tokens
                        + usage_before.output_tokens
                        + usage_before.thinking_tokens,
                );
            let decision_kind = runner.last_decision_kind();
            let pause_reason = scheduler
                .supervisor
                .observe(role_id, tokens_used, &decision_kind);
            if let Some(reason) = pause_reason {
                let mut mgr = session_arc.lock().await;
                let _ = mgr.pause_with_reason(&reason);
                mgr.emit_round_ended(round_num);
                let _ = event_tx.send(ChatEvent::Paused {
                    reason: reason.clone(),
                });
                let _ = event_tx.send(ChatEvent::Status {
                    message: format!("[supervisor pause: {reason}]"),
                });
                continue 'rounds;
            }
        }


        // Round ended

        {
            let mut mgr = session_arc.lock().await;
            mgr.emit_round_ended(round_num);
            mgr.advance_turn().ok();
        }
        let _ = event_tx.send(ChatEvent::RoundEnded { round: round_num });
    }

    // Cleanup
    {
        let mut mgr = session_arc.lock().await;
        if mgr.state() == SessionState::Paused {
            let _ = mgr.resume();
        }
        let _ = mgr.mark_done();
    }
}

// ─── Single-role mode ────────────────────────────────────────────

async fn run_single_role_loop(
    config: ControllerConfig,
    input_rx: &mut mpsc::UnboundedReceiver<ControllerInput>,
    event_tx: &broadcast::Sender<ChatEvent>,
    cancel_flag: Arc<AtomicBool>,
    turn_cancel_flag: Arc<AtomicBool>,
    advisor_hints: Arc<parking_lot::Mutex<std::collections::VecDeque<String>>>,
    plan_stage: SharedPlanStage,
    advisor_pause: AdvisorPauseGate,
    agent_pause_gate: std::sync::Arc<crate::pause_gate::AgentPauseGate>,
    last_user_input: Arc<parking_lot::Mutex<String>>,
) {
    let merged = &config.agent_config;
    let resolver = &config.model_resolver;
    let default_params = &config.default_params;
    let role_id = config.roles.first().cloned().unwrap_or_else(|| "manager".into());

    let tier = config
        .initial_tier
        .or_else(|| {
            merged
                .roles
                .get(&role_id)
                .and_then(|r| ModelTier::parse(&r.model_tier).ok())
        })
        .unwrap_or(ModelTier::Standard);

    let (runner, canonical_id) = match build_runner(
        merged,
        resolver,
        default_params,
        &role_id,
        tier,
        config.primary_model_id.as_deref(),
        event_tx,
        &config.cwd,
        config.subsession_store.clone(),
        &config.session_id,
        cancel_flag.clone(),
        turn_cancel_flag.clone(),
        last_user_input.clone(),
        config.advisor_monitor.runner_gate(),
        advisor_pause.clone(),
        &plan_stage,
        agent_pause_gate.clone(),
        config.stream_mode.clone(),
    )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let _ = event_tx.send(ChatEvent::Error {
                            kind: None,
                            message: format!("构建 runner 失败: {e}"),
                            sub_id: None,
                        });
            return;
        }
    };
    let mut runner = runner.with_advisor_hints(advisor_hints.clone());
    // Advisor v3 pause gate：只装到本 loop 的主 runner（watched
    // role）上；delegate specialist 由工具 handler 另建 runner，
    // 不经过这里，不会被 pause。advisor 未启用时不装（门也不会
    // 被置位，等价无门）。
    let attach_pause_gate = |r: AgentRunner| {
        if config.advisor_monitor.enabled {
            r.with_pause_gate(advisor_pause.clone())
        } else {
            r
        }
    };
    runner = attach_pause_gate(runner);

    // Seed the runner with any pre-existing history (e.g. when
    // resuming a paused session from the SessionStore). Done
    // before the first turn runs so the controller has full prior
    // context. Tokens are re-counted lazily; budget enforcement
    // happens on the next turn's start.
    for msg in &config.initial_history {
        runner.context_mut().push(msg.clone());
    }
    let mut current_role = canonical_id;
    let mut current_tier = tier;
    let current_primary = config.primary_model_id.clone();

    let icon = merged
        .roles
        .get(&current_role)
        .map(|r| r.icon.clone())
        .unwrap_or_else(|| role_icon(&current_role));
    let model_id = runner
        .agent()
        .model_chain
        .first()
        .map(|mc| mc.model.id.clone())
        .unwrap_or_else(|| "?".to_string());

    let _ = event_tx.send(ChatEvent::Prompt {
        icon: icon.clone(),
        role_id: current_role.clone(),
        model_id: model_id.clone(),
    });

    let _ = event_tx.send(ChatEvent::SessionInfo {
        task_id: config.task_id.clone().unwrap_or_else(|| "single-role".into()),
        state: "Running".into(),
        turn: 0,
        roles: vec![RoleInfo {
            id: current_role.clone(),
            name: current_role.clone(),
            icon: icon.clone(),
        }],
    });

    // Global pause state for single-role mode. Unlike the multi-role
    // loop (which gates on a shared `pause_flag` at round boundaries),
    // the single-role loop runs turns inline in the `Input` arm, so we
    // track pause locally and gate the next turn until Resume arrives.
    // Pause/Resume both flow through `input_rx`, so ordering is stable.
    let mut paused = false;

    loop {
        if cancel_flag.load(Ordering::SeqCst) {
            break;
        }

        tokio::select! {
            input = input_rx.recv() => {
                match input {
                    None => break,
                    Some(ControllerInput::Input(text)) => {
                        let trimmed = text.trim().to_string();
                        // 空输入直接丢弃，回到等输入 —— 这里原本是个
                        // **空语句块**（`if trimmed.is_empty() {}`），本意
                        // 显然是 continue（多角色路径写的就是
                        // `if !trimmed.is_empty() { break trimmed }`）。
                        // 落空的后果：前端误发空串/纯空白也会发
                        // UserMessage 并真跑一轮，而部分厂商对 text 为空
                        // 的消息直接 400，这一轮白报错。
                        if trimmed.is_empty() {
                            continue;
                        }

                        // workflow slash 失败时合成的善后输入：Some 时不
                        // continue，落到下面的正常 turn 路径直接开一轮
                        // （实测实锤：失败只发 Status 回等输入，
                        // advisor 的纠正 hint 悬空，50 分钟零产出）。
                        let mut synthetic_followup: Option<String> = None;
                        if trimmed.starts_with('/') {
                            let parts: Vec<&str> = trimmed.splitn(2, ' ').collect();
                            let cmd = parts[0];
                            match cmd {
                                "/exit" | "/quit" => break,
                                "/help" => {
                                    let _ = event_tx.send(ChatEvent::Status {
                                        message: "Commands: /role <id>  /model <tier>  /roles  /clear  /status  /tools  /history  /save <file>  /load <file>  /help  /exit".into(),
                                    });
                                }
                                "/roles" => {
                                    let ids: Vec<&String> = merged.roles.keys().collect();
                                    let roles_info: Vec<RoleInfo> = ids.iter().map(|id| {
                                        let tpl = merged.roles.get(*id).unwrap();
                                        RoleInfo { id: (*id).clone(), name: tpl.name.clone(), icon: tpl.icon.clone() }
                                    }).collect();
                                    let _ = event_tx.send(ChatEvent::RoleList { roles: roles_info });
                                }
                                "/role" => {
                                    let Some(new_role) = parts.get(1) else {
                                        let _ = event_tx.send(ChatEvent::Status { message: "usage: /role <id>".into() });
                                        continue;
                                    };
                                    let history: Vec<Message> = runner.context().messages().to_vec();
                                    match build_runner(merged, resolver, default_params, new_role, current_tier, current_primary.as_deref(), event_tx, &config.cwd, config.subsession_store.clone(), &config.session_id, cancel_flag.clone(), turn_cancel_flag.clone(), last_user_input.clone(), config.advisor_monitor.runner_gate(), advisor_pause.clone(), &plan_stage, agent_pause_gate.clone(), config.stream_mode.clone()).await {
                                        Ok((mut new_runner, rid)) => {
                                            for m in history { new_runner.context_mut().push(m); }
                                            runner = attach_pause_gate(new_runner.with_advisor_hints(advisor_hints.clone()));
                                            current_role = rid.clone();
                                            let mid = runner.agent().model_chain.first().map(|mc| mc.model.id.clone()).unwrap_or_else(|| "?".into());
                                            let ico = merged.roles.get(&current_role).map(|r| r.icon.clone()).unwrap_or_else(|| role_icon(&current_role));
                                            let _ = event_tx.send(ChatEvent::Status { message: format!("Switched to role '{rid}' (tier {})", current_tier.label()) });
                                            let _ = event_tx.send(ChatEvent::Prompt { icon: ico, role_id: current_role.clone(), model_id: mid });
                                        }
                                        Err(e) => { let _ = event_tx.send(error_event(&e, "switch role failed", None)); }
                                    }
                                }
                                "/model" => {
                                    let Some(t) = parts.get(1) else {
                                        let _ = event_tx.send(ChatEvent::Status { message: "usage: /model <premium|standard|budget>".into() });
                                        continue;
                                    };
                                    match ModelTier::parse(t) {
                                        Ok(new_tier) => {
                                            let role = current_role.clone();
                                            let history: Vec<Message> = runner.context().messages().to_vec();
                                            match build_runner(merged, resolver, default_params, &role, new_tier, current_primary.as_deref(), event_tx, &config.cwd, config.subsession_store.clone(), &config.session_id, cancel_flag.clone(), turn_cancel_flag.clone(), last_user_input.clone(), config.advisor_monitor.runner_gate(), advisor_pause.clone(), &plan_stage, agent_pause_gate.clone(), config.stream_mode.clone()).await {
                                                Ok((mut new_runner, _)) => {
                                                    for m in history { new_runner.context_mut().push(m); }
                                                    runner = attach_pause_gate(new_runner.with_advisor_hints(advisor_hints.clone()));
                                                    current_tier = new_tier;
                                                    let _ = event_tx.send(ChatEvent::Status { message: format!("Switched to tier {}", new_tier.label()) });
                                                }
                                                Err(e) => { let _ = event_tx.send(error_event(&e, "switch model failed", None)); }
                                            }
                                        }
                                        Err(e) => { let _ = event_tx.send(ChatEvent::Status { message: format!("invalid tier: {e}") }); }
                                    }
                                }
                                "/history" => {
                                    let msgs = runner.context().messages();
                                    let _ = event_tx.send(ChatEvent::Status { message: format!("history ({} messages):", msgs.len()) });
                                    for (i, m) in msgs.iter().enumerate() {
                                        let preview = if m.content.len() > 200 { format!("{}...", &m.as_text()[..200]) } else { m.as_text() };
                                        let _ = event_tx.send(ChatEvent::Status { message: format!("  {i} [{:?}] {preview}", m.role) });
                                    }
                                }
                                cmd => {
                                    let topic = parts.get(1).unwrap_or(&"").trim();
                                    // workflow 命令（如 /design-and-plan <topic>）按
                                    // submit_input 的设计不记录用户诉求（斜杠命令
                                    // 一律跳过）——确认是已注册 workflow 命令后把
                                    // topic 补记进 last_user_input，否则 advisor
                                    // 审查看到的「用户当前问题」是空串，会把正常的
                                    // workflow 启动误判成"无目标编排"。
                                    if !topic.is_empty()
                                        && crate::workflow::load_workflow_by_command(
                                            cmd,
                                            &config.cwd,
                                        )
                                        .is_ok()
                                    {
                                        *last_user_input.lock() = topic.to_string();
                                    }
                                    match run_workflow_command(cmd, topic, &config, &event_tx, cancel_flag.clone(), agent_pause_gate.clone(), turn_cancel_flag.clone(), advisor_pause.clone()).await {
                                        Ok(Some(summary)) => {
                                            let _ = event_tx.send(ChatEvent::Status {
                                                message: format!("Workflow '{cmd}' 完成。\n{summary}"),
                                            });
                                            // slash 路径不经 manager 的 agent loop，没人会调
                                            // `plan` 工具——产出里带任务清单时代 manager 广播
                                            // PlanProposed，让「添加任务」弹窗与 manager 路径
                                            // 一致弹出（并挂上 plan 阶段门）。
                                            if propose_plan_from_summary(
                                                &event_tx,
                                                &plan_stage,
                                                &current_role,
                                                &summary,
                                                Some((&config.cwd, &config.session_id)),
                                            ) {
                                                let _ = event_tx.send(ChatEvent::Status {
                                                    message: "检测到任务清单，已提交「添加任务」弹窗供勾选导入任务看板。".into(),
                                                });
                                            }
                                        }
                                        Ok(None) => {
                                            let _ = event_tx.send(ChatEvent::Status {
                                                message: format!(
                                                    "[unknown {cmd} — known: role model roles clear status tools history save load help exit quit, plus any workflow command]"
                                                ),
                                            });
                                        }
                                        Err(e) => {
                                            let _ = event_tx.send(ChatEvent::Status {
                                                message: format!("Workflow '{cmd}' 失败: {e}"),
                                            });
                                            // 自动善后：合成一条输入落到
                                            // 下面的正常 turn 路径，带上失败
                                            // 上下文 + advisor 积存的监察建议。
                                            let hints: Vec<String> =
                                                advisor_hints.lock().drain(..).collect();
                                            let hints_text = if hints.is_empty() {
                                                String::new()
                                            } else {
                                                format!("\n\nadvisor 监察建议：\n- {}", hints.join("\n- "))
                                            };
                                            synthetic_followup = Some(format!(
                                                "[自动善后] Workflow '{cmd}' 失败。用户请求：{topic}\n\n错误：\n{e}{hints_text}\n\n请善后：能修复的修复后用错误消息里的 wf_id 以 resume 续跑，或按 advisor 建议换路径重做；无法继续则向用户说明失败原因。"
                                            ));
                                        }
                                    }
                                }
                            }
                            if synthetic_followup.is_none() {
                                continue;
                            }
                        }
                        let trimmed = synthetic_followup.unwrap_or(trimmed);

                        // Pause gate: if the session is paused, hold
                        // this turn until the user resumes. Only
                        // Resume / Abort / CancelTurn are honored while
                        // parked; advisor hints are buffered. A
                        // CancelTurn while parked discards this pending
                        // input and returns to idle.
                        if paused {
                            let _ = event_tx.send(ChatEvent::Status {
                                message: "[会话已暂停 — 恢复后执行本条输入]".into(),
                            });
                            let mut cancelled = false;
                            loop {
                                if cancel_flag.load(Ordering::SeqCst) {
                                    return;
                                }
                                match input_rx.recv().await {
                                    Some(ControllerInput::Resume) => {
                                        paused = false;
                                        let _ = event_tx.send(ChatEvent::Resumed);
                                        break;
                                    }
                                    Some(ControllerInput::ResumeSession(ack)) => {
                                        let was_paused = paused;
                                        paused = false;
                                        let _ = ack.send(was_paused);
                                        break;
                                    }
                                    Some(ControllerInput::Abort) | None => return,
                                    Some(ControllerInput::CancelTurn) => {
                                        cancelled = true;
                                        break;
                                    }
                                    Some(ControllerInput::AdvisorHint(t)) => {
                                        advisor_hints.lock().push_back(t);
                                    }
                                    // Pause while already paused, or any
                                    // other input, is ignored here.
                                    _ => {}
                                }
                            }
                            if cancelled {
                                continue;
                            }
                        }

                        // Session 级暂停门（⏸ = `pause_session` /
                        // `POST /api/chat/pause-session`）：这是随附 UI
                        // 实际用的那一组端点，与上面那个 driver-local
                        // `paused`（legacy `/chat/pause`）是两套独立状态。
                        //
                        // 此前 driver 只看 `paused`，从不查 gate：gate
                        // engaged 时照样把 UserMessage / RoleStarted /
                        // [calling LLM] 全发出去，真正的 park 发生在
                        // runner 内部的 tool-round 边界。用户看到的是
                        // 「消息发出去了，然后永远没结果」，而不是
                        // 「已暂停」—— 按了 ⏸ 再发消息就会命中。
                        //
                        // 这里只轮询 gate 与 session cancel_flag，**不**从
                        // input_rx 取输入：取了就得负责回写，否则暂停期间
                        // 的消息/切角色会被静默吞掉（上面那个 park 循环的
                        // `_ => {}` 就是这个毛病，不要复制它）。
                        // `Paused`/`Resumed` 事件由 gate 的 on_change
                        // listener 统一广播，这里不重复发。
                        if agent_pause_gate.is_paused() {
                            let reason = agent_pause_gate
                                .pause_reason()
                                .unwrap_or_else(|| "会话已暂停".to_string());
                            let _ = event_tx.send(ChatEvent::Status {
                                message: format!(
                                    "[{reason} — 本条输入已收到，▶ 继续后执行]"
                                ),
                            });
                            let mut pending_turn_cancelled = false;
                            loop {
                                if cancel_flag.load(Ordering::SeqCst) {
                                    return;
                                }
                                // `cancel_turn()` sets this atomic before queuing
                                // ControllerInput::CancelTurn. The current Input arm
                                // already owns the pending text, so it cannot consume
                                // that command without also risking later inputs. Drop
                                // the pending turn here; the queued command is harmless
                                // because run_turn_cancellable clears stale idle flags.
                                if turn_cancel_flag.swap(false, Ordering::SeqCst) {
                                    pending_turn_cancelled = true;
                                    break;
                                }
                                if !agent_pause_gate.is_paused() {
                                    break;
                                }
                                tokio::time::sleep(std::time::Duration::from_millis(200))
                                    .await;
                            }
                            if pending_turn_cancelled {
                                let _ = event_tx.send(ChatEvent::Status {
                                    message: "[已取消暂停中等待执行的输入]".into(),
                                });
                                continue;
                            }
                        }

                        let _ = event_tx.send(ChatEvent::Status { message: format!("[calling LLM for role '{current_role}'...]") });
                        // plan 阶段门复位：新的用户消息 = 新的决策周期，
                        // 上一轮未批准的 plan 不再约束 delegate
                        //（同时销账它的弹窗补发）。
                        reset_plan_stage(&plan_stage);
                        let _ = event_tx.send(ChatEvent::UserMessage { text: trimmed.clone() });
                        let _ = event_tx.send(ChatEvent::RoleStarted {
                            role_id: current_role.clone(),
                            detail: "calling LLM".into(),
                            sub_id: None,
                        });
let usage_before = runner.total_usage().clone();
                        // Run the turn with cancellation support (no
                        // auto-timeout — user hits the stop button).
                        let user_msg = [Message::user(trimmed.clone())];
                        let turn_result = run_turn_cancellable(
                            &mut runner,
                            &user_msg,
                            &cancel_flag,
                            &turn_cancel_flag,
                        )
                        .await;
                        match turn_result {
                            Ok(response) => {
                                let mid = runner.agent().model_chain.first().map(|mc| mc.model.id.clone()).unwrap_or_else(|| "?".into());
                                let ico = merged.roles.get(&current_role).map(|r| r.icon.clone()).unwrap_or_else(|| role_icon(&current_role));
                                let usage_after = runner.total_usage();
                                let in_delta = usage_after.input_tokens - usage_before.input_tokens;
                                let out_delta = usage_after.output_tokens - usage_before.output_tokens;
                                let _ = event_tx.send(ChatEvent::RoleTurn { role_id: current_role.clone(), content: strip_think_blocks(&response), is_complete: true, sub_id: None });
                                let _ = event_tx.send(ChatEvent::Status { message: format!("[{current_role} · {mid} · tokens: +{in_delta} in / +{out_delta} out]") });
                                let _ = event_tx.send(ChatEvent::RoleFinished {
                                    role_id: current_role.clone(),
                                    detail: format!("ok, {} chars", response.len()),
                                    sub_id: None,
                                });
                                let _ = event_tx.send(ChatEvent::Prompt { icon: ico, role_id: current_role.clone(), model_id: mid });
                            }
                            Err(e) => {
                                let _ = event_tx.send(ChatEvent::RoleFinished {
                                    role_id: current_role.clone(),
                                    detail: format!("error: {e}"),
                                    sub_id: None,
                                });
                                // Advisor gate 重试耗尽：发
                                // AdvisorTerminated 而不是普通 Error —
                                // 坏答案不落盘成 RoleTurn，driver 回到
                                // 等用户输入（不是 session 终止）。
                                if let AgentError::AdvisorTerminated { reason, detector } = &e {
                                    let _ = event_tx.send(ChatEvent::AdvisorTerminated {
                                        role_id: current_role.clone(),
                                        reason: reason.clone(),
                                        detector: Some(detector.clone()),
                                        sub_id: None,
                                    });
                                } else {
                                    let _ = event_tx.send(error_event(&e, "turn failed", None));
                                }
                            }
                        }
                    }
                    Some(ControllerInput::SwitchRole(new_role)) => {
                        // 重复"切换"到当前角色（UI 重连/刷新会重复发
                        // SwitchRole，日志里曾一次连发 6 条相同的
                        // Switched Status）：不重建 runner、不发 Status
                        // 噪声；仍发 Prompt 让 UI 同步当前角色显示。
                        if new_role == current_role {
                            let mid = runner.agent().model_chain.first().map(|mc| mc.model.id.clone()).unwrap_or_else(|| "?".into());
                            let ico = merged.roles.get(&current_role).map(|r| r.icon.clone()).unwrap_or_else(|| role_icon(&current_role));
                            let _ = event_tx.send(ChatEvent::Prompt { icon: ico, role_id: current_role.clone(), model_id: mid });
                            continue;
                        }
                        let history: Vec<Message> = runner.context().messages().to_vec();
                        match build_runner(merged, resolver, default_params, &new_role, current_tier, current_primary.as_deref(), event_tx, &config.cwd, config.subsession_store.clone(), &config.session_id, cancel_flag.clone(), turn_cancel_flag.clone(), last_user_input.clone(), config.advisor_monitor.runner_gate(), advisor_pause.clone(), &plan_stage, agent_pause_gate.clone(), config.stream_mode.clone()).await {
                            Ok((mut new_runner, rid)) => {
                                for m in history { new_runner.context_mut().push(m); }
                                runner = attach_pause_gate(new_runner.with_advisor_hints(advisor_hints.clone()));
                                current_role = rid.clone();
                                let mid = runner.agent().model_chain.first().map(|mc| mc.model.id.clone()).unwrap_or_else(|| "?".into());
                                let ico = merged.roles.get(&current_role).map(|r| r.icon.clone()).unwrap_or_else(|| role_icon(&current_role));
                                let _ = event_tx.send(ChatEvent::Status { message: format!("Switched to role '{rid}' (tier {})", current_tier.label()) });
                                let _ = event_tx.send(ChatEvent::Prompt { icon: ico, role_id: current_role.clone(), model_id: mid });
                            }
                            Err(e) => { let _ = event_tx.send(error_event(&e, "switch role failed", None)); }
                        }
                    }
                    Some(ControllerInput::SwitchModel(new_tier)) => {
                        let history: Vec<Message> = runner.context().messages().to_vec();
                        match build_runner(merged, resolver, default_params, &current_role, new_tier, current_primary.as_deref(), event_tx, &config.cwd, config.subsession_store.clone(), &config.session_id, cancel_flag.clone(), turn_cancel_flag.clone(), last_user_input.clone(), config.advisor_monitor.runner_gate(), advisor_pause.clone(), &plan_stage, agent_pause_gate.clone(), config.stream_mode.clone()).await {
                            Ok((mut new_runner, _)) => {
                                for m in history { new_runner.context_mut().push(m); }
                                runner = attach_pause_gate(new_runner.with_advisor_hints(advisor_hints.clone()));
                                current_tier = new_tier;
                                let _ = event_tx.send(ChatEvent::Status { message: format!("Switched to tier {}", new_tier.label()) });
                            }
                            Err(e) => { let _ = event_tx.send(error_event(&e, "switch model failed", None)); }
                        }
                    }
                    Some(ControllerInput::Pause) => {
                        paused = true;
                        let _ = event_tx.send(ChatEvent::Paused { reason: "用户暂停".into() });
                    }
                    Some(ControllerInput::Resume) => {
                        paused = false;
                        let _ = event_tx.send(ChatEvent::Resumed);
                    }
                    // V2 resume-session already emits via the gate listener/API.
                    // Single-role has no SessionManager state to restore.
                    Some(ControllerInput::ResumeSession(ack)) => {
                        let _ = ack.send(false);
                    }
                    Some(ControllerInput::AdvisorHint(text)) => {
                        // Idle-side delivery: park the hint in the
                        // shared queue; the runner drains it at the
                        // next turn start. (Mid-turn delivery happens
                        // via the direct `advisor_hint()` push, not
                        // this channel.)
                        advisor_hints.lock().push_back(text);
                    }
                    // 取消当前 turn：仅打断正在跑的 run_turn，不退出
                    // 整个 session。run_single_role_loop 的 select! 循环
                    // 会检测 turn_cancel_flag 并在下一个 500ms tick
                    // 丢弃 run_turn future。
                    Some(ControllerInput::CancelTurn) => {
                        turn_cancel_flag.store(true, Ordering::SeqCst);
                    }
                    Some(ControllerInput::Abort) => break,
                }
            }
        }
    }
}

async fn build_runner(
    merged: &AgentConfig,
    resolver: &ModelResolver,
    default_params: &GenerateParams,
    role_id: &str,
    tier: ModelTier,
    primary_id: Option<&str>,
    event_tx: &broadcast::Sender<ChatEvent>,
    cwd: &Path,
    subsession_store: Arc<SubsessionStore>,
    // UI session id —— 见 [`ControllerConfig::session_id`]。透传给
    // `register_delegate_tool`，最终到 `subsession_store.create()`。
    // 空串 → 旧行为（用 cwd 当 key，落盘会被拒绝）。
    session_id: &str,
    cancel_flag: Arc<AtomicBool>,
    turn_cancel_flag: Arc<AtomicBool>,
    // 用户在主会话里的原始诉求（与 ChatController 共享句柄）。透传给
    // `register_delegate_tool`，最终作为委派返回审查的「是否符合预期」准绳。
    main_topic: Arc<parking_lot::Mutex<String>>,
    // Advisor pre-persistence gate（D5/D6）。`Some` 时装到本 runner
    // 及 delegate specialist runner 上（`with_gate_config`），
    // `run_turn_gated` 在产出被接受前先过 `check_response_gates`；
    // `None` 时 gate 缺省，`run_turn_gated` 等价 `run_turn`。
    advisor_gate: Option<GateConfig>,
    // Advisor intervene 暂停门（v3）：透传给 workflow 工具，让
    // workflow 的分派也受「等待用户拍板」约束。
    advisor_pause: AdvisorPauseGate,
    // plan 阶段门共享句柄：透传给 `register_plan_tool`（置
    // PendingApproval）与 `register_delegate_tool`（拦截实现类角色）。
    plan_stage: &SharedPlanStage,
    agent_pause_gate: std::sync::Arc<crate::pause_gate::AgentPauseGate>,
    stream_mode: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> AgentResult<(AgentRunner, String)> {
    let template = merged
        .roles
        .get(role_id)
        .ok_or_else(|| AgentError::RoleNotFound(role_id.to_string()))?
        .clone();
    let role = template.resolve(default_params).await?;
    let models = if let Some(id) = primary_id {
        let mut chain = Vec::new();
        chain.push(resolver.resolve_id_or_name(id)?);
        for cid in &role.model_chain {
            if chain.iter().any(|m| m.id == *cid) {
                continue;
            }
            if let Ok(m) = resolver.build_model(cid) {
                chain.push(m);
            }
        }
        chain
    } else {
        // 角色模型指派优先取 resolver 最新快照：角色编辑器保存后，
        // 旧 session 重建 runner（SwitchRole 等）也能用上新指派——
        // merged 是 session 创建时的固化快照，可能已过时。
        let (chain_ids, tier) = match resolver.role_model_assignment(&role.id) {
            Some((chain, t)) => (
                if chain.is_empty() {
                    role.model_chain.clone()
                } else {
                    chain
                },
                t.unwrap_or(tier),
            ),
            None => (role.model_chain.clone(), tier),
        };
        resolver.resolve_chain(&role.id, tier, &chain_ids)?
    };
    if models.is_empty() {
        return Err(AgentError::ModelsUnavailable {
            tried: vec![role_id.to_string()],
            failures: vec![],
            next_retry_in: None,
        });
    }
    let mut role = role;

    // Append the tool-call protocol + (for manager) the delegate hints.
    let mut prompt = String::new();
    if !role.allowed_tools.is_empty() {
        prompt.push_str(&tool_usage_prompt(&role.allowed_tools));
        // Any role with "delegate" in its tools gets the delegation capability
        if role.allowed_tools.iter().any(|t| t == "delegate") {
            prompt.push_str(&delegate_tool_hint(merged));
        }
        // Any role with "workflow" in its tools gets the workflow hint.
        if role.allowed_tools.iter().any(|t| t == "workflow") {
            prompt.push_str(&workflow_tool_hint(cwd));
        }
    }
    // Every role — tools or no tools — gets the system ground truth
    // (cwd, host, time). When the user asks about runtime state
    // ("current directory?", "pwd?", …) the model sees real values
    // instead of guessing from training data. This is structural
    // ground-truth injection: we *give* the model the fact, we don't
    // *forbid* hallucinations.
    prompt.push_str(&crate::ground_truth::ground_truth_block(cwd));
    role.system_prompt.push_str(&prompt);

    let agent = Agent::new_with_chain(role_id.to_string(), role.clone(), models, default_params.clone())?;

    if !role.allowed_tools.is_empty() {
        let tm = build_tool_manager(&role.allowed_tools).await
            .map_err(|e| AgentError::Tool(format!("build tool manager '{role_id}': {e}")))?;
        // request_tool：所有带工具的角色都可申请临时使用未授权工具
        // （角色 prompt 与 allowed_tools 不对齐时的软拒绝 + 申请通道）。
        register_request_tool(tm.clone())
            .map_err(|e| AgentError::Tool(format!("register request_tool '{role_id}': {e}")))?;
        // Any role with "delegate" in allowed_tools gets the delegate tool registered,
        // enabling nested subsessions (roleA delegates to roleB, roleB can also delegate)
        if role.allowed_tools.iter().any(|t| t == "delegate") {
            // 必须是真 session_id —— subsession_store 用它当落盘目录名。
            // 之前这里用 `cwd.to_string_lossy()` 凑数，cwd 含 `/` 会被
            // 路径安全检查拒绝，整路 subagent 退化到内存。
            let sid = session_id.to_string();
            register_delegate_tool(
                &tm,
                merged,
                resolver,
                default_params.clone(),
                event_tx.clone(),
                cwd.to_path_buf(),
                subsession_store.clone(),
                sid,
                cancel_flag.clone(),
                turn_cancel_flag.clone(),
                advisor_gate.clone(),
                plan_stage.clone(),
                agent_pause_gate.clone(),
                // 单 session delegate 累计计数器（fresh controller 时为 0）。
                // counter 与 controller 同生命周期，session 结束 / 重启
                // 自动归零。`default_max_delegates` 从 env 读，默认 12。
                std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
                default_max_delegates(),
                main_topic.clone(),
            )
            .await
            .map_err(|e| AgentError::Tool(format!("register delegate: {e}")))?;
        }
        // Any role with "workflow" in allowed_tools gets the workflow
        // tool: trigger a named multi-role workflow by name + topic.
        if role.allowed_tools.iter().any(|t| t == "workflow") {
            register_workflow_tool(
                &tm,
                merged,
                resolver,
                default_params.clone(),
                event_tx.clone(),
                cwd.to_path_buf(),
                cancel_flag.clone(),
                turn_cancel_flag.clone(),
                agent_pause_gate.clone(),
                // 与 delegate 同源：workflow 的每次角色分派也建
                // subsession、过 advisor gate + 返回审查。
                subsession_store.clone(),
                session_id.to_string(),
                advisor_gate.clone(),
                advisor_pause.clone(),
                // 原始诉求：调研流程返回时用来判断用户点名的交付物
                // 是否还欠着（见 workflow::deliverable_reminder）。
                main_topic.clone(),
            )
            .await
            .map_err(|e| AgentError::Tool(format!("register workflow: {e}")))?;
        }
        // Any role with "generate_image" in allowed_tools gets the image
        // generation tool (OpenAI-compatible /v1/images/generations).
        if role.allowed_tools.iter().any(|t| t == "generate_image") {
            crate::image_gen::register_generate_image_tool(
                &tm,
                merged,
                event_tx.clone(),
                cwd.to_path_buf(),
                role_id.to_string(),
            )
            .map_err(|e| AgentError::Tool(format!("register generate_image: {e}")))?;
        }
        // Any role with "plan" in allowed_tools gets the plan tool:
        // 把结构化任务清单提交给用户在弹窗里勾选导入任务看板。
        // manager 在 implementation_plan workflow 跑完后调它。
        if role.allowed_tools.iter().any(|t| t == "plan") {
            register_plan_tool(
                &tm,
                event_tx.clone(),
                role_id.to_string(),
                plan_stage.clone(),
                cwd,
                session_id.to_string(),
            )
            .map_err(|e| AgentError::Tool(format!("register plan: {e}")))?;
        }
        // Any role with "ask" in allowed_tools gets the ask tool:
        // 向用户抛出一道选择题（含图片 / 图片网格 / 上传），弹出选择框。
        // 顶层 turn 用 fire-and-forget（回答作为下一条 user 消息回喂）。
        //
        // 落盘归属 `(cwd, session_id)`：fire-and-forget 弹框此前只活在
        // 内存 `PROMPTS` 表里，进程一重启就永久消失——用户既看不到也
        // 无从补救。带上它，重启后 `pending-prompts` 能把待办弹框补回来。
        if role.allowed_tools.iter().any(|t| t == "ask") {
            register_ask_tool(
                &tm,
                event_tx.clone(),
                role_id.to_string(),
                None,
                Some((cwd.to_path_buf(), session_id.to_string())),
            )
            .map_err(|e| AgentError::Tool(format!("register ask: {e}")))?;
        }
        // Any role with "task_report" in allowed_tools gets the task_report
        // tool: 任务看板闭环——manager 完成任务后调它广播 ChatEvent::TaskReport，
        // ui-server 侧 events_sse 订阅器把它转成 POST /api/tasks/:id/report。
        if role.allowed_tools.iter().any(|t| t == "task_report") {
            register_task_report_tool(&tm, event_tx.clone(), role_id.to_string())
                .map_err(|e| AgentError::Tool(format!("register task_report: {e}")))?;
        }
        // Any role with doc-graph tools in allowed_tools gets the 4 doc-graph
        // tools（scan/context/write/index）。cwd 是 agent 工作目录。
        {
            let has = ["doc_graph_scan", "doc_graph_context", "doc_write", "doc_index"]
                .iter()
                .any(|t| role.allowed_tools.iter().any(|a| a == t));
            if has {
                crate::doc_graph_tools::register_doc_graph_tools(&tm, cwd.to_path_buf())
                    .map_err(|e| AgentError::Tool(format!("register doc_graph tools: {e}")))?;
            }
        }
        // ── 给主 runner 分配 subsession sink（manager / 任何角色通用） ──
        let subsession_sink: Option<Arc<dyn crate::trace::TraceSink>> = if session_id.is_empty() {
            None
        } else {
            let (_sub_id, sink) = subsession_store.create(session_id, role_id);
            Some(sink)
        };
        // 同时挂上 ChatEventTraceSink：把 ParseToolCalls/ToolExec 转成
        // ToolUse/ToolResult/ToolError 广播到 session channel——advisor
        // monitor 靠这些事件观察工具行为（路由审查、D2-D4 异常检测），
        // 没有它 monitor 的 Tool* 分支永远收不到事件。
        let chat_sink: Arc<dyn crate::trace::TraceSink> = Arc::new(ChatEventTraceSink {
            event_tx: event_tx.clone(),
            sub_id: None,
        });
        let runner_sink: Arc<dyn crate::trace::TraceSink> = match subsession_sink {
            Some(sub) => Arc::new(crate::trace::FanOutSink::new(vec![sub, chat_sink])),
            None => chat_sink,
        };
        let mut runner = AgentRunner::new_with_tools(agent, tm)
            .with_role(role_id)
            .with_cwd(cwd.to_path_buf());
        runner = runner.with_sink(runner_sink);
        if let Some(gate) = advisor_gate.clone() {
            runner = runner.with_gate_config(gate);
        }
        runner = runner.with_agent_pause_gate(agent_pause_gate);
        runner = runner.with_stream_mode(stream_mode);
        // 模型配置热更新：UI 保存 models 配置后，本 runner 在下一个
        // turn 边界自动重建 model chain，无需重启 session。
        runner = runner.with_model_hot_reload(
            std::sync::Arc::new(resolver.clone()),
            tier,
            role.model_chain.clone(),
        );
        Ok((runner, role_id.to_string()))
    } else {
        // ── 给主 runner 分配 subsession sink（同上） ──
        let subsession_sink: Option<Arc<dyn crate::trace::TraceSink>> = if session_id.is_empty() {
            None
        } else {
            let (_sub_id, sink) = subsession_store.create(session_id, role_id);
            Some(sink)
        };
        let chat_sink: Arc<dyn crate::trace::TraceSink> = Arc::new(ChatEventTraceSink {
            event_tx: event_tx.clone(),
            sub_id: None,
        });
        let runner_sink: Arc<dyn crate::trace::TraceSink> = match subsession_sink {
            Some(sub) => Arc::new(crate::trace::FanOutSink::new(vec![sub, chat_sink])),
            None => chat_sink,
        };
        let mut runner = AgentRunner::new(agent)
            .with_role(role_id)
            .with_cwd(cwd.to_path_buf());
        // 这条 `.with_sink` 此前漏了：`runner_sink` 构造完就被丢弃
        // （编译器只报了个 unused variable 警告，实际后果是无工具角色的
        // trace 事件既不落子会话日志、也不经 ChatEventTraceSink 广播给
        // UI —— 有工具的那条分支一直是对的，两边不一致）。
        runner = runner.with_sink(runner_sink);
        if let Some(gate) = advisor_gate {
            runner = runner.with_gate_config(gate);
        }
        runner = runner.with_agent_pause_gate(agent_pause_gate);
        runner = runner.with_stream_mode(stream_mode);
        runner = runner.with_model_hot_reload(
            std::sync::Arc::new(resolver.clone()),
            tier,
            role.model_chain.clone(),
        );
        Ok((runner, role_id.to_string()))
    }
}

const BATCH_READ_MAX_PATHS: usize = 10;
const BATCH_READ_OUTPUT_BUDGET: usize = 192 * 1024;

fn tool_prompt_content(name: &str) -> Option<String> {
    use crate::prompts::tool_prompts;

    let local_file = Path::new("prompts/tools").join(format!("{name}.md"));
    if let Ok(text) = std::fs::read_to_string(local_file) {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    tool_prompts::for_tool(name).map(|text| text.trim().to_string())
}

/// Add the model-facing batch contract and wrap the single-file read handler.
/// The sibling tools crate remains the canonical single-file implementation;
/// this adapter only coordinates independent calls and preserves each native
/// result unchanged.
fn add_batch_read_contract(
    mut tool: latte_rs_agent_tools::types::Tool,
) -> latte_rs_agent_tools::types::Tool {
    use futures::{stream, StreamExt};
    use latte_rs_agent_tools::error::ToolError;
    use latte_rs_agent_tools::types::{PropertyType, ToolInputProperty};

    if tool.name != "read" || tool.input_schema.properties.contains_key("paths") {
        return tool;
    }

    if let Some(path) = tool.input_schema.properties.get_mut("path") {
        path.description =
            Some("单个读取目标，支持行范围选择器；有多个相互独立的目标时改用 paths。".into());
    }
    tool.input_schema.properties.insert(
        "paths".into(),
        ToolInputProperty {
            property_type: PropertyType::Array,
            description: Some(
                "1–10 个相互独立读取目标的批量入口；内部并发执行并按输入顺序归并结果，优先用于 2 个以上目标。".into(),
            ),
            enum_values: None,
            minimum: None,
            maximum: None,
            min_length: Some(1),
            max_length: Some(BATCH_READ_MAX_PATHS),
        },
    );
    // ToolInputSchema cannot express oneOf(path, paths), so the handler owns
    // that validation. strict=true would incorrectly require both fields.
    tool.input_schema.required = None;
    tool.strict = None;

    let base_handler = tool.handler.clone();
    tool.handler = Arc::new(move |input, ctx| {
        let base_handler = base_handler.clone();
        Box::pin(async move {
            let path = input.get("path");
            let paths = input.get("paths");
            match (path, paths) {
                (Some(_), Some(_)) => {
                    return Err(ToolError::other(
                        "exactly one of 'path' or 'paths' is required",
                    ));
                }
                (None, None) => {
                    return Err(ToolError::other(
                        "exactly one of 'path' or 'paths' is required",
                    ));
                }
                (Some(value), None) => {
                    if !value.is_string() {
                        return Err(ToolError::other("'path' must be a string"));
                    }
                    return (base_handler)(input, ctx).await;
                }
                (None, Some(_)) => {}
            }

            let values = paths
                .and_then(|value| value.as_array())
                .ok_or_else(|| ToolError::other("'paths' must be an array of strings"))?;
            if values.is_empty() || values.len() > BATCH_READ_MAX_PATHS {
                return Err(ToolError::other(format!(
                    "'paths' must contain 1–{BATCH_READ_MAX_PATHS} entries"
                )));
            }
            let paths = values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .filter(|path| !path.is_empty())
                        .map(str::to_string)
                        .ok_or_else(|| {
                            ToolError::other("every 'paths' entry must be a non-empty string")
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;

            let max_size = input.get("maxSize").cloned();
            let parallelism = if crate::agent::readonly_parallel_enabled() {
                crate::agent::readonly_parallel_max()
            } else {
                1
            };
            let jobs = paths.into_iter().map(|path| {
                let handler = base_handler.clone();
                let ctx = ctx.clone();
                let max_size = max_size.clone();
                async move {
                    let mut single = serde_json::Map::new();
                    single.insert("path".into(), serde_json::Value::String(path.clone()));
                    if let Some(max_size) = max_size {
                        single.insert("maxSize".into(), max_size);
                    }
                    let result = (handler)(serde_json::Value::Object(single), ctx).await;
                    (path, result)
                }
            });
            // `buffered`, unlike `buffer_unordered`, preserves input order.
            let results = stream::iter(jobs)
                .buffered(parallelism)
                .collect::<Vec<_>>()
                .await;

            let count = results.len();
            let mut files = Vec::new();
            let mut failed = Vec::new();
            let mut output_bytes = 0usize;
            for (path, result) in results {
                match result {
                    Ok(value) => {
                        let size = serde_json::to_vec(&value)
                            .map(|bytes| bytes.len())
                            .unwrap_or(0);
                        if !files.is_empty()
                            && output_bytes.saturating_add(size) > BATCH_READ_OUTPUT_BUDGET
                        {
                            failed.push(serde_json::json!({
                                "path": path,
                                "error": format!("batch output budget exceeded ({BATCH_READ_OUTPUT_BUDGET} bytes)")
                            }));
                        } else {
                            output_bytes = output_bytes.saturating_add(size);
                            files.push(value);
                        }
                    }
                    Err(error) => failed.push(serde_json::json!({
                        "path": path,
                        "error": error.to_string()
                    })),
                }
            }

            Ok(serde_json::json!({"files": files, "failed": failed, "count": count}))
        })
    });
    tool
}

fn enrich_tool_for_model(
    mut tool: latte_rs_agent_tools::types::Tool,
) -> latte_rs_agent_tools::types::Tool {
    tool = add_batch_read_contract(tool);
    if let Some(prompt) = tool_prompt_content(&tool.name) {
        if !tool.description.contains(&prompt) {
            tool.description.push_str("\n\n");
            tool.description.push_str(&prompt);
        }
    }
    tool
}

pub(crate) async fn build_tool_manager(
    allowed: &[String],
) -> Result<
    Arc<dyn latte_rs_agent_tools::types::ToolManager>,
    Box<dyn std::error::Error + Send + Sync>,
> {
    use latte_rs_agent_tools::prelude::*;
    let mgr = create_tool_manager();
    for package in builtin_tool_packages() {
        mgr.register_package(package)
            .await
            .map_err(|error| format!("register_package: {error}"))?;
    }
    // Register before enrichment/filtering so request_tool can grant it too.
    mgr.register(code_graph_tool(), None);

    // Enrich the complete registry first. Both initial allowlists and later
    // request_tool grants must clone the exact same model-facing definition.
        for name in mgr.get_tool_names() {
            if let Some(tool) = mgr.get_tool(&name) {
            mgr.unregister(&name);
            mgr.register(enrich_tool_for_model(tool), None);
            }
        }
    FULL_TOOL_POOL.get_or_init(|| {
        mgr.get_tool_names()
            .into_iter()
            .filter_map(|name| mgr.get_tool(&name).map(|tool| (name, tool)))
            .collect()
    });

    // allowed 里是配置层扁平名（bash/read/edit/...）。tools crate 扁平化
    // 后注册名 == 配置名 == 模型 schema 名，配置名直接进 keep 即可匹配。
    let mut keep: std::collections::HashSet<String> = allowed
        .iter()
        .flat_map(|name| vec![name.to_lowercase(), name.clone()])
        .collect();
    if keep.contains("mcp") {
        keep.extend(["mcp_connect".into(), "mcp_list".into(), "mcp_call".into()]);
    }
    if keep.contains("playwright") {
        keep.extend(["playwright_script".into(), "screenshot".into()]);
    }
    if keep.contains("code-graph") {
        keep.insert("code_graph".into());
    }

    for tool_id in mgr.get_tool_names() {
        let short = tool_id
            .rsplit_once('.')
            .map(|(_, name)| name)
            .unwrap_or(&tool_id);
        if !(keep.contains(short) || keep.contains(&tool_id)) {
            mgr.unregister(&tool_id);
        }
    }

    Ok(mgr)
}

// ─── Full tool pool: cached registry of all builtin tools ──────────────
// Built once on first build_tool_manager call (after all packages
// registered, before keep filtering). Used by request_tool to look up
// tool definitions that were filtered out by the allowlist.
// OnceLock is used because initialization requires an async context
// (register_package flattens namespaces) — see rule
// "Keep OnceLock when runtime input is required".
use std::sync::OnceLock;
type FullToolPool = std::collections::HashMap<String, latte_rs_agent_tools::types::Tool>;
static FULL_TOOL_POOL: OnceLock<FullToolPool> = OnceLock::new();

pub(crate) fn full_tool_pool() -> &'static FullToolPool {
    FULL_TOOL_POOL.get().unwrap_or_else(|| {
        // Should never happen: build_tool_manager always runs before
        // register_request_tool. If it does, initialise with a synchronous
        // best-effort pool (no namespace flattening, tools maintain their
        // original name from the package definition).
        use latte_rs_agent_tools::tools::builtin_tool_packages;
        let mut pool = FullToolPool::new();
        for package in builtin_tool_packages() {
            for tool in package.tools {
                let tool = enrich_tool_for_model(tool);
                pool.insert(tool.name.clone(), tool);
            }
        }
        let code_graph = enrich_tool_for_model(code_graph_tool());
        pool.insert(code_graph.name.clone(), code_graph);
        FULL_TOOL_POOL.get_or_init(|| pool)
    })
}

/// Register `request_tool` on the given tool manager. `request_tool` lets
/// a role ask for temporary access to a tool that is not in its allowed
/// list. The tool is looked up from the full builtin tool pool and
/// registered on the fly — no approval needed, but the grant is
/// per-role-manager (i.e. per-runner, lasts for the session).
pub(crate) fn register_request_tool(
    tm: Arc<dyn latte_rs_agent_tools::types::ToolManager>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use latte_rs_agent_tools::types::*;
    use std::sync::Arc;

    let pool = full_tool_pool().clone();

    let input_schema = ToolInputSchema {
        schema_type: SchemaType,
        properties: vec![
            ("tool_name".into(), ToolInputProperty {
                property_type: PropertyType::String,
                description: Some("要申请使用的工具名称。".into()),
                enum_values: None, minimum: None, maximum: None, min_length: None, max_length: None,
            }),
            ("reason".into(), ToolInputProperty {
                property_type: PropertyType::String,
                description: Some("申请使用该工具的原因（角色 prompt 的上下文或任务需求描述）。".into()),
                enum_values: None, minimum: None, maximum: None, min_length: None, max_length: None,
            }),
        ].into_iter().collect(),
        required: Some(vec!["tool_name".into(), "reason".into()]),
        ..Default::default()
    };
    let tm_for_register = tm.clone();
    let handler: SharedToolHandler = Arc::new(move |input: serde_json::Value, _ctx| {
        let tm = tm.clone();  // Arc is cheap; keep a copy for the async block
        let pool = pool.clone();
        Box::pin(async move {
            let tool_err = |msg: String| latte_rs_agent_tools::error::ToolError::Other(msg);

            let tool_name = input
                .get("tool_name")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| tool_err("'tool_name' 不能为空".into()))?;

            let reason = input
                .get("reason")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| tool_err("'reason' 不能为空".into()))?;

            // Look up in the full tool pool.
            let tool = pool.get(&tool_name).ok_or_else(|| {
                tool_err(format!("未知工具 '{tool_name}'，不在可用工具池中"))
            })?;

            // Check if already registered (idempotent).
            if tm.has(&tool_name) {
                return Ok(serde_json::Value::String(format!(
                    "工具 '{tool_name}' 已授权，可以直接使用。"
                )));
            }

            // Temporarily grant access: register the tool into the current
            // manager. The tool will appear in the next model request's
            // tool schemas automatically.
            tm.register(tool.clone(), Some("requested"));

            Ok(serde_json::Value::String(format!(
                "工具 '{tool_name}' 已临时授权，可以在本轮对话中直接调用。原因已记录：{reason}"
            )))
        })
    });

    let tool = latte_rs_agent_tools::types::Tool::builder(
        "request_tool".to_string(),
        "向当前角色申请临时使用某个工具。当你的角色 prompt 或任务需要某个工具，但该工具不在你的可用工具列表中时，先用此工具申请。参数：tool_name(工具名) + reason(申请原因)。申请后自动批准，之后可直接调用该工具。".to_string(),
        input_schema,
        handler,
    )
    .build();

    tm_for_register.register(tool, Some("request_tool"));
    Ok(())
}


// ─────────────────────────────────────────────────────────────────────
// code_graph：结构化代码导航
//
// 设计要点（2026-08-27 重写，起因见实测会话事故）：
//
// 旧实现把 `kind` 映射成 **Rust/TypeScript 语法的 ast-grep pattern**
// （`fn $NAME($$$PARAMS) -> $RET { ... }`、`trait`、`impl`、ES6
// `import`），且不传 `--lang`。在 C 项目上这些 pattern 命中数恒为 0，
// 于是 programmer 角色虽然配置里有 code_graph，实际 151 次工具调用
// 里 0 次用它，全部退化成 `bash grep` + 34 次整文件 `read`（288KB），
// 直接催生了单次 delegate 976 万 input tokens。
//
// 关键修正：**按 pattern 匹配改成按 tree-sitter 节点类型（`kind:`）
// 匹配**。实测对比（某 C 仓库 `src/pac.c`，真实函数 22 个）：
//   - pattern `$RET $NAME($$$PARAMS) { $$$BODY }` → 命中 8（召回 36%）
//   - rule    `kind: function_definition`        → 命中 22（召回 100%）
// 在 pac.c / pa.c / hpa.c / sec.c 上抽查，节点类型路线召回均为 100%。
// 本文件 CODE_GRAPH_KIND_TABLE 里每一个 (语言, 语义类型) → 节点类型的
// 映射都是用 `ast-grep scan --inline-rules` 在真实样本上实测过命中数
// 非 0 的，新增条目务必同样实测，否则又会退化成"配置里有、模型不敢用"。
// ─────────────────────────────────────────────────────────────────────

/// 语义查询类型 → tree-sitter 节点类型的映射表。
///
/// 每行 `(语言, 语义 kind, 节点类型)`。同一 (语言, kind) 可以有多行，
/// 会合并成 `any:` 规则（例如 Go 的 `function` 同时覆盖普通函数和方法）。
const CODE_GRAPH_KIND_TABLE: &[(&str, &str, &str)] = &[
    // ── C ──（实测样本：某 C 仓库 src/pac.c, include/.../edata.h）
    ("c", "function", "function_definition"),
    ("c", "struct", "struct_specifier"),
    ("c", "type", "type_definition"),
    ("c", "macro", "preproc_def"),
    ("c", "macro", "preproc_function_def"),
    ("c", "call", "call_expression"),
    ("c", "import", "preproc_include"),
    ("c", "decl", "declaration"),
    // ── C++ ──
    ("cpp", "function", "function_definition"),
    ("cpp", "struct", "struct_specifier"),
    ("cpp", "class", "class_specifier"),
    ("cpp", "call", "call_expression"),
    ("cpp", "import", "preproc_include"),
    // ── Rust ──
    ("rust", "function", "function_item"),
    ("rust", "struct", "struct_item"),
    ("rust", "enum", "enum_item"),
    ("rust", "trait", "trait_item"),
    ("rust", "impl", "impl_item"),
    ("rust", "call", "call_expression"),
    ("rust", "import", "use_declaration"),
    // ── Go ──
    ("go", "function", "function_declaration"),
    ("go", "function", "method_declaration"),
    ("go", "method", "method_declaration"),
    ("go", "type", "type_declaration"),
    ("go", "struct", "struct_type"),
    ("go", "interface", "interface_type"),
    ("go", "call", "call_expression"),
    ("go", "import", "import_declaration"),
    // ── Python ──
    ("python", "function", "function_definition"),
    ("python", "class", "class_definition"),
    ("python", "call", "call"),
    ("python", "import", "import_statement"),
    // ── TypeScript ──
    ("typescript", "function", "function_declaration"),
    ("typescript", "function", "method_definition"),
    ("typescript", "method", "method_definition"),
    ("typescript", "class", "class_declaration"),
    ("typescript", "interface", "interface_declaration"),
    ("typescript", "call", "call_expression"),
    ("typescript", "import", "import_statement"),
    // ── JavaScript ──
    ("javascript", "function", "function_declaration"),
    ("javascript", "function", "method_definition"),
    ("javascript", "method", "method_definition"),
    ("javascript", "class", "class_declaration"),
    ("javascript", "call", "call_expression"),
    ("javascript", "import", "import_statement"),
    // ── Java ──
    ("java", "function", "method_declaration"),
    ("java", "method", "method_declaration"),
    ("java", "class", "class_declaration"),
    ("java", "interface", "interface_declaration"),
    ("java", "call", "method_invocation"),
    ("java", "import", "import_declaration"),
];

/// 文件扩展名 → ast-grep 语言名。
fn code_graph_lang_from_ext(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "rs" => "rust",
        "go" => "go",
        "py" | "pyi" => "python",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "java" => "java",
        _ => return None,
    })
}

/// 从路径推断语言。目录路径推断不出来（没有扩展名），调用方需要显式传 `lang`。
fn code_graph_infer_lang(path: &str) -> Option<&'static str> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())?
        .to_ascii_lowercase();
    code_graph_lang_from_ext(&ext)
}

/// 查 (语言, 语义 kind) 对应的 tree-sitter 节点类型列表。
fn code_graph_node_kinds(lang: &str, kind: &str) -> Vec<&'static str> {
    CODE_GRAPH_KIND_TABLE
        .iter()
        .filter(|(l, k, _)| *l == lang && *k == kind)
        .map(|(_, _, node)| *node)
        .collect()
}

/// 某语言支持的语义 kind 清单（用于报错时给出可选值）。
fn code_graph_kinds_for_lang(lang: &str) -> Vec<&'static str> {
    let mut v: Vec<&'static str> = CODE_GRAPH_KIND_TABLE
        .iter()
        .filter(|(l, _, _)| *l == lang)
        .map(|(_, k, _)| *k)
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// 支持的语言清单。
fn code_graph_supported_langs() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = CODE_GRAPH_KIND_TABLE.iter().map(|(l, _, _)| *l).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// 构造 ast-grep 的 inline YAML 规则。
///
/// 多个节点类型合并成 `any:`；`name` 非空时再叠一层 `all:` + `regex`
/// 约束，让"只找某个函数"不需要把整个目录的匹配都拉回来。
///
/// YAML 缩进必须逐层算准：ast-grep 对 `rule:` 下的结构很严格，缩进错
/// 一层就报 `invalid type: unit value, expected struct SerializableRule`
/// 并返回 0 命中——而 0 命中对模型来说和"这工具没用"无法区分，正是
/// 旧实现被弃用的原因。所以这里的每个分支都有对应的单元测试。
fn code_graph_build_rule(lang: &str, node_kinds: &[&str], name: Option<&str>) -> String {
    // 先构造"匹配节点类型"这个子规则的行列表（不带缩进）。
    let kind_lines: Vec<String> = if node_kinds.len() == 1 {
        vec![format!("kind: {}", node_kinds[0])]
    } else {
        let mut v = vec!["any:".to_string()];
        for nk in node_kinds {
            v.push(format!("- kind: {nk}"));
        }
        v
    };

    let mut out = format!("id: code_graph\nlanguage: {lang}\nrule:\n");
    match name {
        None => {
            // rule:
            //   kind: X
            // 或
            //   any:
            //   - kind: X
            //   - kind: Y
            for l in &kind_lines {
                out.push_str(&format!("  {l}\n"));
            }
        }
        Some(n) => {
            // rule:
            //   all:
            //   - kind: X                （或 - any: / 后续 - kind 再缩进）
            //   - regex: '...'
            out.push_str("  all:\n");
            for (i, l) in kind_lines.iter().enumerate() {
                if i == 0 {
                    out.push_str(&format!("  - {l}\n"));
                } else {
                    // any: 的子项要落在 `- any:` 这一项的内部，缩进 4 空格。
                    out.push_str(&format!("    {l}\n"));
                }
            }
            // YAML 单引号转义：内部单引号写两遍。
            out.push_str(&format!("  - regex: '{}'\n", n.replace('\'', "''")));
        }
    }
    out
}

/// 从一条 ast-grep JSON 匹配里提取"签名行"——匹配文本的第一行，
/// 并把跨行的参数列表压平。这是 code_graph 省 token 的核心。
///
/// 真实仓库实测（某 C 仓库，10 个最大的源文件 / 431 个函数）：
/// 整文件 328,625 B → 签名清单 47,576 B，**6.9x**。单文件区间 3.1x
/// (edata.h，全是短小的 inline getter) ~ 11.6x (大源文件，函数体长)。
/// 函数体越长压缩比越高，正好对应"想看结构时最不需要函数体"。
fn code_graph_signature_of(text: &str) -> String {
    // 取到函数体开始（`{`）之前，避免把整个 body 带回来。
    let head = match text.find('{') {
        Some(i) => &text[..i],
        None => text,
    };
    // 压平多行参数列表：C/Rust 惯用换行对齐，原样回传浪费 token。
    let flat = head.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        text.lines().next().unwrap_or("").trim().to_string()
    } else {
        flat
    }
}

fn code_graph_tool() -> latte_rs_agent_tools::types::Tool {
    use latte_rs_agent_tools::error::ToolError;
    use latte_rs_agent_tools::types::*;
    use std::sync::Arc;

    fn prop(ty: PropertyType, desc: &str) -> ToolInputProperty {
        ToolInputProperty {
            property_type: ty,
            description: Some(desc.into()),
            enum_values: None,
            minimum: None,
            maximum: None,
            min_length: None,
            max_length: None,
        }
    }
    fn optional_schema(props: Vec<(&str, PropertyType, &str)>) -> ToolInputSchema {
        let mut p = std::collections::BTreeMap::new();
        for (name, ty, desc) in props {
            p.insert(name.to_string(), prop(ty, desc));
        }
        ToolInputSchema {
            schema_type: SchemaType::default(),
            properties: p,
            required: None,
            additional_properties: None,
        }
    }

    let handler: SharedToolHandler = Arc::new(|input: serde_json::Value, _ctx| {
        Box::pin(async move {
            let path = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            let raw_pattern = input
                .get("pattern")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let kind = input
                .get("kind")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let name = input
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let mode = input.get("mode").and_then(|v| v.as_str()).unwrap_or("signatures");
            let want_full = mode == "full";

            // ── 预建索引 fast-path ──
            // UI 启动时把定义类符号（function/struct/class/type…）的签名+行号
            // 预扫进 `.latte/code_graph/index.json`。当这次查询是「按定义 kind
            // 列签名」（无裸 pattern、mode=signatures）且索引对该 path 新鲜时，
            // 直接读索引秒回，省掉一次 ast-grep 冷扫。索引不可用/不新鲜/是
            // call/import 这类非索引 kind → 返回 None，落到下面的实时 ast-grep。
            if !want_full {
                if let Some(k) = kind {
                    const MAX_MATCHES: usize = 200;
                    const MAX_CHARS: usize = 12_000;
                    if let Some((lines, total)) = crate::code_graph_index::query_signatures(
                        std::path::Path::new("."),
                        path,
                        k,
                        name,
                        MAX_MATCHES,
                        MAX_CHARS,
                    ) {
                        let mut result = serde_json::Map::new();
                        result.insert(
                            "source".into(),
                            serde_json::Value::String("prebuilt_index".to_string()),
                        );
                        result.insert(
                            "total_matches".into(),
                            serde_json::Value::Number(total.into()),
                        );
                        result.insert(
                            "shown".into(),
                            serde_json::Value::Number(lines.len().into()),
                        );
                        result.insert(
                            "matches".into(),
                            serde_json::Value::String(lines.join("\n")),
                        );
                        if lines.len() < total {
                            result.insert("note".into(), serde_json::Value::String(format!(
                                "只回传了 {}/{total} 条（来自预建索引）。收窄：把 path 指向单个文件、\
                                 或用 name 过滤。",
                                lines.len()
                            )));
                        }
                        return Ok(serde_json::Value::Object(result));
                    }
                }
            }

            // ── ast-grep 可用性预检 ──
            // 旧实现直接 spawn，缺二进制时模型只拿到一句
            // "ast-grep failed: No such file"，试一次就再也不用这个工具。
            // 这里给可执行的安装指引 + 明确的降级建议。
            if tokio::process::Command::new("ast-grep")
                .arg("--version")
                .output()
                .await
                .map(|o| !o.status.success())
                .unwrap_or(true)
            {
                return Err(ToolError::execution_str(
                    "code_graph",
                    "本机没有可用的 ast-grep，code_graph 无法工作。\
                     安装：`brew install ast-grep` 或 `cargo install ast-grep --locked`。\
                     在装好之前请改用 search/bash grep + 带行范围的 read。"
                        .to_string(),
                ));
            }

            // ── 语言判定 ──
            let lang_owned: String;
            let lang: &str = match input.get("lang").and_then(|v| v.as_str()).map(str::trim) {
                Some(l) if !l.is_empty() => {
                    lang_owned = l.to_ascii_lowercase();
                    &lang_owned
                }
                _ => match code_graph_infer_lang(path) {
                    Some(l) => l,
                    None => {
                        // 目录路径没有扩展名，推断不出语言。
                        // 用 Validation 而非 execution：这是确定性参数
                        // 错误，同参数重试必败，必须让上层判不可重试、
                        // 直接把提示喂回模型改参数（见
                        // `classify_tool_execution_error`）。
                        return Err(ToolError::validation(
                            format!(
                                "code_graph: 无法从 path='{path}' 推断语言（目录或未知扩展名），\
                                 请显式传 lang。支持的 lang：{}",
                                code_graph_supported_langs().join(", ")
                            ),
                            vec![latte_rs_agent_tools::error::ValidationIssue {
                                path: "lang".into(),
                                message: "目录路径必须显式指定 lang".into(),
                            }],
                        ));
                    }
                },
            };

            // ── 组装 ast-grep 命令 ──
            // 两条路径：语义 kind（走 scan --inline-rules，按节点类型匹配，
            // 召回可靠）与裸 pattern（走 run -p，逃生舱，交给模型自己写）。
            let mut cmd = tokio::process::Command::new("ast-grep");
            let rule_text;
            match (kind, raw_pattern) {
                (Some(k), _) => {
                    let node_kinds = code_graph_node_kinds(lang, k);
                    if node_kinds.is_empty() {
                        let avail = code_graph_kinds_for_lang(lang);
                        // 同上：确定性参数错误，走 Validation 不重试。
                        return Err(ToolError::validation(
                            if avail.is_empty() {
                                format!(
                                    "code_graph: 不支持的 lang='{lang}'。支持：{}",
                                    code_graph_supported_langs().join(", ")
                                )
                            } else {
                                format!(
                                    "code_graph: lang='{lang}' 不支持 kind='{k}'。可用 kind：{}。\
                                     或改用 pattern 参数写裸 ast-grep 模式。",
                                    avail.join(", ")
                                )
                            },
                            vec![latte_rs_agent_tools::error::ValidationIssue {
                                path: if avail.is_empty() { "lang".into() } else { "kind".into() },
                                message: "取值不在支持范围内".into(),
                            }],
                        ));
                    }
                    rule_text = code_graph_build_rule(lang, &node_kinds, name);
                    cmd.args(["scan", "--inline-rules", &rule_text, path, "--json=compact"]);
                }
                (None, Some(p)) => {
                    cmd.args(["run", "-l", lang, "-p", p, path, "--json=compact"]);
                }
                (None, None) => {
                    return Err(ToolError::validation(
                        format!(
                            "code_graph: 必须提供 kind 或 pattern 之一。lang='{lang}' 可用 kind：{}",
                            code_graph_kinds_for_lang(lang).join(", ")
                        ),
                        vec![latte_rs_agent_tools::error::ValidationIssue {
                            path: "kind".into(),
                            message: "kind 与 pattern 至少提供一个".into(),
                        }],
                    ));
                }
            }

            let output = cmd.output().await.map_err(|e| {
                ToolError::execution_str("code_graph", format!("ast-grep 执行失败: {e}"))
            })?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

            let parsed: Vec<serde_json::Value> = if stdout.trim().is_empty() {
                Vec::new()
            } else {
                serde_json::from_str(stdout.trim()).unwrap_or_default()
            };

            // ── 结果格式化 ──
            // signatures（默认）：`file:line: 签名`，一条一行。
            // full：带完整匹配文本，只在确实要读实现时用。
            //
            // 输出封顶是必须的：`kind: call_expression` 打在 src/ 上
            // 轻松几千条命中，不封顶就会把这个"省 token 的工具"变成
            // 新的 token 黑洞。截断时明确告诉模型怎么收窄。
            const MAX_MATCHES: usize = 200;
            const MAX_CHARS: usize = 12_000;
            let total = parsed.len();
            let mut lines: Vec<String> = Vec::new();
            let mut chars = 0usize;
            let mut emitted = 0usize;
            // 输出路径归一化基准：ast-grep 未设 current_dir，继承进程 cwd
            // （= 仓库根）。给它绝对 path 时它回传绝对 `file`，给它 `.` 时
            // 回传相对 `file`。而 agent 的 resolve_tool_input_against_cwd 会把
            // 相对 path 拼成绝对，所以常态是绝对——与预建索引 fast-path 回传的
            // 相对路径不一致。这里统一 strip 掉 cwd 前缀，让两条路径（索引 /
            // 实时）以及 read/search/find 的相对显示口径保持一致。
            let proc_cwd = std::env::current_dir().ok();
            for m in parsed.iter().take(MAX_MATCHES) {
                let file_raw = m.get("file").and_then(|v| v.as_str()).unwrap_or("?");
                // 绝对路径且落在 cwd 下 → 转相对；否则原样。
                let file: String = proc_cwd
                    .as_ref()
                    .and_then(|c| std::path::Path::new(file_raw).strip_prefix(c).ok())
                    .map(|rel| rel.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_else(|| file_raw.to_string());
                let line = m
                    .get("range")
                    .and_then(|r| r.get("start"))
                    .and_then(|s| s.get("line"))
                    .and_then(|l| l.as_u64())
                    .map(|l| l + 1) // ast-grep 的 line 是 0-based
                    .unwrap_or(0);
                let text = m.get("text").and_then(|v| v.as_str()).unwrap_or("");
                let body = if want_full {
                    text.to_string()
                } else {
                    code_graph_signature_of(text)
                };
                let entry = format!("{file}:{line}: {body}");
                if chars + entry.len() > MAX_CHARS {
                    break;
                }
                chars += entry.len();
                emitted += 1;
                lines.push(entry);
            }

            let mut result = serde_json::Map::new();
            result.insert("lang".into(), serde_json::Value::String(lang.to_string()));
            result.insert(
                "total_matches".into(),
                serde_json::Value::Number(total.into()),
            );
            result.insert(
                "shown".into(),
                serde_json::Value::Number(emitted.into()),
            );
            result.insert(
                "matches".into(),
                serde_json::Value::String(lines.join("\n")),
            );
            if emitted < total {
                result.insert(
                    "note".into(),
                    serde_json::Value::String(format!(
                        "只回传了 {emitted}/{total} 条。收窄方式：把 path 指向单个文件、\
                         用 name 参数按名字过滤，或换更具体的 kind。"
                    )),
                );
            }
            if total == 0 {
                result.insert(
                    "note".into(),
                    serde_json::Value::String(format!(
                        "0 命中。检查：path 是否存在、lang='{lang}' 是否正确（当前由\
                         {} 得出）、kind 是否适合该语言（可用：{}）。",
                        if input.get("lang").is_some() { "显式参数" } else { "路径扩展名推断" },
                        code_graph_kinds_for_lang(lang).join(", ")
                    )),
                );
            }
            if !stderr.is_empty() {
                result.insert("stderr".into(), serde_json::Value::String(stderr));
            }
            Ok(serde_json::Value::Object(result))
        })
    });

    let schema = optional_schema(vec![
        (
            "path",
            PropertyType::String,
            "搜索路径，文件或目录。默认当前目录。指向单个文件时可省略 lang（按扩展名推断）",
        ),
        (
            "kind",
            PropertyType::String,
            "语义查询类型（推荐用法，按 AST 节点类型精确匹配）：\
             function / method / struct / class / interface / enum / trait / impl / type / macro / call / import / decl。\
             不同语言支持的子集不同，传错会返回该语言的可用清单",
        ),
        (
            "lang",
            PropertyType::String,
            "语言：c / cpp / rust / go / python / typescript / javascript / java。\
             path 是目录时必填",
        ),
        (
            "name",
            PropertyType::String,
            "可选，按正则过滤匹配文本（如只找名字含 decay 的函数）。用它替代把整个目录的匹配全拉回来",
        ),
        (
            "mode",
            PropertyType::String,
            "signatures（默认，只回签名行，省 token）| full（回完整匹配文本，仅在需要读实现时用）",
        ),
        (
            "pattern",
            PropertyType::String,
            "逃生舱：裸 ast-grep 模式（如 'pac_decay_all($$$ARGS)'）。\
             kind 覆盖不到时才用；kind 与 pattern 同时给出时 kind 优先",
        ),
    ]);

    Tool::builder(
        "code_graph",
        "结构化代码导航：按 AST 节点类型查函数/结构体/类/调用点/import，比 grep 精准、比整文件 read 省得多。\
         典型用法——先用它建地图（`kind=function` + `path=src/foo.c` 拿到全部函数签名和行号），\
         再只对少数热点用带行范围的 read 精读。查「谁调用了 X」用 `kind=call` + `name=X`。\
         实测（某 C 仓库 10 个最大源文件 / 431 个函数）：整文件 328KB → 签名清单 47KB，6.9x。\
         支持 c/cpp/rust/go/python/typescript/javascript/java。需要本机装有 ast-grep",
        schema,
        handler,
    )
    .timeout(std::time::Duration::from_secs(30))
    .build()
}

pub(crate) fn tool_usage_prompt(_allowed: &[String]) -> String {
    // 不在 prompt 里枚举工具名：可用工具完全由请求 `tools` 字段的
    // schema 决定（"一切按照工具选择来"）。配置层别名（如 bash）与
    // 注册表名字一旦漂移，枚举出来的清单就是在对模型撒谎。
    r#"
## Tools

Call tools via the native function-calling interface (the request `tools`
field carries each tool's name, description, and JSON schema). Do NOT emit
`<tool_call>` text blocks -- they are no longer parsed. Inspect each tool
result and continue until the task is done.
"#
    .to_string()
}

/// 当前配置里的角色花名册：`id(Name)` 排序拼接。delegate 工具的
/// description/schema 和系统提示共用这一份 —— 角色编辑器新建/改名的
/// 角色下个 session 自动出现，不靠静态列表。
pub(crate) fn role_roster_text(merged: &AgentConfig) -> String {
    let mut entries: Vec<String> = merged
        .roles
        .values()
        .map(|t| {
            if t.name.is_empty() || t.name == t.id {
                t.id.clone()
            } else {
                format!("{}({})", t.id, t.name)
            }
        })
        .collect();
    entries.sort();
    entries.join(", ")
}


/// 单 session 累计 delegate 工具调用上限。env `LATTE_MAX_DELEGATES_PER_SESSION`
/// 覆盖；非法值回退到默认。0 = 禁用限制（无上限）。
/// 详见 `docs/perf/diagnose-latency.md` §5（#1-A 方案）。
pub fn default_max_delegates() -> u32 {
    let raw = std::env::var("LATTE_MAX_DELEGATES_PER_SESSION").ok();
    match raw.and_then(|s| s.parse::<u32>().ok()) {
        Some(n) => n,
        // 默认无限制。delegate 路径有 wall-clock 超时（set_deadline）兜底；
        // 交互式 chat 没有，按设计由人工 ⏸ / advisor 叫停 + 死循环熔断兜住。
        None => 0,
    }
}
/// delegate 工具提示：可用专家列表来自 `merged.roles` 动态生成
/// （与角色编辑器同源），不再硬编码 7 个基础角色。
fn delegate_tool_hint(merged: &AgentConfig) -> String {
    let roster = role_roster_text(merged);
    let roster_line = if roster.is_empty() {
        "（当前配置里没有任何角色）".to_string()
    } else {
        roster
    };
    format!(
        r#"
### 关键规则：你必须使用 delegate 工具

你**必须**使用 `delegate` 工具来完成任务，**绝不能**直接输出计划而不执行。

正确的流程：
1. 分析任务 → 决定需要哪些专家
2. **立即**调用 delegate 工具派发任务
3. 等专家返回结果
4. 综合所有结果输出最终答案

可用专家（来自角色配置，含角色编辑器新建的自定义角色）：{roster_line}

失败升级：工具调用连续失败、专家报错且原因不明、或需要在多个方案间取舍时 → 派 advisor 诊断；诊断清楚之前不要直接回答用户，更不要重复回答旧问题。

**错误流程（禁止）：**
- 只输出计划而不调用 delegate ← 这是最常见的错误！不要这样做！
- 自己分析而不派发给专家

用 native function-calling 调 delegate 工具（参数 role + task）。
"#
    )
}

/// workflow 工具提示：动态列出 `.latte/workflows.d`（项目 + 全局）里
/// 实际可用的 workflow —— UI 管理界面新建的自定义流程会出现在这里，
/// 模型不需要靠猜。清单为空时退化成不带列表的通用提示。
fn workflow_tool_hint(cwd: &std::path::Path) -> String {
    let available = crate::workflow::list_workflows(cwd);
    let list = if available.is_empty() {
        "（当前 .latte/workflows.d 里没有可用 workflow，只能 delegate）\n".to_string()
    } else {
        available
            .iter()
            .map(|(n, d)| {
                if d.is_empty() {
                    format!("- `{n}`\n")
                } else {
                    format!("- `{n}` — {d}\n")
                }
            })
            .collect()
    };
    format!(
        r#"
### 你还可以用 workflow 工具触发多角色工作流

当任务适合**固定的多角色流水线**时，调用 `workflow` 而不是逐个 delegate。
当前可用的 workflow（含 UI 管理界面新建的自定义流程）：

{list}
用 native function-calling 调 workflow 工具（参数 name + topic）。
若 workflow 失败，错误信息会带 wf_id——用 name + resume=<wf_id> 从断点续跑，不要从头重跑。

判断标准（分派前先想流程）：
- 单点问题（读代码、改文件、审查某个具体实现）→ delegate
- 需要多个角色按固定流程协作的完整任务 → workflow，从上面清单里选最贴合的
- 调用 workflow 前先用一句话说明：选哪个、为什么、预期拿到什么结论
- workflow 会跑完整条流水线并把结论返回给你；你综合后再回复用户。

**怎么挑：按交付物落点，不按话题词（唯一判别依据）**

1. 先写下用户点名要的东西**最后落在哪儿**——任务看板条目 / 某个文件 / 对话里的结论 / 代码改动。
2. 再从上面清单里挑**落点相同**的那条：每条流程的描述末尾都标了「落点：」，那是它实际产出什么、
   产出物落在哪儿。挑之前把候选的落点与第 1 步写下的落点逐字比一遍。
3. 话题词（"学习""设计""重构""调研"）只说明**内容**，不决定落点：同一个话题既可能是"要一份清单"，
   也可能是"现在就要内容"，两者落点不同、流程也就不同。用话题词匹配流程名是最常见的选错方式。
4. 用户诉求里出现**几个不同落点，就发几个 workflow 调用**。不许让一条流程"顺带覆盖"另一个落点：
   宣称"一次覆盖"之前，先把每个落点的产出物写出来；写不出来就是在合并交差。

**调研要收口（不是限制轮数）**：调研类流程（`explore` / `design_brainstorm`）想探几轮就探几轮——
大仓库分模块深入是正当的。但每一轮都必须**换一个具体问题**，且随时对着交付物问自己一句：
「这一轮结束后，用户点名的交付物离产出更近了，还是我只是更懂了？」

- 连续两轮答案都是"只是更懂了" → 已有结论就够动手了，转去跑产出交付物的流程。
- **不要重复探索同一范围**：把上一轮的 topic 换几个词再探一遍不会带来新信息。还要探就必须说得出
  **这一轮补的是哪一个具体问题**；说不出来（只是「再摸一遍结构」）就说明该转产出了。
- 用户诉求里点名过交付物时，调研流程的返回末尾会附一条**交付物提醒**（列出还欠着什么、该跑哪个流程）。
  它不阻断你，但别忽略它。
- 把「通读 XXX 产出解剖报告」派成 `delegate`，与再跑一轮 `explore` 是同一件事，同样受这条约束。

**用户选了「先不锁方向 / 以后再定」不要退回调研**：这类回答通常已经隐含阶段骨架
（例如「读整体 → 深读 X → 再定改动点」就是 3 个阶段任务）。直接按这个骨架拆任务清单，
把"定改动点"本身作为最后一个任务；方向没锁 ≠ 无法拆分。

workflow 失败时的兜底（必须遵守）：
- 错误信息里带失败原因——gate 被拦时会附「不合格产出摘要」（REJECT 理由）。先读懂它。
- 能修复的（方案有冲突、内容可调整）：修复后用 resume 续跑，或直接 delegate 重做失败的那一步。
- 不能修复或需要用户拍板的：把失败原因、已完成的中间成果、可选的下一步向用户解释清楚，由用户决定。
- 禁止不解释原因、不给出路，只把错误原样转述给用户就结束。
"#
    )
}
// 进程级单调序号，保证 plan_id 全局唯一。
static PLAN_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 生成全局唯一的 plan_id（`plan` 工具与 slash 命令路径的自动提案共用）。
fn next_plan_id(role_id: &str) -> String {
    format!(
        "plan-{}-{}",
        role_id,
        PLAN_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// plan 阶段门复位为 [`PlanStage::Normal`]，并把上一轮未批准的 plan
/// 弹窗从补发表销账。
///
/// 复位的语义是"新的用户消息 = 新的决策周期，上一份清单不再约束
/// delegate"。既然它已经不作数了，就不该在下次重连时又被补发弹一遍
/// （消息气泡上的「📥 导入任务看板」按钮和右键补救仍在，用户想导入
/// 随时能开）。
fn reset_plan_stage(plan_stage: &SharedPlanStage) {
    let prev = std::mem::replace(&mut *plan_stage.write(), PlanStage::Normal);
    if let PlanStage::PendingApproval { plan_id } = prev {
        crate::choice::dismiss_prompt(&plan_id);
    }
}

/// 从 workflow 产出文本里提取任务看板清单：找第一个内容为
/// `{"tasks": [...]}` 的 ```json 代码块，逐项按 [`PlanTask`] 解析校验。
/// 找不到/解析失败/空清单都返回 None（不提案）。
fn extract_plan_tasks(summary: &str) -> Option<Vec<PlanTask>> {
    let mut rest = summary;
    while let Some(start) = rest.find("```json") {
        let after = &rest[start + "```json".len()..];
        let end = after.find("```")?;
        let body = after[..end].trim();
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(body) {
            if let Some(arr) = val.get("tasks").and_then(|t| t.as_array()) {
                if !arr.is_empty() {
                    let mut tasks: Vec<PlanTask> = Vec::with_capacity(arr.len());
                    let mut ok = true;
                    for t in arr {
                        match serde_json::from_value::<PlanTask>(t.clone()) {
                            Ok(pt) if !pt.title.trim().is_empty() => tasks.push(pt),
                            _ => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    if ok {
                        return Some(tasks);
                    }
                }
            }
        }
        rest = &after[end + 3..];
    }
    None
}

/// slash 命令路径的自动提案：workflow 成功且产出带任务清单时，代
/// manager 广播 `PlanProposed`（slash 路径不经 manager 的 agent loop，
/// 没人会调 `plan` 工具——现场实锤两次会话 0 次 PlanProposed，
/// 「添加任务」弹窗从未弹出）。返回是否发出了提案。
pub(crate) fn propose_plan_from_summary(
    event_tx: &broadcast::Sender<ChatEvent>,
    plan_stage: &SharedPlanStage,
    role_id: &str,
    summary: &str,
    // 弹框快照落盘归属 `(cwd, session_id)`：进程重启后「添加任务」
    // 弹窗仍能补发。`None` = 不落盘（CLI / 测试）。
    persist: Option<(&std::path::Path, &str)>,
) -> bool {
    let Some(tasks) = extract_plan_tasks(summary) else {
        return false;
    };
    let plan_id = next_plan_id(role_id);
    *plan_stage.write() = PlanStage::PendingApproval {
        plan_id: plan_id.clone(),
    };
    let proposed = ChatEvent::PlanProposed {
        role_id: role_id.to_string(),
        plan_id: plan_id.clone(),
        tasks,
    };
    // 未处理快照（同 `plan` 工具）：没订阅者/broadcast 落后时弹窗不丢，
    // 导入成功后由 `POST /api/tasks/import` 按 plan_id 销账。
    // 带 persist 时同时落盘 —— 内存表救不了进程重启。
    crate::choice::register_prompt(&plan_id, proposed.clone(), event_tx.clone(), persist);
    let _ = event_tx.send(proposed);
    true
}

/// 注册 `plan` 工具：把结构化任务清单提交给用户在弹窗里勾选导入
/// 任务看板。manager 在 `implementation_plan` workflow 跑完（或手持
/// 一份具体任务清单）后调它。与 `register_generate_image_tool` 同构
/// （schema + 捕获 event_tx 的 SharedToolHandler），但更简单--不
/// 调模型、不读写文件，只校验 tasks 并广播 `PlanProposed` 事件。
///
/// 工具返回立即（fire-and-forget）：发完事件就回"已提交 N 个候选"，
/// LLM 的 turn 结束；弹窗是纯 UI 侧异步行为，用户何时导入都行。
/// 若用户误关弹窗，可右键 PlanProposed 消息选「导入任务看板」补救
/// （右键读消息上存的结构化 tasks，不靠文本解析）。
///
/// plan 阶段门：发出 PlanProposed 后把 `plan_stage` 置为
/// [`PlanStage::PendingApproval`]——用户导入任务看板（置 Approved）
/// 或发下一条消息（复位 Normal）之前，`register_delegate_tool` 的
/// handler 会拒绝派发实现类角色（programmer*/devops*）。
/// plan 清单内 paths 的机械校验（提交前调用，失败则整单拒绝、不进
/// PendingApproval，模型可修正后同轮重调）：
/// - 存在性：路径逐级向上找已存在的祖先；第一级段都不存在 → 判定
///   幻觉路径并拒绝（要新建的文件只要祖先目录存在即放行）。
/// - 清单内重叠：两个任务的 paths 存在组件级前缀包含 → 拒绝
///   （paths 互不重叠是并行派发不互踩的前提，靠 prompt 嘱咐不可靠）。
/// paths 为空 = 未声明范围，跳过校验。
fn validate_plan_paths(cwd: &std::path::Path, tasks: &[PlanTask]) -> Result<(), String> {
    let normalize = |p: &str| -> String {
        p.trim()
            .trim_start_matches("./")
            .trim_end_matches('/')
            .to_string()
    };
    // ── 存在性 ──
    // 反馈只报路径本身（不带任务标题）并去重：tool_result 只有 256
    // 字节，带标题的长清单一撑就爆，模型看到的是被砍掉尾部的残句。
    let mut missing: Vec<String> = Vec::new();
    let mut check_exists = |_title: &str, raw: &str| {
        let p = normalize(raw);
        if p.is_empty() {
            return;
        }
        let mut cur = cwd.to_path_buf();
        let mut top_exists = false;
        for (idx, comp) in std::path::Path::new(&p).components().enumerate() {
            if matches!(comp, std::path::Component::CurDir) {
                continue;
            }
            cur.push(comp.as_os_str());
            if idx == 0 {
                top_exists = cur.exists();
            }
            if !cur.exists() {
                break;
            }
        }
        if !top_exists && !missing.contains(&p) {
            missing.push(p);
        }
    };
    for t in tasks {
        for p in &t.paths {
            check_exists(&t.title, p);
        }
        for s in &t.subtasks {
            for p in &s.paths {
                check_exists(&s.title, p);
            }
        }
    }
    if !missing.is_empty() {
        // 反馈通道只有 256 字节（tool_result 截断，见 agent.rs 终止态
        // 处理），长清单会被砍掉尾部，模型只能看到前一两条 —— 与其被
        // 动截断，不如主动只报前 3 条 + 总数。
        let head = missing.iter().take(3).cloned().collect::<Vec<_>>().join("、");
        let more = if missing.len() > 3 {
            format!("（共 {} 处）", missing.len())
        } else {
            String::new()
        };
        return Err(format!(
            "paths 的第一级目录在仓库里不存在（疑似幻觉路径，先用 read/search 核实）：{head}{more}"
        ));
    }
    // ── 清单内重叠（仅顶层任务两两比较；子任务继承父任务范围） ──
    //
    // 只读任务不参与校验：重叠的唯一害处是"并行写互相覆盖"，纯阅读/
    // 学习/调研类任务天然可以共用同一份代码。（实测
    // 会话：8 个纯阅读的学习任务因为都要读
    // `include/foo/internal` 被判 45 处冲突，5855 字错误回喂时又
    // 被截到 256 字节，模型看不全、改不动，连撞两次 plan 后靠蒙才过。）
    let mut pairs = 0usize;
    // 冲突热点：重叠路径 → 牵涉到的任务下标集合。按路径聚合而不是按
    // "任务对"罗列，同一个热点路径只报一次，反馈短且可执行。
    let mut hot: std::collections::HashMap<String, std::collections::BTreeSet<usize>> =
        std::collections::HashMap::new();
    for i in 0..tasks.len() {
        if is_readonly_plan_task(&tasks[i]) {
            continue;
        }
        for j in (i + 1)..tasks.len() {
            if is_readonly_plan_task(&tasks[j]) {
                continue;
            }
            for a in tasks[i].paths.iter().map(|p| normalize(p)) {
                for b in tasks[j].paths.iter().map(|p| normalize(p)) {
                    if a.is_empty() || b.is_empty() {
                        continue;
                    }
                    let (pa, pb) = (std::path::Path::new(&a), std::path::Path::new(&b));
                    if pa == pb || pa.starts_with(pb) || pb.starts_with(pa) {
                        pairs += 1;
                        // 记较短的那个（= 包含关系里的父前缀）作热点。
                        let key = if a.len() <= b.len() { a.clone() } else { b.clone() };
                        let entry = hot.entry(key).or_default();
                        entry.insert(i);
                        entry.insert(j);
                    }
                }
            }
        }
    }
    if pairs > 0 {
        let mut top: Vec<(String, usize)> =
            hot.into_iter().map(|(p, ts)| (p, ts.len())).collect();
        // 牵涉任务最多的热点优先；同数按路径名稳定排序。
        top.sort_by(|x, y| y.1.cmp(&x.1).then_with(|| x.0.cmp(&y.0)));
        let head = top
            .iter()
            .take(2)
            .map(|(p, n)| format!("{p}({n} 个任务)"))
            .collect::<Vec<_>>()
            .join("、");
        return Err(format!(
            "paths 范围重叠 {pairs} 处，集中在：{head}。并行会互相覆盖（派发被看板 409），\
             请让各任务范围互不重叠；纯阅读任务的 labels 加「只读」可跳过本校验"
        ));
    }
    Ok(())
}

/// 纯阅读类任务判定：labels 里带只读标记的任务不参与 paths 重叠校验。
///
/// 用 labels 而不是新增字段，是为了不动 `PlanTask` 与后端 `ImportTask`
/// / 前端 `ImportTask` 的三处同构约定。
fn is_readonly_plan_task(t: &PlanTask) -> bool {
    const MARKERS: &[&str] = &["只读", "readonly", "read-only", "学习", "learning", "调研"];
    t.labels
        .iter()
        .any(|l| MARKERS.iter().any(|m| l.eq_ignore_ascii_case(m)))
}

/// [`PlanTask`] 认得的全部键（含 `paths` 的 serde 别名）。
///
/// serde 对未知键默认静默忽略，`deny_unknown_fields` 又会把
/// `extract_plan_tasks` 那条路径上 workflow 产出的 `id`/`week`/
/// `dependencies`/`risks` 一并判死。折中：解析照旧宽容，但把被忽略的
/// 键**原样回执给模型**，让"字段名写错 → 数据静默消失"变成显式反馈。
const PLAN_TASK_KNOWN_KEYS: &[&str] = &[
    "title",
    "description",
    "priority",
    "labels",
    "workflow",
    "paths",
    "subtasks",
    // paths 的 serde 别名
    "involved_paths",
    "involved_files",
    "affected_paths",
    "affected_files",
    "file_paths",
    "files",
];

/// 递归收集 `tasks` 数组里所有不被 [`PlanTask`] 认得的键（去重、稳定
/// 排序）。返回空 = 没有字段被静默丢弃。
fn unknown_plan_task_keys(tasks_arr: &[serde_json::Value]) -> Vec<String> {
    fn walk(v: &serde_json::Value, out: &mut std::collections::BTreeSet<String>) {
        let Some(obj) = v.as_object() else { return };
        for (k, val) in obj {
            if !PLAN_TASK_KNOWN_KEYS.contains(&k.as_str()) {
                out.insert(k.clone());
            }
            if k == "subtasks" {
                if let Some(arr) = val.as_array() {
                    for s in arr {
                        walk(s, out);
                    }
                }
            }
        }
    }
    let mut out = std::collections::BTreeSet::new();
    for t in tasks_arr {
        walk(t, &mut out);
    }
    out.into_iter().collect()
}

/// `pub` 而非 `pub(crate)`：CLI REPL 也要注册它，否则声明了该工具的
/// 角色（manager）在 chat 里调用直接 ToolNotFound，而 UI 里正常。
pub fn register_plan_tool(
    tm: &Arc<dyn latte_rs_agent_tools::types::ToolManager>,
    event_tx: broadcast::Sender<ChatEvent>,
    role_id: String,
    plan_stage: SharedPlanStage,
    cwd: &std::path::Path,
    // 所属 UI session：弹框快照落盘时的归属键。空字符串 = 不落盘
    // （CLI / 测试）。进程重启后「添加任务」弹窗靠这份落盘补发。
    session_id: String,
) -> Result<(), Box<dyn std::error::Error>> {
    use latte_rs_agent_tools::types::{
        PropertyType, SchemaType, SharedToolHandler, Tool, ToolInputProperty, ToolInputSchema,
    };

    // tasks 是数组；ToolInputProperty 无嵌套 items schema，故用描述
    // 把每项结构讲清（title/description/priority/labels/workflow/
    // paths/subtasks）。LLM 按描述产出，handler 逐项 serde 解析 + 校验。
    let input_schema = ToolInputSchema {
        schema_type: SchemaType,
        properties: vec![
            ("tasks".into(), ToolInputProperty {
                property_type: PropertyType::Array,
                description: Some(
                    "任务候选清单（一次调用提交整份清单：拆分出几个任务就放几项，禁止每个任务单独调一次本工具——上一份清单未获用户批准时后续调用会被拒绝）。每项是对象：{title(必填,一句话), description(做什么+验收标准), priority(1-4,1最高), labels(字符串数组), workflow(执行该任务的workflow名:tdd_development/bug_triage/update_docs/annotate_code/learn/learn_loop;学习或讲解类任务绑learn(一次性教程)或learn_loop(逐知识点讲解+出题);没有贴合的必须留空走manager直接执行,禁止硬绑不相关的workflow), paths(可选,字符串数组,任务涉及的文件/目录前缀如\"src/ringbuf\";并行执行时范围重叠的任务会被拒绝派发,拆任务时让各任务范围互不重叠;纯阅读/学习类任务在 labels 里加\"只读\"即可免除重叠校验), subtasks(同构数组,最多一层)}. 调用本工具后任务会出现在用户弹窗里供勾选导入任务看板，不要再以 Markdown 列表输出任务。".into()
                ),
                enum_values: None, minimum: None, maximum: None, min_length: None, max_length: None,
            }),
        ].into_iter().collect(),
        required: Some(vec!["tasks".into()]),
        ..Default::default()
    };

    let handler_role_id = role_id.clone();
    let handler_cwd = cwd.to_path_buf();
    let handler_session_id = session_id;
    let handler: SharedToolHandler = Arc::new(move |input: serde_json::Value, _ctx| {
        let event_tx = event_tx.clone();
        let role_id = handler_role_id.clone();
        let plan_stage = plan_stage.clone();
        let cwd = handler_cwd.clone();
        let session_id = handler_session_id.clone();
        Box::pin(async move {
            let tool_err = |msg: String| latte_rs_agent_tools::error::ToolError::Other(msg);

            // 一次一清单：上一份清单还在等用户批准时拒绝再次提交。
            // 任务拆分必须把全部任务合并进一次调用的 tasks 数组——
            // 逐任务多次调用会让用户弹窗每次只有 1 个任务（观测到
            // 的实际坏行为：11 个任务弹了 11 次窗）。
            if let PlanStage::PendingApproval { plan_id } = &*plan_stage.read() {
                return Err(tool_err(format!(
                    "上一份任务清单还在等待用户批准（plan_id={plan_id}）。plan 工具每轮只提交一次：请把所有拆分出的任务合并进 tasks 数组一次提交；如需修改清单，等用户处理完上一份（导入看板或发消息）后再提交。"
                )));
            }

            let tasks_val = input
                .get("tasks")
                .ok_or_else(|| tool_err("missing 'tasks' field".into()))?;
            let tasks_arr = tasks_val
                .as_array()
                .ok_or_else(|| tool_err("'tasks' must be an array".into()))?;
            if tasks_arr.is_empty() {
                return Err(tool_err("'tasks' must not be empty".into()));
            }
            // 逐项解析 + 校验 title 非空（与后端 import_tasks 的校验对齐）。
            let mut tasks: Vec<PlanTask> = Vec::with_capacity(tasks_arr.len());
            for (i, t) in tasks_arr.iter().enumerate() {
                let pt: PlanTask = serde_json::from_value(t.clone())
                    .map_err(|e| tool_err(format!("tasks[{i}] invalid: {e}")))?;
                if pt.title.trim().is_empty() {
                    return Err(tool_err(format!("tasks[{i}].title must not be empty")));
                }
                tasks.push(pt);
            }

            // paths 机械校验（存在性 + 清单内重叠）：失败整单拒绝、
            // 不进 PendingApproval——模型可修正后同轮重调。
            validate_plan_paths(&cwd, &tasks).map_err(tool_err)?;

            // 被静默丢弃的键：不拦（workflow 产出的 id/week/
            // dependencies/risks 是合法附加信息），但必须回执，否则
            // 「字段名写错 → 整个数组消失 → 校验空转通过」无从发现。
            let dropped = unknown_plan_task_keys(tasks_arr);
            let dropped_note = if dropped.is_empty() {
                String::new()
            } else {
                format!(
                    "\n⚠️ 以下字段不在 plan 工具 schema 内、已被忽略：{}。\
                     若其中有本该写进 paths 的路径信息（schema 字段名是 paths），\
                     请改名后重新提交整份清单。",
                    dropped.join("、")
                )
            };

            let plan_id = next_plan_id(&role_id);
            let n = tasks.len();
            let proposed = ChatEvent::PlanProposed {
                role_id: role_id.clone(),
                plan_id: plan_id.clone(),
                tasks,
            };
            // 未处理快照：plan 弹窗是这份清单的唯一入口，broadcast 丢
            // 一次（无订阅者 / Lagged）就等于清单永久卡在
            // PendingApproval——delegate 被门禁挡着，用户却没有弹窗可
            // 导入。导入成功时 `POST /api/tasks/import` 按 plan_id 销账。
            // 同时落盘：内存快照救不了进程重启（重启后清单同样只剩
            // PendingApproval 的门禁，弹窗无影无踪）。
            crate::choice::register_prompt(
                &plan_id,
                proposed.clone(),
                event_tx.clone(),
                (!session_id.is_empty()).then_some((cwd.as_path(), session_id.as_str())),
            );
            let _ = event_tx.send(proposed);
            // plan 阶段门：任务清单已提交，等用户在弹窗导入任务看板；
            // 批准前 delegate 实现类角色会被工具层拒绝。
            *plan_stage.write() = PlanStage::PendingApproval {
                plan_id: plan_id.clone(),
            };
            Ok(serde_json::Value::String(format!(
                "已提交 {n} 个任务候选给用户选择（plan_id={plan_id}）。请在弹窗中勾选要导入任务看板的项；若弹窗已关闭，可右键本条消息选「导入任务看板」补救。{dropped_note}"
            )))
        })
    });

    let tool = Tool::builder(
        "plan".to_string(),
        "把一份结构化任务清单提交给用户，用户在弹窗里勾选后导入任务看板（backlog）。用于 implementation_plan workflow 跑完或手持具体任务清单时把任务交给看板。参数 tasks 是任务对象数组——一次调用提交整份清单（拆分出几个任务就放几项），不要逐任务多次调用本工具。".to_string(),
        input_schema,
        handler,
    )
    .build();

    tm.register(tool, Some(&role_id));
    Ok(())
}

/// ask 工具的阻塞模式（workflow/delegate 子代理用）：广播选择题后
/// 挂起，等用户在弹框里回答，答案直接作为工具结果返回给提问的子
/// 代理。顶层 turn 不要用——顶层没有"等待方"，靠下一条 user 消息
/// 回喂（fire-and-forget）。
/// 阻塞模式参数：工具挂起等用户，取消则报错退出，不再有超时路径。
#[derive(Clone)]
pub struct AskBlocking {
    /// 用户回答台账：收到答案**立刻**落盘，同一问题再被问到时直接
    /// 回放，不再弹框。`None` = 不记账（顶层 turn / 独立测试）。
    ///
    /// 为什么必须有：答案此前只活在 subagent 的内存 context 里。step
    /// 内的契约重试是全新 subagent（无跨分派记忆），resume 也只跳过
    /// 已完成的 step —— 两条路都会把同一批问题重新弹给用户。
    /// design_and_plan 的 interview 步连问 3-4 题，只要该 step 后续任何
    /// 环节挂了，用户就得从第 1 题重答一遍。
    pub answer_log: Option<std::sync::Arc<crate::workflow::AnswerLog>>,
    /// 记账用的角色 id（提问方）。
    pub role_id: String,
    /// turn 取消旗标：等待期间被取消则工具报错，让 runner 尽快退出。
    pub cancel_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// session 级暂停门：等待期间用户按 ⏸ 则冻结等待，▶ 继续后恢复。
    pub agent_pause_gate: Option<std::sync::Arc<crate::pause_gate::AgentPauseGate>>,
}

/// 默认问询超时不再使用 —— ask 阻塞模式无限等待用户。保留此函数
/// 为兼容外部配置枚举（LATTE_ASK_TIMEOUT_SECS），但 `register_ask_tool`
/// 不再消费其值。
pub fn default_ask_timeout() -> std::time::Duration {
    let raw = std::env::var("LATTE_ASK_TIMEOUT_SECS").ok();
    match raw.and_then(|s| s.parse::<u64>().ok()) {
        Some(n) if n > 0 => std::time::Duration::from_secs(n),
        _ => std::time::Duration::from_secs(600),
    }
}

/// 删掉这次 ask 的跨进程落盘记录（见 [`crate::pending_ask`]）。///
/// 只在**答案已进 checkpoint**或**用户主动取消**时调用。刻意**不**放进
/// `ChoiceGuard::drop`：future 因进程退出而被 drop 时删掉记录，正好把
/// 唯一能救回这次提问的凭据毁掉。
fn clear_persisted_ask(answer_log: Option<&crate::workflow::AnswerLog>, choice_id: &str) {
    if let Some(log) = answer_log {
        if log.session_id().is_some() {
            crate::pending_ask::remove(log.cwd(), choice_id);
        }
    }
}


/// 注册 `ask` 工具：角色向用户抛出一道**选择题**（可带图片、图片
///
/// 两种语义（由 `blocking` 决定）：
/// - `None`（顶层 turn）：fire-and-forget。广播 `ChoiceRequested`
///   （`wait=false`）→ 返回"已展示选择题、请简短引导后结束本轮"的
///   指令串。用户在弹框里选完后，选择结果作为下一条 user 消息
///   （`/chat/send`）回喂角色。
/// - `Some(AskBlocking)`（workflow/delegate 子代理）：广播
///   `ChoiceRequested`（`wait=true`）→ 挂起等待，UI 把答案 POST 到
///   `/api/chat/choice-answer`（[`crate::choice`] 路由）后作为工具
///   结果返回，子代理拿着答案继续干活。取消有兜底。
/// 注册 `ask` 工具。`pub` 而非 `pub(crate)`：CLI REPL 也要注册它，
/// 否则同一个角色在 UI 里能弹选择题、在 `latte-agent chat` 里调 `ask`
/// 直接 ToolNotFound（两条路径行为不一致）。
pub fn register_ask_tool(
    tm: &Arc<dyn latte_rs_agent_tools::types::ToolManager>,
    event_tx: broadcast::Sender<ChatEvent>,
    role_id: String,
    blocking: Option<AskBlocking>,
    // fire-and-forget 分支的弹框快照落盘归属 `(cwd, session_id)`。
    // `Some` 时非阻塞 ask 也写进 `<cwd>/.latte/pending-asks/`，进程
    // 重启后仍能补发给用户（内存 `PROMPTS` 表重启即空）。
    // 阻塞分支不看这个参数——它走 `AskBlocking::answer_log` 那条
    // 带 wf_id 的落盘路径（可续跑）。
    persist: Option<(PathBuf, String)>,
) -> Result<(), Box<dyn std::error::Error>> {
    use latte_rs_agent_tools::types::{
        PropertyType, SchemaType, SharedToolHandler, Tool, ToolInputProperty, ToolInputSchema,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    // 进程级单调序号，保证 choice_id 全局唯一。
    static CHOICE_SEQ: AtomicU64 = AtomicU64::new(0);

    let input_schema = ToolInputSchema {
        schema_type: SchemaType,
        properties: vec![
            ("question".into(), ToolInputProperty {
                property_type: PropertyType::String,
                description: Some("要向用户提出的问题（一句话）。".into()),
                enum_values: None, minimum: None, maximum: None, min_length: None, max_length: None,
            }),
            ("options".into(), ToolInputProperty {
                property_type: PropertyType::Array,
                description: Some(
                    "候选项数组，2-6 项。每项是对象：{label(必填,简短标签), description(可选,一行取舍说明), pros(可选,字符串数组,该方案的优点), cons(可选,字符串数组,该方案的缺点/风险), details(可选,补充说明长文本), image(可选,配图URL,一般是 /api/images/<file>), recommended(可选,true 标记推荐项)}. 方案之间有取舍时**务必填 pros/cons**——UI 的「详情」按钮就是展开这两项给用户比较优缺点的。不要自己加“其他/Other”项——前端会自动附带“其他(自定义)”入口。".into()
                ),
                enum_values: None, minimum: None, maximum: None, min_length: None, max_length: None,
            }),
            ("multi".into(), ToolInputProperty {
                property_type: PropertyType::Boolean,
                description: Some("是否允许多选（默认 false = 单选）。问题本身允许同时选中多项（例如“启用哪些模块”“需要覆盖哪些场景”）时请显式传 true，UI 会渲染成复选框。⚠️ 只在 question 文案里写“（可多选）”是不够的——必须同时传 multi=true，否则 UI 渲染的是单选。".into()),
                enum_values: None, minimum: None, maximum: None, min_length: None, max_length: None,
            }),
            ("layout".into(), ToolInputProperty {
                property_type: PropertyType::String,
                description: Some("展示方式：\"list\"(默认) 或 \"grid\"(图片网格选择,适合每项都有 image 的视觉挑选)。".into()),
                enum_values: Some(vec!["list".into(), "grid".into()]),
                minimum: None, maximum: None, min_length: None, max_length: None,
            }),
            ("allow_upload".into(), ToolInputProperty {
                property_type: PropertyType::Boolean,
                description: Some("是否允许用户上传自己的图片作为答案（默认 false）。".into()),
                enum_values: None, minimum: None, maximum: None, min_length: None, max_length: None,
            }),
        ].into_iter().collect(),
        required: Some(vec!["question".into(), "options".into()]),
        ..Default::default()
    };

    let handler_role_id = role_id.clone();
    let handler: SharedToolHandler = Arc::new(move |input: serde_json::Value, _ctx| {
        let event_tx = event_tx.clone();
        let role_id = handler_role_id.clone();
        let blocking = blocking.clone();
        let persist = persist.clone();
        Box::pin(async move {
            let tool_err = |msg: String| latte_rs_agent_tools::error::ToolError::Other(msg);

            let question = input
                .get("question")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| tool_err("missing non-empty 'question' field".into()))?;

            // options 宽松取值：正常是裸数组，但模型也会包一层对象
            // （实测实锤：programmer 发的是 `{"item":[...]}`,
            // 直接 as_array() 拿不到 → 'options' must be an array,
            // 提问废掉）。包一层时取其中唯一的数组字段，不猜键名。
            let opts_owned;
            let opts_arr = match input.get("options") {
                Some(serde_json::Value::Array(a)) => a,
                Some(serde_json::Value::Object(map)) => {
                    let mut arrays = map.values().filter_map(|v| v.as_array());
                    match (arrays.next(), arrays.next()) {
                        (Some(a), None) => {
                            opts_owned = a.clone();
                            &opts_owned
                        }
                        _ => {
                            return Err(tool_err(
                                "'options' must be an array（收到对象且无法确定其中的候选项数组）"
                                    .into(),
                            ))
                        }
                    }
                }
                _ => return Err(tool_err("'options' must be an array".into())),
            };
            if opts_arr.len() < 2 {
                return Err(tool_err("'options' must have at least 2 entries".into()));
            }
            let mut options: Vec<ChoiceOption> = Vec::with_capacity(opts_arr.len());
            for (i, o) in opts_arr.iter().enumerate() {
                let opt: ChoiceOption = serde_json::from_value(o.clone())
                    .map_err(|e| tool_err(format!("options[{i}] invalid: {e}")))?;
                if opt.label.trim().is_empty() {
                    return Err(tool_err(format!("options[{i}].label must not be empty")));
                }
                options.push(opt);
            }

            // 宽松布尔 + 常见键名别名。此前用裸 `as_bool()`：字符串
            // `"false"` / `"true"` 一律 None → 静默退化成 false，
            // 模型想开多选却拿到单选，无任何报错线索
            // （实测实锤：programmer 发的是 `"multiSelect":"false"`）。
            let pick_bool = |keys: &[&str]| -> bool {
                keys.iter()
                    .filter_map(|k| input.get(*k))
                    .find_map(lenient_bool)
                    .unwrap_or(false)
            };
            // `multi` 双通道判定：显式参数 **或** question 文案。模型
            // 常常只在文案里写"（可多选）"而不传 `multi`（实测
            // 实锤：tutor 的「你最想深入哪条线？（可多选）」整个 args
            // 里没有 multi 键），缺省 false 让前端渲染成单选，用户看着
            // "可多选"只能点一个。两个信号任一为真就开多选——文案是
            // 用户唯一看得见的承诺，不能让它变哑。
            let multi = pick_bool(&["multi", "multiSelect", "multi_select", "multiple"])
                || question_implies_multi(&question);
            let allow_upload = pick_bool(&["allow_upload", "allowUpload"]);
            let layout = input
                .get("layout")
                .and_then(|v| v.as_str())
                .filter(|s| *s == "grid")
                .unwrap_or("")
                .to_string();

            // 回放：这个问题本 run（含 resume 继承）已经答过了 → 直接把
            // 旧答案当工具结果返回。必须在**发 ChoiceRequested 之前**
            // 拦下来，否则弹框已经推给前端，用户照样得再点一次。
            // 用户的回答是不可再生资源，step 重试 / resume 续跑不该让人
            // 重答一遍。发 Status 让回放对用户可见，而不是静默替他作答。
            if let Some(prev) = blocking
                .as_ref()
                .and_then(|blk| blk.answer_log.as_ref())
                .and_then(|log| log.recall(&question))
            {
                let _ = event_tx.send(ChatEvent::Status {
                    message: format!(
                        "↩ 复用你之前的回答（{role_id}）：「{}」→ {prev}",
                        question.chars().take(60).collect::<String>()
                    ),
                });
                return Ok(serde_json::Value::String(format!(
                    "用户此前已回答过这个问题「{question}」，选择：{prev}。请据此继续完成任务，不要重复提问同一个问题。"
                )));
            }

            let choice_id = format!("choice-{}-{}", role_id, CHOICE_SEQ.fetch_add(1, Ordering::Relaxed));
            let n = options.len();
            let requested = ChatEvent::ChoiceRequested {
                role_id: role_id.clone(),
                choice_id: choice_id.clone(),
                question: question.clone(),
                multi,
                layout,
                allow_upload,
                wait: blocking.is_some(),
                options,
            };
            let Some(blk) = blocking else {
                // fire-and-forget：没有等待方，直接推给前端即可。
                //
                // 但"推出去"不等于"到得了"：broadcast 没订阅者就丢弃，
                // 且 Lagged 掉的那几条既进不了 SSE 也进不了 archiver 的
                // event_log（连历史里都没有）。先登记一份未处理快照，
                // 让新连接 / 显式补拉把它重新弹出来；用户提交或跳过时
                // 前端调 `POST /api/chat/prompt-dismiss` 销账。
                // 登记必须早于广播：反序时用户秒答的 dismiss 会先落地，
                // 随后登记的快照成为僵尸，下次重连又弹一遍。
                crate::choice::register_prompt(
                    &choice_id,
                    requested.clone(),
                    event_tx.clone(),
                    persist.as_ref().map(|(c, s)| (c.as_path(), s.as_str())),
                );
                let _ = event_tx.send(requested);
                return Ok(serde_json::Value::String(format!(
                    "已向用户展示 {n} 个选项的选择框（choice_id={choice_id}）。请输出一句简短引导语（例如「请在上方选择」），然后结束本轮，不要调用其他工具，也不要臆测用户会选哪个——等待用户在弹框里选择后再继续。"
                )));
            };
            // 阻塞模式（workflow/delegate 子代理）：挂起等 UI 经
            // `/api/chat/choice-answer` 把答案送进 choice 路由；
            // 无限等待用户，取消则报错退出。暂停门在每次 select
            // 迭代前检查——用户按 ⏸ 时冻结等待，▶ 继续后恢复。
            //
            // **注册必须先于广播**：反过来的话，弹框已经到了前端而
            // PENDING 里还没有挂起项，用户秒答 → `resolve` 返回 false
            // → 404 → 前端降级成普通消息，而随后注册上的等待方再也
            // 收不到答案，这次 ask 无限挂起（超时路径已移除，只能等
            // 24h 或删 session）。
            //
            // 同时把弹框快照存进挂起项：broadcast 是"没订阅者就丢弃"
            // 的，发的这一刻用户可能没开着这个 session 的 SSE。新连接
            // 会经 `choice::pending_for_channel` 把它补发一遍。
            let rx = crate::choice::register(&choice_id, requested.clone(), event_tx.clone());
            // 跨进程落盘：内存表只能救"弹框事件丢了"，救不了"进程没了"。
            // 服务器一重启，这个 oneshot、正 park 的 workflow future、
            // 整个 runner 一起消失，答案再也没有接收方 —— 那一步永远不
            // 会继续。落一条 (session_id, wf_id, question) 记录，重启后
            // 用户回答就能补写进 checkpoint 并触发断点续跑
            // （见 crate::pending_ask）。只有 workflow run 的 ask 有
            // 这三元组；顶层 turn / CLI / 测试没有，跳过。
            if let Some(log) = blk.answer_log.as_ref() {
                if let Some(sid) = log.session_id() {
                    match crate::event_json::chat_event_to_frontend_json(&requested) {
                        Ok(json) => crate::pending_ask::persist(
                            log.cwd(),
                            &choice_id,
                            sid,
                            // 用**根** run 的 id，不是发出提问的那一层：
                            // 嵌套场景下只 resume 子 run 是不够的——父
                            // 流水线不知道自己在等谁，永远醒不过来
                            // （实测现场：requirements_review 的
                            // decide 弹窗答了也只能让子流程跑完，顶层
                            // design_and_plan 依旧卡死）。
                            log.resume_wf_id(),
                            &role_id,
                            &question,
                            &json,
                        ),
                        Err(e) => tracing::warn!(
                            choice_id = %choice_id,
                            error = %e,
                            "挂起 ask 快照序列化失败，跳过落盘（本进程内仍可回答）"
                        ),
                    }
                }
            }
            // 等待 future 被 drop（外层超时熔断 / workflow 中止）时自动
            // 清理 PENDING，防泄漏——否则 has_any_pending 永远为真，
            // advisor 被永久静音。
            let _choice_guard = crate::choice::ChoiceGuard(choice_id.clone());
            let _ = event_tx.send(requested);
            let cancel_watch = {
                let cf = blk.cancel_flag.clone();
                async move {
                    match cf {
                        Some(cf) => {
                            loop {
                                if cf.load(Ordering::SeqCst) {
                                    break;
                                }
                                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            }
                        }
                        None => std::future::pending::<()>().await,
                    }
                }
            };
            tokio::pin!(rx);
            tokio::pin!(cancel_watch);
            let answered = loop {
                // 每次 select 迭代前检查暂停门：按 ⏸ 时冻结等待，
                // ▶ 继续后恢复，不丢失选择框状态。
                if let Some(gate) = &blk.agent_pause_gate {
                    gate.wait_until_resumed(None).await;
                }
                tokio::select! {
                    a = &mut rx => break a.ok(),
                    _ = &mut cancel_watch => {
                        crate::choice::cancel(&choice_id);
                        // 用户主动掐了这一轮 —— 这是有意的放弃，落盘
                        // 记录也该走，否则重启后又弹一遍一个已被放弃的
                        // 问题。（进程被 kill 时 Drop 根本不跑，记录留在
                        // 盘上，这正是孤儿恢复要的。）
                        clear_persisted_ask(blk.answer_log.as_deref(), &choice_id);
                        return Err(tool_err(format!(
                            "等待用户回答期间 turn 被取消（choice_id={choice_id}）"
                        )));
                    }
                }
            };
            match answered {
                Some(answer) => {
                    // **收到即落盘**，在把结果交回模型之前。这一步之后
                    // 无论 step 怎么挂（契约不合格、空产出、advisor 拦、
                    // 预算超支、模型报错），这条回答都还在 checkpoint 里，
                    // 重试与 resume 都能回放。
                    if let Some(log) = &blk.answer_log {
                        log.record(&blk.role_id, &question, &answer);
                    }
                    // 答案已进 checkpoint → 挂起记录销账。
                    // 顺序要紧：先 record 再删。反过来的话，两步之间崩溃
                    // 会让答案和挂起记录一起消失，用户白答一次。
                    clear_persisted_ask(blk.answer_log.as_deref(), &choice_id);
                    Ok(serde_json::Value::String(format!(
                    "用户已回答你的问题「{question}」，选择：{answer}。请据此继续完成任务，不要重复提问同一个问题。"
                    )))
                }
                None => {
                    crate::choice::cancel(&choice_id);
                    Err(tool_err(format!(
                        "等待用户回答时通道意外关闭（choice_id={choice_id}）"
                    )))
                }
            }
        })
    });

    let tool = Tool::builder(
        "ask".to_string(),
        "向用户抛出一道选择题并弹出选择框（支持单选/多选、每项可带配图、图片网格挑选、允许上传自定义图片）。当存在多个取舍明显不同、需要用户拍板的方案时使用；不要用于可自行决定的琐碎问题。参数：question(问题) + options(候选项数组) + 可选 multi/layout/allow_upload。".to_string(),
        input_schema,
        handler,
    )
    // 见 ORCHESTRATION_TOOL_TIMEOUT_SECS：阻塞 ask 无限等用户作答，
    // 不被工具管理器 25min 默认熔断掐断。
    .timeout(std::time::Duration::from_secs(ORCHESTRATION_TOOL_TIMEOUT_SECS))
    .build();
    tm.register(tool, Some(&role_id));
    Ok(())
}

/// 注册 `task_report` 工具：角色向任务看板回报任务执行结果。任务看板
/// 通过 ui-server 侧 SSE 订阅识别 `ChatEvent::TaskReport` 事件
/// 调内部 `POST /api/tasks/:id/report` 完成状态推进（completed →
/// human_review，其他 → todo）。工具本身只做入参校验 + 广播事件，
/// 不调 HTTP——与 `plan`/`ask` 工具同款（ui-server 侧做事件→API 桥接）。
///
/// `result` 必须是 `completed` / `aborted` / `failed` / `timeout` 之一，
/// 与后端 `tasks::report_task` 的入参和 `tasks::RESULTS` 数组对齐。
/// `pub` 而非 `pub(crate)`：CLI REPL 也要注册它，否则声明了该工具的
/// 角色（manager）在 chat 里调用直接 ToolNotFound，而 UI 里正常。
pub fn register_task_report_tool(
    tm: &Arc<dyn latte_rs_agent_tools::types::ToolManager>,
    event_tx: broadcast::Sender<ChatEvent>,
    role_id: String,
) -> Result<(), Box<dyn std::error::Error>> {
    use latte_rs_agent_tools::types::{
        PropertyType, SchemaType, SharedToolHandler, Tool, ToolInputProperty, ToolInputSchema,
    };

    const VALID_RESULTS: [&str; 4] = ["completed", "aborted", "failed", "timeout"];

    let input_schema = ToolInputSchema {
        schema_type: SchemaType,
        properties: vec![
            ("task_id".into(), ToolInputProperty {
                property_type: PropertyType::String,
                description: Some(
                    "要回报的任务 ID（如 LAT-100）；来自任务看板派发时的初始消息。必填。".into(),
                ),
                enum_values: None, minimum: None, maximum: None, min_length: None, max_length: None,
            }),
            ("summary".into(), ToolInputProperty {
                property_type: PropertyType::String,
                description: Some(
                    "完成情况摘要（1-3 句中文，写到任务 history note）。".into(),
                ),
                enum_values: None, minimum: None, maximum: None, min_length: None, max_length: None,
            }),
            ("result".into(), ToolInputProperty {
                property_type: PropertyType::String,
                description: Some(
                    "执行结果枚举。completed → human_review，其他 → todo。".into(),
                ),
                enum_values: Some(VALID_RESULTS.iter().map(|s| serde_json::Value::String(s.to_string())).collect()),
                minimum: None, maximum: None, min_length: None, max_length: None,
            }),
        ].into_iter().collect(),
        required: Some(vec![
            "task_id".into(),
            "summary".into(),
            "result".into(),
        ]),
        ..Default::default()
    };

    let handler_role_id = role_id.clone();
    let handler: SharedToolHandler = Arc::new(move |input: serde_json::Value, _ctx| {
        let event_tx = event_tx.clone();
        let role_id = handler_role_id.clone();
        Box::pin(async move {
            let tool_err = |msg: String| latte_rs_agent_tools::error::ToolError::Other(msg);

            let task_id = input
                .get("task_id")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| tool_err("missing non-empty 'task_id' field".into()))?;

            let summary = input
                .get("summary")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();

            let result = input
                .get("result")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .filter(|s| VALID_RESULTS.contains(&s.as_str()))
                .ok_or_else(|| {
                    tool_err(format!(
                        "result must be one of {VALID_RESULTS:?}"
                    ))
                })?;

            let _ = event_tx.send(ChatEvent::TaskReport {
                role_id: role_id.clone(),
                task_id: task_id.clone(),
                summary: summary.clone(),
                result: result.clone(),
            });
            Ok(serde_json::Value::String(format!(
                "已向任务看板报告 {task_id}（{result}）。任务进入 {} 状态。",
                if result == "completed" { "human_review" } else { "todo" }
            )))
        })
    });

    let tool = Tool::builder(
        "task_report".to_string(),
        "向任务看板回报任务执行结果。在派发你执行的任务完成后调用，把任务推进到 human_review 状态。参数：task_id(任务ID, 必填) + summary(完成情况摘要) + result(completed/aborted/failed/timeout 之一)。只在确认任务执行完成时调用——不要重复调用，不要在用户问询阶段调用。".to_string(),
        input_schema,
        handler,
    )
    .build();

    tm.register(tool, Some(&role_id));
    Ok(())
}


/// Delegate-return gate: review a specialist's output before it flows
/// back to the manager, checking (1) role-responsibility adherence and
/// (2) whether the return actually answers the delegated task. Emits a
/// 🦉 advisor bubble on any non-`ok` verdict; on `intervene`/`terminate`
/// it also appends a review note to the payload the manager consumes.
/// Degrades silently (returns `response` unchanged) when the advisor is
/// unavailable or the review times out.
pub(crate) async fn gate_delegate_return(
    engine: &AdvisorReviewEngine,
    event_tx: &broadcast::Sender<ChatEvent>,
    // 用户在主会话里的原始诉求，作为「是否符合预期」的准绳。空串表示
    // 调用方拿不到（此时 prompt 会显式告知 advisor 不要据此下判）。
    main_topic: &str,
    role_id: &str,
    role_responsibilities: &str,
    task: &str,
    response: String,
    tool_call_summary: &str,
) -> (String, Option<crate::advisor_monitor::ReviewVerdict>) {
    use crate::advisor_monitor::Verdict;
    // Bound the review so a slow/absent advisor model can't stall the
    // delegate return (mirrors the monitor's REVIEW_TIMEOUT_SECS).
    // 预算可配（AdvisorMonitorConfig::review_settings）：实测实锤
    // 45s 硬编码对 20–44s 延迟的慢审查模型太紧，频繁未审放行。
    let timeout = engine.delegate_review_timeout();
    let review = tokio::time::timeout(
        timeout,
        engine.review_delegate(
            main_topic,
            role_id,
            role_responsibilities,
            task,
            &response,
            tool_call_summary,
        ),
    )
    .await;
    let verdict = match review {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            tracing::warn!("delegate-return review failed (degraded): {e}");
            // 降级可见化：审查失败静默放行会让 advisor 看起来「卡住/
            // 消失」，发 Status 让用户知道这次返回没审。
            let _ = event_tx.send(ChatEvent::Status {
                message: format!("⚠️ advisor 审查失败（{role_id} 的返回降级放行）：{e}"),
            });
            return (response, None);
        }
        Err(_) => {
            tracing::warn!("delegate-return review timed out; passing through");
            let _ = event_tx.send(ChatEvent::Status {
                message: format!(
                    "⏳ advisor 审查超时（{}s）：{role_id} 的返回未审直接放行",
                    timeout.as_secs()
                ),
            });
            return (response, None);
        }
    };
    if verdict.verdict == Verdict::Ok {
        return (response, Some(verdict));
    }
    let label = match verdict.verdict {
        Verdict::Warn => "⚠️ warn",
        _ => "🛑 intervene",
    };
    let reason = if verdict.reason.is_empty() {
        "疑似偏离职责或未完全达成委派任务".to_string()
    } else {
        verdict.reason.clone()
    };
    let mut bubble = format!("{label}（{role_id} 委派返回审查）：{reason}");
    if !verdict.hint.is_empty() {
        bubble.push_str(&format!("\n\n> 建议：{}", verdict.hint));
    }
    let _ = event_tx.send(ChatEvent::RoleTurn {
        role_id: "advisor".to_string(),
        content: bubble,
        is_complete: true,
        sub_id: None,
    });
    // `warn` is human-visible only (bubble); `intervene`/`terminate`
    // also annotate the payload so the manager sees the caveat inline.
    let out = if matches!(verdict.verdict, Verdict::Intervene | Verdict::Terminate) {
        let hint_line = if verdict.hint.is_empty() {
            String::new()
        } else {
            format!("\n处理建议：{}", verdict.hint)
        };
        format!(
            "{response}\n\n---\n⚠️ [监察审查] 本返回可能偏离「{role_id}」职责或未完全达成委派任务：{reason}{hint_line}"
        )
    } else {
        response
    };
    (out, Some(verdict))
}

/// UI/controller 路径 delegate 的默认 wall-clock 超时（秒）。
/// 比 CLI 的 300s（`DEFAULT_DELEGATE_TIMEOUT_SECS`）宽：UI 工作流里
/// specialist 常写整套文档/脚本。解析顺序对齐 CLI（chat.rs）：
/// `model.timeout_secs` > env `LATTE_AGENT_DELEGATE_TIMEOUT_SECS` > 本值。
pub(crate) const DEFAULT_UI_DELEGATE_TIMEOUT_SECS: u64 = 900;

/// 编排类工具（`workflow` / `delegate` / 阻塞 `ask`）的单次调用超时。
///
/// 这类工具的一次调用里包着**整条子流水线或一次人机交互**，时长不由
/// 本进程决定：`workflow` 要跑完嵌套子 workflow + 并行评审波（实测
/// design_and_plan ~48min），阻塞 `ask` 要等用户在弹框里作答（可能几
/// 小时）。工具管理器 25min 默认熔断对它们只会误杀，所以统一放到 24h
/// —— 相当于"不靠这层兜底"。
///
/// 真正的控制手段是：
/// - **单次模型调用的 TTFB / idle 流式超时**（`agent.rs`）—— 后端零
///   字节响应时秒级发现，冷却后沿模型链换下一个模型，全链都哑才升级
///   为 `ModelsUnavailable → 自动暂停等用户`。这才是 `workflow` /
///   `delegate` 路径真正的防挂死手段：它按「活性」而非「总时长」判定，
///   不会误杀持续在产出的健康长任务。本常量只是把工具管理器那一层
///   让开，避免两层超时倒挂（工具层比引擎层更紧 → 引擎还在跑，工具
///   已经熔断 → 孤儿 step）。
///   （历史：曾在引擎里加过一层「按分派单元数推算的 wall-clock 规模
///   预算」，但它只看总时长、无法区分「卡死」与「任务就是重」，反复
///   误杀 explore 这类合法长任务，已移除。）
/// - per-step wall-clock 触发的 `TimeoutWarning`（周期复发，让用户拍板）、
///   `turn_cancel_flag`（用户「终止当前任务」）、session `cancel_flag`
///   （全量中止），以及 `run_turn` 内部的死循环熔断。
///
/// 实测实锤：`workflow` 漏配本超时 → manager 调用在第 25 分钟被
/// 熔断，而 step 因已无 wall-clock 硬超时仍在后台跑（孤儿任务）；
/// manager 依错误提示 resume 又是 25min，两次空等 50 分钟零产出。
pub const ORCHESTRATION_TOOL_TIMEOUT_SECS: u64 = 86_400;

/// Specialist 的 wall-clock 超时（秒）。`model_timeout` 是模型目录里
/// 的 per-model `timeout_secs`（workflow 路径拿不到模型 id，传 None）。
pub(crate) fn specialist_timeout_secs(model_timeout: Option<u64>) -> u64 {
    model_timeout
        .or_else(|| {
            std::env::var("LATTE_AGENT_DELEGATE_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(DEFAULT_UI_DELEGATE_TIMEOUT_SECS)
}

async fn register_delegate_tool(
    tm: &Arc<dyn latte_rs_agent_tools::types::ToolManager>,
    merged: &AgentConfig,
    resolver: &ModelResolver,
    default_params: GenerateParams,
    event_tx: broadcast::Sender<ChatEvent>,
    cwd: PathBuf,
    subsession_store: Arc<SubsessionStore>,
    session_id: String,
    cancel_flag: Arc<AtomicBool>,
    turn_cancel_flag: Arc<AtomicBool>,
    // Advisor pre-persistence gate，透传自 build_runner；`Some` 时
    // specialist runner 的产出也先过 D5/D6 gate 再返回给 manager。
    advisor_gate: Option<GateConfig>,
    // plan 阶段门共享句柄：`PendingApproval` 时拒绝派发实现类角色。
    plan_stage: SharedPlanStage,
    // Session-level 暂停门 —— specialist runner 也装上，用户按 ⏸
    // 时 subagent 一起冻结。
    agent_pause_gate: std::sync::Arc<crate::pause_gate::AgentPauseGate>,
    // 单 session 内 delegate 工具调用累计计数器（与 session 同生命周期）。
    // 0 = 禁用限制（向后兼容）。`max_delegates == 0` 时 handler 跳过计数。
    delegate_counter: Arc<AtomicU32>,
    // 单 session 累计 delegate 工具调用上限。`register_delegate_tool` 在
    // 每次 invocation 入口 fetch_add 后检查；超限返回 ClientError。
    // 0 = 禁用（保留字段供 future 配置）。
    max_delegates: u32,
    // 用户在主会话里的原始诉求（与 ChatController 共享同一句柄）。
    // 委派返回审查拿它当「是否符合预期」的准绳。注册时取不到值——它每轮
    // 都变，所以传句柄、在 handler 里按次读取。
    main_topic: Arc<parking_lot::Mutex<String>>,
) -> Result<(), Box<dyn std::error::Error>> {
    use latte_rs_agent_tools::types::{SchemaType, SharedToolHandler, Tool};
    use tokio::sync::Semaphore;

    // 注册时把 merged.roles 的角色花名册写进 description/schema ——
    // 角色编辑器新建/改名的角色对模型立刻可见（与系统提示同源：
    // role_roster_text）。
    let roster = role_roster_text(merged);

    let input_schema = latte_rs_agent_tools::types::ToolInputSchema {
        schema_type: SchemaType,
        properties: vec![
            ("role".into(), ToolInputProperty {
                property_type: PropertyType::String,
                description: Some(format!("Specialist role id. Available roles: {roster}")),
                enum_values: None,
                minimum: None,
                maximum: None,
                min_length: None,
                max_length: None,
            }),
            ("task".into(), ToolInputProperty {
                property_type: PropertyType::String,
                description: Some("Natural-language task for the specialist".into()),
                enum_values: None,
                minimum: None,
                maximum: None,
                min_length: None,
                max_length: None,
            }),
        ]
        .into_iter()
        .collect(),
        required: Some(vec!["role".into(), "task".into()]),
        ..Default::default()
    };

    let sem = Arc::new(Semaphore::new(8));
    let merged_owned = Arc::new(merged.clone());
    let resolver_owned = Arc::new(resolver.clone());
    // Delegate-return gate: an advisor review engine that inspects each
    // specialist's output before it flows back to the manager (role
    // adherence + task-result relevance). Built once; cloned per call.
    let review_engine = AdvisorReviewEngine::new(
        merged_owned.clone(),
        resolver_owned.clone(),
        default_params.clone(),
    )
    .with_review_settings(
        advisor_gate
            .as_ref()
            .map(|g| g.review_settings)
            .unwrap_or_default(),
    );
    // 审查的 LLM 调用也落 subsession trace（与 ui-server 的 monitor
    // 同款）：此前 delegate-return 审查每次 33–45s 却不进任何日志，
    // 是观测盲区。
    let review_engine = if session_id.is_empty() {
        review_engine
    } else {
        let (_sub_id, sink) = subsession_store.create(&session_id, "advisor");
        review_engine.with_subsession_sink(sink)
    };
    let review_engine = Arc::new(review_engine);
    let cancel_flag_owned = Arc::clone(&cancel_flag);
    let turn_cancel_flag_owned = turn_cancel_flag.clone();
    let delegate_counter_owned = Arc::clone(&delegate_counter);
    let max_delegates_owned = max_delegates;
    // 派发幂等台账（见 crate::dispatch_ledger）：同一 (role, task) 正在
    // 跑就拒、已成功就复用上次结果。与 delegate_counter 同生命周期。
    let ledger = crate::dispatch_ledger::DispatchLedger::new();
    let handler: SharedToolHandler = Arc::new(move |input: serde_json::Value, _ctx| {
        let merged = Arc::clone(&merged_owned);
        let resolver = Arc::clone(&resolver_owned);
        let review_engine = Arc::clone(&review_engine);
        let default_params = default_params.clone();
        let event_tx = event_tx.clone();
        let cwd = cwd.clone();
        let sem = Arc::clone(&sem);
        let cancel_flag = Arc::clone(&cancel_flag_owned);
        let turn_cancel_flag = Arc::clone(&turn_cancel_flag_owned);
        let advisor_gate = advisor_gate.clone();
        let subsession_store = subsession_store.clone();
        let session_id = session_id.clone();
        let plan_stage = plan_stage.clone();
        let delegate_counter = Arc::clone(&delegate_counter_owned);
        let max_delegates = max_delegates_owned;
        let ledger = ledger.clone();
        // 每次 specialist 创建时 attach。
        let agent_pause_gate = agent_pause_gate.clone();
        // 主会话诉求句柄：每次 invocation 现读（它每轮都变）。
        let main_topic = Arc::clone(&main_topic);
        Box::pin(async move {
            let tool_err = |msg: String| latte_rs_agent_tools::error::ToolError::Other(msg);

            let role_id = input
                .get("role")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tool_err("missing 'role' field".into()))?
                .to_string();
            let task = input
                .get("task")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tool_err("missing 'task' field".into()))?
                .to_string();

            // plan 阶段门：任务清单已提交但未获用户批准时，禁止派发
            // 实现类角色（programmer*/devops*）；分析/设计/审查类放行。
            // 在分配 subsession / 发 DelegateStarted 之前拦截，拒绝
            // 不留任何副作用。
            if let Some(msg) = plan_gate_rejection(&plan_stage, &role_id) {
                return Err(tool_err(msg));
            }

            // 派发幂等：同身份（role + task）的派发不做第二遍。
            // 位置在**计数之前**：重复派发不该消耗 max_delegates 预算。
            // 也在分配 subsession / 发 DelegateStarted 之前——拒绝不留
            // 任何副作用（否则 UI 上留下一个永远转圈的分派气泡）。
            let dispatch_key =
                crate::dispatch_ledger::DispatchLedger::delegate_key(&role_id, &task);
            let inflight_guard = match ledger.begin(dispatch_key) {
                crate::dispatch_ledger::Begin::Fresh(g) => g,
                crate::dispatch_ledger::Begin::InFlight => {
                    return Err(tool_err(format!(
                        "对 '{role_id}' 的这条委派**正在执行中**（同一 role + task）。\
                         不要重复派发：等它返回即可。若确实需要不同的工作，请修改 task 描述。"
                    )));
                }
                crate::dispatch_ledger::Begin::Done(prev) => {
                    // 已经干过一遍且成功了 —— 直接复用，省掉整次
                    // specialist 往返（几十秒到十几分钟）与重复副作用。
                    let _ = event_tx.send(ChatEvent::Status {
                        message: format!(
                            "↩ 复用已完成的委派结果（{role_id}）：同一 role + task 不重复派发"
                        ),
                    });
                    return Ok(serde_json::Value::String(prev));
                }
            };

            // 单 session 内 delegate 计数检查：超过 max_delegates 直接拒。
            // `max_delegates == 0` 时关闭此功能（向后兼容）。fetch_add
            // 返回旧值；新值 > limit 即超限。
            if max_delegates > 0 {
                let used = delegate_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if used > max_delegates {
                    return Err(tool_err(format!(
                        "本 session 已用尽 {max_delegates} 次 delegate（当前 {used}）—— 请综合上述 specialist 结论直接回复用户，不要再派发"
                    )));
                }
            }
            // 不留任何副作用。
            if let Some(msg) = plan_gate_rejection(&plan_stage, &role_id) {
                return Err(tool_err(msg));
            }

            // 先解析角色与模型链，成功后再分配 subsession / 发
            // DelegateStarted——这些 `?` 失败路径若发生在
            // DelegateStarted 之后，会留下永远「⏳ 执行中…」的分派
            // 气泡（没有配对的 DelegateFinished）。
            let template = merged
                .roles
                .get(&role_id)
                .ok_or_else(|| {
                    tool_err(format!(
                        "role '{}' not found in config. available roles: {}",
                        role_id,
                        role_roster_text(&merged)
                    ))
                })?
                .clone();
            let role = template.resolve(&default_params).await.map_err(|e| {
                tool_err(format!("failed to resolve role '{}': {}", role_id, e))
            })?;
            // 角色模型指派优先取 resolver 最新快照：角色编辑器保存后
            // 新分派立即生效（merged 是 session 创建时的固化快照）。
            let (chain_ids, tier) = match resolver.role_model_assignment(&role.id) {
                Some((chain, t)) => (
                    if chain.is_empty() {
                        role.model_chain.clone()
                    } else {
                        chain
                    },
                    t.unwrap_or(role.default_model_tier),
                ),
                None => (role.model_chain.clone(), role.default_model_tier),
            };
            let models = resolver
                .resolve_chain(&role.id, tier, &chain_ids)
                .map_err(|e| {
                    tool_err(format!("no model for role '{}': {}", role_id, e))
                })?;
            // 熔断：delegate wall-clock 超时。解析顺序对齐 CLI
            // （chat.rs）：model.timeout_secs > env > UI 默认 900s。
            // 实测事故：无超时 → estimate 步骤的 programmer
            // 盲改循环 24min，父 workflow 被无限期吊住。
            let timeout_s = specialist_timeout_secs(
                resolver.get_def(&models[0].id).and_then(|d| d.timeout_secs),
            );

            // 2. Allocate a subsession so the specialist's full event
            //    log is captured. The main chat SSE stream sees only
            //    DelegateStarted/Finished (the summary); the UI can
            //    right-click on those events and fetch the
            //    transcript via /api/sessions/.../subsessions/{sub_id}.
            let (sub_id, sub_sink) =
                subsession_store.create(session_id.as_str(), &role_id);
            let _ = event_tx.send(ChatEvent::DelegateStarted {
                from_role: "manager".into(),
                to_role: role_id.clone(),
                task: task.clone(),
                sub_id: sub_id.clone(),
                wf_id: None,
            });
            // per-subsession 取消登记：UI 右键该分派 →「终止此分派」时
            // 只掐这一条（manager 并行委派多个 specialist 时，不必为了
            // 停一个而 cancel 整轮）。guard 随 handler 退出自动摘除。
            let sub_cancel_flag = crate::sub_cancel::register(&sub_id);
            let _sub_cancel_guard = crate::sub_cancel::SubCancelGuard(sub_id.clone());
            // Fan out ONLY to the subsession memory sink: the
            // `sub_sink` is `Arc<dyn TraceSink>` returning from
            // `SubsessionStore::create` —— 内部已 fanout 到
            // MemorySink（实时读源）+ DiskSink（落盘备份，主
            // session 删除时联删）。这里**不要**再包一层
            // FanoutSink，否则事件会被写两份到内存。

            // 4. Build the specialist runner. We give it a tool
            //    manager iff the role's `allowed_tools` is non-empty;
            //    otherwise the agent answers "I have no file access"
            //    (the bug we hit when the manager delegated but the
            //    programmer refused to read any code).
            let specialist_tm = if role.allowed_tools.is_empty() {
                None
            } else {
                match build_tool_manager(&role.allowed_tools).await {
                    Ok(tm) => Some(tm),
                    Err(e) => {
                        let summary = format!("tool setup for '{}' failed: {}", role_id, e);
                        let _ = event_tx.send(ChatEvent::DelegateFinished {
                            from_role: "manager".into(),
                            to_role: role_id.clone(),
                            status: "failed".into(),
                            summary: summary.clone(),
                            sub_id: sub_id.clone(),
                            wf_id: None,
                        });
                        return Err(tool_err(summary));
                    }
                }
            };
            let agent = Agent::new_with_chain(
                role_id.clone(),
                role.clone(),
                models,
                default_params.clone(),
            )
            .map_err(|e| {
                let summary = format!("failed to create agent for '{}': {}", role_id, e);
                let _ = event_tx.send(ChatEvent::DelegateFinished {
                    from_role: "manager".into(),
                    to_role: role_id.clone(),
                    status: "failed".into(),
                    summary: summary.clone(),
                    sub_id: sub_id.clone(),
                    wf_id: None,
                });
                tool_err(summary)
            })?;
            // 工具轮次上限兜底盲改/跑偏循环（实测事故前写死
            // 0 = 无限，靠 LoopDetector 抓「连续 3 次相同调用」，
            // 对换着参数重写的循环无效）。正常任务远低于默认值，
            // 死循环熔断冒泡 ToolLoopDetected（带 partial）→ 回喂 manager 重派。
            // Wire the workspace cwd so the specialist's path-aware
            // tools (`bash`, `read`, …) chdir into the
            // workspace the user opened — not the Tauri process
            // cwd. See `AgentRunner::with_cwd` for the contract.
            let mut runner = match specialist_tm {
                Some(tm) => AgentRunner::new_with_tools(agent, tm),
                None => AgentRunner::new(agent),
            };
            // Fan-out：子会话 JSONL 日志 + ChatEventTraceSink——专家的
            // 工具错误由此广播到 session channel，advisor monitor 的
            // specialist-error 检测（连续出错 → 提示修正分派）依赖它。
            let specialist_sink: Arc<dyn crate::trace::TraceSink> =
                Arc::new(crate::trace::FanOutSink::new(vec![
                    sub_sink.clone(),
                    Arc::new(ChatEventTraceSink {
                        event_tx: event_tx.clone(),
                        sub_id: Some(sub_id.clone()),
                    }),
                ]));
            runner = runner
                .with_role(role_id.clone())
                .with_cwd(cwd.clone())
                .with_sink(specialist_sink);
            if let Some(gate) = advisor_gate.clone() {
                runner = runner.with_gate_config(gate);
            }
            // Session-level 暂停门：subagent 也共享 —— 用户按 ⏸ 时
            // specialist 在下一个 turn/tool 边界一起 park。
            runner = runner.with_agent_pause_gate(agent_pause_gate.clone());
            // 模型热更新：与主 runner / workflow step runner 对齐——
            // 「模型不可用暂停 → 用户 ▶ 恢复」重试前按 resolver 最新
            // 代际重建 model chain，用户在 UI 改的指派即刻生效。
            runner = runner.with_model_hot_reload(
                resolver.clone(),
                tier,
                chain_ids.clone(),
            );

            // Emit RoleStarted so the UI shows the specialist is working
            let task_clone = task.clone();
            let role_id_clone = role_id.clone();
            let event_tx_clone = event_tx.clone();
            let _ = event_tx_clone.send(ChatEvent::RoleStarted {
                role_id: role_id_clone.clone(),
                detail: format!("delegated: {}", task_clone),
                sub_id: Some(sub_id.clone()),
            });

            // 5. Run the specialist with a wall-clock timeout
            //    (`timeout_s`, resolved above) plus 500ms cancel_flag
            //    polling. 实测事故前无超时：子代理永不结束时
            //    父方永远等待。
            let _permit = sem.acquire().await.map_err(|_| {
                tool_err("delegate pool shut down".into())
            })?;
            // Move runner + messages into a spawned task so we can
            // cancel it from the select! loop. The task owns everything.
            let task_content = task.clone();
            // 循环内 deadline（对齐 oh-my-pi）：让 agent 在临近超时时
            // 优雅退出返回 partial，而不是被 tokio timeout abort 丢失产出。
            runner.set_deadline(std::time::Instant::now() + std::time::Duration::from_secs(timeout_s));
            let mut run_handle = tokio::spawn(async move {
                // gated 版：装了 gate_config 时产出先过 D5/D6；
                // 没装时等价 run_turn。
                runner.run_turn_gated(&[Message::user(task_content)], None).await
            });
            // abort-on-drop：**必须**有。下面的 select! 会在超时/取消
            // 分支里显式 abort，但那是这个 handler future 还在被 poll
            // 的前提下。驱动侧 `run_turn_cancellable` 同样是 500ms 轮询，
            // 它先赢时会直接 drop 整个 run_turn future → 这个 handler
            // future 被丢弃 → 走不到任何 abort 分支，而裸 JoinHandle 被
            // drop **不会**取消任务（tokio 语义）。后果：specialist 脱管
            // 继续真实写文件/跑 bash，事件仍打进共享 event_tx 污染下一
            // 个 turn，气泡永远停在「运行中」（无配对 DelegateFinished），
            // 而 SubCancelGuard 已析构 → 用户再也取消不了它。
            // 两个 500ms 轮询谁先赢是竞态，不能靠"通常是 select 先赢"。
            let _abort_on_drop = crate::sub_cancel::AbortOnDrop(run_handle.abort_handle());
            // 超时被 500ms cancel 轮询每次 select! 重建会永远不响，
            // 必须在循环外 pin 住。
            let timeout = tokio::time::sleep(std::time::Duration::from_secs(timeout_s));
            tokio::pin!(timeout);
            let result: Result<String, latte_rs_agent_tools::error::ToolError>;
            loop {
                tokio::select! {
                    r = &mut run_handle => {
                        match r {
                            // 剥掉 <think> 推理块：主 session 的气泡、
                            // DelegateFinished 摘要和回喂 manager 的工具
                            // 结果都只保留正式回答；原文留在子会话 trace。
                            Ok(Ok(response)) => {
                                // 空产出不算成功：判失败回喂 manager，
                                // 让它重派或换角色，而不是把空串当结论。
                                let stripped = strip_think_blocks(&response);
                                if is_empty_output(&stripped) {
                                    result = Err(tool_err(format!(
                                        "subagent '{role_id}' 返回了空内容"
                                    )));
                                } else {
                                    result = Ok(stripped);
                                }
                                break;
                            }
                            Ok(Err(e)) => {
                                // 死循环熔断但已有实质产出：把正文一并回喂
                                // manager。只报一句 "tool loop" 会让 manager
                                // 以为这一支彻底没成果，从而原样重派——同一
                                // 个坑再烧一遍预算。带上 partial，manager 才
                                // 能判断是「接着收尾」还是「缩小范围重派」。
                                if let AgentError::ToolLoopDetected { tool, partial, .. } = &e {
                                    let stripped = strip_think_blocks(partial);
                                    if !is_empty_output(&stripped) {
                                        result = Err(tool_err(format!(
                                            "subagent failed: 检测到工具死循环（'{tool}' 反复调用）。\
                                             以下是它被中止前已产出的未收尾内容，请据此决定\
                                             「缩小范围重派」还是「让它接着收尾」，不要原样重派：\n\n{stripped}"
                                        )));
                                        break;
                                    }
                                }
                                // Gate 重试耗尽 → subsession 被 advisor
                                // 终止：发 AdvisorTerminated（带 sub_id）
                                // 让 UI 显示"已暂停"状态，再把错误
                                // 作为 tool error 回喂 manager。
                                if let AgentError::AdvisorTerminated { reason, detector } = &e {
                                    let _ = event_tx.send(ChatEvent::AdvisorTerminated {
                                        role_id: role_id.clone(),
                                        reason: reason.clone(),
                                        detector: Some(detector.clone()),
                                        sub_id: Some(sub_id.clone()),
                                    });
                                }
                                result = Err(tool_err(format!("subagent failed: {e}")));
                                break;
                            }
                            Err(e) => {
                                result = Err(tool_err(format!("task join failed: {e}")));
                                break;
                            }
                        }
                    }
                    _ = &mut timeout => {
                        run_handle.abort();
                        let _ = event_tx.send(ChatEvent::RoleFinished {
                            role_id: role_id.clone(),
                            detail: format!("timed out after {timeout_s}s"),
                            sub_id: Some(sub_id.clone()),
                        });
                        let summary = format!(
                            "delegate '{role_id}' 超过 {timeout_s}s 未返回，已中止；\
                             请缩小任务范围或拆步后重派（env LATTE_AGENT_DELEGATE_TIMEOUT_SECS 可调）"
                        );
                        let _ = event_tx.send(ChatEvent::DelegateFinished {
                            from_role: "manager".into(),
                            to_role: role_id.clone(),
                            status: "timeout".into(),
                            summary: summary.clone(),
                            sub_id: sub_id.clone(),
                            wf_id: None,
                        });
                        return Err(tool_err(summary));
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                        if cancel_flag.load(Ordering::SeqCst) {
                            run_handle.abort();
                            let _ = event_tx.send(ChatEvent::RoleFinished {
                                role_id: role_id.clone(),
                                detail: "cancelled by user".into(),
                                sub_id: Some(sub_id.clone()),
                            });
                            let summary = String::from("delegate cancelled by user");
                            let _ = event_tx.send(ChatEvent::DelegateFinished {
                                from_role: "manager".into(),
                                to_role: role_id.clone(),
                                status: "cancelled".into(),
                                summary: summary.clone(),
                                sub_id: sub_id.clone(),
                                wf_id: None,
                            });
                            return Err(tool_err(summary));
                        }
                        if turn_cancel_flag.load(Ordering::SeqCst) {
                            run_handle.abort();
                            let _ = event_tx.send(ChatEvent::RoleFinished {
                                role_id: role_id.clone(),
                                detail: "cancelled by user".into(),
                                sub_id: Some(sub_id.clone()),
                            });
                            let summary = String::from("delegate cancelled by user");
                            let _ = event_tx.send(ChatEvent::DelegateFinished {
                                from_role: "manager".into(),
                                to_role: role_id.clone(),
                                status: "cancelled".into(),
                                summary: summary.clone(),
                                sub_id: sub_id.clone(),
                                wf_id: None,
                            });
                            return Err(tool_err(summary));
                        }
                        // per-subsession 取消：只结束这一条委派，manager
                        // 的其它并行委派继续跑。错误文案要让 manager 知道
                        // 是"人为终止"而不是失败——否则它会自动重派，把
                        // 用户刚掐掉的活again 起一遍。
                        if sub_cancel_flag.load(Ordering::SeqCst) {
                            run_handle.abort();
                            let _ = event_tx.send(ChatEvent::RoleFinished {
                                role_id: role_id.clone(),
                                detail: "terminated by user (subsession)".into(),
                                sub_id: Some(sub_id.clone()),
                            });
                            let summary = format!(
                                "委派 '{role_id}' 被用户手动终止（从 subsession 右键）。\
                                 这是用户的明确意图，**不要**自动重派同一任务；\
                                 如需继续请先向用户确认。"
                            );
                            let _ = event_tx.send(ChatEvent::DelegateFinished {
                                from_role: "manager".into(),
                                to_role: role_id.clone(),
                                status: "cancelled".into(),
                                summary: summary.clone(),
                                sub_id: sub_id.clone(),
                                wf_id: None,
                            });
                            return Err(tool_err(summary));
                        }
                    }
                }
            }
            // ── Delegate-return gate ──
            // Before the specialist's result flows back to the manager,
            // run an advisor review (role adherence + does it answer the
            // task). On `intervene` the returned payload is annotated
            // with a review note so the manager sees the caveat; a 🦉
            // bubble is emitted for the human either way. Degrades
            // silently (returns the output unchanged) if the advisor is
            // unavailable or times out.
            // 先落成 String：parking_lot 的 guard 不是 Send，跨 await 持锁
            // 会让整个 handler future 失去 Send。
            let topic_snapshot: String = main_topic.lock().clone();
            let run_result = match result {
                // manager delegate 路径保持「只批注不重做」——重做决策
                // 是 manager 自己的事（它看得到批注，可自行再委派）。
                // advisor 总开关关闭（advisor_gate None）时跳过审查。
                Ok(response) if advisor_gate.is_some() => Ok(gate_delegate_return(
                    &review_engine,
                    &event_tx,
                    &topic_snapshot,
                    &role_id,
                    &role.system_prompt,
                    &task,
                    response,
                    "", // manager delegate 路径，调用方无工具计数
                )
                .await
                .0),
                other => other,
            };

            // 6. Emit specialist's RoleTurn + RoleFinished so the UI
            //    can show the subagent reply with a reference back to
            //    the DelegateStarted message (@role task).
            match &run_result {
                Ok(response) => {
                    // 成功时 sub_sink 已由 runner 的 with_sink 写入全部事件，
                    // 不再重复 emit RoleFinished —— 需要的话在 TurnEnd 里已有足够信息。
                    let _ = event_tx.send(ChatEvent::RoleTurn {
                        role_id: role_id.clone(),
                        content: response.clone(),
                        is_complete: true,
                        sub_id: Some(sub_id.clone()),
                    });
                    let _ = event_tx.send(ChatEvent::RoleFinished {
                        role_id: role_id.clone(),
                        detail: format!("ok, {} chars", response.len()),
                        sub_id: Some(sub_id.clone()),
                    });
                }
                Err(e) => {
                    // 失败时 sub_sink 中的 trace 在 TurnEnd 前就断了（agent.chat?.await
                    // 提前返回），导致右键「查看日志」看不到失败原因。
                    // 这里写入 TurnEnd 作为 trace 终点事件，让子会话日志有明确结尾。
                    sub_sink.emit(crate::trace::TraceEvent::TurnEnd {
                        meta: crate::trace::TraceMeta::now(0, &role_id, &session_id),
                        total_input: 0,
                        total_output: 0,
                        total_thinking: 0,
                        elapsed_ms: 0,
                    });
                    let _ = event_tx.send(ChatEvent::RoleFinished {
                        role_id: role_id.clone(),
                        detail: format!("error: {}", e),
                        sub_id: Some(sub_id.clone()),
                    });
                    let _ = event_tx.send(ChatEvent::Error {
                        kind: Some(crate::trace::ModelErrorKind::Other {
                            message: format!("delegate {role_id} failed: {e}"),
                        }),
                        message: format!("delegate {role_id} failed: {e}"),
                        sub_id: Some(sub_id.clone()),
                    });
                }
            }

            // 7. Emit DelegateFinished and return result.
            match run_result {
                Ok(response) => {
                    let _ = event_tx.send(ChatEvent::DelegateFinished {
                        from_role: "manager".into(),
                        to_role: role_id.clone(),
                        status: "ok".into(),
                        summary: response.clone(),
                        sub_id: sub_id.clone(),
                        wf_id: None,
                    });
                    // 只缓存成功：失败可能是瞬时的（模型不可用），换个
                    // 时机重试是合理的，重复失败由 max_delegates 与
                    // supervisor 兜。guard 未 complete 就 drop → 在飞
                    // 标记自动摘除，同身份可重试。
                    inflight_guard.complete(response.clone());
                    Ok(serde_json::Value::String(response))
                }
                Err(e) => {
                    let summary = format!("delegate to '{}' failed: {}", role_id, e);
                    let _ = event_tx.send(ChatEvent::DelegateFinished {
                        from_role: "manager".into(),
                        to_role: role_id.clone(),
                        status: "failed".into(),
                        summary: summary.clone(),
                        sub_id: sub_id.clone(),
                        wf_id: None,
                    });
                    Err(tool_err(summary))
                }
            }
        })
    });


    let tool = Tool::builder(
        "delegate".to_string(),
        format!("Delegate a subtask to a specialist agent. Available roles: {roster}"),
        input_schema,
        handler,
    )
    // 见 ORCHESTRATION_TOOL_TIMEOUT_SECS：specialist 委派自带
    // wall-clock 超时（specialist_timeout_secs）与取消旗标，不需要
    // 工具管理器 25min 熔断再插一刀（子代理写整套文档/跑构建时会
    // 正常超过）。
    .timeout(std::time::Duration::from_secs(ORCHESTRATION_TOOL_TIMEOUT_SECS))
    .build();

    tm.register(tool, Some("manager"));
    Ok(())
}

/// Register the `workflow` tool: the manager can trigger a named
/// multi-role workflow (设计/plan/TDD/文档/图谱…). The handler is a thin
/// shell: parse name/topic → `load_workflow` → [`crate::workflow::run_workflow`]
/// (the engine lives in `crate::workflow` so the UI server can reuse it).
///
/// `latte-agent-core` implements its own tiny engine (see
/// `crate::workflow`) instead of reusing `latte-agent-orchestrator`,
/// because the orchestrator depends on core — a dependency cycle.
async fn register_workflow_tool(
    tm: &Arc<dyn latte_rs_agent_tools::types::ToolManager>,
    merged: &AgentConfig,
    resolver: &ModelResolver,
    default_params: GenerateParams,
    event_tx: broadcast::Sender<ChatEvent>,
    cwd: PathBuf,
    cancel_flag: Arc<AtomicBool>,
    turn_cancel_flag: Arc<AtomicBool>,
    agent_pause_gate: std::sync::Arc<crate::pause_gate::AgentPauseGate>,
    // 以下三项透传进 WorkflowRunContext，让 workflow 的每次角色
    // 分派与普通流程 delegate 一致：独立 subsession 日志、advisor
    // 产出门禁 + 返回审查。
    subsession_store: Arc<SubsessionStore>,
    session_id: String,
    advisor_gate: Option<GateConfig>,
    // Advisor intervene 暂停门：分派前 wait、运行中 park——advisor 的
    // 「已暂停等待拍板」对 workflow 流水线真实生效。
    advisor_pause: AdvisorPauseGate,
    // 原始用户诉求。调研类流水线跑完后用它判断「用户点名的交付物是否
    // 还欠着」——这是个二值事实，不涉及任何阈值或轮次计数。
    main_topic: Arc<parking_lot::Mutex<String>>,
) -> Result<(), Box<dyn std::error::Error>> {
    use latte_rs_agent_tools::types::{SchemaType, SharedToolHandler, Tool};
    // 注册时动态枚举 .latte/workflows.d（项目 + 全局）里的可用
    // workflow，写进 schema/description —— UI 管理界面新建的自定义
    // workflow 对模型立刻可见，不再靠静态样例列表。
    let available = crate::workflow::list_workflows(&cwd);
    let available_text = if available.is_empty() {
        "none found in .latte/workflows.d".to_string()
    } else {
        available
            .iter()
            .map(|(n, d)| {
                if d.is_empty() { n.clone() } else { format!("{n} — {d}") }
            })
            .collect::<Vec<_>>()
            .join("; ")
    };

    let input_schema = latte_rs_agent_tools::types::ToolInputSchema {
        schema_type: SchemaType,
        properties: vec![
            ("name".into(), ToolInputProperty {
                property_type: PropertyType::String,
                description: Some(format!("Workflow name. Available: {available_text}")),
                enum_values: None,
                minimum: None,
                maximum: None,
                min_length: None,
                max_length: None,
            }),
            ("topic".into(), ToolInputProperty {
                property_type: PropertyType::String,
                description: Some("The task/topic the workflow should work on. May be omitted/empty when 'resume' is given (falls back to the checkpointed topic)".into()),
                enum_values: None,
                minimum: None,
                maximum: None,
                min_length: None,
                max_length: None,
            }),
            ("resume".into(), ToolInputProperty {
                property_type: PropertyType::String,
                description: Some("Optional wf_id of a previous failed/interrupted run (shown in its error message as 'wf_id=...'). When given, completed steps are loaded from its checkpoint and skipped — the workflow continues from where it stopped instead of starting over".into()),
                enum_values: None,
                minimum: None,
                maximum: None,
                min_length: None,
                max_length: None,
            }),
        ]
        .into_iter()
        .collect(),
        required: Some(vec!["name".into()]),
        ..Default::default()
    };

    let merged_owned = Arc::new(merged.clone());
    let resolver_owned = Arc::new(resolver.clone());
    let agent_pause_gate_owned = agent_pause_gate.clone();
    // 派发幂等台账：同 (name, topic, resume) 的 workflow 不并发跑两条。
    // 两条同身份流水线会互相踩 staging 区、各写一份 checkpoint，产出
    // 谁覆盖谁取决于时序。
    let wf_ledger = crate::dispatch_ledger::DispatchLedger::new();

    let handler: SharedToolHandler = Arc::new(move |input: serde_json::Value, _ctx| {
        let merged = Arc::clone(&merged_owned);
        let resolver = Arc::clone(&resolver_owned);
        let default_params = default_params.clone();
        let event_tx = event_tx.clone();
        let cancel_flag = Arc::clone(&cancel_flag);
        let turn_cancel_flag = turn_cancel_flag.clone();
        let agent_pause_gate = agent_pause_gate_owned.clone();
        let subsession_store = subsession_store.clone();
        let session_id = session_id.clone();
        let advisor_gate = advisor_gate.clone();
        let advisor_pause = advisor_pause.clone();
        let cwd = cwd.clone();
        let wf_ledger = wf_ledger.clone();
        let main_topic = Arc::clone(&main_topic);
        Box::pin(async move {
            let tool_err = |msg: String| latte_rs_agent_tools::error::ToolError::Other(msg);
            let name = input
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tool_err("missing 'name' field".into()))?
                .to_string();
            let topic = input
                .get("topic")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let resume = input
                .get("resume")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            if resume.is_none() && topic.trim().is_empty() {
                return Err(tool_err(
                    "missing 'topic' field (required unless 'resume' is given)".into(),
                ));
            }

            // 派发幂等：同身份流水线正在跑就拒。**不**缓存已完成的
            // workflow —— 同一 topic 再跑一遍常常是有意的（改了配置、
            // 想要新一版方案），而且 workflow 有真实副作用，复用旧
            // 摘要会掩盖「这次什么都没跑」。这里只防并发重入。
            let wf_key = crate::dispatch_ledger::DispatchLedger::workflow_key(
                &name,
                &topic,
                resume.as_deref(),
            );
            let Some(_wf_guard) = wf_ledger.begin_exclusive(wf_key) else {
                return Err(tool_err(format!(
                    "workflow '{name}' 针对同一 topic 的运行**正在进行中**。\
                     不要重复启动：等它返回即可（两条同身份流水线会互相踩 staging 区\
                     与 checkpoint，产出谁覆盖谁取决于时序）。"
                )));
            };

            let wf = crate::workflow::load_workflow(&name, &cwd).map_err(|e| {
                let available = crate::workflow::list_workflows(&cwd)
                    .iter()
                    .map(|(n, _)| n.clone())
                    .collect::<Vec<_>>()
                    .join(", ");
                tool_err(format!("{e}. available workflows: {available}"))
            })?;

            let ctx = crate::workflow::WorkflowRunContext {
                merged,
                resolver,
                default_params,
                cwd,
                event_tx,
                turn_cancel_flag: Some(turn_cancel_flag),
                cancel_flag,
                agent_pause_gate: Some(agent_pause_gate),
                depth: 0,
                subsession_store: Some(subsession_store),
                session_id: Some(session_id),
                advisor_gate,
                advisor_pause: Some(advisor_pause),
                staging: None,
                // 顶层 run：自己就是嵌套链的根。
                root_wf_id: None,
            };
            let result = match &resume {
                Some(rid) => crate::workflow::run_workflow_resume(&wf, &topic, &ctx, rid).await,
                None => crate::workflow::run_workflow(&wf, &topic, &ctx).await,
            };
            result
                .map(|summary| {
                    let mut out = strip_think_blocks(&summary);
                    // 交付物提醒：调研类流水线跑完，且用户诉求里点名过
                    // 交付物时，把"还欠着什么 + 该跑哪个流程"附在返回末
                    // 尾。不数轮次、不设阈值、不阻断——只是把事实摆出来。
                    // 诉求没点名交付物（如「这块代码怎么回事」）时
                    // named 为空，什么也不加。
                    if crate::workflow::is_research_workflow(&name) {
                        let user_ask: String = main_topic.lock().clone();
                        let named = crate::workflow::named_deliverables(&user_ask);
                        if !named.is_empty() {
                            out.push_str(&crate::workflow::deliverable_reminder(&named));
                        }
                    }
                    serde_json::Value::String(out)
                })
                .map_err(|e| {
                    // 用户主动终止 ≠ 执行失败，善后动作相反：绝不能
                    // 自动 resume/重做——否则用户右键「终止此分派」把
                    // 活停掉，manager 立刻又起一遍，功能等于没有。
                    if crate::workflow::is_cancelled_by_user(&e) {
                        return tool_err(format!(
                            "{e}\n\n这是**用户主动终止**，不是失败：不要自动 resume 续跑，\
                             也不要换路径重做同一件事。请简短告知用户已停止，\
                             并询问下一步怎么做，然后结束本轮。"
                        ));
                    }
                    // 失败时给 manager 明确的善后指令——实测实锤：
                    // 裸错误回 tool loop 后 manager 没有动作，流水线
                    // 零产出收场。
                    tool_err(format!(
                        "{e}\n\n请善后：能修复的修复后用上面的 wf_id 以 resume 参数续跑，\
                         或换路径重做；处理完向用户汇报结果，不要静默结束。"
                    ))
                })
        })
    });

    let tool = Tool::builder(
        "workflow".to_string(),
        format!("Run a named multi-role workflow. Available workflows: {available_text}"),
        input_schema,
        handler,
    )
    // 见 ORCHESTRATION_TOOL_TIMEOUT_SECS：一次调用包着整条流水线，
    // 不能吃工具管理器 25min 默认熔断。
    .timeout(std::time::Duration::from_secs(ORCHESTRATION_TOOL_TIMEOUT_SECS))
    .build();

    tm.register(tool, Some("manager"));
    Ok(())
}

/// Try to run a workflow by slash command (e.g. "/plan xxx").
/// Returns `Ok(Some(summary))` if a workflow was found and ran,
/// `Ok(None)` if no workflow matched the command (not an error),
/// or `Err(msg)` if the workflow failed.
async fn run_workflow_command(
    cmd: &str,
    topic: &str,
    config: &ControllerConfig,
    event_tx: &broadcast::Sender<ChatEvent>,
    cancel_flag: Arc<AtomicBool>,
    agent_pause_gate: std::sync::Arc<crate::pause_gate::AgentPauseGate>,
    turn_cancel_flag: Arc<AtomicBool>,
    advisor_pause: AdvisorPauseGate,
) -> Result<Option<String>, String> {
    let wf = match crate::workflow::load_workflow_by_command(cmd, &config.cwd) {
        Ok(w) => w,
        Err(_) => return Ok(None), // no workflow registered for this command
    };

    let ctx = crate::workflow::WorkflowRunContext {
        merged: config.agent_config.clone(),
        resolver: config.model_resolver.clone(),
        default_params: config.default_params.clone(),
        cwd: config.cwd.clone(),
        event_tx: event_tx.clone(),
        cancel_flag,
        turn_cancel_flag: Some(turn_cancel_flag),
        agent_pause_gate: Some(agent_pause_gate),
        depth: 0,
        // 与 manager 的 workflow 工具一致：slash 命令触发的 workflow
        // 分派也建 subsession、过 advisor gate（session_id 为空时
        // subsession 退化到内存，与 delegate 的旧行为一致）。
        subsession_store: Some(config.subsession_store.clone()),
        session_id: Some(config.session_id.clone()),
        advisor_gate: config.advisor_monitor.runner_gate(),
        advisor_pause: Some(advisor_pause),
        staging: None,
        // 顶层 run：自己就是嵌套链的根。
        root_wf_id: None,
    };

    let summary = crate::workflow::run_workflow(&wf, topic, &ctx).await?;
    Ok(Some(summary))
}

fn role_icon(role_id: &str) -> String {
    match role_id {
        "manager" => "🧠",
        "programmer" => "💻",
        "architect" => "🏗️",
        "reviewer" | "reviewer_sanity" => "🔍",
        "reviewer_architecture" => "📐",
        "reviewer_security" => "🔒",
        "tester" => "🧪",
        "security" => "🛡️",
        "devops" => "⚙️",
        "designer" => "🎨",
        "tech_writer" => "📝",
        "pm" => "👔",
        _ => "🤖",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── plan 工具的 paths 机械校验 ───────────────────────────────

    fn plan_task(title: &str, labels: &[&str], paths: &[&str]) -> PlanTask {
        PlanTask {
            title: title.into(),
            description: String::new(),
            priority: None,
            labels: labels.iter().map(|s| s.to_string()).collect(),
            workflow: None,
            paths: paths.iter().map(|s| s.to_string()).collect(),
            subtasks: vec![],
        }
    }

    // ─── paths 字段名别名 + 未知键回执 ────────────────────────────
    //
    // 回归 jemalloc 会话事故：manager 把 `paths` 写成 `involved_paths`，
    // serde 静默丢弃整个数组 → 24 个任务全部 `paths: []` →
    // `validate_plan_paths` 的幻觉路径校验与重叠校验双双空转通过。

    #[test]
    fn plan_task_accepts_involved_paths_alias() {
        let v = serde_json::json!({
            "title": "T-401 P2 修复",
            "involved_paths": ["src/safety_check.c", "include/jemalloc/internal"],
        });
        let t: PlanTask = serde_json::from_value(v).expect("必须解析成功");
        assert_eq!(
            t.paths,
            vec!["src/safety_check.c", "include/jemalloc/internal"],
            "involved_paths 必须映射到 paths，不能被静默丢弃"
        );
    }

    #[test]
    fn plan_task_accepts_other_path_aliases() {
        for key in ["involved_files", "affected_paths", "affected_files", "file_paths", "files"] {
            let v = serde_json::json!({ "title": "t", key: ["src/a.c"] });
            let t: PlanTask = serde_json::from_value(v).unwrap_or_else(|e| panic!("{key}: {e}"));
            assert_eq!(t.paths, vec!["src/a.c"], "别名 {key} 必须映射到 paths");
        }
    }

    #[test]
    fn unknown_plan_task_keys_reports_dropped_fields() {
        // workflow 产出的 id/week/dependencies/risks 是合法附加信息，
        // 不拦但要回执；paths 的别名不算未知键。
        let arr = vec![
            serde_json::json!({
                "title": "a",
                "involved_paths": ["src/a.c"],
                "week": 1,
                "dependencies": ["T-101"],
            }),
            serde_json::json!({
                "title": "b",
                "risks": ["x"],
                "subtasks": [{ "title": "b1", "id": "S-1" }],
            }),
        ];
        let got = unknown_plan_task_keys(&arr);
        assert_eq!(
            got,
            vec!["dependencies", "id", "risks", "week"],
            "未知键必须去重、稳定排序，并递归覆盖 subtasks；paths 别名不算未知"
        );
    }

    #[test]
    fn unknown_plan_task_keys_empty_for_clean_input() {
        let arr = vec![serde_json::json!({
            "title": "a",
            "description": "d",
            "priority": 1,
            "labels": ["x"],
            "workflow": "",
            "paths": ["src/a.c"],
            "subtasks": [],
        })];
        assert!(unknown_plan_task_keys(&arr).is_empty(), "全合法键不应有回执");
    }

    /// 纯阅读/学习类任务共用同一份代码是正常的（不会互相覆盖），
    /// 不该被重叠校验拦下。
    ///
    /// 回归实测事故：8 个只读学习任务因为都要读
    /// `include/foo/internal` 被判 45 处冲突，manager 连撞两次
    /// plan、白烧约 5 分钟模型时间。
    #[test]
    fn readonly_tasks_skip_overlap_check() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        std::fs::create_dir_all(cwd.join("include/foo/internal")).unwrap();
        let tasks = vec![
            plan_task("P1 读公共头", &["学习", "只读"], &["include/foo"]),
            plan_task(
                "P2 读内部头",
                &["只读"],
                &["include/foo/internal"],
            ),
        ];
        assert!(validate_plan_paths(cwd, &tasks).is_ok());
        // 同样的路径，但任务没有只读标记 → 仍然拦。
        let writing = vec![
            plan_task("P1 改公共头", &[], &["include/foo"]),
            plan_task("P2 改内部头", &[], &["include/foo/internal"]),
        ];
        assert!(validate_plan_paths(cwd, &writing).is_err());
    }

    /// 冲突反馈必须短且按路径聚合：tool_result 只有 256 字节，长清单
    /// 会被截断，模型看不全就修不动（事故里 5855 字 / 45 对）。
    #[test]
    fn overlap_error_is_short_and_aggregated() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        std::fs::create_dir_all(cwd.join("include/foo/internal")).unwrap();
        std::fs::create_dir_all(cwd.join("src")).unwrap();
        let hot = "include/foo/internal";
        let tasks: Vec<PlanTask> = (0..8)
            .map(|i| {
                plan_task(
                    &format!("P{i}：一个标题很长的任务，长到足以把错误信息撑爆"),
                    &[],
                    &[hot, "src"],
                )
            })
            .collect();
        let err = validate_plan_paths(cwd, &tasks).expect_err("应判重叠");
        assert!(
            err.len() <= 256,
            "错误必须能塞进 256 字节的 tool_result（实际 {} 字节）：{err}",
            err.len()
        );
        // 保留分类关键词，否则 classify_tool_execution_error 不会判
        // PermanentExec（会变成原样重试）。
        assert!(err.contains("paths 范围重叠"), "{err}");
        // 报热点路径而不是罗列任务对。
        assert!(err.contains(hot), "{err}");
        assert!(err.contains("只读"), "应告知只读逃生阀：{err}");
    }

    /// 幻觉路径清单同样要截断，只报前几条 + 总数。
    #[test]
    fn missing_paths_error_is_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let tasks: Vec<PlanTask> = (0..9)
            .map(|i| plan_task(&format!("T{i}"), &[], &[&format!("no_such_dir_{i}/x.rs")]))
            .collect();
        let err = validate_plan_paths(cwd, &tasks).expect_err("应判幻觉路径");
        assert!(err.contains("疑似幻觉路径"), "{err}");
        assert!(err.contains("共 9 处"), "应给出总数：{err}");
        assert!(err.len() <= 256, "实际 {} 字节：{err}", err.len());
    }

    // ─── Advisor v3 pause gate（controller 侧）─────────────────────

    #[tokio::test]
    async fn advisor_pause_resolves_on_any_user_input() {
        let c = ChatController::new(8);
        assert!(!c.advisor_pause_requested());
        c.request_pause();
        assert!(c.advisor_pause_requested());
        // 任何用户输入都算拍板（继续）：清旗，不取消 turn。
        c.submit_input("继续").await;
        assert!(!c.advisor_pause_requested());
        assert!(!c.turn_cancel_requested());
    }

    #[tokio::test]
    async fn advisor_pause_stop_keyword_cancels_turn() {
        let c = ChatController::new(8);
        c.request_pause();
        // 命中"终止"关键词：resolve + 软终止当前 turn 同时发生。
        c.submit_input("终止本轮吧").await;
        assert!(!c.advisor_pause_requested());
        assert!(c.turn_cancel_requested());
    }

    #[tokio::test]
    async fn advisor_resume_also_resolves_pause_gate() {
        let c = ChatController::new(8);
        c.request_pause();
        c.resume().await;
        assert!(!c.advisor_pause_requested());
    }

    #[test]
    fn strip_think_blocks_cases() {
        // 无 think：原样返回
        assert_eq!(strip_think_blocks("正式回答"), "正式回答");
        // 单个块：剥掉并 trim
        assert_eq!(
            strip_think_blocks("<think>推理过程</think>\n\n正式回答"),
            "正式回答"
        );
        // 多个块 + 中间内容保留
        assert_eq!(
            strip_think_blocks("<think>a</think>第一部分<think>b</think>第二部分"),
            "第一部分第二部分"
        );
        // 未闭合块：丢弃到结尾（仅剩 think → 回退原文）
        assert_eq!(strip_think_blocks("<think>推理中断"), "<think>推理中断");
        // 只有 think（被截断）：回退原文，避免空负载
        assert_eq!(
            strip_think_blocks("<think>只有推理</think>"),
            "<think>只有推理</think>"
        );
    }

    /// 流式 delta 必须带 `sub_id`（与同一个 sink 里的 ToolUse /
    /// ToolResult 分支一致）。
    ///
    /// 此前写死 `None`：子代理的逐 token 流以主角色身份出现，而它的
    /// 工具行带 sub_id，同一段对话被拆到两处；前端按 role_id + sub_id
    /// 建流式气泡，None 会让并行波里两个同 role 分派撞同一个 key，
    /// 两路 token 交错混进一个气泡。
    #[test]
    fn model_delta_carries_sub_id_like_tool_events() {
        use crate::trace::TraceSink as _;

        let (tx, mut rx) = broadcast::channel(8);
        let sink = ChatEventTraceSink {
            event_tx: tx,
            sub_id: Some("programmer-sub-7".into()),
        };
        sink.emit(crate::trace::TraceEvent::ModelDelta {
            meta: crate::trace::TraceMeta::test_default(),
            delta: "半句话".into(),
        });
        match rx.try_recv().unwrap() {
            ChatEvent::RoleTurn { content, is_complete, sub_id, .. } => {
                assert_eq!(content, "半句话");
                assert!(!is_complete, "delta 是增量，不是终态");
                assert_eq!(
                    sub_id.as_deref(),
                    Some("programmer-sub-7"),
                    "delta 必须归属到发起它的 subsession"
                );
            }
            other => panic!("expected RoleTurn, got {other:?}"),
        }

        // 主 session 角色（sub_id: None）仍然不带 —— 不能反过来给主
        // 角色的流硬塞一个 sub_id。
        let (tx2, mut rx2) = broadcast::channel(8);
        let main_sink = ChatEventTraceSink { event_tx: tx2, sub_id: None };
        main_sink.emit(crate::trace::TraceEvent::ModelDelta {
            meta: crate::trace::TraceMeta::test_default(),
            delta: "主角色".into(),
        });
        match rx2.try_recv().unwrap() {
            ChatEvent::RoleTurn { sub_id, .. } => assert!(sub_id.is_none()),
            other => panic!("expected RoleTurn, got {other:?}"),
        }
    }

    #[test]
    fn chat_event_trace_sink_broadcasts_tool_events() {
        use crate::trace::TraceSink as _;

        let (tx, mut rx) = broadcast::channel(8);
        let sink = ChatEventTraceSink { event_tx: tx, sub_id: None };

        // ParseToolCalls → one ToolUse per parsed call (this is what
        // lets the advisor monitor see routing decisions like the
        // manager invoking the `workflow` tool).
        sink.emit(crate::trace::TraceEvent::ParseToolCalls {
            meta: crate::trace::TraceMeta::test_default(),
            raw_in: String::new(),
            parsed: vec![crate::trace::ParsedCall {
                id: String::new(),
                name: "workflow".into(),
                args: "{\"name\":\"design_brainstorm\"}".into(),
            }],
            diagnostics: crate::trace::ParseDiag {
                opens_found: 1,
                closes_matched: 1,
                unmatched_opens: vec![],
            },
        });
        // ToolExec Err → ToolError (this is what lets the advisor see
        // a failed workflow run).
        sink.emit(crate::trace::TraceEvent::ToolExec {
            meta: crate::trace::TraceMeta::test_default(),
            name: "workflow".into(),
            args_json: "{}".into(),
            latency_ms: 1,
            status: crate::trace::ToolStatus::Err("all models unavailable".into()),
        });

        match rx.try_recv().unwrap() {
            ChatEvent::ToolUse { tool_name, args, .. } => {
                assert_eq!(tool_name, "workflow");
                assert!(args.contains("design_brainstorm"));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            ChatEvent::ToolError { tool_name, error, .. } => {
                assert_eq!(tool_name, "workflow");
                assert!(error.contains("all models unavailable"));
            }
            other => panic!("expected ToolError, got {other:?}"),
        }
    }

    /// delegate / workflow speaker 路径的 sink 带 sub_id：工具事件
    /// 必须把它透传给 ChatEvent，前端才能按 subsession 精确归属，
    /// 而不是泄进主 session 按 role_id 猜（日志事故：
    /// task_planner 的 read 全显示在主 session）。
    #[test]
    fn chat_event_trace_sink_propagates_sub_id() {
        use crate::trace::TraceSink as _;

        let (tx, mut rx) = broadcast::channel(8);
        let sink = ChatEventTraceSink {
            event_tx: tx,
            sub_id: Some("task_planner-1".into()),
        };

        sink.emit(crate::trace::TraceEvent::ParseToolCalls {
            meta: crate::trace::TraceMeta::test_default(),
            raw_in: String::new(),
            parsed: vec![crate::trace::ParsedCall {
                id: String::new(),
                name: "read".into(),
                args: "{\"path\":\"a.md\"}".into(),
            }],
            diagnostics: crate::trace::ParseDiag {
                opens_found: 1,
                closes_matched: 1,
                unmatched_opens: vec![],
            },
        });
        sink.emit(crate::trace::TraceEvent::ToolExec {
            meta: crate::trace::TraceMeta::test_default(),
            name: "read".into(),
            args_json: "{}".into(),
            latency_ms: 1,
            status: crate::trace::ToolStatus::Ok("file contents".into()),
        });
        sink.emit(crate::trace::TraceEvent::ToolExec {
            meta: crate::trace::TraceMeta::test_default(),
            name: "read".into(),
            args_json: "{}".into(),
            latency_ms: 1,
            status: crate::trace::ToolStatus::Err("boom".into()),
        });

        match rx.try_recv().unwrap() {
            ChatEvent::ToolUse { sub_id, .. } => {
                assert_eq!(sub_id.as_deref(), Some("task_planner-1"));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            ChatEvent::ToolResult { sub_id, .. } => {
                assert_eq!(sub_id.as_deref(), Some("task_planner-1"));
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            ChatEvent::ToolError { sub_id, .. } => {
                assert_eq!(sub_id.as_deref(), Some("task_planner-1"));
            }
            other => panic!("expected ToolError, got {other:?}"),
        }
    }

    #[test]
    fn delegate_hint_lists_roles_from_config() {
        let mut cfg = AgentConfig::default();
        for (id, name) in [("programmer", "Software Engineer"), ("my_custom_role", "")] {
            cfg.roles.insert(
                id.to_string(),
                crate::role::RoleTemplate {
                    id: id.into(),
                    name: name.into(),
                    category: "engineering".into(),
                    model_tier: "standard".into(),
                    model_chain: vec![],
                    prompt_file: None,
                    temperature: None,
                    tools: vec![],
                    icon: String::new(),
                    skills: vec![],
            code_paths: vec![],
                },
            );
        }
        let hint = delegate_tool_hint(&cfg);
        assert!(hint.contains("programmer(Software Engineer)"), "hint: {hint}");
        assert!(hint.contains("my_custom_role"), "hint: {hint}");
        // 花名册排序：my_custom_role 在 programmer 前
        let roster = role_roster_text(&cfg);
        assert!(roster.find("my_custom_role").unwrap() < roster.find("programmer").unwrap());
    }

    #[test]
    fn workflow_hint_lists_available_workflows() {
        let dir = tempfile::tempdir().unwrap();
        let wf_dir = dir.path().join(".latte").join("workflows.d");
        std::fs::create_dir_all(&wf_dir).unwrap();
        std::fs::write(
            wf_dir.join("my_custom.toml"),
            "name = \"my_custom\"\ndescription = \"自定义流程\"\n[[steps]]\nid = \"a\"\nspeakers = [\"tester\"]\nprompt = \"{{topic}}\"\n",
        )
        .unwrap();
        let hint = workflow_tool_hint(dir.path());
        assert!(hint.contains("`my_custom` — 自定义流程"), "hint: {hint}");
        assert!(hint.contains("分派前先想流程"), "hint: {hint}");
    }

    /// 选流程的判别依据必须是**交付物落点**，而且只能有一条、还得是
    /// 与具体流程无关的通则。
    ///
    /// 回归背景：这里曾经是一张硬编码的「用户要的东西 → 选这个」对照
    /// 表，里面连「安排学习计划 + 拆分任务」这种具体句式都写死了。个别
    /// 化的表有两个坏处：新增自定义流程它一律覆盖不到；以及它和 prompt
    /// 里另一处「按话题选流程」的说法互相矛盾，模型命中先出现的那条就
    /// 选错（实测：用户要的是拆任务清单，manager 按"从浅到深"这个话题
    /// 词跑了交互学习流程，看板 0 个任务）。落点信息现在由每条 workflow
    /// 自己在 description 里声明，提示只留通则。
    #[test]
    fn workflow_hint_selects_by_deliverable_landing_not_topic() {
        let dir = tempfile::tempdir().unwrap();
        let hint = workflow_tool_hint(dir.path());
        assert!(
            hint.contains("按交付物落点，不按话题词"),
            "hint: {hint}"
        );
        assert!(hint.contains("落点："), "hint 必须指向清单里的落点标注: {hint}");
        assert!(
            hint.contains("几个不同落点，就发几个 workflow 调用"),
            "hint: {hint}"
        );
        // 不得再出现按话题匹配流程类型的说法（与落点规则冲突的旧文案）。
        assert!(
            !hint.contains("学习/调研/规划类诉求"),
            "按话题选流程的说法必须清除: {hint}"
        );
        // 不得再出现硬编码的「用户要的东西 → 选这个」对照表。
        assert!(
            !hint.contains("| 用户要的东西 |"),
            "个别化路由表必须清除: {hint}"
        );
        // 收口是按交付物，不是按轮数——提示里不得出现轮次上限。
        assert!(hint.contains("调研要收口（不是限制轮数）"), "hint: {hint}");
        assert!(
            !hint.contains("最多 1 轮"),
            "不该有拍脑袋的轮次上限: {hint}"
        );
        assert!(hint.contains("不要重复探索同一范围"), "hint: {hint}");
        assert!(hint.contains("交付物提醒"), "hint: {hint}");
        // 「先不锁方向」不得成为回退调研的理由。
        assert!(hint.contains("先不锁方向"), "hint: {hint}");
    }

    /// 落点必须由每条 workflow 自己声明，否则「按落点挑」这条通则在
    /// 清单里无据可依，prompt 就又会被迫抄一张个别化的对照表。
    #[test]
    fn every_shipped_workflow_declares_its_deliverable_landing() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("config")
            .join("workflows");
        let mut missing: Vec<String> = Vec::new();
        let mut checked = 0usize;
        for e in std::fs::read_dir(&dir).expect("config/workflows 必须存在").flatten() {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("toml") {
                continue;
            }
            let raw = std::fs::read_to_string(&path).unwrap();
            let wf: crate::workflow::WorkflowDef = match toml::from_str(&raw) {
                Ok(w) => w,
                Err(err) => panic!("{} 解析失败: {err}", path.display()),
            };
            checked += 1;
            if !wf.description.contains("落点：") {
                missing.push(path.file_stem().unwrap().to_string_lossy().into_owned());
            }
        }
        assert!(checked > 0, "没扫到任何 workflow");
        assert!(
            missing.is_empty(),
            "这些 workflow 的 description 没声明落点：{missing:?}"
        );
    }

    #[test]
    fn workflow_hint_empty_dir_degrades_gracefully() {
        // list_workflows 会回退到全局 $LATTE_HOME/workflows.d —— 用
        // ENV_LOCK + 空的 LATTE_HOME 隔离本机全局目录。
        let guard = crate::test_util::ENV_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap();
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("LATTE_HOME");
        std::env::set_var("LATTE_HOME", home.path());
        let hint = workflow_tool_hint(project.path());
        match prev {
            Some(v) => std::env::set_var("LATTE_HOME", v),
            None => std::env::remove_var("LATTE_HOME"),
        }
        drop(guard);
        assert!(hint.contains("没有可用 workflow"), "hint: {hint}");
    }

    /// workflow 工具 schema 必须暴露可选的 `resume` 参数（断点续跑
    /// 入口），且 `topic` 不再必填（resume 时用 checkpoint 的 topic）。
    #[tokio::test]
    async fn workflow_tool_schema_exposes_resume_param() {
        let tm = build_tool_manager(&["workflow".to_string()])
            .await
            .expect("tool manager");
        let merged = AgentConfig {
            advisor: Default::default(),
            models: crate::config::ModelCatalog {
                models: vec![],
                tiers: None,
                role_tiers: None,
            },
            roles: std::collections::HashMap::new(),
        };
        let resolver = crate::model_resolver::ModelResolver::from_config(&merged)
            .expect("resolver from empty config");
        let (event_tx, _rx) = broadcast::channel(8);
        let dir = tempfile::tempdir().unwrap();
        register_workflow_tool(
            &tm,
            &merged,
            &resolver,
            GenerateParams::default(),
            event_tx,
            dir.path().to_path_buf(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            crate::pause_gate::AgentPauseGate::new("test"),
            Arc::new(crate::subsession::SubsessionStore::new()),
            "test-session".into(),
            None,
            AdvisorPauseGate::new(),
            Arc::new(parking_lot::Mutex::new(String::new())),
        )
        .await
        .expect("register workflow tool");

        let tool = tm.get_tool("workflow").expect("workflow tool registered");
        assert!(
            tool.input_schema.properties.contains_key("resume"),
            "schema must expose resume: {:?}",
            tool.input_schema.properties.keys().collect::<Vec<_>>()
        );
        let required = tool.input_schema.required.clone().unwrap_or_default();
        assert!(required.iter().any(|r| r == "name"), "name stays required");
        assert!(
            !required.iter().any(|r| r == "topic"),
            "topic optional (checkpoint supplies it on resume)"
        );
    }

    #[test]
    fn no_steps_returns_empty_plan() {
        let plan = compute_waves::<WorkflowStepConfig>(&[]);
        assert!(plan.waves.is_empty());
    }

    #[test]
    fn single_step_one_wave() {
        let steps = vec![WorkflowStepConfig {
            id: "s1".into(),
            speakers: vec!["a".into()],
            prompt: "test".into(),
            hooks: vec![],
            contract: None,
        }];
        let plan = compute_waves(&steps);
        assert_eq!(plan.waves.len(), 1);
        assert_eq!(plan.waves[0].steps, vec![0]);
    }

    #[test]
    fn two_independent_steps_one_wave() {
        let steps = vec![
            WorkflowStepConfig {
                id: "s1".into(),
                speakers: vec!["a".into()],
                prompt: "test".into(),
                hooks: vec![],
                contract: Some(FileContractConfig {
                    input: None,
                    output: Some("out1.md".into()),
                    placeholder: None,
                    extract_prompt: None,
                    required: true,
                }),
            },
            WorkflowStepConfig {
                id: "s2".into(),
                speakers: vec!["b".into()],
                prompt: "test".into(),
                hooks: vec![],
                contract: Some(FileContractConfig {
                    input: None,
                    output: Some("out2.md".into()),
                    placeholder: None,
                    extract_prompt: None,
                    required: true,
                }),
            },
        ];
        let plan = compute_waves(&steps);
        // Two independent producers → same wave
        assert_eq!(plan.waves.len(), 1);
        assert!(plan.waves[0].steps.contains(&0));
        assert!(plan.waves[0].steps.contains(&1));
    }

    #[test]
    fn dependent_steps_two_waves() {
        let steps = vec![
            WorkflowStepConfig {
                id: "research".into(),
                speakers: vec!["arch".into()],
                prompt: "research".into(),
                hooks: vec![],
                contract: Some(FileContractConfig {
                    input: None,
                    output: Some("summary.md".into()),
                    placeholder: None,
                    extract_prompt: None,
                    required: true,
                }),
            },
            WorkflowStepConfig {
                id: "plan".into(),
                speakers: vec!["pm".into()],
                prompt: "plan: {{research_summary}}".into(),
                hooks: vec![],
                contract: Some(FileContractConfig {
                    input: Some("summary.md".into()),
                    output: Some("plan.md".into()),
                    placeholder: Some("research_summary".into()),
                    extract_prompt: None,
                    required: true,
                }),
            },
        ];
        let plan = compute_waves(&steps);
        assert_eq!(plan.waves.len(), 2, "research and plan should be in separate waves");
        assert_eq!(plan.waves[0].steps, vec![0], "wave 0 should be research");
        assert_eq!(plan.waves[1].steps, vec![1], "wave 1 should be plan");
    }

    #[test]
    fn diamond_dependency() {
        // research → review → plan
        //         → test  → plan (test also depends on research)
        let steps = vec![
            WorkflowStepConfig {
                id: "research".into(),
                speakers: vec!["r".into()],
                prompt: "r".into(),
                hooks: vec![],
                contract: Some(FileContractConfig {
                    input: None,
                    output: Some("out.md".into()),
                    placeholder: None,
                    extract_prompt: None,
                    required: true,
                }),
            },
            WorkflowStepConfig {
                id: "review".into(),
                speakers: vec!["v".into()],
                prompt: "v".into(),
                hooks: vec![],
                contract: Some(FileContractConfig {
                    input: Some("out.md".into()),
                    output: Some("review.md".into()),
                    placeholder: None,
                    extract_prompt: None,
                    required: true,
                }),
            },
            WorkflowStepConfig {
                id: "test".into(),
                speakers: vec!["t".into()],
                prompt: "t".into(),
                hooks: vec![],
                contract: Some(FileContractConfig {
                    input: Some("out.md".into()),
                    output: Some("test.md".into()),
                    placeholder: None,
                    extract_prompt: None,
                    required: true,
                }),
            },
            WorkflowStepConfig {
                id: "plan".into(),
                speakers: vec!["p".into()],
                prompt: "p".into(),
                hooks: vec![],
                contract: Some(FileContractConfig {
                    input: Some("review.md".into()),
                    output: None,
                    placeholder: None,
                    extract_prompt: None,
                    required: true,
                }),
            },
        ];
        let plan = compute_waves(&steps);
        assert_eq!(plan.waves.len(), 3, "diamond should be 3 waves");
        assert_eq!(plan.waves[0].steps, vec![0], "wave 0 should be research");
        // Wave 1: review + test (both depend on research, independent of each other)
        assert_eq!(plan.waves[1].steps.len(), 2, "wave 1 should have 2 parallel steps");
        assert!(plan.waves[1].steps.contains(&1));
        assert!(plan.waves[1].steps.contains(&2));
        assert_eq!(plan.waves[2].steps, vec![3], "wave 2 should be plan");
    }
    // ─── Delegate event schema contract ─────────────────────────
    //
    // The frontend (`src/api/chat.ts:81-82`) types the
    // `DelegateStarted` / `DelegateFinished` variants with
    // snake_case field names (`from_role`, `to_role`, `task`,
    // `status`, `summary`) and PascalCase variant tags. These
    // tests pin the serialization shape so a careless `rename_all`
    // on the enum or a field rename won't silently break the
    // CLI / latte-code-editor chat module / future CLI frontends.

    #[test]
    fn delegate_started_serializes_to_frontend_shape() {
        let event = ChatEvent::DelegateStarted {
            from_role: "manager".into(),
            to_role: "programmer".into(),
            task: "read the project structure".into(),
            sub_id: "programmer-sub-1".into(),
            wf_id: None,
        };
        let json = serde_json::to_value(&event).expect("serialize");
        // PascalCase variant tag → must match the TS discriminated
        // union key in `src/api/chat.ts`.
        assert!(json.get("DelegateStarted").is_some(), "missing variant tag");
        // snake_case fields → must match the TS field names.
        assert_eq!(json["DelegateStarted"]["from_role"], "manager");
        assert_eq!(json["DelegateStarted"]["to_role"], "programmer");
        assert_eq!(
            json["DelegateStarted"]["task"],
            "read the project structure"
        );
        assert_eq!(json["DelegateStarted"]["sub_id"], "programmer-sub-1");
    }

    #[test]
    fn delegate_finished_serializes_to_frontend_shape() {
        let event = ChatEvent::DelegateFinished {
            from_role: "manager".into(),
            to_role: "programmer".into(),
            status: "ok".into(),
            summary: "found 3 files".into(),
            sub_id: "programmer-sub-1".into(),
            wf_id: None,
        };
        let json = serde_json::to_value(&event).expect("serialize");
        assert!(json.get("DelegateFinished").is_some(), "missing variant tag");
        assert_eq!(json["DelegateFinished"]["from_role"], "manager");
        assert_eq!(json["DelegateFinished"]["to_role"], "programmer");
        assert_eq!(json["DelegateFinished"]["status"], "ok");
        assert_eq!(json["DelegateFinished"]["summary"], "found 3 files");
        assert_eq!(json["DelegateFinished"]["sub_id"], "programmer-sub-1");
    }
    #[test]
    fn plan_proposed_serializes_to_frontend_shape() {
        // 验证 PlanProposed 事件经 chat_event_to_frontend_json 转成
        // 前端 internally-tagged 形态：{"type":"PlanProposed",...}，
        // 字段 snake_case；tasks 里空字段被 skip_serializing_if 省略。
        let event = ChatEvent::PlanProposed {
            role_id: "manager".into(),
            plan_id: "plan-manager-0".into(),
            tasks: vec![
                PlanTask {
                    title: "实现 ringbuf 核心读写".into(),
                    description: "覆盖并发读写路径".into(),
                    priority: Some(1),
                    labels: vec!["core".into()],
                    workflow: Some("tdd_development".into()),
                    paths: vec!["src/ringbuf".into()],
                    subtasks: vec![],
                },
                PlanTask {
                    title: "补测试".into(),
                    description: String::new(),
                    priority: None,
                    labels: vec![],
                    workflow: None,
                    paths: vec![],
                    subtasks: vec![],
                },
            ],
        };
        let wire = crate::event_json::chat_event_to_frontend_json(&event).expect("frontend json");
        let v: serde_json::Value = serde_json::from_str(&wire).expect("parse wire");
        // internally-tagged discriminator（与 api.ts 的 union key 对齐）。
        assert_eq!(v["type"], "PlanProposed");
        assert_eq!(v["role_id"], "manager");
        assert_eq!(v["plan_id"], "plan-manager-0");
        let tasks = v["tasks"].as_array().expect("tasks array");
        assert_eq!(tasks.len(), 2);
        // 第一项：全字段序列化。
        assert_eq!(tasks[0]["title"], "实现 ringbuf 核心读写");
        assert_eq!(tasks[0]["priority"], 1);
        assert_eq!(tasks[0]["workflow"], "tdd_development");
        assert_eq!(tasks[0]["paths"], serde_json::json!(["src/ringbuf"]));
        // 第二项：空字段被 skip_serializing_if 省略（title 必留）。
        assert_eq!(tasks[1]["title"], "补测试");
        assert!(tasks[1].get("description").is_none(), "空 description 应省略");
        assert!(tasks[1].get("priority").is_none(), "None priority 应省略");
        assert!(tasks[1].get("labels").is_none(), "空 labels 应省略");
        assert!(tasks[1].get("paths").is_none(), "空 paths 应省略");
    }

    // ─── plan 阶段门（PlanStage） ─────────────────────────────────
    //
    // manager 调 plan 工具 → PendingApproval；用户导入任务看板 →
    // Approved；下一条用户消息 → Normal。PendingApproval 期间
    // delegate 实现类角色（programmer*/devops*）被工具层拒绝。

    /// env 测试串行锁（避免并行 cargo test 污染 env）。
    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// 造一个共享阶段门句柄。
    fn fresh_plan_stage() -> SharedPlanStage {
        Arc::new(parking_lot::RwLock::new(PlanStage::Normal))
    }

    /// 回归防线：`build_runner` 的**两条分支都必须装上 trace sink**。
    ///
    /// 无工具分支曾把 `runner_sink` 造好却忘了 `.with_sink(...)`，编译器只报
    /// 一句 `unused variable: runner_sink`，实际后果是这类角色的 trace 事件
    /// 既不落 `.latte/ui-sessions/<sid>/` 子会话日志、也不经
    /// `ChatEventTraceSink` 广播给 UI——执行轨迹当场看不到、事后查不到。
    /// 有工具的分支一直是对的，两边不一致。
    ///
    /// `build_runner` 此前**零测试覆盖**，这是第一个。
    #[tokio::test]
    async fn build_runner_attaches_trace_sink_for_both_branches() {
        use crate::config::{ModelCatalog, ModelDef};
        use crate::role::RoleTemplate;
        let server = wiremock::MockServer::start().await;
        let model = ModelDef {
            name: "stub".into(),
            api: "openai".into(),
            provider: "test".into(),
            base_url: server.uri(),
            api_key: "test-key".into(),
            context_window: 32000,
            max_tokens: 4096,
            supports_thinking: false,
            supports_vision: false,
            supports_image_generation: false,
            cost_per_million_input: None,
            cost_per_million_output: None,
            tier: Some("standard".into()),
            timeout_secs: None,
        };
        let role_with = |id: &str, tools: Vec<String>| RoleTemplate {
            id: id.into(),
            name: id.into(),
            category: "execution".into(),
            model_tier: "standard".into(),
            model_chain: vec![],
            prompt_file: None,
            temperature: None,
            tools,
            icon: String::new(),
            skills: vec![],
            code_paths: vec![],
        };
        let merged = AgentConfig {
            advisor: Default::default(),
            models: ModelCatalog {
                models: vec![model],
                tiers: None,
                role_tiers: None,
            },
            roles: [
                // 无工具 → 走 build_runner 的 else 分支（曾漏挂 sink）
                ("bare".to_string(), role_with("bare", vec![])),
                // 有工具 → 走 if 分支（一直正确）
                ("handy".to_string(), role_with("handy", vec!["bash".into()])),
            ]
            .into_iter()
            .collect(),
        };
        let resolver = ModelResolver::from_config(&merged).expect("resolver");
        let (event_tx, _rx) = broadcast::channel(64);
        let params = GenerateParams::default();

        for role_id in ["bare", "handy"] {
            let (runner, _) = build_runner(
                &merged,
                &resolver,
                &params,
                role_id,
                ModelTier::Standard,
                None,
                &event_tx,
                std::path::Path::new("/tmp"),
                Arc::new(crate::subsession::SubsessionStore::new()),
                "ui-test",
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(false)),
                Arc::new(parking_lot::Mutex::new(String::new())),
                None,
                AdvisorPauseGate::new(),
                &fresh_plan_stage(),
                crate::pause_gate::AgentPauseGate::new("test"),
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .expect("build_runner");
            assert!(
                runner.has_trace_sink(),
                "角色 '{role_id}' 的 runner 漏挂了 trace sink"
            );
        }
    }

    #[test]
    fn default_max_delegates_respects_env_and_default() {
        // env 缺失 → 0 = 不限制（改自旧默认 12：单 session 的
        // delegate 次数改由 wall-clock timeout 与死循环熔断兜底，
        // 不再用一个固定次数硬砍）。
        let _g = lock_env();
        std::env::remove_var("LATTE_MAX_DELEGATES_PER_SESSION");
        assert_eq!(default_max_delegates(), 0);
        // env 非法 → 回退默认。
        std::env::set_var("LATTE_MAX_DELEGATES_PER_SESSION", "abc");
        assert_eq!(default_max_delegates(), 0);
        // env 合法 → 用之。
        std::env::set_var("LATTE_MAX_DELEGATES_PER_SESSION", "5");
        assert_eq!(default_max_delegates(), 5);
        std::env::remove_var("LATTE_MAX_DELEGATES_PER_SESSION");
    }

    /// 计数器：超限时 handler 应拒绝；`max_delegates == 0` 关闭。
    #[test]
    fn delegate_counter_rejects_over_limit() {
        // 镜像 register_delegate_tool handler 入口的计数检查逻辑。
        let counter = std::sync::atomic::AtomicU32::new(0);
        let max = 2u32;
        let check = |c: &std::sync::atomic::AtomicU32| -> Result<(), String> {
            let used = c.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if max > 0 && used > max {
                Err(format!("本 session 已用尽 {max} 次 delegate（当前 {used}）"))
            } else {
                Ok(())
            }
        };
        assert!(check(&counter).is_ok(), "第1次应通过");
        assert!(check(&counter).is_ok(), "第2次应通过");
        let err = check(&counter).unwrap_err();
        assert!(err.contains("已用尽"), "{err}");
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 3);
    }

    #[test]
    fn plan_gate_rejects_implementation_roles_allows_analysis() {
        let stage = fresh_plan_stage();
        // Normal：一切放行。
        assert!(plan_gate_rejection(&stage, "programmer").is_none());
        assert!(plan_gate_rejection(&stage, "devops").is_none());

        *stage.write() = PlanStage::PendingApproval {
            plan_id: "plan-manager-7".into(),
        };
        // 实现类角色被拒（含细分前缀），消息带 plan_id。
        for rid in ["programmer", "programmer_backend", "devops", "devops_k8s"] {
            let msg = plan_gate_rejection(&stage, rid)
                .unwrap_or_else(|| panic!("{rid} 应被阶段门拒绝"));
            assert!(msg.contains("plan-manager-7"), "{msg}");
            assert!(msg.contains("尚未获用户批准"), "{msg}");
        }
        // 分析/设计/审查类角色放行。
        for rid in ["pm", "architect", "architect_system", "designer_ui", "reviewer", "tester", "security", "tech_writer"] {
            assert!(
                plan_gate_rejection(&stage, rid).is_none(),
                "{rid} 不应被阶段门拦截"
            );
        }

        // Approved：等同 Normal，不再拦截。
        *stage.write() = PlanStage::Approved {
            plan_id: "plan-manager-7".into(),
        };
        assert!(plan_gate_rejection(&stage, "programmer").is_none());
    }

    #[tokio::test]
    async fn plan_tool_call_sets_pending_approval_stage() {
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        let (event_tx, mut rx) = broadcast::channel(8);
        let stage = fresh_plan_stage();
        register_plan_tool(&tm, event_tx, "manager".into(), stage.clone(), std::path::Path::new("."), String::new())
            .expect("register plan");

        let out = tm
            .execute(
                "plan",
                serde_json::json!({
                    "tasks": [{ "title": "实现 ringbuf 核心读写" }]
                }),
                None,
            )
            .await
            .expect("plan tool call");
        let text = out.as_str().expect("string result");
        assert!(text.contains("plan_id=plan-manager-"), "{text}");

        // 事件与阶段门一致：同一 plan_id。
        let ev = rx.try_recv().expect("PlanProposed event");
        let ChatEvent::PlanProposed { plan_id, .. } = ev else {
            panic!("expected PlanProposed, got {ev:?}");
        };
        assert_eq!(
            stage.read().clone(),
            PlanStage::PendingApproval {
                plan_id: plan_id.clone()
            }
        );
    }

    // ─── slash 路径自动提案（extract_plan_tasks / propose_plan_from_summary）──

    #[test]
    fn extract_plan_tasks_reads_json_fence_with_surrounding_text() {
        // gate 通过时的典型产出：VERDICT + 标题 + ```json 代码块。
        let summary = "VERDICT: PASS\n\n【任务清单】\n```json\n{\n  \"tasks\": [\n    {\"title\": \"D1: 理解 stats_print\", \"description\": \"阅读\", \"priority\": 1, \"labels\": [\"学习\"], \"workflow\": \"\", \"subtasks\": [{\"title\": \"D1.1\"}]}\n  ]\n}\n```\n";
        let tasks = extract_plan_tasks(summary).expect("应提取到任务清单");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "D1: 理解 stats_print");
        assert_eq!(tasks[0].subtasks.len(), 1);
    }

    #[test]
    fn extract_plan_tasks_returns_none_without_valid_task_list() {
        // 无代码块 / 非 tasks JSON / 空清单 / 空 title → 都不提案。
        assert!(extract_plan_tasks("VERDICT: PASS，没有代码块").is_none());
        assert!(extract_plan_tasks("```json\n{\"foo\": 1}\n```").is_none());
        assert!(extract_plan_tasks("```json\n{\"tasks\": []}\n```").is_none());
        assert!(extract_plan_tasks("```json\n{\"tasks\": [{\"title\": \"  \"}]}\n```").is_none());
        assert!(extract_plan_tasks("```json\n这不是 JSON\n```").is_none());
    }

    #[tokio::test]
    async fn propose_plan_from_summary_broadcasts_and_sets_stage() {
        let (event_tx, mut rx) = broadcast::channel(8);
        let stage = fresh_plan_stage();
        let summary = "VERDICT: PASS\n```json\n{\"tasks\": [{\"title\": \"任务甲\"}, {\"title\": \"任务乙\"}]}\n```";
        assert!(propose_plan_from_summary(&event_tx, &stage, "manager", summary, None));

        let ev = rx.try_recv().expect("PlanProposed event");
        let ChatEvent::PlanProposed { role_id, plan_id, tasks } = ev else {
            panic!("expected PlanProposed, got {ev:?}");
        };
        assert_eq!(role_id, "manager");
        assert_eq!(tasks.len(), 2);
        assert_eq!(
            stage.read().clone(),
            PlanStage::PendingApproval { plan_id }
        );

        // 无任务清单的产出：不提案、不动阶段门（复位后验证）。
        *stage.write() = PlanStage::Normal;
        assert!(!propose_plan_from_summary(&event_tx, &stage, "manager", "没有任何清单", None));
        assert!(rx.try_recv().is_err(), "不应再发事件");
        assert_eq!(stage.read().clone(), PlanStage::Normal);
    }

    // ─── ask 阻塞模式（workflow/delegate 子代理）──

    /// 优缺点清单的宽松解析：模型的写法五花八门，任何一种都不该让
    /// 整次提问失败，也不该静默丢掉优缺点（「详情」按钮会没内容）。
    #[test]
    fn choice_option_parses_lenient_pros_cons_and_aliases() {
        // 标准形态：pros/cons 裸字符串数组。
        let o: ChoiceOption = serde_json::from_value(serde_json::json!({
            "label": "JWT",
            "pros": ["无状态", "客户端友好"],
            "cons": ["撤销难"],
            "details": "适合 API 客户端。"
        }))
        .expect("standard shape");
        assert_eq!(o.pros, vec!["无状态", "客户端友好"]);
        assert_eq!(o.cons, vec!["撤销难"]);
        assert_eq!(o.details, "适合 API 客户端。");

        // 别名 + 长字符串（换行/分号分隔）+ 项目符号前缀。
        let o: ChoiceOption = serde_json::from_value(serde_json::json!({
            "label": "OAuth2",
            "advantages": "- 委托授权\n- 生态成熟；标准化",
            "risks": [{"text": "实现复杂"}, {"text": "依赖外部 IdP"}],
            "rationale": "对接第三方身份提供方时首选。"
        }))
        .expect("alias shape");
        assert_eq!(o.pros, vec!["委托授权", "生态成熟", "标准化"]);
        assert_eq!(o.cons, vec!["实现复杂", "依赖外部 IdP"]);
        assert_eq!(o.details, "对接第三方身份提供方时首选。");

        // 无法识别的形态退化成空清单，不报错（弹框照样能弹）。
        let o: ChoiceOption = serde_json::from_value(serde_json::json!({
            "label": "Session",
            "pros": 42,
            "cons": null
        }))
        .expect("garbage must not fail the whole ask");
        assert!(o.pros.is_empty() && o.cons.is_empty());

        // 缺省时不序列化，前端拿不到多余空字段。
        let bare = serde_json::to_value(ChoiceOption {
            label: "裸选项".into(),
            ..Default::default()
        })
        .expect("serialize");
        let map = bare.as_object().expect("object");
        assert!(!map.contains_key("pros"));
        assert!(!map.contains_key("cons"));
        assert!(!map.contains_key("details"));

        // `detail` 仍归 description（历史别名），别被 `details` 抢走。
        let o: ChoiceOption = serde_json::from_value(serde_json::json!({
            "label": "x", "detail": "一行说明"
        }))
        .expect("legacy detail alias");
        assert_eq!(o.description, "一行说明");
        assert!(o.details.is_empty());
    }

    /// 弹框事件必须把 pros/cons 透传给前端（「详情」面板的数据源）。
    #[tokio::test]
    async fn ask_event_carries_pros_cons_and_multi() {
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        let (event_tx, mut rx) = broadcast::channel(8);
        register_ask_tool(&tm, event_tx, "architect".into(), None, None).expect("register ask");

        let input = serde_json::json!({
            "question": "鉴权方案？",
            "multiSelect": "true",   // 宽松布尔 + 别名
            "options": [
                {"label": "JWT", "pros": ["无状态"], "cons": ["撤销难"]},
                {"label": "Session", "desc": "服务端存会话"}
            ]
        });
        tm.execute("ask", input, None).await.expect("ask call");

        let ev = rx.try_recv().expect("ChoiceRequested event");
        let ChatEvent::ChoiceRequested { multi, options, .. } = ev else {
            panic!("expected ChoiceRequested, got {ev:?}");
        };
        assert!(multi, "multiSelect:\"true\" 应识别为多选");
        assert_eq!(options[0].pros, vec!["无状态"]);
        assert_eq!(options[0].cons, vec!["撤销难"]);
        assert_eq!(options[1].description, "服务端存会话");
    }

    /// 事故回放（tutor / interview 步）：模型把多选意图只写进
    /// question 文案（`你最想深入哪条线？（可多选）`），args 里完全没有
    /// `multi` 键 → 缺省 false → 前端按 radio 渲染，用户看着"可多选"
    /// 却只能点一个。文案兜底必须把这种情形识别成多选。
    #[tokio::test]
    async fn ask_infers_multi_from_question_text() {
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        let (event_tx, mut rx) = broadcast::channel(8);
        register_ask_tool(&tm, event_tx, "tutor".into(), None, None).expect("register ask");

        // 与线上 args 同形：只有 question + options，无任何 multi 键。
        let input = serde_json::json!({
            "question": "你最想深入哪条线？（可多选）",
            "options": [
                {"label": "6 函数热路径 + tcache"},
                {"label": "arena / bin / extent 三层"},
                {"label": "stats + 调优 cookbook"},
                {"label": "HPA 新机制（5.3 新增）"}
            ]
        });
        tm.execute("ask", input, None).await.expect("ask call");

        let ev = rx.try_recv().expect("ChoiceRequested event");
        let ChatEvent::ChoiceRequested { multi, options, .. } = ev else {
            panic!("expected ChoiceRequested, got {ev:?}");
        };
        assert!(multi, "question 写了「（可多选）」就必须按多选渲染");
        assert_eq!(options.len(), 4);
    }

    /// 文案没有任何多选标记时保持单选（回归保护：兜底不能把所有提问
    /// 都变成多选）。
    #[tokio::test]
    async fn ask_stays_single_select_without_multi_markers() {
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        let (event_tx, mut rx) = broadcast::channel(8);
        register_ask_tool(&tm, event_tx, "tutor".into(), None, None).expect("register ask");

        tm.execute("ask", ask_input(), None).await.expect("ask call");
        let ev = rx.try_recv().expect("ChoiceRequested event");
        let ChatEvent::ChoiceRequested { multi, .. } = ev else {
            panic!("expected ChoiceRequested, got {ev:?}");
        };
        assert!(!multi, "没有多选标记应保持单选");
    }

    /// 文案推断的边界：否定标记优先（"不可多选" 自身含 "多选" 子串，
    /// 不先排除会被正向标记误命中）。
    #[test]
    fn question_implies_multi_marker_table() {
        for q in [
            "你最想深入哪条线？（可多选）",
            "选哪些模块？可以多选",
            "支持多选：要覆盖哪些场景",
            "可勾选多项",
            "Which areas? (multi-select)",
            "Pick options — select all that apply",
        ] {
            assert!(question_implies_multi(q), "应判为多选: {q}");
        }
        for q in [
            "选哪个方案？",
            "你打算投入多少时间？",
            "选一个方向（不可多选）",
            "只能单选：主力模型是哪个",
            "只能选一个",
            "这里非多选",
        ] {
            assert!(!question_implies_multi(q), "应判为单选: {q}");
        }
    }

    fn ask_input() -> serde_json::Value {
        serde_json::json!({
            "question": "选哪个方案？",
            "options": [{"label": "方案A"}, {"label": "方案B"}]
        })
    }

    #[tokio::test]
    async fn ask_fire_and_forget_returns_immediately_with_wait_false() {
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        let (event_tx, mut rx) = broadcast::channel(8);
        register_ask_tool(&tm, event_tx, "manager".into(), None, None).expect("register ask");

        let out = tm.execute("ask", ask_input(), None).await.expect("ask call");
        let text = out.as_str().expect("string result");
        assert!(text.contains("结束本轮"), "{text}");

        let ev = rx.try_recv().expect("ChoiceRequested event");
        let ChatEvent::ChoiceRequested { wait, .. } = ev else {
            panic!("expected ChoiceRequested, got {ev:?}");
        };
        assert!(!wait, "顶层 fire-and-forget 必须 wait=false");
    }

    #[tokio::test]
    async fn ask_blocking_waits_and_returns_user_answer() {
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        let (event_tx, mut rx) = broadcast::channel(8);
        register_ask_tool(
            &tm,
            event_tx,
            "manager".into(),
            Some(AskBlocking {
                cancel_flag: None,
                agent_pause_gate: None,
                answer_log: None,
                role_id: "manager".into(),
            }),
            None,
        )
        .expect("register ask");

        let tm2 = tm.clone();
        let call = tokio::spawn(async move { tm2.execute("ask", ask_input(), None).await });

        // 事件必须带 wait=true；拿到 choice_id 后模拟 UI 提交答案。
        let choice_id = loop {
            let ev = rx.recv().await.expect("event");
            if let ChatEvent::ChoiceRequested { choice_id, wait, .. } = ev {
                assert!(wait, "阻塞 ask 必须 wait=true");
                break choice_id;
            }
        };
        assert!(crate::choice::resolve(&choice_id, "方案A".to_string()));

        let out = call.await.expect("join").expect("ask call");
        let text = out.as_str().expect("string result");
        assert!(text.contains("选择：方案A"), "{text}");
        assert!(text.contains("不要重复提问"), "{text}");
    }

    /// 编排类工具必须显式带 `ORCHESTRATION_TOOL_TIMEOUT_SECS`，不能吃
    /// 工具管理器 25min 默认熔断。`ask` 是本测试能低成本注册的代表；
    /// `workflow` / `delegate` 用同一常量（漏配的后果见常量文档：
    /// 25min 熔断 + 孤儿 step + resume 空等 50 分钟）。
    #[tokio::test]
    async fn orchestration_tools_opt_out_of_manager_default_timeout() {
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        let (event_tx, _rx) = broadcast::channel(8);
        register_ask_tool(
            &tm,
            event_tx,
            "manager".into(),
            Some(AskBlocking {
                cancel_flag: None,
                agent_pause_gate: None,
                answer_log: None,
                role_id: "manager".into(),
            }),
            None,
        )
        .expect("register ask");

        let tool = tm.get_tool("ask").expect("ask 已注册");
        assert_eq!(
            tool.timeout,
            Some(std::time::Duration::from_secs(ORCHESTRATION_TOOL_TIMEOUT_SECS)),
            "阻塞 ask 必须显式声明超时，否则被工具管理器默认值熔断"
        );
        // 25min（工具管理器默认）远小于本值——回归防线：一旦有人把
        // 常量改小到默认值量级，这条会炸。
        assert!(
            tool.timeout.expect("timeout") > std::time::Duration::from_secs(1500),
            "编排工具超时不得回落到 25min 量级"
        );
    }

    /// 宽松布尔单元表：模型把 boolean 发成字符串/0-1 是常态。
    #[test]
    fn lenient_bool_accepts_model_dialects() {
        use serde_json::json;
        for v in [json!(true), json!("true"), json!("True"), json!("  TRUE "),
                  json!("yes"), json!("1"), json!("on"), json!(1)] {
            assert_eq!(lenient_bool(&v), Some(true), "should be true: {v}");
        }
        for v in [json!(false), json!("false"), json!("FALSE"), json!("no"),
                  json!("0"), json!("off"), json!(0)] {
            assert_eq!(lenient_bool(&v), Some(false), "should be false: {v}");
        }
        // 无法识别的形态 → None（调用点自行决定默认值）。
        for v in [json!("maybe"), json!(2), json!(-1), json!(1.5), json!(null),
                  json!([]), json!({})] {
            assert_eq!(lenient_bool(&v), None, "should be None: {v}");
        }
    }

    /// 事故回放（tutor / interview 步）：`recommended` 发成
    /// 字符串 `"true"`，此前 serde 严格解析报
    /// `options[0] invalid: invalid type: string "true", expected a
    /// boolean` → PermanentExec 不重试 → 选择框根本没弹出来。
    #[tokio::test]
    async fn ask_accepts_string_bool_in_option_recommended() {
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        let (event_tx, mut rx) = broadcast::channel(8);
        register_ask_tool(&tm, event_tx, "tutor".into(), None, None).expect("register ask");

        let input = serde_json::json!({
            "question": "第 4 题：你最想深入的方向？",
            "options": [
                {"label": "核心分配链路", "description": "main.c → cache", "recommended": "true"},
                {"label": "extent 内存池与 rtree", "description": "edata 元数据"},
            ]
        });
        tm.execute("ask", input, None)
            .await
            .expect("字符串布尔不该让 ask 失败");

        let ev = rx.try_recv().expect("ChoiceRequested event");
        let ChatEvent::ChoiceRequested { options, .. } = ev else {
            panic!("expected ChoiceRequested");
        };
        assert!(options[0].recommended, "\"true\" 必须解析成 true");
        assert!(!options[1].recommended, "缺省仍是 false");
    }

    /// 无法识别的 `recommended` 形态只丢角标，不让提问失败。
    #[tokio::test]
    async fn ask_tolerates_garbage_recommended_without_failing() {
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        let (event_tx, mut rx) = broadcast::channel(8);
        register_ask_tool(&tm, event_tx, "tutor".into(), None, None).expect("register ask");

        let input = serde_json::json!({
            "question": "选哪个？",
            "options": [
                {"label": "A", "recommended": "大概吧"},
                {"label": "B", "recommended": 7},
            ]
        });
        tm.execute("ask", input, None).await.expect("垃圾值不该失败");
        let ChatEvent::ChoiceRequested { options, .. } = rx.try_recv().expect("event") else {
            panic!("expected ChoiceRequested");
        };
        assert!(!options[0].recommended && !options[1].recommended);
    }

    /// 走真实管线：`coerce_tool_input_to_schema`（runner 在
    /// `tm.execute` 前调用）→ execute。顶层 `multi`/`options` 由工具
    /// 管理器的 schema 校验先拦，所以纠正必须发生在 execute 之前，
    /// handler 里再宽松也没用。
    async fn ask_via_pipeline(
        tm: &Arc<dyn latte_rs_agent_tools::types::ToolManager>,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, latte_rs_agent_tools::error::ToolError> {
        let schema = tm.get_tool("ask").expect("ask 已注册").input_schema;
        let coerced = crate::agent::coerce_tool_input_to_schema(input, &schema);
        tm.execute("ask", coerced, None).await
    }

    /// 事故回放（programmer）：模型把 options 包进
    /// `{"item":[...]}`、每项用 `desc` 而不是 `description`、
    /// 多选键写成 `"multiSelect":"false"`。原始报错是
    /// `Tool not found: ask`（角色没这个工具），补上工具后紧接着就会
    /// 撞上 `Property 'options' must be Array, got object`。
    #[tokio::test]
    async fn ask_normalizes_wrapped_options_and_aliases() {
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        let (event_tx, mut rx) = broadcast::channel(8);
        register_ask_tool(&tm, event_tx, "programmer".into(), None, None).expect("register ask");

        let input = serde_json::json!({
            "question": "归档份数：你希望按哪种粒度归档？",
            "options": {"item": [
                {"label": "归档 5 份（推荐）", "desc": "把 5 份过期报告全部 git mv"},
                {"label": "只归档 Architect 指定的 3 份", "desc": "贴着设计走"},
            ]},
            "multi": "false"
        });
        ask_via_pipeline(&tm, input).await.expect("包一层的 options 应被解开");

        let ChatEvent::ChoiceRequested { options, multi, .. } = rx.try_recv().expect("event") else {
            panic!("expected ChoiceRequested");
        };
        assert!(!multi, "\"false\" 字符串必须解析成 false");
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].label, "归档 5 份（推荐）");
        assert_eq!(
            options[0].description, "把 5 份过期报告全部 git mv",
            "desc 别名必须落到 description，不能被静默丢弃"
        );
    }

    /// `"multi": "true"` / `"allow_upload": 1` 走宽松布尔。
    #[tokio::test]
    async fn ask_accepts_string_bool_for_multi_and_allow_upload() {
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        let (event_tx, mut rx) = broadcast::channel(8);
        register_ask_tool(&tm, event_tx, "manager".into(), None, None).expect("register ask");

        let input = serde_json::json!({
            "question": "多选题",
            "options": [{"label": "A"}, {"label": "B"}],
            "multi": "true",
            "allow_upload": 1
        });
        ask_via_pipeline(&tm, input).await.expect("ask call");
        let ChatEvent::ChoiceRequested { multi, allow_upload, .. } =
            rx.try_recv().expect("event")
        else {
            panic!("expected ChoiceRequested");
        };
        assert!(multi, "\"true\" → multi");
        assert!(allow_upload, "1 → allow_upload");
    }

    /// options 是对象但含多个数组时无法判定 —— 不瞎猜键名，让 schema
    /// 校验照常报错，模型拿到反馈自己修。
    #[tokio::test]
    async fn ask_rejects_ambiguous_options_object() {
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        let (event_tx, _rx) = broadcast::channel(8);
        register_ask_tool(&tm, event_tx, "manager".into(), None, None).expect("register ask");

        let input = serde_json::json!({
            "question": "q",
            "options": {"a": [{"label": "A"}, {"label": "B"}], "b": [{"label": "C"}]}
        });
        let err = ask_via_pipeline(&tm, input)
            .await
            .expect_err("多个候选数组必须报错");
        assert!(err.to_string().contains("schema validation"), "{err}");
    }

    /// 纠正器只按 schema 声明动手，绝不碰未声明字段，也不猜无法识别
    /// 的值 —— 该报的校验错还得报，模型才拿得到反馈。
    #[tokio::test]
    async fn coercion_leaves_undeclared_and_unrecognized_values_alone() {
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        let (event_tx, _rx) = broadcast::channel(8);
        register_ask_tool(&tm, event_tx, "manager".into(), None, None).expect("register ask");
        let schema = tm.get_tool("ask").expect("ask").input_schema;

        let out = crate::agent::coerce_tool_input_to_schema(
            serde_json::json!({
                "question": "q",
                "options": [{"label": "A"}, {"label": "B"}],
                "multi": "大概吧",
                "未声明字段": "true",
            }),
            &schema,
        );
        assert_eq!(out["multi"], "大概吧", "识别不了的布尔值原样保留");
        assert_eq!(out["未声明字段"], "true", "未声明字段不得被改写");
        assert!(out["options"].is_array(), "已经是数组的不动");
        // question 是 String 声明，字符串照旧。
        assert_eq!(out["question"], "q");
    }

    /// 派发幂等：同一 (role, task) 的第二次 delegate 直接复用上次结果，
    /// **不再打模型**、不再产生副作用。
    ///
    /// 此前没有任何一道闸管这件事：`dedupe_native_tool_calls` 只管同一
    /// 条响应内；`LoopDetector` 每轮重建、阈值是同轮连续 3 次；
    /// `Supervisor` 看的是被压成不带参数的字符串 `"delegate"`（既误伤
    /// 连续 3 轮的合法分工，又看不见同轮重复）。一次委派几十秒到十几
    /// 分钟，白跑一遍是实打实的代价。
    #[tokio::test]
    async fn duplicate_delegate_reuses_previous_result_without_calling_model_again() {
        use crate::config::{ModelCatalog, ModelDef};
        use crate::role::RoleTemplate;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let body = serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 0,
            "model": "test",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "专家结论：这段逻辑应当收敛到单一入口，理由与证据如下，长度足够越过 advisor 的短输出阈值。" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
        })
        .to_string();

        let server = wiremock::MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(body)),
            )
            .await;

        let merged = AgentConfig {
            advisor: Default::default(),
            models: ModelCatalog {
                models: vec![ModelDef {
                    name: "stub".into(),
                    api: "openai".into(),
                    provider: "test".into(),
                    base_url: server.uri(),
                    api_key: "test-key".into(),
                    context_window: 32000,
                    max_tokens: 4096,
                    supports_thinking: false,
                    supports_vision: false,
                    supports_image_generation: false,
                    cost_per_million_input: None,
                    cost_per_million_output: None,
                    tier: Some("standard".into()),
                    timeout_secs: None,
                }],
                tiers: None,
                role_tiers: None,
            },
            roles: [(
                "programmer".to_string(),
                RoleTemplate {
                    id: "programmer".into(),
                    name: "programmer".into(),
                    category: "execution".into(),
                    model_tier: "standard".into(),
                    model_chain: vec![],
                    prompt_file: None,
                    temperature: None,
                    tools: vec![],
                    icon: String::new(),
                    skills: vec![],
                    code_paths: vec![],
                },
            )]
            .into_iter()
            .collect(),
        };
        let resolver = ModelResolver::from_config(&merged).expect("resolver");
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        let (event_tx, mut rx) = broadcast::channel(64);
        register_delegate_tool(
            &tm,
            &merged,
            &resolver,
            GenerateParams::default(),
            event_tx,
            std::path::PathBuf::from("/tmp"),
            Arc::new(crate::subsession::SubsessionStore::new()),
            "ui-test".into(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            None,
            fresh_plan_stage(),
            crate::pause_gate::AgentPauseGate::new("test"),
            Arc::new(std::sync::atomic::AtomicU32::new(0)),
            0,
            Arc::new(parking_lot::Mutex::new("测试主诉求".to_string())),
        )
        .await
        .expect("register delegate");

        let args = serde_json::json!({ "role": "programmer", "task": "收敛重复入口" });

        let first = tm
            .execute("delegate", args.clone(), None)
            .await
            .expect("首次派发应成功");
        let calls_after_first = server.received_requests().await.unwrap().len();
        assert!(calls_after_first >= 1, "首次必须真的打了模型");

        // 第二次同身份派发：结果一致，且模型请求数**没有增加**。
        let second = tm
            .execute("delegate", args.clone(), None)
            .await
            .expect("重复派发应直接复用，不该报错");
        assert_eq!(first, second, "复用的结果必须与首次一致");
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            calls_after_first,
            "重复派发不得再打模型"
        );

        // 复用要对用户可见（Status），且**不能**再发一对
        // DelegateStarted/Finished（否则 UI 上凭空多一条分派记录）。
        let mut started = 0usize;
        let mut reuse_notices = 0usize;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                ChatEvent::DelegateStarted { .. } => started += 1,
                ChatEvent::Status { message } if message.contains("复用已完成的委派结果") => {
                    reuse_notices += 1
                }
                _ => {}
            }
        }
        assert_eq!(started, 1, "只应有首次派发的 DelegateStarted");
        assert_eq!(reuse_notices, 1, "复用要发一条 Status 让用户看见");

        // 换一个 task → 不该被挡住（会真的再打模型）。
        let other = tm
            .execute(
                "delegate",
                serde_json::json!({ "role": "programmer", "task": "另一件事" }),
                None,
            )
            .await
            .expect("不同任务应正常派发");
        assert!(
            server.received_requests().await.unwrap().len() > calls_after_first,
            "不同任务必须真的派发出去"
        );
        assert!(!other.as_str().unwrap_or_default().is_empty());
    }

    /// 阻塞 ask 收到答案必须**立刻**落盘，然后同一问题再问时直接回放
    /// —— 不再弹第二个选择框，用户不用重答。
    ///
    /// 实测形状：design_and_plan 的 interview 步连问 3-4 题，
    /// 只要该 step 后续任何环节挂了（契约不合格 / 空产出 / advisor 拦 /
    /// 预算超支 / 模型报错），契约重试是**全新 subagent**、resume 又只
    /// 跳过已完成的 step —— 两条路都会把同一批问题重新弹一遍。
    #[tokio::test]
    async fn blocking_ask_records_answer_and_replays_it_without_second_dialog() {
        let dir = tempfile::tempdir().unwrap();
        let log = std::sync::Arc::new(crate::workflow::AnswerLog::new(
            dir.path(),
            "wf-replay-1",
            None,
            None,
            Default::default(),
        ));
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        let (event_tx, mut rx) = broadcast::channel(32);
        register_ask_tool(
            &tm,
            event_tx,
            "tutor".into(),
            Some(AskBlocking {
                cancel_flag: None,
                agent_pause_gate: None,
                answer_log: Some(log.clone()),
                role_id: "tutor".into(),
            }),
            None,
        )
        .expect("register ask");

        let input = serde_json::json!({
            "question": "第 1 题：你最终想达成什么？",
            "options": [{"label": "学习代码机制"}, {"label": "改源码/做实现"}]
        });

        // 第一次：正常弹框 + 用户作答。
        let tm2 = tm.clone();
        let inp2 = input.clone();
        let call = tokio::spawn(async move { tm2.execute("ask", inp2, None).await });
        let choice_id = loop {
            if let ChatEvent::ChoiceRequested { choice_id, .. } = rx.recv().await.expect("event") {
                break choice_id;
            }
        };
        assert!(crate::choice::resolve(&choice_id, "改源码/做实现".to_string()));
        let first = call.await.expect("join").expect("ask ok");
        assert!(first.as_str().expect("str").contains("改源码/做实现"));

        // 落盘即时性：此刻还没有任何 step 完成，答案已经可回放。
        assert_eq!(
            log.recall("第 1 题：你最终想达成什么？").as_deref(),
            Some("改源码/做实现")
        );

        // 第二次（模拟 step 重试后的全新 subagent 又问同一题）：
        // 必须直接返回旧答案，且**不再**发 ChoiceRequested。
        while rx.try_recv().is_ok() {} // 清空历史事件
        let second = tm
            .execute("ask", input, None)
            .await
            .expect("重放不该失败");
        let text = second.as_str().expect("str");
        assert!(text.contains("此前已回答"), "{text}");
        assert!(text.contains("改源码/做实现"), "{text}");

        let mut popped_again = false;
        let mut saw_status = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                ChatEvent::ChoiceRequested { .. } => popped_again = true,
                ChatEvent::Status { message } if message.contains("复用你之前的回答") => {
                    saw_status = true
                }
                _ => {}
            }
        }
        assert!(!popped_again, "同一问题不得再弹一次选择框让用户重答");
        assert!(saw_status, "回放要对用户可见，不能静默替他作答");
    }

    /// 没有台账时行为不变（顶层 turn / 独立测试）：照常弹框，不记账。
    #[tokio::test]
    async fn blocking_ask_without_answer_log_still_prompts() {
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        let (event_tx, mut rx) = broadcast::channel(8);
        register_ask_tool(
            &tm,
            event_tx,
            "tutor".into(),
            Some(AskBlocking {
                cancel_flag: None,
                agent_pause_gate: None,
                answer_log: None,
                role_id: "tutor".into(),
            }),
            None,
        )
        .expect("register ask");

        let tm2 = tm.clone();
        let call = tokio::spawn(async move {
            tm2.execute(
                "ask",
                serde_json::json!({
                    "question": "q?",
                    "options": [{"label": "A"}, {"label": "B"}]
                }),
                None,
            )
            .await
        });
        let choice_id = loop {
            if let ChatEvent::ChoiceRequested { choice_id, .. } = rx.recv().await.expect("event") {
                break choice_id;
            }
        };
        assert!(crate::choice::resolve(&choice_id, "A".to_string()));
        assert!(call.await.expect("join").is_ok());
    }

    /// programmer 必须带 `ask`：缺了它模型照样会调，拿到
    /// `Tool not found: ask` 就只能瞎猜（实测实锤：programmer
    /// 问「归档 3 份还是 5 份」，Tool not found: ask）。
    ///
    /// 两份都要查：`config/agents/`（权威模板，install.sh 装到全局）
    /// 用 `[roles.<id>]` 嵌套表，`.latte/agents/`（本项目生效副本）
    /// 是平铺表。
    #[test]
    fn programmer_role_has_ask_tool() {
        let tools_of = |raw: &str, label: &str| -> Vec<String> {
            let v: toml::Value = toml::from_str(raw).expect("合法 TOML");
            let table = v
                .get("roles")
                .and_then(|r| r.get("programmer"))
                .unwrap_or(&v);
            table
                .get("tools")
                .unwrap_or_else(|| panic!("{label} 缺 tools 字段"))
                .as_array()
                .expect("tools 应是数组")
                .iter()
                .map(|t| t.as_str().unwrap_or_default().to_string())
                .collect()
        };
        for (raw, label) in [
            (include_str!("../../config/agents/programmer.toml"), "config/agents"),
            (include_str!("../../.latte/agents/programmer.toml"), ".latte/agents"),
        ] {
            let tools = tools_of(raw, label);
            assert!(
                tools.iter().any(|t| t == "ask"),
                "{label} 的 programmer.tools 必须含 ask，实际: {tools:?}"
            );
        }
    }

    #[tokio::test]
    async fn ask_blocking_cancel_flag_aborts_wait() {
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        let (event_tx, mut rx) = broadcast::channel(8);
        let cancel = Arc::new(AtomicBool::new(false));
        register_ask_tool(
            &tm,
            event_tx,
            "manager".into(),
            Some(AskBlocking {
                cancel_flag: Some(cancel.clone()),
                agent_pause_gate: None,
                answer_log: None,
                role_id: "manager".into(),
            }),
            None,
        )
        .expect("register ask");

        let tm2 = tm.clone();
        let call = tokio::spawn(async move { tm2.execute("ask", ask_input(), None).await });
        // 等提问发出再取消。
        loop {
            let ev = rx.recv().await.expect("event");
            if matches!(ev, ChatEvent::ChoiceRequested { .. }) {
                break;
            }
        }
        cancel.store(true, Ordering::SeqCst);
        let err = call.await.expect("join").expect_err("取消必须报错");
        assert!(err.to_string().contains("取消"), "{err}");
    }

    /// 一次一清单防护：上一份清单 PendingApproval 期间，第二次 plan
    /// （坏行为样本：manager 把 11 个任务分 11 次调用，用户弹窗
    /// 每次只有 1 个任务。）
    #[tokio::test]
    async fn plan_tool_rejects_second_call_while_pending() {
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        let (event_tx, mut rx) = broadcast::channel(8);
        let stage = fresh_plan_stage();
        register_plan_tool(&tm, event_tx, "manager".into(), stage.clone(), std::path::Path::new("."), String::new())
            .expect("register plan");

        tm.execute(
            "plan",
            serde_json::json!({ "tasks": [{ "title": "任务一" }] }),
            None,
        )
        .await
        .expect("first plan call");
        let err = tm
            .execute(
                "plan",
                serde_json::json!({ "tasks": [{ "title": "任务二" }] }),
                None,
            )
            .await
            .expect_err("pending 期间第二次调用必须被拒绝");
        assert!(err.to_string().contains("等待用户批准"), "{err}");

        // 只发出过一次 PlanProposed。
        let mut n = 0;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, ChatEvent::PlanProposed { .. }) {
                n += 1;
            }
        }
        assert_eq!(n, 1, "第二次调用被拒，不能重复发 PlanProposed");

        // 用户处理完（复位 Normal）后可以再提交。
        *stage.write() = PlanStage::Normal;
        tm.execute(
            "plan",
            serde_json::json!({ "tasks": [{ "title": "任务二" }] }),
            None,
        )
        .await
        .expect("复位后第二次调用应放行");
    }

    /// paths 机械校验：第一级段就不存在的路径判定为幻觉路径，整单
    /// 拒绝且**不进** PendingApproval（模型可修正后同轮重调）。
    #[tokio::test]
    async fn plan_tool_rejects_hallucinated_paths_without_pending() {
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        let (event_tx, _rx) = broadcast::channel(8);
        let stage = fresh_plan_stage();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/ringbuf")).unwrap();
        register_plan_tool(&tm, event_tx, "manager".into(), stage.clone(), dir.path(), String::new())
            .expect("register plan");

        let err = tm
            .execute(
                "plan",
                serde_json::json!({ "tasks": [{ "title": "改缓存", "paths": ["hallucinated_dir/x.rs"] }] }),
                None,
            )
            .await
            .expect_err("幻觉路径必须被拒绝");
        assert!(err.to_string().contains("hallucinated_dir"), "{err}");
        // 校验失败不算提交：仍是 Normal，修正后可立即重调。
        assert_eq!(stage.read().clone(), PlanStage::Normal);
        tm.execute(
            "plan",
            serde_json::json!({ "tasks": [{ "title": "改缓存", "paths": ["src/ringbuf/x.rs"] }] }),
            None,
        )
        .await
        .expect("修正后应放行");
    }

    /// 新建文件路径放行：叶子不存在但祖先目录存在（paths 指向要
    /// 创建的新文件是合法场景）。
    #[tokio::test]
    async fn plan_tool_allows_new_file_under_existing_ancestor() {
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        let (event_tx, _rx) = broadcast::channel(8);
        let stage = fresh_plan_stage();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        register_plan_tool(&tm, event_tx, "manager".into(), stage.clone(), dir.path(), String::new())
            .expect("register plan");

        tm.execute(
            "plan",
            serde_json::json!({ "tasks": [{ "title": "新建模块", "paths": ["src/new_mod.rs"] }] }),
            None,
        )
        .await
        .expect("祖先存在的待新建路径应放行");
        assert!(matches!(*stage.read(), PlanStage::PendingApproval { .. }));
    }

    /// 清单内 paths 前缀重叠（含相等）→ 整单拒绝并列出冲突对。
    #[tokio::test]
    async fn plan_tool_rejects_overlapping_paths_within_plan() {
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        let (event_tx, _rx) = broadcast::channel(8);
        let stage = fresh_plan_stage();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/ringbuf")).unwrap();
        std::fs::create_dir_all(dir.path().join("src/cache")).unwrap();
        register_plan_tool(&tm, event_tx, "manager".into(), stage.clone(), dir.path(), String::new())
            .expect("register plan");

        let err = tm
            .execute(
                "plan",
                serde_json::json!({ "tasks": [
                    { "title": "任务A", "paths": ["src/ringbuf"] },
                    { "title": "任务B", "paths": ["src/ringbuf/read.rs"] },
                    { "title": "任务C", "paths": ["src/cache"] }
                ]}),
                None,
            )
            .await
            .expect_err("清单内重叠必须被拒绝");
        let msg = err.to_string();
        // 反馈按**热点路径**聚合（不再罗列任务对）：tool_result 只有
        // 256 字节，罗列式清单会被截断，模型看不全就改不动。
        assert!(msg.contains("src/ringbuf"), "{msg}");
        assert!(msg.contains("paths 范围重叠"), "{msg}");
        assert!(!msg.contains("src/cache"), "不该牵连无重叠的任务范围: {msg}");
        assert!(msg.len() <= 256, "错误必须塞进 256 字节: {msg}");
        assert_eq!(stage.read().clone(), PlanStage::Normal);

        // 互不重叠则放行。
        tm.execute(
            "plan",
            serde_json::json!({ "tasks": [
                { "title": "任务A", "paths": ["src/ringbuf"] },
                { "title": "任务C", "paths": ["src/cache"] }
            ]}),
            None,
        )
        .await
        .expect("互不重叠应放行");
    }

    /// 空 paths = 未声明范围：不做校验直接过（与现状语义一致）。
    #[tokio::test]
    async fn plan_tool_skips_validation_for_undeclared_paths() {
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        let (event_tx, _rx) = broadcast::channel(8);
        let stage = fresh_plan_stage();
        let dir = tempfile::tempdir().unwrap();
        register_plan_tool(&tm, event_tx, "manager".into(), stage.clone(), dir.path(), String::new())
            .expect("register plan");

        tm.execute(
            "plan",
            serde_json::json!({ "tasks": [{ "title": "无范围任务" }] }),
            None,
        )
        .await
        .expect("空 paths 不应触发校验");
    }

    #[tokio::test]
    async fn delegate_tool_gate_blocks_programmer_allows_pm() {
        use crate::config::ModelCatalog;
        use crate::role::RoleTemplate;

        let mk_role = |id: &str| RoleTemplate {
            id: id.into(),
            name: id.into(),
            category: "engineering".into(),
            model_tier: "standard".into(),
            model_chain: vec![],
            prompt_file: None,
            temperature: None,
            tools: vec![],
            icon: String::new(),
            skills: vec![],
            code_paths: vec![],
        };
        let merged = AgentConfig {
            advisor: Default::default(),
            models: ModelCatalog {
                models: vec![],
                tiers: None,
                role_tiers: None,
            },
            roles: [
                ("programmer".to_string(), mk_role("programmer")),
                ("pm".to_string(), mk_role("pm")),
            ]
            .into_iter()
            .collect(),
        };
        let resolver = ModelResolver::from_config(&merged).expect("resolver");
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        let (event_tx, _rx) = broadcast::channel(8);
        let stage = fresh_plan_stage();
        *stage.write() = PlanStage::PendingApproval {
            plan_id: "plan-manager-3".into(),
        };
        register_delegate_tool(
            &tm,
            &merged,
            &resolver,
            GenerateParams::default(),
            event_tx,
            std::path::PathBuf::from("/tmp"),
            Arc::new(crate::subsession::SubsessionStore::new()),
            "ui-test".into(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            None,
            stage.clone(),
            crate::pause_gate::AgentPauseGate::new("test"),
            // fresh counter + max=0（本测试只验阶段门，无 delegate 计数）。
            Arc::new(std::sync::atomic::AtomicU32::new(0)),
            0,
            Arc::new(parking_lot::Mutex::new("测试主诉求".to_string())),
        )
        .await
        .expect("register delegate");

        // 实现类角色：被阶段门拒绝（中文错误 + plan_id），且拒绝发生在
        // 模型解析之前（配置里没有任何模型，若穿透门禁会先报模型错误）。
        let err = tm
            .execute(
                "delegate",
                serde_json::json!({ "role": "programmer", "task": "改代码" }),
                None,
            )
            .await
            .expect_err("programmer 应被门禁拒绝");
        let msg = err.to_string();
        assert!(msg.contains("尚未获用户批准"), "{msg}");
        assert!(msg.contains("plan-manager-3"), "{msg}");

        // 分析类角色：穿透门禁（后续因无模型而失败，但绝不是门禁错误）。
        let err = tm
            .execute(
                "delegate",
                serde_json::json!({ "role": "pm", "task": "写 PRD" }),
                None,
            )
            .await
            .expect_err("无模型时 pm 也会在后续步骤失败");
        assert!(
            !err.to_string().contains("尚未获用户批准"),
            "pm 不应被阶段门拦截: {err}"
        );

        // 批准后实现类同样穿透门禁（失败于模型解析而非门禁）。
        *stage.write() = PlanStage::Approved {
            plan_id: "plan-manager-3".into(),
        };
        let err = tm
            .execute(
                "delegate",
                serde_json::json!({ "role": "programmer", "task": "改代码" }),
                None,
            )
            .await
            .expect_err("无模型时 programmer 也会在后续步骤失败");
        assert!(
            !err.to_string().contains("尚未获用户批准"),
            "Approved 后不应再拦截: {err}"
        );
    }

    #[tokio::test]
    async fn delegate_tool_aborts_runaway_specialist_on_timeout() {
        use crate::config::{ModelCatalog, ModelDef};
        use crate::role::RoleTemplate;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        // 模型端永不返回（30s 延迟），model.timeout_secs=1 → delegate
        // 必须在 ~1s 被熔断：tool error + DelegateFinished status=timeout。
        // 回归：实测事故前 controller delegate 无 wall-clock 超时，
        // 失控子代理把父 workflow 吊死 24min。
        let server = wiremock::MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .set_delay(std::time::Duration::from_secs(30))
                            .set_body_string("{}"),
                    ),
            )
            .await;

        let merged = AgentConfig {
            advisor: Default::default(),
            models: ModelCatalog {
                models: vec![ModelDef {
                    name: "stub-slow".into(),
                    api: "openai".into(),
                    provider: "test".into(),
                    base_url: server.uri(),
                    api_key: "test-key".into(),
                    context_window: 32000,
                    max_tokens: 4096,
                    supports_thinking: false,
                    supports_vision: false,
                    supports_image_generation: false,
                    cost_per_million_input: None,
                    cost_per_million_output: None,
                    tier: Some("standard".into()),
                    timeout_secs: Some(1),
                }],
                tiers: None,
                role_tiers: None,
            },
            roles: [(
                "programmer".to_string(),
                RoleTemplate {
                    id: "programmer".into(),
                    name: "programmer".into(),
                    category: "execution".into(),
                    model_tier: "standard".into(),
                    model_chain: vec![],
                    prompt_file: None,
                    temperature: None,
                    tools: vec![],
                    icon: String::new(),
                    skills: vec![],
                    code_paths: vec![],
                },
            )]
            .into_iter()
            .collect(),
        };
        let resolver = ModelResolver::from_config(&merged).expect("resolver");
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        let (event_tx, mut rx) = broadcast::channel(16);
        register_delegate_tool(
            &tm,
            &merged,
            &resolver,
            GenerateParams::default(),
            event_tx,
            std::path::PathBuf::from("/tmp"),
            Arc::new(crate::subsession::SubsessionStore::new()),
            "ui-test".into(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            None,
            fresh_plan_stage(),
            crate::pause_gate::AgentPauseGate::new("test"),
            Arc::new(std::sync::atomic::AtomicU32::new(0)),
            0,
            Arc::new(parking_lot::Mutex::new("测试主诉求".to_string())),
        )
        .await
        .expect("register delegate");

        let err = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            tm.execute(
                "delegate",
                serde_json::json!({ "role": "programmer", "task": "写一份永远写不完的文档" }),
                None,
            ),
        )
        .await
        .expect("delegate 必须在熔断后返回，而不是挂死")
        .expect_err("超时熔断应返回 tool error");
        assert!(
            err.to_string().contains("未返回"),
            "错误应说明超时中止: {err}"
        );

        // DelegateFinished status="timeout" 必须已广播（UI 依赖它收尾）。
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match rx.recv().await {
                    Ok(ChatEvent::DelegateFinished { status, .. }) => break status,
                    Ok(_) => continue,
                    Err(e) => panic!("event stream ended before DelegateFinished: {e}"),
                }
            }
        })
        .await
        .expect("DelegateFinished timeout 事件");
        assert_eq!(status, "timeout");
    }

    #[test]
    fn specialist_circuit_breaker_defaults_and_env_override() {
        // 轮次上限那半已随死代码删除（`specialist_max_tool_rounds` /
        // `DEFAULT_SPECIALIST_MAX_TOOL_ROUNDS`）：值读出来只是存进
        // `AgentRunner.max_tool_rounds`，从不被读取。现在只剩 wall-clock
        // 超时这一路熔断。
        assert_eq!(
            specialist_timeout_secs(None),
            DEFAULT_UI_DELEGATE_TIMEOUT_SECS
        );
        // per-model timeout_secs 优先级最高。
        assert_eq!(specialist_timeout_secs(Some(120)), 120);
    }

    #[tokio::test]
    async fn user_message_resets_plan_stage_to_normal() {
        use crate::config::{ModelCatalog, ModelDef};
        use crate::role::RoleTemplate;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(
                        serde_json::json!({
                            "id": "chatcmpl-test",
                            "object": "chat.completion",
                            "created": 0,
                            "model": "test",
                            "choices": [{
                                "index": 0,
                                "message": { "role": "assistant", "content": "plain reply（占位长回答：超过 advisor D5 短输出 gate 的 50 字符阈值，避免测试被 gate 重试干扰）" },
                                "finish_reason": "stop"
                            }],
                            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
                        })
                        .to_string(),
                    )),
            )
            .await;

        let agent_config = Arc::new(AgentConfig {
            advisor: Default::default(),
            models: ModelCatalog {
                models: vec![ModelDef {
                    name: "stub-standard".into(),
                    api: "openai".into(),
                    provider: "test".into(),
                    base_url: server.uri(),
                    api_key: "test-key".into(),
                    context_window: 32000,
                    max_tokens: 4096,
                    supports_thinking: false,
                    supports_vision: false,
                    supports_image_generation: false,
                    cost_per_million_input: None,
                    cost_per_million_output: None,
                    tier: Some("standard".into()),
                    timeout_secs: None,
                }],
                tiers: None,
                role_tiers: None,
            },
            roles: [(
                "manager".to_string(),
                RoleTemplate {
                    id: "manager".into(),
                    name: "manager".into(),
                    category: "planning".into(),
                    model_tier: "standard".into(),
                    model_chain: vec![],
                    prompt_file: None,
                    temperature: None,
                    tools: vec![],
                    icon: "👔".into(),
                    skills: vec![],
            code_paths: vec![],
                },
            )]
            .into_iter()
            .collect(),
        });
        let resolver = Arc::new(ModelResolver::from_config(&agent_config).unwrap());
        let dir = tempfile::tempdir().unwrap();

        let cfg = ControllerConfig {
            task_id: None,
            roles: vec!["manager".to_string()],
            initial_prompt: None,
            max_rounds: 0,
            session_token_budget: 0,
            agent_config,
            model_resolver: resolver,
            default_params: GenerateParams::default(),
            primary_model_id: None,
            initial_tier: None,
            initial_history: vec![],
            cwd: dir.path().to_path_buf(),
            subsession_store: Arc::new(crate::subsession::SubsessionStore::new()),
            advisor_monitor: AdvisorMonitorConfig::default(),
            stream_mode: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            max_delegates_per_session: crate::controller::default_max_delegates(),
            session_id: String::new(),
        };

        let controller = ChatController::new(64);
        // 新 controller 默认 Normal。
        assert_eq!(controller.plan_stage(), PlanStage::Normal);
        let mut rx = controller.spawn(cfg).await;

        // 模拟 plan 已提交待批准；下一条用户消息应复位 Normal。
        controller.set_plan_stage(PlanStage::PendingApproval {
            plan_id: "plan-manager-9".into(),
        });
        controller.submit_input("继续").await;
        loop {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
                .await
                .expect("event timeout")
                .expect("recv");
            if matches!(ev, ChatEvent::UserMessage { .. }) {
                break;
            }
        }
        assert_eq!(
            controller.plan_stage(),
            PlanStage::Normal,
            "driver 收到用户消息后阶段门必须复位 Normal"
        );

        controller.abort().await;
    }

    /// 回归测试：abort() 之后 session 必须能恢复，不能永久变哑。
    ///
    /// 旧行为（bug）：abort() 只置 `cancel_flag = true` 并发 Abort，
    /// driver break 退出后 `input_rx` 被 drop，但 `input_tx` 仍是
    /// `Some(dead_sender)`；`cancel_flag` 全代码从不复位，任何重启的
    /// driver 在循环头 `if cancel_flag.load() { break }` 立刻再退出。
    /// 结果 `/api/chat/send` 一路返回 202，SSE 连着但永远静默。
    #[tokio::test]
    async fn abort_then_respawn_revives_driver() {
        let server = stub_llm_server().await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = stub_single_role_config(server.uri(), dir.path());

        let controller = ChatController::new(64);

        // 未 spawn：driver 视为「死」（没有可投递的接收端）。
        assert!(
            controller.is_driver_dead().await,
            "spawn 之前 is_driver_dead 必须为 true"
        );

        let mut rx = controller.spawn(cfg).await;
        assert!(
            !controller.is_driver_dead().await,
            "spawn 之后 driver 必须活着"
        );

        // 第一轮：确认 driver 正常消费输入。
        controller.submit_input("第一条").await;
        wait_for_user_message(&mut rx, "第一条").await;

        // ── abort：driver 退出 ──
        controller.abort().await;
        // driver 需要一点时间跑到循环头、break、drop input_rx。
        let dead = wait_until(|| controller.is_driver_dead()).await;
        assert!(dead, "abort 之后 driver 必须真的死掉（input_rx 被 drop）");

        // 旧 bug 的核心复现：此时发消息会静默丢弃。
        controller.submit_input("abort 后的消息（应被丢弃）").await;

        // ── respawn：session 复活 ──
        controller
            .respawn()
            .await
            .expect("respawn 必须成功（spawn 存过 config）");
        assert!(
            !controller.is_driver_dead().await,
            "respawn 之后 driver 必须重新活着"
        );
        // cancel_flag 必须已复位，否则新 driver 会在循环头立刻再退出。
        assert!(
            !controller.cancel_flag.load(Ordering::SeqCst),
            "respawn 必须复位 cancel_flag，否则新 driver 立刻自杀"
        );

        // 第二轮：新 driver 必须真的消费输入（不是又一个哑 session）。
        controller.submit_input("第二条").await;
        wait_for_user_message(&mut rx, "第二条").await;

        controller.abort().await;
    }

    /// 轮询等待条件成立（最多 ~2s），避免 sleep 固定时长带来的抖动。
    async fn wait_until<F, Fut>(mut cond: F) -> bool
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        for _ in 0..200 {
            if cond().await {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        false
    }

    /// 起一个总是回固定长回答的 OpenAI-兼容 mock，返回它的 base_url。
    /// 回答故意超过 advisor D5 短输出阈值，避免 gate 重试干扰断言。
    async fn stub_llm_server() -> wiremock::MockServer {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let server = wiremock::MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(
                        serde_json::json!({
                            "id": "chatcmpl-test",
                            "object": "chat.completion",
                            "created": 0,
                            "model": "test",
                            "choices": [{
                                "index": 0,
                                "message": { "role": "assistant", "content": "占位长回答：超过 advisor D5 短输出 gate 的 50 字符阈值，避免测试被 gate 重试干扰。" },
                                "finish_reason": "stop"
                            }],
                            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
                        })
                        .to_string(),
                    )),
            )
            .await;
        server
    }

    /// 单角色（manager）driver 的最小可用配置，指向 `base_url` 的 mock。
    fn stub_single_role_config(base_url: String, cwd: &std::path::Path) -> ControllerConfig {
        use crate::config::{ModelCatalog, ModelDef};
        use crate::role::RoleTemplate;

        let agent_config = Arc::new(AgentConfig {
            advisor: Default::default(),
            models: ModelCatalog {
                models: vec![ModelDef {
                    name: "stub-standard".into(),
                    api: "openai".into(),
                    provider: "test".into(),
                    base_url,
                    api_key: "test-key".into(),
                    context_window: 32000,
                    max_tokens: 4096,
                    supports_thinking: false,
                    supports_vision: false,
                    supports_image_generation: false,
                    cost_per_million_input: None,
                    cost_per_million_output: None,
                    tier: Some("standard".into()),
                    timeout_secs: None,
                }],
                tiers: None,
                role_tiers: None,
            },
            roles: [(
                "manager".to_string(),
                RoleTemplate {
                    id: "manager".into(),
                    name: "manager".into(),
                    category: "planning".into(),
                    model_tier: "standard".into(),
                    model_chain: vec![],
                    prompt_file: None,
                    temperature: None,
                    tools: vec![],
                    icon: "👔".into(),
                    skills: vec![],
                    code_paths: vec![],
                },
            )]
            .into_iter()
            .collect(),
        });
        let resolver = Arc::new(ModelResolver::from_config(&agent_config).unwrap());
        ControllerConfig {
            task_id: None,
            roles: vec!["manager".to_string()],
            initial_prompt: None,
            max_rounds: 0,
            session_token_budget: 0,
            agent_config,
            model_resolver: resolver,
            default_params: GenerateParams::default(),
            primary_model_id: None,
            initial_tier: None,
            initial_history: vec![],
            cwd: cwd.to_path_buf(),
            subsession_store: Arc::new(crate::subsession::SubsessionStore::new()),
            advisor_monitor: AdvisorMonitorConfig::default(),
            stream_mode: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            max_delegates_per_session: crate::controller::default_max_delegates(),
            session_id: String::new(),
        }
    }

    /// 空/纯空白输入必须被丢弃，不能真跑一轮。
    ///
    /// 原来这里是个**空语句块**（`if trimmed.is_empty() {}`），本意显然
    /// 是 continue（多角色路径写的就是 `if !trimmed.is_empty()`）。落空
    /// 的后果：前端误发空串也会发 UserMessage 并真调模型，而部分厂商对
    /// text 为空的消息直接 400，这一轮白报错。
    #[tokio::test]
    async fn blank_input_is_dropped_without_running_a_turn() {
        let server = stub_llm_server().await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = stub_single_role_config(server.uri(), dir.path());

        let controller = ChatController::new(64);
        let mut rx = controller.spawn(cfg).await;

        controller.submit_input("").await;
        controller.submit_input("   \n\t ").await;
        // 紧跟一条真输入：它必须是第一个被处理的 —— 说明前两条既没发
        // UserMessage 也没跑 turn（不是"慢"，是"没有"）。
        controller.submit_input("真正的问题").await;

        loop {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
                .await
                .expect("等 UserMessage 超时")
                .expect("recv");
            if let ChatEvent::UserMessage { text } = ev {
                assert_eq!(
                    text, "真正的问题",
                    "空输入不该产生 UserMessage（它先到，说明空 if 又漏了）"
                );
                break;
            }
        }

        controller.abort().await;
    }

    /// `AbortOnDrop` 必须真的取消 task。
    ///
    /// 裸 `JoinHandle` 被 drop 不取消任务是 tokio 的既定语义，delegate
    /// 那条路正是踩在这上面：驱动侧 500ms 轮询先赢时直接 drop 整个
    /// run_turn future，handler 走不到任何 abort 分支，specialist 脱管
    /// 继续写文件/跑 bash，事件还在往共享 event_tx 里发。
    #[tokio::test]
    async fn abort_on_drop_really_cancels_the_task() {
        use std::sync::atomic::AtomicBool;

        let ran_to_completion = Arc::new(AtomicBool::new(false));
        let flag = ran_to_completion.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            flag.store(true, Ordering::SeqCst);
        });
        {
            let _guard = crate::sub_cancel::AbortOnDrop(handle.abort_handle());
            // 模拟"外层 future 被 drop"：guard 在这里出作用域。
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(
            !ran_to_completion.load(Ordering::SeqCst),
            "guard drop 后 task 必须被取消 —— 否则它会脱管跑到底"
        );

        // 反向对照：没有 guard 时裸 drop JoinHandle 不会取消（锁死我们
        // 依赖的 tokio 语义，将来若变了这个测试会提醒）。
        let ran2 = Arc::new(AtomicBool::new(false));
        let flag2 = ran2.clone();
        drop(tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            flag2.store(true, Ordering::SeqCst);
        }));
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert!(
            ran2.load(Ordering::SeqCst),
            "裸 drop JoinHandle 不取消任务（这正是需要 AbortOnDrop 的原因）"
        );
    }

    /// Session 级暂停门 engaged 时，driver **不得**先把
    /// `UserMessage` / `RoleStarted` / `[calling LLM]` 发出去再到 runner
    /// 内部 park —— 那样 UI 看到的是「消息发出去了，然后永远没结果」
    /// 而不是「已暂停」。这是随附 UI 会命中的路径（它只用
    /// `pause-session` / `resume-session` 这一组端点）。
    ///
    /// 断言两半：暂停期间只出「输入已收到、▶ 继续后执行」的 Status，
    /// 不出 `UserMessage`；resume 之后那条输入被真的执行。
    #[tokio::test]
    async fn session_gate_paused_holds_input_instead_of_faking_a_turn() {
        let server = stub_llm_server().await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = stub_single_role_config(server.uri(), dir.path());

        let controller = ChatController::new(64);
        let mut rx = controller.spawn(cfg).await;

        // 先确认 driver 正常工作（排除"哑 session"造成的假阳性）。
        controller.submit_input("热身").await;
        wait_for_user_message(&mut rx, "热身").await;

        // ⏸：engage session gate（= POST /api/chat/pause-session）。
        assert!(controller.pause_session(), "首次 pause 应返回 true");

        controller.submit_input("暂停期间发的消息").await;

        // 暂停期间：必须出现"输入已收到"的 Status，且**不能**出现
        // 这条输入的 UserMessage（旧 bug 就是照发不误）。
        let mut saw_hold_status = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await {
                Ok(Ok(ChatEvent::UserMessage { text })) if text == "暂停期间发的消息" => {
                    panic!("暂停期间不该发出这条输入的 UserMessage（driver 没查 gate）");
                }
                Ok(Ok(ChatEvent::Status { message })) => {
                    if message.contains("▶ 继续后执行") {
                        saw_hold_status = true;
                    }
                    // 旧 bug 的另一个指纹：暂停期间就发 [calling LLM]。
                    assert!(
                        !message.contains("calling LLM"),
                        "暂停期间不该发 [calling LLM]：{message}"
                    );
                }
                Ok(Ok(_)) => {}
                Ok(Err(_)) => break,
                Err(_) => {
                    if saw_hold_status {
                        break;
                    }
                }
            }
        }
        assert!(
            saw_hold_status,
            "暂停期间应给出「输入已收到、▶ 继续后执行」的反馈，而不是静默"
        );

        // ▶：resume 之后那条被 hold 的输入必须真的执行。
        assert!(
            controller
                .resume_session()
                .await
                .gate_paused_for_ms
                .is_some(),
            "resume 应返回暂停时长"
        );
        wait_for_user_message(&mut rx, "暂停期间发的消息").await;

        controller.abort().await;
    }

    /// CancelTurn issued while a V2-paused input is parked must discard
    /// that input. Resuming must not run it, and the queued CancelTurn must
    /// not poison the next fresh input.
    #[tokio::test]
    async fn session_gate_paused_pending_input_can_be_cancelled() {
        let server = stub_llm_server().await;
        let dir = tempfile::tempdir().unwrap();
        let cfg = stub_single_role_config(server.uri(), dir.path());

        let controller = ChatController::new(64);
        let mut rx = controller.spawn(cfg).await;
        assert!(controller.pause_session());
        controller.submit_input("不要执行这条").await;

        loop {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("wait for parked status")
                .expect("recv");
            if matches!(ev, ChatEvent::Status { ref message } if message.contains("▶ 继续后执行")) {
                break;
            }
        }

        controller.cancel_turn().await;
        loop {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("wait for pending cancellation")
                .expect("recv");
            match ev {
                ChatEvent::UserMessage { text } if text == "不要执行这条" => {
                    panic!("cancelled parked input must never start")
                }
                ChatEvent::Status { message } if message.contains("已取消暂停中等待执行的输入") => {
                    break;
                }
                _ => {}
            }
        }

        assert!(
            controller
                .resume_session()
                .await
                .gate_paused_for_ms
                .is_some()
        );
        controller.submit_input("恢复后的新输入").await;
        loop {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
                .await
                .expect("wait for fresh input")
                .expect("recv");
            match ev {
                ChatEvent::UserMessage { text } if text == "不要执行这条" => {
                    panic!("cancelled parked input ran after resume")
                }
                ChatEvent::UserMessage { text } if text == "恢复后的新输入" => break,
                _ => {}
            }
        }

        controller.abort().await;
    }

    /// 从事件流里等一条内容匹配的 `UserMessage` —— 证明 driver 真的
    /// 收到并处理了这条输入（哑 session 会在这里超时）。
    async fn wait_for_user_message(rx: &mut broadcast::Receiver<ChatEvent>, expect: &str) {
        loop {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
                .await
                .unwrap_or_else(|_| panic!("等 UserMessage('{expect}') 超时 —— driver 没在消费输入"))
                .expect("recv");
            if let ChatEvent::UserMessage { text } = ev {
                if text == expect {
                    return;
                }
            }
        }
    }

    /// `max_rounds: 0` 必须表示「不限轮次」，绝不能让 `1..=0` 变成
    /// 空区间把 driver 直接跑完退出（session 出生即哑）。
    #[test]
    fn round_limit_zero_means_unlimited_not_empty_range() {
        // 0 → 不限：区间非空，循环体至少跑一轮。
        let limit = effective_round_limit(0);
        assert_eq!(limit, u32::MAX);
        assert!(
            (1..=limit).next().is_some(),
            "max_rounds=0 时轮次区间必须非空，否则 driver 出生即死"
        );
        // 回归对照：修复前的写法就是空区间。
        assert!(
            (1..=0u32).next().is_none(),
            "1..=0 确实是空区间（这正是旧 bug 的成因）"
        );
        // 非 0 原样透传。
        assert_eq!(effective_round_limit(1), 1);
        assert_eq!(effective_round_limit(20), 20);
        assert_eq!((1..=effective_round_limit(3)).count(), 3);
    }

    #[test]
    fn choice_requested_serializes_to_frontend_shape() {
        // 验证 ChoiceRequested 经 chat_event_to_frontend_json 转成前端
        // internally-tagged 形态：{"type":"ChoiceRequested",...}，字段
        // snake_case；options 里空字段被 skip_serializing_if 省略。
        let event = ChatEvent::ChoiceRequested {
            role_id: "manager".into(),
            choice_id: "choice-manager-0".into(),
            question: "选哪种鉴权方式？".into(),
            multi: false,
            layout: "grid".into(),
            allow_upload: true,
            wait: false,
            options: vec![
                ChoiceOption {
                    label: "JWT".into(),
                    description: "无状态 Bearer token".into(),
                    image: "/api/images/jwt.png".into(),
                    recommended: true,
                    pros: vec!["无状态".into()],
                    cons: vec!["撤销难".into()],
                    details: "适合 API 客户端。".into(),
                },
                ChoiceOption {
                    label: "OAuth2".into(),
                    description: String::new(),
                    image: String::new(),
                    recommended: false,
                    ..Default::default()
                },
            ],
        };
        let wire = crate::event_json::chat_event_to_frontend_json(&event).expect("frontend json");
        let v: serde_json::Value = serde_json::from_str(&wire).expect("parse wire");
        assert_eq!(v["type"], "ChoiceRequested");
        assert_eq!(v["role_id"], "manager");
        assert_eq!(v["choice_id"], "choice-manager-0");
        assert_eq!(v["question"], "选哪种鉴权方式？");
        assert_eq!(v["multi"], false);
        assert_eq!(v["layout"], "grid");
        assert_eq!(v["allow_upload"], true);
        let opts = v["options"].as_array().expect("options array");
        assert_eq!(opts.len(), 2);
        // 第一项：全字段序列化。
        assert_eq!(opts[0]["label"], "JWT");
        assert_eq!(opts[0]["description"], "无状态 Bearer token");
        assert_eq!(opts[0]["image"], "/api/images/jwt.png");
        assert_eq!(opts[0]["recommended"], true);
        // 优缺点 / 详情随事件透传（前端「详情」面板的数据源）。
        assert_eq!(opts[0]["pros"][0], "无状态");
        assert_eq!(opts[0]["cons"][0], "撤销难");
        assert_eq!(opts[0]["details"], "适合 API 客户端。");
        // 第二项：空 description/image + recommended=false 被省略。
        assert_eq!(opts[1]["label"], "OAuth2");
        assert!(opts[1].get("description").is_none(), "空 description 应省略");
        assert!(opts[1].get("image").is_none(), "空 image 应省略");
        assert!(opts[1].get("recommended").is_none(), "recommended=false 应省略");
        assert!(opts[1].get("pros").is_none(), "空 pros 应省略");
        assert!(opts[1].get("cons").is_none(), "空 cons 应省略");
    }

    #[test]
    fn delegate_finished_carries_failure_status() {
        // The handler emits `status = "failed" | "timeout"` in error
        // paths. Pin both values so a typo here doesn't ship.
        let failed = ChatEvent::DelegateFinished {
            from_role: "manager".into(),
            to_role: "programmer".into(),
            status: "failed".into(),
            summary: "delegate to 'programmer' failed: model unavailable".into(),
            sub_id: "programmer-sub-2".into(),
            wf_id: None,
        };
        let json = serde_json::to_value(&failed).unwrap();
        assert_eq!(json["DelegateFinished"]["status"], "failed");
        assert_eq!(json["DelegateFinished"]["sub_id"], "programmer-sub-2");

        let timed_out = ChatEvent::DelegateFinished {
            from_role: "manager".into(),
            to_role: "programmer".into(),
            status: "timeout".into(),
            summary: "delegate to 'programmer' timed out after 60s".into(),
            sub_id: "programmer-sub-3".into(),
            wf_id: None,
        };
        let json = serde_json::to_value(&timed_out).unwrap();
        assert_eq!(json["DelegateFinished"]["status"], "timeout");
        assert_eq!(json["DelegateFinished"]["sub_id"], "programmer-sub-3");
    }

    #[test]
    fn delegate_event_roundtrip_through_workspace_wrapper() {
        // Mirrors the Tauri runtime's `WorkspaceChatEvent` wrap
        // (see `src-tauri/src/chat_panel/types.rs:8` and
        // `controller_runtime.rs:88-95`): the controller event
        // lands inside `{ workspaceId, event: <ChatEvent> }` over
        // the Tauri `chat:event` channel. Both layers must agree
        // on the shape or the frontend never sees the dispatch.
        let inner = ChatEvent::DelegateStarted {
            from_role: "manager".into(),
            to_role: "programmer".into(),
            task: "ping".into(),
            sub_id: "programmer-sub-4".into(),
            wf_id: None,
        };
        let wrapped = serde_json::json!({
            "workspaceId": "ws-1",
            "event": inner,
        });
        let parsed: ChatEvent =
            serde_json::from_value(wrapped["event"].clone()).expect("parse");
        match parsed {
            ChatEvent::DelegateStarted { from_role, to_role, task, sub_id, .. } => {
                assert_eq!(from_role, "manager");
                assert_eq!(to_role, "programmer");
                assert_eq!(task, "ping");
                assert_eq!(sub_id, "programmer-sub-4");
            }
            other => panic!("expected DelegateStarted, got {other:?}"),
        }
    }
    // ─── TimeoutWarning wire shape ─────────────────────────────
    //
    // The soft-timeout path emits this when a turn has been
    // running past `soft_timeout_secs` but the user has not yet
    // decided whether to cancel. The frontend surfaces a
    // "继续等待 / 终止" prompt; the backend's hard-kill at
    // `hard_timeout_secs` is the safety net. Pin the field names
    // so a rename here doesn't silently break the UI.
    #[test]
    fn timeout_warning_serializes_to_frontend_shape() {
        let event = ChatEvent::TimeoutWarning {
            role_id: "programmer".into(),
            elapsed_secs: 130,
            soft_timeout_secs: 120,
            hard_timeout_secs: 360,
            sub_id: None,
        };
        let json = serde_json::to_value(&event).expect("serialize");
        assert!(json.get("TimeoutWarning").is_some(), "missing variant tag");
        assert_eq!(json["TimeoutWarning"]["role_id"], "programmer");
        assert_eq!(json["TimeoutWarning"]["elapsed_secs"], 130);
        assert_eq!(json["TimeoutWarning"]["soft_timeout_secs"], 120);
        assert_eq!(json["TimeoutWarning"]["hard_timeout_secs"], 360);
        // sub_id is None → must be skipped (not serialized as null)
        // to keep wire shape lean.
        assert!(
            json["TimeoutWarning"].get("sub_id").is_none(),
            "sub_id must be skipped when None"
        );
    }

    #[test]
    fn timeout_warning_carries_sub_id_for_delegate_subsession() {
        // When a *delegate* subsession is the one over budget, the
        // UI's "查看日志" action needs the sub_id to land on the
        // subagent transcript directly.
        let event = ChatEvent::TimeoutWarning {
            role_id: "programmer".into(),
            elapsed_secs: 150,
            soft_timeout_secs: 120,
            hard_timeout_secs: 360,
            sub_id: Some("programmer-sub-99".into()),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["TimeoutWarning"]["sub_id"], "programmer-sub-99");
    }

    /// workflow step 已无硬超时，超时分支发 `hard_timeout_secs: 0`
    /// 表示「不会被强杀，等或手动终止」。前端按 `> 0` 决定是否显示
    /// 「硬超时将在 Ns 强制终止」文案，所以 0 必须原样出现在 wire 上
    /// （不能被 skip_serializing_if 省掉，否则 UI 读到 undefined
    /// 走进 NaN 分支）。
    #[test]
    fn timeout_warning_zero_hard_timeout_survives_serialization() {
        let event = ChatEvent::TimeoutWarning {
            role_id: "architect".into(),
            elapsed_secs: 900,
            soft_timeout_secs: 900,
            hard_timeout_secs: 0,
            sub_id: Some("architect-sub-1".into()),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(
            json["TimeoutWarning"]["hard_timeout_secs"], 0,
            "0 必须保留在 wire 上，供前端判定「无硬超时」"
        );
        // elapsed 独立于 soft：复发时报的是真实已跑秒数。
        assert_eq!(json["TimeoutWarning"]["elapsed_secs"], 900);

        // 前端同款判定（chat_impl.ts: ev.hard_timeout_secs > 0）。
        let hard = json["TimeoutWarning"]["hard_timeout_secs"]
            .as_u64()
            .expect("hard_timeout_secs 必须是数字，不能缺字段");
        assert!(hard == 0, "无硬超时分支");
    }

    // ─── AdvisorHint driver plumbing ──────────────────────────────
    //
    // Driver-level test for the advisor injection channel (design
    // §7): a hint pushed while the driver is idle must (a) NOT
    // trigger a turn on its own, and (b) ride along with the next
    // user input's context as a synthetic `🦉 advisor 监察：` message.

    #[tokio::test]
    async fn advisor_hint_enters_context_without_triggering_turn() {
        use crate::config::{ModelCatalog, ModelDef};
        use crate::role::RoleTemplate;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(
                        serde_json::json!({
                            "id": "chatcmpl-test",
                            "object": "chat.completion",
                            "created": 0,
                            "model": "test",
                            "choices": [{
                                "index": 0,
                                "message": { "role": "assistant", "content": "plain reply（占位长回答：超过 advisor D5 短输出 gate 的 50 字符阈值，避免测试被 gate 重试干扰）" },
                                "finish_reason": "stop"
                            }],
                            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
                        })
                        .to_string(),
                    )),
            )
            .await;

        let agent_config = Arc::new(AgentConfig {
            advisor: Default::default(),
            models: ModelCatalog {
                models: vec![ModelDef {
                    name: "stub-standard".into(),
                    api: "openai".into(),
                    provider: "test".into(),
                    base_url: server.uri(),
                    api_key: "test-key".into(),
                    context_window: 32000,
                    max_tokens: 4096,
                    supports_thinking: false,
                    supports_vision: false,
                    supports_image_generation: false,
                    cost_per_million_input: None,
                    cost_per_million_output: None,
                    tier: Some("standard".into()),
                    timeout_secs: None,
                }],
                tiers: None,
                role_tiers: None,
            },
            roles: [(
                "manager".to_string(),
                RoleTemplate {
                    id: "manager".into(),
                    name: "manager".into(),
                    category: "planning".into(),
                    model_tier: "standard".into(),
                    model_chain: vec![],
                    prompt_file: None,
                    temperature: None,
                    tools: vec![],
                    icon: "👔".into(),
                    skills: vec![],
            code_paths: vec![],
                },
            )]
            .into_iter()
            .collect(),
        });
        let resolver = Arc::new(ModelResolver::from_config(&agent_config).unwrap());
        let dir = tempfile::tempdir().unwrap();

        let cfg = ControllerConfig {
            task_id: None,
            roles: vec!["manager".to_string()],
            initial_prompt: None,
            max_rounds: 0,
            session_token_budget: 0,
            agent_config,
            model_resolver: resolver,
            default_params: GenerateParams::default(),
            primary_model_id: None,
            initial_tier: None,
            initial_history: vec![],
            cwd: dir.path().to_path_buf(),
            subsession_store: Arc::new(crate::subsession::SubsessionStore::new()),
            advisor_monitor: AdvisorMonitorConfig::default(),
            stream_mode: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            max_delegates_per_session: crate::controller::default_max_delegates(),
            session_id: String::new(),
        };

        async fn wait_request_count(server: &wiremock::MockServer, n: usize) {
            for _ in 0..150 {
                let got = server.received_requests().await.map(|r| r.len()).unwrap_or(0);
                if got >= n {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            panic!("timed out waiting for {n} model requests");
        }

        let controller = ChatController::new(64);
        let _rx = controller.spawn(cfg).await;

        // Turn 1 runs normally.
        controller.submit_input("第一问").await;
        wait_request_count(&server, 1).await;

        // A hint pushed while idle must not trigger a new turn.
        controller.advisor_hint("改用 src/lib.rs 路径");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "advisor hint must not trigger a turn on its own"
        );

        // The hint rides along with the next user input's context.
        controller.submit_input("第二问").await;
        wait_request_count(&server, 2).await;
        let reqs = server.received_requests().await.unwrap();
        let body2 = String::from_utf8_lossy(&reqs[1].body);
        assert!(
            body2.contains("🦉 advisor 监察"),
            "second turn's request carries the hint: {body2}"
        );
        assert!(body2.contains("改用 src/lib.rs 路径"), "hint text: {body2}");
        // …and the user question was recorded for the monitor.
        assert_eq!(controller.last_user_input(), "第二问");

        controller.abort().await;
    }

    // ─── workflow 失败自动善后 ───────────────────────────────────
    //
    // slash 路径 workflow 失败 → 不停在等输入：合成一条「[自动善后]」
    // 输入立刻开一轮（实测实锤：gate 失败后 advisor hint 悬空、
    // 无人动作，50 分钟流水线零产出）。
    #[tokio::test]
    async fn workflow_failure_triggers_auto_followup_turn() {
        use crate::config::{ModelCatalog, ModelDef};
        use crate::role::RoleTemplate;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(
                        serde_json::json!({
                            "id": "chatcmpl-test",
                            "object": "chat.completion",
                            "created": 0,
                            "model": "test",
                            "choices": [{
                                "index": 0,
                                "message": { "role": "assistant", "content": "善后回复（占位长回答：超过 advisor D5 短输出 gate 的 50 字符阈值，避免测试被 gate 重试干扰）" },
                                "finish_reason": "stop"
                            }],
                            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
                        })
                        .to_string(),
                    )),
            )
            .await;

        let agent_config = Arc::new(AgentConfig {
            advisor: Default::default(),
            models: ModelCatalog {
                models: vec![ModelDef {
                    name: "stub-standard".into(),
                    api: "openai".into(),
                    provider: "test".into(),
                    base_url: server.uri(),
                    api_key: "test-key".into(),
                    context_window: 32000,
                    max_tokens: 4096,
                    supports_thinking: false,
                    supports_vision: false,
                    supports_image_generation: false,
                    cost_per_million_input: None,
                    cost_per_million_output: None,
                    tier: Some("standard".into()),
                    timeout_secs: None,
                }],
                tiers: None,
                role_tiers: None,
            },
            roles: [(
                "manager".to_string(),
                RoleTemplate {
                    id: "manager".into(),
                    name: "manager".into(),
                    category: "planning".into(),
                    model_tier: "standard".into(),
                    model_chain: vec![],
                    prompt_file: None,
                    temperature: None,
                    tools: vec![],
                    icon: "👔".into(),
                    skills: vec![],
                    code_paths: vec![],
                },
            )]
            .into_iter()
            .collect(),
        });
        let resolver = Arc::new(ModelResolver::from_config(&agent_config).unwrap());
        let dir = tempfile::tempdir().unwrap();
        // 必败 workflow：单 step + 不可能通过的产出契约（max_retries
        // 默认 0）。advisor 关闭，契约耗尽后直接失败。
        let wf_dir = dir.path().join(".latte").join("workflows.d");
        std::fs::create_dir_all(&wf_dir).unwrap();
        std::fs::write(
            wf_dir.join("failwf.toml"),
            r#"
name = "failwf"
command = "/failwf"
[[steps]]
id = "only"
role = "manager"
task = "做点事"
[steps.output_contract]
require = ["永远不可能出现的验收字符串"]
"#,
        )
        .unwrap();

        let cfg = ControllerConfig {
            task_id: None,
            roles: vec!["manager".to_string()],
            initial_prompt: None,
            max_rounds: 0,
            session_token_budget: 0,
            agent_config,
            model_resolver: resolver,
            default_params: GenerateParams::default(),
            primary_model_id: None,
            initial_tier: None,
            initial_history: vec![],
            cwd: dir.path().to_path_buf(),
            subsession_store: Arc::new(crate::subsession::SubsessionStore::new()),
            advisor_monitor: AdvisorMonitorConfig {
                enabled: false,
                ..Default::default()
            },
            stream_mode: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            max_delegates_per_session: crate::controller::default_max_delegates(),
            session_id: String::new(),
        };

        async fn wait_request_count(server: &wiremock::MockServer, n: usize) {
            for _ in 0..150 {
                let got = server.received_requests().await.map(|r| r.len()).unwrap_or(0);
                if got >= n {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            panic!("timed out waiting for {n} model requests");
        }

        let controller = ChatController::new(64);
        let _rx = controller.spawn(cfg).await;

        controller.submit_input("/failwf 做点事").await;
        // 请求 1 = workflow 的 worker step（契约失败）；请求 2 =
        // 自动善后 turn（没有它就证明失败后停在了等输入）。
        wait_request_count(&server, 2).await;
        let reqs = server.received_requests().await.unwrap();
        let body2 = String::from_utf8_lossy(&reqs[1].body);
        assert!(body2.contains("[自动善后]"), "第二轮是自动善后 turn: {body2}");
        assert!(body2.contains("wf_id="), "善后输入带 wf_id: {body2}");
        assert!(body2.contains("请善后"), "善后输入带处置指令: {body2}");

        controller.abort().await;
    }

    // ─── Soft-timeout warning plumbing ───────────────────────────
    //
    // 验证 soft timeout → TimeoutWarning + turn 仍能完成的契约。
    // Mock server 2s 响应 + model.timeout_secs=1s → 第 1s 触发软
    // 警告，第 2s 拿到响应跑完；硬超时 3s 永远不到。
    #[tokio::test]
    async fn cancellable_turn_can_be_cancelled() {
        use crate::config::{ModelCatalog, ModelDef};
        use crate::role::RoleTemplate;
        use std::time::Duration;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .set_delay(Duration::from_millis(5_000))
                            .set_body_string(
                                serde_json::json!({
                                    "id": "chatcmpl-test",
                                    "object": "chat.completion",
                                    "created": 0,
                                    "model": "test",
                                    "choices": [{
                                        "index": 0,
                                        "message": { "role": "assistant", "content": "slow reply" },
                                        "finish_reason": "stop"
                                    }],
                                    "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
                                })
                                .to_string(),
                            ),
                    ),
            )
            .await;

        let agent_config = Arc::new(AgentConfig {
            advisor: Default::default(),
            models: ModelCatalog {
                models: vec![ModelDef {
                    name: "stub-slow".into(),
                    api: "openai".into(),
                    provider: "test".into(),
                    base_url: server.uri(),
                    api_key: "test-key".into(),
                    context_window: 32000,
                    max_tokens: 4096,
                    supports_thinking: false,
                    supports_vision: false,
                    supports_image_generation: false,
                    cost_per_million_input: None,
                    cost_per_million_output: None,
                    tier: Some("standard".into()),
                    timeout_secs: Some(1),
                }],
                tiers: None,
                role_tiers: None,
            },
            roles: [(
                "programmer".to_string(),
                RoleTemplate {
                    id: "programmer".into(),
                    name: "programmer".into(),
                    category: "execution".into(),
                    model_tier: "standard".into(),
                    model_chain: vec![],
                    prompt_file: None,
                    temperature: None,
                    tools: vec![],
                    icon: "💻".into(),
                    skills: vec![],
            code_paths: vec![],
                },
            )]
            .into_iter()
            .collect(),
        });
        let resolver = Arc::new(ModelResolver::from_config(&agent_config).unwrap());
        let dir = tempfile::tempdir().unwrap();

        let cfg = ControllerConfig {
            task_id: None,
            roles: vec!["programmer".to_string()],
            initial_prompt: None,
            max_rounds: 0,
            session_token_budget: 0,
            agent_config,
            model_resolver: resolver,
            default_params: GenerateParams::default(),
            primary_model_id: None,
            initial_tier: None,
            initial_history: vec![],
            cwd: dir.path().to_path_buf(),
            subsession_store: Arc::new(crate::subsession::SubsessionStore::new()),
            advisor_monitor: AdvisorMonitorConfig::default(),
            stream_mode: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            max_delegates_per_session: crate::controller::default_max_delegates(),
            session_id: String::new(),
        };

        let controller = ChatController::new(64);
        let mut rx = controller.spawn(cfg).await;

        // 发一条慢请求（5s 响应），然后在 1s 后 cancel_turn。
        controller.submit_input("请分析这个慢请求").await;
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        controller.cancel_turn().await;

        // 收事件：期待 Error 事件描述 turn cancelled，而不是
        // 看到 "slow reply" 的 RoleTurn。
        let mut saw_cancelled = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
                Ok(Ok(ChatEvent::RoleTurn { content, .. })) => {
                    panic!("turn should be cancelled, not complete: {content}");
                }
                Ok(Ok(ChatEvent::Error { message, .. })) => {
                    if message.contains("cancelled") {
                        saw_cancelled = true;
                        break;
                    }
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => panic!("recv error: {e}"),
                Err(_) => continue,
            }
        }
        assert!(saw_cancelled, "turn must be cancelled by cancel_turn()");

        controller.abort().await;
    }

    // ─── UserMessage 事件 ─────────────────────────────────────────
    //
    // 回放里要能看到用户自己发的内容：driver 收到非斜杠 Input 时先广播
    // UserMessage 再跑 turn；斜杠命令（/roles 等）不产生该事件。

    #[tokio::test]
    async fn user_message_emitted_for_input_but_not_slash_command() {
        use crate::config::{ModelCatalog, ModelDef};
        use crate::role::RoleTemplate;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(
                        serde_json::json!({
                            "id": "chatcmpl-test",
                            "object": "chat.completion",
                            "created": 0,
                            "model": "test",
                            "choices": [{
                                "index": 0,
                                "message": { "role": "assistant", "content": "plain reply（占位长回答：超过 advisor D5 短输出 gate 的 50 字符阈值，避免测试被 gate 重试干扰）" },
                                "finish_reason": "stop"
                            }],
                            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
                        })
                        .to_string(),
                    )),
            )
            .await;

        let agent_config = Arc::new(AgentConfig {
            advisor: Default::default(),
            models: ModelCatalog {
                models: vec![ModelDef {
                    name: "stub-standard".into(),
                    api: "openai".into(),
                    provider: "test".into(),
                    base_url: server.uri(),
                    api_key: "test-key".into(),
                    context_window: 32000,
                    max_tokens: 4096,
                    supports_thinking: false,
                    supports_vision: false,
                    supports_image_generation: false,
                    cost_per_million_input: None,
                    cost_per_million_output: None,
                    tier: Some("standard".into()),
                    timeout_secs: None,
                }],
                tiers: None,
                role_tiers: None,
            },
            roles: [(
                "manager".to_string(),
                RoleTemplate {
                    id: "manager".into(),
                    name: "manager".into(),
                    category: "planning".into(),
                    model_tier: "standard".into(),
                    model_chain: vec![],
                    prompt_file: None,
                    temperature: None,
                    tools: vec![],
                    icon: "👔".into(),
                    skills: vec![],
            code_paths: vec![],
                },
            )]
            .into_iter()
            .collect(),
        });
        let resolver = Arc::new(ModelResolver::from_config(&agent_config).unwrap());
        let dir = tempfile::tempdir().unwrap();

        let cfg = ControllerConfig {
            task_id: None,
            roles: vec!["manager".to_string()],
            initial_prompt: None,
            max_rounds: 0,
            session_token_budget: 0,
            agent_config,
            model_resolver: resolver,
            default_params: GenerateParams::default(),
            primary_model_id: None,
            initial_tier: None,
            initial_history: vec![],
            cwd: dir.path().to_path_buf(),
            subsession_store: Arc::new(crate::subsession::SubsessionStore::new()),
            advisor_monitor: AdvisorMonitorConfig::default(),
            stream_mode: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            max_delegates_per_session: crate::controller::default_max_delegates(),
            session_id: String::new(),
        };

        async fn next_event(rx: &mut tokio::sync::broadcast::Receiver<ChatEvent>) -> ChatEvent {
            tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
                .await
                .expect("event timeout")
                .expect("recv")
        }

        let controller = ChatController::new(64);
        let mut rx = controller.spawn(cfg).await;

        // 1) 普通输入 → 必须先出现 UserMessage（且 text 与输入一致），
        //    且它先于本 turn 的 RoleTurn。
        controller.submit_input("你好，世界").await;
        let mut saw_user_message = false;
        loop {
            match next_event(&mut rx).await {
                ChatEvent::UserMessage { text } => {
                    assert_eq!(text, "你好，世界");
                    saw_user_message = true;
                }
                ChatEvent::RoleTurn { .. } => break,
                _ => {}
            }
        }
        assert!(saw_user_message, "send 路径必须广播 UserMessage");

        // 2) 斜杠命令 → RoleList 返回，但绝不能出现 UserMessage。
        controller.submit_input("/roles").await;
        loop {
            match next_event(&mut rx).await {
                um @ ChatEvent::UserMessage { .. } => {
                    panic!("slash command must not emit UserMessage: {um:?}")
                }
                ChatEvent::RoleList { .. } => break,
                _ => {}
            }
        }

        controller.abort().await;
    }

    // 多角色 supervisor 自动暂停必须能由外层 `Resume` 恢复。
    //
    // 旧实现只在手动 `/pause` 的内层 park loop 处理 Resume；supervisor
    // 在 round 尾把 SessionRecord 置为 Paused 后会回到外层收输入，而外层
    // 的 Resume arm 是空的。之后虽然还能收到消息、发 RoundStarted，但
    // 每轮都会被 `session not running` 跳过，session 永久空转。
    #[tokio::test]
    async fn multi_role_supervisor_auto_pause_can_resume() {
        use crate::config::{ModelCatalog, ModelDef};
        use crate::role::RoleTemplate;

        let server = stub_llm_server().await;
        let stub_role = |id: &str, icon: &str| {
            (
                id.to_string(),
                RoleTemplate {
                    id: id.into(),
                    name: id.into(),
                    category: "planning".into(),
                    model_tier: "standard".into(),
                    model_chain: vec![],
                    prompt_file: None,
                    temperature: None,
                    tools: vec![],
                    icon: icon.into(),
                    skills: vec![],
                    code_paths: vec![],
                },
            )
        };
        let agent_config = Arc::new(AgentConfig {
            advisor: Default::default(),
            models: ModelCatalog {
                models: vec![ModelDef {
                    name: "stub-standard".into(),
                    api: "openai".into(),
                    provider: "test".into(),
                    base_url: server.uri(),
                    api_key: "test-key".into(),
                    context_window: 32000,
                    max_tokens: 4096,
                    supports_thinking: false,
                    supports_vision: false,
                    supports_image_generation: false,
                    cost_per_million_input: None,
                    cost_per_million_output: None,
                    tier: Some("standard".into()),
                    timeout_secs: None,
                }],
                tiers: None,
                role_tiers: None,
            },
            roles: [stub_role("manager", "👔"), stub_role("programmer", "🧑‍💻")]
                .into_iter()
                .collect(),
        });
        let resolver = Arc::new(ModelResolver::from_config(&agent_config).unwrap());

        let dir = tempfile::tempdir().unwrap();
        let git_ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !git_ok {
            eprintln!("skipping multi_role_supervisor_auto_pause_can_resume: git unavailable");
            return;
        }

        let cfg = ControllerConfig {
            task_id: Some("t-supervisor-resume".to_string()),
            roles: vec!["manager".to_string(), "programmer".to_string()],
            initial_prompt: Some("exercise supervisor resume".to_string()),
            max_rounds: 20,
            // mock 每轮报告 15 tokens；第一位角色结束即自动暂停。
            session_token_budget: 1,
            agent_config,
            model_resolver: resolver,
            default_params: GenerateParams::default(),
            primary_model_id: None,
            initial_tier: None,
            initial_history: vec![],
            cwd: dir.path().to_path_buf(),
            subsession_store: Arc::new(crate::subsession::SubsessionStore::new()),
            advisor_monitor: AdvisorMonitorConfig::default(),
            stream_mode: Arc::new(AtomicBool::new(false)),
            max_delegates_per_session: crate::controller::default_max_delegates(),
            session_id: String::new(),
        };

        async fn next_event(
            rx: &mut tokio::sync::broadcast::Receiver<ChatEvent>,
        ) -> ChatEvent {
            tokio::time::timeout(std::time::Duration::from_secs(15), rx.recv())
                .await
                .expect("event timeout")
                .expect("recv")
        }

        let controller = ChatController::new(256);
        let mut rx = controller.spawn(cfg).await;
        controller.submit_input("first turn").await;

        loop {
            match next_event(&mut rx).await {
                ChatEvent::Paused { reason } => {
                    assert!(reason.contains("token budget"), "unexpected pause: {reason}");
                    break;
                }
                _ => {}
            }
        }

        // 使用真实 V2 resume-session 对应的公开入口，而不是 legacy
        // `resume()`；即使 supervisor pause 没有 engage gate，也必须把
        // SessionManager 从 Paused 恢复。
        assert!(controller.resume_session().await.driver_resumed);

        controller.submit_input("turn after resume").await;
        loop {
            match next_event(&mut rx).await {
                // 恢复后必须真的有角色起跑。具体是哪个角色由 scheduler
                // 的 order 决定（不一定是 manager），这里只关心「不再被
                // session not running 跳过」。
                ChatEvent::RoleStarted { role_id, .. } => {
                    assert!(
                        role_id == "manager" || role_id == "programmer",
                        "unexpected role: {role_id}"
                    );
                    break;
                }
                ChatEvent::Status { message } if message.contains("session not running") => {
                    panic!("resumed session remained non-running: {message}");
                }
                _ => {}
            }
        }

        controller.abort().await;
    }

    // 多角色 HIL：`/pause <role>` 让单个角色在轮次里被跳过（其余角色照
    // 常跑），`/resume <role>` 恢复后该角色重新参与。锁定 controller
    // 多角色路径的 per-role 暂停闸门 + slash 命令入口接线（此前只在
    // CLI REPL 里有，controller 路径漏接）。
    #[tokio::test]
    async fn multi_role_pause_skips_only_that_role() {
        use crate::config::{ModelCatalog, ModelDef};
        use crate::role::RoleTemplate;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(
                        serde_json::json!({
                            "id": "chatcmpl-test",
                            "object": "chat.completion",
                            "created": 0,
                            "model": "test",
                            "choices": [{
                                "index": 0,
                                "message": { "role": "assistant", "content": "ok" },
                                "finish_reason": "stop"
                            }],
                            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
                        })
                        .to_string(),
                    )),
            )
            .await;

        let stub_role = |id: &str, icon: &str| {
            (
                id.to_string(),
                RoleTemplate {
                    id: id.into(),
                    name: id.into(),
                    category: "planning".into(),
                    model_tier: "standard".into(),
                    model_chain: vec![],
                    prompt_file: None,
                    temperature: None,
                    tools: vec![],
                    icon: icon.into(),
                    skills: vec![],
                    code_paths: vec![],
                },
            )
        };

        let agent_config = Arc::new(AgentConfig {
            advisor: Default::default(),
            models: ModelCatalog {
                models: vec![ModelDef {
                    name: "stub-standard".into(),
                    api: "openai".into(),
                    provider: "test".into(),
                    base_url: server.uri(),
                    api_key: "test-key".into(),
                    context_window: 32000,
                    max_tokens: 4096,
                    supports_thinking: false,
                    supports_vision: false,
                    supports_image_generation: false,
                    cost_per_million_input: None,
                    cost_per_million_output: None,
                    tier: Some("standard".into()),
                    timeout_secs: None,
                }],
                tiers: None,
                role_tiers: None,
            },
            roles: [stub_role("manager", "👔"), stub_role("programmer", "🧑‍💻")]
                .into_iter()
                .collect(),
        });
        let resolver = Arc::new(ModelResolver::from_config(&agent_config).unwrap());

        // 多角色路径要求真实 git 仓库（WorkspaceManager::resolve_repo_root
        // 走 `git rev-parse --show-toplevel`）。
        let dir = tempfile::tempdir().unwrap();
        let git_ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !git_ok {
            eprintln!("skipping multi_role_pause_skips_only_that_role: git unavailable");
            return;
        }

        let cfg = ControllerConfig {
            task_id: Some("t-pause".to_string()),
            roles: vec!["manager".to_string(), "programmer".to_string()],
            initial_prompt: Some("do the thing".to_string()),
            max_rounds: 20,
            session_token_budget: 0,
            agent_config,
            model_resolver: resolver,
            default_params: GenerateParams::default(),
            primary_model_id: None,
            initial_tier: None,
            initial_history: vec![],
            cwd: dir.path().to_path_buf(),
            subsession_store: Arc::new(crate::subsession::SubsessionStore::new()),
            advisor_monitor: AdvisorMonitorConfig::default(),
            stream_mode: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            max_delegates_per_session: crate::controller::default_max_delegates(),
            session_id: String::new(),
        };

        async fn next_event(rx: &mut tokio::sync::broadcast::Receiver<ChatEvent>) -> ChatEvent {
            tokio::time::timeout(std::time::Duration::from_secs(15), rx.recv())
                .await
                .expect("event timeout")
                .expect("recv")
        }

        let controller = ChatController::new(256);
        let mut rx = controller.spawn(cfg).await;

        // 1) 单独暂停 programmer → 必须收到结构化 RolePaused 事件。
        controller.submit_input("/pause programmer").await;
        loop {
            if let ChatEvent::RolePaused { role_id } = next_event(&mut rx).await {
                assert_eq!(role_id, "programmer");
                break;
            }
        }

        // 2) 触发一轮：programmer 应被跳过（不产生 RoleStarted），manager
        //    照常起跑。收集到 RoundEnded 为止。
        controller.submit_input("go").await;
        let mut programmer_skipped = false;
        let mut programmer_started = false;
        let mut manager_started = false;
        loop {
            match next_event(&mut rx).await {
                ChatEvent::Status { message } => {
                    if message.contains("programmer") && message.contains("skipped — role paused") {
                        programmer_skipped = true;
                    }
                }
                ChatEvent::RoleStarted { role_id, .. } => {
                    if role_id == "programmer" {
                        programmer_started = true;
                    }
                    if role_id == "manager" {
                        manager_started = true;
                    }
                }
                ChatEvent::RoundEnded { .. } => break,
                _ => {}
            }
        }
        assert!(programmer_skipped, "paused role must emit the skip status");
        assert!(!programmer_started, "paused role must NOT start a turn");
        assert!(manager_started, "non-paused role must still run");

        // 3) 恢复 programmer → 必须收到结构化 RoleResumed 事件。
        controller.submit_input("/resume programmer").await;
        loop {
            if let ChatEvent::RoleResumed { role_id } = next_event(&mut rx).await {
                assert_eq!(role_id, "programmer");
                break;
            }
        }

        // 4) 再触发一轮：programmer 这次应正常起跑。
        controller.submit_input("go").await;
        let mut programmer_started_after_resume = false;
        loop {
            match next_event(&mut rx).await {
                ChatEvent::RoleStarted { role_id, .. } if role_id == "programmer" => {
                    programmer_started_after_resume = true;
                }
                ChatEvent::RoundEnded { .. } => break,
                _ => {}
            }
        }
        assert!(
            programmer_started_after_resume,
            "resumed role must run again"
        );

        controller.abort().await;
    }

    /// 锁定 bash 工具的契约：allowed "bash" 直接保留 registry 的
    /// "bash" 工具（扁平化后注册名 == 配置名），且接受 {command, cwd}
    /// （cwd 由 resolve_tool_input_against_cwd 注入）。
    #[tokio::test]
    async fn bash_tool_kept_and_accepts_cwd() {
        let mgr = build_tool_manager(&["read".into(), "write".into(), "bash".into(), "search".into()])
            .await
            .expect("build_tool_manager");
        let names: Vec<String> = mgr.get_tool_names();
        assert!(names.contains(&"bash".to_string()), "allowed \"bash\" 应保留 bash 工具: {names:?}");
        // bash 必须接受 {command, cwd}。
        let args = serde_json::json!({"command":"pwd","cwd":"/tmp"});
        let r = mgr.execute("bash", args, None).await;
        assert!(r.is_ok(), "bash 应接受 {{command,cwd}}，却失败: {:?}", r.err());
        // eval 不在 allowed 里，被过滤掉。
        assert!(!names.iter().any(|n| n.starts_with("eval")), "eval 不应被保留（不在 allowed）: {names:?}");
    }

    /// 工具级说明是模型行为的单一事实源：read 的批量能力必须同时出现
    /// 在 description 和 schema 中，不能依赖某个角色 prompt 恰好提到它。
    #[tokio::test]
    async fn read_tool_exposes_batch_contract_in_description_and_schema() {
        let mgr = build_tool_manager(&["read".into()])
            .await
            .expect("build_tool_manager");
        let read = mgr.get_tool("read").expect("read tool");
        assert!(
            read.description.contains("read(paths") || read.description.contains("paths"),
            "read description must teach the batch form: {}",
            read.description
        );
        let paths = read
            .input_schema
            .properties
            .get("paths")
            .expect("read schema must expose paths");
        assert_eq!(paths.min_length, Some(1));
        assert_eq!(paths.max_length, Some(10));
    }

    /// `paths` 不是只给模型看的文案：handler 必须真正按输入顺序批量执行，
    /// 且单项失败不能丢掉其余成功结果。
    #[tokio::test]
    async fn read_tool_batch_executes_with_partial_failure() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        let missing = dir.path().join("missing.txt");
        tokio::fs::write(&a, "alpha").await.unwrap();
        tokio::fs::write(&b, "beta").await.unwrap();

        let mgr = build_tool_manager(&["read".into()])
            .await
            .expect("build_tool_manager");
        let out = mgr
            .execute(
                "read",
                serde_json::json!({
                    "paths": [
                        a.to_string_lossy(),
                        missing.to_string_lossy(),
                        b.to_string_lossy()
                    ]
                }),
                None,
            )
            .await
            .expect("batch read should partially succeed");
        let files = out["files"].as_array().expect("files array");
        let failed = out["failed"].as_array().expect("failed array");
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0]["content"], "alpha",
            "successful results keep input order"
        );
        assert_eq!(
            files[1]["content"], "beta",
            "successful results keep input order"
        );
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0]["path"], missing.to_string_lossy().as_ref());
        assert_eq!(out["count"], 3);
    }

    #[tokio::test]
    async fn read_tool_single_path_stays_backward_compatible() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("single.txt");
        tokio::fs::write(&path, "unchanged").await.unwrap();
        let mgr = build_tool_manager(&["read".into()]).await.unwrap();

        let out = mgr
            .execute(
                "read",
                serde_json::json!({"path": path.to_string_lossy()}),
                None,
            )
            .await
            .expect("single read");
        assert_eq!(out["content"], "unchanged");
        assert!(
            out.get("files").is_none(),
            "single result must not be wrapped"
        );
    }

    #[tokio::test]
    async fn read_tool_rejects_invalid_batch_shapes() {
        let mgr = build_tool_manager(&["read".into()]).await.unwrap();
        let cases = [
            serde_json::json!({}),
            serde_json::json!({"path":"a", "paths":["b"]}),
            serde_json::json!({"paths":[]}),
            serde_json::json!({"paths":["1","2","3","4","5","6","7","8","9","10","11"]}),
            serde_json::json!({"paths":["ok", 2]}),
        ];
        for input in cases {
            assert!(
                mgr.execute("read", input.clone(), None).await.is_err(),
                "invalid input should fail: {input}"
            );
        }
    }

    #[tokio::test]
    async fn read_tool_batch_enforces_output_budget() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.txt");
        let second = dir.path().join("second.txt");
        let payload = "x".repeat(110 * 1024);
        tokio::fs::write(&first, &payload).await.unwrap();
        tokio::fs::write(&second, &payload).await.unwrap();
        let mgr = build_tool_manager(&["read".into()]).await.unwrap();

        let out = mgr
            .execute(
                "read",
                serde_json::json!({"paths":[first.to_string_lossy(), second.to_string_lossy()]}),
                None,
            )
            .await
            .expect("budgeted batch read");
        assert_eq!(out["files"].as_array().unwrap().len(), 1);
        let failed = out["failed"].as_array().unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0]["path"], second.to_string_lossy().as_ref());
        assert!(failed[0]["error"].as_str().unwrap().contains("budget"));
        assert_eq!(out["count"], 2);
    }

    /// 动态授权必须拿到与初始 allowlist 相同的增强后工具；旧实现先缓存
    /// FULL_TOOL_POOL 再注入 description，导致 request_tool 授权出的 read
    /// 没有工具说明，也没有批量 schema。
    #[tokio::test]
    async fn requested_read_keeps_model_description_and_batch_schema() {
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        register_request_tool(tm.clone()).expect("register request_tool");
        tm.execute(
            "request_tool",
            serde_json::json!({"tool_name":"read", "reason":"inspect files"}),
            None,
        )
        .await
        .expect("grant read");

        let read = tm.get_tool("read").expect("granted read");
        assert!(
            read.description.contains("paths"),
            "description lost after grant"
        );
        assert!(read.input_schema.properties.contains_key("paths"));
    }

    /// 锁定 code_graph 暴露契约：allowed 含 "code_graph" 时，
    /// build_tool_manager 必须注册出名为 "code_graph" 的工具。
    /// 这是「programmer 为什么没用上 code_graph」排查的回归护栏——
    /// 一旦 keep 匹配逻辑或工具名漂移导致 code_graph 被过滤掉，此测试立刻失败。
    #[tokio::test]
    async fn code_graph_kept_when_allowed() {
        let mgr = build_tool_manager(&[
            "read".into(),
            "write".into(),
            "bash".into(),
            "search".into(),
            "delegate".into(),
            "code_graph".into(),
            "ask".into(),
        ])
        .await
        .expect("build_tool_manager");
        let names: Vec<String> = mgr.get_tool_names();
        assert!(
            names.contains(&"code_graph".to_string()),
            "allowed 含 code_graph 时必须注册 code_graph 工具: {names:?}"
        );
    }

    /// 反向契约：allowed 不含 code_graph 时不应注册它（避免误暴露）。
    #[tokio::test]
    async fn code_graph_absent_when_not_allowed() {
        let mgr = build_tool_manager(&["read".into(), "search".into()])
            .await
            .expect("build_tool_manager");
        let names: Vec<String> = mgr.get_tool_names();
        assert!(
            !names.contains(&"code_graph".to_string()),
            "未 allow 时不应注册 code_graph: {names:?}"
        );
    }

    /// Advisor gate 端到端（driver 级）：manager turn 产出连续撞 D5
    /// → 重试耗尽 → run_turn_gated raise AdvisorTerminated → driver
    /// 发 ChatEvent::AdvisorTerminated（坏答案不落盘为 RoleTurn）；
    /// session 不终止，下一条用户输入照常跑通（继续语义）。
    #[tokio::test]
    async fn driver_emits_advisor_terminated_after_gate_retries_exhausted() {
        use crate::config::{ModelCatalog, ModelDef};
        use crate::role::RoleTemplate;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let openai_body = |content: &str| serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 0,
            "model": "test",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": content },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
        }).to_string();

        let server = wiremock::MockServer::start().await;
        // 前 3 次请求（1 initial + max_retries(2)）都给撞 D5 的短输出；
        // 之后的请求给正常长回答（第二问用）。
        struct FirstN(std::sync::atomic::AtomicUsize, usize);
        impl wiremock::Match for FirstN {
            fn matches(&self, _req: &wiremock::Request) -> bool {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < self.1
            }
        }
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .and(FirstN(std::sync::atomic::AtomicUsize::new(0), 3))
                    .respond_with(ResponseTemplate::new(200).set_body_string(openai_body("TODO"))),
            )
            .await;
        let good = "这是第二问的正常完整回答：逐条说明结论与依据，长度远超五十字符的 D5 阈值。";
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(good))),
            )
            .await;

        let agent_config = Arc::new(AgentConfig {
            advisor: Default::default(),
            models: ModelCatalog {
                models: vec![ModelDef {
                    name: "stub-standard".into(),
                    api: "openai".into(),
                    provider: "test".into(),
                    base_url: server.uri(),
                    api_key: "test-key".into(),
                    context_window: 32000,
                    max_tokens: 4096,
                    supports_thinking: false,
                    supports_vision: false,
                    supports_image_generation: false,
                    cost_per_million_input: None,
                    cost_per_million_output: None,
                    tier: Some("standard".into()),
                    timeout_secs: None,
                }],
                tiers: None,
                role_tiers: None,
            },
            roles: [(
                "manager".to_string(),
                RoleTemplate {
                    id: "manager".into(),
                    name: "manager".into(),
                    category: "planning".into(),
                    model_tier: "standard".into(),
                    model_chain: vec![],
                    prompt_file: None,
                    temperature: None,
                    tools: vec![],
                    icon: "👔".into(),
                    skills: vec![],
            code_paths: vec![],
                },
            )]
            .into_iter()
            .collect(),
        });
        let resolver = Arc::new(ModelResolver::from_config(&agent_config).unwrap());
        let dir = tempfile::tempdir().unwrap();
        let cfg = ControllerConfig {
            task_id: None,
            roles: vec!["manager".to_string()],
            initial_prompt: None,
            max_rounds: 0,
            session_token_budget: 0,
            agent_config,
            model_resolver: resolver,
            default_params: GenerateParams::default(),
            primary_model_id: None,
            initial_tier: None,
            initial_history: vec![],
            cwd: dir.path().to_path_buf(),
            subsession_store: Arc::new(crate::subsession::SubsessionStore::new()),
            // enabled=true（默认）→ driver 给 manager runner 装 gate。
            advisor_monitor: AdvisorMonitorConfig::default(),
            stream_mode: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            max_delegates_per_session: crate::controller::default_max_delegates(),
            session_id: String::new(),
        };

        let controller = ChatController::new(64);
        let mut rx = controller.spawn(cfg).await;

        // 第一问：3 次产出都撞 D5 → AdvisorTerminated；途中任何
        // RoleTurn 都不得携带坏答案 "TODO"。
        controller.submit_input("第一问").await;
        let (role_id, reason, detector, sub_id) = loop {
            match tokio::time::timeout(std::time::Duration::from_secs(15), rx.recv())
                .await
                .expect("event timeout")
                .expect("recv")
            {
                ChatEvent::RoleTurn { content, .. } => {
                    assert!(
                        !content.contains("TODO"),
                        "bad answer must not be accepted as RoleTurn: {content}"
                    );
                }
                ChatEvent::AdvisorTerminated { role_id, reason, detector, sub_id } => {
                    break (role_id, reason, detector, sub_id);
                }
                _ => {}
            }
        };
        assert_eq!(role_id, "manager");
        assert_eq!(detector.as_deref(), Some("D5"), "gate detector propagates");
        assert!(reason.contains("D5"), "reason names the detector: {reason}");
        assert!(sub_id.is_none());

        // 软终止语义：session 没死，第二问照常跑通并落盘 RoleTurn。
        controller.submit_input("第二问").await;
        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(15), rx.recv())
                .await
                .expect("event timeout")
                .expect("recv")
            {
                ChatEvent::RoleTurn { role_id, content, .. } if role_id == "manager" => {
                    assert_eq!(content, good, "session continues after termination");
                    break;
                }
                _ => {}
            }
        }

        controller.abort().await;
    }

    // ─── task_report tool (P0-2) ───────────────────────────────────
    //
    // 任务报告闭环：manager 调 `task_report` 工具 → 广播
    // `ChatEvent::TaskReport { task_id, summary, result }` → ui-server
    // SSE 订阅时识别此事件并调 POST /api/tasks/:id/report（已存在的
    // 后端路由 handlers.rs:998）。这样 manager 完成任务后能自动把
    // 任务推进到 human_review 状态，避免永远停在 in_progress。
    //
    // 本文件测试只覆盖 controller 侧的事件语义（注册工具 + 工具调用
    // 产生事件 + 事件序列化）。ui-server 侧的事件→HTTP 桥接在
    // latte-agent-ui-server/src/handlers.rs 的 events_sse 里。

    #[test]
    fn task_report_serializes_to_frontend_shape() {
        // 锁定 wire shape：与 api.ts union 配对。
        let event = ChatEvent::TaskReport {
            role_id: "manager".into(),
            task_id: "LAT-100".into(),
            summary: "实现 ringbuf 核心读写".into(),
            result: "completed".into(),
        };
        let wire = crate::event_json::chat_event_to_frontend_json(&event).expect("frontend json");
        let v: serde_json::Value = serde_json::from_str(&wire).expect("parse wire");
        assert_eq!(v["type"], "TaskReport");
        assert_eq!(v["role_id"], "manager");
        assert_eq!(v["task_id"], "LAT-100");
        assert_eq!(v["summary"], "实现 ringbuf 核心读写");
        assert_eq!(v["result"], "completed");
    }

    #[tokio::test]
    async fn task_report_tool_call_emits_chat_event() {
        // 工具层：调 `task_report` 工具 → 必须广播 ChatEvent::TaskReport。
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        let (event_tx, mut rx) = broadcast::channel(8);
        register_task_report_tool(&tm, event_tx, "manager".into())
            .expect("register task_report");

        let out = tm
            .execute(
                "task_report",
                serde_json::json!({
                    "task_id": "LAT-100",
                    "summary": "实现 ringbuf 核心读写",
                    "result": "completed"
                }),
                None,
            )
            .await
            .expect("task_report tool call");
        let text = out.as_str().expect("string result");
        assert!(text.contains("LAT-100"), "tool result names the task: {text}");

        let ev = rx.try_recv().expect("TaskReport event");
        match ev {
            ChatEvent::TaskReport { task_id, summary, result, .. } => {
                assert_eq!(task_id, "LAT-100");
                assert_eq!(summary, "实现 ringbuf 核心读写");
                assert_eq!(result, "completed");
            }
            other => panic!("expected TaskReport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn task_report_tool_rejects_bad_result() {
        // 校验：result 必须是 completed/aborted/failed/timeout 之一。
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        let (event_tx, _rx) = broadcast::channel(8);
        register_task_report_tool(&tm, event_tx, "manager".into())
            .expect("register task_report");

        let err = tm
            .execute(
                "task_report",
                serde_json::json!({
                    "task_id": "LAT-100",
                    "summary": "x",
                    "result": "succeeded"  // 不是合法 enum
                }),
                None,
            )
            .await
            .expect_err("bad result must be rejected");
        // schema 层 enum_values 校验在 handler 之前生效（model
        // provider 也会先看 schema），错误信息不必列出合法值；
        // 只断言"被拒"即可。
        let msg = err.to_string();
        assert!(
            msg.contains("schema") || msg.contains("validation") || msg.contains("completed"),
            "error should mention schema/validation/allowed values: {msg}"
        );
    }

    #[tokio::test]
    async fn task_report_tool_requires_task_id() {
        // 校验：task_id 非空。
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        let (event_tx, _rx) = broadcast::channel(8);
        register_task_report_tool(&tm, event_tx, "manager".into())
            .expect("register task_report");

        let err = tm
            .execute(
                "task_report",
                serde_json::json!({"task_id": "", "summary": "x", "result": "completed"}),
                None,
            )
            .await
            .expect_err("empty task_id must be rejected");
        assert!(
            err.to_string().contains("task_id"),
            "error should mention task_id: {err}"
        );
    }

    #[tokio::test]
    async fn request_tool_grants_missing_tool() {
        // build_tool_manager 会先注册所有包并填充 FULL_TOOL_POOL。
        // 空 allowed → 除 request_tool 外全部被过滤掉。
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        register_request_tool(tm.clone()).expect("register request_tool");

        // request_tool 本身必须对所有角色可用。
        let names = tm.get_tool_names();
        assert!(names.contains(&"request_tool".to_string()),
            "request_tool must be registered for all roles: {names:?}");

        // 申请一个不存在的工具 → 拒绝。
        let err = tm
            .execute(
                "request_tool",
                serde_json::json!({"tool_name": "bogus_tool_999", "reason": "test"}),
                None,
            )
            .await
            .expect_err("bogus tool must be rejected");
        assert!(err.to_string().contains("未知工具"), "error: {err}");

        // 申请一个真实工具（read）→ 自动批准并注册。
        let result = tm
            .execute(
                "request_tool",
                serde_json::json!({"tool_name": "read", "reason": "need to read files for testing"}),
                None,
            )
            .await
            .expect("request_tool should succeed for 'read'");
        let result_str = serde_json::to_string(&result).unwrap_or_default();
        assert!(result_str.contains("已临时授权"), "expected 'granted', got: {result_str}");

        // 申请后 read 应已注册。
        let names_after = tm.get_tool_names();
        assert!(names_after.contains(&"read".to_string()),
            "read must be registered after request: {names_after:?}");
    }

    // ─── code_graph ────────────────────────────────────────────────
    //
    // 回归护栏：旧实现把 kind 映射成 Rust 语法的 pattern 又不传 --lang，
    // 在 C 项目上恒 0 命中，导致角色配置里有 code_graph 却从不被调用。
    // 下面的测试锁住「每个语言至少能查函数」和「C 必须走 tree-sitter
    // 节点类型而不是 Rust pattern」这两条底线。

    #[test]
    fn code_graph_c_function_maps_to_tree_sitter_kind() {
        // C 的 function 必须映射到 function_definition。
        // 实测依据：某 C 仓库 src/pac.c 真实函数 22 个，
        // `kind: function_definition` 命中 22（100%），而旧的
        // pattern `$RET $NAME($$$PARAMS) { $$$BODY }` 只命中 8（36%）。
        let kinds = code_graph_node_kinds("c", "function");
        assert_eq!(kinds, vec!["function_definition"], "C function 映射错误");
    }

    #[test]
    fn code_graph_no_rust_syntax_leaks_into_other_langs() {
        // 旧 bug 的直接形态：非 Rust 语言拿到 Rust 的节点类型。
        for lang in ["c", "cpp", "go", "python", "typescript", "javascript", "java"] {
            for kind in code_graph_kinds_for_lang(lang) {
                for node in code_graph_node_kinds(lang, kind) {
                    assert!(
                        !matches!(node, "function_item" | "struct_item" | "trait_item"
                                      | "impl_item" | "enum_item" | "use_declaration"),
                        "{lang}/{kind} 漏进了 Rust 专属节点类型 {node}"
                    );
                }
            }
        }
    }

    #[test]
    fn code_graph_every_lang_supports_function() {
        // 每个声明支持的语言都必须至少能查函数——这是建地图的最小能力。
        for lang in code_graph_supported_langs() {
            assert!(
                !code_graph_node_kinds(lang, "function").is_empty(),
                "{lang} 缺 function 映射"
            );
        }
    }

    #[test]
    fn code_graph_lang_inference_from_extension() {
        assert_eq!(code_graph_infer_lang("src/pac.c"), Some("c"));
        assert_eq!(code_graph_infer_lang("include/foo/internal/edata.h"), Some("c"));
        assert_eq!(code_graph_infer_lang("a/b/mod.rs"), Some("rust"));
        assert_eq!(code_graph_infer_lang("ui/src/api.ts"), Some("typescript"));
        assert_eq!(code_graph_infer_lang("main.go"), Some("go"));
        // 目录 / 未知扩展名推断不出来，handler 会要求显式传 lang。
        assert_eq!(code_graph_infer_lang("src/"), None);
        assert_eq!(code_graph_infer_lang("README"), None);
    }

    #[test]
    fn code_graph_rule_yaml_shapes() {
        // 缩进错一层 ast-grep 就报 "invalid type: unit value" 并返回 0 命中，
        // 而 0 命中对模型来说和"这工具没用"无法区分——正是旧实现被弃用的
        // 机制。所以这里锁死四种形态的**逐字节** YAML。
        // 每种形态都已用 `ast-grep scan --inline-rules` 实测可解析。

        // 1) 单节点类型
        assert_eq!(
            code_graph_build_rule("c", &["function_definition"], None),
            "id: code_graph\nlanguage: c\nrule:\n  kind: function_definition\n"
        );

        // 2) 多节点类型 → any（旧实现在这里把 `any:` 顶到了第 0 列）
        assert_eq!(
            code_graph_build_rule("go", &["function_declaration", "method_declaration"], None),
            "id: code_graph\nlanguage: go\nrule:\n  any:\n  \
             - kind: function_declaration\n  - kind: method_declaration\n"
        );

        // 3) 单节点类型 + name → all
        assert_eq!(
            code_graph_build_rule("c", &["function_definition"], Some("decay")),
            "id: code_graph\nlanguage: c\nrule:\n  all:\n  \
             - kind: function_definition\n  - regex: 'decay'\n"
        );

        // 4) 多节点类型 + name → all 里嵌 any，any 的子项缩进 4 空格
        assert_eq!(
            code_graph_build_rule("go", &["function_declaration", "method_declaration"], Some("M")),
            "id: code_graph\nlanguage: go\nrule:\n  all:\n  - any:\n    \
             - kind: function_declaration\n    - kind: method_declaration\n  - regex: 'M'\n"
        );

        // YAML 单引号必须转义，否则规则被截断成非法 YAML。
        let r = code_graph_build_rule("c", &["function_definition"], Some("it's"));
        assert!(r.contains("regex: 'it''s'"), "单引号未转义: {r}");
    }

    #[test]
    fn code_graph_signature_strips_body_and_flattens_params() {
        // C 项目常见换行风格：返回类型独占一行、参数跨行对齐。
        // 签名模式必须压平并砍掉函数体——这是 19x 体积压缩的来源。
        let text = "static bool\npac_init(tsdn_t *tsdn, pac_t *pac,\n    base_t *base) {\n\tint x = 1;\n\treturn false;\n}";
        let sig = code_graph_signature_of(text);
        assert_eq!(sig, "static bool pac_init(tsdn_t *tsdn, pac_t *pac, base_t *base)");
        assert!(!sig.contains("return false"), "函数体没被砍掉: {sig}");

        // 没有函数体的匹配（如 import / 宏）原样压平即可。
        let sig = code_graph_signature_of("#include \"foo/internal/pac.h\"");
        assert_eq!(sig, "#include \"foo/internal/pac.h\"");
    }

    #[test]
    fn code_graph_unknown_kind_lists_available() {
        // 传错 kind 时必须能拿到该语言的可用清单，否则模型只能瞎猜、
        // 试一次失败就永久放弃这个工具（旧实现的实际后果）。
        let avail = code_graph_kinds_for_lang("c");
        assert!(avail.contains(&"function"), "{avail:?}");
        assert!(avail.contains(&"struct"), "{avail:?}");
        assert!(avail.contains(&"macro"), "{avail:?}");
        // C 没有 trait/impl，不该出现。
        assert!(!avail.contains(&"trait"), "{avail:?}");
        assert!(code_graph_kinds_for_lang("brainfuck").is_empty());
    }

    // ─── code_graph 端到端（真的 spawn ast-grep）─────────────────────
    //
    // 上面的单元测试只锁得住映射表，锁不住"ast-grep 真的认这个节点类型"
    // ——那必须真跑一次。旧实现的 bug 恰恰是"表里有值但跑出来 0 命中"，
    // 所以这层测试是防回归的关键。
    // 没装 ast-grep 时跳过而不是失败（CI 不一定有）。

    fn cg_have_ast_grep() -> bool {
        std::process::Command::new("ast-grep")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// 多语言样本。C 部分刻意用 C 项目常见的风格：返回类型独占一行、
    /// 参数跨行对齐、static 函数——旧 pattern 实现在这种风格上召回 36%。
    fn cg_fixture() -> tempfile::TempDir {
        let d = tempfile::tempdir().expect("tempdir");
        let w = |name: &str, body: &str| {
            std::fs::write(d.path().join(name), body).expect("write fixture");
        };
        w(
            "sample.c",
            "#include <stdlib.h>\n\
             #define PAC_MAGIC 0x1234\n\
             #define PAC_ROUND(x) (((x) + 7) & ~7)\n\
             \n\
             struct pac_s {\n\tint npages;\n\tint ndirty;\n};\n\
             \n\
             static bool\n\
             pac_init(void *tsdn, struct pac_s *pac,\n    int npages) {\n\
             \tpac->npages = npages;\n\
             \tpac->ndirty = 0;\n\
             \tif (npages < 0) {\n\t\treturn true;\n\t}\n\
             \tif (npages > PAC_MAGIC) {\n\t\treturn true;\n\t}\n\
             \treturn false;\n}\n\
             \n\
             void\n\
             pac_decay_all(struct pac_s *pac) {\n\
             \tpac_init(NULL, pac, 0);\n\
             \tpac->ndirty = 0;\n\
             \tpac->npages = PAC_ROUND(pac->npages);\n\
             \tpac_init(NULL, pac, pac->npages);\n}\n\
             \n\
             int\n\
             pac_npages_get(struct pac_s *pac) {\n\
             \tif (pac == NULL) {\n\t\treturn -1;\n\t}\n\
             \tint n = pac->npages;\n\
             \tif (n < 0) {\n\t\tn = 0;\n\t}\n\
             \treturn n;\n}\n",
        );
        w(
            "sample.rs",
            "use std::fmt;\n\
             pub struct Foo { a: i32 }\n\
             pub trait Bar { fn go(&self); }\n\
             impl Bar for Foo { fn go(&self) { let _ = self.a; } }\n\
             pub fn top_level(x: i32) -> i32 { x + 1 }\n",
        );
        w(
            "sample.py",
            "import os\n\
             class Foo:\n    def bar(self):\n        return os.getcwd()\n\
             def baz(x):\n    return Foo().bar()\n",
        );
        w(
            "sample.go",
            "package m\n\
             import \"fmt\"\n\
             type S struct{ A int }\n\
             func F(x int) int { return x }\n\
             func (s *S) M() { fmt.Println(1) }\n",
        );
        w(
            "sample.ts",
            "import { a } from \"b\";\n\
             export interface I { x: number }\n\
             export class C { m(): number { return 1; } }\n\
             export function f(n: number): number { return n + 1; }\n",
        );
        d
    }

    /// 直接驱动 code_graph 的 handler（`Tool::handler` 是公开字段），
    /// 不用起 ToolManager。
    async fn cg_call(
        args: serde_json::Value,
    ) -> Result<serde_json::Value, latte_rs_agent_tools::error::ToolError> {
        let tool = code_graph_tool();
        let ctx = latte_rs_agent_tools::types::ToolExecutionContext::fresh("code_graph", 0);
        (tool.handler)(args, ctx).await
    }

    fn cg_matches(v: &serde_json::Value) -> String {
        v.get("matches").and_then(|m| m.as_str()).unwrap_or("").to_string()
    }
    fn cg_total(v: &serde_json::Value) -> u64 {
        v.get("total_matches").and_then(|m| m.as_u64()).unwrap_or(0)
    }

    #[tokio::test]
    async fn code_graph_e2e_c_functions_full_recall() {
        if !cg_have_ast_grep() {
            eprintln!("skip: ast-grep 未安装");
            return;
        }
        let d = cg_fixture();
        let p = d.path().join("sample.c");
        let out = cg_call(serde_json::json!({
            "path": p.to_str().unwrap(), "kind": "function"
        }))
        .await
        .expect("code_graph 应成功");

        // 3 个函数定义：pac_init / pac_decay_all / pac_npages_get。
        // 旧 pattern 实现在"返回类型独占一行"的风格上会全部漏掉。
        assert_eq!(cg_total(&out), 3, "C 函数召回不全: {out:?}");
        let m = cg_matches(&out);
        for f in ["pac_init", "pac_decay_all", "pac_npages_get"] {
            assert!(m.contains(f), "缺函数 {f}: {m}");
        }
        // 语言应由 .c 扩展名自动推断。
        assert_eq!(out.get("lang").and_then(|v| v.as_str()), Some("c"));
    }

    #[tokio::test]
    async fn code_graph_e2e_signatures_much_smaller_than_file() {
        if !cg_have_ast_grep() {
            eprintln!("skip: ast-grep 未安装");
            return;
        }
        let d = cg_fixture();
        let p = d.path().join("sample.c");
        let file_size = std::fs::read_to_string(&p).unwrap().len();

        let sigs = cg_call(serde_json::json!({
            "path": p.to_str().unwrap(), "kind": "function"
        }))
        .await
        .unwrap();
        let sig_text = cg_matches(&sigs);

        // 签名模式不能带函数体。
        assert!(!sig_text.contains("return pac->npages"), "签名混进函数体: {sig_text}");
        assert!(!sig_text.contains('{'), "签名混进函数体起始: {sig_text}");
        // 必须带 file:line，供后续 ranged read 用。
        assert!(sig_text.contains("sample.c:"), "缺 file:line 定位: {sig_text}");

        // full 模式应含函数体且明显更大。
        let full = cg_call(serde_json::json!({
            "path": p.to_str().unwrap(), "kind": "function", "mode": "full"
        }))
        .await
        .unwrap();
        let full_text = cg_matches(&full);
        assert!(full_text.contains("return n;"), "full 模式应含函数体: {full_text}");
        assert!(
            sig_text.len() < full_text.len(),
            "签名({}) 应小于 full({})", sig_text.len(), full_text.len()
        );

        // 省 token 是这个工具存在的理由。这里比的是"签名正文"而不是整条
        // 输出行——临时目录的绝对路径前缀在真实仓库里是短相对路径，
        // 算进去会掩盖真实压缩率。
        let sig_body: usize = sig_text
            .lines()
            .map(|l| l.split_once(": ").map(|(_, b)| b.len()).unwrap_or(l.len()))
            .sum();
        assert!(
            sig_body * 3 < file_size,
            "签名正文({sig_body}) 相对整文件({file_size}) 压缩不足"
        );
    }

    #[tokio::test]
    async fn code_graph_e2e_all_langs_find_functions() {
        if !cg_have_ast_grep() {
            eprintln!("skip: ast-grep 未安装");
            return;
        }
        let d = cg_fixture();
        // (文件, 最少命中数, 必须出现的名字)
        let cases: &[(&str, u64, &str)] = &[
            ("sample.c", 3, "pac_init"),
            ("sample.rs", 1, "top_level"),
            ("sample.py", 2, "baz"),
            ("sample.go", 2, "F"),
            ("sample.ts", 1, "f"),
        ];
        for (file, min_n, needle) in cases {
            let p = d.path().join(file);
            let out = cg_call(serde_json::json!({
                "path": p.to_str().unwrap(), "kind": "function"
            }))
            .await
            .unwrap_or_else(|e| panic!("{file} 查询失败: {e}"));
            assert!(
                cg_total(&out) >= *min_n,
                "{file} 命中 {} < 期望 {min_n}: {out:?}", cg_total(&out)
            );
            assert!(
                cg_matches(&out).contains(needle),
                "{file} 缺 {needle}: {}", cg_matches(&out)
            );
        }
    }

    #[tokio::test]
    async fn code_graph_e2e_name_filter_narrows() {
        if !cg_have_ast_grep() {
            eprintln!("skip: ast-grep 未安装");
            return;
        }
        let d = cg_fixture();
        let p = d.path().join("sample.c");
        let out = cg_call(serde_json::json!({
            "path": p.to_str().unwrap(), "kind": "function", "name": "decay"
        }))
        .await
        .unwrap();
        assert_eq!(cg_total(&out), 1, "name 过滤应只留 1 个: {out:?}");
        assert!(cg_matches(&out).contains("pac_decay_all"));
    }

    #[tokio::test]
    async fn code_graph_e2e_c_struct_and_macro() {
        if !cg_have_ast_grep() {
            eprintln!("skip: ast-grep 未安装");
            return;
        }
        let d = cg_fixture();
        let p = d.path().join("sample.c");

        let s = cg_call(serde_json::json!({
            "path": p.to_str().unwrap(), "kind": "struct"
        }))
        .await
        .unwrap();
        assert!(cg_total(&s) >= 1, "C struct 应有命中: {s:?}");
        assert!(cg_matches(&s).contains("pac_s"), "{}", cg_matches(&s));

        let m = cg_call(serde_json::json!({
            "path": p.to_str().unwrap(), "kind": "macro"
        }))
        .await
        .unwrap();
        assert!(cg_total(&m) >= 1, "C macro 应有命中: {m:?}");
        assert!(cg_matches(&m).contains("PAC_MAGIC"), "{}", cg_matches(&m));
    }

    #[tokio::test]
    async fn code_graph_e2e_directory_requires_lang() {
        if !cg_have_ast_grep() {
            eprintln!("skip: ast-grep 未安装");
            return;
        }
        let d = cg_fixture();
        let dir = d.path().to_str().unwrap().to_string();

        // 目录推断不出语言 → 必须报错并列出可选 lang，而不是静默 0 命中。
        let err = cg_call(serde_json::json!({ "path": &dir, "kind": "function" }))
            .await
            .expect_err("目录不带 lang 应报错");
        let msg = err.to_string();
        assert!(msg.contains("lang"), "报错应提示传 lang: {msg}");

        // 显式传 lang 后应能跨文件工作。
        let out = cg_call(serde_json::json!({
            "path": &dir, "kind": "function", "lang": "c"
        }))
        .await
        .unwrap();
        assert!(cg_total(&out) >= 3, "显式 lang=c 应命中 C 函数: {out:?}");
    }

    #[tokio::test]
    async fn code_graph_e2e_unsupported_kind_lists_available() {
        if !cg_have_ast_grep() {
            eprintln!("skip: ast-grep 未安装");
            return;
        }
        let d = cg_fixture();
        let p = d.path().join("sample.c");
        // C 没有 trait —— 必须明确告知可用 kind，而不是 0 命中让模型瞎猜。
        let err = cg_call(serde_json::json!({
            "path": p.to_str().unwrap(), "kind": "trait"
        }))
        .await
        .expect_err("C 的 trait 应报错");
        let msg = err.to_string();
        assert!(msg.contains("function"), "报错应列出可用 kind: {msg}");
        assert!(msg.contains("struct"), "报错应列出可用 kind: {msg}");
    }

    #[tokio::test]
    async fn code_graph_e2e_raw_pattern_escape_hatch() {
        if !cg_have_ast_grep() {
            eprintln!("skip: ast-grep 未安装");
            return;
        }
        let d = cg_fixture();
        let p = d.path().join("sample.c");
        // kind 覆盖不到的查询交给模型自己写 pattern。
        //
        // 注意 C 的 pattern 限制：裸的 `helper($$$ARGS)` 在 C 里命中 0，
        // 因为它会被解析成 K&R 风格的函数声明而不是 call_expression
        // （实测：`helper($$$ARGS)` → 0，`helper(3)` → 1）。这正是为什么
        // 查调用点要用 kind=call + name，而不是裸 pattern（见下一个测试）。
        // 这里用一个在 C 里确实可用的形态。
        let out = cg_call(serde_json::json!({
            "path": p.to_str().unwrap(), "pattern": "int $N($$$P) { $$$B }"
        }))
        .await
        .unwrap();
        assert!(cg_total(&out) >= 1, "裸 pattern 应有命中: {out:?}");
        assert!(cg_matches(&out).contains("pac_npages_get"), "{}", cg_matches(&out));
    }

    #[tokio::test]
    async fn code_graph_e2e_call_sites_via_kind_beats_raw_pattern() {
        if !cg_have_ast_grep() {
            eprintln!("skip: ast-grep 未安装");
            return;
        }
        let d = cg_fixture();
        let p = d.path().join("sample.c");

        // 语义路线：kind=call + name → 找齐 pac_decay_all 里对 pac_init
        // 的 2 处调用。这是"谁调用了 X"这类问题的正确用法。
        let via_kind = cg_call(serde_json::json!({
            "path": p.to_str().unwrap(), "kind": "call", "name": "pac_init"
        }))
        .await
        .unwrap();
        assert_eq!(cg_total(&via_kind), 2, "kind=call 应找到 2 处调用: {via_kind:?}");

        // 裸 pattern 路线在 C 上不可靠（K&R 声明歧义）。锁住这个差异，
        // 避免以后有人把 call 的实现"优化"回 pattern。
        let via_pattern = cg_call(serde_json::json!({
            "path": p.to_str().unwrap(), "pattern": "pac_init($$$ARGS)"
        }))
        .await
        .unwrap();
        assert!(
            cg_total(&via_pattern) < cg_total(&via_kind),
            "C 上裸 pattern 查调用点不应优于 kind 路线（pattern={}, kind={}）",
            cg_total(&via_pattern), cg_total(&via_kind)
        );
    }

    #[tokio::test]
    async fn code_graph_e2e_zero_match_has_actionable_note() {
        if !cg_have_ast_grep() {
            eprintln!("skip: ast-grep 未安装");
            return;
        }
        let d = cg_fixture();
        let p = d.path().join("sample.c");
        let out = cg_call(serde_json::json!({
            "path": p.to_str().unwrap(), "kind": "function", "name": "no_such_symbol_xyz"
        }))
        .await
        .unwrap();
        assert_eq!(cg_total(&out), 0);
        let note = out.get("note").and_then(|v| v.as_str()).unwrap_or("");
        assert!(!note.is_empty(), "0 命中必须给排查提示");
        assert!(note.contains("kind") || note.contains("lang"), "提示不可操作: {note}");
    }
}
