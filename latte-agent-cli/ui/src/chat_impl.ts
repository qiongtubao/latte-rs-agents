import { ChatEvent, RoleInfo, sendMessage, sendChoiceAnswer, sendCommand, switchRole, cancelTurn, pauseSessionV2, resumeSessionV2, pauseRole, resumeRole, importTasks, refineParentFor, uploadImage, listWorkflows, listTasks, resumeWorkflow, getCurrentSessionId, type ImportTask, type ChoiceOption, type TaskView } from "./api";
import { BUILTIN_CMD_HINTS, mergeWorkflowCommands, type CmdHint } from "./cmd_hints";

/** ChoiceRequested 事件的窄化类型（从 ChatEvent union 抽出）。 */
type ChoiceRequestedEvent = Extract<ChatEvent, { type: "ChoiceRequested" }>;
import { extractImportableTasks } from "./workflows_panel";
import { extractCodeRefs, makeRefChips } from "./linkify";
import { buildSelectedPlanTasks } from "./plan_import";
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
  let timeoutPromptEl: HTMLElement | null = null;
  let timeoutPromptRole: string | null = null;
  /** advisor 暂停期间的「等待拍板」横幅；拍板或 Resumed 事件后清除。 */
  let advisorPauseBannerEl: HTMLElement | null = null;

  /** Last event identity rendered. SSE reconnects can replay the same
   * persisted event immediately after live delivery; suppress only exact
   * adjacent duplicates, preserving legitimate repeated turns. */
  let lastEventIdentity = "";

  function eventIdentity(e: ChatEvent): string {
    switch (e.type) {
      // Lifecycle events can legitimately repeat for the same role/detail
      // (e.g. two archived turns), so they are not deduplicated.
      case "RoleTurn": return `${e.type}|${e.role_id}|${e.sub_id ?? ""}|${e.is_complete}|${e.content}`;
      case "WorkflowTurn": return `${e.type}|${e.wf_id}|${e.step_id}|${e.round}|${e.content}`;
      case "ToolUse": return `${e.type}|${e.sub_id ?? ""}|${e.tool_name}|${e.args}`;
      case "ToolResult": return `${e.type}|${e.sub_id ?? ""}|${e.tool_name}|${e.result}`;
      case "ToolError": return `${e.type}|${e.sub_id ?? ""}|${e.tool_name}|${e.error}`;
      default: return "";
    }
  }
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
  /** wf_id → workflow run status and display name. */
  const workflowStates = new Map<string, HTMLElement>();
  const wfNames = new Map<string, string>();
  /** WorkflowStep metadata is paired FIFO with the next DelegateStarted. */
  const workflowStepQueues = new Map<string, Array<{ key: string; roleId: string; taskText: string }>>();
  const workflowStepMsgIds = new Map<string, string>();
  const workflowStepSubIds = new Map<string, string>();
  /** role_id → active streaming response bubble. */
  const streamingEl = new Map<string, HTMLElement>();
  /** role_id → accumulated raw streaming text. */
  const streamingRaw = new Map<string, string>();
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
  // ── ask 选择框（内联渲染在系统消息气泡里）──
  // ChoiceRequested 事件触发。选项/说明/图片来自模型（不可信），一律
  // 用 textContent / img.src 构建，绝不 innerHTML 拼接。回传分两路：
  // wait=true（子代理阻塞等答）→ POST choice-answer 直达等待方；
  // wait=false/缺省（顶层 turn）→ 拼成 user 消息经 sendMessage 回喂
  // （后端会 echo 一条 UserMessage 事件渲染用户气泡，这里不手动补）。
  function renderChoiceDialog(bubble: HTMLElement, e: ChoiceRequestedEvent): void {
    const multi = !!e.multi;
    const grid = e.layout === "grid";
    const card = document.createElement("div");
    card.className = "choice-card" + (grid ? " grid" : "");
    card.dataset.choiceId = e.choice_id;

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
      statusLine.textContent = ans.length ? "已选：" + ans.join("、") : "未选择";
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
      if (sendText === null) return;
      const showError = (err: unknown) => {
        console.error("[chat] choice submit failed:", err);
        done.textContent = `${summary}（发送失败：${err instanceof Error ? err.message : String(err)}，请手动输入你的选择）`;
      };
      if (e.wait) {
        // 阻塞中的子代理在等这个答案：直达 choice 路由；挂起项已消失
        // （超时/服务重启）时降级为普通 user 消息回喂。
        sendChoiceAnswer(e.choice_id, sendText)
          .then((delivered) => (delivered ? Promise.resolve() : sendMessage(sendText)))
          .catch(showError);
      } else {
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

    bubble.appendChild(card);
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

    // 每个任务一行：勾选框 + 可编辑 title/desc/priority。
    const rows: { cb: HTMLInputElement; titleInp: HTMLInputElement; descInp: HTMLTextAreaElement; priSel: HTMLSelectElement; task: ImportTask }[] = [];
    const list = document.createElement("div");
    list.className = "plan-import-list";
    for (const t of tasks) {
      const row = document.createElement("div");
      row.className = "plan-import-row";
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
      rows.push({ cb, titleInp, descInp, priSel, task: t });
    }
    box.appendChild(list);

    // 父任务下拉框：默认「自动」（拆分会话由后端 refine 映射解析，
    // 页面刷新也不丢）；可人工改选具体根任务、或显式选「无（根任
    // 务）」覆盖自动关联——防止父任务关联丢失时无法干预。
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
    // 异步拉根任务列表填充候选；本地 refineParent 命中时预选。
    void listTasks()
      .catch(() => [] as TaskView[])
      .then((all) => {
        if (!document.getElementById("plan-import-modal")) return; // 弹窗已关
        for (const t of all) {
          if (t.parent_id) continue; // 父任务只能是根任务
          const o = document.createElement("option");
          o.value = t.id;
          o.textContent = `${t.id} · ${t.title}`;
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
      const all = rows.every(r => r.cb.checked);
      rows.forEach(r => { r.cb.checked = !all; });
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
      const selected = buildSelectedPlanTasks(rows.map(r => ({
        checked: r.cb.checked,
        title: r.titleInp.value,
        description: r.descInp.value,
        priority: Number(r.priSel.value),
        task: r.task,
      })));
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
    // 插到 messages 流的最顶端，让用户一眼看到。
    container.messagesEl.insertBefore(node, container.messagesEl.firstChild);
    timeoutPromptEl = node;
    timeoutPromptRole = ev.role_id;
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
        hideTimeoutPrompt(e.role_id);
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
        // 首个 delta 创建气泡，后续 delta 追加到同一气泡（按 role 追踪）。
        const streamRow = streamingEl.get(e.role_id);
        if (e.is_complete) {
          // 终态：若之前有流式气泡，把终态 content 追加进去/结束；否则整段加新泡。
          if (streamRow) {
            const contentEl = streamRow.querySelector<HTMLElement>(".msg-content");
            if (contentEl) {
              // 终态 content 是完整文本——直接用 innerHTML 替换（避免与
              // 已追加的 delta 拼接误差）。仅当 content 与已显示不一致时。
              contentEl.innerHTML = renderMarkdown(e.content);
            }
            streamingEl.delete(e.role_id);
            streamingRaw.delete(e.role_id);
          } else {
            addMessage({ kind: "role", content: e.content, meta: e.role_id, icon, subagent, subId: roleSubId, reference: ref, filePath: getFilePath(e.role_id) });
          }
          currentToolCall = ""; currentActivity = ""; updateFooter();
          setStatus("connected"); clearWaitTimer();
          if (di) {
            activeDelegates.delete(subId);
            if (activeDelegates.size === 0) {
              if (!replaying) switchRole("manager").catch(() => {});
              container.rolePill.textContent = `${roleIcon("manager")} manager`;
            }
          }
        } else {
          // 增量 delta：已有流式气泡则累积重渲染，否则新建。
          if (streamRow) {
            const contentEl = streamRow.querySelector<HTMLElement>(".msg-content");
            const raw = (streamingRaw.get(e.role_id) ?? "") + e.content;
            streamingRaw.set(e.role_id, raw);
            if (contentEl) contentEl.innerHTML = renderMarkdown(raw);
          } else {
            const row = addMessage({ kind: "role", content: e.content, meta: e.role_id, icon, subagent, subId: roleSubId, reference: undefined, filePath: getFilePath(e.role_id) });
            streamingEl.set(e.role_id, row);
            streamingRaw.set(e.role_id, e.content);
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
        const finishedDi = activeDelegates.get(e.sub_id);
        if (finishedDi?.stateEl) {
          finishedDi.stateEl.textContent = isFail ? `❌ ${e.status}` : "✅ 完成";
          finishedDi.stateEl.className = `delegate-state ${isFail ? "failed" : "done"}`;
        }
        msg.addEventListener("contextmenu", (ev) => { ev.preventDefault(); onShowSubsession?.(e.sub_id, label, msg); });
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
        const n = e.tasks.length;
        const msg = addMessage({
          kind: "system",
          content: `📋 ${e.role_id} 提交了 ${n} 个任务候选（${e.plan_id}），请在弹窗勾选导入任务看板`,
          planTasks: e.tasks,
          planId: e.plan_id,
        });
        // 气泡内附「导入任务看板」按钮，作为弹窗入口。
        const btn = document.createElement("button");
        btn.className = "workflow-import-btn";
        btn.textContent = `📥 导入任务看板（${n} 个）`;
        btn.addEventListener("click", () => openPlanImportModal(e.tasks, e.plan_id));
        msg.querySelector(".msg-bubble")?.appendChild(btn);
        setFooter(`${e.role_id} 提交 ${n} 个任务候选`);
        resetWaitTimer();
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
        // advisor 暂停门（choice_id 前缀 "advisor-pause-"）：渲染专用
        // 拍板卡片 + 置顶「已暂停」横幅，不走通用 select+提交流程。
        if (e.choice_id.startsWith("advisor-pause-")) {
          const msg = addMessage({
            kind: "system",
            content: `🦉 ${e.role_id} 介入：${e.question}`,
          });
          const bubble = msg.querySelector(".msg-bubble") as HTMLElement | null;
          if (bubble) renderAdvisorPausePrompt(bubble, e);
          showAdvisorPauseBanner();
          setFooter("advisor 已暂停，等待拍板");
          resetWaitTimer();
          break;
        }
        // ask 工具抛出的选择题：渲染一条系统消息 + 内联选择卡片。
        // 用户在卡片里选择/上传后，选择结果作为下一条 user 消息回喂角色。
        const msg = addMessage({
          kind: "system",
          content: `❓ ${e.role_id} 请你选择：${e.question}`,
        });
        const bubble = msg.querySelector(".msg-bubble") as HTMLElement | null;
        if (bubble) renderChoiceDialog(bubble, e);
        setFooter(`${e.role_id} 等待你的选择`);
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
    streamingEl.clear();
    streamingRaw.clear();
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
    hideAdvisorPauseBanner();
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
