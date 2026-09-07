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
import { buildSelectedPlanTasks, type PlanImportRowInput } from "./plan_import";
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
        task: { title: "实现 ringbuf", labels: ["core"], workflow: "tdd_development", paths: ["src/ringbuf"], subtasks: [{ title: "压测" }] },
      }),
    ]);
    expect(out).toHaveLength(1);
    expect(out[0].title).toBe("实现 ringbuf");
    expect(out[0].description).toBe("并发读写");
    expect(out[0].priority).toBe(1);
    expect(out[0].labels).toEqual(["core"]);
    expect(out[0].workflow).toBe("tdd_development");
    expect(out[0].paths).toEqual(["src/ringbuf"]);
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

// ─── 多层嵌套 subtasks：勾选树递归透传 ────────────────────────────
//
// 弹窗为每个 subtask 递归生成子行（subRows），勾选状态逐层生效：
// 未勾选/空标题的行连同整个子树跳过。不传 subRows（旧调用方）时
// subtasks 原样透传，不做逐层过滤。

/** 按弹窗行为递归构造勾选树行（默认全勾选）。 */
function deepRow(t: ImportTask, over: Partial<PlanImportRowInput> = {}): PlanImportRowInput {
  return {
    checked: true,
    title: t.title,
    description: t.description ?? "",
    priority: t.priority ?? 2,
    task: t,
    subRows: t.subtasks?.map((s) => deepRow(s)),
    ...over,
  };
}

/** 三层嵌套：根 ─┬─ 子A ─┬─ 孙A1
 *               │      └─ 孙A2
 *               └─ 子B */
function nestedTask(): ImportTask {
  return {
    title: "根",
    subtasks: [
      { title: "子A", subtasks: [{ title: "孙A1" }, { title: "孙A2" }] },
      { title: "子B" },
    ],
  };
}

describe("buildSelectedPlanTasks：多层嵌套勾选树", () => {
  it("全勾选：三层结构完整透传，子行的编辑值（标题/优先级）生效", () => {
    const r = deepRow(nestedTask(), { title: "根-改" });
    r.subRows![0].title = "子A-改";
    r.subRows![0].subRows![0].priority = 1;
    const out = buildSelectedPlanTasks([r]);
    expect(out).toHaveLength(1);
    expect(out[0].title).toBe("根-改");
    expect(out[0].subtasks!.map(s => s.title)).toEqual(["子A-改", "子B"]);
    expect(out[0].subtasks![0].subtasks!.map(s => s.title)).toEqual(["孙A1", "孙A2"]);
    expect(out[0].subtasks![0].subtasks![0].priority).toBe(1);
  });

  it("取消中间层「子A」：整个子树（含孙）被丢弃，兄弟「子B」保留", () => {
    const r = deepRow(nestedTask());
    r.subRows![0].checked = false;
    const out = buildSelectedPlanTasks([r]);
    expect(out[0].subtasks!.map(s => s.title)).toEqual(["子B"]);
  });

  it("取消叶子「孙A2」：只移除该叶子，孙A1 保留", () => {
    const r = deepRow(nestedTask());
    r.subRows![0].subRows![1].checked = false;
    const out = buildSelectedPlanTasks([r]);
    expect(out[0].subtasks![0].subtasks!.map(s => s.title)).toEqual(["孙A1"]);
  });

  it("子树的行全部未勾选：父行输出不带 subtasks（而不是空数组）", () => {
    const r = deepRow(nestedTask());
    r.subRows!.forEach(s => { s.checked = false; });
    const out = buildSelectedPlanTasks([r]);
    expect(out).toHaveLength(1);
    expect(out[0].subtasks).toBeUndefined();
  });

  it("不传 subRows（旧调用方）：多层 subtasks 原样透传不过滤", () => {
    const out = buildSelectedPlanTasks([
      row({ title: "t", task: nestedTask() }),
    ]);
    expect(out[0].subtasks![0].subtasks!.map(s => s.title)).toEqual(["孙A1", "孙A2"]);
  });

  it("嵌套层空标题同样跳过（含其子树）", () => {
    const r = deepRow(nestedTask());
    r.subRows![1].title = "   "; // 子B 空标题
    const out = buildSelectedPlanTasks([r]);
    expect(out[0].subtasks!.map(s => s.title)).toEqual(["子A"]);
  });
});
