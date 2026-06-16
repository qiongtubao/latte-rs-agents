//! latte-agent — Multi-role agent discussion CLI.
//!
//! Usage:
//!   latte-agent discuss --topic "Design REST API" --roles pm,architect,programmer
//!   latte-agent list roles
//!   latte-agent config show

use clap::{Parser, Subcommand};

mod commands;

use commands::{config::ConfigCmd, discuss::DiscussCmd, list::ListCmd};

#[derive(Parser)]
#[command(name = "latte-agent", version, about = "Multi-role agent discussion system")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a multi-agent discussion
    Discuss(DiscussCmd),
    /// List available roles, models, or workflows
    List(ListCmd),
    /// Show or modify configuration
    Config(ConfigCmd),
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Command::Discuss(cmd) => cmd.run().await,
        Command::List(cmd) => cmd.run().await,
        Command::Config(cmd) => cmd.run().await,
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
