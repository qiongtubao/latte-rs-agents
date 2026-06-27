# latte-agent

A multi-role agent CLI that runs single-role REPL chats (`chat`) and
multi-role discussions (`discuss`) over a configurable set of models
(Anthropic, OpenAI, DeepSeek, Ollama, …).

## Quick start

```bash
# Resolve deps and build
cargo build --release

# Single-role chat
latte-agent chat -r manager -t standard

# Multi-role discussion
latte-agent discuss --topic "Design REST API" \
    --roles pm,architect,programmer,reviewer

# Run a specific workflow from config/workflows/
latte-agent workflow design-review --input docs/design.md
```

## Configuration

Three-layer config: **CLI flags** > **project** (`config/agents.toml`,
`config/models.toml`) > **global** (`~/.latte/models.yaml`).
Identical `model.id` entries merge by filling empty fields — the
project wins on `api_key` when both layers define one.

```toml
# config/models.toml — tier→model mapping + role overrides
[models.tiers]
premium  = "claude-opus-4-20250514"
standard = "claude-sonnet-4-20250514"
budget   = "deepseek-chat"

[models.role_tiers.manager]
premium  = "claude-opus-4-20250514"
standard = "deepseek-v4-flash"
budget   = "deepseek-chat"
```

```toml
# config/agents.toml — role definitions
[roles.manager]
id = "manager"
category = "planning"
model_tier = "premium"
prompt_file = "prompts/manager.md"
temperature = 0.5
tools = ["read", "list", "search"]
icon = "👔"
model_chain = ["deepseek-v4-flash"]   # fallbacks appended after this
```

The `manager` role delegates to specialists via the `delegate` tool —
programmer, architect, reviewer, tester, security, devops, designer,
tech_writer, pm — each configured in the same `agents.toml`.

## Debugging & observability

`latte-agent` ships a full-chain observability layer. Enable on any
chat or discuss run with `--debug`:

```bash
latte-agent chat --debug                                   # pretty on tty, jsonl when piped
latte-agent chat --debug --debug-format jsonl              # force jsonl
latte-agent chat --debug --debug-hooks redact_pii,enforce_tool_allowlist
latte-agent chat --no-session-index                        # skip the always-on index sink
```

When `--debug` is on, every chat writes:

- A **pretty** event stream to **stdout** (or jsonl with `--debug-format jsonl`)
- A **full JSONL trace** to `~/.latte/traces/<role>.jsonl`
- A **metadata-only index** to `~/.latte/sessions/<role>.idx` (always
  on, unless `--no-session-index` is set)

The always-on index is small (one line per event, no content fields)
and gives `latte-agent debug` retroactive visibility into sessions
even when they were run without `--debug`.

### Built-in hooks

Three hooks ship in v1, registered via `--debug-hooks`:

| Hook | Point | Behavior |
|---|---|---|
| `redact_pii` | `PreCall` | Replaces phone numbers, emails, AWS keys, and OpenAI keys in outgoing messages with `<REDACTED:type>` placeholders. Hand-rolled regex; no external crate. |
| `enforce_tool_allowlist` | `PostParse` | Aborts the turn if any parsed tool call names a tool outside the role's allowed list. |
| `require_tool_call` | `PostResponse` | Aborts if the model emitted neither a `<tool_call` marker nor a substantive response (≥ 20 words by default). |

```bash
latte-agent chat --debug --debug-hooks redact_pii,enforce_tool_allowlist,require_tool_call
```

### `latte-agent debug` subcommand set

The `debug` subcommands are **read-only** against the on-disk trace
store and **never call any model API**. They take seconds, not
minutes, so iteration is fast:

```bash
latte-agent debug sessions                       # list all known sessions
latte-agent debug parse "<tool_callbash/>"       # re-run the parser on text
latte-agent debug prompt --role manager         # build the system prompt
latte-agent debug session <session-id>          # print all events for a session
latte-agent debug session <id> --kind model     # filter by event kind
latte-agent debug tokens <id>                   # token usage summary
latte-agent debug trace <id>                    # timeline with latencies
latte-agent debug replay <id>                    # re-parse stored raw outputs
```

Use `debug replay` to iterate on hook strategies against historical
traces before deploying them to a live session.

### What gets traced

Every `TraceEvent` carries a `TraceMeta` (turn, role, ts, session_id)
plus a payload. Nine variants cover the run_turn lifecycle:

| Event | When | Payload |
|---|---|---|
| `SessionStart` / `SessionEnd` | lifecycle | tier, model chain, allowed tools / totals |
| `PromptBuilt` | before model call | system prompt (full), user input, est tokens |
| `ModelCall` / `ModelRawOut` | model round-trip | model id, latency, params json, finish reason / raw content (full) |
| `ParseToolCalls` | after extract_tool_calls | raw slice, parsed calls, diagnostics |
| `ToolExec` | around `tm.execute` | resolved name, args json, latency, status (ok/err) |
| `HookFired` | every hook invocation | hook name, point, outcome kind |
| `TurnEnd` | exit | total input/output/thinking tokens, elapsed ms |

Specialist runs (manager → programmer/architect/etc.) inherit the
manager's sink via `ScopedSink`, which overrides `meta.role` on every
emitted event so manager and specialist events are distinguishable in
the same trace file.

## Architecture

```text
AgentConfig (TOML) ──▶ ModelResolver ──▶ Agent ──▶ AgentRunner
        │                    │                │            │
   roles + models      tier→model       client+tools   run_turn()
                                                     │
                                              TraceSink (Fanout of
                                              [StdoutSink, JsonlSink,
                                               IndexSink])
                                                     │
                                              HookChain (PreCall,
                                              PostResponse, PostParse,
                                              PreTool, PostTool)
```

`AgentRunner::run_turn` emits `TraceEvent` at 5 sites and runs the
`HookChain` at 5 points. `HookOutcome::Abort` surfaces as
`AgentError::HookAborted`.

## Workspace layout

```
latte-rs-agents/
├── Cargo.toml                       workspace root
├── config/
│   ├── agents.toml                  roles + tools + prompts
│   ├── models.toml                  tier mappings + model catalog
│   ├── workflows/                   discussion workflow templates
│   └── README.md                    per-file format docs
├── prompts/                         system prompt templates (one per role)
├── latte-agent-core/                 runtime: agent, runner, hooks, trace
├── latte-agent-orchestrator/        multi-role discussion logic
├── latte-agent-cli/                  `latte-agent` binary
└── latte-rs-model-router/            (legacy) provider clients
```

## Status

This project has completed:

- **Tasks 1-8** (commit `8c2f2b1`): `TraceSink` infrastructure
  (`NullSink`, `JsonlSink`, `StdoutSink`, `IndexSink`, `FanoutSink`,
  `ScopedSink`), full `TraceEvent` enum (9 variants), `Hook` trait +
  `HookChain`, three built-in hooks (`RedactPii`, `EnforceToolAllowlist`,
  `RequireToolCall`).
- **Task 9** (commit `a6e3503`): `AgentRunner` gained `sink`,
  `hooks`, `role_id` fields with builder methods.
- **Tasks 10-11** (commit `1d9fb65`): `run_turn` emits at 5 sites and
  runs hooks at 5 points; `delegate` tool wraps manager sink in
  `ScopedSink` so specialist events appear under their own role.
- **Task 12** (commit `e6f1163` + closeout `eddca2a`): `latte-agent
  debug` subcommand set (7 subcommands) + `--debug*` flag wiring on
  `chat` and `discuss`.
- **Task 13** (commit `b9ef153`): `--debug` wiring on `discuss`.

The end-to-end loop (`DISCOVER → PLAN → EXECUTE → VERIFY → ITERATE`)
that this plan enables is exercised by running the system against a
real workload and inspecting `~/.latte/traces/*.jsonl` with
`latte-agent debug trace <id>`.

## HIL Blackboard (v1)

Run a multi-role agent session inside a git worktree with a `plan.md` blackboard, support for `/pause` + `/resume`, and `@<role>` injection routed to a specific specialist:

```bash
# Start a session (auto-creates the worktree + plan.md)
latte-agent run --task-id fix-redis-bug --initial-prompt "Redis pool doesn't recycle after 5xx"

# Open the HIL REPL
latte-agent chat --task-id fix-redis-bug --roles manager,programmer,reviewer

# In the REPL:
#   > look at the redis pool
#   > /pause                  # exits; state persisted to .latte/sessions/<id>.json
#   > @programmer check this  # queues a message for programmer's next turn
#   > /resume                 # (only valid if state is Paused)
#   > /quit                   # marks the session Done

# Resume from outside the REPL
latte-agent chat --task-id fix-redis-bug

# Inject from another terminal
latte-agent inject --task-id fix-redis-bug --role programmer --message "..."

# Pause / resume from outside the REPL
latte-agent pause  --task-id fix-redis-bug
latte-agent resume --task-id fix-redis-bug

# Surgical rollback: reset the worktree code, keep plan.md and trace
latte-agent checkpoint rollback --task-id fix-redis-bug --id 3

# Archive + cleanup
latte-agent run --task-id fix-redis-bug --archive --cleanup
```

The worktree lives at `<repo>/.latte/worktrees/<task-id>/`; the base
branch HEAD stays clean for the entire session. Session state is
persisted atomically to `<worktree>/.latte/sessions/<id>.json` and is
human-readable / hand-editable (operators can delete "毒药消息" while
paused). Six new `TraceEvent` variants (`SessionStarted`, `SessionPaused`,
`SessionResumed`, `RoleInjected`, plus the inherited `CheckpointCreated` and
`CheckpointRolledBack` from the `WorkspaceManager`/`CheckpointEngine`
modules) land in `~/.latte/traces/<id>.jsonl`.

See `docs/superpowers/specs/2026-06-28-latte-hil-blackboard-v1-design.md`
for the design and `docs/superpowers/plans/2026-06-28-latte-hil-blackboard-v1-impl.md`
for the implementation plan.

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

## Roadmap

Per spec `docs/superpowers/specs/2026-06-25-latte-agent-debug-observability-design.md` §3:

- **Checkpoint / node-level retry** — the v1 of the HIL Blackboard
  system ships in this release (see the "HIL Blackboard (v1)"
  section above). The v2 spec will add full state serialization,
  restore-from-snapshot, retry budget, and a real `SessionManager`.