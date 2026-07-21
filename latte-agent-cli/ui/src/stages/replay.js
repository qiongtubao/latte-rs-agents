// replay —— 从持久化的 `.latte/ui-sessions/*.jsonl` 逐条重放事件，
// 用纯函数 `stageReducer` 重建整棵 Stage 树（以及每个 Stage 的日志）。
//
// 落盘格式（见后端 sessions.rs）：
//   - 首行（可能有多行）是 meta：`{"type":"__meta__", ..., "created_at_unix_ms": <ms>}`；
//   - 其余每行是一个 ChatEvent 的前端 JSON（与实时 SSE 收到的逐字一致）。
//
// 因为折叠是纯的，重放同一有序事件序列即可复现实时折叠出的 store。与实时
// 折叠的唯一差别是：TraceRecord.timestampMs 取自持久化记录的时间戳，而不是
// 墙钟接收时间。为此这里不直接用 `foldEvents`（它对所有记录共用一个时钟），
// 而是逐条调用 `stageReducer`，并让 ctx.now() 返回“当前正在处理的记录”的
// 持久化时间戳。
//
// 防御式解析：跳过空行、非法 JSON、meta 行与无法识别的记录，绝不抛错——
// 一条坏行不应中断整个会话的重放。
import { createContext, emptyStore, stageReducer } from "./stageReducer";
/** meta 行的判别字段（与 ChatEvent 的 type 不冲突）。 */
const META_TYPE = "__meta__";
/** 类型守卫：值是一个非空对象。 */
function isObject(value) {
    return typeof value === "object" && value !== null;
}
/** 从对象中读取一个有限数值字段（否则返回 undefined）。 */
function numberField(value) {
    return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}
/**
 * 从一条原始记录中提取持久化时间戳（unix 毫秒）。
 * 依次尝试常见字段；都没有时回退到 `fallback`（通常是最近一条 meta 的
 * created_at_unix_ms）。兼容“事件直接落盘”与“事件被包在 record 里”两种形态。
 */
function timestampFrom(raw, fallback) {
    return (numberField(raw.timestampMs) ??
        numberField(raw.timestamp_unix_ms) ??
        numberField(raw.timestamp) ??
        numberField(raw.ts) ??
        numberField(raw.created_at_unix_ms) ??
        fallback);
}
/**
 * 从一条原始记录中提取 ChatEvent。兼容两种形态：
 *  - 事件直接落盘：记录本身就带有 `type` 字段（当前后端格式）；
 *  - 事件被包裹：记录形如 `{ event: {...}, ts?: ... }`。
 * 无法识别时返回 null（调用方会跳过）。
 */
function eventFrom(raw) {
    if (isObject(raw.event) && typeof raw.event.type === "string") {
        return raw.event;
    }
    if (typeof raw.type === "string") {
        return raw;
    }
    return null;
}
/**
 * 把一段 JSONL 文本解析为有序的 ReplayRecord 列表。
 * 逐行处理，防御式跳过：空行 / 非法 JSON / 非对象 / meta 行 / 无 type 的记录。
 * meta 行的 created_at_unix_ms 会被记住，作为后续无自带时间戳事件的回退时间戳。
 */
export function parseJsonlRecords(jsonl) {
    const records = [];
    let lastTs = 0;
    for (const line of jsonl.split(/\r?\n/)) {
        const trimmed = line.trim();
        if (!trimmed)
            continue; // 跳过空行
        let raw;
        try {
            raw = JSON.parse(trimmed);
        }
        catch {
            continue; // 跳过非法 JSON
        }
        if (!isObject(raw))
            continue;
        // meta 行：记住 created_at 作为时间戳基准，然后跳过（它不是事件）。
        if (raw.type === META_TYPE) {
            const created = numberField(raw.created_at_unix_ms);
            if (created !== undefined)
                lastTs = created;
            continue;
        }
        const event = eventFrom(raw);
        if (!event)
            continue; // 无法识别的记录：跳过而不抛错
        const ts = timestampFrom(raw, lastTs);
        lastTs = ts; // 单调推进：后续无时间戳的事件继承上一条的时间戳
        records.push({ event, timestampMs: ts });
    }
    return records;
}
/**
 * 把一组有序的 ReplayRecord 折叠进一个全新的 StageStore，
 * 每条 TraceRecord.timestampMs 取自对应记录的持久化时间戳。
 *
 * 通过一个闭包变量 `currentTs` 驱动 ctx.now()：每处理一条记录前先把
 * currentTs 设为该记录的时间戳，reducer 内部对该事件生成的所有 TraceRecord
 * 都会读到这个值。ctx 在 reducer 之间被透传（保留同一个 now 函数），因此
 * 无需每条重建 ctx。
 */
export function foldRecords(records) {
    let store = emptyStore();
    let currentTs = 0;
    let ctx = createContext(() => currentTs);
    for (const rec of records) {
        currentTs = rec.timestampMs;
        const res = stageReducer(store, rec.event, ctx);
        store = res.store;
        ctx = res.ctx;
    }
    return store;
}
/**
 * 便捷函数：解析 JSONL 文本并折叠成 StageStore（保留持久化时间戳）。
 * 这是“返回折叠后 store 状态”的入口，可用于测试或直接读取重建结果。
 */
export function replayJsonl(jsonl) {
    return foldRecords(parseJsonlRecords(jsonl));
}
/**
 * 从 ReplayRecord 列表把 store 播种起来（在订阅实时流之前调用）。
 * 通过注入的 `seed` 回调把记录交给 store（与 stageStore 的 seed action 对接），
 * 不与具体 store 实现耦合。同时返回折叠后的 store 状态，便于调用方直接使用。
 */
export function seedStoreFromRecords(records, seed) {
    seed(records);
    return foldRecords(records);
}
/**
 * 从 JSONL 文本把 store 播种起来（在订阅实时流之前调用）。
 * 解析 → 交给 seeder → 返回折叠后的 store 状态。
 */
export function seedStoreFromJsonl(jsonl, seed) {
    return seedStoreFromRecords(parseJsonlRecords(jsonl), seed);
}
/** 从 ReplayRecord 列表取出纯事件序列（供只接收 events 的 seed 变体使用）。 */
export function recordsToEvents(records) {
    return records.map((r) => r.event);
}
