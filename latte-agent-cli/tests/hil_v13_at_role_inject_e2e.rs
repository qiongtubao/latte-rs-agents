//! E2E for v1.3b: REPL `@<role>` injection actually lands in the
//! target role's history.
//!
//! Drives the REPL through a single round via an HTTP-mock LLM
//! endpoint. The script is:
//!   1. `@programmer check this` — REPL parses to
//!      `ReplInput::RoleInject { role_id: "programmer", message: "check this" }`
//!      and `chat.rs:1851` calls `queue_inject(worktree, "programmer", "check this")`
//!      which appends to `<worktree>/.latte/inject/programmer.txt`.
//!   2. Round 1 starts. `scheduler.order = ["programmer", "manager"]`
//!      (alphabetical with manager last). The role loop first hits
//!      `programmer`; `chat.rs:1885-1897` drains
//!      `.latte/inject/programmer.txt` and appends a synthetic
//!      `Role::User` message with content `"[INJECTED]\n<drained content>"`
//!      to `RoleHistory.messages` via `mgr.append_to_role("programmer", ...)`.
//!      `append_to_role` persists the new history to the session JSON.
//!      The role's LLM call then runs against the mock.
//!   3. `manager` gets its turn and runs against the mock.
//!   4. `/quit` reads from stdin and `mark_done` transitions to Done.
//!
//! After the binary exits, the test reads the final session JSON and
//! asserts that `roles[programmer].messages` contains a `role: "user"`
//! message whose `content` starts with `"[INJECTED]"` and includes
//! the injected text `"check this"`.

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

/// Mock OpenAI chat/completions server. Returns a plain-text canned
/// response on every request. Two responses are pre-baked so the
/// round-1 programmer LLM call and the round-1 manager LLM call both
/// complete (round 2 never runs because `/quit` reads the second
/// line of stdin and breaks the loop before its role loop fires).
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
        let remaining = Arc::new(Mutex::new(2));
        let captured_clone = captured.clone();
        let shutdown_clone = shutdown.clone();
        let remaining_clone = remaining.clone();
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
                // Plain text on both calls — no ask_human, no tool call.
                // The assertion is on the role's history JSON, not on
                // the LLM response, so the response body content does
                // not need to embed the injected text.
                let body = serde_json::json!({
                    "id": "chatcmpl-mock",
                    "object": "chat.completion",
                    "created": 0,
                    "model": "mock",
                    "choices": [{
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": "Acknowledged.",
                        },
                        "finish_reason": "stop",
                    }],
                    "usage": {"prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120},
                });
                let body_str = serde_json::to_string(&body).unwrap();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    body_str.len(),
                    body_str,
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
                let mut r = remaining_clone.lock().unwrap();
                *r -= 1;
                if *r == 0 { break; }
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
async fn at_role_inject_lands_in_specialist_history() {
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
    let task_id = format!("at-role-inject-{}", std::process::id());

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

    // 7. Spawn chat with stdin that drives the inject + quit flow.
    //    --roles manager,programmer: scheduler.order is alphabetical
    //      with manager last, so [programmer, manager]. This means
    //      programmer's turn (and queue drain) happens first.
    //    --max-rounds 2:
    //      round 1: top reads "@programmer check this" from stdin ->
    //               queue_inject(programmer.txt). Role loop: programmer
    //               drains queue -> appends `[INJECTED]\ncheck this`
    //               to programmer history. LLM call 1 (programmer).
    //               Then manager LLM call 2.
    //      round 2: top reads "/quit" from stdin -> mark_done -> break.
    //    The script is exactly 2 lines, no ask_human, no manager input.
    let script = b"@programmer check this\n/quit\n";
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
        "expected a Done session JSON after the inject + quit cycle"
    );
    let record: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&done_session).unwrap()
    ).unwrap();

    // 9. Assert the programmer role has the injected message in its
    //    history (proves the queue drain + append_to_role path landed).
    //    The synthetic message format from chat.rs:1891 is:
    //      "[INJECTED]\n<content of programmer.txt>"
    //    where the queue file ends with a trailing newline from
    //    `writeln!` in queue_inject (chat.rs:2022). So the literal
    //    content is `"[INJECTED]\ncheck this\n"`.
    let roles = record["roles"].as_array().expect("roles array");
    let programmer = roles.iter()
        .find(|r| r["role_id"].as_str() == Some("programmer"))
        .expect("programmer role");
    let messages = programmer["messages"].as_array().expect("messages array");
    let has_inject = messages.iter().any(|m| {
        m["role"].as_str() == Some("user")
            && m["content"].as_str()
                .map(|c| c.contains("[INJECTED]") && c.contains("check this"))
                .unwrap_or(false)
    });
    assert!(has_inject,
        "programmer's history should contain the injected message with [INJECTED] sentinel \
         and 'check this' payload; got messages: {:#?}", messages);

    // 10. Sanity: the queue file was consumed and removed by the drain
    //     path. If the drain never fired, the file would still be
    //     present at <worktree>/.latte/inject/programmer.txt.
    let queue_path = repo.join(format!(
        ".latte/worktrees/{}/.latte/inject/programmer.txt", task_id
    ));
    assert!(!queue_path.exists(),
        "inject queue file should be drained and removed after round 1; \
         still present at {}", queue_path.display());

    // 11. Sanity: the mock server captured the LLM request with
    //     a "messages" array (so we know round 1 actually ran an
    //     LLM call rather than failing earlier).
    let captured = server.captured_request.lock().unwrap();
    let req = captured.as_ref().expect("mock server should have captured the request");
    let req_messages = req["messages"].as_array().expect("messages array");
    assert!(!req_messages.is_empty(), "LLM request had no messages");
}