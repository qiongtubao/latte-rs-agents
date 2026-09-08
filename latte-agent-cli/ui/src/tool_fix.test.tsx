// tool_fix 人工介入弹窗的 DOM 渲染 + 提交流程测试。
//
// 背景：tool_fix（latte-agent-core 的 Scheme A 触发 HIL）通过
// `ChatEvent::ToolFixRequested` 把 choice 路由复用——前端只需渲染一个
// 等宽编辑器让用户改 JSON 字符串，提交时 POST choice-answer 直达挂起
// 方；放弃走 prompt-dismiss。回归 2026-09-07 jemalloc 会话：模型反复
// 写出坏 JSON、subsession 修复都救不回来时（PERMANENT_BREAK_AT 阈值），
// 让用户直接修比让流程判失败再返工更省 token。
import { describe, it, expect, vi, beforeEach } from "vitest";
import { mountChat } from "./chat_impl";
import type { ChatEvent } from "./api";

const answered: Array<{ choiceId: string; answer: string }> = [];
const dismissed: string[] = [];
const sent: string[] = [];
vi.mock("./api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./api")>();
  return {
    ...actual,
    sendMessage: (msg: string) => { sent.push(msg); return Promise.resolve(); },
    sendChoiceAnswer: (choiceId: string, answer: string) => {
      answered.push({ choiceId, answer });
      return Promise.resolve(true);
    },
    dismissPrompt: (id: string) => { dismissed.push(id); return Promise.resolve(); },
    listWorkflows: () => Promise.resolve([]),
    listTasks: () => Promise.resolve([]),
  };
});

function mount() {
  document.body.replaceChildren();
  const el = <T extends HTMLElement>(tag: string) =>
    document.createElement(tag) as unknown as T;
  const statusPill = el<HTMLElement>("div");
  const statusLabel = el<HTMLElement>("span");
  statusLabel.className = "status-label";
  const statusTime = el<HTMLElement>("span");
  statusTime.className = "status-time";
  statusPill.append(statusLabel, statusTime);
  const container = {
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
  document.body.appendChild(container.messagesEl);
  for (const id of [
    "cmd-autocomplete", "role-autocomplete", "contextMenu", "editOverlay",
    "editTextarea", "editSave", "editCancel", "editCancelTop", "subagentLog",
    "subagentOverlay",
  ]) {
    const node = document.createElement(id === "editTextarea" ? "textarea" : "div");
    node.id = id;
    document.body.appendChild(node);
  }
  const chat = mountChat({ container, initialRole: "manager" });
  return { chat, messagesEl: container.messagesEl };
}

function toolFixEvent(
  opts: Partial<Extract<ChatEvent, { type: "ToolFixRequested" }>> = {},
): ChatEvent {
  return {
    type: "ToolFixRequested",
    role_id: opts.role_id ?? "manager",
    choice_id: opts.choice_id ?? "toolfix-plan-1",
    tool_name: opts.tool_name ?? "plan",
    malformed_args: opts.malformed_args ?? '{"path": "/foo/bar",',
    error_detail: opts.error_detail ?? "unexpected end of JSON input",
    wait: opts.wait ?? true,
  };
}

const card = () => document.querySelector(".tool-fix-card");
const overlay = () => document.querySelector(".choice-overlay");
const editor = () =>
  document.querySelector<HTMLTextAreaElement>(".tool-fix-card__editor");
const broken = () =>
  document.querySelector<HTMLElement>(".tool-fix-card__broken");
const submitBtn = () =>
  document.querySelector<HTMLButtonElement>(
    ".tool-fix-card__foot .choice-submit",
  );
const cancelBtn = () =>
  document.querySelector<HTMLButtonElement>(
    ".tool-fix-card__foot .tool-fix-card__cancel",
  );

beforeEach(() => {
  answered.length = 0;
  dismissed.length = 0;
  sent.length = 0;
});

describe("tool_fix 人工介入弹窗", () => {
  it("wait=true 弹模态，预填坏参数，可直接改", () => {
    const { chat } = mount();
    chat.handleEvent(toolFixEvent());
    expect(overlay(), "应弹出居中模态").toBeTruthy();
    expect(card()).toBeTruthy();
    expect(broken()?.textContent).toContain('"path": "/foo/bar",');
    expect(editor()?.value).toBe('{"path": "/foo/bar",');
    expect(editor()?.tagName).toBe("TEXTAREA");
    expect(editor()?.spellcheck).toBe(false);
  });

  it("提交：sendChoiceAnswer(choice_id, 修正后文本)", async () => {
    const { chat } = mount();
    chat.handleEvent(toolFixEvent());
    const ta = editor();
    if (!ta) throw new Error("missing editor");
    ta.value = '{"path": "/foo/bar"}';
    submitBtn()?.click();
    await Promise.resolve();
    await Promise.resolve();
    expect(answered).toEqual([
      { choiceId: "toolfix-plan-1", answer: '{"path": "/foo/bar"}' },
    ]);
    expect(dismissed).toContain("toolfix-plan-1");
    expect(card()?.classList.contains("answered")).toBe(true);
    expect(submitBtn()?.disabled).toBe(true);
  });

  it("放弃：仅 dismissPrompt，不发 sendChoiceAnswer", async () => {
    const { chat } = mount();
    chat.handleEvent(toolFixEvent());
    cancelBtn()?.click();
    await Promise.resolve();
    expect(answered, "放弃不该发答案").toEqual([]);
    expect(dismissed).toContain("toolfix-plan-1");
    expect(card()?.classList.contains("answered")).toBe(true);
  });

  it("wait=false：仅 console.warn，不挂起、不弹模态", () => {
    const { chat } = mount();
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    chat.handleEvent(toolFixEvent({ wait: false }));
    expect(card(), "wait=false 不该渲染卡片").toBeNull();
    expect(overlay(), "wait=false 不该有弹窗").toBeNull();
    expect(warn).toHaveBeenCalled();
    warn.mockRestore();
  });

  it("坏参数按纯文本渲染，不解释 HTML（防 XSS）", () => {
    const { chat } = mount();
    chat.handleEvent(
      toolFixEvent({ malformed_args: '<img src=x onerror=alert(1)>{"a": 1}' }),
    );
    expect(document.querySelector(".tool-fix-card__broken img")).toBeNull();
    expect(broken()?.textContent).toBe('<img src=x onerror=alert(1)>{"a": 1}');
  });

  it("历史回放（replaying）渲染为 answered 态，不弹模态", () => {
    const { chat } = mount();
    chat.replayEvents([toolFixEvent()]);
    expect(overlay()).toBeNull();
    expect(card()?.classList.contains("answered")).toBe(true);
    expect(submitBtn()?.disabled).toBe(true);
  });
});