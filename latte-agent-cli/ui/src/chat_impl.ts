import { ChatEvent, RoleInfo, sendMessage, sendChoiceAnswer, dismissPrompt, sendCommand, switchRole, cancelTurn, cancelSubagent, pauseSessionV2, resumeSessionV2, pauseRole, resumeRole, importTasks, refineParentFor, uploadImage, listWorkflows, listTasks, resumeWorkflow, getCurrentSessionId, type ImportTask, type ChoiceOption, type TaskView } from "./api";
import { BUILTIN_CMD_HINTS, mergeWorkflowCommands, type CmdHint } from "./cmd_hints";

/** ChoiceRequested 事件的窄化类型（从 ChatEvent union 抽出）。 */
type ChoiceRequestedEvent = Extract<ChatEvent, { type: "ChoiceRequested" }>;
import { extractImportableTasks } from "./workflows_panel";
import { extractCodeRefs, makeRefChips } from "./linkify";
import { eventIdentity } from "./event_identity";
import { buildSelectedPlanTasks, type PlanImportRowInput } from "./plan_import";
import { parentOptions } from "./task_board";
import type { CodeRef } from "./host";
interface UIBinding {

  messagesEl: HTMLElement; formEl: HTMLFormElement; inputEl: HTMLTextAreaElement;
  sendBtn: HTMLButtonElement; clearBtn: HTMLButtonElement; quitBtn: HTMLButtonElement;
  /** Pause button. */
  pauseBtn: HTMLButtonElement;
  /** Resume button. */
  resumeBtn: HTMLButtonElement;
  statusPill: HTMLElement; roleSelect: HTMLSelectElement;
  /** Per-role pause/resume toggle (acts on the selected role). */
  rolePauseToggle: HTMLButtonElement;
  rolePill: HTMLElement;
  modelPill: HTMLElement; footerMsg: HTMLElement;
}
export interface ChatController {
  appendUser(content: string): string;
  handleEvent(e: ChatEvent): void;
  replayEvents(events: ChatEvent[]): void;
  setFooter(msg: string): void;
  setRoleSelected(roleId: string): void;
  refreshRoles(roles: RoleInfo[], selected: string): void;
  setStatus(status: "connected" | "disconnected" | "thinking" | "stalled"): void;
  clear(): void;
  focus(): void;
  insertContext(ref: CodeRef & { quote?: string }): void;
  setRoleFilePaths(paths: Record<string, string>): void;
}

const ROLE_ICONS: Record<string, string> = {
  manager: "👔", programmer: "💻", architect: "🏗️", reviewer: "🔍",
  reviewer_sanity: "🔍", reviewer_architecture: "📐", reviewer_security: "🔒",
  tester: "🧪", security: "🛡️", devops: "⚙️", designer: "🎨",
  tech_writer: "📝", pm: "📋",
  // workflow 不是角色（是固定流水线发起方）：不给角色头像，只用图标
  // 标记分派来源。
  workflow: "🔀",
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

// ─── Markdown 渲染 ──────────────────────────────────────────────
// 模型输出是 Markdown，但内容不可信：所有文本先 escapeHtml，再在
// 转义后的文本上做块级/行内转换，因此任何原始 HTML 都会被转义，
// 不存在注入面。覆盖聊天消息常用语法：围栏代码块、标题、无序/
// 有序列表、行内 code、粗体、斜体、http(s) 链接。

/** 行内转换。输入必须是已 escapeHtml 的文本。 */
function renderInline(escaped: string): string {
  // 行内 code 先抽出来占位，避免其中的 `*` 等被后续规则误转。
  const codes: string[] = [];
  let s = escaped.replace(/`([^`\n]+)`/g, (_m, c: string) => {
    codes.push(c);
    return "\u0000" + (codes.length - 1) + "\u0000";
  });
  // 链接 [text](url) —— 仅放行 http(s)，其余按原文显示。
  s = s.replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, (m, text: string, url: string) =>
    /^https?:\/\//i.test(url)
      ? `<a href="${url}" target="_blank" rel="noopener noreferrer">${text}</a>`
      : m);
  // 粗体先于斜体（否则 ** 会被 * 规则吃掉）。
  s = s.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
  s = s.replace(/\*([^*\n]+)\*/g, "<em>$1</em>");
  // 还原行内 code（内容已转义，不再做任何转换）。
  s = s.replace(/\u0000(\d+)\u0000/g, (_m, i: string) => `<code>${codes[Number(i)]}</code>`);
  return s;
}

/** 块级转换：标题、列表、空行分段。输入必须是已 escapeHtml 的文本。 */
function renderBlocks(escaped: string): string {
  const lines = escaped.split("\n");
  const out: string[] = [];
  let para: string[] = [];
  let list: { type: "ul" | "ol"; items: string[] } | null = null;

  const flushPara = () => {
    if (para.length) {
      out.push(`<p>${renderInline(para.join("\n"))}</p>`);
      para = [];
    }
  };
  const flushList = () => {
    if (list) {
      out.push(`<${list.type}>${list.items.map((i) => `<li>${renderInline(i)}</li>`).join("")}</${list.type}>`);
      list = null;
    }
  };

  for (const line of lines) {
    const h = line.match(/^(#{1,4})\s+(.*)$/);
    const ul = line.match(/^\s*[-*]\s+(.*)$/);
    const ol = line.match(/^\s*\d+[.)]\s+(.*)$/);
    if (h) {
      flushPara(); flushList();
      // # → h3、## → h4、### → h5：气泡内标题不宜比正文大太多。
      const tag = `h${Math.min(h[1].length + 2, 5)}`;
      out.push(`<${tag}>${renderInline(h[2])}</${tag}>`);
    } else if (ul) {
      flushPara();
      if (!list || list.type !== "ul") { flushList(); list = { type: "ul", items: [] }; }
      list.items.push(ul[1]);
    } else if (ol) {
      flushPara();
      if (!list || list.type !== "ol") { flushList(); list = { type: "ol", items: [] }; }
      list.items.push(ol[1]);
    } else if (line.trim() === "") {
      flushPara(); flushList();
    } else {
      flushList();
      para.push(line);
    }
  }
  flushPara(); flushList();
  return out.join("");
}

export function renderMarkdown(text: string): string {
  if (!text.includes("```")) return renderBlocks(escapeHtml(text));
  const parts: string[] = [];
  let remaining = text;
  while (true) {
    const start = remaining.indexOf("```");
    if (start === -1) { parts.push(renderBlocks(escapeHtml(remaining))); break; }
    parts.push(renderBlocks(escapeHtml(remaining.slice(0, start))));
    const after = remaining.slice(start + 3);
    const end = after.indexOf("```");
    if (end === -1) { parts.push(renderBlocks(escapeHtml(remaining))); break; }
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
  onShowSubsession?: (subId: string, label: string, anchor?: HTMLElement) => void;
  onShowSessionLog?: (anchor?: HTMLElement) => Promise<void> | void;
  onEditRole?: (roleId: string) => void;

  /** Fork a new session from the given event prefix (the source
   *  session's visible history up to and including the right-clicked
   *  message). main.ts wires this to forkSession() + activateSession(). */
  onFork?: (events: ChatEvent[]) => Promise<void> | void;

  onReconnect?: () => void;
}): ChatController {
  const { container, initialRole, initialModel, onRoleSwitch, onShowSubsession, onShowSessionLog, onEditRole, onFork } = opts;
  let msgCounter = 0;
  let turnStartTime = 0;
  // ── Fork support: mirror the ordered ChatEvent stream this session
  // has processed (replay + live), so a right-click can fork the
  // discussion up to a chosen message. `currentEventIdx` is the index
  // (into `allEvents`) of the event currently being handled; message
  // rows are tagged with it so the context menu can map a clicked row
  // back to an event-stream position. -1 = not inside handleEvent
  // (synthetic local rows like "→ /clear" stay untagged, no fork).
  const allEvents: ChatEvent[] = [];
  let currentEventIdx = -1;
  /** 当前显示的「超时询问」条；同一 role 重复触发替换之，
   *  跨 role 的并行执行按 role 维度分别保留。 */
  /** 超时询问条：key → 该条 DOM。
   *
   *  key 是 `sub_id ?? role_id`：DAG 并行波会同时跑多条分派
   *  （design_and_plan 的 req_review ‖ code_review），而同一 role 也
   *  可能在一个 wave 里出现两次，所以只有 sub_id 才唯一。主 turn 的
   *  超时不带 sub_id，退化用 role_id 作 key。
   *
   *  此前是单槽（timeoutPromptEl + timeoutPromptRole），后到的 warning
   *  会把前一条**静默顶掉**，且按钮打的是全局 cancelTurn —— 用户看到
   *  的是 B 的提示，按下去杀的是整轮。 */
  const timeoutPrompts = new Map<string, HTMLElement>();
  /** 承载所有询问条的容器，置顶。按需创建、空了就移除。 */
  let timeoutStackEl: HTMLElement | null = null;
  /** advisor 暂停期间的「等待拍板」横幅；拍板或 Resumed 事件后清除。 */
  let advisorPauseBannerEl: HTMLElement | null = null;

  /** Last event identity rendered. SSE reconnects can replay the same
   * persisted event immediately after live delivery; suppress only exact
   * adjacent duplicates, preserving legitimate repeated turns. */
  let lastEventIdentity = "";
  // ── Delegate tracking (keyed by sub_id for parallel delegates) ──
  interface DelegateInfo {
    targetRole: string;
    taskText: string;
    delegateMsgId: string;
    capturedTools: { tool: string; args: string; result?: string }[];
    /** "⏳ 执行中…" badge on the DelegateStarted bubble. */
    stateEl?: HTMLElement;
    /** 流程内分派带 wf_id；独立委派为 undefined。终止确认文案据此
     *  区分「结束整次 workflow」与「只停这一条委派」。 */
    wfId?: string;
  }
  const activeDelegates = new Map<string, DelegateInfo>();
  let currentDelegateSubId = ""; // most recent delegate (for non-sub_id legacy events)
  /** wf_id → workflow run status and display name. */
  const workflowStates = new Map<string, HTMLElement>();
  const wfNames = new Map<string, string>();
  /** WorkflowStep metadata is paired FIFO with the next DelegateStarted. */
  const workflowStepQueues = new Map<string, Array<{ key: string; roleId: string; taskText: string }>>();
  const workflowStepMsgIds = new Map<string, string>();
  const workflowStepSubIds = new Map<string, string>();
  /** role_id + sub_id → active streaming response bubble. */
  const streamingEl = new Map<string, HTMLElement>();
  /** role_id + sub_id → accumulated raw streaming text. */
  const streamingRaw = new Map<string, string>();
  const streamKey = (roleId: string, subId?: string | null): string =>
    `${roleId}\u0000${subId ?? ""}`;
  function clearStream(roleId: string, subId?: string | null): void {
    const key = streamKey(roleId, subId);
    streamingEl.delete(key);
    streamingRaw.delete(key);
  }
  function clearAllStreams(): void {
    streamingEl.clear();
    streamingRaw.clear();
  }
  function forgetDelegate(subId: string): void {
    delegateToolCounts.delete(subId);
    activeDelegates.delete(subId);
    if (currentDelegateSubId === subId) {
      currentDelegateSubId = "";
      // Preserve the legacy no-sub_id fallback only when another delegate is
      // genuinely still active; otherwise the next main RoleStarted must not
      // inherit the retired subsession.
      for (const activeSubId of activeDelegates.keys()) {
        currentDelegateSubId = activeSubId;
      }
    }
  }
  /** Drop-only/abort paths can miss DelegateFinished. Reconcile the live
   *  badge/map without synthesising a duplicate chat message. */
  function retireDelegate(subId: string, status: string): void {
    const di = activeDelegates.get(subId);
    if (!di) return;
    clearStream(di.targetRole, subId);
    const badge = di.stateEl ?? delegateBadge(subId);
    if (badge) {
      badge.textContent = `❌ ${status}`;
      badge.className = "delegate-state failed";
    }
    hideTimeoutPromptByKey(subId);
    forgetDelegate(subId);
  }
  function retireWorkflowDelegates(wfId: string, status: string): void {
    for (const [subId, di] of [...activeDelegates]) {
      if (di.wfId === wfId) retireDelegate(subId, status);
    }
  }
  function retireTurnDelegates(status: string): void {
    for (const [subId, di] of [...activeDelegates]) {
      if (!di.wfId) retireDelegate(subId, status);
    }
  }
  function retireAllDelegates(status: string): void {
    for (const subId of [...activeDelegates.keys()]) retireDelegate(subId, status);
  }
  /** wf_id → workflow output transcript used for plan import extraction. */
  const workflowTranscripts = new Map<string, string>();
  /** role_id → 配置文件 basename，由 main.ts 加载后注入 */
  let roleFilePaths = new Map<string, string>();

  /** Find the (most recent) pending delegate targeting `roleId` —
   *  fallback for tool events without sub_id (legacy archives / direct
   *  feeds); current backend fills sub_id on delegate tool events, so
   *  this only fires for old data. */
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
    delegateToolCounts.clear();
  }
  /** sub_id → 该 subsession 已发生的工具调用数（心跳用，见
   *  [`bumpDelegateHeartbeat`]）。 */
  const delegateToolCounts = new Map<string, number>();
  /** subagent 的工具事件按设计不进主对话正文（详情面板里有完整流水），
   *  但一次委派可能连续几分钟没有任何可见变化，看起来就像卡死了
   *  （实测：单个 refine step 跑了 228 秒、42 条工具事件，
   *  主对话零变化）。这里给它加心跳：delegate 气泡的「⏳ 执行中…」
   *  徽章与该角色的 executing 行都带上「工具调用数 + 最近工具名」。 */
  /** delegate 气泡上的「⏳ 执行中…」徽章。
   *
   *  优先用 `activeDelegates` 里的 live 引用；取不到时按 sub_id 从 DOM
   *  里找回 —— 历史回放会把 `activeDelegates` 清空（replayEvents 视历史
   *  为「已发生」），但 session 完全可能还在跑（刷新页面 / 切回来时的
   *  常态），此时后续 live 事件仍要能翻转这枚徽章。 */
  function delegateBadge(subId: string): HTMLElement | null {
    const live = activeDelegates.get(subId)?.stateEl;
    if (live) return live;
    const sel = `.message-row[data-sub-id="${subId.replace(/"/g, '\\"')}"] .delegate-state`;
    return container.messagesEl.querySelector(sel) as HTMLElement | null;
  }
  function bumpDelegateHeartbeat(subId: string, roleId: string, toolName: string): void {
    const n = (delegateToolCounts.get(subId) ?? 0) + 1;
    delegateToolCounts.set(subId, n);
    const badge = delegateBadge(subId);
    if (badge) badge.textContent = `⏳ 执行中… 🔧${n} ${toolName}`;
    const row = latestExecutingRow(roleId, subId);
    if (row && row.subId === subId) {
      const content = row.row.querySelector(".message.status .content");
      if (content) content.textContent = `🧠 ${roleId} 执行中… 🔧${n} ${toolName}`;
    }
  }
  let lastUserMsgId = "";
  let lastRoleStarted = "";
  let subagentTools: string[] = [];
  /** role_id → pending status rows; sub_id distinguishes parallel delegates. */
  const executingRowsByRole = new Map<string, Array<{ row: HTMLElement; subId: string }>>();
  function pushExecutingRow(roleId: string, row: HTMLElement, subId: string): void {
    const q = executingRowsByRole.get(roleId) ?? [];
    q.push({ row, subId });
    executingRowsByRole.set(roleId, q);
  }

  /** 工具调用/结果的折叠落点：优先按 sub_id 精确命中该 subsession
   *  的未结行（并行委派同一 role 时不会串行）；事件不带 sub_id
   *  （主 session 角色、旧归档回放）时退回该 role 最近一条未结行。 */
  function latestExecutingRow(roleId: string, subId?: string | null): { row: HTMLElement; subId: string } | undefined {
    const q = executingRowsByRole.get(roleId);
    if (!q || q.length === 0) return undefined;
    if (subId) {
      const hit = q.find((e) => e.subId === subId);
      if (hit) return hit;
    }
    return q[q.length - 1];
  }

  /** RoleFinished/Error 配对取出并移除一条未结行：优先 sub_id 精确
   *  命中；事件不带 sub_id（主角色 turn、旧归档回放）或未命中时
   *  退化为该 role 最早的未结行——不变量是「每个 finish 清一行」，
   *  不留孤儿行。 */
  function takeExecutingRow(
    roleId: string,
    subId: string | null | undefined,
  ): { row: HTMLElement; subId: string } | undefined {
    const q = executingRowsByRole.get(roleId);
    if (!q || q.length === 0) return undefined;
    let idx = subId ? q.findIndex((e) => e.subId === subId) : -1;
    if (idx < 0) idx = 0;
    const [entry] = q.splice(idx, 1);
    if (q.length === 0) executingRowsByRole.delete(roleId);
    return entry;
  }

  // 把工具调用折叠进 role 的 executing 状态行（而不是独立气泡）。
  // - 第一次调用时在状态行内创建一个 .tool-log 子元素并折叠状态。
  // - 同一 role 后续的工具调用追加在 .tool-log 内。
  // - 返回 true 表示已折叠，false 表示需要降级为独立气泡。
  function appendToolToExecutingRow(roleId: string, line: string, subId?: string | null): boolean {
    const execEntry = latestExecutingRow(roleId, subId);
    if (!execEntry) return false;
    const row = execEntry.row;
    if (!row) return false;
    const inner = row.querySelector(".message.status") as HTMLElement | null;
    if (!inner) return false;
    let log = inner.querySelector(".tool-log") as HTMLElement | null;
    if (!log) {
      log = document.createElement("div");
      log.className = "tool-log collapsed";
      log.dataset.full = "";
      const header = document.createElement("div");
      header.className = "tool-log-header";
      header.textContent = "🧰 工具调用";
      header.addEventListener("click", (e) => {
        e.stopPropagation();
        log!.classList.toggle("collapsed");
      });
      log.appendChild(header);
      const body = document.createElement("div");
      body.className = "tool-log-body";
      log.appendChild(body);
      // 计数徽章，跟在 header 末尾
      const count = document.createElement("span");
      count.className = "tool-log-count";
      body.dataset.count = "0";
      header.appendChild(count);
      inner.appendChild(log);
    }
    const body = log.querySelector(".tool-log-body") as HTMLElement;
    const entry = document.createElement("div");
    entry.className = "tool-log-line";
    entry.textContent = line;
    body.appendChild(entry);
    const c = body.dataset.count ? parseInt(body.dataset.count, 10) + 1 : 1;
    body.dataset.count = String(c);
    const countBadge = log.querySelector(".tool-log-count") as HTMLElement;
    countBadge.textContent = ` (${c})`;
    return true;
  }

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
    /** plan 工具提交的任务候选：右键「导入任务看板」读它重开弹窗。 */
    planTasks?: ImportTask[];
    /** PlanProposed 事件的 plan_id：补救路径重开弹窗时随导入请求带上，
     *  让后端把对应 session 的 plan 阶段门置为 Approved。 */
    planId?: string;
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
    /** plan 工具提交的任务候选（PlanProposed 事件渲染的消息带它）。 */
    planTasks?: ImportTask[];
    /** PlanProposed 事件的 plan_id（与 planTasks 配套，导入时透传后端）。 */
    planId?: string;
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
    // Tag with the event-stream index so a right-click can fork the
    // discussion up to this message. Only rows produced while handling
    // a real ChatEvent get a valid index; synthetic local rows don't.
    if (currentEventIdx >= 0) row.dataset.eventSeq = String(currentEventIdx);

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
        content.innerHTML = renderMarkdown(opts2.content);
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
      planTasks: opts2.planTasks,
      planId: opts2.planId,
      el: row,
    });
    container.messagesEl.appendChild(row);
    if (isNearBottom()) scrollToBottom();
    return row;
  }
  // ── ask 选择框（弹窗 + 消息流内联卡片）──
  // ChoiceRequested 事件触发。选项/说明/图片来自模型（不可信），一律
  // 用 textContent / img.src 构建，绝不 innerHTML 拼接。回传分两路：
  // wait=true（子代理阻塞等答）→ POST choice-answer 直达等待方；
  // wait=false/缺省（顶层 turn）→ 拼成 user 消息经 sendMessage 回喂
  // （后端会 echo 一条 UserMessage 事件渲染用户气泡，这里不手动补）。
  //
  // 呈现：卡片本体建在消息流里（历史留痕），随后**被移进居中弹窗**
  // （同一个 DOM 节点搬家，状态/监听全保留）。用户不下拉到底也能看
  // 见「会话正卡在等你选择」。收起弹窗后右下角留一枚待办铃，可随时
  // 重新展开。

  /** 一次待回答的选择（卡片 + 它在消息流里的归属地）。 */
  interface ChoiceEntry {
    e: ChoiceRequestedEvent;
    /** 选择卡片本体。在消息流与弹窗之间搬家，永远只有一份。 */
    card: HTMLElement;
    /** 卡片在消息流里的家（system 消息行）。弹窗关闭时归位。 */
    home: HTMLElement;
    /** 卡片被移进弹窗时，home 里显示的「点此展开」占位。 */
    hint: HTMLElement;
    /** 已提交 / 已跳过。 */
    answered: boolean;
    /** 用户主动收起过：不再自动弹，只能由待办铃点开。 */
    collapsed: boolean;
  }
  const choiceQueue: ChoiceEntry[] = [];
  let activeChoice: ChoiceEntry | null = null;
  let choiceOverlayEl: HTMLElement | null = null;
  let choicePillEl: HTMLElement | null = null;
  let choiceKeyHandler: ((ev: KeyboardEvent) => void) | null = null;

  function pendingChoices(): ChoiceEntry[] {
    return choiceQueue.filter((x) => !x.answered);
  }

  /** 右下角待办铃：还有没在弹窗里显示的待答选择时出现。 */
  function refreshChoicePill(): void {
    const waiting = pendingChoices().filter((x) => x !== activeChoice);
    if (waiting.length === 0) {
      choicePillEl?.remove();
      choicePillEl = null;
      return;
    }
    if (!choicePillEl) {
      const pill = document.createElement("button");
      pill.type = "button";
      pill.className = "choice-pending-pill";
      pill.addEventListener("click", () => {
        const next = pendingChoices().find((x) => x !== activeChoice);
        if (!next) return;
        next.collapsed = false;
        openChoiceModal(next);
      });
      document.body.appendChild(pill);
      choicePillEl = pill;
    }
    choicePillEl.textContent =
      waiting.length === 1
        ? "❓ 有 1 个选择等你回答 · 点击展开"
        : `❓ 有 ${waiting.length} 个选择等你回答 · 点击展开`;
  }

  /** 关掉弹窗外壳，把卡片搬回消息流。不改 answered/collapsed。 */
  function teardownChoiceModal(): void {
    const entry = activeChoice;
    activeChoice = null;
    if (choiceKeyHandler) {
      document.removeEventListener("keydown", choiceKeyHandler);
      choiceKeyHandler = null;
    }
    if (entry) {
      entry.home.appendChild(entry.card);
      entry.hint.remove();
    }
    choiceOverlayEl?.remove();
    choiceOverlayEl = null;
  }

  /** 用户收起弹窗（Esc / 点遮罩 / 点「稍后再选」）：卡片归位，留铃。 */
  function collapseChoiceModal(): void {
    if (activeChoice) activeChoice.collapsed = true;
    teardownChoiceModal();
    refreshChoicePill();
  }

  /** 把某个待答选择放进居中弹窗。 */
  function openChoiceModal(entry: ChoiceEntry): void {
    if (activeChoice === entry) return;
    if (entry.answered) return;
    // 让位：正在显示的那个先收起（保留在队列里，铃里还能点回来）。
    if (activeChoice) collapseChoiceModal();
    activeChoice = entry;
    entry.collapsed = false;

    const overlay = document.createElement("div");
    overlay.className = "choice-overlay";
    overlay.id = "choice-modal";
    const box = document.createElement("div");
    box.className = "choice-modal";
    overlay.appendChild(box);

    const head = document.createElement("div");
    head.className = "choice-modal__head";
    const title = document.createElement("div");
    title.className = "choice-modal__title";
    title.textContent = `❓ ${entry.e.role_id} 请你选择`;
    head.appendChild(title);
    if (entry.e.wait) {
      const blocked = document.createElement("span");
      blocked.className = "choice-modal__blocked";
      blocked.textContent = "已阻塞等待你的回答";
      head.appendChild(blocked);
    }
    const later = document.createElement("button");
    later.type = "button";
    later.className = "choice-modal__later";
    later.textContent = "稍后再选";
    later.title = "收起弹窗（Esc）；右下角待办铃可重新展开";
    later.addEventListener("click", collapseChoiceModal);
    head.appendChild(later);
    box.appendChild(head);

    const body = document.createElement("div");
    body.className = "choice-modal__body";
    body.appendChild(entry.card); // 搬家：同一节点，状态不丢
    box.appendChild(body);

    // home 里留个可点的占位，说明卡片去哪了。
    entry.hint.textContent = "（选择框已弹出显示 · 点此重新展开）";
    entry.hint.style.display = "";
    entry.home.appendChild(entry.hint);

    // 点遮罩空白处 = 收起；点弹窗内部不关。
    overlay.addEventListener("click", (ev) => {
      if (ev.target === overlay) collapseChoiceModal();
    });
    choiceKeyHandler = (ev: KeyboardEvent) => {
      // 弹窗已不在文档里（session 切换 / 整个 chat 被重挂）→ 顺手摘掉
      // 监听并放行按键，别让僵尸实例继续响应 Esc。
      if (!choiceOverlayEl || !choiceOverlayEl.isConnected) {
        if (choiceKeyHandler) document.removeEventListener("keydown", choiceKeyHandler);
        choiceKeyHandler = null;
        return;
      }
      if (ev.key === "Escape") {
        ev.preventDefault();
        collapseChoiceModal();
      }
    };
    document.addEventListener("keydown", choiceKeyHandler);

    document.body.appendChild(overlay);
    choiceOverlayEl = overlay;
    refreshChoicePill();
  }

  /** 队列里挑下一个「没被用户收起过」的待答选择自动弹出。 */
  function pumpChoiceQueue(): void {
    if (!activeChoice) {
      const next = pendingChoices().find((x) => !x.collapsed);
      if (next) {
        openChoiceModal(next); // 内部会刷新待办铃
        return;
      }
    }
    // 弹窗已被别的选择占着（或都被用户收起）→ 至少把待办铃摆出来。
    refreshChoicePill();
  }

  /** 卡片已提交/跳过：关弹窗、卡片归位（answered 态留在消息流里）。 */
  function settleChoice(entry: ChoiceEntry): void {
    entry.answered = true;
    if (activeChoice === entry) teardownChoiceModal();
    entry.hint.remove();
    refreshChoicePill();
    pumpChoiceQueue();
  }

  /** 切 session / clear / replay 结束：清掉弹窗与待办铃，不留僵尸框。 */
  function clearChoiceDialogs(): void {
    teardownChoiceModal();
    choiceQueue.length = 0;
    choicePillEl?.remove();
    choicePillEl = null;
  }

  /** 多题弹框：一个卡片渲染 N 道题，**全部答完**才能提交，提交时一次
   *  把 N 个答案送出。
   *
   *  为什么要这样而不是弹 N 个单题框：顶层 `ask` 是 fire-and-forget，
   *  每个答案各自成一条 user 消息、各自一个 turn，于是角色会在只拿到
   *  部分答案时就往下走。实测（2026-09-07 jemalloc 会话）manager 一轮
   *  问 3 道，用户答完 2 道它就启动了 4 条 workflow，剩下 2 道的答案
   *  在那些流水线跑完之后才到 —— 前面全按默认假设做了。 */
  function renderMultiChoiceDialog(
    bubble: HTMLElement,
    e: ChoiceRequestedEvent,
    opts?: { archived?: boolean },
  ): void {
    const archived = !!opts?.archived;
    const questions = e.questions ?? [];
    let forceMessage = false;
    const card = document.createElement("div");
    card.className = "choice-card multi-q" + (archived ? " answered" : "");
    card.dataset.choiceId = e.choice_id;
    let entry: ChoiceEntry | null = null;

    const head = document.createElement("div");
    head.className = "choice-head";
    const chip = document.createElement("span");
    chip.className = "choice-chip multi";
    chip.textContent = `${questions.length} 道题 · 全部答完后一起提交`;
    head.appendChild(chip);
    card.appendChild(head);

    /** 每题的选中项标签（按题序）。 */
    const picked: (string[] | null)[] = questions.map(() => null);

    const submitBtn = document.createElement("button");
    submitBtn.className = "choice-submit";
    submitBtn.textContent = "提交全部";
    submitBtn.disabled = true;
    const statusLine = document.createElement("div");
    statusLine.className = "choice-status";

    function refresh(): void {
      const done = picked.filter((p) => p && p.length > 0).length;
      statusLine.textContent = `已答 ${done} / ${questions.length}`;
      // 少一题就不许提交 —— 这是"收齐才回"在 UI 侧的落点。
      submitBtn.disabled = done < questions.length;
    }

    const qsWrap = document.createElement("div");
    qsWrap.className = "choice-questions";
    questions.forEach((q, qi) => {
      const qBlock = document.createElement("div");
      qBlock.className = "choice-q-block";
      qBlock.dataset.questionId = q.id;

      const qHead = document.createElement("div");
      qHead.className = "choice-q-head";
      const qChip = document.createElement("span");
      qChip.className = "choice-chip" + (q.multi ? " multi" : "");
      qChip.textContent = q.multi ? "多选" : "单选";
      qHead.appendChild(qChip);
      const qText = document.createElement("div");
      qText.className = "choice-question";
      // textContent：选项与题面来自模型，一律按纯文本渲染，不解释 HTML。
      qText.textContent = `${qi + 1}. ${q.question}`;
      qHead.appendChild(qText);
      qBlock.appendChild(qHead);

      const optsWrap = document.createElement("div");
      optsWrap.className = "choice-opts" + (q.layout === "grid" ? " grid" : "");
      const sel = new Set<number>();
      q.options.forEach((opt, oi) => {
        const btn = document.createElement("button");
        btn.className = "choice-opt";
        btn.type = "button";
        const label = document.createElement("div");
        label.className = "choice-opt-label";
        label.textContent = opt.label;
        btn.appendChild(label);
        if (opt.description) {
          const d = document.createElement("div");
          d.className = "choice-opt-desc";
          d.textContent = opt.description;
          btn.appendChild(d);
        }
        btn.addEventListener("click", () => {
          if (q.multi) {
            if (sel.has(oi)) sel.delete(oi);
            else sel.add(oi);
          } else {
            sel.clear();
            sel.add(oi);
          }
          Array.from(optsWrap.children).forEach((c, ci) =>
            c.classList.toggle("selected", sel.has(ci)),
          );
          picked[qi] = Array.from(sel)
            .sort((a, b) => a - b)
            .map((i) => q.options[i].label);
          refresh();
        });
        optsWrap.appendChild(btn);
      });
      qBlock.appendChild(optsWrap);
      qsWrap.appendChild(qBlock);
    });
    card.appendChild(qsWrap);

    const foot = document.createElement("div");
    foot.className = "choice-foot";
    foot.appendChild(statusLine);
    foot.appendChild(submitBtn);
    card.appendChild(foot);

    function finish(summary: string, send: null | (() => void)): void {
      card.classList.add("answered");
      qsWrap.style.display = "none";
      foot.style.display = "none";
      const done = document.createElement("div");
      done.className = "choice-answer";
      done.textContent = summary;
      card.appendChild(done);
      void dismissPrompt(e.choice_id).catch((err) =>
        console.warn("[chat] prompt dismiss failed:", err),
      );
      if (entry) settleChoice(entry);
      send?.();
    }

    submitBtn.addEventListener("click", () => {
      const lines = questions.map(
        (q, qi) => `${qi + 1}. ${q.question} → ${(picked[qi] ?? []).join("、")}`,
      );
      const summary = `你的选择：\n${lines.join("\n")}`;
      const showError = (err: unknown) => {
        console.error("[chat] multi choice submit failed:", err);
        const box = card.querySelector<HTMLElement>(".choice-answer");
        if (box) {
          box.textContent = `${box.textContent}（发送失败：${
            err instanceof Error ? err.message : String(err)
          }，请手动输入你的选择）`;
        }
      };
      if (e.wait && !forceMessage) {
        // 阻塞路径：逐题投递，后端收齐才唤醒等待方（202 = 已收下、未收齐）。
        finish(summary, () => {
          void (async () => {
            try {
              let delivered = true;
              for (let qi = 0; qi < questions.length; qi++) {
                const ans = (picked[qi] ?? []).join("、");
                const ok = await sendChoiceAnswer(e.choice_id, ans, questions[qi].id);
                if (!ok) {
                  delivered = false;
                  break;
                }
              }
              // 任何一题没送达（挂起项已消失 / 服务重启）→ 整批降级为
              // 普通 user 消息，语义对用户诚实，也不会只送一半。
              if (!delivered) await sendMessage(`我的选择：\n${lines.join("\n")}`);
            } catch (err) {
              showError(err);
            }
          })();
        });
      } else {
        // fire-and-forget：N 个答案合成**一条** user 消息，只起一个 turn。
        // 这正是修的那个问题 —— 不再是"每答一题就一个 turn"。
        finish(summary, () => {
          sendMessage(`我的选择：\n${lines.join("\n")}`).catch(showError);
        });
      }
    });

    if (archived) {
      submitBtn.disabled = true;
      const revive = document.createElement("button");
      revive.className = "choice-revive";
      revive.textContent = "仍要回答";
      revive.addEventListener("click", () => {
        forceMessage = true;
        card.classList.remove("answered");
        qsWrap.style.display = "";
        foot.style.display = "";
        revive.remove();
        refresh();
      });
      card.appendChild(revive);
    } else {
      refresh();
    }

    bubble.appendChild(card);
    // 与单题路径同一套队列登记：卡片在消息流与弹窗之间搬家，只有一份。
    if (archived) return;
    const hint = document.createElement("button");
    hint.type = "button";
    hint.className = "choice-moved-hint";
    hint.style.display = "none";
    hint.addEventListener("click", () => {
      if (entry) openChoiceModal(entry);
    });
    bubble.appendChild(hint);
    entry = { e, card, home: bubble, hint, answered: false, collapsed: false };
    choiceQueue.push(entry);
    pumpChoiceQueue();
  }

  function renderChoiceDialog(
    bubble: HTMLElement,
    e: ChoiceRequestedEvent,
    opts?: { archived?: boolean },
  ): void {
    // 多题弹框走独立渲染：一个卡片 N 道题、一个提交按钮，**全部答完**
    // 才能提交。单题形态（questions 空/缺省）继续走下面原来的路径，
    // 逐字不变。
    if ((e.questions?.length ?? 0) > 1) {
      renderMultiChoiceDialog(bubble, e, opts);
      return;
    }
    const archived = !!opts?.archived;
    // 存档卡片经「仍要回答」复活时置真：强制走普通 user 消息，
    // 不再尝试 choice 路由（原等待方已随进程一起消失）。
    let forceMessage = false;
    const multi = !!e.multi;
    const grid = e.layout === "grid";
    const card = document.createElement("div");
    card.className = "choice-card" + (grid ? " grid" : "") + (archived ? " answered" : "");
    card.dataset.choiceId = e.choice_id;
    /** 本卡片在队列里的登记项；建完卡片后赋值（finish 里闭包引用）。 */
    let entry: ChoiceEntry | null = null;

    // 卡片自带标题（弹窗里就是弹窗标题区下的问题正文）：单选/多选
    // 一眼可辨——用户此前根本看不出这道题能不能多选。
    const head = document.createElement("div");
    head.className = "choice-head";
    const chip = document.createElement("span");
    chip.className = "choice-chip" + (multi ? " multi" : "");
    chip.textContent = multi ? "多选 · 可勾选多项" : grid ? "单选 · 图片" : "单选";
    head.appendChild(chip);
    const questionEl = document.createElement("div");
    questionEl.className = "choice-question";
    questionEl.textContent = e.question;
    head.appendChild(questionEl);
    card.appendChild(head);

    const optsWrap = document.createElement("div");
    optsWrap.className = "choice-opts" + (grid ? " grid" : "");
    card.appendChild(optsWrap);

    // 选择状态：普通选项按 index，"其他" 特殊项，上传项。
    const selected = new Set<number>();
    let otherSelected = false;
    let uploaded: { path: string; name: string } | null = null;
    let otherText = "";

    const OTHER_IDX = e.options.length; // 伪索引：其他（自定义）

    const submitBtn = document.createElement("button");
    submitBtn.className = "choice-submit";
    submitBtn.textContent = "提交";
    submitBtn.disabled = true;

    const statusLine = document.createElement("div");
    statusLine.className = "choice-status";
    statusLine.textContent = "未选择";

    function answersText(): string[] {
      const out: string[] = [];
      for (const i of selected) {
        const o = e.options[i];
        out.push(o.image ? `${o.label}（图片：${o.image}）` : o.label);
      }
      if (otherSelected) out.push(otherText.trim() ? `其他：${otherText.trim()}` : "其他（未填）");
      if (uploaded) out.push(`上传图片：${uploaded.path}`);
      return out;
    }
    function refresh(): void {
      [...optsWrap.querySelectorAll(".choice-opt")].forEach((el) => {
        const idx = Number((el as HTMLElement).dataset.idx);
        const sel = idx === OTHER_IDX ? otherSelected : selected.has(idx);
        el.classList.toggle("sel", sel);
      });
      const ans = answersText();
      statusLine.textContent = ans.length ? "已选：" + ans.join("、") : multi ? "未选择（可勾选多项）" : "未选择";
      submitBtn.textContent = multi && ans.length > 1 ? `提交（${ans.length} 项）` : "提交";
      submitBtn.disabled = ans.length === 0;
    }
    function pick(idx: number): void {
      const isOther = idx === OTHER_IDX;
      if (multi) {
        if (isOther) otherSelected = !otherSelected;
        else if (selected.has(idx)) selected.delete(idx);
        else selected.add(idx);
      } else {
        selected.clear(); otherSelected = false; uploaded = null;
        if (isOther) otherSelected = true; else selected.add(idx);
      }
      refresh();
    }

    /** 优缺点/详情面板（「详情」按钮展开）。无内容时不建。 */
    const buildDetailPanel = (o: ChoiceOption): HTMLElement | null => {
      const pros = (o.pros ?? []).filter((s) => s.trim());
      const cons = (o.cons ?? []).filter((s) => s.trim());
      const extra = (o.details ?? "").trim();
      if (pros.length === 0 && cons.length === 0 && !extra) return null;
      const wrap = document.createElement("div");
      wrap.className = "choice-detail";
      wrap.hidden = true;
      const addList = (heading: string, items: string[], cls: string): void => {
        if (items.length === 0) return;
        const sec = document.createElement("div");
        sec.className = `choice-detail__sec ${cls}`;
        const h = document.createElement("div");
        h.className = "choice-detail__heading";
        h.textContent = heading;
        sec.appendChild(h);
        const ul = document.createElement("ul");
        ul.className = "choice-detail__list";
        for (const it of items) {
          const li = document.createElement("li");
          li.textContent = it.trim();
          ul.appendChild(li);
        }
        sec.appendChild(ul);
        wrap.appendChild(sec);
      };
      addList("✅ 优点", pros, "pros");
      addList("⚠️ 缺点 / 风险", cons, "cons");
      if (extra) {
        const p = document.createElement("div");
        p.className = "choice-detail__text";
        p.textContent = extra;
        wrap.appendChild(p);
      }
      return wrap;
    };

    // 渲染一个选项行/格。
    const buildOpt = (o: ChoiceOption | null, idx: number, isOther: boolean): HTMLElement => {
      const opt = document.createElement("div");
      opt.className = "choice-opt" + (multi ? " check" : " radio");
      opt.dataset.idx = String(idx);
      const mark = document.createElement("div");
      mark.className = "choice-mark";
      mark.textContent = multi ? "✓" : "●";
      opt.appendChild(mark);
      if (o?.image) {
        const img = document.createElement("img");
        img.className = grid ? "choice-pic" : "choice-thumb";
        img.src = o.image;
        img.alt = "";
        opt.appendChild(img);
      }
      const txt = document.createElement("div");
      txt.className = "choice-txt";
      const label = document.createElement("div");
      label.className = "choice-label";
      label.textContent = isOther ? "其他（自定义）" : (o?.label ?? "");
      if (o?.recommended) {
        const rec = document.createElement("span");
        rec.className = "choice-rec";
        rec.textContent = " ✓ 推荐";
        label.appendChild(rec);
      }
      txt.appendChild(label);
      if (o?.description) {
        const desc = document.createElement("div");
        desc.className = "choice-desc";
        desc.textContent = o.description;
        txt.appendChild(desc);
      }
      // 「详情」按钮：展开该方案的优缺点，不触发选中（stopPropagation）。
      const detail = o ? buildDetailPanel(o) : null;
      if (detail) {
        const btn = document.createElement("button");
        btn.type = "button";
        btn.className = "choice-detail-btn";
        btn.textContent = "详情 ▾";
        btn.title = "查看该方案的优缺点";
        btn.addEventListener("click", (ev) => {
          ev.stopPropagation();
          const show = detail.hidden;
          detail.hidden = !show;
          btn.textContent = show ? "收起 ▴" : "详情 ▾";
          btn.classList.toggle("open", show);
        });
        txt.appendChild(btn);
        txt.appendChild(detail);
      }
      if (isOther) {
        const inp = document.createElement("input");
        inp.className = "choice-other-input";
        inp.placeholder = "输入自定义内容…";
        inp.addEventListener("input", () => { otherText = inp.value; refresh(); });
        inp.addEventListener("click", (ev) => ev.stopPropagation());
        txt.appendChild(inp);
      }
      opt.appendChild(txt);
      opt.addEventListener("click", () => {
        pick(idx);
        if (isOther && otherSelected) setTimeout(() => opt.querySelector<HTMLInputElement>(".choice-other-input")?.focus(), 20);
      });
      return opt;
    };

    e.options.forEach((o, i) => optsWrap.appendChild(buildOpt(o, i, false)));
    // 始终附带「其他（自定义）」——与 oh-my-pi 约定一致，模型不需自己加。
    optsWrap.appendChild(buildOpt(null, OTHER_IDX, true));

    // 上传自定义图片。
    if (e.allow_upload) {
      const up = document.createElement("div");
      up.className = "choice-uploader";
      const prompt = document.createElement("div");
      prompt.className = "choice-up-prompt";
      prompt.textContent = "🖼️ 上传你自己的图片作为选择（点击选择）";
      up.appendChild(prompt);
      const fileInput = document.createElement("input");
      fileInput.type = "file";
      fileInput.accept = "image/*";
      fileInput.style.display = "none";
      up.appendChild(fileInput);
      const preview = document.createElement("img");
      preview.className = "choice-up-preview";
      preview.style.display = "none";
      up.appendChild(preview);
      up.addEventListener("click", () => fileInput.click());
      fileInput.addEventListener("change", async () => {
        const f = fileInput.files?.[0];
        if (!f) return;
        prompt.textContent = "上传中…";
        try {
          const res = await uploadImage(f);
          uploaded = { path: res.path, name: f.name };
          preview.src = res.path;
          preview.style.display = "block";
          prompt.textContent = `已上传：${f.name}（点此更换）`;
          up.classList.add("has-file");
          if (!multi) { selected.clear(); otherSelected = false; }
          refresh();
        } catch (err) {
          prompt.textContent = `上传失败：${err instanceof Error ? err.message : String(err)}`;
        }
      });
      card.appendChild(up);
    }

    const foot = document.createElement("div");
    foot.className = "choice-foot";
    foot.appendChild(statusLine);
    const skipBtn = document.createElement("button");
    skipBtn.className = "choice-skip";
    skipBtn.textContent = "跳过";
    foot.appendChild(skipBtn);
    foot.appendChild(submitBtn);
    card.appendChild(foot);

    function finish(summary: string, sendText: string | null): void {
      card.classList.add("answered");
      optsWrap.style.display = "none";
      foot.style.display = "none";
      card.querySelector(".choice-uploader")?.remove();
      const done = document.createElement("div");
      done.className = "choice-answer";
      done.textContent = summary;
      card.appendChild(done);
      // 弹框已处理 → 从后端补发表销账，否则下次重连/重放又被弹一遍。
      // 幂等且最佳努力：失败只 warn，不影响答案本身的回喂。
      void dismissPrompt(e.choice_id).catch((err) =>
        console.warn("[chat] prompt dismiss failed:", err),
      );
      // 关弹窗、卡片搬回消息流（answered 态留痕），弹下一个待答的。
      if (entry) settleChoice(entry);
      if (sendText === null) return;
      const showError = (err: unknown) => {
        console.error("[chat] choice submit failed:", err);
        done.textContent = `${summary}（发送失败：${err instanceof Error ? err.message : String(err)}，请手动输入你的选择）`;
      };
      if (e.wait && !forceMessage) {
        // 阻塞中的子代理在等这个答案：直达 choice 路由；挂起项已消失
        // （超时/服务重启）时降级为普通 user 消息回喂。
        sendChoiceAnswer(e.choice_id, sendText)
          .then((delivered) => (delivered ? Promise.resolve() : sendMessage(sendText)))
          .catch(showError);
      } else {
        // forceMessage = 从存档卡片「仍要回答」进来的：原提问方（子代理
        // 的等待 future）在重启时就没了，choice 路由必然 404，直接走
        // 普通消息，语义对用户也是诚实的。
        sendMessage(sendText).catch(showError);
      }
    }

    submitBtn.addEventListener("click", () => {
      const ans = answersText();
      if (ans.length === 0) return;
      const joined = ans.join("、");
      finish(`你的选择：${joined}`, `我的选择：${joined}`);
    });
    skipBtn.addEventListener("click", () => {
      finish("已跳过此选择。", "我先跳过这个选择，你按最合理的默认继续。");
    });

    // 存档态（history 重放出来的历史选择题）：只展示问过什么，不给
    // 可点的按钮。仍未处理的那条会由 pending-prompts 补拉成 live 事件
    // 覆盖掉这张卡（见 shouldSkipPrompt）。
    //
    // 但"不可点"不等于"没救"：服务重启后 PENDING/PROMPTS（内存表）
    // 都空了，弹框只剩历史里那一份，pending-prompts 补不出来 —— 而
    // 用户的选择本身仍有价值。所以给一个显式的「仍要回答」入口：点开
    // 才恢复选项，且强制走普通 user 消息（forceMessage）。
    // 不默认可点是因为历史里绝大多数是**已经答过**的题，给可点卡片会
    // 让用户误答一遍、发出一条莫名其妙的消息。
    if (archived) {
      optsWrap.style.display = "none";
      foot.style.display = "none";
      const note = document.createElement("div");
      note.className = "choice-answer";
      note.textContent = "（历史记录：这道选择题已不在待办中）";
      card.appendChild(note);
      const revive = document.createElement("button");
      revive.type = "button";
      revive.className = "choice-revive";
      revive.textContent = e.wait
        ? "仍要回答（原提问方已不在等待，将作为新消息发送）"
        : "仍要回答（作为新消息发送）";
      revive.addEventListener("click", () => {
        forceMessage = true;
        note.remove();
        revive.remove();
        card.classList.remove("answered");
        optsWrap.style.display = "";
        foot.style.display = "";
      });
      card.appendChild(revive);
    }

    bubble.appendChild(card);

    // 登记进弹窗队列。存档态（history 重放出来的历史选择题）不弹窗、
    // 不留铃——那是已经发生过的事，不该冒出个僵尸弹框要求用户回答。
    // 真正还在等的那条会经 pending-prompts 以 live 事件再来一次
    // （archived=false），那时才弹。
    if (archived) return;
    const hint = document.createElement("button");
    hint.type = "button";
    hint.className = "choice-moved-hint";
    hint.style.display = "none";
    hint.addEventListener("click", () => { if (entry) openChoiceModal(entry); });
    bubble.appendChild(hint);
    entry = { e, card, home: bubble, hint, answered: false, collapsed: false };
    choiceQueue.push(entry);
    pumpChoiceQueue();
  }

  // ── advisor 暂停拍板卡片 ──
  // advisor monitor 判 intervene 时后端广播 ChoiceRequested
  // （choice_id 以 "advisor-pause-" 开头）。此时 manager runner 被
  // advisor 暂停门 park 住，只有两种解除方式：POST resume-session
  // 或任意用户输入（含「终止」时后端会同时取消当前 turn）。所以
  // 不走通用 select+提交卡片，直接给两个一键按钮。
  function renderAdvisorPausePrompt(bubble: HTMLElement, e: ChoiceRequestedEvent): void {
    const card = document.createElement("div");
    card.className = "advisor-pause-card";

    const head = document.createElement("div");
    head.className = "advisor-pause-card__head";
    head.textContent = "🦉 advisor 介入：已暂停 manager 执行，等待你拍板";
    card.appendChild(head);

    const body = document.createElement("div");
    body.className = "advisor-pause-card__body";
    body.textContent = e.question;
    card.appendChild(body);

    const actions = document.createElement("div");
    actions.className = "advisor-pause-card__actions";
    const continueBtn = document.createElement("button");
    continueBtn.className = "advisor-pause-card__btn advisor-pause-card__btn--continue";
    continueBtn.textContent = "▶ 继续";
    continueBtn.title = "解除 advisor 暂停，workflow 继续执行";
    const abortBtn = document.createElement("button");
    abortBtn.className = "advisor-pause-card__btn advisor-pause-card__btn--abort";
    abortBtn.textContent = "⏹ 终止本轮";
    abortBtn.title = "解除 advisor 暂停并取消当前 turn";
    actions.appendChild(continueBtn);
    actions.appendChild(abortBtn);
    card.appendChild(actions);

    const statusLine = document.createElement("div");
    statusLine.className = "advisor-pause-card__status";
    card.appendChild(statusLine);

    function finish(summary: string): void {
      card.classList.add("answered");
      actions.style.display = "none";
      statusLine.textContent = summary;
      hideAdvisorPauseBanner();
    }
    function fail(summary: string, err: unknown): void {
      console.error(`[chat] advisor 拍板失败:`, err);
      statusLine.textContent = `${summary}：${err instanceof Error ? err.message : String(err)}（可重试）`;
      continueBtn.disabled = false;
      abortBtn.disabled = false;
    }

    continueBtn.addEventListener("click", async () => {
      continueBtn.disabled = true; abortBtn.disabled = true;
      try {
        await resumeSessionV2();
        finish("✅ 已拍板：继续执行");
      } catch (err) {
        fail("❌ 继续失败", err);
      }
    });
    abortBtn.addEventListener("click", async () => {
      continueBtn.disabled = true; abortBtn.disabled = true;
      try {
        await sendMessage("终止本轮");
        finish("🛑 已拍板：终止本轮");
      } catch (err) {
        fail("❌ 终止失败", err);
      }
    });

    bubble.appendChild(card);
  }
  // ── plan 导入弹窗 ──
  // PlanProposed 事件触发（主路径）或右键「导入任务看板」（补救路径）
  // 勾选 + 可编辑 title/description/priority 后调 importTasks 进 todo。
  function openPlanImportModal(tasks: ImportTask[], planId?: string): void {
    // 防重复弹窗：已存在则先移除再重建（同 planId 重开）。
    document.getElementById("plan-import-modal")?.remove();
    // 拆分会话（看板「拆分子任务」入口）的导入带 parent_id，成为父任务的子任务。
    const sid = getCurrentSessionId();
    const refineParent = sid ? refineParentFor(sid) : undefined;

    const overlay = document.createElement("div");
    overlay.id = "plan-import-modal";
    overlay.className = "plan-import-overlay";

    const box = document.createElement("div");
    box.className = "plan-import-box";
    const header = document.createElement("div");
    header.className = "plan-import-header";
    header.textContent = `导入任务看板 · ${tasks.length} 个候选${planId ? `（${planId}）` : ""}${refineParent ? `（作为 ${refineParent} 的子任务）` : ""}`;
    box.appendChild(header);

    // 每个任务一行：勾选框 + 可编辑 title/desc/priority；subtasks 任意
    // 深度递归成行（缩进表达层级），勾选父行会级联勾掉整个子树。
    type PlanRow = { cb: HTMLInputElement; titleInp: HTMLInputElement; descInp: HTMLTextAreaElement; priSel: HTMLSelectElement; task: ImportTask; subRows: PlanRow[] };
    const rows: PlanRow[] = [];
    const allRows: PlanRow[] = []; // 拍平的全部行，给「全选/全不选」用
    const list = document.createElement("div");
    list.className = "plan-import-list";
    const addRow = (t: ImportTask, depth: number): PlanRow => {
      const row = document.createElement("div");
      row.className = "plan-import-row";
      if (depth > 0) row.style.marginLeft = `${depth * 20}px`;
      const cb = document.createElement("input");
      cb.type = "checkbox";
      cb.checked = true;
      cb.className = "plan-import-cb";
      const titleInp = document.createElement("input");
      titleInp.type = "text";
      titleInp.value = t.title;
      titleInp.className = "plan-import-title";
      const descInp = document.createElement("textarea");
      descInp.value = t.description ?? "";
      descInp.rows = 2;
      descInp.className = "plan-import-desc";
      descInp.placeholder = "描述 + 验收标准";
      const priSel = document.createElement("select");
      for (const p of [1, 2, 3, 4]) {
        const o = document.createElement("option");
        o.value = String(p);
        o.textContent = `P${p}`;
        if (t.priority === p) o.selected = true;
        priSel.appendChild(o);
      }
      // 无 priority 时默认 P2。
      if (t.priority == null) priSel.value = "2";
      priSel.className = "plan-import-pri";
      const top = document.createElement("div");
      top.className = "plan-import-row-top";
      top.append(cb, titleInp, priSel);
      row.append(top, descInp);
      list.appendChild(row);
      const rec: PlanRow = { cb, titleInp, descInp, priSel, task: t, subRows: [] };
      // 勾选/取消父行级联到整个子树。
      cb.addEventListener("change", () => {
        const walk = (r: PlanRow): void => {
          r.cb.checked = cb.checked;
          r.subRows.forEach(walk);
        };
        rec.subRows.forEach(walk);
      });
      allRows.push(rec);
      for (const s of t.subtasks ?? []) rec.subRows.push(addRow(s, depth + 1));
      return rec;
    };
    for (const t of tasks) rows.push(addRow(t, 0));
    box.appendChild(list);

    // 父任务下拉框：默认「自动」（拆分会话由后端 refine 映射解析，
    // 页面刷新也不丢）；可人工改选任意任务作为父任务、或显式选「无
    // （根任务）」覆盖自动关联——防止父任务关联丢失时无法干预。
    const parentRow = document.createElement("div");
    parentRow.className = "plan-import-parent";
    const parentLbl = document.createElement("span");
    parentLbl.textContent = "父任务：";
    const parentSel = document.createElement("select");
    const autoOpt = document.createElement("option");
    autoOpt.value = "";
    autoOpt.textContent = refineParent ? `自动（拆分会话关联 → ${refineParent}）` : "自动（无关联则作为根任务）";
    parentSel.appendChild(autoOpt);
    const noneOpt = document.createElement("option");
    noneOpt.value = "none";
    noneOpt.textContent = "（无 — 作为根任务）";
    parentSel.appendChild(noneOpt);
    parentRow.append(parentLbl, parentSel);
    box.appendChild(parentRow);
    // 异步拉任务列表填充候选：任意层级都可作父任务，树形缩进帮助
    // 辨识层级；本地 refineParent 命中时预选。
    void listTasks()
      .catch(() => [] as TaskView[])
      .then((all) => {
        if (!document.getElementById("plan-import-modal")) return; // 弹窗已关
        for (const { task: t, depth } of parentOptions(all)) {
          const o = document.createElement("option");
          o.value = t.id;
          o.textContent = `${"　".repeat(depth)}${t.id} · ${t.title}`;
          parentSel.appendChild(o);
        }
        if (refineParent && [...parentSel.options].some(o => o.value === refineParent)) {
          parentSel.value = refineParent;
        }
      });

    // 底部操作栏：全选切换 + 导入 + 取消。
    const bar = document.createElement("div");
    bar.className = "plan-import-bar";
    const toggleAll = document.createElement("button");
    toggleAll.type = "button";
    toggleAll.textContent = "全选/全不选";
    toggleAll.addEventListener("click", () => {
      const all = allRows.every(r => r.cb.checked);
      allRows.forEach(r => { r.cb.checked = !all; });
    });
    const cancelBtn = document.createElement("button");
    cancelBtn.type = "button";
    cancelBtn.textContent = "取消";
    cancelBtn.addEventListener("click", () => overlay.remove());
    const importBtn = document.createElement("button");
    importBtn.type = "button";
    importBtn.className = "plan-import-go";
    importBtn.textContent = "导入选中项";
    bar.append(toggleAll, cancelBtn, importBtn);
    box.appendChild(bar);

    importBtn.addEventListener("click", async () => {
      // 勾选树递归收集：未勾选的行连同其子树一起不进导入清单。
      const toInput = (r: PlanRow): PlanImportRowInput => ({
        checked: r.cb.checked,
        title: r.titleInp.value,
        description: r.descInp.value,
        priority: Number(r.priSel.value),
        task: r.task,
        subRows: r.subRows.length > 0 ? r.subRows.map(toInput) : undefined,
      });
      const selected = buildSelectedPlanTasks(rows.map(toInput));
      if (selected.length === 0) { alert("未选择任何任务"); return; }
      importBtn.disabled = true;
      importBtn.textContent = "导入中…";
      try {
        // 父任务三态：具体 id = 人工指定；none = 显式根任务（发 "" 覆盖
        // 自动关联）；"" = 自动（不带 parent_id，后端按 session_id 查
        // refine 映射）。
        const pv = parentSel.value;
        const parentId = pv === "" ? undefined : pv === "none" ? "" : pv;
        const resp = await importTasks(selected, planId, parentId, sid ?? undefined);
        // 导入即进 todo：执行由用户到任务看板手动派发（不自动调度）。
        const effectiveParent = pv === "none" ? undefined : (parentId ?? refineParent);
        const content = effectiveParent
          ? `✅ 已导入 ${resp.created.length} 个子任务到 ${effectiveParent}（todo），请到任务看板派发执行`
          : `✅ 已导入 ${resp.created.length} 个任务到看板（todo），请到任务看板派发执行`;
        addMessage({ kind: "system", content });
        overlay.remove();
      } catch (err) {
        importBtn.disabled = false;
        importBtn.textContent = "导入选中项";
        alert(`导入失败: ${(err as Error).message}`);
      }
    });

    overlay.appendChild(box);
    // 点遮罩空白处关闭（补救路径下用户可随时重开）。
    overlay.addEventListener("click", (ev) => { if (ev.target === overlay) overlay.remove(); });
    document.body.appendChild(overlay);
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
  });

  // ── /command autocomplete ──
  // 内建命令固定；workflow 的斜杠命令（/plan /learn 等）启动时从
  // GET /api/workflows 拉取合并进来，让自定义 workflow 也能被补全。
  const cmdBox: HTMLElement = document.getElementById("cmd-autocomplete")!;
  const CMD_HINTS: CmdHint[] = [...BUILTIN_CMD_HINTS];
  // 拉取 workflow 命令并入提示；失败静默（不阻塞输入，仅补全减少）。
  listWorkflows()
    .then((workflows) => {
      const merged = mergeWorkflowCommands(CMD_HINTS, workflows);
      CMD_HINTS.splice(0, CMD_HINTS.length, ...merged);
    })
    .catch(() => {
      /* 拉取失败：仅使用内建命令，不打扰用户 */
    });
  let cmdIdx = -1;
  // ── @role autocomplete data ──
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
  // 统一 input handler：斜杠命令 / @角色 自动完成
  container.inputEl.addEventListener("input", function() {
    this.style.height = "auto";
    this.style.height = Math.min(this.scrollHeight, 140) + "px";
    const cursor = this.selectionStart;
    const before = this.value.substring(0, cursor);
    // /command 只匹配行首或空格后的 /xxx
    const slashMatch = before.match(/(?:^|\s)(\/[a-z]*)$/i);
    if (slashMatch) {
      const partial = slashMatch[1].toLowerCase();
      const entries = CMD_HINTS.filter(r => r.cmd.startsWith(partial));
      if (entries.length > 0) {
        cmdIdx = -1;
        cmdBox.innerHTML = entries.map((r,i) =>
          `<div class="cmd-autocomplete-item${i===0?' active':''}" data-cmd="${r.cmd}"><span class="ca-icon">${r.icon}</span><span class="ca-name">${r.cmd}</span><span class="ca-desc">${r.desc}</span></div>`
        ).join("");
        cmdBox.style.display = "block";
        cmdIdx = 0;
        acBox.style.display = "none";
      } else {
        cmdBox.style.display = "none";
      }
    } else {
      cmdBox.style.display = "none";
    }
    // @role 自动完成
    const roleMatch = before.match(/@([a-z_]*)$/i);
    if (roleMatch && cmdBox.style.display === "none") {
      const f = roleMatch[1].toLowerCase();
      const entries = ROLE_HINTS.filter(r => r.id.startsWith(f));
      if (entries.length > 0) {
        acIdx = -1;
        acBox.innerHTML = entries.map((r,i)=>`<div class="role-autocomplete-item${i===0?' active':''}" data-role="${r.id}"><span class="ra-icon">${r.icon}</span><span class="ra-name">@${r.id}</span><span class="ra-desc">${r.desc}</span></div>`).join("");
        acBox.style.display = "block";
        acIdx = 0;
      } else {
        acBox.style.display = "none";
      }
    } else {
      if (cmdBox.style.display === "none") acBox.style.display = "none";
    }
  });
  container.inputEl.addEventListener("keydown", function(e) {
    if (cmdBox.style.display !== "none") {
      const items = cmdBox.querySelectorAll(".cmd-autocomplete-item");
      if (e.key === "ArrowDown") { e.preventDefault(); cmdIdx = Math.min(cmdIdx+1, items.length-1); items.forEach((el,i)=>el.classList.toggle("active",i===cmdIdx)); return; }
      if (e.key === "ArrowUp") { e.preventDefault(); cmdIdx = Math.max(cmdIdx-1, 0); items.forEach((el,i)=>el.classList.toggle("active",i===cmdIdx)); return; }
      if (e.key === "Enter" || e.key === "Tab") { e.preventDefault(); const el=items[cmdIdx] as HTMLElement | undefined; if(el&&el.dataset){const ta=container.inputEl;const val=ta.value;const cursor=ta.selectionStart;const before=val.substring(0,cursor);const rest=val.substring(cursor);const m=before.match(/(^|\s)(\/[a-z]*)$/);if(m){const prefix=m[1];const cmd=(el.dataset as Record<string,string>).cmd||"";ta.value=prefix+cmd+" "+rest;const pos=prefix.length+cmd.length+1;ta.setSelectionRange(pos,pos);}cmdBox.style.display="none";ta.focus();} return; }
      if (e.key === "Escape") { cmdBox.style.display="none"; e.stopPropagation(); return; }
    }
    if (acBox.style.display !== "none") {
      const items = acBox.querySelectorAll(".role-autocomplete-item");
      if (e.key === "ArrowDown") { e.preventDefault(); acIdx = Math.min(acIdx+1, items.length-1); items.forEach((el,i)=>el.classList.toggle("active",i===acIdx)); return; }
      if (e.key === "ArrowUp") { e.preventDefault(); acIdx = Math.max(acIdx-1, 0); items.forEach((el,i)=>el.classList.toggle("active",i===acIdx)); return; }
      if (e.key === "Enter" || e.key === "Tab") { e.preventDefault(); const el=items[acIdx] as HTMLElement | undefined; if(el&&el.dataset){const ta=container.inputEl,pos=ta.selectionStart,b=ta.value.substring(0,pos),a=ta.value.substring(pos),ai=b.lastIndexOf("@");if(ai>=0){ta.value=b.substring(0,ai)+"@"+el.dataset.role+" "+a;acBox.style.display="none";ta.focus();}} return; }
      if (e.key === "Escape") { acBox.style.display="none"; e.stopPropagation(); return; }
    }
    if (e.key === "Enter" && !e.shiftKey && cmdBox.style.display === "none" && acBox.style.display === "none") { e.preventDefault(); container.sendBtn.click(); }
  });
  cmdBox.addEventListener("click", function(e) {
    const item = (e.target as HTMLElement).closest(".cmd-autocomplete-item") as HTMLElement | null;
    if (item && item.dataset) { const ta=container.inputEl;const val=ta.value;const cursor=ta.selectionStart;const before=val.substring(0,cursor);const rest=val.substring(cursor);const m=before.match(/(^|\s)(\/[a-z]*)$/);if(m){const prefix=m[1];const cmd=(item.dataset as Record<string,string>).cmd||"";ta.value=prefix+cmd+" "+rest;const pos=prefix.length+cmd.length+1;ta.setSelectionRange(pos,pos);}cmdBox.style.display="none";ta.focus(); }
  });
  acBox.addEventListener("click", function(e) {
    const item = (e.target as HTMLElement).closest(".role-autocomplete-item") as HTMLElement | null;
    if (item && item.dataset) { const ta=container.inputEl,pos=ta.selectionStart,b=ta.value.substring(0,pos),a=ta.value.substring(pos),ai=b.lastIndexOf("@");if(ai>=0){ta.value=b.substring(0,ai)+"@"+item.dataset.role+" "+a;acBox.style.display="none";ta.focus();} }
  });
  // 点击输入框/下拉框以外区域时关闭所有下拉
  document.addEventListener("mousedown", (e) => {
    const target = e.target as Node;
    if (cmdBox.style.display !== "none" && !cmdBox.contains(target) && target !== container.inputEl) {
      cmdBox.style.display = "none";
    }
    if (acBox.style.display !== "none" && !acBox.contains(target) && target !== container.inputEl) {
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
      if (contentEl) contentEl.innerHTML = renderMarkdown(v);
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

  // Two explicit buttons: 暂停 and 继续. Only one is active at a time —
  // when running you can pause, when paused you can resume. The
  // authoritative state is reconciled by the Paused/Resumed ChatEvents
  // (see the event handler below), so a click/event race still
  // converges to the correct enabled/disabled state.
  let isPaused = false;
  function renderPauseButtons() {
    container.pauseBtn.disabled = isPaused;
    container.resumeBtn.disabled = !isPaused;
  }
  renderPauseButtons();
  container.pauseBtn.addEventListener("click", async () => {
    if (container.pauseBtn.disabled) return;
    container.pauseBtn.disabled = true;
    try {
      // 全 session 冻结（对齐 oh-my-pi agentPauseGate）：turn / tool /
      // subagent 一起停，后端广播 Paused 驱动按钮复位。
      await pauseSessionV2();
    } catch (e) {
      console.error("[chat] pause failed:", e);
      container.pauseBtn.disabled = false; // re-enable on failure
    }
  });
  container.resumeBtn.addEventListener("click", async () => {
    if (container.resumeBtn.disabled) return;
    container.resumeBtn.disabled = true;
    try {
      await resumeSessionV2();
    } catch (e) {
      console.error("[chat] resume failed:", e);
      container.resumeBtn.disabled = false; // re-enable on failure
    }
  });
  container.roleSelect.addEventListener("change", async () => {
    const roleId = container.roleSelect.value;
    if (onRoleSwitch) { await onRoleSwitch(roleId); } else { await switchRole(roleId); }
    addMessage({ kind: "system", content: `→ switched to /role ${roleId}` });
    updateRolePauseToggle();
  });

  // ── Per-role pause (多角色 HIL) ──────────────────────────────
  // Roles individually paused via `/pause <role>`. Distinct from the
  // global 暂停/继续 buttons above. Driven by the RolePaused /
  // RoleResumed ChatEvents so a paused role is both visible (⏸ marker
  // in the role dropdown + toggle button state) and controllable.
  const pausedRoles = new Set<string>();
  // Refresh the dropdown option labels: paused roles get a ⏸ prefix.
  // Each option stores its base label in dataset.baseLabel so markers
  // don't accumulate across re-renders.
  function applyPausedMarkers(): void {
    for (const opt of Array.from(container.roleSelect.options)) {
      const base = opt.dataset.baseLabel ?? opt.textContent ?? opt.value;
      opt.dataset.baseLabel = base;
      opt.textContent = pausedRoles.has(opt.value) ? `⏸ ${base}` : base;
    }
  }
  // Reflect the *selected* role's paused state on the toggle button.
  function updateRolePauseToggle(): void {
    const roleId = container.roleSelect.value;
    const paused = pausedRoles.has(roleId);
    container.rolePauseToggle.textContent = paused ? "▶ 角色" : "⏸ 角色";
    container.rolePauseToggle.title = paused
      ? `恢复角色 ${roleId}：重新参与每轮`
      : `暂停角色 ${roleId}：该角色在每轮里被跳过，其余角色照常（多角色 HIL）`;
    container.rolePauseToggle.classList.toggle("active", paused);
  }
  // Apply an incoming RolePaused / RoleResumed transition.
  function setRolePaused(roleId: string, paused: boolean): void {
    if (paused) pausedRoles.add(roleId); else pausedRoles.delete(roleId);
    applyPausedMarkers();
    updateRolePauseToggle();
  }
  // Toggle a single role's paused state through the dedicated per-role
  // endpoints (`pauseRole` / `resumeRole`). Shared by the toolbar toggle
  // button and the avatar right-click menu. The optimistic ⏸ marker is
  // driven by the RolePaused / RoleResumed ChatEvent the backend echoes.
  async function toggleRolePause(roleId: string): Promise<void> {
    if (!roleId) return;
    const willPause = !pausedRoles.has(roleId);
    try {
      addMessage({ kind: "system", content: `→ ${willPause ? "暂停" : "恢复"}角色 ${roleId}` });
      if (willPause) { await pauseRole(roleId); } else { await resumeRole(roleId); }
    } catch (e) {
      console.error("[chat] role pause toggle failed:", e);
    }
  }
  container.rolePauseToggle.addEventListener("click", async () => {
    const roleId = container.roleSelect.value;
    if (!roleId) return;
    container.rolePauseToggle.disabled = true;
    try {
      await toggleRolePause(roleId);
    } finally {
      container.rolePauseToggle.disabled = false;
    }
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

  // ── 弹框去重表 ──
  // 弹框类事件（ChoiceRequested / PlanProposed）现在有三条到达路径：
  //   1. 实时 SSE；
  //   2. history 重放（`clear() + replayEvents`，切 session / 刷新 /
  //      broadcast lag 补齐都走它）；
  //   3. 挂起弹框补拉（新建 SSE 连接的 replay 前缀，以及
  //      `GET /api/chat/pending-prompts`）。
  // 同一个 choice_id / plan_id 会被送来多次，必须按 id 收敛，否则
  // 同一个问题渲染成好几张卡片。
  //
  // `live` 区分「历史里那条（已经发生过，可能早就答完了）」和「后端
  // 确认仍未处理的那条」：历史那张渲染成不可交互的存档卡片；补拉那
  // 张是真的还等着用户 → 替换掉存档卡片，恢复可交互。
  const promptRows = new Map<string, { row: HTMLElement; live: boolean }>();

  /** 弹框去重第一步：本条是否应跳过（已有等价或更权威的卡片）。
   *  非 replay 的重复（= 后端确认仍挂起）会先摘掉旧的存档卡片，
   *  让调用方重新渲染一张可交互的。 */
  function shouldSkipPrompt(id: string): boolean {
    const prev = promptRows.get(id);
    if (!prev) return false;
    // 已有 live 卡片 → 补拉/重放都不再渲染；
    // 只有存档卡片但本条也是重放 → 同样跳过。
    if (prev.live || replaying) return true;
    // 存档卡片 + 实时/补拉事件 → 摘掉，由调用方渲染可交互的那张。
    prev.row.remove();
    promptRows.delete(id);
    return false;
  }

  /** 弹框去重第二步：登记这条弹框渲染出的消息行。 */
  function registerPromptRow(id: string, row: HTMLElement): void {
    promptRows.set(id, { row, live: !replaying });
  }

  /** 弹框卡片的挂载点。
   *
   *  system 行没有 `.msg-bubble`（addMessage 非 avatar 分支只建
   *  `.message.system`），所以首选 `.message.system`。原来这里是裸的
   *  `querySelector(...)` + `if (bubble)`：选择器一旦对不上（e0bd456
   *  的回归就是查了 `.msg-bubble`），事件到了 UI 却静默不渲染，没有
   *  任何线索。现在逐级回退到 `.msg-bubble` 再到消息行本身，并在回退
   *  时报错——宁可卡片位置不好看，也不能让弹框凭空消失。 */
  function promptHost(msg: HTMLElement, promptId: string): HTMLElement {
    const host = msg.querySelector(".message.system") as HTMLElement | null;
    if (host) return host;
    const fallback = msg.querySelector(".msg-bubble") as HTMLElement | null;
    console.error(
      `[chat] prompt ${promptId}: 找不到 .message.system 挂载点，回退到 ${fallback ? ".msg-bubble" : "消息行"}`,
    );
    return fallback ?? msg;
  }

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
  function makeSubsessionBtn(subId: string, label: string, anchor?: HTMLElement): HTMLElement {
    const lnk = document.createElement("span");
    lnk.className = "subsession-link";
    lnk.textContent = "📋 详情";
    lnk.addEventListener("click", (e) => { e.stopPropagation(); onShowSubsession?.(subId, label, anchor); });
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

  // 右键角色头像 → 弹出小菜单：暂停/恢复该角色（多角色 HIL）+
  // 编辑角色。头像 class 形如 `msg-avatar <roleId>`（用户自己的是
  // `msg-avatar self-avatar`，忽略）。菜单是一个独立浮层，与消息
  // 右键菜单（#contextMenu）互不干扰。
  let avatarMenu: HTMLElement | null = null;
  function closeAvatarMenu(): void {
    if (avatarMenu) { avatarMenu.remove(); avatarMenu = null; }
  }
  container.messagesEl.addEventListener("contextmenu", (e) => {
    const avatar = (e.target as HTMLElement).closest(".msg-avatar") as HTMLElement | null;
    if (!avatar) return;
    const roleId = avatar.classList[1];
    if (!roleId || roleId === "self-avatar") return;
    e.preventDefault();
    closeAvatarMenu();
    const menu = document.createElement("div");
    menu.className = "context-menu avatar-menu";
    menu.style.display = "block";
    const paused = pausedRoles.has(roleId);
    const pauseItem = document.createElement("div");
    pauseItem.className = "menu-item";
    pauseItem.textContent = paused ? `▶ 恢复角色「${roleId}」` : `⏸ 暂停角色「${roleId}」`;
    pauseItem.title = paused
      ? `恢复角色 ${roleId}：重新参与每轮`
      : `暂停角色 ${roleId}：该角色在每轮里被跳过，其余角色照常（多角色 HIL）`;
    pauseItem.addEventListener("click", () => { closeAvatarMenu(); void toggleRolePause(roleId); });
    menu.appendChild(pauseItem);
    if (onEditRole) {
      const editItem = document.createElement("div");
      editItem.className = "menu-item";
      editItem.textContent = "✎ 编辑角色";
      editItem.addEventListener("click", () => { closeAvatarMenu(); onEditRole!(roleId); });
      menu.appendChild(editItem);
    }
    document.body.appendChild(menu);
    menu.style.left = Math.min(e.clientX, window.innerWidth - 200) + "px";
    menu.style.top = Math.min(e.clientY, window.innerHeight - 100) + "px";
    avatarMenu = menu;
  });
  document.addEventListener("click", (e) => {
    if (avatarMenu && !avatarMenu.contains(e.target as Node)) closeAvatarMenu();
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
      } else if (act === "view-execution-log" && hasSubId) {
        // Subagent rows use the dedicated subsession action; all top-level
        // messages, including advisor RoleTurn bubbles, use session history.
        el.style.display = "none";
      } else if (act === "terminate-subagent") {
        // 只对**还在跑**的分派显示：activeDelegates 由 DelegateStarted
        // 装入、DelegateFinished 摘除，所以它就是"这条还活着吗"。
        // 已结束的分派显示终止项没有意义（点了只会拿到 404）。
        const liveSubId = row.dataset.subId;
        el.style.display =
          liveSubId && activeDelegates.has(liveSubId) ? "" : "none";
      } else if (act === "add-to-board" && !(record?.planTasks && record.planTasks.length > 0)) {
        // 仅 plan 工具产出的消息（带 planTasks）显示「导入任务看板」。
        el.style.display = "none";
      } else if (act === "fork" && (!onFork || row.dataset.eventSeq === undefined)) {
        // 「从此处分叉」仅在有 fork 回调、且该行能映射到事件流位置
        // 时显示（合成的本地系统行没有 eventSeq，不可分叉）。
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
          onShowSubsession(subId, label, record.el);
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
          void onShowSessionLog(record.el);
        } else {
          alert("查看本次执行日志回调未注册");
        }
        break;
      }
      case "terminate-subagent": {
        // 只终止这一条分派，同一并行波里的兄弟分派继续跑。
        // 后端 404（分派刚好跑完）不当错误处理——如实告诉用户。
        const subId = record.el.dataset.subId;
        if (!subId) {
          alert("该消息没有关联的分派");
          break;
        }
        const who = record.meta ? `${record.meta}` : "该分派";
        // 文案要说实话：流程内分派（带 wf_id）停掉会结束整次 workflow
        // 运行——下游步骤依赖它的产出，没法接着跑。独立委派才是真的
        // 只停一条。两种情况都保留 session，且不会被自动重启。
        const inWorkflow = !!activeDelegates.get(subId)?.wfId;
        const scope = inWorkflow
          ? "这会结束本次 workflow 运行（下游步骤依赖它的产出）。已完成的步骤保留在 checkpoint，可稍后手动续跑。"
          : "只结束这一条委派，其它并行委派继续跑。";
        if (!confirm(`终止分派「${who}」？\n\n${scope}\n未完成的产出会丢失。`)) {
          break;
        }
        void cancelSubagent(subId).then((ok) => {
          if (!ok) {
            addMessage({
              kind: "system",
              content: `⏹ 分派「${who}」已经结束了，无需终止。`,
            });
          }
        });
        break;
      }
      case "add-to-board": {
        // 补救路径：从消息记录上存的结构化 planTasks 重开导入弹窗
        // （不靠文本解析）。仅带 planTasks 的消息有此入口。
        if (record.planTasks && record.planTasks.length > 0) {
          openPlanImportModal(record.planTasks, record.planId);
        } else {
          alert("该消息没有可导入的任务候选");
        }
        break;
      }
      case "fork": {
        // 从被点的这条消息处分叉：把事件流前缀（含该事件）交给
        // main.ts 去建新 session 并切过去。
        const seqRaw = record.el.dataset.eventSeq;
        const seq = seqRaw === undefined ? -1 : Number(seqRaw);
        if (!onFork || !Number.isInteger(seq) || seq < 0) {
          alert("该消息无法作为分叉点");
          break;
        }
        const prefix = allEvents.slice(0, seq + 1);
        void onFork(prefix);
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
  // 后端每跑过 soft_timeout_secs 就推一次 `TimeoutWarning`（周期复发，
  // 不再是只发一次后静默），我们按 sub_id 维度各挂一条「继续等待 /
  // 终止」询问条到置顶容器里。分派自然结束（RoleFinished / Error /
  // DelegateFinished）时移除对应那条。
  //
  // 后端已无硬超时（hard_timeout_secs=0）：不会强杀，全靠用户拍板或
  // run_turn 内部的工具轮次上限兜底。

  /** 询问条容器：按需创建并置顶插入。 */
  function ensureTimeoutStack(): HTMLElement {
    if (timeoutStackEl && timeoutStackEl.isConnected) return timeoutStackEl;
    const el = document.createElement("div");
    el.className = "timeout-prompt-stack";
    container.messagesEl.insertBefore(el, container.messagesEl.firstChild);
    timeoutStackEl = el;
    return el;
  }

  /** 容器空了就撤掉，避免留一个空 div 占位。 */
  function pruneTimeoutStack(): void {
    if (timeoutStackEl && timeoutStackEl.childElementCount === 0) {
      timeoutStackEl.remove();
      timeoutStackEl = null;
    }
  }

  /** 移除某条询问条。`key` 是 `sub_id ?? role_id`。 */
  function hideTimeoutPromptByKey(key: string): void {
    const el = timeoutPrompts.get(key);
    if (el) {
      el.remove();
      timeoutPrompts.delete(key);
      pruneTimeoutStack();
    }
  }

  /** 分派结束时的清理入口。
   *
   *  带 sub_id 就精确移除那一条；不带（主 turn 事件、旧归档回放）则
   *  清掉该 role 名下的全部询问条 —— 否则并行波里某条没有 sub_id 的
   *  事件会漏清，留下永远不消失的僵尸提示。 */
  function hideTimeoutPrompt(roleId: string, subId?: string | null): void {
    if (subId) {
      hideTimeoutPromptByKey(subId);
      return;
    }
    hideTimeoutPromptByKey(roleId);
    for (const [key, el] of [...timeoutPrompts]) {
      if (el.dataset.roleId === roleId) {
        el.remove();
        timeoutPrompts.delete(key);
      }
    }
    pruneTimeoutStack();
  }

  /** 清掉全部询问条（切 session / clear / replay 收尾）。 */
  function hideAllTimeoutPrompts(): void {
    for (const el of timeoutPrompts.values()) el.remove();
    timeoutPrompts.clear();
    pruneTimeoutStack();
  }

  // ── advisor 暂停横幅 ──────────────────────────────────────────
  // advisor 暂停不发 Paused 事件（不同于用户手动 ⏸），所以单独维护
  // 一条置顶横幅提示「已暂停，等待拍板」。由 advisor-pause 的
  // ChoiceRequested 挂上，拍板（renderAdvisorPausePrompt.finish）或
  // 收到 Resumed 事件后清除。
  function hideAdvisorPauseBanner(): void {
    if (advisorPauseBannerEl) {
      advisorPauseBannerEl.remove();
      advisorPauseBannerEl = null;
    }
  }
  function showAdvisorPauseBanner(): void {
    hideAdvisorPauseBanner();
    const node = document.createElement("div");
    node.className = "timeout-prompt advisor-pause-banner";
    node.innerHTML = `
      <div class="timeout-prompt__head">
        <span class="timeout-prompt__icon">🦉</span>
        <strong>advisor 已暂停 manager 执行，等待你拍板</strong>
      </div>
      <div class="timeout-prompt__body">
        请在下方 advisor 介入卡片里点「▶ 继续」或「⏹ 终止本轮」；直接输入消息也会解除暂停。
      </div>
    `;
    container.messagesEl.insertBefore(node, container.messagesEl.firstChild);
    advisorPauseBannerEl = node;
  }
  function showTimeoutPrompt(ev: Extract<ChatEvent, { type: "TimeoutWarning" }>): void {
    // key 用 sub_id（唯一）优先，主 turn 无 sub_id 时退回 role_id。
    // 同 key 复发 → 原地更新已跑秒数，不新增一条（后端按 soft 周期
    // 重发，否则同一分派会堆出一叠提示）。
    const key = ev.sub_id ?? ev.role_id;
    const isDispatch = !!ev.sub_id;
    const existing = timeoutPrompts.get(key);
    if (existing) {
      const head = existing.querySelector(".timeout-prompt__elapsed");
      if (head) head.textContent = `${ev.elapsed_secs}s`;
      return;
    }

    const node = document.createElement("div");
    node.className = "timeout-prompt";
    node.dataset.roleId = ev.role_id;
    if (ev.sub_id) node.dataset.subId = ev.sub_id;
    // 并行波里同时挂多条，标题必须能区分是哪一条分派 —— 带上
    // workflow 名（有则显示），否则用户看不出该终止哪个。
    const wfName = isDispatch
      ? (activeDelegates.get(ev.sub_id!)?.wfId
          ? wfNames.get(activeDelegates.get(ev.sub_id!)!.wfId!) ?? "workflow"
          : "")
      : "";
    const scopeTag = wfName ? `<span class="timeout-prompt__tag">🔀 ${wfName}</span>` : "";
    // 终止按钮的语义随粒度变：分派级只掐这一条（复用右键终止的
    // per-subsession 通道），主 turn 级才是整轮取消。
    const cancelLabel = isDispatch ? "终止此分派" : "终止当前任务";
    node.innerHTML = `
      <div class="timeout-prompt__head">
        <span class="timeout-prompt__icon">⏳</span>
        <strong>${roleIcon(ev.role_id)} ${ev.role_id} 已运行 <span class="timeout-prompt__elapsed">${ev.elapsed_secs}s</span></strong>
        ${scopeTag}
      </div>
      <div class="timeout-prompt__body">
        超过设定的 ${ev.soft_timeout_secs}s 软超时${ev.hard_timeout_secs > 0 ? `（硬超时将在 ${ev.hard_timeout_secs}s 强制终止）` : "（无硬超时，不会被强杀；可继续等待或手动终止）"}。
        是否继续等待？
      </div>
      <div class="timeout-prompt__actions">
        <button class="timeout-prompt__btn timeout-prompt__btn--continue" data-act="continue">继续等待</button>
        <button class="timeout-prompt__btn timeout-prompt__btn--cancel" data-act="cancel">${cancelLabel}</button>
      </div>
    `;
    // 「继续等待」只收掉这一条，不动后端（分派继续跑）。后端会在下
    // 一个 soft 周期再提醒一次。
    node.querySelector('[data-act="continue"]')!.addEventListener("click", () => {
      hideTimeoutPromptByKey(key);
    });
    node.querySelector('[data-act="cancel"]')!.addEventListener("click", () => {
      hideTimeoutPromptByKey(key);
      if (isDispatch) {
        // per-subsession：只掐这一条，不牵连同波兄弟，也不会被
        // manager 自动 resume（后端带 CANCELLED_BY_USER 标记）。
        void cancelSubagent(ev.sub_id!).then((ok) => {
          if (!ok) {
            addMessage({
              kind: "system",
              content: `⏹ 分派「${ev.role_id}」已经结束了，无需终止。`,
            });
          }
        });
      } else {
        void cancelTurn().catch((err) => {
          console.error("[chat] cancelTurn failed", err);
        });
      }
    });
    ensureTimeoutStack().appendChild(node);
    timeoutPrompts.set(key, node);
  }
  function handleEvent(e: ChatEvent): void {
    const identity = eventIdentity(e);
    if (identity && identity === lastEventIdentity) return;
    lastEventIdentity = identity;
    allEvents.push(e);
    currentEventIdx = allEvents.length - 1;
    try {
      dispatchEvent(e);
    } finally {
      currentEventIdx = -1;
    }
  }
  function dispatchEvent(e: ChatEvent): void {
    switch (e.type) {
      case "RoleStarted": {
        subagentTools = [];
        const startedSubId = e.sub_id || findDelegateSubByRole(e.role_id) || currentDelegateSubId || undefined;
        // A missing terminal (SSE loss, model error, cancelled outer future,
        // or replay ending on a partial) must never make this new turn append
        // to the previous turn's partial bubble.
        clearStream(e.role_id, e.sub_id);
        const isDelegate = !!startedSubId;
        const node = addMessage({
          kind: "status",
          content: `🧠 ${e.role_id} 开始执行…`,
          meta: e.role_id,
          subId: startedSubId,
          state: "executing",
        });
        if (isDelegate) {
          node.classList.add("is-delegate-role");
          node.dataset.delegate = "true";
          if (startedSubId) {
            const detail = document.createElement("span");
            detail.className = "subsession-link";
            detail.textContent = "📋 详情";
            detail.addEventListener("click", (event) => {
              event.stopPropagation();
              onShowSubsession?.(startedSubId, `${roleIcon(e.role_id)} ${e.role_id}`, node);
            });
            node.querySelector(".message.status")?.appendChild(detail);
          }
        }
        pushExecutingRow(e.role_id, node, startedSubId ?? "");
        lastRoleStarted = e.role_id;
        break;
      }
      case "RoleFinished": {
        clearStream(e.role_id, e.sub_id);
        const entry = takeExecutingRow(e.role_id, e.sub_id);
        if (entry) {
          const inner = entry.row.querySelector(".message.status") as HTMLElement | null;
          if (inner) {
            inner.classList.remove("executing");
            inner.classList.add("done");
            const content = inner.querySelector(".content");
            if (content) content.textContent = `✅ ${e.role_id} 完成`;
          }
        } else {
          addMessage({ kind: "status", content: `✅ ${e.role_id} 完成`, meta: e.role_id, state: "done" });
        }
        hideTimeoutPrompt(e.role_id, e.sub_id);
        break;
      }
      case "RolePaused": {
        setRolePaused(e.role_id, true);
        addMessage({ kind: "system", content: `⏸ 角色已暂停：${e.role_id}` });
        break;
      }
      case "RoleResumed": {
        setRolePaused(e.role_id, false);
        addMessage({ kind: "system", content: `▶ 角色已恢复：${e.role_id}` });
        break;
      }
      case "UserMessage": {
        // 后端回显的用户消息（发送、回放同一路径，UI 不做本地乐观渲染）。
        const node = addMessage({ kind: "user", content: e.text });
        lastUserMsgId = node.dataset.messageId || "";
        break;
      }
      case "ImageGenerated": {
        // generate_image 工具产出：角色气泡 + 图片 + 截断的 prompt 说明。
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
      case "ToolUse": {
        const t = truncate(e.args, 100);
        const line = `🔧 ${e.tool_name}(${t})`;
        subagentTools.push(line);
        currentToolCall = `🔧 ${e.tool_name}`; currentActivity = `正在调用 ${e.tool_name}…`;
        updateFooter(); updateStatusPillLabel(`🔧 ${resolveIcon(e.role_id)} ${e.tool_name}`); resetWaitTimer();
        if (e.sub_id) {
          // Subagent tools are persisted in the subsession trace. Keep them
          // out of the top-level chat; the standard Subsession panel renders
          // the complete event stream when the user opens Details.
          // 但主对话必须有「还活着」的信号 —— 只更新徽章/状态行，不刷正文。
          bumpDelegateHeartbeat(e.sub_id, e.role_id, e.tool_name);
          break;
        }
        if (!appendToolToExecutingRow(e.role_id, line)) {
          const toolRow = addMessage({ kind: "tool", content: `${e.tool_name} ${t}`, meta: e.role_id, icon: resolveIcon(e.role_id), filePath: getFilePath(e.role_id) });
          const useChips = makeRefChips(extractCodeRefs(e.tool_name, e.args));
          if (useChips) toolRow.querySelector(".msg-bubble")?.appendChild(useChips);
        }
        break;
      }
      case "ToolResult": {
        const short = truncate(e.result, 80);
        const resultLine = e.result.toLowerCase().startsWith("error")
          ? `❌ ${e.tool_name} → ${short}`
          : `✅ ${e.tool_name} → ${short}`;
        subagentTools.push(resultLine);
        updateFooter(); updateStatusPillLabel(`${currentRoleIcon || resolveIcon(e.role_id)} 处理中…`); resetWaitTimer();
        if (e.sub_id) {
          // Render through the same standard subsession view as every other
          // delegate, rather than the legacy live overlay.
          break;
        }
        if (!appendToolToExecutingRow(e.role_id, resultLine)) {
          const resultRow = addMessage({ kind: "tool", content: `${e.tool_name} → ${truncate(e.result, 200)}`, meta: e.role_id, icon: resolveIcon(e.role_id), filePath: getFilePath(e.role_id) });
          const resultChips = makeRefChips(extractCodeRefs(e.tool_name, "", e.result));
          if (resultChips) resultRow.querySelector(".msg-bubble")?.appendChild(resultChips);
        }
        break;
      }
      case "ToolError": {
        const line = `❌ ${e.tool_name}: ${truncate(e.error, 120)}`;
        subagentTools.push(line);
        updateFooter(); resetWaitTimer();
        if (e.sub_id) {
          // The standard subsession view owns subagent tool errors too.
          break;
        }
        if (!appendToolToExecutingRow(e.role_id, line)) {
          console.warn("[chat] late ToolError (no executing row):", e.tool_name, e.error);
          setFooter(`❌ ${e.tool_name}: ${truncate(e.error, 80)}`);
        }
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
        const ref = di
          ? { refId: di.delegateMsgId, preview: `@${di.targetRole}: ${di.taskText.slice(0, 30)}` }
          : (lastUserMsgId && e.role_id === "manager" && e.is_complete
            ? (() => {
                const userMsg = getMsgById(lastUserMsgId);
                return { refId: lastUserMsgId, preview: (userMsg?.content || "用户消息").slice(0, 30) };
              })()
            : undefined);

        // ── 流式增量渲染 ──
        // `is_complete:false` 的事件携带模型逐 token 生成的增量片段。
        // 首个 delta 创建气泡，后续 delta 追加到同一气泡。
        //
        // key 必须是 `role_id + sub_id` 而不是裸 role_id：DAG 并行波里
        // 两个分派可以命中同一个 role（或分派 role 与主角色相同），只按
        // role 索引会让两路 token 交错写进同一个气泡，正文混成一团。
        // （后端此前还把 delta 的 sub_id 写死成 None，两个坑叠在一起；
        // 见 controller.rs 的 ModelDelta 分支。）
        const key = streamKey(e.role_id, e.sub_id);
        const streamRow = streamingEl.get(key);
        if (e.is_complete) {
          // 终态：若之前有流式气泡，把终态 content 追加进去/结束；否则整段加新泡。
          if (streamRow) {
            const contentEl = streamRow.querySelector<HTMLElement>(".msg-content");
            if (contentEl) {
              // 终态 content 是完整文本——直接用 innerHTML 替换（避免与
              // 已追加的 delta 拼接误差）。仅当 content 与已显示不一致时。
              contentEl.innerHTML = renderMarkdown(e.content);
            }
            streamingEl.delete(key);
            streamingRaw.delete(key);
          } else {
            addMessage({ kind: "role", content: e.content, meta: e.role_id, icon, subagent, subId: roleSubId, reference: ref, filePath: getFilePath(e.role_id) });
          }
          currentToolCall = ""; currentActivity = ""; updateFooter();
          setStatus("connected"); clearWaitTimer();
          if (di) {
            forgetDelegate(subId);
            // 分派结束只更新角色徽标显示，**不**再自动发
            // switchRole("manager")：那是一次真实的 HTTP 调用，会把
            // 服务端当前角色改掉 —— 用户手动切到别的角色后，一个分派
            // 完成就把他的选择静默改回 manager（服务端还会重建 runner）。
            // 分派回到 manager 是后端编排的内部事实，不该反向覆盖用户
            // 在 UI 上的显式选择。
            if (activeDelegates.size === 0) {
              container.rolePill.textContent = `${roleIcon("manager")} manager`;
            }
          }
        } else {
          // 增量 delta：已有流式气泡则累积重渲染，否则新建。
          if (streamRow) {
            const contentEl = streamRow.querySelector<HTMLElement>(".msg-content");
            const raw = (streamingRaw.get(key) ?? "") + e.content;
            streamingRaw.set(key, raw);
            if (contentEl) contentEl.innerHTML = renderMarkdown(raw);
          } else {
            const row = addMessage({ kind: "role", content: e.content, meta: e.role_id, icon, subagent, subId: roleSubId, reference: undefined, filePath: getFilePath(e.role_id) });
            streamingEl.set(key, row);
            streamingRaw.set(key, e.content);
          }
          updateStatusPillLabel(`${icon} 模型输出中…`);
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
      case "Paused": isPaused = true; renderPauseButtons(); addMessage({ kind: "status", content: `[paused] ${e.reason}` }); break;
      case "Resumed": isPaused = false; renderPauseButtons(); hideAdvisorPauseBanner(); addMessage({ kind: "status", content: "[resumed]" }); break;
      case "RoundStarted":
        addMessage({ kind: "system", content: `[回合 ${e.round} 开始]` }); setFooter(`回合 ${e.round} 开始`);
        resetWaitTimer(); currentToolCall = ""; currentDelegate = ""; updateFooter(); break;
      case "RoundEnded": addMessage({ kind: "system", content: `[回合 ${e.round} 结束]` }); resetWaitTimer(); break;
      case "DelegateStarted": {
        const taskText = e.task.trim() || "(empty)";
        const wfName = e.wf_id ? (wfNames.get(e.wf_id) ?? "workflow") : "";
        const queued = e.wf_id ? workflowStepQueues.get(e.wf_id) : undefined;
        const step = queued?.find((candidate) => candidate.roleId === e.to_role);
        if (step && queued) {
          queued.splice(queued.indexOf(step), 1);
          workflowStepSubIds.set(step.key, e.sub_id);
        }
        const displayTask = step?.taskText ?? taskText;
        const delegateMsg = addMessage({
          kind: "role",
          content: `@${e.to_role} ${displayTask}`,
          meta: wfName ? `manager · 🔀 ${wfName}` : (e.from_role || "manager"),
          icon: roleIcon(e.from_role || "manager"),
          subId: e.sub_id,
          filePath: getFilePath(e.to_role),
        });
        const msgId = delegateMsg.dataset.messageId || "";
        if (step) workflowStepMsgIds.set(step.key, msgId);
        const stateEl = document.createElement("span");
        stateEl.className = "delegate-state pending";
        stateEl.textContent = "⏳ 执行中…";
        delegateMsg.querySelector(".msg-bubble")?.appendChild(stateEl);
        activeDelegates.set(e.sub_id, {
          targetRole: e.to_role,
          taskText: displayTask,
          delegateMsgId: msgId,
          capturedTools: [],
          stateEl,
          wfId: e.wf_id ?? undefined,
        });
        currentDelegateSubId = e.sub_id;
        subagentTools = [];
        delegateRunning = true; currentDelegate = `${e.from_role} → ${e.to_role}`;
        currentToolCall = ""; currentActivity = `等待 ${e.to_role}`;
        const icon = currentRoleIcon || "🧠";
        updateFooter(); updateStatusPillLabel(`⏳ ${icon} → ${e.to_role}`);
        container.rolePill.textContent = `⏳ ${roleIcon(e.to_role)} ${e.to_role}`;
        resetWaitTimer(); break;
      }
      case "DelegateFinished": {
        delegateRunning = false;
        const fi = roleIcon(e.from_role), ti = roleIcon(e.to_role);
        const wfTag = e.wf_id ? ` 🔀${wfNames.get(e.wf_id) ?? "workflow"}` : "";
        const label = `${fi} ${e.from_role} → ${ti} ${e.to_role}`;
        const isFail = e.status !== "ok";
        const kind = isFail ? "error" : "system";
        const prefix = isFail ? "❌" : "✅";
        const statusText = isFail ? `失败(${e.status})` : "ok";
        // summary 是专家返回/失败原因的正文，失败时尤其要看，拼进消息里。
        const summary = e.summary?.trim() ? `\n${truncate(e.summary, 300)}` : "";
        const msg = addMessage({ kind, content: `${prefix}${wfTag} ${fi}${e.from_role}←${ti}${e.to_role}(${statusText})${summary}`, subId: e.sub_id });
        msg.appendChild(makeSubsessionBtn(e.sub_id, label, msg));
        if (isFail) msg.classList.add("fail-flash");
        // Flip the pending badge on the DelegateStarted bubble.
        // 用 delegateBadge：历史回放清过 activeDelegates 的场景（刷新
        // 后 session 仍在跑）也能找回那枚 ⏳ 徽章。
        const finishedBadge = delegateBadge(e.sub_id);
        if (finishedBadge) {
          finishedBadge.textContent = isFail ? `❌ ${e.status}` : "✅ 完成";
          finishedBadge.className = `delegate-state ${isFail ? "failed" : "done"}`;
        }
        // 这条分派结束 → 收掉它的超时询问条。DelegateFinished 是分派
        // 结束最可靠的信号（RoleFinished 在某些路径上不带 sub_id），
        // 并行波里必须按 sub_id 精确移除，不能误清兄弟那条。
        hideTimeoutPromptByKey(e.sub_id);
        msg.addEventListener("contextmenu", (ev) => { ev.preventDefault(); onShowSubsession?.(e.sub_id, label, msg); });
        currentDelegate = ""; currentActivity = "";
        const icon = currentRoleIcon || resolveIcon(e.from_role);
        updateFooter(); updateStatusPillLabel(`${icon} 思考中…`);
        container.rolePill.textContent = `${icon} ${e.from_role}`;
        // Clean up delegate tracking and any partial stream that ended
        // without a complete RoleTurn.
        clearStream(e.to_role, e.sub_id);
        forgetDelegate(e.sub_id);
        resetWaitTimer(); break;
      }
      case "Done":
        clearAllStreams();
        retireAllDelegates("会话已终止");
        setStatus("connected"); clearWaitTimer(); addMessage({ kind: "system", content: "[会话结束]" }); setFooter("会话结束"); break;
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
        // 运行态控制按钮：暂停 / 继续（全 session 级）
        const pauseBtn = document.createElement("button");
        pauseBtn.className = "wf-run-control-btn";
        pauseBtn.textContent = "⏸ 暂停";
        pauseBtn.title = "暂停整个 session（workflow 会在下个 step/model 边界 park）";
        pauseBtn.addEventListener("click", async () => {
          pauseBtn.disabled = true;
          try {
            await pauseSessionV2();
            pauseBtn.textContent = "▶ 继续";
            pauseBtn.title = "恢复整个 session（workflow 继续）";
            pauseBtn.onclick = async () => {
              pauseBtn.disabled = true;
              try {
                await resumeSessionV2();
                pauseBtn.textContent = "⏸ 暂停";
                pauseBtn.title = "暂停整个 session";
                pauseBtn.onclick = null; // 恢复原事件
              } catch { pauseBtn.disabled = false; }
            };
          } catch { pauseBtn.disabled = false; }
        });
        msg.querySelector(".msg-bubble")?.appendChild(pauseBtn);
        workflowStates.set(e.wf_id, stateEl);
        wfNames.set(e.wf_id, e.name);
        setFooter(`workflow ${e.name} 运行中…`);
        resetWaitTimer();
        break;
      }
      case "WorkflowStep": {
        const key = `${e.wf_id}:${e.step_id}`;
        const queue = workflowStepQueues.get(e.wf_id) ?? [];
        queue.push({
          key,
          roleId: e.role_id,
          taskText: e.task?.trim() || e.description?.trim() || e.step_id,
        });
        workflowStepQueues.set(e.wf_id, queue);
        resetWaitTimer();
        break;
      }
      case "WorkflowTurn": {
        const key = `${e.wf_id}:${e.step_id}`;
        const dispatchId = workflowStepMsgIds.get(key);
        const reference = dispatchId
          ? { refId: dispatchId, preview: (getMsgById(dispatchId)?.content ?? "").slice(0, 60) }
          : undefined;
        addMessage({
          kind: "role",
          content: e.content,
          meta: `${e.role_id}（workflow）`,
          icon: roleIcon(e.role_id),
          reference,
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
        // JoinSet::abort_all can drop sibling step futures before they emit
        // DelegateFinished. The workflow terminal is authoritative for all
        // remaining delegates owned by this wf_id.
        retireWorkflowDelegates(e.wf_id, isFail ? e.status : "工作流已结束");
        const stateEl = workflowStates.get(e.wf_id);
        if (stateEl) {
          stateEl.textContent = isFail ? `❌ ${e.status}` : "✅ 完成";
          stateEl.className = `delegate-state ${isFail ? "failed" : "done"}`;
          workflowStates.delete(e.wf_id);
        }
        const summary = e.summary?.trim() ? `\n${truncate(e.summary, 300)}` : "";
        const msg = addMessage({
          kind: isFail ? "error" : "system",
          content: `${isFail ? "❌" : "✅"} 工作流「${e.name}」${isFail ? `失败(${e.status})` : "完成"}${summary}`,
        });
        // 失败时给一个「🔄 续跑」按钮：调后端 /api/workflows/resume
        // 从 checkpoint 续跑这条失败工作流。`wf_id` 显式带过去更确定
        // （若不传，后端反向扫 event_log 也行，但当前消息里的 wf_id
        // 就是失败那条本身，最准确）。点击后按钮禁用 + 反馈状态。
        if (isFail) {
          const resumeBtn = document.createElement("button");
          resumeBtn.className = "wf-resume-btn";
          resumeBtn.textContent = `🔄 续跑（wf_id=${e.wf_id}）`;
          resumeBtn.title = "从断点续跑这个失败的工作流（已完成步骤会自动跳过）";
          resumeBtn.addEventListener("click", async () => {
            const sid = getCurrentSessionId();
            if (!sid) {
              resumeBtn.textContent = "❌ 无 session_id";
              resumeBtn.disabled = true;
              return;
            }
            resumeBtn.disabled = true;
            const orig = resumeBtn.textContent;
            resumeBtn.textContent = "⏳ 续跑中…";
            try {
              const resp = await resumeWorkflow(sid, e.wf_id);
              resumeBtn.textContent = `✅ 已发起续跑（${resp.name}）`;
              // 续跑产生的 WorkflowStarted/Step/Turn/Finished 事件会经
              // 该 session 的 SSE 流回 → 自然出现在聊天面板，无需额外
              // 订阅。按钮保持禁用状态，避免重复点击。
            } catch (err) {
              resumeBtn.textContent = `❌ ${(err as Error).message}`;
              // 失败可重试：把按钮恢复成可点击。
              resumeBtn.disabled = false;
              setTimeout(() => { resumeBtn.textContent = orig; }, 4000);
            }
          });
          msg.querySelector(".msg-bubble")?.appendChild(resumeBtn);
        }
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
      case "PlanProposed": {
        // plan 工具提交的任务候选：渲染消息 + 导入按钮。弹窗 debounce：
        // 连续多个 plan 调用（如 LLM 先测试再提真实任务）只对最后一个弹窗
        //
        // 去重：同一 plan_id 可能同时经 history 重放和挂起弹框补拉到达。
        if (shouldSkipPrompt(e.plan_id)) break;
        const n = e.tasks.length;
        const msg = addMessage({
          kind: "system",
          content: `📋 ${e.role_id} 提交了 ${n} 个任务候选（${e.plan_id}），请在弹窗勾选导入任务看板`,
          planTasks: e.tasks,
          planId: e.plan_id,
        });
        registerPromptRow(e.plan_id, msg);
        // 气泡内附「导入任务看板」按钮，作为弹窗入口。
        const btn = document.createElement("button");
        btn.className = "workflow-import-btn";
        btn.textContent = `📥 导入任务看板（${n} 个）`;
        btn.addEventListener("click", () => openPlanImportModal(e.tasks, e.plan_id));
        msg.querySelector(".msg-bubble")?.appendChild(btn);
        setFooter(`${e.role_id} 提交 ${n} 个任务候选`);
        resetWaitTimer();
        // 重放的历史 plan 不自动弹窗：它可能早就导入过 / 用户已经放弃，
        // 每次刷新都糊一个旧清单的模态框上来纯属骚扰。真正还没处理的
        // 那份会经挂起弹框补拉（pending-prompts）以 live 事件再来一次，
        // 走下面的自动弹窗。存档消息上的按钮与右键补救始终可用。
        if (replaying) break;
        // 防抖弹窗：1.5 秒内无新 PlanProposed 才自动打开
        if ((window as unknown as Record<string, unknown>).__planDebounceTimer) {
          clearTimeout((window as unknown as Record<string, unknown>).__planDebounceTimer as number);
        }
        (window as unknown as Record<string, unknown>).__planDebounceTimer = setTimeout(() => {
          openPlanImportModal(e.tasks, e.plan_id);
          (window as unknown as Record<string, unknown>).__planDebounceTimer = undefined;
        }, 1500);
        break;
      }
      case "ChoiceRequested": {
        // 去重：同一 choice_id 可能同时经 history 重放和挂起弹框补拉
        // 到达（SSE 新连接的 replay 前缀 + GET pending-prompts）。
        if (shouldSkipPrompt(e.choice_id)) break;
        // advisor 暂停门（choice_id 前缀 "advisor-pause-"）：渲染专用
        // 拍板卡片 + 置顶「已暂停」横幅，不走通用 select+提交流程。
        if (e.choice_id.startsWith("advisor-pause-")) {
          const msg = addMessage({
            kind: "system",
            content: `🦉 ${e.role_id} 介入：${e.question}`,
          });
          registerPromptRow(e.choice_id, msg);
          renderAdvisorPausePrompt(promptHost(msg, e.choice_id), e);
          showAdvisorPauseBanner();
          setFooter("advisor 已暂停，等待拍板");
          resetWaitTimer();
          break;
        }
        // ask 工具抛出的选择题：消息流里留一条系统消息 + 选择卡片，
        // 卡片随即被搬进居中弹窗（renderChoiceDialog 内部处理），用户
        // 不用下拉到底也知道会话正等着自己回答。选择结果按 wait 分两
        // 路回传（choice-answer 直达 / 作为下一条 user 消息）。
        const msg = addMessage({
          kind: "system",
          content: `❓ ${e.role_id} 请你选择：${e.question}`,
        });
        registerPromptRow(e.choice_id, msg);
        // 重放出来的历史选择题渲染成存档态（不可点）：它大概率早就答
        // 过了，给一张能点的卡片只会让用户白点一次（提交必然 404 降级
        // 成一条莫名其妙的新消息）。仍未回答的那条会经 pending-prompts
        // 以 live 事件再来一次，替换成可交互卡片。
        renderChoiceDialog(promptHost(msg, e.choice_id), e, { archived: replaying });
        setFooter(replaying ? `${e.role_id} 曾请你选择（历史）` : `${e.role_id} 等待你的选择`);
        resetWaitTimer();
        break;
      }
      case "TimeoutWarning": {
        // 后端报告：当前 turn 跑过 soft timeout 但仍在跑。后端
        // 不会自动终止 —— 弹出「继续等待 / 终止…」让用户决定。
        // 后端 hard_timeout_secs 现在是 0（无硬超时，不强杀），改由
        // 每个 soft 周期复发提醒。
        //
        // 按 sub_id 维度各挂一条：DAG 并行波（req_review ‖ code_review）
        // 会同时超时，同一条复发则原地更新秒数而不堆叠。
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
        const entry = targetRole ? takeExecutingRow(targetRole, errSubId) : undefined;
        if (entry) {
          const row = entry.row;
          const inner = row.querySelector(".message.status") as HTMLElement | null;
          if (inner) {
            inner.classList.remove("executing");
            inner.classList.add("error");
            const content = inner.querySelector(".content");
            if (content) content.textContent = `❌ ${targetRole} 失败: ${e.message}`;
            // 关联到具体 subagent (delegate 失败时后端带 sub_id；
            // 缺失时继承配对行的 subId，可能是空串)
            const linked = errSubId || entry.subId;
            if (linked) {
              inner.dataset.subId = linked;
              row.dataset.subId = linked;
            }
          }
         } else {
        }
        if (errSubId) {
          const errRole = activeDelegates.get(errSubId)?.targetRole ?? targetRole;
          if (errRole) clearStream(errRole, errSubId);
          retireDelegate(errSubId, "中断");
        } else {
          if (targetRole) clearStream(targetRole);
          // A top-level turn error/cancel drops the parent future. Any
          // delegate handlers still registered under it will never reach
          // their explicit DelegateFinished cleanup.
          retireTurnDelegates("父任务中断");
        }
        setFooter(`错误: ${truncate(e.message, 80)}`);
        // 该分派失败 —— 它的 TimeoutWarning 失去意义，收掉对应那条。
        // 带 sub_id 时精确移除（并行波里不误伤兄弟的提示条）。
        if (errSubId) {
          hideTimeoutPrompt(targetRole ?? "", errSubId);
        } else if (targetRole) {
          hideTimeoutPrompt(targetRole);
        }
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
    for (const r of roles) { const opt = document.createElement("option"); opt.value=r.id; opt.dataset.baseLabel=`${r.icon??""} ${r.id}`; if (r.id===selected) opt.selected=true; container.roleSelect.appendChild(opt); }
    applyPausedMarkers();
    updateRolePauseToggle();
  }
  function clear(): void {
    container.messagesEl.innerHTML="";
    msgCounter=0;
    messageStore.length=0;
    allEvents.length=0;
    currentEventIdx=-1;
    clearAllStreams();
    clearAllDelegates();
    subagentTools.length = 0;
    lastUserMsgId = "";
    lastRoleStarted = "";
    currentToolCall="";
    currentActivity="";
    currentDelegate="";
    delegateRunning=false;
    // 清掉残留的 timeout 询问条（切 session / 强制 clear 都用）。
    hideAllTimeoutPrompts();
    hideAdvisorPauseBanner();
    // 弹框状态一起清：
    // - promptRows：去重表跟着消息区一起作废，否则重放时所有弹框都
    //   被判成"已渲染过"而跳过，聊天区永远缺这几条。
    // - plan 导入弹窗挂在 document.body 上，`messagesEl.innerHTML=""`
    //   清不掉它。切 session 后它还浮在页面上，而里面的 sid/parentId
    //   是打开时算的 —— 用户在新 session 里点「导入」会把任务导到**旧**
    //   session 的父任务下。
    // - 已排队但没触发的自动弹窗定时器同理（1.5s 内切了 session 就会
    //   在新 session 上弹出旧清单）。
    promptRows.clear();
    document.getElementById("plan-import-modal")?.remove();
    const pendingPlanTimer = (window as unknown as Record<string, unknown>).__planDebounceTimer;
    if (pendingPlanTimer) {
      clearTimeout(pendingPlanTimer as number);
      (window as unknown as Record<string, unknown>).__planDebounceTimer = undefined;
    }
    // 上一个 session 的选择弹窗/待办铃不能跟着漂到新 session。
    clearChoiceDialogs();
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
      hideAllTimeoutPrompts();
      hideAdvisorPauseBanner();
      clearWaitTimer();
      // 冷启动重播：若历史停在 Paused（server 重启前 workflow 被暂停/
      // 进程崩溃），残留的运行中徽章永远等不到完成事件——标注「已中断」，
      // 避免误导；▶ 会触发后端从 checkpoint 续跑（chat_resume_session
      // 兜底逻辑）。仍在实时运行的 workflow（无尾随 Paused）不受影响：
      // 后续 live 事件会正常翻转这些徽章。
      if (isPaused) {
        for (const el of workflowStates.values()) {
          el.textContent = "⚠️ 已中断（点 ▶ 从断点续跑）";
          el.className = "delegate-state failed";
        }
        workflowStates.clear();
      }
      setStatus("connected");
    }
  }

  function setRoleFilePaths(paths: Record<string, string>): void {
    roleFilePaths = new Map(Object.entries(paths));
  }

  return { appendUser, handleEvent, replayEvents, setFooter, setRoleSelected, refreshRoles, setStatus, clear, focus, insertContext, setRoleFilePaths };
}

function truncate(s: string, max: number): string { if (s.length<=max) return s; return s.slice(0,max)+"…"; }
