//! Black-box test for `WorkspaceManager::create`. Uses a real temp git repo.

use std::process::Command;
use latte_agent_core::workspace::{WorkspaceManager, WorkspaceState};

fn run(cwd: &std::path::Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(cwd).output().unwrap();
    assert!(out.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
}

#[test]
fn create_makes_worktree_branch_and_initial_commit() {
    let dir = tempfile::tempdir().unwrap();
    run(dir.path(), &["init", "-q", "-b", "main"]);
    run(dir.path(), &["config", "user.email", "test@local"]);
    run(dir.path(), &["config", "user.name", "Test"]);
    std::fs::write(dir.path().join("seed.txt"), "seed\n").unwrap();
    run(dir.path(), &["add", "-A"]);
    run(dir.path(), &["commit", "-q", "-m", "init"]);

    let mgr = WorkspaceManager::create(dir.path(), "fix-redis", "redis bug").unwrap();
    assert_eq!(mgr.spec().task_id, "fix-redis");
    assert!(matches!(mgr.state(), WorkspaceState::Created));
    assert!(mgr.blackboard().exists());

    // Worktree shows up in `git worktree list`
    let listing = Command::new("git")
        .args(["worktree", "list"])
        .current_dir(dir.path())
        .output().unwrap();
    let listing = String::from_utf8_lossy(&listing.stdout);
    assert!(listing.contains("fix-redis"), "worktree list missing task: {}", listing);

    // The worktree branch has the initial commit
    let log = Command::new("git")
        .args(["-C", mgr.spec().worktree_root.to_str().unwrap(), "log", "--oneline"])
        .output().unwrap();
    let log = String::from_utf8_lossy(&log.stdout);
    assert!(log.contains("init"), "worktree branch missing initial commit: {}", log);

    // Main branch is untouched
    let main_log = Command::new("git")
        .args(["log", "--oneline"])
        .current_dir(dir.path())
        .output().unwrap();
    let main_log = String::from_utf8_lossy(&main_log.stdout);
    assert!(!main_log.contains("checkpoint"), "main branch was contaminated: {}", main_log);
}

use std::fs;
