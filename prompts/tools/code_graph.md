<instruction>
- PREFER this tool over `read` and `search` for code exploration. It returns precise signatures+line numbers at minimal token cost.
- Use `kind` parameter (recommended): `function`, `struct`, `type`, `macro`, `call`, `import`, `decl`. Avoids pattern syntax errors.
- Use `name` to filter by substring — drastically reduces output when scanning directories.
- Default mode `signatures` returns one-line-per-match. Only use `mode=full` when you need the implementation body.
- For directories, pass `lang` explicitly (e.g. `lang=c` for `.c`/`.h` files).
- Workflow: `code_graph(kind=function, path=file)` → read signatures → pick targets → `read(path, offset=line, limit=30)` for details.
</instruction>

<critical>
- NEVER `read` an entire source file to find function names or line numbers — use `code_graph` first.
- AVOID scanning entire `src/` without `name` filter — use narrower `path` or add `name` to limit results.
- 0 matches usually means wrong `lang` or `kind` for the language — check the error message for available kinds.
- Token comparison: reading a 500-line .c file costs ~4000 tokens; `code_graph` signatures for the same file costs ~200 tokens (20x savings).
</critical>
