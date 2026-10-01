# learn_loop：读完代码后如何写出学习文档

> 依据（均已读过源码/配置，未做端到端实跑）：
> `config/workflows/learn_loop.toml`、`config/workflows/doc_review_loop.toml`、
> `latte-agent-core/src/workflow.rs`（`assemble_draft_from_sections`、`run_learn_control_step`、
> `deterministic_grade_quiz`、`deterministic_learning_ask`）。
>
> 读法：先看 §1 的一句话与 §2 的全景图，再看 §3 的目录树（知道每个文件是谁写的），
> 然后按 §4 的步骤逐个看。§9 是一次完整的示例运行。§10 是已确认的缺陷。

## 1. 一句话

先由 **plan** 读代码、拆出 3–6 个知识点；再由 **doc_review** 让「总编写框架 → 多个 writer
一人一章并发写 → 审稿员对照源码挑错，不合格只让被点名的章重写」；文档由**引擎**拼成
`draft.md` 给学员读；学员答题，答错或有补充就带着反馈回到 doc_review 增强文档；最后 **merge** 定稿。

## 2. 全景图

```
                       ┌────────────────────── QUIZ_FAIL ──────────────────────┐
                       │                                                       │
plan ─► annotate ─► doc_review ─► [引擎装配draft.md] ─► read_confirm ─► quiz ──┤
                       ▲                                (点「开始答题」) (弹窗答题)│ QUIZ_PASS
                       │                                                       ▼
                       └───────────── SUPPLEMENT ◄──────────────────────── supplement
                                                                               │ ALL_DONE
                                                                               ▼
                                                    merge ─► [sync_code 可选] ─► report
```

- `read_confirm`、`quiz` 由引擎确定性执行（弹窗、判分、写账本不走模型）。
- `doc_review` 是一个**嵌套 workflow**（`doc_review_loop`），不是单个模型步。

## 3. 产物目录：每个文件谁写、谁读

根目录 `.latte/learn/<slug>/`（只有定稿在 `docs/learn/`）：

| 文件 | 谁写 | 谁读 | 作用 |
|---|---|---|---|
| `plan.json` | plan | annotate、outline、writer、proofread、quiz(引擎)、merge | 知识点、证据、例子、题目、答案、`doc_outline` |
| `ledger.json` | plan 初始化；quiz(引擎) 改判分；supplement 记补充 | read_confirm、quiz、supplement、merge、report | 只记事实：`round`、每点 `wrong`/`passed`、`supplements` |
| `code/<kid>-<文件名>` + `code/README.md` | annotate | outline/writer（引用）、merge、sync_code | 注释版代码副本（源码不动） |
| `sections/00_overview.md` | outline | writer（先读它对齐比喻）、引擎装配 | 全局比喻 + 全景链路 + 学习路径 |
| `index.md` | outline | proofread | 阅读入口与各章链接 |
| `feedback.md` | outline | 各 writer | 本轮是全量重写还是局部修补，及按章分组的意见 |
| `sections/<kid>.md` | 每个 writer 各写一个 | proofread、引擎装配 | 一个知识点一章正文 |
| `draft.md` | **引擎装配**（writer 不写） | 学员、merge | 学员阅读的整篇文档 |
| `docs/learn/<slug>.md` | merge | — | 最终交付 |

`plan.json` 每个知识点的字段：`id`、`title`、`evidence`(file:line)、`teach_points`、`example`、
`doc_section`、`question`、`options`(4 项)、`answer`。`ledger.json`：`{round, knowledge_points:[{id,wrong,passed}], supplements, _graded_answers}`。

## 4. 步骤详解

### 步骤 1：plan —— 读代码，定「教什么」

- 角色 tutor；工具 `read`/`code_graph`/`search`/`write`，**没有 `ask`**（设计阶段不打扰学员）。
- 引擎要求：必须至少调过一次取证工具、必须 `write` 过（否则重试）。
- 做的事：
  1. 取证：`code_graph` 取签名和行号，`read` 精读，`search` 找宏/字符串。
  2. 拆 3–6 个互不重叠、由浅入深的知识点，第一个必须是「这东西是什么、整体怎么运转」。
  3. 写 `doc_outline`：第 1 节固定为「全局心智模型与贯穿比喻」；涉及分档/分类/分层，必须含
     「怎么存（物理视图）」与「为什么这样存（权衡与反向思考）」。
  4. 每点写 `example`（最小具体例子：传什么进去→出来什么）和一道考理解的四选一题
     （不许考「在第几行」这种查表题，禁 `recommended` 字段，正确项位置自己打乱）。
  5. 落盘 `plan.json`、`ledger.json`（`round:0`，每点 `wrong:0, passed:false`）。同 slug 重跑也必须刷新，不能跳过。
- 输出：**只输出 slug**（3–60 字符，禁空格、`/`、`\`、反引号），下游把它拼进路径（`{{plan_out}}`）。

### 步骤 2：annotate —— 生成注释版代码副本

- 工具只有 `read` + `write`，**没有 `edit`**，所以物理上改不到源文件。
- 按每个知识点的 `evidence` 读原文，逐字保留代码、只加面向零基础的中文注释，写到
  `code/<kid>-<文件名>`；同一文件被多点引用则合并；最后写 `code/README.md` 对照表。
- 作用：让 writer/学员写文档、读文档时有可对照的带讲解代码。

### 步骤 3：doc_review —— 写文档（核心，嵌套 `doc_review_loop`）

引擎把 `slug`、`draft`、`plan`、`ledger` 路径作为 topic 传入。内部分三个 wave：

```
wave0 outline ──► wave1 write_chapter ×N（并发）──► wave2 proofread ──┐
    ▲                                                                  │
    └──────────────── NEEDS_FIX（最多 3 轮）◄──────────────────────────┘
                                          LOOKS_CLEAR → 返回外层
```

#### 3.1 outline：总编，只搭骨架，不写章节正文

按顺序写三个文件：
1. `sections/00_overview.md`：生活化比喻（把核心角色映射成实体）→ 端到端全景（一次典型操作从头到尾）→
   **学习路径**（每章要回答的**设计问题**，说明这章解决前一章留下的什么矛盾）→ 各章相对链接。
2. `index.md`：一句话主题 + 阅读顺序 + 各章链接与一句话摘要。
3. `feedback.md`：
   - prompt 末尾**没有**「【上轮审查反馈】」→ 写单行 `MODE: FULL`（首轮，全部重写）。
   - **有**反馈 → 写 `MODE: PATCH`，把审稿的每条 ISSUE 原文按章 id 分组抄进去：
     ```
     MODE: PATCH
     ## k3
     - ISSUE: k3 | BLOCKER | 事实错误 | …
     ## overview
     - ISSUE: overview | BLOCKER | …
     ```
     `## overview` 组代表全局意见：outline 当场改 `00_overview.md` 与 `index.md`。
   - outline **不许**自己改 `sections/k*.md`。

#### 3.2 write_chapter：章节 writer（引擎 fan_out 并发）

- 引擎读 `plan.json` 的 `knowledge_points`，**每个知识点派发一个 writer**（`max_items=32`）。
  并发度由 `LATTE_WORKFLOW_CONCURRENCY` 控制（缺省 4），超出的排队，有空位补下一个。
- 每个 writer 只知道自己是哪一章（`{{item_id}}`、`{{item_index}}`）。
- **写权限被引擎锁死**：`write_path_allowlist = [.latte/learn/{{slug}}/sections/{{item_id}}.md]`，
  写别的路径会被工具层拒绝（有单测 `write_path_allowlist_confines_writes_to_the_rendered_scope`）。
- `tolerate_failures = true`：某章失败不拖垮其他章，该章产出以 `FAN_OUT_ITEM_FAILED` 开头，审稿按缺章判 BLOCKER。
- writer 的决策表（先读 `feedback.md`，不存在按 `MODE: FULL`）：

  | feedback 状态 | 本章文件 | 行为 |
  |---|---|---|
  | `MODE: PATCH`，无 `## <本章id>` 组 | 已存在 | 只输出 `UNCHANGED`，不动文件 |
  | `MODE: PATCH`，有 `## <本章id>` 组 | 已存在 | 只按这些意见局部修改 |
  | `MODE: FULL` | 任意 | 整章重写（旧文件仅参考，不好就推倒） |

- 写前必须：读 `00_overview.md`（对齐比喻与本章位置）；**自己读 evidence 行号区间**，
  不许凭记忆、不许沿用旧章里未核对的说法。
- 每章 7 段，依次回答：
  1. 它是什么 / 要解决什么问题（承接总览比喻，术语首现即解释）；
  2. 作者的设计与动机（依据只能是源码注释/文档/实现的必然性，无依据标 `[推测]`）；
  3. 备选方案与取舍（表格「方案/好处/坏处」，数值级具体，含设计代价与失效场景）；
  4. 怎么存 / 怎么运转（物理布局或调用链 + ASCII 图）；
  5. 真实源码（**原文**片段 + `file:start-end` + 行内中文注释；禁止「简化版」「…省略」「伪代码」）；
  6. 最小示例（输入→每步中间值→输出，算式自己重算）；
  7. 本章小结与承上启下（引出下一章矛盾）。

#### 3.3 proofread：审稿员，只读不改，对照源码核查

- 工具 `read`/`search`/`code_graph`/`doc_graph_context`，必须至少 `read` 一次。
- 顺序：`index.md` → `00_overview.md` → 各章；**每章至少核对 2 处**关键代码/数值与源码一致。
- 也会拿到 `{{chapters_out}}`（各 writer 汇总产出），含 `FAN_OUT_ITEM_FAILED` 或章文件缺失 → 该章缺章 = BLOCKER。
- 分级：

  | 级别 | 内容 | 是否触发返工 |
  |---|---|---|
  | BLOCKER | 事实错误（须亲自读源码确认）；伪代码冒充源码；没讲清设计动机/备选/好处坏处；把猜测写成事实；章序跳跃、前后重复矛盾、只罗列代码；缺章 | 是 |
  | NIT | 措辞、图表偏密、术语铺垫、行号小偏差、排版 | 否 |

- 输出格式：`ISSUE: <k_id 或 overview> | <BLOCKER/NIT> | <类别> | <具体问题> | <应补内容>`；
  章 id 必须写准（返工靠它分派给对应 writer）。
- 最后一行：有任一 BLOCKER → `NEEDS_FIX`，否则 `LOOKS_CLEAR`；除末行外不许出现这两个词。
- 循环参数：`loop_until=LOOKS_CLEAR`，`loop_back_to=outline`，`max_iterations=3`；
  `prompt_token_ratio=0.8`，上下文溢出自动全新重启（最多 2 次，重启时保留 `index.md`）。

#### 3.4 返工一轮到底怎么走（NEEDS_FIX）

1. proofread 输出 `NEEDS_FIX` + ISSUE 列表；
2. 引擎跳回 outline，把 ISSUE 列表作为「【上轮审查反馈】」拼到 outline 的 topic 末尾；
3. outline 见到反馈 → 写 `MODE: PATCH` + 按章分组的 `feedback.md`；
4. 引擎再次 fan_out：被点名的章 writer 只改意见涉及处，其余章输出 `UNCHANGED`；
5. proofread 重审；第 3 轮仍不过则停止循环（保险丝）。

### 步骤 4：引擎装配 draft.md（零模型调用）

`assemble_draft_from_sections`，在 read_confirm 与 quiz 执行前调用：

1. `sections/` 或 `plan.json` 不存在 → 不装配（返回 false）；
2. 若 `plan.json` 有 `design.decisions`，引擎先写入「核心结论与设计决策」
   （一句话结论 + 决策总览表），不依赖 outline 手写；没有 `design` 时跳过，旧文档装配不变；
3. 再拼 `00_overview.md`，然后按知识点顺序逐章；
4. 找章文件：先 `<id>.md`，找不到再匹配 `<id>_*`、`*_<id>`；空文件跳过；
5. 装配时剥掉符号索引 mermaid（节点多数是 `fn xxx` / `struct xxx`），数据流图保留；
6. 各段之间以 `\n\n---\n\n` 分隔，整体覆盖写 `draft.md`；全空则不写。

### 步骤 5：read_confirm —— 等学员读完（引擎）

- 弹一个只有一项「开始答题，验证掌握程度」的窗口，窗口里直接渲染 `draft.md` 内容；
- 题面：`学习材料（第 {round+1} 版）：.latte/learn/<slug>/draft.md。读完了吗？`；
- 引擎先查 `AnswerLog.recall(题面)`：**题面文本相同则直接回放上次答案，不弹窗**。

### 步骤 6：quiz —— 判分（引擎，确定性）

1. 从 ledger 取所有 `passed == false` 的知识点，从 plan 取对应题目；
2. 一次 `questions=[...]` 弹全部题，每题题面追加 `［第{wrong+1}次作答］`（避免题面重复被回放）；
3. 判分：按 **label 文本**比对，与选项位置无关；归一化（去首尾空白/收尾标点/忽略大小写）后
   相等，或互相包含（正确 label ≥2 字才允许包含）即对；乱填、空、自由文本按错；
4. 改账本：答对 `passed=true`；答错 `wrong+1`；用 `_graded_answers` 的 `id:attempt` 做回执，重放不重复计数；
5. 输出：逐题「判分结果 / 你选的是」；答错题附正确答案、一句「为什么」（该点 `teach_points` 首条）、
   `file:line`，并列出该点 `requires` 里仍未掌握的前置；末行 `QUIZ_PASS`（全对）或 `QUIZ_FAIL`。
6. `QUIZ_FAIL` → 引擎跳回 `doc_review`，这段判分文本整体成为「【上轮审查反馈】」，
   即 outline 会把它当作章级意见，走 3.4 的 PATCH 路径，让答错点对应的章增强「易错点辨析」。
7. `max_iterations=10` 为保险丝，不设「错几次放弃」。

### 步骤 7：supplement —— 全对后收集补充（模型步）

- 一个只有「没有了，全部理解」的窗口；想补充的学员走自动附带的「其他（自定义）」输入；
- 题面必须带轮次号（防回放）；
- `没有了` → 写一次 ledger（仅刷新 `updated_at`）→ 末行 `ALL_DONE`；
- `其他：<内容>` → 把原文追加进 `ledger.supplements`（`{round, asked}`）→ 末行 `SUPPLEMENT`，
  引擎跳回 `doc_review`，补充内容作为反馈让 outline 分派。

### 步骤 8：merge、sync_code、report

- **merge**（read+write）：把补充融入正文并归位、「易错点辨析」润色保留；重排、生成目录、去重、清过程痕迹
  （「第几版」「本轮新增」）；保留全部 `file:line`、代码摘录、` ```mermaid ` 块、`code/` 引用；
  文末加「注释版代码副本」索引；写 `docs/learn/<slug>.md`，`draft.md` 保留作存档。
- **sync_code**（可选，主题含「同步注释」/「写进源码」才跑）：`edit` 把注释写进真实源码，
  只改注释、以代码原文为锚点，找不到锚点就跳过并说明。会修改仓库，建议开 isolated worktree。
- **report**（只读）：输出掌握进度 X/Y、答错次数、补充清单、交付路径。

## 5. 硬约束速查（都是引擎字段，不是 prompt 劝告）

| 想防止的问题 | 机制 |
|---|---|
| writer 覆盖别人的章 | `write_path_allowlist` |
| 一章失败拖垮全篇 | `fan_out.tolerate_failures` + 审稿判缺章 |
| 模型凭记忆写文档 | plan/writer 必须读源码；审稿每章核对 2 处 |
| 伪代码冒充源码 | writer 标准 + proofread BLOCKER |
| 文档顺序漂移/丢内容 | 引擎按 plan 顺序装配 |
| annotate 改到源码 | 无 `edit` 工具 |
| 返工范围失控 | `feedback.md` 按章分组 + `UNCHANGED` |
| 审稿无限循环 | `max_iterations=3`；外层 quiz/supplement 各 10 |
| 上下文爆掉 | `prompt_token_ratio` + 自动重启 |

## 6. 一次完整示例（3 个知识点 k1/k2/k3）

1. plan 写出 3 点，`ledger.round=0`；输出 slug `demo`。
2. annotate 生成 `code/k1-*.rs` 等副本。
3. doc_review 第 1 轮：outline 写 overview/index、`MODE: FULL`；3 个 writer 并发写 `k1.md`/`k2.md`/`k3.md`；
   proofread 报 `ISSUE: k2 | BLOCKER | 事实错误 …` → `NEEDS_FIX`。
4. 第 2 轮：outline 写 `MODE: PATCH` + `## k2`；k1、k3 writer 输出 `UNCHANGED`，只重写 k2；proofread `LOOKS_CLEAR`。
5. 引擎拼出 `draft.md`；read_confirm 弹「第 1 版」，学员读完点击。
6. quiz：k1 对、k2 错、k3 对 → 账本 `k2.wrong=1`，`QUIZ_FAIL`，判分文本回到 outline。
7. doc_review：outline `PATCH` + `## k2`（来自判分反馈）→ 只重写 k2 加易错点辨析 → 再拼 draft.md。
8. read_confirm → quiz（只考 k2，题面 `［第2次作答］`）→ `QUIZ_PASS`。
9. supplement：学员输入「想了解锁竞争」→ `SUPPLEMENT` → 回 doc_review 补内容 → 再答 → `ALL_DONE`。
10. merge 定稿，report 输出 3/3。

## 7. 前置条件与失败处理小结

- 任何一步 `require_tools`/`output_contract` 不满足，引擎按 `max_retries` 重试，再失败则 workflow 失败；
- `plan.json` 或 `ledger.json` 缺失，`run_learn_control_step` 直接返回错误；
- 引擎不会阻止你手改 `draft.md`，但下次装配会被 `sections/` 覆盖——要改请改章文件。

## 8. 与旧设计文档的差异

`docs/learn-loop-doc-driven-design.md` §5 的表格与 §5.2（`doc` 单步、`loop_back_routes = doc`）
已过期，现行为 `doc_review` 嵌套 workflow；该文档未同步。

## 9. 缺陷与修复记录

1. **`ledger.round` 从不自增（已修复）**。原因：设计文档里「doc 步 round+1」随 doc→doc_review
   迁移丢失，全仓只有 `read_confirm` 读它。后果：返工后 `read_confirm` 题面仍是 `第 1 版`，
   `AnswerLog::recall`（`workflow.rs` `deterministic_learning_ask`）静默回放旧答案、不弹窗。
   修复：引擎函数 `bump_learn_round`，在 learn_loop 的 quiz（QUIZ_FAIL）/ supplement（SUPPLEMENT）
   触发 `Rework` 跳回 `doc_review` 时自增 `ledger.round` 并写盘（两处 `SerialLoopNav::Rework` 分支）。
   验证：单测 `bump_learn_round_increments_persisted_round`；`quiz_fail_reworks_to_doc_then_pass_advances` 仍通过。
   未做端到端实跑。
2. **quiz 判分反馈没有专属的章 id 分组（已修复）**。原先反馈是自由文本，outline 要自己拆出 `## k2`，
   拆错会让该增强的章被 `UNCHANGED` 跳过。修复：`format_quiz_feedback` 在有答错时追加
   「【按章分组的返工意见】」块（引擎确定性生成，只含答错的章，格式 `## <k_id>\n- …`），
   `doc_review_loop` 的 outline prompt 要求把该块原样抄进 `feedback.md`。
   验证：单测 `quiz_feedback_groups_wrong_points_by_chapter_id`；未做端到端实跑。
   残余风险：抄录仍由 outline 模型执行，只是不再需要它判断章 id。
