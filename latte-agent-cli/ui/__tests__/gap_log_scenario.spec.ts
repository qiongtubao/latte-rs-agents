/**
 * 模拟用户原始场景的 e2e 测试：
 * 在 redis 8.x 代码库中分析 gap_log 数据结构。
 *
 * 核心验证：delegate 不再因超时而失败（原 bug 在 30s 就 timeout）。
 */

import { test, expect, chromium, type Page, type Browser } from "@playwright/test";

const UI_BASE = process.env.UI_BASE_URL ?? "http://localhost:4567";

test.describe("Redis gap_log 分析场景（原始超时 bug 复现）", () => {
  let browser: Browser;
  let page: Page;

  test.beforeAll(async () => {
    browser = await chromium.launch({ headless: true, args: ["--no-sandbox", "--disable-dev-shm-usage"] });
  });

  test.beforeEach(async () => {
    page = await browser.newPage({ viewport: { width: 1400, height: 900 } });
    page.on("pageerror", (err) => console.error("[pageerror]", err.message));
    await page.goto(UI_BASE, { waitUntil: "domcontentloaded", timeout: 15000 });
    await page.waitForTimeout(3000);
  });

  test.afterEach(async () => {
    await page.close();
  });

  test.afterAll(async () => {
    await browser.close();
  });

  test("S0: gap_log 分析委托不超时", async () => {
    // 发送用户原始的问题
    await page.evaluate(() => {
      (document.getElementById("chat-input") as HTMLTextAreaElement).value =
        "分析一下当前项目中的gap_log 数据结构，如果需要修改成环形数组 怎么修改";
      document.getElementById("chat-send")?.click();
    });

    // 验证 delegate 至少成功开始（第一轮委派到达 programmer）
    // 原始 bug 在 30s 就 timeout，所以 60s 内能看到第一次委派就算修复成功
    let delegateSeen = false;
    for (let i = 0; i < 12; i++) {
      await page.waitForTimeout(5000);
      delegateSeen = await page.evaluate(() => {
        const msgs = Array.from(document.querySelectorAll(".message, .message-row, .message"));
        return msgs.some(m => m.textContent?.includes("→ programmer"));
      });
      if (delegateSeen) break;
    }
    expect(delegateSeen).toBe(true);

    // 再等 60s，验证没有被超时中断（没有 DelegateFinished timeout 状态）
    let noTimeoutError = true;
    for (let i = 0; i < 12; i++) {
      await page.waitForTimeout(5000);
      const state = await page.evaluate(() => {
        const statusEl = document.querySelector(".status-label");
        const msgs = Array.from(document.querySelectorAll(".message, .message-row, .message"));
        const timeoutMsg = msgs.find(m =>
          m.textContent?.includes("timeout") || m.textContent?.includes("超时")
        );
        return {
          status: statusEl?.textContent || "",
          stillRunning: statusEl?.textContent?.includes("⏳") || statusEl?.textContent?.includes("委托"),
          hasTimeout: !!timeoutMsg,
        };
      });
      if (state.hasTimeout) {
        noTimeoutError = false;
        break;
      }
      // 如果已经回到已连接状态，说明任务完成，提前结束
      if (state.status === "已连接") break;
    }

    // 核心断言：没有超时错误
    expect(noTimeoutError).toBe(true);
  });

  test("S1: 前台无控制台错误", async () => {
    const consoleErrors: string[] = [];
    page.on("console", (msg) => {
      if (msg.type() === "error") consoleErrors.push(msg.text());
    });

    await page.evaluate(() => {
      (document.getElementById("chat-input") as HTMLTextAreaElement).value =
        "分析gap_log";
      document.getElementById("chat-send")?.click();
    });

    // 等 30s，看 delegate 是否正常启动
    for (let i = 0; i < 6; i++) {
      await page.waitForTimeout(5000);
      const started = await page.evaluate(() => {
        const msgs = Array.from(document.querySelectorAll(".message, .message-row, .message"));
        return msgs.some(m => m.textContent?.includes("→ programmer") || m.textContent?.includes("→ architect"));
      });
      if (started) break;
    }

    expect(consoleErrors.length).toBe(0);
  });
});
