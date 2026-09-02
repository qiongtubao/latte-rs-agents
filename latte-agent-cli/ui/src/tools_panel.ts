// 工具管理页面：全功能工具查看/搜索/过滤/批量操作面板。
// 支持工具测试（测试工具是否能正常执行），以及工具文档（模型运行时读取的
// `prompts/tools/<id>.md`）的查看/编辑 —— dynamic 工具只读，其余可编辑保存。
import { listTools, setToolEnabled, testTool, getToolDoc, putToolDoc } from "./api";
import type { ToolEntry, TestToolResponse, ToolDocResponse } from "./api";
import { renderMarkdown } from "./chat_impl";

// ─── 类型定义 ────────────────────────────────────────────────────────

export interface UIBinding {
  panelEl: HTMLElement;
  openBtn: HTMLButtonElement;
  closeBtn: HTMLButtonElement;
  refreshBtn: HTMLButtonElement;
  bodyEl: HTMLElement;
  statusEl: HTMLElement;
  filterEl: HTMLInputElement;
}

export interface ToolsPageController {
  isOpen(): boolean;
  open(): void;
  close(): void;
  /** 打开面板并定位到指定工具（清空过滤、滚动到卡片并高亮）。 */
  selectTool(id: string): void;
}

interface Stats {
  total: number;
  enabled: number;
  disabled: number;
  byKind: Record<string, number>;
}

// ─── 挂载函数 ────────────────────────────────────────────────────────

export function mountToolsPage(opts: { container: UIBinding }): ToolsPageController {
  const { container } = opts;
  let entries: ToolEntry[] = [];
  let filterText = "";
  let kindFilter = "all";
  let statusFilter: "all" | "enabled" | "disabled" = "all";
  let selectedIds = new Set<string>();
  let autoRefreshTimer: ReturnType<typeof setInterval> | null = null;

  container.openBtn.addEventListener("click", function () {
    container.panelEl.classList.remove("hidden");
    // 显式调用 refresh 和 auto-refresh，返回值赋值给临时变量防止 tree-shaking
    const _r = refresh();
    const _s = startAutoRefresh();
  });
  container.closeBtn.addEventListener("click", function () {
    container.panelEl.classList.add("hidden");
    const _s = stopAutoRefresh();
  });
  container.refreshBtn.addEventListener("click", function () {
    const _r = refresh();
  });
  container.filterEl.addEventListener("input", function () {
    filterText = container.filterEl.value.trim().toLowerCase();
    render();
  });

  const setStatus = (msg: string, error = false) => {
    container.statusEl.textContent = msg;
    container.statusEl.classList.toggle("error", error);
  };

  function startAutoRefresh(): void {
    stopAutoRefresh();
    autoRefreshTimer = setInterval(() => { const _r = refresh(false); }, 15000);
  }
  function stopAutoRefresh(): void {
    if (autoRefreshTimer !== null) { clearInterval(autoRefreshTimer); autoRefreshTimer = null; }
  }

  async function refresh(showStatus = true): Promise<void> {
    if (showStatus) setStatus("加载中…");
    try {
      const resp = await listTools();
      entries = resp;
      if (showStatus) setStatus(`已加载 ${entries.length} 个工具`);
      render();
    } catch (e) {
      const msg = (e as Error).message;
      if (showStatus) setStatus(`加载失败: ${msg}`, true);
      renderError(msg);
    }
  }

  function getStats(): Stats {
    const byKind: Record<string, number> = {};
    let enabled = 0, disabled = 0;
    for (const t of entries) {
      const k = t.kind || "other";
      byKind[k] = (byKind[k] || 0) + 1;
      if (t.enabled) enabled++; else disabled++;
    }
    return { total: entries.length, enabled, disabled, byKind };
  }

  function getFiltered(): ToolEntry[] {
    return entries.filter(t => {
      // text filter
      if (filterText) {
        const haystack = `${t.id} ${t.description ?? ""} ${t.registered_by ?? ""}`.toLowerCase();
        if (!haystack.includes(filterText)) return false;
      }
      // kind filter
      if (kindFilter !== "all" && t.kind !== kindFilter) return false;
      // status filter
      if (statusFilter === "enabled" && !t.enabled) return false;
      if (statusFilter === "disabled" && t.enabled) return false;
      return true;
    });
  }

  function render(): void {
    container.bodyEl.replaceChildren();
    const stats = getStats();
    const filtered = getFiltered();

    // ── 统计卡片区 ──
    const statsBar = document.createElement("div");
    statsBar.className = "tools-stats-bar";
    statsBar.innerHTML = `
      <div class="tools-stat-card tools-stat-total"><span class="stat-num">${stats.total}</span><span class="stat-label">总数</span></div>
      <div class="tools-stat-card tools-stat-enabled"><span class="stat-num">${stats.enabled}</span><span class="stat-label">已启用</span></div>
      <div class="tools-stat-card tools-stat-disabled"><span class="stat-num">${stats.disabled}</span><span class="stat-label">已禁用</span></div>
      ${Object.entries(stats.byKind).map(([k, n]) =>
        `<div class="tools-stat-card tools-stat-kind"><span class="stat-num">${n}</span><span class="stat-label">${kindLabel(k)}</span></div>`
      ).join("")}
    `;
    container.bodyEl.appendChild(statsBar);

    // ── 展示层提示：enable/disable 目前只影响本面板显示 ──
    const notice = document.createElement("div");
    notice.className = "tools-runtime-notice";
    notice.style.cssText =
      "margin:8px 0;padding:8px 12px;border:1px solid var(--warn,#a8832a);border-radius:6px;color:var(--warn,#a8832a);font-size:12px;line-height:1.5;";
    notice.textContent =
      "提示：此处的启用/禁用仅用于面板展示，不影响运行时——角色实际可用工具由各角色配置（agents.toml 的 tools 字段）决定，manager/advisor 的系统工具由代码兜底不可关闭。";
    container.bodyEl.appendChild(notice);

    // ── 过滤工具栏 ──
    const toolbar = document.createElement("div");
    toolbar.className = "tools-toolbar";

    const kindSelect = document.createElement("select");
    kindSelect.className = "tools-kind-select";
    const kindOptions = [
      { value: "all", label: "全部类型" },
      { value: "builtin", label: "内置工具" },
      { value: "dynamic", label: "动态注册" },
      { value: "package_alias", label: "包别名" },
    ];
    for (const o of kindOptions) {
      const opt = document.createElement("option");
      opt.value = o.value; opt.textContent = o.label;
      kindSelect.appendChild(opt);
    }
    kindSelect.value = kindFilter;
    kindSelect.addEventListener("change", () => { kindFilter = kindSelect.value; render(); });
    toolbar.appendChild(kindSelect);

    const statusSelect = document.createElement("select");
    statusSelect.className = "tools-status-select";
    const statusOptions = [
      { value: "all", label: "全部状态" },
      { value: "enabled", label: "仅已启用" },
      { value: "disabled", label: "仅已禁用" },
    ];
    for (const o of statusOptions) {
      const opt = document.createElement("option");
      opt.value = o.value; opt.textContent = o.label;
      statusSelect.appendChild(opt);
    }
    statusSelect.value = statusFilter;
    statusSelect.addEventListener("change", () => { statusFilter = statusSelect.value as typeof statusFilter; render(); });
    toolbar.appendChild(statusSelect);

    // 批量操作按钮
    const batchActions = document.createElement("div");
    batchActions.className = "tools-batch-actions";
    const batchEnableBtn = document.createElement("button");
    batchEnableBtn.type = "button";
    batchEnableBtn.className = "tools-batch-btn";
    batchEnableBtn.textContent = `批量启用 (${selectedIds.size})`;
    batchEnableBtn.disabled = selectedIds.size === 0;
    batchEnableBtn.addEventListener("click", () => void batchToggle(true));
    batchActions.appendChild(batchEnableBtn);

    const batchDisableBtn = document.createElement("button");
    batchDisableBtn.type = "button";
    batchDisableBtn.className = "tools-batch-btn";
    batchDisableBtn.textContent = `批量禁用 (${selectedIds.size})`;
    batchDisableBtn.disabled = selectedIds.size === 0;
    batchDisableBtn.addEventListener("click", () => void batchToggle(false));
    batchActions.appendChild(batchDisableBtn);

    if (selectedIds.size > 0) {
      const clearSel = document.createElement("button");
      clearSel.type = "button";
      clearSel.className = "tools-batch-btn tools-batch-clear";
      clearSel.textContent = "取消选择";
      clearSel.addEventListener("click", () => { selectedIds.clear(); render(); });
      batchActions.appendChild(clearSel);
    }
    toolbar.appendChild(batchActions);
    container.bodyEl.appendChild(toolbar);

    // ── 搜索结果计数 ──
    if (filterText || kindFilter !== "all" || statusFilter !== "all") {
      const resultCount = document.createElement("div");
      resultCount.className = "tools-result-count";
      resultCount.textContent = `匹配 ${filtered.length} / ${entries.length} 个工具`;
      container.bodyEl.appendChild(resultCount);
    }

    // ── 工具列表 ──
    if (filtered.length === 0) {
      const empty = document.createElement("div");
      empty.className = "tools-empty";
      empty.textContent = "没有匹配的工具。";
      container.bodyEl.appendChild(empty);
      return;
    }

    // 按类型分组
    const groups = new Map<string, ToolEntry[]>();
    for (const t of filtered) {
      const k = t.kind || "other";
      if (!groups.has(k)) groups.set(k, []);
      groups.get(k)!.push(t);
    }
    const order = ["builtin", "package_alias", "dynamic", "other"];

    for (const kind of order) {
      const items = groups.get(kind);
      if (!items || items.length === 0) continue;

      const groupEl = document.createElement("section");
      groupEl.className = "tools-group";

      const heading = document.createElement("h3");
      heading.className = "tools-group-heading";
      heading.textContent = `${kindLabel(kind)} (${items.length})`;
      groupEl.appendChild(heading);

      const list = document.createElement("div");
      list.className = "tools-list";
      for (const t of items) list.appendChild(renderCard(t));
      groupEl.appendChild(list);
      container.bodyEl.appendChild(groupEl);
    }
  }

  /** 渲染加载失败的错误状态 */
  function renderError(msg: string): void {
    container.bodyEl.replaceChildren();
    // 统计卡片（全 0）
    const statsBar = document.createElement("div");
    statsBar.className = "tools-stats-bar";
    statsBar.innerHTML = `
      <div class="tools-stat-card tools-stat-total"><span class="stat-num">0</span><span class="stat-label">总数</span></div>
      <div class="tools-stat-card tools-stat-enabled"><span class="stat-num">0</span><span class="stat-label">已启用</span></div>
      <div class="tools-stat-card tools-stat-disabled"><span class="stat-num">0</span><span class="stat-label">已禁用</span></div>
    `;
    container.bodyEl.appendChild(statsBar);
    // 错误提示区域
    const errorBox = document.createElement("div");
    errorBox.className = "tools-error-box";
    const errorIcon = document.createElement("span");
    errorIcon.className = "tools-error-icon";
    errorIcon.textContent = "⚠️";
    errorBox.appendChild(errorIcon);
    const errorMsg = document.createElement("span");
    errorMsg.className = "tools-error-msg";
    errorMsg.textContent = msg;
    errorBox.appendChild(errorMsg);
    const retryBtn = document.createElement("button");
    retryBtn.type = "button";
    retryBtn.className = "tools-error-retry";
    retryBtn.textContent = "重试";
    retryBtn.addEventListener("click", () => { const _r = refresh(); });
    errorBox.appendChild(retryBtn);
    container.bodyEl.appendChild(errorBox);
  }

  function renderCard(t: ToolEntry): HTMLElement {
    const card = document.createElement("div");
    card.className = "tools-card" + (t.enabled ? "" : " tools-card-disabled");
    card.dataset.toolId = t.id;

    // 选择框（批量操作）
    const sel = document.createElement("input");
    sel.type = "checkbox";
    sel.className = "tools-card-select";
    sel.checked = selectedIds.has(t.id);
    sel.addEventListener("change", () => {
      if (sel.checked) selectedIds.add(t.id);
      else selectedIds.delete(t.id);
      render(); // 重新渲染以更新批量按钮状态
    });
    card.appendChild(sel);

    const body = document.createElement("div");
    body.className = "tools-card-body";

    // 头部：ID + 状态标签 + 类型标签
    const header = document.createElement("div");
    header.className = "tools-card-header";

    const idEl = document.createElement("span");
    idEl.className = "tools-card-id";
    idEl.textContent = t.id;
    header.appendChild(idEl);

    const statusBadge = document.createElement("span");
    statusBadge.className = `tools-card-badge ${t.enabled ? "badge-enabled" : "badge-disabled"}`;
    statusBadge.textContent = t.enabled ? "已启用" : "已禁用";
    header.appendChild(statusBadge);

    const kindBadge = document.createElement("span");
    kindBadge.className = `tools-card-badge badge-kind badge-${t.kind || "other"}`;
    kindBadge.textContent = kindLabel(t.kind || "other");
    header.appendChild(kindBadge);

    body.appendChild(header);

    // 描述
    if (t.description) {
      const desc = document.createElement("div");
      desc.className = "tools-card-desc";
      desc.textContent = t.description;
      body.appendChild(desc);
    }

    // 注册点 / 元信息
    if (t.registered_by) {
      const meta = document.createElement("div");
      meta.className = "tools-card-meta";
      meta.innerHTML = `<span class="meta-label">注册点：</span><code>${t.registered_by}</code>`;
      body.appendChild(meta);
    }

    // 点击卡片主体查看详情
    body.style.cursor = "pointer";
    body.addEventListener("click", (e) => {
      if (e.target === sel || toggle.contains(e.target as Node)) return;
      showToolDetail(t);
    });
    card.appendChild(body);

    // 开关
    const toggle = document.createElement("label");
    toggle.className = "tools-card-toggle";
    const cb = document.createElement("input");
    cb.type = "checkbox";
    cb.checked = t.enabled;
    cb.addEventListener("change", () => void onToggle(t, cb.checked));
    const toggleLabel = document.createElement("span");
    toggleLabel.textContent = t.enabled ? "启用" : "禁用";
    cb.addEventListener("change", () => { toggleLabel.textContent = cb.checked ? "启用" : "禁用"; });
    toggle.append(cb, toggleLabel);
    card.appendChild(toggle);

    // 测试按钮
    const testBtn = document.createElement("button");
    testBtn.type = "button";
    testBtn.className = "tools-card-test-btn";
    testBtn.textContent = "测试";
    testBtn.title = "测试工具是否能正常执行";
    testBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      void onTestTool(t);
    });
    card.appendChild(testBtn);

    return card;
  }

  async function onToggle(t: ToolEntry, enabled: boolean): Promise<void> {
    setStatus(`${t.id}: ${enabled ? "启用中…" : "禁用中…"}`);
    try {
      await setToolEnabled(t.id, enabled);
      const i = entries.findIndex(x => x.id === t.id);
      if (i >= 0) entries[i] = { ...entries[i], enabled };
      setStatus(`✅ ${t.id} 已${enabled ? "启用" : "禁用"}（新 session 生效）`);
      render();
    } catch (e) {
      setStatus(`切换失败: ${(e as Error).message}`, true);
    }
  }

  /** 测试工具是否能正常执行。简单工具直接用默认参数，复杂工具在详情弹层里测试。 */
  async function onTestTool(t: ToolEntry): Promise<void> {
    setStatus(`${t.id}: 测试中…`);
    const defaultArgs: Record<string, unknown> = {};
    if (t.id === "read" || t.id.endsWith(".read") || t.id === "file.read") {
      defaultArgs.path = "Cargo.toml";
    } else if (t.id === "exec" || t.id.endsWith(".exec") || t.id === "shell.exec") {
      defaultArgs.command = "echo hello";
    } else if (t.id === "write" || t.id.endsWith(".write") || t.id === "file.write") {
      defaultArgs.path = "/tmp/latte-test.txt";
      defaultArgs.content = "test";
    } else if (t.id === "list" || t.id.endsWith(".list") || t.id === "file.list") {
      defaultArgs.path = ".";
    } else if (t.id === "grep" || t.id.endsWith(".grep") || t.id === "search.grep") {
      defaultArgs.pattern = "test";
      defaultArgs.path = ".";
    } else if (t.id === "glob" || t.id.endsWith(".glob") || t.id === "file.glob") {
      defaultArgs.path = "*";
    } else if (t.id === "delegate") {
      defaultArgs.role_id = "manager";
      defaultArgs.task = "测试任务";
    } else if (t.id === "ast_grep" || t.id === "code_graph") {
      defaultArgs.pattern = "fn main";
      defaultArgs.path = ".";
    } else {
      defaultArgs.input = "test";
    }
    try {
      const resp = await testTool({ tool_id: t.id, args: defaultArgs });
      if (resp.ok) {
        setStatus(`✅ ${t.id} 测试通过 (${resp.latency_ms}ms)`);
      } else {
        setStatus(`✗ ${t.id} 测试失败: ${resp.error ?? "未知错误"}`, true);
      }
    } catch (err) {
      setStatus(`✗ ${t.id} 测试失败: ${(err as Error).message}`, true);
    }
  }

  async function batchToggle(enabled: boolean): Promise<void> {
    const ids = [...selectedIds];
    setStatus(`批量${enabled ? "启用" : "禁用"} ${ids.length} 个工具…`);
    let ok = 0, fail = 0;
    for (const id of ids) {
      try {
        await setToolEnabled(id, enabled);
        const i = entries.findIndex(x => x.id === id);
        if (i >= 0) entries[i] = { ...entries[i], enabled };
        ok++;
      } catch {
        fail++;
      }
    }
    selectedIds.clear();
    setStatus(`✅ 已${enabled ? "启用" : "禁用"} ${ok} 个工具${fail ? `，${fail} 个失败` : ""}`);
    render();
  }

  // ─── 工具详情弹层 ─────────────────────────────────────────────

  function showToolDetail(t: ToolEntry): void {
    const overlay = document.getElementById("tool-detail-overlay")!;
    const title = document.getElementById("tool-detail-title")!;
    const body = document.getElementById("tool-detail-body")!;
    const closeBtn = document.getElementById("tool-detail-close") as HTMLButtonElement;

    title.textContent = `🔧 ${t.id}`;
    body.replaceChildren();

    // ── 简介区：ID / 类型 / 状态 / 描述 / 注册点 ──
    const infoSection = document.createElement("section");
    infoSection.className = "tool-detail-section";

    const infoHeading = document.createElement("div");
    infoHeading.className = "tool-detail-section-heading";
    infoHeading.textContent = "简介";
    infoSection.appendChild(infoHeading);

    const rows: { label: string; value: string }[] = [
      { label: "ID", value: t.id },
      { label: "类型", value: kindLabel(t.kind || "other") },
      { label: "状态", value: t.enabled ? "已启用" : "已禁用" },
    ];
    if (t.description) rows.push({ label: "描述", value: t.description });
    if (t.registered_by) rows.push({ label: "注册点", value: t.registered_by });
    for (const r of rows) {
      const row = document.createElement("div");
      row.className = "tool-detail-row";
      const label = document.createElement("span");
      label.className = "tool-detail-label";
      label.textContent = r.label;
      const value = document.createElement("span");
      value.className = "tool-detail-value";
      value.textContent = r.value;
      row.append(label, value);
      infoSection.appendChild(row);
    }
    body.appendChild(infoSection);

    // ── 文档区：模型运行时读取的 prompts/tools/<id>.md ──
    const docSection = document.createElement("section");
    docSection.className = "tool-detail-section tool-doc-section";
    body.appendChild(docSection);
    void renderToolDoc(t, docSection);

    overlay.classList.remove("hidden");
    const close = () => overlay.classList.add("hidden");
    closeBtn.onclick = close;
    overlay.onclick = (e) => { if (e.target === overlay) close(); };
  }

  /**
   * 渲染工具文档区：拉取 `GET /api/tools/:id/doc`，展示模型运行时真正读到的
   * 那份 Markdown（磁盘 `prompts/tools/<id>.md` 优先，回退编译期内置常量）。
   *
   * 可编辑性由后端 `editable` 决定：dynamic（controller 运行时动态注册，
   * 如 delegate / workflow）只读；builtin / package_alias 可编辑，保存写到
   * `<cwd>/prompts/tools/<id>.md`，新 session 生效。
   */
  async function renderToolDoc(t: ToolEntry, host: HTMLElement): Promise<void> {
    host.replaceChildren();

    const heading = document.createElement("div");
    heading.className = "tool-detail-section-heading";
    heading.textContent = "工具文档（模型读取）";
    host.appendChild(heading);

    const loading = document.createElement("div");
    loading.className = "tool-doc-loading";
    loading.textContent = "加载文档中…";
    host.appendChild(loading);

    let doc: ToolDocResponse;
    try {
      doc = await getToolDoc(t.id);
    } catch (e) {
      loading.remove();
      const err = document.createElement("div");
      err.className = "tool-doc-error";
      err.textContent = `文档加载失败：${(e as Error).message}`;
      host.appendChild(err);
      return;
    }
    loading.remove();

    // ── 元信息条：来源徽章 + 可编辑徽章 + 文件路径 ──
    const metaBar = document.createElement("div");
    metaBar.className = "tool-doc-meta";

    const sourceBadge = document.createElement("span");
    sourceBadge.className = `tool-doc-badge tool-doc-source-${doc.source}`;
    sourceBadge.textContent = sourceLabel(doc.source);
    sourceBadge.title = sourceHint(doc.source);
    metaBar.appendChild(sourceBadge);

    const editBadge = document.createElement("span");
    editBadge.className = `tool-doc-badge ${doc.editable ? "tool-doc-editable" : "tool-doc-readonly"}`;
    editBadge.textContent = doc.editable ? "可编辑" : "只读";
    editBadge.title = doc.editable
      ? `可保存到 ${doc.path}（新 session 生效）`
      : "动态注册工具（运行时由 controller 注册），其文档不可编辑";
    metaBar.appendChild(editBadge);

    const pathEl = document.createElement("code");
    pathEl.className = "tool-doc-path";
    pathEl.textContent = doc.path;
    metaBar.appendChild(pathEl);

    const actions = document.createElement("div");
    actions.className = "tool-doc-actions";
    metaBar.appendChild(actions);
    host.appendChild(metaBar);

    // ── 内容区：查看（渲染 markdown）/ 编辑（textarea）双态 ──
    const viewEl = document.createElement("div");
    viewEl.className = "tool-doc-view markdown-body";

    const editEl = document.createElement("textarea");
    editEl.className = "tool-doc-editor";
    editEl.spellcheck = false;

    const statusEl = document.createElement("div");
    statusEl.className = "tool-doc-status";

    const paintView = (markdown: string) => {
      if (markdown.trim()) {
        viewEl.innerHTML = renderMarkdown(markdown);
      } else {
        viewEl.replaceChildren();
        const empty = document.createElement("div");
        empty.className = "tool-doc-empty";
        empty.textContent = doc.editable
          ? "该工具还没有文档。点击「编辑」新建 —— 保存后模型会在下个 session 读到它。"
          : "该工具没有文档。";
        viewEl.appendChild(empty);
      }
    };

    let editing = false;
    let current = doc.content;
    paintView(current);

    host.append(viewEl, editEl, statusEl);

    const syncMode = () => {
      viewEl.classList.toggle("hidden", editing);
      editEl.classList.toggle("hidden", !editing);
    };
    syncMode();

    // 只读工具：不渲染编辑按钮，只给一条说明。
    if (!doc.editable) {
      const note = document.createElement("span");
      note.className = "tool-doc-note";
      note.textContent = "动态工具不可编辑";
      actions.appendChild(note);
      return;
    }

    const editBtn = document.createElement("button");
    editBtn.type = "button";
    editBtn.className = "tool-doc-btn";
    editBtn.textContent = "编辑";

    const saveBtn = document.createElement("button");
    saveBtn.type = "button";
    saveBtn.className = "tool-doc-btn tool-doc-btn-primary hidden";
    saveBtn.textContent = "保存";

    const cancelBtn = document.createElement("button");
    cancelBtn.type = "button";
    cancelBtn.className = "tool-doc-btn hidden";
    cancelBtn.textContent = "取消";

    const setEditing = (on: boolean) => {
      editing = on;
      syncMode();
      editBtn.classList.toggle("hidden", on);
      saveBtn.classList.toggle("hidden", !on);
      cancelBtn.classList.toggle("hidden", !on);
    };

    editBtn.addEventListener("click", () => {
      editEl.value = current;
      setEditing(true);
      statusEl.textContent = "";
      statusEl.classList.remove("error");
      editEl.focus();
    });

    cancelBtn.addEventListener("click", () => {
      setEditing(false);
      statusEl.textContent = "";
      statusEl.classList.remove("error");
    });

    saveBtn.addEventListener("click", () => {
      void (async () => {
        const next = editEl.value;
        saveBtn.disabled = true;
        statusEl.classList.remove("error");
        statusEl.textContent = "保存中…";
        try {
          await putToolDoc(t.id, next);
          current = next;
          paintView(current);
          setEditing(false);
          statusEl.textContent = `✅ 已保存到 ${doc.path}（新 session 生效）`;
          // 保存后来源必定变成磁盘覆盖文件。
          doc.source = "disk";
          sourceBadge.className = "tool-doc-badge tool-doc-source-disk";
          sourceBadge.textContent = sourceLabel("disk");
          sourceBadge.title = sourceHint("disk");
          setStatus(`✅ ${t.id} 文档已保存`);
        } catch (e) {
          statusEl.classList.add("error");
          statusEl.textContent = `保存失败：${(e as Error).message}`;
        } finally {
          saveBtn.disabled = false;
        }
      })();
    });

    actions.append(editBtn, saveBtn, cancelBtn);
  }

  return {
    isOpen: () => !container.panelEl.classList.contains("hidden"),
    open: function () {
      container.panelEl.classList.remove("hidden");
      const _r = refresh();
      const _s = startAutoRefresh();
    },
    close: function () {
      container.panelEl.classList.add("hidden");
      const _s = stopAutoRefresh();
    },
    selectTool: function (id: string) {
      container.panelEl.classList.remove("hidden");
      const _s = startAutoRefresh();
      void (async () => {
        if (entries.length === 0) await refresh();
        // 清空所有过滤条件，确保目标工具卡片一定渲染出来
        filterText = "";
        kindFilter = "all";
        statusFilter = "all";
        container.filterEl.value = "";
        render();
        const card = container.bodyEl.querySelector<HTMLElement>(`[data-tool-id="${CSS.escape(id)}"]`);
        if (!card) {
          setStatus(`未找到工具「${id}」`, true);
          return;
        }
        card.scrollIntoView({ behavior: "smooth", block: "center" });
        // 重新触发高亮动画（连续跳转同一个工具也能看到闪烁）
        card.classList.remove("highlight-flash");
        void card.offsetWidth;
        card.classList.add("highlight-flash");
        setStatus(`已定位到工具「${id}」`);
      })();
    },
  };
}

function kindLabel(kind: string): string {
  switch (kind) {
    case "builtin": return "内置工具";
    case "dynamic": return "动态注册";
    case "package_alias": return "包别名";
    default: return kind;
  }
}

/** 文档来源徽章文案。 */
function sourceLabel(source: string): string {
  switch (source) {
    case "disk": return "项目文件";
    case "embedded": return "内置默认";
    case "none": return "无文档";
    default: return source;
  }
}

/** 文档来源 tooltip：解释这份文档从哪来、模型读的是哪一份。 */
function sourceHint(source: string): string {
  switch (source) {
    case "disk":
      return "来自项目本地 prompts/tools/ 下的文件，优先级最高——模型运行时读的就是这一份。";
    case "embedded":
      return "来自编译期内置的默认文档。保存编辑会在项目里新建覆盖文件，此后模型改读那一份。";
    case "none":
      return "该工具当前没有任何文档（既无项目文件也无内置默认）。";
    default:
      return "";
  }
}

// 向后兼容别名：main.ts 仍引用 mountToolsPanel
export const mountToolsPanel = mountToolsPage;