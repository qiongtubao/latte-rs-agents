<!-- SUMMARY -->
以 AST 方式高效探索代码结构，查询函数、类型、调用和声明签名。
<!-- /SUMMARY -->
<!-- DETAILS -->
<instruction>
- PREFER this tool over `read` and `search` for code exploration. It returns precise signatures+line spans at minimal token cost.
- Use `kind` parameter (recommended): `function`, `struct`, `type`, `macro`, `call`, `import`, `decl`. Avoids pattern syntax errors.
- Use `name` to filter by substring — drastically reduces output when scanning directories.
- Default mode `signatures` returns one-line-per-match. Only use `mode=full` when you need the implementation body.
- `lang` is optional: inferred from the file extension, or from a directory's dominant language by content. Pass it explicitly only for mixed-language directories or when inference picks wrong (the result's `lang`/`lang_note` fields tell you what was used).
- Each match is `path:START-END: signature`. **`START-END` is the definition's full span — paste it straight into `read`.** Never guess how long a function is.
- Workflow: `code_graph(kind=function, path=file)` → pick targets → `read(paths=["file:3935-3979", "file:4009-4018"])` to batch-read those exact functions in one call.
</instruction>

<critical>
- NEVER `read` an entire source file to find function names or line numbers — use `code_graph` first.
- NEVER invent a line range for a function body. Use the `START-END` span code_graph gave you; a guessed range cuts the body mid-way and you will not notice.
- AVOID scanning entire `src/` without `name` filter — use narrower `path` or add `name` to limit results.
- 0 matches usually means wrong `lang` or `kind` for the language — check the error message for available kinds.
- Token comparison: reading a 500-line .c file costs ~4000 tokens; `code_graph` signatures for the same file costs ~200 tokens (20x savings).
</critical>
<!-- /DETAILS -->
