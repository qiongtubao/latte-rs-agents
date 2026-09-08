//! Tool-fix human intervention (Scheme A).
//!
//! When the subsession repair (Scheme B) exhausts its attempts AND the model
//! keeps emitting malformed tool args or deterministic validation failures,
//! we surface a popup (HIL) so the user can fix the JSON/input directly.
//! On submit we prune the failed retry rounds out of the conversation history
//! so the model sees only the corrected call + its successful result.

use crate::controller::ChatEvent;
use latte_ai::models::Message;

/// How many failed repair rounds before we surface the human-intervention popup.
pub const FIX_BREAK_AT: usize = crate::agent::PERMANENT_BREAK_AT;

/// A pending human-fix request awaiting the user's corrected JSON/input.
#[derive(Debug, Clone)]
pub struct FixRequest {
    pub tool_name: String,
    pub malformed_args: String,
    pub error_detail: String,
    /// The index (into the agent's message history) where the first failed
    /// attempt round begins. Used to prune that whole blob on success.
    pub first_failed_idx: usize,
}

/// Context for a tool-fix intervention. Constructed by the controller and
/// threaded into the runner so it can surface `ToolFixRequested` events.
#[derive(Clone, Default)]
pub struct ToolFixContext {
    /// Number of consecutive malformed/validation failures already observed.
    pub malformed_streak: usize,
    /// Raw JSON that is malformed (most recent attempt).
    pub last_malformed_args: Option<String>,
    /// Last error detail.
    pub last_error: Option<String>,
    /// Index of the first failed round in history (for pruning).
    pub first_failed_idx: Option<usize>,
}

/// Classify whether a `MalformedArgs` / `PermanentExec` error has hit the
/// intervention threshold.
pub fn should_intervene(context: &ToolFixContext) -> bool {
    context.malformed_streak >= FIX_BREAK_AT
}

/// Build the `ChatEvent` that the frontend renders as a fix popup.
pub fn build_fix_event(
    role_id: &str,
    choice_id: &str,
    req: &FixRequest,
    wait: bool,
) -> ChatEvent {
    ChatEvent::ToolFixRequested {
        role_id: role_id.to_string(),
        choice_id: choice_id.to_string(),
        tool_name: req.tool_name.clone(),
        malformed_args: req.malformed_args.clone(),
        error_detail: req.error_detail.clone(),
        wait,
    }
}

/// Prune the failed retry rounds out of the messages vec and inject a single
/// corrected tool-call note so the model continues with a clean view.
pub fn prune_failed_rounds(
    messages: &mut Vec<Message>,
    first_failed_idx: usize,
    corrected_args: &str,
) {
    if first_failed_idx >= messages.len() {
        return;
    }
    messages.truncate(first_failed_idx);
    messages.push(Message::user(format!(
        "[工具参数已由人工修正并执行结果如下]\n修正后的参数: {corrected_args}"
    )));
}
