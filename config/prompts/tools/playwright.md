<instruction>
- Use for: browser automation, e2e testing, screenshot capture, web page interaction.
- Write focused scripts that test one behavior per call.
- Use selectors that are resilient to minor UI changes (data-testid, role, text content).
- Take screenshots to verify visual state when needed.
</instruction>

<critical>
- NEVER write overly complex multi-page scripts in a single call — break into focused steps.
- NEVER assume page load is instant — use proper waitFor/expect patterns.
- NEVER hard-code timeouts without justification — prefer condition-based waiting.
</critical>
