# 根本分析：LLM 工具调用参数错误的问题本质

> 不是"输出太大要截断"，而是**LLM 调用了错误的参数**（search 没限制 paths 扫全项目），
> 应该在**参数层面预防 LLM 犯错**，而不是等大结果出来再截断。

## 1. 我们当前的问题链

### 1.1 search 工具的 schema 残缺

search 工具实际处理 5 个参数（`pattern`、`paths`、`i`、`skip`、`limit`），
但**schema 只声明了 `pattern` 一个属性**（`optional_required` 只传了 pattern）：

```rust
// tools/search.rs:434-441
optional_required(
    vec![("pattern", PropertyType::String, "regex 模式，必填")],  // ← 只有 pattern!
    &["pattern"],
)
```

**后果**：LLM 不知道 `paths` 是可传的、不知道可以限制搜索范围、不知道 `paths` 默认扫全项目。

### 1.2 schema 描述质量差

```rust
"按正则搜索文件内容"           // search 描述 — 没告诉 LLM "默认扫全项目，应限制 paths"
"文件路径，支持 :N-M 选择器"  // read path 描述 — 没告诉 LLM "别读大文件"
```

对比 oh-my-pi 的 describe 带明确的行为约束和默认值提示：
```
"file, directory, glob, internal URL; pass several as a semicolon-delimited list.
 Omitted -> searches the workspace root (\".\")"
```

### 1.3 LLM 参数错误的纠正反馈缺失

我们的 `DefaultRetryPolicy::loopback_to_model` 对 `MalformedArgs` 返回 `false`：
```rust
fn loopback_to_model(&self, kind: &ToolCallErrorKind) -> bool {
    matches!(kind,
        ToolCallErrorKind::HookAborted { .. }
            | ToolCallErrorKind::Execution { .. }
            | ToolCallErrorKind::Timeout  // ← MalformedArgs 不在其中！
    )
}
```
schema 校验失败 (`validate_input`) 产生的 `ToolError::validation` 被 `classify_tool_execution_error` 归为 `Execution`，所以会喂回模型。但 **LLM 调用参数本身合法（pattern 必填了）只是没传 paths**，schema 根本不会失败。

### 1.4 工具默认值不安全

```rust
let raw_paths = if input.get("paths").is_some() {
    parse_paths(...)
} else {
    vec![".".to_string()]  // ← 默认扫全项目
};
```
没有安全兜底限制（如 `max_depth`、自动排除 `.latte/`、或 `max_result_bytes`）。

## 2. oh-my-pi 如何预防 LLM 参数错误（4 层）

### 第 1 层：完整的工具 schema + 精确描述（让 LLM 第一次就传对）

每个工具的每个参数都完整声明在 schema 里，并带 `.describe()`：
```typescript
// tools/grep.ts:77-85
const searchSchema = type({
    pattern: type("string").describe("regex pattern"),
    "path?": type("string").describe(
        'file, directory, glob, or "<file>:<lines>" selector; '
        + 'pass several as semicolon-delimited list. '
        + '**Omitted -> searches the workspace root (".")**',  // ← 明确告诉 LLM 默认行为
    ),
    "skip?": type("number").describe("files to skip for pagination"),
});
```

**关键**：schema 描述了每个参数+默认值+副作用。LLM 知道传什么、不传的后果。

### 第 2 层：`strict: true` 强制 LLM 按 schema 输出

oh-my-pi 对所有工具启用 `strict: true`（OpenAI Structured Outputs）：
- provider 侧强制 LLM 输出必须符合 schema
- 任何额外的字段会被 provider 丢弃
- schema 不兼容时自动 fallback 到非 strict（fail-open，不崩）

我们的 `Tool.strict: Option<bool>` 已有此字段，但**search 没启用**（`build()` 后没调用 `.strict(true)`）。

### 第 3 层：执行前参数校验 + LLM 怪癖归一化（让 LLM 第二次传对）

oh-my-pi 在 `validateToolCall` 里做两层：

**A. LLM 怪癖主动修复（先修复再校验）**
```typescript
normalizeOptionalNullsForSchema(json, args)     // 可选字段的 null/"" → 删除
normalizeEnumStringWhitespace(json, args)        // enum 空格容错
normalizeIdentifierStringWhitespace(args)        // 路径尾随换行去除
normalizeStringEncodedArrayUnions(json, args)    // '["a","b"]' → ["a","b"]
normalizeSingleStringField(json, args)           // 单参工具放错 key
normalizeDoubleEncodedKeys(args)                 // 双重 JSON 编码的 key
healInbandArgSpill(args)                         // 标签泄漏修复
```

**B. 校验失败 → 错误喂回模型（让模型自纠正）**
```typescript
// validation.ts:1826-1832
export function validateToolCall(tools, toolCall) {
    // ...校验失败抛 ValidationError...
    // error 消息截断 256 字符/字段（防大 payload echo 回）
    // 然后被 agent loop 捕获，作为 tool_result 喂回模型
}
```

### 第 4 层：工具输出硬约束（兜底）

每层约束独立。即使 LLM 传错参数/结果过大，工具自身也保证：
- `DEFAULT_MAX_COLUMN=512`（每行）
- `MULTI_FILE_PER_FILE_MATCHES=20`（每文件）
- `INTERNAL_TOTAL_CAP=2000`（总行）
- `DEFAULT_MAX_BYTES=50KB`（总输出）
- `NATIVE_GREP_MAX_FILE_BYTES=4MB`（超大文件只扫前 4MB）

**不在结果里截断，而是从根源上限制 LLM 能获取的数据量。**

---

## 3. 可落地到我们工作流的修复

### P0：补全 search schema 声明 + 启用 strict（最优先）

```rust
// search.rs
Tool::builder("search", "按正则搜索文件内容。默认搜整个项目，建议用 paths 限制范围。",
    required(vec![
        ("pattern", PropertyType::String, "regex 模式"),
        ("paths", PropertyType::Array, "搜索路径，默认全项目"),
        ("i", PropertyType::Boolean, "大小写不敏感"),
        ("skip", PropertyType::Number, "分页跳过前 N 个文件"),
        ("limit", PropertyType::Number, "每文件最多匹配行数"),
    ]),
    ...
)
.strict(true)  // 启用 OpenAI Structured Outputs
.build()
```

**同时** `paths` 的默认值改为 `"."` 但 schema 暴露 `paths` 参数，LLM 知道可以传。

### P1：提高工具 description 质量

把描述改得像 oh-my-pi 一样精确、有默认值提示：
```rust
// read 工具
"读取文件内容。支持 path:N-M 行范围。大文件建议用 :start-end 分段读。默认 maxSize=10MB。"
```

### P2：执行前参数校验 + 错误反馈闭环

在 `tool_manager.rs:execute()` 中，`validate_input` 失败后的 `ToolError::validation` 会被 `classify_tool_execution_error` 归为 `Execution`，**已经会 loopback 到模型**。关键缺口是：
1. 很多工具没把完整参数列在 schema 里 → 校验形同虚设
2. 没有 LLM 怪癖归一化（`normalizeStringEncodedArrayUnions` 等）
3. 错误消息太长（没有 256 字段截断）

### P3：search 工具输出安全兜底

在工具 handler 内（而非上层）：
```rust
const MAX_TOTAL_BYTES: usize = 200_000;  // 单次 search 返回上限
// 在组装 matches 时累积字节，超限截断 + truncated=true
```

### P4：可选 — validateToolCall 式的 LLM 怪癖归一化

在 `agent.rs:1560`（`Message::tool_result` 前）或 `tool_manager.rs` 的 `execute` 中，对 LLM 参数做归一化再传 handler：

```rust
// 伪代码 — 对已知的 LLM 怪癖自动修复
let input = normalize_llm_args(input, &tool);
// - 可选字段 null/"" → 删除
// - 路径尾随空格/换行 → trim
// - JSON 字符串表示的数组 → 解析为真数组
// - 双重 JSON 编码的 key → 解一层
```

---

## 4. 本次诊断更新（tasks/lessons.md）

1. **工具 schema 不完整 → LLM 不知道可选参数 → 用不安全默认值 → 撑爆 context**
   - 每个工具的每个参数都应该声明在 schema 里，即使它是可选的
   - description 要写清楚默认值和副作用（"不传则扫全项目"）
2. **启用 `strict: true`** — 让 provider 也约束 LLM 输出格式
3. **schema 校验失败要 loopback 到模型** — 让我们自己知道校验失败了（目前 `MalformedArgs` 不喂回，但 schema 校验失败归 `Execution` 会喂回，这个差异需要确认是不是设计意图）
4. **工具输出应有硬上限**（行截断、总字节截断）— 不信任 LLM 的参数选择，兜底