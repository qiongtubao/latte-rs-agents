// 回归：右键菜单的 `data-action` 必须在 index.html 里存在，且与
// chat_impl.ts 的 switch 分支一一对应。
//
// 这类 bug 很安静：在 chat_impl.ts 里加了 `case "terminate-subagent"`
// 但忘了在 index.html 补 `<button data-action="terminate-subagent">`，
// 代码能编译、typecheck 也过，只是那个菜单项**永远不出现**——用户根本
// 没法触发。反向漏配（HTML 有按钮、TS 没分支）同样安静：点了没反应。
//
// 同时检查 dist/index.html —— UI server 实际 serve 的是构建产物，源码
// 改了没重新 build 时用户看到的还是旧菜单。

import { describe, it, expect } from "vitest";
import { readFileSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, "..");

/** 右键菜单当前应支持的全部动作。新增菜单项时在这里登记一笔。 */
const REQUIRED_ACTIONS = [
  "edit",
  "delete",
  "view-subagent",
  "view-execution-log",
  "terminate-subagent",
  "add-to-board",
  "fork",
] as const;

function actionsIn(html: string): Set<string> {
  const found = new Set<string>();
  for (const m of html.matchAll(/data-action="([\w-]+)"/g)) found.add(m[1]);
  return found;
}

describe("右键菜单 data-action 覆盖", () => {
  it("chat_impl.ts 对每个动作都有 case 分支", () => {
    const src = readFileSync(resolve(ROOT, "src/chat_impl.ts"), "utf8");
    const missing = REQUIRED_ACTIONS.filter(
      (a) => !src.includes(`case "${a}"`),
    );
    expect(
      missing,
      `index.html 有按钮但 chat_impl.ts 缺 case 分支（点了没反应）：${missing.join(", ")}`,
    ).toEqual([]);
  });

  // 源码与构建产物都要有，否则「改了源码没 build」时用户看到旧菜单。
  for (const rel of ["index.html", "dist/index.html"] as const) {
    const targetPath = resolve(ROOT, rel);
    const exists = existsSync(targetPath);
    const itFn = exists ? it : it.skip;

    itFn(`${rel} 含全部菜单项${exists ? "" : "（文件不存在，跳过）"}`, () => {
      const present = actionsIn(readFileSync(targetPath, "utf8"));
      const missing = REQUIRED_ACTIONS.filter((a) => !present.has(a));
      expect(
        missing,
        `${rel} 里缺失的 data-action（菜单项永远不出现）：${missing.join(", ")}`,
      ).toEqual([]);
    });
  }

  it("终止分派项标了 danger 样式", () => {
    // 破坏性操作（丢弃已产出内容）必须视觉上区分于「查看日志」这类
    // 只读项，和「删除消息」保持一致。
    const html = readFileSync(resolve(ROOT, "index.html"), "utf8");
    const line = html
      .split("\n")
      .find((l) => l.includes('data-action="terminate-subagent"'));
    expect(line, "找不到终止分派菜单项").toBeTruthy();
    expect(line!, "终止分派是破坏性操作，应带 danger class").toContain(
      "danger",
    );
  });
});
