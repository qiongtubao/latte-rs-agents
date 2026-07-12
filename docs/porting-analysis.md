# latte-rs-agents UI → latte-code-editor 移植分析

## 两个项目的 UI 架构对比

| 维度 | latte-rs-agents/ui (源) | latte-code-editor (目标) |
|------|------------------------|------------------------|
| UI 框架 | Vanilla TypeScript + DOM | React 19 + TypeScript |
| 通信方式 | HTTP REST + SSE | Tauri IPC (invoke + listen) |
| 状态管理 | 闭包变量 + DOM 属性 | Zustand store |
| 事件模型 | `EventSource` (SSE) | `@tauri-apps/api/event.listen()` |
| 渲染 | `document.createElement` | React `useState` + JSX |
| 构建 | Vite | Vite (Tauri) |
| 后端 | Rust axum HTTP | Rust Tauri commands |

## 需要移植的核心功能

1. **@role 派发** — `@programmer 消息` 自动切角色 + subsession 弹窗
2. **活动追踪** — status pill 显示 `🔧 read` / `⏳ 委托中`
3. **300s 超时** — 5分钟倒计时 + 进度条
4. **角色头像** — 消息左侧 32px avatar circle
5. **Subsession 弹窗** — 右键/点击弹出执行过程
6. **耗时显示** — `· ⏱ 12秒` 在 token 行末
7. **委托失败** — `❌` + 红色样式

## 关键事件映射

```
SSE Event (源)              → Tauri Event (目标)
─────────────────────────────────────────────────────
RoleTurn                    → chat:controller_event (type: "roleTurn")
Status                      → chat:controller_event (type: "status")
ToolUse/ToolResult          → chat:controller_event (type: "toolUse"/"toolResult")
DelegateStarted/Finished    → chat:controller_event (type: delegate events)
Prompt                      → chat:controller_event (type: "prompt")
Done/Error                  → chat:controller_event (type: "done"/"error")
```

## 技术风险

1. **事件格式差异** — SSE 用内部标记 `{"type":"RoleTurn",...}`，Tauri 事件用 `{"kind":"roleTurn",...}`
2. **SSE 保持连接 vs Tauri 事件** — SSE 自动重连，Tauri 事件需要手动管理 listener 生命周期
3. **React 状态管理** — vanilla DOM 的直接操作需转为 React 受控组件
4. **@role 切换** — HTTP `/api/chat/role` 需映射为 Tauri `chat_controller_submit`
