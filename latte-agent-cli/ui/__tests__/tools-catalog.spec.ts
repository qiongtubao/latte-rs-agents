/**
 * 工具面板的数据结构 / 文档编辑 / MCP / 模板 端到端验证。
 *
 * 前置：`latte-agent ui` 已启动（默认 :4567，用 UI_BASE_URL 覆盖）。
 * 用例 3 会通过面板的「测试」通道连一个 stub MCP server（需要 node），
 * 连不上就跳过——它验证的是「外部工具能进面板并能写文档」。
 */
import { test, expect, chromium, Browser, Page } from "@playwright/test";
import { writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

const UI_BASE = process.env.UI_BASE_URL ?? "http://localhost:4567";

const STUB_MCP = `
const lines = require("readline").createInterface({ input: process.stdin });
lines.on("line", (line) => {
  let req; try { req = JSON.parse(line); } catch { return; }
  const reply = (result) => process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: req.id, result }) + "\\n");
  if (req.method === "initialize") return reply({ protocolVersion: "0.1.0" });
  if (req.method === "tools/list") return reply({ tools: [{
      name: "e2e_probe_tool", description: "e2e 用的探针工具。",
      inputSchema: { type: "object", properties: { q: { type: "string" } }, required: ["q"] } }] });
  if (req.method === "tools/call") return reply({ ok: true });
  reply({});
});
`;

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

test.beforeAll(async () => {
  browser = await chromium.launch({ headless: true, args: ["--no-sandbox"] });
  page = await browser.newPage({ viewport: { width: 1400, height: 950 } });
  // 「删除文档」带 window.confirm，playwright 默认 dismiss。
  page.on("dialog", (d) => void d.accept());
  await page.goto(UI_BASE, { waitUntil: "domcontentloaded" });
  await page.waitForSelector("#chat-input");
  await openPanel();
});

test.afterAll(async () => {
  await browser.close();
});

test("每张卡片都带 label / 分类 / 文档状态三类元数据", async () => {
  const cards = await page.locator(".tools-card").evaluateAll((els) =>
    els.map((e) => ({
      id: (e as HTMLElement).dataset.toolId ?? "?",
      badges: [...e.querySelectorAll(".tools-card-badge")].map((b) => (b.textContent ?? "").trim()),
      desc: (e.querySelector(".tools-card-desc")?.textContent ?? "").trim(),
    })),
  );
  expect(cards.length).toBeGreaterThan(20);
  for (const card of cards) {
    // 状态 + 类型 + 文档状态
    expect(card.badges.length, `${card.id} 徽章不全: ${card.badges}`).toBeGreaterThanOrEqual(3);
    expect(
      card.badges.some((b) => ["无文档", "项目文档", "全局文档"].includes(b)),
      `${card.id} 缺文档状态徽章: ${card.badges}`,
    ).toBe(true);
    expect(card.desc, `${card.id} 描述为空`).not.toBe("");
    expect(card.desc).not.toMatch(/^builtin tool:|^dynamic tool registered via/);
  }
  // 「缺文档」筛选能把没写说明的工具集中列出来
  await page.locator(".tools-doc-select").selectOption("without");
  const missing = await page.locator(".tools-card").count();
  await page.locator(".tools-doc-select").selectOption("with");
  const withDoc = await page.locator(".tools-card").count();
  await page.locator(".tools-doc-select").selectOption("all");
  expect(missing + withDoc).toBe(cards.length);
  expect(withDoc).toBeGreaterThan(0);
});

test("文档编辑器保存后立刻读回，并回执热更新了几个会话", async () => {
  await openDetail("todo");
  await page.locator(".tool-doc-btn", { hasText: "编辑" }).first().click();
  const stamp = `e2e-${Date.now()}`;
  await page.locator(".tool-doc-summary-editor").fill(`${stamp} 简介`);
  await page.locator(".tool-doc-detail-editor").fill(`${stamp} 详情正文`);
  await page.locator(".tool-doc-btn", { hasText: "保存" }).first().click();

  const status = page.locator(".tool-doc-status");
  await expect(status).toContainText("已保存到", { timeout: 10_000 });
  await expect(status).toContainText(/已热更新到 \d+ 个活跃会话|新建 session 即生效/);
  // 视图立刻显示新内容
  await expect(page.locator(".tool-doc-summary-view")).toContainText(`${stamp} 简介`);
  await expect(page.locator(".tool-doc-detail-view")).toContainText(`${stamp} 详情正文`);
  await closeDetail();

  // 卡片徽章跟着从「无文档」变成「项目文档」
  await expect(
    page.locator('.tools-card[data-tool-id="todo"] .badge-doc-yes'),
  ).toHaveCount(1);

  // 重新打开：磁盘上读回来的内容逐字一致
  await openDetail("todo");
  await expect(page.locator(".tool-doc-summary-view")).toContainText(`${stamp} 简介`);
  await expect(page.locator(".tool-doc-detail-view")).toContainText(`${stamp} 详情正文`);
  await closeDetail();
});

test("模板文档：识别语法、给出渲染预览和可用变量", async () => {
  await openDetail("todo");
  await page.locator(".tool-doc-btn", { hasText: "编辑" }).first().click();
  await page
    .locator(".tool-doc-detail-editor")
    .fill("{{#if has_read}}READ-ON{{else}}READ-OFF{{/if}} @ {{CWD}}");
  await page.locator(".tool-doc-btn", { hasText: "保存" }).first().click();
  await expect(page.locator(".tool-doc-status")).toContainText("已保存到", { timeout: 10_000 });
  await closeDetail();

  await openDetail("todo");
  const templateBox = page.locator(".tool-doc-template");
  await expect(templateBox.locator("summary")).toContainText("模板：已启用");
  // 渲染预览走 has_read=true 分支，不能把两个分支都吐出来
  const preview = templateBox.locator(".tool-doc-builtin-view");
  await expect(preview).toContainText("READ-ON");
  await expect(preview).not.toContainText("READ-OFF");
  await expect(templateBox.locator(".tool-doc-template-ctx")).toContainText("{{cwd}}");
  // 编辑缓冲仍是模板原文（不能把渲染结果写回磁盘）
  await page.locator(".tool-doc-btn", { hasText: "编辑" }).first().click();
  await expect(page.locator(".tool-doc-detail-editor")).toHaveValue(/\{\{#if has_read\}\}/);
  await page.locator(".tool-doc-btn", { hasText: "取消" }).first().click();
  await closeDetail();
});

test("外部 MCP 工具进面板并可写文档", async () => {
  // 注意：这个用例必须排在「删除文档」之前跑（playwright 按声明顺序执行），
  // 由后者统一清理写入的文档。

  const script = path.join(tmpdir(), `latte-e2e-mcp-${Date.now()}.js`);
  writeFileSync(script, STUB_MCP, "utf8");

  // 借面板的「测试」通道连 server（它走的就是运行时那套 tool manager）
  const connected = await page.evaluate(async (cmd) => {
    const resp = await fetch("/api/tools/test", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ tool_id: "mcp_connect", args: { command: cmd } }),
    });
    return (await resp.json()) as { ok: boolean; response?: string; error?: string };
  }, `node ${script}`);
  test.skip(!connected.ok, `MCP stub 连不上（需要 node）：${connected.error ?? ""}`);
  expect(connected.response).toContain("e2e_probe_tool");

  await page.locator("#tools-refresh").click();
  const card = page.locator('.tools-card[data-tool-id="e2e_probe_tool"]');
  await expect(card).toHaveCount(1, { timeout: 10_000 });
  await expect(card.locator(".badge-kind")).toHaveText("外部 MCP");
  // 已连接的 server 列在面板顶部
  await expect(page.locator(".tools-mcp-bar")).toContainText(script);

  await openDetail("e2e_probe_tool");
  // MCP server 报的描述就是简介
  await expect(page.locator(".tool-doc-summary-view")).toContainText("e2e 用的探针工具");
  // 概览里要标出这个工具来自哪个 MCP server（同一 spec 反复跑时脚本名带时间戳，
  // 所以只匹配固定前缀）。
  await expect(
    page.locator(".tool-detail-value").filter({ hasText: "latte-e2e-mcp" }),
  ).toHaveCount(1);
  // 也能给外部工具写文档
  await page.locator(".tool-doc-btn", { hasText: "编辑" }).first().click();
  await page.locator(".tool-doc-detail-editor").fill("外部工具的调用约定：先确认参数。");
  await page.locator(".tool-doc-btn", { hasText: "保存" }).first().click();
  await expect(page.locator(".tool-doc-status")).toContainText("已保存到", { timeout: 10_000 });
  await closeDetail();
});

test("删除文档：回到只有内置描述的状态（顺带清理本 spec 的写入）", async () => {
  await openDetail("todo");
  // 前面两个用例给 todo 写过文档，这里删掉：既验删除，也让 spec 自清理。
  await page.locator('.tool-doc-btn-danger[data-target="project"]').click();
  const status = page.locator(".tool-doc-status");
  await expect(status).toContainText("项目文档已删除", { timeout: 10_000 });
  await expect(page.locator(".tool-doc-detail-view")).toContainText("还没有项目文档");
  await expect(page.locator(".tool-doc-btn-danger")).toHaveCount(0);
  await closeDetail();
  await expect(page.locator('.tools-card[data-tool-id="todo"] .badge-doc-no')).toHaveCount(1);

  // MCP 探针工具的文档也删掉，保持项目 .latte/tools.d 干净。
  // 走 API 而不是点按钮：按钮文案按层变（「删除项目文档」/「删除全局文档」），
  // 之前用 hasText:"删除文档" 匹配不到，静默跳过 → 仓库里留下了 e2e 产物。
  const removed = await page.evaluate(async () => {
    const results: number[] = [];
    for (const target of ["project", "global"]) {
      const resp = await fetch(`/api/tools/e2e_probe_tool/doc?target=${target}`, {
        method: "DELETE",
      });
      results.push(resp.status);
    }
    return results;
  });
  expect(removed).toEqual([200, 200]);
});
