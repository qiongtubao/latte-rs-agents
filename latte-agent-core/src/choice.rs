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
//!
//! 同样的丢失问题对**非阻塞**弹框（fire-and-forget 的 ask、
//! `PlanProposed`）一样存在，只是后果不是"卡死"而是"用户根本没看到
//! 这个弹框"。它们没有等待方、不进 `PENDING`，所以另有一张
//! [`PROMPTS`] 快照表：[`register_prompt`] 登记、[`dismiss_prompt`]
//! 在用户处理后销账、[`pending_prompts_for_channel`] 补发。
//! [`pending_dialogs_for_channel`] 是两者的并集，SSE 补发与
//! `GET /api/chat/pending-prompts` 共用。
//!
//! 注意 [`has_any_pending`] / [`has_pending_for`] 只看 `PENDING`：
//! 它们的语义是"有角色正阻塞等用户"（advisor 静音、workflow 超时
//! 倒计时暂停）。非阻塞弹框没人在等，若也算进去，用户忽略一个
//! plan 弹窗就会永久静音 advisor、永久冻结超时熔断。

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

/// 一个未处理的**非阻塞**弹框（fire-and-forget 的 ask、`PlanProposed`）。
///
/// 这类弹框没有等待方，所以从不进 [`PENDING`]——过去它们的唯一副本
/// 就是那一次 broadcast：发出的瞬间没有 SSE 订阅者（切了 session、
/// 关了 tab、EventSource 正在重连），或 broadcast 落后（`Lagged` 那
/// 几条既不进 SSE 也不进 archiver 的 event_log），弹框就永久消失，
/// 用户既没看到也无从补救。这里存一份快照，让新连接 / 显式补拉
/// （`GET /api/chat/pending-prompts`）能重新把它渲染出来。
struct Prompt {
    /// 幂等键：ask 用 `choice_id`，plan 用 `plan_id`。
    id: String,
    /// 弹框事件快照。
    event: ChatEvent,
    /// 所属事件通道（= session 的 broadcast），`same_channel` 判归属。
    chan: broadcast::Sender<ChatEvent>,
    /// 落盘归属 `(cwd, session_id)`。`Some` 时这条快照同时写进
    /// `<cwd>/.latte/pending-asks/`，[`dismiss_prompt`] 时一并删除。
    ///
    /// 为什么需要：`PROMPTS` 是**进程内**内存表，服务器一重启就空了。
    /// 而非阻塞弹框（fire-and-forget 的 ask、`PlanProposed`）此前只有
    /// 这一份副本 —— 用户没来得及处理就重启/刷新，弹框永久消失，既
    /// 看不到也无从补救（jemalloc 现场：workflow 挂死 + 进程重启后，
    /// 待办弹框连痕迹都不剩）。
    persist: Option<(std::path::PathBuf, String)>,
}

/// 每个通道最多保留的未处理弹框数。用户忽略掉的弹框不会有人来
/// `dismiss_prompt`，无上限会随会话时长单调增长（且每次重连都全量
/// 补发）。超出时淘汰同通道最老的一条。
const MAX_PROMPTS_PER_CHANNEL: usize = 16;

/// 按插入顺序保留（补发时保持时间序，Vec 而非 HashMap）。
static PROMPTS: LazyLock<Mutex<Vec<Prompt>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// 登记一个未处理的非阻塞弹框快照。**在广播事件之前调用**（与
/// [`register`] 同理：用户可能在事件刚到前端就点掉，dismiss 早于
/// 登记会留下一条僵尸快照，下次重连又弹一遍）。
///
/// 同 `id` 重复登记按后者覆盖（幂等：slash 路径与 plan 工具都可能
/// 对同一份清单代发）。
///
/// `persist` = `Some((cwd, session_id))` 时**同时落盘**到
/// `<cwd>/.latte/pending-asks/`：内存表只能救"弹框事件丢了"，救不了
/// "进程没了"。`None`（CLI / 测试 / 拿不到 session 的路径）只进内存。
pub fn register_prompt(
    id: &str,
    event: ChatEvent,
    chan: broadcast::Sender<ChatEvent>,
    persist: Option<(&std::path::Path, &str)>,
) {
    // 落盘先于内存登记：反序时若进程在两步之间挂掉，内存那份也随之
    // 消失，等于什么都没登记；先落盘则至少盘上有据可查。
    if let Some((cwd, sid)) = persist {
        match crate::event_json::chat_event_to_frontend_json(&event) {
            Ok(json) => crate::pending_ask::persist_prompt(cwd, id, sid, &json),
            Err(e) => tracing::warn!(
                prompt_id = %id,
                error = %e,
                "非阻塞弹框快照序列化失败，跳过落盘（本进程内仍可处理）"
            ),
        }
    }
    let owned = persist.map(|(cwd, sid)| (cwd.to_path_buf(), sid.to_string()));
    let mut g = PROMPTS.lock().unwrap();
    if let Some(slot) = g.iter_mut().find(|p| p.id == id) {
        slot.event = event;
        slot.chan = chan;
        slot.persist = owned;
        return;
    }
    g.push(Prompt { id: id.to_string(), event, chan: chan.clone(), persist: owned });
    // 只对同通道计数：A session 的弹框不该被 B session 的挤掉。
    while g.iter().filter(|p| p.chan.same_channel(&chan)).count() > MAX_PROMPTS_PER_CHANNEL {
        if let Some(pos) = g.iter().position(|p| p.chan.same_channel(&chan)) {
            // 淘汰时一并删盘，否则被挤掉的那条在重启后又会被
            // load_for_session 捞出来补发（内存里已经没有它了）。
            if let Some((cwd, _)) = g[pos].persist.as_ref() {
                crate::pending_ask::remove_prompt(cwd, &g[pos].id);
            }
            g.remove(pos);
        } else {
            break;
        }
    }
}

/// 用户已处理该弹框（提交了选择 / 跳过 / 导入了任务清单）→ 不再补发。
/// 返回是否命中（幂等，未命中不是错误）。
///
/// 命中且该快照登记过落盘归属时，同步删掉盘上那份 —— 否则重启后
/// [`crate::pending_ask::load_for_session`] 会把已处理的弹框当成待办
/// 再弹一遍。
pub fn dismiss_prompt(id: &str) -> bool {
    let mut g = PROMPTS.lock().unwrap();
    let before = g.len();
    for p in g.iter().filter(|p| p.id == id) {
        if let Some((cwd, _)) = p.persist.as_ref() {
            crate::pending_ask::remove_prompt(cwd, id);
        }
    }
    g.retain(|p| p.id != id);
    g.len() != before
}

/// 删掉盘上那条非阻塞弹框记录，**不要求内存表里还有它**。
///
/// 为什么单独给一个入口：[`dismiss_prompt`] 靠内存表里的 `persist`
/// 找落盘位置，可进程重启后 `PROMPTS` 是空的 —— 用户此时处理的正是
/// 从盘上补发出来的那条弹框，走 `dismiss_prompt` 会因为"未命中"而
/// 不删盘，于是下次刷新它又回来了。ui-server 的 dismiss 路径能拿到
/// `cwd`，用这个函数兜底。幂等。
pub fn dismiss_persisted_prompt(cwd: &std::path::Path, id: &str) {
    crate::pending_ask::remove_prompt(cwd, id);
}

/// 属于 `chan` 且仍未处理的非阻塞弹框快照，按发生顺序返回。
pub fn pending_prompts_for_channel(chan: &broadcast::Sender<ChatEvent>) -> Vec<ChatEvent> {
    PROMPTS
        .lock()
        .unwrap()
        .iter()
        .filter(|p| p.chan.same_channel(chan))
        .map(|p| p.event.clone())
        .collect()
}

/// 该通道（session）需要补发的**全部**弹框：阻塞 ask（[`PENDING`]，
/// 按 `choice_id` 稳定排序）+ 未处理的非阻塞弹框（[`PROMPTS`]，时间序）。
/// SSE 新连接与 `GET /api/chat/pending-prompts` 都用这一份，保证两条
/// 补齐路径看到的是同一个集合。
pub fn pending_dialogs_for_channel(chan: &broadcast::Sender<ChatEvent>) -> Vec<ChatEvent> {
    let mut out = pending_for_channel(chan);
    out.extend(pending_prompts_for_channel(chan));
    out
}

/// 丢弃该通道所有未处理的非阻塞弹框（session 删除 / 归档时调用，
/// 防止随进程生命周期泄漏）。
pub fn clear_prompts_for_channel(chan: &broadcast::Sender<ChatEvent>) {
    PROMPTS.lock().unwrap().retain(|p| !p.chan.same_channel(chan));
}

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

    // ── 非阻塞弹框补发表（PROMPTS） ──

    /// 非阻塞弹框的核心契约：登记后能按通道补发，销账后不再补发。
    /// 没有这张表，fire-and-forget 的 ask 与 PlanProposed 只有
    /// broadcast 那一份副本，没订阅者/Lagged 就永久消失。
    #[test]
    fn prompt_replayed_until_dismissed() {
        let (ev, chan) = fixture("prompt-1");
        register_prompt("prompt-1", ev, chan.clone(), None);
        assert_eq!(pending_prompts_for_channel(&chan).len(), 1);
        assert!(dismiss_prompt("prompt-1"), "首次销账应命中");
        assert!(
            pending_prompts_for_channel(&chan).is_empty(),
            "已处理的弹框不再补发（否则重连弹僵尸框）"
        );
        // 幂等：重复销账不是错误（另一个 tab 可能已处理过）。
        assert!(!dismiss_prompt("prompt-1"));
    }

    /// 补发只认自己的通道 —— 与阻塞表同样的隔离要求，否则 A 会话的
    /// plan 弹窗会在 B 会话弹出来。
    #[test]
    fn prompts_scope_by_channel() {
        let (ev_a, chan_a) = fixture("prompt-chan-a");
        let (ev_b, chan_b) = fixture("prompt-chan-b");
        register_prompt("prompt-chan-a", ev_a, chan_a.clone(), None);
        register_prompt("prompt-chan-b", ev_b, chan_b.clone(), None);
        assert_eq!(pending_prompts_for_channel(&chan_a).len(), 1);
        // 克隆的 Sender 与原通道同源，也应命中。
        assert_eq!(pending_prompts_for_channel(&chan_a.clone()).len(), 1);
        clear_prompts_for_channel(&chan_a);
        assert!(
            pending_prompts_for_channel(&chan_a).is_empty(),
            "session 删除后该通道的快照应清空"
        );
        assert_eq!(
            pending_prompts_for_channel(&chan_b).len(),
            1,
            "B 通道不受影响"
        );
        clear_prompts_for_channel(&chan_b);
    }

    /// 同 id 重复登记按覆盖处理（幂等）：slash 路径与 plan 工具都可能
    /// 对同一份清单代发，补发两张一样的卡片是 UI bug。
    #[test]
    fn prompt_register_is_idempotent_by_id() {
        let (ev1, chan) = fixture("prompt-dup");
        let (ev2, _other) = fixture("prompt-dup");
        register_prompt("prompt-dup", ev1, chan.clone(), None);
        register_prompt("prompt-dup", ev2, chan.clone(), None);
        assert_eq!(pending_prompts_for_channel(&chan).len(), 1);
        clear_prompts_for_channel(&chan);
    }

    /// 用户忽略掉的弹框没人来销账，无上限会随会话时长单调增长，且每次
    /// 重连都全量补发。超出上限时淘汰同通道最老的一条。
    #[test]
    fn prompts_are_capped_per_channel() {
        let (_seed, chan) = fixture("prompt-cap-seed");
        for i in 0..(MAX_PROMPTS_PER_CHANNEL + 5) {
            let id = format!("prompt-cap-{i}");
            let (ev, _c) = fixture(&id);
            register_prompt(&id, ev, chan.clone(), None);
        }
        let pending = pending_prompts_for_channel(&chan);
        assert_eq!(pending.len(), MAX_PROMPTS_PER_CHANNEL);
        // 淘汰最老的 → 头部是第 5 条（0..4 被挤掉），顺序仍是时间序。
        match &pending[0] {
            ChatEvent::ChoiceRequested { choice_id, .. } => {
                assert_eq!(choice_id, "prompt-cap-5", "应淘汰同通道最老的几条")
            }
            other => panic!("期望 ChoiceRequested，实际 {other:?}"),
        }
        clear_prompts_for_channel(&chan);
    }

    /// 非阻塞弹框**不得**算进阻塞表：`has_any_pending` /
    /// `has_pending_for` 的语义是「有角色正阻塞等用户」，用来静音
    /// advisor、冻结 workflow 超时倒计时。把用户忽略掉的 plan 弹窗也算
    /// 进去，等于永久静音 advisor、永久冻结熔断。
    /// （断言用通道/角色维度而非全局 `has_any_pending`：全局表被并行
    /// 跑的其它测试共享，全局断言会 flaky。）
    #[test]
    fn prompts_do_not_count_as_blocking_pending() {
        let (ev, chan) = fixture("choice-nonblockingrole-1");
        register_prompt("choice-nonblockingrole-1", ev, chan.clone(), None);
        assert!(
            pending_for_channel(&chan).is_empty(),
            "非阻塞弹框不该进阻塞表"
        );
        assert!(
            !has_pending_for("nonblockingrole"),
            "非阻塞弹框不该让 has_pending_for 为真（超时熔断会被永久冻结）"
        );
        assert_eq!(
            pending_prompts_for_channel(&chan).len(),
            1,
            "但它必须在非阻塞补发表里"
        );
        clear_prompts_for_channel(&chan);
    }

    /// SSE 与 `GET /api/chat/pending-prompts` 共用同一个集合：阻塞项在
    /// 前、非阻塞项在后，两条补齐路径必须看到一样的内容，否则一条路
    /// 径补出来的卡片在另一条路径下会缺。
    #[test]
    fn dialogs_union_covers_blocking_and_prompts() {
        let (ev_block, chan) = fixture("choice-union-block");
        let _rx = register("choice-union-block", ev_block, chan.clone());
        let (ev_prompt, _c) = fixture("prompt-union");
        register_prompt("prompt-union", ev_prompt, chan.clone(), None);

        let ids: Vec<String> = pending_dialogs_for_channel(&chan)
            .iter()
            .map(|ev| match ev {
                ChatEvent::ChoiceRequested { choice_id, .. } => choice_id.clone(),
                other => panic!("期望 ChoiceRequested，实际 {other:?}"),
            })
            .collect();
        assert_eq!(ids, vec!["choice-union-block", "prompt-union"]);

        cancel("choice-union-block");
        clear_prompts_for_channel(&chan);
    }
}
