//! Workflow step 的**进行中上下文**落盘，让重启后的续跑能接着中断处继续，
//! 而不是把被中断的 step 从头重跑。
//!
//! # 为什么需要
//!
//! checkpoint（`workflow-runs/<wf_id>.jsonl`）只记**已完成** step 的产出。
//! 一个 step 内 subagent 的对话（assistant 的 tool_calls、工具结果、已读过的
//! 文件）只活在进程内存里；进程一重启，续跑只能起一个全新 subagent、重读
//! prompt、重做全部取证。实测：proofreader 已经跑了几十分钟，重启后一切归零。
//!
//! # 布局
//!
//! 一个**根 run** 一个目录：`<cwd>/.latte/workflow-runs/<root_wf_id>.inflight/`
//! （目录扩展名不是 `jsonl`，所有按 `.jsonl` 扫 checkpoint 的读者天然忽略它）。
//! 里面的文件名 `<kind>.<key>.json`：
//!
//! | kind          | 写入者                     | 内容                          |
//! |---------------|----------------------------|-------------------------------|
//! | `live-step`   | 正在跑的 step 的 runner    | [`SavedStep`]                 |
//! | `live-nested` | 正在跑的嵌套 workflow      | `{"wf_id": 嵌套 run 的 id}`   |
//! | `carry-*`     | [`adopt`]（续跑起跑时）    | 同名 `live-*` 改名而来        |
//!
//! `live-*` 与 `carry-*` 分开是为了**只在真正的续跑里**恢复：正常运行里
//! step 失败后引擎的契约重试/父级重试必须仍是全新 subagent（见
//! `run_step_speaker` 的文档），它们只读 `carry-*`，而 `carry-*` 只由
//! [`adopt`] 在续跑起跑那一刻生成，且读取即消费。
//!
//! 续跑会给根 run 换新的 `wf_id`，所以 [`adopt`] 负责把旧根的目录搬到新根名下。

use std::path::{Path, PathBuf};

use latte_ai::models::Message;
use serde::{Deserialize, Serialize};

/// 一个被中断 step 的对话快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedStep {
    /// 起跑时的完整 prompt。续跑前比对：不一致（上游产出/反馈变了）说明
    /// 快照属于另一次任务，必须丢弃。
    pub prompt: String,
    /// 不含 system 的全部消息（用户 prompt + 已完成的 tool 往返）。
    /// 只在**工具轮次边界**落盘，所以 tool_call ↔ tool_result 一定成对。
    pub messages: Vec<Message>,
    /// 本 turn 已发生的工具调用数与摘要。引擎的 `require_tools` 契约只看
    /// 「本轮真实调用过什么」，恢复后若丢了这份统计，一个早已调过工具的
    /// step 会被误判成没调、触发整轮重跑。
    #[serde(default)]
    pub tool_count: usize,
    #[serde(default)]
    pub tool_summaries: Vec<String>,
}

/// runner 侧的落盘配置：在哪个根 run 名下、用哪个 key、对应哪份起跑 prompt。
#[derive(Debug, Clone)]
pub struct StepJournalCfg {
    pub cwd: PathBuf,
    pub root: String,
    pub key: String,
    pub prompt: String,
}

impl StepJournalCfg {
    pub fn save(&self, messages: &[Message], tool_count: usize, tool_summaries: &[String]) {
        save_step(
            &self.cwd,
            &self.root,
            &self.key,
            &SavedStep {
                prompt: self.prompt.clone(),
                messages: messages.to_vec(),
                tool_count,
                tool_summaries: tool_summaries.to_vec(),
            },
        );
    }
}
fn inflight_dir(cwd: &Path, root_wf_id: &str) -> PathBuf {
    cwd.join(".latte")
        .join("workflow-runs")
        .join(format!("{root_wf_id}.inflight"))
}

/// 文件名安全化：只留字母数字和 `_-`，其余（含路径分隔符、`.`）换成 `_`。
fn sanitize(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn file(cwd: &Path, root_wf_id: &str, kind: &str, key: &str) -> PathBuf {
    inflight_dir(cwd, root_wf_id).join(format!("{kind}.{}.json", sanitize(key)))
}

/// 原子写：先写临时文件再 rename，进程在任意时刻被杀都不会留下半截 JSON。
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// 记录当前 step 的对话。失败只 warn：快照是恢复手段，不能拖垮运行。
pub fn save_step(cwd: &Path, root_wf_id: &str, key: &str, saved: &SavedStep) {
    let path = file(cwd, root_wf_id, "live-step", key);
    let result = serde_json::to_vec(saved)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        .and_then(|bytes| write_atomic(&path, &bytes));
    if let Err(e) = result {
        tracing::warn!(key, error = %e, "step journal write failed (run continues)");
    }
}

/// step 正常收尾（成功或确定性失败）后清掉自己的快照。
pub fn clear_step(cwd: &Path, root_wf_id: &str, key: &str) {
    let _ = std::fs::remove_file(file(cwd, root_wf_id, "live-step", key));
}

/// 取走续跑带来的快照（读取即消费）。`prompt` 不一致 / 文件损坏 → None。
pub fn take_carried_step(
    cwd: &Path,
    root_wf_id: &str,
    key: &str,
    prompt: &str,
) -> Option<SavedStep> {
    let path = file(cwd, root_wf_id, "carry-step", key);
    let raw = std::fs::read(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let saved: SavedStep = serde_json::from_slice(&raw).ok()?;
    (saved.prompt == prompt && !saved.messages.is_empty()).then_some(saved)
}

/// 嵌套 workflow 起跑时登记自己的 `wf_id`。
pub fn save_nested_link(cwd: &Path, root_wf_id: &str, link: &str, nested_wf_id: &str) {
    let path = file(cwd, root_wf_id, "live-nested", link);
    let body = serde_json::json!({ "wf_id": nested_wf_id }).to_string();
    if let Err(e) = write_atomic(&path, body.as_bytes()) {
        tracing::warn!(link, error = %e, "nested link write failed (run continues)");
    }
}

/// 嵌套 workflow 成功收尾后摘掉登记。**失败不摘**：失败的嵌套 run 恰恰是
/// 续跑时要接着走的那个。
pub fn clear_nested_link(cwd: &Path, root_wf_id: &str, link: &str) {
    let _ = std::fs::remove_file(file(cwd, root_wf_id, "live-nested", link));
}

/// 取走续跑带来的嵌套 run id（读取即消费）。
pub fn take_carried_nested(cwd: &Path, root_wf_id: &str, link: &str) -> Option<String> {
    let path = file(cwd, root_wf_id, "carry-nested", link);
    let raw = std::fs::read(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    v.get("wf_id")?.as_str().map(str::to_string)
}

/// 续跑起跑：把旧根 run 目录里的 `live-*` 搬到新根名下并改名 `carry-*`，
/// 旧目录整个删除（旧根的 `carry-*` 若还没被消费就是过期的，一并丢弃）。
pub fn adopt(cwd: &Path, old_root: &str, new_root: &str) {
    let old = inflight_dir(cwd, old_root);
    let Ok(entries) = std::fs::read_dir(&old) else {
        return;
    };
    let new = inflight_dir(cwd, new_root);
    let _ = std::fs::create_dir_all(&new);
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(rest) = name.strip_prefix("live-") {
            let _ = std::fs::rename(e.path(), new.join(format!("carry-{rest}")));
        }
    }
    let _ = std::fs::remove_dir_all(&old);
}

/// 删除某根 run 的整个进行中目录（session 删除时用）。
pub fn remove_root(cwd: &Path, root_wf_id: &str) {
    let _ = std::fs::remove_dir_all(inflight_dir(cwd, root_wf_id));
}

/// 从 `wf-<name>-<micros>` 里取出 workflow 名。`wf_id` 只由
/// `run_workflow_inner` 用这个格式生成；格式不符时原样返回，保证 key 稳定。
pub fn workflow_name_of(wf_id: &str) -> &str {
    let rest = wf_id.strip_prefix("wf-").unwrap_or(wf_id);
    match rest.rsplit_once('-') {
        Some((name, tail)) if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => rest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(prompt: &str) -> SavedStep {
        SavedStep {
            prompt: prompt.into(),
            messages: vec![Message::user("q"), Message::assistant("a")],
            tool_count: 0,
            tool_summaries: vec![],
        }
    }

    #[test]
    fn workflow_name_strips_prefix_and_timestamp() {
        assert_eq!(
            workflow_name_of("wf-doc_review_loop-1790766106987597"),
            "doc_review_loop"
        );
        // 名字里自带 '-' 和数字尾巴也不能被多切。
        assert_eq!(workflow_name_of("wf-a-b-12"), "a-b");
        assert_eq!(workflow_name_of("weird"), "weird");
    }

    /// 只有经过 adopt（续跑起跑）的快照才会被读到：同一 run 里 step 失败后
    /// 引擎重试必须拿到全新 subagent，不能读到自己刚失败的那份。
    #[test]
    fn live_snapshot_is_invisible_until_adopted() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        save_step(cwd, "wf-old-1", "k", &saved("P"));
        assert!(take_carried_step(cwd, "wf-old-1", "k", "P").is_none());

        adopt(cwd, "wf-old-1", "wf-new-2");
        assert!(!inflight_dir(cwd, "wf-old-1").exists(), "旧目录应被搬走");
        let got = take_carried_step(cwd, "wf-new-2", "k", "P").expect("adopt 后可读");
        assert_eq!(got.messages.len(), 2);
        // 读取即消费。
        assert!(take_carried_step(cwd, "wf-new-2", "k", "P").is_none());
    }

    #[test]
    fn prompt_mismatch_discards_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        save_step(cwd, "r1", "k", &saved("旧 prompt"));
        adopt(cwd, "r1", "r2");
        assert!(take_carried_step(cwd, "r2", "k", "新 prompt").is_none());
    }

    #[test]
    fn cleared_step_does_not_survive_adopt() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        save_step(cwd, "r1", "k", &saved("P"));
        clear_step(cwd, "r1", "k");
        adopt(cwd, "r1", "r2");
        assert!(take_carried_step(cwd, "r2", "k", "P").is_none());
    }

    /// 二次续跑：上一次搬来但没消费的 carry-* 是过期的，不能再往后传。
    #[test]
    fn unconsumed_carry_is_not_forwarded() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        save_step(cwd, "r1", "k", &saved("P"));
        adopt(cwd, "r1", "r2");
        adopt(cwd, "r2", "r3");
        assert!(take_carried_step(cwd, "r3", "k", "P").is_none());
    }

    #[test]
    fn nested_link_roundtrip_and_failure_keeps_link() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        save_nested_link(cwd, "r1", "learn_loop.doc_review.doc_review_loop", "wf-x-9");
        // 未 adopt 不可见。
        assert!(take_carried_nested(cwd, "r1", "learn_loop.doc_review.doc_review_loop").is_none());
        adopt(cwd, "r1", "r2");
        assert_eq!(
            take_carried_nested(cwd, "r2", "learn_loop.doc_review.doc_review_loop").as_deref(),
            Some("wf-x-9")
        );
    }

    #[test]
    fn nested_link_cleared_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        save_nested_link(cwd, "r1", "l", "wf-x-9");
        clear_nested_link(cwd, "r1", "l");
        adopt(cwd, "r1", "r2");
        assert!(take_carried_nested(cwd, "r2", "l").is_none());
    }

    #[test]
    fn keys_cannot_escape_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        save_step(cwd, "r1", "../../etc/x", &saved("P"));
        let names: Vec<String> = std::fs::read_dir(inflight_dir(cwd, "r1"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 1);
        assert!(!names[0].contains('/'));
    }
}
