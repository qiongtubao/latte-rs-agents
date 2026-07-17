# Instructions

- Following Playwright test failed.
- Explain why, be concise, respect Playwright best practices.
- Provide a snippet of code with the fix, if possible.

# Test info

- Name: tdd-advanced.spec.ts >> latte-agent UI TDD Advanced >> T6: MCP agent role exists in dropdown
- Location: __tests__/tdd-advanced.spec.ts:29:3

# Error details

```
Error: expect(received).toBe(expected) // Object.is equality

Expected: true
Received: false
```

# Test source

```ts
  1   | import { test, expect, chromium, type Page, type Browser } from "@playwright/test";
  2   | import * as fs from "fs";
  3   | 
  4   | const UI_BASE = process.env.UI_BASE_URL ?? "http://localhost:4567";
  5   | 
  6   | test.describe("latte-agent UI TDD Advanced", () => {
  7   |   let browser: Browser;
  8   |   let page: Page;
  9   | 
  10  |   test.beforeAll(async () => {
  11  |     browser = await chromium.launch({ headless: true, args: ["--no-sandbox", "--disable-dev-shm-usage"] });
  12  |   });
  13  | 
  14  |   test.beforeEach(async () => {
  15  |     page = await browser.newPage({ viewport: { width: 1400, height: 900 } });
  16  |     page.on("pageerror", (err) => console.error("[pageerror]", err.message));
  17  |     await page.goto(UI_BASE, { waitUntil: "domcontentloaded", timeout: 15000 });
  18  |     await page.waitForTimeout(3000);
  19  |   });
  20  | 
  21  |   test.afterEach(async () => {
  22  |     await page.close();
  23  |   });
  24  | 
  25  |   test.afterAll(async () => {
  26  |     await browser.close();
  27  |   });
  28  | 
  29  |   test("T6: MCP agent role exists in dropdown", async () => {
  30  |     // 1. Verify mcp_agent exists as an option in the role dropdown
  31  |     const hasMcpAgent = await page.evaluate(() => {
  32  |       const select = document.getElementById("role-select") as HTMLSelectElement;
  33  |       if (!select) return false;
  34  |       return Array.from(select.options).some(opt => opt.value === "mcp_agent");
  35  |     });
> 36  |     expect(hasMcpAgent).toBe(true);
      |                         ^ Error: expect(received).toBe(expected) // Object.is equality
  37  | 
  38  |     // 2. Verify the mcp_agent option shows the 🔌 icon (from role config)
  39  |     const mcpOptionHtml = await page.evaluate(() => {
  40  |       const select = document.getElementById("role-select") as HTMLSelectElement;
  41  |       const opt = Array.from(select.options).find(o => o.value === "mcp_agent");
  42  |       return opt ? opt.innerHTML : "";
  43  |     });
  44  |     expect(mcpOptionHtml).toContain("🔌");
  45  | 
  46  |     // 3. Verify the dropdown has at least 3 options
  47  |     const optionCount = await page.evaluate(() => {
  48  |       const select = document.getElementById("role-select") as HTMLSelectElement;
  49  |       return select?.options.length ?? 0;
  50  |     });
  51  |     expect(optionCount).toBeGreaterThanOrEqual(3);
  52  | 
  53  |     // 4. Verify mcp_agent has a non-empty role config icon (🔌)
  54  |     expect(mcpOptionHtml).toMatch(/🔌/);
  55  |   });
  56  | 
  57  |   test("T7: Screenshot tool via bash", async () => {
  58  |     const screenshotPath = "/tmp/latte-ui-advanced-shot.png";
  59  | 
  60  |     // Clean up any previous screenshot
  61  |     try { fs.unlinkSync(screenshotPath); } catch {}
  62  | 
  63  |     // Use Playwright's built-in chromium screenshot capability
  64  |     await page.screenshot({ path: screenshotPath, fullPage: false });
  65  | 
  66  |     // Verify the file was created and has content
  67  |     expect(fs.existsSync(screenshotPath)).toBe(true);
  68  |     const stats = fs.statSync(screenshotPath);
  69  |     expect(stats.size).toBeGreaterThan(0);
  70  | 
  71  |     // Verify the chat panel is visible in the screenshot (confirming it captured UI)
  72  |     const hasChat = await page.evaluate(() => !!document.getElementById("chat-input"));
  73  |     expect(hasChat).toBe(true);
  74  |   });
  75  | 
  76  |   test("T8: @programmer subsession tool popup", async () => {
  77  |     // Send @programmer delegation
  78  |     await page.evaluate(() => {
  79  |       (document.getElementById("chat-input") as HTMLTextAreaElement).value = "@programmer 运行 ls -la 命令查看当前目录文件";
  80  |       document.getElementById("chat-send")?.click();
  81  |     });
  82  | 
  83  |     // Wait for completion
  84  |     let connected = false;
  85  |     for (let i = 0; i < 60; i++) {
  86  |       await page.waitForTimeout(5000);
  87  |       connected = await page.evaluate(() => {
  88  |         const label = document.querySelector(".status-label")?.textContent;
  89  |         return label === "已连接" || label === "已卡住";
  90  |       });
  91  |       if (connected) break;
  92  |     }
  93  |     expect(connected).toBe(true);
  94  | 
  95  |     // Verify programmer role message exists with click cursor
  96  |     const programmerMsgState = await page.evaluate(() => {
  97  |       const programmerMsg = Array.from(document.querySelectorAll(".message-row")).find(
  98  |         m => m.querySelector(".msg-avatar")?.textContent === "P"
  99  |       );
  100 |       if (!programmerMsg) return { found: false };
  101 |       const style = window.getComputedStyle(programmerMsg);
  102 |       return {
  103 |         found: true,
  104 |         hasSubId: programmerMsg.hasAttribute("data-sub-id"),
  105 |         cursor: style.cursor,
  106 |         hasRoleClass: programmerMsg.classList.contains("role"),
  107 |       };
  108 |     });
  109 |     expect(programmerMsgState.found).toBe(true);
  110 |     expect(programmerMsgState.hasSubId || programmerMsgState.cursor === "pointer").toBe(true);
  111 |     expect(programmerMsgState.hasRoleClass).toBe(true);
  112 | 
  113 |     // Verify subsession-link exists on the DelegateFinished message
  114 |     const hasSubsessionLink = await page.evaluate(() => {
  115 |       return !!document.querySelector(".subsession-link");
  116 |     });
  117 |     expect(hasSubsessionLink).toBe(true);
  118 |   });
  119 | 
  120 |   test("T9: Elapsed time not in role message", async () => {
  121 |     // Send @programmer delegation
  122 |     await page.evaluate(() => {
  123 |       (document.getElementById("chat-input") as HTMLTextAreaElement).value = "@programmer 查看当前目录";
  124 |       document.getElementById("chat-send")?.click();
  125 |     });
  126 | 
  127 |     // Wait for completion
  128 |     let connected = false;
  129 |     for (let i = 0; i < 60; i++) {
  130 |       await page.waitForTimeout(5000);
  131 |       connected = await page.evaluate(() => {
  132 |         const label = document.querySelector(".status-label")?.textContent;
  133 |         return label === "已连接" || label === "已卡住";
  134 |       });
  135 |       if (connected) break;
  136 |     }
```