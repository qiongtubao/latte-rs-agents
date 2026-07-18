import { describe, it, expect, afterEach, vi } from "vitest";
import type { LatteHost, CodeRef } from "./host";

// host.ts touches the bare `window` global; vitest runs in node, so each
// test installs a minimal window stub and re-imports the module fresh
// (the module caches the handshake result).

type Handler = (ev: { data?: unknown }) => void;

interface FakeWindow {
  listeners: Map<string, Set<Handler>>;
  posted: { data: unknown; targetOrigin: string }[];
  win: Record<string, unknown>;
  emit(type: string, data?: unknown): void;
}

function installFakeWindow(): FakeWindow {
  const listeners = new Map<string, Set<Handler>>();
  const posted: { data: unknown; targetOrigin: string }[] = [];
  const win: Record<string, unknown> = {
    addEventListener: (type: string, fn: Handler) => {
      if (!listeners.has(type)) listeners.set(type, new Set());
      listeners.get(type)!.add(fn);
    },
    removeEventListener: (type: string, fn: Handler) => {
      listeners.get(type)?.delete(fn);
    },
    postMessage: (data: unknown, targetOrigin: string) => {
      posted.push({ data, targetOrigin });
    },
  };
  win.parent = win;
  (globalThis as Record<string, unknown>).window = win;
  return {
    listeners,
    posted,
    win,
    emit(type: string, data?: unknown): void {
      for (const fn of listeners.get(type) ?? []) fn({ data });
    },
  };
}

async function importFreshHost(): Promise<typeof import("./host")> {
  vi.resetModules();
  return import("./host");
}

afterEach(() => {
  delete (globalThis as Record<string, unknown>).window;
});

describe("waitForHost", () => {
  it("returns a pre-injected __LATTE_HOST__ immediately", async () => {
    const fake = installFakeWindow();
    const injected: LatteHost = { platform: "tauri", sessionKey: "k" };
    fake.win.__LATTE_HOST__ = injected;
    const host = await (await importFreshHost()).waitForHost(50);
    expect(host).toBe(injected);
  });

  it("times out to null in plain browser mode", async () => {
    installFakeWindow();
    const host = await (await importFreshHost()).waitForHost(30);
    expect(host).toBeNull();
  });

  it("resolves via the latte-host-ready event", async () => {
    const fake = installFakeWindow();
    const mod = await importFreshHost();
    const promise = mod.waitForHost(500);
    const injected: LatteHost = { platform: "web" };
    fake.win.__LATTE_HOST__ = injected;
    fake.emit("latte-host-ready");
    expect(await promise).toBe(injected);
  });

  it("resolves via a latte:init postMessage and builds capability methods", async () => {
    const fake = installFakeWindow();
    const mod = await importFreshHost();
    const promise = mod.waitForHost(500);
    fake.emit("message", {
      type: "latte:init",
      host: {
        platform: "tauri",
        sessionKey: "latte:session:ws1",
        workspaceRoot: "/home/user/proj",
        capabilities: ["openLocation", "revealInGraph"],
      },
    });
    const host = await promise;
    expect(host).not.toBeNull();
    expect(host!.platform).toBe("tauri");
    expect(host!.sessionKey).toBe("latte:session:ws1");
    expect(host!.workspaceRoot).toBe("/home/user/proj");
    // advertised capabilities become fire-and-forget postMessage calls
    const ref: CodeRef = { path: "src/foo.rs", startLine: 12 };
    host!.openLocation!(ref);
    host!.revealInGraph!(ref);
    expect(fake.posted).toEqual([
      { data: { type: "latte:call", method: "openLocation", args: [ref] }, targetOrigin: "*" },
      { data: { type: "latte:call", method: "revealInGraph", args: [ref] }, targetOrigin: "*" },
    ]);
    // capabilities not advertised stay absent (feature detection)
    expect(host!.openDoc).toBeUndefined();
  });

  it("ignores unrelated messages and still falls back to timeout", async () => {
    const fake = installFakeWindow();
    const mod = await importFreshHost();
    const promise = mod.waitForHost(30);
    fake.emit("message", { type: "something:else", host: {} });
    fake.emit("message", { type: "latte:init" }); // missing host object
    fake.emit("message", null);
    expect(await promise).toBeNull();
    expect(fake.posted).toEqual([]);
  });

  it("caches the handshake result", async () => {
    installFakeWindow();
    const mod = await importFreshHost();
    const first = await mod.waitForHost(30);
    expect(first).toBeNull();
    expect(mod.getHost()).toBeNull();
    // second call must not re-run the handshake (no window access needed)
    expect(await mod.waitForHost(30)).toBeNull();
  });
});
