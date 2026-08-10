// @vitest-environment jsdom
import { describe, it, expect } from "vitest";
import { enhanceCodeBlocks } from "./codeblock";

function make(html: string): HTMLElement {
  const el = document.createElement("div");
  el.innerHTML = html;
  return el;
}

describe("enhanceCodeBlocks", () => {
  it("把 pre 包进 .code-block 并加复制按钮", () => {
    const el = make('<pre><code class="language-rust">fn main() {}</code></pre>');
    enhanceCodeBlocks(el);
    const wrap = el.querySelector(".code-block");
    expect(wrap).not.toBeNull();
    expect(wrap!.querySelector("pre")).not.toBeNull();
    const btns = wrap!.querySelectorAll(".code-block-btn");
    expect(btns.length).toBe(1); // 无宿主时只有复制
    expect(btns[0].textContent).toBe("复制");
  });

  it("幂等：重复调用不重复包裹", () => {
    const el = make("<pre><code>x = 1</code></pre>");
    enhanceCodeBlocks(el);
    enhanceCodeBlocks(el);
    expect(el.querySelectorAll(".code-block").length).toBe(1);
    expect(el.querySelectorAll(".code-block-btn").length).toBe(1);
  });

  it("语言 info 不是路径 / 无宿主时不出现「在编辑器中打开」", () => {
    const el = make(
      '<pre><code class="language-rust">a</code></pre><pre><code class="language-src/foo.rs">b</code></pre>',
    );
    enhanceCodeBlocks(el);
    expect(el.textContent).not.toContain("在编辑器中打开");
  });
});
