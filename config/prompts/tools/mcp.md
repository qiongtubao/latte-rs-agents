<!-- SUMMARY -->
连接 MCP 服务、发现可用工具并调用外部 MCP 端点。
<!-- /SUMMARY -->
<!-- DETAILS -->
<instruction>
- Use for: connecting to MCP servers, listing available MCP tools, calling MCP tool endpoints.
- First `mcp_connect` to establish connection, then `mcp_list` to discover available tools, then `mcp_call` to invoke.
- Treat MCP tools as external services — they may be slow or unavailable.
</instruction>

<critical>
- NEVER call MCP tools without first listing available tools — the server may not support what you expect.
- NEVER assume MCP server availability — handle connection failures gracefully.
- Prefer built-in tools over MCP equivalents when both exist — built-in tools are faster and more reliable.
</critical>

<!-- /DETAILS -->
