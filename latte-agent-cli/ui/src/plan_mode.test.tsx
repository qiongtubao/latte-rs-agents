// PlanModeChanged 事件 + plan mode 开关的渲染自测（chat_impl.ts）。
// 后端端点/事件并行开发中，这里按契约驱动 mountChat，验证 —
//   1. 开关点击 → setPlanMode(sessionId, active) 调用 + 乐观翻转；
//      端点失败时回滚；
//   2. PlanModeChanged 事件翻转按钮激活态 + 顶部「Plan」徽章 + 留
//      status 行痕；
//   3. 回放（replayEvents）按最后一条恢复；clear() 归零（切 session）。
import { describe, it, expect, vi, beforeEach } from "vitest";
import { mountChat } from "./chat_impl";
import type { ChatEvent } from "./api";

const setPlanMode = vi.fn(async (_sessionId: string, _active: boolean) => {});
vi.mock("./api", async () => {
  const actual = await vi.importActual<typeof import("./api")>("./api");
  return {
    ...actual,
    setPlanMode: (sessionId: string, active: boolean) => setPlanMode(sessionId, active),
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
  // 开关挂在 form 的 .input-bar-actions 里；徽章挂在 statusPill 所在的
  // topbar-status。两者都给父节点，模拟 index.html 结构。
  const inputBar = document.createElement("div");
  inputBar.className = "input-bar-actions";
  container.formEl.appendChild(inputBar);
  const chatContainer = document.createElement("div");
  chatContainer.append(container.messagesEl, container.formEl);
  const topbarStatus = document.createElement("div");
  topbarStatus.className = "topbar-status";
  topbarStatus.appendChild(container.statusPill);
  document.body.append(chatContainer, topbarStatus);
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
  const btn = document.querySelector(".plan-mode-toggle") as HTMLButtonElement;
  const pill = document.querySelector(".plan-mode-pill") as HTMLElement;
  return { chat, messagesEl: container.messagesEl, btn, pill };
}

const planModeEvent = (active: boolean): ChatEvent => ({ type: "PlanModeChanged", active });

describe("plan mode 开关与 PlanModeChanged", () => {
  beforeEach(() => {
    setPlanMode.mockClear();
  });

  it("开关点击 → 调端点 + 乐观翻转激活态与徽章", async () => {
    const { btn, pill } = mount();
    expect(btn.textContent).toBe("📐 Plan");
    expect(btn.classList.contains("active")).toBe(false);
    expect(pill.style.display).toBe("none");
    btn.click();
    await vi.waitFor(() => expect(setPlanMode).toHaveBeenCalledWith("", true));
    expect(btn.classList.contains("active")).toBe(true);
    expect(btn.textContent).toBe("📐 Plan ✓");
    expect(pill.style.display).not.toBe("none");
    // 再点关闭
    btn.click();
    await vi.waitFor(() => expect(setPlanMode).toHaveBeenCalledWith("", false));
    expect(btn.classList.contains("active")).toBe(false);
    expect(pill.style.display).toBe("none");
  });

  it("端点失败时回滚按钮态", async () => {
    setPlanMode.mockRejectedValueOnce(new Error("network"));
    const { btn, pill } = mount();
    btn.click();
    await vi.waitFor(() => expect(btn.classList.contains("active")).toBe(false));
    expect(pill.style.display).toBe("none");
  });

  it("PlanModeChanged 事件翻转 UI 态并留 status 行痕", () => {
    const { chat, btn, pill, messagesEl } = mount();
    chat.handleEvent(planModeEvent(true));
    expect(btn.classList.contains("active")).toBe(true);
    expect(pill.style.display).not.toBe("none");
    const rows = messagesEl.querySelectorAll(".message.status");
    expect(rows).toHaveLength(1);
    expect(rows[0].textContent).toContain("[plan mode] 已开启");
    chat.handleEvent(planModeEvent(false));
    expect(btn.classList.contains("active")).toBe(false);
    expect(pill.style.display).toBe("none");
    expect(messagesEl.querySelectorAll(".message.status")).toHaveLength(2);
  });

  it("回放按最后一条恢复；clear() 归零", () => {
    const { chat, btn, pill } = mount();
    chat.replayEvents([planModeEvent(true), planModeEvent(false), planModeEvent(true)]);
    expect(btn.classList.contains("active")).toBe(true);
    expect(pill.style.display).not.toBe("none");
    // 切 session：clear 归零，等回放恢复
    chat.clear();
    expect(btn.classList.contains("active")).toBe(false);
    expect(pill.style.display).toBe("none");
    chat.replayEvents([planModeEvent(false)]);
    expect(btn.classList.contains("active")).toBe(false);
  });
});
