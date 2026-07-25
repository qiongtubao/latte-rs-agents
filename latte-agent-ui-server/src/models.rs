//! Model catalog 管理：项目/全局双层 CRUD。
//! 持久化（任一格式都接受，详见 [`ModelsState::load_dir`]）：
//!   - 项目：`.latte/models.d/*.toml` / `*.yaml`（按厂商拆分或多 model 集中写均可）
//!   - 全局：`~/.latte/models.d/*.toml` / `*.yaml`（推荐按厂商拆分）
//!
//! 项目优先于全局：与现有 `config_layer::load` 的"项目优先"语义一致。
//! 写入策略：先写项目目录；若项目路径不可用则写全局。
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use latte_agent_core::config::{AgentConfig, ModelDef};
use latte_agent_core::model_resolver::ModelTier;
use serde::{Deserialize, Serialize};
/// `composite_key` 路径安全转换：`openai/gpt-4o` → `openai__gpt-4o.toml`。
pub fn key_to_filename(key: &str) -> String {
    let safe = key.replace(['/', '\\', ':'], "__");
    // 检查是否以 .toml 结尾（有些 key 如 "glm-5.2" 自带的 `.2` 不是扩展名）
    if safe.ends_with(".toml") {
        safe
    } else {
        format!("{safe}.toml")
    }
}

/// `openai__gpt-4o.toml` → `openai/gpt-4o`。
pub fn filename_to_key(filename: &str) -> Option<String> {
    let stem = std::path::Path::new(filename).file_stem()?.to_str()?;
    if !stem.contains("__") {
        return None;
    }
    let (provider, model) = stem.split_once("__")?;
    if provider.is_empty() || model.is_empty() {
        return None;
    }
    Some(format!("{provider}/{model}"))
}

/// 合并后的 models catalog 状态。
#[derive(Default)]
pub struct ModelsState {
    /// `composite_key -> ModelDef`。项目优先：项目文件覆盖全局。
    pub merged: BTreeMap<String, ModelDef>,
    /// 记录每个 key 来自哪个文件（项目 / 全局），用于 UI 提示。
    pub sources: BTreeMap<String, ModelSource>,
    /// 记录每个 key 实际所在的文件绝对路径（disk 扫描结果）。
    /// UI 用它显示「文件位置」和定位保存目标。
    pub paths: BTreeMap<String, PathBuf>,
    /// 全局 tier 映射：`tier -> composite_key`。
    /// Per-role tier 覆盖：`role_id -> (tier -> composite_key)`。
    pub role_tiers: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelSource {
    Project,
    Global,
}

impl ModelsState {
    /// 从项目目录 + 全局目录加载并合并。`load_with_global` 风格的
    /// "项目优先" 语义。
    pub fn load(project_models_dir: &Path, global_models_dir: &Path) -> Result<Self> {
        let mut state = Self::default();
        // 先加载全局（低优先级）
        if global_models_dir.is_dir() {
            Self::load_dir(global_models_dir, ModelSource::Global, &mut state)?;
        }
        // 再加载项目（高优先级，覆盖同名 key）
        if project_models_dir.is_dir() {
            Self::load_dir(project_models_dir, ModelSource::Project, &mut state)?;
        }
        Ok(state)
    }

    /// 扫描目录，读出所有 model 定义。可接受三种文件格式（按优先级尝试）：
    ///   1. `Vec<ModelDef>`（TOML `[[models]]` 数组 / YAML 顶层列表，常见于
    ///      `~/.latte/models.d/*.toml` 按厂商拆分的文件 —— 单文件可声明多个 model，
    ///      model 的 `provider` / `model_name` 字段决定 composite_key）。
    ///   2. `AgentConfig`（TOML `[[models.models]]` 嵌套在 `[models]` 下，
    ///      或 YAML 等价的 `models: [...]` 嵌套格式 —— 复用了项目级配置 schema）。
    ///   3. 单个 `ModelDef`（flat TOML / YAML，老的 per-model 单文件格式，
    ///      `write_project` 仍按此格式写出，所以必须保留兼容）。
    ///
    /// 解析失败的扩展名/文件会静默跳过；其它错误通过 `?` 向上抛。
    /// 找到的 model 通过 `def.composite_key()` 入库，不再依赖文件名命名约定。
    fn load_dir(dir: &Path, source: ModelSource, state: &mut Self) -> Result<()> {
        for entry in std::fs::read_dir(dir)
            .map_err(|e| anyhow!("read_dir {}: {e}", dir.display()))?
            .flatten()
        {
            let p = entry.path();
            if !p.is_file() {
                continue;
            }
            let ext = match p.extension().and_then(|s| s.to_str()) {
                Some(e) if matches!(e, "toml" | "yaml" | "yml") => e,
                _ => continue,
            };
            let content = match std::fs::read_to_string(&p) {
                Ok(c) => c,
                Err(_) => continue,
            };
            // 先尝试批量，再尝试单条；单文件可能只声明 1 个 model。
            for def in parse_models_in_file(&content, ext) {
                let key = format!("{}/{}", def.provider, def.name);
                if key.is_empty() || !key.contains('/') {
                    // composite_key 非法（provider 或 model_name 空）→ 跳过这条，
                    // 避免污染 catalog。其它同文件 model 仍会入库。
                    continue;
                }
                state.merged.insert(key.clone(), def);
                state.sources.insert(key.clone(), source);
                // 记录文件绝对路径，UI「文件位置」展示用。`read_dir` 返回的
                // path 在不同平台可能是相对路径，canonicalize 兜底拿绝对路径；
                // 失败时退回原值（仍可显示，只是路径可能是相对的）。
                let abs = p.canonicalize().unwrap_or_else(|_| p.clone());
                state.paths.insert(key, abs);
            }
        }
        Ok(())
    }

    /// 列出所有 model（按 composite_key 排序）。
    pub fn list(&self) -> Vec<ModelDef> {
        self.merged.values().cloned().collect()
    }

    /// 写一个 ModelDef 到项目目录（覆盖同名文件）。
    pub fn write_project(
        project_models_dir: &Path,
        key: &str,
        def: &ModelDef,
    ) -> Result<PathBuf> {
        std::fs::create_dir_all(project_models_dir)
            .map_err(|e| anyhow!("mkdir {}: {e}", project_models_dir.display()))?;
        let path = project_models_dir.join(key_to_filename(key));
        let content = toml::to_string_pretty(def)
            .map_err(|e| anyhow!("serialize {key}: {e}"))?;
        std::fs::write(&path, content)
            .map_err(|e| anyhow!("write {}: {e}", path.display()))?;
        Ok(path)
    }

    /// 从项目目录删除一个 model 文件。
    pub fn delete_project(project_models_dir: &Path, key: &str) -> Result<bool> {
        let path = project_models_dir.join(key_to_filename(key));
        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|e| anyhow!("remove {}: {e}", path.display()))?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// 写一个 ModelDef 到全局目录 `~/.latte/models.d/<provider>__<id>.toml`。
    /// 用于「保存到全局」按钮 —— 把当前 model 配置提升到全局层，
    /// 与 `GlobalConfig::load_default` 后续加载行为一致。
    pub fn write_global(
        global_models_dir: &Path,
        key: &str,
        def: &ModelDef,
    ) -> Result<PathBuf> {
        std::fs::create_dir_all(global_models_dir)
            .map_err(|e| anyhow!("mkdir {}: {e}", global_models_dir.display()))?;
        let path = global_models_dir.join(key_to_filename(key));
        let content = toml::to_string_pretty(def)
            .map_err(|e| anyhow!("serialize {}: {}", key, e))?;
        std::fs::write(&path, content)
            .map_err(|e| anyhow!("write {}: {}", path.display(), e))?;
        Ok(path)
    }

    /// 全局目录路径解析（`~/.latte/models.d/`），与 `GlobalConfig::global_dir`
    /// 同源，但放在这里避免 ui-server 直接依赖 core 的 internal state。
    pub fn global_models_dir() -> PathBuf {
        latte_agent_core::config::ConfigLayer::Global
            .root_dir()
            .map(|d| d.join("models.d"))
            .unwrap_or_else(|| {
                std::env::var("HOME")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_default()
                    .join(".latte/models.d")
            })
    }
}
/// 顶层 `[[models]]` 数组的 schema。`toml::from_str::<Vec<ModelDef>>`
/// 不能直接吃这种语法（serde 报错 "expected a sequence"），必须有一个
/// 字段名 = `models` 的父 struct 来接住 `[[models]]` 这层命名。
/// 这是 `GlobalConfig::RouterStyleDoc` 的最小复刻 —— 那个 struct 是
/// private，所以这里本地定义一份（YAML 同理）。
#[derive(Debug, Default, Deserialize)]
struct ModelsFile {
    #[serde(default)]
    models: Vec<ModelDef>,
}

/// 解析单个 model 文件内容为 model 列表（可能 0/1/N 条）。
/// 严格按 dual-schema 策略：先尝试 `ModelsFile`（`[[models]]` / 顶层
/// `models: [...]`），再回退到 `AgentConfig`（`[models]` + `[[models.models]]`），
/// 再回退到单条 `ModelDef`（legacy flat 格式 —— `write_project` 写出风格）。
/// 返回 *去重前* 的原始列表，由调用方按 `composite_key()` 入库。
fn parse_models_in_file(content: &str, ext: &str) -> Vec<ModelDef> {
    match ext {
        "toml" => {
            // 1. TOML `[[models]]` 数组（global vendor-split 风格）
            if let Ok(doc) = toml::from_str::<ModelsFile>(content) {
                if !doc.models.is_empty() {
                    return doc.models;
                }
            }
            // 2. TOML `[models]` + `[[models.models]]` 项目 schema
            if let Ok(cfg) = toml::from_str::<AgentConfig>(content) {
                if !cfg.models.models.is_empty() {
                    return cfg.models.models;
                }
            }
            // 3. flat 单 ModelDef（legacy write_project 格式）
            if let Ok(def) = toml::from_str::<ModelDef>(content) {
                return vec![def];
            }
            Vec::new()
        }
        "yaml" | "yml" => {
            // 1. YAML 顶层 `models: [...]`（router-style 裸 list）
            if let Ok(doc) = serde_yaml::from_str::<ModelsFile>(content) {
                if !doc.models.is_empty() {
                    return doc.models;
                }
            }
            // 2. YAML `models:` 嵌套在 AgentConfig schema 下
            if let Ok(cfg) = serde_yaml::from_str::<AgentConfig>(content) {
                if !cfg.models.models.is_empty() {
                    return cfg.models.models;
                }
            }
            // 3. flat 单 ModelDef
            if let Ok(def) = serde_yaml::from_str::<ModelDef>(content) {
                return vec![def];
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// 校验 ModelDef：必要字段不能为空。
pub fn validate(def: &ModelDef) -> Result<()> {
    if def.provider.is_empty() {
        return Err(anyhow!("provider is required"));
    }
    if def.name.is_empty() {
        return Err(anyhow!("id is required"));
    }
    if def.api.is_empty() {
        return Err(anyhow!("api is required (openai/anthropic)"));
    }
    if def.base_url.is_empty() {
        return Err(anyhow!("base_url is required"));
    }
    if def.context_window == 0 {
        return Err(anyhow!("context_window must be > 0"));
    }
    if def.max_tokens == 0 {
        return Err(anyhow!("max_tokens must be > 0"));
    }
    if let Some(tier) = &def.tier {
        if ModelTier::parse(tier).is_err() {
            return Err(anyhow!("invalid tier {:?} (premium/standard/budget)", tier));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_filename_round_trip() {
        assert_eq!(key_to_filename("openai/gpt-4o"), "openai__gpt-4o.toml");
        assert_eq!(filename_to_key("openai__gpt-4o.toml").as_deref(), Some("openai/gpt-4o"));
        // 边界
        assert_eq!(filename_to_key("plain.toml"), None);
        assert_eq!(filename_to_key("foo__.toml"), None);
        assert_eq!(filename_to_key("__bar.toml"), None);
    }

    #[test]
    fn validate_rejects_empty_provider() {
        let mut def = sample_def();
        def.provider = String::new();
        assert!(validate(&def).is_err());
    }

    #[test]
    fn validate_rejects_invalid_tier() {
        let mut def = sample_def();
        def.tier = Some("nonsense".into());
        assert!(validate(&def).is_err());
    }

    #[test]
    fn validate_accepts_valid_def() {
        assert!(validate(&sample_def()).is_ok());
    }

    #[test]
    fn load_merges_project_over_global() {
        let tmp = std::env::temp_dir().join(format!("latte-models-test-{}", std::process::id()));
        let proj = tmp.join("project");
        let glob = tmp.join("global");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(&glob).unwrap();

        let global_def = sample_def();
        let global_path = glob.join("openai__gpt-4o.toml");
        std::fs::write(&global_path, toml::to_string_pretty(&global_def).unwrap()).unwrap();

        // 项目覆盖 api_key
        let mut project_def = sample_def();
        project_def.api_key = "project-key".into();
        let project_path = proj.join("openai__gpt-4o.toml");
        std::fs::write(&project_path, toml::to_string_pretty(&project_def).unwrap()).unwrap();

        let state = ModelsState::load(&proj, &glob).unwrap();
        let def = state.merged.get("openai/gpt-4o").unwrap();
        assert_eq!(def.api_key, "project-key");
        assert_eq!(state.sources["openai/gpt-4o"], ModelSource::Project);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 回归：用户在 ~/.latte/models.d/*.toml 下写的 `[[models]]` 数组格式
    /// （按厂商拆分，单文件多 model）必须被 UI server 识别 —— 之前
    /// `load_dir` 要求文件名带 `__` 且把每个文件当单 ModelDef 解析，
    /// 全部全局 model 在 UI 里消失（"暂无 model"）。
    #[test]
    fn load_reads_global_array_of_tables() {
        let tmp = std::env::temp_dir().join(format!(
            "latte-models-global-aot-{}",
            std::process::id()
        ));
        let proj = tmp.join("project");
        let glob = tmp.join("global");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(&glob).unwrap();

        // 文件名不带 `__`，内容是 `[[models]]` 数组 —— 真实环境里
        // ~/.latte/models.d/deepseek.toml 就是这种格式。
        std::fs::write(
            glob.join("deepseek.toml"),
            "\
[[models]]\n\
name = \"deepseek-v4-flash\"\n\
api = \"openai\"\n\
provider = \"deepseek\"\n\
base_url = \"https://api.deepseek.com\"\n\
api_key = \"sk-global\"\n\
context_window = 1000000\n\
max_tokens = 384000\n\
\n\
[[models]]\n\
name = \"deepseek-chat\"\n\
api = \"openai\"\n\
provider = \"deepseek\"\n\
base_url = \"https://api.deepseek.com\"\n\
api_key = \"sk-global-chat\"\n\
context_window = 65536\n\
max_tokens = 8192\n\
",
        )
        .unwrap();

        let state = ModelsState::load(&proj, &glob).unwrap();

        // 两条 model 都应被发现
        let v4 = state
            .merged
            .get("deepseek/deepseek-v4-flash")
            .expect("deepseek-v4-flash must be loaded from global array-of-tables");
        assert_eq!(v4.api_key, "sk-global");
        assert_eq!(state.sources["deepseek/deepseek-v4-flash"], ModelSource::Global);

        let chat = state
            .merged
            .get("deepseek/deepseek-chat")
            .expect("deepseek-chat must be loaded from same file");
        assert_eq!(chat.api_key, "sk-global-chat");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 项目侧 `[[models.models]]` 嵌套在 `[models]` 下的格式（与项目根
    /// `.latte/models.toml` 一致）也要被识别 —— 之前的 scanner 不会
    /// 处理这种 schema。
    #[test]
    fn load_reads_project_models_models_array() {
        let tmp = std::env::temp_dir().join(format!(
            "latte-models-proj-ms-{}",
            std::process::id()
        ));
        let proj = tmp.join("project");
        let glob = tmp.join("global");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(&glob).unwrap();

        std::fs::write(
            proj.join("shared.toml"),
            "\
[models.tiers]\n\
premium = \"anthropic/claude-opus-4-20250514\"\n\
\n\
[[models.models]]\n\
name = \"claude-opus-4-20250514\"\n\
api = \"anthropic\"\n\
provider = \"anthropic\"\n\
base_url = \"https://api.anthropic.com\"\n\
api_key = \"sk-proj\"\n\
context_window = 200000\n\
max_tokens = 8192\n\
",
        )
        .unwrap();

        let state = ModelsState::load(&proj, &glob).unwrap();
        let opus = state
            .merged
            .get("anthropic/claude-opus-4-20250514")
            .expect("opus must be loaded from project [[models.models]]");
        assert_eq!(opus.api_key, "sk-proj");
        assert_eq!(
            state.sources["anthropic/claude-opus-4-20250514"],
            ModelSource::Project
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn sample_def() -> ModelDef {
        ModelDef {
            name: "gpt-4o".into(),
            api: "openai".into(),
            provider: "openai".into(),
            base_url: "https://api.openai.com".into(),
            api_key: "sk-test".into(),
            context_window: 128000,
            max_tokens: 4096,
            supports_thinking: false,
            supports_vision: true,
            cost_per_million_input: Some(2.5),
            cost_per_million_output: Some(10.0),
            tier: Some("premium".into()),
            timeout_secs: None,
        }
    }
}
