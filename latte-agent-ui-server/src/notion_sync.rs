//! Notion 同步：把任务看板的变动镜像到本地 latte-rs-notion-client
//! (`http://127.0.0.1:3210`) 的 `/api/ext/{ns}/records/{id}` 端点。
//!
//! 设计：
//! - **触发**：每次 `TaskStore` 落盘（`persist()`）→ 把任务 id 推入
//!   `DirtySet`（内存态，本地文件是 source of truth）。
//! - **推送**：`sync_dirty_tasks` 后台定时任务每 5s 跑一次，
//!   逐个 PUT 到 notion-client；成功后从 dirty 移除。
//! - **重试**：网络/5xx 错误 3 次重试（500ms 退避）；4xx 不重试。
//! - **配置**：env `LATTE_NOTION_URL`（默认 `http://127.0.0.1:3210`）
//!   + `LATTE_NOTION_TOKEN`（必填，缺失时整个 sync 禁用并静默跳过）。
//! - **命名空间**：`agents-tasks`（含连字符，符合
//!   `^[a-z0-9][a-z0-9-]{0,31}$`）。
//!
//! 任务 id 复用 `tasks::Task.id`（如 `LAT-100`），符合
//! `^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$`。

/// 同步配置：从 `LATTE_NOTION_URL` / `LATTE_NOTION_TOKEN` 环境变量加载。
/// `token` 为空时 `enabled = false`，所有 sync 路径静默 no-op。
#[derive(Debug, Clone, PartialEq)]
pub struct NotionSyncConfig {
    pub enabled: bool,
    pub base_url: String,
    pub token: String,
    pub namespace: String,
}

impl NotionSyncConfig {
    /// 从环境变量读配置。`LATTE_NOTION_TOKEN` 缺失 → 整体禁用。
    /// `LATTE_NOTION_URL` 缺失 → 默认 `http://127.0.0.1:3210`（与
    /// latte-rs-notion-client 的默认端口对齐）。
    pub fn from_env() -> Self {
        let token = std::env::var("LATTE_NOTION_TOKEN").unwrap_or_default();
        let base_url = std::env::var("LATTE_NOTION_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:3210".to_string());
        Self {
            enabled: !token.is_empty(),
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            namespace: "agents-tasks".to_string(),
        }
    }
}

/// 同步结果（供测试与日志使用；生产代码主要看 `Ok` 走日志）。
#[derive(Debug, PartialEq)]
pub enum SyncResult {
    Ok,
    /// 4xx：客户端错（如 401 token 错、404 ns 错、400 title 空）→ 不重试。
    ClientError(u16, String),
    /// 5xx/网络/超时 → 重试 3 次后仍失败，标 dirty 等下一轮。
    RetriesExhausted(String),
}

/// 同步轮汇总（供测试与日志）。
#[derive(Debug, Default, PartialEq)]
pub struct SyncSummary {
    pub attempted: usize,
    pub succeeded: usize,
    /// 4xx（不重试，dirty 已丢弃）
    pub failed_client: usize,
    /// 5xx/网络（重试耗尽，dirty 保留）
    pub failed_server: usize,
    /// task_id 在 dirty 但 store.get 拿不到（被删除或未加载）
    pub skipped_missing: usize,
}

/// 文档镜像同步轮汇总（供测试与日志）。
#[derive(Debug, Default, PartialEq)]
pub struct DocSyncSummary {
    pub attempted: usize,
    pub succeeded: usize,
    pub failed_client: usize,
    pub failed_server: usize,
    pub skipped_missing: usize,
}

/// 从一篇 doc-graph 文档（含 frontmatter）构建 Notion record body。
/// `rel_path` 形如 "entities/auth-service.md"。
pub fn build_doc_record_body(content: &str, rel_path: &str) -> serde_json::Value {
    // 提取 frontmatter 的 type/tags（宽松解析：取 `type: X` 和 `tags:` 下一行 list）。
    let mut doc_type = String::new();
    let mut tags: Vec<String> = Vec::new();
    let mut in_tags = false;
    for line in content.lines().take(30) {
        let t = line.trim();
        if t.starts_with("type:") {
            doc_type = t.trim_start_matches("type:").trim().to_string();
        } else if t == "tags:" {
            in_tags = true;
        } else if in_tags {
            if t.starts_with('-') {
                tags.push(t.trim_start_matches('-').trim().to_string());
            } else {
                in_tags = false; // 遇到非 list 行结束 tags
            }
        }
    }
    let title = std::path::Path::new(rel_path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| rel_path.to_string());
    serde_json::json!({
        "title": title,
        "props": {
            "doc_type": doc_type,
            "tags": tags,
            "path": rel_path,
        },
        "content_md": content,
    })
}

/// 同步一篇 doc dirty：读 `.latte-review/docs/{rel}` 内容 → PUT agents-docs。
/// 成功删标记，5xx 保留标记（下轮再试），4xx 删标记（数据错不重试）。
async fn sync_one_doc_dirty(
    http: &reqwest::Client,
    cfg: &NotionSyncConfig,
    cwd: &std::path::Path,
    marker: &str,
    rel_path: &str,
) -> SyncResult {
    let full = cwd.join(".latte-review").join("docs").join(rel_path);
    let content = match std::fs::read_to_string(&full) {
        Ok(c) => c,
        Err(_) => return SyncResult::ClientError(404, format!("missing doc file: {rel_path}")),
    };
    let body = build_doc_record_body(&content, rel_path);
    // record id 用 marker（`dirty_marker_name` 已把 rel_path 里的 `/`、`.`、
    // `\\` 转成 `[A-Za-z0-9._-]` 合法 id）。rel_path 直接当 id 会因含 `/`
    // 命中不了单段路由（404）/ id 校验（400）——之前所有文档因此永不推送
    // 且 4xx 删标记静默丢镜像。真实路径在 body.props.path 里保留。
    let mut doc_cfg = cfg.clone();
    doc_cfg.namespace = "agents-docs".to_string();
    upsert_with_retry(http, &doc_cfg, marker, &body).await
}

/// 同步所有 `.latte/.docs-dirty/` 标记的文档到 Notion（agents-docs）。
/// 返回完成后是否应删标记（成功 / 4xx）。
pub async fn sync_doc_dirty(
    http: &reqwest::Client,
    cfg: &NotionSyncConfig,
    cwd: &std::path::Path,
) -> DocSyncSummary {
    let mut summary = DocSyncSummary::default();
    if !cfg.enabled {
        return summary;
    }
    let dirty_dir = cwd.join(".latte").join(".docs-dirty");
    let Ok(entries) = std::fs::read_dir(&dirty_dir) else {
        return summary;
    };
    let mut to_remove: Vec<String> = Vec::new();
    for e in entries.flatten() {
        let marker = e.file_name().to_string_lossy().to_string();
        let rel_path = std::fs::read_to_string(e.path())
            .unwrap_or_default()
            .trim()
            .to_string();
        if rel_path.is_empty() {
            to_remove.push(marker);
            continue;
        }
        summary.attempted += 1;
        match sync_one_doc_dirty(http, cfg, cwd, &marker, &rel_path).await {
            SyncResult::Ok => {
                summary.succeeded += 1;
                to_remove.push(marker);
            }
            SyncResult::ClientError(_, _) => {
                summary.failed_client += 1;
                to_remove.push(marker);
            }
            SyncResult::RetriesExhausted(_) => {
                summary.failed_server += 1;
            }
        }
    }
    for marker in to_remove {
        let _ = std::fs::remove_file(dirty_dir.join(marker));
    }
    summary
}

/// 构造 PUT 请求 URL：`{base}/api/ext/{ns}/records/{id}`。
pub fn record_url(base: &str, ns: &str, id: &str) -> String {
    format!("{base}/api/ext/{ns}/records/{id}")
}

/// 4xx 是否「数据错、删标记不重试」。**只有** 400/404 算致命——
/// 那意味着这条记录本身有问题，重推无用。401（token 错/轮换）/
/// 403/408/429 都是瞬时/配置错，删标记会把那次改动永久丢掉（修好
/// token 后也不补推），按服务端错对待：留标记等下轮/重启。
fn is_fatal_client_error(status: u16) -> bool {
    matches!(status, 400 | 404)
}

/// 单次 PUT：失败按 4xx/网络区分返回 `SyncResult`。
pub async fn upsert_one(
    http: &reqwest::Client,
    cfg: &NotionSyncConfig,
    task_id: &str,
    body: &serde_json::Value,
) -> SyncResult {
    let url = record_url(&cfg.base_url, &cfg.namespace, task_id);
    let resp = match http
        .put(&url)
        .bearer_auth(&cfg.token)
        .json(body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return SyncResult::RetriesExhausted(format!("network: {e}")),
    };
    let status = resp.status();
    if status.is_success() {
        return SyncResult::Ok;
    }
    let text = resp.text().await.unwrap_or_default();
    if status.is_client_error() {
        return SyncResult::ClientError(status.as_u16(), text);
    }
    SyncResult::RetriesExhausted(format!("server {}: {}", status.as_u16(), text))
}

/// 推送一个任务：3 次重试（每次间隔 500ms），4xx 立即返回 `ClientError`。
pub async fn upsert_with_retry(
    http: &reqwest::Client,
    cfg: &NotionSyncConfig,
    task_id: &str,
    body: &serde_json::Value,
) -> SyncResult {
    for attempt in 0..3u32 {
        match upsert_one(http, cfg, task_id, body).await {
            SyncResult::Ok => return SyncResult::Ok,
            SyncResult::ClientError(code, msg) => {
                eprintln!("[notion-sync] {task_id} 客户端错 {code}: {msg}（不重试）");
                return SyncResult::ClientError(code, msg);
            }
            SyncResult::RetriesExhausted(msg) => {
                if attempt + 1 < 3 {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    continue;
                }
                eprintln!("[notion-sync] {task_id} 3 次重试仍失败: {msg}");
                return SyncResult::RetriesExhausted(msg);
            }
        }
    }
    SyncResult::RetriesExhausted("loop exited without result".into())
}

/// 任务 dirty 集合：每次 `TaskStore::persist` 把 id 推入，
/// `sync_dirty_tasks` 取出推送成功后移除。**内存索引 + 落盘标记双写**：
/// 内存做快速读，磁盘标记 `<cwd>/.latte/.tasks-dirty/<id>` 是重启/离线
/// 的真相源——与 doc 镜像的 `.latte/.docs-dirty/` 完全同构。
///
/// 此前是纯内存（重启即清空 → 改过未推的任务永久漏推 Notion）。现在
/// 每次 mark 同步写标记文件，sync 成功/4xx 删文件，5xx/离线留文件；
/// `TaskStore::load` 扫目录重建内存集，离线期间积累的 dirty 重启后
/// 自然推出去。
#[derive(Debug, Default, Clone)]
pub struct DirtySet {
    ids: std::collections::HashSet<String>,
}

/// `.latte/.tasks-dirty/` 目录（与 `.docs-dirty/` 平级）。
fn dirty_dir(cwd: &std::path::Path) -> std::path::PathBuf {
    cwd.join(".latte").join(".tasks-dirty")
}

/// 任务 id → 标记文件名（id 已是 `LAT###` 这类安全串，无需转义；
/// 防御性再过一道，保证不逃出目录）。
fn dirty_marker_name(id: &str) -> String {
    id.replace(['/', '\\', '.'], "_")
}

/// 写一个 dirty 标记文件（内容 = 任务 id，便于人读/排查）。
/// best-effort：写失败只告警，内存标记照样生效（宁漏盘不阻断写路径）。
fn mark_dirty_on_disk(cwd: &std::path::Path, id: &str) {
    let dir = dirty_dir(cwd);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("[notion-sync] create {}: {e}", dir.display());
        return;
    }
    let path = dir.join(dirty_marker_name(id));
    if let Err(e) = std::fs::write(&path, id) {
        eprintln!("[notion-sync] write dirty {}: {e}", path.display());
    }
}

/// 删一个 dirty 标记文件（sync 成功 / 4xx 销账）。幂等。
pub fn clear_dirty_on_disk(cwd: &std::path::Path, id: &str) {
    let path = dirty_dir(cwd).join(dirty_marker_name(id));
    let _ = std::fs::remove_file(path);
}

/// 扫 `<cwd>/.latte/.tasks-dirty/` 重建内存 dirty 集合（重启后）。
/// 文件名与内容都还原成任务 id（取内容优先，兼容 marker_name 转义）。
pub fn load_dirty_from_disk(cwd: &std::path::Path) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    let Ok(entries) = std::fs::read_dir(dirty_dir(cwd)) else {
        return out;
    };
    for e in entries.flatten() {
        let path = e.path();
        if !path.is_file() {
            continue;
        }
        if let Ok(id) = std::fs::read_to_string(&path) {
            let id = id.trim().to_string();
            if !id.is_empty() {
                out.insert(id);
            }
        }
    }
    out
}

impl DirtySet {
    /// 标记 dirty：内存 + 落盘双写。`cwd` 是项目根（dirty 目录所在）。
    pub fn mark(&mut self, cwd: &std::path::Path, id: &str) {
        self.ids.insert(id.to_string());
        mark_dirty_on_disk(cwd, id);
    }
    /// take 后某 id 是否仍在内存集合里（即 take 期间被重新 mark 过）。
    /// sync 用此判定「clear_dirty_on_disk 会否抹掉一次新 mark」——
    /// 修前 sync_once 拿 take → 用户在循环里 mark → 清盘 = 丢这次 mark。
    pub fn contains_after_take(&self, id: &str) -> bool {
        self.ids.contains(id)
    }
    /// 重启重建时批量注入盘上扫出的 id（**不再落盘**——标记文件已存在）。
    pub(crate) fn restore_id(&mut self, id: String) {
        self.ids.insert(id);
    }
    /// 下一轮 `sync` 还会再读到。
    pub fn take_dirty(&mut self) -> Vec<String> {
        let out: Vec<String> = self.ids.iter().cloned().collect();
        self.ids.clear();
        out
    }

    pub fn dirty_count(&self) -> usize {
        self.ids.len()
    }
}

/// 把一个任务序列化为 Notion `/api/ext/{ns}/records` PUT 请求体。
/// - `title` = 任务 title（必填）。
/// - `props` = 状态/优先级/labels/workflow 等结构化字段。
/// - `content_md` = 任务描述 + 最近 4 条 history note。
pub fn build_record_body(t: &crate::tasks::Task) -> serde_json::Value {
    let mut notes: Vec<String> = Vec::new();
    for h in t
        .history
        .iter()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        if let Some(note) = &h.note {
            notes.push(format!(
                "- [{}] {} → {}: {}",
                h.at,
                h.from.as_deref().unwrap_or("?"),
                h.to,
                note
            ));
        }
    }
    let description = if t.description.is_empty() {
        "(无描述)".to_string()
    } else {
        t.description.clone()
    };
    let mut content = format!(
        "# [{id}] {title}\n\n{description}\n\n**State**: `{state}` · **Priority**: {priority} · **Workflow**: `{wf}`\n\n",
        id = t.id,
        title = t.title,
        state = t.state,
        priority = t.priority,
        wf = t.workflow.as_deref().unwrap_or("(none)"),
    );
    if !notes.is_empty() {
        content.push_str("## 最近变更\n\n");
        content.push_str(&notes.join("\n"));
        content.push('\n');
    }

    let mut props = serde_json::Map::new();
    props.insert("state".into(), serde_json::Value::String(t.state.clone()));
    props.insert(
        "priority".into(),
        serde_json::Value::Number(t.priority.into()),
    );
    props.insert(
        "labels".into(),
        serde_json::Value::Array(
            t.labels
                .iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect(),
        ),
    );
    props.insert(
        "workflow".into(),
        serde_json::Value::String(t.workflow.clone().unwrap_or_default()),
    );
    if let Some(sched) = t.scheduled_at {
        props.insert(
            "scheduled_at".into(),
            serde_json::Value::Number(sched.into()),
        );
    }
    // 看板已支持任意深度任务树：真实 parent_id 透传到 props。Notion
    // 记录是平铺结构（title/props/content_md），props 里保留父子引用
    // 供检索，不在 Notion 端重建层级。
    if let Some(pid) = &t.parent_id {
        props.insert("parent_id".into(), serde_json::Value::String(pid.clone()));
    }

    serde_json::json!({
        "title": t.title,
        "props": serde_json::Value::Object(props),
        "content_md": content,
    })
}

/// 单次同步轮：取出 TaskStore dirty 集合，逐个 PUT 推送。
/// 成功 → 任务 id 不再回到 dirty（本轮已同步）。
/// 失败（4xx） → 从 dirty 移除（不再重试，避免垃圾日志）。
/// 失败（5xx/网络，重试耗尽）→ 任务 id 保留在 dirty 等下一轮。
pub async fn sync_dirty_tasks(
    http: &reqwest::Client,
    cfg: &NotionSyncConfig,
    store: &mut crate::tasks::TaskStore,
) -> SyncSummary {
    if !cfg.enabled {
        return SyncSummary::default();
    }

    let cwd = store.project_root().to_path_buf();
    let dirty = store.take_notion_dirty();
    let mut summary = SyncSummary {
        attempted: dirty.len(),
        ..SyncSummary::default()
    };
    for task_id in dirty {
        let body = match store.get(&task_id) {
            Some(t) => build_record_body(t),
            None => {
                // 本地已删（delete_task 标过 dirty 但对象没了）：盘标记
                // 清掉，否则成为永远读不回的孤儿标记。v1 不发 DELETE。
                clear_dirty_on_disk(&cwd, &task_id);
                summary.skipped_missing += 1;
                continue;
            }
        };
        match upsert_with_retry(http, cfg, &task_id, &body).await {
            SyncResult::Ok => {
                // take → 用户在循环里 mark 这条 id 的 race 防护：
                // 取到 Ok 想清盘，但 take 期间又被 mark 过，磁盘标记
                // 留着（内存 dirty 由 mark_notion_dirty 重填，下轮再推）。
                if !store.notion_dirty_contains(&task_id) {
                    clear_dirty_on_disk(&cwd, &task_id);
                }
                summary.succeeded += 1;
            }
            SyncResult::ClientError(code, _) => {
                if is_fatal_client_error(code) {
                    clear_dirty_on_disk(&cwd, &task_id);
                    summary.failed_client += 1;
                } else {
                    // 401/403/408/429：留标记重推。
                    store.mark_notion_dirty(&task_id);
                    summary.failed_server += 1;
                }
            }
            SyncResult::RetriesExhausted(_) => {
                // 5xx/网络错：盘标记留着，内存重新标上下轮再试。
                store.mark_notion_dirty(&task_id);
                summary.failed_server += 1;
            }
        }
    }
    summary
}
/// 后台循环：每 5s 调一次 `sync_dirty_tasks`。配置缺失（`enabled=false`）
/// 时是纯 no-op（take_dirty 不调用、dirty 保留）。由 `lib::spawn` 在
/// server 启动时 tokio::spawn。
pub async fn notion_sync_loop(b: std::sync::Arc<crate::UiBackend>) {
    // 启动时跑一次（接住上次关闭时未推送的 dirty 已被新的 create_task
    // 标上；但若用户没重启进程就直接调用，dirty 已经在内存）。
    sync_once(&b).await;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        sync_once(&b).await;
    }
}

async fn sync_once(b: &crate::UiBackend) {
    let (http, cfg) = match (b.notion_http.lock().as_ref(), b.notion_cfg.lock().as_ref()) {
        (Some(h), Some(c)) => (h.clone(), c.clone()),
        _ => return,
    };
    if !cfg.enabled {
        return;
    }
    // 短临界区：take_dirty 持锁拿任务 id 列表。parking_lot 写锁
    // guard 不 Send，跨 .await 持锁会编译失败（future not Send）。
    // 单 id 同步逻辑另抽到 sync_task_id（不再持 store 锁）。
    let dirty = b.tasks.write().take_notion_dirty();
    let mut summary = SyncSummary {
        attempted: dirty.len(),
        ..SyncSummary::default()
    };
    let mut re_dirty: Vec<String> = Vec::new();
    for task_id in dirty {
        let body = {
            let store = b.tasks.read();
            match store.get(&task_id) {
                Some(t) => build_record_body(t),
                None => {
                    // 本地已删：清盘标记（孤儿标记否则永远留在盘上）。
                    clear_dirty_on_disk(&b.cwd, &task_id);
                    summary.skipped_missing += 1;
                    continue;
                }
            }
        };
        match upsert_with_retry(&http, &cfg, &task_id, &body).await {
            SyncResult::Ok => {
                // take → 用户在循环里 mark 这条 id 的 race 防护：
                // 内存里又被 mark 时清盘会丢这次改动——留着标记让下轮再推。
                if !b.tasks.read().notion_dirty_contains(&task_id) {
                    clear_dirty_on_disk(&b.cwd, &task_id);
                }
                summary.succeeded += 1;
            }
            SyncResult::ClientError(code, _) => {
                if is_fatal_client_error(code) {
                    // 400/404 = 记录本身有问题，删标记不重试。
                    clear_dirty_on_disk(&b.cwd, &task_id);
                    summary.failed_client += 1;
                } else {
                    // 401/403/408/429 等瞬时/配置错：留标记等下轮，
                    // 修好 token/限流放开后会自动补推。
                    re_dirty.push(task_id);
                    summary.failed_server += 1;
                }
            }
            SyncResult::RetriesExhausted(_) => {
                // 5xx/网络错：盘标记留着（重启/下轮仍能读到），内存重新
                // 标上让本轮 take 之后它还活着。
                re_dirty.push(task_id);
                summary.failed_server += 1;
            }
        }
    }
    if !re_dirty.is_empty() {
        let mut store = b.tasks.write();
        for id in re_dirty {
            store.mark_notion_dirty(&id);
        }
    }
    if summary.attempted > 0 {
        eprintln!(
            "[notion-sync] attempted={} ok={} 4xx={} 5xx={} missing={}",
            summary.attempted,
            summary.succeeded,
            summary.failed_client,
            summary.failed_server,
            summary.skipped_missing
        );
    }

    // 文档镜像（agents-docs）：读 .latte/.docs-dirty/ 标记，PUT 到
    // notion-client。与任务 dirty 独立（doc_write 写标记文件）。
    let cwd = b.cwd.clone();
    let doc_summary = sync_doc_dirty(&http, &cfg, &cwd).await;
    if doc_summary.attempted > 0 {
        eprintln!(
            "[notion-sync] docs attempted={} ok={} 4xx={} 5xx={} missing={}",
            doc_summary.attempted,
            doc_summary.succeeded,
            doc_summary.failed_client,
            doc_summary.failed_server,
            doc_summary.skipped_missing
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Mutex;

    /// 并行 cargo test 默认会让 env 改动的测试相互污染；用全局
    /// Mutex 把 3 个 env 测试串行（仅本进程内）。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// 持锁并清掉两个 env（用于 disabled 测试），返回时还原。
    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn config_from_env_disabled_when_token_missing() {
        let _g = lock_env();
        let prev_url = std::env::var_os("LATTE_NOTION_URL");
        let prev_token = std::env::var_os("LATTE_NOTION_TOKEN");
        std::env::remove_var("LATTE_NOTION_URL");
        std::env::remove_var("LATTE_NOTION_TOKEN");
        let cfg = NotionSyncConfig::from_env();
        std::env::remove_var("LATTE_NOTION_URL");
        std::env::remove_var("LATTE_NOTION_TOKEN");
        if let Some(v) = prev_url {
            std::env::set_var("LATTE_NOTION_URL", v);
        }
        if let Some(v) = prev_token {
            std::env::set_var("LATTE_NOTION_TOKEN", v);
        }
        assert!(!cfg.enabled, "no token = disabled");
        assert_eq!(cfg.base_url, "http://127.0.0.1:3210");
        assert_eq!(cfg.namespace, "agents-tasks");
    }

    #[test]
    fn config_from_env_enabled_when_token_set() {
        let _g = lock_env();
        let prev = std::env::var_os("LATTE_NOTION_TOKEN");
        std::env::set_var("LATTE_NOTION_TOKEN", "secret");
        let cfg = NotionSyncConfig::from_env();
        if let Some(v) = prev {
            std::env::set_var("LATTE_NOTION_TOKEN", v);
        } else {
            std::env::remove_var("LATTE_NOTION_TOKEN");
        }
        assert!(cfg.enabled);
        assert_eq!(cfg.token, "secret");
    }

    #[test]
    fn config_from_env_respects_url_override() {
        let _g = lock_env();
        let prev_url = std::env::var_os("LATTE_NOTION_URL");
        let prev_token = std::env::var_os("LATTE_NOTION_TOKEN");
        std::env::set_var("LATTE_NOTION_URL", "http://10.0.0.5:9999");
        std::env::set_var("LATTE_NOTION_TOKEN", "t");
        let cfg = NotionSyncConfig::from_env();
        std::env::remove_var("LATTE_NOTION_URL");
        if let Some(v) = prev_url {
            std::env::set_var("LATTE_NOTION_URL", v);
        }
        if let Some(v) = prev_token {
            std::env::set_var("LATTE_NOTION_TOKEN", v);
        } else {
            std::env::remove_var("LATTE_NOTION_TOKEN");
        }
        assert!(cfg.enabled);
        assert_eq!(cfg.base_url, "http://10.0.0.5:9999");
    }

    #[test]
    fn dirty_set_marks_takes_and_clears() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let mut s = DirtySet::default();
        s.mark(cwd, "LAT-100");
        s.mark(cwd, "LAT-101");
        s.mark(cwd, "LAT-100");
        assert_eq!(s.dirty_count(), 2);
        let taken: HashSet<String> = s.take_dirty().into_iter().collect();
        assert!(taken.contains("LAT-100"));
        assert!(taken.contains("LAT-101"));
        assert_eq!(s.dirty_count(), 0, "take 后内存集合应清空");
        // 盘标记仍在（take 不动盘——sync 成功才删）。
        assert!(cwd.join(".latte/.tasks-dirty/LAT-100").is_file());
        assert!(cwd.join(".latte/.tasks-dirty/LAT-101").is_file());
    }

    /// sync_once 期间的 race：take 清空内存后，用户再次 mark 同 id。
    /// 修前 sync 走 Ok 分支会无脑 clear_dirty_on_disk → 这次新 mark
    /// 永久丢失。contains_after_take 给出保护信号。
    #[test]
    fn contains_after_take_detects_user_mark_during_sync() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let mut s = DirtySet::default();
        s.mark(cwd, "LAT-100");
        // take 清空内存（不动盘）。
        let _ = s.take_dirty();
        assert!(
            !s.contains_after_take("LAT-100"),
            "take 后未重 mark 时应不在集合里"
        );
        // 模拟 sync 循环期间用户又改了一次——内存集合重新出现。
        s.mark(cwd, "LAT-100");
        assert!(
            s.contains_after_take("LAT-100"),
            "take 后重 mark 必须能被 contains_after_take 看见"
        );
    }

    /// dirty 标记落盘 → 重启重建（`load_dirty_from_disk` + restore_id）→
    /// sync 成功删盘。这是「离线/重启期间改动不漏推」的闭环。
    #[test]
    fn dirty_survives_restart_and_clears_on_sync() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        // 第一进程：标脏 → 落盘标记。
        {
            let mut s = DirtySet::default();
            s.mark(cwd, "LAT-100");
        }
        // 模拟重启：新 DirtySet 从盘重建。
        let reloaded = load_dirty_from_disk(cwd);
        assert!(reloaded.contains("LAT-100"), "重启后 dirty 应恢复");
        // 重建进新集合 → sync 成功后删盘标记。
        let mut s2 = DirtySet::default();
        for id in reloaded {
            s2.restore_id(id);
        }
        assert_eq!(s2.dirty_count(), 1);
        clear_dirty_on_disk(cwd, "LAT-100");
        assert!(!cwd.join(".latte/.tasks-dirty/LAT-100").exists());
        assert!(load_dirty_from_disk(cwd).is_empty(), "清盘后不再恢复");
    }

    #[test]
    fn record_body_serializes_task_state() {
        let t = crate::tasks::Task {
            schema: 1,
            id: "LAT-100".into(),
            title: "实现 ringbuf 核心读写".into(),
            description: "覆盖并发读写路径".into(),
            priority: 1,
            state: "in_progress".into(),
            labels: vec!["core".into()],
            task_type: Some("feature".into()),
            parent_id: Some("LAT-99".into()),
            sub_order: 0,
            scheduled_at: None,
            workflow: Some("tdd_development".into()),
            paths: vec!["src/ringbuf".into()],
            artifact_paths: Some(vec!["src/ringbuf".into()]),
            depends_on: vec![],
            dispatch_pending: false,
            isolated: None,
            worktree: None,
            runs: vec![],
            created_at: 1000,
            updated_at: 2000,
            history: vec![],
        };
        let body = build_record_body(&t);
        assert_eq!(body["title"], "实现 ringbuf 核心读写");
        let props = &body["props"];
        assert_eq!(props["state"], "in_progress");
        assert_eq!(props["priority"], 1);
        assert_eq!(props["labels"], serde_json::json!(["core"]));
        assert_eq!(props["workflow"], "tdd_development");
        assert_eq!(
            props["parent_id"], "LAT-99",
            "任意深度树的 parent_id 透传到 props"
        );
        let content = body["content_md"].as_str().expect("content_md string");
        assert!(content.contains("覆盖并发读写路径"));
        assert!(content.contains("[LAT-100]"));
    }

    /// HTTP 集成：4xx 不重试，立即返回 `ClientError`。
    #[tokio::test]
    async fn upsert_one_returns_client_error_on_4xx() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/ext/agents-tasks/records/LAT-100"))
            .and(header("authorization", "Bearer secret"))
            .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"error":"unauthorized"}"#))
            .mount(&server)
            .await;

        let cfg = NotionSyncConfig {
            enabled: true,
            base_url: server.uri(),
            token: "secret".into(),
            namespace: "agents-tasks".into(),
        };
        let http = reqwest::Client::new();
        let body = serde_json::json!({"title":"x","props":{},"content_md":""});
        let r = upsert_with_retry(&http, &cfg, "LAT-100", &body).await;
        assert_eq!(
            r,
            SyncResult::ClientError(401, r#"{"error":"unauthorized"}"#.to_string())
        );
    }

    /// HTTP 集成：5xx 触发重试 3 次，最终失败返回 `RetriesExhausted`。
    #[tokio::test]
    async fn upsert_with_retry_exhausts_on_5xx() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let cfg = NotionSyncConfig {
            enabled: true,
            base_url: server.uri(),
            token: "t".into(),
            namespace: "agents-tasks".into(),
        };
        let http = reqwest::Client::new();
        let body = serde_json::json!({"title":"x","props":{},"content_md":""});
        let r = upsert_with_retry(&http, &cfg, "LAT-1", &body).await;
        assert!(
            matches!(r, SyncResult::RetriesExhausted(_)),
            "expected RetriesExhausted, got {r:?}"
        );
    }

    /// HTTP 集成：2xx 立即成功。
    #[tokio::test]
    async fn upsert_one_returns_ok_on_2xx() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;

        let cfg = NotionSyncConfig {
            enabled: true,
            base_url: server.uri(),
            token: "t".into(),
            namespace: "agents-tasks".into(),
        };
        let http = reqwest::Client::new();
        let body = serde_json::json!({"title":"x","props":{},"content_md":""});
        let r = upsert_with_retry(&http, &cfg, "LAT-1", &body).await;
        assert_eq!(r, SyncResult::Ok);
    }

    /// sync_dirty_tasks：成功 → succeeded 计数 + dirty 清除。

    /// sync_dirty_tasks：成功 → succeeded 计数 + dirty 清除。
    #[tokio::test]
    async fn sync_dirty_pushes_and_clears() {
        use crate::tasks::TaskStore;
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = TaskStore::load(dir.path()).expect("load");
        store
            .create("demo", "", 3, vec![], None, None, None, None, "user")
            .expect("create");
        assert_eq!(store.notion_dirty_count(), 1, "create 标 dirty");

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;
        let cfg = NotionSyncConfig {
            enabled: true,
            base_url: server.uri(),
            token: "t".into(),
            namespace: "agents-tasks".into(),
        };
        let http = reqwest::Client::new();
        let summary = sync_dirty_tasks(&http, &cfg, &mut store).await;
        assert_eq!(summary.succeeded, 1);
        assert_eq!(summary.failed_client, 0);
        assert_eq!(summary.failed_server, 0);
        assert_eq!(store.notion_dirty_count(), 0, "成功后 dirty 清空");
    }

    /// sync_dirty_tasks：401（token 错/轮换，瞬时/配置错）→ **保留**
    /// 标记等补推；只有 400/404（记录本身坏）才删。这锁死「修好 token
    /// 后未推送的改动会自动补上」的承诺。
    #[tokio::test]
    async fn sync_dirty_keeps_marker_on_401_but_clears_on_400() {
        use crate::tasks::TaskStore;
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // 401：token 错。
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = TaskStore::load(dir.path()).expect("load");
        store
            .create("demo", "", 3, vec![], None, None, None, None, "user")
            .expect("create");
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad token"))
            .mount(&server)
            .await;
        let cfg = NotionSyncConfig {
            enabled: true,
            base_url: server.uri(),
            token: "t".into(),
            namespace: "agents-tasks".into(),
        };
        let http = reqwest::Client::new();
        let summary = sync_dirty_tasks(&http, &cfg, &mut store).await;
        assert_eq!(summary.failed_server, 1, "401 归为可重试（留标记）");
        assert_eq!(summary.failed_client, 0, "401 不是致命数据错");
        assert_eq!(
            store.notion_dirty_count(),
            1,
            "401 必须保留 dirty（修好 token 后补推）"
        );

        // 400：记录本身坏 → 删标记 + 计入 failed_client。
        let dir2 = tempfile::tempdir().expect("tempdir");
        let mut store2 = TaskStore::load(dir2.path()).expect("load");
        store2
            .create("demo", "", 3, vec![], None, None, None, None, "user")
            .expect("create");
        let server2 = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad record"))
            .mount(&server2)
            .await;
        let cfg2 = NotionSyncConfig {
            enabled: true,
            base_url: server2.uri(),
            token: "t".into(),
            namespace: "agents-tasks".into(),
        };
        let summary2 = sync_dirty_tasks(&http, &cfg2, &mut store2).await;
        assert_eq!(summary2.failed_client, 1, "400 是致命数据错");
        assert_eq!(store2.notion_dirty_count(), 0, "400 删除标记（不再重推）");
    }

    /// sync_dirty_tasks：5xx 失败 → dirty 保留待下一轮。
    #[tokio::test]
    async fn sync_dirty_keeps_dirty_on_5xx() {
        use crate::tasks::TaskStore;
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = TaskStore::load(dir.path()).expect("load");
        store
            .create("demo", "", 3, vec![], None, None, None, None, "user")
            .expect("create");

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let cfg = NotionSyncConfig {
            enabled: true,
            base_url: server.uri(),
            token: "t".into(),
            namespace: "agents-tasks".into(),
        };
        let http = reqwest::Client::new();
        let summary = sync_dirty_tasks(&http, &cfg, &mut store).await;
        assert_eq!(summary.failed_server, 1);
        assert_eq!(store.notion_dirty_count(), 1, "失败后 dirty 保留");
    }

    /// sync_dirty_tasks：disabled 时是 no-op。
    #[tokio::test]
    async fn sync_dirty_disabled_is_noop() {
        use crate::tasks::TaskStore;
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = TaskStore::load(dir.path()).expect("load");
        store
            .create("demo", "", 3, vec![], None, None, None, None, "user")
            .expect("create");
        let cfg = NotionSyncConfig {
            enabled: false,
            base_url: "http://127.0.0.1:1".into(),
            token: String::new(),
            namespace: "agents-tasks".into(),
        };
        let http = reqwest::Client::new();
        let summary = sync_dirty_tasks(&http, &cfg, &mut store).await;
        assert_eq!(summary.attempted, 0);
        assert_eq!(store.notion_dirty_count(), 1, "disabled 不消费 dirty");
    }
    // ─── doc 镜像（agents-docs） ────────────────────────────────────

    #[test]
    fn build_doc_record_body_extracts_frontmatter() {
        let content = "---\ntype: entity\ntitle: Auth Service\nsources:\n  - src/auth.rs\ntags:\n  - rust\n  - auth\ncreated: 2026-08-09\nupdated: 2026-08-09\n---\n\n# Auth Service\n\n正文";
        let body = build_doc_record_body(content, "entities/auth-service.md");
        assert_eq!(body["title"], "auth-service");
        assert_eq!(body["props"]["doc_type"], "entity");
        assert_eq!(body["props"]["tags"], serde_json::json!(["rust", "auth"]));
        assert_eq!(body["props"]["path"], "entities/auth-service.md");
        let content_md = body["content_md"].as_str().unwrap();
        assert!(
            content_md.starts_with("---\n"),
            "保留完整 frontmatter + 正文"
        );
    }

    /// sync_doc_dirty：成功 PUT 到 agents-docs 并删标记。
    #[tokio::test]
    async fn sync_doc_dirty_pushes_and_clears_marker() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let dir = tempfile::tempdir().unwrap();
        let doc_rel = "entities/auth-service.md";
        let doc_path = dir.path().join(".latte-review/docs/entities");
        std::fs::create_dir_all(&doc_path).unwrap();
        std::fs::write(
            doc_path.join("auth-service.md"),
            "---\ntype: entity\ntitle: Auth Service\ntags:\n  - rust\n---\n\nbody",
        )
        .unwrap();
        let dirty_dir = dir.path().join(".latte/.docs-dirty");
        std::fs::create_dir_all(&dirty_dir).unwrap();
        std::fs::write(dirty_dir.join("entities.auth-service.md"), doc_rel).unwrap();

        let server = MockServer::start().await;
        // record id = marker 文件名（`entities.auth-service.md`，合法
        // `[A-Za-z0-9._-]`），不是含 `/` 的 rel_path——后者会被
        // notion-client 拒（404/400）。
        Mock::given(method("PUT"))
            .and(path(
                "/api/ext/agents-docs/records/entities.auth-service.md",
            ))
            .and(header("authorization", "Bearer secret"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;

        let cfg = NotionSyncConfig {
            enabled: true,
            base_url: server.uri(),
            token: "secret".into(),
            namespace: "agents-tasks".into(),
        };
        let http = reqwest::Client::new();
        let s = sync_doc_dirty(&http, &cfg, dir.path()).await;
        assert_eq!(s.attempted, 1);
        assert_eq!(s.succeeded, 1);
        assert_eq!(
            std::fs::read_dir(dirty_dir).unwrap().count(),
            0,
            "成功后删除 dirty 标记"
        );
    }

    /// sync_doc_dirty：5xx 保留标记。
    #[tokio::test]
    async fn sync_doc_dirty_keeps_marker_on_5xx() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let dir = tempfile::tempdir().unwrap();
        let doc_path = dir.path().join(".latte-review/docs/entities");
        std::fs::create_dir_all(&doc_path).unwrap();
        std::fs::write(
            doc_path.join("a.md"),
            "---\ntype: entity\ntitle: A\n---\n\nx",
        )
        .unwrap();
        let dirty_dir = dir.path().join(".latte/.docs-dirty");
        std::fs::create_dir_all(&dirty_dir).unwrap();
        std::fs::write(dirty_dir.join("entities.a.md"), "entities/a.md").unwrap();

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let cfg = NotionSyncConfig {
            enabled: true,
            base_url: server.uri(),
            token: "t".into(),
            namespace: "agents-tasks".into(),
        };
        let http = reqwest::Client::new();
        let s = sync_doc_dirty(&http, &cfg, dir.path()).await;
        assert_eq!(s.failed_server, 1);
        assert_eq!(
            std::fs::read_dir(dirty_dir).unwrap().count(),
            1,
            "5xx 保留标记"
        );
    }

    /// sync_doc_dirty：disabled 是 no-op。
    #[tokio::test]
    async fn sync_doc_dirty_disabled_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let dirty_dir = dir.path().join(".latte/.docs-dirty");
        std::fs::create_dir_all(&dirty_dir).unwrap();
        std::fs::write(dirty_dir.join("a.md"), "entities/a.md").unwrap();
        let cfg = NotionSyncConfig {
            enabled: false,
            base_url: "http://127.0.0.1:1".into(),
            token: String::new(),
            namespace: "agents-tasks".into(),
        };
        let http = reqwest::Client::new();
        let s = sync_doc_dirty(&http, &cfg, dir.path()).await;
        assert_eq!(s.attempted, 0);
        assert_eq!(
            std::fs::read_dir(dirty_dir).unwrap().count(),
            1,
            "disabled 不消费标记"
        );
    }
}
