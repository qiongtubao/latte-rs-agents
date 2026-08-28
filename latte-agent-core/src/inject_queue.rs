//! 每角色注入队列（`<worktree>/.latte/inject/<role-id>.txt`）的**唯一**读取实现。
//!
//! 队列文件由外部（CLI 的 `@role` 命令 / UI）追加写入，在该角色的下一个回合
//! 开头被整体读出、作为合成 `Role::User` 消息注入其历史，然后删除。
//!
//! ## 为什么单独抽一个模块
//!
//! 这段逻辑曾有**四份**独立实现：
//!   - `latte-agent-cli` 的 `RoleInjector::drain`（唯一抽成函数的那份，却只有
//!     自己的测试在用）；
//!   - `latte-agent-cli/src/commands/chat.rs` 的 HIL 循环内联一份；
//!   - `latte-agent-core/src/controller.rs` 的多角色循环内联一份；
//!   - `AgentRunner::drain_inject_queue`（从未接线）。
//!
//! 四份不一致：controller 那份检查了「内容全是空白」并跳过注入，chat.rs 那份
//! **没检查**——空队列文件会注入一条只有 `[INJECTED]` 头、没有正文的消息。
//! 统一到这里之后该差异消失。
//!
//! 放在 core 而不是 cli：`controller.rs` 在 core 里，core 不能依赖 cli。

use std::path::{Path, PathBuf};

/// 队列文件路径。
pub fn queue_path(worktree_root: &Path, role_id: &str) -> PathBuf {
    worktree_root
        .join(".latte")
        .join("inject")
        .join(format!("{role_id}.txt"))
}

/// 读出并清空指定角色的注入队列。
///
/// - 文件不存在 → `None`；
/// - 内容全是空白 → 删除文件并返回 `None`（不注入空消息）；
/// - 否则 → 删除文件并返回内容。
///
/// 读失败（权限等）时返回 `None` 且**不**删文件：宁可下一轮重试，也不静默丢内容。
///
/// 注意：读与删之间崩溃会导致下一轮重复注入（非原子）。这是已知取舍，沿用
/// 原实现语义。
pub fn drain(worktree_root: &Path, role_id: &str) -> Option<String> {
    let path = queue_path(worktree_root, role_id);
    if !path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(&path).ok()?;
    if content.trim().is_empty() {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let _ = std::fs::remove_file(&path);
    Some(content)
}

/// 注入消息的统一前缀标记。上层据此在历史里辨识注入内容。
pub const INJECT_MARKER: &str = "[INJECTED]";

/// 把 drain 出的内容包成注入消息正文。
pub fn format_injected(content: &str) -> String {
    format!("{INJECT_MARKER}\n{content}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_none() {
        let d = tempfile::tempdir().unwrap();
        assert!(drain(d.path(), "programmer").is_none());
    }

    #[test]
    fn whitespace_only_is_dropped_not_injected() {
        // 这正是 chat.rs 旧内联实现的 bug：空内容也会注入一条空 [INJECTED]。
        let d = tempfile::tempdir().unwrap();
        let p = queue_path(d.path(), "programmer");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "  \n\t\n").unwrap();
        assert!(drain(d.path(), "programmer").is_none(), "空白内容不该注入");
        assert!(!p.exists(), "空白队列仍应被清掉，否则每轮重试");
    }

    #[test]
    fn content_is_returned_and_file_removed() {
        let d = tempfile::tempdir().unwrap();
        let p = queue_path(d.path(), "programmer");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "look at foo.rs\n").unwrap();
        assert_eq!(drain(d.path(), "programmer").as_deref(), Some("look at foo.rs\n"));
        assert!(!p.exists());
        assert!(format_injected("x").starts_with(INJECT_MARKER));
    }
}
