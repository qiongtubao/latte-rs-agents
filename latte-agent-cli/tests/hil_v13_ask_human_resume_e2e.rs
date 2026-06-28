//! E2E for v1.3a full flow: ask_human pauses, the REPL surfaces the
//! question to the human, the human's reply resumes the session, and
//! `resume_with_message` appends a `[HUMAN @ ts]\nreply` message to
//! the role's history. The session then transitions to Done via
//! /quit.
//!
//! Drives the REPL through a complete pause + resume + quit cycle via
//! an HTTP mock LLM endpoint. Uses the same HTTP-mock pattern as
//! `hil_v13_ask_human_mock_e2e` but extends the script to:
//!   1. round 1: programmer's LLM call returns ask_human -> session Paused
//!   2. round 2: Paused check reads "yes, refactor first" from stdin,
//!      calls resume_with_message(programmer, reply), transitions to Running
//!   3. round 2: /quit reads from stdin -> mark_done -> session Done
//!
//! The mock handles up to 2 LLM calls. With `--max-rounds 2`, only 1
//! call actually happens (round 1's ask_human); the /quit in round 2
//! fires before the role loop runs. The second response is just a
//! safety net in case the round order changes.

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

/// Mock OpenAI chat/completions server. Returns a canned
/// `<tool_callask_human>` body on the first request and a plain text
/// body on subsequent requests. Stops after the configured number of
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
                if *shutdown_clone.lock().unwrap() { break; }
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
                        *captured_clone.lock().unwrap() = Some(v);
                    }
                }
                // First call (remaining goes 2 -> 1): emit ask_human.
                // Second call (remaining goes 1 -> 0): emit plain text.
                let is_ask_human_call = {
                    let mut r = remaining_clone.lock().unwrap();
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
                if *remaining_clone.lock().unwrap() == 0 {
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
async fn ask_human_full_pause_resume_done_cycle() {
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
    let task_id = format!("askhuman-resume-{}", std::process::id());

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

    // 7. Spawn chat with stdin that drives the full pause+resume+quit
    //    cycle. --roles manager,programmer: round order sorts then
    //    moves manager to the end, so [programmer, manager].
    //    --max-rounds 2:
    //      round 1: top read "first task" -> programmer LLM call 1
    //               (ask_human) -> session Paused -> continue 'rounds
    //      round 2: Paused check reads "yes, refactor first" from
    //               stdin -> resume_with_message(programmer, reply)
    //               -> state Resumed -> start() -> state Running
    //               -> falls through to top read_line which reads
    //               "/quit" -> mark_done -> break.
    let script = b"first task\nyes, refactor first\n/quit\n";
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
    child.stdin.as_mut().unwrap().write_all(script).unwrap();
    drop(child.stdin.take());
    let chat_out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&chat_out.stdout);
    let stderr = String::from_utf8_lossy(&chat_out.stderr);
    println!("[chat stdout]\n{}", stdout);
    println!("[chat stderr]\n{}", stderr);

    // 8. Read the final session JSON. Assert state == Done.
    let sessions_dir = repo.join(format!(".latte/worktrees/{}/.latte/sessions", task_id));
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
    let done_session = done_session.expect(
        "expected a Done session JSON after the full pause+resume+quit cycle"
    );
    let record: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&done_session).unwrap()
    ).unwrap();

    // 9. Assert the programmer role has the human's reply in its history
    //    (proves resume_with_message worked). The synthetic message
    //    format from session.rs is: "[HUMAN @ <ts>]\n<reply>".
    let roles = record["roles"].as_array().expect("roles array");
    let programmer = roles.iter()
        .find(|r| r["role_id"].as_str() == Some("programmer"))
        .expect("programmer role");
    let messages = programmer["messages"].as_array().expect("messages array");
    let has_human_reply = messages.iter().any(|m| {
        m["role"].as_str() == Some("user")
            && m["content"].as_str()
                .map(|c| c.contains("[HUMAN @") && c.contains("yes, refactor first"))
                .unwrap_or(false)
    });
    assert!(has_human_reply,
        "programmer's history should contain the human's reply with [HUMAN @ ...] prefix; \
         got messages: {:#?}", messages);

    // 10. Sanity: the mock server captured the LLM request with
    //     a "messages" array.
    let captured = server.captured_request.lock().unwrap();
    let req = captured.as_ref().expect("mock server should have captured the request");
    let messages = req["messages"].as_array().expect("messages array");
    assert!(!messages.is_empty(), "LLM request had no messages");
}
