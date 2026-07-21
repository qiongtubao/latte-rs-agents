// LogViewer —— 展示单个 Stage 的 Stage_Log。
//
// 通过一个分段控件 (segmented control) 在两个日志区域之间切换：
//  - Event_Trace：按接收顺序 (seq) 列出该 Stage 的所有事件记录，
//    每行显示 kind、时间戳，并可展开查看完整的原始负载 (需求 5.1、5.4)。
//  - Model_Exchange：分组展示发送给模型的 prompt 与原始 response
//    (需求 5.2)。
//
// 分段控件把当前 LogView 保存在组件本地状态里 (需求 5.3)。
// 当该 Stage 没有任何记录时（Event_Trace 为空且 Model_Exchange 的
// prompt/response 均为空），渲染空状态提示 (需求 4.4)。
//
// 组件通过共享的 stageStore 单例与 selectStageLog 选择器读取
// `store.byId[stageId].log`，从而在日志更新时做最小化重渲染。

import { useState, type JSX } from "react";
import { useStore } from "zustand";

import { selectStageLog, stageStore } from "../stages/stageStore";
import type { LogView, TraceRecord } from "../stages/types";

export interface LogViewerProps {
  /** 要展示日志的 Stage id。 */
  stageId: string;
}

/** 把接收时间戳格式化为稳定、可读的字符串。 */
function formatTimestamp(timestampMs: number): string {
  return new Date(timestampMs).toISOString();
}

/** 把任意原始负载序列化为可读的 JSON 文本（对不可序列化值兜底）。 */
function formatRaw(raw: unknown): string {
  try {
    return JSON.stringify(raw, null, 2);
  } catch {
    return String(raw);
  }
}

/** Event_Trace 中的单行：显示 kind / 时间戳，并可展开原始负载。 */
function TraceRow({ record }: { record: TraceRecord }): JSX.Element {
  const [expanded, setExpanded] = useState(false);

  return (
    <li className="log-viewer__trace-row" data-seq={record.seq}>
      <button
        type="button"
        className="log-viewer__trace-summary"
        aria-expanded={expanded}
        onClick={() => setExpanded((v) => !v)}
      >
        <span className="log-viewer__trace-kind">{record.kind}</span>
        <span className="log-viewer__trace-timestamp">
          {formatTimestamp(record.timestampMs)}
        </span>
        {record.summary ? (
          <span className="log-viewer__trace-label">{record.summary}</span>
        ) : null}
      </button>
      {expanded ? (
        <pre className="log-viewer__trace-raw" data-testid="trace-raw">
          {formatRaw(record.raw)}
        </pre>
      ) : null}
    </li>
  );
}

/**
 * 渲染指定 Stage 的 Log_Viewer。
 *
 * 若该 Stage 不存在或没有任何日志记录，渲染空状态提示 (需求 4.4)。
 */
export function LogViewer({ stageId }: LogViewerProps): JSX.Element {
  const log = useStore(stageStore, selectStageLog(stageId));
  const [view, setView] = useState<LogView>("event_trace");

  const hasEventTrace = (log?.eventTrace.length ?? 0) > 0;
  const hasModelExchange = Boolean(
    log && (log.modelExchange.prompt || log.modelExchange.response),
  );

  // 空状态：既无 Event_Trace 记录，也无 Model_Exchange 内容 (需求 4.4)。
  if (!log || (!hasEventTrace && !hasModelExchange)) {
    return (
      <div className="log-viewer log-viewer--empty" data-stage-id={stageId}>
        <p className="log-viewer__empty" data-testid="log-viewer-empty">
          No logs available for this stage
        </p>
      </div>
    );
  }

  // 按 seq（接收顺序）排序，保证 Event_Trace 展示顺序稳定 (需求 5.4)。
  const orderedTrace = [...log.eventTrace].sort((a, b) => a.seq - b.seq);

  return (
    <div className="log-viewer" data-stage-id={stageId}>
      {/* 分段控件：在 Event_Trace 与 Model_Exchange 之间切换 (需求 5.3)。 */}
      <div
        className="log-viewer__toggle"
        role="tablist"
        aria-label="Log view"
      >
        <button
          type="button"
          role="tab"
          className="log-viewer__toggle-btn"
          aria-selected={view === "event_trace"}
          data-active={view === "event_trace"}
          data-testid="toggle-event-trace"
          onClick={() => setView("event_trace")}
        >
          Event Trace
        </button>
        <button
          type="button"
          role="tab"
          className="log-viewer__toggle-btn"
          aria-selected={view === "model_exchange"}
          data-active={view === "model_exchange"}
          data-testid="toggle-model-exchange"
          onClick={() => setView("model_exchange")}
        >
          Model Exchange
        </button>
      </div>

      {view === "event_trace" ? (
        <ol className="log-viewer__trace" data-testid="event-trace-view">
          {orderedTrace.map((record) => (
            <TraceRow key={record.seq} record={record} />
          ))}
        </ol>
      ) : (
        <div
          className="log-viewer__exchange"
          data-testid="model-exchange-view"
        >
          <section className="log-viewer__exchange-region">
            <h4 className="log-viewer__exchange-heading">Prompt</h4>
            <pre className="log-viewer__exchange-prompt" data-testid="exchange-prompt">
              {log.modelExchange.prompt}
            </pre>
          </section>
          <section className="log-viewer__exchange-region">
            <h4 className="log-viewer__exchange-heading">Response</h4>
            <pre
              className="log-viewer__exchange-response"
              data-testid="exchange-response"
            >
              {log.modelExchange.response}
            </pre>
          </section>
        </div>
      )}
    </div>
  );
}

export default LogViewer;
