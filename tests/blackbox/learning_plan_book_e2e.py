#!/usr/bin/env python3
"""学习类任务拆分的端到端黑盒验证：拆出来必须是一本书的目录。

用 HTTP + SSE 驱动 `latte-agent ui`（真实后端 + 真实模型），在目标仓库里发：

  /learn-plan 学习 <目标仓库> 源码……

然后拦截 PlanProposed 弹窗事件，断言拆出的任务清单满足「书目录」形态：

  1. 编/章结构：任务标题带章节号（第 N 编 / 第 M 章 或 T/L 编号），labels
     继承阶段分组；
  2. 先修链：学习类任务之间必须有真实的 depends_on（不允许全空——学习类
     不适用默认并行）；
  3. 章级验收是「能讲清/能推出什么」，description 里不得把裸行号当验收依据
     （形如 :123 或「第 123 行」出现在验收语句里即失败）；
  4. 顺序自洽：depends_on 指向的任务在清单里排在它前面（按数组顺序）；
  5. 有「验证与输出」性质的收尾任务（重写/基准/复述 任一关键词）。

只断言、不导入（不点弹窗，提案留在 pendings 里随 server 退出丢弃）。

用法：
  python3 tests/blackbox/learning_plan_book_e2e.py [--repo <path>] [--port 4792]
前提：target/debug/latte-agent 已编译；~/.latte/models.d 有可用模型 key。
"""

import argparse
import json
import os
import queue
import re
import shutil
import subprocess
import sys
import threading
import time
import urllib.request

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
BIN = os.environ.get("LATTE_AGENT_BIN", os.path.join(ROOT, "target", "debug", "latte-agent"))
TEST_HOME = os.path.join(ROOT, ".latte", "test-home")
DEFAULT_REPO = os.path.expanduser("~/Documents/github/jemalloc")

events = queue.Queue()
all_events = []
STOP = False


def log(msg):
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)


def fail(msg):
    log(f"FAIL: {msg}")
    sys.exit(1)


def http_json(method, url, body=None, timeout=30):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    if data is not None:
        req.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.status, json.loads(r.read() or b"{}")


def sse_reader(url):
    try:
        with urllib.request.urlopen(url, timeout=3600) as r:
            for raw in r:
                if STOP:
                    return
                line = raw.decode("utf-8", "replace").strip()
                if not line.startswith("data:"):
                    continue
                try:
                    ev = json.loads(line[5:].strip())
                except json.JSONDecodeError:
                    continue
                if not isinstance(ev, dict):
                    continue
                all_events.append(ev)
                events.put(ev)
    except Exception as e:
        if not STOP:
            events.put({"type": "__sse_error__", "error": str(e)})


def wait_event(pred, timeout, what):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            ev = events.get(timeout=max(1, deadline - time.time()))
        except queue.Empty:
            break
        if ev.get("type") == "__sse_error__":
            fail(f"SSE 断开：{ev['error']}（等的是 {what}）")
        if pred(ev):
            return ev
    recent = [e.get("type") for e in all_events[-12:]]
    fail(f"等 {what} 超时（{timeout}s）。最近事件：{recent}")


def flatten(tasks, out):
    for t in tasks:
        out.append(t)
        flatten(t.get("subtasks") or [], out)


def check_book_shape(tasks):
    """对扁平化后的任务清单做书目录断言，返回问题列表（空 = 通过）。"""
    problems = []
    titles = [t.get("title", "") for t in tasks]

    # 1. 编/章结构：至少一半任务标题带章节号或编/章字样
    numbered = [t for t in titles
                if re.search(r"第\s*\d+\s*[编章]|^[A-Za-z]?\d+(\.\d+)+", t)]
    if len(numbered) < max(2, len(titles) // 2):
        problems.append(f"章节号覆盖不足：{len(numbered)}/{len(titles)} 个标题带章节号")

    # 2. 先修链：学习类任务之间必须有真实 depends_on
    deps = [t for t in tasks if t.get("depends_on")]
    if len(deps) < max(1, len(tasks) // 3):
        problems.append(f"depends_on 太少：{len(deps)}/{len(tasks)} 个任务声明了先修"
                        "（学习类不适用默认并行）")

    # 3. 章级验收不得以裸行号为依据
    for t in tasks:
        desc = t.get("description", "")
        for m in re.finditer(r"(?:验收|标准)[^。；\n]*[:：]\s*\d{2,}|第\s*\d{2,}\s*行", desc):
            problems.append(f"验收含裸行号：{t.get('title', '')[:30]} → …{m.group(0)[:40]}…")

    # 4. depends_on 顺序自洽：被依赖者必须排在前面
    pos = {}
    for i, t in enumerate(tasks):
        pos.setdefault(t.get("title", ""), i)
    for i, t in enumerate(tasks):
        for dep in t.get("depends_on") or []:
            hit = next((p for title, p in pos.items() if dep and dep in title), None)
            if hit is not None and hit >= i:
                problems.append(f"依赖顺序倒置：「{t.get('title', '')[:24]}」依赖排在"
                                f"后面的「{dep[:24]}」")

    # 5. 收尾有验证/输出章
    if not any(re.search(r"重写|基准|benchmark|复述|讲清|画出|实战|验证|实验|总结|迷你",
                         t) for t in titles[-3:]):
        problems.append("收尾 3 个任务里没有「验证与输出」性质的任务")

    # 6. 章是任务本体：禁止编级容器任务（标题带「（N章）」= 多章捆成一个任务，
    #    对 learn_loop 太大）
    for t in titles:
        if re.search(r"（\d+\s*章）", t):
            problems.append(f"编级容器任务：「{t[:40]}」把多章捆成一个任务")

    return problems


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--repo", default=DEFAULT_REPO)
    ap.add_argument("--port", type=int, default=4792)
    ap.add_argument("--goal", default=None)
    args = ap.parse_args()

    repo = os.path.abspath(args.repo)
    if not os.path.isdir(repo):
        fail(f"目标仓库不存在：{repo}")
    goal = args.goal or (
        f"把 {os.path.basename(repo)} 这个仓库的源码学习拆成一份完整的学习计划："
        "面向有 C 基础但不了解本项目的高级工程师，学成后能讲清其核心机制并动手验证。")

    if not os.path.isfile(BIN):
        fail(f"二进制不存在：{BIN}（先 cargo build -p latte-agent-cli）")

    # 复用 learn_loop_e2e 的隔离 LATTE_HOME（钉住可用模型链）。
    agents = os.path.join(TEST_HOME, "agents.d")
    os.makedirs(agents, exist_ok=True)
    for role in ("tutor", "advisor", "architect", "reviewer", "task_planner", "manager"):
        src = os.path.join(ROOT, ".latte", "agents.d", f"{role}.toml")
        if os.path.isfile(src):
            shutil.copy(src, os.path.join(agents, f"{role}.toml"))
    link = os.path.join(TEST_HOME, "models.d")
    target = os.path.expanduser("~/.latte/models.d")
    if not os.path.islink(link) and not os.path.exists(link):
        os.symlink(target, link)

    server_env = dict(os.environ)
    server_env["LATTE_HOME"] = TEST_HOME
    server = subprocess.Popen(
        [BIN, "ui", "--port", str(args.port),
         "--models-config", os.path.join(ROOT, "config", "models.toml")],
        cwd=repo, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
        env=server_env)
    global STOP
    try:
        for _ in range(60):
            try:
                http_json("GET", f"http://127.0.0.1:{args.port}/api/models", timeout=2)
                break
            except Exception:
                time.sleep(1)
                if server.poll() is not None:
                    fail(f"server 启动失败：{server.stderr.read().decode()[-500:]}")

        _, sess = http_json("POST", f"http://127.0.0.1:{args.port}/api/sessions", {})
        sid = sess["session_id"]
        log(f"session: {sid}（repo={repo}）")

        t = threading.Thread(
            target=sse_reader,
            args=(f"http://127.0.0.1:{args.port}/api/events?id={sid}",),
            daemon=True)
        t.start()
        time.sleep(1)

        log(f"启动 /learn-plan：{goal[:60]}…")
        http_json("POST", f"http://127.0.0.1:{args.port}/api/chat/command",
                  {"session_id": sid, "command": f"/learn-plan {goal}"})

        while True:
            ev = wait_event(
                lambda e: e.get("type") in ("PlanProposed", "WorkflowFinished",
                                            "WorkflowStep", "Error"),
                timeout=3600, what="plan 提案或 workflow 结束")
            if ev["type"] == "Error":
                fail(f"收到 Error 事件：{str(ev)[:300]}")
            if ev["type"] == "WorkflowStep":
                log(f"── step {ev.get('index')}/{ev.get('total')}: {ev.get('step_id')}")
                continue
            if ev["type"] == "WorkflowFinished":
                if ev.get("status") != "ok" and not any(
                        e.get("type") == "PlanProposed" for e in all_events):
                    fail(f"workflow 未成功结束且无提案：{ev.get('summary', '')[:300]}")
                continue
            # PlanProposed：拿到任务清单，做书目录断言
            plan_tasks = ev.get("tasks") or (ev.get("plan") or {}).get("tasks") or []
            if not plan_tasks:
                fail(f"PlanProposed 里没有 tasks：{list(ev.keys())}")
            flat = []
            flatten(plan_tasks, flat)
            log(f"收到提案：{len(plan_tasks)} 个顶层任务，扁平化后 {len(flat)} 个")
            for t in flat:
                deps = t.get("depends_on") or []
                log(f"  · {t.get('title', '')[:50]}"
                    + (f"  ←dep:{','.join(d[:20] for d in deps)}" if deps else ""))

            dump = os.path.join(repo, ".latte", "last-learning-plan.json")
            with open(dump, "w") as f:
                json.dump(plan_tasks, f, ensure_ascii=False, indent=2)
            log(f"提案已落盘：{dump}")

            problems = check_book_shape(flat)
            if problems:
                for p in problems:
                    log(f"  ✗ {p}")
                fail(f"书目录断言 {len(problems)} 项未过")
            print("PASS")
            return
    finally:
        STOP = True
        server.terminate()
        try:
            server.wait(timeout=10)
        except subprocess.TimeoutExpired:
            server.kill()


if __name__ == "__main__":
    main()
