/**
 * Playwright e2e：自动上下文压缩设置面板的**显示**与**修改生效**。
 *
 * 覆盖三件事（对应用户能感知的行为）：
 *   1. 面板能打开、输入框显示的是后端算出的**生效值**（不是空白）；
 *   2. 改完点「保存到项目」有成功反馈、且真的写了配置文件；
 *   3. 重新打开面板读到的是**改后的值**——这是"修改生效"的证明
 *      （值经过 后端写盘 → 内存 merged → resolver 快照 → GET 回读 一整圈）。
 *
 * 前置：先起服务 `latte-agent ui`（默认 :4567）。
 */
import { test, expect, chromium, Page, Browser } from "@playwright/test";
import * as fs from "fs";

const UI_BASE = process.env.UI_BASE_URL ?? "http://localhost:4567";

test.describe("自动上下文压缩设置面板", () => {
  let browser: Browser;
  let page: Page;
  /** 记录测试前的生效值，跑完还原，避免污染开发者本机配置。 */
  let original: Record<string, string> = {};

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
    // 等**mount 完成**再操作。`#chat-input` 出现不代表 mount 完了——面板
    // 按钮的 click handler 是在那之后才绑的，早点会静默丢失（实测：
    // domcontentloaded/load 之后立刻点 #models-btn，面板不开且无报错）。
    await page.waitForSelector('body[data-ui-mounted="1"]', { timeout: 15000 });
  });

  /** 打开模型面板并展开压缩设置块——走**真实点击**，与用户操作一致。 */
  async function openCompactionPanel(): Promise<void> {
    const panel = page.locator("#models-panel");
    if (await panel.evaluate((el) => el.classList.contains("hidden"))) {
      await page.locator("#models-btn").click();
    }
    await expect(panel).toBeVisible({ timeout: 10000 });

    // <details> 默认折叠，点 summary 展开（同样是真实点击）。
    const details = page.locator("#compaction-setting");
    await expect(details).toBeVisible({ timeout: 10000 });
    if (!(await details.evaluate((el) => (el as HTMLDetailsElement).open))) {
      await details.locator("summary").click();
    }
    await page.waitForFunction(
      () => {
        const el = document.getElementById("compaction-threshold") as HTMLInputElement | null;
        return !!el && el.value.trim().length > 0;
      },
      { timeout: 10000 },
    );
  }

  test("面板显示后端算出的生效值", async () => {
    await openCompactionPanel();

    // 输入框不是空白 —— 它显示的是 GET /api/compaction 的 effective。
    const threshold = await page.locator("#compaction-threshold").inputValue();
    const keepTurns = await page.locator("#compaction-keep-turns").inputValue();
    const summaryMax = await page.locator("#compaction-summary-max").inputValue();
    const maxPasses = await page.locator("#compaction-max-passes").inputValue();
    original = { threshold, keepTurns, summaryMax, maxPasses };

    expect(Number(threshold)).toBeGreaterThanOrEqual(0.5);
    expect(Number(threshold)).toBeLessThanOrEqual(0.95);
    expect(Number(keepTurns)).toBeGreaterThanOrEqual(1);
    expect(Number(summaryMax)).toBeGreaterThanOrEqual(256);
    expect(Number(maxPasses)).toBeGreaterThanOrEqual(1);

    // 徽标要么报参数、要么报已关闭，不能是空的（空 = 没渲染）。
    const badge = await page.locator("#compaction-effective-badge").textContent();
    expect((badge ?? "").trim().length).toBeGreaterThan(0);

    // 路径行要给出两层写入目标，供用户知道改的是哪个文件。
    const paths = await page.locator("#compaction-paths").textContent();
    expect(paths ?? "").toContain("compaction.toml");
  });

  test("修改后保存到项目：有成功反馈、写盘、且重开面板读到新值", async () => {
    await openCompactionPanel();

    const before = await page.locator("#compaction-keep-turns").inputValue();
    // 取一个与当前不同的合法值（1..=32），确保断言不会因"本来就是这个数"而假过。
    const target = String(Number(before) === 7 ? 5 : 7);

    await page.locator("#compaction-keep-turns").fill(target);
    await page.locator("#compaction-threshold").fill("0.75");
    await page.locator("#compaction-save-project").click();

    // 1. 成功反馈（措辞里必须提到生效范围，避免用户以为要重启）
    await expect(page.locator("#compaction-status")).toContainText("已保存", {
      timeout: 10000,
    });
    const status = await page.locator("#compaction-status").textContent();
    expect(status ?? "").toContain("生效");

    // 2. 真的写了文件（路径由面板自己报出来，不猜）
    const pathsText = (await page.locator("#compaction-paths").textContent()) ?? "";
    const m = pathsText.match(/项目：(\S+compaction\.toml)/);
    expect(m, `路径行应含项目文件: ${pathsText}`).not.toBeNull();
    const projectFile = m![1];
    expect(fs.existsSync(projectFile), `${projectFile} 应已写入`).toBe(true);
    const raw = fs.readFileSync(projectFile, "utf8");
    expect(raw).toContain("[compaction]");
    expect(raw).toContain(`keep_recent_turns = ${target}`);

    // 3. **修改生效**：重开面板（重新 GET）读到的是新值
    await page.reload({ waitUntil: "domcontentloaded" });
    await page.waitForSelector('body[data-ui-mounted="1"]', { timeout: 15000 });
    await openCompactionPanel();
    expect(await page.locator("#compaction-keep-turns").inputValue()).toBe(target);
    expect(Number(await page.locator("#compaction-threshold").inputValue())).toBeCloseTo(0.75, 5);

    // 还原（不污染本机配置）：把首个测试记录的原值写回。
    if (original.keepTurns) {
      await page.locator("#compaction-keep-turns").fill(original.keepTurns);
      await page.locator("#compaction-threshold").fill(original.threshold);
      await page.locator("#compaction-save-project").click();
      await expect(page.locator("#compaction-status")).toContainText("已保存", {
        timeout: 10000,
      });
    }
  });

  test("被环境变量接管的字段应标灰并说明", async () => {
    await openCompactionPanel();
    // env_overrides 由后端上报；面板据此 disable 输入框并写 title。
    // 本机通常没设这些 env，此时所有框都可编辑——两种情况都要自洽：
    // disabled 的必须有解释性 title，可编辑的必须没有。
    const rows = await page.evaluate(() => {
      const ids = [
        "compaction-enabled",
        "compaction-threshold",
        "compaction-keep-turns",
        "compaction-summary-max",
        "compaction-max-passes",
        "compaction-sections",
      ];
      return ids.map((id) => {
        const el = document.getElementById(id) as HTMLInputElement | null;
        return { id, disabled: !!el?.disabled, title: el?.title ?? "" };
      });
    });
    for (const r of rows) {
      if (r.disabled) {
        expect(r.title, `${r.id} 被禁用就必须说明原因`).toContain("环境变量");
      } else {
        expect(r.title, `${r.id} 可编辑时不该有 env 提示`).not.toContain("环境变量");
      }
    }
  });
});
