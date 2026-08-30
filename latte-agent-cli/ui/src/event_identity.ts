// 事件身份（内容指纹）。两个用途，必须是同一份实现：
//  1. chat_impl.handleEvent 抑制**相邻**重复（SSE 重连后同一条持久化
//     事件紧跟 live 投递到达）；
//  2. main.ts 对齐「历史快照 + 订阅窗口缓冲」时的去重
//     （见 history_merge.ts）—— 订阅先于拉历史，两者必然有重叠区。
//
// 只给「内容唯一」的事件类型算指纹：生命周期事件（RoleStarted /
// DelegateStarted / WorkflowStep …）在一次会话里合法地重复出现，
// 按内容去重会把并行波或返工轮的第二次执行吃掉，所以一律返回 ""
// （= 不参与去重）。

import type { ChatEvent } from "./api";

/** 返回事件的内容指纹；空串表示「该类型不参与内容去重」。 */
export function eventIdentity(e: ChatEvent): string {
  switch (e.type) {
    case "RoleTurn":
      return `${e.type}|${e.role_id}|${e.sub_id ?? ""}|${e.is_complete}|${e.content}`;
    case "WorkflowTurn":
      return `${e.type}|${e.wf_id}|${e.step_id}|${e.round}|${e.content}`;
    case "ToolUse":
      return `${e.type}|${e.sub_id ?? ""}|${e.tool_name}|${e.args}`;
    case "ToolResult":
      return `${e.type}|${e.sub_id ?? ""}|${e.tool_name}|${e.result}`;
    case "ToolError":
      return `${e.type}|${e.sub_id ?? ""}|${e.tool_name}|${e.error}`;
    default:
      return "";
  }
}

/** 生命周期事件的「实例键」：用于历史/缓冲重叠区的去重。
 *
 *  与 [`eventIdentity`] 的分工：内容指纹管得住有正文的事件，但
 *  DelegateStarted/RoleStarted/WorkflowStep 这类没有正文的生命周期
 *  事件在重叠区里也会重复，且它们恰恰是「执行中」气泡的来源——
 *  重复渲染会出现两条一模一样的 ⏳ 行。这些事件带 `sub_id` /
 *  `wf_id`+`step_id`，在一次 run 里是唯一的，可以安全地按实例去重。
 *
 *  返回空串表示无法判定唯一性（不去重）。 */
export function eventInstanceKey(e: ChatEvent): string {
  switch (e.type) {
    case "DelegateStarted":
    case "RoleStarted":
      return e.sub_id ? `${e.type}|${e.sub_id}` : "";
    case "DelegateFinished":
      return e.sub_id ? `${e.type}|${e.sub_id}|${e.status}` : "";
    case "WorkflowStep":
      return `${e.type}|${e.wf_id}|${e.step_id}|${e.index}`;
    case "WorkflowStarted":
    case "WorkflowFinished":
      return `${e.type}|${e.wf_id}`;
    case "PlanProposed":
      return `${e.type}|${e.plan_id}`;
    default:
      return "";
  }
}

/** 去重键：内容指纹优先，其次生命周期实例键；都没有就返回空串。 */
export function eventDedupKey(e: ChatEvent): string {
  return eventIdentity(e) || eventInstanceKey(e);
}
