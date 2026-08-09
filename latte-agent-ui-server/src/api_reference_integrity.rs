//! `docs/api-reference.md` 完整性校验：扫 `latte-agent-ui-server/src/lib.rs`
//! 的所有 `.route(...)` 声明，断言每个端点路径前缀至少出现在 API 文档
//! 某个章节标题中。
//!
//! 增删路由时如果忘了同步 `docs/api-reference.md`，CI 即可发现。

use std::fs;

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

/// 路由前缀 → 章节关键词（必须出现至少一次；关键词必须小写）。
fn route_to_keywords(route: &str) -> Vec<String> {
    let r = route.trim_start_matches('/');
    let kw = |s: &str| s.to_string();
    if r == "health" {
        return vec![kw("健康检查")];
    }
    if r.starts_with("sessions") || r == "session" || r.starts_with("session/") {
        return vec![kw("会话管理")];
    }
    if r.starts_with("roles/") || r == "roles" {
        if r.contains("config") || r.contains("toml") || r.contains(":id") {
            return vec![kw("角色配置"), kw("角色查询")];
        }
        return vec![kw("角色查询")];
    }
    if r.starts_with("chat/") {
        return vec![kw("聊天命令")];
    }
    if r == "events" {
        return vec![kw("sse 事件流")];
    }
    if r.starts_with("traces") || r == "subsessions" {
        return vec![kw("trace"), kw("调试")];
    }
    if r.starts_with("self-loop") {
        return vec![kw("self-loop")];
    }
    if r == "role-graph" {
        return vec![kw("角色-工具关系图")];
    }
    if r.starts_with("models") {
        return vec![kw("模型管理")];
    }
    if r.starts_with("tools") {
        return vec![kw("工具管理")];
    }
    if r.starts_with("tasks") {
        return vec![kw("任务看板")];
    }
    if r.starts_with("workflows") {
        return vec![kw("工作流管理")];
    }
    if r.starts_with("images") {
        return vec![kw("文档图像上传")];
    }
    vec![]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_route_has_matching_chapter() {
        let chapters = read_api_ref_chapter_keywords();
        let routes = read_lib_routes();
        let mut missing: Vec<(String, Vec<String>)> = Vec::new();
        for r in &routes {
            let kws = route_to_keywords(r);
            if kws.is_empty() {
                continue;
            }
            let ok = kws.iter().any(|kw| chapters.iter().any(|c| c.contains(kw)));
            if !ok {
                missing.push((r.clone(), kws));
            }
        }
        if !missing.is_empty() {
            let detail: Vec<String> = missing
                .iter()
                .map(|(r, kws)| {
                    format!(
                        "  - {} (expected keyword in chapter titles: {:?})",
                        r, kws
                    )
                })
                .collect();
            panic!(
                "routes 缺少文档章节（routes 不为空；路径前缀应在 docs/api-reference.md 章节标题里出现）：\n{}",
                detail.join("\n")
            );
        }
    }
}
