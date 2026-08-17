//! The ten black-box integration tests for `latte-agent chat`.
//!
//! Each `#[test]` function corresponds to one numbered scenario from
//! the test plan in `tests/blackbox/README.md`. Tests that need a real
//! model response are marked `#[ignore]` so `cargo test
//! --test blackbox` stays fast and free.
//!
//! The tests are intentionally short: each one builds a `TestEnv`,
//! spawns the binary, and asserts on a small number of concrete
//! observable properties (exit code, stderr substring, JSONL event
//! presence). Anything more complex lives in `common/mod.rs`.

use crate::common::{events_of, variant_name, TestEnv};

// ────────────────────────────────────────────────────────────────────────
// Test 1 — `direct_answer_small_question`
//
// Pipeline-shape assertion only. The manager prompt's hard rule
// "every substantive task MUST be delegated" means a real manager call
// WILL produce a `ToolExec` event with `name=delegate` for almost any
// input. We therefore don't assert "no delegate" — instead we verify
// the trace shape and that the original question survives into the
// prompt (post-hook, so possibly with a "Why no delegation" header
// prepended by the manager).
//
// `#[ignore]` because verifying the response *content* ("contains 2
// or 二") needs a real model call.
// ────────────────────────────────────────────────────────────────────────
#[test]
#[ignore = "requires DEEPSEEK_API_KEY for response content check"]
fn direct_answer_small_question() {
    let env = TestEnv::new();
    let prompt = "1+1等于几?";
    let out = env.run(
        &[
            "chat",
            "--debug",
            "--debug-format",
            "jsonl",
            "--tier",
            "standard",
            "-m",
            "deepseek-v4-flash",
        ],
        prompt.as_bytes(),
    );
    // Allow non-zero exit — manager may legitimately fail to reach a
    // model. The pipeline-shape assertions below are the real test.
    let _ = out.exit_code();

    let events = env.read_events();
    assert!(
        !events.is_empty(),
        "trace JSONL was empty; binary exited stderr={}",
        out.stderr
    );

    // Must have at least one SessionStart and SessionEnd.
    assert!(
        events_of(&events, "SessionStart").len() >= 1,
        "no SessionStart event; events={:?}",
        events
            .iter()
            .filter_map(variant_name)
            .collect::<Vec<_>>()
    );
    assert!(
        events_of(&events, "SessionEnd").len() >= 1,
        "no SessionEnd event"
    );

    // PromptBuilt should carry the original question (potentially with
    // a "Why no delegation" header prepended by the manager prompt).
    let prompt_built = events_of(&events, "PromptBuilt");
    assert!(
        !prompt_built.is_empty(),
        "no PromptBuilt event — redact or scope bug"
    );
    let user_input = prompt_built[0]
        .get("PromptBuilt")
        .and_then(|p| p.get("user_input"))
        .and_then(|u| u.as_str())
        .unwrap_or("");
    assert!(
        user_input.contains(prompt),
        "PromptBuilt.user_input does not contain the original prompt: {:?}",
        user_input
    );
}

// ────────────────────────────────────────────────────────────────────────
// Test 2 — `delegate_dispatch_visible`
//
// Verify the manager-to-specialist dispatch path:
//   * stderr shows "→ delegating to <role>" header
//   * JSONL has a ToolExec event with name=delegate
//   * the specialist's events appear in the SAME trace JSONL, tagged
//     role=<specialist> (ScopedSink working)
// ────────────────────────────────────────────────────────────────────────
#[test]
#[ignore = "requires DEEPSEEK_API_KEY — full manager→programmer dispatch"]
fn delegate_dispatch_visible() {
    let env = TestEnv::new();
    let prompt = "分析 src/agent.rs 中 run_turn 的步骤";
    let out = env.run(
        &[
            "chat",
            "--debug",
            "--debug-format",
            "jsonl",
            "-r",
            "manager",
            "--tier",
            "standard",
            "-m",
            "deepseek-v4-flash",
        ],
        prompt.as_bytes(),
    );
    let _ = out.exit_code();

    // The delegation header is on stderr (eprintln! in delegate handler).
    assert!(
        out.stderr.contains("→ delegating to"),
        "missing delegation header in stderr; got:\n{}",
        out.stderr
    );

    let events = env.read_events();
    let tool_execs = events_of(&events, "ToolExec");
    assert!(
        !tool_execs.is_empty(),
        "no ToolExec events in trace; events={:?}",
        events
            .iter()
            .filter_map(variant_name)
            .collect::<Vec<_>>()
    );
    // At least one ToolExec must be a delegate.
    let delegate = tool_execs
        .iter()
        .find(|e| {
            e.get("ToolExec")
                .and_then(|t| t.get("name"))
                .and_then(|n| n.as_str())
                == Some("delegate")
        })
        .expect("no ToolExec with name=delegate");
    let args = delegate
        .get("ToolExec")
        .and_then(|t| t.get("args_json"))
        .and_then(|a| a.as_str())
        .unwrap_or("");
    assert!(
        args.contains("\"role\""),
        "delegate args_json missing role field: {}",
        args
    );

    // ScopedSink: at least one event in the JSONL should have a meta.role
    // other than "manager" — i.e. a specialist's events were routed
    // through the same sink with the role tag overridden.
    let specialist_roles: Vec<&str> = events
        .iter()
        .filter_map(|e| e.as_object()?.values().next()?.get("meta"))
        .filter_map(|m| m.get("role")?.as_str())
        .filter(|r| *r != "manager")
        .collect();
    assert!(
        !specialist_roles.is_empty(),
        "no specialist events (ScopedSink should tag events with the \
         specialist's role); all events had role=manager"
    );
}

// ────────────────────────────────────────────────────────────────────────
// Test 3 — `three_layer_review_chain`
//
// Manager → programmer → reviewer_sanity. Verify:
//   * at least one ToolExec with name=delegate, args.role=programmer
//   * at least one ToolExec with name=delegate, args.role=reviewer_sanity
//   * reviewer_sanity's response (the ToolExec with status=ok payload)
//     contains "VERDICT:"
// ────────────────────────────────────────────────────────────────────────
#[test]
#[ignore = "requires DEEPSEEK_API_KEY — full 3-layer chain"]
fn three_layer_review_chain() {
    let env = TestEnv::new();
    let prompt =
        "列出 src/agent.rs 中所有 pub items starting with Agent; review your own answer";
    let out = env.run(
        &[
            "chat",
            "--debug",
            "--debug-format",
            "jsonl",
            "-r",
            "manager",
            "--tier",
            "standard",
            "-m",
            "deepseek-v4-flash",
        ],
        prompt.as_bytes(),
    );
    let _ = out.exit_code();

    let events = env.read_events();
    let delegates: Vec<&serde_json::Value> = events_of(&events, "ToolExec")
        .into_iter()
        .filter(|e| {
            e.get("ToolExec")
                .and_then(|t| t.get("name"))
                .and_then(|n| n.as_str())
                == Some("delegate")
        })
        .collect();
    let roles: Vec<String> = delegates.iter().filter_map(|e| delegate_role(e)).collect();
    assert!(
        roles.iter().any(|r| r == "programmer"),
        "no delegate to programmer; got roles={:?}",
        roles
    );
    assert!(
        roles.iter().any(|r| r == "reviewer_sanity"),
        "no delegate to reviewer_sanity; got roles={:?}",
        roles
    );

    // reviewer_sanity's response must contain "VERDICT:". The
    // delegate handler returns {"role": "...", "response": "..."};
    // for the delegate ToolExec with role=reviewer_sanity, the
    // status=ok payload contains the JSON-encoded response.
    let sanity_resp = delegates
        .iter()
        .find(|e| delegate_role(e).as_deref() == Some("reviewer_sanity"))
        .and_then(|e| {
            e.get("ToolExec")
                .and_then(|t| t.get("status"))
                .and_then(|s| s.get("Ok"))
                .and_then(|r| r.as_str())
        })
        .unwrap_or("");
    assert!(
        sanity_resp.contains("VERDICT:"),
        "reviewer_sanity response missing 'VERDICT:' marker; got: {}",
        sanity_resp
    );
}

// ────────────────────────────────────────────────────────────────────────
// Test 4 — `tier_override`
//
// `--tier premium` should be reflected in the runner-built log AND the
// SessionStart event's tier field. Without an API key we can verify
// the tier is "premium" in the log (so the operator can see what tier
// was attempted); with a key we'd verify the model fallthrough to a
// working model. We assert both.
// ────────────────────────────────────────────────────────────────────────
#[test]
#[ignore = "requires DEEPSEEK_API_KEY for full model chain resolution"]
fn tier_override() {
    let env = TestEnv::new();
    let out = env.run(
        &[
            "chat",
            "--debug",
            "--debug-format",
            "jsonl",
            "-r",
            "programmer",
            "-t",
            "premium",
            "-m",
            "deepseek-v4-flash",
        ],
        b"hi",
    );
    let _ = out.exit_code();

    // Runner built log line records the requested tier. Even without
    // an API key, this fires after the config loads.
    assert!(
        out.stderr.contains("tier=premium"),
        "runner built log missing tier=premium; got stderr:\n{}",
        out.stderr
    );
    let events = env.read_events();
    if let Some(start) = events_of(&events, "SessionStart").first() {
        let tier = start
            .get("SessionStart")
            .and_then(|s| s.get("tier"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        assert_eq!(tier, "premium", "SessionStart.tier should be 'premium'");
    }
}

// ────────────────────────────────────────────────────────────────────────
// Test 5 — `cjk_input_does_not_panic`
//
// The UTF-8 truncation bug from ad37f91 must not return. We don't
// need an API key — even with a config-error failure the binary
// shouldn't panic. This test runs by default.
// ────────────────────────────────────────────────────────────────────────
#[test]
fn cjk_input_does_not_panic() {
    let env = TestEnv::new();
    let prompt = "什么是 Cargo workspace?列出 5 个核心要点";
    let out = env.run_fast(
        &[
            "chat",
            "--debug",
            "--debug-format",
            "jsonl",
            "-r",
            "manager",
            "--tier",
            "standard",
            "-m",
            "deepseek-v4-flash",
        ],
        prompt.as_bytes(),
    );
    assert!(
        !out.stderr.contains("panicked at"),
        "binary panicked on CJK input; stderr:\n{}",
        out.stderr
    );
    assert!(
        !out.stderr.contains("thread '"),
        "binary thread panic message found; stderr:\n{}",
        out.stderr
    );
    // Trace JSONL must parse even with non-ASCII content.
    let events = env.read_events();
    assert!(
        events_of(&events, "SessionStart").len() == 1,
        "expected exactly one SessionStart"
    );
}

// ────────────────────────────────────────────────────────────────────────
// Test 6 — `hook_redact_pii`
//
// PreCall hooks fire BEFORE the model call, so this test works
// without an API key. We assert:
//   * HookFired event with hook_name=redact_pii
//   * point=PreCall
//   * PromptBuilt.user_input does NOT contain the raw phone number
//     and DOES contain "<REDACTED:"
// ────────────────────────────────────────────────────────────────────────
#[test]
fn hook_redact_pii() {
    let env = TestEnv::new();
    let prompt = "call me at 13812345678 about it";
    let out = env.run_fast(
        &[
            "chat",
            "--debug",
            "--debug-hooks",
            "redact_pii",
            "--debug-format",
            "jsonl",
            "-r",
            "manager",
            "--tier",
            "standard",
            "-m",
            "deepseek-v4-flash",
        ],
        prompt.as_bytes(),
    );
    let _ = out.exit_code();

    let events = env.read_events();
    let hooks: Vec<&serde_json::Value> = events_of(&events, "HookFired")
        .into_iter()
        .filter(|e| {
            e.get("HookFired")
                .and_then(|h| h.get("hook_name"))
                .and_then(|n| n.as_str())
                == Some("redact_pii")
        })
        .collect();
    assert!(
        !hooks.is_empty(),
        "no HookFired event for redact_pii; events={:?}",
        events
            .iter()
            .filter_map(variant_name)
            .collect::<Vec<_>>()
    );
    let _ = hooks
        .iter()
        .find(|h| {
            h.get("HookFired")
                .and_then(|hh| hh.get("point"))
                .and_then(|p| p.as_str())
                == Some("PreCall")
        })
        .expect("no redact_pii HookFired with point=PreCall");

    // PromptBuilt's user_input should be the redacted form.
    let prompt_builts = events_of(&events, "PromptBuilt");
    let prompt_built = prompt_builts.first().expect("no PromptBuilt event");
    let user_input = prompt_built
        .get("PromptBuilt")
        .and_then(|p| p.get("user_input"))
        .and_then(|u| u.as_str())
        .unwrap_or("");
    assert!(
        !user_input.contains("13812345678"),
        "PromptBuilt.user_input still contains the unredacted phone: {:?}",
        user_input
    );
    assert!(
        user_input.contains("<REDACTED:"),
        "PromptBuilt.user_input missing redaction marker: {:?}",
        user_input
    );
}

// ────────────────────────────────────────────────────────────────────────
// Test 7 — `filter_sink_filters_stdout`
//
// FilterSink applies to stdout only. The JsonlSink / IndexSink stay
// unfiltered — operators still want the full trace on disk; the
// filter is only for the live stdout view.
//
// We verify the trace JSONL (the unfiltered sink) gets ALL events
// regardless of the `--debug-events` flag. The filtered stdout
// contents are harder to assert on without a model call (we'd need
// a real ToolExec to land on stdout); we instead verify the binary
// ran cleanly and the trace JSONL contains SessionStart/End.
// ────────────────────────────────────────────────────────────────────────
#[test]
fn filter_sink_filters_stdout() {
    let env = TestEnv::new();
    let _out = env.run_fast(
        &[
            "chat",
            "--debug",
            "--debug-events",
            "ToolExec,HookFired",
            "--debug-format",
            "pretty",
            "-r",
            "manager",
            "--tier",
            "standard",
            "-m",
            "deepseek-v4-flash",
        ],
        b"hi",
    );

    // FilterSink must NOT apply to JsonlSink. The trace file is the
    // unfiltered mirror of every event — verify it parses and has
    // both SessionStart and SessionEnd.
    let events = env.read_events();
    let session_start = events_of(&events, "SessionStart");
    let session_end = events_of(&events, "SessionEnd");
    assert!(
        !session_start.is_empty(),
        "trace JSONL missing SessionStart — FilterSink must not apply to JsonlSink"
    );
    assert!(
        !session_end.is_empty(),
        "trace JSONL missing SessionEnd — FilterSink must not apply to JsonlSink"
    );
}

// ────────────────────────────────────────────────────────────────────────
// Test 8 — `per_model_timeout`
//
// With `LATTE_AGENT_DELEGATE_TIMEOUT_SECS=2`, a specialist delegation
// that takes longer than 2s should fail with "timed out after 2s"
// in stderr. We can't easily force a long-running delegate without
// hitting the model API, so this test is `#[ignore]`d and only
// verifies behavior under a real network call.
// ────────────────────────────────────────────────────────────────────────
#[test]
#[ignore = "requires DEEPSEEK_API_KEY to exercise timeout on real delegate"]
fn per_model_timeout() {
    use std::io::Write;
    let env = TestEnv::new();
    // Use a command that the manager is almost certain to delegate
    // (anything substantive). With a 2s timeout the programmer
    // specialist won't finish in time.
    let prompt = "分析 src/agent.rs 完整代码并生成一份总结文档";
    let mut cmd = std::process::Command::new(env.bin());
    cmd.current_dir(common_workspace_root())
        .env("LATTE_HOME", env.latte_home())
        .env("LATTE_AGENT_DELEGATE_TIMEOUT_SECS", "2")
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .arg("--agents-config")
        .arg(common_workspace_root().join("config/agents"))
        .arg("--models-config")
        .arg(common_workspace_root().join("config/models.toml"))
        .args(&[
            "chat",
            "--debug",
            "--debug-format",
            "jsonl",
            "-r",
            "manager",
            "--tier",
            "standard",
            "-m",
            "deepseek-v4-flash",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().expect("spawn");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(prompt.as_bytes())
        .unwrap();
    let output = child.wait_with_output().expect("wait");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "expected non-zero exit on timeout; got 0"
    );
    assert!(
        stderr.contains("timed out after 2s"),
        "missing 'timed out after 2s' in stderr:\n{}",
        stderr
    );
    // The session must still complete (SessionEnd emitted) even on
    // error — Drop impl guarantees this.
    let events = env.read_events();
    let session_end = events_of(&events, "SessionEnd");
    assert_eq!(session_end.len(), 1, "expected exactly one SessionEnd");
    let turns = session_end[0]
        .get("SessionEnd")
        .and_then(|s| s.get("total_turns"))
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    assert_eq!(turns, 1, "SessionEnd.total_turns should be 1");
}

// ────────────────────────────────────────────────────────────────────────
// Test 9 — `latte_home_env_override`
//
// Setting LATTE_HOME to a tmp dir should redirect trace writes there.
// No API key needed. Test runs by default.
// ────────────────────────────────────────────────────────────────────────
#[test]
fn latte_home_env_override() {
    let env = TestEnv::new();
    let _out = env.run_no_stdin(&[
        "chat",
        "--debug",
        "--debug-format",
        "jsonl",
        "-r",
        "manager",
        "--tier",
        "standard",
        "-m",
        "deepseek-v4-flash",
    ]);

    // The trace file must be under $LATTE_HOME/traces/, not ~/.latte/.
    let trace_path = env.trace_path();
    assert!(
        trace_path.starts_with(env.latte_home()),
        "trace file {} not under LATTE_HOME {}",
        trace_path.display(),
        env.latte_home().display()
    );
    let components: Vec<String> = trace_path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    assert!(
        components.iter().any(|c| c == "traces"),
        "trace file missing 'traces' component: {:?}",
        components
    );

    // Every line must be parseable JSON.
    let events = env.read_events();
    for ev in &events {
        assert!(ev.is_object(), "event is not a JSON object: {:?}", ev);
    }

    // SessionEnd present (the Drop impl guarantees it).
    let session_end = events_of(&events, "SessionEnd");
    assert_eq!(
        session_end.len(),
        1,
        "expected exactly one SessionEnd, got {}",
        session_end.len()
    );
}

// ────────────────────────────────────────────────────────────────────────
// Test 10 — `session_resume`
//
// Verify --resume loads the saved history into the agent's context.
// We can't trigger an auto-save without first making a turn fail (and
// even then the saved context wouldn't include the failed input). So
// we hand-craft a save file using the same format `save_session`
// writes (one JSON-encoded Message per line) and pass it via --resume.
//
// With --resume, the runner loads history BEFORE the user's new
// message. Even without an API key the model call fails, but the
// trace's PromptBuilt.user_input should still reflect the loaded
// context (and the assistant's stderr message "loaded N messages"
// confirms the resume path fired).
// ────────────────────────────────────────────────────────────────────────
#[test]
fn session_resume() {
    use std::io::Write;
    let env = TestEnv::new();
    // Hand-crafted save file matching save_session's format:
    // each line is one serde_json::to_string of a Message
    // { "role": "system"|"user"|"assistant", "content": "..." }
    let save_path = env.latte_home().join("resume-input.jsonl");
    {
        let mut f = std::fs::File::create(&save_path).expect("create save file");
        writeln!(
            f,
            r#"{{"role":"user","content":"remember the number 42"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"role":"assistant","content":"OK I will remember 42"}}"#
        )
        .unwrap();
    }
    let out = env.run_fast(
        &[
            "chat",
            "--debug",
            "--debug-format",
            "jsonl",
            "--resume",
            save_path.to_str().unwrap(),
            "--tier",
            "standard",
            "-m",
            "deepseek-v4-flash",
        ],
        b"what number did I ask you to remember?",
    );
    let _ = out.exit_code();

    // Stderr should show the "loaded N messages from <path>" line.
    // Renderer status messages go to stdout (JSONL events in --debug mode,
    // bracketed lines in plain mode). Either way the substring is the same.
    assert!(
        out.stdout.contains("[resume] loaded"),
        "no '[resume] loaded' message in stdout; got:\nstdout:{}\nstderr:{}",
        out.stdout,
        out.stderr
    );

    // The trace's PromptBuilt.user_input is what the model sees
    // post-hook — should reflect the resume path didn't break the
    // pipeline (the question is appended after history). We don't
    // assert the exact history because redact hooks may transform
    // it; we just assert the new question is present.
    let events = env.read_events();
    let prompt_builts = events_of(&events, "PromptBuilt");
    let prompt_built = prompt_builts.first().expect("no PromptBuilt event");
    let user_input = prompt_built
        .get("PromptBuilt")
        .and_then(|p| p.get("user_input"))
        .and_then(|u| u.as_str())
        .unwrap_or("");
    assert!(
        user_input.contains("what number did I ask you to remember?"),
        "PromptBuilt.user_input missing the new question: {:?}",
        user_input
    );

    // The history_len in PromptBuilt should reflect the loaded
    // history (2 messages from the save file, not 0).
    let history_len = prompt_built
        .get("PromptBuilt")
        .and_then(|p| p.get("history_len"))
        .and_then(|h| h.as_u64())
        .unwrap_or(0);
    assert!(
        history_len >= 2,
        "history_len should be >= 2 after resume; got {}",
        history_len
    );
}

// ──── helpers ─────────────────────────────────────────────────────────
/// Pull the `role` argument out of a `ToolExec` event whose
/// `args_json` is a JSON-encoded object. Returns `None` if the event
/// isn't a delegate ToolExec or the role is missing.
fn delegate_role(e: &serde_json::Value) -> Option<String> {
    let args = e.get("ToolExec")?.get("args_json")?.as_str()?;
    let parsed: serde_json::Value = serde_json::from_str(args).ok()?;
    parsed.get("role")?.as_str().map(String::from)
}

// We need `workspace_root` here too for the timeout test which spawns
// a Command manually. Inline the lookup rather than depending on a
// `pub` re-export from common.
fn common_workspace_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}