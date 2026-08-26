<instruction>
- Use when the user must make a decision between distinct alternatives that you cannot resolve from context alone.
- Provide structured choices with clear descriptions of tradeoffs for each option.
- Include a recommended default when one option is clearly safer or more conventional.
- Use for: architecture decisions, scope tradeoffs, ambiguous requirements, style preferences.
</instruction>

<critical>
- NEVER ask when you can determine the answer from project conventions, existing code patterns, or standard defaults.
- NEVER ask open-ended questions — always provide concrete options to choose from.
- NEVER use `ask` as a stalling tactic when you should be doing exploration work first.
- Batch related questions into a single `ask` call rather than asking one at a time.
</critical>
