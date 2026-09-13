Manage the session todo list (phases + tasks, atomic batch ops) to track multi-step work.

<instruction>
- Use when a task needs 3+ distinct steps, or the user gives a multi-step plan / numbered list.
- When the user gives an explicit list: build EVERY item into a task — never compress or merge items.
- Don't use for trivial single-step tasks.
- Don't let a todo call monopolize a turn: send `init` in the same batch as your first substantive tool call; send `done` in the same batch as the next action or final verification.
- Mark a task `done` immediately when finished — don't batch completions for later; mark `in_progress` when starting a new task (at most one).
- Ops: init {list:[{phase, items}]} rebuilds the list; start/done/rm/drop {task}; append {phase, items}; view {} reads current state. Combine related changes into one `ops` array.
</instruction>

<critical>
- NEVER finish your turn while todo items are still pending/in_progress — keep working or explain why you're blocked.
- NEVER mark a task done without having actually completed and verified it.
- Todo state is kept by the session — you only send `ops`; do not invent or resend a `currentPhases` field.
</critical>
