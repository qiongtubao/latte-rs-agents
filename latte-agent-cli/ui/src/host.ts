// Host bridge contract (design doc §5.1, contract C3).
//
// When the UI runs inside latte-code-editor (Tauri webview iframe), the host
// page hands over capabilities either by injecting `window.__LATTE_HOST__`
// (same-origin, plus a "latte-host-ready" event) or — when the iframe is
// served from another origin and the parent cannot write into it — via a
// `postMessage({ type: "latte:init", host })` handshake; capability calls
// then go back over `postMessage({ type: "latte:call", method, args })`.
// The UI in turn exposes `window.__LATTE_UI__` after mount. In plain
// browser/CLI mode neither exists and every host capability must be
// feature-detected (`if (host.openLocation)`).

export interface CodeRef {
  /** Path relative to the workspace root. */
  path: string;
  startLine?: number;
  endLine?: number;
  column?: number;
  /** Qualified symbol name (used for graph lookups). */
  symbol?: string;
}

export interface LatteHost {
  platform?: "tauri" | "web";
  /** Absolute path of the active workspace. */
  workspaceRoot?: string;
  /** Session persistence key, isolated per workspace (replaces the bare
   * localStorage key used in CLI mode). */
  sessionKey?: string;
  /** Jump to a code/doc location in the editor. */
  openLocation?(ref: CodeRef): void;
  /** Locate & highlight the node in the graph panel. */
  revealInGraph?(ref: CodeRef): void;
  /** Open in the doc viewer (.md / doc_gen output). */
  openDoc?(ref: CodeRef): void;
}

export interface LatteUiApi {
  focus(): void;
  insertContext(ref: CodeRef & { quote?: string }): void;
}

declare global {
  interface Window {
    __LATTE_HOST__?: LatteHost;
    __LATTE_UI__?: LatteUiApi;
  }
}

/** undefined = handshake not attempted yet; null = web mode (no host). */
let hostCache: LatteHost | null | undefined;

/**
 * Resolve the injected host, if any. Three channels, first ready wins:
 *  1. `__LATTE_HOST__` already present (same-origin direct injection);
 *  2. the "latte-host-ready" window event (same-origin, late injection);
 *  3. a cross-origin `{ type: "latte:init", host: {...} }` postMessage
 *     from the parent page (iframe is served from another origin, so the
 *     parent cannot write `contentWindow.__LATTE_HOST__` directly).
 * After `timeoutMs` with none of the above, falls back to null (plain
 * browser/CLI mode). The result is cached.
 */
export function waitForHost(timeoutMs = 250): Promise<LatteHost | null> {
  if (hostCache !== undefined) return Promise.resolve(hostCache);
  if (typeof window === "undefined") {
    hostCache = null;
    return Promise.resolve(null);
  }
  if (window.__LATTE_HOST__) {
    hostCache = window.__LATTE_HOST__;
    return Promise.resolve(hostCache);
  }
  return new Promise((resolve) => {
    const finish = (host: LatteHost | null): void => {
      clearTimeout(timer);
      window.removeEventListener("latte-host-ready", onReady);
      window.removeEventListener("message", onMessage);
      hostCache = host;
      resolve(host);
    };
    const onReady = (): void => finish(window.__LATTE_HOST__ ?? null);
    const onMessage = (event: MessageEvent): void => {
      const host = hostFromInitMessage(event.data);
      if (host) finish(host);
    };
    const timer = setTimeout(() => finish(null), timeoutMs);
    window.addEventListener("latte-host-ready", onReady);
    window.addEventListener("message", onMessage);
  });
}

/** Host capabilities the parent may advertise via `latte:init`. */
const CAPABILITY_NAMES = ["openLocation", "revealInGraph", "openDoc"] as const;

/** Build a LatteHost from a cross-origin `{ type: "latte:init" }`
 * postMessage payload: data fields are copied verbatim, and each
 * advertised capability becomes a fire-and-forget call that posts
 * `{ type: "latte:call", method, args: [ref] }` back to the parent
 * window. The parent validates `event.origin` on its side; the init
 * message carries only data, so origin is not checked here. Returns null
 * for anything that is not an init message. */
function hostFromInitMessage(data: unknown): LatteHost | null {
  if (!data || typeof data !== "object") return null;
  const msg = data as { type?: unknown; host?: unknown };
  if (msg.type !== "latte:init" || !msg.host || typeof msg.host !== "object") {
    return null;
  }
  const src = msg.host as Record<string, unknown>;
  const host: LatteHost = {};
  if (src.platform === "tauri" || src.platform === "web") host.platform = src.platform;
  if (typeof src.workspaceRoot === "string") host.workspaceRoot = src.workspaceRoot;
  if (typeof src.sessionKey === "string") host.sessionKey = src.sessionKey;
  const caps = Array.isArray(src.capabilities) ? src.capabilities : [];
  for (const name of CAPABILITY_NAMES) {
    if (!caps.includes(name)) continue;
    host[name] = (ref: CodeRef): void => {
      window.parent.postMessage({ type: "latte:call", method: name, args: [ref] }, "*");
    };
  }
  return host;
}

/** Read the cached host (post-handshake). Also picks up a synchronously
 * injected `__LATTE_HOST__` even if `waitForHost` has not run yet. */
export function getHost(): LatteHost | null {
  if (hostCache !== undefined) return hostCache;
  if (typeof window === "undefined") return null;
  return window.__LATTE_HOST__ ?? null;
}

/** Expose the UI api to the host page (`window.__LATTE_UI__`). Call once
 * after mount completes; the host may queue calls until this appears. */
export function setUiApi(api: LatteUiApi): void {
  if (typeof window === "undefined") return;
  window.__LATTE_UI__ = api;
}
