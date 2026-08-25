//! `ask` 工具阻塞模式的回答路由：`choice_id` → 一次性答案通道。
//!
//! 顶层 turn 的 ask 是 fire-and-forget（选择结果作为下一条 user 消息
//! 回喂）；workflow / delegate 子代理没有"下一轮"——它的 ask 必须
//! 挂起等用户回答。子代理调 ask 时在本模块注册 `choice_id`，UI 提交
//! 答案（`POST /api/chat/choice-answer`）时经 [`resolve`] 送达。
//!
//! 进程级全局表即可：`choice_id` 由进程级单调序号保证全局唯一，
//! 单进程 server 内不会撞键。
//!
//! 每个挂起项还存了**弹框事件快照**和它所属的事件通道。broadcast
//! 是"发出去即丢弃"的：发事件那一刻没有 SSE 订阅者（用户切了
//! session / 关了 tab / 正在重连），弹框就永远送不到前端，而阻塞
//! 的 ask 会无限等下去。[`pending_for_channel`] 让新连上来的 SSE
//! 把仍在等待的弹框补发一遍（见 ui-server `events_sse`）。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use tokio::sync::{broadcast, oneshot};

use crate::controller::ChatEvent;

/// 一个挂起的阻塞 ask。
struct Pending {
    /// 答案投递通道（一次性）。
    answer: oneshot::Sender<String>,
    /// 原始 `ChatEvent::ChoiceRequested` 快照，供重连补发。
    event: ChatEvent,
    /// 该弹框所属的事件通道（= session 的 broadcast）。用
    /// `same_channel` 判归属，不必把 session_id 一路透传到工具层。
    chan: broadcast::Sender<ChatEvent>,
}

static PENDING: LazyLock<Mutex<HashMap<String, Pending>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 阻塞中的 ask 注册等待通道。同一 `choice_id` 重复注册会顶掉旧通道
/// （旧等待方收到 `RecvError`，按超时同语义处理）。
///
/// **必须在广播 `ChoiceRequested` 之前调用**：反过来的话，用户秒答
/// 时 [`resolve`] 找不到挂起项 → 404 → 前端降级成普通消息，而随后
/// 注册上的等待方再也收不到答案，工具无限挂起。
pub fn register(
    choice_id: &str,
    event: ChatEvent,
    chan: broadcast::Sender<ChatEvent>,
) -> oneshot::Receiver<String> {
    let (tx, rx) = oneshot::channel();
    PENDING.lock().unwrap().insert(
        choice_id.to_string(),
        Pending { answer: tx, event, chan },
    );
    rx
}

/// 仍在等待回答、且属于 `chan` 这个事件通道的弹框事件快照。
/// 新的 SSE 连接用它补发弹框（顺序按 `choice_id` 稳定）。
pub fn pending_for_channel(chan: &broadcast::Sender<ChatEvent>) -> Vec<ChatEvent> {
    let g = PENDING.lock().unwrap();
    let mut out: Vec<(&String, &ChatEvent)> = g
        .iter()
        .filter(|(_, p)| p.chan.same_channel(chan))
        .map(|(id, p)| (id, &p.event))
        .collect();
    out.sort_by(|a, b| a.0.cmp(b.0));
    out.into_iter().map(|(_, ev)| ev.clone()).collect()
}

/// 投递用户答案。返回 `false` 表示没有匹配的挂起提问（已回答 / 已
/// 超时清理 / 未知 id）——调用方应据此回 404 让前端降级。
pub fn resolve(choice_id: &str, answer: String) -> bool {
    match PENDING.lock().unwrap().remove(choice_id) {
        Some(p) => {
            // 等待方可能已被取消（cancel/超时后 Receiver 已 drop），
            // 发送失败也视为"已送达不了"，按无挂起处理。
            p.answer.send(answer).is_ok()
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

    /// 测试用弹框快照 + 独立通道。
    fn fixture(choice_id: &str) -> (ChatEvent, broadcast::Sender<ChatEvent>) {
        let (tx, _rx) = broadcast::channel(8);
        let ev = ChatEvent::ChoiceRequested {
            role_id: "tester".into(),
            choice_id: choice_id.to_string(),
            question: "选哪个？".into(),
            multi: false,
            layout: String::new(),
            allow_upload: false,
            wait: true,
            options: vec![],
        };
        (ev, tx)
    }

    /// 注册一个挂起项，返回 (answer_rx, 通道)。通道必须持有，drop 掉
    /// broadcast::Sender 会让 `same_channel` 的归属判断失去意义。
    fn reg(choice_id: &str) -> (oneshot::Receiver<String>, broadcast::Sender<ChatEvent>) {
        let (ev, tx) = fixture(choice_id);
        let rx = register(choice_id, ev, tx.clone());
        (rx, tx)
    }

    #[tokio::test]
    async fn register_then_resolve_delivers_answer() {
        let (rx, _chan) = reg("choice-test-1");
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
        let (rx, _chan) = reg("choice-test-2");
        cancel("choice-test-2");
        assert!(!resolve("choice-test-2", "x".to_string()));
        // 发送端被移除后，等待方收到 Err（通道关闭）。
        assert!(rx.await.is_err());
    }

    /// ChoiceGuard 随 future 一起 drop 时必须清理 PENDING —— 否则
    /// `has_any_pending` 永远为真，advisor 被永久静音。
    #[test]
    fn choice_guard_drop_clears_pending() {
        let (_rx, _chan) = reg("choice-guard-1");
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
        let (_rx, _chan) = reg(&choice_id);

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

    /// 补发只认自己的通道：A session 的挂起弹框不能漏到 B session
    /// 的 SSE 流里（弹框会串会话，用户在错误的会话里被问）。
    #[test]
    fn pending_for_channel_scopes_by_channel() {
        let (ev_a, chan_a) = fixture("choice-chan-a-1");
        let (ev_b, chan_b) = fixture("choice-chan-b-1");
        let _rx_a = register("choice-chan-a-1", ev_a, chan_a.clone());
        let _rx_b = register("choice-chan-b-1", ev_b, chan_b.clone());

        let a = pending_for_channel(&chan_a);
        assert_eq!(a.len(), 1, "只应补发本通道的挂起弹框");
        match &a[0] {
            ChatEvent::ChoiceRequested { choice_id, .. } => {
                assert_eq!(choice_id, "choice-chan-a-1")
            }
            other => panic!("补发的应是 ChoiceRequested，实际 {other:?}"),
        }
        // 克隆出来的 Sender 与原通道同源，也应命中。
        assert_eq!(pending_for_channel(&chan_a.clone()).len(), 1);

        cancel("choice-chan-a-1");
        assert!(
            pending_for_channel(&chan_a).is_empty(),
            "已回答/已取消的弹框不再补发（否则重连时弹出僵尸框）"
        );
        assert_eq!(pending_for_channel(&chan_b).len(), 1, "B 通道不受影响");
        cancel("choice-chan-b-1");
    }

    /// 答案先到、注册后到的竞态：`register` 必须先于弹框广播。本测试
    /// 锁住「注册后立刻能 resolve」这半边契约。
    #[tokio::test]
    async fn resolve_immediately_after_register_succeeds() {
        let (rx, _chan) = reg("choice-race-1");
        // 模拟用户在弹框推给前端的同一刻就点了选项。
        assert!(
            resolve("choice-race-1", "秒答".to_string()),
            "注册已完成，秒答必须能送达"
        );
        assert_eq!(rx.await.expect("answer delivered"), "秒答");
    }
}
