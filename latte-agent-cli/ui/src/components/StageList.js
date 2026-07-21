import { jsx as _jsx, jsxs as _jsxs } from "react/jsx-runtime";
import { createRoot } from "react-dom/client";
import { useStore } from "zustand";
import { useShallow } from "zustand/react/shallow";
import { selectTopLevelStages, stageStore } from "../stages/stageStore";
import LogViewer from "./LogViewer";
import StageNode from "./StageNode";
// 覆盖层的最小内联定位样式：保证在没有额外 CSS 的情况下 LogViewer
// 也能以居中面板 / 遮罩层的形式呈现（需求 4.2）。样式细节可后续在
// styles.css 中覆盖，这里只保证基础可用性。
const overlayStyle = {
    position: "fixed",
    inset: 0,
    display: "flex",
    alignItems: "center",
    justifyContent: "center",
    background: "rgba(15, 23, 42, 0.45)",
    zIndex: 1000,
};
const panelStyle = {
    position: "relative",
    maxWidth: "min(720px, 90vw)",
    maxHeight: "80vh",
    overflow: "auto",
    background: "#ffffff",
    borderRadius: 12,
    boxShadow: "0 12px 40px rgba(0, 0, 0, 0.18)",
    padding: "2.5rem 1.25rem 1.25rem",
};
const closeStyle = {
    position: "absolute",
    top: 8,
    right: 8,
};
/**
 * 渲染会话的顶层 Stage 列表。
 *
 * - 按 store 的 `order` 顺序，将每个顶层 Stage 渲染为 <StageNode>
 *   （需求 1.1、6.2）；StageNode 内部递归渲染其 Subagent_Stage 子树。
 * - 当某个 Stage 打开日志时（activeLogStageId 非空），以覆盖层渲染
 *   LogViewer；点击遮罩或关闭按钮调用 closeLog（需求 4.2）。
 */
export function StageList() {
    // useShallow 让选择器在数组内容浅相等时复用旧引用，避免
    // useSyncExternalStore 因每次返回新数组而反复重渲染。
    const stages = useStore(stageStore, useShallow(selectTopLevelStages));
    const activeLogStageId = useStore(stageStore, (state) => state.activeLogStageId);
    function handleCloseLog() {
        stageStore.getState().closeLog();
    }
    return (_jsxs("div", { className: "stage-list", "data-testid": "stage-list", children: [stages.map((stage) => (_jsx(StageNode, { stageId: stage.id }, stage.id))), activeLogStageId ? (_jsx("div", { className: "stage-list__log-overlay", "data-testid": "log-overlay", role: "dialog", "aria-modal": "true", style: overlayStyle, onClick: handleCloseLog, children: _jsxs("div", { className: "stage-list__log-panel", style: panelStyle, onClick: (event) => event.stopPropagation(), children: [_jsx("button", { type: "button", className: "stage-list__log-close", "aria-label": "\u5173\u95ED\u65E5\u5FD7", "data-testid": "log-viewer-close", style: closeStyle, onClick: handleCloseLog, children: "\u00D7" }), _jsx(LogViewer, { stageId: activeLogStageId })] }) })) : null] }));
}
/**
 * 在给定 DOM 容器上创建 React root 并渲染 <StageList/>。
 * 供 main.ts（纯 TS 入口）替换原扁平消息列表挂载点时调用。
 */
export function mountStageList(container) {
    const root = createRoot(container);
    root.render(_jsx(StageList, {}));
    return root;
}
export default StageList;
