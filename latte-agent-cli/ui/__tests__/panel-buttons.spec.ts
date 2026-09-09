/**
 * Playwright 回归：侧边面板按钮点一下就能把对应面板打开。
 *
 * 为什么单独立一条：这件事曾被**误判成应用 bug**。实测现象是
 * "点 #models-btn 面板不开、且没有任何 JS 报错"，看起来像 handler 没绑；
 * 真因是**点得太早**——`#chat-input` 出现不代表 mount 完成，面板按钮的
 * click handler 在那之后才绑，早点的 click 落在裸按钮上被静默丢弃。
 *
 * 修法是给 mount 完成加确定性标记（`body[data-ui-mounted="1"]`，见
 * `src/main.ts`），测试等它而不是 `waitForTimeout` 硬等。这条用例同时
 * 锁住两件事：标记存在，且标记之后所有面板按钮都真的可用。
 */
import { test, expect, chromium, Page, Browser } from "@playwright/test";

const UI_BASE = process.env.UI_BASE_URL ?? "http://localhost:4567";

/** (按钮 id, 面板 id) —— 顶栏上每个开面板的按钮。 */
const PANELS: ReadonlyArray<readonly [string, string]> = [
  ["models-btn", "models-panel"],
  ["tools-btn", "tools-panel"],
  ["log-btn", "log-panel"],
];

test.describe("侧边面板按钮", () => {
  let browser: Browser;
  let page: Page;

  test.beforeAll(async () => {
    browser = await chromium.launch({
      headless: true,
      args: ["--no-sandbox", "--disable-dev-shm-usage"],
    });
  });

  test.afterAll(async () => {
    await browser.close();
  });

  test.beforeEach(async () => {
    page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
    await page.goto(UI_BASE, { waitUntil: "domcontentloaded", timeout: 15000 });
  });

  test("mount 完成标记存在（e2e 的确定性等待信号）", async () => {
    await expect(page.locator('body[data-ui-mounted="1"]')).toHaveCount(1, {
      timeout: 15000,
    });
  });

  test("mount 完成后，每个面板按钮都能打开对应面板", async () => {
    await page.waitForSelector('body[data-ui-mounted="1"]', { timeout: 15000 });

    // 注意：`.hidden` 在这套 CSS 里是 `transform: translateX(110%)`，**不是**
    // `display:none` —— 元素仍在布局里、Playwright 判定为 "visible"，只是被
    // 推出视口。所以开合状态一律以 **class** 为准，不用 toBeVisible/
    // toBeHidden（实测后者会把带 .hidden 的面板判成可见）。
    const isOpen = (panelId: string) =>
      page.locator(`#${panelId}`).evaluate((el) => !el.classList.contains("hidden"));

    for (const [btnId, panelId] of PANELS) {
      expect(await isOpen(panelId), `${panelId} 初始应关闭`).toBe(false);

      await page.locator(`#${btnId}`).click();
      await expect
        .poll(() => isOpen(panelId), {
          message: `点 #${btnId} 应打开 #${panelId}`,
          timeout: 5000,
        })
        .toBe(true);

      // 关掉，避免面板互相遮挡影响下一轮点击。
      await page.locator(`#${panelId.replace("-panel", "-close")}`).click();
      await expect
        .poll(() => isOpen(panelId), {
          message: `点关闭应收起 #${panelId}`,
          timeout: 5000,
        })
        .toBe(false);
    }
  });

  test("mount 未完成时点击会丢失——这就是必须等标记的原因", async () => {
    // 刻意不等标记，立刻点。这条用例记录的是**为什么要等**：
    // 若将来 handler 改成在 DOM 就绪时同步绑定，这条会失败，届时可以
    // 删掉它并简化其它用例的等待——那是好事，说明脆弱性被消除了。
    const clickedTooEarly = await page.evaluate(() => {
      const btn = document.getElementById("models-btn");
      const panel = document.getElementById("models-panel");
      if (!btn || !panel) return null;
      const mounted = document.body.dataset.uiMounted === "1";
      btn.dispatchEvent(new MouseEvent("click", { bubbles: true }));
      return { mounted, stillHidden: panel.classList.contains("hidden") };
    });
    // 只在确实"还没 mount"时断言点击无效；若这台机器快到已经 mount 完，
    // 那这条不适用，跳过而不是误报。
    if (clickedTooEarly && !clickedTooEarly.mounted) {
      expect(
        clickedTooEarly.stillHidden,
        "mount 前的点击应当无效（这正是要等 data-ui-mounted 的理由）",
      ).toBe(true);
    }
    // 等标记之后一定能开。
    await page.waitForSelector('body[data-ui-mounted="1"]', { timeout: 15000 });
    await page.locator("#models-btn").click();
    await expect
      .poll(
        () =>
          page
            .locator("#models-panel")
            .evaluate((el) => !el.classList.contains("hidden")),
        { message: "等标记后点击应能打开面板", timeout: 5000 },
      )
      .toBe(true);
  });
});
