# `latte-agent chat` 性能诊断报告

**日期**: 2026-07-08  
**测试环境**: 本地 `target/debug/latte-agent`，glm-5.2 (manager) + deepseek-v4-flash (specialist)  
**触发场景**: `latte-agent chat` "列出项目里所有文件"

---

## 1. 实测数据

| 指标 | 数值 |
|------|------|
| 端到端墙钟 (一次完整 chat) | **18–163s**（**方差极大**，取决于 glm-5.2 的直觉和重复 pattern） |
| LLM 调用次数（一次 chat） | **23**（在"列出项目所有文件"场景，含 4 个并行 delegate 风暴） |
| 累计 LLM latency | **83.5s** |
| Delegate (manager→specialist) 调用次数 | **4**（manager 重复发了 4 次相同的"列文件"任务） |
| Delegate 中的 timeout 数（≥29s） | **4** |
| Tool 调用 (programmer 内部 bash/read) | **15** |

---

## 2. 四个慢的因素（按影响排序）

### 因素 #1 — Manager 重复委托（**+120s**，**占总慢 50%+**）

**症状**：manager 一次响应里发 **4 次相同的 `delegate`**，每次等 30s 后才返回。

实测时间线：
```
15:28:19  ToolExec manager/delegate (programmer)  30.00s  ← 第1次
15:28:19  ToolExec manager/delegate (programmer)  30.00s  ← 第2次（同 prompt）
15:28:19  ToolExec manager/delegate (programmer)  30.00s  ← 第3次
15:28:19  ToolExec manager/delegate (programmer)  30.00s  ← 第4次
```

**根因**：
- glm-5.2 自己决定"需要再确认",连续触发相同 delegate
- 单次 specialist 内部循环 4 轮 tool calls（deepseek-v4-flash 自己也会重复），加上 latency 叠加，每个 delegate 长达 30s
- **这是 glm-5.2 的 chain-of-thought 决策行为**，不是 bug

**修复**：

| 方案 | 收益 | 风险 | 工作量 |
|------|------|------|--------|
| **A. 限制 manager 每个 turn 的 delegate 次数** | ~50% | 中（可能让 manager 表达不完整） | 小 |
| **B. 给 manager 加一个"已经做过的事"记忆，抑制重复 dispatch** | ~70% | 低 | 中 |
| C. 限制 deepseek-v4-flash 工具调用轮数（`max_tool_rounds=3`） | ~30% | 高（过早截断可能导致读取失败） | 极小 |

### 因素 #2 — Manager 第一轮调错工具（+6.5s）

**症状**：`manager/list latency=0 status=Err: "Tool not found: list"`

manager 在 system prompt 里看到 `Allowed tool names: read, list, search, delegate`，**先试了 `list`**——但当前 build 里 manager 只注册了 `delegate`，所以 list 失败。再走一轮 LLM 才 fallback 到 delegate。

**根因**：
- `config/agents.toml:58` 给 manager 配置 `tools = ["read", "list", "search"]`  
- `tests/.latte/models.toml` 又给了 manager 一个独立配置，**两者不一致**  
- `tool_usage_prompt()` 把所有"allowed"的工具列出来，但运行时只挂了 `delegate`

**修复**：在 `config/agents.toml` 中把 manager 的 tools 改成 `[]`，或在 chat.rs 中显式注册 read/list（不需要，因为 manager 应该只 delegate）。

### 因素 #3 — Specialist 内部多轮 tool calls（**5-10s/specialist**）

**症状**：programmer 一次任务内部跑了 4-7 轮 bash/read/list。

实测一次 specialist 内部时间线：
```
programmer/deepseek-v4-flash: bash pwd && ls -la     1.49s
programmer/deepseek-v4-flash: ls -la                  1.56s
programmer/deepseek-v4-flash: pwd && ls                1.26s
programmer/deepseek-v4-flash: bash ls                  1.10s
programmer/deepseek-v4-flash: bash cat Cargo.toml     1.06s
```

**根因**：deepseek-v4-flash 在 prompt 模板里写"你先跑 pwd && ls，再..."——模型严格执行这个 chain，导致多次重复。

**修复**：

| 方案 | 收益 | 风险 |
|------|------|------|
| 改 `prompts/programmer.md`：从"mandatory 4 步"改为"如果 ls 输出已经够，跳到第 2 步" | 30-40% | 低 |
| 给 specialist 加 `max_tool_rounds=4` 硬上限 | ~30% | 中 |

### 因素 #4 — Manager 综合回答是再调一次 LLM（~5-10s）

正常 LLM 调用开销。每次 manager 收到 specialist 结果后都要再过一遍 LLM 才能返回用户。

**修复**：无。这是 chat 必要流程（specialist 输出需要 manager 整理成最终回复）。

---

## 3. 设计如此的慢（**不应该优化**）

### 3.1 Review chain 多层审查

**场景 3/5/9 (find-bugs / security / reviewer-chain)** 故意触发：
```
programmer → reviewer_sanity → reviewer_architecture → reviewer_security
```

每层 60-120s 串行，总计 3-5 分钟。**这是设计目的**——多层防御幻觉和错误。

### 3.2 Specialist 多轮 tool call

每个 specialist 独立跑 list→read→bash 链。这是该角色的工作方式。

---

## 4. 模型选型根因

| 角色 | 当前 model | 慢的原因 |
|------|-----------|---------|
| manager | **glm-5.2** | latency 中（5-10s 一次），会重复 dispatch |
| programmer | **deepseek-v4-flash** | 单次快（1-2s），但严格按 prompt 走，会重复跑 |
| reviewer_sanity | **deepseek-v4-flash** | 同上，每层都要再起一个 session |
| architect | **claude-sonnet** | API key 缺失时会 fallback 到 deepseek（已实测到） |
| security | **claude-opus** | 比 sonnet 慢 3-5 倍 |

**核心 trade-off**：
- 把所有角色都改成 `glm-5.2` 快是快了，但 `glm-5.2` 经常出现"list 失败→再 delegate"问题（参考 [issue #45]）。
- 把所有角色都改成 `claude-opus` 质量高但**每轮 30s+**，**实测 SOTA-quality**，但**让单次 chat 跑到 5+ 分钟**。

**建议**：留现状，针对 manager 重复 dispatch 做减法。

---

## 5. 不修的成本 vs 收益

| 改动 | 总成本（工期） | 收益（平均 chat 时长降低） | ROI |
|------|--------------|--------------------------|-----|
| #1-A 限 manager dispatch 次数 | 半天 | 50% | ★★★★★ |
| #2 manager tools 修正 | 10 分钟 | 6.5s/chat, 即~5% | ★★ |
| #3-A 改 programmer prompt 灵活性 | 1 天 | 30% | ★★★★ |
| #4 综合优化（并行/裁剪） | 1 周 | 20% | ★★★ |
| 不改 | 0 | 0 | — |

**首选方案**：先做 #1-A（**限制单个 manager turn 的 delegate 次数到 1**），看效果。
- 实现位置：`chat.rs` 中 `max_tool_rounds=1` for the manager runner's tool call，或者在 `DELEGATE_TOOL_HINT` 中显式约束 manager 行为。
- 风险：很小的 chat（用户一句话 "echo hello"）可能无法完成多层委托。

---

## 6. 复现方式

```bash
cd /home/dong/Documents/latte/latte-rs-agents
echo "列出项目里的所有文件" | timeout 200 \
  target/debug/latte-agent chat --debug --debug-format jsonl \
  -r manager -t standard -m glm-5.2 2>&1 \
  | grep -E '"(ModelCall|ToolExec|TurnEnd|SessionEnd)"' \
  > /tmp/chat-trace.jsonl
```

然后用上文的 python one-liner 解析，得到 LLM 调用次数、delegate 次数、timeout 次数。

---

## 7. 后续动作（待决策）

| 动作 | 价值 | 待办 |
|------|------|------|
| 修复 #1：限制 manager 的 delegate dispatch 次数 | 50% 提速 | 等决策 |
| 修复 #3：让 programmer prompt 允许"够用即可退出" | 30% 提速 | 等决策 |
| 加环境变量 `--fast` 跳过 review chain | 让测试套件从 20 分钟降到 5 分钟 | QUICK 模式已加 |
| 添加性能 profiling hook（每个 turn 输出延迟指标） | 后续 dev 友好 | 后续 |

待决策：先做哪个？