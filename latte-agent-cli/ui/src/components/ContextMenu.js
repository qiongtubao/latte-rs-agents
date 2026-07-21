import { jsx as _jsx } from "react/jsx-runtime";
// ContextMenu —— 消息右键菜单，提供“查看日志”动作。
//
// 任意一条消息（当前的、历史的，或任意深度的 Subagent_Stage）右键时
// 弹出本菜单。它只以 `stageId` 作为寻址键，因此对最近的 Stage、历史
// Stage 以及嵌套的 Subagent_Stage 行为完全一致，且不依赖 Stage 的
// status 或它在历史中的位置（需求 4.1、4.2、4.3、7.1、7.2）。
//
// 激活“查看日志”会调用 store 的 `openLog(stageId)` 打开 Log_Viewer，
// 随后关闭菜单。点击菜单外部或选择动作都会触发关闭（dismiss）。
import { useEffect, useRef } from "react";
import { stageStore } from "../stages/stageStore";
/**
 * 渲染右键上下文菜单。
 *
 * - 在给定的屏幕坐标 (x, y) 以 fixed 定位呈现。
 * - 提供唯一动作 “查看日志”，调用 `openLog(stageId)`（需求 4.1、4.2）。
 *   该动作不因 Stage 的 status 或历史位置而被禁用，故对最近 /
 *   历史 / 嵌套 Subagent_Stage 一致可用（需求 4.3、7.1、7.2）。
 * - 点击菜单外部或选择动作后调用 `onClose` 消隐菜单。
 */
export function ContextMenu({ stageId, x, y, onClose, store = stageStore, }) {
    const menuRef = useRef(null);
    // 外部点击 / 按下 Escape 时消隐菜单。
    useEffect(() => {
        function handlePointerDown(event) {
            const node = menuRef.current;
            if (node && !node.contains(event.target)) {
                onClose();
            }
        }
        function handleKeyDown(event) {
            if (event.key === "Escape") {
                onClose();
            }
        }
        document.addEventListener("mousedown", handlePointerDown);
        document.addEventListener("keydown", handleKeyDown);
        return () => {
            document.removeEventListener("mousedown", handlePointerDown);
            document.removeEventListener("keydown", handleKeyDown);
        };
    }, [onClose]);
    function handleViewLogs() {
        // 只以 stageId 寻址：对任意 status / 历史位置 / 嵌套深度一致。
        store.getState().openLog(stageId);
        onClose();
    }
    return (_jsx("div", { ref: menuRef, className: "context-menu", role: "menu", "data-stage-id": stageId, style: { position: "fixed", top: y, left: x }, children: _jsx("button", { type: "button", role: "menuitem", className: "context-menu__item", "data-testid": "context-menu-view-logs", onClick: handleViewLogs, children: "\u67E5\u770B\u65E5\u5FD7" }) }));
}
export default ContextMenu;
