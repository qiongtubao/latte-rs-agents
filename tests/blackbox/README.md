# Black-box tests for `latte-agent`

Two layers of black-box coverage for the `latte-agent` CLI:

| Layer | Path | Format | Speed | Needs API key? |
|-------|------|--------|-------|----------------|
| Rust integration | `latte-agent-cli/tests/blackbox/` | `.rs` (10 tests) | Fast default; ignored subset needs `DEEPSEEK_API_KEY` | 5 default no, 5 ignored yes |
| Bash smoke | `tests/blackbox/` | `.sh` (5 scripts) | < 30s total | 4 no, 1 SKIPs without key |

## What's black-box vs the existing unit tests

The existing `cargo test --lib` and per-file `#[cfg(test)] mod tests`
in `latte-agent-core`, `latte-agent-orchestrator`, and
`latte-agent-cli` cover the agent runtime, the trace sinks, the
tool-call parser, and the JSONL round-trip. Those tests construct
the components directly and stub out the model client.

**This suite** tests the *binary* — the user-facing surface. It
spawns `target/debug/latte-agent` as a subprocess and asserts on
the real exit code, stderr, stdout, and the JSONL trace the
binary writes to `$LATTE_HOME/traces/<session-id>.jsonl`. No mocks.

## Layout

```text
latte-agent-cli/tests/blackbox.rs            # integration-test crate root
latte-agent-cli/tests/blackbox/mod.rs        # the 10 #[test] functions
latte-agent-cli/tests/blackbox/common/mod.rs # TestEnv, RunOutput, helpers

tests/blackbox/smoke_help.sh                 # --help has --debug/--tier/-r
tests/blackbox/smoke_invalid_role.sh         # bogus role → clear error
tests/blackbox/smoke_list_roles.sh           # manager + 3 reviewer roles
tests/blackbox/smoke_delegation_trace.sh     # manager pipeline (needs key)
tests/blackbox/smoke_cjk_no_panic.sh         # UTF-8 throughout
tests/blackbox/run-all.sh                    # runs the 5 above, reports counts
tests/blackbox/README.md                     # this file
```

## Running

### Bash smoke (fast, no model calls)

```bash
bash tests/blackbox/run-all.sh
```

### Rust integration tests

```bash
# Default suite — no API key, no model calls, runs in <30s.
cargo test --test blackbox

# Full suite including the 5 #[ignore]'d tests that exercise the
# manager→programmer→reviewer chain end-to-end. Requires
# DEEPSEEK_API_KEY in the environment. Run sequentially so the
# delegated specialists don't all hit the API at once:
DEEPSEEK_API_KEY=... cargo test --test blackbox -- --ignored --test-threads=1
```

## The 15 tests

### Rust integration (`latte-agent-cli/tests/blackbox/`)

| # | Test | Verifies | API key |
|---|------|----------|---------|
| 1 | `direct_answer_small_question` | Trace carries `SessionStart`/`PromptBuilt`/`SessionEnd`; user input lands in the prompt | yes (`#[ignore]`) |
| 2 | `delegate_dispatch_visible` | stderr shows `→ delegating to <role>`; JSONL has `ToolExec{name=delegate}`; ScopedSink tags specialist events with their role | yes (`#[ignore]`) |
| 3 | `three_layer_review_chain` | Two `ToolExec{delegate}` events (`programmer`, `reviewer_sanity`); sanity's response contains `VERDICT:` | yes (`#[ignore]`) |
| 4 | `tier_override` | `--tier premium` appears in runner-built log + `SessionStart.tier` | yes (`#[ignore]`) |
| 5 | `cjk_input_does_not_panic` | Chinese input doesn't trigger a panic; trace parses cleanly | no |
| 6 | `hook_redact_pii` | `HookFired{hook=redact_pii, point=PreCall}` fires; `PromptBuilt.user_input` is redacted (`<REDACTED:phone>`) | no |
| 7 | `filter_sink_filters_stdout` | `--debug-events ToolExec,HookFired` filter is applied to stdout only; JSONL still carries all events | no |
| 8 | `per_model_timeout` | `LATTE_AGENT_DELEGATE_TIMEOUT_SECS=2` produces `timed out after 2s`; `SessionEnd` still emitted | yes (`#[ignore]`) |
| 9 | `latte_home_env_override` | Trace JSONL lands under `$LATTE_HOME/traces/`, not `~/.latte/` | no |
| 10 | `session_resume` | `--resume <path>` loads history into context (`history_len >= 2`); stderr shows `[resume] loaded N messages` | no |

### Bash smoke (`tests/blackbox/`)

| # | Script | Verifies |
|---|--------|----------|
| 11 | `smoke_help.sh` | `chat --help` lists `--debug`, `--tier`, `-r/--role` |
| 12 | `smoke_invalid_role.sh` | `chat -r bogus_role` → stderr contains `role ... not found` |
| 13 | `smoke_list_roles.sh` | `list roles` shows `manager`, `reviewer_sanity`, `reviewer_architecture`, `reviewer_security` |
| 14 | `smoke_delegation_trace.sh` | Output contains `→ delegating to` / `VERDICT:` / `Why no delegation` (any one). **SKIPs without `DEEPSEEK_API_KEY`** |
| 15 | `smoke_cjk_no_panic.sh` | Chinese input doesn't trigger `panicked at`; the `turn response` log line lands |

## The slow-test convention

Tests that make a real model call are tagged one of two ways:

* Rust tests: `#[ignore = "requires DEEPSEEK_API_KEY"]`. They run
  only with `cargo test --test blackbox -- --ignored`.
* Bash tests: print `SKIP` and exit 0 when the API key is absent
  (e.g. `smoke_delegation_trace.sh`).

This keeps the default `cargo test` cycle under 30s and free of
token charges. Developers with a working `DEEPSEEK_API_KEY` can
opt in by setting the env var and re-running.

## Test isolation

Every test creates its own `tempfile::TempDir`, sets
`LATTE_HOME=<that dir>` in the subprocess environment, and clears
the `DEEPSEEK_API_KEY` / `ANTHROPIC_API_KEY` / `GLM_API_KEY` /
`OPENAI_API_KEY` vars so a developer's global shell config can't
leak in. The temp dir is removed on Drop.

The bash tests use `mktemp -d` and `trap ... EXIT` for the same
effect.

## Caveats the tests document

* **`chatlog::global_log_dir()` ignores `LATTE_HOME`.** The chat
  log lands at `~/.latte/logs/` regardless of where traces go.
  The Rust tests work around this by snapshotting `~/.latte/logs/`
  at `TestEnv::new()` time and diffing after each run to find the
  new file. Bash tests don't depend on this.
* **`save_to_default` (auto-save on turn failure) also ignores
  `LATTE_HOME`.** Test 10 (`session_resume`) hand-crafts a save
  file in `$LATTE_HOME` to exercise `--resume` without first having
  to deliberately fail a turn.
* **The manager prompt mandates delegation for every substantive
  task**, so Test 1's expected `NO ToolExec{name=delegate}` is
  relaxed: we verify trace shape + user input rather than absence
  of delegation. Tests 2/3 still require delegation, but they
  accept any number of delegates (not just one).
* **DEEPSEEK_API_KEY is required** for the five `#[ignore]`'d
  Rust tests and one bash script. Without it the model call fails
  fast with `"model 'X' has no api_key configured"`, the trace
  still carries `SessionStart` / `PromptBuilt` / `SessionEnd`, and
  the five default tests pass.

## Adding a new test

1. Rust: add a `#[test] fn` to `tests/blackbox/mod.rs`. Use
   `TestEnv::new()` for isolation. Tag `#[ignore]` if it makes a
   real model call.
2. Bash: drop a `smoke_*.sh` script in `tests/blackbox/`. End with
   `echo "PASS"`, or `echo "SKIP: <reason>" ; exit 0` if the test
   needs a secret the runner doesn't have.
3. Update the test table in this README.