# doc-graph 工具 + write_doc workflow + Notion 文档镜像 — 设计

> 日期：2026-08-09
> 状态：已批准（brainstorming 后）

## 1. 目标

让 latte-rs-agents 的 manager 能通过 4 个工具直接使用 `latte-rs-doc-graph`（`latte-doc-review-graph`），
并提供一个 `write_doc` workflow 组合成完整流程；文档保存到本地（doc-graph 标准 `docs/` 目录），
并镜像到本地 `latte-rs-notion-client` → Notion（`agents-docs` 命名空间）。

## 2. 工具契约

### 2.1 `doc_graph_scan`

重建 doc-graph 知识图谱索引。

- **入参**：`{}`（cwd 取 agent 运行目录）
- **行为**：调 `latte-review scan -C <cwd> --json` 子进程，重建 `.latte-review/graph.json`
- **返回**：`{ok, node_count, edge_count, community_count, orphan_count, graph_path}`

### 2.2 `doc_graph_context`

基于查询，预读相关文档块（5-stage context pipeline）。

- **入参**：`{query, max_docs?, budget?}`
- **行为**：调 `latte-review context -C <cwd> "<query>" --json`
- **返回**：`{ok, index, overview, pages}`（预读 bundle）

### 2.3 `doc_write`

生成 frontmatter + 落盘 `.md` 文档，重建索引，并标 Notion dirty。

- **入参**：`{title, doc_type?, sources?, tags?, body}`
- **行为**：
  1. 生成 frontmatter YAML（`type`, `title`, `sources`, `tags`, `created`, `updated`）
  2. 确定路径：`docs/{entity|concept|feature|...}/<slug>.md`（doc_type 映射子目录，slug = kebab）
  3. 写 `.md`
  4. 写 `.latte/.docs-dirty/<rel_path>` 标记文件
  5. 调 `latte-review scan --json` 重建图
- **返回**：`{ok, path, title, node_count, edge_count}`

### 2.4 `doc_index`

生成 `docs/index.md` 目录表。

- **入参**：`{}` 或 `{rebuild?}`
- **行为**：
  1. 调 `latte-review scan --json`（若 rebuild）或读已有 graph.json
  2. 按 doc_type 分组列目录
  3. 写 `docs/index.md`
- **返回**：`{ok, index_path, doc_count}`

## 3. write_doc workflow

```toml
[[workflow.write_doc]]  # config/workflows/write_doc.toml
# 步骤：
#   doc_write → doc_graph_scan → doc_index → 汇报
```

workflow 步骤由 manager 依次调：`doc_write`（写正文）→ `doc_graph_scan`（重建图）→ `doc_index`（生成目录）→ 汇报摘要。

## 4. Notion 文档镜像

- **命名空间**：`agents-docs`（对应 task 的 `agents-tasks`）
- **触发**：`doc_write` 落盘后写 `<cwd>/.latte/.docs-dirty/<rel_path>` 标记文件
- **ui-server loop**：`notion_sync_loop` 扩展——轮询 `.latte/.docs-dirty/`，读对应 `.md`，`build_doc_record_body` 组装，PUT 到 `/api/ext/agents-docs/records/<rel_path>`，成功后删标记
- **body**：`{title, props: {doc_type, tags, sources}, content_md: <frontmatter + body>}`
- **复用**：`upsert_with_retry`（4xx 不重试 / 5xx 3 次重试 500ms 退避）

## 5. 关键决策

1. **doc-graph 走 CLI 子进程**：不改 `latte-rs-doc-graph` 源码，靠已安装的 `latte-review` binary
2. **工具放 latte-agent-core**：新 `doc_graph_tools.rs` 模块，`build_runner` 里按 role tools 列表条件注册（与 `plan`/`ask`/`delegate` 同款）
3. **markdown 落 `.latte-review/docs/`**（doc-graph 默认）：scan 能识别，无需改 doc-graph 配置
4. **Notion 镜像走 `.docs-dirty/` 标记文件 + ui-server loop 轮询**：与 task notion_sync 解耦但同风格
5. **`doc_graph_context` 预读 bundle**：不是全文读取，是 5-stage 召回的相关块

## 6. 配置

- `config/agents/manager.toml`：tools 列表加 `doc_graph_scan`, `doc_graph_context`, `doc_write`, `doc_index`
- `prompts/manager.md`：加 4 工具使用说明 + write_doc workflow 指引

## 7. 范围

**本阶段做**：
- 4 个 doc-graph 工具（`latte-agent-core/src/doc_graph_tools.rs`）
- `write_doc` workflow（`config/workflows/write_doc.toml`）
- Notion `agents-docs` 镜像（`latte-agent-ui-server/src/notion_sync.rs` 扩展）

**不做**：
- doc-graph lib 读写 API 扩展（不引入 cross-crate 编译依赖，走 CLI）
- UI 面板
- 多项目 wiki 聚合

## 8. 测试

- `doc_graph_tools` 单测：用 wiremock 假 `latte-review` binary（`Command` 注入 PATH 指向假脚本）验证入参/返回
- 4 工具各自单元：frontmatter 生成、slug 计算、路径映射、body 组装
- `write_doc` workflow：配置正确加载 + validate 通过
- Notion doc 镜像：标记文件 → build_doc_record_body → PUT → 删标记（wiremock）
- 全 workspace `cargo test --workspace --lib`
