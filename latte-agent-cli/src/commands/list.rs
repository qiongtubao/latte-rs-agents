//! `latte-agent list` — list available roles, models, or workflows.

use clap::{Args, ValueEnum};

use super::config_layer::CliOverrides;

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

    /// Path to agents config (file or directory).
    #[arg(long, default_value = "config/agents")]
    pub agents_config: String,

    /// Path to models config.
    #[arg(long, default_value = "config/models.toml")]
    pub models_config: String,

    /// Path to discussion workflows (file or directory).
    #[arg(long, default_value = "config/workflows")]
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
        // Three-layer merge: global ~/.latte/ + project (roles are
        // project-only, but the loader also surfaces them consistently).
        let resolved = super::config_layer::load(
            Some(&self.agents_config),
            Some(&self.models_config),
            CliOverrides::default(),
        )?;
        let config = &resolved.config;

        println!("Available Roles:");
        println!("{:<20} {:<15} {:<12} {}", "ID", "CATEGORY", "TIER", "TOOLS");
        println!("{}", "-".repeat(70));

        let default_params = latte_ai::params::GenerateParams::default();
        let mut ids: Vec<&String> = config.roles.keys().collect();
        ids.sort();
        for id in &ids {
            let template = &config.roles[*id];
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
        // Three-layer merge: global ~/.latte/ + project + CLI overrides.
        let resolved = super::config_layer::load(
            Some(&self.agents_config),
            Some(&self.models_config),
            CliOverrides::default(),
        )?;
        let config = &resolved.config;

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

        if let Some(ref tiers) = config.models.tiers {
            println!("\nTier Mappings:");
            for (tier, model_id) in tiers {
                println!("  {} → {}", tier, model_id);
            }
        }

        if !resolved.sources.global.is_empty() {
            println!("\n(merged from global: {})", resolved.sources.global.join(", "));
        }
        Ok(())
    }

    async fn list_workflows(&self) -> AnyResult {
        let registry = latte_agent_orchestrator::WorkflowRegistry::load(&self.discussion_config)
            .map_err(|e| format!("failed to load workflows: {}", e))?;

        if let Some(ref default_wf) = registry.default {
            println!("Default Workflow: {}", default_wf.name);
            println!("  Description: {}", default_wf.description);
            println!("  Steps: {}", default_wf.steps.len());
            println!();
        } else {
            println!("(No default workflow)");
            println!();
        }

        println!("Named Workflows:");
        let mut names: Vec<&String> = registry.workflows.keys().collect();
        names.sort();
        for name in names {
            let wf = &registry.workflows[name];
            println!(
                "  {} \u{2014} {} ({} steps)",
                name,
                wf.description,
                wf.steps.len()
            );
            for step in &wf.steps {
                println!("    {}: {:?}", step.id, step.speakers);
            }
        }

        if registry.workflows.is_empty() && registry.default.is_none() {
            println!("No workflows defined in {}", self.discussion_config);
        }

        Ok(())
    }
}
