# 移植指南：将 latte-rs-agents 嵌入 latte-code-editor

> 本文档说明如何将 `latte-rs-agents` 的核心 AI Agent 运行时移植到 Tauri 桌面应用 `latte-code-editor` 中。

---

## 架构总览

```
                        ┌─────────────────────────────────┐
                        │       latte-code-editor          │
                        │  ┌───────────────────────────┐   │
                        │  │  Tauri Frontend (React)   │   │
                        │  │  listen("chat:controller  │   │
                        │  │    _event")                │   │
                        │  └──────────┬────────────────┘   │
                        │             │ Tauri IPC           │
                        │  ┌──────────▼────────────────┐   │
                        │  │  Tauri Commands (Rust)    │   │
                        │  │  commands.rs: Tauri →     │   │
                        │  │    ChatController adapter │   │
                        │  └──────────┬────────────────┘   │
                        │             │                     │
                        │  ┌──────────▼────────────────┐   │
                        │  │  latte-agent-core (crate)  │   │
                        │  │  ChatController            │   │
                        │  │  AgentRunner               │   │
                        │  │  prompts::for_role()       │   │
                        │  └───────────────────────────┘   │
                        └─────────────────────────────────┘
```

**关键思路**：不复制代码，而是把 `latte-agent-core` 当作 Rust crate 依赖。`latte-code-editor` 直接调用其 `ChatController`，用 Tauri 的 `window.emit()` 替代 axum SSE。

---

## 第一步：Cargo 依赖

在 `latte-code-editor/src-tauri/Cargo.toml` 中添加：

```toml
[dependencies]
latte-agent-core = { path = "../../latte-rs-agents/latte-agent-core" }
latte-ai = { path = "../../latte-rs-model-router/latte-ai" }
latte-rs-agent-tools = { path = "../../latte-rs-agent-tools" }
```

**版本说明**：使用 `path` 引用以便在开发时同步改动。发布时需要改为 git 依赖或 crates.io 版本。

**核心导入清单：**

```rust
use latte_agent_core::config::AgentConfig;
use latte_agent_core::controller::{ChatController, ControllerConfig, ChatEvent};
use latte_agent_core::model_resolver::{ModelResolver, ModelTier};
use latte_agent_core::session::SessionRecord;
use latte_agent_core::prompts;        // 内嵌的 prompt 常量
use latte_agent_core::role::RoleTemplate;
use latte_ai::params::GenerateParams;
```

---

## 第二步：控制器适配器

`latte-code-editor` 用 `controller_adapter.rs` 包装 ChatController：

```rust
// src-tauri/src/chat_panel/controller_adapter.rs
use latte_agent_core::controller::{ChatController, ControllerConfig, ChatEvent};
use tokio::sync::broadcast;

pub struct ControllerAdapter {
    controller: ChatController,
    event_tx: broadcast::Sender<ChatEvent>,
}

impl ControllerAdapter {
    pub async fn spawn(config: ControllerConfig) -> Self {
        let (event_tx, _) = broadcast::channel(256);
        let controller = ChatController::spawn(config, event_tx.clone()).await;
        Self { controller, event_tx }
    }

    /// 把 ChatEvent 桥接到 Tauri 事件系统
    pub fn subscribe_events(app_handle: &tauri::AppHandle, rx: broadcast::Receiver<ChatEvent>) {
        tokio::spawn(async move {
            let mut rx = rx;
            while let Ok(event) = rx.recv().await {
                let payload = ControllerEventPayload::from(event);
                let _ = app_handle.emit("chat:controller_event", &payload);
            }
        });
    }
}

/// ChatEvent → 前端可消费的 JSON 格式
#[derive(Serialize)]
pub struct ControllerEventPayload {
    #[serde(flatten)]
    inner: ChatEvent,  // externally-tagged JSON, 前端再做转换
}
```

---

## 第三步：事件协议映射

`latte-rs-agents` 使用 **externally-tagged** JSON（`{"Status":{"message":"..."}}`），
`latte-code-editor` 前端需要一个 **internally-tagged** 格式（`{"type":"Status","message":"..."}`）。

### 事件名映射

| latte-rs-agents SSE event | latte-code-editor Tauri event |
|---|---|
| `chat_event` | `chat:controller_event` |
| — | `chat:turn`（单轮聊天） |
| — | `chat:tool_event`（工具调用） |
| — | `chat:swarm_event`（群聊） |
| — | `chat:manager_status`（管理器状态） |
| — | `chat:need_decision`（需人工决策） |

### 事件转换（Rust 端）

```rust
// 在 controller_adapter.rs 中
fn chat_event_to_frontend_json(ev: &ChatEvent) -> serde_json::Value {
    // 将 externally-tagged {"Variant":{...}} 转为 internally-tagged {"type":"Variant",...}
    match ev {
        ChatEvent::RoleTurn { role_id, content, is_complete } => json!({
            "type": "RoleTurn", "role_id": role_id, "content": content, "is_complete": is_complete
        }),
        ChatEvent::Status { message } => json!({
            "type": "Status", "message": message
        }),
        ChatEvent::Done => json!({ "type": "Done" }),
        // ... 其余变体类似
    }
}
```

### 事件转换（TypeScript 端）

```typescript
// src/api/chat.ts — React 侧的适配器
export type ChatEvent =
  | { type: "RoleTurn"; role_id: string; content: string; is_complete: boolean }
  | { type: "Status"; message: string }
  | { type: "Done" }
  // ... 其余变体

export class ReactChatEventAdapter {
  static applyChatEvent(store: ChatStore, event: ChatEvent): void {
    switch (event.type) {
      case "RoleTurn":
        store.appendMessage({ role: event.role_id, content: event.content });
        break;
      case "Status":
        store.setStatus(event.message);
        break;
      case "Done":
        store.setSessionState("done");
        break;
      // ...
    }
  }
}
```

---

## 第四步：Tauri 命令清单

以下 Tauri IPC 命令封装了 `latte-rs-agents` 的功能。每个命令的 Rust handler 在 `src-tauri/src/chat_panel/commands.rs` 中。

### 角色与模型

| 命令 | 说明 |
|---|---|
| `chat_list_roles` | 列出所有可用角色 |
| `chat_list_models` | 列出可用模型 |
| `chat_get_role_config` | 获取单个角色的配置 |
| `chat_set_role_model` | 为角色设置模型 |
| `chat_set_role_model_chain` | 设置角色的模型回退链 |
| `chat_set_default_model` | 设置全局默认模型 |

### 聊天控制

| 命令 | 说明 |
|---|---|
| `chat_start_discussion` | 启动多角色讨论 |
| `chat_continue` | 继续当前对话 |
| `chat_cancel` | 取消当前生成 |
| `chat_stream` | 单角色流式聊天 |

### 控制器（Controller）

| 命令 | 说明 |
|---|---|
| `chat_controller_spawn` | 生成一个新的 ChatController |
| `chat_controller_submit` | 向控制器提交消息 |
| `chat_controller_pause` | 暂停控制器 |
| `chat_controller_resume` | 恢复控制器 |
| `chat_controller_abort` | 中止控制器 |

### HIL 会话（Human-In-The-Loop）

| 命令 | 说明 |
|---|---|
| `chat_hil_start` | 启动 HIL 会话 |
| `chat_hil_send` | 发送消息到 HIL 会话 |
| `chat_hil_edit_message` | 编辑会话中的消息 |
| `chat_hil_inject` | 向特定角色注入消息 |
| `chat_hil_continue` | 继续 HIL 会话 |
| `chat_hil_transition` | 切换角色/状态转换 |
| `chat_hil_get_state` | 获取当前 HIL 状态 |

### 会话管理

| 命令 | 说明 |
|---|---|
| `chat_session_list` | 列出所有会话 |
| `chat_session_get` | 获取单个会话详情 |
| `chat_session_delete` | 删除会话 |
| `chat_session_edit_message` | 编辑会话中的历史消息 |

### 工作流

| 命令 | 说明 |
|---|---|
| `chat_save_workflow` | 保存工作流 |
| `chat_delete_workflow` | 删除工作流 |
| `chat_list_workflows` | 列出工作流 |
| `chat_get_workflow_full` | 获取完整工作流详情 |

### 群聊（Swarm）

| 命令 | 说明 |
|---|---|
| `chat_start_swarm` | 启动群聊模式 |
| `chat_start_manager_session` | 启动 manager 驱动的多角色会话 |
| `chat_submit_user_decision` | 提交用户决策（暂停后） |
| `chat_submit_user_continue` | 继续执行（暂停后） |

---

## 第五步：配置层移植

`latte-rs-agents` 的配置系统有三个层级，移植时按相同优先级：

```
CLI 参数（最高） > 项目配置（.latte/agents/） > 全局配置（$LATTE_HOME/）
```

在 `latte-code-editor` 中，Tauri 命令的调用流程：

```rust
// src-tauri/src/chat_panel/global_config.rs
use latte_agent_core::config::AgentConfig;
use latte_agent_core::global_config::GlobalConfig;

pub fn load_merged_config() -> AgentConfig {
    // 1. 加载全局配置（$LATTE_HOME/models.yaml + $LATTE_HOME/agents.d/）
    let global = GlobalConfig::load().unwrap_or_default();

    // 2. 加载项目配置（.latte/agents/）
    let project = AgentConfig::load_project().unwrap_or_default();

    // 3. 合并（项目配置覆盖全局配置的同名字段）
    let merged = project.merge(global);

    // 4. 回退到内建模板（built-in RoleTemplate）
    if merged.roles.is_empty() {
        merged.fallback_to_builtin()
    }

    merged
}
```

---

## 第六步：前端渲染适配

`latte-code-editor` 前端使用 React + Zustand 替代 latte-rs-agents 的纯 DOM 渲染。

```typescript
// src/stores/chatStore.ts
import { listen } from "@tauri-apps/api/event";

interface ChatStore {
  messages: Message[];
  status: string;
  sessionState: "idle" | "active" | "paused" | "done";
}

// 订阅 Tauri 事件
const unlisten = await listen<ChatEvent>("chat:controller_event", (event) => {
  const store = useChatStore.getState();
  ReactChatEventAdapter.applyChatEvent(store, event.payload);
});
```

**前端文件对应关系：**

| latte-rs-agents (vanilla TS) | latte-code-editor (React) |
|---|---|
| `chat_impl.ts`（`mountChat`） | `src/components/ChatPanel.tsx` |
| `self-loop.ts` | `src/components/SelfLoopPanel.tsx` |
| `trace.ts` | `src/components/TraceViewer.tsx` |
| `role_graph.ts` | `src/components/RoleGraph.tsx` |
| `api.ts`（fetch + EventSource） | `src/api/chat.ts`（Tauri invoke + listen） |
| `main.ts` | `src/App.tsx` |

---

## 第七步：构建与测试

```bash
# 1. 构建 latte-rs-agents（确保最新）
cd latte-rs-agents
cargo build

# 2. 构建 latte-code-editor
cd latte-code-editor
cargo tauri build

# 3. 开发模式
cargo tauri dev

# 4. 运行 TDD 测试（在 latte-code-editor 中）
cd src-tauri
cargo test
```

---

## 常见问题

### Q: 为什么用 `path` 依赖？

开发时修改 `latte-rs-agents/prompts/` 下的 markdown prompt，`latte-code-editor` 会在下一次 `cargo build` 时自动获取更新。`prompts.rs` 使用 `include_str!` 编译时嵌入，无需文件 I/O。

### Q: 移植后 SSE 怎么处理？

`latte-code-editor` 不需要 SSE。`ChatController` 的 `broadcast::Receiver` 直接转发到 Tauri 的 `AppHandle::emit()`。这是比 SSE 更简单的单进程 IPC。

### Q: 移植时需要保留 `latte-agent-cli` 吗？

不需要。`latte-code-editor` 只依赖 `latte-agent-core` crate。`latte-agent-cli` 是独立的 CLI 二进制入口，不影响库的使用。

### Q: 事件丢失怎么办？

`broadcast::channel` 是有容量限制的（默认 256）。如果消费端处理慢，旧事件会被丢弃。建议：
- 在 Tauri 命令 handler 中用 `tokio::spawn` 异步消费
- 设置适当的 channel 容量
- 关键事件（如 `Done`）不会因为队列满而丢失——前端在收到 `Done` 之前应确保所有消息已处理
