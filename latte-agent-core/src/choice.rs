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

/// Drop guard：ask 等待的 future 被 drop（workflow 中止 / 外层超时熔断）
/// 时自动清理挂起项，避免 PENDING 泄漏。`cancel` 是幂等的——答案已
/// 送达后 remove 不存在的键无副作用。
pub struct ChoiceGuard(pub String);

impl Drop for ChoiceGuard {
    fn drop(&mut self) {
        cancel(&self.0);
    }
}

/// 是否有任何角色有挂起的阻塞 ask（未回答、未取消）。供 advisor
/// monitor 使用：任何角色在等用户选择时，advisor 不应介入。
pub fn has_any_pending() -> bool {
    !PENDING.lock().unwrap().is_empty()
}

/// 该 role 是否有挂起的阻塞 ask（未回答、未取消）。workflow 超时
/// 熔断据此判定「子代理不是卡死，而是在等用户回答」——此时暂停
/// wall-clock 倒计时，等用户把答案交给 choice-answer 再继续。
pub fn has_pending_for(role_id: &str) -> bool {
    let prefix = format!("choice-{role_id}-");
    PENDING
        .lock()
        .unwrap()
        .keys()
        .any(|k| k.starts_with(&prefix))
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

    /// ChoiceGuard 随 future 一起 drop 时必须清理 PENDING —— 否则
    /// `has_any_pending` 永远为真，advisor 被永久静音。
    #[test]
    fn choice_guard_drop_clears_pending() {
        let _rx = register("choice-guard-1");
        {
            let _g = ChoiceGuard("choice-guard-1".to_string());
            assert!(has_any_pending(), "guard 在作用域内，挂起项应存在");
        }
        assert!(
            !resolve("choice-guard-1", "x".to_string()),
            "guard drop 后挂起项应已被清理"
        );
    }

    /// `has_pending_for` 靠 `choice-{role_id}-` 前缀识别归属，而
    /// choice_id 由 `register_ask_tool` 用
    /// `format!("choice-{}-{}", role_id, seq)` 生成。两处格式一旦漂移，
    /// workflow 的「等用户时暂停超时倒计时」会静默失效（永远 false →
    /// 正在等用户作答的 step 被误报超时）。本测试锁死这个契约。
    #[test]
    fn has_pending_for_matches_generated_choice_id_shape() {
        // 与 controller.rs 的 choice_id 生成方式保持一致。
        let role_id = "architect";
        let choice_id = format!("choice-{}-{}", role_id, 7);
        let _rx = register(&choice_id);

        assert!(
            has_pending_for(role_id),
            "生成格式 {choice_id} 应被 has_pending_for(\"{role_id}\") 命中"
        );
        // 不误伤其它角色：前缀必须精确到 role_id 后紧跟 '-'。
        assert!(!has_pending_for("arch"), "前缀不完整的角色名不应命中");
        assert!(!has_pending_for("programmer"), "无关角色不应命中");

        cancel(&choice_id);
        assert!(!has_pending_for(role_id), "清理后不应再命中");
    }
}
