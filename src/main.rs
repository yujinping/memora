//! Memora (忆庐) — 轻量自托管 AI 长期记忆服务
//!
//! @author yujinping
//! @intent P1 骨架：Axum + sea-orm 启动、项目路由、Bearer 中间件、_meta.db。
//!         P2 存储抽象：Repository 契约 + SQLite 文件后端 + 内存后端，
//!         业务层只依赖 trait，后端可由配置与项目登记值切换。
//!         P3 MCP 工具层：rmcp Streamable HTTP，9 个标准记忆工具可读写。
//!         P4 管理面：项目增 / 删 / 用量统计（`/api/v1`，ADMIN_TOKEN 保护）。
//!         P4.2 运行形态：命令行子命令（`run` / `start` / `stop` / `restart` /
//!         `status` / `init`）、配置来源显式化、优雅退出。
//!         `run` 是前台形态（供 systemd / 容器托管），其余子命令构成自守护形态，
//!         两种部署方式共用同一份二进制。
//!
//! @intent P4.3 配置体系整改：单实例 + XDG 绝对默认路径 + TOML。
//!         原先默认配置取「`CWD/.env` → 可执行文件目录/.env → `DATA_DIR/.env`」
//!         中首个命中者，而 `CWD` 既是相对路径又排第一，于是「你在跟哪个实例说话」
//!         取决于你在哪个目录敲命令：`status` 假报「未运行」，`stop` 打印
//!         「未运行」并返回 0 而进程照旧在跑。改为绝对默认路径后，
//!         任何目录下的 `memora status` 都指向同一实例。

mod admin;
mod auth;
mod cli;
mod config;
mod daemon;
mod domain;
mod entity;
mod error;
mod instance;
mod mcp;
mod meta;
mod reply;
mod routes;
mod state;
mod storage;

use std::net::SocketAddr;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Result;
use sea_orm::Database;

use cli::Command;
use config::{Config, DEFAULT_LOG};
use instance::InstanceFile;
use state::AppState;
use storage::StorageRegistry;

/// 进程入口：解析命令行并按子命令分发。
///
/// @intent 命令解析与配置加载**先于日志初始化**：日志级别可能写在配置文件里，
///         而配置文件是在 `Config::load*` 中读取的。顺序颠倒会让配置文件中的
///         `log` 永远不生效——P4.2 修正了这一处隐性缺陷。
fn main() -> ExitCode {
    // Rust 运行时会忽略 SIGPIPE，于是 `memora --help | head -1` 这类用法会在管道
    // 提前关闭时把 `println!` 的 EPIPE 变成 panic 并打印堆栈。恢复 SIGPIPE 的默认
    // 处置（终止进程）才能符合 Unix 工具惯例。
    //
    // SAFETY: 启动最早期、尚无线程与异步运行时，`signal(2)` 只影响本进程的信号处置。
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let parsed = match cli::parse(argv) {
        Ok(parsed) => parsed,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::from(2);
        }
    };

    match parsed.command {
        Command::Help => {
            print!("{}", cli::usage());
            ExitCode::SUCCESS
        }
        Command::Version => {
            println!("memora {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Command::Init { force } => {
            // `init` 的职责就是生成配置文件，故用容忍缺失的策略：
            // 否则 `memora init --config ~/.config/memora/config.toml`
            // 会因为那个文件尚不存在而永远无法创建它。
            match Config::load_for_init(&parsed.overrides, parsed.config_path.as_deref()) {
                Ok((config, source)) => {
                    init_tracing(&config.log);
                    source.log();
                    finish(daemon::init(&config, force))
                }
                Err(err) => {
                    eprintln!("error: {err:#}");
                    ExitCode::from(2)
                }
            }
        }
        command => {
            let (config, source) =
                match Config::load(&parsed.overrides, parsed.config_path.as_deref()) {
                    Ok(loaded) => loaded,
                    Err(err) => {
                        eprintln!("error: {err:#}");
                        return ExitCode::from(2);
                    }
                };
            init_tracing(&config.log);

            finish(match command {
                // 配置来源只对「启动类」命令有意义：`stop` / `status` 每次执行都打一遍
                // 只会变成噪声，而它们的输出里已经包含 PID 文件路径，足以定位目录。
                Command::Run => {
                    source.log();
                    serve(config).map(|()| 0)
                }
                Command::Start => {
                    source.log();
                    daemon::start(&config)
                }
                Command::Stop => daemon::stop(&config),
                Command::Restart => daemon::restart(&config),
                Command::Status => daemon::status(&config),
                // Help / Version / Init 已在上方分支处理
                Command::Help | Command::Version | Command::Init { .. } => unreachable!(),
            })
        }
    }
}

/// 初始化日志。
///
/// @intent 级别来自已解析的配置（`RUST_LOG` > 配置文件的 `log` > 默认值），
///         故「日志级别改在哪里」只有一个答案。默认 info：启动横幅与后端装配信息
///         是排查部署问题的第一手线索；同时把 sqlx / sea-orm 压到 warn，
///         避免每条 SQL 都刷日志。
fn init_tracing(log: &str) {
    let filter = tracing_subscriber::EnvFilter::try_new(log).unwrap_or_else(|err| {
        eprintln!("warning: invalid log filter {log:?} ({err}); falling back to {DEFAULT_LOG:?}");
        tracing_subscriber::EnvFilter::new(DEFAULT_LOG)
    });
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// 统一收尾：把子命令的返回值转成进程退出码，错误只打印不 panic。
fn finish(result: Result<i32>) -> ExitCode {
    match result {
        Ok(code) => ExitCode::from(code as u8),
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// 前台运行 `/mcp` 与 `/api/v1` 服务。
///
/// @intent 收到 SIGTERM / SIGINT 后走 axum 的优雅退出：停止接受新连接、等待在途请求
///         结束，再由 SQLite 正常关闭连接（WAL 落盘）。若直接 SIGKILL，WAL 虽可恢复
///         但会留下需要回放的日志，且不满足「可预期停机」的运维要求。
#[tokio::main]
async fn serve(config: Config) -> Result<()> {
    std::fs::create_dir_all(&config.data_dir)?;

    // 元库 _meta.db 存放项目注册信息（projects 表）
    let meta_path = Path::new(&config.data_dir).join("_meta.db");
    let url = format!("sqlite://{}?mode=rwc", meta_path.display());
    let db = Database::connect(&url).await?;
    meta::init_meta_db(&db).await?;

    // 存储后端注册表：本构建可用的后端 + 校验默认后端
    let storage = StorageRegistry::from_config(&config)?;
    let available: Vec<&str> = storage
        .available_kinds()
        .iter()
        .map(|k| k.as_str())
        .collect();
    tracing::info!(
        default_backend = %config.storage_backend,
        available = ?available,
        "storage backends ready"
    );

    let state = AppState {
        meta: db,
        config: config.clone(),
        storage: Arc::new(storage),
    };

    warn_about_risky_config(&config);

    let app = routes::create_app(state);
    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(
        pid = std::process::id(),
        address = %addr,
        data_dir = %config.data_dir,
        config_file = %config.config_path.display(),
        "Memora (忆庐) listening on http://{addr}"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // 自守护场景下实例记录由 `start` 写入；若确实是本进程的，退出时顺手清掉，
    // 避免留下需要靠「陈旧记录清理」兜底的文件。
    remove_own_instance_file(&config);
    tracing::info!("stopped");
    Ok(())
}

/// 启动时对两类「能启动但用不了 / 不安全」的配置给出显式告警。
///
/// @intent 这些配置问题的共同特征是**不会导致启动失败**，只会在首次使用时表现为
///         费解的 401 / 403。把它们前移到启动日志，是最省事的排障手段。
fn warn_about_risky_config(config: &Config) {
    if config.admin_token.is_empty() {
        tracing::warn!(
            config_file = %config.config_path.display(),
            "admin_token is not set: every /api/v1 request will be rejected with 401 \
             (project creation and statistics are unavailable). Set it in the config file \
             or via the ADMIN_TOKEN environment variable"
        );
    }

    // MCP 的 Host 白名单是最容易踩的部署坑：rmcp 默认只信回环 Host（防 DNS 重绑定），
    // 而 Caddy 反代会透传真实域名。
    if config.mcp_allowed_hosts.is_empty() {
        tracing::warn!(
            allowed_hosts = ?config.effective_mcp_allowed_hosts(),
            "mcp_allowed_hosts is not set: only loopback Hosts are accepted. \
             Public deployments behind a reverse proxy must add their domain, \
             otherwise MCP requests are rejected with 403"
        );
    } else {
        tracing::info!(
            allowed_hosts = ?config.effective_mcp_allowed_hosts(),
            "MCP host allowlist"
        );
    }
}

/// 若实例记录里的 pid 是本进程，则删除它。
fn remove_own_instance_file(config: &Config) {
    let instance_file = InstanceFile::new(Path::new(&config.data_dir));
    if let Ok(Some(record)) = instance_file.read() {
        if record.pid == std::process::id() as i32 {
            match instance_file.remove() {
                Ok(()) => tracing::debug!(
                    path = %instance_file.path().display(),
                    "removed instance record"
                ),
                Err(err) => tracing::warn!(error = %err, "failed to remove the instance record"),
            }
        }
    }
}

/// 等待 SIGTERM / SIGINT。
///
/// @intent 两个信号都接：SIGTERM 来自 `memora stop` 与 systemd，SIGINT 来自交互式
///         Ctrl-C。任一到达即触发优雅退出。
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(handler) => handler,
        Err(err) => {
            tracing::error!(error = %err, "cannot install SIGTERM handler");
            return;
        }
    };
    let mut interrupt = match signal(SignalKind::interrupt()) {
        Ok(handler) => handler,
        Err(err) => {
            tracing::error!(error = %err, "cannot install SIGINT handler");
            return;
        }
    };

    tokio::select! {
        _ = terminate.recv() => tracing::info!("received SIGTERM, shutting down gracefully"),
        _ = interrupt.recv() => tracing::info!("received SIGINT, shutting down gracefully"),
    }
}
