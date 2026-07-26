//! 任务看板（Task Board）：任务是一等实体，有多状态生命周期；`todo`
//! 可带 `scheduled_at` 排期，由 scheduler 到点自动派发给 manager
//! （新建一个 ui-session 执行）。设计文档：`docs/task-board-design.md`。
//!
//! 存储：`<cwd>/.latte/tasks/`（ui-server 的 cwd 即项目根，天然按项目
//! 隔离）——`board.json` 元信息 + 每任务 `<id>.json`，写入一律
//! `tmp + rename` 原子替换（同 `ui-sessions/` 约定）；delete 移入
//! `archive/` 子目录。
//!
//! 协议无关：`UiBackend` 上挂 `Arc<RwLock<TaskStore>>`，HTTP 薄壳在
//! `crate::handlers`，本文件的 `*_task` 函数返回 `Result<_, ApiError>`，
//! 与 `crate::api` 的函数同级复用。

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::api::{self, ApiError};
use crate::UiBackend;

// ─── 状态机常量 ───────────────────────────────────────────────────

/// 全部已知状态（§2 状态机，8 态）。
pub const STATES: [&str; 8] = [
    "backlog",
    "todo",
    "in_progress",
    "human_review",
    "rework",
    "merging",
    "done",
    "cancelled",
];
/// 可派发/进行中的状态。
pub const ACTIVE_STATES: [&str; 4] = ["todo", "in_progress", "rework", "merging"];
/// 终态。
pub const TERMINAL_STATES: [&str; 2] = ["done", "cancelled"];

const SCHEMA: i64 = 1;
const DEFAULT_ID_PREFIX: &str = "LAT";
/// 任务 id 起始序号（`{id_prefix}-{seq}`）。
const FIRST_SEQ: i64 = 100;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ─── 数据模型 ─────────────────────────────────────────────────────

/// 一次执行记录：每次派发（含 rework 重跑）新建 ui-session 并追加一条；
/// `runs.last().session_id` 即"查看执行详情"的跳转目标。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TaskRun {
    pub session_id: String,
    pub started_at: i64,
    #[serde(default)]
    pub ended_at: Option<i64>,
    /// `completed` / `aborted` / `failed` / `timeout`。
    #[serde(default)]
    pub result: Option<String>,
}

/// 状态变迁历史。`actor` ∈ `user` / `scheduler` / `manager`。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct HistoryEntry {
    pub at: i64,
    #[serde(default)]
    pub from: Option<String>,
    pub to: String,
    pub actor: String,
    #[serde(default)]
    pub note: Option<String>,
}

/// 单个任务（每任务一个 `<id>.json`）。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Task {
    pub schema: i64,
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// 1（最高）– 4（最低）。
    #[serde(default = "default_priority")]
    pub priority: i64,
    pub state: String,
    #[serde(default)]
    pub labels: Vec<String>,
    /// 最多一层嵌套：父任务不允许再有 `parent_id`。
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub sub_order: i64,
    /// 排期只是 `todo` 上的属性，不单独设 `scheduled` 状态。
    #[serde(default)]
    pub scheduled_at: Option<i64>,
    #[serde(default)]
    pub runs: Vec<TaskRun>,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub history: Vec<HistoryEntry>,
}

fn default_priority() -> i64 {
    3
}

impl Task {
    /// 状态合法性校验：只校验是否已知状态；手动改状态不做严格流转白
    /// 名单（放宽），但任何 state 变化必须记 history（见 [`Task::set_state`]）。
    pub fn is_known_state(state: &str) -> bool {
        STATES.contains(&state)
    }

    /// 切状态并记 history（from=旧状态）。状态未变化时什么都不做。
    pub fn set_state(&mut self, to: &str, actor: &str, note: Option<String>, now: i64) {
        if self.state == to {
            return;
        }
        let from = std::mem::replace(&mut self.state, to.to_string());
        self.history.push(HistoryEntry {
            at: now,
            from: Some(from),
            to: to.to_string(),
            actor: actor.to_string(),
            note,
        });
        self.updated_at = now;
    }

    /// 记一条不改变状态的 history（如排期变更、派发失败备注）。
    pub fn push_note(&mut self, actor: &str, note: String, now: i64) {
        self.history.push(HistoryEntry {
            at: now,
            from: None,
            to: self.state.clone(),
            actor: actor.to_string(),
            note: Some(note),
        });
        self.updated_at = now;
    }

    /// 当前状态可用的"下一步动作"（§2.1），给 UI 渲染动作区用。
    pub fn effective_actions(&self) -> Vec<&'static str> {
        match self.state.as_str() {
            "backlog" => vec!["to_todo", "edit", "cancel"],
            "todo" if self.scheduled_at.is_some() => {
                vec!["dispatch", "reschedule", "unschedule", "to_backlog"]
            }
            "todo" => vec!["dispatch", "schedule", "to_backlog"],
            "in_progress" => vec!["view_session", "abort"],
            "human_review" => vec!["view_session", "approve", "rework"],
            "rework" => vec!["dispatch", "edit"],
            "merging" => vec!["view_session", "mark_done"],
            "done" | "cancelled" => vec!["reopen"],
            _ => vec![],
        }
    }
}

/// 看板元信息（`board.json`）。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BoardMeta {
    pub schema: i64,
    pub project: String,
    pub id_prefix: String,
    pub next_seq: i64,
    pub states: Vec<String>,
    pub active_states: Vec<String>,
    pub terminal_states: Vec<String>,
}

impl BoardMeta {
    /// 默认值（§3.1）：id_prefix "LAT"，project 取 cwd 目录名。
    fn default_for(project: String) -> Self {
        Self {
            schema: SCHEMA,
            project,
            id_prefix: DEFAULT_ID_PREFIX.to_string(),
            next_seq: FIRST_SEQ,
            states: STATES.iter().map(|s| s.to_string()).collect(),
            active_states: ACTIVE_STATES.iter().map(|s| s.to_string()).collect(),
            terminal_states: TERMINAL_STATES.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// 列表/详情返回视图：Task + 子任务聚合 + 当前可用动作。
#[derive(Serialize, Clone, Debug)]
pub struct TaskView {
    #[serde(flatten)]
    pub task: Task,
    /// 子任务总数（非子任务本身也有，恒 0）。
    pub sub_total: usize,
    /// 子任务中已终态（done/cancelled）的数量。
    pub sub_done: usize,
    /// 子任务各状态计数。
    pub sub_state_counts: BTreeMap<String, usize>,
    pub actions: Vec<&'static str>,
}

// ─── TaskStore ────────────────────────────────────────────────────

/// 目录扫描 + 内存索引 + 原子写 + `next_seq` 分配（单进程串行）。
/// 所有写操作：先改内存，再原子写文件（`<file>.tmp` 后 rename）。
pub struct TaskStore {
    dir: PathBuf,
    meta: BoardMeta,
    tasks: HashMap<String, Task>,
}

impl TaskStore {
    /// 加载 `<cwd>/.latte/tasks/`：目录不存在则创建；`board.json`
    /// 缺失时按默认值新建（project 取 cwd 目录名）。
    pub fn load(cwd: &Path) -> std::io::Result<Self> {
        let dir = cwd.join(".latte").join("tasks");
        std::fs::create_dir_all(&dir)?;

        let meta_path = dir.join("board.json");
        let meta = match std::fs::read_to_string(&meta_path) {
            Ok(raw) => serde_json::from_str::<BoardMeta>(&raw).unwrap_or_else(|_| {
                BoardMeta::default_for(project_name(cwd))
            }),
            Err(_) => {
                let m = BoardMeta::default_for(project_name(cwd));
                // 落一份默认 board.json（失败不阻塞启动）。
                let _ = write_atomic_json(&meta_path, &m);
                m
            }
        };

        let mut tasks = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let path = e.path();
                if path.extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                if path.file_name().and_then(|s| s.to_str()) == Some("board.json") {
                    continue;
                }
                if let Ok(raw) = std::fs::read_to_string(&path) {
                    if let Ok(t) = serde_json::from_str::<Task>(&raw) {
                        tasks.insert(t.id.clone(), t);
                    }
                }
            }
        }
        Ok(Self { dir, meta, tasks })
    }

    pub fn meta(&self) -> &BoardMeta {
        &self.meta
    }

    pub fn get(&self, id: &str) -> Option<&Task> {
        self.tasks.get(id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Task> {
        self.tasks.get_mut(id)
    }

    /// 全量列表（按 created_at 再按 id 排序，稳定输出）。
    pub fn list(&self) -> Vec<Task> {
        let mut out: Vec<Task> = self.tasks.values().cloned().collect();
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        out
    }

    /// 某任务的子任务（按 sub_order 排序）。
    pub fn children_of(&self, id: &str) -> Vec<Task> {
        let mut out: Vec<Task> = self
            .tasks
            .values()
            .filter(|t| t.parent_id.as_deref() == Some(id))
            .cloned()
            .collect();
        out.sort_by(|a, b| a.sub_order.cmp(&b.sub_order).then(a.id.cmp(&b.id)));
        out
    }

    /// 到期排期任务：`state == "todo" && scheduled_at <= now`。
    pub fn due_task_ids(&self, now: i64) -> Vec<String> {
        let mut out: Vec<String> = self
            .tasks
            .values()
            .filter(|t| t.state == "todo" && t.scheduled_at.is_some_and(|s| s <= now))
            .map(|t| t.id.clone())
            .collect();
        out.sort();
        out
    }

    /// 新建任务：分配 id = `{id_prefix}-{next_seq}`（board.json 同样
    /// 原子写），记创建 history（actor=user），原子写任务文件。
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        &mut self,
        title: &str,
        description: &str,
        priority: i64,
        labels: Vec<String>,
        parent_id: Option<String>,
        scheduled_at: Option<i64>,
    ) -> Result<Task, String> {
        if title.trim().is_empty() {
            return Err("title 不能为空".into());
        }
        if !(1..=4).contains(&priority) {
            return Err(format!("priority 必须在 1-4 之间，收到 {priority}"));
        }
        // 父子规则（§3.3，最多一层）。
        if let Some(pid) = &parent_id {
            let parent = self
                .tasks
                .get(pid)
                .ok_or_else(|| format!("parent_id {pid:?} 不存在"))?;
            if parent.parent_id.is_some() {
                return Err(format!("最多一层嵌套：{pid} 本身已是子任务"));
            }
        }
        let now = now_ms();
        let id = format!("{}-{}", self.meta.id_prefix, self.meta.next_seq);
        self.meta.next_seq += 1;
        let sub_order = parent_id
            .as_ref()
            .map(|pid| {
                self.children_of(pid)
                    .iter()
                    .map(|t| t.sub_order)
                    .max()
                    .unwrap_or(-1)
                    + 1
            })
            .unwrap_or(0);
        let task = Task {
            schema: SCHEMA,
            id: id.clone(),
            title: title.trim().to_string(),
            description: description.to_string(),
            priority,
            state: "backlog".to_string(),
            labels,
            parent_id,
            sub_order,
            scheduled_at,
            runs: vec![],
            created_at: now,
            updated_at: now,
            history: vec![HistoryEntry {
                at: now,
                from: None,
                to: "backlog".to_string(),
                actor: "user".to_string(),
                note: Some("任务创建".to_string()),
            }],
        };
        self.tasks.insert(id.clone(), task.clone());
        self.save_task(&task)?;
        self.save_meta()?;
        Ok(task)
    }

    /// 把内存中的任务原子写回 `<id>.json`。
    pub fn save_task(&self, task: &Task) -> Result<(), String> {
        write_atomic_json(&self.dir.join(format!("{}.json", task.id)), task)
    }

    /// 内存里的 `id` 当前值落盘（配合 `get_mut` 原地改后用）。
    pub fn persist(&self, id: &str) -> Result<(), String> {
        let t = self
            .tasks
            .get(id)
            .ok_or_else(|| format!("task {id:?} 不存在"))?;
        self.save_task(t)
    }

    fn save_meta(&self) -> Result<(), String> {
        write_atomic_json(&self.dir.join("board.json"), &self.meta)
    }

    /// 删除：文件移入 `archive/` 子目录并从内存移除。
    pub fn delete(&mut self, id: &str) -> Result<(), String> {
        if self.tasks.remove(id).is_none() {
            return Err(format!("task {id:?} 不存在"));
        }
        let src = self.dir.join(format!("{id}.json"));
        if src.exists() {
            let archive = self.dir.join("archive");
            std::fs::create_dir_all(&archive)
                .map_err(|e| format!("create {}: {e}", archive.display()))?;
            let dst = archive.join(format!("{id}.json"));
            std::fs::rename(&src, &dst)
                .map_err(|e| format!("archive {}: {e}", src.display()))?;
        }
        Ok(())
    }

    /// 给已有任务设置 parent 时的校验（v1 的 update 不改 parent，仅
    /// 备后续用）：parent 必须存在且本身无 parent；本任务不能有子任务。
    pub fn validate_reparent(&self, id: &str, parent_id: &str) -> Result<(), String> {
        let parent = self
            .tasks
            .get(parent_id)
            .ok_or_else(|| format!("parent_id {parent_id:?} 不存在"))?;
        if parent.parent_id.is_some() {
            return Err(format!("最多一层嵌套：{parent_id} 本身已是子任务"));
        }
        if !self.children_of(id).is_empty() {
            return Err("该任务已有子任务，不能再挂到父任务下".to_string());
        }
        Ok(())
    }

    /// 组装 TaskView（含子任务聚合）。
    pub fn view(&self, task: &Task) -> TaskView {
        let children = self.children_of(&task.id);
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut done = 0;
        for c in &children {
            *counts.entry(c.state.clone()).or_insert(0) += 1;
            if TERMINAL_STATES.contains(&c.state.as_str()) {
                done += 1;
            }
        }
        TaskView {
            actions: task.effective_actions(),
            task: task.clone(),
            sub_total: children.len(),
            sub_done: done,
            sub_state_counts: counts,
        }
    }
}

fn project_name(cwd: &Path) -> String {
    cwd.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("project")
        .to_string()
}

/// 原子写 JSON：先写 `<file>.tmp` 再 rename（同 ui-sessions 约定）。
fn write_atomic_json<T: Serialize>(path: &Path, v: &T) -> Result<(), String> {
    let data = serde_json::to_vec_pretty(v).map_err(|e| format!("serialize: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &data).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))?;
    Ok(())
}

// ─── 请求体 ───────────────────────────────────────────────────────

/// `POST /api/tasks` 请求体。
#[derive(Deserialize)]
pub struct CreateTaskRequest {
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// 缺省 3（普通）。
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub scheduled_at: Option<i64>,
}

/// `PATCH /api/tasks/:id` 请求体：全 Option，缺省字段不变。
#[derive(Deserialize)]
pub struct TaskPatch {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub state: Option<String>,
    /// 双层 Option：缺省 = 不变；`null` = 清除排期；数值 = 设定排期。
    #[serde(default)]
    pub scheduled_at: Option<Option<i64>>,
    #[serde(default)]
    pub labels: Option<Vec<String>>,
}

/// `POST /api/tasks/:id/report` 请求体（manager 回报）。
#[derive(Deserialize)]
pub struct ReportTaskRequest {
    #[serde(default)]
    pub summary: String,
    /// `completed` / `aborted` / `failed` / `timeout`。
    pub result: String,
}

// ─── 协议无关 API（与 crate::api 同级） ───────────────────────────

fn map_store_err(e: String) -> ApiError {
    // store 错误统一当 400（参数/规则类）；持久化 IO 失败信息含
    // "write"/"rename"/"create" 等，归 500 更准，但为简单起见统一
    // 走 bad_request 之外的判断成本高于收益——IO 失败信息原样透出。
    if e.contains("不存在") {
        ApiError::not_found(e)
    } else {
        ApiError::bad_request(e)
    }
}

/// `GET /api/tasks` — 全量列表（含子任务聚合进度）。
pub fn list_tasks(b: &UiBackend) -> Vec<TaskView> {
    let store = b.tasks.read();
    store.list().iter().map(|t| store.view(t)).collect()
}

pub fn get_task(b: &UiBackend, id: &str) -> Result<TaskView, ApiError> {
    let store = b.tasks.read();
    let t = store
        .get(id)
        .ok_or_else(|| ApiError::not_found(format!("task {id:?} 不存在")))?;
    Ok(store.view(t))
}

pub fn create_task(b: &UiBackend, req: CreateTaskRequest) -> Result<TaskView, ApiError> {
    let mut store = b.tasks.write();
    let t = store
        .create(
            &req.title,
            &req.description,
            req.priority.unwrap_or(3),
            req.labels,
            req.parent_id,
            req.scheduled_at,
        )
        .map_err(map_store_err)?;
    Ok(store.view(&t))
}

pub fn update_task(b: &UiBackend, id: &str, patch: TaskPatch) -> Result<TaskView, ApiError> {
    let mut store = b.tasks.write();
    let now = now_ms();
    {
        let t = store
            .get_mut(id)
            .ok_or_else(|| ApiError::not_found(format!("task {id:?} 不存在")))?;
        if let Some(title) = patch.title {
            if title.trim().is_empty() {
                return Err(ApiError::bad_request("title 不能为空"));
            }
            t.title = title.trim().to_string();
        }
        if let Some(desc) = patch.description {
            t.description = desc;
        }
        if let Some(p) = patch.priority {
            if !(1..=4).contains(&p) {
                return Err(ApiError::bad_request(format!(
                    "priority 必须在 1-4 之间，收到 {p}"
                )));
            }
            t.priority = p;
        }
        if let Some(labels) = patch.labels {
            t.labels = labels;
        }
        if let Some(sched) = patch.scheduled_at {
            t.scheduled_at = sched;
        }
        if let Some(state) = patch.state {
            if !Task::is_known_state(&state) {
                return Err(ApiError::bad_request(format!("未知状态 {state:?}")));
            }
            // 手动改状态放宽（不做严格流转白名单），但必须记 history。
            t.set_state(&state, "user", None, now);
            // 改 state 为非 todo 时清排期。
            if t.state != "todo" {
                t.scheduled_at = None;
            }
        }
        t.updated_at = now;
    }
    store.persist(id).map_err(ApiError::internal)?;
    let t = store.get(id).expect("刚 persist 的任务必然存在");
    Ok(store.view(t))
}

/// 派发消息模板（中文；文档 §5 以本函数为准）。
fn build_dispatch_message(
    id: &str,
    title: &str,
    priority: i64,
    description: &str,
    children: &[Task],
) -> String {
    let mut msg = format!("[任务 {id}] {title}\n优先级：P{priority}\n描述：{description}\n");
    if !children.is_empty() {
        msg.push_str("子任务：\n");
        for (i, c) in children.iter().enumerate() {
            msg.push_str(&format!("  {}. [{}] {}（{}）\n", i + 1, c.id, c.title, c.state));
        }
    }
    msg.push_str("请组织团队完成该任务，完成后给出结果摘要。");
    msg
}

/// `POST /api/tasks/:id/dispatch` — 立即执行（或 rework 重新派发）：
/// 新建 ui-session → 打 label → 发首条消息给 manager →
/// state → in_progress，runs 追加一条。`actor` ∈ `user` / `scheduler`。
pub async fn dispatch_task(b: &UiBackend, id: &str, actor: &str) -> Result<TaskView, ApiError> {
    // 1. 读锁内校验 + 收集消息素材（不持锁跨 await）。
    let (title, priority, description, children) = {
        let store = b.tasks.read();
        let t = store
            .get(id)
            .ok_or_else(|| ApiError::not_found(format!("task {id:?} 不存在")))?;
        if t.state != "todo" && t.state != "rework" {
            return Err(ApiError::bad_request(format!(
                "state {:?} 不可派发（仅 todo/rework 可派发）",
                t.state
            )));
        }
        (
            t.title.clone(),
            t.priority,
            t.description.clone(),
            store.children_of(id),
        )
    };

    // 2. 建 session + label + 首条消息。任何一步失败都不改任务状态。
    let info = api::create_session(b).await?;
    let label = format!("[{id}] {}", title.chars().take(20).collect::<String>());
    api::set_session_label(b, &info.session_id, &label)?;
    let msg = build_dispatch_message(id, &title, priority, &description, &children);
    api::chat_send(b, Some(&info.session_id), &msg).await?;

    // 3. 写锁更新状态（任务可能已被并发删除 → 404，session 留着无害）。
    let now = now_ms();
    let mut store = b.tasks.write();
    {
        let t = store
            .get_mut(id)
            .ok_or_else(|| ApiError::not_found(format!("task {id:?} 不存在")))?;
        t.set_state("in_progress", actor, None, now);
        t.scheduled_at = None;
        t.runs.push(TaskRun {
            session_id: info.session_id.clone(),
            started_at: now,
            ended_at: None,
            result: None,
        });
        t.updated_at = now;
    }
    store.persist(id).map_err(ApiError::internal)?;
    let t = store.get(id).expect("刚 persist 的任务必然存在");
    Ok(store.view(t))
}

/// `POST /api/tasks/:id/abort` — 中止执行：对当前 run 的 session 调
/// 现有 `chat_abort`，run 补 ended_at/result=aborted，state → todo。
pub async fn abort_task(b: &UiBackend, id: &str) -> Result<TaskView, ApiError> {
    let session_id = {
        let store = b.tasks.read();
        let t = store
            .get(id)
            .ok_or_else(|| ApiError::not_found(format!("task {id:?} 不存在")))?;
        if t.state != "in_progress" {
            return Err(ApiError::bad_request(format!(
                "state {:?} 不可中止（仅 in_progress）",
                t.state
            )));
        }
        t.runs
            .last()
            .filter(|r| r.ended_at.is_none())
            .map(|r| r.session_id.clone())
            .ok_or_else(|| ApiError::bad_request("没有进行中的 run"))?
    };

    // session 可能已被用户删掉：中止失败不阻塞任务状态回退。
    let _ = api::chat_abort(b, Some(&session_id)).await;

    let now = now_ms();
    let mut store = b.tasks.write();
    {
        let t = store
            .get_mut(id)
            .ok_or_else(|| ApiError::not_found(format!("task {id:?} 不存在")))?;
        if let Some(run) = t.runs.last_mut().filter(|r| r.ended_at.is_none()) {
            run.ended_at = Some(now);
            run.result = Some("aborted".to_string());
        }
        t.set_state("todo", "user", Some("中止执行".to_string()), now);
        t.updated_at = now;
    }
    store.persist(id).map_err(ApiError::internal)?;
    let t = store.get(id).expect("刚 persist 的任务必然存在");
    Ok(store.view(t))
}

/// `POST /api/tasks/:id/report` — manager 回报：run 补 ended_at/result；
/// completed → human_review，其他 → todo。history actor=manager。
pub fn report_task(
    b: &UiBackend,
    id: &str,
    summary: &str,
    result: &str,
) -> Result<TaskView, ApiError> {
    const RESULTS: [&str; 4] = ["completed", "aborted", "failed", "timeout"];
    if !RESULTS.contains(&result) {
        return Err(ApiError::bad_request(format!(
            "result 必须是 {RESULTS:?} 之一，收到 {result:?}"
        )));
    }
    let now = now_ms();
    let mut store = b.tasks.write();
    {
        let t = store
            .get_mut(id)
            .ok_or_else(|| ApiError::not_found(format!("task {id:?} 不存在")))?;
        let run = t
            .runs
            .last_mut()
            .filter(|r| r.ended_at.is_none())
            .ok_or_else(|| ApiError::bad_request("没有进行中的 run 可回报"))?;
        run.ended_at = Some(now);
        run.result = Some(result.to_string());
        let next = if result == "completed" {
            "human_review"
        } else {
            "todo"
        };
        let note = if summary.trim().is_empty() {
            None
        } else {
            Some(summary.chars().take(200).collect::<String>())
        };
        t.set_state(next, "manager", note, now);
        t.updated_at = now;
    }
    store.persist(id).map_err(ApiError::internal)?;
    let t = store.get(id).expect("刚 persist 的任务必然存在");
    Ok(store.view(t))
}

/// `DELETE /api/tasks/:id` — 文件移入 `archive/` 并从内存移除。
pub fn delete_task(b: &UiBackend, id: &str) -> Result<(), ApiError> {
    let mut store = b.tasks.write();
    store.delete(id).map_err(map_store_err)
}

// ─── Scheduler ────────────────────────────────────────────────────

/// 看板 scheduler（§4）：启动时先立即扫一遍（补发关机期间错过的排
/// 期），之后每 5s 扫一次，到期任务逐个派发（history actor=scheduler）。
pub async fn scheduler_loop(b: std::sync::Arc<UiBackend>) {
    scan_and_dispatch(&b).await;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        scan_and_dispatch(&b).await;
    }
}

async fn scan_and_dispatch(b: &std::sync::Arc<UiBackend>) {
    let now = now_ms();
    let due: Vec<String> = b.tasks.read().due_task_ids(now);
    for id in due {
        // 派发前重读内存再校验一次（symphony dispatch_issue 的
        // re-fetch 防 stale）。
        let still_due = b
            .tasks
            .read()
            .get(&id)
            .map(|t| t.state == "todo" && t.scheduled_at.is_some_and(|s| s <= now))
            .unwrap_or(false);
        if !still_due {
            continue;
        }
        if let Err(e) = dispatch_task(b, &id, "scheduler").await {
            eprintln!("[task-scheduler] dispatch {id} failed ({}): {}", e.status, e.message);
            // 失败防刷屏：清掉排期，否则每 5s 重试一次且每次新建 session。
            // 用户看到 history 备注后可重新排期或手动执行。
            let mut store = b.tasks.write();
            if let Some(t) = store.get_mut(&id) {
                t.scheduled_at = None;
                t.push_note(
                    "scheduler",
                    format!("自动派发失败（{}），已取消排期", e.message),
                    now_ms(),
                );
                if let Err(pe) = store.persist(&id) {
                    eprintln!("[task-scheduler] persist {id} failed: {pe}");
                }
            }
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store() -> (tempfile::TempDir, TaskStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = TaskStore::load(dir.path()).expect("load");
        (dir, store)
    }

    fn make_task(store: &mut TaskStore, title: &str) -> Task {
        store
            .create(title, "desc", 2, vec![], None, None)
            .expect("create")
    }

    #[test]
    fn create_persists_and_reloads() {
        let (dir, mut store) = tmp_store();
        let t = make_task(&mut store, "实现 ringbuf");
        assert_eq!(t.id, "LAT-100");
        assert_eq!(t.state, "backlog");
        assert_eq!(t.priority, 2);
        // 落盘文件存在且内容可解析。
        let file = dir.path().join(".latte/tasks/LAT-100.json");
        let raw = std::fs::read_to_string(&file).expect("task file");
        let parsed: Task = serde_json::from_str(&raw).expect("parse task file");
        assert_eq!(parsed.title, "实现 ringbuf");
        // board.json 落盘。
        let board = std::fs::read_to_string(dir.path().join(".latte/tasks/board.json"))
            .expect("board.json");
        let meta: BoardMeta = serde_json::from_str(&board).expect("parse board");
        assert_eq!(meta.id_prefix, "LAT");
        // 重新加载 → 任务在内存里。
        let store2 = TaskStore::load(dir.path()).expect("reload");
        assert_eq!(store2.get("LAT-100").map(|t| t.title.as_str()), Some("实现 ringbuf"));
        assert_eq!(store2.meta().next_seq, 101);
    }

    #[test]
    fn next_seq_increments() {
        let (_dir, mut store) = tmp_store();
        let a = make_task(&mut store, "a");
        let b = make_task(&mut store, "b");
        assert_eq!(a.id, "LAT-100");
        assert_eq!(b.id, "LAT-101");
        assert_eq!(store.meta().next_seq, 102);
    }

    #[test]
    fn atomic_write_leaves_no_tmp_files() {
        let (dir, mut store) = tmp_store();
        make_task(&mut store, "x");
        let tasks_dir = dir.path().join(".latte/tasks");
        let tmps: Vec<_> = std::fs::read_dir(&tasks_dir)
            .expect("read_dir")
            .flatten()
            .filter(|e| e.path().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(tmps.is_empty(), "原子写不应留下 .tmp 文件: {tmps:?}");
    }

    #[test]
    fn parent_child_rules_enforced() {
        let (_dir, mut store) = tmp_store();
        let parent = make_task(&mut store, "父");
        let child = store
            .create("子", "", 3, vec![], Some(parent.id.clone()), None)
            .expect("create child");
        assert_eq!(child.parent_id.as_deref(), Some(parent.id.as_str()));
        assert_eq!(child.sub_order, 0);
        // parent 必须存在。
        let err = store
            .create("孤儿", "", 3, vec![], Some("LAT-999".into()), None)
            .unwrap_err();
        assert!(err.contains("不存在"), "{err}");
        // 拒绝第二层嵌套：parent_id 指向本身也是子任务的任务。
        let err = store
            .create("孙", "", 3, vec![], Some(child.id.clone()), None)
            .unwrap_err();
        assert!(err.contains("最多一层"), "{err}");
        // 拒绝给已有子任务的任务设 parent。
        let other = make_task(&mut store, "另一个父");
        let err = store.validate_reparent(&parent.id, &other.id);
        assert!(err.is_err(), "已有子任务的任务不能再设 parent: {err:?}");
        // sub_order 递增。
        let child2 = store
            .create("子2", "", 3, vec![], Some(parent.id.clone()), None)
            .expect("child2");
        assert_eq!(child2.sub_order, 1);
        // 聚合。
        let view = store.view(store.get(&parent.id).unwrap());
        assert_eq!(view.sub_total, 2);
        assert_eq!(view.sub_state_counts.get("backlog"), Some(&2));
        assert_eq!(view.sub_done, 0);
    }

    #[test]
    fn state_change_records_history() {
        let (_dir, mut store) = tmp_store();
        let t = make_task(&mut store, "h");
        let now = now_ms();
        store
            .get_mut(&t.id)
            .unwrap()
            .set_state("todo", "user", None, now);
        store.persist(&t.id).unwrap();
        let t = store.get(&t.id).unwrap().clone();
        assert_eq!(t.state, "todo");
        assert_eq!(t.history.len(), 2, "创建 1 条 + 变迁 1 条");
        let h = &t.history[1];
        assert_eq!(h.from.as_deref(), Some("backlog"));
        assert_eq!(h.to, "todo");
        assert_eq!(h.actor, "user");
        // 同值 set_state 不再记 history。
        let len_before = t.history.len();
        store.get_mut(&t.id).unwrap().set_state("todo", "user", None, now);
        assert_eq!(store.get(&t.id).unwrap().history.len(), len_before);
    }

    #[test]
    fn due_tasks_selects_only_due_todo() {
        let (_dir, mut store) = tmp_store();
        let now = now_ms();
        // 到期 todo
        let due = store
            .create("到期", "", 3, vec![], None, Some(now - 1000))
            .unwrap();
        store.get_mut(&due.id).unwrap().set_state("todo", "user", None, now);
        // 未到期 todo
        let future = store
            .create("未到期", "", 3, vec![], None, Some(now + 60_000))
            .unwrap();
        store.get_mut(&future.id).unwrap().set_state("todo", "user", None, now);
        // backlog 且已到期（不应选出：只派 todo）
        store
            .create("backlog到期", "", 3, vec![], None, Some(now - 1000))
            .unwrap();
        store.persist(&due.id).unwrap();
        store.persist(&future.id).unwrap();
        let ids = store.due_task_ids(now);
        assert_eq!(ids, vec![due.id.clone()]);
    }

    #[test]
    fn delete_moves_file_to_archive() {
        let (dir, mut store) = tmp_store();
        let t = make_task(&mut store, "删我");
        let file = dir.path().join(".latte/tasks").join(format!("{}.json", t.id));
        assert!(file.exists());
        store.delete(&t.id).expect("delete");
        assert!(store.get(&t.id).is_none(), "内存中应移除");
        assert!(!file.exists(), "原位置文件应移走");
        let archived = dir
            .path()
            .join(".latte/tasks/archive")
            .join(format!("{}.json", t.id));
        assert!(archived.exists(), "文件应在 archive/ 下");
        // 删不存在的任务 → 错误。
        assert!(store.delete("LAT-999").is_err());
        // 重新加载不应恢复已归档任务。
        let store2 = TaskStore::load(dir.path()).expect("reload");
        assert!(store2.get(&t.id).is_none());
    }
}
