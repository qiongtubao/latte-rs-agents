//! `ask` 工具阻塞模式的回答路由：`choice_id` → 一次性答案通道。
//!
//! 顶层 turn 的 ask 是 fire-and-forget（选择结果作为下一条 user 消息
//! 回喂）；workflow / delegate 子代理没有"下一轮"——它的 ask 必须
//! 挂起等用户回答。子代理调 ask 时在本模块注册 `choice_id`，UI 提交
//! 答案（`POST /api/chat/choice-answer`）时经 [`resolve`] 送达。
//!
//! 进程级全局表即可：`choice_id` 由进程级单调序号保证全局唯一，
//! 单进程 server 内不会撞键。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use tokio::sync::oneshot;

static PENDING: LazyLock<Mutex<HashMap<String, oneshot::Sender<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 阻塞中的 ask 注册等待通道。同一 `choice_id` 重复注册会顶掉旧通道
/// （旧等待方收到 `RecvError`，按超时同语义处理）。
pub fn register(choice_id: &str) -> oneshot::Receiver<String> {
    let (tx, rx) = oneshot::channel();
    PENDING.lock().unwrap().insert(choice_id.to_string(), tx);
    rx
}

/// 投递用户答案。返回 `false` 表示没有匹配的挂起提问（已回答 / 已
/// 超时清理 / 未知 id）——调用方应据此回 404 让前端降级。
pub fn resolve(choice_id: &str, answer: String) -> bool {
    match PENDING.lock().unwrap().remove(choice_id) {
        Some(tx) => {
            // 等待方可能已被取消（cancel/超时后 Receiver 已 drop），
            // 发送失败也视为"已送达不了"，按无挂起处理。
            tx.send(answer).is_ok()
        }
        None => false,
    }
}

/// 超时或取消时清理挂起项，避免泄漏。
pub fn cancel(choice_id: &str) {
    PENDING.lock().unwrap().remove(choice_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_then_resolve_delivers_answer() {
        let rx = register("choice-test-1");
        assert!(resolve("choice-test-1", "方案A".to_string()));
        assert_eq!(rx.await.expect("answer delivered"), "方案A");
        // 已消费：重复投递返回 false。
        assert!(!resolve("choice-test-1", "方案B".to_string()));
    }

    #[test]
    fn resolve_unknown_id_returns_false() {
        assert!(!resolve("choice-test-nope", "x".to_string()));
    }

    #[tokio::test]
    async fn cancel_drops_pending_entry() {
        let rx = register("choice-test-2");
        cancel("choice-test-2");
        assert!(!resolve("choice-test-2", "x".to_string()));
        // 发送端被移除后，等待方收到 Err（通道关闭）。
        assert!(rx.await.is_err());
    }
}
