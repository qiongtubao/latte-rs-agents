// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach } from "vitest";
import { openImageModal, closeImageModal, installImageModalHandler } from "./image_modal";

describe("image_modal", () => {
  beforeEach(() => {
    closeImageModal();
  });

  afterEach(() => {
    closeImageModal();
  });

  it("openImageModal 成功渲染图片弹窗并在 DOM 中显示", () => {
    openImageModal("/api/doc-asset?path=test.svg", "测试架构图");
    const modal = document.querySelector(".img-modal");
    expect(modal).not.toBeNull();
    expect(modal!.querySelector(".md-modal-title")?.textContent).toContain("测试架构图");
    const img = modal!.querySelector<HTMLImageElement>(".img-modal-canvas img");
    expect(img).not.toBeNull();
    expect(img!.src).toContain("/api/doc-asset?path=test.svg");
    expect(img!.alt).toBe("测试架构图");
  });

  it("点击关闭按钮或按 Escape 键关闭弹窗", () => {
    openImageModal("/api/doc-asset?path=test.svg", "测试架构图");
    const closeBtn = document.querySelector<HTMLButtonElement>('.img-modal [data-act="close"]');
    expect(closeBtn).not.toBeNull();
    closeBtn!.click();
    expect(document.querySelector(".img-modal")).toBeNull();

    // 再次打开并通过 Escape 键关闭
    openImageModal("/api/doc-asset?path=test.svg", "测试架构图");
    expect(document.querySelector(".img-modal")).not.toBeNull();
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    expect(document.querySelector(".img-modal")).toBeNull();
  });

  it("支持 SVG 与 PNG 模式切换按钮", async () => {
    openImageModal("/api/doc-asset?path=test.svg", "测试架构图");
    const modal = document.querySelector(".img-modal");
    expect(modal).not.toBeNull();
    const toggleBtn = modal!.querySelector<HTMLButtonElement>('[data-act="toggle-mode"]');
    expect(toggleBtn).not.toBeNull();
    expect(toggleBtn!.textContent).toContain("切换到 PNG 高清模式");
  });

  it("重置缩放按钮重置画布 transform", () => {
    openImageModal("/api/doc-asset?path=test.png", "位图测试");
    const modal = document.querySelector(".img-modal");
    const canvas = modal!.querySelector<HTMLElement>(".img-modal-canvas")!;
    canvas.style.transform = "scale(5)";
    const resetBtn = modal!.querySelector<HTMLButtonElement>('[data-act="reset-zoom"]');
    resetBtn!.click();
    expect(canvas.style.transform).toBe("translate(0px, 0px) scale(1)");
  });

  it("installImageModalHandler 拦截 markdown-body 中的图片点击", () => {
    installImageModalHandler();
    const container = document.createElement("div");
    container.className = "markdown-body";
    const img = document.createElement("img");
    img.src = "/api/doc-asset?path=foo.png";
    img.alt = "结构图";
    container.appendChild(img);
    document.body.appendChild(container);

    img.click();
    const modal = document.querySelector(".img-modal");
    expect(modal).not.toBeNull();
    expect(modal!.querySelector(".img-modal-canvas img")?.getAttribute("src")).toBe(
      "/api/doc-asset?path=foo.png",
    );
    container.remove();
  });

  it("installImageModalHandler 拦截 a.img-link 点击，防止页面跳转", () => {
    installImageModalHandler();
    const container = document.createElement("div");
    container.className = "markdown-body";
    container.innerHTML = `<a class="img-link" href="/api/doc-asset?path=bar.svg" data-img-src="/api/doc-asset?path=bar.svg"><img src="/api/doc-asset?path=bar.svg" alt="矢量图"></a>`;
    document.body.appendChild(container);

    const img = container.querySelector("img")!;
    img.click();
    const modal = document.querySelector(".img-modal");
    expect(modal).not.toBeNull();
    expect(modal!.querySelector(".img-modal-canvas img")?.getAttribute("src")).toBe(
      "/api/doc-asset?path=bar.svg",
    );
    container.remove();
  });
});
