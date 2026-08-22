# Task Refine Workflow Contract Fix Implementation Plan

> **For agentic workers:** Implement task-by-task with tests first and verify each step before completion.

**Goal:** Make `task_refine` distinguish approval, rework, and terminal rejection without allowing a gate to pass a draft that still has blocking issues.

**Architecture:** Keep the existing four-step workflow and Markdown draft format. Change only the gate protocol: reviewer/gate use `VERDICT: ACCEPT`, `VERDICT: REVISE`, or `VERDICT: REJECT`; only `ACCEPT` reaches submit; gate reports the decision and issues without copying the draft. The submit step continues to receive the latest `draft` variable.

**Tech Stack:** TOML workflow definitions, Rust workflow parser/validation tests, Cargo test.

---

### Task 1: Lock the gate contract with regression tests

**Files:**
- Modify: `latte-agent-core/src/workflow.rs` near `loop_tests`
- Test: `latte-agent-core/src/workflow.rs`

- [ ] Add tests that load both task-refine configuration files and assert the gate loop token is `VERDICT: ACCEPT`, the gate output contract requires `VERDICT:`, and the gate prompt contains no instruction to append the full draft.
- [ ] Add a test that the task-refine submit prompt still consumes the latest `draft` value and does not reference gate output as the draft source.
- [ ] Run `cargo test -p latte-agent-core workflow::loop_tests::task_refine_gate_contract` and confirm it fails against the current configuration.

### Task 2: Fix the task-refine workflow definitions

**Files:**
- Modify: `.latte/workflows.d/task_refine.toml:62-98`
- Modify: `config/workflows/task_refine.toml:62-98`

- [ ] Replace `loop_until = "VERDICT: PASS"` with `loop_until = "VERDICT: ACCEPT"`.
- [ ] Define the gate outcomes explicitly: `ACCEPT` continues to submit, `REVISE` loops to refine, and `REJECT` is terminal.
- [ ] Remove the gate instructions that copy or append the complete draft.
- [ ] Require the gate to output only the verdict plus actionable issue records, preserving file/line evidence for `REVISE` and `REJECT`.
- [ ] Keep the output contract at `require = ["VERDICT:"]` so `REVISE` and `REJECT` remain valid gate outputs.

### Task 3: Verify the fixed contract and existing loop behavior

**Files:**
- No new files.

- [ ] Run the focused Rust contract test and the existing serial/DAG loop tests.
- [ ] Run `cargo test -p latte-agent-core workflow::loop_tests`.
- [ ] Parse both TOML files through the workflow validator and confirm no configuration errors.
- [ ] Run `git diff --check` for the changed files and record the exact test counts.
