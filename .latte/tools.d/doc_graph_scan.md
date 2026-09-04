<!-- SUMMARY -->
扫描并索引项目文档，支持在文档变更后刷新文档图。
<!-- /SUMMARY -->
<!-- DETAILS -->
<instruction>
- Use to scan and index project documentation into the document graph.
- Run after significant doc changes to keep the graph up-to-date.
- Pairs with `doc_graph_context` for querying and `doc_write`/`doc_index` for maintenance.
</instruction>

<critical>
- NEVER scan the entire repo repeatedly — scan only changed or new documentation paths.
- Use before `doc_graph_context` queries to ensure the index is fresh.
</critical>

<!-- /DETAILS -->
