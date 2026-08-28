<instruction>
- 读**代码**文件（.c/.h/.cpp/.rs/.go/.py/.ts/.js/.java）且不带行范围时，`read` **默认回传结构摘要**：只列顶层定义的签名 + 行号，函数体折叠成 `… N ln elided (start-end)`。（代码走 code-graph）
- 读**文档**（.md/.markdown/.mdx/.txt）且不带行范围时，`read` **默认回传大纲**：只列各级标题 + 行号，正文折叠。（文档走 doc-graph 式大纲）
- 两种情况你都**不需要**特意换别的工具——直接 `read` 就自动拿到「地图/目录」，省 token。
- 要看某段**完整内容**：按摘要/大纲里的行号重读，例如 `read path:120-160`（footer 有现成示例）。
- 要**整文件原文**（含折叠部分）：`read path:raw`。
- 已知确切行号：`read path:start-end` / `path:start+count`。
</instruction>

<critical>
- footer 会列出被折叠的行范围——要看细节就**只重读需要的那几段**，不要一上来 `:raw` 拉整个大文件/长文档。
- 摘要/大纲只对够大的代码/带标题文档生效；解析失败、文件小、或散文无标题会自动回退整文件，无需担心。
- 要精确匹配某个符号名/调用点，用 `code_graph`（带 `name` 过滤）比整文件摘要更聚焦。
</critical>

