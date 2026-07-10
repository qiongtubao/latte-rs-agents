// self-loop 面板：触发 AI 自调试，把进度事件流渲染到 DOM。
//
// AI self-debug loop 工作流：
//   1. 用户在 self-loop 面板输入 task（如"make the chat input autoresize"）。
//   2. 前端 POST /api/self-loop/start → 后端 spawn node 进程跑
//      self-loop/runner.ts。
//   3. runner.ts 用 playwright + chrome --headless 打开前端，
//      截图、读 console、读 trace，自主决定修改代码、跑 vite build /
//      playwright test，重启 vite，再次截图验证，循环 N 轮。
//   4. 整个过程通过 SSE 把进度（"iteration 3: read console, found X"）
//      和截图（base64 PNG）流回前端。
//   5. self-debug 完成后，runner 把最终 diff 写到 .latte/self-loop/。
//
// 关键不变量：
//   - 自调试过程中不允许人类介入，除非点"Stop"。
//   - self-loop 始终在前端之外跑（独立 node 进程），不会因为页面
//     刷新而中断。

import {
  SelfLoopEvent,
  startSelfLoop,
  stopSelfLoop,
  subscribeSelfLoop,
} from "./api";

interface UIBinding {
  panelEl: HTMLElement;
  openBtn: HTMLButtonElement;
  closeBtn: HTMLButtonElement;
  formEl: HTMLFormElement;
  taskInputEl: HTMLInputElement;
  maxInputEl: HTMLInputElement;
  progressEl: HTMLElement;
  screenshotsEl: HTMLElement;
  layoutEl: HTMLElement;
}

export interface SelfLoopController {
  isOpen(): boolean;
  open(): void;
}

export function mountSelfLoop(opts: { container: UIBinding }): SelfLoopController {
  const { container } = opts;
  let unsubscribe: (() => void) | null = null;

  container.openBtn.addEventListener("click", () => {
    container.panelEl.classList.remove("hidden");
    container.layoutEl.classList.add("with-self-loop");
  });
  container.closeBtn.addEventListener("click", () => {
    container.panelEl.classList.add("hidden");
    container.layoutEl.classList.remove("with-self-loop");
  });

  container.formEl.addEventListener("submit", async (e) => {
    e.preventDefault();
    const task = container.taskInputEl.value.trim();
    const max = parseInt(container.maxInputEl.value, 10) || 5;
    if (!task) return;
    container.progressEl.innerHTML = "";
    container.screenshotsEl.innerHTML = "";
    pushEvent({ kind: "iteration", iteration: 0, message: `▶ starting: ${task} (max ${max})`, timestamp_unix_ms: Date.now() });
    await startSelfLoop(task, max);
    if (!unsubscribe) {
      unsubscribe = subscribeSelfLoop((ev) => pushEvent(ev));
    }
  });

  function pushEvent(ev: SelfLoopEvent): void {
    // 进度条目
    const div = document.createElement("div");
    div.className = `iter ${ev.kind}`;
    div.dataset.selfLoopKind = ev.kind;
    div.dataset.iteration = String(ev.iteration);
    const head = document.createElement("div");
    head.className = "iter-head";
    head.textContent = `[${ev.kind}] iter ${ev.iteration}`;
    div.appendChild(head);
    const body = document.createElement("div");
    body.className = "iter-body";
    body.textContent = ev.message;
    div.appendChild(body);
    container.progressEl.appendChild(div);
    container.progressEl.scrollTop = container.progressEl.scrollHeight;

    // 截图
    if (ev.kind === "screenshot" && ev.screenshot) {
      const img = document.createElement("img");
      img.src = `data:image/png;base64,${ev.screenshot}`;
      img.alt = `iter ${ev.iteration} screenshot`;
      img.title = `iter ${ev.iteration}: ${ev.message}`;
      container.screenshotsEl.appendChild(img);
    }
  }

  return {
    isOpen: () => !container.panelEl.classList.contains("hidden"),
    open: () => container.openBtn.click(),
  };
}
