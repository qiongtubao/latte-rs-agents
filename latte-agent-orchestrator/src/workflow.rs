//! Discussion workflow: defines the structure of a multi-agent discussion.
//!
//! Mirrors gsd-core's workflow .md files pattern: a workflow is a sequence of
//! steps, each specifying which agents speak and with what prompt.

use serde::{Deserialize, Serialize};

/// A complete discussion workflow: a sequence of steps with hooks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscussionWorkflow {
    /// Workflow name (e.g. "code_review", "requirements_review").
    pub name: String,
    /// Human-readable description.
    #[serde(default)]
    pub description: String,
    /// Sequence of steps.
    pub steps: Vec<WorkflowStep>,
    /// Maximum discussion rounds.
    #[serde(default = "default_max_rounds")]
    pub max_rounds: usize,
    /// Token budget for shared context (0 = unlimited).
    #[serde(default)]
    pub context_token_budget: usize,
}

fn default_max_rounds() -> usize {
    3
}

impl Default for DiscussionWorkflow {
    fn default() -> Self {
        Self {
            name: "default".into(),
            description: String::new(),
            steps: Vec::new(),
            max_rounds: 3,
            context_token_budget: 0,
        }
    }
}

/// A single step within a discussion workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStep {
    /// Step identifier (e.g. "present", "review", "decide").
    pub id: String,
    /// Human-readable description.
    #[serde(default)]
    pub description: String,
    /// Agents that speak in this step, in order.
    pub speakers: Vec<String>,
    /// Prompt template for this step. Supports `{{variable}}` substitution.
    pub prompt: String,
    /// Hooks to inject before/after this step.
    #[serde(default)]
    pub hooks: Vec<StepHook>,
    /// If set, the combined output of this step is stored under this key
    /// for use in subsequent steps.
    #[serde(default)]
    pub output_key: Option<String>,
}

/// A hook that runs before or after a workflow step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepHook {
    /// Hook kind.
    pub kind: HookKind,
    /// Agent source for content injection (only for Contribution).
    #[serde(default)]
    pub source: Option<String>,
    /// Template for the hook content.
    #[serde(default)]
    pub template: Option<String>,
    /// Message for gating hooks (shown when check fails).
    #[serde(default)]
    pub message: Option<String>,
}

/// Types of step hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookKind {
    /// Inject content before the step begins.
    Contribution,
    /// Blocking validation check (must pass to proceed).
    Gate,
}

impl DiscussionWorkflow {
    /// Load a workflow from a TOML string.
    pub fn parse(toml_str: &str) -> Result<Self, crate::error::OrchError> {
        toml::from_str(toml_str)
            .map_err(|e| crate::error::OrchError::Config(format!("invalid workflow TOML: {}", e)))
    }

    /// Validate the workflow: check that all referenced agents exist.
    pub fn validate(&self, available_agents: &[String]) -> Result<(), crate::error::OrchError> {
        for step in &self.steps {
            for speaker in &step.speakers {
                if !available_agents.contains(speaker) {
                    return Err(crate::error::OrchError::AgentNotFound(
                        speaker.clone(),
                        available_agents.to_vec(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Build the prompt for a step by substituting variables.
    pub fn build_step_prompt(
        &self,
        step: &WorkflowStep,
        vars: &std::collections::HashMap<String, String>,
    ) -> String {
        let mut prompt = step.prompt.clone();
        for (key, value) in vars {
            prompt = prompt.replace(&format!("{{{{{}}}}}", key), value);
        }
        prompt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_workflow() {
        let toml = r#"
name = "code_review"
description = "Multi-perspective code review"
max_rounds = 1

[[steps]]
id = "explain"
speakers = ["programmer"]
prompt = "Explain the changes at {{code_path}}"

[[steps]]
id = "review"
speakers = ["reviewer", "tester"]
prompt = "Review the changes described above."
"#;

        let wf = DiscussionWorkflow::parse(toml).unwrap();
        assert_eq!(wf.name, "code_review");
        assert_eq!(wf.steps.len(), 2);
        assert_eq!(wf.steps[0].speakers, vec!["programmer"]);
        assert_eq!(wf.steps[1].speakers, vec!["reviewer", "tester"]);
    }

    #[test]
    fn test_validate_missing_agent() {
        let wf = DiscussionWorkflow {
            name: "test".into(),
            steps: vec![WorkflowStep {
                id: "s1".into(),
                description: String::new(),
                speakers: vec!["missing_agent".into()],
                prompt: "test".into(),
                hooks: vec![],
                output_key: None,
            }],
            ..Default::default()
        };

        let err = wf.validate(&["pm".into(), "dev".into()]).unwrap_err();
        assert!(matches!(err, crate::error::OrchError::AgentNotFound(_, _)));
    }

    #[test]
    fn test_build_step_prompt() {
        let wf = DiscussionWorkflow::default();
        let step = WorkflowStep {
            id: "s1".into(),
            description: String::new(),
            speakers: vec!["pm".into()],
            prompt: "Topic: {{topic}}, Sprint: {{sprint}}".into(),
            hooks: vec![],
            output_key: None,
        };

        let mut vars = std::collections::HashMap::new();
        vars.insert("topic".into(), "Auth".into());
        vars.insert("sprint".into(), "S3".into());

        let prompt = wf.build_step_prompt(&step, &vars);
        assert_eq!(prompt, "Topic: Auth, Sprint: S3");
    }
}
