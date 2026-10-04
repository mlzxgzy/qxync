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

    /// ★ M8.2 / ★ M15/T5：启动时恢复挂载（**默认开启**）。
    ///
    /// 先按 `$XDG_STATE_HOME/qxync/mounts.json` 恢复上次实际挂载的挂载点
    /// （含手工 `qxync mount` 挂的、没登记进任务表的），再补任务表里启用但还没挂上的。
    /// 已挂载的幂等跳过，单个失败只 WARN，不影响其它挂载点、也不阻止 daemon 启动。
    ///
    /// ★ T5 起默认开（原先默认关是怕「凭空挂载」；幂等 + 只恢复真挂过的消解了这个顾虑）。
    /// 要关用 `--no-restore-mounts` 或 `QXNYC_TASK_RESTORE=0`（验收矩阵要「开跑前环境干净」时）。
    #[arg(long)]
    restore_tasks: bool,

    /// ★ M15/T5：显式关掉启动恢复。`--restore-tasks` 现在是默认行为，这个开关才是「关」。
    #[arg(long)]
    no_restore_mounts: bool,

    /// ★ M15/T6：收到 SIGTERM 后「挂载守护」的最长秒数（**0 = 无限，默认**）。
    ///
    /// SIGTERM 时 daemon 会停同步、**保留 FUSE 挂载点**继续应答内核
    /// （`ls` / 已下载文件的读都还正常），这个秒数是「最多守护多久」。
    ///
    /// ★ **默认无限是刻意的，但要知道代价**：systemd 的 restart 是
    /// 「stop 旧进程 → 等它退出 → start 新进程」，守护进程不退会把 stop 阶段
    /// 卡死，最终被 `TimeoutStopSec` 后的 SIGKILL 杀掉 —— 挂载一样断。
    /// **所以这条参数不是给 `systemctl restart` 用的**，那条路请用
    /// `systemctl reload qxyncd`（SIGHUP，挂载点一秒都不中断）。
    ///
    /// 无论设不设时长，守护都能被立刻收干净：`qxync daemon stop`（IPC）、
    /// 再发一次 SIGTERM、或挂载表被卸空。设一个有限值只是防「忘了它还活着」。
    #[arg(long, default_value_t = 0)]
    mount_hold_secs: u64,
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
    // ★ M8.2 / ★ M15/T5：启动时恢复挂载。**默认开**（T5 起），三个开关从强到弱：
    //   `--no-restore-mounts` > `QXNYC_TASK_RESTORE=0` > `--restore-tasks`
    // 环境变量同时认开（1/true/yes）与关（0/false/no），这样 systemd unit 里
    // 想显式关掉不必改命令行。
    let env_restore = match std::env::var("QXNYC_TASK_RESTORE").ok().as_deref() {
        Some(v) if matches!(v, "1" | "true" | "yes") => Some(true),
        Some(v) if matches!(v, "0" | "false" | "no") => Some(false),
        _ => None,
    };
    let restore = if args.no_restore_mounts {
        false
    } else {
        env_restore.unwrap_or(true)
    };
    let res = rt.block_on(daemon::run(daemon::Options {
        link_id: args.link,
        socket,
        auto_login: args.auto_login,
        restore_tasks: restore,
        // ★ M15/T6：SIGTERM 后进「挂载守护」模式的上限（0 = 无限）
        mount_hold_secs: args.mount_hold_secs,
    }));
    // ★ daemon 化之后 fd 0/1/2 全都指向 `/dev/null`（见 [`daemonize`]），所以启动期的
    //   致命错误如果不写进日志文件就**彻底不可见** —— 用户只会看到 `qxync daemon start`
    //   报「socket 未就绪」，查不到任何原因。这一行就是那条线索，别删。
    if let Err(e) = &res {
        tracing::error!("qxyncd 启动失败: {e:#}");
    }
    res
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
