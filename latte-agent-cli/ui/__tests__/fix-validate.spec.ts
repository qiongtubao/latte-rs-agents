/**
 * 验证 delegate timeout 修复的测试。
 *
 * 核心验证点：
 * 1. 委派完成（无超时）—— delegate 应该运行到子 agent 主动返回
 * 2. 委派 UI 渲染正确—— delegateStarted/finished 事件
 * 3. 长时间运行不超时—— 模拟需要多步的分析任务
 */

import { test, expect, chromium, type Page, type Browser } from "@playwright/test";

const UI_BASE = process.env.UI_BASE_URL ?? "http://localhost:4567";

test.describe("Delegate no-timeout fix validation", () => {
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

  test("F1: @programmer 委托完成没有超时", async () => {
    // 发送一条需要多步分析的委托
    await page.evaluate(() => {
      (document.getElementById("chat-input") as HTMLTextAreaElement).value =
        "@programmer 查看package.json的依赖和脚本";
      document.getElementById("chat-send")?.click();
    });

    // 等待委托完成（最多 120s，是原来的 4 倍——即便没有超时也不会卡死）
    let delegateDone = false;
    for (let i = 0; i < 24; i++) {
      await page.waitForTimeout(5000);
      delegateDone = await page.evaluate(() => {
        // 检查是否有 DelegateFinished 事件
        const msgs = Array.from(document.querySelectorAll(".message-row, .message"));
        const finished = msgs.some(
          (m) =>
            m.textContent?.includes("P") &&
            (m.textContent?.includes("✅") || m.textContent?.includes("Done"))
        );
        const status = document.querySelector(".status-label")?.textContent;
        return finished || status === "已连接";
      });
      if (delegateDone) break;
    }

    expect(delegateDone).toBe(true);

    // 验证委托完成——核心修复验证
    const state = await page.evaluate(() => {
      const msgs = Array.from(document.querySelectorAll(".message, .message-row, .message"));
      const programmerMsg = Array.from(document.querySelectorAll(".message-row")).find(
        (m) => m.querySelector(".msg-avatar")?.textContent === "P"
      );
      return {
        hasProgrammerReply: !!programmerMsg,
        hasDelegateEvent: msgs.some((m) =>
          m.textContent?.includes("🤝 @manager → @programmer")
        ),
        statusLabel: document.querySelector(".status-label")?.textContent,
      };
    });

    expect(state.hasDelegateEvent).toBe(true);
    expect(state.hasProgrammerReply).toBe(true);
    expect(state.statusLabel).toBe("已连接");
  });

  test("F2: 无控制台错误", async () => {
    const errors: string[] = [];
    page.on("console", (msg) => {
      if (msg.type() === "error") errors.push(msg.text());
    });

    // 发送委托并等待完成
    await page.evaluate(() => {
      (document.getElementById("chat-input") as HTMLTextAreaElement).value =
        "@programmer 列出当前目录文件";
      document.getElementById("chat-send")?.click();
    });

    for (let i = 0; i < 24; i++) {
      await page.waitForTimeout(5000);
      const status = await page.evaluate(
        () => document.querySelector(".status-label")?.textContent
      );
      if (status === "已连接" || status === "已卡住") break;
    }

    // 没有 console.error
    expect(errors.length).toBe(0);
  });
});
