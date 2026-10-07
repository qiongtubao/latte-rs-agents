import { describe, it, expect } from "vitest";
import { repairMermaidSource } from "./mermaid_repair";

// 用例取自 jemalloc 会话里实测渲染失败的图源（真实浏览器 + mermaid 12 验证过
// 修补后能出图，见 commit 说明）。这里只断言修补的文本形状。
describe("repairMermaidSource", () => {
  it("节点文字含括号/方括号/# 时加引号", () => {
    const out = repairMermaidSource(
      "flowchart TD\n  A[前端调用 resolve_one(choice_id, q)] --> D[arena->bins[ind] 定位]\n  R2[#[ignore] 5 个] --> A",
    )!;
    expect(out).toContain('A["前端调用 resolve_one(choice_id, q)"]');
    expect(out).toContain('D["arena->bins[ind] 定位"]');
    expect(out).toContain('R2["#[ignore] 5 个"]');
  });

  it("已加引号的节点与复合形状不动", () => {
    const src = 'flowchart TD\n  A["f(x)"] --> B[[sub(x)]]\n  C((圆)) --> A[f(y)]';
    const out = repairMermaidSource(src)!;
    expect(out).toContain('A["f(x)"]');
    expect(out).toContain("B[[sub(x)]]");
    expect(out).toContain("C((圆))");
    expect(out).toContain('A["f(y)"]');
  });

  it("连线文字含特殊字符时加引号", () => {
    const out = repairMermaidSource("flowchart TD\n  base -->|base_alloc_impl (src/base.c:485)| ed\n  R -->|@role line| I")!;
    expect(out).toContain('-->|"base_alloc_impl (src/base.c:485)"| ed');
    expect(out).toContain('-->|"@role line"| I');
  });

  it("subgraph 标题不是合法 id 时改成 id[\"标题\"]，合法写法不动", () => {
    const out = repairMermaidSource('graph LR\n  subgraph 面向用户（低频）\n    U\n  end\n  subgraph FAST["入口 (x.h:1)"]\n  end')!;
    expect(out).toContain('subgraph sg1["面向用户（低频）"]');
    expect(out).toContain('subgraph FAST["入口 (x.h:1)"]');
  });

  it("虚线带文字的各种混写", () => {
    const out = repairMermaidSource(
      'flowchart TD\n  H2 -- "bins[5]" .-> P5\n  ED -.-> "容纳" --> BM\n  D1 -.废弃的 "单全局堆" .-> A1',
    )!;
    expect(out).toContain('H2 -. "bins[5]" .-> P5');
    expect(out).toContain('ED -. "容纳" .-> BM');
    expect(out).toContain('D1 -. "废弃的 单全局堆" .-> A1');
  });

  it("标签字符串拼接与字面 \\n", () => {
    const out = repairMermaidSource('flowchart LR\n  F1["size 参数" + "\\n寄存器传入"] --> F2')!;
    expect(out).toContain('F1["size 参数<br/>寄存器传入"]');
  });

  it("两条连线挤在一行时拆开", () => {
    const out = repairMermaidSource("flowchart LR\n  n0 --> n1 n1 --> n2\n  n2 --> n3")!;
    expect(out).toContain("  n0 --> n1\n  n1 --> n2");
  });

  it("缺图类型头且第一行是节点语句时补 flowchart TD", () => {
    expect(repairMermaidSource('t3["sz_pind2sz_tab"] --> t4')!.startsWith("flowchart TD\n")).toBe(true);
  });

  it("不是 flowchart、或第一行是散文时不猜（返回 null）", () => {
    expect(repairMermaidSource("classDiagram\n  class A { +f(x) }")).toBeNull();
    expect(repairMermaidSource("Hmm, this starts with a mermaid block --> x")).toBeNull();
    expect(repairMermaidSource('  "priority": 1,')).toBeNull();
    expect(repairMermaidSource("")).toBeNull();
  });

  it("没有可修的地方返回 null（不必白跑第二次渲染）", () => {
    expect(repairMermaidSource('flowchart TD\n  A["x"] --> B')).toBeNull();
  });
});
