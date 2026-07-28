import { ChatEvent, RoleInfo, sendMessage, sendCommand, switchRole, cancelTurn, abortSession, importTasks } from "./api";
import { extractImportableTasks } from "./workflows_panel";
import { extractCodeRefs, makeRefChips } from "./linkify";
import type { CodeRef } from "./host";
interface UIBinding {

  messagesEl: HTMLElement; formEl: HTMLFormElement; inputEl: HTMLTextAreaElement;
  sendBtn: HTMLButtonElement; clearBtn: HTMLButtonElement; quitBtn: HTMLButtonElement;
  /** Abort button — required for the runtime abort flow. */
  abortBtn: HTMLButtonElement;
  statusPill: HTMLElement; roleSelect: HTMLSelectElement; rolePill: HTMLElement;
  modelPill: HTMLElement; footerMsg: HTMLElement;
}
export interface ChatController {
  appendUser(content: string): string;
  handleEvent(e: ChatEvent): void;
  /** Replay archived events when switching back to a session.
   * Renders messages but suppresses network side effects (role
   * switching) and resets status/timers afterwards. */
  replayEvents(events: ChatEvent[]): void;
  setFooter(msg: string): void;
  setRoleSelected(roleId: string): void;
  refreshRoles(roles: RoleInfo[], selected: string): void;
  setStatus(s: "connected" | "disconnected" | "thinking" | "stalled"): void;
  clear(): void;
  /** Focus the chat input (exposed to the host as `__LATTE_UI__.focus`). */
  focus(): void;
  /** Update role → file path map for filename display in bubbles. */
  setRoleFilePaths(paths: Record<string, string>): void;
  /** Insert a code reference (and optional quote block) at the input
   * cursor — the host calls this via `__LATTE_UI__.insertContext`. */
  insertContext(ref: CodeRef & { quote?: string }): void;
}

const ROLE_ICONS: Record<string, string> = {
  manager: "👔", programmer: "💻", architect: "🏗️", reviewer: "🔍",
  reviewer_sanity: "🔍", reviewer_architecture: "📐", reviewer_security: "🔒",
  tester: "🧪", security: "🛡️", devops: "⚙️", designer: "🎨",
  tech_writer: "📝", pm: "📋",
};

function roleIcon(roleId: string): string {
  return ROLE_ICONS[roleId] ?? "🤖";
}

function fmtTime(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  const s = Math.floor(ms/1000);
  if (s < 60) return `${s}秒`;
  return `${Math.floor(s/60)}分${s%60}秒`;
}

function escapeHtml(text: string): string {
  return text
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#039;");
}

function renderContentWithCode(text: string): string {
  if (!text.includes("```")) return escapeHtml(text);
  const parts: string[] = [];
  let remaining = text;
  while (true) {
    const start = remaining.indexOf("```");
    if (start === -1) { parts.push(escapeHtml(remaining)); break; }
    parts.push(escapeHtml(remaining.slice(0, start)));
    const after = remaining.slice(start + 3);
    const end = after.indexOf("```");
    if (end === -1) { parts.push(escapeHtml(remaining)); break; }
    const langAndCode = after.slice(0, end);
    const newline = langAndCode.indexOf("\n");
    const lang = newline === -1 ? "" : langAndCode.slice(0, newline).trim();
    const code = newline === -1 ? langAndCode : langAndCode.slice(newline + 1);
    parts.push(`<pre><code${lang ? ` class="language-${lang}"` : ""}>${escapeHtml(code)}</code></pre>`);
    remaining = after.slice(end + 3);
  }
  return parts.join("");
}

export function mountChat(opts: {
  container: UIBinding; initialRole: string; initialModel?: string;
  onRoleSwitch?: (roleId: string) => Promise<void>;
  onShowSubsession?: (subId: string, label: string) => void;

  /** 状态行（status）右键「查看本次执行日志」的回调 —— 主 turn 没
   *  有 subId 时用 session 全量历史代替 subsession 详情面板。 */
  onShowSessionLog?: () => Promise<void> | void;
  onEditRole?: (roleId: string) => void;
  onReconnect?: () => void;
}): ChatController {
  const { container, initialRole, initialModel, onRoleSwitch, onShowSubsession, onShowSessionLog, onEditRole } = opts;
  let msgCounter = 0;
  let turnStartTime = 0;
  /** 当前显示的「超时询问」条；同一 role 重复触发替换之，
   *  跨 role 的并行执行按 role 维度分别保留。 */
  let timeoutPromptEl: HTMLElement | null = null;
  let timeoutPromptRole: string | null = null;

  // ── Delegate tracking (keyed by sub_id for parallel delegates) ──
  interface DelegateInfo {
    targetRole: string;
    taskText: string;
    delegateMsgId: string;
    capturedTools: { tool: string; args: string; result?: string }[];
    /** "⏳ 执行中…" badge on the DelegateStarted bubble. */
    stateEl?: HTMLElement;
  }
  const activeDelegates = new Map<string, DelegateInfo>();
  let currentDelegateSubId = ""; // most recent delegate (for non-sub_id legacy events)
  /** wf_id → pending badge element on the WorkflowStarted bubble. */
  const workflowStates = new Map<string, HTMLElement>();
  /** wf_id → 该 workflow 各 turn 的文本累积（用于完成后扫描任务 JSON）。 */
  const workflowTranscripts = new Map<string, string>();
  /** role_id → 配置文件 basename，由 main.ts 加载后注入 */
  let roleFilePaths = new Map<string, string>();

  /** Find the (most recent) pending delegate targeting `roleId` —
   *  tool events carry role_id but no sub_id, so parallel delegates
   *  are attributed by role. */
  function findDelegateSubByRole(roleId: string): string {
    let found = "";
    for (const [subId, di] of activeDelegates) {
      if (di.targetRole === roleId) found = subId;
    }
    return found;
  }

  function getFilePath(roleId: string): string | undefined {
    return roleFilePaths.get(roleId);
  }

  function clearAllDelegates(): void {
    activeDelegates.clear();
    currentDelegateSubId = "";
    workflowStates.clear();
    workflowTranscripts.clear();
  }

  let lastUserMsgId = "";
  let lastRoleStarted = "";
  let subagentTools: string[] = [];
  /** role_id → RoleStarted 时插入的 executing 状态行；用于
   *  RoleFinished / Error 把同一行切到 .done / .error，而不是再插一行。
   *  同时记录对应的 subId（manager 主 turn 时为空；delegate 跑这个
   *  role 时是 delegate 的 sub_id），让右键「查看日志」能直接命中。 */
  const executingRowByRole = new Map<string, HTMLElement>();
  const executingSubIdByRole = new Map<string, string>();

  function buildSubagentDetail(): string {
    if (subagentTools.length === 0) return "没有工具调用日志";
    return subagentTools.map((l, i) => `[${i + 1}] ${l}`).join("\n");
  }

  function nextId(): string { return `m${++msgCounter}`; }
  // ── Message store (for references & context menu) ──
  interface MsgRecord {
    id: string;
    kind: string;
    content: string;
    meta?: string;
    icon?: string;
    reference?: { refId: string; preview: string };
    subagent?: { detail: string } | null;
    timestamp: string;
    el: HTMLElement;
  }
  const messageStore: MsgRecord[] = [];

  function getMsgById(id: string): MsgRecord | undefined {
    return messageStore.find(m => m.id === id);
  }

  function fmtDisplayTime(): string {
    return new Date().toLocaleTimeString('zh-CN', { hour: '2-digit', minute: '2-digit' });
  }

  function avatarInitial(roleId: string): string {
    if (roleId === 'manager') return 'M';
    if (roleId === 'programmer') return 'P';
    if (roleId === 'architect') return 'A';
    if (roleId === 'reviewer') return 'R';
    if (roleId === 'tester') return 'T';
    if (roleId === 'security') return 'S';
    if (roleId === 'devops') return 'D';
    if (roleId === 'designer') return 'Ds';
    if (roleId === 'tech_writer') return 'W';
    if (roleId === 'pm') return 'Pm';
    return roleId.charAt(0).toUpperCase();
  }

  // ── smart scroll: only auto-scroll if user is near the bottom ──
  function isNearBottom(): boolean {
    const el = container.messagesEl;
    return el.scrollHeight - el.scrollTop - el.clientHeight < 80;
  }
  function scrollToBottom(): void {
    container.messagesEl.scrollTop = container.messagesEl.scrollHeight;
  }

  // ── addMessage: render a message into the chat ──
  function addMessage(opts2: {
    kind: "user"|"role"|"tool"|"status"|"system"|"error";
    content: string;
    meta?: string;
    subId?: string;
    icon?: string;
    reference?: { refId: string; preview: string };
    subagent?: { detail: string } | null;
    timestamp?: string;
    /** 状态行（status）专用：'executing' | 'done' | 'error'，决定
     *  CSS 颜色 + 图标。仅 status / system 行生效。 */
    state?: "executing" | "done" | "error";
    /** 角色对应的配置文件路径（仅 role / tool / error 类消息生效） */
    filePath?: string;
  }): HTMLElement {
    const kind = opts2.kind;
    const ts = opts2.timestamp || fmtDisplayTime();
    const isSelf = kind === "user";
    const withAvatar = isSelf || kind === "role" || kind === "tool" || kind === "error";
    const id = nextId();

    // Resolve avatar class & text
    let avatarCss = "";
    let avatarText = "?";
    let metaName = "";
    if (isSelf) {
      avatarCss = "self-avatar";
      avatarText = "我";
      metaName = "我";
    } else if (kind === "role" || kind === "tool" || kind === "error") {
      const roleId = opts2.meta || "";
      avatarCss = roleId;
      avatarText = avatarInitial(roleId);
      metaName = opts2.icon ? `${opts2.icon} ${roleId}` : roleId;
    }

    const row = document.createElement("div");
    row.className = `message-row ${isSelf ? "self" : kind}`;
    row.dataset.messageId = id;
    if (opts2.subId) row.dataset.subId = opts2.subId;

    if (withAvatar) {
      // ── Avatar + bubble layout ──
      const avatar = document.createElement("div");
      avatar.className = `msg-avatar${avatarCss ? " " + avatarCss : ""}`;
      avatar.textContent = avatarText;

      const bubble = document.createElement("div");
      bubble.className = "msg-bubble";

      // Meta: name + timestamp
      const meta = document.createElement("div");
      meta.className = "msg-meta";
      const nameSpan = document.createElement("span");
      nameSpan.textContent = metaName || avatarText;
      const timeSpan = document.createElement("span");
      timeSpan.className = "msg-time";
      timeSpan.textContent = ts;
      meta.appendChild(nameSpan);
      meta.appendChild(timeSpan);
      bubble.appendChild(meta);

      // Content
      const content = document.createElement("div");
      content.className = "msg-content";

      if (kind === "tool") {
        content.classList.add("collapsible", "collapsed");
        content.dataset.full = opts2.content;
        const short = opts2.content.length > 80 ? opts2.content.substring(0, 80) : opts2.content;
        content.textContent = short + (opts2.content.length > 80 ? "…" : "");
        const indicator = document.createElement("span");
        indicator.className = "collapse-indicator";
        indicator.textContent = "▶";
        content.appendChild(indicator);
        content.addEventListener("click", (e) => {
          e.stopPropagation();
          const isCollapsed = content.classList.toggle("collapsed");
          const full = content.dataset.full!;
          if (isCollapsed) {
            const s = full.length > 80 ? full.substring(0, 80) + "…" : full;
            content.textContent = s;
          } else {
            content.textContent = "▼ " + full;
          }
          const ind = document.createElement("span");
          ind.className = "collapse-indicator";
          ind.textContent = isCollapsed ? "▶" : "▼";
          content.appendChild(ind);
        });
      } else {
        content.innerHTML = renderContentWithCode(opts2.content);
      }
      bubble.appendChild(content);

      // ── Filepath row (bottom of bubble) ──
      if (opts2.filePath) {
        const fpRow = document.createElement("div");
        fpRow.className = "msg-filepath";
        fpRow.textContent = opts2.filePath;
        fpRow.title = "配置文件路径";
        bubble.appendChild(fpRow);
      }

      // ── Quote block ──
      if (opts2.reference) {
        const ref = opts2.reference;
        const refMsg = getMsgById(ref.refId);
        const previewText = ref.preview || (refMsg ? refMsg.content.substring(0, 30) : "(引用消息)");
        const quoteDiv = document.createElement("div");
        quoteDiv.className = "quote-block";
        quoteDiv.innerHTML = `
          <div class="quote-preview">
            <span class="ref-icon">↩️ 引用</span>
            <span>${previewText.length > 30 ? previewText.substring(0, 30) + "…" : previewText}</span>
          </div>
          <div class="clickable-ref" data-refid="${ref.refId}">📎 跳转到引用</div>
        `;
        quoteDiv.addEventListener("click", (e) => {
          e.stopPropagation();
          jumpToMessage(ref.refId);
        });
        bubble.appendChild(quoteDiv);
      }

      // ── Subagent badge ──
      if (opts2.subagent) {
        const badge = document.createElement("div");
        badge.className = "subagent-badge";
        badge.textContent = "🔍 subagent 过程";
        badge.addEventListener("click", (e) => {
          e.stopPropagation();
          const logEl = document.getElementById("subagentLog")!;
          logEl.textContent = (opts2.subagent?.detail) || "[subagent 日志]";
          const overlay = document.getElementById("subagentOverlay")!;
          overlay.style.display = "flex";
        });
        bubble.appendChild(badge);
      }

      row.appendChild(avatar);
      row.appendChild(bubble);
    } else {
      // ── Status / system (no avatar) ──
      // Status 行有「执行中 → 完成 / 失败」三态生命周期：RoleStarted
      // 时落库为 .executing, RoleFinished 切到 .done, Error 切到
      // .error。CSS 颜色 / 图标随 state 变。subId 提到 row 上以便
      // 右键「查看日志」能命中 subsession 面板。
      const div = document.createElement("div");
      div.className = `message ${kind}`;
      div.dataset.messageId = id;
      if (opts2.subId) {
        div.dataset.subId = opts2.subId;
        row.dataset.subId = opts2.subId;
      }
      if (opts2.meta) {
        const meta = document.createElement("span");
        meta.className = "meta";
        meta.textContent = `${opts2.icon ?? ""} ${opts2.meta}`;
        div.appendChild(meta);
      }
      const content = document.createElement("div");
      content.className = "content";
      content.textContent = opts2.content;
      div.appendChild(content);
      // 状态初始 = executing（默认由 RoleStarted 调用）。新加的
      // status / system 行不会被自动标记 —— 它们没有生命周期。
      if (opts2.state) div.classList.add(opts2.state);
      row.appendChild(div);
    }




    // ── Store record for context menu & reference lookup ──
    messageStore.push({
      id,
      kind,
      content: opts2.content,
      meta: opts2.meta,
      icon: opts2.icon,
      reference: opts2.reference,
      subagent: opts2.subagent ?? null,
      timestamp: ts,
      el: row,
    });
    container.messagesEl.appendChild(row);
    if (isNearBottom()) scrollToBottom();
    return row;
  }


  container.formEl.addEventListener("submit", async (e) => {
    e.preventDefault();
    const rawText = container.inputEl.value.trim();
    if (!rawText) return;
    container.inputEl.value = "";
    // 发送期间禁用 send 按钮防双击；abort 按钮保持可用 —— 用户
    // 可能想把刚发的卡住 turn 中途打断。
    container.sendBtn.disabled = true;

    if (rawText.startsWith("/")) {
      addMessage({ kind: "system", content: `→ ${rawText}` });
      await sendCommand(rawText);
    } else {
      // 所有用户输入都交给 manager；@manager 等价于不指定角色。
      // 用户气泡不做本地乐观渲染 —— 等后端 UserMessage 事件回显，
      // 实时与回放走同一路径，不会双份。
      const messageText = rawText.replace(/^@manager\s+/i, "").trim();
      setStatus("thinking");
      updateStatusPillLabel("正在调用 LLM…");
      container.footerMsg.textContent = "⟳ 等待 manager 响应…";
      turnStartTime = Date.now();
      await sendMessage(messageText || rawText);
      startWaitTimer();
    }
    container.sendBtn.disabled = false;
    // 注：abortBtn 不在这里重置 disable 状态 —— abort 按钮自带
    // 独立的 disabled 跟踪（click handler finally 里设回 false）。
  });

  // ── @role autocomplete ──
  const acBox = document.getElementById("role-autocomplete")!;
  const ROLE_HINTS = [
    {id:"programmer",icon:"💻",desc:"读代码、分析实现"},
    {id:"architect",icon:"🏗️",desc:"架构评估、模块分析"},
    {id:"reviewer",icon:"🔍",desc:"代码审查、质量"},
    {id:"tester",icon:"🧪",desc:"测试策略、边界"},
    {id:"security",icon:"🛡️",desc:"安全审计"},
    {id:"devops",icon:"⚙️",desc:"构建部署"},
    {id:"designer",icon:"🎨",desc:"UI/UX 设计"},
    {id:"tech_writer",icon:"📝",desc:"文档写作"},
    {id:"pm",icon:"📋",desc:"需求分析"},
  ];
  let acIdx = -1;
  container.inputEl.addEventListener("keydown", function(e) {
    if (acBox.style.display !== "none") {
      const items = acBox.querySelectorAll(".role-autocomplete-item");
      if (e.key === "ArrowDown") { e.preventDefault(); acIdx = Math.min(acIdx+1, items.length-1); items.forEach((el,i)=>el.classList.toggle("active",i===acIdx)); return; }
      if (e.key === "ArrowUp") { e.preventDefault(); acIdx = Math.max(acIdx-1, 0); items.forEach((el,i)=>el.classList.toggle("active",i===acIdx)); return; }
      if (e.key === "Enter" || e.key === "Tab") { e.preventDefault(); const el=items[acIdx] as HTMLElement | undefined; if(el&&el.dataset){const ta=container.inputEl,pos=ta.selectionStart,b=ta.value.substring(0,pos),a=ta.value.substring(pos),ai=b.lastIndexOf("@");if(ai>=0){ta.value=b.substring(0,ai)+"@"+el.dataset.role+" "+a;acBox.style.display="none";ta.focus();}} return; }
      if (e.key === "Escape") { acBox.style.display="none"; e.stopPropagation(); return; }
    }
    if (e.key === "Enter" && !e.shiftKey && acBox.style.display === "none") { e.preventDefault(); container.sendBtn.click(); }
  });
  container.inputEl.addEventListener("input", function() {
    this.style.height = "auto";
    this.style.height = Math.min(this.scrollHeight, 140) + "px";
    const p = this.selectionStart, b = this.value.substring(0, p), m = b.match(/@([a-z_]*)$/i);
    if (!m) { acBox.style.display="none"; return; }
    const f = m[1].toLowerCase();
    const entries = ROLE_HINTS.filter(r => r.id.startsWith(f));
    if (entries.length === 0) { acBox.style.display="none"; return; }
    acIdx = -1;
    acBox.innerHTML = entries.map((r,i)=>`<div class="role-autocomplete-item${i===0?' active':''}" data-role="${r.id}"><span class="ra-icon">${r.icon}</span><span class="ra-name">@${r.id}</span><span class="ra-desc">${r.desc}</span></div>`).join("");
    acBox.style.display = "block";
    acIdx = 0;
  });
  acBox.addEventListener("click", function(e) {
    const item = (e.target as HTMLElement).closest(".role-autocomplete-item") as HTMLElement | null;
    if (item && item.dataset) { const ta=container.inputEl,pos=ta.selectionStart,b=ta.value.substring(0,pos),a=ta.value.substring(pos),ai=b.lastIndexOf("@");if(ai>=0){ta.value=b.substring(0,ai)+"@"+item.dataset.role+" "+a;acBox.style.display="none";ta.focus();} }
  });
  // 点击输入框/下拉框以外区域时关闭角色选择下拉
  document.addEventListener("mousedown", (e) => {
    if (acBox.style.display === "none") return;
    const target = e.target as Node;
    if (!acBox.contains(target) && target !== container.inputEl) {
      acBox.style.display = "none";
    }
  });
  // ── Edit message modal ──
  const editOverlay = document.getElementById("editOverlay")!;
  const editTextarea = document.getElementById("editTextarea") as HTMLTextAreaElement;
  let editingRecord: MsgRecord | null = null;

  function openEditModal(record: MsgRecord): void {
    editingRecord = record;
    editTextarea.value = record.content;
    editOverlay.style.display = "flex";
    editTextarea.focus();
    editTextarea.setSelectionRange(editTextarea.value.length, editTextarea.value.length);
  }
  function closeEditModal(): void {
    editingRecord = null;
    editOverlay.style.display = "none";
  }
  function saveEditModal(): void {
    if (!editingRecord) return;
    const v = editTextarea.value.trim();
    if (v) {
      editingRecord.content = v;
      const contentEl = editingRecord.el.querySelector(".msg-content") as HTMLElement | null;
      if (contentEl) contentEl.innerHTML = renderContentWithCode(v);
    }
    closeEditModal();
  }
  document.getElementById("editSave")!.addEventListener("click", saveEditModal);
  document.getElementById("editCancel")!.addEventListener("click", closeEditModal);
  document.getElementById("editCancelTop")!.addEventListener("click", closeEditModal);
  editOverlay.addEventListener("click", (e) => { if (e.target === editOverlay) closeEditModal(); });
  editTextarea.addEventListener("keydown", (e) => {
    if (e.key === "Escape") { e.stopPropagation(); closeEditModal(); }
    if ((e.ctrlKey || e.metaKey) && e.key === "Enter") { e.preventDefault(); saveEditModal(); }
  });

  container.clearBtn.addEventListener("click", async () => { addMessage({ kind: "system", content: "→ /clear" }); await sendCommand("/clear"); });
  container.quitBtn.addEventListener("click", async () => { addMessage({ kind: "system", content: "→ /quit" }); await sendCommand("/quit"); });
  // Abort button: kills all in-flight turns + subagents + workflows.
  // 默认常驻可见。点击禁用自己 + 改 label，直到 server 响应回来。
  // 防御策略：
  //  - 双击防 race 用 disabled + early-return guard
  //  - 网络 / RPC 错误：try/catch 兜底，finally 一定能恢复 UI
  //  - DOM 操作不抛：button 是静态 HTML，不会 null；textContent 不会抛
  //  - abortSession 内部其他错误（404 等）被 fetch 包装成 Promise reject，
  //    也走 catch 分支
  container.abortBtn.addEventListener("click", async () => {
    if (container.abortBtn.disabled) return;
    container.abortBtn.disabled = true;
    container.abortBtn.textContent = "⏹ 终止中…";
    try {
      await abortSession();
    } catch (e) {
      // 任何错误（网络、404、5xx、JS 异常）都不让按钮卡死。
      console.error("[chat] abortSession failed:", e);
    } finally {
      // 无论 try/catch 走哪条都要恢复 UI，否则按钮会一直 disabled。
      container.abortBtn.disabled = false;
      container.abortBtn.textContent = "⏹ 终止";
    }
  });
  container.roleSelect.addEventListener("change", async () => {
    const roleId = container.roleSelect.value;
    if (onRoleSwitch) { await onRoleSwitch(roleId); } else { await switchRole(roleId); }
    addMessage({ kind: "system", content: `→ switched to /role ${roleId}` });
  });

  const statusLabel = container.statusPill.querySelector(".status-label") as HTMLElement;
  const statusTime = container.statusPill.querySelector(".status-time") as HTMLElement;
  let currentStatus: "connected"|"disconnected"|"thinking"|"stalled" = "disconnected";

  function setStatus(s: "connected"|"disconnected"|"thinking"|"stalled"): void {
    currentStatus = s;
    container.statusPill.className = `status-pill ${s}`;
    switch (s) {
      case "connected": statusLabel.textContent = "已连接"; statusTime.textContent = ""; container.statusPill.style.removeProperty("--wait-progress"); break;
      case "disconnected": statusLabel.textContent = "已断开"; statusTime.textContent = ""; container.statusPill.style.removeProperty("--wait-progress"); break;
      case "thinking": statusLabel.textContent = "思考中"; break;
      case "stalled": statusLabel.textContent = "已卡住"; break;
    }
  }
  container.statusPill.addEventListener("click", () => {});
  container.statusPill.title = "点击重连";

  let currentActivity = "";
  let currentDelegate = "";
  let currentToolCall = "";
  let delegateRunning = false;
  let currentRoleIcon = "";
  // When true, handleEvent is replaying archived history for a
  // session switch — suppress network side effects (switchRole).
  let replaying = false;

  function resolveIcon(roleId: string): string {
    return currentRoleIcon || roleIcon(roleId);
  }

  function updateFooter(): void {
    const icon = currentRoleIcon || "🧠";
    if (currentToolCall) { container.footerMsg.textContent = `⚡ ${currentToolCall}`; }
    else if (delegateRunning && currentDelegate) { container.footerMsg.textContent = `🔄 ${currentDelegate}`; }
    else if (currentActivity) { container.footerMsg.textContent = `⟳ ${icon} ${currentActivity}`; }
  }

  function updateStatusPillLabel(customLabel: string): void {
    if (currentStatus === "thinking") statusLabel.textContent = customLabel;
  }

  function updateRoleDisplay(roleId: string, icon: string): void {
    currentRoleIcon = icon;
    container.rolePill.textContent = `${icon} ${roleId}`;
  }

  const WAIT_TIMEOUT_MS = 300_000;
  let waitTimer: number|null = null;
  let waitTickTimer: number|null = null;
  let waitStart = 0;

  function formatWaitTime(secs: number): string {
    if (secs < 60) return `${secs}秒`;
    return `${Math.floor(secs/60)}分${secs%60}秒`;
  }

  function tickWaitTimer(): void {
    const elapsedMs = Date.now()-waitStart;
    const elapsedSecs = Math.floor(elapsedMs/1000);
    container.statusPill.style.setProperty("--wait-progress", `${Math.min(100,(elapsedMs/WAIT_TIMEOUT_MS)*100)}%`);
    const icon = currentRoleIcon||"🧠";
    if (currentStatus==="stalled") { statusTime.textContent = `已 ${formatWaitTime(elapsedSecs)}`; }
    else if (delegateRunning) { statusTime.textContent = `${icon} 委托中 ${formatWaitTime(elapsedSecs)}`; }
    else if (currentToolCall) { statusTime.textContent = `🔧 ${formatWaitTime(elapsedSecs)}`; }
    else { statusTime.textContent = `${icon} ${formatWaitTime(Math.max(0,Math.ceil((WAIT_TIMEOUT_MS-elapsedMs)/1000)))}`; }
  }

  function startWaitTimer(): void {
    clearWaitTimer(); waitStart = Date.now(); setStatus("thinking"); tickWaitTimer();
    waitTickTimer = window.setInterval(tickWaitTimer, 1000);
    waitTimer = window.setTimeout(() => {
      setStatus("stalled"); tickWaitTimer();
      addMessage({ kind:"status", content:`[卡住] 模型 ${Math.floor((Date.now()-waitStart)/1000)}秒 内无响应。可以继续输入，agent 可能稍后恢复；也可以 /clear 重试。` });
    }, WAIT_TIMEOUT_MS);
  }

  function resetWaitTimer(): void {
    if (waitTimer===null||currentStatus==="stalled") return;
    window.clearTimeout(waitTimer); waitStart = Date.now(); tickWaitTimer();
    waitTimer = window.setTimeout(() => {
      setStatus("stalled"); tickWaitTimer();
      addMessage({ kind:"status", content:`[卡住] 模型 ${Math.floor((Date.now()-waitStart)/1000)}秒 内无响应。可以继续输入，agent 可能稍后恢复；也可以 /clear 重试。` });
    }, WAIT_TIMEOUT_MS);
  }

  function clearWaitTimer(): void {
    if (waitTimer!==null) { window.clearTimeout(waitTimer); waitTimer=null; }
    if (waitTickTimer!==null) { window.clearInterval(waitTickTimer); waitTickTimer=null; }
    currentToolCall=""; currentActivity=""; currentDelegate=""; delegateRunning=false;
    updateFooter(); statusTime.textContent="";
  }

  currentRoleIcon = roleIcon(initialRole);
  container.rolePill.textContent = `${currentRoleIcon} ${initialRole}`;
  function makeSubsessionBtn(subId: string, label: string): HTMLElement {
    const lnk = document.createElement("span");
    lnk.className = "subsession-link";
    lnk.textContent = "📋 详情";
    lnk.addEventListener("click", (e) => { e.stopPropagation(); if (onShowSubsession) onShowSubsession(subId, label); });
    return lnk;
  }

  // ── Jump to referenced message ──
  function jumpToMessage(refId: string): void {
    const row = container.messagesEl.querySelector(`.message-row[data-message-id="${refId}"], .message[data-message-id="${refId}"]`) as HTMLElement | null;
    if (!row) return;
    row.scrollIntoView({ behavior: "smooth", block: "center" });
    row.classList.add("highlight-flash");
    setTimeout(() => row.classList.remove("highlight-flash"), 1200);
  }

  // ── Context menu (right-click) ──
  let selectedMsgId: string | null = null;

  // 右键角色头像 → 打开角色编辑器。头像 class 形如
  // `msg-avatar <roleId>`（用户自己的是 `msg-avatar self-avatar`，忽略）。
  container.messagesEl.addEventListener("contextmenu", (e) => {
    if (!onEditRole) return;
    const avatar = (e.target as HTMLElement).closest(".msg-avatar") as HTMLElement | null;
    if (!avatar) return;
    const roleId = avatar.classList[1];
    if (!roleId || roleId === "self-avatar") return;
    e.preventDefault();
    onEditRole(roleId);
  });

  container.messagesEl.addEventListener("contextmenu", (e) => {
    // 头像右键走上面的角色编辑器分支，不弹消息菜单。
    if ((e.target as HTMLElement).closest(".msg-avatar")) return;
    const row = (e.target as HTMLElement).closest(".message-row") as HTMLElement | null;
    if (!row || !row.dataset.messageId) return;
    e.preventDefault();
    selectedMsgId = row.dataset.messageId;
    const menu = document.getElementById("contextMenu")!;
    // 按 row 类型隐藏不适用的菜单项 —— status / system 行没有可
    // 编辑内容（执行中/完成/失败三态，编辑了也没意义），所以
    // edit / delete 隐藏；view-subagent 仅在 row 关联了 subId 时
    // 出现（delegate 的角色行才有 subId，主 turn 没有）。
    const record = getMsgById(row.dataset.messageId);
    const isStatus = !!row.querySelector(".message.status, .message.system");
    const hasSubId = !!row.dataset.subId;
    menu.querySelectorAll<HTMLElement>(".menu-item").forEach((el) => {
      const act = el.getAttribute("data-action");
      if (isStatus && (act === "edit" || act === "delete")) {
        el.style.display = "none";
      } else if (act === "view-subagent" && !hasSubId) {
        el.style.display = "none";
      } else if (act === "view-execution-log" && (isStatus === false || hasSubId)) {
        // 仅「主 turn 的 status 行」显示本次执行日志入口（无 subId
        // 但又是执行类行）；其他行隐藏。
        el.style.display = "none";
      } else {
        el.style.display = "";
      }
    });
    menu.style.display = "block";
    menu.style.left = Math.min(e.clientX, window.innerWidth - 200) + "px";
    menu.style.top = Math.min(e.clientY, window.innerHeight - 180) + "px";
    if (record === undefined) {
      // 抑制 TS6133 unused
    }
  });

  document.addEventListener("click", (e) => {
    const menu = document.getElementById("contextMenu")!;
    if (!menu.contains(e.target as Node)) menu.style.display = "none";
  });

  document.getElementById("contextMenu")!.addEventListener("click", (e) => {
    const action = (e.target as HTMLElement).closest(".menu-item")?.getAttribute("data-action");
    const menu = document.getElementById("contextMenu")!;
    if (!action || !selectedMsgId) { menu.style.display = "none"; return; }
    const record = getMsgById(selectedMsgId);
    if (!record) { menu.style.display = "none"; return; }
    switch (action) {
      case "edit": {
        openEditModal(record);
        break;
      }
      case "delete": {
        if (confirm("确定删除这条消息吗？")) {
          const idx = messageStore.indexOf(record);
          if (idx >= 0) messageStore.splice(idx, 1);
          record.el.remove();
        }
        break;
      }
      case "view-subagent": {
        const record = getMsgById(selectedMsgId!);
        const subId = record?.el.dataset.subId;
        if (subId && onShowSubsession) {
          // 有子会话：打开完整过程（含工具调用/bash 日志）
          const label = record.meta ? `${roleIcon(record.meta)} ${record.meta}` : "subagent";
          onShowSubsession(subId, label);
        } else if (record && record.subagent) {
          const logEl = document.getElementById("subagentLog")!;
          logEl.textContent = record.subagent.detail || "无详细信息";
          document.getElementById("subagentOverlay")!.style.display = "flex";
        } else {
          alert("该消息没有关联 subagent 过程");
        }
        break;
      }
      case "view-execution-log": {
        // 主 turn 的 status 行（无 subId）：跳到本次 session 全量
        // 历史 —— 上面有 ToolUse / RoleTurn / Status 等完整流水。
        if (onShowSessionLog) {
          void onShowSessionLog();
        } else {
          alert("查看本次执行日志回调未注册");
        }
        break;
      }
    }
    menu.style.display = "none";
  });
  document.getElementById("subagentOverlay")!.addEventListener("click", (e) => {
    if (e.target === document.getElementById("subagentOverlay")!) {
      document.getElementById("subagentOverlay")!.style.display = "none";
    }
  });
  // ── Timeout prompt ──────────────────────────────────────────────
  //
  // 后端在 turn 跑过 soft_timeout_secs 时推 `TimeoutWarning` 上来，
  // 我们渲染一条「继续等待 / 终止当前任务」询问条挂在消息流最
  // 顶端。同 role 再次触发替换；turn 自然结束（RoleTurn / RoleFinished
  // / Error 之一到达）时移除。硬超时兜底由后端 hard_timeout_secs
  // 触发，到时 Error 事件把执行状态行切到 .error。
  function hideTimeoutPrompt(roleId: string): void {
    if (timeoutPromptRole === roleId && timeoutPromptEl) {
      timeoutPromptEl.remove();
      timeoutPromptEl = null;
      timeoutPromptRole = null;
    }
  }
  function showTimeoutPrompt(ev: Extract<ChatEvent, { type: "TimeoutWarning" }>): void {
    // 同一 role 已经在显示 prompt → 替换；不同 role 串行覆盖（不
    // 维护 per-role map —— 现阶段单 turn 一次只跑一个 role，
    // 真正的并行来自 delegate（不带 TimeoutWarning，交给 delegate
    // 的 cancel_flag 路径），所以覆盖式就够）。
    if (timeoutPromptEl && timeoutPromptRole !== ev.role_id) {
      // 另一条 prompt 还在：旧的关掉，新的替上来。
      timeoutPromptEl.remove();
      timeoutPromptEl = null;
    }
    if (timeoutPromptEl) timeoutPromptEl.remove();

    const node = document.createElement("div");
    node.className = "timeout-prompt";
    node.dataset.roleId = ev.role_id;
    node.innerHTML = `
      <div class="timeout-prompt__head">
        <span class="timeout-prompt__icon">⏳</span>
        <strong>${roleIcon(ev.role_id)} ${ev.role_id} 已运行 ${ev.elapsed_secs}s</strong>
      </div>
      <div class="timeout-prompt__body">
        超过设定的 ${ev.soft_timeout_secs}s 软超时（硬超时将在 ${ev.hard_timeout_secs}s 强制终止）。
        是否继续等待？
      </div>
      <div class="timeout-prompt__actions">
        <button class="timeout-prompt__btn timeout-prompt__btn--continue" data-act="continue">继续等待</button>
        <button class="timeout-prompt__btn timeout-prompt__btn--cancel" data-act="cancel">终止当前任务</button>
      </div>
    `;
    // 「继续等待」只关掉 banner，不动后端（turn 继续跑）。
    node.querySelector('[data-act="continue"]')!.addEventListener("click", () => {
      hideTimeoutPrompt(ev.role_id);
    });
    // 「终止当前任务」发 cancel-turn + 关 banner。后端会立刻丢掉
    // run_turn future，session 继续接收新输入。
    node.querySelector('[data-act="cancel"]')!.addEventListener("click", () => {
      hideTimeoutPrompt(ev.role_id);
      void cancelTurn().catch((e) => {
        console.error("[chat] cancelTurn failed", e);
      });
    });
    // 插到 messages 流的最顶端（在 user / role / tool 消息之前），
    // 让用户一眼就能看到。
    container.messagesEl.insertBefore(node, container.messagesEl.firstChild);
    timeoutPromptEl = node;
    timeoutPromptRole = ev.role_id;
  }
  function handleEvent(e: ChatEvent): void {
    switch (e.type) {
      case "RoleStarted": {
        subagentTools = [];
        // 该 role 可能在某个 active delegate subsession 里跑
        // (manager @programmer ...)，找一下 subId 让右键能跳日志。
        const startedSubId = findDelegateSubByRole(e.role_id) || currentDelegateSubId || undefined;
        const node = addMessage({
          kind: "status",
          content: `🧠 ${e.role_id} 开始执行…`,
          meta: e.role_id,
          subId: startedSubId,
          state: "executing",
        });
        executingRowByRole.set(e.role_id, node);
        executingSubIdByRole.set(e.role_id, startedSubId ?? "");
        lastRoleStarted = e.role_id;
        break;
      }
      case "RoleFinished": {
        // 找到 RoleStarted 时插入的 executing 行，原地切到 .done，
        // 不再插一行。失败也走这条 — "error" 状态单独在 Error event 里设。
        const row = executingRowByRole.get(e.role_id);
        if (row) {
          const inner = row.querySelector(".message.status") as HTMLElement | null;
          if (inner) {
            inner.classList.remove("executing");
            inner.classList.add("done");
            const content = inner.querySelector(".content");
            if (content) content.textContent = `✅ ${e.role_id} 完成`;
          }
          executingRowByRole.delete(e.role_id);
          executingSubIdByRole.delete(e.role_id);
        } else {
          // 没有匹配的 executing 行（边角事件）—— 退回老行为
          addMessage({ kind: "status", content: `✅ ${e.role_id} 完成`, meta: e.role_id, state: "done" });
        }
        // turn 结束（不论 ok / failed）就把这条 role 的 timeout
        // prompt 收起来 —— 后端不会再推 TimeoutWarning，下一次
        // Warning 出现时再重新展示。
        hideTimeoutPrompt(e.role_id);
        break;
      }
      case "UserMessage": {
        // 后端回显的用户消息（发送、回放同一路径，UI 不做本地乐观渲染）。
        const node = addMessage({ kind: "user", content: e.text });
        lastUserMsgId = node.dataset.messageId || "";
        break;
      }
      case "ToolUse": {
        const t = truncate(e.args, 100);
        subagentTools.push(`🔧 ${e.tool_name}(${t})`);
        const toolRow = addMessage({ kind: "tool", content: `${e.tool_name} ${t}`, meta: e.role_id, icon: resolveIcon(e.role_id), filePath: getFilePath(e.role_id) });
        const useChips = makeRefChips(extractCodeRefs(e.tool_name, e.args));
        if (useChips) toolRow.querySelector(".msg-bubble")?.appendChild(useChips);
        currentToolCall = `🔧 ${e.tool_name}`; currentActivity = `正在调用 ${e.tool_name}…`;
        updateFooter(); updateStatusPillLabel(`🔧 ${resolveIcon(e.role_id)} ${e.tool_name}`); resetWaitTimer();
        // Attribute by role so parallel delegates each collect their own tools.
        const toolSubId = findDelegateSubByRole(e.role_id) || currentDelegateSubId;
        const di = activeDelegates.get(toolSubId);
        if (di) di.capturedTools.push({ tool: e.tool_name, args: e.args });
        break;
      }
      case "ToolResult": {
        const short = truncate(e.result, 80);
        subagentTools.push(`✅ ${e.tool_name} → ${short}`);
        const resultRow = addMessage({ kind: "tool", content: `${e.tool_name} → ${truncate(e.result, 200)}`, meta: e.role_id, icon: resolveIcon(e.role_id), filePath: getFilePath(e.role_id) });
        const resultChips = makeRefChips(extractCodeRefs(e.tool_name, "", e.result));
        if (resultChips) resultRow.querySelector(".msg-bubble")?.appendChild(resultChips);
        updateFooter(); updateStatusPillLabel(`${currentRoleIcon || resolveIcon(e.role_id)} 处理中…`); resetWaitTimer();
        const resultSubId = findDelegateSubByRole(e.role_id) || currentDelegateSubId;
        const di2 = activeDelegates.get(resultSubId);
        if (di2 && di2.capturedTools.length > 0) {
          const last = di2.capturedTools[di2.capturedTools.length - 1];
          if (last.tool === e.tool_name) last.result = e.result;
        }
        break;
      }
      case "ToolError": {
        subagentTools.push(`❌ ${e.tool_name}: ${truncate(e.error, 100)}`);
        updateFooter(); resetWaitTimer(); break;
      }
      case "ImageGenerated": {
        // generate_image 工具产出：角色气泡 + 图片 + 截断的 prompt 说明。
        // prompt 走 addMessage 的 escapeHtml 路径（纯文本），<img> 用
        // createElement 构建 —— 绝不 innerHTML 拼接用户/模型内容。
        const icon = resolveIcon(e.role_id);
        const imgMsg = addMessage({
          kind: "role",
          content: `🖼 ${truncate(e.prompt, 120)}`,
          meta: e.role_id,
          icon,
          filePath: getFilePath(e.role_id),
        });
        const link = document.createElement("a");
        link.href = e.path;
        link.target = "_blank";
        link.rel = "noopener";
        const img = document.createElement("img");
        img.className = "msg-img";
        img.src = e.path;
        img.alt = truncate(e.prompt, 120);
        link.appendChild(img);
        imgMsg.querySelector(".msg-bubble")?.appendChild(link);
        updateFooter(); resetWaitTimer();
        break;
      }
      case "RoleTurn": {
        const icon = resolveIcon(e.role_id);
        const subagent = subagentTools.length > 0 ? { detail: buildSubagentDetail() } : undefined;

        // Look up delegate info by sub_id (or fall back to currentDelegateSubId)
        const subId = e.sub_id || currentDelegateSubId;
        const di = activeDelegates.get(subId);
        const roleSubId = di ? subId : undefined;

        // Build reference: subagent reply → delegation task; manager summary → user request.
        // 不再要求 activeDelegates 为空 —— 如果有委托一直没完成（卡住/超时），
        // manager 的总结仍然应该引用最开始的用户任务。
        const ref = di
          ? { refId: di.delegateMsgId, preview: `@${di.targetRole}: ${di.taskText.slice(0, 30)}` }
          : (lastUserMsgId && e.role_id === "manager" && e.is_complete
            ? (() => {
                const userMsg = getMsgById(lastUserMsgId);
                return { refId: lastUserMsgId, preview: (userMsg?.content || "用户消息").slice(0, 30) };
              })()
            : undefined);

        addMessage({ kind: "role", content: e.content, meta: e.role_id, icon, subagent, subId: roleSubId, reference: ref, filePath: getFilePath(e.role_id) });
        currentToolCall = ""; currentActivity = ""; updateFooter();
        if (e.is_complete) {
          setStatus("connected"); clearWaitTimer();
          if (di) {
            activeDelegates.delete(subId);
            if (activeDelegates.size === 0) {
              if (!replaying) switchRole("manager").catch(() => {});
              container.rolePill.textContent = `${roleIcon("manager")} manager`;
            }
          }
        } else {
          updateStatusPillLabel(`${icon} 模型输入中…`);
          resetWaitTimer();
        }
        break;
      }
      case "Status": {
        let msg = e.message;
        if (msg.includes('tokens:') && turnStartTime > 0) {
          msg = msg + ` · ⏱ ${fmtTime(Date.now() - turnStartTime)}`;
          turnStartTime = 0;
        }
        addMessage({ kind: "status", content: msg });
        if (!currentToolCall && !delegateRunning) container.footerMsg.textContent = `⟳ ${currentRoleIcon || "🧠"} ${e.message}`;
        resetWaitTimer();
        break;
      }
      case "Prompt":
        updateRoleDisplay(e.role_id, e.icon);
        container.modelPill.textContent = e.model_id;
        break;
      case "SessionInfo":
        // Server greets each new SSE subscription with the current
        // session snapshot; roles/model are already loaded via
        // getSession, so nothing to render here.
        break;
      case "Paused": addMessage({ kind: "status", content: `[paused] ${e.reason}` }); break;
      case "Resumed": addMessage({ kind: "status", content: "[resumed]" }); break;
      case "RoundStarted":
        addMessage({ kind: "system", content: `[回合 ${e.round} 开始]` }); setFooter(`回合 ${e.round} 开始`);
        resetWaitTimer(); currentToolCall = ""; currentDelegate = ""; updateFooter(); break;
      case "RoundEnded": addMessage({ kind: "system", content: `[回合 ${e.round} 结束]` }); resetWaitTimer(); break;
      case "DelegateStarted": {
        const taskText = e.task.trim() || "(empty)";
        const delegateMsg = addMessage({
          kind: "role",
          content: `@${e.to_role} ${taskText}`,
          meta: e.from_role || "manager",
          icon: roleIcon(e.from_role || "manager"),
          subId: e.sub_id,
          filePath: getFilePath(e.to_role),
        });
        const msgId = delegateMsg.dataset.messageId || "";
        // Pending-state badge on the delegate bubble — flipped to
        // ✅/❌ by DelegateFinished, or to a timeout error by the watchdog.
        const stateEl = document.createElement("span");
        stateEl.className = "delegate-state pending";
        stateEl.textContent = "⏳ 执行中…";
        delegateMsg.querySelector(".msg-bubble")?.appendChild(stateEl);
        activeDelegates.set(e.sub_id, {
          targetRole: e.to_role,
          taskText,
          delegateMsgId: msgId,
          capturedTools: [],
          stateEl,
        });
        currentDelegateSubId = e.sub_id;
        subagentTools = [];

        delegateRunning = true; currentDelegate = `${e.from_role} → ${e.to_role}`;
        currentToolCall = ""; currentActivity = `等待 ${e.to_role}`;
        const icon = currentRoleIcon || "🧠"; updateFooter(); updateStatusPillLabel(`⏳ ${icon} → ${e.to_role}`);
        container.rolePill.textContent = `⏳ ${roleIcon(e.to_role)} ${e.to_role}`;
        resetWaitTimer(); break;
      }
      case "DelegateFinished": {
        delegateRunning = false;
        const fi = roleIcon(e.from_role), ti = roleIcon(e.to_role);
        const label = `${fi} ${e.from_role} → ${ti} ${e.to_role}`;
        const isFail = e.status !== "ok";
        const kind = isFail ? "error" : "system";
        const prefix = isFail ? "❌" : "✅";
        const statusText = isFail ? `失败(${e.status})` : "ok";
        // summary 是专家返回/失败原因的正文，失败时尤其要看，拼进消息里。
        const summary = e.summary?.trim() ? `\n${truncate(e.summary, 300)}` : "";
        const msg = addMessage({ kind, content: `${prefix} ${fi}${e.from_role}←${ti}${e.to_role}(${statusText})${summary}`, subId: e.sub_id });
        msg.appendChild(makeSubsessionBtn(e.sub_id, label));
        if (isFail) msg.classList.add("fail-flash");
        // Flip the pending badge on the DelegateStarted bubble.
        const finishedDi = activeDelegates.get(e.sub_id);
        if (finishedDi?.stateEl) {
          finishedDi.stateEl.textContent = isFail ? `❌ ${e.status}` : "✅ 完成";
          finishedDi.stateEl.className = `delegate-state ${isFail ? "failed" : "done"}`;
        }
        msg.addEventListener("contextmenu", (ev) => { ev.preventDefault(); if (onShowSubsession) onShowSubsession(e.sub_id, label); });
        currentDelegate = ""; currentActivity = "";
        const icon = currentRoleIcon || resolveIcon(e.from_role);
        updateFooter(); updateStatusPillLabel(`${icon} 思考中…`);
        container.rolePill.textContent = `${icon} ${e.from_role}`;
        // Clean up delegate tracking
        activeDelegates.delete(e.sub_id);
        resetWaitTimer(); break;
      }
      case "Done": setStatus("connected"); clearWaitTimer(); addMessage({ kind: "system", content: "[会话结束]" }); setFooter("会话结束"); break;
      case "WorkflowStarted": {
        const msg = addMessage({
          kind: "role",
          content: `🔀 工作流「${e.name}」启动\n${truncate(e.topic, 200)}`,
          meta: "manager",
          icon: roleIcon("manager"),
          filePath: getFilePath("manager"),
        });
        const stateEl = document.createElement("span");
        stateEl.className = "delegate-state pending";
        stateEl.textContent = "⏳ 工作流执行中…";
        msg.querySelector(".msg-bubble")?.appendChild(stateEl);
        workflowStates.set(e.wf_id, stateEl);
        setFooter(`workflow ${e.name} 运行中…`);
        resetWaitTimer();
        break;
      }
      case "WorkflowStep": {
        const desc = e.description?.trim() || e.step_id;
        addMessage({ kind: "status", content: `▶️ 步骤 ${e.index}/${e.total} · ${desc}` });
        resetWaitTimer();
        break;
      }
      case "WorkflowTurn": {
        const icon = roleIcon(e.role_id);
        addMessage({
          kind: "role",
          content: e.content,
          meta: e.role_id,
          icon,
          filePath: getFilePath(e.role_id),
        });
        workflowTranscripts.set(
          e.wf_id,
          (workflowTranscripts.get(e.wf_id) ?? "") + "\n" + e.content,
        );
        resetWaitTimer();
        break;
      }
      case "WorkflowFinished": {
        const isFail = e.status !== "ok";
        const stateEl = workflowStates.get(e.wf_id);
        if (stateEl) {
          stateEl.textContent = isFail ? `❌ ${e.status}` : "✅ 完成";
          stateEl.className = `delegate-state ${isFail ? "failed" : "done"}`;
          workflowStates.delete(e.wf_id);
        }
        const summary = e.summary?.trim() ? `\n${truncate(e.summary, 300)}` : "";
        addMessage({
          kind: isFail ? "error" : "system",
          content: `${isFail ? "❌" : "✅"} 工作流「${e.name}」${isFail ? `失败(${e.status})` : "完成"}${summary}`,
        });
        // plan 类 workflow 完成后：扫描 transcript 里的任务 JSON，
        // 给「导入任务看板」按钮（与 workflows 面板同一提取逻辑）。
        if (!isFail) {
          const transcript = workflowTranscripts.get(e.wf_id) ?? "";
          const found = extractImportableTasks(transcript);
          if (found) {
            const msg = addMessage({ kind: "system", content: `📋 检测到 ${found.length} 个可导入任务` });
            const btn = document.createElement("button");
            btn.className = "workflow-import-btn";
            btn.textContent = `📥 导入任务看板（${found.length} 个任务）`;
            btn.addEventListener("click", async () => {
              if (!confirm(`导入 ${found.length} 个任务到看板（backlog）？`)) return;
              btn.disabled = true;
              try {
                const resp = await importTasks(found);
                btn.textContent = `✅ 已创建 ${resp.created.length} 个任务，请到任务看板查看`;
              } catch (err) {
                btn.disabled = false;
                btn.textContent = `导入失败: ${(err as Error).message}`;
              }
            });
            msg.querySelector(".msg-bubble")?.appendChild(btn);
          }
        }
        workflowTranscripts.delete(e.wf_id);
        setFooter(`workflow ${e.name} ${e.status}`);
        resetWaitTimer();
        break;
      }
      case "TimeoutWarning": {
        // 后端报告：当前 turn 跑过 soft timeout 但仍在跑。后端
        // 不会自动终止 —— 弹出「继续等待 / 终止当前任务」让用户
        // 决定，硬超时（hard_timeout_secs）才会兜底强杀。
        //
        // 多次 TimeoutWarning 进来时替换之前那条（同一 role 继续
        // 跑就会有）；不同 role 的同时执行则按 role 维度跟踪。
        showTimeoutPrompt(e);
        break;
      }
      case "Error": {
        setStatus("connected");
        clearWaitTimer();
        // 优先把现有 executing 状态行切到 .error（保留上下文，能
        // 右键「查看日志」直接命中 subagent 过程）；找不到再新插
        // 一条 error 行。
        const errSubId = e.sub_id;
        const targetRole = lastRoleStarted;
        const row = targetRole ? executingRowByRole.get(targetRole) : undefined;
        if (row) {
          const inner = row.querySelector(".message.status") as HTMLElement | null;
          if (inner) {
            inner.classList.remove("executing");
            inner.classList.add("error");
            const content = inner.querySelector(".content");
            if (content) content.textContent = `❌ ${targetRole} 失败: ${e.message}`;
            // 关联到具体 subagent (delegate 失败时后端带 sub_id；
            // 主 turn 错误继承最近 RoleStarted 的 subId，可能是 undefined)
            if (errSubId) {
              inner.dataset.subId = errSubId;
              row.dataset.subId = errSubId;
            } else {
              const inherited = executingSubIdByRole.get(targetRole);
              if (inherited) {
                inner.dataset.subId = inherited;
                row.dataset.subId = inherited;
              }
            }
          }
          executingRowByRole.delete(targetRole);
          executingSubIdByRole.delete(targetRole);
         } else {
        }
        setFooter(`错误: ${truncate(e.message, 80)}`);
        // turn 失败 —— TimeoutWarning 失去意义（后端不会再有新
        // warning），把 banner 收掉。
        if (targetRole) hideTimeoutPrompt(targetRole);
        break;
      }
      default: console.warn("[chat] unknown event", e);
    }
  }

  function appendUser(content: string): string {
    const node = addMessage({ kind:"user", content });
    lastUserMsgId = node.dataset.messageId!;
    return node.dataset.messageId!;
  }
  function focus(): void { container.inputEl.focus(); }
  /** Host → UI (`__LATTE_UI__.insertContext`, design doc §5.4): insert
   * `参考 path:start-end (symbol) ` at the input cursor, with the
   * optional quote rendered as a `> ` block above it. */
  function insertContext(ref: CodeRef & { quote?: string }): void {
    const ta = container.inputEl;
    let text = "";
    if (ref.quote?.trim()) {
      text += ref.quote.trim().split("\n").map((l) => `> ${l}`).join("\n") + "\n";
    }
    let range = "";
    if (ref.startLine !== undefined) {
      range = `:${ref.startLine}`;
      if (ref.endLine !== undefined && ref.endLine !== ref.startLine) range += `-${ref.endLine}`;
    }
    text += `参考 ${ref.path}${range}${ref.symbol ? ` (${ref.symbol})` : ""} `;
    const pos = ta.selectionStart ?? ta.value.length;
    ta.value = ta.value.slice(0, pos) + text + ta.value.slice(pos);
    ta.selectionStart = ta.selectionEnd = pos + text.length;
    focus();
  }
  function setFooter(msg: string): void { container.footerMsg.textContent = msg; }
  function setRoleSelected(roleId: string): void { container.rolePill.textContent = roleId; container.roleSelect.value = roleId; }
  function refreshRoles(roles: RoleInfo[], selected: string): void {
    container.roleSelect.innerHTML = "";
    for (const r of roles) { const opt = document.createElement("option"); opt.value=r.id; opt.innerHTML=`${r.icon??""} ${r.id}`; if (r.id===selected) opt.selected=true; container.roleSelect.appendChild(opt); }
  }
  function clear(): void {
    container.messagesEl.innerHTML="";
    msgCounter=0;
    messageStore.length=0;
    clearAllDelegates();
    subagentTools.length = 0;
    lastUserMsgId = "";
    lastRoleStarted = "";
    currentToolCall="";
    currentActivity="";
    currentDelegate="";
    delegateRunning=false;
    // 清掉残留的 timeout 询问条（切 session / 强制 clear 都用）。
    if (timeoutPromptEl) {
      timeoutPromptEl.remove();
      timeoutPromptEl = null;
      timeoutPromptRole = null;
    }
    updateFooter();
  }
  function replayEvents(events: ChatEvent[]): void {
    replaying = true;
    try {
      for (const e of events) handleEvent(e);
    } finally {
      replaying = false;
      // History replay leaves no in-flight delegate/timer state.
      clearAllDelegates();
      subagentTools.length = 0;
      // Replay 完发现 banner 还在（说明后端当时正在跑 turn）→
      // 关掉 —— replay 表达的是"已经发生过的历史"，不应该有
      // 正在等的 prompt。
      if (timeoutPromptEl) {
        timeoutPromptEl.remove();
        timeoutPromptEl = null;
        timeoutPromptRole = null;
      }
      clearWaitTimer();
      setStatus("connected");
    }
  }

  function setRoleFilePaths(paths: Record<string, string>): void {
    roleFilePaths = new Map(Object.entries(paths));
  }

  return { appendUser, handleEvent, replayEvents, setFooter, setRoleSelected, refreshRoles, setStatus, clear, focus, insertContext, setRoleFilePaths };
}

function truncate(s: string, max: number): string { if (s.length<=max) return s; return s.slice(0,max)+"…"; }
