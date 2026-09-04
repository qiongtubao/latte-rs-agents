/**
 * `briefOf` 的规则必须与后端 `split_brief_and_detail` 一致：首行优先，
 * 首行仍过长时按第一个句末标点断句。
 *
 * 背景：工具列表卡片原来直接渲染后端返回的完整注册描述，`read` 那种
 * 200+ 字的说明会把列表撑成文字墙，详情弹层的「简介」栏同样是一大段。
 */
import { describe, it, expect } from "vitest";

import { briefOf } from "./tools_panel";

describe("briefOf", () => {
  it("多行文本取首行", () => {
    expect(briefOf("Searches files: Rust regex.\n\n<instruction>\n- x\n</instruction>"))
      .toBe("Searches files: Rust regex.");
  });

  it("短的单行原样返回", () => {
    expect(briefOf("列出目录内容")).toBe("列出目录内容");
  });

  it("过长单行按第一个句末标点断句", () => {
    const desc =
      "读取文件内容。支持行范围选择器：path:start-end、path:start+count、path:raw；读代码文件且不带行范围时默认回传结构摘要。";
    expect(briefOf(desc)).toBe("读取文件内容。");
  });

  it("英文句点后要求空白，避免在版本号/缩写处断开", () => {
    const long =
      "AST structural code search. 26+ languages supported, use $NAME/$_ for one node and $$$NAME for zero-or-more nodes.";
    expect(briefOf(long)).toBe("AST structural code search.");
    const noBreak = "Runs pi v1.2 scripts in a persistent shell without any sentence terminator at all here";
    expect(briefOf(noBreak)).toBe(noBreak);
  });

  it("空输入返回空串", () => {
    expect(briefOf("")).toBe("");
    expect(briefOf("   \n  ")).toBe("");
  });
});
