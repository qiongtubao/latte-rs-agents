<instruction>
- Use to assign subtasks to specialist roles (programmer, architect, reviewer, tester, etc.).
- Provide clear task description with: goal, scope, acceptance criteria. Do NOT include specific commands or tool usage instructions.
- Batch independent tasks in parallel — don't serialize what can run concurrently.
- After specialists return, synthesize results and check for contradictions before answering the user.
- **复用已有产物，别让 specialist 从零重读**：若已有相关产出（如 `lab/notes/*.md`、之前步骤的报告），在任务描述里**指名点出这些文件路径 + 一句话摘要**，并把范围收敛为"读增量 / 只更新差异"，而不是让 specialist 把整批既有笔记再全量读一遍。这是最大的一处 token 浪费来源。
</instruction>

<critical>
- NEVER specify which tools the specialist should use or in what order — they choose their own approach.
- NEVER delegate trivial tasks (< 10 lines of change) that you can do directly with `read`/`write`/`bash`.
- Each delegated task must be independently verifiable — don't delegate steps that depend on your unreturned context.
- Wait for ALL specialists to return before giving final answers to the user.
</critical>
