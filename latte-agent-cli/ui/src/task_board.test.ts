// 任务看板「状态 → 动作」映射的纯函数测试。
//
// 状态机是 task_board.ts 的核心：每个状态声明自己的下一步动作
// （STATE_ACTIONS），todo 有排期时切换到 todo_scheduled 变体。
// 这里锁定映射关系，防止改动作列表时悄悄破坏 mockup 定义的流转。
import { describe, it, expect } from "vitest";
import {
  STATES, STATE_ACTIONS, COLUMN_ORDER,
  effectiveActions, lastSessionId, actionRequest, actionToast, actionLabel,
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
      .toEqual(["to_todo", "edit", "cancel"]);
  });

  it("todo 无排期：立即执行 / 指定时间执行 / 移回 Backlog", () => {
    expect(keysOf({ state: "todo", scheduled_at: null }))
      .toEqual(["run_now", "schedule", "to_backlog"]);
  });

  it("todo 有排期：切换到 todo_scheduled 变体（修改时间 / 取消排期）", () => {
    expect(keysOf({ state: "todo", scheduled_at: Date.now() + 60_000 }))
      .toEqual(["run_now", "schedule", "unschedule"]);
    // 排期动作的 label 变为「修改时间」
    const sched = effectiveActions({ state: "todo", scheduled_at: 1 })
      .find(a => a.key === "schedule");
    expect(sched?.label).toBe("修改时间");
  });

  it("in_progress：查看对话 / 中止", () => {
    expect(keysOf({ state: "in_progress", scheduled_at: null }))
      .toEqual(["open_session", "abort"]);
  });

  it("human_review：查看对话 / 通过 / 打回", () => {
    expect(keysOf({ state: "human_review", scheduled_at: null }))
      .toEqual(["open_session", "approve", "reject"]);
  });

  it("rework：重新派发 / 编辑", () => {
    expect(keysOf({ state: "rework", scheduled_at: null }))
      .toEqual(["run_now", "edit"]);
  });

  it("merging：查看对话 / 标记完成", () => {
    expect(keysOf({ state: "merging", scheduled_at: null }))
      .toEqual(["open_session", "mark_done"]);
  });

  it("done / cancelled：只能重新打开", () => {
    expect(keysOf({ state: "done", scheduled_at: null })).toEqual(["reopen"]);
    expect(keysOf({ state: "cancelled", scheduled_at: null })).toEqual(["reopen"]);
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

describe("actionLabel（workflow 绑定任务的执行按钮文案）", () => {
  it("run_now + 已绑定 workflow → 「▶ 运行 <workflow>」", () => {
    const a = STATE_ACTIONS.todo.find((x) => x.key === "run_now")!;
    expect(actionLabel({ workflow: "discussion" }, a)).toBe("▶ 运行 discussion");
  });

  it("run_now + 未绑定 workflow → 保持原 label", () => {
    const a = STATE_ACTIONS.todo.find((x) => x.key === "run_now")!;
    expect(actionLabel({ workflow: null }, a)).toBe(a.label);
    expect(actionLabel({ workflow: null }, a)).toBe("▶ 立即执行");
  });

  it("非 run_now 动作不受 workflow 影响", () => {
    const a = STATE_ACTIONS.todo.find((x) => x.key === "schedule")!;
    expect(actionLabel({ workflow: "discussion" }, a)).toBe(a.label);
  });
});

describe("actionRequest / actionToast", () => {
  it("UI 侧处理的动作（schedule/edit/open_session）不发请求，返回 null", () => {
    expect(actionRequest("schedule", "LAT-1")).toBeNull();
    expect(actionRequest("edit", "LAT-1")).toBeNull();
    expect(actionRequest("open_session", "LAT-1")).toBeNull();
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
