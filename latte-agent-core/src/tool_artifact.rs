//! Owner-scoped spill storage for large successful tool results.
//!
//! The model receives an opaque `artifact://` capability plus a bounded
//! head/tail preview. Full content lives outside the workspace and can only be
//! paged back through `artifact_read` by the same runner owner.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures::FutureExt;
use latte_rs_agent_tools::error::ToolError;
use latte_rs_agent_tools::types::{
    PropertyType, SchemaType, SharedToolHandler, Tool, ToolInputProperty, ToolInputSchema,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Bounded representation of a tool result exposed to the model and event
/// stream. `content_complete` is false whenever any bytes were omitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutputPreview {
    pub content: String,
    pub total_bytes: usize,
    pub content_complete: bool,
    pub artifact: Option<String>,
}

const DEFAULT_SPILL_THRESHOLD: usize = 48 * 1024;
const PREVIEW_HEAD_CHARS: usize = 8 * 1024;
const PREVIEW_TAIL_CHARS: usize = 4 * 1024;
const DEFAULT_READ_BYTES: usize = 16 * 1024;
const MAX_READ_BYTES: usize = 32 * 1024;
const ARTIFACT_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
static SPILL_COUNT: AtomicU64 = AtomicU64::new(0);

fn spill_threshold() -> usize {
    std::env::var("LATTE_AGENT_TOOL_ARTIFACT_THRESHOLD")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_SPILL_THRESHOLD)
        .clamp(8 * 1024, 1024 * 1024)
}

fn root() -> PathBuf {
    std::env::var_os("LATTE_AGENT_TOOL_ARTIFACT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("latte-agent-tool-artifacts"))
}

fn safe_owner(owner: &str) -> Option<String> {
    if owner.trim().is_empty() {
        return None;
    }
    Some(
        owner
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                    ch
                } else {
                    '_'
                }
            })
            .take(160)
            .collect(),
    )
}

fn owner_from_metadata(metadata: Option<&serde_json::Value>) -> Option<String> {
    metadata?.get("job_owner")?.as_str().and_then(safe_owner)
}

fn capability_id(capability: &str) -> Option<&str> {
    let id = capability.trim().strip_prefix("artifact://")?;
    (!id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
    .then_some(id)
}

fn artifact_path(owner: &str, id: &str) -> PathBuf {
    root().join(owner).join(format!("{id}.txt"))
}

fn align_start(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while offset < text.len() && !text.is_char_boundary(offset) {
        offset += 1;
    }
    offset
}

fn align_end(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn preview(text: &str, capability: &str) -> String {
    let chars = text.chars().count();
    let head: String = text.chars().take(PREVIEW_HEAD_CHARS).collect();
    let tail: String = text
        .chars()
        .rev()
        .take(PREVIEW_TAIL_CHARS)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!(
        "{head}\n\n…[工具结果过大：中间内容已保存为 {capability}；原始 {} 字节 / {chars} 字符。需要细节时调用 artifact_read 分页读取]…\n\n{tail}",
        text.len()
    )
}

fn maybe_gc() {
    if SPILL_COUNT.fetch_add(1, Ordering::Relaxed) % 64 != 0 {
        return;
    }
    let now = SystemTime::now();
    let Ok(owners) = std::fs::read_dir(root()) else {
        return;
    };
    for owner in owners.flatten() {
        let path = owner.path();
        if !path.is_dir() {
            continue;
        }
        if let Ok(files) = std::fs::read_dir(&path) {
            for file in files.flatten() {
                let expired = file
                    .metadata()
                    .and_then(|meta| meta.modified())
                    .ok()
                    .and_then(|modified| now.duration_since(modified).ok())
                    .is_some_and(|age| age > ARTIFACT_TTL);
                if expired {
                    let _ = std::fs::remove_file(file.path());
                }
            }
        }
        if std::fs::read_dir(&path)
            .ok()
            .is_some_and(|mut entries| entries.next().is_none())
        {
            let _ = std::fs::remove_dir(path);
        }
    }
}

/// Spill one large model-visible tool result. The returned metadata makes the
/// boundary explicit to trace/UI consumers: a preview is never full evidence.
pub fn spill_if_large(text: String, owner: &str) -> ToolOutputPreview {
    let total_bytes = text.len();
    if total_bytes <= spill_threshold() {
        return ToolOutputPreview {
            content: text,
            total_bytes,
            content_complete: true,
            artifact: None,
        };
    }
    let Some(owner) = safe_owner(owner) else {
        return ToolOutputPreview {
            content: format!(
                "{}\n\n…[工具结果过大且当前 runner 没有稳定 owner，完整内容未持久化；原始 {} 字节]…\n\n{}",
                text.chars().take(PREVIEW_HEAD_CHARS).collect::<String>(),
                total_bytes,
                text.chars()
                    .rev()
                    .take(PREVIEW_TAIL_CHARS)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<String>()
            ),
            total_bytes,
            content_complete: false,
            artifact: None,
        };
    };
    maybe_gc();
    let id = uuid::Uuid::new_v4().to_string();
    let capability = format!("artifact://{id}");
    let path = artifact_path(&owner, &id);
    let stored = path
        .parent()
        .and_then(|dir| std::fs::create_dir_all(dir).ok().map(|_| dir))
        .and_then(|_| {
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, text.as_bytes())
                .ok()
                .and_then(|_| std::fs::rename(&tmp, &path).ok())
        })
        .is_some();
    let artifact = stored.then_some(capability);
    let mut content = preview(&text, artifact.as_deref().unwrap_or("未持久化"));
    if artifact.is_none() {
        content.push_str("\n[artifact 写盘失败，不能调用 artifact_read 取回完整内容]");
    }
    ToolOutputPreview {
        content,
        total_bytes,
        content_complete: false,
        artifact,
    }
}

fn property(property_type: PropertyType, description: &str) -> ToolInputProperty {
    ToolInputProperty {
        property_type,
        description: Some(description.to_string()),
        enum_values: None,
        minimum: None,
        maximum: None,
        min_length: None,
        max_length: None,
        items: None,
        properties: None,
        required: None,
        additional_properties: None,
        ref_: None,
    }
}

/// Build the owner-scoped bounded artifact reader automatically attached to
/// every tool-capable runner.
pub fn artifact_read_tool() -> Tool {
    let mut properties = BTreeMap::new();
    properties.insert(
        "capability".into(),
        property(
            PropertyType::String,
            "工具结果中返回的 artifact:// capability。只能读取当前会话/分派自己的 artifact。",
        ),
    );
    let mut offset = property(
        PropertyType::Integer,
        "从 UTF-8 文件的哪个字节偏移开始读取，默认 0。",
    );
    offset.minimum = Some(0.0);
    properties.insert("offset".into(), offset);
    let mut limit = property(
        PropertyType::Integer,
        "本次最多读取多少字节，默认 16384，硬上限 32768。",
    );
    limit.minimum = Some(1.0);
    limit.maximum = Some(MAX_READ_BYTES as f64);
    properties.insert("limit".into(), limit);
    let schema = ToolInputSchema {
        schema_type: SchemaType,
        properties,
        required: Some(vec!["capability".into()]),
        additional_properties: Some(false),
        defs: None,
    };
    let handler: SharedToolHandler = Arc::new(|input, ctx| {
        async move {
            let owner = owner_from_metadata(ctx.metadata.as_ref())
                .ok_or_else(|| ToolError::other("artifact_read requires a stable session owner"))?;
            let capability = input
                .get("capability")
                .and_then(|value| value.as_str())
                .ok_or_else(|| ToolError::other("capability is required"))?;
            let id = capability_id(capability)
                .ok_or_else(|| ToolError::other("invalid artifact capability"))?;
            let offset = input
                .get("offset")
                .and_then(|value| value.as_u64())
                .unwrap_or(0) as usize;
            let limit = input
                .get("limit")
                .and_then(|value| value.as_u64())
                .unwrap_or(DEFAULT_READ_BYTES as u64)
                .clamp(1, MAX_READ_BYTES as u64) as usize;
            let path = artifact_path(&owner, id);
            let bytes = std::fs::read(&path)
                .map_err(|_| ToolError::other("artifact not found or not owned by this runner"))?;
            let text = String::from_utf8(bytes)
                .map_err(|_| ToolError::other("artifact is not valid UTF-8"))?;
            let start = align_start(&text, offset);
            let end = align_end(&text, start.saturating_add(limit));
            let next = (end < text.len()).then_some(end);
            Ok(json!({
                "capability": capability,
                "content": &text[start..end],
                "offset": start,
                "nextOffset": next,
                "totalBytes": text.len(),
                "truncated": next.is_some()
            }))
        }
        .boxed()
    });
    Tool::builder(
        "artifact_read",
        "分页读取当前会话或 workflow 分派中被自动外置的大工具结果。只接受工具返回的 artifact:// capability；单次最多 32768 字节。",
        schema,
        handler,
    )
    .concurrency_safe(true)
    .build()
}
