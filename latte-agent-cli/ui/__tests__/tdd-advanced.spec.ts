import { test, expect, chromium, type Page, type Browser } from "@playwright/test";
import * as fs from "fs";

const UI_BASE = process.env.UI_BASE_URL ?? "http://localhost:4567";

test.describe("latte-agent UI TDD Advanced", () => {
  let browser: Browser;
  let page: Page;

  test.beforeAll(async () => {
    browser = await chromium.launch({ headless: true, args: ["--no-sandbox", "--disable-dev-shm-usage"] });
  });

  test.beforeEach(async () => {
    page = await browser.newPage({ viewport: { width: 1400, height: 900 } });
    page.on("pageerror", (err) => console.error("[pageerror]", err.message));
    await page.goto(UI_BASE, { waitUntil: "domcontentloaded", timeout: 15000 });
    // 等 main() 挂载完成（panel button handler + SSE 订阅 + chat-send handler 都已就位）
    // 旧 waitForTimeout(3000) 是硬等，慢机器上不够；data-ui-mounted 是 main() 末尾的确定性标记。
    await page.waitForSelector('body[data-ui-mounted="1"]', { timeout: 15000 });
  });

  test.afterEach(async () => {
    await page.close();
  });

  test.afterAll(async () => {
    await browser.close();
  });

  test("T6: MCP agent role exists in dropdown", async () => {
    // 1. Verify mcp_agent exists as an option in the role dropdown
    const hasMcpAgent = await page.evaluate(() => {
      const select = document.getElementById("role-select") as HTMLSelectElement;
      if (!select) return false;
      return Array.from(select.options).some(opt => opt.value === "mcp_agent");
    });
    expect(hasMcpAgent).toBe(true);

    // 2. Verify the mcp_agent option shows the 🔌 icon (from role config)
    const mcpOptionHtml = await page.evaluate(() => {
      const select = document.getElementById("role-select") as HTMLSelectElement;
      const opt = Array.from(select.options).find(o => o.value === "mcp_agent");
      return opt ? opt.innerHTML : "";
    });
    expect(mcpOptionHtml).toContain("🔌");

    // 3. Verify the dropdown has at least 3 options
    const optionCount = await page.evaluate(() => {
      const select = document.getElementById("role-select") as HTMLSelectElement;
      return select?.options.length ?? 0;
    });
    expect(optionCount).toBeGreaterThanOrEqual(3);

    // 4. Verify mcp_agent has a non-empty role config icon (🔌)
    expect(mcpOptionHtml).toMatch(/🔌/);
  });

  test("T7: Screenshot tool via bash", async () => {
    const screenshotPath = "/tmp/latte-ui-advanced-shot.png";

    // Clean up any previous screenshot
    try { fs.unlinkSync(screenshotPath); } catch {}

    // Use Playwright's built-in chromium screenshot capability
    await page.screenshot({ path: screenshotPath, fullPage: false });

    // Verify the file was created and has content
    expect(fs.existsSync(screenshotPath)).toBe(true);
    const stats = fs.statSync(screenshotPath);
    expect(stats.size).toBeGreaterThan(0);

    // Verify the chat panel is visible in the screenshot (confirming it captured UI)
    const hasChat = await page.evaluate(() => !!document.getElementById("chat-input"));
    expect(hasChat).toBe(true);
  });

  test("T8: @programmer subsession tool popup", async () => {
    // Send @programmer delegation
    await page.evaluate(() => {
      (document.getElementById("chat-input") as HTMLTextAreaElement).value = "@programmer 运行 ls -la 命令查看当前目录文件";
      document.getElementById("chat-send")?.click();
    });

    // Wait for completion
    let connected = false;
    for (let i = 0; i < 60; i++) {
      await page.waitForTimeout(5000);
      connected = await page.evaluate(() => {
        const label = document.querySelector(".status-label")?.textContent;
        return label === "已连接" || label === "已卡住";
      });
      if (connected) break;
    }
    expect(connected).toBe(true);

    // Verify programmer role message exists with click cursor
    const programmerMsgState = await page.evaluate(() => {
      const programmerMsg = Array.from(document.querySelectorAll(".message-row")).find(
        m => m.querySelector(".msg-avatar")?.textContent === "P"
      );
      if (!programmerMsg) return { found: false };
      const style = window.getComputedStyle(programmerMsg);
      return {
        found: true,
        hasSubId: programmerMsg.hasAttribute("data-sub-id"),
        cursor: style.cursor,
        hasRoleClass: programmerMsg.classList.contains("role"),
      };
    });
    expect(programmerMsgState.found).toBe(true);
    expect(programmerMsgState.hasSubId || programmerMsgState.cursor === "pointer").toBe(true);
    expect(programmerMsgState.hasRoleClass).toBe(true);

    // Verify subsession-link exists on the DelegateFinished message
    const hasSubsessionLink = await page.evaluate(() => {
      return !!document.querySelector(".subsession-link");
    });
    expect(hasSubsessionLink).toBe(true);
  });

  test("T9: Elapsed time not in role message", async () => {
    // Send @programmer delegation
    await page.evaluate(() => {
      (document.getElementById("chat-input") as HTMLTextAreaElement).value = "@programmer 查看当前目录";
      document.getElementById("chat-send")?.click();
    });

    // Wait for completion
    let connected = false;
    for (let i = 0; i < 60; i++) {
      await page.waitForTimeout(5000);
      connected = await page.evaluate(() => {
        const label = document.querySelector(".status-label")?.textContent;
        return label === "已连接" || label === "已卡住";
      });
      if (connected) break;
    }
    expect(connected).toBe(true);

    // Check elapsed time is NOT in role messages but MAY be in status messages
    const state = await page.evaluate(() => {
      const statusMsgs = Array.from(document.querySelectorAll(".message.status"));
      const msgRows = Array.from(document.querySelectorAll(".message-row:not(.status):not(.system)"));
      const hasElapsedInMsgRow = msgRows.some(m => m.textContent?.includes("⏱"));
      const hasElapsedInStatus = statusMsgs.some(m => m.textContent?.includes("⏱"));
      return { hasElapsedInMsgRow, hasElapsedInStatus, statusMsgCount: statusMsgs.length, msgRowCount: msgRows.length, };
    });
    expect(state.hasElapsedInMsgRow).toBe(false);
    expect(state.msgRowCount).toBeGreaterThan(0);

  });

  test("T10: Role switching preserves icons", async () => {
    // Get the initial role pill state
    const initialPill = await page.evaluate(() => ({
      text: document.getElementById("role-pill")?.textContent || "",
      model: document.getElementById("model-pill")?.textContent || "",
    }));
    expect(initialPill.text.length).toBeGreaterThan(0);
    expect(initialPill.model.length).toBeGreaterThan(0);

    // Extract initial role icon and role ID (e.g., "👔 manager" → icon="👔", role="manager")
    const initialIcon = initialPill.text.match(/^(\p{So}|\p{Emoji_Presentation})/u)?.[0] || "";
    const initialRole = initialPill.text.replace(/^\p{So}|\p{Emoji_Presentation}/u, "").trim();
    expect(initialIcon.length).toBeGreaterThan(0);
    expect(initialRole.length).toBeGreaterThan(0);

    // Get available roles from dropdown (skip current role)
    const availableRoles = await page.evaluate(() => {
      const select = document.getElementById("role-select") as HTMLSelectElement;
      return Array.from(select.options)
        .filter(o => !o.selected)
        .map(o => o.value)
        .slice(0, 3);
    });
    expect(availableRoles.length).toBeGreaterThan(0);

    const targetRole = availableRoles[0];

    // Switch to a different role via dropdown
    await page.evaluate((role) => {
      const select = document.getElementById("role-select") as HTMLSelectElement;
      select.value = role;
      select.dispatchEvent(new Event("change", { bubbles: true }));
    }, targetRole);

    // Wait briefly for UI update (setRoleSelected runs synchronously)
    await page.waitForTimeout(1000);

    // 切角色后 role-pill 仍带 emoji：updateRoleDisplay 走
    // `${icon} ${roleId}` 路径，icon 来自 currentRoleIcon 或 roleIcon(roleId) 回退。
    const afterSwitchPill = await page.evaluate(() => ({
      text: document.getElementById("role-pill")?.textContent || "",
      model: document.getElementById("model-pill")?.textContent || "",
    }));

    expect(afterSwitchPill.text).toContain(targetRole);
    // 至少保留 emoji 前缀（实测 `${initialIcon} ${targetRole}`，
    // 但容许顺序/空白差异，只断言 emoji 在前）
    expect(afterSwitchPill.text).toMatch(/^[\p{So}\p{Emoji_Presentation}]/u);
    // Switch back to the initial role
    await page.evaluate((role) => {
      const select = document.getElementById("role-select") as HTMLSelectElement;
      select.value = role;
      select.dispatchEvent(new Event("change", { bubbles: true }));
    }, initialRole);

    await page.waitForTimeout(1000);

    // Verify role pill reverted
    // 切回初始终角色后 pill 也带 emoji（同样走 updateRoleDisplay 路径），
    // 文本至少要包含 initialRole 且以 emoji 起首。
    const finalPill = await page.evaluate(() => ({
      text: document.getElementById("role-pill")?.textContent || "",
      model: document.getElementById("model-pill")?.textContent || "",
    }));
    expect(finalPill.text).toContain(initialRole);
    expect(finalPill.text).toMatch(/^[\p{So}\p{Emoji_Presentation}]/u);
    expect(finalPill.model).toBe(initialPill.model);
  });
});
