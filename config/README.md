# Config layout

## 同步包（给其他机器）

`config/` 是**跨机器同步的权威源**。同步内容：

| 目录 | 内容 | 同步目标 |
| ---- | ---- | -------- |
| `config/workflows/` | 全部 workflow 定义（精致化中文版本，与 `.latte/workflows.d/` 内容一致） | 目标机 `.latte/workflows.d/` |
| `config/agents/` | 全部角色配置（含注释的权威版；`archive/` 是已归档的冗余角色） | 目标机 `.latte/agents.d/` |
| `config/prompts/` | 全部角色 prompt（精致化中文版本，与仓库根 `prompts/` 运行时副本一致） | 目标机 `prompts/` |

**编辑约定**：改 prompt/workflow/角色配置时先改 `config/` 下的权威版，
再复制到运行时副本（`prompts/`、`.latte/workflows.d/`、`.latte/agents.d/`），
与 `scripts/sync-config.sh` 的推送方向保持一致。

**不同步**：`config/models.toml` 与 `~/.latte/models.d/`——`api_key`
等机密留在各机器本地，模型编目由每台机器自行维护。`.latte/agents.d/`
里的 `model_chain`/`temperature` 也是**本机调优值**，不回灌 `config/`
（权威版固定 `deepseek-v4-flash` 链以保证可移植的确定性解析）。

同步脚本：`scripts/sync-config.sh [目标项目路径]`（推 config/agents +
prompts 到目标项目的 `.latte/`）。

## 运行时分层

> **What this directory is**: `config/` is a **reference / template**
> bundle for new projects. It is **not** read by `latte-agent` at
> runtime in the `latte-rs-agents` repo itself.
>
> At runtime, `latte-agent` reads from the **project root**:
>
> - `.latte/agents.d/` (per-role TOML files)
> - `.latte/models.d/` (model catalog)
> - `.latte/workflows.d/` (per-workflow TOML files)
> - `prompts/` (per-role prompt markdown)
> - `.latte/ui-sessions/` (chat session logs)
>
> The actual config files at runtime live in `.latte/` 与 `prompts/`。`config/`
> is here so contributors can copy it as a starting point for a new
> project, and so the binary's built-in defaults (compiled in via
> `prompts.rs::template_for`) match a checked-in example.

## Internationalisation

All role prompts (`prompts/*.md`) and workflows (`.latte/workflows/*.toml`)
in this repo are written in **Simplified Chinese**. They are the source
of truth that `latte-agent` compiles into the binary via
`include_str!`, and they are mirrored to `.latte/prompts.d/` and
`~/.latte/prompts.d/` for the runtime override chain.

If you want to ship your own role definitions or workflows in a different
language, the layering is:

1. **Compile-time defaults** (in the binary): `prompts/<id>.md` and
   `config/agents.toml` (role metadata) are embedded via `include_str!`
   in `latte-agent-core/src/prompts.rs::template_for`. Re-bake the
   binary to change.
2. **Project-level runtime override** (per project): `.latte/agents/`
   and `.latte/prompts/`. The first file matching `<role>.md` wins;
   fall-through order is the role's `prompt_file` (project) →
   `~/.latte/prompts/` (global) → `prompts::for_role` (compile-time).
3. **Global runtime override** (per user, per machine):
   `~/.latte/prompts/` and `~/.latte/agents/`. Same fall-through rules.
XML-tagged regions inside prompts (`<role>`, `<rules>`) are parsed
verbatim by the model and the runner — do **not** translate them,
only the surrounding prose. Prompts must NOT name agent tools: the
available tool set is defined solely by each role's `tools` list and
carried to the model via the request `tools` schema.

Both **directory** layouts are supported by
`AgentConfig::load` / `WorkflowRegistry::load` (single-file
`agents.toml` / `discussion.toml` are still loadable by
`load()` on the project layer, but the global layer uses
`agents/` and `workflows/` directories only).
## Global layer (~/.latte/)

All subdirectories under `~/.latte/` (or `$LATTE_HOME`) use the same
layout as the project layer. **The global layer is optional**; a missing
path is silently skipped so a project-only setup keeps working.

| Subdir | Loaded by | Per-file format |
| ------ | --------- | --------------- |
| `models.{yaml,toml,yml}` and `models/*.yaml\|*.toml` | `GlobalConfig::load_default` | router-style or project-style (see below) |
| `agents/` | `AgentConfig::load_with_global` | one `[roles.<id>]` per file |
| `workflows/` | `WorkflowRegistry::load_with_global` | one workflow per file |
| `prompts/` | `role.rs::resolve_global_prompt` | one `*.md` per role |
| `logs/` | `ChatLog` / `TraceSink` | chat session logs |
| `logs/agents/` | `ChatLog` / `TraceSink` | per-agent session logs |
`api_key`, `base_url`, and other `ModelDef` fields are resolved from
three sources in increasing priority. The first non-empty value wins;
later layers fill only the fields an earlier layer left unset (so
`${ENV_VAR}` placeholders in project config are also treated as unset).

| Priority | Source | Location |
| -------- | ------ | -------- |
| 3 (lowest) | Global | `~/.latte/models.yaml` / `.toml` / `.yml`, or every `*.yaml`/`*.toml` under `~/.latte/models.d/` (override with `$LATTE_HOME`) |
| 2 | Project | `--models-config` flag, default `config/models.toml` |
| 1 (highest) | CLI | `--api-key KEY` (optionally scoped via `--model ID`), or `--model-override ID.FIELD=VALUE` (repeatable) |

Examples:

```bash
# Fill the api_key of every model that has no key yet
latte-agent chat --role pm --tier budget --api-key "$DEEPSEEK_API_KEY"

# Target a single model
latte-agent chat --role pm --tier budget \
    --api-key "$DEEPSEEK_API_KEY" --model deepseek-chat

# Override base_url, name, max_tokens, etc. on a specific model
latte-agent chat --role pm --tier budget \
    --model-override deepseek-chat.base_url=http://localhost:11434 \
    --model-override deepseek-chat.api_key=ollama
```

Global config files accept both **router-style** (matches
`latte-rs-model-router/models.toml` and the original `~/.latte/models.yaml`).
注意 `ModelDef` 没有独立的 `id` 字段 —— `name` 就是 model id（tier 映射
与 `--model` 引用的键，也是 API 请求里的 `model` 字段），不要把它写成
展示名：

```yaml
models:
  - name: deepseek-v4-flash
    api: openai
    provider: deepseek
    base_url: https://api.deepseek.com
    api_key: sk-...
    context_window: 1000000
    max_tokens: 384000
    tier: budget
```

and **project-style** (matches `config/models.toml`'s nested form):

```toml
[models]
tiers = { budget = "deepseek-v4-flash" }

[[models.models]]
name = "deepseek-v4-flash"
# ...
```
