<!-- SUMMARY -->
按主题或问题查询文档图，返回语义相关的文档上下文。
<!-- /SUMMARY -->
<!-- DETAILS -->
<instruction>
- Use to query the document graph for relevant context based on a topic or question.
- Returns semantically related documentation chunks from the indexed doc graph.
- Useful for: understanding project conventions, finding related docs, gathering context before decisions.
</instruction>

<critical>
- NEVER use as a substitute for reading actual source code — this is for documentation only.
- If the graph returns no results, the docs may not be indexed yet — try `doc_graph_scan` first.
</critical>

<!-- /DETAILS -->
