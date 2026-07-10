/**
 * AI self-debug loop runner.
 *
 * 这个文件被 `latte-agent ui` spawn 出来，承担"无人工介入"修改前端的核心工作：
 *
 *   1. 用 chrome --headless 打开 UI（截图 + 收 console + 抽 DOM）
 *   2. 读 ~/.latte/traces/ 最新 jsonl 看后端事件流
 *   3. 调 latte-agent CLI（manager 角色，多模态）拿到修改 diff
 *   4. apply diff 到 src/，重跑 tsc + 重新截图
 *   5. 重复 N 轮
 *
 * 每步往 stdout 写一行 JSON `SelfLoopEvent`，被 Rust 后端 SSE 转给前端。
 *
 * 设计上故意保持简单：
 *   - 不维护内部状态机
 *   - 不依赖 DB / 网络
 *   - 每个 step 失败立刻 emit `error` 然后退出（exit code != 0）
 *   - AI 决策通过 child_process 调 `latte-agent chat` (one-shot stdin mode)
 *
 * 单测：见 tests/runner.test.ts
 */

import { chromium, ConsoleMessage } from "playwright";
import * as fs from "node:fs/promises";
import * as path from "node:path";
import { execFile, spawn } from "node:child_process";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

/**
 * Find a chromium executable:
 *   1) LATTE_AGENT_CHROME env var
 *   2) /usr/bin/google-chrome (system Chrome on Linux)
 *   3) /usr/bin/chromium-browser / chromium (Debian/Ubuntu)
 *   4) undefined → fall back to playwright's bundled chromium (requires
 *      `playwright install chromium`).
 */
import * as fsSync from "node:fs";

function resolveChromePath(): string | undefined {
  const env = process.env.LATTE_AGENT_CHROME;
  if (env && env.length > 0) return env;
  for (const p of ["/usr/bin/google-chrome", "/usr/bin/chromium-browser", "/usr/bin/chromium"]) {
    if (fsSync.existsSync(p)) return p;
  }
  return undefined;
}

const execFileAsync = promisify(execFile);

// ─── SelfLoopEvent JSON output ──────────────────────────────────

interface SelfLoopEvent {
  kind: "started" | "iteration" | "log" | "screenshot" | "done" | "error";
  iteration: number;
  message: string;
  screenshot?: string;
  data?: unknown;
  timestamp_unix_ms: number;
}

function emit(ev: Omit<SelfLoopEvent, "timestamp_unix_ms">): void {
  const full: SelfLoopEvent = { ...ev, timestamp_unix_ms: Date.now() };
  // 一行一个 JSON，stderr 是给人看的 log。
  process.stdout.write(JSON.stringify(full) + "\n");
}

function log(message: string): void {
  process.stderr.write(`[self-loop] ${message}\n`);
}

// ─── CLI args ───────────────────────────────────────────────────

interface TaskArgs {
  task: string;
  max_iterations: number;
  ui_base_url: string;
}

function parseArgs(): TaskArgs {
  const argv = process.argv.slice(2);
  const idx = argv.indexOf("--task-json");
  if (idx < 0 || idx + 1 >= argv.length) {
    throw new Error("missing --task-json <json>");
  }
  const parsed = JSON.parse(argv[idx + 1]) as Partial<TaskArgs>;
  if (!parsed.task) throw new Error("--task-json.task is required");
  return {
    task: parsed.task,
    max_iterations: parsed.max_iterations ?? 5,
    ui_base_url: parsed.ui_base_url ?? "http://localhost:4567",
  };
}

// ─── DOM probe ──────────────────────────────────────────────────

interface DomSnapshot {
  url: string;
  title: string;
  bodyText: string;
  inputCount: number;
  messageCount: number;
  rolePill: string | null;
  statusPill: string | null;
  consoleErrors: string[];
  consoleWarnings: string[];
  pageErrors: string[];
}

async function probeUi(uiBaseUrl: string, outScreenshotPath?: string): Promise<DomSnapshot> {
  const browser = await chromium.launch({
    headless: true,
    executablePath: resolveChromePath(),
    args: ["--no-sandbox", "--disable-dev-shm-usage"],
  });
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();
  const consoleErrors: string[] = [];
  const consoleWarnings: string[] = [];
  const pageErrors: string[] = [];
  page.on("console", (msg: ConsoleMessage) => {
    const text = msg.text();
    if (msg.type() === "error") consoleErrors.push(text);
    else if (msg.type() === "warning") consoleWarnings.push(text);
  });
  page.on("pageerror", (err) => {
    pageErrors.push(err.message);
  });

  await page.goto(uiBaseUrl, { waitUntil: "load", timeout: 15_000 });
  await page.waitForSelector(".messages", { timeout: 5_000 }).catch(() => {});
  // 等首个 ChatEvent 来（如果有）或 1 秒。
  await page.waitForTimeout(1_000);

  const snap = await page.evaluate((): Omit<DomSnapshot, "consoleErrors" | "consoleWarnings" | "pageErrors"> => {
    const bodyText = (document.body.innerText ?? "").slice(0, 4000);
    const inputCount = document.querySelectorAll("input, textarea").length;
    const messageCount = document.querySelectorAll(".message").length;
    const rolePill = document.querySelector("#role-pill")?.textContent ?? null;
    const statusPill = document.querySelector("#status-pill")?.textContent ?? null;
    return {
      url: location.href,
      title: document.title,
      bodyText,
      inputCount,
      messageCount,
      rolePill,
      statusPill,
    };
  });

  let screenshotB64: string | undefined;
  if (outScreenshotPath) {
    const buf = await page.screenshot({ fullPage: false });
    await fs.writeFile(outScreenshotPath, buf);
    screenshotB64 = buf.toString("base64");
  } else {
    const buf = await page.screenshot({ fullPage: false });
    screenshotB64 = buf.toString("base64");
  }

  await browser.close();

  return {
    ...snap,
    consoleErrors,
    consoleWarnings,
    pageErrors,
    // (screenshotB64 is returned via separate emit; we don't pack it into DomSnapshot to keep JSON small)
  };
}

// ─── AI step: call latte-agent chat ─────────────────────────────

interface AiStepResult {
  decision: "apply_diff" | "no_change" | "give_up";
  diff: string;
  rationale: string;
}

async function aiDecide(opts: {
  task: string;
  iteration: number;
  snapshot: DomSnapshot;
  recentTrace: string;
  srcDir: string;
}): Promise<AiStepResult> {
  // Build a focused prompt asking the model to either produce a unified diff
  // for files under srcDir/, or say "no change needed" / "give up".
  //
  // We invoke `latte-agent chat` in non-tty mode so it prints one reply and
  // exits. The reply should be a JSON object matching AiStepResult.
  const prompt = buildAiPrompt(opts);

  // 写入 prompt 到临时文件，避免命令行长度限制。
  const tmpPrompt = path.join(opts.srcDir, "..", ".self-loop-prompt.md");
  await fs.writeFile(tmpPrompt, prompt);

  // Find latte-agent binary（PATH 里有，或用 ../target/debug/latte-agent）
  const bin = await resolveLatteAgentBinary();

  const actualCwd = path.resolve(opts.srcDir, "..", "..", "..");
  log(`aiDecide iter=${opts.iteration}: calling ${bin} (prompt ${prompt.length}B)`);
  log(`  actualCwd=${actualCwd}`);
  log(`  bin=${bin}`);
  log(`  exists(bin)=${fsSync.existsSync(bin)}`);
  log(`  exists(actualCwd)=${fsSync.existsSync(actualCwd)}`);
  log(`  exists(actualCwd/.latte)=${fsSync.existsSync(path.join(actualCwd, ".latte"))}`);
  try {
    const stdout = await runProcessWithStdin(
      bin,
      [
        "chat",
        "-r", "manager",
        "-t", "budget",
        "--no-session-index",
        "--output", "json",
      ],
      prompt,
      120_000,
      path.resolve(opts.srcDir, "..", "..", ".."),       // workspace root
    );
    return parseAiReply(stdout);
  } catch (err) {
    const msg = (err as Error).message;
    log(`aiDecide failed: ${msg}`);
    return {
      decision: "give_up",
      diff: "",
      rationale: `AI call failed: ${msg.slice(0, 400)}`,
    };
  } finally {
    await fs.unlink(tmpPrompt).catch(() => {});
  }
}

function buildAiPrompt(opts: {
  task: string;
  iteration: number;
  snapshot: DomSnapshot;
  recentTrace: string;
  srcDir: string;
}): string {
  const { snapshot, recentTrace } = opts;
  // latte-agent chat non-tty 模式只读 stdin 第一行 (chat.rs read_line)。
  // 所以 task 必须放在第一行，meta context 放在后面（虽然会被丢弃，
  // 但写出来方便 logging / debug）。
  // 真正可靠的修复是改 chat.rs 支持 heredoc；这里先用 first-line = task。
  const firstLine = "TASK: " + opts.task;
  const ctxLines: string[] = [
    "",
    "## Iteration",
    String(opts.iteration) + " (max " + String(opts.iteration + 5) + ")",
    "",
    "## Current UI state (from chrome --headless probe)",
    "- URL: " + snapshot.url,
    "- Title: " + snapshot.title,
    "- rolePill: " + String(snapshot.rolePill),
    "- statusPill: " + String(snapshot.statusPill),
    "- inputCount: " + String(snapshot.inputCount),
    "- messageCount: " + String(snapshot.messageCount),
    "- console errors: " + JSON.stringify(snapshot.consoleErrors.slice(0, 10)),
    "- console warnings: " + JSON.stringify(snapshot.consoleWarnings.slice(0, 5)),
    "- page errors: " + JSON.stringify(snapshot.pageErrors.slice(0, 5)),
    "- body text (truncated):",
    "```",
    snapshot.bodyText.slice(0, 1500),
    "```",
    "",
    "## Recent trace (last ~30 events)",
    "```",
    recentTrace.slice(0, 3000),
    "```",
    "",
    "## Source files you can edit",
    "All .ts / .css / .html files under " + opts.srcDir + " (relative to repo root).",
    "",
    "## Output format",
    'Respond with a single JSON object (no prose, no code fence): {"decision":"apply_diff","diff":"...","rationale":"..."}',
    "",
    "Or use:",
    '- {"decision":"no_change","diff":"","rationale":"UI is already correct"} when task is satisfied.',
    '- {"decision":"give_up","diff":"","rationale":"..."} when stuck after 3+ attempts.',
    "",
    "Constraints:",
    "- Only edit files under " + opts.srcDir + ". Never modify Rust code.",
    "- Keep diffs minimal.",
    "- Do NOT introduce new npm deps.",
    "- After editing, the test suite must still pass (we will re-run tsc + screenshot).",
  ];
  return firstLine + "\n" + ctxLines.join("\n");
}
function parseAiReply(stdout: string): AiStepResult {
  // latte-agent chat --output json emits JSONL events; last "Done" or any
  // message with role_turn content is the final assistant text.
  let lastText = "";
  for (const line of stdout.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    try {
      const obj = JSON.parse(trimmed) as { type?: string; content?: string };
      if (obj.type === "role_turn" && typeof obj.content === "string") {
        lastText = obj.content;
      }
    } catch {
      // ignore non-JSON lines
    }
  }
  // 提取第一个 JSON 块。
  const match = lastText.match(/\{[\s\S]*\}/);
  if (!match) {
    return {
      decision: "give_up",
      diff: "",
      rationale: `could not find JSON in AI reply: ${lastText.slice(0, 200)}`,
    };
  }
  try {
    const obj = JSON.parse(match[0]) as Partial<AiStepResult>;
    if (obj.decision !== "apply_diff" && obj.decision !== "no_change" && obj.decision !== "give_up") {
      return { decision: "give_up", diff: "", rationale: `unknown decision: ${String(obj.decision)}` };
    }
    return {
      decision: obj.decision,
      diff: obj.diff ?? "",
      rationale: obj.rationale ?? "",
    };
  } catch (e) {
    return { decision: "give_up", diff: "", rationale: `JSON parse failed: ${(e as Error).message}` };
  }
}

function runProcessWithStdin(
  bin: string,
  args: string[],
  stdinData: string,
  timeoutMs: number,
  cwd: string,
): Promise<string> {
  return new Promise((resolve, reject) => {
    const child = spawn(bin, args, {
      cwd,
      stdio: ["pipe", "pipe", "pipe"],
      env: process.env,
    });
    let stdout = "";
    let stderr = "";
    let killed = false;
    const t = setTimeout(() => {
      killed = true;
      child.kill("SIGTERM");
      reject(new Error(`timeout after ${timeoutMs}ms`));
    }, timeoutMs);
    child.stdout.on("data", (d) => (stdout += d.toString()));
    child.stderr.on("data", (d) => (stderr += d.toString()));
    child.on("error", (err) => {
      clearTimeout(t);
      if (!killed) reject(err);
    });
    child.on("close", (code) => {
      clearTimeout(t);
      if (killed) return;
      if (code === 0) resolve(stdout);
      else reject(new Error(`exit ${code}: ${stderr.slice(0, 800)}`));
    });
    child.stdin.write(stdinData);
    child.stdin.end();
  });
}

async function resolveLatteAgentBinary(): Promise<string> {
  // 优先用 dev build（target/debug/latte-agent），保证用的是当前仓库代码；
  // PATH 里的 latte-agent 可能是 cargo install 的旧版，没有最新 flag。
  const dev = path.resolve(__dirname, "..", "..", "..", "target", "debug", "latte-agent");
  if (fsSync.existsSync(dev)) return dev;
  if (await commandExists("latte-agent")) return "latte-agent";
  throw new Error(
    `latte-agent binary not found at ${dev} or in PATH. Build it with \`cargo build --bin latte-agent\`.`,
  );
}

async function commandExists(cmd: string): Promise<boolean> {
  const PATH = process.env.PATH ?? "";
  for (const dir of PATH.split(path.delimiter)) {
    try {
      await fs.access(path.join(dir, cmd));
      return true;
    } catch {
      // continue
    }
  }
  return false;
}

// ─── Apply diff ─────────────────────────────────────────────────

async function applyDiff(srcDir: string, diff: string): Promise<void> {
  if (!diff.trim()) return;
  // 用 git apply 最稳。srcDir 必须在 git repo 内。
  const proc = spawn("git", ["apply", "--whitespace=fix", "-"], {
    cwd: srcDir,
    stdio: ["pipe", "pipe", "pipe"],
  });
  return new Promise<void>((resolve, reject) => {
    let stderr = "";
    proc.stderr.on("data", (d) => (stderr += d.toString()));
    proc.on("close", (code) => {
      if (code === 0) resolve();
      else reject(new Error(`git apply failed (code ${code}): ${stderr.slice(0, 800)}`));
    });
    proc.stdin.write(diff);
    proc.stdin.end();
  });
}

// ─── Read latest trace ──────────────────────────────────────────

async function readRecentTrace(traceDir: string, maxBytes = 8_000): Promise<string> {
  let entries: string[] = [];
  try {
    entries = await fs.readdir(traceDir);
  } catch {
    return "";
  }
  const jsonls = entries.filter((n) => n.endsWith(".jsonl"));
  if (jsonls.length === 0) return "";
  jsonls.sort();
  const latest = jsonls[jsonls.length - 1];
  const raw = await fs.readFile(path.join(traceDir, latest), "utf8").catch(() => "");
  if (raw.length <= maxBytes) return raw;
  return "..." + raw.slice(-maxBytes);
}

// ─── Main loop ──────────────────────────────────────────────────

const REPO_ROOT = path.resolve(__dirname, "..", "..", "..");
const SRC_DIR = path.join(REPO_ROOT, "latte-agent-cli", "ui", "src");
const TRACE_DIR = path.join(process.env.HOME ?? "/tmp", ".latte", "traces");

async function main(): Promise<void> {
  const args = parseArgs();
  emit({ kind: "started", iteration: 0, message: `task="${args.task}" max_iterations=${args.max_iterations}` });

  let previousErrors: string[] = [];
  for (let iter = 1; iter <= args.max_iterations; iter++) {
    emit({ kind: "iteration", iteration: iter, message: `── iter ${iter}/${args.max_iterations} ──` });

    // 1. probe UI
    let snap: DomSnapshot;
    try {
      snap = await probeUi(args.ui_base_url);
    } catch (e) {
      const msg = (e as Error).message;
      emit({ kind: "error", iteration: iter, message: `probe failed: ${msg.slice(0, 400)}` });
      return;
    }
    const shotDir = path.join(REPO_ROOT, ".latte", "self-loop");
    await fs.mkdir(shotDir, { recursive: true });
    const shotPath = path.join(shotDir, `iter-${iter}.png`);
    // re-screenshot for emit (probeUi already saved it once but the b64 was discarded)
    // — simpler: call probeUi without outScreenshot and emit inline b64
    {
      const browser = await chromium.launch({
        headless: true,
        executablePath: resolveChromePath(),
        args: ["--no-sandbox", "--disable-dev-shm-usage"],
      });
      const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
      try {
        await page.goto(args.ui_base_url, { waitUntil: "load", timeout: 15_000 });
        await page.waitForTimeout(800);
        const buf = await page.screenshot();
        await fs.writeFile(shotPath, buf);
        emit({ kind: "screenshot", iteration: iter, message: shotPath, screenshot: buf.toString("base64") });
      } finally {
        await browser.close();
      }
    }

    // 2. recent trace
    const trace = await readRecentTrace(TRACE_DIR);

    // 3. AI decides
    emit({ kind: "log", iteration: iter, message: "calling AI to decide next change…" });
    const decision = await aiDecide({
      task: args.task,
      iteration: iter,
      snapshot: snap,
      recentTrace: trace,
      srcDir: SRC_DIR,
    });
    emit({ kind: "log", iteration: iter, message: `AI decision: ${decision.decision} — ${decision.rationale.slice(0, 300)}` });

    if (decision.decision === "give_up") {
      emit({ kind: "error", iteration: iter, message: decision.rationale });
      return;
    }
    if (decision.decision === "no_change") {
      // 校验：probe 再来一次，看 console errors 是否消失 / 行为对。
      const newSnap = await probeUi(args.ui_base_url);
      const newErrs = newSnap.consoleErrors.length + newSnap.pageErrors.length;
      const oldErrs = previousErrors.length;
      if (newErrs === 0 && oldErrs > 0) {
        emit({ kind: "done", iteration: iter, message: `task complete: console errors cleared (${oldErrs} → 0)` });
        return;
      }
      if (iter >= args.max_iterations) {
        emit({ kind: "done", iteration: iter, message: `max iterations reached with no_change` });
        return;
      }
      // 没有 diff 也没改善就继续，让 AI 看新截图再决定。
      emit({ kind: "log", iteration: iter, message: "no_change but errors remain; will probe again next iter" });
      previousErrors = [...newSnap.consoleErrors, ...newSnap.pageErrors];
      continue;
    }

    // decision === "apply_diff"
    try {
      await applyDiff(REPO_ROOT, decision.diff);
      emit({ kind: "log", iteration: iter, message: `applied diff (${decision.diff.length}B)` });
    } catch (e) {
      emit({ kind: "error", iteration: iter, message: `apply failed: ${(e as Error).message.slice(0, 400)}` });
      return;
    }

    // 4. tsc check
    try {
      await execFileAsync("npx", ["tsc", "--noEmit", "-p", path.join(REPO_ROOT, "latte-agent-cli", "ui")], {
        timeout: 60_000,
        maxBuffer: 4 * 1024 * 1024,
      });
      emit({ kind: "log", iteration: iter, message: "tsc --noEmit OK" });
    } catch (e) {
      emit({ kind: "error", iteration: iter, message: `tsc failed: ${(e as Error).message.slice(0, 600)}` });
      // 不回滚 — 让下一轮 AI 看 tsc 错误自己改。
      previousErrors = [(e as Error).message.slice(0, 600)];
      continue;
    }

    // 5. probe 一次，errors 减少 = 进步。
    const postSnap = await probeUi(args.ui_base_url);
    const postErrs = postSnap.consoleErrors.length + postSnap.pageErrors.length;
    const prevErrs = snap.consoleErrors.length + snap.pageErrors.length;
    emit({
      kind: "log",
      iteration: iter,
      message: `console errors: ${prevErrs} → ${postErrs}`,
    });
    if (postErrs === 0 && prevErrs > 0) {
      emit({ kind: "done", iteration: iter, message: `task complete: console errors cleared (${prevErrs} → 0)` });
      return;
    }
    previousErrors = [...postSnap.consoleErrors, ...postSnap.pageErrors];
  }

  emit({ kind: "done", iteration: args.max_iterations, message: "exhausted iterations without converge" });
}

main().catch((e) => {
  emit({ kind: "error", iteration: 0, message: (e as Error).message });
  process.exit(1);
});
