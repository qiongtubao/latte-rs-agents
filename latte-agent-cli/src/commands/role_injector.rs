//! Per-role inject queue. Files live at
//! `<worktree>/.latte/inject/<role-id>.txt`. They are append-only
//! until the next turn of that role, at which point the entire file
//! is read, its content prepended to the role's `ConversationContext`
//! as a synthetic `Role::User` message, and the file is deleted.
//!
//! In v1 the queue files are not crash-safe (a crash between read
//! and delete will re-inject on the next turn). v1.1 will rename
//! to `.processing` for atomicity.

use std::io::Write;
use std::path::{Path, PathBuf};

pub struct RoleInjector;

impl RoleInjector {
    /// Append `message` to the queue for `role_id`. The file is
    /// created if it does not exist.
    pub fn queue_for(worktree_root: &Path, role_id: &str, message: &str) -> std::io::Result<()> {
        let path = Self::queue_path(worktree_root, role_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        writeln!(f, "{}", message)?;
        Ok(())
    }

    /// Read and drain the queue for `role_id`. Returns the
    /// accumulated content (possibly empty) as a `String` suitable
    /// for prepending to the role's `ConversationContext` as a
    /// synthetic user message.
    /// 委托给 `latte_agent_core::inject_queue::drain` —— 该逻辑曾有四份
    /// 独立实现且行为不一致（详见那个模块的文档）。这里只保留薄封装，
    /// 让既有调用方与测试不必改签名。
    pub fn drain(worktree_root: &Path, role_id: &str) -> std::io::Result<Option<String>> {
        Ok(latte_agent_core::inject_queue::drain(worktree_root, role_id))
    }

    pub fn queue_path(worktree_root: &Path, role_id: &str) -> PathBuf {
        latte_agent_core::inject_queue::queue_path(worktree_root, role_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_then_drain_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        RoleInjector::queue_for(dir.path(), "programmer", "hello").unwrap();
        RoleInjector::queue_for(dir.path(), "programmer", "world").unwrap();
        let drained = RoleInjector::drain(dir.path(), "programmer").unwrap();
        assert_eq!(drained.as_deref(), Some("hello\nworld\n"));
        // File is gone after drain
        assert!(!RoleInjector::queue_path(dir.path(), "programmer").exists());
    }

    #[test]
    fn drain_empty_queue_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let drained = RoleInjector::drain(dir.path(), "ghost").unwrap();
        assert_eq!(drained, None);
    }

    #[test]
    fn drain_consumes_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = RoleInjector::queue_path(dir.path(), "x");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();
        let drained = RoleInjector::drain(dir.path(), "x").unwrap();
        assert_eq!(drained, None);
        assert!(!path.exists());
    }
}