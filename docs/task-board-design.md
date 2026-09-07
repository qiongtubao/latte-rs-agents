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

非目标（v1 不做）：

- 多项目聚合视图、跨任务依赖（blocked-by）、cron 周期任务
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
