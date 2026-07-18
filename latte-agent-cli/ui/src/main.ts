import type { SessionInfo } from "./api";
import {
  ensureSession, getCurrentSessionId, listSessions, createSession,
  switchSession, clearLocalSessionId, getSession, subscribeEvents, fetchSubsession,
} from "./api";
import { mountChat } from "./chat_impl";
import type { ChatController } from "./chat_impl";
import { mountTrace } from "./trace";
import { mountSelfLoop } from "./self-loop";
import { mountRoleGraph } from "./role_graph";

function $(id: string): HTMLElement {
  const el = document.getElementById(id);
  if (!el) throw new Error(`#${id} not found`);
  return el;
}

async function refreshSessionSelect(
  select: HTMLSelectElement,
  currentId: string,
): Promise<void> {
  const sessions = await listSessions();
  select.innerHTML = "";
  for (const s of sessions) {
    const opt = document.createElement("option");
    opt.value = s.session_id;
    const label =
      s.preview && s.preview !== "(new)"
        ? `${s.preview.slice(0, 40)}${s.preview.length > 40 ? "…" : ""} (${s.initial_role})`
        : `(${s.initial_role}, ${s.session_id.slice(0, 12)}…)`;
    opt.textContent = label;
    if (s.session_id === currentId) opt.selected = true;
    select.appendChild(opt);
  }
}

async function main(): Promise<void> {
  let currentId: string;
  try {
    currentId = await ensureSession();
  } catch (e) {
    showFatal(`failed to create/load session: ${String(e)}`);
    return;
  }
  let session: SessionInfo;
  try {
    session = await getSession();
  } catch (e) {
    showFatal(`failed to load session: ${String(e)}`);
    return;
  }
  console.log(`[ui] session_id=${currentId}, role=${session.role}`);

  let sseDisconnector: () => void = () => {};
  let sseConnector: () => void = () => {};

  const chat: ChatController = mountChat({
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
    onRoleSwitch: async (roleId) => { chat.setRoleSelected(roleId); },
    onReconnect: () => sseConnector(),
    onShowSubsession: async (subId, label) => {
      $("subsession-label").textContent = label;
      const body = $("subsession-body");
      body.textContent = "fetching…";
      $("subsession-panel").classList.remove("hidden");
      try {
        const events = await fetchSubsession(subId);
        body.innerHTML = "";
        if (events.length === 0) { body.textContent = "(no events captured)"; return; }
        for (const ev of events) {
          const d = document.createElement("div");
          d.className = "sub-event";
          const meta = document.createElement("div");
          meta.className = "ev-meta";
          meta.textContent = (ev as Record<string, unknown>).type as string ?? "?";
          d.appendChild(meta);
          const data = document.createElement("div");
          data.className = "ev-data";
          data.textContent = safeJSON(ev as Record<string, unknown>);
          d.appendChild(data);
          body.appendChild(d);
        }
      } catch (e) { body.textContent = `error: ${String(e)}`; }
    },
  });
  chat.refreshRoles(session.available_roles, session.role);
  $("subsession-close").addEventListener("click", () => {
    $("subsession-panel").classList.add("hidden");
  });

  const trace = mountTrace({
    container: {
      panelEl: $("trace-panel"), listEl: $("trace-list"),
      sessionSelectEl: $("trace-session-select") as HTMLSelectElement,
      refreshBtn: $("trace-refresh") as HTMLButtonElement,
      toggleBtn: $("trace-toggle") as HTMLButtonElement,
      closeBtn: $("trace-close") as HTMLButtonElement,
    },
  });
  await trace.refresh();

  const selfLoop = mountSelfLoop({
    container: {
      panelEl: $("self-loop-panel"), openBtn: $("self-loop-btn") as HTMLButtonElement,
      closeBtn: $("self-loop-close") as HTMLButtonElement,
      formEl: $("self-loop-form") as HTMLFormElement,
      taskInputEl: $("self-loop-task") as HTMLInputElement,
      maxInputEl: $("self-loop-max") as HTMLInputElement,
      progressEl: $("self-loop-progress"),
      screenshotsEl: $("self-loop-screenshots"),
    },
  });

  const roleGraph = mountRoleGraph({
    container: {
      panelEl: $("role-graph-panel"), openBtn: $("role-graph-btn") as HTMLButtonElement,
      closeBtn: $("role-graph-close") as HTMLButtonElement,
      refreshBtn: $("role-graph-refresh") as HTMLButtonElement,
      statsEl: $("role-graph-stats"), bodyEl: $("role-graph-body"),
    },
  });

  await trace.refresh();

  const sessionSelect = $("session-select") as HTMLSelectElement;
  const newSessionBtn = $("session-new-btn") as HTMLButtonElement;

  function openSse(): void {
    sseDisconnector();
    const { disconnect, reconnect } = subscribeEvents(
      (ev) => chat.handleEvent(ev),
      (status) => chat.setStatus(status),
    );
    sseDisconnector = disconnect;
    sseConnector = reconnect;
  }
  openSse();

  newSessionBtn.addEventListener("click", async () => {
    try {
      const id = await createSession();
      window.localStorage.setItem("latte-agent-ui-session-id", id);
      await refreshSessionSelect(sessionSelect, id);
      sessionSelect.value = id;
      chat.clear();
      const info = await getSession();
      chat.setRoleSelected(info.role);
      chat.refreshRoles(info.available_roles, info.role);
      openSse();
      chat.setFooter(`new session ${id.slice(0, 12)}… · ${info.role}`);
    } catch (e) { chat.setFooter(`new session failed: ${String(e)}`); }
  });

  chat.setFooter(`ready · role=${session.role} · session=${currentId}`);
  console.log(`[ui] mounted; self-loop panel`, selfLoop.isOpen() ? "open" : "closed");
}

function safeJSON(obj: Record<string, unknown>): string {
  try {
    const seen = new Set<unknown>();
    const txt = JSON.stringify(obj, (_k, v) => {
      if (typeof v === "object" && v !== null) { if (seen.has(v)) return; seen.add(v); }
      return v;
    }, 2);
    if (!txt) return String(obj);
    return txt.length > 5000 ? txt.slice(0, 5000) + "…" : txt;
  } catch { return String(obj); }
}

function showFatal(msg: string): void {
  const el = document.createElement("div");
  el.style.cssText = "position:fixed;inset:0;display:flex;align-items:center;justify-content:center;background:#eaeef3;color:#dc2626;font-family:system-ui,-apple-system,Segoe UI,Roboto,Helvetica Neue,sans-serif;padding:2rem;text-align:center;";
  el.innerHTML = `<pre style="white-space:pre-wrap;background:#ffffff;padding:2rem;border-radius:20px;box-shadow:0 12px 40px rgba(0,0,0,0.12);color:#0f172a;">${msg}</pre>`;
  document.body.appendChild(el);
}

main().catch((e) => { console.error("[fatal]", e); showFatal(String(e)); });
