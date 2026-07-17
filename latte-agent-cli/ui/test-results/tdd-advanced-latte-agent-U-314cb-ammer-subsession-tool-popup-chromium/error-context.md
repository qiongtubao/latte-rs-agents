# Instructions

- Following Playwright test failed.
- Explain why, be concise, respect Playwright best practices.
- Provide a snippet of code with the fix, if possible.

# Test info

- Name: tdd-advanced.spec.ts >> latte-agent UI TDD Advanced >> T8: @programmer subsession tool popup
- Location: __tests__/tdd-advanced.spec.ts:76:3

# Error details

```
Error: expect(received).toBe(expected) // Object.is equality

Expected: true
Received: false
```

# Test source

```ts
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
  36  |     expect(hasMcpAgent).toBe(true);
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
> 117 |     expect(hasSubsessionLink).toBe(true);
      |                               ^ Error: expect(received).toBe(expected) // Object.is equality
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
  137 |     expect(connected).toBe(true);
  138 | 
  139 |     // Check elapsed time is NOT in role messages but MAY be in status messages
  140 |     const state = await page.evaluate(() => {
  141 |       const statusMsgs = Array.from(document.querySelectorAll(".message.status"));
  142 |       const msgRows = Array.from(document.querySelectorAll(".message-row:not(.status):not(.system)"));
  143 |       const hasElapsedInMsgRow = msgRows.some(m => m.textContent?.includes("⏱"));
  144 |       const hasElapsedInStatus = statusMsgs.some(m => m.textContent?.includes("⏱"));
  145 |       return { hasElapsedInMsgRow, hasElapsedInStatus, statusMsgCount: statusMsgs.length, msgRowCount: msgRows.length, };
  146 |     });
  147 |     expect(state.hasElapsedInMsgRow).toBe(false);
  148 |     expect(state.msgRowCount).toBeGreaterThan(0);
  149 | 
  150 |   });
  151 | 
  152 |   test("T10: Role switching preserves icons", async () => {
  153 |     // Get the initial role pill state
  154 |     const initialPill = await page.evaluate(() => ({
  155 |       text: document.getElementById("role-pill")?.textContent || "",
  156 |       model: document.getElementById("model-pill")?.textContent || "",
  157 |     }));
  158 |     expect(initialPill.text.length).toBeGreaterThan(0);
  159 |     expect(initialPill.model.length).toBeGreaterThan(0);
  160 | 
  161 |     // Extract initial role icon and role ID (e.g., "👔 manager" → icon="👔", role="manager")
  162 |     const initialIcon = initialPill.text.match(/^(\p{So}|\p{Emoji_Presentation})/u)?.[0] || "";
  163 |     const initialRole = initialPill.text.replace(/^\p{So}|\p{Emoji_Presentation}/u, "").trim();
  164 |     expect(initialIcon.length).toBeGreaterThan(0);
  165 |     expect(initialRole.length).toBeGreaterThan(0);
  166 | 
  167 |     // Get available roles from dropdown (skip current role)
  168 |     const availableRoles = await page.evaluate(() => {
  169 |       const select = document.getElementById("role-select") as HTMLSelectElement;
  170 |       return Array.from(select.options)
  171 |         .filter(o => !o.selected)
  172 |         .map(o => o.value)
  173 |         .slice(0, 3);
  174 |     });
  175 |     expect(availableRoles.length).toBeGreaterThan(0);
  176 | 
  177 |     const targetRole = availableRoles[0];
  178 | 
  179 |     // Switch to a different role via dropdown
  180 |     await page.evaluate((role) => {
  181 |       const select = document.getElementById("role-select") as HTMLSelectElement;
  182 |       select.value = role;
  183 |       select.dispatchEvent(new Event("change", { bubbles: true }));
  184 |     }, targetRole);
  185 | 
  186 |     // Wait briefly for UI update (setRoleSelected runs synchronously)
  187 |     await page.waitForTimeout(1000);
  188 | 
  189 |     // Verify role pill updated to the new role (without icon, just role ID)
  190 |     const afterSwitchPill = await page.evaluate(() => ({
  191 |       text: document.getElementById("role-pill")?.textContent || "",
  192 |       model: document.getElementById("model-pill")?.textContent || "",
  193 |     }));
  194 | 
  195 |     // The role pill shows just the role ID (setRoleSelected sets textContent = roleId)
  196 |     expect(afterSwitchPill.text).toBe(targetRole);
  197 | 
  198 |     // Model pill should remain unchanged (no server switch, so no Prompt event)
  199 |     expect(afterSwitchPill.model).toBe(initialPill.model);
  200 | 
  201 |     // Switch back to the initial role
  202 |     await page.evaluate((role) => {
  203 |       const select = document.getElementById("role-select") as HTMLSelectElement;
  204 |       select.value = role;
  205 |       select.dispatchEvent(new Event("change", { bubbles: true }));
  206 |     }, initialRole);
  207 | 
  208 |     await page.waitForTimeout(1000);
  209 | 
  210 |     // Verify role pill reverted
  211 |     const finalPill = await page.evaluate(() => ({
  212 |       text: document.getElementById("role-pill")?.textContent || "",
  213 |       model: document.getElementById("model-pill")?.textContent || "",
  214 |     }));
  215 |     expect(finalPill.text).toBe(initialRole);
  216 |     expect(finalPill.model).toBe(initialPill.model);
  217 |   });
```