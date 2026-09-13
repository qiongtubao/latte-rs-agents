<instruction>
- Use when the user must make a decision between distinct alternatives that you cannot resolve from context alone.
- Provide structured choices with clear descriptions of tradeoffs for each option.
- Include a recommended default when one option is clearly safer or more conventional.
- Use for: architecture decisions, scope tradeoffs, ambiguous requirements, style preferences.
- **Quiz / re-test questions**: the three flags `allow_repeat` / `shuffle` / `auto_answer` are
  **NOT implemented** (the handler never parses them — they are silently ignored). Do not rely on them:
  - Re-asking the *same question text* is silently replayed from the earlier answer (AnswerLog
    protects "a user's answer is not renewable" across step retries / resume) and no dialog is
    shown. To re-test, make the question text differ (e.g. append a round marker like `［第2次作答］`).
  - No server-side shuffling exists: place the correct option at a random position yourself.
    Grading must match by label text, never by position.
  - Blocking ask waits indefinitely — there is no timeout auto-answer to disable.
</instruction>

<critical>
- NEVER ask when you can determine the answer from project conventions, existing code patterns, or standard defaults.
- NEVER ask open-ended questions — always provide concrete options to choose from.
- NEVER use `ask` as a stalling tactic when you should be doing exploration work first.
- **NEVER offer a punt-back option** — an option that carries no next action ("先不锁方向", "再看看", "以后再定",
  "let's decide later", "keep it flexible"). Whoever picks it hands the decision straight back to you, and you are
  left with nothing to converge on — the observed failure mode is going back to another round of exploration and
  never producing the deliverable the user asked for. Every option must name a concrete direction you can act on
  immediately. If "don't commit yet" genuinely is a valid answer, spell out its action:
  "按阶段拆任务，改动点留作最后一个任务" — not "先不锁方向".
- Batch related questions into a single `ask` call rather than asking one at a time.
- NEVER set `recommended` on a quiz option — it renders as a literal "✓ 推荐" badge next to the label (i.e. the answer key, printed on the question).
</critical>
