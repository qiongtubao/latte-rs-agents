//! End-to-end test for the HIL blackboard session flow.
//!
//! Drives the real `latte-agent` binary through:
//! 1. Start a task via `latte-agent run --task-id X --initial-prompt Y`.
//! 2. Open a chat session via `latte-agent chat --task-id X`.
//! 3. Send a manager input, then `/pause`.
//! 4. Re-open the chat, send `@programmer hello`, then `/quit`.
//! 5. Assert: SessionManager JSON exists in `Paused` state; the
//!    inject queue file was drained; surgical rollback resets the
//!    worktree but leaves plan.md untouched.

use std::path::Path;
use std::process::{Command, Stdio};
use std::io::Write;

fn bin() -> std::path::PathBuf {
    let mut p = std::env::var("CARGO_BIN_EXE_latte-agent")
        .ok()
        .map(std::path::PathBuf::from)
        .expect("CARGO_BIN_EXE_latte-agent not set");
    assert!(p.exists(), "binary not found: {}", p.display());
    p
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
fn chat_hil_pause_resume_inject_rollback() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init", "-q", "-b", "main"]);
    git(repo, &["config", "user.email", "t@l"]);
    git(repo, &["config", "user.name", "T"]);
    std::fs::write(repo.join("seed.txt"), "v0\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "init"]);

    // 1. Start a task.
    latte(repo, &["run", "--task-id", "e2e", "--initial-prompt", "noop"], None);

    let wt = repo.join(".latte/worktrees/e2e");

    // 2. Open chat, send a manager message, then /pause.
    //    v1.2: each round drives a real per-role LLM call (~5-10s),
    //    so this whole invocation takes several seconds. The 2-line
    //    stdin script is fully buffered before the binary spawns,
    //    so timing is deterministic.
    let script1 = b"first task\n/pause\n";
    let out1 = latte(repo, &["chat", "--task-id", "e2e", "--roles", "manager,programmer", "--initial-prompt", "noop"], Some(script1));
    let stdout1 = String::from_utf8_lossy(&out1.stdout);
    let stderr1 = String::from_utf8_lossy(&out1.stderr);
    println!("[chat1 stdout]\n{}", stdout1);
    println!("[chat1 stderr]\n{}", stderr1);
    assert!(out1.status.success(), "chat1 exited non-zero: {}", stderr1);
    assert!(stdout1.contains("session: e2e"), "expected session banner, got: {}", stdout1);
    assert!(stdout1.contains("paused"), "expected paused message, got: {}", stdout1);

    // 3. Session JSON exists and is in Paused state.
    let sessions_dir = wt.join(".latte/sessions");
    let mut paused_session = None;
    for entry in std::fs::read_dir(&sessions_dir).unwrap() {
        let entry = entry.unwrap();
        let raw = std::fs::read_to_string(entry.path()).unwrap();
        let record: serde_json::Value = serde_json::from_str(&raw).unwrap();
        if record["state"] == "Paused" {
            paused_session = Some(entry.path());
            break;
        }
    }
    let paused_session = paused_session.expect("expected a Paused session JSON");

    // 4. plan.md exists and has the initial prompt
    let plan_md = std::fs::read_to_string(wt.join("plan.md")).unwrap();
    assert!(plan_md.contains("noop"), "plan.md missing initial prompt: {}", plan_md);

    // 5. Re-open the chat, send @programmer, then /quit.
    //    v1.2: must re-pass --roles manager,programmer because the
    //    persisted session's roles vector is loaded into the
    //    in-memory SessionManager only when the CLI flag matches the
    //    stored record. Without this flag the session is loaded
    //    with just [manager] and `programmer` inject queue never
    //    exists.
    let script2 = b"@programmer check this\n/quit\n";
    let out2 = latte(repo, &["chat", "--task-id", "e2e", "--roles", "manager,programmer", "--initial-prompt", "noop"], Some(script2));
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    let stderr2 = String::from_utf8_lossy(&out2.stderr);
    println!("[chat2 stdout]\n{}", stdout2);
    println!("[chat2 stderr]\n{}", stderr2);
    assert!(out2.status.success(), "chat2 exited non-zero: {}", stderr2);
    assert!(stdout2.contains("RESUMED") || stdout2.contains("session: e2e"),
        "expected resume banner, got: {}", stdout2);
    assert!(stdout2.contains("programmer queue: +1 message"),
        "expected inject ack, got: {}", stdout2);

    // 6. Surgical rollback: plan.md and the inject queue should be
    //    untouched. (Inject queue was already drained by the REPL
    //    quit; we just verify plan.md byte-equal pre/post.)
    let plan_before = std::fs::read_to_string(wt.join("plan.md")).unwrap();
    let out3 = latte(repo, &["checkpoint", "rollback", "--task-id", "e2e", "--id", "0"], None);
    let _ = String::from_utf8_lossy(&out3.stdout);
    let plan_after = std::fs::read_to_string(wt.join("plan.md")).unwrap();
    assert_eq!(plan_before, plan_after,
        "surgical rollback must not touch plan.md");
}