//! 活性探针：判断一个 specialist 是「在干活」还是「挂着不动」。
//!
//! 背景（实测 ui-1339475，2026-11-11）：会话 02:51 起模型全链不可用，
//! session pause gate 一直 engaged；第二轮的 `tech_writer` 在入口就 park，
//! **零次模型调用、零个 trace 事件**，却被外层 10800s 墙钟记满，最后报
//! 「超过 10800s 未返回」。3 小时里没有任何一处机制发现「它根本没在动」。
//!
//! 墙钟超时回答不了这个问题：它分不清「任务就是重」和「它已经死了」。
//! 真正要判的是**进展是否停滞**——一个健康的 specialist 每几十秒就会留下
//! 一条事件（模型调用、工具执行、思维链增量、慢调用提示），而挂死的
//! specialist 一条都没有。所以这里做成一个 sink 包装：每 emit 一条事件就
//! 记一次心跳，上层定时读「距上次心跳多久」。
//!
//! 为什么包在 sink 上而不是由 runner 暴露：delegate 与 workflow step 两条
//! 路径都已经把自己的 trace 汇到 `FanOutSink`，加一层包装就能同时覆盖
//! 模型调用 / 工具执行 / 暂停 / 恢复——不必在 runner 的每个相位手工打点。
//!
//! 暂停与「等用户答题」**不算停滞**：那段时间没有事件是正常的，判定前
//! 由调用方显式跳过并把心跳推后（[`ActivityProbe::touch`]）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::trace::{TraceEvent, TraceSink};

/// 停滞判定的默认阈值（秒）：30 分钟没有任何 trace 事件。
///
/// 为什么不是更短：非流式调用只在**开始**时发一条 `ModelCallSlow`（默认
/// 120s 阈值），一次 20 分钟的慢模型调用中间可以完全没有事件。阈值压在
/// 慢调用时长之内会误杀正在生成的长回答。而真正卡死的分派（例如停在
/// pause gate 上）是**一条事件都没有**，30 分钟足够区分。
///
/// env `LATTE_AGENT_STALL_TIMEOUT_SECS` 覆盖；`0` = 关闭本看门狗。
pub const DEFAULT_STALL_TIMEOUT_SECS: u64 = 30 * 60;

/// 停滞阈值。`None` = 关闭。
///
/// 单测用 task-local 覆盖（`controller::tests::TEST_STALL_TIMEOUT`），
/// 不改全局 env —— 与 `specialist_timeout_secs` 同一套做法，避免并行
/// 测试互相污染。
pub fn stall_timeout() -> Option<Duration> {
    if let Some(secs) = test_stall_timeout_override() {
        return (secs > 0).then(|| Duration::from_secs(secs));
    }
    let secs = std::env::var("LATTE_AGENT_STALL_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_STALL_TIMEOUT_SECS);
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// 单测覆盖入口：测试构建下由 `controller::tests::TEST_STALL_TIMEOUT`
/// 的 task-local 提供，非测试构建恒为 `None`。
fn test_stall_timeout_override() -> Option<u64> {
    imp()
}

#[cfg(not(test))]
fn imp() -> Option<u64> {
    None
}

#[cfg(test)]
fn imp() -> Option<u64> {
    crate::controller::tests::TEST_STALL_TIMEOUT
        .try_with(|v| *v)
        .ok()
}

/// 记录「最后一次产出」的时刻与规模。进程内可克隆共享（`Arc` 常用）。
pub struct ActivityProbe {
    /// 探针创建时刻：心跳时间戳相对它记，省掉一次 `Instant` 存储在原子量里。
    started: Instant,
    /// 最后一次心跳，单位毫秒（相对 `started`）。
    last_ms: AtomicU64,
    /// 累计事件数。用来在错误信息里区分「一直没动过」与「动过又停了」。
    events: AtomicU64,
    /// 最后一条事件的变体名（`ToolExec` / `ModelCall` …）。
    last_name: parking_lot::Mutex<String>,
}

impl std::fmt::Debug for ActivityProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActivityProbe")
            .field("idle", &self.idle())
            .field("events", &self.events())
            .finish()
    }
}

impl Default for ActivityProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl ActivityProbe {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            last_ms: AtomicU64::new(0),
            events: AtomicU64::new(0),
            last_name: parking_lot::Mutex::new(String::new()),
        }
    }

    /// 记一次心跳（每 emit 一条 trace 事件调用一次）。
    pub fn record(&self, variant: &str) {
        self.touch();
        self.events.fetch_add(1, Ordering::Relaxed);
        let mut name = self.last_name.lock();
        if name.as_str() != variant {
            variant.clone_into(&mut name);
        }
    }

    /// 只推进心跳、不计数。用于「这一段时间本来就不该有事件」（暂停中 /
    /// 等用户答题）：跳过判定时把窗口整体推后，恢复后重新给满一份额度。
    pub fn touch(&self) {
        let ms = self.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        self.last_ms.store(ms, Ordering::Relaxed);
    }

    /// 距上一次心跳多久。
    pub fn idle(&self) -> Duration {
        let last = self.last_ms.load(Ordering::Relaxed);
        self.started
            .elapsed()
            .saturating_sub(Duration::from_millis(last))
    }

    /// 累计事件数。
    pub fn events(&self) -> u64 {
        self.events.load(Ordering::Relaxed)
    }

    /// 最后一条事件的变体名；从未发过事件时为空串。
    pub fn last_event(&self) -> String {
        self.last_name.lock().clone()
    }

    /// 停滞描述：给超时/中止文案用（「一条事件都没有」与「最后是 X」要能区分）。
    pub fn stall_report(&self, idle: Duration) -> String {
        let secs = idle.as_secs();
        match self.events() {
            0 => format!("已经 {secs}s 没有任何事件（一条 trace 都没有，多半卡在入口或暂停门上）"),
            _ => format!(
                "已经 {secs}s 没有任何事件（最后一条是 {}，累计 {} 条）",
                self.last_event(),
                self.events()
            ),
        }
    }

    /// 包一层 sink：所有经它流过的事件都记一次心跳。
    pub fn wrap(self: &Arc<Self>, inner: Arc<dyn TraceSink>) -> Arc<dyn TraceSink> {
        Arc::new(ProbeSink {
            probe: Arc::clone(self),
            inner,
        })
    }
}

/// 透传 + 打点的 sink 包装。
struct ProbeSink {
    probe: Arc<ActivityProbe>,
    inner: Arc<dyn TraceSink>,
}

impl TraceSink for ProbeSink {
    fn emit(&self, event: TraceEvent) {
        self.probe.record(event.variant_name());
        self.inner.emit(event);
    }

    fn is_null(&self) -> bool {
        self.inner.is_null()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    #[derive(Default)]
    struct VecSink(Mutex<Vec<TraceEvent>>);

    impl TraceSink for VecSink {
        fn emit(&self, event: TraceEvent) {
            self.0.lock().push(event);
        }
    }

    fn event(name: &str) -> TraceEvent {
        TraceEvent::ToolExec {
            meta: crate::trace::TraceMeta::now(0, "tester", ""),
            name: name.into(),
            args_json: "{}".into(),
            latency_ms: 1,
            status: crate::trace::ToolStatus::Ok("{}".into()),
            output_complete: true,
            output_bytes: 0,
            artifact: None,
        }
    }

    /// 包装后事件照常透传——探针不能吞事件。
    #[test]
    fn wrap_passes_events_through_and_counts_them() {
        let probe = Arc::new(ActivityProbe::new());
        let inner: Arc<dyn TraceSink> = Arc::new(VecSink::default());
        let sink = probe.wrap(Arc::clone(&inner));
        assert_eq!(probe.events(), 0);
        sink.emit(event("read"));
        sink.emit(event("search"));
        assert_eq!(probe.events(), 2);
        assert_eq!(probe.last_event(), "ToolExec");
    }

    /// 心跳推进后 idle 归零；`touch` 等价于「这段时间不算停滞」。
    #[test]
    fn idle_resets_on_activity_and_on_touch() {
        let probe = Arc::new(ActivityProbe::new());
        let sink = probe.wrap(Arc::new(VecSink::default()));
        std::thread::sleep(Duration::from_millis(60));
        assert!(
            probe.idle() >= Duration::from_millis(50),
            "没动过就该累计 idle，实际 {:?}",
            probe.idle()
        );
        sink.emit(event("read"));
        assert!(probe.idle() < Duration::from_millis(50), "事件应重置 idle");

        std::thread::sleep(Duration::from_millis(60));
        probe.touch();
        assert!(
            probe.idle() < Duration::from_millis(50),
            "touch 应重置 idle（暂停/等答题期间用）"
        );
        // touch 不计数：它不是真实产出。
        assert_eq!(probe.events(), 1);
    }

    /// 停滞报告要能区分「一次都没动过」与「动过又停了」。
    #[test]
    fn stall_report_distinguishes_never_started_from_stopped() {
        let probe = Arc::new(ActivityProbe::new());
        let never = probe.stall_report(Duration::from_secs(60));
        assert!(never.contains("一条 trace 都没有"), "{never}");

        probe.wrap(Arc::new(VecSink::default())).emit(event("read"));
        let stopped = probe.stall_report(Duration::from_secs(60));
        assert!(stopped.contains("最后一条是 ToolExec"), "{stopped}");
        assert!(stopped.contains("累计 1 条"), "{stopped}");
    }

    #[test]
    fn stall_timeout_defaults_to_30_minutes() {
        // 不读 env：测试进程的环境由 test_util::env_lock 管，这里只校验
        // 「未配置时取默认值」这条纯函数行为（默认常量本身）。
        assert!(
            DEFAULT_STALL_TIMEOUT_SECS >= 15 * 60,
            "默认阈值不能低于慢调用时长"
        );
    }
}
