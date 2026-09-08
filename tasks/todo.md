# JSON 错误死循环拦截、Subsession 旁路修复与人工干预修剪功能报告

## 1. 架构目标
解决模型在生成工具参数调用时可能陷入的 JSON 语法错误死循环，避免垃圾 token 严重污染主会话上下文，并支持用户在线弹窗修正。

## 2. 方案 B：Subsession 旁路自动修复（零上下文污染）
1. **启发式规则先行**：在 `latte-agent-core/src/agent.rs` 中引入 `fast_heuristic_json_repair`：
   - 自动剥除 LLM 误输出的 markdown 代码块包裹（```json ... ```）；
   - 修复末尾遗留的逗号（trailing commas）；
   - 自动补全未闭合的括号与大括号。
2. **Subsession 极简子模型修复**：
   - 当启发式无法修复时，启动无历史上下文隔离的专用修复 prompt；
   - 仅传入当前工具名、报错信息与坏 JSON，要求仅输出修正后的合法 JSON；
   - 修复成功后直接用于工具执行，主消息历史不追加任何纠错垃圾轮次（0 污染）。
3. **死循环计数修复**：
   - 将 `MalformedArgs` 纳入 `permanent_streak` 连续失败计数；
   - 达到 `PERMANENT_NUDGE_AT`（5次）时主动注入硬提示要求停止盲目重试；
   - 达到 `PERMANENT_BREAK_AT`（8次）时触发熔断或人工介入。

## 3. 方案 A：弹窗人工介入 (HIL) 与上下文修剪 (Context Pruning)
1. **事件与等待通道**：
   - 扩展 `ChatEvent::ToolFixRequested`；
   - 携带 `tool_name`、`malformed_args`、`error_detail` 与 `choice_id`；
   - 注册到 `crate::choice` 机制中挂起等待前端用户在代码编辑器中修正。
2. **上下文修剪与回退**：
   - 用户提交修正参数后，调用 `crate::tool_fix::prune_failed_rounds`；
   - 将过去连续重试产生的大量错误消息历史彻底剪除；
   - 注入单次整洁的执行注解与正确结果，模型后续执行如同一次性成功。

## 4. 验证测试
- `test_clean_json_string_and_heuristics`: 验证启发式规则对代码块、尾随逗号、未闭合大括号的自动修复。
- `test_malformed_args_streak_breaks_out`: 验证连续坏 JSON 被正确纳入 streak 并安全熔断。
- `test_human_intervention_and_pruning`: 验证达到熔断阈值时触发人工介入弹窗，提交正确参数后成功裁剪前面失败轮次。
