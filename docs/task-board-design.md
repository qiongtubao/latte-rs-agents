# 任务看板（Task Board）设计文档

> 状态：草稿 · 分支 `latte/task-board` · 2026-07-26
> 参考：OpenAI Symphony（`/home/dong/Documents/github/symphony`，SPEC.md §4/§7/§8）
> 原型：`task-board.html`（仓库根目录静态 mockup）

## 1. 目标

给每个项目提供一个本地任务看板：任务是一等实体，有多状态生命周期；
不同状态提供不同的"下一步动作"；`todo` 状态可指定时间执行；
执行 = 把任务交给 manager（新建一个 ui-session，多角色协作）；
"查看执行详情"直接跳转到现有的 ui-session 多角色对话页面。

非目标（v1 不做）：

- 多项目聚合视图、跨任务依赖（blocked-by）、cron 周期任务
- 多层任务树（只允许一层父子嵌套）
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
  "parent_id": "LAT-100",
  "sub_order": 0,

  "scheduled_at": null,

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

- **`runs` 数组**：每次执行（含重跑、rework 再派发）新建 ui-session 并追加一条；
  `runs.last().session_id` 即"查看执行详情"跳转目标，
  对应 `.latte/ui-sessions/<session_id>.jsonl`
- **`result` 枚举**：`completed` / `aborted` / `failed` / `timeout`
- **`history` 内嵌**：事件量小不单独开 JSONL；`actor` ∈ `user` / `scheduler` / `manager`

### 3.3 任务嵌套（父子）

- 扁平文件 + `parent_id` 引用，**最多一层**（父任务不允许再有 `parent_id`）
- 子任务是普通任务：独立状态机、独立排期/派发/ runs
- 父状态**手动控制，不自动级联**；父卡片显示聚合进度（`子任务 3/5`），
  全部子任务终态时抽屉给提示 + 一键确认完成
- 派发父任务时把子任务清单拼进发给 manager 的消息（见 §5）

## 4. Scheduler

ui-server 内一个 tokio task（对齐 symphony 的 poll loop，大幅简化）：

- 每 5s 扫一次 `.latte/tasks/`：`state == "todo" && scheduled_at != null && scheduled_at <= now` → 派发
- 派发前重读文件再校验一次（symphony `dispatch_issue` 的 re-fetch 防 stale）
- 服务重启后补偿：启动时扫一遍，已过点的排期任务立即派发
- v1 不做并发上限/每状态配额（symphony 的 `max_concurrent_agents_by_state`），
  需要时再加到 board.json

## 5. 派发：任务 → manager

`dispatch(task)`：

1. 新建 ui-session（initial_role = `manager`），label = `[LAT-101] 任务标题`
2. 把任务内容作为首条 user message 发给 manager：

```
[任务 LAT-101] 实现新 ringbuf 核心读写路径
优先级：P1
描述：……
（若是父任务）子任务：
  1. [LAT-102] 并发压测（todo）
  2. [LAT-103] 迁移调用点（todo）
完成后请汇报结果摘要。
```

3. 任务 → `in_progress`，`runs` 追加 `{session_id, started_at}`
4. 之后走现有机制：manager delegate / workflow，多角色对话在 ui-session 页面可见

### 5.1 状态回报（manager → 看板）— v1 已落地：C + /report 接口

| 方案 | 做法 | 取舍 |
|---|---|---|
| A. 工具回报（推荐） | manager 增加 `task_report` 工具（或 prompt 约定调用 `POST /api/tasks/<id>/report`），完成时上报 summary → 任务进 `human_review` | 状态准确；要给 manager 注入 task_id 上下文 |
| B. 会话侧监听 | ui-server 监听 session 的 turn 结束事件，超时无活动 → 自动进 `human_review` | 零侵入但不准（manager 可能多问几轮） |
| C. 纯手动 | 用户看完对话手动拖状态 | 最简单，体验差 |

**v1 落地（2026-07-26）**：**方案 C + `/report` 接口已备**——
`POST /api/tasks/:id/report`（body `{summary, result}`）已实现并可用
（`tasks::report_task`：completed → human_review，其他 → todo，history
actor=manager），但 manager 侧的工具/prompt 注入（方案 A 完整版）留作
后续；在此之前用户始终可以手动改状态（PATCH）。

§5 的派发消息模板以实际代码为准（`tasks::build_dispatch_message`）。

## 6. API（`latte-agent-ui-server/src/tasks.rs`）

```
GET    /api/tasks                     # 全量列表（含子任务聚合进度）
POST   /api/tasks                     # 新建（可带 parent_id）
GET    /api/tasks/<id>                # 详情
PATCH  /api/tasks/<id>                # 改标题/描述/优先级/状态/排期
POST   /api/tasks/<id>/dispatch       # 立即执行（或 rework 重新派发）
POST   /api/tasks/<id>/abort          # 中止执行（走现有 chat/abort 机制）
POST   /api/tasks/<id>/report         # manager 回报完成（方案 A）
DELETE /api/tasks/<id>                # 删除（或进 archive/）
```

`TaskStore`：目录扫描 + 内存索引 + 原子写 + `next_seq` 分配，挂在 `UiBackend` 上。

## 7. UI

- mockup 先行：`task-board.html`（已完成，根目录），确认交互后移植为
  `latte-agent-cli/ui` 的真实面板（与 traces / role editor 同级的 tab）
- 看板 7 列 + 顶栏统计；卡片显示 id/优先级/排期 chip/session chip/子任务进度
- 详情抽屉：状态徽章、**当前状态可用的下一步动作区**、描述、属性、
  子任务列表（父任务）、状态历史时间线
- "查看对话 / session 链接" → 跳转到现有 ui-session 对话页（按 session_id 定位 tab）

## 8. 实施步骤

1. `task-board.html` mockup 评审（本文档配套的视觉稿）✅
2. `tasks.rs`：Task 模型 + TaskStore + 原子写 + 单测
3. REST API 接入 `handlers.rs` / `lib.rs` 路由
4. Scheduler tokio task + dispatch（建 session 发 manager）
5. manager 回报链路（方案 A：prompt 注入 task_id + `/report` 接口）
6. vite 真实面板移植，接真实 API
7. 黑盒测试：`tests/blackbox/` 补任务生命周期用例

## 9. 开放问题

- **Q1 状态回报**：§5.1 方案 A/B/C，倾向 A+C 兜底，待确认
- **Q2 中止语义**：abort 时是否同时 cancel session 里正在跑的 delegate/workflow？
  （现有 `/chat/abort` 能力直接复用，但 task 侧回到 todo 还是 cancelled 需要定）
- **Q3 排期任务与关机**：服务不在运行时错过的排期，启动时立即补发还是提示后人工确认？
  （当前设计：立即补发）
- **Q4 归档**：`archive/` 手动移入还是 Done 超过 N 天自动归档？（当前设计：手动）
