<instruction>
- Use to submit a structured task list to the user for approval before execution.
- Include: title, description (goal + acceptance criteria), priority, labels, paths, dependencies, subtasks.
- Submit the FULL plan in one call — don't submit tasks one by one.
- Only submit after sufficient exploration/analysis is complete.
</instruction>

<critical>
- NEVER submit a plan before understanding the codebase well enough to estimate scope and dependencies.
- NEVER submit tasks with specific commands or tool instructions — describe WHAT and WHY, not HOW.
- NEVER proceed with implementation before the user approves the plan (unless explicitly told to skip planning).
- Task paths should not overlap — parallel execution requires non-overlapping file scopes.
</critical>
