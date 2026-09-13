// Markdown → HTML 渲染器（聊天气泡 / ask 卡片文档 / 工具面板文档共用）。
//
// 安全口径与旧版自研渲染器一致：模型输出不可信，markdown-it 以
// `html: false` 运行 —— 任何原始 HTML 一律转义，不存在注入面。
// 链接只放行 http(s) 并强制 noopener；mermaid 源码先进
// `<div class="mermaid">` 占位，真正的 SVG 渲染在 codeblock.ts 的
// enhanceRenderedMarkdown 里懒加载完成（mermaid 以 securityLevel
// "strict" 运行，标签经 DOMPurify 消毒）。
//
// 相对 markdown-it 默认行为的定制：
//   - 标题级别 +2（# → h3），气泡内标题不宜比正文大太多；
//   - 围栏代码块用 highlight.js（common 语言子集）上色，未知语言
//     回退为纯转义文本；```mermaid 输出占位 div；
//   - 任务列表（- [ ] / - [x]）经 markdown-it-task-lists。

import MarkdownIt, { type RendererRule } from "markdown-it";
// @ts-expect-error 插件无类型定义
import taskLists from "markdown-it-task-lists";
import texmath from "markdown-it-texmath";
import container from "markdown-it-container";
import footnote from "markdown-it-footnote";
import deflist from "markdown-it-deflist";
import katex from "katex";
import hljs from "highlight.js/lib/common";

/** 本地相对路径（非 http(s)/绝对/data:/blob:）：图片打到 /api/doc-asset，
 *  .md 文档链接走 UI 内嵌预览。 */
function isLocalPath(src: string): boolean {
  return !/^(https?:)?\/\//i.test(src) && !/^(data|blob|file):/i.test(src) && !src.startsWith("/") && src.trim() !== "";
}

/** 本地 .md 链接：交给前端 doc viewer 内嵌预览（href 置空防跳转）。 */
function isLocalMarkdown(src: string): boolean {
  return isLocalPath(src) && /\.md(?:[#?].*)?$/i.test(src);
}

const md = new MarkdownIt({
  html: false,
  linkify: false,
  typographer: false,
});
md.use(taskLists, { label: true, labelAfter: true });
md.use(texmath, { engine: katex, delimiters: "dollars" });
md.use(footnote);
md.use(deflist);
// ::: details [标题] … ::: —— 安全的折叠块语法（html:false 下 <details>
// 原样转义，容器语法由渲染器自己产出标签，无注入面）。
md.use(container, "details", {
  validate: (info: string) => /^details(\s+.*)?$/.test(info.trim()),
  render: (tokens: { info: string; nesting: number }[], idx: number) => {
    const token = tokens[idx];
    if (token.nesting !== 1) return "</details>\n";
    const title = token.info.trim().replace(/^details\s*/, "").trim();
    return `<details class="md-details"><summary>${md.utils.escapeHtml(title || "展开详情")}</summary>\n`;
  },
});

// 标题降两级：# → h3、## → h4 …（封顶 h6）。
const shiftHeading = (delta: number): RendererRule => (tokens, idx, options, _env, self) => {
  const token = tokens[idx];
  const level = Math.min(Number(token.tag.slice(1)) + delta, 6);
  token.tag = `h${level}`;
  return self.renderToken(tokens, idx, options);
};
md.renderer.rules.heading_open = shiftHeading(2);
md.renderer.rules.heading_close = shiftHeading(2);

// 外部链接新窗口打开且不带 opener。markdown-it 内置 validateLink
// 已拒绝 javascript: 等协议（按原文输出），这里只需补 http(s) 属性。
const defaultLinkOpen: RendererRule =
  md.renderer.rules.link_open ??
  ((tokens, idx, options, _env, self) => self.renderToken(tokens, idx, options));
md.renderer.rules.link_open = (tokens, idx, options, env, self) => {
  const token = tokens[idx];
  const href = String(token.attrGet("href") ?? "");
  if (/^https?:\/\//i.test(href)) {
    token.attrSet("target", "_blank");
    token.attrSet("rel", "noopener noreferrer");
  } else if (isLocalMarkdown(href)) {
    // 本地 md 文档：标记为 doc-link，点击由 doc_viewer 打开内嵌预览。
    token.attrSet("href", "#");
    token.attrSet("class", "doc-link");
    token.attrSet("data-doc-path", href);
  }
  return defaultLinkOpen(tokens, idx, options, env, self);
};

// 图片：本地相对路径改走受控只读路由 /api/doc-asset（服务端白名单目录），
// 否则在 UI 源站上 404。
md.renderer.rules.image = (tokens, idx, options, env, self) => {
  const token = tokens[idx];
  const src = String(token.attrGet("src") ?? "");
  if (isLocalPath(src) && !isLocalMarkdown(src)) {
    token.attrSet("src", `/api/doc-asset?path=${encodeURIComponent(src)}`);
    token.attrSet("loading", "lazy");
  }
  // alt 需从子 token 重新渲染纯文本（对齐 markdown-it 默认 image 规则）。
  const altIdx = token.attrIndex("alt");
  if (altIdx >= 0 && token.attrs) {
    token.attrs[altIdx][1] = self.renderInlineAsText(token.children ?? [], options, env);
  }
  return self.renderToken(tokens, idx, options);
};

// 围栏代码块：mermaid → 占位 div（后处理渲染 SVG）；已注册语言 →
// hljs 高亮；其余 → 纯转义。class 保持 `language-xxx`，codeblock.ts
// 的工具条靠它识别语言/路径。
md.renderer.rules.fence = (tokens, idx) => {
  const token = tokens[idx];
  const info = token.info.trim();
  const lang = info.split(/\s+/g)[0] ?? "";

  if (lang === "mermaid") {
    return `<div class="mermaid">${md.utils.escapeHtml(token.content)}</div>\n`;
  }

  let inner: string;
  let highlighted = false;
  if (lang && hljs.getLanguage(lang)) {
    try {
      inner = hljs.highlight(token.content, { language: lang, ignoreIllegals: true }).value;
      highlighted = true;
    } catch {
      inner = md.utils.escapeHtml(token.content);
    }
  } else {
    inner = md.utils.escapeHtml(token.content);
  }

  const cls = `${highlighted ? "hljs " : ""}${lang ? `language-${lang}` : ""}`;
  return `<pre><code${cls ? ` class="${cls}"` : ""}>${inner}</code></pre>\n`;
};

/** 渲染 Markdown 为 HTML 字符串。输出可安全用于 innerHTML。 */
export function renderMarkdown(text: string): string {
  return md.render(text);
}
