//! `qsync` —— QSync-Linux 前端 CLI（M0：协议打通）。
//!
//! 后端守护 `qxyncd` 尚未实现，本版 CLI 直接驱动 `qxync-client` 对 NAS 跑通
//! 「登录 → 列举 → stat → 下载 → 上传 → 建目录」这条链，作为 M0 的验收工具。

mod ipc_client;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use qxync_client::Client;
use qxync_core::ipc::{
    default_socket_path, CursorInfo, ErrorKind, GetData, LsData, MountInfo, PingData, PutData,
    Request, StatusData,
};
use qxync_core::{ConfigPaths, Credentials, DirEntry, LinkConfig, HOME_ROOT};
use qxync_fuse::QxyncFs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(
    name = "qsync",
    version,
    about = "QSync for Linux —— on-demand 同步客户端（M0 协议版）"
)]
struct Cli {
    /// 连接 id（对应 ~/.config/qsync/links/<id>.json）
    #[arg(long, global = true, default_value = "default")]
    link: String,

    /// 覆盖 NAS 地址（配合 --user/--password 首次登录）
    #[arg(long, global = true, env = "QSYNC_HOST")]
    host: Option<String>,

    #[arg(long, global = true, default_value_t = 9834)]
    port: u16,

    /// 使用 HTTPS（默认 true，用 --no-https 关闭）
    #[arg(long, global = true, default_value_t = true, action = clap::ArgAction::Set)]
    https: bool,

    /// 自签证书：跳过 TLS 校验
    #[arg(long, global = true)]
    insecure: bool,

    /// 只走 IPv4（对端 IPv6 路由不通时用）
    #[arg(long, global = true)]
    ipv4: bool,

    /// daemon 的 IPC socket 路径（默认 $XDG_RUNTIME_DIR/qxync/qxyncd.sock）
    #[arg(long, global = true, env = "QSYNC_SOCKET")]
    socket: Option<PathBuf>,

    /// 强制走 daemon IPC（默认：socket 可连就走 daemon）
    #[arg(long, global = true, conflicts_with = "direct")]
    via_daemon: bool,

    /// 强制直连 NAS（忽略 daemon）
    #[arg(long, global = true)]
    direct: bool,

    #[arg(long, global = true, env = "QSYNC_USER")]
    user: Option<String>,

    /// 口令；不给则读 credentials.json，再不给则报错（避免落进 shell 历史）
    #[arg(long, global = true, env = "QSYNC_PASSWORD", hide_env_values = true)]
    password: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// 登录并把凭据写入 ~/.config/qsync/credentials.json (0600)
    Login,
    /// 会话状态 + 服务端信息（max_log / uid / 迁移状态）
    Status,
    /// 列目录（自动翻页）
    Ls {
        #[arg(default_value = HOME_ROOT)]
        path: String,
    },
    /// 单文件 stat（注意：需要 `目录 + 文件名`，不是全路径）
    Stat { dir: String, name: String },
    /// 下载文件
    Get {
        /// 远端目录
        dir: String,
        /// 远端文件名
        name: String,
        /// 本地输出路径（默认 ./<name>）
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// 上传文件
    Put {
        /// 本地文件
        local: PathBuf,
        /// 远端目录
        dest: String,
        /// 目标文件名（默认用本地文件名）
        #[arg(long)]
        name: Option<String>,
    },
    /// 建目录
    Mkdir { parent: String, name: String },
    /// 挂载只读 on-demand 视图（M1：整文件水合）
    Mount {
        /// 本地挂载点
        mountpoint: PathBuf,
        /// 远端根目录（普通用户固定 /home）
        #[arg(long, default_value = HOME_ROOT)]
        remote: String,
        /// 水合缓存目录（默认 ~/.local/share/qsync/cache）
        #[arg(long)]
        cache_dir: Option<PathBuf>,
        /// FUSE 事件循环线程数（默认 4）
        #[arg(long, default_value_t = 4)]
        threads: usize,
        /// 进程退出时自动卸载（需要 /etc/fuse.conf 里 user_allow_other）
        #[arg(long)]
        auto_unmount: bool,
        /// 单个文件的水合超时秒数（默认 60，对应 Qsync 的 CANCEL_FETCH_DATA）
        #[arg(long, default_value_t = 60)]
        hydrate_timeout: u64,
    },
    /// 卸载 FUSE 挂载点
    Umount { mountpoint: PathBuf },

    /// 查看/设置 pin：`qsync pin <远端路径> [pinned|unpinned|unspecified|excluded]`
    Pin { path: String, state: Option<String> },

    /// 查看某路径的占位符状态（pin + 远端元数据）
    State { path: String },

    /// 守护进程生命周期
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
}

#[derive(Subcommand, Debug)]
enum DaemonAction {
    /// 拉起 qxyncd（已在运行则直接报成功）
    Start,
    /// 请求 qxyncd 干净退出
    Stop,
    /// ping + 状态快照
    Status,
}

fn paths() -> Result<ConfigPaths> {
    Ok(ConfigPaths::discover()?)
}

/// 优先用命令行参数；其次读已有 link 文件；最后用默认值兜底。
fn resolve_link(cli: &Cli) -> Result<LinkConfig> {
    let p = paths()?;
    if let Ok(mut link) = LinkConfig::load(&p, &cli.link) {
        if let Some(h) = &cli.host {
            link.host = h.clone();
        }
        if cli.port != 9834 {
            link.port = cli.port;
        }
        if cli.insecure {
            link.insecure = true;
        }
        if cli.ipv4 {
            link.ipv4_only = true;
        }
        if let Some(u) = &cli.user {
            link.user = u.clone();
        }
        return Ok(link);
    }
    match (&cli.host, &cli.user) {
        (Some(h), Some(u)) => Ok(LinkConfig {
            id: cli.link.clone(),
            host: h.clone(),
            port: cli.port,
            https: cli.https,
            insecure: cli.insecure,
            user: u.clone(),
            home_root: HOME_ROOT.to_string(),
            ipv4_only: cli.ipv4,
        }),
        _ => bail!(
            "没有找到连接配置 {}，且缺少 --host/--user。\n首次使用：qsync --host <NAS> --port <端口> --insecure --user <用户> --password <口令> login",
            cli.link
        ),
    }
}

fn resolve_password(cli: &Cli, link: &LinkConfig) -> Result<String> {
    if let Some(p) = &cli.password {
        return Ok(p.clone());
    }
    let p = paths()?;
    if let Ok(c) = Credentials::load(&p) {
        if c.host == link.host && c.user == link.user {
            return Ok(c.password);
        }
    }
    bail!("没有口令：用 --password / QSYNC_PASSWORD，或先跑 `qsync login --password` 写凭据")
}

async fn connect(cli: &Cli, need_creds: bool) -> Result<(Client, LinkConfig)> {
    let link = resolve_link(cli)?;
    let mut client = Client::new(&link)?;
    if need_creds {
        let pw = resolve_password(cli, &link)?;
        client
            .login(&link.user, &pw)
            .await
            .with_context(|| format!("登录 {}@{} 失败", link.user, link.host))?;
    }
    Ok((client, link))
}

fn human_size(n: u64) -> String {
    const K: u64 = 1024;
    match n {
        0..=1023 => format!("{n} B"),
        n if n < K * K => format!("{:.1} KiB", n as f64 / K as f64),
        n if n < K * K * K => format!("{:.1} MiB", n as f64 / (K * K) as f64),
        n => format!("{:.2} GiB", n as f64 / (K * K * K) as f64),
    }
}

fn print_entry(e: &DirEntry) {
    let kind = if e.isfolder { "d" } else { "-" };
    let mt = e
        .mtime_text
        .clone()
        .unwrap_or_else(|| e.epochmt.to_string());
    let child = if e.have_child { "+" } else { " " };
    println!(
        "{kind}{child} {:>12}  {:>10}  {}",
        human_size(e.display_size()),
        e.filesize,
        format_args!("{mt}  {}", e.filename)
    );
}

#[tokio::main]
async fn main() -> Result<()> {
    // Rust 默认忽略 SIGPIPE → `qsync ls | head` 会在 println! 上 EPIPE panic。
    // 恢复系统默认行为（进程被 SIGPIPE 终止），这是 CLI 的惯例。
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };

    // 日志：RUST_LOG=info 可看到水合/失败细节
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let socket = cli.socket.clone().unwrap_or_else(default_socket_path);

    // ---- daemon 生命周期命令（不参与自动路由）----
    if let Cmd::Daemon { action } = &cli.cmd {
        return daemon_cmd(&cli, &socket, action).await;
    }

    // ---- 路由：显式指定 > 自动探测（socket 可连就走 daemon）----
    let use_ipc = if cli.direct {
        false
    } else if cli.via_daemon {
        true
    } else {
        ipc_client::available(&socket).await
    };
    if use_ipc {
        match route_via_daemon(&cli, &socket).await {
            Ok(()) => return Ok(()),
            // daemon 连不上/中途挂了 → 回退直连，并提示（M1.5 兼容期行为）
            Err(e) if e.kind == ErrorKind::NotRunning => {
                eprintln!("⚠️ daemon 不可用（{}），回退直连", e.message);
            }
            Err(e) => {
                eprintln!("❌ {}", e.message);
                std::process::exit(e.kind.exit_code());
            }
        }
    }

    match &cli.cmd {
        Cmd::Login => {
            let link = resolve_link(&cli)?;
            let mut client = Client::new(&link)?;
            let pw = resolve_password(&cli, &link)?;
            let session = client.login(&link.user, &pw).await?;
            paths()?.ensure_dirs()?;
            let link_path = link.save(&paths()?)?;
            let cred_path = Credentials {
                host: link.host.clone(),
                user: link.user.clone(),
                password: pw,
            }
            .save(&paths()?)?;
            println!("✅ 登录成功");
            println!("   sid        : {}", session.sid);
            println!("   user       : {}", session.username);
            println!("   NAS UID    : {}", session.uid.as_deref().unwrap_or("-"));
            println!(
                "   Qsync 版本 : {} / QPKG {} / build {}",
                session.nas.qsync_version.as_deref().unwrap_or("-"),
                session.nas.qpkg_version.as_deref().unwrap_or("-"),
                session.nas.build.as_deref().unwrap_or("-")
            );
            if let Some(reason) = session.busy_reason() {
                println!("   ⚠️  {reason} —— 同步应暂停");
            }
            println!("   连接配置   : {}", link_path.display());
            println!("   凭据       : {} (0600)", cred_path.display());
        }
        Cmd::Status => {
            let (client, link) = connect(&cli, true).await?;
            let max_log = client.max_log().await?;
            let nas = client.nas_uid().await?;
            let alive = client.check_alive().await?;
            println!("连接      : {} ({})", link.id, link.base_url());
            println!("用户      : {}", link.user);
            println!("会话存活  : {alive}");
            println!(
                "max_log   : {}  notify={}  global_notify={}  sync_signal={}",
                max_log.max_log, max_log.notify, max_log.global_notify, max_log.sync_signal
            );
            println!(
                "NAS       : MAC {} / Qsync {} / QPKG {} / build {}",
                nas.mac0.as_deref().unwrap_or("-"),
                nas.qsync_version.as_deref().unwrap_or("-"),
                nas.qpkg_version.as_deref().unwrap_or("-"),
                nas.build.as_deref().unwrap_or("-")
            );
            if let Some(reason) = nas.busy_reason() {
                println!("⚠️  {reason}");
            }
            println!("capabilities: qbox_cgi={} fcgi={}", nas.qbox_cgi, nas.fcgi);
        }
        Cmd::Ls { path } => {
            let (client, _) = connect(&cli, true).await?;
            let entries = client.list(path).await?;
            println!("# {path}  ({} 项)", entries.len());
            for e in &entries {
                print_entry(e);
            }
        }
        Cmd::Stat { dir, name } => {
            let (client, _) = connect(&cli, true).await?;
            match client.stat(dir, name).await? {
                Some(e) => {
                    print_entry(&e);
                    println!(
                        "    exist={} versioning_support={} privilege={}",
                        e.exist,
                        e.versioning_support,
                        e.privilege.as_deref().unwrap_or("-")
                    );
                }
                None => bail!("{dir}/{name} 不存在"),
            }
        }
        Cmd::Get { dir, name, out } => {
            let (client, _) = connect(&cli, true).await?;
            let dest = out.clone().unwrap_or_else(|| PathBuf::from(name));
            let expected = client.stat(dir, name).await?.map(|e| e.filesize);
            let written = client.download_to_file(dir, name, &dest).await?;
            if let Some(exp) = expected {
                if exp != written {
                    // 铁则 1 的前置检查：长度不符绝不能当成功
                    bail!("下载长度不符：服务端说 {exp} 字节，实际写入 {written} 字节（{dir}/{name}）");
                }
            }
            println!(
                "✅ {} -> {}  ({} 字节)",
                format_args!("{dir}/{name}"),
                dest.display(),
                written
            );
        }
        Cmd::Put { local, dest, name } => {
            let (client, _) = connect(&cli, true).await?;
            let bytes =
                std::fs::read(local).with_context(|| format!("读取 {}", local.display()))?;
            let target = name
                .clone()
                .or_else(|| local.file_name().map(|s| s.to_string_lossy().into_owned()))
                .context("无法从本地路径推断文件名，请用 --name")?;
            client.upload_bytes(dest, &target, bytes.clone()).await?;
            let mtime = std::fs::metadata(local)?
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            if mtime > 0 {
                client.set_mtime(dest, &target, mtime).await?;
            }
            println!(
                "✅ 上传 {} ({} 字节) -> {dest}/{target}，mtime={mtime}",
                local.display(),
                bytes.len()
            );
        }
        Cmd::Mkdir { parent, name } => {
            let (client, _) = connect(&cli, true).await?;
            client.mkdir(parent, name).await?;
            println!("✅ 已建目录 {parent}/{name}");
        }
        Cmd::Pin { .. } | Cmd::State { .. } => {
            bail!("`pin`/`state` 需要 daemon：先 `qsync daemon start`（或用 --socket 指向运行中的 daemon）");
        }
        Cmd::Daemon { .. } => unreachable!("daemon 命令在路由前已处理"),
        Cmd::Mount {
            mountpoint,
            remote,
            cache_dir,
            threads,
            auto_unmount,
            hydrate_timeout,
        } => {
            let (client, link) = connect(&cli, true).await?;
            let cache = cache_dir.clone().unwrap_or_else(|| {
                paths()
                    .map(|p| p.data_dir.join("cache"))
                    .unwrap_or_else(|_| PathBuf::from("/tmp/qxync-cache"))
            });
            let client = Arc::new(client);
            let fs = QxyncFs::new(client, remote.clone(), cache.clone())
                .context("初始化 FUSE 文件系统失败")?
                .with_hydrate_timeout(std::time::Duration::from_secs(*hydrate_timeout));
            println!(
                "挂载 {} -> {}",
                format_args!("{}:{}", link.host, remote),
                mountpoint.display()
            );
            println!("  缓存目录 : {}", cache.display());
            println!("  只读 + on-demand（M1 整文件水合；TTL=0.5s，{threads} 线程，auto_unmount={auto_unmount}）");
            println!(
                "  卸载     : qsync umount {}  （或 fusermount3 -u）",
                mountpoint.display()
            );
            let mnt = mountpoint.clone();
            let (n, au) = (*threads, *auto_unmount);
            // 用普通 OS 线程跑 FUSE 会话：tokio 的 spawn_blocking 线程带着 runtime 上下文，
            // 在回调里再 block_on 另一个 runtime 会踩 "Cannot start a runtime from within a runtime"。
            let handle = std::thread::Builder::new()
                .name("qxync-fuse".into())
                .spawn(move || qxync_fuse::mount(fs, &mnt, n, au))
                .context("创建 FUSE 线程失败")?;
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("FUSE 线程 panic"))?
                .context("挂载失败")?;
            println!("已卸载 {}", mountpoint.display());
        }
        Cmd::Umount { mountpoint } => {
            let out = std::process::Command::new("fusermount3")
                .arg("-u")
                .arg(mountpoint)
                .output()
                .or_else(|_| {
                    std::process::Command::new("fusermount")
                        .arg("-u")
                        .arg(mountpoint)
                        .output()
                })
                .context("执行 fusermount3 失败")?;
            if out.status.success() {
                println!("✅ 已卸载 {}", mountpoint.display());
            } else {
                bail!(
                    "卸载失败: {}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- daemon / IPC

/// 找到同目录下的 `qxyncd`（`cargo build` 后就在 target/debug/ 旁边）。
fn daemon_binary() -> Result<PathBuf> {
    let me = std::env::current_exe().context("无法定位自身可执行文件")?;
    let cand = me.with_file_name(if cfg!(windows) {
        "qxyncd.exe"
    } else {
        "qxyncd"
    });
    if cand.exists() {
        return Ok(cand);
    }
    bail!(
        "找不到 {}（先 `cargo build --workspace`，或用 --socket 指向已在运行的 daemon）",
        cand.display()
    )
}

fn ipc_err(e: qxync_core::ipc::IpcError) -> anyhow::Error {
    anyhow::anyhow!("{}", e.message)
}

/// CLI 命令 → IPC 请求；返回 None 表示该命令没有 IPC 形态。
fn to_request(cli: &Cli) -> Option<Request> {
    Some(match &cli.cmd {
        Cmd::Login => Request::Login {
            user: cli.user.clone(),
            password: cli.password.clone(),
        },
        Cmd::Status => Request::Status,
        Cmd::Ls { path } => Request::Ls { path: path.clone() },
        Cmd::Stat { dir, name } => Request::Stat {
            dir: dir.clone(),
            name: name.clone(),
        },
        Cmd::Get { dir, name, out } => Request::Get {
            dir: dir.clone(),
            name: name.clone(),
            dest: out.clone().unwrap_or_else(|| PathBuf::from(name)),
        },
        Cmd::Put { local, dest, name } => Request::Put {
            local: local.clone(),
            dest: dest.clone(),
            name: name.clone(),
        },
        Cmd::Mkdir { parent, name } => Request::Mkdir {
            parent: parent.clone(),
            name: name.clone(),
        },
        Cmd::Mount {
            mountpoint,
            remote,
            cache_dir,
            threads,
            auto_unmount,
            hydrate_timeout,
        } => Request::Mount {
            mountpoint: mountpoint.clone(),
            remote: Some(remote.clone()),
            cache_dir: cache_dir.clone(),
            threads: Some(*threads),
            auto_unmount: Some(*auto_unmount),
            hydrate_timeout_secs: Some(*hydrate_timeout),
        },
        Cmd::Umount { mountpoint } => Request::Umount {
            mountpoint: mountpoint.clone(),
        },
        Cmd::Pin { path, state } => Request::Pin {
            path: path.clone(),
            state: state.clone(),
        },
        Cmd::State { .. } | Cmd::Daemon { .. } => return None,
    })
}

fn split_remote(path: &str) -> (String, String) {
    match path.rfind('/') {
        Some(i) if i > 0 => (path[..i].to_string(), path[i + 1..].to_string()),
        _ => ("/".to_string(), path.trim_start_matches('/').to_string()),
    }
}

/// 走 daemon 执行本轮命令。
async fn route_via_daemon(
    cli: &Cli,
    socket: &std::path::Path,
) -> Result<(), qxync_core::ipc::IpcError> {
    // `state` 需要两步（stat + pin 查询）
    if let Cmd::State { path } = &cli.cmd {
        let (dir, name) = split_remote(path);
        let entry: Option<DirEntry> = ipc_client::call(socket, Request::Stat { dir, name }).await?;
        let pin: serde_json::Value = ipc_client::call(
            socket,
            Request::Pin {
                path: path.clone(),
                state: None,
            },
        )
        .await
        .unwrap_or(serde_json::Value::Null);
        match entry {
            Some(e) => {
                print_entry(&e);
                println!(
                    "    remote={path}  pin={}  exist={}",
                    pin.get("pin").and_then(|v| v.as_str()).unwrap_or("unknown"),
                    e.exist
                );
            }
            None => println!(
                "{path} 不存在（pin={}）",
                pin.get("pin").and_then(|v| v.as_str()).unwrap_or("unknown")
            ),
        }
        return Ok(());
    }

    let req = match to_request(cli) {
        Some(r) => r,
        None => return Ok(()),
    };
    let socket = socket.to_path_buf();
    match &cli.cmd {
        Cmd::Login => {
            let d: qxync_core::ipc::LoginData = ipc_client::call(&socket, req).await?;
            println!(
                "✅ 已通过 daemon 登录：user={} sid={} uid={}",
                d.user,
                d.sid_masked,
                d.uid.as_deref().unwrap_or("-")
            );
        }
        Cmd::Status => {
            let st: StatusData = ipc_client::call(&socket, req).await?;
            print_status(&st);
        }
        Cmd::Ls { .. } => {
            let d: LsData = ipc_client::call(&socket, req).await?;
            println!("# {}  ({} 项，经 daemon)", d.path, d.total);
            for e in &d.entries {
                print_entry(e);
            }
        }
        Cmd::Stat { .. } => {
            let e: Option<DirEntry> = ipc_client::call(&socket, req).await?;
            match e {
                Some(e) => {
                    print_entry(&e);
                    println!(
                        "    exist={} versioning_support={} privilege={}",
                        e.exist,
                        e.versioning_support,
                        e.privilege.as_deref().unwrap_or("-")
                    );
                }
                None => {
                    return Err(qxync_core::ipc::IpcError::new(
                        ErrorKind::BadRequest,
                        "目标不存在".to_string(),
                    ))
                }
            }
        }
        Cmd::Get { dir, name, .. } => {
            let d: GetData = ipc_client::call(&socket, req).await?;
            println!(
                "✅ {dir}/{name} -> {}  ({} 字节，经 daemon)",
                d.dest.display(),
                d.bytes
            );
        }
        Cmd::Put { local, .. } => {
            let d: PutData = ipc_client::call(&socket, req).await?;
            println!(
                "✅ 上传 {} ({} 字节) -> {}，mtime={}（经 daemon）",
                local.display(),
                d.bytes,
                d.remote_path,
                d.mtime
            );
        }
        Cmd::Mkdir { parent, name } => {
            ipc_client::call_ok(&socket, req).await?;
            println!("✅ 已建目录 {parent}/{name}（经 daemon）");
        }
        Cmd::Mount { .. } => {
            let m: MountInfo = ipc_client::call(&socket, req).await?;
            println!(
                "✅ 已挂载 {} -> {}（只读，daemon 持有，pid 见 `qsync daemon status`）",
                m.mountpoint.display(),
                m.remote
            );
        }
        Cmd::Umount { mountpoint } => {
            ipc_client::call_ok(&socket, req).await?;
            println!("✅ 已卸载 {}", mountpoint.display());
        }
        Cmd::Pin { path, state } => {
            let v: serde_json::Value = ipc_client::call(&socket, req).await?;
            if state.is_some() {
                println!(
                    "✅ pin {} = {}",
                    v.get("path").and_then(|x| x.as_str()).unwrap_or(path),
                    v.get("pin").and_then(|x| x.as_str()).unwrap_or("?")
                );
            } else {
                println!(
                    "{}",
                    v.get("pin")
                        .and_then(|x| x.as_str())
                        .unwrap_or("unspecified")
                );
            }
        }
        Cmd::State { .. } | Cmd::Daemon { .. } => unreachable!(),
    }
    Ok(())
}

fn print_status(st: &StatusData) {
    println!(
        "daemon    : v{} pid={} uptime={}s",
        st.daemon.version, st.daemon.pid, st.daemon.uptime_secs
    );
    println!("socket    : {}", st.daemon.socket);
    println!(
        "连接      : {} ({}://{}:{})",
        st.link.id,
        if st.link.https { "https" } else { "http" },
        st.link.host,
        st.link.port
    );
    println!(
        "用户      : {}{}",
        st.link.user,
        if st.link.ipv4_only {
            "  [仅 IPv4]"
        } else {
            ""
        }
    );
    println!(
        "登录状态  : {}",
        if st.logged_in {
            "已登录"
        } else {
            "未登录（先 `qsync login`）"
        }
    );
    if let Some(s) = &st.session {
        println!("会话      : sid={} 存活={}", s.sid_masked, s.alive);
    }
    if let Some(s) = &st.server {
        println!(
            "服务端    : Qsync {} / QPKG {} / build {}  qbox_cgi={} fcgi={}",
            s.qsync_version.as_deref().unwrap_or("-"),
            s.qpkg_version.as_deref().unwrap_or("-"),
            s.build.as_deref().unwrap_or("-"),
            s.qbox_cgi,
            s.fcgi
        );
        if let Some(r) = &s.busy_reason {
            println!("⚠️  {r} —— 同步应暂停");
        }
    }
    if let Some(c) = &st.cursors {
        println!(
            "游标      : max_log={} global_notify={} sync_signal={}",
            c.max_log, c.global_notify, c.sync_signal
        );
    }
    println!(
        "水合统计  : {} 次 / {} 字节",
        st.hydro.count, st.hydro.bytes
    );
    if st.mounts.is_empty() {
        println!("挂载      : （无）");
    } else {
        for m in &st.mounts {
            println!(
                "挂载      : {} -> {}{}",
                m.mountpoint.display(),
                m.remote,
                if m.readonly { " (ro)" } else { "" }
            );
        }
    }
    let _ = CursorInfo {
        max_log: 0,
        global_notify: 0,
        sync_signal: 0,
    };
}

async fn daemon_cmd(cli: &Cli, socket: &std::path::Path, action: &DaemonAction) -> Result<()> {
    match action {
        DaemonAction::Start => {
            if ipc_client::available(socket).await {
                println!("✅ qxyncd 已在运行（{}）", socket.display());
                return Ok(());
            }
            let exe = daemon_binary()?;
            let mut cmd = std::process::Command::new(&exe);
            cmd.arg("--link").arg(&cli.link).arg("--socket").arg(socket);
            let status = cmd.status().context("启动 qxyncd 失败")?;
            if !status.success() {
                bail!("qxyncd 启动失败（退出码 {:?}）", status.code());
            }
            let mut ready = false;
            for _ in 0..40 {
                if ipc_client::available(socket).await {
                    ready = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            if !ready {
                bail!("qxyncd 已派生但 socket 未就绪：{}", socket.display());
            }
            let p: PingData = ipc_client::call(socket, Request::Ping)
                .await
                .map_err(ipc_err)?;
            println!(
                "✅ qxyncd 已启动 pid={} version={} socket={}",
                p.pid,
                p.daemon_version,
                socket.display()
            );
        }
        DaemonAction::Stop => {
            if !ipc_client::available(socket).await {
                println!("qxyncd 未在运行");
                return Ok(());
            }
            ipc_client::call_ok(socket, Request::Shutdown)
                .await
                .map_err(ipc_err)?;
            for _ in 0..40 {
                if !ipc_client::available(socket).await {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            println!("✅ qxyncd 已停止");
        }
        DaemonAction::Status => {
            if !ipc_client::available(socket).await {
                bail!("qxyncd 未在运行（socket {}）", socket.display());
            }
            let p: PingData = ipc_client::call(socket, Request::Ping)
                .await
                .map_err(ipc_err)?;
            let st: StatusData = ipc_client::call(socket, Request::Status)
                .await
                .map_err(ipc_err)?;
            println!(
                "pong      : {} v{} pid={} uptime={}s",
                p.pong, p.daemon_version, p.pid, p.uptime_secs
            );
            print_status(&st);
        }
    }
    Ok(())
}
