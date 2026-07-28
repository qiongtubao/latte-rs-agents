//! Server-side store for per-task subsession event logs.
//!
//! Each `DelegateStarted/Finished` carries a `sub_id` that points back
//! to one of these. The HTTP API exposes them via
//! `GET /api/subsessions?id=<sub_id>`.
//!
//! 存储策略（两层）：
//!   1. **内存（`MemorySink`）**：进程内 `Mutex<Vec<((SessionId, SubId), Entry)>>`，
//!      `MAX_AGE = 1h` GC。保证活的 subagent 实时回读零 IO。
//!   2. **磁盘（`subsession_disk::SubsessionDiskSink`）**：可选。开启后
//!      每个 subagent 的事件流同时落到
//!      `<ui-sessions>/<session_id>/<sub_id>.jsonl`，主 session 删除时联删。
//!      目的：UI server 重启后仍能查历史 subagent 过程（排查场景）。
//!
//! 磁盘与内存生命周期分离：
//!   - 内存 1h 自动 GC（不让 RSS 涨）。
//!   - 磁盘不自动按时间清理——主 session 删除时联删；否则保留。
//!
//! 文件名 `<sub_id>.jsonl`：`sub_id` 已含 `role-micros`，UI server
//! 单进程内唯一；跨进程撞名概率极低（约同微秒内同角色名），后果是
//! 后写覆盖前写（接受——这是排查辅助，不是审计）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};

use crate::subsession_disk::SubsessionDiskSink;
use crate::trace::{FanoutSink, MemorySink, TraceEvent, TraceSink};

/// How long a subsession's events are kept in memory before GC reclaims
/// them. Long enough that the user can copy/paste from the viewer
/// before the entry disappears; short enough that a long-running
/// server doesn't OOM. 1 hour matches typical debugging flow.
///
/// Note: only the in-memory cache is GC'd. The on-disk file (if
/// persistence is enabled) is removed only when the parent session
/// is deleted via `delete_for_session`.
pub const MAX_AGE: Duration = Duration::from_secs(60 * 60);

/// Compound key: one tab's worth of subsessions.
pub type SessionId = String;
pub type SubId = String;

/// Process-wide subsession store.
///
/// 默认（`new()`）无磁盘；UI server 启动时用 `with_persistence()` 打开
/// 落盘。两层共索引：内存 `inner` 活读；磁盘靠 `sub_to_session` 索引
/// 由 `with_persistence` 启动时一次扫盘构建，避免每次 `read_persisted`
/// 全目录 grep。
pub struct SubsessionStore {
    /// (session, sub) → memory entry.
    inner: Mutex<Vec<((SessionId, SubId), Entry)>>,
    /// Optional disk persistence: base dir + 启动时构建的 sub→session 索引。
    persistence: Option<Persistence>,
}

struct Persistence {
    /// `<cwd>/.latte/ui-sessions` —— 主 session 与 subagent 共用此目录。
    /// subagent 文件落在 `<base>/<session_id>/<sub_id>.jsonl`。
    base_dir: PathBuf,
    /// 启动时从磁盘读 meta 构建；运行时 `create()` 同步插入。
    sub_to_session: RwLock<HashMap<SubId, SessionId>>,
}

struct Entry {
    /// 实时读源。clone 给 runner，跑完后 UI 读 snapshot。
    sink: Arc<MemorySink>,
    /// 磁盘镜像（若持久化启用且 try_new 成功）。drop 时自动 flush。
    /// 不直接给 runner —— runner 通过 `create()` 返回的 FanoutSink
    /// 收到事件，fanout 内部再调到这里。
    _disk_sink: Option<Arc<SubsessionDiskSink>>,
    created_at: Instant,
}

impl Default for SubsessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SubsessionStore {
    /// In-memory only. Used by tests and any caller that doesn't want
    /// disk persistence.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Vec::new()),
            persistence: None,
        }
    }

    /// 带磁盘落盘的构造。`base_dir` 通常是 `<cwd>/.latte/ui-sessions`。
    ///
    /// 启动时：
    ///   1. `mkdir -p base_dir`（失败就 warn + 退化成纯内存，不 panic）。
    ///   2. 扫一遍 `<base>/<sid>/<sub_id>.jsonl`，从 meta 行提取
    ///      `sub_id → session_id` 索引（供 `read_persisted` 用）。
    ///
    /// 行为：
    ///   - 索引只用来从 sub_id 反查 session_id（O(1)），不把事件载入内存。
    ///   - 不清旧文件（按"和主 session 生命周期联动"约定，删主 session
    ///     时联删；启动时不动——主 session 文件 `restore_sessions` 阶段
    ///     会载入，被它们 orphan 的 subagent 目录以后清理时一并处理）。
    pub fn with_persistence(base_dir: PathBuf) -> Self {
        let persistence = match Self::init_persistence(&base_dir) {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!(
                    "[subsession] disk persistence disabled (base={}): {e}",
                    base_dir.display()
                );
                None
            }
        };
        Self {
            inner: Mutex::new(Vec::new()),
            persistence,
        }
    }

    fn init_persistence(base_dir: &Path) -> Result<Persistence, String> {
        std::fs::create_dir_all(base_dir)
            .map_err(|e| format!("mkdir {}: {e}", base_dir.display()))?;
        let sub_to_session = scan_subagent_index(base_dir)?;
        Ok(Persistence {
            base_dir: base_dir.to_path_buf(),
            sub_to_session: RwLock::new(sub_to_session),
        })
    }

    /// 分配一条新 subsession。返回：
    ///   - `SubId`：给主聊天 `DelegateStarted.sub_id` 用
    ///   - `Arc<dyn TraceSink>`：给 runner 调 `emit`
    ///
    /// 有持久化时 sink 内部 fanout 到 `MemorySink` + `DiskSink`；
    /// 磁盘不可写时降级到纯内存（`create()` 本身不失败，runner 看不出）。
    pub fn create(
        &self,
        session_id: &str,
        role_name: &str,
    ) -> (SubId, Arc<dyn TraceSink>) {
        let sub_id = format!(
            "{}-{}",
            role_name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_micros())
                .unwrap_or(0)
        );

        let mem = Arc::new(MemorySink::new());
        let now = Instant::now();

        // 尝试挂载磁盘 sink。失败就只返内存 sink（caller 不知道）。
        let disk_sink = self.persistence.as_ref().and_then(|p| {
            match SubsessionDiskSink::try_new(&p.base_dir, session_id, &sub_id, role_name) {
                Ok(d) => {
                    p.sub_to_session
                        .write()
                        .insert(sub_id.clone(), session_id.to_string());
                    Some(Arc::new(d))
                }
                Err(e) => {
                    eprintln!(
                        "[subsession] disk sink for {sub_id} (session {session_id}) failed: {e}; \
                         memory-only this subsession"
                    );
                    None
                }
            }
        });

        // fanout 顺序：先内存后磁盘。内存必须成功（OOM 才挂），磁盘是 best-effort。
        let mut sinks: Vec<Arc<dyn TraceSink>> = vec![mem.clone() as Arc<dyn TraceSink>];
        if let Some(d) = disk_sink.as_ref() {
            sinks.push(d.clone() as Arc<dyn TraceSink>);
        }
        let fanout = Arc::new(FanoutSink::new(sinks));

        self.inner.lock().push((
            (session_id.to_string(), sub_id.clone()),
            Entry {
                sink: mem,
                _disk_sink: disk_sink,
                created_at: now,
            },
        ));

        (sub_id, fanout)
    }

    /// 复用已有 subsession 或新建。用于主角色 runner（manager 等）--
    /// `/role switch` 重建 runner 时不会每次都新建空文件，而是复用
    /// 同一角色的已有 subsession（如果还在内存 1h 窗口内）。
    ///
    /// delegate 路径继续用 [`create`]（每次新建，因为每次委派是独立
    /// 的子会话）。
    pub fn get_or_create(
        &self,
        session_id: &str,
        role_name: &str,
    ) -> (SubId, Arc<dyn TraceSink>) {
        self.sweep();
        let key_prefix = format!("{role_name}-");
        let mut g = self.inner.lock();
        for ((sid, sub_id), entry) in g.iter().rev() {
            if sid == session_id && sub_id.starts_with(&key_prefix) {
                let mut sinks: Vec<Arc<dyn TraceSink>> =
                    vec![entry.sink.clone() as Arc<dyn TraceSink>];
                if let Some(d) = entry._disk_sink.as_ref() {
                    sinks.push(d.clone() as Arc<dyn TraceSink>);
                }
                let fanout = Arc::new(FanoutSink::new(sinks));
                return (sub_id.clone(), fanout);
            }
        }
        drop(g);
        self.create(session_id, role_name)
    }

    /// 内存里的实时快照。None = 已被 GC 或从未存在。
    pub fn snapshot(&self, session_id: &str, sub_id: &str) -> Option<Vec<TraceEvent>> {
        self.sweep();
        let key = (session_id.to_string(), sub_id.to_string());
        let mut g = self.inner.lock();
        g.iter_mut()
            .find(|(k, _)| k == &key)
            .map(|(_, e)| e.sink.snapshot())
    }

    /// 按 `sub_id` 查（忽略 session 组件）。`sub_id` 全局唯一（role +
    /// unix micros），且 web UI 不知道 session_id，所以 HTTP 层走这
    /// 个。优先看内存。
    pub fn snapshot_any(&self, sub_id: &str) -> Option<Vec<TraceEvent>> {
        self.sweep();
        let mut g = self.inner.lock();
        g.iter_mut()
            .find(|((_, sid), _)| sid == sub_id)
            .map(|(_, e)| e.sink.snapshot())
    }

    /// 磁盘回查。内存 miss 后调用。读 meta + 事件行，转 `Vec<TraceEvent>`。
    /// 文件不存在 / meta 缺 sub_id / 事件反序列化失败 → None。
    pub fn read_persisted(&self, sub_id: &str) -> Option<Vec<TraceEvent>> {
        let persistence = self.persistence.as_ref()?;
        let sid = persistence
            .sub_to_session
            .read()
            .get(sub_id)
            .cloned()?;
        let path = persistence.base_dir.join(&sid).join(format!("{sub_id}.jsonl"));
        let raw = std::fs::read_to_string(&path).ok()?;
        parse_persisted_file(&raw)
    }

    /// 删主 session 时联调：移除 `<base>/<sid>/` 整个目录 + 内存
    /// 里属于该 sid 的所有 entries + 索引。
    ///
    /// 返回删除的 subagent 文件数（不含目录本身；目录随文件一起 rm）。
    /// 目录不存在时返回 0（幂等）。
    pub fn delete_for_session(&self, session_id: &str) -> usize {
        // 1) 内存：移除属于此 sid 的所有 entries（含 `_disk_sink`，drop 时
        //    会 flush+close 文件句柄，但文件我们接下来就 rm 掉了——`rm`
        //    在 unixes 上对打开的文件也 OK，文件 inode 延后释放）。
        {
            let mut g = self.inner.lock();
            g.retain(|((sid, _), _)| sid != session_id);
        }

        // 2) 索引：清掉属于此 sid 的 sub_id。
        let removed_sub_ids: Vec<SubId> = match self.persistence.as_ref() {
            Some(p) => {
                let mut idx = p.sub_to_session.write();
                let mut to_remove = Vec::new();
                for (k, v) in idx.iter() {
                    if v == session_id {
                        to_remove.push(k.clone());
                    }
                }
                for k in &to_remove {
                    idx.remove(k);
                }
                to_remove
            }
            None => Vec::new(),
        };

        // 3) 磁盘：rm 整个子目录。rm 之后即便 sub_id 文件句柄还开着
        //    也无影响（unlink 立即生效，下次 fd close 释放 inode）。
        let Some(persistence) = self.persistence.as_ref() else {
            return 0;
        };
        let session_dir = persistence.base_dir.join(session_id);
        let count = removed_sub_ids.len();
        match std::fs::remove_dir_all(&session_dir) {
            Ok(()) => count,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => {
                eprintln!(
                    "[subsession] delete_for_session({}): rm {}: {e}",
                    session_id,
                    session_dir.display()
                );
                0
            }
        }
    }

    /// Lazy sweep: drop in-memory entries older than `MAX_AGE`. Called
    /// before each `snapshot`; O(n) but n is bounded by user activity.
    fn sweep(&self) {
        let now = Instant::now();
        let mut g = self.inner.lock();
        g.retain(|(_, entry)| now.duration_since(entry.created_at) < MAX_AGE);
    }

    /// 内存活 entry 数。tests / debug 用。
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    /// 索引里 subagent 数（含磁盘已落盘、内存已 GC 的）。debug 用。
    pub fn persisted_index_size(&self) -> usize {
        self.persistence
            .as_ref()
            .map(|p| p.sub_to_session.read().len())
            .unwrap_or(0)
    }
}

// ─── helpers ──────────────────────────────────────────────────────

/// 启动时扫一遍 `<base>/<sid>/<sub_id>.jsonl`，从每条 meta 行抽
/// `sub_id → session_id` 索引。读失败 / meta 缺字段 → 跳过该文件。
fn scan_subagent_index(base_dir: &Path) -> Result<HashMap<SubId, SessionId>, String> {
    let mut idx = HashMap::new();
    let session_dirs = match std::fs::read_dir(base_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(idx),
        Err(e) => return Err(format!("read_dir {}: {e}", base_dir.display())),
    };
    for entry in session_dirs.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue; // 跳过主 session 的 `<sid>.jsonl` 文件
        }
        let files = match std::fs::read_dir(&path) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for f in files.flatten() {
            let fp = f.path();
            if fp.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some((sub_id, sid)) = read_meta_subid_sid(&fp) {
                idx.insert(sub_id, sid);
            }
        }
    }
    Ok(idx)
}

fn read_meta_subid_sid(path: &Path) -> Option<(SubId, SessionId)> {
    let raw = std::fs::read_to_string(path).ok()?;
    let first = raw.lines().next()?;
    let v: serde_json::Value = serde_json::from_str(first).ok()?;
        if v.get("type").and_then(|t| t.as_str()) != Some(crate::subsession_disk::META_TYPE) {
        return None;
    }
    let sub_id = v.get("sub_id")?.as_str()?.to_string();
    let sid = v.get("session_id")?.as_str()?.to_string();
    Some((sub_id, sid))
}

/// 解析一个 persisted subagent jsonl：meta 行（必须）+ 事件行。
/// 任何解析失败的事件单独跳过（不污染整文件返回）。
fn parse_persisted_file(raw: &str) -> Option<Vec<TraceEvent>> {
    let mut out = Vec::new();
    for line in raw.lines() {
        if line.is_empty() {
            continue;
        }
        // 跳过 meta 行（与 ui-sessions 同形）。
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if v.get("type").and_then(|t| t.as_str()) == Some(crate::subsession_disk::META_TYPE) {
                continue;
            }
        }
        match serde_json::from_str::<TraceEvent>(line) {
            Ok(ev) => out.push(ev),
            Err(_) => continue, // 单条坏行不致命：跳过
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

// ─── tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::{ToolStatus, TraceMeta, TraceSink};
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    fn tool_exec_ok(role: &str) -> TraceEvent {
        TraceEvent::ToolExec {
            meta: TraceMeta::now(SEQ.fetch_add(1, Ordering::Relaxed), role, "test"),
            name: "file.read".into(),
            args_json: "{}".into(),
            latency_ms: 3,
            status: ToolStatus::Ok("ok".into()),
        }
    }

    // ── 内存路径（保持旧行为） ───────────────────────────────────

    #[test]
    fn snapshot_any_finds_by_sub_id_regardless_of_session_key() {
        // Regression: 兼容旧 web UI（只传 sub_id 不传 session_id）。
        let store = SubsessionStore::new();
        let (sub_id, sink) = store.create("/workspace/cwd/path", "programmer");
        sink.emit(tool_exec_ok("programmer"));

        assert!(store.snapshot("/workspace/cwd/path", &sub_id).is_some());
        assert!(store.snapshot("default", &sub_id).is_none());
        let events = store.snapshot_any(&sub_id).expect("snapshot_any");
        assert_eq!(events.len(), 1);
        assert!(store.snapshot_any("no-such-sub").is_none());
    }

    // ── 磁盘落盘 + 回读 ─────────────────────────────────────────

    #[test]
    fn create_writes_meta_and_events_under_session_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("ui-sessions");
        let store = SubsessionStore::with_persistence(base.clone());

        let (sub_id, sink) = store.create("sid-A", "programmer");
        sink.emit(tool_exec_ok("programmer"));
        sink.emit(tool_exec_ok("programmer"));

        let file = base.join("sid-A").join(format!("{sub_id}.jsonl"));
        assert!(file.exists(), "file {} should exist", file.display());

        let raw = std::fs::read_to_string(&file).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 3, "1 meta + 2 events: {raw}");

        // meta 行
        let meta: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(meta["type"], crate::subsession_disk::META_TYPE);
        assert_eq!(meta["sub_id"], sub_id);
        assert_eq!(meta["session_id"], "sid-A");
        assert_eq!(meta["role_name"], "programmer");
        assert!(meta["created_at_unix_ms"].is_u64());
    }

    #[test]
    fn read_persisted_round_trip_after_fresh_store() {
        // 1) 落盘
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("ui-sessions");
        let (sub_id, sink) = {
            let store = SubsessionStore::with_persistence(base.clone());
            let (sid, s) = store.create("sid-B", "reviewer");
            s.emit(tool_exec_ok("reviewer"));
            s.emit(tool_exec_ok("reviewer"));
            s.emit(tool_exec_ok("reviewer"));
            (sid, s)
        };
        // 内存 1h 内，snapshot_any 仍可见
        // （同 process 内 store 没 drop，所以这条本来就 in-memory）

        // 2) 模拟重启：drop 旧 store，开新的（同一 base_dir）
        drop(sink);
        // tmp 目录还在；同 base_dir 重新开 store
        let store2 = SubsessionStore::with_persistence(base.clone());

        // 3) 内存空，但磁盘回查应得 3 条
        assert_eq!(store2.len(), 0);
        let events = store2
            .read_persisted(&sub_id)
            .expect("persisted events should be readable after restart");
        assert_eq!(events.len(), 3);

        // 4) 索引已建
        assert_eq!(store2.persisted_index_size(), 1);
    }

    #[test]
    fn delete_for_session_removes_dir_and_index() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("ui-sessions");
        let store = SubsessionStore::with_persistence(base.clone());

        // 两个 session，每个各一个 subagent
        let (sub_a, sink_a) = {
            let s = store.create("sid-A", "programmer");
            s.1.emit(tool_exec_ok("programmer"));
            s
        };
        let (sub_b, sink_b) = {
            let s = store.create("sid-B", "reviewer");
            s.1.emit(tool_exec_ok("reviewer"));
            s
        };
        drop(sink_a);
        drop(sink_b);

        // 两个目录都存在
        assert!(base.join("sid-A").exists());
        assert!(base.join("sid-B").exists());

        // 删 sid-A
        let removed = store.delete_for_session("sid-A");
        assert_eq!(removed, 1, "exactly 1 subagent file under sid-A");

        // sid-A 整目录没了
        assert!(!base.join("sid-A").exists(), "sid-A dir should be gone");
        // sid-B 还在
        assert!(base.join("sid-B").exists(), "sid-B dir should remain");
        let file_b = base.join("sid-B").join(format!("{sub_b}.jsonl"));
        assert!(file_b.exists());

        // 索引里 sid-A 的 sub_id 没了
        assert!(store.read_persisted(&sub_a).is_none());
        // sid-B 还能读
        assert!(store.read_persisted(&sub_b).is_some());
        // 内存里 sid-A 的 entry 也没了
        assert!(store.snapshot("sid-A", &sub_a).is_none());
        // len 应只含 sid-B 的活 entry
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn delete_for_session_unknown_sid_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("ui-sessions");
        let store = SubsessionStore::with_persistence(base.clone());

        // 没有任何 entry 时调
        assert_eq!(store.delete_for_session("nonexistent"), 0);
        // 无 persistence 时调（new()）
        let in_mem = SubsessionStore::new();
        assert_eq!(in_mem.delete_for_session("anything"), 0);
    }

    #[test]
    fn disk_failure_falls_back_to_memory() {
        // base 是文件而非目录：scan_subagent_index 会读 base 失败，
        // init_persistence 失败，store 自动降级到 in-memory。
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("not-a-dir");
        std::fs::write(&base, "blocking file").unwrap();
        let store = SubsessionStore::with_persistence(base);

        // store 应退化到 in-memory，不 panic
        let (sub_id, sink) = store.create("sid-X", "tester");
        sink.emit(tool_exec_ok("tester"));
        let events = store.snapshot_any(&sub_id).expect("in-memory works");
        assert_eq!(events.len(), 1);
        assert_eq!(store.persisted_index_size(), 0, "no disk = no index");
    }

    #[test]
    fn unsafe_session_id_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("ui-sessions");
        let store = SubsessionStore::with_persistence(base.clone());

        // 路径穿越 / 嵌入分隔符：应被 disk sink 拒绝
        let (_sub, sink) = store.create("../escape", "tester");
        sink.emit(tool_exec_ok("tester"));
        // 内存正常
        assert_eq!(store.len(), 1);
        // 磁盘没建 escape 目录
        assert!(!base.join("../escape").exists());
        // 索引也没建
        assert_eq!(store.persisted_index_size(), 0);
    }

    #[test]
    fn with_persistence_empty_base_yields_empty_index() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("ui-sessions");
        // base 不存在，构造时 create_dir_all 应创建空目录
        let store = SubsessionStore::with_persistence(base.clone());
        assert!(base.is_dir());
        assert_eq!(store.persisted_index_size(), 0);
    }

    #[test]
    fn with_persistence_skips_main_session_files() {
        // 主 session 文件 `<base>/<sid>.jsonl` 也在 base 下；scan
        // 不应把它当 subagent 收录。
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("ui-sessions");
        std::fs::create_dir_all(&base).unwrap();
        // 模拟主 session jsonl（无 meta sub_id）—— 故意不带
        // 正确 meta 字段，让 scan 的容错逻辑有机会跳过它。
        std::fs::write(
            base.join("sid-A.jsonl"),
            "{\"type\":\"__meta__\",\"session_id\":\"sid-A\"}\n{\"UserMessage\":{}}\n",
        )
        .unwrap();

        let store = SubsessionStore::with_persistence(base.clone());
        // 用真实 create() 落盘一个 subagent 文件
        let (sub_id, sink) = store.create("sid-A", "programmer");
        sink.emit(tool_exec_ok("programmer"));
        drop(sink);

        // 只收 subagent，不收主 session 文件
        assert_eq!(store.persisted_index_size(), 1);
        assert!(store.read_persisted(&sub_id).is_some());
        // 主 session 文件本身未被 scan 当成 subagent 收录
        // （它的 meta 也没 sub_id 字段，read_meta_subid_sid 会返回 None）
    }
}
