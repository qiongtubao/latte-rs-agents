//! Built-in lifecycle hooks. See spec §7.4.
//!
//! Three hooks ship in v1:
//!   - `RedactPii`            : replaces phones, emails, AWS keys, OpenAI keys
//!                              in pre-call messages with `<REDACTED:type>`.
//!   - `EnforceToolAllowlist` : aborts post-parse if a tool call names a tool
//!                              outside the role's allowed list.
//!   - `RequireToolCall`      : aborts post-response if the model emits
//!                              neither a tool call nor a substantive answer.
//!
//! All three are no-state, no-allocation hooks. They implement `Hook`
//! by overriding exactly one method and returning the right `HookOutcome`.

use super::{Hook, HookOutcome, PostParseCtx, PostResponseCtx, PreCallCtx};

// ─── RedactPii ─────────────────────────────────────────────────────────

/// Pre-call hook. Walks every message in the outgoing list and replaces
/// common PII patterns with `<REDACTED:type>` placeholders. The
/// replacements are done in-place on `Message::content`.
///
/// PII types detected (hand-rolled regex, no external `regex` crate):
///   - `phone`  : 11 consecutive digits, optional `+86` / `86-` prefix.
///   - `email`  : `<local>@<domain>.<tld>` shape (conservative).
///   - `aws_key`: `AKIA[0-9A-Z]{16}` (AWS access key id format).
///   - `api_key`: `sk-` or `sk-ant-` prefix followed by 32+ alnum/dash/underscore
///               chars (OpenAI / Anthropic style).
///
/// False-positive tolerant: an 11-digit order ID may be misread as a
/// phone. That's recoverable (the user can read the redacted order
/// number) while false negatives (a leaked phone that wasn't redacted)
/// are not. To disable at runtime, omit the hook from `--debug-hooks`.
pub struct RedactPii;

impl Hook for RedactPii {
    fn name(&self) -> &str { "redact_pii" }

    fn pre_call(&self, ctx: &mut PreCallCtx) -> HookOutcome<()> {
        for m in ctx.messages.iter_mut() {
            // 提取文本内容，脱敏，再写回
            let text = m.as_text();
            let redacted = redact_text(&text);
            m.content = vec![latte_ai::models::ContentPart::text(redacted)];
        }
        HookOutcome::Continue
    }
}

/// Hand-rolled regex-style replacement. Scans byte-by-byte and
/// tries each pattern at every position. Returns the redacted string.
fn redact_text(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some(consumed) = try_match_aws(bytes, i) {
            out.push_str("<REDACTED:aws_key>");
            i += consumed;
        } else if let Some(consumed) = try_match_openai(bytes, i) {
            out.push_str("<REDACTED:api_key>");
            i += consumed;
        } else if let Some(consumed) = try_match_email(bytes, i) {
            out.push_str("<REDACTED:email>");
            i += consumed;
        } else if let Some(consumed) = try_match_phone(bytes, i) {
            out.push_str("<REDACTED:phone>");
            i += consumed;
        } else {
            // Copy one UTF-8 char (handles multi-byte safely).
            let ch = s[i..].chars().next().expect("non-empty remaining");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn try_match_aws(b: &[u8], i: usize) -> Option<usize> {
    if i + 20 > b.len() { return None; }
    if &b[i..i + 4] != b"AKIA" { return None; }
    for j in 4..20 {
        let c = b[i + j];
        // AKIA followed by 16 chars in [0-9A-Z].
        if !(c.is_ascii_digit() || (b'A'..=b'Z').contains(&c)) { return None; }
    }
    Some(20)
}

fn try_match_openai(b: &[u8], i: usize) -> Option<usize> {
    // sk-ant-... or sk-... prefix. Look for the longer match first.
    let prefix_len = if b[i..].starts_with(b"sk-ant-") { 7 }
                     else if b[i..].starts_with(b"sk-") { 3 }
                     else { return None; };
    let mut j = i + prefix_len;
    let mut count = 0;
    while j < b.len() && count < 200 {
        let c = b[j];
        if c.is_ascii_alphanumeric() || c == b'-' || c == b'_' { j += 1; count += 1; }
        else { break; }
    }
    if count >= 32 { Some(j - i) } else { None }
}

fn try_match_email(b: &[u8], i: usize) -> Option<usize> {
    // <local>@<domain>.<tld> — conservative shape: local part is
    // [A-Za-z0-9._+-]+, domain is [A-Za-z0-9.-]+ with at least one dot.
    let start = i;
    let mut j = i;
    while j < b.len() && is_email_local(b[j]) { j += 1; }
    if j == i { return None; }
    if j >= b.len() || b[j] != b'@' { return None; }
    j += 1;
    let domain_start = j;
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'.' || b[j] == b'-') { j += 1; }
    if j == domain_start { return None; }
    // Require at least one dot in the domain.
    if !b[domain_start..j].iter().any(|&c| c == b'.') { return None; }
    // TLD must end with 2+ letters.
    let last_dot = b[domain_start..j].iter().rposition(|&c| c == b'.')? + domain_start;
    let tld_len = j - last_dot - 1;
    if tld_len < 2 { return None; }
    Some(j - start)
}

fn is_email_local(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'.' || c == b'_' || c == b'-' || c == b'+'
}

fn try_match_phone(b: &[u8], i: usize) -> Option<usize> {
    // Optional +86 / 86- prefix, then exactly 11 ASCII digits.
    let mut j = i;
    if b[j..].starts_with(b"+86") { j += 3; }
    else if b[j..].starts_with(b"86-") { j += 3; }
    let digit_start = j;
    while j < b.len() && b[j].is_ascii_digit() { j += 1; }
    let n = j - digit_start;
    if n == 11 { Some(j - i) } else { None }
}

// ─── EnforceToolAllowlist ─────────────────────────────────────────────

/// Post-parse hook. Aborts if any parsed call names a tool that's
/// not in the role's allowed list. The allowlist is provided at
/// construction time; AgentRunner wires the role's `allowed_tools`
/// here (Task 12).
pub struct EnforceToolAllowlist {
    allowed: std::collections::HashSet<String>,
}

impl EnforceToolAllowlist {
    pub fn new(allowed: Vec<String>) -> Self {
        Self { allowed: allowed.into_iter().collect() }
    }

    /// Construct from anything string-like. Convenience for callers
    /// that have a `&[&str]` or a `Vec<String>`.
    pub fn from<I, S>(allowed: I) -> Self
    where I: IntoIterator<Item = S>, S: Into<String> {
        Self { allowed: allowed.into_iter().map(Into::into).collect() }
    }
}

impl Hook for EnforceToolAllowlist {
    fn name(&self) -> &str { "enforce_tool_allowlist" }

    fn post_parse(&self, ctx: &mut PostParseCtx) -> HookOutcome<Vec<crate::trace::ParsedCall>> {
        let bad: Vec<&str> = ctx.parsed.iter()
            .map(|c| c.name.as_str())
            .filter(|n| !self.allowed.contains(*n))
            .collect();
        if bad.is_empty() {
            HookOutcome::Continue
        } else {
            let mut allowlist: Vec<&str> = self.allowed.iter().map(|s| s.as_str()).collect();
            allowlist.sort_unstable();
            HookOutcome::Abort {
                reason: format!(
                    "tool(s) {:?} not in allowed list: {:?}",
                    bad, allowlist
                ),
            }
        }
    }
}

// ─── RequireToolCall ───────────────────────────────────────────────────

/// Post-response hook. Aborts if the model emitted neither a tool
/// call nor a substantive final answer.
///
/// Heuristic: a "substantive" response is one with at least
/// `min_words` whitespace-separated tokens. Responses containing
/// `<tool_call` are always accepted (the model is acting, not just
/// chatting).
pub struct RequireToolCall { min_words: usize }

impl Default for RequireToolCall {
    fn default() -> Self { Self { min_words: 20 } }
}

impl RequireToolCall {
    pub fn with_min_words(mut self, n: usize) -> Self {
        self.min_words = n;
        self
    }
}

impl Hook for RequireToolCall {
    fn name(&self) -> &str { "require_tool_call" }

    fn post_response(&self, ctx: &mut PostResponseCtx) -> HookOutcome<()> {
        if ctx.raw.contains("<tool_call") { return HookOutcome::Continue; }
        let word_count = ctx.raw.split_whitespace().count();
        if word_count >= self.min_words {
            HookOutcome::Continue
        } else {
            HookOutcome::Abort {
                reason: format!(
                    "expected tool call or substantive final answer (>= {} words), got {} words / {} chars",
                    self.min_words, word_count, ctx.raw.len()
                ),
            }
        }
    }
}

// ─── ContextMonitor ─────────────────────────────────────────────────────

/// Pre-call hook. Monitors the outgoing message list and aborts when
/// estimated token usage exceeds the configured thresholds (as fractions
/// of the session token budget).
///
/// `warn_at` / `abort_at` are fractions (0.0–1.0) of `token_budget`.
/// When estimated tokens exceed `warn_at * token_budget`, a warning is
/// logged via `eprintln!`. When they exceed `abort_at * token_budget`,
/// the hook returns `Abort` so the caller can compact or escalate.
pub struct ContextMonitor {
    token_budget: usize,
    warn_at: f64,
    abort_at: f64,
}

impl ContextMonitor {
    /// Create a new context monitor.
    ///
    /// # Panics
    /// If `warn_at > abort_at` or either is outside `[0.0, 1.0]`.
    pub fn new(token_budget: usize, warn_at: f64, abort_at: f64) -> Self {
        assert!(
            warn_at <= abort_at,
            "ContextMonitor: warn_at ({}) must not exceed abort_at ({})",
            warn_at,
            abort_at
        );
        assert!(
            (0.0..=1.0).contains(&warn_at),
            "ContextMonitor: warn_at must be in [0.0, 1.0], got {}",
            warn_at
        );
        assert!(
            (0.0..=1.0).contains(&abort_at),
            "ContextMonitor: abort_at must be in [0.0, 1.0], got {}",
            abort_at
        );
        Self { token_budget, warn_at, abort_at }
    }

    fn estimated_tokens(messages: &[latte_ai::models::Message]) -> usize {
        messages.iter().map(|m| m.as_text().len().div_ceil(4)).sum()
    }
}

impl Hook for ContextMonitor {
    fn name(&self) -> &str { "context_monitor" }

    fn pre_call(&self, ctx: &mut PreCallCtx) -> HookOutcome<()> {
        let estimated = Self::estimated_tokens(ctx.messages);
        let warn_threshold = (self.token_budget as f64 * self.warn_at) as usize;
        let abort_threshold = (self.token_budget as f64 * self.abort_at) as usize;

        if estimated >= abort_threshold {
            return HookOutcome::Abort {
                reason: format!(
                    "context monitor: estimated {} tokens >= abort threshold {} (budget={}, abort_at={})",
                    estimated, abort_threshold, self.token_budget, self.abort_at
                ),
            };
        }

        if estimated >= warn_threshold {
            // Warning: emit to stderr (operators can see it without
            // needing full trace mode). The hook continues.
            eprintln!(
                "[hook] context_monitor: estimated {} tokens >= warn threshold {} (budget={}, warn_at={})",
                estimated, warn_threshold, self.token_budget, self.warn_at
            );
        }

        HookOutcome::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::Message;
    use crate::trace::ParsedCall;
    use latte_ai::models::Role as MsgRole;

    fn msgs_with(text: &str) -> Vec<Message> {
        vec![Message::user(text)]
    }

    // ─── RedactPii tests ────────────────────────────────────────────

    #[test]
    fn redacts_chinese_phone() {
        let mut msgs = msgs_with("call me at 13812345678 today");
        let mut ctx = PreCallCtx { messages: &mut msgs };
        let _ = RedactPii.pre_call(&mut ctx);
        assert!(msgs[0].content.contains("<REDACTED:phone>"), "got: {}", msgs[0].content);
        assert!(!msgs[0].content.contains("13812345678"), "phone leaked: {}", msgs[0].content);
    }

    #[test]
    fn redacts_phone_with_country_prefix() {
        let mut msgs = msgs_with("call +8613812345678 today");
        let mut ctx = PreCallCtx { messages: &mut msgs };
        let _ = RedactPii.pre_call(&mut ctx);
        assert!(msgs[0].content.contains("<REDACTED:phone>"), "got: {}", msgs[0].content);
        assert!(!msgs[0].content.contains("13812345678"), "phone leaked: {}", msgs[0].content);
    }

    #[test]
    fn redacts_email() {
        let mut msgs = msgs_with("ping alice@example.com about it");
        let mut ctx = PreCallCtx { messages: &mut msgs };
        let _ = RedactPii.pre_call(&mut ctx);
        assert!(msgs[0].content.contains("<REDACTED:email>"), "got: {}", msgs[0].content);
        assert!(!msgs[0].content.contains("alice@example.com"), "email leaked: {}", msgs[0].content);
    }

    #[test]
    fn redacts_aws_key() {
        let mut msgs = msgs_with("AKIAIOSFODNN7EXAMPLE was the key");
        let mut ctx = PreCallCtx { messages: &mut msgs };
        let _ = RedactPii.pre_call(&mut ctx);
        assert!(msgs[0].content.contains("<REDACTED:aws_key>"), "got: {}", msgs[0].content);
        assert!(!msgs[0].content.contains("AKIAIOSFODNN7EXAMPLE"), "key leaked: {}", msgs[0].content);
    }

    #[test]
    fn redacts_openai_key() {
        let mut msgs = msgs_with("use sk-abcdef1234567890abcdef1234567890abcdef for auth");
        let mut ctx = PreCallCtx { messages: &mut msgs };
        let _ = RedactPii.pre_call(&mut ctx);
        assert!(msgs[0].content.contains("<REDACTED:api_key>"), "got: {}", msgs[0].content);
        assert!(!msgs[0].content.contains("sk-abcdef"), "key leaked: {}", msgs[0].content);
    }

    #[test]
    fn redacts_ant_style_key() {
        let mut msgs = msgs_with("use sk-ant-api03-abcdefghijklmnopqrstuvwxyz1234567890ABCD for auth");
        let mut ctx = PreCallCtx { messages: &mut msgs };
        let _ = RedactPii.pre_call(&mut ctx);
        assert!(msgs[0].content.contains("<REDACTED:api_key>"), "got: {}", msgs[0].content);
        assert!(!msgs[0].content.contains("sk-ant-api03"), "ant key leaked: {}", msgs[0].content);
    }

    #[test]
    fn redacts_multiple_pii_types() {
        let mut msgs = msgs_with(
            "email a@b.com or call 13900000000; key AKIAIOSFODNN7EXAMPLE; use sk-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa now"
        );
        let mut ctx = PreCallCtx { messages: &mut msgs };
        let _ = RedactPii.pre_call(&mut ctx);
        let s = &msgs[0].content;
        let count = s.matches("<REDACTED:").count();
        assert_eq!(count, 4, "expected 4 redactions (email, phone, aws, key) in: {}", s);
    }

    #[test]
    fn leaves_normal_text_alone() {
        let mut msgs = msgs_with("the quick brown fox jumps over 13 lazy dogs");
        let mut ctx = PreCallCtx { messages: &mut msgs };
        let _ = RedactPii.pre_call(&mut ctx);
        assert_eq!(msgs[0].as_text(), "the quick brown fox jumps over 13 lazy dogs");
    }

    #[test]
    fn redact_pii_on_call() {
        let mut msgs = vec![
            Message::user("call 13812345678"),
            Message::assistant("AKIAIOSFODNN7EXAMPLE leaked"),
        ];
        let mut ctx = PreCallCtx { messages: &mut msgs };
        let _ = RedactPii.pre_call(&mut ctx);
        assert!(msgs[0].as_text().contains("<REDACTED:phone>"));
        assert!(msgs[1].as_text().contains("<REDACTED:aws_key>"));
    }

    // ─── EnforceToolAllowlist tests ─────────────────────────────────

    #[test]
    fn enforce_allowlist_aborts_on_unknown_tool() {
        let h = EnforceToolAllowlist::new(vec!["read".into(), "exec".into()]);
        let mut parsed = vec![
            ParsedCall { name: "read".into(), args: "{}".into() },
            ParsedCall { name: "rm_rf".into(), args: "{}".into() },
        ];
        let mut ctx = PostParseCtx { parsed: &mut parsed };
        let outcome = h.post_parse(&mut ctx);
        match outcome {
            HookOutcome::Abort { reason } => {
                assert!(reason.contains("rm_rf"), "abort reason: {}", reason);
                assert!(reason.contains("read"), "abort lists allowlist");
            }
            other => panic!("expected Abort, got {:?}", other),
        }
    }

    #[test]
    fn enforce_allowlist_continues_when_all_known() {
        let h = EnforceToolAllowlist::new(vec!["read".into(), "exec".into()]);
        let mut parsed = vec![ParsedCall { name: "read".into(), args: "{}".into() }];
        let mut ctx = PostParseCtx { parsed: &mut parsed };
        let outcome = h.post_parse(&mut ctx);
        assert!(matches!(outcome, HookOutcome::Continue), "got {:?}", outcome);
    }

    #[test]
    fn enforce_allowlist_continues_on_empty_parsed() {
        let h = EnforceToolAllowlist::new(vec!["read".into()]);
        let mut parsed: Vec<ParsedCall> = vec![];
        let mut ctx = PostParseCtx { parsed: &mut parsed };
        let outcome = h.post_parse(&mut ctx);
        assert!(matches!(outcome, HookOutcome::Continue));
    }

    // ─── RequireToolCall tests ──────────────────────────────────────

    #[test]
    fn require_tool_call_aborts_on_empty_response() {
        let h = RequireToolCall::default();
        let mut ctx = PostResponseCtx { raw: "" };
        let outcome = h.post_response(&mut ctx);
        match outcome {
            HookOutcome::Abort { reason } => {
                assert!(reason.contains("expected tool call"), "got: {}", reason);
            }
            other => panic!("expected Abort, got {:?}", other),
        }
    }

    #[test]
    fn require_tool_call_aborts_on_short_response() {
        let h = RequireToolCall::default();
        let mut ctx = PostResponseCtx { raw: "ok" };
        let outcome = h.post_response(&mut ctx);
        assert!(matches!(outcome, HookOutcome::Abort { .. }));
    }

    #[test]
    fn require_tool_call_continues_on_substantive_response() {
        let h = RequireToolCall::default();
        let text: String = std::iter::repeat("word").take(25).collect::<Vec<_>>().join(" ");
        let mut ctx = PostResponseCtx { raw: &text };
        let outcome = h.post_response(&mut ctx);
        assert!(matches!(outcome, HookOutcome::Continue), "got {:?}", outcome);
    }

    #[test]
    fn require_tool_call_continues_when_response_has_tool_call_marker() {
        let h = RequireToolCall::default();
        let mut ctx = PostResponseCtx {
            raw: "thinking...\n<tool_callbash/>"
        };
        let outcome = h.post_response(&mut ctx);
        assert!(matches!(outcome, HookOutcome::Continue));
    }

    #[test]
    fn require_tool_call_min_words_override() {
        let h = RequireToolCall::default().with_min_words(2);
        let mut ctx = PostResponseCtx { raw: "two words" };
        let outcome = h.post_response(&mut ctx);
        assert!(matches!(outcome, HookOutcome::Continue));
    }
}
