// 模型管理面板：下拉菜单选中一个 model → 表单展示所有字段（含 api_key）
// → 点保存写回项目目录 `<cwd>/.latte/models.d/<provider>__<id>.toml`。
//
// 历史：早期版本是 `<table>` 列出所有 model，但 model 字段很多（11 个 +
// 4 个可空数字），单行 table 信息密度太低、key 显示省略号、编辑要走
// "edit" 弹窗 → UX 不友好。改成 dropdown + form 一对一编辑。
//
// 数据契约：见 `api.ts` 的 `ModelsListResponse` / `ModelWithSource`。
// `file_path` 字段是后端独立扫盘拿到的真实绝对路径，UI 直接展示给用户。
import { listModels, updateModel, deleteModel, getModelToml, putModelToml } from "./api";
import type { ModelDef, ModelWithSource } from "./api";
interface UIBinding {
  panelEl: HTMLElement;
  openBtn: HTMLButtonElement;
  closeBtn: HTMLButtonElement;
  refreshBtn: HTMLButtonElement;
  newBtn: HTMLButtonElement;
  bodyEl: HTMLElement;
  statusEl: HTMLElement;
  pathsEl: HTMLElement;
  /** model 选择下拉框（脚本挂载时由调用者提供，避免硬编码 ID）。 */
  selectEl: HTMLSelectElement;
  /**
   * 「测试」按钮的回调：传当前表单的 def（可能是新建未保存的）与
   * 对应 key（已存在的 model 才有，null 表示新建）。main.ts 注入一个
   * 直接打开测试弹层的闭包。
   */
  onTestClick: (opts: { def: ModelDef; key: string | null }) => void;
}
export interface ModelsPanelController {
  isOpen(): boolean;
  open(): void;
  /** 打开面板并选中指定 model。接受 `provider/name` 全键或裸 name
   * （角色 model_chain 里的写法，按 name 匹配）。 */
  selectModel(key: string): void;
}

/** 字段渲染顺序：表单 label 列表，按"基本 → 网络 → 计费 → 行为"分组。 */
type FieldKind = "text" | "longtext" | "number" | "checkbox" | "select";

interface FieldSpec {
  key: keyof ModelDef;
  label: string;
  kind: FieldKind;
  placeholder?: string;
  options?: readonly string[];
}

const FIELDS: ReadonlyArray<FieldSpec> = [
  // 基本
  { key: "name", label: "id", kind: "text", placeholder: "deepseek-v4-pro" },
  { key: "provider", label: "provider", kind: "text", placeholder: "deepseek" },
  { key: "api", label: "api 协议", kind: "select", options: ["openai", "anthropic", "google"] },
  // 网络
  { key: "base_url", label: "base_url", kind: "longtext", placeholder: "https://api.deepseek.com" },
  { key: "api_key", label: "api_key（支持 ${ENV} 引用环境变量）", kind: "longtext", placeholder: "${DEEPSEEK_API_KEY}" },
  // 容量
  { key: "context_window", label: "context_window（tokens）", kind: "number" },
  { key: "max_tokens", label: "max_tokens（tokens）", kind: "number" },
  { key: "timeout_secs", label: "timeout_secs（per-turn）", kind: "number" },
  // 能力开关
  { key: "supports_thinking", label: "supports_thinking", kind: "checkbox" },
  { key: "supports_vision", label: "supports_vision", kind: "checkbox" },
  { key: "supports_image_generation", label: "supports_image_generation", kind: "checkbox" },
  // 计费
  { key: "cost_per_million_input", label: "cost / 1M input (USD)", kind: "number" },
  { key: "cost_per_million_output", label: "cost / 1M output (USD)", kind: "number" },
  { key: "tier", label: "tier", kind: "select", options: ["premium", "standard", "budget"] },
];

/** `<select>` 的 `valueAsNumber` / `checked` / `value` 三种访问方式
 * 收敛到一个函数，避免散落在渲染 + 收集两处分别断言。
 */
type FormInput = HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement;

function readInput(inp: FormInput, kind: FieldKind): string | number | boolean {
  if (kind === "checkbox") {
    return (inp as HTMLInputElement).checked;
  }
  const raw = inp.value.trim();
  if (kind === "number") {
    if (raw === "") return "";
    const n = Number(raw);
    return Number.isFinite(n) ? n : "";
  }
  return raw;
}

export function mountModelsPanel(opts: { container: UIBinding }): ModelsPanelController {
  const { container } = opts;
  let items: ModelWithSource[] = [];
  let currentKey: string | null = null;
  let isLoading = false;
  let isSaving = false;

  // TOML 源文件编辑元素
  const tabBarEl = document.getElementById("models-tab-bar")!;
  const tabBtns = tabBarEl?.querySelectorAll<HTMLButtonElement>(".role-editor-tab") ?? [];
  const tomlPane = document.getElementById("models-toml-pane")!;
  const tomlEditor = document.getElementById("models-toml-editor") as HTMLTextAreaElement | null;
  const tomlSaveBtn = document.getElementById("models-toml-save") as HTMLButtonElement | null;
  const tomlReloadBtn = document.getElementById("models-toml-reload") as HTMLButtonElement | null;
  const tomlStatusEl = document.getElementById("models-toml-status") as HTMLElement | null;

  container.openBtn.addEventListener("click", () => {
    container.panelEl.classList.remove("hidden");
    showTab("form");
    void refresh();
  });
  container.closeBtn.addEventListener("click", () => container.panelEl.classList.add("hidden"));
  container.refreshBtn.addEventListener("click", () => {
    showTab("form");
    void refresh();
  });
  container.newBtn.addEventListener("click", () => {
    showTab("form");
    selectNewBlank();
  });

  container.selectEl.addEventListener("change", () => {
    currentKey = container.selectEl.value || null;
    const activeTab = tabBarEl?.querySelector(".role-editor-tab.active")?.getAttribute("data-tab");
    renderForm();
    if (activeTab === "toml" && currentKey) {
      void loadToml(currentKey);
    }
  });

  function setStatus(msg: string, error = false): void {
    container.statusEl.textContent = msg;
    container.statusEl.classList.toggle("error", error);
  }

  function setTomlStatus(msg: string, error = false): void {
    if (tomlStatusEl) {
      tomlStatusEl.textContent = msg;
      tomlStatusEl.classList.toggle("error", error);
    }
  }

  function showTab(tab: "form" | "toml"): void {
    tabBtns.forEach((b) => b.classList.toggle("active", b.dataset.tab === tab));
    container.bodyEl.classList.toggle("hidden", tab !== "form");
    if (tomlPane) tomlPane.classList.toggle("hidden", tab !== "toml");
  }

  // ── tab 切换 ──
  Array.from(tabBtns).forEach((btn) => {
    btn.addEventListener("click", () => {
      const target = btn.dataset.tab;
      if (!target) return;
      showTab(target as "form" | "toml");
      if (target === "toml" && currentKey) {
        void loadToml(currentKey);
      }
    });
  });

  // ── TOML 编辑事件 ──
  tomlSaveBtn?.addEventListener("click", () => void saveToml());
  tomlReloadBtn?.addEventListener("click", () => {
    if (currentKey) void loadToml(currentKey);
  });

  /** 加载当前 model 的 TOML 源文件到编辑器。 */
  async function loadToml(key: string): Promise<void> {
    if (!tomlEditor || !tomlStatusEl) return;
    setTomlStatus("加载中…");
    tomlEditor.disabled = true;
    try {
      const raw = await getModelToml(key);
      tomlEditor.value = raw;
      tomlEditor.disabled = false;
      setTomlStatus(`已加载 ${key}`);
    } catch (e) {
      setTomlStatus(`加载失败: ${(e as Error).message}`, true);
    }
  }

  /** 保存 TOML 源文件。 */
  async function saveToml(): Promise<void> {
    if (isSaving || !currentKey || !tomlEditor) return;
    isSaving = true;
    setTomlStatus("保存中…");
    try {
      await putModelToml(currentKey, tomlEditor.value);
      setTomlStatus("✅ 已保存");
      await refresh();
    } catch (e) {
      setTomlStatus(`保存失败: ${(e as Error).message}`, true);
    } finally {
      isSaving = false;
    }
  }

  async function refresh(): Promise<void> {
    if (isLoading) return;
    isLoading = true;
    setStatus("加载中…");
    try {
      const resp = await listModels();
      items = resp.models;
      container.pathsEl.textContent =
        `项目: ${resp.project_models_dir} · 全局: ${resp.global_models_dir}`;
      populateSelect();
      const keep = currentKey !== null && items.some(m => m.key === currentKey);
      if (keep && currentKey !== null) {
        container.selectEl.value = currentKey;
      } else if (items.length > 0) {
        currentKey = items[0].key;
        container.selectEl.value = currentKey!;
      } else {
        currentKey = null;
        container.selectEl.value = "";
      }
      setStatus(`已加载 ${items.length} 个 model`);
      renderForm();
    } catch (e) {
      setStatus(`加载失败: ${(e as Error).message}`, true);
    } finally {
      isLoading = false;
    }
  }

  function selectNewBlank(): void {
    currentKey = null;
    container.selectEl.value = "";
    renderForm();
  }

  function populateSelect(): void {
    container.selectEl.replaceChildren();
    const placeholder = document.createElement("option");
    placeholder.value = "";
    placeholder.textContent = items.length === 0
      ? "（暂无 model，点击 + 新建）"
      : "（选择一个 model，或点 + 新建）";
    container.selectEl.appendChild(placeholder);
    for (const m of items) {
      const opt = document.createElement("option");
      opt.value = m.key;
      opt.textContent = `${m.key}${m.name !== m.key ? `  —  ${m.name}` : ""}`;
      container.selectEl.appendChild(opt);
    }
  }

  function renderForm(): { inputs: Map<keyof ModelDef, FormInput> } {
    container.bodyEl.replaceChildren();
    const current = currentKey
      ? items.find(m => m.key === currentKey) ?? null
      : null;

    if (current) {
      const meta = document.createElement("div");
      meta.className = "models-meta";
      const sourceBadge = document.createElement("span");
      sourceBadge.className = `models-source-${current.source}`;
      sourceBadge.textContent = current.source;
      meta.appendChild(document.createTextNode("文件位置: "));
      meta.appendChild(sourceBadge);
      meta.appendChild(document.createTextNode(" "));
      const pathCode = document.createElement("code");
      pathCode.className = "models-path-code";
      pathCode.textContent = current.file_path || "（仅内存，未落盘）";
      pathCode.title = current.file_path;
      meta.appendChild(pathCode);
      container.bodyEl.appendChild(meta);
    }

    const form = document.createElement("form");
    form.className = "models-form";

    const inputs = new Map<keyof ModelDef, FormInput>();
    for (const f of FIELDS) {
      const wrap = document.createElement("label");
      wrap.className = `models-form-row models-form-${f.kind}`;
      const lab = document.createElement("span");
      lab.className = "models-form-label";
      lab.textContent = f.label;
      wrap.appendChild(lab);

      let input: FormInput;
      if (f.kind === "select") {
        const sel = document.createElement("select");
        for (const opt of f.options ?? []) {
          const o = document.createElement("option");
          o.value = opt;
          o.textContent = opt;
          sel.appendChild(o);
        }
        input = sel;
      } else if (f.kind === "checkbox") {
        const cb = document.createElement("input");
        cb.type = "checkbox";
        input = cb;
      } else if (f.kind === "number") {
        const num = document.createElement("input");
        num.type = "number";
        num.step = "any";
        if (f.placeholder) num.placeholder = f.placeholder;
        input = num;
      } else if (f.kind === "longtext") {
        const ta = document.createElement("textarea");
        ta.rows = 2;
        if (f.placeholder) ta.placeholder = f.placeholder;
        input = ta;
      } else {
        const t = document.createElement("input");
        t.type = "text";
        if (f.placeholder) t.placeholder = f.placeholder;
        input = t;
      }

      const v = current ? current[f.key] : undefined;
      input.value = "";
      if (f.kind === "checkbox") {
        (input as HTMLInputElement).checked = Boolean(v);
      } else if (f.kind === "number") {
        if (typeof v === "number" && Number.isFinite(v)) {
          (input as HTMLInputElement).value = String(v);
        }
      } else if (typeof v === "string") {
        input.value = v;
      }

      wrap.appendChild(input);
      form.appendChild(wrap);
      inputs.set(f.key, input);
    }

    const actions = document.createElement("div");
    actions.className = "models-form-actions";
    const saveProject = document.createElement("button");
    saveProject.type = "button";
    saveProject.className = "primary";
    saveProject.dataset.target = "project";
    saveProject.textContent = current ? "保存到项目" : "创建到项目";
    if (isSaving) saveProject.disabled = true;
    actions.appendChild(saveProject);
    if (current) {
      const saveGlobal = document.createElement("button");
      saveGlobal.type = "button";
      saveGlobal.dataset.target = "global";
      saveGlobal.textContent = "保存到全局";
      if (isSaving) saveGlobal.disabled = true;
      actions.appendChild(saveGlobal);
      const testBtn = document.createElement("button");
      testBtn.type = "button";
      testBtn.className = "models-test-btn";
      testBtn.textContent = "测试";
      actions.appendChild(testBtn);
      testBtn.addEventListener("click", () => {
        const def = collectDef(inputs);
        if (!def) {
          setStatus("测试失败：必填字段缺失（id / provider / api）", true);
          return;
        }
        container.onTestClick({ def, key: currentKey });
      });
      const deleteBtn = document.createElement("button");
      deleteBtn.type = "button";
      deleteBtn.className = "danger";
      deleteBtn.textContent = "删除";
      actions.appendChild(deleteBtn);
      deleteBtn.addEventListener("click", () => {
        void onDelete(currentKey!);
      });
    }
    if (current && current.source === "global") {
      const note = document.createElement("span");
      note.className = "models-form-note";
      note.textContent = "保存到项目：相当于把全局 model 复制到项目层";
      actions.appendChild(note);
    }
    form.appendChild(actions);

    const submitSave = (target: "project" | "global"): void => {
      void onSave(inputs, target);
    };
    saveProject.addEventListener("click", () => submitSave("project"));
    if (current) {
      const saveGlobalBtn = actions.querySelector<HTMLButtonElement>(
        'button[data-target="global"]',
      );
      saveGlobalBtn?.addEventListener("click", () => submitSave("global"));
    }

    container.bodyEl.appendChild(form);
    return { inputs };
  }

  async function onDelete(key: string): Promise<void> {
    if (isSaving) return;
    const def = items.find(m => m.key === key);
    const label = def ? `${def.provider}/${def.name}` : key;
    if (!confirm(`确定删除 model "${label}"？此操作不可撤销。`)) return;
    isSaving = true;
    setStatus("删除中…");
    try {
      await deleteModel(key);
      setStatus(`已删除 ${label}`);
      items = items.filter(m => m.key !== key);
      if (currentKey === key) currentKey = null;
      populateSelect();
      renderForm();
    } catch (err) {
      setStatus(`删除失败: ${(err as Error).message}`, true);
    } finally {
      isSaving = false;
    }
  }
  async function onSave(
    inputs: Map<keyof ModelDef, FormInput>,
    target: "project" | "global",
  ): Promise<void> {
    if (isSaving) return;
    isSaving = true;
    setStatus(target === "global" ? "保存到全局…" : "保存到项目…");
    try {
      const def = collectDef(inputs);
      if (!def) {
        setStatus("保存失败：必填字段缺失（id / provider / api）", true);
        return;
      }
      const isNew = currentKey === null;
      const targetKey = isNew ? `${def.provider}/${def.name}` : currentKey!;
      const updated = await updateModel(targetKey, def, target);
      await refresh();
      setStatus(`✅ 已保存 ${updated.key}（${updated.source}）`);
      currentKey = updated.key;
      container.selectEl.value = updated.key;
      renderForm();
    } catch (err) {
      setStatus(`保存失败: ${(err as Error).message}`, true);
    } finally {
      isSaving = false;
      const save = container.bodyEl.querySelector("button.primary") as HTMLButtonElement | null;
      if (save) save.disabled = false;
    }
  }

  function trySelect(key: string): void {
    let found = items.find(m => m.key === key);
    if (!found) {
      const byName = items.filter(m => m.name === key);
      found = byName.find(m => m.source !== "catalog") ?? byName[0];
    }
    if (found) {
      currentKey = found.key;
      container.selectEl.value = found.key;
      renderForm();
    } else {
      setStatus(`未找到 model「${key}」`, true);
    }
  }

  return {
    isOpen: () => !container.panelEl.classList.contains("hidden"),
    open: () => {
      container.panelEl.classList.remove("hidden");
      showTab("form");
      void refresh();
    },
    selectModel: (key: string) => {
      container.panelEl.classList.remove("hidden");
      showTab("form");
      if (items.length === 0) {
        void (async () => {
          await refresh();
          trySelect(key);
        })();
      } else {
        trySelect(key);
      }
    },
  };
}
/** 从表单 inputs 收集成 ModelDef。空数字字段留 null（保留 Rust 侧
 * 的 Option<...> 语义）。必填字段缺失 → 返回 null。
 */
function collectDef(inputs: Map<keyof ModelDef, FormInput>): ModelDef | null {
  const text = (k: keyof ModelDef): string => {
    const inp = inputs.get(k);
    return inp ? inp.value.trim() : "";
  };
  const num = (k: keyof ModelDef): number | null => {
    const v = readInput(inputs.get(k)!, "number");
    if (v === "") return null;
    return v as number;
  };
  const bool = (k: keyof ModelDef): boolean => {
    const inp = inputs.get(k);
    return inp ? (inp as HTMLInputElement).checked : false;
  };

  const name = text("name");
  const provider = text("provider");
  const api = text("api") || "openai";
  if (!name || !provider || !api) {
    return null;
  }
  return {
    name: name,
    provider,
    api,
    base_url: text("base_url") || "",
    api_key: text("api_key") || "",
    context_window: num("context_window") ?? 0,
    max_tokens: num("max_tokens") ?? 0,
    supports_thinking: bool("supports_thinking"),
    supports_vision: bool("supports_vision"),
    supports_image_generation: bool("supports_image_generation"),
    cost_per_million_input: num("cost_per_million_input"),
    cost_per_million_output: num("cost_per_million_output"),
    tier: text("tier") || null,
    timeout_secs: num("timeout_secs"),
  };
}