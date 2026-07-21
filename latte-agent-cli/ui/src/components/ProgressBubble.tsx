// ProgressBubble —— 进行中 → 最终回复的“就地”渲染组件。
//
// 一个 Stage 在运行时，本组件显示最新事件文本 + 一个 spinner；
// 当 Stage 完成（或出错）后，同一个组件实例改为渲染最终 content
// 并移除 spinner。父层用稳定的 `stage.id` 作为 key，因此 React 会
// 就地更新已有 DOM 节点，而不是卸载/重建，保证会话流不跳动、不重复。
//
// 注意：本组件不触碰 `stage.log`（Event_Trace / Model_Exchange 由
// reducer 维护并在 progress→reply 转换后保持不变，见需求 3.3）。

import type { JSX } from "react";

import type { Stage } from "../stages/types";

export interface ProgressBubbleProps {
  /** 要渲染的 Stage；其 status 决定进行中还是已完成的呈现。 */
  stage: Stage;
}

/**
 * 渲染单个 Stage 的气泡。
 *
 * - `in_progress`：显示 `currentEventText` 与 spinner（需求 2.1、2.2）。
 *   reducer 每次更新 `currentEventText` 都会重渲染文本（需求 2.3）。
 * - `complete` / `error`：显示 `content`，移除 spinner（需求 3.1、3.2）。
 *
 * spinner 当且仅当 status 为 `in_progress` 时出现（设计属性 4）。
 */
export function ProgressBubble({ stage }: ProgressBubbleProps): JSX.Element {
  const inProgress = stage.status === "in_progress";

  return (
    <div
      className="progress-bubble"
      data-stage-id={stage.id}
      data-status={stage.status}
    >
      {inProgress ? (
        <div className="progress-bubble__progress">
          <span
            className="progress-bubble__spinner"
            role="status"
            aria-live="polite"
            aria-label="进行中"
            data-testid="progress-spinner"
          />
          <span className="progress-bubble__event-text">
            {stage.currentEventText}
          </span>
        </div>
      ) : (
        <div className="progress-bubble__content">{stage.content}</div>
      )}
    </div>
  );
}

export default ProgressBubble;
