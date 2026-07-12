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

export function mountChat(opts: {
  container: UIBinding; initialRole: string; initialModel?: string;
  onRoleSwitch?: (roleId: string) => Promise<void>;
  onShowSubsession?: (subId: string, label: string) => void;
  onReconnect?: () => void;
}): ChatController {
  const { container, initialRole, initialModel, onRoleSwitch, onShowSubsession } = opts;
  let msgCounter = 0;
  let turnStartTime = 0;
  let delegationTargetRole = "";
  // Capture tool events for @role delegation subsession
  let capturedTools: { tool: string; args: string; result?: string }[] = [];

  function nextId(): string { return `m${++msgCounter}`; }

  function addMessage(opts2: { kind: "user"|"role"|"tool"|"status"|"system"|"error"; content: string; meta?: string; subId?: string; icon?: string; }): HTMLElement {
    const kind = opts2.kind;
    const withAvatar = kind === "role" || kind === "tool" || kind === "error";
    if (withAvatar && opts2.icon) {
      const row = document.createElement("div");
      row.className = `message-row ${kind}`;
      row.dataset.messageId = nextId();
      if (opts2.subId) row.dataset.subId = opts2.subId;
      const avatar = document.createElement("div");
      avatar.className = "msg-avatar";
      avatar.textContent = opts2.icon;
      const bubble = document.createElement("div");
      bubble.className = "msg-bubble";
      if (opts2.meta) {
        const meta = document.createElement("div");
        meta.className = "msg-meta";
        meta.textContent = opts2.meta;
        bubble.appendChild(meta);
      }
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
        content.textContent = opts2.content;
      }
      bubble.appendChild(content);
      row.appendChild(avatar);
      row.appendChild(bubble);
      // For @role replies: right-click to show subsession tool calls
      if (delegationTargetRole && kind === "role" && opts2.meta) {
        attachToolPopup(row, opts2.meta);
      }
      container.messagesEl.appendChild(row);
      return row;
    }
    const div = document.createElement("div");
    div.className = `message ${kind}`;
    div.dataset.messageId = nextId();
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
    container.messagesEl.appendChild(div);
    return div;
  }

  function attachToolPopup(el: HTMLElement, label: string): void {
    el.style.cursor = "pointer";
    el.title = "右键查看执行过程";
    el.addEventListener("contextmenu", (ev) => {
      ev.preventDefault();
      showToolPopup(label);
    });
    el.addEventListener("click", (ev) => {
      if (ev.button === 0) showToolPopup(label);
    });
  }

  function showToolPopup(label: string): void {
    // Remove existing popup
    document.querySelectorAll(".subsession-overlay").forEach(e => e.remove());

    const overlay = document.createElement("div");
    overlay.className = "subsession-overlay";

    const popup = document.createElement("div");
    popup.className = "subsession-popup";

    const header = document.createElement("div");
    header.className = "subsession-popup-header";
    header.innerHTML = `<span>🔍 ${label} 执行过程</span>`;

    const close = document.createElement("button");
    close.textContent = "×";
    close.className = "subsession-popup-close";
    close.addEventListener("click", () => overlay.remove());
    header.appendChild(close);
    popup.appendChild(header);

    const body = document.createElement("div");
    body.className = "subsession-popup-body";

    if (capturedTools.length === 0) {
      body.innerHTML = '<div class="subsession-empty">(无工具调用)</div>';
    } else {
      for (const ct of capturedTools) {
        const item = document.createElement("div");
        item.className = "subsession-item";
        const argsBrief = ct.args.length > 80 ? ct.args.substring(0, 80) + "…" : ct.args;
        item.innerHTML = `<div class="subsession-tool">🔧 ${ct.tool} ${argsBrief}</div>`;
        if (ct.result) {
          const resBrief = ct.result.length > 200 ? ct.result.substring(0, 200) + "…" : ct.result;
          item.innerHTML += `<div class="subsession-result">${resBrief}</div>`;
        }
        body.appendChild(item);
      }
    }

    popup.appendChild(body);
    overlay.appendChild(popup);
    document.body.appendChild(overlay);
  }

  container.formEl.addEventListener("submit", async (e) => {
    e.preventDefault();
    const rawText = container.inputEl.value.trim();
    if (!rawText) return;
    container.inputEl.value = "";
    container.sendBtn.disabled = true;

    const atMatch = rawText.match(/^@(\w+)\s+(.*)/s);
    if (atMatch) {
      const targetRole = atMatch[1];
      const msgText = atMatch[2].trim() || "(empty)";
      addMessage({ kind: "system", content: `🤝 @manager → @${targetRole}: ${msgText}` });
      setStatus("thinking");
      updateStatusPillLabel(`→ @${targetRole}`);
      container.footerMsg.textContent = `⏳ 派发给 @${targetRole}: ${msgText.substring(0, 60)}…`;
      turnStartTime = Date.now();
      delegationTargetRole = targetRole;
      capturedTools = []; // reset tool capture
      try {
        await switchRole(targetRole);
        await sendMessage(msgText);
      } catch (err) {
        addMessage({ kind: "error", content: `派发给 @${targetRole} 失败: ${String(err)}` });
        delegationTargetRole = "";
      }
      startWaitTimer();
    } else if (rawText.startsWith("/")) {
      addMessage({ kind: "system", content: `→ ${rawText}` });
      await sendCommand(rawText);
    } else {
      addMessage({ kind: "user", content: rawText });
      setStatus("thinking");
      updateStatusPillLabel("正在调用 LLM…");
      container.footerMsg.textContent = "⟳ 等待模型响应…";
      turnStartTime = Date.now();
      await sendMessage(rawText);
      startWaitTimer();
    }
    container.sendBtn.disabled = false;
  });

  container.inputEl.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); container.sendBtn.click(); }
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
  let delegationTargetRole2 = "";

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

  function handleEvent(e: ChatEvent): void {
    switch (e.type) {
      case "RoleTurn": {
        const icon = resolveIcon(e.role_id);
        addMessage({ kind:"role", content:e.content, meta:e.role_id, icon });
        currentToolCall=""; currentActivity=""; updateFooter();
        if (e.is_complete) {
          setStatus("connected"); clearWaitTimer();
          if (delegationTargetRole) {
            delegationTargetRole = "";
            switchRole("manager").catch(() => {});
            container.rolePill.textContent = `${roleIcon("manager")} manager`;
          }
        }
        else { updateStatusPillLabel(`${icon} 模型输入中…`); resetWaitTimer(); }
        break;
      }
      case "Status": {
        let msg = e.message;
        if (msg.includes('tokens:') && turnStartTime > 0) {
          msg = msg + ` · ⏱ ${fmtTime(Date.now() - turnStartTime)}`;
          turnStartTime = 0;
        }
        addMessage({ kind:"status", content:msg });
        if (!currentToolCall&&!delegateRunning) container.footerMsg.textContent = `⟳ ${currentRoleIcon||"🧠"} ${e.message}`;
        resetWaitTimer();
        break;
      }
      case "Prompt":
        updateRoleDisplay(e.role_id, e.icon);
        container.modelPill.textContent = e.model_id;
        break;
      case "Paused": addMessage({ kind:"status", content:`[paused] ${e.reason}` }); break;
      case "Resumed": addMessage({ kind:"status", content:"[resumed]" }); break;
      case "RoundStarted":
        addMessage({ kind:"system", content:`[回合 ${e.round} 开始]` }); setFooter(`回合 ${e.round} 开始`);
        resetWaitTimer(); currentToolCall=""; currentDelegate=""; updateFooter(); break;
      case "RoundEnded": addMessage({ kind:"system", content:`[回合 ${e.round} 结束]` }); resetWaitTimer(); break;
      case "ToolUse": {
        const t = truncate(e.args,150);
        addMessage({ kind:"tool", content:`${e.tool_name} ${t}`, meta:e.role_id, icon:resolveIcon(e.role_id) });
        currentToolCall=`🔧 ${e.tool_name}`; currentActivity=`正在调用 ${e.tool_name}…`;
        updateFooter(); updateStatusPillLabel(`🔧 ${resolveIcon(e.role_id)} ${e.tool_name}`); resetWaitTimer();
        // Capture for @role delegation popup
        if (delegationTargetRole) capturedTools.push({ tool: e.tool_name, args: e.args });
        break;
      }
      case "ToolResult": {
        addMessage({ kind:"tool", content:`${e.tool_name} → ${truncate(e.result,200)}`, meta:e.role_id, icon:resolveIcon(e.role_id) });
        currentToolCall=""; currentActivity=`${e.tool_name} 完成`;
        updateFooter(); updateStatusPillLabel(`${currentRoleIcon||resolveIcon(e.role_id)} 处理中…`); resetWaitTimer();
        // Capture result for @role delegation popup
        if (delegationTargetRole && capturedTools.length > 0) {
          const last = capturedTools[capturedTools.length - 1];
          if (last.tool === e.tool_name) last.result = e.result;
        }
        break;
      }
      case "ToolError": {
        addMessage({ kind:"error", content:`${e.tool_name} failed: ${truncate(e.error,200)}`, meta:e.role_id, icon:resolveIcon(e.role_id) });
        currentToolCall=""; currentActivity=`${e.tool_name} 出错`;
        updateFooter(); resetWaitTimer(); break;
      }
      case "DelegateStarted": {
        const t = truncate(e.task,60);
        addMessage({ kind:"system", content:`🤝 ${e.from_role} → ${e.to_role}`, subId:e.sub_id });
        delegateRunning=true; currentDelegate=`${e.from_role} → ${e.to_role}`;
        currentToolCall=""; currentActivity=`等待 ${e.to_role}`;
        const icon=currentRoleIcon||"🧠"; updateFooter(); updateStatusPillLabel(`⏳ ${icon} → ${e.to_role}`);
        container.rolePill.textContent = `⏳ ${roleIcon(e.to_role)} ${e.to_role}`;
        resetWaitTimer(); break;
      }
      case "DelegateFinished": {
        delegateRunning=false;
        const fi=roleIcon(e.from_role), ti=roleIcon(e.to_role);
        const label=`${fi} ${e.from_role} → ${ti} ${e.to_role}`;
        const isFail = e.status === "failed" || e.status === "timeout";
        const kind = isFail ? "error" : "system";
        const prefix = isFail ? "❌" : "✅";
        const statusText = isFail ? `失败(${e.status})` : "ok";
        const msg = addMessage({ kind, content:`${prefix} ${fi}${e.from_role}←${ti}${e.to_role}(${statusText})`, subId:e.sub_id });
        msg.appendChild(makeSubsessionBtn(e.sub_id, label));
        if (isFail) msg.classList.add("fail-flash");
        msg.style.cursor="pointer";
        msg.addEventListener("click", () => { if (onShowSubsession) onShowSubsession(e.sub_id, label); });
        msg.addEventListener("contextmenu", (ev) => { ev.preventDefault(); if (onShowSubsession) onShowSubsession(e.sub_id, label); });
        currentDelegate=""; currentActivity="";
        const icon=currentRoleIcon||resolveIcon(e.from_role);
        updateFooter(); updateStatusPillLabel(`${icon} 思考中…`);
        container.rolePill.textContent = `${icon} ${e.from_role}`;
        resetWaitTimer(); break;
      }
      case "Done": setStatus("connected"); clearWaitTimer(); addMessage({ kind:"system", content:"[会话结束]" }); setFooter("会话结束"); break;
      case "Error": setStatus("connected"); clearWaitTimer(); addMessage({ kind:"error", content:e.message, meta:"error" }); setFooter(`错误: ${truncate(e.message,80)}`); break;
      default: console.warn("[chat] unknown event", e);
    }
  }

  function appendUser(content: string): string { const node = addMessage({ kind:"user", content }); return node.dataset.messageId!; }
  function setFooter(msg: string): void { container.footerMsg.textContent = msg; }
  function setRoleSelected(roleId: string): void { container.rolePill.textContent = roleId; container.roleSelect.value = roleId; }
  function refreshRoles(roles: RoleInfo[], selected: string): void {
    container.roleSelect.innerHTML = "";
    for (const r of roles) { const opt = document.createElement("option"); opt.value=r.id; opt.innerHTML=`${r.icon??""} ${r.id}`; if (r.id===selected) opt.selected=true; container.roleSelect.appendChild(opt); }
  }
  function clear(): void { container.messagesEl.innerHTML=""; msgCounter=0; currentToolCall=""; currentActivity=""; currentDelegate=""; delegateRunning=false; updateFooter(); }
  return { appendUser, handleEvent, setFooter, setRoleSelected, refreshRoles, setStatus, clear };
}

function truncate(s: string, max: number): string { if (s.length<=max) return s; return s.slice(0,max)+"…"; }
