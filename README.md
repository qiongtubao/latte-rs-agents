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

#### Google Gemini（OpenAI-compatible）

`latte-ai` 当前没有独立的 Google wire protocol。Gemini 必须通过 Google 官方
OpenAI-compatible endpoint 配置；`api` 表示 wire protocol，因此应写
`api = "openai"`，**不要**写 `api = "google"`：

```toml
# config/models.toml
[[models.models]]
# name 同时是 tier 引用和请求中的 model 字段；按 Google 当前可用模型调整。
name = "gemini-2.5-pro"
api = "openai"
provider = "google"
base_url = "https://generativelanguage.googleapis.com/v1beta/openai"
api_key = "${GEMINI_API_KEY}"
context_window = 1048576  # 按所选模型的实际限制调整
max_tokens = 65536        # 按所选模型的实际限制调整
```

运行时会在该 `base_url` 后请求 `/chat/completions`，最终路径为
`/v1beta/openai/chat/completions`。如需通过 tier 使用它，请把现有
`[models.tiers]` 或 `[models.role_tiers.<role>]` 的值改为同一个 `name`；不要在
同一 TOML 文件里重复声明 `[models.tiers]` 表。

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
| `LATTE_AGENT_AUTO_REMEDIATE_MAX` | 2 | 单 session 累计 **workflow 失败自动善后**次数上限。workflow 失败后引擎会合成一条 `[自动善后 N/M]` 输入喂回 manager 让它自救（resume 或换路径）；额度用尽后**不再自动重跑**，停下来发一条带可执行选项的提示等用户拍板。`0` = 失败即刻转人工。实测事故：此前这个外层环没有计数，同一个目标反复整条重跑了 3 次（其中一条 3.56M 输入 token 完全打水漂），而 UI 上只显示"又一条 workflow 开始跑" |
| `LATTE_AGENT_CONTEXT_COMPACT` | 1（开） | 工具循环内**主动压缩陈旧工具结果**。工具循环每轮都重传全部历史，第 N 轮要把前 N-1 轮的工具结果原文再发一遍——实测一个子会话 70 次调用、返回内容合计 ≈47K token，那一个 turn 烧了 1.79M 输入 token（38 倍）。设 `0`/`false`/`no`/`off` 退回旧行为（全部逐字重传） |
| `LATTE_AGENT_CONTEXT_KEEP_RECENT` | 6 | 保留**逐字原文**的最近工具结果条数，钳在 `1..=64`。更早的压成头 + 尾 + 标记 |
| `LATTE_AGENT_CONTEXT_STALE_CHARS` | 900 | 陈旧工具结果的压缩预算（字符），钳在 `200..=8000`。低于 `预算 × 4/3` 的结果不动（省不下 1/3 不值得压）。调小省得更多，但模型认不出自己读过什么就会重读，**一次重读 = 一整轮往返**（含重传全部历史），比多留几百字符贵得多 |
| `LATTE_AGENT_AUTO_COMPACT` | 1（开） | **自动上下文压缩**：占用达阈值时把旧历史摘要成一条 checkpoint 并替换掉。与 `LATTE_AGENT_CONTEXT_COMPACT` 分工不同——后者只缩短工具结果**文本**、不减消息条数（实测覆盖约 30% 内容、把 105 倍放大降到 90 倍）；本项真正**减少历史长度**，让长会话能一直续下去而不是撞窗口。设 `0`/`false`/`no`/`off` 关闭 |
| `LATTE_AGENT_COMPACT_THRESHOLD_RATIO` | 0.8 | 触发压缩的上下文占用比例（占 `context_window`），钳在 `0.5..=0.95`。留 20% 余量给"本轮还要产生的输出 + 工具结果"，否则刚压完又撞窗口。目录没配 `context_window` 时**恒不触发**（没有分母就没有压力，宁可不压也不按猜的窗口误压） |
| `LATTE_AGENT_COMPACT_KEEP_RECENT_TURNS` | 2 | 保留**逐字原文**的尾部轮数，钳在 `1..=32`。一轮 = 一条 user 消息起、到下一条 user 前为止。切点会自动避开 `tool_call ↔ tool_result` 配对（切在中间会产生孤儿 tool_result、被 provider 直接拒），前导 System 永不进入压缩区间 |
| `LATTE_AGENT_COMPACT_SUMMARY_MAX_TOKENS` | 2048 | 摘要输出的 token 上限，钳在 `256..=16384`。摘要是结构化清单，给太大反而鼓励模型把原文抄进去、失去压缩意义 |
| `LATTE_AGENT_COMPACT_MAX_PASSES` | 1 | 单次检查内最多连续压几遍，钳在 `1..=3`。防"压不动还一直压"——每次压缩都是一次真实 LLM 调用 |
| `LATTE_AGENT_COMPACT_SECTIONS` | （8 段默认模板） | 自定义摘要模板段落，用 `\|` 分隔（如 `"Goal\|Progress\|Next"`）。默认 8 段取自 deepseek-harness：Primary Request and Intent / Key Technical Concepts / Files and Code / Errors and Fixes / Pending Jobs / Current Work / Next Step / Critical Context |

#### 压缩的两种触发

| 触发 | 判据 | 用途 |
|---|---|---|
| **pressure** | 轮次入口检查 `占用 / context_window >= threshold_ratio` | 常规。依赖模型目录里声明的 `context_window` 是对的 |
| **context-overflow** | 供应商**真的报了**「超出上下文窗口」 | 兜底。**跳过阈值判断**，不看声明值 |

第二种是必需的，因为第一种的判据里 `context_window` 是**配置声明的值**：
声明写大了阈值就永不触发。实测事故——jemalloc 的三个模型全写
`context_window = 1000000`（阈值 0.8 → 800K），7 个会话里 `ContextCompacted`
**一次都没出现**，主动压缩形同不存在。被动触发只认「请求被拒」这个事实，
不依赖配置正确性。

溢出识别覆盖各家 OpenAI 兼容端点的措辞（`context length exceeded` /
`maximum context` / `input too long` / `context_length_exceeded` / 中文
「上下文长度超出」等）。判据故意收紧成「超限动作词 + 上下文名词」的组合：
误判的代价是白烧一次摘要调用还压掉好历史，所以宁可漏判、退回原错误。

被动触发时按 **真压缩 → 兜底瘦身** 两级处理：先把旧历史摘要成 checkpoint
（真正减少消息条数），压不动（历史太短、或摘要调用也失败）时至少把超大
单条消息裁短。被动触发只压一遍就交回重试——真实窗口未知，多压也无从判断
够不够，让重试结果说话。

> **压缩治不了工具循环内的放大**：自动压缩挂在 **turn 入口**（放进工具
> 循环里替换历史容易切断 `tool_call ↔ tool_result` 配对）。而 22.7 倍放大
> 发生在**单个 turn 的工具循环内部**——第 N 次调用要把前 N-1 次的历史再发
> 一遍。对这个问题唯一的杠杆是 **prompt caching**（让重传变便宜），压缩解决
> 的是另一件事：长会话别撞窗口。

#### 压缩参数也可写在配置文件里

上面 6 个环境变量都能改成 `[compaction]` 配置段，写在任意 agents 配置文件
（项目 `.latte/agents.d/*.toml` 或全局 `~/.latte/agents.d/*.toml`）：

```toml
[compaction]
enabled = true
threshold_ratio = 0.8       # 占 context_window 的比例，钳 0.5..=0.95
keep_recent_turns = 2       # 保留逐字原文的尾部轮数，钳 1..=32
summary_max_tokens = 2048   # 摘要输出上限，钳 256..=16384
max_passes = 1              # 单次检查最多压几遍，钳 1..=3
sections = ["Goal", "Progress", "Next Step"]   # 自定义摘要模板段落
```

**优先级：环境变量 > 项目配置 > 全局配置 > 内置默认**。环境变量留在最高
优先级，是为了保留「临时压测某个参数不用改文件」的能力。每个字段都是可选的
——只写想改的那几项，其余走下层/默认；越界值一律钳到安全区间，配错不会把
行为推到极端（阈值 0 每轮都压、保留 0 轮丢上下文）。

**动态生效**：配置段随 `ModelResolver` 快照热更新（与角色 model_chain 同款
机制），改完保存后**已加载的会话下一轮即生效**，不必重开 session。

**UI 面板**：Web UI 的 Models 面板里有「🗜️ 自动上下文压缩」折叠块，可直接
改参数并选择「保存到项目 / 保存到全局」，保存后同样是已加载会话下一轮生效。
被环境变量接管的字段会标灰并提示（env 优先级最高，在面板上改不会生效）。
对应端点 `GET/PUT /api/compaction`（见 `docs/api-reference.md` 9c 节）。
| `LATTE_AI_PROMPT_CACHE_KEY` | 1（开） | 是否下发顶层 `prompt_cache_key`（**缓存亲和键**）。与模型目录里的 `prompt_cache` 是**两条独立机制**：后者声明「缓存边界在哪」（Anthropic 式 `cache_control`，塞进 OpenAI body 容易 400，所以默认关）；本项只声明「这些请求属于同一会话」，走供应商的自动前缀缓存。命中自动缓存要求请求落到**持有那份 KV cache 的机器**上，网关的负载均衡不保证这点，这个键就是给它做路由亲和的。键取 session id（整个会话内稳定）。个别对未知顶层字段严格 400 的端点用 `0` 关，或在模型目录里写 `prompt_cache_key = false`。实测事故：jemalloc 会话三个模型全是 `api = "openai"` 且都没开 `prompt_cache`，22.19M 输入 token **零缓存命中** |
| `LATTE_AGENT_ADVISOR_REVIEW_TIMEOUT_SECS` | 90 | advisor 对 delegate/workflow 返回做**单次审查**的超时（秒），钳在 `10..=600`。超时按「未审直接放行」降级，所以这是「一次审查值多少钱」的预算。实测事故：默认 90s 撞上常态 >120s 的慢模型，一个会话里 4 次静默放行——审查门禁形同虚设。也可写 `[advisor] delegate_review_timeout_secs`；若审查和干活共用慢模型池，要么调大，要么给 advisor 单独配快模型 |
| `LATTE_AGENT_ADVISOR_RETURN_MAX_REDO` | 1 | workflow speaker 的返回被 advisor 判 intervene/terminate 时的**重做上限**（每次重做都是全新 subagent），钳到硬顶 3。也可写 `[advisor] return_max_redo` |
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

### edit 锚点失败的两级升级

`edit` 的确定性失败（锚点找不到 / 命中多处 / 快照过期）带**失败码**
（`EDIT_ANCHOR_NOT_FOUND` / `EDIT_AMBIGUOUS` / `EDIT_STALE_TAG` /
`EDIT_BAD_ARGS`），据此做三件事：

1. **不重试**。同一份输入第二次不可能成功。此前这些失败没命中错误分类器
   那张中文子串黑名单，被归成「瞬时执行错误」自动重跑一遍——实测一个
   `ToolUse` 对着**两条**完全相同的 `ToolError`，4 次失败白烧 4 次往返。
2. **报文自带补救动作**：不只说哪里错了，还说下一步干什么（「先 `read`
   重读该文件，按当前内容与返回的 tag 重写锚点」）。实测模型连续 4 次
   只收到「找不到匹配」，就一直靠猜改锚点，从没想到该重读。
3. **按文件累计计数**：同一文件连续 3 次锚点类失败后，从软提示升级为
   硬性「换策略」指令（停止重发、给出重读/放弃两条明确出路）。对该文件
   成功 `read`/`edit`/`write` 即清零。

> **为什么现有的死循环熔断抓不到它**：熔断按「工具名 + 逐字节相同的参数」
> 计数（连击 5 次）或「最近 8 次里 ≤2 种调用且各自重复」（打转）。模型猜
> 锚点时**每次 `old_text` 都不一样**，两条规则一条都不触发。所以这里按
> **语义类别**计数，而不是按参数指纹。

另外，`edit` 的布尔参数（`all` / `delete` / `insert_after` /
`insert_before`）接受字符串写法（`"true"` / `"1"` / `"yes"` / `"on"`）——
实测部分模型就是这么发的，只认 JSON 布尔会静默判 `false`，然后报「请显式
传 `all: true`」，而模型认为自己已经传了，于是原地打转。宽容解析的同时会
往返回体的 `warnings` 里记一条，避免用宽容掩盖模型的类型错误。

## 工具说明（tool docs）

模型在 schema 里读到的工具说明 = **基线描述**（代码里写死）+ **项目/全局 md**
（`.latte/tools.d/<工具名>.md`）。md 首行是一句话简介，其后是细则；可以用
`{{#if has_eval}}…{{/if}}` 按当前会话真实可用的工具改写内容。

在 Web UI 的 Tools 面板里直接编辑：保存后**就地刷新所有活跃会话**
（返回体里的 `refreshed_managers` 就是刷新了几个），不必重开 session。
外部 MCP server 的工具连上后会注册成一等工具，同样能写文档、能在面板里管理。

详见 `docs/tool-docs.md`。

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
| `/api/tools` | GET | 工具目录（内置/动态/别名/外部 MCP + 文档状态） |
| `/api/tools/:id/doc` | GET/PUT/DELETE | 工具的模型侧说明（支持模板，保存即热更新） |

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
