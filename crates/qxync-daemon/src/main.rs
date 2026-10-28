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
mod identity;
mod peer_host;
mod sync;

use anyhow::{bail, Context, Result};
use clap::Parser;
use qxync_core::ipc::{log_prefix_for, pid_path_for, socket_path_for};
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

/// 进程入口：把 [`run`] 的错误映射成退出码。
///
/// ★ M37：只有一个错误需要区分 —— 「另一个 daemon 已经占着 socket」用专用退出码
/// （[`daemon::EXIT_ALREADY_RUNNING`]），unit 里 `RestartPreventExitStatus=` 认它，
/// 这样 `Restart=always` 不会为「已经有人在同步」无限重启。
/// 其余错误保持 anyhow 的默认形态（`Error: ...` + 退出码 1）。
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            // 与 anyhow 的 `Termination` 输出一致：daemon 化后 fd 2 是 `/dev/null`，
            // 前台/CLI 模式下这行才是用户看到的报错（日志里另有一份 tracing 记录）。
            eprintln!("Error: {e:?}");
            if e.downcast_ref::<daemon::AlreadyRunning>().is_some() {
                std::process::ExitCode::from(daemon::EXIT_ALREADY_RUNNING)
            } else {
                std::process::ExitCode::FAILURE
            }
        }
    }
}

/// 真正的入口（原来的 `main`）。
fn run() -> Result<()> {
    // ★ M37：守护进程**必须忽略** SIGPIPE（Rust 运行时的默认处置），绝不能再照搬 CLI 的
    //   「恢复 SIG_DFL」—— 这一行是 0.7.10 及以前「休眠唤醒后 qxyncd 自己消失」的根因。
    //
    //   CLI 恢复 SIG_DFL 是对的：`qxync ls | head` 里 stdout 被提前关掉，进程该按 shell
    //   惯例安静地死掉。但 daemon 是**长驻多连接**进程（NAS HTTPS 长连接 + 保活 + 本地
    //   IPC + FUSE），把 SIGPIPE 设回 SIG_DFL 等于「只要有一次写到一个对端已关闭的
    //   socket/pipe，内核立刻杀掉整个进程」：
    //     * 没有任何清理机会 —— FUSE 挂载点、上传队列、journal 全停在半路；
    //     * tracing 的 non_blocking appender 缓冲来不及落盘，日志断在死前几分钟；
    //     * systemd 只看到 `code=killed, signal=PIPE`，而且 **SIGPIPE 在 systemd 的
    //       clean-exit 白名单里**（`systemd.service(5)`：SIGHUP/SIGINT/SIGTERM/SIGPIPE
    //       都算「干净退出」）→ `Result=success`，`Restart=on-failure` 永远不重启。
    //   真机形状（2026-10-26）：盒盖 s2idle 2 分钟后唤醒，唤醒第 20 秒死亡 —— 正是
    //   唤醒后第一轮变更轮询/会话保活去写休眠前建立的旧连接的时刻。见
    //   docs/M37-休眠唤醒后守护被SIGPIPE杀死.md。
    //
    //   忽略 SIGPIPE 之后，这些写入只会返回 EPIPE/ECONNRESET 错误，由各自的错误处理
    //   路径按「这条连接坏了 → 重连」消化，进程继续活着（Rust std 里唯一会在 EPIPE
    //   上 panic 的是 println!/print! 系列，daemon 里只有 `--print-config` 这一条一次性
    //   路径用它们，不会跑在长驻循环里）。
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    let args = Args::parse();
    if !qxync_core::LinkConfig::is_valid_id(&args.link) {
        bail!(
            "连接 id 不合法：{:?}（只允许 A-Za-z0-9 . _ -，且不能是 . / ..）",
            args.link
        );
    }

    // ★ 顺序很重要：fork 必须发生在建 tokio 运行时之前（多线程运行时 fork 不安全）
    if !args.foreground && !args.print_config {
        daemonize().context("daemon 化失败")?;
    }
    let _log_guard = init_logging(&args.log_level, &args.link).context("初始化日志失败")?;

    let socket = args
        .socket
        .clone()
        .unwrap_or_else(|| socket_path_for(&args.link));
    if args.print_config {
        println!(
            "{}",
            serde_json::json!({
                "link": args.link,
                "socket": socket,
                "pid_file": pid_path_for(&args.link),
                "log_prefix": log_prefix_for(&args.link),
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

/// `RUST_LOG`/`--log-level` → stderr（前台）+ 按天滚动文件
/// `<state>/log/qxyncd.log`（default）/ `qxyncd-<id>.log`（其它连接）。
fn init_logging(
    level: &str,
    link_id: &str,
) -> Result<Option<tracing_appender::non_blocking::WorkerGuard>> {
    use tracing_subscriber::layer::SubscriberExt as _;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));
    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);

    // ★ WPS 保存修复的伴生补丁：把 `log` crate 的记录桥接到 `tracing`。
    //   fuser 用的是 `log`，而本程序只初始化 `tracing` —— 不桥接的话
    //   fuser 的「[Not Implemented] link(...)」「Unknown FUSE opcode」
    //   全部静默丢弃，故障无从诊断（真机 WPS 保存失败查了整轮日志零线索）。
    let _ = log::set_logger(&FuserLogBridge);
    // fuser 的告警按 tracing 的 WARN 对齐；别让 log 的全局级别把它挡掉。
    log::set_max_level(log::LevelFilter::Warn);

    let log_dir = qxync_core::ipc::default_log_dir();
    // ★ M39：日志策略（单文件上限 / 保留天数 / 总占用）来自 settings.json。
    //   在这里先应用一次，今天的段一打开就受约束；之后 `settings_save` 会热更新
    //   （写路径每次现读策略，见 `qxync_core::logcfg::policy`）。
    //   settings.json 坏了/不存在不影响日志：退回默认策略（32MB / 14 天 / 256MB）。
    //   注意：这一行必须发生在 subscriber 装好**之前**（writer 要按策略决定开局切不切段），
    //   所以策略文本先存下来，等 subscriber 装好再 `info!` 出去 —— 否则会被丢弃，
    //   而「这次按什么策略在写」正是排查日志占用时第一眼要看的信息。
    let mut policy_note: Option<String> = None;
    if let Ok(paths) = qxync_core::ConfigPaths::discover() {
        match qxync_core::settings::Settings::load(&paths) {
            Ok(st) => {
                let p = qxync_core::logcfg::apply_settings(&st);
                policy_note = Some(format!(
                    "单文件 {} MB / 保留 {} 天 / 总量 {} MB",
                    p.max_file_bytes / 1048576,
                    p.keep_days,
                    p.max_total_bytes / 1048576
                ));
            }
            Err(e) => {
                policy_note = Some(format!("（读取 settings.json 失败，用默认策略: {e}）"));
            }
        }
    }
    // ★ 日志文件建不出来时**绝不能 panic**：daemon 化后 fd 0/1/2 都指向
    //   /dev/null，启动期 panic = 用户只看到「socket 未就绪」且日志零线索
    //   （xv 真机排查过一整轮）。`rolling::daily()` 内部是 `.expect()` → panic，
    //   所以这里用 builder 拿 `Result`，失败就退到只写 stderr。
    //
    //   ★ M39：换成自己的 `RotatingWriter` —— tracing_appender 只能按**时间**
    //   轮转，同一天内没有大小上限（真机 2026-10-23 单文件 5.17 GB 就是这样来的）。
    //   文件名规则不变（`<前缀>.log.<YYYY-MM-DD>`），所以历史文件仍然排得进序列。
    let prefix = log_prefix_for(link_id);
    let prefix = prefix.trim_end_matches(".log").to_string();
    let appender = std::fs::create_dir_all(&log_dir)
        .ok()
        .and_then(|()| qxync_core::logcfg::RotatingWriter::new(&log_dir, &prefix).ok());
    let (file_layer, guard) = match appender {
        Some(appender) => {
            let (nb, guard) = tracing_appender::non_blocking(appender);
            let layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(nb);
            (Some(layer), Some(guard))
        }
        None => {
            eprintln!("⚠️ 日志目录不可写({})，只输出到 stderr", log_dir.display());
            (None, None)
        }
    };

    // ★ 不能用 `SubscriberInitExt::init()`/`try_init()`：tracing-subscriber
    //   的默认 feature 带 `tracing-log`，init 内部还会调 `LogTracer::init()`
    //   去占 `log` 全局 logger —— 而本进程在上面已用 `log::set_logger`
    //   装了自己的 fuser 桥，于是 SetLoggerError → **启动即 panic**；
    //   daemon 化后 stderr 指向 /dev/null，用户只看到「socket 未就绪」。
    //   直接设全局 subscriber 即可；log → tracing 的桥由 FuserLogBridge 负责。
    let subscriber = tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer);
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|e| anyhow::anyhow!("设置全局 tracing subscriber 失败: {e}"))?;
    if let Some(note) = policy_note {
        tracing::info!("★ M39：日志轮转策略已应用（{note}）");
    }
    Ok(guard)
}

/// ★ M41：fuser 桥同一诊断的限流窗口（**10 分钟**）。
///
/// 真机取证：`[Not Implemented]` 这类行 10-26 共 225 条、随机分布在整天 ——
/// 60s 桶只能压掉约一半，且它们是「某个 FUSE 操作没实现」的长期已知诊断，
/// 重复频率没有诊断价值。同一归一化签名 10 分钟一条，足够提示「还在发生」。
const FUSER_BRIDGE_WINDOW: std::time::Duration = std::time::Duration::from_secs(600);

/// ★ M41：fuser 日志的**归一化签名**（限流用，不含 opcode 数字 / 具体路径）。
///
/// * 多行消息只取第一行（真机栈式诊断的第一行才是「是什么」）；
/// * 连续数字压成 `#`：`Unknown FUSE opcode 0x15` 与 `0x16` 属于同一类诊断，
///   否则每个 opcode 各自一个键 = 等于没限流。
///
/// 具体值不丢：放行的那条消息是**完整原文**（见 [`FuserLogBridge::log`]）。
fn fuser_log_signature(msg: &str) -> String {
    let first = msg.lines().next().unwrap_or("");
    let mut out = String::with_capacity(first.len().min(256));
    let mut prev_digit = false;
    for ch in first.chars().take(240) {
        if ch.is_ascii_digit() {
            if !prev_digit {
                out.push('#');
                prev_digit = true;
            }
        } else {
            prev_digit = false;
            out.push(ch);
        }
    }
    out
}

/// `log` → `tracing` 桥（只装一次，见 [`init_logging`]）。
///
/// fuser 0.17 的所有诊断（未实现的 FUSE 操作、未知 opcode、会话错误）
/// 都走 `log` crate；不桥接 = 全部蒸发。目标统一加 `fuser` 前缀便于过滤。
struct FuserLogBridge;
impl log::Log for FuserLogBridge {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Warn
    }
    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // ★ M41：同一诊断（归一化签名）10 分钟最多一条；被压的写 debug（原文不丢），
        //   放行的那条在末尾带上「过去窗口内另省略 N 条」。
        //
        // 桥里只做 `format!` + 一次 `noise` 的 HashMap 判定（锁内不碰 IO），
        // 不改变 log → tracing 的级别映射。
        let msg = record.args().to_string();
        let adm = qxync_core::noise::dedupe_in(
            "fuser.bridge",
            &fuser_log_signature(&msg),
            FUSER_BRIDGE_WINDOW,
        );
        if !adm.emit {
            tracing::debug!(target: "fuser", "{msg}");
            return;
        }
        let suffix = adm.note();
        match record.level() {
            log::Level::Error => {
                tracing::error!(target: "fuser", "{}{}", msg, suffix)
            }
            log::Level::Warn => tracing::warn!(target: "fuser", "{}{}", msg, suffix),
            log::Level::Info => tracing::info!(target: "fuser", "{}{}", msg, suffix),
            log::Level::Debug => tracing::debug!(target: "fuser", "{}{}", msg, suffix),
            log::Level::Trace => tracing::trace!(target: "fuser", "{}{}", msg, suffix),
        }
    }
    fn flush(&self) {}
}

// ---------------------------------------------------------------- 单测

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------ ★ M41 fuser 签名

    /// 同一 opcode 族（数字不同）与多行消息的首行 → 同一签名（否则限流无效）。
    #[test]
    fn fuser_signature_normalizes_numbers_and_multiline() {
        assert_eq!(
            fuser_log_signature("[Not Implemented] ioctl(0x15)\nbacktrace: a\nb"),
            fuser_log_signature("[Not Implemented] ioctl(0x16)"),
            "多行只取首行、数字归一化后必须同签名"
        );
        assert_eq!(
            fuser_log_signature("Unknown FUSE opcode 42"),
            fuser_log_signature("Unknown FUSE opcode 7")
        );
        // 原文不丢的前提：签名只是键，放行消息仍是完整原文（另测）
        assert_eq!(fuser_log_signature("no digits here"), "no digits here");
    }

    /// 不同诊断必须各自放行（不能把「未实现」和「会话错误」压成一条）。
    #[test]
    fn fuser_signature_keeps_distinct_diagnostics_apart() {
        let a = fuser_log_signature("[Not Implemented] link(...)");
        let b = fuser_log_signature("Unknown FUSE opcode 0x15");
        let c = fuser_log_signature("Session destroyed by peer");
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
    }

    /// 限流窗口必须是 10 分钟（不是 60s：真机 `[Not Implemented]` 间隔随机，60s 只压一半）。
    #[test]
    fn fuser_bridge_window_is_ten_minutes() {
        assert_eq!(FUSER_BRIDGE_WINDOW, std::time::Duration::from_secs(600));
    }

    /// 同一签名 10 分钟内压掉、不同 opcode 各自放行。
    #[test]
    fn fuser_bridge_dedupes_same_signature_within_window() {
        let site = "test.fuser.bridge.dedupe";
        qxync_core::noise::reset_site(site);
        let sig = fuser_log_signature("[Not Implemented] ioctl(0x15)");
        assert!(
            qxync_core::noise::dedupe_in(site, &sig, FUSER_BRIDGE_WINDOW).emit,
            "首次必须放行"
        );
        assert!(
            !qxync_core::noise::dedupe_in(site, &sig, FUSER_BRIDGE_WINDOW).emit,
            "同一签名 10 分钟内必须压掉"
        );
        let other = fuser_log_signature("Unknown FUSE opcode 0x15");
        assert!(
            qxync_core::noise::dedupe_in(site, &other, FUSER_BRIDGE_WINDOW).emit,
            "另一种诊断必须各自放行"
        );
        qxync_core::noise::reset_site(site);
    }

    /// 窗口到期后再放行，并如实带上「过去窗口内另省略 N 条」。
    #[test]
    fn fuser_bridge_reemits_after_window_expires() {
        let site = "test.fuser.bridge.expiry";
        qxync_core::noise::reset_site(site);
        // 用 20ms 的窗口验证「到期即放行」语义（生产常量是 600s，见上一个测试）。
        let win = std::time::Duration::from_millis(20);
        assert!(qxync_core::noise::dedupe_in(site, "sig", win).emit);
        assert!(!qxync_core::noise::dedupe_in(site, "sig", win).emit);
        std::thread::sleep(std::time::Duration::from_millis(30));
        let a = qxync_core::noise::dedupe_in(site, "sig", win);
        assert!(a.emit, "窗口到期必须再放行");
        assert_eq!(a.suppressed, 1, "放行时要报出被压掉的条数");
        qxync_core::noise::reset_site(site);
    }
}
