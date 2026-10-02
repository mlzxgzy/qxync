//! qxyncd —— qxync 守护进程（M1.5：本地 IPC；FUSE 由本进程持有）。
//!
//! ```text
//! qxync (CLI) ──unix socket + JSON 行──► qxyncd
//!                                          ├─ QxyncSession（登录/保活/重登）
//!                                          ├─ MountRegistry（FUSE 线程）
//!                                          └─ （M2 起）水合/轮询/队列/脱水
//! ```
//! 契约见 [`qxync_core::ipc`] 与 `docs/M1.5-设计.md`。

mod daemon;
mod peer_host;
mod sync;

use anyhow::{Context, Result};
use clap::Parser;
use qxync_core::ipc::{default_pid_path, default_socket_path};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "qxyncd",
    version,
    about = "qxync —— QNAP Qsync 的 Linux 常驻同步守护"
)]
struct Args {
    /// 连接 id（对应 ~/.config/qxync/links/<id>.json）
    #[arg(long, default_value = "default")]
    link: String,

    /// IPC socket 路径（默认 $XDG_RUNTIME_DIR/qxync/qxyncd.sock）
    #[arg(long)]
    socket: Option<PathBuf>,

    /// 前台运行（默认 daemon 化）
    #[arg(long)]
    foreground: bool,

    /// 启动时自动登录（失败只告警，不退出）
    #[arg(long)]
    auto_login: bool,

    /// 日志级别（等同 RUST_LOG）
    #[arg(long, default_value = "info")]
    log_level: String,

    /// 只打印解析后的配置，不做任何事（排障用）
    #[arg(long)]
    print_config: bool,

    /// ★ M8.2：启动时恢复 enabled=true 的同步任务（默认关闭）。
    /// 也可用环境变量 QXNYC_TASK_RESTORE=1 打开。
    /// 默认关闭是刻意的：恢复会「凭空挂载」，可能让上一次跑崩留下的挂载复活。
    #[arg(long)]
    restore_tasks: bool,
}

fn main() -> Result<()> {
    // 同 CLI：别让 EPIPE 变成 panic
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let args = Args::parse();

    // ★ 顺序很重要：fork 必须发生在建 tokio 运行时之前（多线程运行时 fork 不安全）
    if !args.foreground && !args.print_config {
        daemonize().context("daemon 化失败")?;
    }
    let _log_guard = init_logging(&args.log_level).context("初始化日志失败")?;

    let socket = args.socket.clone().unwrap_or_else(default_socket_path);
    if args.print_config {
        println!(
            "{}",
            serde_json::json!({
                "link": args.link,
                "socket": socket,
                "pid_file": default_pid_path(),
                "foreground": args.foreground,
                "auto_login": args.auto_login,
                "log_level": args.log_level,
            })
        );
        return Ok(());
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("建 tokio 运行时失败")?;
    // ★ M8.2：`--restore-tasks` 或 QXNYC_TASK_RESTORE=1 → 启动时恢复启用的任务
    let restore = args.restore_tasks
        || matches!(
            std::env::var("QXNYC_TASK_RESTORE").ok().as_deref(),
            Some("1") | Some("true") | Some("yes")
        );
    rt.block_on(daemon::run(daemon::Options {
        link_id: args.link,
        socket,
        auto_login: args.auto_login,
        restore_tasks: restore,
    }))
}

/// fork + setsid + 重定向标准流（日志走文件，见 [`init_logging`]）。
fn daemonize() -> Result<()> {
    unsafe {
        match libc::fork() {
            -1 => return Err(std::io::Error::last_os_error()).context("fork"),
            0 => { /* 子进程继续 */ }
            _ => std::process::exit(0), // 父进程返回，让 `qxync daemon start` 立即结束
        }
        if libc::setsid() == -1 {
            return Err(std::io::Error::last_os_error()).context("setsid");
        }
        // 标准流重定向到 /dev/null（不关闭 0/1/2，避免后续 open 抢占这些 fd）
        let devnull = std::ffi::CString::new("/dev/null").unwrap();
        let fd = libc::open(devnull.as_ptr(), libc::O_RDWR);
        if fd >= 0 {
            libc::dup2(fd, 0);
            libc::dup2(fd, 1);
            libc::dup2(fd, 2);
            if fd > 2 {
                libc::close(fd);
            }
        }
    }
    Ok(())
}

/// `RUST_LOG`/`--log-level` → stderr（前台）+ 按天滚动文件 `<state>/log/qxyncd.log`。
fn init_logging(level: &str) -> Result<Option<tracing_appender::non_blocking::WorkerGuard>> {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));
    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);

    let log_dir = qxync_core::ipc::default_log_dir();
    match std::fs::create_dir_all(&log_dir) {
        Ok(()) => {
            let appender = tracing_appender::rolling::daily(&log_dir, "qxyncd.log");
            let (nb, guard) = tracing_appender::non_blocking(appender);
            let file_layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(nb);
            tracing_subscriber::registry()
                .with(filter)
                .with(stderr_layer)
                .with(file_layer)
                .init();
            Ok(Some(guard))
        }
        Err(e) => {
            // 退到只写 stderr（例如 state 目录不可写）
            tracing_subscriber::registry()
                .with(filter)
                .with(stderr_layer)
                .init();
            eprintln!(
                "⚠️ 日志目录不可写({}): {e}，只输出到 stderr",
                log_dir.display()
            );
            Ok(None)
        }
    }
}
