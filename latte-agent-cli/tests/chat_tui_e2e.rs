//! `latte-agent chat` 分离模式（split screen）的交互 e2e。
//!
//! # 为什么单独一套 pty 测试
//!
//! 分离模式的 bug 有一整类是**单元测试与非 TTY e2e 都测不到**的——它们
//! 只在真 TTY + raw mode 下出现。本轮实测抓到过三个：
//!
//! 1. `\x1b[0A`：ECMA-48 规定 CUU 参数为 0 时按 1 处理，于是 `MoveUp(0)`
//!    真的上移一行、吃掉一行历史；
//! 2. `SplitScreen` 存在 `static` 里 → Rust 的 static 永不 drop → 恢复
//!    终端的 `leave()` 从未执行，退出后用户终端留在 raw mode（无回显）；
//! 3. 终端上报 0 列时把宽度钳到 1，输出被折成"每字符一行"。
//!
//! 三个都是靠 pty 实机验证发现的，所以这套测试是渲染层的回归网。
//!
//! # 跳过条件
//!
//! 需要 `python3`（用它的 `pty` 模块开伪终端）。缺少时跳过而非失败——
//! CI 环境不一定有，但本地开发必须能跑。

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = <root>/latte-agent-cli
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn binary() -> PathBuf {
    // 与 `cargo test` 同一 profile 的产物目录。
    let mut p = repo_root().join("target");
    p.push(if cfg!(debug_assertions) { "debug" } else { "release" });
    p.push("latte-agent");
    p
}

fn python3_available() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 建一个最小工作目录：有 `.latte/` 但没有仓库内容，避免测试去读真实代码。
fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("latte-tui-e2e-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".latte")).expect("scratch dir");
    std::fs::write(dir.join("seed.txt"), "seed\n").expect("seed file");
    dir
}

/// 跑 pty 驱动，返回 pty 上收到的全部输出（含转义序列）。
fn drive(cwd: &Path, steps: &[(&str, &str)]) -> String {
    let script = repo_root().join("scripts/pty_drive.py");
    let mut cmd = Command::new("python3");
    cmd.arg(&script)
        .arg("--binary")
        .arg(binary())
        .arg("--cwd")
        .arg(cwd)
        .arg("--timeout")
        .arg("90");
    for (wait, text) in steps {
        cmd.arg("--step").arg(format!("{wait}:{text}"));
    }
    let out = cmd.output().expect("run pty_drive.py");
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// 分离模式必须真的启用，且**退出时把终端恢复干净**。
///
/// 判据用 bracketed paste 的开关配对（`?2004h` / `?2004l`）：它是
/// "进入/离开分离模式"最容易观察的指纹。少了 `?2004l` 说明恢复链没跑，
/// 用户的终端会留在 raw mode——这正是 static 不 drop 那个 bug 的症状。
#[test]
fn split_screen_enters_and_restores_the_terminal() {
    if !python3_available() {
        eprintln!("[skip] python3 不可用，跳过 pty 交互测试");
        return;
    }
    let cwd = scratch_dir("restore");
    let out = drive(&cwd, &[("4.0", "/quit")]);

    let on = out.matches("\u{1b}[?2004h").count();
    let off = out.matches("\u{1b}[?2004l").count();
    assert!(on >= 1, "应进入分离模式（开启 bracketed paste）：\n{}", tail(&out));
    assert_eq!(
        on, off,
        "bracketed paste 的开/关必须配对，否则终端留在 raw mode（static 永不 drop 那个 bug）"
    );
}

/// `MoveUp(0)` 绝不能下发：ECMA-48 规定 CUU 参数为 0 时按 1 处理，
/// 会真的上移一行、把上一行历史吃掉。
#[test]
fn never_emits_cursor_up_zero() {
    if !python3_available() {
        eprintln!("[skip] python3 不可用，跳过 pty 交互测试");
        return;
    }
    let cwd = scratch_dir("cuu0");
    let out = drive(&cwd, &[("4.0", "/roles"), ("2.0", "/quit")]);
    assert!(
        !out.contains("\u{1b}[0A"),
        "输出里出现了 \\x1b[0A，会吃掉一行历史：\n{}",
        tail(&out)
    );
}

/// 斜杠命令的输出必须真的显示出来（走历史区），而不是被 viewport 覆盖
/// 或因宽度退化被打碎成每字符一行。
#[test]
fn slash_command_output_reaches_the_history_area() {
    if !python3_available() {
        eprintln!("[skip] python3 不可用，跳过 pty 交互测试");
        return;
    }
    let cwd = scratch_dir("cmds");
    let out = drive(&cwd, &[("4.0", "/roles"), ("2.5", "/tools"), ("2.5", "/quit")]);
    assert!(
        out.contains("Available roles ("),
        "/roles 的输出应出现在历史区：\n{}",
        tail(&out)
    );
    assert!(
        out.contains("tools ("),
        "/tools 的输出应出现在历史区：\n{}",
        tail(&out)
    );
}

/// 宽度退化回归：终端上报 0 列时若把宽度钳到 1，输出会被折成
/// "每字符一行"。这里给一个正常 winsize，断言输出**没有**被打碎。
///
/// 判据：连续的单字符行不应该成片出现。打碎时 `\x1b[36m` 这种序列会
/// 变成 6 个单字符行，非常显眼。
#[test]
fn output_is_not_shredded_into_one_char_per_line() {
    if !python3_available() {
        eprintln!("[skip] python3 不可用，跳过 pty 交互测试");
        return;
    }
    let cwd = scratch_dir("width");
    let out = drive(&cwd, &[("4.0", "/roles"), ("2.5", "/quit")]);
    let mut run = 0usize;
    let mut worst = 0usize;
    for line in out.split("\r\n") {
        // 只数"恰好一个可见字符"的行；空行是正常的分隔。
        if line.chars().count() == 1 {
            run += 1;
            worst = worst.max(run);
        } else {
            run = 0;
        }
    }
    assert!(
        worst < 5,
        "出现了 {worst} 连续单字符行，输出被打碎（终端宽度退化到 1）：\n{}",
        tail(&out)
    );
}

/// 分离模式下**不能**有独立 spinner。
///
/// `style::Spinner` 往 stderr 写 `\r\x1b[K`：把光标拉回行首并清行，正好
/// 擦掉底部的输入行。原代码注释写着"用 `\r` 所以不会打架"——那只对旧的
/// 交错式输出成立，对固定 viewport 是直接冲突。实测抓到 56 次。
#[test]
fn no_bare_spinner_escapes_in_split_mode() {
    if !python3_available() {
        eprintln!("[skip] python3 不可用，跳过 pty 交互测试");
        return;
    }
    let cwd = scratch_dir("spinner");
    // 触发一次真实模型调用（生成期间才会有 spinner）。
    let out = drive(
        &cwd,
        &[("4.0", "用一句话说你好"), ("25.0", "/quit")],
    );
    assert!(
        !out.contains("\r\u{1b}[K"),
        "分离模式下出现了 spinner 的 `\\r\\x1b[K`，会擦掉输入行：\n{}",
        tail(&out)
    );
}

/// 核心：**turn 进行中输入行必须是活的**。
///
/// 这是输入/显示分离的主要目的，也是与 UI 的流程差异所在——UI 的发送框
/// 始终可用。改造前 `read_user_line` 只在两个 turn 之间被调用，turn 期间
/// 没有任何东西读 stdin：实测 turn 中打的字在日志里的位置**晚于**
/// `TurnEnd`，即回显发生在 turn 结束之后。
#[test]
fn input_line_is_live_during_a_turn() {
    if !python3_available() {
        eprintln!("[skip] python3 不可用，跳过 pty 交互测试");
        return;
    }
    let cwd = scratch_dir("live-input");
    let marker = "ZZMARKERZZ";
    let out = drive(
        &cwd,
        &[
            ("4.0", "用两百字讲讲内存分配器"),
            // turn 还在跑时打字（`~` 前缀 = 不补回车，只打字不提交），
            // 应当立刻回显。
            ("3.0", "~ZZMARKERZZ"),
            ("40.0", "/quit"),
        ],
    );
    let m = out.find(marker);
    let t = out.find("TurnEnd");
    let (Some(m), Some(t)) = (m, t) else {
        panic!("日志里应同时出现 marker 与 TurnEnd：\n{}", tail(&out));
    };
    assert!(
        m < t,
        "turn 期间打的字必须立刻回显（marker@{m} 应早于 TurnEnd@{t}）：\n{}",
        tail(&out)
    );
}

/// Ctrl-C 在 turn **进行中**必须中止本轮，且**不能终结会话**。
///
/// 两处曾经的问题：
/// 1. 原来 Ctrl-C 只清输入行，turn 继续跑——用户在一个跑了十几分钟的
///    turn 里按 Ctrl-C，期望的是"停下来"；
/// 2. 加了中止之后，中止走 `Err` 路径被当成 turn 失败：触发 auto-save、
///    打印 `turn failed`，且 REPL 主循环拿到 `Err` 会**退出整个会话**。
///    实测复现，靠哨兵错误串区分。
#[test]
fn ctrl_c_cancels_the_turn_without_killing_the_session() {
    if !python3_available() {
        eprintln!("[skip] python3 不可用，跳过 pty 交互测试");
        return;
    }
    let cwd = scratch_dir("ctrlc");
    let out = drive(
        &cwd,
        &[
            ("4.0", "用一千字详细讲讲内存分配器的完整历史"),
            // turn 跑起来后按 Ctrl-C（`~` = 不补回车，直接发控制字符）。
            ("6.0", "~\u{3}"),
            // 中止后会话必须还活着：再发一轮短问题应能正常完成。
            ("5.0", "用一句话说你好"),
            ("30.0", "/quit"),
        ],
    );
    assert!(
        out.contains("本轮已中止"),
        "Ctrl-C 应中止当前 turn：\n{}",
        tail(&out)
    );
    assert!(
        !out.contains("auto-save"),
        "用户主动中止不该被当成 turn 失败而 auto-save：\n{}",
        tail(&out)
    );
    assert!(
        !out.contains("turn failed"),
        "用户主动中止不该打印 `turn failed`：\n{}",
        tail(&out)
    );
    // 会话存活的证据：中止之后仍完成了一次 turn。
    assert!(
        out.contains("TurnEnd"),
        "中止后应还能正常跑完下一轮（会话未退出）：\n{}",
        tail(&out)
    );
}

/// 多行命令输出必须**攒成一段**再发，不能逐行发。
///
/// 分离模式下每次输出都要清+重画整个 viewport，逐行发 51 个角色就是
/// 51 次重绘。实测：批量化前 8 次 `/roles` 制造 936 个重绘段 / 67KB，
/// 批量化后 108 段 / 31KB。不是正确性问题，但在慢终端上会可见闪烁。
#[test]
fn multi_line_command_output_is_batched() {
    if !python3_available() {
        eprintln!("[skip] python3 不可用，跳过 pty 交互测试");
        return;
    }
    let cwd = scratch_dir("batch");
    // 连发 4 次 /roles（每次 50+ 行）。
    let out = drive(
        &cwd,
        &[("4.0", "/roles"), ("1.2", "/roles"), ("1.2", "/roles"), ("1.2", "/roles"), ("1.5", "/quit")],
    );
    let listings = out.matches("Available roles (").count();
    assert!(listings >= 3, "至少应有 3 次 /roles 输出，实际 {listings}");
    // `\x1b[J`（清到屏幕底）是每次 viewport 重绘的指纹。
    let redraws = out.matches("\u{1b}[J").count();
    // 逐行发时每个角色一次重绘（4×51≈200+，实测 936）；攒批后应远低于此。
    assert!(
        redraws < 60 * listings,
        "重绘次数 {redraws} 相对 {listings} 次列表输出过多，多行输出可能又变成逐行发了"
    );
}

fn tail(s: &str) -> String {
    let n = s.chars().count();
    s.chars().skip(n.saturating_sub(1200)).collect()
}
