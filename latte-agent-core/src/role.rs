//! Role system: role definitions, categories, and built-in role constructors.
//!
//! Mirrors gsd-core's agent definition pattern: each role has an identity (system prompt),
//! a default model tier, and allowed tools.

use latte_ai::params::GenerateParams;
use serde::{Deserialize, Serialize};

/// Classification of a role in the development lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleCategory {
    /// Planning roles: PM, Architect, Manager.
    Planning,
    /// Execution roles: Programmer, DevOps, Tech Writer.
    Execution,
    /// Verification roles: Tester, Reviewer, Security.
    Verification,
    /// General discussion / all-purpose.
    Discussion,
}

impl RoleCategory {
    /// Parse from string (case-insensitive, snake_case or kebab-case).
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().replace('-', "_").as_str() {
            "planning" => Some(Self::Planning),
            "execution" => Some(Self::Execution),
            "verification" => Some(Self::Verification),
            "discussion" => Some(Self::Discussion),
            _ => None,
        }
    }
}

/// A role template — defines the identity, behavior, and defaults for an agent role.
///
/// Loaded from TOML config + markdown prompt files. Supports `{{variable}}` template
/// substitution via handlebars.
#[derive(Debug, Clone)]
pub struct Role {
    /// Unique identifier (e.g. "pm", "programmer", "tester").
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Lifecycle category.
    pub category: RoleCategory,
    /// System prompt template (handlebars). Rendered with context variables.
    pub system_prompt: String,
    /// Default model tier for this role (resolves to the primary model).
    pub default_model_tier: crate::model_resolver::ModelTier,
    /// Fallback model chain, in priority order (highest first).
    ///
    /// When the primary model (resolved from `default_model_tier`) is
    /// unavailable (rate-limited, 5xx, etc.), the agent walks this list and
    /// tries each model in order. Empty = no fallback beyond the primary.
    pub model_chain: Vec<String>,
    /// Default generation params (temperature, top_p, etc.).
    pub default_params: GenerateParams,
    /// Tools this role typically uses (tool names).
    pub allowed_tools: Vec<String>,
    /// Display icon/emoji.
    pub icon: String,
}

impl Role {
    /// Render the system prompt with the given context variables.
    pub fn render_prompt(
        &self,
        vars: &serde_json::Value,
    ) -> Result<String, crate::error::AgentError> {
        let reg = new_handlebars();
        reg.render_template(&self.system_prompt, vars)
            .map_err(|e| crate::error::AgentError::Template(self.id.clone(), e))
    }

    /// Render the system prompt with string variables.
    pub fn render_prompt_str(
        &self,
        vars: &std::collections::HashMap<String, String>,
    ) -> Result<String, crate::error::AgentError> {
        let json = serde_json::to_value(vars).unwrap_or_default();
        self.render_prompt(&json)
    }
}

/// Lightweight, serializable role template for TOML config.
///
/// Used as an intermediate step: loaded from TOML, then converted to `Role`
/// by resolving the prompt file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleTemplate {
    pub id: String,
    pub name: String,
    pub category: String,
    pub model_tier: String,
    /// Fallback model chain in priority order (highest priority first).
    /// TOML example: `model_chain = ["gpt-4o", "deepseek-chat", "claude-3-haiku"]`.
    #[serde(default)]
    pub model_chain: Vec<String>,
    pub prompt_file: Option<String>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub icon: String,
    /// Additional skill files (*.md) loaded on top of the main prompt.
    /// Each skill file is appended to the system prompt with a heading.
    /// Example: `skills = ["screenshot_skill"]` loads `prompts/screenshot_skill.md`.
    #[serde(default)]
    pub skills: Vec<String>,
    /// 领域代码/文档路径（相对工作目录或绝对路径）。角色实例化时
    /// **确定性注入**系统提示：文件直接内联内容（≤ 8KB），目录注入
    /// 树状清单 —— 不依赖模型自觉去读。角色编辑器可编辑。
    #[serde(default)]
    pub code_paths: Vec<String>,
}

impl RoleTemplate {
    /// Resolve this template into a full `Role` by loading the prompt file.
    pub async fn resolve(
        &self,
        default_params: &GenerateParams,
    ) -> Result<Role, crate::error::AgentError> {
        let category = RoleCategory::parse(&self.category)
            .ok_or_else(|| {
                crate::error::AgentError::Config(format!(
                    "unknown role category '{}' for role '{}'",
                    self.category, self.id
                ))
            })?;

        let model_tier = crate::model_resolver::ModelTier::parse(&self.model_tier)?;

        let mut params = default_params.clone();
        if let Some(t) = self.temperature {
            params.temperature = Some(t);
        }

        let system_prompt = match &self.prompt_file {
            Some(path) => match tokio::fs::read_to_string(path).await {
                Ok(content) => content,
                Err(primary_err) => {
                    // Absolute paths, `~`-prefixed paths, and paths
                    // with `..` are deliberate user intent — we
                    // still try the global `prompts.d/` override,
                    // but we do NOT fall back to the binary's
                    // built-in prompts (the user clearly meant
                    // *this* file, not a default).
                    let skip_global =
                        path.starts_with('/') || path.starts_with('~') || path.contains("..");
                    if skip_global {
                        return Err(crate::error::AgentError::Config(format!(
                            "cannot read prompt file '{}' for role '{}': {}",
                            path, self.id, primary_err
                        )));
                    }
                    match resolve_global_prompt(path, &self.id).await {
                        Some(content) => content,
                        None => match crate::prompts::for_role(&self.id) {
                            // Built-in: compiled into the binary at
                            // build time via `include_str!`. Always
                            // available, no filesystem dependency.
                            // This is what makes `latte-agent chat`
                            // work out-of-the-box on a fresh checkout
                            // (no project config, no global
                            // overrides, just the binary).
                            Some(content) => content.to_string(),
                            None => {
                                return Err(crate::error::AgentError::Config(format!(
                                    "cannot read prompt file '{}' for role '{}': {}",
                                    path, self.id, primary_err
                                )));
                            }
                        },
                    }
                }
            },
            None => match crate::prompts::for_role(&self.id) {
                Some(content) => content.to_string(),
                None => format!(
                    "You are a {}. Respond in character as a {}.\nYour task: {{topic}}",
                    self.name, self.name
                ),
            },
        };

        // Load skills: append each skill file content to system prompt
        let mut system_prompt = system_prompt;
        for skill_name in &self.skills {
            let skill_paths = [
                format!("prompts/{skill_name}.md"),
                format!("{}/prompts/{skill_name}.md", std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default()),
            ];
            let skill_content = skill_paths.iter()
                .find_map(|p| std::fs::read_to_string(p).ok())
                .or_else(|| crate::prompts::for_skill(skill_name).map(String::from));
        }

        // code_paths：领域代码/文档资料，实例化时**确定性注入**——
        // 文件直接内联内容、目录注入树状清单，不依赖模型自觉去读。
        // 预算：单文件 ≤ 8KB（超出截断）、目录树深度 ≤ 3 / ≤ 200 条、
        // 总量 ≤ 32KB。深入阅读仍靠 read/search 工具（prompt 装不下
        // 整个代码库，这部分结构上绕不开）。
        if !self.code_paths.is_empty() {
            system_prompt.push_str("\n\n### 你的领域代码/文档资料\n\n以下内容来自你配置的领域路径，已直接载入，无需再读取；目录类路径只列出结构，文件内容用 read 工具按需查看：\n");
            let mut budget: usize = 32 * 1024;
            for p in &self.code_paths {
                if budget == 0 {
                    system_prompt.push_str("\n（已达注入上限，其余路径省略；可用工具自行查看）\n");
                    break;
                }
                let rendered = render_code_path(p, budget);
                budget -= rendered.len();
                system_prompt.push_str(&rendered);
            }
        }

        Ok(Role {
            id: self.id.clone(),
            name: self.name.clone(),
            category,
            system_prompt,
            default_model_tier: model_tier,
            model_chain: self.model_chain.clone(),
            default_params: params,
            allowed_tools: self.tools.clone(),
            icon: if self.icon.is_empty() {
                default_icon(&self.id)
            } else {
                self.icon.clone()
            },
        })
    }
}

fn default_icon(role_id: &str) -> String {
    match role_id {
        "pm" => "📋".into(),
        "architect" => "🏗️".into(),
        "programmer" => "💻".into(),
        "tester" => "🧪".into(),
        "reviewer" => "🔍".into(),
        "devops" => "🚀".into(),
        "security" => "🛡️".into(),
        "designer" => "🎨".into(),
        "tech_writer" => "📝".into(),
        "manager" => "👔".into(),
        _ => "🤖".into(),
    }
}

// ─── code_paths 确定性注入 ────────────────────────────────────────

/// 单文件内联上限（字节）。
const CODE_PATH_FILE_CAP: usize = 8 * 1024;
/// 目录树最大条目数。
const CODE_PATH_TREE_CAP: usize = 200;
/// 目录树最大深度。
const CODE_PATH_TREE_DEPTH: usize = 3;
/// 目录遍历时跳过的目录名。
const CODE_PATH_SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", "__pycache__"];

/// 把一条 code path 渲染成注入文本（长度 ≤ budget）：
/// - 文件 → 内联内容（≤ 8KB，超出截断标注）
/// - 目录 → 树状清单（深度 ≤ 3、≤ 200 条，跳过 .git/target 等）
/// - 不存在 → 明确标注（确定性反馈，不静默）
fn render_code_path(path: &str, budget: usize) -> String {
    let p = std::path::Path::new(path);
    let body = match std::fs::metadata(p) {
        Err(_) => format!("\n#### `{path}`\n\n（路径不存在——请检查角色配置）\n"),
        Ok(m) if m.is_file() => render_code_file(p, path),
        Ok(_) => render_code_dir(p, path),
    };
    if body.len() > budget {
        let mut cut: String = body.chars().take(budget).collect();
        cut.push_str("\n…（超出注入预算，截断）\n");
        cut
    } else {
        body
    }
}

fn render_code_file(p: &std::path::Path, display: &str) -> String {
    match std::fs::read(p) {
        Err(e) => format!("\n#### `{display}`\n\n（读取失败：{e}）\n"),
        Ok(bytes) => {
            let (slice, truncated) = if bytes.len() > CODE_PATH_FILE_CAP {
                (&bytes[..CODE_PATH_FILE_CAP], true)
            } else {
                (&bytes[..], false)
            };
            let text = String::from_utf8_lossy(slice);
            let note = if truncated { "\n…（超过 8KB，截断）" } else { "" };
            format!("\n#### `{display}`\n\n```\n{text}{note}\n```\n")
        }
    }
}

fn render_code_dir(p: &std::path::Path, display: &str) -> String {
    let mut entries: Vec<String> = Vec::new();
    walk_code_dir(p, p, 0, &mut entries);
    if entries.is_empty() {
        return format!("\n#### `{display}/`\n\n（空目录或全部条目被过滤）\n");
    }
    entries.sort();
    let total = entries.len();
    let listing = entries
        .into_iter()
        .take(CODE_PATH_TREE_CAP)
        .map(|e| format!("- {e}\n"))
        .collect::<String>();
    let note = if total > CODE_PATH_TREE_CAP {
        format!("…（共 {total} 条，只列前 {CODE_PATH_TREE_CAP} 条）\n")
    } else {
        String::new()
    };
    format!("\n#### `{display}/`（目录结构）\n\n{listing}{note}")
}

fn walk_code_dir(
    root: &std::path::Path,
    dir: &std::path::Path,
    depth: usize,
    out: &mut Vec<String>,
) {
    if depth >= CODE_PATH_TREE_DEPTH || out.len() >= CODE_PATH_TREE_CAP {
        return;
    }
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        if out.len() >= CODE_PATH_TREE_CAP {
            return;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        if path.is_dir() {
            if CODE_PATH_SKIP_DIRS.contains(&name.as_str()) || name.starts_with('.') {
                continue;
            }
            out.push(format!("{rel}/"));
            walk_code_dir(root, &path, depth + 1, out);
        } else {
            out.push(rel);
        }
    }
}

/// Create a handlebars registry with default settings (no escaping for markdown).
fn new_handlebars() -> handlebars::Handlebars<'static> {
    let mut reg = handlebars::Handlebars::new();
    reg.register_escape_fn(handlebars::no_escape);
    // Strict mode is OFF: missing variables render as `{{var}}` literals
    // rather than failing. This keeps prompts that reference project
    // context (e.g. manager.md) usable from the chat REPL where the
    // orchestrator's full context variables aren't injected.
    reg.set_strict_mode(false);
    reg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_role_category_parse() {
        assert_eq!(RoleCategory::parse("planning"), Some(RoleCategory::Planning));
        assert_eq!(RoleCategory::parse("Execution"), Some(RoleCategory::Execution));
        assert_eq!(RoleCategory::parse("verification"), Some(RoleCategory::Verification));
        assert_eq!(RoleCategory::parse("discussion"), Some(RoleCategory::Discussion));
        assert_eq!(RoleCategory::parse("unknown"), None);
    }

    #[test]
    fn test_default_icon() {
        assert_eq!(default_icon("pm"), "📋");
        assert_eq!(default_icon("unknown_role"), "🤖");
    }

    #[test]
    fn test_render_prompt() {
        let role = Role {
            id: "test".into(),
            name: "Test".into(),
            category: RoleCategory::Discussion,
            system_prompt: "You are a {{role_name}}. Topic: {{topic}}".into(),
            default_model_tier: crate::model_resolver::ModelTier::Standard,
            model_chain: vec![],
            default_params: GenerateParams::default(),
            allowed_tools: vec![],
            icon: "🧪".into(),
        };

        let vars: std::collections::HashMap<String, String> = [
            ("role_name".into(), "Tester".into()),
            ("topic".into(), "Login flow".into()),
        ]
        .into_iter()
        .collect();

        let rendered = role.render_prompt_str(&vars).unwrap();
        assert!(rendered.contains("Tester"));
        assert!(rendered.contains("Login flow"));
    }
    // ── global prompt fallback ──────────────────────────────────────────

    /// Helper: redirect `LATTE_HOME` to a temp dir for the duration of a
    /// test. Same thread-safety pattern as `config::tests::with_latte_home`.
    /// Async-aware: the closure returns a future so the env var stays
    /// set for the duration of the awaited operation.
    /// Helper for `#[tokio::test]` tests. Sets `LATTE_HOME` to a
    /// thread-scoped temp dir, runs `f` (which receives the path
    /// and returns a future), and cleans up `LATTE_HOME` on drop of
    /// the returned guard. The global `Mutex` serialises env-var
    /// access across parallel test threads. The closure is invoked
    /// synchronously to ensure env-var ordering; the returned future
    /// runs on the caller's tokio runtime.
    fn with_latte_home<F, Fut>(f: F) -> impl std::future::Future<Output = ()>
    where
        F: FnOnce(std::path::PathBuf) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        use std::sync::{Mutex, MutexGuard};
        // `MutexGuard` doesn't have a `'static` bound by default, so
        // we coerce via the `static` lifetime explicitly.
        let lock_guard: MutexGuard<'static, ()> = crate::test_util::ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap();


        let tmp = std::env::temp_dir().join(format!(
            "latte_role_test_home_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let prev = std::env::var("LATTE_HOME").ok();
        std::env::set_var("LATTE_HOME", &tmp);

        // Run the closure synchronously to obtain the future. Then
        // move the cleanup work into a Drop guard so it fires after
        // the caller has awaited the future.
        let tmp_for_closure = tmp.clone();
        let fut = f(tmp_for_closure.clone());
        struct Cleanup {
            _lock_guard: std::sync::MutexGuard<'static, ()>,
            tmp: std::path::PathBuf,
            prev: Option<String>,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                match &self.prev {
                    Some(v) => std::env::set_var("LATTE_HOME", v),
                    None => std::env::remove_var("LATTE_HOME"),
                }
                let _ = std::fs::remove_dir_all(&self.tmp);
            }
        }
        let cleanup = Cleanup {
            _lock_guard: lock_guard,
            tmp,
            prev,
        };
        async move {
            fut.await;
            drop(cleanup);
        }
    }
    fn make_template(prompt_file: Option<&str>) -> RoleTemplate {
        RoleTemplate {
            id: "pm".into(),
            name: "PM".into(),
            category: "planning".into(),
            model_tier: "standard".into(),
            model_chain: vec![],
            prompt_file: prompt_file.map(String::from),
            temperature: None,
            tools: vec![],
            icon: "".into(),
            skills: vec![],
            code_paths: vec![],
        }
    }

    #[tokio::test]
    async fn test_resolve_injects_code_paths() {
        // 真实临时夹具：一个文件 + 一个目录 + 一个不存在路径
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("notes.md"), "# 领域笔记\n要点一二三").unwrap();
        std::fs::create_dir_all(root.join("src/inner")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn lib() {}").unwrap();
        std::fs::write(root.join("src/inner/deep.rs"), "fn deep() {}").unwrap();

        let mut tpl = make_template(None);
        tpl.code_paths = vec![
            root.join("notes.md").to_string_lossy().into_owned(),
            root.join("src").to_string_lossy().into_owned(),
            root.join("missing").to_string_lossy().into_owned(),
        ];
        let role = tpl.resolve(&Default::default()).await.unwrap();
        let sp = &role.system_prompt;
        assert!(sp.contains("你的领域代码/文档资料"), "prompt: {sp}");
        // 文件内容被确定性内联
        assert!(sp.contains("要点一二三"), "prompt: {sp}");
        // 目录树条目
        assert!(sp.contains("lib.rs"), "prompt: {sp}");
        assert!(sp.contains("inner/"), "prompt: {sp}");
        // 不存在路径明确标注
        assert!(sp.contains("路径不存在"), "prompt: {sp}");
        // 空 code_paths 不注入
        let role2 = make_template(None).resolve(&Default::default()).await.unwrap();
        assert!(!role2.system_prompt.contains("你的领域代码/文档资料"));
    }

    #[test]
    fn test_render_code_file_truncates_over_8kb() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("big.txt");
        std::fs::write(&f, "x".repeat(10 * 1024)).unwrap();
        let out = render_code_path(&f.to_string_lossy(), 32 * 1024);
        assert!(out.contains("超过 8KB，截断"), "out: {}", &out[..200]);
        assert!(out.len() < 9 * 1024);
    }

    #[test]
    fn test_render_code_dir_skips_hidden_and_capped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/config"), "x").unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("target/out.o"), "x").unwrap();
        std::fs::write(root.join("keep.rs"), "x").unwrap();
        let out = render_code_path(&root.to_string_lossy(), 32 * 1024);
        assert!(out.contains("keep.rs"), "out: {out}");
        assert!(!out.contains(".git"), "out: {out}");
        assert!(!out.contains("target"), "out: {out}");
        // 超预算截断
        let tiny = render_code_path(&root.to_string_lossy(), 50);
        assert!(tiny.contains("超出注入预算，截断"), "tiny: {tiny}");
    }

    #[tokio::test]
    async fn test_resolve_uses_project_prompt_first() {
        // Project-side prompt exists; global override also exists.
        // Project must win.
        let project_prompt = std::env::temp_dir().join("latte_role_test_project_prompt.md");
        std::fs::write(&project_prompt, "FROM_PROJECT").unwrap();
        let template = make_template(Some(project_prompt.to_str().unwrap()));

        with_latte_home(|home| async move {
            let prompts = home.join("prompts.d");
            std::fs::create_dir_all(&prompts).unwrap();
            // (No global file for this test — project path is the
            // only source.)
            let role = template.resolve(&GenerateParams::default()).await.unwrap();
            assert_eq!(role.system_prompt, "FROM_PROJECT");
        })
        .await;
        let _ = std::fs::remove_file(&project_prompt);
    }

    #[tokio::test]
    async fn test_resolve_falls_back_to_global_prompts_d() {
        // Project path is missing; global prompts.d has the file.
        let template = make_template(Some("pm.md"));
        with_latte_home(|home| async move {
            let prompts = home.join("prompts.d");
            std::fs::create_dir_all(&prompts).unwrap();
            std::fs::write(prompts.join("pm.md"), "FROM_GLOBAL_FALLBACK").unwrap();
            let role = template.resolve(&GenerateParams::default()).await.unwrap();
            assert_eq!(role.system_prompt, "FROM_GLOBAL_FALLBACK");
        })
        .await;
    }
    #[tokio::test]
    async fn test_resolve_skips_global_for_absolute_paths() {
        // Absolute configured path: no global fallback attempted.
        let template = make_template(Some("/nonexistent/absolute/path.md"));
        with_latte_home(|home| async move {
            let prompts = home.join("prompts.d");
            std::fs::create_dir_all(&prompts).unwrap();
            std::fs::write(prompts.join("path.md"), "SHOULD_NOT_BE_USED").unwrap();
            let err = template
                .resolve(&GenerateParams::default())
                .await
                .err()
                .expect("absolute path missing on disk must error");
            assert!(format!("{err}").contains("cannot read prompt file"));
        })
        .await;
    }
}

/// Try to load a role's prompt from the global `~/.latte/prompts.d/`
/// directory. Returns `Some(content)` if the global file exists, `None`
/// otherwise (no `LATTE_HOME`, missing dir, or missing file).
///
/// Lookup rules:
/// 1. Take the **basename** of the configured `prompt_file` (e.g.
///    `prompts/pm.md` → `pm.md`).
/// 2. Read `$LATTE_HOME/prompts.d/<basename>`.
/// 3. Absolute paths, `~`-prefixed paths, and paths containing `..`
///    skip the global fallback (the user clearly meant a specific
///    location).
async fn resolve_global_prompt(
    configured: &str,
    _role_id: &str,
) -> Option<String> {
    use crate::global_config::GlobalConfig;
    let global_dir = GlobalConfig::global_dir()?;

    if configured.starts_with('/') || configured.starts_with('~') {
        return None;
    }
    if configured.contains("..") {
        return None;
    }
    let basename = std::path::Path::new(configured)
        .file_name()
        .and_then(|s| s.to_str())?;
    let candidate = global_dir.join("prompts.d").join(basename);
    match tokio::fs::read_to_string(&candidate).await {
        Ok(content) => Some(content),
        Err(_) => None,
    }
}

