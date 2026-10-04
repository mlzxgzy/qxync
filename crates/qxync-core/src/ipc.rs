//! # 本地 IPC 契约（daemon ↔ CLI/GUI）
//!
//! 传输：unix domain socket，**一行一个 JSON**（见 `docs/M1.5-设计.md` §1）。
//! 这里只放**类型与路径规则**，不做任何 IO，方便 CLI/daemon/测试三方共用。

use crate::config::ConfigPaths;
use crate::model::DirEntry;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// 协议版本：请求/响应都带，用于向前兼容。
pub const IPC_VERSION: u32 = 1;

// ---------------------------------------------------------------- 路径规则

/// 默认 socket 路径：`$XDG_RUNTIME_DIR/qxync/qxyncd.sock`，退到 state 目录。
pub fn default_socket_path() -> PathBuf {
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        if !rt.is_empty() {
            return PathBuf::from(rt).join("qxync/qxyncd.sock");
        }
    }
    match ConfigPaths::discover() {
        Ok(p) => p.state_dir.join("qxyncd.sock"),
        Err(_) => PathBuf::from("/tmp/qxync/qxyncd.sock"),
    }
}

/// pid 文件：`<state>/qxyncd.pid`。
pub fn default_pid_path() -> PathBuf {
    match ConfigPaths::discover() {
        Ok(p) => p.state_dir.join("qxyncd.pid"),
        Err(_) => PathBuf::from("/tmp/qxync/qxyncd.pid"),
    }
}

/// 日志目录：`<state>/log`。
pub fn default_log_dir() -> PathBuf {
    match ConfigPaths::discover() {
        Ok(p) => p.log_dir(),
        Err(_) => PathBuf::from("/tmp/qxync/log"),
    }
}

// ---------------------------------------------------------------- 请求

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum Request {
    /// 存活探测，不做任何 IO。
    Ping,
    /// 状态快照（**不触发登录**）。
    Status,
    /// 登录；`user`/`password` 省略时读 `credentials.json`。
    Login {
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        password: Option<String>,
    },
    Logout,
    Ls {
        path: String,
    },
    Stat {
        dir: String,
        name: String,
    },
    Get {
        dir: String,
        name: String,
        dest: PathBuf,
    },
    Put {
        local: PathBuf,
        dest: String,
        #[serde(default)]
        name: Option<String>,
    },
    Mkdir {
        parent: String,
        name: String,
    },
    /// pin：`state=Some(...)` 设置（`pinned`/`unpinned`/`unspecified`/`excluded`），`None` 查询。
    Pin {
        path: String,
        #[serde(default)]
        state: Option<String>,
    },
    Mount {
        mountpoint: PathBuf,
        /// ★ 这一个 NAS 文件夹（省略 = Qsync 家目录 `/home`）。
        /// 一对多是删除过的：一次挂载只对应**一个**远端路径。
        #[serde(default)]
        remote: Option<String>,
        #[serde(default)]
        cache_dir: Option<PathBuf>,
        #[serde(default)]
        threads: Option<usize>,
        #[serde(default)]
        auto_unmount: Option<bool>,
        #[serde(default)]
        hydrate_timeout_secs: Option<u64>,
        /// 读写挂载（默认只读）。开写路径必须为 true，否则写操作回 EROFS。
        #[serde(default)]
        read_write: Option<bool>,
        /// ★ M2c：本地大批删除熔断阈值（60 秒窗口内的删除次数；0 = 关闭）。
        #[serde(default)]
        delete_limit: Option<usize>,
        /// ★ M3：缓存模式 `pagecache`（默认）/ `direct`（绕过 page cache，mmap 不可用）。
        #[serde(default)]
        cache_mode: Option<String>,
        /// ★ M8.2：把这次挂载登记成一条**持久化任务**（id 缺省 `default`）。
        /// 省略 = 不登记 —— **M7 及以前的行为一字不变**（fuse-matrix 走的就是这条路）。
        #[serde(default)]
        task: Option<String>,
        /// ★ M8.2：显式要求落盘（与 `task` 二选一；`task` 有值即视为要落盘）。
        #[serde(default)]
        save_task: Option<bool>,
        /// ★ M8.4：这次挂载的冲突策略（5 选项见 `tasks::CONFLICTS`）。
        /// 省略 = 默认「重命名本地文件」（= M2c 既有行为）。
        #[serde(default)]
        conflict: Option<String>,
    },
    Umount {
        mountpoint: PathBuf,
    },
    Mounts,
    /// ★ M8.2：同步任务（持久化的挂载登记 + 策略）。
    ///
    /// * `action="list"`   → 全部任务 + 运行时状态（`TaskInfo`）；
    /// * `action="get"`    → 单个任务（`id` 必填）；
    /// * `action="save"`   → 落盘一个任务（`task` 必填，做归一化 + 校验）；
    /// * `action="delete"` → 删掉登记（**不动挂载点里的任何数据**）；
    /// * `action="pause"`  → `enabled=false` 并**卸载**该任务（见 docs/M8 §M8.2 的语义说明）；
    /// * `action="resume"` → `enabled=true` 并重新挂载；
    /// * `action="mount"`  → 按任务登记的参数挂载（不改登记）。
    Tasks {
        action: String,
        #[serde(default)]
        id: Option<String>,
        /// `save` 用：完整任务对象。
        #[serde(default)]
        task: Option<crate::tasks::Task>,
    },
    /// NAS 目录信息：家目录根 + NAS 上登记的 Qsync 同步文件夹（就是下拉的候选来源）。
    Roots,
    /// ★ M7：选择性同步规则（`exclude` 编译结果 + 可选单路径判定）。
    Rules {
        /// 给一条远端绝对路径，返回「会不会被隐藏 / 是哪条规则」。
        #[serde(default)]
        match_path: Option<String>,
    },
    /// ★ M7：LAN 对等设备（配对 / 探活 / 事件）。
    ///
    /// * `action="status"` → 监听地址、身份、配对码、已配对设备、计数；
    /// * `action="list"`   → 已配对设备（token 掩码）；
    /// * `action="pair"`   → 用 `addr` + `code` 配对（可选 `name`）；
    /// * `action="ping"`   → 探活 `addr`（或已配对设备名）；
    /// * `action="events"` → 最近收到的对端事件；
    /// * `action="notify"` → 把 `path` 当成变更广播给所有对端（测试/脚本用）；
    /// * `action="fetch"`  → 从对端 `addr`（或设备名）直传 `path` 到 `dest`（LAN 直连自检）。
    Peer {
        action: String,
        #[serde(default)]
        addr: Option<String>,
        #[serde(default)]
        code: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        dest: Option<PathBuf>,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// ★ M2c：变更发现（三游标轮询 + baseline 对账）。
    ///
    /// * `once=true`  → 立即跑一轮，返回 [`SyncInfo`]；
    /// * `once=false` → 只返回当前状态（与 `status` 里的 `sync` 相同）；
    /// * `force_deletes=true` → 解除删除熔断并允许这一轮执行批量删除；
    /// * `max_deletes` → 临时覆盖「一次对账最多删多少项」；
    /// * `interval_secs` → 调整后台轮询间隔（0 = 暂停轮询）。
    Sync {
        #[serde(default)]
        once: Option<bool>,
        #[serde(default)]
        force_deletes: Option<bool>,
        #[serde(default)]
        max_deletes: Option<usize>,
        #[serde(default)]
        interval_secs: Option<u64>,
    },
    /// ★ M5：本地状态库（SQLite）自检 —— 路径 / schema 版本 / 游标 / baseline / pin / 队列。
    Store {
        /// 顺带跑 `PRAGMA integrity_check`（慢一点点，给验收脚本用）。
        #[serde(default)]
        integrity: Option<bool>,
    },
    /// 删除远端条目（M2c 测试/脚本用；FUSE 的 unlink 走同一客户端方法）。
    Rm {
        dir: String,
        name: String,
    },
    /// ★ M3：脱水（丢掉本地缓存内容、只留占位符）。
    ///
    /// * `path` 指定单个远端路径（不给就看 `all`/限额/闲置）；
    /// * `idle_secs` 只清闲置 ≥ N 秒的（0 = 不限制）；
    /// * `cache_limit` 形如 `512M` / `2G` / `25%`，按 LRU 清到不超限；
    /// * `force=true` 跳过「刚访问过」保护窗口；
    /// * `dry_run=true` 只算不删。
    Dehydrate {
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        all: Option<bool>,
        #[serde(default)]
        idle_secs: Option<u64>,
        #[serde(default)]
        cache_limit: Option<String>,
        #[serde(default)]
        force: Option<bool>,
        #[serde(default)]
        dry_run: Option<bool>,
        #[serde(default)]
        mountpoint: Option<PathBuf>,
    },
    /// ★ M8.3：同步活动日志（GUI 的「文件更新中心 / 错误列表」读它）。
    ///
    /// * `limit`  最多返回多少条（默认 200，上限 10000）；
    /// * `since`  unix 秒下界（闭区间）；
    /// * `query`  在 path/detail 上做子串匹配（「按文件名搜索」）；
    /// * `level`  `all`（默认）/ `ok` / `error` / `blocked` —— `error` 就是「错误列表」；
    /// * `clear`  true = 清空日志（**只清日志**，不动游标/baseline/pin/队列）。
    Journal {
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        since: Option<i64>,
        #[serde(default)]
        query: Option<String>,
        #[serde(default)]
        level: Option<String>,
        #[serde(default)]
        clear: Option<bool>,
    },
    /// ★ M8.4：全局设置（`~/.config/qxync/settings.json`）—— 只读。
    ///
    /// 返回 `SettingsData`（当前设置 + 落盘路径 + autostart 实际状态 + 环境里的代理变量）。
    Settings,
    /// ★ M8.4：写全局设置。
    ///
    /// * `settings` 完整设置对象（做归一化 + 校验；`manual` 代理缺服务器会**报错**）；
    /// * `autostart_exe` 要写进 `~/.config/autostart/qxync.desktop` 的可执行文件路径
    ///   （GUI 传自己的 `current_exe()`；CLI 不传则尝试取同目录下的 `qxync-gui`）。
    SettingsSave {
        settings: crate::settings::Settings,
        #[serde(default)]
        autostart_exe: Option<String>,
    },
    /// ★ M8.4：冲突策略为「每个文件都问我」时攒下的**待裁决队列**。
    ///
    /// * `action="list"`     → 全部待裁决（`resolution` 为空的 + 已裁决未执行的）；
    /// * `action="resolve"`  → 给 `id` 定夺：`keep_local` / `keep_remote` / `keep_both`；
    /// * `action="clear"`    → 清空队列（**只清队列，不动文件**）。
    Decisions {
        action: String,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        resolution: Option<String>,
    },
    /// ★ M8.4：文件页的「节省空间模式」三态（仅在线 / 本地可用 / 始终可用）。
    ///
    /// `path` 是**远端目录**（如 `/home`）；返回该目录下每个条目的
    /// pin 状态 + 本地已缓存字节 + 归纳出的三态。
    FileStates {
        path: String,
    },
    /// ★ M8.4：释放空间状态（设置 →「释放空间」页的数据源）。
    ///
    /// `now=true` = `Free Up Space Now`：按当前策略立刻跑一轮脱水
    /// （仍然走 M3 的安全检查链）。
    Space {
        #[serde(default)]
        now: Option<bool>,
    },
    /// 干净退出：卸载所有挂载点、删 socket/pid。
    Shutdown,
}

impl Request {
    pub fn method(&self) -> &'static str {
        match self {
            Request::Ping => "ping",
            Request::Status => "status",
            Request::Login { .. } => "login",
            Request::Logout => "logout",
            Request::Ls { .. } => "ls",
            Request::Stat { .. } => "stat",
            Request::Get { .. } => "get",
            Request::Put { .. } => "put",
            Request::Mkdir { .. } => "mkdir",
            Request::Pin { .. } => "pin",
            Request::Mount { .. } => "mount",
            Request::Umount { .. } => "umount",
            Request::Mounts => "mounts",
            Request::Tasks { .. } => "tasks",
            Request::Roots => "roots",
            Request::Rules { .. } => "rules",
            Request::Peer { .. } => "peer",
            Request::Sync { .. } => "sync",
            Request::Store { .. } => "store",
            Request::Rm { .. } => "rm",
            Request::Dehydrate { .. } => "dehydrate",
            Request::Journal { .. } => "journal",
            Request::Settings => "settings",
            Request::SettingsSave { .. } => "settings_save",
            Request::Decisions { .. } => "decisions",
            Request::FileStates { .. } => "file_states",
            Request::Space { .. } => "space",
            Request::Shutdown => "shutdown",
        }
    }

    /// 长任务（下载/上传/挂载）给 CLI 的默认超时更长。
    pub fn is_long_running(&self) -> bool {
        matches!(
            self,
            Request::Get { .. }
                | Request::Put { .. }
                | Request::Mount { .. }
                | Request::Sync { .. }
                | Request::Dehydrate { .. }
                | Request::Peer { .. }
        ) || matches!(
            self,
            // ★ M8.2：list/get/save/delete 很快，只有 mount/resume/pause 会真的挂载
            Request::Tasks { action, .. }
                if matches!(action.as_str(), "mount" | "resume" | "pause")
        )
    }
}

/// 请求信封：`{"v":1,"method":"status", ...}`（`method` 与参数同层）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestEnvelope {
    pub v: u32,
    #[serde(flatten)]
    pub req: Request,
}

impl RequestEnvelope {
    pub fn new(req: Request) -> Self {
        Self {
            v: IPC_VERSION,
            req,
        }
    }
}

// ---------------------------------------------------------------- 响应

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    BadVersion,
    BadRequest,
    NotLoggedIn,
    Auth,
    Transport,
    Status,
    Parse,
    Io,
    Unsupported,
    NotRunning,
}

impl ErrorKind {
    /// 给脚本用的稳定退出码。
    pub fn exit_code(self) -> i32 {
        match self {
            ErrorKind::Auth | ErrorKind::NotLoggedIn => 2,
            ErrorKind::Transport | ErrorKind::NotRunning => 3,
            ErrorKind::Status => 4,
            ErrorKind::Io => 5,
            _ => 1,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcError {
    pub kind: ErrorKind,
    pub message: String,
}

impl IpcError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub v: u32,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<IpcError>,
}

impl Response {
    pub fn ok<T: Serialize>(data: T) -> Self {
        Self {
            v: IPC_VERSION,
            ok: true,
            data: serde_json::to_value(data).ok(),
            error: None,
        }
    }

    pub fn empty() -> Self {
        Self {
            v: IPC_VERSION,
            ok: true,
            data: None,
            error: None,
        }
    }

    pub fn err(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            v: IPC_VERSION,
            ok: false,
            data: None,
            error: Some(IpcError::new(kind, message)),
        }
    }

    pub fn into_result<T: serde::de::DeserializeOwned>(self) -> Result<T, IpcError> {
        if self.ok {
            let v = self.data.unwrap_or(serde_json::Value::Null);
            serde_json::from_value(v)
                .map_err(|e| IpcError::new(ErrorKind::Parse, format!("响应解析失败: {e}")))
        } else {
            Err(self
                .error
                .unwrap_or_else(|| IpcError::new(ErrorKind::BadRequest, "未知错误")))
        }
    }
}

// ---------------------------------------------------------------- 数据载荷

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub version: String,
    pub pid: u32,
    pub uptime_secs: u64,
    pub socket: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkInfo {
    pub id: String,
    pub host: String,
    pub port: u16,
    pub https: bool,
    pub user: String,
    pub ipv4_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    /// **只回掩码**，避免日志/截图泄漏 sid。
    pub sid_masked: String,
    pub alive: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub qsync_version: Option<String>,
    pub qpkg_version: Option<String>,
    pub build: Option<String>,
    pub qbox_cgi: bool,
    pub fcgi: bool,
    pub busy_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CursorInfo {
    pub max_log: u64,
    pub global_notify: u64,
    pub sync_signal: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HydroStats {
    pub count: u64,
    pub bytes: u64,
}

/// 上传队列快照（M2b）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UploadInfo {
    pub pending: u64,
    /// 是否有作业正在上传（在途）。
    #[serde(default)]
    pub active: bool,
    pub done: u64,
    pub failed: u64,
    pub retries: u64,
    pub bytes: u64,
}

/// ★ M11：删除队列快照（`unlink`/`rmdir` 异步入队，后台同目录攒批推送）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeleteInfo {
    /// 排队中（还没发出）的删除条目数。
    pub pending: u64,
    /// 是否正有一批在途（已取走、尚未拿到 NAS 回包）。
    #[serde(default)]
    pub active: bool,
    /// 成功删除的条目数。
    pub done: u64,
    /// 重试超限、放弃的条目数（**非零就该看一眼**）。
    pub failed: u64,
    pub retries: u64,
    /// 累计发出的批次数（每批一次 HTTP 请求）。
    #[serde(default)]
    pub batches: u64,
    pub deleted: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountInfo {
    pub mountpoint: PathBuf,
    /// 这个挂载点对应的**那一个** NAS 文件夹。
    pub remote: String,
    pub readonly: bool,
}

/// 三个持久化游标（M2c；对应 Windows 版注册表里的 `QSYNC_PROCESSED_MAX_*_LOG_INDEX_64`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncCursors {
    pub config: i64,
    pub notify: i64,
    pub global_notify: i64,
    pub max_log_seen: u64,
    pub log_missing_count: u64,
}

/// 变更发现状态（`status` / `sync` 共用）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncInfo {
    /// 轮询是否在跑。
    pub enabled: bool,
    pub interval_secs: u64,
    pub polls: u64,
    /// 距离上次轮询的秒数（没跑过 = 0）。
    pub last_poll_age_secs: u64,
    pub cursors: SyncCursors,
    pub baseline_entries: u64,
    /// 最近一次轮询的统计。
    pub refreshed: u64,
    pub conflicts: u64,
    pub uploaded: u64,
    pub deleted: u64,
    pub deletes_blocked: u64,
    pub events: u64,
    /// 事件里出现过的设备（`uid:次数`），诊断「事件是谁产生的」。
    pub devices: Vec<String>,
    pub last_error: Option<String>,
    /// 删除保护熔断的原因（有值 = 当前有删除被挡住，`--force-deletes` 可放行）。
    pub delete_block_reason: Option<String>,
    pub note: Option<String>,
}

/// ★ M3：本地缓存 / 脱水状态。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheInfo {
    /// `pagecache` / `direct`。
    pub mode: String,
    /// 本地已缓存字节数（各挂载求和）。
    pub used_bytes: u64,
    pub total_files: u64,
    pub hydrated_files: u64,
    /// 当前限额（字节；未设 = None）。
    pub limit_bytes: Option<u64>,
    /// 后台扫描的闲置阈值（秒；0 = 关闭定时脱水）。
    pub idle_secs: u64,
    /// 累计脱水次数 / 释放字节。
    pub dehydrated_total: u64,
    pub freed_total_bytes: u64,
    /// 上次扫描距今秒数（没扫过 = 0）。
    pub last_sweep_age_secs: u64,
    /// 被安全检查挡下的分类计数（最近一次扫描）。
    pub blocked_dirty: u64,
    pub blocked_pinned: u64,
    pub blocked_open: u64,
    pub blocked_mapped: u64,
    pub blocked_inflight: u64,
    pub last_error: Option<String>,
}

/// ★ M3：`dehydrate` 的返回。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DehydrateData {
    pub dehydrated: u64,
    pub freed_bytes: u64,
    /// 执行后的本地缓存字节（全部挂载）。
    pub used_bytes: u64,
    pub limit_bytes: Option<u64>,
    pub dry_run: bool,
    pub targets: Vec<String>,
    /// 被挡下的（路径 → 原因）。
    pub blocked: Vec<(String, String)>,
}

/// ★ M5：本地状态库快照（`store` 请求的返回）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StoreData {
    /// 状态库路径（`<data>/sync/<host>/sync.db`）。
    pub path: String,
    pub schema_version: i64,
    /// `PRAGMA integrity_check` 的结果（只在 `integrity=true` 时有值），正常是 `"ok"`。
    pub integrity: Option<String>,
    pub cursors: SyncCursors,
    pub baseline_entries: u64,
    /// 路径 → pin 状态（M5 起持久化）。
    pub pins: BTreeMap<String, String>,
    /// 未完成的上传作业数。
    pub uploads: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusData {
    pub daemon: DaemonInfo,
    /// 当前 NAS 连接。**`None` = daemon 在空转待命**（进程在跑，但一份连接配置都没有）——
    /// 前端据此显示「未配置」（既有文案 `top.conn_none` / `home.conn_no_link`）。
    #[serde(default)]
    pub link: Option<LinkInfo>,
    pub logged_in: bool,
    pub session: Option<SessionInfo>,
    pub server: Option<ServerInfo>,
    pub cursors: Option<CursorInfo>,
    pub hydro: HydroStats,
    #[serde(default)]
    pub uploads: Option<UploadInfo>,
    /// ★ M11：删除队列（只读挂载为 `None`）。
    #[serde(default)]
    pub deletes: Option<DeleteInfo>,
    #[serde(default)]
    pub sync: Option<SyncInfo>,
    /// ★ M3：缓存/脱水状态。
    #[serde(default)]
    pub cache: Option<CacheInfo>,
    pub mounts: Vec<MountInfo>,
}

/// ★ `qbox_get_syncing_folder_list` 的一项（NAS 侧登记的 Qsync 同步文件夹）。
///
/// ★ 真机字段（2026-10-02 HAR，`detail=1`）是 `name` / `path` / `privilege` /
/// `realpath`，**不是** `folder` / `permission`：解析在 `qxync-client`（两边都认）。
/// 早期单测是照着想象的形状写的，真机从没跑到过「有内容」的响应。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncingFolderInfo {
    /// 同步文件夹的**展示名**（真机 `name`，如 `Qsync`；老字段 `folder` 也认）。
    pub folder: String,
    /// NAS 上报的**共享路径**（真机 `path`，如 `/share/homes/test1/.Qsync`）。
    pub path: Option<String>,
    /// ★ 映射成**客户端可见路径**（如 `/home/.Qsync`）—— 由 `qxync-client` 按
    /// `home_root` + 用户名换算；映射不出来时为 `None`（GUI 就别把它当选项目）。
    pub client_path: Option<String>,
    pub permission: i64,
    pub read_deletable: bool,
    pub realpath: Option<String>,
    pub volume_id: Option<String>,
}

/// ★ `roots` 请求的返回：NAS 上登记的同步文件夹（= 一对一配对时可选的 NAS 文件夹）。
///
/// 家目录不是「配置项」而是 Qsync 协议里的固定命名空间（[`crate::HOME_ROOT`]），
/// 所以这里不再回一个 `home_root` 字段。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RootsData {
    /// NAS 上报的 Qsync 同步文件夹（普通账号没配对时是空数组 —— 实测如此）。
    pub syncing_folders: Vec<SyncingFolderInfo>,
    /// 一句话解释（例如「未登录时拿不到同步文件夹列表」）。
    pub note: Option<String>,
}

/// ★ M8.3：`journal` 请求的返回。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct JournalData {
    /// 明细（**最新的在前**）。
    pub entries: Vec<crate::store::JournalEntry>,
    /// 库里的总条数（不是本次返回的条数）。
    pub total: i64,
    /// 按状态分组的计数（`ok` / `error` / `blocked`）。
    pub counts: BTreeMap<String, i64>,
    /// 本次是否执行了清空。
    pub cleared: bool,
    /// 清空时删掉的行数。
    pub removed: usize,
    /// 上限与轮转参数（让界面能解释「为什么只有这些」）。
    pub limit_rows: i64,
    pub max_age_days: i64,
    pub note: Option<String>,
}

/// ★ M8.2：一条任务的落盘登记 + 当前运行时状态。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskInfo {
    pub task: crate::tasks::Task,
    /// 该任务的挂载点此刻是否真的挂着（daemon 挂载表里有它）。
    pub mounted: bool,
    /// 最近一次挂载/恢复失败的原因（没有 = None）。
    pub last_error: Option<String>,
}

/// ★ M8.2：`tasks` 请求 `action="list"` 的返回。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TasksData {
    /// 落盘的任务。
    pub tasks: Vec<TaskInfo>,
    /// 解析失败的任务文件（路径，原因）—— **不整体失败**，单独报出来。
    pub bad_files: Vec<(String, String)>,
    /// 任务目录里**一个任务文件都没有**时为 true（界面据此提示「还没建过任务」）。
    pub empty: bool,
    /// 一句话解释。
    pub note: Option<String>,
}

impl TasksData {
    pub fn count(&self) -> usize {
        self.tasks.len()
    }
    pub fn enabled_count(&self) -> usize {
        self.tasks.iter().filter(|t| t.task.enabled).count()
    }
}

/// ★ M7：`rules` 请求的返回（`qxync rules [--json] [--match PATH]`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RulesData {
    /// 生效的远端根（多根时不止一个）。
    pub roots: Vec<String>,
    /// link 里配的原始 `exclude`。
    pub exclude: Vec<String>,
    /// 编译后的规则（按生效顺序，已去掉注释/空行）。
    pub patterns: Vec<String>,
    /// 解析不了的规则原文（不静默）。
    pub bad: Vec<String>,
    pub filter_temp: bool,
    /// 内置临时文件规则（自检/文档用）。
    pub temp_patterns: Vec<String>,
    /// `--match` 的输入。
    pub match_path: Option<String>,
    /// 命中哪个根 + 根相对路径。
    pub match_root: Option<String>,
    pub match_rel: Option<String>,
    /// 判定结果：true = 挂载点里看不到。
    pub match_hidden: Option<bool>,
    /// `excluded` / `temp` / `visible`。
    pub match_reason: Option<String>,
}

/// ★ M7：一台已配对设备（token 只回掩码）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PeerDeviceInfo {
    pub name: String,
    pub addr: String,
    /// 形如 `ab12…ef90`，**绝不回全量 token**。
    pub token_masked: String,
}

/// ★ M7：`peer` 请求的通用返回（按 action 填不同字段）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PeerData {
    pub action: String,
    /// 是否配置了 `peer_listen`（LAN 服务开关）。
    pub enabled: bool,
    /// 实际绑定的地址（服务起来了才有值）。
    pub listen: Option<String>,
    /// 本机对等身份名。
    pub identity: String,
    pub roots: Vec<String>,
    /// 当前配对码（`status` 才回；配对成功后轮换）。
    pub pairing_code: Option<String>,
    pub devices: Vec<PeerDeviceInfo>,
    /// 累计：发出/收到的事件、被拒请求、LAN 命中区间与字节。
    pub events_out: u64,
    pub events_in: u64,
    pub rejected: u64,
    pub lan_hits: u64,
    pub lan_bytes: u64,
    /// `ping` 结果。
    pub peer_name: Option<String>,
    pub peer_version: Option<String>,
    pub peer_roots: Vec<String>,
    pub pairing_open: Option<bool>,
    pub took_ms: Option<u64>,
    /// `pair` 结果。
    pub paired_name: Option<String>,
    pub paired_addr: Option<String>,
    pub paired_token_masked: Option<String>,
    /// `events` 结果（倒序，最新在前）。
    pub events: Vec<PeerEventInfo>,
    /// `fetch` 结果。
    pub fetch_bytes: Option<u64>,
    pub fetch_from: Option<String>,
    pub fetch_dest: Option<PathBuf>,
    /// 一句话说明。
    pub note: Option<String>,
}

/// ★ M7：一条对端事件（`peer events`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PeerEventInfo {
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    pub kind: String,
    pub ts: u64,
    /// 从哪台设备收到（我们记的是事件里带的路径来源；名字不可用时为空）。
    pub from: Option<String>,
}

pub fn mask_token(token: &str) -> String {
    let n = token.chars().count();
    if n <= 8 {
        return "****".to_string();
    }
    let head: String = token.chars().take(4).collect();
    let tail: String = token.chars().skip(n - 4).collect();
    format!("{head}…{tail}")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginData {
    pub sid_masked: String,
    pub user: String,
    pub uid: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LsData {
    pub path: String,
    pub total: usize,
    pub entries: Vec<DirEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetData {
    pub bytes: u64,
    pub dest: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PutData {
    pub bytes: u64,
    pub remote_path: String,
    pub mtime: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingData {
    pub pong: bool,
    pub daemon_version: String,
    pub pid: u32,
    pub uptime_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShutdownData {
    pub unmounted: usize,
}

// ---------------------------------------------------------------- ★ M8.4 数据体

/// ★ M8.4：`settings` / `settings_save` 的返回。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SettingsData {
    pub settings: crate::settings::Settings,
    /// `settings.json` 的落盘路径（**可能还不存在**：全默认时不写文件）。
    pub path: String,
    /// `~/.config/autostart/qxync.desktop` 路径。
    pub autostart_path: String,
    /// 该桌面项此刻是否真的存在（「开机自启」是否已生效）。
    pub autostart_present: bool,
    /// 本次 save 是否真的写了文件。
    pub saved: bool,
    /// 环境里的代理变量（「自动检测」实际会读到什么，如实展示）。
    pub proxy_env: BTreeMap<String, String>,
    /// `manual` 代理解析出来的 URL（诊断用；无 = None）。
    pub proxy_url: Option<String>,
    /// 一句话说明（例如「改了 peer_listen 需要重启 daemon」）。
    pub note: Option<String>,
}

/// ★ M8.4：待裁决队列里的一条冲突。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DecisionInfo {
    /// 稳定 id（路径 + 首次发现时间）。
    pub id: String,
    /// 远端绝对路径（冲突的那个文件）。
    pub path: String,
    /// 属于哪个任务（事件里带的 task id；缺省 `default`）。
    pub task_id: String,
    /// 本地签名（大小 / mtime），裁决时给人看的。
    pub local_size: u64,
    pub local_mtime: i64,
    /// 远端签名。
    pub remote_size: u64,
    pub remote_mtime: i64,
    pub is_dir: bool,
    /// 首次发现时间（unix 秒）。
    pub created_unix: i64,
    /// `None` = 还没裁决；否则是 `keep_local` / `keep_remote` / `keep_both`。
    pub resolution: Option<String>,
}

/// ★ M8.4：一条文件的三态（对齐 Qsync 的 Online-only / Locally available / Always available）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpaceState {
    /// 仅在线（占位符，没有任何本地内容）。
    Online,
    /// 本地可用（部分或全部内容已在本地缓存，但没有 pin）。
    Local,
    /// 始终可用（`pin=pinned`，永不自动脱水）。
    Always,
}

impl SpaceState {
    /// GUI 文案（照抄 §1.7 术语表）。
    pub fn label(self) -> &'static str {
        match self {
            SpaceState::Online => "仅在线",
            SpaceState::Local => "本地可用",
            SpaceState::Always => "始终可用",
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            SpaceState::Online => "online",
            SpaceState::Local => "local",
            SpaceState::Always => "always",
        }
    }
}

/// ★ M8.4：`file_states` 里的一条。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FileStateInfo {
    pub name: String,
    /// 该条目的远端绝对路径。
    pub remote: String,
    pub is_dir: bool,
    pub size: u64,
    /// 本地已缓存字节数（0 = 仅在线）。
    pub hydrated_bytes: u64,
    /// `online` / `local` / `always`。
    pub state: String,
    /// 原始 pin 状态（`pinned` / `unpinned` / `unspecified` / `excluded`）。
    pub pin: String,
    /// 是否本地有未上传改动（界面上要区别对待）。
    pub dirty: bool,
    /// 该条目是否被规则隐藏（隐藏的不该出现在三态列里，这里只是兜底信息）。
    pub hidden: bool,
}

/// ★ M8.4：`file_states` 的返回。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FileStatesData {
    pub path: String,
    /// 命中的挂载点；没挂载时为 None（此时 `entries` 为空）。
    pub mountpoint: Option<String>,
    /// 命中的远端根。
    pub root: Option<String>,
    pub entries: Vec<FileStateInfo>,
    /// 汇总（验收脚本直接断言这三个数）。
    pub online: usize,
    pub local: usize,
    pub always: usize,
    pub note: Option<String>,
}

/// ★ M8.4：`space` 的返回（设置 →「释放空间」页 + `qxync space`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SpaceData {
    /// 量的哪个路径（缓存目录所在文件系统）。
    pub fs_path: String,
    pub fs_total: u64,
    pub fs_avail: u64,
    pub fs_free: u64,
    pub fs_avail_pct: u8,
    /// 缓存当前占用字节。
    pub cache_used_bytes: u64,
    /// 设置里的自动释放策略。
    pub auto: bool,
    pub mode: String,
    pub below_pct: u8,
    pub every_hours: u64,
    /// 按现在的空间/时间，这一轮**会不会**触发（判定结果，纯函数算出来的）。
    pub would_run: bool,
    pub reason: String,
    /// 上一次触发时间（unix 秒；0 = 从未）。
    pub last_run_unix: u64,
    /// 是否在用 `QXNYC_TEST_FAKE_STATVFS` 注入值（验收要能看见这一点）。
    pub injected: bool,
    /// `now=true` 时是否真的跑了。
    pub ran: bool,
    pub dehydrated: u64,
    pub freed_bytes: u64,
    /// 被安全检查链挡下的项（(`路径`, 原因)）—— **自动释放也不例外**。
    pub blocked: Vec<(String, String)>,
    pub note: Option<String>,
}

/// ★ M8.4：`decisions` 的返回。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DecisionsData {
    pub action: String,
    /// 全部待裁决项（含已裁决待执行）。
    pub decisions: Vec<DecisionInfo>,
    /// 尚未裁决的条数。
    pub pending: usize,
    /// 已裁决待执行的条数。
    pub resolved: usize,
    /// `clear` 删掉了多少条。
    pub removed: usize,
    pub note: Option<String>,
}

/// sid 掩码（诊断用，绝不回传完整值）。
pub fn mask_sid(sid: &str) -> String {
    let head: String = sid.chars().take(4).collect();
    format!("{head}…")
}

// ---------------------------------------------------------------- 行帧编解码

/// 编码成一行（JSON 紧凑格式天然不含换行）。
pub fn encode_line<T: Serialize>(value: &T) -> Result<Vec<u8>, serde_json::Error> {
    let mut buf = serde_json::to_vec(value)?;
    buf.push(b'\n');
    Ok(buf)
}

/// 解码一行（容忍尾随 `\r`/空白）。
pub fn decode_line<T: serde::de::DeserializeOwned>(line: &[u8]) -> Result<T, serde_json::Error> {
    let s = std::str::from_utf8(line).unwrap_or("");
    serde_json::from_str(s.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trip_is_flat() {
        let req = RequestEnvelope::new(Request::Ls {
            path: "/home/qxync-test".into(),
        });
        let line = encode_line(&req).unwrap();
        let text = String::from_utf8(line.clone()).unwrap();
        assert!(text.contains("\"method\":\"ls\""), "{text}");
        assert!(text.contains("\"v\":1"), "{text}");
        assert!(text.ends_with('\n'));
        let back: RequestEnvelope = decode_line(&line).unwrap();
        assert_eq!(back.req, req.req);
        assert_eq!(back.v, IPC_VERSION);
    }

    #[test]
    fn mount_request_round_trip() {
        let e = RequestEnvelope::new(Request::Mount {
            mountpoint: "/home/me/mnt".into(),
            remote: Some("/home".into()),
            cache_dir: None,
            threads: Some(4),
            auto_unmount: Some(true),
            hydrate_timeout_secs: Some(600),
            read_write: Some(false),
            delete_limit: Some(0),
            cache_mode: Some("direct".into()),
            task: Some("t1".into()),
            save_task: None,
            conflict: Some("ask".into()),
        });
        let line = encode_line(&e).unwrap();
        let back: RequestEnvelope = decode_line(&line).unwrap();
        match back.req {
            Request::Mount {
                threads,
                hydrate_timeout_secs,
                remote,
                delete_limit,
                cache_mode,
                ..
            } => {
                assert_eq!(threads, Some(4));
                assert_eq!(hydrate_timeout_secs, Some(600));
                assert_eq!(remote.as_deref(), Some("/home"));
                assert_eq!(delete_limit, Some(0));
                assert_eq!(cache_mode.as_deref(), Some("direct"));
            }
            other => panic!("解成了 {other:?}"),
        }
    }

    #[test]
    fn sync_request_round_trip_and_flag() {
        let e = RequestEnvelope::new(Request::Sync {
            once: Some(true),
            force_deletes: Some(false),
            max_deletes: Some(5),
            interval_secs: None,
        });
        let back: RequestEnvelope = decode_line(&encode_line(&e).unwrap()).unwrap();
        match back.req {
            Request::Sync {
                once, max_deletes, ..
            } => {
                assert_eq!(once, Some(true));
                assert_eq!(max_deletes, Some(5));
            }
            other => panic!("解成了 {other:?}"),
        }
        assert!(e.req.is_long_running());
        assert_eq!(
            Request::Rm {
                dir: "/home".into(),
                name: "a".into()
            }
            .method(),
            "rm"
        );
    }

    #[test]
    fn dehydrate_request_round_trip() {
        let e = RequestEnvelope::new(Request::Dehydrate {
            path: Some("/home/a.bin".into()),
            all: Some(false),
            idle_secs: Some(600),
            cache_limit: Some("512M".into()),
            force: Some(true),
            dry_run: Some(true),
            mountpoint: None,
        });
        let back: RequestEnvelope = decode_line(&encode_line(&e).unwrap()).unwrap();
        match back.req {
            Request::Dehydrate {
                path,
                idle_secs,
                cache_limit,
                dry_run,
                ..
            } => {
                assert_eq!(path.as_deref(), Some("/home/a.bin"));
                assert_eq!(idle_secs, Some(600));
                assert_eq!(cache_limit.as_deref(), Some("512M"));
                assert_eq!(dry_run, Some(true));
            }
            other => panic!("解成了 {other:?}"),
        }
        assert_eq!(e.req.method(), "dehydrate");
        assert!(e.req.is_long_running());
        // CacheInfo / DehydrateData 的序列化默认值要能解析（向前兼容）
        let info: CacheInfo = serde_json::from_str("{}").unwrap();
        assert_eq!(info.used_bytes, 0);
        let d: DehydrateData = serde_json::from_str("{}").unwrap();
        assert_eq!(d.dehydrated, 0);
    }

    #[test]
    fn roots_request_round_trip_and_defaults() {
        let e = RequestEnvelope::new(Request::Roots);
        assert_eq!(e.req.method(), "roots");
        assert!(!e.req.is_long_running());
        let back: RequestEnvelope = decode_line(&encode_line(&e).unwrap()).unwrap();
        assert_eq!(back.req, Request::Roots);
        // 老客户端不看新字段、新客户端不看老字段：默认值都要能解析
        let d: RootsData = serde_json::from_str("{}").unwrap();
        assert!(d.syncing_folders.is_empty());
        let m: MountInfo =
            serde_json::from_str(r#"{"mountpoint":"/m","remote":"/home","readonly":true}"#)
                .unwrap();
        assert_eq!(m.remote, "/home");
        // ★ LinkInfo：老响应里可能还带着已删除的 `roots` / `home_root` 字段 → 必须能忽略
        let li: LinkInfo = serde_json::from_str(
            r#"{"id":"default","host":"nas","port":9834,"https":true,"user":"u","ipv4_only":false,
                "home_root":"/home","roots":["/home"]}"#,
        )
        .unwrap();
        assert_eq!(li.id, "default");
    }

    #[test]
    fn m7_rules_request_round_trip() {
        let e = RequestEnvelope::new(Request::Rules {
            match_path: Some("/home/qxync-test/secret".into()),
        });
        assert_eq!(e.req.method(), "rules");
        assert!(!e.req.is_long_running());
        let back: RequestEnvelope = decode_line(&encode_line(&e).unwrap()).unwrap();
        match back.req {
            Request::Rules { match_path } => {
                assert_eq!(match_path.as_deref(), Some("/home/qxync-test/secret"))
            }
            other => panic!("解析成了 {other:?}"),
        }
        // 省略 match_path 也要能解析（CLI 只跑 `qxync rules`）
        let e2: RequestEnvelope = serde_json::from_str(r#"{"v":1,"method":"rules"}"#).unwrap();
        assert_eq!(e2.req, Request::Rules { match_path: None });
        // 响应缺字段 → 默认值
        let d: RulesData = serde_json::from_str("{}").unwrap();
        assert!(d.patterns.is_empty() && d.match_hidden.is_none() && !d.filter_temp);
    }

    #[test]
    fn m7_peer_request_round_trip_and_token_mask() {
        let e = RequestEnvelope::new(Request::Peer {
            action: "pair".into(),
            addr: Some("127.0.0.1:9840".into()),
            code: Some("123456".into()),
            name: Some("laptop".into()),
            path: None,
            dest: None,
            limit: None,
        });
        assert_eq!(e.req.method(), "peer");
        assert!(e.req.is_long_running(), "配对/直传可能慢，给长超时");
        let back: RequestEnvelope = decode_line(&encode_line(&e).unwrap()).unwrap();
        match back.req {
            Request::Peer {
                action, addr, code, ..
            } => {
                assert_eq!(action, "pair");
                assert_eq!(addr.as_deref(), Some("127.0.0.1:9840"));
                assert_eq!(code.as_deref(), Some("123456"));
            }
            other => panic!("解析成了 {other:?}"),
        }
        // status 只有 action
        let e2: RequestEnvelope =
            serde_json::from_str(r#"{"v":1,"method":"peer","action":"status"}"#).unwrap();
        assert_eq!(
            e2.req,
            Request::Peer {
                action: "status".into(),
                addr: None,
                code: None,
                name: None,
                path: None,
                dest: None,
                limit: None,
            }
        );
        let d: PeerData = serde_json::from_str("{}").unwrap();
        assert!(d.devices.is_empty() && d.events.is_empty() && d.listen.is_none());
        assert_eq!(mask_token("abcdefghijkl"), "abcd…ijkl");
        assert_eq!(mask_token("short"), "****");
        assert_eq!(mask_token(""), "****");
    }

    #[test]
    fn store_request_round_trip() {
        let e = RequestEnvelope::new(Request::Store {
            integrity: Some(true),
        });
        let back: RequestEnvelope = decode_line(&encode_line(&e).unwrap()).unwrap();
        match back.req {
            Request::Store { integrity } => assert_eq!(integrity, Some(true)),
            other => panic!("解成了 {other:?}"),
        }
        assert_eq!(e.req.method(), "store");
        assert!(!e.req.is_long_running());
        // 空对象也要能解析（向前兼容）
        let d: StoreData = serde_json::from_str("{}").unwrap();
        assert_eq!(d.baseline_entries, 0);
        assert_eq!(d.schema_version, 0);
        assert!(d.pins.is_empty());
        assert!(d.integrity.is_none());
    }

    #[test]
    fn response_ok_and_err() {
        let ok = Response::ok(LsData {
            path: "/home".into(),
            total: 0,
            entries: vec![],
        });
        let line = encode_line(&ok).unwrap();
        let back: Response = decode_line(&line).unwrap();
        let data: LsData = back.into_result().unwrap();
        assert_eq!(data.path, "/home");

        let e = Response::err(ErrorKind::Auth, "sid 过期");
        let back: Response = decode_line(&encode_line(&e).unwrap()).unwrap();
        let err = back.into_result::<LsData>().unwrap_err();
        assert_eq!(err.kind, ErrorKind::Auth);
        assert_eq!(err.kind.exit_code(), 2);
    }

    #[test]
    fn sid_mask_never_leaks() {
        assert_eq!(mask_sid("abcd1234efgh"), "abcd…");
        assert_eq!(mask_sid("xy"), "xy…");
    }

    #[test]
    fn method_and_long_running_flags() {
        assert_eq!(Request::Status.method(), "status");
        assert!(Request::Get {
            dir: "/home".into(),
            name: "a".into(),
            dest: "/tmp/a".into()
        }
        .is_long_running());
        assert!(!Request::Ls { path: "/".into() }.is_long_running());
    }

    #[test]
    fn unknown_method_is_a_parse_error_not_a_panic() {
        let bad = br#"{"v":1,"method":"nope"}"#;
        assert!(decode_line::<RequestEnvelope>(bad).is_err());
    }
}
