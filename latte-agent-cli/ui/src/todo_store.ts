// todo 进度的纯状态模块（无 DOM 依赖，可在 node 环境单测）。
//
// 后端每次完成 todo 工具调用后广播 TodoUpdated 事件，携带**全量**
// phases（整表替换语义，不需要 diff）。chat_impl.ts 的事件分发把事件
// 交给 applyTodoUpdated 归约成本地 TodoState，内联卡片与 composer
// 上方的 dock 面板都从这里派生渲染（不轮询）。
//
// 契约不变量：至多一个 in_progress；空 phases 表示列表被清空/从未创建。
// sub_id 缺省/null = 主会话；非空 = 某个 delegate/workflow 子会话的
// 独立清单——按 sub_id 分桶存放，与主会话互不干扰。
import type { TodoPhase, TodoTask, TodoTaskStatus } from "./api";

/** 一个会话视图的 todo 快照：主会话清单 + 各子会话分桶。 */
export interface TodoState {
  /** 主会话（sub_id 缺省/null）的全量 phases。 */
  phases: TodoPhase[];
  /** 子会话分桶：sub_id → 该子会话的全量 phases（整表替换；
   *  空 phases 的子会话直接移除桶）。 */
  subs: ReadonlyMap<string, TodoPhase[]>;
}

export function emptyTodoState(): TodoState {
  return { phases: [], subs: new Map() };
}

/** 应用一条 TodoUpdated 事件：主会话（sub_id 空）整表替换主清单；
 *  子会话（sub_id 非空）整表替换该 sub_id 的分桶（空 phases = 清掉
 *  该子会话的清单，移除桶）。主/子会话互不影响。 */
export function applyTodoUpdated(
  state: TodoState,
  payload: { phases: TodoPhase[]; sub_id?: string | null },
): TodoState {
  if (payload.sub_id) {
    const subs = new Map(state.subs);
    if (payload.phases.length === 0) subs.delete(payload.sub_id);
    else subs.set(payload.sub_id, payload.phases);
    return { phases: state.phases, subs };
  }
  return { phases: payload.phases, subs: state.subs };
}

export interface TodoCounts {
  /** status === "completed" 的任务数。 */
  done: number;
  total: number;
}

/** 任意一份 phases 清单（主会话或某个子会话桶）的计数。 */
export function countTodoPhases(phases: TodoPhase[]): TodoCounts {
  let done = 0;
  let total = 0;
  for (const p of phases) {
    const c = countPhase(p);
    done += c.done;
    total += c.total;
  }
  return { done, total };
}

/** 主会话清单计数（等价于 countTodoPhases(state.phases)）。 */
export function countTodos(state: TodoState): TodoCounts {
  return countTodoPhases(state.phases);
}

export function countPhase(phase: TodoPhase): TodoCounts {
  let done = 0;
  for (const t of phase.tasks) if (t.status === "completed") done++;
  return { done, total: phase.tasks.length };
}

/** phase 全部完成（每个任务都 completed）：展开态里这种 phase
 *  默认折叠成一行 `name 3/3`。空 phase 不算完成。 */
export function isPhaseComplete(phase: TodoPhase): boolean {
  return phase.tasks.length > 0 && phase.tasks.every((t) => t.status === "completed");
}

/** 任意一份 phases 清单的当前活动项：优先第一个 in_progress，否则
 *  第一个 pending，否则最后一个 completed；都没有（全 abandoned /
 *  空清单）返回 null。 */
export function currentTaskInPhases(
  phases: TodoPhase[],
): { phase: string; task: TodoTask } | null {
  const firstOf = (status: TodoTaskStatus): { phase: string; task: TodoTask } | null => {
    for (const p of phases) {
      for (const t of p.tasks) if (t.status === status) return { phase: p.name, task: t };
    }
    return null;
  };
  const inProgress = firstOf("in_progress");
  if (inProgress) return inProgress;
  const pending = firstOf("pending");
  if (pending) return pending;
  for (let i = phases.length - 1; i >= 0; i--) {
    const p = phases[i];
    for (let j = p.tasks.length - 1; j >= 0; j--) {
      if (p.tasks[j].status === "completed") return { phase: p.name, task: p.tasks[j] };
    }
  }
  return null;
}

/** 主会话清单的当前活动项（等价于 currentTaskInPhases(state.phases)）。 */
export function currentTask(
  state: TodoState,
): { phase: string; task: TodoTask } | null {
  return currentTaskInPhases(state.phases);
}

/** 有 todo 记录的子会话 id（按首次出现顺序）。 */
export function subTodoIds(state: TodoState): string[] {
  return [...state.subs.keys()];
}

/** 取某 sub_id 的清单；不存在时返回空数组。 */
export function subPhases(state: TodoState, subId: string): TodoPhase[] {
  return state.subs.get(subId) ?? [];
}

/** 任意一份 phases 清单的单行摘要：`📝 Todos 2/5 · 当前: <活动项>`；
 *  全部完成时 `· 全部完成`；空清单（被清空）时 `📝 Todos 已清空`。 */
export function todoSummaryLineFor(phases: TodoPhase[]): string {
  const { done, total } = countTodoPhases(phases);
  if (total === 0) return "📝 Todos 已清空";
  const head = `📝 Todos ${done}/${total}`;
  if (done === total) return `${head} · 全部完成`;
  const cur = currentTaskInPhases(phases);
  return cur ? `${head} · 当前: ${cur.task.content}` : head;
}

/** 消息流内联紧凑卡片 / dock 折叠态共用的主会话单行摘要
 *  （等价于 todoSummaryLineFor(state.phases)）。 */
export function todoSummaryLine(state: TodoState): string {
  return todoSummaryLineFor(state.phases);
}
