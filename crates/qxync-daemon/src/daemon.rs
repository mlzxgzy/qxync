//! daemon 主体：状态、IPC 服务端、各方法实现。
//!
//! 设计见 `docs/M1.5-设计.md`：单进程持有 FUSE；CLI/GUI 通过 unix socket 的 JSON 行协议访问。

use anyhow::{bail, Context, Result};
use qxync_client::peer::PeerEvent;
use qxync_client::{Client, Session};
use qxync_core::dehydrate::{Block, CacheLimit, Policy};
use qxync_core::ipc::{
    decode_line, encode_line, mask_sid, mask_token, CacheInfo, CursorInfo, DaemonInfo,
    DecisionInfo, DecisionsData, DehydrateData, ErrorKind, FileStateInfo, FileStatesData, GetData,
    HydroStats, IpcError, JournalData, LinkInfo, LoginData, LsData, MountInfo, PeerData, PingData,
    PutData, Request, RequestEnvelope, Response, RootsData, RulesData, ServerInfo, SessionInfo,
    SettingsData, ShutdownData, SpaceData, StatusData, StoreData, SyncCursors, SyncInfo, TaskInfo,
    TasksData, IPC_VERSION,
};
use qxync_core::mounts::{MountRecord, MountsFile};
use qxync_core::rules::Rules;
use qxync_core::settings::{ProxySpec, Settings};
use qxync_core::store::JournalEntry;
use qxync_core::tasks::{conflict_label, has_any_task, Task, CONFLICT_RENAME_LOCAL};
use qxync_core::{ConfigPaths, Credentials, Error as CoreError, LinkConfig, PeerConfig};
use qxync_fuse::delete::DeleteQueue;
use qxync_fuse::upload::UploadQueue;
use qxync_fuse::{CacheMode, FsHandle, HydroCounters, LocalView, MountHandle, PinMap, QxyncFs};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, Mutex, Notify};

use crate::peer_host::PeerHost;
use crate::sync::{self, MountView, SyncConfig, SyncState, SyncStats};

/// ★ M8.3：journal 的保留上限（**必须有**：同步日志是无限增长型数据，
/// 不加约束会把 `sync.db` 撑大）。默认 1 万条 / 30 天，环境变量可调。
fn journal_max_rows() -> i64 {
    std::env::var("QXNYC_JOURNAL_MAX_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000)
}
fn journal_max_age_days() -> i64 {
    std::env::var("QXNYC_JOURNAL_MAX_AGE_DAYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30)
}
/// 轮转间隔秒数（默认 300；验收里调成 1 用来快速观察）。
fn journal_trim_secs() -> u64 {
    std::env::var("QXNYC_JOURNAL_TRIM_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300)
        .max(1)
}

/// ★ M10：挂载点等「重登回复」的最长时间（FUSE 线程会同步阻塞在这里）。
const SESSION_REFRESH_TIMEOUT: Duration = Duration::from_secs(30);
/// ★ M10：两次重登之间的最小间隔 —— 多个挂载点同时撞上会话失效时只登一次。
const SESSION_REFRESH_DEDUP: Duration = Duration::from_secs(3);
/// ★ M10：会话保活探测间隔（`QXNYC_SESSION_KEEPALIVE=0` 关掉）。
const DEFAULT_KEEPALIVE_SECS: u64 = 120;

pub struct Options {
    pub link_id: String,
    pub socket: PathBuf,
    pub auto_login: bool,
    /// ★ M8.2 / ★ M15/T5：启动时恢复挂载。
    ///
    /// 恢复做两件事（顺序固定）：先按 `mounts.json` 恢复**上一次实际挂了什么**
    /// （含手工 `qxync mount` 挂的），再补 `tasks/` 里**还没挂上**的启用任务。
    ///
    /// ★ M15/T5 起**默认开启**。当初默认关是怕「凭空挂载」，那个顾虑现在被两件事
    /// 消解了：
    ///
    /// * **幂等** —— 已挂载（`is_mounted` / 挂载表命中）就跳过，不会叠一层；
    /// * **只恢复真的挂过的** —— 恢复源是 `mounts.json`（现场），不是「猜用户想要什么」。
    ///   上次崩在半路的挂载因为没写进记录，不会复活。
    ///
    /// 仍可关掉：`--no-restore-mounts` 或 `QXNYC_TASK_RESTORE=0`
    /// （验收矩阵要「开跑前环境干净」时用）。
    pub restore_tasks: bool,
    /// ★ M15/T6：收到 SIGTERM 后转入「挂载守护模式」的最长秒数（**0 = 无限**）。
    ///
    /// ★ **默认无限是刻意的，但要知道它的代价**：systemd 的 restart 是
    /// 「先 stop 旧进程、**等它退出**、再 start 新进程」，一个不退出的守护进程
    /// 会把 stop 阶段卡死，最终被 `TimeoutStopSec` 后的 SIGKILL 杀掉 ——
    /// 挂载一样断。**所以这条路不是给 `systemctl restart` 用的**，
    /// 它给的是「手工 `kill -TERM` 之后本地文件还能读」这个过渡态。
    ///
    /// 想要 systemd 下挂载点一秒不断，用 **SIGHUP**（`systemctl reload qxyncd`）：
    /// 它不终止进程、不碰 FUSE 会话。
    ///
    /// 无论设不设时长，守护进程都能被**立刻**收干净：
    /// `qxync daemon stop`（IPC）、第二次 SIGTERM、挂载表变空。
    pub mount_hold_secs: u64,
}

/// ★ M15/T6：**主循环为什么退出** —— 决定退场时要不要卸载挂载。
///
/// 这三种来源语义本来就不同，T6 之前却是同一条路径（`daemon.rs` 主循环
/// `break` 之后一律 `shutdown_all_mounts`），于是
/// 「我想重启一下同步」和「我要把它关掉」产生了同样的后果。
///
/// 刻意**不用** `Options` 上的开关让 CLI 传参：`qxync daemon stop` 走的是 IPC，
/// 根本不 spawn 新进程，没有机会传 Options；而 `systemctl restart` 的 stop 阶段
/// 与 `stop` 发的是同一个 SIGTERM，也无法从信号本身分辨意图。
/// 唯一可靠的判据就是**信号/IPC 的来源**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitReason {
    /// IPC `Shutdown`（`qxync daemon stop` 走的就是它）—— 用户明确要「关掉」。
    IpcShutdown,
    /// SIGINT（Ctrl-C）—— 同样明确要「关掉」。
    CtrlC,
    /// SIGTERM（`systemctl stop` 与 `systemctl restart` **都**发它）——
    /// 语义是「别打断我」，所以要保留挂载。
    Sigterm,
}

impl ExitReason {
    /// 这次退出要不要**真正卸载**全部挂载点。
    ///
    /// 只有「用户明确要关掉」的两种才卸载。SIGTERM 走保留挂载（→ 挂载守护模式）。
    fn should_unmount(self) -> bool {
        !matches!(self, ExitReason::Sigterm)
    }

    /// 日志/错误文案里的名字。
    fn label(self) -> &'static str {
        match self {
            ExitReason::IpcShutdown => "IPC shutdown（qxync daemon stop）",
            ExitReason::CtrlC => "SIGINT",
            ExitReason::Sigterm => "SIGTERM",
        }
    }
}

/// ★ M15/T6：同步引擎的「代次」令牌 —— SIGHUP 重载与「进挂载守护模式」都靠它。
///
/// 四个后台任务（poller / 脱水 / journal 落库 / 自动释放空间 / 会话保活）都把自己
/// 这一代的 [`EngineGen`] 带进循环；[`SyncEpoch::bump`] 一次，它们就在**做完当前
/// 一轮之后**干净退出（不是被 `abort` 劈开，见 [`reload_sync_engine`] 的理由）。
///
/// 为什么不用 `JoinHandle::abort()`：poller 那一轮 `run_sync_once` 会在改
/// baseline、推冲突决策，从中间劈开等于让同一处改动被判两次冲突。
struct SyncEpoch {
    tx: tokio::sync::watch::Sender<u64>,
}

impl SyncEpoch {
    fn new() -> Self {
        // 后一个 `_rx` 只是为了让 channel 活到 `subscribe`；`subscribe` 不依赖它。
        let (tx, _rx) = tokio::sync::watch::channel(0u64);
        Self { tx }
    }

    /// 当前这一代的句柄（后台任务启动时各拿一份）。
    fn gen(&self) -> EngineGen {
        let mut rx = self.tx.subscribe();
        // `borrow_and_update`：把当前值标记成「已看到」。**必须**如此 ——
        // 否则新任务第一次 `changed()` 会立刻返回（它以为换代发生在订阅之前），
        // 于是刚启动就退出。而且它顺带避免了 `borrow` 的 Ref 活到语句末尾、
        // 与下面把 `rx` move 进结构体冲突的借用错误。
        let _ = *rx.borrow_and_update();
        EngineGen { rx }
    }

    /// 换一代：所有还在跑的旧代任务会在各自下一个循环点退出。
    ///
    /// 用 `send_modify` 而不是 `send`：进程刚起来、一个订阅者都还没有时
    /// `send` 会返回 `Err` 且**不改变值**（那会让后面 subscribe 的人拿到旧代次）。
    fn bump(&self) -> u64 {
        self.tx.send_modify(|v| *v += 1);
        *self.tx.borrow()
    }
}

/// ★ M15/T6：一个后台任务所属的「代次」。
///
/// 克隆一份给 `tokio::spawn`（`watch::Receiver` 本身是 Clone 的）。
#[derive(Clone)]
struct EngineGen {
    rx: tokio::sync::watch::Receiver<u64>,
}

impl EngineGen {
    /// 睡 `d`；期间代次被换掉就立刻醒并返回 `false`（= 这一代该退休了）。
    ///
    /// 只需要「睡一段时间」的任务用它（脱水 / 自动释放空间 / 保活 / journal）。
    async fn sleep(&mut self, d: Duration) -> bool {
        tokio::select! {
            _ = self.rx.changed() => false,
            _ = tokio::time::sleep(d) => true,
        }
    }

    /// 「代次被换掉」的通知，供已经有 `select!` 的地方（poller）加一个分支。
    ///
    /// `watch::Receiver::changed()` 在**没有**新值时会挂起，所以不会自己醒 ——
    /// 这正是 `select!` 需要的语义。任务正在做当前一轮时换代，它会在这一轮
    /// 做完、回到 `select!` 时立刻看到通知并退出（不会中断正在做的对账）。
    async fn changed(&mut self) {
        let _ = self.rx.changed().await;
    }
}

pub(crate) struct MountEntry {
    info: MountInfo,
    /// ★ M10：这个挂载点自己的 NAS 客户端（sid 与 daemon 主体那份**热同步**）。
    ///
    /// 读写挂载的上传队列也共用它，所以推一次 sid 两处都生效。
    client: Arc<Client>,
    /// ★ M10：挂载时用的是哪个账号 —— 同 link 换账号登录时**不能**把新 sid 推给旧挂载点
    /// （节点表/缓存还是上一个账号的树，推过去会张冠李戴）。
    user: String,
    /// ★ M3：fuser 的后台会话（`umount` 时 join）。
    session: Option<qxync_fuse::BackgroundSession>,
    /// ★ M3：脱水要用的内核通知句柄（`inval_inode`）。
    notifier: qxync_fuse::Notifier,
    counters: Arc<HydroCounters>,
    /// 读写挂载时的上传队列（卸载前要排空）。
    upload: Option<Arc<UploadQueue>>,
    /// ★ M11：读写挂载时的删除队列（卸载前要排空，否则「本地已删、远端还在」）。
    delete_queue: Option<Arc<DeleteQueue>>,
    /// ★ M2c：共享节点表句柄（同步引擎在挂载线程外刷新远端变更）。
    pub(crate) handle: FsHandle,
    /// ★ M3：缓存模式（pagecache / direct）。
    cache_mode: CacheMode,
    /// ★ M8.4：该挂载点的冲突策略（同步引擎按它分派冲突）。
    conflict: String,
    /// ★ M15/T5：挂载时的**原始参数**（`mount()` 收到的那一份，canonicalize 之后）。
    ///
    /// 存它而不是卸载时现凑，是为了让 `mounts.json` 与「当时实际挂的是什么」逐字一致 ——
    /// 线程数、水合超时、删除上限这些从 `MountEntry` 里已经取不回来了（`MountEntry`
    /// 只留了同步引擎要用的 `cache_mode` / `conflict`）。
    rec: MountRecord,
}

pub(crate) struct State {
    version: &'static str,
    started: Instant,
    socket: PathBuf,
    pub(crate) link: LinkConfig,
    client: Mutex<Client>,
    session: Mutex<Option<Session>>,
    /// 远端路径 → pin 状态（与 FUSE 实例共享，`getfattr -n user.qxync.pin` 能看到）。
    pins: PinMap,
    pub(crate) mounts: StdMutex<HashMap<PathBuf, MountEntry>>,
    /// ★ M2c：引擎专用的 HTTP 客户端（不抢 `client` 的锁，长轮询不阻塞 IPC 命令）。
    engine_client: Mutex<Option<Arc<Client>>>,
    /// ★ M10：挂载点在 FUSE 线程里遇到鉴权失败时，同步要一个新 sid（见 `spawn_session_broker`）。
    sid_refresher: StdMutex<Option<qxync_fuse::SidRefresher>>,
    /// 游标 + baseline（原子落盘）。
    sync_store: StdMutex<SyncState>,
    /// 引擎计数器。
    sync_stats: Arc<SyncStats>,
    /// 引擎参数（`--force-deletes` / `--max-deletes` 临时改）。
    sync_cfg: StdMutex<SyncConfig>,
    /// 后台轮询间隔秒数（0 = 暂停）。
    sync_interval: StdMutex<u64>,
    /// ★ M3：脱水配置 + 计数。
    dehydrate_cfg: StdMutex<DehydrateCfg>,
    dehydrate_stats: Arc<DehydrateStats>,
    /// ★ M7：选择性同步规则（`link.exclude` + 临时文件过滤）；挂载时注入 FUSE 与同步引擎。
    rules: Arc<Rules>,
    /// ★ M7：解析失败的规则原文（`qxync rules` 要显示，不静默）。
    rules_bad: Vec<String>,
    /// ★ M7：已配对的对等设备（与 FUSE 共享：水合时先试 LAN）。
    peers: Arc<StdMutex<Vec<PeerConfig>>>,
    /// ★ M7：对等主机（事件快路径 / 配对 / 直传）；`peer_listen` 没配时也持有（listen=None）。
    peer: StdMutex<Option<Arc<PeerHost>>>,
    /// ★ M7：对端事件到达时唤醒轮询线程。
    peer_wake: Arc<Notify>,
    /// ★ M8.3：journal 写入缓冲。热路径（同步轮 / 脱水）只往这里 push，
    /// 由后台任务每 500ms 批量落库 —— **绝不在热路径上开事务写库**。
    journal_buf: StdMutex<Vec<qxync_core::store::JournalEntry>>,
    /// ★ M8.4：全局设置（`settings.json`）。挂载/登录时的代理、自动释放空间、
    /// 通知开关都从这里取；**默认值 = M8.3 的行为**。
    settings: StdMutex<qxync_core::settings::Settings>,
    /// ★ M8.4：解析好的代理规格（启动时算一次；`settings_save` 后刷新）。
    /// 记下来是为了 `status` 能如实报告「当前到底走不走代理」。
    proxy: StdMutex<qxync_core::settings::ProxySpec>,
    /// ★ M8.4：上一次「按频率释放空间」的触发时间（unix 秒）。
    auto_free_last: Arc<AtomicU64>,
    /// ★ M15/T6：同步引擎的代次令牌。SIGHUP 重载 / SIGTERM 转挂载守护都靠它
    /// 把 poller 等后台任务收干净（详见 [`SyncEpoch`]）。
    sync_epoch: SyncEpoch,
}

// ---------------------------------------------------------------- 入口

/// ★ 空转待命时，任何需要 NAS 连接的请求都回这一句。
const NOT_CONFIGURED: &str = "daemon 在运行，但还没有配置 NAS 连接：先 `qxync login`\
     （或在 GUI「设置 → 连接」里保存），配好后 daemon 会自动开始同步，不需要重启。";

/// 空转待命是怎么结束的。
enum IdleExit {
    /// link 配置出现了：交给 [`run`] 转入同步模式（同一个进程）。
    Configured,
    /// 收到 shutdown / SIGINT：正常退出。
    Shutdown,
}

/// 绑 socket + 写 pid 文件。「谁占着这个 socket」的语义只在这里定义一次，
/// 空转待命与同步模式共用。
async fn bind_socket(opts: &Options) -> Result<(UnixListener, PathBuf)> {
    if let Some(dir) = opts.socket.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("创建 socket 目录失败: {}", dir.display()))?;
        qxync_core::config::restrict_perms(dir, 0o700)
            .with_context(|| format!("设置 socket 目录权限失败: {}", dir.display()))?;
    }
    if opts.socket.exists() {
        if ping_socket(&opts.socket).await.is_ok() {
            bail!("daemon 已在运行（socket {}）", opts.socket.display());
        }
        std::fs::remove_file(&opts.socket).ok(); // 陈旧 socket
    }
    let listener = UnixListener::bind(&opts.socket)
        .with_context(|| format!("绑定 socket 失败: {}", opts.socket.display()))?;
    qxync_core::config::restrict_perms(&opts.socket, 0o600)?;
    let pid_path = qxync_core::ipc::default_pid_path();
    std::fs::write(
        &pid_path,
        format!("{}\n{}\n", std::process::id(), opts.socket.display()),
    )
    .ok();
    Ok((listener, pid_path))
}

/// ★ 空转待命：**没有任何连接配置时，daemon 也要一直活着**。
///
/// 只服务 `Ping` / `Status` / `Shutdown`（其余请求一律回 [`NOT_CONFIGURED`]），
/// 每 2 秒看一眼 link 文件；一旦出现就收掉自己的 socket/pid 返回，
/// 由 [`run`] 在同一进程里接管并转入同步模式。
///
/// 为什么不做成「直接退出」：用户的计划是 daemon 常驻后台（systemd / 自启），
/// 「没配连接」是**正常状态**而不是错误 —— 配好连接之后它自己就该开始干活。
async fn idle_until_configured(paths: &ConfigPaths, opts: &Options) -> Result<IdleExit> {
    let (listener, pid_path) = bind_socket(opts).await?;
    let socket = opts.socket.clone();
    let started = Instant::now();
    let (tx, mut rx) = mpsc::channel::<()>(1);
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    let mut sigterm = terminate_signal();
    tracing::info!(
        "qxyncd 空转待命 pid={} socket={} —— 只服务 ping/status/shutdown 与设置读写",
        std::process::id(),
        socket.display()
    );
    loop {
        tokio::select! {
            _ = rx.recv() => {
                cleanup_idle(&socket, &pid_path);
                tracing::info!("qxyncd 退出（空转待命：始终没有配置连接）");
                return Ok(IdleExit::Shutdown);
            }
            _ = tokio::signal::ctrl_c() => {
                cleanup_idle(&socket, &pid_path);
                tracing::info!("收到 SIGINT（空转待命）");
                return Ok(IdleExit::Shutdown);
            }
            _ = wait_sigterm(&mut sigterm) => {
                cleanup_idle(&socket, &pid_path);
                tracing::info!("收到 SIGTERM（空转待命）");
                return Ok(IdleExit::Shutdown);
            }
            _ = tick.tick() => {
                if LinkConfig::load(paths, &opts.link_id).is_ok() {
                    // 让 `run()` 能干净地重新绑（它自己会建 socket/pid）
                    cleanup_idle(&socket, &pid_path);
                    return Ok(IdleExit::Configured);
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let tx = tx.clone();
                        let socket = socket.clone();
                        let paths = paths.clone();
                        tokio::spawn(async move {
                            if let Err(e) =
                                handle_idle_conn(stream, started, socket, tx, paths).await
                            {
                                tracing::warn!("连接结束（空转待命）: {e}");
                            }
                        });
                    }
                    Err(e) => tracing::warn!("accept 失败: {e}"),
                }
            }
        }
    }
}

fn cleanup_idle(socket: &Path, pid_path: &Path) {
    let _ = std::fs::remove_file(socket);
    let _ = std::fs::remove_file(pid_path);
}

/// SIGTERM 的监听器 —— **`systemd --user stop/restart` 发的就是它**。
///
/// 不处理的话默认动作是立刻终止：FUSE 挂载点会留在那儿、socket/pid 也不删。
///
/// 注册失败就退化成「永不触发」而**不是** panic：daemon 化之后 stderr 指向 `/dev/null`，
/// panic 信息会彻底丢掉，不如让它照常跑、只少一条优雅退出路径。
fn terminate_signal() -> Option<tokio::signal::unix::Signal> {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!("注册 SIGTERM 处理器失败（{e}）：`systemctl stop` 会退化为直接终止");
            None
        }
    }
}

/// `select!` 用的「等 SIGTERM」future：没有监听器就永远挂起。
async fn wait_sigterm(sig: &mut Option<tokio::signal::unix::Signal>) {
    wait_signal(sig).await
}

/// ★ M15/T6：SIGHUP 的监听器 —— `systemctl reload qxyncd` 默认发的就是它。
///
/// **为什么 SIGHUP 才是「重启不丢挂载」真正能用的那条路**：reload **不终止进程**，
/// 于是 FUSE 会话压根不会断。SIGTERM 不行 —— systemd 的 restart 是
/// 「stop 旧进程 → 等它退出 → start 新进程」，一个不退出的守护进程会把 stop
/// 阶段卡死到 `TimeoutStopSec`，最后被 SIGKILL 杀掉，挂载一样断。
///
/// 注册失败同样退化成「永不触发」而不 panic（与 [`terminate_signal`] 同理）。
fn hangup_signal() -> Option<tokio::signal::unix::Signal> {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!("注册 SIGHUP 处理器失败（{e}）：`systemctl reload` 将退化为无操作");
            None
        }
    }
}

/// 「等某个信号」future：没有监听器就永远挂起。
async fn wait_signal(sig: &mut Option<tokio::signal::unix::Signal>) {
    match sig {
        Some(s) => {
            s.recv().await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// ★ M15/T6：`select!` 用的「等 SIGHUP」future。
async fn wait_sighup(sig: &mut Option<tokio::signal::unix::Signal>) {
    wait_signal(sig).await
}

/// 空转待命时的连接处理：放行 `Ping` / `Status` / `Shutdown`，以及**纯本地文件**的
/// `Settings` / `SettingsSave`（GUI 的设置页要用；它们跟 NAS 连接无关）。
async fn handle_idle_conn(
    stream: UnixStream,
    started: Instant,
    socket: PathBuf,
    shutdown: mpsc::Sender<()>,
    paths: ConfigPaths,
) -> Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let resp = match decode_line::<RequestEnvelope>(line.as_bytes()) {
            Ok(env) if env.v != IPC_VERSION => Response::err(
                ErrorKind::BadVersion,
                format!("协议版本 {} 不受支持（本进程支持 {IPC_VERSION}）", env.v),
            ),
            Ok(env) => {
                let out: Result<serde_json::Value, IpcError> = match env.req {
                    Request::Ping => to_value(PingData {
                        pong: true,
                        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
                        pid: std::process::id(),
                        uptime_secs: started.elapsed().as_secs(),
                    }),
                    Request::Status => to_value(idle_status(&socket, started)),
                    Request::Settings => to_value(idle_settings_data(
                        &paths,
                        false,
                        Some("daemon 空转待命中：设置是纯本地文件，与有没有 NAS 连接无关".into()),
                    )),
                    Request::SettingsSave {
                        settings,
                        autostart_exe,
                    } => idle_settings_save(&paths, settings, autostart_exe),
                    Request::Shutdown => {
                        // 与同步模式一致：先回响应，再触发退出
                        let tx = shutdown.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(150)).await;
                            let _ = tx.send(()).await;
                        });
                        to_value(ShutdownData { unmounted: 0 })
                    }
                    other => {
                        tracing::debug!("空转待命：拒绝 {}（还没有配置 NAS 连接）", other.method());
                        Err(IpcError::new(ErrorKind::NotLoggedIn, NOT_CONFIGURED))
                    }
                };
                match out {
                    Ok(v) => Response::ok(v),
                    Err(e) => Response::err(e.kind, e.message),
                }
            }
            Err(e) => Response::err(ErrorKind::BadRequest, format!("请求解析失败: {e}")),
        };
        wr.write_all(&encode_line(&resp)?).await?;
    }
    Ok(())
}

/// 空转待命时的 `status`：daemon 信息如实报，`link: None` 让前端显示「未配置」。
fn idle_status(socket: &Path, started: Instant) -> StatusData {
    StatusData {
        daemon: DaemonInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            pid: std::process::id(),
            uptime_secs: started.elapsed().as_secs(),
            socket: socket.display().to_string(),
        },
        link: None,
        logged_in: false,
        session: None,
        server: None,
        cursors: None,
        hydro: HydroStats::default(),
        uploads: None,
        // ★ M11：待命状态没有挂载点，也就没有删除队列。
        deletes: None,
        sync: None,
        cache: None,
        // ★ T8：待命状态没有挂载点，也就没有在途传输。
        transfers: None,
        mounts: Vec::new(),
    }
}

pub async fn run(opts: Options) -> Result<()> {
    let paths = ConfigPaths::discover()?;
    paths
        .ensure_dirs()
        .with_context(|| "创建配置 / 数据 / 日志目录失败")?;

    // ★ 常驻后台的第一要义是**起得来**：一份连接配置都还没有时，daemon 照样要活着
    //   （ping 有回应、status 报「未配置」），而不是直接退出。
    //   这层「空转待命」只服务 ping/status/shutdown 并盯着 link 文件；配置一出现就
    //   原地转入下面的同步模式 —— 不 re-exec、不换进程，也不用用户再敲一次 `daemon start`。
    if let Err(e) = LinkConfig::load(&paths, &opts.link_id) {
        tracing::warn!(
            "还没有可用的 NAS 连接「{}」（{e}）—— daemon 空转待命，配好连接后自动开始同步",
            opts.link_id
        );
        match idle_until_configured(&paths, &opts).await? {
            IdleExit::Shutdown => return Ok(()),
            IdleExit::Configured => tracing::info!("检测到 NAS 连接配置，转入同步模式"),
        }
    }

    let link = LinkConfig::load(&paths, &opts.link_id)
        .with_context(|| format!("读取连接配置失败（先 `qxync --host ... login`）"))?;
    let (listener, pid_path) = bind_socket(&opts).await?;

    // ★ M8.4：全局设置（代理 / 自动释放空间 / 通知）。
    //   读不到就全默认（= M8.3 行为）：**设置文件坏了也不该让 daemon 起不来**。
    let settings = Settings::load(&paths).unwrap_or_else(|e| {
        tracing::warn!("读取 settings.json 失败，按默认设置运行: {e}");
        Settings::default()
    });
    let proxy = match settings.proxy.resolve() {
        Ok(p) => {
            match &p {
                ProxySpec::None => tracing::info!("代理：不使用（显式 no_proxy）"),
                ProxySpec::Auto => tracing::info!(
                    "代理：自动检测（环境变量 {:?}）",
                    Settings::proxy_env().keys().collect::<Vec<_>>()
                ),
                ProxySpec::Manual { url, auth } => tracing::info!(
                    "代理：手动 {url}（认证 {}）",
                    if auth.is_some() { "开" } else { "关" }
                ),
            }
            p
        }
        Err(e) => {
            tracing::warn!("代理设置无效（连接时会失败）: {e}");
            ProxySpec::Auto
        }
    };
    // ★ M8.4：所有 HTTP 客户端都按设置里的代理构造
    let client = Client::new_with_proxy(&link, Some(&settings.proxy))?;
    // ★ M2c：游标 + baseline 落在 <data>/sync/<host>/（不同 NAS 互不污染）
    let sync_dir = qxync_core::sync::sync_state_dir(&paths.data_dir, &link.host);
    let sync_store = SyncState::load(&sync_dir)
        .with_context(|| format!("读取同步状态失败: {}", sync_dir.display()))?;
    tracing::info!(
        "同步状态: {}（游标 notify={} config={} global={}，baseline {} 项）",
        sync_dir.display(),
        sync_store.cursors.notify,
        sync_store.cursors.config,
        sync_store.cursors.global_notify,
        sync_store.baseline.len()
    );
    // ★ M5：pin 以前只在内存里 —— daemon 一重启就全丢，M3 的脱水安全检查
    //   （pinned/excluded 不脱水）会静默失守。现在从状态库加载、写穿回去。
    let pins_seed: HashMap<String, String> = sync_store
        .store
        .pins()
        .unwrap_or_default()
        .into_iter()
        .collect();
    if !pins_seed.is_empty() {
        tracing::info!("从状态库恢复 {} 条 pin", pins_seed.len());
    }
    let sync_interval: u64 = std::env::var("QXNYC_POLL_INTERVAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(qxync_core::sync::DEFAULT_POLL_INTERVAL_SECS);
    // ★ M7：选择性同步规则 —— 坏规则不静默，启动日志里点名
    let parsed_rules = link.rules();
    if !parsed_rules.rules.is_empty() {
        tracing::info!(
            "选择性同步：{} 条排除规则（临时文件过滤 {}）",
            parsed_rules.rules.len(),
            if parsed_rules.rules.filter_temp() {
                "开"
            } else {
                "关"
            }
        );
    }
    for bad in &parsed_rules.bad {
        tracing::warn!("排除规则无法解析，已忽略: {bad:?}");
    }
    let state = Arc::new(State {
        version: env!("CARGO_PKG_VERSION"),
        started: Instant::now(),
        socket: opts.socket.clone(),
        link,
        client: Mutex::new(client),
        session: Mutex::new(None),
        pins: Arc::new(StdMutex::new(pins_seed)),
        mounts: StdMutex::new(HashMap::new()),
        engine_client: Mutex::new(None),
        sid_refresher: StdMutex::new(None),
        sync_store: StdMutex::new(sync_store),
        sync_stats: Arc::new(SyncStats::default()),
        sync_cfg: StdMutex::new(SyncConfig::default()),
        sync_interval: StdMutex::new(sync_interval),
        dehydrate_cfg: StdMutex::new(DehydrateCfg::from_env()),
        dehydrate_stats: Arc::new(DehydrateStats::default()),
        rules: Arc::new(parsed_rules.rules),
        rules_bad: parsed_rules.bad,
        peers: Arc::new(StdMutex::new(Vec::new())),
        peer: StdMutex::new(None),
        peer_wake: Arc::new(Notify::new()),
        journal_buf: StdMutex::new(Vec::new()),
        settings: StdMutex::new(settings),
        proxy: StdMutex::new(proxy),
        auto_free_last: Arc::new(AtomicU64::new(0)),
        sync_epoch: SyncEpoch::new(),
    });

    // ★ M7：LAN 对等主机（`link.peer_listen` 配了才真正监听；失败只告警不致命 ——
    //   同步主路径永远不依赖 LAN）
    match PeerHost::start(
        paths.clone(),
        &state.link,
        &state,
        state.peers.clone(),
        state.peer_wake.clone(),
    )
    .await
    {
        Ok(host) => {
            tracing::info!(
                "对等主机就绪：身份={} 监听={:?} 已配对={}",
                host.name,
                host.listen,
                host.peer_list().len()
            );
            *state.peer.lock().unwrap() = Some(host);
        }
        Err(e) => tracing::warn!("LAN 对等服务启动失败（不影响 NAS 同步）: {e}"),
    }

    tracing::info!(
        "qxyncd 启动 pid={} socket={} link={}",
        std::process::id(),
        opts.socket.display(),
        state.link.id
    );
    if opts.auto_login {
        match login_internal(&state, None, None).await {
            Ok(s) => tracing::info!("自动登录成功 user={} sid={}", s.username, mask_sid(&s.sid)),
            Err(e) => tracing::warn!("自动登录失败（可稍后 `qxync login`）: {}", e.message),
        }
    }

    // ★ M10：会话热更新 —— 挂载点遇到鉴权失败时同步要新 sid（经纪人）+ 定时保活。
    //   经纪人**不进代次机制**：它是挂载点在 FUSE 线程里同步要 sid 的通道，
    //   而 SIGHUP 的前提就是绝不碰 FUSE（重载它只会让在途的水合失败）。
    {
        let refresher = spawn_session_broker(&state);
        *state.sid_refresher.lock().unwrap() = Some(refresher);
    }

    // ★ M15/T7：僵尸挂载清理。**必须在 `restore_mounts_on_start` 之前** ——
    //   `kill -9` / OOM 之后挂载点留在 `/proc/self/mounts` 里但已 ENOTCONN，
    //   不先清掉，恢复会一直撞「路径已挂载」而失败。
    if opts.restore_tasks {
        cleanup_zombie_mounts();
    }

    // ★ M8.2 / ★ M15/T5：恢复挂载（**默认开启**，见 Options::restore_tasks 的说明）
    if opts.restore_tasks {
        let st = state.clone();
        let lid = opts.link_id.clone();
        tokio::spawn(async move {
            // 给 socket/会话一点时间就绪：恢复要挂载，需要已登录
            for _ in 0..40 {
                if st.client.lock().await.sid().is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            restore_mounts_on_start(&st, &lid).await;
        });
    }

    // ★ M15/T6：同步引擎整套在这里起（poller / 脱水 / 自动释放空间 / journal /
    //   会话保活）。收在一处是为了 SIGHUP 能**整套**重启 —— 少起一个就是
    //   「重载之后那个功能悄悄不工作了」，而这种故障极难排查。
    let mut engine = start_sync_engine(&state);

    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
    let mut sigterm = terminate_signal();
    let mut sighup = hangup_signal();
    // ★ M15/T6：break 时带上**为什么**。三种来源语义本来就不同（见 [`ExitReason`]），
    // 循环之后才决定「真正卸载」还是「保留挂载转守护」。
    let reason = loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                tracing::info!("收到 shutdown 请求（IPC）");
                break ExitReason::IpcShutdown;
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("收到 SIGINT");
                break ExitReason::CtrlC;
            }
            _ = wait_sigterm(&mut sigterm) => {
                tracing::info!("收到 SIGTERM");
                break ExitReason::Sigterm;
            }
            _ = wait_sighup(&mut sighup) => {
                // ★ M15/T6：重载同步但**一个 FUSE 会话都不碰** → 挂载点零中断。
                //   这是 systemd 下真正能用的「重启同步」路径（`systemctl reload`）。
                tracing::info!("收到 SIGHUP：重载同步引擎（FUSE 挂载点不中断）");
                engine = reload_sync_engine(&state, engine).await;
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let st = state.clone();
                        let tx = shutdown_tx.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_conn(st, stream, tx).await {
                                tracing::warn!("连接结束: {e}");
                            }
                        });
                    }
                    Err(e) => tracing::warn!("accept 失败: {e}"),
                }
            }
        }
    };

    if reason.should_unmount() {
        // 用户明确要「关掉」（IPC stop / Ctrl-C）→ 真正卸载全部挂载再退。
        let n = shutdown_all_mounts(&state).await;
        let _ = std::fs::remove_file(&opts.socket);
        let _ = std::fs::remove_file(&pid_path);
        tracing::info!("qxyncd 退出（{}，已卸载 {n} 个挂载点）", reason.label());
        return Ok(());
    }

    // ★ M15/T6：SIGTERM → **保留挂载**，停同步，进程转「挂载守护」。
    let why = hold_mounts(
        &state,
        &listener,
        &mut shutdown_rx,
        &shutdown_tx,
        &mut sigterm,
        &mut sighup,
        opts.mount_hold_secs,
        engine,
    )
    .await;
    // 守护结束（IPC stop / 第二次 SIGTERM / 挂载表空 / 超时）→ 这次真卸载。
    let n = shutdown_all_mounts(&state).await;
    let _ = std::fs::remove_file(&opts.socket);
    let _ = std::fs::remove_file(&pid_path);
    tracing::info!(
        "qxyncd 退出（SIGTERM 后转入挂载守护；守护结束于「{why}」；已卸载 {n} 个挂载点）"
    );
    Ok(())
}

/// ★ M15/T6：启动整套同步引擎，返回各任务的 join 句柄。
///
/// 刻意**不含**会话经纪人（[`spawn_session_broker`]）—— 它服务的是 FUSE 线程，
/// 而重载的前提就是不动 FUSE。
fn start_sync_engine(state: &Arc<State>) -> Vec<tokio::task::JoinHandle<()>> {
    let gen = state.sync_epoch.gen();
    vec![
        // ★ M2c：后台轮询（三游标 + baseline 对账）；QXNYC_POLL_INTERVAL=0 可暂停
        spawn_poller(state.clone(), gen.clone()),
        // ★ M3：后台脱水（闲置 + 缓存限额）；QXNYC_DEHYDRATE_IDLE / QXNYC_CACHE_LIMIT 开启
        spawn_dehydrator(state.clone(), gen.clone()),
        // ★ M8.4：自动释放空间（设置里的 `free_space`；默认关 → 不改变 M8.3 行为）
        spawn_auto_free(state.clone(), gen.clone()),
        // ★ M8.3：journal 后台落库（批量 + 轮转）
        spawn_journal_flusher(state.clone(), gen.clone()),
        // ★ M10：会话保活（sid 失效就重登并把新 sid 推给挂载点）
        spawn_session_keeper(state.clone(), gen),
    ]
}

/// ★ M15/T6：SIGHUP —— 重启同步引擎，**绝不碰 FUSE 会话**（挂载点零中断）。
///
/// 两步，顺序不能反：先 [`SyncEpoch::bump`] 让旧一代在**做完当前一轮之后**退出，
/// 等它们真的退了再拉新的。
///
/// ★ 为什么不直接 `JoinHandle::abort()`：poller 那一轮 `run_sync_once` 正在改
/// baseline、推冲突决策，从中间劈开等于让同一处改动被判成两次冲突 ——
/// 那是在制造数据正确性问题，比「重载慢几秒」严重得多。
async fn reload_sync_engine(
    state: &Arc<State>,
    old: Vec<tokio::task::JoinHandle<()>>,
) -> Vec<tokio::task::JoinHandle<()>> {
    stop_sync_engine(state, old).await;
    // 顺带重登：会话过期正是用户想「重启同步」的常见起因。失败不阻塞 ——
    // poller 的懒登录会自己再试（`spawn_poller` 里那句 `login_internal`）。
    if state.client.lock().await.sid().is_none() {
        match login_internal(state, None, None).await {
            Ok(s) => tracing::info!("SIGHUP 重载：重新登录成功 user={}", s.username),
            Err(e) => tracing::warn!(
                "SIGHUP 重载：重新登录失败（poller 会懒登录重试）: {}",
                e.message
            ),
        }
    }
    let handles = start_sync_engine(state);
    tracing::info!("SIGHUP 重载完成：同步引擎已重启，FUSE 挂载点未中断");
    handles
}

/// ★ M15/T6：停掉同步引擎的全部后台任务（等它们做完当前一轮再退）。
///
/// SIGHUP 重载与 SIGTERM 转挂载守护共用这前半段，区别只在**之后要不要再拉起来**。
///
/// 等不到就放弃等待（只 WARN）：一个卡在网络 IO 上的任务不该把整个重载/退出
/// 拖住 —— 进程都要被 SIGKILL 了，多等那几秒毫无意义。
async fn stop_sync_engine(state: &Arc<State>, old: Vec<tokio::task::JoinHandle<()>>) {
    state.sync_epoch.bump();
    for h in old {
        if tokio::time::timeout(Duration::from_secs(5), h)
            .await
            .is_err()
        {
            tracing::warn!("同步引擎：有旧任务没在 5s 内退出（继续，不阻塞）");
        }
    }
}

/// ★ M15/T6：SIGTERM 之后的「挂载守护」阶段 —— 同步停了，挂载点留着。
///
/// ## 状态与能做什么
///
/// 同步引擎全部退出、`mounts.json` 已写盘、**没有调用 fusermount** ——
/// 内核里的 FUSE 连接还在，本进程继续应答它。按任务书附录 A.3 的矩阵，这意味着：
///
/// | 能力 | 守护期间 |
/// | --- | --- |
/// | `ls` / `readdir` / `ls -l` / `getfattr` | 正常（吃内存快照，零网络） |
/// | 打开**已下载**的文件读内容 | 正常（FUSE 回调在本进程里） |
/// | 打开**未下载**的文件 | 失败（引擎停了，不再去 NAS 水合） |
/// | 写文件 | 进上传队列，但 worker 已停 → 队列留着，下次启动再推 |
///
/// ## ★ 这条路的边界（不要粉饰）
///
/// **它救不了 `systemctl restart`。** systemd 的 restart 是「stop 旧进程 →
/// 等它退出 → start 新进程」，一个永不退出的守护进程会把 stop 阶段卡死，
/// 最终被 `TimeoutStopSec` 之后的 SIGKILL 杀掉 —— 挂载一样断。
/// 所以本模式**不打算、也不试图**活过 systemd 的 stop 阶段。
///
/// systemd 下真正能做到「挂载点一秒不断」的是 **SIGHUP**（`systemctl reload`）：
/// 它不终止进程，也就没有「等旧进程退出」这一步。`systemctl restart` 的体验
/// 由 T5 兜底：短暂断开后启动自动重挂。
///
/// ## 退出条件（守护进程绝不能变成「收不干净的东西」）
///
/// | 条件 | 语义 |
/// | --- | --- |
/// | IPC `Shutdown`（`qxync daemon stop`） | 用户明确要关掉 → 真卸载 |
/// | 第二次 SIGTERM / SIGINT | 「我说了停」→ 真卸载 |
/// | 挂载表变空 | 没什么可守护的 → 直接退 |
/// | `--mount-hold-secs` 超时（默认 0 = 无限） | 兜底，防「忘了它还活着」 |
/// | SIGHUP | 不退出 —— **恢复同步引擎**（挂载点本来就在） |
///
/// 返回一句人类可读的「为什么结束」，进日志。
#[allow(clippy::too_many_arguments)]
async fn hold_mounts(
    state: &Arc<State>,
    listener: &UnixListener,
    shutdown_rx: &mut mpsc::Receiver<()>,
    shutdown_tx: &mpsc::Sender<()>,
    sigterm: &mut Option<tokio::signal::unix::Signal>,
    sighup: &mut Option<tokio::signal::unix::Signal>,
    hold_secs: u64,
    engine: Vec<tokio::task::JoinHandle<()>>,
) -> &'static str {
    // 1) 停同步（poller / 脱水 / 自动释放空间 / journal 落库 / 会话保活）
    stop_sync_engine(state, engine).await;
    // 2) 挂载记录写盘。语义是「上一次实际挂了什么」—— 挂载还挂着，记录就该留着；
    //    ★ 这里绝不能去动 mounts.json 的内容，T5 的自动重挂全靠它。
    mounts_persist(&state.mounts);
    let n = state.mounts.lock().unwrap().len();
    if n == 0 {
        tracing::info!("SIGTERM：挂载表本来就是空的，无需挂载守护");
        return "挂载表为空";
    }
    tracing::info!(
        "SIGTERM：进入挂载守护模式 —— 同步已停，{n} 个挂载点保留（目录列表与已下载文件仍可读）。\
         注意：`systemctl restart` 会卡在 stop 阶段直到 TimeoutStopSec 后 SIGKILL，\
         那种场景请用 `systemctl reload qxyncd`（SIGHUP，挂载点零中断）。\
         结束守护：`qxync daemon stop`、再发一次 SIGTERM，或设 --mount-hold-secs"
    );

    let mut engine: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                tracing::info!("挂载守护：收到 shutdown 请求（IPC）");
                return "IPC shutdown（qxync daemon stop）";
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("挂载守护：收到 SIGINT");
                return "SIGINT";
            }
            _ = wait_sigterm(sigterm) => {
                tracing::info!("挂载守护：再次收到 SIGTERM，结束守护并卸载挂载");
                return "第二次 SIGTERM";
            }
            _ = wait_sighup(sighup) => {
                // ★ 守护中收到 SIGHUP：挂载点本来就在，把同步加回去即可
                //   （于是 `systemctl reload` 在守护态下也是有意义的）。
                tracing::info!("挂载守护：收到 SIGHUP，恢复同步引擎");
                engine = reload_sync_engine(state, std::mem::take(&mut engine)).await;
            }
            _ = tick.tick() => {
                if state.mounts.lock().unwrap().is_empty() {
                    tracing::info!("挂载守护：挂载表已空（挂载点被用户卸载了），结束守护");
                    return "挂载表变空";
                }
            }
            _ = hold_deadline(hold_secs) => {
                tracing::info!("挂载守护：达到 --mount-hold-secs={hold_secs} 上限，结束守护");
                return "超过 --mount-hold-secs 上限";
            }
            accepted = listener.accept() => {
                // ★ 守护期间 IPC 仍要服务：`qxync daemon stop` 正是靠它结束守护的，
                //   `status` 也要能问（否则用户看到「进程活着但 status 连不上」）。
                //   复用主循环那个 sender：谁在 `recv` 由当前阶段决定，语义一致。
                match accepted {
                    Ok((stream, _)) => {
                        let st = state.clone();
                        let tx = shutdown_tx.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_conn(st, stream, tx).await {
                                tracing::warn!("连接结束（挂载守护）: {e}");
                            }
                        });
                    }
                    Err(e) => tracing::warn!("accept 失败（挂载守护）: {e}"),
                }
            }
        }
    }
}

/// 守护模式的超时上限。`0` = 永不触发，于是这个分支永远不完成。
async fn hold_deadline(hold_secs: u64) {
    if hold_secs == 0 {
        std::future::pending::<()>().await
    } else {
        tokio::time::sleep(Duration::from_secs(hold_secs)).await
    }
}

/// CLI 用来判断「socket 在但进程已死」。
async fn ping_socket(path: &Path) -> Result<()> {
    let stream = UnixStream::connect(path).await?;
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();
    let line = String::from_utf8(encode_line(&RequestEnvelope::new(Request::Ping))?)?;
    wr.write_all(line.as_bytes()).await?;
    let reply = tokio::time::timeout(Duration::from_secs(3), lines.next_line())
        .await
        .context("ping 超时")??
        .context("对端关闭")?;
    let resp: Response = decode_line(reply.as_bytes())?;
    if resp.ok {
        Ok(())
    } else {
        bail!("ping 返回错误")
    }
}

// ---------------------------------------------------------------- 连接处理

async fn handle_conn(
    state: Arc<State>,
    stream: UnixStream,
    shutdown: mpsc::Sender<()>,
) -> Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let resp = match decode_line::<RequestEnvelope>(line.as_bytes()) {
            Ok(env) if env.v != IPC_VERSION => Response::err(
                ErrorKind::BadVersion,
                format!("协议版本 {} 不受支持（本进程支持 {IPC_VERSION}）", env.v),
            ),
            Ok(env) => dispatch(&state, env.req, &shutdown).await,
            Err(e) => Response::err(ErrorKind::BadRequest, format!("请求解析失败: {e}")),
        };
        wr.write_all(&encode_line(&resp)?).await?;
    }
    Ok(())
}

async fn dispatch(state: &Arc<State>, req: Request, shutdown: &mpsc::Sender<()>) -> Response {
    tracing::debug!("← {}", req.method());
    let out: Result<serde_json::Value, IpcError> = match req {
        Request::Ping => to_value(PingData {
            pong: true,
            daemon_version: state.version.to_string(),
            pid: std::process::id(),
            uptime_secs: state.started.elapsed().as_secs(),
        }),
        Request::Status => status(state).await,
        Request::Login { user, password } => {
            login_internal(state, user, password).await.and_then(|s| {
                to_value(LoginData {
                    sid_masked: mask_sid(&s.sid),
                    user: s.username.clone(),
                    uid: s.uid.clone(),
                })
            })
        }
        Request::Logout => logout(state).await,
        Request::Ls { path } => ls(state, path).await,
        Request::Stat { dir, name } => stat(state, dir, name).await,
        Request::Get { dir, name, dest } => get(state, dir, name, dest).await,
        Request::Put { local, dest, name } => put(state, local, dest, name).await,
        Request::Mkdir { parent, name } => mkdir(state, parent, name).await,
        Request::Pin { path, state: pin } => pin_cmd(state, path, pin),
        Request::Mount {
            mountpoint,
            remote,
            cache_dir,
            threads,
            auto_unmount,
            hydrate_timeout_secs,
            read_write,
            delete_limit,
            cache_mode,
            task,
            save_task,
            conflict,
        } => {
            // ★ M8.2：只有显式要求（`task` 或 `save_task`）才登记任务；
            //   默认不登记 → qxync CLI / 验收矩阵的行为与 M7 **一字不变**。
            let want_task = save_task.unwrap_or(false) || task.is_some();
            let reg = if want_task {
                // ★ 缓存目录也要落进任务登记，否则「任务方式挂载」改不了缓存位置
                Some(
                    Task::from_mount(
                        task.clone(),
                        mountpoint.clone(),
                        remote
                            .clone()
                            .filter(|r| !r.trim().is_empty())
                            .map(|r| qxync_core::normalize_root(&r)),
                        read_write.unwrap_or(false),
                        cache_mode.clone(),
                        threads,
                        hydrate_timeout_secs,
                        delete_limit,
                        auto_unmount,
                    )
                    .with_cache_dir(cache_dir.clone())
                    // ★ M8.4：登记任务时把冲突策略一起落盘
                    .with_conflict(conflict.clone()),
                )
            } else {
                None
            };
            let out = mount(
                state,
                mountpoint,
                remote,
                cache_dir,
                threads.unwrap_or(4),
                auto_unmount.unwrap_or(false),
                Duration::from_secs(hydrate_timeout_secs.unwrap_or(60)),
                read_write.unwrap_or(false),
                delete_limit,
                cache_mode,
                conflict.clone(),
            )
            .await;
            // 挂载成功后才落盘登记：失败不该留下一条「指向不存在挂载」的任务
            if out.is_ok() {
                if let Some(mut t) = reg {
                    match ConfigPaths::discover() {
                        Ok(paths) => match t.save(&paths) {
                            Ok(p) => tracing::info!("M8.2 任务已登记: {} → {}", t.id, p.display()),
                            Err(e) => tracing::warn!("登记任务失败（挂载仍然有效）：{e}"),
                        },
                        Err(e) => tracing::warn!("登记任务失败（读配置目录）：{e}"),
                    }
                }
            }
            out
        }
        Request::Umount { mountpoint } => {
            let mp = mountpoint.clone();
            let out = umount(state, mountpoint).await;
            // 卸载成功 → 把对应任务标成停用（**只改登记，不动数据**）
            if out.is_ok() {
                if let Err(e) = disable_task_by_mountpoint(&mp) {
                    tracing::warn!("更新任务登记失败（卸载已完成）：{e}");
                }
            }
            out
        }
        Request::Mounts => mounts(state),
        Request::Roots => roots_cmd(state).await,
        Request::Rules { match_path } => rules_cmd(state, match_path),
        Request::Tasks { action, id, task } => tasks_cmd(state, &action, id, task).await,
        Request::Journal {
            limit,
            since,
            query,
            level,
            clear,
        } => journal_cmd(state, limit, since, query, level, clear),
        Request::Settings => settings_cmd(state),
        Request::SettingsSave {
            settings,
            autostart_exe,
        } => settings_save_cmd(state, settings, autostart_exe),
        Request::Decisions {
            action,
            id,
            resolution,
        } => decisions_cmd(state, &action, id, resolution),
        Request::FileStates { path } => file_states_cmd(state, path).await,
        Request::Space { now } => space_cmd(state, now.unwrap_or(false)).await,
        Request::Peer {
            action,
            addr,
            code,
            name,
            path,
            dest,
            limit,
        } => peer_cmd(state, action, addr, code, name, path, dest, limit).await,
        Request::Sync {
            once,
            force_deletes,
            max_deletes,
            interval_secs,
        } => {
            sync_cmd(
                state,
                once.unwrap_or(false),
                force_deletes.unwrap_or(false),
                max_deletes,
                interval_secs,
            )
            .await
        }
        Request::Rm { dir, name } => rm(state, dir, name).await,
        Request::Store { integrity } => store_info(state, integrity.unwrap_or(false)),
        Request::Dehydrate {
            path,
            all,
            idle_secs,
            cache_limit,
            force,
            dry_run,
            mountpoint,
        } => {
            dehydrate_cmd(
                state,
                DehydrateOpts {
                    path,
                    all: all.unwrap_or(false),
                    idle_secs,
                    cache_limit,
                    force: force.unwrap_or(false),
                    dry_run: dry_run.unwrap_or(false),
                    mountpoint,
                },
            )
            .await
        }
        Request::Shutdown => {
            // 先回响应，再触发退出，避免对端拿不到回包
            let tx = shutdown.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(150)).await;
                let _ = tx.send(()).await;
            });
            to_value(ShutdownData { unmounted: 0 })
        }
    };
    match out {
        Ok(v) => Response::ok(v),
        Err(e) => {
            tracing::debug!("→ 错误({:?}): {}", e.kind, e.message);
            Response::err(e.kind, e.message)
        }
    }
}

fn to_value<T: serde::Serialize>(v: T) -> Result<serde_json::Value, IpcError> {
    serde_json::to_value(v).map_err(|e| IpcError::new(ErrorKind::Parse, e.to_string()))
}

fn map_err(e: CoreError) -> IpcError {
    // ★ M10：先按「是不是会话失效」判定（Auth，或服务端回 4/5 号 status）——
    //   调用方靠这个 kind 决定要不要重登重试。
    let kind = if e.is_auth() {
        ErrorKind::Auth
    } else {
        match &e {
            CoreError::Auth(_) => ErrorKind::Auth,
            CoreError::Transport(_) => ErrorKind::Transport,
            CoreError::Status { .. } => ErrorKind::Status,
            CoreError::Parse(_) => ErrorKind::Parse,
            CoreError::Io(_) => ErrorKind::Io,
            CoreError::Db(_) => ErrorKind::Io,
            CoreError::Unsupported(_) => ErrorKind::Unsupported,
        }
    };
    IpcError::new(kind, e.to_string())
}

/// 会话失效的两种表现：`Auth`，或 `get_list`/`stat` 回 status 4/5（判定在 core，三层共用）。
fn is_auth_error(e: &CoreError) -> bool {
    e.is_auth()
}

// ---------------------------------------------------------------- 会话

async fn current_session(state: &State) -> Option<Session> {
    state.session.lock().await.clone()
}

/// 懒登录：设计 §4 —— daemon 启动不强制登录，**首次需要时**用凭据文件登录。
async fn ensure_session(state: &Arc<State>) -> Result<(), IpcError> {
    if state.client.lock().await.sid().is_some() {
        return Ok(());
    }
    tracing::info!("首次请求需要会话，尝试用凭据登录");
    login_internal(state, None, None).await.map(|_| ())
}

async fn require_sid(state: &State) -> Result<String, IpcError> {
    let guard = state.client.lock().await;
    guard.sid().map(|s| s.to_string()).ok_or_else(|| {
        IpcError::new(
            ErrorKind::NotLoggedIn,
            "尚未登录（先 `qxync --via-daemon login`）",
        )
    })
}

/// 登录：参数 → 凭据文件 → 报错。成功则写回凭据（0600）。
async fn login_internal(
    state: &Arc<State>,
    user: Option<String>,
    password: Option<String>,
) -> Result<Session, IpcError> {
    let paths = ConfigPaths::discover().map_err(|e| IpcError::new(ErrorKind::Io, e.to_string()))?;
    let creds = Credentials::load(&paths).ok();
    let user = user
        .or_else(|| creds.as_ref().map(|c| c.user.clone()))
        .unwrap_or_else(|| state.link.user.clone());
    let password = password
        .or_else(|| creds.as_ref().map(|c| c.password.clone()))
        .ok_or_else(|| {
            IpcError::new(
                ErrorKind::Auth,
                "没有口令：用 `qxync login --password <口令>` 或先在 CLI 登录一次",
            )
        })?;

    let session = {
        let mut client = state.client.lock().await;
        client.login(&user, &password).await.map_err(map_err)?
    };
    if let Some(reason) = session.busy_reason() {
        tracing::warn!("NAS 状态：{reason}（同步应暂停）");
    }
    *state.session.lock().await = Some(session.clone());
    // ★ M10：把新 sid **热推**给所有挂载点。挂载点各自持有自己的 `Client`，
    //   挂载时拷的是当时那个 sid；不推的话它会一直 EIO 到重新挂载。
    push_sid_to_mounts(state, &session.sid, &session.username);
    // 凭据回写（如果这次是从命令行传进来的）
    let _ = Credentials {
        host: state.link.host.clone(),
        user: user.clone(),
        password,
    }
    .save(&paths);
    tracing::info!(
        "登录成功 user={} sid={}",
        session.username,
        mask_sid(&session.sid)
    );
    Ok(session)
}

async fn logout(state: &Arc<State>) -> Result<serde_json::Value, IpcError> {
    {
        let mut client = state.client.lock().await;
        let _ = client.logout().await;
    }
    *state.session.lock().await = None;
    // ★ M10：挂载点的 sid 一并作废（下一次访问会按凭据重登，与 IPC 的懒登录一致）
    for c in mount_clients(state) {
        c.clear_sid();
    }
    to_value(serde_json::json!({}))
}

// ---------------------------------------------------------------- ★ M10 会话热更新

/// 当前所有挂载点自己的 NAS 客户端。
fn mount_clients(state: &Arc<State>) -> Vec<Arc<Client>> {
    let g = state.mounts.lock().unwrap();
    g.values().map(|m| m.client.clone()).collect()
}

/// 把 sid 推给「同一个账号」的挂载点（sid 已经一致的跳过）。返回真正改了几个。
///
/// 换账号（同一 link 不同 user）的挂载点**不推**：它的节点表/缓存还是上一个账号的树，
/// 推过去会张冠李戴 —— 只告警并让用户重新挂载。
fn push_sid_to_mounts(state: &Arc<State>, sid: &str, user: &str) -> usize {
    let (clients, other) = {
        let g = state.mounts.lock().unwrap();
        sid_push_targets(g.values().map(|m| (m.user.clone(), m.client.clone())), user)
    };
    if other > 0 {
        tracing::warn!(
            "账号已切换（当前 {user}）：{other} 个挂载点属于旧账号，未推新 sid —— 请重新挂载"
        );
    }
    let n = push_sid_to(sid, clients.into_iter());
    if n > 0 {
        tracing::info!("会话已热更新到 {n} 个挂载点（sid={}）", mask_sid(sid));
    }
    n
}

/// 该把新 sid 推给哪些挂载点：只推**同账号**的（`user` 为空 = 挂载时还没会话，按同账号处理）。
///
/// 返回 `(要推的客户端, 因换账号跳过的挂载点数)`。
fn sid_push_targets(
    mounts: impl Iterator<Item = (String, Arc<Client>)>,
    user: &str,
) -> (Vec<Arc<Client>>, usize) {
    let mut same = Vec::new();
    let mut other = 0usize;
    for (muser, client) in mounts {
        if muser.is_empty() || muser == user {
            same.push(client);
        } else {
            other += 1;
        }
    }
    (same, other)
}

/// 纯函数版本（可单测）：只给 sid 还不一样的客户端推。
fn push_sid_to(sid: &str, clients: impl Iterator<Item = Arc<Client>>) -> usize {
    let mut n = 0;
    for c in clients {
        if c.sid().as_deref() != Some(sid) {
            c.set_sid(sid);
            n += 1;
        }
    }
    n
}

/// ★ M10：会话经纪人 —— 挂载点在 FUSE 线程里**同步**要一个新 sid。
///
/// 同步侧通过 `std::sync::mpsc` 发请求并等回复；异步侧用 daemon 的 runtime 串行重登，
/// 避免多个挂载点同时打登录接口（真机上并发登录会被 NAS 风控）。重登成功后
/// `login_internal` 已经负责把 sid 推给所有挂载点。
fn spawn_session_broker(state: &Arc<State>) -> qxync_fuse::SidRefresher {
    let (tx, mut rx) = mpsc::unbounded_channel::<std::sync::mpsc::SyncSender<Option<String>>>();
    let st = state.clone();
    tokio::spawn(async move {
        let mut last_ok: Option<Instant> = None;
        while let Some(reply) = rx.recv().await {
            // 刚登过就复用当前 sid（多个挂载点同时失效时不要再登一遍）
            if let Some(t) = last_ok {
                if t.elapsed() < SESSION_REFRESH_DEDUP {
                    let sid = st.client.lock().await.sid();
                    if sid.is_some() {
                        let _ = reply.send(sid);
                        continue;
                    }
                }
            }
            match login_internal(&st, None, None).await {
                Ok(s) => {
                    last_ok = Some(Instant::now());
                    let _ = reply.send(Some(s.sid));
                }
                Err(e) => {
                    tracing::warn!("挂载点请求重登失败: {}", e.message);
                    let _ = reply.send(None);
                }
            }
        }
    });
    Arc::new(move || {
        // 容量 1：回复那边永远不阻塞（超时丢弃也不会把 broker 卡住）
        let (rtx, rrx) = std::sync::mpsc::sync_channel::<Option<String>>(1);
        if tx.send(rtx).is_err() {
            return None;
        }
        rrx.recv_timeout(SESSION_REFRESH_TIMEOUT).ok().flatten()
    })
}

/// ★ M10：保活一轮该做什么（抽成纯函数，方便单测 —— 真机上很难稳定造出「sid 被服务端踢掉」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeeperStep {
    /// 还没登录过 —— 不主动碰 NAS（保持「懒登录」语义）
    Idle,
    /// sid 还活着，什么都不用做
    Ok,
    /// 探测说「已经失效」→ 重登一次（`login_internal` 会把新 sid 推给挂载点）
    Relogin,
    /// 探测本身失败（网络抖动）→ 这轮不动，下一轮再探（不要在断网时反复打登录接口）
    ProbeFailed,
}

fn keeper_step(has_sid: bool, probe: &std::result::Result<bool, CoreError>) -> KeeperStep {
    if !has_sid {
        return KeeperStep::Idle;
    }
    match probe {
        Ok(true) => KeeperStep::Ok,
        Ok(false) => KeeperStep::Relogin,
        Err(_) => KeeperStep::ProbeFailed,
    }
}

/// ★ M10：会话保活 —— 定期探一次；sid 失效就重登，并把新 sid 推给所有挂载点。
///
/// 以前只有 IPC 命令会重登（`with_client!`），挂载点与同步引擎都不会：
/// sid 一过期，挂载点 `ls` 直接 EIO、轮询一直报错，直到用户手动重挂。
///
/// ★ M15/T6：带一代 [`EngineGen`] —— 换代时退（SIGHUP 会重起一个新的）。
fn spawn_session_keeper(state: Arc<State>, mut gen: EngineGen) -> tokio::task::JoinHandle<()> {
    let secs = std::env::var("QXNYC_SESSION_KEEPALIVE")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_KEEPALIVE_SECS);
    if secs == 0 {
        tracing::info!("会话保活已禁用（QXNYC_SESSION_KEEPALIVE=0）");
        return tokio::spawn(async {});
    }
    tracing::info!("会话保活已启动：每 {secs}s 探一次（QXNYC_SESSION_KEEPALIVE 可调）");
    tokio::spawn(async move {
        loop {
            if !gen.sleep(Duration::from_secs(secs.max(1))).await {
                tracing::info!("会话保活退出（同步引擎已换代）");
                return;
            }
            let (has_sid, probe) = {
                let c = state.client.lock().await;
                let has = c.sid().is_some();
                let probe = if has {
                    c.check_alive().await
                } else {
                    Ok(false)
                };
                (has, probe)
            };
            match keeper_step(has_sid, &probe) {
                KeeperStep::Idle | KeeperStep::Ok => {}
                KeeperStep::ProbeFailed => {
                    tracing::debug!("会话保活探测失败（{probe:?}），下一轮再试");
                }
                KeeperStep::Relogin => {
                    tracing::warn!("会话保活：sid 已失效（{probe:?}），重登并推给挂载点");
                    if let Err(e) = login_internal(&state, None, None).await {
                        tracing::warn!("会话保活重登失败: {}", e.message);
                    }
                }
            }
        }
    })
}

/// 拿客户端锁执行；遇会话失效自动重登重试一次。
macro_rules! with_client {
    ($state:expr, |$c:ident| $body:expr) => {{
        let first = {
            let guard = $state.client.lock().await;
            let $c = &*guard;
            $body.await
        };
        match first {
            Ok(v) => Ok(v),
            Err(e) if is_auth_error(&e) => {
                tracing::warn!("会话失效（{e}），重登后重试");
                login_internal($state, None, None).await?;
                let guard = $state.client.lock().await;
                let $c = &*guard;
                $body.await.map_err(map_err)
            }
            Err(e) => Err(map_err(e)),
        }
    }};
}

// ---------------------------------------------------------------- 各方法

async fn status(state: &Arc<State>) -> Result<serde_json::Value, IpcError> {
    let session = current_session(state).await;
    let logged_in = session.is_some();

    // 只读快照：登录了才去问 NAS，失败也不报错（status 不该失败）
    let (server, cursors, alive) = if logged_in {
        let nas = with_client!(state, |c| c.nas_uid()).ok();
        let max_log = with_client!(state, |c| c.max_log()).ok();
        let alive = with_client!(state, |c| c.check_alive()).unwrap_or(false);
        (
            nas.map(|n| ServerInfo {
                qsync_version: n.qsync_version.clone(),
                qpkg_version: n.qpkg_version.clone(),
                build: n.build.clone(),
                qbox_cgi: n.qbox_cgi,
                fcgi: n.fcgi,
                busy_reason: n.busy_reason().map(|s| s.to_string()),
            }),
            max_log.map(|m| CursorInfo {
                max_log: m.max_log,
                global_notify: m.global_notify,
                sync_signal: m.sync_signal,
            }),
            alive,
        )
    } else {
        (None, None, false)
    };

    let (mounts, hydro, uploads, deletes, transfers) = snapshot_mounts(state);
    to_value(StatusData {
        daemon: DaemonInfo {
            version: state.version.to_string(),
            pid: std::process::id(),
            uptime_secs: state.started.elapsed().as_secs(),
            socket: state.socket.display().to_string(),
        },
        link: Some(LinkInfo {
            id: state.link.id.clone(),
            host: state.link.host.clone(),
            port: state.link.port,
            https: state.link.https,
            user: state.link.user.clone(),
            ipv4_only: state.link.ipv4_only,
        }),
        logged_in,
        session: session.map(|s| SessionInfo {
            sid_masked: mask_sid(&s.sid),
            alive,
        }),
        server,
        cursors,
        hydro,
        uploads,
        deletes,
        sync: Some(sync_info(state)),
        cache: Some(cache_info(state)),
        // ★ T8：传输汇总（没有在途作业时为 `None`）
        transfers,
        mounts,
    })
}

async fn ls(state: &Arc<State>, path: String) -> Result<serde_json::Value, IpcError> {
    ensure_session(state).await?;
    let entries = with_client!(state, |c| c.list(&path))?;
    to_value(LsData {
        path,
        total: entries.len(),
        entries,
    })
}

async fn stat(
    state: &Arc<State>,
    dir: String,
    name: String,
) -> Result<serde_json::Value, IpcError> {
    ensure_session(state).await?;
    let e = with_client!(state, |c| c.stat(&dir, &name))?;
    to_value(e)
}

async fn get(
    state: &Arc<State>,
    dir: String,
    name: String,
    dest: PathBuf,
) -> Result<serde_json::Value, IpcError> {
    ensure_session(state).await?;
    let expected = with_client!(state, |c| c.stat(&dir, &name))?.map(|e| e.filesize);
    let bytes = with_client!(state, |c| c.download_to_file(&dir, &name, &dest))?;
    // 铁则 1 的前置校验：长度不符绝不当作成功
    if let Some(exp) = expected {
        if exp != bytes {
            return Err(IpcError::new(
                ErrorKind::Io,
                format!("下载长度不符：服务端 {exp} 字节，实际 {bytes} 字节"),
            ));
        }
    }
    to_value(GetData { bytes, dest })
}

async fn put(
    state: &Arc<State>,
    local: PathBuf,
    dest: String,
    name: Option<String>,
) -> Result<serde_json::Value, IpcError> {
    ensure_session(state).await?;
    let bytes = std::fs::read(&local)
        .map_err(|e| IpcError::new(ErrorKind::Io, format!("读取 {}: {e}", local.display())))?;
    let target = name
        .or_else(|| local.file_name().map(|s| s.to_string_lossy().into_owned()))
        .ok_or_else(|| IpcError::new(ErrorKind::BadRequest, "无法推断远端文件名"))?;
    let payload = bytes.clone();
    with_client!(state, |c| c.upload_bytes(&dest, &target, payload.clone()))?;
    let mtime = std::fs::metadata(&local)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if mtime > 0 {
        with_client!(state, |c| c.set_mtime(&dest, &target, mtime))?;
    }
    // ★ M7：事件快路径 —— 已配对的对端立刻知道这个路径变了（失败只记日志）
    if let Some(host) = state.peer.lock().unwrap().clone() {
        host.notify_async(
            format!("{dest}/{target}"),
            bytes.len() as u64,
            mtime,
            "modified",
        );
    }
    to_value(PutData {
        bytes: bytes.len() as u64,
        remote_path: format!("{dest}/{target}"),
        mtime,
    })
}

async fn mkdir(
    state: &Arc<State>,
    parent: String,
    name: String,
) -> Result<serde_json::Value, IpcError> {
    ensure_session(state).await?;
    with_client!(state, |c| c.mkdir(&parent, &name))?;
    to_value(serde_json::json!({}))
}

fn pin_cmd(
    state: &Arc<State>,
    path: String,
    pin: Option<String>,
) -> Result<serde_json::Value, IpcError> {
    const VALID: [&str; 4] = ["pinned", "unpinned", "unspecified", "excluded"];
    match pin {
        Some(pin) => {
            if !VALID.contains(&pin.as_str()) {
                return Err(IpcError::new(
                    ErrorKind::BadRequest,
                    format!("pin 只能是 {VALID:?} 之一，收到 {pin:?}"),
                ));
            }
            // M1.5 只登记；M2/M5 的 Dehydrator 与 read() 消费它。
            // ★ M5：同时写穿到状态库 —— 重启后 pin 还在，脱水安全检查不会静默失守。
            state.pins.lock().unwrap().insert(path.clone(), pin.clone());
            if let Err(e) = state.sync_store.lock().unwrap().store.set_pin(&path, &pin) {
                tracing::warn!("pin 落库失败（{path} = {pin}）: {e}");
            }
            tracing::info!("pin {path} = {pin}");
            to_value(serde_json::json!({ "path": path, "pin": pin }))
        }
        None => {
            let cur = state
                .pins
                .lock()
                .unwrap()
                .get(&path)
                .cloned()
                .unwrap_or_else(|| "unspecified".to_string());
            to_value(serde_json::json!({ "path": path, "pin": cur }))
        }
    }
}

/// ★ M5：本地状态库快照（`qxync store [--integrity]`）。
///
/// 一次性把「同步正确性的核心状态」摊开给脚本看：游标、baseline 行数、pin、未完成上传。
fn store_info(state: &Arc<State>, integrity: bool) -> Result<serde_json::Value, IpcError> {
    let g = state.sync_store.lock().unwrap();
    let ic = if integrity {
        Some(
            g.store
                .integrity_check()
                .map_err(|e| IpcError::new(ErrorKind::Io, e.to_string()))?,
        )
    } else {
        None
    };
    let store = &g.store;
    let data = StoreData {
        path: g.db_path().display().to_string(),
        schema_version: store
            .schema_version()
            .map_err(|e| IpcError::new(ErrorKind::Io, e.to_string()))?,
        integrity: ic,
        cursors: SyncCursors {
            config: g.cursors.config,
            notify: g.cursors.notify,
            global_notify: g.cursors.global_notify,
            max_log_seen: g.cursors.max_log_seen,
            log_missing_count: g.cursors.log_missing_count,
        },
        baseline_entries: store
            .baseline_len()
            .map_err(|e| IpcError::new(ErrorKind::Io, e.to_string()))?,
        pins: store
            .pins()
            .map_err(|e| IpcError::new(ErrorKind::Io, e.to_string()))?,
        uploads: store
            .uploads()
            .map_err(|e| IpcError::new(ErrorKind::Io, e.to_string()))?
            .len() as u64,
    };
    to_value(data)
}

/// `qxync roots` / GUI 的「NAS 目录」面板：**家目录根 + NAS 上登记的同步文件夹**。
///
/// 这就是一对一配对时能选的 NAS 文件夹来源。这里**不再**做「把每个根列一遍」的可读性探测
/// —— 那会真的把整个目录列出来（贵），而且一对多删掉之后也没有「多个根」要探了。
/// 选中某个文件夹之后，`ls` 会如实告诉你能不能读。
///
/// 可写性：**不做预判**。能写的判据在 NAS 侧 —— Qsync 里登记成同步文件夹的目录
/// （下面列出来的这些）可以写；没登记过的共享文件夹，服务端会拒绝上传（`status:20`）。
/// 所以客户端的做法是：用户勾读写就按读写挂，真被拒了在错误列表里如实报。
async fn roots_cmd(state: &Arc<State>) -> Result<serde_json::Value, IpcError> {
    let logged_in = current_session(state).await.is_some();
    let syncing = if logged_in {
        with_client!(state, |c| c.syncing_folders()).unwrap_or_default()
    } else {
        Vec::new()
    };
    let note = if logged_in {
        "能不能写由 NAS 决定：Qsync 里登记成同步文件夹的目录（下面这些）可以写；\
         没登记过的共享文件夹会被服务端拒绝（status 20）。客户端不替你改只读。"
            .to_string()
    } else {
        "未登录：拿不到 NAS 上登记的同步文件夹列表，只能手输或用「浏览…」".to_string()
    };
    to_value(RootsData {
        syncing_folders: syncing,
        note: Some(note),
    })
}

/// ★ M7：`qxync rules [--match PATH]` —— 选择性同步规则一览 + 单路径判定。
fn rules_cmd(
    state: &Arc<State>,
    match_path: Option<String>,
) -> Result<serde_json::Value, IpcError> {
    let rules = &state.rules;
    // 一对一：这里只有一个远端根（link 的 home_root）
    let roots = vec![qxync_core::HOME_ROOT.to_string()];
    let mut data = RulesData {
        roots: roots.clone(),
        exclude: state.link.exclude.clone(),
        patterns: rules.pattern_list(),
        bad: state.rules_bad.clone(),
        filter_temp: rules.filter_temp(),
        temp_patterns: qxync_core::rules::TEMP_PATTERNS
            .iter()
            .map(|s| s.to_string())
            .collect(),
        ..Default::default()
    };
    if let Some(path) = match_path {
        let root = qxync_core::rules::containing_root(&roots, &path).map(|s| s.to_string());
        let rel = root
            .as_deref()
            .and_then(|r| qxync_core::rules::rel_under(r, &path));
        let (hidden, reason) = match &root {
            None => (false, "outside-roots".to_string()),
            Some(r) => match rules.hides_remote(r, &path, false) {
                Some(h) => (true, h.as_str().to_string()),
                None => (false, "visible".to_string()),
            },
        };
        data.match_path = Some(path.clone());
        data.match_root = root;
        data.match_rel = rel;
        data.match_hidden = Some(hidden);
        data.match_reason = Some(reason);
    }
    to_value(data)
}

/// ★ M7：`qxync peer <action>` —— 设备配对 / 探活 / 事件 / LAN 直传自检。
#[allow(clippy::too_many_arguments)]
async fn peer_cmd(
    state: &Arc<State>,
    action: String,
    addr: Option<String>,
    code: Option<String>,
    name: Option<String>,
    path: Option<String>,
    dest: Option<PathBuf>,
    limit: Option<usize>,
) -> Result<serde_json::Value, IpcError> {
    let host = state
        .peer
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| IpcError::new(ErrorKind::Unsupported, "对等主机未初始化"))?;
    let bad = |m: String| IpcError::new(ErrorKind::BadRequest, m);
    // ★ M7：LAN 直传命中统计（各挂载求和），`peer status` 里能看到省了多少 NAS 请求
    let (lan_hits, lan_bytes) = {
        let g = state.mounts.lock().unwrap();
        let mut hits = 0u64;
        let mut bytes = 0u64;
        for m in g.values() {
            let (_, h, b, _) = m.handle.lan_stats().snapshot();
            hits += h;
            bytes += b;
        }
        (hits, bytes)
    };
    let with_lan = |d: PeerData| PeerData {
        lan_hits,
        lan_bytes,
        ..d
    };
    match action.as_str() {
        "status" => to_value(with_lan(host.status())),
        "list" => {
            let devices: Vec<qxync_core::ipc::PeerDeviceInfo> = host
                .peer_list()
                .into_iter()
                .map(|p| qxync_core::ipc::PeerDeviceInfo {
                    name: p.name,
                    addr: p.addr,
                    token_masked: mask_token(&p.token),
                })
                .collect();
            to_value(with_lan(PeerData {
                action: "list".into(),
                devices,
                ..host.status()
            }))
        }
        "pair" => {
            let addr = addr.ok_or_else(|| bad("pair 需要 addr".into()))?;
            let code = code.ok_or_else(|| bad("pair 需要 code（配对码）".into()))?;
            host.pair(&addr, &code)
                .await
                .map_err(bad)
                .and_then(to_value)
        }
        "ping" => {
            let target = addr
                .or(name)
                .ok_or_else(|| bad("ping 需要 addr 或 name".into()))?;
            host.ping(&target).await.map_err(bad).and_then(to_value)
        }
        "events" => to_value(with_lan(PeerData {
            action: "events".into(),
            events: host.events(limit.unwrap_or(50).min(200)),
            ..host.status()
        })),
        "notify" => {
            let path = path.ok_or_else(|| bad("notify 需要 path".into()))?;
            // 事件只是「某路径可能变了」的提示，不携带内容：size/mtime 由接收方自己 stat。
            let n = host
                .notify(PeerEvent::new(path.clone(), 0, 0, "modified"))
                .await;
            to_value(with_lan(PeerData {
                action: "notify".into(),
                events_out: n as u64,
                note: Some(format!("事件已送达 {n} 台对端")),
                ..host.status()
            }))
        }
        "fetch" => {
            let target = addr
                .or(name)
                .ok_or_else(|| bad("fetch 需要 addr 或 name".into()))?;
            let path = path.ok_or_else(|| bad("fetch 需要 path".into()))?;
            let dest = dest.ok_or_else(|| bad("fetch 需要 dest".into()))?;
            host.fetch(&target, &path, &dest)
                .await
                .map_err(bad)
                .and_then(to_value)
        }
        other => Err(IpcError::new(
            ErrorKind::BadRequest,
            format!(
                "未知 peer action {other:?}（可用：status/list/pair/ping/events/notify/fetch）"
            ),
        )),
    }
}

#[allow(clippy::too_many_arguments)]
async fn mount(
    state: &Arc<State>,
    mountpoint: PathBuf,
    remote: Option<String>,
    cache_dir: Option<PathBuf>,
    threads: usize,
    auto_unmount: bool,
    hydrate_timeout: Duration,
    read_write: bool,
    delete_limit: Option<usize>,
    cache_mode: Option<String>,
    conflict: Option<String>,
) -> Result<serde_json::Value, IpcError> {
    ensure_session(state).await?;
    let sid = require_sid(state).await?;
    // ★ M10：记下这个挂载点用的是哪个账号（换账号时靠它决定推不推新 sid）
    let mount_user = current_session(state)
        .await
        .map(|s| s.username)
        .unwrap_or_default();
    let mp = std::fs::canonicalize(&mountpoint).map_err(|e| {
        IpcError::new(
            ErrorKind::Io,
            format!("挂载点不可用 {}: {e}", mountpoint.display()),
        )
    })?;
    if state.mounts.lock().unwrap().contains_key(&mp) {
        return Err(IpcError::new(
            ErrorKind::BadRequest,
            format!("{} 已经挂载了", mp.display()),
        ));
    }

    // 这一个挂载点对应的 NAS 文件夹：显式给了就用它，否则用 link 的 home_root
    let remote =
        qxync_core::normalize_root(&remote.unwrap_or_else(|| qxync_core::HOME_ROOT.to_string()));
    // ★ 可写性**不预判**：能不能写由 NAS 说了算 —— Qsync 里登记成同步文件夹的目录
    //   （`qbox_get_syncing_folder_list` 列出来的那些）可以写；没登记过的共享文件夹会被
    //   服务端拒绝（实测 `status:20`）。以前这里按「是不是 home_root」一刀切成只读，
    //   那是错的判据（家目录之外登记过的目录照样能写）：用户勾了「读写」就按读写挂，
    //   真被服务端拒了由上传队列 / 错误列表如实报出来，而不是在本地替 NAS 做决定。
    // 缓存按「主机」隔离：不同 NAS 上的同名路径不能共用缓存文件
    let host_ns: String = state
        .link
        .host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cache = cache_dir
        .unwrap_or_else(|| {
            ConfigPaths::discover()
                .map(|p| p.data_dir.join("cache"))
                .unwrap_or_else(|_| PathBuf::from("/tmp/qxync/cache"))
        })
        .join(host_ns);

    // 给 FUSE 一个独立 Client（只带 sid），避免和 daemon 主体抢同一把锁
    // ★ M8.4：FUSE 的客户端也要走设置里的代理（否则挂载后水合直连、绕开代理）
    let fuse_client =
        Client::new_with_proxy(&state.link, Some(&state.settings.lock().unwrap().proxy))
            .map_err(map_err)?;
    fuse_client.set_sid(sid);
    let counters = Arc::new(HydroCounters::default());
    let fuse_client = Arc::new(fuse_client);
    let mut fs = QxyncFs::new(fuse_client.clone(), remote.clone(), cache.clone())
        .map_err(|e| IpcError::new(ErrorKind::Io, e.to_string()))?
        .with_hydrate_timeout(hydrate_timeout)
        .with_counters(counters.clone())
        .with_pins(state.pins.clone())
        // ★ M7：选择性同步规则 + LAN 对端（水合时先试 LAN，失败回落 NAS）
        .with_rules(state.rules.clone())
        .with_peers(state.peers.clone())
        // ★ M15/T9：注入稳定身份索引所在的 `Store`。
        //
        // 注入之后 rename 会同步迁移 `nodes` 行、删除会清索引、上传队列能按身份
        // 反查最新名字。不注入功能也安全（退化方向保守），但 T9 的收益拿不到。
        //
        // `SyncState.store` 是裸 `Store`，这里包一层 `Arc` 交给 FUSE 侧 ——
        // `Store` 的连接本来就在 `Arc` 里（见 `qxync-core/src/store.rs`），
        // 所以两边看到的仍是**同一个 SQLite 连接**，不会出现两个连接抢写锁。
        .with_nodes_store(Arc::new(state.sync_store.lock().unwrap().store.clone()));
    if let Some(limit) = delete_limit {
        fs = fs.with_delete_limit(limit);
    }
    // ★ M3：缓存模式（pagecache 默认 / direct 绕过 page cache）
    let mode = match cache_mode.as_deref() {
        None => CacheMode::PageCache,
        Some(s) => CacheMode::parse(s).ok_or_else(|| {
            IpcError::new(
                ErrorKind::BadRequest,
                format!("cache_mode 只能是 pagecache/direct，收到 {s:?}"),
            )
        })?,
    };
    fs = fs.with_cache_mode(mode);
    let mut upload_queue = None;
    let mut delete_queue = None;
    if read_write {
        let marker_dir = ConfigPaths::discover()
            .map(|p| p.data_dir.join("upload-queue"))
            .unwrap_or_else(|_| cache.join("upload-queue"));
        let q = UploadQueue::new(
            fuse_client.clone(),
            tokio::runtime::Handle::current(),
            marker_dir.clone(),
        )
        .map_err(|e| IpcError::new(ErrorKind::Io, format!("创建上传队列失败: {e}")))?;
        q.spawn_worker()
            .map_err(|e| IpcError::new(ErrorKind::Io, format!("启动上传 worker 失败: {e}")))?;
        // ★ M11：删除队列。与上传队列**共用同一个队列目录 / 同一个 queue.db**
        // （表各自独立：`uploads` / `deletes`），不额外造一个库。
        // 有了它，`unlink` 立刻返回、远端删除由 worker 同目录攒批推送。
        let dq = DeleteQueue::new(
            fuse_client.clone(),
            tokio::runtime::Handle::current(),
            marker_dir,
        )
        .map_err(|e| IpcError::new(ErrorKind::Io, format!("创建删除队列失败: {e}")))?;
        dq.spawn_worker()
            .map_err(|e| IpcError::new(ErrorKind::Io, format!("启动删除 worker 失败: {e}")))?;
        fs = fs
            .with_write_mode()
            .with_upload_queue(q.clone())
            .with_delete_queue(dq.clone());
        upload_queue = Some(q);
        delete_queue = Some(dq);
    }
    // ★ M2c/M3：必须在 fs 被交给 fuser 之前取句柄，同步引擎与脱水都靠它
    let handle = fs.handle();
    // ★ M3：上传成功后清掉节点 dirty（否则脱水永远被 dirty 挡住）
    if let Some(q) = &upload_queue {
        let h = handle.clone();
        let peer = state.peer.lock().unwrap().clone();
        // 回调是 `'static` 的，捕获 `Arc<State>`（而不是那个 `StdMutex` 本身），
        // 回调里现取锁 —— 上传 worker 线程只在这一小段持锁。
        let st = state.clone();
        q.set_success_hook(Arc::new(move |remote: &str, sig: (u64, i64)| {
            let (size, mtime) = match h.node(remote) {
                // 上传期间用户又改了这个文件 → 节点上已经不是刚传上去的那一版了。
                // 此刻推进 baseline 到「刚传上去的签名」是**错的**：远端真有的是那
                // 一版，而本地马上要传的还有下一版。把 baseline 停在旧版本，
                // 下一版落地时再推一次，两次保存就都是各自的增量，不会互相打架。
                // （节点不存在 = 已被删；此时推进反而会留下幽灵 baseline 条目。）
                Some(n) if n.size == sig.0 && n.mtime == sig.1 => (n.size, n.mtime),
                Some(_) => {
                    tracing::debug!(
                        "{remote}: 上传期间本地又变了，baseline 暂不推进（等下一版落地）"
                    );
                    return;
                }
                None => return,
            };
            // 本地改动已经落到 NAS → 清 dirty，并让对端走事件快路径
            h.clear_dirty(remote);
            // ★ 同时把 baseline 推进到刚写上去的签名：否则「上传落地 → 下轮轮询」
            // 这段窗口里，用户再保存一次会被判成双方都改 → 凭空多一个冲突副本。
            {
                let mut g = st.sync_store.lock().unwrap();
                if let Err(e) = g.note_uploaded(remote, size, mtime) {
                    tracing::warn!("上传后推进 baseline 失败 {remote}: {e}");
                }
            }
            if let (Some(host), Some((size, mtime))) = (peer.as_ref(), Some((size, mtime))) {
                host.notify_async(remote.to_string(), size, mtime, "modified");
            }
        }));
    }

    // ★ M3：用 spawn（而不是阻塞的 mount2）—— 拿到 Notifier 才能发 inval_inode
    let MountHandle { session, notifier } =
        qxync_fuse::spawn(fs, &mp, threads, auto_unmount, !read_write)
            .map_err(|e| IpcError::new(ErrorKind::Io, format!("挂载失败: {e}")))?;

    // ★ M9：后台把远端新内容原子换上之后，必须让内核丢掉旧的 page cache ——
    //   否则 pagecache 模式下 `cat` 会拿到内核里那份陈旧页。
    {
        let n = notifier.clone();
        handle.set_invalidator(std::sync::Arc::new(move |ino| n.inval_inode(ino, 0, 0)));
    }
    // ★ M10：sid 过期时，让挂载点能**同步**要一个新 sid 并原地重试
    //   （不注入的话 `ls`/`cat` 会一直 EIO 到重新挂载）。
    if let Some(f) = state.sid_refresher.lock().unwrap().clone() {
        handle.set_sid_refresher(f);
    }

    // 等挂载生效（Session::new 已同步挂上，这里只是兜底）
    let mut mounted = false;
    for _ in 0..40 {
        if is_mounted(&mp) {
            mounted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !mounted {
        let _ = std::process::Command::new("fusermount3")
            .arg("-u")
            .arg(&mp)
            .output();
        return Err(IpcError::new(
            ErrorKind::Io,
            format!("挂载 {} 未生效", mp.display()),
        ));
    }

    let info = MountInfo {
        mountpoint: mp.clone(),
        remote: remote.clone(),
        readonly: !read_write,
    };
    // ★ M8.4：冲突策略（未知值落回默认；与 Task::normalize 同一套判定）
    let conflict = conflict
        .filter(|c| qxync_core::tasks::CONFLICTS.contains(&c.as_str()))
        .unwrap_or_else(|| CONFLICT_RENAME_LOCAL.to_string());
    // ★ M15/T5：记下这次挂载的完整参数，供写 `mounts.json`（daemon 重启后按它恢复）。
    // 取的是**归一化之后**的值（`mp` 是 canonicalize 过的、`remote` 过了
    // `normalize_root`），所以恢复出来的挂载点和现在这个是同一个，不会出现
    // 「记录里是软链路径、实际挂的是真实路径」这种对不上的情况。
    let rec = MountRecord {
        mountpoint: mp.clone(),
        remote: remote.clone(),
        read_write,
        cache_mode: mode.as_str().to_string(),
        conflict: conflict.clone(),
        threads,
        hydrate_timeout_secs: hydrate_timeout.as_secs(),
        delete_limit,
        auto_unmount,
    };
    state.mounts.lock().unwrap().insert(
        mp.clone(),
        MountEntry {
            info: info.clone(),
            client: fuse_client.clone(),
            user: mount_user.clone(),
            session: Some(session),
            notifier,
            counters,
            upload: upload_queue,
            delete_queue,
            handle,
            cache_mode: mode,
            conflict: conflict.clone(),
            rec: rec.clone(),
        },
    );
    tracing::info!(
        "已挂载 {} -> {}（{} 线程，auto_unmount={auto_unmount}，cache_mode={}，冲突策略={}，{}）",
        mp.display(),
        info.remote,
        threads,
        mode.as_str(),
        conflict_label(&conflict),
        if read_write { "读写" } else { "只读" }
    );
    // ★ M15/T5：挂载成功才写盘。写失败只 WARN —— 记录文件是为了下次重启省一步，
    // 不能因为它把「已经挂上了」这件事变成失败（用户会以为没挂成功去重试）。
    mounts_persist(&state.mounts);
    to_value(info)
}

/// ★ M15/T5：把当前**挂载表**整体写进 `mounts.json`（daemon 重启后照它恢复）。
///
/// 刻意是「全量重建」而不是「读出来改一条再写回」：
///
/// * 挂载表本身就是唯一真相 —— 用它重建，内存与磁盘不可能对不上；
/// * 避开读-改-写之间的窗口（并发挂载时后写的会覆盖先写的）；
/// * 读不到旧文件也能写（比如用户手工删了 `mounts.json`，或格式升级后
///   旧版本被降级丢弃）—— 那就从零建一份，不用先救活旧文件。
///
/// 失败只 WARN：这份文件的作用是「下次重启省一步」，写不进去最坏结果是
/// 下次要手工重挂，**不该让 `mount` / `umount` 因此失败**。
fn mounts_persist(mounts: &StdMutex<HashMap<PathBuf, MountEntry>>) {
    let paths = match ConfigPaths::discover() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("挂载记录：读配置目录失败，本次的挂载改动不会被记下: {e}");
            return;
        }
    };
    // 按挂载点排序：挂载表是 HashMap（顺序不定），不排序的话每次写盘的字节序
    // 都不一样，diff 一片噪音、也没法人工核对。
    let mut recs: Vec<MountRecord> = mounts
        .lock()
        .unwrap()
        .values()
        .map(|e| e.rec.clone())
        .collect();
    // ★ M15/T6：把「仍挂着、但不由本进程托管」的记录**带上**。
    //   `mounts.json` 的语义是「上一次实际挂了什么」，而 T6 之后挂载点可以由
    //   另一个 qxyncd 进程托管着（它在挂载守护模式里）。那些挂载点不在本进程的
    //   挂载表里，纯「从挂载表全量重建」会把它们从记录里抹掉 —— 那个进程一旦
    //   结束，挂载点就再也回不来了（T5/T6 的语义都断掉）。
    //   判据与 T7 一样保守：内核挂载表里有 **且** subtype 是 qxync 才算。
    for rec in externally_held_records(&paths, &recs) {
        tracing::info!(
            "挂载记录：{} 由其他 qxyncd 进程托管，保留记录不覆盖",
            rec.mountpoint.display()
        );
        recs.push(rec);
    }
    recs.sort_by(|a, b| a.mountpoint.cmp(&b.mountpoint));
    let f = MountsFile {
        version: qxync_core::mounts::MOUNTS_VERSION,
        mounts: recs,
    };
    if let Err(e) = f.save(&paths) {
        tracing::warn!("挂载记录：写盘失败（本次挂载/卸载改动不会被记下）: {e}");
    }
}

/// ★ M15/T6：从 `mounts.json` 里挑出「**仍挂着、但不由本进程托管**」的记录，
/// 让 [`mounts_persist`] 写盘时把它们带上。
///
/// 存在的原因：T6 之后挂载点可能由另一个 qxyncd 进程托管（它在挂载守护模式里
/// 停同步、留着 FUSE 会话）。那些挂载点不在本进程的挂载表里，而
/// `mounts_persist` 是「从挂载表全量重建」—— 不带上就会把它们从记录里抹掉。
/// 记录一丢，等托管进程结束，挂载点就再也恢复不了（T5 的语义整个断掉）。
///
/// 判据刻意保守（与 T7 同一套边界）：**内核挂载表里有 + subtype 是 qxync +
/// 本进程的挂载表里没有**，三个条件都满足才算。别的文件系统绝不会被写进
/// 我们的记录里。
///
/// 读 `mounts.json` 失败就返回空 —— 那时写盘会退化成「纯挂载表重建」，
/// 与 T5 现有的降级行为一致（不报错，不阻塞）。
fn externally_held_records(paths: &ConfigPaths, mine: &[MountRecord]) -> Vec<MountRecord> {
    let (recorded, _) = MountsFile::load(paths);
    let Some(f) = recorded else {
        return Vec::new();
    };
    f.mounts
        .into_iter()
        .filter(|rec| !mine.iter().any(|m| m.mountpoint == rec.mountpoint))
        .filter(|rec| is_mounted(&rec.mountpoint) && is_qxync_mount(&rec.mountpoint))
        .collect()
}

async fn umount(state: &Arc<State>, mountpoint: PathBuf) -> Result<serde_json::Value, IpcError> {
    umount_inner(state, mountpoint, true).await
}

/// `umount` 的真正实现。
///
/// `persist_record` 区分两种「卸载」—— 这一层区分是 T5 能成立的前提：
///
/// * `true` = **用户主动卸载**（IPC `umount`）→ 意图是「我不要这个挂载点了」，
///   必须从 `mounts.json` 去掉，否则下次启动又给它挂回来；
/// * `false` = **daemon 退出时的强制清理**（`shutdown_all_mounts`）→ 进程马上就
///   没了，这只是把手上的挂载收干净，**不是**用户不要它了。若这里也去改记录，
///   `mounts.json` 会在每次 daemon 退出时被清空，T5 的自动重挂就永远失效。
///
/// （T6「SIGTERM 保留挂载」做完之后，退出路径会连 fusermount 都不调，
/// 但这个区分仍然要留着 —— 它现在的语义是「谁该为这次卸载负责」。）
async fn umount_inner(
    state: &Arc<State>,
    mountpoint: PathBuf,
    persist_record: bool,
) -> Result<serde_json::Value, IpcError> {
    let mp = std::fs::canonicalize(&mountpoint).unwrap_or(mountpoint);
    let mut entry = state.mounts.lock().unwrap().remove(&mp).ok_or_else(|| {
        IpcError::new(
            ErrorKind::BadRequest,
            format!("{} 不在挂载表里", mp.display()),
        )
    })?;

    // 先把未完成的上传做完，避免卸载丢改动
    if let Some(q) = &entry.upload {
        if !q.drain(Duration::from_secs(120)) {
            tracing::warn!("卸载前上传队列未排空（继续卸载，标记文件保留，下次启动会重试）");
        }
        q.shutdown();
    }
    // ★ M11：删除队列同理 —— 卸载时没发出去的删除必须做完，
    // 否则会留下「本地已经删了、NAS 上还在」的空洞。
    // 排不空也不阻塞卸载：`deletes` 表已持久化，下次启动会重新入队。
    if let Some(q) = &entry.delete_queue {
        if !q.drain(Duration::from_secs(120)) {
            tracing::warn!("卸载前删除队列未排空（继续卸载，未发出的删除已入库，下次启动会重试）");
        }
        q.shutdown();
    }
    let out = std::process::Command::new("fusermount3")
        .arg("-u")
        .arg(&mp)
        .output()
        .or_else(|_| {
            std::process::Command::new("fusermount")
                .arg("-u")
                .arg(&mp)
                .output()
        })
        .map_err(|e| IpcError::new(ErrorKind::Io, format!("执行 fusermount3 失败: {e}")))?;
    if !out.status.success() {
        // 放回挂载表，保持状态一致
        let msg = format!(
            "卸载失败: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        state.mounts.lock().unwrap().insert(mp.clone(), entry);
        return Err(IpcError::new(ErrorKind::Io, msg));
    }
    // 等挂载线程真正退出（fuser 的后台会话）
    match entry.session.take() {
        Some(s) => {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(s.join());
            });
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Ok(())) => tracing::info!("已卸载 {}", mp.display()),
                Ok(Err(e)) => tracing::warn!("挂载会话退出报错: {e}"),
                Err(e) => tracing::warn!("等待挂载线程退出超时: {e}"),
            }
        }
        None => tracing::info!("已卸载 {}", mp.display()),
    }
    // ★ M15/T5：真正卸掉了才从 `mounts.json` 里去掉。放在这一步之后 ——
    // 上面的失败分支会把挂载表条目放回去，那时就**不该**动记录文件。
    if persist_record {
        mounts_persist(&state.mounts);
    }
    to_value(serde_json::json!({}))
}

fn mounts(state: &Arc<State>) -> Result<serde_json::Value, IpcError> {
    let (list, _, _, _, _) = snapshot_mounts(state);
    to_value(list)
}

fn snapshot_mounts(
    state: &Arc<State>,
) -> (
    Vec<MountInfo>,
    HydroStats,
    Option<qxync_core::ipc::UploadInfo>,
    Option<qxync_core::ipc::DeleteInfo>,
    Option<qxync_core::ipc::TransferInfo>,
) {
    let g = state.mounts.lock().unwrap();
    let list = g.values().map(|m| m.info.clone()).collect();
    let (mut count, mut bytes) = (0u64, 0u64);
    let mut uploads: Option<qxync_core::ipc::UploadInfo> = None;
    // ★ M11：删除队列汇总。任一挂载点有删除队列就报（`pending>0` 说明还有没推完的删除）。
    let mut deletes: Option<qxync_core::ipc::DeleteInfo> = None;
    // ★ T8：传输汇总。**任一挂载点有在途作业就报**（全 0 时给 `None`，
    // 让前端能区分「没有在传」与「这个版本不懂 T8」）。
    let mut tr = qxync_core::ipc::TransferInfo::default();
    for m in g.values() {
        let (c, b) = m.counters.snapshot();
        count += c;
        bytes += b;
        if let Some(u) = m.upload.as_ref().map(|q| q.snapshot()) {
            let e = uploads.get_or_insert_with(Default::default);
            e.active = e.active || u.active;
            e.pending += u.pending;
            e.done += u.done;
            e.failed += u.failed;
            e.retries += u.retries;
            e.bytes += u.bytes;
        }
        if let Some(d) = m.delete_queue.as_ref().map(|q| q.snapshot()) {
            let e = deletes.get_or_insert_with(Default::default);
            e.active = e.active || d.active;
            e.pending += d.pending;
            e.done += d.done;
            e.failed += d.failed;
            e.retries += d.retries;
            e.batches += d.batches;
            e.deleted += d.deleted;
        }
        // ★ T8：下载在途（节点表）+ 上传在途（队列），两边都不重不漏
        let (dn, dd, dt) = m.handle.download_transfers();
        tr.downloading += dn;
        tr.done_bytes += dd;
        tr.total_bytes += dt;
        if let Some(q) = m.upload.as_ref() {
            let (un, ud, ut) = q.upload_transfers();
            tr.uploading += un;
            tr.done_bytes += ud;
            tr.total_bytes += ut;
        }
    }
    tr.active = tr.downloading + tr.uploading;
    let transfers = (tr.active > 0).then_some(tr);
    (
        list,
        HydroStats { count, bytes },
        uploads,
        deletes,
        transfers,
    )
}

async fn shutdown_all_mounts(state: &Arc<State>) -> usize {
    let mps: Vec<PathBuf> = state.mounts.lock().unwrap().keys().cloned().collect();
    let mut n = 0;
    for mp in mps {
        // 忽略错误：即使 fusermount 失败，进程退出时 auto_unmount 也会兜底。
        // ★ M15/T5：`persist_record=false` —— 退出时的清理不是「用户不要这个挂载点」，
        // 不能让它把 `mounts.json` 清空（否则 T5 的自动重挂每次都无记录可依）。
        if let Ok(_) = umount_inner(state, mp, false).await {
            n += 1;
        }
    }
    n
}

/// 判断路径是否已是挂载点（读 `/proc/self/mounts`，处理空格的八进制转义）。
fn is_mounted(path: &Path) -> bool {
    let Ok(txt) = std::fs::read_to_string("/proc/self/mounts") else {
        return false;
    };
    let target = path.to_string_lossy().to_string();
    txt.lines().any(|line| {
        let mut it = line.split_whitespace();
        let _dev = it.next();
        match it.next() {
            Some(mp) => mp.replace("\\040", " ") == target,
            None => false,
        }
    })
}

/// ★ M15/T6/T7：`/proc/self/mounts` 里该挂载点的 **fstype**（如 `fuse.qxync`）。
///
/// 为什么需要它：**「`is_mounted` 为真」不足以说明「这是我们挂的」**。
/// 路径相同但挂载者是别人的情况很现实（T7 的安全边界就靠这一条挡住误删）。
/// 挂载时 subtype 写死在 [`qxync_fuse::mount_options`] 的 `Subtype("qxync")`
/// （`qxync-fuse/src/lib.rs`），内核里就呈现为 `fuse.qxync`。
fn mount_subtype(path: &Path) -> Option<String> {
    let txt = std::fs::read_to_string("/proc/self/mounts").ok()?;
    let target = path.to_string_lossy().to_string();
    txt.lines()
        .find_map(|line| parse_mounts_line(line, &target))
}

/// 解析 `/proc/self/mounts` 的一行，取出挂载点等于 `target` 时的 **fstype**（第三列）。
///
/// 内核会把路径里的空格写成八进制转义 `\040`（`is_mounted` 原本就处理这个），
/// 不还原就会让带空格的挂载点永远对不上 —— 那样 T7 会漏清、T6 会漏识别。
///
/// 残行返回 `None` 而不是 panic：`/proc` 的内容随时在变，读到半行是可能的。
fn parse_mounts_line(line: &str, target: &str) -> Option<String> {
    let mut it = line.split_whitespace();
    let _dev = it.next()?; // 设备
    let mp = it.next()?; // 挂载点
    if mp.replace("\\040", " ") != target {
        return None;
    }
    // 第三列是 fstype：FUSE 挂载带 subtype 时形如 `fuse.qxync`
    Some(it.next()?.to_string())
}

/// ★ M15/T6/T7：这个挂载点是不是**我们（qxync）**挂的。
///
/// 只认 `fuse.qxync`（`Subtype("qxync")` 的呈现形式）。拿不准时一律判「不是
/// 自己的」—— 判错的代价是「该清的僵尸没清」（用户手工 `fusermount -uz` 一下
/// 就行），反过来会把别人的挂载点卸掉（那是不可逆的数据事故）。
fn is_qxync_mount(path: &Path) -> bool {
    mount_subtype(path).map(|fstype| is_qxync_type(&fstype)) == Some(true)
}

/// fstype 是不是 qxync 挂载留下的那个（**纯函数** —— 判定要能脱离 FUSE 单测）。
fn is_qxync_type(fstype: &str) -> bool {
    fstype == QXYNC_FSTYPE
}

/// qxync 挂载在 `/proc/self/mounts` 里的 fstype。
const QXYNC_FSTYPE: &str = "fuse.qxync";

// ---------------------------------------------------------------- ★ M15/T7 僵尸挂载清理

/// ★ M15/T7：daemon 启动早期清理**僵尸挂载**（内核里还挂着、但已经没人应答的）。
///
/// ## 为什么需要
///
/// `kill -9` / OOM 之后挂载点留在 `/proc/self/mounts` 里，但持 FUSE 会话的进程
/// 已经没了 → 用户 `cd` 进去看到 `Transport endpoint is not connected`（ENOTCONN）。
/// 不清掉它，后面 [`restore_mounts_on_start`] 会一直撞「路径已挂载」而恢复失败，
/// 于是「重启能自愈」这条路整个断掉。
///
/// ## 安全边界（四条，少一条就可能误卸别人的挂载）
///
/// 1. **只遍历 `mounts.json` 里记过的挂载点**，绝不扫全盘 —— 我们没有权力
///    决定系统上哪些挂载点该存在，只对「自己记过的那几个」负责；
/// 2. **必须已挂载**（`is_mounted`）才有得清，没挂载的直接跳过；
/// 3. **必须写探针失败**（= 内核已经不应答了）才动手 —— 活的挂载点绝不碰；
/// 4. **必须 subtype 是 `fuse.qxync`**（[`is_qxync_mount`]）—— 是别的文件系统
///    就只 WARN，绝不卸载。
///
/// ## ★ 刻意保留的两点
///
/// * **用 `-uz`（lazy）而不是 `-u`**：`-u` 遇到「挂载点里还有进程持有 fd」
///   会失败，而那种情况恰恰是崩溃现场最常见的形态；
/// * **清完不删 `mounts.json` 里的记录** —— 这才是 T7 的意义所在：
///   记录留着，恢复逻辑才能把它重新挂回来。删了记录就变成「清理 = 卸载用户
///   的挂载点」，与意图正好相反。
///
/// 失败只 WARN，绝不阻塞 daemon 启动。
fn cleanup_zombie_mounts() {
    let paths = match ConfigPaths::discover() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("僵尸挂载清理：读配置目录失败，跳过: {e}");
            return;
        }
    };
    let (recorded, warns) = MountsFile::load(&paths);
    for w in warns {
        tracing::warn!("{w}");
    }
    let mut todo: Vec<PathBuf> = recorded
        .map(|f| f.mounts.into_iter().map(|m| m.mountpoint).collect())
        .unwrap_or_default();
    if todo.is_empty() {
        return;
    }
    // 排序只为日志可复现（HashMap 顺序不定）
    todo.sort();
    tracing::info!("僵尸挂载清理：检查 {} 个已记录的挂载点", todo.len());

    let mut cleaned = 0usize;
    for mp in todo {
        // 边界 2 先过：没挂载就没什么可清的（`mounts.json` 有记录但没挂 = 正常）
        let mounted = is_mounted(&mp);
        // ★ subtype 的检查排在探针**之前**（见 [`zombie_action`] 的说明）：
        //   探针要写文件，对别人的文件系统写一次就是一次不该有的副作用。
        let ours = mounted && is_qxync_mount(&mp);
        // 边界 3：只有真的要动手时才探活（探针有副作用，不能白写）
        let alive = match zombie_action(mounted, ours) {
            ZombieAction::Unmount => probe_mount_alive(&mp),
            ZombieAction::Skip(reason) => {
                if mounted && !ours {
                    tracing::warn!(
                        "僵尸挂载清理：{} 在内核挂载表里但 subtype 不是 {QXYNC_FSTYPE}\
                         （可能是别的文件系统），不碰",
                        mp.display()
                    );
                } else if let Some(r) = reason {
                    tracing::debug!("僵尸挂载清理：{} 跳过（{r}）", mp.display());
                }
                continue;
            }
        };
        if alive {
            continue;
        }
        match lazy_umount(&mp) {
            Ok(()) => {
                cleaned += 1;
                // ★ 刻意不动 mounts.json：记录留着，恢复逻辑会把它重新挂回来。
                tracing::info!(
                    "僵尸挂载清理：{} 已 ENOTCONN，lazy 卸载成功（记录保留，将由启动恢复重建）",
                    mp.display()
                );
            }
            Err(e) => tracing::warn!(
                "僵尸挂载清理：{} 是僵尸但 lazy 卸载失败（跳过，不阻塞启动）: {e}",
                mp.display()
            ),
        }
    }
    if cleaned > 0 {
        tracing::info!("僵尸挂载清理：{} 个已清掉", cleaned);
    }
}

/// ★ M15/T7：某个已记录的挂载点该不该被清掉（**纯函数** —— 真机上要 `kill -9`
/// 才造得出这些组合，所以判定逻辑必须能脱离 FUSE 单测）。
#[derive(Debug, PartialEq, Eq)]
enum ZombieAction {
    /// 探针确认 ENOTCONN 且确认是我们挂的 → 可以 `fusermount3 -uz`
    Unmount,
    /// 不动。`String` 是原因（`None` = 「没挂载」，不值得记日志）
    Skip(Option<&'static str>),
}

/// T7 安全判定的唯一实现处 —— [`cleanup_zombie_mounts`] 与单测都走它。
///
/// 判定的**顺序**本身是有讲究的：
///
/// * `mounted == false` → 不碰。没挂载不是僵尸（那是「记录有过、现在没挂」的
///   正常状态，比如上次卸载了但记录还在）；
/// * `!ours` → 不碰。这是**最关键的一条**：路径相同但挂载者是别人的情况很现实，
///   卸掉别人的挂载点是不可逆的数据事故。宁可漏清（用户手工
///   `fusermount3 -uz` 一下就行）；
/// * 剩下的才交给探针确认（[`probe_mount_alive`] 单独判 ENOTCONN）。
fn zombie_action(mounted: bool, ours: bool) -> ZombieAction {
    if !mounted {
        return ZombieAction::Skip(None);
    }
    if !ours {
        return ZombieAction::Skip(Some("不是 qxync 挂的，绝不碰"));
    }
    ZombieAction::Unmount
}

/// 探针文件名。**必须能安全地删掉**，所以不与任何真实文件同名。
const ZOMBIE_PROBE: &str = ".qxync-probe";

/// ★ M15/T7：往挂载点里写一个探针文件，看内核还应答不。
///
/// 活的挂载点 → 写得进去（并把探针删掉）；僵尸 → `ENOTCONN`，写失败。
///
/// 刻意**不用 `fs::metadata`**：那会命中内核的 dentry/inode 缓存，
/// 僵尸挂载点也可能返回缓存里的属性，看起来「活着」—— 于是清理永远不触发。
/// 写文件必须真走一遍 FUSE 的 `create` → 才拿得到真话。
///
/// 只读挂载（`qxync mount` 默认只读）会写失败并返回 `EROFS` —— 那不是僵尸。
/// 所以**只有 `ENOTCONN` 才算僵尸**，其它错误一律当「活的」（宁可漏清，
/// 也不能对着一个健康挂载点动手）。
fn probe_mount_alive(mp: &Path) -> bool {
    let probe = mp.join(ZOMBIE_PROBE);
    match std::fs::File::create(&probe) {
        Ok(_) => {
            // 探针绝不能留在用户的挂载点里（会被 readdir 看到、会被同步上去）
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(e) if e.raw_os_error() == Some(libc::ENOTCONN) => {
            tracing::debug!(
                "僵尸挂载清理：{} 探针写入返回 ENOTCONN（已确认僵尸）",
                mp.display()
            );
            false
        }
        Err(e) => {
            // EROFS（只读挂载）、EACCES、EIO… 一律按「活的」处理
            tracing::debug!(
                "僵尸挂载清理：{} 探针写入失败但不是 ENOTCONN（{e}），按活的处理",
                mp.display()
            );
            true
        }
    }
}

/// ★ M15/T7：lazy 卸载（`fusermount3 -uz`，回退 `fusermount -uz`）。
///
/// 懒卸载的理由见 [`cleanup_zombie_mounts`]：挂载点里通常还有进程持有 fd。
fn lazy_umount(mp: &Path) -> std::io::Result<()> {
    let try_cmd = |bin: &str| std::process::Command::new(bin).arg("-uz").arg(mp).output();
    let attempt = match try_cmd("fusermount3") {
        Ok(o) => Ok(o),
        Err(_) => try_cmd("fusermount"),
    };
    let out = attempt.map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("fusermount3 / fusermount 都不可用: {e}"),
        )
    })?;
    if out.status.success() {
        return Ok(());
    }
    Err(std::io::Error::other(format!(
        "fusermount -uz 失败: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )))
}

// ---------------------------------------------------------------- M2c 同步引擎

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 当前同步状态（`status` / `sync` 共用；计数器是**累计值**）。
fn sync_info(state: &Arc<State>) -> SyncInfo {
    let snap = state.sync_stats.snapshot();
    let (cursors, baseline_entries, note) = {
        let g = state.sync_store.lock().unwrap();
        (g.cursors, g.baseline.len(), None::<String>)
    };
    let interval = *state.sync_interval.lock().unwrap();
    SyncInfo {
        enabled: interval > 0,
        interval_secs: interval,
        polls: snap.polls,
        last_poll_age_secs: if snap.last_poll_unix == 0 {
            0
        } else {
            now_secs().saturating_sub(snap.last_poll_unix)
        },
        cursors: SyncCursors {
            config: cursors.config,
            notify: cursors.notify,
            global_notify: cursors.global_notify,
            max_log_seen: cursors.max_log_seen,
            log_missing_count: cursors.log_missing_count,
        },
        baseline_entries: baseline_entries as u64,
        refreshed: snap.refreshed,
        conflicts: snap.conflicts,
        uploaded: snap.uploaded,
        deleted: snap.deleted,
        deletes_blocked: snap.deletes_blocked,
        events: snap.events,
        devices: snap.devices,
        last_error: snap.last_error,
        delete_block_reason: snap.delete_block_reason,
        note: snap.last_note.or(note),
    }
}

/// 引擎专用客户端（带当前 sid，不占 `state.client` 的锁）。
async fn engine_client(state: &Arc<State>) -> Result<Arc<Client>, IpcError> {
    ensure_session(state).await?;
    let sid = require_sid(state).await?;
    let mut g = state.engine_client.lock().await;
    let stale = g
        .as_ref()
        .map(|c| c.sid().as_deref() != Some(sid.as_str()))
        .unwrap_or(true);
    if stale {
        let c = Client::new_with_proxy(&state.link, Some(&state.settings.lock().unwrap().proxy))
            .map_err(map_err)?;
        c.set_sid(sid);
        *g = Some(Arc::new(c));
    }
    Ok(g.clone().expect("刚填过"))
}

/// 当前挂载视图（同步引擎的操作对象）。
///
/// 一对一：**一个挂载点 = 一个远端根**，所以一个挂载就是一个视图。
fn build_views(state: &Arc<State>) -> Vec<MountView> {
    let g = state.mounts.lock().unwrap();
    let mut out = Vec::new();
    for m in g.values() {
        out.push(MountView {
            mountpoint: m.info.mountpoint.clone(),
            remote_root: m.info.remote.clone(),
            view: Arc::new(m.handle.clone()) as Arc<dyn LocalView>,
            upload: m.upload.clone(),
            read_only: m.info.readonly,
            rules: state.rules.clone(),
            conflict: m.conflict.clone(),
        });
    }
    out
}

/// 跑一轮同步（拉事件 + baseline 对账）。
async fn run_sync_once(state: &Arc<State>) -> Result<sync::SyncReport, IpcError> {
    let client = engine_client(state).await?;
    let views = build_views(state);
    let cfg = state.sync_cfg.lock().unwrap().clone();
    let user = state.link.user.clone();
    let report = sync::poll_once(
        &client,
        &views,
        &state.sync_store,
        &cfg,
        &state.sync_stats,
        &user,
    )
    .await;
    // ★ M8.3：把这一轮的结论写进 journal（界面「文件更新中心 / 错误列表」的数据来源）。
    //   只 push 到内存缓冲，落库交给后台批量任务 —— 这里位于轮询热路径上。
    journal_record_sync(state, &report);
    // `--force-deletes` 只放行一轮
    {
        let mut c = state.sync_cfg.lock().unwrap();
        if c.force_deletes {
            c.force_deletes = false;
        }
    }
    Ok(report)
}

/// `qxync sync`：查看/触发/调参同步引擎。
async fn sync_cmd(
    state: &Arc<State>,
    once: bool,
    force_deletes: bool,
    max_deletes: Option<usize>,
    interval_secs: Option<u64>,
) -> Result<serde_json::Value, IpcError> {
    if let Some(secs) = interval_secs {
        *state.sync_interval.lock().unwrap() = secs.min(86_400);
    }
    if let Some(m) = max_deletes {
        state.sync_cfg.lock().unwrap().delete_protection.max_entries = m;
        tracing::info!("删除保护阈值 = {m}");
    }
    if force_deletes {
        // 只把 force_deletes 置位（决策处直接跳过检查），不动阈值 —— 否则 `--force-deletes`
        // 会把后续所有轮次的绝对阈值永久放开。
        state.sync_cfg.lock().unwrap().force_deletes = true;
        // 同时解除 FUSE 侧的本地批量删除熔断
        let guards: Vec<_> = state
            .mounts
            .lock()
            .unwrap()
            .values()
            .map(|m| m.handle.delete_guard())
            .collect();
        for g in guards {
            g.reset();
        }
        state.sync_stats.delete_block_reason.lock().unwrap().take();
        tracing::warn!("删除保护已解除（--force-deletes，仅放行一轮）");
    }
    if once {
        let report = run_sync_once(state).await?;
        tracing::info!(
            "sync --once: events={} refreshed={} updating={} uploaded={} conflicts={} deleted={} blocked={}",
            report.events,
            report.refreshed,
            report.content_updating,
            report.uploaded,
            report.conflicts,
            report.deleted,
            report.deletes_blocked
        );
    }
    to_value(sync_info(state))
}

/// 后台轮询：按 `sync_interval` 周期跑 `run_sync_once`；未登录时静默跳过。
///
/// ★ M15/T6：带一代 [`EngineGen`] —— SIGHUP 重载时它做完当前一轮就退出。
fn spawn_poller(state: Arc<State>, mut gen: EngineGen) -> tokio::task::JoinHandle<()> {
    let interval = *state.sync_interval.lock().unwrap();
    if interval == 0 {
        tracing::info!("变更轮询已禁用（QXNYC_POLL_INTERVAL=0）");
        return tokio::spawn(async {});
    }
    tracing::info!("变更轮询已启动：每 {interval}s 一轮（QXNYC_POLL_INTERVAL 可调）");
    tokio::spawn(async move {
        let mut last_run = Instant::now() - Duration::from_secs(3600);
        loop {
            let secs = *state.sync_interval.lock().unwrap();
            if secs == 0 {
                if !gen.sleep(Duration::from_secs(2)).await {
                    tracing::info!("变更轮询退出（同步引擎已换代）");
                    return;
                }
                continue;
            }
            // ★ M7：正常睡到下一轮；对端事件到达时提前醒来（事件是快路径，对账是主路径）
            //
            // ★ M15/T6：多了一个「换代通知」分支 —— 它一到就退出，**不会**顺手续一轮，
            //   否则新旧两代会同时对同一棵树做对账（同一处改动被判两次冲突）。
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(secs.clamp(1, 3600))) => {}
                _ = gen.changed() => {
                    tracing::info!("变更轮询退出（同步引擎已换代）");
                    return;
                }
                _ = state.peer_wake.notified() => {
                    let since = last_run.elapsed();
                    if since < crate::peer_host::WAKE_DEBOUNCE {
                        tokio::time::sleep(crate::peer_host::WAKE_DEBOUNCE - since).await;
                    }
                    tracing::debug!("对端事件唤醒轮询（距上轮 {:?}）", last_run.elapsed());
                }
            }
            if *state.sync_interval.lock().unwrap() == 0 {
                continue;
            }
            if state.client.lock().await.sid().is_none() {
                // 懒登录：没会话就不轮询（避免每次都发一个必然失败的请求）
                match login_internal(&state, None, None).await {
                    Ok(_) => {}
                    Err(e) => {
                        tracing::debug!("轮询等待登录: {}", e.message);
                        continue;
                    }
                }
            }
            last_run = Instant::now();
            match run_sync_once(&state).await {
                Ok(r) => {
                    if r.conflicts > 0 || r.deletes_blocked > 0 || !r.errors.is_empty() {
                        tracing::warn!(
                            "轮询有异常: conflicts={} blocked={} errors={:?}",
                            r.conflicts,
                            r.deletes_blocked,
                            r.errors
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!("轮询失败: {}", e.message);
                    *state.sync_stats.last_error.lock().unwrap() = Some(e.message.clone());
                    // ★ M10：会话失效 → 重登（`login_internal` 会把新 sid 推给挂载点）。
                    //   以前这里只是记一条错误，sid 死了就每一轮都失败。
                    if e.kind == ErrorKind::Auth {
                        tracing::warn!("轮询遇会话失效，重登后下一轮继续");
                        if let Err(e2) = login_internal(&state, None, None).await {
                            tracing::warn!("轮询重登失败: {}", e2.message);
                        }
                    }
                }
            }
        }
    })
}

/// `qxync rm <dir> <name>`：删远端条目（脚本/测试用；FUSE 的 unlink 走同一方法）。
async fn rm(state: &Arc<State>, dir: String, name: String) -> Result<serde_json::Value, IpcError> {
    ensure_session(state).await?;
    let d = dir.clone();
    let n = name.clone();
    with_client!(state, |c| c.delete_entry(&d, &n))?;
    let path = format!("{}/{}", dir.trim_end_matches('/'), name);
    let p2 = path.clone();
    // 记 write log（尽力而为，让其它设备看到）
    let _ = with_client!(state, |c| c
        .write_log(&p2, qxync_client::write_action::DELETE));
    to_value(serde_json::json!({ "deleted": path }))
}

// ---------------------------------------------------------------- M3 脱水

/// 脱水配置（环境变量初始化，IPC 可临时覆盖）。
#[derive(Debug, Clone)]
struct DehydrateCfg {
    /// 定时脱水：只清闲置 ≥ N 秒的文件（0 = 关闭定时脱水）。
    idle_secs: u64,
    /// 缓存限额（`512M` / `2G` / `25%`）。
    limit: Option<CacheLimit>,
    /// 扫描间隔（秒）。
    interval_secs: u64,
}

impl DehydrateCfg {
    fn from_env() -> Self {
        let idle = std::env::var("QXNYC_DEHYDRATE_IDLE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0u64);
        let limit = std::env::var("QXNYC_CACHE_LIMIT")
            .ok()
            .and_then(|v| CacheLimit::parse(&v));
        let interval = std::env::var("QXNYC_DEHYDRATE_INTERVAL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60u64);
        Self {
            idle_secs: idle,
            limit,
            interval_secs: interval.max(1),
        }
    }
    fn enabled(&self) -> bool {
        self.idle_secs > 0 || self.limit.is_some()
    }
}

#[derive(Default)]
struct DehydrateStats {
    runs: AtomicU64,
    dehydrated: AtomicU64,
    freed: AtomicU64,
    blocked_dirty: AtomicU64,
    blocked_pinned: AtomicU64,
    blocked_open: AtomicU64,
    blocked_mapped: AtomicU64,
    blocked_inflight: AtomicU64,
    last_run_unix: AtomicU64,
    last_error: StdMutex<Option<String>>,
}

/// `qxync dehydrate` 的参数。
struct DehydrateOpts {
    path: Option<String>,
    all: bool,
    idle_secs: Option<u64>,
    cache_limit: Option<String>,
    force: bool,
    dry_run: bool,
    mountpoint: Option<PathBuf>,
}

/// 缓存/脱水状态快照（`status` 用）。
fn cache_info(state: &Arc<State>) -> CacheInfo {
    let cfg = state.dehydrate_cfg.lock().unwrap().clone();
    let st = &state.dehydrate_stats;
    let g = state.mounts.lock().unwrap();
    let mut used = 0u64;
    let mut files = 0u64;
    let mut hydrated = 0u64;
    let mut mode = "pagecache".to_string();
    for m in g.values() {
        let s = m.handle.cache_stats();
        used += s.used_bytes;
        files += s.total_files;
        hydrated += s.hydrated_files;
        if m.cache_mode == CacheMode::Direct {
            mode = "direct".to_string();
        }
    }
    let last = st.last_run_unix.load(Ordering::Relaxed);
    CacheInfo {
        mode,
        used_bytes: used,
        total_files: files,
        hydrated_files: hydrated,
        limit_bytes: cfg
            .limit
            .map(|l| resolve_limit(Some(l), &first_cache_dir(state)).unwrap_or(0)),
        idle_secs: cfg.idle_secs,
        dehydrated_total: st.dehydrated.load(Ordering::Relaxed),
        freed_total_bytes: st.freed.load(Ordering::Relaxed),
        last_sweep_age_secs: if last == 0 {
            0
        } else {
            now_secs().saturating_sub(last)
        },
        blocked_dirty: st.blocked_dirty.load(Ordering::Relaxed),
        blocked_pinned: st.blocked_pinned.load(Ordering::Relaxed),
        blocked_open: st.blocked_open.load(Ordering::Relaxed),
        blocked_mapped: st.blocked_mapped.load(Ordering::Relaxed),
        blocked_inflight: st.blocked_inflight.load(Ordering::Relaxed),
        last_error: st.last_error.lock().unwrap().clone(),
    }
}

fn first_cache_dir(state: &Arc<State>) -> PathBuf {
    state
        .mounts
        .lock()
        .unwrap()
        .values()
        .next()
        .map(|m| m.handle.cache_dir().to_path_buf())
        .or_else(|| {
            ConfigPaths::discover()
                .ok()
                .map(|p| p.data_dir.join("cache"))
        })
        .unwrap_or_else(|| PathBuf::from("/tmp/qxync/cache"))
}

/// 百分比限额 → 字节（用缓存所在文件系统的总容量）。
fn resolve_limit(limit: Option<CacheLimit>, cache_dir: &std::path::Path) -> Option<u64> {
    limit.map(|l| match l {
        CacheLimit::Bytes(b) => b,
        CacheLimit::Percent(_) => {
            let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
            let c = std::ffi::CString::new(cache_dir.to_string_lossy().as_bytes()).ok();
            let total = match c {
                Some(c) => {
                    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0 {
                        st.f_blocks as u64 * st.f_frsize as u64
                    } else {
                        0
                    }
                }
                None => 0,
            };
            l.bytes(total)
        }
    })
}

/// 扫描 `/proc/*/maps`，找出挂载点下被 mmap 的远端路径（脱水必须避开它们）。
/// 扫 `/proc/*/maps` 找出「挂载点下被 mmap 的文件」→ 映射回远端路径。
///
/// 一对一：挂载点**就是**那个远端根，所以「挂载点 + 挂载点内的相对路径」直接拼成远端路径。
/// 映射错了后果很严重：mmap 判定失守 → 脱水会在别人还映射着的时候清内容（铁则 2 的相关保护）。
fn mmap_remotes(
    mountpoint: &std::path::Path,
    remote_root: &str,
) -> std::collections::BTreeSet<String> {
    use std::collections::BTreeSet;
    let mut out = BTreeSet::new();
    let mp = mountpoint.to_string_lossy().to_string();
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return out;
    };
    for p in procs.flatten() {
        let name = p.file_name();
        let name = name.to_string_lossy();
        if !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(maps) = std::fs::read_to_string(p.path().join("maps")) else {
            continue;
        };
        for line in maps.lines() {
            if let Some(i) = line.find(&mp) {
                let mapped = &line[i..];
                let mapped = mapped.trim_end();
                if mapped == mp {
                    continue;
                }
                let rest = &mapped[mp.len()..];
                if rest.starts_with('/') {
                    out.insert(format!("{}{}", remote_root.trim_end_matches('/'), rest));
                }
            }
        }
    }
    out
}

/// 执行一次脱水（`qxync dehydrate` 与后台扫描共用）。
async fn run_dehydrate(state: &Arc<State>, opts: DehydrateOpts) -> DehydrateData {
    let recent = if opts.force { 0 } else { 300 };
    run_dehydrate_with_recent(state, opts, recent).await
}

async fn run_dehydrate_with_recent(
    state: &Arc<State>,
    opts: DehydrateOpts,
    recent_secs: u64,
) -> DehydrateData {
    let now = now_secs();
    let cfg = state.dehydrate_cfg.lock().unwrap().clone();
    let idle_secs = opts.idle_secs.unwrap_or(cfg.idle_secs);
    let limit_spec = match &opts.cache_limit {
        Some(s) => CacheLimit::parse(s),
        None => cfg.limit,
    };
    if opts.cache_limit.is_some() && limit_spec.is_none() {
        let mut d = DehydrateData::default();
        d.dry_run = opts.dry_run;
        return d;
    }

    // 选定挂载点（一对一：一个挂载点 = 一个远端根，所以每个挂载只有一轮）
    let targets: Vec<(PathBuf, String, FsHandle, qxync_fuse::Notifier, CacheMode)> = {
        let g = state.mounts.lock().unwrap();
        let mut out = Vec::new();
        for m in g.values().filter(|m| {
            opts.mountpoint
                .as_ref()
                .map(|mp| m.info.mountpoint == *mp)
                .unwrap_or(true)
        }) {
            out.push((
                m.info.mountpoint.clone(),
                m.info.remote.clone(),
                m.handle.clone(),
                m.notifier.clone(),
                m.cache_mode,
            ));
        }
        out
    };

    let mut out = DehydrateData {
        dry_run: opts.dry_run,
        ..Default::default()
    };
    let manual_path = opts.path.clone();
    for (mp, remote_root, handle, notifier, _mode) in &targets {
        if let Some(p) = &manual_path {
            if !(p == remote_root
                || p.starts_with(&format!("{}/", remote_root.trim_end_matches('/'))))
            {
                continue;
            }
        }
        let cache_dir = handle.cache_dir().to_path_buf();
        let limit = resolve_limit(limit_spec, &cache_dir);
        out.limit_bytes = limit;
        let mapped = mmap_remotes(mp, remote_root);
        let policy = Policy {
            idle_secs,
            cache_limit: limit,
            recent_secs,
            now,
        };
        let cands = if let Some(p) = &manual_path {
            handle.candidate(p).into_iter().collect::<Vec<_>>()
        } else {
            handle.dehydrate_candidates()
        };
        let plan = qxync_core::dehydrate::plan(&cands, &policy, &mapped);
        if manual_path.is_some() && plan.targets.is_empty() && plan.blocked.is_empty() {
            continue;
        }
        for (path, why) in &plan.blocked {
            out.blocked.push((path.clone(), why.reason().to_string()));
            record_block(&state.dehydrate_stats, *why);
        }
        for c in &plan.targets {
            out.targets.push(c.remote.clone());
            if opts.dry_run {
                out.freed_bytes += c.hydrated_bytes;
                out.dehydrated += 1;
                continue;
            }
            let n = notifier.clone();
            match handle.dehydrate_now(&c.remote, &policy, mapped.contains(&c.remote), move |ino| {
                n.inval_inode(ino, 0, 0)
            }) {
                qxync_fuse::DehydrateOutcome::Freed(bytes) => {
                    out.dehydrated += 1;
                    out.freed_bytes += bytes;
                }
                qxync_fuse::DehydrateOutcome::Blocked(b) => {
                    out.blocked.push((c.remote.clone(), b.reason().to_string()));
                    record_block(&state.dehydrate_stats, b);
                }
                qxync_fuse::DehydrateOutcome::Failed(e) => {
                    out.blocked.push((c.remote.clone(), format!("失败: {e}")));
                    *state.dehydrate_stats.last_error.lock().unwrap() = Some(e);
                }
            }
        }
    }
    out.used_bytes = cache_info(state).used_bytes;
    // 累计统计（手动与后台共用；dry-run 不计数）
    let st = &state.dehydrate_stats;
    st.runs.fetch_add(1, Ordering::Relaxed);
    st.last_run_unix.store(now, Ordering::Relaxed);
    if !opts.dry_run {
        st.dehydrated.fetch_add(out.dehydrated, Ordering::Relaxed);
        st.freed.fetch_add(out.freed_bytes, Ordering::Relaxed);
    }
    out
}

fn record_block(stats: &Arc<DehydrateStats>, b: Block) {
    let c = match b {
        Block::Dirty => &stats.blocked_dirty,
        Block::PendingUpload => &stats.blocked_dirty,
        Block::Pinned | Block::Excluded => &stats.blocked_pinned,
        Block::Open => &stats.blocked_open,
        Block::Mapped => &stats.blocked_mapped,
        Block::InFlight => &stats.blocked_inflight,
        // 其它（不是文件/没内容/刚访问过）不单独计数
        _ => return,
    };
    c.fetch_add(1, Ordering::Relaxed);
}

/// `qxync dehydrate`：IPC 入口。
async fn dehydrate_cmd(
    state: &Arc<State>,
    mut opts: DehydrateOpts,
) -> Result<serde_json::Value, IpcError> {
    if opts.cache_limit.is_some()
        && CacheLimit::parse(opts.cache_limit.as_deref().unwrap()).is_none()
    {
        return Err(IpcError::new(
            ErrorKind::BadRequest,
            format!(
                "cache_limit 写法无效: {:?}（例：512M / 2G / 25%）",
                opts.cache_limit
            ),
        ));
    }
    // IPC 传进来的闲置/限额临时覆盖（下次后台扫描继续用环境变量的值）
    if let Some(idle) = opts.idle_secs {
        state.dehydrate_cfg.lock().unwrap().idle_secs = idle;
    }
    if !opts.all && opts.path.is_none() && opts.cache_limit.is_none() && opts.idle_secs.is_none() {
        // 没给范围：按当前配置扫一遍
        opts.all = true;
    }
    let out = run_dehydrate(state, opts).await;
    to_value(out)
}

/// 后台脱水：按 `QXYNC_DEHYDRATE_INTERVAL` 周期扫「闲置 + 限额」。
///
/// ★ M15/T6：带一代 [`EngineGen`] —— 换代时做完当前一轮就退。
fn spawn_dehydrator(state: Arc<State>, mut gen: EngineGen) -> tokio::task::JoinHandle<()> {
    let cfg = state.dehydrate_cfg.lock().unwrap().clone();
    if cfg.enabled() {
        tracing::info!(
            "自动脱水已启动：每 {}s 扫一次，闲置 ≥ {}s，限额 {:?}",
            cfg.interval_secs,
            cfg.idle_secs,
            cfg.limit
        );
    } else {
        tracing::info!(
            "自动脱水未启用（QXNYC_DEHYDRATE_IDLE=0 且未设 QXNYC_CACHE_LIMIT）；\
             手动 `qxync dehydrate` 可用，`qxync dehydrate --idle-secs N` 可动态开启"
        );
    }
    tokio::spawn(async move {
        loop {
            let interval = state.dehydrate_cfg.lock().unwrap().interval_secs.max(1);
            if !gen
                .sleep(Duration::from_secs(interval.clamp(5, 3600)))
                .await
            {
                tracing::info!("自动脱水退出（同步引擎已换代）");
                return;
            }
            let cfg = state.dehydrate_cfg.lock().unwrap().clone();
            if !cfg.enabled() {
                continue;
            }
            // 闲置阈值本身就是「耐心值」：设了 idle 就不再叠加 300s 保护窗口
            // （没设 idle 时按限额清，才需要「刚访问过」的保护）。
            let recent = if cfg.idle_secs > 0 { 0 } else { 300 };
            let out = run_dehydrate_with_recent(
                &state,
                DehydrateOpts {
                    path: None,
                    all: true,
                    idle_secs: Some(cfg.idle_secs),
                    cache_limit: None,
                    force: false,
                    dry_run: false,
                    mountpoint: None,
                },
                recent,
            )
            .await;
            if out.dehydrated > 0 {
                tracing::info!(
                    "自动脱水：清理 {} 个文件 / 释放 {} 字节（缓存现 {} 字节）",
                    out.dehydrated,
                    out.freed_bytes,
                    out.used_bytes
                );
                journal_log(
                    &state,
                    JournalEntry::ok(
                        "dehydrate",
                        "",
                        format!(
                            "自动释放空间：脱水 {} 个文件 / 释放 {} 字节",
                            out.dehydrated, out.freed_bytes
                        ),
                    )
                    .with_bytes(out.freed_bytes as i64),
                );
            }
        }
    })
}

// ---------------------------------------------------------------- ★ M8.2 同步任务

fn core_err(e: impl std::fmt::Display) -> IpcError {
    IpcError::new(ErrorKind::Io, e.to_string())
}

fn bad_req(msg: impl Into<String>) -> IpcError {
    IpcError::new(ErrorKind::BadRequest, msg.into())
}

/// 把落盘的任务与「此刻是否真的挂着」拼起来。
fn task_info_join(state: &Arc<State>, task: Task) -> TaskInfo {
    let mounted = {
        let g = state.mounts.lock().unwrap();
        g.keys().any(|k| k == &task.mountpoint)
    };
    TaskInfo {
        task,
        mounted,
        last_error: None,
    }
}

/// 卸载成功后把对应的任务标成停用（**只改登记，不动挂载点里的数据**）。
fn disable_task_by_mountpoint(mp: &Path) -> Result<(), String> {
    let paths = ConfigPaths::discover().map_err(|e| e.to_string())?;
    let (list, _) = Task::list(&paths);
    let want = std::fs::canonicalize(mp).unwrap_or_else(|_| mp.to_path_buf());
    for mut t in list {
        let cand = std::fs::canonicalize(&t.mountpoint).unwrap_or_else(|_| t.mountpoint.clone());
        if cand == want && t.enabled {
            t.enabled = false;
            t.save(&paths).map_err(|e| e.to_string())?;
            tracing::info!("M8.2 任务已停用: {}（挂载点 {}）", t.id, mp.display());
        }
    }
    Ok(())
}

/// 按任务登记的参数挂载（**复用既有 `mount()` 路径，不改 FUSE**）。
///
/// 一对一：任务里的 NAS 文件夹就是挂载点对应**唯一**的那个根；没写就跟 link 的
/// `home_root`（`Task::effective_root`）。
async fn mount_task(state: &Arc<State>, t: &Task) -> Result<serde_json::Value, IpcError> {
    std::fs::create_dir_all(&t.mountpoint)
        .map_err(|e| core_err(format!("建挂载点 {} 失败: {e}", t.mountpoint.display())))?;
    let remote = t.effective_root();
    mount(
        state,
        t.mountpoint.clone(),
        Some(remote),
        // ★ 任务里的缓存目录（None = 默认 $XDG_DATA_HOME/qxync/cache）
        t.cache_dir.clone(),
        t.threads.unwrap_or(4),
        t.auto_unmount,
        Duration::from_secs(t.hydrate_timeout_secs.unwrap_or(60)),
        t.read_write,
        t.delete_limit,
        Some(t.cache_mode.clone()),
        // ★ M8.4：任务上的冲突策略（5 选项）
        Some(t.conflict.clone()),
    )
    .await
}

/// `Request::Tasks` 的实现。
async fn tasks_cmd(
    state: &Arc<State>,
    action: &str,
    id: Option<String>,
    task: Option<Task>,
) -> Result<serde_json::Value, IpcError> {
    let paths = ConfigPaths::discover().map_err(core_err)?;
    match action {
        "list" => {
            let (list, bad) = Task::list(&paths);
            let infos: Vec<TaskInfo> = list.into_iter().map(|t| task_info_join(state, t)).collect();
            let empty = !has_any_task(&paths);
            let data = TasksData {
                tasks: infos,
                bad_files: bad
                    .into_iter()
                    .map(|(p, e)| (p.display().to_string(), e))
                    .collect(),
                empty,
                note: Some(if empty {
                    "还没有登记过任务；`mount` 时带 task/save_task 即可登记".into()
                } else {
                    "任务登记在 ~/.config/qxync/tasks/<id>.json；删除登记不会动挂载点里的数据"
                        .into()
                }),
            };
            to_value(data)
        }
        "get" => {
            let id = id.ok_or_else(|| bad_req("get 需要 id"))?;
            let t = Task::load(&paths, &id).map_err(core_err)?;
            to_value(task_info_join(state, t))
        }
        "save" => {
            let mut t = task.ok_or_else(|| bad_req("save 需要 task"))?;
            t.normalize().map_err(core_err)?;
            // ★ 提交前的「目的地冲突」检查：本地文件夹重复 / 嵌套 → 直接拒；
            //   NAS 文件夹重复 → 只作为 warnings 回给界面（只读挂同一个是合法用法）。
            let (others, _bad) = Task::list(&paths);
            let (errors, warnings) = t.conflict_report(&others);
            if !errors.is_empty() {
                return Err(IpcError::new(
                    ErrorKind::BadRequest,
                    format!("目的地冲突：{}", errors.join("；")),
                ));
            }
            let p = t.save(&paths).map_err(core_err)?;
            to_value(serde_json::json!({
                "saved": true,
                "path": p.display().to_string(),
                "task": t,
                "warnings": warnings,
            }))
        }
        "delete" => {
            let id = id.ok_or_else(|| bad_req("delete 需要 id"))?;
            let existed = Task::delete(&paths, &id).map_err(core_err)?;
            // ⚠️ 只删登记；挂载点里的文件、缓存、baseline 一律不动
            to_value(serde_json::json!({
                "deleted": existed,
                "id": id,
                "note": "只删任务登记，未改动挂载点里的任何数据",
            }))
        }
        "pause" => {
            let id = id.ok_or_else(|| bad_req("pause 需要 id"))?;
            let mut t = Task::load(&paths, &id).map_err(core_err)?;
            t.enabled = false;
            t.save(&paths).map_err(core_err)?;
            // 语义（见 docs/M8 §M8.2）：暂停 = 停用登记 + **卸载该挂载点**。
            // 已入队的上传在 umount 里会被排空（不丢改动）；其它任务完全不受影响。
            let mp = t.mountpoint.clone();
            let mounted = state
                .mounts
                .lock()
                .unwrap()
                .contains_key(&std::fs::canonicalize(&mp).unwrap_or_else(|_| mp.clone()));
            let mut unmounted = false;
            if mounted {
                umount(state, mp.clone()).await?;
                unmounted = true;
            }
            to_value(serde_json::json!({
                "id": id, "enabled": false, "unmounted": unmounted,
                "note": "暂停 = 停用登记并卸载该挂载点；已入队的上传会先排空",
            }))
        }
        "resume" => {
            let id = id.ok_or_else(|| bad_req("resume 需要 id"))?;
            let mut t = Task::load(&paths, &id).map_err(core_err)?;
            t.enabled = true;
            t.save(&paths).map_err(core_err)?;
            let mounted = state.mounts.lock().unwrap().contains_key(
                &std::fs::canonicalize(&t.mountpoint).unwrap_or_else(|_| t.mountpoint.clone()),
            );
            let mut m = serde_json::Value::Null;
            if !mounted {
                m = mount_task(state, &t).await?;
            }
            to_value(serde_json::json!({
                "id": id, "enabled": true, "remounted": !mounted, "mount": m,
            }))
        }
        "mount" => {
            let id = id.ok_or_else(|| bad_req("mount 需要 id"))?;
            let t = Task::load(&paths, &id).map_err(core_err)?;
            let m = mount_task(state, &t).await?;
            to_value(serde_json::json!({ "id": id, "mount": m }))
        }
        other => Err(bad_req(format!(
            "未知的 tasks action: {other:?}（可用 list/get/save/delete/pause/resume/mount）"
        ))),
    }
}

/// ★ M15/T5：daemon 启动时按 `mounts.json` 恢复挂载（用户无感）。
///
/// 与 M8.2 的 [`restore_tasks_on_start`] 的分工：
///
/// * `mounts.json` 记的是**上一次实际挂了什么**（现场）—— 包括手工 `qxync mount`
///   挂的、没登记进任务表的那些；
/// * `tasks/` 记的是**用户登记了哪些共享文件夹**（意图）。
///
/// 两份都读、现场优先。`mounts.json` 里的挂载点先占位，任务表里同名的跳过
/// （用户可能已经改过任务里的参数，让现场那份赢更贴近「重启前是什么样」）。
///
/// ## 三个不变量（M15/T5 改动点 3）
///
/// 1. **必须已登录** —— 调用方 [`run`] 那边已等过 `sid`，这里 `mount()` 自己
///    还有 `ensure_session` + `require_sid` 兜底，登录不上只会这批失败；
/// 2. **幂等** —— 已被别处挂上（或已经是挂载点）的跳过，不重复挂；
/// 3. **失败不阻塞** —— 单个失败只 WARN，继续恢复下一个，绝不让 daemon 起不来。
pub(crate) async fn restore_mounts_on_start(state: &Arc<State>, link_id: &str) {
    let paths = match ConfigPaths::discover() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("恢复挂载：读配置目录失败 {e}");
            return;
        }
    };
    // 现场：上一次实际挂了什么
    let (recorded, warns) = MountsFile::load(&paths);
    for w in warns {
        tracing::warn!("{w}");
    }
    let mut todo: Vec<MountRecord> = recorded.map(|f| f.mounts).unwrap_or_default();
    // 排序：挂载恢复是「一个一个挂」，顺序影响谁先占用连接/线程；按挂载点排
    // 让行为可复现（不然 HashMap 顺序会让日志每次都不一样）。
    todo.sort_by(|a, b| a.mountpoint.cmp(&b.mountpoint));

    if !todo.is_empty() {
        tracing::info!("恢复挂载：{} 个挂载点待恢复（link={link_id}）", todo.len());
    }
    let want = todo.len();
    let mut done = 0usize;
    for rec in &todo {
        let mp = &rec.mountpoint;
        // 不变量 2：已挂载就跳过。**顺手把记录补齐**（上次记的 remote 与现在挂的
        // 不一致，说明这期间用户改过 —— 此时不覆盖，只记一条 WARN）。
        if let Some(info) = state.mounts.lock().unwrap().get(mp) {
            if info.info.remote == rec.remote {
                tracing::info!("恢复挂载：{} 已在挂载，跳过", mp.display());
                done += 1;
            } else {
                tracing::warn!(
                    "恢复挂载：{} 已挂载但 remote 不同（{} vs 记录 {}），跳过",
                    mp.display(),
                    info.info.remote,
                    rec.remote
                );
            }
            continue;
        }
        // ★ M15/T6：挂载点在内核挂载表里、但**不在本进程的挂载表里** ——
        //   说明它是**另一个 qxyncd 进程**托管的（旧进程收到 SIGTERM 后进了
        //   「挂载守护模式」：停同步、但保留 FUSE 会话；见 [`hold_mounts`]）。
        //
        //   这种情况**本进程只做同步、不碰那个挂载点**：同步引擎全是纯 async
        //   任务、一样都不依赖 FUSE 会话（任务书 §0.2 已核实），所以完全能工作。
        //   而如果硬去挂，同一个路径会挂第二层 FUSE —— 内核只认最后那层，
        //   旧进程还握着一个已经没人应答的会话，用户看到的是更难查的现象。
        //
        //   ★ 记录**不覆盖**：mounts.json 记的是「上一次实际挂了什么」，
        //   那个挂载点现在**确实挂着**（只是不是本进程挂的），抹掉记录等于
        //   告诉下一个进程「这里没挂过」，等旧进程一挂它就永远消失了。
        if is_mounted(mp) {
            if is_qxync_mount(mp) {
                tracing::info!(
                    "恢复挂载：{} 已由其他 qxyncd 进程托管（FUSE 会话在它那里），\
                     本进程只做同步、不重复挂（记录保留）",
                    mp.display()
                );
            } else {
                // 路径相同但挂载者是别人的东西 —— 绝不覆盖、绝不卸载（T7 同一条边界）
                tracing::warn!(
                    "恢复挂载：{} 已被别的文件系统占用（subtype={:?}），跳过且不改动记录",
                    mp.display(),
                    mount_subtype(mp)
                );
            }
            continue;
        }
        // 挂载点目录可能已经被删了（用户清了目录树）—— 建回来，否则 mount() 必失败
        if let Err(e) = std::fs::create_dir_all(mp) {
            tracing::warn!(
                "恢复挂载：建挂载点 {} 失败（跳过，不影响其它）: {e}",
                mp.display()
            );
            continue;
        }
        match mount(
            state,
            mp.clone(),
            Some(rec.remote.clone()),
            // 记录里刻意不存 cache_dir（见 mounts.rs 模块文档）→ None = 默认目录
            None,
            rec.threads.max(1),
            rec.auto_unmount,
            Duration::from_secs(rec.hydrate_timeout_secs),
            rec.read_write,
            rec.delete_limit,
            Some(rec.cache_mode.clone()),
            Some(rec.conflict.clone()),
        )
        .await
        {
            Ok(_) => {
                tracing::info!("恢复挂载成功: {} → {}", mp.display(), rec.remote);
                done += 1;
            }
            // 不变量 3：只 WARN，继续下一个
            Err(e) => tracing::warn!(
                "恢复挂载失败（跳过，不影响其它挂载点）: {} → {}: {}",
                mp.display(),
                rec.remote,
                e.message
            ),
        }
    }
    if done > 0 {
        tracing::info!("恢复挂载：{done}/{want} 成功", want = want);
    }

    // 意图：用户登记了、但不在现场记录里的任务（第一次用、或上次没挂成功）
    restore_tasks_on_start(state, link_id).await;
}

/// ★ M8.2：daemon 启动时恢复 `enabled=true` 的任务。
///
/// **只有显式开启才跑**（`--restore-tasks` / `QXNYC_TASK_RESTORE=1`，
/// 见 [`Options::restore_tasks`]）：任务恢复会「凭空挂载」，会把上一次跑崩
/// 留下的挂载在重启时复活，打乱验收矩阵的前提。
///
/// ★ M15/T5 起只在 [`restore_mounts_on_start`] 的末尾被调用（现场恢复完再补意图），
/// 所以现场已有的挂载点不会再被这里重复挂一遍。
pub(crate) async fn restore_tasks_on_start(state: &Arc<State>, link_id: &str) {
    let paths = match ConfigPaths::discover() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("恢复任务：读配置目录失败 {e}");
            return;
        }
    };
    let (list, bad) = Task::list(&paths);
    for (p, e) in &bad {
        tracing::warn!("任务文件解析失败，已跳过 {}: {e}", p.display());
    }
    let todo: Vec<Task> = list
        .into_iter()
        .filter(|t| t.enabled)
        // ★ M15/T5：现场已经挂上的不再重复挂（`restore_mounts_on_start` 刚挂过）
        .filter(|t| !state.mounts.lock().unwrap().contains_key(&t.mountpoint))
        // ★ M15/T6：**别的 qxyncd 进程托管着**的也不挂。
        //   判据比 T7 宽一档：这里只需要「路径已是挂载点」，因为本函数只是「不去挂」，
        //   并不改动任何东西（不卸载、不写记录）—— 误判的代价只是少挂一个，
        //   而硬挂上去的代价是同一路径叠两层 FUSE。
        .filter(|t| !is_mounted(&t.mountpoint))
        .collect();
    if todo.is_empty() {
        tracing::info!("恢复任务：没有启用的任务待恢复（link={link_id}）");
        return;
    }
    tracing::info!("恢复任务：{} 个启用的任务待恢复", todo.len());
    for t in todo {
        match mount_task(state, &t).await {
            Ok(_) => tracing::info!("恢复任务成功: {} → {}", t.id, t.mountpoint.display()),
            Err(e) => tracing::warn!(
                "恢复任务失败（跳过，不影响其它任务）: {} → {}: {}",
                t.id,
                t.mountpoint.display(),
                e.message
            ),
        }
    }
}

// ---------------------------------------------------------------- ★ M8.3 同步日志

/// 往 journal 缓冲区塞一条（**非阻塞、不写库**）。
fn journal_log(state: &Arc<State>, e: JournalEntry) {
    let mut b = state.journal_buf.lock().unwrap();
    // 极端情况下（库一直写不进去）也不许无限吃内存
    if b.len() < 50_000 {
        b.push(e);
    }
}

/// 把一轮同步的结论翻译成 journal 条目。
///
/// 刻意**只记有内容的项**：一轮什么都没发生的轮询不写日志，否则日志会被噪声淹没
/// （默认 30 秒一轮 = 一天 2880 条空记录）。
fn journal_record_sync(state: &Arc<State>, r: &sync::SyncReport) {
    let mut n = 0usize;
    if r.events > 0 {
        journal_log(
            state,
            JournalEntry::ok(
                "remote_change",
                "",
                format!("拉到 {} 条事件（跳过 {} 条）", r.events, r.events_skipped),
            ),
        );
        n += 1;
    }
    if r.refreshed > 0 {
        journal_log(
            state,
            JournalEntry::ok(
                "remote_change",
                "",
                format!("远端改动刷新本地元数据 {} 项", r.refreshed),
            ),
        );
        n += 1;
    }
    if r.content_updating > 0 {
        journal_log(
            state,
            JournalEntry::ok(
                "remote_change",
                "",
                format!(
                    "远端改动、本地有水 {} 项：旧内容继续可读，后台更新新版本",
                    r.content_updating
                ),
            ),
        );
        n += 1;
    }
    if r.uploaded > 0 {
        journal_log(
            state,
            JournalEntry::ok(
                "upload",
                "",
                format!("本地改动上传回 NAS {} 项", r.uploaded),
            ),
        );
        n += 1;
    }
    if r.conflicts > 0 {
        journal_log(
            state,
            JournalEntry::ok(
                "conflict",
                "",
                format!("双方都改 → 生成 {} 个冲突副本", r.conflicts),
            ),
        );
        n += 1;
    }
    if r.deleted > 0 {
        journal_log(
            state,
            JournalEntry::ok(
                "delete",
                "",
                format!("按对账结果删除本地/远端条目 {} 项", r.deleted),
            ),
        );
        n += 1;
    }
    if r.deletes_blocked > 0 {
        journal_log(
            state,
            JournalEntry::blocked(
                "delete",
                "",
                format!(
                    "{} 项删除被熔断挡住（`qxync sync --once --force-deletes` 可放行一轮）",
                    r.deletes_blocked
                ),
            ),
        );
        n += 1;
    }
    for e in &r.errors {
        journal_log(state, JournalEntry::error("sync", "", e.clone()));
        n += 1;
    }
    if n == 0 && r.dirs_scanned > 0 {
        // 扫过但没变化：记一条很轻的，方便「更新中心」看出引擎活着
        journal_log(
            state,
            JournalEntry::ok(
                "scan",
                "",
                format!("对账完成：扫描 {} 个目录，无变化", r.dirs_scanned),
            ),
        );
    }
}

/// 后台批量落库 + 轮转。
///
/// ★ M15/T6：带一代 [`EngineGen`] —— 换代时退。
///
/// 退之前会把缓冲区里剩下的条目**再落一次库**：journal 记的是「发生过什么」，
/// 丢掉一批会让用户排障时凭空少掉一段历史（而那正是他们重载同步的原因）。
fn spawn_journal_flusher(state: Arc<State>, mut gen: EngineGen) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let max_rows = journal_max_rows();
        let max_age = journal_max_age_days();
        let trim_secs = journal_trim_secs();
        tracing::info!(
            "同步日志（journal）已启动：上限 {max_rows} 条 / {max_age} 天，每 {trim_secs}s 轮转一次"
        );
        let mut last_trim = Instant::now() - Duration::from_secs(trim_secs + 1);
        loop {
            if !gen.sleep(Duration::from_millis(500)).await {
                flush_journal_once(&state);
                tracing::info!("journal 落库退出（同步引擎已换代，缓冲区已落盘）");
                return;
            }
            flush_journal_once(&state);
            // 周期性轮转（默认 300s，QXNYC_JOURNAL_TRIM_SECS 可调）
            if last_trim.elapsed() >= Duration::from_secs(trim_secs) {
                last_trim = Instant::now();
                let g = state.sync_store.lock().unwrap();
                match g.store.journal_trim(max_rows, max_age) {
                    Ok(0) => {}
                    Ok(n) => tracing::debug!("journal 轮转：删除 {n} 条"),
                    Err(e) => tracing::warn!("journal 轮转失败: {e}"),
                }
            }
        }
    })
}

/// ★ M15/T6：把 journal 缓冲区里攒的条目落一次库。
///
/// 抽出来是为了让「换代退出」也能先把缓冲区落盘（丢掉一批历史日志会让用户
/// 在排障时凭空少看到一段，而重载同步往往正是他们排障的手段）。
///
/// 落库失败只 WARN —— journal 是**观察**数据，丢了不该影响任何别的行为。
fn flush_journal_once(state: &Arc<State>) {
    let batch: Vec<JournalEntry> = {
        let mut b = state.journal_buf.lock().unwrap();
        if b.is_empty() {
            Vec::new()
        } else {
            std::mem::take(&mut *b)
        }
    };
    if batch.is_empty() {
        return;
    }
    let g = state.sync_store.lock().unwrap();
    if let Err(e) = g.store.journal_add_batch(&batch) {
        tracing::warn!("journal 落库失败（丢弃 {} 条）: {e}", batch.len());
    }
}

/// `Request::Journal`：查 / 清空。
fn journal_cmd(
    state: &Arc<State>,
    limit: Option<usize>,
    since: Option<i64>,
    query: Option<String>,
    level: Option<String>,
    clear: Option<bool>,
) -> Result<serde_json::Value, IpcError> {
    let max_rows = journal_max_rows();
    let max_age = journal_max_age_days();
    let g = state.sync_store.lock().unwrap();
    let mut removed = 0usize;
    let cleared = clear.unwrap_or(false);
    if cleared {
        removed = g.store.journal_clear().map_err(core_err)?;
        tracing::info!("journal 已清空（{removed} 条）");
    }
    let lvl = level.unwrap_or_else(|| "all".into());
    let entries = g
        .store
        .journal_list(
            limit.unwrap_or(200),
            since,
            query.as_deref(),
            if lvl == "all" {
                None
            } else {
                Some(lvl.as_str())
            },
        )
        .map_err(core_err)?;
    let total = g.store.journal_count().map_err(core_err)?;
    let counts = g.store.journal_counts().map_err(core_err)?;
    to_value(JournalData {
        entries,
        total,
        counts,
        cleared,
        removed,
        limit_rows: max_rows,
        max_age_days: max_age,
        note: Some(format!(
            "日志只保留最近 {max_rows} 条 / {max_age} 天（QXNYC_JOURNAL_MAX_ROWS / _MAX_AGE_DAYS 可调）；清空日志不影响同步状态"
        )),
    })
}

// ================================================================ ★ M8.4
// 设置中心 / 冲突待裁决队列 / 文件三态 / 自动释放空间

/// 拼一个状态快照（`settings` 与 `settings_save` 共用）。
/// 设置落盘的**文件层**：校验代理 → 处理 autostart 桌面项 → 写 `settings.json`。
///
/// 空转待命与同步模式共用这一半，避免两边逻辑漂移。
/// 返回 `(落盘路径, 是否写了 autostart 桌面项, 解析好的代理)`。
fn save_settings_to_disk(
    paths: &ConfigPaths,
    s: &mut Settings,
    autostart_exe: Option<String>,
) -> Result<(PathBuf, bool, ProxySpec), IpcError> {
    s.normalize();
    // 手动代理缺服务器/缺用户名 → **明确报错**，不静默退回直连
    let proxy = s.proxy.resolve().map_err(|e| bad_req(e.to_string()))?;

    // 开机自启：只有真要用的时候才需要可执行文件路径
    let mut autostart_written = false;
    if s.launch_at_startup || autostart_exe.is_some() {
        let exe = match autostart_exe
            .clone()
            .map(PathBuf::from)
            .or_else(default_gui_exe)
        {
            Some(e) => e,
            None => {
                return Err(bad_req(
                    "开机自启需要 qxync-gui 的绝对路径：没传 autostart_exe，\
                     同目录下也没找到 qxync-gui（CLI 可显式传 GUI 路径）",
                ))
            }
        };
        s.apply_autostart(paths, &exe).map_err(core_err)?;
        autostart_written = s.launch_at_startup;
    } else {
        // 关掉自启：把桌面项删掉（不存在也算成功）
        s.apply_autostart(paths, &PathBuf::from("qxync-gui"))
            .map_err(core_err)?;
    }
    let saved_path = s.save(paths).map_err(core_err)?;
    Ok((saved_path, autostart_written, proxy))
}

/// 空转待命时的 `settings` / `settings_save` 返回。
///
/// **设置是纯本地文件，跟有没有 NAS 连接无关** —— 而且 GUI 的设置页（登录表单就在那一页）
/// 靠它渲染，所以空转时也必须能读能写，否则用户还没配连接就先看到一个报错。
fn idle_settings_data(paths: &ConfigPaths, saved: bool, note: Option<String>) -> SettingsData {
    let s = Settings::load(paths).unwrap_or_default();
    let proxy_url = match s.proxy.resolve() {
        Ok(ProxySpec::Manual { url, .. }) => Some(url),
        _ => None,
    };
    SettingsData {
        path: Settings::file(paths).display().to_string(),
        autostart_path: Settings::autostart_file(paths).display().to_string(),
        autostart_present: Settings::autostart_present(paths),
        saved,
        proxy_env: Settings::proxy_env(),
        proxy_url,
        note,
        settings: s,
    }
}

/// 空转待命时的 `settings_save`：只落盘，不刷新内存态（那时压根没有内存态）。
fn idle_settings_save(
    paths: &ConfigPaths,
    mut s: Settings,
    autostart_exe: Option<String>,
) -> Result<serde_json::Value, IpcError> {
    save_settings_to_disk(paths, &mut s, autostart_exe)?;
    to_value(idle_settings_data(
        paths,
        true,
        Some("设置已落盘（daemon 空转待命中：还没有配置 NAS 连接）".into()),
    ))
}

fn settings_data(state: &Arc<State>, saved: bool, note: Option<String>) -> SettingsData {
    let paths = ConfigPaths::discover().ok();
    let s = state.settings.lock().unwrap().clone();
    let proxy = state.proxy.lock().unwrap().clone();
    SettingsData {
        path: paths
            .as_ref()
            .map(|p| Settings::file(p).display().to_string())
            .unwrap_or_default(),
        autostart_path: paths
            .as_ref()
            .map(|p| Settings::autostart_file(p).display().to_string())
            .unwrap_or_default(),
        autostart_present: paths
            .as_ref()
            .map(Settings::autostart_present)
            .unwrap_or(false),
        saved,
        proxy_env: Settings::proxy_env(),
        proxy_url: match &proxy {
            ProxySpec::Manual { url, .. } => Some(url.clone()),
            _ => None,
        },
        note,
        settings: s,
    }
}

/// `Request::Settings`：只读当前设置。
fn settings_cmd(state: &Arc<State>) -> Result<serde_json::Value, IpcError> {
    to_value(settings_data(
        state,
        false,
        Some("settings.json 不存在时一切取默认值（代理=自动检测、不自动释放、不开机自启）".into()),
    ))
}

/// `Request::SettingsSave`：归一化 → 校验 → 落盘 → 刷新内存态（含 autostart 桌面项）。
fn settings_save_cmd(
    state: &Arc<State>,
    mut s: Settings,
    autostart_exe: Option<String>,
) -> Result<serde_json::Value, IpcError> {
    let paths = ConfigPaths::discover().map_err(core_err)?;
    let (saved_path, autostart_written, proxy) =
        save_settings_to_disk(&paths, &mut s, autostart_exe)?;
    *state.settings.lock().unwrap() = s.clone();
    *state.proxy.lock().unwrap() = proxy;
    journal_log(
        state,
        JournalEntry::ok(
            "settings",
            "",
            format!(
                "设置已保存：代理={}，自动释放={}，开机自启={}，通知={}",
                s.proxy.mode,
                if s.free_space.auto { "开" } else { "关" },
                if s.launch_at_startup { "开" } else { "关" },
                if s.desktop_notifications {
                    "开"
                } else {
                    "关"
                }
            ),
        ),
    );
    tracing::info!("M8.4 设置已保存: {}", saved_path.display());
    to_value(settings_data(
        state,
        true,
        Some(if autostart_written {
            "开机自启已写入 autostart 桌面项；代理改动手动模式后，已在跑的水合客户端要等下次挂载才生效".into()
        } else {
            "设置已落盘（代理改动对手动建立的连接要等下次挂载；LAN 监听地址改动要重启 daemon）"
                .into()
        }),
    ))
}

/// 找 GUI 可执行文件：优先与当前进程同目录的 `qxync-gui`。
fn default_gui_exe() -> Option<PathBuf> {
    let cur = std::env::current_exe().ok()?;
    let cand = cur.parent()?.join("qxync-gui");
    cand.is_file().then_some(cand)
}

/// `Request::Decisions`：冲突待裁决队列。
fn decisions_cmd(
    state: &Arc<State>,
    action: &str,
    id: Option<String>,
    resolution: Option<String>,
) -> Result<serde_json::Value, IpcError> {
    let mut removed = 0usize;
    {
        let g = state.sync_store.lock().unwrap();
        match action {
            "list" => {}
            "resolve" => {
                let id = id.ok_or_else(|| bad_req("resolve 需要 id"))?;
                let res = resolution.unwrap_or_default();
                if !matches!(res.as_str(), "keep_local" | "keep_remote" | "keep_both") {
                    return Err(bad_req(format!(
                        "resolution 只能是 keep_local / keep_remote / keep_both，收到 {res:?}"
                    )));
                }
                if !g
                    .store
                    .decision_resolve(&id, Some(&res))
                    .map_err(core_err)?
                {
                    return Err(IpcError::new(
                        ErrorKind::BadRequest,
                        format!("没有这条待裁决：{id}"),
                    ));
                }
                journal_log(
                    state,
                    JournalEntry::ok(
                        "conflict",
                        "",
                        format!("用户裁决冲突 {id} → {res}（下一轮同步执行）"),
                    ),
                );
            }
            "clear" => {
                removed = g.store.decisions_clear().map_err(core_err)?;
                journal_log(
                    state,
                    JournalEntry::ok(
                        "conflict",
                        "",
                        format!("清空冲突待裁决队列（{removed} 条，未动文件）"),
                    ),
                );
            }
            other => return Err(bad_req(format!("未知 action {other:?}"))),
        }
    }
    let g = state.sync_store.lock().unwrap();
    let rows = g.store.decisions().map_err(core_err)?;
    let decisions: Vec<DecisionInfo> = rows
        .iter()
        .map(|r| DecisionInfo {
            id: r.id.clone(),
            path: r.path.clone(),
            task_id: r.task_id.clone(),
            local_size: r.local_size,
            local_mtime: r.local_mtime,
            remote_size: r.remote_size,
            remote_mtime: r.remote_mtime,
            is_dir: r.is_dir,
            created_unix: r.created_unix,
            resolution: r.resolution.clone(),
        })
        .collect();
    let pending = decisions.iter().filter(|d| d.resolution.is_none()).count();
    let resolved = decisions.len() - pending;
    to_value(DecisionsData {
        action: action.to_string(),
        decisions,
        pending,
        resolved,
        removed,
        note: Some(
            "「每个文件都问我」的冲突会停在这里：文件两边都不动，裁决后由下一轮同步执行".into(),
        ),
    })
}

/// ★ T8：把 `decisions` 表的行压成「路径 → 待裁决冲突数」。
///
/// ## 只数**待裁决**的
/// `resolution IS NULL` 才算待裁决。已裁决的行还留在表里（那是历史留痕，
/// 用户回头还能看「当时选了哪个」），算进去会让界面显示「1 个冲突待处理」
/// 而其实早就处理完了 —— 这种谎报比不显示更糟。
///
/// ## 查表失败 → `None` 而不是空表
/// 空表的含义是「没有冲突」，会把 `in_sync` 判成 `true`。查不到就必须说
/// 「查不到」—— 调用方据此把 `conflicts_known` 置 `false`，界面显示「未知」。
fn pending_conflicts_by_path(
    rows: qxync_core::Result<Vec<qxync_core::store::DecisionRow>>,
) -> Option<std::collections::HashMap<String, usize>> {
    match rows {
        Ok(rows) => {
            let mut m = std::collections::HashMap::new();
            for r in rows {
                if r.resolution.is_none() {
                    *m.entry(r.path).or_insert(0) += 1;
                }
            }
            Some(m)
        }
        Err(e) => {
            tracing::warn!("读待裁决冲突失败，file_states 的同步维度会标成未知: {e}");
            None
        }
    }
}

/// `Request::FileStates`：目录里每个条目的三态（仅在线 / 本地可用 / 始终可用）。
async fn file_states_cmd(state: &Arc<State>, path: String) -> Result<serde_json::Value, IpcError> {
    // 该目录属于哪个挂载点/远端根？没有挂载就没有本地缓存可谈。
    let hit = {
        let g = state.mounts.lock().unwrap();
        g.values().find_map(|m| {
            let r = m.info.remote.trim_end_matches('/');
            if path == r || path.starts_with(&format!("{r}/")) {
                Some((m.handle.clone(), r.to_string(), m.info.mountpoint.clone()))
            } else {
                None
            }
        })
    };
    let Some((handle, root, mp)) = hit else {
        return to_value(FileStatesData {
            path,
            note: Some("该目录不在任何挂载点下：三态（本地缓存情况）只有挂载后才算得出来".into()),
            ..Default::default()
        });
    };
    ensure_session(state).await?;
    let dir = path.trim_end_matches('/').to_string();
    let entries = with_client!(state, |c| c.list(&dir))?;
    // ★ T8：一次把整个目录的待裁决冲突查出来（`decisions` 表，同一个 SQLite 连接）。
    //   查表失败 → `None` = 「没查到」，如实传给 FUSE 侧让 `conflicts_known=false`，
    //   绝不把「查不到」当成「没有冲突」。
    let decisions: Option<std::collections::HashMap<String, usize>> = {
        let g = state.sync_store.lock().unwrap();
        pending_conflicts_by_path(g.store.decisions())
    };
    let mut out = Vec::new();
    let (mut online, mut local, mut always) = (0usize, 0usize, 0usize);
    // ★ T8：同步维度汇总
    let (mut n_in_sync, mut n_out_sync, mut n_unknown, mut n_transfer) =
        (0usize, 0usize, 0usize, 0usize);
    for e in entries {
        let remote = format!("{}/{}", dir, e.filename);
        let cand = handle.candidate(&remote);
        let pin = cand
            .as_ref()
            .map(|c| c.pin.clone())
            .unwrap_or_else(|| "unspecified".into());
        let hydrated_bytes = cand.as_ref().map(|c| c.hydrated_bytes).unwrap_or(0);
        let dirty = cand.as_ref().map(|c| c.dirty).unwrap_or(false);
        let state_str = if pin == "pinned" {
            always += 1;
            "always"
        } else if !e.isfolder && hydrated_bytes > 0 {
            local += 1;
            "local"
        } else {
            online += 1;
            "online"
        };
        // ★ T8：同步维度 = dirty / 待传作业 / 待裁决冲突 三源汇总（派生值）
        let conflicts = decisions.as_ref().and_then(|m| m.get(&remote).copied());
        let fs = handle.file_states(&remote, conflicts);
        match &fs {
            Some(s) if s.in_sync => n_in_sync += 1,
            Some(_) => n_out_sync += 1,
            None => n_unknown += 1,
        }
        if fs.as_ref().is_some_and(|s| s.progress.is_some()) {
            n_transfer += 1;
        }
        out.push(FileStateInfo {
            name: e.filename.clone(),
            remote: remote.clone(),
            is_dir: e.isfolder,
            size: e.filesize,
            hydrated_bytes,
            state: state_str.to_string(),
            pin,
            dirty,
            hidden: handle.hidden(&remote, e.isfolder).is_some(),
            in_sync: fs.as_ref().map(|s| s.in_sync),
            pending_upload: fs.as_ref().is_some_and(|s| s.pending_upload),
            progress: fs.as_ref().and_then(|s| s.progress),
            conflicts: fs.as_ref().map(|s| s.conflicts).unwrap_or(0),
            conflicts_known: fs.as_ref().is_some_and(|s| s.conflicts_known),
        });
    }
    to_value(FileStatesData {
        path: dir,
        mountpoint: Some(mp.display().to_string()),
        root: Some(root),
        entries: out,
        online,
        local,
        always,
        note: Some("仅在线 ← 无本地内容；本地可用 ← 已缓存部分/全部；始终可用 ← pin=pinned".into()),
        in_sync: n_in_sync,
        out_of_sync: n_out_sync,
        unknown: n_unknown,
        transferring: n_transfer,
    })
}

/// 量空间用的路径：`statvfs` 只关心**文件系统**，所以目录还不存在时沿父目录往上找第一个存在的。
///
/// 为什么需要：刚装好、还没挂载过任何任务时缓存目录并不存在，
/// 直接 `statvfs` 会 `ENOENT` —— 那会让「释放空间」页与自动释放任务一起失效（踩过）。
fn space_probe_path(dir: &std::path::Path) -> PathBuf {
    let mut cur = dir.to_path_buf();
    loop {
        if cur.exists() {
            return cur;
        }
        match cur.parent() {
            Some(p) if p != cur => cur = p.to_path_buf(),
            _ => return PathBuf::from("/"),
        }
    }
}

/// 自动释放的「刚访问过」保护窗口秒数（默认 300；`QXNYC_AUTO_FREE_RECENT` 只在验收里调）。
fn auto_free_recent_secs() -> u64 {
    std::env::var("QXNYC_AUTO_FREE_RECENT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300)
}

/// ★ M8.4：自动释放空间的后台任务。
///
/// **与手动脱水走同一条路**（`run_dehydrate_with_recent`）—— 也就是说 M3 的安全检查链
/// （dirty / pending / pinned / excluded / open / mapped / in-flight）一个都没绕过。
/// 本函数只负责：量空间 → 判定该不该跑 → 把判定翻译成一次脱水调用。
fn spawn_auto_free(state: Arc<State>, mut gen: EngineGen) -> tokio::task::JoinHandle<()> {
    let interval = std::env::var("QXNYC_AUTO_FREE_INTERVAL")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(60)
        .clamp(1, 3600);
    tokio::spawn(async move {
        loop {
            if !gen.sleep(Duration::from_secs(interval)).await {
                tracing::info!("自动释放空间退出（同步引擎已换代）");
                return;
            }
            let cfg = state.settings.lock().unwrap().free_space.clone();
            if !cfg.auto {
                continue;
            }
            let dir = space_probe_path(&first_cache_dir(&state));
            let space = match qxync_core::freespace::probe(&dir) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("自动释放空间：量 {} 失败: {e}", dir.display());
                    continue;
                }
            };
            let used = cache_info(&state).used_bytes;
            // 「按频率」也必须留出耐心值：绝不允许 idle=0 变成「把所有能脱的都脱掉」
            let idle = state.dehydrate_cfg.lock().unwrap().idle_secs.max(3600);
            let last = state.auto_free_last.load(Ordering::Relaxed);
            let d = qxync_core::freespace::decide(&space, used, &cfg, last, now_secs(), idle);
            if !d.run {
                tracing::debug!("自动释放空间未触发：{}", d.reason);
                continue;
            }
            tracing::info!(
                "自动释放空间触发：{}（缓存已用 {} 字节，文件系统可用 {}%）",
                d.reason,
                used,
                d.avail_pct
            );
            // 「刚访问过」保护窗口：默认 300s（M3 既有行为）；
            // 验收可以用 QXNYC_AUTO_FREE_RECENT=0 把它关掉，观察低空间触发（**只是测试旋钮**）。
            let recent = if d.idle_secs > 0 {
                0
            } else {
                auto_free_recent_secs()
            };
            // ⚠ 限额 0 的语义是「能清多少清多少」，但 `CacheLimit::parse("0")` 会判为无效写法
            //   而让整轮变成**空操作**（踩过）。所以这里夹到最小 1 字节 —— 与「腾不出那么多」等价。
            let limit = d.cache_limit_bytes.map(|b| b.max(1).to_string());
            let out = run_dehydrate_with_recent(
                &state,
                DehydrateOpts {
                    path: None,
                    all: true,
                    idle_secs: Some(d.idle_secs),
                    cache_limit: limit,
                    force: false,
                    dry_run: false,
                    mountpoint: None,
                },
                recent,
            )
            .await;
            state.auto_free_last.store(now_secs(), Ordering::Relaxed);
            let blocked: u64 = out.blocked.len() as u64;
            tracing::info!(
                "自动释放空间完成：脱水 {} 个 / 释放 {} 字节 / 被挡下 {} 个（{}）",
                out.dehydrated,
                out.freed_bytes,
                blocked,
                d.reason
            );
            journal_log(
                &state,
                JournalEntry::ok(
                    "dehydrate",
                    "",
                    format!(
                        "自动释放空间：{} → 脱水 {} 个 / 释放 {} 字节 / 被挡下 {} 个",
                        d.reason, out.dehydrated, out.freed_bytes, blocked
                    ),
                )
                .with_bytes(out.freed_bytes as i64),
            );
        }
    })
}

/// `Request::Space`：释放空间状态；`now=true` 顺带执行「立即释放空间」。
///
/// ⚠ 「立即释放空间」用的也是 [`run_dehydrate_with_recent`] —— **没有旁路**。
async fn space_cmd(state: &Arc<State>, now: bool) -> Result<serde_json::Value, IpcError> {
    let cfg = state.settings.lock().unwrap().free_space.clone();
    let dir = space_probe_path(&first_cache_dir(state));
    let injected = std::env::var("QXNYC_TEST_FAKE_STATVFS")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    let space = qxync_core::freespace::probe(&dir).map_err(core_err)?;
    let used = cache_info(state).used_bytes;
    let idle = state.dehydrate_cfg.lock().unwrap().idle_secs.max(3600);
    let last = state.auto_free_last.load(Ordering::Relaxed);
    let d = qxync_core::freespace::decide(&space, used, &cfg, last, now_secs(), idle);

    let mut data = SpaceData {
        fs_path: dir.display().to_string(),
        fs_total: space.total,
        fs_avail: space.avail,
        fs_free: space.free,
        fs_avail_pct: space.avail_pct(),
        cache_used_bytes: used,
        auto: cfg.auto,
        mode: cfg.mode.clone(),
        below_pct: cfg.below_pct,
        every_hours: cfg.every_hours,
        would_run: d.run,
        reason: d.reason.clone(),
        last_run_unix: last,
        injected,
        ..Default::default()
    };

    if now {
        // Free Up Space Now：按判定结果跑（`below_pct` 模式只在真低于阈值时才腾；
        // 用户点「立即」时若空间充足，就按「闲置 ≥ 1 小时」清一遍可清的）。
        let (limit, idle_secs) = if d.run {
            (d.cache_limit_bytes.map(|b| b.to_string()), d.idle_secs)
        } else {
            (None, idle)
        };
        let recent = if idle_secs > 0 { 0 } else { 300 };
        let out = run_dehydrate_with_recent(
            state,
            DehydrateOpts {
                path: None,
                all: true,
                idle_secs: Some(idle_secs),
                cache_limit: limit,
                force: false,
                dry_run: false,
                mountpoint: None,
            },
            recent,
        )
        .await;
        data.ran = true;
        data.dehydrated = out.dehydrated;
        data.freed_bytes = out.freed_bytes;
        data.blocked = out.blocked.clone();
        // 「立即」也算一次触发：按频率模式要重置计时，否则会连着触发
        state.auto_free_last.store(now_secs(), Ordering::Relaxed);
        data.last_run_unix = now_secs();
        data.cache_used_bytes = cache_info(state).used_bytes;
        journal_log(
            state,
            JournalEntry::ok(
                "dehydrate",
                "",
                format!(
                    "立即释放空间：脱水 {} 个 / 释放 {} 字节 / 被挡下 {} 个",
                    data.dehydrated,
                    data.freed_bytes,
                    data.blocked.len()
                ),
            )
            .with_bytes(data.freed_bytes as i64),
        );
    }

    data.note = Some(if injected {
        "⚠ 正在使用 QXNYC_TEST_FAKE_STATVFS 注入的剩余空间（验收模式）".into()
    } else {
        "脱水永远走 M3 的安全检查链：dirty / 待上传 / pinned / excluded / 打开中 / mmap / 传输中 一律跳过".into()
    });
    to_value(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_with_sid(sid: Option<&str>) -> Arc<Client> {
        let link = LinkConfig {
            id: "t".into(),
            host: "nas.invalid".into(),
            port: 9834,
            https: true,
            insecure: true,
            user: "test1".into(),
            ipv4_only: false,
            exclude: Vec::new(),
            filter_temp: true,
            peer_listen: None,
            peer_name: None,
        };
        let c = Client::new(&link).unwrap();
        if let Some(s) = sid {
            c.set_sid(s);
        }
        Arc::new(c)
    }

    /// ★ M10：只给「sid 还不一样」的客户端推 —— 相同的不要重复推（也别谎报推了几个）。
    #[test]
    fn push_sid_only_touches_stale_clients() {
        let fresh = client_with_sid(Some("new"));
        let stale = client_with_sid(Some("old"));
        let empty = client_with_sid(None);
        let pushed = push_sid_to(
            "new",
            vec![fresh.clone(), stale.clone(), empty.clone()].into_iter(),
        );
        assert_eq!(pushed, 2, "只有 old / none 需要推");
        assert_eq!(fresh.sid().as_deref(), Some("new"));
        assert_eq!(stale.sid().as_deref(), Some("new"));
        assert_eq!(empty.sid().as_deref(), Some("new"));

        // 再推一次：应该一个都不动
        assert_eq!(
            push_sid_to(
                "new",
                vec![fresh.clone(), stale.clone(), empty.clone()].into_iter()
            ),
            0
        );

        // 登出：挂载点的 sid 被清掉
        stale.clear_sid();
        assert_eq!(stale.sid(), None);
    }

    /// ★ M10：换账号时不能把新 sid 推给旧账号的挂载点（节点表还是旧账号的树）。
    #[test]
    fn sid_push_targets_skips_mounts_of_another_account() {
        let mine = client_with_sid(Some("old"));
        let theirs = client_with_sid(Some("old"));
        let unknown = client_with_sid(Some("old"));
        let (targets, skipped) = sid_push_targets(
            vec![
                ("test1".to_string(), mine.clone()),
                ("other".to_string(), theirs.clone()),
                (String::new(), unknown.clone()),
            ]
            .into_iter(),
            "test1",
        );
        assert_eq!(targets.len(), 2, "同账号 + 未知账号都要推");
        assert_eq!(skipped, 1, "另一个账号的要跳过");
        assert_eq!(push_sid_to("new", targets.into_iter()), 2);
        assert_eq!(mine.sid().as_deref(), Some("new"));
        assert_eq!(unknown.sid().as_deref(), Some("new"));
        assert_eq!(theirs.sid().as_deref(), Some("old"), "别人的 sid 不许动");
    }

    /// ★ M10：保活的分支 —— 没登录不碰 NAS；活着不动；失效就重登；网络抖动别把登录接口打爆。
    #[test]
    fn keeper_step_branches() {
        assert_eq!(keeper_step(false, &Ok(false)), KeeperStep::Idle);
        assert_eq!(
            keeper_step(false, &Err(CoreError::Transport("断网".into()))),
            KeeperStep::Idle
        );
        assert_eq!(keeper_step(true, &Ok(true)), KeeperStep::Ok);
        assert_eq!(keeper_step(true, &Ok(false)), KeeperStep::Relogin);
        assert_eq!(
            keeper_step(true, &Err(CoreError::Transport("断网".into()))),
            KeeperStep::ProbeFailed
        );
    }

    /// ★ M10：4/5 号 status 也要被认成「会话失效」——`map_err` 把它归到 `ErrorKind::Auth`，
    /// 轮询据此决定重登（以前它落进泛化的 `Status`，sid 死了就每轮都失败）。
    #[test]
    fn map_err_marks_session_status_as_auth() {
        let e = map_err(CoreError::status(4, "get_list"));
        assert_eq!(e.kind, ErrorKind::Auth);
        let e = map_err(CoreError::status(5, "stat"));
        assert_eq!(e.kind, ErrorKind::Auth);
        // 日志缺失（-17）不是鉴权问题
        let e = map_err(CoreError::status(-17, "qbox_get_sync_log"));
        assert_eq!(e.kind, ErrorKind::Status);
    }

    // ---------------------------------------------------------- ★ M15/T6 退出原因

    /// ★ M15/T6 验收 3：**`qxync daemon stop` 仍然真正卸载挂载**。
    /// 这是 T6 最容易做错的一条 —— 一旦「统一保留挂载」，`daemon stop` 就再也不
    /// 关不掉挂载点了。
    #[test]
    fn t6_exit_reason_dispatch_unmounts_only_on_explicit_shutdown() {
        assert!(
            ExitReason::IpcShutdown.should_unmount(),
            "IPC shutdown = 用户明确要关掉，必须真卸载"
        );
        assert!(
            ExitReason::CtrlC.should_unmount(),
            "Ctrl-C 同样是「我要关掉」，必须真卸载"
        );
        assert!(
            !ExitReason::Sigterm.should_unmount(),
            "★ SIGTERM（systemd stop/restart 都发它）必须**保留挂载**"
        );
        // 三个来源互不相同 —— 正是这个区分让 T6 成立
        assert_ne!(ExitReason::IpcShutdown, ExitReason::CtrlC);
        assert_ne!(ExitReason::IpcShutdown, ExitReason::Sigterm);
        // label 只用于日志，但要有区分度（排障时全靠它）
        assert_ne!(ExitReason::IpcShutdown.label(), ExitReason::Sigterm.label());
    }

    /// ★ M15/T6：代次令牌 —— bump 之后旧代任务必须立刻知道该退，
    /// 而**在 bump 之后才创建的新代任务不能一启动就误判成「已被换代」**。
    ///
    /// 后半条是最要命的：如果 `gen()` 不把当前值标记成「已看到」，那么
    /// `reload_sync_engine` 里的顺序（先 bump、再 `start_sync_engine`）会让
    /// 每一个新任务在第一次 `select!` 就立刻退出 —— 重载变成「同步静悄悄地
    /// 再也不工作」。这种故障没有任何报错，只表现为「同步不动了」，最难查。
    #[tokio::test]
    async fn t6_epoch_bump_retires_old_generation_only() {
        let epoch = SyncEpoch::new();
        let mut old = epoch.gen();

        // 没换代时：通知挂着不醒（否则 select! 会立刻全选它）
        tokio::select! {
            _ = old.changed() => panic!("没换代就收到通知，代次机制坏了"),
            _ = tokio::time::sleep(Duration::from_millis(30)) => {}
        }

        // —— 复现 `reload_sync_engine` 的真实顺序：bump → 等旧的退 → 起新的 ——
        assert_eq!(epoch.bump(), 1, "第一次 bump 应为 1");
        // 旧代立刻收到通知
        tokio::time::timeout(Duration::from_millis(500), old.changed())
            .await
            .expect("旧代任务没收到换代通知");

        let mut fresh = epoch.gen();
        // ★ 新代**不能**收到：那会让刚拉起的新 poller 立即退出
        tokio::select! {
            _ = fresh.changed() => panic!("新代误判成已换代：重载会把新任务也杀掉"),
            _ = tokio::time::sleep(Duration::from_millis(30)) => {}
        }

        // 再 bump 一次，两个都该收到（新代这时确实是旧代了）
        assert_eq!(epoch.bump(), 2, "第二次 bump 应为 2");
        tokio::time::timeout(Duration::from_millis(500), fresh.changed())
            .await
            .expect("第二次 bump 后新代也该收到通知");
    }

    /// ★ M15/T6：`EngineGen::sleep` 在换代时要**立刻**醒（不能睡满整个间隔）。
    ///
    /// 这一条是「`systemctl reload` 之后同步真的重载了」的关键：poller 间隔
    /// 默认 30s、脱水可到 3600s，要是它们睡满才退，重载等于要等一小时。
    #[tokio::test]
    async fn t6_engine_sleep_wakes_immediately_on_epoch_change() {
        let epoch = SyncEpoch::new();
        let mut gen = epoch.gen();
        let long = Duration::from_secs(3600);
        // 换代后 sleep 必须返回 false，且几乎立刻返回
        epoch.bump();
        let ok = tokio::time::timeout(Duration::from_millis(500), gen.sleep(long))
            .await
            .expect("换代没唤醒 sleep，重载会被拖到下一个轮询周期");
        assert!(!ok, "换代后 sleep 应报告「该退休了」");
    }

    /// ★ M15/T6：没换代时 `sleep` 要老老实实睡满并返回 true。
    /// 少了这条，「重载」就会变成「刚启动就被杀掉」。
    #[tokio::test]
    async fn t6_engine_sleep_sleeps_through_when_epoch_is_stable() {
        let epoch = SyncEpoch::new();
        let mut gen = epoch.gen();
        let ok = gen.sleep(Duration::from_millis(40)).await;
        assert!(ok, "代次没变时 sleep 应睡满并返回 true");
    }

    // ---------------------------------------------------------- ★ M15/T7 僵尸清理

    /// ★ M15/T7 验收 2：**挂载点上有别的文件系统时不会被误清**。
    ///
    /// 这是 T7 最关键的安全断言：`subtype` 不是 `fuse.qxync` 一律不动 ——
    /// 路径相同但挂载者是别人的情况很现实，卸掉别人的挂载是不可逆的数据事故。
    #[test]
    fn t7_zombie_action_unmounts_only_our_own_dead_mounts() {
        // 我们挂的 + 探针确认 ENOTCONN → 清
        assert_eq!(
            zombie_action(true, true),
            ZombieAction::Unmount,
            "自己挂的僵尸必须能清掉，否则 kill -9 之后永远自愈不了"
        );
        // ★ 不是 qxync 挂的 → 不动（哪怕它也已经 ENOTCONN）
        assert_eq!(
            zombie_action(true, false),
            ZombieAction::Skip(Some("不是 qxync 挂的，绝不碰")),
            "别人的文件系统绝不能被 lazy 卸载"
        );
        // 没挂载 → 不动（那是「记录有过、现在没挂」的正常状态，不是僵尸）
        assert_eq!(
            zombie_action(false, true),
            ZombieAction::Skip(None),
            "没挂载就没得清"
        );
        assert_eq!(
            zombie_action(false, false),
            ZombieAction::Skip(None),
            "没挂载 + 别人的 = 还是不动"
        );
    }

    /// ★ M15/T7：探针必须是**唯一**的判活手段，且只在「已经判定可能是我们自己的
    /// 僵尸」时才写 —— 因为探针会往挂载点里写文件，对别人的文件系统写一次就是
    /// 一次不该有的副作用。`zombie_action` 返回 `Skip` 时不该走到探针。
    #[test]
    fn t7_probe_is_only_reached_for_our_own_mounts() {
        // subtype 不是 qxync 时，判定在探针之前就返回了
        assert!(matches!(zombie_action(true, false), ZombieAction::Skip(_)));
        // 自己挂的才继续走探针（Unmount 分支的语义）
        assert_eq!(zombie_action(true, true), ZombieAction::Unmount);
    }

    /// ★ M15/T6/T7：subtype 判定只认 `fuse.qxync`。
    ///
    /// `Subtype("qxync")` 在 `/proc/self/mounts` 里呈现为 `fuse.qxync`；
    /// 拿不准时一律判「不是自己的」—— 误判的代价（漏清一个僵尸）远小于
    /// 反过来的代价（卸掉别人的挂载）。
    #[test]
    fn t7_only_fuse_qxync_counts_as_ours() {
        assert!(is_qxync_type("fuse.qxync"));
        // 常见的别的东西，一律不是自己的
        for other in ["tmpfs", "ext4", "fuse.sshfs", "overlay", "fuse", ""] {
            assert!(!is_qxync_type(other), "{other:?} 不该被当成 qxync 挂载");
        }
    }

    /// ★ M15/T7/T6：`/proc/self/mounts` 的解析 —— 路径含空格时内核写成 `\040`。
    ///
    /// 用真实的 `/proc/self/mounts` 行做输入（不是手编的），因为解析错列的后果是
    /// 「把 fstype 读成挂载点」→ 判定全错。
    #[test]
    fn t7_mount_subtype_parses_proc_mounts_line() {
        let line = "qxync /home/kami/qxync fuse.qxync rw,nosuid,nodev,relatime 0 0";
        assert_eq!(
            parse_mounts_line(line, "/home/kami/qxync"),
            Some("fuse.qxync".into())
        );

        // 含空格的挂载点：内核把空格写成 \040
        let line = "qxync /home/kami/my\\040mount fuse.qxync rw 0 0";
        assert_eq!(
            parse_mounts_line(line, "/home/kami/my mount"),
            Some("fuse.qxync".into()),
            "含空格的挂载点必须能对上（八进制转义还原）"
        );

        // 别的挂载点
        assert_eq!(parse_mounts_line(line, "/other"), None);
        // 别的文件系统
        let line = "/dev/sda1 /home/kami/qxync ext4 rw,relatime 0 0";
        assert_eq!(
            parse_mounts_line(line, "/home/kami/qxync"),
            Some("ext4".into())
        );
        // 残行不能 panic（`/proc` 读出来的东西不保证格式）
        assert_eq!(parse_mounts_line("", "/x"), None);
        assert_eq!(parse_mounts_line("only-one-field", "/x"), None);
        assert_eq!(parse_mounts_line("a b", "/x"), None);
    }

    /// ★ M15/T6：`mounts_persist` 的「带上别人托管的记录」逻辑 ——
    /// 用真实 `MountsFile` 走一遍，保证 T6 与 T5 的语义接得上。
    ///
    /// 关键点：那些记录不能被抹掉。抹掉了，托管的进程一挂，挂载点就永远消失
    /// （T5 的「启动自动重挂」会以为那里没挂过）。
    #[test]
    fn t6_externally_held_records_keeps_unmounted_records_of_other_hosts() {
        let p = tmp_paths("t6-held");
        // 磁盘上有一条记录，但它此刻**不在内核挂载表里**（本机没有真挂载）
        let mut f = MountsFile::default();
        f.put(mount_rec("/home/kami/qxync"));
        f.save(&p).unwrap();

        let mine = vec![mount_rec("/home/kami/other")];
        let held = externally_held_records(&p, &mine);
        assert!(
            held.is_empty(),
            "没挂着的记录不算「被托管」，不该被带进写盘（那会让已卸载的挂载点复活）；\
             got {held:?}"
        );
    }

    /// ★ M15/T6：挂载表里已有的记录不会被重复带一遍（去重）。
    #[test]
    fn t6_externally_held_records_does_not_duplicate_own_records() {
        let p = tmp_paths("t6-dup");
        let mine = vec![mount_rec("/home/kami/qxync")];
        let held = externally_held_records(&p, &mine);
        for h in &held {
            assert!(
                !mine.iter().any(|m| m.mountpoint == h.mountpoint),
                "{} 既是自己的又被当成别人的，重复了",
                h.mountpoint.display()
            );
        }
    }

    /// ★ T8：待裁决冲突只数 `resolution IS NULL` 的行。
    ///
    /// 已裁决的行留在表里是**故意的**（历史留痕），算进去会让界面一直显示
    /// 「1 个冲突待处理」而其实用户早就选完了。
    #[test]
    fn t8_pending_conflicts_only_counts_unresolved() {
        use qxync_core::store::{DecisionRow, Store};
        let store = Store::open_in_memory().unwrap();
        let mk = |path: &str, res: Option<&str>| DecisionRow {
            id: DecisionRow::id_for(path),
            path: path.into(),
            task_id: "t1".into(),
            is_dir: false,
            local_size: 1,
            local_mtime: 1,
            remote_size: 2,
            remote_mtime: 2,
            created_unix: 1,
            resolution: res.map(|s| s.to_string()),
        };
        store.decision_upsert(&mk("/home/a.txt", None)).unwrap();
        store
            .decision_upsert(&mk("/home/b.txt", Some("keep_local")))
            .unwrap();
        store
            .decision_upsert(&mk("/home/c.txt", Some("keep_both")))
            .unwrap();

        let m = pending_conflicts_by_path(store.decisions()).expect("查得到");
        assert_eq!(m.get("/home/a.txt"), Some(&1), "待裁决的要算进去");
        assert_eq!(
            m.get("/home/b.txt"),
            None,
            "已裁决的是历史留痕，不该再报「待处理」"
        );
        assert_eq!(m.get("/home/c.txt"), None);
        assert_eq!(m.len(), 1, "只有一条待裁决");
    }

    /// ★ T8：查表失败必须报「不知道」而不是「没有冲突」。
    ///
    /// 这是最容易写错的一处：空表的含义是「无冲突」，会让 `in_sync` 判成
    /// `true` —— 用户看到一个其实有冲突的文件被标成「已同步」。
    #[test]
    fn t8_pending_conflicts_reports_unknown_on_query_failure() {
        // 造一个真实的 `Err`（等价于「表打不开 / SQL 失败」）
        let err: qxync_core::Result<Vec<qxync_core::store::DecisionRow>> =
            Err(qxync_core::Error::Db("no such table: decisions".into()));
        assert!(
            pending_conflicts_by_path(err).is_none(),
            "查不到必须回 None（= 未知），绝不能回空表"
        );
    }

    // ------------------------------------------------------------ 测试脚手架

    /// 造一份 `ConfigPaths`（每个用例一个独立目录；`/tmp` 只有 10M tmpfs）。
    fn tmp_paths(tag: &str) -> ConfigPaths {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp/daemon-test")
            .join(tag);
        let _ = std::fs::remove_dir_all(&base);
        // `MountsFile::file` 落在 `state_dir/qxync/`，先把那两级建出来
        std::fs::create_dir_all(base.join("state/qxync")).unwrap();
        ConfigPaths {
            config_dir: base.join("config"),
            data_dir: base.join("data"),
            state_dir: base.join("state"),
        }
    }

    fn mount_rec(mp: &str) -> MountRecord {
        MountRecord {
            mountpoint: mp.into(),
            remote: "/home".into(),
            read_write: true,
            cache_mode: "pagecache".into(),
            conflict: "rename_local".into(),
            threads: 4,
            hydrate_timeout_secs: 60,
            delete_limit: None,
            auto_unmount: true,
        }
    }
}
