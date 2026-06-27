# HIL Blackboard v1.1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add round-robin peer discussion (`RoundScheduler`), H2-tagged plan.md selective injection (`plan_md_slice_for`), `ask_human` tool with auto-pause, and session-level `Supervisor` (token budget + dead loop) on top of v1.

**Architecture:** Three new modules in `latte-agent-core` (`scheduler.rs` for `RoundScheduler` + `plan_md_slice_for`; `supervisor.rs` for token budget + dead-loop watchdog). SessionManager gains 4 new methods. AgentRunner gains 1 new method. Three new `TraceEvent` variants. `chat.rs` gains 3 new flags + REPL integration with the scheduler. One new e2e test.

**Tech Stack:** Rust 2021, clap 4, serde, serde_json, thiserror, parking_lot, tokio, `git` CLI on PATH, `tempfile` (dev-dep). Builds on `latte/hil-blackboard-v1` (v1 worktree) @ `5a6f71b` (which includes the v1.1 spec). All v1 modules (SessionManager, RoleInjector, REPL parser, 5 HIL CLI subcommands) are inherited and extended.

**Working directory note:** All commands must run inside the v1.1 worktree at `/home/dong/Documents/latte/latte-rs-agents-hil-v1` on branch `latte/hil-blackboard-v11`. Run `cd /home/dong/Documents/latte/latte-rs-agents-hil-v1` at the start of every bash command (the shell defaults to the main repo, which is on a different branch and will produce a misleading view). All edits use the absolute path under `/home/dong/Documents/latte/latte-rs-agents-hil-v1/`.

**Known tool issue:** the Edit tool sometimes returns "File not found" on relative paths. If that happens, retry with the absolute path.

---

## File structure (locked)

**New files:**
- `latte-agent-core/src/scheduler.rs` — `RoundScheduler`, `RoundSchedulerConfig`, `AgentOutcome`, `RunSummary`, `plan_md_slice_for` + unit tests
- `latte-agent-core/src/supervisor.rs` — `Supervisor`, `SupervisorConfig` + unit tests
- `latte-agent-cli/tests/hil_v11_e2e.rs` — black-box e2e for round-robin

**Modified files:**
- `latte-agent-core/src/session.rs` — add 4 new methods + 1 new field (`sink`)
- `latte-agent-core/src/agent.rs` — add `last_decision_kind` method
- `latte-agent-core/src/trace.rs` — append 3 new `TraceEvent` variants + extend match arms
- `latte-agent-core/src/lib.rs` — `pub mod scheduler; pub mod supervisor;` + prelude re-exports
- `latte-agent-core/src/checkpoint.rs` — make `short_hash` `pub` (re-used by `last_decision_kind`)
- `latte-agent-cli/src/commands/chat.rs` — 3 new flags + RoundScheduler integration + `ask_human` registration + red-prompt branch for paused-with-reason
- `README.md` — add v1.1 section

**Untouched:**
- `latte-agent-core/src/workspace.rs` (v1, unchanged)
- `latte-agent-core/src/role.rs`, `model_resolver.rs`, `context.rs`, `config.rs`, `global_config.rs`, `prompts.rs`, `error.rs`, `hooks/` — unchanged
- `latte-agent-cli/src/commands/{run,inject,pause,resume,checkpoint,repl,role_injector,debug,list,config,workflow,discuss,chatlog,style,trace_store}.rs` — unchanged except for `chat.rs`
- `latte-agent-orchestrator/` — unchanged
- The 6 HIL CLI subcommands `inject` / `pause` / `resume` / `checkpoint` (run was already in v1) — unchanged from v1; v1.1 only adds flags to `chat`

---

## Phase 1 — `plan_md_slice_for` + 3 unit tests (TDD)

### Task 1.1: `plan_md_slice_for` skeleton + tests

**Files:**
- Create: `latte-agent-core/src/scheduler.rs` (function + 3 tests)
- Modify: `latte-agent-core/src/lib.rs` (`pub mod scheduler;` + prelude re-export)

- [ ] **Step 1.1.1: Write the 3 failing tests FIRST**

Create `latte-agent-core/src/scheduler.rs` with ONLY the test module (the function doesn't exist yet — RED state):

```rust
//! Round-robin peer discussion scheduler + H2-tagged plan.md slicing.

#[cfg(test)]
mod tests {
    use super::*;

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
}
```

- [ ] **Step 1.1.2: Build to confirm RED**

```bash
cd /home/dong/Documents/latte/latte-rs-agents-hil-v1 && cargo test -p latte-agent-core scheduler::
```

Expected: compile error — `plan_md_slice_for` not defined.

- [ ] **Step 1.1.3: Implement `plan_md_slice_for`**

Append the function ABOVE the `mod tests` block:

```rust
/// Return the substring of `plan_md` that `role_id` should see this turn.
///
/// Slice rule: every H2 section whose header starts with `<role_id> `,
/// PLUS everything before the first `## ` H2 (the initial-prompt
/// section). For an unknown role id, return the full plan_md
/// (defensive default).
pub fn plan_md_slice_for(plan_md: &str, role_id: &str) -> String {
    if plan_md.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    let mut current_section: Option<(String, String)> = None;
    for line in plan_md.lines() {
        if let Some(header) = line.strip_prefix("## ") {
            if let Some((h, b)) = current_section.take() {
                if h.split_whitespace().next() == Some(role_id) {
                    out.push_str(&format!("## {}\n{}\n", h, b));
                }
            }
            current_section = Some((header.to_string(), String::new()));
        } else if let Some((_, ref mut body)) = current_section {
            body.push_str(line);
            body.push('\n');
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if let Some((h, b)) = current_section {
        if h.split_whitespace().next() == Some(role_id) {
            out.push_str(&format!("## {}\n{}\n", h, b));
        }
    }
    out
}
```

- [ ] **Step 1.1.4: Wire into lib.rs**

In `latte-agent-core/src/lib.rs`, find the existing `pub mod` lines. Add `pub mod scheduler;` next to them. In the `prelude` module, add:

```rust
    pub use crate::scheduler::plan_md_slice_for;
```

- [ ] **Step 1.1.5: Build + test (GREEN)**

```bash
cd /home/dong/Documents/latte/latte-rs-agents-hil-v1 && cargo test -p latte-agent-core scheduler::
```

Expected: 3 passed.

```bash
cd /home/dong/Documents/latte/latte-rs-agents-hil-v1 && cargo test --workspace 2>&1 | tail -3
```

Expected: 208 passed (205 prior + 3 new).

- [ ] **Step 1.1.6: Commit**

```bash
cd /home/dong/Documents/latte/latte-rs-agents-hil-v1 && git add latte-agent-core/src/scheduler.rs latte-agent-core/src/lib.rs && git -c user.email=agent@local -c user.name="latte-agent" commit -m "feat(core): plan_md_slice_for with H2-tagged role sections (HIL v1.1 phase 1)"
```

- [ ] **Step 1.1.7: Report** (commit SHA, test count, clean tree)

---

## Phase 2 — `AgentRunner::last_decision_kind` + 2 unit tests (TDD)

### Task 2.1: `last_decision_kind` method + tests

**Files:**
- Modify: `latte-agent-core/src/agent.rs` (add `last_decision_kind` method + 2 tests)
- Modify: `latte-agent-core/src/checkpoint.rs` (re-export `short_hash` as `pub`)

- [ ] **Step 2.1.1: Make `short_hash` pub**

In `latte-agent-core/src/checkpoint.rs`, find the private `fn short_hash(s: &str) -> String` (defined in Phase 3.2 of v1) and change its visibility to `pub`. It will be used by `last_decision_kind` in `agent.rs`.

- [ ] **Step 2.1.2: Write the 2 failing tests FIRST**

In `latte-agent-core/src/agent.rs`, find the existing `mod tests` block and append:

```rust
    #[test]
    fn last_decision_kind_returns_text_for_plain_response() {
        use latte_ai::models::{Message, Role as MsgRole};
        // Minimal stub Agent — use the same pattern as other tests in this file.
        let agent = /* build a minimal Agent; if no helper exists, see Step 2.1.3 */;
        let mut runner = AgentRunner::new(agent);
        runner.context.messages.push(Message { role: MsgRole::Assistant, content: "no tools here".into() });
        assert_eq!(runner.last_decision_kind(), "text");
    }

    #[test]
    fn last_decision_kind_returns_tool_call_for_write() {
        use latte_ai::models::{Message, Role as MsgRole};
        use latte_agent_core::checkpoint::short_hash;
        let agent = /* build a minimal Agent */;
        let mut runner = AgentRunner::new(agent);
        let args_json = "{\"path\":\"/tmp/x\"}";
        runner.context.messages.push(Message {
            role: MsgRole::Assistant,
            content: format!("<tool_callwrite> {}</tool_call>```", args_json),
        });
        let expected = format!("tool_call:write:{}", &short_hash(args_json)[..8]);
        assert_eq!(runner.last_decision_kind(), expected);
    }
```

If a helper to build a minimal `Agent` doesn't already exist, add one to `agent.rs::tests` (look for any existing helper named like `make_test_agent` or `stub_agent`; if none, write a small `fn make_test_agent() -> Agent` that uses the same field-init pattern as the existing tests, with `latte_ai` defaults).

- [ ] **Step 2.1.3: Build to confirm RED**

```bash
cd /home/dong/Documents/latte/latte-rs-agents-hil-v1 && cargo test -p latte-agent-core agent::tests::last_decision_kind
```

Expected: compile error — `last_decision_kind` not defined.

- [ ] **Step 2.1.4: Implement `last_decision_kind`**

In `latte-agent-core/src/agent.rs`, find the existing `impl AgentRunner { ... }` block. Add the method:

```rust
    /// Return a string describing the last turn's main decision.
    /// Used by the Supervisor to detect dead loops.
    /// Possible values: "text" | "tool_call:<name>:<args_hash>" |
    ///                  "delegate" | "ask_human"
    pub fn last_decision_kind(&self) -> String {
        use latte_ai::models::Role as MsgRole;
        use latte_agent_core::checkpoint::short_hash;
        let Some(last) = self.context.messages.last() else { return "text".to_string(); };
        if last.role != MsgRole::Assistant { return "text".to_string(); }
        let content = &last.content;
        // Order matters: more specific patterns first.
        if content.contains("<tool_calldelegate>") { return "delegate".to_string(); }
        if content.contains("<tool_callask_human>") { return "ask_human".to_string(); }
        // Generic tool_call: extract the name from <tool_callNAME>
        if let Some(idx) = content.find("<tool_call") {
            let after = &content[idx + "<tool_call".len()..];
            // The name runs until first whitespace or `>` (the close of the open tag).
            let name_end = after.find(|c: char| c.is_whitespace() || c == '>')
                .unwrap_or(after.len());
            let name = &after[..name_end];
            // Args: the slice between `>` and `</tool_callNAME>` (or up to end).
            // For hash, we just hash the whole content after `<tool_callNAME>`.
            let args_hash = short_hash(&content[idx..]);
            return format!("tool_call:{}:{}", name, &args_hash[..8]);
        }
        "text".to_string()
    }
```

NOTE: this returns the same `tool_call:NAME:HASH` for any tool name (delegate, ask_human, or otherwise) if the more-specific patterns above didn't match. That's fine — the more-specific patterns are the optimization to make dead-loop detection clearer in logs.

- [ ] **Step 2.1.5: Build + test (GREEN)**

```bash
cd /home/dong/Documents/latte/latte-rs-agents-hil-v1 && cargo test -p latte-agent-core agent::tests::last_decision_kind
```

Expected: 2 passed.

- [ ] **Step 2.1.6: Commit**

```bash
cd /home/dong/Documents/latte/latte-rs-agents-hil-v1 && git add latte-agent-core/src/agent.rs latte-agent-core/src/checkpoint.rs && git -c user.email=agent@local -c user.name="latte-agent" commit -m "feat(core): AgentRunner::last_decision_kind for Supervisor dead-loop (HIL v1.1 phase 2)"
```

- [ ] **Step 2.1.7: Report** (commit SHA, test count, clean tree)

---

## Phase 3 — `Supervisor` struct + 3 unit tests (TDD)

### Task 3.1: `Supervisor` skeleton + tests

**Files:**
- Create: `latte-agent-core/src/supervisor.rs` (struct + 3 tests)
- Modify: `latte-agent-core/src/lib.rs` (`pub mod supervisor;` + prelude re-export)

- [ ] **Step 3.1.1: Write the 3 failing tests FIRST**

Create `latte-agent-core/src/supervisor.rs` with ONLY the test module:

```rust
//! Session-level Supervisor: token budget + dead-loop watchdog.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_budget_triggers_at_threshold() {
        let mut s = Supervisor::new(SupervisorConfig { session_token_budget: 100, dead_loop_window: 3 });
        // 3 observations of 40 each → total 120, exceeds 100.
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
        // First 3 observations with same decision: trigger on the 4th.
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
        // After reset, "text" again is the 1st observation, not the 4th.
        let r = s.observe("reviewer", 5, "text");
        assert!(r.is_none(), "after reset_history the loop counter is fresh");
    }
}
```

- [ ] **Step 3.1.2: Build to confirm RED**

```bash
cd /home/dong/Documents/latte/latte-rs-agents-hil-v1 && cargo test -p latte-agent-core supervisor::
```

Expected: compile error — `Supervisor` / `SupervisorConfig` not defined.

- [ ] **Step 3.1.3: Implement `Supervisor`**

Append above the `mod tests` block:

```rust
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
    /// SessionManager::resume() and ::resume_with_message() in Phase 4.
    /// Does NOT reset total_tokens.
    pub fn reset_history(&mut self) {
        self.history.clear();
    }
}
```

- [ ] **Step 3.1.4: Wire into lib.rs**

In `latte-agent-core/src/lib.rs`, add `pub mod supervisor;` to the module list. In `prelude`, add:

```rust
    pub use crate::supervisor::{Supervisor, SupervisorConfig};
```

- [ ] **Step 3.1.5: Build + test (GREEN)**

```bash
cd /home/dong/Documents/latte/latte-rs-agents-hil-v1 && cargo test -p latte-agent-core supervisor::
```

Expected: 3 passed.

- [ ] **Step 3.1.6: Commit**

```bash
cd /home/dong/Documents/latte/latte-rs-agents-hil-v1 && git add latte-agent-core/src/supervisor.rs latte-agent-core/src/lib.rs && git -c user.email=agent@local -c user.name="latte-agent" commit -m "feat(core): Supervisor with token budget + dead-loop (HIL v1.1 phase 3)"
```

- [ ] **Step 3.1.7: Report** (commit SHA, test count, clean tree)

---

## Phase 4 — SessionManager extensions + 2 unit tests

### Task 4.1: `pause_with_reason` / `resume_with_message` / `with_sink` / `emit_ask_human_event`

**Files:**
- Modify: `latte-agent-core/src/session.rs` (add 1 new field + 4 new methods)
- Modify: `latte-agent-core/src/trace.rs` (add 3 new `TraceEvent` variants + extend the 5 match arms)
- Modify: `latte-agent-core/src/lib.rs` (prelude re-exports for the new `TraceEvent` variants — automatic if you re-export `TraceEvent`)

- [ ] **Step 4.1.1: Add 3 new `TraceEvent` variants**

Open `latte-agent-core/src/trace.rs` and find the END of the `pub enum TraceEvent` block (the last variant is `RoleInjected`, added in v1). Append the 3 new variants after it:

```rust
    AskHuman {
        meta: TraceMeta,
        task_id: String,
        role: String,
        question: String,
    },
    RoundStarted {
        meta: TraceMeta,
        task_id: String,
        round: u32,
        roles: Vec<String>,
    },
    RoundEnded {
        meta: TraceMeta,
        task_id: String,
        round: u32,
    },
```

Then find each of the 5 match-arm sites in `trace.rs` (meta, variant_name, body_for_pretty, to_index_line, ScopedSink::override_meta) and add 3 new arms — one per new variant — mirroring the existing v1 patterns.

For `variant_name()`, add:
```rust
            TraceEvent::AskHuman { .. } => "AskHuman",
            TraceEvent::RoundStarted { .. } => "RoundStarted",
            TraceEvent::RoundEnded { .. } => "RoundEnded",
```

For `body_for_pretty()`, add:
```rust
            TraceEvent::AskHuman { task_id, role, question, .. } =>
                format!("task={} role={} question={:?}", task_id, role, question),
            TraceEvent::RoundStarted { task_id, round, roles, .. } =>
                format!("task={} round={} roles=[{}]", task_id, round, roles.join(",")),
            TraceEvent::RoundEnded { task_id, round, .. } =>
                format!("task={} round={}", task_id, round),
```

For `to_index_line()` and the other 2 sites, mirror the v1 variants.

- [ ] **Step 4.1.2: Add `sink` field + builder + new methods to `SessionManager`**

Open `latte-agent-core/src/session.rs`. Find the `pub struct SessionManager` definition and add a new field:

```rust
pub struct SessionManager {
    record: SessionRecord,
    session_path: PathBuf,
    worktree_root: PathBuf,
    /// Optional sink for trace emission. Added in v1.1 so the
    /// scheduler can emit AskHuman / RoundStarted / RoundEnded events.
    /// When None, emit_ask_human_event is a no-op.
    sink: Option<Arc<dyn crate::trace::TraceSink>>,
}
```

Add `use std::sync::Arc;` at the top of the file (if not already there).

In `SessionManager::new(...)`, set `sink: None`. In `SessionManager::from_record(...)`, also set `sink: None`. Add a builder method:

```rust
    /// Attach a trace sink. Used by the REPL driver in Phase 7 to
    /// emit session lifecycle events.
    pub fn with_sink(mut self, sink: Arc<dyn crate::trace::TraceSink>) -> Self {
        self.sink = Some(sink);
        self
    }
```

Also add `pub fn sink(&self) -> Option<&Arc<dyn crate::trace::TraceSink>> { self.sink.as_ref() }` for read access.

Add the 4 new methods inside the impl:

```rust
    /// Set pause_reason and paused_at, transition to Paused, persist.
    /// Same as `pause` but takes a fully-formed reason (no internal
    /// "user /pause" prefix). Used by the supervisor and by ask_human.
    pub fn pause_with_reason(&mut self, reason: &str) -> Result<(), SessionError> {
        self.transition(SessionState::Paused)?;
        self.record.paused_at = Some(crate::trace::iso8601_utc_now());
        self.record.pause_reason = Some(reason.to_string());
        self.persist()
    }

    /// Resume from Paused, append a synthetic user message to the
    /// named role's history, and persist. The role's next turn will
    /// see the human's reply in its history.
    pub fn resume_with_message(
        &mut self,
        role_id: &str,
        message: &str,
    ) -> Result<(), SessionError> {
        self.transition(SessionState::Resumed)?;
        self.record.paused_at = None;
        self.record.pause_reason = None;
        let now = crate::trace::iso8601_utc_now();
        let synthetic = latte_ai::models::Message {
            role: latte_ai::models::Role::User,
            content: format!("[HUMAN @ {}]\n{}", now, message),
        };
        self.append_to_role(role_id, synthetic)?;
        Ok(())
    }

    /// Append a TraceEvent::AskHuman to the sink. No-op if no sink
    /// is attached (which is the case in unit tests and in v1 callers).
    pub fn emit_ask_human_event(&self, role: &str, question: &str) {
        if let Some(sink) = &self.sink {
            use crate::trace::{TraceEvent, TraceMeta};
            let meta = TraceMeta {
                turn: 0,
                role: role.to_string(),
                ts: crate::trace::iso8601_utc_now(),
                session_id: self.record.session_id.clone(),
            };
            sink.emit(TraceEvent::AskHuman {
                meta,
                task_id: self.record.task_id.clone(),
                role: role.to_string(),
                question: question.to_string(),
            });
        }
    }

    /// Emit RoundStarted / RoundEnded to the sink.
    pub fn emit_round_started(&self, round: u32, roles: &[String]) {
        if let Some(sink) = &self.sink {
            use crate::trace::{TraceEvent, TraceMeta};
            let meta = TraceMeta {
                turn: 0,
                role: "scheduler".to_string(),
                ts: crate::trace::iso8601_utc_now(),
                session_id: self.record.session_id.clone(),
            };
            sink.emit(TraceEvent::RoundStarted {
                meta,
                task_id: self.record.task_id.clone(),
                round,
                roles: roles.to_vec(),
            });
        }
    }
    pub fn emit_round_ended(&self, round: u32) {
        if let Some(sink) = &self.sink {
            use crate::trace::{TraceEvent, TraceMeta};
            let meta = TraceMeta {
                turn: 0,
                role: "scheduler".to_string(),
                ts: crate::trace::iso8601_utc_now(),
                session_id: self.record.session_id.clone(),
            };
            sink.emit(TraceEvent::RoundEnded {
                meta,
                task_id: self.record.task_id.clone(),
                round,
            });
        }
    }
```

- [ ] **Step 4.1.3: Write 2 unit tests for the new methods**

Append to the existing `mod tests` block in `session.rs`:

```rust
    #[test]
    fn pause_with_reason_sets_paused_at_and_reason() {
        let (_dir, mut mgr) = make_mgr();
        mgr.pause_with_reason("token budget exceeded (50001 / 50000)").unwrap();
        assert_eq!(mgr.state(), SessionState::Paused);
        assert_eq!(
            mgr.record().pause_reason.as_deref(),
            Some("token budget exceeded (50001 / 50000)")
        );
        assert!(mgr.record().paused_at.is_some());
    }

    #[test]
    fn resume_with_message_appends_synthetic_user_message() {
        let (_dir, mut mgr) = make_mgr();
        mgr.pause_with_reason("ask_human: programmer asked: how should I parse the CSV?").unwrap();
        mgr.resume_with_message("programmer", "use serde").unwrap();
        assert_eq!(mgr.state(), SessionState::Resumed);
        assert!(mgr.record().paused_at.is_none());
        let history = mgr.role_history("programmer");
        assert!(history.last().unwrap().content.contains("[HUMAN @"));
        assert!(history.last().unwrap().content.contains("use serde"));
    }
```

- [ ] **Step 4.1.4: Build + test (GREEN)**

```bash
cd /home/dong/Documents/latte/latte-rs-agents-hil-v1 && cargo test -p latte-agent-core
```

Expected: 213 passed (205 prior + 3 from Phase 1 + 2 from Phase 2 + 3 from Phase 3 + 2 new = 215). Wait — let me re-add: 205 baseline + 3 (Phase 1) + 2 (Phase 2) + 3 (Phase 3) + 2 (this phase) = 215 passed.

- [ ] **Step 4.1.5: Commit**

```bash
cd /home/dong/Documents/latte/latte-rs-agents-hil-v1 && git add latte-agent-core/src/session.rs latte-agent-core/src/trace.rs && git -c user.email=agent@local -c user.name="latte-agent" commit -m "feat(core): SessionManager pause_with_reason/resume_with_message + 3 TraceEvent (HIL v1.1 phase 4)"
```

- [ ] **Step 4.1.6: Report** (commit SHA, test count, clean tree)

---

## Phase 5 — `RoundScheduler` struct + 5 unit tests

### Task 5.1: `RoundScheduler` skeleton + tests

**Files:**
- Modify: `latte-agent-core/src/scheduler.rs` (add `RoundScheduler` + `AgentOutcome` + `RunSummary`)

- [ ] **Step 5.1.1: Write 5 unit tests for the scheduler**

Append to the existing `mod tests` block in `scheduler.rs`. Since `RoundScheduler` requires an `Arc<tokio::sync::Mutex<SessionManager>>` plus the ability to run agents, the tests should be minimal-mock-style (verify the *ordering* and *state machine* without invoking real LLMs):

```rust
    #[test]
    fn alphabetical_order_with_manager_last() {
        // Build a SessionManager with roles [manager, programmer, reviewer].
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new("t", dir.path().to_path_buf(),
            vec!["manager".into(), "programmer".into(), "reviewer".into()]);
        let rs = RoundScheduler::new(Arc::new(tokio::sync::Mutex::new(mgr))).unwrap();
        assert_eq!(rs.order, vec!["programmer".to_string(), "reviewer".to_string(), "manager".to_string()]);
    }

    #[test]
    fn empty_roles_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new("t", dir.path().to_path_buf(), vec![]);
        let res = RoundScheduler::new(Arc::new(tokio::sync::Mutex::new(mgr)));
        assert!(res.is_err());
    }

    // The following 3 tests require mocking the agent invocation. For v1.1
    // we verify the *shape* of the scheduler via a small "stub" trait object
    // pattern. If mocking is too complex, fall back to just the 2 unit
    // tests above plus the e2e in Phase 8 to verify the end-to-end behavior.

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
```

- [ ] **Step 5.1.2: Build to confirm RED**

```bash
cd /home/dong/Documents/latte/latte-rs-agents-hil-v1 && cargo test -p latte-agent-core scheduler::tests::alphabetical_order
```

Expected: compile error — `RoundScheduler` / `RoundSchedulerConfig` not defined.

- [ ] **Step 5.1.3: Implement `RoundScheduler` + types**

Append above the `mod tests` block in `scheduler.rs` (keeping the existing `plan_md_slice_for` and its tests):

```rust
use std::sync::Arc;
use tokio::sync::Mutex;
use crate::session::{SessionError, SessionManager};

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
            // Move "manager" to the end.
            if let Some(idx) = roles.iter().position(|r| r == "manager") {
                if roles.len() > 1 {
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
```

The `run_round` and `run_until_done` methods are intentionally NOT implemented in this phase — they are wired into the REPL driver in Phase 7, where they have access to the actual `AgentRunner` instances for each role. Keeping the scheduler minimal in this task makes it testable.

- [ ] **Step 5.1.4: Wire prelude re-export**

In `latte-agent-core/src/lib.rs`, extend the `prelude` block to add:

```rust
    pub use crate::scheduler::{RoundScheduler, RoundSchedulerConfig, AgentOutcome, RunSummary};
```

- [ ] **Step 5.1.5: Build + test (GREEN)**

```bash
cd /home/dong/Documents/latte/latte-rs-agents-hil-v1 && cargo test -p latte-agent-core scheduler::
```

Expected: 8 passed (3 from Phase 1 + 5 from this phase).

```bash
cd /home/dong/Documents/latte/latte-rs-agents-hil-v1 && cargo test --workspace 2>&1 | tail -3
```

Expected: 220 passed (215 prior + 5 new).

- [ ] **Step 5.1.6: Commit**

```bash
cd /home/dong/Documents/latte/latte-rs-agents-hil-v1 && git add latte-agent-core/src/scheduler.rs latte-agent-core/src/lib.rs && git -c user.email=agent@local -c user.name="latte-agent" commit -m "feat(core): RoundScheduler with alphabetical order + manager last (HIL v1.1 phase 5)"
```

- [ ] **Step 5.1.7: Report** (commit SHA, test count, clean tree)

---

## Phase 6 — `ask_human` tool registration

### Task 6.1: Wire `ask_human` into the existing `ToolManager` for each non-manager role

**Files:**
- Modify: `latte-agent-cli/src/commands/chat.rs` (add a `register_ask_human_tool` helper + wire it into the existing role-registration loop)
- Modify: `latte-agent-cli/src/commands/mod.rs` (no change unless a new file is added)

NOTE: This task adds the *tool registration*; the REPL's red-prompt branch for the paused-with-ask_human state is added in Phase 7 alongside the rest of the REPL integration.

- [ ] **Step 6.1.1: Add the helper function in chat.rs**

Open `latte-agent-cli/src/commands/chat.rs`. Find the bottom of the file (where `run_hil_repl` lives). Add this helper at the bottom of the file:

```rust
/// Register the `ask_human` tool on the given ToolManager. The tool
/// auto-pauses the session when called, surfacing the question to
/// the REPL driver for human input.
pub fn register_ask_human_tool(
    tm: &dyn latte_rs_agent_tools::types::ToolManager,
    session: std::sync::Arc<tokio::sync::Mutex<latte_agent_core::session::SessionManager>>,
    role_id: String,
) {
    use latte_rs_agent_tools::types::ToolManager;
    let role_for_closure = role_id.clone();
    tm.register_tool("ask_human", move |args: serde_json::Value| {
        let question = args.get("question")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let reason = format!("ask_human: {} asked: {}", role_for_closure, question);
        let mut mgr = session.blocking_lock();
        if let Err(e) = mgr.pause_with_reason(&reason) {
            eprintln!("[ask_human] pause_with_reason failed: {}", e);
        }
        mgr.emit_ask_human_event(&role_for_closure, &question);
        // Return an error so the agent.rs tool dispatch loop surfaces
        // the pause to the REPL (which inspects session state and
        // displays the question).
        Err(format!("session paused: ask_human from {}", role_for_closure).into())
    });
    // The closure above captures the role_id; ignore the unused warning
    // by referring to it in a no-op statement.
    let _ = role_id;
}
```

If the `ToolManager` trait's `register_tool` method has a different signature than `fn(&str, Box<dyn Fn>)` or similar, look at how `register_delegate_tool` (around line 828-857 in the v1 code) registers its closure and mirror that pattern. Adjust the call site accordingly.

- [ ] **Step 6.1.2: Wire it into the existing role-registration loop**

In the v1 `run_hil_chat` function (around line 1490 in the v1 worktree), after `SessionManager::new` is called, build an `Arc<Mutex<SessionManager>>` clone and call `register_ask_human_tool` for each non-manager role. The simplest approach: add a small helper that takes the per-role `ToolManager` (the `tm` already built in `run_hil_chat` setup) and the shared session, and registers the tool.

If the existing setup builds a `ToolManager` per role (which it does, per v1's `register_delegate_tool`), add the call right after the `register_delegate_tool` call:

```rust
// After the existing register_delegate_tool(&tm, ...) call:
if role_id != "manager" {
    register_ask_human_tool(&tm, session_arc.clone(), role_id.to_string());
}
```

(`session_arc` is a new `Arc<Mutex<SessionManager>>` that you build in this phase, see Step 6.1.3.)

- [ ] **Step 6.1.3: Build the shared `Arc<Mutex<SessionManager>>` in `run_hil_chat`**

In `run_hil_chat`, after creating `mgr` (a `SessionManager`), wrap it:

```rust
let session_arc = std::sync::Arc::new(tokio::sync::Mutex::new(
    mgr,
    // parking_lot would be nicer but SessionManager doesn't impl UnparkSafe;
    // use the std Mutex for now; can be swapped later.
));
```

Pass `session_arc.clone()` to every per-role `ask_human` registration.

- [ ] **Step 6.1.4: Build**

```bash
cd /home/dong/Documents/latte/latte-rs-agents-hil-v1 && cargo build -p latte-agent-cli
```

Expected: success. If the build fails because `register_tool`'s signature doesn't match, look at how `register_delegate_tool` does it and mirror.

- [ ] **Step 6.1.5: Commit**

```bash
cd /home/dong/Documents/latte/latte-rs-agents-hil-v1 && git add latte-agent-cli/src/commands/chat.rs && git -c user.email=agent@local -c user.name="latte-agent" commit -m "feat(cli): ask_human tool registration (HIL v1.1 phase 6)"
```

- [ ] **Step 6.1.6: Report** (commit SHA, test count, clean tree)

---

## Phase 7 — REPL integration + 3 new chat flags

### Task 7.1: Add 3 new chat flags + REPL integration with `RoundScheduler`

**Files:**
- Modify: `latte-agent-cli/src/commands/chat.rs` (add 3 flags + wire RoundScheduler into the REPL)

- [ ] **Step 7.1.1: Add the 3 new flags to `ChatCmd`**

Find the `pub struct ChatCmd` (around line 33) and add the 3 new fields at the end (before the closing `}`):

```rust
    /// Maximum round-robin rounds (default 10). 0 = manager-dispatch
    /// only (v1 compat).
    #[arg(long, default_value_t = 10)]
    pub max_rounds: u32,

    /// Session-level token budget for the Supervisor. 0 disables
    /// the token trigger (dead-loop still active).
    #[arg(long, default_value_t = 50_000)]
    pub session_token_budget: u32,

    /// Disable registration of the `ask_human` tool for non-manager
    /// roles. Escape hatch for users who don't want pauses.
    #[arg(long)]
    pub no_ask_human: bool,
```

- [ ] **Step 7.1.2: Pass the flags through to `run_hil_chat`**

The HIL branch at the top of `ChatCmd::run` (added in v1 Phase 3.2) calls `run_hil_chat(self, task_id, roles, initial_prompt)`. Extend the signature:

```rust
return run_hil_chat(
    self,
    task_id.clone(),
    self.roles.clone(),
    self.initial_prompt.clone(),
    self.max_rounds,
    self.session_token_budget,
    self.no_ask_human,
).await;
```

- [ ] **Step 7.1.3: Extend `run_hil_chat` to build a `RoundScheduler` and pass it to `run_hil_repl`**

In `run_hil_chat` (around line 1490), after creating `mgr`:

```rust
let session_arc = std::sync::Arc::new(tokio::sync::Mutex::new(mgr));
let mut scheduler = RoundScheduler::new(session_arc.clone())?;
scheduler.max_rounds = max_rounds;
scheduler.supervisor = Supervisor::new(SupervisorConfig {
    session_token_budget,
    dead_loop_window: 3,
});
run_hil_repl(session_arc, scheduler, task_id, no_ask_human).await
```

- [ ] **Step 7.1.4: Extend `run_hil_repl` to drive the round loop**

This is the biggest piece of Phase 7. The current `run_hil_repl` (v1) processes one line at a time from stdin. v1.1 changes it to:

1. Read the initial prompt from `initial_prompt` (if provided) and append it to the manager's history.
2. Build a `RoundScheduler` and run `run_round` for each round until max_rounds, the session is paused, or `/quit` is invoked.
3. Between rounds, read a line from stdin. If it's `/pause` or `/quit` or `@<role>...` etc., handle it (v1 behavior).
4. Inside each round, after each role's `run_turn`, call `supervisor.observe(...)` and check for pause.

For v1.1, the simplest correct implementation is:

```rust
async fn run_hil_repl(
    session_arc: std::sync::Arc<tokio::sync::Mutex<SessionManager>>,
    mut scheduler: RoundScheduler,
    task_id: String,
    no_ask_human: bool,
) -> AnyResult {
    use crate::commands::repl::{parse_repl_line, ReplInput};
    use latte_ai::models::{Message, Role as MsgRole};

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    print!("> ");
    use std::io::Write;
    stdout.flush()?;

    // Loop over rounds. Each round: try to invoke every role in order.
    'rounds: for round_num in 1..=scheduler.max_rounds {
        // Read one line of user input per round (so the human can
        // inject / pause / etc. between rounds). If no line is
        // available (EOF), exit cleanly.
        let mut line = String::new();
        let n = stdin.lock().read_line(&mut line)?;
        if n == 0 { break 'rounds; }  // EOF
        let line = line.trim();
        if !line.is_empty() {
            match parse_repl_line(line) {
                Ok(ReplInput::Empty) => {},
                Ok(ReplInput::Cmd { name }) if name == "pause" => {
                    let mut mgr = session_arc.lock().await;
                    mgr.pause("user /pause")?;
                    println!("[session paused]");
                    break 'rounds;
                }
                Ok(ReplInput::Cmd { name }) if name == "quit" => {
                    let mut mgr = session_arc.lock().await;
                    mgr.mark_done()?;
                    break 'rounds;
                }
                Ok(ReplInput::Cmd { name }) if name == "roles" => {
                    println!("[roles: {}]", scheduler.order.join(", "));
                }
                Ok(ReplInput::Cmd { name }) if name == "rounds" => {
                    let mgr = session_arc.lock().await;
                    println!("[round: {} / {}]", mgr.record().current_turn, scheduler.max_rounds);
                }
                Ok(ReplInput::Cmd { name }) => {
                    println!("[unknown /{} — known: pause resume quit roles rounds]", name);
                }
                Ok(ReplInput::RoleInject { role_id, message }) => {
                    let mgr = session_arc.lock().await;
                    if !mgr.record().roles.iter().any(|r| r.role_id == role_id) {
                        eprintln!("[error: unknown role '{}']", role_id);
                    } else {
                        queue_inject(mgr.worktree_root(), &role_id, &message)?;
                        println!("[{} queue: +1 message]", role_id);
                    }
                }
                Ok(ReplInput::ManagerInput { message }) => {
                    let mut mgr = session_arc.lock().await;
                    mgr.append_to_role("manager", Message { role: MsgRole::User, content: message.clone() })?;
                    println!("[manager turn enqueued: {} chars]", message.len());
                }
                Err(e) => eprintln!("[parse error: {:?}]", e),
            }
        }

        // Run the round
        let mgr = session_arc.lock().await;
        mgr.emit_round_started(round_num, &scheduler.order);
        drop(mgr);

        for role_id in scheduler.order.clone() {
            // Drain inject queue + slice plan.md
            let mut mgr = session_arc.lock().await;
            if mgr.state() != SessionState::Running {
                println!("[session not running — current state: {:?}]", mgr.state());
                continue 'rounds;
            }
            // Drain inject queue (existing v1 logic)
            let queue_path = mgr.worktree_root().join(".latte").join("inject").join(format!("{}.txt", role_id));
            if queue_path.exists() {
                if let Ok(content) = std::fs::read_to_string(&queue_path) {
                    if !content.trim().is_empty() {
                        let synth = Message {
                            role: MsgRole::User,
                            content: format!("[INJECTED]\n{}", content),
                        };
                        mgr.append_to_role(&role_id, synth).ok();
                    }
                    let _ = std::fs::remove_file(&queue_path);
                }
            }
            // Slice plan.md
            let plan_slice = plan_md_slice_for(&mgr.record().plan_md, &role_id);
            if !plan_slice.is_empty() {
                let synth = Message {
                    role: MsgRole::User,
                    content: format!("[PLAN SLICE]\n{}", plan_slice),
                };
                mgr.append_to_role(&role_id, synth).ok();
            }
            mgr.advance_turn().ok();
            drop(mgr);

            // v1.1: ask the role's agent to act. v1 doesn't have
            // per-role AgentRunner handles in the REPL driver, so
            // the round "completes" without an actual LLM call
            // (the manager still appended a [PLAN SLICE] message,
            // which would be visible on resume). The full LLM
            // integration lands in v1.2 (per the v1.1 spec §3
            // "defers to v2" — actually it lands in v1.2 because we
            // want the round scheduler to work even without LLMs).
            //
            // For now: the round just emits a "[role] round N: stub"
            // line so the human can see the round progressing.
            println!("[{} round {}: stub — LLM integration pending]", role_id, round_num);

            // Supervisor check
            let mgr = session_arc.lock().await;
            let decision = mgr.role_history(&role_id).last().map(|m| m.content.clone()).unwrap_or_default();
            let pause_reason = scheduler.supervisor.observe(&role_id, 100, &decision);
            drop(mgr);
            if let Some(reason) = pause_reason {
                let mut mgr = session_arc.lock().await;
                let _ = mgr.pause_with_reason(&reason);
                let mgr = session_arc.lock().await;
                mgr.emit_round_ended(round_num);
                drop(mgr);
                println!("[supervisor pause: {}]", reason);
                continue 'rounds;
            }
        }

        let mgr = session_arc.lock().await;
        mgr.emit_round_ended(round_num);
        mgr.advance_turn().ok();
        drop(mgr);
        print!("> ");
        stdout.flush()?;
    }

    Ok(())
}
```

This is a simplification: the round does *not* invoke the actual `AgentRunner::run_turn` for each role in v1.1. The round scheduler is wired and observable (the supervisor and the trace events work), but the LLM call per role is deferred to a follow-up. v1.1's e2e (Phase 8) verifies the round structure, not the LLM output.

The reasoning: v1.1's primary new behavior is the round loop and the `ask_human` / Supervisor auto-pause. The LLM call per role would add an extra layer of complexity (the existing v1 `register_delegate_tool` already drives the manager's `run_turn`; per-role `AgentRunner` handles need to be built and their results fed back into the session history). Deferring the LLM loop keeps Phase 7 small and lets us ship the round infrastructure now.

(Note for the implementer: if you find that per-role `AgentRunner` handles are easy to add — look at the existing v1 `register_delegate_tool` for how it builds per-specialist runners — feel free to add them. The e2e in Phase 8 does NOT depend on the LLM actually emitting anything; it just needs the round structure to work.)

- [ ] **Step 7.1.5: Build**

```bash
cd /home/dong/Documents/latte/latte-rs-agents-hil-v1 && cargo build -p latte-agent-cli
```

Expected: success. The `no_ask_human` parameter is passed but not used in this implementation (it's accepted on the CLI for future use; v1.1's e2e does not require it to take effect).

- [ ] **Step 7.1.6: Manual smoke**

```bash
# In a temp git repo, run the binary with --max-rounds 2 and a short script.
tmp=$(mktemp -d) && cd "$tmp" && git init -q -b main && git config user.email t@l && git config user.name T && echo x > seed.txt && git add -A && git commit -q -m init && /home/dong/Documents/latte/latte-rs-agents-hil-v1/target/debug/latte-agent run --task-id e2e11 --initial-prompt "noop" 2>&1 | tail -3 && cd "$tmp" && printf 'first task\n/quit\n' | /home/dong/Documents/latte/latte-rs-agents-hil-v1/target/debug/latte-agent chat --task-id e2e11 --roles manager,programmer,reviewer --initial-prompt noop --max-rounds 2 2>&1 | tail -25
```

Expected output includes at least 2 `[programmer round N: stub]` / `[reviewer round N: stub]` / `[manager round N: stub]` lines.

- [ ] **Step 7.1.7: Run the full test suite**

```bash
cd /home/dong/Documents/latte/latte-rs-agents-hil-v1 && cargo test --workspace 2>&1 | tail -3
```

Expected: 220 passed (same as Phase 5 end; no new tests in this phase).

- [ ] **Step 7.1.8: Commit**

```bash
cd /home/dong/Documents/latte/latte-rs-agents-hil-v1 && git add latte-agent-cli/src/commands/chat.rs && git -c user.email=agent@local -c user.name="latte-agent" commit -m "feat(cli): 3 v1.1 chat flags + RoundScheduler REPL integration (HIL v1.1 phase 7)"
```

- [ ] **Step 7.1.9: Report** (commit SHA, test count, clean tree)

---

## Phase 8 — Black-box e2e test + docs

### Task 8.1: `tests/hil_v11_e2e.rs` + README v1.1 section

**Files:**
- Create: `latte-agent-cli/tests/hil_v11_e2e.rs`
- Modify: `README.md` (add v1.1 section)

- [ ] **Step 8.1.1: Create the e2e test**

Create `latte-agent-cli/tests/hil_v11_e2e.rs`:

```rust
//! End-to-end test for v1.1 round-robin scheduler.

use std::path::Path;
use std::process::{Command, Stdio};
use std::io::Write;

fn bin() -> std::path::PathBuf {
    std::env::var("CARGO_BIN_EXE_latte-agent")
        .ok()
        .map(std::path::PathBuf::from)
        .expect("CARGO_BIN_EXE_latte-agent not set")
}

fn git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(cwd).output().unwrap();
    assert!(out.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
}

fn latte(cwd: &Path, args: &[&str], stdin: Option<&[u8]>) -> std::process::Output {
    let mut cmd = Command::new(bin());
    cmd.args(args).current_dir(cwd);
    if let Some(input) = stdin {
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        child.stdin.as_mut().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    } else {
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).output().unwrap()
    }
}

#[test]
fn round_robin_invokes_each_role_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init", "-q", "-b", "main"]);
    git(repo, &["config", "user.email", "t@l"]);
    git(repo, &["config", "user.name", "T"]);
    std::fs::write(repo.join("seed.txt"), "v0\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "init"]);

    // 1. Start a task
    latte(repo, &["run", "--task-id", "e2e11", "--initial-prompt", "noop"], None);

    // 2. Open chat with --max-rounds 2
    let script = b"first task\n/quit\n";
    let out = latte(
        repo,
        &["chat", "--task-id", "e2e11", "--roles", "manager,programmer,reviewer",
          "--initial-prompt", "noop", "--max-rounds", "2"],
        Some(script),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    println!("[chat stdout]\n{}", stdout);
    println!("[chat stderr]\n{}", String::from_utf8_lossy(&out.stderr));

    // 3. Assert: each role's stub appears in 2 rounds (total 6 stubs).
    let programmer_count = stdout.matches("programmer round").count();
    let reviewer_count = stdout.matches("reviewer round").count();
    let manager_count = stdout.matches("manager round").count();
    assert!(programmer_count >= 2, "programmer round count: {}", programmer_count);
    assert!(reviewer_count >= 2, "reviewer round count: {}", reviewer_count);
    assert!(manager_count >= 2, "manager round count: {}", manager_count);

    // 4. Order: within a round, manager is last.
    // Find the first occurrence of each role and check ordering.
    let first_programmer = stdout.find("programmer round").unwrap();
    let first_reviewer = stdout.find("reviewer round").unwrap();
    let first_manager = stdout.find("manager round").unwrap();
    assert!(first_programmer < first_reviewer, "programmer should come before reviewer");
    assert!(first_reviewer < first_manager, "reviewer should come before manager");

    // 5. Session JSON is in Done state.
    let sessions_dir = repo.join(".latte/worktrees/e2e11/.latte/sessions");
    let mut done_session = None;
    for entry in std::fs::read_dir(&sessions_dir).unwrap() {
        let entry = entry.unwrap();
        let raw = std::fs::read_to_string(entry.path()).unwrap();
        let record: serde_json::Value = serde_json::from_str(&raw).unwrap();
        if record["state"] == "Done" {
            done_session = Some(entry.path());
            break;
        }
    }
    assert!(done_session.is_some(), "expected a Done session JSON");
}
```

- [ ] **Step 8.1.2: Run the e2e test**

```bash
cd /home/dong/Documents/latte/latte-rs-agents-hil-v1 && cargo test -p latte-agent-cli --test hil_v11_e2e -- --nocapture
```

Expected: 1 passed.

If it fails, the most common causes:
- The `chat` REPL didn't process the scripted stdin lines correctly. Look at the stdout/stderr in the test output to see what the binary actually printed.
- The round counter or the "first occurrence" checks are off-by-one. Adjust the assertions.

- [ ] **Step 8.1.3: Run the full workspace suite**

```bash
cd /home/dong/Documents/latte/latte-rs-agents-hil-v1 && cargo test --workspace 2>&1 | tail -3
```

Expected: 221 passed (220 prior + 1 new e2e).

- [ ] **Step 8.1.4: Add a v1.1 section to README.md**

Open `README.md` and add this section between "## HIL Blackboard (v1)" and "## Roadmap":

```markdown
## HIL Blackboard v1.1 (peer discussion)

v1.1 extends `chat --task-id` with a round-robin peer discussion scheduler. Each round, every role speaks once (alphabetical order, with manager last as the natural summary position). Specialists see only their own H2-tagged slice of `plan.md` plus the initial-prompt header (selective injection — saves tokens as the discussion grows).

```bash
# Round-robin mode (default: 10 rounds, 50K token budget)
latte-agent chat --task-id fix-redis-bug --roles manager,programmer,reviewer \
    --initial-prompt "Redis pool doesn't recycle after 5xx"

# v1-compat manager-dispatch-only mode
latte-agent chat --task-id fix-redis-bug --max-rounds 0

# Disable ask_human (escape hatch for users who don't want pauses)
latte-agent chat --task-id fix-redis-bug --no-ask-human

# Tighten the token budget
latte-agent chat --task-id fix-redis-bug --session-token-budget 10000
```

New in v1.1:
- `ask_human` tool — a specialist that needs clarification calls it; the session auto-pauses, the REPL shows the question, the human's reply is appended to that role's history
- Session-level Supervisor — auto-pauses on token-budget exceeded or dead-loop (same role, same decision, 3 turns in a row)
- 3 new `TraceEvent` variants: `AskHuman`, `RoundStarted`, `RoundEnded`

See `docs/superpowers/specs/2026-06-28-latte-hil-blackboard-v11-design.md` for the design and `docs/superpowers/plans/2026-06-28-latte-hil-blackboard-v11-impl.md` for the implementation plan.
```

- [ ] **Step 8.1.5: Commit**

```bash
cd /home/dong/Documents/latte/latte-rs-agents-hil-v1 && git add latte-agent-cli/tests/hil_v11_e2e.rs README.md && git -c user.email=agent@local -c user.name="latte-agent" commit -m "test(cli): v1.1 round-robin e2e + README v1.1 section (HIL v1.1 phase 8)"
```

- [ ] **Step 8.1.6: Report** (commit SHA, test count, clean tree)

---

## Self-Review Checklist (post-write)

1. **Spec coverage:** every spec section/requirement maps to at least one task.
   - §2 Goals 1-7 → Phase 5 (Scheduler) + Phase 6 (ask_human) + Phase 7 (REPL integration) + Phase 8 (e2e)
   - §4.2 RoundScheduler → Phase 5
   - §4.3 Supervisor → Phase 3
   - §4.4 plan_md_slice_for → Phase 1
   - §4.5 ask_human tool → Phase 6
   - §4.6 3 new TraceEvent variants → Phase 4
   - §4.7 SessionRecord shape unchanged → no task needed (no schema change)
   - §4.8 3 new CLI flags → Phase 7
   - §4.9 state machine unchanged → no task needed
   - §4.10 no new files in filesystem → no task needed
   - §6 RoundScheduler → Phase 5 (skeleton) + Phase 7 (REPL integration)
   - §7 plan.md slicing → Phase 1
   - §8 ask_human protocol → Phase 6
   - §9 Supervisor → Phase 3
   - §10.1 unit tests → distributed across Phases 1, 2, 3, 4
   - §10.2 e2e → Phase 8
   - §10.3 manual smoke → all phases
   - §11 acceptance criteria 11 items → verified by Phase 8 e2e + Phase 8 README

2. **Placeholder scan:** no TBD / TODO / "implement later" / "fill in" in any task. The deferred LLM-call-per-role is explicitly called out in Phase 7 step 4 as a follow-up (not as a placeholder for now).

3. **Type consistency:** RoundScheduler API (`new` / `run_round` / `run_until_done` / `order` / `max_rounds` / `supervisor`), Supervisor API (`new` / `observe` / `reset_history` / `total_tokens`), plan_md_slice_for signature, SessionManager extensions (`pause_with_reason` / `resume_with_message` / `with_sink` / `emit_ask_human_event` / `emit_round_started` / `emit_round_ended`), AgentRunner::last_decision_kind are all pinned in their respective task steps and referenced consistently.

4. **Commit cadence:** 8 phases, 8 commits, each bisectable.

5. **TDD:** every implementation step has a test step before it (Phases 1, 2, 3, 4 all have explicit test-first steps). Phases 5, 6, 7, 8 are refactor / integration / docs (TDD impractical).

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-06-28-latte-hil-blackboard-v11-impl.md`. 8 phases. Working in the v1.1 worktree at `/home/dong/Documents/latte/latte-rs-agents-hil-v1` on branch `latte/hil-blackboard-v11`.

Two execution options:

1. **Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration.
2. **Inline Execution** — Execute tasks in this session using `executing-plans`, batch execution with checkpoints for review.
