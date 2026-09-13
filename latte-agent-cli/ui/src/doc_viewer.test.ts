// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach, afterEach, type Mock } from "vitest";
import { openDocViewer, installDocLinkHandler } from "./doc_viewer";

const flush = () => new Promise((r) => setTimeout(r, 0));

// mermaid 动态 import 挡掉：文档里若含 mermaid 围栏，懒渲染不进 jsdom。
vi.mock("mermaid", () => ({
  default: { initialize: vi.fn(), render: vi.fn(async () => ({ svg: "<svg/>" })) },
}));

const DOC = "# 第一章\n\n内容一。\n\n## 小节\n\n内容二。\n\n```rust\nfn main() {}\n```\n";

type FakeResponse = { ok: boolean; status?: number; text: () => Promise<string> };
let fetchMock: Mock<[string], Promise<FakeResponse>>;

beforeEach(() => {
  fetchMock = vi.fn<[string], Promise<FakeResponse>>(async () => ({
    ok: true,
    text: async () => DOC,
  }));
  vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
  document.querySelectorAll(".doc-viewer").forEach((el) => el.remove());
  vi.unstubAllGlobals();
});

describe("doc_viewer", () => {
  it("拉取 /api/doc 并渲染全文 + 代码块工具条 + TOC", async () => {
    await openDocViewer(".latte/learn/rust/draft.md");
    await flush();
    const box = document.querySelector(".doc-viewer")!;
    expect(box).not.toBeNull();
    expect(fetchMock).toHaveBeenCalledWith(
      `/api/doc?path=${encodeURIComponent(".latte/learn/rust/draft.md")}`,
    );
    const content = box.querySelector(".doc-viewer-content")!;
    // 标题降级 # → h3
    expect(content.querySelector("h3")!.textContent).toBe("第一章");
    // 代码块有复制工具条
    expect(content.querySelector(".code-block-bar")).not.toBeNull();
    // TOC 两项（h3 + h4）
    const tocItems = box.querySelectorAll(".doc-viewer-toc-item");
    expect(tocItems.length).toBe(2);
    expect(tocItems[0].textContent).toBe("第一章");
    // 标题打了锚点 id
    expect(content.querySelector("h3")!.id).toMatch(/^doc-h-/);
  });

  it("接口失败时显示错误，不白屏", async () => {
    fetchMock.mockResolvedValueOnce({ ok: false, status: 404, text: async () => "not found" });
    await openDocViewer(".latte/learn/none.md");
    await flush();
    expect(document.querySelector(".doc-viewer-content")!.textContent).toContain("加载失败");
  });

  it("点击 a.doc-link 触发内嵌预览（全局委托）", async () => {
    installDocLinkHandler();
    const host = document.createElement("div");
    host.innerHTML = '<a href="#" class="doc-link" data-doc-path="docs/learn/x.md">文档</a>';
    document.body.appendChild(host);
    host.querySelector("a")!.click();
    await flush();
    expect(document.querySelector(".doc-viewer")).not.toBeNull();
    expect(fetchMock).toHaveBeenCalledWith(
      `/api/doc?path=${encodeURIComponent("docs/learn/x.md")}`,
    );
    host.remove();
  });
});
