# doc-graph 工具 + write_doc workflow + Notion 文档镜像 — 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给 latte-rs-agents 的 manager 提供 4 个 doc-graph 工具（scan / context / write / index）+ 1 个 write_doc workflow，并把文档镜像到本地 latte-rs-notion-client → Notion（agents-docs）。

**Architecture:** 工具在 `latte-agent-core` 新 `doc_graph_tools.rs` 模块中实现，通过调 `latte-review` CLI 子进程（env `LATTE_REVIEW_BIN` 或 PATH 回退）执行 scan/context；`doc_write` 生成 frontmatter 落盘 `.latte-review/docs/` + 写 `.latte/.docs-dirty/` 标记 + 重建图；`doc_index` 生成 `docs/index.md`。ui-server 的 `notion_sync.rs` 扩展 `agents-docs` 镜像：轮询 `.latte/.docs-dirty/` → PUT → 删标记。

**Tech Stack:** Rust (tokio::process, serde_json, Command), latte-doc-review-graph CLI (via `latte-review` binary), reqwest, wiremock (test).

---

## Task 1: 新模块 `doc_graph_tools.rs` 骨架 + 2 个只读工具（scan / context）

**Files:**
- Create: `latte-agent-core/src/doc_graph_tools.rs`
- Modify: `latte-agent-core/src/lib.rs`（`pub mod doc_graph_tools;`）

### Step 1: 写 lib.rs 模块声明（编译失败，red）

```rust
pub mod doc_graph_tools;
```

### Step 2: 写 scan 的失败测试（red）

`latte-agent-core/src/doc_graph_tools.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scan_parses_cli_json_output() {
        // 用一个假 "latte-review" binary：真实调 `latte-review scan -C <tmp> --json`，
        // 断言输出解析成 Stats。
        // （真实路径：若 LATTE_REVIEW_BIN 未设且 PATH 无 latte-review，返回 Err。）
        let out = run_scan("/tmp").await;
        // 期望：Err("latte-review 命令未找到...") 或 Ok（若 binary 存在）。
        // 为可测，用 env 注入假 binary。
    }
}
```

（注：测试依赖真实 latte-review 二进制，跨仓不保证存在。所以核心测试集中在**纯函数**上——命令构建、输出解析、路径计算。CLI 调用用注入 binary 的方式做最小冒烟。）

### Step 3: 实现纯函数 + scan/context

```rust
/// doc_graph_scan / doc_graph_context 的 cwd 来源（包在 SharedToolHandler 闭包里）。
pub fn resolve_latte_review_bin() -> Result<String, String> {
    if let Ok(b) = std::env::var("LATTE_REVIEW_BIN") {
        if !b.trim().is_empty() { return Ok(b.trim().to_string()); }
    }
    // 回退 PATH（系统会找）。
    Ok("latte-review".to_string())
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ScanStats { pub node_count: usize, pub edge_count: usize, pub community_count: usize, pub orphan_count: usize }

pub async fn run_scan(cwd: &Path) -> Result<ScanStats, String> {
    let bin = resolve_latte_review_bin()?;
    let out = tokio::process::Command::new(&bin)
        .args(["scan", "-C"])
        .arg(cwd)
        .arg("--json")
        .output().await
        .map_err(|e| format!("无法执行 {bin}: {e}"))?;
    if !out.status.success() {
        return Err(format!("scan 失败: {}", String::from_utf8_lossy(&out.stderr)));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("解析 scan 输出失败: {e}"))
}

pub async fn run_context(cwd: &Path, query: &str) -> Result<serde_json::Value, String> {
    let bin = resolve_latte_review_bin()?;
    let out = tokio::process::Command::new(&bin)
        .args(["context", "-C"])
        .arg(cwd)
        .arg(query)
        .arg("--json")
        .output().await
        .map_err(|e| format!("无法执行 {bin}: {e}"))?;
    if !out.status.success() {
        return Err(format!("context 失败: {}", String::from_utf8_lossy(&out.stderr)));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("解析 context 输出失败: {e}"))
}
```

---

## Task 2: 4 个工具注册（scan / context / write / index）

**Files:**
- Modify: `latte-agent-core/src/doc_graph_tools.rs`
- Modify: `latte-agent-core/src/controller.rs`（build_runner 注册）

### Step 1: doc_write 纯函数（frontmatter 生成 + slug + 路径）

```rust
pub fn slugify(title: &str) -> String {
    title.to_lowercase().chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == ' ')
        .map(|c| if c == ' ' { '-' } else { c })
        .collect::<String>()
        .trim_matches('-').to_string()
}

/// doc_type → 子目录（与 doc-graph 的 docs/{type}/ 布局一致）。
pub fn doc_type_dir(doc_type: &str) -> String {
    match doc_type.to_lowercase().as_str() {
        "entity" => "entities",
        "concept" => "concepts",
        "feature" => "features",
        _ => "entities", // 默认落 entities（最小惊讶）
    }
}

pub fn build_frontmatter(title: &str, doc_type: &str, sources: &[String], tags: &[String], now_unix: i64) -> String {
    // 手写 YAML——doc-graph frontmatter 解析接受宽松格式。
    let mut s = String::new();
    s.push_str("---\n");
    s.push_str(&format!("type: {doc_type}\n"));
    s.push_str(&format!("title: \"{title}\"\n"));
    if !sources.is_empty() {
        s.push_str("sources:\n");
        for src in sources { s.push_str(&format!("  - \"{src}\"\n")); }
    }
    if !tags.is_empty() {
        s.push_str("tags:\n");
        for t in tags { s.push_str(&format!("  - \"{t}\"\n")); }
    }
    // YYYY-MM-DD date
    let date = chrono::DateTime::from_timestamp(now_unix, 0)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default();
    s.push_str(&format!("created: {date}\nupdated: {date}\n"));
    s.push_str("---\n");
    s
}

/// 写 .md 到 `<cwd>/.latte-review/docs/{dir}/{slug}.md`，返回相对路径。
pub fn write_doc_md(cwd: &Path, title: &str, doc_type: &str, sources: &[String], tags: &[String], body: &str, now_unix: i64) -> Result<String, String> {
    let dir = cwd.join(".latte-review").join("docs").join(doc_type_dir(doc_type));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let slug = slugify(title);
    if slug.is_empty() { return Err("slug 为空".into()); }
    let rel = format!("{}/{}", doc_type_dir(doc_type), format!("{slug}.md"));
    let path = cwd.join(".latte-review").join("docs").join(&rel);
    let front = build_frontmatter(title, doc_type, sources, tags, now_unix);
    std::fs::write(&path, format!("{front}{body}")).map_err(|e| e.to_string())?;
    Ok(rel)
}
```

### Step 2: 注册 4 个工具（doc_graph_tools.rs 提供 register fn）

```rust
pub fn register_doc_graph_tools(
    tm: &Arc<dyn ToolManager>,
    cwd: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    // scan
    tm.register(Tool::builder("doc_graph_scan",
        "重建 doc-graph 知识图谱索引(.latte-review/graph.json)。参数：无。",
        ToolInputSchema { properties: vec![].into_iter().collect(), ..Default::default() },
        Arc::new(move |input: serde_json::Value, _ctx| {
            let cwd = cwd.clone();
            Box::pin(async move {
                let stats = run_scan(&cwd).await.map_err(tool_err)?;
                Ok(serde_json::json!({ "ok": true, "node_count": stats.node_count, ... }))
            })
        }),
    ).build(), None);

    // context: {query, max_docs?, budget?}
    // write: {title, doc_type?, sources?, tags?, body}
    // index: {} or {rebuild?}
}
```

---

## Task 3: controller.rs 注册 + manager 配置

**Files:**
- Modify: `latte-agent-core/src/controller.rs`（build_runner 里加 4 个 if 分支）
- Modify: `config/agents/manager.toml`（tools 列表加 4 个）
- Modify: `prompts/manager.md`（加 4 工具说明）

---

## Task 4: write_doc workflow

**Files:**
- Create: `config/workflows/write_doc.toml`

---

## Task 5: Notion agents-docs 镜像

**Files:**
- Modify: `latte-agent-ui-server/src/notion_sync.rs`
- Modify: `latte-agent-ui-server/src/tasks.rs`（如需要）或独立

### doc_write 标 dirty

doc_write 落盘后写 `<cwd>/.latte/.docs-dirty/{rel_path}`（用相对路径文件名，考虑嵌套目录——用 base64 URL-safe 编码文件名或 `.` 替换 `/`）。

### ui-server loop

扩展 `notion_sync_loop`：读 `.latte/.docs-dirty/` 目录 → 对每个标记读对应 `.md` → PUT agents-docs → 删标记。

---

## Task 6: 全 workspace 测试 + commit
