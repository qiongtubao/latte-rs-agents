# Plan: DiscussionOrchestrator → ChatController Unification

## Context

`DiscussionOrchestrator` owns workflow-step scheduling (nested round→step→speakers) and consensus, but duplicates pause/resume/events infrastructure that `ChatController` already provides. Goal: make `DiscussionOrchestrator` use `ChatController` internally so it gets pause/resume/events/ask_human/trace/SessionManager for free.

**Current scheduling difference:**
- `run_multi_role_loop` (in `ChatController`): flat round-robin on `order` (a `Vec<String>` of role ids). For each round, for each role_id, run_turn. Infra: SessionManager, RoundScheduler, Supervisor, inject queue, plan slice.
- `DiscussionOrchestrator::run()`: for each round, for each `WorkflowStep`, for each `step.speakers`, run_turn with a step-specific prompt. Then consensus check. No SessionManager/RoundScheduler/Supervisor.

The two are fundamentally different scheduling patterns that resist a single "add workflow steps to run_multi_role_loop" approach without massive complexity.

## Design Decision

**Do not add WorkflowStep awareness to `run_multi_role_loop`.** Instead:

1. Keep `run_multi_role_loop` unchanged — it is the HIL chat mode with human-in-the-loop input.
2. Add a **new mode** to `ChatController`/`run_driver`: `MultiRoleWorkflow` mode that iterates `DiscussionWorkflow` steps internally.
3. `DiscussionOrchestrator::run()` becomes a thin wrapper: build `ControllerConfig` containing workflow info, call `ChatController::spawn`, collect events, extract `DiscussionResult`.

This separates concerns cleanly:
- `run_multi_role_loop` = HIL round-robin with human input at each round.
- New workflow loop = step-structured scheduling, no human input per turn, uses same channel infrastructure.
- Both share pause/resume/cancel infrastructure and the `ChatEvent` broadcast.

## Detailed Changes

### Phase 1: Extend `ControllerConfig` with Optional Workflow Fields

**File:** `latte-agent-core/src/controller.rs`

After `no_ask_human`, add:
```rust
// ─── Workflow mode (multi-role discussion orchestration) ─────
/// Optional workflow definition. When `Some`, the multi-role loop
/// executes workflow steps instead of flat round-robin.
pub workflow: Option<Arc<latte_agent_orchestrator::workflow::DiscussionWorkflow>>,
/// Consensus method for workflow mode.
pub consensus: Option<latte_agent_orchestrator::consensus::ConsensusMethod>,
/// Template variables for prompt interpolation.
pub workflow_variables: HashMap<String, String>,
```

The `DiscussionWorkflow` carries: `steps: Vec<WorkflowStep>`, `max_rounds`, `context_token_budget`. The controller doesn't need `ConsensusMethod` for scheduling — that's an orchestrator concern. But putting it here lets the workflow loop emit `RoundEnded` with consensus info.

### Phase 2: Route `run_driver` to New Mode

**File:** `latte-agent-core/src/controller.rs`

In `run_driver`, after the existing `is_multi` check, add:
```rust
let is_workflow = config.workflow.is_some();
if is_workflow {
    run_workflow_loop(config, input_rx, event_tx, cancel_flag, pause_flag).await;
} else if is_multi {
    run_multi_role_loop(config, input_rx, event_tx, cancel_flag, pause_flag).await;
} else {
    run_single_role_loop(config, input_rx, event_tx, cancel_flag).await;
}
```

### Phase 3: Implement `run_workflow_loop`

**File:** `latte-agent-core/src/controller.rs`

New async function, following the same pattern as `run_multi_role_loop` but with workflow-step scheduling. Key structure:

```rust
async fn run_workflow_loop(
    config: ControllerConfig,
    input_rx: &mut mpsc::UnboundedReceiver<ControllerInput>,
    event_tx: &broadcast::Sender<ChatEvent>,
    cancel_flag: &AtomicBool,
    pause_flag: &AtomicBool,
) {
    let workflow = config.workflow.as_ref().expect("workflow required");
    let consensus = config.consensus.as_ref();
    let session_arc: Arc<Mutex<SessionManager>> = /* build similar to run_multi_role_loop */;

    // Build agent runners for all unique speakers across all steps
    let mut runners: HashMap<String, AgentRunner> = build_runners_for_speakers(&config, &workflow, session_arc.clone()).await;

    for round_num in 1..=config.max_rounds {
        if cancel_flag.load(Ordering::SeqCst) { break; }

        let _ = event_tx.send(ChatEvent::RoundStarted { round: round_num });
        let mut round_records: Vec<TurnRecord> = Vec::new();
        let mut step_transcripts: HashMap<String, String> = HashMap::new();

        // --- Execute workflow steps ---
        for step in &workflow.steps {
            // Pre-step hooks (Contribution/Gate)
            for hook in &step.hooks {
                match hook.kind {
                    HookKind::Contribution => { /* inject content similar to orchestrator::run */ },
                    HookKind::Gate => { /* gate handling */ },
                }
            }

            // Step speaker loop
            for speaker in &step.speakers {
                if cancel_flag.load(Ordering::SeqCst) { break; }

                // Check pause/supervisor
                if pause_flag.load(Ordering::SeqCst) {
                    // pause-wait loop identical to run_multi_role_loop pattern
                    wait_for_resume(input_rx, session_arc.clone(), cancel_flag, pause_flag, event_tx).await;
                }

                let runner = runners.get_mut(speaker).expect("speaker registered");
                let prompt = workflow.build_step_prompt(step, &config.workflow_variables);
                let context_prompt = build_step_context(prompt, step_transcripts.get(&step.id));

                let system_vars = serde_json::json!({...});
                let response = runner.run_turn(&[
                    Message { role: Role::User, content: context_prompt }
                ], Some(&system_vars)).await.map_err(|e| { ... }).unwrap_or_default();

                // Record the turn
                let record = TurnRecord { ... };
                round_records.push(record.clone());
                step_transcripts.entry(step.id.clone()).or_default().push_str(&format!("[{}]: {}\n", speaker, response));

                // Emit RoleTurn event
                let _ = event_tx.send(ChatEvent::RoleTurn { role_id: speaker.clone(), content: response.clone(), is_complete: true });

                // Supervisor check
                // ...
            }

            // Post-step output_key
            if let Some(ref key) = step.output_key {
                if let Some(last) = step.speakers.last() {
                    config.workflow_variables.insert(key.clone(), step_transcripts.get(&step.id).cloned().unwrap_or_default());
                }
            }
        }

        // --- Consensus check ---
        let consensus_reached = if let Some(ref method) = consensus {
            if method.requires_vote() {
                let votes = collect_votes_for(&round_records, &runners);
                method.evaluate(&votes).consensus
            } else { true }
        } else { true };

        let _ = event_tx.send(ChatEvent::RoundEnded { round: round_num });
        // Persist round
        // ...

        if consensus_reached { break; }
    }

    let _ = event_tx.send(ChatEvent::Done);
}
```

**Critical details to preserve from orchestrator::run:**
1. **Step context stitching** — each speaker in a step gets the accumulated transcript of prior speakers in the same step. This is the `step_transcript` accumulator in orchestrator.rs lines 129-194.
2. **`build_step_prompt` with `{{step_id}}`, `{{speaker}}`, `{{topic}}` substitution** — orchestrator.rs lines 140-146.
3. **Pre-step hook: `HookKind::Contribution`** — injects content from a source agent template (orchestrator.rs lines 102-126).
4. **Post-step output_key** — stores step output as variable for later steps (orchestrator.rs lines 197-199).
5. **Consensus after each round** — checks last turn per role (orchestrator.rs lines 210-224; `collect_votes` function lines 377-399).

**New capabilities gained from ChatController infra:**
1. **Pause/resume** — `pause_flag`, `ChatEvent::Paused`/`Resumed`, pause-wait loop.
2. **Cancel** — `cancel_flag` checked throughout.
3. **SessionManager** — persistence, state tracking, round/turn bookkeeping.
4. **Supervisor** — token budget monitoring, auto-pause.
5. **AskHuman** — tool registration, `ControllerInput::AskHumanReply` handling.
6. **Event broadcast** — existing `ChatEvent` types used throughout.

### Phase 4: Rewrite `DiscussionOrchestrator::run` as Thin Wrapper

**File:** `latte-agent-orchestrator/src/orchestrator.rs`

```rust
use latte_agent_core::controller::{ChatController, ControllerConfig};

impl DiscussionOrchestrator {
    pub async fn run(&mut self) -> OrchResult<DiscussionResult> {
        let controller = ChatController::new(256);
        let config = self.build_controller_config();
        let mut events = controller.spawn(config).await;
        // Spawn input task: when controller awaits input, we have none
        // (workflow mode is autonomous, not interactive)
        // Submit a dummy to kick-start
        controller.submit_input("__autonomous__").await;

        let mut rounds = Vec::new();
        let mut total_usage = TokenUsage::default();
        let mut consensus_reached = false;

        // Collect events until Done
        use tokio::sync::broadcast;
        loop {
            match events.recv().await {
                Ok(ChatEvent::RoleTurn { role_id, content, .. }) => {
                    // Record turn — but we need round/step context.
                    // Either: (a) enrich ChatEvent with workflow context, or
                    // (b) emit a richer event from the workflow loop.
                    //
                    // RECOMMENDATION: (b) Add ChatEvent::WorkflowTurn or extend
                    // ChatEvent::RoleTurn with round, step_id fields.
                }
                Ok(ChatEvent::RoundStarted { round }) => {
                    rounds.push(Round::new(round as usize));
                }
                Ok(ChatEvent::RoundEnded { round }) => {
                    // Check consensus from round context (need richer event)
                }
                Ok(ChatEvent::Done) => break,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("controller events lagged by {n}");
                }
                Err(broadcast::error::RecvError::Closed) => break,
                _ => {}
            }
        }

        Ok(DiscussionResult { rounds, consensus_reached, votes: None, summary: None, total_usage })
    }
}
```

**The approach has two sub-options:**

**Option A (recommended): Orchestrator `new()` takes `ControllerConfig`-ready agents + workflow data. `run()` calls `ChatController::spawn()` with the config. The workflow loop inside `run_workflow_loop` handles all step scheduling. Orchestrator just reads events.**

**Option B (simpler but less separation): Orchestrator delegates agent-runner building to `ChatController` entirely. `DiscussionConfig` maps one-to-one to `ControllerConfig + workflow + consensus`.** DiscussionOrchestrator becomes primarily a coordinator that translates `DiscussionResult` from the event stream.

**Recommended: Option B.** The orchestrator builds runners (as it already does in `new()`), then constructs a `ControllerConfig` and spawns. The controller's `run_workflow_loop` uses those runners.

### Phase 5: Consensus Integration into Event Loop

Consensus naturally fits as a post-round check inside `run_workflow_loop`. After all steps for a round complete:

1. Collect the last turn per role from `round_records`.
2. Call `consensus_method.evaluate(&votes)`.
3. If consensus reached, emit a `ChatEvent::ConsensusReached` (new event variant) and break round loop.

**New event variant:**
```rust
ChatEvent::ConsensusReached {
    agreement: f64,
    winner: Option<String>,
}
```

This is a **light touch** — one new event variant and a few lines in the workflow loop. No architectural change.

### Phase 6: `context_token_budget` Handling

`DiscussionWorkflow.context_token_budget` and `DiscussionConfig.context_token_budget` already exist (both default 0 = unlimited). This maps naturally to `ControllerConfig.session_token_budget` which is already `u32` with `0 = disabled`. The budget concept is identical.

**Change:** In the thin `DiscussionOrchestrator::run`, either:
- Pass `workflow.context_token_budget` as `ControllerConfig.session_token_budget`, or
- Keep `DiscussionConfig.context_token_budget` for the override (it was only used to initialize `ConversationContext` which is dead code — `#[allow(dead_code)]`).

**Recommendation:** Merge `context_token_budget` into `ControllerConfig.session_token_budget` in the config construction. Drop the `ConversationContext` field from `DiscussionOrchestrator` (it's dead code).

### Phase 7: No Conflict with `run_hil_repl` Replacement

The `run_hil_repl` replacement work replaced a stdin-based REPL with the `ChatController` channel-based driver. That is:
- `run_hil_repl` (old) → `ChatController::spawn` + `run_multi_role_loop` (new, already done).

This unification adds **`run_workflow_loop`** as a second mode alongside `run_multi_role_loop`. There is **no overlap or conflict** because:
- `run_hil_repl` was the CLI chat pump — replaced by `ChatController` + input channel.
- `DiscussionOrchestrator` is the programmatic orchestrator used by `discuss` and `workflow` CLI commands.
- This plan changes `DiscussionOrchestrator::run` to use `ChatController::spawn` — the callers (`discuss.rs`, `workflow.rs`) don't change.
- The `run_workflow_loop` function parallels `run_multi_role_loop` structurally (same pause/resume/event patterns) but schedules differently (workflow steps instead of flat order).

### Phase 8: Required New Event Variants

`ChatEvent` needs:
1. `RoundStarted { round, step_id }` — to carry which step is being entered.
2. `RoundEnded { round, consensus_reached, agreement }` — consensus data.
3. `ConsensusReached { agreement, winner }` (optional, could merge into RoundEnded).

These are additive — existing consumers ignore unknown variants.

## Task Flow

### Step 1: Add workflow fields to `ControllerConfig`
- **Files:** `latte-agent-core/src/controller.rs`
- **Changes:** Add `workflow`, `consensus`, `workflow_variables` fields to `ControllerConfig`.
- **Acceptance:** `ControllerConfig` compiles with the new fields, existing callers unaffected (default to `None`/`HashMap::new()`).

### Step 2: Implement `run_workflow_loop`
- **Files:** `latte-agent-core/src/controller.rs`
- **Changes:** New async function alongside `run_multi_role_loop`. Implements nested round→step→speakers scheduling. Uses same input channel (with auto-kickstart since workflow is autonomous), pause/resume/cancel flags, event broadcast.
- **Acceptance:** Workflow cycle runs with correct step-order scheduling, pre-step hooks, step context accumulation, output_key storage, consensus check, pause/supervisor integration.

### Step 3: Route `run_driver` to workflow mode
- **Files:** `latte-agent-core/src/controller.rs`
- **Changes:** Add `is_workflow` check in `run_driver`, dispatch to `run_workflow_loop`.
- **Acceptance:** Setting `config.workflow = Some(...)` enters the new loop.

### Step 4: Rewrite `DiscussionOrchestrator::run` as thin wrapper
- **Files:** `latte-agent-orchestrator/src/orchestrator.rs`
- **Changes:** `run()` builds `ControllerConfig` from `DiscussionConfig` + agents, calls `ChatController::spawn`, reads events, produces `DiscussionResult`. `context` field removed (dead code). `run_with_events` folded into the event-based approach.
- **Acceptance:** Callers (`discuss.rs`, `workflow.rs`) get same-or-better behavior with pause/resume/ask_human available.

### Step 5: Add consensus event variants + consensus in loop
- **Files:** `latte-agent-core/src/controller.rs`, `latte-agent-orchestrator/src/consensus.rs`
- **Changes:** Add `ChatEvent::ConsensusReached` variant. Call `consensus.evaluate()` after each round in `run_workflow_loop`.
- **Acceptance:** Workflow loop breaks on consensus; event carries agreement/winner data.

### Step 6: Update callers and remove dead code
- **Files:** `latte-agent-cli/src/commands/discuss.rs`, `latte-agent-cli/src/commands/workflow.rs`, `latte-agent-orchestrator/src/orchestrator.rs`
- **Changes:** No caller changes needed — they construct `DiscussionConfig` and call `orchestrator.run()` same as before. Internally `DiscussionOrchestrator` now uses `ChatController`. Remove `ConversationContext` dead field.
- **Acceptance:** `cargo build --workspace` compiles clean. `cargo test` passes.

## ADR

**Decision:** Add `run_workflow_loop` as a new scheduling mode in `ChatController`, not as a modification to `run_multi_role_loop`.

**Drivers:**
1. Two scheduling patterns share pause/resume infra but differ in loop structure — mixing them produces a complex conditionals soup.
2. Workflow scheduling is fixed/autonomous (no human input per round), unlike HIL chat which waits for user at each round.
3. Minimal risk to existing HIL chat code path.

**Alternatives considered:**
1. **Modify `run_multi_role_loop` to accept optional workflow steps** — rejected because the round input-wait semantics differ fundamentally (workflow mode auto-submits, HIL waits for user).
2. **Make orchestrator own all scheduling, bypassing ChatController entirely** — rejected because it duplicates pause/resume/SessionManager/Supervisor infra.
3. **Make `WorkflowStep` implement a trait that `run_multi_role_loop` calls** — over-engineered; the patterns are divergent enough that a separate loop is simpler.

**Consequences:**
- New code path (`run_workflow_loop`) needs separate testing.
- `DiscussionOrchestrator` retains its public API — callers unaffected.
- Future: could extract shared infra (pause-wait loop, supervisor check, SessionManager bootstrap) between the two multi-role modes.

**Follow-ups:**
- Add `ChatEvent::RoundStarted` enrichment with `step_id`.
- Add e2e test: `discuss` command with workflow → uses `run_workflow_loop`.
- Consider whether to expose `run_workflow_loop` publicly or keep it private to `ChatController`.