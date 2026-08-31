# latte-agent

多角色 AI Agent 运行时，支持单角色 REPL 聊天（`chat`）、多角色讨论（`discuss`）、
Web UI 界面（`ui`）、以及 AI 自调试（self-loop），兼容多种后端模型
（Anthropic、OpenAI、DeepSeek、Ollama、Google Gemini 等）。

---

## 快速开始

```bash
# 编译
cargo build --release

# 单角色聊天
latte-agent chat -r manager -t standard

# 多角色讨论
latte-agent discuss --topic "设计 REST API" \
    --roles pm,architect,programmer,reviewer

# 运行工作流
latte-agent workflow design-review --input docs/design.md
```

---

## Web UI

```bash
# 生产模式（先构建前端）
cd latte-agent-cli/ui && pnpm build && cd ../..
latte-agent ui

# 开发模式（hot-reload）
latte-agent ui --dev
```

默认地址：`http://localhost:4567`。

### UI 面板

| 面板 | 功能 |
|---|---|
| Chat | 对话界面，支持角色切换、多角色委派、子会话查看 |
| Trace | 调试事件查看器，浏览 `$LATTE_HOME/traces/` 下的 JSONL 日志 |
| Self-Loop | AI 自调试循环：输入任务描述，AI 自动截图、修改、验证 |
| Role Graph | 角色 × 工具代码关系图 |

### UI 测试

```bash
# 先启动服务
cd ~/Documents/latte/latte-code-editor
LATTE_AGENT_DELEGATE_TIMEOUT_SECS=600 ../latte-rs-agents/target/debug/latte-agent ui

# 另一个终端运行测试
cd latte-rs-agents/latte-agent-cli/ui
pnpm exec playwright test                     # 全部 13 个 e2e 测试
pnpm exec playwright test --grep "TDD"        # 仅 TDD 测试
UI_BASE_URL=http://localhost:5173 pnpm exec playwright test  # vite 开发模式
```

## 配置

三层配置：**CLI 参数** > **项目配置**（`config/agents.toml` + `config/models.toml`） > **全局配置**（`~/.latte/models.yaml`）。

### 模型配置

```toml
# config/models.toml
[models.tiers]
premium  = "claude-opus-4-20250514"
standard = "claude-sonnet-4-20250514"
budget   = "deepseek-chat"

[models.role_tiers.manager]
premium  = "claude-opus-4-20250514"
standard = "deepseek-v4-flash"
budget   = "deepseek-chat"
```

### 角色配置

```toml
# config/agents/manager.toml
[roles.manager]
id = "manager"
name = "Engineering Manager"
category = "planning"
model_tier = "premium"
prompt_file = "prompts/manager.md"
temperature = 0.2
tools = ["delegate"]
icon = "👔"
model_chain = ["deepseek-v4-flash"]
skills = []  # 可添加 screenshot_skill 等
```

`manager` 角色通过 `delegate` 工具派发任务给专业角色——
programmer、architect、reviewer、tester、security、devops、designer、tech_writer、pm。

### 运行参数（环境变量）

| 变量 | 默认 | 说明 |
|---|---|---|
| `LATTE_AGENT_DELEGATE_TIMEOUT_SECS` | CLI 300s / UI 900s | specialist（delegate / workflow step）的 wall-clock 超时，超时即中止并回喂 manager 重派。**注意**：模型目录里的 per-model `timeout_secs` 管的是**流式超时**（首 token / chunk 间隔），与这条 wall-clock 超时是两套独立机制，不存在覆盖关系 |
| `LATTE_AGENT_SLOW_CALL_NOTICE_SECS` | 120 | 单次模型调用慢提示阈值（仅提示，不中断） |
| `LATTE_AI_FIRST_EVENT_TIMEOUT_SECS` | 100 | 首 token（TTFB）超时。reasoning 模型思考久，窗口给得宽。模型目录里的 `timeout_secs` **精确覆盖**它（既能放宽也能收紧，用于给已知秒回的模型配快速失败） |
| `LATTE_AI_IDLE_TIMEOUT_SECS` | 120 | 两个 SSE chunk 之间的最大间隔。只要 token 在流动就不触发，总生成时间无上限。同样被 per-model `timeout_secs` 精确覆盖 |
| `LATTE_AGENT_AUTO_PAUSE_MAX_RETRIES` | 5 | 模型全链不可用时自动暂停后的退避重试次数上限，用尽转人工（0 = 不自动重试，立刻等人点 ▶） |
| `LATTE_MAX_DELEGATES_PER_SESSION` | 0（不限） | 单 session 累计 delegate 调用次数上限 |
| `LATTE_AGENT_READONLY_PARALLEL` | 1（开） | 同一轮里**连续**的只读工具调用（`read` / `code_graph`）并发执行。设 `0`/`false`/`no`/`off` 退回严格串行。同时也是 `read` 批量读的并发开关 |
| `LATTE_AGENT_READONLY_PARALLEL_MAX` | 8 | 只读并发的 in-flight 上限，钳在 `1..=32`（`code_graph` 走 tree-sitter 解析，是 CPU 密集的）。`read` 批量读复用同一个值 |
| `LATTE_AGENT_PARALLEL_TOOL_CALLS` | 1（开） | 下发 OpenAI 协议的 `parallel_tool_calls`，显式声明"一条响应里可以发多个工具调用"。`0`/`false`/`off` → 下发 `false` 强制单调用；`omit` → 字段完全不下发（个别兼容端点不认它，如 litellm #22637 的 Bedrock Converse + Claude 4.5）。Anthropic 默认就允许并行，该路径不下发 |
| `LATTE_AGENT_DELEGATE_PARALLEL` | 0（关） | 同一轮里 ≥2 个 `delegate` 调用并发派发。默认关：子 agent 会写文件，并发有真实竞态风险 |
| `LATTE_AGENT_BLOCKING_CALLS_LAST` | 1（开） | 同一轮里**会停下来等人作答**的调用（`ask`、内部有 `ask` 步的 `workflow`）排到最后执行，同批其他派发调用先跑。设 `0`/`false`/`off` 退回严格按模型给出的下标串行 |

### 减少模型往返（read 批量读）

`read` 接受 `paths` 数组，一次调用读多个文件（上限 10 个），内部并发、按输入顺序回传：

```json
{"paths": ["src/a.c:20-80", "include/b.h", "src/c.c:raw"]}
→ {"files": [ …每项与单文件返回逐字段一致… ], "failed": [{"path": …, "error": …}], "count": 3}
```

- **部分失败不整体失败**：一个路径读不到只出现在 `failed` 里，其余文件照常返回。
- **字节预算** 192KB，按输入顺序累加；超出的文件转进 `failed` 并说明是预算而非文件坏了（第一个文件永远收下）。
- 单路径调用（`{"path": "..."}`）的返回形状**逐字段不变**，不包 `files`。

> **为什么批量读比工具并发重要得多**：实测，programmer 那 594s 里
> 185 次工具调用的真实 I/O 合计只有 **18s**，其余全是 186 次模型往返。把 4 个
> 独立取证并成 1 次调用，省的是 3 次**秒级**往返（还包括重传整个对话历史），
> 而不是 3 次**毫秒级** I/O。工具侧并发（`LATTE_AGENT_READONLY_PARALLEL`）
> 的天花板就是那 18s，作用是"批量之后工具侧不要变成新瓶颈"。

> **只读并发的边界**：任何非只读工具（`bash` / `write` / `edit` / `delegate`）都是**屏障**，
> 它前后的只读调用不会被合并进同一个并发段——`bash "echo x > f"` → `read f` 仍严格
> 按模型给出的顺序执行。记账（死循环探测、`tool_result` 回填、`PermanentExec` 连击
> 熔断）一律按原始调用顺序进行，模型看到的消息序列与串行路径逐字节一致；并发只
> 改变「工具什么时候真正执行」。
>
> 注意这个开关的收益**取决于模型是否批量发调用**。实测会话的实测是 186 轮里
> 185 轮只发 1 个工具调用（`parsed` 长度恒为 1），此时并发无从发生、行为与串行完全
> 相同。让模型批量发依靠 `read` 自身随工具 schema 下发的 description 和
> `paths` 字段说明，而不是角色 prompt；这条批量入口也不依赖模型是否愿意发多个
> 并行 tool_calls，是更可靠的一条路。

> **关于 workflow 的「防挂死」**：曾有一个 `LATTE_AGENT_WORKFLOW_BUDGET_PER_UNIT_SECS`
> 按分派单元数推算 workflow 的 wall-clock 时间上限，跑满即中止。它只看**总时长**、
> 无法区分「后端卡死」与「任务本身就重」，反复误杀持续在产出的健康长任务（如
> 大仓库 explore 单步产出巨量报告），已移除。防挂死现在完全交给**单次模型调用的
> TTFB/idle 流式超时**：后端零字节响应时秒级发现，冷却后沿模型链换下一个模型，
> 全链都哑才升级为 `ModelsUnavailable → 自动暂停等用户`。合法长任务只要一直在
> 产出就不受时限约束。

### 工具循环的终止条件

工具调用循环**没有轮次上限**（deadline-only 模式）。终止靠这四条：

1. 模型返回不带 tool_calls 的回复 —— 正常收尾；
2. `deadline` —— 仅 delegate 与 workflow step 会设置（见
   `LATTE_AGENT_DELEGATE_TIMEOUT_SECS`）。**交互式 `chat` 不设**，超时按设计
   交给人工 ⏸ 或 advisor 角色叫停；
3. **死循环熔断** —— 唯一始终生效的自动刹车，两条规则并行：
   - **连击**：同一工具 + 逐字节相同的参数连续 5 次；
   - **打转**：最近 8 次调用里只有 ≤2 种调用、且每种都重复出现（抓
     `read A → search B → read A → search B …` 这类交替空转——它每次都与
     上一次不同，连击规则抓不到）。

   两条都要求参数逐字节相同，所以「改参数后重试」这种正常进展不会被误杀；
   打转规则还要求每种调用都重复出现，因此「读 → 改 → 读验证」（改的参数
   每次都变）也不会误判。中止时报 `ToolLoopDetected`。
4. 确定性失败熔断 —— 同一工具连续 8 次被输入校验拒绝（参数每次都不同，
   死循环熔断抓不到这种），第 5 次时先追加一条硬指令劝它换路径。

第 3、4 条中止时会把模型**已产出的正文**作为 `partial` 带出来，workflow 与
delegate 据此降级采纳，不会把前面几十轮的成果一起丢掉。

## Skill 系统

Skill 是扩展 Agent 能力的指令模块。在角色 TOML 中声明，运行时追加到 system prompt。

```toml
# 在角色配置中启用 skill
[roles.programmer]
skills = ["screenshot_skill"]
```

### 内建角色

```bash
latte-agent chat -r manager          # 工程经理（PM 驱动多角色）
latte-agent chat -r programmer       # 软件工程师
latte-agent chat -r architect        # 系统架构师
latte-agent chat -r reviewer         # 代码审查
latte-agent chat -r tester           # 测试工程师
latte-agent chat -r security         # 安全审计
latte-agent chat -r devops           # DevOps
latte-agent chat -r designer         # UI/UX 设计师
latte-agent chat -r tech_writer      # 技术文档撰写
latte-agent chat -r pm               # 产品经理
latte-agent chat -r mcp_agent        # MCP 协议 Agent
```

---

## API 参考

Web UI 提供完整的 REST + SSE API，详情见 `docs/api-reference.md`。

| 端点 | 方法 | 说明 |
|---|---|---|
| `/health` | GET | 健康检查 |
| `/api/sessions` | GET | 列出活跃会话 |
| `/api/sessions` | POST | 创建新会话 |
| `/api/session` | GET | 获取会话详情 |
| `/api/roles` | GET | 列出角色 |
| `/api/chat/send` | POST | 发送消息 |
| `/api/chat/command` | POST | 斜杠命令 |
| `/api/chat/role` | POST | 切换角色 |
| `/api/events` | GET | SSE 事件流 |
| `/api/traces` | GET | 列出 trace |
| `/api/traces/:id` | GET | 读取 trace |
| `/api/subsessions` | GET | 子会话日志 |
| `/api/self-loop/start` | POST | 启动自调试 |
| `/api/self-loop/events` | GET | 自调试 SSE |
| `/api/self-loop/stop` | POST | 停止自调试 |
| `/api/role-graph` | GET | 角色工具关系图 |

---

## 调试与可观测

```bash
# 启用调试追踪
latte-agent chat --debug
latte-agent chat --debug --debug-format jsonl

# 调试子命令（只读，不调用模型 API）
latte-agent debug sessions                       # 列出所有会话
latte-agent debug parse "<tool_callbash/>"       # 重解析器
latte-agent debug prompt --role manager          # 构建 system prompt
latte-agent debug session <id>                   # 打印所有事件
latte-agent debug trace <id>                     # 时间线 + 时延
latte-agent debug tokens <id>                    # Token 用量汇总
latte-agent debug replay <id>                    # 重放历史解析
```

### 调试 Hook

```bash
latte-agent chat --debug --debug-hooks redact_pii,enforce_tool_allowlist,require_tool_call
```

| Hook | 作用点 | 行为 |
|---|---|---|
| `redact_pii` | PreCall | 替换出站消息中的手机号、邮箱、API key |
| `enforce_tool_allowlist` | PostParse | 仅允许角色白名单中的工具调用 |
| `require_tool_call` | PostResponse | 如果模型没有实质性输出则中止 |

---

## 架构

```
┌────────────────────────────────────────────────────────┐
│                    latte-agent-cli                       │
│  ┌──────────┐  ┌──────────┐  ┌──────────────────────┐  │
│  │ chat     │  │ discuss  │  │ ui (axum + SSE)      │  │
│  └────┬─────┘  └────┬─────┘  └──────────┬───────────┘  │
│       │              │                   │              │
├───────┴──────────────┴───────────────────┴──────────────┤
│                  latte-agent-core                        │
│  ┌──────────┐  ┌──────────┐  ┌──────────────────────┐  │
│  │ ChatCon  │  │ Agent    │  │ Config (TOML)        │  │
│  │ troller  │  │ Runner   │  │ prompts, models      │  │
│  ├──────────┤  ├──────────┤  ├──────────────────────┤  │
│  │ Session  │  │ Trace    │  │ HookChain            │  │
│  │ Manager  │  │ Sink     │  │ (redact, allowlist)  │  │
│  └──────────┘  └──────────┘  └──────────────────────┘  │
├────────────────────────────────────────────────────────┤
│  latte-agent-orchestrator                               │
│  (多角色调度、RoundScheduler、Supervisor)              │
└────────────────────────────────────────────────────────┘
```

### 运行流程

```
AgentConfig (TOML) ──▶ ModelResolver ──▶ Agent ──▶ AgentRunner
        │                    │                │            │
   roles + models      tier→model       client+tools   run_turn()
                                                     │
                                              TraceSink (Fanout of
                                              [StdoutSink, JsonlSink,
                                               IndexSink])
                                                     │
                                              HookChain (PreCall,
                                              PostResponse, PostParse,
                                              PreTool, PostTool)
```

## 移植（latte-code-editor）

详情见 `docs/porting-guide.md`。`latte-code-editor` 通过 Cargo path 依赖引入 `latte-agent-core`，
用 Tauri IPC 替代 axum SSE，React 替代 vanilla DOM 渲染。

```bash
# latte-code-editor Cargo.toml
latte-agent-core = { path = "../../latte-rs-agents/latte-agent-core" }
```

---

## 工作区结构

```
latte-rs-agents/
├── Cargo.toml                      # workspace root
├── config/
│   ├── agents/                     # 角色 TOML 配置（每个角色一个文件）
│   ├── models.toml                 # 模型层映射
│   └── workflows/                  # 讨论工作流模板
├── prompts/                        # 角色 prompt + skill 文件
├── latte-agent-core/               # 核心运行时
├── latte-agent-orchestrator/       # 多角色调度逻辑
├── latte-agent-cli/                # CLI 二进制 + Web UI 入口
│   └── ui/                         # 前端（Vanilla TS + Vite）
│       ├── src/                    # TypeScript 源码
│       └── __tests__/              # Playwright e2e 测试
├── docs/
│   ├── api-reference.md            # API 文档
│   ├── porting-guide.md            # latte-code-editor 移植指南
│   └── skill-system.md             # Skill 系统文档
└── .latte/                         # 项目级配置（可选）
```

---

## 状态

已完成的里程碑：

- **基础 CLI**：单角色 `chat` + 多角色 `discuss`
- **Web UI**：axum HTTP server + SSE 事件流 + Chat/Trace/Self-Loop/RoleGraph 面板
- **Trace 系统**：`TraceSink`（NullSink / JsonlSink / StdoutSink / IndexSink / FanoutSink）
- **调试子命令**：7 个 `latte-agent debug *` 子命令
- **HookChain**：3 个内建 hook（redact_pii / enforce_tool_allowlist / require_tool_call）
- **HIL Blackboard**：git worktree + plan.md 的多角色 HIL 会话
- **AI Self-Loop**：AI 自动截图 → 修改 → 验证的闭环
- **Skill 系统**：基于 TOML 配置的 skill 加载框架
- **Porting Bridge**：`latte-code-editor` 的 Tauri 集成接口

---

## 构建说明

```bash
# 完整构建
cargo build --release

# 仅核心库（用于 latte-code-editor 等下游）
cargo build -p latte-agent-core

# 前端构建
cd latte-agent-cli/ui
pnpm install
pnpm build

# 运行全部测试
cd latte-agent-cli/ui && pnpm exec playwright test
```
