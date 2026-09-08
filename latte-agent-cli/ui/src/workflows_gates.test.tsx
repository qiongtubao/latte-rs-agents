// 只读门禁摘要的 DOM 渲染测试。
// 放在 .test.tsx：vitest.config.ts 的 environmentMatchGlobs 只给 **/*.test.tsx
// 配 jsdom，.test.ts 跑在 node 环境下没有 document。
import { describe, it, expect } from "vitest";
import { makeGatesBlock } from "./workflows_panel";

// ─── 只读门禁摘要（表单 Tab 暴露 TOML 里的门禁字段） ──────────────
//
// 动机（实测 2026-09-07 jemalloc 会话）：表单 Tab 只能编辑 5 个字段，
// 而 TOML 里一个 step 能配 28 个 —— 决定「产出合格与否、要不要返工」的
// 那些全在表单之外，于是 learn.toml 的 verify 步把
// require = ["KIND","PASS","FAIL"] 写成了 AND 语义（任务书写的是
// 「输出 PASS 或 FAIL」），100% 不可能满足、每次靠 advisor 兜底放行，
// UI 上却完全看不出有这道门。
describe("makeGatesBlock（门禁只读展示）", () => {
  it("逐条渲染门禁，并提示只能在 TOML Tab 改", () => {
    const el = makeGatesBlock([
      "必须包含其中**任一**（OR）：VERDICT: PASS / VERDICT: FAIL",
      "返工环：产出不含「VERDICT: PASS」→ 跳回 'write' 重做，最多 2 轮",
    ]);
    const items = Array.from(el.querySelectorAll("li")).map((li) => li.textContent);
    expect(items).toHaveLength(2);
    expect(items[0]).toContain("任一");
    expect(items[1]).toContain("跳回 'write'");
    expect(el.textContent).toContain("只能在「TOML」Tab 编辑");
  });

  it("无门禁时说明「不校验、不返工」，且不用警告色", () => {
    const el = makeGatesBlock([]);
    expect(el.querySelectorAll("li")).toHaveLength(0);
    expect(el.textContent).toContain("不校验、不返工");
    // 很多 step 本来就不需要门禁，不该渲染成警告
    expect(el.querySelector(".workflow-gates-empty")).not.toBeNull();
  });

  it("门禁文本按纯文本渲染，不解释 HTML（防注入）", () => {
    const el = makeGatesBlock(["<img src=x onerror=alert(1)>"]);
    expect(el.querySelector("img")).toBeNull();
    expect(el.querySelector("li")?.textContent).toBe("<img src=x onerror=alert(1)>");
  });
});

