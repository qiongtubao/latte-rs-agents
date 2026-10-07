// 代码块工具条（renderMarkdown 输出的 <pre> 后处理）：hover 显示
// 「复制」按钮；围栏 info string 形如文件路径（```src/foo.rs）且宿主
// 支持 openLocation 时，额外显示「在编辑器中打开」。CLI 无宿主时只有复制。
//
// enhanceRenderedMarkdown 是所有 innerHTML = renderMarkdown(...) 之后
// 的统一后处理入口：代码块工具条 + mermaid 占位 div 的懒加载 SVG 渲染。
// 流式 delta 每帧都会整体重渲染再调一次，两个步骤都必须幂等。

import { getHost } from "./host";
import { repairMermaidSource } from "./mermaid_repair";

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

/** SVG 字符串 → PNG 下载（canvas 2x 光栅化）。使用 data URI 避免 blob URL 导致 canvas tainted 报 SecurityError。 */
function downloadPng(svgText: string, filename: string): void {
  let cleanSvg = svgText;
  if (!cleanSvg.includes('xmlns="http://www.w3.org/2000/svg"')) {
    cleanSvg = cleanSvg.replace('<svg ', '<svg xmlns="http://www.w3.org/2000/svg" ');
  }
  const svgUrl = `data:image/svg+xml;charset=utf-8,${encodeURIComponent(cleanSvg)}`;
  const img = new Image();
  img.onload = () => {
    const w = (img.naturalWidth || 800) * 2;
    const h = (img.naturalHeight || 600) * 2;
    const canvas = document.createElement("canvas");
    canvas.width = w;
    canvas.height = h;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    ctx.fillStyle = "#ffffff";
    ctx.fillRect(0, 0, canvas.width, canvas.height);
    ctx.drawImage(img, 0, 0, canvas.width, canvas.height);
    try {
      canvas.toBlob((b) => {
        if (b) downloadBlob(b, filename);
      }, "image/png");
    } catch {
      // 回退直接下载原 SVG
      downloadBlob(new Blob([svgText], { type: "image/svg+xml" }), filename.replace(/\.png$/i, ".svg"));
    }
  };
  img.onerror = () => {
    // 回退下载原 SVG
    downloadBlob(new Blob([svgText], { type: "image/svg+xml" }), filename.replace(/\.png$/i, ".svg"));
  };
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
        <span class="md-modal-title">图表预览（光标聚焦无限缩放 · 拖拽平移 · 双击居中）</span>
        <button type="button" class="code-block-btn" data-act="reset">重置居中</button>
        <button type="button" class="code-block-btn" data-act="copy-src">复制图源</button>
        <button type="button" class="code-block-btn" data-act="dl-svg">下载 SVG</button>
        <button type="button" class="code-block-btn" data-act="dl-png">下载 PNG</button>
        <button type="button" class="code-block-btn" data-act="close">关闭</button>
      </div>
      <div class="mermaid-viewport">
        <div class="mermaid-canvas">${svgHtml}</div>
      </div>
    </div>`;
  document.body.appendChild(overlay);
  mermaidModal = overlay;

  // 先声明 onKey 再声明 close：close 引用 onKey 做 removeEventListener，
  // 避免「close 中读 onKey 时 onKey 还在 TDZ」的潜在时序风险
  // （目前所有触发 close 的事件都是异步的所以正常路径不会触发，但
  // 任何后续在 setup 阶段同步触发 close 的改动都会撞上 TDZ）。
  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      close();
    }
  };
  const close = () => {
    overlay.remove();
    document.removeEventListener("keydown", onKey);
    if (mermaidModal === overlay) mermaidModal = null;
  };
  overlay.addEventListener("click", (e) => { if (e.target === overlay) close(); });
  document.addEventListener("keydown", onKey);

  const viewport = overlay.querySelector<HTMLElement>(".mermaid-viewport")!;
  const canvas = overlay.querySelector<HTMLElement>(".mermaid-canvas")!;
  const svgEl = canvas.querySelector("svg");
  if (svgEl) {
    svgEl.style.maxWidth = "none";
    svgEl.style.display = "block";
  }

  let scale = 1;
  let tx = 0;
  let ty = 0;

  const apply = () => {
    canvas.style.transform = `translate(${tx}px, ${ty}px) scale(${scale})`;
  };

  // 自动等比适配视口居中
  const fitToViewport = () => {
    const vRect = viewport.getBoundingClientRect();
    const sRect = svgEl ? svgEl.getBoundingClientRect() : canvas.getBoundingClientRect();
    const rawW = (svgEl ? (parseFloat(svgEl.getAttribute("width") || "0") || sRect.width / scale) : sRect.width / scale) || 600;
    const rawH = (svgEl ? (parseFloat(svgEl.getAttribute("height") || "0") || sRect.height / scale) : sRect.height / scale) || 400;
    scale = Math.min(2.0, Math.max(0.2, Math.min((vRect.width * 0.85) / rawW, (vRect.height * 0.85) / rawH)));
    tx = (vRect.width - rawW * scale) / 2;
    ty = (vRect.height - rawH * scale) / 2;
    apply();
  };

  // 初始布局完成后自适应一次
  requestAnimationFrame(fitToViewport);

  // 以鼠标光标为中心缩放（Cursor-Centered Zoom）：滚轮缩放向鼠标所指字迹聚焦，支持高达 100x 放大
  viewport.addEventListener("wheel", (e) => {
    e.preventDefault();
    const vRect = viewport.getBoundingClientRect();
    const factor = e.deltaY < 0 ? 1.25 : 1 / 1.25;
    const newScale = Math.min(100, Math.max(0.05, scale * factor));

    const vx = e.clientX - vRect.left;
    const vy = e.clientY - vRect.top;
    const cx = (vx - tx) / scale;
    const cy = (vy - ty) / scale;

    tx = vx - cx * newScale;
    ty = vy - cy * newScale;
    scale = newScale;
    apply();
  }, { passive: false });

  // 拖拽平移
  let drag: { x: number; y: number } | null = null;
  viewport.addEventListener("pointerdown", (e) => {
    if (e.button !== 0) return;
    drag = { x: e.clientX - tx, y: e.clientY - ty };
    viewport.setPointerCapture(e.pointerId);
  });
  viewport.addEventListener("pointermove", (e) => {
    if (drag) {
      tx = e.clientX - drag.x;
      ty = e.clientY - drag.y;
      apply();
    }
  });
  viewport.addEventListener("pointerup", () => { drag = null; });

  // 双击居中还原
  viewport.addEventListener("dblclick", (e) => {
    e.preventDefault();
    e.stopPropagation();
    fitToViewport();
  });

  overlay.querySelector('[data-act="reset"]')!.addEventListener("click", fitToViewport);
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
  let renderId = `mmd-${Date.now()}-${mermaidSeq++}`;
  const cleanup = (id: string) => {
    // 清理 mermaid 在某些异常路径下向 body 遗留的临时容器
    document.getElementById(`d${id}`)?.remove();
    document.getElementById(id)?.remove();
  };
  try {
    const mermaid = (await loadMermaid()).default;
    mermaid.initialize({
      startOnLoad: false,
      securityLevel: "strict",
      // 禁止 mermaid 遇到语法错误时向 document.body 注入包含版本号的错误 SVG
      suppressErrorRendering: true,
    });
    let svg: string;
    try {
      ({ svg } = await mermaid.render(renderId, source));
    } catch (first) {
      // 模型手写的图源常有可机械修补的语法错误（节点文字含括号没加引号等），
      // 修补后再试一次；修不了就按原报错回退。
      cleanup(renderId);
      const repaired = repairMermaidSource(source);
      if (!repaired) throw first;
      renderId = `mmd-${Date.now()}-${mermaidSeq++}`;
      try {
        ({ svg } = await mermaid.render(renderId, repaired));
      } catch {
        throw first;
      }
    }
    if (!el.isConnected) return;
    el.innerHTML = svg;
    el.dataset.mermaidDone = "1";
    mermaidSources.set(el, source);
    enhanceMermaidFigure(el);
  } catch (err) {
    cleanup(renderId);
    // 图源非法（或流式中途的半成品）：回退为带工具条的普通代码块，
    // 保留原文可见，不静默吞掉内容；同时写明为什么没出图。
    if (!el.isConnected) return;
    const note = document.createElement("div");
    note.className = "mermaid-error-note";
    const reason = String((err as Error)?.message ?? err).split("\n")[0].slice(0, 200);
    note.textContent = `图表语法有误，未能渲染，下方为图源。${reason ? `（${reason}）` : ""}`;
    const pre = document.createElement("pre");
    const code = document.createElement("code");
    code.className = "language-mermaid";
    code.textContent = source;
    pre.appendChild(code);
    el.replaceChildren(note, pre);
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
