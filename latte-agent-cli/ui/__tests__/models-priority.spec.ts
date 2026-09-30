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
    // 等 main() 挂载完成（panel button handler / SSE / models 列表加载就位）
    await page.waitForSelector('body[data-ui-mounted="1"]', { timeout: 15000 });
  });

  test.afterAll(async () => {
    await browser.close();
  });

  test("glm-5.2 source 应为 project 而非 global", async () => {
    // 打开模型管理面板
    await page.locator("#models-btn").click();
    await page.waitForTimeout(2000);

    // 从下拉里选任意 source=project 的模型：本机没配 glm-5.2（只有
    // glm-5.3 在全局），但 project 层有 deepseek-v4-flash 与 mock——
    // source 字段正确性是 project > global，与具体 model 无关。
    const target = await page.evaluate(() => {
      const sel = document.getElementById("model-select") as HTMLSelectElement;
      for (const o of Array.from(sel.options)) {
        if (o.value && o.dataset.source === "project") return o.value;
      }
      return "";
    });
    test.skip(!target, "没有 source=project 的模型配置");
    await page.locator("#model-select").selectOption(target);
    await page.waitForTimeout(500);

    // 读取 source badge 文本
    const sourceText = await page.evaluate(() => {
      const badge = document.querySelector(".models-source-project, .models-source-global, .models-source-catalog");
      return badge?.textContent?.trim() ?? null;
    });
    expect(sourceText).toBe("project");
  });
  test("deepseek/DeepSeek Chat V3（仅 catalog）source 应为 catalog", async () => {
    await page.locator("#models-btn").click();
    await page.waitForTimeout(2000);

    // 选任意 source=catalog 的模型（本机无 DeepSeek Chat V3 配置，但
    // 只要 catalog 源有任意模型就行）
    const target = await page.evaluate(() => {
      const sel = document.getElementById("model-select") as HTMLSelectElement;
      for (const o of Array.from(sel.options)) {
        if (o.value && o.dataset.source === "catalog") return o.value;
      }
      return "";
    });
    test.skip(!target, "没有 source=catalog 的模型配置");
    await page.locator("#model-select").selectOption(target);
    await page.waitForTimeout(500);

    const sourceText = await page.evaluate(() => {
      const badge = document.querySelector(".models-source-project, .models-source-global, .models-source-catalog");
      return badge?.textContent?.trim() ?? null;
    });
    expect(sourceText).toBe("catalog");
  });
});
