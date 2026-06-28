//! E2E for v1.1 spec §12 item 10: all 3 new `TraceEvent` variants
//! (`AskHuman`, `RoundStarted`, `RoundEnded`) appear in
//! `~/.latte/traces/<id>.jsonl` after a round-robin session.
//!
//! Spins up a local HTTP server that simulates an OpenAI-compatible
//! LLM endpoint. The server returns a canned
//! `<tool_callask_human>{"question": "..."}</tool_call></tool_callask_human>`
//! body on the first call (so the role's LLM response fires the
//! `ask_human` tool, which emits a `TraceEvent::AskHuman` and pauses
//! the session) and a plain text body on the second call (so the
//! resumed role's LLM response finishes the round and emits
//! `TraceEvent::RoundEnded` for that round).
//!
//! The REPL script drives 2 rounds: round 1 emits `RoundStarted` +
//! `AskHuman` (paused mid-round); round 2 reads the human's reply
//! from stdin, resumes the session, the role's LLM call returns plain
//! text, and the round loop emits `RoundEnded` for round 1 + the
//! second round's `RoundStarted` + `RoundEnded`. The script ends
//! with `/quit` which transitions the session to `Done`.
//!
//! Verifies:
//!   - the trace JSONL at `~/.latte/traces/hil-<task_id>.jsonl`
//!     contains at least one `RoundStarted` event,
//!   - at least one `RoundEnded` event,
//!   - at least one `AskHuman` event.
//!
//! The chat command emits a `TraceEvent` per round start (before the
//! role loop), and one at round end (after the role loop). The
//! `ask_human` tool's closure (chat.rs:2095) emits `AskHuman` before
//! the session auto-pauses. The trace JSONL is the union of all
//! `JsonlSink` emissions across both rounds.

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

fn bin() -> PathBuf {
    std::env::var("CARGO_BIN_EXE_latte-agent")
        .ok()
        .map(PathBuf::from)
        .expect("CARGO_BIN_EXE_latte-agent not set by cargo test")
}

/// Mock OpenAI chat/completions server. Returns a canned
/// `<tool_callask_human>` body on the first request and a plain text
/// body on the second request. Stops after the configured number of
/// responses.
struct MockOpenAIServer {
    addr: SocketAddr,
    captured_request: Arc<Mutex<Option<serde_json::Value>>>,
    shutdown: Arc<Mutex<bool>>,
    /// How many requests the mock should respond to before closing.
    remaining: Arc<Mutex<u32>>,
}

impl MockOpenAIServer {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = Arc::new(Mutex::new(None));
        let shutdown = Arc::new(Mutex::new(false));
        let remaining = Arc::new(Mutex::new(2));
        let captured_clone = captured.clone();
        let shutdown_clone = shutdown.clone();
        let remaining_clone = remaining.clone();
        let question_text = "should I refactor the DB connection first or write the migration?";
        tokio::spawn(async move {
            loop {
                if *shutdown_clone.lock() { break; }
                let (mut socket, _) = match listener.accept().await {
                    Ok(c) => c,
                    Err(_) => break,
                };
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 65536];
                let read_result = tokio::time::timeout(
                    Duration::from_secs(5),
                    socket.read(&mut buf),
                ).await;
                let n = match read_result {
                    Ok(Ok(n)) => n,
                    _ => continue,
                };
                if n == 0 { continue; }
                let req_str = String::from_utf8_lossy(&buf[..n]);
                if let Some(idx) = req_str.find("\r\n\r\n") {
                    let body = &req_str[idx + 4..];
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
                        *captured_clone.lock() = Some(v);
                    }
                }
                // First call (remaining goes 2 -> 1): emit ask_human.
                // Second call (remaining goes 1 -> 0): emit plain text.
                let is_ask_human_call = {
                    let mut r = remaining_clone.lock();
                    *r -= 1;
                    *r == 1
                };
                let body = if is_ask_human_call {
                    serde_json::json!({
                        "id": "chatcmpl-mock-1",
                        "object": "chat.completion",
                        "created": 0,
                        "model": "mock",
                        "choices": [{
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": format!(
                                    "I need to ask the human a question before proceeding.\n<tool_callask_human> {{\"question\": \"{}\"}}</tool_callask_human>",
                                    question_text
                                ),
                            },
                            "finish_reason": "stop",
                        }],
                        "usage": {"prompt_tokens": 100, "completion_tokens": 50, "total_tokens": 150},
                    })
                } else {
                    serde_json::json!({
                        "id": "chatcmpl-mock-2",
                        "object": "chat.completion",
                        "created": 1,
                        "model": "mock",
                        "choices": [{
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": "Thanks, I'll refactor the DB connection first.",
                            },
                            "finish_reason": "stop",
                        }],
                        "usage": {"prompt_tokens": 100, "completion_tokens": 30, "total_tokens": 130},
                    })
                };
                let body_str = serde_json::to_string(&body).unwrap();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body_str.len(),
                    body_str,
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
                if *remaining_clone.lock() == 0 {
                    break;
                }
            }
        });
        Self { addr, captured_request: captured, shutdown, remaining }
    }

    fn api_base(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for MockOpenAIServer {
    fn drop(&mut self) {
        *self.shutdown.lock() = true;
    }
}

/// Write the mock model declaration into the worktree's
/// `.latte/models.d/` so the config layer can resolve the model id
/// "mock" before --model-override targets it.
fn write_mock_toml(repo: &Path, api_base: &str) {
    let mock_toml = format!(r#"
[[models.models]]
id = "mock"
name = "Mock"
api = "openai"
provider = "mock"
base_url = "{api_base}"
api_key = "PLACEHOLDER"
context_window = 8192
max_tokens = 4096

[models.tiers]
premium = "mock"
standard = "mock"
budget = "mock"
"#);
    let dir = repo.join(".latte").join("models.d");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("mock.toml"), mock_toml).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn v11_trace_events_round_started_round_ended_ask_human_appear_in_jsonl() {
    // 1. Start the mock LLM server.
    let server = MockOpenAIServer::start().await;
    let api_base = server.api_base();

    // 2. Use the worktree root as cwd. .latte/ + prompts/ + source
    //    tree are all present here, so the binary can resolve
    //    manager + programmer roles + their prompt files.
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap()
        .to_path_buf();

    // 3. Use a unique task_id per test run so re-runs (which
    //    leave a worktree behind if the binary crashes) don't
    //    collide with the previous run's worktree.
    let task_id = format!("v11trace-{}", std::process::id());

    // 4. Write the mock model declaration.
    write_mock_toml(&repo, &api_base);

    // 5. Build the model-override arg (sets base_url to our mock).
    let model_override = format!("mock.base_url={}", api_base);

    // 6. Run `latte-agent run` to create the worktree.
    let run_out = Command::new(bin())
        .arg("run")
        .arg("--task-id").arg(&task_id)
        .arg("--initial-prompt").arg("noop")
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(run_out.status.success(),
        "run failed: stderr={}", String::from_utf8_lossy(&run_out.stderr));

    // 7. Spawn chat with stdin that drives the full 2-round cycle.
    //    --roles manager,programmer: round order sorts then moves
    //    manager to the end, so the per-round role loop runs
    //    [programmer, manager].
    //    --max-rounds 2:
    //      round 1: top read "first task" -> programmer LLM call 1
    //               (ask_human) -> session Paused -> continue 'rounds
    //      round 2: Paused check reads "yes, refactor first" from
    //               stdin -> resume_with_message(programmer, reply)
    //               -> state Resumed -> start() -> state Running
    //               -> /quit reads from stdin -> mark_done -> break.
    //    --debug: required to make the chat command emit the full
    //             JSONL trace to `<LATTE_HOME>/traces/<session_id>.jsonl`
    //             (without it, only the index is written).
    let script = b"first task\nyes, refactor first\n/quit\n";
    let mut chat_cmd = Command::new(bin());
    chat_cmd.arg("chat")
        .arg("--task-id").arg(&task_id)
        .arg("--roles").arg("manager,programmer")
        .arg("--initial-prompt").arg("noop")
        .arg("--max-rounds").arg("2")
        .arg("--debug")
        .arg("--model-id").arg("mock")
        .arg("--api-key").arg("mock-key")
        .arg("--model-override").arg(&model_override)
        .current_dir(&repo);
    chat_cmd.stdin(Stdio::piped());
    chat_cmd.stdout(Stdio::piped());
    chat_cmd.stderr(Stdio::piped());
    let mut child = chat_cmd.spawn().unwrap();
    child.stdin.as_mut().unwrap().write_all(script).unwrap();
    drop(child.stdin.take());
    let chat_out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&chat_out.stdout);
    let stderr = String::from_utf8_lossy(&chat_out.stderr);
    println!("[chat stdout]\n{}", stdout);
    println!("[chat stderr]\n{}", stderr);

    // 8. Locate the trace JSONL. The chat command uses
    //    `session_id = format!("hil-{}", task_id)` for the JSONL
    //    trace (see chat.rs:1640 + mod.rs:44). The trace file lives
    //    under the `latte_home()` directory (see
    //    `trace_store::latte_home`), which resolves to:
    //      1. `$LATTE_HOME` env var if set and non-empty
    //      2. `<cwd>/.latte` if cwd contains a `.latte` directory
    //         (the worktree root does — `latte-agent run` created it)
    //      3. `$HOME/.latte` as a final fallback
    //    We probe each location in order to find the trace file the
    //    binary actually wrote.
    let trace_filename = format!("hil-{}.jsonl", task_id);
    let candidates: Vec<PathBuf> = {
        let mut v = Vec::new();
        if let Ok(p) = std::env::var("LATTE_HOME") {
            if !p.is_empty() {
                v.push(PathBuf::from(p).join("traces").join(&trace_filename));
            }
        }
        v.push(repo.join(".latte").join("traces").join(&trace_filename));
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            v.push(home.join(".latte").join("traces").join(&trace_filename));
        }
        v
    };
    let trace_path = candidates
        .iter()
        .find(|p| p.exists())
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "trace file not found at any of: {:?}; stderr={}",
                candidates, stderr
            )
        });
    let raw_trace = std::fs::read_to_string(&trace_path).unwrap_or_else(|e| {
        panic!("read {}: {}", trace_path.display(), e)
    });

    // 9. Parse each line as JSON. Each line is a `TraceEvent` whose
    //    top-level structure is `{ "<VariantName>": { ... } }`. Count
    //    occurrences of the three new v1.1 variants.
    let mut round_started_count = 0usize;
    let mut round_ended_count = 0usize;
    let mut ask_human_count = 0usize;
    for (i, line) in raw_trace.lines().enumerate() {
        if line.trim().is_empty() { continue; }
        let v: serde_json::Value = serde_json::from_str(line).unwrap_or_else(|e| {
            panic!("parse {}:{} — {}: {}", trace_path.display(), i + 1, e, line)
        });
        let obj = v.as_object().expect("event must be a JSON object");
        for variant in obj.keys() {
            match variant.as_str() {
                "RoundStarted" => round_started_count += 1,
                "RoundEnded" => round_ended_count += 1,
                "AskHuman" => ask_human_count += 1,
                _ => {}
            }
        }
    }

    // 10. Assert at least one of each new variant. The chat command
    //     emits `RoundStarted` once at the top of each round, and
    //     `RoundEnded` once at the bottom of each round (and once
    //     per supervisor pause mid-round). `AskHuman` fires when the
    //     registered tool is called and the session auto-pauses.
    assert!(
        round_started_count >= 1,
        "expected at least 1 RoundStarted event in trace, got {}. \
         Trace file: {}\n---\n{}\n---",
        round_started_count, trace_path.display(), raw_trace
    );
    assert!(
        round_ended_count >= 1,
        "expected at least 1 RoundEnded event in trace, got {}. \
         Trace file: {}\n---\n{}\n---",
        round_ended_count, trace_path.display(), raw_trace
    );
    assert!(
        ask_human_count >= 1,
        "expected at least 1 AskHuman event in trace, got {}. \
         Trace file: {}\n---\n{}\n---",
        ask_human_count, trace_path.display(), raw_trace
    );

    // 11. Sanity: the mock server captured the LLM request with
    //     a "messages" array. Proves the role's LLM call actually
    //     reached the mock and the ask_human response was
    //     processed.
    let captured = server.captured_request.lock();
    let req = captured.as_ref().expect("mock server should have captured the request");
    let messages = req["messages"].as_array().expect("messages array");
    assert!(!messages.is_empty(), "LLM request had no messages");
}
