// Regression: WorkflowFinished + transcript 抽出任务列表后，「📥 导入任务看板」
// 必须走勾选弹窗（与 PlanProposed 一致），不再 confirm() 一把全导。
//
// 2026-10-19 踩坑（rocksdb learn_plan，26 个任务）：
//   1. UI 文本显示「📋 检测到 26 个可导入任务」，但按钮一直没渲染、
//      原生 confirm() 也不弹 —— 用户看不到任何入口。
//      根因：handleEvent("WorkflowFinished") 里调 addMessage({kind:"system"})，
//      system 走的是 addMessage 的「无头像」分支，row 里只有 .message.system +
//      .content，**没有 .msg-bubble**。旧代码用
//      `msg.querySelector(".msg-bubble")?.appendChild(btn)` 被 `?.` 静默吞掉。
//      PlanProposed 也是同病（只是它后面 1.5s 防抖自动开模态，把这个 drop 掩盖了）。
//
// 2. 即便按钮挂上了，confirm() 全导对 26 个任务意义不大，必须能挑。
//
// 这里锁死三件事：
//   1. WorkflowFinished 的按钮真的能在 DOM 里查得到（修复 DOM 挂载）；
//   2. 点按钮 / 1.5s 防抖后，自动打开勾选弹窗（同 PlanProposed 体验）；
//   3. PlanProposed 实时路径的按钮也真的能挂上（不再依赖自动弹窗兜底）。

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { mountChat } from "./chat_impl";
import type { ChatEvent } from "./api";

// 这次回归只验证 DOM 行为，不实际触发 importTasks；importTasks
// 那边走真实代码。资源：`import_tasks` 触发路径在 chat_impl 之外
// 单独有集成测试覆盖。

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
    const e = document.createElement(id === "editTextarea" ? "textarea" : "div");
    e.id = id;
    document.body.appendChild(e);
  }
  return mountChat({ container, initialRole: "manager" });
}

/** 喂一个 WorkflowStarted 在前（让 chat 把 transcript buffer 挂上），
 *  再喂 WorkflowTurn（带 ```json 任务块），
 *  最后 WorkflowFinished（ok）。这样能命中「成功结束 → 扫描 → 装按钮」
 *  那一支。 */
function pushFinishedWithTasks(wfId: string, taskCount: number): ChatEvent[] {
  const tasks = Array.from({ length: taskCount }, (_, i) => ({
    title: `任务 ${i + 1}`,
  }));
  return [
    { type: "WorkflowStarted", name: "learn_plan", wf_id: wfId, topic: "t" },
    {
      type: "WorkflowTurn",
      wf_id: wfId,
      step_id: "breakdown",
      role_id: "manager",
      content: `本次产出：\n\n\`\`\`json\n${JSON.stringify({ tasks })}\n\`\`\``,
      round: 1,
    },
    {
      type: "WorkflowFinished",
      name: "learn_plan",
      wf_id: wfId,
      status: "ok",
      summary: "done",
    },
  ];
}

describe("WorkflowFinished → 检测到任务 → 走勾选弹窗（同 PlanProposed）", () => {
  beforeEach(() => {
    vi.useRealTimers();
    // 隔离：上一次用例残留的 plan 防抖定时器可能还在待触发。
    (window as unknown as Record<string, unknown>).__planDebounceTimer = undefined;
    document.body.replaceChildren();
  });

  afterEach(() => {
    // 防抖定时器跨用例泄漏 → 把残留定时器清掉。
    const pending = (window as unknown as Record<string, unknown>).__planDebounceTimer;
    if (pending) {
      clearTimeout(pending as number);
      (window as unknown as Record<string, unknown>).__planDebounceTimer = undefined;
    }
    document.getElementById("plan-import-modal")?.remove();
  });

  it("transcript 里含 26 个任务，按钮真的进 DOM（rocksdb 现场复现）", () => {
    const chat = mount();
    for (const ev of pushFinishedWithTasks("wf-rocksdb-26", 26)) chat.handleEvent(ev);

    const btns = document.querySelectorAll<HTMLButtonElement>(".workflow-import-btn");
    expect(btns).toHaveLength(1);
    const btn = btns[0];
    // 内容是 transcript 抽出来的 N = 26（与 PlanProposed 同格式）
    expect(btn.textContent).toContain("26 个");
    expect(btn.textContent).not.toContain("26 个任务"); // 旧「一把全导」格式
    // 不再挂在任何残留的 .msg-bubble 上（system 消息根本没有 .msg-bubble）
    expect(btn.closest(".msg-bubble")).toBeNull();
    // 真的挂在 system 消息的 .content 容器里 —— 这是修复后唯一能落点的地方。
    expect(btn.closest(".message.system > .content")).not.toBeNull();
  });

  it("点按钮打开勾选弹窗，不再 confirm() 一把全导", () => {
    vi.useFakeTimers();
    const chat = mount();
    for (const ev of pushFinishedWithTasks("wf-click-3", 3)) chat.handleEvent(ev);
    // 把防抖定时器先耗掉，免得它替按钮抢先弹窗。
    vi.advanceTimersByTime(2000);

    const btn = document.querySelector<HTMLButtonElement>(".workflow-import-btn");
    expect(btn).toBeTruthy();
    btn!.click();

    const modal = document.getElementById("plan-import-modal");
    expect(modal, "点 WorkflowFinished 的导入按钮应打开勾选弹窗").not.toBeNull();
    // 弹窗里要有 3 行勾选 + 「导入选中项」按钮。
    expect(modal!.querySelectorAll(".plan-import-row")).toHaveLength(3);
    expect(modal!.querySelector(".plan-import-go")).not.toBeNull();
  });

  it("live 路径 1.5s 防抖后自动开勾选弹窗", () => {
    vi.useFakeTimers();
    const chat = mount();
    for (const ev of pushFinishedWithTasks("wf-auto-2", 2)) chat.handleEvent(ev);

    // 防抖期内没弹
    vi.advanceTimersByTime(1400);
    expect(document.getElementById("plan-import-modal")).toBeNull();

    // 过了 1.5s 自动开
    vi.advanceTimersByTime(200);
    const modal = document.getElementById("plan-import-modal");
    expect(modal, "1.5s 防抖后应自动打开勾选弹窗").not.toBeNull();
    expect(modal!.querySelectorAll(".plan-import-row")).toHaveLength(2);
  });

  it("replay 路径不自动弹窗（历史里的旧清单骚扰）", () => {
    vi.useFakeTimers();
    const chat = mount();
    chat.replayEvents(pushFinishedWithTasks("wf-replay-5", 5));
    vi.advanceTimersByTime(5000);

    // 按钮还在（存档卡片还要能补救），但弹窗不自动出现
    expect(document.querySelectorAll(".workflow-import-btn")).toHaveLength(1);
    expect(document.getElementById("plan-import-modal")).toBeNull();
  });

  it("与 PlanProposed 共用防抖：连续 plan → workflow-finished 只弹最后一份", () => {
    vi.useFakeTimers();
    const chat = mount();
    chat.handleEvent({
      type: "PlanProposed",
      role_id: "manager",
      plan_id: "plan-A",
      tasks: [{ title: "A 任务" }],
    });
    // 0.5s 后又来一个 WorkflowFinished —— 应该把 PlanProposed 的弹窗计划覆盖掉
    vi.advanceTimersByTime(500);
    for (const ev of pushFinishedWithTasks("wf-B", 4)) chat.handleEvent(ev);

    // 1.4s 后还没到原 PlanProposed 的 1.5s 点 + WorkflowFinished 重置的
    // 新定时器（1500ms 延迟）= 2000ms 触发点
    vi.advanceTimersByTime(1400);
    expect(document.getElementById("plan-import-modal")).toBeNull();

    // 再 200ms（共 2100ms）越过 WorkflowFinished 触发点
    vi.advanceTimersByTime(200);

    const modal = document.getElementById("plan-import-modal");
    expect(modal).not.toBeNull();
    // wf-B 的任务是 4 个；plan-A 是 1 个 —— 行数对得上说明是 wf-B 胜出
    expect(modal!.querySelectorAll(".plan-import-row")).toHaveLength(4);
  });

  it("transcript 里没有 ```json``` 任务块 → 按钮不渲染", () => {
    const chat = mount();
    chat.handleEvent({ type: "WorkflowStarted", name: "wf", wf_id: "wf-x", topic: "t" });
    chat.handleEvent({
      type: "WorkflowTurn",
      wf_id: "wf-x",
      step_id: "breakdown",
      role_id: "manager",
      content: "没任务的纯文本。",
      round: 1,
    });
    chat.handleEvent({
      type: "WorkflowFinished",
      name: "wf",
      wf_id: "wf-x",
      status: "ok",
      summary: "done",
    });
    expect(document.querySelectorAll(".workflow-import-btn")).toHaveLength(0);
  });

  it("失败的 workflow 不渲染按钮（isFail 短路）", () => {
    const chat = mount();
    const tasks = [{ title: "x" }];
    chat.handleEvent({ type: "WorkflowStarted", name: "wf", wf_id: "wf-fail", topic: "t" });
    chat.handleEvent({
      type: "WorkflowTurn",
      wf_id: "wf-fail",
      step_id: "breakdown",
      role_id: "manager",
      content: "```json\n" + JSON.stringify({ tasks }) + "\n```",
      round: 1,
    });
    chat.handleEvent({
      type: "WorkflowFinished",
      name: "wf",
      wf_id: "wf-fail",
      status: "failed",
      summary: "OOM",
    });
    expect(document.querySelectorAll(".workflow-import-btn")).toHaveLength(0);
  });

  it("PlanProposed 实时路径的按钮也能挂上（不再被自动弹窗掩盖）", () => {
    vi.useFakeTimers();
    const chat = mount();
    chat.handleEvent({
      type: "PlanProposed",
      role_id: "manager",
      plan_id: "plan-rt-1",
      tasks: [{ title: "拆 ringbuf" }],
    });
    // 实时路径 1.5s 防抖后自动开弹窗 —— 但那跟按钮是否进 DOM 是两件事。
    vi.advanceTimersByTime(2000);
    const btns = document.querySelectorAll<HTMLButtonElement>(".workflow-import-btn");
    expect(btns).toHaveLength(1);
    expect(btns[0].closest(".message.system > .content")).not.toBeNull();
    expect(btns[0].closest(".msg-bubble")).toBeNull();
  });

  it("取消后再来同 wf_id 的 WorkflowFinished → 不再自动弹窗，也不重复刷消息", () => {
    vi.useFakeTimers();
    const chat = mount();
    for (const ev of pushFinishedWithTasks("wf-replay-same", 4)) chat.handleEvent(ev);
    vi.advanceTimersByTime(2000); // 第一次自动开弹窗

    let modal = document.getElementById("plan-import-modal");
    expect(modal).not.toBeNull();
    // 模拟用户点取消 → onClose("canceled") → wf_id 进入 importedOrDismissedWfs
    const cancelBtn = [...modal!.querySelectorAll<HTMLButtonElement>("button")]
      .find((b) => b.textContent === "取消");
    expect(cancelBtn, "弹窗应有「取消」按钮").toBeTruthy();
    cancelBtn!.click();
    expect(document.getElementById("plan-import-modal")).toBeNull();

    // 同 wf_id 再来一次（生产里可能是重连补发、补归档等）→ 不应自动弹，
    // 也不应再造一条「检测到 N 个 + 导入按钮」（消息流不要重复刷）。
    const btnsBefore = document.querySelectorAll(".workflow-import-btn").length;
    for (const ev of pushFinishedWithTasks("wf-replay-same", 4)) chat.handleEvent(ev);
    vi.advanceTimersByTime(3000);
    expect(
      document.getElementById("plan-import-modal"),
      "已处理过的 wf 不应再自动弹窗",
    ).toBeNull();
    expect(
      document.querySelectorAll(".workflow-import-btn").length,
      "已处理过的 wf 不应再造一条导入按钮（消息流不要重复刷）",
    ).toBe(btnsBefore);

    // 原有按钮仍在，点击可作为再入口打开弹窗
    const btns = document.querySelectorAll<HTMLButtonElement>(".workflow-import-btn");
    expect(btns).toHaveLength(1);
    btns[0].click();
    modal = document.getElementById("plan-import-modal");
    expect(modal, "按钮应仍能作为再入口打开弹窗").not.toBeNull();
  });
});