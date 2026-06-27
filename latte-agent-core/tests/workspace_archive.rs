use std::path::Path;
use std::process::Command;
use latte_agent_core::workspace::{MergeMode, WorkspaceManager, WorkspaceState};

fn run(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn archive_with_no_ff_creates_merge_commit_on_main() {
    let dir = tempfile::tempdir().unwrap();
    run(dir.path(), &["init", "-q", "-b", "main"]);
    run(dir.path(), &["config", "user.email", "t@l"]);
    run(dir.path(), &["config", "user.name", "T"]);
    std::fs::write(dir.path().join("seed.txt"), "v0\n").unwrap();
    run(dir.path(), &["add", "-A"]);
    run(dir.path(), &["commit", "-q", "-m", "init"]);

    let mut mgr = WorkspaceManager::create(dir.path(), "x", "do thing").unwrap();
    // Make a change in the worktree + commit
    std::fs::write(mgr.spec().worktree_root.join("seed.txt"), "v1\n").unwrap();
    run(&mgr.spec().worktree_root, &["add", "-A"]);
    run(&mgr.spec().worktree_root, &["commit", "-q", "-m", "work"]);

    let sha = mgr.archive(MergeMode::NoFf).unwrap();
    assert!(!sha.is_empty());
    assert!(matches!(mgr.state(), WorkspaceState::Archived { .. }));

    // Main branch log shows the merge commit
    let log = Command::new("git")
        .args(["log", "--oneline", "--graph"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&log.stdout);
    assert!(
        log.contains("archive(x)"),
        "main log missing merge commit:\n{}",
        log
    );
}

#[test]
fn archive_with_conflict_transitions_to_failed() {
    let dir = tempfile::tempdir().unwrap();
    run(dir.path(), &["init", "-q", "-b", "main"]);
    run(dir.path(), &["config", "user.email", "t@l"]);
    run(dir.path(), &["config", "user.name", "T"]);
    std::fs::write(dir.path().join("seed.txt"), "main\n").unwrap();
    run(dir.path(), &["add", "-A"]);
    run(dir.path(), &["commit", "-q", "-m", "init"]);

    let mut mgr = WorkspaceManager::create(dir.path(), "y", "conflict case").unwrap();
    // Worktree changes seed.txt to "work\n"
    std::fs::write(mgr.spec().worktree_root.join("seed.txt"), "work\n").unwrap();
    run(&mgr.spec().worktree_root, &["add", "-A"]);
    run(&mgr.spec().worktree_root, &["commit", "-q", "-m", "work"]);

    // Now change main's seed.txt so a merge will conflict
    std::fs::write(dir.path().join("seed.txt"), "main2\n").unwrap();
    run(dir.path(), &["add", "-A"]);
    run(dir.path(), &["commit", "-q", "-m", "main change"]);

    let res = mgr.archive(MergeMode::NoFf);
    assert!(res.is_err());
    assert!(matches!(mgr.state(), WorkspaceState::Failed { .. }));

    // Worktree dir is preserved
    assert!(
        mgr.spec().worktree_root.exists(),
        "worktree should be preserved on failure"
    );
}