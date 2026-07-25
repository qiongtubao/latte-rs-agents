// 回归：`src/main.ts` 调 `$("xxx")` 的所有 id 必须都能在 `index.html`
// 里找到，否则 `$()` 会抛 `#xxx not found` 让整页空白。
//
// 历史 bug：源码 `index.html` 在某次提交里被删了 `tools-btn` /
// `models-btn` / `trace-toggle` 等按钮，但 `main.ts` 的 `mountXxx({...})`
// 仍然通过 `$()` 强引用这些 id —— 结果页面一加载就抛
// `Error: #trace-toggle not found` 然后白屏。

import { describe, it, expect } from "vitest";
import { readFileSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, "..");

describe("index.html 必须覆盖 main.ts 引用的所有 id", () => {
  // 同时检查 Vite 源（开发态）和 vite 构建产物（生产态），因为 UI
  // server 实际是把 dist/ 暴露给浏览器的；dist 不存在时整组跳过。
  const targets = ["index.html", "dist/index.html"] as const;

  for (const rel of targets) {
    const targetPath = resolve(ROOT, rel);
    const exists = existsSync(targetPath);

    describe(rel, () => {
      const itFn = exists ? it : it.skip;
      const reason = exists ? "" : "（文件不存在，跳过）";

      itFn(`main.ts 引用的所有 id 都在 ${rel} 里 ${reason}`, () => {
        const mainSrc = readFileSync(resolve(ROOT, "src/main.ts"), "utf8");
        const html = readFileSync(targetPath, "utf8");

        // 从 main.ts 抽出 `$("xxx")` 形式的 id 引用。
        // 排除模板字符串里的（`$("${...}")` 这种是动态的）。
        const calls = [
          ...mainSrc.matchAll(/\$\(\s*"([\w-]+)"\s*\)/g),
        ];
        const referencedIds = new Set<string>();
        for (const m of calls) referencedIds.add(m[1]);

        // 从 index.html 抽出所有 id 属性。
        const presentIds = new Set<string>();
        for (const m of html.matchAll(/\bid="([\w-]+)"/g)) {
          presentIds.add(m[1]);
        }

        // 差异：列出 main.ts 引用但 index.html 缺失的 id。
        const missing: string[] = [];
        for (const id of referencedIds) {
          if (!presentIds.has(id)) missing.push(id);
        }

        // 失败时把所有缺漏 id 一并列出，方便一次性补齐。
        expect(
          missing,
          `main.ts 中引用但 ${rel} 里缺失的 id：` + missing.join(", "),
        ).toEqual([]);
      });
    });
  }
});