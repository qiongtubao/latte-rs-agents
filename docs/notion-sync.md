# Notion 同步（latte-rs-agents → latte-rs-notion-client）

任务看板的变更镜像到本地 `latte-rs-notion-client` 的 `/api/ext/{ns}/records/{id}` 端点。
Notion 侧每个任务 = `📦 agents-tasks` 根页下的子页面，props 是 JSON 块、content_md 转 Notion blocks。

## 配置

| 环境变量 | 必填 | 默认 | 说明 |
|---|---|---|---|
| `LATTE_NOTION_TOKEN` | 是 | — | notion-client `~/.config/latte/config.toml` 的 `api_token` |
| `LATTE_NOTION_URL` | 否 | `http://127.0.0.1:3210` | notion-client 监听地址 |

`token` 缺失时整个 sync 静默禁用，server 仍正常启动；dirty 保留在内存中，等下次 token 设上后重启进程才会推送。

## 数据流

```
[User creates/updates/dispatches/reports/aborts a task]
   ↓
TaskStore.create / persist / delete
   ↓ mark_notion_dirty(id)
[TaskStore.notion_dirty: HashSet<String>]  ← 内存态，不落盘
   ↓
[background task: notion_sync_loop, every 5s]
   ↓
take_notion_dirty() → Vec<String>
   ↓ for each id
upsert_with_retry(http, PUT /api/ext/agents-tasks/records/{id}, 3 retries on 5xx)
   ↓
SyncResult::Ok           → 任务从 dirty 移除
SyncResult::ClientError  → 4xx（401/400），从 dirty 移除（不重试）
SyncResult::RetriesExhausted → 5xx/网络，标回 dirty 等下一轮
```

## 集成点

| 路径 | 标 dirty 方式 |
|---|---|
| `TaskStore::create` | 内部 `self.notion_dirty.mark(&id)` |
| `TaskStore::persist`（update_task / dispatch_task / report_task / abort_task / workflow_finish 等所有改状态的路径） | 内部 `self.notion_dirty.mark(id)` |
| `TaskStore::delete` | 在 `tasks::delete_task` 显式 `mark_notion_dirty(id)`（DELETE 留 v2 真正发 DELETE 请求） |

## 序列化（build_record_body）

```json
{
  "title": "任务标题",
  "props": {
    "state": "in_progress",
    "priority": 1,
    "labels": ["core"],
    "workflow": "tdd_development",
    "scheduled_at": 1234567890  // 可选
  },
  "content_md": "# [LAT-100] ...\n\n**State**: `in_progress` · **Priority**: 1 ...\n\n## 最近变更\n\n- [...]"
}
```

## 重试与失败

- 4xx（401/400/404）→ 不重试，从 dirty 移除（配置/数据错）
- 5xx/网络 → 3 次重试（500ms 退避）；仍失败标回 dirty 等下轮
- 启动后 dirty 不丢：本地 `create/update/delete` 路径都标 dirty，重启进程后无遗言（dirty 是内存态，重启即清空——避免在 server 启动瞬间连发 dirty flush）

## 已知限制（v1）

- **DELETE 镜像**：`task` 删除时只标 dirty；Notion 端旧子页面留着（v1 暂未实现 DELETE 同步请求），留待 v2 加 `sync_dirty_tasks` 的 delete 分支
- **启动 dirty**：ui-server 启动时如果 notion-client 还没起来，第一次 5s 同步会 5xx 失败，标回 dirty，5s 后再试（指数退避可作 v1.1 改进）
- **无冲突解决**：本地和 Notion 端都改时，Notion 端被覆盖（PUT 是 upsert）
- **无 select 同步**：Notion 端手动改的字段不会被拉回本地
