// trace 视图：列出 ~/.latte/traces/ 下的 JSONL session，渲染事件时间线。
//
// 这个面板是 self-debug loop 的核心反馈：
//   - 跑前端 → 看 UI 行为
//   - 看 Trace → 看到底发起了什么 ChatEvent / ToolUse
//   - 看 LatteTrace → 看到底哪条 HTTP 错了、哪条 SSE 中断
// AI 自测时根据 TraceEvent 序列决策修改。

import { listTraces, readTrace, TraceSummary } from "./api";

interface UIBinding {
  panelEl: HTMLElement;
  listEl: HTMLElement;
  sessionSelectEl: HTMLSelectElement;
  refreshBtn: HTMLButtonElement;
  toggleBtn: HTMLButtonElement;
  layoutEl: HTMLElement;
}

export interface TraceController {
  refresh(): Promise<void>;
}

export function mountTrace(opts: { container: UIBinding }): TraceController {
  const { container } = opts;
  let selectedSession: string | null = null;

  container.refreshBtn.addEventListener("click", () => {
    void refresh();
  });

  container.sessionSelectEl.addEventListener("change", async () => {
    selectedSession = container.sessionSelectEl.value;
    await loadSelected();
  });

  container.toggleBtn.addEventListener("click", () => {
    container.panelEl.classList.toggle("hidden");
    container.layoutEl.classList.toggle("with-trace");
  });

  async function refresh(): Promise<void> {
    let traces: TraceSummary[] = [];
    try {
      traces = await listTraces();
    } catch (e) {
      container.listEl.innerHTML = `<div class="trace-event"><span class="trace-kind">error</span> failed to list traces: ${escapeHtml(String(e))}</div>`;
      return;
    }
    container.sessionSelectEl.innerHTML = "";
    if (traces.length === 0) {
      const opt = document.createElement("option");
      opt.textContent = "(no traces)";
      opt.disabled = true;
      container.sessionSelectEl.appendChild(opt);
      container.listEl.innerHTML = `<div class="trace-event"><span class="trace-meta">no trace sessions in ~/.latte/traces/ yet — run \`latte-agent chat\` to populate.</span></div>`;
      return;
    }
    for (const t of traces) {
      const opt = document.createElement("option");
      opt.value = t.session_id;
      const stamp = new Date(t.modified_unix * 1000).toLocaleTimeString();
      opt.textContent = `${t.session_id}  (${formatBytes(t.size_bytes)}, ${stamp})`;
      container.sessionSelectEl.appendChild(opt);
    }
    if (selectedSession && traces.some((t) => t.session_id === selectedSession)) {
      container.sessionSelectEl.value = selectedSession;
    } else {
      selectedSession = traces[0].session_id;
      container.sessionSelectEl.value = selectedSession;
    }
    await loadSelected();
  }

  async function loadSelected(): Promise<void> {
    if (!selectedSession) return;
    container.listEl.innerHTML = `<div class="trace-event"><span class="trace-meta">loading ${escapeHtml(selectedSession)}…</span></div>`;
    try {
      const data = await readTrace(selectedSession);
      renderEvents(data.events);
    } catch (e) {
      container.listEl.innerHTML = `<div class="trace-event"><span class="trace-kind">error</span> failed to read trace: ${escapeHtml(String(e))}</div>`;
    }
  }

  function renderEvents(events: unknown[]): void {
    container.listEl.innerHTML = "";
    if (events.length === 0) {
      container.listEl.innerHTML = `<div class="trace-event"><span class="trace-meta">(empty)</span></div>`;
      return;
    }
    for (const ev of events) {
      const obj = ev as { kind?: string; meta?: { ts?: number }; payload?: unknown };
      const kind = obj.kind ?? "?";
      const ts = obj.meta?.ts ? new Date(obj.meta.ts).toLocaleTimeString() : "?";
      const payload = obj.payload;
      const div = document.createElement("div");
      div.className = `trace-event ${kind}`;
      const head = document.createElement("span");
      head.className = "trace-kind";
      head.textContent = kind;
      const tsSpan = document.createElement("span");
      tsSpan.className = "trace-meta";
      tsSpan.textContent = ` ${ts}`;
      div.appendChild(head);
      div.appendChild(tsSpan);
      if (payload !== undefined) {
        const pre = document.createElement("pre");
        pre.style.margin = "0.3rem 0 0 0";
        pre.style.whiteSpace = "pre-wrap";
        pre.textContent = truncate(JSON.stringify(payload, null, 0), 1200);
        div.appendChild(pre);
      }
      container.listEl.appendChild(div);
    }
    container.listEl.scrollTop = container.listEl.scrollHeight;
  }

  return { refresh };
}

function formatBytes(b: number): string {
  if (b < 1024) return `${b}B`;
  if (b < 1024 * 1024) return `${(b / 1024).toFixed(1)}K`;
  return `${(b / 1024 / 1024).toFixed(1)}M`;
}

function escapeHtml(s: string): string {
  return s.replace(/[&<>"']/g, (c) => {
    switch (c) {
      case "&": return "&amp;";
      case "<": return "&lt;";
      case ">": return "&gt;";
      case '"': return "&quot;";
      case "'": return "&#39;";
      default: return c;
    }
  });
}

function truncate(s: string, max: number): string {
  return s.length <= max ? s : s.slice(0, max) + "…";
}
