//! Tests for ChatRenderer trait + FanoutRenderer.

use std::sync::Arc;

use latte_agent_core::controller::{ChatEvent, RoleInfo};
use latte_agent_core::renderer::{ChatEventMetadata, ChatRenderer, FanoutRenderer};

/// 记录所有收到的事件，方便断言
#[derive(Default, Clone)]
struct RecordingRenderer {
    events: Arc<parking_lot::Mutex<Vec<String>>>,
}

impl RecordingRenderer {
    fn new() -> Self {
        Self { events: Arc::new(parking_lot::Mutex::new(Vec::new())) }
    }
    fn events(&self) -> Vec<String> {
        self.events.lock().clone()
    }
}

#[async_trait::async_trait]
impl ChatRenderer for RecordingRenderer {
    async fn on_role_turn(&self, meta: &ChatEventMetadata) {
        self.events.lock().push(format!("role_turn:{}", meta.role_id));
    }
    async fn on_status(&self, message: &str) {
        self.events.lock().push(format!("status:{}", message));
    }
    async fn on_paused(&self, reason: &str) {
        self.events.lock().push(format!("paused:{}", reason));
    }
    async fn on_done(&self) {
        self.events.lock().push("done".to_string());
    }
    async fn on_round_started(&self, round: u32) {
        self.events.lock().push(format!("round_started:{}", round));
    }
    async fn on_session_info(&self, task_id: &str, state: &str, turn: u32) {
        self.events.lock().push(format!("session_info:{}:{}:{}", task_id, state, turn));
    }
    async fn on_role_list(&self, roles: &[RoleInfo]) {
        self.events.lock().push(format!("role_list:{}", roles.len()));
    }
}

#[tokio::test]
async fn fanout_renderer_dispatches_to_all_children() {
    let r1 = RecordingRenderer::new();
    let r2 = RecordingRenderer::new();
    let fanout = FanoutRenderer::new(vec![Box::new(r1.clone()), Box::new(r2.clone())]);
    fanout.on_status("hello").await;
    let e1 = r1.events();
    let e2 = r2.events();
    assert_eq!(e1, vec!["status:hello"]);
    assert_eq!(e2, vec!["status:hello"]);
}

#[tokio::test]
async fn dispatch_event_routes_done() {
    let rec = RecordingRenderer::new();
    rec.dispatch_event(&ChatEvent::Done).await;
    assert_eq!(rec.events(), vec!["done"]);
}

#[tokio::test]
async fn dispatch_event_routes_round_started() {
    let rec = RecordingRenderer::new();
    rec.dispatch_event(&ChatEvent::RoundStarted { round: 5 }).await;
    assert_eq!(rec.events(), vec!["round_started:5"]);
}

#[tokio::test]
async fn dispatch_event_routes_session_info() {
    let rec = RecordingRenderer::new();
    rec.dispatch_event(&ChatEvent::SessionInfo {
        task_id: "task-1".into(),
        state: "Running".into(),
        turn: 3,
        roles: vec![],
    })
    .await;
    assert_eq!(rec.events(), vec!["session_info:task-1:Running:3"]);
}

#[tokio::test]
async fn dispatch_event_routes_role_list() {
    let rec = RecordingRenderer::new();
    rec.dispatch_event(&ChatEvent::RoleList {
        roles: vec![
            RoleInfo { id: "pm".into(), name: "产品经理".into(), icon: "📋".into() },
            RoleInfo { id: "programmer".into(), name: "程序员".into(), icon: "💻".into() },
        ],
    })
    .await;
    assert_eq!(rec.events(), vec!["role_list:2"]);
}

#[tokio::test]
async fn fanout_dispatches_done() {
    let r1 = RecordingRenderer::new();
    let r2 = RecordingRenderer::new();
    let fanout = FanoutRenderer::new(vec![Box::new(r1.clone()), Box::new(r2.clone())]);
    fanout.dispatch_event(&ChatEvent::Done).await;
    assert_eq!(r1.events(), vec!["done"]);
    assert_eq!(r2.events(), vec!["done"]);
}