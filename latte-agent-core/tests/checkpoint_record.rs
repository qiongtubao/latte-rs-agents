//! Black-box test for `CheckpointEngine::record_write`. Runs against a
//! real temp git repo + worktree; uses a `CapturingSink` to assert that
//! the trace event for each checkpoint is emitted.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use latte_agent_core::checkpoint::CheckpointEngine;
use latte_agent_core::trace::{TraceEvent, TraceSink};
use parking_lot::Mutex;

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

#[derive(Default)]
struct CapturingSink {
    events: Mutex<Vec<TraceEvent>>,
}

impl TraceSink for CapturingSink {
    fn emit(&self, e: TraceEvent) {
        self.events.lock().push(e);
    }
}

#[test]
fn record_write_creates_checkpoint_patch_and_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    run(&repo, &["init", "-q", "-b", "main"]);
    run(&repo, &["config", "user.email", "t@l"]);
    run(&repo, &["config", "user.name", "T"]);
    std::fs::write(repo.join("seed.txt"), "seed\n").unwrap();
    run(&repo, &["add", "-A"]);
    run(&repo, &["commit", "-q", "-m", "init"]);

    // Simulate a worktree at repo/.latte/worktrees/fix-x
    let wt = repo.join(".latte/worktrees/fix-x");
    run(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "latte/fix-x",
            wt.to_str().unwrap(),
            "main",
        ],
    );
    std::fs::create_dir_all(wt.join(".latte/checkpoints/fix-x")).unwrap();
    let sink = Arc::new(CapturingSink::default());
    let mut engine = CheckpointEngine::new(
        wt.clone(),
        repo.join(".latte/checkpoints/fix-x"),
        sink.clone(),
    )
    .unwrap();

    // First write
    std::fs::write(wt.join("foo.rs"), "fn main() {}\n").unwrap();
    let cp1 = engine
        .record_write("write", &serde_json::json!({"path": "foo.rs"}))
        .unwrap();
    assert_eq!(cp1.id, 0);
    assert!(cp1.diff_path.exists());
    assert!(cp1.diff_summary.files_changed >= 1);

    // Second write
    std::fs::write(wt.join("foo.rs"), "fn main() { println!(\"hi\"); }\n").unwrap();
    let cp2 = engine
        .record_write("edit", &serde_json::json!({"path": "foo.rs"}))
        .unwrap();
    assert_eq!(cp2.id, 1);

    // Manifest
    let manifest =
        std::fs::read_to_string(repo.join(".latte/checkpoints/fix-x/manifest.json")).unwrap();
    assert_eq!(manifest.lines().count(), 2);

    // Sink received two CheckpointCreated events
    let events = sink.events.lock();
    let count = events
        .iter()
        .filter(|e| matches!(e, TraceEvent::CheckpointCreated { .. }))
        .count();
    assert_eq!(count, 2, "expected 2 CheckpointCreated, got {:?}", events.len());
}
