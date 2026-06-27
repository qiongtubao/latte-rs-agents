//! Round-robin peer discussion scheduler + H2-tagged plan.md slicing.

use std::sync::Arc;
use tokio::sync::Mutex;
use crate::session::{SessionError, SessionManager};

/// Return the substring of `plan_md` that `role_id` should see this turn.
///
/// Slice rule: every H2 section whose header starts with `<role_id> `,
/// PLUS everything before the first `## ` H2 (the initial-prompt
/// section). If no H2 header starts with `<role_id> `, return the full
/// plan_md (defensive default for unknown roles).
pub fn plan_md_slice_for(plan_md: &str, role_id: &str) -> String {
    if plan_md.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    let mut matched_any = false;
    let mut current_section: Option<(String, String)> = None;

    for line in plan_md.lines() {
        if let Some(header) = line.strip_prefix("## ") {
            // Flush previous section, if any.
            if let Some((h, b)) = current_section.take() {
                if h.split_whitespace().next() == Some(role_id) {
                    matched_any = true;
                    out.push_str(&format!("## {}\n{}\n", h, b));
                }
            }
            current_section = Some((header.to_string(), String::new()));
        } else if let Some((_, ref mut body)) = current_section {
            body.push_str(line);
            body.push('\n');
        } else {
            // Lines before the first H2 belong to the initial-prompt prelude.
            out.push_str(line);
            out.push('\n');
        }
    }
    // Flush trailing section.
    if let Some((h, b)) = current_section {
        if h.split_whitespace().next() == Some(role_id) {
            matched_any = true;
            out.push_str(&format!("## {}\n{}\n", h, b));
        }
    }

    if !matched_any {
        return plan_md.to_string();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use crate::session::SessionManager;

    #[test]
    fn extracts_matching_h2() {
        let plan = "# Task\n\ninitial prompt here\n\n## programmer round 1\nfoo work\n\n## reviewer round 1\nbar review\n";
        let s = plan_md_slice_for(plan, "programmer");
        assert!(s.contains("initial prompt here"), "top-of-file must be included: {}", s);
        assert!(s.contains("## programmer round 1"), "own H2 must be included: {}", s);
        assert!(s.contains("foo work"), "own body must be included: {}", s);
        assert!(!s.contains("## reviewer round 1"), "other role's H2 must NOT be included: {}", s);
        assert!(!s.contains("bar review"), "other role's body must NOT be included: {}", s);
    }

    #[test]
    fn unknown_role_returns_full() {
        let plan = "# Task\n\ninit\n\n## programmer round 1\nfoo\n";
        let s = plan_md_slice_for(plan, "ghost");
        assert_eq!(s, plan);
    }

    #[test]
    fn empty_plan_returns_empty() {
        assert_eq!(plan_md_slice_for("", "programmer"), "");
    }

    #[test]
    fn alphabetical_order_with_manager_last() {
        // Build a SessionManager with roles [manager, programmer, reviewer].
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new(
            "t",
            dir.path().to_path_buf(),
            vec!["manager".into(), "programmer".into(), "reviewer".into()],
        );
        let rs = RoundScheduler::new(Arc::new(tokio::sync::Mutex::new(mgr))).unwrap();
        assert_eq!(
            rs.order,
            vec!["programmer".to_string(), "reviewer".to_string(), "manager".to_string()]
        );
    }

    #[test]
    fn empty_roles_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new("t", dir.path().to_path_buf(), vec![]);
        let res = RoundScheduler::new(Arc::new(tokio::sync::Mutex::new(mgr)));
        assert!(res.is_err());
    }

    #[test]
    fn config_max_rounds_zero_means_v1_compat() {
        // RoundSchedulerConfig { max_rounds: 0, .. } is valid (not an error).
        // The scheduler itself does not enforce this; the REPL driver does
        // (it picks manager-dispatch vs round-robin based on max_rounds).
        let cfg = RoundSchedulerConfig { max_rounds: 0, session_token_budget: 0 };
        assert_eq!(cfg.max_rounds, 0);
    }

    #[test]
    fn default_config_is_10_rounds_50k_tokens() {
        let cfg = RoundSchedulerConfig::default();
        assert_eq!(cfg.max_rounds, 10);
        assert_eq!(cfg.session_token_budget, 50_000);
    }

    #[test]
    fn default_order_only_manager_is_just_manager() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new("t", dir.path().to_path_buf(), vec!["manager".into()]);
        let rs = RoundScheduler::new(Arc::new(tokio::sync::Mutex::new(mgr))).unwrap();
        assert_eq!(rs.order, vec!["manager".to_string()]);
    }
}

/// Drives the round-robin peer discussion. One `run_round` call
/// invokes every role once, in stable order (alphabetical by role_id
/// with manager last).
///
/// The actual agent invocation is done in the REPL driver (chat.rs);
/// the scheduler is responsible for ordering, supervision, and
/// session lifecycle. This split keeps the scheduler free of
/// LLM-API coupling and easy to unit-test.
pub struct RoundScheduler {
    pub session: Arc<Mutex<SessionManager>>,
    pub order: Vec<String>,
    pub max_rounds: u32,
    pub supervisor: crate::supervisor::Supervisor,
}

#[derive(Debug, Clone)]
pub struct RoundSchedulerConfig {
    pub max_rounds: u32,
    pub session_token_budget: u32,
}

impl Default for RoundSchedulerConfig {
    fn default() -> Self {
        Self { max_rounds: 10, session_token_budget: 50_000 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentOutcome {
    Completed,
    PausedBySupervisor(String),
    PausedByAskHuman(String),
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct RunSummary {
    pub rounds_completed: u32,
    pub total_tokens: u32,
    pub outcomes: Vec<(String, AgentOutcome)>,
}

impl RoundScheduler {
    pub fn new(session: Arc<Mutex<SessionManager>>) -> Result<Self, SessionError> {
        let order = {
            let mgr = session.blocking_lock();
            let mut roles: Vec<String> = mgr.record().roles.iter().map(|r| r.role_id.clone()).collect();
            if roles.is_empty() {
                return Err(SessionError::UnknownRole("empty role list".into()));
            }
            roles.sort();
            // Move "manager" to the end (if present and there's more than one role).
            if roles.len() > 1 {
                if let Some(idx) = roles.iter().position(|r| r == "manager") {
                    let m = roles.remove(idx);
                    roles.push(m);
                }
            }
            roles
        };
        let config = RoundSchedulerConfig::default();
        let supervisor = crate::supervisor::Supervisor::new(
            crate::supervisor::SupervisorConfig {
                session_token_budget: config.session_token_budget,
                dead_loop_window: 3,
            }
        );
        Ok(Self {
            session,
            order,
            max_rounds: config.max_rounds,
            supervisor,
        })
    }
}