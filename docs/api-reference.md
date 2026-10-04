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

### `POST /api/chat/plan-mode`

开关当前会话的 plan mode（只读规划模式）：开启后角色的工具集在**下一个
turn 边界**收敛为只读白名单（read/search/code_graph/ask/plan/todo），方案产出
走 `plan` 工具的既有审批流。开关变化广播 `ChatEvent::PlanModeChanged`（进事件
归档，回放即恢复徽章）。幂等：同值重复设置不重发事件。

**请求体：** `{ "session_id": "abc123", "active": true }`

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

用户按 ⏸ 触发全 session 暂停门：非流式模型请求会被暂停并在 ▶ 后重试同一轮；已产生输出的流式调用和工具在安全边界完成；后续 turn 等待继续。

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
| `TodoUpdated` | `phases` / `sub_id?` | 会话内 todo 列表快照（todo 工具成功应用 ops 后广播）。`phases[]` = `{ name, tasks: [{ content, status }] }`，status ∈ `pending`/`in_progress`/`completed`/`abandoned` |
| `PlanModeChanged` | `active` | plan mode（只读规划模式）开关变化；UI 亮/灭徽章。开关走 `POST /api/chat/plan-mode {session_id?, active}` 或 CLI `/planmode [on|off]`；工具收敛在下一个 turn 边界生效（runner 重建为只读白名单 read/search/code_graph/ask/plan/todo） |

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
  | { type: "TaskReport"; role_id: string; task_id: string; summary: string; result: string }
  | { type: "TodoUpdated"; phases: { name: string; tasks: { content: string; status: "pending" | "in_progress" | "completed" | "abandoned" }[] }[]; sub_id?: string }
  | { type: "PlanModeChanged"; active: boolean };
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
## 9b-goal. Goal 层完成契约

### `GET /api/goal`

读取本 session 的 goal 层完成契约（跨 turn、跨进程存在的目标）。

**查询参数：** `id`（session id，必填）

**响应 `200`（有目标）：**

```json
{
  "goal": {
    "id": "goal-1",
    "revision": 7,
    "objective": "把 loop 的两个缺口补齐并全量验证",
    "phase": "active",
    "blocked_reason": null,
    "round": 3,
    "max_rounds": 12,
    "rounds_remaining": 9,
    "tokens_used": 412330,
    "token_budget": 2000000,
    "tokens_remaining": 1587670
  },
  "auto_continuation": true
}
```

**响应 `200`（无目标）：** `{"goal": null, "auto_continuation": false}`

`phase` ∈ `active | paused | blocked | complete`。`blocked` 时 `blocked_reason`
带稳定 `code`：`round-limit`（轮次耗尽）、`token-budget`（预算耗尽）、
`agent-reported`（模型主动报卡点）。**轮次或预算耗尽产出 `blocked` 而不是
`complete`——耗尽 ≠ 达成。**

`auto_continuation` 是**进程本地**的续跑许可（不持久化），与 `phase`
**正交**：目标可以是 `active` 而续跑是关的。典型场景是进程刚重启——目标从
`<cwd>/.latte/goals/<session_id>.json` 载回，但一律 disarmed，等用户显式
resume。UI 必须把这两个维度分开显示，否则用户看到 `active` 却发现它不动，
无从理解。

controller 还没起时**不会**为了回答这个请求而 spawn 一个（读操作不该有
副作用），此时退回读盘上的快照，`auto_continuation` 恒为 `false`。

**写侧不另开端点**：`/goal set <目标> | show | pause | resume | drop |
rounds <N> | budget <N|off>` 走 `POST /api/chat/command`（见 3 节），模型侧
走 `goal` 工具，三条路共享同一份状态。变更实时经 SSE 的 `GoalUpdated`
事件推送（见 4 节）；本端点用于**刷新页面 / broadcast lag 之后**恢复徽章
——事件是瞬时的，目标不是。

## 9c. 自动上下文压缩设置

### `GET /api/compaction`

读取自动上下文压缩的**生效参数**与各层原始声明。

生效值由后端按 **环境变量 > 项目配置 > 全局配置 > 内置默认** 算出并钳位，
UI 直接渲染即可，不必自己算优先级。

**响应 `200`：**

```json
{
  "effective": {
    "enabled": true,
    "threshold_ratio": 0.8,
    "keep_recent_turns": 2,
    "summary_max_tokens": 2048,
    "max_passes": 1,
    "sections": ["Primary Request and Intent", "..."]
  },
  "project": { "threshold_ratio": null, "...": null },
  "global": { "...": null },
  "env_overrides": ["threshold_ratio"],
  "project_file": "<cwd>/.latte/agents.d/compaction.toml",
  "global_file": "~/.latte/agents.d/compaction.toml"
}
```

`env_overrides` 列出被环境变量接管的字段——那些字段在面板上改了**不会生效**
（env 优先级最高），UI 应据此标灰并提示。

### `PUT /api/compaction`

写 `[compaction]` 到指定层并**立刻热生效**：更新内存 merged 后推进
`ModelResolver` 快照，运行中的 runner 从 resolver 取压缩参数，因此
**已加载的会话下一轮就用新值**，不必重开 session。

**请求体：** `target` 选写哪一层（`"project"` 默认 / `"global"`），其余字段
缺席 = 不配置（交给下层或内置默认）。

```json
{ "target": "project", "threshold_ratio": 0.75, "keep_recent_turns": 4 }
```

**响应 `200`：** 与 `GET` 同结构（回传保存后的生效值）

## 10. 工具管理

### `GET /api/tools`

列出所有已注册工具（短名 + git.* 展开 + delegate/workflow 等）。

**响应 `200`：** `ToolEntry[]`（含 `id` / `kind` / `description` / `enabled` / `registered_by`）

### `POST /api/tools/test`

测试单工具（独立执行一次）。

**请求体：** `{ "tool_id": "read", "args": { "path": "README.md" } }`

**响应 `200`**

### `POST /api/tools/:id/toggle`

启用/禁用工具（更新 `.latte/tools.yaml`）。**仅影响工具面板展示**，
运行时可用工具由角色配置的 `tools` 字段决定。

**请求体：** `{ "enabled": false }`

**响应 `200`**

### `GET /api/tools/:id/doc`

读取工具的**模型侧说明** —— 即模型运行时随工具 schema 一起读到的那份文本。
完整说明与拆分约定见 `docs/tool-docs.md`。

**响应 `200`：**

```json
{
  "id": "read",
  "label": "Read",
  "brief": "读取文件、目录或多个独立目标，支持行范围与批量读取。",
  "builtin_detail": "支持行范围选择器：path:start-end、path:start+count…",
  "summary": "读取文件、目录或多个独立目标，支持行范围与批量读取。",
  "content": "<instruction>\n读取单个文件…\n</instruction>",
  "rendered_content": "<instruction>\n读取单个文件…\n</instruction>",
  "is_template": false,
  "template_vars": ["cwd", "os", "tool", "tool_count", "tools", "doc_path"],
  "template_flags": ["hasread", "haswrite", "iswindows", "…"],
  "editable": true,
  "source": "project",
  "path": "/abs/path/.latte/tools.d/read.md",
  "matched_id": "read",
  "description": "读取文件或目录。把要读的目标放进 `paths` 数组…",
  "kind": "builtin",
  "layers": [
    { "layer": "project", "exists": true,  "path": "/abs/.latte/tools.d/read.md", "matched_id": "read", "active": true },
    { "layer": "global",  "exists": false, "path": "/home/u/.latte/tools.d/read.md", "matched_id": "read", "active": false }
  ]
}
```

- `brief`：一句话简介。md 里写过 `<!-- SUMMARY -->` 就用它，否则取 `description` 的首行/首句。
- `builtin_detail`：`description` 里简介之后的部分。模型总能看到，但编译进代码，面板里只读。
- `summary` / `content`：磁盘 md 的两段原文。**这两个字段是编辑缓冲**（`PUT` 的入参），
  不要直接当简介/详情展示；无 md 时为空串（不能把内置描述灌进编辑器，否则一保存就复制一遍）。
- `rendered_content` / `is_template` / `template_vars` / `template_flags`：模板支持。
  `content` 里含 `{{…}}` 时 `is_template = true`，`rendered_content` 是按当前进程可见工具集
  渲染后的预览（真正下发给模型时按各会话自己的工具集渲染）。
- `source`：`project` | `global` | `none`（此时 `path` 是将要写入的路径）。
- `matched_id`：实际命中的文档 id。走别名回退时与 `id` 不同（`mcp_call` → `mcp`）。
- `kind`：`builtin` | `dynamic` | `package_alias` | `mcp`。
- `layers`：项目层 / 全局层各自的落点（项目层在前），语义与 models 面板一致——
  项目层覆盖全局层。`exists = false` 时 `path` 是**将要写入**的路径，`active` 标出当前生效的那层。

### `PUT /api/tools/:id/doc`

写入项目文档 `<cwd>/.latte/tools.d/<id>.md`，**并就地刷新所有活跃会话的工具描述**。

**请求体：** `{ "summary": "一句话简介", "content": "Markdown 详情（可含模板）", "target": "project" }`
—— 后端合成为带 `<!-- SUMMARY -->` / `<!-- DETAILS -->` 标记的 md。

`target` 与 models 的 `update_model` 同名同义：`project`（默认，写
`<cwd>/.latte/tools.d/<id>.md`）| `global`（写 `$LATTE_HOME/tools.d/<id>.md`）。
UI 上就是「保存到项目 / 保存到全局」两个按钮，共用这一条路由。其它值 → 400。

**响应 `200`：**

```json
{ "path": "/abs/path/.latte/tools.d/read.md", "source": "project", "refreshed_managers": 2 }
```

`refreshed_managers` 是就地刷新了几个活跃 `ToolManager`（>0 表示进行中的会话已经用上新文档，
不需要重开 session）。

**响应 `400`：** `id` 含路径分隔符 / `..`（防路径穿越）。

### `DELETE /api/tools/:id/doc?target=project|global`

删掉指定层的文档（`target` 默认 `project`），返回体同 `PUT`。
删项目层后若全局层还有文档，运行时**自动回落到全局层**（不是变成「无文档」）。
幂等（文件本来就没有也返回 200）；层必须显式给出，不做隐式跨层删除。

---

## 11. 任务看板

> 设计文档：`docs/task-board-design.md`。存储：`<cwd>/.latte/tasks/{board.json, LAT-*.json, archive/}`。

### `GET /api/tasks`

列出所有任务（含后代聚合进度）。

**响应 `200`：** `TaskView[]`（含 `actions` / `sub_total` / `sub_done` / `sub_state_counts` / `blocked_by`）

`blocked_by` 是服务端算好的**未满足依赖 id 列表**（`depends_on` 里尚未进终态的那些）。非空
= 现在派发必然 409，前端据此把「派发」按钮置灰。

`dispatch_pending` = 已派过但被闸挡下、**已排队等自动续跑**。scheduler 每 5s 扫一遍这些
任务，闸一放开就自动派发（遵守同一个并发上限）；用户把任务移出 `todo`/`rework` 即撤销。

`isolated` / `isolated_effective` / `worktree` = 隔离执行（Phase 2）。`isolated` 是任务自己的
声明（`null` = 跟随 `task_type`），`isolated_effective` 是服务端解析后的结果，`worktree` 是
已建好的工作树（`{root, branch, base_branch, base_sha}`；`null` = 当前没有）。

### `POST /api/tasks`

新建任务。

**请求体：** `CreateTaskRequest`（title / description / priority / labels / parent_id / scheduled_at / workflow / paths / depends_on / isolated）

`depends_on` = 依赖的任务 id 数组，全部进终态（`done`/`cancelled`）后本任务才可派发。
未知 id / 自依赖 / 父子关系 / 成环一律 `400`（不静默丢弃）。父子是**聚合**关系不是依赖，
父任务作为容器本来就等子任务，所以禁止在同一条祖先链上声明依赖。

**响应 `200`：** `TaskView`

### `GET /api/tasks/<id>`

获取任务详情。

**响应 `200`：** `TaskView`

### `PATCH /api/tasks/<id>`

修改任务字段（partial patch）。

**请求体：** `TaskPatch`（与 `create` 相同字段）。`paths` / `depends_on` 是**整体替换**语义
（空数组 = 清空）；`depends_on` 走与 `create` 相同的校验。`isolated` 三态：缺省 = 不变，
`null` = 跟随类型默认，`true`/`false` = 显式。

**响应 `200`：** `TaskView`

### `DELETE /api/tasks/<id>`

删除任务（文件移入 `archive/`）。

**响应 `200`**

### `POST /api/tasks/import`

批量导入任务（plan 工具批准后调用）。导入的任务一律进 `todo`，是否派发由用户在任务看板手动操作（不做自动调度）。

**请求体：** `ImportTasksRequest`（`tasks` / `plan_id?` / `parent_id?` / `session_id?`）。
item 的 `depends_on` 既可写已存在的任务 id，也可写**同一批任务的标题**（新任务此时还没有 id）；
解析在全部任务创建完成后统一进行，所以前向引用（依赖清单里后面才出现的任务）同样有效。
解析不到或标题歧义 → 整单 `400` 并点名该引用（不静默丢弃）。`parent_id` 三态：任务 id = 显式指定父任务（任意深度任务树：父任务本身也可以是子任务，item 的 `subtasks` 也可继续嵌套）；空串 = 显式「无父任务」；缺省 = 按 `session_id` 查拆分会话映射（`/api/tasks/<id>/refine` 登记，页面刷新不丢）

**响应 `200`：** `ImportTasksResponse`（含 `created`）

### `POST /api/tasks/<id>/refine`

拆分子任务：新建 session 跑 `task_refine` workflow（task_planner 出拆分草案 → reviewer 评审 → 终审门 REJECT 自动返工 → 过审后用 plan 工具提交子任务清单，用户在弹窗勾选导入，前端带 `parent_id`）。plan 提交前对 `paths` 做机械校验（路径前缀必须真实存在、清单内互不重叠；**声明了 `depends_on` 关系的一对任务不参与重叠校验**——它们由看板串行执行，重叠只在并发时有害），失败整单打回模型修正。不改变任务状态、不记 run。任意层级任务都可拆（任务树深度不限）；`in_progress`/`merging` 中的任务不可拆。

**请求体：** `{}`

**响应 `200`：** `{ "session_id": "..." }`

### `POST /api/tasks/dispatch-ready`

一键批量派发所有 `todo` 任务（按 priority 排序），遵守并发上限、依赖与文件范围约束；
被挡下的任务留在 `todo` 并在 `skipped` 里带原因。

**请求体：** `{ "max_concurrent": 3 }`（缺省读 `LATTE_DISPATCH_MAX_CONCURRENT`，默认 3）

**响应 `200`：** `DispatchReadyResponse`（`dispatched` / `skipped`）

### `POST /api/tasks/<id>/dispatch`

派发单个任务（manager 走 task 调度 / workflow 绑定的任务直接跑 workflow）。

**派发闸（按顺序）：**

1. 状态必须是 `todo`/`rework`，否则 `400`；
2. **依赖未满足** → `409`，消息点名等哪些任务及其当前状态；
3. **同族文件冲突** → `409`。同一棵任务树下的任务**默认并行**；只有在无法证明它们不碰
   同一批文件时才互斥——双方都声明了 `paths` 且不重叠即放行，任一方未声明或声明后重叠
   则拒绝。容器祖先在跑不阻塞后代。
4. **跨族 paths 重叠** → `409`。

**隔离任务（`isolated_effective = true`）跳过第 3、4 条**：它们各自在独立 git worktree 里
执行，物理上不可能互相覆盖，`paths` 重叠也无所谓。依赖闸（第 2 条）照旧生效。

隔离任务的生命周期：派发时在 `$LATTE_HOME/worktrees/<项目>/<任务 id>` 建工作树（分支
`latte/task/<id>`，从当前 base 分支派生），agent 与 workflow 的工作目录都指向它
（`merging`/`done` 的生命周期钩子也跑在树里——`merge` 钩子要在**改动所在的树**里跑测试，
跑主仓等于验证一份还没合并的工作区）；
`state → merging` 只跑 `merge` 钩子（在**树里**验证 + 出提交包，不动 base）；
`state → done` 才机械合并回 base、回收工作树，然后归档钩子在主仓总结。
合并刻意不挂在 `merging`——那样它会排在 LLM 验证流之后，模型不可用或流程等人拍板时
合并被无限期拖住，而用户点批准的意思就是"合进去"。冲突则**保留工作树与分支**、把冲突
详情写进任务 history 等人处置；`cancelled` 回收但**保留分支**（未合并的提交还拿得到）。仓库不是 git 时隔离自动降级为普通派发，
原因记在 history 里。

⚠️ 合并只带 git 认得的改动，**被 gitignore 忽略的产物不会带回**（构建产物本就该丢，但把
文档交付物目录 gitignore 掉的项目要注意）。代码图索引在隔离树里用独立文件，不与主仓
共用——否则隔离树改过的代码会覆盖主仓索引、让所有人查到错的行号。

带未完成子任务的父任务走**容器分流**：父任务标记 `in_progress` 作为聚合节点，逐个派发
`todo` 子任务；被闸挡下的子任务留在 `todo`，原因写进该子任务的 history（看板可见）。

**请求体：** `{}`

**响应 `200`：** `TaskView`

### `POST /api/tasks/<id>/abort`

中止任务执行（取消旗标 + chat_abort）。

**响应 `200`：** `TaskView`

### `POST /api/tasks/<id>/resume`

**重试执行**：让任务绑定的执行 session 从断点续跑——服务重启/网络/模型故障导致
执行中断后，用户在看板点「↺ 重试」。**任务状态不变**（仍 `in_progress`，run 的
`ended_at` 保持 `None`——续跑的是同一个 run，不是新开）。

底层走 `POST /api/chat/resume` 的同一机制：workflow 任务从 checkpoint 跳过已完成
step 续跑（`run_workflow_resume`）；经典 chat 任务恢复 controller/gate。

启动对账（`reconcile_interrupted_runs`）不再把中断任务回退 `todo`：任务与 session
绑定，重启只在 history 标记「上次执行中断」并清 `dispatch_pending`，由用户决定
「重试」（续跑）还是「中止」（放弃这次 run 回 `todo` 重新派发）。

仅 `in_progress` + 有进行中 run 可调；绑定的 session 已被删 → `400`（提示改走中止）。

**响应 `200`：** `TaskView`

### `GET /api/tasks/<id>/workflow-runs`

列出该任务关联的所有 workflow run（`learn_loop` / `code_review` / `merge` 等
lifecycle hook 与派发 workflow 都算；按 `session_id` 关联、`started_at` 升序）。

**响应 `200`：** `TaskWorkflowRunSummary[]`

### `GET /api/tasks/<id>/workflow-runs/<wf_id>`

单条 run 完整详情（含全部 step output 全文与 ask 回答）。`wf_id` 必须属于该任务，
否则 `404`。

**响应 `200`：** `TaskWorkflowRunDetail`

### `GET /api/tasks/<id>/worktree-diff`

隔离任务的**改动预览**：批准（`merging`）等于把工作树合并回 base，批之前得先看得见
改的是什么——此前用户只能看 workflow 的文字摘要。

口径与合并**完全一致**：已提交 + 未提交 + 未跟踪的新文件都算（`merge_back` 的 `add -A`
会把它们一起提交），比较基准是 base 分支当前 tip（分支已不在时退回派生点 SHA）。合并完
再查会得到空结果。

**响应 `200`：** `WorktreeDiff | null`（`null` = 非隔离任务，或已合并回收）

```json
{
  "branch": "latte/task/LAT-101", "root": "/Users/x/.latte/worktrees/proj-ab12cd34/LAT-101",
  "base": "main",
  "files": [{ "path": "src/ringbuf.c", "added": 42, "removed": 7, "binary": false }],
  "file_count": 3, "total_added": 51, "total_removed": 7,
  "commits": [{ "sha": "a1b2c3d", "subject": "task(LAT-101): 实现环形缓冲" }]
}
```

`files` 最多 40 个（大重构能改上千文件，全塞进响应只会拖慢抽屉）；`file_count` 是**未截断
的真实总数**，前端据此显示「还有 N 个」。

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

---

## 14. 文档只读资源（markdown 内嵌预览）

> 前端 markdown 渲染把本地相对路径图片改写到 `/api/doc-asset`，把本地
> `.md` 链接交给内嵌文档查看器（`/api/doc`）。两个路由共用白名单：
> 只允许读 `<cwd>` 下 `.latte/learn/`、`.latte/sessions/<sid>/learn/`（learn_loop 会话学习工作区）、`docs/` 里的文件；拒绝对路径、
> `..`、反斜杠。

### `GET /api/doc-asset?path=<相对路径>`

读取文档内嵌图片（白名单目录 + 扩展名白名单）。

**支持扩展：** `png` / `jpg` / `jpeg` / `gif` / `webp` / `svg`  
**错误：** `400` 路径非法或类型不支持；`404` 文件不存在

**响应 `200`：** image/* 内容

### `GET /api/doc?path=<相对路径>`

只读 markdown 文档（内嵌预览用），仅 `.md`，上限 1 MiB。

**错误：** `400` 非 .md 或路径非法；`404` 不存在；`413` 超限

**响应 `200`：** `text/markdown; charset=utf-8` 全文

---

## 15. 代码图谱可视化

> code_graph 的一键可视化：结构图读预建符号索引
> （`.latte/code_graph/index.json`），调用图现场扫 ast-grep（索引定
> caller 归属，绕过工具侧的 200 条/字符封顶）。返回 mermaid 图源文本，
> 前端走统一 ```mermaid 渲染管线。

### `GET /api/code-graph/mermaid?path=<rel>&graph=structure|calls`

**参数：** `path` 相对仓库根（默认 `.`）；`graph` = `structure`（默认）| `calls`  
**错误：** `400` graph 非法；`422` 索引未构建/范围无符号/ast-grep 缺失等（body 为可读原因）

**响应 `200`：** `{ "mermaid": "flowchart TD\n…", "truncated": false }`
