// history_merge 的单测：覆盖实测现场的两类关键场景 ——
// 「首批生命周期事件必须补放」与「重叠区不得重复渲染」。

import { describe, expect, it } from "vitest";

import type { ChatEvent } from "./api";
import { pendingAfterHistory } from "./history_merge";

const wfStarted: ChatEvent = {
  type: "WorkflowStarted",
  name: "task_refine",
  topic: "拆分 LAT-103",
  wf_id: "wf-1",
};
const step1: ChatEvent = {
  type: "WorkflowStep",
  wf_id: "wf-1",
  step_id: "refine",
  description: "出草案",
  index: 1,
  total: 4,
  role_id: "task_planner",
  task: "拆分",
};
const delegateStarted: ChatEvent = {
  type: "DelegateStarted",
  from_role: "manager",
  to_role: "task_planner",
  task: "拆分",
  sub_id: "task_planner-1",
  wf_id: "wf-1",
};
const roleStarted: ChatEvent = {
  type: "RoleStarted",
  role_id: "task_planner",
  detail: "workflow step 'refine'",
  sub_id: "task_planner-1",
};
const toolUse = (args: string): ChatEvent => ({
  type: "ToolUse",
  role_id: "task_planner",
  tool_name: "read",
  args,
  sub_id: "task_planner-1",
});

describe("pendingAfterHistory", () => {
  it("历史为空时，缓冲区整批补放（后台已开跑的 session 的首批事件）", () => {
    const buffered = [wfStarted, step1, delegateStarted, roleStarted];
    expect(pendingAfterHistory([], buffered)).toEqual(buffered);
  });

  it("重叠区按 key 丢弃：历史已有的生命周期事件不再重复渲染", () => {
    const history = [wfStarted, step1, delegateStarted, roleStarted];
    const buffered = [delegateStarted, roleStarted, toolUse("a")];
    expect(pendingAfterHistory(history, buffered)).toEqual([toolUse("a")]);
  });

  it("内容相同的工具事件只抵消一次，第二次仍然补放（合法重复调用）", () => {
    const history = [toolUse("same")];
    const buffered = [toolUse("same"), toolUse("same")];
    expect(pendingAfterHistory(history, buffered)).toEqual([toolUse("same")]);
  });

  it("无去重键的事件一律保留（Status/Prompt 之类）", () => {
    const status: ChatEvent = { type: "Status", message: "tokens: 10" };
    const history = [status];
    expect(pendingAfterHistory(history, [status, status])).toEqual([status, status]);
  });

  it("不带 sub_id 的 RoleStarted 无法判定唯一性 → 保留（宁可多渲染一行也不丢）", () => {
    const bare: ChatEvent = { type: "RoleStarted", role_id: "manager", detail: "turn" };
    expect(pendingAfterHistory([bare], [bare])).toEqual([bare]);
  });

  it("缓冲为空 → 空数组", () => {
    expect(pendingAfterHistory([wfStarted], [])).toEqual([]);
  });
});
