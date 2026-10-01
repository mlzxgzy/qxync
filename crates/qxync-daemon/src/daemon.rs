//! daemon 主体：状态、IPC 服务端、各方法实现。
//!
//! 设计见 `docs/M1.5-设计.md`：单进程持有 FUSE；CLI/GUI 通过 unix socket 的 JSON 行协议访问。

use anyhow::{bail, Context, Result};
use qxync_client::{Client, Session};
use qxync_core::dehydrate::{Block, CacheLimit, Policy};
use qxync_core::ipc::{
    decode_line, encode_line, mask_sid, CacheInfo, CursorInfo, DaemonInfo, DehydrateData,
    ErrorKind, GetData, HydroStats, IpcError, LinkInfo, LoginData, LsData, MountInfo, PingData,
    PutData, Request, RequestEnvelope, Response, ServerInfo, SessionInfo, ShutdownData, StatusData,
    StoreData, SyncCursors, SyncInfo, IPC_VERSION,
};
use qxync_core::{ConfigPaths, Credentials, Error as CoreError, LinkConfig, HOME_ROOT};
use qxync_fuse::upload::UploadQueue;
use qxync_fuse::{CacheMode, FsHandle, HydroCounters, LocalView, MountHandle, PinMap, QxyncFs};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, Mutex};

use crate::sync::{self, MountView, SyncConfig, SyncState, SyncStats};

pub struct Options {
    pub link_id: String,
    pub socket: PathBuf,
    pub auto_login: bool,
}

struct MountEntry {
    info: MountInfo,
    /// ★ M3：fuser 的后台会话（`umount` 时 join）。
    session: Option<qxync_fuse::BackgroundSession>,
    /// ★ M3：脱水要用的内核通知句柄（`inval_inode`）。
    notifier: qxync_fuse::Notifier,
    counters: Arc<HydroCounters>,
    /// 读写挂载时的上传队列（卸载前要排空）。
    upload: Option<Arc<UploadQueue>>,
    /// ★ M2c：共享节点表句柄（同步引擎在挂载线程外刷新远端变更）。
    handle: FsHandle,
    /// ★ M3：缓存模式（pagecache / direct）。
    cache_mode: CacheMode,
}

struct State {
    version: &'static str,
    started: Instant,
    socket: PathBuf,
    link: LinkConfig,
    client: Mutex<Client>,
    session: Mutex<Option<Session>>,
    /// 远端路径 → pin 状态（与 FUSE 实例共享，`getfattr -n user.qsync.pin` 能看到）。
    pins: PinMap,
    mounts: StdMutex<HashMap<PathBuf, MountEntry>>,
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
}

// ---------------------------------------------------------------- 入口

pub async fn run(opts: Options) -> Result<()> {
    let paths = ConfigPaths::discover()?;
    paths.ensure_dirs()?;
    let link = LinkConfig::load(&paths, &opts.link_id)
        .with_context(|| format!("读取连接配置失败（先 `qsync --host ... login`）"))?;

    if let Some(dir) = opts.socket.parent() {
        std::fs::create_dir_all(dir)?;
        qxync_core::config::restrict_perms(dir, 0o700)?;
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

    let client = Client::new(&link)?;
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
    let pins_seed: HashMap<String, String> = sync_store.store.pins().unwrap_or_default().into_iter().collect();
    if !pins_seed.is_empty() {
        tracing::info!("从状态库恢复 {} 条 pin", pins_seed.len());
    }
    let sync_interval: u64 = std::env::var("QSYNC_POLL_INTERVAL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(qxync_core::sync::DEFAULT_POLL_INTERVAL_SECS);
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
    });

    tracing::info!(
        "qxyncd 启动 pid={} socket={} link={}",
        std::process::id(),
        opts.socket.display(),
        state.link.id
    );
    if opts.auto_login {
        match login_internal(&state, None, None).await {
            Ok(s) => tracing::info!("自动登录成功 user={} sid={}", s.username, mask_sid(&s.sid)),
            Err(e) => tracing::warn!("自动登录失败（可稍后 `qsync login`）: {}", e.message),
        }
    }

    // ★ M2c：后台轮询（三游标 + baseline 对账）；QSYNC_POLL_INTERVAL=0 可暂停
    spawn_poller(state.clone());
    // ★ M3：后台脱水（闲置 + 缓存限额）；QSYNC_DEHYDRATE_IDLE / QSYNC_CACHE_LIMIT 开启
    spawn_dehydrator(state.clone());

    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
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
        } => {
            mount(
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
            )
            .await
        }
        Request::Umount { mountpoint } => umount(state, mountpoint).await,
        Request::Mounts => mounts(state),
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
            "尚未登录（先 `qsync --via-daemon login`）",
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
                "没有口令：用 `qsync login --password <口令>` 或先在 CLI 登录一次",
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
        link: LinkInfo {
            id: state.link.id.clone(),
            host: state.link.host.clone(),
            port: state.link.port,
            https: state.link.https,
            user: state.link.user.clone(),
            ipv4_only: state.link.ipv4_only,
        },
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

/// ★ M5：本地状态库快照（`qsync store [--integrity]`）。
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

    let remote = remote.unwrap_or_else(|| HOME_ROOT.to_string());
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
    let mut fuse_client = Client::new(&state.link).map_err(map_err)?;
    fuse_client.set_sid(sid);
    let counters = Arc::new(HydroCounters::default());
    let fuse_client = Arc::new(fuse_client);
    let mut fs = QxyncFs::new(fuse_client.clone(), remote.clone(), cache.clone())
        .map_err(|e| IpcError::new(ErrorKind::Io, e.to_string()))?
        .with_hydrate_timeout(hydrate_timeout)
        .with_counters(counters.clone())
        .with_pins(state.pins.clone());
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
        q.set_success_hook(Arc::new(move |remote: &str| h.clear_dirty(remote)));
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
        remote,
        readonly: !read_write,
    };
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
        },
    );
    tracing::info!(
        "已挂载 {}（{} 线程，auto_unmount={auto_unmount}，cache_mode={}）",
        mp.display(),
        threads,
        mode.as_str()
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
        let mut c = Client::new(&state.link).map_err(map_err)?;
        c.set_sid(sid);
        *g = Some(Arc::new(c));
    }
    Ok(g.clone().expect("刚填过"))
}

/// 当前挂载视图（同步引擎的操作对象）。
fn build_views(state: &Arc<State>) -> Vec<MountView> {
    let g = state.mounts.lock().unwrap();
    g.values()
        .map(|m| MountView {
            mountpoint: m.info.mountpoint.clone(),
            remote_root: m.info.remote.clone(),
            view: Arc::new(m.handle.clone()) as Arc<dyn LocalView>,
            upload: m.upload.clone(),
            read_only: m.info.readonly,
        })
        .collect()
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
    // `--force-deletes` 只放行一轮
    {
        let mut c = state.sync_cfg.lock().unwrap();
        if c.force_deletes {
            c.force_deletes = false;
        }
    }
    Ok(report)
}

/// `qsync sync`：查看/触发/调参同步引擎。
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
        tracing::info!("变更轮询已禁用（QSYNC_POLL_INTERVAL=0）");
        return;
    }
    tracing::info!("变更轮询已启动：每 {interval}s 一轮（QSYNC_POLL_INTERVAL 可调）");
    tokio::spawn(async move {
        loop {
            let secs = *state.sync_interval.lock().unwrap();
            if secs == 0 {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
            tokio::time::sleep(Duration::from_secs(secs.clamp(1, 3600))).await;
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

/// `qsync rm <dir> <name>`：删远端条目（脚本/测试用；FUSE 的 unlink 走同一方法）。
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
        let idle = std::env::var("QSYNC_DEHYDRATE_IDLE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0u64);
        let limit = std::env::var("QSYNC_CACHE_LIMIT")
            .ok()
            .and_then(|v| CacheLimit::parse(&v));
        let interval = std::env::var("QSYNC_DEHYDRATE_INTERVAL")
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

/// `qsync dehydrate` 的参数。
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

/// 执行一次脱水（`qsync dehydrate` 与后台扫描共用）。
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

    // 选定挂载点
    let targets: Vec<(PathBuf, String, FsHandle, qxync_fuse::Notifier, CacheMode)> = {
        let g = state.mounts.lock().unwrap();
        g.values()
            .filter(|m| {
                opts.mountpoint
                    .as_ref()
                    .map(|mp| m.info.mountpoint == *mp)
                    .unwrap_or(true)
            })
            .map(|m| {
                (
                    m.info.mountpoint.clone(),
                    m.info.remote.clone(),
                    m.handle.clone(),
                    m.notifier.clone(),
                    m.cache_mode,
                )
            })
            .collect()
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

/// `qsync dehydrate`：IPC 入口。
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

/// 后台脱水：按 `QSYNC_DEHYDRATE_INTERVAL` 周期扫「闲置 + 限额」。
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
            "自动脱水未启用（QSYNC_DEHYDRATE_IDLE=0 且未设 QSYNC_CACHE_LIMIT）；\
             手动 `qsync dehydrate` 可用，`qsync dehydrate --idle-secs N` 可动态开启"
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
            }
        }
    });
}
