//! `qxync` —— 前端 CLI（M0：协议打通）。
//!
//! 后端守护 `qxyncd` 尚未实现，本版 CLI 直接驱动 `qxync-client` 对 NAS 跑通
//! 「登录 → 列举 → stat → 下载 → 上传 → 建目录」这条链，作为 M0 的验收工具。

mod ipc_client;
mod progress;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use progress::DownloadProgress;
use qxync_client::Client;
use qxync_core::ipc::{
    socket_path_for, CacheInfo, CursorInfo, DecisionsData, DehydrateData, ErrorKind,
    FileStatesData, GetData, LsData, MountInfo, PeerData, PingData, PutData, Request, RootsData,
    RouteInfo, RoutesData, RulesData, SettingsData, ShutdownData, SpaceData, StatusData, StoreData,
    SyncInfo, TasksData,
};
use qxync_core::search::{KindFilter, MatchMode, SearchQuery, SortKey};
use qxync_core::settings::{Settings, PROXY_AUTO, PROXY_MANUAL, PROXY_NONE};
use qxync_core::{ConfigPaths, Credentials, DirEntry, LinkConfig, Task, HOME_ROOT};
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

    /// ★ M17：两步验证（2SV）的 TOTP 密钥（Base32，或 otpauth:// URI）。
    /// 会随凭据保存（0600），之后 daemon 重登自动现算验证码。
    #[arg(long, global = true, env = "QXNYC_OTP_SECRET", hide_env_values = true)]
    otp_secret: Option<String>,

    /// ★ M17：两步验证的一次性 6 位验证码（只用于本次登录，不保存）。
    #[arg(long, global = true, env = "QXNYC_OTP_CODE", hide_env_values = true)]
    otp_code: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// 登录并把凭据写入 ~/.config/qxync/credentials.json (0600)
    ///
    /// 开了两步验证（2SV）的账号：加 `--otp-secret <Base32密钥>`（会随凭据保存），
    /// 或 `--otp-code <6位验证码>`（只用于本次登录）。
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
    /// ★ M42：本地文件搜索（只读同步状态库 —— 不挂载、不联网、不用 daemon）
    ///
    /// 搜的是同步**元数据**（baseline + 本地新建），不是文件内容：
    /// 按路径/文件名子串（默认）或通配符匹配，可按子树、大小、类型过滤，
    /// 并能把命中换算成挂载点里的本地路径（`--local`）。
    ///
    /// 新鲜度：baseline 每轮对账/每次上传成功都会落盘，远端侧最多滞后一轮（默认 30s）；
    /// 要绝对新鲜先跑一轮 `qxync sync --once`。
    #[command(visible_alias = "search")]
    Find {
        /// 子串（默认，大小写不敏感）；省略 = 全部
        pattern: Option<String>,
        /// 通配符模式：`*` 任意序列、`?` 单字符；模式里没有 `/` 时只匹配文件名
        #[arg(long)]
        glob: bool,
        /// 大小写敏感（默认不敏感）
        #[arg(long)]
        case_sensitive: bool,
        /// 只搜这棵子树（远端路径，或挂载点下的本地路径）
        #[arg(long, value_name = "PATH")]
        under: Option<String>,
        /// 只搜这个任务（任务 id，同时决定 --local 的映射）
        #[arg(long, value_name = "ID")]
        task: Option<String>,
        /// 只要文件（f）或目录（d）
        #[arg(long = "type", value_name = "f|d")]
        r#type: Option<String>,
        /// 最小文件大小（K/M/G/T，1024 进制）
        #[arg(long, value_name = "SIZE")]
        min_size: Option<String>,
        /// 最大文件大小（K/M/G/T，1024 进制）
        #[arg(long, value_name = "SIZE")]
        max_size: Option<String>,
        /// 排序：path（路径升序，默认）/ size（大的在前）/ mtime（新的在前）
        #[arg(long, value_name = "path|size|mtime", default_value = "path")]
        sort: String,
        /// 反向排序
        #[arg(long)]
        reverse: bool,
        /// 最多返回多少条（0 = 全部）
        #[arg(long, value_name = "N", default_value_t = 100)]
        limit: usize,
        /// 输出挂载点里的本地路径（映射不到时回退远端路径）
        #[arg(long)]
        local: bool,
        /// 长格式：类型 大小 时间 路径
        #[arg(long)]
        long: bool,
        /// 输出 JSON（脚本用）
        #[arg(long)]
        json: bool,
    },
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
        /// 这一个 NAS 文件夹（一对一）；默认 /home
        #[arg(long = "remote", default_value = HOME_ROOT)]
        remote: String,
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

    /// ★ M19：删除相关数据（任务 / 连接 / 本机全部）。
    ///
    /// **默认只预览**（等价于 `--dry-run`），真正删除必须显式加 `--yes`。
    ///
    /// ```text
    /// qxync purge --task home            # 这条任务 + 缓存 + baseline/日志/待裁决
    /// qxync --link nas2 purge --connection  # 这条连接 + 它的全部本地数据
    /// qxync purge --all                  # 本机所有 qxync 数据（恢复出厂）
    /// ```
    ///
    /// 任何作用域都**不动 NAS 数据、不动挂载点里的文件**；`--all` 会先停掉所有 daemon。
    Purge {
        /// 只删这条任务及其相关数据
        #[arg(long)]
        task: Option<String>,
        /// 删掉 `--link` 选中的这条连接及其全部本地数据
        #[arg(long)]
        connection: bool,
        /// 删掉本机所有 qxync 数据（所有连接 / 任务 / 缓存 / 状态 / 日志）
        #[arg(long)]
        all: bool,
        /// 只打印将要删除的内容（不加 --yes 时的默认行为）
        #[arg(long)]
        dry_run: bool,
        /// 真的删除（不加这个参数时只预览）
        #[arg(long)]
        yes: bool,
        /// 直接输出 JSON（脚本/验收用）
        #[arg(long)]
        json: bool,
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

    /// ★ M23：Baloo 索引状态检测（Dolphin 集成开关 `dolphin_tags` 的前置条件）
    BalooStatus {
        /// 直接输出 JSON（脚本/验收用）
        #[arg(long)]
        json: bool,
    },

    /// ★ M39：日志目录占用 / 轮转策略；`--prune` 立即按策略清理
    ///
    /// 不带 `--prune` 只读（目录、总占用、单文件上限、保留天数、文件表）。
    /// 策略本身用 `qxync settings --set log_max_file_mb=... log_keep_days=...` 改，
    /// 保存后 daemon 会热生效并立刻清一次。
    Log {
        /// 立即按当前策略清理（删旧文件，不动正在写的那个）
        #[arg(long)]
        prune: bool,
        /// 直接输出 JSON（脚本/验收用）
        #[arg(long)]
        json: bool,
    },

    /// ★ M8.4：冲突待裁决队列（策略 = 每个文件都问我）
    ///
    /// 一次只能做一件事：`--resolve` / `--forget` / `--diff` / `--clear` 互斥。
    /// 早期版本让它们同时出现时「按代码顺序静默取一个」—— 用户敲了
    /// `--diff X --forget Y` 却只看到 diff，会以为 forget 也执行了。现在由 clap
    /// 直接报参数矛盾（退出码 2），宁可报错也不静默。
    Conflicts {
        /// 裁决某一条：`--resolve <id> --as keep_local|keep_remote|keep_both`
        #[arg(long, value_name = "ID", conflicts_with_all = ["forget", "diff", "clear"])]
        resolve: Option<String>,
        /// 裁决方式：保留本地 / 保留 NAS 上的 / 两份都留（**必须**和 `--resolve` 一起给）
        #[arg(long = "as", value_name = "RESOLUTION", requires = "resolve")]
        resolution: Option<String>,
        /// 删掉单条（**只删队列行，不动文件**）——「这条我不想管」的出口
        #[arg(long, value_name = "ID", conflicts_with_all = ["diff", "clear"])]
        forget: Option<String>,
        /// 打印某一条的本地/NAS 文本对比（超 512 KiB 或二进制只给原因）
        #[arg(long, value_name = "ID", conflicts_with = "clear")]
        diff: Option<String>,
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

    /// ★ T3：校验本地水合缓存的内容完整性（**不碰 NAS**，断网也能跑）
    ///
    /// 逐区间比 `.qxsum` 里的 `xxhash64`，能定位到**具体文件和具体区间**。
    /// 发现损坏时：报告模式只报；`--repair` 把坏区间退回「按需水合」并清掉校验和。
    /// 退出码：有损坏 = 1，全清 = 0（可直接进 CI / 定时任务）。
    Verify {
        /// 缓存目录（默认 ~/.local/share/qxync/cache）
        #[arg(long)]
        path: Option<PathBuf>,
        /// 把坏区间退回「按需水合」（不下载，下次读时自然修复）
        #[arg(long)]
        repair: bool,
        /// 直接输出 JSON（脚本/验收用）
        #[arg(long)]
        json: bool,
    },

    /// ★ M16：列出所有 NAS 连接（含各自 daemon / 登录 / 凭据状态）
    Links {
        /// 直接输出 JSON（脚本/验收用）
        #[arg(long)]
        json: bool,
    },

    /// ★ M40：多路径状态 / 诊断（内网 / 公网 / DDNS 各条路径）
    ///
    /// 不带 `--probe` 只读 daemon 路由表的最近结果（零网络）；`--probe` 立即对
    /// 每条路径做一次 DNS + HTTP 探测。daemon 没在跑 / 版本不支持时自动回落到
    /// 直连探测（不需要登录）。
    Routes {
        /// 立即探测全部路径（会发网络请求，最多约 3 秒）
        #[arg(long)]
        probe: bool,
        /// 直接输出 JSON（脚本/验收用）
        #[arg(long)]
        json: bool,
    },

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
        /// 这一个 NAS 文件夹（一对一）；不填 = 用 link 的 home_root。
        /// 要同步多个 NAS 文件夹请分建多个任务。
        #[arg(long = "root")]
        root: Option<String>,
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
    /// 拉起 qxyncd（已在运行则直接报成功）；`--all` = 所有连接
    Start {
        #[arg(long)]
        all: bool,
    },
    /// 请求 qxyncd 干净退出（★ M15/T6：会**真正卸载**全部 FUSE 挂载点）；`--all` = 所有连接
    ///
    /// 停止前会先检查挂载点有没有被别的进程占用（在挂载点里打开了文件、或者 `cd`
    /// 进去了）。有的话**列出 pid / 程序 / 路径并拒绝停止**，关掉那些程序再重试；
    /// 挂载点干净才继续。确实要硬停就用 `--force`（挂载点里打开着的文件会立刻失效）。
    Stop {
        #[arg(long)]
        all: bool,
        /// 挂载点被占用时也停止（默认拒绝并报「谁在占用」）
        #[arg(long)]
        force: bool,
    },
    /// ping + 状态快照；`--all` = 所有连接
    Status {
        #[arg(long)]
        all: bool,
    },
    /// ★ M15/T6：重载同步引擎（SIGHUP）—— **挂载点一秒都不中断**；`--all` = 所有连接
    ///
    /// 等价于 `systemctl --user reload qxyncd`。这是「让同步重新跑起来」的
    /// 推荐做法：restart 会短暂断开挂载点（systemd 语义限制，见 docs/M15 T6）。
    Reload {
        #[arg(long)]
        all: bool,
    },
}

fn paths() -> Result<ConfigPaths> {
    Ok(ConfigPaths::discover()?)
}

/// 把用户给的路径变成绝对路径（不改写内容：`..` / 符号链接保持原样，交给 fusermount）。
///
/// 用途：`chdir` 之后相对路径就失效了，所以卸载命令要先把挂载点定死。
fn absolute_path(p: &std::path::Path) -> PathBuf {
    if p.is_absolute() {
        return p.to_path_buf();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(p))
        .unwrap_or_else(|_| p.to_path_buf())
}

/// 本进程的工作目录在某个挂载点里时，切到 `/`。
///
/// 为什么需要：`qxync daemon stop` / `qxync umount` 常常就是**在挂载点里**敲的。
/// 本进程会被占用扫描排除（它下一秒就退出），但 `fusermount3 -u` 不看扫描 ——
/// 只要 cwd 还在挂载点里就回 EBUSY，纯属误伤。
fn chdir_out_of(mounts: &[PathBuf]) {
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    if mounts.iter().any(|mp| cwd.starts_with(mp)) {
        let _ = std::env::set_current_dir("/");
    }
}

/// 这个路径此刻是不是挂载点（读 `/proc/self/mounts`，处理空格的八进制转义）。
///
/// 用途：`daemon stop` 之后复核「挂载点真的没了吗」—— 进程退出不等于卸载成功：
/// `--force` 或者检查之后到卸载之前又有人打开文件，`fusermount` 会失败、daemon
/// 按约定继续退出，挂载点就留下一个连不上的死挂载。这时必须如实告诉用户。
fn is_mountpoint(path: &std::path::Path) -> bool {
    let Ok(txt) = std::fs::read_to_string("/proc/self/mounts") else {
        return false;
    };
    let target = path.to_string_lossy().to_string();
    txt.lines().any(|line| {
        let mut it = line.split_whitespace();
        let _dev = it.next();
        match it.next() {
            Some(mp) => unescape_mount_field(mp) == target,
            None => false,
        }
    })
}

/// `/proc/self/mounts` 的转义：`\040` 空格、`\011` tab、`\012` 换行、`\134` 反斜杠。
fn unescape_mount_field(field: &str) -> String {
    field
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
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
            // ★ M6：未配 roots 时由 `LinkConfig::roots()` 退回家目录根
            ipv4_only: cli.ipv4,
            // ★ M7：登录时先不定选择性同步规则（改 link JSON 后重启 daemon 生效）
            exclude: Vec::new(),
            filter_temp: true,
            peer_listen: None,
            peer_name: None,
            endpoints: Vec::new(),
            local_namespace: None,
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
    if let Ok(c) = Credentials::load_for(&p, link) {
        return Ok(c.password);
    }
    bail!("没有口令：用 --password / QXNYC_PASSWORD，或先跑 `qxync login --password` 写凭据")
}

/// ★ M17：本次登录要用的两步验证输入。
///
/// 优先级：`--otp-code`（一次性）> `--otp-secret`（本次）> 凭据里保存的 `otp_secret`。
/// 返回 `None` 表示没提供 —— 账号没开 2SV 时完全不影响登录。
fn resolve_otp(cli: &Cli, link: &LinkConfig) -> Option<String> {
    if let Some(code) = &cli.otp_code {
        return Some(code.clone());
    }
    if let Some(secret) = &cli.otp_secret {
        return Some(secret.clone());
    }
    Credentials::load_for(&paths().ok()?, link)
        .ok()
        .and_then(|c| c.otp_secret)
}

/// ★ M17：命令行给的 `--otp-secret` 归一化成可保存的 Base32 密钥。
///
/// 传 6 位验证码是**用错参数**（那是一次性的，不该落盘）→ 明确报错，
/// 不静默当成密钥存下来。
fn cli_otp_secret(cli: &Cli) -> Result<Option<String>> {
    let Some(raw) = &cli.otp_secret else {
        return Ok(None);
    };
    match qxync_core::totp::normalize_otp(raw)? {
        qxync_core::Otp::Secret(secret) => Ok(Some(secret)),
        qxync_core::Otp::Code(_) => bail!(
            "--otp-secret 要的是 TOTP 密钥（Base32），不是 6 位验证码；\
             一次性验证码请用 --otp-code（不会保存）"
        ),
    }
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
        let otp = resolve_otp(cli, &link);
        client
            .login_with_otp(&link.user, &pw, otp.as_deref())
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
    let socket = cli
        .socket
        .clone()
        .unwrap_or_else(|| socket_path_for(&cli.link));

    // ---- daemon 生命周期命令（不参与自动路由）----
    if let Cmd::Daemon { action } = &cli.cmd {
        return daemon_cmd(&cli, &socket, action).await;
    }
    // ★ M16：连接列表是纯本地 + 逐个 ping，不走 socket 路由。
    if let Cmd::Links { json } = &cli.cmd {
        return links_cmd(*json).await;
    }
    // ★ M19：「删除相关数据」自己决定走 daemon 还是本地（连接级 / 全局要先停 daemon），
    // 因此放在自动路由之前，和 daemon / links 命令同一类。
    if let Cmd::Purge { .. } = &cli.cmd {
        return run_purge(&cli).await;
    }

    // ---- 路由：显式指定 > 自动探测（socket 可连就走 daemon）----
    // ★ T3：`verify` 是**纯本地**的（只读缓存文件，不问 NAS、不用 daemon），
    // 所以在路由之前就短路 —— 否则 `--via-daemon` 会把它塞进 IPC，而 IPC 里
    // 并没有这个方法。
    if matches!(cli.cmd, Cmd::Verify { .. }) {
        return run_verify(&cli).await;
    }
    // ★ M42：`find` 也是纯本地（只读状态库，不问 NAS、不用 daemon）——
    //   路由前短路，daemon 没起、没挂载、断网时照常可用。
    if matches!(cli.cmd, Cmd::Find { .. }) {
        if cli.via_daemon {
            bail!("`find` 是纯本地命令（只读同步状态库），不支持 --via-daemon");
        }
        return run_find(&cli);
    }
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
            // 先校验命令行给的密钥格式：别等登录成功了才因为 --otp-secret 里是
            // 6 位验证码而报错（那样凭据没写、用户还白登了一次）。
            let cli_secret = cli_otp_secret(&cli)?;
            let otp = resolve_otp(&cli, &link);
            let session = match client.login_with_otp(&link.user, &pw, otp.as_deref()).await {
                Ok(s) => s,
                Err(e) => {
                    // ★ M40：登录失败时把每条路径的现场直接摆出来 —— 用户最想知道
                    //   的正是「到底是内网那条不通、还是 DDNS 没更新」。
                    eprintln!("❌ 登录 {}@{} 失败: {e}", link.user, link.host);
                    if client.router().endpoint_count() > 1 {
                        eprintln!("正在检测全部访问路径（DNS + HTTP）…");
                        let _ = client.probe_all_routes(qxync_client::PROBE_TIMEOUT).await;
                        print_routes(&client.router().snapshot(), false);
                    }
                    return Err(anyhow::Error::new(e))
                        .with_context(|| format!("登录 {}@{} 失败", link.user, link.host));
                }
            };
            paths()?.ensure_dirs()?;
            let link_path = link.save(&paths()?)?;
            // 空输入 = 不改：保留已保存的 2SV 密钥（只换口令时别把它抹掉）
            let existing_secret = Credentials::load_for(&paths()?, &link)
                .ok()
                .and_then(|c| c.otp_secret);
            let otp_secret = cli_secret.or(existing_secret);
            let cred = Credentials::new(link.host.clone(), link.user.clone(), pw)
                .with_otp_secret(otp_secret);
            let has_otp = cred.has_otp();
            let cred_path = cred.save_for(&paths()?, &link)?;
            println!("✅ 登录成功");
            println!("   sid        : {}", session.sid);
            println!("   路径       : {}", client.base_url());
            println!("   user       : {}", session.username);
            println!("   NAS UID    : {}", session.uid.as_deref().unwrap_or("-"));
            println!(
                "   Qsync 版本 : {} / QPKG {} / build {}",
                session.nas.qsync_version.as_deref().unwrap_or("-"),
                session.nas.qpkg_version.as_deref().unwrap_or("-"),
                session.nas.build.as_deref().unwrap_or("-")
            );
            println!(
                "   两步验证   : {}",
                if session.two_step {
                    "已通过（TOTP）"
                } else {
                    "未启用 / 本次未使用"
                }
            );
            if has_otp {
                println!("   TOTP 密钥  : 已保存（daemon 重登时自动现算验证码）");
            }
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
            // 逆向得到的就绪字段 + 限流信息（status=8 之外的"忙"信号都在这里）
            let lim = |v: Option<u64>| v.map(|x| x.to_string()).unwrap_or_else(|| "-".into());
            println!(
                "限流      : server_limit={}  cgi_number={}",
                lim(max_log.server_limit),
                lim(max_log.cgi_number)
            );
            if let Some(secs) = max_log.advised_interval_secs() {
                println!("降速      : sync_signal=2 → 建议下次轮询间隔 {secs}s");
            }
            if let Some(reason) = max_log.busy_reason() {
                println!("⚠️  {reason}");
            }
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
            // ★ M26：下载超过 3 秒才亮进度行（快下载输出零变化）；
            //   stat 拿到的远端大小做总量兜底（服务端 chunked 时 Content-Length 缺失）。
            let prog = DownloadProgress::new(name).with_total_hint(expected);
            let res = client
                .download_to_file_with_progress(dir, name, &dest, Some(&|w, t| prog.update(w, t)))
                .await;
            prog.finish();
            let written = res?;
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
                eprintln!(
                    "⚠️ 直连模式：设置已写入文件；正在运行的 daemon 要重启才会读到代理/释放空间/日志策略的新值\
                     （走 daemon 的 `qxync settings` 会热生效并立即清理一次旧日志）"
                );
                // ★ M23：直连写文件绕过了 daemon 的 Baloo 门禁 —— 这里必须把
                //   前置条件说清楚，否则用户开了 dolphin_tags 却没排除索引，
                //   daemon 重启后 tags 不会生效（挂载时按安全条件算过）。
                if st.dolphin_tags || st.dolphin_overlay {
                    eprintln!(
                        "⚠️ dolphin_tags / dolphin_overlay=true：开启前每个挂载点必须已\
                         排除出 Baloo 索引，否则内容索引器会把整个 NAS 拉到本地（且不会生效）。\
                         排除：balooctl6 config add excludeFolders <挂载点>"
                    );
                }
            }
            print_settings(&st, &paths, *json, saved)?;
        }
        Cmd::Verify { .. } => unreachable!("verify 在路由前已短路"),
        Cmd::Find { .. } => unreachable!("find 在路由前已短路（纯本地）"),
        Cmd::Routes { probe, json } => run_routes_direct(&cli, *probe, *json).await?,
        Cmd::Pin { .. }
        | Cmd::State { .. }
        | Cmd::Sync { .. }
        | Cmd::Dehydrate { .. }
        | Cmd::Space { .. }
        | Cmd::Log { .. }
        | Cmd::BalooStatus { .. }
        | Cmd::Conflicts { .. }
        | Cmd::FileStates { .. }
        | Cmd::Store { .. }
        | Cmd::Roots { .. }
        | Cmd::Rules { .. }
        | Cmd::Task { .. }
        | Cmd::Journal { .. }
        | Cmd::Peer { .. } => {
            bail!("`pin`/`state`/`sync`/`dehydrate`/`space`/`baloo-status`/`log`/`conflicts`/`file-states`/`store`/`roots`/`rules`/`task`/`peer` 需要 daemon：先 `qxync daemon start`（或用 --socket 指向运行中的 daemon）");
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
        Cmd::Links { .. } => unreachable!("links 命令在路由前已处理"),
        Cmd::Purge { .. } => unreachable!("purge 命令在路由前已处理"),
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
            // 直连模式（进程内 FUSE）：一对一，就挂这一个 NAS 文件夹
            let remote = if remote.trim().is_empty() {
                HOME_ROOT.to_string()
            } else {
                remote.clone()
            };
            let (client, link) = connect(&cli, true).await?;
            // ★ M16：与 daemon 同一套命名空间（default 仍然落在 cache/<host>）
            let p = paths()?;
            let ns = qxync_core::config::link_namespace(&link);
            let cache = cache_dir
                .clone()
                .unwrap_or_else(|| p.data_dir.join("cache"))
                .join(&ns);
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
                // ★ M16：队列按连接隔离，避免不同 NAS 的同名远端路径互相污染
                let marker_dir = p.queue_dir_for(&link);
                // 旧版全局队列的迁移只由 daemon 启动时做（它先抢 socket，能确认
                // 旧 daemon 已退出）；直连模式发现了就提示，不在这里冒险搬。
                if !marker_dir.join("queue.db").exists()
                    && p.legacy_queue_dir().join("queue.db").exists()
                {
                    eprintln!(
                        "⚠️ 检测到旧版全局上传队列 {}；先启动一次 qxyncd（`qxync daemon start`）                         让它迁移到 {}，否则这里的直连上传不会接着旧队列继续",
                        p.legacy_queue_dir().display(),
                        marker_dir.display()
                    );
                }
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
            let mp = absolute_path(mountpoint);
            // 用户常常就在挂载点里敲命令：本进程 cwd 先挪出去，否则 fusermount 回 EBUSY。
            chdir_out_of(std::slice::from_ref(&mp));
            // 与 daemon 侧同一套判据：先看谁在用，别让用户对着 Device or resource busy 猜。
            let busy = qxync_core::mountbusy::scan(&mp);
            if busy.is_in_use() {
                bail!(
                    "{} 卸载失败：还有进程在使用它（先关掉这些程序 / 退出这些目录再重试）：\n{}",
                    mp.display(),
                    qxync_core::mountbusy::report(std::slice::from_ref(&busy))
                );
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
                .context("执行 fusermount3 失败")?;
            if out.status.success() {
                println!("✅ 已卸载 {}", mp.display());
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
            otp: cli.otp_code.clone().or_else(|| cli.otp_secret.clone()),
        },
        Cmd::Status => Request::Status,
        Cmd::Routes { probe, .. } => Request::Routes {
            probe: Some(*probe),
        },
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
            remote: Some(remote.clone()),
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
        Cmd::BalooStatus { .. } => Request::BalooStatus,
        Cmd::Conflicts {
            resolve,
            resolution,
            forget,
            diff,
            clear,
            ..
        } => {
            // 四种模式**互斥**由 clap 保证（见 `Cmd::Conflicts` 的 conflicts_with），
            // 所以这里的 if 链不存在「同时给两个、静默取一个」的情况；顺序只是兜底。
            Request::Decisions {
                action: if *clear {
                    "clear".to_string()
                } else if diff.is_some() {
                    "diff".to_string()
                } else if forget.is_some() {
                    "forget".to_string()
                } else if resolve.is_some() {
                    "resolve".to_string()
                } else {
                    "list".to_string()
                },
                id: diff
                    .clone()
                    .or_else(|| forget.clone())
                    .or_else(|| resolve.clone()),
                resolution: resolution.clone(),
            }
        }
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
                root,
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
                            root.clone(),
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
        // ★ M39：日志目录/策略（`prune` 决定是否真的删）
        Cmd::Log { prune, .. } => Request::Logs {
            prune: Some(*prune),
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
        // verify / state / find / daemon 都不走 IPC
        Cmd::Verify { .. }
        | Cmd::Find { .. }
        | Cmd::State { .. }
        | Cmd::Daemon { .. }
        | Cmd::Links { .. }
        | Cmd::Purge { .. } => return None,
    })
}

fn split_remote(path: &str) -> (String, String) {
    match path.rfind('/') {
        Some(i) if i > 0 => (path[..i].to_string(), path[i + 1..].to_string()),
        _ => ("/".to_string(), path.trim_start_matches('/').to_string()),
    }
}

/// ★ T3：`qxync verify` —— 纯本地校验缓存内容完整性。
///
/// 之所以不走 daemon：它只读本地缓存文件（`<cache>` + `.qxstate` + `.qxsum`），
/// 不问 NAS、不要凭据，断网/未挂载时也能跑 —— 「数据出问题的时候往往正是网络
/// 不方便的时候」。
async fn run_verify(cli: &Cli) -> Result<()> {
    let Cmd::Verify { path, repair, json } = &cli.cmd else {
        bail!("内部错误：run_verify 只能处理 verify");
    };
    let cache = path.clone().unwrap_or_else(|| {
        paths()
            .map(|p| p.data_dir.join("cache"))
            .unwrap_or_else(|_| PathBuf::from("/tmp/qxync-cache"))
    });
    if !cache.is_dir() {
        bail!("缓存目录不存在: {}", cache.display());
    }
    let rep = qxync_fuse::verify_cache_dir(&cache, *repair);
    if *json {
        println!(
            "{}",
            serde_json::to_string_pretty(&rep).map_err(|e| anyhow::anyhow!("序列化失败: {e}"))?
        );
    } else {
        print_verify(&cache, &rep, *repair);
    }
    // 有损坏 = 退出码 1，方便 `qxync verify && echo ok` / 定时任务报警。
    if !rep.is_clean() {
        std::process::exit(1);
    }
    Ok(())
}

/// ★ M42：`qxync find` —— 只读同步状态库的本地文件搜索。
///
/// 为什么纯本地、不走 daemon：状态库（`sync.db`）里已经有 `baseline`（远端路径 +
/// 签名）与 `nodes.local_only`（本地新建），只读打开即可回答「同步树里有哪些文件」。
/// 这样 daemon 不在、没挂载、断网时都能用；代价是远端侧最多滞后一轮轮询
/// （默认 30s，上传成功会立即落盘）。
fn run_find(cli: &Cli) -> Result<()> {
    let Cmd::Find {
        pattern,
        glob,
        case_sensitive,
        under,
        task,
        r#type,
        min_size,
        max_size,
        sort,
        reverse,
        limit,
        local,
        long,
        json,
    } = &cli.cmd
    else {
        bail!("内部错误：run_find 只能处理 find");
    };

    let p = paths()?;
    let link = LinkConfig::load(&p, &cli.link)?;
    let db = p.sync_dir_for(&link).join(qxync_core::DB_FILE);
    let store = qxync_core::Store::open_readonly(&db)?.ok_or_else(|| {
        anyhow::anyhow!(
            "状态库不存在：{}（这个连接还没同步过；先 `qxync sync --once` 跑一轮对账再搜）",
            db.display()
        )
    })?;

    // 任务：--task 指定时只搜它；否则这个连接的全部任务（用于 --under 换算与 --local 映射）。
    let ids = all_link_ids(&p, &cli.link);
    let tasks: Vec<Task> = match task {
        Some(tid) => {
            let t = Task::load(&p, tid).map_err(|e| anyhow::anyhow!("任务 {tid} 加载失败：{e}"))?;
            if !t.belongs_to(&cli.link, &ids) {
                bail!(
                    "任务 {tid} 不属于连接 {}（换 --link 或换 --task）",
                    cli.link
                );
            }
            vec![t]
        }
        None => qxync_core::tasks::Task::list_for_link(&p, &cli.link, &ids).0,
    };

    let kind = match r#type.as_deref() {
        None => KindFilter::Any,
        Some("f" | "file" | "files") => KindFilter::File,
        Some("d" | "dir" | "dirs" | "directory" | "directories") => KindFilter::Dir,
        Some(other) => bail!("--type 只接受 f（文件）或 d（目录）：{other}"),
    };
    let sort_key = match sort.as_str() {
        "path" => SortKey::Path,
        "size" => SortKey::Size,
        "mtime" | "time" => SortKey::Mtime,
        other => bail!("--sort 只接受 path / size / mtime：{other}"),
    };
    let query = SearchQuery {
        pattern: pattern.clone(),
        mode: if *glob {
            MatchMode::Glob
        } else {
            MatchMode::Substring
        },
        case_sensitive: *case_sensitive,
        under: match under {
            Some(raw) => Some(resolve_under(raw, &tasks)?),
            None => None,
        },
        kind,
        min_size: min_size.as_deref().map(parse_size_arg).transpose()?,
        max_size: max_size.as_deref().map(parse_size_arg).transpose()?,
        sort: sort_key,
        reverse: *reverse,
        // CLI 的 `--limit 0` = 不截断（core 里用 None 表达）。
        limit: if *limit == 0 { None } else { Some(*limit) },
    };

    let page = store.search(&query)?;

    if *json {
        let hits: Vec<serde_json::Value> = page
            .hits
            .iter()
            .map(|h| {
                let mapped = local_hit(&h.path, &tasks);
                serde_json::json!({
                    "path": h.path,
                    "local_path": mapped.as_ref().map(|(p, _)| p.display().to_string()),
                    "task_id": mapped.as_ref().map(|(_, id)| id.clone()),
                    "size": h.size,
                    "mtime": h.mtime,
                    "mtime_text": format_unix_secs(h.mtime),
                    "is_dir": h.is_dir,
                    "origin": h.origin.as_str(),
                })
            })
            .collect();
        let out = serde_json::json!({
            "pattern": pattern.clone(),
            "under": query.under.clone(),
            "total": page.total,
            "truncated": page.truncated(),
            "hits": hits,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    if store.baseline_len()? == 0 && !page.hits.is_empty() {
        eprintln!(
            "提示：状态库里还没有对账数据（只搜到了本地新建的文件）；\
             先 `qxync sync --once` 或等一轮同步再搜远端文件"
        );
    }

    let mut unmapped = 0usize;
    for h in &page.hits {
        let mapped = if *local {
            local_hit(&h.path, &tasks)
        } else {
            None
        };
        if *local && mapped.is_none() {
            unmapped += 1;
        }
        let shown = mapped
            .map(|(path, _)| path.display().to_string())
            .unwrap_or_else(|| h.path.clone());
        if *long {
            println!(
                "{} {:>10}  {}  {}",
                if h.is_dir { "d" } else { "f" },
                human_size(h.size),
                format_unix_secs(h.mtime),
                shown
            );
        } else {
            println!("{shown}");
        }
    }
    if *local && unmapped > 0 {
        eprintln!(
            "提示：{unmapped} 条命中没有可映射的挂载任务，已显示远端路径\
             （`qxync find --task <id>` 指定任务可强制映射）"
        );
    }
    if page.truncated() {
        eprintln!(
            "…共 {} 条，已显示 {} 条（--limit 0 显示全部）",
            page.total,
            page.hits.len()
        );
    }
    Ok(())
}

/// ★ M42：`--under` 解析 —— 接受远端路径，也接受挂载点下的本地路径（自动换算成远端）。
fn resolve_under(raw: &str, tasks: &[Task]) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("--under 不能为空");
    }
    if !raw.starts_with('/') {
        bail!("--under 需要绝对路径（远端路径，或挂载点下的本地路径）：{raw}");
    }
    let raw = raw.trim_end_matches('/');
    for t in tasks {
        let mount = t.mountpoint.to_string_lossy();
        let mount = mount.trim_end_matches('/');
        if let Some(rel) = strip_local_prefix(raw, mount) {
            let root = t.effective_root();
            return Ok(join_remote(&root, rel));
        }
    }
    // 不在任何挂载点下：按远端路径原样使用（用户可能抄的是 `qxync ls` 的输出）。
    Ok(raw.to_string())
}

/// `path`（本地绝对路径）在 `prefix`（挂载点）下时返回相对部分；按路径段判边界。
fn strip_local_prefix<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    if prefix.is_empty() {
        return Some(path.trim_start_matches('/'));
    }
    if path == prefix {
        return Some("");
    }
    path.strip_prefix(prefix)?.strip_prefix('/')
}

/// 远端根 + 相对路径 → 远端路径（根 `/` 特判）。
fn join_remote(root: &str, rel: &str) -> String {
    let root = root.trim_end_matches('/');
    if rel.is_empty() {
        return if root.is_empty() {
            "/".to_string()
        } else {
            root.to_string()
        };
    }
    if root.is_empty() {
        format!("/{rel}")
    } else {
        format!("{root}/{rel}")
    }
}

/// ★ M42：命中路径 → 某个任务挂载点里的本地路径（第一个能对上的任务）。
fn local_hit(remote: &str, tasks: &[Task]) -> Option<(PathBuf, String)> {
    for t in tasks {
        let root = t.effective_root();
        if let Some(rel) = strip_remote_root(remote, &root) {
            let local = if rel.is_empty() {
                t.mountpoint.clone()
            } else {
                t.mountpoint.join(rel)
            };
            return Some((local, t.id.clone()));
        }
    }
    None
}

/// 远端路径在远端根 `root` 下时返回相对部分（按路径段判边界；根 `/` 特判）。
fn strip_remote_root<'a>(path: &'a str, root: &str) -> Option<&'a str> {
    let root = root.trim_end_matches('/');
    if root.is_empty() {
        return Some(path.trim_start_matches('/'));
    }
    if path == root {
        return Some("");
    }
    path.strip_prefix(root)?.strip_prefix('/')
}

/// ★ M42：`--min-size` / `--max-size` 的大小解析（K/M/G/T，1024 进制）。
fn parse_size_arg(raw: &str) -> Result<u64> {
    let s = raw.trim();
    if s.is_empty() {
        bail!("大小不能为空（示例：1024 / 10K / 2M / 1G）");
    }
    let (num, mul) = match s.chars().next_back().map(|c| c.to_ascii_lowercase()) {
        Some('k') => (&s[..s.len() - 1], 1024u64),
        Some('m') => (&s[..s.len() - 1], 1024 * 1024),
        Some('g') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        Some('t') => (&s[..s.len() - 1], 1024u64.pow(4)),
        _ => (s, 1),
    };
    let n: u64 = num.trim().parse().map_err(|_| {
        anyhow::anyhow!("大小格式不对：{raw}（示例：1024 / 10K / 2M / 1G；1024 进制）")
    })?;
    n.checked_mul(mul)
        .ok_or_else(|| anyhow::anyhow!("大小太大：{raw}"))
}

/// ★ M42：某个连接的全部 id（含「文件在但解析失败」的兜底），与 GUI 的同名辅助一致。
fn all_link_ids(p: &ConfigPaths, link_id: &str) -> Vec<String> {
    let (links, _bad) = LinkConfig::list(p);
    let mut ids: Vec<String> = links.into_iter().map(|l| l.id).collect();
    if !ids.iter().any(|id| id == link_id) {
        ids.push(link_id.to_string());
    }
    ids.sort();
    ids
}

/// 人读格式的 verify 报告。
fn print_verify(cache: &std::path::Path, rep: &qxync_fuse::VerifyReport, repair: bool) {
    if rep.files.is_empty() {
        println!("✅ 缓存里没有已水合的文件可校验: {}", cache.display());
        return;
    }
    let bad: Vec<&qxync_fuse::VerifyFileReport> = rep.files.iter().filter(|f| f.is_bad()).collect();
    for f in &bad {
        let head = format!("❌ {}", f.cache);
        if let Some(why) = &f.stale_state {
            println!("{head}  位图不可信（{why}）");
        }
        if f.missing_sums && f.stale_state.is_none() {
            println!("{head}  缺校验和表，无法校验内容");
        }
        if !f.bad_chunks.is_empty() {
            // 区间号 + 字节偏移都给出来，便于手工核对/定点重取
            let shown: Vec<String> = f
                .bad_chunks
                .iter()
                .take(10)
                .map(|i| format!("#{i}"))
                .collect();
            let more = if f.bad_chunks.len() > 10 {
                format!(" …共 {} 个", f.bad_chunks.len())
            } else {
                String::new()
            };
            println!(
                "{head}  {}/{} 区间内容损坏: {}{more}",
                f.bad_chunks.len(),
                f.chunks_total,
                shown.join(" ")
            );
        }
    }
    let total: u64 = rep.files.iter().map(|f| f.size).sum();
    println!(
        "── 校验 {} 个文件 / {:.2} MiB（已就绪 {:.2} MiB），耗时 {} ms",
        rep.files.len(),
        total as f64 / 1048576.0,
        rep.bytes_verified as f64 / 1048576.0,
        rep.elapsed_ms
    );
    if rep.is_clean() {
        println!("✅ 全部通过：缓存内容与校验和一致");
    } else if repair {
        println!(
            "✅ 已修复：{} 个坏区间退回按需水合{}",
            rep.repaired_chunks,
            if rep.whole_hashes_written > 0 {
                format!("，{} 个整文件校验和已更新", rep.whole_hashes_written)
            } else {
                String::new()
            }
        );
        println!("   （坏区间下次被读到时会自动从 NAS 重新取）");
    } else {
        println!(
            "⚠️  {} 个文件有问题。加 --repair 可把坏区间退回按需水合（本次不下载）",
            bad.len()
        );
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
        // verify / find 走不到这里（路由前已短路）
        Cmd::Verify { .. } => unreachable!(),
        Cmd::Find { .. } => unreachable!(),
        Cmd::Login => {
            let d: qxync_core::ipc::LoginData = match ipc_client::call(&socket, req).await {
                Ok(d) => d,
                Err(e) => {
                    // ★ M40：daemon 侧的登录已经逐路径试过；这里给一条「怎么查」的提示，
                    //   不让用户只看到一句网络错误。
                    eprintln!(
                        "❌ 经 daemon 登录失败：{}\n   提示：`qxync routes --probe` 可逐条检查 DNS / 内网-公网 / 可达性",
                        e.message
                    );
                    return Err(e);
                }
            };
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
        Cmd::Routes { probe, json } => {
            match ipc_client::call::<RoutesData>(&socket, req).await {
                Ok(d) => print_routes(&d, *json),
                Err(e) => {
                    // 老 daemon 不认识 routes：改成直连探测，而不是让用户对着
                    // 一句「未知 method」发呆。
                    eprintln!("⚠️ daemon 不支持多路径查询（{}），改为直连探测", e.message);
                    run_routes_direct(cli, *probe, *json).await.map_err(|e| {
                        qxync_core::ipc::IpcError::new(ErrorKind::Io, e.to_string())
                    })?;
                }
            }
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
            // ★ M26：经 daemon 也要进度 —— 信封声明 progress:true，daemon 在下载
            //   期间往同一条 socket 推进度行，这里原地刷新（老 daemon 自动退化）。
            //   stderr 非终端时 enabled()=false：不声明、daemon 不推行，输出零变化。
            let prog = DownloadProgress::new(name);
            let res: Result<GetData, _> =
                ipc_client::call_with_progress(&socket, req, prog.enabled(), |p| {
                    prog.update(p.written, p.total)
                })
                .await;
            prog.finish();
            let d = res?;
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
            println!(
                "✅ 已挂载 {} -> {}（{}，daemon 持有，pid 见 `qxync daemon status`）",
                m.mountpoint.display(),
                m.remote,
                if *rw { "读写" } else { "只读" }
            );
            // ★ M23：挂载点会被 Baloo 索引 → 如实警告（挂载本身已成功）。
            // 内容索引器读文件 = 水合，这与 Dolphin 集成开关无关，值得单独提醒。
            if let Some(w) = &m.baloo_warning {
                println!("⚠️  {w}");
            }
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
        Cmd::BalooStatus { json } => {
            let d: qxync_core::ipc::BalooStatusData = ipc_client::call(&socket, req).await?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&d).map_err(|e| {
                        qxync_core::ipc::IpcError::new(ErrorKind::Parse, e.to_string())
                    })?
                );
            } else {
                print_baloo_status(&d);
            }
        }
        Cmd::Log { prune, json } => {
            let d: qxync_core::ipc::LogsData = ipc_client::call(&socket, req).await?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&d).map_err(|e| {
                        qxync_core::ipc::IpcError::new(ErrorKind::Parse, e.to_string())
                    })?
                );
            } else {
                print_logs(&d, *prune);
            }
        }
        Cmd::Conflicts { json, diff, .. } => {
            // diff 返回的是另一种结构，别按 DecisionsData 解
            if diff.is_some() {
                let d: qxync_core::ipc::DiffData = ipc_client::call(&socket, req).await?;
                if *json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&d).map_err(|e| {
                            qxync_core::ipc::IpcError::new(ErrorKind::Parse, e.to_string())
                        })?
                    );
                } else {
                    print_diff(&d);
                }
            } else {
                let d: qxync_core::ipc::DecisionsData = ipc_client::call(&socket, req).await?;
                if *json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&d).map_err(|e| {
                            qxync_core::ipc::IpcError::new(ErrorKind::Parse, e.to_string())
                        })?
                    );
                } else {
                    print_decisions(&d);
                }
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
            // daemon 侧会先扫「谁在用」并拒绝占用中的卸载；这里先做两件本进程的事：
            // 把路径定成绝对（chdir 之后相对路径就废了）、把 cwd 挪出挂载点。
            let mp = absolute_path(mountpoint);
            chdir_out_of(std::slice::from_ref(&mp));
            ipc_client::call_ok(
                &socket,
                Request::Umount {
                    mountpoint: mp.clone(),
                },
            )
            .await?;
            println!("✅ 已卸载 {}", mp.display());
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
        Cmd::State { .. } | Cmd::Daemon { .. } | Cmd::Links { .. } | Cmd::Purge { .. } => {
            unreachable!()
        }
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
        "Dolphin   : Tags 列={} 角标={}（user.xdg.tags / user.qxync.overlay；开启前挂载点须排除出 Baloo 索引，见 `qxync baloo-status`）",
        if st.dolphin_tags { "开" } else { "关" },
        if st.dolphin_overlay { "开" } else { "关" }
    );
    println!(
        "日志策略  : 单文件 {} MB  保留 {} 天  总量上限 {} MB（0 = 该字段不限制；占用见 `qxync log`）",
        st.log_max_file_mb, st.log_keep_days, st.log_max_total_mb
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
        "Dolphin   : Tags 列={} 角标={}（user.xdg.tags / user.qxync.overlay；开启前挂载点须排除出 Baloo 索引，见 `qxync baloo-status`）",
        if st.dolphin_tags { "开" } else { "关" },
        if st.dolphin_overlay { "开" } else { "关" }
    );
    println!(
        "日志策略  : 单文件 {} MB  保留 {} 天  总量上限 {} MB（0 = 该字段不限制；占用见 `qxync log`）",
        st.log_max_file_mb, st.log_keep_days, st.log_max_total_mb
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

/// ★ M23：Baloo 索引状态（Dolphin 集成开关 `dolphin_tags` 的前置检测）。
fn print_baloo_status(d: &qxync_core::ipc::BalooStatusData) {
    println!(
        "Baloo     : {}",
        match (&d.balooctl, d.indexing_enabled) {
            (None, _) => "未安装（没有索引风险；Dolphin 的 Tags 列也不可用）".to_string(),
            (Some(b), true) => format!("已安装（{}），索引开启", b),
            (Some(b), false) => format!("已安装（{}），索引全局关闭（Tags 列也不会显示）", b),
        }
    );
    println!(
        "配置文件  : {}（{}）",
        d.config_path,
        if d.config_exists {
            "存在"
        } else {
            "不存在"
        }
    );
    println!(
        "Dolphin   : Tags 列={}（dolphin_tags） 角标={}（dolphin_overlay）",
        if d.dolphin_tags { "开" } else { "关" },
        if d.dolphin_overlay { "开" } else { "关" }
    );
    // ★ M25：角标插件检测是**本地文件检查**，不走 daemon —— 老版本 daemon 不报
    //   `overlay_plugin` 字段，而插件加载发生在跑 CLI 的这个桌面会话里，
    //   本地查最准（规则与 Qt 的插件查找对齐，见 qxync_core::dolphin）。
    match qxync_core::dolphin::detect_overlay_plugin() {
        Some(p) if p.effective => {
            println!("角标插件  : 已安装（{}）", p.path.display());
        }
        Some(p) => {
            println!(
                "角标插件  : 装在 {}，但 QT_PLUGIN_PATH 未包含该目录 —— Dolphin 不会加载\
                 （建议改用系统级安装：pacman -U qxync-dolphin-overlay-*.pkg.tar.zst）",
                p.path.display()
            );
        }
        None => {
            println!(
                "角标插件  : 未安装（角标开关开了也不会画角标；Arch 装 \
                 qxync-dolphin-overlay 包，其它发行版见 docs/M23-Dolphin集成.md §8）"
            );
        }
    }
    if d.mounts.is_empty() {
        println!("挂载点    : （无 —— 开关可以先打开，挂载时会再检查）");
    }
    for m in &d.mounts {
        println!(
            "  {}  {}{}",
            m.mountpoint,
            if m.indexed {
                "⚠️ 会被 Baloo 索引（危险）"
            } else if m.excluded {
                "✅ 已排除"
            } else {
                "✅ 不会被索引"
            },
            match (m.tags_active, m.overlay_active) {
                (true, true) => " · Tags+角标已生效",
                (true, false) => " · Tags 已生效",
                (false, true) => " · 角标已生效",
                (false, false) => "",
            }
        );
        if m.indexed {
            println!("            排除命令: {}", m.exclude_cmd);
        }
    }
    println!(
        "能否打开  : {}",
        if d.can_enable {
            "✅ 可以（所有挂载点都不会被索引）"
        } else {
            "❌ 不行 —— 先按上面的命令排除，再重试"
        }
    );
}

/// unix 秒 → 本地 `YYYY-MM-DD HH:MM:SS`（`libc::localtime_r`，与日志文件名同源）。
fn format_unix_secs(secs: i64) -> String {
    let t = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return format!("{secs}");
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// ★ M39：`qxync log` 的人读报告。
fn print_logs(d: &qxync_core::ipc::LogsData, pruned: bool) {
    println!("日志目录  : {}", d.dir);
    println!(
        "当前占用  : {}（{} 个文件）",
        human_size(d.total_bytes),
        d.file_count
    );
    println!(
        "轮转策略  : 单文件 {} MB  保留 {} 天  总量上限 {} MB{}",
        d.max_file_mb,
        d.keep_days,
        d.max_total_mb,
        "（0 = 该项不限制；改策略：qxync settings --set log_keep_days=7）"
    );
    if pruned {
        if d.deleted == 0 && d.failed.is_empty() {
            println!("清理结果  : 没有可清理的文件");
        } else {
            println!(
                "清理结果  : 删除 {} 个文件，释放 {}",
                d.deleted,
                human_size(d.freed_bytes)
            );
        }
    }
    if d.files.is_empty() {
        println!("文件      : （空）");
    } else {
        println!("文件      : （按大小降序，最多 {} 条）", d.files.len());
        for f in &d.files {
            let when = if f.mtime > 0 {
                format_unix_secs(f.mtime)
            } else {
                "-".into()
            };
            println!("  {:>10}  {}  {}", human_size(f.size), when, f.name);
        }
    }
    if !d.in_use.is_empty() {
        // 「跳过」不是失败：这些文件还有进程开着（daemon/GUI 正在写），不能删。
        println!("跳过（正被写入）:");
        for p in &d.in_use {
            println!("  {p}");
        }
    }
    if !d.failed.is_empty() {
        println!("⚠️ 没删掉的:");
        for e in &d.failed {
            println!("  {e}");
        }
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
    if let Some(x) = &d.removed_one {
        println!("已忽略    : {}（未动文件）", x.path);
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

/// 「本地独有 / NAS 独有」统计行。
///
/// ★ **别按字段名猜方向** —— 真实语义（见 `qxync_core::decisions::text_diff`）是：
///
/// * `-` 行 / `removed` = **只有本地有**（这一行取的是本地那一侧的内容，NAS 没有）；
/// * `+` 行 / `added`   = **只有 NAS 有**（这一行取的是 NAS 那一侧的内容，本地没有）。
///
/// 写反的后果不是「显示难看」：用户看着「本地多 N 行」去点「保留 NAS 上的」，
/// 方向整个反过来，内容被覆盖且撤销不了。
fn diff_counts_line(added: usize, removed: usize) -> String {
    format!("本地独有 {removed} 行 / NAS 独有 {added} 行")
}

/// diff 单行渲染：`本地行号  NAS行号  标记 内容`。
///
/// 行号 0 表示「这一侧没有对应行」 → 打 `-`（打 0 会被读成「第 0 行」）。
fn diff_row_line(l: &qxync_core::decisions::DiffLine) -> String {
    let num = |n: u32| {
        if n == 0 {
            "-".to_string()
        } else {
            n.to_string()
        }
    };
    format!(
        "{:>6} {:>6} {}{}",
        num(l.local_no),
        num(l.remote_no),
        l.kind,
        l.text
    )
}

/// diff 图例。方向与 [`diff_counts_line`] 一致：`-` 本地独有、`+` NAS 独有。
const DIFF_LEGEND: &str = "红/－ = 只有本地有（保留本地会把这份内容推到 NAS）  \
                           绿/＋ = 只有 NAS 上有（保留 NAS 上的会覆盖本地）";

/// 「为什么没对比」的一句话（`Skipped` 必须打印原因 —— 静默返回空 diff 会让人
/// 以为「两边一样」）。
fn diff_skip_reason(s: &qxync_core::decisions::DiffSkip) -> String {
    use qxync_core::decisions::DiffSkip;
    match s {
        DiffSkip::TooLarge { size, limit } => format!(
            "文件 {} 超出 {} 的文本对比上限，不做逐行对比（可继续用「保留本地 / 保留 NAS 上的」裁决）",
            human_size(*size),
            human_size(*limit)
        ),
        DiffSkip::Binary => "这是二进制文件（含 NUL 字节），不做逐行对比".to_string(),
        DiffSkip::IsDir => "这是目录，不做逐行对比".to_string(),
        DiffSkip::NoContent { which, detail } => {
            // `which` 只可能是 local / remote（daemon 侧赋值）；`unknown` 是
            // `DiffData` 的默认值，理论上到不了这里，但到了也不能瞎猜成 NAS。
            let (side, sep) = match which.as_str() {
                "local" => ("本地", ""),
                "remote" => ("NAS", " "), // 中英混排留一个空格，别印成「NAS内容…」
                _ => ("两侧之一", ""),
            };
            // daemon 有一条 detail 已经自成一句（「本地内容读不到：…」），再套前缀
            // 会变成「本地内容读不到：本地内容读不到：…」。先剥一层再统一加。
            let prefix = format!("{side}{sep}内容读不到：");
            let detail = detail.strip_prefix(&prefix).unwrap_or(detail);
            format!("{prefix}{detail}")
        }
    }
}

/// 本地 vs NAS 的文本对比。
///
/// **`Skipped` 必须打印原因** —— 静默返回空 diff 会让人以为「两边一样」。
fn print_diff(d: &qxync_core::ipc::DiffData) {
    use qxync_core::decisions::DiffOutcome;
    println!("# {}", d.path);
    println!(
        "大小      : 本地 {} / NAS {}",
        human_size(d.local_size),
        human_size(d.remote_size)
    );
    match &d.outcome {
        DiffOutcome::Diff(r) => {
            println!("差异      : {}", diff_counts_line(r.added, r.removed));
            if r.added == 0 && r.removed == 0 {
                println!("（内容逐行相同）");
            }
            if r.truncated {
                println!("（行数过多，只显示前 {} 行）", r.lines.len());
            }
            println!();
            println!("  本地   NAS   内容");
            for l in &r.lines {
                println!("{}", diff_row_line(l));
            }
            println!();
            println!("{DIFF_LEGEND}");
        }
        DiffOutcome::Skipped(s) => {
            println!("未对比    : {}", diff_skip_reason(s));
        }
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
    // ★ M15/T8：同步维度汇总。**与上面那行正交** —— 「仅在线 + 已同步」是
    // 完全正常的状态（没缓存但跟 NAS 一致），所以必须分开报，不能合成一个数。
    println!(
        "同步汇总  : 已同步 {} · 未同步 {} · 未知 {}{}",
        d.in_sync,
        d.out_of_sync,
        d.unknown,
        if d.transferring > 0 {
            format!(" · 传输中 {}", d.transferring)
        } else {
            String::new()
        }
    );
    for e in &d.entries {
        // ★ T8：两个维度组合成一句话 —— 空间三态 + 同步维度。
        let space = if e.is_dir {
            "目录".to_string()
        } else {
            match e.state.as_str() {
                "always" => "始终可用".to_string(),
                "local" => "本地可用".to_string(),
                _ => "仅在线".to_string(),
            }
        };
        // `in_sync = None` = 没查到节点（路径还没被 `lookup` 过）→ 说「未知」，
        // 绝不谎报「已同步」。
        let sync = match e.in_sync {
            Some(v) => qxync_fuse::sync_state_label(v, e.progress, e.conflicts),
            None => "同步状态未知".to_string(),
        };
        println!(
            "{:<8} {:>12}  {:<12} {:<10} · {}  {}{}",
            space,
            human_size(e.size),
            e.pin,
            sync,
            e.name,
            if e.dirty {
                "（有未上传改动）"
            } else {
                ""
            },
            if e.pending_upload {
                "（队列中）"
            } else {
                ""
            },
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

/// NAS 目录一览（NAS 上登记的同步文件夹；家目录是协议固定命名空间 `/home`）。
fn print_roots(d: &RootsData) {
    if d.syncing_folders.is_empty() {
        println!("NAS 同步文件夹 : （无 —— 该账号没有在 Qsync 里配同步文件夹）");
    } else {
        println!("NAS 同步文件夹 : {} 个", d.syncing_folders.len());
        for f in &d.syncing_folders {
            println!(
                "  · {}  客户端路径={}  权限={}  可删除={}  realpath={}",
                f.folder,
                // ★ 选 NAS 文件夹时能直接用的路径（映射不出来就退回共享路径）
                f.client_path
                    .as_deref()
                    .or(f.path.as_deref())
                    .unwrap_or("-"),
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
        println!("            上次扫描 {}s 前", c.last_sweep_age_secs);
    }
    // ★ 这几个 `blocked_*` 是 daemon 每次脱水 `fetch_add` 的**累计值**（只增不清），
    //   不是「最近一次扫描被挡了多少」—— 混在一起印会让人以为现在正被挡这么多。
    let blocked =
        c.blocked_dirty + c.blocked_pinned + c.blocked_open + c.blocked_mapped + c.blocked_inflight;
    if blocked > 0 {
        println!(
            "            累计被挡：dirty={} pinned={} open={} mmap={} 在途={}（自 daemon 启动，只增不清）",
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

/// 「多久以前」的人话（`status` / `sync` 展示用：`95` → `1 分钟`）。
fn human_age_secs(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs} 秒"),
        60..=3599 => format!("{} 分钟", secs / 60),
        3600..=86_399 => format!("{} 小时 {} 分钟", secs / 3600, (secs % 3600) / 60),
        _ => format!("{} 天", secs / 86_400),
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
    if s.full_sync_pending {
        println!(
            "  全量同步: 进行中（首次同步 / 太久没同步；完成后恢复每 {}s 轮询）",
            s.interval_secs
        );
    }
    if s.last_sync_ok_age_secs > 0 {
        println!(
            "  上次成功: {}前（完整跑完一轮对账）",
            human_age_secs(s.last_sync_ok_age_secs)
        );
    }
    println!(
        "  游标    : notify={} config={} global_notify={} max_log_seen={}（日志空 -17 × {}）",
        s.cursors.notify,
        s.cursors.config,
        s.cursors.global_notify,
        s.cursors.max_log_seen,
        s.cursors.log_missing_count
    );
    // ★ `SyncStats` 里的计数器是 daemon 生命周期内的**累计值**（fetch_add，只增不清）：
    //   13 小时 × 每 30s 一轮 × 每轮 526 项被挡 = 69 万 —— 它绝不是「这一轮被挡了多少」。
    //   不写「累计」会让人以为现在真有几十万条在等，白查一圈。
    //   下面那行 `delete_block_reason` 才是「最近一次**完整**轮询有没有被挡」（daemon
    //   在干净轮次会清空它），所以那里的「本轮有 N 项…」措辞是可信的。
    println!(
        "  baseline: {} 项｜自 daemon 启动累计：事件 {}｜远端刷新 {}｜入队上传 {}｜冲突 {}｜删除 {}｜删除被挡 {}（累计）",
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
    if let Some(n) = &s.note {
        // ★ M30：网络失败不再进「最近错误」（红字），而是「说明」里的一行
        //   「已缓冲，等联网」——`qxync status` / `qxync sync` 也要能看到它。
        println!("  说明    : {n}");
    }
    if let Some(e) = &s.last_error {
        println!("  ⚠️  最近错误: {e}");
    }
}

/// ★ M40：daemon 不在 / 不认识 `routes` 时的直连探测。
///
/// **不需要登录、不写任何配置**：构造 Client 后直接探测（探测请求不带 sid）。
/// 这也是「连不上时先查多路径」在没有 daemon 的机器上仍然可用的原因。
async fn run_routes_direct(cli: &Cli, _probe: bool, json: bool) -> Result<()> {
    let link = resolve_link(cli)?;
    let client = match Settings::load(&paths()?) {
        Ok(st) => Client::new_with_settings(&link, &st)?,
        Err(_) => Client::new(&link)?,
    };
    if client.router().endpoint_count() > 1 {
        eprintln!(
            "（直连模式：不需要登录，正在探测 {} 条路径）",
            client.router().endpoint_count()
        );
    }
    // 直连没有 daemon 的后台巡检，不探一次就全是「未知」—— 直接探。
    let _ = client.probe_all_routes(qxync_client::PROBE_TIMEOUT).await;
    let mut data: RoutesData = client.router().snapshot();
    data.probed = true;
    print_routes(&data, json);
    Ok(())
}

/// ★ M40：人读的多路径表。
fn print_routes(d: &RoutesData, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(d).unwrap_or_else(|_| "{}".into())
        );
        return;
    }
    println!(
        "多路径    : {} 条（按顺序尝试；✓ = 当前{}）",
        d.endpoints.len(),
        if d.probed {
            "，本次已探测"
        } else {
            "；只读最近结果，加 --probe 立即检测"
        }
    );
    for (i, e) in d.endpoints.iter().enumerate() {
        let mark = if e.active { "✓" } else { " " };
        let scheme = if e.https { "https" } else { "http" };
        let addr = if e.host.parse::<std::net::Ipv6Addr>().is_ok() {
            format!("[{}]:{}", e.host, e.port)
        } else {
            format!("{}:{}", e.host, e.port)
        };
        let dns = route_dns_label(e);
        let latency = e
            .latency_ms
            .map(|ms| format!("{ms} ms"))
            .unwrap_or_else(|| "-".into());
        println!(
            "  {:>2} {}  {:<14} {}://{}  {}  {}  {}",
            i + 1,
            mark,
            truncate_label(&e.label, 14),
            scheme,
            addr,
            dns,
            latency,
            route_state_label(&e.state)
        );
        if e.resolved_changed {
            println!("        └ 域名解析结果与上次不同（DDNS 可能刚更新）");
        }
        if e.cooldown_secs > 0 {
            println!("        └ 冷却中：{} 秒后可再试", e.cooldown_secs);
        }
        if let Some(note) = &e.note {
            println!("        └ {note}");
        }
        if !matches!(e.state.as_str(), "ok" | "unknown") {
            if let Some(err) = &e.last_error {
                println!("        └ 最近错误：{err}");
            }
        }
    }
    if let Some(sw) = &d.last_switch {
        println!(
            "最近切换  : {} 从 {} 到 {}（原因 {}）",
            fmt_ago(sw.at_unix),
            sw.from,
            sw.to,
            sw.reason
        );
    }
    if d.endpoints.len() > 1 {
        println!(
            "提示      : 顺序 = 优先级（额外路径在前、主地址兜底）；改路径用 GUI「设置 → 连接」\n\
             \x20           或直接编辑 ~/.config/qxync/links/<连接 id>.json 的 endpoints"
        );
    }
}

fn route_state_label(state: &str) -> &'static str {
    match state {
        "ok" => "正常",
        "failed" => "失败",
        "cooling" => "冷却中",
        "blocked" => "已禁用（身份不符）",
        _ => "未探测",
    }
}

fn route_dns_label(e: &RouteInfo) -> String {
    if e.resolved.is_empty() {
        return "DNS 无结果".into();
    }
    e.resolved
        .iter()
        .zip(e.resolved_scopes.iter())
        .map(|(ip, scope)| format!("{ip}〔{scope}〕"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn truncate_label(s: &str, max_chars: usize) -> String {
    let n = s.chars().count();
    if n <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// unix 秒 → 「刚刚 / N 秒前 / N 分钟前 / N 小时前 / N 天前」。
fn fmt_ago(unix: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(unix);
    let d = now.saturating_sub(unix).max(0);
    match d {
        0..=5 => "刚刚".into(),
        6..=59 => format!("{d} 秒前"),
        60..=3599 => format!("{} 分钟前", d / 60),
        3600..=86399 => format!("{} 小时前", d / 3600),
        _ => format!("{} 天前", d / 86400),
    }
}

fn print_status(st: &StatusData) {
    println!(
        "daemon    : v{} pid={} uptime={}s",
        st.daemon.version, st.daemon.pid, st.daemon.uptime_secs
    );
    println!("socket    : {}", st.daemon.socket);
    match &st.link {
        Some(link) => {
            println!(
                "连接      : {} ({}://{}:{})",
                link.id,
                if link.https { "https" } else { "http" },
                link.host,
                link.port
            );
            println!(
                "用户      : {}{}",
                link.user,
                if link.ipv4_only { "  [仅 IPv4]" } else { "" }
            );
        }
        None => println!(
            "连接      : 未配置（daemon 空转待命中 —— 先 `qxync login`，\
             或在 GUI「设置 → 连接」里保存；配好后会自动开始同步）"
        ),
    }
    println!(
        "登录状态  : {}",
        if st.logged_in {
            "已登录"
        } else {
            "未登录（先 `qxync login`）"
        }
    );
    if let Some(r) = &st.routes {
        // ★ M40：多路径摘要（只有一条路径时也显示当前地址，便于确认走的是哪条）
        let active = r.active.as_deref().unwrap_or("-");
        if r.total > 1 {
            let sw = r
                .last_switch
                .as_ref()
                .map(|s| {
                    format!(
                        "，上次切换 {}（{}，原因 {}）",
                        fmt_ago(s.at_unix),
                        s.to,
                        s.reason
                    )
                })
                .unwrap_or_default();
            println!(
                "路径      : {} 条（当前 {}；{}/{} 可用{sw}）",
                r.total, active, r.healthy, r.total
            );
        } else {
            println!("路径      : {active}");
        }
    }
    if let Some(s) = &st.session {
        // ★ M22：离线时 daemon 不发 check_alive（M20 的卡顿修复），alive=false
        //   只是「不知道」—— 按 alive_known 区分，别把断网误导成会话失效。
        //   （alive=true 只可能来自真探测，老 daemon 没有新字段时也按存活展示。）
        let alive_text = if !s.alive && !s.alive_known {
            "未知（离线未探测）"
        } else if s.alive {
            "存活"
        } else {
            "失活"
        };
        println!("会话      : sid={} 存活={}", s.sid_masked, alive_text);
    }
    // ★ M20：连通性 + 离线缓冲区。离线时 `status` 不再发网络请求，这一段就是
    // 用户排查「怎么不同步了」时最该看到的答案。
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if let Some(c) = &st.connectivity {
        if !c.known {
            println!("连通性    : 未检测（daemon 还没发过网络请求）");
        } else if c.online {
            println!("连通性    : 在线");
        } else {
            let dur = (now_unix - c.since_unix).max(0);
            println!(
                "连通性    : 离线（已 {dur} 秒）{}",
                c.last_error
                    .as_deref()
                    .map(|e| format!("｜最近错误: {e}"))
                    .unwrap_or_default()
            );
        }
    }
    if let Some(o) = &st.offline {
        println!(
            "离线缓冲  : 待上传 {}｜待删除 {}｜待建目录 {}｜待改名 {}｜待改 mtime {}{}",
            o.pending_upload,
            o.pending_delete,
            o.pending_mkdir,
            o.pending_rename,
            o.pending_set_mtime,
            if o.replaying {
                "｜结构操作待回放/回放中（上传/删除暂时冻结）"
            } else {
                ""
            }
        );
        if let Some(n) = &o.last_note {
            println!("            : {n}");
        }
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
    // ★ T8：传输汇总。`None` = 此刻没有在途作业（不是「查不到」——
    // 老版本 daemon 也会给 `None`，那种情况下 GUI 另有版本号可判断）。
    if let Some(t) = &st.transfers {
        println!(
            "传输中    : 共 {} 个文件（下载 {} · 上传 {}）｜{}/{} 字节",
            t.active, t.downloading, t.uploading, t.done_bytes, t.total_bytes
        );
    }
    if let Some(c) = &st.cache {
        print_cache(c);
    }
    if let Some(u) = &st.uploads {
        // ★ `done` / `failed` / `retries` / `bytes` 是队列生命周期内的累计值（只增不清）；
        //   `pending` / `active` 才是「此刻」。
        println!(
            "上传队列  : 待上传 {}｜上传中 {}｜完成 {}（累计）｜失败 {}（累计）｜重试 {}（累计）｜{} 字节（累计）",
            u.pending,
            if u.active { "yes" } else { "no" },
            u.done,
            u.failed,
            u.retries,
            u.bytes
        );
    }
    // ★ M11：删除队列。`pending>0` = 有删除还没推给 NAS（正常只会持续几百毫秒）；
    // `failed>0` = 有删除重试超限被放弃了 —— 这时 NAS 上会残留文件，需要人工处理。
    if let Some(d) = &st.deletes {
        // ★ 同上传队列：只有 `pending` / `active` 是此刻，其余都是累计值。
        println!(
            "删除队列  : 待删 {}｜推送中 {}｜已删 {}（累计）｜失败 {}（累计）｜重试 {}（累计）｜批次 {}（累计）",
            d.pending,
            if d.active { "yes" } else { "no" },
            d.deleted,
            d.failed,
            d.retries,
            d.batches
        );
    }
    if let Some(s) = &st.sync {
        print_sync(s);
    }
    println!(
        "水合统计  : {} 次 / {} 字节（累计，自 daemon 启动）",
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

/// 取 daemon 日志的尾部（`<state>/log/qxyncd.log*` 里最新的那一个）。
///
/// 为什么需要它：`qxyncd` 默认 daemon 化，fork 之后 fd 0/1/2 都指向 `/dev/null`，
/// 所以它的启动失败**不会**出现在调用方的终端上 —— 唯一的线索就在日志文件里。
/// 读不到就如实说读不到，不要假装没有线索。
fn daemon_log_tail(link_id: &str) -> String {
    let dir = qxync_core::ipc::default_log_dir();
    let prefix = qxync_core::ipc::log_prefix_for(link_id);
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            if !e.file_name().to_string_lossy().starts_with(&prefix) {
                continue;
            }
            if let Ok(t) = e.metadata().and_then(|m| m.modified()) {
                if newest.as_ref().is_none_or(|(bt, _)| t > *bt) {
                    newest = Some((t, e.path()));
                }
            }
        }
    }
    let Some((_, path)) = newest else {
        return format!(
            "（读不到 daemon 日志目录 {}，前缀 {}）",
            dir.display(),
            prefix
        );
    };
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let tail: Vec<&str> = text.lines().rev().take(8).collect();
    if tail.is_empty() {
        return format!("（daemon 日志 {} 还是空的）", path.display());
    }
    let mut out = format!("后台 daemon 日志尾部（{}）：", path.display());
    for line in tail.into_iter().rev() {
        out.push_str("\n    ");
        out.push_str(line);
    }
    out
}

async fn daemon_cmd(cli: &Cli, socket: &std::path::Path, action: &DaemonAction) -> Result<()> {
    let all = matches!(
        action,
        DaemonAction::Start { all: true }
            | DaemonAction::Stop { all: true, .. }
            | DaemonAction::Status { all: true }
            | DaemonAction::Reload { all: true }
    );
    if !all {
        return daemon_action_one(&cli.link, socket, action).await;
    }
    if cli.socket.is_some() {
        bail!("`daemon --all` 不能和显式 `--socket` 一起用（每个连接有自己的 socket）");
    }
    let p = paths()?;
    let (links, bad) = LinkConfig::list(&p);
    for (bp, be) in &bad {
        eprintln!("⚠️ 连接配置解析失败，已跳过 {}: {be}", bp.display());
    }
    if links.is_empty() {
        println!("没有连接配置（{}）", p.config_dir.join("links").display());
        return Ok(());
    }
    let mut failed = 0usize;
    for link in &links {
        let s = socket_path_for(&link.id);
        if let Err(e) = daemon_action_one(&link.id, &s, action).await {
            eprintln!("❌ [{}] {e:#}", link.id);
            failed += 1;
        }
    }
    if failed > 0 {
        bail!("{failed}/{} 个连接失败", links.len());
    }
    Ok(())
}

async fn daemon_action_one(
    link_id: &str,
    socket: &std::path::Path,
    action: &DaemonAction,
) -> Result<()> {
    match action {
        DaemonAction::Start { .. } => {
            if ipc_client::available(socket).await {
                println!("✅ qxyncd 已在运行（{}，link={link_id}）", socket.display());
                return Ok(());
            }
            // ★ **不检查 link 配置**：daemon 的职责是常驻后台，没配 NAS 时它空转待命
            //   （只服务 ping/status/shutdown），配置一出现就自己转入同步模式。
            //   所以「还没有连接配置」不是启动失败的理由。
            let exe = daemon_binary()?;
            let mut cmd = std::process::Command::new(&exe);
            cmd.arg("--link").arg(link_id).arg("--socket").arg(socket);
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
                // 兜底：把后台日志的尾部带回来。daemon 起不来时唯一的线索在日志文件里。
                bail!(
                    "qxyncd 已派生但 socket 未就绪：{}\n{}",
                    socket.display(),
                    daemon_log_tail(link_id)
                );
            }
            let p: PingData = ipc_client::call(socket, Request::Ping)
                .await
                .map_err(ipc_err)?;
            println!(
                "✅ qxyncd 已启动 link={link_id} pid={} version={} socket={}",
                p.pid,
                p.daemon_version,
                socket.display()
            );
            if LinkConfig::load(&ConfigPaths::discover()?, link_id).is_err() {
                println!(
                    "   空转待命：还没有配置 NAS 连接「{link_id}」—— 先 `qxync --link {link_id} login`，\
                     或在 GUI「设置 → 连接」里保存；配好后它会自动开始同步（不用重启）"
                );
            }
        }
        DaemonAction::Stop { force, .. } => {
            if !ipc_client::available(socket).await {
                println!("qxyncd（link={link_id}）未在运行");
                return Ok(());
            }
            let ping: PingData = ipc_client::call(socket, Request::Ping)
                .await
                .map_err(ipc_err)?;
            // 空转待命的 daemon（还没有连接配置）会对 `mounts` 回 NotLoggedIn ——
            // 它没有挂载点，当成空表即可，不然 `daemon stop` 反而停不了一个闲着的 daemon。
            let mounts: Vec<MountInfo> = match ipc_client::call(socket, Request::Mounts).await {
                Ok(m) => m,
                Err(e) if e.kind == ErrorKind::NotLoggedIn => Vec::new(),
                Err(e) => return Err(ipc_err(e)),
            };
            let mps: Vec<PathBuf> = mounts.iter().map(|m| m.mountpoint.clone()).collect();
            // 用户常常就是在挂载点里敲命令：本进程的 cwd 先挪出去。扫描里本进程本来
            // 就被排除，但 fusermount 不看扫描 —— cwd 还在挂载点里就是 EBUSY。
            chdir_out_of(&mps);
            let busy =
                qxync_core::mountbusy::scan_many(mps.iter().map(|p| p.as_path()), &[ping.pid]);
            if !busy.is_empty() {
                let text = qxync_core::mountbusy::report(&busy);
                if !*force {
                    bail!(
                        "{text}\n\n没有停止 qxyncd（pid {}）。先关掉上面列出的程序、退出这些目录后重试；\n\
                         确实要强制停止：`qxync daemon stop --force`（挂载点里打开着的文件会立刻失效）。",
                        ping.pid
                    );
                }
                eprintln!("⚠️  挂载点仍被占用，按 --force 继续停止：");
                eprintln!("{text}");
            }
            let sd: ShutdownData = ipc_client::call(socket, Request::Shutdown)
                .await
                .map_err(ipc_err)?;
            if sd.pending_uploads + sd.pending_deletes > 0 {
                eprintln!(
                    "⏳ qxyncd 已收到停止请求：还有 {} 个上传、{} 个删除要收尾（两个队列各自最长等 120 秒；作业已入库，等不到会在下次启动继续）…",
                    sd.pending_uploads, sd.pending_deletes
                );
            }
            // ★ 等**进程**真的退出，而不是等 socket 连不上：
            //   daemon 退出前要先排空队列、再卸载挂载点（队列卡住时最长 ~2 分钟），
            //   而 socket 直到最后才删。旧实现只看 socket，10 秒就打印「已停止」，
            //   用户以为好了、其实进程还在 —— 这就是「daemon 停不下来」的观感来源。
            let proc_dir = format!("/proc/{}", ping.pid);
            let t0 = std::time::Instant::now();
            // 上限 260 秒：上传队列 120s + 删除队列 120s（每个挂载点各算一次）之后
            // 还要留出 fusermount 的时间。比这更久就真不正常了。
            let mut next_hint = Duration::from_secs(5);
            loop {
                if !std::path::Path::new(&proc_dir).exists() {
                    break;
                }
                let waited = t0.elapsed();
                if waited > Duration::from_secs(260) {
                    bail!(
                        "qxyncd（pid {}）收到停止请求后 260 秒仍未退出：卡在排空上传/删除队列或卸载挂载点。\n\
                         看日志（~/.local/state/qxync/log/qxyncd.log*）或 `qxync daemon status --all`；\
                         必要时 `kill -9 {}`，但那样挂载点可能残留（要手工 `fusermount3 -uz <挂载点>`）。",
                        ping.pid,
                        ping.pid
                    );
                }
                if waited > next_hint {
                    if next_hint == Duration::from_secs(5) {
                        eprintln!(
                            "⏳ qxyncd（pid {}）还在退出：在排空上传/删除队列并卸载挂载点（两个队列各自最长 120 秒）…",
                            ping.pid
                        );
                    } else {
                        eprintln!("⏳ 已等 {} 秒，仍在退出…", waited.as_secs());
                    }
                    next_hint += Duration::from_secs(30);
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            // 进程退了 ≠ 挂载点一定干净：`--force`、或者检查之后到卸载之前又有人打开
            // 文件时，fusermount 会失败、daemon 按约定继续退出，留下「连不上」的死挂载。
            // 给 auto_unmount 一点时间，再复核一次，如实报出来。
            tokio::time::sleep(Duration::from_millis(300)).await;
            let leftover: Vec<&PathBuf> = mps.iter().filter(|mp| is_mountpoint(mp)).collect();
            if !leftover.is_empty() {
                eprintln!("⚠️  qxyncd 已停止，但这些挂载点还在（卸载失败，多半是还有进程占用）：");
                for mp in &leftover {
                    eprintln!("   {}", mp.display());
                }
                eprintln!(
                    "   清理：先关掉占用者，再 `fusermount3 -uz <挂载点>`；重新 `qxync daemon start` 也会按记录重挂。"
                );
            }
            println!("✅ qxyncd 已停止（link={link_id}）");
        }
        DaemonAction::Status { .. } => {
            if !ipc_client::available(socket).await {
                bail!(
                    "qxyncd（link={link_id}）未在运行（socket {}）",
                    socket.display()
                );
            }
            let p: PingData = ipc_client::call(socket, Request::Ping)
                .await
                .map_err(ipc_err)?;
            let st: StatusData = ipc_client::call(socket, Request::Status)
                .await
                .map_err(ipc_err)?;
            println!("link      : {} (socket {})", link_id, socket.display());
            println!(
                "pong      : {} v{} pid={} uptime={}s",
                p.pong, p.daemon_version, p.pid, p.uptime_secs
            );
            print_status(&st);
        }
        DaemonAction::Reload { .. } => {
            // ★ M15/T6：SIGHUP = 重载同步引擎但**不碰 FUSE 会话**。
            //   走信号而不是新增 IPC 方法，与 `systemctl reload` 完全同一条路径 ——
            //   用户在两种部署下得到同样的行为。
            let p: PingData = ipc_client::call(socket, Request::Ping)
                .await
                .map_err(|_| anyhow::anyhow!("qxyncd 未在运行（socket {}）", socket.display()))?;
            // pid 走 IPC 传来是 u32，内核接口要 i32；pid 是内核分配的小整数，
            // 实践中不会越界，但仍然显式挡一道，避免任何理论上的截断。
            let pid = libc::pid_t::try_from(p.pid)
                .map_err(|_| anyhow::anyhow!("qxyncd 返回了非法 pid={}", p.pid))?;
            let rc = unsafe { libc::kill(pid, libc::SIGHUP) };
            if rc != 0 {
                bail!(
                    "给 qxyncd(pid={}) 发 SIGHUP 失败: {}",
                    p.pid,
                    std::io::Error::last_os_error()
                );
            }
            // SIGHUP 是异步的：这里只能确认「信号发到了」，重载是否完成看日志。
            // 不 sleep 轮询 —— 同步引擎重载包含「等旧任务做完当前一轮」，
            // 短则几秒长则几十秒，CLI 不该在这里卡住。
            println!(
                "✅ 已给 qxyncd(pid={}, link={link_id}) 发 SIGHUP：同步引擎将重载（轮询 / 脱水 / 队列 / journal）",
                p.pid
            );
            println!("   FUSE 挂载点**不中断** —— 这正是用 reload 而不是 restart 的原因");
        }
    }
    Ok(())
}

/// ★ M16：`qxync links [--json]` —— 所有连接 + 各自 daemon/凭据状态。
// ---------------------------------------------------------------- ★ M19 删除相关数据

/// `--socket` 显式值优先，否则按连接 id 推导（与其它命令同一口径）。
fn purge_socket(cli: &Cli) -> PathBuf {
    cli.socket
        .clone()
        .unwrap_or_else(|| socket_path_for(&cli.link))
}

/// 某个连接的任务列表（包含「文件在但解析失败」的兜底 id）。
fn purge_tasks_for_link(p: &ConfigPaths, link_id: &str) -> Vec<qxync_core::Task> {
    let (links, _bad) = LinkConfig::list(p);
    let mut ids: Vec<String> = links.into_iter().map(|l| l.id).collect();
    if !ids.iter().any(|id| id == link_id) {
        ids.push(link_id.to_string());
    }
    ids.sort();
    qxync_core::tasks::Task::list_for_link(p, link_id, &ids).0
}

/// 停掉某个 socket 上的 daemon 并等它真的消失（删除连接/全局数据的前提）。
async fn stop_daemon_at(socket: &std::path::Path) {
    if !ipc_client::available(socket).await {
        return;
    }
    let _ = ipc_client::call_ok(socket, Request::Shutdown).await;
    for _ in 0..40 {
        if !ipc_client::available(socket).await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    eprintln!(
        "⚠️  daemon 没有在 10s 内退出：{}（相关数据可能在它退出后又被写回，建议稍后重跑）",
        socket.display()
    );
}

fn print_purge_plan(plan: &qxync_core::purge::PurgePlan) {
    println!("将删除（{}）：", plan.title);
    for it in &plan.items {
        println!(
            "  [{}] {:<10} {}  （{} / {} 个文件）",
            if it.optional { "数据" } else { "基础" },
            it.label,
            it.path,
            human_size(it.bytes),
            it.files
        );
    }
    if let Some(rows) = plan.db_rows {
        if rows.total() > 0 {
            println!(
                "  [状态库] baseline {} 行 / 日志 {} 行 / 待裁决 {} 行",
                rows.baseline, rows.journal, rows.decisions
            );
        }
    }
    for w in &plan.warnings {
        println!("  ⚠️  {w}");
    }
    for n in &plan.notes {
        println!("  · {n}");
    }
    println!(
        "合计：{} / {} 个文件（其中勾选/确认后删除 {} / {} 个文件）",
        human_size(plan.total_bytes),
        plan.total_files,
        human_size(plan.optional_bytes),
        plan.optional_files
    );
}

fn print_purge_outcome_value(v: &serde_json::Value) {
    let target = v
        .get("task_id")
        .or_else(|| v.get("target"))
        .or_else(|| v.get("id"))
        .and_then(|x| x.as_str())
        .unwrap_or("?");
    let paths = v
        .get("deleted_paths")
        .and_then(|x| x.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let bytes = v.get("bytes").and_then(|x| x.as_u64()).unwrap_or(0);
    let files = v.get("files").and_then(|x| x.as_u64()).unwrap_or(0);
    println!(
        "✅ 已删除「{target}」的相关数据：{} 条路径 / {} / {} 个文件",
        paths,
        human_size(bytes),
        files
    );
    if let Some(rows) = v.get("db_rows") {
        let n = |k: &str| rows.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        if n("baseline") + n("journal") + n("decisions") > 0 {
            println!(
                "   状态库：baseline {} 行 / 日志 {} 行 / 待裁决 {} 行",
                n("baseline"),
                n("journal"),
                n("decisions")
            );
        }
    }
    for w in v
        .get("warnings")
        .and_then(|x| x.as_array())
        .into_iter()
        .flatten()
    {
        println!("   ⚠️  {}", w.as_str().unwrap_or("?"));
    }
    println!("   NAS 数据与挂载点里的文件未受影响");
}

/// 打印计划；`json` 时直接输出计划对象（脚本可解析）。
fn purge_preview(plan: &qxync_core::purge::PurgePlan, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(plan)?);
    } else {
        print_purge_plan(plan);
        println!("（预览：没有改动任何东西；真正删除请加 --yes）");
    }
    Ok(())
}

/// 执行计划并打印结果（连接级 / 全局用；两者的路径都已归一化好）。
fn purge_execute(p: &ConfigPaths, plan: &qxync_core::purge::PurgePlan, json: bool) -> Result<()> {
    qxync_core::purge::validate_plan(plan)?;
    let out = qxync_core::purge::execute(plan);
    let v = serde_json::json!({
        "purged": true,
        "scope": out.scope,
        "target": out.target,
        "deleted_paths": out.deleted_paths,
        "bytes": out.bytes,
        "files": out.files,
        "db_rows": out.db_rows,
        "warnings": out.warnings,
        "notes": out.notes,
    });
    if json {
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        print_purge_outcome_value(&v);
    }
    if plan.scope == qxync_core::purge::SCOPE_ALL {
        // 没有连接了；提示下一步（不自动重建目录 —— 让用户/下次启动决定）。
        println!("   本机已回到「从未配置过」的状态");
    }
    let _ = p;
    Ok(())
}

/// ★ M19：`qxync purge` —— 删除相关数据（任务 / 连接 / 本机全部）。
///
/// 默认**只预览**；`--yes` 才真的删。作用域三选一。
async fn run_purge(cli: &Cli) -> Result<()> {
    let Cmd::Purge {
        task,
        connection,
        all,
        dry_run,
        yes,
        json,
    } = &cli.cmd
    else {
        unreachable!()
    };
    let p = paths()?;
    let execute_now = *yes && !*dry_run;
    // 作用域必须**恰好一个**：三个都给时静默取一个（比如 --all 压掉 --task）会很危险。
    let scopes = usize::from(*all) + usize::from(*connection) + usize::from(task.is_some());
    if scopes != 1 {
        bail!("purge 需要 --task <id> / --connection / --all 三种作用域**恰好一个**（现在是 {scopes} 个）");
    }

    if *all {
        let (links, bad) = LinkConfig::list(&p);
        let plan = qxync_core::purge::plan_all(&p, &links);
        if !execute_now {
            return purge_preview(&plan, *json);
        }
        // 先停所有 daemon：状态库 / 缓存 / 日志不能被正在跑的进程继续写。
        for l in &links {
            stop_daemon_at(&socket_path_for(&l.id)).await;
        }
        // 解析失败的 link 文件也可能还有 daemon 在跑：从文件名取 id 试着停一下。
        for (path, _err) in &bad {
            if let Some(id) = path.file_stem().and_then(|s| s.to_str()) {
                stop_daemon_at(&socket_path_for(id)).await;
            }
        }
        stop_daemon_at(&purge_socket(cli)).await;
        return purge_execute(&p, &plan, *json);
    }

    if *connection {
        let lid = cli.link.clone();
        let link = LinkConfig::load(&p, &lid)?;
        let tasks = purge_tasks_for_link(&p, &lid);
        let plan = qxync_core::purge::plan_link(&p, &link, &tasks);
        if !execute_now {
            return purge_preview(&plan, *json);
        }
        stop_daemon_at(&purge_socket(cli)).await;
        return purge_execute(&p, &plan, *json);
    }

    let tid = task
        .clone()
        .ok_or_else(|| anyhow::anyhow!("purge 需要 --task <id> / --connection / --all 三者之一"))?;
    let t = qxync_core::Task::load(&p, &tid)?;
    let (links, _bad) = LinkConfig::list(&p);
    let ids: Vec<String> = links.into_iter().map(|l| l.id).collect();
    let lid = t
        .effective_link_id(&ids)
        .unwrap_or_else(|| "default".into());
    let link = LinkConfig::load(&p, &lid)?;
    let others = purge_tasks_for_link(&p, &lid);
    let db_path = p.sync_dir_for(&link).join(qxync_core::store::DB_FILE);
    let db_ro = qxync_core::Store::open_readonly(&db_path).ok().flatten();
    let plan = qxync_core::purge::plan_task(&p, &link, &t, &others, db_ro.as_ref());
    if !execute_now {
        return purge_preview(&plan, *json);
    }

    // daemon 在跑时**必须**走 IPC：挂载表与 SyncState 内存副本都在它手里。
    let socket = purge_socket(cli);
    if !cli.direct && ipc_client::available(&socket).await {
        let v: serde_json::Value = ipc_client::call(
            &socket,
            Request::Tasks {
                action: "purge".into(),
                id: Some(tid),
                task: None,
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!(e.message))?;
        if *json {
            println!("{}", serde_json::to_string_pretty(&v)?);
        } else {
            print_purge_outcome_value(&v);
        }
        return Ok(());
    }

    // daemon 没在跑：本进程直接执行（与 GUI 的 `task_purge` 同一条路）。
    qxync_core::purge::validate_plan(&plan)?;
    let out = qxync_core::purge::execute(&plan);
    qxync_core::purge::forget_mount_record(&p, &link, &t.mountpoint);
    let root = t.effective_root();
    let shared_root = others
        .iter()
        .filter(|o| o.id != t.id)
        .any(|o| o.effective_root() == root);
    let mut rows = qxync_core::purge::PurgeDbRows::default();
    if !shared_root && db_ro.is_some() {
        let store = qxync_core::Store::open(&db_path)?;
        rows = qxync_core::purge::delete_task_rows(&store, &root)?;
    }
    let v = serde_json::json!({
        "purged": true,
        "task_id": tid,
        "link_id": lid,
        "unmounted": false,
        "deleted_paths": out.deleted_paths,
        "bytes": out.bytes,
        "files": out.files,
        "db_rows": {
            "baseline": rows.baseline,
            "journal": rows.journal,
            "decisions": rows.decisions,
        },
        "warnings": out.warnings,
        "notes": out.notes,
    });
    if *json {
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        print_purge_outcome_value(&v);
    }
    Ok(())
}

async fn links_cmd(json: bool) -> Result<()> {
    let p = paths()?;
    let (links, bad) = LinkConfig::list(&p);
    let mut rows = Vec::new();
    for link in &links {
        let socket = socket_path_for(&link.id);
        let running = ipc_client::available(&socket).await;
        let mut logged_in = false;
        let mut pid: Option<u32> = None;
        let mut error: Option<String> = None;
        if running {
            match ipc_client::call::<PingData>(&socket, Request::Ping).await {
                Ok(ping) => pid = Some(ping.pid),
                Err(e) => error = Some(e.message),
            }
            match ipc_client::call::<StatusData>(&socket, Request::Status).await {
                Ok(st) => logged_in = st.logged_in,
                Err(e) => {
                    if error.is_none() {
                        error = Some(e.message);
                    }
                }
            }
        }
        let cred = Credentials::present_for(&p, link);
        let otp = Credentials::present_otp_for(&p, link);
        rows.push(serde_json::json!({
            "id": link.id,
            "host": link.host,
            "port": link.port,
            "https": link.https,
            "insecure": link.insecure,
            "user": link.user,
            // ★ M40：路径数 / 顺序（主机只展示主地址，路径细节用 `qxync routes`）
            "path_count": link.endpoint_list().len(),
            "paths": link
                .endpoint_list()
                .iter()
                .map(|e| serde_json::json!({
                    "label": e.label,
                    "host": e.host,
                    "port": e.port,
                    "https": e.https,
                    "primary": e.primary,
                }))
                .collect::<Vec<_>>(),
            "socket": socket.display().to_string(),
            "credential_present": cred.is_some(),
            // ★ M17：只报「有没有存 2SV 密钥」，绝不回传密钥本身
            "otp_present": otp,
            "daemon": {
                "running": running,
                "pid": pid,
                "logged_in": logged_in,
                "error": error,
            },
        }));
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "links": rows,
                "bad": bad.iter().map(|(p, e)| serde_json::json!({
                    "path": p.display().to_string(), "error": e,
                })).collect::<Vec<_>>(),
            }))?
        );
        return Ok(());
    }
    if rows.is_empty() {
        println!(
            "还没有 NAS 连接。用 `qxync --host <NAS> --user <用户> --password <口令> login`，\
             或在 GUI「设置 → 连接」里添加。"
        );
    }
    for r in &rows {
        let id = r["id"].as_str().unwrap_or("?");
        let host = r["host"].as_str().unwrap_or("?");
        let port = r["port"].as_u64().unwrap_or(0);
        let user = r["user"].as_str().unwrap_or("?");
        let running = r["daemon"]["running"].as_bool().unwrap_or(false);
        let logged = r["daemon"]["logged_in"].as_bool().unwrap_or(false);
        let cred = r["credential_present"].as_bool().unwrap_or(false);
        let otp = r["otp_present"].as_bool().unwrap_or(false);
        let paths = r["path_count"].as_u64().unwrap_or(1);
        println!(
            "- {id:<10} {user}@{host}:{port}  路径={paths}  daemon={}  login={}  credential={}  2sv={}{}",
            if running { "运行" } else { "停止" },
            if logged { "已登录" } else { "未登录" },
            if cred { "有" } else { "无" },
            if otp { "已配置" } else { "无" },
            if running {
                format!("  socket={}", r["socket"].as_str().unwrap_or("?"))
            } else {
                String::new()
            }
        );
        if let Some(err) = r["daemon"]["error"].as_str() {
            println!("  ⚠️ {err}");
        }
    }
    for (bp, be) in &bad {
        eprintln!("⚠️ 连接配置解析失败，已跳过 {}: {be}", bp.display());
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
            root,
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
                root.clone(),
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
            t.root
                .clone()
                .unwrap_or_else(|| "(link home_root)".to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cli_from(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("qxync").chain(args.iter().copied()))
    }

    fn decisions_req(args: &[&str]) -> (String, Option<String>, Option<String>) {
        let cli = cli_from(args).unwrap_or_else(|e| panic!("{args:?} 应能解析: {e}"));
        match to_request(&cli).expect("conflicts 有 IPC 形态") {
            Request::Decisions {
                action,
                id,
                resolution,
            } => (action, id, resolution),
            other => panic!("{args:?} 解成了 {other:?}"),
        }
    }

    /// ★ 参数矛盾必须**报错**，不能按代码顺序静默取一个。
    ///
    /// 修复前：`--clear --diff X` 静默执行 clear；`--diff X --forget Y` 静默只 diff。
    /// 用户会以为两件事都做了（尤其 forget 是「忽略这条」，静默丢裁决意图很危险）。
    #[test]
    fn conflicts_modes_are_mutually_exclusive() {
        for args in [
            &["conflicts", "--clear", "--diff", "a"][..],
            &["conflicts", "--clear", "--forget", "a"],
            &[
                "conflicts",
                "--clear",
                "--resolve",
                "a",
                "--as",
                "keep_local",
            ],
            &["conflicts", "--diff", "a", "--forget", "b"],
            &[
                "conflicts",
                "--diff",
                "a",
                "--resolve",
                "b",
                "--as",
                "keep_local",
            ],
            &[
                "conflicts",
                "--forget",
                "a",
                "--resolve",
                "b",
                "--as",
                "keep_local",
            ],
        ] {
            let e = cli_from(args).expect_err(&format!("{args:?} 必须报参数冲突"));
            assert_eq!(
                e.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "{args:?} 的错误类型不对: {e}"
            );
        }
        // `--as` 单独给 = resolution 会被静默忽略 → 必须报错（requires=resolve）
        assert!(
            cli_from(&["conflicts", "--as", "keep_local"]).is_err(),
            "--as 没有 --resolve 时必须报错，不能静默忽略"
        );
        // 互斥只针对这四个模式：`--json` 可以和任意一个共存
        assert!(cli_from(&["conflicts", "--diff", "a", "--json"]).is_ok());
    }

    /// 每种模式各映射成什么请求（一次只有一种，不允许歧义）。
    #[test]
    fn conflicts_modes_map_to_exactly_one_request() {
        assert_eq!(decisions_req(&["conflicts"]), ("list".into(), None, None));
        assert_eq!(
            decisions_req(&["conflicts", "--clear"]),
            ("clear".into(), None, None)
        );
        assert_eq!(
            decisions_req(&["conflicts", "--diff", "a"]),
            ("diff".into(), Some("a".into()), None)
        );
        assert_eq!(
            decisions_req(&["conflicts", "--forget", "a"]),
            ("forget".into(), Some("a".into()), None)
        );
        assert_eq!(
            decisions_req(&["conflicts", "--resolve", "a", "--as", "keep_local"]),
            (
                "resolve".into(),
                Some("a".into()),
                Some("keep_local".into())
            )
        );
    }

    /// ★ 方向回归：`removed` = 本地独有行、`added` = NAS 独有行。
    ///
    /// 早期 `print_diff` 写成「本地多 {added} / NAS 多 {removed}」+ 图例反向，
    /// 用户会照着反的结论选「保留哪边」。这里用**真实数据源**（`text_diff`）造
    /// diff，再把 CLI 的三处文字（统计行 / 行号列 / 图例）全部钉住方向。
    #[test]
    fn diff_text_direction_matches_text_diff_semantics() {
        use qxync_core::decisions::text_diff;
        let r = text_diff("a\nlocal-only\nc\n", "a\nnas-only\nc\n");
        assert_eq!((r.added, r.removed), (1, 1));

        // `text_diff` 的真语义：'-' 行 = 本地独有（取本地文本，NAS 行号 0）
        let minus = r.lines.iter().find(|l| l.kind == '-').unwrap();
        assert_eq!(minus.text, "local-only");
        assert_eq!((minus.local_no, minus.remote_no), (2, 0));
        // '+' 行 = NAS 独有（取 NAS 文本，本地行号 0）
        let plus = r.lines.iter().find(|l| l.kind == '+').unwrap();
        assert_eq!(plus.text, "nas-only");
        assert_eq!((plus.local_no, plus.remote_no), (0, 2));

        // 统计行方向（不是 1:1，才能看出有没有写反）
        assert_eq!(diff_counts_line(3, 2), "本地独有 2 行 / NAS 独有 3 行");
        // 行号列：没有对应行的一侧打 '-'（打 0 会被读成「第 0 行」）
        assert_eq!(diff_row_line(plus), "     -      2 +nas-only");
        assert_eq!(diff_row_line(minus), "     2      - -local-only");
        // 图例方向必须和统计行一致
        assert!(DIFF_LEGEND.contains("－ = 只有本地有"), "{DIFF_LEGEND}");
        assert!(DIFF_LEGEND.contains("＋ = 只有 NAS 上有"), "{DIFF_LEGEND}");
    }

    /// `Skipped` 的原因必须**原样可读、不重复前缀**（daemon 有一条 detail 自带
    /// 「本地内容读不到：」前缀，再套一层会变成双重前缀）。
    #[test]
    fn diff_skip_reason_is_readable_and_not_double_prefixed() {
        use qxync_core::decisions::DiffSkip;
        // 本地：daemon 已自带前缀的那条 → 剥掉再统一加，不能重复
        let s = diff_skip_reason(&DiffSkip::NoContent {
            which: "local".into(),
            detail: "本地内容读不到：文件只水合了一部分".into(),
        });
        assert_eq!(s, "本地内容读不到：文件只水合了一部分");
        // 本地：自带无前缀的（例如「没有已挂载的任务覆盖这个路径」）→ 加上
        let s = diff_skip_reason(&DiffSkip::NoContent {
            which: "local".into(),
            detail: "没有已挂载的任务覆盖这个路径".into(),
        });
        assert_eq!(s, "本地内容读不到：没有已挂载的任务覆盖这个路径");
        // remote：一侧是 NAS，不能错标成本地
        let s = diff_skip_reason(&DiffSkip::NoContent {
            which: "remote".into(),
            detail: "从 NAS 下载内容失败：timeout".into(),
        });
        assert_eq!(s, "NAS 内容读不到：从 NAS 下载内容失败：timeout");
        // 未知 which（DiffData 默认值）不能瞎猜成 NAS
        let s = diff_skip_reason(&DiffSkip::NoContent {
            which: "unknown".into(),
            detail: "尚未对比".into(),
        });
        assert_eq!(s, "两侧之一内容读不到：尚未对比");
        // 其它跳过原因都要说明「为什么没比」
        assert!(diff_skip_reason(&DiffSkip::IsDir).contains("目录"));
        assert!(diff_skip_reason(&DiffSkip::Binary).contains("二进制"));
        assert!(diff_skip_reason(&DiffSkip::TooLarge { size: 5, limit: 4 }).contains("上限"));
    }
}
