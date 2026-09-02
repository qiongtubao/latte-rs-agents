# API 参考文档

> latte-rs-agents Web UI 的 HTTP + SSE API。服务默认运行在 `http://localhost:4567`。
>
> **维护说明**：本文档由 `tests/api_reference_integrity_test.rs` 校验：扫 `latte-agent-ui-server/src/lib.rs` 的所有 `route(...)` 调用，断言每个路径至少出现在本章 `## N. xxx` 章节标题中。增删端点后必须同步本文档。

---

## 端点索引（按域分组）

> 完整路径都在 `/api/` 前缀下（`/health` 在根路径）。

| 域 | 端点数 | 章节 |
|---|---|---|
| 健康检查 | 1 | §1 |
| 会话管理 | 6 | §2 |
| 角色查询 | 3 | §3 |
| 角色配置（编辑器） | 2 | §3.5 |
| 聊天命令 | 11 | §4 |
| SSE 事件流 | 1 | §5 |
| Trace / 调试 | 3 | §6 |
| Self-Loop | 3 | §7 |
| 角色-工具关系图 | 1 | §8 |
| 模型管理 | 5 | §9 |
| 工具管理 | 3 | §10 |
| 任务看板 | 9 | §11 |
| 工作流管理 | 8 | §12 |
| 文档图像上传 | 2 | §13 |

---

## 1. 健康检查

### `GET /health`

存活探针。返回 `"ok"`。

**响应 `200`：** `ok`

---

## 2. 会话管理

### `GET /api/sessions`

列出当前所有活跃会话。

**响应 `200`：** 数组，元素是 `SessionSummary`（`session_id` / `preview` / `initial_role` / `created_at_unix_ms` / `last_activity_unix_ms` / `is_paused` / `label`）。

### `POST /api/sessions`

创建新会话，分配一个 `ChatController` 实例。

**请求体：** `{ "session_id": "<可选，从已有恢复>" }`（任意内容均可）

**响应 `200`：** `SessionInfo` JSON（含 `session_id` / `role` / `model` / `tier` / `available_sessions` / `available_roles` / `is_paused`）。

### `DELETE /api/sessions?id=<sid>`

删除会话并停止其 controller。删除活跃会话前客户端应切换到另一个会话。

**响应 `200`**

### `POST /api/sessions/fork`

从源会话的事件前缀 fork 出新会话（按右键消息向上复制）。

**请求体：**
```json
{
  "source_session_id": "abc123",
  "events": [ ... 源会话前缀的 ChatEvent 数组（按时间顺序）... ]
}
```

**响应 `200`：** 新 `SessionInfo`。

### `GET /api/session?id=<sid>`

获取指定会话详情。

**响应 `200`：** `SessionInfo`。  
**响应 `404`：** `"session <sid> not found"`

### `GET /api/session/history?id=<sid>`

返回该会话的归档 ChatEvent 列表（用于切换 tab 后恢复聊天内容）。

**响应 `200`：** ChatEvent 数组（按时间顺序）。

### `POST /api/session/label`

重命名会话（空字符串清除自定义名，回退到 preview）。

**请求体：**
```json
{ "session_id": "abc123", "label": "重构 auth" }
```

**响应 `200`**

---

## 3. 角色查询

### `GET /api/roles`

返回所有可用角色（简版 `RoleInfo[]`，含 `id` / `name` / `icon`）。

**响应 `200`：** `RoleInfo[]`

### `POST /api/roles`

创建新角色（写 `.latte/agents.d/<id>.toml`）。

**请求体：** `RoleConfigEntry`（含 `id` / `name` / `model_tier` / `model_chain` / `temperature` / `tools` / `icon` 等）。

**响应 `200`：** `RoleConfigEntry`

### `DELETE /api/roles/:id`

删除角色（删 `.latte/agents.d/<id>.toml`）。

**响应 `200`**

---

## 3.5. 角色配置（编辑器）

> 与 §3「角色查询」分开：本节是编辑器的完整角色视图，含 `category` / `defaultModelTier` / `modelChain` / `prompt` / `code_paths` / `allowed_tools` 等。

### `GET /api/roles/config`

返回完整角色配置（含每个角色的系统 prompt、tools 列表等）。

**响应 `200`：** `RolesConfigResponse`（含 `roles: RoleConfigEntry[]`）。

### `POST /api/roles/config`

保存完整角色配置（编辑器批量保存场景）。

**请求体：** `RolesConfigResponse`

**响应 `200`**

### `GET /api/roles/:id/toml`

读取角色的 TOML 源文件（`.latte/agents.d/<id>.toml`）。

**响应 `200`：** `{ "toml": "..." }`

### `PUT /api/roles/:id/toml`

保存角色 TOML 源文件（保留原字段顺序）。

**响应 `200`**

### `POST /api/roles/test`

测试单角色（独立 ChatController 跑一轮）。

**请求体：** `{ "role_id": "...", "tier": "standard", "model_id": "..." }`

**响应 `200`**

---

## 4. 聊天命令

> 所有聊天端点都接收 `application/json`，body 含 `session_id`（可选，省略时使用最近活跃会话）。`POST /api/chat/{send,command,role}` 返回 `202 Accepted`（消息入队，结果通过 SSE 推送）；控制类（pause/resume/abort/cancel）返回 `200 OK` 或 `404 Not Found`。

### `POST /api/chat/send`

发送用户消息。

**请求体：** `{ "session_id": "abc123", "message": "..." }`

**响应 `202`**

### `POST /api/chat/choice-answer`

阻塞中的 ask（`ChoiceRequested.wait=true`，workflow/delegate 子代理正挂起等回答）的答案投递口。

**请求体：** `{ "choice_id": "choice-manager-3", "answer": "方案A" }`

两条投递路径：

1. **等待方还活着**（同进程）：答案经 core 的 choice 路由直接交给挂起的 `ask` 工具调用，子代理拿着答案继续干活。
2. **孤儿**（服务器重启过，等待方随进程消失）：从 `<cwd>/.latte/pending-asks/<choice_id>.json` 读回这次提问属于哪个 run，把答案补写成该 run checkpoint 里的一条 `Answer` 记录，然后触发断点续跑。续跑的 run 跳过已完成的 step，走到同一个 `ask` 时 `AnswerLog::recall` 命中、不再弹框，整个 workflow 继续。

**响应 `200`** —— 已送达（直达等待方，或答案已落 checkpoint + 续跑已拉起）
**`404`** —— 既没有等待方也没有可恢复的落盘记录（未知 id / checkpoint 已清）：前端应降级为 `POST /api/chat/send` 回喂
**`500`** —— 答案已落 checkpoint 但续跑起不来（workflow 定义被删等）。**前端不要**把答案当普通消息重发，它已经在 checkpoint 里了

### `GET /api/chat/pending-prompts`

本会话**仍未被用户处理**的弹框事件（阻塞 ask + 非阻塞 ask/plan + 跨进程孤儿 ask），以 SSE 同构的前端事件 JSON 数组返回。

弹框事件比普通事件脆弱：broadcast 是「没订阅者就丢弃」的，`Lagged` 掉的那几条既不进 SSE 也不进 archiver 的 `event_log`（连 `GET /api/session/history` 都没有），长会话里早期的还会被 `MAX_LOG` 挤出去。SSE 只在**新建连接**时补发挂起弹框，而 broadcast lag 时连接还活着；`clear() + replayEvents(history)` 又会把弹框卡片连未提交状态一起抹掉。所以前端每次全量重放之后都要显式拉这一份补齐。

数据来源两处：内存表（`choice::PENDING` / `choice::PROMPTS`，同 id 时内存优先）+ 盘上的 `<cwd>/.latte/pending-asks/`（服务器重启后内存全空，孤儿 ask 只在这里；它们**可答**，见上面的 `choice-answer`）。已答过的（问题在 checkpoint 里有 `Answer` 行）、checkpoint 已清的、超过 7 天的都不再返回。

**查询参数：** `id`（session_id，必填）

**响应 `200`**：`[{ "type": "ChoiceRequested", ... }, { "type": "PlanProposed", ... }]`

### `POST /api/chat/prompt-dismiss`

用户已处理某个**非阻塞**弹框（提交了选择 / 跳过）→ 从补发表销账，避免重连时弹出僵尸框。阻塞 ask 由 `choice-answer` 自动销账；plan 清单由 `POST /api/tasks/import` 带 `plan_id` 自动销账。

**请求体：** `{ "prompt_id": "choice-programmer-2" }`（ask 用 `choice_id`，plan 用 `plan_id`）

**响应 `200`**（幂等：未命中也回 200）

### `POST /api/chat/command`

发送斜杠命令（`/role <id>` / `/model <id>` / `/clear` / `/save` / `/tier <tier>` 等）。

**请求体：** `{ "session_id": "abc123", "command": "/clear" }`

**响应 `202`**

### `POST /api/chat/role`

切换当前会话的角色。

**请求体：** `{ "session_id": "abc123", "role_id": "programmer" }`

**响应 `202`**

### `POST /api/chat/cancel-turn`

取消当前 turn（不退出 session）。等当前 round 的 LLM 流返回后丢弃。

**请求体：** `{ "session_id": "abc123" }`

**响应 `200`**

### `POST /api/chat/abort`

终止整个 session（所有 in-flight subagent / workflow / multi-role 全部停止）。**同时连带取消**该 session 事件流上的 workflow resume（`session_workflows` 取消旗标）。

**请求体：** `{ "session_id": "abc123" }`

**响应 `200`**

### `POST /api/chat/pause`

暂停当前 turn（暂停门）：等用户在 tool 调后决策时用。

**请求体：** `{ "session_id": "abc123" }`

**响应 `200`**

### `POST /api/chat/resume`

恢复被 pause 的 turn。

**请求体：** `{ "session_id": "abc123" }`

**响应 `200`**

### `POST /api/chat/pause-session`

用户按 ⏸ 触发全 session 冻结（让所有 in-flight 与后续 turn 挂起）。

**请求体：** `{ "session_id": "abc123" }`

**响应 `200`**

### `POST /api/chat/resume-session`

恢复全 session 冻结。

**请求体：** `{ "session_id": "abc123" }`

**响应 `200`**

### `POST /api/chat/pause-role`

暂停单个角色（多角色 HIL）：该角色的回合被跳过，同一轮其他角色继续。
与 `resume-role` 配对。

**请求体：** `{ "session_id": "abc123", "role_id": "programmer" }`

**响应 `202`**

### `POST /api/chat/resume-role`

恢复单个角色（多角色 HIL）。与 `pause-role` 配对。

**请求体：** `{ "session_id": "abc123", "role_id": "programmer" }`

**响应 `202`**

### `POST /api/chat/stream-mode`

运行时切换流式/非流式输出模式。

**请求体：** `{ "session_id": "abc123", "stream": true }`

**响应 `200`**

---

## 5. SSE 事件流

### `GET /api/events?id=<sid>`

ChatController 实时事件流。每个会话独立推送。

**协议：** Server-Sent Events（`text/event-stream`）

**事件类型：** `chat_event`（payload 是 internally-tagged JSON，`type` 字段标明变体）。

**事件清单：**

| 变体 | 字段 | 说明 |
|---|---|---|
| `RoleTurn` | `role_id` / `content` / `is_complete` / `sub_id?` | 角色发言 |
| `Status` | `message` | 状态提示 |
| `Prompt` | `icon` / `role_id` / `model_id` | 正在输入提示 |
| `Paused` | `reason` | session 暂停 |
| `Resumed` | — | session 恢复 |
| `RoundStarted` | `round` | 多角色轮次开始 |
| `RoundEnded` | `round` | 多角色轮次结束 |
| `RoleStarted` | `role_id` / `detail` | 角色生命周期 |
| `RoleFinished` | `role_id` / `detail` | 角色生命周期 |
| `RolePaused` | `role_id` | HIL 单角色暂停 |
| `RoleResumed` | `role_id` | HIL 单角色恢复 |
| `Done` | — | session 结束 |
| `Error` | `kind?` / `message` / `sub_id?` | 错误 |
| `RoleList` | `roles` | 角色列表 |
| `ContextCleared` | — | 上下文清除 |
| `SessionInfo` | `task_id` / `state` / `turn` / `roles` | 会话信息 |
| `ToolUse` | `role_id` / `tool_name` / `args` / `sub_id?` | 工具调用 |
| `ToolResult` | `role_id` / `tool_name` / `result` / `sub_id?` | 工具结果 |
| `ToolError` | `role_id` / `tool_name` / `error` / `sub_id?` | 工具错误 |
| `DelegateStarted` | `from_role` / `to_role` / `task` / `sub_id` | 委派 |
| `DelegateFinished` | `from_role` / `to_role` / `status` / `summary` / `sub_id` | 委派完成 |
| `WorkflowStarted` | `name` / `topic` / `wf_id` | workflow 开始 |
| `WorkflowStep` | `wf_id` / `step_id` / `description` / `index` / `total` / `role_id` / `task` | workflow 步骤 |
| `WorkflowTurn` | `wf_id` / `step_id` / `role_id` / `content` / `round` | workflow 轮次 |
| `WorkflowFinished` | `name` / `wf_id` / `status` / `summary` | workflow 结束 |
| `ImageGenerated` | `role_id` / `path` / `prompt` | generate_image 产物 |
| `PlanProposed` | `role_id` / `plan_id` / `tasks` | plan 工具提交；slash 命令跑完的 workflow 产出含任务清单时也会代发 |
| `ChoiceRequested` | `role_id` / `choice_id` / `question` / `multi` / `layout` / `allow_upload` / `wait` / `options` | ask 工具弹框（`wait=true` = 子代理阻塞等答，答案走 `/api/chat/choice-answer`）。`options[]` = `ChoiceOption`：`label` / `description?` / `image?` / `recommended?` / `pros?` / `cons?` / `details?`（后三项供前端「详情」按钮展开优缺点）。UI 收到即弹居中弹窗，收起后右下角留待办铃 |
| `TimeoutWarning` | `role_id` / `elapsed_secs` / `soft_timeout_secs` / `hard_timeout_secs` / `sub_id?` | 软超时 |
| `AdvisorTerminated` | `role_id` / `reason` / `detector?` / `sub_id?` | advisor 终止 |
| `UserMessage` | `text` | 客户端回放用 |
| `ContextCleared` | — | 上下文清除 |
| `TaskReport` | `role_id` / `task_id` / `summary` / `result` | manager 任务报告（事件 → ui-server → POST /api/tasks/:id/report） |

**Keepalive：** 每 15 秒发送一个注释行（`:`）。

**TypeScript 类型定义：**
```typescript
export type ChatEvent =
  | { type: "RoleTurn"; role_id: string; content: string; is_complete: boolean; sub_id?: string }
  | { type: "Status"; message: string }
  | { type: "Prompt"; icon: string; role_id: string; model_id: string }
  | { type: "Paused"; reason: string }
  | { type: "Resumed" }
  | { type: "RoundStarted"; round: number }
  | { type: "RoundEnded"; round: number }
  | { type: "RoleStarted"; role_id: string; detail: string }
  | { type: "RoleFinished"; role_id: string; detail: string }
  | { type: "RolePaused"; role_id: string }
  | { type: "RoleResumed"; role_id: string }
  | { type: "Done" }
  | { type: "Error"; kind?: string; message: string; sub_id?: string }
  | { type: "RoleList"; roles: RoleInfo[] }
  | { type: "ContextCleared" }
  | { type: "SessionInfo"; task_id: string; state: string; turn: number; roles: RoleInfo[] }
  | { type: "ToolUse"; role_id: string; tool_name: string; args: string; sub_id?: string | null }
  | { type: "ToolResult"; role_id: string; tool_name: string; result: string; sub_id?: string | null }
  | { type: "ToolError"; role_id: string; tool_name: string; error: string; sub_id?: string | null }
  | { type: "DelegateStarted"; from_role: string; to_role: string; task: string; sub_id: string }
  | { type: "DelegateFinished"; from_role: string; to_role: string; status: string; summary: string; sub_id: string }
  | { type: "WorkflowStarted"; name: string; topic: string; wf_id: string }
  | { type: "WorkflowStep"; wf_id: string; step_id: string; description: string; index: number; total: number; role_id: string; task: string }
  | { type: "WorkflowTurn"; wf_id: string; step_id: string; role_id: string; content: string; round: number }
  | { type: "WorkflowFinished"; name: string; wf_id: string; status: string; summary: string }
  | { type: "ImageGenerated"; role_id: string; path: string; prompt: string }
  | { type: "PlanProposed"; role_id: string; plan_id: string; tasks: ImportTask[] }
  | { type: "ChoiceRequested"; role_id: string; choice_id: string; question: string; multi: boolean; layout: string; allow_upload: boolean; wait: boolean; options: ChoiceOption[] }
  | { type: "TimeoutWarning"; role_id: string; elapsed_secs: number; soft_timeout_secs: number; hard_timeout_secs: number; sub_id?: string }
  | { type: "AdvisorTerminated"; role_id: string; reason: string; detector?: string; sub_id?: string }
  | { type: "UserMessage"; text: string }
  | { type: "TaskReport"; role_id: string; task_id: string; summary: string; result: string };
```

---

## 6. Trace / 调试

### `GET /api/logs`

列出或读取 `<cwd>/.latte/ui-sessions/` 下的日志文件。

**查询参数：** 无参数 → 返回文件列表；`?file=<name>&tail=<n>` → 返回该文件末 n 行。

**响应 `200`**

### `GET /api/traces`

列出 `$LATTE_HOME/traces/` 目录下的所有 JSONL trace 文件。

**响应 `200`：** `TraceSummary[]`（含 `session_id` / `path` / `size_bytes` / `modified_unix`）。

### `GET /api/traces/<session_id>`

读取指定会话的完整 trace 事件列表（路径参数是 trace 文件名，不含 `.jsonl`）。

**响应 `200`：** `{ "session_id": "...", "events": [...] }`

### `GET /api/subsessions?id=<sub_id>`

获取委派子会话的完整事件日志（对应 `DelegateStarted.sub_id`）。

**响应 `200`：** ChatEvent 数组。

---

## 7. Self-Loop（AI 自调试）

### `POST /api/self-loop/start`

启动 AI 自调试循环（后台 spawn `tsx self-loop/runner.ts`）。

**请求体：**
```json
{ "task": "让聊天输入框支持自动调整高度", "max_iterations": 5 }
```

**响应 `200`：** `{ "started": true, "task": "...", "max_iterations": 5 }`  
**响应 `409`：** `"self-loop already running"`

### `GET /api/self-loop/events`

Self-loop 进度 SSE 流。

**事件类型：** `self_loop_event`（payload 含 `kind` / `iteration` / `message` / `screenshot` / `data` / `timestamp_unix_ms`）。  
**Ping：** 每 10 秒一个 `event: ping` / `data: ""`。

### `POST /api/self-loop/stop`

停止当前运行的 self-loop。

**请求体：** 无  
**响应 `200`**

---

## 8. 角色-工具关系图

### `GET /api/role-graph`

获取角色 × 工具的代码关系图（结合 TOML 配置的 tool 声明 + TreeSitter 扫 `register_*_tool` 调用点）。

**响应 `200`：** `RoleGraph`（含 `nodes` / `edges` / `stats` / `project_root`）。

---

## 9. 模型管理

### `GET /api/models`

列出合并后的模型 catalog（项目层覆盖后）。

**响应 `200`：** `ModelDef[]`

### `POST /api/models`

创建新模型（写 `.latte/models.d/<key>.toml`）。

**请求体：** `ModelDef`

**响应 `200`**

### `PATCH /api/models/<key>`

更新模型字段（合并到现有 TOML，保留其他字段）。

**请求体：** `ModelDef`（部分字段）

**响应 `200`**

### `DELETE /api/models/<key>`

删除模型（删 `.latte/models.d/<key>.toml`）。

**响应 `200`**

### `POST /api/models/test`

测试单模型（独立 ChatController 跑一轮，跳过 cooldown）。

**请求体：** `{ "model_id": "...", "tier": "standard" }`

**响应 `200`**

### `GET /api/models/<key>/capabilities`

读取模型能力（vision / image_generation / thinking / cost_per_million / timeout_secs）。

**响应 `200`：** `ModelCapabilities`

### `GET /api/models/<key>/toml`

读取模型 TOML 源文件。

**响应 `200`：** `{ "toml": "..." }`

### `PUT /api/models/<key>/toml`

保存模型 TOML 源文件（保留字段顺序）。

**响应 `200`**

---

## 9b. Advisor 开关

### `GET /api/advisor`

读取 advisor 监察的启用状态。

**响应 `200`：** `{ "enabled": true }`

### `PUT /api/advisor`

开关 advisor 监察。

**请求体：** `{ "enabled": false }`

**响应 `200`：** 更新后的状态

## 10. 工具管理

### `GET /api/tools`

列出所有已注册工具（短名 + git.* 展开 + delegate/workflow 等）。

**响应 `200`：** `ToolEntry[]`（含 `id` / `kind` / `description` / `enabled` / `registered_by`）

### `POST /api/tools/test`

测试单工具（独立执行一次）。

**请求体：** `{ "tool_id": "read", "args": { "path": "README.md" } }`

**响应 `200`**

### `POST /api/tools/:id/toggle`

启用/禁用工具（更新 `.latte/tools.yaml`）。

**请求体：** `{ "enabled": false }`

**响应 `200`**

### `GET /api/tools/:id/doc`

读取工具的**模型侧 Markdown 文档** —— 即模型运行时随工具 schema 一起读到的那份说明。
解析顺序与 controller 的 `tool_prompt_content` 一致：磁盘 `<cwd>/prompts/tools/<id>.md`
优先，缺省回退到编译期 `include_str!` 的内置默认文档。

**响应 `200`：**

```json
{
  "id": "read",
  "content": "<instruction>\n读取单个文件…\n</instruction>",
  "editable": true,
  "source": "disk",
  "path": "prompts/tools/read.md",
  "description": "builtin tool: read",
  "kind": "builtin"
}
```

- `source`：`disk`（项目本地覆盖文件，模型实际读的就是这份）| `embedded`（编译期内置默认）| `none`（该工具无任何文档）。
- `editable`：`kind == "dynamic"`（controller 运行时动态注册，如 `delegate` / `workflow`）时为 `false`，只读；其余为 `true`。

### `PUT /api/tools/:id/doc`

写入工具的模型侧 Markdown 文档到 `<cwd>/prompts/tools/<id>.md`，新 session 生效。

**请求体：** Markdown 原文（`text/plain` 风格的裸字符串，不包 JSON）。

**响应 `200`**

**响应 `400`：** 工具为 `dynamic`（动态注册）→ 文档只读；或 `id` 含路径分隔符/`..`（防路径穿越）。

---

## 11. 任务看板

> 设计文档：`docs/task-board-design.md`。存储：`<cwd>/.latte/tasks/{board.json, LAT-*.json, archive/}`。

### `GET /api/tasks`

列出所有任务（含子任务聚合进度）。

**响应 `200`：** `TaskView[]`（含 `actions` / `sub_total` / `sub_done` / `sub_state_counts`）

### `POST /api/tasks`

新建任务。

**请求体：** `CreateTaskRequest`（title / description / priority / labels / parent_id / scheduled_at / workflow / paths）

**响应 `200`：** `TaskView`

### `GET /api/tasks/<id>`

获取任务详情。

**响应 `200`：** `TaskView`

### `PATCH /api/tasks/<id>`

修改任务字段（partial patch）。

**请求体：** `TaskPatch`（与 `create` 相同字段）

**响应 `200`：** `TaskView`

### `DELETE /api/tasks/<id>`

删除任务（文件移入 `archive/`）。

**响应 `200`**

### `POST /api/tasks/import`

批量导入任务（plan 工具批准后调用）。导入的任务一律进 `todo`，是否派发由用户在任务看板手动操作（不做自动调度）。

**请求体：** `ImportTasksRequest`（`tasks` / `plan_id?` / `parent_id?` / `session_id?`）。`parent_id` 三态：任务 id = 显式指定父任务（须为根任务，item 不得再嵌套 `subtasks`）；空串 = 显式「无父任务」；缺省 = 按 `session_id` 查拆分会话映射（`/api/tasks/<id>/refine` 登记，页面刷新不丢）

**响应 `200`：** `ImportTasksResponse`（含 `created`）

### `POST /api/tasks/<id>/refine`

拆分子任务：新建 session 跑 `task_refine` workflow（task_planner 出拆分草案 → reviewer 评审 → 终审门 REJECT 自动返工 → 过审后用 plan 工具提交子任务清单，用户在弹窗勾选导入，前端带 `parent_id`）。plan 提交前对 `paths` 做机械校验（路径前缀必须真实存在、清单内互不重叠），失败整单打回模型修正。不改变任务状态、不记 run。仅根任务可拆；`in_progress`/`merging` 中的任务不可拆。

**请求体：** `{}`

**响应 `200`：** `{ "session_id": "..." }`

### `POST /api/tasks/dispatch-ready`

一键批量派发所有 `todo` 任务（按 priority 排序）。

**请求体：** `{ "max_concurrent": 3 }`

**响应 `200`：** `DispatchReadyResponse`（`dispatched` / `skipped`）

### `POST /api/tasks/<id>/dispatch`

派发单个任务（manager 走 task 调度 / workflow 绑定的任务直接跑 workflow）。

**请求体：** `{}`

**响应 `200`：** `TaskView`

### `POST /api/tasks/<id>/abort`

中止任务执行（取消旗标 + chat_abort）。

**响应 `200`：** `TaskView`

### `POST /api/tasks/<id>/report`

manager 回报任务完成（state → human_review）。

**请求体：** `ReportTaskRequest`（`summary` / `result`：`completed`/`aborted`/`failed`/`timeout`）

**响应 `200`：** `TaskView`

---

## 11b. 任务类型注册表

### `GET /api/task-types`

列出已注册的任务类型（`TaskTypeEntry` 数组）。

### `POST /api/task-types`

新建任务类型。

### `GET /api/task-types/:id`

读取单个任务类型。

### `PUT /api/task-types/:id`

更新任务类型。

### `DELETE /api/task-types/:id`

删除任务类型。

## 12. 工作流管理

> 文件：`workflows.d/<name>.toml`（项目层）或 `~/.latte/workflows.d/`（全局）。引擎：`latte-agent-core/src/workflow.rs`（serial + DAG 双引擎）。

### `GET /api/workflows`

列出所有 workflow（项目 + 全局）。

**响应 `200`：** `WorkflowSummary[]`（含 `name` / `description` / `steps_count` / `source` / `file_path` / `command`）

### `POST /api/workflows`

新建 workflow（写 TOML）。

**请求体：** `WorkflowForm`（name / description / command / max_rounds / steps）

**响应 `200`**

### `GET /api/workflows/<name>`

读 workflow 详情（含原始 TOML）。

**响应 `200`：** `WorkflowDetail`

### `PUT /api/workflows/<name>`

更新 workflow（用 toml_edit 保留字段顺序）。

**请求体：** `WorkflowForm`

**响应 `200`**

### `DELETE /api/workflows/<name>`

删除 workflow（项目层）。

**响应 `200`**

### `POST /api/workflows/validate`

校验 workflow TOML 合法性与 DAG 连通性。

**请求体：** `WorkflowForm`

**响应 `200`：** `ValidateResponse`（`ok` / `errors` / `warnings`）

### `POST /api/workflows/run`

启动 workflow 测试运行（独立 session，事件走 `/api/workflows/run/events` SSE）。

**请求体：** `{ "name": "learn", "topic": "..." }`

**响应 `200`：** `{ "run_id": "wf-..." }`

### `GET /api/workflows/run/events`

workflow run 进度 SSE 流（独立 session，与主 events 隔离）。

### `POST /api/workflows/run/stop`

停止当前 workflow run。

**请求体：** `{}`

**响应 `200`**

### `POST /api/workflows/resume`

从 checkpoint 续跑失败的 workflow（用 checkpoint 的 `wf_id` 定位，跑出新 run）。

**请求体：** `{ "session_id": "...", "wf_id": "...", "topic": "可选" }`

**响应 `202`**

### `GET /api/workflows/<name>/toml`

读取 workflow TOML 源文件。

**响应 `200`：** `{ "toml": "..." }`

### `PUT /api/workflows/<name>/toml`

保存 workflow TOML 源文件。

**响应 `200`**

---

## 13. 文档图像上传

> gen-image 工具（`generate_image`）的产物 + markdown 内引用的图片上传。

### `POST /api/images`

上传图片（multipart/form-data 或 raw body），存 `<cwd>/.latte/images/upload-<ts>.<ext>`。

**支持扩展：** `png` / `jpg` / `jpeg` / `gif` / `webp`  
**上限：** 10 MiB

**响应 `200`：** `{ "path": "/api/images/upload-1747000010000.png" }`

### `GET /api/images/<file>`

下载图片（相对 `.latte/images/` 路径）。

**响应 `200`：** image/* 内容
