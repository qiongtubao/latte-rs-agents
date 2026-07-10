// API client + ChatEvent 类型 — 与 Rust 端 `controller.rs::ChatEvent` 字段对齐。

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
  available_roles: RoleInfo[];
}

export interface TraceSummary {
  session_id: string;
  path: string;
  size_bytes: number;
  modified_unix: number;
}

// ChatEvent —— Rust enum ChatEvent 的 JSON 表示。
// 后端用 `serde_json::to_string(&event)` 序列化，前端按 `type` 字段 dispatch。
//
// 注意：字段是 snake_case 还是 camelCase？查 controller.rs `ChatEvent::RoleTurn`
// 字段是 `role_id, content, is_complete`。serde 默认 snake_case，所以前端读
// `event.role_id` 而不是 `event.roleId`。这点和 latte-ts-models 的前端约定不同
// （那边是 camelCase），本 UI 单独约定 snake_case 与 Rust 字段一致。
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
  | { type: "ToolResult"; role_id: string; tool_name: string; result: string }
  | { type: "DelegateStarted"; from_role: string; to_role: string; task: string }
  | { type: "DelegateFinished"; from_role: string; to_role: string; status: string; summary: string }
  // 自定义 SelfLoopEvent（不是 ChatEvent，是 /api/self-loop/events 的 payload）
  | { type: "SelfLoopEvent"; kind: string; iteration: number; message: string; screenshot?: string; data?: unknown; timestamp_unix_ms: number };

// SelfLoopEvent — 与 Rust 端 ui.rs::SelfLoopEvent 字段对齐。
export interface SelfLoopEvent {
  kind: "started" | "iteration" | "log" | "screenshot" | "done" | "error";
  iteration: number;
  message: string;
  screenshot?: string;
  data?: unknown;
  timestamp_unix_ms: number;
}

// ─── REST helpers ────────────────────────────────────────────────

export async function getSession(): Promise<SessionInfo> {
  const r = await fetch("/api/session");
  if (!r.ok) throw new Error(`GET /api/session ${r.status}`);
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
    body: JSON.stringify({ message }),
  });
}

export async function sendCommand(command: string): Promise<void> {
  await fetch("/api/chat/command", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ command }),
  });
}

export async function switchRole(role_id: string): Promise<void> {
  await fetch("/api/chat/role", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ role_id }),
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

// ─── SSE ─────────────────────────────────────────────────────────

export function subscribeEvents(
  onEvent: (e: ChatEvent) => void,
  // SSE 连接状态变化（开 / 关）的回调。第一次连接成功 = "connected"，
  // 服务端关闭或网络断开 = "disconnected"（此时 EventSource 已经被
  // 我们关掉，不会再 auto-reconnect，要重连请调返回的 `reconnect`）。
  onConnectionStatus: (status: "connected" | "disconnected") => void,
): { disconnect: () => void; reconnect: () => void } {
  let es: EventSource | null = null;

  const connect = (): void => {
    es = new EventSource("/api/events");
    es.addEventListener("chat_event", (e) => {
      try {
        const data = JSON.parse((e as MessageEvent).data) as ChatEvent;
        onEvent(data);
      } catch (err) {
        console.error("[sse] failed to parse chat_event", err, e);
      }
    });
    es.addEventListener("open", () => {
      onConnectionStatus("connected");
    });
    es.addEventListener("error", () => {
      // 关键：error 时主动 close()，停掉浏览器自带的 auto-reconnect。
      // 之前这里只发了个 "retrying…" 消息但 EventSource 仍在反复
      // 重连 —— 用户关掉 UI 服务端后页面就一直挂着不释放。
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
      if (es) return; // 已经连着就别重复建
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

export async function stopSelfLoop(): Promise<void> {
  await fetch("/api/self-loop/stop", { method: "POST" });
}
