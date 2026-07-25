//! `latte-agent config` — show or modify configuration.

use clap::{Args, Subcommand};

use super::config_layer::CliOverrides;

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
    /// Path to agents config (file or directory).
    #[arg(long, default_value = ".latte/agents.d")]
    pub agents_config: String,

    /// Path to models config.
    #[arg(long, default_value = ".latte/models.d")]
    pub models_config: String,

    /// Path to discussion workflows (file or directory).
    #[arg(long, default_value = ".latte/workflows.d")]
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

        // Three-layer merge: global (~/.latte/) ← project (CLI flags) ← CLI overrides.
        let resolved = super::config_layer::load(
            Some(&self.agents_config),
            Some(&self.models_config),
            CliOverrides::default(),
        )?;
        if resolved.sources.global.is_empty() {
            println!("  (none found; checked ~/.latte/models.{{yaml,toml}} and ~/.latte/models.d/)");
        } else {
            for src in &resolved.sources.global {
                println!("  {}", src);
            }
        }

        println!("\n[Project: agents @ {}]", self.agents_config);
        match std::fs::metadata(&self.agents_config) {
            Ok(m) => println!("  loaded ({} bytes)", m.len()),
            Err(_) => println!("  (not present)"),
        }

        println!("\n[Project: models @ {}]", self.models_config);
        match std::fs::metadata(&self.models_config) {
            Ok(m) => println!("  loaded ({} bytes)", m.len()),
            Err(_) => println!("  (not present)"),
        }

        println!("\n[Discussion: {}]", self.discussion_config);
        match std::fs::metadata(&self.discussion_config) {
            Ok(m) => println!("  loaded ({} bytes)", m.len()),
            Err(_) => println!("  (not present)"),
        }
        println!("\n[Merged model catalog: {} models]", resolved.config.models.models.len());
        for m in &resolved.config.models.models {
            let key = classify_key(&m.api_key);
            println!(
                "  {:<35} provider={} base_url={} api_key={}",
                m.name, m.provider, m.base_url, key
            );
        }

        if !resolved.sources.cli_overrides.is_empty() {
            println!("\n[CLI overrides]");
            for o in &resolved.sources.cli_overrides {
                println!("  {}", o);
            }
        }

        Ok(())
    }
}

/// Three-state display for an api_key: empty, unresolved `${VAR}` placeholder,
/// or a real (redacted) value.
fn classify_key(s: &str) -> String {
    let t = s.trim();
    if t.is_empty() {
        "<empty>".to_string()
    } else if t.starts_with("${") {
        format!("<placeholder:{}>", t)
    } else if t.len() <= 8 {
        "***".to_string()
    } else {
        format!("{}***{}", &t[..4], &t[t.len() - 2..])
    }
}
