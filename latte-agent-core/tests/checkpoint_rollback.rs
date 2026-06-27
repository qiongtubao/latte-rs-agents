//! Black-box tests for `CheckpointEngine::rollback` covering the
//! `Code` and `Full` rollback modes. Runs against a real temp git
//! repo + worktree (no mocks).

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use latte_agent_core::checkpoint::{CheckpointEngine, RollbackMode};
use latte_agent_core::trace::TraceSink;

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

fn setup() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    run(&repo, &["init", "-q", "-b", "main"]);
    run(&repo, &["config", "user.email", "t@l"]);
    run(&repo, &["config", "user.name", "T"]);
    std::fs::write(repo.join("seed.txt"), "v0\n").unwrap();
    run(&repo, &["add", "-A"]);
    run(&repo, &["commit", "-q", "-m", "init"]);

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

    let storage = repo.join(".latte/checkpoints/fix-x");
    std::fs::create_dir_all(&storage).unwrap();
    (dir, repo, wt)
}

#[derive(Clone)]
struct NullSink;
impl TraceSink for NullSink {
    fn emit(&self, _e: latte_agent_core::trace::TraceEvent) {}
}

#[test]
fn rollback_code_resets_worktree() {
    let (_dir, repo, wt) = setup();
    let mut engine = CheckpointEngine::new(
        wt.clone(),
        repo.join(".latte/checkpoints/fix-x"),
        Arc::new(NullSink),
    )
    .unwrap();

    // cp0: clean (empty diff)
    let _ = engine
        .record_write("init", &serde_json::json!({}))
        .unwrap();
    // cp1: add a line
    std::fs::write(wt.join("foo.rs"), "v1\n").unwrap();
    let cp1 = engine
        .record_write("write", &serde_json::json!({}))
        .unwrap();
    // cp2: change again
    std::fs::write(wt.join("foo.rs"), "v2\n").unwrap();
    let _cp2 = engine
        .record_write("write", &serde_json::json!({}))
        .unwrap();

    // Roll back to cp1 — foo.rs should be "v1"
    engine.rollback(cp1.id, RollbackMode::Code).unwrap();
    assert_eq!(
        std::fs::read_to_string(wt.join("foo.rs")).unwrap(),
        "v1\n"
    );
}

#[test]
fn rollback_full_truncates_trace_jsonl() {
    let (dir, repo, wt) = setup();
    // Set LATTE_TRACE_FILE to a temp file we control
    let trace_path = dir.path().join("trace.jsonl");
    std::fs::write(&trace_path, b"").unwrap();
    std::env::set_var("LATTE_TRACE_FILE", &trace_path);

    let mut engine = CheckpointEngine::new(
        wt.clone(),
        repo.join(".latte/checkpoints/fix-x"),
        Arc::new(NullSink),
    )
    .unwrap();
    // cp0: empty trace
    let cp0 = engine
        .record_write("init", &serde_json::json!({}))
        .unwrap();
    assert_eq!(cp0.trace_event_index, 0);
    // Append to the trace "file" then cp1: offset should reflect new length
    std::fs::write(&trace_path, b"line-A\nline-B\nline-C\nline-D\n").unwrap();
    let cp1 = engine
        .record_write("write", &serde_json::json!({}))
        .unwrap();
    let offset_at_cp1 = cp1.trace_event_index;
    assert!(offset_at_cp1 > 0);

    // Roll back to cp1 with full mode
    engine.rollback(cp1.id, RollbackMode::Full).unwrap();
    let len_after = std::fs::metadata(&trace_path).unwrap().len();
    assert_eq!(len_after, offset_at_cp1);

    std::env::remove_var("LATTE_TRACE_FILE");
}