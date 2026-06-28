# Config layout

> **What this directory is**: `config/` is a **reference / template**
> bundle for new projects. It is **not** read by `latte-agent` at
> runtime in the `latte-rs-agents` repo itself.
>
> At runtime, `latte-agent` reads from the **project root**:
>
> - `.latte/agents/` (per-role TOML files)
> - `.latte/models.toml` (model catalog)
> - `.latte/workflows/` (per-workflow TOML files)
> - `.latte/logs/` (chat session logs)
>
> - `agents/`, `models/`, `workflows/`, `prompts/`, `logs/` (same layout as project)
> - `logs/agents/` (per-agent session logs, separated by agent id)
>
> The actual config files at runtime live in `.latte/`. `config/`
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
XML-tagged regions inside prompts (`<role>`, `<rules>`,
`<tool_calldelegate>`) and tool names (`delegate`, `bash`, `read`,
`write`, `list`, `search`, `deepseek-v4-flash`, etc.) are parsed
verbatim by the model and the runner — do **not** translate them,
only the surrounding prose.

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
`latte-rs-model-router/models.toml` and the original `~/.latte/models.yaml`):

```yaml
models:
  - id: deepseek-v4-flash
    name: DeepSeek-v4-flash
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
id = "deepseek-v4-flash"
# ...
```
