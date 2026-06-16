//! Agent and AgentRunner: turn-based agent execution.
//!
//! An `Agent` is a configured role with a resolved model client.
//! An `AgentRunner` wraps an agent with a conversation context and executes turns:
//! each turn processes incoming messages through the AI model and returns the
//! response. Supports tool calling via a text-based `<tool_call>` protocol.

use std::sync::Arc;

use latte_ai::client::AiClient;
use latte_ai::models::{Completion, Message, Role, TokenUsage};
use latte_ai::params::GenerateParams;

use crate::context::ConversationContext;
use crate::error::{AgentError, AgentResult};
use crate::role::Role as RoleDef;

// ─── Agent ────────────────────────────────────────────────────────────────

/// A fully configured agent: role identity + resolved model client.
#[derive(Clone)]
pub struct Agent {
    /// Unique name in a discussion (e.g. "pm", "dev_lead").
    pub name: String,
    /// Role definition (system prompt, defaults).
    pub role: RoleDef,
    /// AI model client, pre-configured with the resolved model.
    pub client: AiClient,
    /// Generation parameters for this agent instance.
    pub params: GenerateParams,
}

impl Agent {
    /// Create a new agent.
    pub fn new(
        name: String,
        role: RoleDef,
        model: latte_ai::models::Model,
        params: GenerateParams,
    ) -> AgentResult<Self> {
        let client = AiClient::new(model)?;
        Ok(Self {
            name,
            role,
            client,
            params,
        })
    }

    /// Create a minimal agent for testing with a mock-able model.
    pub fn new_with_client(
        name: String,
        role: RoleDef,
        client: AiClient,
        params: GenerateParams,
    ) -> Self {
        Self {
            name,
            role,
            client,
            params,
        }
    }

    /// Send a single chat completion request (one-turn, no context).
    pub async fn chat(
        &self,
        messages: &[Message],
        params: Option<&GenerateParams>,
    ) -> AgentResult<Completion> {
        let p = params.unwrap_or(&self.params);
        Ok(self.client.chat(messages, p).await?)
    }

    /// Build the system message for this agent.
    pub fn system_message(&self, vars: &serde_json::Value) -> AgentResult<Message> {
        let content = self.role.render_prompt(vars)?;
        Ok(Message {
            role: Role::System,
            content,
        })
    }
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("name", &self.name)
            .field("role", &self.role.id)
            .field("model", &self.client.model().id)
            .finish()
    }
}

// ─── AgentRunner ───────────────────────────────────────────────────────────

/// Per-agent turn executor with conversation context and optional tool support.
///
/// Maintains the full conversation history and executes turn-by-turn:
///   1. Build message list: system prompt + history + new user messages
///   2. Send to AI model
///   3. If response contains `<tool_call>`, execute tool, append result, goto 2
///   4. Record final response in history
///   5. Return response text
pub struct AgentRunner {
    agent: Agent,
    context: ConversationContext,
    /// Max tool-call round trips per turn (0 = disables tool calling).
    max_tool_rounds: usize,
    /// Optional tool manager for executing tool calls.
    tool_manager: Option<Arc<dyn latte_rs_agent_tools::types::ToolManager>>,
    /// Accumulated token usage across all turns.
    total_usage: TokenUsage,
}

impl AgentRunner {
    /// Create a new runner for an agent (no tools).
    pub fn new(agent: Agent) -> Self {
        Self {
            agent,
            context: ConversationContext::default(),
            max_tool_rounds: 0,
            tool_manager: None,
            total_usage: TokenUsage::default(),
        }
    }

    /// Create a runner with tool support.
    pub fn new_with_tools(
        agent: Agent,
        tool_manager: Arc<dyn latte_rs_agent_tools::types::ToolManager>,
        max_tool_rounds: usize,
    ) -> Self {
        Self {
            agent,
            context: ConversationContext::default(),
            max_tool_rounds,
            tool_manager: Some(tool_manager),
            total_usage: TokenUsage::default(),
        }
    }

    /// Create a runner with a pre-existing context.
    pub fn with_context(agent: Agent, context: ConversationContext) -> Self {
        Self {
            agent,
            context,
            max_tool_rounds: 0,
            tool_manager: None,
            total_usage: TokenUsage::default(),
        }
    }

    /// Set the max tool-call round trips per turn.
    pub fn set_max_tool_rounds(&mut self, n: usize) {
        self.max_tool_rounds = n;
    }

    /// Get a reference to the agent.
    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    /// Get the conversation context.
    pub fn context(&self) -> &ConversationContext {
        &self.context
    }

    /// Get mutable conversation context.
    pub fn context_mut(&mut self) -> &mut ConversationContext {
        &mut self.context
    }

    /// Total token usage accumulated so far.
    pub fn total_usage(&self) -> &TokenUsage {
        &self.total_usage
    }

    /// Run one turn: process a user message and return the assistant's response.
    ///
    /// The turn builds a complete message list:
    ///   [system_prompt, ...history, new_user_messages...]
    ///
    /// If tools are configured, enters a tool-call loop:
    ///   request → response → parse tool_calls → execute → append result → repeat
    pub async fn run_turn(
        &mut self,
        new_messages: &[Message],
        system_vars: Option<&serde_json::Value>,
    ) -> AgentResult<String> {
        let default_vars = serde_json::json!({});
        let vars = system_vars.unwrap_or(&default_vars);

        let sys_msg = self.agent.system_message(vars)?;

        let mut messages: Vec<Message> = Vec::new();
        messages.push(sys_msg);
        messages.extend_from_slice(self.context.messages());
        messages.extend_from_slice(new_messages);

        let max_rounds = if self.tool_manager.is_some() {
            self.max_tool_rounds.max(1)
        } else {
            1
        };

        let mut final_response = String::new();

        for round in 0..max_rounds {
            let completion = self.agent.chat(&messages, None).await?;

            self.total_usage.input_tokens += completion.usage.input_tokens;
            self.total_usage.output_tokens += completion.usage.output_tokens;
            self.total_usage.thinking_tokens += completion.usage.thinking_tokens;

            final_response = completion.content.clone();

            // Check for tool calls
            let tool_calls = extract_tool_calls(&final_response);
            if tool_calls.is_empty() {
                break;
            }

            if let Some(tm) = &self.tool_manager {
                // Append assistant message with tool calls
                messages.push(Message {
                    role: Role::Assistant,
                    content: final_response.clone(),
                });

                // Execute each tool call
                for tc in &tool_calls {
                    let input: serde_json::Value = serde_json::from_str(&tc.args)
                        .unwrap_or(serde_json::Value::String(tc.args.clone()));

                    let ctx = latte_rs_agent_tools::types::ToolExecutionContext::fresh(
                        &tc.name,
                        1,
                    );

                    match tm.execute(&tc.name, input.clone(), Some(ctx)).await {
                        Ok(result) => {
                            messages.push(Message {
                                role: Role::User,
                                content: format!(
                                    "[tool_result for {}]\n{}",
                                    tc.name,
                                    serde_json::to_string_pretty(&result)
                                        .unwrap_or_else(|_| format!("{:?}", result))
                                ),
                            });
                        }
                        Err(e) => {
                            messages.push(Message {
                                role: Role::User,
                                content: format!("[tool_error for {}]\n{}", tc.name, e),
                            });
                        }
                    }
                }
            }

            if round + 1 >= max_rounds {
                return Err(AgentError::MaxToolRoundsExceeded(max_rounds));
            }
        }

        // Store in context
        for msg in new_messages {
            self.context.push(msg.clone());
        }
        self.context.push(Message {
            role: Role::Assistant,
            content: final_response.clone(),
        });

        Ok(final_response)
    }

    /// Run a turn without recording in history (useful for side queries).
    pub async fn run_turn_ephemeral(
        &self,
        new_messages: &[Message],
        system_vars: Option<&serde_json::Value>,
    ) -> AgentResult<String> {
        let default_vars = serde_json::json!({});
        let vars = system_vars.unwrap_or(&default_vars);

        let mut messages = vec![self.agent.system_message(vars)?];
        messages.extend_from_slice(new_messages);

        let completion = self.agent.chat(&messages, None).await?;
        Ok(completion.content)
    }

    /// Clear conversation history (but keep total usage stats).
    pub fn reset_context(&mut self) {
        self.context.clear();
    }

    /// Prune context to stay within token budget, keeping last N messages.
    pub fn prune_context(&mut self, keep_last: usize) {
        self.context.prune_to_budget(keep_last);
    }
}

impl std::fmt::Debug for AgentRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRunner")
            .field("agent", &self.agent.name)
            .field("messages", &self.context.messages().len())
            .field("tokens", &self.context.token_count())
            .field("tools", &self.tool_manager.is_some())
            .finish()
    }
}

// ─── Tool Call Parsing ────────────────────────────────────────────────────

/// A parsed tool call from model output.
#[derive(Debug, Clone)]
struct ToolCall {
    name: String,
    args: String,
}

/// Extract `<tool_call>name args</tool_call>` patterns from model output.
fn extract_tool_calls(text: &str) -> Vec<ToolCall> {
    let mut results = Vec::new();
    let mut remaining = text;

    while let Some(start) = remaining.find("<tool_call>") {
        let inner_start = start + "<tool_call>".len();
        if let Some(end) = remaining[inner_start..].find("</tool_call>") {
            let inner = &remaining[inner_start..inner_start + end];
            let (name, args) = if let Some(space) = inner.find(char::is_whitespace) {
                let n = inner[..space].trim().to_string();
                let a = inner[space + 1..].trim().to_string();
                (n, a)
            } else {
                (inner.trim().to_string(), String::new())
            };
            results.push(ToolCall { name, args });
            remaining = &remaining[inner_start + end + "</tool_call>".len()..];
        } else {
            break;
        }
    }

    results
}

/// Convenience: parameters override for agent construction.
pub struct AgentParams {
    pub params: GenerateParams,
}

impl AgentParams {
    pub fn from_role(role: &RoleDef) -> Self {
        Self {
            params: role.default_params.clone(),
        }
    }
}

impl From<GenerateParams> for AgentParams {
    fn from(params: GenerateParams) -> Self {
        Self { params }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_resolver::ModelTier;
    use crate::role::Role as AgentRole;
    use crate::role::RoleCategory;
    use latte_ai::models::Model;
    use latte_ai::models::Role as MsgRole;

    fn test_role() -> AgentRole {
        AgentRole {
            id: "test".into(),
            name: "Test Agent".into(),
            category: RoleCategory::Discussion,
            system_prompt: "You are a {{role_name}}. Topic: {{topic}}".into(),
            default_model_tier: ModelTier::Standard,
            default_params: GenerateParams::default(),
            allowed_tools: vec![],
            icon: "🧪".into(),
        }
    }

    fn test_model() -> Model {
        Model {
            id: "test-model".into(),
            name: "Test Model".into(),
            api: latte_ai::models::ApiType::OpenAiCompletions,
            provider: "test".into(),
            base_url: "http://localhost:9999".into(),
            api_key: "test-key".into(),
            context_window: 32000,
            max_tokens: 4096,
            supports_thinking: false,
            cost_per_million_input: 0.0,
            cost_per_million_output: 0.0,
        }
    }

    #[test]
    fn test_agent_construction() {
        let role = test_role();
        let agent = Agent::new("test-agent".into(), role, test_model(), GenerateParams::default());
        assert!(agent.is_ok());
    }

    #[test]
    fn test_system_message_building() {
        let role = test_role();
        let agent =
            Agent::new("test-agent".into(), role, test_model(), GenerateParams::default()).unwrap();

        let vars = serde_json::json!({
            "role_name": "Tester",
            "topic": "Auth module"
        });

        let msg = agent.system_message(&vars).unwrap();
        assert!(msg.content.contains("Tester"));
        assert!(msg.content.contains("Auth module"));
        assert!(matches!(msg.role, MsgRole::System));
    }

    #[test]
    fn test_runner_construction() {
        let role = test_role();
        let agent =
            Agent::new("test-agent".into(), role, test_model(), GenerateParams::default()).unwrap();
        let runner = AgentRunner::new(agent);
        assert_eq!(runner.context().messages().len(), 0);
        assert_eq!(runner.total_usage().input_tokens, 0);
    }

    #[test]
    fn test_runner_context_management() {
        let role = test_role();
        let agent =
            Agent::new("test-agent".into(), role, test_model(), GenerateParams::default()).unwrap();
        let mut runner = AgentRunner::new(agent);

        runner.context_mut().push(Message {
            role: MsgRole::User,
            content: "Hello".into(),
        });
        assert_eq!(runner.context().messages().len(), 1);
    }

    #[test]
    fn test_prune_context() {
        let role = test_role();
        let agent =
            Agent::new("test-agent".into(), role, test_model(), GenerateParams::default()).unwrap();
        let mut runner = AgentRunner::new(agent);

        runner.context_mut().set_token_budget(10);
        runner.context_mut().push(Message {
            role: MsgRole::User,
            content: "a".repeat(200),
        });

        runner.prune_context(1);
        assert!(runner.context().messages().len() <= 1);
    }

    #[test]
    fn test_extract_tool_calls() {
        let text = r#"Some text before
<tool_call>read {"path": "src/main.rs"}</tool_call>
More text
<tool_call>search {"pattern": "TODO"}</tool_call>
End"#;

        let calls = extract_tool_calls(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "read");
        assert_eq!(calls[0].args, r#"{"path": "src/main.rs"}"#);
        assert_eq!(calls[1].name, "search");
        assert_eq!(calls[1].args, r#"{"pattern": "TODO"}"#);
    }

    #[test]
    fn test_extract_tool_calls_no_args() {
        let text = "<tool_call>list_models</tool_call>";
        let calls = extract_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "list_models");
        assert_eq!(calls[0].args, "");
    }

    #[test]
    fn test_extract_tool_calls_empty() {
        let calls = extract_tool_calls("Just a regular response, no tools.");
        assert!(calls.is_empty());
    }
}
