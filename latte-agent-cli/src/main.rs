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
    // 把 `build.rs` 编译产物里的 rtk binary 注入到 PATH, 这样
    // `bash {"command": "rtk <subcmd>"}` 能直接解析到 (不需要全局装 rtk).
    // `build.rs` 在 submodule 没 init / 用户 opt-out 时不会创建 rtk binary,
    // 这里同样 no-op, rtk_assistant role 走 fallback 路径.
    init_rtk_path();

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

/// 把 `build.rs` 编译到 `OUT_DIR/rtk` 的 rtk binary 路径 prepend 到
/// 当前进程的 `PATH`. 这样 `bash {"command": "rtk <subcmd>"}` 在
/// 不依赖用户全局装 rtk 的前提下就能解析到 binary.
///
/// 安全 / 边界:
/// - `OUT_DIR` 是 cargo 在 build 时给 `build.rs` 设的常量, runtime 用
///   `env!("OUT_DIR")` 嵌入, 不做文件系统探测. 也就是说: 如果 build.rs
///   因为 submodule 没 init / `LATTE_AGENT_SKIP_RTK_BUILD=1` 没编译 rtk,
///   `OUT_DIR` 仍然指向一个有效目录, 只是里面没 `rtk` binary. 我们在
///   prepend PATH 之前先检查 binary 是否存在, 存在才加, 不存在就 no-op.
/// - 改 PATH 只影响当前进程 (subprocess 继承), 不污染用户 shell env.
fn init_rtk_path() {
    let rtk_dir = std::path::Path::new(env!("OUT_DIR"));
    let rtk_bin_name = if cfg!(windows) { "rtk.exe" } else { "rtk" };
    let rtk_bin = rtk_dir.join(rtk_bin_name);
    if !rtk_bin.exists() {
        // build.rs 跳过了 (submodule 缺 / 用户 opt-out / 编译失败), 不动 PATH.
        return;
    }

    let current_path = match std::env::var_os("PATH") {
        Some(p) => p,
        None => return,
    };
    // 把 rtk 所在目录放在 PATH 最前, 优先于任何系统 / 用户装的同名 binary.
    let mut iter = std::env::split_paths(&current_path);
    let new_path = match std::env::join_paths(
        std::iter::once(rtk_dir.to_path_buf()).chain(iter.by_ref()),
    ) {
        Ok(p) => p,
        Err(_) => return,
    };
    // SAFETY: tokio runtime 还没启动, 不会有线程持有 env var 句柄.
    unsafe {
        std::env::set_var("PATH", new_path);
    }
    let _ = iter; // keep iter alive until after join_paths consumes it
}
