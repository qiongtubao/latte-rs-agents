// 学习/项目文档的 UI 内嵌预览：markdown 里的本地 .md 链接（renderMarkdown
// 输出 <a class="doc-link" data-doc-path="...">）点击后在弹窗里渲染全文，
// 左侧带标题目录（TOC）。文档经 /api/doc 只读路由获取（服务端白名单
// .latte/learn/ 与 docs/），渲染与聊天正文同一 markdown 管线 + 后处理
// （mermaid、代码块工具条都生效）。

import { renderMarkdown } from "./markdown";
import { enhanceRenderedMarkdown } from "./codeblock";

let overlay: HTMLElement | null = null;

function closeViewer(): void {
  overlay?.remove();
  overlay = null;
}

/** 拉取并渲染一份本地 md 文档。幂等（重复调用替换旧弹窗）。 */
export async function openDocViewer(path: string): Promise<void> {
  closeViewer();

  overlay = document.createElement("div");
  overlay.className = "md-modal-overlay doc-viewer";
  overlay.innerHTML = `
    <div class="md-modal doc-viewer-box">
      <div class="md-modal-bar">
        <span class="md-modal-title doc-viewer-path"></span>
        <button type="button" class="code-block-btn" data-act="close">关闭</button>
      </div>
      <div class="doc-viewer-body">
        <nav class="doc-viewer-toc"></nav>
        <div class="doc-viewer-content markdown-body"><div class="doc-viewer-loading">加载中…</div></div>
      </div>
    </div>`;
  overlay.querySelector<HTMLElement>(".doc-viewer-path")!.textContent = path;
  document.body.appendChild(overlay);

  overlay.querySelector('[data-act="close"]')!.addEventListener("click", closeViewer);
  overlay.addEventListener("click", (e) => { if (e.target === overlay) closeViewer(); });
  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") { closeViewer(); document.removeEventListener("keydown", onKey); }
  };
  document.addEventListener("keydown", onKey);

  const contentEl = overlay.querySelector<HTMLElement>(".doc-viewer-content")!;
  const tocEl = overlay.querySelector<HTMLElement>(".doc-viewer-toc")!;

  let text: string;
  try {
    const res = await fetch(`/api/doc?path=${encodeURIComponent(path)}`);
    if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
    text = await res.text();
  } catch (err) {
    contentEl.innerHTML = "";
    const div = document.createElement("div");
    div.className = "doc-viewer-loading";
    div.textContent = `文档加载失败：${String(err)}`;
    contentEl.appendChild(div);
    return;
  }
  if (!overlay.isConnected) return;

  contentEl.innerHTML = renderMarkdown(text);
  enhanceRenderedMarkdown(contentEl);

  // TOC：渲染后的标题（# 已降为 h3 起）收集成侧边目录，点击滚动定位。
  const headings = contentEl.querySelectorAll("h3, h4, h5, h6");
  if (headings.length > 1) {
    let seq = 0;
    for (const h of headings) {
      const id = `doc-h-${seq++}`;
      h.id = id;
      const item = document.createElement("a");
      item.href = `#${id}`;
      item.className = `doc-viewer-toc-item toc-${h.tagName.toLowerCase()}`;
      item.textContent = h.textContent ?? "";
      item.addEventListener("click", (e) => {
        e.preventDefault();
        h.scrollIntoView({ behavior: "smooth", block: "start" });
      });
      tocEl.appendChild(item);
    }
  } else {
    tocEl.style.display = "none";
  }
}

let installed = false;

/** 全局委托：点击 a.doc-link 打开内嵌预览。mount 时调一次即可。 */
export function installDocLinkHandler(): void {
  if (installed) return;
  installed = true;
  document.addEventListener("click", (e) => {
    const link = (e.target as HTMLElement).closest<HTMLElement>("a.doc-link");
    if (!link) return;
    e.preventDefault();
    const path = link.dataset.docPath;
    if (path) void openDocViewer(path);
  });
}
