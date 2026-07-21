//! DiscussionOrchestrator: the top-level coordinator for multi-agent discussions.
//!
//! Owns the agent pool, shared context, and round workflow. Drives the discussion
//! loop: for each round, iterate workflow steps, have agents speak, collect
//! responses, check consensus, repeat until done.

use std::collections::HashMap;

use latte_agent_core::agent::AgentRunner;
use latte_agent_core::context::ConversationContext;
use latte_ai::models::{Message, Role, TokenUsage};

use crate::consensus::ConsensusMethod;
use crate::error::{OrchError, OrchResult};
use crate::round::{DiscussionResult, Round, TurnRecord};
use crate::workflow::{DiscussionWorkflow, HookKind};

/// Configuration for a discussion session.
#[derive(Debug, Clone)]
pub struct DiscussionConfig {
    /// Which workflow to use.
    pub workflow: DiscussionWorkflow,
    /// Consensus method.
    pub consensus: ConsensusMethod,
    /// Maximum rounds (overrides workflow default if set).
    pub max_rounds: Option<usize>,
    /// Token budget for shared context.
    pub context_token_budget: usize,
    /// Global template variables (e.g. {{topic}}, {{project}}).
    pub variables: HashMap<String, String>,
}

impl DiscussionConfig {
    /// Maximum rounds, respecting override.
    pub fn effective_max_rounds(&self) -> usize {
        self.max_rounds.unwrap_or(self.workflow.max_rounds)
    }
}

/// The discussion orchestrator.
pub struct DiscussionOrchestrator {
    /// All participating agents, keyed by name.
    agents: HashMap<String, AgentRunner>,
    /// Shared conversation context.
    context: ConversationContext,
    /// Discussion configuration.
    config: DiscussionConfig,
    /// Accumulated history of all rounds.
    rounds: Vec<Round>,
}

impl DiscussionOrchestrator {
    /// Create a new orchestrator with the given agents and configuration.
    pub fn new(
        agents: HashMap<String, AgentRunner>,
        config: DiscussionConfig,
    ) -> OrchResult<Self> {
        // Validate all workflow speakers exist
        let agent_names: Vec<String> = agents.keys().cloned().collect();
        config.workflow.validate(&agent_names)?;

        // Check moderator if needed
        if let Some(moderator) = config.consensus.requires_moderator() {
            if !agents.contains_key(moderator) {
                return Err(OrchError::AgentNotFound(
                    moderator.into(),
                    agent_names,
                ));
            }
        }

        Ok(Self {
            agents,
            context: ConversationContext::new(config.context_token_budget),
            config,
            rounds: Vec::new(),
        })
    }

    /// Borrow the registered agents. Used by callers that need
    /// to emit per-role trace events (e.g. `SessionEnd`) after
    /// `run()` returns. The orchestrator already drove the
    /// runners through their turns, so the cumulative token
    /// usage is available via `runner.total_usage()`.
    pub fn agents(&self) -> &HashMap<String, AgentRunner> {
        &self.agents
    }

    /// Run the full discussion. Returns the discussion result.
    pub async fn run(&mut self) -> OrchResult<DiscussionResult> {
        let max_rounds = self.config.effective_max_rounds();
        let mut total_usage = TokenUsage::default();

        for round_num in 0..max_rounds {
            let mut round = Round::new(round_num);

            for step in &self.config.workflow.steps.clone() {
                let agent_names: Vec<String> = self.agents.keys().cloned().collect();

                // --- Pre-step hooks ---
                for hook in &step.hooks {
                    match hook.kind {
                        HookKind::Contribution => {
                            if let Some(source) = &hook.source {
                                if let Some(template) = &hook.template {
                                    if let Some(_agent_runner) = self.agents.get(source) {
                                        let rendered = self.render_template(template);
                                        round.record(TurnRecord {
                                            agent: source.clone(),
                                            role_id: source.clone(),
                                            response: rendered.clone(),
                                            round: round_num,
                                            step_id: step.id.clone(),
                                            turn_number: round.turns.len(),
                                        });
                                    }
                                }
                            }
                        }
                        HookKind::Gate => {
                            // Gates are informational for now (human-check pattern)
                            // In future: integrate verification agent
                        }
                    }
                }

                // --- Step speakers ---
                let mut step_transcript = String::new();

                for speaker in &step.speakers {
                    let agent_runner = self
                        .agents
                        .get_mut(speaker)
                        .ok_or_else(|| OrchError::AgentNotFound(
                            speaker.clone(),
                            agent_names.clone(),
                        ))?;

                    // Build the prompt for this speaker
                    let mut vars = self.config.variables.clone();
                    vars.insert("step_id".into(), step.id.clone());
                    vars.insert("speaker".into(), speaker.clone());
                    vars.insert("topic".into(), self.config.variables.get("topic").cloned().unwrap_or_default());

                    let step_prompt = self.config.workflow.build_step_prompt(step, &vars);

                    // Build context: include transcript of current step so far
                    let context_prompt = if step_transcript.is_empty() {
                        step_prompt
                    } else {
                        format!(
                            "{}\n\n--- Preceding discussion in this step ---\n{}",
                            step_prompt, step_transcript
                        )
                    };

                    // Build system vars for the agent
                    let system_vars = serde_json::json!({
                        "role_name": speaker,
                        "topic": vars.get("topic").unwrap_or(&String::new()),
                        "step": step.id,
                    });

                    // Run the agent's turn
                    let response = agent_runner
                        .run_turn(
                            &[Message::user(context_prompt)],
                            Some(&system_vars),
                        )
                        .await?;

                    // Update usage
                    let usage = agent_runner.total_usage();
                    total_usage.input_tokens += usage.input_tokens;
                    total_usage.output_tokens += usage.output_tokens;

                    // Record the turn
                    round.record(TurnRecord {
                        agent: speaker.clone(),
                        role_id: agent_runner.agent().role.id.clone(),
                        response: response.clone(),
                        round: round_num,
                        step_id: step.id.clone(),
                        turn_number: round.turns.len(),
                    });

                    step_transcript.push_str(&format!(
                        "[{}]: {}\n",
                        speaker, response
                    ));

                    // Store step output if configured
                    if let Some(ref key) = step.output_key {
                        self.config.variables.insert(key.clone(), response.clone());
                    }
                }

                // --- Post-step hooks ---
                for hook in &step.hooks {
                    if hook.kind == HookKind::Gate {
                        // Informational gate
                    }
                }
            }

            // --- Consensus check ---
            if self.config.consensus.requires_vote() {
                let votes = collect_votes(&round);
                let vote_result = self.config.consensus.evaluate(&votes);
                round.consensus_reached = vote_result.consensus;
            } else {
                round.consensus_reached = true; // no consensus method → auto-consensus
            }

            self.rounds.push(round);

            // Break if consensus reached
            if self.rounds.last().map(|r| r.consensus_reached).unwrap_or(false) {
                break;
            }
        }

        let consensus_reached = self.rounds.last().map(|r| r.consensus_reached).unwrap_or(false);

        Ok(DiscussionResult {
            rounds: self.rounds.clone(),
            consensus_reached,
            votes: None,
            summary: None,
            total_usage,
        })
    }

    /// Run the discussion, calling `on_turn` after each agent turn completes.
    /// This is a streaming variant of `run()` — it yields the same result but
    /// notifies a callback after every turn so UIs can show progress.
    pub async fn run_with_events<F>(
        &mut self,
        mut on_turn: F,
    ) -> OrchResult<DiscussionResult>
    where
        F: FnMut(&TurnRecord),
    {
        let max_rounds = self.config.effective_max_rounds();
        let mut total_usage = TokenUsage::default();

        for round_num in 0..max_rounds {
            let mut round = Round::new(round_num);

            for step in self.config.workflow.steps.clone() {
                let agent_names: Vec<String> = self.agents.keys().cloned().collect();
                let mut step_transcript = String::new();

                for speaker in &step.speakers {
                    let agent_runner = self.agents.get_mut(speaker).ok_or_else(|| {
                        OrchError::AgentNotFound(speaker.clone(), agent_names.clone())
                    })?;

                    let mut vars = self.config.variables.clone();
                    vars.insert("step_id".into(), step.id.clone());
                    vars.insert("speaker".into(), speaker.clone());
                    vars.insert(
                        "topic".into(),
                        self.config.variables.get("topic").cloned().unwrap_or_default(),
                    );

                    let step_prompt = self.config.workflow.build_step_prompt(&step, &vars);
                    let context_prompt = if step_transcript.is_empty() {
                        step_prompt
                    } else {
                        format!(
                            "{}\n\n--- Preceding discussion in this step ---\n{}",
                            step_prompt, step_transcript
                        )
                    };

                    let system_vars = serde_json::json!({
                        "role_name": speaker,
                        "topic": vars.get("topic").unwrap_or(&String::new()),
                        "step": step.id,
                    });

                    let response = agent_runner
                        .run_turn(
                        &[Message::user(context_prompt)],
                            Some(&system_vars),
                        )
                        .await?;

                    let usage = agent_runner.total_usage();
                    total_usage.input_tokens += usage.input_tokens;
                    total_usage.output_tokens += usage.output_tokens;

                    let record = TurnRecord {
                        agent: speaker.clone(),
                        role_id: agent_runner.agent().role.id.clone(),
                        response: response.clone(),
                        round: round_num,
                        step_id: step.id.clone(),
                        turn_number: round.turns.len(),
                    };
                    round.record(record.clone());

                    // Streaming notification
                    on_turn(&record);

                    step_transcript.push_str(&format!("[{}]: {}\n", speaker, response));

                    if let Some(ref key) = step.output_key {
                        self.config.variables.insert(key.clone(), response.clone());
                    }
                }
            }

            if self.config.consensus.requires_vote() {
                let votes = collect_votes(&round);
                let vote_result = self.config.consensus.evaluate(&votes);
                round.consensus_reached = vote_result.consensus;
            } else {
                round.consensus_reached = true;
            }

            self.rounds.push(round);
            if self.rounds.last().map(|r| r.consensus_reached).unwrap_or(false) {
                break;
            }
        }

        let consensus_reached = self
            .rounds
            .last()
            .map(|r| r.consensus_reached)
            .unwrap_or(false);

        Ok(DiscussionResult {
            rounds: self.rounds.clone(),
            consensus_reached,
            votes: None,
            summary: None,
            total_usage,
        })
    }

    /// Get the discussion transcript as a single string.
    pub fn transcript(&self) -> String {
        self.rounds
            .iter()
            .enumerate()
            .map(|(i, r)| {
                format!(
                    "=== Round {} ===\n{}",
                    i + 1,
                    r.transcript()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render_template(&self, template: &str) -> String {
        let mut result = template.to_string();
        for (key, value) in &self.config.variables {
            result = result.replace(&format!("{{{{{}}}}}", key), value);
        }
        result
    }
}

/// Collect votes from the last response of each agent in a round.
fn collect_votes(round: &Round) -> Vec<crate::round::AgentVote> {
    round
        .turns
        .iter()
        .map(|t| {
            // Simple heuristic: extract a position from the last sentence
            let position = if t.response.contains("agree") || t.response.contains("yes") {
                "yes".to_string()
            } else if t.response.contains("disagree") || t.response.contains("no") {
                "no".to_string()
            } else {
                "abstain".to_string()
            };

            crate::round::AgentVote {
                agent: t.agent.clone(),
                position,
                confidence: 0.8,
                reasoning: t.response.clone(),
            }
        })
        .collect()
}
