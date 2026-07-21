/**
 * 验证默认 turn 超时：发送消息后应该看到后端发出 Error 事件
 * （不再无限等待）。这是修复"查看当前文件夹路径后页面卡死"的根因。
 *
 * 关键设计：
 *   1. 用 env var `LATTE_AGENT_TURN_TIMEOUT_SECS=20` 强制把超时压短，
 *      不依赖 glm.toml 里的 timeout_secs=300。这样测试 CI 跑得快且
 *      稳定，不被模型配置的改动影响。
 *   2. 用唯一 probe 文本 + `hasText` 过滤 selector，避免命中之前测试
 *      残留的 chat history。
 *   3. fill 之前等 3 秒让 SSE 订阅建立（之前测试 race 在这里挂）。
 */
import { test, expect, chromium, Page, Browser } from "@playwright/test";

const UI_BASE = process.env.UI_BASE_URL ?? "http://localhost:4567";
// 20s 强制超时 + 5s 网络/事件缓冲 = 等 25s 内必看到超时
const TIMEOUT_WAIT_MS = 25_000;

test.describe("默认 turn 超时修复", () => {
  let browser: Browser;
  let page: Page;

  test.beforeAll(async () => {
    browser = await chromium.launch({ headless: true, args: ["--no-sandbox", "--disable-dev-shm-usage"] });
  });

  test.afterAll(async () => {
    await browser.close();
  });

  test("glm-5.2 没有 timeout_secs 时应触发 turn 超时", async () => {
    test.setTimeout(TIMEOUT_WAIT_MS + 30_000);

    page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
    // 创建一个隔离的新 session，避免复用之前测试遗留的 chat history
    await page.goto(UI_BASE, { waitUntil: "domcontentloaded", timeout: 15_000 });
    await page.waitForSelector("#chat-input", { timeout: 10_000 });
    // 给 SSE 订阅 + /api/session 初始化时间（关键：fill 之前必须等订阅建立）
    await page.waitForTimeout(3000);

    // 用唯一标识避免 selector 抓到旧会话残留
    const probe = `测试默认超时-${Date.now()}`;
    await page.locator("#chat-input").fill(probe);
    await page.locator("#chat-send").click();

    // 等该条特定用户消息出现（避免 .first() 命中旧会话残留）
    const userRow = page.locator(".message-row.self", { hasText: probe });
    await expect(userRow).toHaveCount(1, { timeout: 10_000 });
    console.log(`[test] 消息已发送，等待最多 ${TIMEOUT_WAIT_MS / 1000}s 看后端超时错误`);

    // 等待错误或 RoleFinished 事件（来自 SSE）
    // 后端超时阈值 = max(env `LATTE_AGENT_TURN_TIMEOUT_SECS`, model.timeout_secs, default=120s)。
    // CI 跑测试前用 env 强制压到 20s；本地没设 env 时也至少 120s 触发。
    const startTime = Date.now();
    let foundError = false;
    while (Date.now() - startTime < TIMEOUT_WAIT_MS) {
      const errors = await page.locator(".message-row.error, .message.error, .message.status").allTextContents();
      const combined = errors.join(" | ");
      if (combined.includes("timed out") || combined.includes("timeout")) {
        foundError = true;
        const elapsed = Math.floor((Date.now() - startTime) / 1000);
        console.log(`[test] ✓ ${elapsed}s 后看到超时提示: ${combined.substring(0, 200)}`);
        break;
      }
      await page.waitForTimeout(2000);
    }

    if (!foundError) {
      // 兜底看日志面板（之前我们刚加的日志查看功能）
      await page.evaluate(() => {
        const btn = document.getElementById("log-btn") as HTMLButtonElement | null;
        btn?.click();
      });
      await page.waitForTimeout(2000);
      console.log("[test] 超时未触发，检查日志：");
    }

    expect(foundError).toBe(true);
    await page.close();
  });
});