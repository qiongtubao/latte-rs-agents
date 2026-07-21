/**
 * Playwright e2e 测试：验证日志面板功能。
 *
 * 流程：
 *   1. 打开页面
 *   2. 点击"📋 日志"按钮
 *   3. 验证日志面板显示（移除 .hidden）
 *   4. 验证默认显示了文件列表
 *   5. 验证点击文件后显示了具体内容
 *
 * 注：.side-panel 隐藏依赖 transform: translateX(110%)，所以不能用
 * toBeHidden()（它只检查 display: none），要用 toHaveClass('hidden')。
 */
import { test, expect, chromium, Page, Browser } from "@playwright/test";

const UI_BASE = process.env.UI_BASE_URL ?? "http://localhost:4567";

/** 触发 #log-btn 的 click 事件：避开 Playwright 真实点击的可视性检查。 */
async function clickLogBtn(page: Page): Promise<void> {
  await page.evaluate(() => {
    const btn = document.getElementById("log-btn") as HTMLButtonElement | null;
    btn?.click();
  });
}

/** 关闭 #log-panel：直接触发 #log-close。 */
async function closeLogPanel(page: Page): Promise<void> {
  await page.evaluate(() => {
    const btn = document.getElementById("log-close") as HTMLButtonElement | null;
    btn?.click();
  });
}

/** 在 #log-file-list 中触发第 idx 个条目的 dblclick。 */
async function dblClickLogItem(page: Page, idx: number): Promise<void> {
  await page.evaluate((i: number) => {
    const items = document.querySelectorAll("#log-file-list .log-file-item");
    const target = items[i] as HTMLElement | undefined;
    target?.dispatchEvent(new MouseEvent("dblclick", { bubbles: true }));
  }, idx);
}

test.describe("日志面板", () => {
  let browser: Browser;
  let page: Page;

  test.beforeAll(async () => {
    browser = await chromium.launch({ headless: true, args: ["--no-sandbox", "--disable-dev-shm-usage"] });
  });

  test.beforeEach(async () => {
    page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
    await page.goto(UI_BASE, { waitUntil: "domcontentloaded", timeout: 15_000 });
    await page.waitForSelector("#log-btn", { timeout: 10_000 });
    // 等主脚本完全初始化：UI 控制器、SSE 连接、log 面板挂载都就绪
    await page.waitForTimeout(2000);
  });

  test.afterEach(async () => {
    await page.close();
  });

  test.afterAll(async () => {
    await browser.close();
  });

  test("点击日志按钮显示日志面板", async () => {
    const panel = page.locator("#log-panel");
    await expect(panel).toHaveClass(/hidden/);

    await clickLogBtn(page);
    await page.waitForTimeout(500);

    await expect(panel).not.toHaveClass(/hidden/);

    const fileList = page.locator("#log-file-list .log-file-item");
    await expect(fileList.first()).toBeVisible({ timeout: 5_000 });
    const count = await fileList.count();
    expect(count).toBeGreaterThan(0);
    console.log(`[test] 找到 ${count} 个日志文件`);

    const logContent = page.locator("#log-content .log-line");
    const contentCount = await logContent.count();
    console.log(`[test] 第一个文件显示 ${contentCount} 行`);
  });

  test("双击文件切换日志内容", async () => {
    await clickLogBtn(page);
    await page.waitForSelector("#log-file-list .log-file-item", { timeout: 5_000 });
    await page.waitForTimeout(500);

    const fileItems = page.locator("#log-file-list .log-file-item");
    const count = await fileItems.count();
    expect(count).toBeGreaterThan(1);

    await dblClickLogItem(page, 1);
    await page.waitForTimeout(500);

    const status = page.locator("#log-status");
    const statusText = await status.textContent();
    expect(statusText).toContain("ui-");
    console.log(`[test] 双击后状态: ${statusText}`);

    const lines = page.locator("#log-content .log-line");
    const lineCount = await lines.count();
    expect(lineCount).toBeGreaterThan(0);
  });

  test("关闭按钮隐藏日志面板", async () => {
    await clickLogBtn(page);
    await page.waitForTimeout(500);
    const panel = page.locator("#log-panel");
    await expect(panel).not.toHaveClass(/hidden/);

    await closeLogPanel(page);
    await page.waitForTimeout(500);
    await expect(panel).toHaveClass(/hidden/);
  });

  test("查看'查看当前文件夹路径'卡住时的日志", async () => {
    await clickLogBtn(page);
    await page.waitForSelector("#log-file-list .log-file-item", { timeout: 5_000 });
    await page.waitForTimeout(500);

    // 找一个包含了"查看当前文件夹路径"的日志文件
    const fileItems = page.locator("#log-file-list .log-file-item");
    const count = await fileItems.count();
    let foundIdx = -1;
    for (let i = 0; i < count; i++) {
      const name = await fileItems.nth(i).locator(".log-file-name").textContent();
      if (name?.includes("1784574568707")) {
        foundIdx = i;
        break;
      }
    }
    expect(foundIdx).toBeGreaterThanOrEqual(0);
    await dblClickLogItem(page, foundIdx);
    console.log(`[test] 找到目标日志文件，索引 ${foundIdx}`);

    await page.waitForTimeout(500);

    const content = await page.locator("#log-content").textContent();
    expect(content).toContain("UserMessage");
    expect(content).toContain("查看当前文件夹路径");
    expect(content).toContain("RoleStarted");
    expect(content).not.toContain("RoleFinished");
    console.log("[test] ✓ 日志清晰显示：用户消息发出后卡在 RoleStarted");
  });
});
