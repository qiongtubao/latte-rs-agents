// todo 进度的纯状态模块（无 DOM 依赖，可在 node 环境单测）。
//
// 后端每次完成 todo 工具调用后广播 TodoUpdated 事件，携带**全量**
// phases（整表替换语义，不需要 diff）。chat_impl.ts 的事件分发把事件
// 交给 applyTodoUpdated 归约成本地 TodoState，内联卡片与 composer
// 上方的 dock 面板都从这里派生渲染（不轮询）。
//
// 契约不变量：至多一个 in_progress；空 phases 表示列表被清空/从未创建。
// sub_id 非空（delegate/workflow 子会话）的清单暂不展示：applyTodoUpdated
// 原样返回旧 state（引用不变），调用方据此跳过重渲染。
import type { TodoPhase, TodoTask, TodoTaskStatus } from "./api";

/** 一个会话的 todo 清单快照。 */
export interface TodoState {
  phases: TodoPhase[];
}

export function emptyTodoState(): TodoState {
  return { phases: [] };
}

/** 应用一条 TodoUpdated 事件：主会话（sub_id 空）整表替换；子会话
 *  （sub_id 非空）忽略，原样返回旧 state。空 phases = 列表被清空。 */
export function applyTodoUpdated(
  state: TodoState,
  payload: { phases: TodoPhase[]; sub_id?: string | null },
): TodoState {
  if (payload.sub_id) return state;
  return { phases: payload.phases };
}

export interface TodoCounts {
  /** status === "completed" 的任务数。 */
  done: number;
  total: number;
}

export function countTodos(state: TodoState): TodoCounts {
  let done = 0;
  let total = 0;
  for (const p of state.phases) {
    const c = countPhase(p);
    done += c.done;
    total += c.total;
  }
  return { done, total };
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

/** 当前活动项：优先第一个 in_progress，否则第一个 pending，否则最后
 *  一个 completed；都没有（全 abandoned / 空清单）返回 null。 */
export function currentTask(
  state: TodoState,
): { phase: string; task: TodoTask } | null {
  const firstOf = (status: TodoTaskStatus): { phase: string; task: TodoTask } | null => {
    for (const p of state.phases) {
      for (const t of p.tasks) if (t.status === status) return { phase: p.name, task: t };
    }
    return null;
  };
  const inProgress = firstOf("in_progress");
  if (inProgress) return inProgress;
  const pending = firstOf("pending");
  if (pending) return pending;
  for (let i = state.phases.length - 1; i >= 0; i--) {
    const p = state.phases[i];
    for (let j = p.tasks.length - 1; j >= 0; j--) {
      if (p.tasks[j].status === "completed") return { phase: p.name, task: p.tasks[j] };
    }
  }
  return null;
}

/** 消息流内联紧凑卡片 / dock 折叠态共用的单行摘要：
 *  `📝 Todos 2/5 · 当前: <活动项>`；全部完成时 `· 全部完成`；
 *  空清单（被清空）时 `📝 Todos 已清空`。 */
export function todoSummaryLine(state: TodoState): string {
  const { done, total } = countTodos(state);
  if (total === 0) return "📝 Todos 已清空";
  const head = `📝 Todos ${done}/${total}`;
  if (done === total) return `${head} · 全部完成`;
  const cur = currentTask(state);
  return cur ? `${head} · 当前: ${cur.task.content}` : head;
}
