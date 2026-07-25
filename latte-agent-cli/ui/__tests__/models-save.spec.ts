/**
 * Playwright e2e 测试：验证模型管理面板的保存功能。
 */
import { test, expect, chromium, Page, Browser } from "@playwright/test";
import * as fs from "fs";
import * as path from "path";

const UI_BASE = process.env.UI_BASE_URL ?? "http://localhost:4567";
const MODELS_DIR = process.env.MODELS_DIR ?? "/home/dong/Documents/latte/latte-rs-agents/.latte/models.d";

test.describe("模型管理保存功能", () => {
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

  test("保存到项目应该创建 .toml 文件", async () => {
    await page.locator("#models-btn").click();
    // 等 model 列表加载完成
    await page.waitForTimeout(5000);

    // 获取 options
    const opts = await page.evaluate(() => {
      const sel = document.getElementById("model-select") as HTMLSelectElement;
      return Array.from(sel.options).map(o => ({ value: o.value, text: o.text }));
    });
    const validOpts = opts.filter(o => o.value !== "");
    console.log("model options:", validOpts);
    expect(validOpts.length).toBeGreaterThan(0);

    // 选第一个
    await page.locator("#model-select").selectOption(validOpts[0].value);
    await page.waitForTimeout(1000);

    // 滚动到按钮
    await page.locator("#models-body").evaluate(el => el.scrollTop = el.scrollHeight);
    await page.waitForTimeout(500);

    // 点"保存到项目"
    const saveBtn = page.locator(".models-form-actions button.primary").first();
    const btnText = await saveBtn.textContent();
    console.log("save button text:", btnText);
    await saveBtn.click();
    await page.waitForTimeout(3000);

    const status = await page.locator("#models-status").textContent();
    console.log("save status:", status);
    expect(status).toContain("已保存");

    const afterFiles = fs.readdirSync(MODELS_DIR).filter(f => f.endsWith(".toml"));
    console.log("files after save:", afterFiles);
    expect(afterFiles.length).toBeGreaterThan(0);
  });

  test("修改字段后保存应该更新文件内容", async () => {
    await page.locator("#models-btn").click();
    await page.waitForTimeout(5000);

    const opts = await page.evaluate(() => {
      const sel = document.getElementById("model-select") as HTMLSelectElement;
      return Array.from(sel.options).map(o => ({ value: o.value, text: o.text }));
    });
    const validOpts = opts.filter(o => o.value !== "");
    expect(validOpts.length).toBeGreaterThan(0);

    await page.locator("#model-select").selectOption(validOpts[0].value);
    await page.waitForTimeout(1000);
    await page.locator("#models-body").evaluate(el => el.scrollTop = el.scrollHeight);
    await page.waitForTimeout(500);

    // 修改 max_tokens
    const numInputs = await page.locator(".models-form-number input[type='number']").all();
    expect(numInputs.length).toBeGreaterThanOrEqual(2);
    await numInputs[1].click();
    await numInputs[1].fill("");
    await numInputs[1].fill("12345");
    await page.waitForTimeout(300);

    await page.locator(".models-form-actions button.primary").first().click();
    await page.waitForTimeout(3000);

    const status = await page.locator("#models-status").textContent();
    console.log("save status:", status);
    expect(status).toContain("已保存");

    // 读文件验证
    const tomlFiles = fs.readdirSync(MODELS_DIR).filter(f => f.endsWith(".toml"));
    expect(tomlFiles.length).toBeGreaterThan(0);
    const latestToml = tomlFiles.sort()[tomlFiles.length - 1];
    const content = fs.readFileSync(path.join(MODELS_DIR, latestToml), "utf-8");
    console.log(`content of ${latestToml}:`);
    console.log(content);
    expect(content).toContain("max_tokens = 12345");
  });
});