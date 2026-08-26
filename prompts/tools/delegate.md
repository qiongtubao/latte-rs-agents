<instruction>
- Use to assign subtasks to specialist roles (programmer, architect, reviewer, tester, etc.).
- Provide clear task description with: goal, scope, acceptance criteria. Do NOT include specific commands or tool usage instructions.
- Batch independent tasks in parallel — don't serialize what can run concurrently.
- After specialists return, synthesize results and check for contradictions before answering the user.
</instruction>

<critical>
- NEVER specify which tools the specialist should use or in what order — they choose their own approach.
- NEVER delegate trivial tasks (< 10 lines of change) that you can do directly with `read`/`write`/`bash`.
- Each delegated task must be independently verifiable — don't delegate steps that depend on your unreturned context.
- Wait for ALL specialists to return before giving final answers to the user.
</critical>
