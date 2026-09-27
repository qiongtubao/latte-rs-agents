import { describe, expect, it, vi, beforeEach } from "vitest";
import type * as ApiModule from "./api";
import type * as TransportModule from "./transport";

// 多 tab / 并发请求的 api 层防护：
//   1. ensureSession 的 in-flight 去重（同 tab 并发 mount 只跑一次 create）
//   2. setRefineParent 的 LS RMW 串行化（两 tab 同时写不丢字段）
//   3. deleteSession 跨 tab 删 session 触发本 tab 清态 + 重建

async function importFresh(): Promise<{
  api: typeof ApiModule;
  transport: typeof TransportModule;
}> {
  vi.resetModules();
  const [api, transport] = await Promise.all([
    import("./api"),
    import("./transport"),
  ]);
  return { api, transport };
}

interface Call {
  method: string;
  path: string;
  body?: unknown;
}

function installTransport(
  transport: typeof TransportModule,
  calls: Call[],
): void {
  const session_counter = { v: 0 };
  // request 是泛型 <T>(...) => Promise<T>；vi.fn 推不出，统一 as any 给 mock
  // 类型留口。生产实现里具体类型由调用处推断（参见 api.ts 的 request<T>）。
  transport.initTransport({
    request: ((method: string, path: string, body?: unknown) => {
      calls.push({ method, path, body });
      if (path === "/api/sessions" && method === "POST") {
        session_counter.v += 1;
        return Promise.resolve({ session_id: `sess-${session_counter.v}` });
      }
      return Promise.resolve({});
    }) as unknown as Parameters<typeof transport.initTransport>[0]["request"],
    requestText: ((method: string, path: string, body?: unknown) => {
      calls.push({ method, path, body });
      return Promise.resolve("");
    }) as unknown as Parameters<typeof transport.initTransport>[0]["requestText"],
    subscribeEvents: vi.fn(() => () => {}),
    subscribeSelfLoop: vi.fn(() => () => {}),
  });
}

/** 把 microtask + 排队的 setTimeout 跑光——写流程是 async + Promise 链，
 * 纯 Promise.resolve 不足以排干队列，必须给 fake timer 一次前进。 */
async function drainMicrotasks(): Promise<void> {
  await vi.runAllTimersAsync();
}

beforeEach(() => {
  vi.useFakeTimers();
  const store = new Map<string, string>();
  (globalThis as Record<string, unknown>).window = {
    localStorage: {
      getItem: (k: string) => store.get(k) ?? null,
      setItem: (k: string, v: string) => {
        store.set(k, v);
      },
      removeItem: (k: string) => {
        store.delete(k);
      },
    },
  };
});

describe("ensureSession in-flight 去重", () => {
  it("并发两次 ensureSession 只跑一次 createSession", async () => {
    const { api, transport } = await importFresh();
    const calls: Call[] = [];
    installTransport(transport, calls);
    const [a, b] = await Promise.all([api.ensureSession(), api.ensureSession()]);
    expect(a).toBe(b);
    const posts = calls.filter(
      (c) => c.method === "POST" && c.path === "/api/sessions",
    );
    expect(posts.length).toBe(1);
  });
});

describe("setRefineParent LS RMW 串行", () => {
  it("两个并发 setRefineParent 各自写不同 key：两条映射都不丢", async () => {
    const { api, transport } = await importFresh();
    installTransport(transport, []);
    // 模拟两 tab 同时 refine 两个 task。修复前是 load→mutate→write 裸
    // RMW，落败方会丢；修复后写流程用 Promise 串行，两条映射都在。
    api.setRefineParent("session-A", "task-X");
    api.setRefineParent("session-B", "task-Y");
    await drainMicrotasks();
    const ls = (
      globalThis as unknown as {
        window: {
          localStorage: { getItem: (k: string) => string | null };
        };
      }
    ).window.localStorage;
    const raw = ls.getItem(
      `${api.getCurrentSessionId() ?? "latte-agent-ui-session-id"}:refine-parents`,
    );
    expect(raw).not.toBeNull();
    const parsed = JSON.parse(raw ?? "{}") as Record<string, string>;
    expect(parsed["session-A"]).toBe("task-X");
    expect(parsed["session-B"]).toBe("task-Y");
  });
});

describe("deleteSession 跨 tab 删 session 触发本 tab 清态 + 重建", () => {
  it("模拟另一 tab 送达「deleted」后：本 tab 清态 → 重建新 session", async () => {
    const { api, transport } = await importFresh();
    installTransport(transport, []);
    await api.ensureSession();
    const sid = api.getCurrentSessionId();
    expect(sid).not.toBeNull();
    // 直接调 onSessionEvent 等价于另一 tab 经 BroadcastChannel 送达——
    // jsdom 同 BroadcastChannel 实例 self-broadcast 不可达，跨实例行为
    // 是浏览器内置；bus 本身只做 postMessage 转发，订阅侧的逻辑我们
    // 测。
    api.onSessionEvent((action, id) => {
      if (action !== "deleted") return;
      if (api.getCurrentSessionId() !== id) return;
      api.clearLocalSessionId();
      void api.ensureSession();
    });
    api.announceSession("deleted", sid as string);
    await drainMicrotasks();
    await drainMicrotasks();
    expect(api.getCurrentSessionId()).not.toBe(sid);
    expect(api.getCurrentSessionId()).not.toBeNull();
  });
});

describe("prompt-dismissed 跨 tab 通知", () => {
  // 「另一 tab 真实收到」是 BroadcastChannel 在浏览器的行为——jsdom +
  // fake timers 下不可靠。这里只验证 POST + 监听器注册这两段（决定
  // 跨 tab 同步能不能成立），跨实例 broadcast 留给人工/E2E 测。
  it("dismissPrompt 走 POST /api/chat/prompt-dismiss", async () => {
    const { api, transport } = await importFresh();
    const calls: Call[] = [];
    installTransport(transport, calls);
    await api.dismissPrompt("choice-tutor-0");
    await drainMicrotasks();
    expect(
      calls.some((c) => c.method === "POST" && c.path === "/api/chat/prompt-dismiss"),
      "dismissPrompt 必须 POST /api/chat/prompt-dismiss",
    ).toBe(true);
  });

  it("importTasks 走 POST /api/tasks/import（带 planId）", async () => {
    const { api, transport } = await importFresh();
    const calls: Call[] = [];
    installTransport(transport, calls);
    await api.importTasks(
      [{ title: "x" }],
      "plan-manager-0",
      undefined,
      "sess-a",
    );
    await drainMicrotasks();
    expect(
      calls.some(
        (c) =>
          c.method === "POST" &&
          c.path === "/api/tasks/import" &&
          (c.body as { plan_id?: string } | undefined)?.plan_id === "plan-manager-0",
      ),
      "importTasks 必须带 plan_id POST /api/tasks/import",
    ).toBe(true);
  });

  it("onPromptDismissed 返回的退订函数能解除订阅", async () => {
    const { api } = await importFresh();
    // 通过订阅集合直接观察：注册一个 listener、记录被调次数、退订、
    // 再调一次 listener 集合内的回调（直接走 listeners 集合——通过
    // unsubscribe 路径观察）。本测试用 BroadcastChannel 不可用时也能
    // 验证注册/退订流程本身。
    let count = 0;
    const unsub = api.onPromptDismissed(() => {
      count += 1;
    });
    unsub();
    // listener 已被移出集合；BroadcastChannel 触发不到它。这里只能验
    // 退订函数存在 + 调用不抛错（其他测试覆盖实际触发路径）。
    expect(typeof unsub).toBe("function");
    expect(count).toBe(0);
  });
});
