# latte-agent: HIL Blackboard v1.1 — Peer Discussion + Selective Injection + ask_human + Supervisor

**Date:** 2026-06-28
**Status:** Draft (post-brainstorming, pre-implementation)
**Scope:** v1.1 = round-robin peer discussion scheduler (E2) + H2-tagged plan.md selective injection (C) + `ask_human` tool with auto-pause (H) + session-level Supervisor for token budget + dead-loop (I).
**Builds on:** v1 spec `2026-06-28-latte-hil-blackboard-v1-design.md` (pi-dev @ `35de34b`). All v1 modules (WorkspaceManager, CheckpointEngine, SessionManager, RoleInjector, REPL parser, 5 HIL CLI subcommands) are inherited and unchanged in shape.
**Out of scope (deferred to v2):** Cross-process / cross-worktree SessionManager; remote push / PR creation; worktree-in-worktree; semantic blackboard (pub/sub); model-tier override per role in the schedule.

## 1. Background

v1 delivered the HIL blackboard scaffold:
- `latte-agent chat --task-id X --roles a,b,c --initial-prompt Y` opens a multi-role REPL
- `/pause` and `/resume` save and restore session state via JSON
- `@<role>` lines route to a specific specialist's next turn
- Manager can `delegate` to specialists via the legacy `register_delegate_tool` plumbing

**What v1 is missing** (per spec §一 / §二.2 / §三.4 deferred to v1.1):
1. **Peer discussion** (E2): currently only manager dispatches; specialists are passive workers and never talk to each other. The "small-model collaboration" vision (spec §一) requires peers to query each other.
2. **Selective injection** (C): every specialist sees the full plan.md on every turn. Token cost grows linearly with discussion length; with 3+ roles × 5+ rounds it becomes prohibitive.
3. **ask_human** (H): a specialist that needs clarification must currently invent an answer (per the anti-hallucination guidance in `prompts/manager.md`). There is no way for an agent to halt the session and request human input.
4. **Supervisor** (I): `LoopDetector` in `agent.rs:867` only catches tool-call loops within a single specialist turn. There is no session-level watchdog for runaway token use or agents that go in circles across turns.

This spec adds the four pieces, all built on v1's infrastructure.

## 2. Goals

1. **Round-robin peer discussion.** v1.1 adds a `RoundScheduler` that drives the REPL in rounds. In each round, every role speaks exactly once (in stable order — alphabetical by role_id for v1.1, with manager last as a natural summary position). Each role's turn is one `AgentRunner::run_turn` invocation. A role can call `delegate(role=X, task=Y)` during its turn to make another role do work before that other role's own slot arrives; the delegated work appears in that other role's history before its own turn. The round ends when every role has spoken once. The session ends when `--max-rounds` rounds complete or `/pause` or `/quit` is invoked.

2. **H2-tagged plan.md selective injection.** Every write to plan.md (manager's initial prompt + each role's "## <role> round N" sections) is tagged with a predictable H2 header. `SessionRecord.plan_md` stays the full document; `RoleInjector::slice_for(worktree, role_id)` returns the substring that role should see this turn, defined as: every H2 section whose header starts with the role's id (e.g. `## programmer round 1` is programmer's), plus the top-of-file initial-prompt section (everything before the first `## ` H2). The full plan.md stays the source of truth for the human and for rollback; the slice is a view, not a copy.

3. **`ask_human` tool.** A specialist that needs clarification calls `ask_human(question=...)`. The tool does not return a value at the call site; instead, the session auto-transitions to `Paused` with `pause_reason = "ask_human: <role> asked: <question>"`, emits a `TraceEvent::AskHuman` (new variant), and the REPL prompt turns red and shows the question. The human types a reply; the reply is appended to that role's `messages` as a synthetic `Role::User` message with a `[HUMAN @ <ts>]` prefix, the session auto-`resume`s to `Resumed`, and the role's current `run_turn` is re-invoked to consume the answer.

4. **Session-level Supervisor.** A `Supervisor` struct lives in `latte-agent-core` (next to `SessionManager`) and is consulted between turns. It tracks (a) total tokens consumed in the session and (b) the last N role decisions (delegation, ask_human, tool-call) for each role. Triggers: (a) total session tokens exceed `--session-token-budget` (default 50_000) — auto-pause with reason `"token budget exceeded"`; (b) the same role makes the same decision (delegation/ask_human/same-tool-call) 3 times in a row — auto-pause with reason `"dead loop: <role> repeating <decision>"`. Both pauses go through `SessionManager::pause` so the human sees a single uniform flow.

5. **Zero regression.** All v1 commands still work; `chat --task-id X` without `--max-rounds` defaults to 10; `chat --task-id X` with `--max-rounds 0` is a v1-compatible manager-dispatch-only mode (one turn per role, no round scheduler); all 205 v1 tests still pass.

6. **Trace integration.** Two new `TraceEvent` variants: `AskHuman { meta, task_id, role, question }` and `RoundStarted { meta, task_id, round, roles }` / `RoundEnded { meta, task_id, round }`. The existing 13 variants are unchanged.

7. **Tested end-to-end.** Unit tests for `RoundScheduler` (1 round × 3 roles → 3 invocations in order; max-rounds cap; /pause interrupts mid-round cleanly), `plan.md_slice_for` (extracts by role; falls back to full doc for unknown role; handles empty plan.md), `Supervisor` (token trigger; dead-loop trigger; human reset after a pause). One black-box e2e: `latte-agent chat --task-id e2e11 --roles manager,programmer,reviewer --initial-prompt "noop" --max-rounds 2` with the binary's stdin scripted to type 2 turns and `/quit`; assert each role is invoked exactly 2 times and the trace JSONL has `RoundStarted` × 2.

## 3. Non-Goals (deferred)

- **Cross-process SessionManager** — v2.
- **Remote push / PR creation** — v2.
- **Semantic blackboard (pub/sub)** — v2.
- **Worktree-in-worktree** — v2.
- **Role-aware tier override** (e.g. "manager is always premium") — v2; v1.1 uses the role's `model_tier` from `agents.toml` as v1 already does.
- **v1.1 does not change the manager-only dispatch path** — `chat --max-rounds 0` still works the v1 way (manager dispatches to specialists once and synthesizes). Round-robin is opt-in by default `max-rounds=10`.

## 4. Architecture and Data Model

### 4.1 Top-level architecture (v1.1 delta over v1)

```mermaid
flowchart TB
    User[User] -->|latte-agent chat --task-id X --max-rounds 10| CLI[CLI Entry]
    CLI -->|load| SM[SessionManager]
    SM -->|reads| JSON[.latte/sessions/X.json]
    CLI --> REPL[REPL Driver]
    REPL --> RS[RoundScheduler]
    RS -->|one turn per role| AR[AgentRunner x N]
    AR -->|plan.md slice| SLICE[plan_md_slice_for]
    AR -->|can call| DEL[delegate tool]
    AR -->|can call| ASKH[ask_human tool]
    DEL -->|spawns work| AR2[Another Role Runner]
    ASKH -->|auto-pause| SM
    SM -->|emits| TS[JsonlSink / IndexSink]
    SUP[Supervisor] -->|between turns| SM
    SUP -->|token / dead loop| SM
    AR -.->|write/edit in worktree| CE[CheckpointEngine]
    REPL -->|@role line| IJ[RoleInjector::queue_for]
    IJ -->|file| QQ[.latte/inject/<role>.txt]
    AR -->|drain| IJ
    REPL -->|/pause| SM
    CLI -->|--resume| SM
```

The only **new** boxes are `RoundScheduler`, `Supervisor`, `plan_md_slice_for`, and the `ask_human` tool. Everything else is inherited from v1.

### 4.2 `RoundScheduler` (NEW in `latte-agent-core/src/scheduler.rs`)

```rust
use std::sync::Arc;
use crate::agent::AgentRunner;
use crate::session::{SessionManager, SessionError};

/// Drives the round-robin peer discussion. One `run_round` call
/// invokes every role once, in stable order (alphabetical by role_id
/// with manager last). A role can call `delegate` during its turn to
/// make another role do work in the same round; the delegated work
/// appears in the target role's history before its own turn slot.
///
/// The supervisor is consulted between every role's turn.
pub struct RoundScheduler {
    session: Arc<tokio::sync::Mutex<SessionManager>>,
    /// Order of role ids, computed once at scheduler construction.
    /// For v1.1: alphabetical sort, then move "manager" to the end
    /// so manager speaks last in each round (natural summary position).
    pub order: Vec<String>,
    /// Optional: explicit max rounds. None means "use SessionManager
    /// default of 10". The scheduler advances round only if
    /// current_round < max_rounds.
    pub max_rounds: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct RoundSchedulerConfig {
    pub max_rounds: u32,         // 0 = manager-dispatch-only (v1 compat)
    pub session_token_budget: u32, // 0 = disabled
}

impl RoundScheduler {
    pub fn new(session: Arc<tokio::sync::Mutex<SessionManager>>) -> Result<Self, SessionError>;

    /// Run a single round. Returns the per-role outcomes so the
    /// caller can decide whether to continue. Each role's outcome
    /// is `(role_id, AgentOutcome)` where AgentOutcome is
    /// `Completed | PausedBySupervisor | PausedByAskHuman | Failed`.
    pub async fn run_round(&self) -> Result<Vec<(String, AgentOutcome)>, SessionError>;

    /// Convenience: run rounds until max_rounds is reached, the
    /// session is paused, or a role returns Failed.
    pub async fn run_until_done(&self) -> Result<RunSummary, SessionError>;
}

pub enum AgentOutcome {
    Completed,
    PausedBySupervisor(String), // reason
    PausedByAskHuman(String),   // question
    Failed(String),
}

pub struct RunSummary {
    pub rounds_completed: u32,
    pub total_tokens: u32,
    pub outcomes: Vec<(String, AgentOutcome)>,
}
```

The scheduler consults `Supervisor` between every role's turn. If the supervisor says "pause", the current role's turn is allowed to complete (so its tokens are accounted for), then `SessionManager::pause` is called with the supervisor's reason, and the round loop exits.

### 4.3 `Supervisor` (NEW in `latte-agent-core/src/supervisor.rs`)

```rust
use std::collections::VecDeque;
use crate::session::SessionError;

#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    pub session_token_budget: u32,    // 0 = disabled
    pub dead_loop_window: usize,      // default 3
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self { session_token_budget: 50_000, dead_loop_window: 3 }
    }
}

pub struct Supervisor {
    config: SupervisorConfig,
    total_tokens: u32,
    /// Per-role ring buffer of (decision_kind, decision_payload) pairs.
    /// For v1.1, "decision_kind" is one of
    /// "tool_call:<tool_name>:<args_hash>" | "delegate" | "ask_human".
    /// The buffer holds the last N decisions; if all N match for the
    /// same role, trigger dead loop.
    history: std::collections::HashMap<String, VecDeque<(String, String)>>,
}

impl Supervisor {
    pub fn new(config: SupervisorConfig) -> Self;

    /// Record a turn's outcome. Returns Some(reason) if the supervisor
    /// decides to auto-pause, None otherwise.
    pub fn observe(
        &mut self,
        role_id: &str,
        tokens_used: u32,
        decision: &str,
    ) -> Option<String>;

    pub fn total_tokens(&self) -> u32;
}
```

Triggers (in order):
- `total_tokens + tokens_used > session_token_budget` → `Some("token budget exceeded (X / Y)")`
- For the given `role_id`, the last N observations all have the same `decision` → `Some("dead loop: <role> repeating <decision>")`

After a pause, the supervisor's `total_tokens` is NOT reset (the session is paused, not ended). On `SessionManager::resume`, the supervisor is reset by re-reading the total from the JSON.

### 4.4 `plan.md_slice_for` (NEW, lives in `latte-agent-core/src/scheduler.rs` or a new `latte-agent-core/src/plan_slice.rs`)

```rust
/// Return the substring of `plan_md` that `role_id` should see this turn.
///
/// Slice rule: every H2 section whose header starts with `<role_id> `
/// (e.g. `## programmer round 1`), PLUS everything before the first
/// `## ` H2 (the initial-prompt section). For an unknown role id,
/// return the full plan_md (defensive default).
pub fn plan_md_slice_for(plan_md: &str, role_id: &str) -> String;
```

The slicing is purely structural: split on `\n## ` lines, take the header + body of each section whose first word matches the role id. The "top of file" is the section from byte 0 to the first `\n## `.

### 4.5 `ask_human` tool registration

`ask_human` is registered as a per-specialist tool (NOT for manager). Registration happens in `chat.rs` inside `run_hil_repl`'s setup loop, alongside the existing `register_delegate_tool` call. The tool's Rust side:

```rust
// in chat.rs (NEW)
fn register_ask_human_tool(
    tm: &ToolManager,
    session: Arc<tokio::sync::Mutex<SessionManager>>,
    role_id: String,
) {
    tm.register_tool("ask_human", move |args: serde_json::Value| {
        let question = args.get("question").and_then(|v| v.as_str()).unwrap_or("").to_string();
        // Block the calling tool execution; emit AskHuman event; auto-pause.
        let mut mgr = session.blocking_lock();
        mgr.pause_with_reason(&format!("ask_human: {} asked: {}", role_id, question)).ok();
        mgr.emit_ask_human_event(&role_id, &question);
        // Tool returns an error so the run_turn loop in agent.rs knows
        // the call site is paused; agent.rs already handles this case
        // by returning a PartialResult to the REPL.
        Err(format!("session paused: ask_human from {}", role_id).into())
    });
}
```

The `pause_with_reason` method is a new `SessionManager` method:

```rust
impl SessionManager {
    /// Set pause_reason and paused_at, transition to Paused, persist.
    /// This is the same logic as `pause` but takes a fully-formed
    /// reason string (no internal "user /pause" prefix).
    pub fn pause_with_reason(&mut self, reason: &str) -> Result<(), SessionError>;

    /// Append a `TraceEvent::AskHuman` to the JsonlSink (via a
    /// sink field on SessionManager, set on construction in Phase 3.2).
    pub fn emit_ask_human_event(&self, role_id: &str, question: &str);
}
```

For v1.1, `SessionManager` needs a `sink: Option<Arc<dyn TraceSink>>` field. This is a small additive change; v1's `SessionManager::new` does not take a sink, so v1.1 adds a builder method `with_sink(sink)`. Existing v1 tests pass `None` and behavior is unchanged.

The `ask_human` prompt is added to each non-manager role's `prompts/<role>.md` (or, in v1.1, just the tool name is auto-listed in the tool-usage prompt that `chat.rs:797` already builds).

### 4.6 New `TraceEvent` variants

```rust
pub enum TraceEvent {
    // ... existing 13 variants (9 legacy + 4 v1) ...

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
        roles: Vec<String>,    // speaking order this round
    },
    RoundEnded {
        meta: TraceMeta,
        task_id: String,
        round: u32,
    },
}
```

Three variants appended after the last existing one. The 5 match-arm sites in `trace.rs` get 3 new arms each (mirroring the v1 pattern of one combined arm where possible).

### 4.7 SessionRecord shape (v1.1 delta)

The JSON shape is unchanged (no new fields). The `paused_at` and `pause_reason` fields that v1 already writes are reused for the auto-pause cases (token / dead loop / ask_human). The `pause_reason` string is the source of truth for *why* the session paused, and the human's first action on resume is to read that reason.

### 4.8 CLI flags (v1.1 delta)

`latte-agent chat` gains three flags:

| Flag | Default | Meaning |
|---|---|---|
| `--max-rounds N` | `10` | Maximum round-robin rounds. `0` = manager-dispatch only (v1 behavior). |
| `--session-token-budget N` | `50000` | Supervisor token budget. `0` = disabled. |
| `--no-ask-human` | off | Disable `ask_human` tool registration (escape hatch for users who don't want pauses). |

`chat --task-id X` without these flags behaves as v1.1 with the defaults. `chat` without `--task-id` continues to use the legacy single-role REPL (v1 compat).

### 4.9 State machine (v1.1 delta over v1)

The state machine in `SessionManager` is unchanged. The only new transition triggers are:
- `Running` → `Paused` via `ask_human` (via `pause_with_reason`)
- `Running` → `Paused` via supervisor (via `pause_with_reason`)
- The auto-resume from `Paused` → `Resumed` when the REPL drives the next round (already in v1).

The `SessionError::InvalidTransition` invariant is unchanged.

### 4.10 Filesystem layout (v1.1 delta)

No new files. The supervisor's state lives inside `SessionManager`'s on-disk JSON (encoded as the `pause_reason` field). Round scheduler state is reconstructed at startup from `SessionManager::record()`. The `plan.md` file is the single source of truth; the slice is derived on read.

## 5. Lifecycle (v1.1 round)

```
[Run start]      user runs `chat --task-id X --max-rounds 10`
       |
       v
[SessionManager.open_or_create]
       |
       v
[Round 1 starts]  --trace: RoundStarted { round: 1, roles: [programmer, reviewer, manager] }
       |
       |--- role 1 (e.g. programmer):  run_turn, possibly delegate, possibly ask_human
       |       |  supervisor.observe(role, tokens, decision)
       |       |  if supervisor returns Some(pause_reason): break round
       |       v
       |--- role 2 (e.g. reviewer):  same
       |       v
       |--- role 3 (e.g. manager):  same (manager speaks last)
       |
       v
[Round 1 ends]  --trace: RoundEnded { round: 1 }
       |
       v
[Round 2 starts]  (same)
       ...
       v
[Max rounds reached OR supervisor paused OR /quit OR /pause]
       |
       v
[SessionManager.mark_done | .pause | .mark_failed]
```

The v1 round loop is `max_rounds = 0` (manager dispatches once, no rounds). The v1.1 default is `max_rounds = 10`.

## 6. RoundScheduler

### 6.1 Construction

`RoundScheduler::new(session)` reads `session.record().roles`, sorts the role ids alphabetically, and moves `"manager"` to the end (if present). The `order` field is `pub` so the REPL can print it for the human.

If the role list is empty, the scheduler refuses to construct (return `SessionError::UnknownRole("")`).

### 6.2 Round execution

`run_round` does the following for each role in `order`:

1. Acquire the `SessionManager` lock.
2. If `session.state() != Running`, return early with the partial outcomes.
3. Drain the role's inject queue (`<wt>/.latte/inject/<role>.txt`); if non-empty, prepend a synthetic `Role::User` message with content `"[INJECTED]\n<queue content>"` to the role's history. (This is the v1 mechanism, now under scheduler control.)
4. Read `plan_md` from the session record; compute `plan_md_slice_for(plan_md, role_id)`. Append a synthetic `Role::User` message with content `"[PLAN SLICE]\n<slice>"` to the role's history.
5. Call `role.run_turn(decision_boundary=after_each_tool_call)`. (v1.1 reuses the existing `AgentRunner::run_turn` signature.)
6. After the turn, compute `tokens_used = role.last_turn_tokens()` (read from `total_usage` on the runner).
7. Compute `decision = role.last_decision_kind()` (a string like `"tool_call:write:abc123"` or `"delegate"` or `"ask_human"` or `"text"` for no-tool turns).
8. `session.append_to_role(role_id, role.last_assistant_message())` — persist the assistant's reply.
9. `session.advance_turn()`.
10. `supervisor.observe(role_id, tokens_used, decision)`. If `Some(pause_reason)`, call `session.pause_with_reason(reason)`, push `AgentOutcome::PausedBySupervisor(reason)` to outcomes, break the round.
11. If the role emitted an `ask_human` tool call, the tool's closure already called `session.pause_with_reason("ask_human: ...")`. Detect this (via `session.state() == Paused`) and push `AgentOutcome::PausedByAskHuman(question)`, break the round.
12. Continue to the next role.

If the round completes without any pause, the return is `Ok(outcomes)` with all entries `Completed`.

### 6.3 Termination

`run_until_done` loops `run_round` until:
- A pause happens (return `Ok` with the partial outcomes)
- `max_rounds` rounds complete (return `Ok` with all `Completed` outcomes)
- A role returns `Failed` (return `Err(SessionError::...)`)

`run_until_done` is the entry point the REPL calls.

### 6.4 `decision_kind` extraction

The `AgentRunner` currently does not expose "what did this turn do?" in a structured form. v1.1 adds a single method to `AgentRunner`:

```rust
impl AgentRunner {
    /// Return a string describing the last turn's main decision.
    /// Used by the Supervisor to detect dead loops.
    /// Possible values: "text" | "tool_call:<name>:<args_hash>" |
    ///                  "delegate" | "ask_human"
    pub fn last_decision_kind(&self) -> String;
}
```

The implementation reads the last message in `self.context.messages`:
- If `Role::Tool` → `"text"` (the assistant's reply after the tool came back)
- If `Role::Assistant` and contains `<tool_calldelegate>` → `"delegate"`
- If `Role::Assistant` and contains `<tool_callask_human>` → `"ask_human"`
- If `Role::Assistant` and contains `<tool_callNAME>` for other NAME → `"tool_call:NAME:<args_hash>"` (args_hash is `DefaultHasher` of the args JSON, 8 hex chars, same scheme as v1 `record_write` uses)
- Else → `"text"`

The args_hash is exactly what `CheckpointEngine::record_write` already computes in `short_hash`; v1.1 reuses that function by re-exporting it from `checkpoint.rs`.

## 7. plan.md slicing

`plan_md_slice_for(plan_md, role_id)`:

```
fn plan_md_slice_for(plan_md: &str, role_id: &str) -> String {
    if plan_md.is_empty() { return String::new(); }
    let mut out = String::new();
    let mut current_section: Option<(String, String)> = None;  // (header, body)
    for line in plan_md.lines() {
        if let Some(header) = line.strip_prefix("## ") {
            // Flush current section if it matches
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
            // Top-of-file content (before any ## H2)
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

For role_id = "manager", this also matches sections that start with "manager" (no other role starts with "manager"). The "everything before the first `## `" piece ensures the initial prompt is always visible to every role.

If `role_id` is not in the manager list and no H2 section matches, `out` is just the top-of-file. The defensive default for unknown role ids is to return the full `plan_md` (we can change this to "return empty string" later if the human prefers).

## 8. ask_human protocol

The `ask_human` tool is registered for every non-manager role (manager doesn't need to ask the human; it can just keep delegating). Registration happens in `run_hil_repl`'s setup, inside the existing per-role `AgentRunner::new_with_tools` builder chain:

```rust
let mut runner = AgentRunner::new_with_tools(agent, tm, /* max_tool_rounds= */ 16)
    .with_sink(...)
    .with_hooks(...)
    .with_role(role_id.clone())
    .with_inject_worktree_root(wt.clone());
if role_id != "manager" {
    register_ask_human_tool(&tm, session.clone(), role_id.clone());
}
```

The tool's `args` schema is `{"question": "<text>"}`. When the role emits a `<tool_callask_human> {"question": "..."}</tool_callask_human>`, the existing `extract_tool_calls` parser in `agent.rs:1122` picks it up, dispatches to the registered closure, the closure calls `SessionManager::pause_with_reason(...)`, returns an `Err` to agent.rs, agent.rs propagates the error to the REPL, the REPL sees the session state is `Paused` with reason `"ask_human: ..."`, prints the question in red, and waits for human input.

When the human types their reply, the REPL detects the session state is `Paused`, calls `SessionManager::resume_with_message(reply, target_role)` (a new method that does `resume()` + `append_to_role(target_role, synthetic_user_message_with_human_reply)`), transitions to `Resumed`, and the next round (or the current round's remaining roles) re-invokes the target role's `run_turn` with the human's reply in its history.

### 8.1 `resume_with_message`

```rust
impl SessionManager {
    /// Resume from Paused, append a synthetic user message to the
    /// named role's history, and persist. Idempotent if the session
    /// is already Resumed/Running.
    pub fn resume_with_message(
        &mut self,
        role_id: &str,
        message: &str,
    ) -> Result<(), SessionError>;
}
```

Implementation: `self.transition(Resumed)?; self.record.paused_at = None; self.record.pause_reason = None; self.append_to_role(role_id, Message { role: User, content: format!("[HUMAN @ <ts>]\n{}", message) })?; self.persist()`

## 9. Supervisor

### 9.1 Token budget

`total_tokens` is a `u32` running sum across the session. Each `observe(role, tokens, decision)` call adds `tokens` to it. If the sum exceeds `session_token_budget`, the supervisor returns `Some("token budget exceeded ({total} / {budget})")`.

The default budget is `50_000`. Operators with longer-running sessions can raise it via `--session-token-budget 200000`. Setting `--session-token-budget 0` disables the token trigger (the dead-loop trigger is still active).

### 9.2 Dead loop

For each role, the supervisor keeps a ring buffer of the last N decisions (default N=3). On each `observe`, it pushes the new decision and pops the oldest if the buffer is full. If all entries in the buffer are byte-equal, the supervisor returns `Some("dead loop: <role> repeating <decision>")`.

The decision string is whatever `AgentRunner::last_decision_kind()` returns. So "programmer called `tool_call:write:abc123` three turns in a row" triggers, but "programmer called `tool_call:write:abc123` once, then `text` twice" does not.

### 9.3 Reset on resume

`SessionManager::resume()` (or `resume_with_message`) also resets the supervisor's history (clears the per-role ring buffers). The token counter is NOT reset (the session is paused, not ended; the human may want to see how much was used). On the next `observe`, the counter continues from where it was.

To re-architect: a fresh `Supervisor::reset_history()` method is called by `SessionManager::resume()` and `resume_with_message()`. The token counter is read back from the on-disk JSON (or, in v1.1, just not reset and treated as a soft warning).

## 10. Tests

### 10.1 Unit tests

| Target | Test | What it asserts |
|---|---|---|
| `RoundScheduler` | `alphabetical_order_with_manager_last` | Roles `[manager, programmer, reviewer]` → order `[programmer, reviewer, manager]` |
| `RoundScheduler` | `empty_roles_refuses` | `RoundScheduler::new(empty session)` returns `Err` |
| `RoundScheduler` | `run_round_invokes_each_role_once` | After 1 round, every role's `run_turn` was called exactly once; outcomes all `Completed` |
| `RoundScheduler` | `run_until_done_respects_max_rounds` | `max_rounds=2` → exactly 2 rounds before returning `RunSummary` |
| `RoundScheduler` | `supervisor_pause_interrupts_round` | After 1 role's turn, supervisor returns `Some(pause)`; round exits with the partial outcomes; the second role is not invoked |
| `plan_md_slice_for` | `extracts_matching_h2` | Plan with `## programmer round 1\n...\n## reviewer round 1\n...` → for `programmer`, returns just the first section + the top-of-file; for `reviewer`, just the second + top-of-file; for `manager`, all sections + top-of-file |
| `plan_md_slice_for` | `unknown_role_returns_full` | For `role_id = "ghost"`, returns the full plan_md |
| `plan_md_slice_for` | `empty_plan_returns_empty` | Empty input → empty output |
| `Supervisor` | `token_budget_triggers_at_threshold` | `budget=100`, after 3 observations of 40 each, 4th triggers `token budget exceeded` |
| `Supervisor` | `dead_loop_triggers_after_N_repeats` | After 3 observations of the same decision for one role, the 4th same one triggers |
| `Supervisor` | `reset_history_clears_dead_loop_state` | After 3 repeats, `reset_history()` then 1 more same decision does NOT trigger |
| `AgentRunner::last_decision_kind` | `returns_text_for_plain_response` | Last message is `Role::Assistant` with no tool call → `"text"` |
| `AgentRunner::last_decision_kind` | `returns_tool_call_for_write` | Last message contains `<tool_callwrite>` → `"tool_call:write:<hash>"` |
| `SessionManager::pause_with_reason` | `sets_paused_at_and_reason` | After call, `state==Paused`, `pause_reason` matches |
| `SessionManager::resume_with_message` | `appends_synthetic_user_message` | After call, role's history has the `[HUMAN @ <ts>]` message at the end |

### 10.2 Black-box integration test

`latte-agent-cli/tests/hil_v11_e2e.rs`:

1. Init temp git repo.
2. `latte-agent run --task-id e2e11 --initial-prompt "noop"` to create the worktree.
3. `printf "first task\n/quit\n" | latte-agent chat --task-id e2e11 --roles manager,programmer,reviewer --initial-prompt "noop" --max-rounds 2`.
4. Assert: the binary's stdout contains:
   - `[session: e2e11, state: Created, turn: 0]`
   - `[roles: manager, programmer, reviewer]`
   - `RoundStarted { round: 1, roles: [programmer, reviewer, manager] }` — but wait, the binary does not currently print TraceEvents in this format. Update the test to instead assert: the trace JSONL at `~/.latte/traces/<id>.jsonl` contains `RoundStarted` events.
   - `/quit` exits cleanly.
5. Inspect the trace JSONL: assert ≥ 2 `RoundStarted` events and ≥ 3 (or `max_rounds × roles.length = 6`) `ToolExec` events (one per role per round).
6. Inspect `<worktree>/.latte/sessions/<id>.json`: `state == "Done"`, `current_turn > 0`.

If any of these fail, v1.1 is not done.

### 10.3 Manual smoke checklist

- [ ] `latte-agent chat --task-id X --roles manager,programmer,reviewer` enters round-robin mode
- [ ] In the REPL, `/pause` interrupts mid-round; on resume, the round restarts from the same role order
- [ ] `@programmer ...` queues a message that the next programmer turn consumes
- [ ] A specialist that calls `ask_human` triggers an auto-pause; the REPL shows the question in red
- [ ] `latte-agent chat --task-id X --max-rounds 0` falls back to v1's manager-dispatch-only behavior
- [ ] `latte-agent chat --task-id X --no-ask-human` skips the ask_human tool registration
- [ ] `latte-agent chat --task-id X --session-token-budget 100` and forcing 5 turns triggers a token-budget pause

## 11. Implementation Phasing

8 phases total, with v1's 8 phases already done (in pi-dev @ `35de34b`). The new v1.1 phases land on the same worktree (`latte-rs-agents-hil-v1`) on top of the v1 commits.

| Phase | What it ships | Estimated complexity |
|---|---|---|
| 1 | `plan_md_slice_for` + 3 unit tests | small |
| 2 | `AgentRunner::last_decision_kind` + 2 unit tests | small |
| 3 | `Supervisor` struct + 3 unit tests + config wiring | medium |
| 4 | `SessionManager` extensions: `pause_with_reason`, `resume_with_message`, `with_sink`, `emit_ask_human_event` + 2 unit tests | small |
| 5 | `RoundScheduler` struct + 5 unit tests | medium |
| 6 | `ask_human` tool registration in `run_hil_repl` + 2 new `TraceEvent` variants | small |
| 7 | 3 new `chat` flags (`--max-rounds`, `--session-token-budget`, `--no-ask-human`) + REPL integration | small |
| 8 | Black-box e2e test `tests/hil_v11_e2e.rs` + docs | small |

Each phase ends with `cargo test --workspace` green and a single commit. No phase is "done" until its tests pass.

## 12. Acceptance Criteria (v1.1 done iff ALL true)

- [ ] All v1 tests + e2e still pass (≥ 205 baseline)
- [ ] `cargo test --workspace` green; the new `tests/hil_v11_e2e.rs` passes
- [ ] `latte-agent chat --task-id X --roles a,b,c --initial-prompt Y` enters round-robin mode by default; 10 rounds cap
- [ ] Each round invokes every role exactly once in alphabetical order with manager last
- [ ] `--max-rounds 0` reproduces v1's manager-dispatch-only behavior
- [ ] `--session-token-budget 100` causes an auto-pause after enough turns; resume continues with the same role order
- [ ] A specialist that calls `ask_human` triggers an auto-pause; the REPL shows the question; the human's reply is appended to that role's history and the role's next turn sees it
- [ ] A specialist that calls the same `tool_call:write:abc123` three turns in a row triggers an auto-pause with reason `"dead loop: ..."`
- [ ] `plan_md_slice_for("## programmer round 1\nfoo\n## reviewer round 1\nbar", "programmer")` returns `"## programmer round 1\nfoo\n"`, not the full plan
- [ ] All 3 new `TraceEvent` variants (`AskHuman`, `RoundStarted`, `RoundEnded`) appear in `~/.latte/traces/<id>.jsonl` after a round-robin session
- [ ] README updated to point at this spec and document the new flags
