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
}
