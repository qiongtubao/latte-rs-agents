// 图片大图预览查看器（Markdown 中的图片点击/双击放大）。
// 支持：
// 1. 矢量 SVG 与高清 PNG 模式切换（解决小尺寸 SVG 在 img 标签中放大发虚或太小看不清的问题）；
// 2. 滚轮平滑缩放、拖拽平移、双击重置；
// 3. 原图下载（支持导出 SVG 或高清 PNG，无 canvas tainted 异常）；
// 4. Esc 或点击遮罩关闭。

let imgModal: HTMLElement | null = null;

export function closeImageModal(): void {
  imgModal?.remove();
  imgModal = null;
}

/** 触发浏览器下载 Blob */
function downloadBlob(blob: Blob, filename: string): void {
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

/** 将 SVG 文本光栅化为高清 PNG DataURL（宽度默认为 2000px，保证小图标放大后依然极度清晰） */
function rasterizeSvgToPng(svgText: string, targetWidth = 2000): Promise<{ pngUrl: string; w: number; h: number }> {
  return new Promise((resolve, reject) => {
    let clean = svgText;
    if (!clean.includes('xmlns="http://www.w3.org/2000/svg"')) {
      clean = clean.replace("<svg ", '<svg xmlns="http://www.w3.org/2000/svg" ');
    }
    const dataUri = `data:image/svg+xml;charset=utf-8,${encodeURIComponent(clean)}`;
    const img = new Image();
    img.onload = () => {
      const nw = img.naturalWidth || 400;
      const nh = img.naturalHeight || 200;
      const aspect = nh / nw;
      const w = Math.max(targetWidth, nw * 2);
      const h = Math.round(w * aspect);
      const canvas = document.createElement("canvas");
      canvas.width = w;
      canvas.height = h;
      const ctx = canvas.getContext("2d");
      if (!ctx) return reject(new Error("no canvas context"));
      ctx.fillStyle = "#ffffff";
      ctx.fillRect(0, 0, w, h);
      ctx.drawImage(img, 0, 0, w, h);
      try {
        const pngUrl = canvas.toDataURL("image/png");
        resolve({ pngUrl, w, h });
      } catch (e) {
        reject(e);
      }
    };
    img.onerror = () => reject(new Error("Image failed to load data URI"));
    img.src = dataUri;
  });
}

/** 弹出图片预览模态框。 */
export function openImageModal(src: string, alt = "图片预览"): void {
  closeImageModal();

  const isSvg = /\.svg(?:[#?].*)?$/i.test(src) || src.includes("image/svg+xml");

  const overlay = document.createElement("div");
  overlay.className = "md-modal-overlay img-modal";
  overlay.innerHTML = `
    <div class="md-modal img-modal-box">
      <div class="md-modal-bar">
        <span class="md-modal-title">${alt || "图片预览"}（滚轮缩放 · 拖拽平移 · 双击还原）</span>
        ${isSvg ? '<button type="button" class="code-block-btn" data-act="toggle-mode">切换到 PNG 高清模式</button>' : ""}
        <button type="button" class="code-block-btn" data-act="reset-zoom">重置缩放</button>
        <a class="code-block-btn" href="${src}" target="_blank" rel="noopener noreferrer" style="text-decoration:none;display:inline-flex;align-items:center;">原图</a>
        <button type="button" class="code-block-btn" data-act="dl">下载</button>
        <button type="button" class="code-block-btn" data-act="close">关闭</button>
      </div>
      <div class="img-modal-viewport">
        <div class="img-modal-canvas">
          <img class="img-modal-target" src="${src}" alt="${alt}">
        </div>
      </div>
    </div>`;
  document.body.appendChild(overlay);
  imgModal = overlay;
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
    if (imgModal === overlay) imgModal = null;
  };
  overlay.querySelector('[data-act="close"]')!.addEventListener("click", close);
  overlay.addEventListener("click", (e) => {
    if (e.target === overlay) close();
  });
  document.addEventListener("keydown", onKey);
  const viewport = overlay.querySelector<HTMLElement>(".img-modal-viewport")!;
  const canvas = overlay.querySelector<HTMLElement>(".img-modal-canvas")!;
  const toggleBtn = overlay.querySelector<HTMLButtonElement>('[data-act="toggle-mode"]');
  const resetBtn = overlay.querySelector<HTMLButtonElement>('[data-act="reset-zoom"]');

  let scale = 1;
  let tx = 0;
  let ty = 0;
  let currentMode: "svg" | "png" = isSvg ? "svg" : "png";
  let cachedSvgText: string | null = null;
  let cachedPngUrl: string | null = null;

  const applyTransform = () => {
    canvas.style.transform = `translate(${tx}px, ${ty}px) scale(${scale})`;
  };

  const fitToViewport = () => {
    const vRect = viewport.getBoundingClientRect();
    // 在 jsdom 测试环境中，getBoundingClientRect 为 0，回退为 1x 居中
    if (!vRect.width || !vRect.height) {
      scale = 1;
      tx = 0;
      ty = 0;
      applyTransform();
      return;
    }
    const targetEl = canvas.querySelector("svg, img") as HTMLElement | null;
    const rawW = (targetEl ? (parseFloat(targetEl.getAttribute("width") || "0") || targetEl.getBoundingClientRect().width / scale) : 600) || 600;
    const rawH = (targetEl ? (parseFloat(targetEl.getAttribute("height") || "0") || targetEl.getBoundingClientRect().height / scale) : 400) || 400;
    scale = Math.min(2.0, Math.max(0.1, Math.min((vRect.width * 0.85) / rawW, (vRect.height * 0.85) / rawH)));
    tx = (vRect.width - rawW * scale) / 2;
    ty = (vRect.height - rawH * scale) / 2;
    applyTransform();
  };

  resetBtn?.addEventListener("click", fitToViewport);

  // 光标聚焦无限缩放（0.05x ~ 100x），文字在光标处永不移出视野
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
    applyTransform();
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
      applyTransform();
    }
  });
  viewport.addEventListener("pointerup", () => {
    drag = null;
  });

  // 双击快速重置居中
  viewport.addEventListener("dblclick", (e) => {
    e.preventDefault();
    e.stopPropagation();
    fitToViewport();
  });
  // 如果是 SVG 文件，异步拉取 SVG 源码，实现矢量 DOM 内联展示（避免 img 标签的栅格模糊）
  if (isSvg) {
    void fetch(src)
      .then(async (res) => {
        if (res.ok) {
          const text = await res.text();
          if (text.includes("<svg") && overlay.isConnected) {
            cachedSvgText = text;
            renderSvgMode();
          }
        }
      })
      .catch(() => {});
  }

  function renderSvgMode(): void {
    if (!cachedSvgText) return;
    currentMode = "svg";
    if (toggleBtn) toggleBtn.textContent = "切换到 PNG 高清模式";
    canvas.innerHTML = cachedSvgText;
    const svgEl = canvas.querySelector("svg");
    if (svgEl) {
      svgEl.classList.add("img-modal-inline-svg");
      // 确保有 viewBox，并将样式设为自适应大尺寸展示
      const vb = svgEl.getAttribute("viewBox");
      const w = parseFloat(svgEl.getAttribute("width") || "0");
      const h = parseFloat(svgEl.getAttribute("height") || "0");
      svgEl.removeAttribute("width");
      svgEl.removeAttribute("height");
    }
    requestAnimationFrame(fitToViewport);
  }

  async function renderPngMode(): Promise<void> {
    currentMode = "png";
    if (toggleBtn) toggleBtn.textContent = "切换到 SVG 矢量模式";

    if (!cachedPngUrl && cachedSvgText) {
      if (toggleBtn) toggleBtn.textContent = "生成高清 PNG 中…";
      try {
        const { pngUrl } = await rasterizeSvgToPng(cachedSvgText, 2000);
        cachedPngUrl = pngUrl;
      } catch {
        // 失败时切回矢量模式
        renderSvgMode();
        return;
      }
    }

    if (toggleBtn) toggleBtn.textContent = "切换到 SVG 矢量模式";
    const targetSrc = cachedPngUrl || src;
    canvas.innerHTML = `<img class="img-modal-target img-modal-png-mode" src="${targetSrc}" alt="${alt}">`;
  }

  toggleBtn?.addEventListener("click", () => {
    if (currentMode === "svg") {
      void renderPngMode();
    } else {
      renderSvgMode();
    }
  });

  overlay.querySelector('[data-act="close"]')!.addEventListener("click", close);
  overlay.querySelector('[data-act="dl"]')!.addEventListener("click", () => {
    if (currentMode === "png" && cachedPngUrl) {
      const a = document.createElement("a");
      a.href = cachedPngUrl;
      a.download = `${alt || "image"}-hd.png`;
      a.click();
    } else if (cachedSvgText) {
      downloadBlob(new Blob([cachedSvgText], { type: "image/svg+xml" }), `${alt || "image"}.svg`);
    } else {
      const a = document.createElement("a");
      a.href = src;
      a.download = alt || "image";
      a.click();
    }
  });
}

let installed = false;

/** 全局委托：点击或双击 Markdown 内容中的图片或 img-link 时打开大图模态，防止 404 跳转。 */
export function installImageModalHandler(): void {
  if (installed) return;
  installed = true;

  const triggerModal = (target: HTMLElement, e: MouseEvent) => {
    // 1. 如果点击的是 a.img-link 或其内部子节点
    const imgLink = target.closest<HTMLElement>("a.img-link");
    if (imgLink) {
      e.preventDefault();
      e.stopPropagation();
      const imgSrc = imgLink.dataset.imgSrc || imgLink.getAttribute("href") || "";
      const imgEl = imgLink.querySelector("img");
      const alt = imgEl?.getAttribute("alt") || imgLink.textContent?.trim() || "图片预览";
      if (imgSrc) void openImageModal(imgSrc, alt);
      return true;
    }

    // 2. 如果点击的是 .msg-content 或 .markdown-body 内的 img
    if (target.tagName.toLowerCase() === "img") {
      const isMdImg = target.closest(".msg-content, .markdown-body");
      if (isMdImg) {
        const parentLink = target.closest("a");
        if (parentLink) {
          e.preventDefault();
          e.stopPropagation();
        }
        const img = target as HTMLImageElement;
        const src = img.getAttribute("src") || "";
        const alt = img.getAttribute("alt") || "图片预览";
        if (src) void openImageModal(src, alt);
        return true;
      }
    }
    return false;
  };

  document.addEventListener("click", (e) => {
    triggerModal(e.target as HTMLElement, e);
  });

  // 支持双击同样打开或阻止外层默认跳转
  document.addEventListener("dblclick", (e) => {
    const target = e.target as HTMLElement;
    const handled = triggerModal(target, e);
    if (!handled && (target.closest("a.img-link") || (target.tagName.toLowerCase() === "img" && target.closest(".msg-content, .markdown-body")))) {
      e.preventDefault();
      e.stopPropagation();
    }
  });
}
