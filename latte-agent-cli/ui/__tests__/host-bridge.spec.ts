import { test, expect } from "@playwright/test";
import { spawn, ChildProcess } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

// 跨域宿主桥冒烟：模拟 latte-code-editor 的嵌入方式——
// 父页面（localhost）≠ iframe 内 UI（127.0.0.1），两个 origin，
// 验证 postMessage 版的 latte:init / latte:call 协议在真实浏览器里闭环。
//
// 覆盖点：
//   1. UI 收到 latte:init 后完成握手（__LATTE_UI__ 挂载 = waitForHost 已解析）
//   2. host.sessionKey 生效（session_id 写到宿主给的 localStorage 键）
//   3. UI → 父页面的 latte:call 能被收到且 origin 可校验（编辑器侧 openLocation 的依据）
//   4. 父页面 → UI 的反向通道 latte:ui-call（insertContext 进输入框、focus 聚焦）
//
// 需要已构建的 debug 二进制（target/debug/latte-agent）与最新 ui/dist。
// 跑法：UI_NO_WEBSERVER=1 pnpm exec playwright test __tests__/host-bridge.spec.ts

const PORT = 45699;
const FRAME_ORIGIN = `http://127.0.0.1:${PORT}`;
const PARENT_URL = `http://localhost:${PORT}/__parent_test__`;

let server: ChildProcess;

// Chrome 142+ 的 Local Network Access 检查会拦截 localhost→127.0.0.1 的跨域 iframe
// （net::ERR_BLOCKED_BY_LOCAL_NETWORK_ACCESS_CHECKS），测试里关掉；
// 真实宿主是 WebKitGTK（Linux Tauri），无此检查。
test.use({ launchOptions: { args: ["--disable-features=LocalNetworkAccessChecks"] } });

async function waitForHealth(url: string, timeoutMs = 30_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const r = await fetch(url);
      if (r.ok) return;
    } catch {
      /* not up yet */
    }
    await new Promise((r) => setTimeout(r, 300));
  }
  throw new Error(`server did not become healthy: ${url}`);
}

test.beforeAll(async () => {
  const here = path.dirname(fileURLToPath(import.meta.url));
  const bin = path.resolve(here, "../../../target/debug/latte-agent");
  server = spawn(bin, ["ui", "--port", String(PORT)], { stdio: "ignore" });
  await waitForHealth(`${FRAME_ORIGIN}/health`);
});

test.afterAll(() => {
  server?.kill();
});

test("cross-origin host bridge: init handshake + sessionKey + latte:call", async ({ page }) => {
  // 父页面（不同 origin）：iframe 加载后注入宿主，监听并校验 UI 回呼
  await page.route(PARENT_URL, (route) =>
    route.fulfill({
      contentType: "text/html",
      body: `<!doctype html><html><body>
        <iframe id="f" src="${FRAME_ORIGIN}/" style="width:1200px;height:700px"></iframe>
        <script>
          window.__calls = [];
          const ORIGIN = ${JSON.stringify(FRAME_ORIGIN)};
          const f = document.getElementById("f");
          f.addEventListener("load", () => {
            f.contentWindow.postMessage({
              type: "latte:init",
              host: {
                platform: "tauri",
                sessionKey: "test-session-key-123",
                workspaceRoot: "/tmp/ws",
                capabilities: ["openLocation", "revealInGraph"],
              },
            }, ORIGIN);
          });
          window.addEventListener("message", (e) => {
            if (e.origin !== ORIGIN) return; // 编辑器侧同款 origin 校验
            window.__calls.push(e.data);
          });
        </script></body></html>`,
    }),
  );

  await page.goto(PARENT_URL);
  const frame = page.frames().find((fr) => fr.url().startsWith(FRAME_ORIGIN));
  expect(frame, "chat UI iframe loaded").toBeTruthy();

  // 1. postMessage init 被 UI 接收并完成握手：setUiApi 只在 host 就绪后执行
  await frame!.waitForFunction(() => !!(window as any).__LATTE_UI__, undefined, {
    timeout: 10_000,
  });

  // 2. host.sessionKey 生效：session_id 应写到宿主指定的键，而不是默认键
  await expect
    .poll(() => frame!.evaluate(() => localStorage.getItem("test-session-key-123")), {
      timeout: 10_000,
    })
    .not.toBeNull();
  const defaultKey = await frame!.evaluate(() =>
    localStorage.getItem("latte-agent-ui-session-id"),
  );
  expect(defaultKey).toBeNull();

  // 3. UI → 父页面：latte:call（chip 点击时 host.openLocation 发出的就是这种消息）
  await frame!.evaluate(() => {
    window.parent.postMessage(
      {
        type: "latte:call",
        method: "openLocation",
        args: [{ path: "src/foo.rs", startLine: 42 }],
      },
      "*",
    );
  });
  await page.waitForFunction(() => (window as any).__calls.length > 0);
  const calls = await page.evaluate(() => (window as any).__calls);
  expect(calls[0]).toMatchObject({
    type: "latte:call",
    method: "openLocation",
    args: [{ path: "src/foo.rs", startLine: 42 }],
  });
});

test("cross-origin host bridge: reverse channel latte:ui-call (insertContext + focus)", async ({
  page,
}) => {
  // 反向通道：父页面（跨域）经 postMessage 调用 UI 暴露的能力。
  await page.route(PARENT_URL, (route) =>
    route.fulfill({
      contentType: "text/html",
      body: `<!doctype html><html><body>
        <iframe id="f" src="${FRAME_ORIGIN}/" style="width:1200px;height:700px"></iframe>
        <script>
          const ORIGIN = ${JSON.stringify(FRAME_ORIGIN)};
          const f = document.getElementById("f");
          f.addEventListener("load", () => {
            f.contentWindow.postMessage({
              type: "latte:init",
              host: { platform: "tauri", sessionKey: "test-session-key-456", capabilities: [] },
            }, ORIGIN);
          });
          window.__uiCall = (msg) => f.contentWindow.postMessage(msg, ORIGIN);
        </script></body></html>`,
    }),
  );

  await page.goto(PARENT_URL);
  const frame = page.frames().find((fr) => fr.url().startsWith(FRAME_ORIGIN));
  expect(frame, "chat UI iframe loaded").toBeTruthy();

  // mount 完成（setUiApi + installUiCallListener 同步执行，UI 就绪即监听就绪）
  await frame!.waitForFunction(() => !!(window as any).__LATTE_UI__, undefined, {
    timeout: 10_000,
  });

  // insertContext：引用 + quote 进入聊天输入框
  await page.evaluate(() =>
    (window as any).__uiCall({
      type: "latte:ui-call",
      method: "insertContext",
      args: [
        {
          path: "src/foo.rs",
          startLine: 10,
          endLine: 20,
          symbol: "bar",
          quote: "fn bar() {}",
        },
      ],
    }),
  );
  await expect
    .poll(
      () =>
        frame!.evaluate(
          () => (document.getElementById("chat-input") as HTMLTextAreaElement).value,
        ),
      { timeout: 5_000 },
    )
    .toContain("src/foo.rs:10-20");
  const inputValue = await frame!.evaluate(
    () => (document.getElementById("chat-input") as HTMLTextAreaElement).value,
  );
  expect(inputValue).toContain("bar");
  expect(inputValue).toContain("fn bar() {}");

  // focus：activeElement 切到聊天输入框
  await page.evaluate(() =>
    (window as any).__uiCall({ type: "latte:ui-call", method: "focus", args: [] }),
  );
  await expect
    .poll(
      () => frame!.evaluate(() => document.activeElement?.id),
      { timeout: 5_000 },
    )
    .toBe("chat-input");
});
