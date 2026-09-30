//! Goal 层外循环：一个跨 turn 的**完成契约**。
//!
//! # 为什么需要这一层
//!
//! 在此之前，latte 的完成契约只活在**单次 workflow run** 里。manager 一旦
//! 返回纯文本回复，turn 就结束、session 转 idle 等人——没有任何机制判断
//! 「用户要的那件事到底做完了没有」。两个现成的东西都替代不了它：
//!
//! - **自动善后**（`LATTE_AGENT_AUTO_REMEDIATE_MAX`）是**失败驱动**的，只在
//!   workflow 返回 `Err` 时合成 followup。workflow 报 `ok` 但产出其实很浅
//!   时，一次都不会触发。
//! - **todo stop-reminder**（`crate::todo`）是 **turn 内**的，
//!   `stop_reminders_sent` 每 turn 清零、上限 3，跨不过 turn 边界。
//!
//! 于是失败形状是：workflow 以 `ok` 收尾、summary 却是 REJECT 原文，
//! 没人重新驱动，任务静默欠交。这正是 `WorkflowStepDef::loop_no_release_on`
//! 那条注释记下的事故（「失败被吞成成功，三道带证据的阻断意见被一次复核
//! 否决」）——当时只能靠 advisor 那一次复核赌对。
//!
//! # 设计取自哪里
//!
//! 结构照 deepseek-harness 的 `packages/goal/`：**「目标该不该继续」（持久的
//! [`GoalPhase`]）与「此刻能不能自动继续」（易失的 [`GoalActivation`]）严格
//! 正交**。续跑 prompt 的措辞照 oh-my-pi 的 `goal-continuation.md`（那 6 条
//! 完成审计规定，见 `prompts/goal_continuation.md`）。
//!
//! 三个参考实现的一个共同点值得写下来：**没有一家在引擎里验证「目标是否
//! 达成」**。oh-my-pi 的 `completeGoalFromTool()` 只校验状态机合法性，不查
//! todo、不查 git、不跑测试；deepseek-harness 的 README 写明 "No independent
//! evaluator — the caller that records completion or blocking is
//! authoritative"。本模块同样不做语义裁决：它只保证**流程**正确（谁能结单、
//! 轮次有上限、预算耗尽不算完成），「做完了没有」交给带证据的显式调用。
//!
//! latte 在这件事上其实比三家都强：step 层已经有一整套机械门禁
//! （`require_tools_any` / `require_symbols_resolvable` /
//! `require_citations_observed` / `require_plan_submit`）。缺的从来不是校验
//! 能力，而是一个把这些门禁挂上去的跨 turn 契约载体。

use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// 目标的持久相位。
///
/// 四个相位足够覆盖全部终局，不必为每种失败增生相位——**所有非正常终止
/// 都归到 [`GoalPhase::Blocked`]，靠 [`GoalBlockReason::code`] 区分**
/// （`round-limit` / `token-budget` / 将来的 `provider-quota` …）。这是
/// deepseek-harness `blockedReason.code` 的设计意图。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GoalPhase {
    /// 正在推进：唯一允许自动续跑的相位。
    Active,
    /// 用户主动暂停。
    Paused,
    /// 撞上某条硬边界（轮次/预算/外部限额），需要人介入才能继续。
    Blocked,
    /// 已达成。只能由带证据的显式调用置入，见 [`GoalState::complete`]。
    Complete,
}

impl GoalPhase {
    /// 相位的稳定字符串标识（事件/API 用）。
    pub fn as_str(self) -> &'static str {
        match self {
            GoalPhase::Active => "active",
            GoalPhase::Paused => "paused",
            GoalPhase::Blocked => "blocked",
            GoalPhase::Complete => "complete",
        }
    }
}

/// 阻塞原因。`code` 是**稳定标识**（给 UI/自动化判据用），`message` 面向人。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalBlockReason {
    pub code: String,
    pub message: String,
}

impl GoalBlockReason {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// 轮次耗尽的稳定 code。
pub const BLOCK_CODE_ROUND_LIMIT: &str = "round-limit";
/// token 预算耗尽的稳定 code。
pub const BLOCK_CODE_TOKEN_BUDGET: &str = "token-budget";

/// 目标身份 + 修订号，做 CAS（compare-and-swap）。
///
/// 为什么需要：`complete` / `block` 这些 mutation 可能来自**陈旧的**上下文
/// ——模型在第 3 轮拿到 goal 快照，第 5 轮才调工具结单。带 revision 校验能
/// 让陈旧调用响亮失败，而不是把一个已经变过的目标改错。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalRef {
    pub id: String,
    pub revision: u64,
}

/// 目标快照。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    pub id: String,
    /// 每次 mutation +1。见 [`GoalRef`]。
    pub revision: u64,
    /// 用户的原始诉求，**跨 turn 不变**。
    pub objective: String,
    pub phase: GoalPhase,
    /// 仅 `phase == Blocked` 时存在。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<GoalBlockReason>,
    /// 自动续跑的轮次上限。这是**唯一始终生效的 runaway 兜底**。
    pub max_rounds: u32,
    /// 已经**真正开出去**的续跑轮数（不含被拒的预留）。
    pub rounds_started: u32,
    /// token 预算。`None` = 不限（默认，与 oh-my-pi 的
    /// `tokenBudget?: number` 一致）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
    pub tokens_used: u64,
}

impl Goal {
    /// 本目标的 CAS 句柄。
    pub fn goal_ref(&self) -> GoalRef {
        GoalRef {
            id: self.id.clone(),
            revision: self.revision,
        }
    }

    /// 剩余轮次。
    pub fn rounds_remaining(&self) -> u32 {
        self.max_rounds.saturating_sub(self.rounds_started)
    }

    /// 剩余 token 预算；`None` = 不限。
    pub fn tokens_remaining(&self) -> Option<u64> {
        self.token_budget
            .map(|b| b.saturating_sub(self.tokens_used))
    }
}

/// **进程本地**的续跑许可，与 [`GoalPhase`] 正交，**从不持久化**。
///
/// 为什么必须分开这两个维度（照 deepseek-harness）：phase 回答「这个目标
/// 该不该继续」，activation 回答「本进程此刻可不可以**自动**继续」。合成
/// 一个维度就无法表达「目标还在、但重启后先别自己跑」。
///
/// 硬规则只有一条：**新建 / 载入 / 重启一律 `Disarmed`，只有显式
/// `create` 或 `resume` 才 `Armed`**。见 [`GoalState::load_disarmed`]。
///
/// 这不是洁癖。README 里「服务启动时先对账……对账刻意不自动重派——重启
/// 就重跑半成品会在用户不在场时烧掉大量 token」讲的是同一件事，这里把它
/// 上升成 goal 层的通用安全阀。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalActivation {
    Armed,
    Disarmed,
}

/// 状态机的错误。全部是**流程**错误，不含语义裁决。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalError {
    /// 当前没有目标。
    NoGoal,
    /// CAS 失败：传入的 ref 不是当前目标。
    StaleRef { expected: GoalRef, got: GoalRef },
    /// 相位不允许这个转换。
    InvalidTransition { from: GoalPhase, op: &'static str },
    /// 已有一个未完成的目标，不能再建。
    AlreadyActive { objective: String },
    /// 轮次预算已耗尽，`resume` 前必须先抬高 `max_rounds`。
    RoundsExhausted {
        rounds_started: u32,
        max_rounds: u32,
    },
    /// token 预算已耗尽，`resume` 前必须先抬高或清除预算。
    TokensExhausted { tokens_used: u64, token_budget: u64 },
    /// objective 为空。
    EmptyObjective,
}

impl std::fmt::Display for GoalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GoalError::NoGoal => write!(f, "当前没有活动目标（先用 goal.create 设定一个）"),
            GoalError::StaleRef { expected, got } => write!(
                f,
                "陈旧的 goal 句柄：给的是 \"{}\" rev {}，当前是 \"{}\" rev {}——先 goal.get 读最新状态再重试",
                got.id, got.revision, expected.id, expected.revision
            ),
            GoalError::InvalidTransition { from, op } => write!(
                f,
                "目标当前是 {}，不允许 {} 转换",
                from.as_str(),
                op
            ),
            GoalError::AlreadyActive { objective } => write!(
                f,
                "已有一个未完成的目标（{objective}）——先 complete / drop 它，或用 replace 覆盖"
            ),
            GoalError::RoundsExhausted {
                rounds_started,
                max_rounds,
            } => write!(
                f,
                "目标已用尽 {max_rounds} 轮（已开 {rounds_started} 轮）——resume 前请先抬高 max_rounds"
            ),
            GoalError::TokensExhausted {
                tokens_used,
                token_budget,
            } => write!(
                f,
                "目标已用尽 token 预算（{tokens_used} / {token_budget}）——resume 前请先抬高或清除预算"
            ),
            GoalError::EmptyObjective => write!(f, "objective 不能为空"),
        }
    }
}

impl std::error::Error for GoalError {}

/// 轮次上限的内置默认值。
///
/// 取 deepseek-harness 的 `defaultMaxGoalRounds`（256）同一量级但保守得多：
/// latte 的一"轮"往往是一整条 workflow（内部还能嵌套 3 层、每层
/// `max_iterations` 最多 10），单轮成本比 dsh 的一个 turn 高一到两个数量级。
/// 256 轮在这里等于没有上限。
pub const DEFAULT_MAX_GOAL_ROUNDS: u32 = 24;

/// 轮次上限的硬顶。配再大也钳到这里——这是 runaway 兜底，不是配置项。
pub const MAX_GOAL_ROUNDS_CEILING: u32 = 256;

/// 新建目标时的默认轮次上限。env `LATTE_AGENT_GOAL_MAX_ROUNDS` 覆盖，
/// 钳在 `1..=`[`MAX_GOAL_ROUNDS_CEILING`]。
///
/// 与项目里其他上限一致地提供 env 开关：一台机器上跑的活规模差异很大，
/// 而每次 `goal set` 都手写 `max_rounds` 不现实。显式传参仍然优先。
pub fn default_max_goal_rounds() -> u32 {
    std::env::var("LATTE_AGENT_GOAL_MAX_ROUNDS")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .map(|n| n.clamp(1, MAX_GOAL_ROUNDS_CEILING))
        .unwrap_or(DEFAULT_MAX_GOAL_ROUNDS)
}

/// `block` 需要的最少连续轮数。
///
/// 照 deepseek-harness 的 `blockedAfterConsecutiveRounds`（默认 3）：模型在
/// 第一轮就报「我被卡住了」几乎总是没真试过。要求它先真的试几轮。
pub const BLOCK_MIN_ROUNDS: u32 = 2;

/// goal 层的完整状态：目标 + 进程本地的续跑许可。
#[derive(Debug, Clone)]
pub struct GoalState {
    goal: Option<Goal>,
    activation: GoalActivation,
    /// 单调自增的 id 序号，让同一 session 内的目标 id 不重复。
    next_seq: u64,
}

impl Default for GoalState {
    fn default() -> Self {
        Self {
            goal: None,
            // 安全阀：默认 disarmed。
            activation: GoalActivation::Disarmed,
            next_seq: 1,
        }
    }
}

/// 共享句柄。与 `SharedTodoState` / `SharedPlanStage` 同款形态。
pub type SharedGoal = Arc<parking_lot::RwLock<GoalState>>;

/// 造一个空的共享 goal 状态（无目标、disarmed）。
pub fn new_shared_goal() -> SharedGoal {
    Arc::new(parking_lot::RwLock::new(GoalState::default()))
}

/// round driver 每次询问的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveDecision {
    /// 不需要驱动（没目标 / 非 active / disarmed）。
    Idle,
    /// 该注入一轮 continuation。`round` 是本轮轮号（1-based）。
    Round { goal: Goal, round: u32 },
    /// 刚刚因为撞上硬边界转成 `blocked`——调用方应把 `reason` 告知用户。
    JustBlocked { goal: Goal, reason: GoalBlockReason },
}

impl GoalState {
    /// 当前目标的克隆快照。
    pub fn current(&self) -> Option<Goal> {
        self.goal.clone()
    }

    /// 当前续跑许可。
    pub fn activation(&self) -> GoalActivation {
        self.activation
    }

    /// 是否正在推进一个目标（active，不论 armed）。
    pub fn is_active(&self) -> bool {
        matches!(self.goal.as_ref().map(|g| g.phase), Some(GoalPhase::Active))
    }

    /// **安全阀**：把一个（比如从磁盘/session log 载入的）目标装进来，
    /// 并强制 `Disarmed`。
    ///
    /// 这是 [`GoalActivation`] 那条硬规则的唯一实现点：目标可以跨进程活
    /// 下来，**自动续跑不可以**。载入后必须由人显式 `resume` 才会重新
    /// 跑起来。
    pub fn load_disarmed(&mut self, goal: Goal) {
        self.next_seq = self.next_seq.max(goal.revision + 1);
        self.goal = Some(goal);
        self.activation = GoalActivation::Disarmed;
    }

    /// 显式解除续跑许可（用户 ⏸ / session-start / 任何不确定的时刻）。
    pub fn disarm(&mut self) {
        self.activation = GoalActivation::Disarmed;
    }

    /// 新建目标。允许条件：当前无目标，或当前目标已 `Complete`。
    ///
    /// `replace = true` 时覆盖任何现存目标（对应 oh-my-pi 的 `replaceGoal`）。
    pub fn create(
        &mut self,
        objective: impl Into<String>,
        max_rounds: Option<u32>,
        token_budget: Option<u64>,
        replace: bool,
    ) -> Result<Goal, GoalError> {
        let objective = objective.into();
        if objective.trim().is_empty() {
            return Err(GoalError::EmptyObjective);
        }
        if !replace {
            if let Some(existing) = &self.goal {
                if existing.phase != GoalPhase::Complete {
                    return Err(GoalError::AlreadyActive {
                        objective: existing.objective.clone(),
                    });
                }
            }
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        let goal = Goal {
            id: format!("goal-{seq}"),
            revision: 1,
            objective: objective.trim().to_string(),
            phase: GoalPhase::Active,
            blocked_reason: None,
            max_rounds: max_rounds
                .unwrap_or_else(default_max_goal_rounds)
                .clamp(1, MAX_GOAL_ROUNDS_CEILING),
            rounds_started: 0,
            token_budget,
            tokens_used: 0,
        };
        self.goal = Some(goal.clone());
        // 用户/模型刚显式建的目标 —— 这是唯一两个 arm 点之一。
        self.activation = GoalActivation::Armed;
        Ok(goal)
    }

    /// CAS 校验 + 取可变引用。
    fn expect_current(&mut self, r: &GoalRef) -> Result<&mut Goal, GoalError> {
        let current = self.goal.as_mut().ok_or(GoalError::NoGoal)?;
        if current.id != r.id || current.revision != r.revision {
            return Err(GoalError::StaleRef {
                expected: GoalRef {
                    id: current.id.clone(),
                    revision: current.revision,
                },
                got: r.clone(),
            });
        }
        Ok(current)
    }

    /// `active → paused`，并解除续跑许可。
    pub fn pause(&mut self, r: &GoalRef) -> Result<Goal, GoalError> {
        let g = self.expect_current(r)?;
        if g.phase != GoalPhase::Active {
            return Err(GoalError::InvalidTransition {
                from: g.phase,
                op: "pause",
            });
        }
        g.phase = GoalPhase::Paused;
        g.revision += 1;
        let out = g.clone();
        self.activation = GoalActivation::Disarmed;
        Ok(out)
    }

    /// `{active,paused,blocked} → active`，并**重新授予**续跑许可。
    ///
    /// 硬校验两条预算：轮次与 token 都必须还有余量，否则拒绝——否则
    /// resume 完立刻又撞上限，白跑一轮还要再 block 一次。这对齐
    /// deepseek-harness `resume()` 里那条 `roundsStarted >= maxGoalRounds`
    /// 就抛 `GOAL_INVALID_TRANSITION` 的校验。
    pub fn resume(&mut self, r: &GoalRef) -> Result<Goal, GoalError> {
        let g = self.expect_current(r)?;
        if g.phase == GoalPhase::Complete {
            return Err(GoalError::InvalidTransition {
                from: g.phase,
                op: "resume",
            });
        }
        if g.rounds_started >= g.max_rounds {
            return Err(GoalError::RoundsExhausted {
                rounds_started: g.rounds_started,
                max_rounds: g.max_rounds,
            });
        }
        if let Some(budget) = g.token_budget {
            if g.tokens_used >= budget {
                return Err(GoalError::TokensExhausted {
                    tokens_used: g.tokens_used,
                    token_budget: budget,
                });
            }
        }
        g.phase = GoalPhase::Active;
        g.blocked_reason = None;
        g.revision += 1;
        let out = g.clone();
        self.activation = GoalActivation::Armed;
        Ok(out)
    }

    /// `{active,paused,blocked} → complete`，并解除续跑许可。
    ///
    /// **引擎不做语义裁决**：这里不查 todo、不查 git、不跑测试（三个参考
    /// 实现都不查，见模块文档）。防「假装完成」靠两样东西：
    /// 1. 续跑 prompt 里那 6 条完成审计规定（`prompts/goal_continuation.md`）；
    /// 2. 调用方的**权限门**——见 `controller` 里 goal 工具的
    ///    `complete` 分支。
    pub fn complete(&mut self, r: &GoalRef) -> Result<Goal, GoalError> {
        let g = self.expect_current(r)?;
        if g.phase == GoalPhase::Complete {
            return Err(GoalError::InvalidTransition {
                from: g.phase,
                op: "complete",
            });
        }
        g.phase = GoalPhase::Complete;
        g.blocked_reason = None;
        g.revision += 1;
        let out = g.clone();
        self.activation = GoalActivation::Disarmed;
        Ok(out)
    }

    /// `active → blocked`，并解除续跑许可。
    pub fn block(&mut self, r: &GoalRef, reason: GoalBlockReason) -> Result<Goal, GoalError> {
        let g = self.expect_current(r)?;
        if g.phase != GoalPhase::Active {
            return Err(GoalError::InvalidTransition {
                from: g.phase,
                op: "block",
            });
        }
        g.phase = GoalPhase::Blocked;
        g.blocked_reason = Some(reason);
        g.revision += 1;
        let out = g.clone();
        self.activation = GoalActivation::Disarmed;
        Ok(out)
    }

    /// 丢弃当前目标。
    pub fn clear(&mut self) -> Option<Goal> {
        self.activation = GoalActivation::Disarmed;
        self.goal.take()
    }

    /// 抬高轮次上限 / 改预算（`resume` 前的补救动作）。
    pub fn edit_budget(
        &mut self,
        r: &GoalRef,
        max_rounds: Option<u32>,
        token_budget: Option<Option<u64>>,
    ) -> Result<Goal, GoalError> {
        let g = self.expect_current(r)?;
        if let Some(m) = max_rounds {
            g.max_rounds = m.clamp(1, MAX_GOAL_ROUNDS_CEILING);
        }
        if let Some(b) = token_budget {
            g.token_budget = b;
        }
        g.revision += 1;
        Ok(g.clone())
    }

    /// 记一笔 token 消耗。
    ///
    /// 口径照 oh-my-pi 的 `goalTokenDelta`：**算新消耗、不算复用前缀**。
    /// 它的公式是 `input + cacheWrite + output`（排除 `cacheRead`，理由是
    /// "reused prefix, not new work consumed by the goal"）。
    ///
    /// latte 这里要**减一次** `cache_read`：`TokenUsage::cache_read_tokens`
    /// 的文档写明它是 `input_tokens` 的**子集**（不是额外量），而 oh-my-pi
    /// 的 `input` 与 `cacheRead` 是并列的两个桶。不减就等于把复用前缀当成
    /// 新消耗，长会话里这一项会主导整个预算。
    ///
    /// `cache_write` 必须算进去（oh-my-pi 的注释：轮换 1h 缓存或重锚系统
    /// 提示能一次写 10 万+ token，Anthropic/Bedrock 单独计费）。
    pub fn record_usage(&mut self, delta: &GoalUsageDelta) -> Option<GoalBlockReason> {
        let g = self.goal.as_mut()?;
        // 只在推进中的目标上记账（paused/blocked/complete 期间产生的
        // token 不属于这个目标的自主消耗）。
        if g.phase != GoalPhase::Active {
            return None;
        }
        g.tokens_used = g.tokens_used.saturating_add(delta.billable());
        let budget = g.token_budget?;
        if g.tokens_used < budget {
            return None;
        }
        // 预算耗尽 → **blocked，不是 complete**。
        let reason = GoalBlockReason::new(
            BLOCK_CODE_TOKEN_BUDGET,
            format!(
                "目标用尽了 token 预算（{} / {budget}）。预算耗尽 ≠ 目标达成。",
                g.tokens_used
            ),
        );
        g.phase = GoalPhase::Blocked;
        g.blocked_reason = Some(reason.clone());
        g.revision += 1;
        self.activation = GoalActivation::Disarmed;
        Some(reason)
    }

    /// round driver 的**唯一入口**：原子地「检查 + 占轮号」。
    ///
    /// 为什么必须是一个方法而不是「先 `should_drive()` 再 `begin_round()`」：
    /// 那样两步之间存在窗口，并发/重入会开出两轮同号。这里在同一把写锁内
    /// 完成判断与自增。
    pub fn drive(&mut self) -> DriveDecision {
        let g = match self.goal.as_mut() {
            Some(g) => g,
            None => return DriveDecision::Idle,
        };
        if g.phase != GoalPhase::Active || self.activation != GoalActivation::Armed {
            return DriveDecision::Idle;
        }
        if g.rounds_started >= g.max_rounds {
            let reason = GoalBlockReason::new(
                BLOCK_CODE_ROUND_LIMIT,
                format!(
                    "目标用尽了配置的 {} 轮自动续跑。轮次耗尽 ≠ 目标达成。",
                    g.max_rounds
                ),
            );
            g.phase = GoalPhase::Blocked;
            g.blocked_reason = Some(reason.clone());
            g.revision += 1;
            let goal = g.clone();
            self.activation = GoalActivation::Disarmed;
            return DriveDecision::JustBlocked { goal, reason };
        }
        g.rounds_started += 1;
        g.revision += 1;
        let round = g.rounds_started;
        DriveDecision::Round {
            goal: g.clone(),
            round,
        }
    }
}

/// 一个 turn 的 token 增量。
///
/// 之所以是独立结构而不直接吃 `latte_ai::models::TokenUsage`：本模块要保持
/// 纯逻辑、可单测，不依赖 provider 层的类型。调用方负责把两次累计量相减
/// 成增量（累加**累计值**会让预算按 O(N²) 提前打爆——多角色路径的
/// `Supervisor::observe` 踩过这个坑）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GoalUsageDelta {
    pub input: u64,
    pub output: u64,
    pub thinking: u64,
    /// `input` 的**子集**：命中缓存的部分，记账时要扣掉。
    pub cache_read: u64,
    /// 写缓存的额外量，要算进消耗。
    pub cache_write: u64,
}

impl GoalUsageDelta {
    /// 计入预算的 token 数。见 [`GoalState::record_usage`] 的口径说明。
    pub fn billable(&self) -> u64 {
        self.input.saturating_sub(self.cache_read) + self.output + self.thinking + self.cache_write
    }
}

/// 续跑 prompt 的起始标记。
///
/// driver 把 continuation 当成一条普通输入自投递回通道（见
/// `run_single_role_loop` 的 `self_tx`），所以输入臂需要一个判据来区分
/// 「用户真的说了话」和「这是机器自己的续跑」。用**内容标记**而不是新增
/// 一个 `ControllerInput` 变体：那条臂有 ~400 行且强依赖大量可变捕获，
/// 拆成函数必然漂移；而内容标记本来就是这个代码库的既有约定
/// （`crate::inject_queue::INJECT_MARKER`、自动善后的 `[自动善后 N/M]`
/// 前缀都是这么干的）。
///
/// 两处必须据此分流：
/// 1. **不把整段 prompt 当 `UserMessage` 广播**——一轮上千字符，每轮刷一
///    次会把对话冲垮；照 oh-my-pi 的隐藏消息（`display: false`）改成一行
///    摘要。
/// 2. **不复位 plan 阶段门**。`reset_plan_stage` 的语义是「新的用户消息 =
///    新的决策周期，上一轮未批准的 plan 不再约束 delegate」。自动续跑不是
///    新的用户决策，复位会把一份**正在等用户勾选**的任务清单静默作废。
pub const GOAL_ROUND_MARKER: &str = "<goal_round>";

/// 渲染一轮续跑 prompt。
///
/// # 这段文本为什么这么写
///
/// 骨架取 deepseek-harness `goal-round-driver/src/prompt.ts` 的
/// `<goal_round>` 标签结构（Objective + Round N/max + 「以 workspace 和工具
/// 结果为权威，别信旧叙述」）；**那 6 条完成审计规定**逐条取自 oh-my-pi 的
/// `prompts/goals/goal-continuation.md`，它们是这套机制里最有价值的部分：
///
/// - 第 3 条（「别信上一轮的记忆，重新读」）对症 `edit` 锚点连续失败那类
///   事故——模型一直靠猜改锚点，从没想到该重读。
/// - 第 4 条（「验证范围 = 断言范围」）对症 `require_citations_observed`
///   那条注释记的事故：符号门禁放行了产出（名字都真实存在），但 7 个
///   `#error` 行号里 6 个标错——窄验证被当成宽结论。
/// - 第 6 条（「预算耗尽 ≠ 完成」）对症 `loop_no_release_on` 记的事故：
///   失败被吞成成功。这条同时也是状态机的不变量（撞上限产出 `blocked`
///   而不是 `complete`，见 [`GoalState::drive`] 与
///   [`GoalState::record_usage`]），prompt 与代码两边都写一遍。
///
/// 末句「NEVER narrate continuation — execute」也照抄：没有它，模型很容易
/// 把一整轮花在"我接下来准备如何继续"的说明上，白烧一次往返。
pub fn render_goal_round_prompt(goal: &Goal, round: u32) -> String {
    let budget_line = match (goal.token_budget, goal.tokens_remaining()) {
        (Some(budget), Some(remaining)) => format!(
            "- Token 预算：已用 {} / {budget}，剩余 {remaining}\n",
            goal.tokens_used
        ),
        _ => format!("- Token 预算：未设上限（已用 {}）\n", goal.tokens_used),
    };
    format!(
        "<goal_round>\n\
         Objective: {objective}\n\
         Round: {round}/{max_rounds}\n\
         {budget_line}\n\
         这是**自动续跑**，不是用户的新指令：目标跨 turn 持续有效。\
         绝不把成功重新定义成更小、更容易或已经完成的子集。\n\n\
         在这一轮里继续推进上面这个目标。以**当前的工作区、工具返回结果、\
         磁盘上的真实文件**为权威依据去判断进度——不要把前几轮的叙述当作\
         现状，仓库可能已经变了。做出具体进展，并验证结果。\n\n\
         调用 `goal` 工具的 `complete` 之前，必须先审计当前真实状态：\n\n\
         1. 把目标拆成具体交付物：需要哪些文件、哪些行为、哪些测试、哪些\
         门禁、哪些产物。记进 todo 或推理里。\n\
         2. 每个交付物对应权威证据：文件内容、命令输出、测试通过状态、\
         看板/任务状态。\n\
         3. 亲自查当前状态：读文件、跑命令或测试。绝不依赖早前轮次的记忆。\n\
         4. **验证范围 = 断言范围**。窄检查（某个文件的单测过了）不能证明\
         宽结论（这个特性端到端可用）。引用行号前先确认自己读到过那些行。\n\
         5. **不确定 = 未达成**：间接证据、覆盖不全、产物缺失、只是\
         「看起来对」但没实际查过 —— 一律继续干活，去取更强的证据。\n\
         6. **预算耗尽 ≠ 完成**。绝不因为轮次或 token 快用完就调 complete。\
         预算紧而活没干完 → 让目标留在 active，收尾本轮，把决定权交回用户。\n\n\
         只有当每一个交付物都有直接的、反映当前状态的证据时，才调用\
         `goal` 的 `complete`——那一步会结束自动续跑并向用户汇报完成。\n\
         确实被外部条件卡住（缺输入、权限不足、事实无法核验）时调 `blocked` \
         并说明卡在哪，不要假装完成。\n\n\
         活没干完就继续干。不要叙述你打算怎么继续——直接执行。\n\
         </goal_round>",
        objective = goal.objective,
        max_rounds = goal.max_rounds,
    )
}

// ─── 持久化 ────────────────────────────────────────────────────────────
//
// 落盘的**只有目标快照**，绝不含 [`GoalActivation`]。这不是省事，是本机制
// 的安全阀：目标可以跨进程活下来，自动续跑不可以。载回时统一走
// [`GoalState::load_disarmed`]，必须由人显式 `resume` 才会重新跑起来。
//
// 路径与「best-effort、失败不打断主流程」的口径照 `crate::pending_ask`
// （同类东西：per-session 的可续跑状态，controller 自有，写在
// `cwd/.latte/<kind>/`）。

use std::path::{Path, PathBuf};

/// 目标落盘目录：`<cwd>/.latte/goals`。
fn goal_dir(cwd: &Path) -> PathBuf {
    cwd.join(".latte").join("goals")
}

/// `session_id` 进文件名前按路径分量校验。
///
/// 它多数时候来自进程内生成，但也可能由客户端（编辑器 / HTTP 调用方）
/// 传入，所以不能直接拼进路径。
fn safe_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && !session_id.contains('/')
        && !session_id.contains('\\')
        && !session_id.contains("..")
        && session_id.len() <= 200
}

fn goal_path(cwd: &Path, session_id: &str) -> Option<PathBuf> {
    safe_session_id(session_id).then(|| goal_dir(cwd).join(format!("{session_id}.json")))
}

/// 把当前目标写盘（`None` = 无目标时删除记录）。
///
/// best-effort：写失败只记 warn 不打断。丢一次快照的代价是"重启后目标要
/// 重新设一遍"，而为了写盘失败中止一轮真在干活的续跑显然更糟。
pub fn persist(cwd: &Path, session_id: &str, state: &GoalState) {
    let Some(path) = goal_path(cwd, session_id) else {
        tracing::warn!(session_id, "goal: session_id 不适合做文件名，跳过落盘");
        return;
    };
    let goal = state.current();
    let Some(goal) = goal else {
        // 目标被清除 → 删掉记录，避免下次启动又把它载回来。
        let _ = std::fs::remove_file(&path);
        return;
    };
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!(error = %e, ?parent, "goal: 建目录失败，跳过落盘");
            return;
        }
    }
    match serde_json::to_vec_pretty(&goal) {
        Ok(bytes) => {
            if let Err(e) = std::fs::write(&path, bytes) {
                tracing::warn!(error = %e, ?path, "goal: 写盘失败");
            }
        }
        Err(e) => tracing::warn!(error = %e, "goal: 序列化失败"),
    }
}

/// 从盘上载回目标快照。
///
/// 返回 `Some(goal)` 时调用方**必须**用 [`GoalState::load_disarmed`] 装进
/// 状态（而不是自己拼一个 armed 的 state）——那个方法是安全阀的唯一实现点。
pub fn load(cwd: &Path, session_id: &str) -> Option<Goal> {
    let path = goal_path(cwd, session_id)?;
    let bytes = std::fs::read(&path).ok()?;
    match serde_json::from_slice::<Goal>(&bytes) {
        Ok(g) => Some(g),
        Err(e) => {
            // 读不出来就当没有：goal 是**辅助**契约，格式漂移不该让整个
            // 会话起不来。坏文件留在盘上便于排查。
            tracing::warn!(error = %e, ?path, "goal: 快照解析失败，按无目标继续");
            None
        }
    }
}

/// 删除落盘记录。
pub fn forget(cwd: &Path, session_id: &str) {
    if let Some(path) = goal_path(cwd, session_id) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn armed_goal(max_rounds: u32) -> (GoalState, GoalRef) {
        let mut s = GoalState::default();
        let g = s
            .create("把 loop 缺口补齐", Some(max_rounds), None, false)
            .expect("create");
        let r = g.goal_ref();
        (s, r)
    }

    /// 新建的状态必须是 disarmed —— 安全阀的默认值不能反。
    #[test]
    fn fresh_state_is_disarmed_with_no_goal() {
        let s = GoalState::default();
        assert!(s.current().is_none());
        assert_eq!(s.activation(), GoalActivation::Disarmed);
        assert!(!s.is_active());
    }

    /// 显式 create 是两个 arm 点之一。
    #[test]
    fn create_arms_and_starts_active() {
        let (s, _) = armed_goal(3);
        let g = s.current().expect("goal");
        assert_eq!(g.phase, GoalPhase::Active);
        assert_eq!(g.revision, 1);
        assert_eq!(g.rounds_started, 0);
        assert_eq!(s.activation(), GoalActivation::Armed);
    }

    #[test]
    fn create_rejects_empty_objective_and_double_create() {
        let mut s = GoalState::default();
        assert_eq!(
            s.create("   ", None, None, false).unwrap_err(),
            GoalError::EmptyObjective
        );
        s.create("甲", None, None, false).expect("first");
        match s.create("乙", None, None, false).unwrap_err() {
            GoalError::AlreadyActive { objective } => assert_eq!(objective, "甲"),
            e => panic!("应拒绝重复建目标，实际 {e:?}"),
        }
        // replace 可以覆盖。
        s.create("乙", None, None, true).expect("replace");
        assert_eq!(s.current().unwrap().objective, "乙");
    }

    /// 完成之后可以再建下一个目标。
    #[test]
    fn create_allowed_after_complete() {
        let (mut s, r) = armed_goal(3);
        s.complete(&r).expect("complete");
        s.create("下一件事", None, None, false)
            .expect("create again");
        assert_eq!(s.current().unwrap().phase, GoalPhase::Active);
    }

    /// **安全阀的核心断言**：载入一个 active 目标后必须是 disarmed，
    /// 于是 driver 不会自动跑起来；显式 resume 才 arm。
    #[test]
    fn load_disarmed_blocks_auto_continuation_until_explicit_resume() {
        let (mut src, _) = armed_goal(5);
        let persisted = src.current().expect("goal");
        assert_eq!(persisted.phase, GoalPhase::Active);

        // 模拟进程重启：新状态载入同一个 active 目标。
        let mut s = GoalState::default();
        s.load_disarmed(persisted.clone());
        assert!(s.is_active(), "目标本身应该活下来");
        assert_eq!(
            s.activation(),
            GoalActivation::Disarmed,
            "但自动续跑必须是关的——重启就自己接着跑会在用户不在场时烧 token"
        );
        assert_eq!(
            s.drive(),
            DriveDecision::Idle,
            "disarmed 时 driver 必须不动"
        );

        // 显式 resume → arm → driver 才开始工作。
        let resumed = s.resume(&persisted.goal_ref()).expect("resume");
        assert_eq!(s.activation(), GoalActivation::Armed);
        assert!(matches!(s.drive(), DriveDecision::Round { round: 1, .. }));
        assert!(
            resumed.revision > persisted.revision,
            "mutation 必须推进 revision"
        );
    }

    /// CAS：陈旧 ref 必须被拒，且不改状态。
    #[test]
    fn stale_ref_is_rejected_without_mutating() {
        let (mut s, stale) = armed_goal(5);
        s.pause(&stale).expect("pause with fresh ref");
        let err = s.complete(&stale).unwrap_err();
        match err {
            GoalError::StaleRef { expected, got } => {
                assert_eq!(got.revision, 1);
                assert_eq!(expected.revision, 2);
            }
            e => panic!("应报 StaleRef，实际 {e:?}"),
        }
        assert_eq!(
            s.current().unwrap().phase,
            GoalPhase::Paused,
            "被拒的 mutation 不得改相位"
        );
    }

    /// 相位转换的合法性。
    #[test]
    fn phase_transitions_are_guarded() {
        let (mut s, r1) = armed_goal(5);
        // pause 只能从 active
        let paused = s.pause(&r1).expect("active → paused");
        assert!(matches!(
            s.pause(&paused.goal_ref()).unwrap_err(),
            GoalError::InvalidTransition { op: "pause", .. }
        ));
        // block 只能从 active
        assert!(matches!(
            s.block(&paused.goal_ref(), GoalBlockReason::new("x", "y"))
                .unwrap_err(),
            GoalError::InvalidTransition { op: "block", .. }
        ));
        // paused → active
        let active = s.resume(&paused.goal_ref()).expect("paused → active");
        // active → blocked → active
        let blocked = s
            .block(&active.goal_ref(), GoalBlockReason::new("x", "y"))
            .expect("active → blocked");
        assert_eq!(
            s.activation(),
            GoalActivation::Disarmed,
            "block 必须 disarm"
        );
        let re = s.resume(&blocked.goal_ref()).expect("blocked → active");
        assert!(re.blocked_reason.is_none(), "resume 应清掉 blocked_reason");
        // complete 之后不能再 complete / resume
        let done = s.complete(&re.goal_ref()).expect("complete");
        assert!(matches!(
            s.complete(&done.goal_ref()).unwrap_err(),
            GoalError::InvalidTransition { op: "complete", .. }
        ));
        assert!(matches!(
            s.resume(&done.goal_ref()).unwrap_err(),
            GoalError::InvalidTransition { op: "resume", .. }
        ));
    }

    /// 轮号严格递增，且 drive 会推进 revision。
    #[test]
    fn drive_allocates_monotonic_rounds() {
        let (mut s, _) = armed_goal(3);
        for expected in 1..=3u32 {
            match s.drive() {
                DriveDecision::Round { round, goal } => {
                    assert_eq!(round, expected);
                    assert_eq!(goal.rounds_started, expected);
                }
                d => panic!("第 {expected} 轮应可驱动，实际 {d:?}"),
            }
        }
    }

    /// **轮次到顶 → blocked(round-limit)，不是 complete。**
    #[test]
    fn round_limit_blocks_rather_than_completing() {
        let (mut s, _) = armed_goal(2);
        assert!(matches!(s.drive(), DriveDecision::Round { round: 1, .. }));
        assert!(matches!(s.drive(), DriveDecision::Round { round: 2, .. }));
        match s.drive() {
            DriveDecision::JustBlocked { goal, reason } => {
                assert_eq!(
                    goal.phase,
                    GoalPhase::Blocked,
                    "耗尽是 blocked 而非 complete"
                );
                assert_eq!(reason.code, BLOCK_CODE_ROUND_LIMIT);
                assert!(reason.message.contains("≠ 目标达成"), "{}", reason.message);
            }
            d => panic!("第 3 轮应撞上限，实际 {d:?}"),
        }
        // 到顶后 disarm，且再问也不会又 block 一次（已经不是 active）。
        assert_eq!(s.activation(), GoalActivation::Disarmed);
        assert_eq!(s.drive(), DriveDecision::Idle);
    }

    /// 到顶后必须先抬轮次上限才能 resume。
    #[test]
    fn resume_after_round_limit_requires_raising_the_cap() {
        let (mut s, _) = armed_goal(1);
        s.drive();
        let blocked = match s.drive() {
            DriveDecision::JustBlocked { goal, .. } => goal,
            d => panic!("应撞上限，实际 {d:?}"),
        };
        match s.resume(&blocked.goal_ref()).unwrap_err() {
            GoalError::RoundsExhausted {
                rounds_started,
                max_rounds,
            } => {
                assert_eq!(rounds_started, 1);
                assert_eq!(max_rounds, 1);
            }
            e => panic!("应拒绝 resume，实际 {e:?}"),
        }
        let raised = s
            .edit_budget(&blocked.goal_ref(), Some(4), None)
            .expect("抬高上限");
        s.resume(&raised.goal_ref()).expect("抬高后可以 resume");
        assert!(matches!(s.drive(), DriveDecision::Round { round: 2, .. }));
    }

    /// token 记账口径：扣掉 cache_read（它是 input 的子集），算上 cache_write。
    #[test]
    fn usage_delta_excludes_reused_prefix_and_counts_cache_writes() {
        let d = GoalUsageDelta {
            input: 10_000,
            output: 500,
            thinking: 100,
            cache_read: 9_000,
            cache_write: 2_000,
        };
        // (10000 - 9000) + 500 + 100 + 2000
        assert_eq!(d.billable(), 3_600);

        // 全命中缓存时几乎不计消耗——这正是不减 cache_read 会失真的地方。
        let all_cached = GoalUsageDelta {
            input: 50_000,
            cache_read: 50_000,
            output: 200,
            ..Default::default()
        };
        assert_eq!(all_cached.billable(), 200);
    }

    /// **预算到顶 → blocked(token-budget)，不是 complete。**
    #[test]
    fn token_budget_exhaustion_blocks_rather_than_completing() {
        let mut s = GoalState::default();
        s.create("重活", Some(10), Some(1_000), false)
            .expect("create");
        let under = GoalUsageDelta {
            input: 400,
            ..Default::default()
        };
        assert!(s.record_usage(&under).is_none(), "未到顶不该 block");
        assert_eq!(s.current().unwrap().tokens_used, 400);

        let over = GoalUsageDelta {
            input: 700,
            ..Default::default()
        };
        let reason = s.record_usage(&over).expect("到顶应 block");
        assert_eq!(reason.code, BLOCK_CODE_TOKEN_BUDGET);
        assert!(reason.message.contains("≠ 目标达成"), "{}", reason.message);
        let g = s.current().unwrap();
        assert_eq!(g.phase, GoalPhase::Blocked);
        assert_eq!(g.tokens_used, 1_100);
        assert_eq!(s.activation(), GoalActivation::Disarmed);
        assert_eq!(s.drive(), DriveDecision::Idle, "预算耗尽后不得再自动续跑");
    }

    /// 非 active 相位不记账（暂停期间的消耗不算这个目标的）。
    #[test]
    fn usage_is_only_recorded_while_active() {
        let (mut s, r) = armed_goal(5);
        s.pause(&r).expect("pause");
        let d = GoalUsageDelta {
            input: 5_000,
            ..Default::default()
        };
        assert!(s.record_usage(&d).is_none());
        assert_eq!(s.current().unwrap().tokens_used, 0);
    }

    /// 无预算（None）时永不因 token 被 block。
    #[test]
    fn unbounded_budget_never_blocks() {
        let (mut s, _) = armed_goal(5);
        let huge = GoalUsageDelta {
            input: 50_000_000,
            ..Default::default()
        };
        assert!(s.record_usage(&huge).is_none());
        assert_eq!(s.current().unwrap().phase, GoalPhase::Active);
    }

    /// 轮次上限被钳在安全区间：0 抬到 1，超大钳到硬顶。
    #[test]
    fn max_rounds_is_clamped() {
        let mut s = GoalState::default();
        let g = s.create("a", Some(0), None, true).expect("create");
        assert_eq!(g.max_rounds, 1, "0 轮会让目标建了就立刻 block");
        let g = s.create("b", Some(99_999), None, true).expect("create");
        assert_eq!(g.max_rounds, MAX_GOAL_ROUNDS_CEILING);
        let g = s.create("c", None, None, true).expect("create");
        assert_eq!(g.max_rounds, DEFAULT_MAX_GOAL_ROUNDS);
    }

    /// 默认轮次上限的 env 开关与钳制。
    #[test]
    fn default_max_goal_rounds_reads_env_and_clamps() {
        static L: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = L.lock().unwrap_or_else(|e| e.into_inner());

        std::env::remove_var("LATTE_AGENT_GOAL_MAX_ROUNDS");
        assert_eq!(default_max_goal_rounds(), DEFAULT_MAX_GOAL_ROUNDS);

        std::env::set_var("LATTE_AGENT_GOAL_MAX_ROUNDS", " 40 ");
        assert_eq!(default_max_goal_rounds(), 40, "应容忍空白");

        std::env::set_var("LATTE_AGENT_GOAL_MAX_ROUNDS", "99999");
        assert_eq!(
            default_max_goal_rounds(),
            MAX_GOAL_ROUNDS_CEILING,
            "越界钳到硬顶——这是 runaway 兜底，不接受任意大"
        );

        std::env::set_var("LATTE_AGENT_GOAL_MAX_ROUNDS", "0");
        assert_eq!(default_max_goal_rounds(), 1, "0 会让目标建了就立刻 block");

        std::env::set_var("LATTE_AGENT_GOAL_MAX_ROUNDS", "不是数字");
        assert_eq!(
            default_max_goal_rounds(),
            DEFAULT_MAX_GOAL_ROUNDS,
            "非法值回退默认"
        );

        // 显式传参优先于 env。
        std::env::set_var("LATTE_AGENT_GOAL_MAX_ROUNDS", "40");
        let mut s = GoalState::default();
        let g = s.create("显式优先", Some(7), None, true).expect("create");
        assert_eq!(g.max_rounds, 7);
        // 不传则走 env。
        let g = s.create("走 env", None, None, true).expect("create");
        assert_eq!(g.max_rounds, 40);

        std::env::remove_var("LATTE_AGENT_GOAL_MAX_ROUNDS");
    }

    #[test]
    fn clear_drops_goal_and_disarms() {
        let (mut s, _) = armed_goal(5);
        let dropped = s.clear().expect("有目标可丢");
        assert_eq!(dropped.phase, GoalPhase::Active);
        assert!(s.current().is_none());
        assert_eq!(s.activation(), GoalActivation::Disarmed);
        assert_eq!(s.drive(), DriveDecision::Idle);
    }

    /// 续跑 prompt 必须带齐 6 条审计规定里的关键约束 —— 这段文本是本机制
    /// 里唯一防「假装完成」的东西（引擎不做语义裁决），漏一条就少一道防线。
    #[test]
    fn continuation_prompt_carries_the_completion_audit_rules() {
        let (mut s, _) = armed_goal(7);
        let goal = match s.drive() {
            DriveDecision::Round { goal, round } => {
                assert_eq!(round, 1);
                goal
            }
            d => panic!("应可驱动，实际 {d:?}"),
        };
        let p = render_goal_round_prompt(&goal, 1);

        assert!(
            p.contains("<goal_round>") && p.contains("</goal_round>"),
            "要有标签边界"
        );
        assert!(p.contains("把 loop 缺口补齐"), "要带 objective 原文");
        assert!(p.contains("Round: 1/7"), "要带轮号与上限：{p}");
        assert!(p.contains("未设上限"), "无预算时要说明：{p}");

        // 6 条审计规定的关键措辞
        assert!(
            p.contains("绝不把成功重新定义成更小"),
            "缺「不得缩小成功定义」"
        );
        assert!(
            p.contains("绝不依赖早前轮次的记忆"),
            "缺第 3 条（重新取证）"
        );
        assert!(p.contains("验证范围 = 断言范围"), "缺第 4 条");
        assert!(p.contains("不确定 = 未达成"), "缺第 5 条");
        assert!(p.contains("预算耗尽 ≠ 完成"), "缺第 6 条");
        assert!(
            p.contains("不要叙述你打算怎么继续"),
            "缺「别只叙述、直接执行」"
        );
        // 两条出路都要给：完成走 complete，真卡住走 blocked
        assert!(
            p.contains("complete") && p.contains("blocked"),
            "要给出两条出路"
        );
    }

    /// 有预算时要把剩余量摊给模型看（第 6 条才有判断依据）。
    #[test]
    fn continuation_prompt_shows_remaining_budget() {
        let mut s = GoalState::default();
        s.create("重活", Some(5), Some(10_000), false)
            .expect("create");
        s.record_usage(&GoalUsageDelta {
            input: 3_000,
            ..Default::default()
        });
        let goal = match s.drive() {
            DriveDecision::Round { goal, .. } => goal,
            d => panic!("应可驱动，实际 {d:?}"),
        };
        let p = render_goal_round_prompt(&goal, 1);
        assert!(
            p.contains("已用 3000 / 10000") && p.contains("剩余 7000"),
            "要给出已用/上限/剩余：{p}"
        );
    }

    // ─── 持久化 ──────────────────────────────────────────────────────

    /// **持久化的核心断言**：目标跨进程活下来，自动续跑不跟着活过来。
    ///
    /// 这条是整个 `activation` 设计的意义所在。没有持久化时它是空转的
    /// （重启目标直接就没了，谈不上"重启不自动跑"）；有了持久化，
    /// 「载回即 disarmed」才真正是一道安全阀。
    #[test]
    fn persisted_goal_survives_restart_but_auto_continuation_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();

        // 进程 A：立目标、跑两轮、烧一些 token，然后落盘。
        let mut a = GoalState::default();
        a.create("跨进程也要记得的目标", Some(9), Some(100_000), false)
            .expect("create");
        a.drive();
        a.drive();
        a.record_usage(&GoalUsageDelta {
            input: 7_000,
            output: 300,
            ..Default::default()
        });
        assert_eq!(a.activation(), GoalActivation::Armed);
        persist(cwd, "sess-1", &a);

        // 进程 B：载回。
        let loaded = load(cwd, "sess-1").expect("盘上应有快照");
        let mut b = GoalState::default();
        b.load_disarmed(loaded.clone());

        // 目标本身逐字段活下来。
        let g = b.current().expect("goal");
        assert_eq!(g.objective, "跨进程也要记得的目标");
        assert_eq!(g.phase, GoalPhase::Active);
        assert_eq!(
            g.rounds_started, 2,
            "已用轮次必须一起活下来，否则预算可被重启刷掉"
        );
        assert_eq!(g.max_rounds, 9);
        assert_eq!(g.tokens_used, 7_300, "token 记账同理");
        assert_eq!(g.token_budget, Some(100_000));

        // 但自动续跑没有。
        assert_eq!(b.activation(), GoalActivation::Disarmed);
        assert_eq!(
            b.drive(),
            DriveDecision::Idle,
            "重启后绝不能自己接着跑——用户不在场时那是纯烧 token"
        );

        // 显式 resume 之后才继续，且轮号接着 2 往下走（不从 1 重来）。
        b.resume(&g.goal_ref()).expect("resume");
        assert!(matches!(b.drive(), DriveDecision::Round { round: 3, .. }));
    }

    /// 清除目标要把盘上记录一起删掉，否则下次启动又把它载回来。
    #[test]
    fn persist_removes_the_record_when_goal_is_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let mut s = GoalState::default();
        s.create("待丢弃", None, None, false).expect("create");
        persist(cwd, "sess-2", &s);
        assert!(load(cwd, "sess-2").is_some());

        s.clear();
        persist(cwd, "sess-2", &s);
        assert!(
            load(cwd, "sess-2").is_none(),
            "丢弃后不得还能载回来（否则重启阴魂不散）"
        );
    }

    /// 没有记录 / 记录损坏时都按"无目标"继续，不 panic。
    ///
    /// goal 是**辅助**契约：它的格式漂移不该让整个会话起不来。
    #[test]
    fn load_is_lenient_about_missing_and_corrupt_records() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        assert!(load(cwd, "never-written").is_none());

        std::fs::create_dir_all(goal_dir(cwd)).unwrap();
        std::fs::write(goal_dir(cwd).join("broken.json"), b"{not json").unwrap();
        assert!(load(cwd, "broken").is_none(), "坏文件应按无目标处理");
        assert!(
            goal_dir(cwd).join("broken.json").exists(),
            "坏文件要留在盘上便于排查"
        );
    }

    /// 危险的 session_id 不得拼进路径。
    #[test]
    fn unsafe_session_ids_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        for bad in ["", "../escape", "a/b", "a\\b", &"x".repeat(201)] {
            assert!(goal_path(cwd, bad).is_none(), "应拒绝 {bad:?}");
        }
        // 拒绝时 persist 不写任何东西、也不 panic。
        let mut s = GoalState::default();
        s.create("x", None, None, false).unwrap();
        persist(cwd, "../escape", &s);
        assert!(!goal_dir(cwd).exists() || std::fs::read_dir(goal_dir(cwd)).unwrap().count() == 0);
    }

    /// 不同 session 各自一份，互不覆盖。
    #[test]
    fn sessions_get_independent_records() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let mut a = GoalState::default();
        a.create("甲的目标", None, None, false).unwrap();
        persist(cwd, "sess-a", &a);
        let mut b = GoalState::default();
        b.create("乙的目标", None, None, false).unwrap();
        persist(cwd, "sess-b", &b);

        assert_eq!(load(cwd, "sess-a").unwrap().objective, "甲的目标");
        assert_eq!(load(cwd, "sess-b").unwrap().objective, "乙的目标");

        forget(cwd, "sess-a");
        assert!(load(cwd, "sess-a").is_none());
        assert!(load(cwd, "sess-b").is_some(), "忘掉一个不该动另一个");
    }
}
