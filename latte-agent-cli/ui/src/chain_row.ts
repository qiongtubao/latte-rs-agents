// 角色编辑器「模型链」下拉框。
//
// 数据源是后端 `GET /api/roles/config` 返回的 `available_models`
// （合并后的 catalog），每行渲染一个 `<select>` + 隐藏的 `<input>`：
//   - 选 catalog 模型时 → select 直接给 model id，custom input 隐藏；
//   - 选「自定义 ID」时 → 显示 custom input 给用户手敲任何 model id。
//
// 抽成独立模块的目的是让这段 DOM 逻辑能脱离 `mountRoleEditor` 测试
// （不需要完整的 role editor UIBinding / 状态机就拿到 jsdom 跑）。
//
// 写出去的字符串（`readChainValues`）就是最终要写入 `model_chain` 的
// 内容 —— 没新增协议，只是把 UI 从 `<input>` 换成 `<select>`，让用户
// 不用手敲 model id。

import type { AvailableModel } from "./api";

/** select 里「自定义 ID」选项的占位值 —— 真正的 model id 改从旁边的
 * 文本输入框读，避免和任何 catalog model 重名冲突。 */
export const CHAIN_ROW_CUSTOM_VALUE = "__custom__";

/** 模型链下拉框首项的中文标签（占位符）。 */
export const CHAIN_ROW_PLACEHOLDER_LABEL = "— 选择模型 —";

/** 末项「自定义」选项的中文标签。 */
export const CHAIN_ROW_CUSTOM_LABEL = "✏️ 自定义 ID...";

/** 后端 source 字段 → 中文标签。catalog 在 UI 上保留英文（避免歧义）。 */
const CHAIN_ROW_SOURCE_LABEL: Record<string, string> = {
  project: "项目",
  global: "全局",
  catalog: "catalog",
};

/** 渲染一条模型链下拉框行（包含 select + 可选 custom input）。
 * `modelIndex` 是当前行的下标（0-based），用来在 label 上显示顺序。 */
export function buildChainRow(params: {
  model: string;
  modelIndex: number;
  catalog: AvailableModel[];
  onBrowse: (modelId: string) => void;
  onReorder: (next: string[]) => void;
  onRemove: (next: string[]) => void;
}): HTMLDivElement {
  const { model, modelIndex, catalog, onBrowse, onReorder, onRemove } = params;
  const row = document.createElement("div");
  row.className = "role-editor-chain-row";

  const order = document.createElement("span");
  order.className = "role-editor-chain-order";
  order.textContent = `${modelIndex + 1}.`;

  // cell 包住 select + custom input，让两者在同一格内显示（custom 默认隐藏）。
  const cell = document.createElement("div");
  cell.className = "role-editor-chain-cell";

  const select = document.createElement("select");
  select.className = "role-editor-chain-select";

  const placeholder = document.createElement("option");
  placeholder.value = "";
  placeholder.textContent = CHAIN_ROW_PLACEHOLDER_LABEL;
  select.appendChild(placeholder);

  for (const m of catalog) {
    const opt = document.createElement("option");
    opt.value = m.name;
    const sourceLabel = CHAIN_ROW_SOURCE_LABEL[m.source] ?? m.source;
    // 显示成 `provider/name · [源]`，让用户一眼能区分同名模型不同厂商。
    opt.textContent = `${m.provider}/${m.name} · [${sourceLabel}]`;
    select.appendChild(opt);
  }

  const customOpt = document.createElement("option");
  customOpt.value = CHAIN_ROW_CUSTOM_VALUE;
  customOpt.textContent = CHAIN_ROW_CUSTOM_LABEL;
  select.appendChild(customOpt);

  const custom = document.createElement("input");
  custom.type = "text";
  custom.className = "role-editor-chain-custom";
  custom.placeholder = "model-id";
  custom.hidden = true;

  // 初始状态：catalog 内 → 选对应项并隐藏 custom；catalog 外 → 选「自定义」并显示 custom。
  const inCatalog = catalog.some((m) => m.name === model);
  if (model === "") {
    select.value = "";
    custom.value = "";
  } else if (inCatalog) {
    select.value = model;
    custom.value = "";
  } else {
    select.value = CHAIN_ROW_CUSTOM_VALUE;
    custom.value = model;
    custom.hidden = false;
  }

  // 切到「自定义」时显示输入框；切到 catalog 时隐藏并清空。
  select.addEventListener("change", () => {
    custom.hidden = select.value !== CHAIN_ROW_CUSTOM_VALUE;
    if (custom.hidden) custom.value = "";
    else custom.focus();
  });

  cell.append(select, custom);

  const button = (label: string, title: string, action: () => void): HTMLButtonElement => {
    const el = document.createElement("button");
    el.type = "button";
    el.textContent = label;
    el.title = title;
    el.addEventListener("click", action);
    return el;
  };

  row.append(
    order,
    cell,
    button("🔍", "在模型管理中查看此模型", () => {
      const modelId = select.value === CHAIN_ROW_CUSTOM_VALUE
        ? custom.value.trim()
        : select.value.trim();
      if (modelId) onBrowse(modelId);
    }),
    button("↑", "提高优先级", () => onReorder(swapAdjacent(currentValues(row), modelIndex, -1))),
    button("↓", "降低优先级", () => onReorder(swapAdjacent(currentValues(row), modelIndex, +1))),
    button("×", "删除模型", () => onRemove(removeAt(currentValues(row), modelIndex))),
  );
  return row;
}

/** 读出当前 row 的 model id 字符串。select 选 catalog 模型时直接读 select.value；
 * 选「自定义」时读旁边的文本输入框。 */
export function readRowValue(row: HTMLDivElement): string {
  const select = row.querySelector<HTMLSelectElement>("select.role-editor-chain-select");
  const custom = row.querySelector<HTMLInputElement>("input.role-editor-chain-custom");
  if (!select || !custom) return "";
  return select.value === CHAIN_ROW_CUSTOM_VALUE
    ? custom.value.trim()
    : select.value.trim();
}

/** 读出所有 row 的 model id 列表（去掉空值）。 */
export function readChainValues(container: HTMLElement): string[] {
  return Array.from(container.querySelectorAll<HTMLDivElement>(".role-editor-chain-row"))
    .map(readRowValue)
    .filter(Boolean);
}

/** 渲染所有 row + 「+ 添加模型」按钮。 */
export function renderChain(params: {
  container: HTMLElement;
  models: string[];
  catalog: AvailableModel[];
  onBrowse: (modelId: string) => void;
  /** 用户点「↑/↓/×/添加」后回调：传入下一帧的 model id 列表，
   * 外层逻辑（通常是 `renderChain` 自身）据此重新渲染。 */
  renderNext: (models: string[]) => void;
}): void {
  const { container, models, catalog, onBrowse, renderNext } = params;
  container.replaceChildren();
  const values = models.length ? models : [""];
  values.forEach((model, index) => {
    container.appendChild(buildChainRow({
      model,
      modelIndex: index,
      catalog,
      onBrowse,
      onReorder: renderNext,
      onRemove: renderNext,
    }));
  });
  const add = document.createElement("button");
  add.type = "button";
  add.className = "role-editor-chain-add";
  add.textContent = "+ 添加模型";
  add.addEventListener("click", () => renderNext([...readChainValues(container), ""]));
  container.appendChild(add);
}

// ── 内部 helpers（list 行内 reorder/remove 操作） ──────────────────

/** 复制当前所有 row 的 model id 列表。复用 row 自己读出来的状态，
 * 避免外层维护一份同步影子（容易脱节）。 */
function currentValues(currentRow: HTMLDivElement): string[] {
  const container = currentRow.parentElement;
  if (!container) return [];
  return readChainValues(container);
}

/** 在 `values` 里把 `index` 位置和 `index + dir` 位置调换，
 * 边界外（已经在第一/最后一项）返回原数组。 */
function swapAdjacent(values: string[], index: number, dir: number): string[] {
  const j = index + dir;
  if (j < 0 || j >= values.length) return values;
  const next = values.slice();
  [next[index], next[j]] = [next[j], next[index]];
  return next;
}

/** 删除 `index` 位置（越界返回原数组）。 */
function removeAt(values: string[], index: number): string[] {
  if (index < 0 || index >= values.length) return values;
  const next = values.slice();
  next.splice(index, 1);
  return next;
}
