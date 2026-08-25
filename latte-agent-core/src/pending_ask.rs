//! 阻塞 `ask` 挂起项的**跨进程**落盘记录。
//!
//! [`crate::choice`] 的 `PENDING` 是进程内路由表：`choice_id` →
//! `oneshot::Sender`。它解决"弹框事件丢了"，解决不了"进程没了"——
//! 服务器一重启，等待答案的 oneshot、正 park 的 workflow future、
//! 整个 AgentRunner 一起消失。答案再也没有活着的接收方可投，那一步
//! 永远不会继续。
//!
//! 但让 session 继续下去并不需要"把等待方救活"。resume 的地基本来就
//! 是从 checkpoint 预载已答问题（[`crate::workflow::AnswerLog`] +
//! `CheckpointRecord::Answer`），所以只要知道这个挂起的 ask 属于哪个
//! run，就能这样闭环：
//!
//! 1. 挂起时把 `(choice_id, session_id, wf_id, role_id, question,
//!    弹框事件快照)` 落盘到 `<cwd>/.latte/pending-asks/<choice_id>.json`；
//! 2. 重启后 SSE / `GET /api/chat/pending-prompts` 把它当成**仍然可答**
//!    的弹框补发给前端（[`load_for_session`]）；
//! 3. 用户点确认 → `POST /api/chat/choice-answer` 在内存表里找不到等待
//!    方，转而命中这条记录 → 把答案补写成一条 `Answer` 行
//!    （[`crate::workflow::record_answer_for_run`]）→ 触发
//!    `POST /api/workflows/resume`；
//! 4. 续跑的 run 跳过已完成的 step，走到同一个 `ask` 时 `recall` 直接
//!    命中，不再弹框，流水线继续。
//!
//! 落盘位置跟 checkpoint 同一个 `.latte/` 根（`workflow-runs` 的邻居），
//! 因为这两份东西必须同生共死：checkpoint 没了，挂起记录也没有意义。
//!
//! 只有 workflow run 里的阻塞 ask 会落盘。顶层 turn 的 fire-and-forget
//! ask 没有等待方也没有 checkpoint（答案就是下一条 user 消息），
//! CLI / 测试里的 run 没有 session_id（无从续跑），两者都跳过。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 一条落盘的挂起阻塞 ask。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingAsk {
    /// 本次提问的 id（= 内存表的键，也是文件名）。
    pub choice_id: String,
    /// 所属 UI session —— 续跑要用它 resolve session。
    pub session_id: String,
    /// 所属 workflow run 的 checkpoint id。
    pub wf_id: String,
    /// 提问的角色。
    pub role_id: String,
    /// 问题原文（trim 后）：`AnswerLog::recall` 的匹配键，补写
    /// `Answer` 行时必须与它逐字一致，否则续跑会重新弹框。
    pub question: String,
    /// 弹框事件快照（前端事件 JSON，与 SSE / history 同构）。
    /// 存 JSON 字符串而不是 `ChatEvent`：落盘格式不该被内部枚举的
    /// 演进绑死，读回时解析失败就跳过这条，不至于让整个目录不可读。
    pub event_json: String,
    /// 落盘时间（秒）。用于淘汰陈旧记录。
    pub created_at: u64,
}

/// 超过这个年龄的挂起记录不再补发。用户一周前没答的题，续跑的上下文
/// 早已不成立，弹出来只会让人困惑（记录本身留着，便于排查）。
const MAX_AGE_SECS: u64 = 7 * 24 * 3600;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn dir(cwd: &Path) -> PathBuf {
    cwd.join(".latte").join("pending-asks")
}

/// `choice_id` 来自进程内生成（`choice-{role_id}-{seq}`），但 role_id
/// 源自配置文件，仍按路径分量校验后再拼文件名。
fn safe_id(choice_id: &str) -> bool {
    !choice_id.is_empty()
        && !choice_id.contains('/')
        && !choice_id.contains('\\')
        && !choice_id.contains("..")
        && choice_id.len() <= 200
}

fn path_for(cwd: &Path, choice_id: &str) -> Option<PathBuf> {
    safe_id(choice_id).then(|| dir(cwd).join(format!("{choice_id}.json")))
}

/// 落盘一条挂起记录。**在广播弹框之前调用**（同
/// [`crate::choice::register`]：反序时用户秒答会先来查表，查不到就
/// 走不到孤儿恢复路径）。
///
/// 失败只 warn：挂起记录是恢复辅助，从不作为让 run 失败的理由。
pub fn persist(
    cwd: &Path,
    choice_id: &str,
    session_id: &str,
    wf_id: &str,
    role_id: &str,
    question: &str,
    event_json: &str,
) {
    let Some(path) = path_for(cwd, choice_id) else {
        tracing::warn!(choice_id, "pending ask id 不合法，跳过落盘");
        return;
    };
    let rec = PendingAsk {
        choice_id: choice_id.to_string(),
        session_id: session_id.to_string(),
        wf_id: wf_id.to_string(),
        role_id: role_id.to_string(),
        question: question.trim().to_string(),
        event_json: event_json.to_string(),
        created_at: now_secs(),
    };
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(dir(cwd))?;
        let json = serde_json::to_string(&rec)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&path, json)
    };
    if let Err(e) = write() {
        tracing::warn!(choice_id, error = %e, "pending ask 落盘失败（run 继续）");
    }
}

/// 读一条挂起记录（不删）。
pub fn load(cwd: &Path, choice_id: &str) -> Option<PendingAsk> {
    let path = path_for(cwd, choice_id)?;
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// 删掉一条挂起记录（幂等）。答案送达（活着的 run）或补写进 checkpoint
/// （孤儿恢复）之后调用。
pub fn remove(cwd: &Path, choice_id: &str) {
    if let Some(path) = path_for(cwd, choice_id) {
        let _ = std::fs::remove_file(path);
    }
}

/// 本 session 仍需要补发的挂起 ask，按 `choice_id` 稳定排序。
///
/// 过滤掉三类不该再弹的：
/// - 太老的（见 [`MAX_AGE_SECS`]）；
/// - checkpoint 文件已经不在的（run 的落盘被清了，续跑无从下手）；
/// - 问题已经有答案的 —— 这是**关键的自洽点**：无论答案是活着的 run
///   记的还是重启后补写的，它都会成为 checkpoint 里的一条 `Answer` 行，
///   于是这条挂起记录自动失效，不会重复弹给用户。
pub fn load_for_session(cwd: &Path, session_id: &str) -> Vec<PendingAsk> {
    let Ok(rd) = std::fs::read_dir(dir(cwd)) else {
        return Vec::new();
    };
    let now = now_secs();
    let mut out: Vec<PendingAsk> = Vec::new();
    for entry in rd.flatten() {
        if entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(rec) = serde_json::from_str::<PendingAsk>(&raw) else {
            continue;
        };
        if rec.session_id != session_id {
            continue;
        }
        if now.saturating_sub(rec.created_at) > MAX_AGE_SECS {
            continue;
        }
        match crate::workflow::load_checkpoint(cwd, &rec.wf_id) {
            Ok(state) if state.has_answer(&rec.question) => continue,
            Ok(_) => {}
            // checkpoint 没了/坏了 → 续跑不可能成功，别给用户一个点了
            // 没反应的按钮。
            Err(_) => continue,
        }
        out.push(rec);
    }
    out.sort_by(|a, b| a.choice_id.cmp(&b.choice_id));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个最小可用的 checkpoint（load_checkpoint 需要 meta 行）。
    fn seed_checkpoint(cwd: &Path, wf_id: &str) {
        let dir = cwd.join(".latte").join("workflow-runs");
        std::fs::create_dir_all(&dir).unwrap();
        let meta = serde_json::json!({
            "type": "meta", "wf_id": wf_id,
            "workflow_name": "design_and_plan", "topic": "主题", "started_at": 1,
        });
        std::fs::write(dir.join(format!("{wf_id}.jsonl")), format!("{meta}\n")).unwrap();
    }

    fn persist_one(cwd: &Path, choice_id: &str, sid: &str, wf_id: &str, question: &str) {
        persist(cwd, choice_id, sid, wf_id, "tutor", question, r#"{"type":"ChoiceRequested"}"#);
    }

    #[test]
    fn round_trip_and_remove() {
        let d = tempfile::tempdir().unwrap();
        let cwd = d.path();
        seed_checkpoint(cwd, "wf-1");
        persist_one(cwd, "choice-tutor-0", "sess-a", "wf-1", " 选哪个？ ");

        let rec = load(cwd, "choice-tutor-0").expect("落盘的记录应读得回来");
        assert_eq!(rec.session_id, "sess-a");
        assert_eq!(rec.wf_id, "wf-1");
        // question 必须是 trim 后的：它要和 AnswerLog 的匹配键逐字一致。
        assert_eq!(rec.question, "选哪个？");

        remove(cwd, "choice-tutor-0");
        assert!(load(cwd, "choice-tutor-0").is_none());
        // 幂等。
        remove(cwd, "choice-tutor-0");
    }

    /// 按 session 隔离：A 会话的挂起 ask 不能弹到 B 会话里。
    #[test]
    fn load_for_session_scopes_by_session() {
        let d = tempfile::tempdir().unwrap();
        let cwd = d.path();
        seed_checkpoint(cwd, "wf-1");
        persist_one(cwd, "choice-tutor-0", "sess-a", "wf-1", "A 的问题");
        persist_one(cwd, "choice-tutor-1", "sess-b", "wf-1", "B 的问题");

        let a = load_for_session(cwd, "sess-a");
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].question, "A 的问题");
        assert_eq!(load_for_session(cwd, "sess-b").len(), 1);
        assert!(load_for_session(cwd, "sess-c").is_empty());
    }

    /// 自洽点：答案一旦进了 checkpoint（活着的 run 记的，或重启后补写
    /// 的），这条挂起记录就自动失效 —— 否则用户会被要求重答同一题。
    #[test]
    fn answered_question_is_not_replayed() {
        let d = tempfile::tempdir().unwrap();
        let cwd = d.path();
        seed_checkpoint(cwd, "wf-1");
        persist_one(cwd, "choice-tutor-0", "sess-a", "wf-1", "选哪个？");
        assert_eq!(load_for_session(cwd, "sess-a").len(), 1);

        crate::workflow::record_answer_for_run(cwd, "wf-1", "tutor", "选哪个？", "A");
        assert!(
            load_for_session(cwd, "sess-a").is_empty(),
            "已答过的问题不该再补发成弹框"
        );
    }

    /// checkpoint 没了 = 续跑不可能成功，不给用户一个点了没反应的按钮。
    #[test]
    fn missing_checkpoint_is_not_replayed() {
        let d = tempfile::tempdir().unwrap();
        let cwd = d.path();
        // 故意不建 checkpoint。
        persist_one(cwd, "choice-tutor-0", "sess-a", "wf-gone", "选哪个？");
        assert!(load_for_session(cwd, "sess-a").is_empty());
    }

    /// 陈旧记录淘汰：一周前没答的题，续跑上下文早已不成立。
    #[test]
    fn stale_record_is_not_replayed() {
        let d = tempfile::tempdir().unwrap();
        let cwd = d.path();
        seed_checkpoint(cwd, "wf-1");
        persist_one(cwd, "choice-tutor-0", "sess-a", "wf-1", "选哪个？");
        // 手改 created_at 到 8 天前。
        let p = path_for(cwd, "choice-tutor-0").unwrap();
        let mut rec: PendingAsk = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        rec.created_at = now_secs() - (8 * 24 * 3600);
        std::fs::write(&p, serde_json::to_string(&rec).unwrap()).unwrap();

        assert!(load_for_session(cwd, "sess-a").is_empty());
    }

    /// 路径穿越防护：choice_id 不可逃出 pending-asks 目录。
    #[test]
    fn rejects_unsafe_choice_id() {
        let d = tempfile::tempdir().unwrap();
        let cwd = d.path();
        assert!(path_for(cwd, "../../etc/passwd").is_none());
        assert!(path_for(cwd, "a/b").is_none());
        assert!(path_for(cwd, "").is_none());
        assert!(path_for(cwd, "choice-tutor-0").is_some());
    }
}
