// history_merge —— 「历史快照 + 订阅窗口缓冲」的合并去重。
//
// 为什么需要它（实测实录）：
// 任务看板「拆分子任务」在 HTTP 响应**之前**就 spawn 了 workflow，实测
// WorkflowStarted / WorkflowStep / DelegateStarted / RoleStarted 全部落在
// session 创建后 ~2ms 内 —— 前端那时还没拿到 session_id。而事件 broadcast
// 没有回放：晚订阅一毫秒，这批事件就永久看不到。于是主对话在 subagent
// 跑的 3.8 分钟里一片空白（subagent 的工具事件按设计不进主对话），刷新
// 页面靠 history 回放才显出「执行中」。
//
// 正确顺序是「先订阅（缓冲）→ 再拉历史 → 回放 → flush 缓冲」。代价是
// 订阅时刻与历史快照必然有重叠区，需要去重 —— 就是本模块。
//
// 去重语义：按 key **多重集**匹配，而不是集合。历史里出现两次的事件，
// 缓冲里也允许保留第二次；只有能与历史里某一条配上对的才丢弃。这样
// 合法的重复回合（同一角色说了两次同样的话）不会被吃掉。

import type { ChatEvent } from "./api";
import { eventDedupKey } from "./event_identity";

/**
 * 从 `buffered`（订阅窗口里缓冲的 live 事件）中剔除已经出现在
 * `history`（服务端权威快照）里的那些，返回还需要补放的事件。
 *
 * @param history  已回放的历史事件（时间序）
 * @param buffered 订阅之后收到、尚未渲染的 live 事件（时间序）
 */
export function pendingAfterHistory(
  history: readonly ChatEvent[],
  buffered: readonly ChatEvent[],
): ChatEvent[] {
  const remaining = new Map<string, number>();
  for (const ev of history) {
    const key = eventDedupKey(ev);
    if (!key) continue;
    remaining.set(key, (remaining.get(key) ?? 0) + 1);
  }
  const out: ChatEvent[] = [];
  for (const ev of buffered) {
    const key = eventDedupKey(ev);
    const left = key ? remaining.get(key) ?? 0 : 0;
    if (left > 0) {
      // 与历史里的一条配对成功 → 已经渲染过，丢弃。
      remaining.set(key, left - 1);
      continue;
    }
    out.push(ev);
  }
  return out;
}
