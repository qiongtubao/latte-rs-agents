// @vitest-environment jsdom
//
// exportOutlineHtml 单元测试：大纲 → 自包含只读 HTML。
// 关注三件事：取文档走 /api/doc（路径编码）、产物自包含（无 script/link、
// 原始 HTML 被转义）、失败要把状态码抛给调用方（看板据此 toast）。
import { describe, it, expect, vi, beforeEach, afterEach, type Mock } from "vitest";
import { exportOutlineHtml } from "./markdown";

const MD = "# 大纲标题\n\n- 第一项\n- 第二项\n";
const OUTLINE = "docs/outlines/lat-1.md";

/** jsdom 没实现 Blob URL 静态方法（属性可能根本不存在），单独收窄一下。 */
type UrlStatics = {
  createObjectURL?: (blob: Blob) => string;
  revokeObjectURL?: (url: string) => void;
};
const urlStatics = URL as unknown as UrlStatics;

type FakeResponse = { ok: boolean; status: number; text: () => Promise<string> };
let fetchMock: Mock<[string], Promise<FakeResponse>>;
let createObjectURL: Mock<[Blob], string>;
let revokeObjectURL: Mock<[string], void>;
let openMock: Mock<[string, string], Window | null>;
let originalUrl: UrlStatics;

beforeEach(() => {
  fetchMock = vi.fn<[string], Promise<FakeResponse>>(async () => ({
    ok: true, status: 200, text: async () => MD,
  }));
  vi.stubGlobal("fetch", fetchMock);

  createObjectURL = vi.fn<[Blob], string>(() => "blob:outline");
  revokeObjectURL = vi.fn<[string], void>();
  originalUrl = { createObjectURL: urlStatics.createObjectURL, revokeObjectURL: urlStatics.revokeObjectURL };
  urlStatics.createObjectURL = createObjectURL;
  urlStatics.revokeObjectURL = revokeObjectURL;

  openMock = vi.fn<[string, string], Window | null>(() => null);
  vi.spyOn(window, "open").mockImplementation(openMock as unknown as typeof window.open);
});

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  vi.useRealTimers();
  urlStatics.createObjectURL = originalUrl.createObjectURL;
  urlStatics.revokeObjectURL = originalUrl.revokeObjectURL;
});

/** 取 blob 里的 HTML（jsdom 的 Blob 有 text()）。 */
async function exportedHtml(): Promise<string> {
  expect(createObjectURL).toHaveBeenCalledTimes(1);
  const blob = createObjectURL.mock.calls[0][0];
  return blob.text();
}

describe("exportOutlineHtml", () => {
  it("走 /api/doc 取 MD，产物含渲染后的标题与列表项", async () => {
    await exportOutlineHtml(OUTLINE);

    expect(fetchMock).toHaveBeenCalledWith(
      `/api/doc?path=${encodeURIComponent(OUTLINE)}`,
    );
    const html = await exportedHtml();
    // 标题级别 +2：# → h3（与聊天正文同一渲染管线）
    expect(html).toContain("<h3>大纲标题</h3>");
    expect(html).toContain("<li>第一项</li>");
    expect(html).toContain("<li>第二项</li>");
    expect(html).toContain("<title>任务大纲（导出）</title>");
  });

  it("产物自包含：无 <script>/<link>，MD 里的原始 HTML 被转义", async () => {
    fetchMock.mockResolvedValueOnce({
      ok: true,
      status: 200,
      text: async () => "# 标题\n\n<script>alert(1)</script>\n\n`code`\n",
    });
    await exportOutlineHtml(OUTLINE);

    const html = await exportedHtml();
    expect(html).not.toMatch(/<script/i);
    expect(html).not.toMatch(/<link\s/i);
    expect(html).toContain("&lt;script&gt;");
    // 样式内联，不引外部资源
    expect(html).toContain("<style>");
  });

  it("新窗口打开 blob URL，60s 后回收", async () => {
    vi.useFakeTimers();
    await exportOutlineHtml(OUTLINE);

    expect(openMock).toHaveBeenCalledWith("blob:outline", "_blank");
    expect(revokeObjectURL).not.toHaveBeenCalled();

    await vi.advanceTimersByTimeAsync(60_000);
    expect(revokeObjectURL).toHaveBeenCalledWith("blob:outline");
  });

  it("非 2xx 直接拒绝并带上状态码，不打开窗口", async () => {
    fetchMock.mockResolvedValueOnce({ ok: false, status: 404, text: async () => "" });

    await expect(exportOutlineHtml(OUTLINE)).rejects.toThrow("读大纲失败：404");
    expect(createObjectURL).not.toHaveBeenCalled();
    expect(openMock).not.toHaveBeenCalled();
  });
});
