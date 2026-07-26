# Instructions

- Following Playwright test failed.
- Explain why, be concise, respect Playwright best practices.
- Provide a snippet of code with the fix, if possible.

# Test info

- Name: self-debug.spec.ts >> latte-agent UI self-debug loop >> S3: @programmer 派发并验证
- Location: __tests__/self-debug.spec.ts:65:3

# Error details

```
Error: page.waitForTimeout: Target page, context or browser has been closed
```

```
Error: write EPIPE
```

# Test source

```ts
  1   | import { test, expect, chromium, type Page, type Browser } from "@playwright/test";
  2   | const UI_BASE = process.env.UI_BASE_URL ?? "http://localhost:4567";
  3   | 
  4   | test.describe("latte-agent UI self-debug loop", () => {
  5   |   let browser: Browser;
  6   |   let page: Page;
  7   | 
  8   |   test.beforeAll(async () => {
  9   |     browser = await chromium.launch({ headless: true, args: ["--no-sandbox", "--disable-dev-shm-usage"] });
  10  |   });
  11  |   test.beforeEach(async () => {
  12  |     page = await browser.newPage({ viewport: { width: 1400, height: 900 } });
  13  |     await page.goto(UI_BASE, { waitUntil: "domcontentloaded", timeout: 15000 });
> 14  |     await page.waitForTimeout(3000);
      |     ^ Error: write EPIPE
  15  |   });
  16  |   test.afterEach(async () => { await page.close(); });
  17  |   test.afterAll(async () => { await browser.close(); });
  18  | 
  19  |   test("S1: 打开页面截图并验证基础UI", async () => {
  20  |     const state = await page.evaluate(() => ({
  21  |       hasInput: !!document.getElementById("chat-input"),
  22  |       hasSendBtn: !!document.getElementById("chat-send"),
  23  |       hasStatusPill: !!document.getElementById("status-pill"),
  24  |       rolePill: document.getElementById("role-pill")?.textContent || "",
  25  |       statusLabel: document.querySelector(".status-label")?.textContent || "",
  26  |     }));
  27  |     expect(state.hasInput).toBe(true);
  28  |     expect(state.hasSendBtn).toBe(true);
  29  |     expect(state.hasStatusPill).toBe(true);
  30  |     expect(state.rolePill).toMatch(/[\u{1F000}-\u{1FFFF}]/u);
  31  |     expect(state.statusLabel).toBe("已连接");
  32  |   });
  33  | 
  34  |   test("S2: 发送消息并验证回复", async () => {
  35  |     await page.evaluate(() => {
  36  |       (document.getElementById("chat-input") as HTMLTextAreaElement).value = "查看当前目录";
  37  |       document.getElementById("chat-send")?.click();
  38  |     });
  39  |     for (let i = 0; i < 30; i++) {
  40  |       await page.waitForTimeout(5000);
  41  |       const done = await page.evaluate(() => {
  42  |         const label = document.querySelector(".status-label")?.textContent;
  43  |         return label === "已连接" || label === "已卡住";
  44  |       });
  45  |       if (done) break;
  46  |     }
  47  |     const state = await page.evaluate(() => {
  48  |       const msgs = Array.from(document.querySelectorAll(".message, .message-row, .message"));
  49  |       const lastRole = Array.from(document.querySelectorAll(".message-row.role, .message-row.self, .message-row.tool, .message-row.error")).pop();
  50  |       return {
  51  |         msgCount: msgs.length,
  52  |         hasAvatar: !!lastRole?.querySelector(".msg-avatar"),
  53  |         avatarIcon: lastRole?.querySelector(".msg-avatar")?.textContent || "",
  54  |         hasTime: msgs.some(m => m.textContent?.includes("⏱")),
  55  |         status: document.querySelector(".status-label")?.textContent || "",
  56  |       };
  57  |     });
  58  |     expect(state.msgCount).toBeGreaterThanOrEqual(2);
  59  |     expect(state.hasAvatar).toBe(true);
  60  |     expect(state.avatarIcon.length).toBeGreaterThan(0);
  61  |     expect(state.hasTime).toBe(true);
  62  |     expect(state.status).toBe("已连接");
  63  |   });
  64  | 
  65  |   test("S3: @programmer 派发并验证", async () => {
  66  |     await page.evaluate(() => {
  67  |       (document.getElementById("chat-input") as HTMLTextAreaElement).value = "@programmer 查看package.json";
  68  |       document.getElementById("chat-send")?.click();
  69  |     });
  70  |     for (let i = 0; i < 40; i++) {
  71  |       await page.waitForTimeout(5000);
  72  |       const done = await page.evaluate(() => {
  73  |         const label = document.querySelector(".status-label")?.textContent;
  74  |         return label === "已连接" || label === "已卡住";
  75  |       });
  76  |       if (done) break;
  77  |     }
  78  |     const state = await page.evaluate(() => {
  79  |       const msgs = Array.from(document.querySelectorAll(".message, .message-row, .message"));
  80  |       const programmer = Array.from(document.querySelectorAll(".message-row")).find(
  81  |         m => m.querySelector(".msg-avatar")?.textContent === "P"
  82  |       );
  83  |       return {
  84  |         delegated: msgs.some(m => m.textContent?.includes("🤝 @manager → @programmer")),
  85  |         replied: !!programmer,
  86  |         clickable: programmer?.hasAttribute("data-sub-id") || programmer?.style?.cursor === "pointer",
  87  |         rolePill: document.getElementById("role-pill")?.textContent || "",
  88  |         hasTime: msgs.some(m => m.textContent?.includes("⏱")),
  89  |       };
  90  |     });
  91  |     expect(state.delegated).toBe(true);
  92  |     expect(state.replied).toBe(true);
  93  |     expect(state.clickable).toBe(true);
  94  |     expect(state.hasTime).toBe(true);
  95  |   });
  96  | 
  97  |   test("S4: 全量功能快速验证", async () => {
  98  |     const roles = await page.evaluate(() => {
  99  |       const select = document.getElementById("role-select") as HTMLSelectElement;
  100 |       return { count: select.options.length, ids: Array.from(select.options).map(o => o.value) };
  101 |     });
  102 |     expect(roles.count).toBeGreaterThanOrEqual(10);
  103 |     expect(roles.ids).toContain("mcp_agent");
  104 |     expect(await page.evaluate(() => !!document.getElementById("subsession-panel"))).toBe(true);
  105 |   });
  106 | });
  107 | 
```