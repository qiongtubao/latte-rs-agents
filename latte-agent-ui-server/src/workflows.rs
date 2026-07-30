//! Workflow 管理：列表/读取/新建/更新/删除/校验 + UI 测试运行状态。
//!
//! 文件格式与解析复用 `latte_agent_core::workflow`（`WorkflowDef`）。
//! 解析顺序与 core 一致：项目 `<cwd>/.latte/workflows.d/` 优先，
//! 全局 `$LATTE_HOME/workflows.d/`（或 `~/.latte/workflows.d/`）兜底，
//! 同名时项目副本遮蔽全局副本。
//!
//! 协议无关入口在 `crate::api`，HTTP 壳在 `crate::handlers`；
//! 实际执行引擎是 `latte_agent_core::workflow::run_workflow`。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use latte_agent_core::config::AgentConfig;
use latte_agent_core::controller::ChatEvent;
use latte_agent_core::workflow::{WorkflowDef, WorkflowStepDef};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

// ─── Types ────────────────────────────────────────────────────────

/// 一个 workflow 文件的来源：项目层（可编辑/删除）或全局层（只读，
/// 项目层 update 会生成遮蔽副本）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowSource {
    Project,
    Global,
}

/// `GET /api/workflows` 的列表条目。
#[derive(Debug, Clone, Serialize)]
pub struct WorkflowSummary {
    pub name: String,
    pub description: String,
    pub steps_count: usize,
    pub source: WorkflowSource,
    pub file_path: String,
    /// 触发该 workflow 的斜杠命令（如 `/plan`），无则为 null。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// UI 表单模型：统一成 `speakers` + `prompt` 形态。加载 `role`/`task`
/// 形态的 def 时映射为 `speakers = [role]`、`prompt = task`；保存时
/// 始终写 `speakers` + `prompt`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepForm {
    pub id: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub speakers: Vec<String>,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub output_key: Option<String>,
}

/// 新建/更新/校验的请求体（也是 detail 响应的表单部分）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowForm {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// 触发该 workflow 的斜杠命令（如 `/plan`）。
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub max_rounds: Option<usize>,
    #[serde(default)]
    pub steps: Vec<StepForm>,
}

/// `GET /api/workflows/:name` 的响应：表单字段 + 来源 + 原始文件内容。
#[derive(Debug, Clone, Serialize)]
pub struct WorkflowDetail {
    pub name: String,
    pub description: String,
    pub max_rounds: Option<usize>,
    pub steps: Vec<StepForm>,
    pub source: WorkflowSource,
    pub file_path: String,
    pub raw_toml: String,
    /// 触发该 workflow 的斜杠命令（如 `/plan`），无则为 null。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// `POST /api/workflows/validate` 的响应。
#[derive(Debug, Clone, Serialize)]
pub struct ValidateResponse {
    pub ok: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

// ─── Dirs ─────────────────────────────────────────────────────────

pub fn project_dir(cwd: &Path) -> PathBuf {
    cwd.join(".latte").join("workflows.d")
}

fn global_dir() -> Option<PathBuf> {
    std::env::var_os("LATTE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".latte")))
        .map(|h| h.join("workflows.d"))
}

/// (dir, source) 列表，项目在前（与 core 的解析顺序一致）。
fn search_dirs(cwd: &Path) -> Vec<(PathBuf, WorkflowSource)> {
    let mut dirs = vec![(project_dir(cwd), WorkflowSource::Project)];
    if let Some(g) = global_dir() {
        dirs.push((g, WorkflowSource::Global));
    }
    dirs
}

/// 项目层或全局层是否存在该名字的 workflow。
pub fn exists(cwd: &Path, name: &str) -> bool {
    search_dirs(cwd)
        .iter()
        .any(|(dir, _)| dir.join(format!("{name}.toml")).exists())
}

// ─── Form <-> Def mapping ─────────────────────────────────────────

fn step_to_form(s: &WorkflowStepDef) -> StepForm {
    StepForm {
        id: s.id.clone(),
        description: s.description.clone(),
        // `role`/`task` 形态 → speakers=[role] / prompt=task。
        speakers: s.roles(),
        prompt: s.task_text().to_string(),
        output_key: s.output_key.clone(),
    }
}

/// def → 表单（加载路径；`role`/`task` 被归一成 `speakers`/`prompt`）。
pub fn form_from_def(wf: &WorkflowDef) -> WorkflowForm {
    WorkflowForm {
        name: wf.name.clone(),
        description: wf.description.clone(),
        max_rounds: wf.max_rounds,
        steps: wf.steps.iter().map(step_to_form).collect(),
        command: wf.command.clone(),
    }
}

/// 表单 → 内存 def（测试运行未保存的编辑；始终 `speakers`/`prompt` 形态）。
pub fn def_from_form(form: &WorkflowForm) -> WorkflowDef {
    WorkflowDef {
        name: form.name.clone(),
        command: form.command.clone(),
        description: form.description.clone(),
        max_rounds: form.max_rounds,
        steps: form
            .steps
            .iter()
            .map(|s| WorkflowStepDef {
                id: s.id.clone(),
                description: s.description.clone(),
                role: None,
                task: String::new(),
                speakers: s.speakers.clone(),
                prompt: s.prompt.clone(),
                output_key: s.output_key.clone(),
                depends_on: vec![],
                max_retries: 0,
                loop_until: None,
                max_iterations: None,
            })
            .collect(),
    }
}

// ─── Read ─────────────────────────────────────────────────────────

/// 列出全部 workflow：项目 + 全局，同名项目遮蔽全局，按名字排序。
/// 解析失败的文件不使整个列表失败——条目照常列出，description 带
/// 解析错误说明（编辑器里能看到并修）。
pub fn list(cwd: &Path) -> Vec<WorkflowSummary> {
    let mut out: Vec<WorkflowSummary> = Vec::new();
    for (dir, source) in search_dirs(cwd) {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("toml") {
                continue;
            }
            let name = match path.file_stem().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            let (description, steps_count, command) = match std::fs::read_to_string(&path) {
                Ok(raw) => match toml::from_str::<WorkflowDef>(&raw) {
                    Ok(wf) => (wf.description, wf.steps.len(), wf.command),
                    Err(e) => (format!("(parse error: {e})"), 0, None),
                },
                Err(_) => (String::new(), 0, None),
            };
            out.push(WorkflowSummary {
                name,
                description,
                steps_count,
                source,
                file_path: path.display().to_string(),
                command,
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// 读取单个 workflow（项目优先，全局兜底），带原始文件内容。
pub fn get(cwd: &Path, name: &str) -> Result<WorkflowDetail, String> {
    for (dir, source) in search_dirs(cwd) {
        let path = dir.join(format!("{name}.toml"));
        if !path.exists() {
            continue;
        }
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let wf: WorkflowDef = toml::from_str(&raw)
            .map_err(|e| format!("invalid workflow {}: {e}", path.display()))?;
        let form = form_from_def(&wf);
        return Ok(WorkflowDetail {
            name: form.name,
            description: form.description,
            max_rounds: form.max_rounds,
            steps: form.steps,
            source,
            file_path: path.display().to_string(),
            raw_toml: raw,
            command: wf.command,
        });
    }
    Err(format!("workflow '{name}' not found"))
}

// ─── Write ────────────────────────────────────────────────────────

/// 把表单写进 toml_edit 文档（已有文件原地改值，保住文件头注释；
/// `[[steps]]` 整段重建）。保存永远是 `speakers` + `prompt` 形态。
fn fill_doc(doc: &mut toml_edit::DocumentMut, form: &WorkflowForm) {
    use toml_edit::{value, Array, ArrayOfTables, Item, Table};

    doc["name"] = value(form.name.clone());
    doc["description"] = value(form.description.clone());
    match &form.command {
        Some(cmd) => { doc["command"] = value(cmd.clone()); }
        None => { doc.remove("command"); }
    }
    match form.max_rounds {
        Some(r) => {
            doc["max_rounds"] = value(r as i64);
        }
        None => {
            doc.remove("max_rounds");
        }
    }
    let mut aot = ArrayOfTables::new();
    for s in &form.steps {
        let mut t = Table::new();
        t["id"] = value(s.id.clone());
        if !s.description.is_empty() {
            t["description"] = value(s.description.clone());
        }
        let mut speakers = Array::new();
        for sp in &s.speakers {
            speakers.push(sp.as_str());
        }
        t["speakers"] = value(speakers);
        t["prompt"] = value(s.prompt.clone());
        if let Some(k) = &s.output_key {
            t["output_key"] = value(k.clone());
        }
        aot.push(t);
    }
    doc["steps"] = Item::ArrayOfTables(aot);
}

/// 新文件的注释头（config/workflows/bug_triage.toml 的精简版）。
fn new_file_header(name: &str) -> String {
    format!(
        "# Workflow: {name}\n\
         # Steps run in order; each speaker sees the previous speakers' output in the same step.\n\
         # Prompt vars: {{{{topic}}}} plus any step's output_key ({{{{key}}}}).\n\n"
    )
}

/// 新建项目层 workflow：`<cwd>/.latte/workflows.d/<name>.toml`。
/// 名字已存在（项目或全局）时报错。
pub fn create(cwd: &Path, form: &WorkflowForm) -> Result<WorkflowDetail, String> {
    if exists(cwd, &form.name) {
        return Err(format!("workflow '{}' already exists", form.name));
    }
    let dir = project_dir(cwd);
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    let path = dir.join(format!("{}.toml", form.name));
    let mut doc = toml_edit::DocumentMut::new();
    fill_doc(&mut doc, form);
    std::fs::write(&path, format!("{}{}", new_file_header(&form.name), doc))
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    get(cwd, &form.name)
}

/// 更新项目层 workflow。项目副本已存在 → toml_edit 原地更新；只有
/// 全局副本 → 在项目层生成遮蔽副本。`form.name != name` 即重命名：
/// 写完新文件后删掉旧的项目副本。
pub fn update(cwd: &Path, name: &str, form: &WorkflowForm) -> Result<WorkflowDetail, String> {
    if !exists(cwd, name) {
        return Err(format!("workflow '{name}' not found"));
    }
    let dir = project_dir(cwd);
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    let path = dir.join(format!("{}.toml", form.name));
    let existing = std::fs::read_to_string(&path).ok();
    let mut doc: toml_edit::DocumentMut = match &existing {
        Some(c) => c.parse().map_err(|e| format!("parse {}: {e}", path.display()))?,
        None => toml_edit::DocumentMut::new(),
    };
    fill_doc(&mut doc, form);
    let output = if existing.is_some() {
        doc.to_string()
    } else {
        format!("{}{}", new_file_header(&form.name), doc)
    };
    std::fs::write(&path, output).map_err(|e| format!("write {}: {e}", path.display()))?;
    if form.name != name {
        let old = dir.join(format!("{name}.toml"));
        if old.exists() {
            std::fs::remove_file(&old).map_err(|e| format!("remove {}: {e}", old.display()))?;
        }
    }
    get(cwd, &form.name)
}

/// 只删项目层副本；只有全局副本时报错（全局文件不属于本工作区）。
pub fn delete(cwd: &Path, name: &str) -> Result<(), String> {
    let path = project_dir(cwd).join(format!("{name}.toml"));
    if path.exists() {
        return std::fs::remove_file(&path).map_err(|e| format!("remove {}: {e}", path.display()));
    }
    if exists(cwd, name) {
        return Err(format!(
            "workflow '{name}' only exists in the global config; refusing to delete it"
        ));
    }
    Err(format!("workflow '{name}' not found"))
}

// ─── Validate ─────────────────────────────────────────────────────

/// 提取 prompt 里的 `{{var}}` 占位符。
fn extract_placeholders(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let name = after[..end].trim();
                if !name.is_empty() {
                    out.push(name.to_string());
                }
                rest = &after[end + 2..];
            }
            None => break,
        }
    }
    out
}

/// 表单校验。errors 阻断保存/运行；warnings 仅提示。
pub fn validate(form: &WorkflowForm, merged: &AgentConfig) -> ValidateResponse {
    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    // 名字：^[a-z0-9][a-z0-9_]*$
    let mut chars = form.name.chars();
    let valid_name = chars
        .next()
        .map(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .unwrap_or(false)
        && form
            .name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !valid_name {
        errors.push(format!(
            "invalid name '{}': must match ^[a-z0-9][a-z0-9_]*$",
            form.name
        ));
    }

    if form.steps.is_empty() {
        errors.push("workflow has no steps".into());
    }

    // step id：非空 + 唯一。
    let mut seen_ids: HashSet<&str> = HashSet::new();
    for step in &form.steps {
        if step.id.trim().is_empty() {
            errors.push("step id must not be empty".into());
        } else if !seen_ids.insert(step.id.as_str()) {
            errors.push(format!("duplicate step id '{}'", step.id));
        }
    }

    // speakers：非空、不是 advisor（monitor-only）、存在于 merged.roles。
    for step in &form.steps {
        if step.speakers.is_empty() {
            errors.push(format!("step '{}': speakers must not be empty", step.id));
        }
        for sp in &step.speakers {
            if sp == "advisor" {
                errors.push(format!(
                    "step '{}': 'advisor' is monitor-only; use reviewer for workflow tasks",
                    step.id
                ));
            } else if !merged.roles.contains_key(sp) {
                errors.push(format!("step '{}': role '{sp}' not found in config", step.id));
            }
        }
        if step.prompt.trim().is_empty() {
            warnings.push(format!("step '{}': prompt is empty", step.id));
        }
    }

    // output_key 跨 step 重复 → warning。
    let mut seen_keys: HashSet<&str> = HashSet::new();
    for step in &form.steps {
        if let Some(k) = &step.output_key {
            if !seen_keys.insert(k.as_str()) {
                warnings.push(format!("output_key '{k}' is set by more than one step"));
            }
        }
    }

    // prompt 里的未知 {{var}}（不是 topic、不是任何 step 的 output_key）→ warning。
    for step in &form.steps {
        for var in extract_placeholders(&step.prompt) {
            if var != "topic" && !seen_keys.contains(var.as_str()) {
                warnings.push(format!("step '{}': unknown variable '{{{{{var}}}}}'", step.id));
            }
        }
    }

    ValidateResponse {
        ok: errors.is_empty(),
        errors,
        warnings,
    }
}

// ─── Test-run state ───────────────────────────────────────────────

struct ActiveRun {
    tx: broadcast::Sender<ChatEvent>,
    cancel: Arc<AtomicBool>,
    /// 当前 run 的 id（`wfui-<micros>`），保留给状态查询/日志。
    #[allow(dead_code)]
    run_id: String,
    /// 已发送事件的回放缓冲：前端先 POST run 再连 SSE，连接建立前
    /// 发出的 Started/Step 事件靠它补发，避免丢开头的事件。
    history: Vec<ChatEvent>,
}

/// UI 测试运行的全局状态（一个 backend 同时只允许一个测试运行）。
/// 模式同 `self_loop::SelfLoopState`。
#[derive(Default)]
pub(crate) struct WorkflowRunState {
    inner: parking_lot::RwLock<Option<ActiveRun>>,
}

impl WorkflowRunState {
    /// start 时登记新一轮的事件 sender / cancel flag / run_id。
    pub(crate) fn start(
        &self,
        tx: broadcast::Sender<ChatEvent>,
        cancel: Arc<AtomicBool>,
        run_id: String,
    ) {
        *self.inner.write() = Some(ActiveRun {
            tx,
            cancel,
            run_id,
            history: Vec::new(),
        });
    }

    /// 转发一条事件：同一把写锁内先记录回放再广播。与
    /// `subscribe_with_history`（读锁内建 receiver + 快照 history）
    /// 互斥，保证订阅前的事件只在 history、订阅后的只走 broadcast，
    /// 不丢不重。
    pub(crate) fn broadcast(&self, ev: ChatEvent) {
        if let Some(a) = self.inner.write().as_mut() {
            a.history.push(ev.clone());
            let _ = a.tx.send(ev);
        }
    }

    /// stop：置 cancel flag（运行中的 engine 在下一个 step/speaker 边界退出）。
    pub(crate) fn cancel(&self) {
        if let Some(a) = self.inner.read().as_ref() {
            a.cancel.store(true, Ordering::SeqCst);
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        self.inner.read().is_some()
    }

    /// 订阅并带回放缓冲：receiver 与 history 快照在同一把读锁内取出，
    /// 与 `broadcast`（写锁内记录 + 发送）互斥 —— 订阅前的事件只在
    /// history 里，订阅后的只走 broadcast，不丢不重。
    pub(crate) fn subscribe_with_history(&self) -> (Vec<ChatEvent>, broadcast::Receiver<ChatEvent>) {
        match self.inner.read().as_ref() {
            Some(a) => {
                let rx = a.tx.subscribe();
                (a.history.clone(), rx)
            }
            None => (Vec::new(), broadcast::channel::<ChatEvent>(1).1),
        }
    }

    /// 运行结束（ok/failed/cancelled）后清掉状态。
    pub(crate) fn clear(&self) {
        *self.inner.write() = None;
    }
}

// ─── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use latte_agent_core::role::RoleTemplate;

    fn merged_with_roles(ids: &[&str]) -> AgentConfig {
        let mut cfg = AgentConfig::default();
        for id in ids {
            cfg.roles.insert(
                id.to_string(),
                RoleTemplate {
                    id: id.to_string(),
                    name: id.to_string(),
                    category: "test".into(),
                    model_tier: "standard".into(),
                    model_chain: vec![],
                    prompt_file: None,
                    temperature: None,
                    tools: vec![],
                    icon: "[t]".into(),
                    skills: vec![],
            code_paths: vec![],
                },
            );
        }
        cfg
    }

    fn one_step_form(name: &str, speakers: Vec<String>, prompt: &str) -> WorkflowForm {
        WorkflowForm {
            name: name.into(),
            description: String::new(),
            max_rounds: None,
            steps: vec![StepForm {
                id: "s1".into(),
                description: String::new(),
                speakers,
                prompt: prompt.into(),
                output_key: None,
            }],
        }
    }

    #[test]
    fn validate_rejects_bad_name() {
        let merged = merged_with_roles(&["pm"]);
        for bad in ["", "Bad", "my-workflow", "1 wf", "workflow!"] {
            let v = validate(&one_step_form(bad, vec!["pm".into()], "do {{topic}}"), &merged);
            assert!(!v.ok, "name {bad:?} should be invalid");
            assert!(v.errors.iter().any(|e| e.contains("invalid name")));
        }
        let v = validate(&one_step_form("ok_name1", vec!["pm".into()], "do {{topic}}"), &merged);
        assert!(v.ok, "valid form failed: {:?}", v.errors);
    }

    #[test]
    fn validate_rejects_unknown_role() {
        let merged = merged_with_roles(&["pm"]);
        let v = validate(
            &one_step_form("wf", vec!["ghost".into()], "do {{topic}}"),
            &merged,
        );
        assert!(!v.ok);
        assert!(v.errors.iter().any(|e| e.contains("role 'ghost' not found")));
    }

    #[test]
    fn validate_rejects_advisor() {
        let merged = merged_with_roles(&["pm", "advisor"]);
        let v = validate(
            &one_step_form("wf", vec!["advisor".into()], "do {{topic}}"),
            &merged,
        );
        assert!(!v.ok);
        assert!(v.errors.iter().any(|e| e.contains("monitor-only")));
    }

    #[test]
    fn validate_warns_unknown_var_and_dup_output_key() {
        let merged = merged_with_roles(&["pm"]);
        let form = WorkflowForm {
            name: "wf".into(),
            description: String::new(),
            max_rounds: None,
            steps: vec![
                StepForm {
                    id: "a".into(),
                    description: String::new(),
                    speakers: vec!["pm".into()],
                    prompt: "draft {{topic}}".into(),
                    output_key: Some("draft".into()),
                },
                StepForm {
                    id: "b".into(),
                    description: String::new(),
                    speakers: vec!["pm".into()],
                    prompt: "refine {{draft}} with {{missing}}".into(),
                    output_key: Some("draft".into()),
                },
            ],
        };
        let v = validate(&form, &merged);
        assert!(v.ok, "errors: {:?}", v.errors);
        // {{draft}} 是已知 output_key → 不警告；{{missing}} → 警告；重复 output_key → 警告。
        assert!(v.warnings.iter().any(|w| w.contains("{{missing}}")));
        assert!(!v.warnings.iter().any(|w| w.contains("{{draft}}}'") ));
        assert!(v.warnings.iter().any(|w| w.contains("more than one step")));
    }

    #[test]
    fn list_project_shadows_global() {
        let base = std::env::temp_dir().join(format!(
            "wf-list-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let project = base.join("project");
        let global = base.join("global");
        std::fs::create_dir_all(project.join(".latte/workflows.d")).unwrap();
        std::fs::create_dir_all(global.join("workflows.d")).unwrap();
        std::fs::write(
            global.join("workflows.d/shared.toml"),
            "name = \"shared\"\ndescription = \"global copy\"\n[[steps]]\nid = \"s\"\nspeakers = [\"pm\"]\nprompt = \"x\"\n",
        )
        .unwrap();
        std::fs::write(
            global.join("workflows.d/global_only.toml"),
            "name = \"global_only\"\ndescription = \"only global\"\n[[steps]]\nid = \"s\"\nspeakers = [\"pm\"]\nprompt = \"x\"\n",
        )
        .unwrap();
        std::fs::write(
            project.join(".latte/workflows.d/shared.toml"),
            "name = \"shared\"\ndescription = \"project copy\"\n[[steps]]\nid = \"s\"\nspeakers = [\"pm\"]\nprompt = \"x\"\n[[steps]]\nid = \"t\"\nspeakers = [\"pm\"]\nprompt = \"y\"\n",
        )
        .unwrap();

        let prev = std::env::var_os("LATTE_HOME");
        std::env::set_var("LATTE_HOME", &global);
        let list = list(&project);
        match prev {
            Some(v) => std::env::set_var("LATTE_HOME", v),
            None => std::env::remove_var("LATTE_HOME"),
        }

        let shared = list.iter().find(|s| s.name == "shared").expect("shared listed");
        assert_eq!(shared.description, "project copy", "项目副本必须遮蔽全局");
        assert_eq!(shared.source, WorkflowSource::Project);
        assert_eq!(shared.steps_count, 2);
        let g = list
            .iter()
            .find(|s| s.name == "global_only")
            .expect("global_only listed");
        assert_eq!(g.source, WorkflowSource::Global);
        assert!(list.windows(2).all(|w| w[0].name <= w[1].name), "按名字排序");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn create_update_delete_roundtrip() {
        let base = std::env::temp_dir().join(format!(
            "wf-crud-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&base).unwrap();

        let form = one_step_form("demo", vec!["pm".into()], "do {{topic}}");
        let detail = create(&base, &form).expect("create");
        assert_eq!(detail.source, WorkflowSource::Project);
        assert!(detail.raw_toml.contains("# Workflow: demo"));
        // 重名 create → 冲突。
        assert!(create(&base, &form).unwrap_err().contains("already exists"));
        // update 原地改（项目副本存在）。
        let mut form2 = form.clone();
        form2.description = "v2".into();
        let detail = update(&base, "demo", &form2).expect("update");
        assert_eq!(detail.description, "v2");
        // rename：新文件写入，旧项目副本删除。
        let mut form3 = form2.clone();
        form3.name = "demo_renamed".into();
        let detail = update(&base, "demo", &form3).expect("rename");
        assert_eq!(detail.name, "demo_renamed");
        assert!(!base.join(".latte/workflows.d/demo.toml").exists());
        // delete 项目副本。
        delete(&base, "demo_renamed").expect("delete");
        assert!(delete(&base, "demo_renamed").unwrap_err().contains("not found"));

        let _ = std::fs::remove_dir_all(&base);
    }
}
