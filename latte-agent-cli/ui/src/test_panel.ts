// 模型测试弹层（用户需求：5 种模式 / 图片输入 / 图片生成能力探测）。
//
// 设计：单例 modal 浮在主界面之上。HTML 框架在 index.html 里，
// 这里只接管 DOM 行为 + 调用后端 test.rs。
//
// 后端端点（见 `api.ts`）：
//   - `testModel({def, mode, prompt?, images?})` → 三种模式共一个 POST
//   - `getModelCapabilities(key)` → 能力探测
//
// Tab 说明（与后端 mode 一一对应）：
//   1. 连通测试：仅 GET <base_url>/v1/models，不发 chat。
//   2. AiClient 模式：走 latte_ai::client::AiClient::chat（实际生产代码）。
//   3. raw HTTP 模式：reqwest 直 POST，绕过 AiClient 中间处理。
//   4. 图片输入：multipart + base64 列表塞进 user content。
//   5. 图片生成：能力探测展示（不支持时给说明）。
import {
  listModels,
  testModel,
  getModelCapabilities,
  type ModelDef,
  type ModelWithSource,
  type TestMode,
  type TestModelResponse,
  type ModelCapabilities,
} from "./api";

interface UIBinding {
  /** 整个 modal overlay（包含 .modal 与 .modal-body）。 */
  overlayEl: HTMLElement;
  /** 「×」关闭按钮。 */
  closeBtn: HTMLButtonElement;
  /** 顶部 model selector（option 的 value 是 `provider/name`，空串代表未选）。 */
  modelSelect: HTMLSelectElement;
  /** 「查看能力」按钮 —— 调 `getModelCapabilities`。 */
  capabilitiesBtn: HTMLButtonElement;
  /** 能力徽章区：显示 supports_image_input / supports_image_generation。 */
  capabilityBadgesEl: HTMLElement;
  /** 5 个 tab 按钮。 */
  tabBtns: NodeListOf<HTMLButtonElement> | HTMLButtonElement[];
  /** 5 个 tab pane。 */
  tabPanes: NodeListOf<HTMLElement> | HTMLElement[];
  /** 共用 prompt textarea（连通测试不用）。 */
  promptInput: HTMLTextAreaElement;
  /** 图片输入 tab：file input + 缩略图容器 + 清空按钮。 */
  imageFileInput: HTMLInputElement;
  imagePreviewEl: HTMLElement;
  imageClearBtn: HTMLButtonElement;
  /** 5 个「运行 XXX 测试」按钮。 */
  runConnectivityBtn: HTMLButtonElement;
  runAiclientBtn: HTMLButtonElement;
  runHttpBtn: HTMLButtonElement;
  runImageBtn: HTMLButtonElement;
  /** 结果面板。 */
  resultStatusEl: HTMLElement;
  resultLatencyEl: HTMLElement;
  resultBodyEl: HTMLElement;
  resultAvailableModelsEl: HTMLElement;
}

export interface TestDialog {
  /** 打开弹层。`def` 来自 models_panel 当前编辑的 form（可能是新建未保存的）。 */
  open(opts: { def: ModelDef; key: string | null }): void;
  close(): void;
  isOpen(): boolean;
}

export function mountTestDialog(opts: {
  container: UIBinding;
  getModels: () => ModelWithSource[];
}): TestDialog {
  const { container, getModels } = opts;
  let isOpen = false;
  let currentDef: ModelDef | null = null;
  let currentKey: string | null = null;
  let images: string[] = [];
  let running = false;
  let lastCapabilities: ModelCapabilities | null = null;

  // 关闭（× / Esc / overlay 点击）
  container.closeBtn.addEventListener("click", () => close());
  container.overlayEl.addEventListener("click", (e) => {
    if (e.target === container.overlayEl) close();
  });
  document.addEventListener("keydown", (e) => {
    if (isOpen && e.key === "Escape") close();
  });

  // tab 切换
  const tabBtns = Array.from(container.tabBtns);
  const tabPanes = Array.from(container.tabPanes);
  tabBtns.forEach((btn) => {
    btn.addEventListener("click", () => {
      const target = btn.dataset.tab;
      if (!target) return;
      tabBtns.forEach((b) => b.classList.toggle("active", b === btn));
      tabPanes.forEach((p) => {
        p.classList.toggle("active", p.dataset.pane === target);
      });
    });
  });

  // model 选择变化时，自动拉能力
  container.modelSelect.addEventListener("change", () => {
    const key = container.modelSelect.value;
    currentKey = key || null;
    if (key) {
      void refreshCapabilities(key);
    } else {
      lastCapabilities = null;
      renderCapabilityBadges(null);
    }
  });

  container.capabilitiesBtn.addEventListener("click", () => {
    const key = container.modelSelect.value;
    if (key) {
      void refreshCapabilities(key);
    } else {
      setStatus("请先选一个 model", true);
    }
  });

  // 图片上传
  container.imageFileInput.addEventListener("change", () => {
    const file = container.imageFileInput.files?.[0];
    if (!file) return;
    if (!file.type.startsWith("image/")) {
      setStatus("请选图片文件（image/*）", true);
      return;
    }
    const reader = new FileReader();
    reader.onload = () => {
      const result = reader.result;
      if (typeof result !== "string") {
        setStatus("FileReader 返回非字符串", true);
        return;
      }
      images = [result];
      renderImagePreview();
    };
    reader.onerror = () => setStatus(`读取失败: ${reader.error?.message ?? "?"}`, true);
    reader.readAsDataURL(file);
  });
  container.imageClearBtn.addEventListener("click", () => {
    images = [];
    container.imageFileInput.value = "";
    renderImagePreview();
  });

  // 4 个运行按钮（图片生成 tab 单独处理）
  container.runConnectivityBtn.addEventListener("click", () => void run("connectivity"));
  container.runAiclientBtn.addEventListener("click", () => void run("aiclient"));
  container.runHttpBtn.addEventListener("click", () => void run("http"));
  container.runImageBtn.addEventListener("click", () => void run("image"));

  function close(): void {
    container.overlayEl.classList.add("hidden");
    isOpen = false;
  }

  function setStatus(msg: string, error = false): void {
    container.resultStatusEl.textContent = msg;
    container.resultStatusEl.classList.toggle("error", error);
  }

  function open(opts: { def: ModelDef; key: string | null }): void {
    currentDef = opts.def;
    currentKey = opts.key;
    refreshModelSelect();
    if (opts.key) {
      container.modelSelect.value = opts.key;
      void refreshCapabilities(opts.key);
    } else {
      container.modelSelect.value = "";
      lastCapabilities = null;
      renderCapabilityBadges(null);
    }
    // 清空之前的结果与图片
    container.promptInput.value = "你好，请用一句话介绍自己。";
    images = [];
    container.imageFileInput.value = "";
    renderImagePreview();
    setResult(null);
    setStatus("就绪");
    container.overlayEl.classList.remove("hidden");
    isOpen = true;
  }

  function refreshModelSelect(): void {
    const models = getModels();
    container.modelSelect.replaceChildren();
    const placeholder = document.createElement("option");
    placeholder.value = "";
    placeholder.textContent = "（选择 model）";
    container.modelSelect.appendChild(placeholder);
    for (const m of models) {
      const opt = document.createElement("option");
      opt.value = m.key;
      opt.textContent = `${m.key}${m.name !== m.key ? `  —  ${m.name}` : ""}`;
      container.modelSelect.appendChild(opt);
    }
  }

  async function refreshCapabilities(key: string): Promise<void> {
    setStatus("查询能力中…");
    try {
      const caps = await getModelCapabilities(key);
      lastCapabilities = caps;
      renderCapabilityBadges(caps);
      setStatus("已查询能力");
    } catch (e) {
      setStatus(`查询能力失败: ${(e as Error).message}`, true);
      renderCapabilityBadges(null);
    }
  }

  function renderCapabilityBadges(caps: ModelCapabilities | null): void {
    container.capabilityBadgesEl.replaceChildren();
    if (!caps) {
      const note = document.createElement("span");
      note.className = "capability-note";
      note.textContent = "未查询";
      container.capabilityBadgesEl.appendChild(note);
      return;
    }
    const imgIn = document.createElement("span");
    imgIn.className = `cap-badge ${caps.supports_image_input ? "on" : "off"}`;
    imgIn.textContent = `图片输入: ${caps.supports_image_input ? "✓" : "✗"}`;
    container.capabilityBadgesEl.appendChild(imgIn);
    const imgGen = document.createElement("span");
    imgGen.className = `cap-badge ${caps.supports_image_generation ? "on" : "off"}`;
    imgGen.textContent = `图片生成: ${caps.supports_image_generation ? "✓" : "✗"}`;
    container.capabilityBadgesEl.appendChild(imgGen);
  }

  function renderImagePreview(): void {
    container.imagePreviewEl.replaceChildren();
    if (images.length === 0) {
      const note = document.createElement("span");
      note.className = "image-preview-empty";
      note.textContent = "未选图片";
      container.imagePreviewEl.appendChild(note);
      return;
    }
    for (const dataUrl of images) {
      const img = document.createElement("img");
      img.src = dataUrl;
      img.className = "image-preview-img";
      container.imagePreviewEl.appendChild(img);
    }
  }

  function setResult(r: TestModelResponse | null): void {
    if (!r) {
      container.resultStatusEl.textContent = "";
      container.resultLatencyEl.textContent = "";
      container.resultBodyEl.textContent = "";
      container.resultAvailableModelsEl.textContent = "";
      return;
    }
    container.resultStatusEl.textContent = r.ok
      ? `✓ OK (${r.status ?? "—"})`
      : `✗ ${r.error ?? "失败"}`;
    container.resultStatusEl.classList.toggle("error", !r.ok);
    container.resultStatusEl.classList.toggle("ok", r.ok);
    container.resultLatencyEl.textContent = `${r.latency_ms}ms · mode=${r.mode}`;
    container.resultBodyEl.textContent = r.response ?? r.error ?? "";
    if (r.available_models && r.available_models.length > 0) {
      container.resultAvailableModelsEl.textContent =
        `provider 列出的 model: ${r.available_models.join(", ")}`;
    } else {
      container.resultAvailableModelsEl.textContent = "";
    }
  }

  async function run(mode: TestMode | "image"): Promise<void> {
    if (running) return;
    if (!currentDef) {
      setStatus("没有 model 定义可测", true);
      return;
    }
    // image 模式复用 http 模式 + images 字段
    const actualMode: TestMode = mode === "image" ? "http" : mode;
    const prompt = container.promptInput.value.trim();
    const useImages = mode === "image" ? images : [];

    if (actualMode !== "connectivity" && !prompt) {
      setStatus("prompt 不能为空（连通测试除外）", true);
      return;
    }
    if (mode === "image" && useImages.length === 0) {
      setStatus("图片输入模式需要至少 1 张图片", true);
      return;
    }
    running = true;
    setStatus(`运行 ${mode} 测试中…`);
    setResult(null);
    try {
      const resp = await testModel({
        def: currentDef,
        mode: actualMode,
        prompt,
        images: useImages.length > 0 ? useImages : undefined,
      });
      setResult(resp);
      setStatus(resp.ok ? `✓ ${mode} 测试通过` : `✗ ${mode} 测试失败`);
    } catch (e) {
      setResult({
        ok: false,
        mode: actualMode,
        latency_ms: 0,
        error: (e as Error).message,
      });
      setStatus(`✗ ${mode} 测试失败: ${(e as Error).message}`, true);
    } finally {
      running = false;
    }
  }

  return {
    open,
    close,
    isOpen: () => isOpen,
  };
}
