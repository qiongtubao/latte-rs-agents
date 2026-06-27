# latte-agent: HIL Blackboard + Worktree Sandbox v1

**Date:** 2026-06-28
**Status:** Draft (post-brainstorming, pre-implementation)
**Scope:** v1 = Worktree isolation (A) + Manager delegate to N specialists (B) + SessionManager state machine with JSON persistence (D) + Pause/Resume CLI + REPL (E) + `@role` injection (F) + Surgical rollback (G: git reset only, plan.md + trace untouched).
**Out of scope (deferred to v1.1):** Selective injection /省 Token core (C), `ask_human` tool (H), Supervisor auto-cutoff / Token + dead-loop watchdog (I), worktree-in-worktree nesting, PR automation, semantic blackboard (pub/sub).
**Out of scope (deferred to v2):** Cross-process / cross-worktree pause-resume, real `SessionManager` (this spec's `SessionManager` is per-session-file; v2 is global + supervisor).

## 1. Background

`latte-agent` already has solid primitives at `pi-dev` HEAD `5f3bf8f` (after this spec lands) and the legacy 11-commit `latte/blackboard-sandbox` branch:

- Multi-role Chat / Discuss / Workflow via clap subcommands
- `TraceEvent` observability pipeline (9 legacy variants + 3 added in `latte/blackboard-sandbox`: `DiffSummary` payload + `CheckpointCreated` + `CheckpointRolledBack`)
- 5-point `HookChain` with `HookOutcome::Retry { correction }` reserved
- `LoopDetector` for tool-call loops (tool-level only, **not** session-level — see I deferred to v1.1)
- Built-in hooks: `RedactPii`, `EnforceToolAllowlist`, `RequireToolCall`
- `WorkspaceManager` (Worktree + plan.md + archive + cleanup) — from `latte/blackboard-sandbox`
- `CheckpointEngine` (record_write + rollback code/trace/full) — from `latte/blackboard-sandbox`
- 6 HIL CLI subcommands from `latte/blackboard-sandbox`: `run`, `inject`, `pause`, `resume`, `checkpoint{create,list,rollback}` — **all in place**

This spec **extends** that baseline to add what the legacy spec deferred to v2:
- Real SessionManager state machine (not just a flag file)
- Pause/Resume with full conversation history restoration
- `@role` injection routed to the right specialist's next user message (not just appended to plan.md)
- Rollback semantics aligned with §三.3: reset worktree code, keep plan.md + trace

After this spec, **`latte-agent chat --task-id X` becomes the user entry point** for the HIL blackboard workflow. The legacy `chat` (single-role REPL) and `discuss` (run-to-completion multi-agent) remain available for users who don't want the worktree / pause-resume overhead.

## 2. Goals

1. **Multi-role HIL chat.** `latte-agent chat --task-id X --roles pm,architect,programmer,reviewer` starts a blackboard session: manager as scheduler, specialists share `plan.md` as the single source of truth, all agents (human included) read/write the blackboard. Main repo is isolated via a git worktree.
2. **Real SessionManager state machine.** Each session persists to `<worktree>/.latte/sessions/<session_id>.json` including: role history (full conversation per role, deduped), plan.md content, checkpoint id, current state (`Created | Running | Paused | Resumed | Done | Failed`), pause reason, paused_at. State transitions are atomic via temp-file + rename.
3. **Pause/Resume CLI + REPL.** In-REPL `/pause` writes a pause signal + atomically flushes state to JSON. Next `latte-agent chat --task-id X --resume` (no `--resume`, auto-detect from `Paused` JSON) picks up where it left off. `--force-resume` for `Done` sessions (defensive).
4. **`@role` injection.** In-REPL line starting with `@<role-id> ...` is parsed and routed: the message becomes the next `user` message of that role's conversation, with the role's `ConversationContext` unchanged. No manager mediation. If the role id is unknown, REPL prints an error and the line is not sent to anyone.
5. **Surgical rollback (G).** `latte-agent checkpoint rollback --task-id X --id N` (mode is fixed to `code` only in v1; the `trace` and `full` modes from the legacy spec are deprecated for v1 because the spec §三.3 says "撤销代码,保留讨论" — both `plan.md` and `trace` are explicitly preserved): `git reset --hard <commit-N>` in the worktree, plan.md and trace JSONL untouched. Subsequent specialist runs see the post-reset code state plus the full pre-reset discussion history.
6. **Manager delegate is real.** Manager prompt + `delegate` tool are wired so manager can issue multiple parallel `<tool_calldelegate role=...>...</tool_calldelegate>` in one turn, each spawning a specialist runner with its own `ConversationContext` (per the existing `register_delegate_tool` plumbing in `chat.rs:828-857`). Specialists share the plan.md (read on entry, append on exit) and the worktree file system.
7. **Trace integration.** New `TraceEvent` variants `SessionStarted`, `SessionPaused`, `SessionResumed`, `RoleInjected` (for `@role` lines) flow through the existing `JsonlSink` / `IndexSink` chain. No new sink types.
8. **Zero regression for legacy commands.** `chat` (no `--task-id`), `discuss`, `workflow`, `list`, `config`, `debug` keep their current behavior. `--task-id` on `chat` is opt-in: absent means single-role REPL as before.
9. **Tested end-to-end.** Unit tests for SessionManager (state transitions + JSON round-trip), REPL parser (lines starting with `@`, `pause`, plain), `@role` injection (verifies specialist's next user message), surgical rollback (worktree state post-reset + plan.md/trace byte-equal pre/post). One black-box e2e: `latte-agent chat --task-id X` runs ~2 turns, `/pause`, exit; `--resume` re-opens; one more turn; rollback --id 1; verify code resets, plan.md unchanged, trace JSONL unchanged (modulo SessionPaused/Resumed/RoleInjected appends which the test explicitly accounts for).

## 3. Non-Goals (deferred)

### 3.1 Deferred to v1.1

- **Selective injection (C)** — specialist sees only the plan.md slice relevant to its task. v1.1 will use AST/heading parsing to extract per-role sections.
- **`ask_human` tool (H)** — agent calls a tool, SessionManager auto-pauses, REPL shows a red prompt, human input becomes the agent's next user message. v1.1 will add the tool registration + the auto-pause hook.
- **Supervisor (I)** — session-level Token budget + dead-loop detection. v1.1 will lift `LoopDetector` from `agent.rs` to `SessionManager`.

### 3.2 Deferred to v2

- **Cross-process / cross-worktree SessionManager** — the v1 `SessionManager` is per-session-file under `<worktree>/.latte/sessions/`. v2 will be a global index (SQLite or similar) so users can `latte-agent sessions list` to see all in-flight sessions across all worktrees.
- **Nested worktrees** — one worktree per session, period.
- **Remote push / PR creation** — archive ends at `--no-ff` merge into the base branch; PR automation is its own spec.
- **Semantic blackboard (pub/sub)** — v1 blackboard is `plan.md`; v2 will be a typed message bus.
- **Worktree-in-worktree** — a session inside another session's worktree. v1 rejects with a clear error.

## 4. Architecture and Data Model

### 4.1 Top-level architecture

```mermaid
flowchart TB
    User[User] -->|latte-agent chat --task-id X| CLI[CLI Entry]
    CLI -->|load or create| SM[SessionManager]
    SM -->|reads| JSON[.latte/sessions/X.json]
    SM -->|uses| WM[WorkspaceManager]
    WM -->|worktree| WT[.latte/worktrees/X/]
    WM -->|blackboard| BB[plan.md]
    CLI -->|REPL| REPL[Chat REPL]
    REPL -->|@role line| IJ[RoleInjector]
    IJ -->|append to role queue| QQ[.latte/inject/<role>.txt]
    SM -->|paused?| CTRL[pause flag]
    REPL -->|/pause| SM
    CLI -->|--resume| SM
    SM -->|drives| AR[AgentRunner x N]
    AR -->|plan.md| BB
    AR -->|emits| TS[JsonlSink / IndexSink]
    SM -->|CheckpointCreated/RolledBack/Session*| TS
    AR -.->|write/edit in worktree| CE[CheckpointEngine]
    CE -->|diff patch| CKPT[.latte/checkpoints/X/NNN.patch]
```

### 4.2 Crate topology

No new crates. Changes land in:

- `latte-agent-core/src/session.rs` (**new**): `SessionManager`, `SessionState`, `SessionRecord`, `RoleHistory`, atomic JSON write helpers
- `latte-agent-core/src/trace.rs` (extend): add `SessionStarted`, `SessionPaused`, `SessionResumed`, `RoleInjected` to `TraceEvent`
- `latte-agent-core/src/agent.rs` (small): add `RoleInjector::queue_for(role_id, message)` and the per-role inject queue reader in `AgentRunner::run_turn`
- `latte-agent-cli/src/commands/chat.rs` (extend): `--task-id` opt-in, REPL `/pause` command, REPL `@<role>` parser, on startup auto-load `SessionRecord` if `--task-id` given
- `latte-agent-cli/src/commands/inject.rs` (modify): route `--role` to `RoleInjector::queue_for` (not file append as in `latte/blackboard-sandbox` legacy)
- `latte-agent-cli/src/commands/pause.rs` (modify): call `SessionManager::pause` instead of writing a flag file
- `latte-agent-cli/src/commands/resume.rs` (modify): call `SessionManager::resume`
- `latte-agent-cli/src/commands/checkpoint.rs` (modify): deprecate `trace` and `full` rollback modes; only `code` is supported in v1
- `latte-agent-cli/src/commands/run.rs` (no change)
- `latte-agent-cli/src/main.rs` (no change beyond what's already there)

### 4.3 SessionManager types (NEW)

```rust
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

/// State machine for a single HIL session.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SessionState {
    Created,
    Running,
    Paused,
    Resumed,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleHistory {
    pub role_id: String,
    /// Mirrors the role's `ConversationContext.messages` at last save.
    pub messages: Vec<latte_ai::models::Message>,
    /// Last turn the role completed.
    pub last_turn: u32,
}

/// Per-session persisted record. Lives at
/// `<worktree>/.latte/sessions/<session_id>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_id: String,
    pub task_id: String,
    pub state: SessionState,
    /// Full plan.md content at last save. Restored on resume.
    pub plan_md: String,
    /// Last completed checkpoint id (0 = none).
    pub active_checkpoint_id: u32,
    /// Last manager turn number.
    pub current_turn: u32,
    /// One entry per participating role. Manager is always present.
    pub roles: Vec<RoleHistory>,
    /// ISO8601 UTC. Set on transition into Paused.
    pub paused_at: Option<String>,
    /// Human-readable pause reason.
    pub pause_reason: Option<String>,
    pub started_at: String,
    pub updated_at: String,
}

pub struct SessionManager {
    record: SessionRecord,
    session_path: PathBuf,
    worktree_root: PathBuf,
}

impl SessionManager {
    /// Load a session by id, or create a new one if `task_id` is fresh.
    /// Errors with `NotARepo` if cwd is not inside a git repo.
    pub fn open_or_create(cwd: &Path, task_id: &str, roles: &[String]) -> Result<Self, SessionError>;

    /// Persist the current record to `<worktree>/.latte/sessions/<id>.json`
    /// atomically (write to `.tmp`, rename). Called after every manager
    /// turn completion and on every state transition.
    pub fn persist(&self) -> Result<(), SessionError>;

    pub fn state(&self) -> SessionState;
    pub fn record(&self) -> &SessionRecord;
    pub fn session_path(&self) -> &Path;

    /// Transition to Paused. Sets `paused_at` + `pause_reason`, persists.
    pub fn pause(&mut self, reason: &str) -> Result<(), SessionError>;

    /// Transition to Resumed (only legal from Paused). Clears `paused_at`,
    /// persists. Manager prompt gets a "RESUMED at <ts>" banner.
    pub fn resume(&mut self) -> Result<(), SessionError>;

    /// Transition to Done (only legal from Running or Resumed).
    pub fn mark_done(&mut self) -> Result<(), SessionError>;

    /// Transition to Failed (any state → Failed). Sets `pause_reason` to
    /// the failure message.
    pub fn mark_failed(&mut self, reason: &str) -> Result<(), SessionError>;

    /// Append a message to a role's history. Persists.
    pub fn append_to_role(&mut self, role_id: &str, msg: latte_ai::models::Message) -> Result<(), SessionError>;

    /// Read a role's full history (or empty Vec if role unknown).
    pub fn role_history(&self, role_id: &str) -> Vec<latte_ai::models::Message>;

    /// Bump the current turn counter and persist. Manager calls this
    /// at the end of every turn.
    pub fn advance_turn(&mut self) -> Result<(), SessionError>;
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid state transition from {from:?} to {to:?}")]
    InvalidTransition { from: SessionState, to: SessionState },
    #[error("not a git repository: {0}")]
    NotARepo(PathBuf),
    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),
}
```

### 4.4 SessionRecord JSON format

The persisted JSON at `<worktree>/.latte/sessions/<id>.json` is human-readable
and hand-editable (per spec §三.3 "外科手术式回滚" — operator can edit it
while paused to delete "毒药消息"):

```json
{
  "session_id": "fix-redis-bug-2026-06-28T07-32-11Z",
  "task_id": "fix-redis-bug",
  "state": "Paused",
  "plan_md": "# Task: fix-redis-bug\n\n## Initial prompt\n\nRedis pool doesn't recycle after 5xx\n\n## programmer turn 1\n\nRead Cargo.toml...\n",
  "active_checkpoint_id": 3,
  "current_turn": 2,
  "roles": [
    { "role_id": "manager", "messages": [...], "last_turn": 2 },
    { "role_id": "programmer", "messages": [...], "last_turn": 1 },
    { "role_id": "reviewer", "messages": [...], "last_turn": 1 }
  ],
  "paused_at": "2026-06-28T07:45:22Z",
  "pause_reason": "user /pause",
  "started_at": "2026-06-28T07:32:11Z",
  "updated_at": "2026-06-28T07:45:22Z"
}
```

Operator workflow: in REPL, type `/pause`, then `vim .latte/sessions/<id>.json`,
remove the offending `messages` entry, save, exit REPL. On next
`latte-agent chat --task-id X`, the SessionManager re-hydrates from this
JSON and the offending role's history is missing the bad turn.

### 4.5 New TraceEvent variants

```rust
pub enum TraceEvent {
    // ... existing 12 variants (9 legacy + 3 from latte/blackboard-sandbox) ...

    SessionStarted {
        meta: TraceMeta,
        task_id: String,
        roles: Vec<String>,         // participating role ids
        initial_prompt: String,
    },
    SessionPaused {
        meta: TraceMeta,
        task_id: String,
        reason: String,
        turn: u32,
    },
    SessionResumed {
        meta: TraceMeta,
        task_id: String,
        turn: u32,
    },
    RoleInjected {
        meta: TraceMeta,
        task_id: String,
        target_role: String,        // role id from `@role` line
        message_preview: String,    // first 100 chars
    },
}
```

All four are appended after the existing `CheckpointRolledBack` arm — same
non-reordering rule from the legacy spec §4.5. `ScopedSink` for manager /
specialist routing still applies.

### 4.6 Surgical rollback (G) — the simplified model

The legacy `latte/blackboard-sandbox` spec had three rollback modes: `code`,
`trace`, `full`. This spec **deprecates `trace` and `full`** because spec §三.3
explicitly says "保留讨论" — discussion is in `plan.md` and `trace` JSONL,
both of which must NOT be touched on rollback.

```rust
/// v1 rollback mode. Only `Code` is supported; `Trace` and `Full` from
/// the legacy spec are removed because they would destroy the
/// discussion history that this spec is built to preserve.
pub enum RollbackMode { Code }
```

The CLI flag `--mode` is removed from `latte-agent checkpoint rollback` in v1.
Calling with `--mode trace` or `--mode full` is a hard error.

Behavior on `latte-agent checkpoint rollback --task-id X --id N`:
1. Load `SessionRecord` (which has `active_checkpoint_id`). If session is
   `Running` or `Resumed`, refuse with `must pause or finish first` and
   exit code 64.
2. Find checkpoint N in `<worktree>/.latte/checkpoints/<task>/manifest.json`.
3. `git -C <worktree_root> reset --hard <commit-N>`.
4. Do NOT touch `plan.md` (in worktree root or worktree's `.latte/sessions/`).
5. Do NOT touch `<worktree>/.latte/checkpoints/<task>/manifest.json` or
   `.patch` files.
6. Do NOT touch `~/.latte/traces/<session_id>.jsonl`.
7. Emit `TraceEvent::CheckpointRolledBack` (the existing variant from
   `latte/blackboard-sandbox` is reused — its `mode` field is hard-coded
   to `"code"` since that's the only v1 mode).
8. SessionManager state stays whatever it was (Paused stays Paused; the
   user can `/resume` after rollback to continue with post-reset code +
   pre-reset discussion).

### 4.7 `@role` injection (F) — REPL parser

In the REPL, lines are classified by a small parser at the start of each
read:

| Input pattern | Action |
|---|---|
| Empty line or whitespace-only | Ignored (continue) |
| Starts with `/` | Built-in command (`/pause`, `/resume`, `/help`, `/roles`, `/quit`) |
| Starts with `@<role-id>` (alphanumeric + underscore + dash) | Role injection — see below |
| Anything else | Append to manager's next user message (the legacy chat behavior) |

Role injection parsing:
- `@<role-id>` must match a known role in `SessionRecord.roles`; else
  print `error: unknown role '<id>' (known: manager, programmer, reviewer, ...)`
  to stderr and discard the line.
- The remainder of the line (after `@role-id` and one or more spaces) is
  the message.
- Action: append to `<worktree>/.latte/inject/<role-id>.txt` (one file
  per role, newline-delimited, atomic append). Emit
  `TraceEvent::RoleInjected`.
- The next time that role's `AgentRunner::run_turn` is invoked (which may
  be the same turn for an already-running role, or the next manager-driven
  turn), the inject queue is read in full, drained, and prepended to
  the role's `ConversationContext.messages` as a synthetic `Role::User`
  message marked with a sentinel prefix `[INJECTED]`.

This is **not** Selective Injection (C) — it's plain "queue a message for
the named role's next turn." C in v1.1 will scope the message to the
relevant plan.md slice. v1's `@role` is sufficient for the user story
"人类精准@角色,绕过 manager" without the complexity of plan.md slicing.

### 4.8 Manager delegate (B) — confirming what already works

The legacy `latte/blackboard-sandbox` already provides `WorkspaceManager`
and `CheckpointEngine`. The legacy `chat.rs:828-857` already calls
`register_delegate_tool` with concurrency 4 and 60s timeout. Manager
prompt (`prompts/manager.md`) already says "你唯一可用的工具是 delegate"
and the recent `manager.toml` fix (`5f3bf8f`) gives it `tools = ["delegate"]`.

What this spec adds for B:
- **Per-specialist checkpoint isolation**: when manager delegates a task
  to `programmer`, the specialist's `AgentRunner` runs in a goroutine
  (already true via `tokio::spawn` in `register_delegate_tool`), writes
  to the worktree, and **each completed specialist turn produces a
  Checkpoint** (not just the manager's own tool writes). The legacy
  `CheckpointEngine::record_write` is called by `WorkspaceManager`'s
  wrapped `ToolManager`, which is shared by all specialists running in
  the same worktree. v1 just **confirms this is the wire-up** — no new
  code, just a test that asserts "after manager delegates to programmer
  who calls `write`, a checkpoint with id ≥ 1 exists in
  `<worktree>/.latte/checkpoints/<task>/manifest.json`."

## 5. SessionManager Lifecycle

```
[Created]      --start()        --> [Running]
[Running]      --pause(reason)  --> [Paused]
[Paused]       --resume()       --> [Resumed]
[Resumed]      --manager turn N --> [Running] (auto, after one turn)
[Running]      --mark_done()    --> [Done]
[Any]          --mark_failed(r) --> [Failed]
```

Invalid transitions (e.g. `Done → Running`) return
`SessionError::InvalidTransition`. The error message names both the
current and attempted state.

Invariants:
- `SessionRecord.paused_at.is_some() == (state == Paused)`
- `SessionRecord.pause_reason.is_some() == (state == Paused or Failed)`
- On every state transition, `SessionManager::persist` is called
  synchronously before the method returns
- A `Running` session can be killed by SIGINT; on next `chat --task-id X`
  the SessionManager detects the stale `Running` state and either
  auto-pauses (with `pause_reason = "interrupted by signal"`) or offers
  `--force-resume` to the operator

### 5.1 Filesystem layout

```
<repo_root>/
├── .latte/
│   ├── worktrees/<task>/                 (legacy A: from latte/blackboard-sandbox)
│   │   ├── .latte/
│   │   │   ├── sessions/
│   │   │   │   └── <session-id>.json     (NEW D)
│   │   │   ├── inject/
│   │   │   │   └── <role-id>.txt         (NEW F: role queue files)
│   │   │   ├── control/                  (deprecated by E; kept empty for back-compat)
│   │   │   └── checkpoints/              (legacy A)
│   │   │       └── <task>/
│   │   │           ├── manifest.json
│   │   │           └── NNNN.patch
│   │   ├── plan.md                       (legacy A: blackboard)
│   │   ├── .gitignore                    (contains `.latte/` to keep out of git)
│   │   └── ...project files...
│   └── checkpoints/<task>/               (legacy A: outside worktree, on main repo)
│       └── manifest.json
├── .git/worktrees/<task>/                (git's worktree bookkeeping)
└── ...(main branch untouched by write)...
```

### 5.2 Command-line interface (v1 changes only)

```bash
# Start a multi-role HIL session (NEW: --task-id + --roles on chat)
latte-agent chat --task-id fix-redis-bug \
    --roles manager,programmer,reviewer \
    --initial-prompt "Redis pool doesn't recycle after 5xx"

# Resume from a Paused session (auto-detected from JSON)
latte-agent chat --task-id fix-redis-bug

# In-REPL commands (typed at the chat prompt)
> /pause
> @programmer 先看 src/db/connection.rs 找到 5xx 处理路径
> @reviewer 忽略格式,重点看并发锁逻辑
> /resume
> /roles
> /quit

# Inject from outside the REPL (e.g. from another terminal)
latte-agent inject --task-id fix-redis-bug --role programmer --message "..."

# Surgical rollback (G): only --mode code is accepted in v1
latte-agent checkpoint rollback --task-id fix-redis-bug --id 3
# --mode trace / --mode full is a hard error in v1

# Archive + cleanup (legacy A, unchanged)
latte-agent run --task-id fix-redis-bug --archive --cleanup
```

### 5.3 Backward compatibility

- `latte-agent chat` (no `--task-id`) keeps current single-role behavior.
- `latte-agent discuss`, `workflow`, `list`, `config`, `debug` unchanged.
- `latte-agent run` (from legacy A) keeps its current behavior; the
  `inject`, `pause`, `resume`, `checkpoint` subcommands are rewritten
  to call SessionManager instead of the legacy flag-file.
- `--mode trace` / `--mode full` on `checkpoint rollback` becomes a
  hard error; the error message recommends `latte-agent run --task-id X
  --archive --cleanup` for "discard the whole session" semantics.

## 6. SessionManager

### 6.1 Persistence protocol

`SessionManager::persist` writes to a temp file, then renames atomically:

```
1. Serialize `record` to JSON
2. Write JSON to `<session_path>.tmp`
3. fsync the .tmp file
4. Rename `<session_path>.tmp` → `<session_path>` (atomic on POSIX)
5. Update `SessionRecord.updated_at` to now (so the on-disk record
   always reflects the in-memory state at the moment of last persist)
```

If a `.tmp` file is found at startup, it's a leftover from a crash; the
manager logs a warning and continues (the `.tmp` is overwritten on next
persist).

### 6.2 JSON shape for messages

`RoleHistory.messages` is `Vec<latte_ai::models::Message>`. The
`latte_ai` crate's `Message` is already `Serialize`/`Deserialize`, so
we re-use it. Operator hand-editing: messages have a clear shape
(`{"role": "user"|"assistant"|"tool", "content": "..."}`); deleting
entries is the "毒药消息" removal flow.

### 6.3 Resume semantics

When `latte-agent chat --task-id X` runs and the JSON state is `Paused`:
- `SessionManager::resume` is called automatically (transitions to
  `Resumed`, clears `paused_at`).
- The first manager turn is preceded by a synthetic `Role::User` message
  of the form `[RESUMED at <ts> — previous reason: <pause_reason>]`.
  This makes the resume visible in the manager's context and in the
  trace.
- After one manager turn completes, state auto-transitions to `Running`
  (i.e. the `Resumed` state is transient — see lifecycle diagram).

When the JSON state is `Done` or `Failed`:
- The CLI prints `session is Done/Failed, use --force-resume to reopen`
  and exits 0. With `--force-resume`, state goes to `Resumed` (from
  Done) or stays Failed (operator can read history but not continue).

When the JSON is missing but `--task-id` is given:
- The CLI errors with `no session for task 'X' — use --initial-prompt
  to start a new one` and exit code 64.

When `--task-id` is given **and** `--initial-prompt` is given AND the
JSON exists:
- The CLI errors with `session for task 'X' already exists; drop
  --initial-prompt or remove <session_path>` and exit code 64.
  (Prevents accidentally restarting a Paused session.)

## 7. `@role` injection protocol

### 7.1 Queue file format

`<worktree>/.latte/inject/<role-id>.txt` is a plain text file, one
message per line, oldest at top. The role's `AgentRunner::run_turn`
reads + drains the file atomically at the start of each turn:

```rust
// Pseudocode for the next-turn read in AgentRunner::run_turn
let inject_path = worktree_root.join(".latte/inject").join(role_id).with_extension("txt");
if inject_path.exists() {
    let content = std::fs::read_to_string(&inject_path)?;
    if !content.is_empty() {
        // Drain
        std::fs::remove_file(&inject_path)?;
        // Prepend to messages as a single synthetic user message
        let synthetic = Message {
            role: MsgRole::User,
            content: format!("[INJECTED]\n{}", content),
        };
        self.context.messages.insert(inject_history_boundary, synthetic);
    }
}
```

The drain is atomic in the sense that if `run_turn` crashes between
read and remove, the next turn re-reads the same content (duplicate
injection). v1 accepts this; v1.1 will use `rename` to a `.processing`
sibling for crash-safety.

### 7.2 REPL UX

```
$ latte-agent chat --task-id fix-redis-bug
[session: fix-redis-bug, state: Running, turn: 2]
[roles: manager, programmer, reviewer]
> 看看 redis pool 怎么写的
[manager turn 2] Sending task to model...
[manager] I'll delegate to programmer to investigate.
[programmer turn 1] Reading src/db/connection.rs...
[programmer] Found the issue. See plan.md.
> /pause
[session paused — reason: user /pause]
[REPL exiting; resume with: latte-agent chat --task-id fix-redis-bug]
```

After resume:
```
$ latte-agent chat --task-id fix-redis-bug
[session: fix-redis-bug, state: Paused → Resumed, turn: 2]
[RESUMED at 2026-06-28T07:50:00Z — previous reason: user /pause]
> @reviewer 重点看 src/db/connection.rs 的 lock 逻辑
[reviewer queue: +1 message]
> /quit
```

## 8. Tests

### 8.1 Unit tests

| Target | Test | Asserts |
|---|---|---|
| `SessionManager` | `open_or_create_writes_initial_json` | After `open_or_create` + `persist`, the file exists, parses, has `state == Created` |
| `SessionManager` | `pause_then_persist_round_trips` | `pause(reason)` then `persist` then re-read JSON → `state == Paused`, `paused_at.is_some()` |
| `SessionManager` | `invalid_transition_done_to_running` | Calling `resume()` from `Done` returns `SessionError::InvalidTransition` |
| `SessionManager` | `append_to_role_grows_messages` | `append_to_role("programmer", msg)` then `role_history("programmer")` contains `msg` |
| `SessionManager` | `persist_is_atomic_against_partial_read` | Kill mid-write (`.tmp` left) → next `open_or_create` logs warning but recovers |
| REPL parser | `empty_line_is_ignored` | `parse_line("")` returns `None` |
| REPL parser | `at_sign_routes_to_named_role` | `parse_line("@programmer foo")` returns `RoleInject { role: "programmer", message: "foo" }` |
| REPL parser | `at_sign_unknown_role_errors` | `parse_line("@ghost foo")` returns `Err(UnknownRole("ghost"))` |
| REPL parser | `slash_pause` | `parse_line("/pause")` returns `Cmd::Pause` |
| REPL parser | `plain_text_routes_to_manager` | `parse_line("look at foo.rs")` returns `ManagerInput("look at foo.rs")` |
| `@role` injection | `inject_queue_prepends_to_messages` | Drop a file at `.latte/inject/programmer.txt` with one line, run a single `run_turn` for programmer → its `messages[0]` is the synthetic `[INJECTED]` user message |
| `@role` injection | `inject_queue_drained_after_consume` | Same as above; after the turn, the inject file is gone |
| Surgical rollback | `rollback_resets_worktree_only` | 3 writes → checkpoint 2 captures v2 → rollback --id 1 → `git diff` in worktree matches the post-cp-1 state; `plan.md` byte-equal pre/post; trace JSONL byte-equal pre/post (except for the CheckpointRolledBack event appended) |
| Surgical rollback | `rollback_in_running_state_refuses` | Session is `Running` → `checkpoint rollback` returns `must pause or finish first` and does not touch worktree |
| Manager delegate | `delegate_writes_create_checkpoints` | Spawn a synthetic specialist that issues a `write` call → manifest has 2 entries (the manager's own + the specialist's) |

### 8.2 Black-box integration test

`latte-agent-cli/tests/chat_hil_e2e.rs`:

1. Init a temp git repo + worktree via `WorkspaceManager::create`
2. Spawn `latte-agent chat --task-id e2e --roles manager,programmer --initial-prompt "noop"`
   in a pseudo-tty (using `std::process::Command` + a small synthetic
   REPL driver that sends a known manager prompt + a `/pause` + exit)
3. Assert: `<worktree>/.latte/sessions/e2e-<ts>.json` exists, state is
   `Paused`, plan.md has the manager's turn 1 reply
4. Spawn `latte-agent chat --task-id e2e` again, send `@programmer
   hello`, then `/quit`
5. Assert: state is `Done` (or `Paused` depending on driver), the
   inject file is gone, programmer's history has the injected message
6. `latte-agent checkpoint rollback --task-id e2e --id 1`
7. Assert: worktree's primary file is at the post-cp-1 state, `plan.md`
   is byte-equal to the pre-rollback value (modulo any session-state
   changes that don't touch plan.md)

If the integration test fails, v1 is **not done** (per spec §11).

### 8.3 Manual smoke checklist (no test code)

The implementer must verify these in a real shell before declaring v1
done:

- [ ] `latte-agent chat --task-id smoke --roles manager,programmer --initial-prompt "noop"` enters REPL
- [ ] `/pause` exits cleanly; `~/.latte/sessions/` has the JSON
- [ ] `latte-agent chat --task-id smoke` resumes; REPL shows `[RESUMED ...]`
- [ ] `@programmer hello` in the REPL queues to `.latte/inject/programmer.txt`
- [ ] Next programmer turn (after manager delegates) reads the queue
- [ ] `latte-agent checkpoint rollback --task-id smoke --id 0` resets worktree but leaves plan.md
- [ ] `latte-agent checkpoint rollback --task-id smoke --id 0 --mode trace` errors with "v1 only supports code mode"
- [ ] `latte-agent chat` (no `--task-id`) still works as a single-role REPL
- [ ] `latte-agent discuss` and `latte-agent workflow` unchanged
- [ ] `latte-agent run --task-id X --archive --cleanup` still works

## 9. Implementation Phasing

Estimated 8 phases. The plan doc is at
`docs/superpowers/plans/2026-06-28-latte-hil-blackboard-v1-impl.md`
(written in the next step via the `writing-plans` skill).

| Phase | What it ships | Estimated complexity |
|---|---|---|
| 1 | SessionManager types + JSON persistence (no CLI wiring yet) | small |
| 2 | REPL parser: `@role` + `/pause` + `/resume` + `/quit` (no SessionManager wiring yet) | small |
| 3 | `latte-agent chat --task-id X --resume` plumbing: open SessionManager, drive turns from JSON | medium |
| 4 | `@role` injection: `RoleInjector::queue_for` + per-role drain in `AgentRunner::run_turn` | medium |
| 5 | Surgical rollback (G): deprecate `trace`/`full` modes, update CLI + `latte-agent checkpoint rollback` | small |
| 6 | Wire manager delegate to checkpoint: confirm `WorkspaceManager` wrapped `ToolManager` shared by specialists | small |
| 7 | Integration test (`tests/chat_hil_e2e.rs`) | medium |
| 8 | Docs: update README + spec cross-link | small |

Each phase ends with `cargo build` + `cargo test --workspace` green and
a single commit. No phase is "done" until its tests pass.

## 10. Risks and Open Questions

1. **Concurrent chat --task-id X from two terminals.** v1 detects this
   by holding an `flock` on `<worktree>/.latte/sessions/<id>.lock`. The
   second invocation errors with `another session is open for task 'X'`
   and exits 64. v2 replaces the flock with a real supervisor.
2. **plan.md grows unbounded.** v1 has no compaction; a long session
   bloats the JSON. v1.1 will add a `/compact` REPL command that
   rewrites plan.md + the role histories' user/assistant turns to
   keep only the most recent K turns + a summary.
3. **Specialist write race.** Two specialists running concurrently
   (manager's parallel delegate) may both write to the same file.
   `WorkspaceManager`'s `git add -A && git commit` interleaving can
   produce a corrupt patch. v1 inherits the existing
   `LATTE_AGENT_DELEGATE_CONCURRENCY=4` and 60s timeout. The risk is
   real but low (specialists are told via prompt to work in disjoint
   file paths). v1.1 will add file-level locking per specialist turn.
4. **The "use `trace` or `full` mode" deprecation message must reach
   the operator.** The error from `latte-agent checkpoint rollback
   --mode trace` must say "v1 only supports code mode" + link to
   `latte-agent run --archive --cleanup` for "discard the whole
   session" semantics. The error is `eprintln!`-ed and exits 64.
5. **Operator hand-edits `<worktree>/.latte/sessions/<id>.json` while
   a session is Running.** v1 reads JSON only at startup; mid-session
   edits are ignored until the next resume. v2 will add a SIGHUP
   reload. This is documented in the spec as a known limitation.

## 11. Acceptance Criteria (v1 done iff ALL true)

- [ ] All 12 existing `TraceEvent` variants + the 3 from `latte/blackboard-sandbox` keep working with zero modifications
- [ ] `cargo test --workspace` green; the new `tests/chat_hil_e2e.rs` passes
- [ ] `latte-agent chat --task-id X --roles manager,programmer,reviewer --initial-prompt "..."` enters the HIL REPL with the worktree + plan.md created and SessionManager in `Running` state
- [ ] `/pause` exits cleanly; on resume, the manager gets a `[RESUMED ...]` banner and the conversation continues
- [ ] `@programmer <msg>` in the REPL queues to `.latte/inject/programmer.txt`; the next programmer turn consumes the queue and gets the message prepended to its history
- [ ] `latte-agent checkpoint rollback --task-id X --id 1` resets the worktree to the post-cp-1 state; `plan.md` is byte-equal pre/post; trace JSONL is byte-equal pre/post (modulo the `CheckpointRolledBack` event append)
- [ ] `latte-agent checkpoint rollback --mode trace` and `--mode full` error out with a clear deprecation message
- [ ] `latte-agent chat` (no `--task-id`) still works as a single-role REPL
- [ ] `latte-agent discuss`, `workflow`, `list`, `config`, `debug`, `run` keep their current behavior
- [ ] README updated to point at this spec + document the new `--task-id` flag and `@role` REPL syntax
