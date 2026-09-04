// 工具管理页面：全功能工具查看/搜索/过滤/批量操作面板。
// 支持工具测试，以及项目/全局工具文档（`.latte/tools.d/<id>.md`）的查看与编辑。
// 项目文档覆盖 `$LATTE_HOME/tools.d/<id>.md`（默认 `~/.latte/tools.d`）。
import {
  listToolsWithMeta,
  setToolEnabled,
  testTool,
  getToolDoc,
  putToolDoc,
  deleteToolDoc,
} from "./api";
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
  /** 已写过项目/全局文档的工具数（缺文档的那些模型只能看到内置描述）。 */
  withDoc: number;
  byKind: Record<string, number>;
}

// ─── 挂载函数 ────────────────────────────────────────────────────────

export function mountToolsPage(opts: { container: UIBinding }): ToolsPageController {
  const { container } = opts;
  let entries: ToolEntry[] = [];
  let mcpServers: string[] = [];
  let filterText = "";
  let kindFilter = "all";
  let statusFilter: "all" | "enabled" | "disabled" = "all";
  let docFilter: "all" | "with" | "without" = "all";
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
      const resp = await listToolsWithMeta();
      entries = resp.tools;
      mcpServers = resp.mcp_servers ?? [];
      if (showStatus) {
        const missing = entries.filter((t) => t.has_doc !== true).length;
        setStatus(`已加载 ${entries.length} 个工具（${missing} 个还没写文档）`);
      }
      render();
    } catch (e) {
      const msg = (e as Error).message;
      if (showStatus) setStatus(`加载失败: ${msg}`, true);
      renderError(msg);
    }
  }

  function getStats(): Stats {
    const byKind: Record<string, number> = {};
    let enabled = 0, disabled = 0, withDoc = 0;
    for (const t of entries) {
      const k = t.kind || "other";
      byKind[k] = (byKind[k] || 0) + 1;
      if (t.enabled) enabled++; else disabled++;
      if (t.has_doc === true) withDoc++;
    }
    return { total: entries.length, enabled, disabled, withDoc, byKind };
  }

  function getFiltered(): ToolEntry[] {
    return entries.filter(t => {
      // text filter
      if (filterText) {
        const haystack = `${t.id} ${t.label ?? ""} ${t.description ?? ""} ${t.registered_by ?? ""}`.toLowerCase();
        if (!haystack.includes(filterText)) return false;
      }
      // kind filter
      if (kindFilter !== "all" && t.kind !== kindFilter) return false;
      // status filter
      if (statusFilter === "enabled" && !t.enabled) return false;
      if (statusFilter === "disabled" && t.enabled) return false;
      // doc filter：用来把「还没写文档」的工具集中筛出来补齐
      if (docFilter === "with" && t.has_doc !== true) return false;
      if (docFilter === "without" && t.has_doc === true) return false;
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
      <div class="tools-stat-card tools-stat-enabled"><span class="stat-num">${stats.withDoc}</span><span class="stat-label">已写文档</span></div>
      <div class="tools-stat-card tools-stat-disabled"><span class="stat-num">${stats.total - stats.withDoc}</span><span class="stat-label">缺文档</span></div>
      ${Object.entries(stats.byKind).map(([k, n]) =>
        `<div class="tools-stat-card tools-stat-kind"><span class="stat-num">${n}</span><span class="stat-label">${kindLabel(k)}</span></div>`
      ).join("")}
    `;
    container.bodyEl.appendChild(statsBar);

    // ── 外部 MCP server 列表：外部工具从哪来的，一眼可查 ──
    if (mcpServers.length > 0) {
      const mcpBar = document.createElement("div");
      mcpBar.className = "tools-mcp-bar";
      const label = document.createElement("span");
      label.className = "meta-label";
      label.textContent = `已连接 MCP server（${mcpServers.length}）：`;
      mcpBar.appendChild(label);
      for (const server of mcpServers) {
        const code = document.createElement("code");
        code.textContent = server;
        mcpBar.appendChild(code);
      }
      container.bodyEl.appendChild(mcpBar);
    }

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
      { value: "package_alias", label: "配置别名" },
      { value: "mcp", label: "外部 MCP" },
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

    const docSelect = document.createElement("select");
    docSelect.className = "tools-doc-select";
    for (const o of [
      { value: "all", label: "全部文档状态" },
      { value: "with", label: "已写文档" },
      { value: "without", label: "缺文档" },
    ]) {
      const opt = document.createElement("option");
      opt.value = o.value; opt.textContent = o.label;
      docSelect.appendChild(opt);
    }
    docSelect.value = docFilter;
    docSelect.addEventListener("change", () => { docFilter = docSelect.value as typeof docFilter; render(); });
    toolbar.appendChild(docSelect);

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
    if (filterText || kindFilter !== "all" || statusFilter !== "all" || docFilter !== "all") {
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
    const order = ["builtin", "dynamic", "mcp", "package_alias", "other"];

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
    if (t.label && t.label !== t.id) idEl.title = t.label;
    header.appendChild(idEl);

    const statusBadge = document.createElement("span");
    statusBadge.className = `tools-card-badge ${t.enabled ? "badge-enabled" : "badge-disabled"}`;
    statusBadge.textContent = t.enabled ? "已启用" : "已禁用";
    header.appendChild(statusBadge);

    const kindBadge = document.createElement("span");
    kindBadge.className = `tools-card-badge badge-kind badge-${t.kind || "other"}`;
    kindBadge.textContent = kindLabel(t.kind || "other");
    header.appendChild(kindBadge);

    // 文档状态：一眼看出哪些工具还没写说明（模型看到的就只有内置描述）。
    const docBadge = document.createElement("span");
    const hasDoc = t.has_doc === true;
    docBadge.className = `tools-card-badge ${hasDoc ? "badge-doc-yes" : "badge-doc-no"}`;
    docBadge.textContent = hasDoc
      ? (t.doc_source === "global" ? "全局文档" : "项目文档")
      : "无文档";
    docBadge.title = hasDoc
      ? "已有 .latte/tools.d 文档，运行时会追加给模型"
      : "还没写文档，模型只能看到内置描述";
    header.appendChild(docBadge);

    body.appendChild(header);

    // 描述：卡片上只放一行简介（注册描述的首行/首句），完整描述进 tooltip。
    // 后端返回的是模型看到的完整描述，`read` 那种能有 200+ 字，直接铺在
    // 列表里就是一片文字墙。
    if (t.description) {
      const desc = document.createElement("div");
      desc.className = "tools-card-desc";
      desc.textContent = briefOf(t.description);
      desc.title = t.description;
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

    // ── 概览区：ID / 类型 / 状态 / 注册点 ──
    // 「描述」不放这里：它和下面文档区的「简介」是同一件事，重复两遍还
    // 各占一大段。这里只留身份信息，描述交给文档区（那边拿到的是拆过
    // 首句的一行简介）。
    const infoSection = document.createElement("section");
    infoSection.className = "tool-detail-section";

    const infoHeading = document.createElement("div");
    infoHeading.className = "tool-detail-section-heading";
    infoHeading.textContent = "概览";
    infoSection.appendChild(infoHeading);

    const rows: { label: string; value: string }[] = [
      { label: "ID", value: t.id },
      { label: "类型", value: kindLabel(t.kind || "other") },
      { label: "状态", value: t.enabled ? "已启用" : "已禁用" },
    ];
    if (t.label && t.label !== t.id) rows.push({ label: "名称", value: t.label });
    if (t.registered_by) {
      rows.push({
        label: t.kind === "mcp" ? "MCP server" : "注册点",
        value: t.registered_by,
      });
    }
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

    // ── 文档区：模型运行时读取的说明（注册描述 + 项目/全局 md） ──
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
   * 那份说明。
   *
   * 分三层（对齐 oh-my-pi 的工具文档约定：一份 md 即工具说明，首行一句话
   * 说明用途，其后才是细则）：
   *   - **简介**：`brief`，一行。md 写过 SUMMARY 用它，否则取注册描述首句。
   *   - **详情 / 内置说明**：`builtin_detail`，注册描述里简介之后的部分。
   *     模型总能看到，但它编译进代码，面板里只读。
   *   - **详情 / 项目文档**：`.latte/tools.d/<id>.md`，可编辑，保存后追加给模型。
   */
  async function renderToolDoc(
    t: ToolEntry,
    host: HTMLElement,
    /** 重画后要显示的提示。删除文档会整块重画（分层状态变了），
     *  提示必须写进**新**的 statusEl，否则会被这次重画抹掉。 */
    notice?: string,
  ): Promise<void> {
    host.replaceChildren();

    const heading = document.createElement("div");
    heading.className = "tool-detail-section-heading";
    heading.textContent = "工具说明（模型读取）";
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

    // 走别名回退时说清楚「这份文档其实是 <alias>.md」，否则用户会以为
    // 自己在编辑 mcp_call 专属文档，一保存反而新建了一个文件。
    if (doc.matched_id && doc.matched_id !== doc.id) {
      const aliasNote = document.createElement("span");
      aliasNote.className = "tool-doc-badge tool-doc-alias-note";
      aliasNote.textContent = `继承自 ${doc.matched_id}.md`;
      aliasNote.title =
        `当前显示的是别名组文档 ${doc.matched_id}.md；保存会新建 ${doc.id} 专属文档并覆盖它。`;
      metaBar.appendChild(aliasNote);
    }

    const actions = document.createElement("div");
    actions.className = "tool-doc-actions";
    metaBar.appendChild(actions);
    host.appendChild(metaBar);

    // ── 分层条：项目层 / 全局层各自有没有文档、哪层生效 ──
    // 与 models 面板同一套语义（项目覆盖全局），并且明确写出每层的落点路径，
    // 避免「我点了保存，怎么改的不是我正在看的那份」。
    const layers = doc.layers ?? [];
    const layerBar = document.createElement("div");
    layerBar.className = "tool-doc-layers";
    for (const layer of layers) {
      const chip = document.createElement("span");
      const name = layer.layer === "global" ? "全局层" : "项目层";
      chip.className =
        "tool-doc-layer-chip" +
        (layer.exists ? " layer-exists" : " layer-empty") +
        (layer.active ? " layer-active" : "");
      chip.textContent = `${name}：${layer.exists ? (layer.active ? "生效中" : "被覆盖") : "无"}`;
      chip.title = layer.exists
        ? `${layer.path}${layer.matched_id !== doc.id ? `（别名组文档 ${layer.matched_id}.md）` : ""}`
        : `尚无文档；保存到该层会写入 ${layer.path}`;
      layerBar.appendChild(chip);
    }
    if (layers.length > 0) host.appendChild(layerBar);

    // ── 简介区（一行）──
    const summarySection = document.createElement("div");
    summarySection.className = "tool-doc-subsection";
    const summaryLabel = document.createElement("div");
    summaryLabel.className = "tool-doc-subsection-label";
    summaryLabel.textContent = "简介";
    summarySection.appendChild(summaryLabel);

    const summaryViewEl = document.createElement("div");
    summaryViewEl.className = "tool-doc-summary-view markdown-body";
    const summaryEditEl = document.createElement("textarea");
    summaryEditEl.className = "tool-doc-summary-editor hidden";
    summaryEditEl.spellcheck = false;
    summaryEditEl.placeholder = "一句话说明这个工具用来干什么（留空则沿用内置描述的首句）";
    summarySection.append(summaryViewEl, summaryEditEl);
    host.appendChild(summarySection);

    // ── 详情区：内置说明（只读）+ 项目文档（可编辑）──
    const contentSection = document.createElement("div");
    contentSection.className = "tool-doc-subsection";
    const contentLabel = document.createElement("div");
    contentLabel.className = "tool-doc-subsection-label";
    contentLabel.textContent = "详情";
    contentSection.appendChild(contentLabel);

    if (doc.builtin_detail.trim()) {
      const builtinLabel = document.createElement("div");
      builtinLabel.className = "tool-doc-origin-label";
      builtinLabel.textContent = "内置说明（随工具 schema 下发，只读）";
      const builtinView = document.createElement("div");
      builtinView.className = "tool-doc-builtin-view markdown-body";
      builtinView.innerHTML = renderMarkdown(doc.builtin_detail);
      contentSection.append(builtinLabel, builtinView);
    }

    const projectLabel = document.createElement("div");
    projectLabel.className = "tool-doc-origin-label";
    projectLabel.textContent = "项目文档（追加给模型，可编辑）";
    contentSection.appendChild(projectLabel);

    const contentViewEl = document.createElement("div");
    contentViewEl.className = "tool-doc-detail-view markdown-body";
    const contentEditEl = document.createElement("textarea");
    contentEditEl.className = "tool-doc-detail-editor hidden";
    contentEditEl.spellcheck = false;
    contentEditEl.placeholder = "写给模型的补充说明（Markdown）：调用时机、参数约定、禁忌…";
    contentSection.append(contentViewEl, contentEditEl);
    host.appendChild(contentSection);

    // ── 模板区：语法提示 + 渲染预览 ──
    // 参考 oh-my-pi 的 `prompts/tools/*.md`：文档本身可以按会话真实可用的
    // 工具改写内容（它用 handlebars 的 `{{#if hasEval}}`）。这里把可用变量/
    // 开关直接列给编辑者，并给出「渲染后模型实际读到什么」的预览——否则写
    // 模板等于盲写。
    const templateBox = document.createElement("details");
    templateBox.className = "tool-doc-template";
    const templateSummary = document.createElement("summary");
    templateSummary.textContent = doc.is_template
      ? "模板：已启用（点开看渲染结果与可用变量）"
      : "模板语法（可选）：按当前会话的工具集改写文档";
    templateBox.appendChild(templateSummary);
    templateBox.open = doc.is_template === true;

    const templateHelp = document.createElement("div");
    templateHelp.className = "tool-doc-template-help";
    templateHelp.innerHTML =
      "<code>{{CWD}}</code> 变量 · <code>{{#if has_eval}}…{{else}}…{{/if}}</code> · " +
      "<code>{{#unless is_windows}}…{{/unless}}</code>（键名大小写/下划线不敏感；未知开关按 false）";
    templateBox.appendChild(templateHelp);

    if (doc.is_template && (doc.rendered_content ?? "").trim()) {
      const previewLabel = document.createElement("div");
      previewLabel.className = "tool-doc-origin-label";
      previewLabel.textContent = "渲染预览（按当前进程可见的工具集）";
      const preview = document.createElement("div");
      preview.className = "tool-doc-builtin-view markdown-body";
      preview.innerHTML = renderMarkdown(doc.rendered_content ?? "");
      templateBox.append(previewLabel, preview);
    }

    const vars = (doc.template_vars ?? []).map((v) => `{{${v}}}`).join(" ");
    const flags = (doc.template_flags ?? []).join(" · ");
    if (vars || flags) {
      const ctxEl = document.createElement("div");
      ctxEl.className = "tool-doc-template-ctx";
      ctxEl.textContent = `可用变量：${vars || "（无）"}\n可用开关：${flags || "（无）"}`;
      templateBox.appendChild(ctxEl);
    }
    host.appendChild(templateBox);

    const statusEl = document.createElement("div");
    statusEl.className = "tool-doc-status";
    if (notice) statusEl.textContent = notice;
    host.appendChild(statusEl);

    const paintView = (summaryMd: string, contentMd: string) => {
      // 简介优先显示磁盘 md 写的那句；没写过就用后端拆好的 brief
      // （注册描述首句），而不是把整段描述倒出来。
      const effectiveBrief = summaryMd.trim() || doc.brief.trim();
      if (effectiveBrief) {
        summaryViewEl.innerHTML = renderMarkdown(effectiveBrief);
      } else {
        summaryViewEl.replaceChildren();
        const empty = document.createElement("div");
        empty.className = "tool-doc-empty";
        empty.textContent = doc.editable ? "无简介。点击「编辑」添加。" : "无简介。";
        summaryViewEl.appendChild(empty);
      }

      if (contentMd.trim()) {
        contentViewEl.innerHTML = renderMarkdown(contentMd);
      } else {
        contentViewEl.replaceChildren();
        const empty = document.createElement("div");
        empty.className = "tool-doc-empty";
        empty.textContent = doc.editable
          ? "还没有项目文档。点击「编辑」新建 —— 保存后模型会在下个 session 读到它。"
          : "没有项目文档。";
        contentViewEl.appendChild(empty);
      }
    };

    let editing = false;
    let currentSummary = doc.summary || "";
    let currentContent = doc.content || "";
    paintView(currentSummary, currentContent);

    const syncMode = () => {
      summaryViewEl.classList.toggle("hidden", editing);
      summaryEditEl.classList.toggle("hidden", !editing);
      contentViewEl.classList.toggle("hidden", editing);
      contentEditEl.classList.toggle("hidden", !editing);
    };
    syncMode();

    // 只读工具：不渲染编辑按钮，只给一条说明。
    if (!doc.editable) {
      const note = document.createElement("span");
      note.className = "tool-doc-note";
      note.textContent = "动态工具不可编辑";
      actions.appendChild(note);
      // 不要 return，让只读工具也能查看简介和详情
    } else {
      // 可编辑工具：渲染编辑/保存/取消按钮

      const editBtn = document.createElement("button");
      editBtn.type = "button";
      editBtn.className = "tool-doc-btn";
      editBtn.textContent = "编辑";

      // 两个保存按钮，落点由 data-target 决定 —— 与 models 面板的
      // 「保存到项目 / 保存到全局」一致。默认层是当前生效的那层：正在看
      // 全局文档时点保存，改的就是那份全局文档，而不是悄悄新建项目副本。
      const activeLayer: "project" | "global" =
        doc.source === "global" ? "global" : "project";
      const saveBtn = document.createElement("button");
      saveBtn.type = "button";
      saveBtn.className = "tool-doc-btn tool-doc-btn-primary hidden";
      saveBtn.dataset.target = activeLayer;
      saveBtn.textContent = activeLayer === "global" ? "保存到全局" : "保存到项目";

      const saveOtherBtn = document.createElement("button");
      saveOtherBtn.type = "button";
      saveOtherBtn.className = "tool-doc-btn hidden";
      saveOtherBtn.dataset.target = activeLayer === "global" ? "project" : "global";
      saveOtherBtn.textContent = activeLayer === "global" ? "保存到项目" : "保存到全局";
      saveOtherBtn.title =
        activeLayer === "global"
          ? "在当前项目里覆盖这份全局文档（写 .latte/tools.d/）"
          : "写到 ~/.latte/tools.d/，对所有项目生效（项目层文档仍会覆盖它）";

      const cancelBtn = document.createElement("button");
      cancelBtn.type = "button";
      cancelBtn.className = "tool-doc-btn hidden";
      cancelBtn.textContent = "取消";

      const setEditing = (on: boolean) => {
        editing = on;
        syncMode();
        editBtn.classList.toggle("hidden", on);
        saveBtn.classList.toggle("hidden", !on);
        saveOtherBtn.classList.toggle("hidden", !on);
        cancelBtn.classList.toggle("hidden", !on);
      };

      editBtn.addEventListener("click", () => {
        // 用**当前显示的**简介预填，而不是磁盘上的 md 简介：没写过 md 时
        // 显示的是内置描述的首句，编辑框却是空的——点「编辑」等于把看到的
        // 那句话弄丢了，只能重新手打。后端会在下发给模型时丢掉与内置首句
        // 逐字相同的简介，所以原样保存也不会让模型读到两遍。
        summaryEditEl.value = currentSummary || doc.brief || "";
        contentEditEl.value = currentContent;
        setEditing(true);
        statusEl.textContent = "";
        statusEl.classList.remove("error");
        summaryEditEl.focus();
      });

      cancelBtn.addEventListener("click", () => {
        setEditing(false);
        statusEl.textContent = "";
        statusEl.classList.remove("error");
      });

      const submitSave = (button: HTMLButtonElement) => {
        const target = (button.dataset.target === "global" ? "global" : "project") as
          | "project"
          | "global";
        void (async () => {
          const nextSummary = summaryEditEl.value;
          const nextContent = contentEditEl.value;
          saveBtn.disabled = true;
          saveOtherBtn.disabled = true;
          statusEl.classList.remove("error");
          statusEl.textContent = "保存中…";
          try {
            const saved = await putToolDoc(t.id, nextSummary, nextContent, target);
            currentSummary = nextSummary;
            currentContent = nextContent;
            paintView(currentSummary, currentContent);
            setEditing(false);
            // 热更新回执：后端把活跃 ToolManager 就地重新 enrich 了，
            // 所以「要不要重开 session」这件事不该让用户猜。
            const hot = saved.refreshed_managers > 0
              ? `已热更新到 ${saved.refreshed_managers} 个活跃会话`
              : "当前没有活跃会话，新建 session 即生效";
            const layerName = target === "global" ? "全局层" : "项目层";
            statusEl.textContent = `✅ 已保存到${layerName} ${saved.path || doc.path}（${hot}）`;
            // 生效层：写项目层必定生效；写全局层只有在项目层没有文档时才生效。
            const projectHasDoc =
              (doc.layers ?? []).some((l) => l.layer === "project" && l.exists) ||
              target === "project";
            const effective = projectHasDoc ? "project" : "global";
            doc.source = effective;
            sourceBadge.className = `tool-doc-badge tool-doc-source-${effective}`;
            sourceBadge.textContent = sourceLabel(effective);
            sourceBadge.title = sourceHint(effective);
            setStatus(`✅ ${t.id} 文档已保存（${hot}）`);
            // 列表里的「无文档 / 项目文档」徽章跟着变。
            const index = entries.findIndex((x) => x.id === t.id);
            if (index >= 0) {
              const hasDoc = Boolean(nextSummary.trim() || nextContent.trim());
              entries[index] = {
                ...entries[index],
                has_doc: hasDoc,
                doc_source: hasDoc ? "project" : "none",
              };
              render();
            }
          } catch (e) {
            statusEl.classList.add("error");
            statusEl.textContent = `保存失败：${(e as Error).message}`;
          } finally {
            saveBtn.disabled = false;
            saveOtherBtn.disabled = false;
          }
        })();
      };
      saveBtn.addEventListener("click", () => submitSave(saveBtn));
      saveOtherBtn.addEventListener("click", () => submitSave(saveOtherBtn));

      actions.append(editBtn, saveBtn, saveOtherBtn, cancelBtn);

      // 「删除文档」按层给：哪层有文档就给哪层一个删除按钮。删项目层后若
      // 全局层还有，运行时会回落到全局层——所以删完不能一律显示「无文档」。
      for (const layer of layers.filter((l) => l.exists)) {
        const target = (layer.layer === "global" ? "global" : "project") as
          | "project"
          | "global";
        const layerName = target === "global" ? "全局" : "项目";
        const deleteBtn = document.createElement("button");
        deleteBtn.type = "button";
        deleteBtn.className = "tool-doc-btn tool-doc-btn-danger";
        deleteBtn.dataset.target = target;
        deleteBtn.textContent = `删除${layerName}文档`;
        deleteBtn.title = `删除 ${layer.path}`;
        deleteBtn.addEventListener("click", () => {
          void (async () => {
            const otherExists = layers.some((l) => l.exists && l.layer !== layer.layer);
            const after = otherExists
              ? target === "project"
                ? "之后回落到全局文档。"
                : "项目文档仍然生效。"
              : "模型之后只会看到内置描述。";
            if (!window.confirm(`删除 ${t.id} 的${layerName}文档？${after}`)) return;
            deleteBtn.disabled = true;
            statusEl.classList.remove("error");
            statusEl.textContent = "删除中…";
            try {
              const removed = await deleteToolDoc(t.id, target);
              const hot = removed.refreshed_managers > 0
                ? `已热更新到 ${removed.refreshed_managers} 个活跃会话`
                : "当前没有活跃会话";
              const notice = `🗑️ ${layerName}文档已删除（${hot}）`;
              setStatus(`🗑️ ${t.id} 的${layerName}文档已删除（${hot}）`);
              // 分层状态变了，整块重画一次最稳（会重新 GET 拿到回落后的生效层）；
              // 提示随重画一起带进去，否则会被这次 replaceChildren 抹掉。
              await renderToolDoc(t, host, notice);
              const index = entries.findIndex((x) => x.id === t.id);
              if (index >= 0) {
                const stillHasDoc = otherExists;
                entries[index] = {
                  ...entries[index],
                  has_doc: stillHasDoc,
                  doc_source: stillHasDoc ? (target === "project" ? "global" : "project") : "none",
                };
                render();
              }
            } catch (e) {
              statusEl.classList.add("error");
              statusEl.textContent = `删除失败：${(e as Error).message}`;
              deleteBtn.disabled = false;
            }
          })();
        });
        actions.appendChild(deleteBtn);
      }
    }
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
        docFilter = "all";
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
    case "package_alias": return "配置别名";
    case "mcp": return "外部 MCP";
    default: return kind;
  }
}

/** 简介长度上限（与后端 `BRIEF_MAX_CHARS` 一致）。 */
const BRIEF_MAX_CHARS = 60;

/**
 * 取一份工具说明的「一句话简介」：首行优先，首行仍过长时按第一个句末标点
 * 断句。与后端 `split_brief_and_detail` 同一套规则——列表卡片直接拿完整
 * 注册描述渲染的话，`read` 这种 200+ 字的描述会把列表撑成文字墙。
 */
export function briefOf(text: string): string {
  const first = (text.split("\n", 1)[0] ?? "").trim();
  if (!first) return "";
  if ([...first].length <= BRIEF_MAX_CHARS) return first;
  const m = first.match(/^[\s\S]*?(?:[。！？；]|[.!?;](?=\s|$))/);
  return (m ? m[0] : first).trim();
}

/** 文档来源徽章文案。 */
function sourceLabel(source: string): string {
  switch (source) {
    case "project": return "项目文件";
    case "global": return "全局文件";
    case "none": return "无文档";
    default: return source;
  }
}

/** 文档来源 tooltip：解释这份文档从哪来、模型读的是哪一份。 */
function sourceHint(source: string): string {
  switch (source) {
    case "project":
      return "来自项目 .latte/tools.d/，优先级高于全局文件；模型新建 session 时读的就是这一份。";
    case "global":
      return "来自全局 $LATTE_HOME/tools.d/（默认 ~/.latte/tools.d/）；没有项目覆盖时使用。";
    case "none":
      return "该工具当前没有项目或全局文档。";
    default:
      return "";
  }
}

// 向后兼容别名：main.ts 仍引用 mountToolsPanel
export const mountToolsPanel = mountToolsPage;