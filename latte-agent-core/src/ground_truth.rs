//! Server-derived runtime ground-truth block.
//!
//! `ground_truth_block` builds the `<system_environment>` block that the
//! controller / chat runner appends to every role's system prompt. The
//! values are read from the host process right now (`cwd`, hostname,
//! arch, …) and embedded verbatim in the prompt. This is **structural
//! ground-truth injection**:
//!
//! 1. We are not *restricting* what the model may say.
//! 2. We are *feeding* the model real values for facts the user can ask
//!    about — current directory, hostname, OS, time.
//!
//! Why: observed end-to-end on 2026-07-11 — when a user asked the UI
//! "what's my cwd?" the manager model sometimes answered from training
//! data (`/Users/zhouguodong/...`) instead of running `pwd`. With this
//! block the model has the actual value in its context and won't have
//! to guess.
//!
//! If the model still hallucinates on top of ground truth, that's a
//! different problem (model quality / sampling / temperature) that a
//! prompt-level injection cannot fix anyway.
use std::path::Path;

/// Render the `<system_environment>` block. Read once at agent build
/// time and appended to the system prompt of every role (tool-using
/// or not). The block is plain Markdown — the model's input format
/// already accepts arbitrary text in the system role.
pub fn ground_truth_block(cwd: &Path) -> String {
    let cwd_display = cwd.display().to_string();
    let host = std::env::var("HOSTNAME")
        .or_else(|_| {
            std::fs::read_to_string("/etc/hostname")
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_default();
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!(
        r#"

<system_environment>
This block is appended at agent construction time. Values are read
from the host process right now; treat them as facts, not as goals.

  cwd       = {cwd_display}
  host      = {host}
  os        = {os}
  arch      = {arch}
  epoch_s   = {now}
</system_environment>

When the user asks a question whose answer depends on the host
process — current directory, working directory, whoami, hostname, OS,
architecture, etc. — the answer is right above in `<system_environment>`.
Read it directly; don't delegate to a specialist, don't run any tool,
don't reason from training data. The fields are guaranteed to match
the runtime that is actually serving this conversation.
"#
    )
}
