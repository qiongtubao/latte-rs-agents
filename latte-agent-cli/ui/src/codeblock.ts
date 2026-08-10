// 代码块工具条（renderMarkdown 输出的 <pre> 后处理）：hover 显示
// 「复制」按钮；围栏 info string 形如文件路径（```src/foo.rs）且宿主
// 支持 openLocation 时，额外显示「在编辑器中打开」。CLI 无宿主时只有复制。

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
