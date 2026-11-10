//! Configuration types: agent roles, model catalog, TOML load/save.
//!
//! Mirrors gsd-core's `model-catalog.json` + `config-defaults.manifest.json` pattern,
//! but in TOML (Rust-native config format).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::error::{AgentError, AgentResult};
use crate::role::RoleTemplate;
use std::path::PathBuf;

/// Which layer of the layered config system we are loading from.
///
/// Both layers use the same directory layout: `agents/` for per-role
/// TOML files, `workflows/` for per-workflow TOML files, etc.
/// The only difference is the root directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigLayer {
    /// Project layer — `.latte/` under the current working directory.
    Project,
    /// Global layer — `$LATTE_HOME/` or `~/.latte/`.
    Global,
}

impl ConfigLayer {
    /// Return the root directory for this layer.
    ///
    /// - `Project` → `./.latte`
    /// - `Global` → `$LATTE_HOME` or `$HOME/.latte`
    pub fn root_dir(&self) -> Option<PathBuf> {
        match self {
            ConfigLayer::Project => Some(PathBuf::from(".latte")),
            ConfigLayer::Global => crate::global_config::GlobalConfig::global_dir(),
        }
    }

    /// Return the agents directory for this layer.
    ///
    /// The two layers use different layouts by convention:
    /// - **Project** prefers `.latte/agents/` (no `.d` suffix — the
    ///   layout this project ships), and falls back to
    ///   `.latte/agents.d/` for projects that follow the global
    ///   convention. The first directory that exists wins; if
    ///   neither exists the `.d/` form is returned so callers can
    ///   decide what to do with `None`.
    /// - **Global** always uses `agents.d/`.
    pub fn agents_dir(&self) -> Option<PathBuf> {
        match self {
            ConfigLayer::Project => {
                let root = self.root_dir()?;
                let no_d = root.join("agents");
                if no_d.is_dir() {
                    return Some(no_d);
                }
                Some(root.join("agents.d"))
            }
            ConfigLayer::Global => self.root_dir().map(|d| d.join("agents.d")),
        }
    }

    /// Return the workflows directory for this layer.
    /// Both project and global layers use `workflows.d/`.
    pub fn workflows_dir(&self) -> Option<PathBuf> {
        self.root_dir().map(|d| d.join("workflows.d"))
    }

    /// Return the logs root directory for this layer.
    pub fn logs_dir(&self) -> Option<PathBuf> {
        self.root_dir().map(|d| d.join("logs"))
    }

    /// Return the logs/agents directory for this layer.
    pub fn log_agents_dir(&self) -> Option<PathBuf> {
        self.root_dir().map(|d| d.join("logs").join("agents"))
    }
}

/// Merge `src` roles/tiers into `dst` using **field-level** semantics:
/// - New role ids from `src` are inserted as-is.
/// - Existing role ids get only **empty fields** filled from `src`
///   (`model_chain`, `icon`, `temperature`). Name, category,
///   model_tier, prompt_file, and tools stay with the project's value.
fn merge_global_into(dst: &mut HashMap<String, RoleTemplate>, src: &HashMap<String, RoleTemplate>) {
    for (id, role) in src {
        match dst.entry(id.clone()) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(role.clone());
            }
            std::collections::hash_map::Entry::Occupied(mut e) => {
                let existing = e.get_mut();
                // model_chain: global fills in if project has none.
                if existing.model_chain.is_empty() && !role.model_chain.is_empty() {
                    existing.model_chain = role.model_chain.clone();
                }
                // icon: global fills in if project has default empty.
                if existing.icon.is_empty() && !role.icon.is_empty() {
                    existing.icon = role.icon.clone();
                }
                // temperature: global fills in if project omitted it.
                if existing.temperature.is_none() && role.temperature.is_some() {
                    existing.temperature = role.temperature;
                }
            }
        }
    }
}

fn merge_models_or_insert(dst: &mut Vec<ModelDef>, src: &[ModelDef]) {
    // 按 (provider, name) 复合键去重 —— 历史上用 `id`，但 ModelDef 已经
    // 去掉 `id` 字段，`name` 接管 API id 的语义；`provider` 仍是区分
    // 同名 model 的关键（"gpt-4"@openai vs "gpt-4"@azure 是不同的）。
    for m in src {
        if !dst
            .iter()
            .any(|e| e.provider == m.provider && e.name == m.name)
        {
            dst.push(m.clone());
        }
    }
}

fn merge_tiers_or_insert(
    dst: &mut Option<HashMap<String, String>>,
    src: &Option<HashMap<String, String>>,
) {
    if let Some(s) = src {
        let d = dst.get_or_insert_with(Default::default);
        for (k, v) in s {
            d.entry(k.clone()).or_insert(v.clone());
        }
    }
}

fn merge_role_tiers_or_insert(
    dst: &mut Option<HashMap<String, HashMap<String, String>>>,
    src: &Option<HashMap<String, HashMap<String, String>>>,
) {
    if let Some(s) = src {
        let d = dst.get_or_insert_with(Default::default);
        for (role, tier_map) in s {
            let entry = d.entry(role.clone()).or_insert_with(Default::default);
            for (k, v) in tier_map {
                entry.entry(k.clone()).or_insert(v.clone());
            }
        }
    }
}

// ─── Top-level config ────────────────────────────────────────────────────

/// Top-level agent configuration, loaded from `agents.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentConfig {
    /// Model catalog with tier mappings.
    #[serde(default)]
    pub models: ModelCatalog,
    /// Role definitions.
    #[serde(default)]
    pub roles: HashMap<String, RoleTemplate>,
    /// Advisor 总开关（`[advisor]` 段）。project 层覆盖 global 层。
    #[serde(default)]
    pub advisor: AdvisorSection,
    /// 自动上下文压缩（`[compaction]` 段）。project 层覆盖 global 层。
    #[serde(default)]
    pub compaction: CompactionSection,
    /// workflow / session token 预算的行为（`[token_budget]` 段）。
    /// project 层覆盖 global 层。
    #[serde(default)]
    pub token_budget: TokenBudgetSection,
}

/// `[token_budget]` 配置段：共享 token 计量器**拦不拦**、拦截线在哪。
///
/// 任意 agents 配置文件（project `.latte/agents*.toml`、`agents.d/*.toml`
/// 或 global `~/.latte/agents.d/*.toml`）都可声明：
///
/// ```toml
/// [token_budget]
/// enforce = false       # 默认：只统计，不拦截
/// workflow_limit = 1000000
/// ```
///
/// **默认 `enforce = false`。** 计量器每次都照常记录真实用量（UI 与
/// 收尾报告都靠它），只是不再拿估算值把请求挡在门外。要恢复硬拦截
/// 得显式打开。
///
/// 为什么不默认开：拦截线唯一能看见的信号是「请求发出前的 prompt 估算」，
/// 它系统性偏低（工具 schema、历史重传、thinking 都不在里面），而超限的
/// 后果是**就地判死整步**——实测 jemalloc `wf-learn-1794286593148353`：
/// investigate 步 31 次调用烧 764,742 token，第 32 次准入被拒，31 次
/// 回复全为空（都在调工具、没有 partial），降级采纳无从下手，整步失败、
/// 62 条工具结果作废。结构性的失控本该由 `max_requests` / `max_tool_calls`
/// 这类精确且廉价的计数先抓；用钱当唯一刹车既漏（估算偏低）又误杀
/// （健康的深取证）。预算作为**账本**永远有用，作为**刹车**要慎用。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TokenBudgetSection {
    /// 是否按 `workflow_limit` 拦截。`None` = 未配置 = `false`（只统计）。
    #[serde(default)]
    pub enforce: Option<bool>,
    /// workflow 根级上限（含全部嵌套 run 的实际用量）。`None` = 内置默认
    /// [`crate::workflow::DEFAULT_WORKFLOW_TOKEN_BUDGET`]。拦截关掉时它
    /// 仍是报告里的分母。
    #[serde(default)]
    pub workflow_limit: Option<u64>,
}

impl TokenBudgetSection {
    /// 生效的拦截开关：env > 配置 > 内置默认（关）。
    ///
    /// env 最高优先级与 `[compaction]` 一致：临时压测不该改文件。
    pub fn enforce(&self) -> bool {
        match std::env::var("LATTE_AGENT_TOKEN_BUDGET_ENFORCE") {
            Ok(v) => !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            ),
            Err(_) => self.enforce.unwrap_or(false),
        }
    }

    /// 是否一个字段都没配（分层合并用的整段判定）。
    pub fn is_unset(&self) -> bool {
        self.enforce.is_none() && self.workflow_limit.is_none()
    }
}

/// `[compaction]` 配置段：自动上下文压缩的可调参数。
///
/// 任意 agents 配置文件（project `.latte/agents*.toml`、`agents.d/*.toml`
/// 或 global `~/.latte/agents.d/*.toml`）都可声明：
///
/// ```toml
/// [compaction]
/// enabled = true
/// threshold_ratio = 0.8      # 占 context_window 的比例，钳 0.5..=0.95
/// keep_recent_turns = 2      # 保留逐字原文的尾部轮数，钳 1..=32
/// summary_max_tokens = 2048  # 摘要输出上限，钳 256..=16384
/// max_passes = 1             # 单次检查最多压几遍，钳 1..=3
/// sections = ["Goal", "Progress", "Next Step"]   # 自定义摘要模板段落
/// ```
///
/// 每个字段都是 `Option`，用来区分「未配置」与「显式值」——分层合并时
/// project 层的 `Some` 覆盖 global 层（与 [`AdvisorSection`] 同款语义）。
///
/// 优先级：**环境变量 > 项目配置 > 全局配置 > 内置默认**。环境变量留在
/// 最高优先级是为了保留「临时压测某个参数不用改文件」的能力
/// （见 [`crate::compaction::CompactionPolicy::resolve`]）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CompactionSection {
    /// 总开关（缺省 true）。
    #[serde(default)]
    pub enabled: Option<bool>,
    /// 触发阈值：占 `context_window` 的比例。
    #[serde(default)]
    pub threshold_ratio: Option<f64>,
    /// 保留逐字原文的尾部轮数。
    #[serde(default)]
    pub keep_recent_turns: Option<usize>,
    /// 摘要输出的 token 上限。
    #[serde(default)]
    pub summary_max_tokens: Option<u32>,
    /// 单次检查内最多连续压几遍。
    #[serde(default)]
    pub max_passes: Option<u8>,
    /// 自定义摘要模板段落（只给标题，语义由标题自解释）。
    /// 空 / 未配置 = 用内置 8 段模板。
    #[serde(default)]
    pub sections: Option<Vec<String>>,
}

impl CompactionSection {
    /// 本段是否**完全未配置**（用于分层合并时判断要不要整体取用下层值）。
    pub fn is_unset(&self) -> bool {
        self == &Self::default()
    }
}

/// `[advisor]` 配置段：advisor 监察的总开关。
///
/// 任意 agents 配置文件（project `.latte/agents*.toml`、`agents.d/*.toml`
/// 或 global `~/.latte/agents.d/*.toml`）都可声明：
///
/// ```toml
/// [advisor]
/// enabled = false
/// ```
///
/// `false` 关闭 advisor 的全部活动：事件流 monitor、D5/D6 产出门禁、
/// delegate 返回审查（含 workflow 的审查重做）。模型不可达等降级路径
/// 不受影响（本来就不审查）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AdvisorSection {
    /// 总开关（缺省 true）。`Option` 区分「未配置」与「显式 false」
    /// ——分层合并时 project 层的 `Some` 覆盖 global 层。
    #[serde(default)]
    pub enabled: Option<bool>,
    /// delegate-return 单次审查超时（秒）。
    ///
    /// 此前只是 [`crate::advisor_monitor::AdvisorReviewSettings`] 上的一个
    /// 结构体字段，**没有任何从 TOML/env 读进来的路径**，实际只能改代码
    /// 常量。实测 2026-09-10 jemalloc 会话：默认 90s 撞上常态 >120s 的慢
    /// 模型，4 次「审查超时未审直接放行」——审查门禁静默失效，而这个参数
    /// 当时改不了。
    #[serde(default)]
    pub delegate_review_timeout_secs: Option<u64>,
    /// workflow speaker 返回被判 intervene/terminate 时的重做上限。
    #[serde(default)]
    pub return_max_redo: Option<u8>,
}

impl AdvisorSection {
    /// 生效值：未配置 = 开。
    pub fn enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    /// 整段是否一个字段都没配。分层合并用它判「要不要整段覆盖」。
    ///
    /// 必须逐字段判：只写 `return_max_redo` 时 `enabled` 是 `None`，若还按
    /// 老写法用 `enabled.is_some()` 当闸门，整段永远合并不进来、配置静默
    /// 失效（compaction 段就踩过这个坑）。
    pub fn is_unset(&self) -> bool {
        self.enabled.is_none()
            && self.delegate_review_timeout_secs.is_none()
            && self.return_max_redo.is_none()
    }

    /// 审查超时生效值：**env > 配置 > 内置默认**，并钳到安全区间。
    ///
    /// 钳位下限 10s：再小就等于把审查关掉（还白发一次请求）。上限 600s：
    /// 审查是串在流水线上的，给太大等于让慢模型拖死整条 workflow。
    pub fn delegate_review_timeout_secs(&self) -> u64 {
        let raw = std::env::var("LATTE_AGENT_ADVISOR_REVIEW_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .or(self.delegate_review_timeout_secs)
            .unwrap_or(crate::advisor_monitor::DEFAULT_DELEGATE_REVIEW_TIMEOUT_SECS);
        raw.clamp(10, 600)
    }

    /// 返回重做上限生效值：env > 配置 > 默认，钳在 `0..=MAX_RETURN_REDO`。
    pub fn return_max_redo(&self) -> u8 {
        let raw = std::env::var("LATTE_AGENT_ADVISOR_RETURN_MAX_REDO")
            .ok()
            .and_then(|v| v.trim().parse::<u8>().ok())
            .or(self.return_max_redo)
            .unwrap_or(crate::advisor_monitor::DEFAULT_RETURN_MAX_REDO);
        raw.min(crate::advisor_monitor::MAX_RETURN_REDO)
    }

    /// 组装成运行时用的 [`crate::advisor_monitor::AdvisorReviewSettings`]。
    pub fn review_settings(&self) -> crate::advisor_monitor::AdvisorReviewSettings {
        crate::advisor_monitor::AdvisorReviewSettings {
            delegate_review_timeout_secs: self.delegate_review_timeout_secs(),
            return_max_redo: self.return_max_redo(),
        }
    }
}

/// Configuration for which hooks a role uses. Each variant selects a
/// built-in hook by kind and optionally provides its parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookKindConfig {
    /// No hook (default).
    None,
    /// Redact PII from outgoing messages.
    RedactPii,
    /// Enforce a tool allowlist (abort on disallowed calls).
    EnforceToolAllowlist,
    /// Require at least `min_words` in the model response.
    RequireToolCall { min_words: usize },
    /// Monitor context token usage; abort or warn when approaching
    /// the configured budget.
    ContextMonitor {
        /// Soft threshold (0.0–1.0 of budget). Emit warning above this.
        warn_at: f64,
        /// Hard threshold (0.0–1.0 of budget). Abort above this.
        abort_at: f64,
    },
}

impl Default for HookKindConfig {
    fn default() -> Self {
        Self::None
    }
}

/// A single hook applied at a specific point in the workflow step
/// lifecycle. Mirrors the gsd-core `StepHook` concept.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepHookConfig {
    /// Which hook kind to install.
    pub kind: HookKindConfig,
    /// Optional role filter — only apply when the current role matches.
    /// Empty string or "*" means all roles.
    #[serde(default)]
    pub role_filter: String,
}

impl AgentConfig {
    /// Load config from a path. If `path` is a directory, merge every
    /// `*.toml` inside (models and roles are concatenated). If it is a
    /// file, load that file directly.
    ///
    /// Directory mode supports the layout:
    ///   .latte/agents/
    ///     pm.toml
    ///     architect.toml
    ///     ...
    ///   .latte/workflows/
    ///     code_review.toml
    ///     ...
    pub fn load(path: &str) -> AgentResult<Self> {
        let meta = std::fs::metadata(path).map_err(|e| {
            AgentError::Config(format!("cannot stat config path '{}': {}", path, e))
        })?;

        if meta.is_dir() {
            return Self::load_dir(path);
        }

        let content = std::fs::read_to_string(path).map_err(|e| {
            AgentError::Config(format!("cannot read config file '{}': {}", path, e))
        })?;
        Self::parse(&content)
    }
    /// Load project + global configs and merge them.
    ///
    /// The two layers use different layouts via [`ConfigLayer`]:
    /// - Project: `ConfigLayer::Project.agents_dir()` — prefers
    ///   `./.latte/agents/`, falls back to `./.latte/agents.d/`.
    /// - Global:  `ConfigLayer::Global.agents_dir()` (`$LATTE_HOME/agents.d/` or `~/.latte/agents.d/`)
    ///
    /// **Project-wins** per role id: the project's version replaces
    /// the global one. The global layer only contributes ids the
    /// project did not declare.
    ///
    /// `project_path` — optional override for the project agents dir.
    /// When `None`, defaults to [`ConfigLayer::Project.agents_dir()`].
    ///
    /// Returns `Ok(default)` if neither layer has anything.
    pub fn load_with_global(project_path: Option<&str>) -> AgentResult<Self> {
        let mut merged = Self::default();

        // 1) Project layer: use explicit path or fallback to ConfigLayer.
        let project_dir: Option<String> = project_path
            .filter(|p| std::fs::metadata(p).is_ok())
            .map(|s| s.to_string())
            .or_else(|| {
                ConfigLayer::Project
                    .agents_dir()
                    .filter(|d| d.is_dir())
                    .and_then(|d| d.to_str().map(|s| s.to_string()))
            });
        if let Some(path) = project_dir {
            if let Ok(part) = Self::load(&path) {
                merged.roles.extend(part.roles);
                // Advisor 开关：project 层声明了才覆盖（global 在后，
                // 只填 project 没声明的空档）。
                if !part.advisor.is_unset() {
                    merged.advisor = part.advisor.clone();
                }
                // compaction 独立判断——**不能嵌在 advisor 的 if 里**：
                // 只写 compaction.toml（不写 advisor）时 advisor 是 None，
                // 嵌在里面会导致这段永远不被合并（实测：目录加载后
                // compaction 恒为空段，UI 保存的值读不回来）。
                if !part.compaction.is_unset() {
                    merged.compaction = part.compaction.clone();
                }
                if !part.token_budget.is_unset() {
                    merged.token_budget = part.token_budget.clone();
                }
                for m in part.models.models {
                    if !merged
                        .models
                        .models
                        .iter()
                        .any(|e| e.provider == m.provider && e.name == m.name)
                    {
                        merged.models.models.push(m);
                    }
                }
                if let Some(t) = part.models.tiers {
                    merged
                        .models
                        .tiers
                        .get_or_insert_with(Default::default)
                        .extend(t);
                }
                if let Some(rt) = part.models.role_tiers {
                    let dst = merged
                        .models
                        .role_tiers
                        .get_or_insert_with(Default::default);
                    for (role, tier_map) in rt {
                        dst.entry(role)
                            .or_insert_with(Default::default)
                            .extend(tier_map);
                    }
                }
            }
        }

        // 2) Global layer: always uses ConfigLayer::Global.
        if let Some(agents_dir) = ConfigLayer::Global.agents_dir() {
            if agents_dir.is_dir() {
                if let Ok(global_part) = Self::load(agents_dir.to_str().unwrap()) {
                    merge_global_into(&mut merged.roles, &global_part.roles);
                    merge_models_or_insert(&mut merged.models.models, &global_part.models.models);
                    merge_tiers_or_insert(&mut merged.models.tiers, &global_part.models.tiers);
                    merge_role_tiers_or_insert(
                        &mut merged.models.role_tiers,
                        &global_part.models.role_tiers,
                    );
                    // Advisor 开关：只填 project 未声明的空档（project 优先）。
                    if merged.advisor.is_unset() && !global_part.advisor.is_unset() {
                        merged.advisor = global_part.advisor.clone();
                    }
                    // compaction 同款：project 层整段没配才取全局值。
                    if merged.compaction.is_unset() && !global_part.compaction.is_unset() {
                        merged.compaction = global_part.compaction.clone();
                    }
                    if merged.token_budget.is_unset() && !global_part.token_budget.is_unset() {
                        merged.token_budget = global_part.token_budget.clone();
                    }
                }
            }
        }

        // 3) Built-in role fallback.
        if merged.roles.is_empty() {
            for id in [
                "pm",
                "architect",
                "programmer",
                "tester",
                "reviewer",
                "devops",
                "security",
                "designer",
                "tech_writer",
                "manager",
                "advisor",
            ] {
                if let Some(tmpl) = crate::prompts::template_for(id) {
                    merged.roles.insert(id.to_string(), tmpl);
                }
            }
        }

        Ok(merged)
    }

    /// Load every `*.toml` file in `dir` (non-recursive) and merge the
    /// results. A `roles.<id>` key collision across files is an error so
    /// users get immediate feedback on duplicate role definitions.
    fn load_dir(dir: &str) -> AgentResult<Self> {
        let mut merged = Self::default();
        let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
            .map_err(|e| AgentError::Config(format!("cannot read dir '{}': {}", dir, e)))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("toml"))
            .collect();
        // Deterministic merge order.
        entries.sort();

        for entry in &entries {
            let content = std::fs::read_to_string(entry).map_err(|e| {
                AgentError::Config(format!("cannot read '{}': {}", entry.display(), e))
            })?;
            let mut part = Self::parse(&content)?;

            // 如果标准解析没得到 model，尝试作为裸 ModelDef 解析
            // （write_project 写入的格式：直接是 name/providr/base_url 等字段）
            if part.models.models.is_empty() && part.roles.is_empty() {
                if let Ok(bare_def) = toml::from_str::<ModelDef>(&content) {
                    part.models.models.push(bare_def);
                }
            }

            // Roles: detect duplicate IDs.
            for id in part.roles.keys() {
                if merged.roles.contains_key(id) {
                    return Err(AgentError::Config(format!(
                        "duplicate role '{}' in {}",
                        id,
                        entry.display()
                    )));
                }
            }
            merged.roles.extend(part.roles);

            // Advisor 开关：声明过的文件覆盖先前的（字母序后写赢）。
            if !part.advisor.is_unset() {
                merged.advisor = part.advisor.clone();
            }
            // compaction 独立判断（同上：不能嵌在 advisor 的 if 里）。
            if !part.compaction.is_unset() {
                merged.compaction = part.compaction.clone();
            }
            if !part.token_budget.is_unset() {
                merged.token_budget = part.token_budget.clone();
            }

            // Models: append.
            merged.models.models.extend(part.models.models);
            // Tier maps: merge shallowly (last writer wins per key).
            if let Some(tiers) = part.models.tiers {
                merged
                    .models
                    .tiers
                    .get_or_insert_with(Default::default)
                    .extend(tiers);
            }
            if let Some(rt) = part.models.role_tiers {
                let dst = merged
                    .models
                    .role_tiers
                    .get_or_insert_with(Default::default);
                for (role, tier_map) in rt {
                    dst.entry(role)
                        .or_insert_with(Default::default)
                        .extend(tier_map);
                }
            }
        }

        Ok(merged)
    }

    /// Parse config from TOML string.
    pub fn parse(content: &str) -> AgentResult<Self> {
        toml::from_str(content).map_err(|e| AgentError::Config(format!("invalid TOML: {}", e)))
    }

    /// Save config to a TOML file path.
    pub fn save(&self, path: &str) -> AgentResult<()> {
        let content = toml::to_string_pretty(self)
            .map_err(|e| AgentError::Config(format!("serialization failed: {}", e)))?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Resolve all role templates into full `Role` definitions.
    pub async fn resolve_roles(
        &self,
        default_params: &latte_ai::params::GenerateParams,
    ) -> AgentResult<HashMap<String, crate::role::Role>> {
        let mut roles = HashMap::new();
        for (id, template) in &self.roles {
            let role = template.resolve(default_params).await?;
            roles.insert(id.clone(), role);
        }
        Ok(roles)
    }
}

// ─── Model catalog ───────────────────────────────────────────────────────

/// Model catalog with tier-to-model mappings.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelCatalog {
    /// All known model definitions.
    #[serde(default)]
    pub models: Vec<ModelDef>,
    /// Global tier → model_id mapping.
    #[serde(default)]
    pub tiers: Option<HashMap<String, String>>,
    /// Per-role tier overrides: role_id → (tier → model_id).
    #[serde(default)]
    pub role_tiers: Option<HashMap<String, HashMap<String, String>>>,
}

/// A single model definition in the catalog.
///
/// Compatible with `latte-rs-model-router/models.toml` format, extended with
/// optional `tier`, `supports_thinking`, and `timeout_secs` fields.
///
/// `timeout_secs` is a **streaming** timeout for this model (first token and
/// gap between SSE chunks; read by `latte-ai`, overrides
/// `LATTE_AI_FIRST_EVENT_TIMEOUT_SECS` / `LATTE_AI_IDLE_TIMEOUT_SECS`). It is
/// not a per-turn wall-clock budget: how long a delegate / workflow step may
/// run is bounded by request-count budgets (`workflow::STEP_BUDGET_TIERS`)
/// plus a global backstop (`LATTE_AGENT_DELEGATE_TIMEOUT_SECS`, default 3h).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDef {
    /// Model identifier — 同时是 API 请求里的 `model` 字段与 UI 显示名。
    /// `model_name` 旧字段通过 serde alias 反向兼容。
    #[serde(alias = "model_name")]
    pub name: String,
    pub api: String,
    /// Provider name. 与 `name` 一起组成 composite_key `provider/name`，
    /// 避免同名 model 在不同厂商下冲突。
    pub provider: String,
    /// Base URL for API requests.
    pub base_url: String,
    /// API key (supports `${ENV_VAR}` substitution).
    pub api_key: String,
    /// Context window size in tokens.
    pub context_window: u32,
    /// Maximum output tokens.
    ///
    /// **原样**作为出站请求的输出上限下发，不经过任何隐式钳位 —— 这里写多少
    /// 就发多少。配得超过厂商真实上限时会收到 4xx，那是可诊断的；早先版本会
    /// 悄悄压到 64000，代价是改这个数看不到任何变化、等于配置项是死的。
    /// `0` = 不配置（不下发该字段，由厂商默认值决定）。
    pub max_tokens: u32,
    /// 完全**不下发**输出上限字段，把每次响应的上限交给上游 API 决定。
    ///
    /// 用于**代理**：它转发给一个我们无法探知真实输出上限的后端，发一个猜
    /// 的值只会换来上游 400。典型场景是自建/第三方 OpenAI-compatible 网关
    /// 和 Ollama 这类本地转发层。
    ///
    /// 只对 OpenAI-compatible 协议有效；Anthropic 协议要求该字段必填，
    /// 那条路在完全没配时用 `ANTHROPIC_UNCONFIGURED_MAX_TOKENS`（4096）兜底。
    #[serde(default)]
    pub omit_max_tokens: bool,
    /// 输出上限用哪个字段名下发：`"max_tokens"`（默认）或
    /// `"max_completion_tokens"`。
    ///
    /// 较新的 OpenAI-compatible 端点废弃了 `max_tokens`、只认后者；写死一个
    /// 的后果是遇到新端点时**静默失效**（对方忽略该字段，于是又回到"由厂商
    /// 默认值说话"）。
    #[serde(default)]
    pub max_tokens_field: latte_ai::models::MaxTokensField,

    /// OpenAI-compatible 端点是否下发 prompt caching 断点
    /// （`cache_control`）。**默认关**：不认这个字段的兼容端点看到未知
    /// 字段会 400，无条件下发等于把可用模型打挂。只在确认支持的端点上
    /// 开（OpenRouter 转 Claude、DeepSeek 上下文缓存、阿里云等）。
    ///
    /// Anthropic 原生协议（`api = "anthropic"`）**不看这个开关**：那条
    /// 路的 caching 是协议一等公民、恒开。
    ///
    /// 收益（实测 jemalloc 会话）：单会话 22.19M 输入 token、工具结果
    /// 唯一内容仅 0.97M = 22.7 倍放大。缓存住工具定义 + system prompt
    /// 这段"巨大且不变的前缀"是对症解法。
    #[serde(default)]
    pub prompt_cache: bool,

    /// 是否下发顶层 `prompt_cache_key`（**缓存亲和键**）。
    ///
    /// 与 `prompt_cache` 是两条独立机制：那个声明「缓存边界在哪」（需要
    /// 端点认 Anthropic 的 `cache_control`，风险高所以默认关）；这个只
    /// 声明「这些请求属于同一会话」，走供应商的自动前缀缓存。命中自动
    /// 缓存要求请求落到持有那份 KV cache 的机器上，网关的负载均衡不保证
    /// 这点，这个键就是给它做路由亲和用的。
    ///
    /// 不写 = 跟随全局默认（**开**，`LATTE_AI_PROMPT_CACHE_KEY=0` 可关）。
    /// 写 `false` 给个别对未知顶层字段严格 400 的端点用。
    #[serde(default)]
    pub prompt_cache_key: Option<bool>,
    /// Whether the model supports thinking/reasoning.
    #[serde(default)]
    pub supports_thinking: bool,
    /// Whether the model can accept image inputs. Defaults to `false`
    /// so existing configs work without changes.
    #[serde(default)]
    pub supports_vision: bool,
    /// Whether the model can generate images (image generation API, not chat completion).
    #[serde(default)]
    pub supports_image_generation: bool,
    /// Cost per million input tokens (USD). Optional for local models.
    #[serde(default)]
    pub cost_per_million_input: Option<f64>,
    /// Cost per million output tokens (USD). Optional for local models.
    #[serde(default)]
    pub cost_per_million_output: Option<f64>,
    /// Which tier this model belongs to (for auto-resolution without explicit tier map).
    #[serde(default)]
    pub tier: Option<String>,
    /// 流式超时（秒）：首 token 与 SSE chunk 间隔，精确覆盖 `LATTE_AI_*` 环境变量。
    /// 慢速 reasoning 模型可以放宽（如 300），已知秒回的模型可以收紧做快速失败。
    /// **不是**整个 turn 的总时长——此前 delegate 路径把它复用成总时长，
    /// 配大配小都会误伤另一边，已拆开。
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_config() {
        let toml = r#"
[models]
tiers = { premium = "claude-opus", standard = "claude-sonnet" }

[[models.models]]
name = "claude-opus"
api = "anthropic"
provider = "anthropic"
base_url = "https://api.anthropic.com"
api_key = "test-key"
context_window = 200000
max_tokens = 8192

[[models.models]]
name = "claude-sonnet"
api = "anthropic"
provider = "anthropic"
base_url = "https://api.anthropic.com"
api_key = "test-key"
context_window = 200000
max_tokens = 8192

[roles.pm]
id = "pm"
name = "pm"
category = "planning"
model_tier = "standard"
icon = "📋"
"#;

        let config = AgentConfig::parse(toml).unwrap();
        assert_eq!(config.models.models.len(), 2);
        assert_eq!(config.roles.len(), 1);
    }
    /// 旧 schema `model_name` 字段必须仍能通过 `#[serde(alias)]` 解析为 `id`：
    /// 用户实际目录 `~/.latte/models.d/*.toml` 普遍用 `model_name = "..."` + `provider = "..."`
    /// 两字段配合，如果 alias 丢了 → load 失败 → UI "暂无 model"。
    #[test]
    fn test_legacy_model_name_alias() {
        let toml = r#"
model_name = "deepseek-v4-pro"
api = "openai"
provider = "deepseek"
base_url = "https://api.deepseek.com"
api_key = "sk-test"
context_window = 1000000
max_tokens = 384000
tier = "budget"
timeout_secs = 300
"#;
        let def: ModelDef =
            toml::from_str(toml).expect("legacy `model_name` field must deserialize via alias");
        assert_eq!(def.name, "deepseek-v4-pro");
        assert_eq!(def.provider, "deepseek");
        assert_eq!(def.context_window, 1_000_000);
    }

    #[test]
    fn test_parse_model_def() {
        let toml = r#"
name = "test-model"
api = "openai"
provider = "test"
base_url = "http://localhost"
api_key = "key123"
context_window = 32000
max_tokens = 4096
supports_thinking = false
tier = "budget"
"#;

        let def: ModelDef = toml::from_str(toml).unwrap();
        assert_eq!(def.name, "test-model");
        assert_eq!(def.api, "openai");
        assert_eq!(def.tier, Some("budget".into()));
    }
    #[test]
    fn test_load_dir_merges_role_files() {
        let tmp = std::env::temp_dir().join("latte_agent_test_dir_merge");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        std::fs::write(
            tmp.join("pm.toml"),
            r#"
[roles.pm]
id = "pm"
name = "PM"
category = "planning"
model_tier = "standard"
icon = "P"
"#,
        )
        .unwrap();
        std::fs::write(
            tmp.join("architect.toml"),
            r#"
[roles.architect]
id = "architect"
name = "Arch"
category = "planning"
model_tier = "premium"
icon = "A"
"#,
        )
        .unwrap();
        // Non-TOML files must be ignored.
        std::fs::write(tmp.join("README.md"), "ignore me").unwrap();

        let cfg = AgentConfig::load(tmp.to_str().unwrap()).unwrap();
        assert_eq!(cfg.roles.len(), 2);
        assert!(cfg.roles.contains_key("pm"));
        assert!(cfg.roles.contains_key("architect"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_load_dir_detects_duplicate_role() {
        let tmp = std::env::temp_dir().join("latte_agent_test_dir_dup");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let body = r#"
[roles.pm]
id = "pm"
name = "PM"
category = "planning"
model_tier = "standard"
"#;
        std::fs::write(tmp.join("a.toml"), body).unwrap();
        std::fs::write(tmp.join("b.toml"), body).unwrap();

        let err = AgentConfig::load(tmp.to_str().unwrap()).unwrap_err();
        assert!(format!("{}", err).contains("duplicate role 'pm'"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── load_with_global ────────────────────────────────────────────────

    /// Helper: redirect `LATTE_HOME` to a temp dir for the duration of a
    /// test, restoring the previous value afterwards. Takes a global
    /// mutex so concurrent test threads don't clobber each other's
    /// `LATTE_HOME` (env vars are process-wide, but cargo test runs
    /// each `#[test]` on a separate thread).
    fn with_latte_home<F: FnOnce(&std::path::Path)>(f: F) {
        let _guard = crate::test_util::env_lock();

        let tmp = std::env::temp_dir().join(format!(
            "latte_agent_test_home_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let prev = std::env::var("LATTE_HOME").ok();
        std::env::set_var("LATTE_HOME", &tmp);
        f(&tmp);
        match prev {
            Some(v) => std::env::set_var("LATTE_HOME", v),
            None => std::env::remove_var("LATTE_HOME"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_load_with_global_only_project() {
        // No global layer: project roles only.
        let tmp = std::env::temp_dir().join("latte_agent_test_lwg_project_only");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(
            tmp.join("pm.toml"),
            r#"
[roles.pm]
id = "pm"
name = "PM"
category = "planning"
model_tier = "standard"
icon = "P"
"#,
        )
        .unwrap();

        with_latte_home(|_home| {
            let cfg = AgentConfig::load_with_global(Some(tmp.to_str().unwrap())).unwrap();
            assert_eq!(cfg.roles.len(), 1);
            assert!(cfg.roles.contains_key("pm"));
        });
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_load_with_global_fills_missing_from_global() {
        // Project defines `pm`; global defines `architect`. Both should
        // appear in the merged result, with project-wins on collision.
        let project = std::env::temp_dir().join("latte_agent_test_lwg_project_fill");
        let _ = std::fs::remove_dir_all(&project);
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("pm.toml"),
            r#"
[roles.pm]
id = "pm"
name = "PM_from_project"
category = "planning"
model_tier = "standard"
icon = "P"
"#,
        )
        .unwrap();

        with_latte_home(|home| {
            let agents_dir = home.join("agents.d");
            std::fs::create_dir_all(&agents_dir).unwrap();
            std::fs::write(
                agents_dir.join("architect.toml"),
                r#"
[roles.architect]
id = "architect"
name = "Arch_from_global"
category = "planning"
model_tier = "premium"
icon = "A"
"#,
            )
            .unwrap();

            let cfg = AgentConfig::load_with_global(Some(project.to_str().unwrap())).unwrap();
            assert_eq!(cfg.roles.len(), 2);
            assert!(cfg.roles.contains_key("pm"));
            assert!(cfg.roles.contains_key("architect"));
            // Project-wins: PM came from project.
            assert_eq!(cfg.roles["pm"].name, "PM_from_project");
            // Architect came from global.
            assert_eq!(cfg.roles["architect"].name, "Arch_from_global");
        });
        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn test_advisor_switch_parse_and_default() {
        // 未声明 → 默认开。
        let cfg = AgentConfig::parse(
            "[roles.pm]\nid=\"pm\"\nname=\"P\"\ncategory=\"c\"\nmodel_tier=\"standard\"\n",
        )
        .unwrap();
        assert!(cfg.advisor.enabled());
        // 显式 false。
        let cfg = AgentConfig::parse("[advisor]\nenabled = false\n").unwrap();
        assert!(!cfg.advisor.enabled());
        // 显式 true。
        let cfg = AgentConfig::parse("[advisor]\nenabled = true\n").unwrap();
        assert!(cfg.advisor.enabled());
    }

    #[test]
    fn test_advisor_switch_project_overrides_global() {
        let project = std::env::temp_dir().join("latte_agent_test_advisor_switch");
        let _ = std::fs::remove_dir_all(&project);
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("pm.toml"),
            "[advisor]\nenabled = false\n\n[roles.pm]\nid = \"pm\"\nname = \"P\"\ncategory = \"c\"\nmodel_tier = \"standard\"\n",
        )
        .unwrap();

        with_latte_home(|home| {
            let agents_dir = home.join("agents.d");
            std::fs::create_dir_all(&agents_dir).unwrap();
            std::fs::write(
                agents_dir.join("advisor.toml"),
                "[advisor]\nenabled = true\n",
            )
            .unwrap();

            // project false 覆盖 global true。
            let cfg = AgentConfig::load_with_global(Some(project.to_str().unwrap())).unwrap();
            assert!(!cfg.advisor.enabled(), "project 层应覆盖 global 层");
        });

        // project 未声明 → global 生效（false 也能穿透）。
        std::fs::write(
            project.join("pm.toml"),
            "[roles.pm]\nid = \"pm\"\nname = \"P\"\ncategory = \"c\"\nmodel_tier = \"standard\"\n",
        )
        .unwrap();
        with_latte_home(|home| {
            let agents_dir = home.join("agents.d");
            std::fs::create_dir_all(&agents_dir).unwrap();
            std::fs::write(
                agents_dir.join("advisor.toml"),
                "[advisor]\nenabled = false\n",
            )
            .unwrap();

            let cfg = AgentConfig::load_with_global(Some(project.to_str().unwrap())).unwrap();
            assert!(!cfg.advisor.enabled(), "project 未声明时 global 层应生效");
        });
        let _ = std::fs::remove_dir_all(&project);
    }

    /// 审查参数：env > 配置 > 内置默认，且一律钳到安全区间。
    ///
    /// 这两个字段此前**没有任何**从 TOML/env 读进来的路径（只是
    /// `AdvisorReviewSettings` 上的结构体字段），实际只能改代码常量重编。
    /// advisor 审查参数用例共用的 env 锁：env 是进程级的，而单测并行跑。
    /// 少了它，设过 env 的用例会污染只读配置的用例（实测就是这么挂的）。

    #[test]
    fn advisor_review_settings_are_layered_and_clamped() {
        let _guard = crate::test_util::env_lock();
        // 本用例只验「配置 → 生效值」，env 必须先清干净。
        unsafe {
            std::env::remove_var("LATTE_AGENT_ADVISOR_REVIEW_TIMEOUT_SECS");
            std::env::remove_var("LATTE_AGENT_ADVISOR_RETURN_MAX_REDO");
        }
        use crate::advisor_monitor::{
            DEFAULT_DELEGATE_REVIEW_TIMEOUT_SECS, DEFAULT_RETURN_MAX_REDO, MAX_RETURN_REDO,
        };
        // 未配置 → 内置默认
        let bare = AdvisorSection::default();
        assert!(bare.is_unset(), "空段必须判为未配置");
        assert_eq!(
            bare.delegate_review_timeout_secs(),
            DEFAULT_DELEGATE_REVIEW_TIMEOUT_SECS
        );
        assert_eq!(bare.return_max_redo(), DEFAULT_RETURN_MAX_REDO);

        // 配置值生效
        let configured = AdvisorSection {
            enabled: None,
            delegate_review_timeout_secs: Some(180),
            return_max_redo: Some(2),
        };
        assert!(
            !configured.is_unset(),
            "只配了审查参数（enabled 为 None）也必须算已配置——否则分层合并会\
             把整段丢掉，配置静默失效"
        );
        assert_eq!(configured.delegate_review_timeout_secs(), 180);
        assert_eq!(configured.return_max_redo(), 2);
        assert_eq!(
            configured.review_settings().delegate_review_timeout_secs,
            180
        );

        // 越界一律钳住：配错不会把行为推到极端
        let insane = AdvisorSection {
            enabled: None,
            delegate_review_timeout_secs: Some(0),
            return_max_redo: Some(99),
        };
        assert_eq!(insane.delegate_review_timeout_secs(), 10, "下限钳到 10s");
        assert_eq!(
            insane.return_max_redo(),
            MAX_RETURN_REDO,
            "重做上限钳到硬顶"
        );
        let huge = AdvisorSection {
            enabled: None,
            delegate_review_timeout_secs: Some(99_999),
            return_max_redo: None,
        };
        assert_eq!(huge.delegate_review_timeout_secs(), 600, "上限钳到 600s");
    }

    /// env 覆盖配置文件。单独一个用例并串行化——env 是进程级的，
    /// 与并行兄弟用例共享。
    #[test]
    fn advisor_review_timeout_env_overrides_config() {
        let _guard = crate::test_util::env_lock();
        let key = "LATTE_AGENT_ADVISOR_REVIEW_TIMEOUT_SECS";
        let configured = AdvisorSection {
            enabled: None,
            delegate_review_timeout_secs: Some(180),
            return_max_redo: None,
        };
        unsafe { std::env::set_var(key, "240") };
        let got = configured.delegate_review_timeout_secs();
        unsafe { std::env::remove_var(key) };
        assert_eq!(got, 240, "env 必须压过配置文件");
    }

    #[test]
    fn test_load_with_global_project_wins_on_collision() {
        // Same id `pm` in both layers: project version wins.
        let project = std::env::temp_dir().join("latte_agent_test_lwg_collision");
        let _ = std::fs::remove_dir_all(&project);
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("pm.toml"),
            r#"
[roles.pm]
id = "pm"
name = "PM_project"
category = "planning"
model_tier = "standard"
icon = "P"
"#,
        )
        .unwrap();

        with_latte_home(|home| {
            let agents_dir = home.join("agents.d");
            std::fs::create_dir_all(&agents_dir).unwrap();
            std::fs::write(
                agents_dir.join("pm.toml"),
                r#"
[roles.pm]
id = "pm"
name = "PM_global"
category = "planning"
model_tier = "standard"
icon = "G"
"#,
            )
            .unwrap();

            let cfg = AgentConfig::load_with_global(Some(project.to_str().unwrap())).unwrap();
            assert_eq!(cfg.roles.len(), 1);
            assert_eq!(cfg.roles["pm"].name, "PM_project");
            assert_eq!(cfg.roles["pm"].icon, "P");
        });
        let _ = std::fs::remove_dir_all(&project);
    }
    // ── load_with_global: built-in role fallback ──────────────────────

    #[test]
    fn test_load_with_global_falls_back_to_builtin_roles() {
        // No project, no global: only built-ins. We redirect
        // LATTE_HOME to a temp dir with no `agents.d/`, so the
        // global layer is empty, and the project path is a
        // nonexistent path so the project layer is also empty.
        with_latte_home(|_home| {
            let cfg = AgentConfig::load_with_global(Some(
                "/tmp/__definitely_nonexistent_for_builtin_test__",
            ))
            .unwrap();
            // We expect at minimum the 11 built-ins.
            for id in [
                "pm",
                "architect",
                "programmer",
                "tester",
                "reviewer",
                "devops",
                "security",
                "designer",
                "tech_writer",
                "manager",
                "advisor",
            ] {
                assert!(cfg.roles.contains_key(id), "missing built-in role '{id}'");
            }
        });
    }

    #[test]
    fn test_load_with_global_does_not_override_existing_with_builtin() {
        // Project defines `pm`; the built-in should NOT clobber it
        // (project-wins is the invariant). Built-ins are only used
        // when no layer contributes any role.
        let tmp = std::env::temp_dir().join("latte_agent_test_builtin_no_override");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(
            tmp.join("pm.toml"),
            r#"
[roles.pm]
id = "pm"
name = "PM_project_custom"
category = "planning"
model_tier = "standard"
icon = "P"
"#,
        )
        .unwrap();
        with_latte_home(|_home| {
            let cfg = AgentConfig::load_with_global(Some(tmp.to_str().unwrap())).unwrap();
            assert_eq!(cfg.roles["pm"].name, "PM_project_custom");
        });
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_load_with_global_only_global() {
        with_latte_home(|home| {
            let agents_dir = home.join("agents.d");
            std::fs::create_dir_all(&agents_dir).unwrap();
            std::fs::write(
                agents_dir.join("pm.toml"),
                r#"
[roles.pm]
id = "pm"
name = "PM_global_only"
category = "planning"
model_tier = "standard"
icon = "G"
"#,
            )
            .unwrap();

            let cfg = AgentConfig::load_with_global(None).unwrap();
            assert_eq!(cfg.roles.len(), 1);
            assert_eq!(cfg.roles["pm"].name, "PM_global_only");
        });
    }

    // ── agents_dir layout fallback (.latte/agents vs .latte/agents.d) ─

    /// Project `agents/` (no `.d`) is the layout this project ships.
    /// When that directory exists, `ConfigLayer::Project.agents_dir()`
    /// must return it directly so `load_with_global(None)` picks the
    /// roles up automatically.
    #[test]
    fn test_project_agents_dir_prefers_no_d_layout() {
        // Serialise on the same env lock `with_latte_home` uses, so
        // our `set_current_dir` + `LATTE_HOME` mutations can't race
        // with the other load_with_global tests.
        let _env_guard = crate::test_util::env_lock();

        let project = std::env::temp_dir().join("latte_agent_test_no_d_layout");
        let _ = std::fs::remove_dir_all(&project);
        let agents = project.join(".latte").join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("quick.toml"),
            r#"
[roles.quick]
id = "quick"
name = "Quick"
category = "execution"
model_tier = "budget"
icon = "Q"
"#,
        )
        .unwrap();

        // Cwd guard — restores cwd on Drop, even if assertions panic.
        let prev_cwd = std::env::current_dir().unwrap();
        struct CwdGuard(std::path::PathBuf);
        impl Drop for CwdGuard {
            fn drop(&mut self) {
                let _ = std::env::set_current_dir(&self.0);
            }
        }
        let _cwd_guard = CwdGuard(prev_cwd);
        std::env::set_current_dir(&project).unwrap();

        let resolved = ConfigLayer::Project
            .agents_dir()
            .expect("project agents_dir should resolve");
        assert!(
            resolved.ends_with("agents"),
            "expected no-`.d` layout, got {}",
            resolved.display()
        );
        assert!(
            resolved.is_dir(),
            "{} should be a directory",
            resolved.display()
        );

        let _ = std::fs::remove_dir_all(&project);
    }

    /// When only `.latte/agents.d/` exists, `agents_dir()` must still
    /// return that path so legacy projects keep working.
    #[test]
    fn test_project_agents_dir_falls_back_to_d_layout() {
        let _env_guard = crate::test_util::env_lock();

        let project = std::env::temp_dir().join("latte_agent_test_d_layout_only");
        let _ = std::fs::remove_dir_all(&project);
        let agents_d = project.join(".latte").join("agents.d");
        std::fs::create_dir_all(&agents_d).unwrap();
        std::fs::write(
            agents_d.join("pm.toml"),
            r#"
[roles.pm]
id = "pm"
name = "PM"
category = "planning"
model_tier = "standard"
icon = "P"
"#,
        )
        .unwrap();

        let prev_cwd = std::env::current_dir().unwrap();
        struct CwdGuard(std::path::PathBuf);
        impl Drop for CwdGuard {
            fn drop(&mut self) {
                let _ = std::env::set_current_dir(&self.0);
            }
        }
        let _cwd_guard = CwdGuard(prev_cwd);
        std::env::set_current_dir(&project).unwrap();

        let resolved = ConfigLayer::Project
            .agents_dir()
            .expect("project agents_dir should resolve");
        assert!(
            resolved.ends_with("agents.d"),
            "expected `.d` fallback, got {}",
            resolved.display()
        );
        assert!(
            resolved.is_dir(),
            "{} should be a directory",
            resolved.display()
        );

        let _ = std::fs::remove_dir_all(&project);
    }

    /// End-to-end: with `.latte/agents/quick.toml` present, calling
    /// `load_with_global(None)` from the project root must surface the
    /// `quick` role — this is what `latte-agent chat -r quick` relies on.
    #[test]
    fn test_load_with_global_picks_up_no_d_project_layout() {
        let _env_guard = crate::test_util::env_lock();

        let project = std::env::temp_dir().join("latte_agent_test_no_d_load");
        let _ = std::fs::remove_dir_all(&project);
        let agents = project.join(".latte").join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("quick.toml"),
            r#"
[roles.quick]
id = "quick"
name = "Quick"
category = "execution"
model_tier = "budget"
icon = "Q"
"#,
        )
        .unwrap();

        let prev_cwd = std::env::current_dir().unwrap();
        struct CwdGuard(std::path::PathBuf);
        impl Drop for CwdGuard {
            fn drop(&mut self) {
                let _ = std::env::set_current_dir(&self.0);
            }
        }
        let _cwd_guard = CwdGuard(prev_cwd);
        std::env::set_current_dir(&project).unwrap();

        // `None` forces the loader to use `ConfigLayer::Project.agents_dir()`.
        let cfg = AgentConfig::load_with_global(None).unwrap();
        assert!(
            cfg.roles.contains_key("quick"),
            "expected `quick` role from .latte/agents/ to be loaded; got: {:?}",
            cfg.roles.keys().collect::<Vec<_>>()
        );
        assert_eq!(cfg.roles["quick"].name, "Quick");
        assert_eq!(cfg.roles["quick"].icon, "Q");

        let _ = std::fs::remove_dir_all(&project);
    }

    /// `[token_budget]` 必须能被**目录加载**（`.latte/agents/*.toml`）拾取。
    ///
    /// 只测 `parse()` 不够：这个项目的 agent 配置走的是目录合并
    /// （`load_dir` → project 层 → global 层），少接一个 `is_unset` 判断
    /// 就是「配置写了但不生效」的静默失效——compaction 段踩过这个坑，
    /// 注释里也点名了。这里真建一个目录走完整条合并路径。
    #[test]
    fn token_budget_section_merges_through_directory_load() {
        let _env_guard = crate::test_util::ENV_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap();
        unsafe {
            std::env::remove_var("LATTE_AGENT_TOKEN_BUDGET_ENFORCE");
        }

        let project = std::env::temp_dir().join("latte_agent_test_token_budget_dir");
        let _ = std::fs::remove_dir_all(&project);
        let agents = project.join(".latte").join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("token_budget.toml"),
            "[token_budget]\nenforce = true\nworkflow_limit = 4242\n",
        )
        .unwrap();

        let prev_cwd = std::env::current_dir().unwrap();
        struct CwdGuard(std::path::PathBuf);
        impl Drop for CwdGuard {
            fn drop(&mut self) {
                let _ = std::env::set_current_dir(&self.0);
            }
        }
        let _cwd_guard = CwdGuard(prev_cwd);
        std::env::set_current_dir(&project).unwrap();

        let cfg = AgentConfig::load_with_global(Some(agents.to_str().unwrap())).unwrap();
        assert_eq!(
            cfg.token_budget.enforce,
            Some(true),
            "目录加载必须把 [token_budget] 带进来"
        );
        assert_eq!(cfg.token_budget.workflow_limit, Some(4_242));
        assert!(cfg.token_budget.enforce(), "生效值也必须是开");

        let _ = std::fs::remove_dir_all(&project);
    }

    /// 拦截开关与分母：默认只统计，env 与配置都能改，且**默认值本身**
    /// 必须是「不拦」——这条守的是 jemalloc 那次整步判死的修复面。
    #[test]
    fn token_budget_section_defaults_to_accounting_only() {
        let _guard = ADVISOR_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::remove_var("LATTE_AGENT_TOKEN_BUDGET_ENFORCE");
        }
        // 未配置 → 不拦（这是默认契约，不是「恰好 false」）
        let bare = TokenBudgetSection::default();
        assert!(bare.is_unset());
        assert!(!bare.enforce(), "默认必须只统计，不拿预算当刹车");

        // 只配分母也必须算「已配置」，否则分层合并会把整段丢掉。
        let partial = TokenBudgetSection {
            enforce: None,
            workflow_limit: Some(2_000_000),
        };
        assert!(!partial.is_unset(), "只配 workflow_limit 也是已配置");
        assert!(!partial.enforce(), "未显式开就等于不拦");

        let on = TokenBudgetSection {
            enforce: Some(true),
            workflow_limit: None,
        };
        assert!(on.enforce());

        // env 优先级最高：临时压测不改文件。
        unsafe { std::env::set_var("LATTE_AGENT_TOKEN_BUDGET_ENFORCE", "0") };
        assert!(!on.enforce(), "env 0 必须压过配置里的 true");
        unsafe { std::env::set_var("LATTE_AGENT_TOKEN_BUDGET_ENFORCE", "1") };
        assert!(bare.enforce(), "env 1 必须压过配置里的未配置");
        unsafe {
            std::env::remove_var("LATTE_AGENT_TOKEN_BUDGET_ENFORCE");
        }
    }

    /// `[token_budget]` 必须能从任意 agents 文件里解析出来，并且不改动
    /// 其它段的既有语义（`deny_unknown_fields` 类误伤会在这里暴露）。
    #[test]
    fn token_budget_section_parses_from_toml_alongside_other_sections() {
        let raw = r#"
[token_budget]
enforce = true
workflow_limit = 500000

[compaction]
enabled = false
"#;
        let cfg = AgentConfig::parse(raw).unwrap();
        assert_eq!(cfg.token_budget.enforce, Some(true));
        assert_eq!(cfg.token_budget.workflow_limit, Some(500_000));
        assert_eq!(cfg.compaction.enabled, Some(false));

        // 只写 compaction 的文件里，token_budget 必须保持「未配置」，
        // 这样分层合并才会去取 global 层的值。
        let only_compaction = AgentConfig::parse("[compaction]\nenabled = false\n").unwrap();
        assert!(only_compaction.token_budget.is_unset());
    }
}
