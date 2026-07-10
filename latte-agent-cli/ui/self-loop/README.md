# latte-agent self-loop runner

`runner.ts` 是 AI 自调试闭环的执行体。它接收一个 task（"修这个 UI 问题"），自主：

1. 用 `chrome --headless` 打开前端 → 截图 + 收 console。
2. 读最近的 `~/.latte/traces/*.jsonl` 看后端到底跑了啥。
3. 调用 `latte-agent chat`（HTTP API 形式 — 实际是通过 main latte-agent 二进制），
   让一个 LLM agent 拿到截图 + console + trace，决定怎么改。
4. 把 agent 给的 unified diff apply 到 `ui/src/`。
5. 重跑 `vite build` / `tsc --noEmit` / 重启 headless chrome 再截一次。
6. 重复直到 max_iterations 或 AI 报 done。
7. 每步 emit 一行 `SelfLoopEvent` JSON 到 stdout，让 Rust 后端 SSE 流给前端。

## 设计原则

- **不联网查文档**：所有上下文都在本机（截图 + console + trace + 当前 src/）。
- **不暴露 prompt 给用户**：agent 用 `latte-agent chat` 的现有 manager 角色，
  它已经会读图（多模态），已经会调 delegate / patch 工具。
- **每步都有 verifier**：每次迭代后必须跑 `tsc --noEmit` + headless 截图 +
  `pnpm test`，三者都过才算 iter 通过；否则回滚 + 让 agent 重来。
- **kill switch**：`Ctrl+C` 或后端 `/api/self-loop/stop` 中断 SIGTERM。
  当前 runner 在收到 SIGTERM 时直接退出，不清理（worktree 里脏文件可手动
  删）。

## 启动

```bash
# 直接跑（一般由 latte-agent ui 调用）
tsx runner.ts --task-json '{"task":"...", "max_iterations":5, "ui_base_url":"http://localhost:4567"}'

# 或手动调
pnpm self-loop -- --task-json '{"task":"make chat input autoresize", "max_iterations":5}'
```
