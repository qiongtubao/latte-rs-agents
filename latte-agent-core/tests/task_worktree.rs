//! `TaskWorktree` 的真 git 仓库端到端测试（Phase 2 的隔离原语）。
//!
//! 这些测试都用 `LATTE_HOME` 指到 tempdir，所以不会污染用户的 `~/.latte`。
//! 注意：`LATTE_HOME` 是进程级环境变量，因此本文件的测试**必须串行**跑
//! （用一把全局锁互斥），否则并行的测试会互相改掉对方的 HOME。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use latte_agent_core::task_worktree::TaskWorktree;
use latte_agent_core::workspace::MergeMode;

fn env_lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} 失败: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// 建一个带一次初始提交的仓库，并把 `LATTE_HOME` 指到 home tempdir。
/// 返回 (guard, repo_dir, home_dir)。
fn fixture() -> (
    std::sync::MutexGuard<'static, ()>,
    tempfile::TempDir,
    tempfile::TempDir,
) {
    let guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let repo = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("LATTE_HOME", home.path());
    git(repo.path(), &["init", "-q", "-b", "main"]);
    git(repo.path(), &["config", "user.email", "t@local"]);
    git(repo.path(), &["config", "user.name", "T"]);
    std::fs::write(repo.path().join("seed.txt"), "seed\n").unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-q", "-m", "init"]);
    (guard, repo, home)
}

fn head(repo: &Path) -> String {
    git(repo, &["rev-parse", "HEAD"])
}

#[test]
fn create_places_tree_outside_repo_and_links_state_dir() {
    let (_g, repo, home) = fixture();
    // 主仓的 .latte 里放一份"状态"，用来验证软链指对了地方。
    std::fs::create_dir_all(repo.path().join(".latte/tasks")).unwrap();
    std::fs::write(repo.path().join(".latte/tasks/board.json"), "{}").unwrap();

    let wt = TaskWorktree::open_or_create(repo.path(), "LAT-100", None).unwrap();

    assert!(wt.root().join("seed.txt").exists(), "工作树应有仓库内容");
    assert!(
        wt.root().starts_with(home.path()),
        "任务树必须落在 LATTE_HOME 下（仓库外）：{}",
        wt.root().display()
    );
    assert!(!wt.root().starts_with(repo.path()), "任务树不能落在仓库内");
    assert_eq!(wt.info().branch, "latte/task/LAT-100");
    assert_eq!(wt.info().base_branch, "main");
    assert_eq!(wt.info().base_sha, head(repo.path()));

    // .latte 是软链且指向主仓：项目配置与运行时状态必须与主仓同一份。
    let link = wt.root().join(".latte");
    assert!(
        link.symlink_metadata().unwrap().file_type().is_symlink(),
        ".latte 应是软链"
    );
    assert_eq!(
        std::fs::read_to_string(link.join("tasks/board.json")).unwrap(),
        "{}",
        "经软链应读到主仓的看板数据"
    );

    // worktree 已登记在主仓。
    assert!(git(repo.path(), &["worktree", "list"]).contains("LAT-100"));
}

#[test]
fn two_tasks_get_isolated_trees() {
    let (_g, repo, _home) = fixture();
    let a = TaskWorktree::open_or_create(repo.path(), "LAT-100", None).unwrap();
    let b = TaskWorktree::open_or_create(repo.path(), "LAT-101", None).unwrap();
    assert_ne!(a.root(), b.root());

    // 两个任务同时改**同一个文件**：这正是 v1 必须串行的场景。
    std::fs::write(a.root().join("seed.txt"), "from A\n").unwrap();
    std::fs::write(b.root().join("seed.txt"), "from B\n").unwrap();

    assert_eq!(
        std::fs::read_to_string(a.root().join("seed.txt")).unwrap(),
        "from A\n"
    );
    assert_eq!(
        std::fs::read_to_string(b.root().join("seed.txt")).unwrap(),
        "from B\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("seed.txt")).unwrap(),
        "seed\n",
        "主仓在合并前不受影响"
    );
}

#[test]
fn merge_back_lands_changes_on_base_branch() {
    let (_g, repo, _home) = fixture();
    let wt = TaskWorktree::open_or_create(repo.path(), "LAT-100", None).unwrap();
    assert!(!wt.has_changes().unwrap(), "刚建的树没有改动");

    std::fs::write(wt.root().join("new.txt"), "hello\n").unwrap();
    assert!(wt.has_changes().unwrap(), "未提交的改动也算改动");

    let before = head(repo.path());
    let merge_commit = wt.merge_back(MergeMode::NoFf).unwrap();
    assert_ne!(merge_commit, before, "应产生合并提交");
    assert_eq!(
        std::fs::read_to_string(repo.path().join("new.txt")).unwrap(),
        "hello\n",
        "改动应出现在主仓工作区"
    );
    assert!(git(repo.path(), &["log", "--oneline"]).contains("LAT-100"));
    // 合并后树里已无未合并改动。
    assert!(!wt.has_changes().unwrap());
}

#[test]
fn merge_back_is_noop_without_changes() {
    let (_g, repo, _home) = fixture();
    let wt = TaskWorktree::open_or_create(repo.path(), "LAT-100", None).unwrap();
    let before = head(repo.path());
    let after = wt.merge_back(MergeMode::NoFf).unwrap();
    assert_eq!(before, after, "无改动不该造空合并提交");
    assert_eq!(git(repo.path(), &["log", "--oneline"]).lines().count(), 1);
}

#[test]
fn merge_conflict_aborts_cleanly_and_preserves_the_tree() {
    let (_g, repo, _home) = fixture();
    let wt = TaskWorktree::open_or_create(repo.path(), "LAT-100", None).unwrap();

    // 任务树改 seed.txt……
    std::fs::write(wt.root().join("seed.txt"), "task side\n").unwrap();
    // ……主仓同一行也改了并提交（制造真冲突）。
    std::fs::write(repo.path().join("seed.txt"), "main side\n").unwrap();
    git(repo.path(), &["commit", "-aqm", "main edit"]);
    let before = head(repo.path());

    let err = wt.merge_back(MergeMode::NoFf).expect_err("应报冲突");
    let msg = err.to_string();
    // 断言不依赖 git 的语言环境（用户的 git 可能是中文/英文/其他 locale）：
    // 冲突文件名在任何 locale 下都会出现，而它正是用户最需要看到的信息。
    assert!(
        msg.contains("seed.txt"),
        "错误必须点名冲突文件（冲突详情在 git 的 stdout 里，只读 stderr 会得到空消息）：{msg}"
    );

    // 主仓复位：HEAD 没动、也没卡在半合并状态。
    assert_eq!(head(repo.path()), before, "冲突后 base 分支不该被推进");
    assert!(
        !repo.path().join(".git/MERGE_HEAD").exists(),
        "必须 merge --abort 复位，不能把主仓留在半合并状态"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("seed.txt")).unwrap(),
        "main side\n"
    );

    // 证据保留：树还在，任务分支上的提交还能拿到。
    assert!(wt.root().exists(), "冲突时必须保留工作树给人工处置");
    assert_eq!(
        git(wt.root(), &["show", "-s", "--format=%s", "HEAD"]),
        "task(LAT-100): auto-commit before merge"
    );
}

#[test]
fn cleanup_removes_tree_and_branch_but_never_touches_repo_state() {
    let (_g, repo, _home) = fixture();
    // 主仓 .latte 里放两份"状态"：会话日志 + 看板。
    std::fs::create_dir_all(repo.path().join(".latte/ui-sessions")).unwrap();
    std::fs::write(repo.path().join(".latte/ui-sessions/s.jsonl"), "line\n").unwrap();
    std::fs::create_dir_all(repo.path().join(".latte/tasks")).unwrap();
    std::fs::write(repo.path().join(".latte/tasks/LAT-100.json"), "{}").unwrap();

    let wt = TaskWorktree::open_or_create(repo.path(), "LAT-100", None).unwrap();
    let root: PathBuf = wt.root().to_path_buf();
    // 经软链往"状态目录"写一笔（模拟隔离会话落盘会话日志）。
    std::fs::write(root.join(".latte/ui-sessions/iso.jsonl"), "iso\n").unwrap();
    assert!(
        repo.path().join(".latte/ui-sessions/iso.jsonl").exists(),
        "软链写入应落到主仓"
    );

    wt.cleanup(false).unwrap();

    assert!(!root.exists(), "工作树目录应被删除");
    assert!(
        !git(repo.path(), &["branch", "--list", "latte/task/LAT-100"]).contains("LAT-100"),
        "任务分支应被删除"
    );
    // 关键回归：删树不能跟着软链把主仓状态删了。
    assert_eq!(
        std::fs::read_to_string(repo.path().join(".latte/ui-sessions/s.jsonl")).unwrap(),
        "line\n",
        "cleanup 绝不能删主仓的 .latte（软链只删链本身）"
    );
    assert!(repo.path().join(".latte/ui-sessions/iso.jsonl").exists());
    assert!(repo.path().join(".latte/tasks/LAT-100.json").exists());
}

#[test]
fn cleanup_can_keep_the_branch_for_manual_conflict_handling() {
    let (_g, repo, _home) = fixture();
    let wt = TaskWorktree::open_or_create(repo.path(), "LAT-100", None).unwrap();
    std::fs::write(wt.root().join("new.txt"), "x\n").unwrap();
    git(wt.root(), &["add", "-A"]);
    git(wt.root(), &["commit", "-qm", "work"]);

    wt.cleanup(true).unwrap();
    assert!(!wt.root().exists());
    assert!(
        git(repo.path(), &["branch", "--list", "latte/task/LAT-100"]).contains("LAT-100"),
        "keep_branch=true 时分支必须留着——它是那些提交的唯一入口"
    );
}

/// 留分支时**未提交**的改动也必须留住：agent 通常不自己 commit，
/// 直接删树会把它们销毁，分支保住了却空无一物。
///
/// 回归的是实测踩到的坑：取消一个任务后，agent 改的内容彻底消失。
#[test]
fn cleanup_keeping_branch_commits_uncommitted_work_first() {
    let (_g, repo, _home) = fixture();
    let wt = TaskWorktree::open_or_create(repo.path(), "LAT-100", None).unwrap();
    // 只改不提交（这就是 agent 的常态）+ 一个未跟踪的新文件。
    std::fs::write(wt.root().join("seed.txt"), "agent 改过\n").unwrap();
    std::fs::write(wt.root().join("added.txt"), "new\n").unwrap();

    wt.cleanup(true).unwrap();

    assert!(!wt.root().exists(), "树应被回收（磁盘要还）");
    assert_eq!(
        git(repo.path(), &["show", "latte/task/LAT-100:seed.txt"]),
        "agent 改过",
        "未提交的改动必须先落到分支上，否则「保留分支」等于空承诺"
    );
    assert_eq!(
        git(repo.path(), &["show", "latte/task/LAT-100:added.txt"]),
        "new"
    );
    assert!(git(
        repo.path(),
        &["log", "--format=%s", "-1", "latte/task/LAT-100"]
    )
    .contains("WIP"));
    // 主仓不受影响。
    assert_eq!(
        std::fs::read_to_string(repo.path().join("seed.txt")).unwrap(),
        "seed\n"
    );
}

/// `keep_branch=false`（合并成功后的回收）不需要保命提交——改动已经在 base 里。
#[test]
fn cleanup_without_keeping_branch_does_not_commit() {
    let (_g, repo, _home) = fixture();
    let wt = TaskWorktree::open_or_create(repo.path(), "LAT-100", None).unwrap();
    std::fs::write(wt.root().join("x.txt"), "x\n").unwrap();
    wt.cleanup(false).unwrap();
    assert!(!wt.root().exists());
    assert!(
        !git(repo.path(), &["branch", "--list", "latte/task/LAT-100"]).contains("LAT-100"),
        "分支应被删除"
    );
}

#[test]
fn open_or_create_reuses_an_existing_tree_and_revives_a_leftover_branch() {
    let (_g, repo, _home) = fixture();
    let first = TaskWorktree::open_or_create(repo.path(), "LAT-100", None).unwrap();
    std::fs::write(first.root().join("wip.txt"), "wip\n").unwrap();

    // 复用：rework / 重新派发同一个任务不该丢掉上次的进展。
    let again = TaskWorktree::open_or_create(repo.path(), "LAT-100", None).unwrap();
    assert_eq!(again.root(), first.root());
    assert_eq!(
        std::fs::read_to_string(again.root().join("wip.txt")).unwrap(),
        "wip\n",
        "复用已有工作树时不能清掉未提交的进展"
    );

    // 只删树、留分支（冲突后的状态），再 open 应挂回同一个分支而不是新建。
    git(again.root(), &["add", "-A"]);
    git(again.root(), &["commit", "-qm", "wip commit"]);
    let sha = git(again.root(), &["rev-parse", "HEAD"]);
    again.cleanup(true).unwrap();
    let third = TaskWorktree::open_or_create(repo.path(), "LAT-100", None).unwrap();
    assert_eq!(
        git(third.root(), &["rev-parse", "HEAD"]),
        sha,
        "分支还在时应重挂到该分支，而不是从 base 重新拉一条"
    );
}

#[test]
fn state_link_never_reaches_the_task_branch() {
    let (_g, repo, _home) = fixture();
    // 这个仓库**跟踪** .latte 下的文件（latte 自身的仓库就是这样：角色/流程
    // 配置是源码）。此时绝不能往 info/exclude 写 `/.latte`——那会让整棵
    // .latte 对 git 消失，新增角色配置从此静默不进版本控制。
    std::fs::create_dir_all(repo.path().join(".latte/agents")).unwrap();
    std::fs::write(
        repo.path().join(".latte/agents/manager.toml"),
        "id='manager'\n",
    )
    .unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-qm", "track role config"]);
    let exclude = repo.path().join(".git/info/exclude");
    let before = std::fs::read_to_string(&exclude).unwrap_or_default();

    let wt = TaskWorktree::open_or_create(repo.path(), "LAT-100", None).unwrap();
    assert_eq!(
        std::fs::read_to_string(&exclude).unwrap_or_default(),
        before,
        "仓库跟踪 .latte 下的文件时，绝不能改 info/exclude"
    );

    // 即便 .latte 对 git 可见，合并也不能把这个指向主仓的软链带进历史。
    std::fs::write(wt.root().join("work.txt"), "w\n").unwrap();
    wt.merge_back(MergeMode::NoFf).unwrap();
    let tracked = git(repo.path(), &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(tracked.contains("work.txt"), "任务产出应被合并：{tracked}");
    assert!(
        !tracked.lines().any(|l| l == ".latte"),
        "指向主仓的 .latte 软链绝不能进版本历史：{tracked}"
    );
    assert!(
        tracked.contains(".latte/agents/manager.toml"),
        "原本跟踪的 .latte 内容不该被弄丢：{tracked}"
    );
}

#[test]
fn state_dir_gets_locally_excluded_when_repo_does_not_track_it() {
    let (_g, repo, _home) = fixture();
    // 仓库既没忽略 .latte、也没跟踪它 → 写一条本地 exclude，让它彻底不出现
    // 在 git status 里（否则 agent 自己 `git add -A` 就会把软链提交掉）。
    let _wt = TaskWorktree::open_or_create(repo.path(), "LAT-100", None).unwrap();
    let exclude =
        std::fs::read_to_string(repo.path().join(".git/info/exclude")).unwrap_or_default();
    assert!(exclude.contains("/.latte"), "应追加本地 exclude：{exclude}");
    assert!(exclude.contains("latte:"), "应带溯源标记：{exclude}");
    assert!(git(repo.path(), &["check-ignore", "-q", ".latte"]).is_empty());

    // 幂等：再建一次不追加第二遍。
    let _again = TaskWorktree::open_or_create(repo.path(), "LAT-101", None).unwrap();
    let after = std::fs::read_to_string(repo.path().join(".git/info/exclude")).unwrap();
    assert_eq!(
        after.matches("/.latte").count(),
        1,
        "重复调用不得追加第二遍"
    );
}

#[test]
fn diff_summary_covers_committed_uncommitted_and_untracked() {
    let (_g, repo, _home) = fixture();
    let wt = TaskWorktree::open_or_create(repo.path(), "LAT-100", None).unwrap();

    // 空树：什么都没有。
    let empty = wt.diff_summary(50).unwrap();
    assert_eq!(empty.file_count, 0);
    assert!(empty.commits.is_empty());
    assert_eq!(empty.base, "main");

    // ① 已提交的改动
    std::fs::write(wt.root().join("seed.txt"), "seed\nplus one\n").unwrap();
    git(wt.root(), &["add", "-A"]);
    git(wt.root(), &["commit", "-qm", "extend seed"]);
    // ② 未提交的改动（agent 未必自己 commit，merge_back 会替它提交）
    std::fs::write(wt.root().join("seed.txt"), "seed\nplus one\nplus two\n").unwrap();
    // ③ 未跟踪的新文件（`diff` 看不到，但 `add -A` 会收下）
    std::fs::write(wt.root().join("brand-new.txt"), "new\n").unwrap();

    let d = wt.diff_summary(50).unwrap();
    let paths: Vec<&str> = d.files.iter().map(|f| f.path.as_str()).collect();
    assert!(paths.contains(&"seed.txt"), "{paths:?}");
    assert!(
        paths.contains(&"brand-new.txt"),
        "未跟踪的新文件必须列出，否则会「预览没有、合并却进去了」：{paths:?}"
    );
    assert_eq!(d.file_count, 2);
    assert_eq!(d.total_added, 2, "两次追加共 2 行");
    assert_eq!(d.total_removed, 0);
    assert_eq!(d.commits.len(), 1);
    assert_eq!(d.commits[0].subject, "extend seed");
    assert_eq!(d.branch, "latte/task/LAT-100");
}

#[test]
fn diff_summary_truncates_file_list_but_reports_the_real_count() {
    let (_g, repo, _home) = fixture();
    let wt = TaskWorktree::open_or_create(repo.path(), "LAT-100", None).unwrap();
    for i in 0..7 {
        std::fs::write(wt.root().join(format!("f{i}.txt")), "x\n").unwrap();
    }
    let d = wt.diff_summary(3).unwrap();
    assert_eq!(d.files.len(), 3, "列表按 max_files 截断");
    assert_eq!(
        d.file_count, 7,
        "但总数必须是真实值（UI 要显示「还有 N 个」）"
    );
}

/// 合并之后预览必须变空——与 `has_changes()` 同一个 base 口径，否则会出现
/// 「已经合并了，预览还说有改动」。
#[test]
fn diff_summary_is_empty_after_merge() {
    let (_g, repo, _home) = fixture();
    let wt = TaskWorktree::open_or_create(repo.path(), "LAT-100", None).unwrap();
    std::fs::write(wt.root().join("added.txt"), "x\n").unwrap();
    assert_eq!(wt.diff_summary(50).unwrap().file_count, 1);
    wt.merge_back(MergeMode::NoFf).unwrap();
    let d = wt.diff_summary(50).unwrap();
    assert_eq!(d.file_count, 0, "合并后不该再报改动");
    assert!(d.commits.is_empty());
    assert!(!wt.has_changes().unwrap());
}

#[test]
fn resolve_repo_root_reports_main_repo_from_inside_a_worktree() {
    let (_g, repo, _home) = fixture();
    let wt = TaskWorktree::open_or_create(repo.path(), "LAT-100", None).unwrap();
    let from_repo = TaskWorktree::resolve_repo_root(repo.path()).unwrap();
    let from_tree = TaskWorktree::resolve_repo_root(wt.root()).unwrap();
    assert_eq!(
        std::fs::canonicalize(&from_tree).unwrap(),
        std::fs::canonicalize(&from_repo).unwrap(),
        "从 worktree 里解析出来的必须是主仓根（否则会在树里再套一层树）"
    );
}

#[test]
fn resolve_repo_root_fails_outside_git() {
    let _g = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    assert!(
        TaskWorktree::resolve_repo_root(dir.path()).is_err(),
        "非 git 目录必须报错，让调用方降级为非隔离派发"
    );
}
