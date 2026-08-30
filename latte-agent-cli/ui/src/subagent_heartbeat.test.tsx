// subagent 工具活动的主对话心跳（chat_impl.ts）。
//
// 实测实录：task_refine 的 refine step 跑了 228 秒、发了 42
// 条 ToolUse/ToolResult，全部带 sub_id。带 sub_id 的工具事件按设计不进
// 主对话正文（详情面板里有完整流水），结果整段委派在主对话里零变化，
// 用户判断为「UI 没显示 / 卡死」。修复后：徽章与 executing 行带上
// 「工具调用数 + 最近工具名」的心跳，正文仍然不被工具日志淹没。
import { describe, it, expect } from "vitest";
import { mountChat } from "./chat_impl";
import type { ChatEvent } from "./api";

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

const SUB = "task_planner-1";
const delegateStarted: ChatEvent = {
  type: "DelegateStarted",
  from_role: "manager",
  to_role: "task_planner",
  task: "拆分 LAT-103",
  sub_id: SUB,
  wf_id: "wf-1",
};
const roleStarted: ChatEvent = {
  type: "RoleStarted",
  role_id: "task_planner",
  detail: "workflow step 'refine'",
  sub_id: SUB,
};
const toolUse = (tool: string, args: string): ChatEvent => ({
  type: "ToolUse",
  role_id: "task_planner",
  tool_name: tool,
  args,
  sub_id: SUB,
});

function badge(messagesEl: HTMLElement): HTMLElement | null {
  return messagesEl.querySelector(`.message-row[data-sub-id="${SUB}"] .delegate-state`);
}
function statusRowText(messagesEl: HTMLElement): string {
  const row = messagesEl.querySelector(".message.status .content");
  return row?.textContent ?? "";
}

describe("subagent 工具活动的主对话心跳", () => {
  it("带 sub_id 的工具事件不进正文，但推进徽章与状态行的计数", () => {
    const { chat, messagesEl } = mount();
    chat.handleEvent(delegateStarted);
    chat.handleEvent(roleStarted);
    expect(badge(messagesEl)?.textContent).toBe("⏳ 执行中…");

    chat.handleEvent(toolUse("read", "{\"path\":\"a\"}"));
    chat.handleEvent(toolUse("search", "{\"q\":\"b\"}"));

    expect(badge(messagesEl)?.textContent).toBe("⏳ 执行中… 🔧2 search");
    expect(statusRowText(messagesEl)).toContain("🔧2 search");
    // 正文不能被工具日志灌满：只有 delegate 气泡 + 一条状态行。
    expect(messagesEl.querySelectorAll(".message.tool")).toHaveLength(0);
  });

  it("历史回放清空 delegate 状态后，live 事件仍能按 sub_id 找回徽章", () => {
    const { chat, messagesEl } = mount();
    // 回放：DelegateStarted/RoleStarted 来自 history 快照。
    chat.replayEvents([delegateStarted, roleStarted]);
    // 之后的 live 事件（session 其实还在跑）。
    chat.handleEvent(toolUse("read", "{\"path\":\"a\"}"));
    expect(badge(messagesEl)?.textContent).toBe("⏳ 执行中… 🔧1 read");

    chat.handleEvent({
      type: "DelegateFinished",
      from_role: "manager",
      to_role: "task_planner",
      status: "ok",
      summary: "done",
      sub_id: SUB,
      wf_id: "wf-1",
    });
    expect(badge(messagesEl)?.textContent).toBe("✅ 完成");
  });
});
