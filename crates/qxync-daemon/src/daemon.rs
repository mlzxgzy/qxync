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
    PutData, Request, RequestEnvelope, Response, RootsData, RulesData, ServerInfo,
    SessionInfo, SettingsData, ShutdownData, SpaceData, StatusData, StoreData, SyncCursors,
    SyncInfo, TaskInfo, TasksData, IPC_VERSION,
};
use qxync_core::rules::Rules;
use qxync_core::settings::{ProxySpec, Settings};
use qxync_core::store::JournalEntry;
use qxync_core::tasks::{conflict_label, has_any_task, Task, CONFLICT_RENAME_LOCAL};
use qxync_core::{ConfigPaths, Credentials, Error as CoreError, LinkConfig, PeerConfig};
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

pub struct Options {
    pub link_id: String,
    pub socket: PathBuf,
    pub auto_login: bool,
    /// ★ M8.2：启动时恢复 `enabled=true` 的任务（显式开启，见 `--restore-tasks` / `QXNYC_TASK_RESTORE`）。
    ///
    /// **默认关闭**是刻意的：任务恢复会「凭空挂载」，如果默认打开，
    /// 上一次跑崩留下的挂载会在下一次 daemon 启动时复活，把验收矩阵的前提打乱
    /// （`fuse-matrix.sh` 依赖「开跑前环境是干净的」）。GUI 自己拉起 daemon 时会显式打开。
    pub restore_tasks: bool,
}

pub(crate) struct MountEntry {
    info: MountInfo,
    /// ★ M3：fuser 的后台会话（`umount` 时 join）。
    session: Option<qxync_fuse::BackgroundSession>,
    /// ★ M3：脱水要用的内核通知句柄（`inval_inode`）。
    notifier: qxync_fuse::Notifier,
    counters: Arc<HydroCounters>,
    /// 读写挂载时的上传队列（卸载前要排空）。
    upload: Option<Arc<UploadQueue>>,
    /// ★ M2c：共享节点表句柄（同步引擎在挂载线程外刷新远端变更）。
    pub(crate) handle: FsHandle,
    /// ★ M3：缓存模式（pagecache / direct）。
    cache_mode: CacheMode,
    /// ★ M8.4：该挂载点的冲突策略（同步引擎按它分派冲突）。
    conflict: String,
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
    match sig {
        Some(s) => {
            s.recv().await;
        }
        None => std::future::pending::<()>().await,
    }
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
        sync: None,
        cache: None,
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

    // ★ M8.3：journal 后台落库（批量 + 轮转）
    spawn_journal_flusher(state.clone());

    // ★ M8.2：恢复启用的任务（**显式开启才跑**，见 Options::restore_tasks）
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
            restore_tasks_on_start(&st, &lid).await;
        });
    }

    // ★ M2c：后台轮询（三游标 + baseline 对账）；QXNYC_POLL_INTERVAL=0 可暂停
    spawn_poller(state.clone());
    // ★ M3：后台脱水（闲置 + 缓存限额）；QXNYC_DEHYDRATE_IDLE / QXNYC_CACHE_LIMIT 开启
    spawn_dehydrator(state.clone());
    // ★ M8.4：自动释放空间（设置里的 `free_space`；默认关 → 不改变 M8.3 行为）
    spawn_auto_free(state.clone());

    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
    let mut sigterm = terminate_signal();
    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                tracing::info!("收到 shutdown 请求");
                break;
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("收到 SIGINT");
                break;
            }
            _ = wait_sigterm(&mut sigterm) => {
                tracing::info!("收到 SIGTERM");
                break;
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
    }

    let n = shutdown_all_mounts(&state).await;
    let _ = std::fs::remove_file(&opts.socket);
    let _ = std::fs::remove_file(&pid_path);
    tracing::info!("qxyncd 退出（已卸载 {n} 个挂载点）");
    Ok(())
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
    let kind = match &e {
        CoreError::Auth(_) => ErrorKind::Auth,
        CoreError::Transport(_) => ErrorKind::Transport,
        CoreError::Status { .. } => ErrorKind::Status,
        CoreError::Parse(_) => ErrorKind::Parse,
        CoreError::Io(_) => ErrorKind::Io,
        CoreError::Db(_) => ErrorKind::Io,
        CoreError::Unsupported(_) => ErrorKind::Unsupported,
    };
    IpcError::new(kind, e.to_string())
}

/// 会话失效的两种表现：`Auth`，或 `get_list`/`stat` 回 status 4/5。
fn is_auth_error(e: &CoreError) -> bool {
    match e {
        CoreError::Auth(_) => true,
        CoreError::Status { status, .. } => matches!(status.0, 4 | 5),
        _ => false,
    }
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
    to_value(serde_json::json!({}))
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

    let (mounts, hydro, uploads) = snapshot_mounts(state);
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
            home_root: state.link.root(),
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
        sync: Some(sync_info(state)),
        cache: Some(cache_info(state)),
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
/// 可写性规则（保留自 M6 的实测结论）：**只有家目录根可写** —— 普通账号往非 Qsync
/// 同步文件夹上传会被服务端拒绝（`status:20`）。
async fn roots_cmd(state: &Arc<State>) -> Result<serde_json::Value, IpcError> {
    let home_root = state.link.root();
    let logged_in = current_session(state).await.is_some();
    let syncing = if logged_in {
        with_client!(state, |c| c.syncing_folders()).unwrap_or_default()
    } else {
        Vec::new()
    };
    let note = if logged_in {
        "「家目录」以外的 NAS 文件夹默认只读：服务端会拒绝往非 Qsync 同步文件夹上传（status 20）"
            .to_string()
    } else {
        "未登录：拿不到 NAS 上登记的同步文件夹列表，只能手输或用「浏览…」".to_string()
    };
    to_value(RootsData {
        home_root,
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
    let roots = vec![state.link.root()];
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
    let home_root = state.link.root();
    let remote = qxync_core::normalize_root(&remote.unwrap_or_else(|| home_root.clone()));
    // ★ M6 实测结论保留：只有家目录根可写（普通账号往 /Public 上传会被服务端拒 status 20）。
    //   一对多删掉之后，单根也可能是共享文件夹 —— 所以这里按「是不是 home_root」判可写。
    let read_write = if read_write && remote != home_root {
        tracing::warn!(
            "{} 不是家目录根（{}）：强制只读（实测服务端会拒绝往非 Qsync 同步文件夹上传，status 20）",
            remote,
            home_root
        );
        false
    } else {
        read_write
    };
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
    let mut fuse_client =
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
    .with_peers(state.peers.clone());
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
    if read_write {
        let marker_dir = ConfigPaths::discover()
            .map(|p| p.data_dir.join("upload-queue"))
            .unwrap_or_else(|_| cache.join("upload-queue"));
        let q = UploadQueue::new(
            fuse_client.clone(),
            tokio::runtime::Handle::current(),
            marker_dir,
        )
        .map_err(|e| IpcError::new(ErrorKind::Io, format!("创建上传队列失败: {e}")))?;
        q.spawn_worker()
            .map_err(|e| IpcError::new(ErrorKind::Io, format!("启动上传 worker 失败: {e}")))?;
        fs = fs.with_write_mode().with_upload_queue(q.clone());
        upload_queue = Some(q);
    }
    // ★ M2c/M3：必须在 fs 被交给 fuser 之前取句柄，同步引擎与脱水都靠它
    let handle = fs.handle();
    // ★ M3：上传成功后清掉节点 dirty（否则脱水永远被 dirty 挡住）
    if let Some(q) = &upload_queue {
        let h = handle.clone();
        let peer = state.peer.lock().unwrap().clone();
        q.set_success_hook(Arc::new(move |remote: &str| {
            // 本地改动已经落到 NAS → 清 dirty，并让对端走事件快路径
            let sig = h.node(remote).map(|n| (n.size, n.mtime));
            h.clear_dirty(remote);
            if let (Some(host), Some((size, mtime))) = (peer.as_ref(), sig) {
                host.notify_async(remote.to_string(), size, mtime, "modified");
            }
        }));
    }

    // ★ M3：用 spawn（而不是阻塞的 mount2）—— 拿到 Notifier 才能发 inval_inode
    let MountHandle { session, notifier } =
        qxync_fuse::spawn(fs, &mp, threads, auto_unmount, !read_write)
            .map_err(|e| IpcError::new(ErrorKind::Io, format!("挂载失败: {e}")))?;

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
    state.mounts.lock().unwrap().insert(
        mp.clone(),
        MountEntry {
            info: info.clone(),
            session: Some(session),
            notifier,
            counters,
            upload: upload_queue,
            handle,
            cache_mode: mode,
            conflict: conflict.clone(),
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
    to_value(info)
}

async fn umount(state: &Arc<State>, mountpoint: PathBuf) -> Result<serde_json::Value, IpcError> {
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
    to_value(serde_json::json!({}))
}

fn mounts(state: &Arc<State>) -> Result<serde_json::Value, IpcError> {
    let (list, _, _) = snapshot_mounts(state);
    to_value(list)
}

fn snapshot_mounts(
    state: &Arc<State>,
) -> (
    Vec<MountInfo>,
    HydroStats,
    Option<qxync_core::ipc::UploadInfo>,
) {
    let g = state.mounts.lock().unwrap();
    let list = g.values().map(|m| m.info.clone()).collect();
    let (mut count, mut bytes) = (0u64, 0u64);
    let mut uploads: Option<qxync_core::ipc::UploadInfo> = None;
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
    }
    (list, HydroStats { count, bytes }, uploads)
}

async fn shutdown_all_mounts(state: &Arc<State>) -> usize {
    let mps: Vec<PathBuf> = state.mounts.lock().unwrap().keys().cloned().collect();
    let mut n = 0;
    for mp in mps {
        // 忽略错误：即使 fusermount 失败，进程退出时 auto_unmount 也会兜底
        if let Ok(_) = umount(state, mp).await {
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
        .map(|c| c.sid() != Some(sid.as_str()))
        .unwrap_or(true);
    if stale {
        let mut c =
            Client::new_with_proxy(&state.link, Some(&state.settings.lock().unwrap().proxy))
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
            "sync --once: events={} refreshed={} uploaded={} conflicts={} deleted={} blocked={}",
            report.events,
            report.refreshed,
            report.uploaded,
            report.conflicts,
            report.deleted,
            report.deletes_blocked
        );
    }
    to_value(sync_info(state))
}

/// 后台轮询：按 `sync_interval` 周期跑 `run_sync_once`；未登录时静默跳过。
fn spawn_poller(state: Arc<State>) {
    let interval = *state.sync_interval.lock().unwrap();
    if interval == 0 {
        tracing::info!("变更轮询已禁用（QXNYC_POLL_INTERVAL=0）");
        return;
    }
    tracing::info!("变更轮询已启动：每 {interval}s 一轮（QXNYC_POLL_INTERVAL 可调）");
    tokio::spawn(async move {
        let mut last_run = Instant::now() - Duration::from_secs(3600);
        loop {
            let secs = *state.sync_interval.lock().unwrap();
            if secs == 0 {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
            // ★ M7：正常睡到下一轮；对端事件到达时提前醒来（事件是快路径，对账是主路径）
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(secs.clamp(1, 3600))) => {}
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
                }
            }
        }
    });
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
/// ★ M6：多根视图在挂载点里多一层「视图名」（`~/mnt/Public/a`），所以要传 `view_prefix`；
/// 单根直通传空串（行为与 M3 完全一致）。映射错了后果很严重：mmap 判定失守 →
/// 脱水会在别人还映射着的时候清内容（铁则 2 的相关保护）。
fn mmap_remotes(
    mountpoint: &std::path::Path,
    view_prefix: &str,
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
                let rest = if view_prefix.is_empty() {
                    rest
                } else {
                    match rest.strip_prefix(&format!("/{view_prefix}")) {
                        Some(r) if r.is_empty() || r.starts_with('/') => r,
                        _ => continue,
                    }
                };
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
    let targets: Vec<(
        PathBuf,
        String,
        String,
        FsHandle,
        qxync_fuse::Notifier,
        CacheMode,
    )> = {
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
                String::new(),
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
    for (mp, view_prefix, remote_root, handle, notifier, _mode) in &targets {
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
        let mapped = mmap_remotes(mp, view_prefix, remote_root);
        let policy = Policy {
            idle_secs,
            cache_limit: limit,
            recent_secs,
            now,
        };
        let mut cands = if let Some(p) = &manual_path {
            handle.candidate(p).into_iter().collect::<Vec<_>>()
        } else {
            handle.dehydrate_candidates()
        };
        // ★ M6：多根视图下候选是「整个挂载点的」，按当前根过滤（否则每个根都会把同一批文件算一遍）
        if !view_prefix.is_empty() {
            let r = remote_root.trim_end_matches('/').to_string();
            let prefix = format!("{r}/");
            cands.retain(|c| c.remote == r || c.remote.starts_with(&prefix));
        }
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

/// 后台脱水：按 `QXNYC_DEHYDRATE_INTERVAL` 周期扫「闲置 + 限额」。
fn spawn_dehydrator(state: Arc<State>) {
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
            tokio::time::sleep(Duration::from_secs(interval.clamp(5, 3600))).await;
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
    });
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
    let remote = t.effective_root(&state.link.root());
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
            let (errors, warnings) = t.conflict_report(&others, &state.link.root());
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

/// ★ M8.2：daemon 启动时恢复 `enabled=true` 的任务。
///
/// **只有显式开启才跑**（`--restore-tasks` / `QXNYC_TASK_RESTORE=1`）：
/// 任务恢复会「凭空挂载」，默认打开会让上一次跑崩留下的挂载在重启时复活，
/// 把验收矩阵的前提（开跑前环境干净）打乱。
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
    let todo: Vec<Task> = list.into_iter().filter(|t| t.enabled).collect();
    if todo.is_empty() {
        tracing::info!("恢复任务：没有启用的任务（link={link_id}）");
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
fn spawn_journal_flusher(state: Arc<State>) {
    tokio::spawn(async move {
        let max_rows = journal_max_rows();
        let max_age = journal_max_age_days();
        let trim_secs = journal_trim_secs();
        tracing::info!(
            "同步日志（journal）已启动：上限 {max_rows} 条 / {max_age} 天，每 {trim_secs}s 轮转一次"
        );
        let mut last_trim = Instant::now() - Duration::from_secs(trim_secs + 1);
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let batch: Vec<JournalEntry> = {
                let mut b = state.journal_buf.lock().unwrap();
                if b.is_empty() {
                    Vec::new()
                } else {
                    std::mem::take(&mut *b)
                }
            };
            if !batch.is_empty() {
                let g = state.sync_store.lock().unwrap();
                if let Err(e) = g.store.journal_add_batch(&batch) {
                    tracing::warn!("journal 落库失败（丢弃 {} 条）: {e}", batch.len());
                }
            }
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
    });
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
    let mut out = Vec::new();
    let (mut online, mut local, mut always) = (0usize, 0usize, 0usize);
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
        out.push(FileStateInfo {
            name: e.filename.clone(),
            remote,
            is_dir: e.isfolder,
            size: e.filesize,
            hydrated_bytes,
            state: state_str.to_string(),
            pin,
            dirty,
            hidden: handle
                .hidden(&format!("{}/{}", dir, e.filename), e.isfolder)
                .is_some(),
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
fn spawn_auto_free(state: Arc<State>) {
    let interval = std::env::var("QXNYC_AUTO_FREE_INTERVAL")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(60)
        .clamp(1, 3600);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(interval)).await;
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
    });
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
