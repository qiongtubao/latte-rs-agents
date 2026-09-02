/**
 * 工具相关 API 的「前后端契约」回归测试。
 *
 * 背景：`setToolEnabled` 曾写成 `PATCH /api/tools/:id`，而后端只注册了
 * `POST /api/tools/:id/toggle` —— 类型检查抓不到这种字符串漂移，面板上的
 * 启用/禁用开关与批量操作因此静默失败。
 *
 * 所以这里做两层断言：
 *  1. 前端确实按预期的 method + path 发请求（mock transport 记录调用）；
 *  2. 该 method + path 在后端路由表（`latte-agent-ui-server/src/lib.rs`
 *     的 `.route(...)` 声明）里真实存在 —— 任意一侧改动都会让测试失败。
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

import { describe, it, expect, vi } from "vitest";

import type { ChatTransport } from "./transport";

// ─── 后端路由表解析 ──────────────────────────────────────────────

const HERE = path.dirname(fileURLToPath(import.meta.url));
const LIB_RS = path.resolve(
  HERE,
  "../../../latte-agent-ui-server/src/lib.rs",
);

/**
 * 从 lib.rs 里抽出路由声明，返回 `Set<"METHOD /path">`。
 *
 * 解析策略：按 `.route(` 出现的位置把源码切成窗口（每个窗口到下一个
 * `.route(` 或最近的 `;` 为止 —— 路由是链式声明，所以这个边界是准的）。
 * 窗口里第一个字符串字面量是路径，其余 `get(` / `post(` / `put(` /
 * `patch(` / `delete(` 是方法（兼容 `axum::routing::post(` 全限定写法）。
 *
 * 不用单条大正则跨行匹配：`[\s\S]*?` 会在某条 route 的括号里找不到边界时
 * 一路吞掉后面若干条声明，导致漏解析（而漏解析会让契约断言假阴性）。
 */
function backendApiRoutes(): Set<string> {
  const src = readFileSync(LIB_RS, "utf8");
  const routes = new Set<string>();
  const marker = ".route(";

  const starts: number[] = [];
  for (let i = src.indexOf(marker); i !== -1; i = src.indexOf(marker, i + 1)) {
    starts.push(i);
  }

  for (let n = 0; n < starts.length; n++) {
    const from = starts[n] + marker.length;
    const nextRoute = n + 1 < starts.length ? starts[n + 1] : src.length;
    const semi = src.indexOf(";", from);
    const to = Math.min(nextRoute, semi === -1 ? src.length : semi);
    const window = src.slice(from, to);

    const pathMatch = window.match(/"([^"]+)"/);
    if (!pathMatch) continue;
    const routePath = pathMatch[1];
    // 只收路径形状的（排除 `.route(` 里其它字符串参数的误命中）。
    if (!routePath.startsWith("/")) continue;

    const afterPath = window.slice(pathMatch.index! + pathMatch[0].length);
    for (const hm of afterPath.matchAll(
      /(?:^|[^A-Za-z_])(get|post|put|patch|delete)\s*\(/g,
    )) {
      routes.add(`${hm[1].toUpperCase()} ${routePath}`);
    }
  }
  return routes;
}

/** `/api` 前缀在 lib.rs 里是 nest 上去的，路由声明本身不带它。 */
function stripApiPrefix(p: string): string {
  return p.startsWith("/api/") ? p.slice("/api".length) : p;
}

/** 把具体 id 还原成后端路由里的 `:id` 占位符。 */
function toRouteTemplate(p: string, id: string): string {
  return stripApiPrefix(p).replace(
    new RegExp(`/${id.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}(?=/|$)`),
    "/:id",
  );
}

// ─── 前端 transport mock ─────────────────────────────────────────

interface Call {
  method: string;
  path: string;
  body?: unknown;
}

async function importFresh(): Promise<{
  api: typeof import("./api");
  transport: typeof import("./transport");
}> {
  vi.resetModules();
  const [api, transport] = await Promise.all([
    import("./api"),
    import("./transport"),
  ]);
  return { api, transport };
}

function install(
  transport: typeof import("./transport"),
  calls: Call[],
  reply: unknown = {},
): void {
  const mock = {
    request: vi.fn(async (method: string, p: string, body?: unknown) => {
      calls.push({ method, path: p, body });
      return reply;
    }),
    requestText: vi.fn(async (method: string, p: string, body?: unknown) => {
      calls.push({ method, path: p, body });
      return "";
    }),
    subscribeEvents: vi.fn(() => vi.fn()),
    subscribeSelfLoop: vi.fn(() => vi.fn()),
  };
  transport.initTransport(mock as unknown as ChatTransport);
}

// ─── 测试 ────────────────────────────────────────────────────────

describe("工具 API 与后端路由表一致", () => {
  it("后端路由表可解析，且含已知的 tools 端点（解析器自检）", () => {
    const routes = backendApiRoutes();
    // 自检：解析器本身没坏（否则下面的断言会因为空集合而假阳性通过）。
    expect(routes.size).toBeGreaterThan(20);
    expect(routes).toContain("GET /tools");
    expect(routes).toContain("POST /tools/test");
  });

  it("setToolEnabled → POST /api/tools/:id/toggle（后端存在该路由）", async () => {
    const { api, transport } = await importFresh();
    const calls: Call[] = [];
    install(transport, calls);

    await api.setToolEnabled("read", false);

    expect(calls).toHaveLength(1);
    expect(calls[0].method).toBe("POST");
    expect(calls[0].path).toBe("/api/tools/read/toggle");
    expect(calls[0].body).toEqual({ enabled: false });

    // 关键断言：后端真的注册了这条路由。
    const routes = backendApiRoutes();
    expect(routes).toContain(
      `${calls[0].method} ${toRouteTemplate(calls[0].path, "read")}`,
    );
  });

  it("getToolDoc → GET /api/tools/:id/doc（后端存在该路由）", async () => {
    const { api, transport } = await importFresh();
    const calls: Call[] = [];
    install(transport, calls, {
      id: "read",
      content: "# doc",
      editable: true,
      source: "disk",
      path: "prompts/tools/read.md",
      description: "",
      kind: "builtin",
    });

    const doc = await api.getToolDoc("read");
    expect(doc.source).toBe("disk");

    expect(calls[0].method).toBe("GET");
    expect(calls[0].path).toBe("/api/tools/read/doc");

    const routes = backendApiRoutes();
    expect(routes).toContain(
      `${calls[0].method} ${toRouteTemplate(calls[0].path, "read")}`,
    );
  });

  it("putToolDoc → PUT /api/tools/:id/doc，body 为 Markdown 裸字符串", async () => {
    const { api, transport } = await importFresh();
    const calls: Call[] = [];
    install(transport, calls);

    await api.putToolDoc("read", "# 自定义\n\n- 一行。");

    expect(calls[0].method).toBe("PUT");
    expect(calls[0].path).toBe("/api/tools/read/doc");
    // 必须是裸字符串：transport 对 string body 不做 JSON.stringify，
    // 后端 axum 侧用 `body: String` 提取。包成对象会写进一个 JSON 文本。
    expect(typeof calls[0].body).toBe("string");
    expect(calls[0].body).toBe("# 自定义\n\n- 一行。");

    const routes = backendApiRoutes();
    expect(routes).toContain(
      `${calls[0].method} ${toRouteTemplate(calls[0].path, "read")}`,
    );
  });

  it("工具 id 做 URL 编码（含特殊字符不破坏路径）", async () => {
    const { api, transport } = await importFresh();
    const calls: Call[] = [];
    install(transport, calls);

    await api.setToolEnabled("git.status", true);
    expect(calls[0].path).toBe("/api/tools/git.status/toggle");
  });
});
