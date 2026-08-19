// REST + SSE client for latte-agent-ui. All chat endpoints now require a
// per-tab `session_id`; this module owns a module-level `currentSessionId`
// and attaches it to every request automatically. The session itself is
// persisted in localStorage so a tab refresh reattaches to the same
// chat history without a server roundtrip.
//
// All backend access goes through the active ChatTransport (contract C1,
// see transport.ts) — this module is the only facade UI code calls.

import { getHost } from "./host";
import { getTransport, HttpSseTransport, HttpError } from "./transport";
import { stageStore } from "./stages/stageStore";

/** Realm-agnostic HttpError check: the editor's TauriIpcTransport runs in
 * the host page's JS realm, so errors it throws are never `instanceof`
 * this module's HttpError class. Accept the duck-typed shape
 * (`name === "HttpError"` + numeric `status`) that transport throws for
 * HTTP-style failures (e.g. session not found → 404). Behavior in plain
 * browser/CLI mode is unchanged. */
function isHttpError(e: unknown): e is HttpError {
  return (
    e instanceof HttpError ||
    (typeof e === "object" &&
      e !== null &&
      (e as { name?: unknown }).name === "HttpError" &&
      typeof (e as { status?: unknown }).status === "number")
  );
}

export interface RoleInfo {
  id: string;
  name: string;
  icon: string;
}

export interface SessionInfo {
  session_id: string;
  role: string;
  model: string | null;
  tier: string;
  available_sessions: SessionSummary[];
  available_roles: RoleInfo[];
  /** true when the user pressed ⏸ and the session is frozen. */
  is_paused: boolean;
}


export interface SessionSummary {
  session_id: string;
  /** First user message, truncated; "(no user message yet)" before first send. */
  preview: string;
  /** User-assigned display name; falls back to preview when absent. */
  label?: string | null;
  initial_role: string;
  created_at_unix_ms: number;
  last_activity_unix_ms: number;
  restored?: boolean;
  /** true when the user pressed ⏸ and the session is frozen. */
  is_paused: boolean;
}
export interface TraceSummary {
  session_id: string;
  path: string;
  size_bytes: number;
  modified_unix: number;
}

// ─── Role editor（角色编辑器） ──────────────────────────────────────

/** `GET /api/roles/config` 返回的单个角色条目。 */
export interface RoleConfigEntry {
  id: string;
  name: string;
  category: string;
  icon: string;
  model_tier: string;
  model_chain: string[];
  temperature: number | null;
  tools: string[];
  skills: string[];
  code_paths: string[];
  prompt_file: string | null;
  prompt_path: string | null;
  config_path: string;
  prompt: string;
}

export interface RolesConfig {
  roles: RoleConfigEntry[];
  available_tools: string[];
  /** 角色编辑器的「模型链」下拉框数据源。详见后端 `AvailableModel`。 */
  available_models: AvailableModel[];
  tiers: string[];
  workspace_path: string;
  agents_config_path: string;
  sessions_path: string;
}

/** 角色编辑器「模型链」下拉框的单个选项。
 * 后端与 `GET /api/models` 同源（磁盘 entries，不去重），只取下拉框
 * 需要的字段。`source` 是 "project" / "global" / "catalog" 之一。
 * `key`（provider/name）用于区分同名多条 —— 项目层与全局层各有一份
 * 文件时，两条 `name` 相同但 `key + source` 不同。 */
export interface AvailableModel {
  /** 模型 id —— 选中后写入 `model_chain` 的字符串。 */
  name: string;
  /** 厂商标识。UI 用 `provider/name` 显示，比单 name 更易区分同名
   * 模型在不同厂商下的实例。 */
  provider: string;
  source: string;
  /** 复合键 `provider/name`。 */
  key: string;
}

/** `POST /api/roles/config` 请求体。 */
export interface RoleConfigSave {
  id: string;
  name: string;
  icon: string;
  model_tier: string;
  model_chain: string[];
  temperature: number | null;
  tools: string[];
  code_paths: string[];
  prompt: string;
}


export async function getRolesConfig(): Promise<RolesConfig> {
  return getTransport().request("GET", "/api/roles/config");
}

export async function saveRoleConfig(
  body: RoleConfigSave,
): Promise<RoleConfigEntry> {
  return getTransport().request("POST", "/api/roles/config", body);
}

/** POST /api/roles — 创建新角色。返回新建的 RoleConfigEntry。 */
export async function createRole(
  body: { id: string; name: string },
): Promise<RoleConfigEntry> {
  return getTransport().request("POST", "/api/roles", body);
}

/** DELETE /api/roles/:id — 删除角色。 */
export async function deleteRole(id: string): Promise<void> {
  await getTransport().request("DELETE", `/api/roles/${encodeURIComponent(id)}`);
}

// ChatEvent —— Rust enum ChatEvent 的 JSON 表示（discriminated union）。
// 字段命名沿用 Rust（snake_case）。
export type ChatEvent =
  | { type: "RoleTurn"; role_id: string; content: string; is_complete: boolean; sub_id?: string }
  | { type: "Status"; message: string }
  | { type: "Prompt"; icon: string; role_id: string; model_id: string }
  | { type: "Paused"; reason: string }
  | { type: "Resumed" }
  | { type: "RoundStarted"; round: number }
  | { type: "RoundEnded"; round: number }
  // sub_id 标识本次运行所属的 subsession（delegate / workflow
  // speaker）；同一 role 可被并行 workflow 步同时委派，起止事件
  // 按 (role_id, sub_id) 配对。主 session 角色的 turn 不带 sub_id。
  | { type: "RoleStarted"; role_id: string; detail: string; sub_id?: string | null }
  | { type: "RoleFinished"; role_id: string; detail: string; sub_id?: string | null }
  // 单角色被单独暂停/恢复（多角色 HIL）。区别于 Paused/Resumed
  // （整会话）。UI 据此渲染角色的「已暂停」标记与暂停/恢复切换。
  | { type: "RolePaused"; role_id: string }
  | { type: "RoleResumed"; role_id: string }
  | { type: "Done" }
  | { type: "Error"; kind?: unknown; message: string; sub_id?: string }
  | { type: "RoleList"; roles: RoleInfo[] }
  | { type: "ContextCleared" }
  | { type: "SessionInfo"; task_id: string; state: string; turn: number; roles: RoleInfo[] }
  | { type: "UserMessage"; text: string }
  | { type: "ToolUse"; role_id: string; tool_name: string; args: string }
  | { type: "ToolError"; role_id: string; tool_name: string; error: string }
  | { type: "ToolResult"; role_id: string; tool_name: string; result: string }
  | { type: "ImageGenerated"; role_id: string; path: string; prompt: string }
  // plan 工具提交的任务候选：manager 调 plan 后广播，UI 弹窗勾选导入
  // 看板。tasks 与 POST /api/tasks/import 的 ImportTask 同构。
  | { type: "PlanProposed"; role_id: string; plan_id: string; tasks: ImportTask[] }
  // ask 工具抛出的选择题：manager/角色调 ask 后广播，UI 弹出选择框。
  // wait=false（顶层 turn）：用户提交后把选择结果作为下一条 user 消息
  // （sendMessage）回喂角色；wait=true（workflow/delegate 子代理阻塞
  // 等待）：答案 POST 到 /api/chat/choice-answer 直达等待方。
  | {
      type: "ChoiceRequested";
      role_id: string;
      choice_id: string;
      question: string;
      multi?: boolean;
      layout?: string; // "grid" | "" (list)
      allow_upload?: boolean;
      wait?: boolean;
      options: ChoiceOption[];
    }
  | { type: "DelegateStarted"; from_role: string; to_role: string; task: string; sub_id: string }
  | { type: "DelegateFinished"; from_role: string; to_role: string; status: string; summary: string; sub_id: string }
  | { type: "WorkflowStarted"; name: string; topic: string; wf_id: string }
  | { type: "WorkflowStep"; wf_id: string; step_id: string; description: string; index: number; total: number; role_id: string; task: string }
  | { type: "WorkflowTurn"; wf_id: string; step_id: string; role_id: string; content: string; round: number }
  | { type: "WorkflowFinished"; name: string; wf_id: string; status: string; summary: string }
  // Turn soft-timeout warning. The driver emits this when a turn
  // has been running past its `soft_timeout_secs` but is still
  // alive — the UI shows a "继续等待 / 终止当前任务" prompt and
  // flips `turn_cancel_flag` server-side on the second choice. The
  // hard kill fires at `hard_timeout_secs` as a safety net.
  | {
      type: "TimeoutWarning";
      role_id: string;
      elapsed_secs: number;
      soft_timeout_secs: number;
      hard_timeout_secs: number;
      sub_id?: string;
    }
  | {
      type: "SelfLoopEvent";
      kind: string;
      iteration: number;
      message: string;
      screenshot?: string;
      data?: unknown;
      timestamp_unix_ms: number;
    };
export interface SelfLoopEvent {
  kind: "started" | "iteration" | "log" | "screenshot" | "done" | "error";
  iteration: number;
  message: string;
  screenshot?: string;
  data?: unknown;
  timestamp_unix_ms: number;
}

// ─── Session-id plumbing ───────────────────────────────────────────────
//
// Each browser tab mints a `session_id`, persists it in localStorage,
// and attaches it to every fetch + EventSource. Two tabs in the same
// browser are independent browsing contexts — different localStorage
// entries — but every tab talks to one logical session at the server.

// When running inside the editor host, the session is persisted under a
// host-provided per-workspace key (design doc §5.4); CLI browser mode
// keeps the original bare key.
const DEFAULT_SESSION_STORAGE_KEY = "latte-agent-ui-session-id";
function sessionStorageKey(): string {
  return getHost()?.sessionKey ?? DEFAULT_SESSION_STORAGE_KEY;
}
let currentSessionId: string | null = null;

export function getCurrentSessionId(): string | null {
  return currentSessionId;
}

export async function listSessions(): Promise<SessionSummary[]> {
  return getTransport().request("GET", "/api/sessions");
}

export async function createSession(): Promise<string> {
  const info = await getTransport().request<SessionInfo>("POST", "/api/sessions", {});
  persistSessionId(info.session_id);
  return info.session_id;
}

export function switchSession(sessionId: string): void {
  currentSessionId = sessionId;
}

/** Fork a new session from a prefix of `sourceSessionId`'s visible
 * history. `events` is the ordered ChatEvent stream up to and including
 * the message the user right-clicked. The server clones this prefix as
 * the new session's visible history and reconstructs the agent context
 * from it. Returns the new session id (also persisted to localStorage). */
export async function forkSession(
  sourceSessionId: string,
  events: ChatEvent[],
): Promise<string> {
  const info = await getTransport().request<SessionInfo>(
    "POST",
    "/api/sessions/fork",
    { source_session_id: sourceSessionId, events },
  );
  persistSessionId(info.session_id);
  return info.session_id;
}

/** Rename a session (empty label clears the custom name). */
export async function renameSession(
  sessionId: string,
  label: string,
): Promise<void> {
  await getTransport().request("POST", "/api/session/label", {
    session_id: sessionId,
    label,
  });
}

/** Delete a session server-side (its controller is aborted). */
export async function deleteSession(sessionId: string): Promise<void> {
  await getTransport().request(
    "DELETE",
    `/api/sessions?id=${encodeURIComponent(sessionId)}`,
  );
}

/** Persist the active session id both in module state and localStorage
 * so a reload reattaches to the same session. */
export function persistSessionId(sessionId: string): void {
  currentSessionId = sessionId;
  if (typeof window !== "undefined") {
    window.localStorage.setItem(sessionStorageKey(), sessionId);
  }
}

/**
 * First-mount hook: reuse a previously persisted session id if the
 * server still knows it; otherwise mint a new one and persist it.
 * Idempotent — safe to call on every reload.
 */
export async function ensureSession(): Promise<string> {
  const ls = typeof window !== "undefined" ? window.localStorage : null;
  const persisted = ls?.getItem(sessionStorageKey()) ?? null;
  if (persisted) {
    try {
      await getTransport().request(
        "GET",
        `/api/session?id=${encodeURIComponent(persisted)}`,
      );
      currentSessionId = persisted;
      return persisted;
    } catch (e) {
      // Only an HTTP error means "server forgot the session"; network
      // failures propagate like before (main shows fatal).
      if (!isHttpError(e)) throw e;
      ls?.removeItem(sessionStorageKey());
    }
  }
  const id = await createSession();
  persistSessionId(id);
  return id;
}

/** Forget the current session id locally — used when the user clicks
 * "New Session". Server-side sessions persist until reload. */
export function clearLocalSessionId(): void {
  currentSessionId = null;
  if (typeof window !== "undefined") {
    window.localStorage.removeItem(sessionStorageKey());
  }
}

// ─── REST helpers ──────────────────────────────────────────────────────

function chatBody(extra: Record<string, unknown>): Record<string, unknown> {
  // Always stamp session_id; backend uses it to route to the right
  // ChatController. If somehow null the backend falls back to a 400
  // and the caller (the chat panel) surfaces it.
  return currentSessionId
    ? { session_id: currentSessionId, ...extra }
    : extra;
}

export async function getSession(): Promise<SessionInfo> {
  if (!currentSessionId) throw new Error("no session id; call ensureSession() first");
  return getTransport().request(
    "GET",
    `/api/session?id=${encodeURIComponent(currentSessionId)}`,
  );
}

/** Fetch the archived ChatEvent log for a session — used to restore
 * the chat panel contents after switching sessions. */
export async function getSessionHistory(sessionId: string): Promise<ChatEvent[]> {
  return getTransport().request(
    "GET",
    `/api/session/history?id=${encodeURIComponent(sessionId)}`,
  );
}

export async function getRoles(): Promise<RoleInfo[]> {
  return getTransport().request("GET", "/api/roles");
}

// ─── advisor 全局开关 ────────────────────────────────────────────

export interface AdvisorState {
  /** 生效值（project 层显式声明优先于全局层文件）。 */
  enabled: boolean;
  /** project 层显式声明值；非 null 时全局开关被项目覆盖。 */
  project_override: boolean | null;
  /** 全局开关的持久化文件路径。 */
  file: string;
}

/** GET /api/advisor — advisor 全局开关状态。 */
export async function getAdvisor(): Promise<AdvisorState> {
  return getTransport().request("GET", "/api/advisor");
}

/** PUT /api/advisor — 设置 advisor 全局开关（写全局层 agents.d/advisor.toml）。 */
export async function putAdvisor(enabled: boolean): Promise<AdvisorState> {
  return getTransport().request("PUT", "/api/advisor", { enabled });
}

export async function sendMessage(message: string): Promise<void> {
  await getTransport().request("POST", "/api/chat/send", chatBody({ message }));
}

/** 阻塞中的 ask（`ChoiceRequested.wait=true`，workflow/delegate 子代理
 * 正挂起等回答）的答案直达通道：经后端 choice 路由直接交给等待方，
 * 而不是另起一轮 user 消息。返回 false = 无匹配挂起项（已答/超时/
 * 服务重启），调用方应降级为 `sendMessage` 回喂。 */
export async function sendChoiceAnswer(choiceId: string, answer: string): Promise<boolean> {
  try {
    await getTransport().request("POST", "/api/chat/choice-answer", chatBody({ choice_id: choiceId, answer }));
    return true;
  } catch (err) {
    if (err instanceof HttpError && err.status === 404) return false;
    throw err;
  }
}

export async function sendCommand(command: string): Promise<void> {
  await getTransport().request("POST", "/api/chat/command", chatBody({ command }));
}

/** Cancel only the in-flight turn (the one currently waiting on the
 *  LLM). Distinct from deleting the session: cancel preserves the
 *  session and history, just drops the current run_turn future and
 *  lets the user type a new message. Wired to the
 *  `TimeoutWarning` prompt's "终止当前任务" button. */
export async function cancelTurn(): Promise<void> {
  await getTransport().request("POST", "/api/chat/cancel-turn", chatBody({}));
}

/** Abort the entire session immediately (all in-flight turns, all
 *  subagents, all workflows). The session is torn down and cannot
 *  be resumed. Equivalent to clicking "delete session" but without
 *  removing the archived logs. Kept as a programmatic API; the chat
 *  toolbar no longer surfaces a dedicated 终止 button (pause/resume
 *  only). Still used by the delete-session flow server-side. */
export async function abortSession(): Promise<void> {
  await getTransport().request("POST", "/api/chat/abort", chatBody({}));
}

/** Pause the session. The current session + history are preserved; the
 *  next turn is held until `resumeSession()` is called. Wired to the
 *  "⏸ 暂停" button. The backend echoes a `Paused` ChatEvent. */
export async function pauseSession(): Promise<void> {
  await getTransport().request("POST", "/api/chat/pause", chatBody({}));
}

/** Resume a paused session. The backend echoes a `Resumed` ChatEvent. */
export async function resumeSession(): Promise<void> {
  await getTransport().request("POST", "/api/chat/resume", chatBody({}));
}

/** Session-level 暂停（用户按 ⏸）：全 session 冻结（turn / tool /
 *  subagent 一起停，对齐 oh-my-pi 的 agentPauseGate）。区别于
 *  `pauseSession()`（legacy flag-based，只停 driver loop 的下个
 *  turn）。后端广播 `Paused` 事件驱动 UI 按钮态。 */
export async function pauseSessionV2(): Promise<void> {
  await getTransport().request("POST", "/api/chat/pause-session", chatBody({}));
}

/** Session-level 恢复，与 `pauseSessionV2` 配对。广播 `Resumed`。 */
export async function resumeSessionV2(): Promise<void> {
  await getTransport().request("POST", "/api/chat/resume-session", chatBody({}));
}

/** Pause a single role (multi-role HIL). Orthogonal to `pauseSession()`:
 *  the paused role is skipped each round while the others keep running.
 *  The backend echoes a `RolePaused` ChatEvent. */
export async function pauseRole(role_id: string): Promise<void> {
  await getTransport().request("POST", "/api/chat/pause-role", chatBody({ role_id }));
}

/** Resume a single individually-paused role. Counterpart to `pauseRole()`.
 *  The backend echoes a `RoleResumed` ChatEvent. */
export async function resumeRole(role_id: string): Promise<void> {
  await getTransport().request("POST", "/api/chat/resume-role", chatBody({ role_id }));
}

export async function switchRole(role_id: string): Promise<void> {
  await getTransport().request("POST", "/api/chat/role", chatBody({ role_id }));
}

/** 切换 stream/non-stream 模式。`stream=true` 时模型逐 token 推送
 *  RoleTurn{is_complete:false} 增量事件；`false` 时走非流式（整段返回）。
 *  运行时切换，对当前 session 立即生效。 */
export async function setStreamMode(stream: boolean): Promise<void> {
  await getTransport().request("POST", "/api/chat/stream-mode", chatBody({ stream }));
}

export async function listTraces(): Promise<TraceSummary[]> {
  return getTransport().request("GET", "/api/traces");
}

export async function readTrace(session_id: string): Promise<{ session_id: string; events: unknown[] }> {
  return getTransport().request("GET", `/api/traces/${encodeURIComponent(session_id)}`);
}

// ─── Logs ────────────────────────────────────────────────────────────────
export interface LogFile {
  file: string;
  size: number;
  modified: number;
  lines: string[];
}

/** GET /api/logs — 列出 cwd/.latte/ui-sessions/ 下所有日志文件。 */
export async function listLogs(): Promise<LogFile[]> {
  return getTransport().request("GET", "/api/logs");
}
// (读取指定日志文件直接调用 getTransport().request，无需单独包装：避免 1 行 wrapper)

// ─── SSE ────────────────────────────────────────────────────────────────

export function subscribeEvents(
  onEvent: (e: ChatEvent) => void,
  onConnectionStatus: (status: "connected" | "disconnected") => void,
): { disconnect: () => void; reconnect: () => void } {
  let unsubscribe: (() => void) | null = null;
  let subscribed = false;

  const connect = (): void => {
    if (!currentSessionId) {
      // Caller forgot to ensureSession() — surface as "disconnected" so
      // the UI pill asks the user to reload. Better than silently
      // subscribing to the wrong tab's events.
      onConnectionStatus("disconnected");
      return;
    }
    const t = getTransport();
    if (t instanceof HttpSseTransport) {
      // Keep the status pill semantics of the old inline EventSource
      // code: "connected" on open, "disconnected" on error.
      t.onConnectionStatus = (s) => {
        if (s === "disconnected") subscribed = false;
        onConnectionStatus(s);
      };
    } else {
      // Contract C1 has no status channel; once a custom transport
      // accepted the subscription we consider it connected.
      onConnectionStatus("connected");
    }
    // conversation-stage-logs: fan the same ChatEvent stream into the
    // Stage store so <StageList/> can render the stage tree, WITHOUT
    // disturbing the existing consumer (chat.handleEvent). Guarded so a
    // reducer error can never break the legacy chat handler.
    const routedOnEvent = (ev: ChatEvent): void => {
      try {
        stageStore.getState().applyEvent(ev);
      } catch (err) {
        console.error("[stageStore] applyEvent failed", err);
      }
      onEvent(ev);
    };
    unsubscribe = t.subscribeEvents(currentSessionId, routedOnEvent);
    subscribed = true;
  };

  connect();

  return {
    disconnect: () => {
      if (unsubscribe) {
        unsubscribe();
        unsubscribe = null;
      }
      subscribed = false;
    },
    reconnect: () => {
      if (subscribed) return;
      connect();
    },
  };
}

export function subscribeSelfLoop(onEvent: (e: SelfLoopEvent) => void): () => void {
  return getTransport().subscribeSelfLoop(onEvent);
}

export async function startSelfLoop(task: string, max_iterations: number): Promise<void> {
  await getTransport().request("POST", "/api/self-loop/start", { task, max_iterations });
}

/** Fetch the subsession event log for a delegate call.
 *  @param limit 最多返回 N 条事件（默认 100）；0 表示全部（慎用，可能超大）。
 */
export async function fetchSubsession(subId: string, limit = 100): Promise<unknown[]> {
  const q = `?id=${encodeURIComponent(subId)}&limit=${limit}`;
  return getTransport().request("GET", `/api/subsessions${q}`);
}


export async function stopSelfLoop(): Promise<void> {
  await getTransport().request("POST", "/api/self-loop/stop");
}

// ─── Tools ───────────────────────────────────────────────────────────
export interface ToolEntry {
  id: string;
  kind: string;
  enabled: boolean;
  /** 工具描述，渲染时作为 tooltip。 */
  description?: string;
  /** 注册点（dynamic 工具的函数名）。 */
  registered_by?: string;
}

/** GET /api/tools 返回 `{tools: ToolEntry[]}` 形状（旧 schema 也兼容裸数组）。 */
export interface ToolsListResponse {
  tools: ToolEntry[];
}

/** GET /api/tools：列出所有可用工具 + enabled 状态。 */
export async function listTools(): Promise<ToolEntry[]> {
  const resp = await getTransport().request<ToolEntry[] | ToolsListResponse>(
    "GET",
    "/api/tools",
  );
  // 兼容两种返回形态
  return Array.isArray(resp) ? resp : resp.tools;
}

/** PATCH /api/tools/:id：切换工具启用状态。 */
export async function setToolEnabled(id: string, enabled: boolean): Promise<void> {
  await getTransport().request(
    "PATCH",
    `/api/tools/${encodeURIComponent(id)}`,
    { enabled },
  );
}

// ─── Models ──────────────────────────────────────────────────────────
//
// `ModelDef` 字段跟后端 `latte-agent-core::config::ModelDef` 一一对应（snake_case）。
// `model_name` 字段已被 core 移除（合并进 `id`）；面板渲染和编辑表单用 `id` 即可。

export interface ModelDef {
  name: string;
  api: string;
  provider: string;
  base_url: string;
  api_key: string;
  context_window: number;
  max_tokens: number;
  supports_thinking?: boolean;
  supports_vision?: boolean;
  supports_image_generation?: boolean;
  cost_per_million_input?: number | null;
  cost_per_million_output?: number | null;
  tier?: string | null;
  timeout_secs?: number | null;
}

/** `ModelWithSource` = `ModelDef` + 主键 + 来源标识 + 文件位置。
 * `source` 是 `"project"`（项目 `.latte/models.d/`）、`"global"`（全局
 * `~/.latte/models.d/`）或 `"catalog"`（只在内存，未落盘）。
 * `file_path` 是 disk 扫描拿到的实际绝对路径；空串表示 model 只在
 * 内存 catalog 里（创建后未保存）。
 */
export interface ModelWithSource extends ModelDef {
  key: string;
  source: "project" | "global" | "catalog";
  file_path: string;
}

export interface ModelsListResponse {
  models: ModelWithSource[];
  tiers: Record<string, string>;
  project_models_dir: string;
  global_models_dir: string;
}

/** GET /api/models：列出所有 model。 */
export async function listModels(): Promise<ModelsListResponse> {
  return getTransport().request("GET", "/api/models");
}
export async function createModel(def: ModelDef): Promise<ModelWithSource> {
  return getTransport().request("POST", "/api/models", def);
}

export async function updateModel(
  key: string,
  def: ModelDef,
  target: "project" | "global" = "project",
): Promise<ModelWithSource> {
  return getTransport().request(
    "PATCH",
    `/api/models/${encodeURIComponent(key)}`,
    { target, ...def },
  );
}
/** DELETE /api/models/:key?source=…：删除指定层的 model 文件。
 * 同一 key 可能项目/全局各一份（同名多条），`source` 精确指定删哪层，
 * 只删当前条目这份，另一层保留。 */
export async function deleteModel(
  key: string,
  source: "project" | "global" = "project",
): Promise<void> {
  await getTransport().request(
    "DELETE",
    `/api/models/${encodeURIComponent(key)}?source=${source}`,
  );
}

/** GET /api/models/:key/toml：读取模型 TOML 源文件原始内容。 */
export async function getModelToml(key: string): Promise<string> {
  return getTransport().requestText("GET", `/api/models/${encodeURIComponent(key)}/toml`);
}

/** PUT /api/models/:key/toml：直接写入模型 TOML 源文件原始内容。 */
export async function putModelToml(key: string, raw: string): Promise<void> {
  await getTransport().request("PUT", `/api/models/${encodeURIComponent(key)}/toml`, raw);
}

// ─── Model test (后端 test.rs) ─────────────────────────────────────
//
// 详见后端 `latte-agent-ui-server/src/test.rs`。前端只发请求 + 渲染
// 结果，逻辑全部在 Rust 侧。返回的 `TestModelResponse` 字段全可选
// —— connectivity / aiclient / http 三种模式返回的字段子集不一样
// （只有 connectivity 才有 `available_models`），所以前端按需读取。

export type TestMode = "connectivity" | "aiclient" | "http";

export interface TestModelRequest {
  def: ModelDef;
  mode: TestMode;
  prompt?: string;
  /** base64 data URL 列表（"data:image/png;base64,..."），仅图片输入模式用。 */
  images?: string[];
  probe_path?: string;
}

export interface TestModelResponse {
  ok: boolean;
  mode: string;
  latency_ms: number;
  response?: string;
  error?: string;
  status?: number;
  available_models?: string[];
}

export interface ModelCapabilities {
  supports_image_input: boolean;
  supports_image_generation: boolean;
  latency_ms: number;
}

/** POST /api/models/test —— 跑一次连通 / chat 测试。 */
export async function testModel(
  req: TestModelRequest,
): Promise<TestModelResponse> {
  return getTransport().request("POST", "/api/models/test", req);
}

/** GET /api/models/:key/capabilities —— image input / image generation 探测。 */
export async function getModelCapabilities(
  key: string,
): Promise<ModelCapabilities> {
  return getTransport().request(
    "GET",
    `/api/models/${encodeURIComponent(key)}/capabilities`,
  );
}

// ─── Tasks（任务看板） ───────────────────────────────────────────
//
// 对应后端 task tracker（/api/tasks）。字段 snake_case，时间戳为
// epoch ms（number | null）。状态机见 ui/src/task_board.ts。

// ─── Workflows ───────────────────────────────────────────────────────
//
// `WorkflowForm` / `StepForm` 与后端 `latte-agent-ui-server` 的 workflow
// 管理接口一一对应（snake_case）。`source` 只有 project / global 两种：
// 全局 workflow 只读，「保存」等价于复制一份到项目层（POST create）。

export interface WorkflowSummary {
  name: string;
  description: string;
  steps_count: number;
  source: "project" | "global";
  file_path: string;
  command?: string | null;
}

export interface StepForm {
  id: string;
  description: string;
  speakers: string[];
  prompt: string;
  output_key: string | null;
}

export interface WorkflowForm {
  name: string;
  description: string;
  command?: string | null;
  max_rounds: number | null;
  steps: StepForm[];
}

export interface WorkflowDetail {
  name: string;
  description: string;
  max_rounds: number | null;
  steps: StepForm[];
  source: "project" | "global";
  file_path: string;
  raw_toml: string;
  command?: string | null;
}

export interface ValidateResponse {
  ok: boolean;
  errors: string[];
  warnings: string[];
}

export interface WorkflowRunRequest {
  name?: string;
  workflow?: WorkflowForm;
  topic: string;
  vars?: Record<string, string>;
}

/** GET /api/workflows：列出所有 workflow（项目 + 全局）。 */
export async function listWorkflows(): Promise<WorkflowSummary[]> {
  return getTransport().request("GET", "/api/workflows");
}

/** GET /api/workflows/:name：取单个 workflow 详情（含 raw_toml）。 */
export async function getWorkflow(name: string): Promise<WorkflowDetail> {
  return getTransport().request(
    "GET",
    `/api/workflows/${encodeURIComponent(name)}`,
  );
}

/** POST /api/workflows：新建 workflow（409 = 同名已存在）。 */
export async function createWorkflow(form: WorkflowForm): Promise<WorkflowDetail> {
  return getTransport().request("POST", "/api/workflows", form);
}

/** PUT /api/workflows/:name：更新 workflow；form.name 可与路径名不同（重命名）。 */
export async function updateWorkflow(
  name: string,
  form: WorkflowForm,
): Promise<WorkflowDetail> {
  return getTransport().request(
    "PUT",
    `/api/workflows/${encodeURIComponent(name)}`,
    form,
  );
}

/** DELETE /api/workflows/:name：删除项目层 workflow（全局不可删）。 */
export async function deleteWorkflow(name: string): Promise<void> {
  await getTransport().request(
    "DELETE",
    `/api/workflows/${encodeURIComponent(name)}`,
  );
}

/** POST /api/workflows/validate：校验表单，不落盘。 */
export async function validateWorkflow(form: WorkflowForm): Promise<ValidateResponse> {
  return getTransport().request("POST", "/api/workflows/validate", form);
}

/** POST /api/workflows/run：启动一次试运行（409 = 已有运行中的 run）。 */
export async function runWorkflow(
  req: WorkflowRunRequest,
): Promise<{ run_id: string; started: true }> {
  return getTransport().request("POST", "/api/workflows/run", req);
}

/** POST /api/workflows/run/stop：停止当前运行中的 workflow。 */
export async function stopWorkflowRun(): Promise<void> {
  await getTransport().request("POST", "/api/workflows/run/stop");
}

/** `POST /api/workflows/resume` —— 从断点续跑一个失败的 workflow。
 *  body: `{ session_id, wf_id? }`。
 *  `wf_id` 缺省 → 后端反向扫该 session 的 event_log 找最后一条失败的
 *  `WorkflowFinished`（最常见的"点击继续"场景，前端不用关心 wf_id）。
 *  显式 `wf_id` → 续跑指定的那一次（高级用例）。
 *  立即返回 `{ started, wf_id, name }`；续跑产生的 WorkflowStarted /
 *  Step / Turn / Finished 事件经该 session 的 SSE 流回，前端无需额外
 *  订阅。 */
export async function resumeWorkflow(
  sessionId: string,
  wfId?: string,
): Promise<{ started: true; wf_id: string; name: string }> {
  return getTransport().request("POST", "/api/workflows/resume", {
    session_id: sessionId,
    ...(wfId ? { wf_id: wfId } : {}),
  });
}

export type TaskState =
  | "backlog"
  | "todo"
  | "in_progress"
  | "human_review"
  | "rework"
  | "merging"
  | "done"
  | "cancelled";

export interface TaskRun {
  session_id: string;
  started_at: number;
  ended_at: number | null;
  result: string | null;
}

export interface TaskHistoryEntry {
  at: number;
  from: string | null;
  to: string;
  actor: string;
  note: string | null;
}

export interface TaskView {
  schema: string;
  id: string;
  title: string;
  description: string;
  priority: 1 | 2 | 3 | 4;
  state: TaskState;
  labels: string[];
  parent_id: string | null;
  sub_order: number;
  scheduled_at: number | null;
  runs: TaskRun[];
  created_at: number;
  updated_at: number;
  history: TaskHistoryEntry[];
  /** 绑定的 workflow 名；null = 未绑定（派发时走默认 manager 流程）。 */
  workflow: string | null;
  /** 子任务聚合（父任务用）：总数 / 终态数 / 各状态计数。 */
  sub_total: number;
  sub_done: number;
  sub_state_counts: Record<string, number>;
}

export interface TaskCreateBody {
  title: string;
  description?: string;
  priority?: number;
  parent_id?: string;
  labels?: string[];
  scheduled_at?: number | null;
  /** 可选：创建时绑定 workflow（派发时直接运行该 workflow）。 */
  workflow?: string;
}

export interface TaskPatchBody {
  title?: string;
  description?: string;
  priority?: number;
  state?: TaskState;
  scheduled_at?: number | null;
  labels?: string[];
  /** 可选：缺省 = 不变；null = 清除绑定；字符串 = 绑定该 workflow。 */
  workflow?: string | null;
}

/** POST /api/tasks/import 的单个任务项（支持嵌套 subtasks）。 */
export interface ImportTask {
  title: string;
  description?: string;
  priority?: number;
  labels?: string[];
  workflow?: string;
  /** 任务涉及的文件/目录前缀（相对项目根）；并行执行时范围重叠的任务会被拒绝派发（409）。 */
  paths?: string[];
  subtasks?: ImportTask[];
}

/** ChoiceRequested 事件里的单个选项（与后端 ChoiceOption 同构）。 */
export interface ChoiceOption {
  label: string;
  description?: string;
  /** 配图 URL，一般是 /api/images/<file>。 */
  image?: string;
  recommended?: boolean;
}

/** POST /api/images?ext=<ext>：上传原始图片字节，返回可访问的 URL。
 * 用于选择框「上传自己的图片」——拿到 path 后既能预览也能回喂模型。
 *
 * 走裸 `fetch`（不经共享 transport）：body 是原始二进制，而 transport
 * 的 request 会对非字符串 body 做 JSON.stringify，会破坏字节流。图片
 * 上传是 web UI 专属能力，同源 fetch 足够。 */
export async function uploadImage(file: File): Promise<{ path: string }> {
  const dot = file.name.lastIndexOf(".");
  const ext = (dot >= 0 ? file.name.slice(dot + 1) : "png").toLowerCase();
  const buf = await file.arrayBuffer();
  const r = await fetch(`/api/images?ext=${encodeURIComponent(ext)}`, {
    method: "POST",
    headers: { "Content-Type": "application/octet-stream" },
    body: buf,
  });
  if (!r.ok) {
    throw new Error(`upload image failed: ${r.status} ${await r.text().catch(() => "")}`);
  }
  return r.json();
}

/** GET /api/tasks：列出全部任务。 */
export async function listTasks(): Promise<TaskView[]> {
  return getTransport().request("GET", "/api/tasks");
}

/** POST /api/tasks：新建任务（可带 parent_id 拆分子任务）。 */
export async function createTask(body: TaskCreateBody): Promise<TaskView> {
  return getTransport().request("POST", "/api/tasks", body);
}

/** GET /api/tasks/:id。 */
export async function getTask(id: string): Promise<TaskView> {
  return getTransport().request("GET", `/api/tasks/${encodeURIComponent(id)}`);
}

/** PATCH /api/tasks/:id：全可选字段更新。 */
export async function updateTask(
  id: string,
  body: TaskPatchBody,
): Promise<TaskView> {
  return getTransport().request(
    "PATCH",
    `/api/tasks/${encodeURIComponent(id)}`,
    body,
  );
}

/** POST /api/tasks/:id/dispatch：立即执行（建 manager session 并发送任务）。 */
export async function dispatchTask(id: string): Promise<TaskView> {
  return getTransport().request(
    "POST",
    `/api/tasks/${encodeURIComponent(id)}/dispatch`,
  );
}

/** POST /api/tasks/dispatch-ready 的响应：批量派发结果。 */
export interface DispatchReadyResponse {
  /** 成功派发：[task_id, session_id]。 */
  dispatched: [string, string][];
  /** 跳过（任务留 todo 等位）：[task_id, 原因]。 */
  skipped: [string, string][];
}

/** POST /api/tasks/dispatch-ready：按优先级批量派发全部 todo 任务
 *  （后端遵守并发上限与同族/paths 互斥，冲突/超限任务留 todo 等位）。 */
export async function dispatchReady(): Promise<DispatchReadyResponse> {
  return getTransport().request("POST", "/api/tasks/dispatch-ready");
}

/** POST /api/tasks/:id/abort：中止执行，任务回到 todo。 */
export async function abortTask(id: string): Promise<TaskView> {
  return getTransport().request(
    "POST",
    `/api/tasks/${encodeURIComponent(id)}/abort`,
  );
}

/** DELETE /api/tasks/:id → 204。 */
export async function deleteTask(id: string): Promise<void> {
  await getTransport().request(
    "DELETE",
    `/api/tasks/${encodeURIComponent(id)}`,
  );
}

/** POST /api/tasks/import：批量导入任务（进 backlog），返回新建任务 id 列表。
 *  校验失败时后端返回 400 + 纯文本错误信息。
 *  planId 来自 PlanProposed 事件：带上即视为用户批准该任务清单，
 *  后端会把对应 session 的 plan 阶段门置为 Approved（解除实现类
 *  delegate 拦截），并自动跑一轮批量派发（结果在 auto_dispatch）。 */
export async function importTasks(
  tasks: ImportTask[],
  planId?: string,
): Promise<{ created: string[]; auto_dispatch?: DispatchReadyResponse }> {
  return getTransport().request("POST", "/api/tasks/import", { tasks, plan_id: planId });
}



// ─── Tool test ─────────────────────────────────────────────────────

export interface TestToolRequest {
  tool_id: string;
  args: Record<string, unknown>;
}

export interface TestToolResponse {
  ok: boolean;
  tool_id: string;
  latency_ms: number;
  response?: string;
  error?: string;
}

/** POST /api/tools/test —— 测试工具是否能正常执行。 */
export async function testTool(req: TestToolRequest): Promise<TestToolResponse> {
  return getTransport().request("POST", "/api/tools/test", req);
}

// ─── Role test ─────────────────────────────────────────────────────

export interface TestRoleRequest {
  role_id: string;
  config: RoleConfigSave;
}

export interface TestRoleResponse {
  ok: boolean;
  role_id: string;
  latency_ms: number;
  response?: string;
  error?: string;
}

/** POST /api/roles/test —— 测试角色配置是否能正常与模型对话。 */
export async function testRole(req: TestRoleRequest): Promise<TestRoleResponse> {
  return getTransport().request("POST", "/api/roles/test", req);
}

/** GET /api/roles/:id/toml —— 读取角色 TOML 源文件原始内容。 */
export async function getRoleToml(roleId: string): Promise<string> {
  return getTransport().requestText("GET", `/api/roles/${encodeURIComponent(roleId)}/toml`);
}

/** PUT /api/roles/:id/toml —— 直接写入角色 TOML 源文件原始内容。 */
export async function putRoleToml(roleId: string, raw: string): Promise<void> {
  await getTransport().request("PUT", `/api/roles/${encodeURIComponent(roleId)}/toml`, raw);
}

/** GET /api/workflows/:name/toml —— 读取 workflow TOML 源文件原始内容。 */
export async function getWorkflowToml(name: string): Promise<string> {
  return getTransport().requestText("GET", `/api/workflows/${encodeURIComponent(name)}/toml`);
}

/** PUT /api/workflows/:name/toml —— 直接写入 workflow TOML 源文件原始内容。 */
export async function putWorkflowToml(name: string, raw: string): Promise<void> {
  await getTransport().request("PUT", `/api/workflows/${encodeURIComponent(name)}/toml`, raw);
}
