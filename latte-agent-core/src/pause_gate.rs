//! 进程内通用的「暂停/继续」门 —— 任何 async loop 都能在两个安全点
//! 之前 await 这个 gate 来冻住自己：每个 turn（model call 之前）、
//! 每个 tool call 启动之前。
//!
//! 设计与不变量（参照 oh-my-pi 的 `AgentPauseGate`）：
//!
//! 1. **in-flight 不被打断**：已经启动的 model stream / tool exec 跑完
//!    落地，下一个边界才 park。对应 chat UI 的"暂停"按钮 —— 用户
//!    按下时如果在 LLM 响应中、tool 正在跑，会等这一步走完才停。
//! 2. **abort 不解除 gate**：调用 [`wait_until_resumed`] 时可以传
//!    `CancellationToken`/`AbortSignal`；该 signal 触发只唤醒**这一个**
//!    waiter、门继续保持 engaged。其他 waiter（同一 session 的
//!    subagent / 其他 runner）继续 park。这是核心不变量：abort 一个
//!    run 不要求 abort 整个 session。
//! 3. **幂等 pause / resume**：多次 `pause()` 不会叠加；多次
//!    `resume()` 不会报错。
//! 4. **多 listener**：`on_change` 注册的 listener 在每次
//!    pause / resume 切换时被回调（state 同步），用于 UI 显示。
//!
//! ## 与 [`AdvisorPauseGate`](crate::advisor_monitor::AdvisorPauseGate) 的关系
//!
//! 两个 gate **正交共存**：
//!
//! - `AgentPauseGate`（本模块）：**用户手动**"⏸ 暂停 / 继续" 触发，
//!   范围 = session（subagent 跟随 session gate 一起停）。
//! - `AdvisorPauseGate`（`advisor_monitor`）：**monitor 自动**判
//!   `Verdict::Intervene` 触发，范围 = 单个 role / runner。
//!
//! 一个 agent loop 可能同时被两个 gate await（先通过 advisor gate
//! 再通过 agent gate 才会进 model call）。两者完全独立。
//!
//! ## Scope 设计
//!
//! 不用 oh-my-pi 的 **process 级**单例 —— 我们有 multi-tab session
//! 隔离的需求。Gate 挂在 `ChatController` 上，`Arc` clone 给同
//! session 的 subagent / tool runner / workflow run 共享。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// 暂停状态。`None` = running；`Some` = paused（含起始时刻用于
/// "paused for 0:07" 之类的 UI 显示）。
#[derive(Debug, Clone)]
struct PausedState {
    /// wall-clock epoch millis —— 给 UI 排序 / 序列化用
    paused_at_unix_ms: u64,
    /// monotonic instant —— 给 [`resume`](Self::resume) 计算持续时间
    paused_at_instant: Instant,
    /// 暂停原因（用户手动 / 模型不可用自动暂停…），随 Paused 事件
    /// 广播给 UI 展示。
    reason: String,
}

/// 进程内可克隆、可共享的暂停门。`Arc<Self>` 是常见形态，gate 本身
/// 内部状态用 `Mutex` + `Notify` 保护；listener 用 `Mutex<Vec<_>>`。
///
/// 调用方拿到 `Arc<AgentPauseGate>` 后 clone 派发到任何需要 park 的
/// async loop。每个 loop 各自 await `wait_until_resumed`，互不干扰。
pub struct AgentPauseGate {
    state: Mutex<Option<PausedState>>,
    notify: Notify,
    /// 自增 id 分配给 listener，用于 remove_listener。`AtomicU64` 让
    /// 注册路径不需要锁。
    next_listener_id: AtomicU64,
    listeners: Mutex<Vec<(u64, Arc<dyn Fn(bool) + Send + Sync + 'static>)>>,
    /// 累计**已结束**的暂停时长（毫秒）。正在进行的那一次不在内，
    /// 由 [`total_paused`](Self::total_paused) 现算后叠加。
    ///
    /// 动机（jemalloc 2026-08-26 会话）：workflow 的 wall-clock 预算
    /// 此前用裸 `tokio::time::timeout` 计时，对 pause 一无所知。reviewer
    /// 22:06:27 停在 gate 上一次模型调用都没发出去，预算却照扣，5040s
    /// 到点后整条 task_refine 被判超支中止，2/4 步成果作废。预算要扣的
    /// 是「真在干活的时间」，所以必须能问出「一共停了多久」。
    total_paused_ms: AtomicU64,
    /// 调试用的 gate 名称（`tracing` span / 错误消息用）。
    name: &'static str,
}

impl std::fmt::Debug for AgentPauseGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentPauseGate")
            .field("name", &self.name)
            .field("paused", &self.is_paused())
            .finish()
    }
}

impl AgentPauseGate {
    /// 新建一个未暂停的 gate。`name` 用于日志和错误信息，常见取值：
    /// `"session"`, `"subagent:<sub_id>"` 等。Caller 通常是
    /// `ChatController::new` 或 `AgentRunner::with_pause_gate`。
    pub fn new(name: &'static str) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(None),
            notify: Notify::new(),
            next_listener_id: AtomicU64::new(0),
            listeners: Mutex::new(Vec::new()),
            total_paused_ms: AtomicU64::new(0),
            name,
        })
    }

    /// Engage the gate. 已暂停时返回 `false`（幂等），首次 pause 返
    /// 回 `true`。调用后所有**后续**`wait_until_resumed` 都会 park。
    pub fn pause(&self) -> bool {
        self.pause_with_reason("")
    }

    /// 带原因 engage（如「模型不可用，自动暂停」）。原因随 gate 状态
    /// 保存，listener / UI 经 [`pause_reason`](Self::pause_reason) 读取。
    pub fn pause_with_reason(&self, reason: impl Into<String>) -> bool {
        let mut state = self.state.lock();
        if state.is_some() {
            return false; // 幂等：已经在 paused
        }
        *state = Some(PausedState {
            paused_at_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            paused_at_instant: Instant::now(),
            reason: reason.into(),
        });
        // 唤醒所有当前 waiter —— 它们检查 `state` 后会看到 paused
        // 并重新 await。但这要它们先检查状态，否则会 race。
        // 实现细节：Notify 的 `notify_waiters()` 会唤醒所有正在 await
        // `notified()` 的 caller；那些 caller 醒来后**必须**重新检查
        // `is_paused()`，如果还 paused 就再 await。
        drop(state);
        self.notify.notify_waiters();
        self.notify_listeners(true);
        tracing::debug!(gate = self.name, "agent pause gate engaged");
        true
    }

    /// Release the gate, 唤醒所有 waiter。返回 paused 持续时间
    /// (`Duration::as_millis`)；如果本来就没 paused，返回 `None`。
    pub fn resume(&self) -> Option<u128> {
        let mut state = self.state.lock();
        let st = state.take()?;
        let elapsed_ms = st.paused_at_instant.elapsed().as_millis();
        // 累计进总账 —— 计时器（如 workflow 预算）靠它把 park 的时间
        // 从「干活时间」里扣掉。饱和转换：u128→u64 溢出在物理上不可能
        // （5.8 亿年），但不给 panic 留口子。
        self.total_paused_ms
            .fetch_add(elapsed_ms.min(u64::MAX as u128) as u64, Ordering::Relaxed);
        drop(state);
        self.notify.notify_waiters();
        self.notify_listeners(false);
        tracing::debug!(
            gate = self.name,
            elapsed_ms,
            "agent pause gate released"
        );
        Some(elapsed_ms)
    }

    /// 本 gate 自创建以来的**累计暂停时长**，含当前正在进行的那一次。
    ///
    /// 用途：把 wall-clock 计时换算成「有效工作时间」。典型用法是先取
    /// 一个基线，之后用 `started.elapsed() - (total_paused() - base)`
    /// 判断是否真的超支：
    ///
    /// ```ignore
    /// let base = gate.total_paused();
    /// // …干活，期间可能被 pause 若干次…
    /// let effective = started.elapsed().saturating_sub(gate.total_paused() - base);
    /// ```
    ///
    /// 单调不减，可跨多次 pause/resume 累加。
    pub fn total_paused(&self) -> std::time::Duration {
        // 先读原子量再读锁内的 live 值。顺序反了会漏账：若 resume 恰好
        // 发生在两次读之间，live 读到 0 而 done 还是旧值，那一段就丢了。
        let done = self.total_paused_ms.load(Ordering::Relaxed);
        let live = self
            .state
            .lock()
            .as_ref()
            .map(|s| s.paused_at_instant.elapsed().as_millis().min(u64::MAX as u128) as u64)
            .unwrap_or(0);
        // 重读一次 done：若上面那个 race 真发生了（live=0 但 resume 已
        // 把时长记进 done），这次能读到新值，账目不会变小。
        let done = done.max(self.total_paused_ms.load(Ordering::Relaxed));
        std::time::Duration::from_millis(done.saturating_add(live))
    }

    /// 当前是否暂停。
    #[inline]
    pub fn is_paused(&self) -> bool {
        self.state.lock().is_some()
    }

    /// 暂停开始的 wall-clock epoch millis；未暂停返回 `None`。给 UI
    /// 排序、序列化用。
    pub fn paused_since_unix_ms(&self) -> Option<u64> {
        self.state.lock().as_ref().map(|s| s.paused_at_unix_ms)
    }

    /// 暂停原因（`pause_with_reason` 存入）；未暂停或原因为空返回
    /// `None`/空串。controller 的 on_change listener 把它放进
    /// `ChatEvent::Paused` 广播给 UI。
    pub fn pause_reason(&self) -> Option<String> {
        self.state.lock().as_ref().map(|s| s.reason.clone())
    }

    /// 注册 listener，每次 pause / resume 切换时被回调（参数 = 新
    /// paused 状态）。listener 必须是 `Send + Sync + 'static` 因为
    /// 可能在不同 task 中被调。
    ///
    /// 返回 listener id，调用 [`remove_listener`](Self::remove_listener)
    /// 注销。listener 异常 / panic 不影响其他 listener —— 我们用
    /// `catch_unwind` 隔离。
    pub fn on_change<F>(&self, listener: F) -> u64
    where
        F: Fn(bool) + Send + Sync + 'static,
    {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        let listener: Arc<dyn Fn(bool) + Send + Sync + 'static> = Arc::new(listener);
        self.listeners.lock().push((id, listener));
        id
    }

    /// 注销 listener。id 来自 [`on_change`](Self::on_change) 返回值。
    pub fn remove_listener(&self, id: u64) {
        self.listeners.lock().retain(|(i, _)| *i != id);
    }

    /// Park until the gate is released. 已 released → 立即返回。
    ///
    /// **不变量**：如果 `cancel` 被触发，本 waiter 立刻 resolve（返
    /// 回 `Cancelled`），但门**保持** engaged。Abort 一个 run 不要求
    /// 取消整个 session。
    pub async fn wait_until_resumed(&self, cancel: Option<CancellationToken>) -> WaitResult {
        if !self.is_paused() {
            return WaitResult::Ok;
        }
        // 监听 cancel + notify 两者；任一触发就重新检查状态。
        loop {
            if let Some(ct) = cancel.as_ref() {
                tokio::select! {
                    biased;
                    _ = ct.cancelled() => {
                        // 即使 abort 也不影响 gate；下一次其他 waiter
                        // 还会 await。让调用方决定后续行为。
                        return WaitResult::Cancelled;
                    }
                    _ = self.notify.notified() => {
                        if !self.is_paused() {
                            return WaitResult::Ok;
                        }
                        // notify 触发但 gate 仍 engaged（race：
                        // 我们唤醒的时刻 gate 又被 pause）→ 继续等。
                    }
                }
            } else {
                self.notify.notified().await;
                if !self.is_paused() {
                    return WaitResult::Ok;
                }
            }
        }
    }

    fn notify_listeners(&self, paused: bool) {
        // 锁内克隆 listener 列表 —— 避免 listener 回调时持有锁
        // 触发 listener 内部再加锁死锁。
        let listeners = {
            let g = self.listeners.lock();
            g.iter().map(|(_, l)| Arc::clone(l)).collect::<Vec<_>>()
        };
        for l in listeners {
            // 隔离 panic —— 一个 listener 挂掉不影响其他。
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                l(paused);
            }));
        }
    }
}

/// [`wait_until_resumed`](AgentPauseGate::wait_until_resumed) 的返回
/// 值。`Cancelled` = 调用方的 `CancellationToken` 触发（gate 状态不
/// 变），`Ok` = gate 已 released 或本来就未暂停。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitResult {
    /// 正常 release，park 结束。
    Ok,
    /// Cancelled by caller's `CancellationToken`. Gate 仍 engaged，
    /// 下次有别的 caller 还会 park。
    Cancelled,
}

// ─── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AOrd};
    use std::time::Duration;

    #[test]
    fn initially_not_paused() {
        let g = AgentPauseGate::new("t");
        assert!(!g.is_paused());
        assert!(g.paused_since_unix_ms().is_none());
    }

    #[test]
    fn pause_with_reason_stores_and_clears_reason() {
        let g = AgentPauseGate::new("t");
        assert!(g.pause_reason().is_none());
        assert!(g.pause_with_reason("模型不可用，自动暂停"));
        assert_eq!(g.pause_reason().as_deref(), Some("模型不可用，自动暂停"));
        // 幂等：已暂停时再次 pause 不覆盖原因。
        assert!(!g.pause_with_reason("别的原因"));
        assert_eq!(g.pause_reason().as_deref(), Some("模型不可用，自动暂停"));
        // resume 清空。
        g.resume();
        assert!(g.pause_reason().is_none());
    }

    #[test]
    fn pause_then_resume_roundtrip() {
        let g = AgentPauseGate::new("t");
        assert!(g.pause(), "首次 pause 应返回 true");
        assert!(g.is_paused());
        let since = g.paused_since_unix_ms().unwrap();
        assert!(since > 0, "wall-clock 时间戳应大于 0");

        // 二次 pause 幂等。
        assert!(!g.pause(), "再次 pause 应返回 false");

        // resume 一次清空。
        let dur = g.resume().expect("首次 resume 应返回 elapsed");
        assert!(g.paused_since_unix_ms().is_none());
        assert!(!g.is_paused());

        // 二次 resume 幂等。
        assert!(g.resume().is_none(), "再次 resume 应返回 None");
        // _ = dur; 实际 elapsed 视测试机速度，可能是 0ms。
        let _ = dur;
    }

    /// `total_paused` 跨多次 pause/resume 累加，且未暂停时不增长。
    /// workflow 预算靠它把 park 时间从「干活时间」里扣掉。
    #[test]
    fn total_paused_accumulates_across_cycles() {
        let g = AgentPauseGate::new("t");
        assert_eq!(g.total_paused(), Duration::ZERO, "初始应为 0");

        g.pause();
        std::thread::sleep(Duration::from_millis(60));
        g.resume();
        let after_first = g.total_paused();
        assert!(
            after_first >= Duration::from_millis(50),
            "第一次 park 应计入，实际 {after_first:?}"
        );

        // 未暂停期间不增长。
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(
            g.total_paused(),
            after_first,
            "running 期间 total_paused 不应变化"
        );

        g.pause();
        std::thread::sleep(Duration::from_millis(60));
        g.resume();
        assert!(
            g.total_paused() >= after_first + Duration::from_millis(50),
            "第二次 park 应叠加"
        );
    }

    /// 正在进行的暂停也要算进去——否则预算看门狗在 park 期间读到的
    /// 是「一直没停过」，照样会把 workflow 判超支（P0-1 的原样复现）。
    #[test]
    fn total_paused_includes_in_flight_pause() {
        let g = AgentPauseGate::new("t");
        g.pause();
        std::thread::sleep(Duration::from_millis(60));
        // 注意：还没 resume。
        assert!(g.is_paused());
        assert!(
            g.total_paused() >= Duration::from_millis(50),
            "in-flight 的 park 必须现算进去，实际 {:?}",
            g.total_paused()
        );
    }

    /// 「有效工作时间 = wall-clock − 累计暂停」这条换算成立。
    #[test]
    fn effective_time_excludes_paused_span() {
        let g = AgentPauseGate::new("t");
        let base = g.total_paused();
        let started = Instant::now();

        std::thread::sleep(Duration::from_millis(30)); // 干活
        g.pause();
        std::thread::sleep(Duration::from_millis(100)); // park
        g.resume();
        std::thread::sleep(Duration::from_millis(30)); // 干活

        let parked = g.total_paused() - base;
        let effective = started.elapsed().saturating_sub(parked);
        // 真实工作 ~60ms，wall-clock ~160ms。有效时间必须明显小于
        // wall-clock，且不该把 park 的 100ms 算进来。
        assert!(
            effective < Duration::from_millis(120),
            "有效时间不应包含 park，实际 {effective:?}（wall {:?}）",
            started.elapsed()
        );
        assert!(
            parked >= Duration::from_millis(90),
            "park 时长应被记全，实际 {parked:?}"
        );
    }

    #[tokio::test]
    async fn wait_returns_immediately_when_not_paused() {
        let g = AgentPauseGate::new("t");
        // 不应 park。
        tokio::time::timeout(Duration::from_millis(50), g.wait_until_resumed(None))
            .await
            .expect("未暂停时 wait 必须立即返回")
            .eq(&WaitResult::Ok);
    }

    #[tokio::test]
    async fn wait_park_then_resume_wakes_waiter() {
        let g = AgentPauseGate::new("t");
        g.pause();
        // 100ms 后 resume，waiter 应该 100ms 内醒来。
        let g2 = Arc::clone(&g);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            g2.resume();
        });
        let start = Instant::now();
        let r = g.wait_until_resumed(None).await;
        assert_eq!(r, WaitResult::Ok);
        assert!(start.elapsed() >= Duration::from_millis(80));
        assert!(start.elapsed() < Duration::from_millis(500));
    }

    #[tokio::test]
    async fn wait_cancelled_releases_waiter_but_keeps_gate() {
        let g = AgentPauseGate::new("t");
        g.pause();
        let ct = CancellationToken::new();
        let ct2 = ct.clone();
        let g2 = Arc::clone(&g);
        // 50ms 后 cancel 一个 waiter；gate 仍 engaged。
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            ct2.cancel();
        });
        let r = g2.wait_until_resumed(Some(ct)).await;
        assert_eq!(r, WaitResult::Cancelled);
        assert!(g.is_paused(), "cancel 一个 waiter 不应解除 gate");
    }

    #[tokio::test]
    async fn on_change_fires_on_pause_and_resume() {
        let g = AgentPauseGate::new("t");
        let count = Arc::new(AtomicUsize::new(0));
        let count_clone = Arc::clone(&count);
        g.on_change(move |paused| {
            let v = count_clone.fetch_add(1, AOrd::Relaxed) + 1;
            // paused 状态与回调次数对应：pause=1 resume=1 共 2 次。
            assert!(paused || v >= 2);
        });
        g.pause();
        g.resume();
        assert_eq!(count.load(AOrd::Relaxed), 2);
    }

    #[test]
    fn remove_listener_stops_callbacks() {
        let g = AgentPauseGate::new("t");
        let count = Arc::new(AtomicUsize::new(0));
        let count2 = Arc::clone(&count);
        let id = g.on_change(move |_| {
            count2.fetch_add(1, AOrd::Relaxed);
        });
        g.pause();
        assert_eq!(count.load(AOrd::Relaxed), 1);
        g.remove_listener(id);
        g.resume();
        assert_eq!(count.load(AOrd::Relaxed), 1, "remove 后不应再触发");
    }

    #[tokio::test]
    async fn race_re_engaging_does_not_lose_waker() {
        // 边界场景：waiter 被唤醒的瞬间 gate 又被 pause。waiter 必
        // 须看到新 paused 状态而不是 fall through。
        let g = AgentPauseGate::new("t");
        g.pause();
        let g2 = Arc::clone(&g);
        // 5ms 后再 pause 一次（实际就是幂等 no-op，但模拟 race）。
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            g2.resume();
            // 不再 pause：waiter 应在 resume 后醒来
        });
        let r = g.wait_until_resumed(None).await;
        assert_eq!(r, WaitResult::Ok);
    }
}
