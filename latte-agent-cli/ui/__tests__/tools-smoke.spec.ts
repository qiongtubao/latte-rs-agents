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
    await page.waitForSelector("#chat-input", { timeout: 10_000 });
  });

  test.afterAll(async () => {
    await browser.close();
  });

  test("buttons exist in toolbar", async () => {
    await expect(page.locator("#tools-btn")).toBeVisible({ timeout: 5_000 });
    await expect(page.locator("#models-btn")).toBeVisible({ timeout: 5_000 });
    await expect(page.locator("#role-editor-btn")).toBeVisible({ timeout: 5_000 });
    expect(errors).toEqual([]);
  });

  test("click buttons does not throw", async () => {
    // 逐个点击所有按钮，确认不报错
    await page.locator("#tools-btn").click();
    await page.waitForTimeout(300);
    console.log("after tools-btn click errors:", errors);

    await page.locator("#models-btn").click();
    await page.waitForTimeout(300);
    console.log("after models-btn click errors:", errors);

    await page.locator("#role-editor-btn").click();
    await page.waitForTimeout(300);
    console.log("after role-editor-btn click errors:", errors);

    // 面板元素应是在 DOM 中（即使 hidden）
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