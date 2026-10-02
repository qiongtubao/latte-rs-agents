/** 嵌套 md：文档内相对链接/图片按文档目录解析；```markdown 内嵌围栏不提前关闭。 */
import { describe, it, expect } from "vitest";
import { renderMarkdown, resolveDocPath, repairNestedMarkdownFences } from "./markdown";

describe("resolveDocPath", () => {
  it("相对当前文档目录解析并去掉 fragment", () => {
    expect(resolveDocPath("b.md", "docs/learn")).toBe("docs/learn/b.md");
    expect(resolveDocPath("./b.md#sec", "docs/learn")).toBe("docs/learn/b.md");
    expect(resolveDocPath("../guide/c.md", "docs/learn")).toBe("docs/guide/c.md");
    expect(resolveDocPath("docs/a.md#x")).toBe("docs/a.md");
  });
  it("越过根的 .. 保留，交给服务端拒绝", () => {
    expect(resolveDocPath("../../x.md", "docs")).toBe("../x.md");
  });
});

describe("嵌套文档渲染", () => {
  it("doc-link 与图片按 baseDir 解析", () => {
    const html = renderMarkdown("[下一篇](./next.md) ![图](img/a.png)", { baseDir: "docs/learn" });
    expect(html).toContain('data-doc-path="docs/learn/next.md"');
    expect(html).toContain(`src="/api/doc-asset?path=${encodeURIComponent("docs/learn/img/a.png")}"`);
  });
  it("无 baseDir 时行为不变", () => {
    expect(renderMarkdown("[a](docs/a.md)")).toContain('data-doc-path="docs/a.md"');
  });
});

describe("repairNestedMarkdownFences", () => {
  const src = [
    "```markdown",
    "# 标题",
    "```rust",
    "fn main() {}",
    "```",
    "尾部说明",
    "```",
    "外面的正文",
  ].join("\n");

  it("外层被加长，内层围栏保留为文本", () => {
    const html = renderMarkdown(src);
    // 只有一个代码块，内层 ```rust 作为原文出现在其中
    expect(html.match(/<pre>/g)?.length).toBe(1);
    expect(html).toContain("```rust");
    expect(html).toContain("尾部说明");
    expect(html).toContain("<p>外面的正文</p>");
  });

  it("普通 / 已正确加长的围栏不改动", () => {
    const plain = "```rust\nlet a = 1;\n```";
    expect(repairNestedMarkdownFences(plain)).toBe(plain);
    const ok = "````md\n```js\nx\n```\n````";
    expect(repairNestedMarkdownFences(ok)).toBe(ok);
  });

  it("找不到外层结束时原样保留", () => {
    const broken = "```md\n```js\nx\n```";
    expect(repairNestedMarkdownFences(broken)).toBe(broken);
  });
});
