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
use latte_agent_core::controller::{ChatController, ChatEvent, ControllerConfig};
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
}

impl SessionHandle {
    pub(crate) fn touch(&self) {
        *self.last_activity.lock() = Instant::now();
    }

    /// 懒 spawn：已有 controller 直接返回；恢复 session 在首个
    /// chat_send/subscribe 时在这里真正起 controller（双重检查锁）。
    pub(crate) async fn controller_or_spawn(&self) -> Result<Arc<ChatController>, String> {
        if let Some(c) = self.controller.lock().clone() {
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
        let advisor_monitor_cfg = AdvisorMonitorConfig::default();
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
            initial_history: vec![],
            cwd: self.spawn.cwd.clone(),
            subsession_store: self.spawn.subsession_store.clone(),
            // 透传 SessionHandle 自己的 session_id —— subsession_store
            // 落盘用它当目录名，删 session 时联删。**不要**用
            // `self.spawn.cwd` 凑数（绝对路径会被路径安全检查拒绝）。
            session_id: self.session_id.clone(),
            advisor_monitor: advisor_monitor_cfg.clone(),
        };
        let controller = Arc::new(ChatController::new(256));
        // `spawn` returns a broadcast::Receiver (events consumer); the
        // controller runs in the background. We don't keep the receiver
        // here — the per-tab SSE subscriber is what reads events.
        let _rx = controller.spawn(cfg).await;
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
            );
            // 给 advisor 也分配一个 subsession sink：每次 review 的
            // LLM 调用 / 返回会落盘到
            // `<ui-sessions>/<sid>/advisor-<micros>.jsonl`，跟主角色
            // 同目录，删主 session 时一并清掉。
            let engine = if self.session_id.is_empty() {
                engine
            } else {
                let (_sub_id, sink) = self.spawn.subsession_store.get_or_create(&self.session_id, "advisor");
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
            let mut archive_rx = controller.subscribe();
            tokio::spawn(async move {
                while let Ok(ev) = archive_rx.recv().await {
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
                            if let Some(p) = &persist {
                                p.lock().append_event(&json, &log);
                            }
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
        },
        first_user_msg: parking_lot::Mutex::new(None),
        label: parking_lot::Mutex::new(None),
        event_log: Arc::new(parking_lot::RwLock::new(Vec::new())),
        created_at: now,
        last_activity: Arc::new(parking_lot::Mutex::new(now)),
        initial_role: initial_role.to_string(),
        restored: false,
        persist,
    };
    // 新 session eagerly spawn（行为同改造前）。
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
        out.push(SessionHandle {
            session_id,
            controller: parking_lot::Mutex::new(None),
            spawn_lock: tokio::sync::Mutex::new(()),
            spawn: spawn.clone(),
            first_user_msg: parking_lot::Mutex::new(loaded.first_user_msg),
            label: parking_lot::Mutex::new(loaded.label),
            event_log: Arc::new(parking_lot::RwLock::new(loaded.event_log)),
            created_at,
            last_activity: Arc::new(parking_lot::Mutex::new(created_at)),
            initial_role,
            restored: true,
            persist: Some(Arc::new(parking_lot::Mutex::new(loaded.persist))),
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
