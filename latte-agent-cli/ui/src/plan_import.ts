// plan 导入弹窗的纯逻辑（无 DOM 依赖，可在 node 环境单测）。
//
// buildSelectedPlanTasks 是 PlanProposed 弹窗「导入选中项」按钮与右键
// 补救重开弹窗共用的选择逻辑：把每行的勾选状态 + 可编辑
// title/description/priority 收集成可直接 POST /api/tasks/import 的
// ImportTask[]。chat_impl.ts 的弹窗 DOM 代码读出各行输入值后调它。
//
// subtasks 支持任意深度嵌套：弹窗为每个 subtask 递归生成子行（subRows），
// 勾选树逐层生效 —— 未勾选或空标题的行连同其整个子树一起跳过；
// 老调用方不传 subRows 时，task.subtasks 原样透传。
import type { ImportTask } from "./api";

/** 弹窗一行的输入：勾选状态 + 可编辑值 + 原始任务（提供 labels/workflow/
 * paths/subtasks）。subRows 与 task.subtasks 一一对应，构成递归勾选树。 */
export interface PlanImportRowInput {
  checked: boolean;
  title: string;
  description: string;
  priority: number;
  task: ImportTask;
  /** 子任务勾选树；缺省时 subtasks 原样透传（不逐层过滤）。 */
  subRows?: PlanImportRowInput[];
}

/** 从 plan 导入弹窗各行的勾选+编辑值构建要导入的 ImportTask[]。
 *
 * 输入 rows 是普通对象：checked（是否勾选）、title/description/priority
 * （用户可编辑的值）、task（原始 PlanProposed 任务）。未勾选或空标题
 * （含纯空白）的行跳过（其子树也随之跳过）；空描述输出 undefined
 * （与 ImportTask 可选字段一致），非空描述 trim。
 *
 * 主路径（PlanProposed 弹窗点导入）与补救路径（右键重开弹窗）共用此逻辑。
 */
export function buildSelectedPlanTasks(rows: PlanImportRowInput[]): ImportTask[] {
  const out: ImportTask[] = [];
  for (const r of rows) {
    if (!r.checked) continue;
    const title = r.title.trim();
    if (!title) continue;
    // 有勾选树时按树递归过滤（全被勾掉则不带 subtasks）；否则原样透传。
    let subtasks: ImportTask[] | undefined;
    if (r.subRows) {
      const subs = buildSelectedPlanTasks(r.subRows);
      subtasks = subs.length > 0 ? subs : undefined;
    } else {
      subtasks = r.task.subtasks;
    }
    out.push({
      title,
      description: r.description.trim() || undefined,
      priority: r.priority,
      labels: r.task.labels,
      task_type: (r.task as any).task_type ?? (r.task as any).taskType,
      workflow: r.task.workflow,
      paths: r.task.paths,
      subtasks,
    });
  }
  return out;
}
