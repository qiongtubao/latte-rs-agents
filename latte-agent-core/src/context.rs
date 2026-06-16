//! Conversation context management: message history, token budget tracking,
//! context summarization.

use latte_ai::models::Message;

/// Accumulated conversation context shared across discussion rounds.
///
/// Tracks message history and optional token budget. When the budget is exceeded,
/// older messages are summarized or pruned.
#[derive(Debug, Clone)]
pub struct ConversationContext {
    /// Full message history (newest last).
    messages: Vec<Message>,
    /// Estimated total token count.
    token_count: usize,
    /// Maximum token budget (0 = unlimited).
    token_budget: usize,
}

impl Default for ConversationContext {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            token_count: 0,
            token_budget: 0,
        }
    }
}

impl ConversationContext {
    /// Create a new context with an optional token budget.
    pub fn new(token_budget: usize) -> Self {
        Self {
            messages: Vec::new(),
            token_count: 0,
            token_budget,
        }
    }

    /// Add a message to the context.
    pub fn push(&mut self, message: Message) {
        self.token_count += estimate_tokens(&message.content);
        self.messages.push(message);
    }

    /// Add multiple messages at once.
    /// Add multiple messages at once.
    pub fn extend(&mut self, messages: impl IntoIterator<Item = Message>) {
        for msg in messages {
            self.push(msg);
        }
    }

    /// Set a new token budget.
    pub fn set_token_budget(&mut self, budget: usize) {
        self.token_budget = budget;
    }

    /// Get all messages in the context.
    pub fn messages(&self) -> &[Message] {
        &self.messages
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

    /// Clear all messages and reset token count.
    pub fn clear(&mut self) {
        self.messages.clear();
        self.token_count = 0;
    }

    /// Prune oldest messages until within token budget.
    /// Always keeps at least `keep_last` most recent messages.
    pub fn prune_to_budget(&mut self, keep_last: usize) {
        if self.token_budget == 0 || self.token_count <= self.token_budget {
            return;
        }

        let keep = keep_last.min(self.messages.len());
        let split_at = self.messages.len() - keep;

        // Remove from the front until under budget
        let mut removed = 0;
        for msg in &self.messages[..split_at] {
            removed += estimate_tokens(&msg.content);
        }

        self.messages = self.messages[split_at..].to_vec();
        self.token_count -= removed;
    }

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

    /// Replace all messages with a single summary message.
    pub fn summarize_with(&mut self, summary: &str) {
        self.clear();
        self.push(Message {
            role: latte_ai::models::Role::System,
            content: format!("[Previous conversation summary]\n{}", summary),
        });
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
            content: content.into(),
        }
    }

    #[test]
    fn test_push_and_count() {
        let mut ctx = ConversationContext::default();
        ctx.push(make_msg("Hello world")); // 11 chars → 3 tokens
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
        ctx.push(make_msg(&"a".repeat(400))); // ~100 tokens
        assert!(ctx.budget_exceeded());
    }

    #[test]
    fn test_prune_to_budget() {
        // Budget of 10 tokens — messages total ~52 tokens, so pruning needed
        let mut ctx = ConversationContext::new(10);
        ctx.push(make_msg("short"));
        ctx.push(make_msg(&"a".repeat(200))); // ~50 tokens, pushes over budget
        ctx.prune_to_budget(1);
        // After pruning, only the last message remains (1 message, ~50 tokens)
        // Note: remaining tokens may still exceed budget since a single message
        // can't be further pruned past keep_last=1
        assert_eq!(ctx.messages().len(), 1);
    }
}
