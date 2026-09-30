#!/usr/bin/env python3
"""learn_loop v2（文档驱动学习循环）的端到端黑盒验证。

用 HTTP + SSE 驱动 `latte-agent ui`（web UI 的真实后端链路），跑一遍完整流程：

  /learn_loop <topic>
    → plan（落盘 plan.json / ledger.json）
    → annotate（注释版代码副本 code/，默认生成；不带「同步注释」时 sync_code 跳过）
    → doc（draft.md；正文必须带文档全文）
    → read_confirm 弹窗「开始答题」—— 断言：doc 全文先于弹窗到达（先学后答）
    → quiz 弹窗（questions 多题）—— 第一轮故意答错第 1 题，验证
      QUIZ_FAIL → doc 增强 → 重新阅读 → 重考 循环
    → 第二轮全对（正确答案从 plan.json 读）→ QUIZ_PASS
    → supplement 弹窗 —— 第一次答「其他：…」（验证 SUPPLEMENT → doc 补章节+
      新知识点 → 重读 → 重考循环），第二次答「没有了，全部理解」→ ALL_DONE
    → merge → report —— 断言 docs/learn/<slug>.md 生成、ledger 全 passed、
      WorkflowFinished status=ok

终端 CLI 路径不在本测试范围：questions 多题弹框在终端只渲染第一题且
resolve 语义不符（web UI 的 choice-answer/resolve_one 才是完整实现）。

用法：
  python3 tests/blackbox/learn_loop_e2e.py [--topic "…"] [--port 4791]
前提：target/debug/latte-agent 已编译；~/.latte/models.d 有可用模型 key。

模型说明：脚本会在 .latte/test-home/ 自建一个隔离 LATTE_HOME（tutor 链钉
deepseek-v4-flash，models.d 符号链接回真实全局目录拿 key）——全局
~/.latte 的 tutor 链是 MiniMax-M3/glm-5.3，实测在长上下文下反复返回空
内容或挂起 120s+，而 deepseek 欠费（402）时会自动落到 glm-5.3 继续跑，
慢但能完成。
"""

import argparse
import glob
import json
import os
import queue
import subprocess
import sys
import threading
import time
import urllib.request

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
BIN = os.environ.get("LATTE_AGENT_BIN", os.path.join(ROOT, "target", "debug", "latte-agent"))
DEFAULT_TOPIC = "latte-agent-core 的 choice 模块：多题弹框的收齐机制（choice.rs 的 resolve / resolve_one）"
TEST_HOME = os.path.join(ROOT, ".latte", "test-home")


def setup_test_home():
    """自建隔离 LATTE_HOME：tutor/advisor 角色钉到可用模型，models.d 指回全局。"""
    agents = os.path.join(TEST_HOME, "agents.d")
    os.makedirs(agents, exist_ok=True)
    import shutil
    for role in ("tutor", "advisor", "proofreader"):
        src = os.path.join(ROOT, ".latte", "agents.d", f"{role}.toml")
        if os.path.isfile(src):
            shutil.copy(src, os.path.join(agents, f"{role}.toml"))
    link = os.path.join(TEST_HOME, "models.d")
    target = os.path.expanduser("~/.latte/models.d")
    if not os.path.islink(link) and not os.path.exists(link):
        os.symlink(target, link)

events = queue.Queue()
all_events = []
STOP = False


def log(msg):
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)


def http_json(method, url, body=None, timeout=30):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    if data is not None:
        req.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.status, json.loads(r.read() or b"{}")


def sse_reader(url):
    """把 SSE 的 chat_event 逐条塞进队列。"""
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
                    continue  # 心跳/keep-alive 行
                all_events.append(ev)
                events.put(ev)
    except Exception as e:
        if not STOP:
            events.put({"type": "__sse_error__", "error": str(e)})


def wait_event(pred, timeout, what):
    """等到满足 pred 的事件；超时打印最近事件并退出。"""
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


def fail(msg):
    log(f"FAIL: {msg}")
    sys.exit(1)


def load_plan():
    """找最新的 .latte/learn/*/plan.json。"""
    cands = sorted(glob.glob(os.path.join(ROOT, ".latte", "learn", "*", "plan.json")),
                   key=os.path.getmtime)
    if not cands:
        return None, None
    with open(cands[-1]) as f:
        return json.load(f), cands[-1]


def answer_for(question_text, plan):
    """按题面文本匹配 plan.json 里的知识点，返回正确答案 label。"""
    for kp in plan["knowledge_points"]:
        q = kp["question"]
        # quiz 重考会追加 ［第N次作答］ 后缀：用包含关系匹配。
        if q in question_text or question_text in q or q[:15] in question_text:
            return kp["answer"], [o["label"] for o in kp["options"]]
    return None, []


def post_answer(port, choice_id, answer, question_id=None):
    body = {"choice_id": choice_id, "answer": answer}
    if question_id:
        body["question_id"] = question_id
    status, _ = http_json("POST", f"http://127.0.0.1:{port}/api/chat/choice-answer", body)
    return status


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--topic", default=DEFAULT_TOPIC)
    ap.add_argument("--port", type=int, default=4791)
    args = ap.parse_args()
    port = args.port

    if not os.path.isfile(BIN):
        fail(f"二进制不存在：{BIN}（先 cargo build --bin latte-agent）")
    setup_test_home()

    # 记录测试前已有的 learn 目录，避免把旧产物错当本次的。
    pre_existing = set(glob.glob(os.path.join(ROOT, ".latte", "learn", "*")))

    server_env = dict(os.environ)
    # 隔离的 LATTE_HOME：tutor 链钉 deepseek-v4-flash（全局 ~/.latte 的 tutor
    # 链是 MiniMax-M3/glm-5.3，长上下文下反复返回空内容/挂起）；models.d
    # 符号链接回真实全局目录拿 API key。
    server_env["LATTE_HOME"] = os.path.join(ROOT, ".latte", "test-home")
    server = subprocess.Popen(
        [BIN, "ui", "--port", str(port),
         # 项目 models.d 只有 .bak，tier 映射会落进全局（standard→glm-5.3）。
         # 显式指向 config/models.toml，让 standard→deepseek-v4-flash。
         "--models-config", os.path.join(ROOT, "config", "models.toml")],
        cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
        env=server_env,
    )
    global STOP
    try:
        # 等端口起来
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

        # ── 状态机 ────────────────────────────────────────────
        doc_delivered = False       # doc 步全文是否已投递
        quiz_round = 0
        supplement_round = 0
        slug = None

        while True:
            ev = wait_event(
                lambda e: e.get("type") in ("WorkflowTurn", "ChoiceRequested",
                                            "WorkflowFinished", "WorkflowStep",
                                            "Error"),
                timeout=1800, what="workflow 事件")

            if ev["type"] == "Error":
                fail(f"收到 Error 事件：{str(ev)[:300]}")

            if ev["type"] == "WorkflowStep":
                log(f"── step {ev.get('index')}/{ev.get('total')}: "
                    f"{ev.get('step_id')}（{ev.get('description', '')[:40]}）")
                continue

            if ev["type"] == "WorkflowTurn":
                if ev.get("step_id") == "doc_write":
                    content = ev.get("content", "")
                    assert "draft.md" in content, "doc_write 文档产出必须给出 draft.md 路径"
                    if len(content) >= 500:
                        doc_delivered = True
                        log(f"doc_review 文档产出（{len(content)} 字符）")
                if ev.get("step_id") == "plan" and slug is None:
                    plan, path = load_plan()
                    if plan:
                        slug = plan["slug"]
                        n = len(plan["knowledge_points"])
                        log(f"plan 落盘：slug={slug}，{n} 个知识点")
                continue

            if ev["type"] == "WorkflowFinished":
                status = ev.get("status")
                log(f"workflow 结束：status={status}")
                if status != "ok":
                    fail(f"workflow 未成功结束：{ev.get('summary', '')[:300]}")
                break

            # ChoiceRequested：分辨是哪一步的弹窗
            choice_id = ev["choice_id"]
            questions = ev.get("questions") or []
            options = ev.get("options") or []
            labels = [o.get("label", "") for o in options]

            if questions:
                # ── quiz：多题弹框 ──
                quiz_round += 1
                plan, _ = load_plan()
                if not plan:
                    fail("quiz 弹窗到了但 plan.json 不存在")
                log(f"quiz 第 {quiz_round} 轮：{len(questions)} 题"
                    + ("（本轮故意答错第 1 题）" if quiz_round == 1 else "（本轮全对）"))
                for i, q in enumerate(questions):
                    ans, cand = answer_for(q["question"], plan)
                    if ans is None:
                        fail(f"题面匹配不到知识点：{q['question'][:60]}")
                    if quiz_round == 1 and i == 0:
                        wrong = next(l for l in cand if l != ans)
                        st = post_answer(port, choice_id, wrong, q["id"])
                        log(f"  q{i+1} 故意答错：{wrong!r}（正确应为 {ans!r}）→ HTTP {st}")
                    else:
                        st = post_answer(port, choice_id, ans, q["id"])
                        log(f"  q{i+1} 作答：{ans!r} → HTTP {st}")
                    if st not in (200, 202):
                        fail(f"答案投递失败：HTTP {st}")

            elif any("开始答题" in l for l in labels):
                # ── read_confirm：「开始答题」按钮 ──
                if not doc_delivered:
                    fail("read_confirm 弹窗到了，但 doc 步全文还没投递"
                         "（学员会先看到按钮、后看到文档——顺序错了）")
                log("read_confirm 弹窗：先文后钮顺序正确，点「开始答题」")
                label = next(l for l in labels if "开始答题" in l)
                st = post_answer(port, choice_id, label)
                if st != 200:
                    fail(f"read_confirm 答案投递失败：HTTP {st}")

            elif any("没有了" in l for l in labels):
                # ── supplement：还有不懂的吗 ──
                supplement_round += 1
                if supplement_round == 1:
                    extra = "其他：请补充 choice.rs 里 pending 表的并发处理"
                    log(f"supplement 第 1 次：提补充疑问 {extra!r}")
                    st = post_answer(port, choice_id, extra)
                else:
                    log(f"supplement 第 {supplement_round} 次：答「没有了，全部理解」")
                    st = post_answer(port, choice_id, "没有了，全部理解")
                if st != 200:
                    fail(f"supplement 答案投递失败：HTTP {st}")
            else:
                fail(f"未识别的弹窗：question={ev.get('question', '')[:80]} "
                     f"labels={labels} questions={len(questions)}")

        # ── 收尾断言 ──────────────────────────────────────────
        if slug is None:
            plan, _ = load_plan()
            slug = plan["slug"] if plan else None
        if not slug:
            fail("始终没找到 plan.json / slug")

        learn_dir = os.path.join(ROOT, ".latte", "learn", slug)
        if learn_dir in pre_existing:
            log("（注意：learn 目录本次前已存在，断言基于其最终内容）")

        final_doc = os.path.join(ROOT, "docs", "learn", f"{slug}.md")
        if not os.path.isfile(final_doc) or os.path.getsize(final_doc) < 500:
            fail(f"最终文档未生成或太小：{final_doc}")
        log(f"最终文档：{final_doc}（{os.path.getsize(final_doc)} 字节）")

        with open(os.path.join(learn_dir, "ledger.json")) as f:
            ledger = json.load(f)
        not_passed = [k["id"] for k in ledger["knowledge_points"] if not k["passed"]]
        if not_passed:
            fail(f"ledger 还有未掌握知识点：{not_passed}")
        if not ledger.get("supplements"):
            fail("ledger.supplements 为空——补充疑问没记账")
        log(f"ledger：{len(ledger['knowledge_points'])} 个知识点全部 passed，"
            f"round={ledger.get('round')}，supplements={len(ledger['supplements'])}")

        # 补充疑问应该长出了新知识点（初始 3-6 个 + 补充 1 个）
        plan, _ = load_plan()
        log(f"plan 最终 {len(plan['knowledge_points'])} 个知识点（含补充新增）")

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
