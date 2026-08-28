//! `docs/api-reference.md` 完整性校验：扫 `latte-agent-ui-server/src/lib.rs`
//! 的所有 `.route(...)` 声明，断言每个端点路径前缀至少出现在 API 文档
//! 某个章节标题中。
//!
//! 增删路由时如果忘了同步 `docs/api-reference.md`，CI 即可发现。

use std::fs;

#[cfg(test)]
fn read_lib_routes() -> Vec<String> {
    let raw = fs::read_to_string("../latte-agent-ui-server/src/lib.rs")
        .or_else(|_| fs::read_to_string("src/lib.rs"))
        .expect("lib.rs");
    let mut routes = Vec::new();
    let mut cursor = 0;
    while let Some(idx) = raw[cursor..].find(".route(") {
        let abs = cursor + idx + ".route(".len();
        let slice = &raw[abs..];
        let q = slice.find('"');
        if let Some(qstart) = q {
            let rest = &slice[qstart + 1..];
            let qend = rest.find('"');
            if let Some(qe) = qend {
                let path = rest[..qe].to_string();
                if path.starts_with('/') && !routes.contains(&path) {
                    routes.push(path);
                }
            }
        }
        cursor = abs + 1;
    }
    routes.sort();
    routes
}

#[cfg(test)]
fn read_api_ref_chapter_keywords() -> Vec<String> {
    let raw = fs::read_to_string("../docs/api-reference.md")
        .or_else(|_| fs::read_to_string("docs/api-reference.md"))
        .expect("api-reference.md");
    let mut out = Vec::new();
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            out.push(rest.to_lowercase());
        }
        if let Some(rest) = line.strip_prefix("### `") {
            out.push(rest.to_lowercase());
        }
    }
    out
}

/// 把端点路径规范化成可比较形式。
///
/// 两边写法不统一，必须先归一：
///   - 路由是 `/traces/:id`（无 `/api` 前缀，axum 风格参数）；
///   - 文档是 `` `GET /api/traces/<session_id>` ``（有前缀，尖括号参数），
///     还混用 `:id`、`{key}`，以及 `?id=<sid>` 这种查询串。
///
/// 规则：去查询串 → 路径参数统一成 `*` → 补 `/api` 前缀。
///
/// 例外：`/health` 按设计**不在** `/api/` 下（见 api-reference.md 开头的说明），
/// 所以对根路径端点保持原样，不硬套前缀。
fn normalize_endpoint(path: &str) -> String {
    let p = path.split('?').next().unwrap_or(path);
    let p = p.trim();
    let p = p.strip_prefix("/api").unwrap_or(p);
    let segs: Vec<String> = p
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| {
            let is_param = s.starts_with(':')
                || (s.starts_with('<') && s.ends_with('>'))
                || (s.starts_with('{') && s.ends_with('}'));
            if is_param { "*".to_string() } else { s.to_ascii_lowercase() }
        })
        .collect();
    let joined = segs.join("/");
    // 根路径端点（当前只有 health）不套 /api 前缀。
    if ROOT_LEVEL_ENDPOINTS.contains(&joined.as_str()) {
        return format!("/{joined}");
    }
    format!("/api/{joined}")
}

/// 按设计挂在根路径、不在 `/api/` 下的端点。新增此类端点时在此登记，
/// 否则校验会因为前缀不匹配误报。
const ROOT_LEVEL_ENDPOINTS: &[&str] = &["health"];

/// 从 `docs/api-reference.md` 的 `### ` 标题里抽出所有已登记端点。
#[cfg(test)]
fn read_api_ref_endpoints() -> std::collections::HashSet<String> {
    let raw = fs::read_to_string("../docs/api-reference.md")
        .or_else(|_| fs::read_to_string("docs/api-reference.md"))
        .expect("api-reference.md");
    let mut out = std::collections::HashSet::new();
    for line in raw.lines() {
        if !line.starts_with("###") {
            continue;
        }
        // 按 HTTP 方法定位路径：不能只找 `/api/`，因为 `/health` 按设计
        // 不在该前缀下（漏了它会把已登记端点误报成未登记）。
        // 一行里可能出现多个 `METHOD /path`（同一路径多方法合写）。
        for m in ["GET ", "POST ", "PUT ", "PATCH ", "DELETE "] {
            let mut rest = line;
            while let Some(at) = rest.find(m) {
                let tail = &rest[at + m.len()..];
                if !tail.starts_with('/') {
                    rest = &rest[at + m.len()..];
                    continue;
                }
                let end = tail
                    .find(|c: char| c.is_whitespace() || c == '`' || c == ')' || c == ',')
                    .unwrap_or(tail.len());
                out.insert(normalize_endpoint(&tail[..end]));
                rest = &tail[end..];
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每条 `.route(...)` 都必须在 `docs/api-reference.md` 里有对应端点标题。
    ///
    /// ## 此前为什么形同虚设
    ///
    /// 旧实现有两处缺陷叠加：
    ///
    /// 1. **认不出的路由直接放行**：靠一张手写的「路径前缀 → 章节关键词」映射，
    ///    映射里没有的路由返回空关键词、然后 `continue`。于是**新增一个未登记
    ///    类别的端点永远不会报错**——而这恰恰是本校验唯一要防的场景。
    ///    实锤：`/logs` 加进路由、文档没写，校验照样绿。
    /// 2. **匹配粒度太粗**：只要求「关键词出现在某个章节标题里」。`/traces/:id`
    ///    映射到关键词 `trace`，而文档有 `## 6. Trace / 调试` 章节 → 命中。
    ///    即**同类别下有任意章节，该类别的新端点全部自动通过**。
    ///
    /// 现在改为逐端点精确比对，且**没有放行分支**：路由不在文档里就失败。
    #[test]
    fn every_route_is_documented() {
        let documented = read_api_ref_endpoints();
        let routes = read_lib_routes();
        assert!(
            !routes.is_empty(),
            "没解析到任何路由，说明 read_lib_routes 的解析失效了（比 false-pass 更危险）"
        );
        let missing: Vec<String> = routes
            .iter()
            .filter(|r| !documented.contains(&normalize_endpoint(r)))
            .map(|r| format!("  - {r}  (规范化后: {})", normalize_endpoint(r)))
            .collect();
        assert!(
            missing.is_empty(),
            "以下路由没有在 docs/api-reference.md 里登记（新增端点请同步文档）：\n{}\n\n             文档中已登记 {} 个端点。",
            missing.join("\n"),
            documented.len()
        );
    }

    /// 规范化本身的行为锁定：两边三种参数写法必须归一到同一个 key。
    #[test]
    fn normalize_unifies_param_styles_and_prefix() {
        let expect = "/api/traces/*";
        for form in [
            "/traces/:id",                 // 路由写法
            "/api/traces/<session_id>",    // 文档尖括号
            "/api/traces/{id}",            // 文档花括号
            "/api/traces/:id",             // 文档冒号
            "/api/traces/<session_id>?x=1" // 带查询串
        ] {
            assert_eq!(normalize_endpoint(form), expect, "form = {form}");
        }
        // 大小写与多段
        assert_eq!(normalize_endpoint("/Models/:key/TOML"), "/api/models/*/toml");
        // 根路径端点不套 /api 前缀（否则 /health 会被误报未登记）。
        assert_eq!(normalize_endpoint("/health"), "/health");
    }
}
