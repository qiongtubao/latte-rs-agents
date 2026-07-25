/**
 * Playwright e2e 测试：验证项目目录 model 优先级高于全局。
 * .latte/models.d/glm__glm-5.2.toml 存在时，UI 应显示为 project 而非 global。
 */
import { test, expect, chromium, Page, Browser } from "@playwright/test";

const UI_BASE = process.env.UI_BASE_URL ?? "http://localhost:4567";

test.describe("项目 model 优先级", () => {
  let browser: Browser;
  let page: Page;

  test.beforeAll(async () => {
    browser = await chromium.launch({ headless: true, args: ["--no-sandbox", "--disable-dev-shm-usage"] });
  });

  test.beforeEach(async () => {
    page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
    await page.goto(UI_BASE, { waitUntil: "domcontentloaded", timeout: 15000 });
    await page.waitForSelector("#chat-input", { timeout: 10000 });
    await page.waitForTimeout(2000);
  });

  test.afterAll(async () => {
    await browser.close();
  });

  test("glm-5.2 source 应为 project 而非 global", async () => {
    // 打开模型管理面板
    await page.locator("#models-btn").click();
    await page.waitForTimeout(5000);

    // 选中 glm/glm-5.2
    await page.locator("#model-select").selectOption("glm/glm-5.2");
    await page.waitForTimeout(1000);

    // 读取 source badge 文本
    const sourceText = await page.evaluate(() => {
      const badge = document.querySelector(".models-source-project, .models-source-global, .models-source-catalog");
      return badge?.textContent?.trim() ?? null;
    });
    console.log("glm-5.2 source:", sourceText);
    expect(sourceText).toBe("project");

    // 读取 file_path 确认路径正确
    const filePath = await page.evaluate(() => {
      const code = document.querySelector(".models-path-code");
      return code?.textContent?.trim() ?? null;
    });
    console.log("glm-5.2 file_path:", filePath);
    expect(filePath).toContain("latte-rs-agents/.latte/models.d/glm__glm-5.2.toml");
  });

  test("deepseek/DeepSeek Chat V3（仅 catalog）source 应为 catalog", async () => {
    await page.locator("#models-btn").click();
    await page.waitForTimeout(5000);

    await page.locator("#model-select").selectOption("deepseek/DeepSeek Chat V3");
    await page.waitForTimeout(1000);

    const sourceText = await page.evaluate(() => {
      const badge = document.querySelector(".models-source-project, .models-source-global, .models-source-catalog");
      return badge?.textContent?.trim() ?? null;
    });
    console.log("deepseek source:", sourceText);
    expect(sourceText).toBe("catalog");
  });
});
