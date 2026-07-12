// ============================================================================
// latte-agent-ui ↔ latte-code-editor API 桥接映射
// ============================================================================
//
// 本文件记录了两个代码库之间的 API 对应关系：
//
//   latte-agent-ui (HTTP/SSE)          → HTTP REST + Server-Sent Events
//     └─ latte-rs-agents/latte-agent-cli/ui/src/api.ts
//     └─ latte-rs-agents/latte-agent-cli/src/commands/ui.rs
//
//   latte-code-editor (Tauri invoke)   → Tauri IPC invoke + event listen
//     └─ latte-code-editor/src/api/chat.ts
//     └─ latte-code-editor/src-tauri/src/chat_panel/commands.rs
//
// 当需要将 latte-agent-ui 的功能移植到 latte-code-editor 中时，用此文件
// 查找对应的 Tauri 命令/事件。
//
// 版本: 1.0
// 日期: 2026-07-12
// ============================================================================

// ─── 类型映射 ────────────────────────────────────────────────────────────

/**
 * latte-agent-ui: 通过 HTTP GET /api/sessions 返回的会话摘要。
 * latte-code-editor: chat_session_list (invoke → SessionSummary[])
 */
export interface BridgeSessionSummary {
  // agent-ui 字段                                      // code-editor 等效字段
  session_id: string;                                  //  sessionId
  preview: string;                                     //  (从 messages 推断)
  initial_role: string;                                //  roleIds[0]
  created_at_unix_ms: number;                          //  createdAt
  last_activity_unix_ms: number;                       //  updatedAt
}

/**
 * latte-agent-ui: GET /api/roles 返回的角色信息。
 * latte-code-editor: chat_list_roles → RoleInfo[]
 *
 * agent-ui 返回较简（仅有 id/name/icon），
 * code-editor 返回更多字段 (category/modelChain/defaultModelTier)。
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
 * latte-agent-ui: GET /api/session?id=... 返回的当前会话信息。
 * latte-code-editor: 无直接等效 — 由 Controller 内部管理。
 */
export interface BridgeSessionInfo {
  session_id: string;
  role: string;
  model: string | null;
  tier: string;
  available_sessions: BridgeSessionSummary[];
  available_roles: BridgeRoleInfo[];
}

// ─── API 端点映射表 ───────────────────────────────────────────────────────
//
// 以下是完整的端点级映射。每个条目包含:
//   agent-ui  → HTTP 方法/路径           (来自 api.ts)
//   code-editor → Tauri invoke 命令        (来自 chat.ts + commands.rs)
//   说明         → 语义差异 / 注意事项


/**
 * ┌─────────────────────────────────────────────────────────────────────────┐
 * │ 1. 会话管理 (Session Management)                                        │
 * └─────────────────────────────────────────────────────────────────────────┘
 */

/**
 * agent-ui:  GET  /api/sessions
 *   返回所有活跃会话的摘要列表（无详细消息）。
 *   → listSessions() → SessionSummary[]
 *
 * code-editor:  invoke("chat_session_list") → SessionSummary[]
 *   返回 ~/.latte/chat-sessions/*.json 中持久化的会话摘要。
 *
 * 映射关系：
 *   agent-ui.session_id      ↔ code-editor.SessionSummary.sessionId
 *   agent-ui.preview         ↔ (从 messages[0] 推断)
 *   agent-ui.initial_role    ↔ code-editor.SessionSummary.roleIds[0]
 *   agent-ui.created_at_unix_ms ↔ code-editor.SessionSummary.createdAt
 *   agent-ui.last_activity_unix_ms ↔ code-editor.SessionSummary.updatedAt
 */
export function mapSessionList() {
  // agent-ui:   GET /api/sessions          → fetch
  // code-editor: invoke("chat_session_list") → Tauri invoke
}

/**
 * agent-ui:  POST /api/sessions (body: {})
 *   创建一个新会话，返回 SessionInfo（含 session_id）。
 *   → createSession() → string (session_id)
 *
 * code-editor:  invoke("chat_controller_spawn", { request: ControllerSpawnRequest })
 *   spawn 一个 ChatController 会话。code-editor 由调用方自行生成 sessionId，
 *   而非从服务端获取。需要同时提供 roles/initialPrompt 等完整参数。
 *
 * 对应关系:
 *   agent-ui "POST /api/sessions"   → 分配 session_id + 单角色 ChatController
 *   code-editor "chat_controller_spawn" → 分配 sessionId + roles + 完整配置
 */
export function mapCreateSession() {
  // agent-ui:   POST /api/sessions  (body: {}) → { session_id, role, ... }
  // code-editor: invoke("chat_controller_spawn", { request: {
  //               sessionId: string,
  //               roles: string[],
  //               initialPrompt?: string,
  //               maxRounds?: number,
  //               ...
  //             }})
}

/**
 * agent-ui:  GET /api/session?id=<session_id>
 *   获取指定会话的详细信息（含可用角色列表）。
 *   → getSession() → SessionInfo
 *
 * code-editor:  invoke("chat_session_get", { sessionId }) → StoredSession
 *   返回持久化会话的完整记录，包含消息列表。
 *   对于活跃的 Controller 会话，可通过 "chat_controller_spawn" 时记录的
 *   sessionId 追踪。
 */
export function mapGetSession() {
  // agent-ui:   GET /api/session?id=xxx → { session_id, role, ... }
  // code-editor: invoke("chat_session_get", { sessionId }) → { messages, ... }
}

/**
 * agent-ui:  不单独暴露删除接口（会话在进程内存中，进程退出即释放）
 *
 * code-editor:  invoke("chat_session_delete", { sessionId })
 *   删除持久化的会话 JSON。
 */
export function mapDeleteSession() {
  // agent-ui:   (none — 无持久化存储)
  // code-editor: invoke("chat_session_delete", { sessionId })
}

/**
 * agent-ui:  不暴露编辑已发送消息的接口
 *
 * code-editor:  invoke("chat_session_edit_message", { sessionId, index, newContent })
 *   允许编辑历史消息（仅供 assistant/user 类型）。
 */
export function mapEditSessionMessage() {
  // agent-ui:   (none)
  // code-editor: invoke("chat_session_edit_message", {
  //               sessionId, index, newContent
  //             })
}


/**
 * ┌─────────────────────────────────────────────────────────────────────────┐
 * │ 2. 角色和模型配置 (Role & Model Configuration)                           │
 * └─────────────────────────────────────────────────────────────────────────┘
 */

/**
 * agent-ui:  GET  /api/roles
 *   返回所有可用角色列表（简略：id/name/icon）。
 *   → getRoles() → RoleInfo[]
 *
 * code-editor:  invoke("chat_list_roles") → RoleInfo[]
 *   返回更丰富的角色信息（含 modelChain、defaultModelTier、category）。
 *   数据源：~/.latte-code-editor/roles.yaml
 *
 * code-editor:  invoke("chat_get_role_config") → RoleConfigResponse
 *   返回完整的角色配置，包含 defaultModel、workflows、roles。
 */
export function mapListRoles() {
  // agent-ui:   GET /api/roles → [{ id, name, icon }]
  // code-editor: invoke("chat_list_roles") → [{ id, name, icon, category, modelChain, ... }]
  // code-editor: invoke("chat_get_role_config") → { defaultModel, roles: [], workflows: [] }
}

/**
 * agent-ui:  (无直接等效 — 模型列表通过角色隐式暴露)
 *
 * code-editor:  invoke("chat_list_models") → ModelInfo[]
 *   返回 ~/.latte/models.yaml 中所有可用模型的详细列表。
 *   ModelInfo 包含: id, name, provider, maxTokens, contextWindow, supportsVision, supportsThinking
 */
export function mapListModels() {
  // agent-ui:   (none)
  // code-editor: invoke("chat_list_models") → [{ id, name, provider, maxTokens, ... }]
}

/**
 * agent-ui:  (无直接等效 — 模型选择在启动时由 --model 标志决定)
 *
 * code-editor:  invoke("chat_set_role_model", { roleId, modelId })
 *   为指定角色设置模型。实际委托给 chat_set_role_model_chain([modelId])。
 *
 * code-editor:  invoke("chat_set_role_model_chain", { roleId, chain })
 *   为指定角色设置优先级排序的模型链。chain[0] 为主模型，后续为回退。
 *
 * code-editor:  invoke("chat_set_default_model", { modelId })
 *   设置全局默认模型。
 */
export function mapSetModel() {
  // agent-ui:   (none — 模型在服务启动时固定)
  // code-editor: invoke("chat_set_role_model", { roleId, modelId })
  // code-editor: invoke("chat_set_role_model_chain", { roleId, chain: string[] })
  // code-editor: invoke("chat_set_default_model", { modelId })
}

/**
 * agent-ui:  (无直接等效)
 *
 * code-editor:  invoke("chat_open_config", { configType })
 *   在系统编辑器中打开配置文件。
 *   configType 可以是: "models" | "roles" | "workflow:<id>" | "role:<id>"
 *   返回文件路径字符串。
 */
export function mapOpenConfig() {
  // agent-ui:   (none — 编辑器在浏览器中)
  // code-editor: invoke("chat_open_config", { configType: "models" | "roles" | "workflow:<id>" | "role:<id>" })
}


/**
 * ┌─────────────────────────────────────────────────────────────────────────┐
 * │ 3. 聊天 (Chat — 单角色/多角色)                                          │
 * └─────────────────────────────────────────────────────────────────────────┘
 */

/**
 * agent-ui:  POST /api/chat/send (body: { session_id, message })
 *   向当前角色发送一条消息。控制器消耗消息并生成 ChatEvent 流。
 *   → sendMessage(message) → void (202 ACCEPTED)
 *
 * code-editor:  invoke("chat_controller_submit", { sessionId, text })
 *   为 Controller 驱动的会话提交用户文本输入。等效于 agent-ui 的 sendMessage。
 *
 * 注意：
 *   - agent-ui 接受后立即返回 202，事件通过 SSE 推送。
 *   - code-editor 同样异步，事件通过 Tauri "chat:controller_event" 事件推送。
 */
export function mapSendMessage() {
  // agent-ui:   POST /api/chat/send  { session_id, message } → 202
  // code-editor: invoke("chat_controller_submit", { sessionId, text })
}

/**
 * agent-ui:  POST /api/chat/command (body: { session_id, command })
 *   向控制器发送特殊命令（如 @role 切换、系统指令）。
 *   → sendCommand(command) → void
 *
 * code-editor:  (无独立命令端点 — 所有输入统一走 chat_controller_submit)
 *   agent-ui 的 POST /api/chat/command 本质与 send 相同
 *   （调用 controller.submit_input），区别仅在于前端标签。端口时
 *   可将 command 视为普通文本输入提交。
 */
export function mapSendCommand() {
  // agent-ui:   POST /api/chat/command  { session_id, command } → 202
  // code-editor: invoke("chat_controller_submit", { sessionId, text }) — 统一处理
}

/**
 * agent-ui:  POST /api/chat/role (body: { session_id, role_id })
 *   切换当前会话的活动角色。控制器调用 switch_role 让新角色成为发言人。
 *   → switchRole(role_id) → void
 *
 * code-editor:  (切换角色由 Controller 在 spawn 时通过 roles 数组固定，
 *   或通过后续 submitInput 的上下文隐式切换。无独立切换命令。)
 *
 * 移植要点：
 *   如需在 code-editor 中切换角色，可终止当前 Controller 并重新 spawn
 *   新角色集合，或在 spawn 时指定所有需要的角色，让 Controller 自动轮换。
 */
export function mapSwitchRole() {
  // agent-ui:   POST /api/chat/role  { session_id, role_id } → 202
  // code-editor: (通过 ControllerSpawnRequest.roles 预配置)
}

/**
 * agent-ui:  POST /api/chat/send + 控制器自动流转
 *   单角色模式下，agent-ui 使用单一角色回复。如需开始完整的多角色讨论
 *   （含工作流），需要手动管理。
 *
 * code-editor:  invoke("chat_start_discussion", { request: StartDiscussionRequest })
 *   StartDiscussionRequest 包含 topic, workflow, customRoles, maxRounds。
 *
 * code-editor:  invoke("chat_continue", { request: ContinueDiscussionRequest })
 *   为已存在的讨论追加后续消息。
 *
 * code-editor:  invoke("chat_cancel", { sessionId })
 *   取消进行中的讨论。
 *
 * code-editor:  invoke("chat_cancel_workspace", { workspaceId })
 *   按工作区取消讨论。
 */
export function mapDiscussion() {
  // agent-ui:   (通过 sendMessage + SSE 手动管理多角色会话)
  // code-editor: invoke("chat_start_discussion", { request: { topic, workflow, ... } })
  // code-editor: invoke("chat_continue", { request: { sessionId, message } })
  // code-editor: invoke("chat_cancel", { sessionId })
  // code-editor: invoke("chat_cancel_workspace", { workspaceId })
}


/**
 * ┌─────────────────────────────────────────────────────────────────────────┐
 * │ 4. 控制器会话 (ChatController — 事件驱动多角色)                          │
 * └─────────────────────────────────────────────────────────────────────────┘
 *
 * latte-code-editor 的核心新增能力。Controller 是一个事件驱动的多角色会话
 * 管理器。每个会话通过 client-chosen sessionId 标识，事件通过 Tauri 事件
 * "chat:controller_event" 推送。
 */

/**
 * code-editor:  invoke("chat_controller_spawn", { request: ControllerSpawnRequest })
 *   创建一个新的 ChatController 会话。
 *   ControllerSpawnRequest: { sessionId, taskId?, roles[], initialPrompt?,
 *                             maxRounds?, sessionTokenBudget?, primaryModelId?, ... }
 *
 * code-editor:  invoke("chat_controller_submit", { sessionId, text })
 *   向 Controller 提交用户输入。
 *
 * code-editor:  invoke("chat_controller_pause", { sessionId })
 *   暂停进行中的 Controller 会话。
 *
 * code-editor:  invoke("chat_controller_resume", { sessionId })
 *   恢复已暂停的 Controller 会话。
 *
 * code-editor:  invoke("chat_controller_abort", { sessionId })
 *   中止 Controller 会话。
 *
 * 映射关系:
 *   agent-ui "POST /api/chat/send"        → chat_controller_submit
 *   agent-ui SSE "/api/events"            → Tauri "chat:controller_event"
 *   agent-ui 无暂停/恢复/中止             → chat_controller_{pause,resume,abort}
 */
export function mapController() {
  // agent-ui:   通过 POST /api/chat/send + SSE 订阅实现相近行为
  // code-editor: 5 个 invoke 命令 + "chat:controller_event" 事件
}

/**
 * ┌─────────────────────────────────────────────────────────────────────────┐
 * │ 5. 单角色流式聊天 (Single-role Stream Chat)                              │
 * └─────────────────────────────────────────────────────────────────────────┘
 */

/**
 * agent-ui:  POST /api/chat/send → SSE 接收 ChatEvent (RoleTurn)
 *   单角色模式下，agent-ui 发送消息后通过 SSE 接收流式回复。
 *
 * code-editor:  invoke("chat_stream", { request: StreamRequest }) → StreamReply
 *   一个 invoke 调用即完成单次往返。返回完整回复内容。
 *   StreamRequest: { roleId, content, sessionId?, tier?, primaryModelId?, history? }
 *   StreamReply:   { role_id, session_id, model_id, tier, content }
 *
 * 移植要点：
 *   agent-ui 的流式体验通过 SSE 分片推送 RoleTurn（is_complete 区分中间/最终）。
 *   code-editor 的 chat_stream 是一次性同步返回，适合简单的单轮对话。
 *   如需流式体验，应改用 Controller 模式 + "chat:controller_event"。
 */
export function mapStreamChat() {
  // agent-ui:   POST /api/chat/send + SSE streaming (ChatEvent.RoleTurn)
  // code-editor: invoke("chat_stream", { request }) → StreamReply
}


/**
 * ┌─────────────────────────────────────────────────────────────────────────┐
 * │ 6. 工作流管理 (Workflow Management)                                     │
 * └─────────────────────────────────────────────────────────────────────────┘
 */

/**
 * agent-ui:  (无工作流UI — 角色在 server 启动时固定)
 *
 * code-editor:  invoke("chat_list_workflows") → WorkflowInfo[]
 *   返回摘要信息（含 kind: planned|swarm|manager_led）。
 *
 * code-editor:  invoke("chat_list_workflows_full") → WorkflowPayload[]
 *   返回完整工作流负载（含每个 step 的详情）。
 *
 * code-editor:  invoke("chat_get_workflow_full", { id }) → WorkflowPayload
 *   获取单个工作流的完整配置。
 *
 * code-editor:  invoke("chat_save_workflow", { payload }) → WorkflowMutationResult
 *   保存/更新工作流。
 *
 * code-editor:  invoke("chat_delete_workflow", { id }) → WorkflowMutationResult
 *   删除工作流（内置预设不可删除）。
 */
export function mapWorkflow() {
  // agent-ui:   (none)
  // code-editor: invoke("chat_list_workflows")
  // code-editor: invoke("chat_list_workflows_full")
  // code-editor: invoke("chat_get_workflow_full", { id })
  // code-editor: invoke("chat_save_workflow", { payload })
  // code-editor: invoke("chat_delete_workflow", { id })
  // code-editor: invoke("chat_reset_roles_to_defaults")
}

/**
 * ┌─────────────────────────────────────────────────────────────────────────┐
 * │ 7. 追踪和调试 (Traces & Debugging)                                      │
 * └─────────────────────────────────────────────────────────────────────────┘
 */

/**
 * agent-ui:  GET  /api/traces
 *   列出 ~/.latte/traces/ 下所有 .jsonl 追踪文件。
 *   → listTraces() → TraceSummary[]
 *
 * agent-ui:  GET  /api/traces/:session_id
 *   读取指定追踪文件的内容（JSON 事件数组）。
 *   → readTrace(session_id) → { session_id, events: unknown[] }
 *
 * code-editor:  (目前无等效的独立命令。追踪数据通过 chat_session_get 暴露。)
 *
 * 移植要点：
 *   code-editor 的 SessionStore 提供 chat_session_list/get/delete/editMessage，
 *   与 agent-ui 的 traces 功能重叠但数据格式不同。
 *   .jsonl 追踪格式需转换为 StoredSession 格式。
 */
export function mapTraces() {
  // agent-ui:   GET /api/traces           → listTraces()
  // agent-ui:   GET /api/traces/{id}      → readTrace(session_id)
  // code-editor: invoke("chat_session_list")  — 部分等效
  // code-editor: invoke("chat_session_get", { sessionId })  — 部分等效
}

/**
 * agent-ui:  GET  /api/subsessions?id=<sub_id>
 *   获取委派调用的完整子会话事件日志。
 *   → fetchSubsession(subId) → unknown[]
 *
 * code-editor:  (无等效命令 — subsession 是 agent-ui 特定的 HTTP 端点)
 */
export function mapSubsession() {
  // agent-ui:   GET /api/subsessions?id=xxx → events[]
  // code-editor: (none — DelegateStarted/Finished 事件携带摘要信息)
}


/**
 * ┌─────────────────────────────────────────────────────────────────────────┐
 * │ 8. SSH 事件流 (SSE — Server-Sent Events)                               │
 * └─────────────────────────────────────────────────────────────────────────┘
 */

/**
 * agent-ui:  GET  /api/events?id=<session_id>  (SSE)
 *   订阅 ChatEvent 流。服务端推送 "chat_event" 命名事件。
 *   → subscribeEvents(onEvent, onConnectionStatus) → { disconnect, reconnect }
 *
 * code-editor:  @tauri-apps/api/event.listen("chat:controller_event", callback)
 *   通过 Tauri 事件系统订阅控制器事件。
 *   ControllerEventPayload: { sessionId, kind: ControllerEventKind, ... }
 *
 * 事件映射细节见下方 ChatEvent 映射表。
 */
export function mapEventsSSE() {
  // agent-ui:   EventSource("/api/events?id=xxx") → "chat_event" events
  // code-editor: listen("chat:controller_event", callback)
}

/**
 * agent-ui:  GET  /api/self-loop/events  (SSE)
 *   订阅 Self-Loop 进度事件。推送 "self_loop_event" 命名事件。
 *   → subscribeSelfLoop(onEvent) → unsubscribe function
 *
 * code-editor:  (无等效功能 — Self-Loop 是 agent-ui 特有的 AI 测试功能)
 */
export function mapSelfLoopEvents() {
  // agent-ui:   EventSource("/api/self-loop/events") → "self_loop_event" events
  // code-editor: (none)
}

/**
 * ┌─────────────────────────────────────────────────────────────────────────┐
 * │ 9. Self-Loop 自主调试                                                   │
 * └─────────────────────────────────────────────────────────────────────────┘
 */

/**
 * agent-ui:  POST /api/self-loop/start (body: { task, max_iterations })
 *   启动 AI 自主调试循环。
 *   → startSelfLoop(task, max_iterations) → void
 *
 * agent-ui:  POST /api/self-loop/stop
 *   停止正在进行的 self-loop。
 *   → stopSelfLoop() → void
 *
 * agent-ui:  GET  /api/self-loop/events (SSE)
 *   订阅 self-loop 进度（详见 mapSelfLoopEvents）。
 *
 * code-editor:  (无等效功能 — Self-Loop 是 agent-ui 特有的功能，
 *   在端口时可根据需要实现为独立的 Tauri 命令。)
 */
export function mapSelfLoop() {
  // agent-ui:   POST /api/self-loop/start  → startSelfLoop
  // agent-ui:   POST /api/self-loop/stop   → stopSelfLoop
  // agent-ui:   GET  /api/self-loop/events → subscribeSelfLoop (SSE)
  // code-editor: (none)
}


/**
 * ┌─────────────────────────────────────────────────────────────────────────┐
 * │ 10. 角色图谱 (Role Graph)                                               │
 * └─────────────────────────────────────────────────────────────────────────┘
 */

/**
 * agent-ui:  GET  /api/role-graph
 *   获取角色 × 工具代码关系图谱。
 *   → fetchRoleGraph() → RoleGraph
 *
 * code-editor:  (通过 latte-rs-graph 的 build_code_graph 命令提供类似功能，
 *   但接口不同。code-editor 的图谱绑定到当前 workspace。)
 *
 * 移植要点：
 *   agent-ui 的 /api/role-graph 在服务器启动时基于当前项目生成。
 *   code-editor 的 build_code_graph 在 Tauri 命令中调用，参数更复杂。
 */
export function mapRoleGraph() {
  // agent-ui:   GET /api/role-graph → RoleGraph
  // code-editor: invoke("build_code_graph", ...) — 不同的命令签名
}


/**
 * ┌─────────────────────────────────────────────────────────────────────────┐
 * │ 11. HIL (Human-In-Loop) 可编辑会话                                      │
 * └─────────────────────────────────────────────────────────────────────────┘
 */

/**
 * agent-ui:  (无等效功能 — agent-ui 的会话不可编辑)
 *
 * code-editor:  HIL 会话是可暂停/编辑/继续的持久化会话，用于"可修改聊天内容继续"
 *   场景。会话状态存储在 .latte/sessions/<id>.json。
 *
 * 相关命令:
 *   chat_hil_start       → 创建新 HIL 会话，返回初始快照
 *   chat_hil_send        → 追加消息
 *   chat_hil_edit_message → 编辑或删除历史消息（暂停时可修改）
 *   chat_hil_inject      → 从外部向指定角色注入消息
 *   chat_hil_continue    → 恢复 + 发送（空 content 仅恢复）
 *   chat_hil_transition  → 暂停/恢复/中止状态转换
 *   chat_hil_get_state   → 获取会话完整状态
 *   chat_hil_list_sessions → 列出所有磁盘上的 HIL 会话
 *
 * 这是 code-editor 相对于 agent-ui 的重要增强。agent-ui 的 sendMessage/SSE
 * 模式不可编辑；端口时如需编辑能力，应使用 HIL。
 */
export function mapHIL() {
  // agent-ui:   (none — 发送后不可修改)
  // code-editor: 9 个 chat_hil_* invoke 命令
}


/**
 * ┌─────────────────────────────────────────────────────────────────────────┐
 * │ 12. 管理者工作流 (Manager-led Workflow)                                 │
 * └─────────────────────────────────────────────────────────────────────────┘
 */

/**
 * agent-ui:  (无等效功能 — 所有会话由单一 ChatController 管理)
 *
 * code-editor:  Manager 模式使用一个管理者角色来协调多个子角色。
 *
 * 相关命令:
 *   chat_start_manager_session → 启动管理者会话，返回 sessionId（number）
 *   chat_submit_user_decision  → 提交用户在决策面板中的选择
 *   chat_submit_user_continue  → 推进一步（可选附带自由文本消息）
 *
 * 注意: 以上命令在 chat.ts 中有前端绑定，但 Tauri command 注册中未找到
 * 对应的 Rust #[tauri::command] 实现，可能尚未完成或已移除。
 */
export function mapManagerSession() {
  // agent-ui:   (none)
  // code-editor: invoke("chat_start_manager_session", { topic, workflow }) → number
  // code-editor: invoke("chat_submit_user_decision", { request })
  // code-editor: invoke("chat_submit_user_continue", { sessionId, message })
}


/**
 * ┌─────────────────────────────────────────────────────────────────────────┐
 * │ 13. Swarm 模式 (Planner-driven Multi-agent)                             │
 * └─────────────────────────────────────────────────────────────────────────┘
 */

/**
 * agent-ui:  (无等效功能)
 *
 * code-editor:  Swarm 模式让规划者(planner)角色将任务分解为步骤，
 *   分派给工人(worker)角色执行，最终汇总结果。
 *
 * 相关命令:
 *   chat_start_swarm → 启动 swarm，返回 sessionId（number）
 *   进度通过 Tauri "chat:swarm_event" 事件推送。
 *   SwarmEvent: { kind: "plan" | "step" | "summary" | "file" | "complete" | "error", ... }
 *
 * 注意: chat_start_swarm 在 chat.ts 中有前端绑定，但 Tauri command 注册
 * 中未找到对应的 Rust 实现。
 */
export function mapSwarmSession() {
  // agent-ui:   (none)
  // code-editor: invoke("chat_start_swarm", { request }) → number
  //              + Tauri event "chat:swarm_event"
}


// ─── ChatEvent 类型映射 (双向) ─────────────────────────────────────────────
//
// latte-agent-ui 使用 SSE 推送 "chat_event" 事件，载荷为 internally-tagged
// JSON（{ type: "VariantName", field1: value1, ... }）。
//
// latte-code-editor 使用 Tauri "chat:controller_event" 事件，载荷为
// ControllerEventPayload（{ sessionId, kind: "camelCaseKind", ... }）。
//
// 以下是完整的事件类型映射。
// agent-ui 的 ChatEvent 更多（如 RoleStarted/DelegateStarted/DelegateFinished/
// ToolError），而 code-editor 的 ControllerEventPayload 用 ControllerEventKind
// 枚举了主要事件类别。

/**
 * ChatEvent 双向映射表。
 *
 * latte-agent-ui ChatEvent (SSE "chat_event" 事件)
 *   类型: type 字段（PascalCase 变体名）
 *   来源: latte-rs-agents/latte-agent-cli/ui/src/api.ts:40-68
 *   后端: latte-rs-agents/latte-agent-cli/src/commands/ui.rs
 *         chat_event_to_frontend_json() 转换函数
 *
 * latte-code-editor ControllerEventPayload (Tauri "chat:controller_event" 事件)
 *   类型: kind 字段（camelCase 枚举值）
 *   来源: latte-code-editor/src/api/chat.ts:632-638
 */
export const CHAT_EVENT_MAPPING = {
  // ────────── agent-ui 变体 ────────────  →  ───── code-editor kind  ─────
  //
  // 角色轮次回复（流式内容）
  "RoleTurn":        "roleTurn",         // 两者都有，字段结构一致
  //
  // 状态消息
  "Status":          "status",           // 都有
  //
  // 提示谁在发言
  "Prompt":          "prompt",           // 都有
  //
  // 会话暂停
  "Paused":          "paused",           // 都有
  //
  // 会话恢复
  "Resumed":         "resumed",          // 都有
  //
  // 轮次开始
  "RoundStarted":    "roundStarted",     // 都有
  //
  // 轮次结束
  "RoundEnded":      "roundEnded",       // 都有
  //
  // 会话完成
  "Done":            "done",             // 都有
  //
  // 错误
  "Error":           "error",            // 都有
  //
  // 角色列表
  "RoleList":        "roleList",         // 都有
  //
  // 上下文清除
  "ContextCleared":  "contextCleared",   // 都有
  //
  // 会话信息
  "SessionInfo":     "sessionInfo",      // 都有
  //
  // 工具调用
  "ToolUse":         "toolUse",          // agent-ui: tool_name + args
  //                                        code-editor: 同字段
  //
  // 工具结果
  "ToolResult":      "toolResult",       // agent-ui: tool_name + result
  //                                        code-editor: 同字段
  //
  // ⚠ 以下变体仅存在于 agent-ui（无 code-editor 等效）
  //
  "RoleStarted":     null,               // 角色开始发言
  "RoleFinished":    null,               // 角色结束发言
  "DelegateStarted": null,               // 委派开始 (from_role, to_role, task, sub_id)
  "DelegateFinished": null,              // 委派结束 (from_role, to_role, status, summary, sub_id)
  "ToolError":       null,               // 工具错误
  "SelfLoopEvent":   null,               // Self-loop 进度事件（独立 SSE）
};

/**
 * 反向映射: code-editor kind → agent-ui 变体名
 */
export const CHAT_EVENT_MAPPING_REVERSE: Record<string, string | null> = {
  "roleTurn":        "RoleTurn",
  "status":          "Status",
  "prompt":          "Prompt",
  "paused":          "Paused",
  "resumed":         "Resumed",
  "roundStarted":    "RoundStarted",
  "roundEnded":      "RoundEnded",
  "done":            "Done",
  "error":           "Error",
  "roleList":        "RoleList",
  "contextCleared":  "ContextCleared",
  "sessionInfo":     "SessionInfo",
  "toolUse":         "ToolUse",
  "toolResult":      "ToolResult",
};

/**
 * 字段名映射:
 * agent-ui 使用 snake_case（如 role_id, is_complete, session_id），
 * code-editor 的 ControllerEventPayload 使用原样字段名（snake_case 通过 serde 保持）。
 * 实际上 code-editor 的 ChatEvent 联合类型也使用 snake_case 字段。
 *
 * 因此，字段名在跨端口时基本无需转换。
 *
 * 唯一差异: code-editor 的 ChatEvent 在 RoleTurn 中省略了 role_id 字段名差异，
 * 两者一致。agent-ui ChatEvent 中多出的字段（detail, sub_id 等）在端口时
 * 可忽略或通过额外字段携带。
 */
export const FIELD_MAPPING_SNAKE_CASE = {
  // agent-ui 字段  ↔  code-editor 字段 (两者一致)
  // role_id, content, is_complete, message, icon, model_id, reason,
  // round, task_id, state, turn, tool_name, args, result, error,
  // from_role, to_role, status, summary, sub_id, detail
  //
  // ⚠ agent-ui 额外字段: detail (RoleStarted/RoleFinished 中携带)
  // ⚠ code-editor 额外字段: (ControllerEventPayload 通过 [key: string]: unknown 扩展)
};


// ─── 完整 API 端点汇总 ─────────────────────────────────────────────────────
//
// 以下表格以 Markdown 格式汇总了所有端点和映射关系。
// 在 IDE 中可折叠查看。

/**
 * ┌─────────────────────────────────────────────────────────────────────────┐
 * │ latte-agent-ui HTTP/SSE              │ latte-code-editor Tauri invoke   │
 * ├──────────────────────────────────────┼─────────────────────────────────┤
 * │ GET  /api/sessions                   │ invoke("chat_session_list")      │
 * │ POST /api/sessions                   │ invoke("chat_controller_spawn")  │
 * │ GET  /api/session?id=xxx             │ invoke("chat_session_get")       │
 * │                                      │ invoke("chat_session_delete")    │
 * │                                      │ invoke("chat_session_edit_msg")  │
 * ├──────────────────────────────────────┼─────────────────────────────────┤
 * │ GET  /api/roles                      │ invoke("chat_list_roles")        │
 * │                                      │ invoke("chat_get_role_config")   │
 * │                                      │ invoke("chat_list_models")       │
 * │                                      │ invoke("chat_set_role_model")    │
 * │                                      │ invoke("chat_set_role_model_chain")│
 * │                                      │ invoke("chat_set_default_model") │
 * │                                      │ invoke("chat_open_config")       │
 * ├──────────────────────────────────────┼─────────────────────────────────┤
 * │ POST /api/chat/send                  │ invoke("chat_controller_submit") │
 * │ POST /api/chat/command               │ invoke("chat_controller_submit") │
 * │ POST /api/chat/role                  │ (通过 spawn roles 预配置)        │
 * │                                      │ invoke("chat_controller_spawn")  │
 * │                                      │ invoke("chat_controller_pause")  │
 * │                                      │ invoke("chat_controller_resume") │
 * │                                      │ invoke("chat_controller_abort")  │
 * │                                      │ invoke("chat_start_discussion")  │
 * │                                      │ invoke("chat_continue")          │
 * │                                      │ invoke("chat_cancel")            │
 * │                                      │ invoke("chat_cancel_workspace")  │
 * ├──────────────────────────────────────┼─────────────────────────────────┤
 * │                                      │ invoke("chat_stream")            │
 * ├──────────────────────────────────────┼─────────────────────────────────┤
 * │                                      │ invoke("chat_list_workflows")    │
 * │                                      │ invoke("chat_list_workflows_full")│
 * │                                      │ invoke("chat_get_workflow_full") │
 * │                                      │ invoke("chat_save_workflow")     │
 * │                                      │ invoke("chat_delete_workflow")   │
 * │                                      │ invoke("chat_reset_roles")       │
 * ├──────────────────────────────────────┼─────────────────────────────────┤
 * │ GET  /api/traces                     │ (partially: chat_session_list)   │
 * │ GET  /api/traces/{session_id}        │ (partially: chat_session_get)    │
 * ├──────────────────────────────────────┼─────────────────────────────────┤
 * │ GET  /api/subsessions?id=xxx         │ (none)                          │
 * ├──────────────────────────────────────┼─────────────────────────────────┤
 * │ GET  /api/events?id=xxx (SSE)        │ listen("chat:controller_event")  │
 * │ GET  /api/self-loop/events (SSE)     │ (none)                          │
 * ├──────────────────────────────────────┼─────────────────────────────────┤
 * │ POST /api/self-loop/start            │ (none)                          │
 * │ POST /api/self-loop/stop             │ (none)                          │
 * ├──────────────────────────────────────┼─────────────────────────────────┤
 * │ GET  /api/role-graph                 │ invoke("build_code_graph")       │
 * ├──────────────────────────────────────┼─────────────────────────────────┤
 * │                                      │ 9 × chat_hil_* commands         │
 * │                                      │ invoke("chat_start_manager_session") │
 * │                                      │ invoke("chat_submit_user_decision") │
 * │                                      │ invoke("chat_submit_user_continue") │
 * │                                      │ invoke("chat_start_swarm")       │
 * └──────────────────────────────────────┴─────────────────────────────────┘
 *
 * 总计:
 *   latte-agent-ui: 14 个 HTTP/SSE 端点
 *   latte-code-editor: 30+ 个 Tauri invoke 命令 + 多个 Tauri 事件
 */


// ─── 端口指南 ──────────────────────────────────────────────────────────────
//
// 从 agent-ui 到 code-editor 的基本端口步骤:
//
// 1. 会话创建
//    agent-ui:  POST /api/sessions → 获取 session_id
//    code-editor: invoke("chat_controller_spawn", { request: { sessionId, roles, ... } })
//    需要提前生成唯一的 sessionId（如 crypto.randomUUID()）。
//
// 2. 发送消息
//    agent-ui:  POST /api/chat/send (body: { session_id, message })
//    code-editor: invoke("chat_controller_submit", { sessionId, text })
//
// 3. 接收事件
//    agent-ui:  EventSource("/api/events?id=xxx") → 解析 "chat_event" 事件
//    code-editor: import { listen } from "@tauri-apps/api/event";
//                listen("chat:controller_event", (event) => {
//                  const payload = event.payload as ControllerEventPayload;
//                  // payload.kind 对应 agent-ui 的 e.type
//                });
//
// 4. 角色切换
//    agent-ui:  POST /api/chat/role (body: { session_id, role_id })
//    code-editor: 在 spawn 时指定 roles: ["role1", "role2", ...]，
//                Controller 自动管理角色轮换。
//    如需动态切换，可 abort 后重新 spawn。
//
// 5. 事件类型转换
//    agent-ui SSE event.data.type (PascalCase) ↔ code-editor event.payload.kind (camelCase)
//    使用 CHAT_EVENT_MAPPING / CHAT_EVENT_MAPPING_REVERSE 进行转换。
//
// 6. 会话管理
//    agent-ui 的会话在服务器内存中，进程退出即释放。
//    code-editor 提供持久化存储（~/.latte/chat-sessions/*.json），
//    可通过 chat_session_list/get/delete 管理。
//
// 7. 配置管理
//    agent-ui 在启动时从 ~/.latte/config.yaml / roles.yaml 加载配置。
//    code-editor 提供 chat_list_models/chat_get_role_config 等命令查看配置，
//    以及 chat_set_role_model/chat_save_workflow 等命令修改配置。
//
// 8. 工作流
//    agent-ui 没有工作流 UI，角色在启动时固定。
//    code-editor 有完整的工作流编辑器（CRUD + 分步编辑）。
//
// 9. HIL 会话
//    code-editor 独有功能。agent-ui 的项目若需要"可修改聊天内容继续"，
//    应使用 HIL API 而不是普通的 Controller。
export {};
