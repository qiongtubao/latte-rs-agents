//! AdvisorMonitor（advisor 监察者）— side-channel supervisor that watches the
//! manager's event stream and intervenes on anomalies. Design:
//! `docs/advisor-monitor.md`.
//!
//! Pipeline:
//! 1. Subscribe to the controller's `ChatEvent` broadcast.
//! 2. Run zero-cost deterministic detectors (D1–D4) over the watched role's
//!    turn events. A hit immediately injects a deterministic correction hint
//!    into the current runner's shared hint queue (channel A).
//! 3. A hit (or every turn, in `EveryTurn` mode) triggers a single-turn LLM
//!    review by the advisor role (premium tier, no tools). Verdict
//!    `warn`/`intervene` publishes a `RoleTurn { role_id: "advisor" }` bubble
//!    (channel B) and injects the corrective hint.
//!
//! Fault discipline: the monitor is a pure side channel. Detector/review
//! failures (including an unavailable advisor model) degrade to
//! `tracing::warn` — the main session is never interrupted.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use latte_ai::models::Message;
use latte_ai::params::GenerateParams;
use tokio::sync::broadcast;

use crate::agent::{Agent, WaitPolicy};
use crate::config::AgentConfig;
use crate::controller::{ChatController, ChatEvent};
use crate::error::AgentError;
use crate::model_resolver::{ModelResolver, ModelTier};
use crate::AgentResult;

/// Wall-clock budget for one advisor review call. Bounds the stall the
/// monitor loop experiences when the advisor model is slow; the main
/// session is unaffected either way.
const REVIEW_TIMEOUT_SECS: u64 = 120;
/// Char budget for the specialist response inside the delegate-return
/// review prompt (~8K tokens at the 4-chars-per-token heuristic).
/// Sized to fit real specialist reports whole (observed: 25.5K chars).
const DELEGATE_REVIEW_RESPONSE_MAX_CHARS: usize = 32_000;

/// 默认 delegate-return 审查超时（秒）。实测实锤：慢速审查模型
/// （MiniMax-M3 单次 20–44s）在 45s 预算下频繁「未审直接放行」，
/// 审查形同虚设；放宽到 90s 覆盖慢模型的 p95。
pub const DEFAULT_DELEGATE_REVIEW_TIMEOUT_SECS: u64 = 90;
/// 默认返回重做上限（次）。硬上限见 [`MAX_RETURN_REDO`]。
pub const DEFAULT_RETURN_MAX_REDO: u8 = 1;
/// 返回重做上限的硬天花板：防 intervene 判定抖动导致流水线空转。
pub const MAX_RETURN_REDO: u8 = 3;

// ─── Configuration ─────────────────────────────────────────────────

/// delegate-return 审查与重做的可调参数，挂在
/// [`AdvisorMonitorConfig`] 上随 engine / workflow ctx 下发。
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct AdvisorReviewSettings {
    /// `gate_delegate_return` 单次审查超时（秒）。超时/失败都降级
    /// 放行（未审直接通过），所以这里是「审查值多少钱」的预算。
    #[serde(default = "default_delegate_review_timeout_secs")]
    pub delegate_review_timeout_secs: u64,
    /// workflow speaker 返回被判 intervene/terminate 时的重做上限
    /// （每次重做都是全新 subagent）。默认 1，硬上限
    /// [`MAX_RETURN_REDO`]。
    #[serde(default = "default_return_max_redo")]
    pub return_max_redo: u8,
}

impl Default for AdvisorReviewSettings {
    fn default() -> Self {
        Self {
            delegate_review_timeout_secs: DEFAULT_DELEGATE_REVIEW_TIMEOUT_SECS,
            return_max_redo: DEFAULT_RETURN_MAX_REDO,
        }
    }
}

fn default_delegate_review_timeout_secs() -> u64 {
    DEFAULT_DELEGATE_REVIEW_TIMEOUT_SECS
}

fn default_return_max_redo() -> u8 {
    DEFAULT_RETURN_MAX_REDO
}

/// When the LLM review runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdvisorReviewMode {
    /// Never review (deterministic hints still fire).
    Off,
    /// Review only when a detector fired (default).
    OnAnomaly,
    /// Review at the end of every watched-role turn (premium cost).
    EveryTurn,
}

/// Advisor monitor settings, carried by `ControllerConfig`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AdvisorMonitorConfig {
    /// Master switch. Default true.
    pub enabled: bool,
    /// LLM review trigger mode. Default `OnAnomaly`.
    pub review_mode: AdvisorReviewMode,
    /// Circuit breaker: max LLM reviews per sliding time window (cost
    /// control for anomaly storms). Default 2. Was "per turn" — a
    /// 20-minute manager turn would exhaust it in the first minute and
    /// go blind for the rest, so the quota is time-based now.
    pub max_reviews_per_turn: u32,
    /// Sliding window for the review quota, in seconds. Default 120.
    #[serde(default = "default_review_window_secs")]
    pub review_window_secs: u64,
    /// Read project/global 监察笔记 (`advisor-watchdog.md`) into the
    /// review prompt. Default true.
    pub watchdog_notes: bool,
    /// Pre-persistence response gate（D5/D6）配置。controller 的
    /// driver 在 advisor 启用时把它装到每个 runner 上
    /// （`AgentRunner::with_gate_config`）：turn 产出被接受前先过
    /// `check_response_gates`，命中 → 带批注重试（最多
    /// `gate.max_retries` 次）→ 仍命中 →
    /// `AgentError::AdvisorTerminated` 终止本 turn。
    #[serde(default)]
    pub gate: GateConfig,
    /// delegate-return 审查超时与返回重做上限（实测实锤：45s
    /// 硬编码超时让慢审查模型频繁「未审直接放行」；重做上限此前是
    /// workflow.rs 里的硬编码常量）。
    #[serde(default)]
    pub review_settings: AdvisorReviewSettings,
}

impl Default for AdvisorMonitorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            review_mode: AdvisorReviewMode::OnAnomaly,
            max_reviews_per_turn: 2,
            review_window_secs: default_review_window_secs(),
            watchdog_notes: true,
            gate: GateConfig::default(),
            review_settings: AdvisorReviewSettings::default(),
        }
    }
}

fn default_review_window_secs() -> u64 {
    120
}

impl AdvisorMonitorConfig {
    /// 给 driver 构建 runner 时使用的 gate 配置。advisor 未启用时
    /// 返回 `None`——runner 不带 gate，`run_turn_gated` 退化为
    /// `run_turn`（不影响未启用 advisor 的场景）。
    /// `review_settings` 在此注入 gate 副本，随 `advisor_gate` 的
    /// 既有 plumbing 流到各 advisor 审查 engine 的构建点。
    pub fn runner_gate(&self) -> Option<GateConfig> {
        self.enabled.then(|| {
            let mut gate = self.gate.clone();
            gate.review_settings = self.review_settings;
            gate
        })
    }
}

// ─── v3 pause gate（intervene 暂停门，当前休眠）─────────────────────

/// Intervene 暂停门（**当前休眠**）：v4 起 intervene 只注入纠正 hint
/// 让 manager 自愈，monitor 不再调 `request()`，此门不会被置位。
/// 结构与 runner 侧接线保留，以便未来需要时恢复暂停语义（在 monitor
/// 的 intervene 分支重新调 `controller.request_pause()` 即可）。
///
/// 原始语义：monitor 调 `request()` 置位；runner 在 tool-round 边界
/// （drain advisor hint 的同一位置）调 `wait_if_requested()` 挂起，
/// 直到用户拍板（`resolve()`，任何用户输入都算）或超时自动恢复。
/// 主 runner（driver 经 `AgentRunner::with_pause_gate`）与 delegate /
/// workflow 专家 runner（controller.rs / workflow.rs 装配）都带这门；
/// advisor 自身不带。注意门只在各 runner 的边界生效——in-flight 的
/// 模型调用与工具执行不会被打断；阻塞在长 delegate/workflow 调用里
/// 的 manager 在其返回前也到不了边界。
#[derive(Debug, Clone)]
pub struct AdvisorPauseGate {
    requested: Arc<std::sync::atomic::AtomicBool>,
    resume: Arc<tokio::sync::Notify>,
    timeout: Duration,
}

impl Default for AdvisorPauseGate {
    fn default() -> Self {
        Self::new()
    }
}

impl AdvisorPauseGate {
    /// 防死锁上限（秒）：用户迟迟不拍板时自动恢复继续（warn）。
    pub const DEFAULT_TIMEOUT_SECS: u64 = 600;

    pub fn new() -> Self {
        Self::with_timeout(Duration::from_secs(Self::DEFAULT_TIMEOUT_SECS))
    }

    /// 自定义超时（测试用短超时验证自动恢复）。
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            resume: Arc::new(tokio::sync::Notify::new()),
            timeout,
        }
    }

    /// monitor 侧：Intervene 时请求暂停。幂等。
    pub fn request(&self) {
        self.requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// 当前是否有未拍板的暂停请求。
    pub fn is_requested(&self) -> bool {
        self.requested.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 拍板侧：清旗并唤醒挂起的 runner。controller 在任何用户输入
    /// 到达时调用（选择弹窗的回答也作为普通用户消息回传）。
    pub fn resolve(&self) {
        if self
            .requested
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.resume.notify_waiters();
        }
    }

    /// runner 侧（tool-round 边界）：未置位立即返回；置位则挂起
    /// 直到 `resolve()` 或超时（超时自动清旗恢复 + warn，防死锁）。
    pub async fn wait_if_requested(&self) {
        if !self.is_requested() {
            return;
        }
        let wait = async {
            while self.is_requested() {
                // Notify 丢唤醒（resolve 发生在 notified() 注册之前）
                // 由 100ms 复查兜底：最坏多等 100ms，绝不会死锁。
                tokio::select! {
                    _ = self.resume.notified() => {}
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                }
            }
        };
        if tokio::time::timeout(self.timeout, wait).await.is_err()
            && self
                .requested
                .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            tracing::warn!(
                "advisor pause gate: 用户 {}s 未拍板，超时自动恢复",
                self.timeout.as_secs()
            );
        }
    }
}

// ─── Deterministic detectors (D1–D4) ───────────────────────────────

/// Which detector fired. Labels match the design doc table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DetectorKind {
    /// D1: RoleTurn contains more `<tool_call` opens than ToolUse
    /// executions this turn → calls were dropped by the parser.
    DroppedToolCall,
    /// D2: ToolUse.args is not valid JSON.
    InvalidToolArgs,
    /// D3: ≥2 consecutive ToolError events (benign ENOENT probes
    /// exempt — see `is_benign_probe_error`).
    ToolErrorStreak,
    /// D4: same (tool_name, args) call ≥3 times in a row.
    ToolCallLoop,
    /// D5: final assistant response is shorter than the configured
    /// threshold AND is not a deliberate short ack. Catches the
    /// "model just spat out 53 chars of broken tool syntax" case.
    ShortOutput,
    /// D6: final response contains XML tool-call-looking patterns
    /// (`<read>`, `<bash>`, `<list>`, …) but the parser did not
    /// actually execute any tool this turn. Catches the case where
    /// the model emits a "fake" tool call that the framework does
    /// not recognize, so the response is just the tool syntax with
    /// no real answer.
    ToolCallEcho,
    /// D7: a `WorkflowFinished` event arrived with status != "ok".
    /// Catches the case where the manager's workflow died mid-run
    /// and it silently falls back to manual delegates without
    /// disclosing the failure to the user.
    WorkflowFailed,
    /// D8: ≥3 delegates dispatched strictly serially (each started
    /// only after the previous finished). Independent specialist
    /// tasks should be dispatched in one parallel batch.
    SerialDelegates,
    /// D9: watched role ended a turn with zero tool calls after a
    /// substantial user message. Might be a legit direct answer, but
    /// for complex tasks the correct path is explore→ask→plan——the
    /// hint reminds the manager of the flow (advisory, not blocking).
    NoToolTurn,
}

impl DetectorKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::DroppedToolCall => "D1",
            Self::InvalidToolArgs => "D2",
            Self::ToolErrorStreak => "D3",
            Self::ToolCallLoop => "D4",
            Self::ShortOutput => "D5",
            Self::ToolCallEcho => "D6",
            Self::WorkflowFailed => "D7",
            Self::SerialDelegates => "D8",
            Self::NoToolTurn => "D9",
        }
    }
}
/// One detector hit: the deterministic hint (injected immediately)
/// plus a short evidence line for the LLM review's trigger section.
#[derive(Debug, Clone)]
pub struct Finding {
    pub kind: DetectorKind,
    pub hint: String,
    pub evidence: String,
}

/// Per-turn detector state. `Default` = fresh turn. Each detector
/// fires at most once per turn (`fired` dedup set, design §5).
#[derive(Default)]
struct TurnDetectors {
    tool_use_count: usize,
    consec_tool_errors: usize,
    last_call: Option<(String, String)>,
    same_call_streak: usize,
    fired: HashSet<DetectorKind>,
}

impl TurnDetectors {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn fire_once(
        &mut self,
        kind: DetectorKind,
        hint: String,
        evidence: String,
    ) -> Option<Finding> {
        if self.fired.insert(kind) {
            Some(Finding {
                kind,
                hint,
                evidence,
            })
        } else {
            None
        }
    }

    fn observe_tool_use(&mut self, tool_name: &str, args: &str) -> Vec<Finding> {
        self.tool_use_count += 1;
        let mut out = Vec::new();

        // D2: args must be valid JSON. Args truncated by the event
        // sink (marker `...[+NB]`) are skipped — truncation itself
        // breaks JSON validity and would false-positive.
        if !args_look_truncated(args) && serde_json::from_str::<serde_json::Value>(args).is_err() {
            if let Some(f) = self.fire_once(
                DetectorKind::InvalidToolArgs,
                "工具参数不是合法 JSON（不要用 XML 参数块），工具收到的是原始字符串".to_string(),
                format!("工具 {tool_name} 的参数不是合法 JSON：{}", truncate_chars(args, 200)),
            ) {
                out.push(f);
            }
        }

        // D4: same (name, args) 3× in a row.
        let call = (tool_name.to_string(), args.to_string());
        if self.last_call.as_ref() == Some(&call) {
            self.same_call_streak += 1;
        } else {
            self.same_call_streak = 1;
            self.last_call = Some(call);
        }
        if self.same_call_streak >= 3 {
            if let Some(f) = self.fire_once(
                DetectorKind::ToolCallLoop,
                "检测到调用循环：换思路，不要重试相同调用".to_string(),
                format!("同一调用 {tool_name} 已连续出现 {} 次", self.same_call_streak),
            ) {
                out.push(f);
            }
        }
        out
    }

    fn observe_tool_result(&mut self) {
        self.consec_tool_errors = 0;
    }

    fn observe_tool_error(&mut self, tool_name: &str, error: &str) -> Vec<Finding> {
        // 良性探测错误（ENOENT）不计入 streak，也不打断已有的真实
        // 错误 streak——它只是探索成本，不是失败信号。
        if is_benign_probe_error(error) {
            return Vec::new();
        }
        self.consec_tool_errors += 1;
        if self.consec_tool_errors >= 2 {
            if let Some(f) = self.fire_once(
                DetectorKind::ToolErrorStreak,
                "工具连续报错：先停手诊断（读报错、换路径/命令），不要重复同一调用".to_string(),
                format!(
                    "连续 {} 次工具报错（最近：{tool_name}: {}）",
                    self.consec_tool_errors,
                    truncate_chars(error, 200)
                ),
            ) {
                return vec![f];
            }
        }
        Vec::new()
    }

    /// D1 turn-end reconciliation: count `<tool_call` opens in the
    /// turn's RoleTurn text vs ToolUse executions seen this turn.
    ///
    /// Known limitation: only the *final* round's text is broadcast
    /// (RoleTurn carries run_turn's last response), so a dropped call
    /// in an earlier round of a multi-round tool loop is invisible
    /// here. The dominant case — the final answer containing an
    /// unexecuted call — is caught.
    fn finish_turn(&mut self, role_turn_text: &str) -> Option<Finding> {
        let opens = role_turn_text.matches("<tool_call").count();
        if opens > self.tool_use_count {
            self.fire_once(
                DetectorKind::DroppedToolCall,
                "你的工具调用未被解析执行：检查格式 `<tool_call>NAME {json}</tool_call>`，单行、JSON 参数".to_string(),
                format!(
                    "RoleTurn 含 {opens} 个 `<tool_call` 起始标签，本 turn 仅执行 {} 次工具调用",
                    self.tool_use_count
                ),
            )
        } else {
            None
        }
    }
}

/// The event sink truncates payloads with a `...[+{n}B]` suffix
/// (controller.rs `truncate_event_text`); such args can't be
/// JSON-validated.
fn args_look_truncated(args: &str) -> bool {
    args.ends_with("B]") && args.contains("...[+")
}

/// 良性探测错误：探索期的自愈型失败，不等于「agent 失控」，D3 与
/// specialist streak 都不应计数。两类：
/// 1. 路径不存在（ENOENT）：模型按惯例猜文件名（README.md 等）与
///    列目录同批发出，猜错即报，下一轮看到列表后自行纠正；
/// 2. 行号越界（`start_line N exceeds file length M`）：模型拿着
///    过期/估算的行号区间读文件（文件被并行修改后行号漂移），下
///    一轮拿到正确范围后会自愈。
/// 匹配工具层原样透传的错误文案（如
/// `stat: No such file or directory (os error 2)`）。
fn is_benign_probe_error(error: &str) -> bool {
    error.contains("No such file or directory") || error.contains("exceeds file length")
}

/// Char-boundary-safe truncation with the same marker format the
/// controller's event sink uses.
/// 启发式判断一条 delegate 任务是否属于**调研**（只产出理解，不产出
/// 交付物）。
///
/// 存在的原因：manager 绕过 `explore` 流程、改用「通读 XXX 产出解剖
/// 报告」这类 delegate，效果与再跑一轮探索完全相同，但按 workflow 名
/// 分类抓不到它。实测会话正是这样连追了两个调研 delegate。
///
/// 只用于给 advisor 提供「连着几轮只调研」这一事实，不做任何自动阻断
/// ——所以宁可宽松：命中调研动词且没有落地动词才算。
fn delegate_task_looks_like_research(task: &str) -> bool {
    const RESEARCH: [&str; 12] = [
        "通读", "解剖", "调研", "探索", "摸清", "梳理", "弄清", "学习路径",
        "背景说明", "survey", "explore", "investigate",
    ];
    const DELIVERY: [&str; 10] = [
        "实现", "修改", "重构", "修复", "编写", "写入", "落盘", "提交", "任务清单",
        "验收标准",
    ];
    let has_research = RESEARCH.iter().any(|k| task.contains(k));
    let has_delivery = DELIVERY.iter().any(|k| task.contains(k));
    has_research && !has_delivery
}

/// 从用户诉求里识别点名的交付物。单一实现在
/// [`crate::workflow::named_deliverables`]——工具侧提醒与本处的缺口
/// 判定必须用同一张表，否则两边会对"用户到底要什么"给出不同答案。
fn named_deliverables(user_text: &str) -> Vec<&'static str> {
    crate::workflow::named_deliverables(user_text)
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    format!("{}...[+{}B]", &text[..end], text.len() - end)
}

// ─── Pre-persistence gate (D5 / D6) ───────────────────────────────
// 用途：在主对话的 RoleTurn 落盘/广播给 UI **之前**，对 LLM 最终
// 产出做一次零成本确定性检查。命中 → 让 runner 注入 hint 并重跑
// 同一 turn（最多 2 次），避免坏结论进入 ui-sessions/<id>.jsonl
// 和前端气泡。设计动机：之前 53 chars 的"伪工具调用"输出
// （`<read><path>...</path></read>`）被 manager 当成 16K chars 的
// 详尽答复接受，整条 pipeline 在第一个 delegate 就坏掉。
//
// 与 D1-D4 的关系：D1-D4 在 controller 的 event broadcast 之后由
// AdvisorMonitor 异步观察（侧通道、注入 hint 给下个 turn），
// 不能阻止当前 turn 的 RoleTurn 落盘；本 gate 是**同步、前置、阻断**
// 的检查，专门覆盖"LLM 自己把坏答案当成最终回复"这种情况。

/// 单次 turn 的 gate 阈值。`AgentRunner` 在调用
/// `check_response_gates` 时传进来；控制每条 RoleTurn 的最低
/// 通过线。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GateConfig {
    /// D5: response.len() < 该值则视为短输出。默认 50。
    /// 选 50 的理由：53 chars 的 `<read><path>...</path></read>`
    /// 刚好被覆盖；正常 "好的/收到/已确认" 类短 ack 也在 50 以内
    /// —— 因此配合 D5 的 "非明确短 ack" 启发式（见实现）。
    pub short_output_threshold: usize,
    /// D6: response 里出现以下任意 XML 模式且本 turn 实际 tool
    /// 执行数为 0，视为"工具回声"（模型以为调用了工具但框架没
    /// 识别）。默认覆盖项目里所有常用工具名。
    pub tool_call_echo_patterns: Vec<String>,
    /// Gate 命中最多重试次数。超过则强制 pass + 落盘 +
    /// trace 上标记 GateForcePass，避免无限循环。
    pub max_retries: usize,
    /// delegate-return 审查参数（超时 + 重做上限）。用户面配置在
    /// `AdvisorMonitorConfig::review_settings`，`runner_gate()` 注入
    /// 到这里随既有的 `advisor_gate` plumbing 下发到各 engine 构建
    /// 点——不为它新拉一条参数链。
    #[serde(default)]
    pub review_settings: AdvisorReviewSettings,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            short_output_threshold: 50,
            tool_call_echo_patterns: vec![
                "<read>".to_string(),
                "<bash>".to_string(),
                "<list>".to_string(),
                "<search>".to_string(),
                "<write>".to_string(),
                "<delegate".to_string(),
            ],
            max_retries: 2,
            review_settings: AdvisorReviewSettings::default(),
        }
    }
}

/// Gate 决策。`Pass` = 落盘放行；`Fail` = 阻断 + 给 runner 一条
/// hint 让它重跑 turn。`Fail` 携带 detector 类型方便 trace 记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateVerdict {
    Pass,
    Fail {
        detector: DetectorKind,
        /// 一行中文 hint，runner 把它当作用户消息追加到 context，
        /// 下次 LLM 调用直接看到。
        hint: String,
        /// 给 trace 看的证据摘要（不含 response 全文，避免污染）。
        evidence: String,
    },
}

/// XML tool-call 标签的回声模式集合（无 `<tool_call>` 的合法
/// 协议形式）。只匹配"opening tag"足够：闭合标签 `</read>` 通常
/// 紧跟 opening 出现，但 framing 在长 response 里也可能不闭合
/// （模型截断），所以 opening 已经够用。
fn looks_like_tool_echo(response: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| response.contains(p.as_str()))
}

/// 短 ack 识别。极短且只含确认/拒绝语义的 response 视为合法
/// 短输出，不触发 D5。启发式：长度 ≤ 12 且不含中英文字母以外
/// 的内容（标点/emoji 都不算），或者典型 ack 词开头。
fn looks_like_short_ack(response: &str) -> bool {
    let trimmed = response.trim();
    if trimmed.is_empty() {
        return true; // 空串也放过（极端边界）
    }
    if trimmed.chars().count() > 12 {
        return false;
    }
    // 全是空白/标点 → 当 ack
    if trimmed
        .chars()
        .all(|c| c.is_whitespace() || c.is_ascii_punctuation())
    {
        return true;
    }
    // 典型 ack 开头（中英常见）
    const ACK_PREFIXES: &[&str] = &[
        "ok", "OK", "好的", "收到", "好", "是", "否", "yes", "no", "ack",
        "ack ", "ACK", "done", "Done", "完成", "确认", "明白", "了解",
    ];
    ACK_PREFIXES.iter().any(|p| trimmed.starts_with(p))
}

/// 结构化裁决识别：`VERDICT: ACCEPT` 这类**契约明确要求的**极简产出是
/// 合法的，不该被 D5 当成「没真正回答」。
///
/// 背景（真实互锁）：`task_refine` 的 gate 步契约是「严格只输出三种裁决
/// 之一」+ `output_contract.require = ["VERDICT:"]`——模型照做且写得简洁
/// 时，产出可能短到 15 字符。D5 若判死它，会重试 3 次后
/// 「advisor terminated」判死整条 workflow，于是 `submit` 步执行不到、
/// `plan` 不被调用、拆分弹窗消失——正是我们要修的那个症状。
///
/// 用 `starts_with` 而非 `contains`：只承认「以裁决标记开头」的产出，
/// 避免给「东拉西扯里恰好提到 VERDICT」开后门。
fn looks_like_structured_verdict(response: &str) -> bool {
    let trimmed = response.trim();
    const MARKERS: &[&str] = &["VERDICT:", "VERDICT :", "裁决：", "裁决:"];
    MARKERS.iter().any(|m| trimmed.starts_with(m))
}

/// Pre-persistence gate 的入口。`AgentRunner` 在 LLM/tool 循环
/// 跑完、得到 final_response 但还没 return 给 controller 之前
/// 调用本函数。`tool_use_count_this_turn` 由 runner 在循环里
/// 累加（参见 agent.rs `last_turn_tool_count` 字段）。
///
/// 行为：
/// - 任一 detector 命中 → `Fail`，runner 据此注入 hint + 重跑
/// - 全通过 → `Pass`，response 正常落盘
pub fn check_response_gates(
    response: &str,
    tool_use_count_this_turn: usize,
    config: &GateConfig,
) -> GateVerdict {
    // D5: 短输出（且非明确 ack、非契约要求的结构化裁决）
    //
    // 长度按**字节**计（`str::len()`），这是有意的：UTF-8 字节数是跨语言
    // 的「信息量」粗略代理——阈值 50 按英文校准（chars≈bytes），而 50 字节
    // ≈17 个汉字，同样是「说了句实在话」的量级。若改成按字符计，中文就需要
    // 凑满 50 个汉字才算合格，等于把门槛收紧 3 倍、大量误杀正常中文答复
    // （实测：改成字符计会让 9 个既有 advisor 用例从放行翻转为判死）。
    // 提示文案因此说「字节」而不是「字符」。
    let len_bytes = response.len();
    if len_bytes < config.short_output_threshold
        && !looks_like_short_ack(response)
        && !looks_like_structured_verdict(response)
    {
        return GateVerdict::Fail {
            detector: DetectorKind::ShortOutput,
            hint: format!(
                "[advisor gate D5] 你的上一轮输出只有 {} 字节（阈值 {}），看起来像没真正回答。\n\
                 - 如果你已经调用了工具，请把工具结果**用文字**复述给用户。\n\
                 - 如果没调用工具，请**用文字**直接回答问题，不要只输出工具调用语法。\n\
                 - 调用工具的格式必须是 `<tool_call>NAME {{\"key\": \"value\"}}</tool_call>`（单行、JSON 参数），不是 `<read>...</read>` 这种 XML。",
                len_bytes,
                config.short_output_threshold
            ),
            evidence: format!(
                "D5 ShortOutput: response.len()={} bytes < threshold={}",
                len_bytes,
                config.short_output_threshold
            ),
        };
    }
    // D6: 工具回声（XML 工具标签 + 实际 tool_use_count=0）
    if tool_use_count_this_turn == 0
        && looks_like_tool_echo(response, &config.tool_call_echo_patterns)
    {
        let matched: Vec<&str> = config
            .tool_call_echo_patterns
            .iter()
            .filter(|p| response.contains(p.as_str()))
            .map(|p| p.as_str())
            .collect();
        return GateVerdict::Fail {
            detector: DetectorKind::ToolCallEcho,
            hint: format!(
                "[advisor gate D6] 你的输出包含工具调用标签 {}，但本轮实际没有工具被执行（tool_use_count=0）。\n\
                 这说明你用了**非协议**的 XML 形式（`<read>...</read>` 等），框架无法识别。\n\
                 请改用：\n\
                 - 如果是 read：`<tool_call>read {{\"path\": \"...\"}}</tool_call>`\n\
                 - 如果是 bash：`<tool_call>bash {{\"command\": \"...\"}}</tool_call>`\n\
                 - 如果已经拿到了工具结果但忘了复述：请把结果**用文字**写出来",
                matched.join("/")
            ),
            evidence: format!(
                "D6 ToolCallEcho: matched patterns={:?}, tool_use_count=0",
                matched
            ),
        };
    }
    GateVerdict::Pass
}

// ─── Dispatch digest ───────────────────────────────────────────────

/// 路由审查用的**紧凑分派摘要**，取代原来的原始事件流 transcript。
///
/// ## 为什么换掉原始 transcript
///
/// 旧实现是一个 24,000 字符的环形缓冲，逐条塞进 manager 的
/// `[assistant]` 全文（6K/条）、`[tool_use]` 原始参数、`[tool_result]`
/// 原始结果（1.5K/条）、子会话回复（2K/条）。三个问题叠在一起：
///
/// 1. **静默丢弃**：超预算就丢最老的行，`render()` 不留任何痕迹。advisor
///    拿到一份看起来完整、实则掐了头的记录。
/// 2. **谎报范围**：prompt 标题写「manager 本 turn transcript」，但
///    `reset_turn_state` 从不碰它、`MonitorState` 整会话只建一次，实际是
///    跨会话累积。
/// 3. 于是 advisor 被骗两次（说一轮实为多轮、说完整实为掐头），据此做
///    「过程取证」必然出错——实测会话里它断言
///    「记录中无成功读取 README，故引用为幻觉」，而那次读取真实发生过，
///    只是在被丢掉的那一段里。
///
/// ## 现在的定位
///
/// 路径分工明确后，本摘要**只服务路由审查**（manager 选了哪个 workflow、
/// 派给谁、拆分合理否），不再承担「审查产出内容」——那是
/// `build_delegate_review_prompt`（委派返回审查）的职责，它有完整结果和
/// 正确的截断标注。
///
/// 因此这里只记**决策与其结果状态**，不记正文：条目短（每条 ≤
/// [`DIGEST_LINE_CHARS`]）、条数有限（≤ [`DIGEST_MAX_LINES`]），超限时
/// `render()` **显式标注**省略了多少条。每轮开头清空，所以「本 turn」
/// 这个说法是真的。
#[derive(Default)]
struct DispatchDigest {
    lines: Vec<String>,
    /// 因超出条数上限而丢弃的条数，`render()` 会显式报出来。
    dropped: usize,
    /// 「下次 push 时先清空」。不能在 `reset_turn_state` 里直接清——
    /// EveryTurn 模式的审查发生在 observe() 返回**之后**，那时 turn 已
    /// 结束、reset 已跑过，直接清会让该次审查拿到空摘要。改为惰性清空：
    /// 本轮的摘要留到审查读完，下一轮第一条事件到来时才重置。
    pending_reset: bool,
}

/// 单条摘要的字符上限。只记决策不记正文，240 足够放下角色名 + 任务首句。
const DIGEST_LINE_CHARS: usize = 240;
/// 摘要条数上限。一轮里 40 条分派动作已经远超正常规模（超了本身就是
/// 过度编排的证据，而 `dropped` 计数会把这个信号显式交给 advisor）。
const DIGEST_MAX_LINES: usize = 40;

impl DispatchDigest {
    fn push(&mut self, line: String) {
        if self.pending_reset {
            self.lines.clear();
            self.dropped = 0;
            self.pending_reset = false;
        }
        self.lines.push(truncate_chars(&line, DIGEST_LINE_CHARS));
        while self.lines.len() > DIGEST_MAX_LINES {
            self.lines.remove(0);
            self.dropped += 1;
        }
    }

    /// 标记「下一条事件到来时清空」。见 `pending_reset` 的说明。
    fn mark_reset(&mut self) {
        self.pending_reset = true;
    }

    fn render(&self) -> String {
        if self.lines.is_empty() {
            // 空摘要必须自己说话。留空的话 advisor 拿到的是一个没有内容
            // 的章节标题，它会去猜这个空白是什么意思——实测它猜出了三种
            // 互相矛盾的解释（"manager 没分派" / "数据缺失" / "分派太简单
            // 没被记录"），然后基于其中一种下判。
            debug_assert_eq!(self.dropped, 0, "无行时不可能有丢弃计数");
            return "（本 turn 没有记录到任何分派动作：既没有 workflow 启动，\
                    也没有 delegate / plan 调用。注意这是「摘要为空」这一事实本身，\
                    不要据此推断 manager 正文里说了什么或没说什么。）"
                .to_string();
        }
        let body = self
            .lines
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if self.dropped == 0 {
            return body;
        }
        // 省略必须可见：advisor 据此知道「没看到」不等于「没发生过」。
        format!(
            "[注意：本轮分派动作过多，最早的 {} 条已省略——这本身就是过度编排的信号；\
             不要因为某条动作不在下面就断言它没发生]\n{body}",
            self.dropped
        )
    }
}

// ─── Review verdict parsing ────────────────────────────────────────

/// Review verdict 第四态 `Terminate`：LLM 复审或 deterministic
/// gate（D5/D6 重试耗尽）认为「问题严重到需要停下让用户接管」。
/// controller 接住后发 `ChatEvent::AdvisorTerminated` 而不是
/// `RoleTurn`，坏答案不落盘；用户可继续输入（不是硬终止：
/// runner 不自杀，driver 等用户新消息）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    Warn,
    Intervene,
    Terminate,
}

/// advisor 判 intervene/terminate 后的处置建议：**优先续作**（同一
/// subsession 带审查反馈再跑一轮，保留已完成的取证与上下文，成本约
/// 一两次模型调用）；仅当产出基于虚构事实、方向根本错误或上下文已
/// 被污染到「续作不如重来」时才建议 restart（废弃本次分派，全新
/// runner 重做——整轮工具调用与探索全部作废重烧）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Remedy {
    /// 默认：续作修补。advisor 未写 remedy 行时按此处理。
    #[default]
    Patch,
    Restart,
}

/// Parsed advisor review output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewVerdict {
    pub verdict: Verdict,
    pub reason: String,
    pub hint: String,
    /// intervene/terminate 时的处置建议；ok/warn 时无意义（恒 Patch）。
    pub remedy: Remedy,
}

fn verdict_word(s: &str) -> Option<Verdict> {
    let w = s
        .trim()
        .trim_matches(|c: char| c == '*' || c == '`')
        .trim_end_matches(['.', ';', '。', '；'])
        .trim()
        .to_ascii_lowercase();
    match w.as_str() {
        "ok" => Some(Verdict::Ok),
        "warn" | "warning" => Some(Verdict::Warn),
        "intervene" => Some(Verdict::Intervene),
        "terminate" | "stop" | "halt" | "kill" => Some(Verdict::Terminate),
        _ => None,
    }
}

/// Strip `<think>…</think>` blocks before verdict parsing. Reasoning
/// models put draft `verdict:`/`reason:` lines inside the think block;
/// scanning them would capture the draft plus mid-course corrections
/// ("Actually let me think…") into the user-visible reason.
fn strip_think_blocks(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    loop {
        let Some(start) = rest.find("<think>") else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..start]);
        rest = match rest[start + "<think>".len()..].find("</think>") {
            Some(end) => &rest[start + "<think>".len() + end + "</think>".len()..],
            // 未闭合的 think 块：视为一直延伸到末尾（截断的流式输出）。
            None => return out,
        };
    }
}

/// Parse the advisor's review output. Expected shape:
///
///
/// ```text
/// verdict: ok | warn | intervene
/// reason: <user-visible justification>
/// hint: <one-line correction for the manager>
/// remedy: patch | restart   (可选，仅 intervene/terminate 时有意义)
/// ```
///
/// Tolerates a bare verdict word on its own line and multi-line
/// reason/hint sections. Malformed output degrades to `Ok`
/// (no action — an unreadable review must not inject noise).
/// 缺省 remedy = patch（续作优先），见 [`Remedy`]。
pub fn parse_verdict(raw: &str) -> ReviewVerdict {
    let cleaned = strip_think_blocks(raw);
    let raw = cleaned.as_str();
    let mut verdict: Option<Verdict> = None;
    let mut remedy = Remedy::default();
    let mut reason_lines: Vec<&str> = Vec::new();
    let mut hint_lines: Vec<&str> = Vec::new();
    // 0 = no open section, 1 = reason, 2 = hint.
    let mut section = 0u8;

    for line in raw.lines() {
        let t = line.trim();
        let lower = t.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("verdict:") {
            verdict = verdict_word(rest).or(verdict);
            section = 0;
            continue;
        }
        if let Some(rest) = lower.strip_prefix("remedy:") {
            let w = rest
                .trim()
                .trim_matches(|c: char| c == '*' || c == '`')
                .trim_end_matches(['.', ';', '。', '；'])
                .trim()
                .to_ascii_lowercase();
            remedy = match w.as_str() {
                "restart" | "redo" | "重做" => Remedy::Restart,
                _ => Remedy::Patch,
            };
            section = 0;
            continue;
        }
        if lower.starts_with("reason:") {
            section = 1;
            let rest = t["reason:".len()..].trim();
            if !rest.is_empty() {
                reason_lines.push(rest);
            }
            continue;
        }
        if lower.starts_with("hint:") {
            section = 2;
            let rest = t["hint:".len()..].trim();
            if !rest.is_empty() {
                hint_lines.push(rest);
            }
            continue;
        }
        if verdict.is_none() {
            if let Some(v) = verdict_word(t) {
                verdict = Some(v);
                section = 0;
                continue;
            }
        }
        match section {
            1 if !t.is_empty() => reason_lines.push(t),
            2 if !t.is_empty() => hint_lines.push(t),
            _ => {}
        }
    }

    ReviewVerdict {
        // Malformed output → Ok (silent no-op), per fault discipline.
        verdict: verdict.unwrap_or(Verdict::Ok),
        reason: reason_lines.join("\n").trim().to_string(),
        hint: hint_lines.join("\n").trim().to_string(),
        remedy,
    }
}

// ─── LLM review engine ─────────────────────────────────────────────

/// Builds and runs the single-turn advisor review call.
///
/// Uses `Agent::chat` with `WaitPolicy::NoWait` rather than
/// `AgentRunner::run_turn`: `run_turn` hardcodes `WaitAndRetry`,
/// which would *sleep out* a 30–60s model cooldown inside the
/// monitor loop on a failing advisor model. `NoWait` walks the
/// fallback chain and degrades immediately — the monitor keeps
/// watching. (Deviation from design §3, documented there.)
pub struct AdvisorReviewEngine {
    agent_config: Arc<AgentConfig>,
    resolver: Arc<ModelResolver>,
    default_params: GenerateParams,
    /// Session workspace root (`ControllerConfig.cwd`) for the
    /// project-level 监察笔记. `None` = project layer skipped.
    session_cwd: Option<PathBuf>,
    /// Master switch for reading 监察笔记 (config `watchdog_notes`).
    watchdog_notes: bool,
    /// 可选：把每次 review 的 LLM 调用 / 返回 / verdict 落盘到
    /// `<ui-sessions>/<sid>/advisor-<micros>.jsonl`。`None` = 不落盘
    /// （测试 / 未启用 persistence 的 caller）。由 UI server 在 spawn
    /// advisor monitor 前调 [`with_subsession_sink`] 注入。
    subsession_sink: Option<Arc<dyn crate::trace::TraceSink>>,
    /// delegate-return 审查与重做参数（超时 + 重做上限），来自
    /// `AdvisorMonitorConfig::review_settings`；未注入时用默认值。
    review_settings: AdvisorReviewSettings,
}

impl AdvisorReviewEngine {
    pub fn new(
        agent_config: Arc<AgentConfig>,
        resolver: Arc<ModelResolver>,
        default_params: GenerateParams,
    ) -> Self {
        Self {
            agent_config,
            resolver,
            default_params,
            session_cwd: None,
            watchdog_notes: true,
            subsession_sink: None,
            review_settings: AdvisorReviewSettings::default(),
        }
    }

    /// 注入 delegate-return 审查参数（`AdvisorMonitorConfig::review_settings`）。
    pub fn with_review_settings(mut self, settings: AdvisorReviewSettings) -> Self {
        self.review_settings = settings;
        self
    }

    /// delegate-return 单次审查的超时预算。
    pub fn delegate_review_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.review_settings.delegate_review_timeout_secs)
    }

    /// 返回被判 intervene/terminate 时的重做上限（钳到硬上限内）。
    pub fn return_max_redo(&self) -> u8 {
        self.review_settings.return_max_redo.min(MAX_RETURN_REDO)
    }

    /// Point the engine at the session workspace for project-level
    /// 监察笔记 (`<cwd>/.latte/advisor-watchdog.md`) and set the
    /// master switch (from `AdvisorMonitorConfig::watchdog_notes`).
    pub fn with_watchdog_notes(mut self, session_cwd: PathBuf, enabled: bool) -> Self {
        self.session_cwd = Some(session_cwd);
        self.watchdog_notes = enabled;
        self
    }

    /// 注入 subsession sink（来自 UI server 的 `SubsessionStore::create`）。
    /// 启用后每次 `review()` 会把 LLM 调用 / 原始返回 / verdict 落盘。
    pub fn with_subsession_sink(mut self, sink: Arc<dyn crate::trace::TraceSink>) -> Self {
        self.subsession_sink = Some(sink);
        self
    }

    /// Run one review. `Err` means the advisor could not be reached
    /// or built — callers degrade silently (tracing::warn).
    pub async fn review(
        &self,
        user_question: &str,
        digest: &str,
        trigger: &str,
    ) -> AgentResult<ReviewVerdict> {
        let template = self
            .agent_config
            .roles
            .get("advisor")
            .cloned()
            .or_else(|| crate::prompts::template_for("advisor"))
            .ok_or_else(|| AgentError::RoleNotFound("advisor".to_string()))?;
        let role = template.resolve(&self.default_params).await?;
        let models =
            self.resolver
                .resolve_chain("advisor", ModelTier::Premium, &role.model_chain)?;
        let agent = Agent::new_with_chain(
            "advisor".to_string(),
            role,
            models,
            self.default_params.clone(),
        )?;

        // 监察笔记 read fresh on every review — the file may be
        // written after the session started. Never cached.
        let notes = if self.watchdog_notes {
            read_watchdog_notes(self.session_cwd.as_deref())
        } else {
            String::new()
        };
        let sys = agent.system_message(&serde_json::json!({}))?;
        let user = Message::user(build_review_prompt(user_question, digest, trigger, &notes));
        let model_id = agent
            .model_chain
            .first()
            .map(|m| m.model.id.clone())
            .unwrap_or_default();
        // ── 落盘：调 LLM 后 emit ModelCall（真实时延 + finish_reason。
        // 此前在调用前记 latency=0/finish=''，审查耗时与成败全黑盒）──
        let started = std::time::Instant::now();
        let completion = agent
            .chat(&[sys, user], None, WaitPolicy::NoWait)
            .await;
        let latency_ms = started.elapsed().as_millis() as u64;
        let completion = match completion {
            Ok(c) => c,
            Err(e) => {
                if let Some(sink) = self.subsession_sink.as_ref() {
                    let meta = crate::trace::TraceMeta::now(0, "advisor", "");
                    sink.emit(crate::trace::TraceEvent::ModelCall {
                        meta,
                        model_id: model_id.clone(),
                        params_json: serde_json::to_string(&self.default_params)
                            .unwrap_or_default(),
                        latency_ms,
                        finish_reason: format!("error: {e}"),
                    });
                }
                return Err(e);
            }
        };
        if let Some(sink) = self.subsession_sink.as_ref() {
            let meta = crate::trace::TraceMeta::now(0, "advisor", "");
            sink.emit(crate::trace::TraceEvent::ModelCall {
                meta: meta.clone(),
                model_id: model_id.clone(),
                params_json: serde_json::to_string(&self.default_params).unwrap_or_default(),
                latency_ms,
                finish_reason: completion.stop_reason.clone(),
            });
            // ── 落盘：emit ModelRawOut（含 verdict 原始输出） ──
            sink.emit(crate::trace::TraceEvent::ModelRawOut {
                meta,
                raw_content: completion.content.clone(),
            });
        }
        Ok(parse_verdict(&completion.content))
    }

    /// Review a specialist's delegate return **before** it flows back
    /// into the manager's context. Judges two things:
    ///   1. role adherence — did the specialist stay within its role's
    ///      responsibilities?
    ///   2. result relevance — does the return actually answer the
    ///      delegated task / produce the expected result?
    ///
    /// Reuses the `advisor` role + `parse_verdict`. `Err` means the
    /// advisor could not be reached/built — callers degrade silently
    /// (return the specialist output unchanged).
    pub async fn review_delegate(
        &self,
        main_topic: &str,
        role_id: &str,
        role_responsibilities: &str,
        task: &str,
        response: &str,
        tool_call_summary: &str,
    ) -> AgentResult<ReviewVerdict> {
        let template = self
            .agent_config
            .roles
            .get("advisor")
            .cloned()
            .or_else(|| crate::prompts::template_for("advisor"))
            .ok_or_else(|| AgentError::RoleNotFound("advisor".to_string()))?;
        let role = template.resolve(&self.default_params).await?;
        let models =
            self.resolver
                .resolve_chain("advisor", ModelTier::Premium, &role.model_chain)?;
        let agent = Agent::new_with_chain(
            "advisor".to_string(),
            role,
            models,
            self.default_params.clone(),
        )?;
        let sys = agent.system_message(&serde_json::json!({}))?;
        let user = Message::user(build_delegate_review_prompt(
            main_topic,
            role_id,
            role_responsibilities,
            task,
            response,
            tool_call_summary,
        ));
        let model_id = agent
            .model_chain
            .first()
            .map(|m| m.model.id.clone())
            .unwrap_or_default();
        // 同 review()：真实时延 + finish_reason 在调用完成后记录。
        let started = std::time::Instant::now();
        let completion = agent.chat(&[sys, user], None, WaitPolicy::NoWait).await;
        let latency_ms = started.elapsed().as_millis() as u64;
        let completion = match completion {
            Ok(c) => c,
            Err(e) => {
                if let Some(sink) = self.subsession_sink.as_ref() {
                    let meta = crate::trace::TraceMeta::now(0, "advisor", "");
                    sink.emit(crate::trace::TraceEvent::ModelCall {
                        meta,
                        model_id,
                        params_json: serde_json::to_string(&self.default_params)
                            .unwrap_or_default(),
                        latency_ms,
                        finish_reason: format!("error: {e}"),
                    });
                }
                return Err(e);
            }
        };
        if let Some(sink) = self.subsession_sink.as_ref() {
            let meta = crate::trace::TraceMeta::now(0, "advisor", "");
            sink.emit(crate::trace::TraceEvent::ModelCall {
                meta: meta.clone(),
                model_id,
                params_json: serde_json::to_string(&self.default_params).unwrap_or_default(),
                latency_ms,
                finish_reason: completion.stop_reason.clone(),
            });
            sink.emit(crate::trace::TraceEvent::ModelRawOut {
                meta,
                raw_content: completion.content.clone(),
            });
        }
        Ok(parse_verdict(&completion.content))
    }
}

// ─── 项目监察笔记 (advisor-watchdog.md) ────────────────────────────

/// Candidate 监察笔记 paths in priority order (project first,
/// global second). Both optional — missing files are skipped.
fn watchdog_note_paths(session_cwd: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(2);
    if let Some(cwd) = session_cwd {
        out.push(cwd.join(".latte").join("advisor-watchdog.md"));
    }
    if let Some(global) = crate::global_config::GlobalConfig::global_dir() {
        out.push(global.join("advisor-watchdog.md"));
    }
    out
}

/// Read and concatenate 监察笔记 (project-level first, then global).
/// Missing or unreadable files are silently skipped; returns an
/// empty string when nothing was found.
fn read_watchdog_notes(session_cwd: Option<&Path>) -> String {
    let mut out = String::new();
    for path in watchdog_note_paths(session_cwd) {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let content = content.trim();
        if content.is_empty() {
            continue;
        }
        out.push_str(&format!("--- {} ---\n{}\n\n", path.display(), content));
    }
    out.trim_end().to_string()
}

/// Review prompt: 审查指令 + （可选）监察笔记 + 触发原因 + 用户问题 +
/// transcript + 输出格式。The advisor's own role prompt
/// (`prompts::ADVISOR`) is the system message; this is the user
/// message. `notes` is the raw concatenation of 监察笔记 files
/// (empty when none found or the feature is off).
fn build_review_prompt(
    user_question: &str,
    digest: &str,
    trigger: &str,
    notes: &str,
) -> String {
    let attention_block = if notes.is_empty() {
        String::new()
    } else {
        format!(
            "\n除通用检查项外，特别关注以下本项目监察笔记：\n\n<attention>\n{notes}\n</attention>\n"
        )
    };
    format!(
        r#"# 监察审查任务

你正以**监察者**身份旁路审查 manager（主会话 agent）的**任务分派决策**。下面给出用户当前问题，以及 manager 本 turn 做过的分派动作摘要。

**本次审查只判分派路由，不做产出内容的取证。** 摘要里只有「派了什么、结果是什么状态」，**没有** manager 的回复正文、也没有工具的原始参数与返回。所以：

- 不要评判 manager 回复的措辞或结论细节——那由「委派返回审查」在专家结果返回时单独把关，那里有完整产出。
- **不要因为某件事没出现在摘要里就断言它没发生。** 摘要按设计只收分派动作；「没看到证据」≠「有证据表明没做」。缺证据时最多给 `warn`，不要 `intervene`。

逐项检查：
1. **分派路由是否合理**（manager 的核心职责，也是本次审查的重点）：
   - **简单任务**（一两步就能答/改的）应当**直接完成**，不该动辄开 workflow 或委派角色（过度编排 → 建议 intervene 让它直接做）。
   - **复杂任务**应当**优先尝试匹配已有 workflow**；若选了 workflow，核查所选 workflow 与任务是否对得上（选错/硬套 → 建议纠正）。
   - **确无合适 workflow** 时，manager 自行判断**委派角色**：核查委派的角色是否对口、任务拆分是否合理（把实现派给 reviewer、把审查派给 programmer 这类错配 → 建议纠正）。
2. **思路跑偏**：分派方向是否偏离用户问题、在错误方向上持续投入。
3. **异常循环**：同一分派模式反复重试（同一角色、同一任务反复派发且都失败）。
4. **交付物差距**（与路由同等重要，别只看"角色选得对不对"）：
   - 先读用户问题里**点名的交付物**：任务清单 / 看板 / 计划 / 教程 / 文档 / 某个具体改动。
   - 再看本次分派是在**产出**它，还是又一轮**调研**（探索流程、"通读/解剖/摸清/产出背景说明"类委派）。
   - **调研跑了几轮本身不是问题**——大仓库分模块深入是正当的，不要因为"探了好几次"就报警。
     问题是「点名的交付物一次都没往产出走」。
   - 摘要末尾若出现 `[本 session 累计（跨 turn）]` 块，说明系统已确认：用户点名了交付物、
     调研在跑、而块里「仍然欠着的」那几样**到现在为止没有任何对应的分派动作**。
     **这种情况给 `intervene`**，hint 只点名**欠着的那几样**该跑哪个流程
     （要任务清单 → `implementation_plan` 拆完用 `plan` 提交；要教程 → `learn` / `learn_loop`）。
   - **一次诉求点名多样东西时，交清一样不等于交清全部**。块里如果同时有「已经往产出走的」
     和「仍然欠着的」，别因为前者就判 ok——那正是最容易漏的形态：manager 跑了第一个流程、
     在正文里承诺了第二个，然后就没有了。hint 里**不要**再催已经在跑的那一样。
   - 判断标准是「离用户点名的交付物还有多远」，不是「这轮调研本身做得好不好」。这条**不受**
     「证据缺失 ≠ 证据为负」限制：累计块是系统统计的确定性事实，不是你的推测。
   - 反例（不要误报）：用户只是问「这块代码怎么回事」，调研本身就是交付物 → `ok`。
     这种情况下累计块也不会出现（诉求里没有交付物名词）。点名的东西全部产出后累计块同样消失。
{attention_block}
# 触发原因

{trigger}

# 用户当前问题（主会话主题）

{user_question}

# manager 本 turn 的分派动作摘要

{digest}

# 输出格式（严格遵守，键名小写英文，不要输出其他键）

verdict: ok | warn | intervene
reason: <1-3 句给用户看的裁决理由；verdict 为 ok 时留空>
hint: <给 manager 的一句话纠正提示，具体可执行；仅 intervene 时必填，其余留空>

判定标准：
- 处理过程健康（耗时久、风格问题不算异常）→ ok
- 值得关注但不阻塞的问题 → warn：只产生给用户看的气泡，**不会**打扰 manager
- 继续下去明显浪费或产出错误结果（幻觉实锤、思路跑偏、关键约束遗漏）→ intervene：会给 manager 注入纠正提示"#
    )
}

/// Review prompt for a specialist's delegate return, checked before it
/// flows back to the manager. The advisor's own role prompt is the
/// system message; this is the user message. Focuses on two axes:
/// role-responsibility adherence and task-result relevance.
fn build_delegate_review_prompt(
    main_topic: &str,
    role_id: &str,
    role_responsibilities: &str,
    task: &str,
    response: &str,
    tool_call_summary: &str,
) -> String {
    // Bound the injected sections so a huge system prompt / response
    // doesn't blow the review context. The response is the core
    // evidence of this review: 6K chars truncates typical specialist
    // reports (the observed case: 25.5K chars) to their first quarter
    // and the reviewer ends up judging deliverables it can't see, so
    // it gets a larger dedicated budget (~8K tokens); when even that
    // is exceeded, the truncation note tells the reviewer to judge
    // only what is visible.
    let duties = truncate_chars(role_responsibilities, 2_000);
    let main_topic_s = if main_topic.trim().is_empty() {
        "（未记录到原始用户诉求——此时不要因「偏离主诉求」下判，仅就任务本身裁决）".to_string()
    } else {
        truncate_chars(main_topic, 1_000)
    };
    let task_s = truncate_chars(task, 1_500);
    let resp_s = truncate_chars(response, DELEGATE_REVIEW_RESPONSE_MAX_CHARS);
    let truncation_note = if resp_s.len() < response.len() {
        format!(
            "\n（注意：结果共 {} 字符，超出审查预算被截断，上面只看到前 {} 字符。请仅依据可见内容裁决，不要臆测被截断部分。）",
            response.len(),
            resp_s.len()
        )
    } else {
        String::new()
    };
    // 工具执行证据：任务要求"用工具完成 X"时，最终文本常常不带
    // 工具痕迹（实测实锤：interview step 用 ask 弹窗收齐 4 个
    // 答案后输出 user_profile，advisor 看不到 ask 执行记录，误判
    // "伪造答案"→ intervene → 带反馈重做 → 用户被重复提问）。
    // 把本轮实际工具执行摘要摆进审查上下文，让裁决基于事实而非
    // 文本猜测。空摘要（调用方没有收集到）不渲染该段。
    let tool_section = if tool_call_summary.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\n# 它本轮实际执行的工具调用（引擎侧记录，供核实任务是否真正执行）\n\n{tool_call_summary}\n"
        )
    };
    format!(
        r#"# 委派返回审查任务

专家角色「{role_id}」刚完成一次被委派的子任务，其结果**即将返回给 manager 合并进主会话**。请在返回前做一次把关。

# 用户在主会话里的原始诉求（最终要满足的是这个）

{main_topic_s}

判断「是否符合预期」时以它为准绳：子任务做得再漂亮，若偏离主诉求也要指出；反之，只要它在主诉求这条线上交付了被派的那部分，就不要因为它没顺手做别的事而挑刺。

# 该角色的职责范围（其系统提示，可能截断）

<duties>
{duties}
</duties>

# 委派给它的任务

{task_s}

# 它返回的结果

<response>
{resp_s}
</response>
{truncation_note}
{tool_section}
逐项检查：
1. **偏离职责 / 越权**：是否做了超出「{role_id}」职责范围的事，或没有以该角色应有的专业方式完成（比如让 reviewer 去写实现、让 programmer 只空谈不给代码）。
2. **答非所问 / 未达结果**：返回是否真正回答了委派任务、产出了任务预期的结果——有无跑题、空泛套话、遗漏关键要求，或声称完成但实际没做（幻觉式交付）。

# 输出格式（严格遵守，键名小写英文，不要输出其他键）

verdict: ok | warn | intervene
reason: <1-3 句给用户看的裁决理由；verdict 为 ok 时留空>
hint: <给 manager 的一句话提示，说明该返回的问题以及应如何处理（续作修补 / 重做 / 补充哪部分）；仅 intervene 时必填，其余留空>
remedy: patch | restart

判定标准：
- 切题、在职责内、达成了任务 → ok
- 有瑕疵但基本可用（略不完整 / 轻微偏移）→ warn：只给用户看气泡，不改返回内容
- 明显越权、答非所问、或未达成任务 → intervene：会在返回给 manager 的内容后追加审查提示

remedy 的填写规则（仅 intervene 时填，其余留空）：
- patch（默认，优先选）：让该专家在**同一 subsession** 里带着你的审查意见续作修补——它已完成的探索、读过的代码、工具取证都还在上下文里，修订成本很低。绝大多数「形态不对 / 遗漏要点 / 结论需调整」都该选它。
- restart（慎用）：仅当产出**基于虚构事实、方向根本错误、或上下文已被错误结论污染到不可信**，在坏产出上续作反而比重头再来更糟时才选。restart 会废弃该专家本轮全部工作（所有工具调用与探索作废重跑），代价是整轮 token 与时间重烧——选它必须在 reason 里说清为什么 patch 救不回来。"#
    )
}

// ─── Monitor state machine ─────────────────────────────────────────

/// Outcome of feeding one event to the monitor state.
struct StepOutcome {
    findings: Vec<Finding>,
    turn_ended: bool,
    /// The manager just made a task-routing decision (invoked the
    /// `workflow` tool, or delegated to a role). Triggers an LLM review
    /// of the routing choice — even with no deterministic anomaly — so
    /// the advisor can judge whether the dispatch is reasonable. Unlike
    /// a `Finding`, this does NOT auto-inject a corrective hint; the
    /// review only nudges the manager if it actually finds a problem.
    review_requested: bool,
}

struct MonitorState {
    watched_role: String,
    digest: DispatchDigest,
    detectors: TurnDetectors,
    /// Timestamps of recent LLM reviews — the quota is a sliding
    /// time window (`max_reviews_per_turn` per `review_window_secs`),
    /// not per-turn, so a long-running turn can't go blind.
    review_times: std::collections::VecDeque<std::time::Instant>,
    /// D8 state: delegates currently in flight. A DelegateStarted
    /// arriving while zero are open = another serial dispatch.
    open_delegates: usize,
    serial_delegate_streak: usize,
    /// Specialist tool-error streak (non-watched roles): consecutive
    /// errors without an intervening Ok result.
    specialist_error_streak: usize,
    specialist_streak_warned: bool,
    /// D9: the most recent user message, used to judge whether a
    /// zero-tool turn is suspicious (only substantial requests count).
    last_user_text: String,
    /// D7 去重：最近一次上报过的失败 workflow 的 summary。嵌套
    /// workflow 级联失败时，外层事件的 summary 完整内嵌内层的
    /// summary（同一次失败被上报两遍），contains 判定后折叠，
    /// 避免对同一根因连发审查/暂停。
    last_workflow_failure: Option<String>,
    /// 在跑的 workflow 名栈（Started 入栈 / Finished 出栈）：transcript
    /// 里给嵌套启动标注父 workflow，否则 advisor 看到多条平铺的
    /// "[workflow started]" 会把嵌套步骤误读成重复派发（过度编排误报）。
    workflow_stack: Vec<String>,
    /// **会话级**（跨 turn，`reset_turn_state` 不清零）调研 vs 交付物
    /// 计数。路由审查的摘要按 turn 切片，所以 advisor 看得见"这一轮
    /// 派得对不对"，却看不见"已经连着四轮都在调研、用户点名的任务清
    /// 单一个都没产出"——实测会话里它因此把每一轮都判成 ok。
    research_rounds: u32,
    deliverable_rounds: u32,
    /// **已被产出型分派关掉的交付物标签**（会话级，跨 turn）。
    ///
    /// 为什么不能只留 `deliverable_rounds` 这个计数：用户一句话点名两三
    /// 样东西是常态，而计数一旦 >0 就把缺口整体关闭。实测会话
    /// （jemalloc 12:24）诉求是「安排学习计划 + 拆分任务」，命中三个标签；
    /// manager 正文里写明"先跑 `learn`，再跑 `implementation_plan` 拆任务"，
    /// 但 `learn` 一启动 `deliverable_rounds` 就变 1，`session_facts()` 从此
    /// 恒返回 `None`——欠着的任务清单再没有任何机制会提起它，看板至今
    /// 0 个任务。逐标签记账后，关掉一个只关一个。
    deliverables_done: std::collections::BTreeSet<&'static str>,
}

impl MonitorState {
    fn new(watched_role: String) -> Self {
        Self {
            watched_role,
            digest: DispatchDigest::default(),
            detectors: TurnDetectors::default(),
            review_times: std::collections::VecDeque::new(),
            open_delegates: 0,
            serial_delegate_streak: 0,
            specialist_error_streak: 0,
            specialist_streak_warned: false,
            last_user_text: String::new(),
            last_workflow_failure: None,
            workflow_stack: Vec::new(),
            research_rounds: 0,
            deliverable_rounds: 0,
            deliverables_done: std::collections::BTreeSet::new(),
        }
    }

    /// 会话级事实块，拼在 per-turn 摘要之后送进路由审查。
    ///
    /// 判据是**交付物**，不是轮数：用户诉求里点名了某个交付物、
    /// session 里已经有调研在跑、却一次产出型分派都没有。轮数在这里
    /// 故意不作为条件——大仓库分模块探三轮是健康的，按计数报警会误杀
    /// （与已移除的 `WORKFLOW_BUDGET_PER_UNIT_SECS` 同一类错误）。
    /// 交付物一次都没产出才是可举证的问题。
    fn session_facts(&self) -> Option<String> {
        if self.research_rounds == 0 {
            return None;
        }
        let named = named_deliverables(&self.last_user_text);
        if named.is_empty() {
            return None;
        }
        // 逐标签算差集：关掉一个只关一个。跑了 `learn` 不代表
        // 「拆分任务」也交付了（见 `deliverables_done` 的说明）。
        let pending: Vec<&str> = named
            .iter()
            .copied()
            .filter(|d| !self.deliverables_done.contains(d))
            .collect();
        if pending.is_empty() {
            return None;
        }
        let done: Vec<&str> = named
            .iter()
            .copied()
            .filter(|d| self.deliverables_done.contains(d))
            .collect();
        let done_note = if done.is_empty() {
            format!(
                "产出交付物的分派 **0** 次（未跑规划流程、未提交 plan、未产出文档）。\
                 注意：轮数本身不是问题（分模块深入是正当的），\
                 问题是点名的交付物一次都没往产出走。"
            )
        } else {
            format!(
                "其中**已经往产出走的**：{}；**仍然欠着的**：{}。\
                 注意：已交付一样不等于全部交清——一次诉求里点名多样东西时，\
                 每一样都要有自己的产出型分派。上面「仍然欠着的」那几样，\
                 到现在为止**没有任何**对应的分派动作。",
                done.join("、"),
                pending.join("、"),
            )
        };
        Some(format!(
            "\n\n[本 session 累计（跨 turn）] 用户诉求里点名的交付物：{}。\
             已发生调研类分派 {} 次（探索流程 / 通读·解剖·调研类委派）。{}",
            named.join("、"),
            self.research_rounds,
            done_note,
        ))
    }

    /// 累计一次分派的类型（调研 / 交付物）。`satisfied` 是这次分派关掉的
    /// 交付物标签（空 = 不关任何标签）。
    ///
    /// delegate 侧只能靠任务描述判型——关键词表是启发式的，所以只用来
    /// 喂 advisor 的判断材料（advisor 拿到的是"连着 N 轮"这个事实，
    /// 裁决仍由它做），不用来做任何自动阻断。
    fn tally_dispatch(&mut self, kind_is_research: bool, satisfied: &[&'static str]) {
        if kind_is_research {
            self.research_rounds = self.research_rounds.saturating_add(1);
        }
        if !satisfied.is_empty() {
            self.deliverable_rounds = self.deliverable_rounds.saturating_add(1);
            self.deliverables_done.extend(satisfied.iter().copied());
        }
    }

    /// Per-turn reset (turn end of the watched role).
    fn reset_turn_state(&mut self) {
        // 摘要按「本 turn」语义走：标记待清，下一轮首条事件才真正清空
        // （见 DispatchDigest::pending_reset —— EveryTurn 审查在 observe
        // 返回后才读，此刻直接清会让它拿到空摘要）。
        self.digest.mark_reset();
        self.detectors.reset();
        self.review_times.clear();
        self.serial_delegate_streak = 0;
        self.specialist_error_streak = 0;
        self.specialist_streak_warned = false;
    }

    /// Sliding-window quota check: at most `max` reviews within
    /// `window`. Records the review on success.
    fn allow_review(&mut self, max: u32, window: std::time::Duration) -> bool {
        let now = std::time::Instant::now();
        while let Some(t) = self.review_times.front() {
            if now.duration_since(*t) > window {
                self.review_times.pop_front();
            } else {
                break;
            }
        }
        if self.review_times.len() >= max as usize {
            return false;
        }
        self.review_times.push_back(now);
        true
    }

    fn observe(&mut self, ev: &ChatEvent) -> StepOutcome {
        let mut findings = Vec::new();
        let mut turn_ended = false;
        let mut review_requested = false;
        match ev {
            ChatEvent::RoleTurn {
                role_id,
                content,
                sub_id,
                ..
            } => {
                if role_id == "advisor" {
                    // Our own channel-B bubble. Ignore completely —
                    // otherwise the monitor would review itself.
                } else if sub_id.is_some() {
                    // Specialist subsession turn: context only, no
                    // detectors (its tool events aren't broadcast).
                } else if role_id == &self.watched_role {
                    // D1 runs at turn end, before the per-turn reset.
                    if let Some(f) = self.detectors.finish_turn(content) {
                        findings.push(f);
                    }
                    // D9: 整轮零工具调用就收尾。对复杂任务来说正确
                    // 路径是 explore→ask→plan；只在用户消息有实质
                    // 内容（≥20 字符，中文一句话的量级）时提醒，
                    // 闲聊/简短问答不误报。
                    if self.detectors.tool_use_count == 0
                        && self.last_user_text.chars().count() >= 20
                    {
                        findings.push(Finding {
                            kind: DetectorKind::NoToolTurn,
                            hint: "你这一轮没有调用任何工具就结束了。如果这是复杂任务（多文件/多步骤/有取舍分叉），正确路径是 explore → ask → implementation_plan → plan 提交；如果是简单任务且已答完，或信息确实只够直接回答，忽略本提醒。".into(),
                            evidence: format!(
                                "zero-tool turn after user message ({} chars)",
                                self.last_user_text.chars().count()
                            ),
                        });
                    }
                    self.reset_turn_state();
                    turn_ended = true;
                }
            }
            ChatEvent::UserMessage { text } => {
                self.last_user_text = text.clone();
            }
            ChatEvent::ToolUse {
                role_id,
                tool_name,
                args,
                ..
            } if role_id == &self.watched_role => {
                findings.extend(self.detectors.observe_tool_use(tool_name, args));
                // 提交任务清单即产出交付物：任务清单 / 计划两个标签就此关闭
                // （只关这两个——学习材料、文档不因提交清单而交付）。
                if tool_name == "plan" {
                    let closes = crate::workflow::deliverables_produced_by_plan_submit();
                    self.tally_dispatch(false, &closes);
                }
                // 这里**不**触发路由审查。曾经在 `ToolUse{workflow}` 上
                // 置 `review_requested`，但那一刻摘要里还没有任何关于这次
                // 分派的行——描述这次分派的 `[workflow started]` 行是在
                // 随后的 `ChatEvent::WorkflowStarted` 才 push 的，而审查在
                // `observe()` 返回后立刻发出。结果 advisor 拿到的是**空摘要**
                // 加一句"manager 刚做出任务分派决策"，两者直接矛盾。
                //
                // 实测事故（jemalloc 会话 12:24:02）：manager 发了
                // `workflow{name:"learn"}`，advisor 收到空摘要，于是在
                // <think> 里反复纠结「summary is completely empty…this is
                // contradictory」，最后判 warn 并建议"应当优先匹配 `learn`"
                // ——建议 manager 去做它这一轮已经做了的事。真正该被它抓住
                // 的问题（manager 正文里承诺了 `implementation_plan` 却没发
                // 出对应调用）反而完全看不见。
                //
                // 触发点因此下移到 `WorkflowStarted`（仅顶层），那里 push
                // 与置位在同一个 handler 里，证据必然先于审查落地。
            }
            ChatEvent::ToolResult {
                role_id,
                tool_name,
                result,
                ..
            } if role_id == &self.watched_role => {
                self.detectors.observe_tool_result();
            }
            ChatEvent::ToolError {
                role_id,
                tool_name,
                error,
                ..
            } if role_id == &self.watched_role => {
                self.digest
                    .push(format!("[tool_error] {tool_name} → {error}"));
                findings.extend(self.detectors.observe_tool_error(tool_name, error));
            }
            // wf_id.is_none() 守卫：workflow 流程内分派（from_role 也是
            // manager，但带 wf_id 标记）不算 watched role 自己的委派决策
            // —— D8 串行计数与路由评审不覆盖它们（返回审查由 workflow 引
            // 擎的 gate_delegate_return 承担，避免双重审查）。
            ChatEvent::DelegateStarted {
                from_role,
                to_role,
                task,
                wf_id,
                ..
            } if from_role == &self.watched_role && wf_id.is_none() => {
                self.digest.push(format!(
                    "[delegate → {to_role}] {}",
                    truncate_chars(task, 500)
                ));
                // 「通读/解剖」类 delegate 与再跑一轮 explore 等价，
                // 一起计入调研轮次（见 delegate_task_looks_like_research）。
                if delegate_task_looks_like_research(task) {
                    self.tally_dispatch(true, &[]);
                }
                // D8: serial dispatch streak — a delegate started while
                // none are in flight means the previous one finished
                // first. Independent tasks should be dispatched in one
                // parallel batch instead.
                if self.open_delegates == 0 {
                    self.serial_delegate_streak += 1;
                } else {
                    self.serial_delegate_streak = 0;
                }
                self.open_delegates += 1;
                if self.serial_delegate_streak == 3 {
                    findings.push(Finding {
                        kind: DetectorKind::SerialDelegates,
                        hint: "你已连续 3 个 delegate 串行执行（等前一个返回才发下一个）。如果这些任务之间没有依赖关系，应在同一轮一次性批量发出并行执行，能显著节省时间；确有依赖才串行。".into(),
                        evidence: format!("serial delegate streak ≥ 3 (latest → {to_role})"),
                    });
                }
                // Manager delegating to a role is a routing decision —
                // review whether the target role + task split is sound.
                review_requested = true;
            }
            ChatEvent::DelegateFinished {
                from_role,
                to_role,
                status,
                summary,
                wf_id,
                ..
            } if from_role == &self.watched_role && wf_id.is_none() => {
                self.open_delegates = self.open_delegates.saturating_sub(1);
                self.digest.push(format!(
                    "[delegate {to_role} {status}] {}",
                    truncate_chars(summary, 1_500)
                ));
            }
            // Specialist (delegate subsession) tool events: the watched
            // role's arms above don't match these. ToolUse/ToolResult
            // would flood the transcript, so only errors are recorded —
            // a consecutive-error streak means the dispatch itself
            // (bad paths, wrong role, vague task) likely needs fixing.
            ChatEvent::ToolResult { role_id, .. }
                if role_id != &self.watched_role && role_id != "advisor" =>
            {
                self.specialist_error_streak = 0;
            }
            ChatEvent::ToolError {
                role_id,
                tool_name,
                error,
                ..
            } if role_id != &self.watched_role && role_id != "advisor" => {
                self.digest.push(format!(
                    "[tool_error {role_id}] {tool_name} → {}",
                    truncate_chars(error, 500)
                ));
                // 与 watched-role D3 同理：良性探测错误（ENOENT）不计数。
                if !is_benign_probe_error(error) {
                    self.specialist_error_streak += 1;
                }
                if self.specialist_error_streak >= 2 && !self.specialist_streak_warned {
                    self.specialist_streak_warned = true;
                    findings.push(Finding {
                        kind: DetectorKind::ToolErrorStreak,
                        hint: format!(
                            "专家 {role_id} 连续工具出错（最近：{tool_name} → {}）。如果是你的任务描述给了错误路径/背景，修正后重新派发；如果是角色选择不当，换更合适的角色。",
                            truncate_chars(error, 200)
                        ),
                        evidence: format!(
                            "specialist {role_id} tool error streak ≥ 2: {tool_name}"
                        ),
                    });
                }
            }
            ChatEvent::RoleFinished { role_id, detail, .. }
                if role_id == &self.watched_role =>
            {
                // Turn ended without a RoleTurn (error/timeout path):
                // reset per-turn state. D1 has no final text to
                // reconcile, so nothing fires here.
                if detail.contains("error") || detail.contains("timeout") {
                    self.reset_turn_state();
                    turn_ended = true;
                }
            }
            ChatEvent::WorkflowStarted { name, topic, .. } => {
                // 嵌套标注：栈非空说明这是外层 workflow 的步骤里拉起的
                // 子 workflow，不是又一次独立派发。
                let nesting = match self.workflow_stack.last() {
                    Some(parent) => format!(" (nested in {parent})"),
                    None => String::new(),
                };
                self.digest.push(format!(
                    "[workflow started]{nesting} {name}: {}",
                    truncate_chars(topic, 500)
                ));
                // 只统计顶层启动（栈空）：workflow 步骤里拉起的子流程
                // 不是 manager 的又一次分派决策。
                if self.workflow_stack.is_empty() {
                    // 路由审查在这里触发，而不是在 `ToolUse{workflow}`：
                    // 上面那条 `[workflow started]` 已经进摘要，advisor
                    // 一定能看见被审查的那次分派（见 ToolUse 分支的注释）。
                    review_requested = true;
                    let research = crate::workflow::is_research_workflow(name);
                    // 这个流程能关掉哪几个交付物标签，由 DELIVERABLES 表的
                    // `satisfied_by` 列说话——`learn` 只关「教程 / 学习材料」，
                    // 关不掉「任务清单」。旧实现是个布尔（planning 流程或
                    // learn/write_doc/update_docs → true），于是跑一个 learn
                    // 就把同一句诉求里点名的任务清单也一起"交付"了。
                    let closes = crate::workflow::deliverables_produced_by(name);
                    self.tally_dispatch(research, &closes);
                }
                self.workflow_stack.push(name.clone());
            }
            ChatEvent::WorkflowFinished {
                name,
                status,
                summary,
                ..
            } => {
                self.digest.push(format!(
                    "[workflow {name} {status}] {}",
                    truncate_chars(summary, 1_000)
                ));
                // 正常是 LIFO 出栈；容错起见移除栈中最后一个同名项。
                if let Some(pos) = self.workflow_stack.iter().rposition(|n| n == name) {
                    self.workflow_stack.remove(pos);
                }
                // D7: workflow died — the manager is about to decide
                // how to recover; make sure the failure is disclosed
                // instead of silently papered over with delegates.
                if status != "ok" {
                    // 嵌套 workflow 级联失败折叠：外层 summary 完整内嵌
                    // 内层 summary（或反之），同一次失败只报一次，
                    // 否则 monitor 会对同一根因连发审查、连弹暂停。
                    let nested_dup = self
                        .last_workflow_failure
                        .as_deref()
                        .is_some_and(|prev| {
                            summary.contains(prev) || prev.contains(summary.as_str())
                        });
                    self.last_workflow_failure = Some(summary.clone());
                    if !nested_dup {
                        findings.push(Finding {
                            kind: DetectorKind::WorkflowFailed,
                            hint: format!(
                                "workflow '{name}' {status}：{}。如果你打算降级为 delegate 手工继续，必须在最终答复中向用户明确披露该 workflow 失败及降级原因，不得静默略过。",
                                truncate_chars(summary, 300)
                            ),
                            evidence: format!("WorkflowFinished name={name} status={status}"),
                        });
                    }
                }
            }
            _ => {}
        }
        // 任意角色在回答 ask 弹窗（等待拍板）时，advisor 不得介入——
        // 此刻工具错误/循环类 finding 大概率是 wait=true 的 ask 在等
        // 用户选择（实测实锤：workflow 内 architect/tutor 的 ask
        // 挂起时，advisor 却因 ask 超时弹 🛑 干扰用户选择，用户作答
        // 结果丢失、workflow 无法继续）。挂起期间抑制 finding 注入与
        // LLM 审查，用户作答后续跑。用 has_any_pending 而非
        // has_pending_for(watched_role)：被监控的 manager 可能没有挂
        // 起项，但 workflow 里的 specialist 正在 ask 等用户。
        if crate::choice::has_any_pending() {
            findings.clear();
            review_requested = false;
        }
        StepOutcome {
            findings,
            turn_ended,
            review_requested,
        }
    }
}

// ─── Monitor task ──────────────────────────────────────────────────

/// The advisor monitor. Spawned per session by the session creator
/// (ui-server's `create_session_handle`); exits on `ChatEvent::Done`
/// or when the broadcast channel closes (controller dropped).
/// advisor monitor 对**宿主会话**的最小依赖面。
///
/// 为什么要这个 trait：`AdvisorMonitor::spawn` 原来绑死
/// `Arc<ChatController>`，而 CLI REPL 不走 controller，于是 chat 的
/// 顶层 turn **完全没有 advisor 监察**——intervene/warn 气泡、派单路由
/// 审查、以及全部确定性检测器（D3 文件不存在连击、D8 串行 delegate…）
/// 在 chat 里一个都不跑，同一个会话在 UI 里跑就有。
///
/// monitor 循环实际只用到宿主的这 5 件事，抽出来即可两处复用。
pub trait AdvisorHost: Send + Sync + 'static {
    /// 订阅宿主的事件流（monitor 的输入）。
    fn subscribe(&self) -> broadcast::Receiver<ChatEvent>;
    /// 宿主的事件发送端（monitor 发气泡用）。
    fn event_sender(&self) -> broadcast::Sender<ChatEvent>;
    /// 把纠正提示注入被监察角色的下一轮输入。
    fn advisor_hint(&self, text: &str);
    /// 最近一条用户输入，作为「主诉求」喂给审查 prompt。
    fn last_user_input(&self) -> String;
    /// 请求中止当前 turn（terminate 裁决）。
    ///
    /// 返回 boxed future 而非同步接口：controller 的实现除了置取消标志，
    /// 还要往 `input_tx` 投一条 `CancelTurn` 唤醒阻塞在 `recv()` 的循环，
    /// 而那需要 `.await` 一把 tokio Mutex。用 `try_lock` 降级会在锁竞争
    /// 时**丢掉唤醒**——terminate 是安全关键路径，不接受概率性失效。
    /// 手写 boxed future 可以逐字保留原行为，且不必引入 async_trait。
    fn request_cancel_turn(&self)
        -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>;
}

impl AdvisorHost for ChatController {
    fn subscribe(&self) -> broadcast::Receiver<ChatEvent> {
        ChatController::subscribe(self)
    }
    fn event_sender(&self) -> broadcast::Sender<ChatEvent> {
        ChatController::event_sender(self)
    }
    fn advisor_hint(&self, text: &str) {
        ChatController::advisor_hint(self, text)
    }
    fn last_user_input(&self) -> String {
        ChatController::last_user_input(self)
    }
    fn request_cancel_turn(&self)
        -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async move { ChatController::cancel_turn(self).await })
    }
}


pub struct AdvisorMonitor;

impl AdvisorMonitor {
    /// Spawn the monitor task.
    ///
    /// - `controller`: source of events (subscribe), hint queue
    ///   (channel A), bubble sender (channel B), and the last user
    ///   question.
    /// - `watched_role`: the role whose turns are monitored (the
    ///   session's primary role, usually `"manager"`).
    pub fn spawn(
        controller: Arc<ChatController>,
        config: AdvisorMonitorConfig,
        review_engine: AdvisorReviewEngine,
        watched_role: String,
    ) -> tokio::task::JoinHandle<()> {
        Self::spawn_on_host(controller, config, review_engine, watched_role)
    }

    /// 与 [`Self::spawn`] 同一个循环，但宿主换成 [`AdvisorHost`]——
    /// 让不走 `ChatController` 的宿主（CLI REPL）也能跑监察。
    pub fn spawn_on_host(
        controller: Arc<dyn AdvisorHost>,
        config: AdvisorMonitorConfig,
        review_engine: AdvisorReviewEngine,
        watched_role: String,
    ) -> tokio::task::JoinHandle<()> {
        let mut rx = controller.subscribe();
        let bubble_tx = controller.event_sender();
        tokio::spawn(async move {
            let mut state = MonitorState::new(watched_role);
            loop {
                let ev = match rx.recv().await {
                    Ok(ev) => ev,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("advisor monitor lagged {n} events; continuing");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                if matches!(ev, ChatEvent::Done) {
                    break;
                }

                let outcome = state.observe(&ev);

                // Channel A, deterministic: inject each finding's hint
                // immediately — the runner drains it at the next
                // tool-round boundary, no LLM latency involved.
                for finding in &outcome.findings {
                    controller.advisor_hint(&finding.hint);
                }

                let should_review = match config.review_mode {
                    AdvisorReviewMode::Off => false,
                    // Routing decisions (workflow / delegate) request a
                    // review even without a deterministic anomaly.
                    AdvisorReviewMode::OnAnomaly => {
                        !outcome.findings.is_empty() || outcome.review_requested
                    }
                    AdvisorReviewMode::EveryTurn => {
                        outcome.turn_ended || outcome.review_requested
                    }
                };
                if !should_review {
                    continue;
                }
                if !state.allow_review(
                    config.max_reviews_per_turn,
                    Duration::from_secs(config.review_window_secs),
                ) {
                    tracing::warn!(
                        "advisor monitor: review cap ({}/{}s window) reached; skipping",
                        config.max_reviews_per_turn,
                        config.review_window_secs
                    );
                    continue;
                }

                let trigger = if !outcome.findings.is_empty() {
                    let mut s = "命中确定性检测器：".to_string();
                    for f in &outcome.findings {
                        s.push_str(&format!("\n- {} {:?}：{}", f.kind.label(), f.kind, f.evidence));
                    }
                    s
                } else if outcome.review_requested {
                    "manager 刚做出任务分派决策（调用 workflow 或委派角色）。\
                     请重点核查第 5 项【分派路由是否合理】：\n\
                     - 简单任务是否被过度编排（本可直接完成）？\n\
                     - 复杂任务是否优先尝试匹配 workflow？所选 workflow 是否对口？\n\
                     - 无合适 workflow 而自行委派时，目标角色与任务拆分是否合理？"
                        .to_string()
                } else {
                    "例行审查（every_turn 模式）：本 turn 未命中确定性检测器".to_string()
                };
                let question = controller.last_user_input();
                // per-turn 摘要 + 会话级累计事实。后者只在存在交付物
                // 缺口时出现，让 advisor 能看见"连着几轮只调研"——
                // 单看一轮摘要，每一轮的路由都是"合理"的。
                let mut digest = state.digest.render();
                if let Some(facts) = state.session_facts() {
                    digest.push_str(&facts);
                }

                let review = tokio::time::timeout(
                    Duration::from_secs(REVIEW_TIMEOUT_SECS),
                    review_engine.review(&question, &digest, &trigger),
                )
                .await;
                let verdict = match review {
                    Ok(Ok(v)) => v,
                    Ok(Err(e)) => {
                        // Advisor model unavailable / build failed:
                        // silent degrade — deterministic hints already
                        // went out; the main session is untouched.
                        tracing::warn!("advisor review failed (degraded): {e}");
                        continue;
                    }
                    Err(_) => {
                        tracing::warn!("advisor review timed out after {REVIEW_TIMEOUT_SECS}s");
                        continue;
                    }
                };

                if verdict.verdict == Verdict::Ok {
                    continue;
                }
                // Channel B: user-visible 🦉 bubble (both levels).
                let label = match verdict.verdict {
                    Verdict::Warn => "⚠️ warn",
                    Verdict::Intervene => "🛑 intervene",
                    Verdict::Terminate => "🛑 terminate",
                    Verdict::Ok => unreachable!(),
                };
                let reason = if verdict.reason.is_empty() {
                    "advisor 检测到处理过程异常，建议关注。".to_string()
                } else {
                    verdict.reason.clone()
                };
                let mut bubble = format!("{label}：{reason}");
                // Channel A：warn 与 intervene 都把 hint 注入 manager 的
                // hint 队列（下一 tool-round 边界生效）。实测事故前
                // warn 只发气泡——advisor 警告「过度编排」时 manager 完全
                // 收不到，继续狂奔。hint 为空时仍只发气泡。
                if matches!(verdict.verdict, Verdict::Warn | Verdict::Intervene)
                    && !verdict.hint.is_empty()
                {
                    bubble.push_str(&format!(
                        "\n\n> 给 {} 的纠正提示：{}",
                        state.watched_role, verdict.hint
                    ));
                    controller.advisor_hint(&verdict.hint);
                }
                let _ = bubble_tx.send(ChatEvent::RoleTurn {
                    role_id: "advisor".to_string(),
                    content: bubble,
                    is_complete: true,
                    sub_id: None,
                });

                // v4 行为变更（用户拍板）：`Verdict::Intervene` = **纠正并
                // 继续**，不再置位暂停门/弹拍板窗。气泡与纠正 hint 已在
                // 上面发出并注入 manager 的 hint 队列，manager 在下一个
                // tool-round 边界 drain 到 hint 后自行纠偏，主会话全程
                // 不停。保留暂停能力的唯一路径是「模型不可用」
                // （agent.rs `pause_wait_model_unavailable`，走
                // AgentPauseGate，与本 monitor 无关）。
                //
                // 历史：v3 曾在此 `request_pause()` + ChoiceRequested
                // 等用户拍板。实践中 advisor 的 intervene 多次打在健康
                // 运行上（良性工具报错、gate 按设计拒绝等），每次误停
                // 都要人工解锁，代价远大于收益，故移除。AdvisorPauseGate
                // 结构与 runner 侧接线保留（休眠），如需恢复暂停语义
                // 只需在此重新调 request_pause()。

                if verdict.verdict == Verdict::Terminate {
                    // 让 manager 真正停下来：取消当前 in-flight turn。
                    // 这是 **软终止**——不是 abort 整个 session：
                    // turn_cancel_flag 被 driver（run_turn_cancellable
                    // 的 500ms tick）看到后 drop 掉 in-flight 的
                    // run_turn future，driver 回到等用户输入；若 turn
                    // 刚好已结束，flag 会在下个 turn 入口被清掉，不会
                    // 误伤下一轮。语义见 ChatEvent::AdvisorTerminated。
                    controller.request_cancel_turn().await;
                    let _ = bubble_tx.send(ChatEvent::AdvisorTerminated {
                        role_id: state.watched_role.clone(),
                        reason: reason.clone(),
                        detector: Some("LLM".to_string()),
                        sub_id: None,
                    });
                }
            }
        })
    }
}

// ─── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ModelCatalog, ModelDef};

    fn tool_use(name: &str, args: &str) -> ChatEvent {
        ChatEvent::ToolUse {
            role_id: "manager".into(),
            tool_name: name.into(),
            args: args.into(),
            sub_id: None,
        }
    }

    fn tool_result(name: &str, result: &str) -> ChatEvent {
        ChatEvent::ToolResult {
            role_id: "manager".into(),
            tool_name: name.into(),
            result: result.into(),
            sub_id: None,
        }
    }

    fn tool_error(name: &str, error: &str) -> ChatEvent {
        ChatEvent::ToolError {
            role_id: "manager".into(),
            tool_name: name.into(),
            error: error.into(),
            sub_id: None,
        }
    }

    fn role_turn(content: &str) -> ChatEvent {
        ChatEvent::RoleTurn {
            role_id: "manager".into(),
            content: content.into(),
            is_complete: true,
            sub_id: None,
        }
    }

    fn state() -> MonitorState {
        MonitorState::new("manager".into())
    }

    // ── review quota: sliding time window ─────────────────────────

    #[test]
    fn review_quota_is_a_sliding_time_window() {
        let mut s = state();
        let window = std::time::Duration::from_millis(50);
        assert!(s.allow_review(2, window));
        assert!(s.allow_review(2, window));
        assert!(!s.allow_review(2, window), "quota exhausted within window");
        std::thread::sleep(std::time::Duration::from_millis(60));
        assert!(s.allow_review(2, window), "window slid past — quota recovered");
    }

    // ── D8: serial delegate streak ─────────────────────────────────

    fn delegate_started(to: &str) -> ChatEvent {
        ChatEvent::DelegateStarted {
            from_role: "manager".into(),
            to_role: to.into(),
            task: "task".into(),
            sub_id: format!("{to}-1"),
            wf_id: None,
        }
    }

    fn delegate_finished(to: &str) -> ChatEvent {
        ChatEvent::DelegateFinished {
            from_role: "manager".into(),
            to_role: to.into(),
            status: "ok".into(),
            summary: "summary".into(),
            sub_id: format!("{to}-1"),
            wf_id: None,
        }
    }

    #[test]
    fn d8_fires_on_three_serial_delegates() {
        let mut s = state();
        assert!(s.observe(&delegate_started("pm")).findings.is_empty());
        s.observe(&delegate_finished("pm"));
        assert!(s.observe(&delegate_started("designer")).findings.is_empty());
        s.observe(&delegate_finished("designer"));
        let out = s.observe(&delegate_started("programmer"));
        assert_eq!(out.findings.len(), 1);
        assert_eq!(out.findings[0].kind, DetectorKind::SerialDelegates);
        // 只报一次（== 3 时才 fire）
        let out = s.observe(&delegate_finished("programmer"));
        assert!(out.findings.is_empty());
        let out = s.observe(&delegate_started("tester"));
        assert!(out.findings.is_empty());
    }

    #[test]
    fn d8_silent_when_delegates_overlap() {
        let mut s = state();
        s.observe(&delegate_started("pm"));
        // 第二个在第一个未结束时发出 = 并行，streak 重置
        assert!(s.observe(&delegate_started("designer")).findings.is_empty());
        s.observe(&delegate_finished("pm"));
        // designer 仍在跑，再发一个也算并行
        assert!(s.observe(&delegate_started("programmer")).findings.is_empty());
    }

    // ── specialist tool errors ─────────────────────────────────────

    fn specialist_error(role: &str) -> ChatEvent {
        ChatEvent::ToolError {
            role_id: role.into(),
            tool_name: "read".into(),
            error: "permission denied (os error 13)".into(),
            sub_id: None,
        }
    }

    #[test]
    fn specialist_error_streak_fires_once_per_turn() {
        let mut s = state();
        assert!(s.observe(&specialist_error("programmer")).findings.is_empty());
        let out = s.observe(&specialist_error("programmer"));
        assert_eq!(out.findings.len(), 1);
        assert_eq!(out.findings[0].kind, DetectorKind::ToolErrorStreak);
        assert!(out.findings[0].hint.contains("programmer"));
        // 第三次不重复报
        assert!(s.observe(&specialist_error("programmer")).findings.is_empty());
    }

    #[test]
    fn specialist_ok_result_resets_streak() {
        let mut s = state();
        s.observe(&specialist_error("programmer"));
        s.observe(&ChatEvent::ToolResult {
            role_id: "programmer".into(),
            tool_name: "read".into(),
            result: "ok".into(),
            sub_id: None,
        });
        // streak 被重置，单个错误不再触发
        assert!(s.observe(&specialist_error("programmer")).findings.is_empty());
    }

    #[test]
    fn watched_role_tool_error_still_hits_d3_not_specialist_path() {
        let mut s = state();
        s.observe(&tool_error("read", "boom"));
        let out = s.observe(&tool_error("read", "boom"));
        assert_eq!(out.findings.len(), 1);
        assert_eq!(out.findings[0].kind, DetectorKind::ToolErrorStreak);
        // 走的是 watched-role 检测器，不污染 specialist streak
        assert_eq!(s.specialist_error_streak, 0);
    }

    // ── delegate 复审 prompt：response 预算与截断提示 ──────────────

    /// 主会话主题必须进入委派返回审查——它是「是否符合预期」的准绳。
    ///
    /// 缺了它，advisor 只能拿「被派的子任务」当基准，无法发现「子任务做得
    /// 漂亮但整体跑偏」。对 workflow step 而言这个主题就是整条 workflow 的
    /// topic（见 `SpeakerDispatch::main_topic`）。
    #[test]
    fn delegate_review_prompt_carries_main_topic() {
        let p = build_delegate_review_prompt(
            "把某项目的学习计划拆成任务",
            "programmer",
            "写代码",
            "读 src/arena.c 并总结",
            "结果正文",
            "",
        );
        assert!(
            p.contains("把某项目的学习计划拆成任务"),
            "主会话诉求必须出现在 prompt 里: {p}"
        );
        assert!(
            p.contains("原始诉求"),
            "要有明确的段落标题让模型知道这是准绳: {p}"
        );
    }

    /// 主题为空时不得让 advisor 拿它当依据下判（旧 workflow 路径传空串的
    /// 遗留场景，以及任何取不到用户输入的入口）。
    #[test]
    fn delegate_review_prompt_handles_missing_main_topic() {
        let p = build_delegate_review_prompt("", "programmer", "写代码", "任务", "结果", "");
        assert!(
            p.contains("未记录到原始用户诉求"),
            "空主题要显式告知，不能留白让模型自行想象: {p}"
        );
    }

    #[test]
    fn delegate_review_prompt_fits_typical_report_without_truncation() {
        // 25K 字符的专家报告（本次事故的实际尺寸）应完整进入 prompt。
        let report = "x".repeat(25_000);
        let p = build_delegate_review_prompt("主诉求", "programmer", "写代码", "任务", &report, "");
        assert!(p.contains(&report), "25K 报告不应被截断");
        assert!(!p.contains("被截断"), "未截断时不应出现截断提示");
    }

    #[test]
    fn delegate_review_prompt_notes_truncation_when_over_budget() {
        let report = "x".repeat(DELEGATE_REVIEW_RESPONSE_MAX_CHARS + 10_000);
        let p = build_delegate_review_prompt("主诉求", "programmer", "写代码", "任务", &report, "");
        assert!(p.contains("被截断"), "超预算时必须显式告知复审模型");
        assert!(p.contains("[+"), "保留截断标记");
    }

    // ── D3: 良性探测错误（ENOENT）豁免 ────────────────────────────
    // 复现自真实事故：programmer 与列目录同批猜读 README.md /
    // CONTRIBUTING.md，两个 ENOENT 触发 D3 → advisor intervene →
    // 全 session 暂停。探测性失败是正常探索成本，不应计数。

    const ENOENT: &str =
        "Tool execution failed: read (attempt 1): stat: No such file or directory (os error 2)";

    #[test]
    fn d3_ignores_benign_enoent_probes() {
        let mut s = state();
        assert!(s.observe(&tool_error("read", ENOENT)).findings.is_empty());
        assert!(s.observe(&tool_error("read", ENOENT)).findings.is_empty());
        assert!(s.observe(&tool_error("read", ENOENT)).findings.is_empty());
    }

    #[test]
    fn d3_ignores_stale_line_range_probes() {
        // 过期行号越界读（真实事故：读 tcache.c:1660 但文件只有 1462
        // 行，连续 4 次触发 D3 → advisor 暂停了健康会话）。与 ENOENT
        // 同类豁免。
        let mut s = state();
        let e1 = "start_line 1660 exceeds file length 1462";
        let e2 = "start_line 4920 exceeds file length 3459";
        assert!(s.observe(&tool_error("read", e1)).findings.is_empty());
        assert!(s.observe(&tool_error("read", e1)).findings.is_empty());
        assert!(s.observe(&tool_error("read", e2)).findings.is_empty());
        assert!(s.observe(&tool_error("read", e2)).findings.is_empty());
    }

    #[test]
    fn d3_enoent_neither_counts_nor_resets_streak() {
        let mut s = state();
        // 真实错误 → ENOENT 探测 → 真实错误：streak 延续，第二个
        // 真实错误到达 ≥2 即触发。
        assert!(s.observe(&tool_error("bash", "exit code 1")).findings.is_empty());
        assert!(s.observe(&tool_error("read", ENOENT)).findings.is_empty());
        let out = s.observe(&tool_error("bash", "exit code 1"));
        assert_eq!(out.findings.len(), 1);
        assert_eq!(out.findings[0].kind, DetectorKind::ToolErrorStreak);
    }

    #[test]
    fn specialist_enoent_probes_do_not_fire_streak() {
        let mut s = state();
        let enoent = || ChatEvent::ToolError {
            role_id: "programmer".into(),
            tool_name: "read".into(),
            error: ENOENT.into(),
            sub_id: None,
        };
        assert!(s.observe(&enoent()).findings.is_empty());
        assert!(s.observe(&enoent()).findings.is_empty());
        assert_eq!(s.specialist_error_streak, 0);
        // 之后真实错误仍从 0 开始累计，单个不触发。
        assert!(s.observe(&specialist_error("programmer")).findings.is_empty());
        let out = s.observe(&specialist_error("programmer"));
        assert_eq!(out.findings.len(), 1);
        assert_eq!(out.findings[0].kind, DetectorKind::ToolErrorStreak);
    }

    // ── D9: zero-tool turn after substantial user message ─────────

    #[test]
    fn d9_fires_on_zero_tool_turn_after_substantial_request() {
        let mut s = state();
        s.observe(&ChatEvent::UserMessage {
            text: "帮我分析一下这个项目的架构并给出三个可落地的重构建议".into(),
        });
        let out = s.observe(&role_turn("这个项目架构不错，建议如下：……"));
        assert_eq!(out.findings.len(), 1);
        assert_eq!(out.findings[0].kind, DetectorKind::NoToolTurn);
        assert!(out.findings[0].hint.contains("没有调用任何工具"));
        assert!(out.turn_ended);
    }

    #[test]
    fn d9_silent_with_short_message_or_tool_use() {
        // 短消息（闲聊）不报
        let mut s = state();
        s.observe(&ChatEvent::UserMessage { text: "你好".into() });
        assert!(s.observe(&role_turn("你好！有什么可以帮你？")).findings.is_empty());

        // 用了工具不报
        let mut s = state();
        s.observe(&ChatEvent::UserMessage {
            text: "帮我分析一下这个项目的架构并给出三个可落地的重构建议".into(),
        });
        s.observe(&tool_use("search", "{\"q\":\"arch\"}"));
        assert!(s.observe(&role_turn("查完了，结论如下：……")).findings.is_empty());
    }

    // ── D7: workflow failed ────────────────────────────────────────
    #[test]
    fn workflow_started_marks_nesting_in_transcript() {
        let mut s = state();
        s.observe(&ChatEvent::WorkflowStarted {
            name: "design_and_plan".into(),
            topic: "t".into(),
            wf_id: "wf-outer".into(),
        });
        s.observe(&ChatEvent::WorkflowStarted {
            name: "explore".into(),
            topic: "t".into(),
            wf_id: "wf-inner".into(),
        });
        let rendered = s.digest.render();
        // 嵌套启动必须标注父 workflow，否则 advisor 会把嵌套步骤
        // 误读成对同一请求的重复派发（过度编排误报）。
        assert!(
            rendered.contains("[workflow started] (nested in design_and_plan) explore"),
            "transcript: {rendered}"
        );
        // 内层结束后回到平铺语义。
        s.observe(&ChatEvent::WorkflowFinished {
            name: "explore".into(),
            wf_id: "wf-inner".into(),
            status: "ok".into(),
            summary: "done".into(),
        });
        s.observe(&ChatEvent::WorkflowFinished {
            name: "design_and_plan".into(),
            wf_id: "wf-outer".into(),
            status: "ok".into(),
            summary: "done".into(),
        });
        s.observe(&ChatEvent::WorkflowStarted {
            name: "code_review".into(),
            topic: "t".into(),
            wf_id: "wf-next".into(),
        });
        let rendered = s.digest.render();
        assert!(
            rendered.contains("[workflow started] code_review"),
            "transcript: {rendered}"
        );
    }

    /// 路由审查触发点必须**晚于**该次分派进摘要。
    ///
    /// 实测事故（jemalloc 会话 12:24:02）：触发点原先挂在
    /// `ToolUse{workflow}`，而描述这次分派的 `[workflow started]` 行是随后
    /// 的 `WorkflowStarted` 才 push 的。审查在 `observe()` 返回后立刻发出，
    /// 于是 advisor 收到「manager 刚做出分派决策」+ **空摘要**，两者直接
    /// 矛盾；它猜了半页 <think> 之后判 warn，建议 manager 去用 `learn`
    /// ——而 manager 那一轮发的就是 `learn`。
    #[test]
    fn workflow_routing_review_fires_only_after_dispatch_is_in_digest() {
        let mut s = state();
        // 工具调用本身不再触发审查：此刻摘要里还没有这次分派。
        let out = s.observe(&ChatEvent::ToolUse {
            role_id: "manager".into(),
            tool_name: "workflow".into(),
            args: r#"{"name":"learn","topic":"jemalloc"}"#.into(),
            sub_id: None,
        });
        assert!(
            !out.review_requested,
            "ToolUse 阶段摘要还是空的，此时审查等于让 advisor 盲判"
        );
        // workflow 真正启动时才触发，且摘要里已有对应行。
        let out = s.observe(&ChatEvent::WorkflowStarted {
            name: "learn".into(),
            topic: "jemalloc".into(),
            wf_id: "wf-1".into(),
        });
        assert!(out.review_requested, "顶层 workflow 启动必须触发路由审查");
        let rendered = s.digest.render();
        assert!(
            rendered.contains("[workflow started] learn"),
            "触发审查时摘要必须已含这次分派: {rendered}"
        );
    }

    /// 嵌套子 workflow 不重复触发路由审查（它不是 manager 的又一次分派）。
    #[test]
    fn nested_workflow_start_does_not_request_review() {
        let mut s = state();
        s.observe(&ChatEvent::WorkflowStarted {
            name: "design_and_plan".into(),
            topic: "t".into(),
            wf_id: "wf-outer".into(),
        });
        let out = s.observe(&ChatEvent::WorkflowStarted {
            name: "explore".into(),
            topic: "t".into(),
            wf_id: "wf-inner".into(),
        });
        assert!(
            !out.review_requested,
            "嵌套启动是流程内部步骤，不该当成新的分派决策再审一遍"
        );
    }

    /// 空摘要必须自己说明「空」是什么意思，不能真的留白。
    #[test]
    fn empty_digest_states_its_own_emptiness() {
        let d = DispatchDigest::default();
        let rendered = d.render();
        assert!(!rendered.trim().is_empty(), "空摘要不能渲染成空白");
        assert!(
            rendered.contains("没有记录到任何分派动作"),
            "必须直说没有分派动作: {rendered}"
        );
        assert!(
            rendered.contains("不要据此推断"),
            "必须拦住 advisor 从留白里外推: {rendered}"
        );
    }

    // ── 交付物缺口：按「点名的交付物」判，不按轮数 ─────────────────
    fn wf_started(name: &str) -> ChatEvent {
        ChatEvent::WorkflowStarted {
            name: name.into(),
            topic: "t".into(),
            wf_id: format!("wf-{name}"),
        }
    }

    fn wf_finished(name: &str) -> ChatEvent {
        ChatEvent::WorkflowFinished {
            name: name.into(),
            wf_id: format!("wf-{name}"),
            status: "ok".into(),
            summary: "done".into(),
        }
    }

    fn user_msg(text: &str) -> ChatEvent {
        ChatEvent::UserMessage { text: text.into() }
    }

    /// 实测那一条诉求：点名了"学习计划"和"拆分任务"两个交付物。
    const JEMALLOC_ASK: &str = "查看代码  安排学习计划  拆分任务（从浅到深学习）";

    #[test]
    fn gap_reported_when_a_named_deliverable_never_moves() {
        let mut s = state();
        s.observe(&user_msg(JEMALLOC_ASK));
        s.observe(&wf_started("explore"));
        s.observe(&wf_finished("explore"));
        // 跨 turn 不清零：turn 结束不能抹掉"交付物还没产出"这个事实。
        s.observe(&role_turn("请在上方选择"));
        let facts = s.session_facts().expect("gap must be reported");
        assert!(facts.contains("任务清单"), "{facts}");
        assert!(facts.contains("计划"), "{facts}");
        assert!(facts.contains("**0**"), "{facts}");
        // 明确声明轮数不是判据，避免 advisor 误读成"探多了就报警"。
        assert!(facts.contains("轮数本身不是问题"), "{facts}");
    }

    /// 关键的不误杀用例：探了 5 轮，但用户只是问"这块代码怎么回事"
    /// ——调研本身就是交付物，不该报缺口。
    #[test]
    fn many_research_rounds_are_fine_when_no_deliverable_was_named() {
        let mut s = state();
        s.observe(&user_msg("arena 和 extent 之间的锁顺序是怎么回事？有什么坑"));
        for _ in 0..5 {
            s.observe(&wf_started("explore"));
            s.observe(&wf_finished("explore"));
        }
        assert_eq!(s.research_rounds, 5);
        assert!(
            s.session_facts().is_none(),
            "诉求没点名交付物时，探几轮都不该报警"
        );
    }

    #[test]
    fn single_research_round_already_counts_when_deliverable_stalls() {
        let mut s = state();
        s.observe(&user_msg(JEMALLOC_ASK));
        s.observe(&wf_started("explore"));
        s.observe(&wf_finished("explore"));
        // 判据是交付物没动，不是"探够了 2 轮"——1 轮就足以报缺口。
        assert!(s.session_facts().is_some());
    }

    #[test]
    fn research_style_delegates_count_toward_the_gap() {
        let mut s = state();
        s.observe(&user_msg(JEMALLOC_ASK));
        // 「通读…产出解剖报告」与再跑一轮 explore 等价，必须计入。
        s.observe(&ChatEvent::DelegateStarted {
            from_role: "manager".into(),
            to_role: "architect_system".into(),
            task: "通读本仓库，产出「从浅到深学习路径」解剖报告".into(),
            sub_id: "a-1".into(),
            wf_id: None,
        });
        let facts = s.session_facts().expect("gap must be reported");
        assert!(facts.contains("调研类分派 1 次"), "{facts}");
    }

    #[test]
    fn implementation_delegates_do_not_count_as_research() {
        let mut s = state();
        s.observe(&user_msg(JEMALLOC_ASK));
        // 带落地动词的委派是产出，不是调研。
        s.observe(&ChatEvent::DelegateStarted {
            from_role: "manager".into(),
            to_role: "programmer".into(),
            task: "实现 ctl 端点扩展并补充验收标准".into(),
            sub_id: "p-1".into(),
            wf_id: None,
        });
        assert_eq!(s.research_rounds, 0);
        assert!(s.session_facts().is_none());
    }

    /// 规划流程只关掉它自己产出的那几个标签，**不**顺手关掉别的。
    ///
    /// `JEMALLOC_ASK` 同时点名了「任务清单 / 计划 / 教程 / 学习材料」三样。
    /// 旧实现记一个布尔，跑 `implementation_plan` 就把教程也算交付了；
    /// 现在教程仍然欠着，缺口继续报，但报的是**差集**。
    #[test]
    fn planning_workflow_closes_only_its_own_deliverables() {
        let mut s = state();
        s.observe(&user_msg(JEMALLOC_ASK));
        s.observe(&wf_started("explore"));
        s.observe(&wf_finished("explore"));
        assert!(s.session_facts().is_some());
        s.observe(&wf_started("implementation_plan"));
        let facts = s
            .session_facts()
            .expect("教程还欠着，缺口不该整体关闭");
        assert!(facts.contains("已经往产出走的"), "{facts}");
        assert!(facts.contains("任务清单"), "{facts}");
        assert!(facts.contains("仍然欠着的"), "{facts}");
        assert!(facts.contains("教程 / 学习材料"), "{facts}");
        // 把最后一样也跑掉，缺口才真正关闭。
        // 先让 implementation_plan 结束——栈非空时 learn 会被当成嵌套子
        // 流程（流程内部步骤不是 manager 的又一次分派，不记账）。
        s.observe(&wf_finished("implementation_plan"));
        s.observe(&wf_started("learn"));
        assert!(
            s.session_facts().is_none(),
            "三个标签全部产出后不该再报缺口"
        );
    }

    /// 本次修复的核心回归：跑 `learn` 关不掉「拆分任务」。
    ///
    /// 实测事故（jemalloc 会话 12:24）：manager 正文写明"先 `learn`，再
    /// `implementation_plan` 拆任务"，只发出了 `learn`。旧的布尔记账让
    /// `session_facts()` 从此恒为 `None`，advisor 再也不会提起欠着的任务
    /// 清单——看板至今 0 个任务。
    #[test]
    fn learn_does_not_close_the_task_list_gap() {
        let mut s = state();
        s.observe(&user_msg(JEMALLOC_ASK));
        s.observe(&wf_started("explore"));
        s.observe(&wf_finished("explore"));
        s.observe(&wf_started("learn"));
        let facts = s
            .session_facts()
            .expect("任务清单还欠着，必须继续报缺口");
        assert!(facts.contains("仍然欠着的"), "{facts}");
        assert!(facts.contains("任务清单"), "{facts}");
        assert!(
            facts.contains("**已经往产出走的**：教程 / 学习材料"),
            "已交付的那样要如实标出，否则 advisor 会重复催 learn: {facts}"
        );
    }

    #[test]
    fn plan_submission_closes_the_task_list_gap_only() {
        let mut s = state();
        s.observe(&user_msg(JEMALLOC_ASK));
        s.observe(&wf_started("explore"));
        s.observe(&wf_finished("explore"));
        assert!(s.session_facts().is_some());
        s.observe(&tool_use("plan", "{\"tasks\":[]}"));
        let facts = s
            .session_facts()
            .expect("提交清单关不掉「教程」这一样");
        assert!(facts.contains("任务清单"), "{facts}");
        assert!(facts.contains("教程 / 学习材料"), "{facts}");
    }

    /// 单一交付物的诉求下，跑对流程就该彻底关闭缺口（不引入误报）。
    #[test]
    fn single_named_deliverable_is_fully_closed_by_its_workflow() {
        let mut s = state();
        s.observe(&user_msg("先摸清结构，然后给我拆分任务"));
        s.observe(&wf_started("explore"));
        s.observe(&wf_finished("explore"));
        assert!(s.session_facts().is_some(), "任务清单欠着时要报");
        s.observe(&wf_started("implementation_plan"));
        assert!(
            s.session_facts().is_none(),
            "只点名了任务清单，跑了规划流程就该彻底关闭"
        );
    }

    #[test]
    fn nested_research_workflow_counts_once() {
        let mut s = state();
        // design_and_plan 内部拉起 explore：那是流水线的一步，
        // 不是 manager 的又一次调研决策。
        s.observe(&wf_started("design_and_plan"));
        s.observe(&wf_started("explore"));
        s.observe(&wf_finished("explore"));
        s.observe(&wf_finished("design_and_plan"));
        assert_eq!(s.research_rounds, 0, "嵌套子流程不计入调研");
        assert_eq!(s.deliverable_rounds, 1, "design_and_plan 是规划流程");
    }

    #[test]
    fn learn_workflow_counts_as_deliverable_not_research() {
        let mut s = state();
        s.observe(&wf_started("learn"));
        assert_eq!(s.research_rounds, 0);
        assert_eq!(s.deliverable_rounds, 1, "learn 会落盘教程，是交付物");
    }

    #[test]
    fn named_deliverables_recognizes_the_observed_request() {
        let named = named_deliverables(JEMALLOC_ASK);
        assert!(named.contains(&"任务清单"), "{named:?}");
        assert!(named.contains(&"计划"), "{named:?}");
        // 纯咨询类问句不该被识别成有交付物。
        assert!(named_deliverables("这个函数为什么会崩？").is_empty());
        assert!(named_deliverables("arena 的锁顺序是怎么回事").is_empty());
    }

    #[test]
    fn d7_fires_on_workflow_finished_not_ok() {
        let mut s = state();
        let out = s.observe(&ChatEvent::WorkflowStarted {
            name: "design_brainstorm".into(),
            topic: "设计新功能".into(),
            wf_id: "wf-1".into(),
        });
        assert!(out.findings.is_empty());

        let out = s.observe(&ChatEvent::WorkflowFinished {
            name: "design_brainstorm".into(),
            wf_id: "wf-1".into(),
            status: "failed".into(),
            summary: "step 'evaluate': all models unavailable".into(),
        });
        assert_eq!(out.findings.len(), 1);
        assert_eq!(out.findings[0].kind, DetectorKind::WorkflowFailed);
        assert!(out.findings[0].hint.contains("披露"));
        assert!(out.findings[0].hint.contains("design_brainstorm"));
    }

    /// 嵌套 workflow 级联失败：外层 summary 完整内嵌内层 summary，
    /// 同一根因只报一次，避免连发审查/连弹暂停（日志事故：
    /// architect 空返回 → explore 与 design_and_plan 双双重报 →
    /// advisor 10 秒内连弹两个暂停）。
    #[test]
    fn d7_nested_cascade_reports_once() {
        let mut s = state();
        let inner = "step 'synthesize' speaker 'architect': subagent failed（已完成 1/2 步）";
        let outer =
            format!("step 'explore' nested workflow 'explore': {inner}（已完成 0/6 步）");

        let out1 = s.observe(&ChatEvent::WorkflowFinished {
            name: "explore".into(),
            wf_id: "wf-1".into(),
            status: "failed".into(),
            summary: inner.into(),
        });
        assert_eq!(out1.findings.len(), 1);

        let out2 = s.observe(&ChatEvent::WorkflowFinished {
            name: "design_and_plan".into(),
            wf_id: "wf-2".into(),
            status: "failed".into(),
            summary: outer,
        });
        assert!(
            out2.findings.is_empty(),
            "嵌套级联的重复失败不应再次上报: {:?}",
            out2.findings
        );

        // 根因无关的新失败仍应正常上报。
        let out3 = s.observe(&ChatEvent::WorkflowFinished {
            name: "unrelated".into(),
            wf_id: "wf-3".into(),
            status: "failed".into(),
            summary: "completely different root cause".into(),
        });
        assert_eq!(out3.findings.len(), 1);
    }

    #[test]
    fn d7_silent_on_workflow_ok() {
        let mut s = state();
        let out = s.observe(&ChatEvent::WorkflowFinished {
            name: "design_brainstorm".into(),
            wf_id: "wf-2".into(),
            status: "ok".into(),
            summary: "done".into(),
        });
        assert!(out.findings.is_empty());
    }

    // ── D1: dropped tool call ──────────────────────────────────────

    #[test]
    fn d1_fires_when_turn_text_has_more_opens_than_executions() {
        let mut s = state();
        // One call executed, but the final answer contains two opens.
        assert!(s.observe(&tool_use("read", "{\"path\":\"a\"}")).findings.is_empty());
        let out = s.observe(&role_turn(
            "我来读两个文件\n<tool_call>read {\"path\":\"a\"}</tool_call>\n<tool_call>read {\"path\":\"b\"}</tool_call>",
        ));
        assert_eq!(out.findings.len(), 1);
        assert_eq!(out.findings[0].kind, DetectorKind::DroppedToolCall);
        assert!(out.findings[0].hint.contains("未被解析执行"));
        assert!(out.turn_ended);
    }

    #[test]
    fn d1_silent_when_all_calls_executed() {
        let mut s = state();
        s.observe(&tool_use("read", "{\"path\":\"a\"}"));
        let out = s.observe(&role_turn(
            "<tool_call>read {\"path\":\"a\"}</tool_call>\n读完了。",
        ));
        assert!(out.findings.is_empty());
    }

    #[test]
    fn d1_silent_on_plain_text_turn() {
        let mut s = state();
        let out = s.observe(&role_turn("没有任何工具调用的回答。"));
        assert!(out.findings.is_empty());
        assert!(out.turn_ended);
    }

    // ── D2: invalid JSON args ──────────────────────────────────────

    #[test]
    fn d2_fires_on_non_json_args() {
        let mut s = state();
        let out = s.observe(&tool_use("read", "<path>src/main.rs</path>"));
        assert_eq!(out.findings.len(), 1);
        assert_eq!(out.findings[0].kind, DetectorKind::InvalidToolArgs);
    }

    #[test]
    fn d2_silent_on_valid_json_and_skips_sink_truncated_args() {
        let mut s = state();
        assert!(s.observe(&tool_use("read", "{\"path\":\"a\"}")).findings.is_empty());
        // Sink-truncated payload: invalid JSON but only because of the
        // truncation marker — must not fire.
        let truncated = format!("{}...[+42B]", "{\"path\":\"".to_string() + &"x".repeat(1_200));
        assert!(s.observe(&tool_use("read", &truncated)).findings.is_empty());
    }

    // ── D3: consecutive tool errors ────────────────────────────────

    #[test]
    fn d3_fires_on_two_consecutive_errors_and_dedups_within_turn() {
        let mut s = state();
        assert!(s.observe(&tool_error("exec", "boom1")).findings.is_empty());
        let out = s.observe(&tool_error("exec", "boom2"));
        assert_eq!(out.findings.len(), 1);
        assert_eq!(out.findings[0].kind, DetectorKind::ToolErrorStreak);
        // Third error in the same turn: no re-fire.
        assert!(s.observe(&tool_error("exec", "boom3")).findings.is_empty());
    }

    #[test]
    fn d3_resets_on_success_and_refires_next_turn() {
        let mut s = state();
        s.observe(&tool_error("exec", "boom1"));
        s.observe(&tool_result("exec", "ok"));
        // Streak broken: only one consecutive error now.
        assert!(s.observe(&tool_error("exec", "boom2")).findings.is_empty());
        // End the turn, then two fresh errors fire again.
        s.observe(&role_turn("done"));
        s.observe(&tool_error("exec", "boom3"));
        let out = s.observe(&tool_error("exec", "boom4"));
        assert_eq!(out.findings.len(), 1);
        assert_eq!(out.findings[0].kind, DetectorKind::ToolErrorStreak);
    }

    // ── D4: call loop ──────────────────────────────────────────────

    #[test]
    fn d4_fires_on_third_identical_call_and_dedups() {
        let mut s = state();
        assert!(s.observe(&tool_use("read", "{\"path\":\"a\"}")).findings.is_empty());
        assert!(s.observe(&tool_use("read", "{\"path\":\"a\"}")).findings.is_empty());
        let out = s.observe(&tool_use("read", "{\"path\":\"a\"}"));
        assert_eq!(out.findings.len(), 1);
        assert_eq!(out.findings[0].kind, DetectorKind::ToolCallLoop);
        // Fourth identical: no re-fire this turn.
        assert!(s.observe(&tool_use("read", "{\"path\":\"a\"}")).findings.is_empty());
    }

    #[test]
    fn d4_resets_on_different_call() {
        let mut s = state();
        s.observe(&tool_use("read", "{\"path\":\"a\"}"));
        s.observe(&tool_use("read", "{\"path\":\"a\"}"));
        s.observe(&tool_use("read", "{\"path\":\"b\"}"));
        assert!(s.observe(&tool_use("read", "{\"path\":\"a\"}")).findings.is_empty());
        assert!(s.observe(&tool_use("read", "{\"path\":\"a\"}")).findings.is_empty());
    }

    // ── role filtering / self-ignore ───────────────────────────────

    #[test]
    fn events_from_other_roles_and_advisor_are_ignored() {
        let mut s = state();
        // Another role's errors do not count toward D3.
        s.observe(&ChatEvent::ToolError {
            role_id: "programmer".into(),
            tool_name: "exec".into(),
            error: "x".into(),
            sub_id: None,
        });
        s.observe(&ChatEvent::ToolError {
            role_id: "advisor".into(),
            tool_name: "exec".into(),
            error: "y".into(),
            sub_id: None,
        });
        assert!(s.observe(&tool_error("exec", "z")).findings.is_empty());
        // Advisor's own bubble does not end/reset the turn or enter
        // the transcript.
        s.observe(&tool_use("read", "{\"path\":\"a\"}"));
        let out = s.observe(&ChatEvent::RoleTurn {
            role_id: "advisor".into(),
            content: "⚠️ warn：test".into(),
            is_complete: true,
            sub_id: None,
        });
        assert!(!out.turn_ended);
        assert!(out.findings.is_empty());
        assert!(!s.digest.render().contains("warn：test"));
    }

    #[test]
    fn specialist_turn_is_context_only_no_d1_no_turn_end() {
        let mut s = state();
        let out = s.observe(&ChatEvent::RoleTurn {
            role_id: "programmer".into(),
            content: "<tool_call>read {\"path\":\"x\"}</tool_call>".into(),
            is_complete: true,
            sub_id: Some("sub-1".into()),
        });
        assert!(!out.turn_ended);
        assert!(out.findings.is_empty());
        // 子会话的回复正文**不再**进摘要：路由摘要只记决策，产出内容由
        // 委派返回审查（build_delegate_review_prompt）单独把关。
        assert!(
            !s.digest.render().contains("[delegate turn"),
            "子会话正文不该出现在路由摘要里: {}",
            s.digest.render()
        );
    }

    #[test]
    fn error_rolefinished_ends_turn_without_d1() {
        let mut s = state();
        s.observe(&tool_use("read", "{\"path\":\"a\"}"));
        let out = s.observe(&ChatEvent::RoleFinished {
            role_id: "manager".into(),
            detail: "error: turn failed".into(),
            sub_id: None,
        });
        assert!(out.turn_ended);
        assert!(out.findings.is_empty());
        // State was reset: a fresh turn needs two errors for D3.
        s.observe(&tool_error("exec", "boom"));
        // 原始 tool_use 参数**不再**进摘要（它曾是「过程取证」的素材，也是
        // 误判的来源）；本测试只保证错误收尾路径确实结束了 turn。
        assert!(
            !s.digest.render().contains("[tool_use]"),
            "原始工具参数不该出现在路由摘要里: {}",
            s.digest.render()
        );
    }

    // ── verdict parsing ────────────────────────────────────────────

    #[test]
    fn parse_verdict_ok() {
        let v = parse_verdict("verdict: ok\nreason:\nhint:");
        assert_eq!(v.verdict, Verdict::Ok);
        assert!(v.reason.is_empty());
        assert!(v.hint.is_empty());
    }

    #[test]
    fn parse_verdict_warn_with_sections() {
        let v = parse_verdict(
            "verdict: warn\nreason: 工具连续两次失败\n仍在同一命令上重试\nhint: 先读报错再换路径",
        );
        assert_eq!(v.verdict, Verdict::Warn);
        assert_eq!(v.reason, "工具连续两次失败\n仍在同一命令上重试");
        assert_eq!(v.hint, "先读报错再换路径");
    }

    #[test]
    fn parse_verdict_intervene_bare_word_and_case_insensitive() {
        let v = parse_verdict("分析如下：\nINTERVENE\nreason: 确认幻觉");
        assert_eq!(v.verdict, Verdict::Intervene);
        assert_eq!(v.reason, "确认幻觉");
        let v2 = parse_verdict("Verdict: Warn\nReason: 偏离问题");
        assert_eq!(v2.verdict, Verdict::Warn);
        assert_eq!(v2.reason, "偏离问题");
    }

    #[test]
    fn parse_verdict_malformed_degrades_to_ok() {
        for raw in [
            "",
            "我觉得这个处理过程有点问题，但格式自由发挥",
            "verdict: maybe",
            ":::",
        ] {
            assert_eq!(
                parse_verdict(raw).verdict,
                Verdict::Ok,
                "malformed output must degrade to ok: {raw:?}"
            );
        }
    }

    #[test]
    fn parse_verdict_strips_think_block_drafts() {
        // 推理模型在 <think> 里打 verdict/reason 草稿并自我修正；
        // 解析必须只看 think 块之后的正式输出，否则草稿与犹豫过程
        // （"Actually let me think…"）会漏进用户可见的 reason。
        let raw = "<think>Let me analyze.\nverdict: intervene\nreason: 草稿理由\n\
                   Actually let me think more carefully. The transcript shows...\n</think>\n\
                   verdict: warn\nreason: 正式理由\nhint: 正式提示";
        let v = parse_verdict(raw);
        assert_eq!(v.verdict, Verdict::Warn);
        assert_eq!(v.reason, "正式理由");
        assert_eq!(v.hint, "正式提示");
        // 未闭合的 think 块（截断输出）剥到末尾，剩余部分照常解析。
        let v2 = parse_verdict("verdict: ok\n<think>verdict: intervene\nreason: 草稿");
        assert_eq!(v2.verdict, Verdict::Ok);
    }

    #[test]
    fn parse_verdict_remedy_defaults_patch_and_parses_restart() {
        // 不写 remedy → 默认 patch（续作优先）。
        let v = parse_verdict("verdict: intervene\nreason: 偏航\nhint: 重写");
        assert_eq!(v.remedy, Remedy::Patch);
        // restart 显式解析，容忍别名/中文。
        let v = parse_verdict("verdict: intervene\nreason: 虚构\nhint: 重来\nremedy: restart");
        assert_eq!(v.remedy, Remedy::Restart);
        let v = parse_verdict("verdict: intervene\nreason: x\nhint: y\nremedy: 重做");
        assert_eq!(v.remedy, Remedy::Restart);
        // remedy 行是独立键，不能吞掉后面的 hint 段。
        let v = parse_verdict("verdict: intervene\nreason: x\nremedy: restart\nhint: 保留我");
        assert_eq!(v.remedy, Remedy::Restart);
        assert_eq!(v.hint, "保留我");
        // patch 显式写出也按 patch。
        let v = parse_verdict("verdict: intervene\nreason: x\nhint: y\nremedy: patch");
        assert_eq!(v.remedy, Remedy::Patch);
    }

    // ── transcript rolling ─────────────────────────────────────────

    /// 核心回归：**省略必须可见。**
    ///
    /// 旧的 `Transcript` 超预算就丢最老的行且 `render()` 不留痕迹，advisor
    /// 拿到一份看起来完整、实则掐了头的记录，于是把「我没看到证据」当成
    /// 「有证据表明没发生」——实测会话里它据此断言
    /// 「记录中无成功读取 README，故引用为幻觉」，而那次读取真实发生过，
    /// 只是落在被丢弃的那一段里。
    #[test]
    fn digest_marks_dropped_lines_explicitly() {
        let mut d = DispatchDigest::default();
        for i in 0..(DIGEST_MAX_LINES + 5) {
            d.push(format!("[delegate → r{i:02}] task"));
        }
        let rendered = d.render();
        assert!(rendered.contains("已省略"), "必须显式标注省略: {rendered}");
        assert!(
            rendered.contains("不要因为某条动作不在下面就断言它没发生"),
            "必须提示 advisor 别把「没看到」当「没发生」: {rendered}"
        );
        assert!(rendered.contains("r44"), "最新的动作要保留");
        assert!(!rendered.contains("[delegate → r00]"), "最老的动作被丢弃");
        assert_eq!(d.dropped, 5, "丢弃计数要准确");
    }

    /// 未超限时不该出现省略提示（否则每次审查都白挂一段噪音）。
    #[test]
    fn digest_has_no_notice_when_within_limit() {
        let mut d = DispatchDigest::default();
        d.push("[delegate → programmer] 写代码".into());
        let rendered = d.render();
        assert!(!rendered.contains("已省略"), "未超限不该有省略提示: {rendered}");
        assert_eq!(d.dropped, 0);
    }

    /// 单条过长要截断（带 `[+NB]` 标记），避免一条动作吃掉整份摘要。
    #[test]
    fn digest_truncates_overlong_line_with_marker() {
        let mut d = DispatchDigest::default();
        d.push(format!("[delegate → programmer] {}", "任".repeat(2_000)));
        let rendered = d.render();
        assert!(rendered.contains("[+"), "单条截断要留标记: {}", &rendered[..80.min(rendered.len())]);
    }

    /// 惰性重置：标记后要等下一条事件才清空。
    ///
    /// 不能在 `reset_turn_state` 里直接清——EveryTurn 模式的审查发生在
    /// `observe()` 返回**之后**，那时 reset 已跑过，直接清会让该次审查拿到
    /// 空摘要（等于把刚结束那一轮的分派动作全藏起来）。
    #[test]
    fn digest_reset_is_lazy_until_next_push() {
        let mut d = DispatchDigest::default();
        d.push("[delegate → programmer] 本轮动作".into());
        d.mark_reset();
        assert!(
            d.render().contains("本轮动作"),
            "标记后、下一条事件前，本轮摘要必须仍可读"
        );
        d.push("[delegate → reviewer] 下一轮动作".into());
        let rendered = d.render();
        assert!(rendered.contains("下一轮动作"));
        assert!(!rendered.contains("本轮动作"), "下一轮首条事件才真正清空");
    }

    // ── monitor integration (wiremock advisor model) ───────────────

    fn openai_body(content: &str) -> String {
        serde_json::json!({
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
        })
        .to_string()
    }

    /// AgentConfig whose premium tier points at the given base_url,
    /// with no roles (engine falls back to the built-in advisor
    /// template).
    fn advisor_config_at(base_url: &str) -> Arc<AgentConfig> {
        Arc::new(AgentConfig {
            advisor: Default::default(),
            models: ModelCatalog {
                models: vec![ModelDef {
                    name: "Advisor Premium".into(),
                    api: "openai".into(),
                    provider: "test".into(),
                    base_url: base_url.into(),
                    api_key: "test-key".into(),
                    context_window: 32000,
                    max_tokens: 4096,
                    supports_thinking: false,
                    supports_vision: false,
                    omit_max_tokens: false,
                    max_tokens_field: Default::default(),
                    supports_image_generation: false,
                    cost_per_million_input: None,
                    cost_per_million_output: None,
                    tier: Some("premium".into()),
                    timeout_secs: None,
                }],
                tiers: None,
                role_tiers: None,
            },
            roles: Default::default(),
        })
    }

    fn engine_for(config: Arc<AgentConfig>) -> AdvisorReviewEngine {
        let resolver = Arc::new(ModelResolver::from_config(&config).unwrap());
        AdvisorReviewEngine::new(config, resolver, GenerateParams::default())
    }

    /// Poll the controller's hint queue until `pred` matches or the
    /// deadline passes; returns the queue snapshot on success.
    async fn wait_for_hints(
        controller: &ChatController,
        pred: impl Fn(&[String]) -> bool,
    ) -> Vec<String> {
        for _ in 0..100 {
            let snapshot: Vec<String> =
                controller.advisor_hint_queue().lock().iter().cloned().collect();
            if pred(&snapshot) {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timed out waiting for advisor hints");
    }

    #[tokio::test]
    async fn monitor_full_pipeline_hint_review_bubble() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                        "verdict: intervene\nreason: 工具连续失败，方向可疑\nhint: 停止重复调用，先读报错",
                    ))),
            )
            .await;

        let controller = Arc::new(ChatController::new(64));
        let mut bubble_rx = controller.subscribe();
        let engine = engine_for(advisor_config_at(&server.uri()));
        let _handle = AdvisorMonitor::spawn(
            controller.clone(),
            AdvisorMonitorConfig::default(),
            engine,
            "manager".into(),
        );

        controller.submit_input("帮我修 compile error").await;
        let tx = controller.event_sender();

        // D3: two consecutive tool errors.
        tx.send(tool_error("exec", "boom1")).unwrap();
        tx.send(tool_error("exec", "boom2")).unwrap();

        // 1. Deterministic hint injected immediately (channel A).
        let hints = wait_for_hints(&controller, |h| {
            h.iter().any(|x| x.contains("工具连续报错"))
        })
        .await;
        assert_eq!(hints.len(), 1, "only the deterministic hint so far: {hints:?}");

        // 2. LLM review ran (mock advisor hit once) and produced an
        //    intervene verdict → channel B bubble + corrective hint.
        let bubble = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match bubble_rx.recv().await {
                    Ok(ChatEvent::RoleTurn { role_id, content, .. }) if role_id == "advisor" => {
                        break content
                    }
                    Ok(_) => continue,
                    Err(e) => panic!("event stream ended before advisor bubble: {e}"),
                }
            }
        })
        .await
        .expect("advisor bubble should arrive");
        assert!(bubble.contains("🛑 intervene"), "bubble has verdict label: {bubble}");
        assert!(bubble.contains("工具连续失败"), "bubble carries reason: {bubble}");
        assert!(bubble.contains("停止重复调用"), "bubble quotes hint: {bubble}");

        let hints = wait_for_hints(&controller, |h| h.len() >= 2).await;
        assert!(
            hints.iter().any(|x| x.contains("停止重复调用，先读报错")),
            "corrective hint injected: {hints:?}"
        );

        // 3. The review request carried the trigger + transcript +
        //    user question.
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "exactly one review call: {}", requests.len());
        let body: serde_json::Value =
            serde_json::from_slice(&requests[0].body).expect("request body is json");
        let body_str = body.to_string();
        assert!(body_str.contains("D3"), "trigger evidence in prompt: {body_str}");
        assert!(body_str.contains("[tool_error] exec"), "transcript in prompt");
        assert!(body_str.contains("帮我修 compile error"), "user question in prompt");
    }

    #[tokio::test]
    async fn monitor_terminate_cancels_turn_and_broadcasts_event() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                        "verdict: terminate\nreason: 幻觉实锤，继续只会浪费\nhint:",
                    ))),
            )
            .await;

        let controller = Arc::new(ChatController::new(64));
        let mut bubble_rx = controller.subscribe();
        let engine = engine_for(advisor_config_at(&server.uri()));
        let _handle = AdvisorMonitor::spawn(
            controller.clone(),
            AdvisorMonitorConfig::default(),
            engine,
            "manager".into(),
        );
        let tx = controller.event_sender();

        // D3: two consecutive tool errors trigger the review.
        tx.send(tool_error("exec", "boom1")).unwrap();
        tx.send(tool_error("exec", "boom2")).unwrap();

        // 1. 🛑 terminate 气泡 + AdvisorTerminated 事件都广播出来
        //    （后者此前是死 variant，Terminate verdict 必须让它活过来）。
        tokio::time::timeout(Duration::from_secs(10), async {
            let (mut saw_bubble, mut saw_terminated) = (false, false);
            loop {
                match bubble_rx.recv().await {
                    Ok(ChatEvent::RoleTurn { role_id, content, .. }) if role_id == "advisor" => {
                        assert!(content.contains("🛑 terminate"), "label: {content}");
                        assert!(content.contains("幻觉实锤"), "reason in bubble: {content}");
                        saw_bubble = true;
                    }
                    Ok(ChatEvent::AdvisorTerminated { role_id, reason, detector, sub_id }) => {
                        assert_eq!(role_id, "manager", "terminated role is the watched role");
                        assert!(reason.contains("幻觉实锤"), "reason carried: {reason}");
                        assert_eq!(detector.as_deref(), Some("LLM"), "LLM review source");
                        assert!(sub_id.is_none());
                        saw_terminated = true;
                    }
                    Ok(_) => continue,
                    Err(e) => panic!("event stream ended before terminate events: {e}"),
                }
                if saw_bubble && saw_terminated {
                    break;
                }
            }
        })
        .await
        .expect("terminate bubble + AdvisorTerminated event should arrive");

        // 2. 取消通道被触发：turn_cancel_flag 置位（软终止——driver
        //    丢掉 in-flight turn 后回到等用户输入，不是 abort 整个
        //    session；controller 未 spawn 时 input 通道缺省，只有
        //    flag 这一侧生效）。
        for _ in 0..100 {
            if controller.turn_cancel_requested() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("turn_cancel_flag was not set by a Terminate verdict");
    }

    #[tokio::test]
    async fn monitor_intervene_corrects_without_pausing() {
        // v4 行为（用户拍板）：intervene = 气泡 + 注入纠正 hint，
        // 主会话继续跑——不置暂停门、不弹拍板窗、不发暂停 Status。
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                        "verdict: intervene\nreason: 方向可疑，继续只会浪费\nhint: 先停下来读报错",
                    ))),
            )
            .await;

        let controller = Arc::new(ChatController::new(64));
        let mut bubble_rx = controller.subscribe();
        let engine = engine_for(advisor_config_at(&server.uri()));
        let _handle = AdvisorMonitor::spawn(
            controller.clone(),
            AdvisorMonitorConfig::default(),
            engine,
            "manager".into(),
        );
        let tx = controller.event_sender();

        // D3: two consecutive tool errors trigger the review.
        tx.send(tool_error("exec", "boom1")).unwrap();
        tx.send(tool_error("exec", "boom2")).unwrap();

        // 1. intervene 气泡照常发出（含给 manager 的纠正提示）。
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match bubble_rx.recv().await {
                    Ok(ChatEvent::RoleTurn {
                        role_id, content, ..
                    }) if role_id == "advisor" => {
                        assert!(content.contains("intervene"), "bubble: {content}");
                        assert!(content.contains("方向可疑"), "reason in bubble");
                        assert!(content.contains("先停下来读报错"), "hint in bubble");
                        break;
                    }
                    Ok(_) => continue,
                    Err(e) => panic!("event stream ended before advisor bubble: {e}"),
                }
            }
        })
        .await
        .expect("advisor intervene bubble should arrive");

        // 2. 纠正 hint 已注入共享队列（manager 下个边界 drain 后自愈）。
        let queue = controller.advisor_hint_queue();
        assert!(
            queue.lock().iter().any(|h| h.contains("先停下来读报错")),
            "intervene hint must be injected into the shared queue"
        );

        // 3. 不暂停、不弹窗：气泡到达后再等 300ms，确认无
        //    ChoiceRequested / 暂停 Status，门始终未置位。
        let mut saw_choice = false;
        let mut saw_pause_status = false;
        let deadline = std::time::Instant::now() + Duration::from_millis(300);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(50), bubble_rx.recv()).await {
                Ok(Ok(ChatEvent::ChoiceRequested { .. })) => saw_choice = true,
                Ok(Ok(ChatEvent::Status { message })) if message.contains("等待用户拍板") => {
                    saw_pause_status = true;
                }
                _ => {}
            }
        }
        assert!(!saw_choice, "v4: intervene 不再弹拍板窗");
        assert!(!saw_pause_status, "v4: intervene 不再发暂停 Status");
        assert!(
            !controller.advisor_pause_requested(),
            "v4: intervene 不置暂停门（纠正并继续）"
        );
    }

    // ─── AdvisorPauseGate 单测 ─────────────────────────────────────

    #[tokio::test]
    async fn pause_gate_waits_until_resolved() {
        let gate = AdvisorPauseGate::with_timeout(Duration::from_secs(30));
        // 未置位：立即返回。
        gate.wait_if_requested().await;

        gate.request();
        let waiter = {
            let gate = gate.clone();
            tokio::spawn(async move { gate.wait_if_requested().await })
        };
        // 挂起中：resolve 前 waiter 不应完成。
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!waiter.is_finished(), "gate holds the runner suspended");
        gate.resolve();
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("waiter wakes on resolve")
            .unwrap();
        assert!(!gate.is_requested());
    }

    #[tokio::test]
    async fn pause_gate_timeout_auto_resumes() {
        let gate = AdvisorPauseGate::with_timeout(Duration::from_millis(50));
        gate.request();
        let start = std::time::Instant::now();
        gate.wait_if_requested().await;
        assert!(
            start.elapsed() >= Duration::from_millis(50),
            "waited out the timeout"
        );
        assert!(
            !gate.is_requested(),
            "timeout auto-resume clears the flag (防死锁)"
        );
    }

    #[tokio::test]
    async fn monitor_warn_bubbles_and_injects_hint() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                        "verdict: warn\nreason: 有点跑偏但可能自愈\nhint: 收敛范围，先交付最小可用版",
                    ))),
            )
            .await;

        let controller = Arc::new(ChatController::new(64));
        let mut bubble_rx = controller.subscribe();
        let engine = engine_for(advisor_config_at(&server.uri()));
        let _handle = AdvisorMonitor::spawn(
            controller.clone(),
            AdvisorMonitorConfig::default(),
            engine,
            "manager".into(),
        );
        let tx = controller.event_sender();

        tx.send(tool_error("exec", "boom1")).unwrap();
        tx.send(tool_error("exec", "boom2")).unwrap();

        // warn → bubble arrives, quoting the hint…
        let bubble = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match bubble_rx.recv().await {
                    Ok(ChatEvent::RoleTurn { role_id, content, .. }) if role_id == "advisor" => {
                        break content
                    }
                    Ok(_) => continue,
                    Err(e) => panic!("event stream ended before advisor bubble: {e}"),
                }
            }
        })
        .await
        .expect("advisor bubble should arrive");
        assert!(bubble.contains("⚠️ warn"), "label: {bubble}");
        assert!(bubble.contains("有点跑偏"), "reason: {bubble}");
        assert!(
            bubble.contains("收敛范围"),
            "warn bubble must quote the hint: {bubble}"
        );

        // …and the hint IS injected into the manager's hint queue
        // (实测事故：warn 只发气泡，「过度编排」警告 manager
        // 完全收不到)。队列里同时有确定性 D3 hint 和 warn hint。
        let hints = wait_for_hints(&controller, |h| {
            h.iter().any(|x| x.contains("收敛范围"))
        })
        .await;
        assert!(
            hints.iter().any(|x| x.contains("工具连续报错")),
            "deterministic D3 hint still injected: {hints:?}"
        );
        assert!(
            hints.iter().any(|x| x.contains("收敛范围")),
            "warn hint must be injected into the manager: {hints:?}"
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "review still ran"
        );
    }

    #[tokio::test]
    async fn monitor_degrades_silently_when_advisor_model_unavailable() {
        // Empty catalog → resolve_chain fails immediately.
        let empty = Arc::new(AgentConfig::default());
        let controller = Arc::new(ChatController::new(64));
        let mut bubble_rx = controller.subscribe();
        let engine = engine_for(empty);
        let _handle = AdvisorMonitor::spawn(
            controller.clone(),
            AdvisorMonitorConfig::default(),
            engine,
            "manager".into(),
        );
        let tx = controller.event_sender();

        // D3 fires → deterministic hint still delivered.
        tx.send(tool_error("exec", "boom1")).unwrap();
        tx.send(tool_error("exec", "boom2")).unwrap();
        let hints = wait_for_hints(&controller, |h| {
            h.iter().any(|x| x.contains("工具连续报错"))
        })
        .await;
        assert_eq!(hints.len(), 1);

        // No bubble, no corrective hint — review failed silently.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(controller.advisor_hint_queue().lock().len(), 1);
        match tokio::time::timeout(Duration::from_millis(100), bubble_rx.recv()).await {
            Err(_) => {} // timeout: nothing received — good
            Ok(Ok(ChatEvent::RoleTurn { role_id, .. })) if role_id == "advisor" => {
                panic!("no advisor bubble expected when the model is down")
            }
            _ => {}
        }

        // Monitor is still alive and keeps detecting: D4 next.
        for _ in 0..3 {
            tx.send(tool_use("read", "{\"path\":\"a\"}")).unwrap();
        }
        let hints = wait_for_hints(&controller, |h| {
            h.iter().any(|x| x.contains("调用循环"))
        })
        .await;
        assert_eq!(hints.len(), 2, "D3 + D4 deterministic hints: {hints:?}");
    }

    #[tokio::test]
    async fn monitor_review_cap_limits_calls_per_turn() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .set_body_string(openai_body("verdict: ok")),
                    ),
            )
            .await;

        let controller = Arc::new(ChatController::new(64));
        let engine = engine_for(advisor_config_at(&server.uri()));
        let _handle = AdvisorMonitor::spawn(
            controller.clone(),
            AdvisorMonitorConfig {
                enabled: true,
                review_mode: AdvisorReviewMode::OnAnomaly,
                max_reviews_per_turn: 1,
                review_window_secs: 120,
                watchdog_notes: false,
                gate: GateConfig::default(),
                review_settings: AdvisorReviewSettings::default(),
            },
            engine,
            "manager".into(),
        );
        let tx = controller.event_sender();

        // Two different detectors in one turn, cap = 1 → one review.
        tx.send(tool_error("exec", "boom1")).unwrap();
        tx.send(tool_error("exec", "boom2")).unwrap();
        tx.send(tool_use("read", "not json")).unwrap();
        // Wait for the two deterministic hints, then give the loop a
        // moment to (not) fire a second review.
        wait_for_hints(&controller, |h| h.len() >= 2).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let n = server.received_requests().await.unwrap().len();
        assert_eq!(n, 1, "review cap 1 per turn, got {n}");
    }

    // ── 监察笔记 (advisor-watchdog.md) ────────────────────────────

    /// Redirect LATTE_HOME to `dir` for the rest of the test,
    /// holding the process-wide env lock. Returns the guard +
    /// previous value; restore via `restore_latte_home`.
    fn set_latte_home(dir: &std::path::Path) -> (std::sync::MutexGuard<'static, ()>, Option<String>) {
        let guard = crate::test_util::ENV_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap();
        let prev = std::env::var("LATTE_HOME").ok();
        std::env::set_var("LATTE_HOME", dir);
        (guard, prev)
    }

    fn restore_latte_home(prev: Option<String>) {
        match prev {
            Some(v) => std::env::set_var("LATTE_HOME", v),
            None => std::env::remove_var("LATTE_HOME"),
        }
    }

    /// Drive a monitor with the given engine through a D3 trigger
    /// and return the body of the single review request.
    async fn run_one_review_request(
        engine: AdvisorReviewEngine,
        server: &wiremock::MockServer,
    ) -> String {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(
                        ResponseTemplate::new(200).set_body_string(openai_body("verdict: ok")),
                    ),
            )
            .await;
        let controller = Arc::new(ChatController::new(64));
        let _handle = AdvisorMonitor::spawn(
            controller.clone(),
            AdvisorMonitorConfig::default(),
            engine,
            "manager".into(),
        );
        let tx = controller.event_sender();
        tx.send(tool_error("exec", "boom1")).unwrap();
        tx.send(tool_error("exec", "boom2")).unwrap();
        for _ in 0..150 {
            if server.received_requests().await.map(|r| r.len()).unwrap_or(0) >= 1 {
                let reqs = server.received_requests().await.unwrap();
                return String::from_utf8_lossy(&reqs[0].body).to_string();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timed out waiting for the advisor review request");
    }

    #[tokio::test]
    async fn watchdog_project_notes_appear_in_review_prompt() {
        let proj = tempfile::tempdir().unwrap();
        let latte = proj.path().join(".latte");
        std::fs::create_dir_all(&latte).unwrap();
        std::fs::write(latte.join("advisor-watchdog.md"), "本项目禁止直接改生产 DB").unwrap();
        // Global layer empty → only the project note shows up.
        let global = tempfile::tempdir().unwrap();
        let (_guard, prev) = set_latte_home(global.path());

        let server = wiremock::MockServer::start().await;
        let engine = engine_for(advisor_config_at(&server.uri()))
            .with_watchdog_notes(proj.path().to_path_buf(), true);
        let body = run_one_review_request(engine, &server).await;

        assert!(body.contains("<attention>"), "attention block: {body}");
        assert!(body.contains("本项目禁止直接改生产 DB"), "note content: {body}");
        assert!(body.contains("特别关注以下本项目监察笔记"), "guidance line: {body}");

        restore_latte_home(prev);
    }

    #[tokio::test]
    async fn watchdog_project_and_global_notes_project_first() {
        let proj = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(proj.path().join(".latte")).unwrap();
        std::fs::write(proj.path().join(".latte").join("advisor-watchdog.md"), "PROJECT-NOTE-项目级").unwrap();
        let global = tempfile::tempdir().unwrap();
        std::fs::write(global.path().join("advisor-watchdog.md"), "GLOBAL-NOTE-全局级").unwrap();
        let (_guard, prev) = set_latte_home(global.path());

        let server = wiremock::MockServer::start().await;
        let engine = engine_for(advisor_config_at(&server.uri()))
            .with_watchdog_notes(proj.path().to_path_buf(), true);
        let body = run_one_review_request(engine, &server).await;

        let i_proj = body.find("PROJECT-NOTE-项目级").expect("project note present");
        let i_glob = body.find("GLOBAL-NOTE-全局级").expect("global note present");
        assert!(i_proj < i_glob, "project note before global note: {body}");

        restore_latte_home(prev);
    }

    #[tokio::test]
    async fn watchdog_missing_files_mean_no_attention_block() {
        let proj = tempfile::tempdir().unwrap(); // no .latte inside
        let global = tempfile::tempdir().unwrap(); // empty global
        let (_guard, prev) = set_latte_home(global.path());

        let server = wiremock::MockServer::start().await;
        let engine = engine_for(advisor_config_at(&server.uri()))
            .with_watchdog_notes(proj.path().to_path_buf(), true);
        let body = run_one_review_request(engine, &server).await;

        assert!(!body.contains("<attention>"), "no attention block: {body}");
        // The review itself still works normally.
        assert!(body.contains("监察审查任务"), "base prompt intact: {body}");

        restore_latte_home(prev);
    }

    #[tokio::test]
    async fn watchdog_disabled_switch_skips_reading() {
        let proj = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(proj.path().join(".latte")).unwrap();
        std::fs::write(proj.path().join(".latte").join("advisor-watchdog.md"), "PROJECT-NOTE-项目级").unwrap();
        let global = tempfile::tempdir().unwrap();
        let (_guard, prev) = set_latte_home(global.path());

        let server = wiremock::MockServer::start().await;
        let engine = engine_for(advisor_config_at(&server.uri()))
            .with_watchdog_notes(proj.path().to_path_buf(), false);
        let body = run_one_review_request(engine, &server).await;

        assert!(!body.contains("<attention>"), "switch off → no block: {body}");
        assert!(!body.contains("PROJECT-NOTE"), "note not read: {body}");

        restore_latte_home(prev);
    }

    // ─── D5 / D6 Pre-persistence gate tests ──────────────────────
    // 核心场景：之前 53 chars 的 `<read><path>deps/xredis-gtid/Cargo.toml</path></read>`
    // 伪 tool call 必须被 D5 + D6 同时命中，强制重做 turn。

    #[test]
    fn gate_catches_53char_broken_response() {
        // 复现 某次实测日志 L7 的 programmer 输出。
        // 53 chars ≥ 默认 D5 阈值 50，所以 D5 不撞；D6 命中
        // （<read> + tool_use_count=0）。
        let response = "<read><path>deps/xredis-gtid/Cargo.toml</path></read>";
        assert_eq!(response.len(), 53);
        let cfg = GateConfig::default();
        let verdict = check_response_gates(response, 0, &cfg);
        assert!(
            matches!(verdict, GateVerdict::Fail { detector: DetectorKind::ToolCallEcho, .. }),
            "expected D6 ToolCallEcho fail, got {verdict:?}"
        );
    }
    #[test]
    fn gate_d6_passes_when_tool_was_actually_executed() {
         // 同样的 response，但 tool_use_count=1（工具真执行了，response
        let response = "Here is the result based on the read tool I just called: \
                        <read><path>foo.rs</path></read>";
        let cfg = GateConfig::default();
        let verdict = check_response_gates(response, 1, &cfg);
        assert!(matches!(verdict, GateVerdict::Pass), "expected Pass, got {verdict:?}");
    }

    #[test]
    fn gate_d5_allows_short_ack() {
        for ack in &["好的", "收到", "ok", "OK", "确认", "done", "完成"] {
            let verdict = check_response_gates(ack, 0, &GateConfig::default());
            assert!(
                matches!(verdict, GateVerdict::Pass),
                "ack `{ack}` should Pass D5, got {verdict:?}"
            );
        }
    }

    #[test]
    fn gate_d5_rejects_non_ack_short_response() {
        let response = "TODO"; // 4 chars, not an ack
        let verdict = check_response_gates(response, 0, &GateConfig::default());
        assert!(
            matches!(verdict, GateVerdict::Fail { detector: DetectorKind::ShortOutput, .. }),
            "expected D5, got {verdict:?}"
        );
    }

    #[test]
    fn gate_passes_normal_long_response() {
        let response = "This is a perfectly normal response that explains things in detail \
                        and has way more than the 50 character threshold. It contains real \
                        content, not just tool call syntax.";
        let verdict = check_response_gates(response, 0, &GateConfig::default());
        assert!(matches!(verdict, GateVerdict::Pass), "expected Pass, got {verdict:?}");
    }

    #[test]
    fn gate_d6_hint_mentions_tool_call_format() {
        // D6 hit 时 hint 必须明确告诉模型"用 <tool_call>NAME {json}</tool_call>"
        // 否则下次重做还是错的格式
        let response = "<read><path>foo.rs</path></read> padding padding padding padding padding";
        let cfg = GateConfig::default();
        let verdict = check_response_gates(response, 0, &cfg);
        match verdict {
            GateVerdict::Fail { hint, .. } => {
                assert!(hint.contains("tool_call"), "hint must mention <tool_call> format: {hint}");
                assert!(hint.contains("D6"), "hint should label the detector: {hint}");
            }
            other => panic!("expected Fail, got {other:?}"),
        }
    }

    /// 记录 D5 的长度语义是**字节**，且这是有意选择（不是笔误）。
    ///
    /// UTF-8 字节数是跨语言「信息量」的粗略代理：阈值 50 按英文校准
    /// （chars≈bytes），50 字节 ≈17 个汉字，同属「说了句实在话」的量级。
    /// 曾尝试改成按字符计，结果把中文门槛收紧 3 倍（需凑满 50 汉字才合格），
    /// 9 个既有 advisor 用例从放行翻转为判死。故保持字节语义，并让提示
    /// 文案如实说「字节」而不是「字符」。
    #[test]
    fn gate_d5_length_is_measured_in_bytes_by_design() {
        let cfg = GateConfig::default();
        // 20 个汉字 = 60 字节 > 阈值 → 放行（若按字符计只有 20，会被误杀）。
        let twenty_cjk = "这段回答一共二十个汉字长度足够表达一个结论";
        assert!(
            twenty_cjk.chars().count() < cfg.short_output_threshold,
            "前提：字符数不足阈值"
        );
        assert!(twenty_cjk.len() > cfg.short_output_threshold, "前提：字节数超阈值");
        assert!(
            matches!(check_response_gates(twenty_cjk, 0, &cfg), GateVerdict::Pass),
            "合格的中文短答复必须放行（按字节计），实际 {:?}",
            check_response_gates(twenty_cjk, 0, &cfg)
        );
        // 提示文案要如实标单位，别把字节报成字符。
        match check_response_gates("TODO", 0, &cfg) {
            GateVerdict::Fail { hint, evidence, .. } => {
                assert!(hint.contains("字节"), "hint 应如实说字节: {hint}");
                assert!(evidence.contains("bytes"), "evidence 应标明单位: {evidence}");
            }
            other => panic!("expected Fail, got {other:?}"),
        }
    }

    /// 回归（与 P0 修复互锁）：契约要求的极简裁决不得被 D5 判死。
    ///
    /// `task_refine` 的 gate 步契约是「严格只输出三种裁决之一」+
    /// `output_contract.require = ["VERDICT:"]`。模型照做且写得简洁时产出
    /// 可能只有 15 字符；D5 若判死它，重试 3 次后 advisor terminated →
    /// 整条 workflow 失败 → `submit` 执行不到 → `plan` 不被调用 →
    /// 「添加子任务」弹窗消失，正是要修的症状。
    #[test]
    fn gate_d5_exempts_contract_required_verdicts() {
        let cfg = GateConfig::default();
        for verdict_text in &[
            "VERDICT: ACCEPT",
            "VERDICT: ACCEPT 无阻断问题",
            "VERDICT: REVISE",
            "VERDICT: REJECT",
            "裁决：通过",
        ] {
            assert!(
                verdict_text.chars().count() < cfg.short_output_threshold,
                "前提：{verdict_text} 确实短于阈值"
            );
            assert!(
                matches!(check_response_gates(verdict_text, 0, &cfg), GateVerdict::Pass),
                "契约要求的裁决 `{verdict_text}` 必须放行，实际 {:?}",
                check_response_gates(verdict_text, 0, &cfg)
            );
        }
    }

    /// 豁免只认「以裁决标记开头」，不给「东拉西扯里恰好提到 VERDICT」开后门。
    #[test]
    fn gate_d5_verdict_exemption_requires_leading_marker() {
        let cfg = GateConfig::default();
        let sneaky = "我还没想好 VERDICT:";
        assert!(
            matches!(
                check_response_gates(sneaky, 0, &cfg),
                GateVerdict::Fail { detector: DetectorKind::ShortOutput, .. }
            ),
            "非开头的 VERDICT 不应豁免"
        );
    }

    #[test]
    fn gate_default_config_threshold_is_50() {
        // 默认阈值 50 来自设计：53 chars 的 broken response 刚好被覆盖
        let cfg = GateConfig::default();
        assert_eq!(cfg.short_output_threshold, 50);
        assert_eq!(cfg.max_retries, 2);
        assert!(cfg.tool_call_echo_patterns.contains(&"<read>".to_string()));
    }
}
