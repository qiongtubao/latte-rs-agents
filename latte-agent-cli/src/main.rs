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
    chat::ChatCmd, checkpoint::CheckpointCmd, config::ConfigCmd, debug::DebugCmd,
    discuss::DiscussCmd, inject::InjectCmd, list::ListCmd, pause::PauseCmd,
    resume::ResumeCmd, run::RunCmd, workflow::WorkflowCmd,
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
    /// Run a task inside an isolated worktree sandbox
    Run(RunCmd),
    /// Inject a message into a running task
    Inject(InjectCmd),
    /// Pause a running task
    Pause(PauseCmd),
    /// Resume a paused task
    Resume(ResumeCmd),
    /// Manage checkpoints (create / list / rollback)
    Checkpoint(CheckpointCmd),
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
        Command::Run(cmd) => cmd.run().map_err(Into::into),
        Command::Inject(cmd) => cmd.run().map_err(Into::into),
        Command::Pause(cmd) => cmd.run().map_err(Into::into),
        Command::Resume(cmd) => cmd.run().map_err(Into::into),
        Command::Checkpoint(cmd) => cmd.run().map_err(Into::into),
    };
    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
