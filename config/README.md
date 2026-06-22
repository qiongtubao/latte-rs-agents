# Config layout

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
