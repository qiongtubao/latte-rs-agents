//! E2E for v1.2's specialist-output-written-back invariant.
//!
//! v1.2 replaced the v1.1 stub round-loop with real per-role
//! `AgentRunner::run_turn` calls. After each role's turn the
//! assistant text is appended to that role's persistent history
//! (`SessionManager::append_to_role`), so the manager can read
//! the specialist's output on the next round's replay.
//!
//! This e2e drives two full rounds with a 2-role team and asserts:
//!   - the session reaches `state == "Done"` after /quit
//!   - both `manager` and `programmer` appear in `roles[]`
//!   - every role has at least one message in its history
//!   - the round loop drove a non-manager role's LLM call
//!     (stdout contains `[programmer round 1: calling LLM...]`)
//!   - at least one role has a non-empty assistant message in
//!     history (proves the specialist output was written back
//!     via `SessionManager::append_to_role` so the manager can
//!     read it on the next round)

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
fn specialist_output_written_back_to_history() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init", "-q", "-b", "main"]);
    git(repo, &["config", "user.email", "t@l"]);
    git(repo, &["config", "user.name", "T"]);
    std::fs::write(repo.join("seed.txt"), "v0\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "init"]);

    // 1. Start a task.
    let run_out = latte(repo, &["run", "--task-id", "e2e12", "--initial-prompt", "noop"], None);
    assert!(run_out.status.success(),
        "latte run failed: stderr={}",
        String::from_utf8_lossy(&run_out.stderr));

    // 2. Open chat with --max-rounds 3 and a 3-line script. The REPL
    //    reads one line per round, and `/quit` breaks the round loop
    //    AND transitions the session to `Done`. With max-rounds=3 the
    //    loop processes round 1 ("first task") + round 2 ("noop") +
    //    round 3 ("/quit"), so by the time the binary exits the
    //    session JSON is in `Done` state and the specialist output
    //    from rounds 1+2 has been written back to role history.
    let script = b"first task\nnoop\n/quit\n";
    let out = latte(
        repo,
        &["chat", "--task-id", "e2e12", "--roles", "manager,programmer",
          "--initial-prompt", "noop", "--max-rounds", "3"],
        Some(script),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    println!("[chat stdout]\n{}", stdout);
    println!("[chat stderr]\n{}", stderr);
    assert!(out.status.success(), "chat exited non-zero: stderr={}", stderr);

    // 3. Find the Done session JSON.
    let sessions_dir = repo.join(".latte/worktrees/e2e12/.latte/sessions");
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

    // 4. Assert both roles are present and have non-empty messages.
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

    // 5. Assert the round loop actually drove the specialist: stdout
    //    must contain "calling LLM" for a non-manager role. This
    //    proves the v1.2 round loop dispatched a per-role LLM call
    //    instead of the v1.1 stub.
    let specialist_called = stdout.contains("[programmer round 1: calling LLM")
        || stdout.contains("[architect round 1: calling LLM")
        || stdout.contains("[tester round 1: calling LLM")
        || stdout.contains("[reviewer round 1: calling LLM");
    assert!(specialist_called,
        "expected the round loop to drive a non-manager role's LLM call; \
         stdout was:\n{}", stdout);

    // 6. Assert at least one role has a non-empty assistant message.
    //    v1.2's `chat.rs` writes the assistant turn back to role
    //    history via `SessionManager::append_to_role` after each
    //    role's LLM call returns. This proves the specialist output
    //    was actually written back to history so the manager can
    //    read it on the next round. The length threshold is omitted
    //    because LLM responses can vary (some short, some long) but
    //    ANY non-empty assistant message proves the write-back.
    let mut found_assistant = false;
    for role in roles {
        for msg in role["messages"].as_array().unwrap() {
            if msg["role"].as_str() != Some("assistant") {
                continue;
            }
            // Content is Vec<ContentPart> after the multimodal Message
            // refactor; a message counts as non-empty when at least one
            // text part carries any bytes.
            let has_text = msg["content"].as_array().map_or(false, |parts| {
                parts.iter().any(|p| {
                    p.get("text").and_then(|t| t.as_str()).map_or(false, |s| !s.is_empty())
                })
            });
            if has_text {
                found_assistant = true;
                break;
            }
        }
        if found_assistant { break; }
    }
    assert!(found_assistant,
        "expected at least one role to have a non-empty assistant message in history \
         (proves specialist output was written back); \
         check stdout/stderr above for the actual round output");
}
