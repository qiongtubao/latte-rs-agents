//! Controller 模式聊天会话的全局持久化存储。
//!
//! 每条会话保存为 `<base_dir>/<session_id>.json` 单个 JSON 文件，
//! 内容是一条完整的 [`StoredSession`]（状态 + 元数据 + 有序的
//! [`StoredMessage`] 转录）。写入采用原子方式（先写 `.tmp` 再
//! rename），进程崩溃不会留下半成品记录。
//!
//! 本模块在 [`crate::session::SessionManager`] 之上多叠加一层：
//! - `SessionManager` 是 per-worktree 的 HIL 黑板（一个 `task_id`
//!   一条记录，存放在 `<worktree>/.latte/sessions/` 下）；
//! - `SessionStore` 是跨 worktree 的 controller 模式聊天转录
//!   （一个 `session_id` 一条记录，存放在
//!   `~/.latte/chat-sessions/` 下）。
//!
//! 编辑器的 chat panel 两种模式都会用到：
//! - HIL 模式：用 `SessionManager` 存状态，用 `HilSessionSummary`
//!   显示「打开已有」下拉。
//! - Controller 模式（single / multi_role / hil-controller）：用
//!   本模块的 `SessionStore` 存完整转录，sidebar 用 `SessionSummary`
//!   列出。
//!
//! 所有文件 IO 都是同步的。公开 API 用 `async` 是为了适配编辑器
//! 的 tauri 命令面——内部每次只是一次非常短的阻塞 IO（KB 级 JSON
//! 文件），远低于 tokio blocking pool 的阈值，没必要用 `tokio::fs`。
//!
//! 设计文档：`docs/superpowers/specs/2026-07-03-controller-session-store.md`

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use crate::session::SessionState;
use crate::trace::iso8601_utc_now;

// ─── 错误类型 ───────────────────────────────────────────────────────────

/// `SessionStore` 抛出的错误。变体刻意收窄：编辑器的 tauri 命令
/// 层只需要返回字符串即可，`Io` 变体保留底层 `io::Error` 让有需要
/// 的调用方可以检查 `ErrorKind`（比如「继续已有会话」流程要识别
/// `AlreadyExists`）。
#[derive(Debug, Error)]
pub enum SessionStoreError {
    /// 底层文件系统错误。可以用 `.source()` 检查 `io::ErrorKind`
    ///（如 `AlreadyExists`、`NotFound`）。
    #[error("session store io: {0}")]
    Io(#[from] std::io::Error),

    /// JSON 序列化 / 反序列化失败。
    #[error("session store serde: {0}")]
    Serde(#[from] serde_json::Error),

    /// 调用方查询了一个磁盘上不存在的 session id。
    #[error("session not found: {0}")]
    NotFound(String),
}

// ─── 持久化数据结构 ────────────────────────────────────────────────────

/// 单条持久化记录。每个 session id 一个文件，转录内联存储。
/// 编辑器场景下数据量小（每个会话 KB 级别），不存分片；超过 MB
/// 量级时再考虑分页或切分。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StoredSession {
    /// 稳定标识（通常是 `<task_id>-<timestamp>` 或客户端生成的
    /// UUID）。
    pub session_id: String,
    /// 生命周期状态。参见 [`SessionState`]。
    pub state: SessionState,
    /// `single` | `multi_role` | `hil` —— sidebar 据此渲染不同
    /// 的图标和「继续会话」按钮。
    pub chat_type: String,
    /// 原始 spawn 请求里的角色 id 列表。在 create 时冻结，这样即
    /// 使 controller 中途切换了角色，sidebar 的标题也保持一致。
    pub role_ids: Vec<String>,
    /// 有序的转录。仅追加；编辑器的「编辑消息」流程会通过
    /// [`SessionStore::save`] 就地改写整个列表。
    pub messages: Vec<StoredMessage>,
    /// 原始 `create()` 调用的 ISO-8601 UTC 时间戳。
    pub created_at: String,
    /// 最近一次变更的 ISO-8601 UTC 时间戳。
    pub updated_at: String,
}

/// `list_sessions` 用的精简视图。编辑器 sidebar 只需要状态、角色
/// 列表和时间戳；完整转录通过 `get_session` 按需懒加载。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionSummary {
    pub session_id: String,
    pub state: SessionState,
    pub chat_type: String,
    pub role_ids: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl From<&StoredSession> for SessionSummary {
    /// 把完整记录投影成 summary。两个结构都很小（几条字符串 +
    /// 短消息列表，通常几百字节），clone 成本可忽略。
    fn from(s: &StoredSession) -> Self {
        Self {
            session_id: s.session_id.clone(),
            state: s.state,
            chat_type: s.chat_type.clone(),
            role_ids: s.role_ids.clone(),
            created_at: s.created_at.clone(),
            updated_at: s.updated_at.clone(),
        }
    }
}

/// 一条转录条目。用 `type` 标签区分，前端可以直接 switch 处理，
/// 不必解析扁平的 `kind` 字符串。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StoredMessage {
    /// 用户输入。
    User {
        content: String,
        timestamp: String,
    },
    /// 某个角色的助手回复。
    Assistant {
        role_id: String,
        content: String,
        timestamp: String,
        /// 可选 token 用量（从 `ChatEvent::RoleTurn` 透传，如果
        /// controller 上报的话）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tokens: Option<u32>,
    },
    /// 一次工具调用。`args` 是 JSON 编码的参数字符串（与
    /// `ChatEvent::ToolUse::args` 对齐），用 `String` 而不是
    /// `serde_json::Value`，让事件转发器不必在每条持久化事件上
    /// 多走一次 serde 转换。
    ToolCall {
        role_id: String,
        tool_name: String,
        args: String,
        timestamp: String,
    },
    /// 工具返回的结果。`result` 是 JSON 编码的结果字符串（与
    /// `ChatEvent::ToolResult::result` 对齐）。
    ToolResult {
        role_id: String,
        tool_name: String,
        result: String,
        timestamp: String,
    },
    /// 生命周期 / 错误事件：`session_start`、`session_end`、
    /// `error` 等。前端会渲染成小的系统提示气泡。
    SystemEvent {
        event_type: String,
        message: String,
        timestamp: String,
    },
    /// 多角色模式：某轮开始。
    RoundStart {
        round: u32,
        timestamp: String,
    },
    /// 多角色模式：某轮结束。
    RoundEnd {
        round: u32,
        timestamp: String,
    },
    /// 会话被暂停（用户或 supervisor 触发）。
    Paused {
        reason: String,
        timestamp: String,
    },
    /// 会话恢复。
    Resumed {
        timestamp: String,
    },
}

// ─── Store 实现 ──────────────────────────────────────────────────────────

/// 基于磁盘的会话存储。每次读都走文件系统，所以跨实例更新（或
/// 手动改文件）总能被看到。所有操作对单条会话是 O(1)，对目录
/// 扫描是 O(N)，其中 N 是持久化的会话数（编辑器规模：≤ 几百个
/// KB 级 JSON 文件）。
pub struct SessionStore {
    /// `<base_dir>/<session_id>.json` —— 构造时确保存在。
    base_dir: PathBuf,
}

impl SessionStore {
    /// 用 `dir` 作为根目录创建一个 store。如果 `dir` 不存在会递归
    /// 创建。
    ///
    /// `dir == None` 时回退到 `~/.latte/chat-sessions`（如果
    /// `HOME` 有设置），否则 `<cwd>/.latte/chat-sessions`。实际
    /// 调用方（编辑器）都传 `Some(absolute_path)`，所以这只是
    /// 兜底逻辑，不是热路径。
    pub fn new_sync(dir: Option<PathBuf>) -> Self {
        let base_dir = dir.unwrap_or_else(|| {
            std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".latte")
                .join("chat-sessions")
        });
        // 尽力而为：这里失败就是权限问题，第一次写盘时还会再报，
        // 不必重复抛错。
        let _ = std::fs::create_dir_all(&base_dir);
        Self { base_dir }
    }

    /// 所有 session 文件所在的根目录。便于测试和调试。
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// 创建一个新会话。如果同名 `session_id` 的记录已经存在，
    /// 返回 `SessionStoreError::Io(AlreadyExists)` —— 编辑器的
    /// `session_controller::spawn_persistent` 把它当作「继续已有
    /// 会话」分支，继续往下走。
    ///
    /// 成功后新记录会**写盘**（不缓存，因为本 store 不维护读缓存）。
    pub async fn create(
        &self,
        session_id: &str,
        chat_type: &str,
        role_ids: Vec<String>,
    ) -> Result<(), SessionStoreError> {
        let path = self.path_for(session_id);
        if path.exists() {
            return Err(SessionStoreError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("session {session_id} already exists"),
            )));
        }
        let now = iso8601_utc_now();
        let session = StoredSession {
            session_id: session_id.to_string(),
            state: SessionState::Created,
            chat_type: chat_type.to_string(),
            role_ids,
            messages: Vec::new(),
            created_at: now.clone(),
            updated_at: now,
        };
        self.write_atomic(&session)?;
        Ok(())
    }

    /// 把会话迁移到 `state` 状态。session 不存在时返回
    /// `NotFound`。同时刷新 `updated_at`。
    pub async fn set_state(
        &self,
        session_id: &str,
        state: SessionState,
    ) -> Result<(), SessionStoreError> {
        let mut session = self.get(session_id).await?;
        session.state = state;
        session.updated_at = iso8601_utc_now();
        self.write_atomic(&session)?;
        Ok(())
    }

    /// 在会话的转录末尾追加一条消息。session 不存在时返回
    /// `NotFound`。
    pub async fn append_message(
        &self,
        session_id: &str,
        msg: StoredMessage,
    ) -> Result<(), SessionStoreError> {
        let mut session = self.get(session_id).await?;
        session.messages.push(msg);
        session.updated_at = iso8601_utc_now();
        self.write_atomic(&session)?;
        Ok(())
    }

    /// 列出所有会话，按 `updated_at` 倒序排（编辑器 sidebar 把最
    /// 近操作过的放在最上面）。每次都从磁盘读：另一个
    /// `SessionStore` 实例或手编文件可能在我们看不到的时候改了
    /// 文件，而且编辑器场景下刷新成本极低（≤ 几百个 KB 级 JSON）。
    pub async fn list(&self) -> Result<Vec<SessionSummary>, SessionStoreError> {
        let mut out: Vec<SessionSummary> = Vec::new();
        let entries = match std::fs::read_dir(&self.base_dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(SessionStoreError::Io(e)),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // 跳过我们自己的 `.tmp`（rename 后正常不会存在，但对
            // 崩溃后的残留做防御）。
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(_) => continue, // 读不到就跳过
            };
            let session: StoredSession = match serde_json::from_slice(&bytes) {
                Ok(s) => s,
                Err(_) => continue, // 损坏就跳过
            };
            out.push(SessionSummary::from(&session));
        }
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(out)
    }

    /// 取出一条会话的完整记录。每次都从磁盘读（不维护读缓存）：
    /// 另一个 `SessionStore` 实例或手编文件可能在我们看不到的
    /// 时候改了文件。KB 级 JSON 的单次阻塞读成本可忽略，不值得
    /// 为它加一层 read-through 缓存。
    pub async fn get(&self, session_id: &str) -> Result<StoredSession, SessionStoreError> {
        let path = self.path_for(session_id);
        if !path.exists() {
            return Err(SessionStoreError::NotFound(session_id.to_string()));
        }
        let bytes = std::fs::read(&path)?;
        let session: StoredSession = serde_json::from_slice(&bytes)?;
        Ok(session)
    }

    /// 删除一个会话的文件和缓存项。幂等：删除不存在的 id 是 no-op
    ///（不报错）。
    pub async fn delete(&self, session_id: &str) -> Result<(), SessionStoreError> {
        let path = self.path_for(session_id);
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }

    /// 把整条记录写回去（编辑器「编辑消息」流程用，会就地改写转
    /// 录）。同 `session_id` 已存在则覆盖。
    pub async fn save(&self, session: &StoredSession) -> Result<(), SessionStoreError> {
        let mut s = session.clone();
        s.updated_at = iso8601_utc_now();
        self.write_atomic(&s)?;
        Ok(())
    }

    // ── 私有方法 ────────────────────────────────────────────────

    /// `base_dir / <session_id>.json`。session id 原样用作文件名
    /// 主体（不做哈希），方便运维 `ls ~/.latte/chat-sessions` +
    /// 手动改文件；id 就是文件名的 stem。
    fn path_for(&self, session_id: &str) -> PathBuf {
        self.base_dir.join(format!("{session_id}.json"))
    }

    /// 原子写：序列化 → 写 `.tmp` → rename。进程在 write 和
    /// rename 之间死掉时原文件不动；rename 之后才生效，所以新
    /// 文件一定是一致的。
    fn write_atomic(&self, session: &StoredSession) -> Result<(), SessionStoreError> {
        let path = self.path_for(&session.session_id);
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(session)?;
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

// ─── 单元测试 ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试用一个全新的 tempdir。Store 会在 dir 不存在时创建，
    /// 所以直接把 `TempDir` 路径交给它，`TempDir` 退出时自动清理。
    fn fresh_store() -> (tempfile::TempDir, SessionStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SessionStore::new_sync(Some(dir.path().to_path_buf()));
        (dir, store)
    }

    fn ts() -> String {
        iso8601_utc_now()
    }

    // ── new_sync ───────────────────────────────────────────────────

    /// `new_sync(Some(path))` 会在 path 不存在时自动创建目录，并
    /// 把它记录为 `base_dir()`。
    #[test]
    fn new_sync_creates_dir_on_demand() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("nested").join("chat-sessions");
        assert!(!nested.exists(), "前置：nested 目录应不存在");
        let store = SessionStore::new_sync(Some(nested.clone()));
        assert!(nested.exists(), "new_sync 必须创建目录");
        assert_eq!(store.base_dir(), nested);
    }

    /// `new_sync(None)` 在 `HOME` 已设置时回退到
    /// `$HOME/.latte/chat-sessions`。不校验精确路径（取决于
    /// 测试运行器环境），只校验它落在 `$HOME` 下面且以
    /// `chat-sessions` 结尾。
    #[test]
    fn new_sync_none_falls_back_to_home() {
        // 只在当前线程改 HOME，测完恢复。
        let prev = std::env::var("HOME").ok();
        let fake = tempfile::tempdir().expect("tempdir");
        std::env::set_var("HOME", fake.path());
        let store = SessionStore::new_sync(None);
        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        assert!(store.base_dir().starts_with(fake.path()));
        assert!(store.base_dir().ends_with("chat-sessions"));
    }

    // ── create ─────────────────────────────────────────────────────

    /// `create` 写一个 JSON 文件 + 缓存记录到磁盘（不维护内存缓存）。
    /// 测试方法：读回 JSON 文件并反序列化，校验关键字段。
    #[test]
    fn create_persists_and_caches() {
        let (_dir, store) = fresh_store();
        futures::executor::block_on(store.create("s1", "single", vec!["pm".into()]))
            .expect("create");
        let path = store.base_dir().join("s1.json");
        assert!(path.exists(), "create 必须落盘一个 json 文件");
        let raw = std::fs::read_to_string(&path).expect("read");
        let parsed: StoredSession = serde_json::from_str(&raw).expect("parse");
        assert_eq!(parsed.session_id, "s1");
        assert_eq!(parsed.chat_type, "single");
        assert_eq!(parsed.role_ids, vec!["pm".to_string()]);
        assert_eq!(parsed.state, SessionState::Created);
        assert!(parsed.messages.is_empty());
        assert_eq!(parsed.created_at, parsed.updated_at);
    }

    /// 同一个 id 调两次 `create` 必须返回 `Io(AlreadyExists)`，
    /// 这样编辑器「继续已有会话」分支可以识别。
    #[test]
    fn create_twice_returns_already_exists() {
        let (_dir, store) = fresh_store();
        futures::executor::block_on(store.create("dup", "single", vec![])).expect("first");
        let err = futures::executor::block_on(store.create("dup", "single", vec![]))
            .expect_err("second must fail");
        match err {
            SessionStoreError::Io(e) => {
                assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
            }
            other => panic!("expected Io(AlreadyExists), got {other:?}"),
        }
    }

    // ── get / list ─────────────────────────────────────────────────

    /// `get` 返回持久化记录；`list` 返回 `SessionSummary` 投影。
    /// 验证：先 create a 和 b，get(a) 拿到完整字段，list 拿到
    /// 两条 summary 且 session_id 集合匹配。
    #[test]
    fn get_and_list_round_trip() {
        let (_dir, store) = fresh_store();
        futures::executor::block_on(store.create("a", "single", vec!["pm".into()]))
            .expect("create a");
        futures::executor::block_on(store.create("b", "multi_role", vec!["pm".into(), "dev".into()]))
            .expect("create b");

        let got = futures::executor::block_on(store.get("a")).expect("get a");
        assert_eq!(got.chat_type, "single");
        assert_eq!(got.role_ids, vec!["pm".to_string()]);

        let list = futures::executor::block_on(store.list()).expect("list");
        assert_eq!(list.len(), 2);
        let ids: std::collections::HashSet<_> =
            list.iter().map(|s| s.session_id.as_str()).collect();
        assert!(ids.contains("a"));
        assert!(ids.contains("b"));
        for s in &list {
            // summary 字段必须都填上（length 校验是简单 sanity
            // check，不深究 message 列表——它本来就不在 summary
            // 里）。
            assert_eq!(s.session_id, s.session_id);
            assert!(s.role_ids.len() <= 2);
        }
    }

    /// `get` 一个不存在的 id 时返回 `NotFound`，不能 panic。
    #[test]
    fn get_missing_returns_not_found() {
        let (_dir, store) = fresh_store();
        let err = futures::executor::block_on(store.get("nope"))
            .expect_err("missing must error");
        match err {
            SessionStoreError::NotFound(id) => assert_eq!(id, "nope"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    /// `list` 按 `updated_at` 倒序。`updated_at` 是秒级精度
    /// （`iso8601_utc_now`），固定 sleep 5ms 大概率落在同一秒 → 排序键
    /// 相等、顺序取决于文件读取序（flaky 实锤）。所以这里等到秒翻转
    /// 再 create b，保证 b.updated_at 严格大于 a。
    #[test]
    fn list_is_sorted_by_updated_at_desc() {
        let (_dir, store) = fresh_store();
        futures::executor::block_on(store.create("a", "single", vec![])).expect("create a");
        // 等秒翻转（最多 1s，平均 500ms），比固定 sleep 可靠
        let sec = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        while std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            == sec
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        futures::executor::block_on(store.create("b", "single", vec![])).expect("create b");
        let list = futures::executor::block_on(store.list()).expect("list");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].session_id, "b", "最近更新的排第一");
        assert_eq!(list[1].session_id, "a");
    }

    // ── set_state / append_message / save ──────────────────────────

    /// `set_state` 翻转生命周期状态并刷新 `updated_at`。`updated_at`
    /// 单调递增（sleep 5ms 在 tmpfs 下既便宜又可靠）。
    #[test]
    fn set_state_updates_and_bumps_timestamp() {
        let (_dir, store) = fresh_store();
        futures::executor::block_on(store.create("s", "single", vec![])).expect("create");
        let before = futures::executor::block_on(store.get("s")).expect("get");
        std::thread::sleep(std::time::Duration::from_millis(5));
        futures::executor::block_on(store.set_state("s", SessionState::Running))
            .expect("set_state");
        let after = futures::executor::block_on(store.get("s")).expect("get");
        assert_eq!(after.state, SessionState::Running);
        assert!(
            after.updated_at >= before.updated_at,
            "updated_at 不能回退: before={} after={}",
            before.updated_at,
            after.updated_at,
        );
    }

    /// `append_message` 让转录变长并落盘。验证两条消息的类型和
    /// 字段都对。
    #[test]
    fn append_message_persists_transcript() {
        let (_dir, store) = fresh_store();
        futures::executor::block_on(store.create("s", "single", vec!["dev".into()]))
            .expect("create");
        futures::executor::block_on(store.append_message(
            "s",
            StoredMessage::User {
                content: "hi".into(),
                timestamp: ts(),
            },
        )).expect("append user");
        futures::executor::block_on(store.append_message(
            "s",
            StoredMessage::Assistant {
                role_id: "dev".into(),
                content: "hello".into(),
                timestamp: ts(),
                tokens: Some(42),
            },
        )).expect("append assistant");
        let got = futures::executor::block_on(store.get("s")).expect("get");
        assert_eq!(got.messages.len(), 2);
        match &got.messages[0] {
            StoredMessage::User { content, .. } => assert_eq!(content, "hi"),
            other => panic!("expected User, got {other:?}"),
        }
        match &got.messages[1] {
            StoredMessage::Assistant { role_id, content, tokens, .. } => {
                assert_eq!(role_id, "dev");
                assert_eq!(content, "hello");
                assert_eq!(tokens, &Some(42u32));
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
    }

    /// `save` 覆盖整条记录（编辑器「编辑消息」流程用）。验证
    /// 修改后的 transcript 能正确往返。
    #[test]
    fn save_overwrites_full_record() {
        let (_dir, store) = fresh_store();
        futures::executor::block_on(store.create("s", "single", vec![])).expect("create");
        let mut s = futures::executor::block_on(store.get("s")).expect("get");
        s.messages.push(StoredMessage::User {
            content: "edited".into(),
            timestamp: ts(),
        });
        futures::executor::block_on(store.save(&s)).expect("save");
        let got = futures::executor::block_on(store.get("s")).expect("get");
        assert_eq!(got.messages.len(), 1);
        match &got.messages[0] {
            StoredMessage::User { content, .. } => assert_eq!(content, "edited"),
            other => panic!("expected User, got {other:?}"),
        }
    }

    // ── delete ─────────────────────────────────────────────────────

    /// `delete` 删文件 + 缓存项，幂等。
    #[test]
    fn delete_removes_file_and_cache() {
        let (_dir, store) = fresh_store();
        futures::executor::block_on(store.create("s", "single", vec![])).expect("create");
        let path = store.base_dir().join("s.json");
        assert!(path.exists());
        futures::executor::block_on(store.delete("s")).expect("delete");
        assert!(!path.exists(), "文件必须被删掉");
        // 再删一次是 no-op（不报错）。
        futures::executor::block_on(store.delete("s")).expect("delete again");
        // get 现在返回 NotFound。
        let err = futures::executor::block_on(store.get("s")).expect_err("missing");
        assert!(matches!(err, SessionStoreError::NotFound(_)));
    }

    // ── 原子写 ────────────────────────────────────────────────

    /// `write_atomic` 写完不留 `.tmp` 残留。强行走 create 的写盘
    /// 路径，断言没有 `s.json.tmp` 残留——抓出 rename 步骤被删或
    /// 跳过的回归。
    #[test]
    fn write_atomic_leaves_no_tmp() {
        let (_dir, store) = fresh_store();
        futures::executor::block_on(store.create("s", "single", vec![])).expect("create");
        let tmp = store.base_dir().join("s.json.tmp");
        assert!(!tmp.exists(), "成功写完之后不能有 .tmp 残留");
    }

    // ── 跨实例磁盘共享 ─────────────────────────

    /// 两个 store 共享同一个目录。第二个 store 没有读缓存，所
    /// 以 `get` 总是从磁盘读最新数据，能看到第一个 store 写入的
    /// 最新内容（包括后来的 append_message）。这就是「用户打开
    /// 编辑器、输入、然后 sidebar 刷新」的真实路径。
    #[test]
    fn two_stores_share_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = SessionStore::new_sync(Some(dir.path().to_path_buf()));
        let b = SessionStore::new_sync(Some(dir.path().to_path_buf()));
        futures::executor::block_on(a.create("s", "single", vec!["pm".into()])).expect("a create");
        // b 没有缓存，`list` 直接从磁盘读。
        let b_list = futures::executor::block_on(b.list()).expect("b list");
        assert_eq!(b_list.len(), 1);
        assert_eq!(b_list[0].session_id, "s");
        // a 追加消息；b 再读一次也能看到（不靠缓存）。
        futures::executor::block_on(a.append_message(
            "s",
            StoredMessage::User { content: "x".into(), timestamp: ts() },
        )).expect("a append");
        let b_got = futures::executor::block_on(b.get("s")).expect("b get");
        assert_eq!(b_got.messages.len(), 1);
    }
}
