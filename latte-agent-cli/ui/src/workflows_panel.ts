// Workflow 管理面板：下拉菜单选中一个 workflow → 表单编辑（含步骤卡片列表）
// → 保存写回项目层 `.latte/workflows/`（全局 workflow 保存时自动复制为项目副本）。
//
// 结构对齐 models_panel.ts：dropdown + form 一对一编辑；「测试运行」打开
// modal（对齐 test_panel.ts），POST /api/workflows/run 后用 EventSource
// 订阅 /api/workflows/run/events 实时渲染 transcript。
//
// 数据契约：见 `api.ts` 的 `WorkflowSummary` / `WorkflowDetail` /
// `WorkflowForm` / `ValidateResponse`。
import {
  listWorkflows,
  getWorkflow,
  createWorkflow,
  updateWorkflow,
  deleteWorkflow,
  validateWorkflow,
  runWorkflow,
  stopWorkflowRun,
  getRoles,
  importTasks,
  getWorkflowToml,
  putWorkflowToml,
} from "./api";
import type {
  WorkflowSummary,
  WorkflowDetail,
  WorkflowForm,
  StepForm,
  RoleInfo,
  ValidateResponse,
  ImportTask,
} from "./api";
import { HttpError } from "./transport";

interface UIBinding {
  panelEl: HTMLElement;
  openBtn: HTMLButtonElement;
  closeBtn: HTMLButtonElement;
  refreshBtn: HTMLButtonElement;
  newBtn: HTMLButtonElement;
  bodyEl: HTMLElement;
  statusEl: HTMLElement;
  pathsEl: HTMLElement;
  selectEl: HTMLSelectElement;
  // ─── 试运行弹层 ───
  runOverlayEl: HTMLElement;
  runCloseBtn: HTMLButtonElement;
  runTopicInput: HTMLTextAreaElement;
  runVarsInput: HTMLTextAreaElement;
  runStartBtn: HTMLButtonElement;
  runStopBtn: HTMLButtonElement;
  runStatusEl: HTMLElement;
  runTranscriptEl: HTMLElement;
  // TOML 源文件编辑
  tabBarEl: HTMLElement;
  tabBtns: NodeListOf<HTMLButtonElement>;
  tomlPane: HTMLElement;
  tomlEditor: HTMLTextAreaElement;
  tomlSaveBtn: HTMLButtonElement;
  tomlReloadBtn: HTMLButtonElement;
  tomlStatusEl: HTMLElement;
}

export interface WorkflowsPanelController {
  isOpen(): boolean;
  open(): void;
}

/** SSE 事件（与后端 /api/workflows/run/events 推送的 JSON 一一对应）。 */
type WorkflowRunEvent =
  | { type: "WorkflowStarted"; name: string; topic: string; wf_id: string }
  | { type: "WorkflowStep"; wf_id: string; step_id: string; description: string; index: number; total: number }
  | { type: "WorkflowTurn"; wf_id: string; step_id: string; role_id: string; content: string; round: number }
  | { type: "WorkflowFinished"; name: string; wf_id: string; status: string; summary: string };

/** realm-agnostic 的 HTTP status 提取（对齐 api.ts 的 isHttpError 思路：
 *  编辑器宿主的 transport 抛出的错误跨 JS realm，instanceof 不可靠）。 */
function httpStatusOf(e: unknown): number | null {
  if (e instanceof HttpError) return e.status;
  if (
    typeof e === "object" &&
    e !== null &&
    (e as { name?: unknown }).name === "HttpError" &&
    typeof (e as { status?: unknown }).status === "number"
  ) {
    return (e as { status: number }).status;
  }
  return null;
}

/** 步骤卡片 → speakers 勾选顺序。checkbox 按 roles 列表顺序渲染，但
 *  speakers 数组要保持用户的勾选顺序（= 发言顺序），所以单独维护。 */
const speakerOrder = new WeakMap<HTMLElement, string[]>();

/** 从试运行 transcript 文本里提取可导入的任务列表：扫描 ```json 围栏块，
 *  第一个能 JSON.parse 成 `{tasks: [...]}` 且每项都是带 string title 的
 *  对象的块获胜；找不到返回 null。 */
export function extractImportableTasks(text: string): ImportTask[] | null {
  // 先剥 <think> 块：带思考的模型会在里面复述指令文本（含 ```json
  // 字样），产生假 fence（自优化实测中真实遇到）。
  const cleaned = text.replace(/<think>[\s\S]*?(<\/think>|$)/g, "");
  const fenceRe = /```json[^\S\n]*\n([\s\S]*?)```/g;
  for (const m of cleaned.matchAll(fenceRe)) {
    let parsed: unknown;
    try {
      parsed = JSON.parse(m[1]);
    } catch {
      continue;
    }
    if (typeof parsed !== "object" || parsed === null) continue;
    const tasks = (parsed as { tasks?: unknown }).tasks;
    if (!Array.isArray(tasks) || tasks.length === 0) continue;
    const allValid = tasks.every(
      (t) =>
        typeof t === "object" &&
        t !== null &&
        typeof (t as { title?: unknown }).title === "string" &&
        (t as { title: string }).title.trim() !== "",
    );
    if (allValid) return tasks as ImportTask[];
  }
  return null;
}

export function mountWorkflowsPanel(opts: { container: UIBinding }): WorkflowsPanelController {
  const { container } = opts;
  let items: WorkflowSummary[] = [];
  let currentName: string | null = null;
  let currentDetail: WorkflowDetail | null = null;
  let roles: RoleInfo[] = [];
  let isLoading = false;
  let isSaving = false;
  // 试运行状态
  let runSource: EventSource | null = null;
  let runActive = false;
  // 本轮 run 的纯文本 transcript（WorkflowTurn.content 累加），
  // 供 run 结束后扫描 ```json 任务列表做「导入任务看板」。
  let runTranscriptText = "";

  // 「📥 导入任务看板」按钮：动态创建，放在运行控制行（status 左侧），
  // 只在 run 成功结束且 transcript 里能解析出任务列表时出现。
  const importBtn = document.createElement("button");
  importBtn.type = "button";
  importBtn.textContent = "📥 导入任务看板";
  importBtn.classList.add("hidden");
  container.runStatusEl.parentElement?.insertBefore(importBtn, container.runStatusEl);
  let importableTasks: ImportTask[] | null = null;

  function hideImportBtn(): void {
    importableTasks = null;
    importBtn.classList.add("hidden");
  }

  importBtn.addEventListener("click", () => void onImportTasks());

  async function onImportTasks(): Promise<void> {
    const tasks = importableTasks;
    if (!tasks || runActive) return;
    if (!confirm(`导入 ${tasks.length} 个任务到看板（backlog）？`)) return;
    importBtn.disabled = true;
    try {
      const resp = await importTasks(tasks);
      setRunStatus(`✅ 已创建 ${resp.created.length} 个任务，请到任务看板查看`);
      hideImportBtn();
    } catch (e) {
      // 后端 400 返回纯文本错误信息，直接透出。
      setRunStatus(`导入失败: ${(e as Error).message}`, true);
    } finally {
      importBtn.disabled = false;
    }
  }

  container.openBtn.addEventListener("click", () => {
    container.panelEl.classList.remove("hidden");
    void refresh();
  });
  container.closeBtn.addEventListener("click", () => closePanel());
  container.refreshBtn.addEventListener("click", () => void refresh());
  container.newBtn.addEventListener("click", () => selectNewBlank());

  container.selectEl.addEventListener("change", () => {
    const name = container.selectEl.value;
    if (name) {
      void selectWorkflow(name);
    } else {
      selectNewBlank();
    }
  });

  // 试运行弹层：× / overlay 点击 / Esc 关闭（对齐 test_panel.ts）
  container.runCloseBtn.addEventListener("click", () => closeRunModal());
  container.runOverlayEl.addEventListener("click", (e) => {
    if (e.target === container.runOverlayEl) closeRunModal();
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !container.runOverlayEl.classList.contains("hidden")) {
      closeRunModal();
    }
  });
  container.runStartBtn.addEventListener("click", () => void startRun());
  container.runStopBtn.addEventListener("click", () => void stopRun());

  // ── TOML tab 切换 ──
  const tabBtns = Array.from(container.tabBtns);
  tabBtns.forEach((btn) => {
    btn.addEventListener("click", () => {
      const target = btn.dataset.tab;
      if (!target) return;
      showTab(target as "form" | "toml");
      if (target === "toml" && currentName) {
        void loadToml(currentName);
      }
    });
  });
  container.tomlSaveBtn.addEventListener("click", () => void saveToml());
  container.tomlReloadBtn.addEventListener("click", () => {
    if (currentName) void loadToml(currentName);
  });

  function showTab(tab: "form" | "toml"): void {
    tabBtns.forEach((b) => b.classList.toggle("active", b.dataset.tab === tab));
    container.bodyEl.classList.toggle("hidden", tab !== "form");
    container.tomlPane.classList.toggle("hidden", tab !== "toml");
  }

  async function refresh(): Promise<void> {
    if (isLoading) return;
    isLoading = true;
    setStatus("加载中…");
    try {
      if (roles.length === 0) {
        roles = await getRoles().catch(() => []);
      }
      items = await listWorkflows();
      populateSelect();
      const keep = currentName !== null && items.some(w => w.name === currentName);
      if (keep && currentName !== null) {
        container.selectEl.value = currentName;
        await loadDetail(currentName);
      } else if (items.length > 0) {
        currentName = items[0].name;
        container.selectEl.value = currentName;
        await loadDetail(currentName);
      } else {
        currentName = null;
        currentDetail = null;
        container.selectEl.value = "";
      }
      setStatus(`已加载 ${items.length} 个 workflow`);
      renderForm();
      if (container.tomlPane.classList.contains("hidden") === false && currentName) {
        void loadToml(currentName);
      }
    } catch (e) {
      setStatus(`加载失败: ${(e as Error).message}`, true);
    } finally {
      isLoading = false;
    }
  }

  async function selectWorkflow(name: string): Promise<void> {
    currentName = name;
    setStatus("加载中…");
    try {
      await loadDetail(name);
      setStatus("");
      renderForm();
      // 如果在 TOML tab，同步加载 TOML 内容
      if (container.tomlPane.classList.contains("hidden") === false) {
        void loadToml(name);
      }
    } catch (e) {
      currentDetail = null;
      setStatus(`加载失败: ${(e as Error).message}`, true);
      renderForm();
    }
  }

  async function loadDetail(name: string): Promise<void> {
    currentDetail = await getWorkflow(name);
  }

  async function loadToml(name: string): Promise<void> {
    container.tomlEditor.disabled = true;
    container.tomlStatusEl.textContent = "加载中…";
    try {
      const raw = await getWorkflowToml(name);
      container.tomlEditor.value = raw;
      container.tomlStatusEl.textContent = "（只读加载，修改后点「保存 TOML」写盘）";
    } catch (err) {
      container.tomlStatusEl.textContent = `加载失败: ${(err as Error).message}`;
      container.tomlStatusEl.classList.add("error");
    } finally {
      container.tomlEditor.disabled = false;
    }
  }

  async function saveToml(): Promise<void> {
    if (isSaving || !currentName) return;
    isSaving = true;
    container.tomlStatusEl.textContent = "保存中…";
    container.tomlSaveBtn.disabled = true;
    try {
      await putWorkflowToml(currentName, container.tomlEditor.value);
      container.tomlStatusEl.textContent = "✅ TOML 已保存；新 session 生效";
      container.tomlStatusEl.classList.remove("error");
      await refresh();
    } catch (err) {
      container.tomlStatusEl.textContent = `保存失败: ${(err as Error).message}`;
      container.tomlStatusEl.classList.add("error");
    } finally {
      isSaving = false;
      container.tomlSaveBtn.disabled = false;
    }
  }

  /** 新建：清空 currentName + detail，呈现空白模板。 */
  function selectNewBlank(): void {
    currentName = null;
    currentDetail = null;
    container.selectEl.value = "";
    renderForm();
  }

  function populateSelect(): void {
    container.selectEl.replaceChildren();
    const placeholder = document.createElement("option");
    placeholder.value = "";
    placeholder.textContent = items.length === 0
      ? "（暂无 workflow，点击 ＋ 新建）"
      : "（选择一个 workflow，或点 ＋ 新建）";
    container.selectEl.appendChild(placeholder);
    for (const w of items) {
      const opt = document.createElement("option");
      opt.value = w.name;
      const badge = w.source === "project" ? "项目" : "全局";
      opt.textContent = `${w.name}（${badge} · ${w.steps_count} 步）`;
      container.selectEl.appendChild(opt);
    }
  }

  // ─── 表单渲染（detail → DOM） ─────────────────────────────────────

  function renderForm(): void {
    container.bodyEl.replaceChildren();
    const current = currentDetail;

    // paths 行：当前 workflow 的文件位置
    if (current) {
      container.pathsEl.textContent =
        `文件位置: ${current.file_path}（${current.source === "project" ? "项目" : "全局"}）`;
    } else {
      container.pathsEl.textContent = "新 workflow 将保存到项目目录";
    }

    if (current) {
      const meta = document.createElement("div");
      meta.className = "models-meta";
      const sourceBadge = document.createElement("span");
      sourceBadge.className = `models-source-${current.source}`;
      sourceBadge.textContent = current.source === "project" ? "项目" : "全局";
      meta.appendChild(document.createTextNode("文件位置: "));
      meta.appendChild(sourceBadge);
      meta.appendChild(document.createTextNode(" "));
      const pathCode = document.createElement("code");
      pathCode.className = "models-path-code";
      pathCode.textContent = current.file_path;
      pathCode.title = current.file_path;
      meta.appendChild(pathCode);
      container.bodyEl.appendChild(meta);
    }

    const form = document.createElement("form");
    form.className = "models-form";
    form.addEventListener("submit", (e) => e.preventDefault());

    form.appendChild(makeRow("name", "text", "wf-name", current?.name ?? "", "discussion"));
    form.appendChild(makeRow("description", "text", "wf-description", current?.description ?? "", "一句话描述这个 workflow 做什么"));
    form.appendChild(makeRow(
      "max_rounds（留空 = 不限）",
      "number",
      "wf-max_rounds",
      current?.max_rounds != null ? String(current.max_rounds) : "",
    ));

    // 步骤编辑器
    const stepsWrap = document.createElement("div");
    stepsWrap.className = "workflow-steps";
    stepsWrap.dataset.field = "steps";
    form.appendChild(stepsWrap);
    renderSteps(stepsWrap, current?.steps ?? []);

    const addStepBtn = document.createElement("button");
    addStepBtn.type = "button";
    addStepBtn.className = "workflow-add-step";
    addStepBtn.textContent = "＋ 添加步骤";
    addStepBtn.addEventListener("click", () => {
      const steps = collectSteps(stepsWrap);
      steps.push(blankStep());
      renderSteps(stepsWrap, steps);
    });
    form.appendChild(addStepBtn);

    // 操作行：保存 / 新建 / 删除 / 校验 / 测试运行
    const actions = document.createElement("div");
    actions.className = "models-form-actions";

    const saveBtn = document.createElement("button");
    saveBtn.type = "button";
    saveBtn.className = "primary";
    saveBtn.textContent = current === null
      ? "创建"
      : current.source === "global"
        ? "保存为项目副本"
        : "保存";
    saveBtn.disabled = isSaving;
    saveBtn.addEventListener("click", () => void onSave());
    actions.appendChild(saveBtn);

    const newBtn = document.createElement("button");
    newBtn.type = "button";
    newBtn.textContent = "新建";
    newBtn.addEventListener("click", () => selectNewBlank());
    actions.appendChild(newBtn);

    if (current) {
      const deleteBtn = document.createElement("button");
      deleteBtn.type = "button";
      deleteBtn.className = "danger";
      deleteBtn.textContent = "删除";
      // 全局 workflow 只读，删除入口禁用
      deleteBtn.disabled = current.source === "global";
      if (current.source === "global") deleteBtn.title = "全局 workflow 不可删除";
      deleteBtn.addEventListener("click", () => void onDelete());
      actions.appendChild(deleteBtn);
    }

    const validateBtn = document.createElement("button");
    validateBtn.type = "button";
    validateBtn.textContent = "校验";
    validateBtn.addEventListener("click", () => void onValidate(validateResults));
    actions.appendChild(validateBtn);

    const testBtn = document.createElement("button");
    testBtn.type = "button";
    testBtn.className = "models-test-btn";
    testBtn.textContent = "测试运行";
    testBtn.addEventListener("click", () => openRunModal());
    actions.appendChild(testBtn);

    if (current && current.source === "global") {
      const note = document.createElement("span");
      note.className = "models-form-note";
      note.textContent = "全局 workflow 只读；保存会复制一份到项目层";
      actions.appendChild(note);
    }
    form.appendChild(actions);

    // 校验结果区（errors 红 / warnings 黄）
    const validateResults = document.createElement("div");
    validateResults.className = "workflow-validate-results";
    form.appendChild(validateResults);

    container.bodyEl.appendChild(form);
  }

  /** 顶层字段行：data-field 用 `wf-` 前缀，避免与步骤卡片里的
   *  description / id 等字段撞名（querySelector 按 DOM 序取首个）。 */
  function makeRow(label: string, kind: "text" | "number", field: string, value: string, placeholder?: string): HTMLElement {
    const wrap = document.createElement("label");
    wrap.className = `models-form-row models-form-${kind}`;
    const lab = document.createElement("span");
    lab.className = "models-form-label";
    lab.textContent = label;
    wrap.appendChild(lab);
    const input = document.createElement("input");
    input.type = kind;
    if (kind === "number") input.min = "1";
    if (placeholder) input.placeholder = placeholder;
    input.value = value;
    input.dataset.field = field;
    wrap.appendChild(input);
    return wrap;
  }

  function blankStep(): StepForm {
    return { id: "", description: "", speakers: [], prompt: "", output_key: null };
  }

  /** 把 steps 数组渲染成步骤卡片列表（DOM 重建，调用前先从旧 DOM collect）。 */
  function renderSteps(wrap: HTMLElement, steps: StepForm[]): void {
    wrap.replaceChildren();
    steps.forEach((step, i) => {
      wrap.appendChild(makeStepCard(step, i, steps.length, wrap));
    });
  }

  function makeStepCard(step: StepForm, index: number, total: number, wrap: HTMLElement): HTMLElement {
    const card = document.createElement("div");
    card.className = "workflow-step-card";

    const head = document.createElement("div");
    head.className = "workflow-step-head";
    const title = document.createElement("span");
    title.className = "workflow-step-title";
    title.textContent = `步骤 ${index + 1}`;
    head.appendChild(title);
    const btns = document.createElement("span");
    btns.className = "workflow-step-btns";
    const upBtn = document.createElement("button");
    upBtn.type = "button";
    upBtn.textContent = "上移";
    upBtn.disabled = index === 0;
    upBtn.addEventListener("click", () => moveStep(wrap, index, -1));
    const downBtn = document.createElement("button");
    downBtn.type = "button";
    downBtn.textContent = "下移";
    downBtn.disabled = index === total - 1;
    downBtn.addEventListener("click", () => moveStep(wrap, index, 1));
    const delBtn = document.createElement("button");
    delBtn.type = "button";
    delBtn.className = "danger";
    delBtn.textContent = "删除";
    delBtn.addEventListener("click", () => {
      const steps = collectSteps(wrap);
      steps.splice(index, 1);
      renderSteps(wrap, steps);
    });
    btns.appendChild(upBtn);
    btns.appendChild(downBtn);
    btns.appendChild(delBtn);
    head.appendChild(btns);
    card.appendChild(head);

    card.appendChild(makeStepField("id", "text", step.id, "planner"));
    card.appendChild(makeStepField("description", "text", step.description, "这一步做什么"));

    // speakers：checkbox 组（icon + name），勾选顺序 = 发言顺序
    const spRow = document.createElement("div");
    spRow.className = "models-form-row models-form-text";
    const spLabel = document.createElement("span");
    spLabel.className = "models-form-label";
    spLabel.textContent = "speakers（按勾选顺序发言）";
    spRow.appendChild(spLabel);
    const spBox = document.createElement("div");
    spBox.className = "workflow-speakers";
    const order: string[] = [...step.speakers];
    speakerOrder.set(spBox, order);
    if (roles.length === 0) {
      const hint = document.createElement("span");
      hint.className = "workflow-speakers-empty";
      hint.textContent = "（未能加载角色列表）";
      spBox.appendChild(hint);
    }
    for (const r of roles) {
      const lab = document.createElement("label");
      const cb = document.createElement("input");
      cb.type = "checkbox";
      cb.value = r.id;
      cb.checked = step.speakers.includes(r.id);
      cb.addEventListener("change", () => {
        if (cb.checked) {
          if (!order.includes(r.id)) order.push(r.id);
        } else {
          const at = order.indexOf(r.id);
          if (at >= 0) order.splice(at, 1);
        }
      });
      lab.appendChild(cb);
      lab.appendChild(document.createTextNode(`${r.icon} ${r.name}`));
      spBox.appendChild(lab);
    }
    spRow.appendChild(spBox);
    card.appendChild(spRow);

    // prompt
    const promptRow = document.createElement("label");
    promptRow.className = "models-form-row models-form-longtext";
    const promptLabel = document.createElement("span");
    promptLabel.className = "models-form-label";
    promptLabel.textContent = "prompt";
    promptRow.appendChild(promptLabel);
    const promptTa = document.createElement("textarea");
    promptTa.rows = 3;
    promptTa.placeholder = "支持 {{topic}} 与 {{output_key}} 占位符";
    promptTa.value = step.prompt;
    promptTa.dataset.field = "prompt";
    promptRow.appendChild(promptTa);
    card.appendChild(promptRow);

    card.appendChild(makeStepField("output_key（可选）", "text", step.output_key ?? "", "本步输出写入的变量名"));
    return card;
  }

  function makeStepField(label: string, kind: "text", value: string, placeholder?: string): HTMLElement {
    const wrap = document.createElement("label");
    wrap.className = "models-form-row models-form-text";
    const lab = document.createElement("span");
    lab.className = "models-form-label";
    lab.textContent = label;
    wrap.appendChild(lab);
    const input = document.createElement("input");
    input.type = kind;
    if (placeholder) input.placeholder = placeholder;
    input.value = value;
    input.dataset.field = label.startsWith("output_key") ? "output_key" : label;
    wrap.appendChild(input);
    return wrap;
  }

  function moveStep(wrap: HTMLElement, index: number, delta: number): void {
    const steps = collectSteps(wrap);
    const target = index + delta;
    if (target < 0 || target >= steps.length) return;
    const [moved] = steps.splice(index, 1);
    steps.splice(target, 0, moved);
    renderSteps(wrap, steps);
  }

  // ─── 表单收集（DOM → WorkflowForm） ───────────────────────────────

  function fieldValue(scope: ParentNode, field: string): string {
    const el = scope.querySelector<HTMLInputElement | HTMLTextAreaElement>(
      `[data-field="${field}"]`,
    );
    return el ? el.value.trim() : "";
  }

  function collectSteps(wrap: ParentNode): StepForm[] {
    const cards = Array.from(wrap.querySelectorAll<HTMLElement>(".workflow-step-card"));
    return cards.map((card) => {
      const spBox = card.querySelector<HTMLElement>(".workflow-speakers");
      const order = spBox ? speakerOrder.get(spBox) ?? [] : [];
      // 只保留仍处于勾选状态的（顺序数组在 uncheck 时已移除，这里再兜底过滤）
      const checked = new Set(
        Array.from(card.querySelectorAll<HTMLInputElement>(".workflow-speakers input:checked"))
          .map(cb => cb.value),
      );
      const speakers = order.filter(id => checked.has(id));
      const outputKey = fieldValue(card, "output_key");
      return {
        id: fieldValue(card, "id"),
        description: fieldValue(card, "description"),
        speakers,
        prompt: fieldValue(card, "prompt"),
        output_key: outputKey || null,
      };
    });
  }

  /** 收集当前表单为 WorkflowForm。name 为空 → null（必填校验）。 */
  function collectForm(): WorkflowForm | null {
    const form = container.bodyEl.querySelector("form");
    if (!form) return null;
    const name = fieldValue(form, "wf-name");
    if (!name) return null;
    const roundsRaw = fieldValue(form, "wf-max_rounds");
    const roundsNum = roundsRaw === "" ? null : Number(roundsRaw);
    const stepsWrap = form.querySelector<HTMLElement>('[data-field="steps"]');
    return {
      name,
      description: fieldValue(form, "wf-description"),
      max_rounds: roundsNum !== null && Number.isFinite(roundsNum) ? roundsNum : null,
      steps: stepsWrap ? collectSteps(stepsWrap) : [],
    };
  }

  // ─── 操作：保存 / 删除 / 校验 ─────────────────────────────────────

  async function onSave(): Promise<void> {
    if (isSaving) return;
    const form = collectForm();
    if (!form) {
      setStatus("保存失败：name 必填", true);
      return;
    }
    isSaving = true;
    setStatus("保存中…");
    try {
      // 新建 / 全局 workflow → 一律 POST create（全局保存 = 复制为项目副本）
      const isCreate = currentName === null || currentDetail?.source === "global";
      const saved = isCreate
        ? await createWorkflow(form)
        : await updateWorkflow(currentName!, form);
      currentName = saved.name;
      await refresh();
      // refresh 内部会覆盖 status，保存成功后重新设回成功消息
      setStatus(`✅ 已保存 ${saved.name}（${saved.source === "project" ? "项目" : "全局"}）`);
    } catch (e) {
      if (httpStatusOf(e) === 409) {
        setStatus(`保存失败：已存在同名 workflow "${form.name}"`, true);
      } else {
        setStatus(`保存失败: ${(e as Error).message}`, true);
      }
    } finally {
      isSaving = false;
    }
  }

  async function onDelete(): Promise<void> {
    if (isSaving || currentName === null) return;
    if (currentDetail?.source === "global") {
      setStatus("全局 workflow 不可删除", true);
      return;
    }
    if (!confirm(`确定删除 workflow "${currentName}"？此操作不可撤销。`)) return;
    isSaving = true;
    setStatus("删除中…");
    try {
      await deleteWorkflow(currentName);
      setStatus(`已删除 ${currentName}`);
      currentName = null;
      currentDetail = null;
      await refresh();
    } catch (e) {
      setStatus(`删除失败: ${(e as Error).message}`, true);
    } finally {
      isSaving = false;
    }
  }

  async function onValidate(resultsEl: HTMLElement): Promise<void> {
    const form = collectForm();
    if (!form) {
      setStatus("校验失败：name 必填", true);
      return;
    }
    setStatus("校验中…");
    try {
      const resp = await validateWorkflow(form);
      renderValidateResults(resultsEl, resp);
      setStatus(resp.ok ? "✅ 校验通过" : "校验未通过", !resp.ok);
    } catch (e) {
      setStatus(`校验失败: ${(e as Error).message}`, true);
    }
  }

  function renderValidateResults(wrap: HTMLElement, resp: ValidateResponse): void {
    wrap.replaceChildren();
    for (const err of resp.errors) {
      const line = document.createElement("div");
      line.className = "workflow-validate-error";
      line.textContent = `✗ ${err}`;
      wrap.appendChild(line);
    }
    for (const warn of resp.warnings) {
      const line = document.createElement("div");
      line.className = "workflow-validate-warning";
      line.textContent = `⚠ ${warn}`;
      wrap.appendChild(line);
    }
    if (resp.ok && resp.warnings.length === 0) {
      const line = document.createElement("div");
      line.className = "workflow-validate-ok";
      line.textContent = "✓ 校验通过";
      wrap.appendChild(line);
    }
  }

  // ─── 试运行（modal + SSE transcript） ─────────────────────────────

  function setRunStatus(msg: string, error = false): void {
    container.runStatusEl.textContent = msg;
    container.runStatusEl.classList.toggle("error", error);
  }

  function openRunModal(): void {
    setRunStatus(runActive ? "运行中…" : "就绪");
    container.runOverlayEl.classList.remove("hidden");
  }

  function closeRunModal(): void {
    closeRunStream();
    container.runOverlayEl.classList.add("hidden");
  }

  /** vars 文本域 → Record：每行一个 key=value，空行 / 无 `=` 的行忽略。 */
  function parseVars(text: string): Record<string, string> {
    const vars: Record<string, string> = {};
    for (const line of text.split("\n")) {
      const t = line.trim();
      if (!t) continue;
      const eq = t.indexOf("=");
      if (eq <= 0) continue;
      vars[t.slice(0, eq).trim()] = t.slice(eq + 1).trim();
    }
    return vars;
  }

  async function startRun(): Promise<void> {
    if (runActive) {
      setRunStatus("已有运行中的 workflow，请先停止", true);
      return;
    }
    // 用当前表单的 workflow（未保存的改动也会被测到）
    const form = collectForm();
    if (!form) {
      setRunStatus("表单不完整：name 必填", true);
      return;
    }
    const topic = container.runTopicInput.value.trim();
    if (!topic) {
      setRunStatus("topic 必填", true);
      return;
    }
    const vars = parseVars(container.runVarsInput.value);
    container.runTranscriptEl.replaceChildren();
    runTranscriptText = "";
    hideImportBtn();
    setRunStatus("启动中…");
    container.runStartBtn.disabled = true;
    try {
      await runWorkflow({
        workflow: form,
        topic,
        vars: Object.keys(vars).length > 0 ? vars : undefined,
      });
    } catch (e) {
      if (httpStatusOf(e) === 409) {
        setRunStatus("已有运行中的 workflow，请先停止", true);
      } else {
        setRunStatus(`启动失败: ${(e as Error).message}`, true);
      }
      container.runStartBtn.disabled = false;
      return;
    }
    runActive = true;
    container.runStopBtn.disabled = false;
    setRunStatus("运行中…");
    openRunStream();
  }

  async function stopRun(): Promise<void> {
    setRunStatus("停止中…");
    try {
      await stopWorkflowRun();
    } catch (e) {
      setRunStatus(`停止失败: ${(e as Error).message}`, true);
    }
    // 最终的 cancelled 状态以后端推送的 WorkflowFinished 为准
  }

  /** 订阅 /api/workflows/run/events（EventSource URL 与 transport.ts
   *  的 subscribeEvents 一样用同源相对路径）。 */
  function openRunStream(): void {
    closeRunStream();
    const es = new EventSource("/api/workflows/run/events");
    const onMsg = (e: Event): void => {
      try {
        handleRunEvent(JSON.parse((e as MessageEvent).data) as WorkflowRunEvent);
      } catch (err) {
        console.error("[workflow-run] failed to parse event", err, e);
      }
    };
    // 后端事件类型是 chat_event（缺省 message 兜底，两种都监听；
    // EventSource 对命名事件只派发对应 listener，不会重复触发）
    es.addEventListener("chat_event", onMsg);
    es.addEventListener("message", onMsg);
    es.addEventListener("error", () => {
      // error 时主动 close()，避免浏览器自动重连挂着不释放
      es.close();
      if (runSource === es) runSource = null;
    });
    runSource = es;
  }

  function closeRunStream(): void {
    if (runSource) {
      runSource.close();
      runSource = null;
    }
  }

  function finishRun(): void {
    runActive = false;
    container.runStartBtn.disabled = false;
    closeRunStream();
  }

  function appendTranscript(el: HTMLElement): void {
    container.runTranscriptEl.appendChild(el);
    el.scrollIntoView({ block: "end" });
  }

  function handleRunEvent(ev: WorkflowRunEvent): void {
    switch (ev.type) {
      case "WorkflowStarted": {
        const line = document.createElement("div");
        line.className = "wf-run-line wf-run-started";
        line.textContent = `▶ 开始 workflow "${ev.name}" — topic: ${ev.topic}`;
        appendTranscript(line);
        break;
      }
      case "WorkflowStep": {
        const div = document.createElement("div");
        div.className = "wf-run-step-divider";
        div.textContent = `步骤 ${ev.index}/${ev.total}: ${ev.step_id}${ev.description ? ` — ${ev.description}` : ""}`;
        appendTranscript(div);
        break;
      }
      case "WorkflowTurn": {
        runTranscriptText += ev.content + "\n";
        const turn = document.createElement("div");
        turn.className = "wf-run-turn";
        const head = document.createElement("div");
        head.className = "wf-run-turn-role";
        head.textContent = `${ev.role_id}（round ${ev.round}）`;
        const content = document.createElement("div");
        content.className = "wf-run-turn-content";
        content.textContent = ev.content;
        turn.appendChild(head);
        turn.appendChild(content);
        appendTranscript(turn);
        break;
      }
      case "WorkflowFinished": {
        const line = document.createElement("div");
        const ok = ev.status === "ok";
        line.className = `wf-run-finished ${ok ? "ok" : "failed"}`;
        const label = ok ? "✓ 完成" : ev.status === "cancelled" ? "⏹ 已取消" : "✗ 失败";
        line.textContent = `${label} — ${ev.name}${ev.summary ? `\n${ev.summary}` : ""}`;
        appendTranscript(line);
        setRunStatus(label, !ok);
        finishRun();
        // 成功结束：扫描 transcript 里的 ```json 任务列表，
        // 能解析出来就亮出「导入任务看板」按钮。
        if (ok) {
          const found = extractImportableTasks(runTranscriptText);
          if (found) {
            importableTasks = found;
            importBtn.textContent = `📥 导入任务看板（${found.length} 个任务）`;
            importBtn.classList.remove("hidden");
          }
        }
        break;
      }
    }
  }

  return {
    isOpen: () => !container.panelEl.classList.contains("hidden"),
    open: () => {
      container.panelEl.classList.remove("hidden");
      void refresh();
    },
  };
}
