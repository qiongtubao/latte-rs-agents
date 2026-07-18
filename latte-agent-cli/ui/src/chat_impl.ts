import { ChatEvent, RoleInfo, sendMessage, sendCommand, switchRole } from "./api";

interface UIBinding {
  messagesEl: HTMLElement; formEl: HTMLFormElement; inputEl: HTMLTextAreaElement;
  sendBtn: HTMLButtonElement; clearBtn: HTMLButtonElement; quitBtn: HTMLButtonElement;
  statusPill: HTMLElement; roleSelect: HTMLSelectElement; rolePill: HTMLElement;
  modelPill: HTMLElement; footerMsg: HTMLElement;
}

export interface ChatController {
  appendUser(content: string): string;
  handleEvent(e: ChatEvent): void;
  setFooter(msg: string): void;
  setRoleSelected(roleId: string): void;
  refreshRoles(roles: RoleInfo[], selected: string): void;
  setStatus(s: "connected" | "disconnected" | "thinking" | "stalled"): void;
  clear(): void;
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
  onReconnect?: () => void;
}): ChatController {
  const { container, initialRole, initialModel, onRoleSwitch, onShowSubsession } = opts;
  let msgCounter = 0;
  let turnStartTime = 0;

  // ── Delegate tracking (keyed by sub_id for parallel delegates) ──
  interface DelegateInfo {
    targetRole: string;
    taskText: string;
    delegateMsgId: string;
    capturedTools: { tool: string; args: string; result?: string }[];
  }
  const activeDelegates = new Map<string, DelegateInfo>();
  let currentDelegateSubId = ""; // most recent delegate (for non-sub_id legacy events)

  // ── Reference tracking ──
  let lastUserMsgId = "";
  let lastRoleStarted = "";
  let subagentTools: string[] = [];

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
      const div = document.createElement("div");
      div.className = `message ${kind}`;
      div.dataset.messageId = id;
      if (opts2.subId) div.dataset.subId = opts2.subId;
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
      row.appendChild(div);
    }



    // For @role delegated messages: click shows subsession via subId
    if (opts2.subId && activeDelegates.has(opts2.subId) && kind === "role" && opts2.meta) {
      const roleName = opts2.meta;
      row.style.cursor = "pointer";
      row.title = "点击查看执行过程";
      row.addEventListener("click", (e) => {
        e.stopPropagation();
        if (opts2.subId && onShowSubsession) {
          const label = (roleIcon(roleName) || "") + " " + roleName;
          onShowSubsession(opts2.subId, label);
        }
      });
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
    container.sendBtn.disabled = true;

    if (rawText.startsWith("/")) {
      addMessage({ kind: "system", content: `→ ${rawText}` });
      await sendCommand(rawText);
    } else {
      // 所有用户输入都交给 manager；@manager 等价于不指定角色
      const messageText = rawText.replace(/^@manager\s+/i, "").trim();
      const displayText = messageText || rawText;
      const node = addMessage({ kind: "user", content: displayText });
      lastUserMsgId = node.dataset.messageId || "";
      setStatus("thinking");
      updateStatusPillLabel("正在调用 LLM…");
      container.footerMsg.textContent = "⟳ 等待 manager 响应…";
      turnStartTime = Date.now();
      await sendMessage(messageText || rawText);
      startWaitTimer();
    }
    container.sendBtn.disabled = false;
  });

  container.inputEl.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); container.sendBtn.click(); }
  });
  container.inputEl.addEventListener("input", () => {
    container.inputEl.style.height = "auto";
    container.inputEl.style.height = Math.min(container.inputEl.scrollHeight, 140) + "px";
  });

  // Escape closes any open overlay / menu.
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      document.getElementById("contextMenu")!.style.display = "none";
      document.getElementById("subagentOverlay")!.style.display = "none";
    }
  });

  // Double-click a message row to edit (same as context-menu edit).
  container.messagesEl.addEventListener("dblclick", (e) => {
    const row = (e.target as HTMLElement).closest(".message-row") as HTMLElement | null;
    if (!row || !row.dataset.messageId) return;
    const record = getMsgById(row.dataset.messageId);
    if (!record) return;
    const newContent = prompt("编辑消息内容:", record.content);
    if (newContent !== null && newContent.trim() !== "") {
      record.content = newContent.trim();
      const contentEl = record.el.querySelector(".msg-content") as HTMLElement | null;
      if (contentEl) contentEl.innerHTML = renderContentWithCode(newContent.trim());
    }
  });
  container.clearBtn.addEventListener("click", async () => { addMessage({ kind: "system", content: "→ /clear" }); await sendCommand("/clear"); });
  container.quitBtn.addEventListener("click", async () => { addMessage({ kind: "system", content: "→ /quit" }); await sendCommand("/quit"); });
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

  container.messagesEl.addEventListener("contextmenu", (e) => {
    const row = (e.target as HTMLElement).closest(".message-row") as HTMLElement | null;
    if (!row || !row.dataset.messageId) return;
    e.preventDefault();
    selectedMsgId = row.dataset.messageId;
    const menu = document.getElementById("contextMenu")!;
    menu.style.display = "block";
    menu.style.left = Math.min(e.clientX, window.innerWidth - 200) + "px";
    menu.style.top = Math.min(e.clientY, window.innerHeight - 180) + "px";
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
        const newContent = prompt("编辑消息内容:", record.content);
        if (newContent !== null && newContent.trim() !== "") {
          record.content = newContent.trim();
          const contentEl = record.el.querySelector(".msg-content") as HTMLElement | null;
          if (contentEl) contentEl.innerHTML = renderContentWithCode(newContent.trim());
        }
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
        if (record && record.subagent) {
          const logEl = document.getElementById("subagentLog")!;
          logEl.textContent = record.subagent.detail || "无详细信息";
          document.getElementById("subagentOverlay")!.style.display = "flex";
        } else {
          alert("该消息没有关联 subagent 过程");
        }
        break;
      }
    }
    menu.style.display = "none";
  });


  document.getElementById("closeSubagent")!.addEventListener("click", () => {
    document.getElementById("subagentOverlay")!.style.display = "none";
  });
  document.getElementById("subagentOverlay")!.addEventListener("click", (e) => {
    if (e.target === document.getElementById("subagentOverlay")!) {
      document.getElementById("subagentOverlay")!.style.display = "none";
    }
  });
  function handleEvent(e: ChatEvent): void {
    switch (e.type) {
      case "RoleStarted": {
        subagentTools = [];
        addMessage({ kind: "status", content: `🧠 ${e.role_id} 开始执行…` });
        lastRoleStarted = e.role_id;
        break;
      }
      case "RoleFinished": {
        addMessage({ kind: "status", content: `✅ ${e.role_id} 完成` });
        break;
      }
      case "ToolUse": {
        const t = truncate(e.args, 100);
        subagentTools.push(`🔧 ${e.tool_name}(${t})`);
        addMessage({ kind: "tool", content: `${e.tool_name} ${t}`, meta: e.role_id, icon: resolveIcon(e.role_id) });
        currentToolCall = `🔧 ${e.tool_name}`; currentActivity = `正在调用 ${e.tool_name}…`;
        updateFooter(); updateStatusPillLabel(`🔧 ${resolveIcon(e.role_id)} ${e.tool_name}`); resetWaitTimer();
        const di = activeDelegates.get(currentDelegateSubId);
        if (di) di.capturedTools.push({ tool: e.tool_name, args: e.args });
        break;
      }
      case "ToolResult": {
        const short = truncate(e.result, 80);
        subagentTools.push(`✅ ${e.tool_name} → ${short}`);
        addMessage({ kind: "tool", content: `${e.tool_name} → ${truncate(e.result, 200)}`, meta: e.role_id, icon: resolveIcon(e.role_id) });
        updateFooter(); updateStatusPillLabel(`${currentRoleIcon || resolveIcon(e.role_id)} 处理中…`); resetWaitTimer();
        const di2 = activeDelegates.get(currentDelegateSubId);
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
      case "RoleTurn": {
        const icon = resolveIcon(e.role_id);
        const subagent = subagentTools.length > 0 ? { detail: buildSubagentDetail() } : undefined;

        // Look up delegate info by sub_id (or fall back to currentDelegateSubId)
        const subId = e.sub_id || currentDelegateSubId;
        const di = activeDelegates.get(subId);
        const roleSubId = di ? subId : undefined;

        // Build reference: subagent reply → delegation task; manager summary → user request
        const ref = di
          ? { refId: di.delegateMsgId, preview: `@${di.targetRole}: ${di.taskText.slice(0, 30)}` }
          : (!activeDelegates.size && lastUserMsgId && e.role_id === "manager" && e.is_complete
            ? (() => {
                const userMsg = getMsgById(lastUserMsgId);
                return { refId: lastUserMsgId, preview: (userMsg?.content || "用户消息").slice(0, 30) };
              })()
            : undefined);

        addMessage({ kind: "role", content: e.content, meta: e.role_id, icon, subagent, subId: roleSubId, reference: ref });
        currentToolCall = ""; currentActivity = ""; updateFooter();
        if (e.is_complete) {
          setStatus("connected"); clearWaitTimer();
          if (di) {
            activeDelegates.delete(subId);
            if (activeDelegates.size === 0) {
              switchRole("manager").catch(() => {});
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
        });
        const msgId = delegateMsg.dataset.messageId || "";
        activeDelegates.set(e.sub_id, {
          targetRole: e.to_role,
          taskText,
          delegateMsgId: msgId,
          capturedTools: [],
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
        const isFail = e.status === "failed" || e.status === "timeout";
        const kind = isFail ? "error" : "system";
        const prefix = isFail ? "❌" : "✅";
        const statusText = isFail ? `失败(${e.status})` : "ok";
        const msg = addMessage({ kind, content: `${prefix} ${fi}${e.from_role}←${ti}${e.to_role}(${statusText})`, subId: e.sub_id });
        msg.appendChild(makeSubsessionBtn(e.sub_id, label));
        if (isFail) msg.classList.add("fail-flash");
        msg.style.cursor = "pointer";
        msg.addEventListener("click", () => { if (onShowSubsession) onShowSubsession(e.sub_id, label); });
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
      case "Error": setStatus("connected"); clearWaitTimer(); addMessage({ kind: "error", content: e.message, meta: "error" }); setFooter(`错误: ${truncate(e.message, 80)}`); break;
      default: console.warn("[chat] unknown event", e);
    }
  }

  function appendUser(content: string): string {
    const node = addMessage({ kind:"user", content });
    lastUserMsgId = node.dataset.messageId!;
    return node.dataset.messageId!;
  }
  function setFooter(msg: string): void { container.footerMsg.textContent = msg; }
  function setRoleSelected(roleId: string): void { container.rolePill.textContent = roleId; container.roleSelect.value = roleId; }
  function refreshRoles(roles: RoleInfo[], selected: string): void {
    container.roleSelect.innerHTML = "";
    for (const r of roles) { const opt = document.createElement("option"); opt.value=r.id; opt.innerHTML=`${r.icon??""} ${r.id}`; if (r.id===selected) opt.selected=true; container.roleSelect.appendChild(opt); }
  }
  function clear(): void { container.messagesEl.innerHTML=""; msgCounter=0; messageStore.length=0; currentToolCall=""; currentActivity=""; currentDelegate=""; delegateRunning=false; updateFooter(); }
  return { appendUser, handleEvent, setFooter, setRoleSelected, refreshRoles, setStatus, clear };
}

function truncate(s: string, max: number): string { if (s.length<=max) return s; return s.slice(0,max)+"…"; }
