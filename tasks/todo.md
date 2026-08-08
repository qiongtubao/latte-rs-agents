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
