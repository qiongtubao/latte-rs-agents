//! `latte-agent list` — list available roles, models, or workflows.

use clap::{Args, ValueEnum};
use latte_agent_core::config::AgentConfig;


type AnyResult = Result<(), Box<dyn std::error::Error>>;

#[derive(ValueEnum, Debug, Clone)]
enum ListTarget {
    Roles,
    Models,
    Workflows,
}

/// List available resources.
#[derive(Args, Debug)]
pub struct ListCmd {
    /// What to list: roles, models, workflows.
    #[arg(value_enum)]
    pub target: ListTarget,

    /// Path to agents config.
    #[arg(long, default_value = "config/agents.toml")]
    pub agents_config: String,

    /// Path to models config.
    #[arg(long, default_value = "config/models.toml")]
    pub models_config: String,

    /// Path to discussion config.
    #[arg(long, default_value = "config/discussion.toml")]
    pub discussion_config: String,
}

impl ListCmd {
    pub async fn run(&self) -> AnyResult {
        match self.target {
            ListTarget::Roles => self.list_roles().await,
            ListTarget::Models => self.list_models().await,
            ListTarget::Workflows => self.list_workflows().await,
        }
    }

    async fn list_roles(&self) -> AnyResult {
        let config = AgentConfig::load(&self.agents_config)?;

        println!("Available Roles:");
        println!("{:<20} {:<15} {:<12} {}", "ID", "CATEGORY", "TIER", "TOOLS");
        println!("{}", "-".repeat(70));

        let default_params = latte_ai::params::GenerateParams::default();
        for (id, template) in &config.roles {
            let role = template.resolve(&default_params).await?;
            println!(
                "{} {:<18} {:<15} {:<12} {}",
                role.icon,
                id,
                template.category,
                template.model_tier,
                template.tools.join(", ")
            );
        }

        Ok(())
    }

    async fn list_models(&self) -> AnyResult {
        let config = AgentConfig::load(&self.models_config)?;

        println!("Available Models:");
        println!(
            "{:<35} {:<12} {:<10} {:>10}",
            "ID", "PROVIDER", "TIER", "CONTEXT"
        );
        println!("{}", "-".repeat(80));

        for model in &config.models.models {
            println!(
                "{:<35} {:<12} {:<10} {:>10}",
                model.id,
                model.provider,
                model.tier.as_deref().unwrap_or("-"),
                model.context_window
            );
        }

        // Also show tier mappings
        if let Some(ref tiers) = config.models.tiers {
            println!("\nTier Mappings:");
            for (tier, model_id) in tiers {
                println!("  {} → {}", tier, model_id);
            }
        }

        Ok(())
    }

    async fn list_workflows(&self) -> AnyResult {
        use serde::Deserialize;
        use std::collections::HashMap;

        #[derive(Deserialize)]
        struct WorkflowsFile {
            #[serde(default)]
            default_workflow: Option<latte_agent_orchestrator::DiscussionWorkflow>,
            #[serde(default)]
            workflows: HashMap<String, latte_agent_orchestrator::DiscussionWorkflow>,
        }

        let content = std::fs::read_to_string(&self.discussion_config)?;
        let wf_file: WorkflowsFile = toml::from_str(&content)?;

        if let Some(ref default_wf) = wf_file.default_workflow {
            println!("Default Workflow: {}", default_wf.name);
            println!("  Description: {}", default_wf.description);
            println!("  Steps: {}", default_wf.steps.len());
            println!();
        }

        println!("Named Workflows:");
        for (name, wf) in &wf_file.workflows {
            println!("  {} — {} ({} steps)", name, wf.description, wf.steps.len());
            for step in &wf.steps {
                println!("    {}: {:?}", step.id, step.speakers);
            }
        }

        if wf_file.workflows.is_empty() && wf_file.default_workflow.is_none() {
            println!("No workflows defined in {}", self.discussion_config);
        }

        Ok(())
    }
}
