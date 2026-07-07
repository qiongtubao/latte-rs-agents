//! Build script: 编译 git submodule 里的 rtk binary 到 OUT_DIR/rtk.
//!
//! 流程:
//! 1. 检测 `vendor/rtk/Cargo.toml` (submodule 是否 init).
//! 2. 检测 `OUT_DIR/rtk` 是否已存在, 存在就跳过 (增量 build).
//! 3. 跑 `cd vendor/rtk && cargo build --release`, 让 rtk 自己的
//!    `Cargo.toml` 当作 workspace 根 (避免上层 `[workspace]` 干扰).
//!    rtk 自己 target 隔离在 `vendor/rtk/target/`, 跟 workspace 不冲突.
//! 4. 复制 `vendor/rtk/target/release/rtk` 到 `OUT_DIR/rtk`.
//! 5. runtime `latte-agent-cli/src/main.rs::init_rtk_path` 用
//!    `env!("OUT_DIR")` 拿到路径, prepend 到 PATH, 这样
//!    `bash {"command": "rtk <subcmd>"}` 直接走 rtk, 不需要全局装.
//!
//! Opt-out: 设 `LATTE_AGENT_SKIP_RTK_BUILD=1` 跳过自动编译.
//!   此时 rtk binary 不存在, rtk_assistant role 会走 prompt 里写好的
//!   fallback 路径 (一行 install 提示 + 原生 bash).
//!
//! 为什么不直接 cargo dependency: rtk 是独立 binary crate, 它的 deps
//! 跟 workspace 不一定兼容 (不同版本号会冲突). 隔离编译更稳.
//! 为什么不直接 include_str! 把 rtk 源码塞进来: rtk 编译产物是 binary,
//! 不是 library, 而且 rtk 有自己的 runtime 资源, 隔离更干净.
//!
//! 为什么不直接 `--manifest-path`: rtk 是独立 crate, 但它的 `Cargo.toml`
//! 没有任何 `[workspace]` 标记. 当我们从 latte-rs-agents 根目录用
//! `--manifest-path vendor/rtk/Cargo.toml` 调 cargo, cargo 会从当前
//! 工作目录往上找 `[workspace]`, 找到 latte-rs-agents 的根 workspace,
//! 然后把 rtk 当成那个 workspace 的隐式 member — rtk 一不是
//! `members` 二不是 `exclude`, cargo 报错 "current package believes
//! it's in a workspace when it's not". 所以必须 `cd vendor/rtk`
//! 再 `cargo build`, 让 rtk 自己的目录作为 cargo 的工作目录.

use std::path::Path;
use std::process::Command;

fn main() {
    // Re-run 触发条件: build.rs 自身 + rtk 源码变动.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../vendor/rtk/Cargo.toml");
    println!("cargo:rerun-if-changed=../vendor/rtk/build.rs");
    println!("cargo:rerun-if-changed=../vendor/rtk/src");

    // Opt-out: 让用户/CI 能跳过 (例如 rtk 已全局装, 不需要再编一份).
    if std::env::var("LATTE_AGENT_SKIP_RTK_BUILD").is_ok() {
        return;
    }

    let rtk_dir = Path::new("../vendor/rtk");
    let manifest = rtk_dir.join("Cargo.toml");
    if !manifest.exists() {
        // 没 init submodule. 不 panic, 让 build 继续 (cli 自身没 rtk 也能编),
        // rtk_assistant 角色会走 fallback 路径. 给一个 warning 让用户看到.
        println!(
            "cargo:warning=rtk submodule not initialized; \
             `git submodule update --init vendor/rtk` if you want auto-built rtk. \
             rtk_assistant role will fall back to native bash for now."
        );
        return;
    }

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR set by cargo");
    let rtk_bin_name = if cfg!(windows) { "rtk.exe" } else { "rtk" };
    let rtk_bin = Path::new(&out_dir).join(rtk_bin_name);

    // 增量 build: 上次 build 已经把 rtk 复制到 OUT_DIR, 跳过.
    if rtk_bin.exists() {
        return;
    }

    eprintln!("[build.rs] compiling rtk (first build may take a few minutes)...");
    // `cd vendor/rtk` 让 rtk 自己的目录是 cargo 的 working dir — 这样
    // rtk 的 `Cargo.toml` 就是 workspace 根, 不会往上找到 latte-rs-agents
    // 的 workspace. (用 `--manifest-path` 不会改 working dir, cargo 仍
    // 会从 cwd 往上找 [workspace], 然后报 "current package believes
    // it's in a workspace when it's not".)
    let status = Command::new("cargo")
        .current_dir(rtk_dir)
        .args(["build", "--release"])
        .status()
        .expect("failed to spawn `cargo build` for rtk submodule");

    if !status.success() {
        panic!(
            "rtk submodule build failed (see output above). \
             To skip: set LATTE_AGENT_SKIP_RTK_BUILD=1"
        );
    }

    // rtk 自己 target 隔离在 vendor/rtk/target/release, 跟 workspace target
    // 不冲突. 复制到 OUT_DIR 是因为 OUT_DIR 是 cargo 给 build.rs 唯一
    // guaranteed-writable 路径, runtime 用 `env!("OUT_DIR")` 能拿到.
    let src = rtk_dir.join("target/release").join(rtk_bin_name);
    std::fs::copy(&src, &rtk_bin).unwrap_or_else(|e| {
        panic!(
            "failed to copy {} to OUT_DIR ({}): {}",
            src.display(),
            rtk_bin.display(),
            e
        )
    });
    eprintln!("[build.rs] rtk binary ready at {}", rtk_bin.display());
}
