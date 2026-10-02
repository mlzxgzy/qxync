//! `qxync` —— 前端 CLI（M0：协议打通）。
//!
//! 后端守护 `qxyncd` 尚未实现，本版 CLI 直接驱动 `qxync-client` 对 NAS 跑通
//! 「登录 → 列举 → stat → 下载 → 上传 → 建目录」这条链，作为 M0 的验收工具。

mod ipc_client;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use qxync_client::Client;
use qxync_core::ipc::{
    default_socket_path, CacheInfo, CursorInfo, DecisionsData, DehydrateData, ErrorKind,
    FileStatesData, GetData, LsData, MountInfo, PeerData, PingData, PutData, Request, RootsData,
    RulesData, SettingsData, SpaceData, StatusData, StoreData, SyncInfo, TasksData,
};
use qxync_core::settings::{Settings, PROXY_AUTO, PROXY_MANUAL, PROXY_NONE};
use qxync_core::{ConfigPaths, Credentials, DirEntry, LinkConfig, HOME_ROOT};
use qxync_fuse::{CacheMode, QxyncFs};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(
    name = "qxync",
    version,
    about = "qxync —— QNAP Qsync 的 Linux on-demand 同步客户端（M0 协议版）"
)]
struct Cli {
    /// 连接 id（对应 ~/.config/qxync/links/<id>.json）
    #[arg(long, global = true, default_value = "default")]
    link: String,

    /// 覆盖 NAS 地址（配合 --user/--password 首次登录）
    #[arg(long, global = true, env = "QXNYC_HOST")]
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
    #[arg(long, global = true, env = "QXNYC_SOCKET")]
    socket: Option<PathBuf>,

    /// 强制走 daemon IPC（默认：socket 可连就走 daemon）
    #[arg(long, global = true, conflicts_with = "direct")]
    via_daemon: bool,

    /// 强制直连 NAS（忽略 daemon）
    #[arg(long, global = true)]
    direct: bool,

    #[arg(long, global = true, env = "QXNYC_USER")]
    user: Option<String>,

    /// 口令；不给则读 credentials.json，再不给则报错（避免落进 shell 历史）
    #[arg(long, global = true, env = "QXNYC_PASSWORD", hide_env_values = true)]
    password: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// 登录并把凭据写入 ~/.config/qxync/credentials.json (0600)
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
        /// 远端根（可重复：`--remote /home --remote /Public`）；默认 /home
        #[arg(long = "remote", default_value = HOME_ROOT)]
        remote: Vec<String>,
        /// 水合缓存目录（默认 ~/.local/share/qxync/cache）
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
        /// 读写挂载（M2b 写路径：本地改动会经上传队列推回 NAS）
        #[arg(long)]
        rw: bool,
        /// ★ M2c：本地大批删除熔断阈值（60 秒窗口内最多删多少项；0 = 关闭，默认 100）
        #[arg(long)]
        delete_limit: Option<usize>,
        /// ★ M3：缓存模式 `pagecache`（默认，脱水需先 inval_inode）/ `direct`（绕过 page cache，mmap 不可用）
        #[arg(long, value_name = "MODE")]
        cache_mode: Option<String>,
        /// ★ M8.4：冲突策略 ask | rename_remote | rename_local | replace_remote | replace_local
        /// （默认 rename_local = M2c 既有行为）
        #[arg(long, value_name = "POLICY")]
        conflict: Option<String>,
    },
    /// 卸载 FUSE 挂载点
    Umount { mountpoint: PathBuf },

    /// ★ M2c：变更发现（三游标轮询 + baseline 对账 + 冲突/删除保护）
    Sync {
        /// 立即跑一轮（否则只显示状态）
        #[arg(long)]
        once: bool,
        /// 解除删除保护熔断（放行这一轮的批量删除；同时解除 FUSE 侧本地删除熔断）
        #[arg(long)]
        force_deletes: bool,
        /// 临时改「一次对账最多删多少项」
        #[arg(long)]
        max_deletes: Option<usize>,
        /// 调整后台轮询间隔秒数（0 = 暂停）
        #[arg(long)]
        interval: Option<u64>,
        /// 直接输出 JSON（脚本/验收用；`--once` 后 polls 等计数会变）
        #[arg(long)]
        json: bool,
    },

    /// ★ M6：远端根一览（配置的 roots + NAS 上的同步文件夹 + 可读/可写判定）
    Roots {
        /// 直接输出 JSON（脚本/验收用）
        #[arg(long)]
        json: bool,
    },

    /// ★ M7：选择性同步规则（exclude + 内置临时文件过滤）；`--match` 判定单条路径
    Rules {
        /// 判定这条远端路径会不会在挂载点里被隐藏（如 /home/qxync-test/secret）
        #[arg(long)]
        r#match: Option<String>,
        /// 直接输出 JSON（脚本/验收用）
        #[arg(long)]
        json: bool,
    },

    /// ★ M8.3：同步活动日志（文件更新中心 / 错误列表的数据源）
    Journal {
        /// 最多返回多少条（默认 200）
        #[arg(long)]
        limit: Option<usize>,
        /// 只要 unix 秒 >= 这个值的（闭区间）
        #[arg(long)]
        since: Option<i64>,
        /// 按文件名 / 说明做子串过滤
        #[arg(long)]
        query: Option<String>,
        /// ok | error | blocked | all（默认 all）；`error` 就是「错误列表」
        #[arg(long)]
        level: Option<String>,
        /// 清空日志（**只清日志**，不动游标/baseline/pin/队列）
        #[arg(long)]
        clear: bool,
        /// 直接输出 JSON（脚本/验收用）
        #[arg(long)]
        json: bool,
    },

    /// ★ M8.2：同步任务（持久化的挂载登记 + 策略）
    Task {
        #[command(subcommand)]
        action: TaskAction,
    },

    /// ★ M7：LAN 对等设备（配对 / 探活 / 事件快路径 / 直传自检）
    Peer {
        #[command(subcommand)]
        action: PeerAction,
    },

    /// 删除远端文件/目录（M2c 脚本/测试用；挂在挂载点上 rm 走 FUSE）
    Rm { dir: String, name: String },

    /// ★ M3：脱水（丢掉本地缓存内容、只留占位符；远端数据不动）
    ///
    /// 安全检查（报告 12 §8.1）：pin=pinned/excluded、有未上传改动、有打开的 fd、
    /// 被 mmap、正在水合、刚访问过 —— 任一命中就跳过。
    /// 执行顺序铁则：先 inval_inode 让内核失效，再清内容，最后更新占位符状态。
    Dehydrate {
        /// 只处理这个远端路径（如 /home/qxync-test/big.bin）
        #[arg(long)]
        path: Option<String>,
        /// 处理所有挂载点里所有可脱水的文件（默认：按闲置/限额配置扫）
        #[arg(long)]
        all: bool,
        /// 只清闲置 ≥ N 秒的文件（0 = 不限制；给这个值也会覆盖后台配置）
        #[arg(long)]
        idle_secs: Option<u64>,
        /// 缓存限额，如 512M / 2G / 25%（按 LRU 清到不超限）
        #[arg(long)]
        cache_limit: Option<String>,
        /// 跳过「刚访问过」保护窗口（默认 300s）
        #[arg(long)]
        force: bool,
        /// 只算不删
        #[arg(long)]
        dry_run: bool,
        /// 只处理这个挂载点
        #[arg(long)]
        mount: Option<PathBuf>,
    },

    /// ★ M8.4：全局设置（settings.json）—— 不带 `--set` 就是只读展示
    Settings {
        /// 点号键赋值，可重复：`--set proxy.mode=manual --set proxy.server=10.0.0.1`
        /// （可用键见 `qxync settings --help`）
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,
        /// 开机自启要写进 autostart 桌面项的可执行文件（默认找同目录的 qxync-gui）
        #[arg(long, value_name = "PATH")]
        autostart_exe: Option<PathBuf>,
        /// 直接输出 JSON（脚本/验收用）
        #[arg(long)]
        json: bool,
    },

    /// ★ M8.4：释放空间状态 / 立即释放空间（Free Up Space Now）
    Space {
        /// 立即按当前策略跑一轮脱水（仍走 M3 安全检查链）
        #[arg(long)]
        now: bool,
        /// 直接输出 JSON（脚本/验收用）
        #[arg(long)]
        json: bool,
    },

    /// ★ M8.4：冲突待裁决队列（策略 = 每个文件都问我）
    Conflicts {
        /// 裁决某一条：`--resolve <id> --as keep_local|keep_remote|keep_both`
        #[arg(long, value_name = "ID")]
        resolve: Option<String>,
        /// 裁决方式：保留本地 / 保留 NAS 上的 / 两份都留
        #[arg(long = "as", value_name = "RESOLUTION")]
        resolution: Option<String>,
        /// 清空队列（**只清队列，不动文件**）
        #[arg(long)]
        clear: bool,
        /// 直接输出 JSON（脚本/验收用）
        #[arg(long)]
        json: bool,
    },

    /// ★ M8.4：文件三态（仅在线 / 本地可用 / 始终可用）—— 文件页那一列的 CLI 形态
    FileStates {
        /// 远端目录（默认 /home）
        #[arg(default_value = HOME_ROOT)]
        path: String,
        /// 直接输出 JSON（脚本/验收用）
        #[arg(long)]
        json: bool,
    },

    /// ★ M5：本地状态库（SQLite）—— 游标 / baseline / pin / 上传队列
    Store {
        /// 顺带跑 `PRAGMA integrity_check`（验收脚本用）
        #[arg(long)]
        integrity: bool,
        /// 直接输出 JSON（给脚本/验收用，别解析人读文本）
        #[arg(long)]
        json: bool,
    },

    /// 查看/设置 pin：`qxync pin <远端路径> [pinned|unpinned|unspecified|excluded]`
    Pin { path: String, state: Option<String> },

    /// 查看某路径的占位符状态（pin + 远端元数据）
    State { path: String },

    /// 守护进程生命周期
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
}

/// ★ M8.2：`qxync task` 的子命令。
#[derive(Subcommand, Debug)]
enum TaskAction {
    /// 列出全部任务（含「此刻是否真的挂着」）
    List {
        /// 直接输出 JSON（脚本/验收用）
        #[arg(long)]
        json: bool,
    },
    /// 新建/覆盖一个任务登记（默认同时挂载；`--no-mount` 只登记）
    Add {
        /// 任务 id（也是文件名，只允许 A-Za-z0-9 . _ -）
        #[arg(long, default_value = "default")]
        id: String,
        /// 本地挂载点（绝对路径，不存在会被创建）
        #[arg(long)]
        mountpoint: PathBuf,
        /// 远端根（可重复；不填 = 用 link 的 home_root）
        #[arg(long = "root")]
        roots: Vec<String>,
        /// 读写挂载（默认只读）
        #[arg(long)]
        read_write: bool,
        /// 缓存模式 pagecache | direct
        #[arg(long)]
        cache_mode: Option<String>,
        /// 水合缓存目录（**父目录**；实际缓存会再拼一层 NAS 主机名）。
        /// 不填 = 默认 ~/.local/share/qxync/cache
        #[arg(long)]
        cache_dir: Option<PathBuf>,
        /// 只登记、不挂载
        #[arg(long)]
        no_mount: bool,
        /// ★ M8.4：冲突策略（5 个取值，见 `qxync mount --help`）
        #[arg(long, value_name = "POLICY")]
        conflict: Option<String>,
        /// 直接输出 JSON（脚本/验收用）
        #[arg(long)]
        json: bool,
    },
    /// 删除任务登记（**只删登记，不动挂载点里的任何数据**）
    Rm {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// 暂停：停用登记并卸载该挂载点（已入队的上传会先排空，不丢改动）
    Pause {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// 继续：启用登记并重新挂载
    Resume {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// 按任务登记的参数挂载（不改登记）
    Mount {
        id: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum PeerAction {
    /// 对等主机状态：监听地址 / 身份 / 配对码 / 已配对设备 / 计数
    Status {
        #[arg(long)]
        json: bool,
    },
    /// 已配对设备（token 掩码）
    List {
        #[arg(long)]
        json: bool,
    },
    /// 配对：`qxync peer pair 192.168.1.5:9840 --code 4821`
    Pair {
        /// 对端地址 host:port
        addr: String,
        /// 对方 `qxync peer status` 显示的 6 位配对码
        #[arg(long)]
        code: String,
    },
    /// 探活（地址或已配对设备名）
    Ping {
        target: String,
        #[arg(long)]
        json: bool,
    },
    /// 最近收到的对端事件（事件快路径）
    Events {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// 手动广播一个「某路径可能变了」的事件给所有对端（脚本/验收用）
    Notify { path: String },
    /// 从对端直传一份内容到本地（LAN 直连自检，不经过 NAS）
    Fetch {
        /// 对端（已配对设备名或地址）
        target: String,
        /// 远端路径
        path: String,
        /// 本地输出文件
        dest: PathBuf,
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
            // ★ M6：未配 roots 时由 `LinkConfig::roots()` 退回家目录根
            roots: Vec::new(),
            ipv4_only: cli.ipv4,
            // ★ M7：登录时先不定选择性同步规则（改 link JSON 后重启 daemon 生效）
            exclude: Vec::new(),
            filter_temp: true,
            peer_listen: None,
            peer_name: None,
        }),
        _ => bail!(
            "没有找到连接配置 {}，且缺少 --host/--user。\n首次使用：qxync --host <NAS> --port <端口> --insecure --user <用户> --password <口令> login",
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
    bail!("没有口令：用 --password / QXNYC_PASSWORD，或先跑 `qxync login --password` 写凭据")
}

async fn connect(cli: &Cli, need_creds: bool) -> Result<(Client, LinkConfig)> {
    let link = resolve_link(cli)?;
    // ★ M8.4：直连模式也走 settings.json 里的代理（否则「设置里配了代理」在 CLI 直连时失效）
    let mut client = match Settings::load(&paths()?) {
        Ok(st) => Client::new_with_settings(&link, &st)?,
        Err(_) => Client::new(&link)?,
    };
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
    // Rust 默认忽略 SIGPIPE → `qxync ls | head` 会在 println! 上 EPIPE panic。
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
        // ★ M8.4：设置不依赖 daemon（就是读写 settings.json + autostart 桌面项）；
        //   但**跑着的 daemon 不会自动重读**，所以这里提示一句。
        Cmd::Settings {
            set,
            autostart_exe,
            json,
        } => {
            let paths = paths()?;
            let mut st = Settings::load(&paths)?;
            let mut saved = false;
            if !set.is_empty() {
                for kv in set {
                    let (k, v) = kv
                        .split_once('=')
                        .ok_or_else(|| anyhow::anyhow!("--set 需要 key=value，收到 {kv:?}"))?;
                    st.set_kv(k.trim(), v)?;
                }
                let exe = autostart_exe.clone().or_else(|| default_gui_exe_from_cli());
                st.apply_autostart(
                    &paths,
                    exe.as_deref().unwrap_or(std::path::Path::new("qxync-gui")),
                )?;
                st.save(&paths)?;
                saved = true;
                eprintln!("⚠️ 直连模式：设置已写入文件；正在运行的 daemon 要重启才会读到代理/释放空间的新值");
            }
            print_settings(&st, &paths, *json, saved)?;
        }
        Cmd::Pin { .. }
        | Cmd::State { .. }
        | Cmd::Sync { .. }
        | Cmd::Dehydrate { .. }
        | Cmd::Space { .. }
        | Cmd::Conflicts { .. }
        | Cmd::FileStates { .. }
        | Cmd::Store { .. }
        | Cmd::Roots { .. }
        | Cmd::Rules { .. }
        | Cmd::Task { .. }
        | Cmd::Journal { .. }
        | Cmd::Peer { .. } => {
            bail!("`pin`/`state`/`sync`/`dehydrate`/`space`/`conflicts`/`file-states`/`store`/`roots`/`rules`/`task`/`peer` 需要 daemon：先 `qxync daemon start`（或用 --socket 指向运行中的 daemon）");
        }
        Cmd::Rm { dir, name } => {
            let (client, _) = connect(&cli, true).await?;
            client.delete_entry(dir, name).await?;
            let _ = client
                .write_log(
                    &format!("{}/{}", dir.trim_end_matches('/'), name),
                    qxync_client::write_action::DELETE,
                )
                .await;
            println!("✅ 已删除 {dir}/{name}");
        }
        Cmd::Daemon { .. } => unreachable!("daemon 命令在路由前已处理"),
        Cmd::Mount {
            mountpoint,
            remote,
            cache_dir,
            threads,
            auto_unmount,
            hydrate_timeout,
            rw,
            delete_limit,
            cache_mode,
            // ★ M8.4：直连模式不接冲突策略（引擎在 daemon 侧）
            conflict: _,
        } => {
            // 直连模式的 FUSE 进程只挂一个根；多根需要 daemon 侧合成视图。
            if remote.len() > 1 {
                bail!("多根挂载需要 daemon：先 `qxync daemon start`（或用 --via-daemon）");
            }
            let remote = remote
                .first()
                .cloned()
                .unwrap_or_else(|| HOME_ROOT.to_string());
            let (client, link) = connect(&cli, true).await?;
            let cache = cache_dir.clone().unwrap_or_else(|| {
                paths()
                    .map(|p| p.data_dir.join("cache"))
                    .unwrap_or_else(|_| PathBuf::from("/tmp/qxync-cache"))
            });
            let client = Arc::new(client);
            let mut fs = QxyncFs::new(client.clone(), remote.clone(), cache.clone())
                .context("初始化 FUSE 文件系统失败")?
                .with_hydrate_timeout(std::time::Duration::from_secs(*hydrate_timeout));
            if let Some(limit) = delete_limit {
                fs = fs.with_delete_limit(*limit);
            }
            if let Some(m) = cache_mode {
                let mode = CacheMode::parse(m)
                    .with_context(|| format!("cache_mode 只能是 pagecache/direct，收到 {m:?}"))?;
                fs = fs.with_cache_mode(mode);
            }
            let mut queue = None;
            if *rw {
                // 写路径：上传队列（worker 线程把本地改动推回 NAS）
                let marker_dir = paths()
                    .map(|p| p.data_dir.join("upload-queue"))
                    .unwrap_or_else(|_| cache.join("upload-queue"));
                let q = qxync_fuse::upload::UploadQueue::new(
                    client.clone(),
                    tokio::runtime::Handle::current(),
                    marker_dir,
                )
                .context("创建上传队列失败")?;
                q.spawn_worker().context("启动上传 worker 失败")?;
                fs = fs.with_write_mode().with_upload_queue(q.clone());
                queue = Some(q);
            }
            println!(
                "挂载 {} -> {}",
                format_args!("{}:{}", link.host, remote),
                mountpoint.display()
            );
            println!("  缓存目录 : {}", cache.display());
            println!(
                "  {} + on-demand 区间水合（TTL=0.5s，{threads} 线程，auto_unmount={auto_unmount}）",
                if *rw { "读写" } else { "只读" }
            );
            println!(
                "  卸载     : qxync umount {}  （或 fusermount3 -u）",
                mountpoint.display()
            );
            let mnt = mountpoint.clone();
            let (n, au, ro) = (*threads, *auto_unmount, !*rw);
            let _ = &queue;
            // 用普通 OS 线程跑 FUSE 会话：tokio 的 spawn_blocking 线程带着 runtime 上下文，
            // 在回调里再 block_on 另一个 runtime 会踩 "Cannot start a runtime from within a runtime"。
            let handle = std::thread::Builder::new()
                .name("qxync-fuse".into())
                .spawn(move || qxync_fuse::mount(fs, &mnt, n, au, ro))
                .context("创建 FUSE 线程失败")?;
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("FUSE 线程 panic"))?
                .context("挂载失败")?;
            println!("已卸载 {}", mountpoint.display());
        }
        Cmd::Umount { mountpoint } => {
            // 注意：直连模式下队列在挂载进程里，这里只负责卸载；
            // daemon 模式由 daemon 侧排空队列后再卸载。
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

/// 参数/设置错误：CLI 直接退出（与 clap 的参数错误同一档，退出码 2）。
fn fatal(msg: &str) -> ! {
    eprintln!("❌ {msg}");
    std::process::exit(2);
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
            rw,
            delete_limit,
            cache_mode,
            conflict,
        } => Request::Mount {
            mountpoint: mountpoint.clone(),
            remote: Some(remote[0].clone()),
            roots: Some(remote.clone()),
            cache_dir: cache_dir.clone(),
            threads: Some(*threads),
            auto_unmount: Some(*auto_unmount),
            // ★ M8.2：CLI 的 `mount` 默认**不登记任务**（行为与 M7 一致）；
            //   要登记用 `qxync task add`。
            task: None,
            save_task: None,
            hydrate_timeout_secs: Some(*hydrate_timeout),
            read_write: Some(*rw),
            delete_limit: *delete_limit,
            cache_mode: cache_mode.clone(),
            conflict: conflict.clone(),
        },
        Cmd::Umount { mountpoint } => Request::Umount {
            mountpoint: mountpoint.clone(),
        },
        Cmd::Pin { path, state } => Request::Pin {
            path: path.clone(),
            state: state.clone(),
        },
        Cmd::Sync {
            once,
            force_deletes,
            max_deletes,
            interval,
            ..
        } => Request::Sync {
            once: Some(*once),
            force_deletes: Some(*force_deletes),
            max_deletes: *max_deletes,
            interval_secs: *interval,
        },
        Cmd::Rm { dir, name } => Request::Rm {
            dir: dir.clone(),
            name: name.clone(),
        },
        Cmd::Roots { .. } => Request::Roots,
        Cmd::Settings {
            set, autostart_exe, ..
        } => {
            // 不带 --set = 只读；带了就是「读-改-写」：先把当前设置取回来，
            // 逐键改，再整体回写（**顺序无关**，见 Settings::set_kv）。
            if set.is_empty() {
                Request::Settings
            } else {
                let mut want =
                    local_settings().unwrap_or_else(|e| fatal(&format!("读取设置失败: {e}")));
                for kv in set {
                    let Some((k, v)) = kv.split_once('=') else {
                        fatal(&format!("--set 需要 key=value，收到 {kv:?}"));
                    };
                    if let Err(e) = want.set_kv(k.trim(), v) {
                        fatal(&e.to_string());
                    }
                }
                Request::SettingsSave {
                    settings: want,
                    autostart_exe: autostart_exe.as_ref().map(|p| p.display().to_string()),
                }
            }
        }
        Cmd::Space { now, .. } => Request::Space { now: Some(*now) },
        Cmd::Conflicts {
            resolve,
            resolution,
            clear,
            ..
        } => Request::Decisions {
            action: if *clear {
                "clear".to_string()
            } else if resolve.is_some() {
                "resolve".to_string()
            } else {
                "list".to_string()
            },
            id: resolve.clone(),
            resolution: resolution.clone(),
        },
        Cmd::FileStates { path, .. } => Request::FileStates { path: path.clone() },
        Cmd::Journal {
            limit,
            since,
            query,
            level,
            clear,
            ..
        } => Request::Journal {
            limit: *limit,
            since: *since,
            query: query.clone(),
            level: level.clone(),
            clear: Some(*clear),
        },
        Cmd::Task { action } => match action {
            TaskAction::List { .. } => Request::Tasks {
                action: "list".into(),
                id: None,
                task: None,
            },
            TaskAction::Add {
                id,
                mountpoint,
                roots,
                read_write,
                cache_mode,
                conflict,
                ..
            } => {
                Request::Tasks {
                    action: "save".into(),
                    id: None,
                    task: Some(
                        qxync_core::tasks::Task::from_mount(
                            Some(id.clone()),
                            mountpoint.clone(),
                            roots.clone(),
                            *read_write,
                            cache_mode.clone(),
                            None,
                            None,
                            None,
                            Some(true),
                        )
                        // ★ M8.4：任务上的冲突策略
                        .with_conflict(conflict.clone()),
                    ),
                }
            }
            TaskAction::Rm { id, .. } => Request::Tasks {
                action: "delete".into(),
                id: Some(id.clone()),
                task: None,
            },
            TaskAction::Pause { id, .. } => Request::Tasks {
                action: "pause".into(),
                id: Some(id.clone()),
                task: None,
            },
            TaskAction::Resume { id, .. } => Request::Tasks {
                action: "resume".into(),
                id: Some(id.clone()),
                task: None,
            },
            TaskAction::Mount { id, .. } => Request::Tasks {
                action: "mount".into(),
                id: Some(id.clone()),
                task: None,
            },
        },
        Cmd::Rules { r#match, .. } => Request::Rules {
            match_path: r#match.clone(),
        },
        Cmd::Peer { action } => match action {
            PeerAction::Status { .. } => Request::Peer {
                action: "status".into(),
                addr: None,
                code: None,
                name: None,
                path: None,
                dest: None,
                limit: None,
            },
            PeerAction::List { .. } => Request::Peer {
                action: "list".into(),
                addr: None,
                code: None,
                name: None,
                path: None,
                dest: None,
                limit: None,
            },
            PeerAction::Pair { addr, code } => Request::Peer {
                action: "pair".into(),
                addr: Some(addr.clone()),
                code: Some(code.clone()),
                name: None,
                path: None,
                dest: None,
                limit: None,
            },
            PeerAction::Ping { target, .. } => Request::Peer {
                action: "ping".into(),
                addr: Some(target.clone()),
                code: None,
                name: None,
                path: None,
                dest: None,
                limit: None,
            },
            PeerAction::Events { limit, .. } => Request::Peer {
                action: "events".into(),
                addr: None,
                code: None,
                name: None,
                path: None,
                dest: None,
                limit: Some(*limit),
            },
            PeerAction::Notify { path } => Request::Peer {
                action: "notify".into(),
                addr: None,
                code: None,
                name: None,
                path: Some(path.clone()),
                dest: None,
                limit: None,
            },
            PeerAction::Fetch { target, path, dest } => Request::Peer {
                action: "fetch".into(),
                addr: Some(target.clone()),
                code: None,
                name: None,
                path: Some(path.clone()),
                dest: Some(dest.clone()),
                limit: None,
            },
        },
        Cmd::Store { integrity, .. } => Request::Store {
            integrity: Some(*integrity),
        },
        Cmd::Dehydrate {
            path,
            all,
            idle_secs,
            cache_limit,
            force,
            dry_run,
            mount,
        } => Request::Dehydrate {
            path: path.clone(),
            all: Some(*all),
            idle_secs: *idle_secs,
            cache_limit: cache_limit.clone(),
            force: Some(*force),
            dry_run: Some(*dry_run),
            mountpoint: mount.clone(),
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
        Cmd::Mount { rw, .. } => {
            let m: MountInfo = ipc_client::call(&socket, req).await?;
            let roots = if m.roots.is_empty() {
                m.remote.clone()
            } else {
                m.roots.join(", ")
            };
            println!(
                "✅ 已挂载 {} -> {}（{}，daemon 持有，pid 见 `qxync daemon status`）",
                m.mountpoint.display(),
                roots,
                if *rw { "读写" } else { "只读" }
            );
        }
        Cmd::Task { action } => {
            run_task_cmd(&socket, action).await?;
        }
        Cmd::Journal { json, .. } => {
            let d: qxync_core::ipc::JournalData = ipc_client::call(&socket, req).await?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&d).unwrap_or_else(|_| "{}".into())
                );
            } else {
                print_journal(&d);
            }
        }
        Cmd::Settings { json, set, .. } => {
            let d: qxync_core::ipc::SettingsData = ipc_client::call(&socket, req).await?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&d).map_err(
                        |e| qxync_core::ipc::IpcError::new(ErrorKind::Parse, e.to_string())
                    )?
                );
            } else {
                print_settings_data(&d);
                if !set.is_empty() {
                    println!("✅ 已写入 {}", d.path);
                }
            }
        }
        Cmd::Space { json, now } => {
            let d: qxync_core::ipc::SpaceData = ipc_client::call(&socket, req).await?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&d).map_err(
                        |e| qxync_core::ipc::IpcError::new(ErrorKind::Parse, e.to_string())
                    )?
                );
            } else {
                print_space(&d, *now);
            }
        }
        Cmd::Conflicts { json, .. } => {
            let d: qxync_core::ipc::DecisionsData = ipc_client::call(&socket, req).await?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&d).map_err(
                        |e| qxync_core::ipc::IpcError::new(ErrorKind::Parse, e.to_string())
                    )?
                );
            } else {
                print_decisions(&d);
            }
        }
        Cmd::FileStates { json, .. } => {
            let d: qxync_core::ipc::FileStatesData = ipc_client::call(&socket, req).await?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&d).map_err(
                        |e| qxync_core::ipc::IpcError::new(ErrorKind::Parse, e.to_string())
                    )?
                );
            } else {
                print_file_states(&d);
            }
        }
        Cmd::Roots { json } => {
            let raw: serde_json::Value = ipc_client::call(&socket, req).await?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&raw).map_err(|e| {
                        qxync_core::ipc::IpcError::new(ErrorKind::Parse, e.to_string())
                    })?
                );
            } else {
                let d: RootsData = serde_json::from_value(raw)
                    .map_err(|e| qxync_core::ipc::IpcError::new(ErrorKind::Parse, e.to_string()))?;
                print_roots(&d);
            }
        }
        Cmd::Rules { json, .. } => {
            let raw: serde_json::Value = ipc_client::call(&socket, req).await?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&raw).map_err(|e| {
                        qxync_core::ipc::IpcError::new(ErrorKind::Parse, e.to_string())
                    })?
                );
            } else {
                let d: RulesData = serde_json::from_value(raw)
                    .map_err(|e| qxync_core::ipc::IpcError::new(ErrorKind::Parse, e.to_string()))?;
                print_rules(&d);
            }
        }
        Cmd::Peer { action } => {
            let raw: serde_json::Value = ipc_client::call(&socket, req).await?;
            let d: PeerData = serde_json::from_value(raw.clone())
                .map_err(|e| qxync_core::ipc::IpcError::new(ErrorKind::Parse, e.to_string()))?;
            let json = match action {
                PeerAction::Status { json }
                | PeerAction::List { json }
                | PeerAction::Ping { json, .. } => *json,
                PeerAction::Events { json, .. } => *json,
                _ => false,
            };
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&raw).map_err(|e| {
                        qxync_core::ipc::IpcError::new(ErrorKind::Parse, e.to_string())
                    })?
                );
            } else {
                print_peer(&d);
            }
        }
        Cmd::Sync { once, json, .. } => {
            let raw: serde_json::Value = ipc_client::call(&socket, req).await?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&raw).map_err(|e| {
                        qxync_core::ipc::IpcError::new(ErrorKind::Parse, e.to_string())
                    })?
                );
            } else {
                let info: SyncInfo = serde_json::from_value(raw)
                    .map_err(|e| qxync_core::ipc::IpcError::new(ErrorKind::Parse, e.to_string()))?;
                print_sync(&info);
                if *once {
                    println!("（以上为累计计数；本轮详情见 daemon 日志）");
                }
            }
        }
        Cmd::Rm { dir, name } => {
            ipc_client::call_ok(&socket, req).await?;
            println!("✅ 已删除 {dir}/{name}（经 daemon）");
        }
        Cmd::Dehydrate { dry_run, .. } => {
            let d: DehydrateData = ipc_client::call(&socket, req).await?;
            print_dehydrate(&d, *dry_run);
        }
        Cmd::Store { json, .. } => {
            let raw: serde_json::Value = ipc_client::call(&socket, req).await?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&raw).map_err(|e| {
                        qxync_core::ipc::IpcError::new(ErrorKind::Parse, e.to_string())
                    })?
                );
            } else {
                let d: StoreData = serde_json::from_value(raw)
                    .map_err(|e| qxync_core::ipc::IpcError::new(ErrorKind::Parse, e.to_string()))?;
                print_store(&d);
            }
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

// ---------------------------------------------------------------- ★ M8.4 打印与本地设置

/// 本地读设置（不带 --set 时就是「展示当前设置」）。
fn local_settings() -> Result<Settings> {
    Ok(Settings::load(&paths()?)?)
}

/// 直连模式下找 GUI 可执行文件（用于 autostart 的 Exec=）。
fn default_gui_exe_from_cli() -> Option<PathBuf> {
    let cur = std::env::current_exe().ok()?;
    let cand = cur.parent()?.join("qxync-gui");
    cand.is_file().then_some(cand)
}

/// 代理模式 → Qsync 原文文案（§1.7）。
fn proxy_label(mode: &str) -> &'static str {
    match mode {
        PROXY_NONE => "No proxy（无代理）",
        PROXY_AUTO => "Auto-detect（自动检测）",
        PROXY_MANUAL => "Manual（手动）",
        _ => "未知",
    }
}

/// 直连模式的设置打印（没有 daemon 时的 `qxync settings`）。
fn print_settings(st: &Settings, paths: &ConfigPaths, json: bool, saved: bool) -> Result<()> {
    if json {
        let out = serde_json::json!({
            "settings": st,
            "path": Settings::file(paths).display().to_string(),
            "autostart_path": Settings::autostart_file(paths).display().to_string(),
            "autostart_present": Settings::autostart_present(paths),
            "saved": saved,
            "proxy_env": Settings::proxy_env(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    println!("设置文件  : {}", Settings::file(paths).display());
    println!("代理      : {}", proxy_label(&st.proxy.mode));
    if st.proxy.mode == PROXY_MANUAL {
        println!(
            "            服务器 {}:{}  认证 {}",
            if st.proxy.server.is_empty() {
                "-"
            } else {
                &st.proxy.server
            },
            st.proxy
                .port
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".into()),
            if st.proxy.auth {
                format!("开（{}）", st.proxy.user)
            } else {
                "关".into()
            }
        );
    }
    let env = Settings::proxy_env();
    if env.is_empty() {
        println!("            环境变量：（无 http_proxy/https_proxy）");
    } else {
        for (k, v) in &env {
            println!("            环境变量 {k}={v}");
        }
    }
    println!(
        "开机自启  : {}（桌面项 {}）",
        if st.launch_at_startup { "开" } else { "关" },
        if Settings::autostart_present(paths) {
            "存在"
        } else {
            "不存在"
        }
    );
    println!(
        "桌面通知  : {}   调试日志: {}   关闭进托盘: {}",
        if st.desktop_notifications {
            "开"
        } else {
            "关"
        },
        if st.debug_log { "开" } else { "关" },
        if st.close_to_tray { "开" } else { "关" }
    );
    println!(
        "释放空间  : {}（{}{}）",
        if st.free_space.auto {
            "自动"
        } else {
            "不自动"
        },
        if st.free_space.mode == "frequency" {
            format!("按频率 每 {} 小时", st.free_space.every_hours)
        } else {
            format!("当空间少于 {}%", st.free_space.below_pct)
        },
        if saved { "，本次已保存" } else { "" }
    );
    Ok(())
}

/// 经 daemon 的设置打印。
fn print_settings_data(d: &SettingsData) {
    let st = &d.settings;
    println!(
        "设置文件  : {}{}",
        d.path,
        if d.saved { "（本次已写入）" } else { "" }
    );
    println!("代理      : {}", proxy_label(&st.proxy.mode));
    if let Some(u) = &d.proxy_url {
        println!("            URL {u}");
    }
    if d.proxy_env.is_empty() {
        println!("            环境变量：（无 http_proxy/https_proxy）");
    } else {
        for (k, v) in &d.proxy_env {
            println!("            环境变量 {k}={v}");
        }
    }
    println!(
        "开机自启  : {}（桌面项 {}：{}）",
        if st.launch_at_startup { "开" } else { "关" },
        d.autostart_path,
        if d.autostart_present {
            "存在"
        } else {
            "不存在"
        }
    );
    println!(
        "桌面通知  : {}   调试日志: {}   关闭进托盘: {}",
        if st.desktop_notifications {
            "开"
        } else {
            "关"
        },
        if st.debug_log { "开" } else { "关" },
        if st.close_to_tray { "开" } else { "关" }
    );
    println!(
        "释放空间  : {}（{}）",
        if st.free_space.auto {
            "自动"
        } else {
            "不自动"
        },
        if st.free_space.mode == "frequency" {
            format!("按频率 每 {} 小时", st.free_space.every_hours)
        } else {
            format!("当空间少于 {}%", st.free_space.below_pct)
        }
    );
    if let Some(n) = &d.note {
        println!("说明      : {n}");
    }
}

/// 释放空间状态。
fn print_space(d: &SpaceData, now: bool) {
    println!(
        "量空间    : {}  总 {}  可用 {}（{}%）",
        d.fs_path,
        human_size(d.fs_total),
        human_size(d.fs_avail),
        d.fs_avail_pct
    );
    if d.injected {
        println!("⚠️         : 正在使用 QXNYC_TEST_FAKE_STATVFS 注入值（验收模式）");
    }
    println!("缓存占用  : {}", human_size(d.cache_used_bytes));
    println!(
        "自动释放  : {}（{}）",
        if d.auto { "开" } else { "关" },
        if d.mode == "frequency" {
            format!("按频率 每 {} 小时", d.every_hours)
        } else {
            format!("当空间少于 {}%", d.below_pct)
        }
    );
    println!(
        "本轮判定  : {} —— {}",
        if d.would_run {
            "会触发"
        } else {
            "不触发"
        },
        d.reason
    );
    if d.last_run_unix > 0 {
        println!("上次触发  : {}（unix）", d.last_run_unix);
    }
    if now || d.ran {
        println!(
            "立即释放  : 脱水 {} 个 / 释放 {} / 被挡下 {} 个",
            d.dehydrated,
            human_size(d.freed_bytes),
            d.blocked.len()
        );
        for (p, why) in d.blocked.iter().take(10) {
            println!("            挡下 {p}（{why}）");
        }
    }
    if let Some(n) = &d.note {
        println!("说明      : {n}");
    }
}

/// 冲突待裁决队列。
fn print_decisions(d: &DecisionsData) {
    println!(
        "待裁决    : {} 条（已裁决待执行 {} 条）",
        d.pending, d.resolved
    );
    if d.removed > 0 {
        println!("本次清空  : {} 条", d.removed);
    }
    if d.decisions.is_empty() {
        println!("（队列为空 —— 只有冲突策略为「每个文件都问我」时才会攒条目）");
    }
    for x in &d.decisions {
        println!(
            "  {:<10} {}  本地 {} / 远端 {}  {}",
            x.resolution.as_deref().unwrap_or("待裁决"),
            x.path,
            human_size(x.local_size),
            human_size(x.remote_size),
            x.id
        );
    }
    if let Some(n) = &d.note {
        println!("说明      : {n}");
    }
}

/// 文件三态。
fn print_file_states(d: &FileStatesData) {
    println!(
        "# {}（挂载点 {}，根 {}）",
        d.path,
        d.mountpoint.as_deref().unwrap_or("-"),
        d.root.as_deref().unwrap_or("-")
    );
    println!(
        "三态汇总  : 仅在线 {} · 本地可用 {} · 始终可用 {}",
        d.online, d.local, d.always
    );
    for e in &d.entries {
        println!(
            "{:<8} {:>12}  {:<12} {}{}",
            if e.is_dir {
                "目录"
            } else {
                match e.state.as_str() {
                    "always" => "始终可用",
                    "local" => "本地可用",
                    _ => "仅在线",
                }
            },
            human_size(e.size),
            e.pin,
            e.name,
            if e.dirty {
                "（有未上传改动）"
            } else {
                ""
            }
        );
    }
    if let Some(n) = &d.note {
        println!("说明      : {n}");
    }
}

/// ★ M5：本地状态库（SQLite：游标 / baseline / pin / 上传队列）。
fn print_store(d: &StoreData) {
    println!("状态库    : {}", d.path);
    println!("schema    : v{}", d.schema_version);
    if let Some(ic) = &d.integrity {
        println!("完整性    : {ic}");
    }
    println!(
        "游标      : config={} notify={} global_notify={} max_log_seen={}（日志空 -17 × {}）",
        d.cursors.config,
        d.cursors.notify,
        d.cursors.global_notify,
        d.cursors.max_log_seen,
        d.cursors.log_missing_count
    );
    println!("baseline  : {} 项", d.baseline_entries);
    println!("上传队列  : {} 个未完成作业", d.uploads);
    if d.pins.is_empty() {
        println!("pin       : （无）");
    } else {
        println!("pin       : {} 条", d.pins.len());
        for (path, state) in &d.pins {
            println!("            {state:<12} {path}");
        }
    }
}

/// ★ M7：选择性同步规则。
fn print_rules(d: &RulesData) {
    println!("远端根    : {}", d.roots.join(", "));
    println!("规则条数  : {}", d.patterns.len());
    if d.patterns.is_empty() {
        println!("排除规则  : （无 —— 全部同步）");
    } else {
        for p in &d.patterns {
            println!("            {p}");
        }
    }
    println!(
        "临时文件  : {}（内置 {} 条：{}）",
        if d.filter_temp { "开" } else { "关" },
        d.temp_patterns.len(),
        d.temp_patterns.join(" ")
    );
    if !d.bad.is_empty() {
        println!("⚠️  无法解析: {}", d.bad.join(", "));
    }
    match &d.match_path {
        None => println!("提示      : `qxync rules --match /home/xxx` 可判定单条路径"),
        Some(p) => {
            println!("判定路径  : {p}");
            println!(
                "归属根    : {}（相对路径 {}）",
                d.match_root.as_deref().unwrap_or("（不在任何根内）"),
                d.match_rel.as_deref().unwrap_or("-")
            );
            match d.match_reason.as_deref() {
                Some("excluded") => println!("结果      : 隐藏（命中排除规则）"),
                Some("temp") => println!("结果      : 隐藏（临时文件）"),
                Some("outside-roots") => println!("结果      : 不在同步范围内"),
                _ => println!("结果      : 可见"),
            }
        }
    }
}

/// ★ M7：对等设备 / 事件 / 直传。
fn print_peer(d: &PeerData) {
    match d.action.as_str() {
        "pair" => {
            println!(
                "✅ 已配对 {}（{}），token={}",
                d.paired_name.as_deref().unwrap_or("?"),
                d.paired_addr.as_deref().unwrap_or("?"),
                d.paired_token_masked.as_deref().unwrap_or("?")
            );
            if let Some(n) = &d.note {
                println!("⚠️  {n}");
            }
        }
        "ping" => {
            println!(
                "✅ {} @ {}（版本 {}，{} ms，可配对={}）",
                d.peer_name.as_deref().unwrap_or("?"),
                d.listen.as_deref().unwrap_or("?"),
                d.peer_version.as_deref().unwrap_or("?"),
                d.took_ms.unwrap_or(0),
                d.pairing_open.unwrap_or(false)
            );
            println!("对端根    : {}", d.peer_roots.join(", "));
        }
        "fetch" => {
            println!(
                "✅ 从 {} 直传 {} 字节 → {}（{} ms）",
                d.fetch_from.as_deref().unwrap_or("?"),
                d.fetch_bytes.unwrap_or(0),
                d.fetch_dest
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                d.took_ms.unwrap_or(0)
            );
        }
        "notify" => {
            println!(
                "✅ {}",
                d.note.clone().unwrap_or_else(|| "事件已广播".into())
            );
        }
        "events" => {
            if d.events.is_empty() {
                println!("（还没有收到对端事件）");
            }
            for e in &d.events {
                println!(
                    "  {:>10}  {:<9} {}  ({} 字节, ts={})",
                    e.path, e.kind, "", e.size, e.ts
                );
            }
        }
        _ => {
            // status / list
            println!(
                "身份      : {}（版本 {}）",
                d.identity,
                d.peer_version.as_deref().unwrap_or("?")
            );
            println!(
                "监听      : {}",
                match (&d.enabled, &d.listen) {
                    (true, Some(a)) => format!("{a}（已开启）"),
                    _ => "未开启（link.peer_listen 为空）".to_string(),
                }
            );
            println!("对外根    : {}", d.roots.join(", "));
            if let Some(code) = &d.pairing_code {
                println!(
                    "配对码    : {code}（对方执行 `qxync peer pair <本机地址> --code {code}`）"
                );
            } else {
                println!("配对码    : （已关闭）");
            }
            if d.devices.is_empty() {
                println!("已配对    : （无）");
            } else {
                println!("已配对    : {} 台", d.devices.len());
                for p in &d.devices {
                    println!(
                        "            {:<16} {:<22} {}",
                        p.name, p.addr, p.token_masked
                    );
                }
            }
            println!(
                "事件      : 收到 {} 条 / 发出 {} 条；被拒请求 {}",
                d.events_in, d.events_out, d.rejected
            );
            if let Some(n) = &d.note {
                println!("提示      : {n}");
            }
        }
    }
}

/// ★ M6：远端根一览（配置的 roots + NAS 同步文件夹 + 可读/可写判定）。
fn print_roots(d: &RootsData) {
    println!("家目录根  : {}", d.home_root);
    println!(
        "配置的根  : {}",
        if d.configured.is_empty() {
            "（无）".to_string()
        } else {
            d.configured.join(", ")
        }
    );
    if d.roots.is_empty() {
        println!("根列表    : （无）");
    } else {
        println!("根列表    :");
        for r in &d.roots {
            let view = if r.view_name.is_empty() {
                "-"
            } else {
                r.view_name.as_str()
            };
            let readable = if r.readable {
                "可读".to_string()
            } else {
                match &r.note {
                    Some(n) => format!("不可读：{n}"),
                    None => "不可读".to_string(),
                }
            };
            println!(
                "  {} {:<12} 视图名 {:<10}{}   {}",
                if r.readable { "✅" } else { "❌" },
                r.remote,
                view,
                if r.writable { "可写" } else { "只读" },
                readable
            );
        }
    }
    if d.syncing_folders.is_empty() {
        println!("NAS 同步文件夹 : （无 —— 该账号没有在 Qsync 里配同步文件夹）");
    } else {
        println!("NAS 同步文件夹 : {} 个", d.syncing_folders.len());
        for f in &d.syncing_folders {
            println!(
                "  · {}  权限={}  可删除={}  realpath={}",
                f.folder,
                f.permission,
                f.read_deletable,
                f.realpath.as_deref().unwrap_or("-")
            );
        }
    }
    if let Some(note) = &d.note {
        println!("说明      : {note}");
    }
}

/// ★ M3：缓存/脱水状态。
fn print_cache(c: &CacheInfo) {
    let limit = c
        .limit_bytes
        .map(|b| human_size(b))
        .unwrap_or_else(|| "未设".into());
    println!(
        "本地缓存  : {}（{}），已缓存 {} / 限额 {}｜水合文件 {} / 共 {}｜累计脱水 {} 次 / 释放 {}",
        c.mode,
        if c.idle_secs > 0 {
            format!("闲置 ≥ {}s 自动脱水", c.idle_secs)
        } else {
            "仅手动脱水".to_string()
        },
        human_size(c.used_bytes),
        limit,
        c.hydrated_files,
        c.total_files,
        c.dehydrated_total,
        human_size(c.freed_total_bytes)
    );
    if c.last_sweep_age_secs > 0 {
        println!(
            "            上次扫描 {}s 前｜被挡：dirty={} pinned={} open={} mmap={} 在途={}",
            c.last_sweep_age_secs,
            c.blocked_dirty,
            c.blocked_pinned,
            c.blocked_open,
            c.blocked_mapped,
            c.blocked_inflight
        );
    }
    if let Some(e) = &c.last_error {
        println!("            ⚠️  最近错误: {e}");
    }
}

/// ★ M3：`qxync dehydrate` 的结果。
fn print_dehydrate(d: &DehydrateData, dry_run: bool) {
    println!(
        "{}脱水 {} 个文件，释放 {}；本地缓存现 {} / 限额 {}",
        if dry_run { "[dry-run] " } else { "" },
        d.dehydrated,
        human_size(d.freed_bytes),
        human_size(d.used_bytes),
        d.limit_bytes
            .map(human_size)
            .unwrap_or_else(|| "未设".into())
    );
    for t in d.targets.iter().take(20) {
        println!("  - {t}");
    }
    if d.targets.len() > 20 {
        println!("  …（共 {} 个）", d.targets.len());
    }
    if !d.blocked.is_empty() {
        println!("  跳过 {} 个：", d.blocked.len());
        for (p, why) in d.blocked.iter().take(10) {
            println!("  · {p} —— {why}");
        }
        if d.blocked.len() > 10 {
            println!("  …（共 {} 个）", d.blocked.len());
        }
    }
}

/// 变更发现状态（`qxync sync` / `status` 共用）。
fn print_sync(s: &SyncInfo) {
    println!(
        "变更发现  : {}，每 {}s 一轮，已轮询 {} 次（上次 {}s 前）",
        if s.enabled { "启用" } else { "暂停" },
        s.interval_secs,
        s.polls,
        s.last_poll_age_secs
    );
    println!(
        "  游标    : notify={} config={} global_notify={} max_log_seen={}（日志空 -17 × {}）",
        s.cursors.notify,
        s.cursors.config,
        s.cursors.global_notify,
        s.cursors.max_log_seen,
        s.cursors.log_missing_count
    );
    println!(
        "  baseline: {} 项｜事件 {}｜远端刷新 {}｜入队上传 {}｜冲突 {}｜删除 {}｜删除被挡 {}",
        s.baseline_entries,
        s.events,
        s.refreshed,
        s.uploaded,
        s.conflicts,
        s.deleted,
        s.deletes_blocked
    );
    if !s.devices.is_empty() {
        println!("  事件设备: {}", s.devices.join("  "));
    }
    if let Some(r) = &s.delete_block_reason {
        println!("  ⚠️  {r}（`qxync sync --force-deletes` 放行）");
    }
    if let Some(e) = &s.last_error {
        println!("  ⚠️  最近错误: {e}");
    }
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
            "未登录（先 `qxync login`）"
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
    if let Some(c) = &st.cache {
        print_cache(c);
    }
    if let Some(u) = &st.uploads {
        println!(
            "上传队列  : 待上传 {}｜上传中 {}｜完成 {}｜失败 {}｜重试 {}｜{} 字节",
            u.pending,
            if u.active { "yes" } else { "no" },
            u.done,
            u.failed,
            u.retries,
            u.bytes
        );
    }
    if let Some(s) = &st.sync {
        print_sync(s);
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

// ---------------------------------------------------------------- ★ M8.2 任务

fn parse_err(msg: String) -> qxync_core::ipc::IpcError {
    qxync_core::ipc::IpcError::new(qxync_core::ipc::ErrorKind::Parse, msg)
}

/// `qxync task …`
///
/// 与其它命令不同：**部分动作需要两次 IPC 调用**（`add` = 先登记再挂载），
/// 所以这里自己发请求，而不是只用 `to_request` 生成的那一个。
async fn run_task_cmd(
    socket: &std::path::Path,
    action: &TaskAction,
) -> Result<(), qxync_core::ipc::IpcError> {
    let dumps = |v: &serde_json::Value| -> Result<String, qxync_core::ipc::IpcError> {
        serde_json::to_string_pretty(v).map_err(|e| parse_err(e.to_string()))
    };
    match action {
        TaskAction::List { json } => {
            let raw: serde_json::Value = ipc_client::call(
                socket,
                Request::Tasks {
                    action: "list".into(),
                    id: None,
                    task: None,
                },
            )
            .await?;
            if *json {
                println!("{}", dumps(&raw)?);
                return Ok(());
            }
            let d: TasksData = serde_json::from_value(raw)
                .map_err(|e| parse_err(format!("解析 tasks 响应失败: {e}（daemon 版本过旧？）")))?;
            print_tasks(&d);
        }
        TaskAction::Add {
            id,
            mountpoint,
            roots,
            read_write,
            cache_mode,
            cache_dir,
            no_mount,
            json,
            conflict,
        } => {
            let task = qxync_core::tasks::Task::from_mount(
                Some(id.clone()),
                mountpoint.clone(),
                roots.clone(),
                *read_write,
                cache_mode.clone(),
                None,
                None,
                None,
                Some(true),
            )
            .with_cache_dir(cache_dir.clone())
            // ★ M8.4：冲突策略
            .with_conflict(conflict.clone());
            let saved: serde_json::Value = ipc_client::call(
                socket,
                Request::Tasks {
                    action: "save".into(),
                    id: None,
                    task: Some(task.clone()),
                },
            )
            .await?;
            let mut mounted = serde_json::Value::Null;
            if !*no_mount {
                mounted = ipc_client::call(
                    socket,
                    Request::Tasks {
                        action: "resume".into(),
                        id: Some(task.id.clone()),
                        task: None,
                    },
                )
                .await?;
            }
            if *json {
                println!(
                    "{}",
                    dumps(&serde_json::json!({ "saved": saved, "mount": mounted }))?
                );
            } else {
                println!("✅ 任务已登记：{}", task.summary());
                if *no_mount {
                    println!(
                        "   --no-mount：只登记未挂载；要挂载用 `qxync task mount {}`",
                        task.id
                    );
                } else {
                    println!("   ✅ 已挂载");
                }
            }
        }
        TaskAction::Rm { id, json } => {
            let raw: serde_json::Value = ipc_client::call(
                socket,
                Request::Tasks {
                    action: "delete".into(),
                    id: Some(id.clone()),
                    task: None,
                },
            )
            .await?;
            if *json {
                println!("{}", dumps(&raw)?);
            } else if raw
                .get("deleted")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                println!("✅ 已删除任务登记：{id}（挂载点里的数据未改动）");
            } else {
                println!("ℹ️  没有这个任务：{id}");
            }
        }
        TaskAction::Pause { id, json } => {
            let raw: serde_json::Value = ipc_client::call(
                socket,
                Request::Tasks {
                    action: "pause".into(),
                    id: Some(id.clone()),
                    task: None,
                },
            )
            .await?;
            if *json {
                println!("{}", dumps(&raw)?);
            } else {
                let un = raw
                    .get("unmounted")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                println!(
                    "⏸  已暂停任务：{id}（停用登记{}；已入队的上传会先排空）",
                    if un {
                        "并卸载"
                    } else {
                        "，未在挂载中"
                    }
                );
            }
        }
        TaskAction::Resume { id, json } => {
            let raw: serde_json::Value = ipc_client::call(
                socket,
                Request::Tasks {
                    action: "resume".into(),
                    id: Some(id.clone()),
                    task: None,
                },
            )
            .await?;
            if *json {
                println!("{}", dumps(&raw)?);
            } else {
                println!("▶  已继续任务：{id}");
            }
        }
        TaskAction::Mount { id, json } => {
            let raw: serde_json::Value = ipc_client::call(
                socket,
                Request::Tasks {
                    action: "mount".into(),
                    id: Some(id.clone()),
                    task: None,
                },
            )
            .await?;
            if *json {
                println!("{}", dumps(&raw)?);
            } else {
                println!("✅ 已按任务参数挂载：{id}");
            }
        }
    }
    Ok(())
}

fn print_tasks(d: &TasksData) {
    println!(
        "任务 {} 个（启用 {}）{}",
        d.count(),
        d.enabled_count(),
        if d.empty {
            "· 还没有登记过任务"
        } else {
            ""
        }
    );
    if d.tasks.is_empty() {
        println!(
            "  （空）建一个：qxync task add --id default --mountpoint ~/qxync-mnt --root /home"
        );
    }
    for ti in &d.tasks {
        let t = &ti.task;
        println!(
            "  {} {} [{}] {}  →  {}",
            if t.enabled { "▶" } else { "⏸" },
            t.id,
            if ti.mounted { "已挂载" } else { "未挂载" },
            t.mountpoint.display(),
            if t.roots.is_empty() {
                "(link home_root)".to_string()
            } else {
                t.roots.join(",")
            }
        );
        println!(
            "       模式={} 方向={} 节省空间={} 智能删除={} 排除规则={} 选择性={}",
            t.cache_mode,
            t.direction,
            t.space_saving,
            t.smart_delete,
            t.exclude.len(),
            t.selective.len()
        );
    }
    for (p, e) in &d.bad_files {
        println!("  ⚠️  任务文件解析失败（已跳过）：{p} —— {e}");
    }
    if let Some(n) = &d.note {
        println!("说明：{n}");
    }
}

fn print_journal(d: &qxync_core::ipc::JournalData) {
    if d.cleared {
        println!("🧹 已清空同步日志（删除 {} 条）", d.removed);
    }
    let ok = d.counts.get("ok").copied().unwrap_or(0);
    let err = d.counts.get("error").copied().unwrap_or(0);
    let blk = d.counts.get("blocked").copied().unwrap_or(0);
    println!(
        "同步日志：共 {} 条（ok {} / error {} / blocked {}）· 本次返回 {} 条",
        d.total,
        ok,
        err,
        blk,
        d.entries.len()
    );
    if d.entries.is_empty() {
        println!("  （空）挂载一个同步任务后跑一轮 `qxync sync --once` 就会产生记录（没有挂载点就没有对账）");
    }
    for e in &d.entries {
        let ts = e.ts;
        let mark = match e.status.as_str() {
            "error" => "❌",
            "blocked" => "⛔",
            _ => "✅",
        };
        let p = if e.path.is_empty() {
            String::new()
        } else {
            format!(" {}", e.path)
        };
        let b = if e.bytes > 0 {
            format!(" [{} B]", e.bytes)
        } else {
            String::new()
        };
        println!("  {mark} {ts}  {:<14}{}{}  {}", e.kind, p, b, e.detail);
    }
    if let Some(n) = &d.note {
        println!("说明：{n}");
    }
}
