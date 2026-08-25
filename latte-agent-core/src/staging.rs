//! Workflow 文档暂存层（staging overlay）。
//!
//! 问题：长 workflow（如 design_and_plan）的中途步骤会把未终审的文档
//! 直接写进仓库最终位置——workflow 失败/内容有误时残留污染后续会话，
//! 下游角色还会把未核验的草稿当权威引用（信息偏差）。
//!
//! 方案：workflow toml 声明 `staging = true` 后，本次 run 内所有角色
//! 的 `write` 被重定向到 `<cwd>/.latte/staging/<wf_id>/`（按项目相对
//! 路径镜像），`read` 优先读暂存副本（overlay，下游能看到本 run 最新
//! 草稿而不是旧文件或空气）。workflow 整体 ok 时 [`Staging::promote`]
//! 把文件移到目标位置；失败/取消则保留暂存目录供人工检查，一行
//! `rm -rf` 可清理。
//!
//! `bash` 是**感知**而非拦截（见 [`Staging::wrap_tools`] 的 bash 分支）：
//! 无法可靠改写任意 shell 命令里的路径（`git log docs/a.md` 改成暂存路径
//! 就直接坏了），所以改为让它知情——注入 `LATTE_STAGING_ROOT` 环境变量，
//! 并在命令触碰到「有暂存草稿的路径」时在返回值里挂 `stagingNotice`，
//! 点名哪些文件的权威版本在暂存区。否则会出现很具体的错读：`read` 看到
//! 的是本 run 的草稿，`bash cat` 同一路径看到的却是仓库里的旧版本，
//! 两个通道对同一个文件给出不同答案，而模型无从察觉。
//!
//! 已知边界：`edit` / `doc_write` / `spawn` 等其它写通道仍不拦截；
//! 目录 listing 不合并暂存项。

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use latte_rs_agent_tools::error::ToolError;
use latte_rs_agent_tools::types::{SharedToolHandler, Tool, ToolManager};
use serde::Serialize;

/// 一条暂存写入记录。只进内存（promote 用），不落盘——暂存目录本身
/// 就是现场，manifest 文件反而是第二个真相源。
#[derive(Debug, Clone, Serialize)]
struct ManifestEntry {
    /// 归一化后的绝对目标路径（promote 的目的地）。
    dest_abs: String,
    /// 暂存文件绝对路径。
    staged: String,
    role: String,
    bytes: usize,
}

pub struct Staging {
    /// `<cwd>/.latte/staging/<wf_id>`。
    root: PathBuf,
    cwd: PathBuf,
    entries: parking_lot::Mutex<Vec<ManifestEntry>>,
}

impl Staging {
    pub fn new(cwd: &Path, wf_id: &str) -> Arc<Self> {
        let root = cwd.join(".latte").join("staging").join(wf_id);
        let _ = std::fs::create_dir_all(&root);
        Arc::new(Self {
            root,
            cwd: cwd.to_path_buf(),
            entries: parking_lot::Mutex::new(Vec::new()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 相对路径按 cwd 归一化为绝对路径。
    fn resolve(&self, path: &str) -> PathBuf {
        let p = PathBuf::from(path);
        if p.is_absolute() {
            p
        } else {
            self.cwd.join(p)
        }
    }

    /// 目标绝对路径 → 暂存镜像路径。项目内按相对路径镜像；项目外
    /// 路径镜像到 `_ext/` 下（剥掉根组件，避免写出暂存区）。
    fn staged_path(&self, dest_abs: &Path) -> PathBuf {
        if let Ok(rel) = dest_abs.strip_prefix(&self.cwd) {
            return self.root.join(rel);
        }
        let mut p = self.root.join("_ext");
        for c in dest_abs.components() {
            if let Component::Normal(s) = c {
                p.push(s);
            }
        }
        p
    }

    /// 把 tool manager 里的 write/read 换成暂存包装版（不存在则跳过）。
    /// 包装策略：重写 path 到暂存区后**调用原 handler**——fs 语义
    /// （createDirs/overwrite/行选择器）零重复，返回 json 里把 path
    /// 改回模型请求的原路径并附 `stagedAs`，模型可感知但不困惑。
    pub fn wrap_tools(self: &Arc<Self>, tm: &Arc<dyn ToolManager>, role_id: &str) {
        if let Some(orig) = tm.get_tool("write") {
            let st = Arc::clone(self);
            let orig_handler = orig.handler.clone();
            let role = role_id.to_string();
            let handler: SharedToolHandler = Arc::new(move |input, ctx| {
                let st = Arc::clone(&st);
                let orig_handler = orig_handler.clone();
                let role = role.clone();
                Box::pin(async move {
                    let path = input
                        .get("path")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| ToolError::other("path is required"))?;
                    let dest_abs = st.resolve(path);
                    let staged = st.staged_path(&dest_abs);
                    let mut rewritten = input.clone();
                    rewritten["path"] = serde_json::json!(staged.to_string_lossy());
                    let mut out = (orig_handler)(rewritten, ctx).await?;
                    let bytes = input
                        .get("content")
                        .and_then(|v| v.as_str())
                        .map(|s| s.len())
                        .unwrap_or(0);
                    st.entries.lock().push(ManifestEntry {
                        dest_abs: dest_abs.to_string_lossy().into_owned(),
                        staged: staged.to_string_lossy().into_owned(),
                        role,
                        bytes,
                    });
                    if let Some(obj) = out.as_object_mut() {
                        obj.insert("path".into(), serde_json::json!(path));
                        obj.insert(
                            "stagedAs".into(),
                            serde_json::json!(staged.to_string_lossy()),
                        );
                    }
                    Ok(out)
                })
            });
            let wrapped = Tool::builder(
                "write",
                format!(
                    "{}（workflow 暂存模式：先写草稿区，workflow 终审通过后自动落到目标路径）",
                    orig.description
                ),
                orig.input_schema.clone(),
                handler,
            )
            .concurrency_safe(orig.concurrency_safe)
            .build();
            tm.unregister("write");
            tm.register(wrapped, None);
        }

        if let Some(orig) = tm.get_tool("read") {
            let st = Arc::clone(self);
            let orig_handler = orig.handler.clone();
            let handler: SharedToolHandler = Arc::new(move |input, ctx| {
                let st = Arc::clone(&st);
                let orig_handler = orig_handler.clone();
                Box::pin(async move {
                    let path = input
                        .get("path")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| ToolError::other("path is required"))?;
                    let (file_part, selector) = split_path_selector(path);
                    let dest_abs = st.resolve(file_part);
                    let staged = st.staged_path(&dest_abs);
                    if !staged.is_file() {
                        // 暂存未命中：原样走真实 fs（目录 listing 同理不合并）。
                        return (orig_handler)(input, ctx).await;
                    }
                    let staged_str = staged.to_string_lossy().into_owned();
                    let rewritten_path = match selector {
                        Some(sel) => format!("{staged_str}:{sel}"),
                        None => staged_str,
                    };
                    let mut rewritten = input.clone();
                    rewritten["path"] = serde_json::json!(rewritten_path);
                    let mut out = (orig_handler)(rewritten, ctx).await?;
                    if let Some(obj) = out.as_object_mut() {
                        obj.insert("path".into(), serde_json::json!(path));
                        obj.insert(
                            "stagedFrom".into(),
                            serde_json::json!(staged.to_string_lossy()),
                        );
                    }
                    Ok(out)
                })
            });
            let wrapped = Tool::builder(
                "read",
                orig.description.clone(),
                orig.input_schema.clone(),
                handler,
            )
            .concurrency_safe(orig.concurrency_safe)
            .build();
            tm.unregister("read");
            tm.register(wrapped, None);
        }

        // ── bash：感知，不拦截 ────────────────────────────────────
        // 为什么不改写命令：bash 收的是任意 shell 字符串，把里面的路径
        // 替换成暂存路径会坏掉一大类命令（`git log docs/a.md`、
        // `cargo test`、相对路径拼接…）。所以给它三件事：
        //   1. `LATTE_STAGING_ROOT` / `LATTE_STAGING_ACTIVE` 环境变量
        //      —— 需要读写草稿的命令可以直接寻址；
        //   2. 描述里写清规则，模型知道 bash 写出去的东西**不会**进
        //      暂存区、也不会被 promote；
        //   3. 命令触碰到有草稿的路径时，返回值挂 `stagingNotice`
        //      点名它读到的可能是旧版本 —— 这是最容易踩的坑：`read`
        //      给草稿、`bash cat` 给旧文件，同一路径两个答案。
        if let Some(orig) = tm.get_tool("bash") {
            let st = Arc::clone(self);
            let orig_handler = orig.handler.clone();
            let handler: SharedToolHandler = Arc::new(move |input, ctx| {
                let st = Arc::clone(&st);
                let orig_handler = orig_handler.clone();
                Box::pin(async move {
                    let command = input
                        .get("command")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    // 注入 staging 环境变量（不覆盖调用方已给的同名项）。
                    let mut rewritten = input.clone();
                    let env = rewritten
                        .as_object_mut()
                        .map(|o| {
                            o.entry("env".to_string())
                                .or_insert_with(|| serde_json::json!({}))
                        })
                        .filter(|v| v.is_object());
                    if let Some(env) = env {
                        if let Some(obj) = env.as_object_mut() {
                            obj.entry("LATTE_STAGING_ROOT".to_string()).or_insert_with(
                                || serde_json::json!(st.root().to_string_lossy()),
                            );
                            obj.entry("LATTE_STAGING_ACTIVE".to_string())
                                .or_insert_with(|| serde_json::json!("1"));
                        }
                    }
                    let touched = st.staged_paths_in_command(&command);
                    let mut out = (orig_handler)(rewritten, ctx).await?;
                    if !touched.is_empty() {
                        let listed = touched
                            .iter()
                            .map(|(rel, staged)| format!("{rel} → {staged}"))
                            .collect::<Vec<_>>()
                            .join("; ");
                        if let Some(obj) = out.as_object_mut() {
                            obj.insert(
                                "stagingNotice".into(),
                                serde_json::json!(format!(
                                    "本 workflow 处于暂存模式，命令里这些路径有**更新的草稿**在暂存区，\
                                     仓库里的版本是旧的：{listed}。读请改用 read 工具（自动 overlay）\
                                     或直接读上面的暂存路径；写请用 write 工具——bash 直接写仓库\
                                     不会进暂存区、也不会被终审后的 promote 带上。"
                                )),
                            );
                        }
                    }
                    Ok(out)
                })
            });
            let wrapped = Tool::builder(
                "bash",
                format!(
                    "{}（workflow 暂存模式：草稿区在 $LATTE_STAGING_ROOT。\
                     产出文档请用 write 工具——bash 直接写仓库不进暂存区、\
                     终审 promote 也不会带上它。读已被本 run 改过的文件请用 read 工具。）",
                    orig.description
                ),
                orig.input_schema.clone(),
                handler,
            )
            .concurrency_safe(orig.concurrency_safe)
            .build();
            tm.unregister("bash");
            tm.register(wrapped, None);
        }
    }

    /// workflow 成功结束：把暂存文件移到目标位置（同一文件多次写取
    /// 最后一次）。返回 (已提升列表, 错误列表)；全部成功时清理暂存
    /// 目录，有错误则保留现场。
    pub fn promote(&self) -> (Vec<(String, usize)>, Vec<String>) {
        let entries = self.entries.lock().clone();
        // 去重：dest_abs → 最后一次写入。
        let mut last: std::collections::HashMap<String, (String, usize)> =
            std::collections::HashMap::new();
        let mut order: Vec<String> = Vec::new();
        for e in entries {
            if !last.contains_key(&e.dest_abs) {
                order.push(e.dest_abs.clone());
            }
            last.insert(e.dest_abs, (e.staged, e.bytes));
        }
        let mut promoted = Vec::new();
        let mut errors = Vec::new();
        for dest_abs in order {
            let (staged, bytes) = &last[&dest_abs];
            let dest = PathBuf::from(&dest_abs);
            let result = (|| -> std::io::Result<()> {
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                move_file(Path::new(staged), &dest)
            })();
            match result {
                Ok(()) => promoted.push((dest_abs, *bytes)),
                Err(e) => errors.push(format!("{dest_abs}: {e}")),
            }
        }
        if errors.is_empty() {
            let _ = std::fs::remove_dir_all(&self.root);
        }
        (promoted, errors)
    }

    /// 暂存区当前文件数（失败分支的提示文案用）。
    pub fn pending_count(&self) -> usize {
        self.entries.lock().len()
    }

    /// 当前有暂存草稿的目标路径（去重，绝对路径）。
    fn staged_dests(&self) -> Vec<(String, String)> {
        let mut seen: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        for e in self.entries.lock().iter() {
            seen.insert(e.dest_abs.clone(), e.staged.clone());
        }
        seen.into_iter().collect()
    }

    /// 一条 shell 命令里提到了哪些「有暂存草稿」的路径。
    ///
    /// 匹配绝对路径与项目相对路径两种写法，且要求命中处是**独立 token**
    /// （两侧是命令分隔符/引号/空白），避免 `a.md` 命中 `xa.md` 或
    /// `a.mdx` 这类前后缀误报。
    fn staged_paths_in_command(&self, command: &str) -> Vec<(String, String)> {
        let mut hits: Vec<(String, String)> = Vec::new();
        for (dest_abs, staged) in self.staged_dests() {
            let rel = Path::new(&dest_abs)
                .strip_prefix(&self.cwd)
                .ok()
                .map(|p| p.to_string_lossy().into_owned());
            let candidates: Vec<&str> = match &rel {
                Some(r) => vec![dest_abs.as_str(), r.as_str()],
                None => vec![dest_abs.as_str()],
            };
            if candidates
                .iter()
                .any(|needle| mentions_path_token(command, needle))
            {
                hits.push((rel.unwrap_or_else(|| dest_abs.clone()), staged));
            }
        }
        hits
    }
}

/// `needle` 是否作为独立路径 token 出现在 `haystack` 里。
///
/// 「独立」= 命中处左右不是路径字符（字母数字、`.`、`_`、`-`、`/`）。
/// 这样 `notes/a.md` 不会命中 `notes/a.mdx`，也不会命中 `old_notes/a.md`。
fn mentions_path_token(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let is_path_char = |c: char| c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | '/');
    let bytes = haystack.as_bytes();
    let mut from = 0usize;
    while let Some(rel) = haystack[from..].find(needle) {
        let start = from + rel;
        let end = start + needle.len();
        let before_ok = start == 0
            || !haystack[..start]
                .chars()
                .next_back()
                .map(is_path_char)
                .unwrap_or(false);
        let after_ok = end >= bytes.len()
            || !haystack[end..]
                .chars()
                .next()
                .map(is_path_char)
                .unwrap_or(false);
        if before_ok && after_ok {
            return true;
        }
        // 继续找下一个出现位置（按字符边界前进，避免切断 UTF-8）。
        from = end.min(haystack.len());
        while from < haystack.len() && !haystack.is_char_boundary(from) {
            from += 1;
        }
        if from >= haystack.len() {
            break;
        }
    }
    false
}

/// rename 优先，跨设备等失败回退 copy+remove。
fn move_file(src: &Path, dest: &Path) -> std::io::Result<()> {
    match std::fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(_) => {
            std::fs::copy(src, dest)?;
            std::fs::remove_file(src)
        }
    }
}

/// 复制 tools crate `file.rs::parse_path_selector` 的规则（该函数是
/// crate 私有的）：`:N-M`、`:N`、`:N+count`、`:raw`、`:conflicts`
/// 视为行选择器，`\:` 转义字面冒号。
fn split_path_selector(path: &str) -> (&str, Option<&str>) {
    if let Some(pos) = path.rfind(':') {
        let after_colon = &path[pos + 1..];
        if after_colon.starts_with('\\') {
            return (path, None);
        }
        let before = &path[..pos];
        if before.is_empty() {
            return (path, None);
        }
        if after_colon == "raw"
            || after_colon == "conflicts"
            || after_colon
                .chars()
                .all(|c| c.is_ascii_digit() || c == '-' || c == '+' || c == ',')
        {
            return (before, Some(after_colon));
        }
    }
    (path, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use latte_rs_agent_tools::prelude::*;
    use latte_rs_agent_tools::types::ToolExecutionContext;

    async fn make_tm() -> Arc<dyn ToolManager> {
        let mgr = create_tool_manager();
        for p in builtin_tool_packages() {
            mgr.register_package(p).await.unwrap();
        }
        mgr
    }

    fn ctx() -> Option<ToolExecutionContext> {
        Some(ToolExecutionContext::fresh("t", 0))
    }

    /// 带 cwd metadata 的 ctx——与 AgentRunner 生产路径一致（read 的
    /// 相对路径解析走 metadata.cwd，见 tools crate resolve_tool_path）。
    fn ctx_in(cwd: &Path) -> Option<ToolExecutionContext> {
        let mut c = ToolExecutionContext::fresh("t", 0);
        c.metadata = Some(serde_json::json!({"cwd": cwd.to_string_lossy()}));
        Some(c)
    }

    #[test]
    fn staged_path_mirrors_inside_and_outside_cwd() {
        let cwd = PathBuf::from("/proj");
        let st = Staging::new(&cwd, "wf-test");
        assert_eq!(
            st.staged_path(Path::new("/proj/lab/notes/a.md")),
            st.root().join("lab/notes/a.md")
        );
        assert_eq!(
            st.staged_path(Path::new("/tmp/x.md")),
            st.root().join("_ext/tmp/x.md")
        );
        let _ = std::fs::remove_dir_all(st.root());
    }

    #[tokio::test]
    async fn write_redirects_and_read_overlay_hits() {
        let dir = std::env::temp_dir().join(format!("staging-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let st = Staging::new(&dir, "wf-t1");
        let tm = make_tm().await;
        st.wrap_tools(&tm, "designer");

        // write 落进暂存区，真实位置不存在。
        tm.execute(
            "write",
            serde_json::json!({"path": "notes/plan.md", "content": "草稿v1"}),
            ctx(),
        )
        .await
        .unwrap();
        assert!(!dir.join("notes/plan.md").exists());
        assert!(st.root().join("notes/plan.md").exists());

        // read overlay：请求原路径，读到暂存内容。
        let out = tm
            .execute("read", serde_json::json!({"path": "notes/plan.md"}), ctx())
            .await
            .unwrap();
        assert_eq!(out["content"], "草稿v1");
        assert_eq!(out["path"], "notes/plan.md");
        assert!(out["stagedFrom"].is_string());

        // 行选择器在暂存副本上同样生效。
        tm.execute(
            "write",
            serde_json::json!({"path": "notes/plan.md", "content": "l1\nl2\nl3"}),
            ctx(),
        )
        .await
        .unwrap();
        let out = tm
            .execute("read", serde_json::json!({"path": "notes/plan.md:2-3"}), ctx())
            .await
            .unwrap();
        assert_eq!(out["content"], "l2\nl3");

        // 未命中 overlay：读真实 fs。
        std::fs::write(dir.join("real.md"), "真实文件").unwrap();
        let out = tm
            .execute("read", serde_json::json!({"path": "real.md"}), ctx_in(&dir))
            .await
            .unwrap();
        assert_eq!(out["content"], "真实文件");
        assert!(out.get("stagedFrom").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn promote_moves_last_write_and_cleans_up() {
        let dir = std::env::temp_dir().join(format!("staging-promote-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let st = Staging::new(&dir, "wf-t2");
        let tm = make_tm().await;
        st.wrap_tools(&tm, "devops");

        tm.execute(
            "write",
            serde_json::json!({"path": "a.md", "content": "v1"}),
            ctx(),
        )
        .await
        .unwrap();
        tm.execute(
            "write",
            serde_json::json!({"path": "a.md", "content": "v2-final"}),
            ctx(),
        )
        .await
        .unwrap();
        tm.execute(
            "write",
            serde_json::json!({"path": "sub/b.md", "content": "b"}),
            ctx(),
        )
        .await
        .unwrap();

        let (promoted, errors) = st.promote();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(promoted.len(), 2);
        assert_eq!(std::fs::read_to_string(dir.join("a.md")).unwrap(), "v2-final");
        assert_eq!(std::fs::read_to_string(dir.join("sub/b.md")).unwrap(), "b");
        // 全部成功 → 暂存目录已清理。
        assert!(!st.root().exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 独立 token 匹配：前后缀不能误报，否则 stagingNotice 会满屏乱挂。
    #[test]
    fn path_token_matching_avoids_prefix_and_suffix_false_positives() {
        assert!(mentions_path_token("cat notes/a.md", "notes/a.md"));
        assert!(mentions_path_token("wc -l 'notes/a.md'", "notes/a.md"));
        assert!(mentions_path_token("cat notes/a.md | head", "notes/a.md"));
        assert!(mentions_path_token("notes/a.md", "notes/a.md"));
        // 后缀更长 → 不算命中。
        assert!(!mentions_path_token("cat notes/a.mdx", "notes/a.md"));
        // 前缀更长 → 不算命中。
        assert!(!mentions_path_token("cat old_notes/a.md", "notes/a.md"));
        assert!(!mentions_path_token("cat xnotes/a.md", "notes/a.md"));
        // 第一处是误报、第二处是真命中 → 仍应命中。
        assert!(mentions_path_token(
            "cat notes/a.mdx && cat notes/a.md",
            "notes/a.md"
        ));
        assert!(!mentions_path_token("anything", ""));
        // 非 ASCII 不能把 UTF-8 切断（走 char 边界前进）。
        assert!(mentions_path_token("cat 文档/说明.md", "文档/说明.md"));
        assert!(!mentions_path_token("cat 文档/说明.mdx", "文档/说明.md"));
    }

    /// bash 感知 staging：注入 LATTE_STAGING_ROOT，且命令碰到有草稿的
    /// 路径时挂 stagingNotice。
    ///
    /// 回归动机：此前 bash 完全不知道 staging 存在 —— `read` 给的是本
    /// run 的草稿，`bash cat` 同一路径给的是仓库旧版本，两个通道对同一
    /// 文件给出不同答案，模型无从察觉。
    #[tokio::test]
    async fn bash_sees_staging_env_and_gets_notice_for_staged_paths() {
        let dir = std::env::temp_dir().join(format!("staging-bash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 仓库里已有旧版本，暂存区有新草稿 —— 正是会读错的场景。
        std::fs::create_dir_all(dir.join("notes")).unwrap();
        std::fs::write(dir.join("notes/plan.md"), "仓库里的旧版本").unwrap();

        let st = Staging::new(&dir, "wf-bash");
        let tm = make_tm().await;
        st.wrap_tools(&tm, "programmer");

        // 1) 环境变量注入：命令能读到 LATTE_STAGING_ROOT。
        let out = tm
            .execute(
                "bash",
                serde_json::json!({
                    "command": "printf %s \"$LATTE_STAGING_ROOT|$LATTE_STAGING_ACTIVE\"",
                    "cwd": dir.to_string_lossy(),
                }),
                ctx(),
            )
            .await
            .expect("bash ok");
        let stdout = out["stdout"].as_str().unwrap_or_default();
        assert_eq!(
            stdout,
            format!("{}|1", st.root().to_string_lossy()),
            "bash 必须能看到暂存区位置: {stdout}"
        );
        // 没碰到草稿路径 → 不挂通知（避免噪音）。
        assert!(out.get("stagingNotice").is_none());

        // 2) 写一份草稿，然后 bash 去读同一路径。
        tm.execute(
            "write",
            serde_json::json!({"path": "notes/plan.md", "content": "草稿新版本"}),
            ctx(),
        )
        .await
        .unwrap();

        let out = tm
            .execute(
                "bash",
                serde_json::json!({
                    "command": "cat notes/plan.md",
                    "cwd": dir.to_string_lossy(),
                }),
                ctx(),
            )
            .await
            .expect("bash ok");
        // bash 如实读到仓库旧版本（我们不改写命令）——
        assert_eq!(out["stdout"].as_str().unwrap_or_default(), "仓库里的旧版本");
        // ——但必须明确告知权威版本在暂存区，否则模型会拿旧内容当真。
        let notice = out["stagingNotice"]
            .as_str()
            .expect("碰到有草稿的路径必须挂 stagingNotice");
        assert!(notice.contains("notes/plan.md"), "{notice}");
        assert!(
            notice.contains(&st.root().join("notes/plan.md").to_string_lossy().to_string()),
            "通知要给出暂存路径，模型才能直接去读: {notice}"
        );

        // 3) 不相关的命令不挂通知。
        let out = tm
            .execute(
                "bash",
                serde_json::json!({"command": "echo hi", "cwd": dir.to_string_lossy()}),
                ctx(),
            )
            .await
            .unwrap();
        assert!(out.get("stagingNotice").is_none(), "无关命令不该有噪音");

        // 4) 调用方自带 env 不被覆盖。
        let out = tm
            .execute(
                "bash",
                serde_json::json!({
                    "command": "printf %s \"$LATTE_STAGING_ROOT\"",
                    "cwd": dir.to_string_lossy(),
                    "env": {"LATTE_STAGING_ROOT": "/caller/wins"},
                }),
                ctx(),
            )
            .await
            .unwrap();
        assert_eq!(out["stdout"].as_str().unwrap_or_default(), "/caller/wins");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 描述里必须写清 bash 与 staging 的关系 —— 模型只读描述。
    #[tokio::test]
    async fn bash_description_states_the_staging_rule() {
        let dir = std::env::temp_dir().join(format!("staging-bashdesc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let st = Staging::new(&dir, "wf-bashdesc");
        let tm = make_tm().await;
        st.wrap_tools(&tm, "programmer");
        let desc = tm.get_tool("bash").expect("bash 已注册").description;
        assert!(desc.contains("LATTE_STAGING_ROOT"), "{desc}");
        assert!(desc.contains("write"), "要指明产文档该用 write: {desc}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn selector_split_matches_tools_crate_rules() {
        assert_eq!(split_path_selector("a.md"), ("a.md", None));
        assert_eq!(split_path_selector("a.md:2-5"), ("a.md", Some("2-5")));
        assert_eq!(split_path_selector("a.md:raw"), ("a.md", Some("raw")));
        assert_eq!(split_path_selector("a.md:3+10"), ("a.md", Some("3+10")));
        assert_eq!(split_path_selector("a:name.md"), ("a:name.md", None));
    }
}
