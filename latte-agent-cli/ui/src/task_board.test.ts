// 任务看板「状态 → 动作」映射的纯函数测试。
//
// 状态机是 task_board.ts 的核心：每个状态声明自己的下一步动作
// （STATE_ACTIONS），todo 有排期时切换到 todo_scheduled 变体。
// 这里锁定映射关系，防止改动作列表时悄悄破坏 mockup 定义的流转。
import { describe, it, expect } from "vitest";
import {
  STATES, STATE_ACTIONS, COLUMN_ORDER,
  effectiveActions, lastSessionId, actionRequest, actionToast, actionLabel,
  dispatchReadyToast,
  childTasksOf, descendantIds, parentOptions,
} from "./task_board";
import type { TaskView } from "./api";

function keysOf(task: Pick<TaskView, "state" | "scheduled_at">): string[] {
  return effectiveActions(task).map(a => a.key);
}

describe("任务看板状态机：状态 → 动作映射", () => {
  it("列顺序与 mockup 一致，cancelled 不上看板列", () => {
    expect(COLUMN_ORDER).toEqual([
      "backlog", "todo", "in_progress", "human_review", "rework", "merging", "done",
    ]);
    expect(COLUMN_ORDER).not.toContain("cancelled");
    // 每个看板列 + cancelled 都有状态元数据
    for (const s of [...COLUMN_ORDER, "cancelled"] as const) {
      expect(STATES[s]).toBeDefined();
    }
  });

  it("backlog：移到 Todo / 编辑 / 取消", () => {
    expect(keysOf({ state: "backlog", scheduled_at: null }))
      .toEqual(["to_todo", "edit", "cancel", "delete"]);
  });

  it("todo 无排期：立即执行 / 拆分子任务 / 指定时间执行 / 移回 Backlog", () => {
    expect(keysOf({ state: "todo", scheduled_at: null }))
      .toEqual(["run_now", "refine", "schedule", "to_backlog", "delete"]);
  });

  it("todo 有排期：切换到 todo_scheduled 变体（修改时间 / 取消排期）", () => {
    expect(keysOf({ state: "todo", scheduled_at: Date.now() + 60_000 }))
      .toEqual(["run_now", "refine", "schedule", "unschedule", "delete"]);
    // 排期动作的 label 变为「修改时间」
    const sched = effectiveActions({ state: "todo", scheduled_at: 1 })
      .find(a => a.key === "schedule");
    expect(sched?.label).toBe("修改时间");
  });

  it("in_progress：查看对话 / 中止", () => {
    expect(keysOf({ state: "in_progress", scheduled_at: null }))
      .toEqual(["open_session", "abort", "delete"]);
  });

  it("human_review：查看对话 / 通过 / 打回", () => {
    expect(keysOf({ state: "human_review", scheduled_at: null }))
      .toEqual(["open_session", "approve", "reject", "delete"]);
  });

  it("rework：按类型重做 / 返工 / 编辑", () => {
    expect(keysOf({ state: "rework", scheduled_at: null }))
      .toEqual(["run_now", "run_rework", "edit", "delete"]);
  });

  it("merging：查看对话 / 标记完成", () => {
    expect(keysOf({ state: "merging", scheduled_at: null }))
      .toEqual(["open_session", "mark_done", "delete"]);
  });

  it("done / cancelled：重新打开或删除归档", () => {
    expect(keysOf({ state: "done", scheduled_at: null })).toEqual(["reopen", "delete"]);
    expect(keysOf({ state: "cancelled", scheduled_at: null })).toEqual(["reopen", "delete"]);
  });

  it("每个状态的动作都有 kind，且卡片快捷动作能找到 primary", () => {
    for (const list of Object.values(STATE_ACTIONS)) {
      for (const a of list) {
        expect(["primary", "ghost", "danger"]).toContain(a.kind);
        expect(a.label.length).toBeGreaterThan(0);
      }
    }
    // mockup 行为：卡片上只放主动作 —— done/cancelled 以外每个状态都有 primary
    for (const state of ["backlog", "todo", "todo_scheduled", "in_progress", "human_review", "rework", "merging"]) {
      expect(
        STATE_ACTIONS[state].some(a => a.kind === "primary"),
        `${state} 应有 primary 动作`,
      ).toBe(true);
    }
  });
});

describe("lastSessionId（查看对话按钮的 session 来源）", () => {
  it("runs 为空 → null（按钮禁用）", () => {
    expect(lastSessionId({ runs: [] })).toBeNull();
  });

  it("取 runs 最后一个元素的 session_id", () => {
    const runs: TaskView["runs"] = [
      { session_id: "ui-aaa", started_at: 1, ended_at: 2, result: "ok" },
      { session_id: "ui-bbb", started_at: 3, ended_at: null, result: null },
    ];
    expect(lastSessionId({ runs })).toBe("ui-bbb");
  });
});

describe("actionLabel（执行按钮始终显示任务动作，不以 workflow 名覆盖）", () => {
  it("run_now 始终显示「立即执行」，即使已绑定 workflow", () => {
    const a = STATE_ACTIONS.todo.find((x) => x.key === "run_now")!;
    expect(actionLabel({ workflow: "discussion" }, a)).toBe(a.label);
    expect(actionLabel({ workflow: "discussion" }, a)).toBe("▶ 立即执行");
    expect(actionLabel({ workflow: null }, a)).toBe(a.label);
  });

  it("非 run_now 动作不受 workflow 影响", () => {
    const a = STATE_ACTIONS.todo.find((x) => x.key === "schedule")!;
    expect(actionLabel({ workflow: "discussion" }, a)).toBe(a.label);
  });
});
describe("actionRequest / actionToast", () => {
  it("UI 侧处理的动作（schedule/edit/open_session/refine）不发请求，返回 null", () => {
    expect(actionRequest("schedule", "LAT-1")).toBeNull();
    expect(actionRequest("edit", "LAT-1")).toBeNull();
    expect(actionRequest("open_session", "LAT-1")).toBeNull();
    expect(actionRequest("refine", "LAT-1")).toBeNull();
    expect(actionRequest("no-such-key", "LAT-1")).toBeNull();
  });

  it("STATE_ACTIONS 里出现的每个 key 都有 toast 文案", () => {
    for (const list of Object.values(STATE_ACTIONS)) {
      for (const a of list) {
        const msg = actionToast(a.key, "LAT-1");
        expect(msg).toContain("LAT-1");
      }
    }
  });
});
describe("delete action", () => {
  it("每个状态都提供危险删除动作", () => {
    for (const list of Object.values(STATE_ACTIONS)) {
      expect(list.find(a => a.key === "delete")?.kind).toBe("danger");
    }
  });

  it("delete action 显示归档提示", () => {
    expect(actionToast("delete", "LAT-1")).toContain("归档");
  });
});

describe("dispatchReadyToast（派发全部结果提示）", () => {
  it("全部派出：只报派发数", () => {
    expect(dispatchReadyToast({ dispatched: [["LAT-1", "ui-a"]], skipped: [] }))
      .toBe("已派发 1 个任务");
  });

  it("有跳过：报跳过数并附原因", () => {
    const msg = dispatchReadyToast({
      dispatched: [["LAT-1", "ui-a"]],
      skipped: [["LAT-2", "并发上限：已有 3 个任务在跑（上限 3），等位"]],
    });
    expect(msg).toContain("已派发 1 个任务");
    expect(msg).toContain("跳过 1 个");
    expect(msg).toContain("LAT-2");
    expect(msg).toContain("并发上限");
  });

  it("跳过超过 3 条时截断原因列表", () => {
    const skipped: [string, string][] = ["LAT-1", "LAT-2", "LAT-3", "LAT-4"]
      .map((id) => [id, "r"]);
    const msg = dispatchReadyToast({ dispatched: [], skipped });
    expect(msg).toContain("跳过 4 个");
    expect(msg).toContain("…");
    expect(msg).not.toContain("LAT-4:");
  });
});

// ─── 任意深度任务树的纯函数 ──────────────────────────────────────

/** 构造最小 TaskView；缺省字段给合理默认，只覆盖测试关心的。 */
function mkTask(id: string, parent_id: string | null = null, over: Partial<TaskView> = {}): TaskView {
  return {
    schema: "task-v1", id, title: `标题-${id}`, description: "", priority: 3,
    state: "todo", labels: [], task_type: null, parent_id, sub_order: 0,
    scheduled_at: null, runs: [], created_at: 0, updated_at: 0, history: [],
    workflow: null, effective_workflow: null,
    sub_total: 0, sub_done: 0, sub_state_counts: {},
    ...over,
  };
}

/** 三层树：R ─┬─ A ── B（孙）  另有一棵 C。
 *           └─ A2（sub_order 先于 A 验证排序） */
function threeLevelTree(): TaskView[] {
  return [
    mkTask("R"),
    mkTask("A", "R", { sub_order: 2 }),
    mkTask("A2", "R", { sub_order: 1 }),
    mkTask("B", "A"),
    mkTask("C"),
  ];
}

describe("childTasksOf（直接子任务，按 sub_order/created_at 排序）", () => {
  it("三层树下各层取到自己的直接子任务", () => {
    const tasks = threeLevelTree();
    expect(childTasksOf(tasks, "R").map(t => t.id)).toEqual(["A2", "A"]); // sub_order 1 < 2
    expect(childTasksOf(tasks, "A").map(t => t.id)).toEqual(["B"]);
    expect(childTasksOf(tasks, "B")).toEqual([]);
  });

  it("sub_order 相同按 created_at 排", () => {
    const tasks = [
      mkTask("P"),
      mkTask("x", "P", { created_at: 20 }),
      mkTask("y", "P", { created_at: 10 }),
    ];
    expect(childTasksOf(tasks, "P").map(t => t.id)).toEqual(["y", "x"]);
  });
});

describe("descendantIds（全部后代，防环排除用）", () => {
  it("三层树：根的后代含子与孙，不含另一棵树", () => {
    const tasks = threeLevelTree();
    expect([...descendantIds(tasks, "R")].sort()).toEqual(["A", "A2", "B"]);
    expect(descendantIds(tasks, "A").has("B")).toBe(true);
    expect(descendantIds(tasks, "B").size).toBe(0);
  });
});

describe("parentOptions（父任务下拉：全部任务 + 树形缩进 + 防环）", () => {
  it("不排除时列出全部任务，DFS 序且深度正确（非根任务也可作父）", () => {
    const tasks = threeLevelTree();
    const opts = parentOptions(tasks);
    expect(opts.map(o => o.task.id)).toEqual(["R", "A2", "A", "B", "C"]);
    expect(opts.map(o => o.depth)).toEqual([0, 1, 1, 2, 0]);
  });

  it("编辑时排除自身及全部后代（防环），其它分支保留", () => {
    const tasks = threeLevelTree();
    // 编辑 A：A 自身与后代 B 不可选，A2/R/C 可选
    expect(parentOptions(tasks, "A").map(o => o.task.id)).toEqual(["R", "A2", "C"]);
    // 编辑 R：整棵 R 子树不可选，只剩 C
    expect(parentOptions(tasks, "R").map(o => o.task.id)).toEqual(["C"]);
  });

  it("父任务已不在列表中的孤儿按根处理（深度 0，不丢任务）", () => {
    const tasks = [mkTask("orphan", "GONE"), mkTask("kid", "orphan")];
    const opts = parentOptions(tasks);
    expect(opts.map(o => [o.task.id, o.depth])).toEqual([["orphan", 0], ["kid", 1]]);
  });
});

describe("拆分入口对任意层级开放", () => {
  it("todo / todo_scheduled 的动作含 refine，与是否子任务无关（不再按 parent_id 拦截）", () => {
    // effectiveActions 不看 parent_id：子任务在 todo 状态下同样能再拆；
    // in_progress / merging 由后端拒绝，前端本来就不给这些状态的 refine 动作。
    expect(keysOf({ state: "todo", scheduled_at: null })).toContain("refine");
    expect(keysOf({ state: "todo", scheduled_at: 1 })).toContain("refine");
    expect(keysOf({ state: "in_progress", scheduled_at: null })).not.toContain("refine");
    expect(keysOf({ state: "merging", scheduled_at: null })).not.toContain("refine");
  });
});
