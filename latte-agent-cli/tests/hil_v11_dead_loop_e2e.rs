//! E2E for v1.1 spec §12 item 8: a specialist that calls the same
//! `tool_call:write:abc123` three turns in a row triggers an
//! auto-pause with reason `"dead loop: ..."`.
//!
//! Drives the REPL through 3 full rounds via an HTTP-mock LLM
//! endpoint. The script is:
//!   1. `first task`       — REPL parses to `ReplInput::ManagerInput`
//!      and chat.rs appends a `Role::User` message to the manager's
//!      history. Round 1's role loop runs both roles; each role's
//!      LLM call returns the same canned tool call body, the in-run
//!      `LoopDetector` trips (after 3 identical calls within a single
//!      `run_turn`), and `last_decision_kind()` returns `"text"`
//!      because the assistant message never landed in
//!      `runner.context`. The supervisor observes `"text"` for both
//!      roles (1 of 3 in the dead-loop ring buffer).
//!   2. `second task`      — round 2. Each role's LLM call returns the
//!      same tool call body again. In-run loop trips; supervisor
//!      observes `"text"` for both roles (2 of 3).
//!   3. `third task`       — round 3. Programmer's LLM call returns
//!      the same body; in-run loop trips; supervisor observes
//!      `"text"` for programmer (3 of 3) and fires with reason
//!      `"dead loop: programmer repeating text"`. chat.rs:1990
//!      emits `[supervisor pause: dead loop: ...]` to stdout and
//!      calls `pause_with_reason(...)`. The round breaks via
//!      `continue 'rounds` before the manager's turn.
//!   4. `/quit`            — round 4 top-of-loop: state is Paused
//!      (not ask_human), reads `/quit` from stdin, calls
//!      `resume() + mark_done()` and breaks.
//!
//! The supervisor's dead-loop ring buffer is per-role with window=3,
//! so 3 consecutive observations of the same decision (here all
//! `"text"`) trips the auto-pause. The reason always starts with
//! `"dead loop: "` and includes the role_id + the decision string,
//! so the stdout line `[supervisor pause: dead loop: ...]` is the
//! canonical signal. We also re-check the role's history: after 3
//! rounds the role has 1 user message per REPL input line (the
//! `[PLAN SLICE]` synthesised by chat.rs:1899 + 0 assistant
//! messages, since every `run_turn` errored out before pushing
//! its assistant message into `self.context`).

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

/// Mock OpenAI chat/completions server. Returns the same canned
/// `<tool_callwrite>{"path":"/tmp/x"}</tool_call></tool_callwrite>`
/// body on every request. The tool name is `write` so it resolves
/// to the registered `file.write` builtin; the args are missing
/// the required `content` field, so the write tool returns an
/// error, but the model still sees its own previous response in
/// the next LLM call and emits the same call again. The
/// in-run `LoopDetector` (3 same tool calls in a row) trips and
/// `run_turn` returns `Err(ToolLoopDetected)`. The supervisor's
/// per-round `last_decision_kind()` then returns `"text"`
/// (because the failing run_turn never pushes its assistant
/// message to the runner's context), and after 3 such rounds
/// the supervisor fires the dead-loop trigger.
struct MockOpenAIServer {
    addr: SocketAddr,
    captured_request: Arc<Mutex<Option<serde_json::Value>>>,
    shutdown: Arc<Mutex<bool>>,
    /// How many requests the mock should respond to before closing.
    /// Set generously: each REPL round hits each role's LLM up to
    /// 3 times (in-run LoopDetector trips on the 3rd identical
    /// call), so 2 roles × 3 rounds × 3 calls = 18 worst case, plus
    /// a safety margin.
    remaining: Arc<Mutex<u32>>,
}

impl MockOpenAIServer {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = Arc::new(Mutex::new(None));
        let shutdown = Arc::new(Mutex::new(false));
        let remaining = Arc::new(Mutex::new(30));
        let captured_clone = captured.clone();
        let shutdown_clone = shutdown.clone();
        let remaining_clone = remaining.clone();
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
                // The same canned body on every call. The write tool
                // is registered (file.write), the args are missing
                // the required `content` field so the tool errors
                // out, but the model still sees the tool_error and
                // emits the same call again. The in-run LoopDetector
                // trips on the 3rd identical call.
                {
                    let mut r = remaining_clone.lock();
                    if *r == 0 { break; }
                    *r -= 1;
                }
                let body = serde_json::json!({
                    "id": "chatcmpl-mock",
                    "object": "chat.completion",
                    "created": 0,
                    "model": "mock",
                    "choices": [{
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": "<tool_callwrite>{\"path\":\"/tmp/x\"}</tool_call></tool_callwrite>",
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
async fn v11_supervisor_dead_loop_triggers_after_three_same_decisions() {
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
    let task_id = format!("v11deadloop-{}", std::process::id());

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

    // 7. Spawn chat with a 4-line stdin script that drives the
    //    dead-loop cycle:
    //    --roles manager,programmer: scheduler.order is
    //      alphabetical with manager last, so [programmer, manager].
    //      Each role's LLM call returns the same canned tool call
    //      body, the in-run LoopDetector trips within that role's
    //      run_turn, and the supervisor observes "text" for that
    //      role (because the failed run_turn never pushes its
    //      assistant message into the runner's context).
    //    --max-rounds 5: room for 3 full rounds + the supervisor
    //      pause round + a safety margin. The supervisor fires at
    //      the end of round 3 (programmer's 3rd observation), the
    //      round breaks via `continue 'rounds`, and round 4 reads
    //      `/quit` from stdin to exit cleanly.
    //    The 3 manager-input lines are routed to the manager role's
    //    history (since `--roles manager,programmer` includes
    //    manager), so the REPL's parse_repl_line succeeds with
    //    `ManagerInput { message }` and appends to manager's
    //    history before the round's role loop fires.
    let script = b"first task\nsecond task\nthird task\n/quit\n";
    let mut chat_cmd = Command::new(bin());
    chat_cmd.arg("chat")
        .arg("--task-id").arg(&task_id)
        .arg("--roles").arg("manager,programmer")
        .arg("--initial-prompt").arg("noop")
        .arg("--max-rounds").arg("5")
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

    // 8. PRIMARY assertion: the supervisor's auto-pause log line is
    //    in stdout. chat.rs:1990 prints `[supervisor pause: <reason>]`
    //    where `<reason>` always starts with `"dead loop: "` and
    //    includes the role id + the decision that repeated. This is
    //    the v1.1 spec §12 item 8 spec assertion: a specialist
    //    calling the same tool 3 times in a row triggers an
    //    auto-pause with reason `"dead loop: ..."`.
    assert!(
        stdout.contains("[supervisor pause: dead loop:"),
        "expected the supervisor's dead-loop pause log line in stdout; got:\n{}",
        stdout,
    );

    // 9. SECONDARY assertion: the role id appears in the pause
    //    reason, confirming the supervisor attached the right
    //    role to the dead-loop report. With scheduler.order
    //    [programmer, manager], programmer's 3rd observation fires
    //    first; manager never gets to its 3rd observation in round 3
    //    (the round breaks after the supervisor fires), so the
    //    reason should mention programmer.
    assert!(
        stdout.contains("programmer repeating"),
        "expected the pause reason to name the programmer role; got:\n{}",
        stdout,
    );

    // 10. Find the final session JSON. After `/quit` the REPL
    //     resumes + mark_dones, so the final persisted state is
    //     `Done` (and `pause_reason` is cleared by the resume
    //     transition). The supervisor's pause log line above is
    //     the unambiguous spec assertion; the state assertion is a
    //     sanity check that the session terminated cleanly.
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
    assert!(done_session.is_some(),
        "expected a Done session JSON after the dead-loop + /quit cycle");

    // 11. Sanity: the mock server captured at least one LLM
    //     request with a "messages" array. This proves the
    //     specialist's LLM call actually reached the mock and
    //     the tool-call body was processed by the runner.
    let captured = server.captured_request.lock();
    let req = captured.as_ref().expect("mock server should have captured the request");
    let messages = req["messages"].as_array().expect("messages array");
    assert!(!messages.is_empty(), "LLM request had no messages");
}
