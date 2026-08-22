# Lessons

## Workflow configuration scope

- `task_refine` runtime resolution checks `<project>/.latte/workflows.d/<name>.toml` before `$LATTE_HOME/workflows.d/<name>.toml`; changing the global file alone does not affect a project that has a same-named local override.
- When auditing a session, verify both the intended configuration scope and the loader precedence before attributing stale behavior to a failed code fix.
- Global workflow changes belong in `$LATTE_HOME/workflows.d/`; do not write them into an unrelated project repository unless the user explicitly requests changing project-local precedence.
