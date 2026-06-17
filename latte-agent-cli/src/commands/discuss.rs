//! `latte-agent discuss` — run a multi-agent discussion.

use std::collections::HashMap;

use clap::Args;
use latte_agent_core::agent::{Agent, AgentRunner};
use latte_agent_core::config::AgentConfig;
use latte_agent_core::model_resolver::ModelResolver;
use latte_agent_orchestrator::orchestrator::{DiscussionConfig, DiscussionOrchestrator};
use latte_agent_orchestrator::ConsensusMethod;
use latte_agent_orchestrator::DiscussionWorkflow;
use latte_ai::params::GenerateParams;

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

    /// Path to agents config TOML.
    #[arg(long, default_value = "config/agents.toml")]
    pub agents_config: String,

    /// Path to models config TOML.
    #[arg(long, default_value = "config/models.toml")]
    pub models_config: String,

    /// Path to discussion workflow TOML.
    #[arg(long, default_value = "config/discussion.toml")]
    pub discussion_config: String,

    /// Maximum discussion rounds.
    #[arg(long)]
    pub max_rounds: Option<usize>,

    /// Consensus method: majority, none, weighted.
    #[arg(long, default_value = "none")]
    pub consensus: String,
}

impl DiscussCmd {
    pub async fn run(&self) -> AnyResult {
        println!("=== latte-agent discussion ===");
        println!("Topic: {}", self.topic);
        println!("Roles: {:?}", self.roles);
        println!();

        // 1. Load configs
        let agent_config = AgentConfig::load(&self.agents_config)
            .map_err(|e| format!("failed to load agents config: {}", e))?;
        let models_config = AgentConfig::load(&self.models_config)
            .map_err(|e| format!("failed to load models config: {}", e))?;

        // Merge models from models_config into agent_config
        let mut merged = agent_config;
        merged.models = models_config.models;

        // 2. Build model resolver
        let resolver = ModelResolver::from_config(&merged)?;

        // 3. Resolve roles and create agents
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
        use serde::Deserialize;

        #[derive(Deserialize)]
        struct WorkflowsFile {
            #[serde(default)]
            default_workflow: Option<DiscussionWorkflow>,
            #[serde(default)]
            workflows: HashMap<String, DiscussionWorkflow>,
        }

        let content = std::fs::read_to_string(&self.discussion_config)?;
        let wf_file: WorkflowsFile = toml::from_str(&content)?;

        // Try named workflow first, then default
        if let Some(wf) = wf_file.workflows.get(name) {
            Ok(wf.clone())
        } else if name == "default" {
            wf_file
                .default_workflow
                .ok_or_else(|| "no default workflow found".into())
        } else {
            Err(format!(
                "workflow '{}' not found. Available: {:?}",
                name,
                wf_file.workflows.keys().collect::<Vec<_>>()
            )
            .into())
        }
    }
}
