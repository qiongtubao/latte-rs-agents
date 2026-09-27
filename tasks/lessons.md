# Lessons

## Workflow configuration scope

- `task_refine` runtime resolution checks `<project>/.latte/workflows.d/<name>.toml` before `$LATTE_HOME/workflows.d/<name>.toml`; changing the global file alone does not affect a project that has a same-named local override.
- When auditing a session, verify both the intended configuration scope and the loader precedence before attributing stale behavior to a failed code fix.
- Global workflow changes belong in `$LATTE_HOME/workflows.d/`; do not write them into an unrelated project repository unless the user explicitly requests changing project-local precedence.

## 规则与工具校验必须同一套判据（实测会话）

- **prompt 里的规则不能和工具的机械校验用不同语义**。`task_refine` 旧规则同时要求
  「paths 必须包含验收标准点名的所有文件」和「共读同一文件时改成消费上游子任务的交付物」
  —— 而"消费上游交付物"本身就要把上游 doc 路径写进自己的 paths，正好踩中前一条禁令。
  planner 在**逻辑层**判"读不算冲突"、plan 工具在**路径字符串层**判"出现两次就是重叠"，
  六个链式子任务撞出 14 处重叠，整单被拒、零任务入库。规则给的出路就是规则的违规项时，
  模型无论怎么写都过不了。
- **豁免通道不能只写在工具的 schema description 里**。`is_readonly_plan_task` 的
  「labels 加 `只读` 即跳过重叠校验」出口只存在于 plan 工具的 input_schema，而
  refine/revise 两步用 `tools = [...]` 把 plan 摘掉了 —— 写草案的角色从头到尾看不到这个
  出口，唯一看得到的 submit 步又被「禁止增删改任务内容」锁死。**谁要遵守约束，约束就得
  出现在谁的 prompt 里。**
- **评审步给的修正建议必须自己先过一遍同一套机械校验**。reviewer 要求"把 `src/arena.c`
  补进 L2.4 的 paths"，而 L2.5 已经持有它 —— revise 照单执行，亲手造出第 14 处重叠，
  而 gate 排在 revise 之前，没有任何人复核修补产物。
- **「工具调用失败 → 改口输出文字报告」会把失败洗成成功**。submit 步的 plan 被拒后模型
  输出了一份格式完美的「请指示走 A/B/C」，纯字符串 `output_contract` 看不出问题 →
  step 通过 → `WorkflowFinished: status ok`，而看板上一个任务都没有。凡是"这一步的价值
  在于某个副作用（提案入库 / 文件落盘 / 状态推进）"的 step，都必须校验**副作用本身**，
  不能只校验正文（现已有 `WorkflowStepDef::require_plan_submit`，判据取 plan 工具的
  `PlanStage` 是否推到 `PendingApproval`；这条校验刻意**不走** advisor 语义兜底 ——
  "零任务入库"没有语义解释空间）。
- 机械校验（可自动判定、修法唯一）被拒时应授权**受限自愈**而不是回来问人：submit 步现在
  可以只改 `paths` / `labels` 后重提（`title`/`description`/`priority` 仍锁死）。
  卡在这里问人的代价是整轮拆分白跑。

## 事件流订阅时序（UI）

- ChatEvent broadcast **没有回放**：只有挂起弹框会在建连时补发。任何「先拉 history 再订阅
  SSE」的顺序都会永久丢掉落在两步之间的事件 —— 必须「先订阅（缓冲）→ 拉 history →
  回放 → 去重 flush 缓冲」（`main.ts::replayHistoryAligned` +
  `history_merge.pendingAfterHistory`）。
- 任务看板的 refine/dispatch 在 **HTTP 响应之前**就 spawn 了 workflow，首批事件
  （WorkflowStarted/Step/DelegateStarted/RoleStarted）落在 session 创建后 ~2ms 内，
  前端那时还没拿到 session_id。所以「后台已开跑的 session」是这条时序的必测场景。
- subagent 的 ToolUse/ToolResult 带 `sub_id` 时不进主对话正文（详情面板才有）。单个 step
  跑几分钟时主对话必须另给心跳信号，否则用户只能判断为卡死。

## 契约类流程（workflow）的修正项必须有回写通道

- 如果 gate 放行门槛是「无 blocking」，那 important 就必须有一个**真的会改草案**的下游
  步骤（`revise`），且 submit 要消费修订后的产物。否则 reviewer 写的「必须修正后再提交」
  永远不会被执行。
- prompt 里不要写「用户会在弹窗里看到并自行取舍」这类未经验证的免责说法：`PlanProposed`
  事件只带 tasks，导入弹窗不展示评审结论。虚假免责会让 gate 心安理得地放行带病产物。
- ~~任务看板的任务模型没有依赖字段~~（已过时，2026-09-16 更正）：`depends_on` 已实现
  （controller.rs `PlanTask.depends_on`，带 `deps`/`dependencies` 别名），看板语义是
  「依赖全部进入终态后才可派发；空 = 与同族兄弟并行」。拆分 prompt 应要求真实先后依赖
  写进 `depends_on`——写在 description 里的「依赖 X」派发逻辑读不到，`priority` 只是
  排队顺序、不构成阻塞。学习/课程类拆分的先修顺序尤其要落进 `depends_on`。

## plan 工具：数组之外的顶层键会静默丢任务（2026-09-05 jemalloc 学习拆分会话）

- 模型提交 `{"tasks":[1 个], "item":{…其余 21 个任务…}}` 时，handler 只读
  `tasks`，顶层游离键**无任何回执**——弹窗只弹 1 个任务、plan 门禁却已锁死，
  模型还以为 22 个都提交了，再补交又被「一次一清单」门禁挡住，只能求用户关
  弹窗。教训同「字段名写错 → 数据静默消失」：宽容解析必须配显式回执或硬拒
  绝。修复：顶层非 `tasks` 键**整单拒绝并点名**（不进 PendingApproval，模型
  可同轮合并后重提）；schema 描述里也写明"所有任务必须放进 tasks 数组"。
- 跨仓库任务：plan paths 的相对路径按**会话工作目录**校验，任务针对其他仓库
  时相对路径会被误判成幻觉路径——必须用绝对路径（报错信息里已写明这条出路）。

## 学习类拆分要用专用 workflow（2026-09-17 jemalloc 书目录 e2e，15 轮实录）

- **符号回指门禁（require_symbols_resolvable）套在「书目录」上是打地鼠**：目录章名
  引用的是模块/主题，不是符号，模型每轮重写都从训练记忆漏进新的旧版符号
  （`extents_pa_alloc`）和占位符（`je_xxx`、`README`）——实测 42→7→19→1→21 个
  未命中，5 轮不收敛。学习类拆分走专用 `learn_plan` workflow：章锚定真实文件路径
  （require_plan_tasks 校验存在性），符号级取证留给每章 learn_loop 的 plan 步。
- **运行时读 `.latte/workflows.d/` 和 `<项目>/prompts/`，不是 `config/`**：改了
  `config/workflows/*.toml` 必须同步到本仓和目标仓库的 `.latte/workflows.d/`；
  prompt 同理（否则角色用二进制内置的旧 prompt）。测试曾因此白跑一轮。
- **单次 20KB+ 的任务清单 JSON 必撞输出上限截断**：紧凑格式（不缩进）+ 规模上限
  （章 12-50 个、description ≤60 字、验收一句话）才稳定。
- **评审/门步不给 `tool_call_limits` 会全量核验**：实测 gate 对着一份截断的 verdict
  逐条核验烧了 4 小时工具调用；verdict 自己的输出预算也被工具阶段耗尽（产出截成
  115 字符）。评审是抽查（3-5 处），不是复读。
- **模型空输出 `{}` 是高频抖动**：评审/门步 max_retries ≥ 2；gate 把「终审结论缺失」
  判 REJECT 触发重做是正确兜底，不要放过空 verdict。
- **depends_on 必须写进 tasks JSON 的 schema 示例**（含取值说明=被依赖任务标题原文），
  只在规则文字里提，模型不会输出这个字段。
- **章节号顺序 = 阅读顺序 = depends_on 拓扑序**要写进 prompt，否则会出现「第 29 章
  依赖第 35 章」这种编号倒置。
- **学习类任务 paths 宁空勿猜**：labels 带「学习」「只读」即豁免重叠校验，执行由
  learn_loop 现场取证；凭印象写 test/ 路径（test/unit/bin.c）会被路径校验反复打回。
- TOML 单行双引号字符串里嵌 ASCII 双引号会直接破坏语法——prompt 文本里的强调
  一律用「」。

## LAT-110 事故：proofreader 通读大文档撞穿 prompt_token_budget（2026-09-25）

- **根因链**：proofreader 必须通读 146KB/1,891 行的 `draft.md`；`read` 工具
  每次范围读同时回 `content`（无行号）和 `numberedContent`（带行号）两份
  同段正文（2.15× 体积），且 `maxSize` 默认 10MB → 7 次读累计 313KB，按
  `Σ字节/4` 估算 78,387 est > `prompt_token_budget = 60,000` 硬上限。
  工具结果压缩只豁免「最近 6 条」→ 7 条读取全部豁免，压缩无事可做；
  `context_overflow_restart` 重启后同一份文件必复现同样负载 → 2 次重试耗尽 →
  workflow 失败。
- **修复 P0**：`read` 工具去掉 `numberedContent`（全仓零消费者），单次读
  上限 50KiB、行边界截断、note 给出可粘贴续读选择器；同一份 `draft.md` 通读
  体积 313KB → 154KB（est 78,387 → 38,489 < 48,000 预算）。
- **修复 P1**：`prompt_token_budget` 超限时先跑 `slim_oversized_messages`
  （单条 >8KB 压到头+尾，纯文本、不依赖 LLM 摘要模型），再 LLM 摘要，
  仍超才上抛 `PromptBudgetExceeded`。这条路径在 2026-09-25 事故里能直接救回
  （7 条读取全被剪到 8K → payload 瘦身 ~57K < 60K），不再依赖易失败的
  fresh-restart 兜底。
- **配置同步**：`~/.latte/workflows.d/doc_review_loop.toml` 与
  `~/.latte/prompts.d/proofreader.md` 须与 `config/workflows/`、`prompts/` 权威
  副本保持一致——jemalloc 项目 `.latte/workflows.d/` 空，运行时加载的是全局
  目录，复制时必须同时检查两处。
- **`PROOFREAD_HISTORY` 迁出**：新契约禁止在 `draft.md` 文末维护过程注释
  （proofreader 每次全读会把历史也灌进上下文），该内容移出正文
  或由 writer 在新 subsession 里按需重建。
- **测试更新**：`prompt_budget_restarts_with_compact_topic_not_original_task`
  原先断言「重启后 payload 不含旧 task」——现在 slim 救回后不走 restart，
  改为断言「slim 后的 payload 仍带 task 语义、含中段省略标记、且不走 restart」。
  `read_tool_batch_enforces_output_budget` 原先靠 `numberedContent` 重复撑爆
  192KB batch budget，去重后需要更多文件才触发——改用 5 个 110KB 文件
  （50KiB 截断后每条 ~50KB JSON，3 条在 budget 内，第 4-5 条进 failed）。

## 动态 prompt 预算（2026-09-26）

- **写死预算必然随文档增长失效**：`prompt_token_budget = 60000` 是按
  「1,800 行学习文档 ≈ 53K 输入」手标的，文档涨到 1,891 行 + read 工具
  重复返回两份正文（2.15×）后直接撞穿。改为
  `prompt_token_ratio = 0.8`（`resolve_prompt_budget`，窗口 × 比例 − 输出
  预留，钳在 [16K, 160K]），同一份文档在 128K 窗口模型上自动算出 81,920
  预算，39.2K 的工具结果仅占 47.8%，留足余量。
- **模型窗口必须实测，不能信配置**：`context_window = 1000000` 是抄来的
  占位值，导致 `CompactionPolicy::should_compact` 的 `est_tokens / window`
  比例永远达不到阈值、压缩永不触发（compaction.rs 注释里已记录此坑）。
  2026-09-26 实测 `glm-5.3` 经 `vectide.cn` 网关：201,720 tokens 四处分散
  marker 全部零截断召回，末尾 222,924 tokens 仍能回复 → 真实窗口 ≥ 223K，
  配 `context_window = 200000`（留 23K 安全余量）。`MiniMax-M3` 和
  `deepseek-v4-*` 保守设 128K。
- **全局配置必须与仓库同步**：事故时 `~/.latte/workflows.d/` 里 8 个
  workflow + 9 个 prompt 滞后仓库 2~4 周，实际加载的全是旧版。根因：
  `install.sh` 的 `install_file` 遇到目标文件已存在时，非交互模式直接
  `return 0` 跳过（非 `--force` 不更新）。已改为 `cmp -s` 比对内容，
  相同报 `up-to-date`，不同报 `drifted` 并提示 `--force` 同步；新增
  `prompts/` 安装循环（此前完全漏装）。同步后需跑
  `workflow_copies_do_not_drift` 验证三份一致性（config/workflows.d/
  + .latte/workflows.d/ + ~/.latte/workflows.d/）。
- **`PROOFREAD_HISTORY` 教唆残留**：`learn_loop.toml` 返工说明还在教唆
  writer 在 draft.md 文末维护 HTML 历史注释，与 `doc_review_loop` 的
  fresh-subsession 契约直接冲突。已改为「严禁维护历史注释」。
- **探针方法**：测真实窗口用「标记词放在 prompt 末尾/中段，问模型回显」
  ——能回显 = 全文被送达且无中段截断；零成本（~0.1 元）即可拿到实测值。
  对 `bytes/4` 估算的校准：同一段中文 prompt 实测 `prompt_tokens` 与估算
  偏差 ±15%（vs 之前用 `1e6` 假窗口时的 800K 阈值完全不触发）。
- **`compact_stale_tool_results` 从「按条数豁免」改为「体积预算驱动」**：
  旧实现只压末尾 6 条之外的结果，若这 6 条每条 40KB，240KB 全部豁免。
  新增 `RECENT_TOOL_RESULTS_BUDGET_CHARS = 24_000`（≈6K tokens）作为
  累积体积上限，即使条数未超、体积超了也必须压；同时单条 > 8K 即使在
  最新区也中段省略到 8K。没有配置预算的 step 也因此有了物理上限。
- **失败 turn 的 usage 必须上报**：`run_step_speaker` 和 `delegate` 失败
  时补写的 `TurnEnd` 原来恒为 0（`controller.rs` / `workflow.rs`）。
  事故里 proofreader 成功发了 7 次模型调用，日志却显示 `total_input: 0`，
  成本与上下文占用完全不可观测。修复：`drop(returned)` 前留存
  `last_failed_usage`，`TurnEnd` 取真实值并报 `elapsed_ms`。

## `delete_session` 删盘后异步 `Done` 重生僵尸文件（2026-09-26）

- **现象**：侧栏删除旧 session 后，磁盘上残留 203 字节的 `.latte/ui-sessions/<id>.jsonl`
  空壳文件（仅含 `__meta__` 和 `{"type":"Done"}`），在服务内存 session map 中已不存在，
  导致「盘上文件数 > 内存 session 数」。
- **根因**：`delete_session` 流程为：`abort_if_spawned().await` → `delete_files()`。
  `abort` 会使驱动 loop 退出并向 `event_tx` 广播 `ChatEvent::Done`；而负责落盘的
  archiver / disk writer 运行在独立的 `tokio::spawn` 异步任务中。当 `delete_files()`
  已经把磁盘文件 `std::fs::remove_file` 删掉后，稍后抵达的 `Done` 事件触发
  `append_raw`；此时 `!self.path.exists()` 成立，走新建/promote 分支，把空文件重新写盘。
- **修复**：`SessionPersist` 增加 `deleted: bool` 旗标；`remove_file()` 析构落盘时置
  `deleted = true`。`append_raw`、`append_event`、`set_label`、`compact` 在 `deleted`
  为 true 时全部静默 return no-op，确保物理删除后任何残存异步写事件均不可使文件复活。
  已配单元测试 `deleted_session_persist_is_disarmed_against_resurrection`。

## 重启/离线的本地持久化缺口（2026-09-27）

- **背景**：用户报告「重启后任务看板的 session 关联消失、要重新点立即执行」。定为离线优先：
  本地盘是 source of truth，Notion 只是镜像。
- **根因（一批同形 bug）**：多个状态要么「写了内存没写盘」，要么「写盘的 key 不合法/不可重建」：
  1. **session 文件 lazy-promote**：派发建的 session 先落 `.latte/tmp/`，首个可归档事件才
     promote 到 `ui-sessions/`；重启 `clean_tmp` 清掉未 promote 的，任务 `runs[].session_id`
     变 404。修复：`dispatch_task` 调 `SessionPersist::materialize` 提前落盘 meta。
  2. **notion_dirty 纯内存**：`TaskStore.notion_dirty` 重启即清空 → 改过未推的任务永久漏推。
     修复：改成 `.latte/.tasks-dirty/<id>` 标记文件（与 doc 侧 `.docs-dirty/` 同构），
     `TaskStore::load` 扫盘重建。
  3. **doc 镜像 record id 含 `/`**：`sync_one_doc_dirty` 用 rel_path 当 PUT id →
     notion-client 单段路由/校验拒 → 4xx 删标记 → 全部文档永不镜像。修复：用 marker 文件名。
  4. **next_seq 回绕覆盖任务文件**：`board.json` 损坏/丢 → next_seq 回默认 → 新任务分配已有 id
     → `save_task` 覆盖旧任务文件（**唯一真丢本地数据**）。修复：`TaskStore::load` 对账
     `next_seq ≥ max(已用 seq)+1` 并回写。
  5. **HIL SessionRecord 跨重启续不上**：`session_id = {task_id}-{timestamp}`，`new` 出来的
     path 恒不存在，`session_path().exists()` 判据永远 false。修复：`SessionManager::load_or_create`
     按 `task_id` 找最近记录（`find_latest_record`）；`pause.rs`/`resume.rs` 的重复实现收敛到它。
- **教训**：判断「某状态重启会不会丢」不能只看「有没有一个内存 map」，要看**恢复路径的 key
  是否稳定可重建**。带时间戳/自增序号的 id 生成器 + 「先建成再看文件在不在」的判据 = 经典陷阱。
  离线优先场景下，任何「重建 key」都必须能从盘上稳定推导出来。
- **4xx 语义**：只有 400/404（数据错）才删标记；401/403/408/429 是瞬时/配置错，必须留标记重试
  （否则 token 轮换窗口内的改动被永久丢掉，修好也不会补）。
