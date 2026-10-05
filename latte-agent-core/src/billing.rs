//! 模型账户**余额不足 / 配额耗尽**的进程级登记。
//!
//! 问题：余额不足（402、`insufficient_quota`、「余额不足」）在本次会话内不会
//! 自愈——充值是人的动作。此前它被当成普通的模型局部失败处理：
//! - 冷却挂在 `ModelClient` 上，而每次 delegate / workflow step / advisor 审查
//!   都新建 `Agent`，冷却跟着丢失，每次分派都要白打一次 402；
//! - 全链失败时它和 429 混在一起进入「自动暂停 → 退避重试 5 次 → 等人工」，
//!   重试对它毫无意义，用户看到的只是「模型不可用」，不知道是该充值。
//!   （实测 jemalloc LAT-110：deepseek-v4-flash / v4-pro 402 出现在每一次
//!   全链失败里，session 退避 5 次后挂起等人工 45 分钟。）
//!
//! 现在：按**账户**（provider + base_url + api_key 指纹）登记——同一个 key 下
//! 的模型（如 deepseek-v4-flash 与 v4-pro）一个 402 一起摘除。登记后：
//! - 所有 Agent 的模型链都跳过它（不再发请求），失败清单里写明「余额不足」；
//! - 计算自动重试时间时不算它：剩下的全是这类失败时直接转人工，不退避重试；
//! - 首次登记时广播一条通知（[`subscribe`]），宿主转成用户可见的状态消息；
//! - 过 [`recheck_secs`]（默认 30 分钟）后放行一次探测；用户手动「继续」时
//!   [`clear_all`] 立即全部放行（多半是刚充完值）。

use std::collections::{BTreeSet, HashMap};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use latte_ai::error::AiError;
use latte_ai::models::Model;
use parking_lot::Mutex;
use tokio::sync::broadcast;

/// 一个被摘除的账户。
#[derive(Debug, Clone)]
struct Entry {
    until: Instant,
    provider: String,
    models: BTreeSet<String>,
    message: String,
}

/// 首次登记某账户时广播的通知。
#[derive(Debug, Clone)]
pub struct BillingNotice {
    pub provider: String,
    pub models: Vec<String>,
    pub message: String,
    /// 正在用这些模型的角色（宿主注册了 [`set_role_lookup`] 时才有）。
    pub roles: Vec<String>,
}

/// 被摘除账户的快照（`GET /api/billing` 用）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct BlockedAccount {
    pub provider: String,
    pub models: Vec<String>,
    pub message: String,
    /// 距离自动重新探测还有多少秒。
    pub recheck_in_secs: u64,
    /// `角色 → 它链上被摘除的模型`。
    pub roles: Vec<RoleHit>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RoleHit {
    pub role: String,
    pub models: Vec<String>,
    /// 这个角色的链上**全部**模型都被摘除了（这个角色此刻完全跑不了）。
    pub all_blocked: bool,
}

type RoleLookup = std::sync::Arc<
    dyn Fn(&BTreeSet<String>) -> Vec<(String, Vec<String>, bool)> + Send + Sync,
>;

fn role_lookup() -> &'static Mutex<Option<RoleLookup>> {
    static L: OnceLock<Mutex<Option<RoleLookup>>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(None))
}

/// 宿主注册「模型 → 使用它的角色」的查询（用自己的 ModelResolver）。返回
/// `(角色, 命中的模型, 该角色链是否已全部被摘除)`。
pub fn set_role_lookup(
    f: impl Fn(&BTreeSet<String>) -> Vec<(String, Vec<String>, bool)> + Send + Sync + 'static,
) {
    *role_lookup().lock() = Some(std::sync::Arc::new(f));
}

/// 宿主启动时调一次：登记模型目录（同账户兄弟模型）+ 角色查询。
pub fn register_host(
    resolver: std::sync::Arc<crate::model_resolver::ModelResolver>,
    model_ids: impl IntoIterator<Item = String>,
) {
    register_catalog(model_ids.into_iter().filter_map(|id| resolver.build_model(&id).ok()));
    register_resolver(resolver);
}

/// 用 resolver 注册角色查询：命中 = 角色实际模型链里被摘除的模型。
pub fn register_resolver(resolver: std::sync::Arc<crate::model_resolver::ModelResolver>) {
    set_role_lookup(move |models| {
        resolver
            .roles_using_models(models)
            .into_iter()
            .map(|(role, hit)| {
                let all = role_chain_all_blocked(&resolver, &role);
                (role, hit, all)
            })
            .collect()
    });
}

/// 角色实际模型链上的每个模型所属账户都被摘除了。
fn role_chain_all_blocked(resolver: &crate::model_resolver::ModelResolver, role: &str) -> bool {
    let (chain, tier) = resolver.role_model_assignment(role).unwrap_or_default();
    let tier = tier.unwrap_or(crate::model_resolver::ModelTier::Standard);
    match resolver.resolve_chain(role, tier, &chain) {
        Ok(models) if !models.is_empty() => models.iter().all(|m| blocked(m).is_some()),
        _ => false,
    }
}

fn lookup_roles(models: &BTreeSet<String>) -> Vec<(String, Vec<String>, bool)> {
    let f = role_lookup().lock().clone();
    f.map(|f| f(models)).unwrap_or_default()
}

impl BillingNotice {
    /// 给用户看的一行。
    pub fn render(&self) -> String {
        let roles = if self.roles.is_empty() {
            String::new()
        } else {
            format!("受影响角色：{}。", self.roles.join("、"))
        };
        format!(
            "💳 模型账户余额不足 / 配额耗尽：{}（{}），已从所有模型链中摘除，不再重试。{roles}\
             充值后在暂停提示处点继续（或 /resume）即可重新启用；{} 分钟后也会自动探测一次。原始报错：{}",
            self.provider,
            self.models.join("、"),
            recheck_secs() / 60,
            self.message
        )
    }
}

fn registry() -> &'static Mutex<HashMap<String, Entry>> {
    static R: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn channel() -> &'static broadcast::Sender<BillingNotice> {
    static C: OnceLock<broadcast::Sender<BillingNotice>> = OnceLock::new();
    C.get_or_init(|| broadcast::channel(16).0)
}

/// 订阅首次登记通知（宿主转成 `ChatEvent::Status`）。
pub fn subscribe() -> broadcast::Receiver<BillingNotice> {
    channel().subscribe()
}

/// 把余额通知转发成会话事件通道上的 `ChatEvent::Status`（需在 tokio 运行时内调用；
/// 不在运行时内时静默跳过）。通道关闭（会话结束）时转发任务自行退出。
pub fn forward_to(event_tx: broadcast::Sender<crate::controller::ChatEvent>) {
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let mut rx = subscribe();
    rt.spawn(async move {
        loop {
            match rx.recv().await {
                Ok(n) => {
                    if event_tx
                        .send(crate::controller::ChatEvent::Status { message: n.render() })
                        .is_err()
                        && event_tx.receiver_count() == 0
                    {
                        // 没有任何订阅者：会话已关，退出。
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
}

/// 摘除多久后放行一次探测：env `LATTE_AI_BILLING_RECHECK_SECS`（默认 1800，
/// 钳在 60..=86400）。
pub fn recheck_secs() -> u64 {
    std::env::var("LATTE_AI_BILLING_RECHECK_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|v| v.clamp(60, 86_400))
        .unwrap_or(1800)
}

/// 账户指纹：同一 provider + base_url + key 共享余额。key 只取哈希，不留原文。
fn account_key(m: &Model) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in m.api_key.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{}|{}|{h:016x}", m.provider, m.base_url.trim_end_matches('/'))
}

const BILLING_MARKERS: &[&str] = &[
    "insufficient balance",
    "insufficient_balance",
    "insufficient_quota",
    "insufficient quota",
    "exceeded your current quota",
    "quota exceeded",
    "billing",
    "payment required",
    "credit balance",
    "out of credits",
    "account balance",
    "arrears",
    "余额不足",
    "欠费",
    "额度不足",
    "额度已用完",
    "账户余额",
];

/// 这次失败是不是「账户没钱 / 配额用完」。402 一律算；其余 4xx / 429 / 鉴权
/// 错误看报文（OpenAI 的配额耗尽是 429 `insufficient_quota`，按限流重试只会
/// 每 60s 白打一次）。
pub fn is_billing_error(e: &AiError) -> bool {
    let has_marker = |msg: &str| {
        let m = msg.to_ascii_lowercase();
        BILLING_MARKERS.iter().any(|k| m.contains(k) || msg.contains(k))
    };
    match e {
        AiError::Api { status: 402, .. } => true,
        AiError::Api { status, message } if (400..500).contains(status) => has_marker(message),
        AiError::RateLimited { message, .. } => has_marker(message),
        AiError::Auth(message) => has_marker(message),
        _ => false,
    }
}

fn brief(msg: &str) -> String {
    let s: String = msg.chars().take(160).collect();
    if msg.chars().count() > 160 {
        format!("{s}…")
    } else {
        s
    }
}

/// 登记一次余额类失败。返回 `true` = 这个账户是新摘除的（已广播通知）。
pub fn mark(m: &Model, message: &str) -> bool {
    let key = account_key(m);
    let until = Instant::now() + Duration::from_secs(recheck_secs());
    let mut reg = registry().lock();
    let fresh = reg.get(&key).is_none_or(|e| Instant::now() >= e.until);
    let entry = reg.entry(key).or_insert_with(|| Entry {
        until,
        provider: m.provider.clone(),
        models: BTreeSet::new(),
        message: brief(message),
    });
    entry.until = until;
    entry.models.insert(m.id.clone());
    if fresh {
        entry.message = brief(message);
        let models = entry.models.clone();
        let (provider, message) = (entry.provider.clone(), entry.message.clone());
        drop(reg);
        // 同账户的其它模型（同 key 的 v4-flash / v4-pro）此刻还没撞上，按账户
        // 补进来：只要角色链里有同 provider+base_url+key 的模型都算受影响。
        let siblings = sibling_models(m);
        let all: BTreeSet<String> = models.iter().cloned().chain(siblings).collect();
        let roles = lookup_roles(&all)
            .into_iter()
            .map(|(r, hit, all_blocked)| {
                let tag = if all_blocked { "，整条链不可用" } else { "" };
                format!("{r}（{}{tag}）", hit.join("、"))
            })
            .collect();
        let notice = BillingNotice {
            provider,
            models: all.into_iter().collect(),
            message,
            roles,
        };
        tracing::warn!(provider = %notice.provider, models = ?notice.models, "model account out of balance; removed from chains");
        let _ = channel().send(notice);
    }
    fresh
}

fn sibling_registry() -> &'static Mutex<HashMap<String, BTreeSet<String>>> {
    static S: OnceLock<Mutex<HashMap<String, BTreeSet<String>>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 登记模型目录里的模型（宿主启动时调一次）：账户摘除时据此把同账户、
/// 还没撞上 402 的模型也算进去——它们一定会撞上。
pub fn register_catalog(models: impl IntoIterator<Item = Model>) {
    let mut sib = sibling_registry().lock();
    for m in models {
        sib.entry(account_key(&m)).or_default().insert(m.id.clone());
    }
}

fn sibling_models(m: &Model) -> BTreeSet<String> {
    sibling_registry()
        .lock()
        .get(&account_key(m))
        .cloned()
        .unwrap_or_default()
}

/// 当前被摘除的账户（含受影响角色），供 `GET /api/billing`。
pub fn blocked_accounts() -> Vec<BlockedAccount> {
    let now = Instant::now();
    let entries: Vec<(String, Entry)> = registry()
        .lock()
        .iter()
        .filter(|(_, e)| now < e.until)
        .map(|(k, e)| (k.clone(), e.clone()))
        .collect();
    entries
        .into_iter()
        .map(|(key, e)| {
            let mut models = e.models.clone();
            if let Some(sib) = sibling_registry().lock().get(&key) {
                models.extend(sib.iter().cloned());
            }
            let roles = lookup_roles(&models)
                .into_iter()
                .map(|(role, models, all_blocked)| RoleHit { role, models, all_blocked })
                .collect();
            BlockedAccount {
                provider: e.provider,
                models: models.into_iter().collect(),
                message: e.message,
                recheck_in_secs: e.until.saturating_duration_since(now).as_secs(),
                roles,
            }
        })
        .collect()
}

/// 该模型所属账户正被摘除时返回报错摘要。
pub fn blocked(m: &Model) -> Option<String> {
    let key = account_key(m);
    let mut reg = registry().lock();
    match reg.get_mut(&key) {
        Some(e) if Instant::now() < e.until => {
            e.models.insert(m.id.clone());
            Some(e.message.clone())
        }
        _ => None,
    }
}

/// 当前被摘除的账户（给暂停提示用）：`provider（模型…）`。
pub fn blocked_summary() -> Vec<String> {
    let now = Instant::now();
    registry()
        .lock()
        .values()
        .filter(|e| now < e.until)
        .map(|e| {
            format!(
                "{}（{}）",
                e.provider,
                e.models.iter().cloned().collect::<Vec<_>>().join("、")
            )
        })
        .collect()
}

/// 全部放行（用户手动继续：多半是刚充完值）。返回放行的账户数。
pub fn clear_all() -> usize {
    let mut reg = registry().lock();
    let n = reg.len();
    reg.clear();
    n
}

/// 失败清单里的「余额不足」项的固定前缀，供上层识别。
pub const BILLING_FAILURE_PREFIX: &str = "余额不足/配额耗尽（已摘除，不重试）";

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, provider: &str, key: &str) -> Model {
        let mut m: Model = serde_json::from_value(serde_json::json!({
            "id": id, "name": id, "api": "openai", "provider": provider,
            "base_url": "https://billing-test.invalid/v1", "api_key": key,
            "context_window": 1000, "max_tokens": 100,
            "supports_thinking": false, "supports_vision": false,
            "cost_per_million_input": 0.0, "cost_per_million_output": 0.0
        }))
        .unwrap();
        m.api_key = key.into();
        m
    }

    #[test]
    fn billing_errors_are_recognized_by_status_and_message() {
        assert!(is_billing_error(&AiError::Api { status: 402, message: "x".into() }));
        assert!(is_billing_error(&AiError::RateLimited {
            retry_after: 20.0,
            message: "You exceeded your current quota (insufficient_quota)".into()
        }));
        assert!(is_billing_error(&AiError::Api { status: 403, message: "账户余额不足".into() }));
        assert!(!is_billing_error(&AiError::RateLimited {
            retry_after: 1.0,
            message: "请求过于频繁，请稍后重试".into()
        }));
        assert!(!is_billing_error(&AiError::Api { status: 400, message: "invalid params".into() }));
        assert!(!is_billing_error(&AiError::Api { status: 500, message: "billing service down".into() }));
    }

    #[test]
    fn mark_blocks_whole_account_and_notifies_once() {
        let mut rx = subscribe();
        let flash = model("ds-flash", "deepseek", "k1");
        let pro = model("ds-pro", "deepseek", "k1");
        let other = model("ds-other-key", "deepseek", "k2");
        assert!(mark(&flash, "Insufficient Balance"));
        assert!(!mark(&flash, "Insufficient Balance"), "同一账户只通知一次");
        assert!(blocked(&pro).is_some(), "同 key 的其它模型一起摘除");
        assert!(blocked(&other).is_none(), "不同 key 是不同账户");
        // 通道是进程级的，并行测试也会往里发：只看本测试的账户。
        let mine: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|n| n.models.iter().any(|m| m == "ds-flash"))
            .collect();
        assert_eq!(mine.len(), 1, "首次登记广播一次");
        assert!(blocked_summary().iter().any(|s| s.contains("ds-pro")));
    }

    #[test]
    fn notice_names_affected_roles() {
        let n = BillingNotice {
            provider: "deepseek".into(),
            models: vec!["ds-flash".into()],
            message: "402".into(),
            roles: vec!["tutor（ds-flash）".into(), "summarizer（ds-flash，整条链不可用）".into()],
        };
        let t = n.render();
        assert!(t.contains("受影响角色：tutor（ds-flash）、summarizer（ds-flash，整条链不可用）"), "{t}");
    }

}
