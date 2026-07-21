// StageNode —— 递归渲染单个 Stage 及其嵌套的 Subagent_Stage 子树。
//
// 每个 StageNode 只以 `stageId` 寻址，从共享的 stageStore 单例中做
// 最小化响应式读取（useStore + selectStageById），并渲染：
//  - 该 Stage 的 ProgressBubble；
//  - 一个 onContextMenu（右键）处理器：在光标位置打开 ContextMenu，
//    以便查看该 Stage 的日志（需求 7.1，历史/嵌套一致，见需求 4.x）；
//  - 当 `childIds.length > 0` 时，一个展开/折叠控件，调用
//    `toggleExpanded(stageId)`（需求 6.2、6.4）。
//
// 当 `expanded` 为真时，为每个子 id 递归渲染 `<StageNode>`（需求 6.2、
// 6.3）；折叠时隐藏子节点（需求 6.4）。递归在叶子 Stage（childIds 为
// 空）处自然终止。
//
// 菜单的打开状态与坐标保存在组件本地 state 中；仅当菜单打开时才渲染
// ContextMenu，关闭回调会清除该状态。

import { useState, type JSX, type MouseEvent } from "react";
import { useStore } from "zustand";

import { selectStageById, stageStore } from "../stages/stageStore";
import ContextMenu from "./ContextMenu";
import ProgressBubble from "./ProgressBubble";

export interface StageNodeProps {
  /** 要渲染的 Stage id（唯一寻址键）。 */
  stageId: string;
}

/** ContextMenu 的本地打开状态：视口坐标 (x, y)。 */
interface MenuState {
  x: number;
  y: number;
}

/**
 * 递归渲染一个 Stage 节点。
 *
 * - 通过响应式选择器读取 Stage；若该 id 不存在则不渲染任何内容。
 * - 右键在光标位置打开 ContextMenu（需求 7.1）。
 * - 有子节点时提供展开/折叠控件（需求 6.2、6.4），展开时递归渲染
 *   每个 Subagent_Stage（需求 6.2、6.3）。
 */
export function StageNode({ stageId }: StageNodeProps): JSX.Element | null {
  const stage = useStore(stageStore, selectStageById(stageId));
  const [menu, setMenu] = useState<MenuState | null>(null);

  // Stage 尚未存在（例如异步创建前）时不渲染。
  if (!stage) return null;

  const hasChildren = stage.childIds.length > 0;

  function handleContextMenu(event: MouseEvent): void {
    // 阻止浏览器原生右键菜单，改为在光标处打开我们的 ContextMenu。
    event.preventDefault();
    event.stopPropagation();
    setMenu({ x: event.clientX, y: event.clientY });
  }

  function handleToggle(): void {
    stageStore.getState().toggleExpanded(stageId);
  }

  return (
    <div
      className="stage-node"
      data-stage-id={stageId}
      data-depth={stage.depth}
    >
      <div className="stage-node__row" onContextMenu={handleContextMenu}>
        {hasChildren ? (
          <button
            type="button"
            className="stage-node__disclosure"
            aria-expanded={stage.expanded}
            aria-label={stage.expanded ? "折叠" : "展开"}
            data-testid="stage-node-disclosure"
            onClick={handleToggle}
          >
            {stage.expanded ? "▾" : "▸"}
          </button>
        ) : null}
        <ProgressBubble stage={stage} />
      </div>

      {hasChildren && stage.expanded ? (
        <div className="stage-node__children" data-testid="stage-node-children">
          {stage.childIds.map((childId) => (
            <StageNode key={childId} stageId={childId} />
          ))}
        </div>
      ) : null}

      {menu ? (
        <ContextMenu
          stageId={stageId}
          x={menu.x}
          y={menu.y}
          onClose={() => setMenu(null)}
        />
      ) : null}
    </div>
  );
}

export default StageNode;
