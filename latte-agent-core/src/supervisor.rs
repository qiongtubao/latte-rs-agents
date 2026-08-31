//! Session-level Supervisor: token budget + dead-loop watchdog.

use std::collections::{HashMap, VecDeque};

#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    pub session_token_budget: u32,
    pub dead_loop_window: usize,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self { session_token_budget: 50_000, dead_loop_window: 3 }
    }
}

pub struct Supervisor {
    config: SupervisorConfig,
    total_tokens: u32,
    history: HashMap<String, VecDeque<String>>,
}

impl Supervisor {
    pub fn new(config: SupervisorConfig) -> Self {
        Self { config, total_tokens: 0, history: HashMap::new() }
    }

    pub fn total_tokens(&self) -> u32 { self.total_tokens }

    /// Record a turn's outcome. Returns Some(reason) if the supervisor
    /// decides to auto-pause, None otherwise.
    ///
    /// `tokens_used` 必须是**本轮增量**，不是调用方的累计值 —— 这里
    /// 做的是 `+=`。传累计值会让总量按 O(N²) 膨胀（第 N 轮把前 N-1 轮
    /// 重复加一次），名义预算远早于阈值就被打爆。多角色 driver 曾经就
    /// 是传 `runner.total_usage()`（该 runner 的累计量）。
    pub fn observe(&mut self, role_id: &str, tokens_used: u32, decision: &str) -> Option<String> {
        self.total_tokens = self.total_tokens.saturating_add(tokens_used);

        // Token budget trigger
        if self.config.session_token_budget > 0 && self.total_tokens > self.config.session_token_budget {
            return Some(format!(
                "token budget exceeded ({} / {})",
                self.total_tokens, self.config.session_token_budget
            ));
        }

        // Dead-loop trigger
        let buf = self.history.entry(role_id.to_string()).or_insert_with(VecDeque::new);
        buf.push_back(decision.to_string());
        while buf.len() > self.config.dead_loop_window {
            buf.pop_front();
        }
        if buf.len() == self.config.dead_loop_window && buf.iter().all(|d| d == decision) {
            return Some(format!(
                "dead loop: {} repeating {}",
                role_id, decision
            ));
        }

        None
    }

    /// Clear the per-role dead-loop history. Called by
    /// SessionManager::resume() in Phase 4.
    /// Does NOT reset total_tokens.
    pub fn reset_history(&mut self) {
        self.history.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_budget_triggers_at_threshold() {
        let mut s = Supervisor::new(SupervisorConfig { session_token_budget: 100, dead_loop_window: 3 });
        s.observe("manager", 40, "text");
        s.observe("manager", 40, "text");
        let third = s.observe("manager", 40, "text");
        assert!(third.is_some(), "third observation should trigger token budget");
        let reason = third.unwrap();
        assert!(reason.contains("token budget"), "reason: {}", reason);
    }

    #[test]
    fn dead_loop_triggers_after_N_repeats() {
        let mut s = Supervisor::new(SupervisorConfig { session_token_budget: 0, dead_loop_window: 3 });
        s.observe("programmer", 10, "tool_call:write:abc12345");
        s.observe("programmer", 10, "tool_call:write:abc12345");
        s.observe("programmer", 10, "tool_call:write:abc12345");
        let fourth = s.observe("programmer", 10, "tool_call:write:abc12345");
        assert!(fourth.is_some(), "4th same-decision should trigger dead loop");
        let reason = fourth.unwrap();
        assert!(reason.contains("dead loop"), "reason: {}", reason);
        assert!(reason.contains("programmer"), "reason must name the role: {}", reason);
    }

    #[test]
    fn reset_history_clears_dead_loop_state() {
        let mut s = Supervisor::new(SupervisorConfig { session_token_budget: 0, dead_loop_window: 3 });
        s.observe("reviewer", 5, "text");
        s.observe("reviewer", 5, "text");
        s.observe("reviewer", 5, "text");
        s.reset_history();
        let r = s.observe("reviewer", 5, "text");
        assert!(r.is_none(), "after reset_history the loop counter is fresh");
    }

    /// `observe` 的入参契约是**增量**。这里锁死「传累计值会怎样」——
    /// 多角色 driver 曾经传的就是 `runner.total_usage()`（累计量），
    /// 于是预算按 O(N²) 提前打爆。
    ///
    /// 同一组真实消耗（每轮 30），按增量喂到第 3 轮才刚好 90 < 100 不
    /// 触发；按累计量喂（30/60/90）在第 2 轮就累到 90、第 3 轮 180，
    /// 提前一整轮误报。
    #[test]
    fn observe_takes_a_delta_not_a_running_total() {
        let cfg = || SupervisorConfig { session_token_budget: 100, dead_loop_window: 99 };

        // 正确用法：每轮传增量。真实总消耗 90，不该触发。
        let mut delta = Supervisor::new(cfg());
        for _ in 0..3 {
            assert!(
                delta.observe("manager", 30, "text").is_none(),
                "真实消耗 90 < 预算 100，不该触发"
            );
        }
        assert_eq!(delta.total_tokens(), 90);

        // 旧的错误用法：传累计值。同样的真实消耗被算成 180。
        let mut cumulative = Supervisor::new(cfg());
        assert!(cumulative.observe("manager", 30, "text").is_none());
        assert!(cumulative.observe("manager", 60, "text").is_none());
        let third = cumulative.observe("manager", 90, "text");
        assert!(
            third.is_some(),
            "传累计值会在真实消耗才 90 时误报超支（这正是旧 bug）"
        );
        assert_eq!(cumulative.total_tokens(), 180, "30 + 60 + 90，而真实只花了 90");
    }
}
