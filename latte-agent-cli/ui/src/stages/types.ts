// Stage 数据模型：把扁平的会话消息列表重构为离散的 "Stage" 树。
//
// 每一条会话消息就是一个 Stage，它拥有产生这条消息的事件序列
// (Event_Trace) 与模型交互 (Model_Exchange)。子代理 (delegate) 的工作
// 被建模为递归嵌套的 Subagent_Stage 子节点。
//
// 这些类型是纯数据结构，被纯函数 `stageReducer` 折叠事件流时使用，
// 因此可以独立测试，也可以从持久化的 JSONL 重放中重建。

/** Stage 的运行状态。 */
export type StageStatus = "in_progress" | "complete" | "error";

/** Log_Viewer 当前展示的两个日志区域之一。 */
export type LogView = "event_trace" | "model_exchange";

/** 一条捕获的流事件，带有接收顺序标记以保证稳定排序。 */
export interface TraceRecord {
  seq: number; // stage 内单调递增的接收序号
  kind: string; // ChatEvent 变体键: "Prompt" | "RoleStarted" | ...
  timestampMs: number; // 接收时间 (从 JSONL 重放时取 trace 的时间戳)
  summary: string; // 供 Progress_Bubble 使用的简短人类可读标签
  raw: unknown; // 完整事件负载，供 Event_Trace 详情展示
}

/** 发送给模型的完整 prompt + 原始响应，对应一个 Stage。 */
export interface ModelExchange {
  prompt: string; // 组装后发送给模型的 prompt 文本
  response: string; // 原始模型响应 (累积)
  modelId?: string;
  roleId?: string;
}

export interface StageLog {
  eventTrace: TraceRecord[]; // 按 seq 排序
  modelExchange: ModelExchange;
}

export interface Stage {
  id: string; // 稳定标识；跨越 progress→reply 转换保持不变
  parentId: string | null; // 顶层为 null；Subagent_Stage 会设置该值
  depth: number; // 0 = 顶层，每一层委派递增
  roleId: string;
  title: string; // 例如被委派的任务或角色标签
  status: StageStatus;
  currentEventText: string; // 最新事件摘要 (驱动 Progress_Bubble)
  content: string; // 完成后的最终回复内容
  log: StageLog; // Event_Trace + Model_Exchange
  childIds: string[]; // 有序的 Subagent_Stage 子节点
  expanded: boolean; // 子节点的展开/折叠状态
}

export interface StageStore {
  order: string[]; // 顶层 stage id，按显示顺序排列
  byId: Record<string, Stage>; // 扁平映射；通过 childIds 遍历树
}

/** 创建一个空的 StageLog，保证初始化一致。 */
export function emptyStageLog(): StageLog {
  return {
    eventTrace: [],
    modelExchange: {
      prompt: "",
      response: "",
    },
  };
}

/** 创建一个 Stage 的可选字段。 */
export interface CreateStageOptions {
  id: string;
  roleId: string;
  parentId?: string | null;
  depth?: number;
  title?: string;
  status?: StageStatus;
  currentEventText?: string;
  content?: string;
  log?: StageLog;
  childIds?: string[];
  expanded?: boolean;
}

/**
 * 工厂函数：用一致的默认值创建一个 Stage。
 * 只有 `id` 与 `roleId` 是必需的；其余字段有合理默认值，
 * 让 reducer 与测试的初始化保持一致。
 */
export function createStage(opts: CreateStageOptions): Stage {
  return {
    id: opts.id,
    parentId: opts.parentId ?? null,
    depth: opts.depth ?? 0,
    roleId: opts.roleId,
    title: opts.title ?? "",
    status: opts.status ?? "in_progress",
    currentEventText: opts.currentEventText ?? "",
    content: opts.content ?? "",
    log: opts.log ?? emptyStageLog(),
    childIds: opts.childIds ?? [],
    expanded: opts.expanded ?? false,
  };
}
