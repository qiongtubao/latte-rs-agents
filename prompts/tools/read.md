<instruction>
- For reading specific lines you already know the location of (e.g. after `code_graph` gave you a line number), use `offset` + `limit` to read only the relevant section.
- For config files, READMEs, and non-code text: `read` is the right choice.
- For code exploration (finding functions, structs, understanding module structure): use `code_graph` first — it returns signatures+line numbers at 20x less token cost.
</instruction>

<critical>
- NEVER read an entire large source file (>200 lines) just to find a function definition or line number. Use `code_graph(kind=function, path=file)` instead.
- When you need to read code, prefer `offset` + `limit` (e.g. `offset=120, limit=30`) over reading the whole file.
- If you find yourself reading multiple files sequentially to understand a module, step back and use `code_graph(kind=function, path=directory)` to get an overview first.
</critical>
