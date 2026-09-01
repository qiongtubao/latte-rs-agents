# 设计：Advisor 监察者（supervisor）——旁路监听 manager 处理过程并主动介入

> 目标：用户与 manager 对话时，advisor 作为**监察者**旁路监听整个处理过程，
> 发现异常（工具错误、调用格式坏、循环、幻觉/思路错误）时**主动介入**——
> 不等 manager 委派。（manager 主动咨询 advisor 的委派通道仍保留，二者互补。）

---

## 1. 总体结构

```
用户 ──► ChatController ──► manager runner（工具循环 / delegate）
              │  broadcast<ChatEvent>
              ▼
        AdvisorMonitor（新，core 内）
              │  ① 确定性异常检测器（零成本，即时）
              │  ② 触发式 LLM 审查（advisor 角色，premium）
              ▼
   两条介入通道：
   A. 共享 hint 队列（controller 持有，runner drain）→ 合成消息注入 manager 上下文
   B. ChatEvent::RoleTurn{role_id:"advisor"} → UI 聊天气泡（🦉，用户可见，UI 零改动）
```

全部建在现有机制上：monitor 用 `ChatController::subscribe()` 拿事件流；
介入主要走 controller 持有的共享 hint 队列（`advisor_hint()` 直推，
`ControllerInput::AdvisorHint` 为裸通道嵌入方保留）；展示复用 RoleTurn 气泡。

## 2. 事件流可支撑的检测（无需 trace 层）

ChatEvent 广播已含：RoleTurn（manager 流式输出全文）、ToolUse(name+args)、
ToolResult、ToolError、DelegateStarted/Finished、Status、Done、Error。

**确定性检测器 v1**（纯规则，即时，无模型调用）：

| # | 检测器 | 信号 | 介入提示（确定性文案，立即可用） |
|---|--------|------|------|
| D1 | 调用被丢弃 | RoleTurn 文本含 `<tool_call` 计数 > 本轮 ToolUse 计数（turn 结束核算） | "你的工具调用未被解析执行：检查格式 `<tool_call>NAME {json}</tool_call>`，单行、JSON 参数" |
| D2 | 参数非 JSON | ToolUse.args `serde_json::from_str` 失败 | "工具参数不是合法 JSON（不要用 XML 参数块），工具收到的是原始字符串" |
| D3 | 工具连续报错 | 连续 ≥2 个 ToolError | "工具连续报错：先停手诊断（读报错、换路径/命令），不要重复同一调用" |
| D4 | 调用循环 | 同一 (name,args) 连续 ≥3 次 | "检测到调用循环：换思路，不要重试相同调用" |

D1–D4 命中 → **立即**走通道 A 注入确定性提示（不等 LLM，manager 下轮工具循环即见），
同时触发 LLM 审查（见 §3）做 deeper 诊断。

实现备注（v1，与上表的精确语义）：

- **D1 已知限制**：广播流里只有工具循环**最后一轮**的文本（RoleTurn 携带
  `run_turn` 的最终响应），因此"早轮次被丢弃的调用"不可见；主场景——最终答案里
  残留未执行的 `<tool_call>`——能被抓到。
- **D2 截断豁免**：事件槽（`ChatEventTraceSink`）会把 ToolUse.args 截断到 1200
  字符并追加 `...[+NB]` 标记，截断本身会破坏 JSON 合法性；带该标记的 args 跳过
  D2 校验，避免误报。
- **D3 连续性定义**：只被 ToolResult（成功）打断；中间的 ToolUse 不打断（调用序列
  是 Use→Error→Use→Error，若以 Use 重置则 D3 永不触发）。
- **D3 良性探测豁免**：错误文本含 `No such file or directory`（ENOENT）的 ToolError
  不计入 streak，也不打断已有 streak（watched-role D3 与 specialist streak 同规）。
  模型探索代码库时常按惯例猜文件名（README.md 等）与列目录同批发出，猜错即 ENOENT、
  下一轮自愈——这是探索的正常成本而非失控信号（真实事故：两次 ENOENT 触发
  intervene → 全 session 暂停）。
- **幻觉/思路错误**无法规则检测 → 走 LLM 审查。

## 3. 触发式 LLM 审查（advisor 角色本体）

- **触发模式** `AdvisorReviewMode`：`Off | OnAnomaly | EveryTurn`，默认 `OnAnomaly`
  （EveryTurn 每个 manager turn 结束都审，premium 成本，留给愿意烧钱的场景）。
- 输入：**分派动作摘要**（`DispatchDigest`）+ 用户原始问题。摘要只收
  delegate 派发/返回、workflow 启动/结束、工具错误这几类**决策与结果状态**，
  每条 ≤240 字符、最多 40 条，**不含** RoleTurn 全文、也不含 ToolUse 原始参数
  与 ToolResult 原始返回。超出条数上限时 `render()` **显式标注**省略了多少条。
  每个 manager turn 开头清空（惰性：标记后下一条事件才真清，否则 EveryTurn
  模式的审查——它在 `observe()` 返回之后才读——会拿到空摘要）。
- 调用：advisor prompt（`prompts::ADVISOR`，作为 system message）+ 审查指令作为
  user message。**本路径只判分派路由**（简单任务是否被过度编排、复杂任务是否优先
  匹配 workflow、所选 workflow 与角色是否对口、思路是否跑偏、是否反复重试同一分派），
  **不做产出内容的取证**——那是 §3b 委派返回审查的职责，它有完整产出与工具证据。
  输出三态裁决：`ok / warn / intervene` + 理由 + 给 manager 的纠正提示，键名格式：
  `verdict:` / `reason:` / `hint:`。**malformed 输出降级为 `ok`**（无法解析的审查不得注入噪声）。

  > **为什么不再喂原始事件流**：旧实现是 24,000 字符的滚动 transcript，逐条塞进
  > RoleTurn 全文（6K/条）、ToolUse 原始参数、ToolResult 原始返回（1.5K/条）。三个
  > 问题叠加——超预算**静默**丢最老的行且 `render()` 不留痕迹；prompt 标题写「本 turn」
  > 而 `reset_turn_state` 从不碰它、`MonitorState` 整会话只建一次，实际跨会话累积。
  > 于是 advisor 被骗两次（说一轮实为多轮、说完整实为掐头），据此做「过程取证」必然
  > 出错：实测会话里它断言「记录中无成功读取 README，故引用为幻觉」，
  > 而那次读取真实发生过，只是落在被丢弃的那段里。现在改为「只判路由 + 省略可见」，
  > 并在 advisor prompt 里加了硬约束「证据缺失 ≠ 证据为负，缺失时最多 warn」。
- **分级语义（warn / intervene 拉开）**：
  - `warn`（值得关注但不阻塞的问题）→ 只发通道 B 气泡（⚠️，用户可见），
    **不**向 manager 注 hint——轻打扰，用户自己判断要不要管；气泡也不引用 hint。
  - `intervene`（继续下去明显浪费或产出错误结果：幻觉实锤、思路跑偏、关键约束遗漏）
    → 气泡（🛑）+ 通道 A 纠正 hint 注入 manager（重打扰，工具循环中途纠偏）。
  审查 prompt 的判定标准与输出指引按同一语义书写（warn 时 hint 留空，仅 intervene 必填）。
- **项目监察笔记（WATCHDOG.md 式）**：审查 prompt 除通用检查项外，还会拼接项目级
  监察笔记。发现位置按优先级（都可选、缺失/读取失败静默跳过）：
  1. `<session cwd>/.latte/advisor-watchdog.md`（项目级，cwd 即 `ControllerConfig.cwd`）
  2. `$LATTE_HOME/advisor-watchdog.md` 或 `~/.latte/advisor-watchdog.md`（全局级，
     经 `GlobalConfig::global_dir()` 解析）
  内容原样拼接（项目级在前）进审查 user prompt 的 `<attention>` 区块，前置引导行
  "除通用检查项外，特别关注以下本项目监察笔记："。**每次审查现读**（不缓存——文件
  可能开会话后才写）。开关：`AdvisorMonitorConfig.watchdog_notes`（默认 true），
  由 session 创建处经 `AdvisorReviewEngine::with_watchdog_notes(cwd, enabled)` 装配。
- 实现：`AdvisorReviewEngine` 用 advisor RoleTemplate（config 缺失时回退内置模板）
  + `ModelResolver` 取 premium 链，构造 `Agent`（无工具）后以
  **`Agent::chat(WaitPolicy::NoWait)` 单轮调用**。
  ⚠️ 偏离说明：初稿写的是 `AgentRunner::new`（无工具）+ `run_turn`，但 `run_turn`
  硬编码 `WaitPolicy::WaitAndRetry`——advisor 模型失败时会**在 monitor 循环里睡满
  30–60s 冷却**再重试，监控停摆。`NoWait` 走完整条 fallback 链后立即降级，
  monitor 继续监听。审查另有 120s wall-clock 超时兜底。
- 失败（模型不可用/解析失败/超时）静默降级：`tracing::warn`，只留确定性提示，不打断主流程。

## 3b. 委派返回审查（`gate_delegate_return`）

与 §3 的路由审查**并列的第二条 LLM 审查路径**，两者分工不重叠：

| | §3 路由审查 | §3b 委派返回审查 |
|---|---|---|
| 审什么 | manager 的**分派决策** | 专家交回的**产出** |
| 何时 | manager 调 workflow/delegate 时、或检测器命中 | subsession 返回、结果并入主会话之前 |
| 输入 | 分派动作摘要 + 用户原始问题 | 主会话主题 + 被派任务 + 完整结果 + 工具执行摘要 + 角色职责 |
| 入口 | `AdvisorReviewEngine::review` | `AdvisorReviewEngine::review_delegate` |

输入的四件套各有存在理由：

- **主会话主题**（`main_topic`）——「是否符合预期」的准绳。子任务做得再漂亮，
  偏离主诉求也要指出；反之只要在主诉求这条线上交付了被派那部分，就不该因为它
  没顺手做别的事而挑刺。来源：manager delegate 路径取
  `ChatController::last_user_input`（句柄共享、每次调用现读，因为它每轮都变）；
  workflow step 取本 workflow 的 topic（`SpeakerDispatch::main_topic` ←
  `vars["topic"]`，嵌套时是当前这层，它本身派生自父级）。取不到时传空串，prompt
  会显式告知「未记录到原始用户诉求」，禁止 advisor 据此下判。
- **被派任务** 2K / **角色职责** 2K（其系统提示）。
- **完整结果** 32K（`DELEGATE_REVIEW_RESPONSE_MAX_CHARS`）——刻意调到能装下真实
  专家报告（实测 25.5K）。超出时附**显式截断标注**（字符数 + `[+NB]`），并要求
  「仅依据可见内容裁决，不要臆测被截断部分」。
- **工具执行摘要**（引擎侧记录）——不可省。实测实锤：interview step 用 `ask`
  弹窗收齐 4 个答案后输出 user_profile，advisor 看不到 `ask` 的执行记录，误判
  「伪造答案」→ intervene → 带反馈重做 → 用户被重复提问。

裁决语义与 §3 相同，另多一个 **`remedy: patch | restart`** 键（仅 intervene/
terminate 时填，缺省 patch）：advisor 必须**优先选 patch**——workflow 引擎收到
patch 时**复用同一 subsession 的 runner 续作修补**（上下文里保留着全部已完成的
探索与工具取证，只需追加一轮修订 turn），只有产出基于虚构事实、方向根本错误、
上下文被污染到「续作不如重来」时才选 restart（废弃本轮分派，全新 runner 重做，
整轮工具调用作废重烧）。选型动机见 `workflow.rs` `run_step_speaker` 的
`held_runner` 注释（实测实锤：46 万 input tokens 的探索型分派被 intervene 打回后
整轮重跑）。`intervene`/`terminate` 时除气泡外还会把审查批注追加进 manager
消费的 payload。advisor 不可用或审查超时（`delegate_review_timeout`，可配）→
**静默降级**，原样放行专家产出。

## 4. 注入机制（core 改动点）

1. `ControllerInput` 新增 `AdvisorHint(String)`；`ChatController::advisor_hint(text)`
   公开方法。
2. **实际投递通道是 controller 持有的共享内存队列**（⚠️ 对初稿的修正）：
   `ChatController` 持有 `Arc<Mutex<VecDeque<String>>>`，`advisor_hint()` **直接
   push 进队列**（同步、无锁竞争以外的延迟）；driver 在 `spawn` 时把同一个 `Arc`
   交给 `run_driver`，后者为每个 runner 装配 `AgentRunner::with_advisor_hints(...)`。
   初稿设想的"mpsc → run_driver 收到后推入 runner 队列"在单角色模式下**无法做到
   工具循环中途送达**——driver 在 turn 期间阻塞在 `run_turn().await` 里，mpsc 消息
   要等 turn 结束才被读取。`ControllerInput::AdvisorHint` 变体仍保留：只持有裸输入
   通道的嵌入方（如未来的 Tauri adapter）可经 driver 把 hint 推进同一队列（空闲时
   段路径）。
3. `run_turn` 的 drain 点（`drain_advisor_hints`）：
   - **turn 开始**：构建工作消息列表之前 drain 进 `ConversationContext`，首个模型
     调用即见；
   - **每个工具轮次边界**（`for round` 循环顶部，round > 0）：drain 出的 hint 追加进
     当前工作消息列表（合成 user 消息，前缀 `🦉 advisor 监察：`），下一次模型调用
     ——工具循环中途——即携带。drain 同时也会把 hint 记入 context（跨 turn 留痕；
     中途 drain 的 hint 在 context 里排在当前用户消息之前，时序略有出入，前缀本身
     已表明其为注入批注，v1 接受）。
   - 空闲等待输入时 → hint 停在队列里，并入下一条用户输入的 turn（不单独触发 turn）。
4. `AdvisorMonitor::spawn(controller, config, review_engine, watched_role)`：core 提供
   （`advisor_monitor.rs`）；`ControllerConfig` 加
   `advisor_monitor: AdvisorMonitorConfig`，ui-server 的
   `create_session_handle` 在 `enabled` 时装配（订阅 broadcast + 共享
   `Arc<ChatController>`：hint 走 `advisor_hint()`，气泡走 `event_sender()`）。
   monitor 任务在 `ChatEvent::Done` 或 broadcast 关闭（controller drop）时退出。
5. 用户当前问题的来源：ChatEvent 广播流**不含**用户输入，故
   `ChatController::submit_input` 记录最近一条非斜杠命令输入
   （`last_user_input()`），monitor 审查时读取。
6. monitor 自身容错：检测/审查错误、模型不可用、审查超时（120s）→
   `tracing::warn`，绝不影响主会话；monitor 忽略 `role_id == "advisor"` 的自身
   气泡，避免自反馈循环；delegate 专家的 RoleTurn（`sub_id.is_some()`）既不参与
   检测、也不进分派摘要——专家产出的把关由 §3b 委派返回审查负责。

## 5. 配置与成本控制

- `AdvisorMonitorConfig { enabled: bool = true, review_mode: AdvisorReviewMode = OnAnomaly, max_reviews_per_turn: u32 = 2, watchdog_notes: bool = true, gate: GateConfig, review_settings: AdvisorReviewSettings }`
  （`AdvisorReviewMode::{Off, OnAnomaly, EveryTurn}`；防异常风暴反复烧 premium）。
  `Default` 实现即开启；本 workspace 唯一的 `ControllerConfig` 构造点
  （ui-server `create_session_handle`）使用 Default——默认开。
  `gate`（D5/D6 阈值 + `max_retries = 2`）经 `runner_gate()` 传给 driver：
  advisor 启用时 driver 给每个 runner（含 delegate specialist）装
  `with_gate_config`，`run_turn_gated` 在产出被接受前跑
  `check_response_gates`；未启用时 runner 不带 gate，
  `run_turn_gated` 等价 `run_turn`。
  `review_settings`（`delegate_review_timeout_secs = 90` +
  `return_max_redo = 1`，硬上限 3）在 `runner_gate()` 注入 gate 副本，
  随既有 plumbing 流到各 delegate-return 审查 engine：前者是
  `gate_delegate_return` 的单次审查超时（实测实锤：45s 硬编码对
  20–44s 延迟的慢审查模型太紧，频繁「未审直接放行」），后者是
  workflow speaker 返回被判 intervene/terminate 时的重做上限。
- 确定性提示每 turn 每种检测器最多一次（`fired` 集合去重，turn 结束重置）。
- 每 turn LLM 审查次数 ≤ `max_reviews_per_turn`，超限 `tracing::warn` 跳过。
- hint 队列积压上限 16 条（`advisor_hint` 超限时丢弃最旧），防止异常风暴
  无限堆积。
- 成本控制补充：warn 裁决只发气泡不注 hint（轻打扰）；只有 intervene 会打扰
  manager 的工具循环。

## 6. v1 范围与后续

**v1（本次）**：monitor 框架 + D1–D4 + OnAnomaly LLM 审查 + 两条介入通道 +
项目监察笔记（advisor-watchdog.md）+ warn/intervene 分级 + 单测。
**v2**：EveryTurn 审查 UI 开关（topbar 🦉 toggle）；advisor 专用气泡样式（区别普通消息）。
**v3（已落地）**：
- LLM 复审 `Verdict::Terminate` → monitor 调 `ChatController::cancel_turn()`
  取消 manager 的 in-flight turn（**软终止**：driver 回到等用户输入，不是
  abort session），并广播 `ChatEvent::AdvisorTerminated { detector: Some("LLM") }`。
- ~~LLM 复审 `Verdict::Intervene` → pause gate 暂停门~~（v4 移除，见下）。

**v4（已落地）**：intervene 语义改为「**纠正并继续**」——只发气泡 + 注入纠正
hint（manager 下一个 tool-round 边界 drain 后自愈），不再置位暂停门、不再弹
ChoiceRequested 拍板窗、主会话全程不停。动机：v3 的暂停门多次打在健康运行上
（良性工具报错 streak、gate 按设计拒绝 workflow 等），每次误停都要人工解锁，
代价远大于收益。`AdvisorPauseGate` 结构与 runner 侧接线保留（休眠）；恢复暂停
语义只需在 monitor 的 intervene 分支重新调 `request_pause()`。保留暂停能力的
唯一路径是「模型不可用」自动暂停（`agent.rs pause_wait_model_unavailable`，走
AgentPauseGate，与本 monitor 无关）。
- D5/D6 pre-persistence gate 接入生产 driver：`build_runner` 按
  `AdvisorMonitorConfig::runner_gate()` 给 manager / 多角色 / delegate
  specialist runner 装 `with_gate_config`；gate 命中 → 带批注重试（最多
  `max_retries` 次）→ 重试通过 = 纠偏成功正常继续；重试耗尽 →
  `AgentError::AdvisorTerminated` 上抛，driver 发
  `ChatEvent::AdvisorTerminated`（`detector: Some("D5"/"D6")`，subsession
  带 `sub_id`）而不是 RoleTurn，坏答案不落盘。

**后续**：幻觉检测增强（ToolUse 历史 vs 结论交叉验证）。

## 7. 测试

已实现的测试（全部在 `latte-agent-core`，`cargo test -p latte-agent-core`）：

- 检测器单测（`advisor_monitor::tests`，合成 ChatEvent 序列直接驱动
  `MonitorState::observe`）：D1–D4 各自触发/不触发、每 turn 去重、下一 turn
  重新武装；角色过滤（他角色与 advisor 自身事件忽略）、专家 RoleTurn 不进分派
  摘要、error 路径的 turn 结束重置。
- 分派摘要（`DispatchDigest`）：超限时显式标注省略条数并提示「别把没看到当没发生」、
  未超限不挂噪音、单条过长带 `[+NB]` 标记、惰性重置（标记后下一条事件才清）。
- AdvisorHint 注入：
  - runner 级（`agent::tests`）：turn 开始 drain（首个请求即见 hint + context
    留痕）；工具循环中途 drain（wiremock 恒定返回 tool_call + 工具 handler 推
    hint，断言第 2 个模型请求已携带 `🦉 advisor 监察：`）。
  - driver 级（`controller::tests`）：完整单角色 driver + wiremock manager，
    断言空闲时 advisor_hint 不触发新 turn、下一条用户输入的上下文携带合成消息。
- LLM 审查（`advisor_monitor::tests`，wiremock 作 advisor premium 模型）：
  裁决解析单测（ok/warn/intervene/大小写/多行 section/malformed→ok）；
  全链路（D3 触发 → 确定性 hint 立即入队 → wiremock 裁决 intervene → 🛑 气泡
  + 纠正 hint 入队；断言审查请求包含触发证据/分派摘要/用户问题）；
  **分级**：warn → 只气泡（不引用 hint、队列仍只有确定性 hint），intervene →
  气泡 + 注 hint；`max_reviews_per_turn=1` 时两个检测器只审一次。
- 监察笔记（tmp 目录构造文件，LATTE_HOME 重定向 + ENV_LOCK 隔离）：
  项目级内容进 `<attention>` 区块（含引导行）；项目+全局并存时项目级在前；
  文件缺失 → 无 attention 区块且审查照常；`watchdog_notes=false` → 不读文件。
- monitor 容错：空 model catalog（resolve 失败）→ 确定性 hint 照发、无气泡、
  monitor 存活继续检测（后续 D4 正常触发）。
- v4 intervene 行为（`monitor_intervene_corrects_without_pausing`，wiremock
  裁决 intervene）→ 🛑 气泡（含 hint）照发、纠正 hint 入共享队列，
  且**不**置暂停门、**不**弹 ChoiceRequested、**不**发暂停 Status。
- v3 pause gate（休眠中的基础设施，仅 gate 级单测）：
  - gate 单测：未置位立即返回；置位挂起直到 resolve；短超时自动恢复并清旗。
  - runner 级（`agent::tests`，wiremock 恒定 tool_call + 工具 handler 置位
    gate）：挂起期间无第二次模型调用，resolve 后 tool 循环继续。
  - controller 级：任何用户输入 resolve；命中"终止"关键词同时 `cancel_turn`；
    `resume()` 也算拍板。
