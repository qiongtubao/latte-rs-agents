# 任务看板（Task Board）设计文档

> 状态：v2 · 分支 `latte/task-board` · 2026-08-22
> v2 新增：可配置 **任务类型 `task_type → 默认 workflow`**（`config/task_types.toml` / `.latte/task_types.toml`）
> 参考：OpenAI Symphony（`/home/dong/Documents/github/symphony`，SPEC.md §4/§7/§8）
> 原型：`task-board.html`（仓库根目录静态 mockup）+ `latte-agent-cli/ui/src/task_board.ts`

## 1. 目标

给每个项目提供一个本地任务看板：任务是一等实体，有多状态生命周期；
不同状态提供不同的"下一步动作"；`todo` 状态可指定时间执行；
执行 = 把任务交给 manager（新建一个 ui-session，多角色协作）；
"查看执行详情"直接跳转到现有的 ui-session 多角色对话页面。

v2 新增：**业务类型 `task_type`（可配置）→ 默认 workflow**，类型决定执行流水线；
未指定类型与 workflow 时默认走普通 manager 会话（不绑死）。

v3 新增：**任务依赖 `depends_on` + 同族并行**。同一棵任务树下的任务默认**并行**执行
（v1/v2 是整棵树串行）；先后关系一律用 `depends_on` 显式声明（依赖全部进终态才派发），
文件冲突由 `paths` 判定——两边都声明且不重叠即可并行，任一方未声明则退回保守串行。
Phase 2 起，**隔离类任务**（按 `task_type` 的 `isolated` 或任务自身声明）在
`$LATTE_HOME/worktrees/<项目>/<任务 id>` 的独立 git worktree 里执行，因此完全不受文件
冲突约束——批准（`merging`）时自动合并回 base 分支，冲突保留工作树与分支交人工。

非目标（v1 不做）：

- 多项目聚合视图、cron 周期任务
- 看板拖拽排序（mockup 之后再评估）

## 2. 状态机

对齐 Symphony 的 tracker states（`elixir/WORKFLOW.md`）：

```
                 ┌─────────┐
                 │ Backlog │  暂存，不调度
                 └────┬────┘
                      ▼
        ┌──────►│  Todo   │◄──────────────┐
        │       └──┬───┬──┘               │
        │   立即执行│   │指定时间执行        │
        │          ▼   ▼ (到点自动派发)     │
        │       ┌─────────────┐   中止    │
        └───────│ In Progress │───────────┘
  (reopen)      └──────┬──────┘
                       ▼ manager 回报完成
               ┌──────────────┐  打回   ┌────────┐
               │ Human Review │───────►│ Rework │──┐
               └──────┬───────┘        └────────┘  │重新派发
                      ▼ 通过                        ▼
               ┌──────────┐                  (回 In Progress)
               │ Merging  │
               └────┬─────┘
                    ▼
            ┌───────────────┐
            │ Done/Cancelled│  终态，可 reopen 回 Todo
            └───────────────┘
```

- **active_states**（可派发）：`todo` / `in_progress` / `rework` / `merging`
- **terminal_states**：`done` / `cancelled`
- 排期只是 `todo` 上的一个属性（`scheduled_at`），不单独设 `scheduled` 状态——
  对齐 symphony"状态归 tracker、调度归 orchestrator"的分层

### 2.1 各状态的"下一步动作"

| 状态 | 动作 |
|---|---|
| Backlog | → Todo · 编辑 · 取消 |
| Todo | ▶ 立即执行 · 🕐 指定时间执行 · 移回 Backlog |
| Todo（已排期） | ▶ 立即执行 · 修改时间 · 取消排期 |
| In Progress | 💬 查看对话 · 中止（回 Todo，session 保留） |
| Human Review | 💬 查看对话 · ✓ 通过（→ Merging）· ↩ 打回（→ Rework） |
| Rework | ▶ 重新派发 · 编辑补充要求 |
| Merging | 💬 查看对话 · ✓ 标记完成（→ Done） |
| Done / Cancelled | 重新打开（→ Todo） |

## 2.2 任务类型 `task_type → 默认 workflow`

可配置注册表：`config/task_types.toml`（权威默认，随包 `include_str!` 编进 binary）
+ 项目层覆盖 `.latte/task_types.toml`（同名覆盖，未声明的不动）。`GET /api/task-types` 暴露。

| `task_type` | 名称 | `default_workflow` | 说明 |
|---|---|---|---|
| `feature` | 功能开发 | `tdd_development` | 需求→TDD实现→规格/质量双审 |
| `bugfix` | 缺陷修复 | `bug_triage` | 复现定界→根因→修复方案→终审 |
| `learn` | 学习任务 | `learn` | 六层图解教程 |
| `research` | 探索调研 | `explore` | 通读代码/文档产出决策背景 |
| `docs` | 文档写作 | `write_doc` | doc-graph 写文档 |
| `chore` | 杂项/运维 | *(空)* | 不绑定，走普通 manager 会话 |

派发规则（`tasks::dispatch_task`）：

1. 显式 `workflow` 优先；
2. 否则按 `task_type` 的 `default_workflow` 推导；
3. 都为空 → 普通 manager 会话（`api::chat_send`）。
`TaskView.effective_workflow` 供前端直接展示实际生效值。

## 3. 存储结构

每个项目的数据放在项目根 `.latte/tasks/`（ui-server 的 cwd 即项目根，天然按项目隔离）：

```
.latte/tasks/
├── board.json          # 看板元信息
├── LAT-100.json        # 每任务一个文件
├── LAT-101.json
└── archive/            # 可选：归档的 Done/Cancelled
```

- 每任务一文件，写入用 `tmp + rename` 原子替换（同 `ui-sessions/` 约定）
- 默认加入 `.gitignore`（排期、session 关联是本机运行时状态）；团队想共享看板可手动入库
- 时间一律 epoch ms，`null` 表示无

### 3.1 board.json

```json
{
  "schema": 1,
  "project": "项目名",
  "id_prefix": "LAT",
  "next_seq": 109,
  "states": ["backlog","todo","in_progress","human_review","rework","merging","done","cancelled"],
  "active_states": ["todo","in_progress","rework","merging"],
  "terminal_states": ["done","cancelled"]
}
```

任务 id = `{id_prefix}-{seq}`，`next_seq` 由 TaskStore 单进程串行分配。

### 3.2 单任务 LAT-101.json

```json
{
  "schema": 1,
  "id": "LAT-101",
  "title": "实现新 ringbuf 核心读写路径",
  "description": "……",
  "priority": 1,
  "state": "todo",
  "labels": [],
  "task_type": "feature",
  "parent_id": "LAT-100",
  "sub_order": 0,

  "needs_split": true,
  "outline_path": "docs/outlines/root.md",
  "doc_path": null,

  "scheduled_at": null,
  "workflow": null,

  "runs": [
    {
      "session_id": "ui-2048757-1784996775457",
      "started_at": 1784996775000,
      "ended_at": 1785000375000,
      "result": "completed"
    }
  ],

  "created_at": 1784996360000,
  "updated_at": 1784996760000,

  "history": [
    { "at": 1784996360000, "from": null, "to": "backlog", "actor": "user", "note": "任务创建" }
  ]
}
```

- **`needs_split`**：规划标记，是否需要继续拆分（旧文件缺省 `false` 兼容）；见 §3.4
- **`outline_path`**：大纲文档路径（项目相对），**只在根任务上写**；旧文件缺省 `null`
- **`doc_path`**：已发布的学习文档路径（项目相对），发布时由引擎回写；缺省 `null`
- **`task_type`**：业务类型，可空（旧文件缺省 `null` 兼容）；派发时按注册表推导 `effective_workflow`
- **`workflow`**：显式绑定；空表示未绑定（可被类型默认覆盖）
- **`effective_workflow`**：仅视图字段（`TaskView`），不落盘
- **`runs` 数组**：每次执行（含重跑、rework 再派发）新建 ui-session 并追加一条；
  `runs.last().session_id` 即"查看执行详情"跳转目标，
  对应 `.latte/ui-sessions/<session_id>.jsonl`
- **`result` 枚举**：`completed` / `aborted` / `failed` / `timeout`
- **`history` 内嵌**：事件量小不单独开 JSONL；`actor` ∈ `user` / `scheduler` / `manager` / `code_review` / `workflow`

### 3.3 任务嵌套（任意深度任务树）

- 扁平文件 + `parent_id` 引用，**任意深度**：校验只要求 parent 存在且
  无环（新 parent 不能是本任务自身或其后代）
- 子任务是普通任务：独立状态机、独立排期/派发/ runs，独立 `task_type`；
  子任务可继续再拆（refine/import 均支持多级嵌套）
- 直接子任务全部 done → 父任务自动 done，并向上冒泡直到根
  （`maybe_complete_parent`）；父卡片显示**全部后代**的聚合进度
- 派发父任务时把直接子任务清单拼进发给 manager 的消息（见 §5）

### 3.4 规划标记：needs_split / outline_path / doc_path

**`needs_split` 是规划标记，不是"有没有子任务"的推论。** `true` = 这个大任务还得
继续拆；它和当前是否已经有子任务**解耦**——把拆出来的子任务删掉不会把意图一起
丢掉，反过来叶子任务也可以临时挂子任务。`needs_split=false` 的任务就是叶子：
`POST /api/tasks/:id/refine` 直接回 409（"叶子任务不可再拆"），要拆先在看板把它
置回 `true`。拆分类 workflow（`outline_split` / `task_refine`）的产物必须**显式**
写这个字段（`require_needs_split`），漏写整单打回——"没写"和"写了 false"在
类型化之后分不开，所以这道门查的是原始 JSON 键。

**`outline_path` 只写在根任务上。** 它指向项目相对的大纲文档
`docs/outlines/<slug>.md`；子孙任务一律靠视图字段 `effective_outline_path`
（自身优先，否则沿 `parent_id` 祖先链找最近的一份，带防环上限）解析。
这样一份大纲只有一个"主人"，不会在多个任务上各存一份副本、改一处漏一处；
祖先链上没有 = 这个任务不属于任何大纲（`POST /api/outlines/:id/sync` 此时回 409）。

大纲是任务树的第二权威：看板管"谁在做、做到哪"，大纲管"为什么这么拆"加上
人手写的叙述。机器只碰受控区块，区块外的前言/正文一个字不动：

```markdown
<!-- node:LAT-100 -->
<!-- section:body:start -->
## 概述
人手维护的叙述，任何同步都不碰这一段。
<!-- section:body:end -->
<!-- children:start -->
<!-- node:LAT-101 needs_split -->
### 模块一
<!-- learn-links:start -->
- [模块一笔记](../learn/module-one.md)（task LAT-101）
<!-- learn-links:end -->
<!-- children:end -->
```

手改大纲之后用 `POST /api/outlines/:task_id/sync` 推回看板：只同步**结构**
（id / title / parent_id / needs_split），执行状态、依赖、调度、runs 一律不动——
那些是看板的权威。缺项默认只报不删（误删一行锚点不该等于删掉整棵子树），
真要删得显式 `?prune=true`。

**`doc_path` 指向该任务发布过的学习文档。** 学习 workflow 定稿
`docs/learn/<name>.md` 时由引擎回写这个字段（写不进去只警告，不影响已经落盘的
定稿）；子任务发布时靠它把文档 frontmatter 的 `parent_doc` 指到父文档，学习文档
因此长成一棵镜像任务树的文档树。

### 3.5 大任务怎么拆：outline_split → 导入定稿

一条大任务走 `outline_split` workflow：`breakdown`（task_planner 出任务树 JSON，
每个节点显式带 `needs_split`）→ `outline_draft`（`outline_writer` 写大纲草案，
锚点用 `tmp:<标题>` 临时键，并过 `require_outline_against_plan`：锚点标题集合与
清单逐字一致、`needs_split` 一致、无孤儿锚点）→ `gate`（reviewer 评审，退回
`breakdown`）→ `submit`（零模型，只提出 plan）。

草案落在 `.latte/outline-drafts/<plan_id>.md`；真实 task id 要到用户在看板弹窗里
导入才分配，所以**定稿发生在导入时**：`POST /api/tasks/import` 建完任务后，用
"标题 → 真实 id"映射跑 `outline::rebind_tmp_ids`，把所有 `tmp:` 锚换成 `LAT-*`，
写 `docs/outlines/<slug>.md`，再把 `outline_path` 挂到根任务上（根锚是
`LAT-ROOT` 或本项目里不存在的 id 时，挂到每个顶层新任务，否则整份大纲没人指向）。
任何一个 `tmp:` 锚绑不上就整单失败——绝不把临时锚发布出去；回滚会把大纲文件
（连同导入前已有的同名文件）和各任务的 `outline_path` 一起还原。

子任务拆分（`task_refine`）同理，只是插入在导入时机械完成：建完子任务后解析父
任务的 `effective_outline_path`，按真实 id 把每个直接子任务插到父节点下（已有则
更新，不重复插）；父节点锚点找不到就整单失败，父任务本来就没有大纲则跳过
（这次拆分不维护大纲，不是错误）。
