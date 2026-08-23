//! 任务类型注册表：`task_type → 默认 workflow`
//!
//! 可配置：权威默认在 `config/task_types.toml`（随包编译进 binary，
//! `include_str!`），项目层可在 `<cwd>/.latte/task_types.toml` 按同
//! 格式覆盖/增补（同名 type 覆盖，未声明的不动）。
//!
//! 规则（与 `tasks.rs` 约定一致）：
//! - 未填 `task_type` 或 `workflow` → 普通 manager 会话（不走 workflow）
//! - 填 `task_type` 未填 `workflow` → 按本表 `default_workflow` 推导
//! - 显式 `workflow` → 覆盖类型默认

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskTypeDef {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub default_workflow: String,
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub icon: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskTypeEntry {
    pub id: String,
    #[serde(flatten)]
    pub def: TaskTypeDef,
}

#[derive(Debug, Clone, Default)]
pub struct TaskTypeRegistry {
    pub types: HashMap<String, TaskTypeDef>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RawRegistry {
    #[serde(default)]
    types: HashMap<String, TaskTypeDef>,
}

const BUNDLED_TOML: &str = include_str!("../../config/task_types.toml");

fn project_path(cwd: &Path) -> PathBuf {
    cwd.join(".latte").join("task_types.toml")
}

fn parse_toml(s: &str) -> Option<RawRegistry> {
    toml::from_str(s).ok()
}

impl TaskTypeRegistry {
    /// 加载注册表：bundled 默认 + 项目层 `<cwd>/.latte/task_types.toml` 覆盖。
    pub fn load(cwd: &Path) -> Self {
        let mut reg = Self::from_toml_str(BUNDLED_TOML).unwrap_or_default();
        let pp = project_path(cwd);
        if let Ok(raw) = std::fs::read_to_string(&pp) {
            if let Some(overlay) = Self::from_toml_str(&raw) {
                for (k, v) in overlay.types {
                    reg.types.insert(k, v);
                }
            }
        }
        if reg.types.is_empty() {
            let alt = cwd.join("config").join("task_types.toml");
            if let Ok(raw) = std::fs::read_to_string(&alt) {
                if let Some(loaded) = Self::from_toml_str(&raw) {
                    reg = loaded;
                }
            }
        }
        reg
    }

    fn from_toml_str(s: &str) -> Option<Self> {
        let raw: RawRegistry = parse_toml(s)?;
        Some(Self { types: raw.types })
    }

    pub fn is_known(&self, t: &str) -> bool {
        self.types.contains_key(t)
    }

    pub fn default_workflow(&self, t: &str) -> Option<String> {
        let def = self.types.get(t)?;
        let w = def.default_workflow.trim();
        if w.is_empty() { None } else { Some(w.to_string()) }
    }

    pub fn entries(&self) -> Vec<TaskTypeEntry> {
        let mut v: Vec<TaskTypeEntry> = self.types.iter().map(|(k, v)| TaskTypeEntry { id: k.clone(), def: v.clone() }).collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    pub fn list(&self) -> Vec<(String, TaskTypeDef)> {
        let mut v: Vec<(String, TaskTypeDef)> = self.types.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// 仅加载项目层覆盖（不含 bundled），用于持久化时只写增量。
    fn load_project_only(cwd: &Path) -> RawRegistry {
        let pp = project_path(cwd);
        if let Ok(raw) = std::fs::read_to_string(&pp) {
            if let Some(r) = parse_toml(&raw) { return r; }
        }
        RawRegistry { types: HashMap::new() }
    }

    fn save_project(cwd: &Path, raw: &RawRegistry) -> Result<(), String> {
        let pp = project_path(cwd);
        if let Some(parent) = pp.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        let s = toml::to_string_pretty(raw).map_err(|e| format!("serialize: {e}"))?;
        // 原子写
        let tmp = pp.with_extension("toml.tmp");
        std::fs::write(&tmp, s.as_bytes()).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &pp).map_err(|e| format!("rename {}: {e}", pp.display()))?;
        Ok(())
    }
}

/// 校验 id：仅允许小写字母、数字、_、-，以字母开头，长度 1..32
pub fn validate_id(id: &str) -> Result<(), String> {
    let s = id.trim();
    if s.is_empty() { return Err("id 不能为空".into()); }
    if s.len() > 32 { return Err("id 长度不能超过 32".into()); }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_lowercase() { return Err("id 必须以小写字母开头".into()); }
    for c in s.chars() {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-') {
            return Err(format!("id 含非法字符 '{c}'（仅允许 a-z 0-9 _ -）"));
        }
    }
    Ok(())
}

pub fn upsert_task_type(cwd: &Path, entry: TaskTypeEntry) -> Result<TaskTypeEntry, String> {
    validate_id(&entry.id)?;
    if entry.def.label.trim().is_empty() {
        return Err("label 不能为空".into());
    }
    let wf = entry.def.default_workflow.trim();
    if !wf.is_empty() && !crate::workflows::exists(cwd, wf) {
        return Err(format!("unknown workflow '{wf}'"));
    }
    let mut raw = TaskTypeRegistry::load_project_only(cwd);
    raw.types.insert(entry.id.clone(), entry.def.clone());
    TaskTypeRegistry::save_project(cwd, &raw)?;
    Ok(entry)
}

pub fn delete_task_type(cwd: &Path, id: &str) -> Result<(), String> {
    let mut raw = TaskTypeRegistry::load_project_only(cwd);
    // 若项目层没有该 id，检查是否仅为 bundled 默认——也允许通过在项目层写入"删除标记"来遮蔽？
    // 简化：若项目层不存在但 bundled 存在，则在项目层写入一个空 default_workflow 的"遮蔽"不够直观；
    // 这里改为：若 bundled 存在而项目层不存在，创建一个 tombstone 机制不做，直接报错提示用户在项目层无法删除内置类型，
    // 改为允许删除：我们在项目层记录删除——通过保存一个不含该 id 的全量覆盖？实际上 bundled 无法删除，
    // 最简单：若仅 bundled 有，则提示不能删除内置类型，请通过覆盖 default_workflow 为空来"禁用"。
    // 但为满足"可删除"的 UX，我们允许删除：若 bundled 存在，删除时在项目层写入一个覆盖，将其标记为"已删除"——
    // 实现为：在项目层写入一个特殊空类型然后 load 时跳过？更简单：直接返回错误，让用户改为编辑。
    // 这里放宽：无论哪层有，只要最终 registry 包含该 id，就允许通过"在项目层删除后，下次 load 时若 bundled 还有则仍会回来"
    // 为避免这个回弹，我们需要在项目层维护一个删除集合；简化：直接操作项目层文件——若 bundled 有而项目层无，插入一个"空"并立刻删？不行。
    // 折中：允许删除项目层定义的；内置类型删除时报错。
    let reg = TaskTypeRegistry::load(cwd);
    if !reg.is_known(id) {
        return Err(format!("task_type '{id}' 不存在"));
    }
    if !raw.types.contains_key(id) {
        // 仅 bundled 存在
        return Err(format!("内置类型 '{id}' 不能删除，可将其 default_workflow 设为空来禁用"));
    }
    raw.types.remove(id);
    TaskTypeRegistry::save_project(cwd, &raw)?;
    Ok(())
}

/// 任务的实际执行工作流（显式优先，用于 rework 强绑/兼容路径）：
/// 显式 workflow 优先，否则按 task_type 的默认推导。返回 `None` → manager。
pub fn effective_workflow(
    task_type: Option<&str>,
    explicit_workflow: Option<&str>,
    registry: &TaskTypeRegistry,
) -> Option<String> {
    if let Some(w) = explicit_workflow.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(w.to_string());
    }
    let t = task_type.map(str::trim).filter(|s| !s.is_empty())?;
    registry.default_workflow(t)
}

/// 类型优先推导（redo/看板默认路径）：task_type 的默认 workflow 优先，
/// 显式 workflow 仅作兜底（task_type 无默认时才用）。用户改类型=希望跑
/// 该类型的流程，显式字段不应覆盖。返回 `None` → manager。
pub fn effective_workflow_type_first(
    task_type: Option<&str>,
    explicit_workflow: Option<&str>,
    registry: &TaskTypeRegistry,
) -> Option<String> {
    let t = task_type.map(str::trim).filter(|s| !s.is_empty());
    if let Some(t) = t {
        if let Some(w) = registry.default_workflow(t) {
            return Some(w);
        }
    }
    explicit_workflow
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_loads() {
        let r = TaskTypeRegistry::from_toml_str(BUNDLED_TOML).expect("bundled parse");
        assert!(r.is_known("feature"));
        assert_eq!(r.default_workflow("feature").as_deref(), Some("tdd_development"));
        assert_eq!(r.default_workflow("chore"), None);
    }

    #[test]
    fn effective_prefers_explicit() {
        let r = TaskTypeRegistry::from_toml_str(BUNDLED_TOML).unwrap();
        assert_eq!(effective_workflow(Some("feature"), Some("bug_triage"), &r).as_deref(), Some("bug_triage"));
        assert_eq!(effective_workflow(Some("feature"), None, &r).as_deref(), Some("tdd_development"));
        assert_eq!(effective_workflow(None, None, &r), None);
        assert_eq!(effective_workflow(Some("chore"), None, &r), None);
    }

    #[test]
    fn validate_id_ok() {
        assert!(validate_id("feature").is_ok());
        assert!(validate_id("my_type-1").is_ok());
        assert!(validate_id("1bad").is_err());
        assert!(validate_id("Bad").is_err());
    }
}
