//! Tests for ChatRenderer implementations (CliRenderer + JsonRenderer).
//!
//! 验证：每个事件类型都能正确转成 CLI 文本 / JSON Lines 输出，
//! 字段命名、类型与前端 TS 协议层 (ChatProtocolMessage) 对齐。

use std::io::Write;
use std::sync::Arc;

use latte_agent_core::controller::ChatEvent;
use latte_agent_core::controller::RoleInfo;
use latte_agent_core::renderer::{ChatEventMetadata, ChatRenderer};
use super::super::cli_renderer::CliRenderer;
use super::super::json_renderer::JsonRenderer;

/// 把写入数据收集进共享 Vec<u8> 的 Writer。
struct CapturingWriter {
    buf: Arc<parking_lot::Mutex<Vec<u8>>>,
}

impl CapturingWriter {
    fn new() -> (Arc<parking_lot::Mutex<Vec<u8>>>, Arc<parking_lot::Mutex<Box<dyn Write + Send>>>) {
        let buf = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let writer = CapturingWriter { buf: buf.clone() };
        let shared: Arc<parking_lot::Mutex<Box<dyn Write + Send>>> =
            Arc::new(parking_lot::Mutex::new(Box::new(writer)));
        (buf, shared)
    }
}

impl Write for CapturingWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.lock().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn read_buf(buf: &Arc<parking_lot::Mutex<Vec<u8>>>) -> String {
    String::from_utf8(buf.lock().clone()).expect("output should be utf-8")
}

// ─── CliRenderer ───────────────────────────────────

#[tokio::test]
async fn cli_renderer_emits_status_with_brackets() {
    let (buf, writer) = CapturingWriter::new();
    let r = CliRenderer::new(writer);
    r.on_status("[round 1: started]").await;
    let out = read_buf(&buf);
    assert!(out.contains("[round 1: started]"), "got: {:?}", out);
}

#[tokio::test]
async fn cli_renderer_role_turn_writes_role_id_and_content() {
    let (buf, writer) = CapturingWriter::new();
    let r = CliRenderer::new(writer);
    r.on_role_turn(&ChatEventMetadata {
        role_id: "pm".into(),
        content: "需求分析完成".into(),
        is_complete: true,
    })
    .await;
    let out = read_buf(&buf);
    assert!(out.contains("pm"), "should contain role id: {:?}", out);
    assert!(out.contains("需求分析完成"), "should contain content: {:?}", out);
}

#[tokio::test]
async fn cli_renderer_paused_uses_color_code() {
    let (buf, writer) = CapturingWriter::new();
    let r = CliRenderer::new(writer);
    r.on_paused("用户暂停").await;
    let out = read_buf(&buf);
    assert!(out.contains("paused"), "should contain paused tag: {:?}", out);
    assert!(out.contains("用户暂停"), "should contain reason: {:?}", out);
}

#[tokio::test]
async fn cli_renderer_render_prompt_returns_string() {
    let (_buf, writer) = CapturingWriter::new();
    let r = CliRenderer::new(writer);
    let prompt = r.render_prompt("💬", "manager", "deepseek-v4-flash");
    assert!(prompt.contains("manager"), "prompt should contain role: {:?}", prompt);
    assert!(prompt.contains("deepseek-v4-flash"), "prompt should contain model: {:?}", prompt);
}

// ─── JsonRenderer ───────────────────────────────────

#[tokio::test]
async fn json_renderer_emits_line_delimited_json() {
    let (buf, writer) = CapturingWriter::new();
    let r = JsonRenderer::new(writer);
    r.on_role_turn(&ChatEventMetadata {
        role_id: "pm".into(),
        content: "x".into(),
        is_complete: true,
    })
    .await;
    r.on_status("[round 1]").await;
    r.on_done().await;
    let out = read_buf(&buf);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3, "expected 3 json lines, got: {:?}", lines);
    assert!(lines[0].contains("\"type\":\"role_turn\""), "line 0: {:?}", lines[0]);
    assert!(lines[0].contains("\"role_id\":\"pm\""), "line 0: {:?}", lines[0]);
    assert!(lines[1].contains("\"type\":\"status\""), "line 1: {:?}", lines[1]);
    assert!(lines[2].contains("\"type\":\"done\""), "line 2: {:?}", lines[2]);
}

#[tokio::test]
async fn json_renderer_role_list_includes_all_roles() {
    let (buf, writer) = CapturingWriter::new();
    let r = JsonRenderer::new(writer);
    let roles = vec![
        RoleInfo { id: "pm".into(), name: "产品经理".into(), icon: "📋".into() },
        RoleInfo { id: "programmer".into(), name: "程序员".into(), icon: "💻".into() },
    ];
    r.on_role_list(&roles).await;
    let out = read_buf(&buf);
    assert!(out.contains("\"type\":\"role_list\""), "got: {:?}", out);
    assert!(out.contains("pm"), "got: {:?}", out);
    assert!(out.contains("产品经理"), "got: {:?}", out);
}

#[tokio::test]
async fn json_renderer_round_events() {
    let (buf, writer) = CapturingWriter::new();
    let r = JsonRenderer::new(writer);
    r.on_round_started(3).await;
    r.on_round_ended(3).await;
    let out = read_buf(&buf);
    assert!(out.contains("\"type\":\"round_started\""), "got: {:?}", out);
    assert!(out.contains("\"round\":3"), "got: {:?}", out);
    assert!(out.contains("\"type\":\"round_ended\""), "got: {:?}", out);
}

#[tokio::test]
async fn json_renderer_tool_events() {
    let (buf, writer) = CapturingWriter::new();
    let r = JsonRenderer::new(writer);
    r.on_tool_use("programmer", "edit_file", "{}").await;
    r.on_tool_result("programmer", "edit_file", "ok").await;
    let out = read_buf(&buf);
    assert!(out.contains("\"tool_name\":\"edit_file\""), "got: {:?}", out);
    assert!(out.contains("\"type\":\"tool_use\""), "got: {:?}", out);
    assert!(out.contains("\"type\":\"tool_result\""), "got: {:?}", out);
}

#[tokio::test]
async fn dispatch_event_routes_to_correct_method() {
    let (buf, writer) = CapturingWriter::new();
    let r = JsonRenderer::new(writer);
    r.dispatch_event(&ChatEvent::Done).await;
    r.dispatch_event(&ChatEvent::Resumed).await;
    let out = read_buf(&buf);
    assert!(out.contains("\"type\":\"done\""), "got: {:?}", out);
    assert!(out.contains("\"type\":\"resumed\""), "got: {:?}", out);
}