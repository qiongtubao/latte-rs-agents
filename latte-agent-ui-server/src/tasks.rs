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
    /// 业务类型：feature/bugfix/learn/research/docs/chore 等，
    /// 可配置于 `config/task_types.toml` / `.latte/task_types.toml`。
    /// 旧文件缺省 → None（兼容），未填时派发走普通 manager 会话。
    #[serde(default)]
    pub task_type: Option<String>,
    /// 最多一层嵌套：父任务不允许再有 `parent_id`。
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub sub_order: i64,
    /// 排期只是 `todo` 上的属性，不单独设 `scheduled` 状态。
    #[serde(default)]
    pub scheduled_at: Option<i64>,
    /// 绑定的 workflow 名：派发时直接跑该 workflow（不走 manager 派发）。
    /// 旧落盘文件无此字段 → `None`。为空/空白视为未绑定。
    /// 若未显式绑定但 `task_type` 有默认 workflow，则按类型推导。
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
    /// 派发时实际生效的 workflow（显式 workflow 优先，否则按 task_type 默认推导；None=走 manager）。
    pub effective_workflow: Option<String>,
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
    /// Notion 同步的 dirty 集合：每次 persist 末尾 mark 任务 id，
    /// notion_sync 后台定时任务取走并 PUT。内存态，不落盘（重启即
    /// 清空；用户须重做变更触发重新 sync）。详见 `crate::notion_sync`。
    notion_dirty: crate::notion_sync::DirtySet,
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
                    match serde_json::from_str::<Task>(&raw) {
                        Ok(t) => {
                            tasks.insert(t.id.clone(), t);
                        }
                        // 解析失败原来是静默 `continue`：手写或外部工具
                        // 生成的任务文件少一个必填字段（`note` /
                        // `created_at` / `updated_at` 都没有 serde
                        // default），就毫无提示地**不出现在看板里**。
                        // 实测踩过：造的两个任务文件一直不显示，查到
                        // 这里才发现被吞了。
                        Err(e) => {
                            eprintln!(
                                "[tasks] 跳过无法解析的任务文件 {}: {e}",
                                path.display()
                            );
                        }
                    }
                }
            }
        }
        Ok(Self { dir, meta, tasks, workflow_cancels: HashMap::new(), notion_dirty: Default::default() })
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
        task_type: Option<String>,
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
        // task_type 校验：为 Some 时必须在注册表已知（旧落盘 None 兼容）。
        let task_type = match task_type.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(tt) => {
                let cwd = self.dir.parent().and_then(|p| p.parent()).unwrap_or(std::path::Path::new("."));
                let reg = crate::task_types::TaskTypeRegistry::load(cwd);
                if !reg.types.is_empty() && !reg.is_known(tt) {
                    return Err(format!("unknown task_type '{tt}'"));
                }
                Some(tt.to_string())
            }
        };
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
            task_type,
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
        self.notion_dirty.mark(&id);
        Ok(task)
    }

    /// 把内存中的任务原子写回 `<id>.json`。
    pub fn save_task(&self, task: &Task) -> Result<(), String> {
        write_atomic_json(&self.dir.join(format!("{}.json", task.id)), task)
    }

    /// 内存里的 `id` 当前值落盘（配合 `get_mut` 原地改后用）。
    /// 同时把任务 id 推入 Notion 同步的 dirty 集合（纯内存态，不
    /// 落盘——重启即清空，sync no-op）。
    pub fn persist(&mut self, id: &str) -> Result<(), String> {
        let t = self
            .tasks
            .get(id)
            .ok_or_else(|| format!("task {id:?} 不存在"))?;
        self.save_task(t)?;
        self.notion_dirty.mark(id);
        Ok(())
    }

    /// 手动把任务 id 标记为待同步（`create` / `delete` 路径不走
    /// `persist`，需要调用方显式标 dirty）。`delete` 路径在
    /// `crate::api::delete_task` 处统一调。
    pub fn mark_notion_dirty(&mut self, id: &str) {
        self.notion_dirty.mark(id);
    }

    /// 取出当前 dirty 集合（清空），返回 id 列表。
    pub fn take_notion_dirty(&mut self) -> Vec<String> {
        self.notion_dirty.take_dirty()
    }

    /// 当前 dirty 数量（用于监控/测试）。
    pub fn notion_dirty_count(&self) -> usize {
        self.notion_dirty.dirty_count()
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
        // effective workflow 供前端直接展示（类型优先，与 redo 默认模式一致）
        let cwd = self.dir.parent().and_then(|p| p.parent()).unwrap_or(std::path::Path::new("."));
        let reg = crate::task_types::TaskTypeRegistry::load(cwd);
        let effective = crate::task_types::effective_workflow_type_first(task.task_type.as_deref(), task.workflow.as_deref(), &reg);
        TaskView {
            effective_workflow: effective,
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
    /// 业务类型：feature/bugfix/learn/research/docs/chore 等，须在
    /// `config/task_types.toml` 注册；未知类型 400（旧任务 None 兼容）。
    #[serde(default)]
    pub task_type: Option<String>,
    /// 绑定的 workflow 名（必须存在于 `.latte/workflows.d`，否则 400）。
    /// 为空时按 `task_type` 的默认 workflow 推导；两者都为空 → 走 manager。
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
    /// 业务类型：三态（缺省不变 / null 清空 / 字符串设置），未知类型 400。
    #[serde(default, deserialize_with = "deserialize_nullable")]
    pub task_type: Option<Option<String>>,
    /// 双层 Option：缺省 = 不变；`null` = 解绑 workflow；字符串 = 绑定。
    /// （plain `#[serde(default)]` 会把显式 `null` 折叠成"缺省"，
    /// 必须自定义 deserialize_with 才能区分三者。）
    #[serde(default, deserialize_with = "deserialize_nullable")]
    pub workflow: Option<Option<String>>,
    /// 任务涉及的文件/目录前缀：缺省 = 不变；数组 = 整体替换（空数组
    /// = 清空声明）。
    ///
    /// 原先 `paths` 只能在 create/import 时写入、事后无法修正——一旦
    /// 导入时字段名写错（实测：`involved_paths` 被 serde 静默丢弃），
    /// 26 个任务的范围声明就永久是空的，只能手改 JSON 落盘文件。
    #[serde(
        default,
        alias = "involved_paths",
        alias = "involved_files",
        alias = "affected_paths",
        alias = "affected_files",
        alias = "file_paths",
        alias = "files"
    )]
    pub paths: Option<Vec<String>>,
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
#[derive(Deserialize, Clone)]
pub struct ImportTasksRequest {
    pub tasks: Vec<ImportTask>,
    /// 来源 plan 提案 id（PlanProposed 事件携带）。`Some` 时导入成功
    /// 即视为用户批准该任务清单：对应 session 的 plan 阶段门从
    /// `PendingApproval` 置为 `Approved`，解除实现类 delegate 拦截。
    #[serde(default)]
    pub plan_id: Option<String>,
    /// 父任务指定：拆分导入时全部任务作为该父任务的子任务创建（父
    /// 任务必须是根任务；带 parent_id 的导入不支持 item 再嵌套
    /// subtasks）。三态：非空串 = 显式指定；空串 = 显式「无父任务」
    /// （覆盖 session_id 解析，防止误挂）；缺省 = 按 session_id 查
    /// 拆分会话映射（refine 登记）。
    #[serde(default)]
    pub parent_id: Option<String>,
    /// 导入发起方 session：parent_id 缺省时用它查 `refine_parents`
    /// （看板「拆分子任务」创建的 session → 父任务）。
    #[serde(default)]
    pub session_id: Option<String>,
}

/// 导入的单个任务：除 `title` 外全部可选；`subtasks` 最多一层。
#[derive(Deserialize, Clone)]
pub struct ImportTask {
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub task_type: Option<String>,
    #[serde(default)]
    pub workflow: Option<String>,
    /// 任务涉及的文件/目录前缀（相对项目根）：派发时与在跑任务范围
    /// 重叠会被拒绝（409）。空 = 未声明。
    ///
    /// `alias` 与 [`latte_agent_core::controller::PlanTask`] 保持一致：
    /// 模型写成 `involved_paths` 等同义名时不再静默丢整个数组。
    #[serde(
        default,
        alias = "involved_paths",
        alias = "involved_files",
        alias = "affected_paths",
        alias = "affected_files",
        alias = "file_paths",
        alias = "files"
    )]
    pub paths: Vec<String>,
    #[serde(default)]
    pub subtasks: Vec<ImportTask>,
}

/// `POST /api/tasks/import` 响应：按创建顺序（父先于子）的任务 id。
/// 导入只进 todo，是否派发由用户在任务看板手动操作（不自动调度）。
#[derive(Serialize, Debug)]
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
    validate_task_type(&b.cwd, req.task_type.as_deref())?;
    validate_workflow_name(&b.cwd, req.workflow.as_deref())?;
    let mut store = b.tasks.write();
    let t = store
        .create(
            &req.title,
            &req.description,
            req.priority.unwrap_or(3),
            req.labels,
            req.task_type,
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
fn validate_task_type(cwd: &Path, tt: Option<&str>) -> Result<(), ApiError> {
    if let Some(name) = tt.map(str::trim).filter(|s| !s.is_empty()) {
        let reg = crate::task_types::TaskTypeRegistry::load(cwd);
        if !reg.is_known(name) {
            return Err(ApiError::bad_request(format!("unknown task_type '{name}'")));
        }
    }
    Ok(())
}

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
/// `parent_id` 非空时任务作为该父任务的子任务创建（拆分导入），
/// 此时 item 不得再嵌套 subtasks（只能一层父子）。
fn import_one(
    store: &mut TaskStore,
    cwd: &Path,
    item: &ImportTask,
    parent_id: Option<&str>,
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
    if parent_id.is_some() && !item.subtasks.is_empty() {
        return Err("拆分导入（带 parent_id）不支持再嵌套 subtasks".to_string());
    }
    // workflow 引用必须存在：静默降级会让任务丢掉绑定的流程而无人
    // 察觉——拒绝并报错，让提交方（manager/用户）修正后重试。
    // 空串/空白 = 不绑定：plan 工具约定「没有贴合的必须留空」，产出
    // 就是 workflow: ""，不能当成 workflow 名去校验（实测现场：
    // 6 个任务全部 workflow:"" 导入被 400 整单拒绝）。
    let workflow = match item.workflow.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(wf) if !crate::workflows::exists(cwd, wf) => {
            return Err(format!("unknown workflow '{wf}'"));
        }
        Some(wf) => Some(wf.to_string()),
    };
    let task_type = match item.task_type.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(tt) => {
            let reg = crate::task_types::TaskTypeRegistry::load(cwd);
            if !reg.is_known(tt) {
                return Err(format!("unknown task_type '{tt}'"));
            }
            Some(tt.to_string())
        }
    };
    let parent = store.create(
        &title,
        &item.description,
        item.priority.unwrap_or(3),
        item.labels.clone(),
        task_type.clone(),
        parent_id.map(str::to_string),
        None,
        workflow.clone(),
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
        if let Some(tt) = sub.task_type.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            let reg = crate::task_types::TaskTypeRegistry::load(cwd);
            if !reg.is_known(tt) {
                return Err(format!("unknown task_type '{tt}'（子任务 '{stitle}'）"));
            }
        }
        let sub_task_type = sub.task_type.as_deref().map(str::trim).and_then(|tt| {
            if tt.is_empty() { None } else { Some(tt.to_string()) }
        });
        // 子任务 workflow 引用不存在时同样降级；空串/空白 = 不绑定。
        let sub_workflow = sub.workflow.as_deref().map(str::trim).and_then(|wf| {
            if !wf.is_empty() && crate::workflows::exists(cwd, wf) {
                Some(wf.to_string())
            } else {
                None
            }
        });
        let child = store.create(
            &stitle,
            &sub.description,
            sub.priority.unwrap_or(3),
            sub.labels.clone(),
            sub_task_type,
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
    // 拆分导入：父任务必须存在且是根任务（子任务不能再有子任务）。
    if let Some(pid) = req.parent_id.as_deref() {
        match store.get(pid) {
            None => return Err(format!("父任务 {pid:?} 不存在")),
            Some(p) if p.parent_id.is_some() => {
                return Err(format!("父任务 {pid:?} 本身是子任务，不能再拆"))
            }
            _ => {}
        }
    }
    let mut created: Vec<String> = Vec::new();
    for item in &req.tasks {
        if let Err(e) = import_one(store, cwd, item, req.parent_id.as_deref(), &mut created) {
            return Err(format!(
                "导入任务 {:?} 失败：{e}（已创建：{created:?}）",
                item.title.trim()
            ));
        }
    }
    Ok(created)
}

pub async fn import_tasks(
    b: &UiBackend,
    req: ImportTasksRequest,
) -> Result<ImportTasksResponse, ApiError> {
    // 父任务三态解析：显式 parent_id（含 ""=显式根任务）优先；
    // 缺省时按 session_id 查拆分会话映射（refine 登记，页面刷新不丢）。
    let mut req = req;
    match req.parent_id.as_deref().map(str::trim) {
        Some("") => req.parent_id = None, // 用户在弹窗显式选了「无父任务」
        Some(_) => {}                     // 显式指定，原样校验使用
        None => {
            if let Some(sid) = req.session_id.as_deref() {
                if let Some(pid) = b.refine_parents.read().get(sid) {
                    req.parent_id = Some(pid.clone());
                }
            }
        }
    }
    // 写锁作用域收口在块内：guard 不 Send，不能活过下面的 .await。
    let created = {
        let mut store = b.tasks.write();
        let created =
            import_tasks_into(&mut store, &b.cwd, &req).map_err(ApiError::bad_request)?;
        // 导入即进 todo：执行与否由用户在任务看板手动派发，不做自动调度。
        let now = now_ms();
        for id in &created {
            if let Some(t) = store.get_mut(id) {
                t.set_state("todo", "import", Some("已导入，待派发".into()), now);
            }
            if let Err(e) = store.persist(id) {
                eprintln!("[tasks] persist {id} after import: {e}");
            }
        }
        created
    };
    // plan 阶段门：带 plan_id 的导入 = 用户批准该任务清单。找到持有
    // 该 PendingApproval 的 session（plan 弹窗属于某个 session，stage
    // 按 session 存），置 Approved 解除实现类 delegate 拦截。
    if let Some(plan_id) = &req.plan_id {
        approve_plan_stage(b, plan_id);
        // 清单已导入 = 弹窗已处理：从补发表销账，否则用户下次重连
        // 又被弹一遍同一份清单。
        latte_agent_core::choice::dismiss_prompt(plan_id);
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
        if let Some(paths) = patch.paths {
            // 归一化：去空白、丢空串、去重（保持首次出现顺序）。派发时
            // 的范围互斥按前缀比较，空串会匹配一切，必须挡在入口。
            let mut seen = std::collections::HashSet::new();
            t.paths = paths
                .into_iter()
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty() && seen.insert(p.clone()))
                .collect();
        }
        if let Some(sched) = patch.scheduled_at {
            t.scheduled_at = sched;
        }
        if let Some(tt) = patch.task_type {
            match tt.as_deref().map(str::trim) {
                None => t.task_type = None,
                Some("") => t.task_type = None,
                Some(name) => {
                    let reg = crate::task_types::TaskTypeRegistry::load(&b.cwd);
                    if !reg.is_known(name) {
                        return Err(ApiError::bad_request(format!("unknown task_type '{name}'")));
                    }
                    t.task_type = Some(name.to_string());
                }
            }
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
        let (event_tx, session_id) = match &last_session {
            Some(sid) => match api::session_event_sender(&b, sid).await {
                Ok(tx) => (tx, sid.clone()),
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
                    Ok(tx) => (tx, info.session_id.clone()),
                    Err(e) => {
                        eprintln!("[tasks] hook new session sender: {}", e.message);
                        return;
                    }
                }
            }
        };
        // 复用该 session 的暂停门：用户 ⏸ 时看板派的 workflow 也一起停。
        let agent_pause_gate = api::session_pause_gate(&b, &session_id).await.ok();
        // advisor intervene 暂停门同理：判「等待用户拍板」时流水线 park。
        let advisor_pause = api::session_advisor_pause_gate(&b, &session_id).await.ok();
        // per-turn 取消旗标：生命周期钩子的 workflow 也留终止逃生口。
        let turn_cancel = api::session_turn_cancel_flag(&b, &session_id).await.ok();
        let ctx = WorkflowRunContext {
            merged: Arc::new(b.merged.read().clone()),
            resolver: b.resolver.clone(),
            default_params: GenerateParams::default(),
            cwd: b.cwd.clone(),
            event_tx,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            turn_cancel_flag: turn_cancel,
            depth: 0,
            // 顶层 run：自己就是嵌套链的根。
            root_wf_id: None,
            agent_pause_gate,
            // 跑在真实 session 事件流上：分派建 subsession、过
            // advisor gate，与 manager 的 delegate 一致。
            subsession_store: Some(b.subsession_store.clone()),
            session_id: Some(session_id.clone()),
            advisor_gate: latte_agent_core::advisor_monitor::AdvisorMonitorConfig {
                enabled: b.merged.read().advisor.enabled(),
                ..latte_agent_core::advisor_monitor::AdvisorMonitorConfig::default()
            }
            .runner_gate(),
            advisor_pause,
            staging: None,
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
/// `mode`：
/// - `redo`（默认）：按任务自身的 workflow 绑定重跑——显式 workflow 优先，
///   否则按 task_type 默认推导；rework 状态重派时保留原 workflow，旧审查
///   反馈仅作为 topic 参考（不强制 rework 定点修复）。适合"改了类型/方向
///   要按新要求重跑"的场景。
/// - `rework`：强制走 `rework` 返工流（理解反馈→定点修复→验证→终审），
///   带审查反馈。适合小修小补、反馈指向明确修复点的场景。
///
/// 绑定了 workflow 的任务（effective ≠ None）不发 manager 消息：
/// 直接在该 session 上跑 `run_workflow`（事件进 session 的 broadcast，
/// SSE/归档照常可见），后台跑完后按 report 语义自动迁移状态
/// （actor=workflow，见 [`apply_workflow_finish`]）。
pub async fn dispatch_task(
    b: &UiBackend,
    id: &str,
    actor: &str,
    mode: &str,
) -> Result<TaskView, ApiError> {
    // 0. 容器分流：**带未完成子任务的父任务不自己跑 workflow**。
    //    否则父任务会把子任务清单当成一段文本塞进 topic、整体跑一遍
    //    自己的 workflow 就收尾（human_review），子任务从未被真正执行
    //    ——这正是「拆分本质没执行成」的根因。这里改为把父任务当作
    //    聚合容器：派发它的 todo 子任务，父任务标记 in_progress 等子任务
    //    全部完成后由 maybe_complete_parent 自动收尾。
    {
        let is_container = {
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
            is_container_with_pending_children(&store, id)
        };
        if is_container {
            return dispatch_container(b, id, actor).await;
        }
    }

    // 1. 读锁内校验 + 收集消息素材（不持锁跨 await）。
    let (title, priority, description, children, task_type, workflow, prev_state, recent_notes) = {
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
            t.task_type.clone(),
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

    // 3. workflow 绑定分支：`mode` 决定重做方式。
    //    - "redo"（默认）：**优先按 task_type 默认 workflow 推导**，显式 workflow
    //      仅作兜底（task_type 无默认时才用）。用户改类型=希望跑该类型的流程。
    //    - "rework"：强制走 rework 返工流（带审查反馈），适合小修小补的返工场景。
    let reg = crate::task_types::TaskTypeRegistry::load(&b.cwd);
    let effective = if mode == "redo" {
        // redo：优先 task_type 默认，显式 workflow 仅作兜底
        crate::task_types::effective_workflow_type_first(task_type.as_deref(), workflow.as_deref(), &reg)
    } else {
        crate::task_types::effective_workflow(task_type.as_deref(), workflow.as_deref(), &reg)
    };
    let (wf_name, msg) = if mode == "rework" {
        // 强制返工流：即使 task_type=learn 也走 rework.toml，带审查反馈
        let feedback = if recent_notes.is_empty() {
            String::new()
        } else {
            format!("\n\n【审查反馈与执行历史】\n{}", recent_notes.join("\n---\n"))
        };
        ("rework".to_string(), format!("{base_msg}{feedback}"))
    } else if let Some(bound) = effective {
        // 按类型重跑（redo）：走 effective workflow，rework 状态时附加反馈参考
        let feedback = if prev_state == "rework" && !recent_notes.is_empty() {
            format!("\n\n【历史审查反馈（仅供参考，按新要求重做）】\n{}", recent_notes.join("\n---\n"))
        } else {
            String::new()
        };
        (bound.clone(), format!("{base_msg}{feedback}"))
    } else {
        // 无绑定 → 走普通 manager 会话
        let feedback = if prev_state == "rework" && !recent_notes.is_empty() {
            format!("\n\n【历史审查反馈（仅供参考，按新要求重做）】\n{}", recent_notes.join("\n---\n"))
        } else {
            String::new()
        };
        let msg = format!("{base_msg}{feedback}");
        api::chat_send(b, Some(&info.session_id), &msg).await?;
        // 无绑定路径：直接跳到状态更新（wf_run = None）
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
        let view = store.view(store.get(id).expect("刚 persist 的任务必然存在"));
        return Ok(view);
    };
    let wf = load_workflow(&wf_name, &b.cwd).map_err(|e| {
        ApiError::bad_request(format!("workflow '{wf_name}' 加载失败：{e}"))
    })?;
    let event_tx = api::session_event_sender(b, &info.session_id).await?;
    // workflow 绑定分支不走 chat_send/submit_input，这里把派发消息
    // 显式记为 session 的用户诉求，否则 advisor 审查拿到空问题。
    api::session_record_user_input(b, &info.session_id, &msg).await?;
    let wf_run = Some((wf, event_tx, Arc::new(AtomicBool::new(false)), msg));

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
            // 复用该 session 的暂停门：用户 ⏸ 时这个看板 workflow 也一起停。
            let agent_pause_gate =
                api::session_pause_gate(&b2, &info.session_id).await.ok();
            let session_id = info.session_id.clone();
            // advisor intervene 暂停门：判「等待用户拍板」时流水线 park。
            let advisor_pause =
                api::session_advisor_pause_gate(&b2, &session_id).await.ok();
            // per-turn 取消旗标：step 无硬超时，用户点「终止当前任务」
            // 靠它掐掉卡住的 step（否则只能 session 级全杀）。
            let turn_cancel =
                api::session_turn_cancel_flag(&b2, &session_id).await.ok();
            let ctx = WorkflowRunContext {
                merged: Arc::new(b2.merged.read().clone()),
                resolver: b2.resolver.clone(),
                default_params: GenerateParams::default(),
                cwd: b2.cwd.clone(),
                event_tx: event_tx.clone(),
                cancel_flag: cancel.clone(),
                turn_cancel_flag: turn_cancel.clone(),
                depth: 0,
                // 顶层 run：自己就是嵌套链的根。
                root_wf_id: None,
                agent_pause_gate: agent_pause_gate.clone(),
                // 跑在真实 session 事件流上：分派建 subsession、过
                // advisor gate，与 manager 的 delegate 一致。
                subsession_store: Some(b2.subsession_store.clone()),
                session_id: Some(session_id.clone()),
                advisor_gate: latte_agent_core::advisor_monitor::AdvisorMonitorConfig {
                    enabled: b2.merged.read().advisor.enabled(),
                    ..latte_agent_core::advisor_monitor::AdvisorMonitorConfig::default()
                }
                .runner_gate(),
                advisor_pause: advisor_pause.clone(),
                staging: None,
            };
            let result = run_workflow(&wf, &msg2, &ctx).await;
            // 开发流跑完（非 code_review 本身）→ 链式自动审查。
            //
            // 但**没有代码改动就不审**：`explore` / `learn` 这类纯探索流
            // 产出的是文档结论，`git diff` 是空的，套上 4 步 code_review
            // 只会让 programmer/reviewer/tester 围着一份文档空转，并把
            // 行号错位当成 blocking（实测实录：explore 之后
            // 自动链了 code_review，首步 programmer 就说「该任务是探索
            // 任务…git diff 是空的，没有源码改动」）。
            let changed = workspace_has_reviewable_changes(&b2.cwd);
            let chain_review =
                result.is_ok() && wf.name != "code_review" && changed != Some(false);
            let dev_summary = result.as_ref().ok().cloned().unwrap_or_default();
            finish_workflow_run(&b2, &task_id, result, &cancel, &session_id);
            if chain_review {
                chain_code_review(&b2, &task_id, msg2, dev_summary, event_tx, agent_pause_gate.clone(), session_id).await;
            } else if changed == Some(false) {
                // 让「为什么没有自动审查」在看板上可见，而不是静默跳过。
                let mut store = b2.tasks.write();
                if let Some(t) = store.get_mut(&task_id) {
                    t.push_note(
                        "workflow",
                        format!(
                            "跳过自动代码审查：workflow '{}' 未产生代码改动（工作区无\
                             待审查变更），本任务产出为文档/结论类。",
                            wf.name
                        ),
                        now_ms(),
                    );
                    if let Err(e) = store.persist(&task_id) {
                        eprintln!("[tasks] persist {task_id} after skip review: {e}");
                    }
                }
            }
        });
    }
    Ok(view)
}

/// 容器父任务派发：父任务本身**不跑 workflow**，只作为“子任务全部完成
/// 即完成”的聚合节点。
///
/// 动作：
/// 1. 把父任务迁到 `in_progress`（容器标记，附说明），使 scheduler 不再
///    把它当叶子重复挑起；子任务完成后 [`maybe_complete_parent`] 会把它
///    自动收尾到 `done`。
/// 2. 逐个派发它处于 `todo` 的子任务（按 sub_order）。子任务各自带 workflow
///    绑定，走正常的 workflow / manager 派发路径，是真正干活的执行单元。
///    受并发上限与同族/跨族互斥约束——被 429/409 挡下的子任务留在 `todo`，
///    由 scheduler 后续补派。
///
/// 幂等：父任务已 `in_progress` 时只补派 `todo` 子任务，不重复迁移状态。
async fn dispatch_container(
    b: &UiBackend,
    id: &str,
    actor: &str,
) -> Result<TaskView, ApiError> {
    // 1. 父任务迁 in_progress（容器标记）。仅当当前是 todo/rework 时迁移。
    let now = now_ms();
    let child_ids: Vec<String> = {
        let mut store = b.tasks.write();
        let t = store
            .get_mut(id)
            .ok_or_else(|| ApiError::not_found(format!("task {id:?} 不存在")))?;
        if t.state == "todo" || t.state == "rework" {
            t.set_state(
                "in_progress",
                actor,
                Some("容器任务：派发子任务，等子任务全部完成后自动收尾".into()),
                now,
            );
            t.scheduled_at = None;
            t.updated_at = now;
            store.persist(id).map_err(ApiError::internal)?;
        }
        store
            .children_of(id)
            .into_iter()
            .filter(|c| c.state == "todo")
            .map(|c| c.id)
            .collect()
    };

    // 2. 逐个派发 todo 子任务。子任务派发失败（并发上限 / 范围互斥）不
    //    影响父任务容器状态——留在 todo，scheduler 后续补派。
    for cid in child_ids {
        match Box::pin(dispatch_task(b, &cid, actor, "redo")).await {
            Ok(_) => {}
            Err(e) => {
                eprintln!(
                    "[tasks] container {id}: dispatch child {cid} skipped ({}): {}",
                    e.status, e.message
                );
            }
        }
    }

    let store = b.tasks.read();
    let t = store
        .get(id)
        .ok_or_else(|| ApiError::not_found(format!("task {id:?} 不存在")))?;
    Ok(store.view(t))
}

/// `POST /api/tasks/:id/refine` 响应：拆分会话的 session id。
#[derive(Serialize, Clone, Debug)]
pub struct RefineTaskResponse {
    pub session_id: String,
}

/// `POST /api/tasks/:id/refine` — 拆分子任务：新建 ui-session 跑
/// `task_refine` workflow（manager 分析任务后用 plan 工具提交子任务
/// 清单），用户在该 session 的弹窗里勾选导入。**不改变任务状态、不
/// 记 run**——拆分是规划动作不是执行，任务留在原状态等子任务导入。
///
/// 只拆根任务（子任务不能再嵌套）；执行中（in_progress/merging）的
/// 任务不可拆。
pub async fn refine_task(b: &UiBackend, id: &str) -> Result<RefineTaskResponse, ApiError> {
    // 1. 读锁内校验 + 收集消息素材（不持锁跨 await）。
    let (title, description) = {
        let store = b.tasks.read();
        let t = store
            .get(id)
            .ok_or_else(|| ApiError::not_found(format!("task {id:?} 不存在")))?;
        if t.parent_id.is_some() {
            return Err(ApiError::bad_request(format!(
                "task {id:?} 本身是子任务，不能再拆（只支持一层父子）"
            )));
        }
        if t.state == "in_progress" || t.state == "merging" {
            return Err(ApiError::bad_request(format!(
                "state {:?} 不可拆分（执行中的任务请先中止）",
                t.state
            )));
        }
        (t.title.clone(), t.description.clone())
    };

    // 2. workflow 必须在建 session 之前加载成功（失败不改任何状态）。
    let wf = load_workflow("task_refine", &b.cwd)
        .map_err(|e| ApiError::bad_request(format!("workflow 'task_refine' 加载失败：{e}")))?;

    // 3. 建 session + label + 记录用户诉求（advisor 审查要非空问题）。
    let info = api::create_session(b).await?;
    let label = format!("[{id}] 拆分 {}", title.chars().take(20).collect::<String>());
    api::set_session_label(b, &info.session_id, &label)?;
    let msg = format!(
        "[任务拆分 {id}] {title}\n描述：{description}\n请把该任务拆分为可独立执行、可独立验收的子任务，并用 plan 工具一次性提交完整清单（导入后它们将作为 {id} 的子任务出现在任务看板）。"
    );
    api::session_record_user_input(b, &info.session_id, &msg).await?;
    let event_tx = api::session_event_sender(b, &info.session_id).await?;

    // 4. 登记 拆分会话 → 父任务 映射（server 侧，页面刷新不丢）+ 任务
    //    历史记一笔（不改状态），方便追溯拆分会话。
    b.refine_parents
        .write()
        .insert(info.session_id.clone(), id.to_string());
    {
        let mut store = b.tasks.write();
        if let Some(t) = store.get_mut(id) {
            t.push_note(
                "user",
                format!("已发起拆分（session {}）", info.session_id),
                now_ms(),
            );
            if let Err(e) = store.persist(id) {
                eprintln!("[tasks] persist {id} after refine: {e}");
            }
        }
    }

    // 5. 后台跑 workflow（不阻塞 HTTP 响应）；plan 工具会在该 session
    //    广播 PlanProposed，弹窗导入时前端带上 parent_id=id。
    let b2 = b.clone();
    let session_id = info.session_id.clone();
    let task_id = id.to_string();
    tokio::spawn(async move {
        let agent_pause_gate = api::session_pause_gate(&b2, &session_id).await.ok();
        let advisor_pause = api::session_advisor_pause_gate(&b2, &session_id).await.ok();
        // per-turn 取消旗标：任务细化也走多 step 流水线，留终止逃生口。
        let turn_cancel = api::session_turn_cancel_flag(&b2, &session_id).await.ok();
        let ctx = WorkflowRunContext {
            merged: Arc::new(b2.merged.read().clone()),
            resolver: b2.resolver.clone(),
            default_params: GenerateParams::default(),
            cwd: b2.cwd.clone(),
            event_tx: event_tx.clone(),
            cancel_flag: Arc::new(AtomicBool::new(false)),
            turn_cancel_flag: turn_cancel,
            depth: 0,
            // 顶层 run：自己就是嵌套链的根。
            root_wf_id: None,
            agent_pause_gate: agent_pause_gate.clone(),
            subsession_store: Some(b2.subsession_store.clone()),
            session_id: Some(session_id.clone()),
            advisor_gate: latte_agent_core::advisor_monitor::AdvisorMonitorConfig {
                enabled: b2.merged.read().advisor.enabled(),
                ..latte_agent_core::advisor_monitor::AdvisorMonitorConfig::default()
            }
            .runner_gate(),
            advisor_pause: advisor_pause.clone(),
            staging: None,
        };
        let result = run_workflow(&wf, &msg, &ctx).await;
        if let Err(e) = &result {
            eprintln!("[tasks] refine workflow for {task_id} failed: {e}");
        }
        record_refine_outcome(&b2, &task_id, &session_id, result);
    });
    Ok(RefineTaskResponse {
        session_id: info.session_id,
    })
}

/// `POST /api/tasks/dispatch-ready` 响应。
#[derive(Serialize, Clone, Debug)]
pub struct DispatchReadyResponse {
    /// 成功派发：(task_id, session_id)。
    pub dispatched: Vec<(String, String)>,
    /// 跳过（任务留原状态等位）：(task_id, 原因)。
    pub skipped: Vec<(String, String)>,
}

/// 批量派发的默认并发上限（同时 in_progress 的任务数），可用
/// `LATTE_DISPATCH_MAX_CONCURRENT` 环境变量覆盖。
pub const DEFAULT_DISPATCH_MAX_CONCURRENT: usize = 3;

fn dispatch_max_concurrent_from_env() -> usize {
    std::env::var("LATTE_DISPATCH_MAX_CONCURRENT")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_DISPATCH_MAX_CONCURRENT)
}

/// `POST /api/tasks/dispatch-ready` — 一键/自动批量派发：全部 todo 任务
/// 按 priority 升序（1 最高，同级按 id 保证确定性）逐个调 [`dispatch_task`]：
///
/// - 当前 in_progress 数已达 `max_concurrent`（None = 读
///   `LATTE_DISPATCH_MAX_CONCURRENT`，缺省 3）→ 该任务及剩余任务全部
///   跳过（reason=并发上限），留 todo 等下一轮；
/// - `dispatch_task` 返回 409（同族/paths 冲突）→ 跳过记原因，任务留
///   todo 等在跑任务完成；
/// - 其他错误 → 记原因继续。
///
/// 每成功派发一个任务即置 in_progress，因此下一轮迭代的并发计数与
/// paths/同族冲突判断读的都是最新 store，自然生效。`actor` ∈
/// `user`（一键派发）/ `scheduler`。
pub async fn dispatch_ready(
    b: &UiBackend,
    actor: &str,
    max_concurrent: Option<usize>,
) -> DispatchReadyResponse {
    let max = max_concurrent.unwrap_or_else(dispatch_max_concurrent_from_env);
    // 候选快照：todo 任务按 (priority, id) 升序。快照后状态被并发改掉
    // 的任务由 dispatch_task 内部的状态校验兜底（400 → 记原因跳过）。
    let ready: Vec<String> = {
        let store = b.tasks.read();
        let mut v: Vec<(i64, String)> = store
            .list()
            .into_iter()
            .filter(|t| t.state == "todo")
            .map(|t| (t.priority, t.id.clone()))
            .collect();
        v.sort();
        v.into_iter().map(|(_, id)| id).collect()
    };
    let mut resp = DispatchReadyResponse {
        dispatched: Vec::new(),
        skipped: Vec::new(),
    };
    for id in ready {
        let running = b
            .tasks
            .read()
            .list()
            .iter()
            .filter(|t| t.state == "in_progress")
            .count();
        if running >= max {
            resp.skipped
                .push((id, format!("并发上限：已有 {running} 个任务在跑（上限 {max}），等位")));
            continue;
        }
        match dispatch_task(b, &id, actor, "redo").await {
            Ok(view) => {
                let session_id = view
                    .task
                    .runs
                    .last()
                    .map(|r| r.session_id.clone())
                    .unwrap_or_default();
                resp.dispatched.push((id, session_id));
            }
            Err(e) => {
                resp.skipped.push((id, e.message));
            }
        }
    }
    resp
}

/// agent 自身的簿记目录：它们的增删不构成「代码改动」，不该触发代码审查。
const BOOKKEEPING_PREFIXES: [&str; 4] = [".latte/", ".omc/", ".omp/", ".git/"];

/// 从 `git status --porcelain` 输出判断是否存在**值得审查**的改动。
///
/// 与落盘/进程分离的纯函数，便于单测。规则：逐行取路径，滤掉 agent
/// 簿记目录（`.latte/` 等——每次 run 都会写 trace/task json，若算进去
/// 则「有改动」永真，判据失效）；剩下任何一条即认为有待审查变更。
fn porcelain_has_reviewable_changes(porcelain: &str) -> bool {
    porcelain.lines().any(|line| {
        // porcelain v1 行格式：`XY <path>`，重命名为 `R  old -> new`。
        let path = line.get(3..).unwrap_or("").trim();
        let path = path.rsplit(" -> ").next().unwrap_or(path);
        let path = path.trim_matches('"');
        !path.is_empty() && !BOOKKEEPING_PREFIXES.iter().any(|p| path.starts_with(p))
    })
}

/// 工作区是否存在值得代码审查的改动。
///
/// - `Some(true)`  有待审查变更 → 该链式审查；
/// - `Some(false)` 明确没有（干净工作区）→ 跳过审查；
/// - `None`        无法判定（非 git 仓库 / git 不可用）→ 调用方保守处理
///   （维持链式审查的历史行为，不因探测失败而少审）。
fn workspace_has_reviewable_changes(cwd: &Path) -> Option<bool> {
    let out = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None; // 不是 git 仓库，或 git 报错 → 不做判断
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Some(porcelain_has_reviewable_changes(&text))
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
    agent_pause_gate: Option<Arc<latte_agent_core::pause_gate::AgentPauseGate>>,
    session_id: String,
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
    let advisor_pause = api::session_advisor_pause_gate(b, &session_id).await.ok();
    // per-turn 取消旗标：链式审查也可能卡在某个 step，给用户留逃生口。
    let turn_cancel = api::session_turn_cancel_flag(b, &session_id).await.ok();
    let ctx = WorkflowRunContext {
        merged: Arc::new(b.merged.read().clone()),
        resolver: b.resolver.clone(),
        default_params: GenerateParams::default(),
        cwd: b.cwd.clone(),
        event_tx,
        cancel_flag: Arc::new(AtomicBool::new(false)),
        turn_cancel_flag: turn_cancel,
        depth: 0,
        // 顶层 run：自己就是嵌套链的根。
        root_wf_id: None,
        agent_pause_gate,
        // 与派发 run 同源：分派建 subsession、过 advisor gate。
        subsession_store: Some(b.subsession_store.clone()),
        session_id: Some(session_id),
        advisor_gate: latte_agent_core::advisor_monitor::AdvisorMonitorConfig {
            enabled: b.merged.read().advisor.enabled(),
            ..latte_agent_core::advisor_monitor::AdvisorMonitorConfig::default()
        }
        .runner_gate(),
        advisor_pause,
        staging: None,
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
///
/// 例外：**容器父任务**（有子任务、自己不跑 workflow，只作为“子任务全
/// 完成即完成”的聚合节点）处于 in_progress 时，**不阻塞它自己的子任务**
/// 派发——否则子任务永远无法开跑（见 [`dispatch_task`] 的容器分流）。
/// 兄弟之间、以及在跑的子任务反过来阻塞父任务被当叶子派发，仍然有效。
fn family_running_conflict(store: &TaskStore, id: &str) -> Option<String> {
    let t = store.get(id)?;
    match &t.parent_id {
        // 派发的是子任务：只被在跑的**兄弟**阻塞；父任务作为容器在跑不算冲突。
        Some(pid) => store
            .children_of(pid)
            .into_iter()
            .map(|c| c.id)
            .find(|cid| cid != id && store.get(cid).map(|c| c.state == "in_progress").unwrap_or(false)),
        // 派发的是父任务：任一子任务在跑都算冲突（父不该被当叶子重复派发）。
        None => store
            .children_of(id)
            .into_iter()
            .map(|c| c.id)
            .find(|cid| store.get(cid).map(|c| c.state == "in_progress").unwrap_or(false)),
    }
}

/// 任务是否为“容器”：有子任务，且至少一个子任务尚未进入终态
/// （done/cancelled）。容器任务不自己跑 workflow，而是派发子任务、
/// 等子任务全部完成后由 [`maybe_complete_parent`] 自动收尾。
fn is_container_with_pending_children(store: &TaskStore, id: &str) -> bool {
    let children = store.children_of(id);
    !children.is_empty()
        && children
            .iter()
            .any(|c| c.state != "done" && c.state != "cancelled")
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
#[cfg(test)]
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
/// failed → todo；观察到 cancel → aborted → todo。
///
/// `session_id` 是**本次** run 的 session。只有「最后一条 run 未结束
/// 且正是本次 run」才迁移，其余情况一律 no-op（返回 Ok 让调用方照常
/// 清理取消旗标）：
/// - run 已被 [`abort_task`] 收尾 → 不覆盖用户的回退；
/// - 最后一条 run 是**另一次**派发 → 尤其是「中止 → 立刻重新执行」：
///   `run_workflow` 要到下个 step 边界才退出，此时新 run 已经登记，
///   不按 session 匹配就会把刚起来的新 run 标成 aborted、状态打回
///   `todo`，表现为「重新执行秒失败」。
fn apply_workflow_finish(
    store: &mut TaskStore,
    id: &str,
    result: Result<String, String>,
    cancelled: bool,
    now: i64,
    session_id: &str,
) -> Result<(), String> {
    let t = store.get_mut(id).ok_or_else(|| format!("task {id:?} 不存在"))?;
    if t.runs
        .last()
        .filter(|r| r.ended_at.is_none() && r.session_id == session_id)
        .is_none()
    {
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

/// 把「拆分（refine）」workflow 的最终结果回写任务看板。
///
/// 为什么必需：`refine_task` 此前只把失败打到 stderr，看板永远停在
/// 「已发起拆分（session …）」这一条 note 上。于是 workflow 挂掉时用户
/// 在界面上看不到任何异常，只会困惑「怎么不弹窗添加子任务」——
/// 实测实录：`task_refine` 的 gate 撞 `max_iterations=2`
/// 判 Failed，`submit` 步没执行、`plan` 没被调用、弹窗没出现，而看板
/// 上零痕迹。成功也记一笔：若成功却没弹窗，说明本次没产出可提交清单，
/// 这同样需要让用户看见。
///
/// 只记 note、**不动状态**：拆分不改变任务本身的生命周期状态。
fn record_refine_outcome(
    b: &UiBackend,
    task_id: &str,
    session_id: &str,
    result: Result<String, String>,
) {
    let note = refine_outcome_note(session_id, &result);
    let mut store = b.tasks.write();
    if let Some(t) = store.get_mut(task_id) {
        t.push_note("workflow", note, now_ms());
        if let Err(e) = store.persist(task_id) {
            eprintln!("[tasks] persist {task_id} after refine outcome: {e}");
        }
    }
}

/// 拆分结果的看板文案（与落盘分离，便于单测）。
fn refine_outcome_note(session_id: &str, result: &Result<String, String>) -> String {
    match result {
        // 截断：workflow 的错误摘要可能很长（含最后一次产出摘要），
        // 看板只留可读的开头，完整内容在 session / workflow-runs 日志里。
        Err(e) => {
            let reason: String = e.chars().take(300).collect();
            format!("拆分失败（session {session_id}）：{reason}")
        }
        Ok(_) => format!(
            "拆分流程已完成（session {session_id}）。若未出现导入弹窗，说明本次未产出\
             可提交的子任务清单，可重新发起拆分。"
        ),
    }
}

/// 后台 workflow 跑完后的收尾：迁移状态 + 落盘 + 清取消旗标。
/// 任务已被删 / persist 失败只记日志（都是正常竞态或锦上添花）。
fn finish_workflow_run(
    b: &UiBackend,
    id: &str,
    result: Result<String, String>,
    cancel: &Arc<AtomicBool>,
    session_id: &str,
) {
    let cancelled = cancel.load(Ordering::SeqCst);
    let mut store = b.tasks.write();
    // 只清理**自己**的取消旗标。「中止 → 立刻重新执行」时表里已是新 run
    // 的旗标，无条件 remove 会让新 run 从此中止不掉（abort 走到
    // take_workflow_cancel → None → 误对 workflow session 调 chat_abort）。
    if let Some(flag) = store.take_workflow_cancel(id) {
        if !Arc::ptr_eq(&flag, cancel) {
            store.register_workflow_cancel(id, flag);
        }
    }
    if let Err(e) = apply_workflow_finish(&mut store, id, result, cancelled, now_ms(), session_id) {
        eprintln!("[tasks] workflow finish {id}: {e}");
        return;
    }
    if let Err(e) = store.persist(id) {
        eprintln!("[tasks] persist {id} after workflow finish: {e}");
    }
}

/// `POST /api/tasks/:id/abort` — 中止执行：对当前 run 的 session 调
/// 现有 `chat_abort`，run 补 ended_at/result=aborted，state → todo。
///
/// **没有进行中 run 的 in_progress 任务同样可以中止**，此前这里直接报
/// 400「没有进行中的 run」，制造了两类中止不掉的死状态：
/// 1. **容器父任务**：[`dispatch_container`] 只把父任务迁 `in_progress`
///    作聚合标记、**不 push run**（干活的是子任务）。于是父任务永远
///    400，只能等子任务全部 done 才由 [`maybe_complete_parent`] 收尾
///    ——中途想停下来无路可走（实测实录）。这里改为：
///    容器的「中止」= 递归中止它在跑的子任务，自己回 `todo`。
/// 2. **服务重启后遗留的 `in_progress`**：进程没了，run 的 ended_at
///    还是 None、内存里也没有取消旗标；此时中止只是状态回退，无 run
///    可收尾也应当成功。
pub async fn abort_task(b: &UiBackend, id: &str) -> Result<TaskView, ApiError> {
    // 1. 读锁内取材：本任务的在跑 run（可能没有）+ 在跑的子任务。
    let (session_id, running_children) = {
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
        let sid = t
            .runs
            .last()
            .filter(|r| r.ended_at.is_none())
            .map(|r| r.session_id.clone());
        let kids: Vec<String> = store
            .children_of(id)
            .into_iter()
            .filter(|c| c.state == "in_progress")
            .map(|c| c.id)
            .collect();
        (sid, kids)
    };

    // 2. 容器父任务：中止 = 逐个中止在跑的子任务（它们才是真正的执行
    //    单元）。子任务中止失败不阻塞父任务回退——否则父任务又卡住了。
    //    父子最多一层嵌套（见 [`maybe_complete_parent`]），递归不会深。
    for cid in &running_children {
        if let Err(e) = Box::pin(abort_task(b, cid)).await {
            eprintln!(
                "[tasks] abort {id}: 子任务 {cid} 中止失败（{}）：{}",
                e.status, e.message
            );
        }
    }

    // 3. 有 run 才需要掐执行体。workflow run：置取消旗标（run_workflow 在
    //    下一个 step/speaker 边界退出，随后 finish_workflow_run 认出 run
    //    已被下方收尾便不再重复迁移）。workflow 不占 chat turn，chat_abort
    //    只会误杀托管 session 的 controller，故跳过。经典 run 维持原语义：
    //    session 可能已被用户删掉，中止失败不阻塞任务状态回退。
    if let Some(session_id) = &session_id {
        let wf_cancel = b.tasks.write().take_workflow_cancel(id);
        if let Some(flag) = wf_cancel {
            flag.store(true, Ordering::SeqCst);
        } else {
            let _ = api::chat_abort(b, Some(session_id)).await;
        }
    }

    let now = now_ms();
    let mut store = b.tasks.write();
    apply_abort_finish(&mut store, id, running_children.len(), now)
        .map_err(|e| ApiError::not_found(e))?;
    store.persist(id).map_err(ApiError::internal)?;
    let t = store.get(id).expect("刚 persist 的任务必然存在");
    Ok(store.view(t))
}

/// abort 的落盘部分：收尾未结束的 run（如果有）、state → todo。
/// 与网络/取消旗标解耦，便于单测容器与「无 run」两条路径。
fn apply_abort_finish(
    store: &mut TaskStore,
    id: &str,
    aborted_children: usize,
    now: i64,
) -> Result<(), String> {
    let t = store
        .get_mut(id)
        .ok_or_else(|| format!("task {id:?} 不存在"))?;
    let had_run = match t.runs.last_mut().filter(|r| r.ended_at.is_none()) {
        Some(run) => {
            run.ended_at = Some(now);
            run.result = Some("aborted".to_string());
            true
        }
        None => false,
    };
    let note = if aborted_children > 0 {
        format!("中止执行（容器任务：已中止 {aborted_children} 个在跑子任务）")
    } else if had_run {
        "中止执行".to_string()
    } else {
        "中止执行（无进行中的 run，仅回退状态）".to_string()
    };
    t.set_state("todo", "user", Some(note), now);
    t.updated_at = now;
    Ok(())
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
/// 删除本身不再走 persist 路径，这里显式 mark notion dirty，让
/// 下轮 sync 检测到「本应在 Notion 删除」（由 sync 端实现）。
/// 简化：当前只把 id 推 dirty，sync 端判断本地已无该任务则发 DELETE
/// — 见 notion_sync 的 hook（v1 暂只做 upsert；DELETE 留待 v2）。
pub fn delete_task(b: &UiBackend, id: &str) -> Result<(), ApiError> {
    let mut store = b.tasks.write();
    store.delete(id).map_err(map_store_err)?;
    store.mark_notion_dirty(id);
    Ok(())
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
        if let Err(e) = dispatch_task(b, &id, "scheduler", "redo").await {
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
            .create(title, "desc", 2, vec![], None, None, None, None, "user")
            .expect("create")
    }

    // ─── ImportTask.paths 字段名别名 ─────────────────────────────────
    //
    // 回归 jemalloc 会话事故：plan 清单把 `paths` 写成 `involved_paths`，
    // serde 静默丢弃整个数组 → 导入后 24 个任务全是 `paths: []` →
    // 派发时的跨族文件范围互斥（`path_running_conflict`）失去依据。
    // 与 `latte_agent_core::controller::PlanTask` 的别名集合保持同步。

    #[test]
    fn import_task_accepts_involved_paths_alias() {
        let t: ImportTask = serde_json::from_value(serde_json::json!({
            "title": "T-401 P2 修复",
            "involved_paths": ["src/safety_check.c"],
        }))
        .expect("必须解析成功");
        assert_eq!(
            t.paths,
            vec!["src/safety_check.c"],
            "involved_paths 必须映射到 paths，不能被静默丢弃"
        );
    }

    #[test]
    fn import_task_accepts_all_path_aliases() {
        for key in ["involved_files", "affected_paths", "affected_files", "file_paths", "files"] {
            let t: ImportTask =
                serde_json::from_value(serde_json::json!({ "title": "t", key: ["src/a.c"] }))
                    .unwrap_or_else(|e| panic!("{key}: {e}"));
            assert_eq!(t.paths, vec!["src/a.c"], "别名 {key} 必须映射到 paths");
        }
    }

    #[test]
    fn import_task_canonical_paths_still_works() {
        let t: ImportTask =
            serde_json::from_value(serde_json::json!({ "title": "t", "paths": ["src/b.c"] }))
                .expect("解析");
        assert_eq!(t.paths, vec!["src/b.c"], "规范字段名不能被别名破坏");
    }

    // ─── TaskPatch.paths：导入后可修正范围声明 ────────────────────────

    #[test]
    fn task_patch_paths_updates_and_normalizes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = test_backend(&dir);
        let id = {
            let mut store = b.tasks.write();
            store
                .create("T-405", "desc", 1, vec![], None, None, None, None, "user")
                .expect("create")
                .id
        };
        // 起点是空 paths（= 导入时字段名写错后的实际状态）。
        let got = update_task(
            &b,
            &id,
            serde_json::from_value(serde_json::json!({
                "involved_paths": [
                    "  include/jemalloc/internal/tcache_inlines.h  ",
                    "",
                    "src/tcache.c",
                    "src/tcache.c"
                ]
            }))
            .expect("patch 解析"),
        )
        .expect("update");
        assert_eq!(
            got.task.paths,
            vec!["include/jemalloc/internal/tcache_inlines.h", "src/tcache.c"],
            "别名生效 + trim + 去空串 + 去重"
        );
    }

    #[test]
    fn task_patch_without_paths_keeps_existing() {
        let p: TaskPatch =
            serde_json::from_value(serde_json::json!({ "priority": 2 })).expect("解析");
        assert!(p.paths.is_none(), "缺省 paths 必须是 None（= 不变）");
        let p2: TaskPatch =
            serde_json::from_value(serde_json::json!({ "paths": [] })).expect("解析");
        assert_eq!(p2.paths, Some(vec![]), "显式空数组 = 清空声明，与缺省区分");
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
        let resp = import_tasks(&b, req).await.expect("import");
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
        import_tasks(&b, req).await.expect("import");
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

    /// P1-编排回归（实测实录）：纯探索流（explore/learn）
    /// 产出文档、`git diff` 为空，此前仍被无条件链式套上 4 步 code_review，
    /// 导致 programmer/reviewer/tester 围着一份文档空转，还把行号错位
    /// 当 blocking。判据：工作区没有值得审查的改动就不链式审查。
    #[test]
    fn clean_worktree_has_no_reviewable_changes() {
        assert!(!porcelain_has_reviewable_changes(""), "干净工作区 = 无待审查改动");
    }

    /// agent 自身的簿记目录不算代码改动——每次 run 都会写 trace/task json，
    /// 若算进去则「有改动」永真、判据失效。
    #[test]
    fn bookkeeping_dirs_are_not_reviewable() {
        let porcelain = "?? .latte/\n?? .omc/\n?? .omp/skills/\n";
        assert!(
            !porcelain_has_reviewable_changes(porcelain),
            "仅 agent 簿记目录变动不应触发代码审查"
        );
    }

    /// 真实源码改动要触发审查（已跟踪的修改、新增未跟踪源文件都算）。
    #[test]
    fn real_source_changes_are_reviewable() {
        assert!(porcelain_has_reviewable_changes(" M src/arena.c\n"), "已跟踪修改");
        assert!(porcelain_has_reviewable_changes("?? src/newfile.c\n"), "新增未跟踪源文件");
        assert!(porcelain_has_reviewable_changes("A  include/x.h\n"), "已暂存新增");
        assert!(
            porcelain_has_reviewable_changes("?? .latte/x\n M src/arena.c\n"),
            "簿记 + 真实改动混合时应判有改动"
        );
    }

    /// 重命名行 `R  old -> new` 取目标路径判定。
    #[test]
    fn rename_entries_use_destination_path() {
        assert!(
            porcelain_has_reviewable_changes("R  src/a.c -> src/b.c\n"),
            "源码重命名算改动"
        );
        assert!(
            !porcelain_has_reviewable_changes("R  .latte/a.json -> .latte/b.json\n"),
            "簿记目录内重命名不算"
        );
    }

    /// 端到端：真实 git 仓库上验证探测结果（干净 → Some(false)，
    /// 改了源码 → Some(true)，只动簿记目录 → 仍 Some(false)）。
    #[test]
    fn workspace_detection_on_real_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(p)
                .output()
                .expect("git 可用")
        };
        if !git(&["init", "-q"]).status.success() {
            eprintln!("git init 失败，跳过该用例");
            return;
        }
        let _ = git(&["config", "user.email", "t@t"]);
        let _ = git(&["config", "user.name", "t"]);
        std::fs::write(p.join("main.c"), "int main(){}\n").unwrap();
        let _ = git(&["add", "."]);
        let _ = git(&["commit", "-qm", "init"]);

        // 干净工作区 → 明确「无待审查改动」。
        assert_eq!(
            workspace_has_reviewable_changes(p),
            Some(false),
            "干净仓库应明确判定无改动（此前会白跑一轮 code_review）"
        );

        // 只写 agent 簿记目录（模拟 explore 流只落 trace/task json）。
        std::fs::create_dir_all(p.join(".latte/tasks")).unwrap();
        std::fs::write(p.join(".latte/tasks/LAT-100.json"), "{}").unwrap();
        assert_eq!(
            workspace_has_reviewable_changes(p),
            Some(false),
            "只动 .latte/ 仍应判无代码改动"
        );

        // 真改源码 → 应判有改动。
        std::fs::write(p.join("main.c"), "int main(){return 1;}\n").unwrap();
        assert_eq!(workspace_has_reviewable_changes(p), Some(true));
    }

    /// 非 git 目录 → None（无法判定），调用方保守维持链式审查。
    #[test]
    fn non_git_dir_yields_unknown() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            workspace_has_reviewable_changes(dir.path()),
            None,
            "非 git 仓库应返回 None，让调用方保守处理（不因探测失败而少审）"
        );
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

    /// P1 回归（实测实录）：拆分 workflow 失败必须回写看板。
    /// 此前失败只打 stderr，看板永远停在「已发起拆分」，用户只看到
    /// 「不弹窗」却查不到原因。
    #[test]
    fn refine_failure_note_is_actionable() {
        let err: Result<String, String> = Err(
            "step 'gate' 循环条件「VERDICT: ACCEPT」在 2 次迭代后仍未满足\
             （已达 max_iterations=2）"
                .into(),
        );
        let note = refine_outcome_note("ui-1-2", &err);
        assert!(note.contains("拆分失败"), "必须明说失败: {note}");
        assert!(note.contains("ui-1-2"), "必须带 session 便于追溯: {note}");
        assert!(note.contains("max_iterations"), "必须保留失败原因: {note}");
    }

    /// 过长的 workflow 错误摘要要截断，避免把整份产出灌进看板。
    #[test]
    fn refine_failure_note_is_truncated() {
        let long: Result<String, String> = Err("x".repeat(5000));
        let note = refine_outcome_note("s", &long);
        assert!(note.chars().count() < 400, "应截断: {} 字符", note.chars().count());
    }

    /// 成功也要留痕：成功却没弹窗 = 本次没产出可提交清单，用户需要知道。
    #[test]
    fn refine_success_note_explains_missing_popup() {
        let ok: Result<String, String> = Ok("done".into());
        let note = refine_outcome_note("ui-9", &ok);
        assert!(note.contains("已完成"), "{note}");
        assert!(note.contains("弹窗"), "应解释没弹窗的含义: {note}");
        assert!(!note.contains("失败"), "成功路径不应出现失败字样: {note}");
    }

    /// 回写只记 note、不改任务状态（拆分不影响任务生命周期）。
    #[test]
    fn refine_outcome_records_note_without_state_change() {
        let (_dir, mut store) = tmp_store();
        let t = make_task(&mut store, "待拆分");
        let before_state = t.state.clone();
        let before_len = t.history.len();

        let task = store.get_mut(&t.id).expect("task");
        task.push_note(
            "workflow",
            refine_outcome_note("ui-x", &Err("gate 撞上限".into())),
            now_ms(),
        );

        let after = store.get(&t.id).expect("task");
        assert_eq!(after.state, before_state, "拆分回写不得改状态");
        assert_eq!(after.history.len(), before_len + 1, "应新增一条 history");
        let last = after.history.last().expect("history");
        assert_eq!(last.actor, "workflow");
        assert_eq!(last.from, None, "note 不带状态迁移");
        assert!(
            last.note.as_deref().unwrap_or("").contains("拆分失败"),
            "{:?}",
            last.note
        );
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
            .create("子", "", 3, vec![], None, Some(parent.id.clone()), None, None, "user")
            .expect("create child");
        assert_eq!(child.parent_id.as_deref(), Some(parent.id.as_str()));
        assert_eq!(child.sub_order, 0);
        // parent 必须存在。
        let err = store
            .create("孤儿", "", 3, vec![], None, Some("LAT-999".into()), None, None, "user")
            .unwrap_err();
        assert!(err.contains("不存在"), "{err}");
        // 拒绝第二层嵌套：parent_id 指向本身也是子任务的任务。
        let err = store
            .create("孙", "", 3, vec![], None, Some(child.id.clone()), None, None, "user")
            .unwrap_err();
        assert!(err.contains("最多一层"), "{err}");
        // 拒绝给已有子任务的任务设 parent。
        let other = make_task(&mut store, "另一个父");
        let err = store.validate_reparent(&parent.id, &other.id);
        assert!(err.is_err(), "已有子任务的任务不能再设 parent: {err:?}");
        // sub_order 递增。
        let child2 = store
            .create("子2", "", 3, vec![], None, Some(parent.id.clone()), None, None, "user")
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
            .create("到期", "", 3, vec![], None, None, Some(now - 1000), None, "user")
            .unwrap();
        store.get_mut(&due.id).unwrap().set_state("todo", "user", None, now);
        // 未到期 todo
        let future = store
            .create("未到期", "", 3, vec![], None, None, Some(now + 60_000), None, "user")
            .unwrap();
        store.get_mut(&future.id).unwrap().set_state("todo", "user", None, now);
        // backlog 且已到期（不应选出：只派 todo）
        store
            .create("backlog到期", "", 3, vec![], None, None, Some(now - 1000), None, "user")
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

    /// 回归：plan 工具约定「没有贴合的 workflow 必须留空」，产出就是
    /// `workflow: ""`——空串/空白必须视为「不绑定」，整单 400 是 bug
    /// （实测现场：6 个任务全部 workflow:"" 被 unknown workflow ''
    /// 拒绝）。子任务同理。
    #[test]
    fn import_treats_empty_workflow_as_unbound() {
        let (dir, mut store) = tmp_store();
        let req: ImportTasksRequest = serde_json::from_value(serde_json::json!({
            "tasks": [{
                "title": "父",
                "workflow": "",
                "subtasks": [{ "title": "子", "workflow": "  " }]
            }]
        }))
        .expect("parse");
        let created = import_tasks_into(&mut store, dir.path(), &req)
            .expect("空 workflow 不应报错");
        assert_eq!(created.len(), 2);
        let parent = store.get(&created[0]).expect("parent");
        assert_eq!(parent.workflow, None, "空串应归一化为不绑定");
        let child = store.get(&created[1]).expect("child");
        assert_eq!(child.workflow, None);
        assert_eq!(child.parent_id.as_deref(), Some(parent.id.as_str()));
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
        apply_workflow_finish(&mut store, &t.id, Ok(summary), false, now_ms(), "ui-test")
            .expect("finish");
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
        apply_workflow_finish(&mut store, &t.id, Err("boom".into()), false, now_ms(), "ui-test")
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
        apply_workflow_finish(&mut store, &t.id, Err("cancelled".into()), true, now_ms(), "ui-test")
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
            .create(title, "desc", 2, vec![], None, Some(parent.id.clone()), None, None, "user")
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
        // 容器父任务 in_progress **不再**阻塞它自己的子任务（否则子任务
        // 永远开不了跑）——父作为聚合容器在跑时，子任务照常派发。
        store
            .get_mut(&p.id)
            .unwrap()
            .set_state("in_progress", "user", None, now_ms());
        assert!(
            family_running_conflict(&store, &c2.id).is_none(),
            "容器父任务在跑不应阻塞子任务派发"
        );
        store
            .get_mut(&p.id)
            .unwrap()
            .set_state("todo", "user", None, now_ms());
        // 但子任务在跑 → 父任务被当叶子派发仍算冲突（父不该重复挑起）。
        store
            .get_mut(&c1.id)
            .unwrap()
            .set_state("in_progress", "user", None, now_ms());
        assert_eq!(family_running_conflict(&store, &p.id).as_deref(), Some(c1.id.as_str()));
    }

    #[test]
    fn container_detection_ignores_terminal_children() {
        let (_dir, mut store) = tmp_store();
        let p = make_task(&mut store, "容器父");
        let c1 = make_child(&mut store, &p, "子1");
        let c2 = make_child(&mut store, &p, "子2");
        let now = now_ms();
        // 有未完成子任务 → 是容器
        assert!(is_container_with_pending_children(&store, &p.id));
        // 无子任务的叶子 → 不是容器
        assert!(!is_container_with_pending_children(&store, &c1.id));
        // 子任务全进终态（done/cancelled）→ 不再是容器（可自行收尾）
        store.get_mut(&c1.id).unwrap().set_state("done", "user", None, now);
        store.get_mut(&c2.id).unwrap().set_state("cancelled", "user", None, now);
        assert!(!is_container_with_pending_children(&store, &p.id));
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
        let resp = import_tasks(&b, req).await.expect("import");
        let [a, c] = &resp.created[..] else {
            panic!("应创建 2 个任务");
        };
        {
            let mut store = b.tasks.write();
            let now = now_ms();
            store.get_mut(a).unwrap().set_state("in_progress", "user", None, now);
            store.get_mut(c).unwrap().set_state("todo", "user", None, now);
        }
        let err = dispatch_task(&b, c, "user", "redo").await.expect_err("重叠应 409");
        assert_eq!(err.status, 409);
        assert!(err.message.contains(a), "消息应列出冲突任务 id：{}", err.message);
        assert!(err.message.contains("src/ringbuf"), "消息应列出重叠路径：{}", err.message);
        // 被拒后任务状态不变。
        assert_eq!(b.tasks.read().get(c).unwrap().state, "todo");
    }

    // ─── 批量派发（dispatch_ready） ──────────────────────────────

    /// 造一个带 tempdir 的 UiBackend。dispatch 成功路径会真建 session：
    /// 未绑 workflow 的任务发消息给 manager——默认配置无可用模型，turn
    /// 在后台失败但不回写任务状态，任务稳定停在 in_progress。
    fn test_backend(dir: &tempfile::TempDir) -> UiBackend {
        let cfg = latte_agent_core::AgentConfig::default();
        let resolver = latte_agent_core::ModelResolver::from_config(&cfg).expect("resolver");
        UiBackend::new(crate::UiBackendConfig {
            agent_config: cfg,
            model_resolver: resolver,
            role: None,
            tier: None,
            model_id: None,
            cwd: Some(dir.path().to_path_buf()),
            agents_config: ".latte/agents.d".into(),
        })
        .expect("backend")
    }

    /// 导入一批任务并全部置 todo（无 plan_id，不触发自动调度），返回 id。
    async fn import_as_todo(b: &UiBackend, req: serde_json::Value) -> Vec<String> {
        let req: ImportTasksRequest = serde_json::from_value(req).expect("parse req");
        let resp = import_tasks(b, req).await.expect("import");
        let now = now_ms();
        let mut store = b.tasks.write();
        for id in &resp.created {
            store.get_mut(id).unwrap().set_state("todo", "user", None, now);
        }
        resp.created
    }

    /// 批量派发：3 个 todo（不同 paths）全部派出、状态 in_progress，
    /// 且按 priority 升序（1 最高）的顺序派发。
    #[tokio::test]
    async fn dispatch_ready_dispatches_all_todo_in_priority_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = test_backend(&dir);
        let ids = import_as_todo(
            &b,
            serde_json::json!({"tasks": [
                { "title": "低优先", "priority": 3, "paths": ["src/a"] },
                { "title": "高优先", "priority": 1, "paths": ["src/b"] },
                { "title": "中优先", "priority": 2, "paths": ["src/c"] }
            ]}),
        )
        .await;

        let resp = dispatch_ready(&b, "user", Some(3)).await;
        assert!(resp.skipped.is_empty(), "不应有跳过：{:?}", resp.skipped);
        assert_eq!(
            resp.dispatched.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            vec![ids[1].as_str(), ids[2].as_str(), ids[0].as_str()],
            "派发顺序应按 priority 升序（1 → 2 → 3）"
        );
        // 每个任务各拿到一个互不相同的 session。
        let mut sids: Vec<&str> = resp.dispatched.iter().map(|(_, s)| s.as_str()).collect();
        sids.sort();
        sids.dedup();
        assert_eq!(sids.len(), 3, "session id 不得重复");
        let store = b.tasks.read();
        for id in &ids {
            assert_eq!(store.get(id).unwrap().state, "in_progress");
        }
    }

    /// 并发上限：max=2 时 3 个任务派出 2 个，第 3 个 skipped 留 todo。
    #[tokio::test]
    async fn dispatch_ready_respects_concurrency_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = test_backend(&dir);
        let ids = import_as_todo(
            &b,
            serde_json::json!({"tasks": [
                { "title": "甲", "paths": ["src/a"] },
                { "title": "乙", "paths": ["src/b"] },
                { "title": "丙", "paths": ["src/c"] }
            ]}),
        )
        .await;

        let resp = dispatch_ready(&b, "user", Some(2)).await;
        assert_eq!(resp.dispatched.len(), 2);
        assert_eq!(resp.skipped.len(), 1);
        assert_eq!(resp.skipped[0].0, ids[2]);
        assert!(resp.skipped[0].1.contains("并发上限"), "{}", resp.skipped[0].1);
        let store = b.tasks.read();
        assert_eq!(store.get(&ids[2]).unwrap().state, "todo", "被限流的任务留 todo 等位");
        assert_eq!(store.get(&ids[0]).unwrap().state, "in_progress");
        assert_eq!(store.get(&ids[1]).unwrap().state, "in_progress");
    }

    /// paths 冲突：A 在跑，B 与 A 范围重叠 → B skipped 留 todo；
    /// C 不冲突 → 派出。
    #[tokio::test]
    async fn dispatch_ready_skips_path_conflicts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = test_backend(&dir);
        let ids = import_as_todo(
            &b,
            serde_json::json!({"tasks": [
                { "title": "改 ringbuf", "paths": ["src/ringbuf"] },
                { "title": "改 ringbuf 测试", "paths": ["src/ringbuf/tests"] },
                { "title": "改无关模块", "paths": ["src/other"] }
            ]}),
        )
        .await;
        // A 已在跑（不占用 dispatch_ready 的派发名额之外的上限：上限给 3）。
        b.tasks
            .write()
            .get_mut(&ids[0])
            .unwrap()
            .set_state("in_progress", "user", None, now_ms());

        let resp = dispatch_ready(&b, "user", Some(3)).await;
        assert_eq!(
            resp.dispatched.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            vec![ids[2].as_str()],
            "只有不冲突的 C 应被派出"
        );
        assert_eq!(resp.skipped.len(), 1);
        assert_eq!(resp.skipped[0].0, ids[1]);
        assert!(resp.skipped[0].1.contains(&ids[0]), "原因应指出冲突对象：{}", resp.skipped[0].1);
        assert_eq!(
            b.tasks.read().get(&ids[1]).unwrap().state,
            "todo",
            "冲突任务留 todo 等 A 完成"
        );
    }

    /// 优先级顺序：上限 1 时只有 priority 1 被派出，priority 3 等位。
    #[tokio::test]
    async fn dispatch_ready_priority_wins_under_tight_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = test_backend(&dir);
        let ids = import_as_todo(
            &b,
            serde_json::json!({"tasks": [
                { "title": "后建但急", "priority": 3, "paths": ["src/a"] },
                { "title": "最高优先", "priority": 1, "paths": ["src/b"] }
            ]}),
        )
        .await;

        let resp = dispatch_ready(&b, "user", Some(1)).await;
        assert_eq!(resp.dispatched.len(), 1);
        assert_eq!(resp.dispatched[0].0, ids[1], "priority 1 应先于 3 被派出");
        assert_eq!(resp.skipped.len(), 1);
        assert_eq!(resp.skipped[0].0, ids[0]);
        assert!(resp.skipped[0].1.contains("并发上限"), "{}", resp.skipped[0].1);
        let store = b.tasks.read();
        assert_eq!(store.get(&ids[1]).unwrap().state, "in_progress");
        assert_eq!(store.get(&ids[0]).unwrap().state, "todo");
    }

    /// import 带 plan_id（= 用户批准计划）：新建任务直接置 todo，
    /// 但不自动派发——执行由用户在任务看板手动操作。
    #[tokio::test]
    async fn import_with_plan_id_lands_in_todo_without_dispatch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = test_backend(&dir);
        let req: ImportTasksRequest = serde_json::from_value(serde_json::json!({
            "plan_id": "plan-auto-1",
            "tasks": [
                { "title": "开工甲", "paths": ["src/a"] },
                { "title": "开工乙", "paths": ["src/b"] }
            ]
        }))
        .expect("parse req");
        let resp = import_tasks(&b, req).await.expect("import");
        assert_eq!(resp.created.len(), 2);
        let store = b.tasks.read();
        for id in &resp.created {
            let t = store.get(id).unwrap();
            assert_eq!(t.state, "todo", "导入即进 todo，等用户手动派发");
            assert!(t.runs.is_empty(), "不得自动派发（无 run）");
        }
    }

    /// 不带 plan_id 的导入同样直接进 todo（导入 = 待派发，不进 backlog）。
    #[tokio::test]
    async fn import_without_plan_id_also_lands_in_todo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = test_backend(&dir);
        let req: ImportTasksRequest = serde_json::from_value(serde_json::json!({
            "tasks": [{ "title": "手动导入" }]
        }))
        .expect("parse req");
        let resp = import_tasks(&b, req).await.expect("import");
        assert_eq!(b.tasks.read().get(&resp.created[0]).unwrap().state, "todo");
    }

    /// 拆分导入：带 parent_id 的导入全部挂为该父任务的子任务。
    #[tokio::test]
    async fn import_with_parent_id_creates_children() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = test_backend(&dir);
        let parent = create_task(
            &b,
            serde_json::from_value(serde_json::json!({"title": "父任务"})).expect("parse"),
        )
        .expect("create parent");
        let req: ImportTasksRequest = serde_json::from_value(serde_json::json!({
            "parent_id": parent.task.id,
            "tasks": [{ "title": "子甲" }, { "title": "子乙" }]
        }))
        .expect("parse req");
        let resp = import_tasks(&b, req).await.expect("import");
        assert_eq!(resp.created.len(), 2);
        let store = b.tasks.read();
        for id in &resp.created {
            let t = store.get(id).unwrap();
            assert_eq!(t.parent_id.as_deref(), Some(parent.task.id.as_str()));
            assert_eq!(t.state, "todo");
        }
    }

    /// 拆分会话映射：import 只带 session_id（parent_id 缺省）时，从
    /// refine 登记的 server 侧映射解析出父任务——页面刷新后前端内存
    /// 映射丢失也能正确挂父。
    #[tokio::test]
    async fn import_resolves_parent_from_refine_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = test_backend(&dir);
        let parent = create_task(
            &b,
            serde_json::from_value(serde_json::json!({"title": "父任务"})).expect("parse"),
        )
        .expect("create parent");
        // 模拟 refine_task 登记。
        b.refine_parents
            .write()
            .insert("sess-refine-1".to_string(), parent.task.id.clone());
        let req: ImportTasksRequest = serde_json::from_value(serde_json::json!({
            "session_id": "sess-refine-1",
            "tasks": [{ "title": "子甲" }]
        }))
        .expect("parse req");
        let resp = import_tasks(&b, req).await.expect("import");
        let t = b.tasks.read().get(&resp.created[0]).unwrap().clone();
        assert_eq!(t.parent_id.as_deref(), Some(parent.task.id.as_str()));
    }

    /// 显式空串 parent_id = 用户人工选了「无父任务」，覆盖 session
    /// 映射（防止误挂）。
    #[tokio::test]
    async fn import_explicit_empty_parent_overrides_session_mapping() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = test_backend(&dir);
        let parent = create_task(
            &b,
            serde_json::from_value(serde_json::json!({"title": "父任务"})).expect("parse"),
        )
        .expect("create parent");
        b.refine_parents
            .write()
            .insert("sess-refine-1".to_string(), parent.task.id.clone());
        let req: ImportTasksRequest = serde_json::from_value(serde_json::json!({
            "session_id": "sess-refine-1",
            "parent_id": "",
            "tasks": [{ "title": "根任务甲" }]
        }))
        .expect("parse req");
        let resp = import_tasks(&b, req).await.expect("import");
        let t = b.tasks.read().get(&resp.created[0]).unwrap().clone();
        assert_eq!(t.parent_id, None, "显式空串 = 根任务，不得挂父");
    }

    /// 拆分导入校验：父任务不存在 / 父任务本身是子任务 / item 再嵌套
    /// subtasks，都整单 400。
    #[tokio::test]
    async fn import_with_parent_id_validates_parent_and_nesting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = test_backend(&dir);
        // 父任务不存在
        let req: ImportTasksRequest = serde_json::from_value(serde_json::json!({
            "parent_id": "T-999",
            "tasks": [{ "title": "子甲" }]
        }))
        .expect("parse req");
        let err = import_tasks(&b, req).await.expect_err("未知父任务应 400");
        assert_eq!(err.status, 400);
        assert!(err.message.contains("不存在"), "{}", err.message);

        let parent = create_task(
            &b,
            serde_json::from_value(serde_json::json!({"title": "父任务"})).expect("parse"),
        )
        .expect("create parent");
        // item 再嵌套 subtasks（会变成两层父子）
        let req: ImportTasksRequest = serde_json::from_value(serde_json::json!({
            "parent_id": parent.task.id,
            "tasks": [{ "title": "子甲", "subtasks": [{ "title": "孙任务" }] }]
        }))
        .expect("parse req");
        let err = import_tasks(&b, req).await.expect_err("嵌套 subtasks 应 400");
        assert!(err.message.contains("subtasks"), "{}", err.message);

        // 父任务本身是子任务：先给 parent 挂一个子任务，再拿子任务当父
        let req: ImportTasksRequest = serde_json::from_value(serde_json::json!({
            "parent_id": parent.task.id,
            "tasks": [{ "title": "子甲" }]
        }))
        .expect("parse req");
        let resp = import_tasks(&b, req).await.expect("import");
        let child_id = resp.created[0].clone();
        let req: ImportTasksRequest = serde_json::from_value(serde_json::json!({
            "parent_id": child_id,
            "tasks": [{ "title": "孙任务" }]
        }))
        .expect("parse req");
        let err = import_tasks(&b, req).await.expect_err("子任务不能再拆");
        assert!(err.message.contains("不能再拆"), "{}", err.message);
    }

    /// 拆分子任务入口：建 session 跑 task_refine workflow，任务状态不变、
    /// 不记 run，历史里留拆分会话备注。
    #[tokio::test]
    async fn refine_task_creates_session_without_state_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        // refine 依赖 task_refine workflow：写进项目 workflows.d。
        let wf_dir = dir.path().join(".latte").join("workflows.d");
        std::fs::create_dir_all(&wf_dir).expect("mkdir workflows.d");
        std::fs::write(
            wf_dir.join("task_refine.toml"),
            "name = \"task_refine\"\nmax_rounds = 1\n[[steps]]\nid = \"refine\"\nspeakers = [\"manager\"]\nprompt = \"拆分\"\n",
        )
        .expect("write workflow");
        let b = test_backend(&dir);
        let parent = create_task(
            &b,
            serde_json::from_value(serde_json::json!({"title": "大任务"})).expect("parse"),
        )
        .expect("create parent");
        b.tasks
            .write()
            .get_mut(&parent.task.id)
            .unwrap()
            .set_state("todo", "user", None, now_ms());

        let resp = refine_task(&b, &parent.task.id).await.expect("refine");
        assert!(!resp.session_id.is_empty());
        let store = b.tasks.read();
        let t = store.get(&parent.task.id).unwrap();
        assert_eq!(t.state, "todo", "拆分不改变任务状态");
        assert!(t.runs.is_empty(), "拆分不记 run");
        assert!(
            t.history
                .iter()
                .any(|h| h.note.as_deref().is_some_and(|n| n.contains(&resp.session_id))),
            "历史应记录拆分会话"
        );
    }

    /// 拆分入口校验：子任务不能再拆；执行中的任务不可拆。
    #[tokio::test]
    async fn refine_task_rejects_subtask_and_running() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = test_backend(&dir);
        let parent = create_task(
            &b,
            serde_json::from_value(serde_json::json!({"title": "父"})).expect("parse"),
        )
        .expect("create");
        let child = create_task(
            &b,
            serde_json::from_value(serde_json::json!({"title": "子", "parent_id": parent.task.id}))
                .expect("parse"),
        )
        .expect("create");
        let err = refine_task(&b, &child.task.id).await.expect_err("子任务不能再拆");
        assert_eq!(err.status, 400);
        b.tasks
            .write()
            .get_mut(&parent.task.id)
            .unwrap()
            .set_state("in_progress", "user", None, now_ms());
        let err = refine_task(&b, &parent.task.id)
            .await
            .expect_err("执行中的任务不可拆");
        assert_eq!(err.status, 400);
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
        apply_workflow_finish(&mut store, &t.id, Ok("late success".into()), false, now_ms(), "ui-test")
            .expect("finish");
        let t = store.get(&t.id).unwrap();
        assert_eq!(t.state, "todo", "不得覆盖 abort 的回退");
        assert_eq!(t.runs.last().unwrap().result.as_deref(), Some("aborted"));
        assert_eq!(t.history.len(), history_len, "不得追加 history");
    }

    /// 「中止 → 立刻重新执行」：旧 workflow 到 step 边界才退出，那时
    /// 最后一条 run 已经是新派发的 run。旧 run 的收尾必须按 session
    /// 匹配、认出不是自己就 no-op，否则新 run 被标 aborted、状态打回
    /// todo，用户看到的就是「重新执行秒失败」。
    #[test]
    fn workflow_finish_does_not_clobber_a_newer_run() {
        let (_dir, mut store) = tmp_store();
        let t = make_running_task(&mut store, "中止后重新执行");
        let now = now_ms();
        {
            let t = store.get_mut(&t.id).unwrap();
            // 旧 run（session ui-test）被 abort 收尾
            let run = t.runs.last_mut().unwrap();
            run.ended_at = Some(now);
            run.result = Some("aborted".into());
            t.set_state("todo", "user", Some("中止执行".into()), now);
            // 重新派发：新 run + in_progress
            t.set_state("in_progress", "user", None, now);
            t.runs.push(TaskRun {
                session_id: "ui-new".into(),
                started_at: now,
                ended_at: None,
                result: None,
            });
        }
        // 旧 run 的后台任务这时才收尾（cancelled=true）
        apply_workflow_finish(&mut store, &t.id, Err("cancelled".into()), true, now_ms(), "ui-test")
            .expect("finish");
        let t = store.get(&t.id).unwrap().clone();
        assert_eq!(t.state, "in_progress", "新 run 必须继续跑");
        let run = t.runs.last().unwrap();
        assert_eq!(run.session_id, "ui-new");
        assert!(run.ended_at.is_none(), "新 run 不得被旧 run 收尾");
        // 本次 run 自己的收尾照常生效
        apply_workflow_finish(&mut store, &t.id, Ok("done".into()), false, now_ms(), "ui-new")
            .expect("finish");
        assert_eq!(store.get(&t.id).unwrap().state, "human_review");
    }

    // ─── 中止（abort）的落盘部分 ─────────────────────────────────

    #[test]
    fn abort_closes_open_run_and_returns_to_todo() {
        let (_dir, mut store) = tmp_store();
        let t = make_running_task(&mut store, "叶子任务");
        apply_abort_finish(&mut store, &t.id, 0, now_ms()).expect("abort");
        let t = store.get(&t.id).unwrap();
        assert_eq!(t.state, "todo");
        let run = t.runs.last().unwrap();
        assert!(run.ended_at.is_some());
        assert_eq!(run.result.as_deref(), Some("aborted"));
        assert_eq!(t.history.last().unwrap().note.as_deref(), Some("中止执行"));
    }

    /// 容器父任务没有 run（dispatch_container 只迁状态），中止必须成功
    /// 而不是报「没有进行中的 run」（实测卡死实录）。
    #[test]
    fn abort_container_without_run_succeeds() {
        let (_dir, mut store) = tmp_store();
        let p = make_task(&mut store, "容器父");
        let _c = make_child(&mut store, &p, "子1");
        let now = now_ms();
        store
            .get_mut(&p.id)
            .unwrap()
            .set_state("in_progress", "user", None, now);
        assert!(p.runs.is_empty());
        apply_abort_finish(&mut store, &p.id, 1, now_ms()).expect("abort");
        let p = store.get(&p.id).unwrap();
        assert_eq!(p.state, "todo");
        let note = p.history.last().unwrap().note.as_deref().unwrap_or_default();
        assert!(note.contains("容器任务"), "{note}");
        assert!(note.contains('1'), "{note}");
    }

    /// 服务重启后遗留的 in_progress：run 的 ended_at 还是 None 但进程
    /// 已经没了；中止只是状态回退，也必须成功。
    #[test]
    fn abort_stale_in_progress_without_run_succeeds() {
        let (_dir, mut store) = tmp_store();
        let t = make_task(&mut store, "重启遗留");
        store
            .get_mut(&t.id)
            .unwrap()
            .set_state("in_progress", "user", None, now_ms());
        apply_abort_finish(&mut store, &t.id, 0, now_ms()).expect("abort");
        let t = store.get(&t.id).unwrap();
        assert_eq!(t.state, "todo");
        let note = t.history.last().unwrap().note.as_deref().unwrap_or_default();
        assert!(note.contains("无进行中的 run"), "{note}");
    }
}
