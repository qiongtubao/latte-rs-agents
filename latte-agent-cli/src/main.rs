//! latte-agent — Multi-role agent discussion CLI.
//!
//! Usage:
//!   latte-agent discuss --topic "Design REST API" --roles pm,architect,programmer
//!   latte-agent list roles
//!   latte-agent config show
//!   latte-agent debug <subcommand>   # offline inspection of recorded sessions

use clap::{Parser, Subcommand};

mod commands;

use commands::{
    chat::ChatCmd, config::ConfigCmd, debug::DebugCmd, discuss::DiscussCmd,
    list::ListCmd, workflow::WorkflowCmd,
};

#[derive(Parser)]
#[command(name = "latte-agent", version, about = "Multi-role agent discussion system")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Discuss(DiscussCmd),
    Chat(ChatCmd),
    /// Run a multi-agent discussion using a named workflow
    Workflow(WorkflowCmd),
    List(ListCmd),
    Config(ConfigCmd),
    /// Offline inspection of recorded sessions, prompts, parser, and hooks
    Debug(DebugCmd),
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Command::Discuss(cmd) => cmd.run().await,
        Command::Chat(cmd) => cmd.run().await,
        Command::Workflow(cmd) => cmd.run().await,
        Command::List(cmd) => cmd.run().await,
        Command::Config(cmd) => cmd.run().await,
        Command::Debug(cmd) => cmd.run().await,
    };
    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
