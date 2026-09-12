// todo_store 纯函数测试（TodoUpdated 事件 → TodoState 归约 + 派生选择器）。
//
// 契约：后端每次 todo 工具调用后广播全量 phases（整表替换）；至多一个
// in_progress；空 phases = 列表被清空；sub_id 非空是 delegate/workflow
// 子会话的清单，UI 暂不展示。
import { describe, it, expect } from "vitest";
import {
  emptyTodoState,
  applyTodoUpdated,
  countTodos,
  countPhase,
  isPhaseComplete,
  currentTask,
  todoSummaryLine,
} from "./todo_store";
import type { TodoPhase } from "./api";

function phases(): TodoPhase[] {
  return [
    {
      name: "设计",
      tasks: [
        { content: "定契约", status: "completed" },
        { content: "画原型", status: "completed" },
      ],
    },
    {
      name: "实现",
      tasks: [
        { content: "写 store", status: "in_progress" },
        { content: "写 UI", status: "pending" },
        { content: "写文档", status: "abandoned" },
      ],
    },
  ];
}

describe("applyTodoUpdated：全量替换归约", () => {
  it("主会话（sub_id 缺省/null）整表替换", () => {
    const s0 = emptyTodoState();
    const s1 = applyTodoUpdated(s0, { phases: phases() });
    expect(s1.phases).toHaveLength(2);
    // 再来一条整表替换（不是 merge/diff）
    const s2 = applyTodoUpdated(s1, {
      phases: [{ name: "唯一阶段", tasks: [{ content: "t", status: "pending" }] }],
      sub_id: null,
    });
    expect(s2.phases).toHaveLength(1);
    expect(s2.phases[0].name).toBe("唯一阶段");
  });

  it("空 phases = 列表被清空", () => {
    const s1 = applyTodoUpdated(emptyTodoState(), { phases: phases() });
    const s2 = applyTodoUpdated(s1, { phases: [] });
    expect(countTodos(s2)).toEqual({ done: 0, total: 0 });
  });

  it("子会话（sub_id 非空）忽略：原样返回旧 state（引用不变）", () => {
    const s1 = applyTodoUpdated(emptyTodoState(), { phases: phases() });
    const s2 = applyTodoUpdated(s1, { phases: [], sub_id: "sub-1" });
    expect(s2).toBe(s1);
    expect(countTodos(s2).total).toBe(5);
  });

  it("回放语义：多条事件依次应用，最后一条为准", () => {
    let s = emptyTodoState();
    s = applyTodoUpdated(s, { phases: phases() });
    s = applyTodoUpdated(s, {
      phases: [{ name: "P", tasks: [{ content: "最终", status: "completed" }] }],
    });
    expect(todoSummaryLine(s)).toBe("📝 Todos 1/1 · 全部完成");
  });
});

describe("计数与 phase 完成判定", () => {
  it("countTodos / countPhase 只把 completed 计入 done", () => {
    const s = applyTodoUpdated(emptyTodoState(), { phases: phases() });
    expect(countTodos(s)).toEqual({ done: 2, total: 5 });
    expect(countPhase(s.phases[0])).toEqual({ done: 2, total: 2 });
  });

  it("isPhaseComplete：全 completed 才成立；abandoned / 空 phase 不算", () => {
    const s = applyTodoUpdated(emptyTodoState(), { phases: phases() });
    expect(isPhaseComplete(s.phases[0])).toBe(true);
    expect(isPhaseComplete(s.phases[1])).toBe(false);
    expect(isPhaseComplete({ name: "空", tasks: [] })).toBe(false);
    expect(
      isPhaseComplete({ name: "X", tasks: [
        { content: "a", status: "completed" },
        { content: "b", status: "abandoned" },
      ] }),
    ).toBe(false);
  });
});

describe("currentTask：当前活动项选择器", () => {
  it("优先第一个 in_progress", () => {
    const s = applyTodoUpdated(emptyTodoState(), { phases: phases() });
    expect(currentTask(s)).toEqual({
      phase: "实现",
      task: { content: "写 store", status: "in_progress" },
    });
  });

  it("没有 in_progress 时取第一个 pending", () => {
    const s = applyTodoUpdated(emptyTodoState(), {
      phases: [
        { name: "A", tasks: [{ content: "done", status: "completed" }] },
        { name: "B", tasks: [
          { content: "next", status: "pending" },
          { content: "later", status: "pending" },
        ] },
      ],
    });
    expect(currentTask(s)?.task.content).toBe("next");
  });

  it("只剩 completed 时取最后一个 completed", () => {
    const s = applyTodoUpdated(emptyTodoState(), {
      phases: [
        { name: "A", tasks: [
          { content: "first", status: "completed" },
          { content: "last", status: "completed" },
        ] },
      ],
    });
    expect(currentTask(s)?.task.content).toBe("last");
  });

  it("全 abandoned / 空清单 → null", () => {
    const s = applyTodoUpdated(emptyTodoState(), {
      phases: [{ name: "A", tasks: [{ content: "x", status: "abandoned" }] }],
    });
    expect(currentTask(s)).toBeNull();
    expect(currentTask(emptyTodoState())).toBeNull();
  });
});

describe("todoSummaryLine：单行摘要", () => {
  it("进行中：done/total + 当前活动项", () => {
    const s = applyTodoUpdated(emptyTodoState(), { phases: phases() });
    expect(todoSummaryLine(s)).toBe("📝 Todos 2/5 · 当前: 写 store");
  });

  it("全部完成", () => {
    const s = applyTodoUpdated(emptyTodoState(), {
      phases: [{ name: "A", tasks: [{ content: "t", status: "completed" }] }],
    });
    expect(todoSummaryLine(s)).toBe("📝 Todos 1/1 · 全部完成");
  });

  it("空清单（被清空）", () => {
    expect(todoSummaryLine(emptyTodoState())).toBe("📝 Todos 已清空");
  });
});
