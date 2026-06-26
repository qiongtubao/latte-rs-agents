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
        }
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

