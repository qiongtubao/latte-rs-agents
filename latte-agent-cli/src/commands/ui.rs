//! `latte-agent ui` — Web UI bridge 命令（薄壳）。
//!
//! server 实现已全部抽到 `latte-agent-ui-server` crate，供 CLI 与
//! Tauri 应用（latte-code-editor，docs/ui-embedding-design.md §6
//! 阶段 0）内嵌共用。本文件只负责：clap 参数 → 加载配置 →
//! `latte_agent_ui_server::spawn`。
//!
//! `--dev` 的 vite dev server spawn 留在 CLI：它是纯开发便利
//! （npm 子进程 + [vite] 日志转发），编辑器内嵌用不到；而且
//! `env!("CARGO_MANIFEST_DIR")` 只有在 CLI crate 里才指向
//! `latte-agent-cli/`，候选路径才不会漂。
//!
//! 架构:
//!   - 前端 (latte-agent-cli/ui/, Vite + React): ChatPanel / TracePanel / SelfLoop
//!   - 后端 (latte-agent-ui-server): axum @ :4567，代理 ChatController
//!   - SelfLoop (latte-agent-cli/ui/self-loop/): Playwright + screencap 闭环

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Args;
use latte_agent_core::model_resolver::ModelTier;
use latte_agent_ui_server::UiServerConfig;

/// `latte-agent ui` — 启动 Web UI server。
///
/// 行为：
///   - 加载 agents.toml / models.toml（同 `chat` 命令）。
///   - 起 axum HTTP server @ `--port`（默认 4567）。
///   - 生产模式：把 `latte-agent-cli/ui/dist/` 当静态文件根。
///   - 开发模式（`--dev`）：spawn `npm run dev` 起 vite @ 5173，
///     并在 stdout 打印两个 URL。
#[derive(Args, Debug)]
pub struct UiCmd {
    /// HTTP server 监听端口。开发模式下也是 vite 代理目标端口。
    #[arg(long, default_value_t = 4567)]
    pub port: u16,

    /// 自动打开浏览器。
    #[arg(long, default_value_t = true)]
    pub open: bool,

    /// 角色 id（单角色模式），同 `chat --role`。
    #[arg(short, long)]
    pub role: Option<String>,

    /// 初始模型 tier，同 `chat --tier`。
    #[arg(short = 't', long)]
    pub tier: Option<String>,

    /// 同 `chat --model-id`。
    #[arg(short = 'm', long)]
    pub model_id: Option<String>,

    /// Path to agents config (file or directory).
    #[arg(long, default_value = ".latte/agents.d")]
    pub agents_config: String,

    /// Path to models config.
    #[arg(long, default_value = ".latte/models.d")]
    pub models_config: String,

    /// 开发模式：自动 spawn vite dev server。
    #[arg(long, default_value_t = false)]
    pub dev: bool,

    /// Vite dev server 端口（仅 `--dev` 模式下生效）。
    #[arg(long, default_value_t = 5173)]
    pub vite_port: u16,

    /// 静态前端目录。默认 `latte-agent-cli/ui/dist`（生产模式）。
    #[arg(long)]
    pub static_dir: Option<String>,

    /// agent 工作目录（工具调用、prompt_file 等相对路径的根目录）。
    /// 默认：进程当前目录。
    #[arg(long)]
    pub cwd: Option<String>,
}

impl UiCmd {
    pub async fn run(&self) -> Result<(), Box<dyn std::error::Error>> {
        // 1. 加载配置 —— 复用 commands/config_layer::load。
        let cli_overrides = super::config_layer::CliOverrides {
            api_key: None,
            api_key_target: None,
            field_overrides: vec![],
        };
        let resolved = super::config_layer::load(
            Some(&self.agents_config),
            Some(&self.models_config),
            cli_overrides,
        )
        .map_err(|e| format!("failed to load configuration: {}", e))?;

        // 2. 解析 --tier。未给时传 None，由 server 从初始角色的
        // model_tier 模板推导（语义同原 UiCmd::run）。
        let tier: Option<ModelTier> = match &self.tier {
            Some(t) => Some(parse_tier_str(t)?),
            None => None,
        };

        // 3. spawn vite dev server（仅 dev 模式）。
        if self.dev {
            spawn_vite_dev(self.vite_port, self.port).await?;
        }

        // 4. 起 server（库里完成 bind，端口 0 时 handle.addr 是真实端口）。
        // 解析 cwd：--cwd 参数优先，否则取进程当前目录
        let cwd = self.cwd.as_ref().map(|s| {
            let p = PathBuf::from(s);
            if p.is_absolute() { p } else { std::env::current_dir().unwrap().join(p) }
        });
        // 4. 起 server（库里完成 bind，端口 0 时 handle.addr 是真实端口）。
        let handle = latte_agent_ui_server::spawn(UiServerConfig {
            bind: SocketAddr::from(([0, 0, 0, 0], self.port)),
            static_dir: self.static_dir.as_ref().map(PathBuf::from),
            agent_config: resolved.config,
            model_resolver: resolved.resolver,
            role: self.role.clone(),
            tier,
            model_id: self.model_id.clone(),
            cwd,
            agents_config: self.agents_config.clone(),
        })
        .await?;
        eprintln!(
            "[ui] latte-agent UI server listening on http://localhost:{}",
            handle.addr.port()
        );
        if self.dev {
            eprintln!(
                "[ui] dev mode: vite UI at http://localhost:{} (proxied to backend at :{})",
                self.vite_port,
                handle.addr.port()
            );
        } else if handle.static_dir.is_some() {
            eprintln!("[ui] static UI at http://localhost:{}/", handle.addr.port());
        } else {
            eprintln!(
                "[ui] no static UI built — only API routes are live. \
                 run `pnpm --dir latte-agent-cli/ui install && pnpm --dir latte-agent-cli/ui build` for the production bundle, \
                 or restart with `--dev` to spawn vite."
            );
        }

        // 5. 一直跑到进程结束。
        handle.wait().await;
        Ok(())
    }
}

fn parse_tier_str(s: &str) -> Result<ModelTier, Box<dyn std::error::Error>> {
    ModelTier::parse(s).map_err(|e| e.into())
}

async fn spawn_vite_dev(
    vite_port: u16,
    backend_port: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidates = [
        manifest_dir.join("ui"),
        PathBuf::from("latte-agent-cli/ui"),
    ];
    let ui_dir = candidates.into_iter().find(|p| p.exists());
    let ui_dir = match ui_dir {
        Some(d) => d,
        None => {
            eprintln!(
                "[ui] vite dir not found; tried {} and latte-agent-cli/ui.                  run from the workspace root or build ui/dist for production mode.",
                manifest_dir.join("ui").display()
            );
            return Ok(());
        }
    };
    let backend = format!("http://localhost:{}", backend_port);
    let mut cmd = tokio::process::Command::new("npm");
    cmd.arg("run")
        .arg("dev")
        .env("VITE_PORT", vite_port.to_string())
        .env("VITE_BACKEND", backend)
        .current_dir(&ui_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn()?;
    // Forward vite stdout/stderr to stderr tagged with [vite].
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[vite] {}", line);
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[vite] {}", line);
            }
        });
    }
    // Don't await — vite is a long-running server. Detach.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}

// ─── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tier_str_works() {
        assert!(matches!(parse_tier_str("premium").unwrap(), ModelTier::Premium));
        assert!(matches!(parse_tier_str("standard").unwrap(), ModelTier::Standard));
        assert!(matches!(parse_tier_str("budget").unwrap(), ModelTier::Budget));
        assert!(parse_tier_str("nope").is_err());
    }
}
