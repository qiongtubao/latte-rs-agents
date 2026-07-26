// 回归测试：index.html 中各侧边面板（tools/models/log/workflows）必须与
// role-editor-panel 平级，不能被嵌套在 role-editor-panel 内部。
//
// 历史 bug：role-editor-panel 的 `</aside>` 漏写，导致 tools-panel /
// models-panel / log-panel 在 DOM 树里被嵌套到 role-editor-panel
// 之下。点 models-btn 时虽然会把 models-panel 上的 .hidden 拿掉，
// 但祖先 role-editor-panel 仍带 .hidden（CSS: transform:
// translateX(110%)），整棵子树被推出可视区域，用户看不到任何内容。
//
// 测试覆盖 Vite 源（开发态）和 vite 构建产物（生产态），因为 UI
// server 实际是把 dist/ 暴露给浏览器的。dist/ 不存在时该用例 it.skip
// 而不是失败——这样本地还没跑过 vite build 也不会误报。
import { describe, it, expect } from "vitest";
import { readFileSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { JSDOM } from "jsdom";

const HERE = dirname(fileURLToPath(import.meta.url));
// src/ → ui/ 上一层
const ROOT = resolve(HERE, "..");

describe("index.html 侧边面板结构", () => {
  const targets = ["index.html", "dist/index.html"] as const;

  for (const rel of targets) {
    // 不存在的目标（比如还没跑过 vite build）整组跳过。
    const targetPath = resolve(ROOT, rel);
    const exists = existsSync(targetPath);

    describe(rel, () => {
      const itFn = exists ? it : it.skip;
      const reason = exists ? "" : "（文件不存在，跳过）";

      itFn(`role-editor-panel 已正确闭合，tools/models/log/workflows-panel 不嵌套于其中 ${reason}`, () => {
        const html = readFileSync(targetPath, "utf8");
        const doc = new JSDOM(html).window.document;

        const roleEditor = doc.getElementById("role-editor-panel");
        const tools = doc.getElementById("tools-panel");
        const models = doc.getElementById("models-panel");
        const log = doc.getElementById("log-panel");
        const workflows = doc.getElementById("workflows-panel");
        const workflowRunModal = doc.getElementById("workflow-run-modal");

        // 必须存在
        expect(roleEditor).not.toBeNull();
        expect(tools).not.toBeNull();
        expect(models).not.toBeNull();
        expect(log).not.toBeNull();
        expect(workflows).not.toBeNull();
        expect(workflowRunModal).not.toBeNull();

        // 关键回归断言：兄弟面板不能嵌套在 role-editor-panel 之下。
        // role-editor-panel!.contains(child) === true 表示 child 是
        // role-editor-panel 的后代；为 false 才是兄弟关系。
        expect(roleEditor!.contains(tools!)).toBe(false);
        expect(roleEditor!.contains(models!)).toBe(false);
        expect(roleEditor!.contains(log!)).toBe(false);
        expect(roleEditor!.contains(workflows!)).toBe(false);
        expect(roleEditor!.contains(workflowRunModal!)).toBe(false);
        // workflows-panel 自身也必须正确闭合：试运行弹层不能嵌套在其中，
        // 否则面板关闭（.hidden）时弹层也一起被隐藏。
        expect(workflows!.contains(workflowRunModal!)).toBe(false);
      });
    });
  }
});