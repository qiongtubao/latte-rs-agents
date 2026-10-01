// 重启后会话日志停在 WorkflowStarted、没有 WorkflowFinished。
// 顶部 ▶ 应可点，并走 resumeSessionV2 从 checkpoint 续跑。
import { describe, it, expect, vi, beforeEach } from "vitest";
import { mountChat } from "./chat_impl";
import type { ChatEvent } from "./api";

const resumeSessionV2 = vi.fn(async () => {});
vi.mock("./api", async () => {
  const actual = await vi.importActual<typeof import("./api")>("./api");
  return {
    ...actual,
    resumeSessionV2: () => resumeSessionV2(),
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
  return { chat, resumeBtn: container.resumeBtn };
}

const started: ChatEvent = {
  type: "WorkflowStarted",
  name: "learn_loop",
  topic: "size class",
  wf_id: "wf-learn-1",
};

beforeEach(() => {
  resumeSessionV2.mockClear();
});

describe("中断的 workflow 可以点继续", () => {
  it("回放停在未结束的 workflow 时启用 ▶，点击走续跑", async () => {
    const { chat, resumeBtn } = mount();
    expect(resumeBtn.disabled).toBe(true);
    chat.replayEvents([started]);
    expect(resumeBtn.disabled).toBe(false);
    resumeBtn.click();
    await vi.waitFor(() => expect(resumeSessionV2).toHaveBeenCalledTimes(1));
  });

  it("已经 WorkflowFinished 的不打开 ▶", () => {
    const { chat, resumeBtn } = mount();
    chat.replayEvents([
      started,
      { type: "WorkflowFinished", name: "learn_loop", wf_id: "wf-learn-1", status: "ok", summary: "" },
    ]);
    expect(resumeBtn.disabled).toBe(true);
  });

  it("本进程流水线还活着时不打开 ▶", () => {
    const { chat, resumeBtn } = mount();
    chat.replayEvents([started], { workflowActive: true });
    expect(resumeBtn.disabled).toBe(true);
  });

  it("续跑 409 视为仍在跑，不再把 ▶ 打开", async () => {
    const { HttpError } = await import("./transport");
    resumeSessionV2.mockRejectedValueOnce(
      new HttpError("POST", "/api/chat/resume-session", 409, "already running"),
    );
    const { chat, resumeBtn } = mount();
    chat.replayEvents([started]);
    expect(resumeBtn.disabled).toBe(false);
    resumeBtn.click();
    await vi.waitFor(() => expect(resumeSessionV2).toHaveBeenCalledTimes(1));
    expect(resumeBtn.disabled).toBe(true);
  });
});
