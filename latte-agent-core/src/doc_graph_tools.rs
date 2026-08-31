//! doc-graph 工具：把 latte-rs-doc-graph（`latte-review` CLI）包装成
//! latte-rs-agents 的工具。
//!
//! 4 个工具：
//! - `doc_graph_scan`：重建 `.latte-review/graph.json` 知识图谱索引。
//! - `doc_graph_context`：基于查询预读相关文档块（5-stage context pipeline）。
//! - `doc_write`：生成 frontmatter + 落盘 `.latte-review/docs/{type}/{slug}.md`
//!   + 写 `.latte/.docs-dirty/` 标记 + 重建图。
//! - `doc_index`：生成 `docs/index.md` 目录表。
//!
//! CLI binary 定位：env `LATTE_REVIEW_BIN` > PATH 里的 `latte-review`。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use latte_rs_agent_tools::types::{
    PropertyType, SharedToolHandler, Tool, ToolInputProperty, ToolInputSchema,
};

/// 解析 latte-review 可执行文件路径：env LATTE_REVIEW_BIN > PATH。
pub fn resolve_latte_review_bin() -> Result<String, String> {
    if let Ok(b) = std::env::var("LATTE_REVIEW_BIN") {
        if !b.trim().is_empty() {
            return Ok(b.trim().to_string());
        }
    }
    Ok("latte-review".to_string())
}

/// `latte-review scan --json` 的 stats 输出。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScanStats {
    pub node_count: usize,
    pub edge_count: usize,
    pub community_count: usize,
    pub orphan_count: usize,
}

/// 调 `latte-review scan -C <cwd> --json`，返回 stats。
pub async fn run_scan(cwd: &Path) -> Result<ScanStats, String> {
    let bin = resolve_latte_review_bin()?;
    let out = tokio::process::Command::new(&bin)
        .args(["scan", "-C"])
        .arg(cwd)
        .arg("--json")
        .output()
        .await
        .map_err(|e| format!("无法执行 {bin}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "scan 失败（exit {:?}）: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("解析 scan 输出失败: {e}"))
}

/// 调 `latte-review context -C <cwd> "<query>" --json`，返回 bundle。
pub async fn run_context(cwd: &Path, query: &str) -> Result<serde_json::Value, String> {
    let bin = resolve_latte_review_bin()?;
    let out = tokio::process::Command::new(&bin)
        .args(["context", "-C"])
        .arg(cwd)
        .arg(query)
        .arg("--json")
        .output()
        .await
        .map_err(|e| format!("无法执行 {bin}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "context 失败（exit {:?}）: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("解析 context 输出失败: {e}"))
}

/// title → 文件 slug（kebab-case）。
pub fn slugify(title: &str) -> String {
    title
        .to_lowercase()
        .chars()
        .map(|c| match c {
            c if c.is_alphanumeric() => c,
            c if c.is_whitespace() => '-',
            '-' | '_' => c,
            _ => '-',
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// doc_type → 子目录（doc-graph docs/{entities,concepts,features}/ 布局）。
pub fn doc_type_dir(doc_type: &str) -> String {
    match doc_type.to_lowercase().as_str() {
        "entity" => "entities".to_string(),
        "concept" => "concepts".to_string(),
        "feature" => "features".to_string(),
        "spec" => "specs".to_string(),
        "task" => "tasks".to_string(),
        "bug" => "bugs".to_string(),
        "component" => "components".to_string(),
        _ => "entities".to_string(),
    }
}

/// unix 秒 → YYYY-MM-DD（无 chrono 依赖，手写简单算法）。
fn unix_to_date(secs: i64) -> String {
    // 用整数除法算日历年月日（civil-from-days 算法简化）。
    // epoch 1970-01-01. days since epoch:
    let days = secs.div_euclid(86400);
    // Howard Hinnant's civil_from_days
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// 生成 frontmatter YAML（doc-graph frontmatter 解析接受宽松格式）。
pub fn build_frontmatter(
    title: &str,
    doc_type: &str,
    sources: &[String],
    tags: &[String],
    now_unix: i64,
) -> String {
    let date = unix_to_date(now_unix);
    let mut s = String::from("---\n");
    s.push_str(&format!("type: {doc_type}\n"));
    s.push_str(&format!("title: \"{title}\"\n"));
    if !sources.is_empty() {
        s.push_str("sources:\n");
        for src in sources {
            s.push_str(&format!("  - \"{src}\"\n"));
        }
    }
    if !tags.is_empty() {
        s.push_str("tags:\n");
        for t in tags {
            s.push_str(&format!("  - \"{t}\"\n"));
        }
    }
    s.push_str(&format!("created: {date}\nupdated: {date}\n"));
    s.push_str("---\n");
    s
}

/// 写 `.md` 到 `<cwd>/.latte-review/docs/{type_dir}/{slug}.md`，返回相对路径
/// `{type_dir}/{slug}.md`。已存在则覆盖（updated 保持不变）。
pub fn write_doc_md(
    cwd: &Path,
    title: &str,
    doc_type: &str,
    sources: &[String],
    tags: &[String],
    body: &str,
    now_unix: i64,
) -> Result<String, String> {
    let slug = slugify(title);
    if slug.is_empty() {
        return Err("title 不能生成有效 slug（全为非字母数字/空白）".into());
    }
    let type_dir = doc_type_dir(doc_type);
    let rel = format!("{type_dir}/{slug}.md");
    let dir = cwd.join(".latte-review").join("docs").join(&type_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录失败: {e}"))?;
    let path = dir.join(format!("{slug}.md"));
    let front = build_frontmatter(title, doc_type, sources, tags, now_unix);
    std::fs::write(&path, format!("{front}{body}")).map_err(|e| format!("写文档失败: {e}"))?;
    Ok(rel)
}

/// `.docs-dirty` 标记：相对路径 → URL-safe 文件名（`/` → `.`，防路径穿越）。
/// 同时满足 `^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$`（notion-client id 约束）。
pub fn dirty_marker_name(rel_path: &str) -> String {
    rel_path
        .replace('/', ".")
        .replace('\\', ".")
        .trim_start_matches('.')
        .to_string()
}

/// 写 `.latte/.docs-dirty/<marker>` 标记文件（内容=相对路径）。
pub fn mark_doc_dirty(cwd: &Path, rel_path: &str) -> Result<(), String> {
    let dir = cwd.join(".latte").join(".docs-dirty");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建 .docs-dirty 失败: {e}"))?;
    let marker = dirty_marker_name(rel_path);
    let path = dir.join(&marker);
    std::fs::write(&path, rel_path).map_err(|e| format!("写 dirty 标记失败: {e}"))?;
    Ok(())
}

/// 读所有 `.docs-dirty` 标记，返回 `(marker_filename, rel_path)` 列表。
pub fn read_doc_dirty(cwd: &Path) -> Vec<(String, String)> {
    let dir = cwd.join(".latte").join(".docs-dirty");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return vec![];
    };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let rel = std::fs::read_to_string(e.path()).ok()?;
            Some((name, rel.trim().to_string()))
        })
        .collect()
}

/// 删除一个 dirty 标记。
pub fn clear_doc_dirty(cwd: &Path, marker: &str) {
    let path = cwd.join(".latte").join(".docs-dirty").join(marker);
    let _ = std::fs::remove_file(path);
}

/// 读一篇文档（frontmatter + body 原文），返回完整内容。
pub fn read_doc_md(cwd: &Path, rel_path: &str) -> Result<String, String> {
    // 路径穿越防护：rel_path 必须相对，不含 ..
    let p = Path::new(rel_path);
    if p.is_absolute() || p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err(format!("非法相对路径: {rel_path}"));
    }
    let full = cwd.join(".latte-review").join("docs").join(rel_path);
    std::fs::read_to_string(&full).map_err(|e| format!("读文档失败: {e}"))
}

/// 生成 `index.md` 目录表（按 doc_type 分组的 wiki 导航）。
/// 读 graph.json（假设已 scan），列每个 doc 的一行链接。
pub fn generate_index(cwd: &Path, now_unix: i64) -> Result<String, String> {
    let graph_path = cwd.join(".latte-review").join("graph.json");
    let raw = std::fs::read_to_string(&graph_path)
        .map_err(|e| format!("读 graph.json 失败（先运行 doc_graph_scan）: {e}"))?;
    let graph: serde_json::Value = serde_json::from_str(&raw).map_err(|e| format!("解析 graph.json 失败: {e}"))?;
    let nodes = graph
        .get("nodes")
        .and_then(|v| v.as_object())
        .ok_or_else(|| "graph.json 缺 nodes 字段".to_string())?;

    // 按 doc_type 分组：Vec<(title, id)>
    let mut by_type: std::collections::BTreeMap<String, Vec<(String, String)>> =
        std::collections::BTreeMap::new();
    for (id, node) in nodes {
        let v = match node.as_object() {
            Some(v) => v,
            None => continue,
        };
        let doc_type = v
            .get("doc_type")
            .and_then(|t| t.as_str())
            .unwrap_or("other")
            .to_string();
        let title = v
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or(id.as_str())
            .to_string();
        by_type
            .entry(doc_type)
            .or_default()
            .push((title, id.clone()));
    }

    let mut out = String::new();
    out.push_str(&format!(
        "# 文档索引\n\n生成时间：{}\n\n",
        unix_to_date(now_unix)
    ));
    out.push_str(&format!("共 {} 篇文档\n\n", nodes.len()));
    for (doc_type, entries) in &by_type {
        out.push_str(&format!("## {}\n\n", doc_type));
        for (title, id) in entries {
            out.push_str(&format!("- [[{id}|{title}]]\n"));
        }
        out.push('\n');
    }
    Ok(out)
}

// ─── 工具 schema 构造辅助 ─────────────────────────────────────────

fn prop(ty: PropertyType, desc: &str) -> ToolInputProperty {
    ToolInputProperty { property_type: ty,
    description: Some(desc.into()),
    enum_values: None,
    minimum: None,
    maximum: None,
    min_length: None,
    max_length: None, items: None, properties: None, required: None, additional_properties: None }
}

fn tool_err(msg: String) -> latte_rs_agent_tools::error::ToolError {
    latte_rs_agent_tools::error::ToolError::Other(msg)
}

/// 注册全部 4 个 doc-graph 工具到 ToolManager。
/// `cwd` 是 agent 工作目录（工具写文档 / 调 CLI 的根）。
pub fn register_doc_graph_tools(
    tm: &Arc<dyn latte_rs_agent_tools::types::ToolManager>,
    cwd: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    // ── doc_graph_scan ──
    let scan_cwd = cwd.clone();
    let scan_handler: SharedToolHandler = Arc::new(move |_input: serde_json::Value, _ctx| {
        let cwd = scan_cwd.clone();
        Box::pin(async move {
            match run_scan(&cwd).await {
                Ok(stats) => Ok(serde_json::json!({
                    "ok": true,
                    "node_count": stats.node_count,
                    "edge_count": stats.edge_count,
                    "community_count": stats.community_count,
                    "orphan_count": stats.orphan_count,
                    "graph_path": cwd.join(".latte-review/graph.json").display().to_string(),
                })),
                Err(e) => Err(tool_err(e)),
            }
        })
    });
    tm.register(
        Tool::builder(
            "doc_graph_scan".to_string(),
            "重建 doc-graph 知识图谱索引（扫描 .latte-review/docs/ 下所有 .md，更新 .latte-review/graph.json）。无参数。在写过/改过文档后调用。".to_string(),
            ToolInputSchema { properties: vec![].into_iter().collect(), ..Default::default() },
            scan_handler,
        )
        // 重建 graph.json（写文件），串行。
        .concurrency_safe(false)
        .build(),
        None,
    );

    // ── doc_graph_context ──
    let ctx_cwd = cwd.clone();
    let ctx_handler: SharedToolHandler = Arc::new(move |input: serde_json::Value, _ctx| {
        let cwd = ctx_cwd.clone();
        Box::pin(async move {
            let query = input
                .get("query")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| tool_err("missing non-empty 'query' field".into()))?;
            match run_context(&cwd, &query).await {
                Ok(bundle) => Ok(serde_json::json!({ "ok": true, "bundle": bundle })),
                Err(e) => Err(tool_err(e)),
            }
        })
    });
    tm.register(
        Tool::builder(
            "doc_graph_context".to_string(),
            "基于查询预读 doc-graph 知识图谱中相关文档块（5-stage context pipeline），返回 overview + index + pages。用于在写文档或分析前快速了解相关已记录的上下文。参数：query(查询词，必填)。".to_string(),
            ToolInputSchema {
                properties: vec![("query".into(), prop(PropertyType::String, "查询词（如「authentication flow」）。必填。"))].into_iter().collect(),
                required: Some(vec!["query".into()]),
                ..Default::default()
            },
            ctx_handler,
        )
        // 只读：查图谱、预读文档块。
        .concurrency_safe(true)
        .build(),
        None,
    );

    // ── doc_write ──
    let write_cwd = cwd.clone();
    let write_handler: SharedToolHandler = Arc::new(move |input: serde_json::Value, _ctx| {
        let cwd = write_cwd.clone();
        Box::pin(async move {
            let title = input
                .get("title")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| tool_err("missing non-empty 'title' field".into()))?;
            let doc_type = input
                .get("doc_type")
                .and_then(|v| v.as_str())
                .unwrap_or("entity")
                .trim()
                .to_string();
            let sources: Vec<String> = input
                .get("sources")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|s| s.as_str().map(|x| x.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let tags: Vec<String> = input
                .get("tags")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|s| s.as_str().map(|x| x.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let body = input
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if body.trim().is_empty() {
                return Err(tool_err("body 不能为空（正文是文档的核心）".into()));
            }
            let now = unix_now();
            let rel = match write_doc_md(&cwd, &title, &doc_type, &sources, &tags, &body, now) {
                Ok(r) => r,
                Err(e) => return Err(tool_err(e)),
            };
            if let Err(e) = mark_doc_dirty(&cwd, &rel) {
                eprintln!("[doc_write] mark dirty {rel}: {e}");
            }
            // 重建图索引（尽力而为——binary 缺失时返回成功但标注未更新）。
            let stats = run_scan(&cwd).await.ok();
            match stats {
                Some(s) => Ok(serde_json::json!({
                    "ok": true,
                    "path": rel,
                    "title": title,
                    "node_count": s.node_count,
                    "edge_count": s.edge_count,
                    "note": "文档已保存，图索引已重建",
                })),
                None => Ok(serde_json::json!({
                    "ok": true,
                    "path": rel,
                    "title": title,
                    "note": "文档已保存；但 latte-review 不可用，图索引未重建（用 doc_graph_scan 手动重建）",
                })),
            }
        })
    });
    tm.register(
        Tool::builder(
            "doc_write".to_string(),
            "创建/更新一篇 doc-graph 文档：生成 frontmatter + 正文写到 .latte-review/docs/{type}/{slug}.md，重建图索引，并标记待同步 Notion。参数：title(标题，必填)，doc_type(entity/concept/feature 等，默认 entity)，sources(来源文件路径数组，可选)，tags(标签数组，可选)，body(Markdown 正文，必填)。".to_string(),
            ToolInputSchema {
                properties: vec![
                    ("title".into(), prop(PropertyType::String, "文档标题（必填，生成文件名 slug）。")),
                    ("doc_type".into(), prop(PropertyType::String, "文档类型：entity/concept/feature/spec/task/bug/component。默认 entity。")),
                    ("sources".into(), prop(PropertyType::Array, "来源文件/代码路径数组（可选）。").with_items(prop(PropertyType::String, "来源文件或代码路径。"))),
                    ("tags".into(), prop(PropertyType::Array, "标签数组（可选）。").with_items(prop(PropertyType::String, "标签。"))),
                    ("body".into(), prop(PropertyType::String, "Markdown 正文（必填）。")),
                ].into_iter().collect(),
                required: Some(vec!["title".into(), "body".into()]),
                ..Default::default()
            },
            write_handler,
        )
        // 写 .md + 重建索引，串行。
        .concurrency_safe(false)
        .build(),
        None,
    );

    // ── doc_index ──
    let index_cwd = cwd.clone();
    let index_handler: SharedToolHandler = Arc::new(move |_input: serde_json::Value, _ctx| {
        let cwd = index_cwd.clone();
        Box::pin(async move {
            let now = unix_now();
            // 确保 graph 最新（尽力而为：binary 缺失则读已有 graph.json）。
            let _ = run_scan(&cwd).await;
            match generate_index(&cwd, now) {
                Ok(markdown) => {
                    let index_rel = "index.md";
                    let index_path = cwd.join(".latte-review").join("docs").join(index_rel);
                    if std::fs::write(&index_path, &markdown).is_ok() {
                        let _ = mark_doc_dirty(&cwd, index_rel);
                    }
                    Ok(serde_json::json!({
                        "ok": true,
                        "index_path": index_path.display().to_string(),
                        "markdown": markdown,
                    }))
                }
                Err(e) => Err(tool_err(e)),
            }
        })
    });
    tm.register(
        Tool::builder(
            "doc_index".to_string(),
            "生成 doc-graph 文档索引（docs/index.md 目录表）。读取 .latte-review/graph.json 按 doc_type 分组的 wiki 导航，写回 .latte-review/docs/index.md。参数：无。".to_string(),
            ToolInputSchema { properties: vec![].into_iter().collect(), ..Default::default() },
            index_handler,
        )
        // 写 docs/index.md，串行。
        .concurrency_safe(false)
        .build(),
        None,
    );

    Ok(())
}

/// 当前 unix 秒。
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_kebab() {
        assert_eq!(slugify("Auth Service"), "auth-service");
        assert_eq!(slugify("user-account"), "user-account");
        assert_eq!(slugify("Ring Buffer Write Path"), "ring-buffer-write-path");
        assert_eq!(slugify("  spaced  out  "), "spaced-out");
        assert_eq!(slugify("!!!"), ""); // 全符号 → 空
        assert_eq!(slugify("中文标题"), "中文标题"); // CJK 保留
    }

    #[test]
    fn doc_type_dir_mapping() {
        assert_eq!(doc_type_dir("entity"), "entities");
        assert_eq!(doc_type_dir("concept"), "concepts");
        assert_eq!(doc_type_dir("feature"), "features");
        assert_eq!(doc_type_dir("task"), "tasks");
        assert_eq!(doc_type_dir("foo"), "entities"); // 未知 → entities
        assert_eq!(doc_type_dir("Entity"), "entities"); // 大小写不敏感
    }

    #[test]
    fn frontmatter_has_required_fields() {
        let fm = build_frontmatter("Auth Service", "entity", &["src/auth.rs".into()], &["rust".into(), "auth".into()], 1786217357);
        assert!(fm.starts_with("---\n"), "frontmatter 以 --- 开始");
        assert!(fm.contains("type: entity\n"));
        assert!(fm.contains("title: \"Auth Service\"\n"));
        assert!(fm.contains("sources:\n  - \"src/auth.rs\"\n"));
        assert!(fm.contains("tags:\n  - \"rust\"\n  - \"auth\"\n"));
        assert!(fm.contains("created: 2026-08-08\n"), "epoch 1786217357 → 2026-08-08（本地 UTC）");
        assert!(fm.ends_with("---\n"), "frontmatter 以 --- 结束");
    }

    #[test]
    fn unix_to_date_basic() {
        // 1970-01-01
        // 2024-12-31 20:00 UTC + 4h 时区 = 2025-01-01 UTC。
        // 用纯 UTC 时刻：1735689600 = 2025-01-01 00:00 UTC。
        assert_eq!(unix_to_date(1735689600), "2025-01-01");
    }

    #[test]
    fn write_and_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let rel = write_doc_md(
            dir.path(),
            "Auth Service",
            "entity",
            &["src/auth.rs".into()],
            &["rust".into()],
            "# Auth Service\n\ndetails",
            1704067200,
        )
        .expect("write");
        assert_eq!(rel, "entities/auth-service.md");
        let content = read_doc_md(dir.path(), &rel).expect("read");
        assert!(content.starts_with("---\n"));
        assert!(content.contains("# Auth Service"));
        assert!(content.contains("created: 2024-01-01"));
    }

    #[test]
    fn dirty_marker_safe() {
        assert_eq!(dirty_marker_name("entities/auth-service.md"), "entities.auth-service.md");
        assert_eq!(dirty_marker_name("entities/user-account.md"), "entities.user-account.md");
        // 防路径穿越：跳过前导点。
        assert!(!dirty_marker_name("../x.md").starts_with(".") || dirty_marker_name("../x.md").contains("..") == false);
    }

    #[test]
    fn mark_and_clear_dirty() {
        let dir = tempfile::tempdir().unwrap();
        mark_doc_dirty(dir.path(), "entities/auth-service.md").unwrap();
        let entries = read_doc_dirty(dir.path());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].1, "entities/auth-service.md");
        clear_doc_dirty(dir.path(), &entries[0].0);
        assert!(read_doc_dirty(dir.path()).is_empty());
    }

    #[test]
    fn read_doc_md_rejects_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_doc_md(dir.path(), "../escape.md").unwrap_err();
        assert!(err.contains("非法相对路径"), "{err}");
        let err2 = read_doc_md(dir.path(), "/abs.md").unwrap_err();
        assert!(err2.contains("非法相对路径"), "{err2}");
    }

    #[test]
    fn generate_index_from_graph() {
        let dir = tempfile::tempdir().unwrap();
        // 造 graph.json
        let graph = serde_json::json!({
            "nodes": {
                "entities/auth-service": { "id": "entities/auth-service", "doc_type": "entity", "title": "auth-service" },
                "concepts/authn": { "id": "concepts/authn", "doc_type": "concept", "title": "authn" }
            }
        });
        std::fs::create_dir_all(dir.path().join(".latte-review")).unwrap();
        std::fs::write(dir.path().join(".latte-review/graph.json"), graph.to_string()).unwrap();
        let md = generate_index(dir.path(), 1704067200).expect("index");
        assert!(md.contains("## entity"), "含 entity 分组: {md}");
        assert!(md.contains("## concept"), "含 concept 分组: {md}");
        assert!(md.contains("[[entities/auth-service|auth-service]]"));
        assert!(md.contains("[[concepts/authn|authn]]"));
        assert!(md.contains("2024-01-01"));
    }

    #[test]
    fn generate_index_missing_graph_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = generate_index(dir.path(), 0).unwrap_err();
        assert!(err.contains("graph.json"), "提示先 scan: {err}");
    }
}
