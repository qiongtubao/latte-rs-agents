#!/usr/bin/env python3
"""learn_loop 完整流程验证。

跑 /learn_loop <topic>，观察：

  plan → annotate → 写章节 → read_confirm → quiz → supplement → merge → report

测试行为：
- read_confirm 弹窗自动点「开始答题」
- quiz 弹窗按 plan.json 的正确答案作答（保证 QUIZ_PASS，不触发教学循环）
- supplement 弹窗答「没有了，全部理解」→ ALL_DONE
- 最后断言 WorkflowFinished status=ok + docs/learn/<slug>.md 生成

断言点：
- 章节写完就出现 read_confirm，不等审稿员的 LOOKS_CLEAR
- quiz 与 supplement 都跑完

用法：
  python3 tests/blackbox/learn_loop_full_cycle.py [--topic "…"] [--port 4796]
"""

import argparse
import glob
import json
import os
import queue
import re
import subprocess
import sys
import threading
import time
import urllib.request

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
BIN = os.environ.get("LATTE_AGENT_BIN", os.path.join(ROOT, "target", "debug", "latte-agent"))
TEST_HOME = os.path.join(ROOT, ".latte", "test-home")
DEFAULT_TOPIC = "latte-agent-core 的 inject_queue.rs：消息注入队列的入队与排空机制"

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
    recent = [(e.get("type"), e.get("step_id")) for e in all_events[-12:]]
    fail(f"等 {what} 超时（{timeout}s）。最近：{recent}")


def load_plan():
    cands = sorted(glob.glob(os.path.join(ROOT, ".latte", "learn", "*", "plan.json")),
                   key=os.path.getmtime)
    if not cands:
        return None
    with open(cands[-1]) as f:
        return json.load(f)


def answer_for(question_text, plan):
    for kp in plan["knowledge_points"]:
        q = kp["question"]
        if q in question_text or question_text in q or q[:15] in question_text:
            return kp["answer"]
    return None


def post_answer(port, choice_id, answer, question_id=None):
    body = {"choice_id": choice_id, "answer": answer}
    if question_id:
        body["question_id"] = question_id
    status, _ = http_json("POST", f"http://127.0.0.1:{port}/api/chat/choice-answer", body)
    return status


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--topic", default=DEFAULT_TOPIC)
    ap.add_argument("--port", type=int, default=4796)
    args = ap.parse_args()
    port = args.port

    if not os.path.isfile(BIN):
        fail(f"二进制不存在：{BIN}")

    server_env = dict(os.environ)
    server_env["LATTE_HOME"] = TEST_HOME
    server = subprocess.Popen(
        [BIN, "ui", "--port", str(port),
         "--models-config", os.path.join(ROOT, "config", "models.toml")],
        cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
        env=server_env,
    )
    global STOP
    try:
        for _ in range(60):
            try:
                http_json("GET", f"http://127.0.0.1:{port}/api/models", timeout=2)
                break
            except Exception:
                time.sleep(1)
                if server.poll() is not None:
                    fail(f"server 启动失败：{server.stderr.read().decode()[-500:]}")
        log(f"server 已启动 :{port}")

        _, sess = http_json("POST", f"http://127.0.0.1:{port}/api/sessions", {})
        sid = sess["session_id"]
        log(f"session: {sid}")

        t = threading.Thread(target=sse_reader,
                             args=(f"http://127.0.0.1:{port}/api/events?id={sid}",),
                             daemon=True)
        t.start()
        time.sleep(1)

        log(f"启动 /learn_loop：{args.topic}")
        http_json("POST", f"http://127.0.0.1:{port}/api/chat/command",
                  {"session_id": sid, "command": f"/learn_loop {args.topic}"})

        # ── 状态机：收集 doc/proofread 轮次 + 应答弹窗 ──
        doc_rounds = 0
        proof_verdicts = []
        slug = None
        plan = None
        quiz_answered = False
        supplement_answered = False
        saw_read_confirm = False

        while True:
            ev = wait_event(
                lambda e: e.get("type") in ("WorkflowTurn", "ChoiceRequested",
                                            "WorkflowFinished", "WorkflowStep",
                                            "Error"),
                timeout=3600, what="workflow 事件")

            if ev["type"] == "Error":
                fail(f"Error 事件：{str(ev)[:300]}")

            if ev["type"] == "WorkflowStep":
                log(f"── step {ev.get('index')}/{ev.get('total')}: {ev.get('step_id')}")
                continue

            if ev["type"] == "WorkflowTurn":
                sid_ = ev.get("step_id")
                content = ev.get("content", "")
                if sid_ == "doc_write" and len(content) >= 500:
                    doc_rounds += 1
                    log(f"doc 第 {doc_rounds} 轮投递（{len(content)} 字符）")
                elif sid_ == "proofread":
                    verdict = "LOOKS_CLEAR" if "LOOKS_CLEAR" in content else \
                              "NEEDS_FIX" if "NEEDS_FIX" in content else "UNKNOWN"
                    proof_verdicts.append(verdict)
                    n_issues = len(re.findall(r"^ISSUE:", content, re.M))
                    log(f"proofread 第 {len(proof_verdicts)} 次 verdict={verdict}（{n_issues} 条 ISSUE）")
                elif sid_ == "plan" and slug is None:
                    plan = load_plan()
                    if plan:
                        slug = plan["slug"]
                        log(f"plan 落盘：slug={slug}，{len(plan['knowledge_points'])} 个知识点")
                continue

            if ev["type"] == "WorkflowFinished":
                status = ev.get("status")
                log(f"workflow 结束：status={status}")
                if status != "ok":
                    fail(f"workflow 未成功：{ev.get('summary', '')[:300]}")
                break

            # ── ChoiceRequested ──
            choice_id = ev["choice_id"]
            questions = ev.get("questions") or []
            options = ev.get("options") or []
            labels = [o.get("label", "") for o in options]

            if questions:
                # quiz：全对模式
                if not plan:
                    plan = load_plan()
                if not plan:
                    fail("quiz 弹窗到了但 plan.json 不存在")
                log(f"quiz：{len(questions)} 题（全对模式）")
                for q in questions:
                    ans = answer_for(q["question"], plan)
                    if ans is None:
                        fail(f"题面匹配不到知识点：{q['question'][:60]}")
                    st = post_answer(port, choice_id, ans, q["id"])
                    log(f"  答 {ans!r} → HTTP {st}")
                    if st not in (200, 202):
                        fail(f"答案投递失败：HTTP {st}")
                quiz_answered = True

            elif any("开始答题" in l for l in labels):
                saw_read_confirm = True
                log("read_confirm 弹窗：章节已写完，学员开始阅读")
                label = next(l for l in labels if "开始答题" in l)
                st = post_answer(port, choice_id, label)
                if st != 200:
                    fail(f"read_confirm 投递失败：HTTP {st}")

            elif any("没有了" in l for l in labels):
                log("supplement：答「没有了，全部理解」")
                st = post_answer(port, choice_id, "没有了，全部理解")
                if st != 200:
                    fail(f"supplement 投递失败：HTTP {st}")
                supplement_answered = True
            else:
                fail(f"未识别弹窗：labels={labels} questions={len(questions)}")

        # ── 收尾断言 ──
        if not saw_read_confirm:
            fail("read_confirm 弹窗未到——章节写完后没有放行给学员")
        if not quiz_answered:
            fail("quiz 未跑")
        if not supplement_answered:
            fail("supplement 未跑")

        if slug:
            final_doc = os.path.join(ROOT, "docs", "learn", f"{slug}.md")
            if not os.path.isfile(final_doc):
                fail(f"最终文档未生成：{final_doc}")
            log(f"最终文档：{final_doc}（{os.path.getsize(final_doc)} 字节）")

        log(f"总结：doc {doc_rounds} 轮 / proofread {len(proof_verdicts)} 轮 / quiz ✓ / supplement ✓ / merge ✓")
        print("PASS")
    finally:
        STOP = True
        server.terminate()
        try:
            server.wait(timeout=10)
        except subprocess.TimeoutExpired:
            server.kill()


if __name__ == "__main__":
    main()
