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
    expect(chips[0].textContent).toContain("advisor");
    expect(chips[1].classList.contains("is-dead")).toBe(true);
    expect(chips[1].textContent).toContain("整条链不可用");
    renderBillingBanner(msgs, []);
    expect(host.querySelector(".billing-banner")).toBeNull();
  });
});
