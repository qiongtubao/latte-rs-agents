//! 派发幂等台账：同一次派发不做第二遍。
//!
//! 问题：`delegate` / `workflow` 这两个派发工具此前**没有任何**重复
//! 抑制。现有的三道闸没有一道管这件事：
//!
//! - [`crate::agent`] 的 `dedupe_native_tool_calls` 只在**同一条模型
//!   响应内**去掉逐字相同的 tool_call；跨轮次、或任务描述差一个标点，
//!   两次都会真跑。
//! - `LoopDetector` 每个 tool round 重建（`streak` 不跨轮累计），阈值
//!   是「同一轮内连续 3 次相同调用」——native function-calling 下几乎
//!   不可能触发。
//! - `Supervisor` 的 dead-loop 看的是 `last_decision_kind()`，而 delegate
//!   在那里被压成**不带参数**的字符串 `"delegate"`。于是它既误伤
//!   （连续 3 轮各派不同专家 = 合法分工，照样判死循环暂停）又漏放
//!   （同一轮里派两次一模一样的活，它压根看不见——它是 per-round 粒度）。
//!
//! 代价是实打实的：一次 specialist 委派动辄几十秒到十几分钟，还会重复
//! 产生副作用（重复写文件、重复跑构建、重复提交）。
//!
//! 本模块提供一个**按派发身份**（delegate: role + task；workflow:
//! name + topic + resume）的台账：
//!
//! - **正在跑**的同身份派发 → 立即拒绝，且在分配 subsession /
//!   发 `DelegateStarted` **之前**拒（对齐 `plan_gate_rejection` 的
//!   「拒绝不留副作用」约定，否则 UI 上会留下永远转圈的分派气泡）。
//! - **已成功**的同身份派发 → 直接返回上次的结果，不再跑第二遍。
//! - **失败**的派发不进缓存：失败可能是瞬时的（模型不可用），换个
//!   时机重试是合理的；重复失败由 `max_delegates` 计数与 supervisor 兜。
//!
//! 生命周期与注册该工具的 runner/session 一致（跟 `delegate_counter`
//! 一个量级），不是进程级——不同 session 的同名任务互不影响。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// 一条派发身份的状态。
#[derive(Debug, Clone)]
enum Entry {
    /// 正在执行中。
    InFlight,
    /// 已成功完成，带上次的产出。
    Done(String),
}

/// `begin` 的三种结果。
#[derive(Debug)]
pub enum Begin {
    /// 首次派发：拿着 guard 去跑，跑完调 [`InFlightGuard::complete`]。
    Fresh(InFlightGuard),
    /// 同身份派发正在执行中 —— 调用方应立即返回错误，不要产生副作用。
    InFlight,
    /// 同身份派发此前已成功 —— 调用方应直接返回这个结果。
    Done(String),
}

/// 在飞标记的 RAII 句柄。
///
/// Drop 时若没有 `complete`（早返回、失败、panic、被 abort），自动把
/// 在飞标记摘掉 —— 否则一次失败会把这个身份**永久**锁死，之后合法的
/// 重试全被当成「正在跑」拒掉。
pub struct InFlightGuard {
    ledger: Arc<Inner>,
    key: String,
    completed: bool,
}

impl std::fmt::Debug for InFlightGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InFlightGuard")
            .field("key", &self.key)
            .field("completed", &self.completed)
            .finish()
    }
}

impl InFlightGuard {
    /// 派发成功：把结果写进台账，后续同身份派发直接复用。
    pub fn complete(mut self, result: String) {
        self.completed = true;
        self.ledger
            .map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(self.key.clone(), Entry::Done(result));
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        self.ledger
            .map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.key);
    }
}

#[derive(Default)]
struct Inner {
    map: Mutex<HashMap<String, Entry>>,
}

/// 派发幂等台账。克隆共享同一份状态（内部 `Arc`）。
#[derive(Clone, Default)]
pub struct DispatchLedger {
    inner: Arc<Inner>,
}

impl DispatchLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// 组一个 delegate 的派发身份键。task 去首尾空白后参与——模型偶尔
    /// 多带一个换行，那不是「不同的任务」。
    pub fn delegate_key(role: &str, task: &str) -> String {
        // 长度前缀分帧，而不是靠一个「正常内容里不会出现」的分隔符：
        // 只要分隔符可能出现在载荷里，("a", "b<SEP>c") 就会和
        // ("a<SEP>b", "c") 撞成同一个键。分帧后无歧义。
        let role_len = role.len();
        format!("delegate|{role_len}|{role}|{}", task.trim())
    }

    /// 组一个 workflow 的派发身份键。`resume` 参与键：续跑与新开是
    /// 两件不同的事，不能互相命中。
    pub fn workflow_key(name: &str, topic: &str, resume: Option<&str>) -> String {
        let resume = resume.unwrap_or("");
        format!(
            "workflow|{}|{name}|{}|{resume}|{}",
            name.len(),
            resume.len(),
            topic.trim()
        )
    }

    /// 登记一次派发。见 [`Begin`]。
    pub fn begin(&self, key: String) -> Begin {
        let mut map = self.inner.map.lock().unwrap_or_else(|e| e.into_inner());
        match map.get(&key) {
            Some(Entry::InFlight) => Begin::InFlight,
            Some(Entry::Done(result)) => Begin::Done(result.clone()),
            None => {
                map.insert(key.clone(), Entry::InFlight);
                Begin::Fresh(InFlightGuard {
                    ledger: Arc::clone(&self.inner),
                    key,
                    completed: false,
                })
            }
        }
    }

    /// 只做「互斥」不做「结果缓存」的登记：拿到 `Some(guard)` 说明这个
    /// 身份当前没人在跑；`None` = 正在跑，调用方应拒绝。
    ///
    /// 给 `workflow` 用：同一 topic 再跑一遍常常是有意的（改了配置、
    /// 想要新一版方案），而且 workflow 有真实副作用，复用旧摘要会掩盖
    /// 「这次其实什么都没跑」。所以它只需要防并发重入，不需要缓存。
    /// guard 一 drop（成功或失败）身份即释放。
    pub fn begin_exclusive(&self, key: String) -> Option<InFlightGuard> {
        let mut map = self.inner.map.lock().unwrap_or_else(|e| e.into_inner());
        if map.contains_key(&key) {
            return None;
        }
        map.insert(key.clone(), Entry::InFlight);
        Some(InFlightGuard {
            ledger: Arc::clone(&self.inner),
            key,
            completed: false,
        })
    }

    /// 台账里的条目数（测试与诊断用）。
    pub fn len(&self) -> usize {
        self.inner
            .map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_dispatch_is_fresh_then_cached_after_complete() {
        let ledger = DispatchLedger::new();
        let key = DispatchLedger::delegate_key("programmer", "改 stats.c");

        let Begin::Fresh(guard) = ledger.begin(key.clone()) else {
            panic!("首次派发应是 Fresh");
        };
        guard.complete("已改完".into());

        match ledger.begin(key) {
            Begin::Done(r) => assert_eq!(r, "已改完", "同身份派发应复用上次结果"),
            other => panic!("应命中缓存，got {other:?}"),
        }
    }

    #[test]
    fn concurrent_same_dispatch_is_rejected_while_in_flight() {
        let ledger = DispatchLedger::new();
        let key = DispatchLedger::delegate_key("programmer", "跑构建");

        let Begin::Fresh(_guard) = ledger.begin(key.clone()) else {
            panic!("首次应 Fresh");
        };
        // guard 还活着 = 还在跑。
        assert!(matches!(ledger.begin(key), Begin::InFlight));
    }

    /// guard 未 complete 就 drop（失败/早返回/被 abort）必须摘掉在飞
    /// 标记。否则一次失败把这个身份永久锁死，合法重试全被拒。
    #[test]
    fn dropped_guard_without_complete_frees_the_key() {
        let ledger = DispatchLedger::new();
        let key = DispatchLedger::delegate_key("tester", "跑测试");
        {
            let Begin::Fresh(_guard) = ledger.begin(key.clone()) else {
                panic!("Fresh");
            };
            // 模拟失败路径：不调 complete 直接走。
        }
        assert!(ledger.is_empty(), "失败后不该留下在飞标记");
        assert!(
            matches!(ledger.begin(key), Begin::Fresh(_)),
            "失败后同身份必须还能重试"
        );
    }

    #[test]
    fn keys_distinguish_role_task_and_are_whitespace_insensitive() {
        // task 去首尾空白：多一个换行不算新任务。
        assert_eq!(
            DispatchLedger::delegate_key("pm", "写需求"),
            DispatchLedger::delegate_key("pm", "  写需求\n")
        );
        // role 不同 → 不同键。
        assert_ne!(
            DispatchLedger::delegate_key("pm", "写需求"),
            DispatchLedger::delegate_key("architect", "写需求")
        );
        // 分帧防撞键：载荷里出现分隔符也不能让两组不同的
        // (role, task) 压成同一个键。
        assert_ne!(
            DispatchLedger::delegate_key("a", "b|c"),
            DispatchLedger::delegate_key("a|b", "c")
        );
        assert_ne!(
            DispatchLedger::delegate_key("a", "1|a|b"),
            DispatchLedger::delegate_key("a|1|a", "b")
        );
    }

    #[test]
    fn workflow_key_separates_resume_from_fresh_run() {
        let fresh = DispatchLedger::workflow_key("design_and_plan", "学某个 C 项目", None);
        let resumed =
            DispatchLedger::workflow_key("design_and_plan", "学某个 C 项目", Some("wf-x-1"));
        assert_ne!(fresh, resumed, "resume 与新开是两件事，不能互相命中");
        // 不同 topic 不互撞。
        assert_ne!(
            DispatchLedger::workflow_key("w", "a", None),
            DispatchLedger::workflow_key("w", "b", None)
        );
    }

    /// 不同身份互不影响。
    #[test]
    fn different_identities_are_independent() {
        let ledger = DispatchLedger::new();
        let a = DispatchLedger::delegate_key("programmer", "任务A");
        let b = DispatchLedger::delegate_key("programmer", "任务B");
        let Begin::Fresh(ga) = ledger.begin(a.clone()) else {
            panic!()
        };
        let Begin::Fresh(_gb) = ledger.begin(b) else {
            panic!("不同任务不该被 A 挡住")
        };
        ga.complete("A done".into());
        assert!(matches!(ledger.begin(a), Begin::Done(_)));
    }
}
