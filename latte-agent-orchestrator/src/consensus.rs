//! Consensus mechanisms: voting, deliberation, and weighted decision.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::{OrchError, OrchResult};
use crate::round::{AgentVote, VoteResult};

/// How consensus is reached in a discussion.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsensusMethod {
    /// Simple majority vote (50%+1).
    MajorityVote,
    /// Each role has weight; weighted vote.
    WeightedVote(HashMap<String, f64>),
    /// Discussion runs until consensus threshold or max rounds.
    DeliberateUntilConsensus {
        /// Minimum agreement ratio (0.0–1.0).
        min_agreement: f64,
    },
    /// Moderator decides after hearing all.
    ModeratorDecides {
        /// Name of the moderator agent.
        moderator: String,
    },
    /// Just record all opinions, no consensus needed.
    NoConsensus,
}

impl Default for ConsensusMethod {
    fn default() -> Self {
        Self::NoConsensus
    }
}

impl ConsensusMethod {
    /// Parse from a TOML-like string description.
    pub fn parse(s: &str) -> OrchResult<Self> {
        match s.to_lowercase().as_str() {
            "majority_vote" | "majority" => Ok(Self::MajorityVote),
            "no_consensus" | "none" => Ok(Self::NoConsensus),
            other => Err(OrchError::Config(format!(
                "unknown consensus method: '{}'",
                other
            ))),
        }
    }

    /// Evaluate whether consensus exists based on collected votes.
    pub fn evaluate(&self, votes: &[AgentVote]) -> VoteResult {
        match self {
            Self::NoConsensus => VoteResult {
                consensus: true, // always "succeeds"
                agreement: 1.0,
                ..Default::default()
            },
            Self::MajorityVote => evaluate_majority(votes),
            Self::WeightedVote(weights) => evaluate_weighted(votes, weights),
            Self::DeliberateUntilConsensus { min_agreement } => {
                evaluate_deliberative(votes, *min_agreement)
            }
            Self::ModeratorDecides { .. } => {
                // Moderator decision happens later, after the moderator speaks
                VoteResult {
                    consensus: false,
                    agreement: 0.0,
                    ..Default::default()
                }
            }
        }
    }

    /// Check if this method needs a vote from each agent.
    pub fn requires_vote(&self) -> bool {
        !matches!(self, Self::NoConsensus | Self::ModeratorDecides { .. })
    }

    /// Check if this method needs a moderator agent.
    pub fn requires_moderator(&self) -> Option<&str> {
        match self {
            Self::ModeratorDecides { moderator } => Some(moderator.as_str()),
            _ => None,
        }
    }
}

fn evaluate_majority(votes: &[AgentVote]) -> VoteResult {
    if votes.is_empty() {
        return VoteResult {
            consensus: true,
            agreement: 1.0,
            ..Default::default()
        };
    }

    let mut counts: HashMap<String, usize> = HashMap::new();
    for v in votes {
        *counts.entry(v.position.clone()).or_default() += 1;
    }

    let (winner, count) = counts.into_iter().max_by_key(|(_, c)| *c).unwrap_or_default();
    let total = votes.len();
    let agreement = count as f64 / total as f64;
    let majority = (total / 2) + 1;

    VoteResult {
        votes: votes.to_vec(),
        consensus: count >= majority,
        winner: if count >= majority {
            Some(winner)
        } else {
            None
        },
        agreement,
    }
}

fn evaluate_weighted(votes: &[AgentVote], weights: &HashMap<String, f64>) -> VoteResult {
    let mut scores: HashMap<String, f64> = HashMap::new();
    let mut total_weight: f64 = 0.0;

    for v in votes {
        let w = weights.get(&v.agent).copied().unwrap_or(1.0);
        *scores.entry(v.position.clone()).or_default() += w;
        total_weight += w;
    }

    if total_weight == 0.0 {
        return VoteResult {
            votes: votes.to_vec(),
            consensus: true,
            agreement: 1.0,
            ..Default::default()
        };
    }

    let (winner, score) = scores
        .into_iter()
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or_default();

    VoteResult {
        votes: votes.to_vec(),
        consensus: score > total_weight / 2.0,
        winner: if score > total_weight / 2.0 {
            Some(winner)
        } else {
            None
        },
        agreement: score / total_weight,
    }
}

fn evaluate_deliberative(votes: &[AgentVote], min_agreement: f64) -> VoteResult {
    if votes.is_empty() {
        return VoteResult {
            consensus: false,
            agreement: 0.0,
            ..Default::default()
        };
    }

    let mut counts: HashMap<String, usize> = HashMap::new();
    for v in votes {
        *counts.entry(v.position.clone()).or_default() += 1;
    }

    let (winner, count) = counts.into_iter().max_by_key(|(_, c)| *c).unwrap_or_default();
    let total = votes.len();
    let agreement = count as f64 / total as f64;

    VoteResult {
        votes: votes.to_vec(),
        consensus: agreement >= min_agreement,
        winner: if agreement >= min_agreement {
            Some(winner)
        } else {
            None
        },
        agreement,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vote(agent: &str, position: &str) -> AgentVote {
        AgentVote {
            agent: agent.into(),
            position: position.into(),
            confidence: 1.0,
            reasoning: String::new(),
        }
    }

    #[test]
    fn test_majority_consensus() {
        let votes = vec![
            vote("pm", "yes"),
            vote("dev", "yes"),
            vote("qa", "no"),
        ];
        let result = ConsensusMethod::MajorityVote.evaluate(&votes);
        assert!(result.consensus);
        assert_eq!(result.winner, Some("yes".into()));
        assert!((result.agreement - 2.0 / 3.0).abs() < 0.01);
    }

    #[test]
    fn test_majority_no_consensus() {
        let votes = vec![
            vote("pm", "yes"),
            vote("dev", "no"),
            vote("qa", "no"),
        ];
        let result = ConsensusMethod::MajorityVote.evaluate(&votes);
        assert!(result.consensus);
        assert_eq!(result.winner, Some("no".into()));
    }

    #[test]
    fn test_majority_tie() {
        let votes = vec![vote("pm", "yes"), vote("dev", "no")];
        let result = ConsensusMethod::MajorityVote.evaluate(&votes);
        assert!(!result.consensus);
        assert_eq!(result.winner, None);
    }

    #[test]
    fn test_weighted_vote() {
        let mut weights = HashMap::new();
        weights.insert("pm".into(), 3.0);
        weights.insert("dev".into(), 1.0);
        weights.insert("qa".into(), 1.0);

        let votes = vec![
            vote("pm", "option_a"),
            vote("dev", "option_b"),
            vote("qa", "option_b"),
        ];
        let result = ConsensusMethod::WeightedVote(weights).evaluate(&votes);
        assert!(result.consensus);
        assert_eq!(result.winner, Some("option_a".into()));
    }

    #[test]
    fn test_deliberative_strict() {
        let votes = vec![
            vote("pm", "yes"),
            vote("dev", "yes"),
            vote("qa", "yes"),
            vote("sec", "no"),
        ];
        let result = ConsensusMethod::DeliberateUntilConsensus { min_agreement: 0.75 }
            .evaluate(&votes);
        assert!(result.consensus);
    }

    #[test]
    fn test_no_consensus_always_passes() {
        let result = ConsensusMethod::NoConsensus.evaluate(&[]);
        assert!(result.consensus);
    }
}
