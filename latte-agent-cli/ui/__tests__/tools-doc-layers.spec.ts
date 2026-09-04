/**
 * 工具文档的**分层读写**：与 models / roles 面板同一套语义 —— 读项目层/全局层，
 * 保存到对应文件，项目层覆盖全局层。
 *
 * 前置：服务必须用隔离的 `LATTE_HOME` 启动，否则用例会写到真实的
 * `~/.latte/tools.d/`。跑法见 spec 末尾注释。
 */
import { test, expect, chromium, Browser, Page } from "@playwright/test";

const UI_BASE = process.env.UI_BASE_URL ?? "http://localhost:4567";
/** 拿来做实验的工具：项目里没有它的文档，不会动到仓库里那 18 份。 */
const TOOL = "todo";

let browser: Browser;
let page: Page;

const openPanel = async () => {
  await expect
    .poll(
      async () =>
        page.evaluate(() => {
          (document.getElementById("tools-btn") as HTMLElement).click();
          return document.querySelectorAll(".tools-card").length;
        }),
      { timeout: 20_000 },
    )
    .toBeGreaterThan(0);
};

const openDetail = async (id: string) => {
  await page.locator(`.tools-card[data-tool-id="${id}"] .tools-card-body`).click();
  await page.waitForFunction(() => !document.querySelector(".tool-doc-loading"), undefined, {
    timeout: 10_000,
  });
};

const closeDetail = async () => {
  await page.locator("#tool-detail-close").click();
  await page.waitForFunction(() =>
    document.getElementById("tool-detail-overlay")!.classList.contains("hidden"),
  );
};

/** 直接调 API 清场，避免用例互相污染。 */
const wipe = async (id: string) => {
  await page.evaluate(async (tool) => {
    for (const target of ["project", "global"]) {
      await fetch(`/api/tools/${tool}/doc?target=${target}`, { method: "DELETE" });
    }
  }, id);
};

const layerChips = () =>
  page.locator(".tool-doc-layer-chip").evaluateAll((els) =>
    els.map((e) => ({
      text: (e.textContent ?? "").trim(),
      exists: e.classList.contains("layer-exists"),
      active: e.classList.contains("layer-active"),
    })),
  );

test.beforeAll(async () => {
  browser = await chromium.launch({ headless: true, args: ["--no-sandbox"] });
  page = await browser.newPage({ viewport: { width: 1500, height: 1000 } });
  page.on("dialog", (d) => void d.accept());
  await page.goto(UI_BASE, { waitUntil: "domcontentloaded" });
  await page.waitForSelector("#chat-input");
  await openPanel();
  await wipe(TOOL);
});

test.afterAll(async () => {
  await wipe(TOOL);
  await browser.close();
});

test("两层都空时：分层条显示「无」，保存按钮给出两个落点", async () => {
  await openDetail(TOOL);
  const chips = await layerChips();
  expect(chips.map((c) => c.text)).toEqual(["项目层：无", "全局层：无"]);
  expect(chips.some((c) => c.exists || c.active)).toBe(false);

  await page.locator(".tool-doc-btn", { hasText: "编辑" }).first().click();
  await expect(page.locator('.tool-doc-btn[data-target="project"]')).toHaveText("保存到项目");
  await expect(page.locator('.tool-doc-btn[data-target="global"]')).toHaveText("保存到全局");
  await page.locator(".tool-doc-btn", { hasText: "取消" }).first().click();
  await closeDetail();
});

test("保存到全局：写全局文件、生效层是全局、项目层仍为空", async () => {
  await openDetail(TOOL);
  await page.locator(".tool-doc-btn", { hasText: "编辑" }).first().click();
  await page.locator(".tool-doc-summary-editor").fill("全局简介");
  await page.locator(".tool-doc-detail-editor").fill("全局详情正文");
  await page.locator('.tool-doc-btn[data-target="global"]').click();
  await expect(page.locator(".tool-doc-status")).toContainText("已保存到全局层", {
    timeout: 10_000,
  });
  await closeDetail();

  await openDetail(TOOL);
  await expect(page.locator(".tool-doc-badge").first()).toHaveText("全局文件");
  await expect(page.locator(".tool-doc-detail-view")).toContainText("全局详情正文");
  const chips = await layerChips();
  expect(chips[0]).toEqual({ text: "项目层：无", exists: false, active: false });
  expect(chips[1]).toEqual({ text: "全局层：生效中", exists: true, active: true });
  // 正在看全局文档时，主保存按钮就该指向全局，不能悄悄写成项目副本
  await page.locator(".tool-doc-btn", { hasText: "编辑" }).first().click();
  await expect(page.locator(".tool-doc-btn.tool-doc-btn-primary")).toHaveText("保存到全局");
  await page.locator(".tool-doc-btn", { hasText: "取消" }).first().click();
  await closeDetail();
});

test("再保存到项目：项目层覆盖全局层，两层并存", async () => {
  await openDetail(TOOL);
  await page.locator(".tool-doc-btn", { hasText: "编辑" }).first().click();
  await page.locator(".tool-doc-detail-editor").fill("项目详情正文");
  await page.locator('.tool-doc-btn[data-target="project"]').click();
  await expect(page.locator(".tool-doc-status")).toContainText("已保存到项目层", {
    timeout: 10_000,
  });
  await closeDetail();

  await openDetail(TOOL);
  await expect(page.locator(".tool-doc-detail-view")).toContainText("项目详情正文");
  const chips = await layerChips();
  expect(chips[0]).toEqual({ text: "项目层：生效中", exists: true, active: true });
  expect(chips[1]).toEqual({ text: "全局层：被覆盖", exists: true, active: false });
  // 两层都有文档 → 两个删除按钮
  await expect(page.locator(".tool-doc-btn-danger")).toHaveCount(2);
  await closeDetail();
});

test("删项目层：回落到全局文档，而不是变成「无文档」", async () => {
  await openDetail(TOOL);
  await page.locator('.tool-doc-btn-danger[data-target="project"]').click();
  await expect(page.locator(".tool-doc-status")).toContainText("项目文档已删除", {
    timeout: 10_000,
  });
  // 重画后生效层回落到全局，内容也换回全局那份
  await expect(page.locator(".tool-doc-badge").first()).toHaveText("全局文件");
  await expect(page.locator(".tool-doc-detail-view")).toContainText("全局详情正文");
  const chips = await layerChips();
  expect(chips[0].exists).toBe(false);
  expect(chips[1]).toEqual({ text: "全局层：生效中", exists: true, active: true });

  // 再删全局层 → 彻底没文档，回到只有内置描述
  await page.locator('.tool-doc-btn-danger[data-target="global"]').click();
  await expect(page.locator(".tool-doc-status")).toContainText("全局文档已删除", {
    timeout: 10_000,
  });
  await expect(page.locator(".tool-doc-detail-view")).toContainText("还没有项目文档");
  await expect(page.locator(".tool-doc-btn-danger")).toHaveCount(0);
  await closeDetail();
  await expect(page.locator(`.tools-card[data-tool-id="${TOOL}"] .badge-doc-no`)).toHaveCount(1);
});

/*
跑法（用隔离的全局目录，别污染 ~/.latte）：

  cd latte-rs-agents
  LATTE_HOME=/tmp/latte-e2e-home ./target/debug/latte-agent ui --port 4599
  cd latte-agent-cli/ui
  UI_BASE_URL=http://localhost:4599 pnpm exec playwright test tools-doc-layers
*/
