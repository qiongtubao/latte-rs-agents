# latte-agent Debug Observability + Hook Mechanism

**Date:** 2026-06-25
**Status:** Draft (post-brainstorming, pre-implementation)
**Scope:** TraceSink-based full-chain observability + lifecycle Hook mechanism.
**Out of scope:** Checkpoint-based node-level retry / 语义级崩溃恢复 — deferred to a future spec.

## 1. Background

A real chat session (2026-06-24) exposed a class of bugs that are nearly
invisible from the existing logs:

- The model emitted `<tool_calldelegate>...</tool_calldelegate>` but the
  parser looked for `</tool_call>`. The chat log only carried a 200-char
  preview, so the divergence was hidden until a unit test was written.
- Three specialists returned empty / "tool rounds exhausted" but the
  chatlog summary gave no clue which step in the run_turn loop failed.
- A commit (d5bd7d1) silently shifted the prompt format and the parser
  was rewritten incompletely; the regression sat until someone ran the
  chat command with the right input.

There is also no mechanism to:
- Intercept model output (e.g. block a known-hallucinated tool name)
- Mutate prompts before they go out (e.g. PII redaction)
- Test interception strategies offline against historical traces

This spec adds a traceable, hookable, replayable layer on top of the
existing `AgentRunner::run_turn` loop.

## 2. Goals

1. **Full-chain observability** for every chat / discuss turn:
   - The exact prompt sent to the model (system + history + current input), untruncated
   - The raw model output (full content, not a 200-char preview)
   - The parser's output (parsed tool calls + parse diagnostics)
   - Each tool execution (name, args, result, error, latency)
   - Token usage broken down by input / output / thinking, per turn
2. **Lifecycle Hooks** at well-defined points (pre-call, post-response,
   pre-tool, post-tool) that can: continue, mutate, abort, or request a
   retry-with-correction.
3. **Offline replay** — re-run a recorded session's parse / hook logic
   against the stored raw events, without any API call. This is the
   diagnostic primitive: re-parse a past turn with a new parser variant,
   re-run a hook against a past prompt, diff the outcomes.
4. **Zero overhead when disabled.** No `--debug`, no `NullSink` work
   measurable beyond a single virtual call.

## 3. Non-Goals (deferred)

- **Checkpoint / node-level retry / 语义级崩溃恢复** — state
  serialization, restore, retry budgets, context pruning. Will be a
  separate spec. Hooks here are designed to be the *trigger* layer for
  that future work (a `HookOutcome::Retry { correction }` is reserved in
  the outcome enum but not wired into run_turn semantics in this spec).
- Replacing or altering `chatlog.rs`'s human-readable log format.
- External (dynamic-library) hooks — only built-in Rust hooks in this
  spec. The hook registry is structured to admit external ones later.
- Trace encryption / redaction / upload / sync.
- Automatic trace rotation / cleanup.

## 4. Architecture Overview

```
                       ┌───────────────────────────────────────┐
                       │  AgentRunner::run_turn                │
                       │                                       │
   user input ───────► │  ┌─────────┐  ┌──────────┐           │
                       │  │  hooks  │  │  sinks   │           │
                       │  │  chain  │  │ (trace)  │           │
                       │  └────┬────┘  └────┬─────┘           │
                       │       │  emit      │                  │
   build prompt ─────► │       ▼            ▼                  │
   pre_call hook ────► │  chat() ─► raw + parsed ─► tm.exec()  │
   post_resp hook ───► │       │            │           │     │
   post_parse hook ───►│       ▼            ▼           ▼     │
   pre_tool hook ─────►│     TurnEnd ──► events emitted         │
   post_tool hook ────►│                                       │
                       └───────────────────────────────────────┘

   sinks:  NullSink (default), StdoutSink (pretty), JsonlSink (file),
           IndexSink (always-on metadata index)
   hooks:  user-registered chain, default empty
```

Sinks are append-only observers. Hooks are *interceptors* — they run
inline and can mutate or abort. A hook that wants to *also* observe
emits events through a sink.

## 5. Trace Data Model

All observability flows through one event enum. Every event carries
`turn: u32`, `role: String`, `ts: String` (UTC ISO8601), so manager
+ 3 specialists' events interleave cleanly in one trace file.

```rust
pub struct TraceMeta {
    pub turn: u32,
    pub role: String,        // "manager", "programmer", ...
    pub ts: String,          // "2026-06-25T05:42:16.123Z"
    pub session_id: String,
}

pub enum TraceEvent {
    SessionStart {
        meta: TraceMeta,
        tier: String,
        model_chain: Vec<String>,
        allowed_tools: Vec<String>,
    },
    PromptBuilt {
        meta: TraceMeta,
        system_rendered: String,    // full, untruncated
        history_len: usize,
        user_input: String,
        est_input_tokens: u32,
    },
    ModelCall {
        meta: TraceMeta,
        model_id: String,
        params_json: String,        // serialized GenerateParams
        latency_ms: u64,
        finish_reason: String,      // "stop" | "length" | "tool_use" | "error"
    },
    ModelRawOut {
        meta: TraceMeta,
        raw_content: String,        // full, untruncated
    },
    ParseToolCalls {
        meta: TraceMeta,
        raw_in: String,             // the slice of ModelRawOut we parsed
        parsed: Vec<ParsedCall>,    // (name, args) tuples
        diagnostics: ParseDiag,     // opens found, closes matched, etc.
    },
    ToolExec {
        meta: TraceMeta,
        name: String,               // resolved (bash → exec applied)
        args_json: String,
        latency_ms: u64,
        status: ToolStatus,         // Ok(Result) | Err(String)
    },
    HookFired {
        meta: TraceMeta,
        hook_name: String,
        point: HookPoint,
        outcome_kind: String,       // "continue" | "mutate" | "abort" | "retry"
    },
    TurnEnd {
        meta: TraceMeta,
        total_input: u32,
        total_output: u32,
        total_thinking: u32,
        elapsed_ms: u64,
    },
    SessionEnd {
        meta: TraceMeta,
        total_turns: u32,
        total_input: u32,
        total_output: u32,
        total_thinking: u32,
    },
}

pub struct ParsedCall { pub name: String, pub args: String }
pub struct ParseDiag {
    pub opens_found: u32,
    pub closes_matched: u32,
    pub unmatched_opens: Vec<String>,   // raw slices of `<tool_call...` with no close
}
pub enum ToolStatus { Ok(String), Err(String) }
```

## 6. Sinks

```rust
pub trait TraceSink: Send + Sync {
    fn emit(&self, event: TraceEvent);
}

pub struct NullSink;                       // default; `fn emit(&self, _) {}`
pub struct StdoutSink { pretty: bool };    // multi-line, color when tty
pub struct JsonlSink { path: PathBuf };    // append mode, BufWriter
pub struct IndexSink  { path: PathBuf };   // metadata-only, always-on
pub struct FanoutSink { sinks: Vec<Arc<dyn TraceSink>> }
```

`NullSink` is a `#[inline]` empty function; the compiler eliminates
the call entirely. `IndexSink` is independent of `--debug` and is the
one that always writes, so `latte-agent debug sessions` works
retroactively.

`StdoutSink` pretty format example:

```
─── turn 1 · role=manager · 05:42:16.123Z ───
[PromptBuilt] est_input=1240
[ModelCall] model=deepseek-v4-flash latency=2741ms finish=stop
[ModelRawOut] 1166 chars
  <tool_calldelegate> {"role": "programmer", ...}</tool_calldelegate>
[ParseToolCalls] parsed=3 (delegate, reviewer, security)
[ToolExec] name=delegate latency=842ms status=Ok(...)
[TurnEnd] in=1240 out=1166 think=0 elapsed=28412ms
```

## 7. Hooks

### 7.1 Hook points

```rust
pub enum HookPoint {
    PreCall,        // before chat() — can mutate messages, abort
    PostResponse,   // after model response, before parse — can abort, retry
    PostParse,      // after parse_tool_calls — can abort (e.g. unknown tool)
    PreTool,        // before tm.execute() — can mutate args, abort
    PostTool,       // after tm.execute() — can mutate result
}
```

### 7.2 Outcomes

```rust
pub enum HookOutcome<T> {
    Continue,                   // no change, proceed
    Mutate(T),                  // replace value, proceed
    Abort { reason: String },   // stop run_turn, return AgentError::HookAborted
    Retry { correction: String, /* wired in future spec */ _phantom: () },
}
```

`Retry` is reserved in this spec but **not yet wired** into `run_turn`
semantics. A `Retry` outcome today behaves like `Continue` (logged) so
the hook runs without crashing. The future Checkpoint spec will
implement the actual retry-with-rollback behavior.

### 7.3 Hook trait + chain

```rust
pub trait Hook: Send + Sync {
    fn name(&self) -> &str;
    fn pre_call(&self, _ctx: &mut PreCallCtx) -> HookOutcome<()> { HookOutcome::Continue }
    fn post_response(&self, _ctx: &mut PostResponseCtx) -> HookOutcome<()> { HookOutcome::Continue }
    fn post_parse(&self, _ctx: &mut PostParseCtx) -> HookOutcome<Vec<ParsedCall>> { HookOutcome::Continue }
    fn pre_tool(&self, _ctx: &mut PreToolCtx) -> HookOutcome<serde_json::Value> { HookOutcome::Continue }
    fn post_tool(&self, _ctx: &mut PostToolCtx) -> HookOutcome<String> { HookOutcome::Continue }
}

pub struct HookChain { hooks: Vec<Arc<dyn Hook>> }
impl HookChain {
    pub fn run_pre_call(&self, ctx: &mut PreCallCtx) -> HookOutcome<()> { /* first Abort wins */ }
    // ... per-point
}
```

`HookChain` runs all hooks in registration order; first `Abort` wins.
`Mutate` values are passed to the next hook in the chain (composed
left-to-right). Each run emits a `HookFired` TraceEvent through the
attached sink.

### 7.4 Built-in hooks (this spec ships three)

| Name | Point | Behavior |
|---|---|---|
| `RedactPii` | `PreCall` | Replace Chinese phone numbers (11 digits, optional +86), email addresses, AWS access keys (`AKIA[0-9A-Z]{16}`), and OpenAI-style API keys (`sk-…` and `sk-ant-…` prefixes) with `<REDACTED:phone/email/aws_key/api_key>`. Pure regex, no external crate. |
| `EnforceToolAllowlist` | `PostParse` | For each parsed call, verify `name ∈ allowed_tools`. If any call is unknown, `Abort` with a precise message ("tool 'foo' not in allowed list: read, write, exec, search, list"). |
| `RequireToolCall` | `PostResponse` | If parser returns zero tool calls AND the response is not a final answer (heuristic: no paragraph ≥ 20 words), `Abort` with "expected tool call or substantive final answer, got empty/short response". |

A failing hook is **observable** through trace: `HookFired { outcome_kind: "abort", ... }` is emitted with the reason.

### 7.5 Hook registration

In-session: `AgentRunner::with_hooks(Arc<HookChain>)` builder method.
In-config (future, deferred): a `config/hooks.toml` mirroring `agents.toml` style. Not in this spec.

## 8. UX Surface

### 8.1 Online — `--debug` flag

```bash
latte-agent chat --debug
latte-agent chat --debug --debug-format jsonl      # raw JSONL to stdout
latte-agent discuss --debug --topic "..."
latte-agent chat --debug --debug-hooks redact_pii,enforce_tool_allowlist
```

When `--debug` is on, the runner wires a `FanoutSink` of `[StdoutSink, JsonlSink]`
When `--debug` is on, the runner wires a `FanoutSink` of `[StdoutSink, JsonlSink]`
plus the always-on `IndexSink`. When off, only `IndexSink` is active
(so `latte-agent debug sessions` / `debug tokens` work retroactively
for every past session, even ones run without `--debug`).

New flags:
- `--debug` — enable full trace (pretty to stdout + jsonl to disk)
- `--debug-format <pretty|jsonl>` — stdout format (default: pretty if tty, jsonl otherwise)
- `--debug-hooks <names>` — register comma-separated built-in hook names for this session
- `--no-session-index` — opt out of always-on index sink (sensitive environments)

### 8.2 Offline — `latte-agent debug` subcommand set

| Command | What it does | API calls? |
|---|---|---|
| `debug parse <text>` | Feed `<text>` to `extract_tool_calls`. Print raw + parsed calls + diagnostics. | no |
| `debug prompt --role <id> [--tier <t>] [--input <text>]` | Build the real system prompt + history skeleton for the role. Show full content + token estimate. | no |
| `debug session <id> [--kind <kind>]` | Print a recorded session's events, filterable by `kind` (prompt/model/parse/tool/hook/token). | no |
| `debug sessions` | List `~/.latte/traces/` and `~/.latte/sessions/`. | no |
| `debug tokens <id>` | Aggregate input/output/thinking by turn and role. | no |
| `debug trace <id>` | Timeline: one line per event, latency from previous event. | no |
| `debug replay <id> [--parser <v>] [--hook <name>...]` | Re-run parse and/or hooks against stored `ModelRawOut` events. Diff against original. | no |


All `debug` subcommands are **read-only** against the trace store and
**never call any model API**. This is the safety property that makes
`debug replay` useful: you can iterate on hook strategies against real
prompts and responses without burning tokens.

### 8.3 `--debug-hooks` example

```bash
$ latte-agent chat --debug --debug-hooks enforce_tool_allowlist,redact_pii
...
[HookFired] redact_pii PreCall continue (3 redactions applied)
[HookFired] enforce_tool_allowlist PostParse abort: tool "exec_shell" not in allowed list
run_turn aborted: hook 'enforce_tool_allowlist' aborted at PostParse: ...
```

## 9. Storage Layout

```
~/.latte/
├── logs/
│   └── chat-20260625-054204-73546.log       # existing human log, unchanged
├── sessions/                                # NEW: always-on metadata index
│   └── 20260625-054204-73546.idx
└── traces/                                  # NEW: full trace (only on --debug)
    └── 20260625-054204-73546.jsonl
```

- `sessions/<id>.idx` — line-oriented JSON, one event per line, but
  **metadata only** (no `system_rendered`, no `raw_content`, no
  `args_json`, no `result`). Always written. ~50–200 bytes per event.
- `traces/<id>.jsonl` — line-oriented JSON, full `TraceEvent` per line.
  Written only when `--debug` is on. ~1–10 KB per event.
- Session IDs reuse the existing `chat-YYYYMMDD-HHMMSS-PID` format so
  log ↔ idx ↔ jsonl correlate by name.

## 10. Library API (latte-agent-core)

### 10.1 New module: `src/trace.rs`

Holds `TraceEvent`, `TraceSink`, the four sink implementations, and
`Hook` / `HookOutcome` / `HookChain`. No I/O dependencies beyond `std`.

### 10.2 AgentRunner changes

```rust
pub struct AgentRunner {
    // ... existing fields ...
    sink: Arc<dyn TraceSink>,        // default: Arc::new(NullSink)
    hooks: Arc<HookChain>,           // default: Arc::new(HookChain::empty())
    role_id: String,                 // for TraceMeta.role
}
```

New builder:
```rust
impl AgentRunner {
    pub fn with_sink(self, sink: Arc<dyn TraceSink>) -> Self;
    pub fn with_hooks(self, hooks: Arc<HookChain>) -> Self;
}
```

Five `sink.emit(...)` call sites inside `run_turn`:
1. after prompt assembly → `PromptBuilt`
2. after `agent.chat()` returns → `ModelCall`, `ModelRawOut`
3. after `extract_tool_calls` → `ParseToolCalls`
4. after `tm.execute()` → `ToolExec`
5. at turn exit → `TurnEnd`

Seven `hooks.run_*()` call sites (one per (point × gate)): pre-call,
post-response, post-parse, pre-tool, post-tool. Each emits `HookFired`
through the sink.

### 10.3 delegate tool plumbing

`register_delegate_tool` clones the manager's `sink` and wraps it in
`ScopedSink { inner: sink, role: specialist_id }` so specialist events
appear under their own role in the same trace file.

```rust
let scoped = Arc::new(ScopedSink::new(manager_sink, role.id.clone()));
let mut specialist = AgentRunner::new_with_tools(agent, tm, 8)
    .with_sink(scoped)
    .with_hooks(specialist_hooks);   // currently empty
```

### 10.4 Hook module: `src/hooks/mod.rs`

- `mod builtin` — `RedactPii`, `EnforceToolAllowlist`, `RequireToolCall`
- `mod chain` — `HookChain`
- `mod builtin::redact_pii` — regex table (no external crate)

## 11. CLI changes (latte-agent-cli)

- `ChatCmd` and `DiscussCmd` gain `--debug`, `--debug-format`,
  `--debug-hooks`, `--no-session-index` flags.
- New top-level subcommand: `Command::Debug(DebugCmd)` with the 7
  sub-subcommands listed in §8.2.
- Existing `chatlog.rs` is **untouched** — it keeps writing
  `~/.latte/logs/chat-*.log` exactly as today.
- New `commands/debug.rs` module + `commands/trace_store.rs` (reads
  `traces/*.jsonl` and `sessions/*.idx`).

## 12. Rollout

Each step is independently testable; mergeable as a sequence of small PRs.

1. **trace.rs in core** — `TraceEvent`, `TraceSink` trait, `NullSink`,
   `JsonlSink`, `StdoutSink`, `FanoutSink`, `IndexSink`, `ScopedSink`.
   Unit tests for each sink (esp. `NullSink` zero-allocation, `JsonlSink`
   append-mode correctness, `FanoutSink` ordering).
2. **AgentRunner wiring** — add fields, builder methods, 5 emit sites
   in `run_turn`. Existing 80 tests must still pass; new test asserts
   that a `VecSink` (test-only) receives expected event sequence for a
   2-round tool-call turn.
3. **Hooks in core** — `Hook` trait, `HookOutcome`, `HookChain`,
   `PreCallCtx` / `PostResponseCtx` / etc. New unit tests per built-in
   hook.
4. **Built-in hooks** — `RedactPii` (with regex test cases including
   each PII type), `EnforceToolAllowlist` (allowlist + abort), `RequireToolCall`
   (empty / short / substantive response).
5. **delegate plumbing** — `ScopedSink` in the delegate tool. New
   integration test: a manager delegates to 3 specialists; assert the
   captured `VecSink` shows events from all 4 roles.
6. **CLI flags** — `--debug` / `--debug-format` / `--debug-hooks` /
   `--no-session-index`. Default behavior unchanged. New test: chat
   with `--debug --debug-format jsonl` produces parseable JSONL to
   stdout.
7. **`debug` subcommand set** — 7 subcommands (parse, prompt, session,
   sessions, tokens, trace, replay). Each ships with at least one
   self-test (`assert_cmd` style). For `debug parse` / `debug replay`,
   the round-trip test re-parses a known string and asserts the parsed

8. **Docs** — README section "Debugging", spec index updated, CHANGELOG
   entry.

## 13. Testing Strategy

- **Unit (core)**: sink implementations, hook chain composition, each
  built-in hook's regex / allowlist / heuristic.
- **Integration (cli)**: `--debug` round-trip — run a chat with a fake
  model, assert trace contains expected events. `debug parse` CLI test
  with a hand-crafted multi-call string.
- **Regression**: existing 80 core tests + existing CLI tests must
  continue to pass.
- **Manual**: run a real chat session with `--debug` after the change,
  diff the produced `traces/*.jsonl` against expected structure.

## 14. Risks

- **Disk growth** — `sessions/*.idx` is small but never cleaned. Acceptable
  for v1; document in README that the user can `rm -rf ~/.latte/sessions`
  and `~/.latte/traces` periodically. No auto-rotation in this spec.
- **Sensitive data in traces** — `--debug` writes the *full* prompt,
  which may include PII, secrets the user pasted, or tool output the
  user doesn't want persisted. Mitigated by: (a) opt-in via `--debug`,
  (b) `--no-session-index` for the always-on path, (c) README warning.
  Real redaction-at-rest is a follow-up.
- **Sink ordering** — if a sink panics, others may not get the event.
  `FanoutSink` runs sinks in order and isolates panics (catch_unwind)
  so one bad sink doesn't kill the run. Test this.
- **Hook chain blow-up** — long hook chains multiply overhead. Document
  the per-turn cost; offer `--max-hook-ms <n>` budget in a future spec.
- **Backwards compat** — `NullSink` default keeps every existing call
  site binary-equivalent (no extra fields, no extra methods). The
  builder methods are additive. The `Command::Debug` enum variant is
  additive to the CLI.

## 15. Out-of-Scope Reminder (for the future spec)

Checkpoint / node-level retry is intentionally **not** in this spec.
The `HookOutcome::Retry { correction }` variant is reserved so a future
spec can wire it up without changing hook implementations. That future
spec will cover:
- Agent state serialization (system prompt, history, tool state)
- Checkpoint store (keyed by session + turn + step)
- Restore semantics
- Retry budget + escalation

## 16. Open Questions

1. **Hook chain composition order** — left-to-right, all `Mutate`
   values pass through. Is "first Abort wins" the right semantics, or
   should we have `BestEffort` mode that runs all and reports the most
   severe outcome? (Default: `BestEffort` = first Abort wins. Leave the
   alternative for v2.)
2. **`debug save` semantics** — should this be the user's path to
   promote a `IndexSink`-only session to a full trace? If the index
   doesn't include raw content, this is a no-op. Either drop the
   command, or make it a hint to re-run the session with `--debug`.
   Decision: **drop** `debug save` in v1; document that `--debug` is
   required up front for full trace.
3. **PII redaction false positives** — phone-number regex may catch
   unrelated 11-digit runs (order IDs, timestamps). Acceptable for v1
   (false positives are recoverable; false negatives are not). User can
   disable with `--no-hooks redact_pii`.
