import { describe, it, expect } from "vitest";
import { extractCodeRefs, formatRef } from "./linkify";

describe("extractCodeRefs — args", () => {
  it("parses file_path + offset from full JSON args", () => {
    const refs = extractCodeRefs(
      "Read",
      '{"file_path": "src/main.rs", "offset": 42, "limit": 10}',
    );
    expect(refs).toEqual([{ path: "src/main.rs", startLine: 42 }]);
  });

  it("parses path + line/start_line/end_line fields", () => {
    expect(extractCodeRefs("Edit", '{"path": "a/b.ts", "line": 7}')).toEqual([
      { path: "a/b.ts", startLine: 7 },
    ]);
    expect(
      extractCodeRefs("Edit", '{"file_path": "a/b.ts", "start_line": 3, "end_line": 9}'),
    ).toEqual([{ path: "a/b.ts", startLine: 3, endLine: 9 }]);
  });

  it("falls back to regex on truncated JSON (args cut at 1200 chars)", () => {
    const truncated =
      '{"file_path": "src/lib/foo.rs", "offset": 120, "content": "aaaaaaaa';
    const refs = extractCodeRefs("Write", truncated);
    expect(refs).toEqual([{ path: "src/lib/foo.rs", startLine: 120 }]);
  });

  it("extracts multiple path keys from truncated JSON", () => {
    const refs = extractCodeRefs("Move", '{"file_path": "a.ts", "path": "b.ts", "content":');
    expect(refs.map((r) => r.path)).toEqual(["a.ts", "b.ts"]);
  });

  it("returns [] for args without paths", () => {
    expect(extractCodeRefs("Bash", '{"command": "ls -la"}')).toEqual([]);
    expect(extractCodeRefs("Bash", "")).toEqual([]);
  });
});

describe("extractCodeRefs — result text", () => {
  it("extracts grep-style path:line:col and path:line", () => {
    const result =
      "src/foo.rs:120:3:     fn bar() {\nsrc/bar.ts:45:  const x = 1;";
    const refs = extractCodeRefs("Grep", "{}", result);
    expect(refs).toEqual([
      { path: "src/foo.rs", startLine: 120, column: 3 },
      { path: "src/bar.ts", startLine: 45 },
    ]);
  });

  it("ignores URLs and pure numbers", () => {
    const result = "see http://example.com/a.ts:10 and port 8080: failed";
    expect(extractCodeRefs("Bash", "{}", result)).toEqual([]);
  });

  it("dedupes identical refs", () => {
    const result = "src/foo.rs:1\nsrc/foo.rs:1\nsrc/foo.rs:2";
    expect(extractCodeRefs("Grep", "{}", result)).toEqual([
      { path: "src/foo.rs", startLine: 1 },
      { path: "src/foo.rs", startLine: 2 },
    ]);
  });

  it("merges args + result refs without duplicates", () => {
    const refs = extractCodeRefs(
      "Grep",
      '{"path": "src/foo.rs"}',
      "src/foo.rs:10: match",
    );
    expect(refs).toEqual([
      { path: "src/foo.rs" },
      { path: "src/foo.rs", startLine: 10 },
    ]);
  });

  it("caps the number of refs", () => {
    const result = Array.from({ length: 50 }, (_, i) => `src/f${i}.rs:${i + 1}`).join("\n");
    const refs = extractCodeRefs("Grep", "{}", result);
    expect(refs.length).toBe(20);
  });
});

describe("formatRef", () => {
  it("formats path only", () => {
    expect(formatRef({ path: "src/foo.rs" })).toBe("src/foo.rs");
  });
  it("formats path:line", () => {
    expect(formatRef({ path: "src/foo.rs", startLine: 120 })).toBe("src/foo.rs:120");
  });
  it("formats path:start-end", () => {
    expect(formatRef({ path: "src/foo.rs", startLine: 120, endLine: 160 })).toBe(
      "src/foo.rs:120-160",
    );
  });
  it("does not duplicate start when end == start", () => {
    expect(formatRef({ path: "src/foo.rs", startLine: 5, endLine: 5 })).toBe("src/foo.rs:5");
  });
});
