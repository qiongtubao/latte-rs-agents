import { describe, it, expect, afterEach, vi } from "vitest";
import type { ChatTransport } from "./transport";
import type { ChatEvent, SelfLoopEvent } from "./api";

// api.ts / transport.ts both keep module-level state (currentSessionId,
// currentTransport), so every test re-imports them fresh.

async function importFresh(): Promise<{
  api: typeof import("./api");
  transport: typeof import("./transport");
}> {
  vi.resetModules();
  const [api, transport] = await Promise.all([import("./api"), import("./transport")]);
  return { api, transport };
}

function mockTransport() {
  return {
    request: vi.fn(
      async (_method: string, _path: string, _body?: unknown): Promise<unknown> => ({}),
    ),
    subscribeEvents: vi.fn(
      (_sessionId: string, _onEvent: (ev: ChatEvent) => void): (() => void) => vi.fn(),
    ),
    subscribeSelfLoop: vi.fn(
      (_onEvent: (ev: SelfLoopEvent) => void): (() => void) => vi.fn(),
    ),
  };
}

type MockTransport = ReturnType<typeof mockTransport>;

function init(transport: { initTransport(t: ChatTransport): void }, mock: MockTransport): void {
  // The mock's concrete (non-generic) request signature can't satisfy
  // the generic ChatTransport.request directly — fine at runtime.
  transport.initTransport(mock as unknown as ChatTransport);
}

afterEach(() => {
  delete (globalThis as Record<string, unknown>).window;
});

describe("transport facade", () => {
  it("getTransport() lazily defaults to HttpSseTransport", async () => {
    const { transport } = await importFresh();
    const t = transport.getTransport();
    expect(t).toBeInstanceOf(transport.HttpSseTransport);
    expect(transport.getTransport()).toBe(t); // cached
  });

  it("api request functions go through the injected transport", async () => {
    const { api, transport } = await importFresh();
    const mock = mockTransport();
    init(transport, mock);

    await api.listSessions();
    expect(mock.request).toHaveBeenCalledWith("GET", "/api/sessions");

    await api.getRolesConfig();
    expect(mock.request).toHaveBeenCalledWith("GET", "/api/roles/config");

    await api.deleteSession("s 1");
    expect(mock.request).toHaveBeenCalledWith(
      "DELETE",
      `/api/sessions?id=${encodeURIComponent("s 1")}`,
    );

    await api.deleteTask("LAT 1");
    expect(mock.request).toHaveBeenCalledWith(
      "DELETE",
      `/api/tasks/${encodeURIComponent("LAT 1")}`,
    );
  });
  it("chatBody stamps session_id once persisted", async () => {
    const { api, transport } = await importFresh();
    const mock = mockTransport();
    init(transport, mock);

    await api.sendMessage("hi");
    expect(mock.request).toHaveBeenCalledWith("POST", "/api/chat/send", { message: "hi" });

    api.persistSessionId("abc");
    await api.sendMessage("hi");
    expect(mock.request).toHaveBeenCalledWith("POST", "/api/chat/send", {
      session_id: "abc",
      message: "hi",
    });
    await api.switchRole("tester");
    expect(mock.request).toHaveBeenCalledWith("POST", "/api/chat/role", {
      session_id: "abc",
      role_id: "tester",
    });
  });

  it("getSession requires a session id before hitting the transport", async () => {
    const { api, transport } = await importFresh();
    const mock = mockTransport();
    init(transport, mock);
    await expect(api.getSession()).rejects.toThrow("no session id");
    expect(mock.request).not.toHaveBeenCalled();
  });

  it("subscribeEvents delegates with the current session id and manages reconnect", async () => {
    const { api, transport } = await importFresh();
    const mock = mockTransport();
    init(transport, mock);
    api.persistSessionId("s1");

    const statuses: string[] = [];
    const onEvent = vi.fn();
    const sub = api.subscribeEvents(onEvent, (s) => statuses.push(s));

    // non-Http transport: facade reports connected right away
    expect(mock.subscribeEvents).toHaveBeenCalledTimes(1);
    expect(mock.subscribeEvents.mock.calls[0][0]).toBe("s1");
    expect(statuses).toEqual(["connected"]);

    // reconnect while subscribed is a no-op
    sub.reconnect();
    expect(mock.subscribeEvents).toHaveBeenCalledTimes(1);

    // disconnect → unsubscribe; then reconnect re-subscribes
    const firstUnsub = mock.subscribeEvents.mock.results[0].value;
    sub.disconnect();
    expect(firstUnsub).toHaveBeenCalledTimes(1);
    sub.reconnect();
    expect(mock.subscribeEvents).toHaveBeenCalledTimes(2);
  });

  it("subscribeEvents without a session reports disconnected", async () => {
    const { api, transport } = await importFresh();
    const mock = mockTransport();
    init(transport, mock);
    const statuses: string[] = [];
    api.subscribeEvents(vi.fn(), (s) => statuses.push(s));
    expect(statuses).toEqual(["disconnected"]);
    expect(mock.subscribeEvents).not.toHaveBeenCalled();
  });

  it("subscribeSelfLoop delegates to the transport", async () => {
    const { api, transport } = await importFresh();
    const mock = mockTransport();
    init(transport, mock);
    const onEvent = vi.fn();
    api.subscribeSelfLoop(onEvent);
    expect(mock.subscribeSelfLoop).toHaveBeenCalledWith(onEvent);
  });

  it("ensureSession falls back to createSession on HttpError, rethrows other errors", async () => {
    const { api, transport } = await importFresh();
    const mock = mockTransport();
    init(transport, mock);

    // stale persisted id: GET /api/session 404 → mint a new session
    const store = new Map<string, string>([["latte-agent-ui-session-id", "stale-id"]]);
    (globalThis as Record<string, unknown>).window = {
      localStorage: {
        getItem: (k: string) => store.get(k) ?? null,
        setItem: (k: string, v: string) => void store.set(k, v),
        removeItem: (k: string) => void store.delete(k),
      },
    };
    mock.request
      .mockRejectedValueOnce(new transport.HttpError("GET", "/api/session?id=stale-id", 404, ""))
      .mockResolvedValueOnce({ session_id: "fresh-id" });
    const id = await api.ensureSession();
    expect(id).toBe("fresh-id");
    expect(mock.request).toHaveBeenNthCalledWith(1, "GET", "/api/session?id=stale-id");
    expect(mock.request).toHaveBeenNthCalledWith(2, "POST", "/api/sessions", {});
    expect(store.get("latte-agent-ui-session-id")).toBe("fresh-id");

    // network-style failure propagates
    const { api: api2, transport: transport2 } = await importFresh();
    const mock2 = mockTransport();
    init(transport2, mock2);
    (globalThis as Record<string, unknown>).window = {
      localStorage: {
        getItem: () => "stale-id",
        setItem: () => {},
        removeItem: () => {},
      },
    };
    mock2.request.mockRejectedValueOnce(new TypeError("fetch failed"));
    await expect(api2.ensureSession()).rejects.toThrow("fetch failed");
  });
  it("persists refine parent mapping across a page reload", async () => {
    const store = new Map<string, string>();
    (globalThis as Record<string, unknown>).window = {
      localStorage: {
        getItem: (k: string) => store.get(k) ?? null,
        setItem: (k: string, v: string) => void store.set(k, v),
        removeItem: (k: string) => void store.delete(k),
      },
    };

    const { api } = await importFresh();
    api.setRefineParent("sess-refine-1", "LAT-106");

    const { api: reloadedApi } = await importFresh();
    expect(reloadedApi.refineParentFor("sess-refine-1")).toBe("LAT-106");
  });
});
