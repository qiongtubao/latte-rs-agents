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
> …and from the **user home** (`$LATTE_HOME` or `~/.latte/`):
>
> - `agents.d/`, `models.d/`, `workflows.d/`, `prompts.d/`, `logs/`
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
2. **Project-level runtime override** (per project): `.latte/prompts.d/`
   and `.latte/agents.d/`. The first file matching `<role>.md` wins;
   fall-through order is the role's `prompt_file` (project) →
   `~/.latte/prompts.d/` (global) → `prompts::for_role` (compile-time).
3. **Global runtime override** (per user, per machine):
   `~/.latte/prompts.d/` and `~/.latte/agents.d/`. Same fall-through rules.

XML-tagged regions inside prompts (`<role>`, `<rules>`,
`<tool_calldelegate>`) and tool names (`delegate`, `bash`, `read`,
`write`, `list`, `search`, `deepseek-v4-flash`, etc.) are parsed
verbatim by the model and the runner — do **not** translate them,
only the surrounding prose.

Both **single-file** and **directory** layouts are supported by
`AgentConfig::load` / `WorkflowRegistry::load`.

## Directory layout (preferred)

```
config/
├── agents.toml            # optional top-level defaults
├── agents/                # one file per role
│   ├── pm.toml
│   ├── architect.toml
│   └── ...
├── workflows/             # one file per workflow
│   ├── code_review.toml
│   ├── bug_triage.toml
│   └── ...
└── models.toml            # model catalog (single file)
```

Each `agents/<role>.toml` declares one `[roles.<id>]` section.
Each `workflows/<name>.toml` declares a single `DiscussionWorkflow`
(the `name` field is the canonical id).

## Legacy single-file layout (still supported)

```
config/
├── agents.toml         # [models] + [roles.*]
├── models.toml
└── discussion.toml     # [default_workflow] + [workflows.*]
```

## Global layer (~/.latte/)

Three subdirectories under `~/.latte/` (or `$LATTE_HOME`) are read by
the CLI when present. **The global layer is optional**; a missing path
is silently skipped so a project-only setup keeps working.

| Subdir | Loaded by | Per-file format |
| ------ | --------- | --------------- |
| `models.{yaml,toml,yml}` and `models.d/*.yaml\|*.toml` | `GlobalConfig::load_default` | router-style or project-style (see below) |
| `agents.d/` and `agents.toml` | `AgentConfig::load_with_global` | one `[roles.<id>]` per file, or single combined file |
| `workflows.d/` and `discussion.toml` | `WorkflowRegistry::load_with_global` | one workflow per file, or `[workflows.*]` section |

**Merge semantics** for the agents/workflows layer:

| Priority | Source | Location |
| -------- | ------ | -------- |
| 3 (lowest) | Global | `~/.latte/models.yaml` / `.toml` / `.yml`, or every `*.yaml`/`*.toml` under `~/.latte/models.d/` (override with `$LATTE_HOME`) |
| 2 | Project | `--models-config` flag, default `.latte/models.toml` |
  declare.
- Missing project path → fall through to global layer.
- Missing global path → fall through to project layer.

Model merging has different semantics (field-filling, see below).

Point CLI flags at either the file path or the directory path; the
loader detects via `std::fs::metadata`.

## Three-layer model config resolution

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
