//! End-to-end test for v1.1 round-robin scheduler.

use std::path::Path;
use std::process::{Command, Stdio};
use std::io::Write;

mod common;
use common::{start_model_mock, write_isolated_home};

fn bin() -> std::path::PathBuf {
    std::env::var("CARGO_BIN_EXE_latte-agent")
        .ok()
        .map(std::path::PathBuf::from)
        .expect("CARGO_BIN_EXE_latte-agent not set")
}

fn git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(cwd).output().unwrap();
    assert!(out.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
}

fn latte(cwd: &Path, args: &[&str], stdin: Option<&[u8]>) -> std::process::Output {
    latte_with_home(cwd, args, stdin, None)
}

fn latte_with_home(
    cwd: &Path,
    args: &[&str],
    stdin: Option<&[u8]>,
    home: Option<&Path>,
) -> std::process::Output {
    let mut cmd = Command::new(bin());
    cmd.args(args).current_dir(cwd);
    if let Some(h) = home {
        // 隔离全局配置层，否则会 fallback 到真实 API（见 `start_mock`）。
        cmd.env("LATTE_HOME", h);
    }
    if let Some(input) = stdin {
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        child.stdin.as_mut().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    } else {
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).output().unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn round_robin_invokes_each_role_in_order() {
    // mock + 隔离 HOME：这个测试测的是 round-robin 调度，不是模型行为
    // （见 `start_mock` 的说明）。
    let (_server, base_url) = start_model_mock("stub 产出：本轮无操作。").await;
    let home = tempfile::tempdir().unwrap();
    write_isolated_home(home.path(), &base_url);
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init", "-q", "-b", "main"]);
    git(repo, &["config", "user.email", "t@l"]);
    git(repo, &["config", "user.name", "T"]);
    std::fs::write(repo.join("seed.txt"), "v0\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "init"]);

    // 1. Start a task
    latte_with_home(
        repo,
        &["run", "--task-id", "e2e11", "--initial-prompt", "noop"],
        None,
        Some(home.path()),
    );

    // 2. Open chat with --max-rounds 3 and a 3-line script. The REPL
    //    reads one line per round; `/quit` in round N breaks before
    //    the role loop runs, so to get 2 full rounds of stubs the
    //    script needs 2 manager-input lines + a trailing `/quit`.
    let script = b"first task\nnoop\n/quit\n";
    let out = latte_with_home(
        repo,
        &["chat", "--task-id", "e2e11", "--roles", "manager,programmer,reviewer",
          "--initial-prompt", "noop", "--max-rounds", "3"],
        Some(script),
        Some(home.path()),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    println!("[chat stdout]\n{}", stdout);
    println!("[chat stderr]\n{}", String::from_utf8_lossy(&out.stderr));

    // 3. Assert: each role's stub appears in 2 rounds (total 6 stubs).
    let programmer_count = stdout.matches("programmer round").count();
    let reviewer_count = stdout.matches("reviewer round").count();
    let manager_count = stdout.matches("manager round").count();
    assert!(programmer_count >= 2, "programmer round count: {}", programmer_count);
    assert!(reviewer_count >= 2, "reviewer round count: {}", reviewer_count);
    assert!(manager_count >= 2, "manager round count: {}", manager_count);

    // 4. Order: within a round, manager is last.
    let first_programmer = stdout.find("programmer round").unwrap();
    let first_reviewer = stdout.find("reviewer round").unwrap();
    let first_manager = stdout.find("manager round").unwrap();
    assert!(first_programmer < first_reviewer, "programmer should come before reviewer");
    assert!(first_reviewer < first_manager, "reviewer should come before manager");

    // 5. Session JSON is in Done state.
    let sessions_dir = repo.join(".latte/worktrees/e2e11/.latte/sessions");
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
    assert!(done_session.is_some(), "expected a Done session JSON");
}
