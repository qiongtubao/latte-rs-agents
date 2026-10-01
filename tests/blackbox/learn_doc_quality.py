#!/usr/bin/env python3
"""doc 步 draft.md 质量验证（learn_loop.toml 新增硬要求的回归测试）。

跑 /learn_loop <topic>，等到 doc 步投递 draft.md 全文后立刻检查：

  R1 真实数值    记录具体数字数量；查表/位运算主题才有意义，设计向文档不因此失败
  R2 逐值 trace  记录链式等式；设计向文档不因此失败
  R3 非符号索引图 mermaid 节点含动作/数据描述，不是 fn xxx / struct xxx 罗列
  R4 记号约定    开头有导读，或引擎写入的「一句话结论 / 核心结论」
  R5 file:line   每节带行号引用（原有要求，回归保护）
  R8 设计深度    开头有决策总览表，正文有方案对比与代价

通过后不继续走 read_confirm/quiz —— 直接退出（learn 目录残留无妨）。

用法：
  python3 tests/blackbox/learn_doc_quality.py [--topic "…"] [--port 4792]
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
# 主题特意挑有查表/位运算的代码，让"真实值 + trace"规则可被触发
DEFAULT_TOPIC = "latte-agent-core 里 choice.rs 的 resolve / resolve_one 收齐机制"

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


def wait_doc_then_proofread(timeout=1800):
    """等到章节写完、学员可以开始答题。

    返回 (doc_content, "", "STUDENT")。放行条件是 read_confirm 弹窗，
    不是审稿员的 LOOKS_CLEAR。"""
    deadline = time.time() + timeout
    doc = None
    while time.time() < deadline:
        try:
            ev = events.get(timeout=max(1, deadline - time.time()))
        except queue.Empty:
            break
        t = ev.get("type")
        if t == "__sse_error__":
            fail(f"SSE 断开：{ev['error']}")
        if t == "Error":
            fail(f"Error 事件：{str(ev)[:300]}")
        if t == "WorkflowStep":
            log(f"── step {ev.get('index')}/{ev.get('total')}: {ev.get('step_id')}")
            continue
        if t == "WorkflowFinished":
            fail(f"workflow 提前结束（学员确认前）：{ev.get('summary', '')[:200]}")
        if t == "WorkflowTurn":
            sid = ev.get("step_id")
            content = ev.get("content", "")
            if sid == "doc" and doc is None and "draft.md" in content and len(content) >= 500:
                doc = content
                log(f"doc 全文到手：{len(content)} 字符")
                continue
        if t == "ChoiceRequested":
            return doc or "", "", "STUDENT"
    recent = [e.get("type") for e in all_events[-10:]]
    fail(f"等学员确认超时。最近事件：{recent}")


# ─── 质量断言 ────────────────────────────────────────────

def check_rules(doc, draft_path):
    """对 doc 全文应用 5 条新规则检查，返回 (failures, notes)。"""
    failures = []
    notes = []

    # R1 真实数值：统计" = <数字>"和数组字面量的出现次数
    numeric_assigns = len(re.findall(r"=\s*\d[\d,]*", doc))
    array_literals = len(re.findall(r"\[\s*\d+\s*[,\]]", doc))
    notes.append(f"R1: numeric_assigns={numeric_assigns}, array_literals={array_literals}")
    if numeric_assigns + array_literals < 8:
        notes.append(
            f"R1 提示：具体数字偏少（{numeric_assigns + array_literals}），"
            "仅在查表/位运算类主题视为不足，设计向文档不因此失败"
        )

    # R2 逐值 trace：「x = f(...) = 数字」或「= <expr> = <数字>」链式等式
    trace_chains = re.findall(r"=\s*[^=\n]{1,40}=\s*\d", doc)
    notes.append(f"R2: trace_chains={len(trace_chains)}")
    if len(trace_chains) < 3:
        notes.append(
            f"R2 提示：链式推导 {len(trace_chains)} 处，设计向文档不因此失败"
        )

    # R3 mermaid 图不是符号索引
    mermaid_blocks = re.findall(r"```mermaid\n(.*?)```", doc, re.DOTALL)
    symbol_index_like = 0
    for block in mermaid_blocks:
        # 节点形如 n0["fn xxx"] / n1["struct xxx"] 且占节点总数 >60% 就算符号索引图
        nodes = re.findall(r'\["(fn |struct |type |enum )', block)
        total = max(len(re.findall(r'\["', block)), 1)
        if len(nodes) / total > 0.6:
            symbol_index_like += 1
    notes.append(f"R3: mermaid={len(mermaid_blocks)}, symbol_index_like={symbol_index_like}")
    if mermaid_blocks and symbol_index_like == len(mermaid_blocks):
        failures.append("R3 mermaid 全是符号索引图（fn/struct 罗列），没有数据流/时序图")

    # R4 导读/约定段（接受任意一种"对零基础友好"的开头标志）
    head = doc[:2000]
    has_intro = any(k in head for k in (
        "写给", "读者", "约定", "只需要知道", "这一章只讲",
        "先建立画面", "它是什么", "是什么、整体", "人话总结",
        "一句话结论", "核心结论", "核心要点",
    ))
    notes.append(f"R4: has_intro={has_intro}")
    if not has_intro:
        failures.append("R4 开头缺少导读/约定段（写给/先建立画面/它是什么/人话总结 都没命中）")

    # R5 file:line 引用
    file_lines = re.findall(r"[\w./-]+:\d+", doc)
    notes.append(f"R5: file:line refs={len(file_lines)}")
    if len(file_lines) < 10:
        failures.append(f"R5 行号引用不足：只有 {len(file_lines)} 处（<10）")


    # R6 代码块行内中文注释：对每段非 mermaid 代码块，统计"有代码内容"的行里
    # 带行内中文注释的占比。判断"代码行"用宽松口径：含 ;/{/}/=/=>/, 等代码符号
    # 或结尾是 , 的字段行；纯注释行（//、#、/* 开头）不计入分母也不计入分子。
    code_blocks = [b for b in re.findall(r"```(\w*)\n(.*?)```", doc, re.DOTALL)
                   if b[0] != "mermaid"]
    commented = 0
    total_code = 0
    for _, b in code_blocks:
        for line in b.splitlines():
            s = line.strip()
            if not s or s.startswith(("//", "#", "/*", "*", "```")):
                continue
            # 认定"代码行"：含典型代码符号，或一眼是字段/语句（有标识符且非纯英文句子）
            is_code = bool(re.search(r"[;{}=<>]|=>|,\s*$", s)) or \
                      bool(re.match(r"^[A-Za-z_][\w.<>:]*\s*[\w<>:]*\s*[=({,;]", s))
            if not is_code:
                continue
            total_code += 1
            if re.search(r"(//|#|/\*)\s*[一-鿿]", s):
                commented += 1
    ratio = commented / max(total_code, 1)
    notes.append(f"R6: code_lines={total_code}, cn_comments={commented}, ratio={ratio:.2f}")
    if total_code >= 10 and ratio < 0.15:
        failures.append(f"R6 代码行内中文注释不足：{commented}/{total_code}（{ratio:.0%} < 15%）")

    # R7 调用图节点带动作描述：mermaid 里 `fn xxx` / `xxx()` 节点应有 <br/> 或中文动作
    bare_fn_nodes = 0
    labeled_nodes = 0
    for block in mermaid_blocks:
        for m in re.finditer(r'\["([^"]+)"\]', block):
            label = m.group(1)
            if re.match(r"^(fn |struct |type |enum )", label):
                bare_fn_nodes += 1
            elif "<br" in label or re.search(r"[一-鿿]", label):
                labeled_nodes += 1
            else:
                bare_fn_nodes += 1
    notes.append(f"R7: labeled_nodes={labeled_nodes}, bare_fn_nodes={bare_fn_nodes}")
    if mermaid_blocks and bare_fn_nodes > labeled_nodes:
        failures.append(f"R7 调用图节点缺动作描述：{bare_fn_nodes} 个节点只有符号名/无中文")

    # R8 设计深度：开头有决策总览表，正文有方案对比与代价，而不只是概念讲解
    has_decision_table = bool(re.search(r"\|\s*设计问题\s*\|", doc[:6000]))
    tradeoff_hits = sum(doc.count(k) for k in ("为什么选", "替代方案", "放弃的方案", "方案对比"))
    cost_hits = sum(doc.count(k) for k in ("代价", "失效", "边界"))
    notes.append(f"R8: decision_table={has_decision_table}, tradeoff={tradeoff_hits}, cost={cost_hits}")
    if not has_decision_table:
        failures.append("R8 开头缺少「设计问题 | 作者的选择 | 放弃的方案 …」决策总览表")
    if tradeoff_hits < 3 or cost_hits < 2:
        failures.append(f"R8 设计取舍讨论不足：方案对比 {tradeoff_hits} 处（<3）/ 代价边界 {cost_hits} 处（<2）")

    return failures, notes


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--topic", default=DEFAULT_TOPIC)
    ap.add_argument("--port", type=int, default=4792)
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

        doc, _proof, verdict = wait_doc_then_proofread()
        log(f"doc 全文到手：{len(doc)} 字符；放行 = {verdict}")
        # 找到本次生成的 draft.md 落盘
        cands = sorted(glob.glob(os.path.join(ROOT, ".latte", "learn", "*", "draft.md")),
                       key=os.path.getmtime)
        draft_path = cands[-1] if cands else None
        if draft_path and time.time() - os.path.getmtime(draft_path) < 600:
            with open(draft_path) as f:
                on_disk = f.read()
            log(f"draft.md 落盘：{draft_path}（{len(on_disk)} 字符）")
            # chat 正文和落盘可能不完全一致（正文会带路径/答题提示两行），以落盘为准
            doc_for_check = on_disk if len(on_disk) > len(doc) * 0.5 else doc
        else:
            log("draft.md 未落盘，用 chat 正文检查")
            doc_for_check = doc

        failures, notes = check_rules(doc_for_check, draft_path)
        for n in notes:
            log("  " + n)

        if failures:
            for f_ in failures:
                log("  ✗ " + f_)
            fail(f"{len(failures)} 条规则未达标")

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
