<!-- SUMMARY -->
创建或修改文件，编辑前读取并在写入后验证目标内容。
<!-- /SUMMARY -->
<!-- DETAILS -->
<instruction>
- Use for: creating new files, modifying existing files (str_replace, insert, append).
- For large refactors: make targeted edits with `str_replace` rather than rewriting entire files.
- Always read the target file first (or use `code_graph` to understand its structure) before making edits.
- Verify edits by reading the modified section after writing.
</instruction>

<critical>
- NEVER write an entire file when you only need to change a few lines — use `str_replace` with the minimal old/new string.
- Before editing, confirm the exact text to replace exists in the file. Stale context after multiple edits causes mismatches.
- When creating new code files, match the project's existing style (indentation, naming conventions, import patterns).
</critical>

<!-- /DETAILS -->
