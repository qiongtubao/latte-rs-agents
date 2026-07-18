# 设计：latte-agent-ui 嵌入 latte-code-editor chat 模块

> 目标：UI 功能的单一事实来源留在本仓库（latte-rs-agents），latte-code-editor 只负责
> 同步构建产物 + 换肤，重新编译即获得最新 UI。不重写、不双维护。

---

## 0. 关键判断：不走 React 重写路线

`docs/porting-guide.md` 第六步描述的路线是把 `chat_impl.ts` 重写成 React 组件
（ChatPanel.tsx / Zustand）。**本设计放弃该路线**，理由：

1. 违背核心目标——React 重写意味着本仓库每次 UI 更新都要人工再移植一遍，永远双维护。
2. 该路线已被实践证明失败：latte-code-editor 的 `ChatPanel.tsx` 当前编译不过
   （合并残留引用未定义符号）、`markdown.ts` 存成 UTF-16 损坏、多个前端 API 后端未注册，
   chat 模块处于重构中间态。
3. 本仓库 UI 是 **vanilla TypeScript + 原生 DOM，零运行时依赖**（`latte-agent-cli/ui`），
   这恰恰是最大资产：它可以原样跑在任何 webview 里，包括 Tauri 的，根本不需要 React。

**总思路：不重写，整体嵌入。传输层做一次性抽象，业务代码一行不动。**

---

## 1. 总体架构（终态）

```
latte-rs-agents/latte-agent-cli/ui/          ← 唯一需要改代码的地方
  ├─ src/chat_impl.ts, main.ts, trace.ts …   业务代码（不变）
  ├─ src/api.ts                              ChatTransport 抽象 + HttpSseTransport（一次性重构）
  ├─ src/styles.css                          颜色全部收敛为 CSS 变量（一次性审计）
  └─ dist/                                   pnpm build 产物（~40KB JS + ~20KB CSS）
        │  构建时同步脚本（path 引用，非 submodule）
        ▼
latte-code-editor/
  ├─ public/chat-ui/                         dist 拷贝 + skin.css（编辑器深色皮肤）
  ├─ src/components/ChatAgentPanel.tsx       iframe 宿主：注入 transport / skin / host hooks
  ├─ src/chatBridge.ts                       编辑器侧单例：持有 iframe ref，供图谱/编辑器反向调用
  └─ src-tauri/src/chat_panel/ui_adapter.rs  REST 端点 ↔ Tauri commands 1:1 适配
                                             事件转换复用 ui.rs 的 chat_event_to_frontend_json
```

两个仓库之间只有 **3 个契约**，各自演进时只需守住契约：

| 契约 | 内容 | 定义位置 |
|---|---|---|
| C1 ChatTransport | `request(method, path, body)` + `subscribeEvents(sessionId, cb)` + `subscribeSelfLoop(cb)` | `ui/src/transport.ts`（新增） |
| C2 事件 JSON 格式 | internally-tagged `{"type":"RoleTurn",…}`，23 个 ChatEvent 变体 | Rust 单一实现 `chat_event_to_frontend_json`，从 `commands/ui.rs:1085` 上移到 `latte-agent-core`，axum 与 Tauri 两侧共用 |
| C3 宿主双向桥 | dist 布局、可选 `skin.css`、`__LATTE_HOST__`（编辑器→UI）与 `__LATTE_UI__`（UI→编辑器）双向接口 + `CodeRef` 类型 | 本文档 §5 |

---

## 2. 分发机制：path 引用 + 构建时同步（与现有 Cargo path dep 同哲学）

不用 git submodule（要手动 bump commit）、不发 npm 包（多一层发布）。两个仓库本来就是
同级目录，且 `src-tauri/Cargo.toml` 已 path 依赖 `../../latte-rs-agents/latte-agent-core`，
前端产物用同样的 path 哲学：

`latte-code-editor/scripts/sync-chat-ui.sh`（新建）：

```bash
#!/usr/bin/env bash
# 1. 构建最新 UI（如 dist 已新于 src 则跳过，用内容哈希判断）
pnpm --dir ../../latte-rs-agents/latte-agent-cli/ui build
# 2. 拷贝产物 + 编辑器皮肤
rsync -a --delete ../../latte-rs-agents/latte-agent-cli/ui/dist/ public/chat-ui/
cp skins/chat-ui-vscode-dark.css public/chat-ui/skin.css
```

挂进 `package.json` 的 `frontend:dev` / `frontend:build` 前置步骤（tauri.conf.json 的
`beforeDevCommand`/`beforeBuildCommand` 已走这两个脚本）。

**效果**：改本仓库 UI → 在 latte-code-editor 里 `pnpm dev` / `tauri build` → 自动用最新 UI。
想换肤 → 只改 `skins/chat-ui-vscode-dark.css` 一个文件。

> ⚠️ 构建脚本必须先清理 `ui/src/**.js`（`tsc` 原地编译的 stale 产物会 shadow `.ts`，
> 已知坑，commit d9685fb）。在 ui 的 `package.json` build 前加 `clean` 步骤。

---

## 3. 传输层抽象（唯一的前端重构）

`api.ts`（399 行）已集中所有后端访问，重构是机械式的：

```ts
// ui/src/transport.ts（新增）
export interface ChatTransport {
  request<T>(method: string, path: string, body?: unknown): Promise<T>;
  subscribeEvents(sessionId: string, onEvent: (ev: ChatEvent) => void): () => void;
  subscribeSelfLoop(onEvent: (ev: SelfLoopEvent) => void): () => void;
}
```

- `HttpSseTransport`：现有 fetch + EventSource 原样搬入，CLI/浏览器模式默认。
- `TauriIpcTransport`：**由宿主（编辑器）实现并注入**，UI 仓库只依赖接口。
  `request` → `invoke("ui_" + 命令名)`；`subscribeEvents` → `listen("ui:chat_event")`。
- `api.ts` 全部函数改为接收 transport 参数（或模块级 `initTransport(t)`），调用点
  （main.ts / chat_impl.ts / trace.ts / role_graph.ts / role_editor.ts / self-loop.ts）
  只改注入方式，不改逻辑。
- REST 端点 ↔ Tauri commands 的 1:1 映射 `docs/bridge-api.ts`（871 行）已整理完毕，
  src-tauri 侧 `ui_adapter.rs` 照表实现即可，session 管理复用现有 `SessionMap` 思路
  （编辑器单面板单 session，更简单）。

事件流：Rust `ChatController` 的 broadcast → `chat_event_to_frontend_json`（上移到
latte-agent-core 共享）→ `app.emit("ui:chat_event", payload)` → iframe 内 transport 的
listen 回调。格式与 SSE 完全一致，UI 业务代码零改动。

---

## 4. 换肤机制

现状：`styles.css`（1256 行）`:root` 已有约 15 个 CSS 变量，但只有一套浅色主题。

一次性工作（本仓库）：
1. 审计 styles.css，把硬编码颜色全部收敛进 `:root` 变量；
2. `index.html` 在 styles.css 之后追加 `<link rel="stylesheet" href="skin.css">`——
   文件不存在时静默跳过（CLI 模式无皮肤 = 默认浅色）。

编辑器仓库：
- `skins/chat-ui-vscode-dark.css` 只重新定义变量值（`--bg:#1e1e1e; --accent:#007acc; …`
  对齐编辑器现有 VS Code 深色），由同步脚本拷为 `public/chat-ui/skin.css`。
- iframe 带来天然的 CSS 隔离，皮肤不会与 Tailwind 互相污染。

---

## 5. 宿主双向桥（C3）与编辑器深度联动

iframe 加载与注入握手（父页面 `ChatAgentPanel.tsx` 实现）：

```ts
// UI 入口 main.ts 顶部（一次性改造）
const host = await waitForHost(); // 已存在 __LATTE_HOST__ 直接返回；
                                  // 否则等 "latte-host-ready" 事件，web 模式 200ms 超时回退 HttpSseTransport
```

iframe vs Shadow DOM：选 **iframe**——样式/全局隔离零成本，皮肤注入简单，`document.*`
调用无需改造；Shadow DOM 需要把全部 `document.getElementById` 改为 root 作用域查询，不值得。

### 5.1 双向桥接口（C3 完整定义）

```ts
// 契约类型在本仓库 ui/src/host.ts 定义；编辑器侧从同步产物中引入类型副本
interface CodeRef {
  path: string;                              // 相对 workspace 根的路径
  startLine?: number; endLine?: number; column?: number;
  symbol?: string;                           // 函数/结构体 qualified name（图谱定位用）
}

interface LatteHost {                        // 方向一：编辑器 → 注入 iframe
  platform: "tauri" | "web";
  transport?: ChatTransport;                 // 阶段二提供；阶段一省略走 HTTP
  workspaceRoot?: string;                    // 活动工作区绝对路径
  sessionKey?: string;                       // 会话持久化键，按工作区隔离（替代裸 localStorage 键）
  openLocation?(ref: CodeRef): void;         // 跳转到代码/文档位置
  revealInGraph?(ref: CodeRef): void;        // 图谱面板定位并高亮节点
  openDoc?(ref: CodeRef): void;              // 文档查看器（.md / doc_gen 产物）
}

interface LatteUiApi {                       // 方向二：iframe 暴露给编辑器（mount 后挂 window.__LATTE_UI__）
  focus(): void;
  insertContext(ref: CodeRef & { quote?: string }): void;  // 把代码引用插进输入框
}
```

UI 对所有 host 能力做特性探测（`if (host.openLocation)`），同一套代码在浏览器（CLI）
和 Tauri（编辑器）下都可用——这是"单一事实来源"能成立的关键。

### 5.2 chat → editor：工具调用跳转到代码位置

数据基础已具备：`ToolUse`/`ToolResult` 事件自带 `tool_name` + `args`/`result` 文本
（`latte-agent-core/src/controller.rs:153-163`），subagent 委派过程的 subsession 事件同理。

本仓库 UI 新增 **linkifier**（`chat_impl.ts` 气泡渲染与 `main.ts` subsession 渲染共用）：

- args 提取 `file_path`/`path` 字段（先 `JSON.parse`，失败降级正则——args 在 1200 字符处
  被截断，`controller.rs:58`）→ 渲染为可点击 chip：`📄 src/foo.rs:120`
- result 文本正则提取 `path:line[:col]`（grep/glob 输出格式）→ 同样渲染 chip
- 点击 → `host.openLocation({ path, startLine })` → 编辑器侧
  `openFile(path)`（`api/commands.ts:40`）→ `useEditorStore.openFileOrSwitch(file, line)`
  + `setTargetLine(line, col)`（`useEditorStore.ts:51,56`，跳行能力已存在）
- 无 host（CLI 浏览器模式）→ chip 降级为纯文本，点击复制路径。**linkifier 写在本仓库，
  两个环境同码**——subagent 看代码的过程在编辑器里全程可点跳。

### 5.3 图谱联动（latte-rs-graph）

- **chat → graph**：chip 右键菜单或小图标"在图谱中显示" → `host.revealInGraph({ path, symbol })`
  → 编辑器打开 GraphPanel，用 `graphCommands` 按 `file_path`/`qualified_name` 查节点
  （字段已具备：`graphTypes.ts` 的 `file_path/start_line/qualified_name`），写入
  `useGraphStore.selectedNodeId` + `highlightedNodeIds`（store 字段已存在）并相机居中。
- **graph → chat**：GraphPanel 节点右键新增"在 chat 中询问" → 经 `chatBridge` 单例
  （持有 iframe ref，`ChatAgentPanel` mount 时注册）调用
  `__LATTE_UI__.insertContext({ path, startLine, endLine, symbol })` →
  输入框插入 `参考 src/foo.rs:120-160 (bar) `。FileTree 文件节点同理。
- 反向场景：agent 修改了某文件后，编辑器侧可用 `highlightedNodeIds` 在图谱上闪动对应
  节点（复用 ToolResult linkifier 提取的路径，编辑器监听后自行联动，UI 无感）。

### 5.4 editor → chat：上下文注入

- CodeMirror 选区右键"询问 agent" → `insertContext({ path, startLine, endLine, quote })`
  → UI 输入框生成引用块（复用现有 quote 渲染与 `data-message-id` 跳转机制）。
- 会话按工作区隔离：`sessionKey` 由 host 提供（如 `latte:session:<workspaceId>`），
  替代现在的裸 localStorage 键，解决多工作区串会话（原 §8 风险项）。
- `workspaceRoot` 同时用于 UI 显示相对路径、host resolve 绝对路径。

### 5.5 路径与工作区一致性

前提：agent 的工作目录 = 编辑器活动工作区根。src-tauri spawn controller 时以 active
workspace 为 cwd（阶段一内嵌 server / 阶段二 ui_adapter 均如此）；切换工作区 →
`sessionKey` 变化 → 新 session。`.md` 等文档引用走 `openDoc`（DocViewer/DocPanel），
代码走 `openLocation`，由 host 按扩展名分派，UI 不区分。

---

## 6. 分阶段路线

| 阶段 | 内容 | 改动仓库 | 验证 |
|---|---|---|---|
| 0. 快速落地（可选，约 1 天） | **方案 A**：src-tauri 内嵌 axum server（ui.rs 路由抽成库，绑 127.0.0.1 随机端口）+ iframe 指向它 + skin.css。前端零改动 | 两边少量 | 面板能看到完整聊天 UI、深色皮肤生效 |
| 1. 传输抽象 | 新增 transport.ts + host.ts（契约类型），api.ts 重构为 HttpSseTransport，行为不变 | 本仓库 | 现有 Playwright e2e 全绿 |
| 2. IPC 终态 | src-tauri `ui_adapter.rs`（照 bridge-api.ts 映射表）+ TauriIpcTransport + dist 嵌入，撤掉内嵌 server | 编辑器为主 | 编辑器内聊天全流程 |
| 3a. 跳转联动 | linkifier + `openLocation`（§5.2） | 本仓库为主 | subagent 读代码气泡可点击跳行 |
| 3b. 图谱联动 | `revealInGraph` + 图谱/FileTree "在 chat 中询问"（§5.3） | 编辑器为主 | 双向定位可用 |
| 3c. 上下文与工作区 | 选区注入、sessionKey/workspaceRoot、openDoc（§5.4/§5.5） | 两边少量 | 选区引用进输入框、多工作区不串会话 |

阶段 0 可选：若想一步到位可直接从阶段 1 开始；阶段 0 的价值是立刻验证"嵌入 + 换肤"
链路，且内嵌 server 代码在阶段 2 之前都可复用。

---

## 7. 编辑器现有 chat 模块的处置

- `ChatAgentPanel`（iframe 宿主）替换 `ChatPanel.tsx` 成为侧栏主面板。
- 旧模式（hil / swarm / workflow 编辑器 / manager 决策按钮）是控制面功能，与新 UI 的
  controller 模式有重叠但更多。过渡期保留在 mode 切换 tab 里，逐步把仍有价值的能力
  （工作流编辑、决策按钮）以新 UI 功能的形式回移本仓库，最终下线旧模块。
- `src/chat/protocol.ts` 的渲染器无关思路仅作参考保留，不再发展。

---

## 8. 风险与注意点

- **stale .js shadow .ts**：ui 构建前先清理原地编译产物（§2）。
- **事件格式单点实现**：`chat_event_to_frontend_json` 必须上移到 latte-agent-core 共享，
  否则 axum/Tauri 两侧格式漂移会直接打破契约 C2。
- **args 截断**：ToolUse args 限 1200 字符（`controller.rs:58`），linkifier 必须容错解析；
  若日后需要精确跳转，可在 ChatEvent 增加结构化 `file_refs` 字段（JSON 层面后向兼容）。
- **iframe 细节**：右键菜单/overlay 在 iframe 内正常；剪贴板走 tauri clipboard 插件（已装）；
  面板缩放需同步 iframe 尺寸（宿主组件监听 chatWidth）。
- **双向桥时序**：`__LATTE_UI__` 在 UI mount 完成后才可用，编辑器侧 `chatBridge` 需排队
  缓冲未就绪时的调用（如图谱右键发生在面板未打开时，可先自动展开面板）。
- **Local Network Access 检查**：Chromium 142+ 拦截 `localhost→127.0.0.1` 跨域 iframe
  （`ERR_BLOCKED_BY_LOCAL_NETWORK_ACCESS_CHECKS`）。Linux Tauri 用 WebKitGTK 不受影响；
  **Windows（WebView2 = Chromium）阶段 0 可能中招**——若出现，给 webview 加
  `--disable-features=LocalNetworkAccessChecks`，或提前推进阶段二（无 HTTP server 即无此问题）。
  已有回归测试：`ui/__tests__/host-bridge.spec.ts`（跨域握手 + sessionKey + latte:call）。
- **旧模块的坑与本案无关**：markdown.ts UTF-16、ChatPanel 编译失败、未注册命令均属旧
  React chat，随 §7 逐步下线即可，不要去修。
