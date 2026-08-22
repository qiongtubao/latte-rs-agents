# Tool / Role / Model 管理重整

> **状态：已完成（2026-08-08 核对）**。角色 CRUD（`POST/DELETE /api/roles`）、
> 工具列表 + 启停（`GET /api/tools`、`POST /api/tools/:id/toggle`）、
> 模型 CRUD（`POST/PATCH/DELETE /api/models`）后端均已落地；前端
> `tools_panel.ts`、`models_panel.ts`、`role_editor.ts`（含新建/删除）齐备。
> 本文档仅作历史背景保留。

## 现状
- 角色编辑（`role_editor.ts`）：可选/编辑已有角色，没有"创建"或"删除"入口
- 工具管理：无独立面板，仅 `RoleTemplate.tools: Vec<String>`，是字符串数组
- 模型管理：无独立面板，`model_chain` 是 free text input，ModelDef 没有 CRUD API

## 目标
1. **角色管理**：增/删/改/查；`POST /api/roles` 新建；`DELETE /api/roles/:id` 删除
2. **工具管理**：列出所有已注册工具（短名 + git.* 展开 + delegate/workflow），可启用/禁用；`PATCH /api/tools/:id`
3. **模型管理**：ModelDef 全字段 CRUD；`POST /api/models`、`PATCH /api/models/:key`、`DELETE /api/models/:key`
4. **持久化**：项目 `.latte/` 优先、全局 `~/.latte/` 兜底，与现有约定一致

## 数据流
- 工具（短名 + description + enabled 标志）→ `.latte/tools.yaml`（项目）+ `~/.latte/tools.yaml`（全局）
- 模型 → `.latte/models.d/<key>.toml`（项目）+ `~/.latte/models.d/<key>.yaml`（全局）
- 角色 → `.latte/agents.d/<id>.toml`（项目）+ `~/.latte/agents.d/<id>.toml`（全局）

## 实施
- 后端：`latte-agent-ui-server/src/api.rs` 新增 5 个端点
- 前端：新增 `tools_panel.ts`、`models_panel.ts`，修改 `role_editor.ts` 加"新建"和"删除"按钮

## 验收
- 创建/删除角色：UI 操作后磁盘出现/消失对应 toml
- 模型 CRUD：UI 编辑后 toml/yaml 同步更新
- 工具 toggle：新 session 的 `allowed_tools` 受 `enabled` 标志影响
- `cargo test` 全过，前端 `pnpm build` 成功

## 2026-08-21 UI session 拆分失败修复审查

- [x] 根因：`task_planner` 的 `read` 事件本身带 `sub_id`，前端将工具调用折叠进对应 `RoleStarted` 状态行；显示在主 session 是折叠状态行设计，不是 subagent 归属丢失。
- [x] 根因：`plan` 提交前的 paths 重叠校验返回中文长错误；`agent.rs` 用 `&detail[..256]` 按字节切片，正好切入中文字符并 panic，导致 submit 步失败、`PlanProposed` 不广播、UI 不弹窗。
- [x] 修复：统一使用 `trace::utf8_safe_prefix` 截断 tool error，并覆盖多字节边界测试。
- [x] 修复：paths 重叠、疑似幻觉路径、结构化 tasks 校验错误归类为 `PermanentExec`，不再用相同参数重试。
- [x] 验证：`cargo test -p latte-agent-core permanent_tool_errors_are_not_retried`、`prefix_never_slices_inside_multibyte_character`、`plan_tool_rejects_overlapping_paths_within_plan`、`trace::tests::jsonl_sink_handles_other_kind_without_panic` 均通过。

### 验证补充

- `cargo test -p latte-agent-core`：489 passed。
- `pnpm test -- --run`：127 passed；`pnpm build`：成功。
- `cargo test -p latte-agent-ui-server plan_tool_rejects_overlapping_paths_within_plan`：通过。
- `chat_hil_pause_resume_inject_rollback` 在两次真实模型 E2E 运行中均因找不到 Paused session JSON 失败；该失败发生在用户 `/pause`/模型 `ask_human` 交互状态断言，未命中本次修改路径。`hil_v13_ask_human_mock_e2e` 通过。未将该外部模型时序问题混入本次修复。
- `rustfmt --check` 未作为门禁：仓库现有大量未格式化文件（检查显示 61/142 个文件），本次改动未运行全仓格式化，避免无关大 diff。

## 2026-08-21 task_planner UI 工具归属修复

- [x] 根因：`chat_impl.ts` 之前把带 `sub_id` 的 subagent 工具事件通过 `appendToolToExecutingRow` 写进顶层 `messagesEl`；另外 `src/chat_impl.js` stale 生成文件会优先覆盖 TS 测试导入，导致旧行为持续存在。
- [x] 修复：`ToolUse/ToolResult/ToolError` 带 `sub_id` 时写入独立 subagent 实时日志面板，不再调用主 session 的 `addMessage` 或 `appendToolToExecutingRow`；无 `sub_id` 的旧主 session 事件保留原行为。
- [x] 修复：清理 session 时清空 subagent 工具日志；删除 `src/chat_impl.js`，避免 stale JS shadow TypeScript 源码。
- [x] 测试：新增 `task_planner/read` 归属测试，并验证主 session 没有 `.message.tool` 与 `.tool-log-line`，subagent 面板包含 `read`。
- [x] 验证：`pnpm test -- --run` 通过（15 files / 128 tests）；`pnpm build` 成功；专项执行行测试通过。

## 2026-08-21 task_planner 子代理详情统一

- [x] 根因：task_planner 之前使用 `subagentOverlay/subagentLog` 的专用实时文本展示；普通 delegate 使用 `main.ts -> fetchSubsession -> renderSubsessionEvent` 的标准 Subsession 面板，两条渲染链路格式不同。
- [x] 修复：移除 task_planner 专用详情浮层路径；带 `sub_id` 的工具事件只保留在 subsession trace，不再渲染到主聊天流。
- [x] 修复：task_planner 的 `RoleStarted` 状态行增加 `📋 详情`，点击统一调用 `onShowSubsession(sub_id, label)`。
- [x] 测试：新增标准 subsession 回调测试，验证 task_planner 详情入口传递正确 `sub_id` 与角色标签。
- [x] 验证：`pnpm test -- --run` 通过（15 files / 128 tests）；`pnpm build` 成功。

## 2026-08-21 workflow / delegate 流程统一

- [x] 普通 delegate 与 workflow speaker 均使用 `DelegateStarted/Finished + sub_id` 表示指派、执行与返回；workflow 仅额外携带 `wf_id`，角色来源由 step 固定配置决定。
- [x] UI 的 `WorkflowStep` / `WorkflowTurn` 改为编排元数据，不再生成第二套 subagent 聊天气泡；实际指派卡片统一由 `DelegateStarted` 渲染，详情统一由 `fetchSubsession(sub_id)` 渲染。
- [x] task_planner 继续走 workflow 固定角色路径，无专用执行协议或详情展示路径。
- [x] 回归测试覆盖普通 delegate、workflow 固定角色和 task_planner 详情入口。
- [x] 验证：`pnpm test -- --run` 通过（15 files / 129 tests）；`pnpm build` 成功；`cargo test -p latte-agent-core` 通过（489 tests）。

## 2026-08-21 workflow task_planner 返回显示修复

- [x] 根因：session 事件含 `WorkflowTurn(role_id=task_planner)`，但 UI 只累积 transcript，没有创建角色返回气泡；因此页面只剩 manager 指派和 advisor 输出。
- [x] 修复：恢复 `WorkflowTurn` 角色气泡，并通过 `reference` 引用对应的 `DelegateStarted` 指派消息。
- [x] 修复：`WorkflowStep` 与同 workflow 的 `DelegateStarted` 按角色/FIFO 关联，避免额外生成重复指派卡片。
- [x] 测试：新增 task_planner workflow 返回可见、引用块存在的回归测试。
- [x] 验证：`pnpm test -- --run` 通过（15 files / 130 tests）；`pnpm build` 成功；未生成 `src/chat_impl.js` stale shadow 文件。

## 2026-08-21 advisor 右键执行日志修复

- [x] 根因：advisor 输出是无 `sub_id` 的 `RoleTurn`，右键菜单原先只对 `status && 无 subId` 行显示“查看本次执行日志”，advisor 气泡因此被隐藏。
- [x] 修复：所有无 `sub_id` 的顶层消息均显示 session 执行日志入口；有 `sub_id` 的子代理消息继续使用 subsession 日志入口。
- [x] 修复：session history 使用 ChatEvent 专用渲染器，正确显示 `RoleTurn`、`Status`、`DelegateStarted/Finished`、工具事件等，不再按 TraceEvent 格式解析导致空白。
- [x] 验证：`pnpm test -- --run` 通过（15 files / 132 tests）；`pnpm build` 成功。

## 2026-08-21 刷新页面后导入任务丢失父任务修复

- [x] 根因：`chat_impl.ts` 导入弹窗只从 `api.ts` 的进程内 `refineParents` Map 读取父任务；刷新页面会重新加载模块并清空 Map，弹窗随后把 `parent_id` 留空。服务端虽已支持按 `session_id` 从 `refine_parents` 恢复父任务，但前端映射丢失时未保留本地映射用于 UI 预选与提示。
- [x] 修复：`api.ts` 将拆分会话 → 父任务映射持久化到按 workspace 隔离的 localStorage；读取优先内存缓存，刷新后按 session id 惰性恢复。服务端 `session_id` 回退链路保持不变，显式选择根任务/具体父任务的三态行为不变。
- [x] 红灯：新增刷新模拟测试，修复前 `refineParentFor("sess-refine-1")` 返回 `undefined`；修复后通过。
- [x] 验证：前端 `transport.test.ts` 9/9 通过；服务端 `import_with_parent_id` 相关测试 2/2 通过。

### 审查

- 变更范围仅限前端 refine 映射持久化与回归测试；后端已有的 `session_id` → `refine_parents` 兜底链路未改动。
- 显式父任务、显式根任务、自动关联三态请求语义保持不变。
- 前端生产构建、类型检查、全量 UI 测试、全量 ui-server 单元测试均通过。

## 2026-08-21 task_refine gate 契约修复

- [x] gate 使用明确三态：`VERDICT: ACCEPT` 放行、`VERDICT: REVISE` 回 refine、`VERDICT: REJECT` 终止 workflow。
- [x] gate 不再重写或复述完整草案；submit 消费最新 `draft`，避免复制未修正内容。
- [x] 串行与 DAG workflow 引擎对显式 `VERDICT: REJECT` 立即失败；旧返工测试改用 `VERDICT: REVISE`。
- [x] 新增 task_refine 配置契约测试与 REJECT 终止回归测试，并同步 `.latte/workflows.d` 与 `config/workflows`。
- [x] 验证：`cargo test -p latte-agent-core` 通过，491 tests；`workflow::loop_tests` 通过，9 tests；`task_refine_` 通过，2 tests；`git diff --check` 通过。
