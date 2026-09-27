// 弹框事件（ChoiceRequested / PlanProposed）的补齐与去重（chat_impl.ts）。
//
// 弹框比普通事件脆弱：broadcast 是「没订阅者就丢弃」的，`Lagged` 掉的
// 那几条既不进 SSE 也不进 archiver 的 event_log。补齐之后弹框有了三条
// 到达路径 —— 实时 SSE、history 重放、挂起弹框补拉（SSE 新连接的
// replay 前缀 + `GET /api/chat/pending-prompts`）—— 同一个
// choice_id / plan_id 会被送来多次。这里锁死三件事：
//   1. 同 id 只渲染一张卡（否则同一个问题在聊天区出现好几遍）；
//   2. 重放出来的是**存档态**（不可点）：它大概率早答过了，给可点的
//      卡片只会让用户白点一次（提交必然 404 降级成一条莫名的新消息）；
//      而后端确认仍挂起的那条（补拉的 live 事件）要**替换**存档卡片，
//      恢复可交互；
//   3. 提交/跳过后调 prompt-dismiss 销账，重连不再补出僵尸框。
import { describe, it, expect, vi, beforeEach } from "vitest";
import { mountChat } from "./chat_impl";
import type { ChatEvent } from "./api";

const sendMessage = vi.fn(async (_text: string) => {});
const dismissPrompt = vi.fn(async (_promptId: string) => {});
// plan import 重复防护：服务器返回 409 时前端应降级为「另一 tab 已经成功」
// 而不是弹 alert 报错。
class HttpError409 extends Error {
  readonly status = 409;
  constructor(msg = "plan x 已经被本次 server 导入过一次（重复导入防护）") {
    super(msg);
    this.name = "HttpError";
  }
}
let importTasksMock = vi.fn(async () => {
  throw new HttpError409();
});
vi.mock("./api", async () => {
  const actual = await vi.importActual<typeof import("./api")>("./api");
  return {
    ...actual,
    dismissPrompt: (promptId: string) => dismissPrompt(promptId),
    sendMessage: (text: string) => sendMessage(text),
    importTasks: (...args: unknown[]) =>
      (importTasksMock as (...args: unknown[]) => Promise<unknown>)(...args),
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

const choice = (choiceId: string, wait = false): ChatEvent => ({
  type: "ChoiceRequested",
  role_id: "programmer",
  choice_id: choiceId,
  question: "先重构还是先加功能？",
  multi: false,
  layout: "",
  allow_upload: false,
  wait,
  options: [
    { label: "先重构" },
    { label: "先加功能" },
  ],
});

const plan = (planId: string): ChatEvent => ({
  type: "PlanProposed",
  role_id: "manager",
  plan_id: planId,
  tasks: [{ title: "拆 ringbuf" }],
});

/** 找出某个弹框渲染出的卡片。
 *
 *  注意作用域：**可交互**（live）卡片会被搬进居中弹窗（挂在
 *  `document.body` 上，不在 messagesEl 里），只有存档卡片留在消息流。
 *  所以默认按整个文档找，需要区分位置的用例显式传 `messagesEl`。 */
const cards = (root: HTMLElement | Document, id: string): HTMLElement[] =>
  [...root.querySelectorAll(`.choice-card[data-choice-id="${id}"]`)] as HTMLElement[];

describe("弹框补齐与去重", () => {
  beforeEach(() => {
    dismissPrompt.mockClear();
    sendMessage.mockClear();
    vi.useRealTimers();
  });

  it("同一 choice_id 重复到达只渲染一张卡", () => {
    const { chat } = mount();
    chat.handleEvent(choice("choice-programmer-1"));
    // SSE 新连接的 replay 前缀 + GET pending-prompts 会各送一遍。
    chat.handleEvent(choice("choice-programmer-1"));
    chat.handleEvent(choice("choice-programmer-1"));
    expect(cards(document, "choice-programmer-1")).toHaveLength(1);
  });

  it("history 重放渲染成存档态（不可点），补拉的 live 事件替换成可交互", () => {
    const { chat, messagesEl } = mount();
    chat.replayEvents([choice("choice-programmer-2")]);
    const archived = cards(messagesEl, "choice-programmer-2");
    expect(archived).toHaveLength(1);
    expect(archived[0].classList.contains("answered")).toBe(true);
    // 存档态不给可点按钮：选项区与操作栏都收起。
    expect((archived[0].querySelector(".choice-opts") as HTMLElement).style.display).toBe("none");
    expect((archived[0].querySelector(".choice-foot") as HTMLElement).style.display).toBe("none");

    // pending-prompts 说它其实还没答 → 替换成可交互卡片（仍只有一张），
    // 并立刻弹进居中弹窗（所以按整个文档找，不是 messagesEl）。
    chat.handleEvent(choice("choice-programmer-2"));
    const live = cards(document, "choice-programmer-2");
    expect(live).toHaveLength(1);
    expect(live[0].classList.contains("answered")).toBe(false);
    expect((live[0].querySelector(".choice-opts") as HTMLElement).style.display).not.toBe("none");
    expect(document.querySelector(".choice-overlay"), "live 卡片应在弹窗里").toBeTruthy();
  });

  it("提交选择后调 prompt-dismiss 销账（重连不再补出僵尸框）", () => {
    const { chat } = mount();
    chat.handleEvent(choice("choice-programmer-3"));
    const card = cards(document, "choice-programmer-3")[0];
    (card.querySelector(".choice-opt") as HTMLElement).click();
    (card.querySelector(".choice-submit") as HTMLButtonElement).click();
    expect(dismissPrompt).toHaveBeenCalledWith("choice-programmer-3");
  });

  it("跳过也算已处理，同样销账", () => {
    const { chat } = mount();
    chat.handleEvent(choice("choice-programmer-4"));
    const card = cards(document, "choice-programmer-4")[0];
    (card.querySelector(".choice-skip") as HTMLButtonElement).click();
    expect(dismissPrompt).toHaveBeenCalledWith("choice-programmer-4");
  });

  it("重放的 PlanProposed 不自动弹模态框，实时/补拉的才弹", () => {
    vi.useFakeTimers();
    const { chat } = mount();
    chat.replayEvents([plan("plan-manager-1")]);
    vi.advanceTimersByTime(2000);
    expect(document.getElementById("plan-import-modal")).toBeNull();

    chat.handleEvent(plan("plan-manager-1"));
    vi.advanceTimersByTime(2000);
    expect(document.getElementById("plan-import-modal")).not.toBeNull();
    vi.useRealTimers();
  });

  it("clear 会带走挂在 body 上的 plan 弹窗与排队中的自动弹窗", () => {
    vi.useFakeTimers();
    const { chat } = mount();
    chat.handleEvent(plan("plan-manager-2"));
    vi.advanceTimersByTime(2000);
    expect(document.getElementById("plan-import-modal")).not.toBeNull();
    // 切 session：弹窗挂在 document.body 上，messagesEl.innerHTML=""
    // 清不掉它，而它内部的 sid/parentId 是旧 session 的。
    chat.clear();
    expect(document.getElementById("plan-import-modal")).toBeNull();

    // 已排队但没触发的定时器同理（1.5s 内切了 session）。
    chat.handleEvent(plan("plan-manager-3"));
    chat.clear();
    vi.advanceTimersByTime(2000);
    expect(document.getElementById("plan-import-modal")).toBeNull();
    vi.useRealTimers();
  });

  it("clear 后同一弹框能重新渲染（去重表不能跨会话留存）", () => {
    const { chat } = mount();
    chat.handleEvent(choice("choice-programmer-5"));
    expect(cards(document, "choice-programmer-5")).toHaveLength(1);
    chat.clear();
    // clear 连弹窗一起带走（卡片 + 遮罩都不留）。
    expect(cards(document, "choice-programmer-5")).toHaveLength(0);
    expect(document.querySelector(".choice-overlay")).toBeNull();
    // 重放/补拉必须能再渲染出来，否则聊天区永远缺这一条。
    chat.handleEvent(choice("choice-programmer-5"));
    expect(cards(document, "choice-programmer-5")).toHaveLength(1);
  });

  it("重启后（历史里有弹框、pending 表已空）可经「仍要回答」复活，强制走普通消息", () => {
    const { chat, messagesEl } = mount();
    // 重启后的恢复路径：只有 history 重放，pending-prompts 返回空
    //（PENDING/PROMPTS 是内存表，随进程一起没了）。
    chat.replayEvents([choice("choice-programmer-6", /* wait */ true)]);
    const card = cards(messagesEl, "choice-programmer-6")[0];
    // 默认不可点：历史里绝大多数是已答过的题。
    expect((card.querySelector(".choice-foot") as HTMLElement).style.display).toBe("none");

    const revive = card.querySelector(".choice-revive") as HTMLButtonElement;
    expect(revive).not.toBeNull();
    // wait=true 的存档卡片要说清原提问方已经不在等了。
    expect(revive.textContent).toContain("原提问方已不在等待");
    revive.click();
    expect((card.querySelector(".choice-foot") as HTMLElement).style.display).not.toBe("none");

    (card.querySelector(".choice-opt") as HTMLElement).click();
    (card.querySelector(".choice-submit") as HTMLButtonElement).click();
    // 关键：即便 wait=true 也**不**打 choice 路由（等待方已随进程消失，
    // 必然 404），直接作为新的 user 消息回喂当前角色。
    expect(sendMessage).toHaveBeenCalledWith("我的选择：先重构");
  });

describe("plan import 重复防护：409 降级", () => {
  it("服务器 409 时不弹 alert：本地 plan 卡摘除 + 系统消息提示 + 关弹窗", async () => {
    vi.useFakeTimers();
    importTasksMock.mockImplementation(() => {
      throw new HttpError409();
    });
    const { chat } = mount();
    // 触发实时 PlanProposed → setTimeout(1500ms) 自动弹窗。
    chat.handleEvent(plan("plan-dup-1"));
    vi.advanceTimersByTime(2000);
    expect(document.getElementById("plan-import-modal")).not.toBeNull();

    // 用户点「导入」按钮 → importTasks 走 mock 抛 409。
    const importBtn = document.querySelector<HTMLButtonElement>(
      "#plan-import-modal .plan-import-go",
    );
    expect(importBtn).not.toBeNull();
    // 直接调 click 触发 — 但 chat 里的 alert 仍然是 window.alert，jsdom
    // 会抛 not implemented。把它打掉，再触发。
    const alertSpy = vi.spyOn(window, "alert").mockImplementation(() => undefined);
    try {
      importBtn!.click();
      // flush microtasks：importTasks 抛 → catch 里的 addMessage + removePrompt。
      await Promise.resolve();
      await Promise.resolve();
      // 弹窗已关、本地 prompt 卡已摘。
      expect(document.getElementById("plan-import-modal")).toBeNull();
      // 系统消息：降级提示出现。
      expect(document.body.textContent).toContain(
        "另一个 tab 已经成功导入过这份清单",
      );
      // dismissPrompt 没被调（重复防护是 409 路径，不走 broadcast）。
      expect(dismissPrompt).not.toHaveBeenCalled();
    } finally {
      alertSpy.mockRestore();
    }
  });
});
});

