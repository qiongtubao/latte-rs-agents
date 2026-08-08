//! E2E for v1.3a: ask_human tool actually pauses the session.
//!
//! Spins up a local HTTP server that simulates an OpenAI-compatible
//! LLM endpoint. The server returns a canned `chat/completions`
//! response whose assistant message contains a
//! structured `tool_calls` response. The
//! binary parses the tool call, invokes the `ask_human` tool
//! closure, the closure calls `SessionManager::pause_with_reason`,
//! and the REPL breaks the round. The test asserts the on-disk
//! `SessionRecord` JSON is in `Paused` state with the question in
//! `pause_reason`.
//!
//! Uses the latte-rs-agents-v13 worktree root as the cwd so .latte/ +
//! prompts/ + the source tree are available without copying them into
//! a tempdir. The mock model is declared in
//! `.latte/models.d/mock.toml` (the config layer reads that directory
//! by default).
//!
//! `ask_human` is only registered for non-manager roles (see
//! `chat.rs:1669-1673`). The test uses `--roles programmer`
//! (manager excluded) so the round loop's only LLM call is the
//! programmer's, where `ask_human` is available.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::io::Write;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn bin() -> PathBuf {
    std::env::var("CARGO_BIN_EXE_latte-agent")
        .ok()
        .map(PathBuf::from)
        .expect("CARGO_BIN_EXE_latte-agent not set by cargo test")
}

/// Mock OpenAI chat/completions server. Returns a canned response
/// with a structured `tool_calls` (ask_human) body on the first request. Stops
/// after one response.
struct MockOpenAIServer {
    addr: SocketAddr,
    captured_request: Arc<Mutex<Option<serde_json::Value>>>,
    shutdown: Arc<Mutex<bool>>,
}

impl MockOpenAIServer {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = Arc::new(Mutex::new(None));
        let shutdown = Arc::new(Mutex::new(false));
        let captured_clone = captured.clone();
        let shutdown_clone = shutdown.clone();
        let question_text = "should I refactor the DB connection first or write the migration?";
        tokio::spawn(async move {
            loop {
                if *shutdown_clone.lock().unwrap() { break; }
                let (mut socket, _) = match listener.accept().await {
                    Ok(c) => c,
                    Err(_) => break,
                };
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 65536];
                let read_result = tokio::time::timeout(
                    Duration::from_secs(2),
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
                        *captured_clone.lock().unwrap() = Some(v);
                    }
                }
                let body = serde_json::json!({
                    "id": "chatcmpl-mock-1",
                    "object": "chat.completion",
                    "created": 0,
                    "model": "mock",
                    "choices": [{
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": "I need to ask the human a question before proceeding.",
                            "tool_calls": [{
                                "id": "call_ask_human_1",
                                "type": "function",
                                "function": {
                                    "name": "ask_human",
                                    "arguments": format!("{{\"question\": \"{}\"}}", question_text),
                                },
                            }],
                        },
                        "finish_reason": "tool_calls",
                    }],
                    "usage": {
                        "prompt_tokens": 100,
                        "completion_tokens": 50,
                        "total_tokens": 150,
                    },
                });
                let body_str = serde_json::to_string(&body).unwrap();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body_str.len(),
                    body_str,
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
                break;
            }
        });
        Self { addr, captured_request: captured, shutdown }
    }

    fn api_base(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for MockOpenAIServer {
    fn drop(&mut self) {
        *self.shutdown.lock().unwrap() = true;
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
async fn ask_human_actually_pauses_session_via_real_llm() {
    // 1. Start the mock LLM server.
    let server = MockOpenAIServer::start().await;
    let api_base = server.api_base();

    // Use a unique task_id per test run so re-runs (which
    // leave a worktree behind if the binary crashes) don't
    // collide with the previous run's worktree.
    let task_id = format!("askhuman-test-{}", std::process::id());

    // 2. Use the worktree root as cwd. .latte/ + prompts/ + source
    //    tree are all present here, so the binary can resolve
    //    manager + programmer roles + their prompt files.
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap()
        .to_path_buf();

    // 3. Write the mock model declaration.
    write_mock_toml(&repo, &api_base);

    // 4. Build the model-override arg (sets base_url to our mock).
    let model_override = format!("mock.base_url={}", api_base);

    // 5. Run `latte-agent run` to create the worktree.
    let run_out = Command::new(bin())
        .arg("run")
        .arg("--task-id").arg(&task_id)
        .arg("--initial-prompt").arg("noop")
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(run_out.status.success(),
        "run failed: stderr={}", String::from_utf8_lossy(&run_out.stderr));

    // 6. Spawn chat with stdin that drives 1 round. Use --roles
    //    programmer (not manager) because ask_human is only
    //    registered for non-manager roles (chat.rs:1669-1673).
    let mut chat_cmd = Command::new(bin());
    // 7b. Spawn chat with stdin that drives 1 round. Use --roles
    //     programmer (not manager) because ask_human is only
    //     registered for non-manager roles (chat.rs:1669-1673).
    //     Use a unique task_id per test run so re-runs (which
    //     leave a worktree behind if the binary crashes) don't
    //     collide with the previous run's worktree.
    let task_id = format!("askhuman-test-{}", std::process::id());
    let mut chat_cmd = Command::new(bin());
    chat_cmd.arg("chat")
        .arg("--task-id").arg(&task_id)
        .arg("--roles").arg("manager,programmer")
        .arg("--initial-prompt").arg("noop")
        .arg("--max-rounds").arg("2")
        .arg("--model-id").arg("mock")
        .arg("--api-key").arg("mock-key")
        .arg("--model-override").arg(&model_override)
        .current_dir(&repo);
    chat_cmd.stdin(Stdio::piped());
    chat_cmd.stdout(Stdio::piped());
    chat_cmd.stderr(Stdio::piped());
    let mut child = chat_cmd.spawn().unwrap();
    child.stdin.as_mut().unwrap().write_all(b"hello\n").unwrap();
    drop(child.stdin.take());
    let chat_out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&chat_out.stdout);
    let stderr = String::from_utf8_lossy(&chat_out.stderr);
    println!("[chat stdout]\n{}", stdout);
    println!("[chat stderr]\n{}", stderr);

    // 7. Read the session JSON. Assert state == Paused and
    //    pause_reason contains the ask_human question.
    let sessions_dir = repo.join(&format!(".latte/worktrees/{}/.latte/sessions", task_id));
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
    let paused_session = paused_session.expect(
        "expected a Paused session JSON — ask_human tool should have paused the session"
    );
    let record: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&paused_session).unwrap()
    ).unwrap();
    let reason = record["pause_reason"].as_str().expect("pause_reason should be a string");
    assert!(
        reason.contains("ask_human"),
        "pause_reason should mention ask_human, got: {}", reason
    );
    assert!(
        reason.contains("programmer asked"), // manager is in --roles just to satisfy the REPL dispatch,
        "pause_reason should include the role, got: {}", reason
    );
    assert!(
        reason.contains("should I refactor the DB connection"),
        "pause_reason should include the question text, got: {}", reason
    );

    // 8. Sanity: the mock server captured the LLM request with
    //    a "messages" array.
    let captured = server.captured_request.lock().unwrap();
    let req = captured.as_ref().expect("mock server should have captured the request");
    let messages = req["messages"].as_array().expect("messages array");
    assert!(!messages.is_empty(), "LLM request had no messages");
}
