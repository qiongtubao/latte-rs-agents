import type { SessionInfo } from "./api";
import {
  ensureSession, listSessions, createSession,
  switchSession, getSession, subscribeEvents, fetchSubsession,
  persistSessionId, getSessionHistory, getCurrentSessionId,
  renameSession, deleteSession,
} from "./api";
import { mountChat } from "./chat_impl";
import type { ChatController } from "./chat_impl";
import { mountTrace } from "./trace";
import { mountSelfLoop } from "./self-loop";
import { mountRoleGraph } from "./role_graph";
import { mountRoleEditor } from "./role_editor";
import { mountToolsPanel } from "./tools_panel";
import { mountModelsPanel } from "./models_panel";
import { mountTestDialog } from "./test_panel";
import { mountLogPanel } from "./log";
import type { LatteUiApi } from "./host";
import { waitForHost, setUiApi, installUiCallListener } from "./host";
import { initTransport } from "./transport";
import { extractCodeRefs, makeRefChips } from "./linkify";

function $(id: string): HTMLElement {
  const el = document.getElementById(id);
  if (!el) throw new Error(`#${id} not found`);
  return el;
}

function truncateLabel(text: string, max = 40): string {
  return text.length > max ? `${text.slice(0, max)}…` : text;
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
    const custom = s.label?.trim();
    const label = custom
      ? truncateLabel(custom)
      : s.preview && s.preview !== "(new)" && s.preview !== "(no user message yet)"
        ? `${truncateLabel(s.preview)} (${s.initial_role})`
        : `(${s.initial_role}, ${s.session_id.slice(0, 12)}…)`;
    opt.textContent = label;
    opt.title = s.session_id;
    if (s.session_id === currentId) opt.selected = true;
    select.appendChild(opt);
  }
}

async function main(): Promise<void> {
  // Host handshake (design doc §5): resolves instantly when the editor
  // already injected __LATTE_HOST__, otherwise waits ≤250ms and falls
  // back to plain browser/CLI mode. Everything host-side is
  // feature-detected, so behavior is unchanged without a host.
  const host = await waitForHost();
  // Phase 2: the editor may inject an IPC transport (contract C1) —
  // only possible via same-origin __LATTE_HOST__. Without one the
  // transport lazily defaults to HttpSseTransport (web mode 无感).
  if (host?.transport) initTransport(host.transport);
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
      abortBtn: $("chat-abort") as HTMLButtonElement,
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
    onEditRole: (roleId) => { roleEditor.open(roleId); },
    onShowSubsession: async (subId, label) => {
      $("subsession-label").textContent = label;
      const body = $("subsession-body");
      body.textContent = "fetching…";
      $("subsession-panel").classList.remove("hidden");
      try {
        const events = await fetchSubsession(subId);
        body.innerHTML = "";
        if (events.length === 0) {
          body.textContent = "(没有捕获到事件 — 该委托可能在启动专家前就失败了)";
          return;
        }
        for (const ev of events) {
          body.appendChild(renderSubsessionEvent(ev as Record<string, unknown>));
        }
      } catch (e) { body.textContent = `error: ${String(e)}`; }
    },
    onShowSessionLog: async () => {
      // 主 turn status 行右键 → 把整个 session 的 ChatEvent 历史
      // 拉回来渲染在 subsession 详情面板里。这是给「manager 开
      // 始执行 / 完成 / 失败」一类行用的：它们没有 subId，但
      // 上下文里有完整 ToolUse / RoleTurn 流水。
      $("subsession-label").textContent = "本次执行日志";
      const body = $("subsession-body");
      body.textContent = "fetching…";
      $("subsession-panel").classList.remove("hidden");
      try {
        const events = await getSessionHistory(getCurrentSessionId() || currentId);
        body.innerHTML = "";
        if (events.length === 0) {
          body.textContent = "(本次 session 还没有事件)";
          return;
        }
        for (const ev of events) {
          body.appendChild(renderSubsessionEvent(ev as Record<string, unknown>));
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

  const roleEditor = mountRoleEditor({
    container: {
      panelEl: $("role-editor-panel"),
      roleSelect: $("role-editor-role-select") as HTMLSelectElement,
      closeBtn: $("role-editor-close") as HTMLButtonElement,
      refreshBtn: $("role-editor-refresh") as HTMLButtonElement,
      formEl: $("role-editor-form") as HTMLFormElement,
      nameInput: $("role-editor-name") as HTMLInputElement,
      iconInput: $("role-editor-icon") as HTMLInputElement,
      tierSelect: $("role-editor-tier") as HTMLSelectElement,
      chainEl: $("role-editor-chain"),
      temperatureInput: $("role-editor-temperature") as HTMLInputElement,
      toolsEl: $("role-editor-tools"),
      promptInput: $("role-editor-prompt") as HTMLTextAreaElement,
      statusEl: $("role-editor-status"),
      pathsEl: $("role-editor-paths"),
    },
  });
  $("role-editor-btn").addEventListener("click", () => roleEditor.open());

  // 工具管理面板：列出所有工具 + 切换启用状态
  mountToolsPanel({
    container: {
      panelEl: $("tools-panel"),
      openBtn: $("tools-btn") as HTMLButtonElement,
      closeBtn: $("tools-close") as HTMLButtonElement,
      refreshBtn: $("tools-refresh") as HTMLButtonElement,
      bodyEl: $("tools-body"),
      statusEl: $("tools-status"),
      filterEl: $("tools-filter") as HTMLInputElement,
    },
  });
  // 模型管理面板：下拉菜单 + 表单编辑器
  const modelsItems: ModelWithSource[] = [];  // shared ref for test dialog
  mountModelsPanel({
    container: {
      panelEl: $("models-panel"),
      openBtn: $("models-btn") as HTMLButtonElement,
      closeBtn: $("models-close") as HTMLButtonElement,
      refreshBtn: $("models-refresh") as HTMLButtonElement,
      newBtn: $("models-new") as HTMLButtonElement,
      bodyEl: $("models-body"),
      statusEl: $("models-status"),
      pathsEl: $("models-paths"),
      selectEl: $("model-select") as HTMLSelectElement,
      onTestClick: (opts) => testDialog.open(opts),
    },
  });

  // 测试弹层（singleton，由 models 面板的「测试」按钮触发）。
  const testDialog = mountTestDialog({
    container: {
      overlayEl: $("test-modal"),
      closeBtn: $("test-close-btn") as HTMLButtonElement,
      modelSelect: $("test-model-select") as HTMLSelectElement,
      capabilitiesBtn: $("test-capabilities-btn") as HTMLButtonElement,
      capabilityBadgesEl: $("test-capability-badges"),
      tabBtns: document.querySelectorAll<HTMLButtonElement>(".test-tab"),
      tabPanes: document.querySelectorAll<HTMLElement>(".test-pane"),
      promptInput: $("test-prompt") as HTMLTextAreaElement,
      imageFileInput: $("test-image-input") as HTMLInputElement,
      imagePreviewEl: $("test-image-preview"),
      imageClearBtn: $("test-image-clear") as HTMLButtonElement,
      runConnectivityBtn: $("test-run-connectivity") as HTMLButtonElement,
      runAiclientBtn: $("test-run-aiclient") as HTMLButtonElement,
      runHttpBtn: $("test-run-http") as HTMLButtonElement,
      runImageBtn: $("test-run-image") as HTMLButtonElement,
      resultStatusEl: $("test-result-status"),
      resultLatencyEl: $("test-result-latency"),
      resultBodyEl: $("test-result-body"),
      resultAvailableModelsEl: $("test-result-available"),
    },
    getModels: () => modelsItems,
  });

  // 挂载日志面板：方便排查页面无响应等问题
  mountLogPanel({
    container: {
      panelEl: $("log-panel"),
      openBtn: $("log-btn") as HTMLButtonElement,
      closeBtn: $("log-close") as HTMLButtonElement,
      refreshBtn: $("log-refresh") as HTMLButtonElement,
      logListEl: $("log-file-list"),
      logContentEl: $("log-content"),
      statusEl: $("log-status"),
    },
  });

  await trace.refresh();

  const sessionSelect = $("session-select") as HTMLSelectElement;
  const newSessionBtn = $("session-new-btn") as HTMLButtonElement;
  const renameSessionBtn = $("session-rename-btn") as HTMLButtonElement;
  const delSessionBtn = $("session-del-btn") as HTMLButtonElement;

  function openSse(): void {
    sseDisconnector();
    const { disconnect, reconnect } = subscribeEvents(
      (ev) => chat.handleEvent(ev),
      (status) => chat.setStatus(status),
    );
    sseDisconnector = disconnect;
    sseConnector = reconnect;
  }
  // 首次激活 = 完整走一遍 activateSession：拉历史回放（live 与落盘
  // 恢复的 session 都覆盖）+ 订阅 SSE + 刷新会话列表。没有这一步，
  // 启动后聊天区永远空白（此前只靠切 session 才回放）。
  await activateSession(currentId);

  /** Make `id` the active session: disconnect old SSE, restore its
   * archived chat history, refresh role chrome, subscribe new SSE. */
  async function activateSession(id: string, opts?: { created?: boolean }): Promise<void> {
    sseDisconnector();
    switchSession(id);
    persistSessionId(id);
    chat.clear();
    const history = await getSessionHistory(id).catch(() => []);
    chat.replayEvents(history);
    const info = await getSession();
    chat.refreshRoles(info.available_roles, info.role);
    chat.setRoleSelected(info.role);
    openSse();
    await refreshSessionSelect(sessionSelect, id);
    if (opts?.created) {
      chat.setFooter(`new session ${id.slice(0, 12)}… · ${info.role}`);
    } else {
      const restored = history.length > 0 ? ` · restored ${history.length} events` : "";
      chat.setFooter(`switched · ${info.role} · ${id.slice(0, 12)}…${restored}`);
    }
  }

  sessionSelect.addEventListener("change", async () => {
    const id = sessionSelect.value;
    if (!id || id === getCurrentSessionId()) return;
    try {
      await activateSession(id);
    } catch (e) {
      chat.setFooter(`switch session failed: ${String(e)}`);
    }
  });

  newSessionBtn.addEventListener("click", async () => {
    try {
      const id = await createSession();
      await activateSession(id, { created: true });
    } catch (e) { chat.setFooter(`new session failed: ${String(e)}`); }
  });

  renameSessionBtn.addEventListener("click", async () => {
    const id = getCurrentSessionId();
    if (!id) return;
    const currentLabel =
      sessionSelect.selectedOptions[0]?.textContent?.trim() ?? "";
    const name = prompt("重命名 session（留空恢复默认预览）:", currentLabel);
    if (name === null) return;
    try {
      await renameSession(id, name.trim());
      await refreshSessionSelect(sessionSelect, id);
    } catch (e) { chat.setFooter(`rename failed: ${String(e)}`); }
  });

  delSessionBtn.addEventListener("click", async () => {
    const id = getCurrentSessionId();
    if (!id) return;
    const label = sessionSelect.selectedOptions[0]?.textContent?.trim() ?? id;
    if (!confirm(`删除 session「${label}」？历史将一并清除。`)) return;
    try {
      await deleteSession(id);
      // Activate whatever remains; create a fresh session if none.
      const rest = await listSessions().catch(() => []);
      const nextId = rest[0]?.session_id ?? (await createSession());
      await activateSession(nextId, { created: rest.length === 0 });
    } catch (e) { chat.setFooter(`delete failed: ${String(e)}`); }
  });

  chat.setFooter(`ready · role=${session.role} · session=${currentId}`);
  // Mount complete — expose the UI api to the host (editor) page, both
  // same-origin (`__LATTE_UI__`) and cross-origin (postMessage channel).
  const uiApi: LatteUiApi = {
    focus: () => chat.focus(),
    insertContext: (ref) => chat.insertContext(ref),
  };
  setUiApi(uiApi);
  installUiCallListener(uiApi);
  console.log(`[ui] mounted; self-loop panel`, selfLoop.isOpen() ? "open" : "closed");
}

function truncateText(s: string, max: number): string {
  return s.length <= max ? s : s.slice(0, max) + "…";
}

/** Render one TraceEvent (externally-tagged JSON: `{"Variant": {...}}`)
 * into a readable subsession-panel entry — tool calls show name/args/
 * result instead of a raw JSON dump. */
function renderSubsessionEvent(ev: Record<string, unknown>): HTMLElement {
  const d = document.createElement("div");
  const keys = Object.keys(ev);
  const variant = keys.length === 1 ? keys[0] : "?";
  const payload = (variant !== "?" ? ev[variant] : ev) as Record<string, unknown>;
  const meta = (payload?.meta ?? undefined) as Record<string, unknown> | undefined;

  d.className = `sub-event sub-${variant}`;
  const head = document.createElement("div");
  head.className = "ev-meta";
  head.textContent =
    variant +
    (meta?.role ? ` · ${meta.role}` : "") +
    (meta?.ts ? ` · ${meta.ts}` : "");
  d.appendChild(head);

  const body = document.createElement("div");
  body.className = "ev-data";
  switch (variant) {
    case "ToolExec": {
      const name = String(payload.name ?? "?");
      const argsJson = String(payload.args_json ?? "");
      const args = truncateText(argsJson, 200);
      const status = payload.status as Record<string, unknown> | undefined;
      let statusText = "";
      let resultText = "";
      if (status) {
        if ("Ok" in status) { resultText = String(status.Ok); statusText = `✅ ${truncateText(resultText, 400)}`; }
        else if ("Err" in status) { resultText = String(status.Err); statusText = `❌ ${truncateText(resultText, 400)}`; }
      }
      const latency = payload.latency_ms ? ` · ${payload.latency_ms}ms` : "";
      body.textContent = `🔧 ${name}(${args})${latency}${statusText ? "\n" + statusText : ""}`;
      // Linkify: subagent 读代码的路径渲染为可点击 chip（§5.2）。
      const chips = makeRefChips(extractCodeRefs(name, argsJson, resultText));
      if (chips) body.appendChild(chips);
      break;
    }
    case "ParseToolCalls": {
      const parsed = payload.parsed as Array<Record<string, unknown>> | undefined;
      body.textContent =
        parsed && parsed.length > 0
          ? parsed.map((c) => `→ ${c.name}(${truncateText(String(c.args), 150)})`).join("\n")
          : "(no tool calls parsed)";
      if (parsed && parsed.length > 0) {
        const refs = parsed.flatMap((c) =>
          extractCodeRefs(String(c.name ?? ""), String(c.args ?? "")));
        const chips = makeRefChips(refs);
        if (chips) body.appendChild(chips);
      }
      break;
    }
    case "PromptBuilt":
      body.textContent = `user: ${truncateText(String(payload.user_input ?? ""), 300)}`;
      break;
    case "ModelCall":
      body.textContent = `model=${payload.model_id ?? "?"} · ${payload.latency_ms ?? "?"}ms · ${payload.finish_reason ?? ""}`;
      break;
    case "ModelRawOut":
      body.textContent = truncateText(String(payload.raw_content ?? ""), 400);
      break;
    case "TurnEnd":
      body.textContent = `tokens in=${payload.total_input ?? "?"} out=${payload.total_output ?? "?"} · ${payload.elapsed_ms ?? "?"}ms`;
      break;
    case "SessionStart":
      body.textContent = `tier=${payload.tier ?? "?"}`;
      break;
    default:
      body.textContent = safeJSON(payload ?? ev);
  }
  d.appendChild(body);
  return d;
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
