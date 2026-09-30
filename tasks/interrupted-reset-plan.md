# 任务看板：执行中断 → 留 in_progress + 「重试」续跑

## 背景 & 用户结论

- 点「执行」后任务与 session **绑定**；session 内部失败（网络/模型）是
  session 的问题，应在 **session 层重试**，不该冒到任务状态层。
- 现状：`reconcile_interrupted_runs` 在重启时把 `in_progress` + 未结束 run
  的任务回退 `todo` + run 标 `aborted` → 重启后任务变成「可点击执行」，像
  从没跑过，且丢失了「绑定的 session」。
- 决策（用户）：**留 `in_progress` + 「重试」按钮**。不新增 interrupted
  状态、不做「重置（删 session）」——丢弃断点续跑的可能。

## 设计

任务状态如实反映「绑定的执行 session 活着还是死了」：
- 重启不碰 `in_progress` 任务（不迁状态、不标 run aborted）。
- `in_progress` 的 run 指向的 session 在盘上还在（dispatch 时 `materialize`
  落了 meta），「查看对话」照常。
- 「↺ 重试」= 让绑定的 session **从断点续跑**：workflow 任务从 checkpoint
  跳过已完成 step；经典 chat 任务恢复 gate/driver。

### 复用现成机制（不新造轮子）

- `chat_resume_session`（`api.rs:1700`）：live 则 `resume_session` 恢复 gate；
  否则 `controller_or_spawn` 冷启动 spawn（`spawn_controller` 在
  `sessions.rs:726` 扫 event_log seed `last_failed_workflow`），再
  `spawn_workflow_resume` 从 checkpoint 续跑。**一条路径同时覆盖
  workflow 与经典任务。**
- `last_resumable_workflow_from_log`（`sessions.rs:362`）：找「有
  `WorkflowStarted` 无 `WorkflowFinished`、未被 resume 取代」的中断 run，
  取最外层——正是重启打断的 run。

## 改动

### 后端

1. **`reconcile_interrupted_runs` 改为「标记」而非「回退」**（`tasks.rs:3834`）：
   - 找到 `in_progress` + `runs.last().ended_at.is_none()` 的中断任务；
   - **不改 state、不动 run**（保持「绑定着死 session」的如实状态）；
   - 只 `push_note` 一条 history：「上次执行随服务重启中断，可点『重试』
     断点续跑，或『中止』回 todo」；返回这些 id（日志/测试用）。
   - `dispatch_pending` 置 false（中断的不是待派发，等用户决定）。

2. **`in_progress` 增加 `resume` 动作**（`effective_actions`：
   `["view_session", "resume", "abort"]`）。

3. **新增 `resume_task(b, id)`**（`tasks.rs`）+ 路由
   `POST /api/tasks/:id/resume`：
   - 校验 `state == "in_progress"` 且 `runs.last().ended_at.is_none()`；
   - 取 `runs.last().session_id` → `api::chat_resume_session(b, Some(&sid))`；
   - 不改任务状态（续跑仍属于同一个 run，`ended_at` 保持 None 直到
     `apply_workflow_finish` 或 abort 收尾）；
   - history `push_note`「重试执行（断点续跑 session <sid>）」。
   - session 已被删 → `chat_resume_session` 返回 404，前端 toast 提示改走
     「中止」回 todo 重派。

4. **`abort_task` 不变**：仍把 `in_progress` + 未结束 run 收尾回 `todo`——
   「中止」是用户明确放弃这次执行、要重新派发的语义，与「重试」正交。

### 前端（`task_board.ts` / `api.ts`）

- `STATE_ACTIONS.in_progress`：`[{open_session 查看对话}, {resume ↺ 重试 primary},
  {abort 中止执行 danger}, {delete 删除任务 danger}]`。
- `actionRequest`：`resume` → `POST /api/tasks/:id/resume`。
- `isDispatchAction` 不含 `resume`（续跑 ≠ 派发新 session）。
- `doAction` 通用分支即可；toast「已请求断点续跑」。

## 并发闸说明（已修正）

`in_progress` 计数 = 并发槽。**中断任务占一个槽是正确语义**——它确实绑定着
一个待续跑的 session，只是暂停了。用户重试→继续占；中止→释放并回 todo。
这与「死任务占槽堵死调度」不同：中断任务**有一条活的出路**（重试/中止），
不是死锁。若担心中断任务长期占槽，交给「中止」释放——但默认保留绑定。

## 不做

- 不加 interrupted 状态 / 新列。
- 不删 session（保留断点续跑与「查看对话」）。
- 不自动重派/自动重试（重启仍不烧 token，等用户点重试）。

## 测试

- `reconcile_interrupted_runs`：中断任务**保持** `in_progress`，只加 history
  note，run 不动，`dispatch_pending=false`。
- `resume_task`：`in_progress` + 死 session → 调通 `chat_resume_session`
  （workflow 任务能从 checkpoint 续跑）；非 `in_progress` 返回 400。
- `effective_actions` / `STATE_ACTIONS`：`in_progress` 含 `resume`。
- 更新 `task_board.test.ts` in_progress 期望动作列表。
