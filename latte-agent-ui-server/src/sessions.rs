//! Per-tab session 状态 + 落盘持久化。
//!
//! 每个浏览器 tab 一个 `ChatController`，互不串事件；event_log 环形
//! 缓冲（切 tab 回来时的 replay）。
//!
//! 新 session 先暂存到 `.latte/tmp/<session_id>.jsonl`，首个 ChatEvent
//! 到达时 promote 到 `<cwd>/.latte/ui-sessions/<session_id>.jsonl`。
//! 不发消息的空 session 永不落盘。重启时自动清空 `.latte/tmp/`。
//!
//! 落盘文件格式（`.latte/ui-sessions/<session_id>.jsonl`）：
//!   - 首行 meta：`{"type":"__meta__","session_id",...,"label",...,
//!     "initial_role",...,"created_at_unix_ms",...}`；改 label 时尾加
//!     一条新的 meta 行（恢复时最后一条 meta 生效，选简单正确的方案）。
//!   - 之后每行一个 ChatEvent 的前端 JSON（与 event_log 里的字符串
//!     逐字一致，恢复后 history 输出逐字节相同）。
//!   - 文件 >5000 行或 >2MB 时 compaction：重写为 meta + 内存中
//!     event_log 的尾部窗口（≤5000 条）。
//!   - `UiBackend::new` 扫描 `ui-sessions/` 目录恢复元数据 + event_log
//!     （list/get/history 立即可用），**不** spawn controller；首个
//!     chat_send / subscribe 落到恢复 session 时懒 spawn（agent 上下文
//!     从空开始，本轮只要显示连续性，handle 上 `restored: true` 标记）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use latte_agent_core::advisor_monitor::{
    AdvisorMonitor, AdvisorMonitorConfig, AdvisorReviewEngine,
};
use latte_agent_core::config::AgentConfig;
use latte_agent_core::controller::{ChatController, ChatEvent, ControllerConfig, FailedWorkflow};
use latte_agent_core::event_json::chat_event_to_frontend_json;
use latte_agent_core::model_resolver::{ModelResolver, ModelTier};
use latte_ai::params::GenerateParams;

/// event_log 环形上限（也是落盘 compaction 的行数阈值）。
const MAX_LOG: usize = 5000;
/// 落盘文件字节阈值：超过即 compaction（与行数阈值先到先触发）。
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// meta 行的 type 判别字段（与 ChatEvent 前端 JSON 的 type 不冲突）。
const META_TYPE: &str = "__meta__";

// ─── 落盘 ─────────────────────────────────────────────────────────

/// 一个 session 的落盘状态。全部文件写操作都走这个结构（archiver
/// 追加事件 / set_label 追加 meta / compaction 重写），避免多处写
/// 不一致。行数/字节数是近似计数（仅做 compaction 阈值，不精确）。
#[derive(Debug)]
pub(crate) struct SessionPersist {
    /// 当前文件路径。创建时指向 `.latte/tmp/<id>.jsonl`，
    /// promote 后指向 `.latte/ui-sessions/<id>.jsonl`。
    path: PathBuf,
    /// 最终目标路径（ui-sessions 下的固定位置）。
    final_path: PathBuf,
    session_id: String,
    initial_role: String,
    /// 原始创建时刻（unix ms 墙钟；恢复时用它近似 created_at /
    /// last_activity 的 Instant）。
    created_at_unix_ms: u64,
    /// 最近一次 label（set_label 更新；compaction 重写 meta 时用，
    /// 避免 archiver 绕回 handle 拿锁）。
    last_label: Option<String>,
    lines: u64,
    bytes: u64,
}

impl SessionPersist {
    pub(crate) fn dir_for(cwd: &Path) -> PathBuf {
        cwd.join(".latte").join("ui-sessions")
    }

    fn tmp_dir(cwd: &Path) -> PathBuf {
        cwd.join(".latte").join("tmp")
    }

    fn path_for(cwd: &Path, session_id: &str) -> PathBuf {
        Self::dir_for(cwd).join(format!("{session_id}.jsonl"))
    }

    fn tmp_path_for(cwd: &Path, session_id: &str) -> PathBuf {
        Self::tmp_dir(cwd).join(format!("{session_id}.jsonl"))
    }

    fn meta_line(&self) -> String {
        serde_json::json!({
            "type": META_TYPE,
            "session_id": self.session_id,
            "label": self.last_label,
            "initial_role": self.initial_role,
            "created_at_unix_ms": self.created_at_unix_ms,
        })
        .to_string()
    }

    /// 新建 session：存在 `.latte/tmp/` 目录下，不碰 ui-sessions。
    /// 首个事件到达时由 [`append_event`] 做 promote 到 ui-sessions。
    fn create(cwd: &Path, session_id: &str, initial_role: &str) -> std::io::Result<Self> {
        Ok(Self {
            path: Self::tmp_path_for(cwd, session_id),
            final_path: Self::path_for(cwd, session_id),
            session_id: session_id.to_string(),
            initial_role: initial_role.to_string(),
            created_at_unix_ms: crate::unix_ts_millis(),
            last_label: None,
            lines: 0,
            bytes: 0,
        })
    }

    /// 追加一行到文件。首次追加时在 `.latte/tmp/` 创建文件；
    /// 写完后 promote 到 `.latte/ui-sessions/`（无事不移），确保
    /// 不发消息的空 session 不产生痕迹。
    fn append_raw(&mut self, line: &str) -> std::io::Result<()> {
        if !self.path.exists() {
            std::fs::create_dir_all(self.path.parent().expect("dir_for 确保有父目录"))?;
            let mut f = std::fs::File::create(&self.path)?;
            use std::io::Write;
            let meta = self.meta_line();
            f.write_all(meta.as_bytes())?;
            f.write_all(b"\n")?;
            f.write_all(line.as_bytes())?;
            f.write_all(b"\n")?;
            // ── promote：移到 ui-sessions/ ──
            std::fs::create_dir_all(self.final_path.parent().expect("dir_for 确保有父目录"))?;
            std::fs::rename(&self.path, &self.final_path)?;
            self.path = self.final_path.clone();
            self.lines = 2;
            self.bytes = (meta.len() + 1 + line.len() + 1) as u64;
            Ok(())
        } else {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&self.path)?;
            f.write_all(line.as_bytes())?;
            f.write_all(b"\n")?;
            self.lines += 1;
            self.bytes += line.len() as u64 + 1;
            Ok(())
        }
    }

    /// archiver 每归档一个事件调一次：追加 + 按需 compaction。
    /// 失败只告警不打断聊天（持久化是锦上添花）。
    fn append_event(
        &mut self,
        json_line: &str,
        event_log: &parking_lot::RwLock<Vec<String>>,
    ) {
        if let Err(e) = self.append_raw(json_line) {
            eprintln!("[ui-sessions] append {}: {e}", self.path.display());
            return;
        }
        if self.lines as usize > MAX_LOG || self.bytes > MAX_FILE_BYTES {
            if let Err(e) = self.compact(event_log) {
                eprintln!("[ui-sessions] compact {}: {e}", self.path.display());
            }
        }
    }

    /// 改 label：更新缓存；文件已落盘时尾加一条 meta 覆盖行（恢复时
    /// 最后一条 meta 生效）。文件尚未落盘时只更新内存中的
    /// `last_label`——首个事件会带着正确 label 写 meta。
    fn set_label(&mut self, label: &Option<String>) {
        self.last_label = label.clone();
        if self.path.exists() {
            let line = self.meta_line();
            if let Err(e) = self.append_raw(&line) {
                eprintln!("[ui-sessions] write label {}: {e}", self.path.display());
            }
        }
    }

    /// compaction：重写为 meta + event_log 当前内容（尾部窗口 ≤5000）。
    fn compact(
        &mut self,
        event_log: &parking_lot::RwLock<Vec<String>>,
    ) -> std::io::Result<()> {
        let events = event_log.read().clone();
        let mut out = String::with_capacity(self.bytes.min(MAX_FILE_BYTES) as usize);
        out.push_str(&self.meta_line());
        out.push('\n');
        for line in &events {
            out.push_str(line);
            out.push('\n');
        }
        let tmp = self.path.with_extension("jsonl.tmp");
        std::fs::write(&tmp, &out)?;
        std::fs::rename(&tmp, &self.path)?;
        self.lines = 1 + events.len() as u64;
        self.bytes = out.len() as u64;
        Ok(())
    }

    fn remove_file(&mut self) {
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("[ui-sessions] remove {}: {e}", self.path.display()),
        }
        // 如果文件还在 tmp 路径（从未 promote），也删残余。
        if self.path != self.final_path {
            let _ = std::fs::remove_file(&self.final_path);
        }
    }
}

/// 从磁盘恢复的 session 内容。
struct LoadedSession {
    persist: SessionPersist,
    label: Option<String>,
    event_log: Vec<String>,
    first_user_msg: Option<String>,
}

/// 解析一个 .jsonl：meta 行（最后一条生效）+ 事件行（原样保留字符串，
/// 保证恢复后 history 逐字节一致）。任何解析失败 → None（跳过该文件）。
fn load_session_file(path: PathBuf) -> Option<LoadedSession> {
    let raw = std::fs::read_to_string(&path).ok()?;
    let mut meta: Option<serde_json::Value> = None;
    let mut events: Vec<String> = Vec::new();
    for line in raw.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // 容忍截断/坏行
        };
        if v.get("type").and_then(|t| t.as_str()) == Some(META_TYPE) {
            meta = Some(v);
        } else {
            events.push(line.to_string());
        }
    }
    let meta = meta?;
    let session_id = meta.get("session_id")?.as_str()?.to_string();
    let initial_role = meta
        .get("initial_role")
        .and_then(|r| r.as_str())
        .unwrap_or("manager")
        .to_string();
    let created_at_unix_ms = meta
        .get("created_at_unix_ms")
        .and_then(|t| t.as_u64())
        .unwrap_or_else(crate::unix_ts_millis);
    let label = meta
        .get("label")
        .and_then(|l| l.as_str())
        .map(|s| s.to_string());
    // preview 从第一条 UserMessage 事件推导（与 chat_send 的 80 字符
    // 截断规则一致）。
    let first_user_msg = events.iter().find_map(|line| {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        if v.get("type").and_then(|t| t.as_str()) != Some("UserMessage") {
            return None;
        }
        let text = v.get("text")?.as_str()?;
        Some(text.chars().take(80).collect::<String>())
    });
    let lines = (1 + events.len()) as u64; // 近似：meta 覆盖行未计，无碍阈值
    let bytes = raw.len() as u64;
    Some(LoadedSession {
        persist: SessionPersist {
            final_path: path.clone(),
            path,
            session_id,
            initial_role,
            created_at_unix_ms,
            last_label: label.clone(),
            lines,
            bytes,
        },
        label,
        event_log: events,
        first_user_msg,
    })
}

/// 正向扫描 session event_log，找「最近可续跑」的 workflow，组装成
/// [`FailedWorkflow`] 快照。两类候选：
///
/// 1. **被中断的**（有 `WorkflowStarted` 无 `WorkflowFinished`，且未被
///    后续 resume 取代）——server 重启 / 进程崩溃时 workflow task 直接
///    消亡，永远等不到 Finished。取**最早 Started** 的那条：嵌套
///    workflow 的 Started 总在父之后，最早的一定是最外层，续跑它会
///    从 checkpoint 跳过已完成 step、自动重驱内层。
/// 2. 没有中断候选时退化为**最近一条 `status != "ok"` 的
///    `WorkflowFinished`**（原失败扫描语义）。
///
/// resume 续跑会为旧 run 生成新 wf_id，旧 run 永远停在「未完成」
/// 状态 —— 靠续跑时发的 `Status` 事件（"从断点续跑（checkpoint
/// <旧 wf_id>）"）把旧 wf_id 标记为已取代，避免重复 seed。
///
/// 返回 `Some(_)` = 找到了；checkpoint 文件是否还在由调用方
/// （`spawn_controller`）另查，避免这里多 IO 依赖。
fn last_resumable_workflow_from_log(
    event_log: &Arc<parking_lot::RwLock<Vec<String>>>,
) -> Option<FailedWorkflow> {
    let log = event_log.read();
    // wf_id → (起始 idx, name)
    let mut started: Vec<(usize, String, String)> = Vec::new();
    // wf_id → (结束 idx, status, name, summary)
    let mut finished: HashMap<String, (usize, String, String, String)> = HashMap::new();
    // 已被续跑新 run 取代的旧 wf_id
    let mut superseded: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (idx, line) in log.iter().enumerate() {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // 坏行跳过（进程异常退出可能截断尾部）
        };
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let wf_id = v.get("wf_id").and_then(|s| s.as_str()).unwrap_or("");
        match ty {
            "WorkflowStarted" if !wf_id.is_empty() => {
                let name = v
                    .get("name")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                started.push((idx, wf_id.to_string(), name));
            }
            "WorkflowFinished" if !wf_id.is_empty() => {
                let status = v
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = v
                    .get("name")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let summary = v
                    .get("summary")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                finished.insert(wf_id.to_string(), (idx, status, name, summary));
            }
            "Status" => {
                if let Some(msg) = v.get("message").and_then(|s| s.as_str()) {
                    if let Some(rest) = msg.split("从断点续跑（checkpoint ").nth(1) {
                        let old: String =
                            rest.chars().take_while(|c| *c != '）').collect();
                        if !old.is_empty() {
                            superseded.insert(old);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    // 中断候选：started 顺序即外层优先（嵌套的 Started 更晚）。
    for (_, wf_id, name) in &started {
        if finished.contains_key(wf_id) || superseded.contains(wf_id) {
            continue;
        }
        return Some(FailedWorkflow {
            name: name.clone(),
            wf_id: wf_id.clone(),
            summary: String::new(),
            failed_at_unix_ms: 0, // cold-start 不知道精确时间；0 表示"非实时"
        });
    }
    // 无中断：最近一条失败的 Finished。
    finished
        .into_iter()
        .filter(|(_, (_, status, _, _))| status != "ok")
        .max_by_key(|(_, (idx, _, _, _))| *idx)
        .map(|(wf_id, (_, _, name, summary))| FailedWorkflow {
            name,
            wf_id,
            summary,
            failed_at_unix_ms: 0,
        })
}


/// Streaming deltas are transport-only. The final complete RoleTurn carries
/// the full text, so persisting each partial would let one long answer evict
/// thousands of semantic events from the bounded durable window.
fn should_archive_event(event: &ChatEvent) -> bool {
    !matches!(
        event,
        ChatEvent::RoleTurn {
            is_complete: false,
            ..
        }
    )
}

/// 从持久化的前端 ChatEvent JSON 行重建单角色 runner 的模型上下文。
///
/// 只保留真正属于主对话的文本回合：
/// - `UserMessage` → user；
/// - 完整、非 advisor、非 subsession 的 `RoleTurn` → assistant。
///
/// `ModelDelta`/partial RoleTurn 只是流式展示，delegate/workflow 的
/// `sub_id` 回合和 advisor 旁路输出也不能串进主 runner。损坏行直接跳过，
/// 让单条历史损坏不阻塞整个 session 恢复。
pub(crate) fn initial_history_from_event_log(
    event_log: &[String],
) -> Vec<latte_ai::models::Message> {
    let mut history = Vec::new();
    // A bounded/lagged log may start in the middle of a turn. Never seed
    // the model with a leading or duplicate assistant message: only accept
    // a complete assistant after at least one retained user message.
    let mut has_unanswered_user = false;
    for line in event_log {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("UserMessage") => {
                if let Some(text) = v.get("text").and_then(|x| x.as_str()) {
                    if !text.is_empty() {
                        history.push(latte_ai::models::Message::user(text));
                        has_unanswered_user = true;
                    }
                }
            }
            Some("RoleTurn") => {
                let complete = v
                    .get("is_complete")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(true);
                let is_advisor = v.get("role_id").and_then(|x| x.as_str()) == Some("advisor");
                let is_subsession = v
                    .get("sub_id")
                    .and_then(|x| x.as_str())
                    .is_some_and(|id| !id.is_empty());
                if complete && !is_advisor && !is_subsession && has_unanswered_user {
                    if let Some(content) = v.get("content").and_then(|x| x.as_str()) {
                        if !content.is_empty() {
                            history.push(latte_ai::models::Message::assistant(content));
                            has_unanswered_user = false;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    history
}

// ─── Session 状态 ─────────────────────────────────────────────────

/// 懒 spawn 所需的全部输入（恢复 session 首个 chat/subscribe 时才
/// 真正起 controller）。
#[derive(Clone)]
pub(crate) struct SessionSpawnParams {
    pub(crate) merged: Arc<parking_lot::RwLock<AgentConfig>>,
    pub(crate) resolver: Arc<ModelResolver>,
    pub(crate) cwd: PathBuf,
    pub(crate) primary_model_id: Option<String>,
    pub(crate) initial_tier: Option<ModelTier>,
    pub(crate) subsession_store: Arc<latte_agent_core::subsession::SubsessionStore>,
    /// Pre-existing conversation history to seed the controller with
    /// before the first user turn. Empty for fresh sessions; populated
    /// for forked sessions from the source prefix and for restored
    /// sessions from their persisted event log. Threaded into
    /// `ControllerConfig.initial_history` (single-role path).
    pub(crate) initial_history: Vec<latte_ai::models::Message>,
}

/// Per-tab state. Holds the controller (restored sessions spawn it
/// lazily) and the broadcast channel the SSE stream subscribes to. Two
/// handles never share a controller, so `/api/chat/send` on tab A is
/// invisible to tab B's SSE stream.
pub(crate) struct SessionHandle {
    pub(crate) session_id: String,
    /// controller that runs the actual chat loop and owns history.
    /// `None` = 从磁盘恢复、尚未有 chat/subscribe 落到它（懒 spawn）。
    controller: parking_lot::Mutex<Option<Arc<ChatController>>>,
    /// 懒 spawn 双重检查第二段（防并发 chat_send 起两份 controller）。
    spawn_lock: tokio::sync::Mutex<()>,
    spawn: SessionSpawnParams,
    /// Preview copy of the first user message — used by `/api/sessions`
    /// to render a sidebar entry without re-reading the controller.
    pub(crate) first_user_msg: parking_lot::Mutex<Option<String>>,
    /// User-assigned display name (via POST /api/session/label). When
    /// set it takes precedence over `first_user_msg` in the sidebar.
    pub(crate) label: parking_lot::Mutex<Option<String>>,
    /// Frontend-JSON `ChatEvent`s captured for this session. `GET
    /// /api/session/history` replays these so the chat panel restores
    /// its content when the user switches back to this session.
    pub(crate) event_log: Arc<parking_lot::RwLock<Vec<String>>>,
    pub(crate) created_at: Instant,
    pub(crate) last_activity: Arc<parking_lot::Mutex<Instant>>,
    pub(crate) initial_role: String,
    /// true = 本 session 是从 ui-sessions 落盘恢复的（其可见历史早于
    /// 本次进程启动；agent 上下文从空开始）。
    pub(crate) restored: bool,
    /// 落盘状态（Arc 共享给 archiver 任务）；`None` = 文件创建失败
    /// （只告警，聊天照常）。
    persist: Option<Arc<parking_lot::Mutex<SessionPersist>>>,
    /// 流式模式开关（运行时可切换）。UI toggle -> POST /api/chat/stream-mode
    /// -> `self.stream_mode.store(bool)` -> ControllerConfig -> AgentRunner。
    pub(crate) stream_mode: Arc<std::sync::atomic::AtomicBool>,
}

impl SessionHandle {
    pub(crate) fn touch(&self) {
        *self.last_activity.lock() = Instant::now();
    }

    /// 懒 spawn：已有 controller 直接返回；恢复 session 在首个
    /// chat_send/subscribe 时在这里真正起 controller（双重检查锁）。
    ///
    /// 额外保护：若 controller 存在但 driver 已死（abort 后），自动
    /// respawn driver，避免 session 永久变哑。
    pub(crate) async fn controller_or_spawn(&self) -> Result<Arc<ChatController>, String> {
        // Clone out of parking_lot guard immediately to avoid holding
        // non-Send guard across await points.
        let existing = self.controller.lock().clone();
        if let Some(c) = existing {
            // Fast path: driver alive → return immediately.
            if !c.is_driver_dead().await {
                return Ok(c);
            }
            // Driver dead (post-abort): respawn under lock to avoid
            // racing with another caller.
            let _guard = self.spawn_lock.lock().await;
            // Double-check after acquiring lock.
            if !c.is_driver_dead().await {
                return Ok(c);
            }
            c.respawn()
                .await
                .map_err(|e| format!("respawn failed: {e}"))?;
            return Ok(c);
        }
        let _guard = self.spawn_lock.lock().await;
        if let Some(c) = self.controller.lock().clone() {
            return Ok(c);
        }
        let c = self.spawn_controller().await?;
        *self.controller.lock() = Some(c.clone());
        Ok(c)
    }
    /// Try to grab the controller without spawning. Returns None when
    /// the session is still in the "restored from disk, no chat yet"
    /// state. Used by per-turn operations like `cancel_turn` that
    /// must NOT lazily spawn a controller just to no-op it.
    pub(crate) fn try_controller(&self) -> Option<Arc<ChatController>> {
        self.controller.lock().clone()
    }

    /// delete 路径：起过 controller 才 abort（恢复未激活的 no-op）。
    pub(crate) async fn abort_if_spawned(&self) {
        // 注意先把 guard 释放再 await（parking_lot guard 非 Send）。
        let spawned = self.controller.lock().clone();
        if let Some(c) = spawned {
            c.abort().await;
        }
    }

    /// 删落盘文件（delete session 时调用）。
    pub(crate) fn delete_files(&self) {
        if let Some(p) = &self.persist {
            p.lock().remove_file();
        }
    }

    /// 改 label：内存 + 尾加 meta 覆盖行。
    pub(crate) fn set_label(&self, label: Option<String>) {
        *self.label.lock() = label.clone();
        if let Some(p) = &self.persist {
            p.lock().set_label(&label);
        }
    }

    /// spawn controller + advisor 监察 + event_log 归档（tee 落盘）。
    /// 新 session 在 create 时 eagerly 调；恢复 session 懒调。
    async fn spawn_controller(&self) -> Result<Arc<ChatController>, String> {
        let agent_config_snapshot = Arc::new(self.spawn.merged.read().clone());
        let default_params = GenerateParams::default();
        // advisor 总开关来自 agents 配置（`[advisor] enabled`），不再是
        // 硬编码 default（此前用户无法关闭 advisor——用户反馈
        // 「advisor 总是卡/空喊」却无路可关）。
        let advisor_monitor_cfg = AdvisorMonitorConfig {
            enabled: agent_config_snapshot.advisor.enabled(),
            ..AdvisorMonitorConfig::default()
        };
        let cfg = ControllerConfig {
            task_id: None,
            roles: vec![self.initial_role.clone()],
            initial_prompt: None,
            max_rounds: 0,
            session_token_budget: 0,
            // snapshot 一份当前配置：session 固化创建时刻的配置，
            // 之后的角色编辑只影响新建的 session。
            agent_config: agent_config_snapshot.clone(),
            model_resolver: self.spawn.resolver.clone(),
            default_params: default_params.clone(),
            primary_model_id: self.spawn.primary_model_id.clone(),
            initial_tier: self.spawn.initial_tier.clone(),
            initial_history: self.spawn.initial_history.clone(),
            cwd: self.spawn.cwd.clone(),
            subsession_store: self.spawn.subsession_store.clone(),
            // 透传 SessionHandle 自己的 session_id -- subsession_store
            // 落盘用它当目录名，删 session 时联删。**不要**用
            // `self.spawn.cwd` 凑数（绝对路径会被路径安全检查拒绝）。
            session_id: self.session_id.clone(),
            advisor_monitor: advisor_monitor_cfg.clone(),
            stream_mode: self.stream_mode.clone(),
            // 单 session delegate 累计上限（env 覆盖，默认 12）。
            max_delegates_per_session:
                latte_agent_core::controller::default_max_delegates(),
        };
        let controller = Arc::new(ChatController::new(256));
        // `spawn` returns a broadcast::Receiver (events consumer); the
        // controller runs in the background. We don't keep the receiver
        // here — the per-tab SSE subscriber is what reads events.
        let _rx = controller.spawn(cfg).await;

        // ── Cold-start seed ──
        // 恢复 session 首次 spawn controller 时，把 event_log 历史里
        // 「最近可续跑」的 workflow（被中断的优先，否则最近失败的）写进
        // controller `last_failed_workflow` 状态，让"重启后点继续"无需
        // 显式传 wf_id。Checkpoint 文件丢失则跳过（保留 API 容错：用户
        // 手动清理过 workflow-runs/ 的场景）。
        //
        // 时序：controller.spawn() 内部已经把内部订阅者挂上了，但
        // driver 还没收到任何 ControllerInput、没有 send 任何
        // WorkflowFinished；后续用户发消息触发的新事件会经内部订阅
        // 者覆盖 seed —— 这是正确语义（用户重启后若主动跑了别的
        // workflow，再点"继续"应当续跑那个新的失败，不是历史
        // 那个）。
        if let Some(failed) = last_resumable_workflow_from_log(&self.event_log) {
            let ckpt_path = self
                .spawn
                .cwd
                .join(".latte/workflow-runs")
                .join(format!("{}.jsonl", failed.wf_id));
            // wf_id 路径安全（恢复路径不会遇到恶意输入，但仍守一道）：
            // 拒绝任何带 `/`/`\`/`..` 的串。
            let safe = !failed.wf_id.is_empty()
                && !failed.wf_id.contains('/')
                && !failed.wf_id.contains('\\')
                && !failed.wf_id.contains("..");
            if safe && ckpt_path.exists() {
                controller.set_last_failed_workflow(failed);
            }
        }

        // Advisor 监察者：旁路订阅该 session 的事件流，发现异常时经
        // controller 的 hint 队列纠偏（通道 A）并广播 🦉 气泡（通道 B）。
        // 任务 detach 与下方 archive 任务同生命周期：ChatEvent::Done 或
        // controller drop 后自动退出。
        if advisor_monitor_cfg.enabled {
            let engine = AdvisorReviewEngine::new(
                agent_config_snapshot,
                self.spawn.resolver.clone(),
                default_params,
            )
            .with_watchdog_notes(
                self.spawn.cwd.clone(),
                advisor_monitor_cfg.watchdog_notes,
            )
            .with_review_settings(advisor_monitor_cfg.review_settings);
            // 给 advisor 也分配一个 subsession sink：每次 review 的
            // LLM 调用 / 返回会落盘到
            // `<ui-sessions>/<sid>/advisor-<micros>.jsonl`，跟主角色
            // 同目录，删主 session 时一并清掉。
            let engine = if self.session_id.is_empty() {
                engine
            } else {
                let (_sub_id, sink) = self.spawn.subsession_store.create(&self.session_id, "advisor");
                engine.with_subsession_sink(sink)
            };
            AdvisorMonitor::spawn(
                controller.clone(),
                advisor_monitor_cfg,
                engine,
                self.initial_role.clone(),
            );
        }
        // Archive every ChatEvent as frontend-shaped JSON so a tab that
        // switches away and back can restore the chat contents. Bounded
        // to MAX_LOG entries (oldest dropped) to keep memory flat.
        // 同时 tee 写 ui-sessions 落盘（每行与 event_log 字符串一致）。
        // `Prompt`/`SessionInfo` 是启动时的初始化事件，不发消息的空
        // session 不应该因此落盘——先 skip persist，等有真实对话时才
        // 创建文件。
        {
            let log = self.event_log.clone();
            let persist = self.persist.clone();
            let sid = self.session_id.clone();
            let mut archive_rx = controller.subscribe();
            // 落盘搬到独立任务：archiver 的 recv 循环里只做内存
            // push（快），文件 open/write 经无界 mpsc 交给写手任务。
            // 此前同步 IO 就在 recv 路径上，archiver 因此天生是最慢
            // 的订阅者，最容易被 broadcast(256) 甩掉。
            let (disk_tx, mut disk_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
            if let Some(p) = persist {
                let log_for_disk = log.clone();
                tokio::spawn(async move {
                    while let Some(line) = disk_rx.recv().await {
                        p.lock().append_event(&line, &log_for_disk);
                    }
                });
            }
            tokio::spawn(async move {
                loop {
                    let ev = match archive_rx.recv().await {
                        Ok(ev) => ev,
                        // Lagged 是**可恢复**的：只丢了 n 条，通道还活着。
                        // 此前 `while let Ok(..)` 把它当终止条件——一次
                        // 突发就让 archiver 永久退出，event_log 与落盘
                        // 从此不再增长（切 tab 回来历史停在断点，重启
                        // 后断点之后全丢；正等用户回答的 ask 弹框如果
                        // 落在这段里，前端再也拿不到它）。
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            eprintln!(
                                "[ui-sessions] {sid} archiver 落后，丢弃 {n} 条事件（继续归档）"
                            );
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    };
                    if !should_archive_event(&ev) {
                        continue;
                    }
                    let is_init = matches!(
                        &ev,
                        ChatEvent::Prompt { .. } | ChatEvent::SessionInfo { .. }
                    );
                    if let Ok(json) = chat_event_to_frontend_json(&ev) {
                        {
                            let mut g = log.write();
                            if g.len() >= MAX_LOG {
                                g.remove(0);
                            }
                            g.push(json.clone());
                        }
                        if !is_init {
                            // 写手任务不在时（persist == None）发送失败，
                            // 忽略即可——不落盘的 session 照常聊。
                            let _ = disk_tx.send(json);
                        }
                    }
                }
            });
        }
        Ok(controller)
    }
}

/// Process-wide map of active session IDs to their per-tab handle.
/// `parking_lot::RwLock` because reads (every chat/SSE request) dominate.
pub(crate) type SessionMap = parking_lot::RwLock<HashMap<String, Arc<SessionHandle>>>;

/// Construct a fresh `SessionHandle` whose controller is already
/// `spawn`ed and owns its own broadcast channel. The caller inserts
/// the handle into `SessionMap`; failure to spawn the controller is
/// propagated to the HTTP caller via a 500.
pub(crate) async fn create_session_handle(
    session_id: String,
    initial_role: &str,
    merged: &Arc<parking_lot::RwLock<AgentConfig>>,
    resolver: &Arc<ModelResolver>,
    cwd: &std::path::Path,
    primary_model_id: Option<String>,
    initial_tier: Option<ModelTier>,
    subsession_store: &Arc<latte_agent_core::subsession::SubsessionStore>,
) -> Result<SessionHandle, String> {
    // 落盘：创建失败只告警（聊天照常，只是不持久化）。
    let persist = match SessionPersist::create(cwd, &session_id, initial_role) {
        Ok(p) => Some(Arc::new(parking_lot::Mutex::new(p))),
        Err(e) => {
            eprintln!(
                "[ui-sessions] create {}: {e} (session 不持久化)",
                SessionPersist::path_for(cwd, &session_id).display()
            );
            None
        }
    };
    let now = Instant::now();
    let handle = SessionHandle {
        session_id,
        controller: parking_lot::Mutex::new(None),
        spawn_lock: tokio::sync::Mutex::new(()),
        spawn: SessionSpawnParams {
            merged: merged.clone(),
            resolver: resolver.clone(),
            cwd: cwd.to_path_buf(),
            primary_model_id,
            initial_tier,
            subsession_store: subsession_store.clone(),
            initial_history: Vec::new(),
        },
        first_user_msg: parking_lot::Mutex::new(None),
        label: parking_lot::Mutex::new(None),
        event_log: Arc::new(parking_lot::RwLock::new(Vec::new())),
        created_at: now,
        last_activity: Arc::new(parking_lot::Mutex::new(now)),
        initial_role: initial_role.to_string(),
        restored: false,
        persist,
        stream_mode: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    // 新 session eagerly spawn（行为同改造前）。
    handle.controller_or_spawn().await?;
    Ok(handle)
}

/// Construct a forked `SessionHandle`: a brand-new session pre-seeded
/// with a cloned discussion prefix (`event_lines`, already frontend-JSON
/// strings) for visible history, and `initial_history` (reconstructed
/// LLM messages) so the fork's agent remembers the branch point. The
/// prefix is written to disk up front so the fork survives a restart;
/// the eagerly-spawned controller's archiver then appends any new turns.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn create_forked_handle(
    session_id: String,
    initial_role: &str,
    merged: &Arc<parking_lot::RwLock<AgentConfig>>,
    resolver: &Arc<ModelResolver>,
    cwd: &std::path::Path,
    subsession_store: &Arc<latte_agent_core::subsession::SubsessionStore>,
    event_lines: Vec<String>,
    initial_history: Vec<latte_ai::models::Message>,
    first_user_msg: Option<String>,
    label: Option<String>,
) -> Result<SessionHandle, String> {
    let persist = match SessionPersist::create(cwd, &session_id, initial_role) {
        Ok(mut p) => {
            // Seed the label into the meta line, then write the cloned
            // prefix so the fork is durable before any new turn.
            p.last_label = label.clone();
            for line in &event_lines {
                if let Err(e) = p.append_raw(line) {
                    eprintln!("[ui-sessions] fork persist {}: {e}", p.path.display());
                    break;
                }
            }
            Some(Arc::new(parking_lot::Mutex::new(p)))
        }
        Err(e) => {
            eprintln!(
                "[ui-sessions] fork create {}: {e} (session 不持久化)",
                SessionPersist::path_for(cwd, &session_id).display()
            );
            None
        }
    };
    let now = Instant::now();
    let handle = SessionHandle {
        session_id,
        controller: parking_lot::Mutex::new(None),
        spawn_lock: tokio::sync::Mutex::new(()),
        spawn: SessionSpawnParams {
            merged: merged.clone(),
            resolver: resolver.clone(),
            cwd: cwd.to_path_buf(),
            primary_model_id: None,
            initial_tier: None,
            subsession_store: subsession_store.clone(),
            initial_history,
        },
        first_user_msg: parking_lot::Mutex::new(first_user_msg),
        label: parking_lot::Mutex::new(label),
        event_log: Arc::new(parking_lot::RwLock::new(event_lines)),
        created_at: now,
        last_activity: Arc::new(parking_lot::Mutex::new(now)),
        initial_role: initial_role.to_string(),
        restored: false,
        persist,
        stream_mode: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    // Eager spawn so the seeded `initial_history` is loaded into the
    // controller's context immediately (single-role path).
    handle.controller_or_spawn().await?;
    Ok(handle)
}

/// 扫描 `<cwd>/.latte/ui-sessions/*.jsonl`，恢复 session 元数据 +
/// event_log 为未激活 handle（不 spawn controller）。返回的 handle
/// 由调用方插入 SessionMap。
pub(crate) fn restore_sessions(cwd: &Path, spawn: &SessionSpawnParams) -> Vec<SessionHandle> {
    let dir = SessionPersist::dir_for(cwd);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(loaded) = load_session_file(path) else {
            continue;
        };
        // created_at / last_activity 的 Instant 用墙钟年龄近似（system
        // clock 回拨时 saturate 到 now）。
        let age_ms =
            crate::unix_ts_millis().saturating_sub(loaded.persist.created_at_unix_ms);
        let created_at = Instant::now()
            .checked_sub(Duration::from_millis(age_ms))
            .unwrap_or_else(Instant::now);
        let session_id = loaded.persist.session_id.clone();
        let initial_role = loaded.persist.initial_role.clone();
        // 每个恢复 session 都从自己的持久 event_log 重建模型历史；不能
        // 继续克隆全局 restore_base 里的空 initial_history，否则历史只
        // 在 UI 可见，下一次模型调用却像一段全新对话。
        let restored_spawn = SessionSpawnParams {
            initial_history: initial_history_from_event_log(&loaded.event_log),
            ..spawn.clone()
        };
        out.push(SessionHandle {
            session_id,
            controller: parking_lot::Mutex::new(None),
            spawn_lock: tokio::sync::Mutex::new(()),
            spawn: restored_spawn,
            first_user_msg: parking_lot::Mutex::new(loaded.first_user_msg),
            label: parking_lot::Mutex::new(loaded.label),
            event_log: Arc::new(parking_lot::RwLock::new(loaded.event_log)),
            created_at,
            last_activity: Arc::new(parking_lot::Mutex::new(created_at)),
            initial_role,
            restored: true,
            persist: Some(Arc::new(parking_lot::Mutex::new(loaded.persist))),
            stream_mode: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });
    }
    // 稳定顺序（按创建时间，旧的在前）——list 排序在 api 层做，这里
    // 只保证恢复确定性。
    out.sort_by_key(|h| h.created_at);
    out
}

/// 清理 `<cwd>/.latte/tmp/` 下所有残留文件。服务器启动时调用，
/// 确保上次异常退出遗留的暂存 session 文件不会堆积。
pub fn clean_tmp(cwd: &Path) {
    let dir = SessionPersist::tmp_dir(cwd);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个 event_log（Vec<String>）装进 Arc<RwLock<...>>，
    /// 供 helper 单元测试用。
    fn make_log(lines: &[&str]) -> Arc<parking_lot::RwLock<Vec<String>>> {
        let log: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
        Arc::new(parking_lot::RwLock::new(log))
    }

    #[test]
    fn initial_history_keeps_only_main_complete_text_turns() {
        let lines = vec![
            r#"{"type":"UserMessage","text":"question"}"#.to_string(),
            r#"{"type":"RoleTurn","role_id":"manager","content":"partial","is_complete":false,"sub_id":null}"#.to_string(),
            r#"{"type":"RoleTurn","role_id":"programmer","content":"delegate","is_complete":true,"sub_id":"sub-1"}"#.to_string(),
            r#"{"type":"RoleTurn","role_id":"advisor","content":"watchdog","is_complete":true,"sub_id":null}"#.to_string(),
            r#"{"type":"ModelDelta","role_id":"manager","delta":"token"}"#.to_string(),
            "truncated json".to_string(),
            // 旧日志可能没有 is_complete/sub_id；按完整主回合兼容。
            r#"{"type":"RoleTurn","role_id":"manager","content":"answer"}"#.to_string(),
        ];

        let history = initial_history_from_event_log(&lines);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].as_text(), "question");
        assert_eq!(history[1].as_text(), "answer");
    }

    #[test]
    fn initial_history_drops_leading_unpaired_assistant() {
        let lines = vec![
            r#"{"type":"RoleTurn","role_id":"manager","content":"orphan","is_complete":true,"sub_id":null}"#.to_string(),
            r#"{"type":"UserMessage","text":"question 2"}"#.to_string(),
            r#"{"type":"RoleTurn","role_id":"manager","content":"answer 2","is_complete":true,"sub_id":null}"#.to_string(),
            r#"{"type":"RoleTurn","role_id":"manager","content":"duplicate orphan","is_complete":true,"sub_id":null}"#.to_string(),
        ];

        let history = initial_history_from_event_log(&lines);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].as_text(), "question 2");
        assert_eq!(history[1].as_text(), "answer 2");
    }

    #[test]
    fn partial_role_turns_do_not_consume_durable_window() {
        let mut log = Vec::new();
        let mut archive = |event: ChatEvent| {
            if !should_archive_event(&event) {
                return;
            }
            let json = chat_event_to_frontend_json(&event).expect("serialize event");
            if log.len() >= MAX_LOG {
                log.remove(0);
            }
            log.push(json);
        };

        archive(ChatEvent::UserMessage {
            text: "long streamed question".into(),
        });
        for _ in 0..(MAX_LOG + 100) {
            archive(ChatEvent::RoleTurn {
                role_id: "manager".into(),
                content: "x".into(),
                is_complete: false,
                sub_id: None,
            });
        }
        archive(ChatEvent::RoleTurn {
            role_id: "manager".into(),
            content: "complete answer".into(),
            is_complete: true,
            sub_id: None,
        });
        drop(archive);

        assert_eq!(log.len(), 2, "partial transport events must not be durable");
        assert!(log.iter().all(|line| !line.contains(r#""is_complete":false"#)));
        let history = initial_history_from_event_log(&log);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].as_text(), "long streamed question");
        assert_eq!(history[1].as_text(), "complete answer");
    }

    /// 模拟进程重启：从 ui-sessions/*.jsonl 扫回的 lazy handle 必须
    /// 已携带由事件重建的 initial_history，而不是 restore_base 的空值。
    #[test]
    fn restore_sessions_rebuilds_initial_history_from_event_log() {
        let dir = tempfile::tempdir().unwrap();
        let mut persist = SessionPersist::create(dir.path(), "restored-history", "manager")
            .unwrap();
        persist
            .append_raw(r#"{"type":"UserMessage","text":"before restart"}"#)
            .unwrap();
        persist
            .append_raw(r#"{"type":"RoleTurn","role_id":"manager","content":"remembered answer","is_complete":true,"sub_id":null}"#)
            .unwrap();

        let config = AgentConfig::default();
        let spawn = SessionSpawnParams {
            merged: Arc::new(parking_lot::RwLock::new(config.clone())),
            resolver: Arc::new(ModelResolver::from_config(&config).unwrap()),
            cwd: dir.path().to_path_buf(),
            primary_model_id: None,
            initial_tier: None,
            subsession_store: Arc::new(
                latte_agent_core::subsession::SubsessionStore::new(),
            ),
            initial_history: Vec::new(),
        };

        let restored = restore_sessions(dir.path(), &spawn);
        assert_eq!(restored.len(), 1);
        let history = &restored[0].spawn.initial_history;
        assert_eq!(history.len(), 2, "restart must seed user + assistant history");
        assert_eq!(history[0].as_text(), "before restart");
        assert_eq!(history[1].as_text(), "remembered answer");
    }

    /// 正常路径：log 末尾有一条 status != "ok" 的 WorkflowFinished，
    /// 反向扫描应找到它并组装成 FailedWorkflow。
    #[test]
    fn last_resumable_workflow_from_log_finds_last_non_ok() {
        let log = make_log(&[
            r#"{"type":"WorkflowFinished","name":"a","wf_id":"wf-a","status":"ok","summary":"done"}"#,
            r#"{"type":"WorkflowFinished","name":"b","wf_id":"wf-b","status":"failed","summary":"boom"}"#,
            r#"{"type":"WorkflowFinished","name":"c","wf_id":"wf-c","status":"failed","summary":"again"}"#,
        ]);
        let got = last_resumable_workflow_from_log(&log).unwrap();
        assert_eq!(got.wf_id, "wf-c", "应取最末一条失败的（reverse scan）");
        assert_eq!(got.name, "c");
        assert_eq!(got.summary, "again");
    }

    /// 全 ok → 返回 None。冷启动 session 没有可续跑的失败 workflow。
    #[test]
    fn last_resumable_workflow_from_log_returns_none_when_all_ok() {
        let log = make_log(&[
            r#"{"type":"WorkflowFinished","name":"a","wf_id":"wf-a","status":"ok","summary":"done"}"#,
        ]);
        assert!(last_resumable_workflow_from_log(&log).is_none());
    }

    /// 空 log → None。
    #[test]
    fn last_resumable_workflow_from_log_returns_none_when_empty() {
        let log = make_log(&[]);
        assert!(last_resumable_workflow_from_log(&log).is_none());
    }

    /// 损坏的 JSON 行应当被跳过 —— 真实 ui-sessions/*.jsonl 在
    /// 进程异常退出时可能截断，不能让单行坏数据导致整个 cold-start
    /// 失败。
    #[test]
    fn last_resumable_workflow_from_log_skips_corrupt_lines() {
        let log = make_log(&[
            "not json",
            r#"{"type":"WorkflowFinished","name":"a","wf_id":"wf-a","status":"failed"}"#,
            r#"{"type":"WorkflowFinished"}"#, // 缺 wf_id
        ]);
        let got = last_resumable_workflow_from_log(&log).unwrap();
        assert_eq!(got.wf_id, "wf-a");
    }

    /// 缺字段的 WorkflowFinished（无 wf_id）→ 跳过；不能把空串当
    /// wf_id 写进 state（否则 resume API 会拿空串去拼 checkpoint 路径）。
    #[test]
    fn last_resumable_workflow_from_log_skips_missing_wf_id() {
        let log = make_log(&[
            r#"{"type":"WorkflowFinished","status":"failed"}"#,
        ]);
        assert!(last_resumable_workflow_from_log(&log).is_none());
    }

    /// 多个 status=failed 事件混合 status=ok：reverse scan 应找到
    /// log 末尾**最近的**失败。中间的 ok 不应"清空"前面的失败
    /// （success wipe 是 controller 内部订阅者的语义，仅适用于
    /// 实时事件流；冷启动 scan 保留 reverse 顺序原始语义）。
    #[test]
    fn last_resumable_workflow_from_log_mixed_ok_failed() {
        let log = make_log(&[
            r#"{"type":"WorkflowFinished","name":"a","wf_id":"wf-a","status":"failed","summary":"old"}"#,
            r#"{"type":"WorkflowFinished","name":"b","wf_id":"wf-b","status":"ok","summary":"middle"}"#,
            r#"{"type":"WorkflowFinished","name":"c","wf_id":"wf-c","status":"failed","summary":"new"}"#,
        ]);
        let got = last_resumable_workflow_from_log(&log).unwrap();
        assert_eq!(got.wf_id, "wf-c", "末尾的 failed 覆盖前序的语义");
    }

    /// 中断场景（实测事故）：workflow 被 Paused 后进程重启，
    /// 永远等不到 WorkflowFinished。扫描应挑出最外层未完成的 run
    /// （嵌套 workflow 的 Started 更晚，最早 Started = 最外层）。
    #[test]
    fn last_resumable_workflow_from_log_finds_interrupted_outermost() {
        let log = make_log(&[
            r#"{"type":"WorkflowStarted","name":"design_and_plan","wf_id":"wf-parent","topic":"t"}"#,
            r#"{"type":"WorkflowStarted","name":"explore","wf_id":"wf-nested","topic":"t"}"#,
            r#"{"type":"WorkflowFinished","name":"explore","wf_id":"wf-nested","status":"ok","summary":"s"}"#,
            r#"{"type":"WorkflowStarted","name":"code_review","wf_id":"wf-nested2","topic":"t"}"#,
            r#"{"type":"Paused","reason":"模型不可用"}"#,
        ]);
        let got = last_resumable_workflow_from_log(&log).unwrap();
        assert_eq!(got.wf_id, "wf-parent", "最外层未完成 run 优先于嵌套未完成 run");
        assert_eq!(got.name, "design_and_plan");
    }

    /// 中断的旧 run 已被续跑（新 wf_id 的 run 发出带旧 wf_id 的
    /// Status 事件）→ 旧 run 不再是候选；新 run ok 后无可续跑目标。
    #[test]
    fn last_resumable_workflow_from_log_superseded_by_resume() {
        let log = make_log(&[
            r#"{"type":"WorkflowStarted","name":"a","wf_id":"wf-old","topic":"t"}"#,
            r#"{"type":"Paused","reason":"模型不可用"}"#,
            r#"{"type":"Status","message":"workflow 'a' 从断点续跑（checkpoint wf-old）：跳过已完成的 2 步"}"#,
            r#"{"type":"WorkflowStarted","name":"a","wf_id":"wf-new","topic":"t"}"#,
            r#"{"type":"WorkflowFinished","name":"a","wf_id":"wf-new","status":"ok","summary":"done"}"#,
        ]);
        assert!(last_resumable_workflow_from_log(&log).is_none());
    }

    /// 续跑的新 run 也中断 → 候选是新 run 的 wf_id（旧 run 已取代）。
    #[test]
    fn last_resumable_workflow_from_log_resumed_run_interrupted_again() {
        let log = make_log(&[
            r#"{"type":"WorkflowStarted","name":"a","wf_id":"wf-old","topic":"t"}"#,
            r#"{"type":"Status","message":"workflow 'a' 从断点续跑（checkpoint wf-old）：跳过已完成的 2 步"}"#,
            r#"{"type":"WorkflowStarted","name":"a","wf_id":"wf-new","topic":"t"}"#,
            r#"{"type":"Paused","reason":"模型不可用"}"#,
        ]);
        let got = last_resumable_workflow_from_log(&log).unwrap();
        assert_eq!(got.wf_id, "wf-new");
    }
}
