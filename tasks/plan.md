# Delegate and workflow execution unification

Goal: Make workflow and ordinary delegate share one dispatch/return UI projection; workflow differs only by fixed role selection and orchestration metadata.

Scope:
- Keep `DelegateStarted/Finished` as the shared dispatch/return events; ordinary delegation omits `wf_id`, workflow delegation carries `wf_id`.
- Workflow emits the same delegate lifecycle with `wf_id`; the fixed role remains selected by the workflow step configuration.
- UI renders one standard delegate dispatch card and one standard subagent return; `WorkflowStep/WorkflowTurn` remain orchestration metadata and do not duplicate chat bubbles.
- Keep `sub_id` as the sole subsession detail key.

Verification:
- Rust core event serialization/workflow tests.
- UI execution-row tests for ordinary delegate and workflow fixed-role events.
- Full UI tests and build.
