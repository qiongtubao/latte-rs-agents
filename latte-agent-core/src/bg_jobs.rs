//! bash 后台 job 与会话 / workflow 的集成。
//!
//! bash 工具（`latte_rs_agent_tools::tools::shell`）把 `async: true` 启动、
//! 或前台超过 `timeout` 仍未结束的命令转成后台 job，进程级登记，按
//! `owner` 归属。本模块决定 owner 的粒度并负责 owner 的生命周期：
//!
//! - **owner**：workflow step / delegate 的 runner 用 `sub_id`，主会话
//!   runner 用 `session_id`（即 [`crate::agent::AgentRunner`] 的
//!   `anchor_scope_id`）。runner 每次调工具时把它放进
//!   `ToolExecutionContext.metadata.job_owner`。
//! - **结束即清理**：[`JobOwnerGuard`] 由分派 / 会话的执行函数持有，函数
//!   以任何方式退出（正常返回、出错、取消、future 被 drop）时把该 owner
//!   名下还在跑的 job 整组 kill。此前 workflow step 结束了，它转到后台的
//!   `make all` 还挂着（rocksdb LAT-100 会话）。
//! - **事件**：后台 job 的启动 / 结束经 [`ChatEvent::BackgroundJob`] 进会话
//!   事件流（UI 展示 + ui-sessions jsonl 归档）。工具层只有一个进程级回调，
//!   这里按 owner 路由到对应会话的 `event_tx`。
//! - 完成通知与收尾提醒由 runner 在工具循环里处理（见
//!   [`job_notice_message`] / [`job_stop_reminder`]）。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, Once};

use latte_rs_agent_tools::tools::shell::{self as jobs, JobEvent, JobInfo};
use tokio::sync::broadcast;

use crate::controller::ChatEvent;

struct Route {
    event_tx: broadcast::Sender<ChatEvent>,
    sub_id: Option<String>,
    /// 登记代号：同一 owner 被重新登记（会话 respawn）后，旧 guard
    /// drop 时不能摘掉新登记的路由。
    token: u64,
}

static ROUTES: LazyLock<Mutex<HashMap<String, Route>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static HOOK: Once = Once::new();
static NEXT_TOKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn routes() -> std::sync::MutexGuard<'static, HashMap<String, Route>> {
    ROUTES.lock().unwrap_or_else(|e| e.into_inner())
}

fn to_chat_event(ev: &JobEvent, sub_id: Option<String>) -> ChatEvent {
    ChatEvent::BackgroundJob {
        job_id: ev.job_id.clone(),
        status: ev.status.clone(),
        command: ev.command.clone(),
        cwd: ev.cwd.clone(),
        exit_code: ev.exit_code,
        elapsed_secs: ev.elapsed_secs,
        detail: ev.detail.clone(),
        role_id: ev.role.clone(),
        sub_id,
    }
}

fn install_hook() {
    HOOK.call_once(|| {
        jobs::set_job_event_hook(|ev: &JobEvent| {
            let Some(owner) = ev.owner.as_deref() else {
                return;
            };
            let routes = routes();
            if let Some(route) = routes.get(owner) {
                let _ = route.event_tx.send(to_chat_event(ev, route.sub_id.clone()));
            }
        });
    });
}

/// 某个 owner（分派或会话）的后台 job 生命周期守卫。
///
/// 创建时登记事件路由；Drop 时 kill 该 owner 名下仍在运行的 job、为每个
/// 被 kill 的后台 job 发一条 `stopped` 事件，再摘除路由。
pub struct JobOwnerGuard {
    owner: String,
    reason: &'static str,
    token: u64,
}

impl JobOwnerGuard {
    /// `owner` 为空时返回 `None`（没有可归属的标识，不登记）。
    /// `sub_id`：事件里展示的子会话（主会话传 `None`）。
    /// `reason`：owner 结束时 kill job 的原因，写进事件与 job 结果。
    pub fn new(
        owner: &str,
        event_tx: broadcast::Sender<ChatEvent>,
        sub_id: Option<String>,
        reason: &'static str,
    ) -> Option<Self> {
        if owner.is_empty() {
            return None;
        }
        install_hook();
        let token = NEXT_TOKEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        routes().insert(
            owner.to_string(),
            Route {
                event_tx,
                sub_id,
                token,
            },
        );
        Some(Self {
            owner: owner.to_string(),
            reason,
            token,
        })
    }
}

impl Drop for JobOwnerGuard {
    fn drop(&mut self) {
        let killed = jobs::kill_jobs_owned_by(&self.owner, self.reason);
        let route = {
            let mut routes = routes();
            match routes.get(&self.owner) {
                Some(r) if r.token == self.token => routes.remove(&self.owner),
                // 已被新的登记顶替：只借用它发事件，不摘除。
                Some(r) => Some(Route {
                    event_tx: r.event_tx.clone(),
                    sub_id: r.sub_id.clone(),
                    token: r.token,
                }),
                None => None,
            }
        };
        if let Some(route) = route {
            for info in killed {
                let _ = route.event_tx.send(ChatEvent::BackgroundJob {
                    job_id: info.job_id,
                    status: "stopped".into(),
                    command: info.command,
                    cwd: info.cwd,
                    exit_code: None,
                    elapsed_secs: info.elapsed_secs,
                    detail: Some(self.reason.to_string()),
                    role_id: None,
                    sub_id: route.sub_id.clone(),
                });
            }
        }
    }
}

/// 该 owner 名下已结束、尚未通知过模型的后台 job → 一条注入消息。
/// 每个 job 只通知一次；没有新结束的 job 时返回 `None`。
pub fn job_notice_message(owner: &str) -> Option<String> {
    if owner.is_empty() {
        return None;
    }
    let done = jobs::take_job_notices(owner);
    if done.is_empty() {
        return None;
    }
    let lines: Vec<String> = done
        .iter()
        .map(|j| format!("- {}", jobs::describe_job(j)))
        .collect();
    Some(format!(
        "【后台 job 通知】以下后台 job 已结束：\n{}\n用 bash {{\"jobId\":\"<id>\",\"wait\":true}} 取输出与退出码。",
        lines.join("\n")
    ))
}

/// 该 owner 名下仍在运行的后台 job。
pub fn running_jobs(owner: &str) -> Vec<JobInfo> {
    if owner.is_empty() {
        return Vec::new();
    }
    jobs::running_jobs_owned_by(owner)
}

/// 收尾提醒：分派准备结束时还有后台 job 在跑。分派一结束这些 job 就会被
/// kill，结果也就丢了，所以先提醒模型处理。
pub fn job_stop_reminder(running: &[JobInfo]) -> String {
    let lines: Vec<String> = running
        .iter()
        .map(|j| format!("- {}", jobs::describe_job(j)))
        .collect();
    format!(
        "【系统提醒】你准备结束本次任务，但还有后台 job 在运行：\n{}\n本次任务结束时这些 job 会被终止，输出也会丢失。如果需要它们的结果，用 bash {{\"jobId\":\"<id>\",\"wait\":true,\"timeout\":<秒>}} 等它跑完并核对结果；不需要就用 {{\"jobId\":\"<id>\",\"kill\":true}} 终止。处理完再给出最终答复。",
        lines.join("\n")
    )
}

/// 每个 turn 最多提醒几次（提醒后模型仍直接收尾，就照常结束并 kill）。
pub const JOB_STOP_REMINDERS_PER_TURN: u8 = 2;

#[cfg(test)]
mod tests {
    use super::*;
    use latte_rs_agent_tools::types::ToolExecutionContext;
    use serde_json::json;

    async fn bash(input: serde_json::Value, owner: &str) -> serde_json::Value {
        let tool = latte_rs_agent_tools::tools::ShellToolsPackage::new()
            .tools
            .into_iter()
            .find(|t| t.name == "bash")
            .unwrap();
        let mut ctx = ToolExecutionContext::fresh("bash", 0);
        ctx.metadata = Some(json!({"job_owner": owner, "role": "devops"}));
        (tool.handler)(input, ctx).await.unwrap()
    }

    /// 分派结束（guard drop）时 kill 名下后台 job，并发 running → stopped 事件。
    #[tokio::test]
    async fn guard_drop_kills_owned_jobs_and_emits_events() {
        let (tx, mut rx) = broadcast::channel(16);
        let owner = "bgjobs-test-guard";
        let guard = JobOwnerGuard::new(owner, tx, Some(owner.into()), "分派结束").unwrap();
        let start = bash(json!({"command": "sleep 30", "async": true}), owner).await;
        let id = start["jobId"].as_str().unwrap().to_string();
        assert_eq!(running_jobs(owner).len(), 1);

        drop(guard);
        assert!(running_jobs(owner).is_empty());

        let mut statuses = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let ChatEvent::BackgroundJob {
                job_id,
                status,
                sub_id,
                ..
            } = ev
            {
                assert_eq!(job_id, id);
                assert_eq!(sub_id.as_deref(), Some(owner));
                statuses.push(status);
            }
        }
        assert_eq!(statuses, vec!["running", "stopped"]);
    }

    /// 后台 job 结束后，owner 的下一次模型调用拿到一次（且只一次）通知。
    #[tokio::test]
    async fn finished_job_is_noticed_once() {
        let (tx, _rx) = broadcast::channel(16);
        let owner = "bgjobs-test-notice";
        let _guard = JobOwnerGuard::new(owner, tx, None, "会话结束").unwrap();
        let start = bash(json!({"command": "echo hi", "async": true}), owner).await;
        let id = start["jobId"].as_str().unwrap().to_string();
        let mut notice = None;
        for _ in 0..50 {
            notice = job_notice_message(owner);
            if notice.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let notice = notice.expect("应收到完成通知");
        assert!(
            notice.contains(&id) && notice.contains("exit=0"),
            "{notice}"
        );
        assert!(job_notice_message(owner).is_none(), "只通知一次");
        // 通知后结果仍可取。
        let out = bash(json!({"jobId": id, "wait": true}), owner).await;
        assert_eq!(out["stdout"].as_str().unwrap().trim(), "hi");
    }

    /// 别的 owner 的 job 不受影响。
    #[tokio::test]
    async fn guard_only_kills_its_own_jobs() {
        let (tx, _rx) = broadcast::channel(16);
        let other = "bgjobs-test-other";
        let start = bash(json!({"command": "sleep 30", "async": true}), other).await;
        let id = start["jobId"].as_str().unwrap().to_string();
        drop(JobOwnerGuard::new("bgjobs-test-mine", tx, None, "分派结束"));
        assert_eq!(running_jobs(other).len(), 1);
        let _ = bash(json!({"jobId": id, "kill": true}), other).await;
    }

    #[test]
    fn empty_owner_is_not_registered() {
        let (tx, _rx) = broadcast::channel(1);
        assert!(JobOwnerGuard::new("", tx, None, "x").is_none());
        assert!(job_notice_message("").is_none());
    }
}

/// 端到端：真实 bash 工具 + 桩模型驱动 `AgentRunner::run_turn`。
#[cfg(test)]
mod runner_tests {
    use latte_ai::models::{Message, Model};
    use latte_ai::params::GenerateParams;
    use latte_rs_agent_tools::prelude::create_tool_manager;
    use latte_rs_agent_tools::types::ToolManager as _;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::agent::{Agent, AgentRunner};

    fn model_at(server: &MockServer) -> Model {
        Model {
            id: "stub".into(),
            name: "stub".into(),
            api: latte_ai::models::ApiType::OpenAiCompletions,
            provider: "test".into(),
            base_url: server.uri(),
            api_key: "k".into(),
            context_window: 32000,
            max_tokens: 1024,
            omit_max_tokens: false,
            max_tokens_field: Default::default(),
            prompt_cache: false,
            prompt_cache_key: None,
            supports_thinking: false,
            supports_vision: false,
            cost_per_million_input: 0.0,
            cost_per_million_output: 0.0,
            timeout_secs: None,
        }
    }

    fn body(content: &str, tool_call: Option<(&str, serde_json::Value)>) -> String {
        let has_call = tool_call.is_some();
        let mut message = serde_json::json!({ "role": "assistant", "content": content });
        if let Some((id, args)) = tool_call {
            message["tool_calls"] = serde_json::json!([{
                "id": id,
                "type": "function",
                "function": { "name": "bash", "arguments": args.to_string() }
            }]);
        }
        serde_json::json!({
            "id": "c",
            "object": "chat.completion",
            "model": "stub",
            "choices": [{
                "index": 0,
                "message": message,
                "finish_reason": if has_call { "tool_calls" } else { "stop" }
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        })
        .to_string()
    }

    async fn script(server: &MockServer, bodies: Vec<String>) {
        for b in bodies {
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .respond_with(ResponseTemplate::new(200).set_body_string(b))
                .up_to_n_times(1)
                .mount(server)
                .await;
        }
    }

    fn runner(server: &MockServer) -> AgentRunner {
        let tm = create_tool_manager();
        let bash = latte_rs_agent_tools::tools::ShellToolsPackage::new()
            .tools
            .into_iter()
            .find(|t| t.name == "bash")
            .unwrap();
        tm.register(bash, None);
        let role = crate::role::Role {
            id: "devops".into(),
            name: "DevOps".into(),
            category: crate::role::RoleCategory::Execution,
            system_prompt: "You are devops.".into(),
            default_model_tier: crate::model_resolver::ModelTier::Standard,
            model_chain: vec![],
            default_params: GenerateParams::default(),
            allowed_tools: vec!["bash".into()],
            icon: "⚙".into(),
        };
        let agent = Agent::new_with_chain(
            "t".into(),
            role,
            vec![model_at(server)],
            GenerateParams::default(),
        )
        .unwrap();
        AgentRunner::new_with_tools(agent, tm).with_role("devops")
    }

    async fn request_texts(server: &MockServer) -> Vec<String> {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).into_owned())
            .collect()
    }

    /// 分派 runner 收尾时还有后台 job 在跑 → 注入收尾提醒（每 turn 至多
    /// JOB_STOP_REMINDERS_PER_TURN 次），之后照常结束。
    #[tokio::test]
    async fn sub_agent_is_reminded_about_running_jobs_before_finishing() {
        let server = MockServer::start().await;
        script(
            &server,
            vec![
                body(
                    "",
                    Some((
                        "c1",
                        serde_json::json!({"command": "sleep 30", "async": true}),
                    )),
                ),
                body("done", None),
                body("done", None),
                body("done", None),
            ],
        )
        .await;
        let owner = "bgjobs-runner-sub";
        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        let guard = super::JobOwnerGuard::new(owner, tx, Some(owner.into()), "分派结束");
        let mut r = runner(&server).with_sub_id(owner);
        let out = r.run_turn(&[Message::user("go")], None).await.unwrap();
        assert_eq!(out.trim(), "done");

        let reqs = request_texts(&server).await;
        assert_eq!(
            reqs.len(),
            2 + super::JOB_STOP_REMINDERS_PER_TURN as usize,
            "每次收尾提醒多一次模型调用"
        );
        assert!(reqs[2].contains("还有后台 job 在运行"), "{}", reqs[2]);
        assert_eq!(super::running_jobs(owner).len(), 1);
        drop(guard);
        assert!(
            super::running_jobs(owner).is_empty(),
            "分派结束后 job 被 kill"
        );
    }

    /// 后台 job 跑完后，下一次模型调用收到完成通知。
    #[tokio::test]
    async fn finished_job_notice_reaches_next_model_call() {
        let server = MockServer::start().await;
        script(
            &server,
            vec![
                body(
                    "",
                    Some((
                        "c1",
                        serde_json::json!({"command": "echo hi", "async": true}),
                    )),
                ),
                body(
                    "",
                    Some(("c2", serde_json::json!({"command": "sleep 0.5"}))),
                ),
                body("done", None),
            ],
        )
        .await;
        let owner = "bgjobs-runner-main";
        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        let _guard = super::JobOwnerGuard::new(owner, tx, None, "会话结束");
        let mut r = runner(&server).with_session_id(owner);
        r.run_turn(&[Message::user("go")], None).await.unwrap();

        let reqs = request_texts(&server).await;
        assert_eq!(reqs.len(), 3);
        assert!(!reqs[1].contains("后台 job 通知"), "job 还没结束时不通知");
        assert!(reqs[2].contains("后台 job 通知"), "{}", reqs[2]);
        assert!(reqs[2].contains("exit=0"), "{}", reqs[2]);
    }
}
