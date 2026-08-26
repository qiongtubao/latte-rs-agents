<instruction>
- Use to launch multi-step structured processes: implementation_plan, tdd_development, code_review, explore, etc.
- Choose the workflow that best matches the task complexity. Simple tasks don't need workflows.
- Workflows run autonomously through their defined steps — you monitor progress and handle failures.
</instruction>

<critical>
- NEVER start a workflow for trivial tasks that can be done directly or with a single delegate.
- NEVER start multiple workflows for the same goal — pick one and let it run.
- If a workflow fails, diagnose the root cause before restarting or falling back to manual dispatch.
- Disclose workflow failures to the user — never silently degrade to manual execution.
</critical>
