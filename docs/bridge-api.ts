// ============================================================================
// latte-agent-ui ↔ latte-code-editor API 桥接映射
// ============================================================================
//
// 本文件记录两个代码库之间的 API 对应关系：
//
//   latte-agent-ui (HTTP/SSE)             → HTTP REST + Server-Sent Events
//     └─ latte-rs-agents/latte-agent-cli/ui/src/api.ts
//     └─ latte-rs-agents/latte-agent-cli/src/commands/ui.rs
//
//   latte-code-editor (Tauri invoke)      → Tauri IPC invoke + event listen
//     └─ latte-code-editor/src/api/chat.ts
//     └─ latte-code-editor/src-tauri/src/chat_panel/commands.rs
//
// 完整 agent-ui 端点列表见 `docs/api-reference.md`（由
// `latte-agent-ui-server/src/api_reference_integrity.rs` 校验一致性）。
//
// 本文件**只整理跨端映射**——明确哪些功能在 code-editor 已有对应 invoke，
// 哪些只有 agent-ui 端点（code-editor 暂未实现）。
//
// 版本: 2.0
// 日期: 2026-08-09
// ============================================================================

// ─── 类型映射 ─────────────────────────────────────────────────────────────

/**
 * agent-ui: GET /api/sessions 返回的会话摘要。
 * code-editor: chat_session_list (invoke → SessionSummary[])
 */
export interface BridgeSessionSummary {
  // agent-ui 字段                                  // code-editor 等效字段
  session_id: string;                              //  sessionId
  preview: string;                                 //  (从 messages 推断)
  initial_role: string;                            //  roleIds[0]
  created_at_unix_ms: number;                      //  createdAt
  last_activity_unix_ms: number;                   //  updatedAt
  is_paused?: boolean;                             //  Paused
  label?: string | null;                           //  label
}

/**
 * agent-ui: GET /api/roles 返回的角色信息。
 * code-editor: chat_list_roles → RoleInfo[]
 */
export interface BridgeRoleInfo {
  id: string;
  name: string;
  icon: string;
  // code-editor 额外字段:
  category?: string;
  defaultModelTier?: string;
  modelChain?: string[];
}

/**
 * agent-ui: GET /api/session?id=... 返回的当前会话详情。
 * code-editor: 无直接对应——code-editor 由客户端管理 sessionId。
 */
export interface BridgeSessionInfo {
  session_id: string;
  role: string;
  model: string | null;
  tier: string;
  is_paused: boolean;
  available_sessions: BridgeSessionSummary[];
  available_roles: BridgeRoleInfo[];
}

/**
 * ChatEvent 双端事件类型共用的字段子集（agent-ui 与 code-editor 字段名一致）。
 */
export interface BridgeChatEventCommon {
  type: string;
  role_id?: string;
  content?: string;
  is_complete?: boolean;
  sub_id?: string;
  message?: string;
  icon?: string;
  model_id?: string;
  reason?: string;
  round?: number;
  task_id?: string;
  state?: string;
  turn?: number;
  roles?: BridgeRoleInfo[];
  tool_name?: string;
  args?: string;
  result?: string;
  error?: string;
  from_role?: string;
  to_role?: string;
  status?: string;
  summary?: string;
  detail?: string;
  choices?: ChoiceOption[];
  multi?: boolean;
  layout?: string;
  allow_upload?: boolean;
  options?: ChoiceOption[];
  iteration?: number;
  elapsed_secs?: number;
  soft_timeout_secs?: number;
  hard_timeout_secs?: number;
  detector?: string;
  text?: string;
  choice_id?: string;
  question?: string;
  path?: string;
  prompt?: string;
  plan_id?: string;
  tasks?: BridgeTask[];
  wf_id?: string;
  step_id?: string;
  description?: string;
  index?: number;
  total?: number;
  name?: string;
  topic?: string;
}

export interface ChoiceOption {
  label: string;
  description?: string;
  image?: string;
  recommended?: boolean;
}

export interface BridgeTask {
  title: string;
  description?: string;
  priority?: number;
  labels?: string[];
  workflow?: string;
  paths?: string[];
  subtasks?: BridgeTask[];
}

// ─── 命名空间：每个端点的代码-editor 映射 ──────────────────────────────────
//
// 标注规则：
//   "agent-ui HTTP 路径" : "agent-ui 端点描述 + 调用方式"
//   "code-editor"        : 已知 invoke / 已知 SSE event / (暂未实现)
//
// 设计原则：bridge-api 只记录**真实存在**的代码-editor 接口。
// 注释 // 后面是端口迁移建议（怎么把 agent-ui 行为包到 code-editor）。
// 对于 code-editor 没有对应功能的 agent-ui 端点，标 "（code-editor 暂未实现）"。

/**
 * §1. 健康检查
 *
 * agent-ui:  GET /health
 *   liveness 探针，返回 "ok"。
 * code-editor: (无对应——Tauri 进程由 OS 管理，无需外部探针)
 */
export function mapHealth() {
  // agent-ui:  GET /health → "ok"
  // code-editor: (none)
}

/**
 * §2. 会话管理
 *
 * agent-ui:  GET /api/sessions
 *   → listSessions() → BridgeSessionSummary[]
 * code-editor:  invoke("chat_session_list") → SessionSummary[]
 *
 * agent-ui:  POST /api/sessions
 *   → createSession() → BridgeSessionInfo
 * code-editor:  invoke("chat_controller_spawn") (client chooses sessionId)
 *
 * agent-ui:  DELETE /api/sessions?id=<sid>
 *   → deleteSession() → void
 * code-editor:  invoke("chat_session_delete", { sessionId }) → void
 *
 * agent-ui:  POST /api/sessions/fork
 *   从源会话的事件前缀 fork 新会话。
 *   → forkSession(sourceId, events) → BridgeSessionInfo
 * code-editor:  (暂未实现——建议: 在 spawn 后手动 replay ChatEvent 历史)
 *
 * agent-ui:  GET /api/session?id=<sid>
 *   → getSession(id) → BridgeSessionInfo
 * code-editor:  invoke("chat_session_get", { sessionId }) → StoredSession
 *
 * agent-ui:  GET /api/session/history?id=<sid>
 *   → sessionHistory(id) → ChatEvent[]
 * code-editor:  invoke("chat_session_get", { sessionId }).messages
 *   （data 格式需转换：code-editor 的 messages vs agent-ui 的 ChatEvent 流）
 *
 * agent-ui:  POST /api/session/label
 *   重命名会话。
 *   → setSessionLabel(id, label) → void
 * code-editor:  (暂未实现——建议: 在 StorageSession 上扩展 label 字段)
 */
export function mapSessions() {
  // 端口时：GET → invoke("chat_session_list"); POST → invoke("chat_controller_spawn")
  //         DELETE → invoke("chat_session_delete"); GET(id) → invoke("chat_session_get")
  //         history 与 label 暂无可直接复用。
}

/**
 * §3. 角色查询
 *
 * agent-ui:  GET /api/roles
 *   → getRoles() → BridgeRoleInfo[]
 * code-editor:  invoke("chat_list_roles") → RoleInfo[] (含更多字段)
 *
 * agent-ui:  GET /api/roles/config
 *   → getRolesConfig() → RolesConfigResponse
 * code-editor:  invoke("chat_get_role_config") → RoleConfigResponse
 *
 * agent-ui:  POST /api/roles
 *   → createRole(req) → RoleConfigEntry
 * code-editor:  (暂未实现独立入口——直接写 ~/.latte/agents.d/<id>.toml)
 *
 * agent-ui:  DELETE /api/roles/:id
 *   → deleteRole(id) → void
 * code-editor:  (暂未实现)
 *
 * agent-ui:  GET /api/roles/:id/toml
 *   → getRoleToml(id) → {toml: string}
 * code-editor:  (暂未实现)
 *
 * agent-ui:  PUT /api/roles/:id/toml
 *   → putRoleToml(id, toml) → void
 * code-editor:  (暂未实现)
 *
 * agent-ui:  POST /api/roles/test
 *   单角色测试运行。
 *   → testRole(req) → void
 * code-editor:  (暂未实现)
 */
export function mapRoles() {
  // 端口时：GET /api/roles → invoke("chat_list_roles")
  //         GET /api/roles/config → invoke("chat_get_role_config")
  //         其余 CRUD 与 TOML 写入需新加 Tauri command。
}

/**
 * §4. 聊天命令
 *
 * agent-ui:  POST /api/chat/send
 *   → sendMessage(req) → 202 ACCEPTED
 * code-editor:  invoke("chat_controller_submit", { sessionId, text })
 *
 * agent-ui:  POST /api/chat/command
 *   → sendCommand(req) → 202 ACCEPTED
 * code-editor:  invoke("chat_controller_submit", { sessionId, text })
 *   （code-editor 无独立 command 端点；统一走 submit，command 视作普通文本）
 *
 * agent-ui:  POST /api/chat/role
 *   切换当前会话角色。
 *   → switchRole(req) → 202 ACCEPTED
 * code-editor:  (切角色由 spawn 时 roles 数组固定；动态切换需 abort + respawn)
 *
 * agent-ui:  POST /api/chat/cancel-turn
 *   取消当前 turn（不退出 session）。
 *   → cancelTurn(req) → 200 OK
 * code-editor:  invoke("chat_controller_submit", { sessionId, "" })（弱等效——不一定中断 in-flight）
 *
 * agent-ui:  POST /api/chat/abort
 *   终止整个 session（同时连带取消 workflow resume）。
 *   → abortSession(req) → 200 OK
 * code-editor:  invoke("chat_controller_abort", { sessionId }) → void
 *
 * agent-ui:  POST /api/chat/pause
 *   暂停当前 turn。
 *   → pauseSession(req) → 200 OK
 * code-editor:  invoke("chat_controller_pause", { sessionId }) → void
 *
 * agent-ui:  POST /api/chat/resume
 *   恢复暂停的 turn。
 *   → resumeSession(req) → 200 OK
 * code-editor:  invoke("chat_controller_resume", { sessionId }) → void
 *
 * agent-ui:  POST /api/chat/pause-session
 *   全 session 冻结。
 *   → pauseSession(req) → 200 OK
 * code-editor:  invoke("chat_controller_pause", { sessionId })（复用同一命令）
 *
 * agent-ui:  POST /api/chat/resume-session
 *   全 session 恢复。
 *   → resumeSession(req) → 200 OK
 * code-editor:  invoke("chat_controller_resume", { sessionId })
 *
 * agent-ui:  POST /api/chat/resume-role
 *   HIL 单角色恢复。
 *   → resumeRole(req) → 202 ACCEPTED
 * code-editor:  (暂未实现——agent-ui 特有：per-role pause HIL)
 *
 * agent-ui:  POST /api/chat/stream-mode
 *   切换流式/非流式输出。
 *   → setStreamMode(req) → 200 OK
 * code-editor:  (由 chat_stream 一次性同步决定)
 */
export function mapChat() {
  // 端口时：submit/abort/pause/resume 一一对应 invoke；role/command 需视为
  // 普通文本（"/role xxx"）；cancel-turn 与 pause-session 暂无可用 invoke。
}

/**
 * §5. SSE 事件流
 *
 * agent-ui:  GET /api/events?id=<sid>
 *   → subscribeEvents(onEvent) → unsubscribe
 * code-editor:  listen("chat:controller_event", (event) => { ... })
 *
 * agent-ui:  GET /api/self-loop/events
 *   → subscribeSelfLoop(onEvent) → unsubscribe
 * code-editor:  (暂未实现——Self-Loop 是 agent-ui 特有功能)
 */
export function mapEventsSSE() {
  // 端口时：用 Tauri event.listen 替换 SSE EventSource；事件类型与 ChatEvent
  // 同构但字段在 agent-ui / code-editor 之间 PascalCase ↔ camelCase 转换。
}

/**
 * §6. Trace / 调试
 *
 * agent-ui:  GET /api/traces
 *   → listTraces() → TraceSummary[]
 * code-editor:  (暂未实现——建议: 复用 chat_session_list 跨语义)
 *
 * agent-ui:  GET /api/traces/<session_id>
 *   → readTrace(sessionId) → {events: TraceEvent[]}
 * code-editor:  (暂未实现)
 *
 * agent-ui:  GET /api/subsessions?id=<sub_id>
 *   → fetchSubsession(subId) → ChatEvent[]
 * code-editor:  (暂未实现——agent-ui 特有的委派子会话)
 */
export function mapTraces() {
  // 暂未实现。
}

/**
 * §7. Self-Loop（AI 自调试）
 *
 * agent-ui:  POST /api/self-loop/start
 *   → startSelfLoop(task, maxIter) → void
 * code-editor:  (暂未实现)
 *
 * agent-ui:  POST /api/self-loop/stop
 *   → stopSelfLoop() → void
 * code-editor:  (暂未实现)
 */
export function mapSelfLoop() {
  // 暂未实现。
}

/**
 * §8. 角色-工具关系图
 *
 * agent-ui:  GET /api/role-graph
 *   → fetchRoleGraph() → RoleGraph
 * code-editor:  invoke("build_code_graph", { ... }) (不同签名——基于 workspace)
 */
export function mapRoleGraph() {
  // 端点语义不同：agent-ui 在 server 启动时基于 cwd 固定生成；
  // code-editor 在 Tauri 命令调用时基于当前 workspace 动态生成。
}

/**
 * §9. 模型管理
 *
 * agent-ui:  GET /api/models
 *   → listModels() → ModelDef[]
 * code-editor:  invoke("chat_list_models") → ModelInfo[]
 *
 * agent-ui:  POST /api/models
 *   → createModel(req) → void
 * code-editor:  (暂未实现)
 *
 * agent-ui:  PATCH /api/models/<key>
 *   → updateModel(key, req) → void
 * code-editor:  invoke("chat_set_role_model_chain", { roleId, chain: [key] })
 *   （间接：通过角色 model_chain）
 *
 * agent-ui:  DELETE /api/models/<key>
 *   → deleteModel(key) → void
 * code-editor:  (暂未实现)
 *
 * agent-ui:  POST /api/models/test
 *   → testModel(req) → void
 * code-editor:  (暂未实现)
 *
 * agent-ui:  GET /api/models/<key>/capabilities
 *   → modelCapabilities(key) → ModelCapabilities
 * code-editor:  (暂未实现——chat_list_models 已含 capabilities 字段)
 *
 * agent-ui:  GET  /api/models/<key>/toml
 * agent-ui:  PUT  /api/models/<key>/toml
 * code-editor:  (暂未实现)
 */
export function mapModels() {
  // 端口时：GET → invoke("chat_list_models")；
  // PATCH role_chain 是间接路径；其他写操作暂无。
}

/**
 * §10. 工具管理
 *
 * agent-ui:  GET /api/tools
 *   → listTools() → ToolEntry[]
 * code-editor:  (暂未实现)
 *
 * agent-ui:  POST /api/tools/test
 *   → testTool(req) → void
 * code-editor:  (暂未实现)
 *
 * agent-ui:  POST /api/tools/<id>/toggle
 *   → toggleTool(req) → void
 * code-editor:  (暂未实现)
 */
export function mapTools() {
  // 暂未实现。
}

/**
 * §11. 任务看板
 *
 * agent-ui:  GET /api/tasks
 *   → listTasks() → TaskView[]
 * code-editor:  (暂未实现)
 *
 * agent-ui:  POST /api/tasks
 *   → createTask(req) → TaskView
 * code-editor:  (暂未实现)
 *
 * agent-ui:  GET    /api/tasks/<id>
 * agent-ui:  PATCH  /api/tasks/<id>
 * agent-ui:  DELETE /api/tasks/<id>
 *   → getTask/updateTask/deleteTask
 * code-editor:  (暂未实现)
 *
 * agent-ui:  POST /api/tasks/import
 *   → importTasks(req) → ImportTasksResponse
 * code-editor:  (暂未实现)
 *
 * agent-ui:  POST /api/tasks/dispatch-ready
 *   → dispatchReady(req) → DispatchReadyResponse
 * code-editor:  (暂未实现)
 *
 * agent-ui:  POST /api/tasks/<id>/dispatch
 *   → dispatchTask(id) → TaskView
 * code-editor:  (暂未实现)
 *
 * agent-ui:  POST /api/tasks/<id>/abort
 *   → abortTask(id) → TaskView
 * code-editor:  (暂未实现)
 *
 * agent-ui:  POST /api/tasks/<id>/report
 *   → reportTask(id, summary, result) → TaskView
 * code-editor:  (暂未实现)
 */
export function mapTasks() {
  // 任务看板是 agent-ui 特有功能；code-editor 暂未实现对应管理界面。
}

/**
 * §12. 工作流管理
 *
 * agent-ui:  GET /api/workflows
 *   → listWorkflows() → WorkflowSummary[]
 * code-editor:  invoke("chat_list_workflows") → WorkflowInfo[]
 *
 * agent-ui:  POST /api/workflows
 *   → createWorkflow(req) → WorkflowDetail
 * code-editor:  invoke("chat_save_workflow", { payload }) → WorkflowMutationResult
 *
 * agent-ui:  GET /api/workflows/<name>
 *   → getWorkflow(name) → WorkflowDetail
 * code-editor:  invoke("chat_get_workflow_full", { id }) → WorkflowPayload
 *
 * agent-ui:  PUT /api/workflows/<name>
 *   → updateWorkflow(name, req) → void
 * code-editor:  invoke("chat_save_workflow", { payload }) → WorkflowMutationResult
 *
 * agent-ui:  DELETE /api/workflows/<name>
 *   → deleteWorkflow(name) → void
 * code-editor:  invoke("chat_delete_workflow", { id }) → WorkflowMutationResult
 *
 * agent-ui:  POST /api/workflows/validate
 *   → validateWorkflow(req) → ValidateResponse
 * code-editor:  (暂未实现独立入口)
 *
 * agent-ui:  POST /api/workflows/run
 *   → workflowRunStart(req) → {run_id}
 * code-editor:  (暂未实现)
 *
 * agent-ui:  GET /api/workflows/run/events (SSE)
 *   → workflowRunSubscribe(onEvent) → unsubscribe
 * code-editor:  (暂未实现)
 *
 * agent-ui:  POST /api/workflows/run/stop
 *   → workflowRunStop() → void
 * code-editor:  (暂未实现)
 *
 * agent-ui:  POST /api/workflows/resume
 *   从 checkpoint 续跑失败 workflow。
 *   → workflowResume(req) → void
 * code-editor:  (暂未实现)
 *
 * agent-ui:  GET /api/workflows/<name>/toml
 * agent-ui:  PUT /api/workflows/<name>/toml
 *   → getWorkflowToml/putWorkflowToml
 * code-editor:  (暂未实现)
 */
export function mapWorkflows() {
  // 端口时：list/get/save/delete 已有 invoke；validate/run/resume/toml 暂未实现。
}

/**
 * §13. 文档图像上传
 *
 * agent-ui:  POST /api/images
 *   multipart/form-data 或 raw body → <cwd>/.latte/images/upload-<ts>.<ext>
 * code-editor:  (暂未实现——浏览器内 upload 通过 fetch 直传)
 *
 * agent-ui:  GET /api/images/<file>
 *   → 静态图片字节
 * code-editor:  (暂未实现)
 */
export function mapImages() {
  // 暂未实现。
}

/**
 * HIL / Manager / Swarm：code-editor 独有功能。
 *
 * agent-ui:  (无直接对应；agent-ui 转发 ChatEvent 给前端，
 *   UI 弹出暂停门 / 决策面板 / ChoiceRequested 弹框自己实现)
 *
 * code-editor:  9 × chat_hil_* + 3 × chat_manager_* + 1 × chat_swarm_*
 *   （详见 mapController / mapHIL / mapManagerSession / mapSwarmSession）
 */
export function mapEditorOnly() {
  // 这三类是 code-editor 独有的多角色控制能力。
  // agent-ui 端通过 §4 聊天命令 + §5 SSE 事件 + UI 自行实现等价功能。
}

// ─── ChatEvent 双向映射 ─────────────────────────────────────────────────────
//
// 完整列表见 `docs/api-reference.md` §5 与
// `latte-agent-core/src/event_json.rs` 中 `chat_event_to_frontend_json`。
//
// agent-ui 用 internally-tagged JSON（{ type: "VariantName", field1, ... }）；
// code-editor 用 Tauri 事件（{ sessionId, kind: "camelCaseKind", ... }）。
//
// PascalCase ↔ camelCase 转换。

export const CHAT_EVENT_MAPPING: Record<string, string | null> = {
  // ··· 1:1 映射 ···
  RoleTurn: "roleTurn",
  Status: "status",
  Prompt: "prompt",
  Paused: "paused",
  Resumed: "resumed",
  RoundStarted: "roundStarted",
  RoundEnded: "roundEnded",
  Done: "done",
  Error: "error",
  RoleList: "roleList",
  ContextCleared: "contextCleared",
  SessionInfo: "sessionInfo",
  ToolUse: "toolUse",
  ToolResult: "toolResult",
  // ··· agent-ui 额外（code-editor 暂无对应）···
  RoleStarted: null,
  RoleFinished: null,
  RolePaused: null, // HIL per-role
  RoleResumed: null,
  DelegateStarted: null,
  DelegateFinished: null,
  ToolError: null,
  ImageGenerated: null,
  PlanProposed: null,
  ChoiceRequested: null,
  TimeoutWarning: null,
  AdvisorTerminated: null,
  UserMessage: null,
  TaskReport: null,
  WorkflowStarted: null,
  WorkflowStep: null,
  WorkflowTurn: null,
  WorkflowFinished: null,
};

export const CHAT_EVENT_MAPPING_REVERSE: Record<string, string | null> = Object.fromEntries(
  Object.entries(CHAT_EVENT_MAPPING)
    .filter(([, v]) => v !== null)
    .map(([k, v]) => [v as string, k])
);

// ─── 端口指南 ──────────────────────────────────────────────────────────────
//
// 从 agent-ui 到 code-editor 的基本端口步骤：
//
// 1. 会话创建
//    agent-ui:  POST /api/sessions → 服务端分配 session_id
//    code-editor: invoke("chat_controller_spawn", { request: { sessionId, roles, ... } })
//    注意：code-editor 需客户端生成 sessionId（如 crypto.randomUUID()）。
//
// 2. 发送消息
//    agent-ui:  POST /api/chat/send { session_id, message }
//    code-editor: invoke("chat_controller_submit", { sessionId, text })
//
// 3. 接收事件
//    agent-ui:  EventSource("/api/events?id=xxx") → 解析 "chat_event" event.data
//    code-editor: import { listen } from "@tauri-apps/api/event";
//                listen("chat:controller_event", (event) => {
//                  const payload = event.payload as ControllerEventPayload;
//                  // payload.kind 用 CHAT_EVENT_MAPPING_REVERSE[k] 还原
//                });
//
// 4. 角色切换
//    agent-ui:  POST /api/chat/role { session_id, role_id } → 202
//    code-editor: 在 spawn 时指定 roles: ["role1", "role2", ...]，
//                Controller 自动管理角色轮换。
//    如需动态切换，abort 后重新 spawn。
//
// 5. 事件类型转换
//    agent-ui SSE event.data.type (PascalCase) ↔ code-editor event.payload.kind (camelCase)
//    使用 CHAT_EVENT_MAPPING / CHAT_EVENT_MAPPING_REVERSE。
//
// 6. 暂停/恢复/中止
//    agent-ui:  POST /api/chat/{pause,resume,abort} { session_id }
//    code-editor: invoke("chat_controller_{pause,resume,abort}", { sessionId })
//
// 7. 任务看板 / 工具管理 / 模型 CRUD / 工作流 CRUD
//    agent-ui 有完整面板，code-editor 暂未实现这些管理界面。
//    端口时需新增 Tauri command（chat_list_tasks / chat_save_role_toml 等）。
//
// 8. HIL 会话
//    code-editor 独有功能（chat_hil_* 9 个命令）。agent-ui 没有"可修改聊天内容继续"语义。
//
// 9. 自适应
//    agent-ui 的 hot-path 端点已通过 `latte-agent-ui-server/src/api_reference_integrity.rs`
//    自动校验：所有 lib.rs 路由必须在 docs/api-reference.md 章节里覆盖。
//    增减路由时同步该测试 + bridge-api 分组。
export {};
