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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use latte_agent_core::workflow::{load_workflow, run_workflow, WorkflowRunContext};
use latte_ai::params::GenerateParams;
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
    /// 绑定的 workflow 名：派发时直接跑该 workflow（不走 manager 派发）。
    /// 旧落盘文件无此字段 → `None`。
    #[serde(default)]
    pub workflow: Option<String>,
    /// 任务声明涉及的文件/目录前缀（相对项目根），用于派发时的跨族
    /// 文件范围互斥（见 [`path_running_conflict`]）。空 = 未声明，
    /// 不参与互斥判定（无法证明冲突）。旧落盘文件无此字段 → 空。
    #[serde(default)]
    pub paths: Vec<String>,
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
    /// 进行中的 workflow run 的取消旗标（纯内存态，不落盘）：dispatch
    /// 绑定 workflow 的任务时登记，run 结束或 abort 时清除。abort_task
    /// 置位后 `run_workflow` 在下一个 step/speaker 边界退出。
    workflow_cancels: HashMap<String, Arc<AtomicBool>>,
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
        Ok(Self { dir, meta, tasks, workflow_cancels: HashMap::new() })
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
    /// 原子写），记创建 history（actor 由调用方给：user / import /
    /// scheduler），原子写任务文件。
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        &mut self,
        title: &str,
        description: &str,
        priority: i64,
        labels: Vec<String>,
        parent_id: Option<String>,
        scheduled_at: Option<i64>,
        workflow: Option<String>,
        actor: &str,
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
            workflow,
            paths: vec![],
            runs: vec![],
            created_at: now,
            updated_at: now,
            history: vec![HistoryEntry {
                at: now,
                from: None,
                to: "backlog".to_string(),
                actor: actor.to_string(),
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

    /// 登记一个进行中 workflow run 的取消旗标（dispatch 绑定
    /// workflow 的任务时调用）。
    pub fn register_workflow_cancel(&mut self, id: &str, flag: Arc<AtomicBool>) {
        self.workflow_cancels.insert(id.to_string(), flag);
    }

    /// 取出并移除某任务的 workflow 取消旗标（abort / run 结束清理）。
    /// `None` = 该任务当前没有进行中的 workflow run。
    pub fn take_workflow_cancel(&mut self, id: &str) -> Option<Arc<AtomicBool>> {
        self.workflow_cancels.remove(id)
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
    /// 绑定的 workflow 名（必须存在于 `.latte/workflows.d`，否则 400）。
    #[serde(default)]
    pub workflow: Option<String>,
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
    /// 双层 Option：缺省 = 不变；`null` = 解绑 workflow；字符串 = 绑定。
    /// （plain `#[serde(default)]` 会把显式 `null` 折叠成"缺省"，
    /// 必须自定义 deserialize_with 才能区分三者。）
    #[serde(default, deserialize_with = "deserialize_nullable")]
    pub workflow: Option<Option<String>>,
}

/// 双层 Option 字段的反序列化：键存在即 `Some(值或null)`，缺失时由
/// `#[serde(default)]` 给 `None`。
fn deserialize_nullable<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// `POST /api/tasks/import` 请求体。
#[derive(Deserialize)]
pub struct ImportTasksRequest {
    pub tasks: Vec<ImportTask>,
    /// 来源 plan 提案 id（PlanProposed 事件携带）。`Some` 时导入成功
    /// 即视为用户批准该任务清单：对应 session 的 plan 阶段门从
    /// `PendingApproval` 置为 `Approved`，解除实现类 delegate 拦截。
    #[serde(default)]
    pub plan_id: Option<String>,
}

/// 导入的单个任务：除 `title` 外全部可选；`subtasks` 最多一层。
#[derive(Deserialize)]
pub struct ImportTask {
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub workflow: Option<String>,
    /// 任务涉及的文件/目录前缀（相对项目根）：派发时与在跑任务范围
    /// 重叠会被拒绝（409）。空 = 未声明。
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub subtasks: Vec<ImportTask>,
}

/// `POST /api/tasks/import` 响应：按创建顺序（父先于子）的任务 id。
#[derive(Serialize)]
pub struct ImportTasksResponse {
    pub created: Vec<String>,
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
    validate_workflow_name(&b.cwd, req.workflow.as_deref())?;
    let mut store = b.tasks.write();
    let t = store
        .create(
            &req.title,
            &req.description,
            req.priority.unwrap_or(3),
            req.labels,
            req.parent_id,
            req.scheduled_at,
            req.workflow,
            "user",
        )
        .map_err(map_store_err)?;
    Ok(store.view(&t))
}

/// 校验 workflow 名：`Some(name)` 时必须能在项目/全局
/// `workflows.d` 里找到，否则 400。
fn validate_workflow_name(cwd: &Path, name: Option<&str>) -> Result<(), ApiError> {
    if let Some(n) = name {
        if !crate::workflows::exists(cwd, n) {
            return Err(ApiError::bad_request(format!("unknown workflow '{n}'")));
        }
    }
    Ok(())
}

/// 导入单个任务（含一层子任务）的校验 + 落库。返回新建 id（父先于子）。
/// workflow 引用不存在时，静默降级为 null（不阻塞导入）。
fn import_one(
    store: &mut TaskStore,
    cwd: &Path,
    item: &ImportTask,
    created: &mut Vec<String>,
) -> Result<(), String> {
    let title = item.title.trim();
    if title.is_empty() {
        return Err("title 不能为空".to_string());
    }
    // 标题上限 80 字符（截断而非报错）。
    let title: String = title.chars().take(80).collect();
    if let Some(p) = item.priority {
        if !(1..=4).contains(&p) {
            return Err(format!("priority 必须在 1-4 之间，收到 {p}"));
        }
    }
    if item.subtasks.iter().any(|s| !s.subtasks.is_empty()) {
        return Err("subtasks nested deeper than one level".to_string());
    }
    // workflow 引用必须存在：静默降级会让任务丢掉绑定的流程而无人
    // 察觉——拒绝并报错，让提交方（manager/用户）修正后重试。
    let workflow = match item.workflow.as_ref() {
        Some(wf) if !crate::workflows::exists(cwd, wf) => {
            return Err(format!("unknown workflow '{wf}'"));
        }
        other => other.cloned(),
    };
    let parent = store.create(
        &title,
        &item.description,
        item.priority.unwrap_or(3),
        item.labels.clone(),
        None,
        None,
        workflow,
        "import",
    )?;
    if !item.paths.is_empty() {
        store.get_mut(&parent.id).expect("刚创建").paths = item.paths.clone();
        store.persist(&parent.id)?;
    }
    created.push(parent.id.clone());
    for sub in &item.subtasks {
        let stitle = sub.title.trim();
        if stitle.is_empty() {
            return Err(format!("子任务 title 不能为空（父任务 '{title}'）"));
        }
        let stitle: String = stitle.chars().take(80).collect();
        if let Some(p) = sub.priority {
            if !(1..=4).contains(&p) {
                return Err(format!("priority 必须在 1-4 之间，收到 {p}"));
            }
        }
        // 子任务 workflow 引用不存在时同样降级
        let sub_workflow = sub.workflow.as_ref().and_then(|wf| {
            if crate::workflows::exists(cwd, wf) {
                Some(wf.clone())
            } else {
                None
            }
        });
        let child = store.create(
            &stitle,
            &sub.description,
            sub.priority.unwrap_or(3),
            sub.labels.clone(),
            Some(parent.id.clone()),
            None,
            sub_workflow,
            "import",
        )?;
        if !sub.paths.is_empty() {
            store.get_mut(&child.id).expect("刚创建").paths = sub.paths.clone();
            store.persist(&child.id)?;
        }
        created.push(child.id.clone());
    }
    Ok(())
}

/// `POST /api/tasks/import` 的核心逻辑（可测）：顺序创建，首个失败即
/// 返回 400 语义的错误（消息含失败任务的 title 与已创建的 id）。
fn import_tasks_into(
    store: &mut TaskStore,
    cwd: &Path,
    req: &ImportTasksRequest,
) -> Result<Vec<String>, String> {
    let mut created: Vec<String> = Vec::new();
    for item in &req.tasks {
        if let Err(e) = import_one(store, cwd, item, &mut created) {
            return Err(format!(
                "导入任务 {:?} 失败：{e}（已创建：{created:?}）",
                item.title.trim()
            ));
        }
    }
    Ok(created)
}

pub fn import_tasks(
    b: &UiBackend,
    req: ImportTasksRequest,
) -> Result<ImportTasksResponse, ApiError> {
    let mut store = b.tasks.write();
    let created = import_tasks_into(&mut store, &b.cwd, &req).map_err(ApiError::bad_request)?;
    drop(store);
    // plan 阶段门：带 plan_id 的导入 = 用户批准该任务清单。找到持有
    // 该 PendingApproval 的 session（plan 弹窗属于某个 session，stage
    // 按 session 存），置 Approved 解除实现类 delegate 拦截。
    if let Some(plan_id) = &req.plan_id {
        approve_plan_stage(b, plan_id);
    }
    Ok(ImportTasksResponse { created })
}

/// 把持有 `PendingApproval { plan_id }` 的 session 的 plan 阶段门置为
/// `Approved`。只迁移精确匹配该 plan_id 且仍在等批准的 session——
/// 已被下一条用户消息复位（Normal）或批准的是别的 plan 的不动。
fn approve_plan_stage(b: &UiBackend, plan_id: &str) {
    use latte_agent_core::controller::PlanStage;
    for handle in b.sessions.read().values() {
        let Some(controller) = handle.try_controller() else {
            continue;
        };
        if controller.plan_stage()
            == (PlanStage::PendingApproval {
                plan_id: plan_id.to_string(),
            })
        {
            controller.set_plan_stage(PlanStage::Approved {
                plan_id: plan_id.to_string(),
            });
        }
    }
}

pub fn update_task(b: &UiBackend, id: &str, patch: TaskPatch) -> Result<TaskView, ApiError> {
    let mut hook: Option<&str> = None;
    let mut hook_topic: Option<String> = None;
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
        if let Some(wf) = patch.workflow {
            if let Some(name) = &wf {
                if !crate::workflows::exists(&b.cwd, name) {
                    return Err(ApiError::bad_request(format!("unknown workflow '{name}'")));
                }
            }
            t.workflow = wf;
        }
        if let Some(state) = patch.state {
            if !Task::is_known_state(&state) {
                return Err(ApiError::bad_request(format!("未知状态 {state:?}")));
            }
            // 手动改状态放宽（不做严格流转白名单），但必须记 history。
            let prev = t.state.clone();
            t.set_state(&state, "user", None, now);
            // 改 state 为非 todo 时清排期。
            if t.state != "todo" {
                t.scheduled_at = None;
            }
            // 生命周期钩子：进入 merging/done 时自动执行对应 workflow。
            if state != prev {
                match state.as_str() {
                    "merging" => hook = Some("merge"),
                    "done" => hook = Some("task_archive"),
                    _ => {}
                }
            }
        }
        t.updated_at = now;
        if hook.is_some() {
            hook_topic = Some(build_hook_topic(t));
        }
    }
    store.persist(id).map_err(ApiError::internal)?;
    // 父子联动：本次变更若使子任务全部 done，父任务自动完成。
    {
        let snap = store.get(id).expect("刚 persist 的任务必然存在").clone();
        maybe_complete_parent(&mut store, &snap, now);
    }
    let t = store.get(id).expect("刚 persist 的任务必然存在");
    let view = store.view(t);
    drop(store);
    if let (Some(wf_name), Some(topic)) = (hook, hook_topic) {
        spawn_lifecycle_hook(b, id, wf_name, topic);
    }
    Ok(view)
}

/// 钩子工作流的 topic：任务本体 + 最近的审查/回报记录（取末尾控制长度）。
fn build_hook_topic(t: &Task) -> String {
    let mut topic = format!("[任务 {}] {}\n描述：{}\n", t.id, t.title, t.description);
    let notes: Vec<String> = t
        .history
        .iter()
        .rev()
        .filter_map(|h| h.note.clone())
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if !notes.is_empty() {
        topic.push_str(&format!("\n【执行与审查记录】\n{}", notes.join("\n---\n")));
    }
    tail_chars(&topic, 4000)
}

/// 进入 merging/done 时后台执行对应 workflow（merge / task_archive）。
/// 结果记入任务历史（actor=workflow 名），**不改变状态**——merging→done
/// 的推进与 commit 动作仍由人完成。task_archive 额外把产出写入
/// `.latte/tasks/archive/<id>.md`。
fn spawn_lifecycle_hook(b: &UiBackend, id: &str, wf_name: &str, topic: String) {
    let b = b.clone();
    let id = id.to_string();
    let wf_name = wf_name.to_string();
    tokio::spawn(async move {
        let wf = match load_workflow(&wf_name, &b.cwd) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("[tasks] lifecycle hook load {wf_name}: {e}");
                return;
            }
        };
        // 事件通道：复用最后一次 run 的 session（对话里连续可见），
        // 没有则新建一个观察 session。
        let last_session = {
            let store = b.tasks.read();
            store
                .get(&id)
                .and_then(|t| t.runs.last().map(|r| r.session_id.clone()))
        };
        let event_tx = match &last_session {
            Some(sid) => match api::session_event_sender(&b, sid).await {
                Ok(tx) => tx,
                Err(e) => {
                    eprintln!("[tasks] hook session sender {sid}: {}", e.message);
                    return;
                }
            },
            None => {
                let info = match api::create_session(&b).await {
                    Ok(i) => i,
                    Err(e) => {
                        eprintln!("[tasks] hook create session: {}", e.message);
                        return;
                    }
                };
                let _ = api::set_session_label(&b, &info.session_id, &format!("[{id}] {wf_name}"));
                match api::session_event_sender(&b, &info.session_id).await {
                    Ok(tx) => tx,
                    Err(e) => {
                        eprintln!("[tasks] hook new session sender: {}", e.message);
                        return;
                    }
                }
            }
        };
        let ctx = WorkflowRunContext {
            merged: Arc::new(b.merged.read().clone()),
            resolver: b.resolver.clone(),
            default_params: GenerateParams::default(),
            cwd: b.cwd.clone(),
            event_tx,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            depth: 0,
        };
        let result = run_workflow(&wf, &topic, &ctx).await;
        let mut store = b.tasks.write();
        if let Some(t) = store.get_mut(&id) {
            let note = match &result {
                Ok(s) => format!("{wf_name}：{}", tail_chars(s, 500)),
                Err(e) => format!("{wf_name} 未完成：{}", tail_chars(e, 300)),
            };
            let now = now_ms();
            t.history.push(HistoryEntry {
                at: now,
                from: None,
                to: t.state.clone(),
                actor: wf_name.clone(),
                note: Some(note),
            });
            t.updated_at = now;
        }
        if wf_name == "task_archive" {
            if let Ok(record) = &result {
                let adir = store.dir.join("archive");
                if let Err(e) = std::fs::create_dir_all(&adir)
                    .and_then(|_| std::fs::write(adir.join(format!("{id}.md")), record))
                {
                    eprintln!("[tasks] write archive {id}.md: {e}");
                }
            }
        }
        if let Err(e) = store.persist(&id) {
            eprintln!("[tasks] persist {id} after hook {wf_name}: {e}");
        }
    });
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
///
/// 绑定了 workflow 的任务（`task.workflow = Some`）不发 manager 消息：
/// 直接在该 session 上跑 `run_workflow`（事件进 session 的 broadcast，
/// SSE/归档照常可见），后台跑完后按 report 语义自动迁移状态
/// （actor=workflow，见 [`apply_workflow_finish`]）。
pub async fn dispatch_task(b: &UiBackend, id: &str, actor: &str) -> Result<TaskView, ApiError> {
    // 1. 读锁内校验 + 收集消息素材（不持锁跨 await）。
    let (title, priority, description, children, workflow, prev_state, recent_notes) = {
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
        // 同家族（父 + 兄弟）互斥：plan 拆出的兄弟任务常改同一批文件，
        // 并行执行会互相覆盖（自优化实测发现的问题 3）。
        if let Some(running_id) = family_running_conflict(&store, id) {
            return Err(ApiError {
                status: 409,
                message: format!(
                    "同族任务 {running_id} 正在执行中，为避免冲突请等它完成后再派发"
                ),
            });
        }
        // 跨族文件范围互斥：本任务声明的 paths 与在跑任务重叠时拒绝，
        // 防止两个无亲缘任务并行改同一批文件互相覆盖。
        if let Some((running_id, (mine, theirs))) = path_running_conflict(&store, id) {
            return Err(ApiError {
                status: 409,
                message: format!(
                    "任务 {running_id} 正在执行中且文件范围重叠（本任务 {mine:?} 与对方 {theirs:?}），为避免互相覆盖请等它完成后再派发"
                ),
            });
        }
        // 最近的反馈记录（code_review 终审 / workflow 回报），rework 时喂给返工流。
        let notes: Vec<String> = t
            .history
            .iter()
            .rev()
            .filter_map(|h| h.note.clone())
            .take(3)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        (
            t.title.clone(),
            t.priority,
            t.description.clone(),
            store.children_of(id),
            t.workflow.clone(),
            t.state.clone(),
            notes,
        )
    };

    // 2. 建 session + label。任何一步失败都不改任务状态。
    let info = api::create_session(b).await?;
    let label = format!("[{id}] {}", title.chars().take(20).collect::<String>());
    api::set_session_label(b, &info.session_id, &label)?;
    let base_msg = build_dispatch_message(id, &title, priority, &description, &children);

    // 3. workflow 绑定分支：加载 workflow 并准备运行上下文；加载失败
    //    → 400（不回退到 manager 派发，也不改任务状态）。非绑定任务
    //    走经典路径：发首条消息给 manager。
    //    rework 状态改用 rework 返工流（带审查反馈），而不是原样重跑开发流。
    let wf_run = if let Some(bound_name) = &workflow {
        let (wf_name, msg) = if prev_state == "rework" {
            let feedback = if recent_notes.is_empty() {
                String::new()
            } else {
                format!("\n\n【审查反馈与执行历史】\n{}", recent_notes.join("\n---\n"))
            };
            ("rework".to_string(), format!("{base_msg}{feedback}"))
        } else {
            (bound_name.clone(), base_msg.clone())
        };
        let wf = load_workflow(&wf_name, &b.cwd).map_err(|e| {
            ApiError::bad_request(format!("workflow '{wf_name}' 加载失败：{e}"))
        })?;
        let event_tx = api::session_event_sender(b, &info.session_id).await?;
        Some((wf, event_tx, Arc::new(AtomicBool::new(false)), msg))
    } else {
        api::chat_send(b, Some(&info.session_id), &base_msg).await?;
        None
    };

    // 4. 写锁更新状态（任务可能已被并发删除 → 404，session 留着无害）。
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
    // 取消旗标与状态迁移同一把写锁登记：abort 永远不会先于登记看到
    // in_progress。
    if let Some((_, _, cancel, _)) = &wf_run {
        store.register_workflow_cancel(id, cancel.clone());
    }
    store.persist(id).map_err(ApiError::internal)?;
    let view = store.view(store.get(id).expect("刚 persist 的任务必然存在"));
    drop(store);

    // 5. 后台跑 workflow（不阻塞 HTTP 响应），跑完自动回报。
    if let Some((wf, event_tx, cancel, msg)) = wf_run {
        let b2 = b.clone();
        let task_id = id.to_string();
        let msg2 = msg.clone();
        tokio::spawn(async move {
            let ctx = WorkflowRunContext {
                merged: Arc::new(b2.merged.read().clone()),
                resolver: b2.resolver.clone(),
                default_params: GenerateParams::default(),
                cwd: b2.cwd.clone(),
                event_tx: event_tx.clone(),
                cancel_flag: cancel.clone(),
                depth: 0,
            };
            let result = run_workflow(&wf, &msg2, &ctx).await;
            // 开发流跑完（非 code_review 本身）→ 链式自动审查。
            let chain_review = result.is_ok() && wf.name != "code_review";
            let dev_summary = result.as_ref().ok().cloned().unwrap_or_default();
            finish_workflow_run(&b2, &task_id, result, &cancel);
            if chain_review {
                chain_code_review(&b2, &task_id, msg2, dev_summary, event_tx).await;
            }
        });
    }
    Ok(view)
}

/// 开发 workflow 跑完（→ human_review）后，在同一 session 自动执行
/// code_review 审查流：机器审查在人工把关之前给出终审意见。
/// 审查结果只记入任务历史（actor=code_review），**不改变状态**——
/// 最终 approve/reject 仍由人做。任务已被人工移出 human_review 时
/// 跳过审查。
async fn chain_code_review(
    b: &UiBackend,
    id: &str,
    dispatch_msg: String,
    dev_summary: String,
    event_tx: tokio::sync::broadcast::Sender<latte_agent_core::controller::ChatEvent>,
) {
    let in_review = {
        let store = b.tasks.read();
        store.get(id).map(|t| t.state == "human_review").unwrap_or(false)
    };
    if !in_review {
        return;
    }
    let wf = match load_workflow("code_review", &b.cwd) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[tasks] chain code_review load: {e}");
            return;
        }
    };
    let topic = format!(
        "{dispatch_msg}\n\n【执行摘要】\n{}",
        tail_chars(&dev_summary, 1500)
    );
    let ctx = WorkflowRunContext {
        merged: Arc::new(b.merged.read().clone()),
        resolver: b.resolver.clone(),
        default_params: GenerateParams::default(),
        cwd: b.cwd.clone(),
        event_tx,
        cancel_flag: Arc::new(AtomicBool::new(false)),
        depth: 0,
    };
    let result = run_workflow(&wf, &topic, &ctx).await;
    let mut store = b.tasks.write();
    if let Some(t) = store.get_mut(id) {
        let note = match &result {
            Ok(s) => format!("code_review 终审：{}", tail_chars(s, 500)),
            Err(e) => format!("code_review 未完成：{}", tail_chars(e, 300)),
        };
        let now = now_ms();
        // 终审 ❌（不通过）且任务仍在 human_review → 自动打回 rework，
        // 带着审查反馈；✅/⚠️ 保持 human_review 由人把关。
        let auto_rework = matches!(&result, Ok(s) if verdict_is_reject(s))
            && t.state == "human_review";
        if auto_rework {
            t.set_state("rework", "code_review", Some(note), now);
        } else {
            t.history.push(HistoryEntry {
                at: now,
                from: None,
                to: t.state.clone(),
                actor: "code_review".into(),
                note: Some(note),
            });
        }
        t.updated_at = now;
        if let Err(e) = store.persist(id) {
            eprintln!("[tasks] persist {id} after code_review: {e}");
        }
    }
}

/// 判定 code_review 终审结论是否为"不通过"（❌）。
/// 终审 prompt 约定裁决在输出开头（✅/⚠️/❌），只看前 200 字符——
/// 避免意见表格里出现的 ❌ 误触发（⚠️ 有条件通过不打回）。
fn verdict_is_reject(summary: &str) -> bool {
    summary.chars().take(200).collect::<String>().contains('❌')
}

/// 状态变更后的家族联动：子任务全部 done → 父任务自动 done
/// （actor=workflow，附说明）。有子任务 cancelled 或仍非 done → 不动，
/// 由人决定。父子最多一层嵌套，不会递归。
fn maybe_complete_parent(store: &mut TaskStore, child: &Task, now: i64) {
    let Some(parent_id) = child.parent_id.clone() else {
        return;
    };
    let children = store.children_of(&parent_id);
    if children.is_empty() || !children.iter().all(|c| c.state == "done") {
        return;
    }
    if let Some(p) = store.get_mut(&parent_id) {
        if p.state != "done" {
            p.set_state("done", "workflow", Some("全部子任务已完成".into()), now);
            p.updated_at = now;
            if let Err(e) = store.persist(&parent_id) {
                eprintln!("[tasks] persist parent {parent_id}: {e}");
            }
        }
    }
}

/// 同家族互斥检查：plan 拆出的兄弟任务常改同一批文件，并行执行会
/// 互相覆盖。家族 = 父任务 + 其全部子任务。返回冲突中的 in_progress
/// 任务 id（无冲突 → None）。
fn family_running_conflict(store: &TaskStore, id: &str) -> Option<String> {
    let t = store.get(id)?;
    let family_ids: Vec<String> = match &t.parent_id {
        Some(pid) => {
            let mut v: Vec<String> = store.children_of(pid).into_iter().map(|c| c.id).collect();
            v.push(pid.clone());
            v
        }
        None => store.children_of(id).into_iter().map(|c| c.id).collect(),
    };
    family_ids.into_iter().find(|fid| {
        fid != id && store.get(fid).map(|c| c.state == "in_progress").unwrap_or(false)
    })
}

/// 规范化声明的路径范围：去 `./` 前缀、去尾部 `/`，压掉中间重复的
/// `/`（按路径段重组）。规范化后为空串（如 `"./"`、`"/"`）表示声明
/// 无效，参与判定时被跳过。
fn normalize_declared_path(p: &str) -> String {
    let mut s = p.trim();
    while let Some(rest) = s.strip_prefix("./") {
        s = rest;
    }
    s.split('/')
        .filter(|seg| !seg.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

/// a 是否覆盖 b：按**路径段**比较，a == b 或 a 是 b 的祖先目录。
/// `src/foo` 与 `src/foobar` 段不同，不算覆盖（字符串前缀判断会误判）。
fn path_covers(a: &str, b: &str) -> bool {
    let a_segs: Vec<&str> = a.split('/').filter(|s| !s.is_empty()).collect();
    let b_segs: Vec<&str> = b.split('/').filter(|s| !s.is_empty()).collect();
    a_segs.len() <= b_segs.len() && a_segs.iter().zip(&b_segs).all(|(x, y)| x == y)
}

/// 两个范围列表是否重叠：存在一对 (a, b) 满足 a 覆盖 b 或 b 覆盖 a。
/// 任一方为空 → 不冲突（未声明范围，无法证明会互相覆盖）。
fn paths_overlap(a: &[String], b: &[String]) -> bool {
    first_path_overlap(a, b).is_some()
}

/// 返回第一对重叠路径（双方规范化后的 (a, b)）；无重叠 → None。
fn first_path_overlap(a: &[String], b: &[String]) -> Option<(String, String)> {
    for x in a {
        let x = normalize_declared_path(x);
        if x.is_empty() {
            continue;
        }
        for y in b {
            let y = normalize_declared_path(y);
            if y.is_empty() {
                continue;
            }
            if path_covers(&x, &y) || path_covers(&y, &x) {
                return Some((x, y));
            }
        }
    }
    None
}

/// 跨族文件范围互斥：本任务声明的 paths 与任何 in_progress 任务的
/// paths 重叠时，返回 (冲突任务 id, (本任务路径, 对方路径))。
/// 与同族互斥互补：无亲缘关系的任务并行改同一批文件同样会互相
/// 覆盖，靠显式声明的 paths 拦住。
fn path_running_conflict(store: &TaskStore, id: &str) -> Option<(String, (String, String))> {
    let t = store.get(id)?;
    if t.paths.is_empty() {
        return None;
    }
    store.list().into_iter().find_map(|other| {
        if other.id == id || other.state != "in_progress" || other.paths.is_empty() {
            return None;
        }
        first_path_overlap(&t.paths, &other.paths).map(|pair| (other.id.clone(), pair))
    })
}

/// 截取尾部至多 n 个字符（workflow 摘要取末尾：结论通常在最后）。
fn tail_chars(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    let start = chars.len().saturating_sub(n);
    chars[start..].iter().collect()
}

/// workflow run 结束时的任务状态迁移（actor=workflow）：
/// Ok → completed → human_review（摘要取末尾 500 字符）；Err →
/// failed → todo；观察到 cancel → aborted → todo。run 已被
/// abort_task 收尾（ended_at 已填）时不动状态（返回 Ok 让调用方
/// 照常清理取消旗标）。
fn apply_workflow_finish(
    store: &mut TaskStore,
    id: &str,
    result: Result<String, String>,
    cancelled: bool,
    now: i64,
) -> Result<(), String> {
    let t = store.get_mut(id).ok_or_else(|| format!("task {id:?} 不存在"))?;
    if t.runs.last().filter(|r| r.ended_at.is_none()).is_none() {
        return Ok(());
    }
    let (res, summary) = match (result, cancelled) {
        (Ok(s), false) => ("completed", tail_chars(&s, 500)),
        (Ok(_), true) => ("aborted", String::new()),
        (Err(e), false) => ("failed", tail_chars(&e, 500)),
        (Err(e), true) => ("aborted", tail_chars(&e, 500)),
    };
    {
        let run = t.runs.last_mut().expect("刚校验过有未结束 run");
        run.ended_at = Some(now);
        run.result = Some(res.to_string());
    }
    let next = if res == "completed" { "human_review" } else { "todo" };
    let note = if summary.trim().is_empty() {
        None
    } else {
        Some(summary)
    };
    t.set_state(next, "workflow", note, now);
    t.updated_at = now;
    Ok(())
}

/// 后台 workflow 跑完后的收尾：迁移状态 + 落盘 + 清取消旗标。
/// 任务已被删 / persist 失败只记日志（都是正常竞态或锦上添花）。
fn finish_workflow_run(
    b: &UiBackend,
    id: &str,
    result: Result<String, String>,
    cancel: &Arc<AtomicBool>,
) {
    let cancelled = cancel.load(Ordering::SeqCst);
    let mut store = b.tasks.write();
    store.take_workflow_cancel(id);
    if let Err(e) = apply_workflow_finish(&mut store, id, result, cancelled, now_ms()) {
        eprintln!("[tasks] workflow finish {id}: {e}");
        return;
    }
    if let Err(e) = store.persist(id) {
        eprintln!("[tasks] persist {id} after workflow finish: {e}");
    }
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

    // workflow run：置取消旗标（run_workflow 在下一个 step/speaker
    // 边界退出，随后 finish_workflow_run 看到 run 已被下方收尾便不再
    // 重复迁移）。workflow 不占 chat turn，chat_abort 只会误杀托管
    // session 的 controller，故跳过。经典 run 维持原语义：session
    // 可能已被用户删掉，中止失败不阻塞任务状态回退。
    let wf_cancel = b.tasks.write().take_workflow_cancel(id);
    if let Some(flag) = wf_cancel {
        flag.store(true, Ordering::SeqCst);
    } else {
        let _ = api::chat_abort(b, Some(&session_id)).await;
    }

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
            .create(title, "desc", 2, vec![], None, None, None, "user")
            .expect("create")
    }

    /// plan 阶段门：import 带 plan_id 时，持有对应 PendingApproval 的
    /// session 的 controller 被置 Approved；不带 plan_id 不动 stage。
    #[tokio::test]
    async fn import_with_plan_id_approves_plan_stage() {
        use latte_agent_core::controller::PlanStage;

        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = latte_agent_core::AgentConfig::default();
        let resolver = latte_agent_core::ModelResolver::from_config(&cfg).expect("resolver");
        let b = UiBackend::new(crate::UiBackendConfig {
            agent_config: cfg,
            model_resolver: resolver,
            role: None,
            tier: None,
            model_id: None,
            cwd: Some(dir.path().to_path_buf()),
            agents_config: ".latte/agents.d".into(),
        })
        .expect("backend");

        // 两个 session：s1 持有 plan-manager-1 的 PendingApproval；
        // s2 等的是另一个 plan（不应被动）。
        let mut controllers = Vec::new();
        for sid in ["ui-s1", "ui-s2"] {
            let h = crate::sessions::create_session_handle(
                sid.into(),
                "manager",
                &b.merged,
                &b.resolver,
                &b.cwd,
                None,
                None,
                &b.subsession_store,
            )
            .await
            .expect("session handle");
            let c = h.try_controller().expect("spawned controller");
            b.sessions.write().insert(sid.to_string(), Arc::new(h));
            controllers.push(c);
        }
        controllers[0].set_plan_stage(PlanStage::PendingApproval {
            plan_id: "plan-manager-1".into(),
        });
        controllers[1].set_plan_stage(PlanStage::PendingApproval {
            plan_id: "plan-manager-2".into(),
        });

        // 带 plan_id 导入 → 仅 s1 被批准。
        let req: ImportTasksRequest = serde_json::from_value(serde_json::json!({
            "plan_id": "plan-manager-1",
            "tasks": [{ "title": "实现 ringbuf" }]
        }))
        .expect("parse req");
        let resp = import_tasks(&b, req).expect("import");
        assert_eq!(resp.created.len(), 1);
        assert_eq!(
            controllers[0].plan_stage(),
            PlanStage::Approved {
                plan_id: "plan-manager-1".into()
            }
        );
        assert_eq!(
            controllers[1].plan_stage(),
            PlanStage::PendingApproval {
                plan_id: "plan-manager-2".into()
            },
            "别的 plan 的 session 不应被批准"
        );

        // 不带 plan_id 导入 → stage 不动。
        controllers[1].set_plan_stage(PlanStage::Normal);
        let req: ImportTasksRequest = serde_json::from_value(serde_json::json!({
            "tasks": [{ "title": "无 plan 来源的手动导入" }]
        }))
        .expect("parse req");
        import_tasks(&b, req).expect("import");
        assert_eq!(controllers[1].plan_stage(), PlanStage::Normal);
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
            .create("子", "", 3, vec![], Some(parent.id.clone()), None, None, "user")
            .expect("create child");
        assert_eq!(child.parent_id.as_deref(), Some(parent.id.as_str()));
        assert_eq!(child.sub_order, 0);
        // parent 必须存在。
        let err = store
            .create("孤儿", "", 3, vec![], Some("LAT-999".into()), None, None, "user")
            .unwrap_err();
        assert!(err.contains("不存在"), "{err}");
        // 拒绝第二层嵌套：parent_id 指向本身也是子任务的任务。
        let err = store
            .create("孙", "", 3, vec![], Some(child.id.clone()), None, None, "user")
            .unwrap_err();
        assert!(err.contains("最多一层"), "{err}");
        // 拒绝给已有子任务的任务设 parent。
        let other = make_task(&mut store, "另一个父");
        let err = store.validate_reparent(&parent.id, &other.id);
        assert!(err.is_err(), "已有子任务的任务不能再设 parent: {err:?}");
        // sub_order 递增。
        let child2 = store
            .create("子2", "", 3, vec![], Some(parent.id.clone()), None, None, "user")
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
            .create("到期", "", 3, vec![], None, Some(now - 1000), None, "user")
            .unwrap();
        store.get_mut(&due.id).unwrap().set_state("todo", "user", None, now);
        // 未到期 todo
        let future = store
            .create("未到期", "", 3, vec![], None, Some(now + 60_000), None, "user")
            .unwrap();
        store.get_mut(&future.id).unwrap().set_state("todo", "user", None, now);
        // backlog 且已到期（不应选出：只派 todo）
        store
            .create("backlog到期", "", 3, vec![], None, Some(now - 1000), None, "user")
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

    // ─── workflow 绑定 + 导入 ──────────────────────────────────────

    /// 旧落盘文件（无 `workflow` 字段）必须能反序列化 → None。
    #[test]
    fn task_without_workflow_field_deserializes() {
        let raw = r#"{
            "schema": 1,
            "id": "LAT-100",
            "title": "旧任务",
            "priority": 3,
            "state": "backlog",
            "created_at": 1,
            "updated_at": 1
        }"#;
        let t: Task = serde_json::from_str(raw).expect("parse old task json");
        assert_eq!(t.workflow, None);
        assert_eq!(t.description, "");
        assert!(t.labels.is_empty());
    }

    /// TaskPatch.workflow 双层 Option：缺省 = 不变，null = 解绑，字符串 = 绑定。
    #[test]
    fn patch_workflow_double_option_semantics() {
        let p: TaskPatch = serde_json::from_str("{}").expect("empty patch");
        assert!(p.workflow.is_none(), "缺省 = 不变");
        let p: TaskPatch = serde_json::from_str(r#"{"workflow": null}"#).expect("null patch");
        assert_eq!(p.workflow, Some(None), "null = 解绑");
        let p: TaskPatch =
            serde_json::from_str(r#"{"workflow": "tdd_development"}"#).expect("set patch");
        assert_eq!(
            p.workflow,
            Some(Some("tdd_development".to_string())),
            "字符串 = 绑定"
        );
    }

    /// 项目 `.latte/workflows.d/<name>.toml` fixture（名字必须满足
    /// workflow 命名规则，内容是合法 WorkflowDef）。
    fn write_workflow_fixture(cwd: &Path, name: &str) {
        let dir = cwd.join(".latte/workflows.d");
        std::fs::create_dir_all(&dir).expect("mkdir workflows.d");
        std::fs::write(
            dir.join(format!("{name}.toml")),
            format!(
                "name = \"{name}\"\ndescription = \"test\"\n[[steps]]\nid = \"s\"\nspeakers = [\"pm\"]\nprompt = \"do {{{{topic}}}}\"\n"
            ),
        )
        .expect("write workflow fixture");
    }

    #[test]
    fn import_creates_parents_and_children() {
        let (dir, mut store) = tmp_store();
        write_workflow_fixture(dir.path(), "tdd_development");
        let req: ImportTasksRequest = serde_json::from_value(serde_json::json!({
            "tasks": [
                {
                    "title": "父任务",
                    "description": "d",
                    "priority": 1,
                    "labels": ["a", "b"],
                    "workflow": "tdd_development",
                    "subtasks": [
                        { "title": "子一" },
                        { "title": "子二", "priority": 4 }
                    ]
                },
                { "title": "独立任务" }
            ]
        }))
        .expect("parse import request");
        let created =
            import_tasks_into(&mut store, dir.path(), &req).expect("import should succeed");
        assert_eq!(created, vec!["LAT-100", "LAT-101", "LAT-102", "LAT-103"]);
        let parent = store.get("LAT-100").expect("parent");
        assert_eq!(parent.state, "backlog");
        assert_eq!(parent.priority, 1);
        assert_eq!(parent.labels, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(parent.workflow.as_deref(), Some("tdd_development"));
        assert_eq!(parent.history[0].actor, "import");
        let c1 = store.get("LAT-101").expect("child1");
        let c2 = store.get("LAT-102").expect("child2");
        assert_eq!(c1.parent_id.as_deref(), Some("LAT-100"));
        assert_eq!(c1.sub_order, 0);
        assert_eq!(c2.parent_id.as_deref(), Some("LAT-100"));
        assert_eq!(c2.sub_order, 1);
        assert_eq!(c2.priority, 4);
        assert_eq!(c1.state, "backlog");
        // 缺省字段：priority 3、无 workflow、无 labels。
        let solo = store.get("LAT-103").expect("solo");
        assert_eq!(solo.priority, 3);
        assert_eq!(solo.workflow, None);
    }

    #[test]
    fn import_rejects_empty_title() {
        let (dir, mut store) = tmp_store();
        let req: ImportTasksRequest = serde_json::from_value(serde_json::json!({
            "tasks": [{ "title": "  " }]
        }))
        .expect("parse");
        let err = import_tasks_into(&mut store, dir.path(), &req).unwrap_err();
        assert!(err.contains("title 不能为空"), "{err}");
        assert!(store.list().is_empty(), "失败不应留下任务");
    }

    #[test]
    fn import_rejects_bad_priority() {
        let (dir, mut store) = tmp_store();
        let req: ImportTasksRequest = serde_json::from_value(serde_json::json!({
            "tasks": [{ "title": "ok" }, { "title": "坏优先级", "priority": 9 }]
        }))
        .expect("parse");
        let err = import_tasks_into(&mut store, dir.path(), &req).unwrap_err();
        assert!(err.contains("坏优先级"), "{err}");
        assert!(err.contains("priority"), "{err}");
        // 已创建的 id 要出现在错误消息里（部分导入可见）。
        assert!(err.contains("LAT-100"), "{err}");
        assert!(store.get("LAT-100").is_some(), "第一个任务已创建");
        assert!(store.get("LAT-101").is_none(), "失败的任务未创建");
    }

    #[test]
    fn import_rejects_two_level_nesting() {
        let (dir, mut store) = tmp_store();
        let req: ImportTasksRequest = serde_json::from_value(serde_json::json!({
            "tasks": [{
                "title": "父",
                "subtasks": [{ "title": "子", "subtasks": [{ "title": "孙" }] }]
            }]
        }))
        .expect("parse");
        let err = import_tasks_into(&mut store, dir.path(), &req).unwrap_err();
        assert!(
            err.contains("subtasks nested deeper than one level"),
            "{err}"
        );
    }

    #[test]
    fn import_rejects_unknown_workflow() {
        let (dir, mut store) = tmp_store();
        let req: ImportTasksRequest = serde_json::from_value(serde_json::json!({
            "tasks": [{ "title": "x", "workflow": "no_such_wf_xyz" }]
        }))
        .expect("parse");
        let err = import_tasks_into(&mut store, dir.path(), &req).unwrap_err();
        assert!(err.contains("unknown workflow 'no_such_wf_xyz'"), "{err}");
    }

    /// 标题超过 80 字符截断而非报错。
    #[test]
    fn import_truncates_long_title() {
        let (dir, mut store) = tmp_store();
        let long = "题".repeat(100);
        let req: ImportTasksRequest = serde_json::from_value(serde_json::json!({
            "tasks": [{ "title": long }]
        }))
        .expect("parse");
        let created = import_tasks_into(&mut store, dir.path(), &req).expect("import");
        let t = store.get(&created[0]).expect("task");
        assert_eq!(t.title.chars().count(), 80);
    }

    // ─── workflow 跑完后的自动状态迁移（apply_workflow_finish） ──────

    /// 造一个 in_progress、带一条未结束 run 的任务。
    fn make_running_task(store: &mut TaskStore, title: &str) -> Task {
        let t = make_task(store, title);
        let now = now_ms();
        {
            let t = store.get_mut(&t.id).unwrap();
            t.set_state("in_progress", "user", None, now);
            t.runs.push(TaskRun {
                session_id: "ui-test".into(),
                started_at: now,
                ended_at: None,
                result: None,
            });
        }
        store.persist(&t.id).unwrap();
        store.get(&t.id).unwrap().clone()
    }

    #[test]
    fn workflow_finish_ok_moves_to_human_review() {
        let (_dir, mut store) = tmp_store();
        let t = make_running_task(&mut store, "wf 成功");
        let summary = format!("{}结论", "x".repeat(600));
        apply_workflow_finish(&mut store, &t.id, Ok(summary), false, now_ms()).expect("finish");
        let t = store.get(&t.id).unwrap();
        assert_eq!(t.state, "human_review");
        let run = t.runs.last().unwrap();
        assert!(run.ended_at.is_some());
        assert_eq!(run.result.as_deref(), Some("completed"));
        // 摘要取末尾 500 字符，history actor = workflow。
        let h = t.history.last().unwrap();
        assert_eq!(h.actor, "workflow");
        assert_eq!(h.to, "human_review");
        let note = h.note.as_deref().expect("note");
        assert_eq!(note.chars().count(), 500);
        assert!(note.ends_with("结论"));
    }

    #[test]
    fn workflow_finish_err_moves_back_to_todo() {
        let (_dir, mut store) = tmp_store();
        let t = make_running_task(&mut store, "wf 失败");
        apply_workflow_finish(&mut store, &t.id, Err("boom".into()), false, now_ms())
            .expect("finish");
        let t = store.get(&t.id).unwrap();
        assert_eq!(t.state, "todo");
        let run = t.runs.last().unwrap();
        assert!(run.ended_at.is_some());
        assert_eq!(run.result.as_deref(), Some("failed"));
        let h = t.history.last().unwrap();
        assert_eq!(h.actor, "workflow");
        assert_eq!(h.note.as_deref(), Some("boom"));
    }

    #[test]
    fn workflow_finish_cancelled_marks_aborted() {
        let (_dir, mut store) = tmp_store();
        let t = make_running_task(&mut store, "wf 取消");
        apply_workflow_finish(&mut store, &t.id, Err("cancelled".into()), true, now_ms())
            .expect("finish");
        let t = store.get(&t.id).unwrap();
        assert_eq!(t.state, "todo");
        assert_eq!(t.runs.last().unwrap().result.as_deref(), Some("aborted"));
    }

    /// abort_task 已先收尾（run 已 ended、state 已回 todo）时，
    /// finish 不得再次迁移状态。
    // ─── 父子联动 + 家族互斥 ─────────────────────────────────────

    fn make_child(store: &mut TaskStore, parent: &Task, title: &str) -> Task {
        store
            .create(title, "desc", 2, vec![], Some(parent.id.clone()), None, None, "user")
            .expect("create child")
    }

    #[test]
    fn parent_completes_when_all_children_done() {
        let (_dir, mut store) = tmp_store();
        let p = make_task(&mut store, "父任务");
        let c1 = make_child(&mut store, &p, "子1");
        let c2 = make_child(&mut store, &p, "子2");
        let now = now_ms();
        for c in [&c1, &c2] {
            store
                .get_mut(&c.id)
                .unwrap()
                .set_state("done", "user", None, now);
            let snap = store.get(&c.id).unwrap().clone();
            maybe_complete_parent(&mut store, &snap, now);
        }
        let p = store.get(&p.id).unwrap();
        assert_eq!(p.state, "done", "全部子任务 done 后父任务应自动完成");
        assert_eq!(p.history.last().unwrap().actor, "workflow");
    }

    #[test]
    fn parent_stays_when_child_cancelled_or_pending() {
        let (_dir, mut store) = tmp_store();
        let p = make_task(&mut store, "父任务");
        let c1 = make_child(&mut store, &p, "子1");
        let c2 = make_child(&mut store, &p, "子2");
        let now = now_ms();
        store
            .get_mut(&c1.id)
            .unwrap()
            .set_state("done", "user", None, now);
        store
            .get_mut(&c2.id)
            .unwrap()
            .set_state("cancelled", "user", None, now);
        let snap = store.get(&c2.id).unwrap().clone();
        maybe_complete_parent(&mut store, &snap, now);
        assert_eq!(store.get(&p.id).unwrap().state, "backlog", "有 cancelled 子任务不得自动完成");
    }

    #[test]
    fn family_conflict_detects_running_sibling_and_parent() {
        let (_dir, mut store) = tmp_store();
        let p = make_task(&mut store, "父任务");
        let c1 = make_child(&mut store, &p, "子1");
        let c2 = make_child(&mut store, &p, "子2");
        // 兄弟 in_progress → c2 冲突
        store
            .get_mut(&c1.id)
            .unwrap()
            .set_state("in_progress", "user", None, now_ms());
        assert_eq!(family_running_conflict(&store, &c2.id).as_deref(), Some(c1.id.as_str()));
        // 兄弟空闲 → 无冲突
        store
            .get_mut(&c1.id)
            .unwrap()
            .set_state("todo", "user", None, now_ms());
        assert!(family_running_conflict(&store, &c2.id).is_none());
        // 父 in_progress → 子也冲突；子在跑 → 父 dispatch 也冲突
        store
            .get_mut(&p.id)
            .unwrap()
            .set_state("in_progress", "user", None, now_ms());
        assert_eq!(family_running_conflict(&store, &c2.id).as_deref(), Some(p.id.as_str()));
        store
            .get_mut(&p.id)
            .unwrap()
            .set_state("todo", "user", None, now_ms());
        store
            .get_mut(&c1.id)
            .unwrap()
            .set_state("in_progress", "user", None, now_ms());
        assert_eq!(family_running_conflict(&store, &p.id).as_deref(), Some(c1.id.as_str()));
    }

    // ─── 跨族文件范围互斥（paths） ───────────────────────────────

    #[test]
    fn paths_overlap_prefix_and_segment_boundary() {
        let a = || vec!["src/foo".to_string()];
        // 目录覆盖文件 / 文件在目录内 / 完全相同 → 重叠
        assert!(paths_overlap(&a(), &["src/foo/bar.rs".into()]));
        assert!(paths_overlap(&["src/foo/bar.rs".into()], &a()));
        assert!(paths_overlap(&a(), &a()));
        // 路径段边界：src/foo 与 src/foobar 只是字符串前缀，不算重叠
        assert!(!paths_overlap(&a(), &["src/foobar".into()]));
        assert!(!paths_overlap(&a(), &["src/foobar/x.rs".into()]));
        // 互不相关的路径
        assert!(!paths_overlap(&a(), &["src/baz".into()]));
        // 对称性：祖先/后代方向反过来同样算重叠。
        assert!(paths_overlap(&a(), &["src".into()]));
        assert!(paths_overlap(&["src".into()], &a()));
    }

    #[test]
    fn paths_overlap_empty_never_conflicts() {
        let a = vec!["src/foo".to_string()];
        assert!(!paths_overlap(&[], &a));
        assert!(!paths_overlap(&a, &[]));
        assert!(!paths_overlap(&[], &[]));
        // 规范化后为空的声明（"./"、"/"）视作未声明
        assert!(!paths_overlap(&["./".into()], &a));
        assert!(!paths_overlap(&a, &["/".into()]));
    }

    #[test]
    fn paths_overlap_normalizes_dot_and_trailing_slash() {
        let a = vec!["./src/foo/".to_string()];
        assert!(paths_overlap(&a, &["src/foo".into()]));
        assert!(paths_overlap(&a, &["src/foo/bar.rs".into()]));
        assert_eq!(normalize_declared_path("./src//foo/"), "src/foo");
    }

    #[test]
    fn path_conflict_cross_family() {
        let (_dir, mut store) = tmp_store();
        // 无亲缘关系的两个任务：a 在跑且声明了范围。
        let a = make_task(&mut store, "改 ringbuf");
        store.get_mut(&a.id).unwrap().paths = vec!["src/ringbuf".into()];
        store
            .get_mut(&a.id)
            .unwrap()
            .set_state("in_progress", "user", None, now_ms());
        let b = make_task(&mut store, "改 ringbuf 测试");
        store.get_mut(&b.id).unwrap().paths = vec!["src/ringbuf/tests".into()];
        // 范围重叠 → 冲突，返回在跑任务 id 与重叠路径对。
        let (cid, (mine, theirs)) =
            path_running_conflict(&store, &b.id).expect("重叠应冲突");
        assert_eq!(cid, a.id);
        assert_eq!(mine, "src/ringbuf/tests");
        assert_eq!(theirs, "src/ringbuf");
        // 反向（b 在跑，dispatch a）同样冲突。
        store
            .get_mut(&a.id)
            .unwrap()
            .set_state("todo", "user", None, now_ms());
        store
            .get_mut(&b.id)
            .unwrap()
            .set_state("in_progress", "user", None, now_ms());
        assert_eq!(
            path_running_conflict(&store, &a.id).map(|(cid, _)| cid),
            Some(b.id.clone())
        );
        // 不重叠 → 放行。
        store.get_mut(&a.id).unwrap().paths = vec!["src/other".into()];
        assert!(path_running_conflict(&store, &a.id).is_none());
        // 在跑任务完成（非 in_progress）→ 不再拦截。
        store.get_mut(&a.id).unwrap().paths = vec!["src/ringbuf".into()];
        store
            .get_mut(&b.id)
            .unwrap()
            .set_state("done", "user", None, now_ms());
        assert!(path_running_conflict(&store, &a.id).is_none());
        // 本任务未声明 paths → 永不冲突。
        store
            .get_mut(&b.id)
            .unwrap()
            .set_state("in_progress", "user", None, now_ms());
        let c = make_task(&mut store, "未声明范围");
        assert!(path_running_conflict(&store, &c.id).is_none());
    }

    /// dispatch 层级：跨族 paths 重叠 → 409（在创建 session 之前被拦下）。
    #[tokio::test]
    async fn dispatch_rejects_cross_family_path_overlap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = latte_agent_core::AgentConfig::default();
        let resolver = latte_agent_core::ModelResolver::from_config(&cfg).expect("resolver");
        let b = UiBackend::new(crate::UiBackendConfig {
            agent_config: cfg,
            model_resolver: resolver,
            role: None,
            tier: None,
            model_id: None,
            cwd: Some(dir.path().to_path_buf()),
            agents_config: ".latte/agents.d".into(),
        })
        .expect("backend");

        let req: ImportTasksRequest = serde_json::from_value(serde_json::json!({
            "tasks": [
                { "title": "改 ringbuf", "paths": ["src/ringbuf"] },
                { "title": "改 ringbuf 测试", "paths": ["src/ringbuf/tests"] }
            ]
        }))
        .expect("parse req");
        let resp = import_tasks(&b, req).expect("import");
        let [a, c] = &resp.created[..] else {
            panic!("应创建 2 个任务");
        };
        {
            let mut store = b.tasks.write();
            let now = now_ms();
            store.get_mut(a).unwrap().set_state("in_progress", "user", None, now);
            store.get_mut(c).unwrap().set_state("todo", "user", None, now);
        }
        let err = dispatch_task(&b, c, "user").await.expect_err("重叠应 409");
        assert_eq!(err.status, 409);
        assert!(err.message.contains(a), "消息应列出冲突任务 id：{}", err.message);
        assert!(err.message.contains("src/ringbuf"), "消息应列出重叠路径：{}", err.message);
        // 被拒后任务状态不变。
        assert_eq!(b.tasks.read().get(c).unwrap().state, "todo");
    }

    #[test]
    fn import_persists_paths() {
        let (dir, mut store) = tmp_store();
        let req: ImportTasksRequest = serde_json::from_value(serde_json::json!({
            "tasks": [{
                "title": "改 ringbuf",
                "paths": ["src/ringbuf", "tests/ringbuf.rs"],
                "subtasks": [{ "title": "补压测", "paths": ["benches"] }]
            }]
        }))
        .expect("parse req");
        let created = import_tasks_into(&mut store, dir.path(), &req).expect("import");
        assert_eq!(created.len(), 2);
        assert_eq!(
            store.get(&created[0]).unwrap().paths,
            vec!["src/ringbuf".to_string(), "tests/ringbuf.rs".to_string()]
        );
        assert_eq!(store.get(&created[1]).unwrap().paths, vec!["benches".to_string()]);
        // 重新加载（落盘 → 读回）paths 仍在；旧数据无 paths 字段 → 空。
        let store2 = TaskStore::load(dir.path()).expect("reload");
        assert_eq!(
            store2.get(&created[0]).unwrap().paths,
            vec!["src/ringbuf".to_string(), "tests/ringbuf.rs".to_string()]
        );
    }

    #[test]
    fn task_json_without_paths_defaults_empty() {
        // 旧落盘文件无 paths 字段 → serde default 给空 vec（向后兼容）。
        let t: Task = serde_json::from_value(serde_json::json!({
            "schema": 1, "id": "LAT-1", "title": "旧任务", "state": "backlog",
            "created_at": 0, "updated_at": 0
        }))
        .expect("parse old task");
        assert!(t.paths.is_empty());
    }

    #[test]
    fn verdict_is_reject_checks_head_only() {
        assert!(verdict_is_reject("## 裁决\n\n❌ 不通过\n\n## 理由\n…"));
        assert!(!verdict_is_reject("## 裁决\n\n✅ 通过"));
        assert!(!verdict_is_reject("## 裁决\n\n⚠️ 有条件通过"));
        // 意见表格里的 ❌ 出现在 200 字符之后 → 不误判
        let head = "## 裁决\n\n⚠️ 有条件通过\n".to_string() + &"x".repeat(300);
        assert!(!verdict_is_reject(&format!("{head}\n| 问题 | ❌ |")));
    }

    #[test]
    fn workflow_finish_after_abort_is_noop() {
        let (_dir, mut store) = tmp_store();
        let t = make_running_task(&mut store, "wf 竞态");
        let now = now_ms();
        {
            let t = store.get_mut(&t.id).unwrap();
            let run = t.runs.last_mut().unwrap();
            run.ended_at = Some(now);
            run.result = Some("aborted".into());
            t.set_state("todo", "user", Some("中止执行".into()), now);
        }
        let history_len = store.get(&t.id).unwrap().history.len();
        apply_workflow_finish(&mut store, &t.id, Ok("late success".into()), false, now_ms())
            .expect("finish");
        let t = store.get(&t.id).unwrap();
        assert_eq!(t.state, "todo", "不得覆盖 abort 的回退");
        assert_eq!(t.runs.last().unwrap().result.as_deref(), Some("aborted"));
        assert_eq!(t.history.len(), history_len, "不得追加 history");
    }
}
