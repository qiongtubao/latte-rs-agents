//! Shared helpers for the black-box integration tests.
//!
//! Each test creates a `TestEnv` that:
//!
//!   * points at a fresh `LATTE_HOME` (a per-test temp dir) so traces,
//!     session indices, and chat logs land in a sandbox instead of the
//!     user's `~/.latte`.
//!   * spawns the actual `target/debug/latte-agent` binary with
//!     `--agents-config <project>` and `--models-config <project>` so the
//!     test sees the project's own agents.toml / models.toml.
//!   * captures stdout/stderr/exit_code from the subprocess.
//!   * knows the canonical `session_id` (derived from the chat-log file
//!     stem: `chat-YYYYMMDD-HHMMSS-<pid>`) so the test can locate the
//!     `traces/<id>.jsonl` file emitted by `--debug` and parse events
//!     back into `serde_json::Value`s.
//!
//! Latte-agent's chat log currently writes to `~/.latte/logs/`
//! regardless of `LATTE_HOME` (see `chatlog::global_log_dir`), so we
//! derive the session id from the chat log file the binary created on
//! its own (read it back from `~/.latte/logs/` if needed) and use the
//! matching `traces/<id>.jsonl` under the test's `LATTE_HOME`. For
//! tests where the binary never reaches the point of opening a chat
//! log (e.g. config-load failure), the trace file will be missing too.
//!
//! Tests do NOT mock the model. With `DEEPSEEK_API_KEY` unset the model
//! call fails fast with "no api_key configured" and the trace still
//! carries `SessionStart`, `PromptBuilt`, and `SessionEnd` — exactly the
//! shape the pipeline tests assert against. Tests that need a real
//! model response are marked `#[ignore]` so the default `cargo test`
//! stays fast and free.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

/// Maximum time we wait for the binary to finish. Most model calls in
/// this project target "DeepSeek V4 Flash" which typically returns in
/// 5–30s; 120s gives the manager→programmer→reviewer chain (Test 3)
/// plenty of room without hanging the suite on a hung network.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Wall-clock timeout for tests that don't talk to a model at all
/// (CLI parsing, env-override layout, sink filtering). 30s is plenty
/// for `cargo run`-equivalent startup + a single non-interactive turn.
const FAST_TIMEOUT: Duration = Duration::from_secs(30);

/// One isolated black-box test environment.
///
/// Holds the temp dir alive for the lifetime of the test (Drop
/// recursively removes the dir). The binary is located lazily on
/// first `run` so the harness doesn't pay for `which` on every
/// test construction.
pub struct TestEnv {
    /// Per-test LATTE_HOME. Traces / sessions land here. The chat log
    /// does NOT (chatlog uses $HOME directly), so we don't assert on
    /// the log file's location — only on the trace JSONL and stderr.
    latte_home: TempDir,
    /// Where the binary lives (resolved once, lazily).
    bin: PathBuf,
    /// Cached `~/.latte/logs/` listing taken after the first run; used
    /// to derive the session id from the chat-log file stem.
    baseline_chat_logs: Vec<PathBuf>,
}

impl TestEnv {
    /// Build a fresh env. The temp dir is created on construction; if
    /// the binary can't be located the test fails immediately.
    pub fn new() -> Self {
        let latte_home = TempDir::new().expect("create temp LATTE_HOME");
        Self {
            latte_home,
            bin: locate_binary(),
            baseline_chat_logs: list_chat_logs(),
        }
    }

    /// Path to the per-test LATTE_HOME (e.g. `/tmp/.tmpXXXX/home`).
    pub fn latte_home(&self) -> &Path {
        self.latte_home.path()
    }

    /// Path to the binary.
    pub fn bin(&self) -> &Path {
        &self.bin
    }

    /// Spawn the binary with the given args. `args` is appended AFTER
    /// the auto-injected `--agents-config` / `--models-config` flags
    /// (so the project config is always used) and the binary inherits
    /// `LATTE_HOME=<temp>`. `stdin` is piped in as bytes.
    ///
    /// Returns a `RunOutput` capturing the three streams plus the
    /// wall-clock duration. Tests assert on those.
    pub fn run(&self, args: &[&str], stdin: &[u8]) -> RunOutput {
        self.run_with_timeout(args, stdin, DEFAULT_TIMEOUT)
    }

    /// Same as `run` but with a custom timeout. Used by the
    /// `per_model_timeout` test (which needs to observe the timeout
    /// fire) and by `latte_home_env` (which never reaches the model).
    pub fn run_with_timeout(&self, args: &[&str], stdin: &[u8], timeout: Duration) -> RunOutput {
        let start = Instant::now();
        let mut cmd = Command::new(&self.bin);
        cmd.current_dir(workspace_root())
            .env("LATTE_HOME", self.latte_home())
            .env_remove("DEEPSEEK_API_KEY")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("GLM_API_KEY")
            // The first arg is the subcommand (e.g. "chat"). clap
            // resolves --agents-config / --models-config as flags of
            // that subcommand, so we have to inject them AFTER the
            // subcommand name, not before it.
            .args(inject_config_flags(args, &workspace_root()))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // The trace file lands under LATTE_HOME. Pre-create it so
        // tests can stat the path even if --debug was off (in which
        // case it stays empty).
        let _ = std::fs::create_dir_all(self.latte_home().join("traces"));
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => panic!("failed to spawn {:?}: {}", self.bin, e),
        };
        // Always take + drop stdin so the child sees EOF, regardless
        // of whether the caller provided input. Without this drop,
        // `Stdio::piped()` keeps the write end open and the child
        // blocks forever waiting for the read end to close.
        if let Some(mut s) = child.stdin.take() {
            if !stdin.is_empty() {
                use std::io::Write;
                let _ = s.write_all(stdin);
            }
        }
        let output: Output = match wait_with_timeout(child, timeout) {
            Ok(out) => out,
            Err(e) => panic!(
                "binary did not exit within {:?} (args={:?}): {}",
                timeout, args, e
            ),
        };
        let elapsed = start.elapsed();
        RunOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            status: output.status,
            elapsed,
        }
    }

    /// Spawn the binary but DON'T pipe stdin (stdin gets inherited as
    /// closed → EOF). Useful for `< /dev/null`-style invocations.
    pub fn run_no_stdin(&self, args: &[&str]) -> RunOutput {
        self.run_with_timeout(args, &[], FAST_TIMEOUT)
    }

    /// Fast version (short timeout) for tests that don't reach the
    /// model.
    pub fn run_fast(&self, args: &[&str], stdin: &[u8]) -> RunOutput {
        self.run_with_timeout(args, stdin, FAST_TIMEOUT)
    }

    /// Find the trace JSONL written by the most recent `--debug` run.
    /// Returns the path. Panics if none found.
    pub fn trace_path(&self) -> PathBuf {
        let traces = self.latte_home().join("traces");
        let entries: Vec<PathBuf> = match std::fs::read_dir(&traces) {
            Ok(rd) => rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
                .collect(),
            Err(_) => Vec::new(),
        };
        match entries.as_slice() {
            [] => panic!(
                "no .jsonl trace file under {} — did the binary run with --debug?",
                traces.display()
            ),
            [single] => single.clone(),
            multiple => {
                // Multiple sessions in one test = take the newest by mtime.
                let mut with_mtime: Vec<(PathBuf, std::time::SystemTime)> = multiple
                    .iter()
                    .filter_map(|p| {
                        let m = std::fs::metadata(p).and_then(|x| x.modified()).ok()?;
                        Some((p.clone(), m))
                    })
                    .collect();
                with_mtime.sort_by_key(|(_, t)| *t);
                with_mtime
                    .last()
                    .map(|(p, _)| p.clone())
                    .expect("at least one entry")
            }
        }
    }

    /// Read every event from the most recent trace JSONL as
    /// `serde_json::Value`. Skips blank lines. Panics on parse errors
    /// (test failure).
    pub fn read_events(&self) -> Vec<Value> {
        let path = self.trace_path();
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
        let mut out = Vec::new();
        for (i, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let v: Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("parse {}:{} — {}: {}", path.display(), i + 1, e, line));
            out.push(v);
        }
        out
    }

    /// Find the chat-log file written to `~/.latte/logs/` by this
    /// test's most recent run. Returns its path or `None`.
    ///
    /// `chatlog::global_log_dir` ignores `LATTE_HOME` and uses
    /// `~/.latte/logs/` directly; we snapshot the listing at `new()`
    /// time and diff after each run to find the new file.
    pub fn latest_chat_log(&self) -> Option<PathBuf> {
        let now = list_chat_logs();
        now.into_iter()
            .find(|p| !self.baseline_chat_logs.contains(p))
    }

    /// Read the chat log content (string) for the latest run.
    pub fn read_chat_log(&self) -> Option<String> {
        self.latest_chat_log()
            .and_then(|p| std::fs::read_to_string(p).ok())
    }

    /// Session id (e.g. `chat-20260626-085143-6125`) extracted from
    /// the chat-log file stem. `None` if no chat log was written.
    pub fn session_id(&self) -> Option<String> {
        self.latest_chat_log().and_then(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
        })
    }
}

/// Result of one binary invocation.
#[derive(Debug, Clone)]
pub struct RunOutput {
    pub stdout: String,
    pub stderr: String,
    pub status: std::process::ExitStatus,
    pub elapsed: Duration,
}

impl RunOutput {
    /// Exit code, or -1 if the process was killed by a signal (Unix).
    pub fn exit_code(&self) -> i32 {
        self.status.code().unwrap_or(-1)
    }

    /// True iff exit code is 0.
    pub fn success(&self) -> bool {
        self.status.success()
    }

    /// Search both streams (and concat order doesn't matter for
    /// substring tests). Returns true if `needle` is present
    /// anywhere.
    pub fn contains(&self, needle: &str) -> bool {
        self.stdout.contains(needle) || self.stderr.contains(needle)
    }
}

/// Locate the `latte-agent` binary. Walks up from CARGO_MANIFEST_DIR
/// looking for `target/debug/latte-agent` (and the release variant).
/// Panics if not found — there's no graceful fallback because tests
/// without the binary are meaningless.
fn locate_binary() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest
        .parent()
        .expect("latte-agent-cli lives in a workspace");
    let candidates = [
        workspace.join("target/debug/latte-agent"),
        workspace.join("target/release/latte-agent"),
        manifest.join("target/debug/latte-agent"),
        manifest.join("target/release/latte-agent"),
    ];
    for c in &candidates {
        if c.exists() {
            return c.clone();
        }
    }
    panic!(
        "could not find latte-agent binary; tried {:?}; build with `cargo build -p latte-agent-cli` first",
        candidates
    );
}

/// Path to the workspace root (where `config/`, `Cargo.toml`, etc. live).
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("latte-agent-cli is in a workspace; parent should exist")
        .to_path_buf()
}

/// Insert `--agents-config` and `--models-config` flags immediately
/// after the subcommand name (which is `args[0]`). clap's parser
/// requires subcommand flags to come after the subcommand name, so
/// prepending them doesn't work. We return owned Strings because the
/// `--agents-config <path>` value isn't in the input slice — it's a
/// runtime-joined path.
fn inject_config_flags<'a>(args: &[&'a str], workspace: &std::path::Path) -> Vec<String> {
    let agents = workspace.join("config/agents");
    let models = workspace.join("config/models.toml");
    let mut out: Vec<String> = Vec::with_capacity(args.len() + 4);
    if let Some((first, rest)) = args.split_first() {
        out.push((*first).to_string());
        out.push("--agents-config".to_string());
        out.push(agents.to_string_lossy().into_owned());
        out.push("--models-config".to_string());
        out.push(models.to_string_lossy().into_owned());
        for a in rest {
            out.push((*a).to_string());
        }
    }
    out
}

// Suppress dead_code on the now-unused alias paths (artifact of the
// failed first attempt at this helper).

/// Snapshot the `~/.latte/logs/` directory (creating it if needed so
/// the read_dir doesn't fail on a fresh machine). Returns sorted
/// absolute paths.
fn list_chat_logs() -> Vec<PathBuf> {
    let home = match std::env::var_os("HOME") {
        Some(h) => PathBuf::from(h),
        None => return Vec::new(),
    };
    let dir = home.join(".latte/logs");
    let _ = std::fs::create_dir_all(&dir);
    let mut out: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort();
    out
}

/// Wait for `child` to exit, with a wall-clock timeout. Returns the
/// captured Output on success; on timeout, kills the child and
/// returns an error string.
fn wait_with_timeout(
    mut child: std::process::Child,
    timeout: Duration,
) -> Result<Output, String> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                if let Some(mut s) = child.stdout.take() {
                    use std::io::Read;
                    let _ = s.read_to_end(&mut stdout);
                }
                if let Some(mut s) = child.stderr.take() {
                    use std::io::Read;
                    let _ = s.read_to_end(&mut stderr);
                }
                return Ok(Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "timed out after {:?} (no exit)",
                        timeout
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("try_wait failed: {}", e)),
        }
    }
}

/// Helper: turn a list of JSONL events into the variant name (the
/// outer object's key) so tests don't have to spell out
/// `e.as_object().unwrap().keys().next().unwrap()` every time.
pub fn variant_name(event: &Value) -> Option<&str> {
    event
        .as_object()
        .and_then(|o| o.keys().next())
        .map(String::as_str)
}

/// Helper: pull the inner payload of an event of variant `K` from a
/// list. Returns None if no such variant was emitted.
pub fn events_of<'a>(events: &'a [Value], variant: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|e| variant_name(e) == Some(variant))
        .collect()
}

/// Helper: extract the inner payload object from a `{ "Variant": {…} }`
/// event. Returns None if the event isn't that variant.
pub fn payload<'a>(event: &'a Value, variant: &str) -> Option<&'a Value> {
    let obj = event.as_object()?;
    obj.get(variant)
}

/// Join an OsString array (used to format args for panic messages).
#[allow(dead_code)]
pub fn fmt_args(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}