/**
 * 工具面板回归验证：
 * 1) 查看态只显示渲染后的 markdown，两个 textarea 必须 display:none；
 * 2) 编辑态反过来，view 必须 display:none（历史 bug：CSS 只有旧类名
 *    `.tool-doc-view/.tool-doc-editor` 的 `.hidden` 规则，且没有全局
 *    `.hidden{display:none}`，四个节点永远同时可见）；
 * 3) 列表里的描述不能是 `builtin tool: x` 占位串；
 * 4) 曾经点不到的 code_graph / doc_* / playwright 必须能列出且有详情。
 */
import { test, expect, chromium, Browser, Page } from "@playwright/test";

const UI_BASE = process.env.UI_BASE_URL ?? "http://localhost:4599";

let browser: Browser;
let page: Page;

test.beforeAll(async () => {
  browser = await chromium.launch({ headless: true, args: ["--no-sandbox"] });
  page = await browser.newPage({ viewport: { width: 1400, height: 950 } });
  await page.goto(UI_BASE, { waitUntil: "domcontentloaded" });
  // main() 是异步的，#chat-input 在静态 HTML 里就有，等它不代表 mount 完成。
  await page.waitForSelector("#chat-input");
  await expect
    .poll(async () => page.evaluate(() => {
      (document.getElementById("tools-btn") as HTMLElement).click();
      return document.querySelectorAll(".tools-card").length;
    }), { timeout: 20_000 })
    .toBeGreaterThan(0);
});

test.afterAll(async () => { await browser.close(); });

const openDetail = async (id: string) => {
  await page.locator(`.tools-card[data-tool-id="${id}"] .tools-card-body`).click();
  await page.waitForFunction(() => !document.querySelector(".tool-doc-loading"), undefined, { timeout: 10_000 });
};
const closeDetail = async () => {
  await page.locator("#tool-detail-close").click();
  await page.waitForFunction(
    () => document.getElementById("tool-detail-overlay")!.classList.contains("hidden"),
  );
};
const displays = () => page.evaluate(() => {
  const d = (s: string) => {
    const e = document.querySelector(s);
    return e ? getComputedStyle(e).display : "MISSING";
  };
  return {
    summaryView: d(".tool-doc-summary-view"),
    detailView: d(".tool-doc-detail-view"),
    summaryEditor: d(".tool-doc-summary-editor"),
    detailEditor: d(".tool-doc-detail-editor"),
  };
});

test("view/edit modes are mutually exclusive", async () => {
  await openDetail("read");
  expect(await displays()).toEqual({
    summaryView: "block", detailView: "block",
    summaryEditor: "none", detailEditor: "none",
  });

  await page.locator(".tool-doc-btn", { hasText: "编辑" }).first().click();
  await page.waitForTimeout(200);
  expect(await displays()).toEqual({
    summaryView: "none", detailView: "none",
    summaryEditor: "inline-block", detailEditor: "inline-block",
  });

  await page.locator(".tool-doc-btn", { hasText: "取消" }).first().click();
  await page.waitForTimeout(200);
  expect(await displays()).toEqual({
    summaryView: "block", detailView: "block",
    summaryEditor: "none", detailEditor: "none",
  });
  await closeDetail();
});

test("点「编辑」时简介预填当前显示的那句，不是空框", async () => {
  // 回归：编辑框原来只填磁盘 md 的简介，没写过 md 时显示的是内置描述首句、
  // 编辑框却是空的 —— 点一下「编辑」就把看到的那句话弄丢了。
  for (const id of ["read", "grep"]) {
    await openDetail(id);
    const shown = ((await page.locator(".tool-doc-summary-view").textContent()) ?? "").trim();
    expect(shown.length, `${id} 简介为空`).toBeGreaterThan(0);
    await page.locator(".tool-doc-btn", { hasText: "编辑" }).first().click();
    const prefilled = (await page.locator(".tool-doc-summary-editor").inputValue()).trim();
    expect(prefilled, `${id} 编辑框没预填`).toBe(shown);
    await page.locator(".tool-doc-btn", { hasText: "取消" }).first().click();
    await closeDetail();
  }
});

test("no placeholder or wall-of-text descriptions in the list", async () => {
  const cards = await page.locator(".tools-card").evaluateAll((els) =>
    els.map((e) => ({
      id: (e as HTMLElement).dataset.toolId ?? "?",
      desc: (e.querySelector(".tools-card-desc")?.textContent ?? "").trim(),
      title: e.querySelector(".tools-card-desc")?.getAttribute("title") ?? "",
    })),
  );
  const placeholders = cards
    .filter((x) => x.desc === "" || /^builtin tool:|^dynamic tool registered via/.test(x.desc))
    .map((x) => `${x.id}::${x.desc}`);
  expect(placeholders).toEqual([]);
  // 卡片上只放一行；完整描述留在 title 里，超长由 CSS 省略号收尾。
  const multiline = cards.filter((x) => x.desc.includes("\n")).map((x) => x.id);
  expect(multiline).toEqual([]);
  const read = cards.find((x) => x.id === "read")!;
  expect(read.title.length).toBeGreaterThan(read.desc.length);
  const clamp = await page.locator(".tools-card-desc").first().evaluate((e) => {
    const cs = getComputedStyle(e);
    return { whiteSpace: cs.whiteSpace, overflow: cs.overflow, textOverflow: cs.textOverflow };
  });
  expect(clamp).toEqual({ whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" });
});

test("brief stays one sentence and 详情 falls back to the builtin description", async () => {
  // read 有项目 md：简介来自 md 的 SUMMARY，详情两段都在。
  await openDetail("read");
  const readBrief = (await page.locator(".tool-doc-summary-view").textContent()) ?? "";
  expect([...readBrief.trim()].length).toBeLessThanOrEqual(60);
  await expect(page.locator(".tool-doc-builtin-view")).toHaveCount(1);
  expect(((await page.locator(".tool-doc-detail-view").textContent()) ?? "").length).toBeGreaterThan(100);
  await closeDetail();

  // grep 没有 md：简介取注册描述首句，详情退回注册描述余下部分——
  // 修复前这里是「一大段简介 + 空白详情」。
  await openDetail("grep");
  const grepBrief = ((await page.locator(".tool-doc-summary-view").textContent()) ?? "").trim();
  expect(grepBrief).toBe("AST structural code search.");
  const grepBuiltin = ((await page.locator(".tool-doc-builtin-view").textContent()) ?? "").trim();
  expect(grepBuiltin).toContain("26+ languages");
  await closeDetail();
});

test("previously unreachable tools are listed with their docs", async () => {
  for (const id of ["code_graph", "doc_graph_scan", "doc_graph_context", "doc_index", "doc_write", "playwright", "request_tool"]) {
    await expect(page.locator(`.tools-card[data-tool-id="${id}"]`)).toHaveCount(1);
  }
  for (const id of ["code_graph", "doc_write", "playwright"]) {
    await openDetail(id);
    const detail = (await page.locator(".tool-doc-detail-view").textContent()) ?? "";
    expect(detail.length, `${id} 详情为空`).toBeGreaterThan(100);
    expect(detail).not.toContain("还没有项目文档");
    await closeDetail();
  }
  // 幽灵条目不该再出现
  await expect(page.locator('.tools-card[data-tool-id="request"]')).toHaveCount(0);
});
