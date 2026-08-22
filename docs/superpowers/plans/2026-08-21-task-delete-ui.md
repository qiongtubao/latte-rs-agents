# 任务删除 UI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在任务看板详情抽屉中支持安全删除任务和子任务，并将删除任务归档而非物理删除。

**Architecture:** 复用已有前端 `deleteTask()` 和后端 DELETE API。前端将删除作为危险动作显示在详情抽屉，二次确认后请求并刷新；后端在删除前拒绝有子任务的父任务，保证不会产生孤儿任务。

**Tech Stack:** TypeScript、Vitest、Rust、Axum、现有 TaskStore。

---

### Task 1: 锁定删除动作行为

**Files:**
- Modify: `latte-agent-cli/ui/src/task_board.test.ts`
- Test: existing Vitest suite

- [ ] **Step 1: 写失败测试**
  - 断言 `STATE_ACTIONS` 每个状态包含 `delete` 危险动作。
  - 断言 `actionRequest("delete", "LAT-1")` 返回 Promise，而不是 `null`。
  - 断言 `actionToast("delete", "LAT-1")` 包含任务 ID 和“归档”。
- [ ] **Step 2: 运行 `npm test -- --run src/task_board.test.ts`，确认因未注册 delete action/API 映射而失败。**

### Task 2: 接入前端删除按钮和确认流程

**Files:**
- Modify: `latte-agent-cli/ui/src/task_board.ts`

- [ ] **Step 1: 实现最小前端动作**
  - 从 `./api` 导入已有 `deleteTask`。
  - 在 `STATE_ACTIONS` 各状态加入 `{ key: "delete", label: "删除任务", kind: "danger" }`，满足任务和子任务详情都可见。
  - 在 `actionRequest` 映射 `delete` 到 `deleteTask(taskId)`。
  - 在 `actionToast` 返回 `${taskId} 已删除并归档`。
  - 在 `doAction` 对 delete 先执行 `window.confirm("确认删除 ${task.id}？删除后任务将移入 archive，不能在看板中继续使用。")`；取消直接返回；成功后关闭抽屉、刷新列表；失败保留抽屉并显示 toast。
- [ ] **Step 2: 运行同一 Vitest，确认通过。**

### Task 3: 保护父任务删除

**Files:**
- Modify: `latte-agent-ui-server/src/tasks.rs`
- Test: `latte-agent-ui-server/src/tasks.rs`

- [ ] **Step 1: 写失败测试**
  - 创建父任务和子任务。
  - 调用 `store.delete(&parent.id)`，断言返回错误且包含“子任务”。
  - 断言父、子任务仍在内存中，原始 JSON 仍在任务目录，archive 中没有父任务文件。
- [ ] **Step 2: 运行 `cargo test -p latte-agent-ui-server parent_delete_is_rejected_when_children_exist`，确认失败。**
- [ ] **Step 3: 在 `TaskStore::delete` 开始处先调用 `children_of(id)`；非空时返回“任务存在子任务，不能删除，请先删除或处理子任务”；只有叶子任务才执行现有 archive 流程。**
- [ ] **Step 4: 重跑目标测试及既有 `delete_moves_file_to_archive`，确认通过。**

### Task 4: 回归验证

**Files:**
- No additional files

- [ ] **Step 1: 运行 `cd latte-agent-cli/ui && npm test -- --run src/task_board.test.ts`。**
- [ ] **Step 2: 运行 `cargo test -p latte-agent-ui-server tasks::tests::delete_moves_file_to_archive` 和父任务拒绝测试。**
- [ ] **Step 3: 运行 `cd latte-agent-cli/ui && npm run typecheck`。**
- [ ] **Step 4: 若本地 UI 可启动，打开任务抽屉，分别验证叶子子任务显示删除按钮、确认取消不请求、确认后任务消失；父任务删除显示服务端错误且子任务不变。**
