/**
 * Playwright e2e：任意深度任务树的看板渲染与交互验证。
 *
 * 前置：UI server 已启动（默认 :4567，可用 UI_BASE_URL 覆盖）。
 * 测试通过 REST API 种一棵 4 层任务树（根→子→孙→曾孙），然后验证：
 *   1. 看板递归渲染任意深度（.tb-tree-children 嵌套 3 层）
 *   2. 嵌套卡片带状态芯片（depth>0 不再依赖列位置）
 *   3. 展开/折叠开关可用
 *   4. 子任务抽屉里仍有「＋ 拆分子任务」入口（任意深度可再拆）
 *   5. 新建任务弹窗的父任务下拉包含多层任务（缩进显示）
 *
 * 注意：所有断言都限定在本测试种的那棵树的子树范围内（看板里可能
 * 有别的任务）；MARK 带时间戳保证多次运行不互相干扰。
 */

import { test, expect, chromium, Browser, Page, Locator } from "@playwright/test";

const UI_BASE = process.env.UI_BASE_URL ?? "http://localhost:4567";
const API = `${UI_BASE}/api`;
const MARK = `E2E树${Date.now()}`;

let ids: { root: string; child: string; grand: string; great: string };

async function createTask(title: string, parent_id?: string): Promise<string> {
  const resp = await fetch(`${API}/tasks`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ title, parent_id: parent_id ?? null }),
  });
  if (!resp.ok) throw new Error(`create ${title}: ${resp.status} ${await resp.text()}`);
  const task = (await resp.json()) as { id: string };
  // 置为 todo 上看板列
  const patch = await fetch(`${API}/tasks/${task.id}`, {
    method: "PATCH",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ state: "todo" }),
  });
  if (!patch.ok) throw new Error(`patch ${task.id}: ${patch.status}`);
  return task.id;
}

/** 根卡片 → 它的 tree-node 容器（子树作用域）。 */
function rootNode(page: Page): Locator {
  return page
    .locator(".tb-card", { hasText: `${MARK}-根` })
    .first()
    .locator("xpath=..");
}

test.describe("任务看板：任意深度任务树", () => {
  let browser: Browser;
  let page: Page;

  test.beforeAll(async () => {
    ids = {
      root: await createTask(`${MARK}-根`),
      child: "",
      grand: "",
      great: "",
    };
    ids.child = await createTask(`${MARK}-子`, ids.root);
    ids.grand = await createTask(`${MARK}-孙`, ids.child);
    ids.great = await createTask(`${MARK}-曾孙`, ids.grand);
    browser = await chromium.launch({ headless: true, args: ["--no-sandbox", "--disable-dev-shm-usage"] });
  });

  test.beforeEach(async () => {
    page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
    await page.goto(UI_BASE, { waitUntil: "domcontentloaded", timeout: 15_000 });
    // 等 SSE 连上（右上角「已连接」），此时 main.ts 已挂载完看板事件
    await page.waitForSelector("text=已连接", { timeout: 15_000 });
    // 「已连接」可能早于看板事件挂载完成：点开看板并容忍重试
    // （重复点击会切换面板开关，所以只在还没打开时重试）。
    for (let i = 0; i < 5; i++) {
      const panel = page.locator("#task-board-panel");
      if (await panel.isVisible().catch(() => false)) break;
      await page.click("#task-board-btn");
      await page.waitForTimeout(800);
    }
    await expect(page.locator("#task-board-panel")).toBeVisible();
    // 看板每 5s 自动刷新，留出首次加载余量
    await page.waitForSelector(`text=${MARK}-根`, { timeout: 15_000 });
  });

  test.afterEach(async () => {
    await page.close();
  });

  test.afterAll(async () => {
    await browser?.close();
  });

  test("递归渲染 4 层树（3 层嵌套容器）", async () => {
    const tree = rootNode(page);
    // 子/孙/曾孙 三层嵌套容器
    await expect(tree.locator(".tb-tree-children")).toHaveCount(3);
    // 每一层标题都渲染出来了
    await expect(tree.locator(".tb-card", { hasText: `${MARK}-子` })).toHaveCount(1);
    await expect(tree.locator(".tb-card", { hasText: `${MARK}-孙` })).toHaveCount(1);
    await expect(tree.locator(".tb-card", { hasText: `${MARK}-曾孙` })).toHaveCount(1);
    // 嵌套卡片（depth>0）带状态芯片
    const childCard = tree.locator(".tb-card", { hasText: `${MARK}-子` });
    await expect(childCard.locator(".tb-chip").first()).toBeVisible();
    // 根卡片聚合计数递归统计全部后代（0/3）
    await expect(tree.locator(".tb-card").first()).toContainText("0/3");
    await page.screenshot({ path: "test-results/task-tree-board.png", fullPage: false });
  });

  test("展开/折叠开关作用于子树", async () => {
    const tree = rootNode(page);
    const rootCard = tree.locator(".tb-card").first();
    const toggle = rootCard.locator(".tb-tree-toggle");
    await expect(toggle).toHaveText("▾ 1");

    await toggle.click(); // 折叠
    await expect(tree.locator(".tb-tree-children")).toHaveCount(0);
    await expect(toggle).toHaveText("▸ 1");

    await toggle.click(); // 再展开
    await expect(tree.locator(".tb-card", { hasText: `${MARK}-曾孙` })).toHaveCount(1);
  });

  test("子任务抽屉保留「拆分子任务」入口（任意深度可再拆）", async () => {
    // 打开孙任务（深度 2）的抽屉
    await rootNode(page).locator(".tb-card", { hasText: `${MARK}-孙` }).click();
    const drawer = page.locator(".tb-drawer.open");
    await expect(drawer).toBeVisible();
    // 子任务区 + 拆分按钮都在
    await expect(drawer.locator("text=＋ 拆分子任务")).toBeVisible();
    // 曾孙也列在抽屉的子任务区里
    await expect(drawer.locator(`text=${MARK}-曾孙`)).toBeVisible();
    await page.screenshot({ path: "test-results/task-tree-drawer.png", fullPage: false });
  });

  test("新建任务弹窗的父任务下拉包含多层任务", async () => {
    await page.click("#task-board-new");
    const select = page.locator("select").filter({ hasText: `${MARK}-孙` }).first();
    await expect(select).toBeVisible();
    // 孙任务（深度 2）也出现在候选里，且带缩进
    const optionText = await select.locator("option", { hasText: `${MARK}-孙` }).first().textContent();
    expect(optionText).toMatch(/^\s|^\u3000/);
  });
});
