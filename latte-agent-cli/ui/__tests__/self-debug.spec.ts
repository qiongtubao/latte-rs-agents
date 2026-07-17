import { test, expect, chromium, type Page, type Browser } from "@playwright/test";
const UI_BASE = process.env.UI_BASE_URL ?? "http://localhost:4567";

test.describe("latte-agent UI self-debug loop", () => {
  let browser: Browser;
  let page: Page;

  test.beforeAll(async () => {
    browser = await chromium.launch({ headless: true, args: ["--no-sandbox", "--disable-dev-shm-usage"] });
  });
  test.beforeEach(async () => {
    page = await browser.newPage({ viewport: { width: 1400, height: 900 } });
    await page.goto(UI_BASE, { waitUntil: "domcontentloaded", timeout: 15000 });
    await page.waitForTimeout(3000);
  });
  test.afterEach(async () => { await page.close(); });
  test.afterAll(async () => { await browser.close(); });

  test("S1: 打开页面截图并验证基础UI", async () => {
    const state = await page.evaluate(() => ({
      hasInput: !!document.getElementById("chat-input"),
      hasSendBtn: !!document.getElementById("chat-send"),
      hasStatusPill: !!document.getElementById("status-pill"),
      rolePill: document.getElementById("role-pill")?.textContent || "",
      statusLabel: document.querySelector(".status-label")?.textContent || "",
    }));
    expect(state.hasInput).toBe(true);
    expect(state.hasSendBtn).toBe(true);
    expect(state.hasStatusPill).toBe(true);
    expect(state.rolePill).toMatch(/[\u{1F000}-\u{1FFFF}]/u);
    expect(state.statusLabel).toBe("已连接");
  });

  test("S2: 发送消息并验证回复", async () => {
    await page.evaluate(() => {
      (document.getElementById("chat-input") as HTMLTextAreaElement).value = "查看当前目录";
      document.getElementById("chat-send")?.click();
    });
    for (let i = 0; i < 30; i++) {
      await page.waitForTimeout(5000);
      const done = await page.evaluate(() => {
        const label = document.querySelector(".status-label")?.textContent;
        return label === "已连接" || label === "已卡住";
      });
      if (done) break;
    }
    const state = await page.evaluate(() => {
      const msgs = Array.from(document.querySelectorAll(".message, .message-row, .message"));
      const lastRole = Array.from(document.querySelectorAll(".message-row.role, .message-row.self, .message-row.tool, .message-row.error")).pop();
      return {
        msgCount: msgs.length,
        hasAvatar: !!lastRole?.querySelector(".msg-avatar"),
        avatarIcon: lastRole?.querySelector(".msg-avatar")?.textContent || "",
        hasTime: msgs.some(m => m.textContent?.includes("⏱")),
        status: document.querySelector(".status-label")?.textContent || "",
      };
    });
    expect(state.msgCount).toBeGreaterThanOrEqual(2);
    expect(state.hasAvatar).toBe(true);
    expect(state.avatarIcon.length).toBeGreaterThan(0);
    expect(state.hasTime).toBe(true);
    expect(state.status).toBe("已连接");
  });

  test("S3: @programmer 派发并验证", async () => {
    await page.evaluate(() => {
      (document.getElementById("chat-input") as HTMLTextAreaElement).value = "@programmer 查看package.json";
      document.getElementById("chat-send")?.click();
    });
    for (let i = 0; i < 40; i++) {
      await page.waitForTimeout(5000);
      const done = await page.evaluate(() => {
        const label = document.querySelector(".status-label")?.textContent;
        return label === "已连接" || label === "已卡住";
      });
      if (done) break;
    }
    const state = await page.evaluate(() => {
      const msgs = Array.from(document.querySelectorAll(".message, .message-row, .message"));
      const programmer = Array.from(document.querySelectorAll(".message-row")).find(
        m => m.querySelector(".msg-avatar")?.textContent === "P"
      );
      return {
        delegated: msgs.some(m => m.textContent?.includes("🤝 @manager → @programmer")),
        replied: !!programmer,
        clickable: programmer?.hasAttribute("data-sub-id") || programmer?.style?.cursor === "pointer",
        rolePill: document.getElementById("role-pill")?.textContent || "",
        hasTime: msgs.some(m => m.textContent?.includes("⏱")),
      };
    });
    expect(state.delegated).toBe(true);
    expect(state.replied).toBe(true);
    expect(state.clickable).toBe(true);
    expect(state.hasTime).toBe(true);
  });

  test("S4: 全量功能快速验证", async () => {
    const roles = await page.evaluate(() => {
      const select = document.getElementById("role-select") as HTMLSelectElement;
      return { count: select.options.length, ids: Array.from(select.options).map(o => o.value) };
    });
    expect(roles.count).toBeGreaterThanOrEqual(10);
    expect(roles.ids).toContain("mcp_agent");
    expect(await page.evaluate(() => !!document.getElementById("subsession-panel"))).toBe(true);
  });
});
