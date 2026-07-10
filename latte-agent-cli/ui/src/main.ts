// 主入口：把 chat / trace / self-loop 三个面板粘到一起 + 启 SSE 订阅。

import {
  getSession,
  subscribeEvents,
} from "./api";
import { mountChat } from "./chat";
import { mountTrace } from "./trace";
import { mountSelfLoop } from "./self-loop";

function $(id: string): HTMLElement {
  const el = document.getElementById(id);
  if (!el) throw new Error(`#${id} not found`);
  return el;
}

async function main(): Promise<void> {
  // 1. 拉初始 session。
  let session;
  try {
    session = await getSession();
  } catch (e) {
    showFatal(`failed to reach /api/session: ${String(e)}`);
    return;
  }

  // 2. 起 chat 面板。onReconnect 是占位 —— subscribeEvents 要到下
  //    面才调，那时才能拿到真的 reconnect()。所以这里用一个
  //    reconnectRef 闭包：mountChat 内部点击药丸会调
  //    opts.onReconnect()，而 opts.onReconnect 通过 reconnectRef
  //    间接找到最新的 reconnect。
  let reconnectRef: () => void = () => {};
  const chat = mountChat({
    container: {
      messagesEl: $("messages"),
      formEl: $("chat-form") as HTMLFormElement,
      inputEl: $("chat-input") as HTMLTextAreaElement,
      sendBtn: $("chat-send") as HTMLButtonElement,
      clearBtn: $("chat-clear") as HTMLButtonElement,
      quitBtn: $("chat-quit") as HTMLButtonElement,
      statusPill: $("status-pill"),
      roleSelect: $("role-select") as HTMLSelectElement,
      rolePill: $("role-pill"),
      modelPill: $("model-pill"),
      footerMsg: $("footer-msg"),
    },
    initialRole: session.role,
    initialModel: session.model ?? undefined,
    onRoleSwitch: async (roleId) => {
      chat.setRoleSelected(roleId);
    },
    onReconnect: () => reconnectRef(),
  });
  chat.refreshRoles(session.available_roles, session.role);

  // 3. 起 trace 面板。
  const trace = mountTrace({
    container: {
      panelEl: $("trace-panel"),
      listEl: $("trace-list"),
      sessionSelectEl: $("trace-session-select") as HTMLSelectElement,
      refreshBtn: $("trace-refresh") as HTMLButtonElement,
      toggleBtn: $("trace-toggle") as HTMLButtonElement,
      layoutEl: document.querySelector(".layout") as HTMLElement,
    },
  });
  await trace.refresh();

  // 4. 起 self-loop 面板。
  const selfLoop = mountSelfLoop({
    container: {
      panelEl: $("self-loop-panel"),
      openBtn: $("self-loop-btn") as HTMLButtonElement,
      closeBtn: $("self-loop-close") as HTMLButtonElement,
      formEl: $("self-loop-form") as HTMLFormElement,
      taskInputEl: $("self-loop-task") as HTMLInputElement,
      maxInputEl: $("self-loop-max") as HTMLInputElement,
      progressEl: $("self-loop-progress"),
      screenshotsEl: $("self-loop-screenshots"),
      layoutEl: document.querySelector(".layout") as HTMLElement,
    },
  });

  // 5. 订阅 SSE。subscribeEvents 现在管 EventSource 生命周期：
  //    错误时主动 close（停掉浏览器自带的 auto-reconnect），把
  //    状态变成 "disconnected"；用户点药丸就调 reconnect() 重建。
  const { reconnect } = subscribeEvents(
    (ev) => {
      chat.handleEvent(ev);
    },
    (status) => {
      chat.setStatus(status);
    },
  );
  reconnectRef = reconnect;

  chat.setFooter(`ready · role=${session.role} · session=${session.session_id}`);
  console.log("[ui] mounted; self-loop panel", selfLoop.isOpen() ? "open" : "closed");
}

function showFatal(msg: string): void {
  const el = document.createElement("div");
  el.style.cssText = "position:fixed;inset:0;display:flex;align-items:center;justify-content:center;background:#0e1117;color:#f85149;font-family:sans-serif;padding:2rem;text-align:center;";
  el.textContent = msg;
  document.body.appendChild(el);
}

main().catch((e) => {
  console.error("[ui] fatal", e);
  showFatal(`fatal: ${String(e)}`);
});
