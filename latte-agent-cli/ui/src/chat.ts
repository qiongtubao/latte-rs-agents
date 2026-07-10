// chat 视图：把 ChatEvent 序列化成 DOM 节点，处理用户输入与表单提交。
//
// 设计原则：
//   - 不引 React/Vue 等 UI 框架 — vanilla DOM 让 Playwright + screencap
//     闭环最稳定（每条消息有稳定的 [data-message-id]，截图变化可观察）。
//   - ChatEvent 全部从 /api/events 推送，用户消息发到 /api/chat/send。
//   - 错误/工具调用/状态消息各自走专属 class，方便 self-loop 视觉回归测试。

import {
  ChatEvent,
  RoleInfo,
  sendMessage,
  sendCommand,
  switchRole,
} from "./api";

interface UIBinding {
  messagesEl: HTMLElement;
  formEl: HTMLFormElement;
  inputEl: HTMLTextAreaElement;
  sendBtn: HTMLButtonElement;
  clearBtn: HTMLButtonElement;
  quitBtn: HTMLButtonElement;
  statusPill: HTMLElement;
  roleSelect: HTMLSelectElement;
  rolePill: HTMLElement;
  modelPill: HTMLElement;
  footerMsg: HTMLElement;
}

export interface ChatController {
  /** 添加消息到 UI。返回该消息的 DOM id（用于后续 patch）。 */
  appendUser(content: string): string;
  /** 派发一条 ChatEvent 到 UI。 */
  handleEvent(e: ChatEvent): void;
  /** 设置 footer 文本。 */
  setFooter(msg: string): void;
  /** 切换角色选择器当前选中项（不动后端）。 */
  setRoleSelected(roleId: string): void;
  /** 重建角色下拉。 */
  refreshRoles(roles: RoleInfo[], selected: string): void;
  /**
   * 直接覆盖状态药丸的当前状态（connected / disconnected / thinking /
   * stalled）。被外部（main.ts 的 SSE 连接状态回调）调用 —— 当 SSE
   * 断开时要把状态强制拉到 "disconnected"，不管 agent 当前是不是
   * 在 thinking。这样用户能看到"连接已断"，而不是药丸还停在"思考中"
   * 不动。
   */
  setStatus(s: "connected" | "disconnected" | "thinking" | "stalled"): void;
}

export function mountChat(opts: {
  container: UIBinding;
  initialRole: string;
  initialModel?: string;
  onRoleSwitch?: (roleId: string) => Promise<void>;
  /**
   * 用户点击"已断开"状态药丸时触发 —— 由 main.ts 注入，让 mountChat
   * 不用知道 SSE 怎么 reconnect，只是喊一声"给我重连"。
   */
  onReconnect?: () => void;
}): ChatController {
  const { container, initialRole, initialModel, onRoleSwitch } = opts;
  let msgCounter = 0;

  function nextId(): string {
    return `m${++msgCounter}`;
  }

  function addMessage(opts: {
    kind: "user" | "role" | "tool" | "status" | "system" | "error";
    content: string;
    meta?: string;
  }): HTMLElement {
    const div = document.createElement("div");
    div.className = `message ${opts.kind}`;
    div.dataset.messageId = nextId();
    if (opts.meta) {
      const meta = document.createElement("span");
      meta.className = "meta";
      meta.textContent = opts.meta;
      div.appendChild(meta);
    }
    const content = document.createElement("div");
    content.className = "content";
    // pre-wrap 保留换行（plain text）。
    content.textContent = opts.content;
    div.appendChild(content);
    // 关键：把 div 挂到 messagesEl 上。前面那次 inlining 不小心
    // 把这一行删了，导致 addMessage 创建了 div 但页面看不见。
    container.messagesEl.appendChild(div);
    // 只有在用户已经停在底部（或 50px 容差内）时才自动滚下去，
    // 避免他在翻看历史消息时被突然拉走。
    // scrollHeight - scrollTop - clientHeight 就是"距离底部多少 px"。
    const distanceFromBottom =
      container.messagesEl.scrollHeight -
      container.messagesEl.scrollTop -
      container.messagesEl.clientHeight;
    if (distanceFromBottom < 50) {
      container.messagesEl.scrollTop = container.messagesEl.scrollHeight;
    }
    return div;
  }

  // ─── form ───────────────────────────────────────────────────

  container.formEl.addEventListener("submit", async (e) => {
    e.preventDefault();
    const text = container.inputEl.value.trim();
    if (!text) return;
    container.inputEl.value = "";
    if (text.startsWith("/")) {
      addMessage({ kind: "system", content: `→ ${text}` });
      await sendCommand(text);
    } else {
      addMessage({ kind: "user", content: text });
      await sendMessage(text);
      setStatus("thinking");
      startWaitTimer();
    }
  });

  // Shift+Enter 换行；Enter 提交。
  container.inputEl.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      (container.formEl.querySelector('button[type="submit"]') as HTMLButtonElement)?.click();
    }
  });

  container.clearBtn.addEventListener("click", async () => {
    addMessage({ kind: "system", content: "→ /clear" });
    await sendCommand("/clear");
  });
  container.quitBtn.addEventListener("click", async () => {
    addMessage({ kind: "system", content: "→ /quit" });
    await sendCommand("/quit");
  });

  container.roleSelect.addEventListener("change", async () => {
    const roleId = container.roleSelect.value;
    if (onRoleSwitch) {
      await onRoleSwitch(roleId);
    } else {
      await switchRole(roleId);
    }
    addMessage({ kind: "system", content: `→ switched to /role ${roleId}` });
  });

  // ─── status pill ────────────────────────────────────────────
  //
  // 状态由三部分组成：
  //   - 左侧 ::before 伪元素（圆点 / 旋转环 / 警告符号）
  //   - 中间 .status-label：状态名（中文）
  //   - 右侧 .status-time：倒计时 / 累计时间（仅 thinking / stalled 显示）
  // 颜色和动画全部由 class 状态控制，JS 只切 class、改文字 / 计时、
  // 以及把 0-100% 进度同步到 --wait-progress 让底部进度条跟着填。

  // 在 mount 时一次性查到内部两个子元素（HTML 在 index.html 里已固
  // 定结构），避免每次 setStatus 都走 querySelector。
  const statusLabel = container.statusPill.querySelector(".status-label") as HTMLElement;
  const statusTime = container.statusPill.querySelector(".status-time") as HTMLElement;

  // 跟踪当前状态，让状态药丸的 click 处理器能判断要不要触发重连
  // （只有 disconnected 才需要重连；其他状态点了不做任何事）。
  let currentStatus: "connected" | "disconnected" | "thinking" | "stalled" =
    "disconnected";

  function setStatus(s: "connected" | "disconnected" | "thinking" | "stalled"): void {
    currentStatus = s;
    container.statusPill.className = `status-pill ${s}`;
    switch (s) {
      case "connected":
        statusLabel.textContent = "已连接";
        statusTime.textContent = "";
        // 已连接 / 已断开时不需要进度条
        container.statusPill.style.removeProperty("--wait-progress");
        break;
      case "disconnected":
        statusLabel.textContent = "已断开";
        statusTime.textContent = "";
        container.statusPill.style.removeProperty("--wait-progress");
        break;
      case "thinking":
        statusLabel.textContent = "思考中";
        break;
      case "stalled":
        statusLabel.textContent = "已卡住";
        break;
    }
  }

  // 状态药丸：已断开时点一下就触发重连（其他状态点了什么都不做）。
  // SSE auto-reconnect 已经在 api.ts 里被关掉，所以这里是真的手动重连。
  container.statusPill.addEventListener("click", () => {
    if (currentStatus === "disconnected" && opts.onReconnect) {
      opts.onReconnect();
    }
  });
  // 给药丸加个 title，hover 时告诉用户"点这里重连"
  container.statusPill.title = "点击重连";


  // ─── wait timer (client-side stall detection) ─────────────
  //
  // 用户发消息后如果在 `WAIT_TIMEOUT_MS` 内没看到 RoleTurn / Done /
  // Error 完成事件，就把 status pill 切到 stalled 并提示用户。这层
  // 是纯 UX 兜底；server 端 supervisor / per-call timeout 是真正
  // 的硬超时。SSE 的 keep-alive 仍在跑，所以迟到的响应会清掉这个
  // 警告，用户可以继续打字 / 重新提交。
  const WAIT_TIMEOUT_MS = 60_000;
  let waitTimer: number | null = null;
  // 每秒刷一次的 tick interval（独立于 setTimeout 的 60s 触发器），
  // 卡住后 tick 不停，让"已 XX秒"继续累加。
  let waitTickTimer: number | null = null;
  let waitStart = 0;

  /// 把秒数格式化成 "X秒" / "X分" / "X分Y秒"。倒计时和累计时间共用。
  function formatWaitTime(secs: number): string {
    if (secs < 60) return `${secs}秒`;
    const m = Math.floor(secs / 60);
    const s = secs % 60;
    return s === 0 ? `${m}分` : `${m}分${s}秒`;
  }

  /// 每秒调一次：根据当前进度决定显示倒计时（thinking）还是累计时
  /// 间（stalled），同时把 0-100% 进度同步到 --wait-progress CSS
  /// 变量，让 pill 底部的进度条跟着填。状态切换时也会被显式调一次
  /// 立刻刷新显示。
  function tickWaitTimer(): void {
    const elapsedMs = Date.now() - waitStart;
    const elapsedSecs = Math.floor(elapsedMs / 1000);
    // 进度条：0-1 映射到 0-100%，封顶 100%
    const progressPct = Math.min(100, (elapsedMs / WAIT_TIMEOUT_MS) * 100);
    container.statusPill.style.setProperty("--wait-progress", `${progressPct}%`);
    if (elapsedSecs < WAIT_TIMEOUT_MS / 1000) {
      // 还在 thinking 窗口里：显示倒计时（剩余时间），向上取整避
      // 免提前一秒钟就显示 0
      const remainingSecs = Math.max(0, Math.ceil((WAIT_TIMEOUT_MS - elapsedMs) / 1000));
      statusTime.textContent = `剩余 ${formatWaitTime(remainingSecs)}`;
    } else {
      // 已卡住：显示从开始算起的累计时间（向上计数）
      statusTime.textContent = `已 ${formatWaitTime(elapsedSecs)}`;
    }
  }

  function startWaitTimer(): void {
    clearWaitTimer();
    waitStart = Date.now();
    setStatus("thinking");
    tickWaitTimer(); // 立即更新一次，避免出现空字符串
    waitTickTimer = window.setInterval(tickWaitTimer, 1000);
    waitTimer = window.setTimeout(() => {
      setStatus("stalled");
      tickWaitTimer(); // 状态切换时立刻刷新一次显示
      // tick 继续运行，让卡住后的时间继续累加
      const secs = Math.floor((Date.now() - waitStart) / 1000);
      addMessage({
        kind: "status",
        content: `[卡住] 模型 ${secs}秒 内无响应。可以继续输入，agent 可能稍后恢复；也可以 /clear 重试。`,
      });
    }, WAIT_TIMEOUT_MS);
  }

  function clearWaitTimer(): void {
    if (waitTimer !== null) {
      window.clearTimeout(waitTimer);
      waitTimer = null;
    }
    if (waitTickTimer !== null) {
      window.clearInterval(waitTickTimer);
      waitTickTimer = null;
    }
  }

  if (initialModel) container.modelPill.textContent = initialModel;
  container.rolePill.textContent = initialRole;

  function handleEvent(e: ChatEvent): void {
    switch (e.type) {
      case "RoleTurn":
        addMessage({ kind: "role", content: e.content, meta: `${e.role_id} · ${e.is_complete ? "done" : "streaming…"}` });
        if (e.is_complete) {
          setStatus("connected");
          clearWaitTimer();
        }
        break;
      case "Status":
        addMessage({ kind: "status", content: e.message });
        container.footerMsg.textContent = e.message;
        break;
      case "Prompt":
        container.rolePill.textContent = e.role_id;
        container.modelPill.textContent = e.model_id;
        break;
      case "Paused":
        addMessage({ kind: "status", content: `[paused] ${e.reason}` });
        break;
      case "Resumed":
        addMessage({ kind: "status", content: "[resumed]" });
        break;
      case "RoundStarted":
        addMessage({ kind: "system", content: `[round ${e.round} started]` });
        break;
      case "RoundEnded":
        addMessage({ kind: "system", content: `[round ${e.round} ended]` });
        break;
      case "ToolUse":
        addMessage({ kind: "tool", content: `${e.tool_name}  ${truncate(e.args, 400)}`, meta: e.role_id });
        break;
      case "ToolResult":
        addMessage({ kind: "tool", content: `${e.tool_name} → ${truncate(e.result, 400)}`, meta: e.role_id });
        break;
      case "DelegateStarted":
        addMessage({ kind: "system", content: `${e.from_role} → ${e.to_role}: ${truncate(e.task, 200)}` });
        break;
      case "DelegateFinished":
        addMessage({ kind: "system", content: `${e.from_role} ← ${e.to_role} (${e.status}): ${truncate(e.summary, 200)}` });
        break;
      case "RoleList":
        // 用户在 multi-role 模式里敲了 `/roles`，controller 把当前
        // round 跑的角色列表回传 —— 用它重建下拉。
        if (e.roles && e.roles.length > 0) {
          refreshRoles(e.roles, e.roles[0]?.id ?? initialRole);
        }
        break;
      case "RoleStarted":
        // 已经在 RoleTurn 里渲染；这里空操作。
        break;
      case "RoleFinished":
        addMessage({ kind: "system", content: `${e.role_id} finished: ${e.detail}` });
        break;
      case "ContextCleared":
        container.messagesEl.innerHTML = "";
        addMessage({ kind: "system", content: "[context cleared]" });
        break;
      case "SessionInfo":
        // multi-role 模式启动时 controller 会发这个事件，e.roles 里
        // 有本次 session 实际跑的角色列表。用它重建下拉。rolePill
        // 留给 Prompt 事件去更新"当前正在说话的角色"，这里不要
        // 把 task_id 塞进去（之前这里写成 rolePill = task_id，错的）。
        if (e.roles && e.roles.length > 0) {
          refreshRoles(e.roles, e.roles[0]?.id ?? initialRole);
        }
        break;
      case "Done":
        setStatus("connected");
        clearWaitTimer();
        addMessage({ kind: "system", content: "[session done]" });
        break;
      case "Error":
        setStatus("connected");
        clearWaitTimer();
        addMessage({ kind: "error", content: e.message, meta: "error" });
        break;
      default:
        // 未知事件：忽略（不抛错，方便后端扩展）。
        console.warn("[chat] unknown event", e);
    }
  }

  function appendUser(content: string): string {
    const node = addMessage({ kind: "user", content });
    return node.dataset.messageId!;
  }

  function setFooter(msg: string): void {
    container.footerMsg.textContent = msg;
  }

  function setRoleSelected(roleId: string): void {
    container.rolePill.textContent = roleId;
    container.roleSelect.value = roleId;
  }

  function refreshRoles(roles: RoleInfo[], selected: string): void {
    container.roleSelect.innerHTML = "";
    for (const r of roles) {
      const opt = document.createElement("option");
      opt.value = r.id;
      opt.textContent = `${r.icon ?? ""} ${r.id}`;
      if (r.id === selected) opt.selected = true;
      container.roleSelect.appendChild(opt);
    }
  }

  return { appendUser, handleEvent, setFooter, setRoleSelected, refreshRoles, setStatus };
}

function truncate(s: string, max: number): string {
  if (s.length <= max) return s;
  return s.slice(0, max) + "…";
}
