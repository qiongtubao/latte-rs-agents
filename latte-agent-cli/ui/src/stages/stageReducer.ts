// stageReducer —— 把有序的 ChatEvent 事件流纯函数式地折叠成 StageStore。
//
// reducer 是一个纯函数：不修改任何入参，返回全新的 store（以及推进后的
// ctx）。因为折叠是纯的，同一 reducer 既可以处理实时事件流，也可以从
// 持久化的 JSONL 记录逐条重放来重建整棵 Stage 树。
//
// 本文件按 event.type 做 switch 分发，方便后续任务把 委派/嵌套
// Subagent_Stage (2.3) 与防御式错误处理 (2.4) 平滑地接入到同一处。
// 为保证折叠是全函数 (total)，未识别或暂未实现的事件都走 no-op 分支，
// 绝不静默丢弃到会破坏折叠的程度。

import type { ChatEvent } from "../api";
import {
  createStage,
  type Stage,
  type StageStore,
  type TraceRecord,
} from "./types";

/**
 * reducer 的上下文：跟踪当前的委派栈（活跃 stage id 列表，最内层在末尾）
 * 以及生成唯一 stage id 的计数器和取时间戳的时钟。
 *
 * ctx 与 store 分离：store 只描述已折叠出的 Stage 树，ctx 描述折叠过程中
 * "当前活跃到哪个 Stage" 这一瞬态信息。reducer 会返回推进后的新 ctx，
 * 由 `foldEvents` 在内部串联。
 */
export interface ReducerContext {
  /** 活跃 stage id 的栈，最内层（最近委派）在末尾。 */
  stack: string[];
  /** 用于生成唯一 stage id 的单调计数器。 */
  nextId: number;
  /** 取 TraceRecord.timestampMs 的时钟（便于测试/重放注入）。 */
  now: () => number;
}

/** reducer 的返回值：新的 store 与推进后的 ctx。 */
export interface ReducerResult {
  store: StageStore;
  ctx: ReducerContext;
}

/** 创建一个空的 StageStore。 */
export function emptyStore(): StageStore {
  return { order: [], byId: {} };
}

/** 创建一个初始的 ReducerContext。 */
export function createContext(now: () => number = Date.now): ReducerContext {
  return { stack: [], nextId: 0, now };
}

/** 截断过长文本，保持 Progress_Bubble / 摘要简短。 */
function truncate(text: string, max = 200): string {
  const t = text.replace(/\s+/g, " ").trim();
  return t.length > max ? t.slice(0, max - 1) + "…" : t;
}

/** 为一个事件生成简短的人类可读摘要（驱动 Progress_Bubble 与 Event_Trace）。 */
export function summarize(event: ChatEvent): string {
  switch (event.type) {
    case "Status":
      return truncate(event.message);
    case "ToolUse":
      return truncate(`🔧 ${event.tool_name} ${event.args}`);
    case "ToolResult":
      return truncate(`✓ ${event.tool_name} → ${event.result}`);
    case "ToolError":
      return truncate(`✗ ${event.tool_name}: ${event.error}`);
    case "Prompt":
      return truncate(`${event.icon} ${event.role_id} (${event.model_id})`);
    case "RoleStarted":
      return truncate(`${event.role_id}${event.detail ? `: ${event.detail}` : ""}`);
    case "RoleTurn":
      // 流式回复增量：以内容摘要作为当前事件文本（内容为空时回退到类型名）。
      return event.content ? truncate(event.content) : "RoleTurn";
    case "RoundStarted":
      return `Round ${event.round}`;
    case "DelegateStarted":
      return truncate(`↗ ${event.from_role} → ${event.to_role}: ${event.task}`);
    case "DelegateFinished":
      return truncate(`↙ ${event.to_role} → ${event.from_role}: ${event.summary}`);
    case "Error":
      return truncate(`✗ ${event.message}`);
    default:
      return event.type;
  }
}

/** 为一个新建 Stage 生成标题。 */
function titleFor(event: ChatEvent): string {
  switch (event.type) {
    case "RoleStarted":
      return event.role_id;
    case "Prompt":
      return event.role_id;
    case "RoundStarted":
      return `Round ${event.round}`;
    default:
      return "";
  }
}

/** 从事件中取 role_id（若有）。 */
function roleIdOf(event: ChatEvent): string {
  return "role_id" in event && typeof event.role_id === "string"
    ? event.role_id
    : "";
}

/** 用一个新的 Stage 替换 store 中同 id 的节点（不改变 order / 其它节点）。 */
function putStage(store: StageStore, stage: Stage): StageStore {
  return {
    order: store.order,
    byId: { ...store.byId, [stage.id]: stage },
  };
}

/** 取当前栈顶 Stage（无活跃 Stage 时返回 undefined）。 */
function topStage(store: StageStore, ctx: ReducerContext): Stage | undefined {
  const topId = ctx.stack[ctx.stack.length - 1];
  if (!topId) return undefined;
  return store.byId[topId];
}

/** 创建一个顶层 Stage 并把它压入 order 与委派栈。 */
function createTopLevelStage(
  store: StageStore,
  event: ChatEvent,
  ctx: ReducerContext,
): ReducerResult {
  const id = `stage-${ctx.nextId}`;
  const stage = createStage({
    id,
    roleId: roleIdOf(event),
    parentId: null,
    depth: 0,
    title: titleFor(event),
    status: "in_progress",
    currentEventText: summarize(event),
  });
  const store2: StageStore = {
    order: [...store.order, id],
    byId: { ...store.byId, [id]: stage },
  };
  const ctx2: ReducerContext = {
    ...ctx,
    stack: [...ctx.stack, id],
    nextId: ctx.nextId + 1,
  };
  return { store: store2, ctx: ctx2 };
}

/**
 * 向栈顶 Stage 追加一条 TraceRecord，并把 currentEventText 更新为该摘要。
 * 若当前没有活跃 Stage（栈为空），忽略事件以保持折叠是全函数。
 */
function appendTrace(
  store: StageStore,
  event: ChatEvent,
  ctx: ReducerContext,
): ReducerResult {
  const stage = topStage(store, ctx);
  if (!stage) return { store, ctx };

  const summary = summarize(event);
  const record: TraceRecord = {
    seq: stage.log.eventTrace.length,
    kind: event.type,
    timestampMs: ctx.now(),
    summary,
    raw: event,
  };
  const nextStage: Stage = {
    ...stage,
    currentEventText: summary,
    log: {
      ...stage.log,
      eventTrace: [...stage.log.eventTrace, record],
    },
  };
  return { store: putStage(store, nextStage), ctx };
}

/**
 * 从 Prompt 事件的元数据播种栈顶 Stage 的 Model_Exchange。
 *
 * Prompt 事件携带 icon / role_id / model_id —— 据此设置 modelExchange 的
 * modelId 与 roleId，并在 Stage.roleId 尚未确定时补齐。当前事件模式不携带
 * 完整 prompt 文本字段，故 modelExchange.prompt 保持为空（设计允许该区域为空，
 * Log_Viewer 会展示空状态）。
 */
function seedPromptMetadata(
  store: StageStore,
  event: Extract<ChatEvent, { type: "Prompt" }>,
  ctx: ReducerContext,
): ReducerResult {
  const stage = topStage(store, ctx);
  if (!stage) return { store, ctx };

  const nextStage: Stage = {
    ...stage,
    roleId: stage.roleId || event.role_id,
    log: {
      ...stage.log,
      modelExchange: {
        ...stage.log.modelExchange,
        modelId: event.model_id,
        roleId: event.role_id,
      },
    },
  };
  return { store: putStage(store, nextStage), ctx };
}

/**
 * 把一段 RoleTurn 增量文本累积到栈顶 Stage 的 modelExchange.response，
 * 并把 currentEventText 更新为该增量的摘要。
 * 无活跃 Stage 时忽略以保持折叠全函数。
 */
function appendResponseDelta(
  store: StageStore,
  event: Extract<ChatEvent, { type: "RoleTurn" }>,
  ctx: ReducerContext,
): ReducerResult {
  const stage = topStage(store, ctx);
  if (!stage) return { store, ctx };

  const nextStage: Stage = {
    ...stage,
    currentEventText: summarize(event),
    log: {
      ...stage.log,
      modelExchange: {
        ...stage.log.modelExchange,
        response: stage.log.modelExchange.response + event.content,
      },
    },
  };
  return { store: putStage(store, nextStage), ctx };
}

/**
 * 完成栈顶 Stage：状态置为 complete，把累积的 modelExchange.response 定型为
 * 最终 content，并弹出委派栈。**保持同一 stage id 与其已收集的 log 不变**
 * （不新建 Stage）。无活跃 Stage 时忽略（不匹配的完成事件的完整防御处理属于 2.4）。
 */
function completeTop(store: StageStore, ctx: ReducerContext): ReducerResult {
  const stage = topStage(store, ctx);
  if (!stage) return { store, ctx };

  const nextStage: Stage = {
    ...stage,
    status: "complete",
    content: stage.log.modelExchange.response,
  };
  const ctx2: ReducerContext = {
    ...ctx,
    stack: ctx.stack.slice(0, -1),
  };
  return { store: putStage(store, nextStage), ctx: ctx2 };
}

/**
 * DelegateStarted：在栈顶 Stage 之下创建一个子 Stage（Subagent_Stage）。
 *  - parentId = 当前栈顶 id
 *  - depth = 父.depth + 1
 *  - 把子 id 追加到父的 childIds
 *  - 把子 Stage 压入委派栈
 * 于是任意深度的嵌套委派构成一棵树；后续事件继续附着到栈顶 Stage，
 * 从而让每个 Stage 的日志相互隔离。无活跃父 Stage（栈为空）时直通不做处理，
 * 顶层回退的防御处理留给任务 2.4。
 */
function startDelegate(
  store: StageStore,
  event: Extract<ChatEvent, { type: "DelegateStarted" }>,
  ctx: ReducerContext,
): ReducerResult {
  const parent = topStage(store, ctx);
  const id = `stage-${ctx.nextId}`;

  // 防御式回退 (2.4)：若栈为空（没有活跃父 Stage），不丢弃该委派，而是把它
  // 作为一个顶层 Stage 挂载 —— 压入 order 与委派栈，使后续事件仍能正常附着。
  if (!parent) {
    const topLevel = createStage({
      id,
      roleId: event.to_role,
      parentId: null,
      depth: 0,
      title: event.task || event.to_role,
      status: "in_progress",
      currentEventText: summarize(event),
    });
    const store2: StageStore = {
      order: [...store.order, id],
      byId: { ...store.byId, [id]: topLevel },
    };
    const ctx2: ReducerContext = {
      ...ctx,
      stack: [...ctx.stack, id],
      nextId: ctx.nextId + 1,
    };
    return { store: store2, ctx: ctx2 };
  }

  const child = createStage({
    id,
    roleId: event.to_role,
    parentId: parent.id,
    depth: parent.depth + 1,
    title: event.task || event.to_role,
    status: "in_progress",
    currentEventText: summarize(event),
  });
  const nextParent: Stage = {
    ...parent,
    childIds: [...parent.childIds, id],
  };
  const store2: StageStore = {
    order: store.order,
    byId: { ...store.byId, [parent.id]: nextParent, [id]: child },
  };
  const ctx2: ReducerContext = {
    ...ctx,
    stack: [...ctx.stack, id],
    nextId: ctx.nextId + 1,
  };
  return { store: store2, ctx: ctx2 };
}

/**
 * DelegateFinished：完成栈顶（子）Stage —— 置为 complete，把 summary 存为
 * content，并弹出委派栈。无活跃 Stage 时直通（不匹配的完成事件的完整防御处理属于 2.4）。
 */
function finishDelegate(
  store: StageStore,
  event: Extract<ChatEvent, { type: "DelegateFinished" }>,
  ctx: ReducerContext,
): ReducerResult {
  const stage = topStage(store, ctx);
  if (!stage) return { store, ctx };

  const nextStage: Stage = {
    ...stage,
    status: "complete",
    content: event.summary,
  };
  const ctx2: ReducerContext = {
    ...ctx,
    stack: ctx.stack.slice(0, -1),
  };
  return { store: putStage(store, nextStage), ctx: ctx2 };
}

/**
 * Error 事件的防御式处理 (2.4)：把栈顶 Stage 标记为 error，先把该错误事件
 * 追加进 Event_Trace（避免静默丢失数据），再把错误消息定型为 content 并停止
 * 其 spinner（status=error），随后弹出委派栈（错误的 Stage 与完成的 Stage 一样
 * 是终态，不应再接收后续事件）。已收集的 log 保持可查看。
 * 栈为空（无目标 Stage）时优雅忽略以保持折叠全函数。
 */
function markError(
  store: StageStore,
  event: Extract<ChatEvent, { type: "Error" }>,
  ctx: ReducerContext,
): ReducerResult {
  const stage = topStage(store, ctx);
  if (!stage) return { store, ctx };

  const summary = summarize(event);
  const record: TraceRecord = {
    seq: stage.log.eventTrace.length,
    kind: event.type,
    timestampMs: ctx.now(),
    summary,
    raw: event,
  };
  const nextStage: Stage = {
    ...stage,
    status: "error",
    content: event.message,
    currentEventText: summary,
    log: {
      ...stage.log,
      eventTrace: [...stage.log.eventTrace, record],
    },
  };
  const ctx2: ReducerContext = {
    ...ctx,
    stack: ctx.stack.slice(0, -1),
  };
  return { store: putStage(store, nextStage), ctx: ctx2 };
}

/**
 * 纯 reducer：把单个事件折叠进 store。
 * 不修改入参，返回新的 store 与推进后的 ctx。
 *
 * 已实现：
 *  - (2.1) 顶层 Stage 创建：RoleStarted / RoundStarted / Prompt（且栈为空时）
 *  - (2.1) 事件追踪：Status / ToolUse / ToolResult / ToolError 追加 TraceRecord
 *  - (2.2) Model_Exchange：Prompt 播种 modelId/roleId；RoleTurn 增量累积 response；
 *          RoleFinished / RoleTurn(is_complete) / Done 完成并弹栈（保留同一 id 与 log）
 *  - (2.3) 委派/嵌套 Subagent_Stage：DelegateStarted 在栈顶之下创建子 Stage
 *          （parentId/depth/childIds）并压栈；DelegateFinished 完成子 Stage 并弹栈。
 * 其余事件（Error 等）留给后续任务 (2.4)，此处走 no-op 直通以保持折叠全函数。
 */
export function stageReducer(
  store: StageStore,
  event: ChatEvent,
  ctx: ReducerContext,
): ReducerResult {
  switch (event.type) {
    case "RoleStarted":
    case "RoundStarted": {
      // 栈为空时创建一个新的顶层 Stage；栈非空时直通不丢弃。
      if (ctx.stack.length === 0) {
        return createTopLevelStage(store, event, ctx);
      }
      return { store, ctx };
    }

    case "Prompt": {
      // 栈为空时先创建顶层 Stage，随后用 Prompt 元数据播种其 Model_Exchange；
      // 栈非空时把元数据播种到当前栈顶 Stage。
      let result: ReducerResult = { store, ctx };
      if (ctx.stack.length === 0) {
        result = createTopLevelStage(store, event, ctx);
      }
      return seedPromptMetadata(result.store, event, result.ctx);
    }

    case "Status":
    case "ToolUse":
    case "ToolResult":
    case "ToolError": {
      return appendTrace(store, event, ctx);
    }

    case "RoleTurn": {
      if (event.is_complete) {
        // 先把最终增量累积进 response，再完成并弹栈。
        const res = appendResponseDelta(store, event, ctx);
        return completeTop(res.store, res.ctx);
      }
      // 流式增量：累积 response 并更新 currentEventText。
      return appendResponseDelta(store, event, ctx);
    }

    case "DelegateStarted": {
      // 在栈顶 Stage 之下创建嵌套的 Subagent_Stage 子节点并压栈。
      return startDelegate(store, event, ctx);
    }

    case "DelegateFinished": {
      // 完成栈顶（子）Stage：存 summary 为 content、置为 complete、弹栈。
      return finishDelegate(store, event, ctx);
    }

    case "RoleFinished":
    case "Done": {
      // 完成栈顶 Stage：定型 content、置为 complete、弹栈（保留同一 id 与 log）。
      // 栈为空时（不匹配的 RoleFinished）completeTop 直通忽略，保持折叠全函数 (2.4)。
      return completeTop(store, ctx);
    }

    case "Error": {
      // (2.4) 把栈顶 Stage 标记为 error、记录消息、停止 spinner 并弹栈；
      // 栈为空时优雅忽略。
      return markError(store, event, ctx);
    }

    default:
      // (2.4) 未知/未专门建模的事件：作为 TraceRecord 记录到栈顶 Stage，
      // 保留原始负载，使 Event_Trace 绝不静默丢失数据；无活跃 Stage 时优雅忽略。
      return appendTrace(store, event, ctx);
  }
}

/**
 * 便捷包装：把一个有序事件数组折叠进一个全新的 store，
 * 并在内部管理 ctx 委派栈。
 */
export function foldEvents(
  events: ChatEvent[],
  now: () => number = Date.now,
): StageStore {
  let store = emptyStore();
  let ctx = createContext(now);
  for (const event of events) {
    const res = stageReducer(store, event, ctx);
    store = res.store;
    ctx = res.ctx;
  }
  return store;
}
