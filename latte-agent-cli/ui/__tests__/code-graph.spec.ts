/**
 * code_graph 一键可视化 e2e：
 *   1. /api/code-graph/mermaid 端点直接可用（结构图，轮询等索引构建完）；
 *   2. tools panel → code_graph 详情 → 「生成图」→ 结果容器出现 mermaid SVG；
 *   3. 调用图（ast-grep 实时扫描）对夹具目录产出 entry --> helper 的边。
 *
 * 需要已构建的 debug 二进制与最新 ui/dist；调用图测试需要 ast-grep 在 PATH
 * （不在则该用例自动跳过）。夹具 __cg_e2e__/ 在 beforeAll 写入、afterAll 清理。
 */
import { test, expect, chromium, Browser, Page } from "@playwright/test";
import { spawn, ChildProcess } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const PORT = 45696;
const BASE = `http://127.0.0.1:${PORT}`;

const HERE = path.dirname(fileURLToPath(import.meta.url));
const UI_DIR = path.resolve(HERE, "..");
const FIXTURE_DIR = path.join(UI_DIR, "__cg_e2e__");
const FIXTURE_SRC = [
  "export function helper(): number { return 1; }",
  "export function entry(): void { helper(); }",
  "",
].join("\n");

let server: ChildProcess;
let browser: Browser;
let page: Page;

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

/** 索引构建是后台异步的：轮询端点直到 200。 */
async function waitForMermaidApi(query: string, timeoutMs = 60_000): Promise<unknown> {
  const deadline = Date.now() + timeoutMs;
  let lastStatus = 0;
  while (Date.now() < deadline) {
    const r = await fetch(`${BASE}/api/code-graph/mermaid?${query}`);
    lastStatus = r.status;
    if (r.ok) return r.json();
    await new Promise((r2) => setTimeout(r2, 1000));
  }
  throw new Error(`code-graph/mermaid never became ready (last status ${lastStatus})`);
}

async function hasAstGrep(): Promise<boolean> {
  try {
    const { execFile } = await import("node:child_process");
    return await new Promise((resolve) => {
      execFile("ast-grep", ["--version"], (err) => resolve(!err));
    });
  } catch {
    return false;
  }
}

test.beforeAll(async () => {
  fs.mkdirSync(FIXTURE_DIR, { recursive: true });
  fs.writeFileSync(path.join(FIXTURE_DIR, "mod.ts"), FIXTURE_SRC);

  const bin = path.resolve(HERE, "../../../target/debug/latte-agent");
  server = spawn(bin, ["ui", "--port", String(PORT)], { stdio: "ignore" });
  await waitForHealth(`${BASE}/health`);

  browser = await chromium.launch({ headless: true, args: ["--no-sandbox"] });
  page = await browser.newPage({ viewport: { width: 1400, height: 950 } });
});

test.afterAll(async () => {
  await browser?.close();
  server?.kill();
  fs.rmSync(FIXTURE_DIR, { recursive: true, force: true });
});

test("API：结构图返回 mermaid 图源", async () => {
  const resp = (await waitForMermaidApi("path=src&graph=structure")) as {
    mermaid: string;
    truncated: boolean;
  };
  expect(resp.mermaid).toContain("flowchart TD");
  expect(resp.mermaid).toContain("subgraph");
});

test("API：graph 参数非法 → 400", async () => {
  const r = await fetch(`${BASE}/api/code-graph/mermaid?path=.&graph=bogus`);
  expect(r.status).toBe(400);
});

test("UI：tools panel 详情里一键生成结构图并渲染 SVG", async () => {
  await waitForMermaidApi("path=src&graph=structure"); // 先确保索引就绪
  await page.goto(BASE, { waitUntil: "domcontentloaded" });
  await page.waitForSelector("#chat-input", { timeout: 15_000 });

  // 与 tools-doc-panel.spec.ts 同模式：轮询中重复点击，直到 mount 完成、
  // 卡片真的渲染出来（单次 locator.click 可能打在 mount 之前）。
  await expect
    .poll(async () => page.evaluate(() => {
      (document.getElementById("tools-btn") as HTMLElement).click();
      return document.querySelectorAll(".tools-card").length;
    }), { timeout: 20_000 })
    .toBeGreaterThan(0);
  await page.locator('.tools-card[data-tool-id="code_graph"] .tools-card-body').click();
  await expect(page.locator("#tool-detail-overlay")).toBeVisible({ timeout: 10_000 });

  // 可视化区控件就位
  const section = page.locator(".code-graph-viz");
  await expect(section).toBeVisible();
  await section.locator(".code-graph-viz-path").fill("src");
  await section.locator("button", { hasText: "生成图" }).click();

  // mermaid 懒渲染出 SVG
  const result = section.locator(".code-graph-viz-result");
  await expect(result.locator(".mermaid svg")).toBeVisible({ timeout: 30_000 });
});

test("调用图：夹具模块产出 entry → helper 的边", async () => {
  if (!(await hasAstGrep())) {
    test.skip(true, "ast-grep 不在 PATH");
    return;
  }
  const resp = (await waitForMermaidApi("path=__cg_e2e__&graph=calls")) as {
    mermaid: string;
  };
  expect(resp.mermaid).toContain("flowchart LR");
  expect(resp.mermaid).toContain("fn entry");
  expect(resp.mermaid).toContain("fn helper");
  expect(resp.mermaid).toContain("-->");
});
