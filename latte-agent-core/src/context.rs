//! Conversation context management: message history, token budget tracking,
//! context summarization, and per-message importance for compaction.

use latte_ai::models::Message;
use serde::{Deserialize, Serialize};

/// Per-message importance — controls **compaction order**.
///
/// `prune_to_budget` drops messages in ascending importance order, so
/// `Low` messages are dropped first, `Critical` is preserved unless it
/// is the only thing in the context. Ordinal ordering is the source
/// of truth for "what to drop first"; this enum is `Ord`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum Importance {
    /// Drop first when compacting (chatty filler, intermediate reasoning).
    Low,
    /// Default for newly-pushed messages.
    #[default]
    Normal,
    /// Keep over Low/Normal even when over budget.
    High,
    /// Only dropped as a last resort (and even then, `prune_to_budget`
    /// never drops a Critical to make room — caller must do it).
    Critical,
}

/// Accumulated conversation context shared across discussion rounds.
///
/// Tracks message history, optional token budget, and per-message
/// importance. When the budget is exceeded, `prune_to_budget` drops
/// the least-important oldest messages first.
///
/// `messages` and `importance` are kept in lockstep: every operation
/// that mutates one mutates the other. `messages_mut` is a notable
/// exception — it hands out `&mut [Message]` for ergonomic in-place
/// edits; callers must call [`recompute_token_count`] afterwards if
/// any mutation could have changed content length, and adjust
/// importance via the explicit APIs.
#[derive(Debug, Clone, Default)]
pub struct ConversationContext {
    /// Full message history (newest last).
    messages: Vec<Message>,
    /// Per-message importance, indexed identically to `messages`.
    importance: Vec<Importance>,
    /// Estimated total token count.
    token_count: usize,
    /// Maximum token budget (0 = unlimited).
    token_budget: usize,
}

impl ConversationContext {
    /// Create a new context with an optional token budget.
    pub fn new(token_budget: usize) -> Self {
        Self {
            messages: Vec::new(),
            importance: Vec::new(),
            token_count: 0,
            token_budget,
        }
    }

    // ─── Insert ────────────────────────────────────────────────────────

    /// Add a message to the context with default `Normal` importance.
    pub fn push(&mut self, message: Message) {
        let tokens = estimate_tokens(&message.content);
        self.messages.push(message);
        self.importance.push(Importance::Normal);
        self.token_count += tokens;
    }

    /// Add a message with explicit importance.
    ///
    /// Use this when you know a message should be preserved
    /// (`High`/`Critical`) or is expendable (`Low`) at compaction time.
    pub fn push_with_importance(&mut self, message: Message, importance: Importance) {
        let tokens = estimate_tokens(&message.content);
        self.messages.push(message);
        self.importance.push(importance);
        self.token_count += tokens;
    }

    /// Add multiple messages at once (all with `Normal` importance).
    pub fn extend(&mut self, messages: impl IntoIterator<Item = Message>) {
        for msg in messages {
            self.push(msg);
        }
    }

    /// Add multiple messages with their explicit importances.
    ///
    /// The two iterators must yield the same number of items; a length
    /// mismatch is a logic error and panics (same as `zip`).
    pub fn extend_with_importance(
        &mut self,
        messages: impl IntoIterator<Item = Message>,
        importances: impl IntoIterator<Item = Importance>,
    ) {
        for (m, i) in messages.into_iter().zip(importances) {
            self.push_with_importance(m, i);
        }
    }

    // ─── Configuration ────────────────────────────────────────────────

    /// Set a new token budget.
    pub fn set_token_budget(&mut self, budget: usize) {
        self.token_budget = budget;
    }

    // ─── Read ─────────────────────────────────────────────────────────

    /// Get all messages in the context.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Get the importance of a single message, or `None` if out of range.
    pub fn importance(&self, idx: usize) -> Option<Importance> {
        self.importance.get(idx).copied()
    }

    /// Snapshot of all importances, in the same order as `messages`.
    pub fn importances(&self) -> &[Importance] {
        &self.importance
    }

    /// Current estimated token count.
    pub fn token_count(&self) -> usize {
        self.token_count
    }

    /// Token budget (0 = unlimited).
    pub fn token_budget(&self) -> usize {
        self.token_budget
    }

    /// Whether the token budget is exceeded.
    pub fn budget_exceeded(&self) -> bool {
        self.token_budget > 0 && self.token_count >= self.token_budget
    }

    // ─── Mutation: index-based ────────────────────────────────────────

    /// Remove the message at `idx` and return it. Returns `None` if
    /// `idx` is out of range.
    pub fn remove(&mut self, idx: usize) -> Option<Message> {
        if idx >= self.messages.len() {
            return None;
        }
        self.token_count = self
            .token_count
            .saturating_sub(estimate_tokens(&self.messages[idx].content));
        self.importance.remove(idx);
        Some(self.messages.remove(idx))
    }

    /// Replace the message at `idx` and return the old one. Token count
    /// is updated. The replacement defaults to `Normal` importance.
    /// Returns `None` if `idx` is out of range.
    pub fn replace(&mut self, idx: usize, new: Message) -> Option<Message> {
        if idx >= self.messages.len() {
            return None;
        }
        let old_tokens = estimate_tokens(&self.messages[idx].content);
        let new_tokens = estimate_tokens(&new.content);
        self.token_count = self
            .token_count
            .saturating_sub(old_tokens)
            .saturating_add(new_tokens);
        self.importance[idx] = Importance::Normal;
        Some(std::mem::replace(&mut self.messages[idx], new))
    }

    /// Apply `f` to the message at `idx` in place. Returns `true` if
    /// the index was valid. **Token count is NOT recalculated** —
    /// `f` is expected to perform surgical edits that don't change
    /// length (e.g. fix a typo, redact a substring). If `f` may
    /// change the content length, use [`mutate_with_recompute`].
    pub fn mutate<F>(&mut self, idx: usize, f: F) -> bool
    where
        F: FnOnce(&mut Message),
    {
        let Some(msg) = self.messages.get_mut(idx) else {
            return false;
        };
        f(msg);
        true
    }

    /// Like [`mutate`] but recomputes the token delta based on the
    /// length change of `msg.content`.
    pub fn mutate_with_recompute<F>(&mut self, idx: usize, f: F) -> bool
    where
        F: FnOnce(&mut Message),
    {
        let Some(msg) = self.messages.get_mut(idx) else {
            return false;
        };
        let old_tokens = estimate_tokens(&msg.content);
        f(msg);
        let new_tokens = estimate_tokens(&msg.content);
        self.token_count = self
            .token_count
            .saturating_sub(old_tokens)
            .saturating_add(new_tokens);
        true
    }

    /// Remove a contiguous range of messages. Returns how many were
    /// actually removed (clamped to the current length).
    pub fn clear_range(&mut self, range: std::ops::Range<usize>) -> usize {
        let start = range.start.min(self.messages.len());
        let end = range.end.min(self.messages.len()).max(start);
        let count = end - start;
        if count == 0 {
            return 0;
        }
        let removed_tokens: usize = self.messages[start..end]
            .iter()
            .map(|m| estimate_tokens(&m.content))
            .sum();
        self.messages.drain(start..end);
        self.importance.drain(start..end);
        self.token_count = self.token_count.saturating_sub(removed_tokens);
        count
    }

    // ─── Mutation: predicate-based ────────────────────────────────────

    /// Remove every message matching `pred`. Returns the number removed.
    pub fn remove_where<F>(&mut self, pred: F) -> usize
    where
        F: Fn(&Message) -> bool,
    {
        let mut removed = 0usize;
        let mut removed_tokens = 0usize;
        // Walk back-to-front so removals don't shift indices.
        for i in (0..self.messages.len()).rev() {
            if pred(&self.messages[i]) {
                removed_tokens += estimate_tokens(&self.messages[i].content);
                self.messages.remove(i);
                self.importance.remove(i);
                removed += 1;
            }
        }
        self.token_count = self.token_count.saturating_sub(removed_tokens);
        removed
    }

    /// For every message matching `pred`, replace it with the message
    /// produced by `build(old)`. Returns the number of replacements.
    /// Token count is updated per replacement; replacements reset to
    /// `Normal` importance.
    pub fn replace_where<F, B>(&mut self, pred: F, build: B) -> usize
    where
        F: Fn(&Message) -> bool,
        B: Fn(&Message) -> Message,
    {
        let mut replaced = 0usize;
        for i in 0..self.messages.len() {
            if pred(&self.messages[i]) {
                let new_msg = build(&self.messages[i]);
                let old_tokens = estimate_tokens(&self.messages[i].content);
                let new_tokens = estimate_tokens(&new_msg.content);
                self.token_count = self
                    .token_count
                    .saturating_sub(old_tokens)
                    .saturating_add(new_tokens);
                self.messages[i] = new_msg;
                self.importance[i] = Importance::Normal;
                replaced += 1;
            }
        }
        replaced
    }

    // ─── Mutation: direct ─────────────────────────────────────────────

    /// Mutable access to the underlying messages slice.
    ///
    /// **`token_count` is NOT auto-recomputed** — any length-changing
    /// edit must be followed by [`recompute_token_count`]. Edits that
    /// preserve length (typo fix, substring redaction) can skip it.
    ///
    /// The `importance` sidecar is unchanged regardless; if you've
    /// changed message positions or want to retag, use the explicit
    /// [`set_importance`] / [`importance_mut`] APIs.
    pub fn messages_mut(&mut self) -> &mut [Message] {
        &mut self.messages
    }

    /// Mutable access to the importance sidecar.
    pub fn importance_mut(&mut self) -> &mut [Importance] {
        &mut self.importance
    }

    /// Recompute `token_count` from the current message contents.
    /// Use after bulk edits via `messages_mut`.
    pub fn recompute_token_count(&mut self) {
        self.token_count = self
            .messages
            .iter()
            .map(|m| estimate_tokens(&m.content))
            .sum();
    }

    // ─── Importance setter ────────────────────────────────────────────

    /// Set the importance of message at `idx`. Returns whether the
    /// index was valid.
    pub fn set_importance(&mut self, idx: usize, importance: Importance) -> bool {
        if let Some(slot) = self.importance.get_mut(idx) {
            *slot = importance;
            true
        } else {
            false
        }
    }

    /// Set the importance of every message matching `pred`. Returns
    /// the number of messages re-tagged.
    pub fn set_importance_where<F>(&mut self, pred: F, importance: Importance) -> usize
    where
        F: Fn(&Message) -> bool,
    {
        let mut n = 0;
        for (i, m) in self.messages.iter().enumerate() {
            if pred(m) {
                self.importance[i] = importance;
                n += 1;
            }
        }
        n
    }

    // ─── Wipe ─────────────────────────────────────────────────────────

    /// Clear all messages and reset token count.
    pub fn clear(&mut self) {
        self.messages.clear();
        self.importance.clear();
        self.token_count = 0;
    }

    // ─── Compaction ───────────────────────────────────────────────────

    /// Prune messages until under token budget.
    ///
    /// Drop order: ascending `Importance` first, then oldest first
    /// (lowest index). `Critical` is **never** dropped automatically
    /// — if `Critical` alone exceeds the budget, the prune stops
    /// without fully clearing the budget. Always preserves at least
    /// `keep_last` most-recent messages, even if it means violating
    /// the budget.
    pub fn prune_to_budget(&mut self, keep_last: usize) {
        if self.token_budget == 0 || self.token_count <= self.token_budget {
            return;
        }
        let n = self.messages.len();
        if n == 0 {
            return;
        }

        // Build the candidate drop list: every index that is NOT
        // in the protected "keep_last" tail, paired with its
        // importance. We then sort by (importance asc, index asc) so
        // that low-importance and old messages come first.
        let keep_tail_start = n.saturating_sub(keep_last);
        let mut candidates: Vec<(usize, Importance)> = (0..keep_tail_start)
            .map(|i| (i, self.importance[i]))
            .collect();

        // Never auto-drop Critical.
        candidates.retain(|(_, imp)| *imp != Importance::Critical);

        // Drop the most expendable first; ties broken by oldest first.
        candidates.sort_by_key(|(idx, imp)| (*imp, *idx));

        // Walk the sorted list, dropping until under budget.
        let mut to_drop: Vec<usize> = Vec::new();
        let mut running = self.token_count;
        for (idx, _) in &candidates {
            if running <= self.token_budget {
                break;
            }
            let t = estimate_tokens(&self.messages[*idx].content);
            running -= t;
            to_drop.push(*idx);
        }

        // Delete in descending index order so removals don't shift
        // the indices of items we still want to drop.
        to_drop.sort_unstable_by(|a, b| b.cmp(a));
        for idx in to_drop {
            self.messages.remove(idx);
            self.importance.remove(idx);
        }
        self.token_count = running;
    }

    // ─── Summary ──────────────────────────────────────────────────────

    /// Create a summary prompt for context that was pruned.
    /// Returns `None` if no pruning is needed.
    pub fn build_summary_request(&self) -> Option<String> {
        if self.token_budget == 0 || self.token_count <= self.token_budget {
            return None;
        }
        Some(format!(
            "Summarize the conversation above in under {} tokens, preserving key decisions and facts.",
            self.token_budget / 2
        ))
    }

    /// Replace all messages with a single summary message marked
    /// `High` importance so it survives a subsequent `prune_to_budget`.
    pub fn summarize_with(&mut self, summary: &str) {
        self.clear();
        self.push_with_importance(
            Message {
                role: latte_ai::models::Role::System,
                content: format!("[Previous conversation summary]\n{}", summary),
            },
            Importance::High,
        );
    }

    /// Get the system message if one exists (first message, role=System).
    pub fn system_message(&self) -> Option<&Message> {
        self.messages
            .first()
            .filter(|m| matches!(m.role, latte_ai::models::Role::System))
    }
}

/// Rough token estimation: ~4 chars per token for English text.
fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use latte_ai::models::Role;

    fn make_msg(content: &str) -> Message {
        Message {
            role: Role::User,
            content: content.to_string(),
        }
    }

    #[test]
    fn test_push_and_count() {
        let mut ctx = ConversationContext::default();
        ctx.push(make_msg("Hello world"));
        assert_eq!(ctx.messages().len(), 1);
        assert!(ctx.token_count() > 0);
    }

    #[test]
    fn test_budget_not_exceeded_when_unlimited() {
        let mut ctx = ConversationContext::new(0);
        for _ in 0..100 {
            ctx.push(make_msg("padding padding padding padding"));
        }
        assert!(!ctx.budget_exceeded());
    }

    #[test]
    fn test_budget_exceeded() {
        let mut ctx = ConversationContext::new(50);
        ctx.push(make_msg(&"a".repeat(400)));
        assert!(ctx.budget_exceeded());
    }

    #[test]
    fn test_prune_to_budget() {
        let mut ctx = ConversationContext::new(10);
        ctx.push(make_msg("short"));
        ctx.push(make_msg(&"a".repeat(200)));
        ctx.prune_to_budget(1);
        assert_eq!(ctx.messages().len(), 1);
    }

    // ─── Importance defaults + sidecar consistency ───────────────────

    #[test]
    fn test_default_importance_is_normal() {
        let mut ctx = ConversationContext::default();
        ctx.push(make_msg("a"));
        assert_eq!(ctx.importance(0), Some(Importance::Normal));
    }

    #[test]
    fn test_push_with_importance_records_value() {
        let mut ctx = ConversationContext::default();
        ctx.push_with_importance(make_msg("a"), Importance::Critical);
        ctx.push_with_importance(make_msg("b"), Importance::Low);
        assert_eq!(ctx.importance(0), Some(Importance::Critical));
        assert_eq!(ctx.importance(1), Some(Importance::Low));
    }

    #[test]
    fn test_sidecar_stays_in_sync_with_clear() {
        let mut ctx = ConversationContext::default();
        ctx.push(make_msg("a"));
        ctx.push(make_msg("b"));
        ctx.clear();
        assert!(ctx.messages().is_empty());
        assert!(ctx.importances().is_empty());
        assert_eq!(ctx.token_count(), 0);
    }

    #[test]
    fn test_summarize_with_marks_high_importance() {
        let mut ctx = ConversationContext::default();
        ctx.push(make_msg("prior"));
        ctx.summarize_with("the gist");
        assert_eq!(ctx.messages().len(), 1);
        assert_eq!(ctx.importance(0), Some(Importance::High));
    }

    // ─── Mutation: index-based ────────────────────────────────────────

    #[test]
    fn test_remove_returns_message_and_updates_count() {
        let mut ctx = ConversationContext::default();
        ctx.push(make_msg("a"));
        ctx.push(make_msg("b"));
        let before = ctx.token_count();
        let removed = ctx.remove(0);
        assert_eq!(removed.unwrap().content, "a");
        assert_eq!(ctx.messages().len(), 1);
        assert_eq!(ctx.messages()[0].content, "b");
        assert!(ctx.token_count() < before);
    }

    #[test]
    fn test_remove_out_of_range_returns_none() {
        let mut ctx = ConversationContext::default();
        assert!(ctx.remove(0).is_none());
    }

    #[test]
    fn test_replace_updates_token_count_and_resets_importance() {
        let mut ctx = ConversationContext::default();
        ctx.push_with_importance(make_msg("a"), Importance::High);
        let old = ctx.replace(0, make_msg("aaaaa"));
        assert_eq!(old.unwrap().content, "a");
        assert_eq!(ctx.importance(0), Some(Importance::Normal));
        assert!(ctx.token_count() > 0);
    }

    #[test]
    fn test_mutate_runs_closure_in_place() {
        let mut ctx = ConversationContext::default();
        ctx.push(make_msg("hello world"));
        let ok = ctx.mutate(0, |m| m.content = m.content.replace("world", "rust"));
        assert!(ok);
        assert_eq!(ctx.messages()[0].content, "hello rust");
    }

    #[test]
    fn test_mutate_with_recompute_adjusts_token_count() {
        let mut ctx = ConversationContext::default();
        ctx.push(make_msg("hi"));
        let before = ctx.token_count();
        ctx.mutate_with_recompute(0, |m| m.content = m.content.repeat(100));
        let after = ctx.token_count();
        assert!(after > before * 10);
    }

    #[test]
    fn test_clear_range_partial() {
        let mut ctx = ConversationContext::default();
        for c in ['a', 'b', 'c', 'd'] {
            ctx.push(make_msg(&c.to_string()));
        }
        let removed = ctx.clear_range(1..3);
        assert_eq!(removed, 2);
        assert_eq!(ctx.messages().len(), 2);
        assert_eq!(ctx.messages()[0].content, "a");
        assert_eq!(ctx.messages()[1].content, "d");
        assert_eq!(ctx.importance(0), Some(Importance::Normal));
        assert_eq!(ctx.importance(1), Some(Importance::Normal));
    }

    #[test]
    fn test_clear_range_clamps_out_of_bounds() {
        let mut ctx = ConversationContext::default();
        ctx.push(make_msg("a"));
        ctx.push(make_msg("b"));
        let removed = ctx.clear_range(1..100);
        assert_eq!(removed, 1);
        assert_eq!(ctx.messages().len(), 1);
    }

    // ─── Mutation: predicate-based ────────────────────────────────────

    #[test]
    fn test_remove_where_drops_matching_messages() {
        let mut ctx = ConversationContext::default();
        ctx.push(make_msg("keep"));
        ctx.push(make_msg("drop me"));
        ctx.push(make_msg("keep"));
        ctx.push(make_msg("drop me too"));
        let n = ctx.remove_where(|m| m.content.contains("drop"));
        assert_eq!(n, 2);
        assert_eq!(ctx.messages().len(), 2);
        assert!(ctx.messages().iter().all(|m| !m.content.contains("drop")));
        assert_eq!(ctx.importances().len(), 2);
    }

    #[test]
    fn test_replace_where_rebuilds_matching_messages() {
        let mut ctx = ConversationContext::default();
        ctx.push(make_msg("foo bar"));
        ctx.push(make_msg("foo baz"));
        ctx.push(make_msg("qux"));
        let n = ctx.replace_where(
            |m| m.content.starts_with("foo"),
            |old| make_msg(&old.content.replace("foo", "FOO")),
        );
        assert_eq!(n, 2);
        assert_eq!(ctx.messages()[0].content, "FOO bar");
        assert_eq!(ctx.messages()[1].content, "FOO baz");
        assert_eq!(ctx.messages()[2].content, "qux");
    }

    // ─── Mutation: direct ─────────────────────────────────────────────

    #[test]
    fn test_messages_mut_and_recompute() {
        let mut ctx = ConversationContext::default();
        ctx.push(make_msg("hi"));
        ctx.push(make_msg("there"));
        ctx.messages_mut()[0].content = "x".repeat(400);
        ctx.recompute_token_count();
        assert!(ctx.token_count() >= 100);
    }

    // ─── Importance setter ────────────────────────────────────────────

    #[test]
    fn test_set_importance_where_retags_matching() {
        let mut ctx = ConversationContext::default();
        ctx.push(make_msg("system prompt"));
        ctx.push(make_msg("user chat"));
        ctx.push(make_msg("user chat"));
        let n = ctx.set_importance_where(|m| m.content == "user chat", Importance::Low);
        assert_eq!(n, 2);
        assert_eq!(ctx.importance(0), Some(Importance::Normal));
        assert_eq!(ctx.importance(1), Some(Importance::Low));
        assert_eq!(ctx.importance(2), Some(Importance::Low));
    }

    // ─── Compact-by-importance ────────────────────────────────────────

    #[test]
    fn test_prune_drops_low_before_normal() {
        // Each message is ~50 tokens (200 chars / 4). Budget 100: room
        // for 2 messages, so the Low-importance one (oldest) goes first.
        let mut ctx = ConversationContext::new(100);
        ctx.push_with_importance(make_msg(&"a".repeat(200)), Importance::Normal);
        ctx.push_with_importance(make_msg(&"b".repeat(200)), Importance::Low);
        ctx.push_with_importance(make_msg(&"c".repeat(200)), Importance::Normal);
        ctx.prune_to_budget(0);

        let remaining: Vec<&str> =
            ctx.messages().iter().map(|m| m.content.as_str()).collect();
        assert_eq!(remaining.len(), 2, "expected 2 kept, got {remaining:?}");
        assert!(
            remaining.iter().all(|m| !m.starts_with('b')),
            "Low-importance message should have been dropped first: {remaining:?}"
        );
        assert_eq!(ctx.importances().len(), 2);
    }

    #[test]
    fn test_prune_preserves_critical() {
        let mut ctx = ConversationContext::new(10);
        ctx.push_with_importance(make_msg(&"a".repeat(200)), Importance::Normal);
        ctx.push_with_importance(make_msg(&"b".repeat(200)), Importance::Critical);
        ctx.push_with_importance(make_msg(&"c".repeat(200)), Importance::Normal);
        ctx.prune_to_budget(0);
        let surviving_importance: Vec<Importance> = ctx.importances().to_vec();
        assert!(
            surviving_importance.contains(&Importance::Critical),
            "Critical was dropped: {surviving_importance:?}"
        );
    }
}
