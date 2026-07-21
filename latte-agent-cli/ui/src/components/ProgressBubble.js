import { jsx as _jsx, jsxs as _jsxs } from "react/jsx-runtime";
/**
 * 渲染单个 Stage 的气泡。
 *
 * - `in_progress`：显示 `currentEventText` 与 spinner（需求 2.1、2.2）。
 *   reducer 每次更新 `currentEventText` 都会重渲染文本（需求 2.3）。
 * - `complete` / `error`：显示 `content`，移除 spinner（需求 3.1、3.2）。
 *
 * spinner 当且仅当 status 为 `in_progress` 时出现（设计属性 4）。
 */
export function ProgressBubble({ stage }) {
    const inProgress = stage.status === "in_progress";
    return (_jsx("div", { className: "progress-bubble", "data-stage-id": stage.id, "data-status": stage.status, children: inProgress ? (_jsxs("div", { className: "progress-bubble__progress", children: [_jsx("span", { className: "progress-bubble__spinner", role: "status", "aria-live": "polite", "aria-label": "\u8FDB\u884C\u4E2D", "data-testid": "progress-spinner" }), _jsx("span", { className: "progress-bubble__event-text", children: stage.currentEventText })] })) : (_jsx("div", { className: "progress-bubble__content", children: stage.content })) }));
}
export default ProgressBubble;
