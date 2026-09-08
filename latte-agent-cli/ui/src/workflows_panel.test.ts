// 「导入任务看板」的 transcript 扫描测试：run 成功结束后，前端在纯文本
// transcript 里找 ```json 围栏块，第一个能解析出 {tasks:[{title,...}]}
// 的块作为可导入任务列表。
import { describe, it, expect } from "vitest";
import { extractImportableTasks } from "./workflows_panel";

const VALID_BLOCK = `
一些说明文字
\`\`\`json
{"tasks": [{"title": "修复登录页", "priority": 2}, {"title": "补测试", "subtasks": [{"title": "子任务"}]}]}
\`\`\`
`;

describe("extractImportableTasks（试运行 transcript → 可导入任务）", () => {
  it("能解析出带 title 的 tasks 数组", () => {
    const tasks = extractImportableTasks(VALID_BLOCK);
    expect(tasks).toHaveLength(2);
    expect(tasks![0].title).toBe("修复登录页");
    expect(tasks![1].subtasks?.[0].title).toBe("子任务");
  });

  it("没有 json 围栏块 → null", () => {
    expect(extractImportableTasks("只是普通文本 {tasks: []}")).toBeNull();
  });

  it("跳过坏 JSON，取后面第一个合法块", () => {
    const text = "```json\n{oops}\n```\n" + VALID_BLOCK;
    const tasks = extractImportableTasks(text);
    expect(tasks).toHaveLength(2);
  });

  it("tasks 不是数组 / 为空 / 项缺 title → null", () => {
    expect(extractImportableTasks('```json\n{"tasks": "x"}\n```')).toBeNull();
    expect(extractImportableTasks('```json\n{"tasks": []}\n```')).toBeNull();
    expect(
      extractImportableTasks('```json\n{"tasks": [{"title": "ok"}, {"description": "无 title"}]}\n```'),
    ).toBeNull();
    expect(
      extractImportableTasks('```json\n{"tasks": [{"title": "  "}]}\n```'),
    ).toBeNull();
  });

  it("普通 ``` 块（无 json 语言标记）不匹配", () => {
    expect(
      extractImportableTasks('```\n{"tasks": [{"title": "x"}]}\n```'),
    ).toBeNull();
  });

  it("剥掉 <think> 块里的假 fence，取后面的真 JSON 块", () => {
    const text =
      '<think>指令要求输出 ```json\n{不是真块}\n``` 这样的格式，让我想想…</think>' +
      VALID_BLOCK;
    const tasks = extractImportableTasks(text);
    expect(tasks).toHaveLength(2);
    expect(tasks![0].title).toBe("修复登录页");
  });
});

// ─── 只读门禁摘要：dataset 往返（纯函数，DOM 部分见 .test.tsx） ───
import { parseGatesDataset } from "./workflows_panel";

describe("parseGatesDataset（重渲染后门禁不丢）", () => {
  it("正常往返", () => {
    const gates = ["产出至少 300 字符", "禁止出现：TBD / 待补充"];
    expect(parseGatesDataset(JSON.stringify(gates))).toEqual(gates);
  });

  it("空 / undefined / 坏 JSON / 非数组 → 空列表，不抛异常", () => {
    expect(parseGatesDataset(undefined)).toEqual([]);
    expect(parseGatesDataset("")).toEqual([]);
    expect(parseGatesDataset("{不是 json")).toEqual([]);
    expect(parseGatesDataset('{"a":1}')).toEqual([]);
    expect(parseGatesDataset("null")).toEqual([]);
  });

  it("数组里的非字符串项被过滤掉", () => {
    expect(parseGatesDataset('["ok", 1, null, {"a":1}, "yes"]')).toEqual(["ok", "yes"]);
  });
});
