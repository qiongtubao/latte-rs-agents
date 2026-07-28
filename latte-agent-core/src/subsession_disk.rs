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
//! **Lazy create**：构造时不创建文件，第一次 `emit` 时才创建目录 +
//! 文件 + 写 meta 行。这样 `/role switch` 重建 runner 但没跑 turn 时
//! 不会产生空文件（只有 meta 行没有事件）。
//!
//! 失败语义：所有 IO 错误都吞掉（不进 panic、不进 agent 主循环）。
//! `try_new` 只做参数校验（path safety），不做 IO。`emit` 期间若写盘
//! 失败，整条事件就丢了，但内存副本不受影响。
//!
//! 写策略：直接 `writeln!` 到 `Mutex<File>`，每条事件一次 write。
//! 不用 `BufWriter`--subsession 事件频率低（每 tool call 一次，
//! 不是 per-token），用户态缓冲不仅无收益还会让 test 看不到数据。
//! 内核 page cache 已经在做缓冲，足够。

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

/// Per-subsession disk sink（lazy create）。
///
/// 构造时只存参数（base_dir / session_id / sub_id / role_name），
/// **不创建文件**。第一次 `emit` 时才：
///   1. `create_dir_all(<base>/<session_id>)`
///   2. `OpenOptions::create+append` 打开 `<base>/<sid>/<sub_id>.jsonl`
///   3. 写 meta 行
///   4. 写事件行
///
/// 之后每次 `emit` 只写事件行。`Drop` 时关闭 fd。
pub struct SubsessionDiskSink {
    /// 第一次 emit 时拿锁 + 检查 None -> 创建文件 + 写 meta。
    /// 之后拿锁 + 检查 Some -> 直接写事件。
    state: Mutex<LazyState>,
    base_dir: PathBuf,
    session_id: String,
    sub_id: String,
    role_name: String,
    /// 完整路径，第一次 emit 后填充。debug / 测试断言用。
    #[allow(dead_code)]
    pub path: PathBuf,
}

enum LazyState {
    /// 还没写过任何事件，文件还没创建。
    Uninitialized,
    /// 文件已打开，直接写。
    Open(File),
    /// 文件创建/打开失败，永久放弃（不再重试，避免每次 emit 都打日志）。
    /// 内存 sink 仍能工作，只是这个 subsession 不落盘。
    Failed,
}

impl SubsessionDiskSink {
    /// 参数校验（path safety），不做 IO。实际文件创建在第一次 `emit`。
    pub fn try_new(
        base_dir: &Path,
        session_id: &str,
        sub_id: &str,
        role_name: &str,
    ) -> Result<Self, String> {
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

        let path = base_dir.join(session_id).join(format!("{sub_id}.jsonl"));

        Ok(Self {
            state: Mutex::new(LazyState::Uninitialized),
            base_dir: base_dir.to_path_buf(),
            session_id: session_id.to_string(),
            sub_id: sub_id.to_string(),
            role_name: role_name.to_string(),
            path,
        })
    }

    /// 第一次 emit 时调用：创建目录 + 打开文件 + 写 meta 行。
    /// 成功 -> 返回打开的 File。失败 -> 返回 None（状态设为 Failed）。
    fn initialize(&self) -> Option<File> {
        let session_dir = self.base_dir.join(&self.session_id);
        if let Err(e) = std::fs::create_dir_all(&session_dir) {
            eprintln!(
                "[subsession] mkdir {}: {e}; disk disabled for {}",
                session_dir.display(),
                self.sub_id
            );
            return None;
        }

        let path = session_dir.join(format!("{}.jsonl", self.sub_id));
        let mut file = match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => {
                eprintln!(
                    "[subsession] open {}: {e}; disk disabled for {}",
                    path.display(),
                    self.sub_id
                );
                return None;
            }
        };

        let meta = serde_json::json!({
            "type": META_TYPE,
            "sub_id": self.sub_id,
            "session_id": self.session_id,
            "role_name": self.role_name,
            "created_at_unix_ms": unix_ms_now(),
        });
        if let Err(e) = writeln!(file, "{}", meta) {
            eprintln!("[subsession] write meta to {}: {e}", path.display());
            return None;
        }

        Some(file)
    }
}

impl TraceSink for SubsessionDiskSink {
    fn emit(&self, event: TraceEvent) {
        let json = match serde_json::to_string(&event) {
            Ok(s) => s,
            Err(_) => return,
        };

        let mut state = self.state.lock();
        match &mut *state {
            LazyState::Uninitialized => {
                match self.initialize() {
                    Some(mut file) => {
                        let _ = writeln!(file, "{json}");
                        *state = LazyState::Open(file);
                    }
                    None => {
                        *state = LazyState::Failed;
                    }
                }
            }
            LazyState::Open(file) => {
                let _ = writeln!(file, "{json}");
            }
            LazyState::Failed => {}
        }
    }
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
