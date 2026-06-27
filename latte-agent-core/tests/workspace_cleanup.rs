use std::path::Path;
use std::process::Command;
use latte_agent_core::workspace::{WorkspaceManager, MergeMode};

fn run(cwd: &Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(cwd).output().unwrap();
    assert!(out.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
}

#[test]
fn cleanup_removes_worktree_and_branch() {
    let dir = tempfile::tempdir().unwrap();
    run(dir.path(), &["init", "-q", "-b", "main"]);
    run(dir.path(), &["config", "user.email", "t@l"]);
    run(dir.path(), &["config", "user.name", "T"]);
    std::fs::write(dir.path().join("seed.txt"), "v0\n").unwrap();
    run(dir.path(), &["add", "-A"]); run(dir.path(), &["commit", "-q", "-m", "init"]);

    let mut mgr = WorkspaceManager::create(dir.path(), "z", "go").unwrap();
    let wt = mgr.spec().worktree_root.clone();
    mgr.archive(MergeMode::NoFf).unwrap();
    mgr.cleanup().unwrap();

    assert!(!wt.exists(), "worktree dir should be gone");
    let branches = Command::new("git").args(["branch"]).current_dir(dir.path()).output().unwrap();
    let branches = String::from_utf8_lossy(&branches.stdout);
    assert!(!branches.contains("latte/z"), "branch should be gone: {}", branches);
}