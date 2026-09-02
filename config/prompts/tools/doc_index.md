<instruction>
- Use to manually trigger re-indexing of specific documentation files in the doc graph.
- Useful after batch documentation updates or when `doc_graph_context` returns stale results.
</instruction>

<critical>
- NEVER index non-documentation files (source code, configs, binaries).
- Prefer `doc_graph_scan` for broad re-indexing; use `doc_index` only for targeted file updates.
</critical>
