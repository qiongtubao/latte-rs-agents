// plan 导入弹窗「勾选+编辑值 -> ImportTask[]」纯函数测试。
//
// 测试对象：buildSelectedPlanTasks（plan_import.ts 导出）。
// 它是 PlanProposed 弹窗「导入选中项」按钮与右键补救重开弹窗共用的
// 选择逻辑：把每行的勾选状态 + 可编辑 title/description/priority 收集成
// 可直接 POST /api/tasks/import 的 ImportTask[]。
//
// 如何测试：构造普通对象数组（模拟 DOM 输入值），断言输出--
//   - 未勾选行跳过
//   - 空标题行跳过（即使勾选）
//   - 空描述 -> undefined（与 ImportTask 可选字段一致）
//   - priority/labels/workflow/subtasks 透传
import { describe, it, expect } from "vitest";
import { buildSelectedPlanTasks } from "./plan_import";
import type { ImportTask } from "./api";

/** 构造一行输入（模拟弹窗 DOM 取值），缺省字段给合理默认。 */
function row(over: Partial<{ checked: boolean; title: string; description: string; priority: number; task: ImportTask }>): {
  checked: boolean; title: string; description: string; priority: number; task: ImportTask;
} {
  return {
    checked: true,
    title: "默认标题",
    description: "",
    priority: 2,
    task: { title: "默认标题" },
    ...over,
  };
}

describe("buildSelectedPlanTasks：plan 弹窗选择 -> ImportTask[]", () => {
  it("勾选且标题非空 -> 全量返回，字段正确", () => {
    const out = buildSelectedPlanTasks([
      row({
        title: "实现 ringbuf",
        description: "并发读写",
        priority: 1,
        task: { title: "实现 ringbuf", labels: ["core"], workflow: "tdd_development", subtasks: [{ title: "压测" }] },
      }),
    ]);
    expect(out).toHaveLength(1);
    expect(out[0].title).toBe("实现 ringbuf");
    expect(out[0].description).toBe("并发读写");
    expect(out[0].priority).toBe(1);
    expect(out[0].labels).toEqual(["core"]);
    expect(out[0].workflow).toBe("tdd_development");
    expect(out[0].subtasks).toEqual([{ title: "压测" }]);
  });

  it("未勾选的行跳过", () => {
    const out = buildSelectedPlanTasks([
      row({ checked: false, title: "A" }),
      row({ checked: true, title: "B" }),
    ]);
    expect(out).toHaveLength(1);
    expect(out[0].title).toBe("B");
  });

  it("空标题（含纯空白）的行跳过，即使勾选", () => {
    const out = buildSelectedPlanTasks([
      row({ title: "   " }),
      row({ title: "" }),
      row({ title: "有效" }),
    ]);
    expect(out).toHaveLength(1);
    expect(out[0].title).toBe("有效");
  });

  it("空描述 -> undefined（与 ImportTask 可选字段一致），非空描述 trim", () => {
    const out = buildSelectedPlanTasks([
      row({ title: "x", description: "" }),
      row({ title: "y", description: "  带空白  " }),
    ]);
    expect(out[0].description).toBeUndefined();
    expect(out[1].description).toBe("带空白");
  });

  it("priority 透传；原 task 无 labels/workflow/subtasks 时输出缺省", () => {
    const out = buildSelectedPlanTasks([
      row({ title: "z", priority: 4, task: { title: "z" } }),
    ]);
    expect(out[0].priority).toBe(4);
    expect(out[0].labels).toBeUndefined();
    expect(out[0].workflow).toBeUndefined();
    expect(out[0].subtasks).toBeUndefined();
  });
});
