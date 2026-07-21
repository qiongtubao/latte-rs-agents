// StageList —— 会话视图的顶层容器：把 stageStore 里的顶层 Stage 树
// 渲染为一列 StageNode，并在某个 Stage 打开日志时（activeLogStageId
// 非空）以覆盖层挂载 LogViewer（需求 1.1、4.2、6.2）。
//
// 事件通过 api.ts 的传输层路由进 stageStore（见 subscribeEvents），
// 本组件只做最小化响应式读取：
//  - 顶层 Stage 列表（selectTopLevelStages，配合 useShallow 做浅比较，
//    避免选择器每次返回新数组导致的无限重渲染 / getSnapshot 告警）；
//  - activeLogStageId（决定是否渲染 LogViewer 覆盖层）。
//
// 该文件同时导出 `mountStageList(container)`：在给定的 DOM 容器（既有
// 的会话容器 #messages）上创建 React root 并渲染 <StageList/>，从而
// 替换原先的扁平消息列表挂载点。所有 React/JSX 逻辑都留在本 .tsx 内，
// 纯 TS 的入口 main.ts 只需调用该帮助函数。

import { type CSSProperties, type JSX } from "react";
import { createRoot, type Root } from "react-dom/client";
import { useStore } from "zustand";
import { useShallow } from "zustand/react/shallow";

import { selectTopLevelStages, stageStore } from "../stages/stageStore";
import LogViewer from "./LogViewer";
import StageNode from "./StageNode";

// 覆盖层的最小内联定位样式：保证在没有额外 CSS 的情况下 LogViewer
// 也能以居中面板 / 遮罩层的形式呈现（需求 4.2）。样式细节可后续在
// styles.css 中覆盖，这里只保证基础可用性。
const overlayStyle: CSSProperties = {
  position: "fixed",
  inset: 0,
  display: "flex",
  alignItems: "center",
  justifyContent: "center",
  background: "rgba(15, 23, 42, 0.45)",
  zIndex: 1000,
};

const panelStyle: CSSProperties = {
  position: "relative",
  maxWidth: "min(720px, 90vw)",
  maxHeight: "80vh",
  overflow: "auto",
  background: "#ffffff",
  borderRadius: 12,
  boxShadow: "0 12px 40px rgba(0, 0, 0, 0.18)",
  padding: "2.5rem 1.25rem 1.25rem",
};

const closeStyle: CSSProperties = {
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
export function StageList(): JSX.Element {
  // useShallow 让选择器在数组内容浅相等时复用旧引用，避免
  // useSyncExternalStore 因每次返回新数组而反复重渲染。
  const stages = useStore(stageStore, useShallow(selectTopLevelStages));
  const activeLogStageId = useStore(
    stageStore,
    (state) => state.activeLogStageId,
  );

  function handleCloseLog(): void {
    stageStore.getState().closeLog();
  }

  return (
    <div className="stage-list" data-testid="stage-list">
      {stages.map((stage) => (
        <StageNode key={stage.id} stageId={stage.id} />
      ))}

      {activeLogStageId ? (
        <div
          className="stage-list__log-overlay"
          data-testid="log-overlay"
          role="dialog"
          aria-modal="true"
          style={overlayStyle}
          onClick={handleCloseLog}
        >
          <div
            className="stage-list__log-panel"
            style={panelStyle}
            onClick={(event) => event.stopPropagation()}
          >
            <button
              type="button"
              className="stage-list__log-close"
              aria-label="关闭日志"
              data-testid="log-viewer-close"
              style={closeStyle}
              onClick={handleCloseLog}
            >
              ×
            </button>
            <LogViewer stageId={activeLogStageId} />
          </div>
        </div>
      ) : null}
    </div>
  );
}

/**
 * 在给定 DOM 容器上创建 React root 并渲染 <StageList/>。
 * 供 main.ts（纯 TS 入口）替换原扁平消息列表挂载点时调用。
 */
export function mountStageList(container: HTMLElement): Root {
  const root = createRoot(container);
  root.render(<StageList />);
  return root;
}

export default StageList;
