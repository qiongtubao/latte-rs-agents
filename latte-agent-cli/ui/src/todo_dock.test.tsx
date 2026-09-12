// TodoUpdated 事件的渲染自测（chat_impl.ts）：后端事件并行开发中，
// 这里按契约手工构造事件驱动 mountChat，验证 —
//   1. 消息流里的紧凑 todo 卡「原地更新同一条」，不刷屏；
//   2. composer 上方的 dock 仅在有 todo 时显示，折叠/展开交互；
//   3. 回放（replayEvents）按最后一条 TodoUpdated 恢复状态；
//   4. 子会话（sub_id 非空）清单被忽略；clear() 归零。
import { describe, it, expect, beforeEach } from "vitest";
import { mountChat } from "./chat_impl";
import type { ChatEvent, TodoPhase } from "./api";

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
  // dock 挂在 chat-form 前面：需要 form 有父节点（模拟 .chat-container）。
  const chatContainer = document.createElement("div");
  chatContainer.append(container.messagesEl, container.formEl);
  document.body.appendChild(chatContainer);
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
  return { chat, messagesEl: container.messagesEl, formEl: container.formEl };
}

const todoEvent = (phases: TodoPhase[], sub_id?: string | null): ChatEvent => ({
  type: "TodoUpdated",
  phases,
  sub_id: sub_id ?? null,
});

const twoPhases: TodoPhase[] = [
  {
    name: "设计",
    tasks: [
      { content: "定契约", status: "completed" },
      { content: "画原型", status: "completed" },
    ],
  },
  {
    name: "实现",
    tasks: [
      { content: "写 store", status: "in_progress" },
      { content: "写 UI", status: "pending" },
    ],
  },
];

describe("TodoUpdated 渲染", () => {
  beforeEach(() => {
    document.body.replaceChildren();
  });

  it("内联卡片原地更新同一条 status 行", () => {
    const { chat, messagesEl } = mount();
    chat.handleEvent(todoEvent(twoPhases));
    chat.handleEvent(todoEvent([
      twoPhases[0],
      { name: "实现", tasks: [
        { content: "写 store", status: "completed" },
        { content: "写 UI", status: "in_progress" },
      ] },
    ]));
    const cards = messagesEl.querySelectorAll(".message.status");
    expect(cards).toHaveLength(1);
    expect(cards[0].textContent).toBe("📝 Todos 3/4 · 当前: 写 UI");
  });

  it("dock 插在 chat-form 前，折叠态一行，点击展开按 phase 分组", () => {
    const { chat, formEl } = mount();
    chat.handleEvent(todoEvent(twoPhases));
    const dock = document.querySelector(".todo-dock") as HTMLElement;
    expect(dock).toBeTruthy();
    // 位于 composer（chat-form）正上方
    expect(dock.nextElementSibling).toBe(formEl);
    expect(dock.style.display).not.toBe("none");
    // 折叠态：body 隐藏，summary 显示计数 + 当前活动项
    const body = dock.querySelector(".todo-dock-body") as HTMLElement;
    expect(body.style.display).toBe("none");
    expect(dock.querySelector(".todo-dock-title")!.textContent).toBe("📝 Todos 2/4");
    expect(dock.querySelector(".todo-dock-current")!.textContent).toBe("当前: 写 store");
    // 展开
    (dock.querySelector(".todo-dock-summary") as HTMLButtonElement).click();
    expect(body.style.display).not.toBe("none");
    const phaseHeads = body.querySelectorAll(".todo-phase-head");
    expect(phaseHeads).toHaveLength(2);
    // 完成的 phase（设计 2/2）默认折叠成一行，未完成的（实现）展开任务列表
    expect(phaseHeads[0].textContent).toContain("设计");
    expect(phaseHeads[0].textContent).toContain("2/2");
    const sections = body.querySelectorAll(".todo-phase");
    expect(sections[0].querySelectorAll(".todo-task")).toHaveLength(0);
    const tasks = sections[1].querySelectorAll(".todo-task");
    expect(tasks).toHaveLength(2);
    expect(tasks[0].classList.contains("todo-task--in_progress")).toBe(true);
    expect(tasks[1].classList.contains("todo-task--pending")).toBe(true);
    // 点击完成的 phase 头可展开它（点击后 body 整体重渲染，需重新查询）
    (phaseHeads[0] as HTMLButtonElement).click();
    const sectionsAfter = body.querySelectorAll(".todo-phase");
    expect(sectionsAfter[0].querySelectorAll(".todo-task")).toHaveLength(2);
    expect(
      sectionsAfter[0].querySelector(".todo-task--completed .todo-task-text")!.textContent,
    ).toBe("定契约");
  });

  it("回放按最后一条恢复；清空（空 phases）后 dock 隐藏、卡片显示已清空", () => {
    const { chat, messagesEl } = mount();
    chat.replayEvents([
      todoEvent(twoPhases),
      todoEvent([{ name: "P", tasks: [{ content: "最终", status: "completed" }] }]),
    ]);
    const cards = messagesEl.querySelectorAll(".message.status");
    expect(cards).toHaveLength(1);
    expect(cards[0].textContent).toBe("📝 Todos 1/1 · 全部完成");
    chat.handleEvent(todoEvent([]));
    const dock = document.querySelector(".todo-dock") as HTMLElement;
    expect(dock.style.display).toBe("none");
    expect(messagesEl.querySelectorAll(".message.status")).toHaveLength(1);
    expect(messagesEl.querySelector(".message.status")!.textContent).toBe("📝 Todos 已清空");
  });

  it("子会话（sub_id 非空）清单被忽略", () => {
    const { chat, messagesEl } = mount();
    chat.handleEvent(todoEvent(twoPhases, "sub-1"));
    expect(messagesEl.querySelectorAll(".message.status")).toHaveLength(0);
    expect(document.querySelector(".todo-dock")).toBeNull();
  });

  it("clear() 归零：dock 隐藏、卡片引用失效", () => {
    const { chat, messagesEl } = mount();
    chat.handleEvent(todoEvent(twoPhases));
    chat.clear();
    const dock = document.querySelector(".todo-dock") as HTMLElement;
    expect(dock.style.display).toBe("none");
    expect(messagesEl.querySelectorAll(".message.status")).toHaveLength(0);
  });
});
