// Linkifier (design doc §5.2): extract code references from tool call
// args / results and render them as clickable chips.
//
// ToolUse args are JSON but may be truncated at 1200 chars
// (controller.rs:58), so parsing falls back to regex. ToolResult text is
// scanned for grep-style `path:line[:col]` hits. The same code runs in
// CLI browser mode (chips copy the path) and inside the editor host
// (chips jump via `host.openLocation`).

import type { CodeRef } from "./host";
import { getHost } from "./host";

/** Max chips rendered per bubble — grep output can mention hundreds of
 * paths; beyond this the chips would drown the message. */
const MAX_REFS = 20;

/** `"file_path": "..."` / `"path": "..."` fallback extraction from
 * truncated JSON. */
const PATH_KEY_RE = /"(?:file_path|path)"\s*:\s*"([^"]+)"/g;
const LINE_KEY_RE = /"(?:offset|line|start_line)"\s*:\s*(\d+)/;
const END_LINE_KEY_RE = /"end_line"\s*:\s*(\d+)/;

/** grep-style `path:line[:col]`; path must carry an extension and start
 * after whitespace / quote / bracket / `=` / `,` so URLs and times like
 * `12:30` don't match. */
const GREP_RE = /(?:^|[\s"'`(\[{=,])([\w@./+-]+\.[A-Za-z0-9]+):(\d+)(?::(\d+))?/g;

function num(v: unknown): number | undefined {
  return typeof v === "number" && Number.isFinite(v) ? v : undefined;
}

/** True for strings that clearly aren't workspace paths (URLs, pure
 * numbers/dotted numbers, empty). */
function isNonPath(p: string): boolean {
  if (!p) return true;
  if (p.includes("://")) return true;
  if (/^[\d.]+$/.test(p)) return true;
  return false;
}

function pushRef(refs: CodeRef[], ref: CodeRef): void {
  if (isNonPath(ref.path)) return;
  refs.push(ref);
}

function dedupe(refs: CodeRef[]): CodeRef[] {
  const seen = new Set<string>();
  const out: CodeRef[] = [];
  for (const r of refs) {
    const key = `${r.path}:${r.startLine ?? ""}:${r.endLine ?? ""}:${r.column ?? ""}`;
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(r);
    if (out.length >= MAX_REFS) break;
  }
  return out;
}

/** Extract code references from a tool call. `argsText` is the (possibly
 * truncated) JSON args string; `resultText` is the raw tool output. */
export function extractCodeRefs(
  _toolName: string,
  argsText: string,
  resultText?: string,
): CodeRef[] {
  const refs: CodeRef[] = [];

  // ── args: JSON first, regex fallback for truncated payloads ──
  let parsed: unknown;
  try {
    parsed = JSON.parse(argsText);
  } catch {
    parsed = undefined;
  }
  if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
    const obj = parsed as Record<string, unknown>;
    const path =
      typeof obj.file_path === "string" ? obj.file_path
        : typeof obj.path === "string" ? obj.path
          : undefined;
    if (path) {
      const ref: CodeRef = { path };
      const line = num(obj.offset) ?? num(obj.line) ?? num(obj.start_line);
      if (line !== undefined) ref.startLine = line;
      const end = num(obj.end_line);
      if (end !== undefined) ref.endLine = end;
      if (typeof obj.symbol === "string") ref.symbol = obj.symbol;
      pushRef(refs, ref);
    }
  } else if (argsText) {
    for (const m of argsText.matchAll(PATH_KEY_RE)) {
      pushRef(refs, { path: m[1] });
    }
    if (refs.length > 0) {
      const line = LINE_KEY_RE.exec(argsText);
      if (line) refs[refs.length - 1].startLine = Number(line[1]);
      const end = END_LINE_KEY_RE.exec(argsText);
      if (end) refs[refs.length - 1].endLine = Number(end[1]);
    }
  }

  // ── result: grep-style path:line[:col] ──
  if (resultText) {
    for (const m of resultText.matchAll(GREP_RE)) {
      const ref: CodeRef = { path: m[1], startLine: Number(m[2]) };
      if (m[3]) ref.column = Number(m[3]);
      pushRef(refs, ref);
    }
  }

  return dedupe(refs);
}

/** Display form: `src/foo.rs:120` / `src/foo.rs:120-160`. */
export function formatRef(ref: CodeRef): string {
  let s = ref.path;
  if (ref.startLine !== undefined) {
    s += `:${ref.startLine}`;
    if (ref.endLine !== undefined && ref.endLine !== ref.startLine) {
      s += `-${ref.endLine}`;
    }
  }
  return s;
}

/** Render refs as clickable chips. Click → `host.openLocation` when a
 * host provides it, otherwise copy the path to the clipboard (CLI
 * fallback). A small `◎` graph button appears next to each chip only
 * when the host implements `revealInGraph`. Returns null when there is
 * nothing to render. */
export function makeRefChips(refs: CodeRef[]): HTMLElement | null {
  if (refs.length === 0) return null;
  const wrap = document.createElement("div");
  wrap.className = "code-ref-chips";
  for (const ref of refs) {
    const chip = document.createElement("span");
    chip.className = "code-ref";
    chip.textContent = `📄 ${formatRef(ref)}`;
    chip.title = getHost()?.openLocation
      ? `${formatRef(ref)} — 点击在编辑器中打开`
      : `${formatRef(ref)} — 点击复制路径`;
    chip.addEventListener("click", (e) => {
      e.stopPropagation();
      const host = getHost();
      if (host?.openLocation) {
        host.openLocation(ref);
      } else if (typeof navigator !== "undefined" && navigator.clipboard) {
        navigator.clipboard.writeText(ref.path).catch(() => {});
      }
    });
    wrap.appendChild(chip);

    const host = getHost();
    if (host?.revealInGraph) {
      const graphBtn = document.createElement("span");
      graphBtn.className = "code-ref-graph";
      graphBtn.textContent = "◎";
      graphBtn.title = "在图谱中显示";
      graphBtn.addEventListener("click", (e) => {
        e.stopPropagation();
        getHost()?.revealInGraph?.(ref);
      });
      wrap.appendChild(graphBtn);
    }
  }
  return wrap;
}
