<instruction>
- Use for: running build commands, tests, git operations, installing dependencies, process management.
- Prefer dedicated tools when available: `read` for file content, `search` for pattern matching, `write` for file creation/editing, `code_graph` for code structure.
- Always use `set -e` or check exit codes for multi-step commands.
- For long-running commands, consider timeouts or background execution.
- Combine related commands in one call to reduce round-trips.
</instruction>

<critical>
- NEVER use `cat file` to read files — use the `read` tool instead.
- NEVER use `grep` for code exploration — use `code_graph` or `search` tool.
- NEVER use `sed`/`awk` for file editing — use the `write` tool.
- Capture both stdout and stderr when diagnosing failures.
- Avoid interactive commands (no vim, less, man with pager).
</critical>
