//! 会话级 todo 状态 + 运行中纠偏（TodoTracker）。
//!
//! 与 `latte_rs_agent_tools::tools::todo`（纯函数状态机）的分工：
//! 那个工具是无状态的——调用方传 `currentPhases`、返回新 `phases`。
//! 本模块提供**会话级**的那一层：
//!
//! - [`SharedTodoState`]：每会话一份的 `Vec<TodoPhase>`，由
//!   `ChatController` 持有，`register_todo_tool`（controller.rs）的
//!   handler 与 agent loop 里的 [`TodoTracker`] 共享。handler 负责
//!   「取出 → 调纯函数 → 存回 → 广播 `ChatEvent::TodoUpdated`」；
//!   tracker 只读它，做运行中纠偏。
//! - [`TodoTracker`]：每 turn 一份（`AgentRunner::run_turn` 入口重置），
//!   实现两类纠偏，参数参考 oh-my-pi：
//!   1. **mid-run nudge**：同一 turn 内累计 [`TODO_NUDGE_THRESHOLD`]
//!      次非 todo 工具调用而没碰过 todo 时，注入一次提醒，每 turn
//!      最多 [`TODO_MAX_NUDGES_PER_TURN`] 次；
//!   2. **stop reminder**：模型停止输出（无待处理工具调用）而 todo
//!      还有 pending/in_progress 项时，注入提醒让 agent 继续跑，
//!      每 turn 最多 [`TODO_MAX_STOP_REMINDERS_PER_TURN`] 次；
//!      正在等用户回答（本 turn 调过 ask / 有挂起的选择题）时跳过。

use latte_rs_agent_tools::tools::todo::{TodoPhase, TodoStatus};

/// 每会话 todo 状态的共享句柄（与 `SharedPlanStage` 同款生命周期：
/// `ChatController` 字段，随 runner 重建clone 传递）。
pub type SharedTodoState = std::sync::Arc<parking_lot::RwLock<Vec<TodoPhase>>>;

/// 新建一份空会话 todo 状态。
pub fn new_shared_todo_state() -> SharedTodoState {
    std::sync::Arc::new(parking_lot::RwLock::new(Vec::new()))
}

// ─── ChatEvent::TodoUpdated 的视图类型（与前端约定的线上契约） ─────────

/// `ChatEvent::TodoUpdated` 里的单个 task 视图。
/// `status` 是字符串枚举：`"pending" | "in_progress" | "completed" | "abandoned"`。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TodoTaskView {
    pub content: String,
    pub status: String,
}

/// `ChatEvent::TodoUpdated` 里的单个 phase 视图。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TodoPhaseView {
    pub name: String,
    pub tasks: Vec<TodoTaskView>,
}

/// `TodoStatus` → 线上契约的字符串。
pub fn status_str(status: TodoStatus) -> &'static str {
    match status {
        TodoStatus::Pending => "pending",
        TodoStatus::InProgress => "in_progress",
        TodoStatus::Completed => "completed",
        TodoStatus::Abandoned => "abandoned",
    }
}

/// 把内部状态转成广播视图。
pub fn phase_views(phases: &[TodoPhase]) -> Vec<TodoPhaseView> {
    phases
        .iter()
        .map(|p| TodoPhaseView {
            name: p.name.clone(),
            tasks: p
                .tasks
                .iter()
                .map(|t| TodoTaskView {
                    content: t.content.clone(),
                    status: status_str(t.status).to_string(),
                })
                .collect(),
        })
        .collect()
}

/// 未完成（pending / in_progress）的 task 内容列表，按列表顺序。
pub fn incomplete_tasks(phases: &[TodoPhase]) -> Vec<String> {
    phases
        .iter()
        .flat_map(|p| p.tasks.iter())
        .filter(|t| matches!(t.status, TodoStatus::Pending | TodoStatus::InProgress))
        .map(|t| t.content.clone())
        .collect()
}

/// 第一个 in_progress 的 task 内容（CLI 渲染「当前」用）。
pub fn current_in_progress(phases: &[TodoPhase]) -> Option<String> {
    phases
        .iter()
        .flat_map(|p| p.tasks.iter())
        .find(|t| t.status == TodoStatus::InProgress)
        .map(|t| t.content.clone())
}

/// (completed 数, 非 abandoned 总数) —— CLI 渲染 `done/total` 用。
pub fn progress_counts(phases: &[TodoPhase]) -> (usize, usize) {
    let mut done = 0;
    let mut total = 0;
    for t in phases.iter().flat_map(|p| p.tasks.iter()) {
        if t.status == TodoStatus::Abandoned {
            continue;
        }
        total += 1;
        if t.status == TodoStatus::Completed {
            done += 1;
        }
    }
    (done, total)
}

// ─── TodoTracker：每 turn 的运行中纠偏 ─────────────────────────────────

/// mid-run nudge 的触发阈值：同一 turn 内这么多次非 todo 工具调用
/// 而没碰过 todo，就提醒一次。
pub const TODO_NUDGE_THRESHOLD: u32 = 12;
/// mid-run nudge 每 turn 的上限。
pub const TODO_MAX_NUDGES_PER_TURN: u32 = 2;
/// stop reminder 每 turn 的上限：超过就让 agent 停下（尊重它的
/// 「我做完了/我卡住了」判断，不无限续跑）。
pub const TODO_MAX_STOP_REMINDERS_PER_TURN: u32 = 3;

/// 每 turn 的 todo 纠偏状态。`AgentRunner::run_turn` 入口重置
/// （与 `last_turn_tool_count` 同位置）。
#[derive(Debug, Default, Clone)]
pub struct TodoTracker {
    /// 距上次 todo 调用以来的非 todo 工具调用数（todo 调用会清零）。
    tool_calls_since_todo: u32,
    /// 本 turn 已发的 mid-run nudge 数。
    nudges_sent: u32,
    /// 本 turn 已发的 stop reminder 数。
    stop_reminders_sent: u32,
    /// 本 turn 是否成功调过 ask（fire-and-forget 提问后 stop
    /// reminder 不得催——agent 在等用户回答，不是在摸鱼）。
    asked_user: bool,
}

impl TodoTracker {
    /// 记账一次**成功执行**的工具调用（`apply_call_result` 的
    /// `executed_ok` 分支调用）。`short_name` 是不带命名空间的工具名。
    pub fn record_tool_call(&mut self, short_name: &str) {
        if short_name == "todo" {
            self.tool_calls_since_todo = 0;
        } else {
            self.tool_calls_since_todo += 1;
            if short_name == "ask" {
                self.asked_user = true;
            }
        }
    }

    /// mid-run nudge：该发就返回提醒文本并计入次数，否则 `None`。
    /// 仅在 todo 列表非空且有未完成项时触发。
    pub fn take_nudge(&mut self, phases: &[TodoPhase]) -> Option<String> {
        if self.nudges_sent >= TODO_MAX_NUDGES_PER_TURN
            || self.tool_calls_since_todo < TODO_NUDGE_THRESHOLD
        {
            return None;
        }
        let open = incomplete_tasks(phases).len();
        if open == 0 {
            return None;
        }
        self.nudges_sent += 1;
        // 提醒后重新计数：模型若照办，下一次 todo 调用本就会清零；
        // 若不照办，再攒一轮阈值后（还有额度时）提醒第二次。
        self.tool_calls_since_todo = 0;
        Some(format!(
            "📝 todo 提醒：还有 {open} 个 todo 项未完成。如果刚完成了其中某项，\
             现在就用 todo 工具（done）把它标掉；如果任务范围有变化，用 todo 更新列表 \
             （append/rm/drop）。不要等到全部做完才一次性补标。"
        ))
    }

    /// stop reminder：模型停止输出、无待处理工具调用时该续跑就返回
    /// 提醒文本并计入次数，否则 `None`。
    ///
    /// `waiting_on_user`：有挂起的选择题（阻塞 ask / ChoiceRequested）
    /// 时为 true——agent 在等用户，不催。
    pub fn take_stop_reminder(
        &mut self,
        phases: &[TodoPhase],
        waiting_on_user: bool,
    ) -> Option<String> {
        if self.stop_reminders_sent >= TODO_MAX_STOP_REMINDERS_PER_TURN
            || self.asked_user
            || waiting_on_user
        {
            return None;
        }
        let incomplete = incomplete_tasks(phases);
        if incomplete.is_empty() {
            return None;
        }
        self.stop_reminders_sent += 1;
        let list = incomplete
            .iter()
            .take(5)
            .map(|c| format!("- {c}"))
            .collect::<Vec<_>>()
            .join("\n");
        let more = if incomplete.len() > 5 {
            format!("\n- …（共 {} 项）", incomplete.len())
        } else {
            String::new()
        };
        Some(format!(
            "You stopped with {} incomplete todo item(s):\n{list}{more}\n\
             Please continue working or explain why you're blocked.",
            incomplete.len()
        ))
    }

    /// 测试/观测用：已发 stop reminder 数。
    #[cfg(test)]
    pub fn stop_reminders_sent(&self) -> u32 {
        self.stop_reminders_sent
    }

    /// 测试/观测用：已发 nudge 数。
    #[cfg(test)]
    pub fn nudges_sent(&self) -> u32 {
        self.nudges_sent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phases(items: &[(&str, TodoStatus)]) -> Vec<TodoPhase> {
        vec![TodoPhase {
            name: "P".into(),
            tasks: items
                .iter()
                .map(|(c, s)| TodoItem {
                    content: c.to_string(),
                    status: *s,
                })
                .collect(),
        }]
    }

    use latte_rs_agent_tools::tools::todo::TodoItem;

    #[test]
    fn phase_views_match_wire_contract() {
        let v = phase_views(&phases(&[
            ("A", TodoStatus::Completed),
            ("B", TodoStatus::InProgress),
            ("C", TodoStatus::Pending),
            ("D", TodoStatus::Abandoned),
        ]));
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(
            json,
            serde_json::json!([{
                "name": "P",
                "tasks": [
                    {"content": "A", "status": "completed"},
                    {"content": "B", "status": "in_progress"},
                    {"content": "C", "status": "pending"},
                    {"content": "D", "status": "abandoned"}
                ]
            }])
        );
    }

    #[test]
    fn progress_counts_skip_abandoned() {
        let p = phases(&[
            ("A", TodoStatus::Completed),
            ("B", TodoStatus::InProgress),
            ("C", TodoStatus::Abandoned),
        ]);
        assert_eq!(progress_counts(&p), (1, 2));
        assert_eq!(current_in_progress(&p).as_deref(), Some("B"));
        assert_eq!(incomplete_tasks(&p), vec!["B".to_string()]);
    }

    #[test]
    fn nudge_fires_at_threshold_and_resets_counter() {
        let mut t = TodoTracker::default();
        let p = phases(&[("A", TodoStatus::InProgress)]);
        for _ in 0..TODO_NUDGE_THRESHOLD - 1 {
            t.record_tool_call("bash");
        }
        assert!(t.take_nudge(&p).is_none(), "阈值前不得触发");
        t.record_tool_call("write");
        let msg = t.take_nudge(&p).expect("达到阈值应触发");
        assert!(msg.contains("1 个 todo 项未完成"));
        assert_eq!(t.nudges_sent(), 1);
        // 计数已清零：立刻再问不得触发
        assert!(t.take_nudge(&p).is_none());
    }

    #[test]
    fn nudge_skipped_when_no_open_items_or_empty_list() {
        let mut t = TodoTracker::default();
        for _ in 0..TODO_NUDGE_THRESHOLD {
            t.record_tool_call("bash");
        }
        let done = phases(&[("A", TodoStatus::Completed)]);
        assert!(t.take_nudge(&done).is_none(), "全部完成时不提醒");
        assert!(t.take_nudge(&[]).is_none(), "空列表不提醒");
        let abandoned = phases(&[("A", TodoStatus::Abandoned)]);
        assert!(t.take_nudge(&abandoned).is_none(), "全 abandoned 不提醒");
    }

    #[test]
    fn nudge_capped_per_turn() {
        let mut t = TodoTracker::default();
        let p = phases(&[("A", TodoStatus::Pending)]);
        for i in 0..TODO_MAX_NUDGES_PER_TURN {
            for _ in 0..TODO_NUDGE_THRESHOLD {
                t.record_tool_call("bash");
            }
            assert!(t.take_nudge(&p).is_some(), "第 {} 次应在额度内", i + 1);
        }
        for _ in 0..TODO_NUDGE_THRESHOLD {
            t.record_tool_call("bash");
        }
        assert!(t.take_nudge(&p).is_none(), "额度用尽后不再提醒");
    }

    #[test]
    fn todo_call_resets_nudge_counter() {
        let mut t = TodoTracker::default();
        let p = phases(&[("A", TodoStatus::Pending)]);
        for _ in 0..TODO_NUDGE_THRESHOLD - 1 {
            t.record_tool_call("bash");
        }
        t.record_tool_call("todo");
        assert!(t.take_nudge(&p).is_none(), "todo 调用后计数清零");
    }

    #[test]
    fn stop_reminder_lists_incomplete_and_caps_at_three() {
        let mut t = TodoTracker::default();
        let p = phases(&[
            ("A", TodoStatus::Completed),
            ("B", TodoStatus::InProgress),
            ("C", TodoStatus::Pending),
        ]);
        for i in 0..TODO_MAX_STOP_REMINDERS_PER_TURN {
            let msg = t
                .take_stop_reminder(&p, false)
                .unwrap_or_else(|| panic!("第 {} 次应提醒", i + 1));
            assert!(msg.contains("2 incomplete todo item(s)"));
            assert!(msg.contains("- B"));
            assert!(msg.contains("- C"));
            assert!(!msg.contains("- A"), "已完成项不应列出");
        }
        assert_eq!(t.stop_reminders_sent(), TODO_MAX_STOP_REMINDERS_PER_TURN);
        assert!(
            t.take_stop_reminder(&p, false).is_none(),
            "每 turn 最多 3 次"
        );
    }

    #[test]
    fn stop_reminder_skipped_when_waiting_on_user() {
        let mut t = TodoTracker::default();
        let p = phases(&[("A", TodoStatus::Pending)]);
        // 挂起的选择题（阻塞 ask）
        assert!(t.take_stop_reminder(&p, true).is_none());
        // 本 turn 调过 ask（fire-and-forget）
        t.record_tool_call("ask");
        assert!(t.take_stop_reminder(&p, false).is_none());
    }

    #[test]
    fn stop_reminder_skipped_when_all_done_or_empty() {
        let mut t = TodoTracker::default();
        let done = phases(&[("A", TodoStatus::Completed), ("B", TodoStatus::Abandoned)]);
        assert!(t.take_stop_reminder(&done, false).is_none());
        assert!(t.take_stop_reminder(&[], false).is_none());
    }
}
