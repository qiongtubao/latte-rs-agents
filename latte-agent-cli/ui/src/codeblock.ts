// 代码块工具条（renderMarkdown 输出的 <pre> 后处理）：hover 显示
// 「复制」按钮；围栏 info string 形如文件路径（```src/foo.rs）且宿主
// 支持 openLocation 时，额外显示「在编辑器中打开」。CLI 无宿主时只有复制。
//
// enhanceRenderedMarkdown 是所有 innerHTML = renderMarkdown(...) 之后
// 的统一后处理入口：代码块工具条 + mermaid 占位 div 的懒加载 SVG 渲染。
// 流式 delta 每帧都会整体重渲染再调一次，两个步骤都必须幂等。

import { getHost } from "./host";

/** info string 是否形如可跳转的文件路径（带扩展名、无空白、非 URL）。 */
function looksLikePath(info: string): boolean {
  if (!info || /\s/.test(info) || info.includes("://")) return false;
  return /^[\w@./+-]+\.[A-Za-z0-9]+$/.test(info);
}

/** 把 root 内每个 markdown <pre> 包进 .code-block 并加工具条。幂等。 */
export function enhanceCodeBlocks(root: HTMLElement): void {
  for (const pre of root.querySelectorAll("pre")) {
    if (pre.parentElement?.classList.contains("code-block")) continue;
    const codeEl = pre.querySelector("code");
    const info = codeEl?.className.match(/language-(\S+)/)?.[1] ?? "";

    const wrap = document.createElement("div");
    wrap.className = "code-block";
    pre.parentNode?.insertBefore(wrap, pre);
    wrap.appendChild(pre);

    const bar = document.createElement("div");
    bar.className = "code-block-bar";

    const copyBtn = document.createElement("button");
    copyBtn.type = "button";
    copyBtn.className = "code-block-btn";
    copyBtn.textContent = "复制";
    copyBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      const text = codeEl?.textContent ?? "";
      navigator.clipboard?.writeText(text).then(() => {
        copyBtn.textContent = "已复制";
        setTimeout(() => { copyBtn.textContent = "复制"; }, 1200);
      }).catch(() => {});
    });
    bar.appendChild(copyBtn);

    if (looksLikePath(info) && getHost()?.openLocation) {
      const openBtn = document.createElement("button");
      openBtn.type = "button";
      openBtn.className = "code-block-btn";
      openBtn.textContent = "在编辑器中打开";
      openBtn.title = info;
      openBtn.addEventListener("click", (e) => {
        e.stopPropagation();
        getHost()?.openLocation?.({ path: info });
      });
      bar.appendChild(openBtn);
    }

    wrap.insertBefore(bar, pre);
  }
}

// ─── Mermaid 懒渲染 ─────────────────────────────────────────────
// mermaid 体积大（~2MB），只在页面里真的出现 ```mermaid 块时才动态
// import，Vite 会把它拆成异步 chunk，主 bundle 不增重。

type MermaidModule = typeof import("mermaid");
let mermaidPromise: Promise<MermaidModule> | null = null;
let mermaidSeq = 0;

/** 渲染成功的图源原文（下载/复制用；dataset 放不下多行文本）。 */
const mermaidSources = new WeakMap<HTMLElement, string>();

function loadMermaid(): Promise<MermaidModule> {
  mermaidPromise ??= import("mermaid");
  return mermaidPromise;
}

/** 触发浏览器下载一个 Blob。 */
function downloadBlob(blob: Blob, filename: string): void {
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

/** SVG 字符串 → PNG 下载（canvas 2x 光栅化）。 */
function downloadPng(svgText: string, filename: string): void {
  const svgUrl = URL.createObjectURL(new Blob([svgText], { type: "image/svg+xml" }));
  const img = new Image();
  img.onload = () => {
    const w = img.naturalWidth || 800;
    const h = img.naturalHeight || 600;
    const canvas = document.createElement("canvas");
    canvas.width = w * 2;
    canvas.height = h * 2;
    const ctx = canvas.getContext("2d");
    if (!ctx) { URL.revokeObjectURL(svgUrl); return; }
    ctx.fillStyle = "#ffffff";
    ctx.fillRect(0, 0, canvas.width, canvas.height);
    ctx.drawImage(img, 0, 0, canvas.width, canvas.height);
    URL.revokeObjectURL(svgUrl);
    canvas.toBlob((b) => { if (b) downloadBlob(b, filename); }, "image/png");
  };
  img.onerror = () => URL.revokeObjectURL(svgUrl);
  img.src = svgUrl;
}

/** 当前放大的弹窗（同时最多一个）。 */
let mermaidModal: HTMLElement | null = null;

/** 放大查看：弹窗内滚轮缩放 + 拖拽平移，Esc/点遮罩关闭。 */
function openMermaidModal(svgHtml: string, source: string): void {
  mermaidModal?.remove();
  const overlay = document.createElement("div");
  overlay.className = "md-modal-overlay mermaid-modal";
  overlay.innerHTML = `
    <div class="md-modal mermaid-modal-box">
      <div class="md-modal-bar">
        <span class="md-modal-title">图表预览（滚轮缩放 · 拖拽平移）</span>
        <button type="button" class="code-block-btn" data-act="copy-src">复制图源</button>
        <button type="button" class="code-block-btn" data-act="dl-svg">下载 SVG</button>
        <button type="button" class="code-block-btn" data-act="dl-png">下载 PNG</button>
        <button type="button" class="code-block-btn" data-act="close">关闭</button>
      </div>
      <div class="mermaid-viewport"><div class="mermaid-canvas">${svgHtml}</div></div>
    </div>`;
  document.body.appendChild(overlay);
  mermaidModal = overlay;

  const close = () => { overlay.remove(); if (mermaidModal === overlay) mermaidModal = null; };
  overlay.addEventListener("click", (e) => { if (e.target === overlay) close(); });
  const onKey = (e: KeyboardEvent) => { if (e.key === "Escape") { close(); document.removeEventListener("keydown", onKey); } };
  document.addEventListener("keydown", onKey);

  const viewport = overlay.querySelector<HTMLElement>(".mermaid-viewport")!;
  const canvas = overlay.querySelector<HTMLElement>(".mermaid-canvas")!;
  let scale = 1, tx = 0, ty = 0;
  const apply = () => { canvas.style.transform = `translate(${tx}px, ${ty}px) scale(${scale})`; };
  viewport.addEventListener("wheel", (e) => {
    e.preventDefault();
    scale = Math.min(8, Math.max(0.2, scale * (e.deltaY < 0 ? 1.15 : 1 / 1.15)));
    apply();
  }, { passive: false });
  let drag: { x: number; y: number } | null = null;
  viewport.addEventListener("pointerdown", (e) => { drag = { x: e.clientX - tx, y: e.clientY - ty }; viewport.setPointerCapture(e.pointerId); });
  viewport.addEventListener("pointermove", (e) => { if (drag) { tx = e.clientX - drag.x; ty = e.clientY - drag.y; apply(); } });
  viewport.addEventListener("pointerup", () => { drag = null; });

  overlay.querySelector('[data-act="close"]')!.addEventListener("click", close);
  overlay.querySelector('[data-act="copy-src"]')!.addEventListener("click", (e) => {
    navigator.clipboard?.writeText(source).then(() => {
      const btn = e.currentTarget as HTMLElement;
      btn.textContent = "已复制";
      setTimeout(() => { btn.textContent = "复制图源"; }, 1200);
    }).catch(() => {});
  });
  overlay.querySelector('[data-act="dl-svg"]')!.addEventListener("click", () =>
    downloadBlob(new Blob([svgHtml], { type: "image/svg+xml" }), "diagram.svg"));
  overlay.querySelector('[data-act="dl-png"]')!.addEventListener("click", () =>
    downloadPng(svgHtml, "diagram.png"));
}

/** 渲染完成的 mermaid 图：加 hover 工具条 + 点击放大。幂等。 */
function enhanceMermaidFigure(el: HTMLElement): void {
  if (el.querySelector(":scope > .mermaid-bar")) return;
  const bar = document.createElement("div");
  bar.className = "mermaid-bar";
  bar.innerHTML = `<button type="button" class="code-block-btn" data-act="zoom">放大</button>`;
  el.appendChild(bar);
  const open = () => {
    const svg = el.querySelector("svg");
    if (svg) openMermaidModal(svg.outerHTML, mermaidSources.get(el) ?? "");
  };
  bar.querySelector('[data-act="zoom"]')!.addEventListener("click", (e) => { e.stopPropagation(); open(); });
  el.querySelector("svg")?.addEventListener("click", open);
  el.classList.add("mermaid-zoomable");
}

async function renderMermaidNode(el: HTMLElement): Promise<void> {
  // 渲染是异步的，期间流式重渲染可能已把节点换掉/删掉。
  if (el.dataset.mermaidPending) return;
  el.dataset.mermaidPending = "1";
  const source = el.textContent ?? "";
  try {
    const mermaid = (await loadMermaid()).default;
    mermaid.initialize({ startOnLoad: false, securityLevel: "strict" });
    const { svg } = await mermaid.render(`mmd-${Date.now()}-${mermaidSeq++}`, source);
    if (!el.isConnected) return;
    el.innerHTML = svg;
    el.dataset.mermaidDone = "1";
    mermaidSources.set(el, source);
    enhanceMermaidFigure(el);
  } catch {
    // 图源非法（或流式中途的半成品）：回退为带工具条的普通代码块，
    // 保留原文可见，不静默吞掉内容。
    if (!el.isConnected) return;
    const pre = document.createElement("pre");
    const code = document.createElement("code");
    code.className = "language-mermaid";
    code.textContent = source;
    pre.appendChild(code);
    el.replaceChildren(pre);
    el.classList.add("mermaid-error");
    el.dataset.mermaidDone = "1";
    enhanceCodeBlocks(el);
  } finally {
    delete el.dataset.mermaidPending;
  }
}

export interface EnhanceOptions {
  /** 流式 delta 中途为 true：跳过 mermaid 渲染（图源大概率不完整，
   *  等 is_complete 的终态渲染再出 SVG），其余增强照常。 */
  deferMermaid?: boolean;
}

/** renderMarkdown 输出的统一后处理：代码块工具条 + mermaid SVG。幂等。 */
export function enhanceRenderedMarkdown(root: HTMLElement, opts?: EnhanceOptions): void {
  enhanceCodeBlocks(root);
  for (const el of root.querySelectorAll<HTMLElement>(".mermaid")) {
    if (el.dataset.mermaidDone || el.dataset.mermaidPending) continue;
    if (opts?.deferMermaid) continue;
    void renderMermaidNode(el);
  }
}
