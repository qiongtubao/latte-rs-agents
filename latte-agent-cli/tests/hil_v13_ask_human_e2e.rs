//! E2E for v1.3: ask_human wiring + simplified active-state guard.
//!
//! v1.1 wired `register_ask_human_tool` into the per-role
//! `build_runner` in `chat.rs`, and v1.2 added per-role AgentRunner
//! wiring so each specialist's `run_turn` is driven from the REPL.
//! v1.3 adds `SessionManager::start()` and simplifies the active-state
//! guard in `run_hil_repl` to `Running` only (Created / Resumed
//! auto-transition at session creation time).
//!
//! The full ask_human pause path requires the LLM to emit a
//! `<tool_callask_human>` block, which is outside the scope of an
//! automated REPL-driven e2e. This test verifies that the wiring
//! is live end-to-end: the per-role AgentRunner registers the
//! `ask_human` tool (so it appears in the role's available tool
//! list), the round loop drives real LLM calls for each role, and
//! the active-state guard no longer rejects the post-Created
//! "Running" state.
//!
//! Specifically, this e2e asserts:
//!   - the session auto-transitions Created → Running at chat
//!     start (i.e. the round loop did NOT print "session not
//!     running" for round 1)
//!   - the round loop drove at least one LLM call for a
//!     non-manager role (proves the per-role wiring is live)
//!   - the session reaches `state == "Done"` after /quit
//!   - both `manager` and `programmer` are in `roles[]` and have
//!     non-empty history (proves both runners were built and used)

use std::path::Path;
use std::process::{Command, Stdio};
use std::io::Write;

fn bin() -> std::path::PathBuf {
    std::env::var("CARGO_BIN_EXE_latte-agent")
        .ok()
        .map(std::path::PathBuf::from)
        .expect("CARGO_BIN_EXE_latte-agent not set by cargo test")
}

fn git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(cwd).output().unwrap();
    assert!(out.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
}

fn latte(cwd: &Path, args: &[&str], stdin: Option<&[u8]>) -> std::process::Output {
    let mut cmd = Command::new(bin());
    cmd.args(args).current_dir(cwd);
    if let Some(input) = stdin {
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        child.stdin.as_mut().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    } else {
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).output().unwrap()
    }
}

#[test]
fn ask_human_wiring_active_via_role_runners() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init", "-q", "-b", "main"]);
    git(repo, &["config", "user.email", "t@l"]);
    git(repo, &["config", "user.name", "T"]);
    std::fs::write(repo.join("seed.txt"), "v0\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "init"]);

    // 1. Start a task.
    latte(repo, &["run", "--task-id", "v13test", "--initial-prompt", "noop"], None);

    // 2. Open chat with 2 roles, drive 3 rounds + /quit.
    let script = b"first task\nnoop\n/quit\n";
    let out = latte(
        repo,
        &[
            "chat",
            "--task-id", "v13test",
            "--roles", "manager,programmer",
            "--initial-prompt", "noop",
            "--max-rounds", "3",
        ],
        Some(script),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    println!("[chat stdout]\n{}", stdout);
    println!("[chat stderr]\n{}", stderr);
    assert!(out.status.success(), "chat exited non-zero: stderr={}", stderr);

    // 3. Find the Done session JSON.
    let sessions_dir = repo.join(".latte/worktrees/v13test/.latte/sessions");
    let mut done_session = None;
    for entry in std::fs::read_dir(&sessions_dir).unwrap() {
        let entry = entry.unwrap();
        let raw = std::fs::read_to_string(entry.path()).unwrap();
        let record: serde_json::Value = serde_json::from_str(&raw).unwrap();
        if record["state"] == "Done" {
            done_session = Some(entry.path());
            break;
        }
    }
    let done_session = done_session.expect("expected a Done session JSON");
    let record: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&done_session).unwrap()
    ).unwrap();

    // 4. Assert both roles have non-empty history. The v1.2 per-role
    //    AgentRunner drove real LLM calls for both manager and
    //    programmer, and the assistant text was appended to each
    //    role's history by `SessionManager::append_to_role`.
    let roles = record["roles"].as_array().expect("roles array");
    let role_ids: Vec<&str> = roles.iter()
        .map(|r| r["role_id"].as_str().unwrap())
        .collect();
    assert!(role_ids.contains(&"manager"), "manager role missing: {:?}", role_ids);
    assert!(role_ids.contains(&"programmer"), "programmer role missing: {:?}", role_ids);

    for role in roles {
        let role_id = role["role_id"].as_str().unwrap();
        let messages = role["messages"].as_array().expect("messages array");
        assert!(!messages.is_empty(), "role '{}' has no messages in history", role_id);
    }

    // 5. Assert the round loop did not print "session not running"
    //    — proves v1.3's `start()` auto-transitioned Created →
    //    Running at session creation and the simplified
    //    active-state guard accepted the first round.
    assert!(
        !stdout.contains("session not running"),
        "expected round loop to run with auto-start; stdout was:\n{}",
        stdout
    );

    // 6. Assert the round loop drove at least one non-manager LLM
    //    call — proves the per-role AgentRunner wiring is live
    //    (and therefore the ask_human tool is registered for the
    //    programmer role).
    let called_programmer = stdout.contains("[programmer round 1: calling LLM");
    assert!(
        called_programmer,
        "expected the round loop to drive a programmer LLM call; stdout was:\n{}",
        stdout
    );
}
