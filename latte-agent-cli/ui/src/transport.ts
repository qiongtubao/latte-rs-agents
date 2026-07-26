// Contract C1 (design doc §3): every backend access goes through a
// ChatTransport. `HttpSseTransport` is the default fetch + EventSource
// implementation used in CLI/browser mode; the editor host injects a
// `TauriIpcTransport` via `initTransport()` (phase 2). api.ts is the
// only consumer — all UI code keeps calling api.ts functions.

import type { ChatEvent, SelfLoopEvent } from "./api";

export interface ChatTransport {
  request<T>(method: string, path: string, body?: unknown): Promise<T>;
  /** 同 request，但返回原始文本（不 JSON.parse）。用于 TOML 等纯文本内容。 */
  requestText(method: string, path: string, body?: unknown): Promise<string>;
  /** Subscribe to the chat event stream of a session. Returns an
   * unsubscribe function. */
  subscribeEvents(sessionId: string, onEvent: (ev: ChatEvent) => void): () => void;
  subscribeSelfLoop(onEvent: (ev: SelfLoopEvent) => void): () => void;
  onConnectionStatus?: (status: "connected" | "disconnected") => void;
}

/** HTTP failure raised by `HttpSseTransport.request` on non-2xx
 * responses. Carries `status` so callers whose control flow depends on
 * it (ensureSession's stale-id fallback) can tell HTTP errors apart
 * from network failures. */
export class HttpError extends Error {
  readonly method: string;
  readonly path: string;
  readonly status: number;

  constructor(method: string, path: string, status: number, bodyText: string) {
    super(`${method} ${path} ${status}${bodyText ? `: ${bodyText}` : ""}`);
    this.name = "HttpError";
    this.method = method;
    this.path = path;
    this.status = status;
  }
}

export class HttpSseTransport implements ChatTransport {
  onConnectionStatus?: (status: "connected" | "disconnected") => void;

  async request<T>(method: string, path: string, body?: unknown, timeoutMs = 60_000): Promise<T> {
    const init: RequestInit = { method };
    if (body !== undefined) {
      init.headers = { "Content-Type": "application/json" };
      init.body = typeof body === "string" ? body : JSON.stringify(body);
    }
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), timeoutMs);
    init.signal = controller.signal;
    try {
      const r = await fetch(path, init);
      if (!r.ok) {
        const text = await r.text().catch(() => "");
        throw new HttpError(method, path, r.status, text);
      }
      const text = await r.text();
      return (text ? JSON.parse(text) : undefined) as T;
    } finally {
      clearTimeout(timer);
    }
  }

  async requestText(method: string, path: string, body?: unknown): Promise<string> {
    const init: RequestInit = { method };
    if (body !== undefined) {
      init.headers = { "Content-Type": "application/json" };
      init.body = typeof body === "string" ? body : JSON.stringify(body);
    }
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 60_000);
    init.signal = controller.signal;
    try {
      const r = await fetch(path, init);
      if (!r.ok) {
        const text = await r.text().catch(() => "");
        throw new HttpError(method, path, r.status, text);
      }
      return r.text();
    } finally {
      clearTimeout(timer);
    }
  }

  subscribeEvents(sessionId: string, onEvent: (ev: ChatEvent) => void): () => void {
    const es = new EventSource(`/api/events?id=${encodeURIComponent(sessionId)}`);
    es.addEventListener("chat_event", (e) => {
      try {
        const data = JSON.parse((e as MessageEvent).data) as ChatEvent;
        onEvent(data);
      } catch (err) {
        console.error("[sse] failed to parse chat_event", err, e);
      }
    });
    es.addEventListener("open", () => this.onConnectionStatus?.("connected"));
    es.addEventListener("error", () => {
      es.close();
      this.onConnectionStatus?.("disconnected");
    });
    return () => es.close();
  }

  subscribeSelfLoop(onEvent: (ev: SelfLoopEvent) => void): () => void {
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
}
let currentTransport: ChatTransport | null = null;

export function initTransport(t: ChatTransport): void {
  currentTransport = t;
}

/** The active transport; lazily defaults to HttpSseTransport so plain
 * browser/CLI mode needs zero wiring. */
export function getTransport(): ChatTransport {
  if (!currentTransport) currentTransport = new HttpSseTransport();
  return currentTransport;
}
