import { jsx as _jsx, jsxs as _jsxs } from "react/jsx-runtime";
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
import { useState } from "react";
import { useStore } from "zustand";
import { selectStageById, stageStore } from "../stages/stageStore";
import ContextMenu from "./ContextMenu";
import ProgressBubble from "./ProgressBubble";
/**
 * 递归渲染一个 Stage 节点。
 *
 * - 通过响应式选择器读取 Stage；若该 id 不存在则不渲染任何内容。
 * - 右键在光标位置打开 ContextMenu（需求 7.1）。
 * - 有子节点时提供展开/折叠控件（需求 6.2、6.4），展开时递归渲染
 *   每个 Subagent_Stage（需求 6.2、6.3）。
 */
export function StageNode({ stageId }) {
    const stage = useStore(stageStore, selectStageById(stageId));
    const [menu, setMenu] = useState(null);
    // Stage 尚未存在（例如异步创建前）时不渲染。
    if (!stage)
        return null;
    const hasChildren = stage.childIds.length > 0;
    function handleContextMenu(event) {
        // 阻止浏览器原生右键菜单，改为在光标处打开我们的 ContextMenu。
        event.preventDefault();
        event.stopPropagation();
        setMenu({ x: event.clientX, y: event.clientY });
    }
    function handleToggle() {
        stageStore.getState().toggleExpanded(stageId);
    }
    return (_jsxs("div", { className: "stage-node", "data-stage-id": stageId, "data-depth": stage.depth, children: [_jsxs("div", { className: "stage-node__row", onContextMenu: handleContextMenu, children: [hasChildren ? (_jsx("button", { type: "button", className: "stage-node__disclosure", "aria-expanded": stage.expanded, "aria-label": stage.expanded ? "折叠" : "展开", "data-testid": "stage-node-disclosure", onClick: handleToggle, children: stage.expanded ? "▾" : "▸" })) : null, _jsx(ProgressBubble, { stage: stage })] }), hasChildren && stage.expanded ? (_jsx("div", { className: "stage-node__children", "data-testid": "stage-node-children", children: stage.childIds.map((childId) => (_jsx(StageNode, { stageId: childId }, childId))) })) : null, menu ? (_jsx(ContextMenu, { stageId: stageId, x: menu.x, y: menu.y, onClose: () => setMenu(null) })) : null] }));
}
export default StageNode;
