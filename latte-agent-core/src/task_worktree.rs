//! 任务隔离工作树（Phase 2）：每个看板任务一份独立的 git worktree。
//!
//! # 为什么需要它
//!
//! 任务看板 v3 让同一棵任务树下的子任务**并行**执行（见
//! `latte-agent-ui-server/src/tasks.rs` 的 `family_running_conflict`）。并行的
//! 前提是它们不会互相覆盖文件，而 v3 判据只能靠模型自报的 `paths` 声明——
//! 声明不全就退回串行，声明错了就静默互相覆盖。
//!
//! 唯一能**证明**互不干扰的办法是让它们根本不写同一棵工作树：一任务一
//! worktree，跑完把改动合并回 base 分支。合并冲突是显式的、可处置的；
//! 并发写同一个文件是静默的、不可恢复的。
//!
//! # 与 [`crate::workspace`] 的关系
//!
//! `workspace::WorkspaceManager` 是 HIL blackboard 的工作树：树建在
//! `<repo>/.latte/worktrees/` 里、带 `plan.md` 黑板、有 checkpoint 状态机。
//! 本模块是**任务看板**用的精简版，两点关键差异：
//!
//! 1. **树建在仓库外**（`$LATTE_HOME/worktrees/<project>/<task>`）。建在仓库
//!    内会让主仓的 `search`/`grep`/代码图索引扫到 N 份完整代码副本——N 个并行
//!    任务就是 N 份，命中结果全是重复的。
//! 2. **`.latte` 软链回主仓**。会话日志、workflow 断点、任务看板、代码图索引
//!    都落在 `<cwd>/.latte/` 下，而 agent 的 cwd 就是这棵树。不软链的话这些
//!    状态会写进树里，然后被 `cleanup()` 的 `worktree remove --force` 一起
//!    删掉——归档之后 resume 直接断。
//!
//! # 并发
//!
//! git 用 `O_EXCL` 锁文件（`index.lock` / `packed-refs.lock`）且**不等待**：
//! 同一个仓库上并发 `worktree add` / `merge` 会直接失败而不是排队。各 worktree
//! 共享主仓元数据，所以跨 worktree 的写同样撞这把锁。因此本模块所有改仓库
//! 状态的操作都过 [`with_repo_lock`]（按主仓根序列化），同 oh-my-pi
//! `utils/repo-lock.ts` 的取舍。
//!
//! 本模块是**同步**的（内部全是 `std::process::Command`）。异步调用方应当用
//! `tokio::task::spawn_blocking` 包起来——`git worktree add` 是秒级操作，直接
//! 在 async 线程上跑会把执行器噎住。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};

pub use crate::workspace::{MergeMode, WorkspaceError};

/// 任务分支前缀：`latte/task/<task_id>`。与 HIL 黑板的 `latte/<task_id>`
/// 刻意分开，避免两套机制抢同一个分支名。
const TASK_BRANCH_PREFIX: &str = "latte/task/";

/// 写进 `.git/info/exclude` 的标记行（幂等判据 + 让用户看得懂这条哪来的）。
const STATE_EXCLUDE_MARKER: &str = "# latte: 任务隔离工作树的运行时状态目录（软链，勿提交）";

/// 排除 `.latte` 的 pathspec：`status` / `add` 都带上它，作为
/// `info/exclude` 之外的第二道保险（写 exclude 失败时仍不会误提交软链）。
const EXCLUDE_STATE_PATHSPEC: &str = ":(exclude).latte";

/// 落盘在任务 JSON 里的隔离工作树信息（`Task.worktree`）。
///
/// 记 `base_sha` 而不只记分支名：合并时要能回答"这棵树是从哪个提交分出去
/// 的"，分支名会被移动、SHA 不会。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskWorktreeInfo {
    /// worktree 目录（绝对路径）。agent 的 cwd 就是它。
    pub root: PathBuf,
    /// 任务分支名（`latte/task/<task_id>`）。
    pub branch: String,
    /// 派生时的 base 分支名。
    pub base_branch: String,
    /// 派生点的提交 SHA。
    pub base_sha: String,
}

/// 一个任务的隔离工作树句柄。
#[derive(Debug, Clone)]
pub struct TaskWorktree {
    task_id: String,
    repo_root: PathBuf,
    info: TaskWorktreeInfo,
}

/// 工作树相对 base 的改动摘要（[`TaskWorktree::diff_summary`]）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeDiff {
    pub branch: String,
    pub root: PathBuf,
    /// 比较基准（base 分支名，或分支已不在时的派生点 SHA）。
    pub base: String,
    /// 改动文件（按 `max_files` 截断）。
    pub files: Vec<WorktreeDiffFile>,
    /// 改动文件**总数**（未截断的真实数量）。
    pub file_count: usize,
    pub total_added: u64,
    pub total_removed: u64,
    /// 任务分支上尚未进 base 的提交（新到旧）。
    pub commits: Vec<WorktreeCommit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeDiffFile {
    pub path: String,
    pub added: u64,
    pub removed: u64,
    /// 二进制文件（numstat 给 `-`，行数无意义）。
    pub binary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeCommit {
    pub sha: String,
    pub subject: String,
}

/// 主仓根 → 互斥锁。git 的锁文件不等待，所以改仓库状态必须自己排队。
fn repo_locks() -> &'static Mutex<HashMap<PathBuf, Arc<Mutex<()>>>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 按**主仓根**序列化一段改仓库状态的操作（worktree add / merge / remove）。
///
/// 键取 canonicalize 后的路径：同一个仓库经软链/相对路径进来时必须落到同一
/// 把锁上，否则序列化形同虚设。canonicalize 失败（路径不存在）时退回原值——
/// 退化成"多把锁"，不会错锁。
///
/// 不可重入：**不要**在闭包里再调一次同仓库的 `with_repo_lock`。
pub fn with_repo_lock<T>(repo_root: &Path, f: impl FnOnce() -> T) -> T {
    let key = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let lock = {
        let mut map = repo_locks().lock().expect("repo lock map poisoned");
        map.entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    };
    // 锁中毒（持锁线程 panic）不该让后续任务全部瘫掉：git 操作本身是幂等
    // 可重试的，拿到中毒的锁继续用比永久拒绝服务好。
    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    f()
}

fn git(cwd: &Path, args: &[&str]) -> Result<String, WorkspaceError> {
    git_env(cwd, args, &[])
}

fn git_env(
    cwd: &Path,
    args: &[&str],
    envs: &[(&str, &std::ffi::OsStr)],
) -> Result<String, WorkspaceError> {
    let out = std::process::Command::new("git")
        .args(args)
        .envs(envs.iter().copied())
        .current_dir(cwd)
        .output()
        .map_err(|e| WorkspaceError::GitFailed(format!("spawn git {args:?}: {e}")))?;
    if !out.status.success() {
        // 失败原因要同时看 stderr **和 stdout**：`git merge` 的冲突详情
        // （`CONFLICT (content): Merge conflict in X`）走的是 stdout，只读
        // stderr 会得到一条空错误——看板上就成了「合并失败：」后面什么都没有，
        // 用户完全不知道冲突在哪个文件（实测踩过）。
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let detail = match (stderr.is_empty(), stdout.is_empty()) {
            (false, false) => format!("{stderr}\n{stdout}"),
            (false, true) => stderr,
            (true, false) => stdout,
            (true, true) => format!("exit {:?}", out.status.code()),
        };
        // 截断：冲突文件多时输出可以上千行，而这条消息会进任务 history。
        let detail: String = if detail.chars().count() > 800 {
            detail.chars().take(800).collect::<String>() + "…"
        } else {
            detail
        };
        return Err(WorkspaceError::GitFailed(format!("git {args:?}: {detail}")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `$LATTE_HOME`（缺省 `~/.latte`）—— 与 trace/全局配置同一个约定。
fn latte_home() -> PathBuf {
    if let Some(h) = std::env::var_os("LATTE_HOME") {
        return PathBuf::from(h);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    home.join(".latte")
}

/// 项目在 worktree 根下的目录名：`<仓库目录名>-<路径哈希 8 位>`。
///
/// 带哈希是因为同名仓库很常见（`~/a/redis` 与 `~/b/redis`），只用目录名会
/// 让两个项目的任务树撞进同一个目录。
fn project_slug(repo_root: &Path) -> String {
    let name = repo_root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("project")
        .replace(
            |c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_',
            "-",
        );
    let mut h: u64 = 0xcbf29ce484222325;
    for b in repo_root.to_string_lossy().as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{name}-{:08x}", (h & 0xffff_ffff) as u32)
}

/// 会话工作区派生点（未提交改动快照）的 ref：`<前缀><session_id>`。
const SESSION_BASE_REF: &str = "refs/latte/session-base/";

/// 把 `checkout` 里已跟踪文件的未提交改动（含已暂存）拍成一个提交，父提交
/// 是它的 HEAD。用临时 index，不动用户的 index 和工作区。没有改动返回 None。
///
/// 未跟踪的新文件不收：没进 .gitignore 的 node_modules、构建产物会被整个
/// 打进对象库。
fn snapshot_uncommitted(checkout: &Path, tag: &str) -> Result<Option<String>, WorkspaceError> {
    let head = git(checkout, &["rev-parse", "HEAD"])?;
    let git_path = |p: &str| -> Result<PathBuf, WorkspaceError> {
        git(
            checkout,
            &["rev-parse", "--path-format=absolute", "--git-path", p],
        )
        .map(PathBuf::from)
    };
    let index = git_path(&format!("latte-snapshot-{tag}.index"))?;
    // 复制真 index 而不是 read-tree：带着 stat 缓存，add -u 不必重哈希全仓。
    if std::fs::copy(git_path("index")?, &index).is_err() {
        git_env(
            checkout,
            &["read-tree", "HEAD"],
            &[("GIT_INDEX_FILE", index.as_os_str())],
        )?;
    }
    let result = (|| {
        let env = [("GIT_INDEX_FILE", index.as_os_str())];
        git_env(checkout, &["add", "-u", "--", "."], &env)?;
        let tree = git_env(checkout, &["write-tree"], &env)?;
        if tree == git(checkout, &["rev-parse", "HEAD^{tree}"])? {
            return Ok(None);
        }
        // 快照在保留分支时会被 rebase 掉，不进用户历史；身份用固定值，
        // 免得用户没配 user.name 时整个会话建不出来。
        let ident = std::ffi::OsStr::new("latte");
        let mail = std::ffi::OsStr::new("latte@local");
        git_env(
            checkout,
            &[
                "commit-tree",
                &tree,
                "-p",
                &head,
                "-m",
                "latte: 会话开始时的未提交改动（快照）",
            ],
            &[
                ("GIT_AUTHOR_NAME", ident),
                ("GIT_AUTHOR_EMAIL", mail),
                ("GIT_COMMITTER_NAME", ident),
                ("GIT_COMMITTER_EMAIL", mail),
            ],
        )
        .map(Some)
    })();
    let _ = std::fs::remove_file(&index);
    result
}

/// 任务 id 能否安全用作目录名/分支名的一段。
fn safe_task_id(task_id: &str) -> bool {
    !task_id.is_empty()
        && task_id.len() <= 64
        && task_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !task_id.starts_with('.')
        && !task_id.contains("..")
}

impl TaskWorktree {
    pub fn task_id(&self) -> &str {
        &self.task_id
    }
    pub fn info(&self) -> &TaskWorktreeInfo {
        &self.info
    }
    pub fn root(&self) -> &Path {
        &self.info.root
    }
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// 该任务的 worktree 目录（不保证存在）。
    pub fn derive_root(repo_root: &Path, task_id: &str) -> PathBuf {
        latte_home()
            .join("worktrees")
            .join(project_slug(repo_root))
            .join(task_id)
    }

    /// 解析仓库根；`cwd` 不在 git 仓库里时返回 `NotARepo`（调用方据此降级为
    /// 非隔离派发，而不是让整个看板不可用）。
    pub fn resolve_repo_root(cwd: &Path) -> Result<PathBuf, WorkspaceError> {
        // `--show-toplevel` 在 worktree 里返回该 worktree 的根，不是主仓；
        // 用 `--path-format=absolute --git-common-dir` 才能拿到主仓的 .git，
        // 它的父目录才是主仓根。
        let common = git(
            cwd,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        if common.is_empty() {
            return Err(WorkspaceError::NotARepo(cwd.to_path_buf()));
        }
        let p = PathBuf::from(common);
        let root = p
            .parent()
            .ok_or_else(|| WorkspaceError::NotARepo(cwd.to_path_buf()))?;
        Ok(root.to_path_buf())
    }

    /// 建（或复用）任务的隔离工作树。
    ///
    /// - 目录已存在且仍是这个仓库的合法 worktree → **原样复用**（rework /
    ///   重新派发同一个任务时不该丢掉上次的进展）；
    /// - 分支已存在但树没了（上次 cleanup 只删了树）→ 用已有分支重挂一棵树；
    /// - 都没有 → 从当前 HEAD 拉新分支。
    ///
    /// `base` 为 `None` 时取仓库当前 HEAD 所在分支。
    pub fn open_or_create(
        repo_root: &Path,
        task_id: &str,
        base: Option<&str>,
    ) -> Result<Self, WorkspaceError> {
        Self::open_or_create_at(repo_root, task_id, base, None)
    }

    /// 同 [`Self::open_or_create`]；新建分支时从 `start`（缺省 base 分支）拉出。
    fn open_or_create_at(
        repo_root: &Path,
        task_id: &str,
        base: Option<&str>,
        start: Option<&str>,
    ) -> Result<Self, WorkspaceError> {
        if !safe_task_id(task_id) {
            return Err(WorkspaceError::TaskIdInUse(format!(
                "task_id {task_id:?} 不能用作目录/分支名"
            )));
        }
        let root = Self::derive_root(repo_root, task_id);
        let branch = format!("{TASK_BRANCH_PREFIX}{task_id}");

        with_repo_lock(repo_root, || {
            let base_branch = match base {
                Some(b) => b.to_string(),
                None => {
                    let b = git(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])?;
                    if b.is_empty() || b == "HEAD" {
                        return Err(WorkspaceError::NotARepo(repo_root.to_path_buf()));
                    }
                    b
                }
            };

            // 已有可用的树 → 复用。
            if root.join(".git").exists() {
                let base_sha = git(&root, &["rev-parse", &format!("{branch}@{{0}}")])
                    .or_else(|_| git(&root, &["rev-parse", "HEAD"]))?;
                let wt = Self {
                    task_id: task_id.to_string(),
                    repo_root: repo_root.to_path_buf(),
                    info: TaskWorktreeInfo {
                        root,
                        branch,
                        base_branch,
                        base_sha,
                    },
                };
                wt.link_state_dir()?;
                wt.ensure_state_dir_ignored();
                return Ok(wt);
            }

            let base_sha = match start {
                Some(sha) => sha.to_string(),
                None => git(repo_root, &["rev-parse", &base_branch])?,
            };
            std::fs::create_dir_all(root.parent().expect("derive_root 至少两级"))?;
            let root_str = root.to_str().ok_or_else(|| {
                WorkspaceError::GitFailed(format!("worktree 路径非 UTF-8: {}", root.display()))
            })?;
            // 分支可能是上次留下的（cleanup 删树没删分支，或合并冲突时刻意
            // 保留）：存在就直接挂上去，不存在才 -b 新建。
            let branch_exists = git(
                repo_root,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{branch}"),
                ],
            )
            .is_ok();
            if branch_exists {
                git(repo_root, &["worktree", "add", root_str, &branch])?;
            } else {
                git(
                    repo_root,
                    &["worktree", "add", "-b", &branch, root_str, &base_sha],
                )?;
            }

            let wt = Self {
                task_id: task_id.to_string(),
                repo_root: repo_root.to_path_buf(),
                info: TaskWorktreeInfo {
                    root,
                    branch,
                    base_branch,
                    base_sha,
                },
            };
            wt.link_state_dir()?;
            wt.ensure_state_dir_ignored();
            Ok(wt)
        })
    }

    /// 确保 `.latte` 不被 git 看见。
    ///
    /// **为什么必须做**：worktree 根下的 `.latte` 软链对 git 就是一个未跟踪
    /// 条目（`?? .latte`）。不处理的话 [`Self::merge_back`] 的自动提交会把这个
    /// **指向主仓的软链**提交进任务分支，合并后进用户的仓库历史——一个指向
    /// 别人机器上绝对路径的软链。实测就是这么发现的（`has_changes()` 在刚建好
    /// 的空树上返回 true）。
    ///
    /// 处理顺序：
    /// 1. 已经被忽略（大多数 latte 项目的 `.gitignore` 里就写了 `.latte/`）→
    ///    什么都不做；
    /// 2. 仓库**自己跟踪** `.latte/` 下的文件（latte 自身的仓库就是这样：
    ///    `.latte/agents/*.toml`、`.latte/workflows/*.toml` 是源码的一部分，
    ///    `.gitignore` 只忽略其中的运行时子目录）→ **也什么都不做**。往
    ///    `info/exclude` 写 `/.latte` 会让整棵 `.latte` 对 git 消失，那些
    ///    `!.latte/agents/` 的再包含全部失效，新建的角色配置从此静默不进
    ///    版本控制；
    /// 3. 其余情况才往 `.git/info/exclude` 追加一条。这是 git 专门给"本地
    ///    忽略、不改仓库跟踪文件"的地方：不会被提交、不会被推送、不出现在
    ///    `git status`。带标记注释便于溯源，且幂等。
    ///
    /// 第 2 种情况下靠 [`Self::add_all`] 的 pathspec 兜底排除软链。
    ///
    /// 失败不阻断派发（隔离本身照常可用）。
    fn ensure_state_dir_ignored(&self) {
        if git(&self.repo_root, &["check-ignore", "-q", ".latte"]).is_ok() {
            return;
        }
        if git(&self.repo_root, &["ls-files", "--", ".latte"]).is_ok_and(|out| !out.is_empty()) {
            return;
        }
        let Ok(path) = git(
            &self.repo_root,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "info/exclude",
            ],
        ) else {
            return;
        };
        let path = PathBuf::from(path);
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        if existing.contains(STATE_EXCLUDE_MARKER) {
            return;
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut next = existing;
        if !next.is_empty() && !next.ends_with('\n') {
            next.push('\n');
        }
        next.push_str(STATE_EXCLUDE_MARKER);
        next.push('\n');
        next.push_str("/.latte\n");
        if let Err(e) = std::fs::write(&path, next) {
            eprintln!("[task-worktree] 写 info/exclude 失败（不阻断）: {e}");
        }
    }

    /// 把 `<worktree>/.latte` 做成指向 `<repo>/.latte` 的软链。
    ///
    /// agent 的 cwd 是这棵树，而**运行时状态全部按 cwd 定位**：会话日志
    /// （`.latte/ui-sessions/`）、workflow 断点（`.latte/workflow-runs/`）、
    /// 挂起的提问、任务看板本体（`.latte/tasks/`）、代码图索引、以及项目层
    /// 配置（`.latte/agents.d/`、`.latte/workflows.d/`、`.latte/tools.d/`）。
    ///
    /// 不软链会同时坏两件事：① 项目配置读不到，隔离任务用的角色/流程/工具
    /// 文档与主仓不一致；② 状态写进树里，`cleanup()` 一删就没了，归档后
    /// resume 断掉。
    ///
    /// 已存在（软链或真目录）时不动它——真目录多半是上次非 unix 平台跑出来
    /// 的，覆盖它等于删用户数据。
    fn link_state_dir(&self) -> Result<(), WorkspaceError> {
        let link = self.info.root.join(".latte");
        if link.exists() || link.symlink_metadata().is_ok() {
            return Ok(());
        }
        let target = self.repo_root.join(".latte");
        std::fs::create_dir_all(&target)?;
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, &link)?;
        }
        #[cfg(not(unix))]
        {
            // Windows 建软链要特权；退化成不链——状态会落在树里，
            // cleanup 前调用方应当先归档。
            let _ = &target;
        }
        Ok(())
    }

    /// `git add -A`，但**绝不**把 `.latte` 软链纳入。
    ///
    /// 两种写法必须按 `.latte` 是否已被忽略分流，不能只用一种：
    /// - 已忽略 → 必须用**朴素** `add -A`。带 `:(exclude).latte` 时 git 反而报
    ///   「路径被 .gitignore 忽略，请用 -f」并整条命令失败（实测）。
    /// - 未忽略（`info/exclude` 没写成功）→ 必须带 `:!.latte`，否则朴素 `-A`
    ///   会静默把软链提交进任务分支、合并进用户仓库历史。
    fn add_all(&self) -> Result<(), WorkspaceError> {
        let ignored = git(&self.info.root, &["check-ignore", "-q", ".latte"]).is_ok();
        if ignored {
            git(&self.info.root, &["add", "-A"])?;
        } else {
            git(
                &self.info.root,
                &["add", "-A", "--", EXCLUDE_STATE_PATHSPEC],
            )?;
        }
        Ok(())
    }

    /// 工作树里是否有**尚未合并**的改动：未提交的改动，或分支相对 base 有
    /// 领先提交。`false` = 合并回去等于空操作。
    pub fn has_changes(&self) -> Result<bool, WorkspaceError> {
        if !self.info.root.exists() {
            return Ok(false);
        }
        if self.is_dirty()? {
            return Ok(true);
        }
        Ok(self.unmerged_commits()? > 0)
    }

    fn is_dirty(&self) -> Result<bool, WorkspaceError> {
        Ok(!git(
            &self.info.root,
            &["status", "--porcelain", "--", ".", EXCLUDE_STATE_PATHSPEC],
        )?
        .is_empty())
    }

    /// 任务分支上**还没进 base 分支**的提交数。
    ///
    /// 判据用 `<base 分支当前 tip>..HEAD`，不用派生点 `base_sha`：合并回去之后
    /// base 已经包含这些提交，用 base_sha 比会永远算成"仍有改动"（实测：merge
    /// 之后 `has_changes()` 依然 true）。base 分支被删/改名时退回 base_sha——
    /// 那种情况下"相对派生点有没有新提交"是唯一还能算的判据。
    fn unmerged_commits(&self) -> Result<u64, WorkspaceError> {
        let range_from = if git(
            &self.info.root,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{}", self.info.base_branch),
            ],
        )
        .is_ok()
        {
            self.info.base_branch.clone()
        } else {
            self.info.base_sha.clone()
        };
        let n = git(
            &self.info.root,
            &["rev-list", "--count", &format!("{range_from}..HEAD")],
        )?;
        Ok(n.parse::<u64>().unwrap_or(0))
    }

    /// 把工作树的改动合并回 base 分支。
    ///
    /// 1. 树里未提交的改动先自动提交（agent 未必自己 commit）；
    /// 2. 在主仓上 `git merge <task 分支>`；
    /// 3. 冲突 → `merge --abort` 复位主仓，返回 Err，**保留工作树和分支**
    ///    交人工处置（同 oh-my-pi「patch apply 失败就把 .patch 留在原地」的
    ///    取舍：绝不自动硬解冲突）。
    ///
    /// 返回合并提交 SHA；无改动时返回当前 HEAD（空操作）。
    pub fn merge_back(&self, mode: MergeMode) -> Result<String, WorkspaceError> {
        if !self.info.root.exists() {
            return Err(WorkspaceError::NotFound(self.info.root.clone()));
        }
        with_repo_lock(&self.repo_root, || {
            if !git(
                &self.info.root,
                &["status", "--porcelain", "--", ".", EXCLUDE_STATE_PATHSPEC],
            )?
            .is_empty()
            {
                self.add_all()?;
                git(
                    &self.info.root,
                    &[
                        "commit",
                        "-m",
                        &format!("task({}): auto-commit before merge", self.task_id),
                    ],
                )?;
            }
            if self.unmerged_commits()? == 0 {
                // 无改动：不产生空合并提交（`--no-ff` 会造一个），直接回报
                // 当前 HEAD。
                return git(&self.repo_root, &["rev-parse", "HEAD"]);
            }
            let flag = match mode {
                MergeMode::NoFf => "--no-ff",
                MergeMode::Squash => "--squash",
                MergeMode::FastForward => "--ff-only",
            };
            let msg = format!("task({}): merge {}", self.task_id, self.info.branch);
            if let Err(e) = git(
                &self.repo_root,
                &["merge", flag, "-m", &msg, &self.info.branch],
            ) {
                // 复位主仓，别把 base 分支留在半合并状态。
                let _ = git(&self.repo_root, &["merge", "--abort"]);
                return Err(e);
            }
            git(&self.repo_root, &["rev-parse", "HEAD"])
        })
    }

    /// 删掉工作树和任务分支（best-effort，不 panic）。
    ///
    /// `keep_branch = true` 时只删树、留分支——合并冲突或用户取消之后要留证据，
    /// 分支是唯一还拿得到那些工作的地方。**留分支时会先把未提交的改动自动
    /// 提交上去**：agent 通常不自己 commit（`merge_back` 也得替它提交），直接
    /// `worktree remove --force` 会把这些改动连同目录一起销毁——分支保住了却
    /// 空无一物，等于"留分支"的承诺没兑现。实测取消一个任务就踩到了这个坑。
    ///
    /// **不会**动主仓的 `.latte`：树里的 `.latte` 是软链，`worktree remove`
    /// 与 `remove_dir_all` 都只删软链本身、不跟进目标（有回归测试钉住）。
    pub fn cleanup(&self, keep_branch: bool) -> Result<(), WorkspaceError> {
        with_repo_lock(&self.repo_root, || {
            if keep_branch && self.info.root.exists() {
                // 先落盘未提交的改动，再删树。失败不阻断回收（磁盘照样要还），
                // 但要出声——否则用户以为分支里有东西。
                match git(
                    &self.info.root,
                    &["status", "--porcelain", "--", ".", EXCLUDE_STATE_PATHSPEC],
                ) {
                    Ok(out) if !out.is_empty() => {
                        let msg = format!("task({}): WIP 未完成的改动（保留分支）", self.task_id);
                        if let Err(e) = self
                            .add_all()
                            .and_then(|_| git(&self.info.root, &["commit", "-m", &msg]).map(|_| ()))
                        {
                            eprintln!(
                                "[task-worktree] {}: 保留分支前提交 WIP 失败，未提交的改动将丢失: {e}",
                                self.task_id
                            );
                        }
                    }
                    _ => {}
                }
            }
            if self.info.root.exists() {
                let root_str = self.info.root.to_string_lossy().to_string();
                if let Err(e) = git(
                    &self.repo_root,
                    &["worktree", "remove", "--force", &root_str],
                ) {
                    eprintln!("[task-worktree] worktree remove 失败，改用直接删除: {e}");
                    let _ = std::fs::remove_dir_all(&self.info.root);
                }
            }
            let _ = git(&self.repo_root, &["worktree", "prune"]);
            if !keep_branch {
                if let Err(e) = git(&self.repo_root, &["branch", "-D", &self.info.branch]) {
                    // 分支可能本来就没了（never committed / 已被删），不算失败。
                    eprintln!(
                        "[task-worktree] 删分支 {} 失败（可忽略）: {e}",
                        self.info.branch
                    );
                }
            }
            Ok(())
        })
    }

    /// 从落盘信息重建句柄（重启后仍能合并/清理已有的树）。
    pub fn from_info(repo_root: &Path, task_id: &str, info: TaskWorktreeInfo) -> Self {
        Self {
            task_id: task_id.to_string(),
            repo_root: repo_root.to_path_buf(),
            info,
        }
    }

    /// 工作树相对 base 的改动摘要：改了哪些文件、各加删多少行、有哪些提交。
    ///
    /// 给「批准前先看一眼」用：隔离任务的改动全在树里，用户在看板上点批准
    /// 之前根本看不到自己在批什么——只能看到 workflow 的文字摘要，而文字
    /// 摘要恰恰是最容易漂的东西。
    ///
    /// **含未提交改动**：agent 未必自己 commit，`merge_back` 会替它提交，所以
    /// 这里的口径必须与合并口径一致（否则「预览没有、合并却进去了」）。
    pub fn diff_summary(&self, max_files: usize) -> Result<WorktreeDiff, WorkspaceError> {
        if !self.info.root.exists() {
            return Err(WorkspaceError::NotFound(self.info.root.clone()));
        }
        let base = self.diff_base();
        // `diff <base>` （不带 ..HEAD）= 已提交 + 未提交，一次拿全，与
        // merge_back 的口径一致。
        let numstat = git(&self.info.root, &["diff", "--numstat", &base])?;
        let mut files: Vec<WorktreeDiffFile> = Vec::new();
        let mut total_added = 0u64;
        let mut total_removed = 0u64;
        let mut counted = 0usize;
        for line in numstat.lines() {
            let mut parts = line.split('\t');
            let (a, r, path) = match (parts.next(), parts.next(), parts.next()) {
                (Some(a), Some(r), Some(p)) => (a, r, p),
                _ => continue,
            };
            // 二进制文件的 numstat 是 `-`，按 0 计但仍列出来。
            let added = a.parse::<u64>().unwrap_or(0);
            let removed = r.parse::<u64>().unwrap_or(0);
            total_added += added;
            total_removed += removed;
            counted += 1;
            if files.len() < max_files {
                files.push(WorktreeDiffFile {
                    path: path.to_string(),
                    added,
                    removed,
                    binary: a == "-" || r == "-",
                });
            }
        }
        // 未跟踪的新文件不出现在 `diff` 里，但 `merge_back` 的 `add -A` 会
        // 收下它们——不列出来就会出现「预览没有、合并却进去了」。
        for path in git(
            &self.info.root,
            &["ls-files", "--others", "--exclude-standard"],
        )?
        .lines()
        .filter(|l| !l.is_empty())
        {
            counted += 1;
            if files.len() < max_files {
                files.push(WorktreeDiffFile {
                    path: path.to_string(),
                    added: 0,
                    removed: 0,
                    binary: false,
                });
            }
        }
        let commits = git(
            &self.info.root,
            &["log", "--format=%h\t%s", &format!("{base}..HEAD")],
        )?
        .lines()
        .filter_map(|l| {
            let (sha, subject) = l.split_once('\t')?;
            Some(WorktreeCommit {
                sha: sha.to_string(),
                subject: subject.to_string(),
            })
        })
        .collect();
        Ok(WorktreeDiff {
            branch: self.info.branch.clone(),
            root: self.info.root.clone(),
            base: base.clone(),
            files,
            file_count: counted,
            total_added,
            total_removed,
            commits,
        })
    }

    /// diff / 提交清单的比较基准：base 分支当前 tip（还在的话），否则派生点。
    /// 与 [`Self::unmerged_commits`] 同判据——两处口径必须一致，否则会出现
    /// 「预览说有改动、合并却说没有」。
    fn diff_base(&self) -> String {
        if git(
            &self.info.root,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{}", self.info.base_branch),
            ],
        )
        .is_ok()
        {
            self.info.base_branch.clone()
        } else {
            self.info.base_sha.clone()
        }
    }

    /// 列出本仓库在 `$LATTE_HOME/worktrees/<项目>/` 下的全部任务树目录名
    /// （= task_id）。目录不存在时返回空。
    ///
    /// 给「孤儿回收」用：进程被 kill 在建树与落盘之间、或任务记录被外部删掉
    /// 时，树会永久留在磁盘上（每棵都是一份完整检出，几百 MB 级）。
    pub fn list_task_ids(repo_root: &Path) -> Vec<String> {
        let dir = latte_home().join("worktrees").join(project_slug(repo_root));
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut out: Vec<String> = entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .collect();
        out.sort();
        out
    }

    /// 回收一棵**孤儿**任务树：只在它没有未合并提交时才删，有就留着并返回
    /// `false`（宁可占磁盘，也不能静默销毁没合并的工作成果）。
    ///
    /// `Ok(true)` = 已回收；`Ok(false)` = 有未合并提交，保留。
    pub fn reclaim_orphan(repo_root: &Path, task_id: &str) -> Result<bool, WorkspaceError> {
        let root = Self::derive_root(repo_root, task_id);
        if !root.is_dir() {
            return Ok(true);
        }
        let branch = format!("{TASK_BRANCH_PREFIX}{task_id}");
        // base 分支未知（没有落盘信息），用仓库当前 HEAD 分支近似：孤儿树的
        // 派生点通常就是它。
        let base_branch =
            git(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
        let base_sha = git(repo_root, &["rev-parse", "HEAD"]).unwrap_or_default();
        let wt = Self {
            task_id: task_id.to_string(),
            repo_root: repo_root.to_path_buf(),
            info: TaskWorktreeInfo {
                root,
                branch,
                base_branch,
                base_sha,
            },
        };
        if wt.has_changes().unwrap_or(true) {
            return Ok(false);
        }
        wt.cleanup(false)?;
        Ok(true)
    }

    /// 普通 session 的独立工作空间根：`derive_root(<项目>, <session_id>)`。
    pub fn session_workspace_root(project_root: &Path, session_id: &str) -> PathBuf {
        Self::derive_root(project_root, session_id)
    }

    /// 给普通 session 建独立工作空间，返回目录。
    ///
    /// git 仓库 → git worktree（agent 能看到项目代码，改动互不影响，`.latte`
    /// 软链回项目状态目录）。非 git / 建树失败 → 直接用项目目录：空目录里
    /// agent 看不到任何源码，读代码的 workflow（learn_loop 取证等）全部失效。
    /// 项目目录不是 [`Self::session_workspace_root`]，删会话时不会被删。
    ///
    /// 新建的树从项目里已跟踪文件的未提交改动快照拉出：用户在编辑器里改了
    /// 还没 commit 的代码，会话里也能读到（learn_loop 取证、行号才对得上）。
    /// 快照记在 [`SESSION_BASE_REF`] 下，删会话时据此只算会话自己的产出。
    pub fn create_session_workspace(project_root: &Path, session_id: &str) -> std::io::Result<PathBuf> {
        if !safe_task_id(session_id) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("session id {session_id:?} 不能用作目录名"),
            ));
        }
        if let Ok(repo) = Self::resolve_repo_root(project_root) {
            let fresh = !Self::derive_root(&repo, session_id).join(".git").exists();
            let snapshot = if fresh {
                snapshot_uncommitted(project_root, session_id)
                    .map_err(|e| eprintln!("[sessions] 未提交改动快照失败，会话从 HEAD 开始: {e}"))
                    .ok()
                    .flatten()
            } else {
                None
            };
            if let Ok(wt) = Self::open_or_create_at(&repo, session_id, None, snapshot.as_deref()) {
                if let Some(sha) = &snapshot {
                    let _ = git(
                        &repo,
                        &["update-ref", &format!("{SESSION_BASE_REF}{session_id}"), sha],
                    );
                }
                return Ok(wt.info.root);
            }
        }
        Ok(project_root.to_path_buf())
    }

    /// 保留会话分支前去掉开头的快照提交：分支只剩会话自己的提交，接在原
    /// HEAD 上。不去掉的话，用户合并分支会把自己工作区里仍未提交的那些改动
    /// 再合进来一遍，git 直接拒绝（本地改动会被覆盖）。
    /// 变基冲突时保持原样：快照还在，但会话的产出一行不丢。
    fn drop_session_snapshot(&self, snapshot: &str) {
        with_repo_lock(&self.repo_root, || {
            let root = &self.info.root;
            if self.is_dirty().unwrap_or(false) {
                let msg = format!("task({}): WIP 未完成的改动（保留分支）", self.task_id);
                if self
                    .add_all()
                    .and_then(|_| git(root, &["commit", "-m", &msg]).map(|_| ()))
                    .is_err()
                {
                    return;
                }
            }
            let parent = format!("{snapshot}^");
            if let Err(e) = git(root, &["rebase", "--onto", &parent, snapshot]) {
                let _ = git(root, &["rebase", "--abort"]);
                eprintln!(
                    "[sessions] {}: 去掉未提交改动快照失败，分支里保留快照提交: {e}",
                    self.task_id
                );
            }
        });
    }

    /// 删除 [`create_session_workspace`] 建的目录。`root` 必须恰好是该 session
    /// 的工作空间根，否则不动。幂等。
    ///
    /// 有未合并的工作（未提交改动或领先提交，如 learn_loop 定稿的
    /// `docs/learn/<slug>.md`）时只删树、留分支并把改动提交上去，返回保留的
    /// 分支名；删会话不能静默销毁没合并的成果。没有可丢的东西才连分支一起删。
    pub fn remove_session_workspace(
        project_root: &Path,
        session_id: &str,
        root: &Path,
    ) -> Option<String> {
        let repo = Self::resolve_repo_root(project_root).ok();
        // git 分支下树根按 git 解析出的仓库根派生（可能是 canonical 路径），
        // 与 project_root 直接派生的不一定相同。
        let is_own = root == Self::session_workspace_root(project_root, session_id)
            || repo
                .as_deref()
                .is_some_and(|r| root == Self::derive_root(r, session_id));
        if !is_own {
            return None;
        }
        let mut kept = None;
        if root.join(".git").exists() {
            if let Some(repo) = repo {
                let wt = Self::from_info(
                    &repo,
                    session_id,
                    TaskWorktreeInfo {
                        root: root.to_path_buf(),
                        branch: format!("{TASK_BRANCH_PREFIX}{session_id}"),
                        base_branch: git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"])
                            .unwrap_or_default(),
                        base_sha: git(&repo, &["rev-parse", "HEAD"]).unwrap_or_default(),
                    },
                );
                let base_ref = format!("{SESSION_BASE_REF}{session_id}");
                let snapshot = git(&repo, &["rev-parse", "--verify", "--quiet", &base_ref]).ok();
                // 有快照时以快照为基准：快照本身不是会话的产出。
                let keep = match &snapshot {
                    Some(s) => {
                        wt.is_dirty().unwrap_or(true)
                            || git(root, &["rev-list", "--count", &format!("{s}..HEAD")])
                                .map_or(true, |n| n != "0")
                    }
                    None => wt.has_changes().unwrap_or(true),
                };
                if let (true, Some(s)) = (keep, &snapshot) {
                    wt.drop_session_snapshot(s);
                }
                let _ = wt.cleanup(keep);
                if snapshot.is_some() {
                    let _ = git(&repo, &["update-ref", "-d", &base_ref]);
                }
                if keep {
                    kept = Some(wt.info.branch.clone());
                }
            }
        }
        // 软链只删链本身，不跟进主仓 .latte。
        let _ = std::fs::remove_dir_all(root);
        kept
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_slug_disambiguates_same_named_repos() {
        let a = project_slug(Path::new("/home/u/a/redis"));
        let b = project_slug(Path::new("/home/u/b/redis"));
        assert!(a.starts_with("redis-"));
        assert!(b.starts_with("redis-"));
        assert_ne!(a, b, "同名不同路径的仓库必须落到不同目录");
        assert_eq!(
            a,
            project_slug(Path::new("/home/u/a/redis")),
            "同一路径必须稳定"
        );
    }

    #[test]
    fn task_id_safety_rejects_path_tricks() {
        assert!(safe_task_id("LAT-100"));
        assert!(safe_task_id("lat_100.2"));
        assert!(!safe_task_id(""));
        assert!(!safe_task_id("../escape"));
        assert!(!safe_task_id("a/b"));
        assert!(!safe_task_id(".hidden"));
        assert!(!safe_task_id("a b"));
        assert!(!safe_task_id(&"x".repeat(65)));
    }

    #[test]
    fn derive_root_is_outside_the_repo() {
        let repo = Path::new("/tmp/some-repo");
        let root = TaskWorktree::derive_root(repo, "LAT-100");
        assert!(
            !root.starts_with(repo),
            "任务树必须建在仓库外，否则主仓的 search/代码图会扫到 N 份代码副本：{}",
            root.display()
        );
        assert!(root.ends_with("LAT-100"));
    }

    /// 同一把锁下的操作严格串行（git 的 index.lock 不等待，必须自己排队）。
    #[test]
    fn repo_lock_serializes_same_repo() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let live = Arc::new(AtomicUsize::new(0));
        let max = Arc::new(AtomicUsize::new(0));
        let mut hs = Vec::new();
        for _ in 0..8 {
            let (p, live, max) = (dir.path().to_path_buf(), live.clone(), max.clone());
            hs.push(std::thread::spawn(move || {
                with_repo_lock(&p, || {
                    let n = live.fetch_add(1, Ordering::SeqCst) + 1;
                    max.fetch_max(n, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    live.fetch_sub(1, Ordering::SeqCst);
                })
            }));
        }
        for h in hs {
            h.join().unwrap();
        }
        assert_eq!(max.load(Ordering::SeqCst), 1, "同仓库的改动必须串行");
    }

    /// 不同仓库互不阻塞（否则一个慢仓库会拖死整台机器上的所有任务）。
    #[test]
    fn repo_lock_allows_different_repos_in_parallel() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let (ta, tb) = (a.path().to_path_buf(), b.path().to_path_buf());
        let started = Arc::new(std::sync::Barrier::new(2));
        let s1 = started.clone();
        let h = std::thread::spawn(move || {
            with_repo_lock(&ta, || {
                s1.wait();
                std::thread::sleep(std::time::Duration::from_millis(20));
            })
        });
        with_repo_lock(&tb, || {
            // 若两个仓库共用一把锁，这里会等到上面那 20ms 睡完才进得来，
            // barrier 就会超时死锁——能同时进来即证明锁是按仓库分的。
            started.wait();
        });
        h.join().unwrap();
    }
}

#[cfg(test)]
mod session_workspace_tests {
    use super::*;

    fn git_in(dir: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git 可用")
    }

    fn branch_exists(repo: &Path, branch: &str) -> bool {
        git_in(repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")])
            .status
            .success()
    }

    /// LATTE_HOME 是进程级环境变量，非 git 与 git 两种情形放在同一个用例里
    /// 顺序跑，避免并发用例互相改掉对方的 home。
    #[test]
    fn session_workspace_lifecycle_keeps_project_and_unmerged_work() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("LATTE_HOME", home.path());

        // 非 git：直接用项目目录，删会话不动它。
        let proj = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(proj.path().join(".latte")).unwrap();
        std::fs::write(proj.path().join("main.c"), "int main(){}\n").unwrap();
        let root = TaskWorktree::create_session_workspace(proj.path(), "ui-1-2-w0").unwrap();
        assert_eq!(root, proj.path(), "非 git 项目要让 agent 看得到源码");
        assert_eq!(TaskWorktree::remove_session_workspace(proj.path(), "ui-1-2-w0", &root), None);
        assert!(proj.path().join("main.c").exists());

        // git：会话里产出的文档不能随删会话丢失。
        let repo = tempfile::tempdir().unwrap();
        let r = repo.path();
        if !git_in(r, &["init", "-q"]).status.success() {
            eprintln!("git init 失败，跳过该用例");
            return;
        }
        let _ = git_in(r, &["config", "user.email", "t@t"]);
        let _ = git_in(r, &["config", "user.name", "t"]);
        std::fs::write(r.join("main.c"), "int main(){}\n").unwrap();
        let _ = git_in(r, &["add", "."]);
        let _ = git_in(r, &["commit", "-qm", "init"]);

        let ws = TaskWorktree::create_session_workspace(r, "ui-1-2-w1").unwrap();
        assert_ne!(ws, r);
        assert!(ws.join("main.c").exists());
        assert_eq!(
            TaskWorktree::remove_session_workspace(r, "other", &ws),
            None,
            "路径不匹配不动"
        );
        assert!(ws.is_dir());
        std::fs::create_dir_all(ws.join("docs/learn")).unwrap();
        std::fs::write(ws.join("docs/learn/own.md"), "# 所有权\n").unwrap();
        let branch = format!("{TASK_BRANCH_PREFIX}ui-1-2-w1");
        assert_eq!(
            TaskWorktree::remove_session_workspace(r, "ui-1-2-w1", &ws),
            Some(branch.clone())
        );
        assert!(!ws.exists());
        assert!(branch_exists(r, &branch), "有未合并产出必须留分支");
        let kept = git_in(r, &["show", &format!("{branch}:docs/learn/own.md")]);
        assert_eq!(String::from_utf8_lossy(&kept.stdout), "# 所有权\n");

        // 没产出的会话：树和分支一起删。
        let ws = TaskWorktree::create_session_workspace(r, "ui-1-2-w2").unwrap();
        assert_eq!(TaskWorktree::remove_session_workspace(r, "ui-1-2-w2", &ws), None);
        assert!(!ws.exists());
        assert!(!branch_exists(r, &format!("{TASK_BRANCH_PREFIX}ui-1-2-w2")));

        // 用户改了 main.c 还没 commit：会话里读到的是改后的版本，用户的
        // 工作区和 index 原样不动。
        let head = String::from_utf8_lossy(&git_in(r, &["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_string();
        let edited = "int main(){ return 1; }\n";
        std::fs::write(r.join("main.c"), edited).unwrap();
        std::fs::write(r.join("new.c"), "// 未跟踪\n").unwrap();
        let status_before = git_in(r, &["status", "--porcelain"]).stdout;
        let ws = TaskWorktree::create_session_workspace(r, "ui-1-2-w3").unwrap();
        assert_eq!(std::fs::read_to_string(ws.join("main.c")).unwrap(), edited);
        assert!(!ws.join("new.c").exists(), "未跟踪文件不进快照");
        assert_eq!(git_in(r, &["status", "--porcelain"]).stdout, status_before);
        assert!(git_in(r, &["diff", "--cached", "--quiet"]).status.success());
        // 会话自己没产出：快照不算改动，树、分支、快照 ref 一起删。
        assert_eq!(TaskWorktree::remove_session_workspace(r, "ui-1-2-w3", &ws), None);
        assert!(!branch_exists(r, &format!("{TASK_BRANCH_PREFIX}ui-1-2-w3")));
        assert!(!git_in(
            r,
            &["rev-parse", "--verify", "--quiet", &format!("{SESSION_BASE_REF}ui-1-2-w3")]
        )
        .status
        .success());

        // 会话有产出：保留的分支直接接在原 HEAD 上，只有会话自己的文档，
        // 不夹带用户那份未提交的 main.c。
        let ws = TaskWorktree::create_session_workspace(r, "ui-1-2-w4").unwrap();
        std::fs::create_dir_all(ws.join("docs/learn")).unwrap();
        std::fs::write(ws.join("docs/learn/own.md"), "# 所有权\n").unwrap();
        let branch = format!("{TASK_BRANCH_PREFIX}ui-1-2-w4");
        assert_eq!(
            TaskWorktree::remove_session_workspace(r, "ui-1-2-w4", &ws),
            Some(branch.clone())
        );
        let parent = git_in(r, &["rev-parse", &format!("{branch}^")]).stdout;
        assert_eq!(String::from_utf8_lossy(&parent).trim(), head);
        let main_c = git_in(r, &["show", &format!("{branch}:main.c")]).stdout;
        assert_eq!(String::from_utf8_lossy(&main_c), "int main(){}\n");
        let doc = git_in(r, &["show", &format!("{branch}:docs/learn/own.md")]).stdout;
        assert_eq!(String::from_utf8_lossy(&doc), "# 所有权\n");
        assert_eq!(std::fs::read_to_string(r.join("main.c")).unwrap(), edited);
    }
}
