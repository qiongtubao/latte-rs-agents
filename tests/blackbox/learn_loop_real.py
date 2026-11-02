#!/usr/bin/env python3
"""learn_loop 真实跑通验证（chat 路径：HTTP + SSE 驱动 `latte-agent ui`，等同 web chat）。

与 learn_loop_e2e.py / learn_loop_full_cycle.py 的区别：
- 工作区在 `.latte/sessions/<sid>/learn/<slug>/`（会话隔离的新布局）；
- `--real`：用真实全局 `~/.latte` 与真实模型链启动（不用 test-home 隔离）；
- `--cwd`：在哪个项目目录里启动服务（学习的对象项目）；
- 事件全部落成 JSONL（`--out`），事后用 `latte-agent debug usage <会话id>` 看 token / 耗时。

quiz 默认全对、supplement 答「没有了」；`--wrong-first` 第一轮故意答错第 1 题，走返工路径。
只有 `learn_loop` 自己的 `WorkflowFinished` 才算整体结束（嵌套的 doc_review_loop 也会发一个）。

用法：
  python3 tests/blackbox/learn_loop_real.py --real --wrong-first \
      --cwd ~/Documents/github/jemalloc --port 4812 --topic "<主题文字或 @文件>"
"""
import argparse, glob, json, os, queue, subprocess, sys, threading, time, urllib.request

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
BIN = os.environ.get("LATTE_AGENT_BIN", os.path.join(ROOT, "target", "debug", "latte-agent"))
TEST_HOME = os.path.join(ROOT, ".latte", "test-home")
events, all_events, STOP = queue.Queue(), [], False
T0 = time.time()


def log(m):
    print(f"[{int(time.time() - T0):5d}s] {m}", flush=True)


def http(method, url, body=None, timeout=30):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    if data is not None:
        req.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.status, json.loads(r.read() or b"{}")


def sse(url, sink):
    try:
        with urllib.request.urlopen(url, timeout=7200) as r:
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
                if isinstance(ev, dict):
                    all_events.append(ev)
                    sink.write(json.dumps(ev, ensure_ascii=False) + "\n")
                    sink.flush()
                    events.put(ev)
    except Exception as e:
        if not STOP:
            events.put({"type": "__sse_error__", "error": str(e)})


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--topic", required=True, help="主题文字；以 @ 开头表示从文件读")
    ap.add_argument("--port", type=int, default=4797)
    ap.add_argument("--cwd", default=ROOT)
    ap.add_argument("--out", default=os.path.join(ROOT, ".latte", "tmp", "learn_loop_real"))
    ap.add_argument("--wrong-first", action="store_true")
    ap.add_argument("--real", action="store_true", help="真实全局 ~/.latte，不指定 --models-config")
    ap.add_argument("--timeout", type=int, default=14400)
    a = ap.parse_args()
    topic = open(a.topic[1:], encoding="utf-8").read() if a.topic.startswith("@") else a.topic
    os.makedirs(a.out, exist_ok=True)
    srv_err = open(os.path.join(a.out, "server.stderr"), "w")
    if a.real:
        env = {k: v for k, v in os.environ.items() if k != "LATTE_HOME"}
        cmd = [BIN, "ui", "--port", str(a.port)]
    else:
        env = dict(os.environ, LATTE_HOME=TEST_HOME, LATTE_AGENT_DELEGATE_TIMEOUT_SECS="1800")
        cmd = [BIN, "ui", "--port", str(a.port), "--models-config", os.path.join(ROOT, "config", "models.toml")]
    srv = subprocess.Popen(cmd, cwd=a.cwd, stdout=subprocess.DEVNULL, stderr=srv_err, env=env)
    global STOP
    try:
        for _ in range(60):
            try:
                http("GET", f"http://127.0.0.1:{a.port}/api/models", timeout=2)
                break
            except Exception:
                time.sleep(1)
        _, s = http("POST", f"http://127.0.0.1:{a.port}/api/sessions", {})
        sid = s["session_id"]
        log(f"session {sid}")
        sink = open(os.path.join(a.out, "events.jsonl"), "w")
        threading.Thread(target=sse, args=(f"http://127.0.0.1:{a.port}/api/events?id={sid}", sink), daemon=True).start()
        time.sleep(1)
        http("POST", f"http://127.0.0.1:{a.port}/api/chat/command", {"session_id": sid, "command": f"/learn_loop {topic}"})
        log("已发 /learn_loop")

        def plan():
            pats = [
                f"{a.cwd}/.latte/sessions/{sid}/learn/*/plan.json",
                os.path.expanduser(f"~/.latte/worktrees/*/{sid}/.latte/sessions/{sid}/learn/*/plan.json"),
                f"{TEST_HOME}/worktrees/*/{sid}/.latte/sessions/{sid}/learn/*/plan.json",
            ]
            c = sorted([p for pat in pats for p in glob.glob(pat)], key=os.path.getmtime)
            return json.load(open(c[-1])) if c else None

        def answer(qtext, pl):
            for kp in pl["knowledge_points"]:
                q = kp["question"]
                if q in qtext or qtext in q or q[:15] in qtext:
                    return kp["answer"], [o["label"] for o in kp["options"]]
            return None, []

        quiz_round = 0
        deadline = time.time() + a.timeout
        while time.time() < deadline:
            try:
                ev = events.get(timeout=30)
            except queue.Empty:
                continue
            t = ev.get("type")
            if t == "__sse_error__":
                log(f"SSE 断开 {ev['error']}")
                return 2
            if t == "Error":
                log(f"Error 事件 {str(ev)[:400]}")
            if t == "WorkflowStep":
                log(f"step {ev.get('index')}/{ev.get('total')} {ev.get('step_id')}")
            elif t == "WorkflowTurn":
                c = ev.get("content", "")
                log(f"  turn[{ev.get('step_id')}] {len(c)} 字符: {c[:100].replace(chr(10), ' ')}")
            elif t in ("Status", "StatusMessage"):
                log(f"  status: {str(ev.get('message') or ev.get('content') or ev)[:300]}")
            elif t == "WorkflowFinished" and ev.get("name") == "learn_loop":
                log(f"workflow 结束 status={ev.get('status')} {str(ev.get('summary', ''))[:300]}")
                log(f"会话 {sid}；token 报告：latte-agent debug usage {sid} --dir <项目或 worktree>/.latte")
                return 0 if ev.get("status") == "ok" else 1
            elif t == "ChoiceRequested":
                cid = ev["choice_id"]
                qs = ev.get("questions") or []
                labels = [o.get("label", "") for o in (ev.get("options") or [])]
                post = lambda body: http("POST", f"http://127.0.0.1:{a.port}/api/chat/choice-answer", {"choice_id": cid, **body})
                if qs:
                    quiz_round += 1
                    pl = plan()
                    log(f"quiz 第 {quiz_round} 轮 {len(qs)} 题")
                    for i, q in enumerate(qs):
                        ans, cand = answer(q["question"], pl)
                        if ans is None:
                            log(f"BUG? 题面匹配不到: {q['question'][:80]}")
                            return 3
                        if a.wrong_first and quiz_round == 1 and i == 0:
                            ans = next(l for l in cand if l != ans)
                            log(f"  q{i + 1} 故意答错 {ans!r}")
                        post({"answer": ans, "question_id": q["id"]})
                elif any("开始答题" in l for l in labels):
                    log("read_confirm → 开始答题")
                    post({"answer": next(l for l in labels if "开始答题" in l)})
                elif any("没有了" in l for l in labels):
                    log("supplement → 没有了")
                    post({"answer": "没有了，全部理解"})
                else:
                    log(f"BUG? 未识别弹窗 labels={labels} q={ev.get('question', '')[:80]}")
                    return 3
        log("总超时")
        return 4
    finally:
        STOP = True
        srv.terminate()
        try:
            srv.wait(10)
        except subprocess.TimeoutExpired:
            srv.kill()


if __name__ == "__main__":
    sys.exit(main())
