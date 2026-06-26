//! Black-box integration tests for the `latte-agent` CLI.
//!
//! These run against the real binary in `target/debug/latte-agent`,
//! NOT mocks. Each test creates its own `LATTE_HOME` via `TestEnv`
//! and asserts on stderr / exit code / trace JSONL.
//!
//! ## Running
//!
//! Default (no API key needed, no model calls, fast):
//!
//! ```text
//! cargo test --test blackbox
//! ```
//!
//! Full (requires `DEEPSEEK_API_KEY`, makes real model calls):
//!
//! ```text
//! cargo test --test blackbox -- --ignored --test-threads=1
//! ```
//!
//! Tests marked `#[ignore]` need `DEEPSEEK_API_KEY` to verify model
//! responses end-to-end. The pipeline-shape tests (Test 5 CJK no
//! panic, Test 6 hook fires, Test 7 FilterSink, Test 9 LATTE_HOME
//! override, Test 10 resume) run by default and don't burn tokens —
//! even without an API key they verify the binary's plumbing
//! produces the right trace shape.

#[path = "blackbox/common/mod.rs"]
mod common;

#[path = "blackbox/mod.rs"]
mod tests;