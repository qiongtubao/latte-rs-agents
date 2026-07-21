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
import { createStore } from "zustand/vanilla";
import { createContext, emptyStore, stageReducer, } from "./stageReducer";
/**
 * 创建一个独立的 StageStore 实例。
 *
 * @param now 取 TraceRecord.timestampMs 的时钟；测试/重放可注入以获得
 *            确定性时间戳。默认 `Date.now`。
 */
export function createStageStore(now = Date.now) {
    // reducer 的委派栈上下文保存在闭包里，跨多次 applyEvent 调用串联。
    // 它不是可渲染状态，因此刻意不放进 store 的 state。
    let ctx = createContext(now);
    return createStore()((set, get) => ({
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
            if (!stage)
                return;
            set({
                byId: { ...byId, [id]: { ...stage, expanded: !stage.expanded } },
            });
        },
        openLog: (id) => set({ activeLogStageId: id }),
        closeLog: () => set({ activeLogStageId: null }),
    }));
}
/** 应用共享的默认 StageStore 单例。 */
export const stageStore = createStageStore();
// ── 选择器（selectors） ────────────────────────────────────────────
//
// 选择器是纯函数 state -> 派生值，便于视图层做最小化订阅，也便于测试。
/** 选择顶层 Stage（按 order 顺序），跳过缺失的 id。 */
export function selectTopLevelStages(state) {
    return state.order
        .map((id) => state.byId[id])
        .filter((stage) => stage !== undefined);
}
/** 选择指定 id 的 Stage（不存在时返回 undefined）。 */
export function selectStageById(id) {
    return (state) => state.byId[id];
}
/** 选择指定 Stage 的 Stage_Log（Stage 不存在时返回 undefined）。 */
export function selectStageLog(id) {
    return (state) => state.byId[id]?.log;
}
