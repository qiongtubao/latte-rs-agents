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
    /// 任意深度任务树：`parent_id` 指向上级任务；校验只要求 parent
    /// 存在且无环（新 parent 不能是本任务自身或其后代）。
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
    /// 任务声明涉及的文件/目录前缀（相对项目根），用于派发时的文件
    /// 范围互斥（见 [`path_running_conflict`] / [`family_running_conflict`]）。
    /// 空 = 未声明，无法证明不冲突 —— 同族任务此时退回整棵树串行。
    /// **想让同族任务并行，就必须两边都声明且不重叠。**
    /// 旧落盘文件无此字段 → 空。
    #[serde(default)]
    pub paths: Vec<String>,
    /// 依赖的任务 id：全部进入终态（done/cancelled）后本任务才可派发。
    /// 空 = 无依赖 → 与同族兄弟**并行**（v2 语义；此前同族一律串行）。
    ///
    /// 顺序约束一律靠这个字段显式表达，不再靠"同一棵树"隐式推断——
    /// 「先写测试再实现」这种真依赖必须写出来，拆分 workflow 的产出
    /// 里 `dependencies` 字段过去被静默丢弃，现在收编进这里。
    ///
    /// `cancelled` 依赖视作已满足（不会再产出），否则一个取消的上游
    /// 会把整条下游永久锁死、只能手改 JSON 落盘文件才能解开。
    #[serde(
        default,
        alias = "deps",
        alias = "depends",
        alias = "dependencies",
        alias = "blocked_by",
        alias = "after"
    )]
    pub depends_on: Vec<String>,
    /// **待派发**：用户/scheduler 派过它，但被依赖闸或文件冲突挡下了
    /// （409）。scheduler 每 5s 重试这些任务，一旦闸放开就自动开跑。
    ///
    /// 为什么需要这个标记而不是"扫所有 todo 自动派发"：导入即进 todo，
    /// 但**是否执行由用户拍板**（看板的既有约定）。没有它，「派发全部」
    /// 之后被依赖挡下的下游任务会永远躺在 todo 里等用户再点一次——
    /// 依赖功能就等于半残。有了它，语义是"用户已经表达过要跑这个任务，
    /// 只是当时还不能跑"。
    #[serde(default)]
    pub dispatch_pending: bool,
    /// 是否在**独立 git worktree** 里执行（Phase 2 隔离）。
    /// `None` = 跟随 `task_type` 的默认（见 `TaskTypeRegistry::isolated_default`）。
    ///
    /// 隔离任务的会话 cwd 是它自己的工作树，因此**无论 paths 是否重叠都能
    /// 并行**——它们物理上写的是不同的文件。代价是完成后要合并回 base，
    /// 冲突要人处置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolated: Option<bool>,
    /// 已建好的隔离工作树信息（派发时写入，合并/清理后清空）。
    /// `None` = 该任务当前没有工作树（非隔离，或已归档）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<latte_agent_core::task_worktree::TaskWorktreeInfo>,
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
            "in_progress" => vec!["view_session", "resume", "abort"],
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
    /// 后代任务总数（整棵子树递归统计，非仅直接子任务；无后代恒 0）。
    pub sub_total: usize,
    /// 后代中已终态（done/cancelled）的数量。
    pub sub_done: usize,
    /// 后代各状态计数（后代聚合）。
    pub sub_state_counts: BTreeMap<String, usize>,
    /// 未满足的依赖 id（按声明顺序）。非空 = 现在派发必然 409，前端
    /// 据此把「派发」按钮置灰并显示"等待 X"。
    pub blocked_by: Vec<String>,
    /// **生效**的隔离设置（任务声明优先，否则按 task_type 默认）。
    /// 前端用它显示 🌿 徽标，用户不必去翻类型注册表才知道会不会开树。
    pub isolated_effective: bool,
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
    /// Notion 同步的 dirty 集合：每次 persist 末尾 mark 任务 id（写内存
    /// + 落盘标记 `.latte/.tasks-dirty/<id>`），notion_sync 后台定时任务
    /// 取走并 PUT，成功删盘标记。重启/离线由盘标记兜底恢复，不漏推。
    /// 详见 `crate::notion_sync`。
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
            Ok(raw) => serde_json::from_str::<BoardMeta>(&raw)
                .unwrap_or_else(|_| BoardMeta::default_for(project_name(cwd))),
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
                            eprintln!("[tasks] 跳过无法解析的任务文件 {}: {e}", path.display());
                        }
                    }
                }
            }
        }
        // next_seq 对账：board.json 丢失/损坏回默认，或上次 save_meta 失败，
        // next_seq 都可能小于已存在任务的最大 seq——不纠正的话下一个 create
        // 会分配出已有 id，`save_task` 直接覆盖旧任务文件（静默丢任务）。
        // 这里取所有 `{id_prefix}-{seq}` 任务 id 的最大 seq，强制
        // next_seq ≥ max+1。
        let prefix_dash = format!("{}-", meta.id_prefix);
        let max_seq = tasks
            .keys()
            .filter_map(|id| {
                id.strip_prefix(&prefix_dash)
                    .and_then(|rest| rest.parse::<i64>().ok())
            })
            .max();
        let mut meta = meta;
        if let Some(max_seq) = max_seq {
            let floor = max_seq + 1;
            if meta.next_seq < floor {
                eprintln!(
                    "[tasks] next_seq {} < 已用最大 seq+1={floor}，对账修正",
                    meta.next_seq
                );
                meta.next_seq = floor;
            }
        }
        // 兜底：board.json 损坏/缺失时把修正后的 meta 回写（原子写），
        // 避免每次启动都靠内存对账兜同一份坏盘。
        if meta_path.exists() {
            // 文件在但 next_seq 被对账抬过 → 盘上是坏的，回写修正值。
            if meta.next_seq
                > serde_json::from_str::<BoardMeta>(
                    &std::fs::read_to_string(&meta_path).unwrap_or_default(),
                )
                .map(|m| m.next_seq)
                .unwrap_or(0)
            {
                let _ = write_atomic_json(&meta_path, &meta);
            }
        } else {
            let _ = write_atomic_json(&meta_path, &meta);
        }
        Ok(Self {
            dir,
            meta,
            tasks,
            workflow_cancels: HashMap::new(),
            notion_dirty: {
                // 重启/离线后从 `.latte/.tasks-dirty/` 重建内存 dirty 集合：
                // 上次进程没推出去的标记落盘还在，恢复后继续推 Notion。
                let mut s = crate::notion_sync::DirtySet::default();
                for id in crate::notion_sync::load_dirty_from_disk(cwd) {
                    s.restore_id(id);
                }
                s
            },
        })
    }

    /// 项目根（dirty 标记目录 `.latte/.tasks-dirty/` 所在）：
    /// `self.dir` = `<cwd>/.latte/tasks`，上溯两级。
    /// `<cwd>/.latte/tasks/imported-plans/<plan_id>` 目录：plan_id 已被
    /// import_tasks 成功消费的标记。两 tab 同 origin 各自 import 同
    /// plan_id 时，第二个看到 marker 就在 409 拒绝，避免重复造任务。
    pub(crate) fn imported_plans_dir(&self) -> std::path::PathBuf {
        self.dir.join("imported-plans")
    }
    /// plan_id 是否已成功 import（落盘标记存在）。跨重启稳定——服务
    /// 进程死了 marker 也仍在，下次重启起来仍然判重。
    pub fn plan_already_imported(&self, plan_id: &str) -> bool {
        self.imported_plans_dir()
            .join(plan_id)
            .is_file()
    }
    pub(crate) fn project_root(&self) -> &Path {
        self.dir
            .parent()
            .and_then(|p| p.parent())
            .unwrap_or(std::path::Path::new("."))
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
    /// 测试用：直接塞一个 task 进内存索引（不走 create 的 next_seq/落盘）。
    /// 给反查类/取消旗标类测试造状态用。
    #[cfg(test)]
    pub(crate) fn insert_task_for_test(&mut self, task: Task) {
        self.tasks.insert(task.id.clone(), task);
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

    /// 沿 parent 链爬到根任务 id（无 parent 即自身）。visited 集合
    /// 兜底：既有数据异常成环时返回环入口，不死循环。
    pub fn root_of(&self, id: &str) -> String {
        let mut cur = id.to_string();
        let mut visited = std::collections::HashSet::new();
        visited.insert(cur.clone());
        while let Some(pid) = self.tasks.get(&cur).and_then(|t| t.parent_id.clone()) {
            if !visited.insert(pid.clone()) {
                break;
            }
            cur = pid;
        }
        cur
    }

    /// 全部后代 id（BFS：按层级再按 sub_order 展开，结果稳定）。
    /// visited 集合兜底防环。
    pub fn descendants_of(&self, id: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut visited = std::collections::HashSet::new();
        visited.insert(id.to_string());
        let mut queue: std::collections::VecDeque<String> =
            std::collections::VecDeque::from([id.to_string()]);
        while let Some(cur) = queue.pop_front() {
            for c in self.children_of(&cur) {
                if visited.insert(c.id.clone()) {
                    out.push(c.id.clone());
                    queue.push_back(c.id);
                }
            }
        }
        out
    }

    /// 无环校验：把 `child_id` 挂到 `parent_id` 下是否会成环——沿
    /// parent 链上爬，命中 `child_id` 即成环（新 parent 是本任务自身
    /// 或其后代）。visited 集合兜底：既有数据异常成环时终止上爬而不
    /// 死循环（此时放行，新边本身不加剧异常）。
    fn check_no_cycle(&self, child_id: &str, parent_id: &str) -> Result<(), String> {
        let mut visited = std::collections::HashSet::new();
        let mut cur = Some(parent_id.to_string());
        while let Some(pid) = cur {
            if pid == child_id {
                return Err(format!(
                    "parent_id {pid:?} 会形成环（是本任务自身或其后代）"
                ));
            }
            if !visited.insert(pid.clone()) {
                break;
            }
            cur = self.tasks.get(&pid).and_then(|t| t.parent_id.clone());
        }
        Ok(())
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
                let cwd = self
                    .dir
                    .parent()
                    .and_then(|p| p.parent())
                    .unwrap_or(std::path::Path::new("."));
                let reg = crate::task_types::TaskTypeRegistry::load(cwd);
                if !reg.types.is_empty() && !reg.is_known(tt) {
                    return Err(format!("unknown task_type '{tt}'"));
                }
                Some(tt.to_string())
            }
        };
        // 父子规则（§3.3，任意深度任务树）：parent 必须存在且无环
        // （新 parent 链上爬不得回到新任务自身；新任务尚无后代，环
        // 只会来自异常数据，visited 集合兜底防死循环）。
        let id = format!("{}-{}", self.meta.id_prefix, self.meta.next_seq);
        if let Some(pid) = &parent_id {
            if !self.tasks.contains_key(pid) {
                return Err(format!("parent_id {pid:?} 不存在"));
            }
            self.check_no_cycle(&id, pid)?;
        }
        let now = now_ms();
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
            depends_on: vec![],
            dispatch_pending: false,
            isolated: None,
            worktree: None,
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
        let cwd = self.project_root().to_path_buf();
        self.notion_dirty.mark(&cwd, &id);
        Ok(task)
    }

    /// 把内存中的任务原子写回 `<id>.json`。
    pub fn save_task(&self, task: &Task) -> Result<(), String> {
        write_atomic_json(&self.dir.join(format!("{}.json", task.id)), task)
    }

    /// 内存里的 `id` 当前值落盘（配合 `get_mut` 原地改后用）。
    /// 同时把任务 id 推入 Notion 同步的 dirty 集合（内存 + 落盘标记
    /// 双写，重启/离线不漏推）。
    pub fn persist(&mut self, id: &str) -> Result<(), String> {
        let t = self
            .tasks
            .get(id)
            .ok_or_else(|| format!("task {id:?} 不存在"))?;
        self.save_task(t)?;
        let cwd = self.project_root().to_path_buf();
        self.notion_dirty.mark(&cwd, id);
        Ok(())
    }

    /// 手动把任务 id 标记为待同步（`create` / `delete` 路径不走
    /// `persist`，需要调用方显式标 dirty）。`delete` 路径在
    /// `crate::api::delete_task` 处统一调。
    pub fn mark_notion_dirty(&mut self, id: &str) {
        let cwd = self.project_root().to_path_buf();
        self.notion_dirty.mark(&cwd, id);
    }

    /// 取出当前 dirty 集合（清空），返回 id 列表。
    pub fn take_notion_dirty(&mut self) -> Vec<String> {
        self.notion_dirty.take_dirty()
    }

    /// 当前 dirty 数量（用于监控/测试）。
    pub fn notion_dirty_count(&self) -> usize {
        self.notion_dirty.dirty_count()
    }
    /// sync_once 期间的 take → 用户 mark → 清盘这一 race 防护：take
    /// 后某 id 是否仍在内存 dirty 集合里（被重新 mark 过）。sync 据此
    /// 决定是否跳过 `clear_dirty_on_disk`，避免把新 mark 静默吃掉。
    pub fn notion_dirty_contains(&self, id: &str) -> bool {
        self.notion_dirty.contains_after_take(id)
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
            std::fs::rename(&src, &dst).map_err(|e| format!("archive {}: {e}", src.display()))?;
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
    /// 翻转某任务 workflow 取消旗标（**不**移除）——给 chat_abort 等
    /// 「连带取消」场景用：多个取消路径可能并发触发（abort_task 也可能
    /// 在另一线程走 take 路径），把旗标从 map 里拿走会让另一边误判
    /// 已取消。旗标本身是 `Arc<AtomicBool>`，原地翻 + 留着即可。
    pub fn flip_workflow_cancel(&mut self, id: &str) -> bool {
        if let Some(flag) = self.workflow_cancels.get(id) {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            true
        } else {
            false
        }
    }
    /// 反查 session_id → 当前持有它的任务 id（仅看 in_progress 且最新
    /// run 未结束的）。chat_abort / delete_session 用：连带取消该任务
    /// 的 workflow 取消旗标。多个任务可能在历史里用过同一 session
    /// （轮转 token + 重派），但只有当下还在跑的那条算。
    pub fn task_owning_active_session(&self, session_id: &str) -> Option<String> {
        for t in self.tasks.values() {
            if t.state != "in_progress" {
                continue;
            }
            if let Some(last) = t.runs.last() {
                if last.session_id == session_id && last.ended_at.is_none() {
                    return Some(t.id.clone());
                }
            }
        }
        None
    }

    /// 给已有任务设置 parent 时的校验（v1 的 update 不改 parent，仅
    /// 备后续用）：parent 必须存在，且无环（新 parent 不能是本任务
    /// 自身或其后代）。任意深度任务树允许带着子树整体移动。
    pub fn validate_reparent(&self, id: &str, parent_id: &str) -> Result<(), String> {
        if !self.tasks.contains_key(parent_id) {
            return Err(format!("parent_id {parent_id:?} 不存在"));
        }
        self.check_no_cycle(id, parent_id)
    }

    /// 依赖声明的归一化 + 校验（不落库，供 [`Self::set_depends_on`] 与
    /// 导入路径共用）。返回去重后的依赖 id 列表（保持首次出现顺序）。
    ///
    /// 拒绝四类声明，全部会导致永久卡死或语义混乱，宁可在入口报错：
    /// 1. 依赖自身；
    /// 2. 依赖不存在的任务（写错 id 会静默变成"无依赖"，正是 `paths`
    ///    踩过的坑——静默丢弃比报错难查得多）；
    /// 3. 依赖自己的祖先或后代：父子是**聚合**关系不是依赖关系，父任务
    ///    作为容器本来就要等子任务全完成，再加一条反向依赖就是死锁；
    /// 4. 成环（沿依赖边上溯回到自身）。
    pub fn normalize_depends_on(&self, id: &str, deps: Vec<String>) -> Result<Vec<String>, String> {
        let mut out: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for raw in deps {
            let dep = raw.trim().to_string();
            if dep.is_empty() {
                continue;
            }
            if dep == id {
                return Err(format!("任务 {id} 不能依赖自身"));
            }
            if !self.tasks.contains_key(&dep) {
                return Err(format!("依赖的任务 {dep:?} 不存在"));
            }
            if is_ancestor_of(self, &dep, id) || is_ancestor_of(self, id, &dep) {
                return Err(format!(
                    "{dep} 与 {id} 是父子/祖孙关系，不能声明为依赖（父任务作为容器本就等子任务完成）"
                ));
            }
            if self.dep_reaches(&dep, id) {
                return Err(format!("依赖 {dep} 会形成环（{dep} 直接或间接依赖 {id}）"));
            }
            if seen.insert(dep.clone()) {
                out.push(dep);
            }
        }
        Ok(out)
    }

    /// 沿依赖边（`depends_on`）从 `from` 出发能否到达 `target`。
    /// visited 集合兜底：既有数据异常成环时终止而不死循环。
    fn dep_reaches(&self, from: &str, target: &str) -> bool {
        let mut visited = std::collections::HashSet::new();
        let mut stack = vec![from.to_string()];
        while let Some(cur) = stack.pop() {
            if cur == target {
                return true;
            }
            if !visited.insert(cur.clone()) {
                continue;
            }
            if let Some(t) = self.tasks.get(&cur) {
                stack.extend(t.depends_on.iter().cloned());
            }
        }
        false
    }

    /// 校验 + 落库一份依赖声明（**整体替换**语义，空数组 = 清空）。
    pub fn set_depends_on(&mut self, id: &str, deps: Vec<String>) -> Result<(), String> {
        if !self.tasks.contains_key(id) {
            return Err(format!("task {id:?} 不存在"));
        }
        let deps = self.normalize_depends_on(id, deps)?;
        self.tasks.get_mut(id).expect("刚校验过存在").depends_on = deps;
        self.persist(id)
    }

    /// 未满足的依赖：返回 `(dep_id, dep_state)`，按声明顺序。空 = 就绪。
    ///
    /// 满足判据 = 依赖进入**终态**（`done` / `cancelled`）。用 `done`
    /// 而不是 `human_review` 是为了与 Phase 3 的 worktree 合并语义一致：
    /// 下游要在自己的树里看到上游的改动，上游就必须已经合回 base。
    /// 依赖任务已被删除 → 视作满足（不把下游永久锁死）。
    pub fn unsatisfied_deps(&self, id: &str) -> Vec<(String, String)> {
        let Some(t) = self.tasks.get(id) else {
            return Vec::new();
        };
        t.depends_on
            .iter()
            .filter_map(|dep| {
                let d = self.tasks.get(dep)?;
                if TERMINAL_STATES.contains(&d.state.as_str()) {
                    None
                } else {
                    Some((d.id.clone(), d.state.clone()))
                }
            })
            .collect()
    }

    /// 组装 TaskView（含后代聚合：sub_* 递归统计整棵子树的全部后代，
    /// 不只直接子任务）。
    pub fn view(&self, task: &Task) -> TaskView {
        let descendants = self.descendants_of(&task.id);
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut done = 0;
        for did in &descendants {
            let Some(c) = self.tasks.get(did) else {
                continue;
            };
            *counts.entry(c.state.clone()).or_insert(0) += 1;
            if TERMINAL_STATES.contains(&c.state.as_str()) {
                done += 1;
            }
        }
        // effective workflow 供前端直接展示（类型优先，与 redo 默认模式一致）
        let cwd = self
            .dir
            .parent()
            .and_then(|p| p.parent())
            .unwrap_or(std::path::Path::new("."));
        let reg = crate::task_types::TaskTypeRegistry::load(cwd);
        let effective = crate::task_types::effective_workflow_type_first(
            task.task_type.as_deref(),
            task.workflow.as_deref(),
            &reg,
        );
        TaskView {
            effective_workflow: effective,
            actions: task.effective_actions(),
            blocked_by: self
                .unsatisfied_deps(&task.id)
                .into_iter()
                .map(|(d, _)| d)
                .collect(),
            isolated_effective: task
                .isolated
                .unwrap_or_else(|| reg.isolated_default(task.task_type.as_deref())),
            task: task.clone(),
            sub_total: descendants.len(),
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
    /// 任务涉及的文件/目录前缀（相对项目根）：与在跑任务重叠会被拒绝
    /// 派发。**同族任务想并行必须声明它**（见 [`family_running_conflict`]）。
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
    /// 依赖的任务 id：全部 done 后才可派发。未知 id / 自依赖 / 父子
    /// 关系 / 成环一律 400（不静默丢弃）。
    #[serde(
        default,
        alias = "deps",
        alias = "depends",
        alias = "dependencies",
        alias = "blocked_by",
        alias = "after"
    )]
    pub depends_on: Vec<String>,
    /// 是否在独立 git worktree 里执行；缺省跟随 `task_type` 的默认。
    #[serde(default)]
    pub isolated: Option<bool>,
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
    /// 依赖的任务 id：缺省 = 不变；数组 = 整体替换（空数组 = 清空）。
    /// 校验同 create（未知 id / 自依赖 / 父子关系 / 成环 → 400）。
    #[serde(
        default,
        alias = "deps",
        alias = "depends",
        alias = "dependencies",
        alias = "blocked_by",
        alias = "after"
    )]
    pub depends_on: Option<Vec<String>>,
    /// 隔离开关：缺省 = 不变；`null` = 跟随类型默认；`true`/`false` = 显式。
    #[serde(default, deserialize_with = "deserialize_nullable")]
    pub isolated: Option<Option<bool>>,
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
    /// 父任务指定：拆分导入时全部任务作为该父任务的子任务创建（任意
    /// 深度任务树：父任务本身也可以是子任务，item 的 subtasks 也可
    /// 继续嵌套）。三态：非空串 = 显式指定；空串 = 显式「无父任务」
    /// （覆盖 session_id 解析，防止误挂）；缺省 = 按 session_id 查
    /// 拆分会话映射（refine 登记）。
    #[serde(default)]
    pub parent_id: Option<String>,
    /// 导入发起方 session：parent_id 缺省时用它查 `refine_parents`
    /// （看板「拆分子任务」创建的 session → 父任务）。
    #[serde(default)]
    pub session_id: Option<String>,
}

/// 导入的单个任务：除 `title` 外全部可选；`subtasks` 可任意深度嵌套。
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
    /// 依赖的任务：可写**已存在的任务 id**，也可写同一份清单里其他任务
    /// 的**标题**（拆分场景下新任务还没有 id）。解析在全部任务创建完成
    /// 后统一做（两趟），所以前向引用（依赖后面才出现的任务）同样有效。
    ///
    /// 解析不到 / 标题歧义 → 整单报错，绝不静默丢弃（`paths` 曾因静默
    /// 丢弃导致 26 个任务的范围声明永久为空）。
    #[serde(
        default,
        alias = "deps",
        alias = "depends",
        alias = "dependencies",
        alias = "blocked_by",
        alias = "after"
    )]
    pub depends_on: Vec<String>,
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
    // paths / depends_on 在 create 之后单独落（create 的参数列表已经
    // too_many_arguments）。依赖非法时把刚建的任务删掉再报错，避免留
    // 下一个"用户以为没建成、实际躺在 backlog"的孤儿。
    let id = t.id.clone();
    let paths = normalize_paths(req.paths);
    if !paths.is_empty() {
        store.get_mut(&id).expect("刚创建").paths = paths;
        store.persist(&id).map_err(ApiError::internal)?;
    }
    if !req.depends_on.is_empty() {
        if let Err(e) = store.set_depends_on(&id, req.depends_on) {
            let _ = store.delete(&id);
            return Err(ApiError::bad_request(e));
        }
    }
    if req.isolated.is_some() {
        store.get_mut(&id).expect("刚创建").isolated = req.isolated;
        store.persist(&id).map_err(ApiError::internal)?;
    }
    let t = store.get(&id).expect("刚创建").clone();
    Ok(store.view(&t))
}

/// paths 归一化：去空白、丢空串、去重（保持首次出现顺序）。派发时的
/// 范围互斥按前缀比较，空串会匹配一切，必须挡在入口。
fn normalize_paths(paths: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    paths
        .into_iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty() && seen.insert(p.clone()))
        .collect()
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

/// import item 的公共校验 + 字段解析（`import_one` 递归每层复用）：
/// title 去空白（空 → 报错）并截断到 80 字符；priority 必须在 1-4；
/// task_type / workflow 非空时必须已知。返回
/// (title, priority, task_type, workflow)。
///
/// workflow 引用必须存在：静默降级会让任务丢掉绑定的流程而无人
/// 察觉——拒绝并报错，让提交方（manager/用户）修正后重试。
/// 空串/空白 = 不绑定：plan 工具约定「没有贴合的必须留空」，产出
/// 就是 workflow: ""，不能当成 workflow 名去校验（实测现场：
/// 6 个任务全部 workflow:"" 导入被 400 整单拒绝）。
fn prepare_import_fields(
    cwd: &Path,
    item: &ImportTask,
) -> Result<(String, i64, Option<String>, Option<String>), String> {
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
    Ok((title, item.priority.unwrap_or(3), task_type, workflow))
}

/// 导入单个任务（`subtasks` 可任意深度嵌套）的校验 + 落库：递归
/// 创建，返回新建 id（父先于子，深度优先）。`parent_id` 非空时任务
/// 作为该父任务的子任务创建（拆分导入）。
fn import_one(
    store: &mut TaskStore,
    cwd: &Path,
    item: &ImportTask,
    parent_id: Option<&str>,
    created: &mut Vec<String>,
    dep_refs: &mut Vec<(String, Vec<String>)>,
    by_title: &mut HashMap<String, Vec<String>>,
) -> Result<(), String> {
    let (title, priority, task_type, workflow) = prepare_import_fields(cwd, item)?;
    let task = store.create(
        &title,
        &item.description,
        priority,
        item.labels.clone(),
        task_type,
        parent_id.map(str::to_string),
        None,
        workflow,
        "import",
    )?;
    if !item.paths.is_empty() {
        store.get_mut(&task.id).expect("刚创建").paths = normalize_paths(item.paths.clone());
        store.persist(&task.id)?;
    }
    // 依赖引用留到第二趟解析（前向引用 + 标题引用都要等全部任务建完）。
    if !item.depends_on.is_empty() {
        dep_refs.push((task.id.clone(), item.depends_on.clone()));
    }
    by_title
        .entry(title.to_lowercase())
        .or_default()
        .push(task.id.clone());
    created.push(task.id.clone());
    for sub in &item.subtasks {
        import_one(store, cwd, sub, Some(&task.id), created, dep_refs, by_title)?;
    }
    Ok(())
}

/// `POST /api/tasks/import` 的核心逻辑（可测）：顺序创建，首个失败即
/// 返回 400 语义的错误（消息含失败任务的 title 与已创建的 id）。
///
/// `pub`：CLI（无 ui-server 进程）的 PlanProposed 导入闭环复用它——
/// 见 [`import_tasks_standalone`]。两边写同一份 board.json，schema /
/// id 分配（LAT-100 起）/ history 记录天然逐字一致，不存在第二份实现。
pub fn import_tasks_into(
    store: &mut TaskStore,
    cwd: &Path,
    req: &ImportTasksRequest,
) -> Result<Vec<String>, String> {
    // 拆分导入：父任务必须存在（任意深度任务树——父任务本身也可以是
    // 子任务，item 的 subtasks 也可继续嵌套）。
    if let Some(pid) = req.parent_id.as_deref() {
        if store.get(pid).is_none() {
            return Err(format!("父任务 {pid:?} 不存在"));
        }
    }
    let mut created: Vec<String> = Vec::new();
    // 第一趟：建全部任务（含任意深度 subtasks），记录 id → 声明的依赖
    // 引用，以及标题 → id 的索引（同名标题记多个，解析时判歧义）。
    let mut dep_refs: Vec<(String, Vec<String>)> = Vec::new();
    let mut by_title: HashMap<String, Vec<String>> = HashMap::new();
    for item in &req.tasks {
        if let Err(e) = import_one(
            store,
            cwd,
            item,
            req.parent_id.as_deref(),
            &mut created,
            &mut dep_refs,
            &mut by_title,
        ) {
            return Err(format!(
                "导入任务 {:?} 失败：{e}（已创建：{created:?}）",
                item.title.trim()
            ));
        }
    }
    // 第二趟：解析依赖引用（id 或同批标题）并落库。前向引用在这里才
    // 解析，所以「A 依赖后面声明的 B」也成立。
    for (id, refs) in &dep_refs {
        let resolved = resolve_dep_refs(store, id, refs, &by_title)
            .map_err(|e| format!("任务 {id} 的 depends_on 无法解析：{e}（已创建：{created:?}）"))?;
        store
            .set_depends_on(id, resolved)
            .map_err(|e| format!("任务 {id} 的 depends_on 非法：{e}（已创建：{created:?}）"))?;
    }
    Ok(created)
}

/// 把一条依赖引用解析成任务 id：已存在的任务 id 直接用；否则按同批
/// 导入的标题匹配（trim + 忽略大小写）。歧义或找不到 → Err。
fn resolve_dep_refs(
    store: &TaskStore,
    owner: &str,
    refs: &[String],
    by_title: &HashMap<String, Vec<String>>,
) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for raw in refs {
        let r = raw.trim();
        if r.is_empty() {
            continue;
        }
        if store.get(r).is_some() {
            out.push(r.to_string());
            continue;
        }
        match by_title.get(&r.to_lowercase()).map(|v| v.as_slice()) {
            Some([one]) => out.push(one.clone()),
            Some(many) if many.len() > 1 => {
                return Err(format!(
                    "{r:?} 匹配到多个同标题任务（{}），请改用任务 id",
                    many.join("、")
                ))
            }
            _ => {
                return Err(format!(
                    "{r:?} 既不是已存在的任务 id，也不是本批任务的标题（依赖方 {owner}）"
                ))
            }
        }
    }
    Ok(out)
}

/// 无 `UiBackend` 的导入入口（CLI 的 PlanProposed 导入闭环用）：
/// 现 load 看板 → [`import_tasks_into`] → 全部落 `todo` → persist。
/// 与 [`import_tasks`] 的差别只是没有 refine 父任务解析、plan 阶段门
/// 批准与弹窗销账——那些要 session 句柄，CLI 调用方自己处理（销账走
/// `latte_agent_core::choice::dismiss_prompt`）。
///
/// 并发说明：CLI 与 ui-server 同时跑时共享 `<cwd>/.latte/tasks/`，
/// 本函数每次现 load 现写（单写者串行假设与 TaskStore 一致）；两个
/// 进程同时导入的极端场景下 next_seq 以先落盘者为准、后写者覆盖
/// board.json 元信息但任务文件不丢（按 id 一文件），重启后 load 会
/// 重新归拢。tmp+rename 保证单文件不撕裂。
pub fn import_tasks_standalone(
    cwd: &Path,
    req: &ImportTasksRequest,
) -> Result<Vec<String>, String> {
    let mut store = TaskStore::load(cwd).map_err(|e| format!("load task board: {e}"))?;
    let created = import_tasks_into(&mut store, cwd, req)?;
    // 导入即进 todo（与 import_tasks 同语义）：执行与否由用户手动派发。
    let now = now_ms();
    for id in &created {
        if let Some(t) = store.get_mut(id) {
            t.set_state("todo", "import", Some("已导入，待派发".into()), now);
        }
        store.persist(id)?;
    }
    Ok(created)
}

pub async fn import_tasks(
    b: &UiBackend,
    req: ImportTasksRequest,
) -> Result<ImportTasksResponse, ApiError> {
    // plan_id 幂等：两 tab 同 origin 各自 import 同 plan_id 时，
    // 第二个看到 marker 就在 409 拒绝，避免重复造任务。检查在拿写
    // 锁之前可降低拒绝时的成本，但同进程并发导入仍需锁内检查—见下。
    let mut req = req;
    let plan_id_owned: Option<String> = req.plan_id.clone().filter(|p| !p.is_empty());
    if let Some(plan_id) = plan_id_owned.as_deref() {
        if b.tasks.read().plan_already_imported(plan_id) {
            return Err(ApiError {
                status: 409,
                message: format!(
                    "plan {plan_id:?} 已经被本次 server 导入过一次（重复导入防护）"
                ),
            });
        }
    }
    // 父任务三态解析：显式 parent_id（含 ""=显式根任务）优先；
    // 缺省时按 session_id 查拆分会话映射（refine 登记，页面刷新不丢）。
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
        // 同进程并发二次检查（读锁检查到拿写锁之间有人抢跑的可能）：
        // 持写锁再查一次，命中则放弃。
        if let Some(plan_id) = plan_id_owned.as_deref() {
            if store.plan_already_imported(plan_id) {
                return Err(ApiError {
                    status: 409,
                    message: format!(
                        "plan {plan_id:?} 已经被本次 server 导入过一次（重复导入防护）"
                    ),
                });
            }
        }
        let created = import_tasks_into(&mut store, &b.cwd, &req).map_err(ApiError::bad_request)?;
        // 先写幂等标记，再写任务文件，最后销账 prompt。顺序：
        //   1. 写 marker（成功 → 任何重试都撞 409）
        //   2. persist 各任务（失败也不影响 marker）
        //   3. dismiss_prompt（撤销前端弹框与跨 tab 通知）
        // 即便 1-2 之间崩了，重启后 marker 仍在，下次 import 会被挡。
        if let Some(plan_id) = plan_id_owned.as_deref() {
            let dir = store.imported_plans_dir();
            let _ = std::fs::create_dir_all(&dir);
            let marker = dir.join(plan_id);
            let tmp = marker.with_extension("tmp");
            if let Err(e) = std::fs::write(&tmp, b"imported") {
                eprintln!("[tasks] write plan-import marker {plan_id}: {e}");
            } else if let Err(e) = std::fs::rename(&tmp, &marker) {
                eprintln!("[tasks] rename plan-import marker {plan_id}: {e}");
            }
        }
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
    if let Some(plan_id) = plan_id_owned.as_ref() {
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
    let mut wt_action: Option<WorktreeAction> = None;
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
            t.paths = normalize_paths(paths);
        }
        if let Some(sched) = patch.scheduled_at {
            t.scheduled_at = sched;
        }
        if let Some(iso) = patch.isolated {
            t.isolated = iso;
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
            // 用户手动把任务移出就绪列（backlog/cancelled/…）→ 撤销
            // 「待派发」，否则 scheduler 会把用户刚收回的任务又派出去。
            if t.state != "todo" && t.state != "rework" {
                t.dispatch_pending = false;
            }
            // 生命周期钩子：进入 merging/done 时自动执行对应 workflow。
            if state != prev {
                match state.as_str() {
                    "merging" => hook = Some("merge"),
                    "done" => hook = Some("task_archive"),
                    _ => {}
                }
                // 隔离工作树的收尾（Phase 2）。**合并挂在 `done` 而不是
                // `merging`**：
                // - `merging` = 合并准备（`merge` 钩子在**树里**跑验证、出提交
                //   包），不动 base——状态名就是这个意思；
                // - `done` = 真正合并回 base + 回收工作树；
                // - `cancelled` → 只回收，**保留分支**（取消一个还有未合并提交
                //   的任务不该静默销毁那些提交）。
                //
                // 为什么不在 `merging` 就合并：那样机械合并会排在 LLM 验证流之后
                // ——模型不可用、或流程停在等人拍板时，合并被无限期拖住，而用户
                // 点批准的意思就是"把它合进去"。拆开之后合并只由「标记完成」这个
                // 用户动作触发，永远不受模型状态影响。
                if t.worktree.is_some() {
                    match state.as_str() {
                        "done" => wt_action = Some(WorktreeAction::Merge),
                        "cancelled" => {
                            wt_action = Some(WorktreeAction::Cleanup { keep_branch: true })
                        }
                        _ => {}
                    }
                }
            }
        }
        t.updated_at = now;
        if hook.is_some() {
            hook_topic = Some(build_hook_topic(t));
        }
    }
    // depends_on 要跨任务校验（存在性 / 父子 / 成环），拿不到 &mut Task
    // 的作用域里做不了——放到上面的块外，用 store 级方法整体替换。
    if let Some(deps) = patch.depends_on {
        store
            .set_depends_on(id, deps)
            .map_err(ApiError::bad_request)?;
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
    // 生命周期钩子 + 工作树收尾的编排。两条路，分别对应两个状态：
    //
    // - `merging`（钩子 `merge`，无工作树动作）：钩子跑在**工作树里**——它要跑
    //   测试套件，跑主仓等于验证一份还没合并的工作区。
    // - `done`（工作树动作 `Merge` + 钩子 `task_archive`）：**先合并再归档**。
    //   合并完树就回收了，归档只能跑在主仓——而此时改动已经在主仓里，归档
    //   看到的正是最终状态。
    let worktree_root = view
        .task
        .worktree
        .as_ref()
        .map(|w| w.root.clone())
        .filter(|r| r.is_dir());
    match (hook, hook_topic, wt_action) {
        // done：合并 → 回收 → 归档（顺序不能反，归档要看合并后的主仓）
        (hook, topic, Some(action @ WorktreeAction::Merge)) => {
            spawn_worktree_action_then_hook(b, id, action, hook, topic);
        }
        // 有钩子、无合并动作（merging / 其它）：钩子跑在树里（有树的话）
        (Some(wf_name), Some(topic), action) => {
            let run_cwd = worktree_root.unwrap_or_else(|| b.cwd.clone());
            spawn_lifecycle_hook(b, id, wf_name, topic, run_cwd, action);
        }
        // 只有工作树动作（cancelled）
        (_, _, Some(action)) => spawn_worktree_action(b, id, action),
        _ => {}
    }
    Ok(view)
}

/// 先做工作树动作（合并 + 回收），**再**跑生命周期钩子。
///
/// 用于 `done`：归档要总结的是"最终进了 base 的东西"，而合并完树就没了，
/// 所以钩子只能在主仓跑、且必须排在合并之后。
fn spawn_worktree_action_then_hook(
    b: &UiBackend,
    id: &str,
    action: WorktreeAction,
    hook: Option<&str>,
    topic: Option<String>,
) {
    let b2 = b.clone();
    let id2 = id.to_string();
    let hook = hook.map(str::to_string);
    tokio::spawn(async move {
        run_worktree_action(&b2, &id2, action).await;
        if let (Some(wf_name), Some(topic)) = (hook, topic) {
            // 合并后工作树已回收 → 钩子跑主仓（改动此时就在主仓里）。
            let cwd = b2.cwd.clone();
            spawn_lifecycle_hook(&b2, &id2, &wf_name, topic, cwd, None);
        }
    });
}

/// 隔离工作树的收尾动作（状态迁移触发，后台执行）。
#[derive(Debug, Clone, Copy)]
enum WorktreeAction {
    /// 合并回 base 分支，成功后回收工作树。
    Merge,
    /// 直接回收。`keep_branch` = 保留任务分支（未合并的提交还拿得到）。
    Cleanup { keep_branch: bool },
}

/// 后台执行工作树收尾：git 操作是秒级阻塞调用，不能挡住 HTTP 处理。
///
/// 结果一律写进任务 history（看板可见）：合并成功记提交 SHA，冲突记冲突详情
/// 与"工作树/分支保留在哪"。冲突时**不改状态**——任务停在 `merging` 等人处置，
/// 这是唯一诚实的处理方式（自动硬解冲突会丢改动）。
fn spawn_worktree_action(b: &UiBackend, id: &str, action: WorktreeAction) {
    let b = b.clone();
    let id = id.to_string();
    tokio::spawn(async move { run_worktree_action(&b, &id, action).await });
}

/// [`spawn_worktree_action`] 的同体实现（可 await），供生命周期钩子跑完后
/// **串联**调用——两者并发会出两种错：钩子在主仓验证一份还没合并的改动，
/// 以及清理把钩子脚下的工作树删掉（cwd 消失，工具全报错）。
async fn run_worktree_action(b: &UiBackend, id: &str, action: WorktreeAction) {
    {
        match action {
            WorktreeAction::Merge => match merge_task_worktree(b, id).await {
                Ok(None) => {}
                Ok(Some(sha)) => {
                    {
                        let mut store = b.tasks.write();
                        if let Some(t) = store.get_mut(id) {
                            let short: String = sha.chars().take(10).collect();
                            t.push_note(
                                "worktree",
                                format!("隔离工作树已合并回 base（提交 {short}）"),
                                now_ms(),
                            );
                        }
                        let _ = store.persist(id);
                    }
                    // 合并成功 → 树没用了，回收（分支也删：提交已在 base 里）。
                    cleanup_task_worktree(b, id, false).await;
                }
                Err(e) => {
                    let info = b.tasks.read().get(id).and_then(|t| t.worktree.clone());
                    let mut store = b.tasks.write();
                    if let Some(t) = store.get_mut(id) {
                        let where_ = info
                            .map(|i| {
                                format!("工作树保留在 {}，分支 {}", i.root.display(), i.branch)
                            })
                            .unwrap_or_default();
                        t.push_note(
                            "worktree",
                            format!("⚠️ 合并回 base 失败（需人工处置）：{e}。{where_}"),
                            now_ms(),
                        );
                    }
                    let _ = store.persist(id);
                }
            },
            WorktreeAction::Cleanup { keep_branch } => {
                cleanup_task_worktree(b, id, keep_branch).await;
            }
        }
    }
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
fn spawn_lifecycle_hook(
    b: &UiBackend,
    id: &str,
    wf_name: &str,
    topic: String,
    // 钩子 workflow 的工作目录：隔离任务传它的 worktree 根。`merge` 钩子会跑
    // 测试套件，必须跑在**改动所在的树**里——跑主仓等于验证一份还没合并的
    // 工作区，`task_archive` 也一样（要总结的是这次的改动）。
    run_cwd: PathBuf,
    // 钩子跑完后串联执行的工作树动作。刻意**串联**而不是并发：合并要在验证
    // 之后，清理要在钩子用完那棵树之后（并发清理会把钩子的 cwd 删掉）。
    then: Option<WorktreeAction>,
) {
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
        // 配置与 advisor 闸门在**同一次读锁**里取齐（见 workflow_cfg_snapshot）。
        let (merged_cfg, advisor_gate) = b.workflow_cfg_snapshot();
        let ctx = WorkflowRunContext {
            merged: merged_cfg,
            resolver: b.resolver.clone(),
            default_params: GenerateParams::default(),
            cwd: run_cwd,
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
            advisor_gate,
            advisor_pause,
            staging: None,
            // 顶层 run 自己建（见 run_workflow_inner），宿主传 None。
            dispatch_budget: None,
        };
        let result = run_workflow(&wf, &topic, &ctx).await;
        // 写锁必须在**内层块**里用完就还：`parking_lot` 的 guard 不是 Send，
        // 跨下面那个 await 持有它会让整个 async block 不 Send、tokio::spawn
        // 直接编译不过（显式 drop 也不行——生成器状态机仍认为它活着）。
        {
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
        }
        // 钩子跑完才动工作树：合并要在验证之后，清理要在钩子用完那棵树之后。
        if let Some(action) = then {
            run_worktree_action(&b, &id, action).await;
        }
    });
}

// ─── 任务隔离工作树（Phase 2） ─────────────────────────────────────

/// 任务是否隔离执行：任务自身声明优先，否则按 `task_type` 的默认。
fn task_is_isolated(cwd: &Path, t: &Task) -> bool {
    if let Some(v) = t.isolated {
        return v;
    }
    crate::task_types::TaskTypeRegistry::load(cwd).isolated_default(t.task_type.as_deref())
}

/// 派发前准备隔离工作树，返回 agent 该用的工作目录。
///
/// `Ok(None)` = 不隔离（类型不需要 / 不是 git 仓库），照旧在项目 cwd 里跑。
/// 建树失败**不阻断派发**：降级成非隔离并在任务 history 记一条——否则一台
/// 没装 git 的机器上整个看板都不能派发了。
async fn prepare_isolation(b: &UiBackend, id: &str) -> Option<PathBuf> {
    use latte_agent_core::task_worktree::TaskWorktree;

    let (want, existing) = {
        let store = b.tasks.read();
        let t = store.get(id)?;
        (task_is_isolated(&b.cwd, t), t.worktree.clone())
    };
    if !want {
        return None;
    }
    // 已有工作树且目录还在 → 直接复用（rework / 续跑同一个任务）。
    if let Some(info) = existing {
        if info.root.is_dir() {
            return Some(info.root);
        }
    }
    let cwd = b.cwd.clone();
    let task_id = id.to_string();
    // git 是秒级阻塞操作，别在 async 执行器线程上跑。
    let created = tokio::task::spawn_blocking(move || {
        let repo_root = TaskWorktree::resolve_repo_root(&cwd)?;
        TaskWorktree::open_or_create(&repo_root, &task_id, None).map(|wt| wt.info().clone())
    })
    .await;
    let info = match created {
        Ok(Ok(info)) => info,
        Ok(Err(e)) => {
            // 非 git 仓库是**正常场景**（用 latte 管非 git 项目），不该刷屏
            // 报错，只在 history 记一次降级原因。
            let mut store = b.tasks.write();
            if let Some(t) = store.get_mut(id) {
                t.push_note(
                    "scheduler",
                    format!("未启用隔离工作树（{e}），本次在项目工作区内执行"),
                    now_ms(),
                );
            }
            let _ = store.persist(id);
            return None;
        }
        Err(e) => {
            eprintln!("[tasks] worktree 任务 panic: {e}");
            return None;
        }
    };
    let root = info.root.clone();
    let branch = info.branch.clone();
    {
        let mut store = b.tasks.write();
        if let Some(t) = store.get_mut(id) {
            t.worktree = Some(info);
            t.push_note(
                "scheduler",
                format!("隔离工作树：{}（分支 {branch}）", root.display()),
                now_ms(),
            );
        }
        let _ = store.persist(id);
    }
    Some(root)
}

/// 把任务的工作树合并回 base 分支。
///
/// `Ok(Some(sha))` = 合并成功（或无改动的空操作）；`Ok(None)` = 该任务没有
/// 工作树（非隔离）；`Err(msg)` = 冲突或 git 失败，**工作树与分支都保留**
/// 给人工处置。
async fn merge_task_worktree(b: &UiBackend, id: &str) -> Result<Option<String>, String> {
    use latte_agent_core::task_worktree::TaskWorktree;
    use latte_agent_core::workspace::MergeMode;

    let Some(info) = b.tasks.read().get(id).and_then(|t| t.worktree.clone()) else {
        return Ok(None);
    };
    let cwd = b.cwd.clone();
    let task_id = id.to_string();
    let res = tokio::task::spawn_blocking(move || {
        let repo_root =
            TaskWorktree::resolve_repo_root(&cwd).map_err(|e| format!("解析仓库根失败：{e}"))?;
        let wt = TaskWorktree::from_info(&repo_root, &task_id, info);
        wt.merge_back(MergeMode::NoFf).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("合并任务 panic：{e}"))?;
    res.map(Some)
}

/// 清理任务的工作树。`keep_branch` = 冲突后保留分支（唯一还能拿到那些提交
/// 的地方）。清理后清空 `Task.worktree`，避免下次派发复用一棵已删的树。
async fn cleanup_task_worktree(b: &UiBackend, id: &str, keep_branch: bool) {
    let Some(info) = b.tasks.read().get(id).and_then(|t| t.worktree.clone()) else {
        return;
    };
    match cleanup_worktree_info(&b.cwd, id, info, keep_branch).await {
        Ok(()) => {
            let mut store = b.tasks.write();
            if let Some(t) = store.get_mut(id) {
                t.worktree = None;
            }
            let _ = store.persist(id);
        }
        Err(e) => eprintln!("[tasks] cleanup worktree {id}: {e}"),
    }
}

/// 按**显式给定的** info 清理（不读 store）。删除任务时任务记录已经没了，
/// 只能靠删除前捕获的信息回收，否则工作树与分支永久残留。
async fn cleanup_worktree_info(
    cwd: &Path,
    task_id: &str,
    info: latte_agent_core::task_worktree::TaskWorktreeInfo,
    keep_branch: bool,
) -> Result<(), String> {
    use latte_agent_core::task_worktree::TaskWorktree;
    let cwd = cwd.to_path_buf();
    let task_id = task_id.to_string();
    tokio::task::spawn_blocking(move || {
        let repo_root = TaskWorktree::resolve_repo_root(&cwd).map_err(|e| e.to_string())?;
        TaskWorktree::from_info(&repo_root, &task_id, info)
            .cleanup(keep_branch)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("cleanup panic: {e}"))?
}

/// 登记「待派发」：闸挡下来的任务留在 todo，但标记用户已表达过要跑它，/// 由 scheduler 在闸放开后自动续跑。原因同时写进 history（看板可见），
/// 但**同一条原因不重复记**——scheduler 每 5s 重试一次，逐次记 note 会
/// 把 history 刷成几百条同文。
fn mark_dispatch_pending(b: &UiBackend, id: &str, reason: &str) {
    let mut store = b.tasks.write();
    let Some(t) = store.get_mut(id) else { return };
    let already = t.dispatch_pending
        && t.history
            .iter()
            .rev()
            .find_map(|h| h.note.clone())
            .is_some_and(|n| n.contains(reason));
    t.dispatch_pending = true;
    if !already {
        t.push_note("scheduler", format!("等待派发：{reason}"), now_ms());
    }
    if let Err(e) = store.persist(id) {
        eprintln!("[tasks] persist {id} after defer: {e}");
    }
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
            msg.push_str(&format!(
                "  {}. [{}] {}（{}）\n",
                i + 1,
                c.id,
                c.title,
                c.state
            ));
        }
    }
    msg.push_str("请组织团队完成该任务，完成后给出结果摘要。");
    msg
}

/// 任务族上下文：拆分子任务（refine）/派发时注入的**确定性**背景——
/// 祖先链（层级语境：编号体系、粒度参照）+ 兄弟任务边界（paths/
/// depends_on/state）。拆分时 planner 必须看到兄弟的 paths 才能避开
/// 重叠、看到祖先链才能沿用编号体系；这些此前都没给（实测：refine
/// 只带 title+description，「不要自造编号体系」的约束无信息可依）。
///
/// 故意**不带**兄弟任务的 description：注入面只要边界，内容会让
/// planner 被兄弟细节带偏（且控长）。被拆任务本体由调用方自行附上。
///
/// 纯函数数据装配，零 LLM——与 submit_plan_from/require_plan_tasks
/// 同向：确定性工作归引擎，不烧成模型调用。
fn build_family_context(store: &TaskStore, id: &str) -> String {
    let mut out = String::new();
    // 祖先链：沿 parent_id 从根爬到直接父（不含本体）。visited 兜底
    // 异常成环数据，与 check_no_cycle 同款。
    let mut chain: Vec<&Task> = Vec::new();
    {
        let mut visited = std::collections::HashSet::new();
        visited.insert(id.to_string());
        let mut cur = store.get(id).and_then(|t| t.parent_id.clone());
        while let Some(pid) = cur {
            if !visited.insert(pid.clone()) {
                break;
            }
            match store.get(&pid) {
                Some(t) => {
                    chain.push(t);
                    cur = t.parent_id.clone();
                }
                None => break,
            }
        }
        chain.reverse();
    }
    if !chain.is_empty() {
        out.push_str("【父任务链（根 → 直接父，含各层编号语境）】\n");
        for t in &chain {
            out.push_str(&format!("  [{}] {}\n", t.id, t.title));
        }
    }
    // 兄弟任务：同一 parent 下的其他子任务（含已完成的——paths 边界
    // 依然有效）。按 sub_order 排，与看板展示一致。
    let mut siblings: Vec<Task> = store
        .get(id)
        .and_then(|t| t.parent_id.as_ref())
        .map(|pid| {
            store
                .children_of(pid)
                .into_iter()
                .filter(|t| t.id != id)
                .collect()
        })
        .unwrap_or_default();
    siblings.sort_by_key(|t| t.sub_order);
    if !siblings.is_empty() {
        out.push_str(
            "【兄弟任务（paths 边界：拆出的子任务 paths 不得与它们重叠；依赖看 depends_on）】\n",
        );
        for t in &siblings {
            let paths = if t.paths.is_empty() {
                "未声明".to_string()
            } else {
                t.paths.join(", ")
            };
            let deps = if t.depends_on.is_empty() {
                String::new()
            } else {
                format!(" depends_on={}", t.depends_on.join(","))
            };
            out.push_str(&format!(
                "  [{}] {}（{}）paths={paths}{deps}\n",
                t.id, t.title, t.state
            ));
        }
    }
    out
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
    //    `deferred` 收 409 类拒绝：这类"现在不能跑、稍后能跑"的原因不
    //    直接 return，而是先出读锁、标记 dispatch_pending 再返回错误，
    //    让 scheduler 在闸放开后自动续跑（见 [`scan_and_dispatch`]）。
    let deferred: Option<String> = {
        let store = b.tasks.read();
        // 状态在 step 0 已校验过；这里再确认一次是防两把锁之间被并发改
        // 掉——此时不标 pending，让下面的 400 分支去报"状态不可派发"。
        let dispatchable = store
            .get(id)
            .map(|t| t.state == "todo" || t.state == "rework")
            .unwrap_or(false);
        let unsat = store.unsatisfied_deps(id);
        // 隔离任务**免文件冲突闸**：各写各的 worktree，物理上不可能互相
        // 覆盖，重叠与否都无所谓。依赖闸照旧——先后关系与隔离无关。
        let isolated = store.get(id).is_some_and(|t| task_is_isolated(&b.cwd, t));
        if !dispatchable {
            None
        } else if !unsat.is_empty() {
            let head = unsat
                .iter()
                .take(3)
                .map(|(d, s)| format!("{d}（{s}）"))
                .collect::<Vec<_>>()
                .join("、");
            let more = if unsat.len() > 3 {
                format!("（共 {} 个）", unsat.len())
            } else {
                String::new()
            };
            Some(format!("依赖未完成：{head}{more}，等它们 done 后自动派发"))
        } else if isolated {
            None
        } else if let Some((running_id, why)) = family_running_conflict(&store, id) {
            // 同族文件冲突兜底：同树任务默认并行，只有无法证明不碰同一批
            // 文件时才互斥（双方都声明 paths 且不重叠 → 放行）。
            Some(format!(
                "同族任务 {running_id} 正在执行中且{why}，等它完成后自动派发（或给两边都声明互不重叠的 paths）"
            ))
        } else if let Some((running_id, (mine, theirs))) = path_running_conflict(&store, id) {
            // 跨族文件范围互斥：本任务声明的 paths 与在跑任务重叠时拒绝，
            // 防止两个无亲缘任务并行改同一批文件互相覆盖。
            Some(format!(
                "任务 {running_id} 正在执行中且文件范围重叠（本任务 {mine:?} 与对方 {theirs:?}），等它完成后自动派发"
            ))
        } else {
            None
        }
    };
    if let Some(message) = deferred {
        mark_dispatch_pending(b, id, &message);
        return Err(ApiError {
            status: 409,
            message,
        });
    }
    let (
        title,
        priority,
        description,
        children,
        task_type,
        workflow,
        prev_state,
        recent_notes,
        family_ctx,
    ) = {
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
            build_family_context(&store, id),
        )
    };

    // 2. 隔离工作树（Phase 2）：隔离类任务先备好自己的 worktree，
    //    agent 的工具工作目录指向它，主仓工作区完全不受影响。
    //    `None` = 非隔离 / 非 git 仓库 → 照旧在项目 cwd 里跑。
    let agent_cwd = prepare_isolation(b, id).await;

    // 3. 建 session + label。任何一步失败都不改任务状态。
    let info = api::create_session_in(b, agent_cwd.as_deref()).await?;
    let label = format!("[{id}] {}", title.chars().take(20).collect::<String>());
    api::set_session_label(b, &info.session_id, &label)?;
    // 派发出去的 session 已有工作负载（label + 待跑的派发消息），立即
    // 落盘 meta：否则服务器在「首个可归档事件落盘前」重启，tmp 壳被
    // clean_tmp 清掉，runs[] 里的 session_id 变成 404——「查看对话」
    // 点开报错、又要重新点立即执行。materialize 是 best-effort（失败
    // 只 eprintln），不阻断派发。
    api::materialize_session(b, &info.session_id).await.ok();
    let base_msg = build_dispatch_message(id, &title, priority, &description, &children);
    // 任务族上下文（祖先链 + 兄弟 paths 边界）随本体一起下发：子任务
    // 执行时要知道自己在任务树里的位置，paths 声明的避让边界也在此。
    let base_msg = format!("{base_msg}{family_ctx}");

    // 4. workflow 绑定分支：`mode` 决定重做方式。
    //    - "redo"（默认）：**优先按 task_type 默认 workflow 推导**，显式 workflow
    //      仅作兜底（task_type 无默认时才用）。用户改类型=希望跑该类型的流程。
    //    - "rework"：强制走 rework 返工流（带审查反馈），适合小修小补的返工场景。
    let reg = crate::task_types::TaskTypeRegistry::load(&b.cwd);
    let effective = if mode == "redo" {
        // redo：优先 task_type 默认，显式 workflow 仅作兜底
        crate::task_types::effective_workflow_type_first(
            task_type.as_deref(),
            workflow.as_deref(),
            &reg,
        )
    } else {
        crate::task_types::effective_workflow(task_type.as_deref(), workflow.as_deref(), &reg)
    };
    let (wf_name, msg) = if mode == "rework" {
        // 强制返工流：即使 task_type=learn 也走 rework.toml，带审查反馈
        let feedback = if recent_notes.is_empty() {
            String::new()
        } else {
            format!(
                "\n\n【审查反馈与执行历史】\n{}",
                recent_notes.join("\n---\n")
            )
        };
        ("rework".to_string(), format!("{base_msg}{feedback}"))
    } else if let Some(bound) = effective {
        // 按类型重跑（redo）：走 effective workflow，rework 状态时附加反馈参考
        let feedback = if prev_state == "rework" && !recent_notes.is_empty() {
            format!(
                "\n\n【历史审查反馈（仅供参考，按新要求重做）】\n{}",
                recent_notes.join("\n---\n")
            )
        } else {
            String::new()
        };
        (bound.clone(), format!("{base_msg}{feedback}"))
    } else {
        // 无绑定 → 走普通 manager 会话
        let feedback = if prev_state == "rework" && !recent_notes.is_empty() {
            format!(
                "\n\n【历史审查反馈（仅供参考，按新要求重做）】\n{}",
                recent_notes.join("\n---\n")
            )
        } else {
            String::new()
        };
        let msg = format!("{base_msg}{feedback}");
        api::chat_send(b, Some(&info.session_id), &msg).await?;
        // 无绑定路径：直接跳到状态更新（wf_run = None）
        // 写锁内复查 state：step-0 的校验已释放→await 到这里之间，
        // 另一并发请求可能已把同一任务派成 in_progress 并 push 了 run。
        // 不复查则双派发：双 session、双 token、隔离任务同一 worktree 覆盖。
        let now = now_ms();
        let mut store = b.tasks.write();
        {
            let t = store
                .get_mut(id)
                .ok_or_else(|| ApiError::not_found(format!("task {id:?} 不存在")))?;
            if t.state != "todo" && t.state != "rework" {
                // 已被并发派发：拒绝；本请求建的 session 留作孤儿无害
                // （重启后 reconcile 也不会动它——只 dispatch 状态回退）。
                return Err(ApiError {
                    status: 409,
                    message: format!(
                        "state {:?} 不可派发（已并发被另一请求派发）",
                        t.state
                    ),
                });
            }
            t.set_state("in_progress", actor, None, now);
            t.scheduled_at = None;
            // 真派出去了 → 清「待派发」，否则 scheduler 会一直重试它。
            t.dispatch_pending = false;
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
    let wf = match load_workflow(&wf_name, &b.cwd) {
        Ok(w) => w,
        Err(e) => {
            // 已建并落盘的 session 没人引用了：回收，免得下拉里堆孤儿。
            let _ = api::delete_session(b, &info.session_id).await;
            return Err(ApiError::bad_request(format!(
                "workflow '{wf_name}' 加载失败：{e}"
            )));
        }
    };
    let event_tx = api::session_event_sender(b, &info.session_id).await?;
    // workflow 绑定分支不走 chat_send/submit_input，这里把派发消息
    // 显式记为 session 的用户诉求，否则 advisor 审查拿到空问题。
    api::session_record_user_input(b, &info.session_id, &msg).await?;
    let wf_run = Some((wf, event_tx, Arc::new(AtomicBool::new(false)), msg));

    // 写锁内复查 state：step-0 读锁校验已释放，await 期间另一并发请求
    // 可能已把同一任务派成 in_progress 并 push 了 run。不复查则双派发：
    // 双 session、双 token、隔离任务同一 worktree 覆盖。
    let now = now_ms();
    let (bad_state, gone) = {
        let store = b.tasks.read();
        match store.get(id) {
            Some(t) => ((t.state != "todo" && t.state != "rework").then(|| t.state.clone()), false),
            None => (None, true),
        }
    };
    if gone {
        let _ = api::delete_session(b, &info.session_id).await;
        return Err(ApiError::not_found(format!("task {id:?} 不存在")));
    }
    if let Some(state) = bad_state {
        let _ = api::delete_session(b, &info.session_id).await;
        return Err(ApiError {
            status: 409,
            message: format!("state {state:?} 不可派发（已并发被另一请求派发）"),
        });
    }
    // 上面读锁已释放；写锁内再确认一次（两把锁之间无 await，但保持原语义）。
    let mut store = b.tasks.write();
    if store
        .get(id)
        .is_none_or(|t| t.state != "todo" && t.state != "rework")
    {
        return Err(ApiError {
            status: 409,
            message: "state 不可派发（已并发被另一请求派发）".into(),
        });
    }
    {
        let t = store.get_mut(id).expect("刚校验过存在");
        t.set_state("in_progress", actor, None, now);
        t.scheduled_at = None;
        t.dispatch_pending = false;
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
        // 隔离任务的 workflow 也必须跑在它自己的工作树里，否则 speaker 的
        // 工具写回主仓——隔离只做了一半（会话隔离了、流水线没隔离）。
        let run_cwd = agent_cwd.clone().unwrap_or_else(|| b.cwd.clone());
        tokio::spawn(async move {
            // 复用该 session 的暂停门：用户 ⏸ 时这个看板 workflow 也一起停。
            let agent_pause_gate = api::session_pause_gate(&b2, &info.session_id).await.ok();
            let session_id = info.session_id.clone();
            // advisor intervene 暂停门：判「等待用户拍板」时流水线 park。
            let advisor_pause = api::session_advisor_pause_gate(&b2, &session_id).await.ok();
            // per-turn 取消旗标：step 无硬超时，用户点「终止当前任务」
            // 靠它掐掉卡住的 step（否则只能 session 级全杀）。
            let turn_cancel = api::session_turn_cancel_flag(&b2, &session_id).await.ok();
            // 配置与 advisor 闸门在**同一次读锁**里取齐（见 workflow_cfg_snapshot）。
            let (merged_cfg, advisor_gate) = b2.workflow_cfg_snapshot();
            let ctx = WorkflowRunContext {
                merged: merged_cfg,
                resolver: b2.resolver.clone(),
                default_params: GenerateParams::default(),
                cwd: run_cwd.clone(),
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
                advisor_gate,
                advisor_pause: advisor_pause.clone(),
                staging: None,
                // 顶层 run 自己建（见 run_workflow_inner），宿主传 None。
                dispatch_budget: None,
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
            // 判据必须落在**执行所在的工作树**：隔离任务的改动都在 worktree
            // 里，拿主仓去看 `git status` 永远是干净的 → 自动代码审查会被
            // 静默跳过（隔离任务从此再没有审查环节）。
            let changed = workspace_has_reviewable_changes(&run_cwd);
            let chain_review = result.is_ok() && wf.name != "code_review" && changed != Some(false);
            let dev_summary = result.as_ref().ok().cloned().unwrap_or_default();
            finish_workflow_run(&b2, &task_id, result, &cancel, &session_id);
            if chain_review {
                chain_code_review(
                    &b2,
                    &task_id,
                    msg2,
                    dev_summary,
                    event_tx,
                    agent_pause_gate.clone(),
                    session_id,
                    run_cwd.clone(),
                )
                .await;
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
///    受并发上限与依赖/文件范围约束——被 429/409 挡下的子任务留在 `todo`，
///    由 scheduler 后续补派。
///
/// 幂等：父任务已 `in_progress` 时只补派 `todo` 子任务，不重复迁移状态。
async fn dispatch_container(b: &UiBackend, id: &str, actor: &str) -> Result<TaskView, ApiError> {
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

    // 2. 逐个派发 todo 子任务。子任务派发失败（并发上限 / 依赖未满足 /
    //    范围冲突）不
    //    影响父任务容器状态——留在 todo，scheduler 后续补派。
    for cid in child_ids {
        match Box::pin(dispatch_task(b, &cid, actor, "redo")).await {
            Ok(_) => {}
            Err(e) => {
                eprintln!(
                    "[tasks] container {id}: dispatch child {cid} skipped ({}): {}",
                    e.status, e.message
                );
                // 也记到子任务的 history 上：容器派发的跳过原因（依赖未
                // 满足 / 范围冲突）此前只进 stderr，看板上表现为"子任务
                // 莫名没开跑"，用户无从判断是等依赖还是出错了。
                let mut store = b.tasks.write();
                if let Some(c) = store.get_mut(&cid) {
                    c.push_note("scheduler", format!("未派发：{}", e.message), now_ms());
                }
                if let Err(e) = store.persist(&cid) {
                    eprintln!("[tasks] persist {cid} after skip: {e}");
                }
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
/// 任意层级的任务都可再拆（任务树深度不限）；执行中（in_progress/
/// merging）的任务不可拆。
pub async fn refine_task(b: &UiBackend, id: &str) -> Result<RefineTaskResponse, ApiError> {
    // 读锁内校验 + 收集消息素材（含任务族上下文：祖先链 + 兄弟边界）。
    let (title, description, family_ctx) = {
        let store = b.tasks.read();
        let t = store
            .get(id)
            .ok_or_else(|| ApiError::not_found(format!("task {id:?} 不存在")))?;
        if t.state == "in_progress" || t.state == "merging" {
            return Err(ApiError::bad_request(format!(
                "state {:?} 不可拆分（执行中的任务请先中止）",
                t.state
            )));
        }
        let family_ctx = build_family_context(&store, id);
        (t.title.clone(), t.description.clone(), family_ctx)
    };

    // 2. workflow 必须在建 session 之前加载成功（失败不改任何状态）。
    let wf = load_workflow("task_refine", &b.cwd)
        .map_err(|e| ApiError::bad_request(format!("workflow 'task_refine' 加载失败：{e}")))?;

    // 3. 建 session + label + 记录用户诉求（advisor 审查要非空问题）。
    let info = api::create_session(b).await?;
    let label = format!("[{id}] 拆分 {}", title.chars().take(20).collect::<String>());
    api::set_session_label(b, &info.session_id, &label)?;
    let msg = format!(
        "[任务拆分 {id}] {title}\n描述：{description}\n{family_ctx}\n请把该任务拆分为可独立执行、可独立验收的子任务，并用 plan 工具一次性提交完整清单（导入后它们将作为 {id} 的子任务出现在任务看板）。"
    );
    let event_tx = api::session_event_sender(b, &info.session_id).await?;

    // 4. 登记 拆分会话 → 父任务 映射（server 侧，页面刷新不丢）+ 任务
    //    历史记一笔（不改状态），方便追溯拆分会话。
    b.refine_parents
        .write()
        .insert(info.session_id.clone(), id.to_string());
    {
        let mut store = b.tasks.write();
        if let Some(t) = store.get_mut(id) {
            // 拆分请求原文（含注入的父链+兄弟摘要）进 history：
            // ① 看板可见 planner 拿到的完整上下文，不再是黑盒；
            // ② 测试可断言族上下文确实注进了拆分请求。
            t.push_note(
                "user",
                format!("已发起拆分（session {}）", info.session_id),
                now_ms(),
            );
            t.push_note("refine", msg.clone(), now_ms());
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
        // 配置与 advisor 闸门在**同一次读锁**里取齐（见 workflow_cfg_snapshot）。
        let (merged_cfg, advisor_gate) = b2.workflow_cfg_snapshot();
        let ctx = WorkflowRunContext {
            merged: merged_cfg,
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
            advisor_gate,
            advisor_pause: advisor_pause.clone(),
            staging: None,
            // 顶层 run 自己建（见 run_workflow_inner），宿主传 None。
            dispatch_budget: None,
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
/// - `dispatch_task` 返回 409（依赖未满足 / 文件范围冲突）→ 跳过记原因，任务留
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
            resp.skipped.push((
                id,
                format!("并发上限：已有 {running} 个任务在跑（上限 {max}），等位"),
            ));
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
#[allow(clippy::too_many_arguments)]
async fn chain_code_review(
    b: &UiBackend,
    id: &str,
    dispatch_msg: String,
    dev_summary: String,
    event_tx: tokio::sync::broadcast::Sender<latte_agent_core::controller::ChatEvent>,
    agent_pause_gate: Option<Arc<latte_agent_core::pause_gate::AgentPauseGate>>,
    session_id: String,
    // run_cwd = 执行所在的工作树（隔离任务 = 它的 worktree 根）。审查要看的
    // diff 在那里；用主仓跑审查等于让 reviewer 对着一份没有改动的工作区空转。
    run_cwd: PathBuf,
) {
    let in_review = {
        let store = b.tasks.read();
        store
            .get(id)
            .map(|t| {
                t.state == "human_review"
                    // 只审**本次** run：中止后重派时旧 run 不得对新 run 发起审查。
                    && t.runs.last().is_some_and(|r| r.session_id == session_id)
            })
            .unwrap_or(false)
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
    // 配置与 advisor 闸门在**同一次读锁**里取齐（见 workflow_cfg_snapshot）。
    let (merged_cfg, advisor_gate) = b.workflow_cfg_snapshot();
    let ctx = WorkflowRunContext {
        merged: merged_cfg,
        resolver: b.resolver.clone(),
        default_params: GenerateParams::default(),
        cwd: run_cwd,
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
        advisor_gate,
        advisor_pause,
        staging: None,
        // 顶层 run 自己建（见 run_workflow_inner），宿主传 None。
        dispatch_budget: None,
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
        let auto_rework =
            matches!(&result, Ok(s) if verdict_is_reject(s)) && t.state == "human_review";
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

/// 状态变更后的家族联动：直接子任务全部 done → 父任务自动 done
/// （actor=workflow，附说明），然后继续向上冒泡——祖父的直接子任务
/// （含刚收尾的父）若也全部 done，祖父同样收尾，递归直到根。任一层
/// 有子任务 cancelled 或仍非 done → 停在该层，由人决定。
fn maybe_complete_parent(store: &mut TaskStore, child: &Task, now: i64) {
    let mut cur = child.parent_id.clone();
    let mut visited = std::collections::HashSet::new();
    while let Some(parent_id) = cur {
        if !visited.insert(parent_id.clone()) {
            break; // 数据异常成环兜底
        }
        let children = store.children_of(&parent_id);
        if children.is_empty() || !children.iter().all(|c| c.state == "done") {
            return;
        }
        let (changed, next) = {
            let Some(p) = store.get_mut(&parent_id) else {
                return;
            };
            let changed = p.state != "done";
            if changed {
                p.set_state("done", "workflow", Some("全部子任务已完成".into()), now);
            }
            (changed, p.parent_id.clone())
        };
        if changed {
            if let Err(e) = store.persist(&parent_id) {
                eprintln!("[tasks] persist parent {parent_id}: {e}");
            }
        }
        cur = next;
    }
}

/// `ancestor` 是否为 `id` 的祖先（沿 parent 链上爬，visited 防环兜底）。
fn is_ancestor_of(store: &TaskStore, ancestor: &str, id: &str) -> bool {
    let mut visited = std::collections::HashSet::new();
    let mut cur = store.get(id).and_then(|t| t.parent_id.clone());
    while let Some(pid) = cur {
        if pid == ancestor {
            return true;
        }
        if !visited.insert(pid.clone()) {
            break;
        }
        cur = store.get(&pid).and_then(|t| t.parent_id.clone());
    }
    false
}

/// 同家族**文件冲突**检查（v2）：同树拆出的任务默认**可以并行**，
/// 只有在无法证明它们不碰同一批文件时才互斥。家族 = 同一根任务
/// （[`TaskStore::root_of`]）下的整棵任务树（任意深度）。
/// 返回 `(在跑任务 id, 冲突原因)`，无冲突 → None。
///
/// 判据（对家族内每个在跑任务逐个判）：
/// - 双方都声明了 `paths` 且不重叠 → **放行**（各写各的文件）；
/// - 否则（任一方未声明、或声明了但重叠）→ 冲突。
///
/// 为什么保留兜底而不是彻底放开：未声明范围时无法证明两个任务不会写
/// 同一个文件，而共用一棵工作树下的并发写就是静默互相覆盖（v1 因此
/// 一律串行）。真正解决这件事要靠 per-task worktree（Phase 2），在那
/// 之前"声明了范围才允许并行"是可证明的最大放开幅度。
///
/// **顺序约束不再由这里承担**：先后关系一律写 `depends_on`
/// （见 [`TaskStore::unsatisfied_deps`]），本函数只管写冲突。
///
/// 例外：**容器祖先**（被派发任务的祖先节点，作为"子任务全完成即
/// 完成"的聚合节点）处于 in_progress 时，**不阻塞其后代**派发——
/// 否则子任务永远无法开跑（见 [`dispatch_task`] 的容器分流）。
/// 同树其他分支、以及后代在跑反过来阻塞祖先被当叶子派发，仍然有效。
fn family_running_conflict(store: &TaskStore, id: &str) -> Option<(String, String)> {
    let me = store.get(id)?;
    let root = store.root_of(id);
    std::iter::once(root.clone())
        .chain(store.descendants_of(&root))
        .filter(|oid| oid != id)
        .find_map(|oid| {
            let o = store.get(&oid)?;
            if o.state != "in_progress" {
                return None;
            }
            // 容器祖先在跑只是聚合标记，不阻塞后代派发。
            if is_ancestor_of(store, &oid, id) {
                return None;
            }
            match (me.paths.is_empty(), o.paths.is_empty()) {
                // 双方都声明了范围：只有真重叠才算冲突。
                (false, false) => first_path_overlap(&me.paths, &o.paths).map(|(mine, theirs)| {
                    (
                        o.id.clone(),
                        format!("文件范围重叠（本任务 {mine:?} 与对方 {theirs:?}）"),
                    )
                }),
                // 有一方没声明范围 → 无法证明不冲突，保守串行。
                _ => Some((
                    o.id.clone(),
                    "同族任务未声明 paths，无法证明不会写同一批文件".to_string(),
                )),
            }
        })
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
    let t = store
        .get_mut(id)
        .ok_or_else(|| format!("task {id:?} 不存在"))?;
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
    let next = if res == "completed" {
        "human_review"
    } else {
        "todo"
    };
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
    //    任务树任意深度：abort_task 自身递归处理在跑子任务的子树。
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
            // 旗标只在 step 边界生效；正在跑的 LLM step 靠 per-turn 取消掐掉，
            // 否则状态已回 todo、旧 step 仍在烧 token 并写同一棵工作树。
            let _ = api::chat_cancel_turn(b, Some(session_id)).await;
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
    // 用户主动中止 = 收回"要跑这个任务"的意思：清掉待派发，否则 scheduler
    // 5s 后又把它派出去，表现为"点了中止还在跑"。
    t.dispatch_pending = false;
    t.updated_at = now;
    // 隔离工作树**刻意保留**：中止后任务回 todo，重新派发会复用同一棵树，
    // 中止前干到一半的活还在。真要丢弃就取消任务（cleanup 留分支）。
    Ok(())
}

/// `POST /api/tasks/:id/resume` — 重试执行：让任务绑定的执行 session 从断点
/// 续跑。**任务状态不变**（仍是 `in_progress`，run 的 `ended_at` 保持 None）——
/// 重试是让同一个 run 的执行体重新跑起来，不是新开一个 run。
///
/// 适用场景：服务重启/网络/模型故障导致执行中断，session 文件还在盘上
/// （dispatch 时已 materialize）。workflow 任务从 checkpoint 跳过已完成
/// step 续跑；经典 chat 任务恢复其 controller/gate。底层走
/// [`api::chat_resume_session`]——它与「重启后点继续」是同一条路径，已处理
/// `last_failed_workflow` seed、checkpoint 校验、at-most-one guard。
///
/// 与 [`abort_task`] 正交：重试是「续上这次执行」，中止是「放弃这次执行回
/// todo 重派」。
pub async fn resume_task(b: &UiBackend, id: &str) -> Result<TaskView, ApiError> {
    // 1. 读锁校验 + 取绑定的 session。
    let session_id = {
        let store = b.tasks.read();
        let t = store
            .get(id)
            .ok_or_else(|| ApiError::not_found(format!("task {id:?} 不存在")))?;
        if t.state != "in_progress" {
            return Err(ApiError::bad_request(format!(
                "state {:?} 不可重试（仅 in_progress）",
                t.state
            )));
        }
        t.runs
            .last()
            .filter(|r| r.ended_at.is_none())
            .map(|r| r.session_id.clone())
            .ok_or_else(|| {
                ApiError::bad_request(
                    "没有进行中的 run 可重试（session 可能已被中止/删除，请改用中止回 todo）"
                        .to_string(),
                )
            })?
    };

    // 2. 让绑定的 session 从断点续跑。session 已被删 → 404，提示用户改走
    //    中止（语义：执行体没了，只能放弃这次 run）。
    api::chat_resume_session(b, Some(&session_id)).await.map_err(|e| {
        if e.status == 404 {
            ApiError::bad_request(format!(
                "绑定的执行 session {session_id} 已不存在，无法续跑——请「中止」回 todo 后重新派发"
            ))
        } else {
            e
        }
    })?;

    // 3. 记 history（状态不变）。actor=user：重试是人触发的恢复动作。
    let now = now_ms();
    let mut store = b.tasks.write();
    {
        let t = store
            .get_mut(id)
            .ok_or_else(|| ApiError::not_found(format!("task {id:?} 不存在")))?;
        t.push_note(
            "user",
            format!("重试执行：断点续跑 session {session_id}"),
            now,
        );
        t.updated_at = now;
    }
    store.persist(id).map_err(ApiError::internal)?;
    let t = store.get(id).expect("刚 persist 的任务必然存在");
    Ok(store.view(t))
}

/// `GET /api/tasks/:id/worktree-diff` — 隔离任务的改动预览。
///
/// 存在的理由：隔离任务的改动全在它自己的工作树里，用户在看板上点「批准」
/// （= 触发合并回 base）之前**看不到自己在批什么**——只能看 workflow 的文字
/// 摘要，而文字摘要恰恰最容易与实际改动不符。
///
/// 没有工作树（非隔离任务 / 已合并回收）→ `Ok(None)`，前端不渲染该区块。
pub async fn task_worktree_diff(
    b: &UiBackend,
    id: &str,
) -> Result<Option<latte_agent_core::task_worktree::WorktreeDiff>, ApiError> {
    use latte_agent_core::task_worktree::TaskWorktree;

    let Some(info) = b
        .tasks
        .read()
        .get(id)
        .ok_or_else(|| ApiError::not_found(format!("task {id:?} 不存在")))?
        .worktree
        .clone()
    else {
        return Ok(None);
    };
    let cwd = b.cwd.clone();
    let task_id = id.to_string();
    // git 是阻塞调用；diff 在大改动上能到几十毫秒，别占着 async 线程。
    tokio::task::spawn_blocking(move || {
        let repo_root = TaskWorktree::resolve_repo_root(&cwd)
            .map_err(|e| ApiError::internal(format!("解析仓库根失败：{e}")))?;
        TaskWorktree::from_info(&repo_root, &task_id, info)
            .diff_summary(WORKTREE_DIFF_MAX_FILES)
            .map(Some)
            .map_err(|e| ApiError::internal(format!("读取工作树改动失败：{e}")))
    })
    .await
    .map_err(|e| ApiError::internal(format!("diff 任务 panic：{e}")))?
}

/// 改动预览最多列多少个文件。总数照实回报（`file_count`），前端显示「还有 N 个」。
/// 上限存在的理由：一次大重构能改上千个文件，全塞进 HTTP 响应只会拖慢抽屉。
const WORKTREE_DIFF_MAX_FILES: usize = 40;

// ─── Workflow 执行流水（任务级） ─────────────────────────────────

/// 单条 step 的 checkpoint 记录（与 `latte_agent_core::workflow::CheckpointRecord`
/// 同形，但只取 UI 需要的字段；时间戳单位是**秒**）。
#[derive(Debug, Clone, Serialize)]
pub struct TaskWorkflowRunStep {
    pub step_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_key: Option<String>,
    /// step 产出全文（详情接口才填，列表接口为节省流量置空）。
    pub output: String,
    /// unix 秒。
    pub finished_at: u64,
}

/// 一条 workflow run 的列表视图（`GET /api/tasks/:id/workflow-runs` 返回数组元素）。
#[derive(Debug, Clone, Serialize)]
pub struct TaskWorkflowRunSummary {
    pub wf_id: String,
    pub workflow_name: String,
    /// 首行 meta 的 topic 截断预览（≤200 字符）。
    pub topic_preview: String,
    /// unix 秒。
    pub started_at: u64,
    /// run 所属的 UI session（merge 等 lifecycle hook 复用任务最后一个 run 的 session）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// 已完成的 step 数。
    pub step_count: usize,
    /// 最后一个完成的 step_id（运行中/失败时可用来定位卡点）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_step_id: Option<String>,
    /// 最后一次 step 完成时间（unix 秒）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_finished_at: Option<u64>,
}

/// 一条 workflow run 的完整详情（`GET /api/tasks/:id/workflow-runs/:wf_id`）。
#[derive(Debug, Clone, Serialize)]
pub struct TaskWorkflowRunDetail {
    #[serde(flatten)]
    pub summary: TaskWorkflowRunSummary,
    /// 完整 topic（不截断）。
    pub topic: String,
    /// 按完成顺序的全部 step（含 output 全文）。
    pub steps: Vec<TaskWorkflowRunStep>,
    /// run 里记录的用户回答（learn_loop 之类有交互问答的 workflow）。
    pub answers: Vec<TaskWorkflowRunAnswer>,
}

/// workflow run 中用户在 ask 弹框里给出的回答（`CheckpointRecord::Answer`）。
#[derive(Debug, Clone, Serialize)]
pub struct TaskWorkflowRunAnswer {
    pub role: String,
    pub question: String,
    pub answer: String,
    /// unix 秒。
    pub answered_at: u64,
}

/// 读 `<cwd>/.latte/workflow-runs/<wf_id>.jsonl`，解析成 (summary, detail_parts)。
/// 文件不存在/首行不是 meta → Err(None-ish)；中途坏行跳过。
fn read_workflow_run_jsonl(
    path: &Path,
) -> Result<
    (
        TaskWorkflowRunSummary,
        String,
        Vec<TaskWorkflowRunStep>,
        Vec<TaskWorkflowRunAnswer>,
    ),
    String,
> {
    let raw =
        std::fs::read_to_string(path).map_err(|e| format!("读 {} 失败：{e}", path.display()))?;
    let mut meta: Option<serde_json::Value> = None;
    let mut steps: Vec<TaskWorkflowRunStep> = Vec::new();
    let mut answers: Vec<TaskWorkflowRunAnswer> = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let ty = v.get("type").and_then(|s| s.as_str()).unwrap_or("");
        match ty {
            "meta" => meta = Some(v),
            "step" => {
                let step_id = v
                    .get("step_id")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let output_key = v
                    .get("output_key")
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string());
                let output = v
                    .get("output")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let finished_at = v.get("finished_at").and_then(|n| n.as_u64()).unwrap_or(0);
                steps.push(TaskWorkflowRunStep {
                    step_id,
                    output_key,
                    output,
                    finished_at,
                });
            }
            "answer" => {
                answers.push(TaskWorkflowRunAnswer {
                    role: v
                        .get("role")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string(),
                    question: v
                        .get("question")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string(),
                    answer: v
                        .get("answer")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string(),
                    answered_at: v.get("answered_at").and_then(|n| n.as_u64()).unwrap_or(0),
                });
            }
            _ => {}
        }
    }
    let meta = meta.ok_or_else(|| format!("{} 缺 meta 首行", path.display()))?;
    let wf_id = meta
        .get("wf_id")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let workflow_name = meta
        .get("workflow_name")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let topic = meta
        .get("topic")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let started_at = meta.get("started_at").and_then(|n| n.as_u64()).unwrap_or(0);
    let session_id = meta
        .get("session_id")
        .and_then(|s| s.as_str())
        .map(|s| s.to_string());
    let last_step = steps.last();
    let summary = TaskWorkflowRunSummary {
        wf_id,
        workflow_name,
        topic_preview: topic.chars().take(200).collect(),
        started_at,
        session_id,
        step_count: steps.len(),
        last_step_id: last_step.map(|s| s.step_id.clone()),
        last_finished_at: last_step.map(|s| s.finished_at),
    };
    Ok((summary, topic, steps, answers))
}

/// 列出某任务关联的所有 workflow run（按 started_at 升序）。
///
/// 关联口径：run 首行 `meta.session_id` ∈ `task.runs[].session_id`。
/// merge 等 lifecycle hook 刻意复用任务最后一个 run 的 session（见
/// `spawn_lifecycle_hook`），所以同一 session 可能挂多条 run——
/// `learn_loop`/`code_review`/`merge` 会同时出现在一个任务下。
pub fn task_workflow_runs(
    b: &UiBackend,
    id: &str,
) -> Result<Vec<TaskWorkflowRunSummary>, ApiError> {
    let session_ids: std::collections::HashSet<String> = {
        let store = b.tasks.read();
        let t = store
            .get(id)
            .ok_or_else(|| ApiError::not_found(format!("task {id:?} 不存在")))?;
        t.runs.iter().map(|r| r.session_id.clone()).collect()
    };
    if session_ids.is_empty() {
        return Ok(Vec::new());
    }
    let dir = b.cwd.join(".latte").join("workflow-runs");
    let mut out: Vec<TaskWorkflowRunSummary> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok((summary, _, _, _)) = read_workflow_run_jsonl(&path) else {
                continue;
            };
            let Some(sid) = &summary.session_id else {
                continue;
            };
            if session_ids.contains(sid) {
                out.push(summary);
            }
        }
    }
    out.sort_by_key(|s| s.started_at);
    Ok(out)
}

/// 取某任务下一条 workflow run 的完整详情（含全部 step output 全文）。
/// `wf_id` 必须在任务关联的 run 集合里（先 list 过滤，避免任意 id 读盘）。
pub fn task_workflow_run_detail(
    b: &UiBackend,
    id: &str,
    wf_id: &str,
) -> Result<TaskWorkflowRunDetail, ApiError> {
    // 先 list 过滤：wf_id 必须属于该任务（防止任意路径遍历）。
    let list = task_workflow_runs(b, id)?;
    if !list.iter().any(|s| s.wf_id == wf_id) {
        return Err(ApiError::not_found(format!(
            "task {id:?} 下没有 workflow run {wf_id:?}"
        )));
    }
    let path = b
        .cwd
        .join(".latte")
        .join("workflow-runs")
        .join(format!("{wf_id}.jsonl"));
    let (summary, topic, steps, answers) =
        read_workflow_run_jsonl(&path).map_err(ApiError::internal)?;
    Ok(TaskWorkflowRunDetail {
        summary,
        topic,
        steps,
        answers,
    })
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
pub async fn delete_task(b: &UiBackend, id: &str) -> Result<(), ApiError> {
    // 先掐掉在跑的执行体：否则任务记录没了，workflow 仍在后台烧 token，
    // 收尾时才报「task 不存在」。
    let active_session = {
        let store = b.tasks.read();
        store
            .get(id)
            .filter(|t| t.state == "in_progress")
            .and_then(|t| t.runs.last().filter(|r| r.ended_at.is_none()))
            .map(|r| r.session_id.clone())
    };
    if let Some(sid) = active_session {
        let flag = b.tasks.write().take_workflow_cancel(id);
        match flag {
            Some(f) => {
                f.store(true, Ordering::SeqCst);
                let _ = api::chat_cancel_turn(b, Some(&sid)).await;
            }
            None => {
                let _ = api::chat_abort(b, Some(&sid)).await;
            }
        }
    }
    let mut store = b.tasks.write();
    // 删除前把工作树信息取出来：任务记录一删就再也查不到它，工作树和分支
    // 会永久残留在 `$LATTE_HOME/worktrees/` 下。
    let wt = store.get(id).and_then(|t| t.worktree.clone());
    store.delete(id).map_err(map_store_err)?;
    store.mark_notion_dirty(id);
    drop(store);
    if let Some(info) = wt {
        // 分支保留：删任务不该顺手销毁里面还没合并的提交（分支是唯一入口）。
        let cwd = b.cwd.clone();
        let id = id.to_string();
        tokio::spawn(async move {
            if let Err(e) = cleanup_worktree_info(&cwd, &id, info, true).await {
                eprintln!("[tasks] delete {id}: 回收工作树失败 {e}");
            }
        });
    }
    Ok(())
}

// ─── Scheduler ────────────────────────────────────────────────────

/// 启动时回收**孤儿隔离工作树**：磁盘上有树，但看板里已经没有对应任务
/// （或那个任务已经不再持有工作树）。
///
/// 来源：进程在「建树」与「落盘 Task.worktree」之间被 kill、任务 JSON 被外部
/// 删掉、或 cleanup 失败留下的残骸。每棵树都是一份完整检出（几百 MB 级），
/// 不回收就会一直堆着。
///
/// **有未合并提交的树一律保留**（`reclaim_orphan` 内判定）：宁可占磁盘，
/// 也不能静默销毁没合并的工作成果。
pub async fn reclaim_orphan_worktrees(b: &UiBackend) {
    use latte_agent_core::task_worktree::TaskWorktree;

    let live: std::collections::HashSet<String> = b
        .tasks
        .read()
        .list()
        .into_iter()
        .filter(|t| t.worktree.is_some())
        .map(|t| t.id)
        .collect();
    let cwd = b.cwd.clone();
    let report = tokio::task::spawn_blocking(move || {
        let Ok(repo_root) = TaskWorktree::resolve_repo_root(&cwd) else {
            return Vec::new(); // 非 git 仓库：没有任务树可回收
        };
        let mut out: Vec<(String, Result<bool, String>)> = Vec::new();
        for task_id in TaskWorktree::list_task_ids(&repo_root) {
            if live.contains(&task_id) {
                continue;
            }
            let r = TaskWorktree::reclaim_orphan(&repo_root, &task_id).map_err(|e| e.to_string());
            out.push((task_id, r));
        }
        out
    })
    .await
    .unwrap_or_default();
    for (task_id, r) in report {
        match r {
            Ok(true) => eprintln!("[task-worktree] 回收孤儿工作树 {task_id}"),
            Ok(false) => eprintln!(
                "[task-worktree] 孤儿工作树 {task_id} 有未合并提交，保留（分支 latte/task/{task_id}）"
            ),
            Err(e) => eprintln!("[task-worktree] 回收 {task_id} 失败：{e}"),
        }
    }
}

/// 启动对账：标记**上次进程留下的未结束 run**（不迁状态、不标 aborted）。
///
/// 服务重启/崩溃时，正在跑的 workflow 与 controller 直接消亡，永远等不到
/// `WorkflowFinished`。但任务与它的执行 session 是绑定关系——session 文件在
/// 盘上还在（dispatch 时已 materialize），run 的 `ended_at` 保持 None 如实
/// 反映「绑定着一个已死、但可断点续跑的 session」。
///
/// 处理：**不回退状态、不自动重派**。只写一条 history 让「上次跑过」可见；
/// 任务留在 `in_progress`（绑定的 session 还指着它，只是执行体死了），由用户
/// 在看板点「↺ 重试」（`resume_task` → 该 session 断点续跑）或「中止」
/// （`abort_task` 收尾回 todo）。刻意不自动续跑/重派：重启就自动烧 token
/// 跑半成品，该由人看过 history 再决定。
///
/// **隔离工作树一律保留**：里面是上次干到一半的活，重试/重派会复用同一棵树。
pub fn reconcile_interrupted_runs(b: &UiBackend) -> Vec<String> {
    let now = now_ms();
    let stuck: Vec<String> = b
        .tasks
        .read()
        .list()
        .into_iter()
        .filter(|t| t.state == "in_progress")
        .filter(|t| t.runs.last().is_some_and(|r| r.ended_at.is_none()))
        .map(|t| t.id)
        .collect();
    if stuck.is_empty() {
        return stuck;
    }
    let mut store = b.tasks.write();
    for id in &stuck {
        let Some(t) = store.get_mut(id) else { continue };
        // 中断 ≠ 待派发：清掉否则 scheduler 会把「等用户重试」的任务又派出去。
        t.dispatch_pending = false;
        let kept = if t.worktree.is_some() {
            "，隔离工作树已保留"
        } else {
            ""
        };
        t.push_note(
            "scheduler",
            format!("上次执行随服务重启中断{kept}；点「重试」断点续跑，或「中止」回 todo 重新派发"),
            now,
        );
        t.updated_at = now;
        if let Err(e) = store.persist(id) {
            eprintln!("[tasks] reconcile {id}: {e}");
        }
    }
    eprintln!(
        "[tasks] 启动对账：{} 个中断任务已标记（待重试）（{stuck:?}）",
        stuck.len()
    );
    stuck
}

/// 看板 scheduler（§4）：启动时先立即扫一遍（补发关机期间错过的排
/// 期），之后每 5s 扫一次，到期任务逐个派发（history actor=scheduler）。
pub async fn scheduler_loop(b: std::sync::Arc<UiBackend>) {
    // 启动对账要在回收孤儿之前：它可能把任务回退 todo，但**保留**工作树，
    // 而回收判据看的是"任务还持有工作树吗"——顺序反了会把上次干到一半的
    // 树当孤儿删掉。
    reconcile_interrupted_runs(&b);
    // 启动即回收孤儿工作树（只做一次；正常路径的回收挂在状态迁移上）。
    reclaim_orphan_worktrees(&b).await;
    scan_and_dispatch(&b).await;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        scan_and_dispatch(&b).await;
    }
}

async fn scan_and_dispatch(b: &std::sync::Arc<UiBackend>) {
    let now = now_ms();
    // ① 到期排期任务。
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
            eprintln!(
                "[task-scheduler] dispatch {id} failed ({}): {}",
                e.status, e.message
            );
            // 409 = 依赖未满足 / 文件冲突：**保留排期**，下一 tick 再试。
            // dispatch_task 已标了 dispatch_pending 并记过一次原因；这里
            // 清排期反而会让下游任务永久失去自动派发（实测语义陷阱：
            // 「等依赖」被当成「派发失败」处理，排期被抹掉）。
            if e.status == 409 {
                continue;
            }
            // 其他失败防刷屏：清掉排期，否则每 5s 重试一次且每次新建 session。
            // 用户看到 history 备注后可重新排期或手动执行。
            let mut store = b.tasks.write();
            if let Some(t) = store.get_mut(&id) {
                t.scheduled_at = None;
                t.dispatch_pending = false;
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
    // ② 待派发任务（被依赖/冲突闸挡下过的）：闸放开就自动续跑。
    //    这是「同族并行 + 依赖排序」真正闭环的地方——上游 done 之后
    //    5s 内下游自动开跑，用户不用回看板再点一次「派发全部」。
    //    只挑依赖已满足的，避免对着还在等的任务反复走一遍派发流程。
    let pending: Vec<String> = {
        let store = b.tasks.read();
        let mut v: Vec<(i64, String)> = store
            .list()
            .into_iter()
            .filter(|t| t.dispatch_pending && (t.state == "todo" || t.state == "rework"))
            .filter(|t| store.unsatisfied_deps(&t.id).is_empty())
            .map(|t| (t.priority, t.id))
            .collect();
        v.sort();
        v.into_iter().map(|(_, id)| id).collect()
    };
    for id in pending {
        // 并发上限与手动批量派发同源，避免 scheduler 绕过限流。
        let running = b
            .tasks
            .read()
            .list()
            .iter()
            .filter(|t| t.state == "in_progress")
            .count();
        if running >= dispatch_max_concurrent_from_env() {
            break;
        }
        if let Err(e) = dispatch_task(b, &id, "scheduler", "redo").await {
            // 409 → 继续等（dispatch_task 已保持 pending 标记）。
            if e.status != 409 {
                eprintln!(
                    "[task-scheduler] pending dispatch {id} failed ({}): {}",
                    e.status, e.message
                );
                let mut store = b.tasks.write();
                if let Some(t) = store.get_mut(&id) {
                    t.dispatch_pending = false;
                    t.push_note(
                        "scheduler",
                        format!("自动续跑失败（{}），已取消待派发", e.message),
                        now_ms(),
                    );
                }
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

    /// board.json 坏/丢 → next_seq 回绕 → 新任务覆盖旧任务文件（静默丢）。
    /// load 对账必须强制 next_seq ≥ 已用最大 seq+1，且回写修正值。
    #[test]
    fn load_reconciles_next_seq_after_board_loss() {
        let (dir, mut store) = tmp_store();
        let t1 = make_task(&mut store, "旧任务"); // LAT-100
        assert_eq!(t1.id, "LAT-100");
        // 模拟 board.json 损坏/回绕：写一份 next_seq 倒退的版本。
        let meta_path = dir.path().join(".latte/tasks/board.json");
        let mut bad: BoardMeta = serde_json::from_str(
            &std::fs::read_to_string(&meta_path).unwrap(),
        )
        .unwrap();
        bad.next_seq = 100; // 回绕到起点
        std::fs::write(&meta_path, serde_json::to_string(&bad).unwrap()).unwrap();
        drop(store);

        // 重启加载：对账把 next_seq 抬到 max(seq)+1 = 101。
        let mut store2 = TaskStore::load(dir.path()).expect("reload");
        assert!(
            store2.meta().next_seq >= 101,
            "next_seq 应对账到 ≥101，实际 {}",
            store2.meta().next_seq
        );
        // 新任务拿到全新 id，不覆盖 LAT-100。
        let t2 = make_task(&mut store2, "新任务");
        assert_ne!(t2.id, "LAT-100", "新任务不得复用已用 id");
        assert!(store2.get("LAT-100").is_some(), "旧任务不能被覆盖丢");
        // 盘上的 board.json 也应被回写修正（下次启动不再回绕）。
        let fixed: BoardMeta = serde_json::from_str(
            &std::fs::read_to_string(&meta_path).unwrap(),
        )
        .unwrap();
        assert!(fixed.next_seq >= 101, "board.json 应回写修正后的 next_seq");
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
        for key in [
            "involved_files",
            "affected_paths",
            "affected_files",
            "file_paths",
            "files",
        ] {
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
                None, // agent_cwd：测试会话不隔离
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
        assert_eq!(
            store2.get("LAT-100").map(|t| t.title.as_str()),
            Some("实现 ringbuf")
        );
        assert_eq!(store2.meta().next_seq, 101);
    }

    /// P1-编排回归（实测实录）：纯探索流（explore/learn）
    /// 产出文档、`git diff` 为空，此前仍被无条件链式套上 4 步 code_review，
    /// 导致 programmer/reviewer/tester 围着一份文档空转，还把行号错位
    /// 当 blocking。判据：工作区没有值得审查的改动就不链式审查。
    #[test]
    fn clean_worktree_has_no_reviewable_changes() {
        assert!(
            !porcelain_has_reviewable_changes(""),
            "干净工作区 = 无待审查改动"
        );
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
        assert!(
            porcelain_has_reviewable_changes(" M src/arena.c\n"),
            "已跟踪修改"
        );
        assert!(
            porcelain_has_reviewable_changes("?? src/newfile.c\n"),
            "新增未跟踪源文件"
        );
        assert!(
            porcelain_has_reviewable_changes("A  include/x.h\n"),
            "已暂存新增"
        );
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

    /// CLI 导入闭环的写读往返：`import_tasks_standalone`（无 UiBackend
    /// 入口）写出的 board.json + 任务文件，TaskStore 重新 load 回来
    /// 必须逐项一致——id 从 LAT-100 起、初始状态 todo（含 backlog→
    /// todo 两条 history）、子任务树随根创建、next_seq 推进。
    #[test]
    fn import_tasks_standalone_round_trips_through_task_store() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let req: ImportTasksRequest = serde_json::from_value(serde_json::json!({
            "tasks": [
                { "title": "实现 ringbuf 读写", "priority": 2, "labels": ["core"],
                  "paths": ["src/"],
                  "subtasks": [{ "title": "补单测" }] },
                { "title": "写 README" }
            ]
        }))
        .expect("parse req");
        let created = import_tasks_standalone(cwd, &req).expect("import");
        assert_eq!(
            created,
            vec!["LAT-100", "LAT-101", "LAT-102"],
            "父先于子、深度优先"
        );

        // 重新 load（模拟另一个进程/重启后读同一份文件）。
        let store = TaskStore::load(cwd).expect("reload");
        assert_eq!(store.meta().next_seq, 103);
        let t0 = store.get("LAT-100").expect("LAT-100");
        assert_eq!(t0.state, "todo", "导入即进 todo");
        assert_eq!(t0.priority, 2);
        assert_eq!(t0.paths, vec!["src/".to_string()]);
        // history：创建（backlog）+ 导入落 todo 两条。
        assert_eq!(t0.history.len(), 2, "history: {:?}", t0.history);
        assert_eq!(t0.history[0].to, "backlog");
        assert_eq!(t0.history[1].from.as_deref(), Some("backlog"));
        assert_eq!(t0.history[1].to, "todo");
        // 子任务挂在根任务下。
        let t1 = store.get("LAT-101").expect("LAT-101");
        assert_eq!(t1.parent_id.as_deref(), Some("LAT-100"));
        assert_eq!(t1.title, "补单测");
        assert!(store.get("LAT-102").expect("LAT-102").parent_id.is_none());
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
        assert!(
            note.chars().count() < 400,
            "应截断: {} 字符",
            note.chars().count()
        );
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
            .create(
                "子",
                "",
                3,
                vec![],
                None,
                Some(parent.id.clone()),
                None,
                None,
                "user",
            )
            .expect("create child");
        assert_eq!(child.parent_id.as_deref(), Some(parent.id.as_str()));
        assert_eq!(child.sub_order, 0);
        // parent 必须存在。
        let err = store
            .create(
                "孤儿",
                "",
                3,
                vec![],
                None,
                Some("LAT-999".into()),
                None,
                None,
                "user",
            )
            .unwrap_err();
        assert!(err.contains("不存在"), "{err}");
        // 任意深度任务树：三层（父→子→孙）可以建。
        let grandchild = store
            .create(
                "孙",
                "",
                3,
                vec![],
                None,
                Some(child.id.clone()),
                None,
                None,
                "user",
            )
            .expect("第三层（孙）应可创建");
        assert_eq!(grandchild.parent_id.as_deref(), Some(child.id.as_str()));
        assert_eq!(store.root_of(&grandchild.id), parent.id);
        assert_eq!(
            store.descendants_of(&parent.id),
            vec![child.id.clone(), grandchild.id.clone()],
            "后代 BFS：子先于孙"
        );
        // sub_order 递增。
        let child2 = store
            .create(
                "子2",
                "",
                3,
                vec![],
                None,
                Some(parent.id.clone()),
                None,
                None,
                "user",
            )
            .expect("child2");
        assert_eq!(child2.sub_order, 1);
        // 后代聚合（递归统计整棵子树：子×2 + 孙×1）。
        let view = store.view(store.get(&parent.id).unwrap());
        assert_eq!(view.sub_total, 3);
        assert_eq!(view.sub_state_counts.get("backlog"), Some(&3));
        assert_eq!(view.sub_done, 0);
    }

    /// 环检测两路：validate_reparent 拒绝挂到自身/后代下；
    /// create 在既有数据异常成环时靠 visited 兜底不死循环。
    #[test]
    fn reparent_cycle_rejected_and_create_survives_corrupt_cycle() {
        let (_dir, mut store) = tmp_store();
        let a = make_task(&mut store, "A");
        let b = store
            .create(
                "B",
                "",
                3,
                vec![],
                None,
                Some(a.id.clone()),
                None,
                None,
                "user",
            )
            .expect("B");
        let c = store
            .create(
                "C",
                "",
                3,
                vec![],
                None,
                Some(b.id.clone()),
                None,
                None,
                "user",
            )
            .expect("C");
        // 挂到自身或后代之下 → 成环，拒绝。
        let err = store.validate_reparent(&a.id, &a.id).unwrap_err();
        assert!(err.contains("环"), "{err}");
        let err = store.validate_reparent(&a.id, &c.id).unwrap_err();
        assert!(err.contains("环"), "{err}");
        // 反向（后代挂到祖先下）合法；带子树移动也合法（B 有子任务 C）。
        assert!(store.validate_reparent(&c.id, &a.id).is_ok());
        assert!(store.validate_reparent(&b.id, &a.id).is_ok());
        // 新任务 id 不会出现在任何已有链上，create 天然无环。
        let d = store
            .create(
                "D",
                "",
                3,
                vec![],
                None,
                Some(c.id.clone()),
                None,
                None,
                "user",
            )
            .expect("深层创建");
        assert_eq!(store.root_of(&d.id), a.id);
        // 数据异常兜底：人为造成环（A↔B）后 create / root_of 不死循环。
        store.get_mut(&a.id).unwrap().parent_id = Some(b.id.clone());
        let e = store
            .create(
                "E",
                "",
                3,
                vec![],
                None,
                Some(a.id.clone()),
                None,
                None,
                "user",
            )
            .expect("环数据下 create 应靠 visited 兜底正常返回");
        assert_eq!(e.parent_id.as_deref(), Some(a.id.as_str()));
        let _ = store.root_of(&a.id);
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
        store
            .get_mut(&t.id)
            .unwrap()
            .set_state("todo", "user", None, now);
        assert_eq!(store.get(&t.id).unwrap().history.len(), len_before);
    }

    #[test]
    fn due_tasks_selects_only_due_todo() {
        let (_dir, mut store) = tmp_store();
        let now = now_ms();
        // 到期 todo
        let due = store
            .create(
                "到期",
                "",
                3,
                vec![],
                None,
                None,
                Some(now - 1000),
                None,
                "user",
            )
            .unwrap();
        store
            .get_mut(&due.id)
            .unwrap()
            .set_state("todo", "user", None, now);
        // 未到期 todo
        let future = store
            .create(
                "未到期",
                "",
                3,
                vec![],
                None,
                None,
                Some(now + 60_000),
                None,
                "user",
            )
            .unwrap();
        store
            .get_mut(&future.id)
            .unwrap()
            .set_state("todo", "user", None, now);
        // backlog 且已到期（不应选出：只派 todo）
        store
            .create(
                "backlog到期",
                "",
                3,
                vec![],
                None,
                None,
                Some(now - 1000),
                None,
                "user",
            )
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
        let file = dir
            .path()
            .join(".latte/tasks")
            .join(format!("{}.json", t.id));
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
    fn import_accepts_deep_nesting() {
        let (dir, mut store) = tmp_store();
        let req: ImportTasksRequest = serde_json::from_value(serde_json::json!({
            "tasks": [{
                "title": "父",
                "subtasks": [{ "title": "子", "subtasks": [{ "title": "孙" }] }]
            }]
        }))
        .expect("parse");
        let created =
            import_tasks_into(&mut store, dir.path(), &req).expect("任意深度嵌套应导入成功");
        assert_eq!(
            created,
            vec!["LAT-100", "LAT-101", "LAT-102"],
            "父先于子，深度优先"
        );
        let child = store.get("LAT-101").expect("child");
        let grandchild = store.get("LAT-102").expect("grandchild");
        assert_eq!(child.parent_id.as_deref(), Some("LAT-100"));
        assert_eq!(grandchild.parent_id.as_deref(), Some("LAT-101"));
        assert_eq!(store.root_of("LAT-102"), "LAT-100");
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
        let created =
            import_tasks_into(&mut store, dir.path(), &req).expect("空 workflow 不应报错");
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
        apply_workflow_finish(
            &mut store,
            &t.id,
            Err("boom".into()),
            false,
            now_ms(),
            "ui-test",
        )
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
        apply_workflow_finish(
            &mut store,
            &t.id,
            Err("cancelled".into()),
            true,
            now_ms(),
            "ui-test",
        )
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
            .create(
                title,
                "desc",
                2,
                vec![],
                None,
                Some(parent.id.clone()),
                None,
                None,
                "user",
            )
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
        assert_eq!(
            store.get(&p.id).unwrap().state,
            "backlog",
            "有 cancelled 子任务不得自动完成"
        );
    }

    /// v2 语义：**未声明 paths** 时同族仍然串行（无法证明不冲突）。
    #[test]
    fn family_conflict_detects_running_sibling_and_parent() {
        let (_dir, mut store) = tmp_store();
        let p = make_task(&mut store, "父任务");
        let c1 = make_child(&mut store, &p, "子1");
        let c2 = make_child(&mut store, &p, "子2");
        let conflict_id =
            |s: &TaskStore, id: &str| family_running_conflict(s, id).map(|(cid, _)| cid);
        // 兄弟 in_progress + 双方都没声明 paths → c2 冲突
        store
            .get_mut(&c1.id)
            .unwrap()
            .set_state("in_progress", "user", None, now_ms());
        assert_eq!(conflict_id(&store, &c2.id).as_deref(), Some(c1.id.as_str()));
        // 兄弟空闲 → 无冲突
        store
            .get_mut(&c1.id)
            .unwrap()
            .set_state("todo", "user", None, now_ms());
        assert!(conflict_id(&store, &c2.id).is_none());
        // 容器父任务 in_progress **不再**阻塞它自己的子任务（否则子任务
        // 永远开不了跑）——父作为聚合容器在跑时，子任务照常派发。
        store
            .get_mut(&p.id)
            .unwrap()
            .set_state("in_progress", "user", None, now_ms());
        assert!(
            conflict_id(&store, &c2.id).is_none(),
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
        assert_eq!(conflict_id(&store, &p.id).as_deref(), Some(c1.id.as_str()));
    }

    /// v2 核心放开：同族兄弟**两边都声明了互不重叠的 paths** → 可并行。
    /// 这条是「同树下的任务能并行跑」的回归防线（v1 一律串行）。
    #[test]
    fn family_siblings_with_disjoint_paths_run_in_parallel() {
        let (_dir, mut store) = tmp_store();
        let p = make_task(&mut store, "父任务");
        let c1 = make_child(&mut store, &p, "子1");
        let c2 = make_child(&mut store, &p, "子2");
        store.get_mut(&c1.id).unwrap().paths = vec!["src/a".into()];
        store.get_mut(&c2.id).unwrap().paths = vec!["src/b".into()];
        store
            .get_mut(&c1.id)
            .unwrap()
            .set_state("in_progress", "user", None, now_ms());
        assert!(
            family_running_conflict(&store, &c2.id).is_none(),
            "同族但范围不重叠的兄弟应可并行"
        );
        // 范围重叠 → 仍然互斥，原因里带上重叠的那对路径。
        store.get_mut(&c2.id).unwrap().paths = vec!["src/a/inner.rs".into()];
        let (cid, why) = family_running_conflict(&store, &c2.id).expect("重叠应冲突");
        assert_eq!(cid, c1.id);
        assert!(why.contains("src/a"), "原因应点名重叠路径：{why}");
        // 只有一边声明 → 无法证明不冲突，保守串行。
        store.get_mut(&c2.id).unwrap().paths = vec![];
        assert!(
            family_running_conflict(&store, &c2.id).is_some(),
            "一方未声明 paths 时必须保守串行"
        );
    }

    /// 跨层（根与孙）冲突：家族 = 同一 root 的整棵任务树。
    #[test]
    fn family_conflict_spans_whole_tree() {
        let (_dir, mut store) = tmp_store();
        let root = make_task(&mut store, "根");
        let a = make_child(&mut store, &root, "A");
        let b = make_child(&mut store, &root, "B");
        let g = make_child(&mut store, &a, "孙G");
        let conflict_id =
            |s: &TaskStore, id: &str| family_running_conflict(s, id).map(|(cid, _)| cid);
        // 孙在跑 → 根不能当叶子重复派发（跨层冲突）。
        store
            .get_mut(&g.id)
            .unwrap()
            .set_state("in_progress", "user", None, now_ms());
        assert_eq!(
            conflict_id(&store, &root.id).as_deref(),
            Some(g.id.as_str()),
            "孙在跑应阻塞根被当叶子派发"
        );
        // 同树跨分支：叔伯 B 在跑 → 阻塞孙 G。
        store
            .get_mut(&g.id)
            .unwrap()
            .set_state("todo", "user", None, now_ms());
        store
            .get_mut(&b.id)
            .unwrap()
            .set_state("in_progress", "user", None, now_ms());
        assert_eq!(
            conflict_id(&store, &g.id).as_deref(),
            Some(b.id.as_str()),
            "同树跨分支在跑应算冲突"
        );
        // 但容器祖先（根/A）在跑只是聚合标记，不阻塞后代派发。
        store
            .get_mut(&b.id)
            .unwrap()
            .set_state("todo", "user", None, now_ms());
        store
            .get_mut(&root.id)
            .unwrap()
            .set_state("in_progress", "user", None, now_ms());
        store
            .get_mut(&a.id)
            .unwrap()
            .set_state("in_progress", "user", None, now_ms());
        assert!(
            conflict_id(&store, &g.id).is_none(),
            "容器祖先在跑不应阻塞后代"
        );
    }

    /// maybe_complete_parent 多层冒泡：叶全 done → 中层 done → 根 done。
    #[test]
    fn maybe_complete_parent_bubbles_up_to_root() {
        let (_dir, mut store) = tmp_store();
        let root = make_task(&mut store, "根");
        let mid = make_child(&mut store, &root, "中");
        let leaf1 = make_child(&mut store, &mid, "叶1");
        let leaf2 = make_child(&mut store, &mid, "叶2");
        let now = now_ms();
        for l in [&leaf1, &leaf2] {
            store
                .get_mut(&l.id)
                .unwrap()
                .set_state("done", "user", None, now);
            let snap = store.get(&l.id).unwrap().clone();
            maybe_complete_parent(&mut store, &snap, now);
        }
        assert_eq!(
            store.get(&mid.id).unwrap().state,
            "done",
            "叶全 done → 中层收尾"
        );
        assert_eq!(store.get(&root.id).unwrap().state, "done", "应冒泡到根");
    }

    /// 冒泡停在未完成的层：根的另一支未完 → 中层收尾、根不动。
    #[test]
    fn maybe_complete_parent_stops_at_unfinished_level() {
        let (_dir, mut store) = tmp_store();
        let root = make_task(&mut store, "根");
        let mid = make_child(&mut store, &root, "中");
        let _other = make_child(&mut store, &root, "另一支");
        let leaf = make_child(&mut store, &mid, "叶");
        let now = now_ms();
        store
            .get_mut(&leaf.id)
            .unwrap()
            .set_state("done", "user", None, now);
        let snap = store.get(&leaf.id).unwrap().clone();
        maybe_complete_parent(&mut store, &snap, now);
        assert_eq!(store.get(&mid.id).unwrap().state, "done");
        assert_eq!(
            store.get(&root.id).unwrap().state,
            "backlog",
            "另一支未完，根不得收尾"
        );
    }

    /// view 递归聚合：sub_* 统计整棵子树的全部后代。
    #[test]
    fn view_aggregates_all_descendants() {
        let (_dir, mut store) = tmp_store();
        let root = make_task(&mut store, "根");
        let a = make_child(&mut store, &root, "A");
        let g1 = make_child(&mut store, &a, "孙1");
        let _g2 = make_child(&mut store, &a, "孙2");
        let now = now_ms();
        store
            .get_mut(&g1.id)
            .unwrap()
            .set_state("done", "user", None, now);
        store
            .get_mut(&a.id)
            .unwrap()
            .set_state("todo", "user", None, now);
        let view = store.view(store.get(&root.id).unwrap());
        assert_eq!(view.sub_total, 3, "根的后代 = A + 孙×2");
        assert_eq!(view.sub_done, 1);
        assert_eq!(view.sub_state_counts.get("done"), Some(&1));
        assert_eq!(view.sub_state_counts.get("todo"), Some(&1));
        assert_eq!(view.sub_state_counts.get("backlog"), Some(&1));
        // 中间层只聚合自己的子树。
        let view_a = store.view(store.get(&a.id).unwrap());
        assert_eq!(view_a.sub_total, 2);
        assert_eq!(view_a.sub_done, 1);
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
        store
            .get_mut(&c1.id)
            .unwrap()
            .set_state("done", "user", None, now);
        store
            .get_mut(&c2.id)
            .unwrap()
            .set_state("cancelled", "user", None, now);
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
        let (cid, (mine, theirs)) = path_running_conflict(&store, &b.id).expect("重叠应冲突");
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
            store
                .get_mut(a)
                .unwrap()
                .set_state("in_progress", "user", None, now);
            store
                .get_mut(c)
                .unwrap()
                .set_state("todo", "user", None, now);
        }
        let err = dispatch_task(&b, c, "user", "redo")
            .await
            .expect_err("重叠应 409");
        assert_eq!(err.status, 409);
        assert!(
            err.message.contains(a),
            "消息应列出冲突任务 id：{}",
            err.message
        );
        assert!(
            err.message.contains("src/ringbuf"),
            "消息应列出重叠路径：{}",
            err.message
        );
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
            store
                .get_mut(id)
                .unwrap()
                .set_state("todo", "user", None, now);
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
            resp.dispatched
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
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
        assert!(
            resp.skipped[0].1.contains("并发上限"),
            "{}",
            resp.skipped[0].1
        );
        let store = b.tasks.read();
        assert_eq!(
            store.get(&ids[2]).unwrap().state,
            "todo",
            "被限流的任务留 todo 等位"
        );
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
            resp.dispatched
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            vec![ids[2].as_str()],
            "只有不冲突的 C 应被派出"
        );
        assert_eq!(resp.skipped.len(), 1);
        assert_eq!(resp.skipped[0].0, ids[1]);
        assert!(
            resp.skipped[0].1.contains(&ids[0]),
            "原因应指出冲突对象：{}",
            resp.skipped[0].1
        );
        assert_eq!(
            b.tasks.read().get(&ids[1]).unwrap().state,
            "todo",
            "冲突任务留 todo 等 A 完成"
        );
    }

    // ─── 依赖（depends_on） ─────────────────────────────────────

    /// 依赖满足判据：依赖进终态（done/cancelled）才放行。
    #[test]
    fn unsatisfied_deps_tracks_dependency_states() {
        let (_dir, mut store) = tmp_store();
        let up = make_task(&mut store, "先写测试");
        let down = make_task(&mut store, "再实现");
        store
            .set_depends_on(&down.id, vec![up.id.clone()])
            .expect("set deps");
        // 上游 backlog → 未满足
        assert_eq!(
            store.unsatisfied_deps(&down.id),
            vec![(up.id.clone(), "backlog".to_string())]
        );
        // 上游 human_review（活干完但没批准）→ 仍未满足：下游要的是已
        // 合回 base 的改动，判据统一用终态。
        store
            .get_mut(&up.id)
            .unwrap()
            .set_state("human_review", "user", None, now_ms());
        assert_eq!(store.unsatisfied_deps(&down.id).len(), 1);
        // done → 满足
        store
            .get_mut(&up.id)
            .unwrap()
            .set_state("done", "user", None, now_ms());
        assert!(store.unsatisfied_deps(&down.id).is_empty());
        // cancelled 也算满足：取消的上游不会再产出，否则下游永久锁死。
        store
            .get_mut(&up.id)
            .unwrap()
            .set_state("cancelled", "user", None, now_ms());
        assert!(store.unsatisfied_deps(&down.id).is_empty());
    }

    /// 依赖被删除 → 视作满足（不把下游永久锁死）。
    #[test]
    fn unsatisfied_deps_ignores_deleted_dependency() {
        let (_dir, mut store) = tmp_store();
        let up = make_task(&mut store, "上游");
        let down = make_task(&mut store, "下游");
        store
            .set_depends_on(&down.id, vec![up.id.clone()])
            .expect("set deps");
        store.delete(&up.id).expect("delete");
        assert!(store.unsatisfied_deps(&down.id).is_empty());
    }

    /// 非法依赖声明一律报错，不静默丢弃。
    #[test]
    fn set_depends_on_rejects_illegal_declarations() {
        let (_dir, mut store) = tmp_store();
        let a = make_task(&mut store, "A");
        let b = make_task(&mut store, "B");
        let child = make_child(&mut store, &a, "A 的子任务");
        // 自依赖
        assert!(store
            .set_depends_on(&a.id, vec![a.id.clone()])
            .unwrap_err()
            .contains("不能依赖自身"));
        // 未知 id（写错 id 必须报错，不能静默变成"无依赖"）
        assert!(store
            .set_depends_on(&a.id, vec!["LAT-999".into()])
            .unwrap_err()
            .contains("不存在"));
        // 父子/祖孙：聚合关系不是依赖关系（反向依赖 = 死锁）
        assert!(store
            .set_depends_on(&child.id, vec![a.id.clone()])
            .unwrap_err()
            .contains("父子"));
        assert!(store
            .set_depends_on(&a.id, vec![child.id.clone()])
            .unwrap_err()
            .contains("父子"));
        // 成环（直接 + 间接）
        store
            .set_depends_on(&b.id, vec![a.id.clone()])
            .expect("b 依赖 a");
        assert!(store
            .set_depends_on(&a.id, vec![b.id.clone()])
            .unwrap_err()
            .contains("环"));
        let c = make_task(&mut store, "C");
        store
            .set_depends_on(&c.id, vec![b.id.clone()])
            .expect("c 依赖 b");
        assert!(
            store
                .set_depends_on(&a.id, vec![c.id.clone()])
                .unwrap_err()
                .contains("环"),
            "a→c→b→a 的间接环也要拦住"
        );
        // 合法声明去重并落盘（用一个新任务，避免与上面建立的 b→a 撞环）
        let d = make_task(&mut store, "D");
        store
            .set_depends_on(&d.id, vec![" ".into(), b.id.clone(), b.id.clone()])
            .expect("合法");
        assert_eq!(store.get(&d.id).unwrap().depends_on, vec![b.id.clone()]);
        // 整体替换语义：空数组清空。
        store.set_depends_on(&d.id, vec![]).expect("清空");
        assert!(store.get(&d.id).unwrap().depends_on.is_empty());
    }

    /// 导入：依赖可写同批任务的**标题**，且支持前向引用（依赖后面才
    /// 出现的任务）。两趟解析的回归防线。
    #[tokio::test]
    async fn import_resolves_dep_refs_by_title_including_forward_refs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = test_backend(&dir);
        let ids = import_as_todo(
            &b,
            serde_json::json!({"tasks": [
                // 前向引用：第 1 个任务依赖第 2 个（此时后者还没建）
                { "title": "实现 ringbuf", "paths": ["src/ringbuf.c"], "depends_on": ["写 ringbuf 测试"] },
                { "title": "写 ringbuf 测试", "paths": ["tests/ringbuf"] }
            ]}),
        )
        .await;
        let store = b.tasks.read();
        assert_eq!(
            store.get(&ids[0]).unwrap().depends_on,
            vec![ids[1].clone()],
            "标题引用应解析成任务 id"
        );
        // 视图里带 blocked_by，前端据此置灰派发按钮。
        let view = store.view(store.get(&ids[0]).unwrap());
        assert_eq!(view.blocked_by, vec![ids[1].clone()]);
        assert!(store
            .view(store.get(&ids[1]).unwrap())
            .blocked_by
            .is_empty());
    }

    /// 解析不到的依赖引用 → 整单报错（绝不静默丢弃）。
    #[tokio::test]
    async fn import_rejects_unresolvable_dep_ref() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = test_backend(&dir);
        let req: ImportTasksRequest = serde_json::from_value(serde_json::json!({
            "tasks": [{ "title": "甲", "depends_on": ["不存在的任务"] }]
        }))
        .expect("parse req");
        let err = import_tasks(&b, req).await.expect_err("应整单拒绝");
        assert!(err.message.contains("depends_on"), "{}", err.message);
        assert!(err.message.contains("不存在的任务"), "{}", err.message);
    }

    /// 模型把依赖写成 `dependencies`（workflow 产出的既有字段名）时
    /// 同样收编——此前这个键被静默丢弃。
    #[test]
    fn import_task_accepts_dependencies_alias() {
        let it: ImportTask = serde_json::from_value(serde_json::json!({
            "title": "甲",
            "dependencies": ["LAT-100"]
        }))
        .expect("parse");
        assert_eq!(it.depends_on, vec!["LAT-100".to_string()]);
    }

    /// dispatch 层：依赖未满足 → 409 且原因点名依赖；依赖 done 后放行。
    #[tokio::test]
    async fn dispatch_blocks_until_dependency_done() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = test_backend(&dir);
        let ids = import_as_todo(
            &b,
            serde_json::json!({"tasks": [
                { "title": "写测试", "paths": ["tests/x"] },
                { "title": "实现", "paths": ["src/x.c"], "depends_on": ["写测试"] }
            ]}),
        )
        .await;
        let err = dispatch_task(&b, &ids[1], "user", "redo")
            .await
            .expect_err("依赖未完成应 409");
        assert_eq!(err.status, 409);
        assert!(err.message.contains(&ids[0]), "{}", err.message);
        assert!(err.message.contains("依赖未完成"), "{}", err.message);
        assert_eq!(b.tasks.read().get(&ids[1]).unwrap().state, "todo");
        // 依赖 done → 放行
        b.tasks
            .write()
            .get_mut(&ids[0])
            .unwrap()
            .set_state("done", "user", None, now_ms());
        dispatch_task(&b, &ids[1], "user", "redo")
            .await
            .expect("依赖满足应放行");
        assert_eq!(b.tasks.read().get(&ids[1]).unwrap().state, "in_progress");
    }

    /// 一键批量派发：同族兄弟范围不重叠 → **全部并行派出**（v1 只会
    /// 派出 1 个，其余 409 留 todo）；被依赖挡住的仍留 todo。
    #[tokio::test]
    async fn dispatch_ready_runs_disjoint_siblings_in_parallel() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = test_backend(&dir);
        let ids = import_as_todo(
            &b,
            serde_json::json!({"tasks": [{
                "title": "父任务",
                "subtasks": [
                    { "title": "子甲", "paths": ["src/a"] },
                    { "title": "子乙", "paths": ["src/b"] },
                    { "title": "子丙", "paths": ["src/c"], "depends_on": ["子甲"] }
                ]
            }]}),
        )
        .await;
        // ids[0]=父，1..3=子甲/子乙/子丙。父任务是容器 → dispatch_ready
        // 派发它时走容器分流，逐个派子任务（返回体只记父任务一条）。
        let resp = dispatch_ready(&b, "user", Some(5)).await;
        assert_eq!(
            resp.dispatched
                .iter()
                .map(|(i, _)| i.as_str())
                .collect::<Vec<_>>(),
            vec![ids[0].as_str()],
            "容器父任务派发一次即代表整批子任务"
        );
        let store = b.tasks.read();
        assert_eq!(
            store.get(&ids[1]).unwrap().state,
            "in_progress",
            "子甲应在跑"
        );
        assert_eq!(
            store.get(&ids[2]).unwrap().state,
            "in_progress",
            "子乙与子甲范围不重叠，应同时在跑（v1 会被同族互斥挡成 todo）"
        );
        assert_eq!(
            store.get(&ids[3]).unwrap().state,
            "todo",
            "依赖子甲的子丙必须等着"
        );
        // 跳过原因落在子任务 history 上（看板可见），并点名依赖。
        let note = store
            .get(&ids[3])
            .unwrap()
            .history
            .iter()
            .rev()
            .find_map(|h| h.note.clone())
            .expect("子丙应有未派发原因");
        assert!(note.contains(&ids[1]), "原因应点名依赖：{note}");
    }

    /// 闸挡下来的任务会被标记 `dispatch_pending`，依赖满足后
    /// `scan_and_dispatch` 自动续跑——这是「同族并行 + 依赖排序」闭环的
    /// 最后一环：用户点一次「派发全部」，上游 done 后下游 5s 内自动开跑，
    /// 不需要回看板再点第二次。
    #[tokio::test]
    async fn pending_task_auto_dispatches_once_dependency_is_done() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = std::sync::Arc::new(test_backend(&dir));
        let ids = import_as_todo(
            &b,
            serde_json::json!({"tasks": [
                { "title": "写测试", "paths": ["tests/x"] },
                { "title": "实现", "paths": ["src/x.c"], "depends_on": ["写测试"] }
            ]}),
        )
        .await;

        // 用户点「派发全部」：上游派出，下游被依赖挡下但登记 pending。
        let resp = dispatch_ready(&b, "user", Some(5)).await;
        assert_eq!(resp.dispatched.len(), 1);
        {
            let store = b.tasks.read();
            let down = store.get(&ids[1]).unwrap();
            assert_eq!(down.state, "todo");
            assert!(down.dispatch_pending, "被依赖挡下的任务应登记待派发");
            let note = down
                .history
                .iter()
                .rev()
                .find_map(|h| h.note.clone())
                .unwrap();
            assert!(note.contains("等待派发"), "{note}");
        }

        // 依赖还没 done → 续跑扫描不动它。
        scan_and_dispatch(&b).await;
        assert_eq!(b.tasks.read().get(&ids[1]).unwrap().state, "todo");

        // 上游 done → 下一次扫描自动派出下游，pending 标记清掉。
        b.tasks
            .write()
            .get_mut(&ids[0])
            .unwrap()
            .set_state("done", "user", None, now_ms());
        scan_and_dispatch(&b).await;
        let store = b.tasks.read();
        let down = store.get(&ids[1]).unwrap();
        assert_eq!(down.state, "in_progress", "依赖满足后应自动开跑");
        assert!(!down.dispatch_pending, "派出后必须清掉待派发标记");
    }

    /// 用户把待派发任务收回 backlog → 撤销 pending，scheduler 不再自动
    /// 把它派出去（否则用户刚收回就又被派走）。
    #[tokio::test]
    async fn moving_pending_task_out_of_todo_cancels_auto_dispatch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = std::sync::Arc::new(test_backend(&dir));
        let ids = import_as_todo(
            &b,
            serde_json::json!({"tasks": [
                { "title": "上游", "paths": ["a"] },
                { "title": "下游", "paths": ["b"], "depends_on": ["上游"] }
            ]}),
        )
        .await;
        let _ = dispatch_task(&b, &ids[1], "user", "redo").await;
        assert!(b.tasks.read().get(&ids[1]).unwrap().dispatch_pending);

        update_task(
            &b,
            &ids[1],
            serde_json::from_value(serde_json::json!({ "state": "backlog" })).unwrap(),
        )
        .expect("patch state");
        assert!(!b.tasks.read().get(&ids[1]).unwrap().dispatch_pending);

        b.tasks
            .write()
            .get_mut(&ids[0])
            .unwrap()
            .set_state("done", "user", None, now_ms());
        scan_and_dispatch(&b).await;
        assert_eq!(
            b.tasks.read().get(&ids[1]).unwrap().state,
            "backlog",
            "收回 backlog 的任务不该被自动派发"
        );
    }

    // ─── 隔离工作树（Phase 2） ──────────────────────────────────────

    /// 整个测试二进制**共享**的 `LATTE_HOME`（任务工作树落这儿，不污染
    /// `~/.latte`）。
    ///
    /// 为什么不每个测试一个：`LATTE_HOME` 是进程级环境变量，而测试在同一
    /// 进程里并行跑——各设各的会互相覆盖，断言"工作树在我的 home 下"就会
    /// 间歇性失败。共享一份 + 每个测试自己的仓库 tempdir（project slug 不同）
    /// 已经足够隔离。目录刻意 leak（不随某个测试结束被删）。
    fn test_latte_home() -> &'static Path {
        static HOME: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        HOME.get_or_init(|| {
            let p = tempfile::tempdir().expect("home").keep();
            std::env::set_var("LATTE_HOME", &p);
            p
        })
    }

    /// 在 tempdir 里建一个真 git 仓库当项目根（工作树落 [`test_latte_home`]）。
    fn git_backend(dir: &tempfile::TempDir) -> UiBackend {
        let _ = test_latte_home();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@local"]);
        run(&["config", "user.name", "T"]);
        std::fs::write(dir.path().join("seed.txt"), "seed\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "init"]);
        test_backend(dir)
    }

    /// 生效的隔离设置：任务显式声明 > 类型默认 > 无类型即不隔离。
    #[test]
    fn isolation_resolution_prefers_task_then_type() {
        let (dir, mut store) = tmp_store();
        let cwd = dir.path();
        let plain = make_task(&mut store, "无类型");
        assert!(!task_is_isolated(cwd, &plain), "没有类型的任务不开树");

        let mut feat = plain.clone();
        feat.task_type = Some("feature".into());
        assert!(task_is_isolated(cwd, &feat), "feature 默认隔离");

        let mut research = plain.clone();
        research.task_type = Some("research".into());
        assert!(!task_is_isolated(cwd, &research), "只读类型默认不隔离");

        // 任务显式声明覆盖类型默认（两个方向都要能覆盖）。
        feat.isolated = Some(false);
        assert!(!task_is_isolated(cwd, &feat));
        research.isolated = Some(true);
        assert!(task_is_isolated(cwd, &research));
    }

    /// 派发隔离任务：建出工作树、会话的 agent cwd 指向它、主仓工作区不受影响。
    #[tokio::test]
    async fn dispatching_isolated_task_runs_in_its_own_worktree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = git_backend(&dir);
        // 显式 isolated（不设 task_type，避免走 workflow 路径需要流程文件）。
        let ids = import_as_todo(
            &b,
            serde_json::json!({"tasks": [{ "title": "改 ringbuf" }]}),
        )
        .await;
        b.tasks.write().get_mut(&ids[0]).unwrap().isolated = Some(true);

        dispatch_task(&b, &ids[0], "user", "redo")
            .await
            .expect("派发");

        let (root, branch) = {
            let store = b.tasks.read();
            let t = store.get(&ids[0]).unwrap();
            assert_eq!(t.state, "in_progress");
            let wt = t.worktree.clone().expect("隔离任务必须记下工作树");
            (wt.root, wt.branch)
        };
        assert_eq!(branch, format!("latte/task/{}", ids[0]));
        assert!(root.is_dir(), "工作树目录应存在：{}", root.display());
        assert!(root.join("seed.txt").exists(), "工作树应是仓库的完整检出");
        assert!(!root.starts_with(dir.path()), "工作树必须在仓库外");
        assert!(
            root.starts_with(test_latte_home()),
            "工作树应落在 LATTE_HOME 下"
        );
        // `.latte` 软链回主仓：项目配置与运行时状态共用一份。
        assert!(root
            .join(".latte")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());

        // 会话的 agent cwd = 工作树；会话日志仍落在主仓。
        let sid = b
            .tasks
            .read()
            .get(&ids[0])
            .unwrap()
            .runs
            .last()
            .unwrap()
            .session_id
            .clone();
        let handle = b.sessions.read().get(&sid).cloned().expect("session");
        assert_eq!(handle.agent_cwd_for_test(), root, "agent 必须跑在工作树里");
    }

    /// 「立即执行」后、首个可归档事件落盘前服务器重启：派发出的
    /// session 必须能从 ui-sessions/ 恢复回来。此前 session 要等首个
    /// ChatEvent 才从 tmp/ promote，重启时 tmp 壳被 clean_tmp 清掉，
    /// runs[] 里的 session_id 变成 404——「查看对话」点开报错、要
    /// 重新点立即执行。`materialize_session` 在派发时就把 meta+label
    /// 写进 ui-sessions/，堵住这个窗口。
    #[tokio::test]
    async fn dispatched_session_survives_restart_before_first_event() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = git_backend(&dir);
        // 非隔离 + 无 task_type → 走 manager chat_send 路径，派发消息
        // 立即进事件流；但即便 workflow 路径这里也成立：materialize 在
        // session_event_sender 之前就落了盘。
        let ids = import_as_todo(
            &b,
            serde_json::json!({"tasks": [{ "title": "重启后还要能看会话" }]}),
        )
        .await;
        dispatch_task(&b, &ids[0], "user", "redo")
            .await
            .expect("派发");
        let sid = b
            .tasks
            .read()
            .get(&ids[0])
            .unwrap()
            .runs
            .last()
            .unwrap()
            .session_id
            .clone();

        // 派发路径上 session 文件已落盘到 ui-sessions/（不依赖事件归档）。
        let persisted = dir
            .path()
            .join(".latte/ui-sessions")
            .join(format!("{sid}.jsonl"));
        assert!(
            persisted.is_file(),
            "派发必须把 session meta 落盘，{} 不存在",
            persisted.display()
        );
        // meta 行里带着 [task-id] label（恢复后列表不显示成空白）。
        let raw = std::fs::read_to_string(&persisted).unwrap();
        let meta = raw.lines().next().expect("meta line");
        assert!(
            meta.contains(&ids[0]),
            "meta label 应含任务 id：{meta}"
        );

        // 模拟重启：丢弃旧 backend，UiBackend::new 重新扫盘恢复。
        drop(b);
        let b2 = test_backend(&dir);
        let restored = b2
            .sessions
            .read()
            .get(&sid)
            .cloned()
            .expect("重启后 runs[] 指向的 session 必须恢复得回来");
        assert!(restored.restored, "应从落盘恢复（restored 标记）");
        // 与生产启动一致：server 起来后对账中断的 run（保持 in_progress + 标记）。
        reconcile_interrupted_runs(&b2);
        let expected_label = format!("[{}] {}", ids[0], "重启后还要能看会话");
        assert_eq!(
            restored.label.lock().as_deref(),
            Some(expected_label.as_str()),
            "label 应随 meta 恢复",
        );
        // 任务本体也恢复且保持对账语义（run 中断 → 保持 in_progress，用户点重试/中止）。
        let t = b2.tasks.read().get(&ids[0]).unwrap().clone();
        assert_eq!(t.state, "in_progress", "中断的任务保持 in_progress");
        assert_eq!(t.runs.last().unwrap().session_id, sid, "runs 关联仍在");
        assert!(t.runs.last().unwrap().ended_at.is_none(), "run 未收尾，留给续跑");
    }
    /// 同一任务并发派发两次（双击/两 tab 同时点）：写锁内复查 state
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_dispatch_on_same_task_only_one_succeeds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = std::sync::Arc::new(git_backend(&dir));
        let ids = import_as_todo(
            &b,
            serde_json::json!({"tasks": [{ "title": "竞态派发" }]}),
        )
        .await;
        let id = ids[0].clone();

        // 两个 dispatch 并发触发。join 收集结果。
        let b1 = b.clone();
        let id1 = id.clone();
        let h1 = tokio::spawn(async move { dispatch_task(&b1, &id1, "user", "redo").await });
        let b2 = b.clone();
        let id2 = id.clone();
        let h2 = tokio::spawn(async move { dispatch_task(&b2, &id2, "user", "redo").await });
        let r1 = h1.await.unwrap();
        let r2 = h2.await.unwrap();

        let oks = [&r1, &r2].iter().filter(|r| r.is_ok()).count();
        let conflicts = [&r1, &r2]
            .iter()
            .filter(|r| matches!(r, Err(e) if e.status == 409))
            .count();
        assert_eq!(oks, 1, "只有第一个该派成功：{:?} / {:?}", r1, r2);
        assert_eq!(conflicts, 1, "另一个必须收到 409：{:?} / {:?}", r1, r2);

        // 任务状态 in_progress，runs 只有一条（无双 session）。
        let t = b.tasks.read().get(&id).unwrap().clone();
        assert_eq!(t.state, "in_progress");
        assert_eq!(t.runs.len(), 1, "并发双派只 push 一条 run");
    }

    /// 隔离任务**免文件冲突闸**：两个 paths 完全重叠的隔离任务可以同时跑
    /// （各写各的树，物理上不可能互相覆盖）。这是 Phase 2 的核心收益。
    #[tokio::test]
    async fn isolated_tasks_ignore_path_conflicts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = git_backend(&dir);
        let ids = import_as_todo(
            &b,
            serde_json::json!({"tasks": [
                { "title": "甲", "paths": ["src/same.c"] },
                { "title": "乙", "paths": ["src/same.c"] }
            ]}),
        )
        .await;
        {
            let mut store = b.tasks.write();
            for id in &ids {
                store.get_mut(id).unwrap().isolated = Some(true);
            }
        }
        dispatch_task(&b, &ids[0], "user", "redo")
            .await
            .expect("甲");
        // 非隔离时这里必然 409（paths 重叠）。
        dispatch_task(&b, &ids[1], "user", "redo")
            .await
            .expect("乙也应放行");
        let store = b.tasks.read();
        assert_eq!(store.get(&ids[0]).unwrap().state, "in_progress");
        assert_eq!(store.get(&ids[1]).unwrap().state, "in_progress");
        assert_ne!(
            store.get(&ids[0]).unwrap().worktree.as_ref().unwrap().root,
            store.get(&ids[1]).unwrap().worktree.as_ref().unwrap().root,
            "两个任务必须拿到不同的工作树"
        );
    }

    /// 状态迁移触发的收尾动作：`done` 才合并，`merging` 只跑验证钩子，
    /// `cancelled` 回收但留分支。
    ///
    /// 钉住的是一个设计取舍：合并**不能**挂在 `merging`——那样机械合并会排在
    /// LLM 验证流之后，模型不可用或流程等人拍板时合并被无限期拖住，而用户点
    /// 批准的意思就是"合进去"。
    #[test]
    fn merge_is_triggered_by_done_not_by_merging() {
        let src = std::fs::read_to_string("src/tasks.rs").expect("读自身源码");
        // 判据直接读代码里的动作表（这段逻辑在 update_task 的写锁作用域内，
        // 单测拿不到 spawn 的可观测点，只能钉住映射本身）。
        let table = src
            .split("if t.worktree.is_some() {")
            .nth(1)
            .expect("找到工作树动作表")
            .split("_ => {}")
            .next()
            .expect("动作表主体");
        assert!(
            table.contains(r#""done" => wt_action = Some(WorktreeAction::Merge)"#),
            "合并必须由 done 触发：{table}"
        );
        assert!(
            !table.contains(r#""merging" => wt_action = Some(WorktreeAction::Merge)"#),
            "合并绝不能挂在 merging（会被 LLM 验证流拖住）：{table}"
        );
        assert!(
            table.contains(r#""cancelled" => {"#) && table.contains("keep_branch: true"),
            "取消要回收但保留分支：{table}"
        );
    }

    /// 合并回 base：工作树里的改动进主仓，随后工作树被回收。
    #[tokio::test]
    async fn merge_then_cleanup_lands_changes_and_reclaims_the_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = git_backend(&dir);
        let ids = import_as_todo(&b, serde_json::json!({"tasks": [{ "title": "加个文件" }]})).await;
        b.tasks.write().get_mut(&ids[0]).unwrap().isolated = Some(true);
        dispatch_task(&b, &ids[0], "user", "redo")
            .await
            .expect("派发");
        let root = b
            .tasks
            .read()
            .get(&ids[0])
            .unwrap()
            .worktree
            .clone()
            .unwrap()
            .root;

        // 模拟 agent 在工作树里干活。
        std::fs::write(root.join("added.txt"), "work\n").unwrap();
        assert!(
            !dir.path().join("added.txt").exists(),
            "合并前主仓不该看到改动"
        );

        let sha = merge_task_worktree(&b, &ids[0])
            .await
            .expect("合并")
            .expect("有工作树");
        assert!(!sha.is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("added.txt")).unwrap(),
            "work\n",
            "合并后改动应落到主仓工作区"
        );

        cleanup_task_worktree(&b, &ids[0], false).await;
        assert!(!root.exists(), "回收后工作树目录应消失");
        assert!(
            b.tasks.read().get(&ids[0]).unwrap().worktree.is_none(),
            "回收后必须清空 Task.worktree，否则下次派发会复用一棵已删的树"
        );
        // 主仓的看板数据没被软链牵连删除。
        assert!(
            dir.path().join(".latte/tasks").is_dir(),
            "主仓 .latte 必须完好"
        );
    }

    /// 合并冲突：报错、任务停在原状态、工作树与分支都保留给人工处置。
    #[tokio::test]
    async fn merge_conflict_keeps_the_tree_for_manual_handling() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = git_backend(&dir);
        let ids = import_as_todo(&b, serde_json::json!({"tasks": [{ "title": "改 seed" }]})).await;
        b.tasks.write().get_mut(&ids[0]).unwrap().isolated = Some(true);
        dispatch_task(&b, &ids[0], "user", "redo")
            .await
            .expect("派发");
        let root = b
            .tasks
            .read()
            .get(&ids[0])
            .unwrap()
            .worktree
            .clone()
            .unwrap()
            .root;

        std::fs::write(root.join("seed.txt"), "task side\n").unwrap();
        std::fs::write(dir.path().join("seed.txt"), "main side\n").unwrap();
        let out = std::process::Command::new("git")
            .args(["commit", "-aqm", "main edit"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(out.status.success());

        let err = merge_task_worktree(&b, &ids[0]).await.expect_err("应冲突");
        assert!(err.contains("seed.txt"), "错误要点名冲突文件：{err}");
        assert!(root.exists(), "冲突时必须保留工作树");
        assert!(
            b.tasks.read().get(&ids[0]).unwrap().worktree.is_some(),
            "冲突时不清空 worktree 记录（用户还要按它找现场）"
        );
    }

    /// 非 git 目录：隔离降级为普通派发，并在 history 说明原因（而不是报错）。
    #[tokio::test]
    async fn isolation_degrades_gracefully_outside_git() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _ = test_latte_home();
        let b = test_backend(&dir); // 没有 git init
        let ids = import_as_todo(&b, serde_json::json!({"tasks": [{ "title": "杂事" }]})).await;
        b.tasks.write().get_mut(&ids[0]).unwrap().isolated = Some(true);

        dispatch_task(&b, &ids[0], "user", "redo")
            .await
            .expect("非 git 也要能派发");
        let store = b.tasks.read();
        let t = store.get(&ids[0]).unwrap();
        assert_eq!(t.state, "in_progress");
        assert!(t.worktree.is_none(), "非 git 仓库不该有工作树");
        let note = t.history.iter().rev().find_map(|h| h.note.clone()).unwrap();
        assert!(
            note.contains("未启用隔离工作树"),
            "降级原因要写进 history：{note}"
        );
    }

    /// 孤儿工作树回收：看板里没有对应任务的树被删，**有未合并提交的保留**。
    #[tokio::test]
    async fn orphan_worktrees_are_reclaimed_unless_they_hold_unmerged_work() {
        use latte_agent_core::task_worktree::TaskWorktree;
        let dir = tempfile::tempdir().expect("tempdir");
        let b = git_backend(&dir);
        let repo_root = TaskWorktree::resolve_repo_root(&b.cwd).expect("repo");

        // 三棵树：① 干净的孤儿；② 有未合并提交的孤儿；③ 活任务持有的。
        let clean = TaskWorktree::open_or_create(&repo_root, "ORPHAN-1", None).unwrap();
        let dirty = TaskWorktree::open_or_create(&repo_root, "ORPHAN-2", None).unwrap();
        std::fs::write(dirty.root().join("wip.txt"), "unmerged\n").unwrap();
        let ids = import_as_todo(&b, serde_json::json!({"tasks": [{ "title": "活任务" }]})).await;
        b.tasks.write().get_mut(&ids[0]).unwrap().isolated = Some(true);
        dispatch_task(&b, &ids[0], "user", "redo")
            .await
            .expect("派发");
        let live_root = b
            .tasks
            .read()
            .get(&ids[0])
            .unwrap()
            .worktree
            .clone()
            .unwrap()
            .root;

        reclaim_orphan_worktrees(&b).await;

        assert!(!clean.root().exists(), "干净的孤儿树应被回收");
        assert!(
            dirty.root().exists(),
            "有未合并提交的孤儿树必须保留——宁可占磁盘也不能静默销毁工作成果"
        );
        assert!(live_root.is_dir(), "活任务持有的树不能被回收");
    }

    /// 启动对账：上次进程留下的未结束 run **保持 in_progress**（不迁状态、
    /// 不标 aborted），只记 history 标记中断，工作树保留。任务与 session
    /// 绑定，由用户「重试」断点续跑或「中止」回 todo。
    #[tokio::test]
    async fn startup_marks_interrupted_runs_but_keeps_state_and_worktree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = git_backend(&dir);
        let ids = import_as_todo(
            &b,
            serde_json::json!({"tasks": [{ "title": "被打断的" }, { "title": "正常 todo" }]}),
        )
        .await;
        b.tasks.write().get_mut(&ids[0]).unwrap().isolated = Some(true);
        dispatch_task(&b, &ids[0], "user", "redo")
            .await
            .expect("派发");
        let root = b
            .tasks
            .read()
            .get(&ids[0])
            .unwrap()
            .worktree
            .clone()
            .unwrap()
            .root;
        std::fs::write(root.join("half-done.txt"), "wip\n").unwrap();

        // 模拟"上次进程死了"：run 还挂着未结束，状态仍是 in_progress。
        let reconciled = reconcile_interrupted_runs(&b);
        assert_eq!(
            reconciled,
            vec![ids[0].clone()],
            "只该标记有未结束 run 的任务"
        );

        let store = b.tasks.read();
        let t = store.get(&ids[0]).unwrap();
        assert_eq!(
            t.state, "in_progress",
            "中断任务保持 in_progress（绑定着可续跑的 session），由用户决定重试/中止"
        );
        // run 不被收尾：ended_at 仍 None、result 仍 None——续跑的是同一个 run。
        let run = t.runs.last().unwrap();
        assert!(run.ended_at.is_none(), "不标 aborted，run 留给续跑");
        assert!(run.result.is_none());
        assert!(
            !t.dispatch_pending,
            "不自动重派——重启就重跑会在用户不在场时烧 token"
        );
        assert!(t.worktree.is_some(), "工作树记录必须保留");
        assert!(
            root.join("half-done.txt").exists(),
            "上次干到一半的活不能丢"
        );
        let note = t.history.iter().rev().find_map(|h| h.note.clone()).unwrap();
        assert!(note.contains("中断"), "{note}");
        // 没有 run 的 todo 任务不受影响。
        assert_eq!(store.get(&ids[1]).unwrap().state, "todo");
        assert!(store
            .get(&ids[1])
            .unwrap()
            .history
            .iter()
            .all(|h| h.actor != "scheduler"));
    }

    /// 重试执行：`in_progress` + 绑定的死 session → 断点续跑（状态不变、
    /// run 不收尾）；非 in_progress / 无进行中 run → 400。
    #[tokio::test]
    async fn resume_task_retries_bound_session_without_touching_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = test_backend(&dir);
        let ids = import_as_todo(&b, serde_json::json!({"tasks": [{ "title": "断点续跑" }]})).await;
        // 派发出去建一个真实 session（非隔离 + 无 workflow → 走 chat_send
        // manager 路径），run 记上 session_id。
        dispatch_task(&b, &ids[0], "user", "redo")
            .await
            .expect("派发");

        // todo → 400（造第二个 todo 任务校验状态守卫）。
        let todo_id = import_as_todo(&b, serde_json::json!({"tasks": [{ "title": "别的" }]})).await;
        let err = resume_task(&b, &todo_id[0]).await.expect_err("todo 不可重试");
        assert_eq!(err.status, 400);

        // in_progress → 走 chat_resume_session 续跑（session 在盘上，无
        // workflow → 无 resumable_wf_id，controller_or_spawn 后正常返回）。
        resume_task(&b, &ids[0])
            .await
            .expect("in_progress 应能重试");
        let t = b.tasks.read().get(&ids[0]).unwrap().clone();
        assert_eq!(t.state, "in_progress", "重试不改任务状态");
        assert!(t.runs.last().unwrap().ended_at.is_none(), "run 不收尾");
        let note = t.history.iter().rev().find_map(|h| h.note.clone()).unwrap();
        assert!(note.contains("重试执行"), "{note}");
    }

    /// 绑定的 session 已被删 → resume_task 提示改走中止（不再 404 裸抛）。
    #[tokio::test]
    async fn resume_task_surfaces_deleted_session_as_abort_hint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = test_backend(&dir);
        let ids = import_as_todo(&b, serde_json::json!({"tasks": [{ "title": "死 session" }]})).await;
        dispatch_task(&b, &ids[0], "user", "redo")
            .await
            .expect("派发");
        let sid = b
            .tasks
            .read()
            .get(&ids[0])
            .unwrap()
            .runs
            .last()
            .unwrap()
            .session_id
            .clone();
        crate::api::delete_session(&b, &sid).await.expect("删 session");

        let err = resume_task(&b, &ids[0])
            .await
            .expect_err("session 没了不能续跑");
        assert!(
            err.message.contains("中止") || err.message.contains("重新派发"),
            "要提示改走中止：{}",
            err.message
        );
    }

    /// 对账保留的工作树**不能**被随后的孤儿回收当垃圾删掉（顺序回归）。
    #[tokio::test]
    async fn reconciled_worktree_survives_orphan_reclaim() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = git_backend(&dir);
        let ids = import_as_todo(&b, serde_json::json!({"tasks": [{ "title": "被打断的" }]})).await;
        b.tasks.write().get_mut(&ids[0]).unwrap().isolated = Some(true);
        dispatch_task(&b, &ids[0], "user", "redo")
            .await
            .expect("派发");
        let root = b
            .tasks
            .read()
            .get(&ids[0])
            .unwrap()
            .worktree
            .clone()
            .unwrap()
            .root;
        std::fs::write(root.join("wip.txt"), "wip\n").unwrap();

        reconcile_interrupted_runs(&b);
        reclaim_orphan_worktrees(&b).await;

        assert!(root.is_dir(), "任务仍持有的工作树不该被当孤儿回收");
        assert!(root.join("wip.txt").exists());
    }

    /// 改动预览：口径必须与合并一致（含未提交/未跟踪），合并后归零；
    /// 非隔离任务返回 None（前端不渲染该区块）。
    #[tokio::test]
    async fn worktree_diff_preview_matches_what_merge_would_take() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = git_backend(&dir);
        let ids = import_as_todo(
            &b,
            serde_json::json!({"tasks": [{ "title": "隔离的" }, { "title": "普通的" }]}),
        )
        .await;
        b.tasks.write().get_mut(&ids[0]).unwrap().isolated = Some(true);
        b.tasks.write().get_mut(&ids[1]).unwrap().isolated = Some(false);
        dispatch_task(&b, &ids[0], "user", "redo")
            .await
            .expect("派发隔离");
        dispatch_task(&b, &ids[1], "user", "redo")
            .await
            .expect("派发普通");

        // 非隔离任务没有工作树 → None。
        assert!(
            task_worktree_diff(&b, &ids[1])
                .await
                .expect("查普通任务")
                .is_none(),
            "非隔离任务不该有改动预览"
        );

        let root = b
            .tasks
            .read()
            .get(&ids[0])
            .unwrap()
            .worktree
            .clone()
            .unwrap()
            .root;
        // 未提交改动 + 未跟踪新文件：两者 merge_back 都会收下，预览必须都算。
        std::fs::write(root.join("seed.txt"), "seed\nmore\n").unwrap();
        std::fs::write(root.join("brand-new.txt"), "n\n").unwrap();

        let d = task_worktree_diff(&b, &ids[0])
            .await
            .expect("查隔离任务")
            .expect("有工作树");
        assert_eq!(d.branch, format!("latte/task/{}", ids[0]));
        assert_eq!(d.file_count, 2, "未提交 + 未跟踪都要算：{:?}", d.files);
        assert!(d.files.iter().any(|f| f.path == "brand-new.txt"));
        assert_eq!(d.total_added, 1);

        // 合并之后预览归零（与 has_changes 同口径，不能出现「合并完还说有改动」）。
        merge_task_worktree(&b, &ids[0]).await.expect("合并");
        let after = task_worktree_diff(&b, &ids[0])
            .await
            .expect("再查")
            .expect("树还在");
        assert_eq!(after.file_count, 0);
        assert!(after.commits.is_empty());
    }

    /// 从**隔离任务的会话** fork 出来的会话必须继承那棵工作树。
    ///
    /// 回归的是一种"看起来正常、其实全错"的状态：分叉会话继承了对话历史
    /// （历史里说"我改了 seed.txt"），但 agent 跑在主仓——那些改动在 worktree
    /// 里，分叉会话看不到，于是基于错误的文件状态继续干活。
    #[tokio::test]
    async fn forking_an_isolated_session_inherits_its_worktree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = git_backend(&dir);
        let ids = import_as_todo(&b, serde_json::json!({"tasks": [{ "title": "隔离的" }]})).await;
        b.tasks.write().get_mut(&ids[0]).unwrap().isolated = Some(true);
        dispatch_task(&b, &ids[0], "user", "redo")
            .await
            .expect("派发");
        let (sid, root) = {
            let store = b.tasks.read();
            let t = store.get(&ids[0]).unwrap();
            (
                t.runs.last().unwrap().session_id.clone(),
                t.worktree.clone().unwrap().root,
            )
        };

        let forked = api::fork_session(&b, &sid, vec![]).await.expect("fork");
        let handle = b
            .sessions
            .read()
            .get(&forked.session_id)
            .cloned()
            .expect("forked handle");
        assert_eq!(
            handle.agent_cwd_for_test(),
            root,
            "隔离会话的 fork 必须跑在同一棵工作树里"
        );

        // 普通会话的 fork 仍然是项目 cwd（没有工作树可继承）。
        let plain = api::create_session(&b).await.expect("plain session");
        let forked_plain = api::fork_session(&b, &plain.session_id, vec![])
            .await
            .expect("fork");
        let h2 = b
            .sessions
            .read()
            .get(&forked_plain.session_id)
            .cloned()
            .expect("handle");
        assert_eq!(h2.agent_cwd_for_test(), b.cwd);
    }

    /// dispatch_ready 优先级顺序：上限 1 时只有 priority 1 被派出，priority 3 等位。
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
        assert!(
            resp.skipped[0].1.contains("并发上限"),
            "{}",
            resp.skipped[0].1
        );
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

    /// 两 tab 同 origin 各自 import 同 plan_id（双击 / 跨 tab 同步导入）：
    /// 第二个应被 409 拒绝，落盘 marker 锁住同 plan_id 后续所有 import。
    /// 修前 import_tasks 无 plan_id 幂等保护，双 tab 各自走完流程会造
    /// 两份完全相同的任务。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn import_same_plan_id_concurrently_only_one_succeeds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = std::sync::Arc::new(test_backend(&dir));
        let make_req = || serde_json::from_value::<ImportTasksRequest>(serde_json::json!({
            "plan_id": "plan-manager-dup",
            "tasks": [{ "title": "重复导入候选" }, { "title": "再来一份" }]
        }))
        .expect("parse req");
        let h1 = {
            let b = b.clone();
            tokio::spawn(async move { import_tasks(&b, make_req()).await })
        };
        let h2 = {
            let b = b.clone();
            tokio::spawn(async move { import_tasks(&b, make_req()).await })
        };
        let r1 = h1.await.unwrap();
        let r2 = h2.await.unwrap();
        let oks = [&r1, &r2].iter().filter(|r| r.is_ok()).count();
        let conflicts = [&r1, &r2]
            .iter()
            .filter(|r| matches!(r, Err(e) if e.status == 409))
            .count();
        assert_eq!(oks, 1, "并发导入只该成功一个：{:?} / {:?}", r1, r2);
        assert_eq!(conflicts, 1, "另一个必须 409：{:?} / {:?}", r1, r2);
        // 只造出 2 条任务（不是 4）。
        let store = b.tasks.read();
        let n = store.list().iter().filter(|t| t.state == "todo").count();
        assert_eq!(n, 2, "重复防护后任务数应是一次导入的 2 条，不是 4 条");
        // marker 落盘存在。
        assert!(b.tasks.read().plan_already_imported("plan-manager-dup"));
    }

    /// 同 plan_id 串行二次导入：第二次必须 409，**不**重新造任务。
    #[tokio::test]
    async fn import_same_plan_id_sequentially_second_call_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = test_backend(&dir);
        let req: ImportTasksRequest = serde_json::from_value(serde_json::json!({
            "plan_id": "plan-manager-seq",
            "tasks": [{ "title": "甲" }]
        }))
        .expect("parse req");
        let r1 = import_tasks(&b, req.clone()).await.expect("first ok");
        assert_eq!(r1.created.len(), 1);
        let r2 = import_tasks(&b, req).await.expect_err("second need 409");
        assert_eq!(r2.status, 409);
        // 不带 plan_id 的导入不受 marker 影响。
        let req3: ImportTasksRequest = serde_json::from_value(serde_json::json!({
            "tasks": [{ "title": "无 plan 来源" }]
        }))
        .expect("parse req");
        let r3 = import_tasks(&b, req3).await.expect("no-plan ok");
        assert_eq!(r3.created.len(), 1);
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

    /// 拆分导入校验：父任务必须存在；任意深度任务树下 item 可再嵌套
    /// subtasks、父任务本身也可以是子任务。
    #[tokio::test]
    async fn import_with_parent_id_validates_parent_and_nesting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = test_backend(&dir);
        // 父任务不存在 → 400
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
        // item 再嵌套 subtasks：任意深度任务树下合法。
        let req: ImportTasksRequest = serde_json::from_value(serde_json::json!({
            "parent_id": parent.task.id,
            "tasks": [{ "title": "子甲", "subtasks": [{ "title": "孙任务" }] }]
        }))
        .expect("parse req");
        let resp = import_tasks(&b, req).await.expect("嵌套 subtasks 应可导入");
        assert_eq!(resp.created.len(), 2);
        let child_id = resp.created[0].clone();
        {
            let store = b.tasks.read();
            assert_eq!(
                store.get(&child_id).unwrap().parent_id.as_deref(),
                Some(parent.task.id.as_str())
            );
            assert_eq!(
                store.get(&resp.created[1]).unwrap().parent_id.as_deref(),
                Some(child_id.as_str())
            );
        }
        // 父任务本身也可以是子任务：拿刚导入的子任务当父继续导入。
        let req: ImportTasksRequest = serde_json::from_value(serde_json::json!({
            "parent_id": child_id,
            "tasks": [{ "title": "曾孙任务" }]
        }))
        .expect("parse req");
        let resp = import_tasks(&b, req).await.expect("子任务也可作为父任务");
        let store = b.tasks.read();
        assert_eq!(
            store.get(&resp.created[0]).unwrap().parent_id.as_deref(),
            Some(child_id.as_str())
        );
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
            t.history.iter().any(|h| h
                .note
                .as_deref()
                .is_some_and(|n| n.contains(&resp.session_id))),
            "历史应记录拆分会话"
        );
    }

    /// 拆分入口校验：任意层级任务都可再拆（子任务也能拆，任务树
    /// 深度不限）；执行中（in_progress/merging）的任务不可拆。
    #[tokio::test]
    async fn refine_task_allows_subtask_rejects_running() {
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
            serde_json::from_value(serde_json::json!({"title": "父"})).expect("parse"),
        )
        .expect("create");
        let child = create_task(
            &b,
            serde_json::from_value(serde_json::json!({"title": "子", "parent_id": parent.task.id}))
                .expect("parse"),
        )
        .expect("create");
        // 子任务也可以再拆（任意深度任务树）。
        let resp = refine_task(&b, &child.task.id)
            .await
            .expect("子任务应可再拆");
        assert!(!resp.session_id.is_empty());
        // 执行中的任务不可拆。
        b.tasks.write().get_mut(&parent.task.id).unwrap().set_state(
            "in_progress",
            "user",
            None,
            now_ms(),
        );
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
        assert_eq!(
            store.get(&created[1]).unwrap().paths,
            vec!["benches".to_string()]
        );
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
        apply_workflow_finish(
            &mut store,
            &t.id,
            Ok("late success".into()),
            false,
            now_ms(),
            "ui-test",
        )
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
        apply_workflow_finish(
            &mut store,
            &t.id,
            Err("cancelled".into()),
            true,
            now_ms(),
            "ui-test",
        )
        .expect("finish");
        let t = store.get(&t.id).unwrap().clone();
        assert_eq!(t.state, "in_progress", "新 run 必须继续跑");
        let run = t.runs.last().unwrap();
        assert_eq!(run.session_id, "ui-new");
        assert!(run.ended_at.is_none(), "新 run 不得被旧 run 收尾");
        // 本次 run 自己的收尾照常生效
        apply_workflow_finish(
            &mut store,
            &t.id,
            Ok("done".into()),
            false,
            now_ms(),
            "ui-new",
        )
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
        let note = p
            .history
            .last()
            .unwrap()
            .note
            .as_deref()
            .unwrap_or_default();
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
        let note = t
            .history
            .last()
            .unwrap()
            .note
            .as_deref()
            .unwrap_or_default();
        assert!(note.contains("无进行中的 run"), "{note}");
    }

    // ─── build_family_context：拆分/派发注入的祖先链 + 兄弟边界 ──────
    //
    // 契约：被拆任务的 planner 必须能看到父链（编号体系参照）和兄弟
    // 任务的 paths/depends_on（paths 避让边界），不含兄弟 description。

    #[test]
    fn family_context_root_task_empty() {
        let (_dir, mut store) = tmp_store();
        let t = make_task(&mut store, "根任务");
        assert!(
            build_family_context(&store, &t.id).is_empty(),
            "根任务无父无兄弟 → 无上下文"
        );
    }

    #[test]
    fn family_context_ancestor_chain_root_to_parent() {
        let (_dir, mut store) = tmp_store();
        let root = make_task(&mut store, "L1：学透仓库");
        let mid = store
            .create(
                "L2：内存管理",
                "desc",
                2,
                vec![],
                None,
                Some(root.id.clone()),
                None,
                None,
                "user",
            )
            .expect("create");
        let leaf = store
            .create(
                "L3：extent 层",
                "desc",
                2,
                vec![],
                None,
                Some(mid.id.clone()),
                None,
                None,
                "user",
            )
            .expect("create");
        let ctx = build_family_context(&store, &leaf.id);
        assert!(ctx.contains("父任务链"), "{ctx}");
        let i_root = ctx.find("L1：学透仓库").expect("根在链中");
        let i_mid = ctx.find("L2：内存管理").expect("中间层在链中");
        assert!(i_root < i_mid, "链序必须是 根 → 直接父");
        assert!(
            !ctx.contains(&leaf.title),
            "被拆任务本体不进父链（由调用方另附）"
        );
    }

    #[test]
    fn family_context_siblings_carry_paths_and_deps_not_description() {
        let (_dir, mut store) = tmp_store();
        let parent = make_task(&mut store, "L2：精读主线");
        let older = store
            .create(
                "L2.1：fastpath",
                "机密描述甲",
                2,
                vec![],
                None,
                Some(parent.id.clone()),
                None,
                None,
                "user",
            )
            .expect("create");
        let me = store
            .create(
                "L2.2：slowpath",
                "desc",
                2,
                vec![],
                None,
                Some(parent.id.clone()),
                None,
                None,
                "user",
            )
            .expect("create");
        let younger = store
            .create(
                "L2.3：底层原理",
                "desc",
                2,
                vec![],
                None,
                Some(parent.id.clone()),
                None,
                None,
                "user",
            )
            .expect("create");
        // 兄任务声明 paths + depends_on；弟任务留空。
        {
            let t = store.get_mut(&older.id).unwrap();
            t.paths = vec!["src/arena.c".into(), "src/extent.c".into()];
        }
        store
            .set_depends_on(&younger.id, vec![me.id.clone()])
            .expect("set deps");
        let ctx = build_family_context(&store, &me.id);
        assert!(ctx.contains("兄弟任务"), "{ctx}");
        assert!(ctx.contains("L2.1：fastpath"), "{ctx}");
        assert!(ctx.contains("src/arena.c"), "兄弟 paths 必须可见：{ctx}");
        assert!(ctx.contains(&format!("depends_on={}", me.id)), "{ctx}");
        assert!(ctx.contains("L2.3：底层原理"), "{ctx}");
        assert!(
            !ctx.contains("机密描述甲"),
            "兄弟 description 不注入（只给边界）"
        );
        assert!(!ctx.contains(&format!("[{}] L2.2", me.id)), "自身不算兄弟");
    }

    #[test]
    fn family_context_undeclared_paths_marked() {
        let (_dir, mut store) = tmp_store();
        let parent = make_task(&mut store, "父");
        store
            .create(
                "兄",
                "desc",
                2,
                vec![],
                None,
                Some(parent.id.clone()),
                None,
                None,
                "user",
            )
            .expect("create");
        let me = store
            .create(
                "我",
                "desc",
                2,
                vec![],
                None,
                Some(parent.id.clone()),
                None,
                None,
                "user",
            )
            .expect("create");
        let ctx = build_family_context(&store, &me.id);
        assert!(ctx.contains("paths=未声明"), "空 paths 要显式标注：{ctx}");
    }

    /// 无限深度拆分：任意层数祖先链都按 根→父 全量给出。
    #[test]
    fn family_context_unbounded_depth() {
        let (_dir, mut store) = tmp_store();
        let mut parent: Option<String> = None;
        let mut leaf = String::new();
        for depth in 1..=6 {
            let t = store
                .create(
                    &format!("L{depth}"),
                    "desc",
                    2,
                    vec![],
                    None,
                    parent.clone(),
                    None,
                    None,
                    "user",
                )
                .expect("create");
            leaf = t.id.clone();
            parent = Some(t.id);
        }
        let ctx = build_family_context(&store, &leaf);
        let pos: Vec<usize> = (1..=5)
            .map(|d| {
                ctx.find(&format!("] L{d}"))
                    .unwrap_or_else(|| panic!("L{d} 应在父链: {ctx}"))
            })
            .collect();
        assert!(
            pos.windows(2).all(|w| w[0] < w[1]),
            "深度不限的父链必须按 根→父 排序: {ctx}"
        );
    }
    /// 拆分请求注入族上下文：深拆（L2 再拆 L3）时 planner 必须看到
    /// 祖先链（L1 → L2 编号语境）与兄弟任务的 paths 边界。
    #[tokio::test]
    async fn refine_request_carries_family_context() {
        let dir = tempfile::tempdir().expect("tempdir");
        let wf_dir = dir.path().join(".latte").join("workflows.d");
        std::fs::create_dir_all(&wf_dir).expect("mkdir workflows.d");
        std::fs::write(
            wf_dir.join("task_refine.toml"),
            "name = \"task_refine\"\nmax_rounds = 1\n[[steps]]\nid = \"refine\"\nspeakers = [\"manager\"]\nprompt = \"拆分\"\n",
        )
        .expect("write workflow");
        let b = test_backend(&dir);
        // 三层树：L1 根 → L2（带兄弟 L2a）→ 被拆的 L3 候选父。
        let l1 = create_task(
            &b,
            serde_json::from_value(serde_json::json!({"title": "L1：学透仓库"})).unwrap(),
        )
        .expect("l1");
        let l2 = create_task(
            &b,
            serde_json::from_value(serde_json::json!({
            "title": "L2：内存管理", "parent_id": l1.task.id }))
            .unwrap(),
        )
        .expect("l2");
        let l2a = create_task(
            &b,
            serde_json::from_value(serde_json::json!({
            "title": "L2a：并发模型", "parent_id": l1.task.id, "paths": ["src/threads.c"] }))
            .unwrap(),
        )
        .expect("l2a");
        // 被拆任务 = L2 的子任务（深拆），兄弟 = L2a？不——兄弟同 parent。
        // L2 下建两个子任务：被拆的 + 一个带 paths 的兄弟。
        let target = create_task(
            &b,
            serde_json::from_value(serde_json::json!({
            "title": "L2.1：extent 层精读", "parent_id": l2.task.id }))
            .unwrap(),
        )
        .expect("target");
        let sib = create_task(
            &b,
            serde_json::from_value(serde_json::json!({
            "title": "L2.2：tcache 精读", "parent_id": l2.task.id, "paths": ["src/tcache.c"] }))
            .unwrap(),
        )
        .expect("sib");
        b.tasks.write().get_mut(&sib.task.id).unwrap().paths = vec!["src/tcache.c".into()];

        let _ = l2a; // L2a 是叔辈（parent 的兄弟），不应出现在本任务的兄弟段
        let resp = refine_task(&b, &target.task.id).await.expect("refine");
        assert!(!resp.session_id.is_empty());

        let store = b.tasks.read();
        let t = store.get(&target.task.id).unwrap();
        // history 里应有一条 refine 原文，含父链与兄弟摘要。
        let req = t
            .history
            .iter()
            .rev()
            .filter_map(|h| h.note.as_deref())
            .find(|n| n.contains("任务拆分"))
            .expect("refine 请求原文应入 history");
        assert!(req.contains("父任务链"), "缺父链：{req}");
        assert!(req.contains("L1：学透仓库"), "{req}");
        assert!(req.contains("L2：内存管理"), "{req}");
        assert!(req.contains("兄弟任务"), "缺兄弟段：{req}");
        assert!(req.contains("L2.2：tcache 精读"), "{req}");
        assert!(req.contains("src/tcache.c"), "兄弟 paths 要可见：{req}");
        assert!(
            !req.contains("L2a：并发模型"),
            "叔辈不是兄弟，不该进兄弟段：{req}"
        );
    }

    // ─── TaskWorkflowRun*：任务级 workflow run 流水 ────────────────────

    /// 按 session_id 把 `.latte/workflow-runs/*.jsonl` 反查回任务：
    /// merge / code_review / task_archive 等 lifecycle hook 复用任务最后
    /// 一个 run 的 session（见 spawn_lifecycle_hook），所以同一 session
    /// 可能挂多条 run——这里验证关联正确、且 step 顺序保留。
    #[test]
    fn task_workflow_runs_links_via_session_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = test_backend(&dir);
        let task = {
            let mut store = b.tasks.write();
            let t = store
                .create("T", "d", 2, vec![], None, None, None, None, "user")
                .expect("create");
            // 塞两条 run：主派发 session 是 sess-1（learn_loop/code_review
            // 都挂在它下面），第二次派发 sess-2。merge 会复用 sess-2。
            let mut t = t;
            t.runs.push(TaskRun {
                session_id: "sess-1".into(),
                started_at: 1000,
                ended_at: Some(2000),
                result: Some("completed".into()),
            });
            t.runs.push(TaskRun {
                session_id: "sess-2".into(),
                started_at: 3000,
                ended_at: None,
                result: None,
            });
            *store.get_mut(&t.id).unwrap() = t.clone();
            store.persist(&t.id).unwrap();
            t
        };

        // 造两条 workflow run：learn_loop 挂 sess-1，merge 挂 sess-2。
        // 再加一条无关 run（sess-other）验证不误关联。
        let runs_dir = dir.path().join(".latte").join("workflow-runs");
        std::fs::create_dir_all(&runs_dir).unwrap();
        std::fs::write(
            runs_dir.join("wf-learn_loop-1.jsonl"),
            concat!(
                r#"{"type":"meta","wf_id":"wf-learn_loop-1","workflow_name":"learn_loop","topic":"T desc","started_at":1500,"session_id":"sess-1"}"#,
                "\n",
                r#"{"type":"step","wf_id":"wf-learn_loop-1","workflow_name":"learn_loop","topic":"T desc","step_id":"plan","output_key":"plan_out","output":"plan output","finished_at":1600}"#,
                "\n",
                r#"{"type":"step","wf_id":"wf-learn_loop-1","workflow_name":"learn_loop","topic":"T desc","step_id":"tutor","output_key":null,"output":"tutor output","finished_at":1700}"#,
                "\n",
            ),
        )
        .unwrap();
        std::fs::write(
            runs_dir.join("wf-merge-1.jsonl"),
            concat!(
                r#"{"type":"meta","wf_id":"wf-merge-1","workflow_name":"merge","topic":"T merge","started_at":4000,"session_id":"sess-2"}"#,
                "\n",
                r#"{"type":"step","wf_id":"wf-merge-1","workflow_name":"merge","topic":"T merge","step_id":"final_check","output_key":"check","output":"check out","finished_at":4100}"#,
                "\n",
            ),
        )
        .unwrap();
        std::fs::write(
            runs_dir.join("wf-other-1.jsonl"),
            r#"{"type":"meta","wf_id":"wf-other-1","workflow_name":"merge","topic":"X","started_at":4500,"session_id":"sess-other"}"#,
        )
        .unwrap();

        let runs = task_workflow_runs(&b, &task.id).expect("list");
        assert_eq!(runs.len(), 2, "只应关联 sess-1/sess-2 的 run：{runs:?}");
        assert_eq!(runs[0].wf_id, "wf-learn_loop-1", "按 started_at 升序");
        assert_eq!(runs[1].wf_id, "wf-merge-1");
        assert_eq!(runs[0].step_count, 2);
        assert_eq!(runs[0].last_step_id.as_deref(), Some("tutor"));
        assert_eq!(runs[1].last_step_id.as_deref(), Some("final_check"));

        // detail：拿到完整 step output + 不属于本任务的 wf_id 应 404。
        let detail = task_workflow_run_detail(&b, &task.id, "wf-learn_loop-1").expect("detail");
        assert_eq!(detail.steps.len(), 2);
        assert_eq!(detail.steps[0].step_id, "plan");
        assert_eq!(detail.steps[0].output, "plan output");
        let err = task_workflow_run_detail(&b, &task.id, "wf-other-1").unwrap_err();
        assert_eq!(err.status, 404, "外任务 run 应 404：{err:?}");
    }

    /// 任务还没派发过 → `runs` 空 → 流水必然空（lifecycle hook 还没挂上来）。
    #[test]
    fn task_workflow_runs_empty_when_never_dispatched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let b = test_backend(&dir);
        let task = {
            let mut store = b.tasks.write();
            store
                .create("T", "d", 2, vec![], None, None, None, None, "user")
                .expect("create")
        };
        let runs = task_workflow_runs(&b, &task.id).expect("list");
        assert!(runs.is_empty(), "未派发任务不该有流水：{runs:?}");
    }
}
