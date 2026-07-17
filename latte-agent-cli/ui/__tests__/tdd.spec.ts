import { test, expect, chromium, type Page, type Browser } from "@playwright/test";

const UI_BASE = process.env.UI_BASE_URL ?? "http://localhost:4567";

test.describe("latte-agent UI 移植功能 TDD", () => {
  let browser: Browser;
  let page: Page;

  test.beforeAll(async () => {
    browser = await chromium.launch({ headless: true, args: ["--no-sandbox", "--disable-dev-shm-usage"] });
  });

  test.beforeEach(async () => {
    page = await browser.newPage({ viewport: { width: 1400, height: 900 } });
    page.on("pageerror", (err) => console.error("[pageerror]", err.message));
    await page.goto(UI_BASE, { waitUntil: "domcontentloaded", timeout: 15000 });
    await page.waitForTimeout(3000);
  });

  test.afterEach(async () => {
    await page.close();
  });

  test.afterAll(async () => {
    await browser.close();
  });

  test("T1: 初始状态显示角色头像、模型名、连接状态", async () => {
    const state = await page.evaluate(() => ({
      rolePill: document.getElementById("role-pill")?.textContent || "",
      modelPill: document.getElementById("model-pill")?.textContent || "",
      statusClass: document.getElementById("status-pill")?.className || "",
      statusLabel: document.querySelector(".status-label")?.textContent || "",
      roleOptions: document.querySelectorAll("#role-select option").length,
    }));
    expect(state.rolePill).toMatch(/^[\u{1F000}-\u{1FFFF}]/u);
    expect(state.modelPill.length).toBeGreaterThan(0);
    expect(state.statusClass).toContain("connected");
    expect(state.statusLabel).toBe("已连接");
    expect(state.roleOptions).toBeGreaterThan(0);
  });

  test("T2: @programmer 派发并获取回复", async () => {
    await page.evaluate(() => {
      (document.getElementById("chat-input") as HTMLTextAreaElement).value = "@programmer 查看package.json";
      document.getElementById("chat-send")?.click();
    });
    // Poll manually with longer intervals
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

    const state = await page.evaluate(() => {
      const msgs = Array.from(document.querySelectorAll(".message, .message-row, .message"));
      const programmerMsg = Array.from(document.querySelectorAll(".message-row")).find(
        m => m.querySelector(".msg-avatar")?.textContent === "P"
      );
      return {
        delegationShown: msgs.some(m => m.textContent?.includes("🤝 @manager → @programmer")),
        programmerReplied: !!programmerMsg,
        programmerClickable: programmerMsg?.hasAttribute("data-sub-id") || programmerMsg?.style?.cursor === "pointer",
        programmerTitle: (programmerMsg as HTMLElement)?.title || "",
        hasElapsedTime: msgs.some(m => m.textContent?.includes("⏱")),
        switchedBack: (document.getElementById("role-pill")?.textContent || "").includes("👔"),
      };
    });
    expect(state.delegationShown).toBe(true);
    expect(state.programmerReplied).toBe(true);
    expect(state.programmerClickable).toBe(true);
    expect(state.programmerTitle).toBe("点击查看执行过程");
    expect(state.hasElapsedTime).toBe(true);
    expect(state.switchedBack).toBe(true);
  });

  test("T3: 右键打开 Subsession 弹窗", async () => {
    await page.evaluate(() => {
      (document.getElementById("chat-input") as HTMLTextAreaElement).value = "@programmer 查看项目结构";
      document.getElementById("chat-send")?.click();
    });
    for (let i = 0; i < 60; i++) {
      await page.waitForTimeout(5000);
      const done = await page.evaluate(() => {
        const label = document.querySelector(".status-label")?.textContent;
        return label === "已连接" || label === "已卡住";
      });
      if (done) break;
    }
    await page.evaluate(() => {
      const msg = Array.from(document.querySelectorAll(".message-row")).find(
        m => m.querySelector(".msg-avatar")?.textContent === "P"
      );
      if (msg) msg.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true }));
    });
    await page.waitForTimeout(500);
    const menuVisible = await page.evaluate(() => {
      const menu = document.querySelector(".context-menu");
      if (!menu) return false;
      const style = window.getComputedStyle(menu);
      return style.display !== "none" && style.visibility !== "hidden";
    });
    expect(menuVisible).toBe(true);
    const firstItem = await page.evaluate(() => {
      const items = document.querySelectorAll(".context-menu-item, .context-menu li, .context-menu button, .menu-item");
      return items.length > 0 ? items[0]?.textContent || "" : "";
    });
    expect(firstItem).toContain("编辑");
    await page.evaluate(() => {
      document.body.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    });
    await page.waitForTimeout(300);
  });

  test("T4: 耗时显示", async () => {
    await page.evaluate(() => {
      (document.getElementById("chat-input") as HTMLTextAreaElement).value = "@programmer 查看当前目录";
      document.getElementById("chat-send")?.click();
    });
    for (let i = 0; i < 60; i++) {
      await page.waitForTimeout(5000);
      const done = await page.evaluate(() => {
        const label = document.querySelector(".status-label")?.textContent;
        return label === "已连接" || label === "已卡住";
      });
      if (done) break;
    }
    const state = await page.evaluate(() => {
      const msgs = Array.from(document.querySelectorAll(".message, .message-row, .message"));
      return {
        hasElapsedInRole: Array.from(document.querySelectorAll(".message-row:not(.status):not(.system)")).some(m => m.textContent?.includes("⏱")),
        hasElapsedAnywhere: msgs.some(m => m.textContent?.includes("⏱")),
      };
    });
    expect(state.hasElapsedInRole).toBe(false);
    expect(state.hasElapsedAnywhere).toBe(true);
  });

  test("T5: 页面可渲染", async () => {
    expect(await page.title()).toBe("latte-agent UI");
    expect(await page.evaluate(() => !!document.getElementById("chat-input"))).toBe(true);
  });
});
