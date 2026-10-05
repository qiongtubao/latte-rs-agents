// @vitest-environment jsdom
import { describe, it, expect } from "vitest";
import { renderBillingBanner } from "./billing_banner";

describe("billing_banner", () => {
  it("点名受影响角色，整条链不可用的角色单独标红；无账户时移除", () => {
    const host = document.createElement("div");
    const msgs = document.createElement("div");
    host.appendChild(msgs);
    renderBillingBanner(msgs, [
      {
        provider: "deepseek",
        models: ["deepseek-v4-flash", "deepseek-v4-pro"],
        message: "402 Insufficient Balance",
        recheck_in_secs: 1800,
        roles: [
          { role: "advisor", models: ["deepseek-v4-pro"], all_blocked: false },
          { role: "summarizer", models: ["deepseek-v4-flash"], all_blocked: true },
        ],
      },
    ]);
    const banner = host.querySelector(".billing-banner")!;
    expect(banner.nextElementSibling).toBe(msgs);
    expect(banner.textContent).toContain("deepseek-v4-flash、deepseek-v4-pro");
    const chips = banner.querySelectorAll(".billing-banner__chip");
    expect(chips.length).toBe(2);
    // 整条链不可用的排在前面、标红；其余折叠在 details 里。
    expect(chips[0].textContent).toBe("summarizer");
    expect(chips[0].classList.contains("is-dead")).toBe(true);
    expect(banner.textContent).toContain("整条链不可用");
    expect(banner.querySelector("details")!.textContent).toContain("1 个角色");
    expect(chips[1].textContent).toBe("advisor");
    renderBillingBanner(msgs, []);
    expect(host.querySelector(".billing-banner")).toBeNull();
  });
});
