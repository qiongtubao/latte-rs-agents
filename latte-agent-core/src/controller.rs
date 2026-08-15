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
use std::time::Instant;

use latte_ai::models::{Message, Role as MsgRole};
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
use crate::trace::{FanoutSink, ModelErrorKind};
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
        AgentError::MaxToolRoundsExceeded(_) => ModelErrorKind::Other { message: "max tool rounds exceeded".into() },
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

pub(crate) struct ChatEventTraceSink {
    pub(crate) event_tx: broadcast::Sender<ChatEvent>,
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
                    });
                }
            }
            crate::trace::TraceEvent::ModelDelta { meta, delta } => {
                if !delta.is_empty() {
                    let _ = self.event_tx.send(ChatEvent::RoleTurn {
                        role_id: meta.role,
                        content: delta,
                        is_complete: false,
                        sub_id: None,
                    });
                }
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
                    });
                }
                crate::trace::ToolStatus::Err(error) => {
                    let _ = self.event_tx.send(ChatEvent::ToolError {
                        role_id: meta.role,
                        tool_name: name,
                        error: truncate_event_text(&error, 1_500),
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
    RoleStarted { role_id: String, detail: String },
    /// A role has finished working.
    RoleFinished { role_id: String, detail: String },
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
    ToolUse {
        role_id: String,
        tool_name: String,
        args: String,
    },
    /// Tool result (for display in the editor UI).
    ToolResult {
        role_id: String,
        tool_name: String,
        result: String,
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
    },
    /// Specialist returned (or failed/timeout). `status` is one of
    /// `"ok" | "failed" | "timeout" | "cancelled"`. `summary` is the
    /// specialist's last assistant turn on success, or the error
    /// message on failure — suitable for showing in the activity
    /// stream and for the manager to consume as the tool result.
    /// `sub_id` matches the `DelegateStarted.sub_id` so the UI can
    /// look up the full subsession transcript via `/api/sessions/...`.
    DelegateFinished {
        from_role: String,
        to_role: String,
        status: String,
        summary: String,
        sub_id: String,
    },
    ToolError {
        role_id: String,
        tool_name: String,
        error: String,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
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

/// `ask` 工具的单个选项。前端 `ChoiceRequested` 弹框逐项渲染。
/// 字段与前端 `api.ts` 的 `ChoiceOption` 同构。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChoiceOption {
    /// 选项显示标签，必填非空。
    pub label: String,
    /// 可选补充说明，显示在标签下方。空则不序列化。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// 可选配图 URL（一般是 `/api/images/<file>`）。列表模式显示为
    /// 缩略图，`layout="grid"` 时显示为大图卡片。空则不序列化。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub image: String,
    /// 标记为推荐项；前端加「推荐」角标。默认 false。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recommended: bool,
}

// ─── Controller input ────────────────────────────────────────────

enum ControllerInput {
    Input(String),
    Pause,
    Resume,
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

/// Event-driven chat session controller.
pub struct ChatController {
    input_tx: tokio::sync::Mutex<Option<mpsc::UnboundedSender<ControllerInput>>>,
    event_tx: broadcast::Sender<ChatEvent>,
    cancel_flag: Arc<AtomicBool>,
    pause_requested: Arc<AtomicBool>,
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
    /// `submit_input`. The AdvisorMonitor reads it to give the LLM
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
            pause_requested: Arc::new(AtomicBool::new(false)),
            turn_cancel_flag: Arc::new(AtomicBool::new(false)),
            advisor_hints: Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new())),
            last_user_input: Arc::new(parking_lot::Mutex::new(String::new())),
            plan_stage: Arc::new(parking_lot::RwLock::new(PlanStage::Normal)),
            advisor_pause: AdvisorPauseGate::new(),
            last_failed_workflow: Arc::new(parking_lot::RwLock::new(None)),
            agent_pause_gate,
        }
    }

    /// Start the driver loop. Returns a receiver that gets all events.
    pub async fn spawn(
        &self,
        config: ControllerConfig,
    ) -> broadcast::Receiver<ChatEvent> {
        let (new_tx, input_rx) = mpsc::unbounded_channel();
        let _old = std::mem::replace(&mut *self.input_tx.lock().await, Some(new_tx));
        drop(_old);

        let event_tx = self.event_tx.clone();
        let cancel_flag = self.cancel_flag.clone();
        let pause_flag = self.pause_requested.clone();
        let turn_cancel_flag = self.turn_cancel_flag.clone();
        let advisor_hints = self.advisor_hints.clone();
        let plan_stage = self.plan_stage.clone();
        let advisor_pause = self.advisor_pause.clone();
        let agent_pause_gate = self.agent_pause_gate.clone();

        // 内部订阅者：跟踪 WorkflowFinished 事件，维护
        // last_failed_workflow 状态。
        let state_slot = self.last_failed_workflow.clone();
        let mut state_rx = self.event_tx.subscribe();
        tokio::spawn(async move {
            while let Ok(ev) = state_rx.recv().await {
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
                pause_flag,
                turn_cancel_flag,
                advisor_hints,
                plan_stage,
                advisor_pause,
                agent_pause_gate,
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
        if self.pause_requested() {
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
            let _ = tx.send(ControllerInput::Input(text.to_string()));
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
        self.pause_requested.store(false, Ordering::SeqCst);
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
    /// Session-level 恢复（与 [`pause_session`] 配对）。返回 paused
    /// 时长 ms；若本来就没 paused 返回 `None`。Resumed 事件同样由
    /// on_change listener 统一广播。
    pub fn resume_session(&self) -> Option<u128> {
        self.agent_pause_gate.resume()
    }

    /// 当前是否有未拍板的 advisor 暂停请求（测试与嵌入方断言用）。
    pub fn pause_requested(&self) -> bool {
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
    pause_flag: Arc<AtomicBool>,
    turn_cancel_flag: Arc<AtomicBool>,
    advisor_hints: Arc<parking_lot::Mutex<std::collections::VecDeque<String>>>,
    plan_stage: SharedPlanStage,
    advisor_pause: AdvisorPauseGate,
    agent_pause_gate: std::sync::Arc<crate::pause_gate::AgentPauseGate>,
) {
    let is_multi = config.roles.len() > 1 || config.task_id.is_some();

    if is_multi {
        run_multi_role_loop(
            config,
            &mut input_rx,
            event_tx,
            cancel_flag,
            &*pause_flag,
            turn_cancel_flag,
            advisor_hints,
            plan_stage,
            advisor_pause,
            agent_pause_gate,
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
        )
        .await;
    }

    let _ = event_tx.send(ChatEvent::Done);
}

// ─── Multi-role HIL mode ─────────────────────────────────────────

async fn run_multi_role_loop(
    config: ControllerConfig,
    input_rx: &mut mpsc::UnboundedReceiver<ControllerInput>,
    event_tx: &broadcast::Sender<ChatEvent>,
    cancel_flag: Arc<AtomicBool>,
    pause_flag: &AtomicBool,
    turn_cancel_flag: Arc<AtomicBool>,
    advisor_hints: Arc<parking_lot::Mutex<std::collections::VecDeque<String>>>,
    plan_stage: SharedPlanStage,
    advisor_pause: AdvisorPauseGate,
    agent_pause_gate: std::sync::Arc<crate::pause_gate::AgentPauseGate>,
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
            Some(session_arc.clone()),
            event_tx,
            &config.cwd,
            config.subsession_store.clone(),
            &config.session_id,
            cancel_flag.clone(),
            turn_cancel_flag.clone(),
            config.advisor_monitor.runner_gate(),
            advisor_pause.clone(),
            &plan_stage,
            agent_pause_gate.clone(),
            config.stream_mode.clone(),
        )
        .await
        {
            Ok((mut runner, _canonical_id)) => {
                runner = runner.with_inject_worktree_root(worktree_root.clone());
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
    'rounds: for round_num in 1..=scheduler.max_rounds {
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
                    let mut mgr = session_arc.lock().await;
                    let _ = mgr.pause("user /pause");
                    let _ = event_tx.send(ChatEvent::Paused {
                        reason: "用户暂停".into(),
                    });
                    break 'rounds;
                }
                Some(ControllerInput::Abort) => break 'rounds,
                // 取消当前 turn：仅打断正在跑的 run_turn，不退出
                // 整个 round 循环。turn_cancel_flag 会被 run_turn
                // 的 select! 循环检测到，丢弃该轮结果；下一轮用户
                // 输入照常接收。
                Some(ControllerInput::CancelTurn) => {
                    turn_cancel_flag.store(true, Ordering::SeqCst);
                }
                Some(ControllerInput::Resume) => {}
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
                    let mut mgr = session_arc.lock().await;
                    let _ = mgr.pause("user /pause");
                    let _ = event_tx.send(ChatEvent::Paused {
                        reason: "用户暂停".into(),
                    });
                    break 'rounds;
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
                    let _ = event_tx.send(ChatEvent::Status {
                        message: format!(
                            "[round: {} / {}]",
                            mgr.record().current_turn,
                            scheduler.max_rounds
                        ),
                    });
                    continue 'rounds;
                }
                cmd => {
                    let parts: Vec<&str> = line.splitn(2, ' ').collect();
                    let topic = parts.get(1).unwrap_or(&"").trim();
                    match run_workflow_command(cmd, topic, &config, &event_tx, cancel_flag.clone(), agent_pause_gate.clone(), advisor_pause.clone()).await {
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
            // 未批准的 plan 不再约束 delegate。
            *plan_stage.write() = PlanStage::Normal;
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

                // Drain inject queue
                let inject_path = mgr
                    .worktree_root()
                    .join(".latte")
                    .join("inject")
                    .join(format!("{role_id}.txt"));
                if inject_path.exists() {
                    if let Ok(content) = std::fs::read_to_string(&inject_path) {
                        if !content.trim().is_empty() {
                            let synth = Message::user(format!("[INJECTED]\n{}", content));
                            mgr.append_to_role(role_id, synth).ok();
                        }
                        let _ = std::fs::remove_file(&inject_path);
                    }
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
            });

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
            // Check accumulated token usage against the session token
            // budget. Uses the runner's actual `total_usage` (not a
            // hardcoded placeholder). If the budget is exceeded, the
            // supervisor triggers an auto-pause and we skip remaining
            // roles in this round.
            let usage = runner.total_usage();
            let tokens_used = usage.input_tokens + usage.output_tokens + usage.thinking_tokens;
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

        // Check pause flag
        if pause_flag.load(Ordering::SeqCst) {
            {
                let mut mgr = session_arc.lock().await;
                let _ = mgr.pause("用户请求暂停");
            }
            let _ = event_tx.send(ChatEvent::Paused {
                reason: "用户请求暂停".into(),
            });
            // Pause-wait loop: only accept Resume or Abort
            'pause: loop {
                if cancel_flag.load(Ordering::SeqCst) {
                    break 'rounds;
                }
                match input_rx.recv().await {
                    Some(ControllerInput::Resume) => {
                        let mut mgr = session_arc.lock().await;
                        if mgr.state() == SessionState::Paused {
                            let _ = mgr.resume();
                        }
                        pause_flag.store(false, Ordering::SeqCst);
                        let _ = event_tx.send(ChatEvent::Resumed);
                        break 'pause;
                    }
                    Some(ControllerInput::Abort) | None => break 'rounds,
                    Some(ControllerInput::AdvisorHint(text)) => {
                        // Paused: park the hint; the runner drains it
                        // when the session resumes.
                        advisor_hints.lock().push_back(text);
                    }
                    _ => {}
                }
            }
        }
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
        None,
        event_tx,
        &config.cwd,
        config.subsession_store.clone(),
        &config.session_id,
        cancel_flag.clone(),
        turn_cancel_flag.clone(),
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
                        if trimmed.is_empty() {
                        }

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
                                    match build_runner(merged, resolver, default_params, new_role, current_tier, current_primary.as_deref(), None, event_tx, &config.cwd, config.subsession_store.clone(), &config.session_id, cancel_flag.clone(), turn_cancel_flag.clone(), config.advisor_monitor.runner_gate(), advisor_pause.clone(), &plan_stage, agent_pause_gate.clone(), config.stream_mode.clone()).await {
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
                                            match build_runner(merged, resolver, default_params, &role, new_tier, current_primary.as_deref(), None, event_tx, &config.cwd, config.subsession_store.clone(), &config.session_id, cancel_flag.clone(), turn_cancel_flag.clone(), config.advisor_monitor.runner_gate(), advisor_pause.clone(), &plan_stage, agent_pause_gate.clone(), config.stream_mode.clone()).await {
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
                                    match run_workflow_command(cmd, topic, &config, &event_tx, cancel_flag.clone(), agent_pause_gate.clone(), advisor_pause.clone()).await {
                                        Ok(Some(summary)) => {
                                            let _ = event_tx.send(ChatEvent::Status {
                                                message: format!("Workflow '{cmd}' 完成。\n{summary}"),
                                            });
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
                                        }
                                    }
                                }
                            }
                            continue;
                        }

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

                        let _ = event_tx.send(ChatEvent::Status { message: format!("[calling LLM for role '{current_role}'...]") });
                        // plan 阶段门复位：新的用户消息 = 新的决策周期，
                        // 上一轮未批准的 plan 不再约束 delegate。
                        *plan_stage.write() = PlanStage::Normal;
                        let _ = event_tx.send(ChatEvent::UserMessage { text: trimmed.clone() });
                        let _ = event_tx.send(ChatEvent::RoleStarted {
                            role_id: current_role.clone(),
                            detail: "calling LLM".into(),
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
                                });
                                let _ = event_tx.send(ChatEvent::Prompt { icon: ico, role_id: current_role.clone(), model_id: mid });
                            }
                            Err(e) => {
                                let _ = event_tx.send(ChatEvent::RoleFinished {
                                    role_id: current_role.clone(),
                                    detail: format!("error: {e}"),
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
                        let history: Vec<Message> = runner.context().messages().to_vec();
                        match build_runner(merged, resolver, default_params, &new_role, current_tier, current_primary.as_deref(), None, event_tx, &config.cwd, config.subsession_store.clone(), &config.session_id, cancel_flag.clone(), turn_cancel_flag.clone(), config.advisor_monitor.runner_gate(), advisor_pause.clone(), &plan_stage, agent_pause_gate.clone(), config.stream_mode.clone()).await {
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
                        match build_runner(merged, resolver, default_params, &current_role, new_tier, current_primary.as_deref(), None, event_tx, &config.cwd, config.subsession_store.clone(), &config.session_id, cancel_flag.clone(), turn_cancel_flag.clone(), config.advisor_monitor.runner_gate(), advisor_pause.clone(), &plan_stage, agent_pause_gate.clone(), config.stream_mode.clone()).await {
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
                    Some(ControllerInput::Abort) | None => break,
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
    session: Option<Arc<Mutex<SessionManager>>>,
    event_tx: &broadcast::Sender<ChatEvent>,
    cwd: &Path,
    subsession_store: Arc<SubsessionStore>,
    // UI session id —— 见 [`ControllerConfig::session_id`]。透传给
    // `register_delegate_tool`，最终到 `subsession_store.create()`。
    // 空串 → 旧行为（用 cwd 当 key，落盘会被拒绝）。
    session_id: &str,
    cancel_flag: Arc<AtomicBool>,
    turn_cancel_flag: Arc<AtomicBool>,
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
        resolver.resolve_chain(&role.id, tier, &role.model_chain)?
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
            register_plan_tool(&tm, event_tx.clone(), role_id.to_string(), plan_stage.clone())
                .map_err(|e| AgentError::Tool(format!("register plan: {e}")))?;
        }
        // Any role with "ask" in allowed_tools gets the ask tool:
        // 向用户抛出一道选择题（含图片 / 图片网格 / 上传），弹出选择框。
        if role.allowed_tools.iter().any(|t| t == "ask") {
            register_ask_tool(&tm, event_tx.clone(), role_id.to_string())
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
        });
        let runner_sink: Arc<dyn crate::trace::TraceSink> = match subsession_sink {
            Some(sub) => Arc::new(crate::trace::FanOutSink::new(vec![sub, chat_sink])),
            None => chat_sink,
        };
        let mut runner = AgentRunner::new_with_tools(agent, tm, 16)
            .with_role(role_id)
            .with_cwd(cwd.to_path_buf());
        runner = runner.with_sink(runner_sink);
        if let Some(gate) = advisor_gate.clone() {
            runner = runner.with_gate_config(gate);
        }
        runner = runner.with_agent_pause_gate(agent_pause_gate);
        runner = runner.with_stream_mode(stream_mode);
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
        });
        let runner_sink: Arc<dyn crate::trace::TraceSink> = match subsession_sink {
            Some(sub) => Arc::new(crate::trace::FanOutSink::new(vec![sub, chat_sink])),
            None => chat_sink,
        };
        let mut runner = AgentRunner::new(agent)
            .with_role(role_id)
            .with_cwd(cwd.to_path_buf());
        if let Some(gate) = advisor_gate {
            runner = runner.with_gate_config(gate);
        }
        runner = runner.with_agent_pause_gate(agent_pause_gate);
        runner = runner.with_stream_mode(stream_mode);
        Ok((runner, role_id.to_string()))
    }
}


pub(crate) async fn build_tool_manager(
    allowed: &[String],
) -> Result<Arc<dyn latte_rs_agent_tools::types::ToolManager>, Box<dyn std::error::Error + Send + Sync>> {
    use latte_rs_agent_tools::prelude::*;
    let mgr = create_tool_manager();
    for p in builtin_tool_packages() {
        mgr.register_package(p).await
            .map_err(|e| format!("register_package: {e}"))?;
    }
    // allowed 里是配置层扁平名（bash/read/edit/...）。latte-rs-agent-tools
    // 扁平化之后 registry 名与配置名一致（"bash"/"read"/"git_status"/...），
    // 直接进 keep；仅个别工具仍带点号注册名，需要补一条映射。
    let mut keep: std::collections::HashSet<String> = allowed
        .iter()
        .flat_map(|s| vec![s.to_lowercase(), s.clone()])
        .collect();
    // 配置层扁平名 → registry 注册名。只剩 browser/todo 两个包仍用
    // 点号注册名（browser.browser / todo.todo）。
    let flat_to_registry: std::collections::HashMap<&str, &str> = [
        ("todo", "todo.todo"),
        ("browser", "browser.browser"),
    ].into_iter().collect();
    for (flat, registry) in &flat_to_registry {
        if keep.contains(*flat) {
            keep.insert(registry.to_string());
            // 注册名本身进 keep，直接匹配 registry 名。
        }
    }
    // mcp 是配置层分组别名（一个名字展开成 3 个 mcp_* 工具），不是工具名别名。
    if keep.contains("mcp") {
        keep.insert("mcp_connect".to_string());
        keep.insert("mcp_list".to_string());
        keep.insert("mcp_call".to_string());
    }
    // playwright 同理：展开成 playwright_script + screenshot（扁平化后
    // 截图工具独立注册为 "screenshot"，原 playwright_screenshot 已不存在）。
    if keep.contains("playwright") {
        keep.insert("playwright_script".to_string());
        keep.insert("screenshot".to_string());
    }
    // Register code_graph tool if allowed
    if keep.contains("code_graph") || keep.contains("code-graph") {
        let cg = code_graph_tool();
        mgr.register(cg, None);
    }
    // 过滤：注册名全名或短名（点号后缀）命中 keep 就保留。
    // 短名兜底兼容仍带点号注册名的工具（browser.browser/todo.todo）
    // 以及无点号的扁平名工具（short == 全名）。
    for tool_id in mgr.get_tool_names() {
        let short = tool_id
            .rsplit_once('.')
            .map(|(_, s)| s.to_string())
            .unwrap_or_else(|| tool_id.clone());
        if !(keep.contains(&short) || keep.contains(&tool_id)) {
            mgr.unregister(&tool_id);
        }
    }
    Ok(mgr)
}

fn code_graph_tool() -> latte_rs_agent_tools::types::Tool {
    use latte_rs_agent_tools::types::*;
    use latte_rs_agent_tools::error::ToolError;
    use std::sync::Arc;
    use serde_json::json;

    fn prop(ty: PropertyType, desc: &str) -> ToolInputProperty {
        ToolInputProperty {
            property_type: ty,
            description: Some(desc.into()),
            enum_values: None, minimum: None, maximum: None, min_length: None, max_length: None,
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
            let pattern = input.get("pattern")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::other("pattern is required"))?;
            let path = input.get("path")
                .and_then(|v| v.as_str())
                .unwrap_or(".");
            let kind = input.get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("function");
            let query = match kind {
                "function" => format!("fn $NAME($$$PARAMS) -> $RET {{ $$$BODY }}"),
                "struct" => format!("struct $NAME {{ $$$FIELDS }}"),
                "class" => format!("class $NAME {{ $$$BODY }}"),
                "import" => format!("import {{ $$$IMPORTS }} from \"$SRC\""),
                "interface" => format!("interface $NAME {{ $$$BODY }}"),
                "trait" => format!("trait $NAME {{ $$$BODY }}"),
                "impl" => format!("impl $NAME {{ $$$BODY }}"),
                "call" => format!("$CALLEE($$$ARGS)"),
                _ => pattern.to_string(),
            };
            let output = tokio::process::Command::new("sg")
                .args(["-p", &query, path])
                .output().await
                .map_err(|e| ToolError::execution_str("code_graph", format!("sg failed: {e}")))?;
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let mut result = serde_json::Map::new();
            result.insert("matches".into(), serde_json::Value::String(stdout));
            if !stderr.is_empty() {
                result.insert("stderr".into(), serde_json::Value::String(stderr));
            }
            Ok(serde_json::Value::Object(result))
        })
    });
    let schema = optional_schema(vec![
        ("pattern", PropertyType::String, "AST 模式，如 fn $NAME($$$PARAMS) -> $RET { $$$BODY }"),
        ("path", PropertyType::String, "搜索路径，默认当前目录"),
        ("kind", PropertyType::String, "查询类型: function/struct/class/import/interface/trait/impl/call"),
    ]);
    Tool::builder("code_graph", "用 AST 模式查询代码结构，比 grep 更精准。支持多种结构类型查询", schema, handler)
        .timeout(std::time::Duration::from_secs(30))
        .build()
}

pub(crate) fn tool_usage_prompt(allowed: &[String]) -> String {
    let names_str = allowed.join(", ");
    format!(
        r#"
## Tools

You have access to the following tools: {names_str}.
Call tools via the native function-calling interface (the request `tools`
field carries each tool's name, description, and JSON schema). Do NOT emit
`<tool_call>` text blocks -- they are no longer parsed. Inspect each tool
result and continue until the task is done.
"#
    )
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


/// 单 session 累计 delegate 工具调用上限（默认 12）。env `LATTE_MAX_DELEGATES_PER_SESSION`
/// 覆盖；非法值回退到默认。0 = 禁用限制（保留字段 compatibility）。
/// 详见 `docs/perf/diagnose-latency.md` §5（#1-A 方案）。
pub fn default_max_delegates() -> u32 {
    let raw = std::env::var("LATTE_MAX_DELEGATES_PER_SESSION").ok();
    match raw.and_then(|s| s.parse::<u32>().ok()) {
        Some(n) => n,
        None => 12,
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
"#
    )
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
pub(crate) fn register_plan_tool(
    tm: &Arc<dyn latte_rs_agent_tools::types::ToolManager>,
    event_tx: broadcast::Sender<ChatEvent>,
    role_id: String,
    plan_stage: SharedPlanStage,
) -> Result<(), Box<dyn std::error::Error>> {
    use latte_rs_agent_tools::types::{
        PropertyType, SchemaType, SharedToolHandler, Tool, ToolInputProperty, ToolInputSchema,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    // 进程级单调序号，保证 plan_id 全局唯一。
    static PLAN_SEQ: AtomicU64 = AtomicU64::new(0);

    // tasks 是数组；ToolInputProperty 无嵌套 items schema，故用描述
    // 把每项结构讲清（title/description/priority/labels/workflow/
    // paths/subtasks）。LLM 按描述产出，handler 逐项 serde 解析 + 校验。
    let input_schema = ToolInputSchema {
        schema_type: SchemaType,
        properties: vec![
            ("tasks".into(), ToolInputProperty {
                property_type: PropertyType::Array,
                description: Some(
                    "任务候选清单（一次调用提交整份清单：拆分出几个任务就放几项，禁止每个任务单独调一次本工具——上一份清单未获用户批准时后续调用会被拒绝）。每项是对象：{title(必填,一句话), description(做什么+验收标准), priority(1-4,1最高), labels(字符串数组), workflow(执行该任务的workflow名:tdd_development/bug_triage/update_docs/annotate_code;没有贴合的必须留空走manager直接执行,禁止硬绑不相关的workflow), paths(可选,字符串数组,任务涉及的文件/目录前缀如\"src/ringbuf\";并行执行时范围重叠的任务会被拒绝派发,拆任务时让各任务范围互不重叠), subtasks(同构数组,最多一层)}. 调用本工具后任务会出现在用户弹窗里供勾选导入任务看板，不要再以 Markdown 列表输出任务。".into()
                ),
                enum_values: None, minimum: None, maximum: None, min_length: None, max_length: None,
            }),
        ].into_iter().collect(),
        required: Some(vec!["tasks".into()]),
        ..Default::default()
    };

    let handler_role_id = role_id.clone();
    let handler: SharedToolHandler = Arc::new(move |input: serde_json::Value, _ctx| {
        let event_tx = event_tx.clone();
        let role_id = handler_role_id.clone();
        let plan_stage = plan_stage.clone();
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

            let plan_id = format!(
                "plan-{}-{}",
                role_id,
                PLAN_SEQ.fetch_add(1, Ordering::Relaxed)
            );
            let n = tasks.len();
            let _ = event_tx.send(ChatEvent::PlanProposed {
                role_id: role_id.clone(),
                plan_id: plan_id.clone(),
                tasks,
            });
            // plan 阶段门：任务清单已提交，等用户在弹窗导入任务看板；
            // 批准前 delegate 实现类角色会被工具层拒绝。
            *plan_stage.write() = PlanStage::PendingApproval {
                plan_id: plan_id.clone(),
            };
            Ok(serde_json::Value::String(format!(
                "已提交 {n} 个任务候选给用户选择（plan_id={plan_id}）。请在弹窗中勾选要导入任务看板的项；若弹窗已关闭，可右键本条消息选「导入任务看板」补救。"
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

/// 注册 `ask` 工具：角色向用户抛出一道**选择题**（可带图片、图片
/// 网格、允许上传自定义图片），会话据此弹出选择框。
///
/// 语义（与 `plan` 一样是 UI 侧交互，不读写文件、不调模型）：校验
/// 入参 → 广播 `ChatEvent::ChoiceRequested` → 返回一段“已展示选择题、
/// 请简短引导后结束本轮”的指令串。工具返回 `Ok`（而非 `ask_human`
/// 的 `Err`）：web 单角色 loop 无 SessionManager 可暂停，靠模型拿到
/// 该结果后自然收尾本轮；用户在弹框里选完后，选择结果作为下一条
/// user 消息（`/chat/send`）回喂角色，模型据此继续。
pub(crate) fn register_ask_tool(
    tm: &Arc<dyn latte_rs_agent_tools::types::ToolManager>,
    event_tx: broadcast::Sender<ChatEvent>,
    role_id: String,
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
                    "候选项数组，2-6 项。每项是对象：{label(必填,简短标签), description(可选,一行取舍说明), image(可选,配图URL,一般是 /api/images/<file>), recommended(可选,true 标记推荐项)}. 不要自己加“其他/Other”项——前端会自动附带“其他(自定义)”入口。".into()
                ),
                enum_values: None, minimum: None, maximum: None, min_length: None, max_length: None,
            }),
            ("multi".into(), ToolInputProperty {
                property_type: PropertyType::Boolean,
                description: Some("是否允许多选（默认 false = 单选）。".into()),
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
        Box::pin(async move {
            let tool_err = |msg: String| latte_rs_agent_tools::error::ToolError::Other(msg);

            let question = input
                .get("question")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| tool_err("missing non-empty 'question' field".into()))?;

            let opts_arr = input
                .get("options")
                .and_then(|v| v.as_array())
                .ok_or_else(|| tool_err("'options' must be an array".into()))?;
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

            let multi = input.get("multi").and_then(|v| v.as_bool()).unwrap_or(false);
            let allow_upload = input.get("allow_upload").and_then(|v| v.as_bool()).unwrap_or(false);
            let layout = input
                .get("layout")
                .and_then(|v| v.as_str())
                .filter(|s| *s == "grid")
                .unwrap_or("")
                .to_string();

            let choice_id = format!("choice-{}-{}", role_id, CHOICE_SEQ.fetch_add(1, Ordering::Relaxed));
            let n = options.len();
            let _ = event_tx.send(ChatEvent::ChoiceRequested {
                role_id: role_id.clone(),
                choice_id: choice_id.clone(),
                question,
                multi,
                layout,
                allow_upload,
                options,
            });
            Ok(serde_json::Value::String(format!(
                "已向用户展示 {n} 个选项的选择框（choice_id={choice_id}）。请输出一句简短引导语（例如「请在上方选择」），然后结束本轮，不要调用其他工具，也不要臆测用户会选哪个——等待用户在弹框里选择后再继续。"
            )))
        })
    });

    let tool = Tool::builder(
        "ask".to_string(),
        "向用户抛出一道选择题并弹出选择框（支持单选/多选、每项可带配图、图片网格挑选、允许上传自定义图片）。当存在多个取舍明显不同、需要用户拍板的方案时使用；不要用于可自行决定的琐碎问题。参数：question(问题) + options(候选项数组) + 可选 multi/layout/allow_upload。".to_string(),
        input_schema,
        handler,
    )
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
pub(crate) fn register_task_report_tool(
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
    role_id: &str,
    role_responsibilities: &str,
    task: &str,
    response: String,
) -> String {
    use crate::advisor_monitor::Verdict;
    // Bound the review so a slow/absent advisor model can't stall the
    // delegate return (mirrors the monitor's REVIEW_TIMEOUT_SECS).
    let review = tokio::time::timeout(
        std::time::Duration::from_secs(45),
        engine.review_delegate(role_id, role_responsibilities, task, &response),
    )
    .await;
    let verdict = match review {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            tracing::warn!("delegate-return review failed (degraded): {e}");
            return response;
        }
        Err(_) => {
            tracing::warn!("delegate-return review timed out; passing through");
            return response;
        }
    };
    if verdict.verdict == Verdict::Ok {
        return response;
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
    if matches!(verdict.verdict, Verdict::Intervene | Verdict::Terminate) {
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
    }
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
    let review_engine = Arc::new(AdvisorReviewEngine::new(
        merged_owned.clone(),
        resolver_owned.clone(),
        default_params.clone(),
    ));
    let cancel_flag_owned = Arc::clone(&cancel_flag);
    let turn_cancel_flag_owned = turn_cancel_flag.clone();
    let delegate_counter_owned = Arc::clone(&delegate_counter);
    let max_delegates_owned = max_delegates;
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
        // 每次 specialist 创建时 attach。
        let agent_pause_gate = agent_pause_gate.clone();
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
            });
            // Fan out ONLY to the subsession memory sink: the
            // `sub_sink` is `Arc<dyn TraceSink>` returning from
            // `SubsessionStore::create` —— 内部已 fanout 到
            // MemorySink（实时读源）+ DiskSink（落盘备份，主
            // session 删除时联删）。这里**不要**再包一层
            // FanoutSink，否则事件会被写两份到内存。
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
            let tier = role.default_model_tier;
            let models = resolver
                .resolve_chain(&role.id, tier, &role.model_chain)
                .map_err(|e| {
                    tool_err(format!("no model for role '{}': {}", role_id, e))
                })?;

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
                });
                tool_err(summary)
            })?;
            // `max_tool_rounds = 0` → unlimited; the agent decides
            // when it's done. LoopDetector in agent.rs trips on
            // actually stuck patterns.
            // Wire the workspace cwd so the specialist's path-aware
            // tools (`shell.exec`, `file.read`, …) chdir into the
            // workspace the user opened — not the Tauri process
            // cwd. See `AgentRunner::with_cwd` for the contract.
            let mut runner = match specialist_tm {
                Some(tm) => AgentRunner::new_with_tools(agent, tm, 0),
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

            // Emit RoleStarted so the UI shows the specialist is working
            let task_clone = task.clone();
            let role_id_clone = role_id.clone();
            let event_tx_clone = event_tx.clone();
            let _ = event_tx_clone.send(ChatEvent::RoleStarted {
                role_id: role_id_clone.clone(),
                detail: format!("delegated: {}", task_clone),
            });

            // 5. Run the specialist. No wall-clock timeout — the
            //    subagent runs until completion or explicit cancellation
            //    via the session's cancel_flag. Check periodically.
            let _permit = sem.acquire().await.map_err(|_| {
                tool_err("delegate pool shut down".into())
            })?;
            // Move runner + messages into a spawned task so we can
            // cancel it from the select! loop. The task owns everything.
            let task_content = task.clone();
            let mut run_handle = tokio::spawn(async move {
                // gated 版：装了 gate_config 时产出先过 D5/D6；
                // 没装时等价 run_turn。
                runner.run_turn_gated(&[Message::user(task_content)], None).await
            });
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
                    _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                        if cancel_flag.load(Ordering::SeqCst) {
                            run_handle.abort();
                            let _ = event_tx.send(ChatEvent::RoleFinished {
                                role_id: role_id.clone(),
                                detail: "cancelled by user".into(),
                            });
                            let summary = String::from("delegate cancelled by user");
                            let _ = event_tx.send(ChatEvent::DelegateFinished {
                                from_role: "manager".into(),
                                to_role: role_id.clone(),
                                status: "cancelled".into(),
                                summary: summary.clone(),
                                sub_id: sub_id.clone(),
                            });
                            return Err(tool_err(summary));
                        }
                        if turn_cancel_flag.load(Ordering::SeqCst) {
                            run_handle.abort();
                            let _ = event_tx.send(ChatEvent::RoleFinished {
                                role_id: role_id.clone(),
                                detail: "cancelled by user".into(),
                            });
                            let summary = String::from("delegate cancelled by user");
                            let _ = event_tx.send(ChatEvent::DelegateFinished {
                                from_role: "manager".into(),
                                to_role: role_id.clone(),
                                status: "cancelled".into(),
                                summary: summary.clone(),
                                sub_id: sub_id.clone(),
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
            let run_result = match result {
                Ok(response) => Ok(gate_delegate_return(
                    &review_engine,
                    &event_tx,
                    &role_id,
                    &role.system_prompt,
                    &task,
                    response,
                )
                .await),
                Err(e) => Err(e),
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
                    });
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
    _turn_cancel_flag: Arc<AtomicBool>,
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

    let handler: SharedToolHandler = Arc::new(move |input: serde_json::Value, _ctx| {
        let merged = Arc::clone(&merged_owned);
        let resolver = Arc::clone(&resolver_owned);
        let default_params = default_params.clone();
        let event_tx = event_tx.clone();
        let cancel_flag = Arc::clone(&cancel_flag);
        let agent_pause_gate = agent_pause_gate_owned.clone();
        let subsession_store = subsession_store.clone();
        let session_id = session_id.clone();
        let advisor_gate = advisor_gate.clone();
        let advisor_pause = advisor_pause.clone();
        let cwd = cwd.clone();
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
                cancel_flag,
                agent_pause_gate: Some(agent_pause_gate),
                depth: 0,
                subsession_store: Some(subsession_store),
                session_id: Some(session_id),
                advisor_gate,
                advisor_pause: Some(advisor_pause),
            };
            let result = match &resume {
                Some(rid) => crate::workflow::run_workflow_resume(&wf, &topic, &ctx, rid).await,
                None => crate::workflow::run_workflow(&wf, &topic, &ctx).await,
            };
            result
                .map(|summary| serde_json::Value::String(strip_think_blocks(&summary)))
                .map_err(tool_err)
        })
    });

    let tool = Tool::builder(
        "workflow".to_string(),
        format!("Run a named multi-role workflow. Available workflows: {available_text}"),
        input_schema,
        handler,
    )
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
        agent_pause_gate: Some(agent_pause_gate),
        depth: 0,
        // 与 manager 的 workflow 工具一致：slash 命令触发的 workflow
        // 分派也建 subsession、过 advisor gate（session_id 为空时
        // subsession 退化到内存，与 delegate 的旧行为一致）。
        subsession_store: Some(config.subsession_store.clone()),
        session_id: Some(config.session_id.clone()),
        advisor_gate: config.advisor_monitor.runner_gate(),
        advisor_pause: Some(advisor_pause),
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

    // ─── Advisor v3 pause gate（controller 侧）─────────────────────

    #[tokio::test]
    async fn advisor_pause_resolves_on_any_user_input() {
        let c = ChatController::new(8);
        assert!(!c.pause_requested());
        c.request_pause();
        assert!(c.pause_requested());
        // 任何用户输入都算拍板（继续）：清旗，不取消 turn。
        c.submit_input("继续").await;
        assert!(!c.pause_requested());
        assert!(!c.turn_cancel_requested());
    }

    #[tokio::test]
    async fn advisor_pause_stop_keyword_cancels_turn() {
        let c = ChatController::new(8);
        c.request_pause();
        // 命中"终止"关键词：resolve + 软终止当前 turn 同时发生。
        c.submit_input("终止本轮吧").await;
        assert!(!c.pause_requested());
        assert!(c.turn_cancel_requested());
    }

    #[tokio::test]
    async fn advisor_resume_also_resolves_pause_gate() {
        let c = ChatController::new(8);
        c.request_pause();
        c.resume().await;
        assert!(!c.pause_requested());
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

    #[test]
    fn chat_event_trace_sink_broadcasts_tool_events() {
        use crate::trace::TraceSink as _;

        let (tx, mut rx) = broadcast::channel(8);
        let sink = ChatEventTraceSink { event_tx: tx };

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

    #[test]
    fn default_max_delegates_respects_env_and_default() {
        // env 缺失 → 默认 12。
        let _g = lock_env();
        std::env::remove_var("LATTE_MAX_DELEGATES_PER_SESSION");
        assert_eq!(default_max_delegates(), 12);
        // env 非法 → 回退默认。
        std::env::set_var("LATTE_MAX_DELEGATES_PER_SESSION", "abc");
        assert_eq!(default_max_delegates(), 12);
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
        register_plan_tool(&tm, event_tx, "manager".into(), stage.clone())
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

    /// 一次一清单防护：上一份清单 PendingApproval 期间，第二次 plan
    /// 调用被拒绝（提示合并到一次调用），且不重复发 PlanProposed。
    /// （坏行为样本：manager 把 11 个任务分 11 次调用，用户弹窗
    /// 每次只有 1 个任务。）
    #[tokio::test]
    async fn plan_tool_rejects_second_call_while_pending() {
        let tm = build_tool_manager(&[]).await.expect("tool manager");
        let (event_tx, mut rx) = broadcast::channel(8);
        let stage = fresh_plan_stage();
        register_plan_tool(&tm, event_tx, "manager".into(), stage.clone())
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
            options: vec![
                ChoiceOption {
                    label: "JWT".into(),
                    description: "无状态 Bearer token".into(),
                    image: "/api/images/jwt.png".into(),
                    recommended: true,
                },
                ChoiceOption {
                    label: "OAuth2".into(),
                    description: String::new(),
                    image: String::new(),
                    recommended: false,
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
        // 第二项：空 description/image + recommended=false 被省略。
        assert_eq!(opts[1]["label"], "OAuth2");
        assert!(opts[1].get("description").is_none(), "空 description 应省略");
        assert!(opts[1].get("image").is_none(), "空 image 应省略");
        assert!(opts[1].get("recommended").is_none(), "recommended=false 应省略");
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
        };
        let wrapped = serde_json::json!({
            "workspaceId": "ws-1",
            "event": inner,
        });
        let parsed: ChatEvent =
            serde_json::from_value(wrapped["event"].clone()).expect("parse");
        match parsed {
            ChatEvent::DelegateStarted { from_role, to_role, task, sub_id } => {
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

    /// 锁定 bash 工具的 schema 契约：allowed "bash" 保留 registry 的
    /// "bash" 工具（命名空间已扁平化），且接受 {command, cwd}
    /// （cwd 由 resolve_tool_input_against_cwd 注入）。
    #[tokio::test]
    async fn bash_tool_kept_and_accepts_cwd() {
        let mgr = build_tool_manager(&["read".into(), "write".into(), "bash".into(), "search".into()])
            .await
            .expect("build_tool_manager");
        let names: Vec<String> = mgr.get_tool_names();
        assert!(names.contains(&"bash".to_string()), "bash 应被保留: {names:?}");
        // bash 必须接受 {command, cwd}。
        let args = serde_json::json!({"command":"pwd","cwd":"/tmp"});
        let r = mgr.execute("bash", args, None).await;
        assert!(r.is_ok(), "bash 应接受 {{command,cwd}}，却失败: {:?}", r.err());
        // eval 不在 allowed 里，被过滤掉。
        assert!(!names.contains(&"eval".to_string()), "eval 不应被保留（不在 allowed）: {names:?}");
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
}

