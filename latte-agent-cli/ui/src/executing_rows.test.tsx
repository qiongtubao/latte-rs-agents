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

function mount(onShowSubsession?: (subId: string, label: string, anchor?: HTMLElement) => void) {
  document.body.replaceChildren();
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
  const chat = mountChat({ container, initialRole: "manager", onShowSubsession });
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
  it("工具事件按 sub_id 不写入主 session", () => {
    const { chat, messagesEl } = mount();
    chat.handleEvent(started("tester-1"));
    chat.handleEvent(started("tester-2"));
    chat.handleEvent({ type: "ToolUse", role_id: "tester", tool_name: "read", args: "{}", sub_id: "tester-2" });
    chat.handleEvent({ type: "ToolUse", role_id: "tester", tool_name: "search", args: "{}", sub_id: "tester-1" });
    expect(messagesEl.querySelectorAll(".message.tool")).toHaveLength(0);
    expect(messagesEl.querySelectorAll(".tool-log-line")).toHaveLength(0);
  });

  it("task_planner details use the standard subsession callback", () => {
    let opened: { subId: string; label: string; anchor?: HTMLElement } | undefined;
    const { chat, messagesEl } = mount((subId, label, anchor) => { opened = { subId, label, anchor }; });
    chat.handleEvent({
      type: "RoleStarted",
      role_id: "task_planner",
      detail: "workflow step 'submit'",
      sub_id: "task_planner-submit",
    });
    chat.handleEvent({
      type: "ToolUse",
      role_id: "task_planner",
      tool_name: "read",
      args: '{"path":"lab/notes/baseline.md"}',
      sub_id: "task_planner-submit",
    });
    expect(messagesEl.querySelectorAll(".message.tool")).toHaveLength(0);
    expect(messagesEl.querySelectorAll(".tool-log-line")).toHaveLength(0);
    const details = messagesEl.querySelector<HTMLElement>(".subsession-link");
    expect(details).not.toBeNull();
    details?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    expect(opened?.subId).toBe("task_planner-submit");
    expect(opened?.label).toBe("🤖 task_planner");
    expect(opened?.anchor).toBeInstanceOf(HTMLElement);
  });

  it("workflow fixed-role execution uses the same delegate card projection", () => {
    const { chat, messagesEl } = mount();
    chat.handleEvent({
      type: "WorkflowStarted",
      name: "task_refine",
      topic: "split task",
      wf_id: "wf-1",
    });
    chat.handleEvent({
      type: "WorkflowStep",
      wf_id: "wf-1",
      step_id: "refine",
      description: "inspect and split",
      index: 1,
      total: 2,
      role_id: "task_planner",
      task: "inspect repository",
    });
    chat.handleEvent({
      type: "DelegateStarted",
      from_role: "manager",
      to_role: "task_planner",
      task: "inspect repository",
      sub_id: "task-planner-1",
      wf_id: "wf-1",
    });
    expect(messagesEl.querySelectorAll(".message-row.role")).toHaveLength(2);
    expect(messagesEl.textContent).toContain("@task_planner inspect repository");
    expect(messagesEl.textContent).not.toContain("workflow）");
  });

  it("task_planner workflow return is visible and references its dispatch", () => {
    const { chat, messagesEl } = mount();
    chat.handleEvent({ type: "WorkflowStarted", name: "task_refine", topic: "split", wf_id: "wf-2" });
    chat.handleEvent({
      type: "WorkflowStep",
      wf_id: "wf-2",
      step_id: "refine",
      description: "",
      index: 1,
      total: 1,
      role_id: "task_planner",
      task: "inspect repository",
    });
    chat.handleEvent({
      type: "DelegateStarted",
      from_role: "manager",
      to_role: "task_planner",
      task: "inspect repository",
      sub_id: "task-planner-2",
      wf_id: "wf-2",
    });
    chat.handleEvent({
      type: "WorkflowTurn",
      wf_id: "wf-2",
      step_id: "refine",
      role_id: "task_planner",
      content: "已完成仓库检查。",
      round: 0,
    });
    const roles = messagesEl.querySelectorAll(".message-row.role");
    expect(roles).toHaveLength(3);

    expect(messagesEl.textContent).toContain("已完成仓库检查");
    expect(messagesEl.querySelector(".quote-block")).not.toBeNull();
  });

  it("advisor top-level message keeps session log action visible", () => {
    const { chat, messagesEl } = mount();
    const menu = document.getElementById("contextMenu")!;
    const item = document.createElement("button");
    item.className = "menu-item";
    item.dataset.action = "view-execution-log";
    menu.appendChild(item);
    chat.handleEvent({ type: "RoleTurn", role_id: "advisor", content: "🛑 intervene: 返回不完整", is_complete: true });
    const row = messagesEl.querySelector<HTMLElement>(".message-row.role")!;
    row.dispatchEvent(new MouseEvent("contextmenu", { bubbles: true, clientX: 10, clientY: 10 }));
    expect(item.style.display).not.toBe("none");
  });
});

  it("重复到达同一 advisor intervene 事件时只渲染一次", () => {
    const { chat, messagesEl } = mount();
    const event = {
      type: "RoleTurn" as const,
      role_id: "advisor",
      content: "🛑 intervene（task_planner 委派返回审查）：返回内容被截断",
      is_complete: true,
    };
    chat.handleEvent(event);
    chat.handleEvent(event);
    expect(messagesEl.textContent).toContain("返回内容被截断");
    expect(messagesEl.querySelectorAll(".message-row.role")).toHaveLength(1);
  });
