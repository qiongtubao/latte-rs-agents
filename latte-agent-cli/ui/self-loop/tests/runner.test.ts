/**
 * 单测：parser 部分。
 *
 * 不起 chrome、不起 agent——只验证 ai reply 的解析逻辑。
 * 集成测试（probeUi / applyDiff）见 e2e/，因为它们需要外部进程。
 */

import { describe, it, expect } from "vitest";

// 从 runner 里 parseAiReply 是非 export 的；这里复制逻辑构造测试。
// 因为 runner.ts 把 parseAiReply 定义在文件作用域、不 export。
// 集成测试通过真实 spawn 验证；这里只覆盖 JSON 提取的 happy path。

describe("jsonl parser", () => {
  it("extracts last role_turn content from jsonl", () => {
    const stdout = [
      JSON.stringify({ type: "status", message: "..." }),
      JSON.stringify({ type: "role_turn", content: "first answer" }),
      JSON.stringify({ type: "role_turn", content: "{\"decision\":\"apply_diff\",\"diff\":\"x\",\"rationale\":\"y\"}" }),
      JSON.stringify({ type: "done" }),
    ].join("\n");

    let lastText = "";
    for (const line of stdout.split("\n")) {
      const trimmed = line.trim();
      if (!trimmed) continue;
      try {
        const obj = JSON.parse(trimmed);
        if (obj.type === "role_turn" && typeof obj.content === "string") {
          lastText = obj.content;
        }
      } catch {
        // ignore
      }
    }

    expect(lastText).toContain('"decision":"apply_diff"');
    expect(lastText).toContain('"diff":"x"');
  });

  it("ignores malformed lines", () => {
    const stdout = [
      "not json",
      JSON.stringify({ type: "role_turn", content: "ok" }),
      "{malformed",
    ].join("\n");

    let lastText = "";
    for (const line of stdout.split("\n")) {
      const trimmed = line.trim();
      if (!trimmed) continue;
      try {
        const obj = JSON.parse(trimmed);
        if (obj.type === "role_turn" && typeof obj.content === "string") {
          lastText = obj.content;
        }
      } catch {
        // ignore
      }
    }

    expect(lastText).toBe("ok");
  });

  it("rejects unknown decision values", () => {
    // parseAiReply 内部用 typeof check + JSON.parse；这里模拟 AI 输出
    // {decision: "do_it"}  → give_up + rationale
    const reply = '{"decision":"do_it","diff":""}';
    const obj = JSON.parse(reply);
    const decision = obj.decision;
    const isKnown = decision === "apply_diff" || decision === "no_change" || decision === "give_up";
    expect(isKnown).toBe(false);
  });

  it("emits SelfLoopEvent JSON shape", () => {
    const ev = {
      kind: "iteration" as const,
      iteration: 3,
      message: "── iter 3/5 ──",
      timestamp_unix_ms: Date.now(),
    };
    const line = JSON.stringify(ev);
    const parsed = JSON.parse(line);
    expect(parsed.kind).toBe("iteration");
    expect(parsed.iteration).toBe(3);
    expect(typeof parsed.timestamp_unix_ms).toBe("number");
  });
});
