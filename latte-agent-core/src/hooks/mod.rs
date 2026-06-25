//! Lifecycle hooks for AgentRunner. See spec §7.
//!
//! A hook can observe, mutate, or abort the agent at five points:
//!   PreCall, PostResponse, PostParse, PreTool, PostTool.
//!
//! The default no-op chain is `HookChain::empty()`. Three built-in
//! hooks (RedactPii, EnforceToolAllowlist, RequireToolCall) ship in
//! Tasks 6-8; future work can add more (Task 12 wires them via
//! `--debug-hooks <names>`).

use std::sync::Arc;

use serde_json::Value;

use crate::trace::{HookPoint, ParsedCall};
/// Re-export so consumers can construct `Message` without reaching
/// into `latte_ai::models` themselves.
pub use latte_ai::models::Message;

// ─── Per-hook-point context ───────────────────────────────────────────

/// Pre-send hook. The `&mut` lets the hook mutate the outgoing
/// message list (e.g. redact PII, add a guard rail).
pub struct PreCallCtx<'a> { pub messages: &'a mut Vec<Message> }

/// Post-model hook. The hook sees the raw model output (including
/// the tool-call markers). It can abort (e.g. on empty/short output)
/// but cannot mutate the text in v1.
pub struct PostResponseCtx<'a> { pub raw: &'a str }

/// Post-parse hook. The hook sees the parsed tool-call list. It can
/// mutate (e.g. drop a disallowed call) or abort (e.g. unknown tool).
pub struct PostParseCtx<'a> { pub parsed: &'a mut Vec<ParsedCall> }

/// Pre-execute hook. The hook sees the resolved tool name and the
/// parsed args; can mutate args or abort.
pub struct PreToolCtx<'a> { pub name: &'a str, pub args: &'a mut Value }

/// Post-execute hook. The hook sees the tool result; can mutate the
/// result string (e.g. truncate large output) or abort.
pub struct PostToolCtx<'a> { pub name: &'a str, pub result: &'a mut String }

// ─── Outcome ───────────────────────────────────────────────────────────

/// Result of running a hook at a given point.
///
/// - `Continue`: no change, proceed.
/// - `Mutate(T)`: replace the value, proceed.
/// - `Abort { reason }`: stop run_turn, return AgentError::HookAborted.
/// - `Retry { correction }`: reserved for the future Checkpoint spec.
///   In v1 this behaves like `Continue` (logged) so hooks can be
///   written without crashing the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome<T> {
    Continue,
    Mutate(T),
    Abort { reason: String },
    Retry { correction: String },
}

impl<T> HookOutcome<T> {
    /// Stable string label for the outcome kind, used by HookFired
    /// TraceEvents so the trace store can record what each hook did.
    pub fn kind(&self) -> &'static str {
        match self {
            HookOutcome::Continue => "continue",
            HookOutcome::Mutate(_) => "mutate",
            HookOutcome::Abort { .. } => "abort",
            HookOutcome::Retry { .. } => "retry",
        }
    }

    /// If the outcome is `Mutate`, return the value. Returns `None`
    /// for Continue / Abort / Retry.
    pub fn into_mutate(self) -> Option<T> {
        match self {
            HookOutcome::Mutate(v) => Some(v),
            _ => None,
        }
    }
}

// ─── Hook trait + chain ───────────────────────────────────────────────

/// Implement on a struct to add lifecycle behavior to an AgentRunner.
/// All methods have a default of `HookOutcome::Continue` so a hook
/// only needs to override the points it cares about.
pub trait Hook: Send + Sync {
    fn name(&self) -> &str;

    fn pre_call(&self, _ctx: &mut PreCallCtx) -> HookOutcome<()> { HookOutcome::Continue }
    fn post_response(&self, _ctx: &mut PostResponseCtx) -> HookOutcome<()> { HookOutcome::Continue }
    fn post_parse(&self, _ctx: &mut PostParseCtx) -> HookOutcome<Vec<ParsedCall>> { HookOutcome::Continue }
    fn pre_tool(&self, _ctx: &mut PreToolCtx) -> HookOutcome<Value> { HookOutcome::Continue }
    fn post_tool(&self, _ctx: &mut PostToolCtx) -> HookOutcome<String> { HookOutcome::Continue }
}

/// Ordered chain of hooks. Composition rules:
/// - `Continue` and `Retry { .. }` proceed to the next hook (Retry
///   is logged but treated like Continue in v1).
/// - `Abort { reason }` short-circuits the chain; the caller gets
///   the first Abort's reason.
/// - `Mutate(v)` is composed: the latest Mutate value is what the
///   next hook sees. The chain returns the last Mutate (or Continue
///   if no hook returned Mutate).
///
/// Each `run_*` method takes an `on_fire` callback so the caller can
/// emit a HookFired TraceEvent for every hook that fired (regardless
/// of outcome).
pub struct HookChain {
    hooks: Vec<Arc<dyn Hook>>,
}

impl HookChain {
    pub fn empty() -> Self { Self { hooks: vec![] } }
    pub fn push(mut self, h: Arc<dyn Hook>) -> Self { self.hooks.push(h); self }
    pub fn len(&self) -> usize { self.hooks.len() }
    pub fn is_empty(&self) -> bool { self.hooks.is_empty() }
    pub fn hooks(&self) -> &[Arc<dyn Hook>] { &self.hooks }

    /// Run all pre_call hooks. Returns the first Abort or the last
    /// Mutate; Continue if no hook returned anything interesting.
    pub fn run_pre_call(
        &self,
        ctx: &mut PreCallCtx,
        mut on_fire: impl FnMut(&str, HookPoint, &str),
    ) -> HookOutcome<()> {
        for h in &self.hooks {
            let outcome = h.pre_call(ctx);
            on_fire(h.name(), HookPoint::PreCall, outcome.kind());
            if let HookOutcome::Abort { reason } = &outcome {
                return HookOutcome::Abort { reason: reason.clone() };
            }
        }
        HookOutcome::Continue
    }

    pub fn run_post_response(
        &self,
        ctx: &mut PostResponseCtx,
        mut on_fire: impl FnMut(&str, HookPoint, &str),
    ) -> HookOutcome<()> {
        for h in &self.hooks {
            let outcome = h.post_response(ctx);
            on_fire(h.name(), HookPoint::PostResponse, outcome.kind());
            if let HookOutcome::Abort { reason } = &outcome {
                return HookOutcome::Abort { reason: reason.clone() };
            }
        }
        HookOutcome::Continue
    }

    pub fn run_post_parse(
        &self,
        ctx: &mut PostParseCtx,
        mut on_fire: impl FnMut(&str, HookPoint, &str),
    ) -> HookOutcome<Vec<ParsedCall>> {
        let mut last_mutate: Option<Vec<ParsedCall>> = None;
        for h in &self.hooks {
            let outcome = h.post_parse(ctx);
            on_fire(h.name(), HookPoint::PostParse, outcome.kind());
            match outcome {
                HookOutcome::Continue | HookOutcome::Retry { .. } => {}
                HookOutcome::Mutate(p) => {
                    *ctx.parsed = p.clone();
                    last_mutate = Some(p);
                }
                HookOutcome::Abort { reason } => return HookOutcome::Abort { reason },
            }
        }
        match last_mutate {
            Some(p) => HookOutcome::Mutate(p),
            None => HookOutcome::Continue,
        }
    }

    pub fn run_pre_tool(
        &self,
        ctx: &mut PreToolCtx,
        mut on_fire: impl FnMut(&str, HookPoint, &str),
    ) -> HookOutcome<Value> {
        let mut last_mutate: Option<Value> = None;
        for h in &self.hooks {
            let outcome = h.pre_tool(ctx);
            on_fire(h.name(), HookPoint::PreTool, outcome.kind());
            match outcome {
                HookOutcome::Continue | HookOutcome::Retry { .. } => {}
                HookOutcome::Mutate(v) => {
                    *ctx.args = v.clone();
                    last_mutate = Some(v);
                }
                HookOutcome::Abort { reason } => return HookOutcome::Abort { reason },
            }
        }
        match last_mutate {
            Some(v) => HookOutcome::Mutate(v),
            None => HookOutcome::Continue,
        }
    }

    pub fn run_post_tool(
        &self,
        ctx: &mut PostToolCtx,
        mut on_fire: impl FnMut(&str, HookPoint, &str),
    ) -> HookOutcome<String> {
        let mut last_mutate: Option<String> = None;
        for h in &self.hooks {
            let outcome = h.post_tool(ctx);
            on_fire(h.name(), HookPoint::PostTool, outcome.kind());
            match outcome {
                HookOutcome::Continue | HookOutcome::Retry { .. } => {}
                HookOutcome::Mutate(s) => {
                    *ctx.result = s.clone();
                    last_mutate = Some(s);
                }
                HookOutcome::Abort { reason } => return HookOutcome::Abort { reason },
            }
        }
        match last_mutate {
            Some(s) => HookOutcome::Mutate(s),
            None => HookOutcome::Continue,
        }
    }
}

impl Default for HookChain {
    fn default() -> Self { Self::empty() }
}

pub mod builtin;

#[cfg(test)]
mod tests {
    use super::*;

    struct NoopHook(&'static str);
    impl Hook for NoopHook {
        fn name(&self) -> &str { self.0 }
    }

    struct AbortHook { reason: &'static str }
    impl Hook for AbortHook {
        fn name(&self) -> &str { "abort" }
        fn pre_call(&self, _ctx: &mut PreCallCtx) -> HookOutcome<()> {
            HookOutcome::Abort { reason: self.reason.to_string() }
        }
    }

    struct MutatePreToolHook { arg: Value }
    impl Hook for MutatePreToolHook {
        fn name(&self) -> &str { "mutate-pre-tool" }
        fn pre_tool(&self, _ctx: &mut PreToolCtx) -> HookOutcome<Value> {
            HookOutcome::Mutate(self.arg.clone())
        }
    }

    #[test]
    fn empty_chain_continue() {
        let chain = HookChain::empty();
        let mut msgs: Vec<Message> = vec![];
        let mut ctx = PreCallCtx { messages: &mut msgs };
        let fires: std::cell::RefCell<Vec<(String, HookPoint, String)>> =
            std::cell::RefCell::new(vec![]);
        let outcome = chain.run_pre_call(&mut ctx, |n, p, k| {
            fires.borrow_mut().push((n.to_string(), p, k.to_string()));
        });
        assert_eq!(outcome, HookOutcome::Continue);
        assert!(fires.borrow().is_empty(), "no hooks fired in empty chain");
    }

    #[test]
    fn first_abort_wins() {
        let chain = HookChain::empty()
            .push(Arc::new(NoopHook("noop1")))
            .push(Arc::new(AbortHook { reason: "first" }))
            .push(Arc::new(AbortHook { reason: "second" }));
        let mut msgs: Vec<Message> = vec![];
        let mut ctx = PreCallCtx { messages: &mut msgs };
        let fires: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(vec![]);
        let outcome = chain.run_pre_call(&mut ctx, |n, _p, _k| {
            fires.borrow_mut().push(n.to_string());
        });
        match outcome {
            HookOutcome::Abort { reason } => assert_eq!(reason, "first"),
            other => panic!("expected Abort(first), got {:?}", other),
        }
        let fired: Vec<String> = fires.borrow().iter().cloned().collect();
        assert_eq!(fired, vec!["noop1".to_string(), "abort".to_string()],
            "third hook should not fire after first Abort");
    }

    #[test]
    fn pre_tool_mutate_composes() {
        let chain = HookChain::empty()
            .push(Arc::new(MutatePreToolHook { arg: serde_json::json!("first") }))
            .push(Arc::new(MutatePreToolHook { arg: serde_json::json!("second") }));
        let mut args = serde_json::json!("initial");
        let mut ctx = PreToolCtx { name: "exec", args: &mut args };
        let outcome = chain.run_pre_tool(&mut ctx, |_, _, _| {});
        match outcome {
            HookOutcome::Mutate(v) => assert_eq!(v, serde_json::json!("second"), "last Mutate wins"),
            other => panic!("expected Mutate, got {:?}", other),
        }
        assert_eq!(*ctx.args, serde_json::json!("second"), "ctx mutated to last Mutate value");
    }

    #[test]
    fn noop_hook_fires_with_continue_outcome() {
        let chain = HookChain::empty().push(Arc::new(NoopHook("only")));
        let mut msgs: Vec<Message> = vec![];
        let mut ctx = PreCallCtx { messages: &mut msgs };
        let fires: std::cell::RefCell<Vec<(String, HookPoint, String)>> =
            std::cell::RefCell::new(vec![]);
        let outcome = chain.run_pre_call(&mut ctx, |n, p, k| {
            fires.borrow_mut().push((n.to_string(), p, k.to_string()));
        });
        assert_eq!(outcome, HookOutcome::Continue);
        let fired = fires.borrow();
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].0, "only");
        assert_eq!(fired[0].1, HookPoint::PreCall);
        assert_eq!(fired[0].2, "continue");
    }
    #[test]
    fn outcome_kind_labels() {
        assert_eq!(HookOutcome::<()>::Continue.kind(), "continue");
        assert_eq!(HookOutcome::Mutate(42).kind(), "mutate");
        assert_eq!(HookOutcome::<()>::Abort { reason: "x".into() }.kind(), "abort");
}
}
