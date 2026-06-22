//! `latte-agent discuss` — run a multi-agent discussion.

use std::collections::HashMap;

use clap::Args;
use latte_agent_core::agent::{Agent, AgentRunner};
use latte_agent_orchestrator::orchestrator::{DiscussionConfig, DiscussionOrchestrator};
use latte_agent_orchestrator::ConsensusMethod;
use latte_agent_orchestrator::DiscussionWorkflow;
use latte_ai::params::GenerateParams;

use super::config_layer::{self, CliOverrides};

type AnyResult = Result<(), Box<dyn std::error::Error>>;

/// Run a multi-agent discussion.
#[derive(Args, Debug)]
pub struct DiscussCmd {
    /// Discussion topic.
    #[arg(short, long)]
    pub topic: String,

    /// Comma-separated list of roles (e.g. "pm,architect,programmer").
    #[arg(short, long, value_delimiter = ',')]
    pub roles: Vec<String>,

    /// Workflow name to use (from discussion.toml).
    #[arg(short, long)]
    pub workflow: Option<String>,

    /// Path to agents config TOML (file or directory).
    #[arg(long, default_value = "config/agents")]
    pub agents_config: String,

    /// Path to models config TOML.
    #[arg(long, default_value = "config/models.toml")]
    pub models_config: String,

    /// Path to discussion workflow TOML (file or directory).
    #[arg(long, default_value = "config/workflows")]
    pub discussion_config: String,

    /// Maximum discussion rounds.
    #[arg(long)]
    pub max_rounds: Option<usize>,

    /// Consensus method: majority, none, weighted.
    #[arg(long, default_value = "none")]
    pub consensus: String,

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

impl DiscussCmd {
    pub async fn run(&self) -> AnyResult {
        println!("=== latte-agent discussion ===");
        println!("Topic: {}", self.topic);
        println!("Roles: {:?}", self.roles);
        println!();
        // 1. Three-layer config: global (~/.latte/) ← project (CLI flags) ← CLI overrides.
        let cli = build_cli_overrides(self)?;
        let resolved = config_layer::load(
            Some(&self.agents_config),
            Some(&self.models_config),
            cli,
        )
        .map_err(|e| format!("failed to load configuration: {}", e))?;
        let merged = resolved.config;
        let resolver = resolved.resolver;

        // 2. Resolve roles and create agents
        let default_params = GenerateParams::default();
        let mut agents: HashMap<String, AgentRunner> = HashMap::new();

        for role_name in &self.roles {
            let template = merged
                .roles
                .get(role_name)
                .ok_or_else(|| format!("role '{}' not found in config", role_name))?;
            let role = template.resolve(&default_params).await?;
            let tier = role.default_model_tier;
            // Resolve the full model chain (primary + fallbacks) so the
            // agent can walk the priority list when the primary is
            // rate-limited or otherwise unavailable.
            let models = resolver.resolve_chain(&role.id, tier, &role.model_chain)?;
            let model_id = models[0].id.clone();

            let agent = Agent::new_with_chain(role_name.clone(), role, models, default_params.clone())?;
            let runner = AgentRunner::new(agent);

            agents.insert(role_name.clone(), runner);
            println!("  {} ({}) → {}", role_name, template.model_tier, model_id);
        }
        println!();

        // 4. Load or build workflow
        let workflow = if let Some(ref wf_name) = self.workflow {
            self.load_named_workflow(wf_name)?
        } else {
            self.build_default_workflow()
        };

        // 5. Create orchestrator config
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

        // 6. Run discussion
        println!("Starting discussion ({} round(s))...\n", config.effective_max_rounds());

        let mut orchestrator = DiscussionOrchestrator::new(agents, config)?;
        let result = orchestrator.run().await?;

        // 7. Print results
        println!("\n=== Discussion Complete ===");
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

    fn build_default_workflow(&self) -> DiscussionWorkflow {
        let steps = self
            .roles
            .iter()
            .enumerate()
            .map(|(i, role)| latte_agent_orchestrator::WorkflowStep {
                id: format!("step_{}", i),
                description: format!("{} speaks", role),
                speakers: vec![role.clone()],
                prompt: format!(
                    "You are discussing: {{topic}}. Share your perspective as a {}.",
                    role
                ),
                hooks: vec![],
                output_key: None,
            })
            .collect();

        DiscussionWorkflow {
            name: "default".into(),
            description: "Auto-generated from roles".into(),
            steps,
            max_rounds: 1,
            context_token_budget: 32000,
        }
    }

    fn load_named_workflow(&self, name: &str) -> Result<DiscussionWorkflow, Box<dyn std::error::Error>> {
        let registry = latte_agent_orchestrator::WorkflowRegistry::load(&self.discussion_config)
            .map_err(|e| format!("failed to load workflows: {}", e))?;
        registry.resolve(Some(name)).map_err(|e| e.into())
    }
}

fn build_cli_overrides(cmd: &DiscussCmd) -> Result<CliOverrides, String> {
    let mut out = CliOverrides {
        api_key: cmd.api_key.clone(),
        api_key_target: cmd.model.clone(),
        field_overrides: Vec::with_capacity(cmd.model_overrides.len()),
    };
    for raw in &cmd.model_overrides {
        let (id, field, value) = super::chat::parse_model_override(raw)
            .map_err(|e| format!("invalid --model-override '{}': {}", raw, e))?;
        out.field_overrides.push((id, field, value));
    }
    Ok(out)
}
