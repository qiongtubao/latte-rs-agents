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
| `LATTE_AGENT_DELEGATE_TIMEOUT_SECS` | CLI 300s / UI 900s | specialist（delegate / workflow step）的 wall-clock 超时，超时即中止并回喂 manager 重派。模型目录里的 per-model `timeout_secs` 优先级最高 |
| `LATTE_AGENT_SLOW_CALL_NOTICE_SECS` | 120 | 单次模型调用慢提示阈值（仅提示，不中断） |
| `LATTE_AGENT_AUTO_PAUSE_MAX_RETRIES` | 5 | 模型全链不可用时自动暂停后的退避重试次数上限，用尽转人工（0 = 不自动重试，立刻等人点 ▶） |
| `LATTE_AGENT_WORKFLOW_BUDGET_PER_UNIT_SECS` | 420 | workflow 时间预算的「每分派单元」秒数。预算只计**有效工作时间**，暂停期间不扣 |
| `LATTE_MAX_DELEGATES_PER_SESSION` | 0（不限） | 单 session 累计 delegate 调用次数上限 |

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
