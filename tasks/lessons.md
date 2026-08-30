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
- 任务看板的任务模型**没有依赖字段**（`ImportTask`/`Task` 只有 title/description/priority/
  labels/task_type/workflow/paths/subtasks），依赖只能靠 priority 表达；拆分 prompt 必须
  显式说明这一点，否则模型会把依赖写进 description 文字里自我安慰。
