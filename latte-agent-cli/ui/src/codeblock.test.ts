// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { enhanceCodeBlocks, enhanceRenderedMarkdown } from "./codeblock";

// mermaid 走动态 import，测试里 mock 掉，避免在 jsdom 里真的渲染 SVG。
const mermaidRender = vi.fn(async (_id: string, source: string) => ({
  svg: `<svg data-source="${source.trim()}"></svg>`,
}));
vi.mock("mermaid", () => ({
  default: {
    initialize: vi.fn(),
    render: (id: string, source: string) => mermaidRender(id, source),
  },
}));

function make(html: string): HTMLElement {
  const el = document.createElement("div");
  el.innerHTML = html;
  return el;
}

/** 等 renderMermaidNode 的异步链跑完。 */
const flush = () => new Promise((r) => setTimeout(r, 0));

beforeEach(() => {
  mermaidRender.mockClear();
});

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

describe("enhanceRenderedMarkdown", () => {
  it("同时增强代码块并懒渲染 mermaid 占位 div", async () => {
    const el = make(
      '<pre><code class="language-rust">fn main() {}</code></pre>' +
        '<div class="mermaid">graph TD;\nA--&gt;B;\n</div>',
    );
    // 节点须挂到 document 上，渲染完成的 isConnected 守卫才放行。
    document.body.appendChild(el);
    enhanceRenderedMarkdown(el);
    expect(el.querySelector(".code-block-bar")).not.toBeNull();
    await flush();
    const mmd = el.querySelector(".mermaid") as HTMLElement;
    expect(mmd.dataset.mermaidDone).toBe("1");
    expect(mmd.querySelector("svg")).not.toBeNull();
    expect(mermaidRender).toHaveBeenCalledTimes(1);
    el.remove();
  });

  it("deferMermaid（流式中途）跳过 mermaid，只加代码块工具条", async () => {
    const el = make('<div class="mermaid">graph TD;</div>');
    document.body.appendChild(el);
    enhanceRenderedMarkdown(el, { deferMermaid: true });
    await flush();
    expect(mermaidRender).not.toHaveBeenCalled();
    expect(el.querySelector("svg")).toBeNull();
    el.remove();
  });

  it("幂等：渲染完成的 mermaid 节点不重复渲染", async () => {
    const el = make('<div class="mermaid">graph TD;</div>');
    document.body.appendChild(el);
    enhanceRenderedMarkdown(el);
    await flush();
    enhanceRenderedMarkdown(el);
    await flush();
    expect(mermaidRender).toHaveBeenCalledTimes(1);
    el.remove();
  });

  it("mermaid 渲染失败回退为普通代码块，不丢原文", async () => {
    mermaidRender.mockRejectedValueOnce(new Error("parse error"));
    const el = make('<div class="mermaid">not a diagram</div>');
    document.body.appendChild(el);
    enhanceRenderedMarkdown(el);
    await flush();
    await flush();
    const mmd = el.querySelector(".mermaid")!;
    expect(mmd.classList.contains("mermaid-error")).toBe(true);
    const code = mmd.querySelector("pre code.language-mermaid");
    expect(code).not.toBeNull();
    expect(code!.textContent).toBe("not a diagram");
    // 回退的 pre 也补上了工具条
    expect(mmd.querySelector(".code-block-bar")).not.toBeNull();
    el.remove();
  });

  it("首次渲染失败时用修补后的图源重试；成功后放大/复制仍用原图源", async () => {
    mermaidRender.mockRejectedValueOnce(new Error("Parse error on line 2"));
    const el = make('<div class="mermaid">flowchart TD\n  A[调用 f(x)] --&gt; B</div>');
    document.body.appendChild(el);
    enhanceRenderedMarkdown(el);
    for (let i = 0; i < 4; i++) await flush();
    const mmd = el.querySelector(".mermaid") as HTMLElement;
    expect(mermaidRender).toHaveBeenCalledTimes(2);
    expect(mermaidRender.mock.calls[1][1]).toContain('A["调用 f(x)"]');
    expect(mmd.classList.contains("mermaid-error")).toBe(false);
    expect(mmd.querySelector("svg")).not.toBeNull();
    el.remove();
  });

  it("修补后仍失败：回退代码块并写明原始报错", async () => {
    mermaidRender.mockRejectedValueOnce(new Error("Parse error on line 2\nlong detail"));
    mermaidRender.mockRejectedValueOnce(new Error("second"));
    const el = make('<div class="mermaid">flowchart TD\n  A[f(x)] --&gt; B</div>');
    document.body.appendChild(el);
    enhanceRenderedMarkdown(el);
    for (let i = 0; i < 4; i++) await flush();
    const mmd = el.querySelector(".mermaid")!;
    expect(mmd.classList.contains("mermaid-error")).toBe(true);
    const note = mmd.querySelector(".mermaid-error-note")!.textContent!;
    expect(note).toContain("Parse error on line 2");
    expect(note).not.toContain("second");
    expect(mmd.querySelector("pre code")!.textContent).toBe("flowchart TD\n  A[f(x)] --> B");
    el.remove();
  });

  it("渲染完成的 mermaid 有「放大」入口，点击打开弹窗可下载/复制图源", async () => {
    const el = make('<div class="mermaid">graph TD; A--&gt;B;</div>');
    document.body.appendChild(el);
    enhanceRenderedMarkdown(el);
    await flush();
    await flush();
    const zoomBtn = el.querySelector<HTMLElement>('.mermaid-bar [data-act="zoom"]');
    expect(zoomBtn).not.toBeNull();
    zoomBtn!.click();
    const modal = document.querySelector(".md-modal-overlay.mermaid-modal");
    expect(modal).not.toBeNull();
    expect(modal!.querySelector(".mermaid-canvas svg")).not.toBeNull();
    expect(modal!.querySelector('[data-act="dl-svg"]')).not.toBeNull();
    expect(modal!.querySelector('[data-act="dl-png"]')).not.toBeNull();
    expect(modal!.querySelector('[data-act="copy-src"]')).not.toBeNull();
    (modal!.querySelector('[data-act="close"]') as HTMLElement).click();
    expect(document.querySelector(".mermaid-modal")).toBeNull();
    el.remove();
  });
});
