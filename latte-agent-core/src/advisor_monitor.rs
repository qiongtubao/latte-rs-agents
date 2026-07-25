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

use std::collections::{HashSet, VecDeque};
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

/// Hard budget for the rolling transcript fed to the LLM review.
/// ~6K tokens at the 4-chars-per-token heuristic used in agent.rs.
const MAX_TRANSCRIPT_CHARS: usize = 24_000;
/// Cap on any single transcript entry (a full RoleTurn can be huge).
const MAX_ENTRY_CHARS: usize = 6_000;
/// Wall-clock budget for one advisor review call. Bounds the stall the
/// monitor loop experiences when the advisor model is slow; the main
/// session is unaffected either way.
const REVIEW_TIMEOUT_SECS: u64 = 120;

// ─── Configuration ─────────────────────────────────────────────────

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
    /// Circuit breaker: max LLM reviews per turn (cost control for
    /// anomaly storms). Default 2.
    pub max_reviews_per_turn: u32,
    /// Read project/global 监察笔记 (`advisor-watchdog.md`) into the
    /// review prompt. Default true.
    pub watchdog_notes: bool,
}

impl Default for AdvisorMonitorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            review_mode: AdvisorReviewMode::OnAnomaly,
            max_reviews_per_turn: 2,
            watchdog_notes: true,
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
    /// D3: ≥2 consecutive ToolError events.
    ToolErrorStreak,
    /// D4: same (tool_name, args) call ≥3 times in a row.
    ToolCallLoop,
}

impl DetectorKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::DroppedToolCall => "D1",
            Self::InvalidToolArgs => "D2",
            Self::ToolErrorStreak => "D3",
            Self::ToolCallLoop => "D4",
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

/// Char-boundary-safe truncation with the same marker format the
/// controller's event sink uses.
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

// ─── Rolling transcript ────────────────────────────────────────────

/// Rolling per-turn transcript fed to the LLM review. Oldest lines
/// are dropped once the char budget is exceeded (recent context is
/// the most diagnostic).
struct Transcript {
    lines: VecDeque<String>,
    chars: usize,
    max_chars: usize,
}

impl Transcript {
    fn new(max_chars: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            chars: 0,
            max_chars,
        }
    }

    fn push(&mut self, line: String) {
        self.chars += line.len() + 1; // + '\n'
        self.lines.push_back(line);
        while self.chars > self.max_chars {
            if let Some(old) = self.lines.pop_front() {
                self.chars -= old.len() + 1;
            } else {
                break;
            }
        }
    }

    fn render(&self) -> String {
        self.lines
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

// ─── Review verdict parsing ────────────────────────────────────────

/// Three-state review verdict (design §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    Warn,
    Intervene,
}

/// Parsed advisor review output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewVerdict {
    pub verdict: Verdict,
    pub reason: String,
    pub hint: String,
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
        _ => None,
    }
}

/// Parse the advisor's review output. Expected shape:
///
/// ```text
/// verdict: ok | warn | intervene
/// reason: <user-visible justification>
/// hint: <one-line correction for the manager>
/// ```
///
/// Tolerates a bare verdict word on its own line and multi-line
/// reason/hint sections. Malformed output degrades to `Ok`
/// (no action — an unreadable review must not inject noise).
pub fn parse_verdict(raw: &str) -> ReviewVerdict {
    let mut verdict: Option<Verdict> = None;
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
        }
    }

    /// Point the engine at the session workspace for project-level
    /// 监察笔记 (`<cwd>/.latte/advisor-watchdog.md`) and set the
    /// master switch (from `AdvisorMonitorConfig::watchdog_notes`).
    pub fn with_watchdog_notes(mut self, session_cwd: PathBuf, enabled: bool) -> Self {
        self.session_cwd = Some(session_cwd);
        self.watchdog_notes = enabled;
        self
    }

    /// Run one review. `Err` means the advisor could not be reached
    /// or built — callers degrade silently (tracing::warn).
    pub async fn review(
        &self,
        user_question: &str,
        transcript: &str,
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
        let user = Message::user(build_review_prompt(user_question, transcript, trigger, &notes));
        let completion = agent
            .chat(&[sys, user], None, WaitPolicy::NoWait)
            .await?;
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
    transcript: &str,
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

你正以**监察者**身份旁路审查 manager（主会话 agent）刚刚的处理过程。下面是用户当前问题与 manager 本 turn 的事件 transcript。

逐项检查：
1. **幻觉**：声称读过没读过的文件、编造路径或结论、结论与工具返回的证据矛盾。
2. **工具误用**：调用格式错误、参数不合法、无意义的重复调用。
3. **思路跑偏**：偏离用户问题、在错误方向上持续投入。
4. **异常循环**：同一失败模式反复重试。
{attention_block}
# 触发原因

{trigger}

# 用户当前问题

{user_question}

# manager 本 turn transcript（滚动截断到最近 ~6K tokens）

{transcript}

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

// ─── Monitor state machine ─────────────────────────────────────────

/// Outcome of feeding one event to the monitor state.
struct StepOutcome {
    findings: Vec<Finding>,
    turn_ended: bool,
}

struct MonitorState {
    watched_role: String,
    transcript: Transcript,
    detectors: TurnDetectors,
    reviews_this_turn: u32,
}

impl MonitorState {
    fn new(watched_role: String) -> Self {
        Self {
            watched_role,
            transcript: Transcript::new(MAX_TRANSCRIPT_CHARS),
            detectors: TurnDetectors::default(),
            reviews_this_turn: 0,
        }
    }

    fn observe(&mut self, ev: &ChatEvent) -> StepOutcome {
        let mut findings = Vec::new();
        let mut turn_ended = false;
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
                    self.transcript.push(format!(
                        "[delegate turn {role_id}] {}",
                        truncate_chars(content, 2_000)
                    ));
                } else if role_id == &self.watched_role {
                    self.transcript.push(format!(
                        "[assistant] {}",
                        truncate_chars(content, MAX_ENTRY_CHARS)
                    ));
                    // D1 runs at turn end, before the per-turn reset.
                    if let Some(f) = self.detectors.finish_turn(content) {
                        findings.push(f);
                    }
                    self.detectors.reset();
                    self.reviews_this_turn = 0;
                    turn_ended = true;
                }
            }
            ChatEvent::ToolUse {
                role_id,
                tool_name,
                args,
            } if role_id == &self.watched_role => {
                self.transcript.push(format!("[tool_use] {tool_name} {args}"));
                findings.extend(self.detectors.observe_tool_use(tool_name, args));
            }
            ChatEvent::ToolResult {
                role_id,
                tool_name,
                result,
            } if role_id == &self.watched_role => {
                self.transcript.push(format!(
                    "[tool_result] {tool_name} → {}",
                    truncate_chars(result, 1_500)
                ));
                self.detectors.observe_tool_result();
            }
            ChatEvent::ToolError {
                role_id,
                tool_name,
                error,
            } if role_id == &self.watched_role => {
                self.transcript
                    .push(format!("[tool_error] {tool_name} → {error}"));
                findings.extend(self.detectors.observe_tool_error(tool_name, error));
            }
            ChatEvent::DelegateStarted {
                from_role,
                to_role,
                task,
                ..
            } if from_role == &self.watched_role => {
                self.transcript.push(format!(
                    "[delegate → {to_role}] {}",
                    truncate_chars(task, 500)
                ));
            }
            ChatEvent::DelegateFinished {
                from_role,
                to_role,
                status,
                summary,
                ..
            } if from_role == &self.watched_role => {
                self.transcript.push(format!(
                    "[delegate {to_role} {status}] {}",
                    truncate_chars(summary, 1_500)
                ));
            }
            ChatEvent::RoleFinished { role_id, detail }
                if role_id == &self.watched_role =>
            {
                // Turn ended without a RoleTurn (error/timeout path):
                // reset per-turn state. D1 has no final text to
                // reconcile, so nothing fires here.
                if detail.contains("error") || detail.contains("timeout") {
                    self.detectors.reset();
                    self.reviews_this_turn = 0;
                    turn_ended = true;
                }
            }
            _ => {}
        }
        StepOutcome {
            findings,
            turn_ended,
        }
    }
}

// ─── Monitor task ──────────────────────────────────────────────────

/// The advisor monitor. Spawned per session by the session creator
/// (ui-server's `create_session_handle`); exits on `ChatEvent::Done`
/// or when the broadcast channel closes (controller dropped).
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
                    AdvisorReviewMode::OnAnomaly => !outcome.findings.is_empty(),
                    AdvisorReviewMode::EveryTurn => outcome.turn_ended,
                };
                if !should_review {
                    continue;
                }
                if state.reviews_this_turn >= config.max_reviews_per_turn {
                    tracing::warn!(
                        "advisor monitor: review cap ({}/turn) reached; skipping",
                        config.max_reviews_per_turn
                    );
                    continue;
                }
                state.reviews_this_turn += 1;

                let trigger = if outcome.findings.is_empty() {
                    "例行审查（every_turn 模式）：本 turn 未命中确定性检测器".to_string()
                } else {
                    let mut s = "命中确定性检测器：".to_string();
                    for f in &outcome.findings {
                        s.push_str(&format!("\n- {} {:?}：{}", f.kind.label(), f.kind, f.evidence));
                    }
                    s
                };
                let question = controller.last_user_input();
                let transcript = state.transcript.render();

                let review = tokio::time::timeout(
                    Duration::from_secs(REVIEW_TIMEOUT_SECS),
                    review_engine.review(&question, &transcript, &trigger),
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
                    Verdict::Ok => unreachable!(),
                };
                let reason = if verdict.reason.is_empty() {
                    "advisor 检测到处理过程异常，建议关注。".to_string()
                } else {
                    verdict.reason.clone()
                };
                let mut bubble = format!("{label}：{reason}");
                // Channel A is intervene-only: warn is a light touch
                // (the user judges), intervene actively corrects the
                // manager mid-loop.
                if verdict.verdict == Verdict::Intervene && !verdict.hint.is_empty() {
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
        }
    }

    fn tool_result(name: &str, result: &str) -> ChatEvent {
        ChatEvent::ToolResult {
            role_id: "manager".into(),
            tool_name: name.into(),
            result: result.into(),
        }
    }

    fn tool_error(name: &str, error: &str) -> ChatEvent {
        ChatEvent::ToolError {
            role_id: "manager".into(),
            tool_name: name.into(),
            error: error.into(),
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
        });
        s.observe(&ChatEvent::ToolError {
            role_id: "advisor".into(),
            tool_name: "exec".into(),
            error: "y".into(),
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
        assert!(!s.transcript.render().contains("warn：test"));
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
        assert!(s.transcript.render().contains("[delegate turn programmer]"));
    }

    #[test]
    fn error_rolefinished_ends_turn_without_d1() {
        let mut s = state();
        s.observe(&tool_use("read", "{\"path\":\"a\"}"));
        let out = s.observe(&ChatEvent::RoleFinished {
            role_id: "manager".into(),
            detail: "error: turn failed".into(),
        });
        assert!(out.turn_ended);
        assert!(out.findings.is_empty());
        // State was reset: a fresh turn needs two errors for D3.
        s.observe(&tool_error("exec", "boom"));
        assert!(s.transcript.render().contains("[tool_use]"));
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

    // ── transcript rolling ─────────────────────────────────────────

    #[test]
    fn transcript_drops_oldest_lines_beyond_budget() {
        let mut t = Transcript::new(100);
        for i in 0..20 {
            t.push(format!("line-{i:02}-{}", "x".repeat(20)));
        }
        let rendered = t.render();
        assert!(rendered.len() <= 100 + 30, "budget respected: {}", rendered.len());
        assert!(rendered.contains("line-19"), "tail kept");
        assert!(!rendered.contains("line-00"), "head dropped");
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
    async fn monitor_warn_bubbles_without_hint_injection() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};

        let server = wiremock::MockServer::start().await;
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/chat/completions"))
                    .respond_with(ResponseTemplate::new(200).set_body_string(openai_body(
                        "verdict: warn\nreason: 有点跑偏但可能自愈\nhint: 这句话绝不能进 manager 上下文",
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

        // warn → bubble arrives…
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
        // …but the hint is NOT quoted in the bubble…
        assert!(
            !bubble.contains("绝不能进"),
            "warn bubble must not quote the hint: {bubble}"
        );

        // …and NOT injected: the queue holds only the deterministic
        // D3 hint, even after the review completed.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let hints: Vec<String> = controller.advisor_hint_queue().lock().iter().cloned().collect();
        assert_eq!(hints.len(), 1, "only the deterministic D3 hint: {hints:?}");
        assert!(hints[0].contains("工具连续报错"));
        assert!(
            !hints.iter().any(|x| x.contains("绝不能进")),
            "warn must not inject into the manager: {hints:?}"
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
                watchdog_notes: false,
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
}
