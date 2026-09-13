// 看板任务 × todo 项的「已派发」点亮匹配（纯函数，无 DOM 依赖，可单测）。
//
// 参考 oh-my-pi 的联动设计：进行中的子代理/任务描述与某条 pending
// todo 文本匹配时，该 todo 项在 UI 上点亮高亮（不改任何状态，纯展示）。
// 这里匹配的是看板 in_progress 任务的 title 与 todo 项 content。
//
// 匹配策略刻意保守（宁可不亮也不错标 —— 误标会让用户以为任务已派发）：
//   1. 归一化（小写 + 去全部空白）后任一方向包含 → 命中；
//   2. 否则按字符 bigram 的 Dice 重合度，阈值 0.8（偏高）才命中。
// 空串、过短（归一化后 < 4 字符）一律不命中。

/** 看板任务的最低信息（调用方从 TaskView 投影出来）。 */
export interface BoardTaskRef {
  id: string;
  title: string;
}

const MIN_LEN = 4;
const DICE_THRESHOLD = 0.8;

/** 归一化：小写 + 去掉所有空白（中英文混排、全角空格一并去掉）。 */
function normalize(s: string): string {
  return s.toLowerCase().replace(/\s+/g, "");
}

/** 字符 bigram 集合；不足一个 bigram 时返回空集。 */
function bigrams(s: string): Set<string> {
  const out = new Set<string>();
  for (let i = 0; i + 2 <= s.length; i++) out.add(s.slice(i, i + 2));
  return out;
}

/** Dice 重合度：2·|A∩B| / (|A|+|B|)，范围 [0,1]。 */
function diceCoefficient(a: Set<string>, b: Set<string>): number {
  if (a.size === 0 || b.size === 0) return 0;
  let inter = 0;
  for (const g of a) if (b.has(g)) inter++;
  return (2 * inter) / (a.size + b.size);
}

/** todo 项 content 与看板任务 title 是否「同一回事」（保守）。 */
export function titleMatchesTodo(todoContent: string, taskTitle: string): boolean {
  const a = normalize(todoContent);
  const b = normalize(taskTitle);
  if (a.length < MIN_LEN || b.length < MIN_LEN) return false;
  if (a.includes(b) || b.includes(a)) return true;
  return diceCoefficient(bigrams(a), bigrams(b)) >= DICE_THRESHOLD;
}

/** 在看板任务列表里找与 todo 项 content 匹配的第一个任务（调用方
 *  应只传 in_progress 任务）。无命中返回 null。 */
export function todoLinkedTask(
  todoContent: string,
  boardTasks: BoardTaskRef[],
): BoardTaskRef | null {
  for (const t of boardTasks) {
    if (titleMatchesTodo(todoContent, t.title)) return t;
  }
  return null;
}
