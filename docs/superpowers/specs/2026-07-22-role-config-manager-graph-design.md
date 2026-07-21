# Role Configuration, Model Chain, and Manager Graph Access

## Problem
Role editor saves are persisted, but active sessions intentionally snapshot configuration at creation time, which makes edits appear ineffective. The model chain is rendered as a comma-separated text field, and the manager role is unable to directly inspect code or the role graph because its prompt and allowlist restrict it to delegation/workflow.

## Decision
- Preserve session snapshot semantics; do not silently destroy active conversation context.
- Make the UI state explicit: saved configuration applies to new sessions.
- Replace the model-chain text input with ordered rows supporting add, remove, and move operations while preserving the existing `string[]` API.
- Grant manager direct `read`, `search`, and `list` tools in addition to `delegate` and `workflow`.
- Update manager instructions so it may inspect files, search code, and use graph information directly while retaining delegation.
- Keep specialist permissions role-local; delegation does not overwrite the specialist allowlist.

## Acceptance
1. Saving a role returns and displays the persisted values, including ordered model chain and tools.
2. The UI makes new-session-only behavior explicit.
3. Model chain can be edited row-by-row and submitted in priority order.
4. Manager configuration and prompt no longer prohibit direct inspection/search.
5. Role graph endpoint remains available and manager has the tools needed to investigate it.
6. Frontend build and targeted Rust tests pass.
