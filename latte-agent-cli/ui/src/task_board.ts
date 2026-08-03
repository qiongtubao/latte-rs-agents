// 任务看板面板：把 task-board.html 静态 mockup 移植成真实前端。
//
// 数据来自后端 task tracker（/api/tasks，见 api.ts 的 Task* 类型）；
// 无 WebSocket，进面板拉一次后每 5s 轮询，任何写操作后立即刷新。
//
// 状态机（与 mockup 一致）：
//   列顺序  backlog / todo / in_progress / human_review / rework / merging / done
//   cancelled 不上看板列。
// 每个状态声明自己的「下一步动作」（STATE_ACTIONS），卡片上只放主
// 动作，其余进右侧详情抽屉。
import {
  listTasks, createTask, updateTask, dispatchTask, abortTask,
  dispatchReady, listWorkflows,
} from "./api";
import type { TaskView, TaskState, WorkflowSummary, DispatchReadyResponse } from "./api";

// ─── 状态机定义（纯数据/纯函数，单独可测） ─────────────────────────

export interface StateMeta {
  name: string;
  color: string;
  bg: string;
  hint: string;
}

export const STATES: Record<TaskState, StateMeta> = {
  backlog:      { name: "Backlog",      color: "#94a3b8", bg: "#f1f5f9", hint: "暂存区，不会被调度" },
  todo:         { name: "Todo",         color: "#3b82f6", bg: "#dbeafe", hint: "可立即执行，或指定时间执行" },
  in_progress:  { name: "In Progress",  color: "#22c55e", bg: "#dcfce7", hint: "已交给 manager 执行中" },
  human_review: { name: "Human Review", color: "#f59e0b", bg: "#fef3c7", hint: "执行完毕，等待人工确认" },
  rework:       { name: "Rework",       color: "#ef4444", bg: "#fee2e2", hint: "被打回，需重新派发" },
  merging:      { name: "Merging",      color: "#8b5cf6", bg: "#ede9fe", hint: "评审通过，合并收尾中" },
  done:         { name: "Done",         color: "#10b981", bg: "#d1fae5", hint: "已完成" },
  cancelled:    { name: "Cancelled",    color: "#64748b", bg: "#e2e8f0", hint: "已取消" },
};

export const COLUMN_ORDER: TaskState[] = [
  "backlog", "todo", "in_progress", "human_review", "rework", "merging", "done",
];

export type ActionKind = "primary" | "ghost" | "danger";

export interface TaskAction {
  key: string;
  label: string;
  kind: ActionKind;
}

/** 每个状态的「下一步」动作（状态机核心：状态 → 动作列表）。
 * todo_scheduled 是 todo + 已排期的变体。 */
export const STATE_ACTIONS: Record<string, TaskAction[]> = {
  backlog: [
    { key: "to_todo", label: "→ 移到 Todo", kind: "primary" },
    { key: "edit", label: "编辑", kind: "ghost" },
    { key: "cancel", label: "取消任务", kind: "danger" },
  ],
  todo: [
    { key: "run_now", label: "▶ 立即执行", kind: "primary" },
    { key: "schedule", label: "🕐 指定时间执行", kind: "ghost" },
    { key: "to_backlog", label: "移回 Backlog", kind: "ghost" },
  ],
  todo_scheduled: [
    { key: "run_now", label: "▶ 立即执行", kind: "primary" },
    { key: "schedule", label: "修改时间", kind: "ghost" },
    { key: "unschedule", label: "取消排期", kind: "ghost" },
  ],
  in_progress: [
    { key: "open_session", label: "💬 查看对话", kind: "primary" },
    { key: "abort", label: "中止执行", kind: "danger" },
  ],
  human_review: [
    { key: "open_session", label: "💬 查看对话", kind: "primary" },
    { key: "approve", label: "✓ 确认通过", kind: "primary" },
    { key: "reject", label: "↩ 打回重做", kind: "danger" },
  ],
  rework: [
    { key: "run_now", label: "▶ 重新派发", kind: "primary" },
    { key: "edit", label: "编辑补充要求", kind: "ghost" },
  ],
  merging: [
    { key: "open_session", label: "💬 查看对话", kind: "ghost" },
    { key: "mark_done", label: "✓ 标记完成", kind: "primary" },
  ],
  done: [
    { key: "reopen", label: "重新打开", kind: "ghost" },
  ],
  cancelled: [
    { key: "reopen", label: "重新打开", kind: "ghost" },
  ],
};

/** todo + 已排期时动作列表切换到 todo_scheduled 变体。 */
export function effectiveActions(
  task: Pick<TaskView, "state" | "scheduled_at">,
): TaskAction[] {
  if (task.state === "todo" && task.scheduled_at != null) {
    return STATE_ACTIONS.todo_scheduled;
  }
  return STATE_ACTIONS[task.state] ?? [];
}

/** 动作按钮文案：绑定了 workflow 的任务，「立即执行 / 重新派发」按钮
 *  直接标出将运行的 workflow（派发时后端会直接运行它）。 */
export function actionLabel(
  task: Pick<TaskView, "workflow">,
  a: TaskAction,
): string {
  if (a.key === "run_now" && task.workflow) return `▶ 运行 ${task.workflow}`;
  return a.label;
}

/** 「查看对话」用的 session：runs 最后一个元素；没有则 null（按钮禁用）。 */
export function lastSessionId(task: Pick<TaskView, "runs">): string | null {
  const last = task.runs[task.runs.length - 1];
  return last ? last.session_id : null;
}

/** 动作 key → 后端调用。schedule / edit / open_session 在前端处理，
 * 不走这里（返回 null 表示需要 UI 介入）。 */
export function actionRequest(
  key: string,
  taskId: string,
): Promise<unknown> | null {
  switch (key) {
    case "to_todo":
      return updateTask(taskId, { state: "todo" });
    case "to_backlog":
      return updateTask(taskId, { state: "backlog", scheduled_at: null });
    case "run_now":
      return dispatchTask(taskId);
    case "unschedule":
      return updateTask(taskId, { scheduled_at: null });
    case "abort":
      return abortTask(taskId);
    case "approve":
      return updateTask(taskId, { state: "merging" });
    case "reject":
      return updateTask(taskId, { state: "rework" });
    case "mark_done":
      return updateTask(taskId, { state: "done" });
    case "reopen":
      return updateTask(taskId, { state: "todo" });
    case "cancel":
      return updateTask(taskId, { state: "cancelled", scheduled_at: null });
    default:
      return null; // schedule / edit / open_session：UI 侧处理
  }
}

/** 动作成功后的 toast 文案。 */
export function actionToast(key: string, taskId: string): string {
  switch (key) {
    case "to_todo": return `${taskId} 已移到 Todo，可立即或定时执行`;
    case "to_backlog": return `${taskId} 已移回 Backlog`;
    case "run_now": return `${taskId} 已交给 manager，新 session 已创建`;
    case "unschedule": return `${taskId} 已取消排期`;
    case "abort": return `${taskId} 已中止，回到 Todo`;
    case "approve": return `${taskId} 评审通过，进入 Merging`;
    case "reject": return `${taskId} 已打回重做`;
    case "mark_done": return `${taskId} 已完成 🎉`;
    case "reopen": return `${taskId} 已重新打开`;
    case "cancel": return `${taskId} 已取消`;
    default: return `${taskId} 已更新`;
  }
}

/** 「派发全部」结果提示：派了 N 个、跳过 M 个（附前几条原因）。 */
export function dispatchReadyToast(resp: DispatchReadyResponse): string {
  const n = resp.dispatched.length;
  const m = resp.skipped.length;
  let msg = `已派发 ${n} 个任务`;
  if (m > 0) {
    const reasons = resp.skipped
      .slice(0, 3)
      .map(([id, reason]) => `${id}: ${reason}`)
      .join("；");
    msg += `，跳过 ${m} 个（${reasons}${m > 3 ? "；…" : ""}）`;
  }
  return msg;
}

// ─── 工具函数 ────────────────────────────────────────────────────

function fmtTime(ts: number): string {
  const d = new Date(ts);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

function toLocalInput(d: Date): string {
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

function el(tag: string, className?: string, text?: string): HTMLElement {
  const e = document.createElement(tag);
  if (className) e.className = className;
  if (text !== undefined) e.textContent = text;
  return e;
}

const POLL_INTERVAL_MS = 5000;

// ─── 面板挂载 ────────────────────────────────────────────────────

export interface TaskBoardContainer {
  panelEl: HTMLElement;
  openBtn: HTMLButtonElement;
  closeBtn: HTMLButtonElement;
  newBtn: HTMLButtonElement;
  dispatchAllBtn: HTMLButtonElement;
  statRunningEl: HTMLElement;
  statScheduledEl: HTMLElement;
  statReviewEl: HTMLElement;
  statusEl: HTMLElement;
  boardEl: HTMLElement;
}

export interface TaskBoardController {
  isOpen(): boolean;
  open(): void;
  close(): void;
}

export function mountTaskBoard(opts: {
  container: TaskBoardContainer;
  /** 「查看对话」：切到执行该任务的 session（main.ts 的 activateSession）。 */
  onOpenSession: (sessionId: string) => void;
}): TaskBoardController {
  const { container, onOpenSession } = opts;
  let tasks: TaskView[] = [];
  let isLoading = false;
  let loadedOnce = false;
  let pollTimer: number | null = null;
  let drawerTaskId: string | null = null;

  // ── toast ──
  const toastEl = el("div", "tb-toast");
  document.body.appendChild(toastEl);
  let toastTimer: number | undefined;
  function toast(msg: string): void {
    toastEl.textContent = msg;
    toastEl.classList.add("show");
    window.clearTimeout(toastTimer);
    toastTimer = window.setTimeout(() => toastEl.classList.remove("show"), 2400);
  }

  const setStatus = (msg: string, error = false) => {
    container.statusEl.textContent = msg;
    container.statusEl.classList.toggle("error", error);
  };

  // ── 详情抽屉（动态创建，fixed 定位） ──
  const drawerMask = el("div", "tb-drawer-mask");
  const drawer = el("div", "tb-drawer");
  document.body.append(drawerMask, drawer);
  drawerMask.addEventListener("click", closeDrawer);

  function openDrawer(id: string): void {
    drawerTaskId = id;
    renderDrawer();
    drawer.classList.add("open");
    drawerMask.classList.add("open");
  }
  function closeDrawer(): void {
    drawerTaskId = null;
    drawer.classList.remove("open");
    drawerMask.classList.remove("open");
  }

  function renderDrawer(): void {
    if (!drawerTaskId) return;
    const task = tasks.find(t => t.id === drawerTaskId);
    if (!task) { closeDrawer(); return; }
    const st = STATES[task.state] ?? STATES.backlog;
    drawer.replaceChildren();

    // header
    const header = el("div", "tb-drawer-header");
    header.appendChild(el("span", "tb-task-id", task.id));
    header.appendChild(el("h3", undefined, task.title));
    const closeBtn = el("button", "tb-drawer-close", "✕") as HTMLButtonElement;
    closeBtn.addEventListener("click", closeDrawer);
    header.appendChild(closeBtn);
    drawer.appendChild(header);

    const body = el("div", "tb-drawer-body");

    // 状态徽章
    const stateSec = el("div", "tb-drawer-section");
    const badge = el("span", "tb-state-badge");
    badge.style.background = st.bg;
    badge.style.color = st.color;
    const dot = el("span");
    dot.style.cssText = `width:8px;height:8px;border-radius:50%;background:${st.color}`;
    badge.append(dot, document.createTextNode(st.name));
    stateSec.appendChild(badge);
    body.appendChild(stateSec);

    // 下一步动作区
    const actSec = el("div", "tb-drawer-section");
    actSec.appendChild(el("div", "tb-label", "下一步动作 · 当前状态可执行"));
    const box = el("div", "tb-next-step-box");
    box.appendChild(el("div", "tb-next-title", `⚡ ${st.name} 状态下可用`));
    const actions = el("div", "tb-actions");
    for (const a of effectiveActions(task)) {
      actions.appendChild(actionButton(task, a, false));
    }
    box.appendChild(actions);
    actSec.appendChild(box);
    body.appendChild(actSec);

    // 描述
    const descSec = el("div", "tb-drawer-section");
    descSec.appendChild(el("div", "tb-label", "任务描述"));
    descSec.appendChild(el("div", "tb-desc-box", task.description || "（无描述）"));
    body.appendChild(descSec);

    // 属性
    const propSec = el("div", "tb-drawer-section");
    propSec.appendChild(el("div", "tb-label", "属性"));
    propSec.appendChild(kv("优先级", `P${task.priority}`));
    if (task.workflow) {
      propSec.appendChild(kv("Workflow", `🔀 ${task.workflow}（派发时直接运行该 workflow）`));
    }
    propSec.appendChild(kv("创建时间", fmtTime(task.created_at)));
    if (task.scheduled_at != null && task.state === "todo") {
      propSec.appendChild(kv("排期时间", `🕐 ${fmtTime(task.scheduled_at)}（到点自动交给 manager）`));
    }
    const sessionId = lastSessionId(task);
    if (sessionId) {
      const row = el("div", "tb-kv");
      row.appendChild(el("span", "tb-k", "执行会话"));
      const v = el("span", "tb-v");
      const link = el("a", undefined, `${sessionId} ↗`) as HTMLAnchorElement;
      link.href = "#";
      link.addEventListener("click", (e) => {
        e.preventDefault();
        onOpenSession(sessionId);
      });
      v.appendChild(link);
      row.appendChild(v);
      propSec.appendChild(row);
    }
    if (task.parent_id) {
      const row = el("div", "tb-kv");
      row.appendChild(el("span", "tb-k", "父任务"));
      const v = el("span", "tb-v");
      const link = el("a", undefined, `${task.parent_id} ↗`) as HTMLAnchorElement;
      link.href = "#";
      link.addEventListener("click", (e) => {
        e.preventDefault();
        openDrawer(task.parent_id!);
      });
      v.appendChild(link);
      row.appendChild(v);
      propSec.appendChild(row);
    }
    if (task.labels.length > 0) {
      propSec.appendChild(kv("标签", task.labels.join(", ")));
    }
    body.appendChild(propSec);

    // 子任务区（根任务可拆分）
    if (!task.parent_id) {
      body.appendChild(renderChildrenSection(task));
    }

    // 状态历史
    const histSec = el("div", "tb-drawer-section");
    histSec.appendChild(el("div", "tb-label", "状态历史"));
    const timeline = el("div", "tb-timeline");
    const hist = [...task.history].reverse();
    if (hist.length === 0) {
      timeline.appendChild(el("div", "tb-hint", "（暂无历史）"));
    }
    hist.forEach((h, i) => {
      const item = el("div", "tb-tl-item" + (i === 0 ? " hl" : ""));
      const fromName = h.from ? (STATES[h.from as TaskState]?.name ?? h.from) : "创建";
      const toName = STATES[h.to as TaskState]?.name ?? h.to;
      let text = h.from ? `${fromName} → ${toName}` : `任务创建于 ${toName}`;
      if (h.note) text += `：${h.note}`;
      if (h.actor && h.actor !== "user") text += `（${h.actor}）`;
      item.appendChild(el("div", "tb-tl-text", text));
      item.appendChild(el("div", "tb-tl-time", fmtTime(h.at)));
      timeline.appendChild(item);
    });
    histSec.appendChild(timeline);
    body.appendChild(histSec);

    drawer.appendChild(body);
  }

  function kv(k: string, v: string): HTMLElement {
    const row = el("div", "tb-kv");
    row.appendChild(el("span", "tb-k", k));
    row.appendChild(el("span", "tb-v", v));
    return row;
  }

  function renderChildrenSection(task: TaskView): HTMLElement {
    const sec = el("div", "tb-drawer-section");
    const head = el("div", "tb-children-head");
    head.appendChild(el("div", "tb-label", `子任务 ${task.sub_done}/${task.sub_total}`));
    const addBtn = el("button", "tb-btn tb-btn-ghost tb-btn-sm", "＋ 拆分子任务") as HTMLButtonElement;
    addBtn.addEventListener("click", () => openTaskModal({ parentId: task.id }));
    head.appendChild(addBtn);
    sec.appendChild(head);

    // 子任务明细由全量列表按 parent_id 推导（后端只给聚合计数）。
    const children = tasks
      .filter((t) => t.parent_id === task.id)
      .sort((a, b) => a.sub_order - b.sub_order || a.created_at - b.created_at);
    if (children.length > 0) {
      const list = el("div", "tb-child-list");
      for (const c of children) {
        const cst = STATES[c.state] ?? STATES.backlog;
        const item = el("div", "tb-child-item");
        const cdot = el("span", "tb-state-dot");
        cdot.style.background = cst.color;
        item.appendChild(cdot);
        item.appendChild(el("span", "tb-child-id", c.id));
        item.appendChild(el("span", "tb-child-title", c.title));
        item.appendChild(el("span", "tb-child-state", cst.name));
        item.addEventListener("click", () => openDrawer(c.id));
        list.appendChild(item);
      }
      sec.appendChild(list);
    } else {
      sec.appendChild(el("div", "tb-hint", "还没有子任务。拆分子任务后由父任务跟踪进度。"));
    }
    return sec;
  }

  // ── 动作按钮 ──
  function actionButton(task: TaskView, a: TaskAction, small: boolean): HTMLButtonElement {
    const cls = a.kind === "primary" ? "tb-btn-primary" : a.kind === "danger" ? "tb-btn-danger" : "tb-btn-ghost";
    const btn = el("button", `tb-btn ${cls}${small ? " tb-btn-sm" : ""}`, actionLabel(task, a)) as HTMLButtonElement;
    // 「查看对话」没有 run 时禁用。
    if (a.key === "open_session" && !lastSessionId(task)) {
      btn.disabled = true;
      btn.title = "还没有执行记录（无 session）";
    }
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      void doAction(task, a.key);
    });
    return btn;
  }

  async function doAction(task: TaskView, key: string): Promise<void> {
    if (key === "schedule") { openSchedule(task); return; }
    if (key === "edit") { openTaskModal({ edit: task }); return; }
    if (key === "open_session") {
      const sid = lastSessionId(task);
      if (sid) onOpenSession(sid);
      return;
    }
    const req = actionRequest(key, task.id);
    if (!req) return;
    try {
      await req;
      toast(actionToast(key, task.id));
    } catch (e) {
      toast(`操作失败: ${(e as Error).message}`);
    }
    await refresh({ silent: true });
  }

  // ── 定时弹窗 ──
  const scheduleMask = el("div", "tb-modal-mask");
  const scheduleModal = el("div", "tb-modal");
  scheduleMask.appendChild(scheduleModal);
  document.body.appendChild(scheduleMask);
  let scheduleTaskId: string | null = null;

  scheduleModal.appendChild(el("h4", undefined, "指定时间执行"));
  const scheduleSub = el("div", "tb-modal-sub");
  scheduleModal.appendChild(scheduleSub);
  const scheduleInput = document.createElement("input");
  scheduleInput.type = "datetime-local";
  scheduleModal.appendChild(scheduleInput);
  const quickTimes = el("div", "tb-quick-times");
  const quickBtn = (label: string, fn: () => Date): HTMLButtonElement => {
    const b = el("button", "tb-btn tb-btn-ghost tb-btn-sm", label) as HTMLButtonElement;
    b.type = "button";
    b.addEventListener("click", () => { scheduleInput.value = toLocalInput(fn()); });
    return b;
  };
  quickTimes.append(
    quickBtn("10 分钟后", () => new Date(Date.now() + 10 * 60000)),
    quickBtn("1 小时后", () => new Date(Date.now() + 60 * 60000)),
    quickBtn("今晚 20:00", () => { const d = new Date(); d.setHours(20, 0, 0, 0); return d; }),
    quickBtn("明早 9:00", () => { const d = new Date(Date.now() + 86400_000); d.setHours(9, 0, 0, 0); return d; }),
  );
  scheduleModal.appendChild(quickTimes);
  const scheduleActions = el("div", "tb-modal-actions");
  const scheduleCancel = el("button", "tb-btn tb-btn-ghost", "取消") as HTMLButtonElement;
  scheduleCancel.type = "button";
  scheduleCancel.addEventListener("click", closeSchedule);
  const scheduleOk = el("button", "tb-btn tb-btn-primary", "确认排期") as HTMLButtonElement;
  scheduleOk.type = "button";
  scheduleOk.addEventListener("click", () => void confirmSchedule());
  scheduleActions.append(scheduleCancel, scheduleOk);
  scheduleModal.appendChild(scheduleActions);
  scheduleMask.addEventListener("click", (e) => {
    if (e.target === scheduleMask) closeSchedule();
  });

  function openSchedule(task: TaskView): void {
    scheduleTaskId = task.id;
    scheduleSub.textContent = `${task.id} · ${task.title}`;
    const base = task.scheduled_at != null ? new Date(task.scheduled_at) : new Date(Date.now() + 3600_000);
    scheduleInput.value = toLocalInput(base);
    scheduleMask.classList.add("open");
  }
  function closeSchedule(): void {
    scheduleTaskId = null;
    scheduleMask.classList.remove("open");
  }
  async function confirmSchedule(): Promise<void> {
    const v = scheduleInput.value;
    if (!v) { toast("请先选择时间"); return; }
    const id = scheduleTaskId;
    if (!id) return;
    const ts = new Date(v).getTime();
    try {
      await updateTask(id, { scheduled_at: ts });
      closeSchedule();
      toast(`${id} 已排期 ${fmtTime(ts)}，到点自动执行`);
    } catch (e) {
      toast(`排期失败: ${(e as Error).message}`);
    }
    await refresh({ silent: true });
  }

  // ── 新建 / 编辑任务弹窗 ──
  const taskMask = el("div", "tb-modal-mask");
  const taskModal = el("div", "tb-modal");
  taskMask.appendChild(taskModal);
  document.body.appendChild(taskMask);
  let editingTaskId: string | null = null;

  const taskModalTitle = el("h4", undefined, "新建任务");
  taskModal.appendChild(taskModalTitle);
  const taskForm = document.createElement("form");
  taskForm.className = "tb-form";

  const titleLabel = el("label", "tb-field", "标题");
  const titleInput = document.createElement("input");
  titleInput.type = "text";
  titleInput.required = true;
  titleInput.placeholder = "任务标题（必填）";
  titleLabel.appendChild(titleInput);

  const descLabel = el("label", "tb-field", "描述");
  const descInput = document.createElement("textarea");
  descInput.rows = 4;
  descInput.placeholder = "任务描述：背景、验收标准、交给 manager 的说明…";
  descLabel.appendChild(descInput);

  const row = el("div", "tb-form-row");
  const prioLabel = el("label", "tb-field", "优先级");
  const prioSelect = document.createElement("select");
  for (const p of [1, 2, 3, 4]) {
    const opt = document.createElement("option");
    opt.value = String(p);
    opt.textContent = `P${p}`;
    if (p === 3) opt.selected = true;
    prioSelect.appendChild(opt);
  }
  prioLabel.appendChild(prioSelect);

  const parentLabel = el("label", "tb-field", "父任务（可选）");
  const parentSelect = document.createElement("select");
  parentLabel.appendChild(parentSelect);

  const wfLabel = el("label", "tb-field", "Workflow（可选）");
  const wfSelect = document.createElement("select");
  wfLabel.appendChild(wfSelect);
  row.append(prioLabel, parentLabel, wfLabel);

  const formActions = el("div", "tb-modal-actions");
  const formCancel = el("button", "tb-btn tb-btn-ghost", "取消") as HTMLButtonElement;
  formCancel.type = "button";
  formCancel.addEventListener("click", closeTaskModal);
  const formOk = el("button", "tb-btn tb-btn-primary", "创建") as HTMLButtonElement;
  formOk.type = "submit";
  formActions.append(formCancel, formOk);

  taskForm.append(titleLabel, descLabel, row, formActions);
  taskModal.appendChild(taskForm);
  taskMask.addEventListener("click", (e) => {
    if (e.target === taskMask) closeTaskModal();
  });
  taskForm.addEventListener("submit", (e) => {
    e.preventDefault();
    void submitTaskForm();
  });

  /** 用当前可选 workflow 列表填充下拉框；首项是「不绑定」。 */
  function populateWorkflowSelect(wfs: WorkflowSummary[], selected: string): void {
    wfSelect.replaceChildren();
    const none = document.createElement("option");
    none.value = "";
    none.textContent = "不绑定 workflow";
    wfSelect.appendChild(none);
    for (const w of wfs) {
      const opt = document.createElement("option");
      opt.value = w.name;
      opt.textContent = w.description ? `${w.name}（${w.description}）` : w.name;
      wfSelect.appendChild(opt);
    }
    // 绑定的 workflow 可能已被删除：保留一个占位项让当前值可见。
    if (selected && !wfs.some((w) => w.name === selected)) {
      const opt = document.createElement("option");
      opt.value = selected;
      opt.textContent = `${selected}（已不存在）`;
      wfSelect.appendChild(opt);
    }
    wfSelect.value = selected;
  }

  function openTaskModal(opts2: { parentId?: string; edit?: TaskView } = {}): void {
    editingTaskId = opts2.edit?.id ?? null;
    taskModalTitle.textContent = opts2.edit ? `编辑任务 ${opts2.edit.id}` : "新建任务";
    formOk.textContent = opts2.edit ? "保存" : "创建";
    titleInput.value = opts2.edit?.title ?? "";
    descInput.value = opts2.edit?.description ?? "";
    prioSelect.value = String(opts2.edit?.priority ?? 3);

    // 父任务候选：只列无 parent 的根任务；编辑时排除自己。
    parentSelect.replaceChildren();
    const none = document.createElement("option");
    none.value = "";
    none.textContent = "（无 — 作为根任务）";
    parentSelect.appendChild(none);
    for (const t of tasks) {
      if (t.parent_id) continue;
      if (opts2.edit && t.id === opts2.edit.id) continue;
      const opt = document.createElement("option");
      opt.value = t.id;
      opt.textContent = `${t.id} · ${t.title}`;
      parentSelect.appendChild(opt);
    }
    parentSelect.value = opts2.parentId ?? opts2.edit?.parent_id ?? "";
    // 拆分子任务入口进来时父任务固定，不允许改。
    parentSelect.disabled = !!opts2.parentId;

    // workflow 下拉：先按编辑值/空值放好占位，再异步拉最新列表填充。
    const wfSelected = opts2.edit?.workflow ?? "";
    populateWorkflowSelect([], wfSelected);
    void listWorkflows()
      .catch(() => [] as WorkflowSummary[])
      .then((wfs) => {
        // 弹窗仍开着才回填（避免关掉后改到无关状态）。
        if (taskMask.classList.contains("open")) {
          populateWorkflowSelect(wfs, wfSelect.value || wfSelected);
        }
      });

    taskMask.classList.add("open");
    titleInput.focus();
  }
  function closeTaskModal(): void {
    editingTaskId = null;
    taskMask.classList.remove("open");
  }
  async function submitTaskForm(): Promise<void> {
    const title = titleInput.value.trim();
    if (!title) { toast("请填写标题"); return; }
    const description = descInput.value.trim();
    const priority = Number(prioSelect.value);
    const workflow = wfSelect.value;
    try {
      if (editingTaskId) {
        // PATCH 语义：workflow 缺省 = 不变，null = 清除绑定。
        await updateTask(editingTaskId, {
          title, description, priority,
          workflow: workflow || null,
        });
        toast(`${editingTaskId} 已保存`);
      } else {
        const parentId = parentSelect.value || undefined;
        const created = await createTask({
          title,
          description: description || undefined,
          priority,
          parent_id: parentId,
          workflow: workflow || undefined,
        });
        toast(`${created.id} 已创建${parentId ? `（${parentId} 的子任务）` : ""}`);
      }
      closeTaskModal();
    } catch (e) {
      toast(`保存失败: ${(e as Error).message}`);
      return;
    }
    await refresh({ silent: true });
  }

  // ── 渲染看板 ──
  function render(): void {
    container.boardEl.replaceChildren();
    // 看板卡片只放根任务；子任务通过父任务抽屉进入。
    const roots = tasks.filter(t => !t.parent_id);
    for (const key of COLUMN_ORDER) {
      const st = STATES[key];
      const list = roots.filter(t => t.state === key);
      const col = el("div", "tb-column");
      const head = el("div", "tb-column-header");
      const dot2 = el("span", "tb-state-dot");
      dot2.style.background = st.color;
      head.append(dot2,
        el("span", "tb-state-name", st.name),
        el("span", "tb-count", String(list.length)),
        el("span", "tb-hint", st.hint));
      col.appendChild(head);
      const bodyEl = el("div", "tb-column-body");
      for (const task of list) bodyEl.appendChild(renderCard(task));
      col.appendChild(bodyEl);
      container.boardEl.appendChild(col);
    }
    // 顶栏统计（统计所有任务，含子任务）
    container.statRunningEl.textContent = String(tasks.filter(t => t.state === "in_progress").length);
    container.statScheduledEl.textContent = String(tasks.filter(t => t.state === "todo" && t.scheduled_at != null).length);
    container.statReviewEl.textContent = String(tasks.filter(t => t.state === "human_review").length);
  }

  function renderCard(task: TaskView): HTMLElement {
    const card = el("div", "tb-card");
    card.addEventListener("click", () => openDrawer(task.id));

    const top = el("div", "tb-card-top");
    top.appendChild(el("span", "tb-task-id", task.id));
    top.appendChild(el("span", `tb-prio p${task.priority}`, `P${task.priority}`));
    card.appendChild(top);
    card.appendChild(el("div", "tb-title", task.title));

    const meta = el("div", "tb-meta");
    if (task.workflow) {
      meta.appendChild(el("span", "tb-chip tb-chip-workflow", `🔀 ${task.workflow}`));
    }
    if (task.scheduled_at != null && task.state === "todo") {
      const due = task.scheduled_at <= Date.now();
      meta.appendChild(el("span", `tb-chip tb-chip-sched${due ? " due" : ""}`,
        `🕐 ${due ? "到点待派发" : fmtTime(task.scheduled_at) + " 执行"}`));
    }
    const sessionId = lastSessionId(task);
    if (sessionId) {
      const chip = el("span", "tb-chip tb-chip-session", "💬 session");
      chip.title = sessionId;
      chip.addEventListener("click", (e) => {
        e.stopPropagation();
        onOpenSession(sessionId);
      });
      meta.appendChild(chip);
    }
    if (task.state === "in_progress" && task.runs.length > 0) {
      meta.appendChild(el("span", "tb-chip tb-chip-rounds", `run #${task.runs.length}`));
    }
    if (task.sub_total > 0) {
      meta.appendChild(el("span", "tb-chip tb-chip-rounds", `子任务 ${task.sub_done}/${task.sub_total}`));
    }
    card.appendChild(meta);

    // 卡片快捷动作（只放主动作，其余进抽屉）
    const primary = effectiveActions(task).find(a => a.kind === "primary");
    if (primary) {
      const quick = el("div", "tb-quick-actions");
      quick.appendChild(actionButton(task, primary, true));
      card.appendChild(quick);
    }
    return card;
  }

  // ── 数据 ──
  async function refresh(opts2: { silent?: boolean } = {}): Promise<void> {
    if (isLoading) return;
    isLoading = true;
    if (!opts2.silent) setStatus("加载中…");
    try {
      tasks = await listTasks();
      loadedOnce = true;
      setStatus(`已加载 ${tasks.length} 个任务 · 每 ${POLL_INTERVAL_MS / 1000}s 自动刷新`);
      render();
      if (drawerTaskId) renderDrawer();
    } catch (e) {
      setStatus(`加载失败: ${(e as Error).message}`, true);
      if (loadedOnce) toast(`刷新失败: ${(e as Error).message}`);
    } finally {
      isLoading = false;
    }
  }

  function startPolling(): void {
    stopPolling();
    pollTimer = window.setInterval(() => void refresh({ silent: true }), POLL_INTERVAL_MS);
  }
  function stopPolling(): void {
    if (pollTimer !== null) {
      window.clearInterval(pollTimer);
      pollTimer = null;
    }
  }

  // ── 开关 ──
  function open(): void {
    container.panelEl.classList.remove("hidden");
    void refresh();
    startPolling();
  }
  function close(): void {
    container.panelEl.classList.add("hidden");
    stopPolling();
    closeDrawer();
    closeSchedule();
    closeTaskModal();
  }

  container.openBtn.addEventListener("click", open);
  container.closeBtn.addEventListener("click", close);
  container.newBtn.addEventListener("click", () => openTaskModal());
  container.dispatchAllBtn.addEventListener("click", () => void dispatchAll());

  /** 「派发全部」：后端按优先级批量派发 todo，冲突/超限的留 todo 等位。 */
  async function dispatchAll(): Promise<void> {
    container.dispatchAllBtn.disabled = true;
    try {
      const resp = await dispatchReady();
      toast(dispatchReadyToast(resp));
    } catch (e) {
      toast(`派发失败: ${(e as Error).message}`);
    } finally {
      container.dispatchAllBtn.disabled = false;
    }
    await refresh({ silent: true });
  }

  return {
    isOpen: () => !container.panelEl.classList.contains("hidden"),
    open,
    close,
  };
}
