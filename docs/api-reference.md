# API 参考文档

> latte-rs-agents Web UI 的 HTTP + SSE API。服务默认运行在 `http://localhost:4567`。

---

## 总览

所有 API 路径以 `/api/` 为前缀。身份认证：无（开发工具，仅绑定 localhost）。

- **REST 端点**：聊天命令、会话管理、角色查询、trace 查看
- **SSE 端点**：实时事件流（ChatEvent、SelfLoopEvent）
- **静态文件**：生产模式下 `/` 提供 `latte-agent-cli/ui/dist/` 静态资源

---

## 1. 健康检查

### `GET /health`

存活探针。返回 `"ok"`。

**响应：**
```
ok
```

---

## 2. 会话管理

### `GET /api/sessions`

列出当前所有活跃会话。

**响应 `200`：**
```json
[
  {
    "session_id": "abc123",
    "preview": "hello world",
    "initial_role": "manager",
    "created_at_unix_ms": 1747000000000,
    "last_activity_unix_ms": 1747000010000
  }
]
```

### `POST /api/sessions`

创建新会话。分配一个新的 `ChatController` 实例。

**请求体：** `{}`

**响应 `200`：**
```json
{
  "session_id": "def456",
  "role": "manager",
  "model": "deepseek-v4-flash",
  "tier": "premium",
  "available_sessions": [
    { "session_id": "abc123", "preview": "hello world", "initial_role": "manager", "created_at_unix_ms": 1747000000000, "last_activity_unix_ms": 1747000010000 }
  ],
  "available_roles": [
    { "id": "manager", "name": "Engineering Manager", "icon": "👔" }
  ]
}
```

### `GET /api/session`

获取指定会话的详情。

**查询参数：** `id`（会话 ID）

**响应 `200`：**
```json
{
  "session_id": "abc123",
  "role": "manager",
  "model": "deepseek-v4-flash",
  "tier": "premium",
  "available_sessions": [],
  "available_roles": []
}
```

**响应 `404`：** `"session not found"`

---

## 3. 角色查询

### `GET /api/roles`

列出所有可用的 Agent 角色。

**响应 `200`：**
```json
[
  { "id": "manager",     "name": "Engineering Manager",   "icon": "👔" },
  { "id": "programmer",  "name": "Software Engineer",      "icon": "💻" },
  { "id": "architect",   "name": "System Architect",       "icon": "🏗️" },
  { "id": "reviewer",    "name": "Code Reviewer",          "icon": "🔍" },
  { "id": "tester",      "name": "QA Engineer",            "icon": "🧪" },
  { "id": "security",    "name": "Security Auditor",       "icon": "🛡️" },
  { "id": "devops",      "name": "DevOps Engineer",        "icon": "⚙️" },
  { "id": "designer",    "name": "UI/UX Designer",          "icon": "🎨" },
  { "id": "tech_writer", "name": "Technical Writer",       "icon": "📝" },
  { "id": "pm",          "name": "Product Manager",        "icon": "📋" }
]
```

---

## 4. 聊天命令

### `POST /api/chat/send`

发送一条用户消息到当前会话。

**请求体：**
```json
{
  "session_id": "abc123",
  "message": "分析一下这个代码库的结构"
}
```

`session_id` 可选：省略时使用最近活跃的会话。

**响应 `202 Accepted`：** 消息已入队，结果通过 SSE 推送。

### `POST /api/chat/command`

发送一个斜杠命令。

**请求体：**
```json
{
  "session_id": "abc123",
  "command": "/clear"
}
```

支持的命令：
| 命令 | 效果 |
|---|---|
| `/role <id>` | 切换当前角色 |
| `/model <id>` | 切换模型 |
| `/clear` | 清除对话上下文 |
| `/save` | 保存当前会话 |
| `/tier <tier>` | 切换模型层级（premium/standard/budget） |

**响应 `202 Accepted`**

### `POST /api/chat/role`

切换当前会话的角色。

**请求体：**
```json
{
  "session_id": "abc123",
  "role_id": "programmer"
}
```

**响应 `202 Accepted`**

---

## 5. SSE 事件流

### `GET /api/events`

ChatController 的实时事件流。每个会话独立推送。

**查询参数：** `id`（会话 ID，必填）

**协议：** Server-Sent Events（`text/event-stream`）

**事件类型：**

#### `chat_event`

Payload 是一个 internally-tagged JSON（`type` 字段标明变体类型）：

```json
// 角色发言
{ "type": "RoleTurn", "role_id": "programmer", "content": "代码库结构如下...", "is_complete": true }

// 状态提示
{ "type": "Status", "message": "programmer 正在分析代码库..." }

// 正在输入提示
{ "type": "Prompt", "icon": "💻", "role_id": "programmer", "model_id": "deepseek-v4-flash" }

// 暂停/继续
{ "type": "Paused", "reason": "等待用户决策" }
{ "type": "Resumed" }

// 多角色轮次
{ "type": "RoundStarted", "round": 1 }
{ "type": "RoundEnded", "round": 1 }

// 角色生命周期
{ "type": "RoleStarted", "role_id": "programmer", "detail": "开始分析..." }
{ "type": "RoleFinished", "role_id": "programmer", "detail": "分析完成，耗时 12.3秒" }

// 工具调用
{ "type": "ToolUse", "role_id": "programmer", "tool_name": "read", "args": "src/main.rs" }
{ "type": "ToolResult", "role_id": "programmer", "tool_name": "read", "result": "fn main() { ... }" }
{ "type": "ToolError", "role_id": "programmer", "tool_name": "bash", "error": "command not found" }

// 代理委派
{ "type": "DelegateStarted", "from_role": "manager", "to_role": "programmer", "task": "实现用户登录模块", "sub_id": "sub_001" }
{ "type": "DelegateFinished", "from_role": "manager", "to_role": "programmer", "status": "ok", "summary": "登录模块已实现...", "sub_id": "sub_001" }

// 会话结束
{ "type": "Done" }

// 错误
{ "type": "Error", "message": "API key 未配置" }

// 角色列表 / 上下文清除
{ "type": "RoleList", "roles": [ { "id": "programmer", "name": "Software Engineer", "icon": "💻" } ] }
{ "type": "ContextCleared" }

// 会话信息
{ "type": "SessionInfo", "task_id": "fix-bug", "state": "Active", "turn": 5, "roles": [...] }
```

**Keepalive：** 每 15 秒发送一个注释行（`:`）保持连接。

**TypeScript 类型定义：**
```typescript
export type ChatEvent =
  | { type: "RoleTurn"; role_id: string; content: string; is_complete: boolean }
  | { type: "Status"; message: string }
  | { type: "Prompt"; icon: string; role_id: string; model_id: string }
  | { type: "Paused"; reason: string }
  | { type: "Resumed" }
  | { type: "RoundStarted"; round: number }
  | { type: "RoundEnded"; round: number }
  | { type: "RoleStarted"; role_id: string; detail: string }
  | { type: "RoleFinished"; role_id: string; detail: string }
  | { type: "Done" }
  | { type: "Error"; message: string }
  | { type: "RoleList"; roles: RoleInfo[] }
  | { type: "ContextCleared" }
  | { type: "SessionInfo"; task_id: string; state: string; turn: number; roles: RoleInfo[] }
  | { type: "ToolUse"; role_id: string; tool_name: string; args: string }
  | { type: "ToolError"; role_id: string; tool_name: string; error: string }
  | { type: "ToolResult"; role_id: string; tool_name: string; result: string }
  | { type: "DelegateStarted"; from_role: string; to_role: string; task: string; sub_id: string }
  | { type: "DelegateFinished"; from_role: string; to_role: string; status: string; summary: string; sub_id: string };

export interface RoleInfo {
  id: string;
  name: string;
  icon: string;
}
```

---

## 6. Trace（调试事件追踪）

### `GET /api/traces`

列出 `$LATTE_HOME/traces/` 目录下的所有 JSONL trace 文件。

**响应 `200`：**
```json
[
  {
    "session_id": "fix-bug-001",
    "path": "/home/user/.latte/traces/fix-bug-001.jsonl",
    "size_bytes": 45920,
    "modified_unix": 1747000010000
  }
]
```

### `GET /api/traces/:session_id`

读取指定会话的完整 trace 事件列表。

**路径参数：** `session_id`（trace 文件名，不含 `.jsonl`）

**响应 `200`：**
```json
{
  "session_id": "fix-bug-001",
  "events": [
    { "turn": 1, "role": "manager", "kind": "SessionStart", ... },
    { "turn": 1, "role": "manager", "kind": "ModelCall", ... },
    { "turn": 1, "role": "manager", "kind": "TurnEnd", ... }
  ]
}
```

### `GET /api/subsessions`

获取委派子会话的完整事件日志。

**查询参数：** `id`（子会话 ID，对应 `DelegateStarted.sub_id`）

**响应 `200`：**
```json
[
  { "type": "RoleTurn", "role_id": "programmer", ... },
  { "type": "ToolUse", "role_id": "programmer", ... },
  ...
]
```

---

## 7. Self-Loop（AI 自调试）

### `POST /api/self-loop/start`

启动 AI 自调试循环。后台 spawn 一个 `tsx self-loop/runner.ts` 进程。

**请求体：**
```json
{
  "task": "让聊天输入框支持自动调整高度",
  "max_iterations": 5
}
```

**响应 `200`：**
```json
{
  "started": true,
  "task": "让聊天输入框支持自动调整高度",
  "max_iterations": 5
}
```

**响应 `409`：** `"self-loop already running"`

### `GET /api/self-loop/events`

Self-loop 进度 SSE 流。

**事件类型：**

```
event: self_loop_event
data: {"kind":"iteration","iteration":1,"message":"第1轮：截图并分析布局","screenshot":"data:image/png;base64,...","timestamp_unix_ms":1747000010000}
```

```json
{ "kind": "iteration",    "iteration": 1, "message": "分析布局",     "screenshot": "base64...", "timestamp_unix_ms": 1747000010000 }
{ "kind": "edit",         "iteration": 1, "message": "修改 CSS",     "screenshot": "base64...", "timestamp_unix_ms": 1747000010000 }
{ "kind": "build",        "iteration": 1, "message": "构建项目",     "data": { "exit_code": 0 }, "timestamp_unix_ms": 1747000010000 }
{ "kind": "test",         "iteration": 1, "message": "跑测试",        "data": { "passed": 13, "failed": 0 }, "timestamp_unix_ms": 1747000010000 }
{ "kind": "complete",     "iteration": 2, "message": "修改完成",     "data": { "diff_path": "/tmp/diff.patch" }, "timestamp_unix_ms": 1747000010000 }
{ "kind": "error",        "iteration": 1, "message": "编译错误",     "data": { "error": "..." }, "timestamp_unix_ms": 1747000010000 }
```

**Keepalive：** 每 10 秒一个 `event: ping` / `data:`。

### `POST /api/self-loop/stop`

停止当前运行的 self-loop。

**请求体：** 无

**响应 `200`**

---

## 8. 角色-工具关系图

### `GET /api/role-graph`

获取角色 × 工具的代码关系图。结合 TOML 配置中的 tool 声明与 TreeSitter 扫描到的 `register_*_tool` 调用点。

**响应 `200`：**
```json
{
  "nodes": [
    { "id": "manager", "kind": "Role", "label": "Engineering Manager" },
    { "id": "delegate", "kind": "Tool", "label": "delegate" },
    { "id": "read", "kind": "Tool", "label": "read" },
    { "id": "register_delegate_tool:42", "kind": "ToolRegistration", "label": "register_delegate_tool", "detail": "latte-agent-core/src/controller.rs:42" }
  ],
  "edges": [
    { "source": "manager", "target": "delegate", "kind": "USES_TOOL" },
    { "source": "register_delegate_tool:42", "target": "delegate", "kind": "REGISTERED_BY" }
  ],
  "stats": { "roles": 14, "tools": 5, "registrations": 8 },
  "project_root": "/home/user/project"
}
```

---

## 9. 会话 ID 管理

每个浏览器标签页独立维护一个 `session_id`：

- 首次加载时通过 `POST /api/sessions` 创建
- 存储在 `localStorage` 的 `latte-agent-ui-session-id` 键下
- 页面刷新时自动重用：调用 `GET /api/session?id=<stored>` 确认服务端仍存在
- "New Session" 按钮清理本地存储并创建新会话

**JavaScript 使用示例：**

```typescript
// 创建/恢复会话
const sessionId = await ensureSession();

// 发送消息
await fetch("/api/chat/send", {
  method: "POST",
  headers: { "Content-Type": "application/json" },
  body: JSON.stringify({ message: "你好" }),
});

// 订阅 SSE 事件流
const es = new EventSource(`/api/events?id=${sessionId}`);
es.addEventListener("chat_event", (e) => {
  const event = JSON.parse(e.data);
  console.log(event.type, event);
});
```
