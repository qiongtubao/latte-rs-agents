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
   * unsubscribe function. `onResync` 在事件流可能漏事件时触发
   * （服务端广播 lag 通知 / 看门狗判定半开重建前）——调用方应拉
   * 权威历史重放补齐。 */
  subscribeEvents(sessionId: string, onEvent: (ev: ChatEvent) => void, onResync?: () => void): () => void;
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

  subscribeEvents(sessionId: string, onEvent: (ev: ChatEvent) => void, onResync?: () => void): () => void {
    const url = `/api/events?id=${encodeURIComponent(sessionId)}`;
    let es: EventSource | null = null;
    let closed = false;
    let lastEventAt = Date.now();
    let retryTimer: ReturnType<typeof setTimeout> | undefined;

    const connect = (): void => {
      if (closed) return;
      lastEventAt = Date.now();
      es = new EventSource(url);
      es.addEventListener("chat_event", (e) => {
        lastEventAt = Date.now();
        try {
          const data = JSON.parse((e as MessageEvent).data) as ChatEvent;
          onEvent(data);
        } catch (err) {
          console.error("[sse] failed to parse chat_event", err, e);
        }
      });
      // 服务端每 15s 的 ping（真实 SSE 事件；keep-alive 注释对 JS
      // 不可见，看门狗只能靠真实事件判断连接还活着）。
      es.addEventListener("ping", () => {
        lastEventAt = Date.now();
      });
      es.addEventListener("open", () => {
        lastEventAt = Date.now();
        this.onConnectionStatus?.("connected");
      });
      es.addEventListener("error", (e) => {
        // 服务端广播 lag 通知是带 data 的 MessageEvent：连接没断但
        // 中间可能丢事件——触发全量重放补齐，不要断开。
        if ((e as MessageEvent).data !== undefined) {
          lastEventAt = Date.now();
          onResync?.();
          return;
        }
        // 连接错误：close 后 EventSource 不再自动重连，3s 后重建并
        // 重放补齐断开期间的事件。
        es?.close();
        es = null;
        this.onConnectionStatus?.("disconnected");
        if (!closed && retryTimer === undefined) {
          retryTimer = setTimeout(() => {
            retryTimer = undefined;
            onResync?.();
            connect();
          }, 3000);
        }
      });
    };
    connect();
    // 看门狗：超过 60s 没有任何事件（含 ping）说明连接半开——机器
    // 休眠/网络挂起时 EventSource 不报错也没数据，UI 会永久停在旧
    // 状态（jemalloc 现场实锤）。只能主动断开重建。
    const watchdog = setInterval(() => {
      if (closed || !es) return;
      if (Date.now() - lastEventAt > 60_000) {
        es.close();
        es = null;
        onResync?.();
        connect();
      }
    }, 15_000);
    return () => {
      closed = true;
      clearInterval(watchdog);
      if (retryTimer !== undefined) clearTimeout(retryTimer);
      es?.close();
    };
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
