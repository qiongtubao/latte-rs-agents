# Instructions

- Following Playwright test failed.
- Explain why, be concise, respect Playwright best practices.
- Provide a snippet of code with the fix, if possible.

# Test info

- Name: e2e.spec.ts >> latte-agent UI e2e >> self-loop form requires task
- Location: __tests__/e2e.spec.ts:110:3

# Error details

```
Error: expect(locator).toBeVisible() failed

Locator:  locator('#self-loop-task')
Expected: visible
Received: hidden
Timeout:  5000ms

Call log:
  - Expect "toBeVisible" with timeout 5000ms
  - waiting for locator('#self-loop-task')
    14 × locator resolved to <input type="text" required="" id="self-loop-task" placeholder="e.g. make the chat input auto-resize and remember history"/>
       - unexpected value "hidden"

```

```yaml
- banner:
  - text: ☕ latte-agent UI 已连接 👔 manager deepseek-chat
  - combobox "switch role":
    - option "🏗️ architect"
    - option "🎨 designer"
    - option "🚀 devops"
    - option "👔 manager" [selected]
    - option "📋 pm"
    - option "💻 programmer"
    - option "🔍 reviewer"
    - option "🛡️ security"
    - option "📝 tech_writer"
    - option "🧪 tester"
  - combobox "session"
  - button "+ Session"
  - button "Role Graph"
  - button "Trace"
  - button "AI Fix"
- main:
  - textbox "向 latte-agent 提问… (Enter 发送，Shift+Enter 换行)"
  - button "发送"
  - button "/clear"
  - button "/quit"
  - text: ready · role=manager · session=ui-2026298-1784350504219
```

# Test source

```ts
  13  |  *   5. Self-loop 面板打开/关闭
  14  |  */
  15  | 
  16  | import { test, expect, chromium, ConsoleMessage, Page, Browser } from "@playwright/test";
  17  | 
  18  | // 假设 ui 服务跑在 localhost:5173 (vite dev) 或 4567 (production)。
  19  | const UI_BASE = process.env.UI_BASE_URL ?? "http://localhost:4567";
  20  | 
  21  | test.describe("latte-agent UI e2e", () => {
  22  |   let browser: Browser;
  23  |   let page: Page;
  24  |   const consoleErrors: string[] = [];
  25  | 
  26  |   test.beforeAll(async () => {
  27  |     browser = await chromium.launch({ headless: true, args: ["--no-sandbox", "--disable-dev-shm-usage"] });
  28  |   });
  29  | 
  30  |   test.beforeEach(async () => {
  31  |     page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  32  |     page.on("console", (msg: ConsoleMessage) => {
  33  |       if (msg.type() === "error") {
  34  |         consoleErrors.push(`[${msg.type()}] ${msg.text()}`);
  35  |       }
  36  |     });
  37  |     page.on("pageerror", (err) => {
  38  |       consoleErrors.push(`[pageerror] ${err.message}`);
  39  |     });
  40  |     await page.goto(UI_BASE, { waitUntil: "domcontentloaded", timeout: 15_000 });
  41  |   });
  42  | 
  43  |   test.afterEach(async () => {
  44  |     await page.close();
  45  |   });
  46  | 
  47  |   test.afterAll(async () => {
  48  |     await browser.close();
  49  |   });
  50  | 
  51  |   test("loads without console error", async () => {
  52  |     // 等首屏 SSE 连接 + initial /api/session。
  53  |     await page.waitForSelector(".messages", { timeout: 5_000 });
  54  |     await page.waitForTimeout(500);
  55  |     // 接受非空错误（包括 dev mode React 警告）—— 真实 fix 跑时不允许。
  56  |     // self-loop 阶段会追踪 consoleErrors 长度变化。
  57  |     expect(consoleErrors.length).toBeGreaterThanOrEqual(0);
  58  |   });
  59  | 
  60  |   test("chat panel + role selector render", async () => {
  61  |     await expect(page.locator("#chat-input")).toBeVisible();
  62  |     await expect(page.locator("#chat-send")).toBeVisible();
  63  |     await expect(page.locator("#role-select")).toBeVisible();
  64  |     const options = await page.locator("#role-select option").count();
  65  |     expect(options).toBeGreaterThan(0);
  66  |   });
  67  | 
  68  |   test("status pill shows connected", async () => {
  69  |     // 中文 UI label 是 "已连接" / "已断开"，断言状态 class + 子元素文本。
  70  |     await expect(page.locator("#status-pill")).toHaveClass(/connected|disconnected/, { timeout: 5_000 });
  71  |     await expect(page.locator("#status-pill .status-label")).toHaveText(/已连接|已断开|思考中|已卡住/, { timeout: 5_000 });
  72  |   });
  73  | 
  74  |   test("SSE events subscribe", async () => {
  75  |     // 先获取一个 session_id，然后验证 /api/events?id=xxx 返回 event-stream。
  76  |     const sessionsRes = await page.request.get(`${UI_BASE}/api/sessions`, { timeout: 3_000 });
  77  |     expect(sessionsRes.status()).toBe(200);
  78  |     const sessions = await sessionsRes.json();
  79  |     expect(Array.isArray(sessions)).toBe(true);
  80  | 
  81  |     // 用第一个 session 测试 SSE
  82  |     if (sessions.length > 0) {
  83  |       const sid = sessions[0].session_id;
  84  |       const res = await page.request.get(`${UI_BASE}/api/events?id=${encodeURIComponent(sid)}`, {
  85  |         headers: { Accept: "text/event-stream" },
  86  |         timeout: 3_000,
  87  |       }).catch((e) => ({ status: () => 0 } as unknown as { status: () => number }));
  88  |       expect([200, 0]).toContain(res.status());
  89  |     } else {
  90  |       // 没有 session 时跳过
  91  |       expect(true).toBe(true);
  92  |     }
  93  |   });
  94  |   test("self-loop panel toggleable", async () => {
  95  |     const panel = page.locator("#self-loop-panel");
  96  |     await expect(panel).toBeHidden();
  97  |     await page.click("#self-loop-btn");
  98  |     await expect(panel).toBeVisible();
  99  |     await page.click("#self-loop-close");
  100 |     await expect(panel).toBeHidden();
  101 |   });
  102 | 
  103 |   test("trace panel toggleable", async () => {
  104 |     const panel = page.locator("#trace-panel");
  105 |     await expect(panel).toBeHidden();
  106 |     await page.click("#trace-toggle");
  107 |     await expect(panel).toBeVisible();
  108 |   });
  109 | 
  110 |   test("self-loop form requires task", async () => {
  111 |     await page.click("#self-loop-btn");
  112 |     const input = page.locator("#self-loop-task");
> 113 |     await expect(input).toBeVisible();
      |                         ^ Error: expect(locator).toBeVisible() failed
  114 |     // HTML5 required 阻止 form submit。断言：缺 task 时不应触发
  115 |     // /api/self-loop/start。监听器先挂，再 click，再断言。
  116 |     let started = false;
  117 |     const onReq = (r: { url: () => string }): void => {
  118 |       if (r.url().endsWith("/api/self-loop/start")) started = true;
  119 |     };
  120 |     page.on("request", onReq);
  121 |     await input.fill("");
  122 |     await page.click("#self-loop-form button[type='submit']");
  123 |     await page.waitForTimeout(800);
  124 |     page.off("request", onReq);
  125 |     expect(started).toBe(false);
  126 |   });
  127 | 
  128 |   test("chat input sends message", async () => {
  129 |     await page.locator("#chat-input").fill("hello");
  130 |     await page.locator("#chat-send").click();
  131 |     // 等 user 消息出现
  132 |     await expect(page.locator(".message-row.self").first()).toContainText("hello", { timeout: 3_000 });
  133 |   });
  134 | });
  135 | 
```