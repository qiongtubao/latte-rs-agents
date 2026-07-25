// 工具管理面板：列出所有可用工具、显示描述、切换启用状态。
//
// 工具分三类（来自后端 `ToolEntry.kind`）：
// - builtin：Rust 实现的 builtin package（read/write/exec/git.* 等）
// - dynamic：controller 动态 register 的工具（delegate/workflow 等）
// - package_alias：包级别别名（git / bash / mcp）
import { listTools, setToolEnabled } from "./api";
import type { ToolEntry } from "./api";

interface UIBinding {
  panelEl: HTMLElement;
  openBtn: HTMLButtonElement;
  closeBtn: HTMLButtonElement;
  refreshBtn: HTMLButtonElement;
  bodyEl: HTMLElement;
  statusEl: HTMLElement;
  filterEl: HTMLInputElement;
}

export interface ToolsPanelController {
  isOpen(): boolean;
  open(): void;
}

export function mountToolsPanel(opts: { container: UIBinding }): ToolsPanelController {
  const { container } = opts;
  let entries: ToolEntry[] = [];
  let isLoading = false;
  let filterText = "";

  container.openBtn.addEventListener("click", () => container.panelEl.classList.remove("hidden"));
  container.closeBtn.addEventListener("click", () => container.panelEl.classList.add("hidden"));
  container.refreshBtn.addEventListener("click", () => void refresh());
  container.filterEl.addEventListener("input", () => {
    filterText = container.filterEl.value.trim().toLowerCase();
    render();
  });

  const setStatus = (msg: string, error = false) => {
    container.statusEl.textContent = msg;
    container.statusEl.classList.toggle("error", error);
  };

  async function refresh(): Promise<void> {
    if (isLoading) return;
    isLoading = true;
    setStatus("加载中…");
    try {
      const resp = await listTools();
      entries = resp;
      setStatus(`已加载 ${entries.length} 个工具`);
      render();
    } catch (e) {
      setStatus(`加载失败: ${(e as Error).message}`, true);
    } finally {
      isLoading = false;
    }
  }

  function render(): void {
    container.bodyEl.replaceChildren();
    const filtered = filterText
      ? entries.filter(t => t.id.toLowerCase().includes(filterText) || (t.description ?? "").toLowerCase().includes(filterText))
      : entries;

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
      heading.textContent = kindLabel(kind);
      groupEl.appendChild(heading);
      const list = document.createElement("div");
      list.className = "tools-list";
      for (const t of items) list.appendChild(renderRow(t));
      groupEl.appendChild(list);
      container.bodyEl.appendChild(groupEl);
    }
    if (filtered.length === 0) {
      const empty = document.createElement("p");
      empty.className = "hint";
      empty.textContent = "没有匹配的工具。";
      container.bodyEl.appendChild(empty);
    }
  }

  function renderRow(t: ToolEntry): HTMLElement {
    const row = document.createElement("div");
    row.className = "tools-row" + (t.enabled ? "" : " tools-row-disabled");

    const main = document.createElement("div");
    main.className = "tools-row-main";

    const header = document.createElement("div");
    header.className = "tools-row-header";

    const idEl = document.createElement("span");
    idEl.className = "tools-row-id";
    idEl.textContent = t.id;
    header.appendChild(idEl);

    if (t.registered_by) {
      const reg = document.createElement("span");
      reg.className = "tools-row-reg";
      reg.textContent = `reg: ${t.registered_by}`;
      reg.title = "controller 动态注册点";
      header.appendChild(reg);
    }

    main.appendChild(header);

    if (t.description) {
      const desc = document.createElement("div");
      desc.className = "tools-row-desc";
      desc.textContent = t.description;
      main.appendChild(desc);
    }

    const toggle = document.createElement("label");
    toggle.className = "tools-row-toggle";
    const cb = document.createElement("input");
    cb.type = "checkbox";
    cb.checked = t.enabled;
    cb.addEventListener("change", () => void onToggle(t, cb.checked));
    const toggleLabel = document.createElement("span");
    toggleLabel.textContent = t.enabled ? "启用" : "禁用";
    cb.addEventListener("change", () => { toggleLabel.textContent = cb.checked ? "启用" : "禁用"; });
    toggle.append(cb, toggleLabel);

    row.append(main, toggle);
    return row;
  }

  async function onToggle(t: ToolEntry, enabled: boolean): Promise<void> {
    setStatus(`${t.id}: ${enabled ? "启用中…" : "禁用中…"}`);
    try {
      await setToolEnabled(t.id, enabled);
      // 更新本地状态（保持 toggle 一致）
      const i = entries.findIndex(x => x.id === t.id);
      if (i >= 0) entries[i] = { ...entries[i], enabled };
      setStatus(`✅ ${t.id} 已${enabled ? "启用" : "禁用"}（新 session 生效）`);
      render();
    } catch (e) {
      setStatus(`切换失败: ${(e as Error).message}`, true);
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

function kindLabel(kind: string): string {
  switch (kind) {
    case "builtin": return "内置工具 (builtin)";
    case "dynamic": return "动态注册 (dynamic)";
    case "package_alias": return "包别名 (alias)";
    default: return kind;
  }
}
