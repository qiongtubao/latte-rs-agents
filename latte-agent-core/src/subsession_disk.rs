//! Per-subsession disk sink: best-effort persistence of `TraceEvent`
//! lines to `<ui-sessions>/<session_id>/<sub_id>.jsonl` for offline
//! debugging after the in-memory cache expires or the server restarts.
//!
//! 文件结构：
//!   - 首行 meta（与 `ui-sessions/*.jsonl` 同构）：`{"type":"__meta__",
//!     "sub_id","session_id","role_name","created_at_unix_ms"}`
//!   - 之后每行一个 `TraceEvent` 的 serde_json 序列化（与
//!     `trace::JsonlSink` 同形）。
//!
//! 失败语义：所有 IO 错误都吞掉（不进 panic、不进 agent 主循环）。
//! `try_new` 把"目录不存在 / 文件打不开"提前到构造期；构造失败 →
//! 调用方回退到纯内存（不打日志也行——`SubsessionStore` 自己 warn）。
//! `emit` 期间若写盘失败，整条事件就丢了，但内存副本不受影响，UI
//! 仍能看到（只是这次会话来过的事重启后查不到了）。
//!
//! 写策略：直接 `writeln!` 到 `Mutex<File>`，每条事件一次 write。
//! 不用 `BufWriter`——subsession 事件频率很低（每 tool call 一次，
//! 不是 per-token），用户态缓冲不仅无收益还会让 test 看不到数据。
//! 内核 page cache 已经在做缓冲，足够。
//!
//! Path safety：`session_id` 在 `try_new` 拒绝含 `/` `\` `..` 的值，
//! 防止把 subagent 文件写到 `base_dir` 之外。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;

use crate::trace::{TraceEvent, TraceSink};

/// Meta 行的 type 判别字段。与 `ui-sessions/*.jsonl` 的 `META_TYPE`
/// 同值，让 `restore_sessions` 那套解析器将来扩展到 subsessions 时能
/// 直接复用。
pub const META_TYPE: &str = "__meta__";

/// Per-subsession disk sink。
///
/// 写流程：构造时打开文件（`create + append`）并写一行 meta；之后
/// 每次 `emit` 直接 `writeln!` 一行 `TraceEvent` JSON。`Drop` 关闭
/// 文件 fd（Rust 析构 File 自动 close，无需手动）。
pub struct SubsessionDiskSink {
    writer: Mutex<File>,
    /// 完整路径，调试 / 测试断言用。
    #[allow(dead_code)]
    pub path: PathBuf,
}

impl SubsessionDiskSink {
    /// 打开 `<base>/<session_id>/<sub_id>.jsonl` 并写 meta 行。
    ///
    /// 失败原因（全部以 `String` 返回便于上层 warn）：
    ///   - base 或 session_id 目录创建失败（权限 / 路径不存在 / ENOSPC）
    ///   - 文件打开失败（同上）
    ///   - meta 行写盘失败（同上，且不应回退——文件可能半写）
    pub fn try_new(
        base_dir: &Path,
        session_id: &str,
        sub_id: &str,
        role_name: &str,
    ) -> Result<Self, String> {
        // session_id 可能包含路径分隔符？不应当：chat session id 是
        // UI server 生成的 uuid-like 串。但保险起见做一次 sanitize——
        // 任何 `/` `\` 或 `..` 都拒绝（不让外部写盘路径逃出 base_dir）。
        if session_id.is_empty()
            || session_id.contains('/')
            || session_id.contains('\\')
            || session_id.contains("..")
        {
            return Err(format!(
                "subsession: refusing unsafe session_id {:?} for disk path",
                session_id
            ));
        }

        let session_dir = base_dir.join(session_id);
        std::fs::create_dir_all(&session_dir)
            .map_err(|e| format!("mkdir {}: {e}", session_dir.display()))?;

        let path = session_dir.join(format!("{sub_id}.jsonl"));
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("open {}: {e}", path.display()))?;

        let meta = serde_json::json!({
            "type": META_TYPE,
            "sub_id": sub_id,
            "session_id": session_id,
            "role_name": role_name,
            "created_at_unix_ms": unix_ms_now(),
        });
        writeln!(file, "{}", meta).map_err(|e| format!("write meta to {}: {e}", path.display()))?;
        // 立即 flush meta：构造函数返回后 caller 立即把 sink 交给
        // runner，runner 后续 emit 才是事件流。meta 必须先到。
        // （即使没显式 flush，`File::write_all` 在 macOS 上也会触发
        // fsync 行为；在 Linux 上数据进 page cache 但仍可读——但为
        // 了一致性还是显式 flush 一下，meta 行只是 1 次额外 syscall。）
        std::io::Write::flush(&mut file)
            .map_err(|e| format!("flush meta to {}: {e}", path.display()))?;

        Ok(Self {
            writer: Mutex::new(file),
            path,
        })
    }
}

impl TraceSink for SubsessionDiskSink {
    fn emit(&self, event: TraceEvent) {
        let json = match serde_json::to_string(&event) {
            Ok(s) => s,
            Err(_) => return, // 序列化失败：跳过，不影响内存 sink
        };
        let mut g = self.writer.lock();
        // 写失败就吞——单条事件丢不阻塞 agent 主循环；下次 sweep
        // 时 subsession_store 仍可继续工作。
        let _ = writeln!(g, "{json}");
    }
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
