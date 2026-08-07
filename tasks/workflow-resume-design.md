# Workflow Resume 后端链路设计

## 1. 背景

Workflow 失败时，`run_workflow_inner` 会把已完成步骤写入 checkpoint (`<cwd>/.latte/workflow-runs/<wf_id>.jsonl`)。
`run_workflow_resume` 读取该 checkpoint，跳过已完成步骤，把已完成的 output_key→output 注入 vars，
从断点继续执行。**目前只有 agent 的 `workflow` tool 能触发 resume**（通过 `resume` 参数），
UI 没有途径传入 wf_id 触发续跑。

## 2. 后端链路总览

```
用户点击"续跑"按钮
  → 前端 POST /api/workflows/:name/resume { wf_id, session_id?, topic? }
  → handlers.rs: workflow_resume_h (HTTP 薄壳)
  → api.rs: workflow_resume (协议无关逻辑)
  → 获取 session 的 event_tx → 构造 WorkflowRunContext
  → spawn run_workflow_resume(wf, topic, ctx, wf_id)
  → 事件通过 session 的 broadcast → SSE / event_log
```

## 3. 关键数据流

### 3.1 Checkpoint 格式 (`workflow.rs:596-618`)

JSONL 文件，首行是 Meta 记录，后续每行一条 Step 记录：

```json
// 首行
{"type":"meta","wf_id":"wf-learn-1728390123456","workflow_name":"learn","topic":"xxx","started_at":1728390123}
// 已完成 step 行
{"type":"step","wf_id":"wf-learn-...","workflow_name":"learn","topic":"xxx","step_id":"analyze","output_key":"analysis","output":"...", "finished_at":1728390456}
```

`load_checkpoint` 从 checkpoint 恢复 `CheckpointState { workflow_name, topic, completed: Vec<(step_id, output_key, output)> }`。

### 3.2 `run_workflow_resume` 签名 (`workflow.rs:807-814`)

```rust
pub async fn run_workflow_resume(
    wf: &WorkflowDef,
    topic: &str,             // 可为空（回退到 checkpoint topic）
    ctx: &WorkflowRunContext,
    resume_wf_id: &str,      // 失败 run 的 wf_id
) -> Result<String, String>
```

内部调用 `run_workflow_inner(wf, topic, ctx, Some(resume_wf_id))`，与 `run_workflow` 共享完全相同的引擎路径，区别仅在于 `resume_wf_id` 参数。

### 3.3 `WorkflowRunContext` 构造 (`workflow.rs:520-531`)

```rust
pub struct WorkflowRunContext {
    pub merged: Arc<AgentConfig>,
    pub resolver: Arc<ModelResolver>,
    pub default_params: GenerateParams,
    pub cwd: PathBuf,
    pub event_tx: broadcast::Sender<ChatEvent>,
    pub cancel_flag: Arc<AtomicBool>,
    pub depth: u8,
}
```

### 3.4 事件路由

关键洞察：**resume 必须复用 session 的 event broadcast**，不能走 standalone `WorkflowRunState`（测试运行专用）。

理由：
- 用户从 chat 的「WorkflowFinished（失败）」气泡点续跑，事件应回到该 session 的 SSE 流
- `session_event_sender(&b, session_id)` 返回 `broadcast::Sender<ChatEvent>`，该 sender 已在 session 的 SSE handler 中由 `subscribe_session` 订阅
- 任务看板 (`tasks.rs:939-970`) 已验证此模式：获取 session event_tx → 构造 WorkflowRunContext → spawn run_workflow → 事件自动进 session 的 SSE 和 event_log 归档

路由路径：
```
run_workflow → event_tx.send(WorkflowStarted/Step/Turn/Finished)
  → session 的 broadcast channel
    → SSE handler (events_sse) 收到 → 推向前端 EventSource
    → event_log 环形缓冲 → 落盘
```

## 4. 设计决策

### 决策 1: 新 API 端点

| 条目 | 值 |
|------|-----|
| 路径 | `POST /api/workflows/:name/resume` |
| 请求体 | `{ "wf_id": "wf-learn-...", "session_id": "ui-...", "topic": "..." }` |
| 响应体 | `{ "started": true, "run_id": "wf-learn-..." }` |
| 状态码 | 202 Accepted（异步启动）, 404（workflow/checkpoint 不存在）, 400（校验失败）, 409（冲突） |

请求体字段说明：
- `wf_id`: **必填**。失败 run 的 checkpoint wf_id（取自 `WorkflowFinished` 事件或错误消息的 `wf_id=...`）
- `session_id`: **必填**。哪个 session 的事件流上跑这个 resume。前端从 `WorkflowFinished` 事件所在的 session 取
- `topic`: **可选**。覆盖 topic，空时回退到 checkpoint 中的 topic

### 决策 2: `context_assembly` — 从 `UiBackend` 获取组件

```rust
// 1. 获取 session 的 event_tx
let event_tx = api::session_event_sender(&b, &session_id).await?;

// 2. 构造 WorkflowRunContext（复用 tasks.rs 的现成模式）
let ctx = WorkflowRunContext {
    merged: Arc::new(b.merged.read().clone()),
    resolver: b.resolver.clone(),
    default_params: GenerateParams::default(),
    cwd: b.cwd.clone(),
    event_tx,
    cancel_flag: Arc::new(AtomicBool::new(false)),
    depth: 0,
};
```

注意：`cancel_flag` 需要从 `WorkflowRunEvent` 端（如 abort）可访问，因此应存入 `UiBackend` 的某字段，类似 `workflow_run` 的 cancel 模式，或复用 `tasks.rs` 中的 `workflow_cancels: HashMap<String, Arc<AtomicBool>>`。

### 决策 3: 事件路由 — 复用 session 的 broadcast channel

走 `session_event_sender` 获取 session controller 的 `broadcast::Sender<ChatEvent>`。

`run_workflow` 产生的全部 `WorkflowStarted/Step/Turn/Finished` 事件直接进入该 channel：
- 前端已有 SSE handler (`events_sse`) 监听该 channel，收到事件后通过 `chat_event_to_frontend_json` 序列化推给浏览器
- 事件也进入 session 的 event_log 环形缓冲，切 tab 回来仍然可见
- 不需要新建独立 SSE 流

### 决策 4: 代码位置

| 层 | 文件 | 函数 |
|----|------|------|
| 协议无关逻辑 | `latte-agent-ui-server/src/api.rs` | `workflow_resume()` |
| HTTP 薄壳 | `latte-agent-ui-server/src/handlers.rs` | `workflow_resume_h()` |
| 路由注册 | `latte-agent-ui-server/src/lib.rs` | `build_router()` 中 `"/workflows/:name/resume"` |

### 决策 5: 校验与安全

1. **wf_id 有效性**：`load_checkpoint` 已做安全检查（拒绝含 `/`、`\`、`..` 的 wf_id），直接调用它
2. **workflow 存在性**：`load_workflow(&name, &cwd)` 返回 404
3. **workflow_name 匹配**：checkpoint 的 `workflow_name` 必须与 URL 的 `:name` 一致（`load_checkpoint` 已在 `run_workflow_resume` 内部校验）
4. **session 归属**：`session_event_sender` 通过 `resolve_session` 校验 session 存在，不存在返回 404
5. **并发保护**：已有 test run 的 `workflow_run` 是独立的；但 session 上同时只能跑一个 workflow（否则事件交错）。建议在 `UiBackend` 上维护一个 `running_workflows: Arc<RwLock<HashMap<String, Arc<AtomicBool>>>>` 来防止同一 session 并发跑多个 workflow

### 决策 6: 前端绑定

前端在 `WorkflowFinished` 事件渲染失败状态时，添加一个"🔄 续跑"按钮：
- 点击后调用 `POST /api/workflows/<name>/resume`，body 为 `{ "wf_id": "...", "session_id": "..." }`
- name 和 wf_id 可从 `WorkflowFinished` 事件中提取（`WorkflowFinished` 事件应包含 `wf_id` 和 `workflow_name`）
- 接口返回后，前端复用现有的 SSE 连接（无需重新订阅），等待新一轮的 `WorkflowStarted` → ... → `WorkflowFinished` 事件

## 5. 实现概要

### 5.1 api.rs 新增函数

```rust
/// `POST /api/workflows/:name/resume` 的请求体。
#[derive(Deserialize)]
pub struct WorkflowResumeRequest {
    /// 失败 run 的 checkpoint wf_id（来自 WorkflowFinished 事件中的 wf_id）。
    pub wf_id: String,
    /// 哪个 session 的事件流上跑这个 resume。
    pub session_id: String,
    /// 可选 topic，空时回退到 checkpoint 中的 topic。
    #[serde(default)]
    pub topic: Option<String>,
}

/// 校验后启动 workflow resume 的异步任务。
/// 事件通过 session 的 broadcast channel 推送，前端无需新建 SSE 连接。
pub fn workflow_resume(
    b: &UiBackend,
    name: &str,
    req: WorkflowResumeRequest,
) -> Result<serde_json::Value, ApiError> {
    // 1. 校验 session 存在（获取 event_tx）
    // 2. 校验 workflow 存在
    // 3. 校验 checkpoint 存在（load_checkpoint 含安全检查）
    // 4. 构造 WorkflowRunContext
    // 5. 注册 cancel 句柄
    // 6. spawn run_workflow_resume
    // 7. 返回 202
}
```

### 5.2 handlers.rs 新增函数

```rust
pub(crate) async fn workflow_resume_h(
    axum::extract::Path(name): axum::extract::Path<String>,
    State(state): State<AppState>,
    Json(req): Json<api::WorkflowResumeRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    api::workflow_resume(&state.backend, &name, req)
        .map(Json)
        .map_err(Into::into)
}
```

### 5.3 lib.rs 路由注册

在 `build_router` 的 workflows 区块添加：

```rust
.route("/workflows/:name/resume", post(workflow_resume_h))
```

## 6. 风险与注意

- **cancel_flag 生命周期**：resume 启动后需要能在 session 级 abort 时取消。建议将 cancel_flag 存入 `UiBackend` 的 `running_workflows` 映射，在 `chat_abort` 时连带置位
- **并发冲突**：同一 session 同时跑两个 workflow（resume + 新 run）会导致事件交错。建议实现 at-most-one-per-session 的 guard
- **checkpoint 残留**：resume 成功后产生新的 checkpoint 文件（新 wf_id），旧文件保留。可以考虑在 resume 成功时清理旧文件