//! latte-agent → latte-code-editor 桥接适配器
//!
//! 提供一组 Rust 函数，让 latte-code-editor 的 Tauri 后端可以直接调用
//! latte-agent 的核心功能（ChatController、delegate、SSE 事件流）。
//!
//! 使用方式：在 latte-code-editor 的 chat_panel/commands.rs 中调用这些函数，
//! 不再需要重复实现 agent 逻辑。

/// 将 agent-ui 的 ChatEvent 转换为 code-editor 的 ControllerEventPayload
///
/// agent-ui SSE 格式（内部标记）：
///   {"type":"RoleTurn","role_id":"manager","content":"...","is_complete":true}
///
/// code-editor Tauri 格式（外部标记 + kind 字段）：
///   {"kind":"roleTurn","role_id":"manager","content":"...","is_complete":true}
pub fn chat_event_to_controller_payload(
    event_json: &str,
) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(event_json).map_err(|e| format!("parse: {e}"))?;

    match value {
        serde_json::Value::Object(mut map) => {
            // 提取 type 字段作为 kind
            let kind = map
                .remove("type")
                .and_then(|v| v.as_str().map(String::from))
                .ok_or_else(|| "missing 'type' field".to_string())?;

            // 转换为 code-editor 的 kind 格式
            let code_kind = match kind.as_str() {
                "RoleTurn" => "roleTurn",
                "Status" => "status",
                "Prompt" => "prompt",
                "Paused" => "paused",
                "Resumed" => "resumed",
                "RoundStarted" => "roundStarted",
                "RoundEnded" => "roundEnded",
                "ToolUse" => "toolUse",
                "ToolResult" => "toolResult",
                "ToolError" => "toolError",
                "DelegateStarted" => "delegateStarted",
                "DelegateFinished" => "delegateFinished",
                "Done" => "done",
                "Error" => "error",
                _ => return Err(format!("unknown event type: {kind}")),
            };

            map.insert("kind".into(), serde_json::Value::String(code_kind.into()));
            serde_json::to_string(&serde_json::Value::Object(map))
                .map_err(|e| format!("serialize: {e}"))
        }
        serde_json::Value::String(kind) => {
            // Unit variant: "Done" → {"kind":"done"}
            let code_kind = match kind.as_str() {
                "Done" => "done",
                _ => return Err(format!("unknown unit variant: {kind}")),
            };
            Ok(format!(r#"{{"kind":"{code_kind}"}}"#))
        }
        other => Err(format!("unexpected event shape: {other}")),
    }
}

/// 将 code-editor 的 ControllerInput 转换为 agent-ui 的 SSE 消息格式
pub fn controller_input_to_chat_event(
    kind: &str,
    payload: &serde_json::Value,
) -> Result<String, String> {
    let agent_type = match kind {
        "roleTurn" => "RoleTurn",
        "status" => "Status",
        "toolUse" => "ToolUse",
        "toolResult" => "ToolResult",
        "done" => "Done",
        "error" => "Error",
        _ => return Err(format!("unknown controller event kind: {kind}")),
    };

    let mut map = serde_json::Map::new();
    map.insert("type".into(), serde_json::Value::String(agent_type.into()));
    if let serde_json::Value::Object(fields) = payload {
        for (k, v) in fields {
            map.insert(k.clone(), v.clone());
        }
    }
    serde_json::to_string(&serde_json::Value::Object(map))
        .map_err(|e| format!("serialize: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_turn_conversion() {
        let agent_event = r#"{"type":"RoleTurn","role_id":"manager","content":"hello","is_complete":true}"#;
        let converted = chat_event_to_controller_payload(agent_event).unwrap();
        assert!(converted.contains("\"kind\":\"roleTurn\""));
        assert!(converted.contains("\"role_id\":\"manager\""));
        assert!(converted.contains("\"content\":\"hello\""));
    }

    #[test]
    fn delegate_started_conversion() {
        let agent_event = r#"{"type":"DelegateStarted","from_role":"manager","to_role":"programmer","task":"read files","sub_id":"prog-1"}"#;
        let converted = chat_event_to_controller_payload(agent_event).unwrap();
        assert!(converted.contains("\"kind\":\"delegateStarted\""));
        assert!(converted.contains("\"from_role\":\"manager\""));
    }

    #[test]
    fn done_conversion() {
        let agent_event = r#""Done""#;
        let converted = chat_event_to_controller_payload(agent_event).unwrap();
        assert_eq!(converted, r#"{"kind":"done"}"#);
    }

    #[test]
    fn reverse_role_turn() {
        let code_event = r#"{"kind":"roleTurn","role_id":"programmer","content":"hi","is_complete":true}"#;
        let val: serde_json::Value = serde_json::from_str(code_event).unwrap();
        let kind = val.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let converted = controller_input_to_chat_event(kind, &val).unwrap();
        assert!(converted.contains("\"type\":\"RoleTurn\""));
        assert!(converted.contains("\"role_id\":\"programmer\""));
    }
}
