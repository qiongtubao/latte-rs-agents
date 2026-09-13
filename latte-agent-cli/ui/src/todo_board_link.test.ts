// todo_board_link 纯函数测试：看板任务 title × todo 项 content 的
// 「已派发」匹配。策略保守（宁可不亮也不错标）：归一化（小写+去空白）
// 后包含关系，或字符 bigram Dice 重合度 ≥ 0.8；归一化后 < 4 字符不命中。
import { describe, it, expect } from "vitest";
import { titleMatchesTodo, todoLinkedTask } from "./todo_board_link";

describe("titleMatchesTodo：保守相似度匹配", () => {
  it("归一化后包含关系命中（大小写/空白不敏感，双向）", () => {
    expect(titleMatchesTodo("Write Store", "write  store")).toBe(true);
    expect(titleMatchesTodo("实现 store 模块并补测试", "store 模块")).toBe(true);
    expect(titleMatchesTodo("store 模块", "实现 store 模块并补测试")).toBe(true);
  });

  it("高度重合（bigram Dice ≥ 0.8）命中", () => {
    // 仅结尾一个字符不同：dice = 10/12 ≈ 0.833
    expect(titleMatchesTodo("abcdefg", "abcdefx")).toBe(true);
  });

  it("部分重合但低于阈值不命中（宁缺毋滥）", () => {
    // dice = 4/12 ≈ 0.33
    expect(titleMatchesTodo("abcdefg", "abcxeyg")).toBe(false);
    // "写todostore" vs "写store":dice ≈ 0.67,低于 0.8
    expect(titleMatchesTodo("写 todo store", "写 store")).toBe(false);
  });

  it("过短 / 空串一律不命中", () => {
    expect(titleMatchesTodo("abc", "abc")).toBe(false);
    expect(titleMatchesTodo("", "实现 store 模块")).toBe(false);
    expect(titleMatchesTodo("实现 store 模块", "")).toBe(false);
  });
});

describe("todoLinkedTask：从看板任务里找匹配项", () => {
  const tasks = [
    { id: "LAT-1", title: "实现 store 模块" },
    { id: "LAT-2", title: "写文档" },
  ];

  it("命中返回第一个匹配任务", () => {
    expect(todoLinkedTask("实现 store 模块并补测试", tasks)).toEqual(tasks[0]);
  });

  it("无命中返回 null", () => {
    expect(todoLinkedTask("重构 UI 布局", tasks)).toBeNull();
  });
});
