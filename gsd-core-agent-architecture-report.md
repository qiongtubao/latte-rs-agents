# GSD-Core Agent Architecture — Comprehensive Report

## 1. Directory Structure Overview (Top 3 Levels)

```
gsd-core/
├── agents/              (34 agent definition .md files, 704KB total)
├── capabilities/        (32 capability directories, each with capability.json)
│   ├── audit/
│   ├── ai-integration/
│   ├── antigravity/
│   ├── claude/
│   ├── code-review/
│   ├── codebuddy/
│   ├── codex/
│   ├── copilot/
│   ├── cursor/
│   ├── drift/
│   ├── gap-analysis/
│   ├── gemini/
│   ├── graphify/
│   ├── hermes/
│   ├── intel/
│   ├── kilo/
│   ├── kimi/
│   ├── mempalace/
│   ├── nyquist/
│   ├── opencode/
│   ├── pattern-mapper/
│   ├── profile-pipeline/
│   ├── qwen/
│   ├── research/
│   ├── schema-gate/
│   ├── security/
│   ├── tdd/
│   ├── trae/
│   ├── ui/
│   └── windsurf/
├── commands/gsd/        (slash-command skill definitions)
├── docs/                (documentation, skills, superpowers, tutorials)
├── gsd-core/
│   ├── bin/
│   │   ├── gsd-tools.cjs          (2043 lines — main CLI hub)
│   │   ├── gsd_run                (shell wrapper)
│   │   ├── lib/                   (compiled CJS modules)
│   │   └── shared/                (model-catalog.json, config manifests)
│   ├── contexts/         (dev.md, research.md, review.md)
│   ├── references/       (80+ reference docs consumed by agents)
│   ├── templates/        (35+ template files)
│   └── workflows/        (88 workflow .md files)
├── src/                 (TypeScript source, ~100 .cts files)
│   ├── core.cts                  (utilities, constants, re-exports)
│   ├── state.cts                 (2506 lines — STATE.md lifecycle engine)
│   ├── init.cts                  (2304 lines — agent dispatch init)
│   ├── verify.cts                (1881 lines — verification suite)
│   ├── commands.cts              (1439 lines — standalone commands + model resolution)
│   ├── install-profiles.cts      (773 lines — agent/skill profile resolution)
│   ├── loop-resolver.cts         (669 lines — loop hook resolution)
│   ├── surface.cts               (554 lines — runtime skill/agent surface)
│   ├── model-resolver.cts        (522 lines — model assignment policy)
│   ├── model-catalog.cts         (226 lines — typed model catalog access)
│   ├── model-profiles.cts        (38 lines — re-export shim)
│   ├── capability-activation.cts (100 lines — config key activation)
│   ├── capability-state.cts      (456 lines — capability state resolver)
│   ├── command-routing-hub.cts   (398 lines — typed dispatch hub)
│   ├── agent-command-router.cts  (103 lines — classify-failure handler)
│   ├── config-loader.cts         (715 lines — project config loading)
│   ├── configuration.cts         (252 lines — legacy-key normalization, defaults)
│   ├── clusters.cts              (150 lines — skill cluster definitions)
│   └── loop-host-contract.cjs    (generated, 105 lines — 12 canonical points)
├── tests/               (744+ test files)
├── scripts/             (50+ build/CI scripts)
├── hooks/               (git hooks, statusline, update banner)
└── assets/              (logos, icons)
```

## 2. Agent Definition Mechanism

### 2.1 File Format

Agents are defined as **Markdown files** in `agents/`, each with:

**YAML Frontmatter** (at the top, `---` delimited):
```yaml
---
name: gsd-planner
description: Creates executable phase plans with task breakdown...
tools: Read, Write, Edit, Bash, Glob, Grep, WebFetch, mcp__context7__*
color: green
# hooks: (optional — commented out in most agents)
---
```

**XML-tagged Role Body** (the main content):
- `<role>` — agent persona, responsibilities, spawning context
- `<documentation_lookup>` — how to fetch library docs
- `<project_context>` — project-specific context loading
- Domain-specific sections (`<task_breakdown>`, `<execution_flow>`, `<deviation_rules>`, etc.)

### 2.2 Agent Inventory (34 agents)

| Category | Agents |
|----------|--------|
| **Planning** | gsd-planner, gsd-roadmapper, gsd-pattern-mapper, gsd-framework-selector, gsd-eval-planner |
| **Research** | gsd-phase-researcher, gsd-project-researcher, gsd-research-synthesizer, gsd-codebase-mapper, gsd-ui-researcher, gsd-domain-researcher, gsd-doc-classifier, gsd-doc-synthesizer, gsd-ai-researcher, gsd-advisor-researcher, gsd-intel-updater, gsd-mempalace-curator, gsd-user-profiler |
| **Execution** | gsd-executor, gsd-debugger, gsd-code-fixer, gsd-doc-writer, gsd-debug-session-manager |
| **Verification** | gsd-verifier, gsd-plan-checker, gsd-integration-checker, gsd-nyquist-auditor, gsd-ui-checker, gsd-ui-auditor, gsd-doc-verifier, gsd-code-reviewer, gsd-security-auditor, gsd-eval-auditor |
| **Discuss** | gsd-assumptions-analyzer |

### 2.3 Key Agent Definition Excerpts

**gsd-planner.md** (47.7KB, 1049 lines):
- Tools: `Read, Write, Edit, Bash, Glob, Grep, WebFetch, mcp__context7__*`
- Color: green
- Contains structured sections: `<role>`, `<context_fidelity>`, `<scope_reduction_prohibition>`, `<planner_authority_limits>`, `<philosophy>`, `<discovery_levels>`, `<task_breakdown>`, `<dependency_graph>`, `<scope_estimation>`

**gsd-executor.md** (41.5KB, 787 lines):
- Tools: `Read, Write, Edit, Bash, Grep, Glob, mcp__context7__*`
- Color: yellow
- Structured: `<execution_flow>`, `<deviation_rules>` (4 rules for auto-fix), `<checkpoint_protocol>`, `<task_commit_protocol>`, `<analysis_paralysis_guard>`

## 3. Model Routing / Assignment Architecture

### 3.1 Three-Layer Model Assignment

The model assignment system has three layers:

```
Layer 1: model-catalog.json  ──→  Agent → Model Profile Mapping
Layer 2: model-resolver.cts   ──→  Config-driven model ID resolution
Layer 3: runtime tier defaults──→  Profile tier → concrete model ID
```

### 3.2 model-catalog.json (`gsd-core/bin/shared/`)

**Profiles**: `quality`, `balanced`, `budget`, `adaptive`, `inherit`

**Phase Types**: `planning`, `discuss`, `research`, `execution`, `verification`, `completion`

**Adaptive Tier Map**:
```json
{ "heavy": "opus", "standard": "sonnet", "light": "haiku" }
```

**Agent Model Mapping** (per agent, per profile):
```json
"gsd-planner": {
  "golden":     "opus",        // quality profile
  "balanced":   "opus",        // balanced profile  
  "budget":     "sonnet",      // budget profile
  "phaseType":  "planning",
  "routingTier": "heavy"       // for dynamic routing / retry escalation
}
```

**Key pattern**: Agents have 3 profile tiers (golden/balanced/budget) mapping to model tiers (opus/sonnet/haiku), plus a `phaseType` and `routingTier` for dynamic routing.

**Runtime Tier Defaults** (16 runtimes):
```json
"claude": {
  "opus":   { "model": "claude-opus-4-8" },
  "sonnet": { "model": "claude-sonnet-4-6" },
  "haiku":  { "model": "claude-haiku-4-5" }
}
```

### 3.3 Model Resolution Pipeline

**`resolveModelInternal(cwd, agentType)`** (model-resolver.cts:160-237):
1. Loads project config via `loadConfig(cwd)`
2. Gets `model_profile` from config (default: `"balanced"`)
3. Looks up agent profile in `MODEL_PROFILES[agentType][profile]` → gets tier (e.g., `"sonnet"`)
4. Resolves tier → concrete model ID via `resolveTierEntry({runtime, tier, overrides})`
5. Falls back through priority: override → runtime default → provider preset

**`resolveModelForTier(cwd, agentType, attempt)`** — dynamic routing with retry escalation:
1. On first attempt (attempt=0), uses agent's configured tier
2. On retry (attempt>0), walks up the tier ladder: light→standard→heavy
3. Uses `nextTier()` for escalation

**`resolveEffortInternal(cwd, agentType)`** — unified effort resolution:
- Checks: override → agent-specific config → tier default → global default
- Effort ladder: `minimal` → `low` → `medium` → `high` → `xhigh` → `max`
- Different tiers get different default efforts (light→low, standard→high, heavy→xhigh)

**`resolveFastModeInternal(cwd, agentType)`** — fast_mode toggle for API runtimes

### 3.4 Config-Driven Overrides (`config-defaults.manifest.json`)

Users can override model assignments per-agent:
```json
{
  "model_profile": "balanced",
  "effort": {
    "default": "high",
    "routing_tier_defaults": { "light": "low", "standard": "high", "heavy": "xhigh" },
    "agent_overrides": {}
  },
  "fast_mode": {
    "enabled": false,
    "routing_tier_defaults": { "light": true, "standard": false, "heavy": false },
    "agent_overrides": {}
  },
  "agent_skills": {}
}
```

Provider presets: `anthropic`, `anthropic-fable`, `openai`, `google`, `qwen`, `generic`

## 4. Agent Lifecycle: Create → Configure → Run

### Step 1: Init — Model Resolution
**File**: `src/init.cts`, function `cmdInitPhaseOp(cwd, phase, raw)` (lines 838-993)

The orchestrator calls `gsd-tools query init.plan-phase <PHASE>` which:
1. Loads project config
2. Resolves model for each role agent (e.g., `resolveModelInternal(cwd, "gsd-planner")`)
3. Resolves effort and fast_mode
4. Returns a JSON blob with: `planner_model`, `researcher_model`, `checker_model`, `research_enabled`, etc.

### Step 2: Agent Skills Block
**File**: `src/init.cts`, function `buildAgentSkillsBlock(config, agentType, projectRoot)` (lines 1863-1959)

Builds the `<agent_skills>` XML block injected into the agent's system prompt:
- Checks `config.agent_skills[agentType]` for custom skill assignments
- Scans project `.claude/rules/` and workspace skills
- Returns a markdown string listing which skills the agent has access to

### Step 3: Agent Spawn
Agents are spawned as subagents by the **orchestrator** (Claude Code running a workflow .md file):

```bash
# In workflows/plan-phase.md:
AGENT_SKILLS_RESEARCHER=$(gsd_run query agent-skills gsd-phase-researcher)
AGENT_SKILLS_PLANNER=$(gsd_run query agent-skills gsd-planner)
AGENT_SKILLS_CHECKER=$(gsd_run query agent-skills gsd-plan-checker)
```

The orchestrator then calls `Agent()` with the resolved model, agent definition file, and skill blocks.

### Step 4: Run & Classify Failure
**File**: `src/agent-command-router.cts` (103 lines)

The `routeAgentCommand` handles `classify-failure` subcommand:
- Detects **quota-exceeded** failures (sentinel strings in error body)
- Detects **classifyHandoff** bugs (Claude Code internal error)
- Returns structured failure classification for retry logic

### Lifecycle Diagram
```
┌─────────────┐     ┌──────────────────┐     ┌─────────────────┐
│ Orchestrator │────▶│ gsd-tools query  │────▶│ init.PhaseOp    │
│ (workflow.md)│     │ init.plan-phase  │     │ resolveModel()  │
└─────────────┘     └──────────────────┘     │ resolveEffort() │
       │                                      └────────┬────────┘
       │  Agent(agentDef, model, skills)               │
       ▼                                               ▼
┌─────────────┐     ┌──────────────────┐     ┌─────────────────┐
│  Subagent   │────▶│ agent-command-   │────▶│ Failure         │
│  Executes   │     │ router           │     │ Classification  │
└─────────────┘     │ classify-failure │     │ (retry/ escalate)│
                    └──────────────────┘     └─────────────────┘
```

## 5. Multi-Agent Orchestration Pattern

### 5.1 Loop Host Contract (12 Canonical Points)
**File**: `gsd-core/bin/lib/loop-host-contract.cjs` (generated, 105 lines)

Defines the phase workflow as 5 steps with 12 hook points:

| Step | Points | Agent Roles | Produces | Consumes |
|------|--------|-------------|----------|----------|
| **discuss** | discuss:pre, discuss:post | orchestrator | CONTEXT.md | — |
| **plan** | plan:pre, plan:post | researcher, planner, checker | PLAN.md | CONTEXT.md |
| **execute** | execute:pre, execute:wave:pre, execute:wave:post, execute:post | executor, verifier | SUMMARY.md | PLAN.md |
| **verify** | verify:pre, verify:post | orchestrator | UAT.md | SUMMARY.md |
| **ship** | ship:pre, ship:post | orchestrator | — | UAT.md |

### 5.2 Workflow Orchestration Files
**Directory**: `gsd-core/workflows/` (88 .md files)

Each workflow is a markdown file with frontmatter pragmas:
```html
<!-- gsd:loop-host
step: plan
points: plan:pre, plan:post
agent-roles: researcher, planner, checker
produces: PLAN.md
consumes: CONTEXT.md
-->
```

Workflows define:
- **Purpose**: what this step does
- **Required reading**: reference docs
- **Available agent types**: exact agent names for subagent spawning
- **Runtime compatibility**: how Agent() spawning works on different platforms
- **Process**: step-by-step shell/bash orchestration with decision logic

### 5.3 Agent Collaboration Pattern (plan-phase example)

From `workflows/plan-phase.md`:
```
1. Init → resolve models for researcher, planner, checker
2. Research (if needed) → spawn gsd-phase-researcher
3. Plan → spawn gsd-planner (with research context)
4. Verify → spawn gsd-plan-checker (reviews plan quality)
5. Revision loop → max 3 iterations of plan → check → revise
6. Done → update STATE.md
```

Key orchestration principles:
- **Role separation is mandatory** — agents never absorb other roles inline
- **Subagent spawning** via `Agent()` tool calls (not inline)
- **Revision loops** with configurable max passes (`max_discuss_passes: 3`)
- **Checkpoint protocol** — human-action gates for auth, decisions
- **Dynamic routing** — on failure, escalate tier (light→standard→heavy) with configurable retry budget

### 5.4 Loop Resolver — Hook Injection
**File**: `src/loop-resolver.cts` (669 lines)

At each loop point (e.g., `plan:pre`), the loop resolver:
1. Reads the capability registry
2. Finds hooks registered for that point
3. Checks activation conditions (config keys, capability state)
4. Renders hook contributions as markdown injected into the agent's context
5. Three hook kinds: `step` (procedural), `contribution` (content injection), `gate` (blocking check)

### 5.5 Autonomous Mode
Config key: `workflow.auto_advance` or `_auto_chain_active`
- **checkpoint:human-verify** → auto-approved (except package-legitimacy gates)
- **checkpoint:decision** → auto-selects first option
- **checkpoint:human-action** → still stops (auth gates can't be automated)

## 6. Capability System

### 6.1 capability.json Structure
**Directory**: `capabilities/` (32 capabilities)

Each capability has:
```json
{
  "id": "mempalace",
  "role": "feature",
  "title": "MemPalace memory",
  "description": "Cross-session, cross-project memory...",
  "tier": "full",
  "requires": [],
  "runtimeCompat": { "supported": ["*"], "unsupported": [] },
  "skills": ["mempalace-recall", "mempalace-capture"],
  "agents": ["gsd-mempalace-curator"],
  "config": { /* typed config schema */ },
  "commands": [ /* CLI command families */ ],
  "hooks": [],      // loop-point hooks
  "steps": [],      // procedural steps
  "contributions": [], // content injection at loop points
  "gates": []       // blocking validation
}
```

### 6.2 Capability Activation Flow
**File**: `src/capability-state.cts` (456 lines), `src/capability-activation.cts` (100 lines)

1. **Registry loading**: `capability-registry.cjs` (generated) aggregates all `capability.json` files
2. **Profile resolution**: which install profile (core/standard/full) is active
3. **Surface resolution**: which skills/agents are enabled
4. **Config activation**: for each capability, check `config.key` → determines if active
5. **Hook injection**: active capabilities inject hooks at their registered loop points

### 6.3 Capability Tiers
- `"full"` — always available, heavyweight
- `"standard"` — available in standard+ profiles
- `"core"` — available in all profiles

## 7. Install Profile System

### 7.1 Profile Definitions (`src/install-profiles.cts`)
```typescript
const PROFILES = {
  core: [     // 8 core loop skills
    'new-project', 'discuss-phase', 'plan-phase',
    'execute-phase', 'phase', 'help', 'update', 'surface'
  ],
  standard: [ // core + phase management + workspace
    ...core, 'phase', 'review', 'config', 'progress',
    'resume-work', 'pause-work', 'workspace'
  ],
  full: '*'   // all skills
};
```

### 7.2 Profile Resolution
1. `resolveProfile({ modes, manifest, registry })` computes transitive closure
2. `computeClosure(base, manifest)` — follows `requires:` dependencies in skill frontmatter
3. `_capabilitySkillsForMode(mode, registry)` — adds capability-contributed skills
4. `stageAgentsForProfile(srcAgentsDir, resolvedProfile)` — filters agents directory

### 7.3 Agent Filtering during Install
`stageAgentsForProfile()` copies only agent files whose stems are in the resolved profile's agent set — unneeded agents are excluded from the runtime installation.

## 8. Command Dispatch System

### 8.1 CLI Hub (`gsd-core/bin/gsd-tools.cjs`, 2043 lines)
Entry point: `main()` → `runCommand()` — routes commands through:
1. **Command family routing** — traditional `family:action` dispatch
2. **Capability command dispatch** (`dispatchCapabilityCommand`) — registry-based routing using `commandFamilies` index

### 8.2 Command Routing Hub (`src/command-routing-hub.cts`, 398 lines)
Typed dispatch with discriminated union result types:
- `OkResult` — success
- `UnknownCommandResult` — command not found
- `InvalidArgsResult` — argument validation failure
- `HandlerRefusalResult` — handler rejected request
- `HandlerFailureResult` — handler threw

### 8.3 Command Families
Router files handle groups: `phase-command-router`, `state-command-router`, `verify-command-router`, `agent-command-router`, `roadmap-command-router`, `init-command-router`, `check-command-router`, `task-command-router`, etc.

## 9. State Management

### 9.1 `src/state.cts` (2506 lines)
Comprehensive STATE.md lifecycle:
- **Read operations**: `load`, `get`, `json`
- **Write operations**: `patch`, `update`, `advance-plan`, `begin-phase`
- **Atomic RMW**: `readModifyWriteStateMd` with `acquireStateLock` (O_EXCL retry)
- **Sync**: `syncStateFrontmatter` — YAML frontmatter ↔ markdown body
- **Gate functions**: `cmdStatePlannedPhase`, `cmdStateCompletePhase`, `cmdStateMilestoneSwitch`
- **Signal files**: `WAITING.json` for decision points

### 9.2 Lock File Pattern
Thread-safe state access via per-file lockfiles with retry on transient errnos (ENOENT, EINVAL, EIO, ESTALE, EAGAIN, EINTR) — handles Docker overlay-fs, NFS, and concurrent O_EXCL races.

## 10. Tool Integration Pattern

### 10.1 Agent Tool Declaration
Agents declare their tool access in YAML frontmatter:
```yaml
tools: Read, Write, Edit, Bash, Glob, Grep, WebFetch, mcp__context7__*
```

The runtime (Claude Code, Codex, etc.) enforces these tool restrictions when spawning the agent.

### 10.2 MCP Integration
Agents reference MCP servers via `mcp__<server>__*` prefix in their tool list. The `mcp__context7__*` pattern is used by most agents for documentation lookups.

### 10.3 GSD CLI Tools
Agents invoke `gsd-tools` via shell to:
- Query state: `gsd_run query state.load`
- Get config: `gsd_run query config-get workflow.auto_advance`
- Record progress: `gsd_run state.record-session`
- Init phases: `gsd_run query init.execute-phase "${PHASE}"`

## 11. Surface Module

**File**: `src/surface.cts` (554 lines)

Manages which skills and agents are active in the runtime:
- `resolveSurface(runtimeConfigDir, manifest)` — computes effective surface
- `applySurface(...)` — syncs skill/agent files to runtime destination
- `listSurface(...)` — shows enabled/disabled skills with token cost
- Profile → surface mapping: core/standard/full profiles determine which skills are installed

## 12. Key Architectural Patterns

### 12.1 YAML Frontmatter + XML Tags
Agents use YAML frontmatter for metadata and XML tags (`<role>`, `<context>`, etc.) for content structure. This is a **markdown-native DSL** — agents are prompts, not code.

### 12.2 Generated CJS from TypeScript
All `.cts` source files compile to `.cjs` in `gsd-core/bin/lib/`. The `core.cts` acts as a barrel re-exporting symbols from ~10 specialized modules.

### 12.3 Federated Config
Config keys can come from multiple sources merged at runtime:
1. `config-defaults.manifest.json` (canonical defaults)
2. Project-level `.gsd/config.json` (user overrides)
3. Capability registry `configSchema` (capability-contributed keys)

### 12.4 Profile → Capability → Agent Pipeline
```
Install Profile → Capability Activation → Agent Selection → Model Assignment
    (core/           (config key         (agents[] in      (model-catalog
     standard/        resolution)         capability.json)   + resolver)
     full)
```

### 12.5 Workflow as Code
Workflow `.md` files contain executable shell orchestration mixed with markdown documentation — they are run by the orchestrator (Claude Code), which interprets both the prose instructions and the embedded bash commands.

## 13. File Summary Table

| File | Lines | Purpose |
|------|-------|---------|
| `agents/*.md` | 34 files, ~704KB | Agent role/persona definitions |
| `src/state.cts` | 2506 | STATE.md lifecycle, locks, sync |
| `src/init.cts` | 2304 | Agent dispatch init, model resolution |
| `gsd-core/bin/gsd-tools.cjs` | 2043 | CLI hub, command dispatch |
| `src/verify.cts` | 1881 | Verification suite, health checks |
| `src/commands.cts` | 1439 | Standalone commands, model resolution |
| `src/install-profiles.cts` | 773 | Profile resolution for skills/agents |
| `src/config-loader.cts` | 715 | Project config loading, merge |
| `src/loop-resolver.cts` | 669 | Loop-point hook resolution |
| `src/surface.cts` | 554 | Runtime skill/agent surface management |
| `src/model-resolver.cts` | 522 | Model assignment policy |
| `src/capability-state.cts` | 456 | Capability state resolution |
| `src/command-routing-hub.cts` | 398 | Typed command dispatch hub |
| `src/model-catalog.cts` | 226 | Typed model catalog access |
| `src/configuration.cts` | 252 | Config normalization, defaults |
| `src/clusters.cts` | 150 | Skill cluster definitions |
| `gsd-core/bin/shared/model-catalog.json` | ~160 | Agent→model mappings, tier defaults |
| `gsd-core/bin/shared/config-defaults.manifest.json` | ~98 | Canonical config defaults |
| `gsd-core/bin/lib/loop-host-contract.cjs` | ~105 | 12-point loop contract |
| `capabilities/*/capability.json` | 32 files | Capability definitions |
| `gsd-core/workflows/*.md` | 88 files | Workflow orchestration |

---

**Report generated 2026-06-14 from `~/Documents/github/gsd-core/`**
