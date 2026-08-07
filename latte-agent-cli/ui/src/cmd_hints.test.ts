// `/command` 自动完成提示合并逻辑单测。
//
// 测试对象：mergeWorkflowCommands（cmd_hints.ts 导出）。
// 验证内建 vs workflow 命令去重、排序、跳过无效命令等。
import { describe, it, expect } from "vitest";
import { BUILTIN_CMD_HINTS, mergeWorkflowCommands } from "./cmd_hints";
import type { WorkflowSummary } from "./api";

/** 构造一条 WorkflowSummary，仅关注 command 和 description。 */
function wf(over: Partial<WorkflowSummary>): WorkflowSummary {
  return {
    name: "test",
    description: "",
    steps_count: 1,
    source: "project",
    file_path: ".latte/workflows.d/test.toml",
    ...over,
  };
}

describe("mergeWorkflowCommands：内建 + workflow 斜杠命令合并", () => {
  it("空 workflow 列表 -> 返回内建原样", () => {
    const out = mergeWorkflowCommands(BUILTIN_CMD_HINTS, []);
    expect(out).toHaveLength(BUILTIN_CMD_HINTS.length);
    expect(out).toEqual(BUILTIN_CMD_HINTS);
  });

  it("内建 + workflow 带 /learn -> 合并，内建在前", () => {
    const out = mergeWorkflowCommands(BUILTIN_CMD_HINTS, [
      wf({ name: "learn", command: "/learn", description: "小白向深度讲解" }),
    ]);
    expect(out).toHaveLength(BUILTIN_CMD_HINTS.length + 1);
    // 内建保持前 N 个，顺序不变
    for (let i = 0; i < BUILTIN_CMD_HINTS.length; i++) {
      expect(out[i]).toEqual(BUILTIN_CMD_HINTS[i]);
    }
    // 最后一个是 /learn
    const learn = out[out.length - 1];
    expect(learn.cmd).toBe("/learn");
    expect(learn.icon).toBe("🛠️");
    expect(learn.desc).toBe("小白向深度讲解");
  });

  it("workflow 命令重名 -> 只保留一条", () => {
    const out = mergeWorkflowCommands(BUILTIN_CMD_HINTS, [
      wf({ name: "a", command: "/test" }),
      wf({ name: "b", command: "/test" }),
    ]);
    expect(out).toHaveLength(BUILTIN_CMD_HINTS.length + 1);
    expect(out[out.length - 1].cmd).toBe("/test");
  });

  it("与内建重名的 workflow -> 内建优先（跳过）", () => {
    const out = mergeWorkflowCommands(BUILTIN_CMD_HINTS, [
      wf({ name: "evil-plan", command: "/plan", description: "邪恶的 plan" }),
    ]);
    expect(out).toHaveLength(BUILTIN_CMD_HINTS.length);
    const planEntry = out.find((h) => h.cmd === "/plan");
    expect(planEntry?.desc).not.toBe("邪恶的 plan");
    expect(planEntry?.desc).toBe("运行实现规划 workflow");
  });

  it("command 为空/不以 / 开头 -> 跳过", () => {
    const out = mergeWorkflowCommands(BUILTIN_CMD_HINTS, [
      wf({ name: "no-cmd", command: "" }),
      wf({ name: "plain", command: "plain" }),
      wf({ name: "rel-path", command: "rel/path" }),
      wf({ name: "valid", command: "/valid" }),
    ]);
    expect(out).toHaveLength(BUILTIN_CMD_HINTS.length + 1);
    expect(out[out.length - 1].cmd).toBe("/valid");
  });

  it("workflow 命令按字典序排序", () => {
    const out = mergeWorkflowCommands([], [
      wf({ name: "z", command: "/zzz" }),
      wf({ name: "a", command: "/aaa" }),
      wf({ name: "m", command: "/mmm" }),
    ]);
    expect(out.map((h) => h.cmd)).toEqual(["/aaa", "/mmm", "/zzz"]);
  });

  it("description 为空时使用默认文案", () => {
    const out = mergeWorkflowCommands([], [
      wf({ name: "x", command: "/x", description: "" }),
    ]);
    expect(out[0].desc).toBe("运行 workflow");
  });

  it("返回全新数组，不修改输入", () => {
    const builtin = [{ cmd: "/a", icon: "📋", desc: "a" }];
    const workflows = [wf({ name: "b", command: "/b", description: "b" })];
    const builtinBefore = [...builtin];
    const wfBefore = [...workflows];
    const out = mergeWorkflowCommands(builtin, workflows);
    // 输入原样
    expect(builtin).toEqual(builtinBefore);
    expect(workflows).toEqual(wfBefore);
    // 输出是新建数组
    expect(out).not.toBe(builtin);
  });
});