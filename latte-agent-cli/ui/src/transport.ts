// Contract C1 (design doc §3): every backend access goes through a
// ChatTransport. `HttpSseTransport` is the default fetch + EventSource
// implementation used in CLI/browser mode; the editor host injects a
// `TauriIpcTransport` via `initTransport()` (phase 2). api.ts is the
// only consumer — all UI code keeps calling api.ts functions.

import type { ChatEvent, SelfLoopEvent } from "./api";

export interface ChatTransport {
  request<T>(method: string, path: string, body?: unknown): Promise<T>;
  /** Subscribe to the chat event stream of a session. Returns an
   * unsubscribe function. */
  subscribeEvents(sessionId: string, onEvent: (ev: ChatEvent) => void): () => void;
  subscribeSelfLoop(onEvent: (ev: SelfLoopEvent) => void): () => void;
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
  /** NOT part of contract C1: connection-status hook the api.ts facade
   * uses to keep the UI status pill behavior identical to the
   * pre-transport EventSource code. Other transports simply don't set
   * it (the facade then reports "connected" once subscribed). */
  onConnectionStatus?: (status: "connected" | "disconnected") => void;

  async request<T>(method: string, path: string, body?: unknown): Promise<T> {
    const init: RequestInit = { method };
    if (body !== undefined) {
      init.headers = { "Content-Type": "application/json" };
      init.body = typeof body === "string" ? body : JSON.stringify(body);
    }
    const r = await fetch(path, init);
    if (!r.ok) {
      const text = await r.text().catch(() => "");
      throw new HttpError(method, path, r.status, text);
    }
    const text = await r.text();
    return (text ? JSON.parse(text) : undefined) as T;
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
      // error 时主动 close() — 浏览器 EventSource 自带重连但那会让关
      // 掉 UI 后页面挂着不释放。
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

/** Inject a host-provided transport (e.g. the editor's
 * TauriIpcTransport). Must run before any api.ts call. */
export function initTransport(t: ChatTransport): void {
  currentTransport = t;
}

/** The active transport; lazily defaults to HttpSseTransport so plain
 * browser/CLI mode needs zero wiring. */
export function getTransport(): ChatTransport {
  if (!currentTransport) currentTransport = new HttpSseTransport();
  return currentTransport;
}
