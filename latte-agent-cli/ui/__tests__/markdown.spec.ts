/**
 * Markdown 渲染能力 e2e（方案 A 验收）：
 * 拦截 SSE /api/events 注入一条 assistant 完整消息（RoleTurn is_complete），
 * 内容覆盖 mermaid 流程图、rust 代码块、GFM 表格、任务列表、引用块、标题，
 * 在真实浏览器里断言：
 *   1. .mermaid 占位 div 被懒渲染成 <svg>（jsdom 做不了，必须真实浏览器）；
 *   2. 代码块有 hljs 高亮 span + hover 工具条，点「复制」写入剪贴板；
 *   3. <table> / .task-list-item-checkbox / <blockquote> / <h3> 都渲染出来；
 *   4. 全程无 console error / pageerror。
 *
 * 需要已构建的 debug 二进制（target/debug/latte-agent）与最新 ui/dist。
 * 跑法：pnpm exec playwright test __tests__/markdown.spec.ts
 */
import { test, expect, chromium, Browser, BrowserContext, Page } from "@playwright/test";
import { spawn, ChildProcess } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const PORT = 45697;
const BASE = `http://127.0.0.1:${PORT}`;

const MD = [
  "# 渲染能力验收",
  "",
  "```mermaid",
  "graph TD;",
  "  A[开始] --> B[结束];",
  "```",
  "",
  "```rust",
  "fn main() {",
  '    let answer = 42;',
  "}",
  "```",
  "",
  "| 特性 | 状态 |",
  "|---|---|",
  "| mermaid | ✅ |",
  "| 表格 | ✅ |",
  "",
  "- [ ] 待办事项",
  "- [x] 已完成事项",
  "",
  "> 引用块：模型输出不可信，html:false。",
  "",
  "行内公式 $E=mc^2$ 与块级：",
  "$$",
  "\\int_0^1 x dx",
  "$$",
  "",
  "::: details 点开看细节",
  "折叠块里的 **粗体** 内容。",
  ":::",
  "",
  "带脚注的句子[^n1]。",
  "",
  "[^n1]: 这是脚注内容。",
  "",
  "![本地图](.latte/learn/__e2e__/img.png)",
  "",
  "[打开学习文档](.latte/learn/__e2e__/doc.md)",
].join("\n");

// e2e 夹具：服务端白名单目录 `.latte/learn/` 下的图片与 md 文档，
// beforeAll 写入、afterAll 清理。
const FIXTURE_DIR = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../.latte/learn/__e2e__",
);
const PNG_1PX = Buffer.from(
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==",
  "base64",
);
const FIXTURE_DOC = "# E2E 学习文档\n\n## 小节A\n\n正文内容。\n\n![文档内图](.latte/learn/__e2e__/img.png)\n";

let server: ChildProcess;
let browser: Browser;
let context: BrowserContext;
let page: Page;
const consoleErrors: string[] = [];

async function waitForHealth(url: string, timeoutMs = 30_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const r = await fetch(url);
      if (r.ok) return;
    } catch {
      /* not up yet */
    }
    await new Promise((r) => setTimeout(r, 300));
  }
  throw new Error(`server did not become healthy: ${url}`);
}

test.beforeAll(async () => {
  fs.mkdirSync(FIXTURE_DIR, { recursive: true });
  fs.writeFileSync(path.join(FIXTURE_DIR, "img.png"), PNG_1PX);
  fs.writeFileSync(path.join(FIXTURE_DIR, "doc.md"), FIXTURE_DOC);

  const here = path.dirname(fileURLToPath(import.meta.url));
  const bin = path.resolve(here, "../../../target/debug/latte-agent");
  server = spawn(bin, ["ui", "--port", String(PORT)], { stdio: "ignore" });
  await waitForHealth(`${BASE}/health`);

  browser = await chromium.launch({ headless: true, args: ["--no-sandbox"] });
  context = await browser.newContext({ viewport: { width: 1400, height: 950 } });
  await context.grantPermissions(["clipboard-read", "clipboard-write"], { origin: BASE });
  page = await context.newPage();
  page.on("console", (msg) => {
    if (msg.type() === "error") consoleErrors.push(msg.text());
  });
  page.on("pageerror", (err) => consoleErrors.push(`[pageerror] ${err.message}`));

  // 拦截聊天事件流：每次（重）连都重放同一条完整 assistant 消息。
  // 断流后 UI 会 resync 清屏再重放历史（真实服务端历史里没有这条），
  // 重连时本路由再次投递同一事件，气泡会重新渲染 —— 断言用
  // auto-retry 语义，容忍这个闪烁。
  await page.route("**/api/events**", (route) =>
    route.fulfill({
      status: 200,
      contentType: "text/event-stream",
      headers: { "Cache-Control": "no-cache" },
      body:
        `event: chat_event\n` +
        `data: ${JSON.stringify({
          type: "RoleTurn",
          role_id: "manager",
          content: MD,
          is_complete: true,
        })}\n\n`,
    }),
  );

  await page.goto(BASE, { waitUntil: "domcontentloaded" });
  await page.waitForSelector("#chat-input", { timeout: 15_000 });
});

test.afterAll(async () => {
  await browser?.close();
  server?.kill();
  fs.rmSync(FIXTURE_DIR, { recursive: true, force: true });
});

test("mermaid 围栏懒渲染为 SVG", async () => {
  const mermaid = page.locator(".msg-content .mermaid").first();
  await expect(mermaid).toBeVisible({ timeout: 15_000 });
  await expect(mermaid.locator("svg")).toBeVisible({ timeout: 30_000 });
  // 渲染完成后打上完成标记，且不是错误回退
  await expect(mermaid).toHaveAttribute("data-mermaid-done", "1");
  await expect(mermaid).not.toHaveClass(/mermaid-error/);
});

test("代码块：hljs 高亮 + 复制按钮写剪贴板", async () => {
  const code = page.locator(".msg-content pre code.hljs.language-rust").first();
  await expect(code).toBeVisible({ timeout: 15_000 });
  await expect(code.locator("span.hljs-keyword").first()).toBeVisible();

  const block = page.locator(".msg-content .code-block").first();
  await block.hover();
  const copyBtn = block.locator(".code-block-btn", { hasText: "复制" });
  await expect(copyBtn).toBeVisible();
  await copyBtn.click();
  await expect(copyBtn).toHaveText("已复制", { timeout: 3_000 });
  const clip = await page.evaluate(() => navigator.clipboard.readText());
  expect(clip).toContain("let answer = 42;");
});

test("表格 / 任务列表 / 引用块 / 标题渲染", async () => {
  // 定位注入消息所在的气泡（含 mermaid 的那个），而不是 .last()——
  // resync 重放期间可能有别的气泡排在后面。
  const content = page.locator(".msg-content:has(.mermaid)").first();
  await expect(content.locator("h3")).toHaveText("渲染能力验收", { timeout: 15_000 });
  await expect(content.locator("table th").first()).toHaveText("特性");
  await expect(content.locator("table td").first()).toHaveText("mermaid");
  await expect(content.locator(".task-list-item-checkbox")).toHaveCount(2);
  await expect(content.locator(".task-list-item-checkbox").nth(1)).toBeChecked();
  await expect(content.locator("blockquote")).toContainText("模型输出不可信");
});

test("全程无 console error", async () => {
  // 等前三个断言涉及的内容都出现过再收尾判断
  await expect(page.locator(".msg-content .mermaid svg").first()).toBeVisible({ timeout: 30_000 });
  const real = consoleErrors.filter(
    (e) => !e.includes("EventSource") && !e.includes("net::ERR"),
  );
  expect(real).toEqual([]);
});

test("数学公式渲染为 KaTeX", async () => {
  const content = page.locator(".msg-content:has(.mermaid)").first();
  await expect(content.locator(".katex").first()).toBeVisible({ timeout: 15_000 });
  await expect(content.locator(".katex-display")).toBeVisible();
});

test("::: details 折叠块可展开", async () => {
  const content = page.locator(".msg-content:has(.mermaid)").first();
  const details = content.locator("details.md-details");
  await expect(details).toBeVisible({ timeout: 15_000 });
  await expect(details).not.toHaveAttribute("open", "");
  await details.locator("summary").click();
  await expect(details).toHaveAttribute("open", "");
  await expect(details).toContainText("折叠块里的");
});

test("脚注渲染为脚注区", async () => {
  const content = page.locator(".msg-content:has(.mermaid)").first();
  await expect(content.locator(".footnote-ref a")).toBeVisible({ timeout: 15_000 });
  await expect(content.locator(".footnotes")).toContainText("这是脚注内容");
});

test("本地图片走 /api/doc-asset 且真实加载", async () => {
  const img = page.locator('.msg-content img[src^="/api/doc-asset"]').first();
  await expect(img).toBeVisible({ timeout: 15_000 });
  await expect
    .poll(async () => img.evaluate((el: HTMLImageElement) => el.naturalWidth), { timeout: 10_000 })
    .toBeGreaterThan(0);
});

test("本地 .md 链接打开内嵌文档预览（含 TOC 与文档内图片）", async () => {
  const content = page.locator(".msg-content:has(.mermaid)").first();
  const link = content.locator("a.doc-link", { hasText: "打开学习文档" });
  await expect(link).toBeVisible({ timeout: 15_000 });
  await link.click();

  const viewer = page.locator(".doc-viewer");
  await expect(viewer).toBeVisible({ timeout: 10_000 });
  await expect(viewer.locator(".doc-viewer-path")).toHaveText(".latte/learn/__e2e__/doc.md");
  await expect(viewer.locator(".doc-viewer-content h3")).toHaveText("E2E 学习文档");
  // TOC：# → h3、## → h4 两个条目
  await expect(viewer.locator(".doc-viewer-toc-item")).toHaveCount(2);
  // 文档内的本地图片同样经 doc-asset 加载
  const docImg = viewer.locator('.doc-viewer-content img[src^="/api/doc-asset"]');
  await expect(docImg).toBeVisible();
  await expect
    .poll(async () => docImg.evaluate((el: HTMLImageElement) => el.naturalWidth), { timeout: 10_000 })
    .toBeGreaterThan(0);

  await viewer.locator('[data-act="close"]').click();
  await expect(viewer).toBeHidden();
});

test("mermaid 点击放大弹窗：缩放容器与下载按钮", async () => {
  const mermaid = page.locator(".msg-content .mermaid").first();
  await expect(mermaid.locator("svg")).toBeVisible({ timeout: 30_000 });
  await mermaid.locator("svg").click();
  const modal = page.locator(".md-modal-overlay.mermaid-modal");
  await expect(modal).toBeVisible({ timeout: 5_000 });
  await expect(modal.locator('[data-act="copy-src"]')).toBeVisible();
  await expect(modal.locator('[data-act="dl-svg"]')).toBeVisible();
  await expect(modal.locator('[data-act="dl-png"]')).toBeVisible();
  await expect(modal.locator(".mermaid-canvas svg")).toBeVisible();
  await modal.locator('[data-act="close"]').click();
  await expect(modal).toBeHidden();
});
