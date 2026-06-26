//! `latte-agent workflow` — run a multi-agent discussion driven by a
//! named workflow file (default: `tech_director_dispatch`).
//!
//! The `tech_director_dispatch` workflow is the closest thing the
//! project has to a "responsible manager": a senior role reads the
//! task, decomposes it into per-role subtasks, the relevant roles
//! (PM, architect, programmer, tester) produce their slices, and the
//! tech_director synthesizes a final answer.
//!
//! Internally this is a thin wrapper over `DiscussCmd` with a default
//! `--workflow` value, so the bulk of the orchestration is unchanged.

use std::collections::HashMap;

use clap::Args;
use latte_agent_core::agent::{Agent, AgentRunner};
use latte_agent_core::config::AgentConfig;
use latte_agent_core::model_resolver::ModelResolver;
use latte_agent_orchestrator::orchestrator::{DiscussionConfig, DiscussionOrchestrator};
use latte_agent_orchestrator::ConsensusMethod;
use latte_agent_orchestrator::DiscussionWorkflow;
use latte_ai::params::GenerateParams;

use super::config_layer::{self, CliOverrides};

type AnyResult = Result<(), Box<dyn std::error::Error>>;

/// Run a multi-agent discussion driven by a named workflow.
#[derive(Args, Debug)]
pub struct WorkflowCmd {
    /// Discussion topic / user's task.
    #[arg(short, long)]
    pub topic: String,

    /// Comma-separated list of role ids (defaults to the workflow's own
    /// speakers when omitted). When you use the default
    /// `tech_director_dispatch` workflow, you can usually skip this.
    #[arg(short, long, value_delimiter = ',')]
    pub roles: Vec<String>,

    /// Workflow name to use. Default: `tech_director_dispatch` — the
    /// tech director decomposes the task and dispatches to PM,
    /// architect, programmer, and tester.
    #[arg(short = 'w', long, default_value = "tech_director_dispatch")]
    pub workflow: Option<String>,

    /// Path to agents config TOML (file or directory).
    #[arg(long, default_value = ".latte/agents")]
    pub agents_config: String,

    /// Path to models config TOML.
    #[arg(long, default_value = ".latte/models.toml")]
    pub models_config: String,

    /// Path to discussion workflow TOML (file or directory).
    #[arg(long, default_value = ".latte/workflows")]
    pub discussion_config: String,

    /// Maximum discussion rounds.
    #[arg(long)]
    pub max_rounds: Option<usize>,

    /// Consensus method: majority, none, weighted.
    #[arg(long, default_value = "none")]
    pub consensus: String,

    /// Pin the chat to a specific model id (or display name).
    #[arg(short = 'm', long = "model-id", value_name = "ID")]
    pub model_id: Option<String>,

    /// Override the api_key for a model. With `--model`, only that model is
    /// touched; otherwise every model that has an empty api_key is filled.
    #[arg(long, value_name = "KEY")]
    pub api_key: Option<String>,

    /// Restrict `--api-key` to a single model id.
    #[arg(long, value_name = "ID", requires = "api_key")]
    pub model: Option<String>,

    /// Per-model field override, repeatable: `id.field=value`.
    #[arg(long = "model-override", value_name = "ID.FIELD=VALUE")]
    pub model_overrides: Vec<String>,
}

impl WorkflowCmd {
    pub async fn run(&self) -> AnyResult {
        println!("=== latte-agent workflow ({} mode) ===", self.workflow_name());
        println!("Topic: {}", self.topic);
        if !self.roles.is_empty() {
            println!("Roles (override): {:?}", self.roles);
        }
        println!();

        // 1. Three-layer config: global (~/.latte/) ← project (CLI flags) ← CLI overrides.
        let cli = self.build_cli_overrides()?;
        let resolved = config_layer::load(
            Some(&self.agents_config),
            Some(&self.models_config),
            cli,
        )
        .map_err(|e| format!("failed to load configuration: {}", e))?;
        let merged = resolved.config;
        let resolver = resolved.resolver;

        // 2. Load the named workflow (default: tech_director_dispatch).
        let registry = latte_agent_orchestrator::WorkflowRegistry::load_with_global(Some(&self.discussion_config))
            .map_err(|e| format!("failed to load workflows: {}", e))?;
        let wf_name = self.workflow_name();
        let workflow = registry
            .resolve(Some(&wf_name))
            .map_err(|e| format!("workflow '{}' not found: {}", wf_name, e))?;

        // 3. Determine which roles to instantiate. If the user gave
        //    --roles, use exactly that. Otherwise, derive the set from
        //    the workflow's `speakers` lists (deduped) so the user
        //    doesn't have to repeat themselves.
        let default_params = GenerateParams::default();
        let mut role_names: Vec<String> = if self.roles.is_empty() {
            let mut set: Vec<String> = Vec::new();
            for step in &workflow.steps {
                for s in &step.speakers {
                    if !set.contains(s) {
                        set.push(s.clone());
                    }
                }
            }
            set
        } else {
            self.roles.clone()
        };

        let mut agents: HashMap<String, AgentRunner> = HashMap::new();
        for role_name in &role_names {
            let template = merged
                .roles
                .get(role_name)
                .ok_or_else(|| format!("role '{}' not found in config", role_name))?;
            let mut role = template.resolve(&default_params).await?;
            // Mirror `chat`: if the role has `allowed_tools`, append
            // a tool-usage section to its system prompt so the
            // model knows it can call read/list/search and how.
            // Without this the workflow step runs in raw-response
            // mode and the model hallucinates a project description
            // instead of inspecting the actual codebase.
            if !role.allowed_tools.is_empty() {
                role.system_prompt.push_str(&super::chat::tool_usage_prompt(
                    &role.allowed_tools,
                ));
            }
            let role_for_tools = role.clone();
            let tier = role.default_model_tier;
            let models = resolver.resolve_chain(&role.id, tier, &role.model_chain)?;
            let model_id = models[0].id.clone();
            let agent = Agent::new_with_chain(role_name.clone(), role, models, default_params.clone())?;
            let runner = if !role_for_tools.allowed_tools.is_empty() {
                let tm = super::chat::build_tool_manager(&role_for_tools.allowed_tools)
                    .await
                    .map_err(|e| format!("tool setup for role '{}' failed: {}", role_name, e))?;
                AgentRunner::new_with_tools(agent, tm, 16)
            } else {
                AgentRunner::new(agent)
            };
            agents.insert(role_name.clone(), runner);
        }
         println!();

        // 4. Workflow variables: the {{topic}} placeholder is what every
        //    step prompt in tech_director_dispatch uses. Steps that
        //    also need {{plan}} / {{pm}} / etc. read those from the
        //    orchestrator's shared context at runtime, not from
        //    initial variables.
        let mut variables = HashMap::new();
        variables.insert("topic".into(), self.topic.clone());

        let consensus = ConsensusMethod::parse(&self.consensus)
            .unwrap_or(ConsensusMethod::NoConsensus);

        let config = DiscussionConfig {
            workflow,
            consensus,
            max_rounds: self.max_rounds,
            context_token_budget: 32000,
            variables,
        };

        println!(
            "Starting workflow ({} round(s), {} steps)...\n",
            config.effective_max_rounds(),
            config.workflow.steps.len()
        );

        let mut orchestrator = DiscussionOrchestrator::new(agents, config)?;
        let result = orchestrator.run().await?;

        println!("\n=== Workflow Complete ===");
        println!("Rounds: {}", result.rounds.len());
        println!("Consensus: {}", if result.consensus_reached { "YES" } else { "NO" });
        println!(
            "Tokens: {} in / {} out",
            result.total_usage.input_tokens, result.total_usage.output_tokens
        );
        println!();
        println!("{}", orchestrator.transcript());
        Ok(())
    }

    fn workflow_name(&self) -> &str {
        self.workflow.as_deref().unwrap_or("tech_director_dispatch")
    }

    /// Build a [`CliOverrides`] from this subcommand. Mirrors
    /// `chat.rs` / `discuss.rs` so the three-layer config pipeline
    /// sees the same shape regardless of which subcommand ran it.
    fn build_cli_overrides(&self) -> Result<CliOverrides, String> {
        let mut out = CliOverrides {
            api_key: self.api_key.clone(),
            api_key_target: self.model.clone(),
            field_overrides: Vec::with_capacity(self.model_overrides.len()),
        };
        for raw in &self.model_overrides {
            let (id, field, value) = super::chat::parse_model_override(raw)
                .map_err(|e| format!("invalid --model-override '{}': {}", raw, e))?;
            out.field_overrides.push((id, field, value));
        }
        Ok(out)
    }
}
