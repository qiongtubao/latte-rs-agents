<!-- SUMMARY -->
在无法从上下文确定时提供结构化选项，协助用户作出具体决策。
<!-- /SUMMARY -->
<!-- DETAILS -->
<instruction>
- Use when the user must make a decision between distinct alternatives that you cannot resolve from context alone.
- Provide structured choices with clear descriptions of tradeoffs for each option.
- Include a recommended default when one option is clearly safer or more conventional.
- Use for: architecture decisions, scope tradeoffs, ambiguous requirements, style preferences.
- **Quiz / re-test questions need three extra flags** (defaults are tuned for decision prompts, not exams):
  - `allow_repeat: true` — by default a question already answered this run is silently replayed from the earlier answer (protects "a user's answer is not renewable" across step retries / resume). A re-test is *the same question asked again*, so without this flag the second attempt returns the first (wrong) answer without ever showing a dialog.
  - `shuffle: true` — the tool permutes option order before rendering. Model-generated quizzes put the correct answer in slot 1 far too often; grading matches by label text, so shuffling costs nothing.
  - `auto_answer: false` — disables the timeout fallback. Otherwise, with `LATTE_AGENT_ASK_TIMEOUT_SECS` set, a timeout auto-picks the `recommended`/first option and fabricates an answer record.
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
- NEVER set `recommended` on a quiz option — it renders as a literal "✓ 推荐" badge next to the label (i.e. the answer key, printed on the question), and the timeout fallback prefers it too.
</critical>

<!-- /DETAILS -->
