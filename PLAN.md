# latte-rs-agents: Multi-Role Agent Discussion System — Implementation Plan

> References: `../latte-rs-model-router` (AiClient), `../latte-rs-agent-tools` (ToolManager),  
> `../../github/gsd-core/` (architecture patterns)

---

## 1. Architecture Overview

```
┌──────────────────────────────────────────────────────────────┐
│                      Discussion Orchestrator                  │
│  round_controller ──▶ turn(N rounds) ──▶ consensus/vote      │
└──────────┬──────────┬──────────┬──────────┬──────────────────┘
           │          │          │          │
     ┌─────▼────┐┌───▼────┐┌───▼────┐┌───▼────┐
     │ PM Agent ││Dev Agt ││QA Agent││Arch Agt│  ...
     │(sonnet)  ││(haiku) ││(haiku) ││(opus)  │
     └─────┬────┘└───┬────┘└───┬────┘└───┬────┘
           │         │         │         │
     ┌─────▼─────────▼─────────▼─────────▼─────────────────────┐
     │                    Agent Runtime                         │
     │  role_prompt ──▶ AiClient ──▶ ToolManager ──▶ response  │
     └──────────────────────────────────────────────────────────┘
     ┌──────────────────────────────────────────────────────────┐
     │                 Configuration Layer                       │
     │  agents.toml (roles) + models.toml (catalog) + tiers     │
     └──────────────────────────────────────────────────────────┘
```

## 2. Crate Structure

```
latte-rs-agents/
├── Cargo.toml                  # workspace root
├── latte-agent-core/           # core agent types + runtime
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── agent.rs            # Agent struct, AgentRunner
│       ├── role.rs             # Role, RoleTemplate, built-in roles
│       ├── config.rs           # AgentConfig, load/save TOML
│       ├── model_resolver.rs   # Tier → concrete Model resolution
│       ├── context.rs          # ConversationContext, history mgmt
│       └── error.rs            # AgentError
├── latte-agent-orchestrator/   # multi-agent discussion
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── orchestrator.rs     # DiscussionOrchestrator, turn loop
│       ├── round.rs            # Round, Turn, TurnOrder
│       ├── consensus.rs        # Vote, consensus algorithms
│       ├── workflow.rs         # DiscussionWorkflow (discuss→plan→exec→verify)
│       └── error.rs            # OrchestratorError
├── latte-agent-cli/            # CLI / demo binary
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs
│       └── commands/
│           ├── mod.rs
│           ├── discuss.rs      # `latte-agent discuss --topic "..." --roles pm,dev,qa`
│           ├── config.rs       # `latte-agent config show|add-role|set-model`
│           └── list.rs         # `latte-agent list roles|models|workflows`
├── config/                     # shipped config templates
│   ├── agents.toml             # default role definitions
│   ├── models.toml             # model catalog (compatible with model-router)
│   └── discussion.toml         # default discussion workflow
└── prompts/                    # role system prompt templates
    ├── pm.md                   # Product Manager
    ├── programmer.md           # Software Engineer
    ├── tester.md               # QA / Test Engineer
    ├── architect.md            # System Architect
    ├── devops.md               # DevOps / SRE
    ├── security.md             # Security Auditor
    ├── reviewer.md             # Code Reviewer
    ├── designer.md             # UI/UX Designer
    ├── tech_writer.md          # Technical Writer
    └── manager.md              # Engineering Manager
```

## 3. Core Types

### 3.1 Agent (`latte-agent-core/src/agent.rs`)

```rust
/// A configured agent ready to participate in discussions.
pub struct Agent {
    /// Unique name in the discussion (e.g. "pm", "dev_lead", "qa").
    pub name: String,
    /// Role definition (system prompt, personality).
    pub role: Role,
    /// Resolved AI model client.
    pub client: AiClient,
    /// Tools this agent can use.
    pub tool_manager: Option<Arc<dyn ToolManager>>,
    /// Per-agent generation params (temperature, top_p, etc.).
    pub params: GenerateParams,
}

/// Turn-based runner: send messages → receive response with tool calls.
pub struct AgentRunner {
    agent: Agent,
    context: ConversationContext,
    max_tool_rounds: usize,  // max tool-call → result loops per turn
    retry_budget: usize,     // tier-escalation retries
}

impl AgentRunner {
    /// Process one turn: send current context, handle tool calls, return final text.
    pub async fn run_turn(&mut self, new_messages: Vec<Message>) -> Result<String>;
}
```

### 3.2 Role (`latte-agent-core/src/role.rs`)

```rust
/// A role template — loaded from TOML or code.
pub struct Role {
    /// Role identifier: "pm", "programmer", "tester", ...
    pub id: String,
    /// Display name.
    pub name: String,
    /// Category for grouping (planning, execution, verification, discussion).
    pub category: RoleCategory,
    /// System prompt template (supports `{{variable}}` substitution).
    pub system_prompt: String,
    /// Default model tier for this role (can be overridden per-instance).
    pub default_model_tier: ModelTier,
    /// Default generation params (temperature, etc.) for this role.
    pub default_params: GenerateParams,
    /// Tools this role typically uses (tool names).
    pub allowed_tools: Vec<String>,
    /// Icon/emoji for display.
    pub icon: String,
}

pub enum RoleCategory {
    Planning,       // PM, Architect, Manager
    Execution,      // Programmer, DevOps
    Verification,   // Tester, Reviewer, Security
    Discussion,     // All-purpose
}
```

### 3.3 Model Resolution (`latte-agent-core/src/model_resolver.rs`)

Mirrors gsd-core's three-layer model assignment:

```rust
/// Three-tier model assignment per agent (mirrors gsd-core quality/balanced/budget).
pub enum ModelTier {
    /// Best available model (cf. opus).
    Premium,
    /// Best-balanced model (cf. sonnet).
    Standard,
    /// Fast/cheap model (cf. haiku).
    Budget,
}

/// Maps agent role + tier → concrete Model.
pub struct ModelResolver {
    catalog: ModelCatalog,
    runtime_defaults: HashMap<String, HashMap<ModelTier, Model>>,
}

impl ModelResolver {
    /// Resolve a concrete model for (role, tier, provider_override).
    pub fn resolve(
        &self,
        role: &str,
        tier: ModelTier,
        provider: Option<&str>,
    ) -> Result<Model>;

    /// Dynamic escalation: budget → standard → premium on retry.
    pub fn escalate(&self, role: &str, current: ModelTier) -> Option<ModelTier>;
}
```

### 3.4 Config (`latte-agent-core/src/config.rs`)

```rust
/// Top-level agent config (serialized as TOML).
pub struct AgentConfig {
    pub models: ModelCatalog,
    pub roles: Vec<RoleConfig>,
    pub tiers: Option<TierConfig>,
}

/// Per-role configuration in TOML.
pub struct RoleConfig {
    pub id: String,
    pub name: String,
    pub category: String,
    pub model_tier: String,         // "premium" | "standard" | "budget"
    pub prompt_file: Option<String>, // path to .md system prompt
    pub temperature: Option<f64>,
    pub tools: Vec<String>,
    pub icon: String,
}

/// Model catalog from TOML (compatible with latte-rs-model-router format).
pub struct ModelCatalog {
    pub models: Vec<ModelDef>,
    /// Per-role tier overrides: roles.<role>.tier = "premium"
    pub role_tiers: Option<HashMap<String, HashMap<String, String>>>,
}

pub struct ModelDef {
    pub id: String,
    pub name: String,
    pub api: String,            // "openai" | "anthropic"
    pub provider: String,
    pub base_url: String,
    pub api_key: String,
    pub context_window: u32,
    pub max_tokens: u32,
    pub supports_thinking: bool,
    pub tier: Option<String>,   // which tier this model belongs to
}
```

## 4. Orchestrator Design

### 4.1 Discussion Orchestrator (`latte-agent-orchestrator/src/orchestrator.rs`)

```rust
pub struct DiscussionOrchestrator {
    agents: HashMap<String, AgentRunner>,
    workflow: DiscussionWorkflow,
    shared_context: ConversationContext,
    history: Vec<TurnRecord>,
}

pub struct DiscussionConfig {
    /// Max discussion rounds (0 = single round, N = N rounds of everyone).
    pub max_rounds: usize,
    /// Turn order: sequential, round-robin, free-form.
    pub turn_order: TurnOrder,
    /// Consensus method at end of discussion.
    pub consensus: ConsensusMethod,
    /// Moderator agent (optional, manages discussion flow).
    pub moderator: Option<String>,
    /// Summarize context when it exceeds this token budget.
    pub context_token_budget: usize,
}

pub enum TurnOrder {
    /// Fixed sequence: [pm, dev, qa] repeats each round.
    Fixed(Vec<String>),
    /// Round-robin with optional skip for abstainers.
    RoundRobin,
    /// Moderator decides who speaks next.
    Moderated,
    /// Agents speak when they have something to say (concurrent).
    FreeForm,
}

pub enum ConsensusMethod {
    /// Simple majority vote.
    MajorityVote,
    /// Each role has weight; weighted vote.
    WeightedVote(HashMap<String, f64>),
    /// Discussion runs until consensus or max rounds.
    DeliberateUntilConsensus { min_agreement: f64 },
    /// No vote — moderator decides after hearing all.
    ModeratorDecides,
    /// Just record all opinions, no consensus needed.
    NoConsensus,
}
```

### 4.2 Workflow (`latte-agent-orchestrator/src/workflow.rs`)

Mirrors gsd-core's 5-step loop (discuss → plan → execute → verify → ship):

```rust
pub struct DiscussionWorkflow {
    pub name: String,
    pub steps: Vec<WorkflowStep>,
}

pub struct WorkflowStep {
    pub id: String,                    // "requirements", "design", "review"
    pub description: String,
    pub speaker_sequence: Vec<String>, // which agents speak in this step
    pub prompt_template: String,       // template with {{variables}}
    pub hooks: Vec<StepHook>,          // pre/post hooks
    pub output_key: Option<String>,    // key to store output in shared context
}

pub enum StepHook {
    /// Inject content before step starts (mirrors gsd-core "contribution").
    Contribution { source: String, template: String },
    /// Blocking validation gate (mirrors gsd-core "gate").
    Gate { condition: String, message: String },
}
```

### 4.3 Round Flow

```
┌──────────────────────────────────────────────────────┐
│ RoundController::run(topic, config)                   │
│                                                       │
│  for round in 0..max_rounds:                          │
│    for step in workflow.steps:                        │
│      ┌─ step hooks (pre) ─┐                          │
│      │  contributions     │                           │
│      │  gates             │                           │
│      └────────────────────┘                           │
│      for speaker in step.speaker_sequence:            │
│        agent = agents[speaker]                        │
│        prompt = build_prompt(                         │
│          role.system_prompt,                          │
│          shared_context,                              │
│          step.prompt_template,                        │
│          previous_turns                               │
│        )                                              │
│        response = agent.run_turn(prompt).await        │
│        record TurnRecord { speaker, response }        │
│        update shared_context                          │
│      ┌─ step hooks (post) ─┐                         │
│      │  validation          │                          │
│      └──────────────────────┘                         │
│    if consensus_achieved():                           │
│      break                                            │
│                                                       │
│  return DiscussionResult { turns, consensus, summary }│
└──────────────────────────────────────────────────────┘
```

## 5. Role Prompt Templates

### 5.1 Template Format (e.g., `prompts/pm.md`)

Uses handlebars-style `{{variable}}` substitution:

```markdown
<role>
You are a **Product Manager** in a software development team discussion.

Your responsibilities:
- Define user stories with clear acceptance criteria
- Prioritize features based on user value and business impact
- Identify scope boundaries and MVP slices
- Ask clarifying questions when requirements are ambiguous
- Push back on over-engineering — focus on outcomes, not solutions

Your personality: pragmatic, user-focused, decisive. You prefer short, 
actionable answers. You call out scope creep. You think in terms of 
"how will this help the user?"

<context>
Topic: {{topic}}
Project: {{project_name}}
{{#if constraints}}Constraints: {{constraints}}{{/if}}
</context>

<rules>
- Speak in first person as the PM
- Keep responses under 300 words unless explicitly asked for detail
- When disagreeing with others, explain why from a user/business perspective
- If you don't know something about the domain, say so
</rules>
```

### 5.2 Built-in Roles (10 roles)

| Role | Category | Default Tier | Tools | Description |
|------|----------|-------------|-------|-------------|
| `pm` | Planning | Standard | read, search | Product Manager — user stories, prioritization, scope |
| `architect` | Planning | Premium | read, search | System Architect — design, tradeoffs, scalability |
| `programmer` | Execution | Budget | read, write, bash, search | Software Engineer — implementation, code, debugging |
| `tester` | Verification | Budget | read, bash, search | QA Engineer — test cases, edge cases, regression |
| `reviewer` | Verification | Standard | read, search | Code Reviewer — code quality, patterns, security |
| `devops` | Execution | Budget | read, bash, write | DevOps/SRE — CI/CD, infra, deployment, monitoring |
| `security` | Verification | Standard | read, search | Security Auditor — vulnerabilities, threat modeling |
| `designer` | Planning | Standard | read | UI/UX Designer — user experience, accessibility |
| `tech_writer` | Execution | Budget | read, write | Technical Writer — docs, changelogs, API docs |
| `manager` | Planning | Premium | read | Engineering Manager — team velocity, risk, deadlines |

## 6. Configuration Format

### 6.1 `config/agents.toml` — Role Definitions

```toml
# Default role definitions. Users can add custom roles.

[roles.pm]
id = "pm"
name = "Product Manager"
category = "planning"
model_tier = "standard"
prompt_file = "prompts/pm.md"
temperature = 0.7
tools = ["read", "search"]
icon = "📋"

[roles.architect]
id = "architect"
name = "System Architect"
category = "planning"
model_tier = "premium"
prompt_file = "prompts/architect.md"
temperature = 0.5
tools = ["read", "search"]
icon = "🏗️"

[roles.programmer]
id = "programmer"
name = "Software Engineer"
category = "execution"
model_tier = "budget"
prompt_file = "prompts/programmer.md"
temperature = 0.3
tools = ["read", "write", "bash", "search"]
icon = "💻"

# ... tester, reviewer, devops, security, designer, tech_writer, manager ...

# Custom role example:
[roles.data_scientist]
id = "data_scientist"
name = "Data Scientist"
category = "execution"
model_tier = "standard"
prompt_file = "prompts/data_scientist.md"
temperature = 0.5
tools = ["read", "bash", "search"]
icon = "📊"
```

### 6.2 `config/models.toml` — Model Catalog

Compatible with `latte-rs-model-router/models.toml` format, extended with tier assignment:

```toml
# Tier-to-model mapping for model resolution.
[tiers]
premium = "claude-opus-4-20250514"
standard = "claude-sonnet-4-20250514"
budget = "deepseek-chat"

# Per-role tier overrides (optional).
[role_tiers.pm]
premium = "claude-opus-4-20250514"
standard = "claude-sonnet-4-20250514"
budget = "deepseek-chat"

[role_tiers.programmer]
premium = "claude-sonnet-4-20250514"
standard = "deepseek-chat"
budget = "qwen2.5-coder-32b-instruct"

[[models]]
id = "claude-opus-4-20250514"
name = "Claude Opus 4"
api = "anthropic"
provider = "anthropic"
base_url = "https://api.anthropic.com"
api_key = "${ANTHROPIC_API_KEY}"
context_window = 200000
max_tokens = 8192
supports_thinking = true

[[models]]
id = "claude-sonnet-4-20250514"
name = "Claude Sonnet 4"
api = "anthropic"
provider = "anthropic"
base_url = "https://api.anthropic.com"
api_key = "${ANTHROPIC_API_KEY}"
context_window = 200000
max_tokens = 8192
supports_thinking = true

[[models]]
id = "deepseek-chat"
name = "DeepSeek Chat V3"
api = "openai"
provider = "deepseek"
base_url = "https://api.deepseek.com"
api_key = "${DEEPSEEK_API_KEY}"
context_window = 65536
max_tokens = 8192
supports_thinking = false

[[models]]
id = "qwen2.5-coder-32b-instruct"
name = "Qwen 2.5 Coder 32B"
api = "openai"
provider = "ollama"
base_url = "http://localhost:11434"
api_key = "ollama"
context_window = 32768
max_tokens = 4096
supports_thinking = false
```

### 6.3 `config/discussion.toml` — Workflow Definitions

```toml
[default_workflow]
name = "standard-software-discussion"
max_rounds = 3
context_token_budget = 32000
turn_order = "fixed"
consensus = "majority_vote"

# Default speaker sequence.
speakers = ["pm", "architect", "programmer", "tester", "reviewer"]

# Pre-defined workflows.
[workflows.requirements_review]
name = "Requirements Review"
description = "PM presents requirements, team discusses feasibility and scope."
steps = [
  { id = "present", speakers = ["pm"], prompt = "Present the requirements for: {{topic}}" },
  { id = "architecture", speakers = ["architect"], prompt = "Review technical feasibility and suggest architecture for the requirements above." },
  { id = "estimate", speakers = ["programmer", "tester"], prompt = "Estimate effort and identify risks. Programmer: implementation complexity. Tester: test surface and edge cases." },
  { id = "decide", speakers = ["pm", "manager"], prompt = "Based on the discussion, finalize scope and priorities. Manager: assess risk and timeline." },
]
consensus = "moderator_decides"
moderator = "manager"

[workflows.code_review]
name = "Code Review Discussion"
description = "Multi-perspective code review with security, style, and logic checks."
steps = [
  { id = "explain", speakers = ["programmer"], prompt = "Explain the changes in the code at: {{code_path}}" },
  { id = "review", speakers = ["reviewer", "security", "tester"], prompt = "Review the code changes described above. Reviewer: patterns and quality. Security: vulnerabilities. Tester: edge cases and test coverage." },
  { id = "respond", speakers = ["programmer"], prompt = "Address the review feedback. For each point, either accept and explain fix, or push back with reasoning." },
]
max_rounds = 1

[workflows.design_brainstorm]
name = "Design Brainstorm"
description = "Open-ended creative discussion about architecture or feature design."
steps = [
  { id = "context", speakers = ["pm"], prompt = "Describe the problem and goals: {{topic}}" },
  { id = "brainstorm", speakers = ["architect", "programmer", "designer"], prompt = "Brainstorm approaches. Architect: system design. Programmer: practical concerns. Designer: UX implications. Build on each other's ideas." },
  { id = "evaluate", speakers = ["reviewer", "security", "devops"], prompt = "Evaluate the proposed approaches. Consider: maintainability, security, operational cost." },
  { id = "synthesize", speakers = ["architect", "pm"], prompt = "Synthesize the discussion into a recommended approach with tradeoffs documented." },
]
max_rounds = 2
consensus = "deliberate_until_consensus"
```

## 7. Implementation Phases

### Phase 1: Foundation — Agent Core
**Goal**: Single agent can be configured, loaded, and run a single turn.

Files to create:
- `latte-agent-core/Cargo.toml`
- `latte-agent-core/src/lib.rs`
- `latte-agent-core/src/error.rs`
- `latte-agent-core/src/role.rs` — Role, RoleCategory, built-in role constructors
- `latte-agent-core/src/config.rs` — AgentConfig, ModelCatalog, TOML load/save
- `latte-agent-core/src/model_resolver.rs` — ModelResolver, ModelTier, resolve/escalate
- `latte-agent-core/src/context.rs` — ConversationContext, Message history
- `latte-agent-core/src/agent.rs` — Agent, AgentRunner, run_turn

Dependencies:
- `latte_ai` (from ../latte-rs-model-router)
- `latte_rs_agent_tools` (from ../latte-rs-agent-tools)
- `tokio`, `serde`, `toml`, `thiserror`, `handlebars`

Acceptance:
- Load `config/agents.toml` → build Agent struct with resolved Model + AiClient
- AgentRunner::run_turn("Hello") → calls AI API → returns response text
- ModelResolver resolves tier → concrete Model from catalog

### Phase 2: Multi-Agent Orchestration
**Goal**: Multiple agents can participate in sequenced discussion rounds.

Files to create:
- `latte-agent-orchestrator/Cargo.toml`
- `latte-agent-orchestrator/src/lib.rs`
- `latte-agent-orchestrator/src/error.rs`
- `latte-agent-orchestrator/src/round.rs` — Round, Turn, TurnOrder, TurnRecord
- `latte-agent-orchestrator/src/consensus.rs` — ConsensusMethod implementations
- `latte-agent-orchestrator/src/workflow.rs` — DiscussionWorkflow, WorkflowStep, StepHook
- `latte-agent-orchestrator/src/orchestrator.rs` — DiscussionOrchestrator

Dependencies:
- `latte-agent-core`
- `latte_ai`, `latte_rs_agent_tools`
- `tokio`, `serde`, `toml`

Acceptance:
- Create orchestrator with 3 agents (pm, dev, tester)
- Run discussion with fixed turn order: pm → dev → tester
- Each agent receives previous speakers' context
- Orchestrator collects all turns into DiscussionResult

### Phase 3: Role Templates & Tools
**Goal**: Rich role system with 10+ built-in roles and tool integration.

Files to create:
- `prompts/pm.md`, `prompts/programmer.md`, `prompts/tester.md`,
  `prompts/architect.md`, `prompts/devops.md`, `prompts/security.md`,
  `prompts/reviewer.md`, `prompts/designer.md`, `prompts/tech_writer.md`,
  `prompts/manager.md`
- `config/agents.toml`
- `config/models.toml`
- `config/discussion.toml`

Acceptance:
- Each role has a detailed, effective system prompt with `<role>`, `<context>`, `<rules>` sections
- Prompts support `{{variable}}` template substitution
- ToolManager integration: agents can use read/write/bash/search tools
- Users can add custom roles via TOML without recompilation

### Phase 4: CLI & Discussion Workflows
**Goal**: Usable CLI with pre-defined discussion workflows.

Files to create:
- `latte-agent-cli/Cargo.toml`
- `latte-agent-cli/src/main.rs`
- `latte-agent-cli/src/commands/mod.rs`
- `latte-agent-cli/src/commands/discuss.rs`
- `latte-agent-cli/src/commands/config.rs`
- `latte-agent-cli/src/commands/list.rs`

Acceptance:
```
# Run a discussion with specific roles
latte-agent discuss --topic "Implement user auth" --roles pm,architect,programmer,tester

# Run a pre-defined workflow
latte-agent discuss --workflow code_review --topic "src/auth/login.rs"

# List available roles, models, workflows
latte-agent list roles
latte-agent list models
latte-agent list workflows

# Show/add configuration
latte-agent config show
latte-agent config add-role --id "ml_engineer" --tier standard
latte-agent config set-model --role programmer --tier premium --model gpt-4o
```

### Phase 5: Advanced Features
**Goal**: Production-quality features from gsd-core patterns.

- **Context summarization**: Auto-summarize when context exceeds token budget
- **Dynamic tier escalation**: On failure/timeout, escalate tier and retry
- **Parallel agent execution**: Run independent agents concurrently within a step
- **Hook system**: Pre/post step hooks with content injection and gating
- **Streaming output**: Stream agent responses as they generate
- **Checkpoint protocol**: Human-action gates for critical decisions
- **Vote/consensus dashboard**: TUI summary of agent positions

## 8. Key Design Decisions

### 8.1 gsd-core Patterns Adopted
- **YAML frontmatter → TOML top-level config**: Role metadata lives in structured TOML
- **`<role>` XML tags → `{{variable}}` templates**: System prompts use handlebars for context injection
- **3-tier model routing**: Premium/Standard/Budget with per-role configurable mapping
- **5-step loop → workflow steps**: discuss→plan→execute→verify→ship → configurable workflow steps
- **Hook contributions/gates**: step-level pre/post hooks mirror loop-resolver injection
- **Capability → role mapping**: Tools assigned per role (mirrors capability.json `skills`/`agents` arrays)
- **Profile-based selection**: Users select which roles participate (mirrors install profiles)

### 8.2 Rust-specific Adaptations
- **Traits over generated JS**: `ToolManager` trait instead of shell-command gsd-tools
- **TOML over JSON**: Canonical Rust config format, better for hand-editing
- **async/await over shell orchestration**: Direct async calls instead of shell scripts
- **`Arc<dyn Trait>` for extensibility**: Users can implement custom `ToolManager`, `HandlerResolver`
- **`thiserror` + `anyhow`**: Error handling, not sentinel-string matching
- **Type-safe templates**: `handlebars` with typed context structs

### 8.3 What We Skip (for now)
- **Shell-based orchestration** — gsd-core runs agents via bash `Agent()` calls; we run them directly in-process
- **STATE.md lifecycle** — gsd-core's complex state machine with lockfiles; we use in-memory context
- **MCP integration** — gsd-core's `mcp__*` tool pattern; add later via ToolManager's `HttpHandlerResolver`
- **Install profiles** — gsd-core's core/standard/full profiles; TOML config serves this purpose
- **Autonomous mode** — gsd-core's auto-approve gates; add as config flag later
- **Lockfile-based state** — gsd-core's atomic RMW pattern; not needed for in-memory discussions

## 9. Dependency Graph

```
latte-rs-model-router (latte_ai)     latte-rs-agent-tools (latte_rs_agent_tools)
           │                                      │
           └──────────────┬───────────────────────┘
                          │
                   latte-agent-core
                          │
                   latte-agent-orchestrator
                          │
                    latte-agent-cli
```

## 10. Verification Strategy

| Phase | Verification |
|-------|-------------|
| 1 | Unit tests: ModelResolver resolves tiers; AgentRunner produces response from mock API |
| 2 | Integration test: 3 agents discuss a topic, each receives prior context, result contains all turns |
| 3 | Snapshot test: role prompts render with variables; custom role loads from TOML |
| 4 | E2E: `latte-agent discuss --topic "Design a REST API" --roles pm,architect` produces coherent discussion |
| 5 | Load test: 10 agents parallel execution; context summarization at token budget boundary |

---

**Plan version**: 1.0  
**Last updated**: 2026-06-14  
**Next step**: Phase 1 implementation — `latte-agent-core`
