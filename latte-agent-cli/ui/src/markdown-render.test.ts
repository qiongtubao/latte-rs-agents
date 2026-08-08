/**
 * renderMarkdown 单元测试：聊天气泡的 Markdown 渲染器。
 *
 * 安全前提：所有输入先 escapeHtml，再转换 —— 任何原始 HTML 都必须
 * 原样转义输出，绝不允许注入可执行标签。
 */
import { describe, it, expect } from "vitest";
import { renderMarkdown } from "./chat_impl";

describe("renderMarkdown", () => {
  it("粗体 **text** 渲染为 <strong>", () => {
    expect(renderMarkdown("这是 **重点** 内容")).toBe(
      "<p>这是 <strong>重点</strong> 内容</p>",
    );
  });

  it("斜体 *text* 渲染为 <em>", () => {
    expect(renderMarkdown("a *b* c")).toBe("<p>a <em>b</em> c</p>");
  });

  it("行内 code 渲染为 <code>，其中的 * 不被转换", () => {
    expect(renderMarkdown("用 `*ptr` 解引用")).toBe(
      "<p>用 <code>*ptr</code> 解引用</p>",
    );
  });

  it("标题 #/##/### 渲染为 h3/h4/h5", () => {
    expect(renderMarkdown("# 标题")).toBe("<h3>标题</h3>");
    expect(renderMarkdown("## 小节")).toBe("<h4>小节</h4>");
    expect(renderMarkdown("### 子节")).toBe("<h5>子节</h5>");
  });

  it("无序列表渲染为 <ul>", () => {
    expect(renderMarkdown("- 甲\n- 乙")).toBe(
      "<ul><li>甲</li><li>乙</li></ul>",
    );
  });

  it("有序列表渲染为 <ol>", () => {
    expect(renderMarkdown("1. 先\n2. 后")).toBe(
      "<ol><li>先</li><li>后</li></ol>",
    );
  });

  it("空行分段为多个 <p>", () => {
    expect(renderMarkdown("第一段\n\n第二段")).toBe(
      "<p>第一段</p><p>第二段</p>",
    );
  });

  it("http(s) 链接渲染为 <a>，带 noopener", () => {
    expect(renderMarkdown("见 [文档](https://example.com/a?b=1)")).toBe(
      '<p>见 <a href="https://example.com/a?b=1" target="_blank" rel="noopener noreferrer">文档</a></p>',
    );
  });

  it("非 http(s) 链接按原文显示", () => {
    expect(renderMarkdown("[x](javascript:alert(1))")).toBe(
      "<p>[x](javascript:alert(1))</p>",
    );
  });

  it("围栏代码块渲染为 <pre><code>，块内 ** 不转换", () => {
    expect(renderMarkdown("```rust\nlet **x** = 1;\n```")).toBe(
      '<pre><code class="language-rust">let **x** = 1;\n</code></pre>',
    );
  });

  it("代码块外的文本仍做 Markdown 转换", () => {
    expect(renderMarkdown("**前**\n```\ncode\n```\n**后**")).toBe(
      "<p><strong>前</strong></p><pre><code>code\n</code></pre><p><strong>后</strong></p>",
    );
  });

  it("<script> 注入被转义", () => {
    const out = renderMarkdown('<script>alert(1)</script>');
    expect(out).not.toContain("<script>");
    expect(out).toContain("&lt;script&gt;");
  });

  it("<img onerror> 注入被转义", () => {
    const out = renderMarkdown('<img src=x onerror=alert(1)>');
    expect(out).not.toContain("<img");
    expect(out).toContain("&lt;img");
  });

  it("未闭合的 ** 原样输出，不产生错误标签", () => {
    expect(renderMarkdown("这是 **未闭合")).toBe("<p>这是 **未闭合</p>");
  });

  it("流式中间态（半个标记）不崩溃", () => {
    expect(renderMarkdown("**粗")).toBe("<p>**粗</p>");
    expect(renderMarkdown("**粗体** 继续 `code")).toBe(
      "<p><strong>粗体</strong> 继续 `code</p>",
    );
  });
});
