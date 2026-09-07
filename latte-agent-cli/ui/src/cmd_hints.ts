// `/command` 自动完成的提示数据纯逻辑（无 DOM 依赖，可在 node 环境单测）。
//
// chat_impl.ts 的斜杠命令补全框读 `CMD_HINTS` 渲染。内建命令是写死的
// （/clear /quit 等运行时内建）；workflow 的斜杠命令（/plan /learn 等）
// 来自后端 `GET /api/workflows`，每条带 `command` 的 workflow 追加进提示，
// 让用户能像内建命令一样被补全。合并与去重逻辑收敛在这里，便于单测。
import type { WorkflowSummary } from "./api";

/** 一条命令补全提示：cmd=斜杠命令，icon=展示图标，desc=说明文字。 */
export interface CmdHint {
  cmd: string;
  icon: string;
  desc: string;
}

/** 内建命令的固定提示（运行时内建，不来自 workflow 配置）。 */
export const BUILTIN_CMD_HINTS: CmdHint[] = [
  { cmd: "/plan", icon: "📋", desc: "运行任务规划 workflow" },
  { cmd: "/clear", icon: "🗑️", desc: "清除对话历史" },
  { cmd: "/quit", icon: "🚪", desc: "退出当前 session" },
  { cmd: "/pause", icon: "⏸️", desc: "暂停当前 agent" },
  { cmd: "/help", icon: "❓", desc: "显示帮助信息" },
  { cmd: "/compact", icon: "📦", desc: "压缩历史（节省 tokens）" },
];

/** workflow 命令提示的默认图标，与内建的中性视觉区分。 */
const WORKFLOW_ICON = "🛠️";

/**
 * 把带斜杠命令的 workflow 合并进内建提示，返回用于渲染的完整列表。
 *
 * 规则：
 * 1. 只取 `command` 非空且以 `/` 开头的 workflow，命令 trim 后作为提示项；
 * 2. 命令去重：与内建命令重名时内建优先（保留其语义描述），同一命令重复
 *    出现（项目/全局副本）也只保留一条；
 * 3. 排序：内建命令保持原有相对顺序在前，workflow 命令按字典序追加在后。
 *
 * @param builtin    内建命令提示（一般传 BUILTIN_CMD_HINTS）。
 * @param workflows  后端 GET /api/workflows 返回的 workflow 列表。
 */
export function mergeWorkflowCommands(
  builtin: CmdHint[],
  workflows: WorkflowSummary[],
): CmdHint[] {
  // 内建命令集合：命中即跳过对应 workflow，保证内建语义不被覆盖。
  const builtinCmds = new Set(builtin.map((h) => h.cmd));
  const seen = new Set(builtinCmds);
  const wfHints: CmdHint[] = [];
  for (const wf of workflows) {
    const cmd = (wf.command ?? "").trim();
    if (!cmd.startsWith("/")) continue;
    if (seen.has(cmd)) continue; // 与内建或已见 workflow 重名，跳过
    seen.add(cmd);
    wfHints.push({
      cmd,
      icon: WORKFLOW_ICON,
      desc: wf.description || "运行 workflow",
    });
  }
  wfHints.sort((a, b) => a.cmd.localeCompare(b.cmd));
  return [...builtin, ...wfHints];
}
