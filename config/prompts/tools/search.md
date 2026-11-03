<!-- SUMMARY -->
跨文件搜索文本模式、错误信息、配置值和字符串字面量。
<!-- /SUMMARY -->
<!-- DETAILS -->
<instruction>
- Best for: finding text patterns, error messages, config values, TODOs, string literals across files.
- For finding function/struct/type definitions: use `code_graph(kind=function/struct/type)` instead — it's AST-aware and won't match comments or strings.
- For finding who calls a function: use `code_graph(kind=call, name=funcName)` instead of `grep`-style regex.
- Use `search` when you need regex flexibility that `code_graph` can't provide (e.g. multi-word patterns, config values, log messages).
</instruction>

<critical>
- If your search pattern is a function/struct/macro name and you want its DEFINITION, use `code_graph` — `search` will return every mention (comments, strings, docs) while `code_graph` returns only the actual definition.
- After `search` finds a match, use `read(offset=line, limit=30)` to see context — don't `read` the whole file.
</critical>

<!-- /DETAILS -->
