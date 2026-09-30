/**
 * renderMarkdown 单元测试：聊天气泡的 Markdown 渲染器（markdown-it）。
 *
 * 安全前提：`html: false` —— 任何原始 HTML 都必须原样转义输出，
 * 绝不允许注入可执行标签。
 */
import { describe, it, expect } from "vitest";
import { renderMarkdown } from "./chat_impl";

describe("renderMarkdown", () => {
  it("粗体 **text** 渲染为 <strong>", () => {
    expect(renderMarkdown("这是 **重点** 内容")).toBe(
      "<p>这是 <strong>重点</strong> 内容</p>\n",
    );
  });

  it("斜体 *text* 渲染为 <em>", () => {
    expect(renderMarkdown("a *b* c")).toBe("<p>a <em>b</em> c</p>\n");
  });

  it("行内 code 渲染为 <code>，其中的 * 不被转换", () => {
    expect(renderMarkdown("用 `*ptr` 解引用")).toBe(
      "<p>用 <code>*ptr</code> 解引用</p>\n",
    );
  });

  it("标题 #/##/### 渲染为 h3/h4/h5", () => {
    expect(renderMarkdown("# 标题")).toBe("<h3>标题</h3>\n");
    expect(renderMarkdown("## 小节")).toBe("<h4>小节</h4>\n");
    expect(renderMarkdown("### 子节")).toBe("<h5>子节</h5>\n");
  });

  it("无序列表渲染为 <ul>", () => {
    expect(renderMarkdown("- 甲\n- 乙")).toBe(
      "<ul>\n<li>甲</li>\n<li>乙</li>\n</ul>\n",
    );
  });

  it("有序列表渲染为 <ol>", () => {
    expect(renderMarkdown("1. 先\n2. 后")).toBe(
      "<ol>\n<li>先</li>\n<li>后</li>\n</ol>\n",
    );
  });

  it("空行分段为多个 <p>", () => {
    expect(renderMarkdown("第一段\n\n第二段")).toBe(
      "<p>第一段</p>\n<p>第二段</p>\n",
    );
  });

  it("http(s) 链接渲染为 <a>，带 noopener", () => {
    expect(renderMarkdown("见 [文档](https://example.com/a?b=1)")).toBe(
      '<p>见 <a href="https://example.com/a?b=1" target="_blank" rel="noopener noreferrer">文档</a></p>\n',
    );
  });

  it("非 http(s) 链接按原文显示", () => {
    expect(renderMarkdown("[x](javascript:alert(1))")).toBe(
      "<p>[x](javascript:alert(1))</p>\n",
    );
  });

  it("围栏代码块渲染为 <pre><code> 并做语法高亮，块内 ** 不转换", () => {
    const out = renderMarkdown("```rust\nlet **x** = 1;\n```");
    expect(out).toContain('<pre><code class="hljs language-rust">');
    expect(out).toContain('<span class="hljs-keyword">let</span>');
    expect(out).toContain("**x**"); // 块内不做 markdown 转换
    expect(out).toContain("</code></pre>");
  });

  it("未注册语言的代码块回退为纯转义（无 hljs class）", () => {
    expect(renderMarkdown("```brainfuck\n+++[>+++<-]\n```")).toBe(
      '<pre><code class="language-brainfuck">+++[&gt;+++&lt;-]\n</code></pre>\n',
    );
  });

  it("代码块外的文本仍做 Markdown 转换", () => {
    expect(renderMarkdown("**前**\n```\ncode\n```\n**后**")).toBe(
      "<p><strong>前</strong></p>\n<pre><code>code\n</code></pre>\n<p><strong>后</strong></p>\n",
    );
  });

  it("```mermaid 围栏渲染为占位 div（SVG 由 enhanceRenderedMarkdown 懒渲染）", () => {
    const out = renderMarkdown("```mermaid\ngraph TD;\nA-->B;\n```");
    expect(out).toBe('<div class="mermaid">graph TD;\nA--&gt;B;\n</div>\n');
  });

  it("GFM 表格渲染为 <table>", () => {
    const out = renderMarkdown("| a | b |\n|---|---|\n| 1 | 2 |");
    expect(out).toContain("<table>");
    expect(out).toContain("<th>a</th>");
    expect(out).toContain("<td>2</td>");
    expect(out).toContain("</table>");
  });

  it("任务列表渲染 checkbox", () => {
    const out = renderMarkdown("- [ ] 待办\n- [x] 完成");
    expect(out).toContain('class="contains-task-list"');
    expect(out).toContain('class="task-list-item-checkbox"');
    expect(out).toContain("checked");
  });

  it("引用块渲染为 <blockquote>", () => {
    expect(renderMarkdown("> 引用")).toBe(
      "<blockquote>\n<p>引用</p>\n</blockquote>\n",
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

  it("mermaid 占位 div 内的 HTML 也被转义", () => {
    const out = renderMarkdown("```mermaid\nA[<script>alert(1)</script>]\n```");
    expect(out).not.toContain("<script>");
    expect(out).toContain("&lt;script&gt;");
  });

  it("未闭合的 ** 原样输出，不产生错误标签", () => {
    expect(renderMarkdown("这是 **未闭合")).toBe("<p>这是 **未闭合</p>\n");
  });

  it("流式中间态（半个标记）不崩溃", () => {
    expect(renderMarkdown("**粗")).toBe("<p>**粗</p>\n");
    expect(renderMarkdown("**粗体** 继续 `code")).toBe(
      "<p><strong>粗体</strong> 继续 `code</p>\n",
    );
  });

  it("行内/块级数学公式渲染为 KaTeX", () => {
    expect(renderMarkdown("行内 $E=mc^2$ 完")).toContain('class="katex"');
    expect(renderMarkdown("$$\n\\int_0^1 x dx\n$$")).toContain("katex-display");
  });

  it("::: details 折叠块渲染为 <details>", () => {
    const out = renderMarkdown("::: details 点开看\n里面的 **内容**\n:::");
    expect(out).toContain('<details class="md-details"><summary>点开看</summary>');
    expect(out).toContain("<strong>内容</strong>");
    expect(out).toContain("</details>");
  });

  it("脚注渲染为脚注区，不再被误解析为链接", () => {
    const out = renderMarkdown("文字[^a]\n\n[^a]: 脚注内容");
    expect(out).toContain('class="footnote-ref"');
    expect(out).toContain('class="footnotes"');
    expect(out).toContain("脚注内容");
    expect(out).not.toContain("%E8%84%9A");
  });

  it("定义列表渲染为 <dl>", () => {
    expect(renderMarkdown("术语\n: 定义内容")).toContain("<dl>");
    expect(renderMarkdown("术语\n: 定义内容")).toContain("<dt>术语</dt>");
  });

  it("本地图片改写到 /api/doc-asset，远程图片不动且保留 alt", () => {
    expect(renderMarkdown("![图](.latte/learn/x.png)")).toContain(
      'src="/api/doc-asset?path=.latte%2Flearn%2Fx.png"',
    );
    const remote = renderMarkdown("![架构图](https://example.com/a.png)");
    expect(remote).toContain('src="https://example.com/a.png"');
    expect(remote).toContain('alt="架构图"');
  });

  it("本地 .md 链接渲染为 doc-link（内嵌预览），不带可跳转 href", () => {
    const out = renderMarkdown("[学习文档](.latte/learn/rust/draft.md)");
    expect(out).toContain('class="doc-link"');
    expect(out).toContain('data-doc-path=".latte/learn/rust/draft.md"');
    expect(out).not.toContain('href=".latte/');
  });

  it("本地图片链接渲染为 img-link，指向 doc-asset 防止跳出 404", () => {
    const out = renderMarkdown("[查看图片](.latte/learn/arch.svg)");
    expect(out).toContain('class="img-link"');
    expect(out).toContain('data-img-src="/api/doc-asset?path=.latte%2Flearn%2Farch.svg"');
    expect(out).toContain('href="/api/doc-asset?path=.latte%2Flearn%2Farch.svg"');
  });

  it("超链接包裹图片同时重写外层 a 与内层 img", () => {
    const out = renderMarkdown("[![架构图](.latte/learn/arch.svg)](.latte/learn/arch.svg)");
    expect(out).toContain('class="img-link"');
    expect(out).toContain('src="/api/doc-asset?path=.latte%2Flearn%2Farch.svg"');
    expect(out).toContain('href="/api/doc-asset?path=.latte%2Flearn%2Farch.svg"');
  });
});
