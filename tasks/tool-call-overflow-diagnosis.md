# 诊断报告：LLM 工具调用上下文撑爆问题

> **状态：大部分已闭环（2026-08-08 核对）**。参数层预防已落地于
> latte-rs-agent-tools `f2ff70c`（schema 补全 + describe 警告 + per-file limit）。
> 残留风险：search 默认 `hidden=true` 仍会遍历 `.latte/`（schema 描述已警告
> LLM 用 `paths` 限制范围）；无总输出字节上限。如需彻底收口，在工具仓库做
> 「默认排除 `.latte/`」+「总字节上限」两个增强。

## 1. 根因诊断

### 现场还原

Manager session (770MB) 最后 4 次 `search` 工具返回 **395MB**，撑爆 LLM 上下文：

| search 参数 | 返回大小 | .latte/jsonl 命中 | 说明 |
|---|---|---|---|
| `RDBCHANNELSYNC` | 4.3 MB | 3.1 MB (72%) | 40 条 jsonl 匹配，单行 16K+ |
| `WAIT_RDB_CHANNEL` | 15.5 MB | 9.6 MB (62%) | 42 条 jsonl 匹配 |
| `CLIENT_REPL_RDB_CHANNEL` | 66.4 MB | 36.9 MB (56%) | 46 条 jsonl 匹配 |
| `SEND_BULK_AND_STREAM` | **309 MB** | **162 MB (52%)** | 35 条 jsonl 匹配 — **自引用炸弹：把自己当前 session 的 jsonl 搜出来** |

### 根因链

```
search 工具默认 paths=["."] → walk_all_files 扫全项目目录
  → include_hidden=true 导致遍历 .latte/（运行时目录）
    → .latte/ui-sessions/*.jsonl 被当作源码搜索
      → 命中后每行是完整 JSONL 行（16K+ 字符，无行长度上限）
        → search 将整行 content 原样返回（无总输出字节上限）
          → controller 将 result_str 直接入 context（无二次截断）
            → 4 轮累积 395MB → context 爆掉
```

### 关键代码点

| 文件 | 行 | 问题 |
|---|---|---|
| `latte-rs-agent-tools/src/tools/search.rs:289-345` | 扫描阶段 | `walk_all_files(root, **true**, use_gitignore)` — include_hidden=true 但 `.latte/` 不在 gitignore 排除列表 |
| `search.rs:319` | 读取内容 | `std::fs::read_to_string(f)` 全量读文件，单行无长度上限 |
| `search.rs:330-335` | 返回匹配 | 每 match 的 content 是整行 JSON（可 16K+），没有 `DEFAULT_MAX_COLUMN` 截断 |
| `latte-rs-agent-tools/src/tools/find.rs:225-228` | 隐藏目录过滤 | 只有 `!include_hidden` 时才跳过 `.latte/` 等隐藏目录 |
| `latte-agent-core/src/agent.rs:1560` | context 入 | `Message::tool_result(r.id, result_str)` 无二次截断 |
| `latte-rs-agent-tools/src/tools/file.rs:129-131` | read 工具 | `fs::read` + `String::from_utf8_lossy` 无大小限制 |
| `latte-rs-agent-tools/src/tools/shell.rs:224-225` | bash 工具 | `child.wait_with_output()` 无大小限制 |

---

## 2. oh-my-pi 的三层防御机制

oh-my-pi 在 LLM 与工具之间设了三道防线，一一对应我们的问题：

### 第 1 层：工具内输出约束（治本）

| 机制 | oh-my-pi 实现 | 对应我们的代码 |
|---|---|---|
| **每行长度截断** | `DEFAULT_MAX_COLUMN = 512` — grep 匹配行截断到 512 字符 | 无 — search 返回整行 |
| **每文件匹配上限** | `MULTI_FILE_PER_FILE_MATCHES = 20`, `SINGLE_FILE_MATCHES = 200` | 已有 `MAX_PER_FILE_LIMIT = 500` 但过大 |
| **总结果上限** | `INTERNAL_TOTAL_CAP = 2000` — 原生 grep 最多返回 2000 行 | 无 |
| **输出字节上限** | `DEFAULT_MAX_BYTES = 50KB` — 所有工具输出经 `OutputSink` 截断 | 无 — search 直接返回全量 |
| **文件大小上限** | `NATIVE_GREP_MAX_FILE_BYTES = 4MB` — 超大文件只扫前 4MB | 无 — 读全文件 |
| **超限标记** | `truncated: true` 标记，通知 LLM 结果不完整 | 已有 `truncated` 标记但无用（没真截断） |

**关键代码**：`packages/coding-agent/src/tools/grep.ts:92-113` (常量定义), `packages/coding-agent/src/session/streaming-output.ts` (OutputSink 50KB 硬顶)

### 第 2 层：context 修剪（兜底）

| 机制 | oh-my-pi 实现 | 对应我们 |
|---|---|---|
| **按年龄剪枝** | `pruneToolOutputs()` — 最近的 `protectTokens: 40_000` tokens 保留，超限的旧结果替换为 `[Output truncated - N tokens]` 占位符 | `prune_to_budget()` 按 importance + keep_last 丢整条消息，不细分工具结果 |
| **同文件 supersede** | `readToolSupersedeKey` — 对同文件的多次 read，旧结果自动标记 `[Superseded by a newer read of this file]` | 无 |
| **无信息结果** | `USELESS_NOTICE` — 空匹配 search 等结果自动标记 `[Uneventful result elided]` | 无 |
| **缓存守卫** | `cacheWarmSuffixTokens` — 已发往 provider 的缓存前缀不触发 prune（避免 cacheWrite 浪费） | 无 |
| **summary 截断** | `truncateToolResultForSummary(text, 2000)` — 分支总结时工具结果截 2000 字符 | 无 |

**关键代码**：`packages/agent/src/compaction/pruning.ts` (pruneToolOutputs), `packages/agent/src/compaction/utils.ts` (truncateToolResultForSummary)

### 第 3 层：模型截断保护（防坑）

| 机制 | oh-my-pi 实现 | 对应我们 |
|---|---|---|
| **length skip** | `stop_reason: length` 时工具参数不完整 → **不执行**，返回 `"Tool call was not executed because the assistant hit its output token limit... split into several smaller tool calls"` | 无 — 当前直接执行截断参数 |
| **参数错误 256 截断** | 工具参数验证错误按字段截 256 字符，避免大 payload echo 回 | 无 |
| **write 大文件分块** | length skip 的 message 指导模型把大 write 拆成多个小 write | 无 |

**关键代码**：`packages/agent/src/agent-loop.ts:1087-1090` (length skip), `packages/agent/src/agent-loop.ts:2320-2321` (message)

---

## 3. 推荐落地方案（优先级排序）

### P0 — 紧急（工具层，治本）

在 `latte-rs-agent-tools/src/tools/search.rs` 中增加：

```rust
// 新增常量
const MAX_CONTENT_CHARS: usize = 1024;    // 单行匹配内容截断
const MAX_TOTAL_BYTES: usize = 200_000;   // 单次 search 返回总字节上限 (200KB)
const EXCLUDED_DIRS: &[&str] = &[
    ".latte", ".git", "target", "node_modules",
    ".claude", ".omc", ".venv", ".mypy_cache",
];
```

改动：
1. `walk_all_files` 增加默认排除目录参数（不依赖 gitignore，因为 `.latte/` 可能部分 git-ignored 部分 committed）
2. `grep_file` 中 `content` 截断到 `MAX_CONTENT_CHARS` + `[...truncated]` 标记
3. 输出组装时检查 `total_bytes`，超限后停止追加并设 `truncated: true`
4. 同步给 `latte-rs-agent-tools/src/tools/file.rs`（read 工具）、`shell.rs`（bash 工具）

### P1 — 重要（context 层，兜底）

在 `latte-agent-core/src/agent.rs` 的 tool_result 入 context 处（约 L1560、L1795）增加：

```rust
// 截断逻辑
const MAX_TOOL_RESULT_CHARS: usize = 200_000;
fn truncate_tool_result(text: &str) -> String {
    if text.len() <= MAX_TOOL_RESULT_CHARS {
        text.to_string()
    } else {
        format!("{}...\n[Output truncated - {} bytes]",
            &text[..MAX_TOOL_RESULT_CHARS],
            text.len() - MAX_TOOL_RESULT_CHARS,
        )
    }
}
```

### P2 — 可选（模型保护）

在 `latte-agent-core/src/agent.rs` 中 `model_call` 返回值处，当 `finish_reason == "length"` 且包含 tool_calls 时，**跳过执行**并返回合成结果（"参数被截断，请拆分后再试"）。

---

## 4. 改动量估计

| 改动 | 文件数 | 行数 | 风险 |
|---|---|---|---|
| search 输出约束 + 目录排除 | 2 (search.rs, find.rs) | ~50 新增 + ~10 改 | **低** — 加常量和 if 守卫，单测覆盖 |
| read/shell 加输出上限 | 2 (file.rs, shell.rs) | ~10 改 + ~10 新增 | **低** — 与 search 同理 |
| context 层截断 | 1 (agent.rs) | ~20 新增 | **低** — 纯函数 + 1 处调用 |
| length 截断保护 | 1 (agent.rs) | ~30 新增 | **中** — 需理解 tool_calls 生命周期 |
| 单测 | 3-5 | ~100 | — |

**总计：~200 行有效代码，~100 行单测。** 三层可独立部署，互不阻塞。

---

## 5. 本次会话经验

更新到 `tasks/lessons.md`：
- **search 工具默认 paths=["."] 时需排除运行时目录**（`.latte/`、`.git/`、`target/`、`node_modules/`）
- **任何工具返回都应设置输出大小上限**（search 行截断、总字节截断），不信任模型或用户输入的搜索范围
- **tool_result 入 context 前应二次截断**（兜底，不依赖单个工具的正确性）
- **`.latte/ui-sessions/*.jsonl` 不应当被 search 索引** — 历史会话日志不应成为搜索内容