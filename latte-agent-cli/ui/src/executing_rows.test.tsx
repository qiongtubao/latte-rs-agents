// 并行同角色委派的 executing 状态行配对测试（chat_impl.ts）。
//
// 日志事故：tester 被两个并行 workflow 步（code_review/review 与
//  requirements_review/estimate）同时委派。旧实现 executingRowByRole
// 是 role_id → 单行的 Map，第二次 RoleStarted 覆盖条目，先启动的
// 「🧠 tester 开始执行…」行丢失引用、永远转圈 —— UI 表现为
// 「tester 卡住了」，但后端日志里两次运行都正常结束。
//
// 修复后：RoleStarted/RoleFinished 事件带 sub_id，前端按
// (role_id, sub_id) 精确配对；每个 start 一行、每个 finish 清一行。
import { describe, it, expect } from "vitest";
import { mountChat } from "./chat_impl";
import type { ChatEvent } from "./api";

function makeBinding() {
  const el = <T extends HTMLElement>(tag: string) => document.createElement(tag) as unknown as T;
  return {
    messagesEl: el<HTMLElement>("div"),
    formEl: el<HTMLFormElement>("form"),
    inputEl: el<HTMLTextAreaElement>("textarea"),
    sendBtn: el<HTMLButtonElement>("button"),
    clearBtn: el<HTMLButtonElement>("button"),
    quitBtn: el<HTMLButtonElement>("button"),
    pauseBtn: el<HTMLButtonElement>("button"),
    resumeBtn: el<HTMLButtonElement>("button"),
    statusPill: el<HTMLElement>("div"),
    roleSelect: el<HTMLSelectElement>("select"),
    rolePauseToggle: el<HTMLButtonElement>("button"),
    rolePill: el<HTMLElement>("div"),
    modelPill: el<HTMLElement>("div"),
    footerMsg: el<HTMLElement>("div"),
  };
}

function mount() {
  const container = makeBinding();
  document.body.appendChild(container.messagesEl);
  // mountChat 还引用若干 document 级元素（命令补全 / 编辑弹窗 /
  // 右键菜单 / subagent 日志浮层）。
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

const started = (subId: string): ChatEvent => ({
  type: "RoleStarted",
  role_id: "tester",
  detail: "workflow step 'x'",
  sub_id: subId,
});
const finished = (subId: string): ChatEvent => ({
  type: "RoleFinished",
  role_id: "tester",
  detail: "ok, 100 chars",
  sub_id: subId,
});

function executingRows(messagesEl: HTMLElement): HTMLElement[] {
  return Array.from(messagesEl.querySelectorAll(".message.status.executing"));
}

describe("并行同角色委派的 executing 行配对", () => {
  it("两个并行 tester：后发先至的 finish 精确配对，不留转圈孤行", () => {
    const { chat, messagesEl } = mount();
    chat.handleEvent(started("tester-1"));
    chat.handleEvent(started("tester-2"));
    expect(executingRows(messagesEl)).toHaveLength(2);

    // #2 先完成（estimate 步较短）：应精确清掉 #2 的行，#1 仍在执行
    chat.handleEvent(finished("tester-2"));
    expect(executingRows(messagesEl)).toHaveLength(1);

    // #1 完成后不应再有 executing 行；完成行共两条
    chat.handleEvent(finished("tester-1"));
    expect(executingRows(messagesEl)).toHaveLength(0);
    const doneRows = messagesEl.querySelectorAll(".message.status.done");
    expect(doneRows.length).toBe(2);
  });

  it("旧归档事件不带 sub_id：退化为 FIFO 清行，同样不留孤行", () => {
    const { chat, messagesEl } = mount();
    chat.handleEvent({ type: "RoleStarted", role_id: "tester", detail: "a" });
    chat.handleEvent({ type: "RoleStarted", role_id: "tester", detail: "b" });
    chat.handleEvent({ type: "RoleFinished", role_id: "tester", detail: "ok" });
    chat.handleEvent({ type: "RoleFinished", role_id: "tester", detail: "ok" });
    expect(executingRows(messagesEl)).toHaveLength(0);
  });

  it("finish 无匹配行时退回新插完成行（边角事件不丢）", () => {
    const { chat, messagesEl } = mount();
    chat.handleEvent(finished("tester-9"));
    const doneRows = messagesEl.querySelectorAll(".message.status.done");
    expect(doneRows.length).toBe(1);
  });
});
