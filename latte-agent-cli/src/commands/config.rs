//! `latte-agent config` — show or modify configuration.

use clap::{Args, Subcommand};

type AnyResult = Result<(), Box<dyn std::error::Error>>;

/// Show or modify configuration.
#[derive(Args, Debug)]
pub struct ConfigCmd {
    #[command(subcommand)]
    pub action: ConfigAction,
}

#[derive(Subcommand, Debug)]
enum ConfigAction {
    /// Show current configuration.
    Show(ConfigShow),
}

#[derive(Args, Debug)]
struct ConfigShow {
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

impl ConfigCmd {
    pub async fn run(&self) -> AnyResult {
        match &self.action {
            ConfigAction::Show(show) => show.run().await,
        }
    }
}

impl ConfigShow {
    async fn run(&self) -> AnyResult {
        println!("=== Configuration ===\n");

        // Agents config
        println!("[Agents: {}]", self.agents_config);
        match std::fs::read_to_string(&self.agents_config) {
            Ok(content) => {
                let line_count = content.lines().count();
                println!("  {} lines, {} bytes", line_count, content.len());
            }
            Err(e) => {
                println!("  Not found: {}", e);
            }
        }

        // Models config
        println!("\n[Models: {}]", self.models_config);
        match std::fs::read_to_string(&self.models_config) {
            Ok(content) => {
                let line_count = content.lines().count();
                println!("  {} lines, {} bytes", line_count, content.len());
            }
            Err(e) => {
                println!("  Not found: {}", e);
            }
        }

        // Discussion config
        println!("\n[Discussion: {}]", self.discussion_config);
        match std::fs::read_to_string(&self.discussion_config) {
            Ok(content) => {
                let line_count = content.lines().count();
                println!("  {} lines, {} bytes", line_count, content.len());
            }
            Err(e) => {
                println!("  Not found: {}", e);
            }
        }

        Ok(())
    }
}
