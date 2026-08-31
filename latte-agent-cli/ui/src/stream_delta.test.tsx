// 流式 delta 的气泡归属（chat_impl.ts）。
//
// 两个叠在一起的旧 bug：
//   1. 后端 `ChatEventTraceSink` 对 `ModelDelta` 把 `sub_id` 写死成
//      None（同一个函数里 ToolUse/ToolResult 都正确带了 sub_id）——
//      子代理的逐 token 流以主角色身份出现，而它的工具行带 sub_id，
//      同一段对话被拆到两处。
//   2. 前端流式气泡按**裸 role_id** 索引 —— DAG 并行波里两个分派命中
//      同一个 role 时，两路 token 交错写进同一个气泡，正文混成一团。
// 修法是两边一起改：后端带上 sub_id，前端 key 改成 role_id + sub_id。
//
// 另外锁死：分派结束**不再**自动发 switchRole("manager")。那是一次真
// 实 HTTP 调用，会把服务端当前角色改掉，静默覆盖用户手动选的角色。
import { describe, it, expect, vi, beforeEach } from "vitest";
import { mountChat } from "./chat_impl";
import type { ChatEvent } from "./api";

const switchRole = vi.fn(async (_roleId: string) => {});
vi.mock("./api", async () => {
  const actual = await vi.importActual<typeof import("./api")>("./api");
  return { ...actual, switchRole: (roleId: string) => switchRole(roleId) };
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
    "cmd-autocomplete", "role-autocomplete", "contextMenu", "editOverlay",
    "editTextarea", "editSave", "editCancel", "editCancelTop", "subagentLog",
    "subagentOverlay",
  ]) {
    const el = document.createElement(id === "editTextarea" ? "textarea" : "div");
    el.id = id;
    document.body.appendChild(el);
  }
  const chat = mountChat({ container, initialRole: "manager" });
  return { chat, messagesEl: container.messagesEl, rolePill: container.rolePill };
}

const delegateStart = (subId: string, role = "programmer", wfId?: string): ChatEvent => ({
  type: "DelegateStarted",
  from_role: "manager",
  to_role: role,
  task: `任务 ${subId}`,
  sub_id: subId,
  ...(wfId ? { wf_id: wfId } : {}),
});

const delta = (role: string, content: string, subId?: string): ChatEvent => ({
  type: "RoleTurn",
  role_id: role,
  content,
  is_complete: false,
  ...(subId ? { sub_id: subId } : {}),
});

const final = (role: string, content: string, subId?: string): ChatEvent => ({
  type: "RoleTurn",
  role_id: role,
  content,
  is_complete: true,
  ...(subId ? { sub_id: subId } : {}),
});

/** 所有角色气泡的正文（按出现顺序）。 */
const roleTexts = (root: HTMLElement): string[] =>
  [...root.querySelectorAll(".message-row.role .msg-content")].map(
    (el) => (el as HTMLElement).textContent?.trim() ?? "",
  );

describe("流式 delta 的气泡归属", () => {
  beforeEach(() => {
    switchRole.mockClear();
  });

  it("同一 role 的两个并行分派：token 交错也不混进同一个气泡", () => {
    const { chat, messagesEl } = mount();
    chat.handleEvent(delegateStart("sub-A"));
    chat.handleEvent(delegateStart("sub-B"));

    // 交错到达（DAG 并行波的真实形态）。
    chat.handleEvent(delta("programmer", "A1", "sub-A"));
    chat.handleEvent(delta("programmer", "B1", "sub-B"));
    chat.handleEvent(delta("programmer", "A2", "sub-A"));
    chat.handleEvent(delta("programmer", "B2", "sub-B"));

    const texts = roleTexts(messagesEl);
    expect(texts).toContain("A1A2");
    expect(texts).toContain("B1B2");
    // 旧行为：两路挤进一个气泡，正文是 "A1B1A2B2"。
    expect(texts.some((t) => t.includes("A1B1"))).toBe(false);
  });

  it("主角色（无 sub_id）与子代理（有 sub_id）分属不同气泡", () => {
    const { chat, messagesEl } = mount();
    chat.handleEvent(delegateStart("sub-1", "manager"));
    chat.handleEvent(delta("manager", "主", undefined));
    chat.handleEvent(delta("manager", "子", "sub-1"));

    const texts = roleTexts(messagesEl);
    expect(texts).toContain("主");
    expect(texts).toContain("子");
  });

  it("终态 RoleTurn 收口的是自己那个气泡，不误收兄弟的", () => {
    const { chat, messagesEl } = mount();
    chat.handleEvent(delegateStart("sub-A"));
    chat.handleEvent(delegateStart("sub-B"));
    chat.handleEvent(delta("programmer", "A1", "sub-A"));
    chat.handleEvent(delta("programmer", "B1", "sub-B"));
    // A 收口成完整文本；B 的气泡不受影响，仍是它自己的增量。
    chat.handleEvent(final("programmer", "A 的完整回答", "sub-A"));

    const texts = roleTexts(messagesEl);
    expect(texts).toContain("A 的完整回答");
    expect(texts).toContain("B1");
  });

  it("分派结束不再自动 switchRole（不覆盖用户手动选的角色）", () => {
    const { chat, messagesEl, rolePill } = mount();
    chat.handleEvent(delegateStart("sub-A"));
    chat.handleEvent(final("programmer", "干完了", "sub-A"));
    // 徽标可以回到 manager（纯显示），但绝不能发 HTTP 改服务端角色。
    expect(switchRole).not.toHaveBeenCalled();
    expect(rolePill.textContent).toContain("manager");

    chat.handleEvent({ type: "RoleStarted", role_id: "manager", detail: "next main turn" });
    const latestStarted = [...messagesEl.querySelectorAll<HTMLElement>(".message-row")]
      .filter((row) => row.textContent?.includes("manager 开始执行"))
      .at(-1);
    expect(latestStarted?.dataset.delegate).not.toBe("true");
    expect(latestStarted?.dataset.subId ?? "").toBe("");
  });

  it("错误终态清流缓存：下一轮同 key 不拼接旧 partial", () => {
    const { chat, messagesEl } = mount();
    chat.handleEvent({ type: "RoleStarted", role_id: "manager", detail: "turn 1" });
    chat.handleEvent(delta("manager", "旧 partial"));
    chat.handleEvent({ type: "RoleFinished", role_id: "manager", detail: "error" });
    chat.handleEvent({ type: "Error", message: "cancelled" });
    chat.handleEvent({ type: "RoleStarted", role_id: "manager", detail: "turn 2" });
    chat.handleEvent(delta("manager", "新 partial"));

    const texts = roleTexts(messagesEl);
    expect(texts).toContain("旧 partial");
    expect(texts).toContain("新 partial");
    expect(texts).not.toContain("旧 partial新 partial");
  });

  it("WorkflowFinished 回收被 abort_all 丢弃、未发 DelegateFinished 的 sibling", () => {
    const { chat, messagesEl } = mount();
    chat.handleEvent(delegateStart("sub-wf", "programmer", "wf-1"));
    chat.handleEvent(delta("programmer", "半截", "sub-wf"));
    chat.handleEvent({
      type: "WorkflowFinished",
      wf_id: "wf-1",
      name: "review",
      status: "failed",
      summary: "one sibling failed",
    });

    const badge = messagesEl.querySelector<HTMLElement>(
      '.message-row[data-sub-id="sub-wf"] .delegate-state',
    );
    expect(badge?.textContent).toContain("failed");
    expect(badge?.classList.contains("failed")).toBe(true);
  });

  it("顶层 Error 回收父 future drop 后残留的 active delegate", () => {
    const { chat, messagesEl } = mount();
    chat.handleEvent(delegateStart("sub-drop"));
    chat.handleEvent({ type: "Error", message: "turn cancelled by user" });

    const badge = messagesEl.querySelector<HTMLElement>(
      '.message-row[data-sub-id="sub-drop"] .delegate-state',
    );
    expect(badge?.textContent).toContain("父任务中断");
    expect(badge?.classList.contains("failed")).toBe(true);
  });

  it("顶层 Error 不越权退休同事件流上的独立 workflow delegate", () => {
    const { chat, messagesEl } = mount();
    chat.handleEvent(delegateStart("sub-turn"));
    chat.handleEvent(delegateStart("sub-workflow", "reviewer", "wf-live"));
    chat.handleEvent({ type: "Error", message: "main turn failed" });

    const turnBadge = messagesEl.querySelector<HTMLElement>(
      '.message-row[data-sub-id="sub-turn"] .delegate-state',
    );
    const workflowBadge = messagesEl.querySelector<HTMLElement>(
      '.message-row[data-sub-id="sub-workflow"] .delegate-state',
    );
    expect(turnBadge?.classList.contains("failed")).toBe(true);
    expect(workflowBadge?.textContent).toBe("⏳ 执行中…");
    expect(workflowBadge?.classList.contains("pending")).toBe(true);
  });

  it("retire 当前 legacy sub_id 后下一主 RoleStarted 不继承旧分派", () => {
    const { chat, messagesEl } = mount();
    chat.handleEvent(delegateStart("sub-retired"));
    chat.handleEvent({ type: "Error", message: "parent dropped" });
    chat.handleEvent({ type: "RoleStarted", role_id: "manager", detail: "next main turn" });

    const startedRows = [...messagesEl.querySelectorAll<HTMLElement>(".message-row")]
      .filter((row) => row.textContent?.includes("manager 开始执行"));
    const latest = startedRows[startedRows.length - 1];
    expect(latest?.dataset.delegate).not.toBe("true");
    expect(latest?.dataset.subId ?? "").toBe("");
  });
});
