//! 按 subsession（`sub_id`）粒度的取消登记表。
//!
//! 三层取消语义，粒度由粗到细：
//!   1. session `cancel_flag` —— abort 整个 session，所有 in-flight
//!      分派一起停（`DELETE /api/sessions`、`POST /api/chat/abort`）。
//!   2. per-turn `turn_cancel_flag` —— 掐掉当前 turn，session 保留
//!      （`POST /api/chat/cancel-turn` 不带 sub_id）。
//!   3. **本模块**：per-subsession —— 只掐掉某一条分派，同一 wave 里
//!      的其它并行分派继续跑（`POST /api/chat/cancel-turn` 带 sub_id，
//!      UI 右键 subsession 消息 →「终止此分派」）。
//!
//! 为什么需要第 3 层：workflow step 已无 wall-clock 硬超时（超时只发
//! `TimeoutWarning` 让用户拍板），而 DAG 的并行波会同时跑多条分派
//! （如 design_and_plan 的 req_review ‖ code_review）。只有 per-turn
//! 粒度时，用户想掐掉卡住的那一条就只能连带干掉整轮——另一条已经跑
//! 了十几分钟的分派也跟着白费。
//!
//! 生命周期：分派开始时 [`register`]，结束时由 [`SubCancelGuard`] 的
//! Drop 自动摘除（正常返回 / 失败 / panic / 被 abort 都覆盖），因此
//! [`is_active`] 可当作"这条分派还在跑吗"的判据。进程级全局表即可：
//! `sub_id` 由 `SubsessionStore::create` 保证唯一。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

/// `sub_id` → 该分派的取消旗标。
static PENDING: LazyLock<Mutex<HashMap<String, Arc<AtomicBool>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 登记一条正在跑的分派，返回它的取消旗标。
///
/// 调用方应把返回的旗标塞进自己的 500ms 轮询分支，并**同时**持有一个
/// [`SubCancelGuard`] 保证退出时摘除登记。同一 `sub_id` 重复登记会顶掉
/// 旧旗标（正常不会发生——`sub_id` 唯一）。
pub fn register(sub_id: &str) -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    PENDING
        .lock()
        .unwrap()
        .insert(sub_id.to_string(), flag.clone());
    flag
}

/// 请求取消某条分派。
///
/// 返回 `false` 表示没有这条正在跑的分派（已结束 / 未知 sub_id）——
/// 调用方据此告诉用户"该分派已结束"，而不是假装取消成功。
pub fn cancel(sub_id: &str) -> bool {
    match PENDING.lock().unwrap().get(sub_id) {
        Some(flag) => {
            flag.store(true, Ordering::SeqCst);
            true
        }
        None => false,
    }
}

/// 摘除登记。分派结束时调用；[`SubCancelGuard`] 已自动处理，一般不用
/// 手工调。幂等。
pub fn unregister(sub_id: &str) {
    PENDING.lock().unwrap().remove(sub_id);
}

/// 该分派是否仍在登记表里（即仍在跑）。供 UI/API 判断"还能不能取消"。
pub fn is_active(sub_id: &str) -> bool {
    PENDING.lock().unwrap().contains_key(sub_id)
}

/// 当前在跑的分派数。仅用于测试与诊断。
pub fn active_count() -> usize {
    PENDING.lock().unwrap().len()
}

/// Drop guard：分派的 future 结束（正常返回 / 出错 / 被 abort）时自动
/// 摘除登记，避免 PENDING 泄漏成"永远显示可取消"的僵尸项。
pub struct SubCancelGuard(pub String);

impl Drop for SubCancelGuard {
    fn drop(&mut self) {
        unregister(&self.0);
    }
}

/// spawn 出去的任务在守卫被 drop 时一起 abort。
///
/// 存在理由：`tokio::task::JoinHandle` 被 drop 时任务**继续跑**。凡是
/// 「spawn 出去、又在 `select!` 里等它、而这个 select 本身可能被外层
/// drop」的地方都必须挂一个 —— 外层一 drop，spawn 的任务就走不到任何
/// abort 分支，脱管跑到底（还在写文件、还在往共享事件流发事件），而
/// [`SubCancelGuard`] 已经析构，用户连"终止此分派"都点不到了。
///
/// 两处调用点各自踩过这个坑：workflow DAG 引擎的并行分派，以及
/// controller 的 `delegate` 工具（驱动侧 `run_turn_cancellable` 的
/// 500ms 轮询先赢时会直接 drop 整个 run_turn future）。任务已结束时
/// abort 是 no-op，正常路径无副作用。
pub struct AbortOnDrop(pub tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_then_cancel_sets_flag() {
        let flag = register("sub-cancel-t1");
        assert!(!flag.load(Ordering::SeqCst), "刚登记时不应已取消");
        assert!(cancel("sub-cancel-t1"), "登记过的分派应能取消");
        assert!(flag.load(Ordering::SeqCst), "取消后旗标必须置位");
        unregister("sub-cancel-t1");
    }

    /// 取消未知/已结束的 sub_id 必须返回 false —— API 据此回 404，
    /// UI 才能提示「该分派已结束」而不是假装成功。
    #[test]
    fn cancel_unknown_sub_id_returns_false() {
        assert!(!cancel("sub-cancel-nope"));
    }

    /// guard drop 后登记被摘除：否则 `is_active` 永远为真，UI 会对
    /// 早已结束的分派继续显示「终止此分派」。
    #[test]
    fn guard_drop_unregisters() {
        {
            let _flag = register("sub-cancel-t2");
            let _g = SubCancelGuard("sub-cancel-t2".to_string());
            assert!(is_active("sub-cancel-t2"));
        }
        assert!(!is_active("sub-cancel-t2"), "guard drop 后应已摘除");
        assert!(!cancel("sub-cancel-t2"), "摘除后不可再取消");
    }

    /// 取消一条不影响另一条 —— 这正是本模块存在的理由（并行波里
    /// 只掐卡住的那条，其余继续）。
    #[test]
    fn cancelling_one_leaves_siblings_running() {
        let a = register("sub-cancel-a");
        let b = register("sub-cancel-b");
        assert!(cancel("sub-cancel-a"));
        assert!(a.load(Ordering::SeqCst), "被取消的那条置位");
        assert!(
            !b.load(Ordering::SeqCst),
            "兄弟分派必须不受影响，否则退化成 per-turn 取消"
        );
        unregister("sub-cancel-a");
        unregister("sub-cancel-b");
    }
}
