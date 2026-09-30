/**
 * 极简 smoke 测试：验证新按钮存在并可点击（不检查面板 visible）。
 */
import { test, expect, chromium, Page, Browser } from "@playwright/test";

const UI_BASE = process.env.UI_BASE_URL ?? "http://localhost:4567";

test.describe("new buttons smoke test", () => {
  let browser: Browser;
  let page: Page;
  const errors: string[] = [];

  test.beforeAll(async () => {
    browser = await chromium.launch({ headless: true, args: ["--no-sandbox", "--disable-dev-shm-usage"] });
  });

  test.beforeEach(async () => {
    errors.length = 0;
    page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
    page.on("console", (msg) => { if (msg.type() === "error") errors.push(msg.text()); });
    page.on("pageerror", (err) => errors.push(err.message));
    await page.goto(UI_BASE, { waitUntil: "domcontentloaded", timeout: 15_000 });
    // 等 main() 挂载完成（panel button handler 绑定）再放行，
    // 否则 domcontentloaded 之后立刻点按钮会落在裸按钮上静默丢失。
    await page.waitForSelector('body[data-ui-mounted="1"]', { timeout: 15_000 });
    await page.waitForSelector("#chat-input", { timeout: 10_000 });
  });

  test.afterAll(async () => {
    await browser.close();
  });

  test("buttons exist in toolbar", async () => {
    await expect(page.locator("#tools-btn")).toBeVisible({ timeout: 5_000 });
    await expect(page.locator("#models-btn")).toBeVisible({ timeout: 5_000 });
    await expect(page.locator("#role-editor-btn")).toBeVisible({ timeout: 5_000 });
    // 过滤掉 404（favicon 等静态资源 404 不影响功能），只关心 JS 错误
    const realErrors = errors.filter(e => !/404|net::ERR_/i.test(e));
    expect(realErrors).toEqual([]);
  });
  test("click buttons does not throw", async () => {
    // 逐个点击所有按钮，确认不报错。每个面板 420px 宽、覆盖右半视口，
    // 先关上一个再开下一个，避免当前打开的面板拦截后续按钮的点击
    // （models-panel 的 .advisor-setting 是典型的 z-index 拦截点）。
    const openAndClose = async (openId: string, panelId: string): Promise<void> => {
      await page.locator(`#${openId}`).click();
      await expect(page.locator(`#${panelId}`)).not.toHaveClass(/hidden/);
      // 关掉面板（用 close 按钮；trace/self-loop 同理，没有 close 的就再点一次 openBtn 切换）
      const closeBtn = page.locator(`#${panelId} .panel-close`);
      if (await closeBtn.count()) await closeBtn.first().click();
      await expect(page.locator(`#${panelId}`)).toHaveClass(/hidden/);
    };

    await openAndClose("tools-btn", "tools-panel");
    await openAndClose("models-btn", "models-panel");
    await openAndClose("role-editor-btn", "role-editor-panel");

    // 面板元素应在 DOM 中（即使 hidden）
    await expect(page.locator("#tools-panel")).toBeAttached();
    await expect(page.locator("#models-panel")).toBeAttached();
    await expect(page.locator("#role-editor-panel")).toBeAttached();

    // 新建/删除按钮也应存在
    await expect(page.locator("#role-editor-new")).toBeAttached();
    await expect(page.locator("#role-editor-delete")).toBeAttached();
    await expect(page.locator("#models-new")).toBeAttached();
    await expect(page.locator("#tools-filter")).toBeAttached();
  });
});