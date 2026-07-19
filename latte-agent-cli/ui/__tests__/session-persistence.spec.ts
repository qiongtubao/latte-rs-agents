import { test, expect } from "@playwright/test";
import { spawn, ChildProcess } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

// 会话持久化 e2e（2026-07 用户报告）：
//   ① 发送的消息要以 UserMessage 事件回显（不再本地乐观渲染）
//   ② 刷新页面后历史回放含用户消息
//   ③ 重启 server 后 session 仍在（落盘恢复），用户消息仍在
//
// 不需要模型：POST /api/chat/send 返回 202，UserMessage 在调用 LLM 前
// 已广播；模型不可达只会产生后续 Error 事件，不影响本用例断言。
//
// 跑法：UI_NO_WEBSERVER=1 pnpm exec playwright test __tests__/session-persistence.spec.ts

const PORT = 45697;
const ORIGIN = `http://127.0.0.1:${PORT}`;

let server: ChildProcess | null = null;

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

async function startServer(): Promise<void> {
  const here = path.dirname(fileURLToPath(import.meta.url));
  const bin = path.resolve(here, "../../../target/debug/latte-agent");
  server = spawn(bin, ["ui", "--port", String(PORT)], { stdio: "ignore" });
  await waitForHealth(`${ORIGIN}/health`);
}

async function stopServer(): Promise<void> {
  server?.kill();
  server = null;
  // 等端口真正释放，避免重启时 EADDRINUSE
  await new Promise((r) => setTimeout(r, 500));
}

test.beforeAll(startServer);
test.afterAll(stopServer);

test("user message echoes, survives reload and server restart", async ({ page }) => {
  const text = `持久化验证消息-${Date.now()}`;

  await page.goto(`${ORIGIN}/`);
  const input = page.locator("#chat-input");
  await expect(input).toBeVisible({ timeout: 10_000 });

  // ① 发送 → UserMessage 事件回显出用户气泡（无本地乐观渲染）
  await input.fill(text);
  await page.locator("#chat-send").click();
  const bubble = page.locator(".message-row.self", { hasText: text });
  await expect(bubble).toHaveCount(1, { timeout: 10_000 });

  // ② 刷新 → 历史回放里用户消息仍在
  await page.reload();
  await expect(page.locator(".message-row.self", { hasText: text })).toHaveCount(1, {
    timeout: 10_000,
  });

  // ③ 重启 server → session 落盘恢复，用户消息仍回放
  await stopServer();
  await startServer();
  await page.reload();
  await expect(page.locator(".message-row.self", { hasText: text })).toHaveCount(1, {
    timeout: 15_000,
  });
});
