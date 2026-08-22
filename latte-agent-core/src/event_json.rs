//! ChatEvent → 前端 wire JSON 的**单一实现**（契约 C2，见
//! `docs/ui-embedding-design.md` §1）。
//!
//! `ChatEvent` 的 serde 默认表示是 externally-tagged
//! （`{"RoleTurn":{...}}`），这是 `latte-code-editor` 与
//! `chat --output json` 消费的 canonical 格式。Web 前端
//! （`latte-agent-cli/ui`，Vite + React）期望 internally-tagged
//! discriminated union（`{"type":"RoleTurn",...}`）。本模块是两者
//! 之间唯一的转换点：`latte-agent-ui-server`（axum + SSE）与未来
//! Tauri IPC 适配器都必须调用这里，避免两侧格式漂移。

use crate::controller::ChatEvent;

/// Convert a `ChatEvent` (externally-tagged JSON object) into the
/// `latte-agent-ui` frontend wire shape (internally-tagged):
///
///   externally:  `{"RoleTurn":{"role_id":"m","content":"hi","is_complete":true}}`
///   internally:  `{"type":"RoleTurn","role_id":"m","content":"hi","is_complete":true}`
///
///   externally:  `{"Done"}`
///   internally:  `{"type":"Done"}`
///
/// All field names stay the same (no `rename_all`); the only
/// transformation is "lift the outer variant key into a `type`
/// discriminator". This matches the TS `ChatEvent` union in
/// `latte-agent-cli/ui/src/api.ts`, so the front-end
/// `switch (e.type)` lands on the right branch.
///
/// `serde_json::Value::Object` guarantees insertion order is
/// preserved on serialization, so we can re-insert `type` first
/// then spread the variant's fields — the resulting string is
/// stable and human-readable in browser dev tools.
pub fn chat_event_to_frontend_json(ev: &ChatEvent) -> Result<String, String> {
    let value = serde_json::to_value(ev).map_err(|e| e.to_string())?;
    match value {
        // Named-field / tuple variant: ChatEvent::RoleTurn { .. } →
        // externally-tagged object `{"RoleTurn":{...}}`.
        serde_json::Value::Object(outer) => {
            if outer.len() != 1 {
                return Err(format!(
                    "ChatEvent should serialize to exactly 1-key object, got {} keys: {:?}",
                    outer.len(),
                    outer.keys().collect::<Vec<_>>()
                ));
            }
            let (variant_name, fields) = outer.into_iter().next().unwrap();
            // `fields` is a Value::Object for variants with named
            // fields. We lift `variant_name` into a `type` discriminator
            // and flatten the fields up to the top level. Unit-like
            // variants can't reach this branch because they serialize
            // to a bare string (handled below).
            let mut out = serde_json::Map::with_capacity(
                1 + fields.as_object().map(|m| m.len()).unwrap_or(0)
            );
            out.insert("type".to_string(), serde_json::Value::String(variant_name));
            if let serde_json::Value::Object(inner) = fields {
                for (k, v) in inner {
                    out.insert(k, v);
                }
            }
            serde_json::to_string(&serde_json::Value::Object(out)).map_err(|e| e.to_string())
        }
        // Unit variant: ChatEvent::Done → bare string `"Done"`.
        // We wrap it as `{"type":"Done"}` to match the TS discriminated
        // union shape (every other case uses the same envelope).
        serde_json::Value::String(variant_name) => {
            Ok(format!(r#"{{"type":{}}}"#, serde_json::to_string(&variant_name).map_err(|e| e.to_string())?))
        }
        // Anything else is a contract bug — surface it instead of
        // silently dropping the event (which is what triggered the
        // original `[chat] unknown event` bug).
        other => Err(format!("unexpected ChatEvent shape: {}", other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::{ChatEvent, RoleInfo};

    /// chat_event_to_frontend_json 必须把 ChatEvent 的 externally-tagged
    /// JSON 转成前端 latte-agent-ui 期望的 internally-tagged JSON。
    /// 这是协议适配层的关键测试 — 改坏会触发 [chat] unknown event。
    #[test]
    fn chat_event_to_frontend_json_handles_status_with_message() {
        let ev = ChatEvent::Status { message: "[calling LLM...]".into() };
        let json = chat_event_to_frontend_json(&ev).expect("convert");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
        // 外层 key = type
        assert_eq!(v["type"], "Status");
        // 字段被平铺到外层
        assert_eq!(v["message"], "[calling LLM...]");
        // 不能残留旧的 "Status" 外层对象
        assert!(v.get("Status").is_none(), "stale externally-tagged object leaked: {}", v);
    }

    #[test]
    fn chat_event_to_frontend_json_handles_user_message() {
        let ev = ChatEvent::UserMessage { text: "分析一下 Cargo.toml".into() };
        let json = chat_event_to_frontend_json(&ev).expect("convert");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(v["type"], "UserMessage");
        assert_eq!(v["text"], "分析一下 Cargo.toml");
        assert!(v.get("UserMessage").is_none());
        // roundtrip：转出的 JSON 再 parse 回来字段不丢
        assert_eq!(v.as_object().unwrap().len(), 2);
    }

    #[test]
    fn chat_event_to_frontend_json_handles_unit_variant() {
        let ev = ChatEvent::Done;
        let json = chat_event_to_frontend_json(&ev).expect("convert");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
        // Done 是 unit variant —— 序列化是 `{"Done":null}`。
        // 转换后只有 `type` 字段，没有额外字段。
        assert_eq!(v["type"], "Done");
        assert_eq!(v.as_object().unwrap().len(), 1, "Done should be a single-field object, got {}", v);
    }

    #[test]
    fn chat_event_to_frontend_json_handles_roleturn_with_all_fields() {
        let ev = ChatEvent::RoleTurn {
            role_id: "manager".into(),
            content: "你好".into(),
            is_complete: true,
            sub_id: None,
        };
        let json = chat_event_to_frontend_json(&ev).expect("convert");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(v["type"], "RoleTurn");
        assert_eq!(v["role_id"], "manager");
        assert_eq!(v["content"], "你好");
        assert_eq!(v["is_complete"], true);
        assert!(v.get("RoleTurn").is_none());
    }

    /// 端到端协议层：events_sse 推出来的数据必须能被前端 api.ts
    /// 里的 ChatEvent union 匹配。验证方法是：把 ChatEvent 经过
    /// chat_event_to_frontend_json → JSON.stringify 模拟前端 dispatch_event
    /// 流程，确认每种类型都能找到 type 字段。
    #[test]
    fn chat_event_to_frontend_json_every_variant_has_type_field() {
        let cases: Vec<(&str, ChatEvent)> = vec![
            ("Status", ChatEvent::Status { message: "x".into() }),
            ("UserMessage", ChatEvent::UserMessage { text: "hi".into() }),
            ("Paused", ChatEvent::Paused { reason: "ask_human".into() }),
            ("Resumed", ChatEvent::Resumed),
            ("RoundStarted", ChatEvent::RoundStarted { round: 1 }),
            ("RoundEnded", ChatEvent::RoundEnded { round: 1 }),
            ("Done", ChatEvent::Done),
            ("Error", ChatEvent::Error { kind: None, message: "boom".into(), sub_id: None }),
            ("ContextCleared", ChatEvent::ContextCleared),
            ("ToolUse", ChatEvent::ToolUse { role_id: "m".into(), tool_name: "read".into(), args: "{}".into(), sub_id: None }),
            ("ToolResult", ChatEvent::ToolResult { role_id: "m".into(), tool_name: "read".into(), result: "ok".into(), sub_id: None }),
            ("ToolError", ChatEvent::ToolError { role_id: "m".into(), tool_name: "read".into(), error: "fail".into(), sub_id: None }),
            ("RoleStarted", ChatEvent::RoleStarted { role_id: "m".into(), detail: "calling LLM".into(), sub_id: None }),
            ("RoleFinished", ChatEvent::RoleFinished { role_id: "m".into(), detail: "ok".into(), sub_id: None }),
            ("RolePaused", ChatEvent::RolePaused { role_id: "programmer".into() }),
            ("RoleResumed", ChatEvent::RoleResumed { role_id: "programmer".into() }),
            ("DelegateStarted", ChatEvent::DelegateStarted { from_role: "manager".into(), to_role: "programmer".into(), task: "ping".into(), sub_id: "x".into(), wf_id: None }),
            ("DelegateFinished", ChatEvent::DelegateFinished { from_role: "manager".into(), to_role: "programmer".into(), status: "ok".into(), summary: "done".into(), sub_id: "x".into(), wf_id: None }),
            ("SessionInfo", ChatEvent::SessionInfo { task_id: "ui-1".into(), state: "running".into(), turn: 0, roles: vec![RoleInfo { id: "manager".into(), name: "Manager".into(), icon: "[m]".into() }] }),
            ("AdvisorTerminated", ChatEvent::AdvisorTerminated { role_id: "manager".into(), reason: "gate D5 重试耗尽".into(), detector: Some("D5".into()), sub_id: None }),
        ];
        for (expected_type, ev) in cases {
            let json = chat_event_to_frontend_json(&ev).expect("convert");
            let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
            assert_eq!(v["type"], expected_type, "type mismatch for variant {}: {}", expected_type, v);
        }
    }
    /// Error 事件携带 sub_id 是 UI 右键「查看日志」的关键字段。
    /// 验证：sub_id 必须出现在前端 wire JSON 里（不要被 serde
    /// 误标 skip_serializing_if 而丢掉）。
    #[test]
    fn chat_event_to_frontend_json_preserves_error_sub_id() {
        let ev = ChatEvent::Error {
            kind: Some(crate::trace::ModelErrorKind::Other { message: "boom".into() }),
            message: "delegate programmer failed: timeout".into(),
            sub_id: Some("sub-42".into()),
        };
        let json = chat_event_to_frontend_json(&ev).expect("convert");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(v["type"], "Error");
        assert_eq!(v["message"], "delegate programmer failed: timeout");
        assert_eq!(v["sub_id"], "sub-42", "sub_id must reach frontend");
        // 没 sub_id 的 Error 不应该泄漏空字符串字段
        let no_sub = ChatEvent::Error { kind: None, message: "main turn failed".into(), sub_id: None };
        let v2: serde_json::Value = serde_json::from_str(
            &chat_event_to_frontend_json(&no_sub).expect("convert")
        ).expect("parse");
        assert!(v2.get("sub_id").is_none(), "sub_id must be skipped when None");
    }

    /// 工具事件携带 sub_id 是前端把 delegate/workflow 子代理的工具
    /// 调用精确归属到 subsession（而不是泄进主 session 按 role 猜）
    /// 的关键字段。验证：sub_id 出现在 wire JSON；None 时字段被跳过，
    /// 旧前端/旧归档回放不受影响。
    #[test]
    fn chat_event_to_frontend_json_preserves_tool_sub_id() {
        let ev = ChatEvent::ToolUse {
            role_id: "task_planner".into(),
            tool_name: "read".into(),
            args: "{}".into(),
            sub_id: Some("task_planner-1".into()),
        };
        let json = chat_event_to_frontend_json(&ev).expect("convert");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(v["type"], "ToolUse");
        assert_eq!(v["sub_id"], "task_planner-1", "sub_id must reach frontend");

        let no_sub = ChatEvent::ToolResult {
            role_id: "manager".into(),
            tool_name: "read".into(),
            result: "ok".into(),
            sub_id: None,
        };
        let v2: serde_json::Value = serde_json::from_str(
            &chat_event_to_frontend_json(&no_sub).expect("convert")
        ).expect("parse");
        assert!(v2.get("sub_id").is_none(), "sub_id must be skipped when None");
    }
}
