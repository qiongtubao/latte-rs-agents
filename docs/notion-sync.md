# Notion 同步（latte-rs-agents → latte-rs-notion-client）

任务看板的变更镜像到本地 `latte-rs-notion-client` 的 `/api/ext/{ns}/records/{id}` 端点。
Notion 侧每个任务 = `📦 agents-tasks` 根页下的子页面，props 是 JSON 块、content_md 转 Notion blocks。

## 配置

| 环境变量 | 必填 | 默认 | 说明 |
|---|---|---|---|
| `LATTE_NOTION_TOKEN` | 是 | — | notion-client `~/.config/latte/config.toml` 的 `api_token` |
| `LATTE_NOTION_URL` | 否 | `http://127.0.0.1:3210` | notion-client 监听地址 |

`token` 缺失时整个 sync 静默禁用，server 仍正常启动；dirty 标在内存 **+ 落盘标记** `.latte/.tasks-dirty/<id>`，等 token 设上、网络恢复或下次重启后自然推出去。

## 数据流

```
[User creates/updates/dispatches/reports/aborts a task]
   ↓
TaskStore.create / persist / delete
   ↓ mark_notion_dirty(id)
[TaskStore.notion_dirty: HashSet<String>]  ← 内存 + 落盘标记 `.latte/.tasks-dirty/<id>`（重启/离线不丢）
   ↓
[background task: notion_sync_loop, every 5s]
   ↓
take_notion_dirty() → Vec<String>
   ↓ for each id
upsert_with_retry(http, PUT /api/ext/agents-tasks/records/{id}, 3 retries on 5xx)
   ↓
SyncResult::Ok           → 删盘标记 + 从 dirty 移除
SyncResult::ClientError  → 400/404（记录本身坏），删盘标记 + 移除（不重试）
                          → 401/403/408/429（token 错/限流等瞬时/配置错），**保留**标记等下轮
SyncResult::RetriesExhausted → 5xx/网络，盘标记保留 + 内存标回 dirty 等下一轮
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

- **400/404（记录本身坏）** → 不重试，从 dirty 移除（删标记）。
- **401/403/408/429（token 错/轮换、限流等瞬时/配置错）** → 不删标记，等下轮；修好 token / 限流放开后自动补推（此前一律按 4xx 删标记 → 401 窗口内的改动被永久丢掉，修好也不会补）。
- **5xx/网络** → 3 次重试（500ms 退避）；仍失败标回 dirty 等下轮。
- 重启/离线 dirty 不丢：每次标 dirty 同步写 `.latte/.tasks-dirty/<id>` 标记文件（与 doc 镜像 `.docs-dirty/` 同构）；`TaskStore::load` 扫盘重建内存集，server 起来后 5s 轮自动续推。离线期间的改动也留在盘上，网络恢复后推出去。
- **doc 镜像 record id** 用 `.docs-dirty` 的 marker 文件名（`dirty_marker_name` 已把 `/`→`.` 等转成合法 `[A-Za-z0-9._-]`），不是含 `/` 的 rel_path——后者命中不了 notion-client 单段路由/id 校验，会 404/400。真实路径在 body 的 `props.path`。

## 已知限制（v1）

- **DELETE 镜像**：`task` 删除时只标 dirty；Notion 端旧子页面留着（v1 暂未实现 DELETE 同步请求），留待 v2 加 `sync_dirty_tasks` 的 delete 分支
- **启动 dirty**：ui-server 启动时如果 notion-client 还没起来，第一次 5s 同步会 5xx 失败，标回 dirty，5s 后再试（指数退避可作 v1.1 改进）
- **无冲突解决**：本地和 Notion 端都改时，Notion 端被覆盖（PUT 是 upsert）
- **无 select 同步**：Notion 端手动改的字段不会被拉回本地
