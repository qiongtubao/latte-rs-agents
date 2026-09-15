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
 *
 * depends_on 走**标题引用**（清单里的新任务还没有 id），所以要跟着弹窗里
 * 的编辑动作重映射：
 * - 目标任务被用户改了标题 → 引用改成新标题（否则后端解析不到，整单 400）；
 * - 目标任务被用户取消勾选（不导入）→ **丢掉这条引用**。这里丢弃是对的：
 *   用户明确不导入那个前置任务，留着引用只会让整单导入失败，或者让任务
 *   永久卡在"等一个不存在的依赖"。
 * - 形如 `LAT-123` 的看板 id 原样保留（引用的是已存在的任务）。
 */
export function buildSelectedPlanTasks(rows: PlanImportRowInput[]): ImportTask[] {
  return buildWithDepMap(rows, collectImportedTitles(rows));
}

/** 看板任务 id 的形状（`LAT-100`）：这类引用指向已存在的任务，原样保留。 */
const BOARD_ID_RE = /^[A-Za-z][A-Za-z0-9_]*-\d+$/;

/** 递归收集**会被导入**的行：原标题（小写）→ 用户编辑后的最终标题。
 * 与 buildWithDepMap 的跳过规则保持一致（未勾选 / 空标题连子树一起跳）。 */
function collectImportedTitles(rows: PlanImportRowInput[]): Map<string, string> {
  const map = new Map<string, string>();
  const walk = (rs: PlanImportRowInput[]): void => {
    for (const r of rs) {
      if (!r.checked) continue;
      const title = r.title.trim();
      if (!title) continue;
      const orig = (r.task.title ?? "").trim().toLowerCase();
      if (orig) map.set(orig, title);
      // 同名 key 后者覆盖：重名标题本来就无法靠标题区分，后端也会判歧义。
      if (r.subRows) walk(r.subRows);
    }
  };
  walk(rows);
  return map;
}

/** 依赖引用重映射：改过的标题跟着改，没导入的引用丢掉，看板 id 原样留。 */
function remapDeps(refs: string[] | undefined, titles: Map<string, string>): string[] | undefined {
  if (!refs || refs.length === 0) return undefined;
  const out: string[] = [];
  for (const raw of refs) {
    const r = raw.trim();
    if (!r) continue;
    const mapped = titles.get(r.toLowerCase());
    if (mapped) out.push(mapped);
    else if (BOARD_ID_RE.test(r)) out.push(r);
  }
  return out.length > 0 ? out : undefined;
}

function buildWithDepMap(
  rows: PlanImportRowInput[],
  titles: Map<string, string>,
): ImportTask[] {
  const out: ImportTask[] = [];
  for (const r of rows) {
    if (!r.checked) continue;
    const title = r.title.trim();
    if (!title) continue;
    // 有勾选树时按树递归过滤（全被勾掉则不带 subtasks）；否则原样透传。
    let subtasks: ImportTask[] | undefined;
    if (r.subRows) {
      const subs = buildWithDepMap(r.subRows, titles);
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
      depends_on: remapDeps(r.task.depends_on, titles),
      subtasks,
    });
  }
  return out;
}
