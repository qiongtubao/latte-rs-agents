// Stage 数据模型：把扁平的会话消息列表重构为离散的 "Stage" 树。
//
// 每一条会话消息就是一个 Stage，它拥有产生这条消息的事件序列
// (Event_Trace) 与模型交互 (Model_Exchange)。子代理 (delegate) 的工作
// 被建模为递归嵌套的 Subagent_Stage 子节点。
//
// 这些类型是纯数据结构，被纯函数 `stageReducer` 折叠事件流时使用，
// 因此可以独立测试，也可以从持久化的 JSONL 重放中重建。
/** 创建一个空的 StageLog，保证初始化一致。 */
export function emptyStageLog() {
    return {
        eventTrace: [],
        modelExchange: {
            prompt: "",
            response: "",
        },
    };
}
/**
 * 工厂函数：用一致的默认值创建一个 Stage。
 * 只有 `id` 与 `roleId` 是必需的；其余字段有合理默认值，
 * 让 reducer 与测试的初始化保持一致。
 */
export function createStage(opts) {
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
