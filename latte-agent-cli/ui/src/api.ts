// REST + SSE client for latte-agent-ui. All chat endpoints now require a
// per-tab `session_id`; this module owns a module-level `currentSessionId`
// and attaches it to every request automatically. The session itself is
// persisted in localStorage so a tab refresh reattaches to the same
// chat history without a server roundtrip.

import { getHost } from "./host";

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
  prompt_file: string | null;
  /** 当前生效的 system prompt 文本。 */
  prompt: string;
}

export interface RolesConfig {
  roles: RoleConfigEntry[];
  available_tools: string[];
  tiers: string[];
}

/** `POST /api/roles/config` 请求体（category/prompt_file/skills 不可改）。 */
export interface RoleConfigSave {
  id: string;
  name: string;
  icon: string;
  model_tier: string;
  model_chain: string[];
  temperature: number | null;
  tools: string[];
  prompt: string;
}

export async function getRolesConfig(): Promise<RolesConfig> {
  const r = await fetch("/api/roles/config");
  if (!r.ok) throw new Error(`GET /api/roles/config ${r.status}`);
  return r.json();
}

export async function saveRoleConfig(
  body: RoleConfigSave,
): Promise<RoleConfigEntry> {
  const r = await fetch("/api/roles/config", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!r.ok) {
    const msg = await r.text().catch(() => "");
    throw new Error(`POST /api/roles/config ${r.status}${msg ? `: ${msg}` : ""}`);
  }
  return r.json();
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
  | { type: "DelegateFinished"; from_role: string; to_role: string; status: string; summary: string; sub_id: string }
  | { type: "WorkflowStarted"; name: string; topic: string; wf_id: string }
  | { type: "WorkflowStep"; wf_id: string; step_id: string; description: string; index: number; total: number }
  | { type: "WorkflowTurn"; wf_id: string; step_id: string; role_id: string; content: string; round: number }
  | { type: "WorkflowFinished"; name: string; wf_id: string; status: string; summary: string }
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
  const r = await fetch("/api/sessions");
  if (!r.ok) throw new Error(`GET /api/sessions ${r.status}`);
  return r.json();
}

export async function createSession(): Promise<string> {
  const r = await fetch("/api/sessions", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: "{}",
  });
  if (!r.ok) throw new Error(`POST /api/sessions ${r.status}`);
  const info: SessionInfo = await r.json();
  persistSessionId(info.session_id);
  return info.session_id;
}

export function switchSession(sessionId: string): void {
  currentSessionId = sessionId;
}

/** Rename a session (empty label clears the custom name). */
export async function renameSession(
  sessionId: string,
  label: string,
): Promise<void> {
  const r = await fetch("/api/session/label", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ session_id: sessionId, label }),
  });
  if (!r.ok) throw new Error(`POST /api/session/label ${r.status}`);
}

/** Delete a session server-side (its controller is aborted). */
export async function deleteSession(sessionId: string): Promise<void> {
  const r = await fetch(
    `/api/sessions?id=${encodeURIComponent(sessionId)}`,
    { method: "DELETE" },
  );
  if (!r.ok) throw new Error(`DELETE /api/sessions ${r.status}`);
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
    const r = await fetch(
      `/api/session?id=${encodeURIComponent(persisted)}`,
    );
    if (r.ok) {
      currentSessionId = persisted;
      return persisted;
    }
    ls?.removeItem(sessionStorageKey());
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
  const r = await fetch(
    `/api/session?id=${encodeURIComponent(currentSessionId)}`,
  );
  if (!r.ok) throw new Error(`GET /api/session ${r.status}`);
  return r.json();
}

/** Fetch the archived ChatEvent log for a session — used to restore
 * the chat panel contents after switching sessions. */
export async function getSessionHistory(sessionId: string): Promise<ChatEvent[]> {
  const r = await fetch(
    `/api/session/history?id=${encodeURIComponent(sessionId)}`,
  );
  if (!r.ok) throw new Error(`GET /api/session/history ${r.status}`);
  return r.json();
}

export async function getRoles(): Promise<RoleInfo[]> {
  const r = await fetch("/api/roles");
  if (!r.ok) throw new Error(`GET /api/roles ${r.status}`);
  return r.json();
}

export async function sendMessage(message: string): Promise<void> {
  await fetch("/api/chat/send", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(chatBody({ message })),
  });
}

export async function sendCommand(command: string): Promise<void> {
  await fetch("/api/chat/command", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(chatBody({ command })),
  });
}

export async function switchRole(role_id: string): Promise<void> {
  await fetch("/api/chat/role", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(chatBody({ role_id })),
  });
}

export async function listTraces(): Promise<TraceSummary[]> {
  const r = await fetch("/api/traces");
  if (!r.ok) throw new Error(`GET /api/traces ${r.status}`);
  return r.json();
}

export async function readTrace(session_id: string): Promise<{ session_id: string; events: unknown[] }> {
  const r = await fetch(`/api/traces/${encodeURIComponent(session_id)}`);
  if (!r.ok) throw new Error(`GET /api/traces/${session_id} ${r.status}`);
  return r.json();
}

// ─── SSE ────────────────────────────────────────────────────────────────

export function subscribeEvents(
  onEvent: (e: ChatEvent) => void,
  onConnectionStatus: (status: "connected" | "disconnected") => void,
): { disconnect: () => void; reconnect: () => void } {
  let es: EventSource | null = null;

  const connect = (): void => {
    if (!currentSessionId) {
      // Caller forgot to ensureSession() — surface as "disconnected" so
      // the UI pill asks the user to reload. Better than silently
      // subscribing to the wrong tab's events.
      onConnectionStatus("disconnected");
      return;
    }
    es = new EventSource(
      `/api/events?id=${encodeURIComponent(currentSessionId)}`,
    );
    es.addEventListener("chat_event", (e) => {
      try {
        const data = JSON.parse((e as MessageEvent).data) as ChatEvent;
        onEvent(data);
      } catch (err) {
        console.error("[sse] failed to parse chat_event", err, e);
      }
    });
    es.addEventListener("open", () => onConnectionStatus("connected"));
    es.addEventListener("error", () => {
      // error 时主动 close() — 浏览器 EventSource 自带重连但那会让关
      // 掉 UI 后页面挂着不释放。
      if (es) {
        es.close();
        es = null;
      }
      onConnectionStatus("disconnected");
    });
  };

  connect();

  return {
    disconnect: () => {
      if (es) {
        es.close();
        es = null;
      }
    },
    reconnect: () => {
      if (es) return;
      connect();
    },
  };
}

export function subscribeSelfLoop(onEvent: (e: SelfLoopEvent) => void): () => void {
  const es = new EventSource("/api/self-loop/events");
  es.addEventListener("self_loop_event", (e) => {
    try {
      const data = JSON.parse((e as MessageEvent).data) as SelfLoopEvent;
      onEvent(data);
    } catch (err) {
      console.error("[sse] failed to parse self_loop_event", err, e);
    }
  });
  return () => es.close();
}

export async function startSelfLoop(task: string, max_iterations: number): Promise<void> {
  await fetch("/api/self-loop/start", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ task, max_iterations }),
  });
}

/** Fetch the subsession event log for a delegate call. */
export async function fetchSubsession(subId: string): Promise<unknown[]> {
  const r = await fetch(`/api/subsessions?id=${encodeURIComponent(subId)}`);
  if (!r.ok) throw new Error(`GET /api/subsessions?${subId} ${r.status}`);
  return r.json();
}


export async function stopSelfLoop(): Promise<void> {
  await fetch("/api/self-loop/stop", { method: "POST" });
}
