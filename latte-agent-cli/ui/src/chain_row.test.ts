// 角色编辑器「模型链」下拉框的单元测试。
//
// 重点测三件事：
//   1. select 用 catalog 数据生成下拉选项（避免用户手敲 model id）；
//   2. catalog 内的 model id → 选项被预选 + custom input 隐藏；
//   3. catalog 外的 model id → 选中「自定义」+ custom input 显示并填好值；
//   4. readChainValues 把 select/custom 状态塌缩成最终要写入 `model_chain` 的字符串。
//
// 用 jsdom 跑（关注 DOM 行为，不绑 UI 状态机）。
// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import {
  buildChainRow,
  CHAIN_ROW_CUSTOM_LABEL,
  CHAIN_ROW_CUSTOM_VALUE,
  CHAIN_ROW_PLACEHOLDER_LABEL,
  readChainValues,
  readRowValue,
  renderChain,
} from "./chain_row";
import type { AvailableModel } from "./api";

const CATALOG: AvailableModel[] = [
  { name: "glm-5.2", provider: "glm", source: "project", key: "glm/glm-5.2" },
  { name: "deepseek-v4-flash", provider: "deepseek", source: "global", key: "deepseek/deepseek-v4-flash" },
  { name: "MiniMax-M3", provider: "anthropic", source: "catalog", key: "anthropic/MiniMax-M3" },
];

function newContainer(): HTMLDivElement {
  return document.createElement("div");
}

describe("buildChainRow", () => {
  it("渲染 catalog 模型 + 占位符 + 自定义三个区段", () => {
    const noop = () => {};
    const row = buildChainRow({
      model: "",
      modelIndex: 0,
      catalog: CATALOG,
      onBrowse: noop,
      onReorder: noop,
      onRemove: noop,
    });
    const select = row.querySelector<HTMLSelectElement>("select.role-editor-chain-select");
    expect(select).not.toBeNull();
    const options = Array.from(select!.options).map((o) => ({
      value: o.value,
      text: o.textContent,
    }));
    // 首项：占位符空值
    expect(options[0]).toEqual({
      value: "",
      text: CHAIN_ROW_PLACEHOLDER_LABEL,
    });
    // catalog 三项
    expect(options[1]).toEqual({
      value: "glm/glm-5.2__project",
      text: "glm/glm-5.2 · [项目]",
    });
    expect(options[2]).toEqual({
      value: "deepseek/deepseek-v4-flash__global",
      text: "deepseek/deepseek-v4-flash · [全局]",
    });
    expect(options[3]).toEqual({
      value: "anthropic/MiniMax-M3__catalog",
      text: "anthropic/MiniMax-M3 · [catalog]",
    });
    // 末项：自定义
    expect(options[4]).toEqual({
      value: CHAIN_ROW_CUSTOM_VALUE,
      text: CHAIN_ROW_CUSTOM_LABEL,
    });
    // 当前 model 是空 → select 选占位符、custom input 隐藏
    expect(select!.value).toBe("");
    const custom = row.querySelector<HTMLInputElement>("input.role-editor-chain-custom")!;
    expect(custom.hidden).toBe(true);
    expect(custom.value).toBe("");
  });

  it("catalog 内的 model id 预选对应项并隐藏 custom input", () => {
    const noop = () => {};
    const row = buildChainRow({
      model: "glm-5.2",
      modelIndex: 0,
      catalog: CATALOG,
      onBrowse: noop,
      onReorder: noop,
      onRemove: noop,
    });
    const select = row.querySelector<HTMLSelectElement>("select.role-editor-chain-select")!;
    const custom = row.querySelector<HTMLInputElement>("input.role-editor-chain-custom")!;
    expect(select.value).toBe("glm/glm-5.2__project");
  });

  it("catalog 外的 model id 选中「自定义」并显示 custom input", () => {
    const noop = () => {};
    const row = buildChainRow({
      model: "future-experimental-model",
      modelIndex: 0,
      catalog: CATALOG,
      onBrowse: noop,
      onReorder: noop,
      onRemove: noop,
    });
    const select = row.querySelector<HTMLSelectElement>("select.role-editor-chain-select")!;
    const custom = row.querySelector<HTMLInputElement>("input.role-editor-chain-custom")!;
    expect(select.value).toBe(CHAIN_ROW_CUSTOM_VALUE);
    expect(custom.hidden).toBe(false);
    expect(custom.value).toBe("future-experimental-model");
  });

  it("select 切到「自定义」时显示输入框并 focus；切回 catalog 时隐藏并清空", () => {
    const noop = () => {};
    const row = buildChainRow({
      model: "glm-5.2",
      modelIndex: 0,
      catalog: CATALOG,
      onBrowse: noop,
      onReorder: noop,
      onRemove: noop,
    });
    const select = row.querySelector<HTMLSelectElement>("select.role-editor-chain-select")!;
    const custom = row.querySelector<HTMLInputElement>("input.role-editor-chain-custom")!;
    const focus = vi.spyOn(custom, "focus");

    // 切到「自定义」→ 显示输入框
    select.value = CHAIN_ROW_CUSTOM_VALUE;
    select.dispatchEvent(new Event("change"));
    expect(custom.hidden).toBe(false);
    expect(focus).toHaveBeenCalled();

    // 切回 catalog → 隐藏并清空输入框
    select.value = "glm/glm-5.2__project";
    select.dispatchEvent(new Event("change"));
    expect(custom.hidden).toBe(true);
    expect(custom.value).toBe("");
  });
});

describe("readRowValue", () => {
  it("select 选 catalog 模型 → 返回 select.value 还原的 model id", () => {
    const noop = () => {};
    const row = buildChainRow({
      model: "glm-5.2",
      modelIndex: 0,
      catalog: CATALOG,
      onBrowse: noop,
      onReorder: noop,
      onRemove: noop,
    });
    expect(readRowValue(row)).toBe("glm-5.2");
  });

  it("select 选「自定义」 → 返回 custom input 的内容（trim）", () => {
    const noop = () => {};
    const row = buildChainRow({
      model: "another-model",
      modelIndex: 0,
      catalog: CATALOG,
      onBrowse: noop,
      onReorder: noop,
      onRemove: noop,
    });
    const custom = row.querySelector<HTMLInputElement>("input.role-editor-chain-custom")!;
    custom.value = "  peter/custom  ";
    expect(readRowValue(row)).toBe("peter/custom");
  });

  it("select 选占位符 → 返回空字符串", () => {
    const noop = () => {};
    const row = buildChainRow({
      model: "",
      modelIndex: 0,
      catalog: CATALOG,
      onBrowse: noop,
      onReorder: noop,
      onRemove: noop,
    });
    expect(readRowValue(row)).toBe("");
  });
});

describe("renderChain", () => {
  it("渲染每一行 + 「+ 添加模型」按钮", () => {
    const container = newContainer();
    const renderNext = vi.fn();
    renderChain({
      container,
      models: ["glm-5.2", "future-model"],
      catalog: CATALOG,
      onBrowse: vi.fn(),
      renderNext,
    });
    const rows = container.querySelectorAll(".role-editor-chain-row");
    expect(rows.length).toBe(2);
    const addBtn = container.querySelector<HTMLButtonElement>(".role-editor-chain-add");
    expect(addBtn).not.toBeNull();
    expect(addBtn!.textContent).toBe("+ 添加模型");
  });

  it("空数组渲染一个空行（让用户能立刻选）", () => {
    const container = newContainer();
    renderChain({
      container,
      models: [],
      catalog: CATALOG,
      onBrowse: vi.fn(),
      renderNext: vi.fn(),
    });
    const rows = container.querySelectorAll(".role-editor-chain-row");
    expect(rows.length).toBe(1);
    const select = rows[0].querySelector<HTMLSelectElement>("select")!;
    expect(select.value).toBe("");
  });

  it("「+ 添加模型」点击 → 调 renderNext（把当前值 + 一个空行）", () => {
    const container = newContainer();
    const renderNext = vi.fn();
    renderChain({
      container,
      models: ["glm-5.2"],
      catalog: CATALOG,
      onBrowse: vi.fn(),
      renderNext,
    });
    const addBtn = container.querySelector<HTMLButtonElement>(".role-editor-chain-add")!;
    addBtn.click();
    expect(renderNext).toHaveBeenCalledWith(["glm-5.2", ""]);
  });
});

describe("readChainValues", () => {
  it("空行占位符会被 filter 掉", () => {
    const container = newContainer();
    renderChain({
      container,
      models: ["", "glm-5.2", ""],
      catalog: CATALOG,
      onBrowse: vi.fn(),
      renderNext: vi.fn(),
    });
    expect(readChainValues(container)).toEqual(["glm-5.2"]);
  });

  it("混合 catalog + 自定义都能正确读出", () => {
    const container = newContainer();
    renderChain({
      container,
      models: ["glm-5.2", "future-model", "deepseek-v4-flash"],
      catalog: CATALOG,
      onBrowse: vi.fn(),
      renderNext: vi.fn(),
    });
    const values = readChainValues(container);
    expect(values).toEqual(["glm-5.2", "future-model", "deepseek-v4-flash"]);
  });
});

describe("↑/↓/× 按钮", () => {
  it("「↑」和「↓」调 renderNext 时传调整顺序后的列表", () => {
    const container = newContainer();
    const renderNext = vi.fn();
    renderChain({
      container,
      models: ["a", "b", "c"],
      catalog: [],
      onBrowse: vi.fn(),
      renderNext,
    });
    const buttons = container.querySelectorAll<HTMLButtonElement>(".role-editor-chain-row button");
    // row 0: order, cell, 🔍, ↑, ↓, ×
    // row 1: ↑ 应该是 buttons[1+3] = 上一行的 ↓
    // 第一个 row 的 ↑ 按钮在 index 3
    const upButtons = container.querySelectorAll<HTMLButtonElement>(".role-editor-chain-row button:nth-child(4)");
    upButtons[1].click(); // 把 b 挪到 a 前面
    expect(renderNext).toHaveBeenLastCalledWith(["b", "a", "c"]);
  });

  it("「×」调 renderNext 时传去掉当前行后的列表", () => {
    const container = newContainer();
    const renderNext = vi.fn();
    renderChain({
      container,
      models: ["a", "b", "c"],
      catalog: [],
      onBrowse: vi.fn(),
      renderNext,
    });
    // 第二个 row 的「×」按钮：querySelectorAll("button") 只数实际 button
    // 元素（cell 里的 select/input 不算），所以 🔍↑↓× 是 0-3。
    const rows = container.querySelectorAll(".role-editor-chain-row");
    const removeBtn = rows[1].querySelectorAll<HTMLButtonElement>("button")[3];
    removeBtn.click();
    expect(renderNext).toHaveBeenLastCalledWith(["a", "c"]);
  });
});
