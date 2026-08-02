// plan 导入弹窗的纯逻辑（无 DOM 依赖，可在 node 环境单测）。
//
// buildSelectedPlanTasks 是 PlanProposed 弹窗「导入选中项」按钮与右键
// 补救重开弹窗共用的选择逻辑：把每行的勾选状态 + 可编辑
// title/description/priority 收集成可直接 POST /api/tasks/import 的
// ImportTask[]。chat_impl.ts 的弹窗 DOM 代码读出各行输入值后调它。
import type { ImportTask } from "./api";

/** 从 plan 导入弹窗各行的勾选+编辑值构建要导入的 ImportTask[]。
 *
 * 输入 rows 是普通对象：checked（是否勾选）、title/description/priority
 * （用户可编辑的值）、task（原始 PlanProposed 任务，提供 labels/workflow/
 * paths/subtasks）。未勾选或空标题（含纯空白）的行跳过；空描述输出 undefined
 * （与 ImportTask 可选字段一致），非空描述 trim。
 *
 * 主路径（PlanProposed 弹窗点导入）与补救路径（右键重开弹窗）共用此逻辑。
 */
export function buildSelectedPlanTasks(
  rows: { checked: boolean; title: string; description: string; priority: number; task: ImportTask }[],
): ImportTask[] {
  const out: ImportTask[] = [];
  for (const r of rows) {
    if (!r.checked) continue;
    const title = r.title.trim();
    if (!title) continue;
    out.push({
      title,
      description: r.description.trim() || undefined,
      priority: r.priority,
      labels: r.task.labels,
      workflow: r.task.workflow,
      paths: r.task.paths,
      subtasks: r.task.subtasks,
    });
  }
  return out;
}
