//! End-to-end test: run a 2-checkpoint synthetic task via the
//! `latte-agent` binary, then rollback --id 1, and assert the
//! worktree is back to the post-Checkpoint-1 state.

use std::path::Path;
use std::process::Command;

fn bin() -> std::path::PathBuf {
    let mut p = std::env::var("CARGO_BIN_EXE_latte-agent")
        .ok()
        .map(std::path::PathBuf::from)
        .expect("CARGO_BIN_EXE_latte-agent not set by cargo test");
    assert!(p.exists(), "binary not found: {}", p.display());
    p
}

fn git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(cwd).output().unwrap();
    assert!(out.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
}

fn latte(cwd: &Path, args: &[&str]) {
    let out = Command::new(bin()).args(args).current_dir(cwd).output().unwrap();
    assert!(out.status.success(), "latte-agent {:?} failed: {}",
        args, String::from_utf8_lossy(&out.stderr));
}

#[test]
fn run_then_rollback_full_returns_worktree_to_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init", "-q", "-b", "main"]);
    git(repo, &["config", "user.email", "t@l"]);
    git(repo, &["config", "user.name", "T"]);
    std::fs::write(repo.join("main.txt"), "original\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "init"]);

    // 1. Start a task
    latte(repo, &["run", "--task-id", "e2e", "--initial-prompt", "noop"]);

    // 2. Two writes inside the worktree (simulating two tool calls),
    //    each followed by an explicit checkpoint
    let wt = repo.join(".latte/worktrees/e2e");
    std::fs::write(wt.join("main.txt"), "v1\n").unwrap();
    latte(repo, &["checkpoint", "create", "--task-id", "e2e"]);
    std::fs::write(wt.join("main.txt"), "v2\n").unwrap();
    latte(repo, &["checkpoint", "create", "--task-id", "e2e"]);

    // 3. List shows at least 2 checkpoints
    let out = Command::new(bin())
        .args(["checkpoint", "list", "--task-id", "e2e"])
        .current_dir(repo)
        .output().unwrap();
    let list = String::from_utf8_lossy(&out.stdout);
    let n = list.lines().filter(|l| l.starts_with('#')).count();
    assert!(n >= 2, "expected at least 2 checkpoints, got {}:\n{}", n, list);

    // 4. Roll back to id 1 (first user-created checkpoint). Worktree's
    //    main.txt should be "v1".
    let out = Command::new(bin())
        .args(["checkpoint", "rollback", "--task-id", "e2e", "--id", "1"])
        .current_dir(repo)
        .output().unwrap();
    assert!(out.status.success(), "rollback failed: {}",
        String::from_utf8_lossy(&out.stderr));
    assert_eq!(std::fs::read_to_string(wt.join("main.txt")).unwrap(), "v1\n");

    // 5. Main branch stays clean
    let main_log = Command::new("git").args(["log", "--oneline"])
        .current_dir(repo).output().unwrap();
    let main_log = String::from_utf8_lossy(&main_log.stdout);
    assert!(!main_log.contains("checkpoint("), "main branch contaminated:\n{}", main_log);
}
