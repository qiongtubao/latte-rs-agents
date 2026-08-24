// 并行波的 per-role 超时询问条（chat_impl.ts）。
//
// 旧实现是单槽（timeoutPromptEl + timeoutPromptRole）：
//   1. DAG 并行波（design_and_plan 的 req_review ‖ code_review）两条
//      分派同时超时时，后到的 warning 把前一条**静默顶掉** —— 用户
//      看到的是 B 的提示。
//   2. 按钮打的是全局 cancelTurn，于是"看到 B 的提示、按下去杀掉整轮
//      （含已跑十几分钟的 A）"。
//   3. 后端改成按 soft 周期复发提醒后，同一条分派会不断堆卡片。
//
// 修复后：按 `sub_id ?? role_id` 各挂一条；复发原地更新秒数；分派级
// 的终止走 per-subsession 通道（只掐这一条）。
import { describe, it, expect, vi, beforeEach } from "vitest";
import { mountChat } from "./chat_impl";
import type { ChatEvent } from "./api";

// cancelSubagent / cancelTurn 会走 transport 发真实请求，这里 mock 掉
// 只验证"点了哪个按钮 → 调了哪个 API、带什么参数"。
// 签名要与真实 API 一致：vi.fn(async () => true) 会把 mock 推导成
// 零参函数，调用处传 subId 就报 TS2554（build 里的 tsc 会拦下来）。
const cancelSubagent = vi.fn(async (_subId: string) => true);
const cancelTurn = vi.fn(async () => {});
vi.mock("./api", async () => {
  const actual = await vi.importActual<typeof import("./api")>("./api");
  return {
    ...actual,
    cancelSubagent: (subId: string) => cancelSubagent(subId),
    cancelTurn: () => cancelTurn(),
  };
});

function makeBinding() {
  const el = <T extends HTMLElement>(tag: string) => document.createElement(tag) as unknown as T;
  const statusPill = el<HTMLElement>("div");
  const statusLabel = el<HTMLElement>("span");
  statusLabel.className = "status-label";
  const statusTime = el<HTMLElement>("span");
  statusTime.className = "status-time";
  statusPill.append(statusLabel, statusTime);
  return {
    messagesEl: el<HTMLElement>("div"),
    formEl: el<HTMLFormElement>("form"),
    inputEl: el<HTMLTextAreaElement>("textarea"),
    sendBtn: el<HTMLButtonElement>("button"),
    clearBtn: el<HTMLButtonElement>("button"),
    quitBtn: el<HTMLButtonElement>("button"),
    pauseBtn: el<HTMLButtonElement>("button"),
    resumeBtn: el<HTMLButtonElement>("button"),
    statusPill,
    roleSelect: el<HTMLSelectElement>("select"),
    rolePauseToggle: el<HTMLButtonElement>("button"),
    rolePill: el<HTMLElement>("div"),
    modelPill: el<HTMLElement>("div"),
    footerMsg: el<HTMLElement>("div"),
  };
}

function mount() {
  document.body.replaceChildren();
  const container = makeBinding();
  document.body.appendChild(container.messagesEl);
  for (const id of [
    "cmd-autocomplete",
    "role-autocomplete",
    "contextMenu",
    "editOverlay",
    "editTextarea",
    "editSave",
    "editCancel",
    "editCancelTop",
    "subagentLog",
    "subagentOverlay",
  ]) {
    const el = document.createElement(id === "editTextarea" ? "textarea" : "div");
    el.id = id;
    document.body.appendChild(el);
  }
  const chat = mountChat({ container, initialRole: "manager" });
  return { chat, messagesEl: container.messagesEl };
}

const warn = (
  roleId: string,
  elapsed: number,
  subId?: string,
): ChatEvent => ({
  type: "TimeoutWarning",
  role_id: roleId,
  elapsed_secs: elapsed,
  soft_timeout_secs: 900,
  hard_timeout_secs: 0, // 后端已无硬超时
  ...(subId ? { sub_id: subId } : {}),
});

const dispatchStart = (roleId: string, subId: string, wfId?: string): ChatEvent => ({
  type: "DelegateStarted",
  from_role: "manager",
  to_role: roleId,
  task: "review something",
  sub_id: subId,
  ...(wfId ? { wf_id: wfId } : {}),
});

const dispatchFinish = (roleId: string, subId: string): ChatEvent => ({
  type: "DelegateFinished",
  from_role: "manager",
  to_role: roleId,
  status: "ok",
  summary: "done",
  sub_id: subId,
});

function prompts(messagesEl: HTMLElement): HTMLElement[] {
  return Array.from(messagesEl.querySelectorAll(".timeout-prompt:not(.advisor-pause-banner)"));
}

beforeEach(() => {
  cancelSubagent.mockClear();
  cancelTurn.mockClear();
});

describe("并行波超时询问条", () => {
  it("两条分派同时超时 → 各挂一条，互不顶掉", () => {
    const { chat, messagesEl } = mount();
    chat.handleEvent(dispatchStart("reviewer", "sub-req"));
    chat.handleEvent(dispatchStart("tester", "sub-code"));

    chat.handleEvent(warn("reviewer", 900, "sub-req"));
    expect(prompts(messagesEl)).toHaveLength(1);

    chat.handleEvent(warn("tester", 900, "sub-code"));
    const shown = prompts(messagesEl);
    expect(shown, "并行两条分派应各有一条提示（旧实现只剩 1 条）").toHaveLength(2);
    // 两条分别标注归属，用户才知道该终止哪个。
    expect(shown.map((p) => p.dataset.subId).sort()).toEqual(["sub-code", "sub-req"]);
  });

  it("同一条复发 → 原地更新秒数，不堆叠", () => {
    const { chat, messagesEl } = mount();
    chat.handleEvent(dispatchStart("reviewer", "sub-req"));
    chat.handleEvent(warn("reviewer", 900, "sub-req"));
    chat.handleEvent(warn("reviewer", 1800, "sub-req"));
    chat.handleEvent(warn("reviewer", 2700, "sub-req"));

    const shown = prompts(messagesEl);
    expect(shown, "复发不应堆卡片").toHaveLength(1);
    expect(
      shown[0].querySelector(".timeout-prompt__elapsed")?.textContent,
      "秒数应更新为最新一次",
    ).toBe("2700s");
  });

  it("一条分派结束 → 只收掉它的提示，兄弟那条保留", () => {
    const { chat, messagesEl } = mount();
    chat.handleEvent(dispatchStart("reviewer", "sub-req"));
    chat.handleEvent(dispatchStart("tester", "sub-code"));
    chat.handleEvent(warn("reviewer", 900, "sub-req"));
    chat.handleEvent(warn("tester", 900, "sub-code"));
    expect(prompts(messagesEl)).toHaveLength(2);

    chat.handleEvent(dispatchFinish("reviewer", "sub-req"));
    const left = prompts(messagesEl);
    expect(left, "只应收掉结束那条").toHaveLength(1);
    expect(left[0].dataset.subId).toBe("sub-code");
  });

  it("分派级终止只掐这一条（走 per-subsession 通道，不是整轮取消）", () => {
    const { chat, messagesEl } = mount();
    chat.handleEvent(dispatchStart("reviewer", "sub-req"));
    chat.handleEvent(dispatchStart("tester", "sub-code"));
    chat.handleEvent(warn("reviewer", 900, "sub-req"));
    chat.handleEvent(warn("tester", 900, "sub-code"));

    const target = prompts(messagesEl).find((p) => p.dataset.subId === "sub-req")!;
    const btn = target.querySelector<HTMLButtonElement>('[data-act="cancel"]')!;
    expect(btn.textContent, "分派级按钮文案应是「终止此分派」").toContain("终止此分派");
    btn.click();

    expect(cancelSubagent, "应调 per-subsession 取消").toHaveBeenCalledWith("sub-req");
    expect(cancelTurn, "绝不能连带整轮取消（会杀掉兄弟分派）").not.toHaveBeenCalled();
    // 被终止那条的提示收掉，兄弟保留。
    const left = prompts(messagesEl);
    expect(left).toHaveLength(1);
    expect(left[0].dataset.subId).toBe("sub-code");
  });

  it("主 turn 超时（无 sub_id）仍是整轮取消", () => {
    const { chat, messagesEl } = mount();
    chat.handleEvent(warn("manager", 900));

    const p = prompts(messagesEl)[0];
    const btn = p.querySelector<HTMLButtonElement>('[data-act="cancel"]')!;
    expect(btn.textContent, "主 turn 没有分派可掐，文案应是整轮取消").toContain(
      "终止当前任务",
    );
    btn.click();
    expect(cancelTurn).toHaveBeenCalled();
    expect(cancelSubagent).not.toHaveBeenCalled();
  });

  it("「继续等待」只收提示，不动后端", () => {
    const { chat, messagesEl } = mount();
    chat.handleEvent(dispatchStart("reviewer", "sub-req"));
    chat.handleEvent(warn("reviewer", 900, "sub-req"));

    prompts(messagesEl)[0]
      .querySelector<HTMLButtonElement>('[data-act="continue"]')!
      .click();

    expect(prompts(messagesEl)).toHaveLength(0);
    expect(cancelSubagent).not.toHaveBeenCalled();
    expect(cancelTurn).not.toHaveBeenCalled();
  });

  it("hard_timeout_secs=0 显示「不会被强杀」而非 NaN 文案", () => {
    const { chat, messagesEl } = mount();
    chat.handleEvent(warn("manager", 900));
    const body = prompts(messagesEl)[0].querySelector(".timeout-prompt__body")!;
    expect(body.textContent).toContain("无硬超时");
    expect(body.textContent).not.toContain("NaN");
    expect(body.textContent, "0 不该被当成真的硬超时秒数渲染").not.toContain(
      "0s 强制终止",
    );
  });
});
