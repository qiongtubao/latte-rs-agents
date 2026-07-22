// stageStore —— 把纯函数 `stageReducer` 包装进一个可订阅的状态容器。
//
// 本仓库没有 React，故使用 zustand 的 vanilla 入口 (`zustand/vanilla`
// 的 `createStore`)，它提供 `getState` / `setState` / `subscribe`，
// 既能被后续的视图层订阅，也能在纯测试中直接驱动。
//
// 设计要点：
//  - store 只持有可渲染状态（order / byId / activeLogStageId）。
//  - reducer 需要在多次 applyEvent 之间串联的委派栈上下文
//    (ReducerContext) 不属于可渲染状态，故保存在工厂闭包里，
//    从而保持 reducer 的“纯”用法：store 在外部替 reducer 管理 ctx。
//  - `applyEvent(event)` 处理实时事件流；`seed(events)` 折叠一段事件
//    数组来初始化状态（供 JSONL 重放使用，见任务 4.2），并把内部
//    ctx 重置为重放结束后的状态，使随后的实时事件能正确衔接。

import { createStore, type StoreApi } from "zustand/vanilla";

import type { ChatEvent } from "../api";
import {
  createContext,
  emptyStore,
  stageReducer,
  type ReducerContext,
} from "./stageReducer";
import type { Stage, StageLog } from "./types";

/**
 * 可订阅的 Stage 状态：reducer 折叠出的 Stage 树（order / byId）加上
 * Log_Viewer 的打开目标（activeLogStageId），以及一组变更动作。
 */
export interface StageState {
  /** 顶层 stage id，按显示顺序排列。 */
  order: string[];
  /** 扁平的 Stage 映射；通过 childIds 遍历树。 */
  byId: Record<string, Stage>;
  /** 当前在 Log_Viewer 中打开的 Stage id；无则为 null。 */
  activeLogStageId: string | null;

  // ── 动作（actions） ──────────────────────────────────────────

  /** 把单个实时事件折叠进 store（内部串联 ReducerContext）。 */
  applyEvent: (event: ChatEvent) => void;
  /** 折叠一段有序事件数组以初始化状态（JSONL 重放入口）。 */
  seed: (events: ChatEvent[]) => void;
  /** 切换某个 Stage 的子节点展开/折叠状态。 */
  toggleExpanded: (id: string) => void;
  /** 打开某个 Stage 的 Log_Viewer（设置 activeLogStageId）。 */
  openLog: (id: string) => void;
  /** 关闭 Log_Viewer（清除 activeLogStageId）。 */
  closeLog: () => void;
}

/** StageStore 的类型别名（zustand vanilla store 句柄）。 */
export type StageStore = StoreApi<StageState>;

/**
 * 创建一个独立的 StageStore 实例。
 *
 * @param now 取 TraceRecord.timestampMs 的时钟；测试/重放可注入以获得
 *            确定性时间戳。默认 `Date.now`。
 */
export function createStageStore(now: () => number = Date.now): StageStore {
  // reducer 的委派栈上下文保存在闭包里，跨多次 applyEvent 调用串联。
  // 它不是可渲染状态，因此刻意不放进 store 的 state。
  let ctx: ReducerContext = createContext(now);

  return createStore<StageState>()((set, get) => ({
    order: [],
    byId: {},
    activeLogStageId: null,

    applyEvent: (event) => {
      const { order, byId } = get();
      const result = stageReducer({ order, byId }, event, ctx);
      // 在外部替 reducer 保存推进后的 ctx（保持 reducer 纯函数用法）。
      ctx = result.ctx;
      set({ order: result.store.order, byId: result.store.byId });
    },

    seed: (events) => {
      let store = emptyStore();
      let c = createContext(now);
      for (const event of events) {
        const result = stageReducer(store, event, c);
        store = result.store;
        c = result.ctx;
      }
      // 重放结束后的 ctx 成为后续实时事件的起点。
      ctx = c;
      set({
        order: store.order,
        byId: store.byId,
        activeLogStageId: null,
      });
    },

    toggleExpanded: (id) => {
      const { byId } = get();
      const stage = byId[id];
      if (!stage) return;
      set({
        byId: { ...byId, [id]: { ...stage, expanded: !stage.expanded } },
      });
    },

    openLog: (id) => set({ activeLogStageId: id }),

    closeLog: () => set({ activeLogStageId: null }),
  }));
}

/** 应用共享的默认 StageStore 单例。 */
export const stageStore: StageStore = createStageStore();

/**
 * 用指定 session 的归档事件替换当前可见 Stage 列表。
 *
 * @param events 后端按时间顺序返回的完整 ChatEvent 历史。
 * @returns 无返回值；共享 store 更新后由 StageList 订阅并重新渲染。
 */
export function replayStageHistory(events: ChatEvent[]): void {
  stageStore.getState().seed(events);
}

// ── 选择器（selectors） ────────────────────────────────────────────
//
// 选择器是纯函数 state -> 派生值，便于视图层做最小化订阅，也便于测试。

/** 选择顶层 Stage（按 order 顺序），跳过缺失的 id。 */
export function selectTopLevelStages(state: StageState): Stage[] {
  return state.order
    .map((id) => state.byId[id])
    .filter((stage): stage is Stage => stage !== undefined);
}

/** 选择指定 id 的 Stage（不存在时返回 undefined）。 */
export function selectStageById(
  id: string,
): (state: StageState) => Stage | undefined {
  return (state) => state.byId[id];
}

/** 选择指定 Stage 的 Stage_Log（Stage 不存在时返回 undefined）。 */
export function selectStageLog(
  id: string,
): (state: StageState) => StageLog | undefined {
  return (state) => state.byId[id]?.log;
}
