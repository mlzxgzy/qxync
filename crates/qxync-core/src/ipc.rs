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
///
/// ## 什么时候才该 bump（关系到能不能混跑新旧版本）
///
/// daemon 对 `v` 是**严格相等**校验（不等于就回 `BadVersion` 拒绝整个请求），所以
/// bump = 新旧 CLI/GUI/daemon **完全不能混跑**。因此：
///
/// * **只加字段 / 只加 action 取值** → **不 bump**。响应结构体都带
///   `#[serde(default)]`：老客户端解新 payload 会忽略未知字段，新客户端解老 payload
///   会拿到默认值；新增 action 在旧 daemon 上会得到明确的 `未知 action "xxx"` 错误
///   （不是静默当成 list）。
/// * **改帧格式 / 改已有字段含义 / 删字段** → 必须 bump。
///
/// 本文件新增的 `DecisionsData.removed_one` 与 `DiffData` 属于第一类，故仍是 1。
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

/// ★ M16：某个连接的 IPC socket 路径。
///
/// * `default` → 旧的 `qxyncd.sock`（`QXNYC_SOCKET` 仍可精确覆盖，测试/多实例兼容）；
/// * 其它 → 与 default 同一个目录下的 `qxyncd-<id>.sock`。**不看 `QXNYC_SOCKET` 的
///   文件名，只看它的目录**：这样隔离环境里设一个 socket 路径，多连接的 socket
///   仍然落在同一个运行目录，不会互相踩。
pub fn socket_path_for(link_id: &str) -> PathBuf {
    let default = default_socket_path();
    if link_id == "default" {
        return default;
    }
    let dir = default
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    dir.join(format!(
        "qxyncd-{}.sock",
        crate::config::sanitize_component(link_id)
    ))
}

/// ★ M16：某个连接的 pid 文件。
pub fn pid_path_for(link_id: &str) -> PathBuf {
    if link_id == "default" {
        return default_pid_path();
    }
    let dir = default_pid_path()
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/tmp/qxync"));
    dir.join(format!(
        "qxyncd-{}.pid",
        crate::config::sanitize_component(link_id)
    ))
}

/// ★ M16：某个连接的滚动日志文件前缀（`tracing_appender::rolling::daily` 用）。
///
/// default 保持 `qxyncd.log`，其余 `qxyncd-<id>.log`。
pub fn log_prefix_for(link_id: &str) -> String {
    if link_id == "default" {
        "qxyncd.log".into()
    } else {
        format!("qxyncd-{}.log", crate::config::sanitize_component(link_id))
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
    ///
    /// ★ M17：`otp` 是两步验证（2SV）的一次性输入 —— 6 位验证码，或 Base32 密钥 /
    /// `otpauth://` URI。省略时用凭据里保存的 `otp_secret`；验证码本身不落盘。
    Login {
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        password: Option<String>,
        #[serde(default)]
        otp: Option<String>,
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
    /// * `action="purge"`  → ★ M19：删掉登记 + 该任务的水合缓存 / baseline / 日志 /
    ///   待裁决（先卸载；挂载点里的文件与 NAS 数据不动）；
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
    /// * `action="forget"`   → **删掉单条**（只删队列行，不动文件）。给用户一个
    ///   「这条我不想管」的出口 —— 否则已裁决的条目只能干等下一轮同步，看着像卡住；
    /// * `action="diff"`     → 本地 vs NAS 的**文本对比**（见 [`DiffOutcome`]）；
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
    /// ★ M23：Baloo 索引状态检测（Dolphin 集成的前置条件）。
    ///
    /// 只读：解析 `~/.config/baloofilerc` + 对照当前所有挂载点，返回
    /// 每个挂载点「是否已排除 / 会不会被索引 / 建议执行的排除命令」。
    /// GUI 开 Dolphin 集成开关前先调它做预检。
    BalooStatus,
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
            Request::BalooStatus => "baloo_status",
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
        ) || matches!(
            self,
            // ★ 冲突文本对比要 stat + 读本地缓存 + 从 NAS 拉最多 `DIFF_MAX_BYTES`
            //   （512 KiB）再跑 LCS；list/resolve/forget/clear 都是轻操作。
            //   CLI 与 GUI 共用这张表，30s 超时会把慢链路上的正常对比判成超时。
            Request::Decisions { action, .. } if action.as_str() == "diff"
        )
    }
}

/// 请求信封：`{"v":1,"method":"status", ...}`（`method` 与参数同层）。
///
/// ★ M26：`progress` 是**客户端能力声明** —— 置 true 表示「这条连接上的调用方
/// 认识进度行」。daemon 看到它才会在长任务（当前只有 `get`）期间往同一条
/// socket 推 [`ProgressLine`]。缺省 false：老 CLI / GUI 不声明 → daemon 一个
/// 进度行都不发，行为与 M25 及以前**一字不变**（serde 对未知字段宽容，
/// 新 CLI 打老 daemon 也一样安全 —— 老 daemon 忽略这个字段）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestEnvelope {
    pub v: u32,
    #[serde(default)]
    pub progress: bool,
    #[serde(flatten)]
    pub req: Request,
}

impl RequestEnvelope {
    pub fn new(req: Request) -> Self {
        Self {
            v: IPC_VERSION,
            progress: false,
            req,
        }
    }

    /// 声明「我认识进度行」的信封（CLI 的 `get` 用）。
    pub fn with_progress(req: Request) -> Self {
        Self {
            v: IPC_VERSION,
            progress: true,
            req,
        }
    }
}

// ---------------------------------------------------------------- 进度行

/// 一条进度推送：`{"v":1,"progress":{"written":..,"total":..}}`。
///
/// `total` 缺失 = 服务端没给 `Content-Length`（chunked），展示侧按「未知总量」处理。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProgressData {
    pub written: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

/// 进度行的完整线格式（与 [`Response`] 同一条 socket 上混发）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProgressLine {
    pub v: u32,
    pub progress: ProgressData,
}

/// 连接上可能读到的一行：进度行或最终响应。
///
/// unagged 反序列化先试 [`ConnLine::Progress`]（要求 `progress` 字段存在），
/// 再试 [`ConnLine::Resp`]（要求 `ok` 字段存在）—— 两种线格式没有歧义。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ConnLine {
    Progress(ProgressLine),
    Resp(Response),
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    /// GUI 的「跳过 TLS 校验」（对应 `LinkConfig.insecure`）。
    ///
    /// GUI 的 `same_link` 靠它判断「重存连接后要不要重启 daemon 才生效」：
    /// 没这个字段时，用户勾/去勾该选项再保存，daemon 仍按旧值跑（改了没生效）。
    ///
    /// `#[serde(default)]` 是双向兼容的关键：老 daemon 的 payload 没有这个键 →
    /// 按 `false` 解析（最坏情况是新 GUI + 老 daemon 多重启一次 daemon，
    /// 不会把「配置不一致」误判成一致）。
    #[serde(default)]
    pub insecure: bool,
    pub user: String,
    pub ipv4_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    /// **只回掩码**，避免日志/截图泄漏 sid。
    pub sid_masked: String,
    pub alive: bool,
    /// ★ M22：`alive` 这条**是不是真探测过**。
    ///
    /// `false` 的含义是「不知道」而不是「死了」：已知离线时 `status` 不发
    /// `check_alive`（M20 的 20s×3 卡顿修复），此时 `alive` 只是保守默认值
    /// `false`。界面必须按「未知」展示，不能写成「失活」—— 那会让用户以为
    /// 会话真失效了去重登，而其实只是网断了。
    ///
    /// `#[serde(default)]`：老 daemon 的 payload 没有这个键 → 按 `false`
    /// （未知）解析 —— 宁可多显示一次「未知」，不把「没探测」粉饰成「失活」。
    #[serde(default)]
    pub alive_known: bool,
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
    /// **累计**水合次数（自 daemon 启动，只增不清）—— 不是「此刻在传的水合数」。
    pub count: u64,
    /// **累计**水合字节数（同上）。
    pub bytes: u64,
}

/// 上传队列快照（M2b）。
///
/// ★ 只有 `pending` / `active` 是「此刻」；`done` / `failed` / `retries` / `bytes`
/// 是队列生命周期内的**累计值**（只增不清）—— 展示时必须标明「累计」，否则
/// 「失败 3」会被读成「现在有 3 个失败作业」。
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
///
/// ★ 同 [`UploadInfo`]：`pending` / `active` 是此刻，其余是**累计值**。
/// `done` 与 `deleted` 都只增不清（成功一批两个各加「这批条目数」，数值相同）。
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
    /// ★ M23：Baloo 索引风险提示（**非阻塞**）。
    ///
    /// `Some(...)` = 这个挂载点会被 Baloo 索引（内容索引器会把占位符整个拉下来，
    /// 与 Dolphin 集成开关无关 —— 挂载本身照常成功，但用户应该排除它）。
    /// `None` = 安全（已排除 / Baloo 关 / 没装 Baloo）。
    /// 老客户端忽略未知字段，不破坏兼容。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baloo_warning: Option<String>,
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
///
/// ★ **哪些是累计、哪些是此刻**（展示时别混）：
///
/// * 累计（`SyncStats` 的 `AtomicU64`，自 daemon 启动只增不清）：`polls`、
///   `events`、`refreshed`、`uploaded`、`conflicts`、`deleted`、`deletes_blocked`；
/// * 此刻：`enabled` / `interval_secs` / `last_poll_age_secs` / `baseline_entries` /
///   `cursors`。`delete_block_reason` / `last_error` 是「最近一次**完整**轮询」的结论
///   （daemon 在干净轮次会清空它们，见 `sync::absorb_round`），既不累计也非实时。
///
/// `refreshed` 等曾被误写成「最近一次轮询的统计」—— 实际 daemon 每轮 `fetch_add`，
/// 用户会看到「冲突 5000」以为现在积了 5000 条冲突。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncInfo {
    /// 轮询是否在跑。
    pub enabled: bool,
    pub interval_secs: u64,
    /// **累计**轮询次数（自 daemon 启动）。
    pub polls: u64,
    /// 距离上次轮询的秒数（没跑过 = 0）。
    pub last_poll_age_secs: u64,
    pub cursors: SyncCursors,
    pub baseline_entries: u64,
    /// **累计**远端刷新次数（自 daemon 启动，只增不清）。
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
    /// 被安全检查挡下的分类计数 —— **累计值**（daemon 每次脱水 `fetch_add`，只增不清），
    /// 不是「最近一次扫描」的数。展示时必须标明累计，否则会被读成「现在有这么多被挡」。
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
    /// ★ M15/T8：**全部挂载点**的传输汇总（下载 + 上传）。
    ///
    /// 单机同时可能有好几个挂载点都在水合，逐个去看 `file_states` 太贵，
    /// 而「现在一共有几个文件在传」是首页最想知道的一句话。
    #[serde(default)]
    pub transfers: Option<TransferInfo>,
    /// ★ M20：连通性状态（在线 / 离线 / 还没试过网络）。
    ///
    /// 界面据此显示「离线」与离线时长；老 daemon 不给这个字段（`None`）时
    /// 前端必须按「正常」处理，不能把缺字段当成离线。
    #[serde(default)]
    pub connectivity: Option<crate::connectivity::ConnectivitySnapshot>,
    /// ★ M20：离线缓冲区汇总（**只有真的有待同步内容时才出现**，避免首页常驻一行 0）。
    #[serde(default)]
    pub offline: Option<OfflineInfo>,
    pub mounts: Vec<MountInfo>,
}

/// ★ M20：离线缓冲区汇总 —— 「离线期间攒了多少改动、联网后要推多久」。
///
/// 三个来源合并展示：上传队列（文件内容）/ 删除队列 / 结构操作 outbox（建目录、改名、
/// 只改 mtime）。分开给计数是为了让用户能判断「只是几个文件没传」还是
/// 「离线时组织过目录」—— 后者恢复时要先回放结构操作，会有可见的先后顺序。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OfflineInfo {
    /// 待上传的文件数。
    pub pending_upload: u64,
    /// 待删除的条目数。
    pub pending_delete: u64,
    /// 待回放的「新建目录」数。
    pub pending_mkdir: u64,
    /// 待回放的「改名 / 移动」数。
    pub pending_rename: u64,
    /// 待回放的「只改 mtime」数。
    pub pending_set_mtime: u64,
    /// 是否还有结构操作待回放 / 正在回放（`true` 时上传/删除队列被冻结）。
    ///
    /// 离线时是「排队等联网」，联网后是「正在回放」—— 对界面来说都是
    /// 「先等这些结构操作落地，文件内容才会开始传」。
    pub replaying: bool,
    /// 最近一次回放的结论 / 错误（人话，直接展示）。
    pub last_note: Option<String>,
}

impl OfflineInfo {
    /// 一共压着多少项（含上传/删除/结构操作）。
    pub fn total(&self) -> u64 {
        self.pending_upload
            + self.pending_delete
            + self.pending_mkdir
            + self.pending_rename
            + self.pending_set_mtime
    }

    /// 是否有任何待同步内容。
    pub fn is_empty(&self) -> bool {
        self.total() == 0 && !self.replaying
    }
}

/// ★ M15/T8：传输进度汇总（`status` 的增量字段）。
///
/// 只报**计数与字节**，不报百分比 —— 各文件大小差几个数量级，算一个「平均
/// 百分比」出来既不均值也没有意义。真正要看某个文件传到哪，去 `file_states`。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TransferInfo {
    /// 当前有在传作业的文件数。
    pub active: usize,
    /// 下载在途的文件数。
    pub downloading: usize,
    /// 上传在途的文件数。
    pub uploading: usize,
    /// 本次查询时各在途文件已传字节之和。
    pub done_bytes: u64,
    /// 本次查询时各在途文件总字节之和。
    pub total_bytes: u64,
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
    /// ★ M17：这次登录是否通过了两步验证（给 GUI 显示，不影响会话）。
    #[serde(default)]
    pub two_step: bool,
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

/// `shutdown` 的返回（**先回包、再退出**，所以这里只报「收到请求那一刻」的状态）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ShutdownData {
    /// 本次退出真正卸载了几个挂载点。旧版本恒为 0（退出路径异步完成，回包时还没卸）。
    pub unmounted: usize,
    /// 收到停止请求时，上传队列里还没落盘的作业数（daemon 会尽力排空，最长 120 秒）。
    ///
    /// ★ 给 `qxync daemon stop` 用：队列里有东西时停止会慢，CLI 据此告诉用户
    /// 「在等什么」，而不是干等 2 分钟看起来像卡死。旧 daemon 不回这两个字段 → 0。
    pub pending_uploads: usize,
    /// 收到停止请求时，删除队列里还没推完的作业数（同上）。
    pub pending_deletes: usize,
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
    /// 稳定 id（**路径派生**：同一路径只有一条，可读、可 diff；不含时间）。
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

    // ------------------------------------------------ ★ M15/T8 同步维度
    //
    // 上面那些是**空间维度**（内容在不在本地），下面是**同步维度**（本地与远端
    // 一不一致）。两个维度**正交**：「仅在线 + 已同步」是完全正常的状态
    //（文件没 hydrated，但跟远端一模一样）。所以不能把它们压成一个枚举。
    /// ★ M15/T8：本地与远端是否一致（`baseline` 一致 且 无待传作业 且 无待裁决冲突）。
    ///
    /// 这是**派生值**，不是独立状态：`dirty` / `pending_upload` / `conflicts`
    /// 三个源任意一个变false 它就得跟着变。`None` = 拿不到节点（路径还没被
    /// `lookup` 过）→ 显示「未知」，不谎报「已同步」。
    #[serde(default)]
    pub in_sync: Option<bool>,
    /// ★ M15/T8：队列里还有这个路径的待传作业。
    #[serde(default)]
    pub pending_upload: bool,
    /// ★ M15/T8：传输进度 `(已传, 总量)`；`None` = 当前没有在传的作业。
    #[serde(default)]
    pub progress: Option<(u64, u64)>,
    /// ★ M15/T8：待裁决冲突数（`decisions` 表里 `resolution IS NULL` 的条数）。
    #[serde(default)]
    pub conflicts: usize,
    /// ★ M15/T8：调用方是否真的查到了冲突信息。
    ///
    /// `false` 时 `conflicts = 0` 的含义是「**没查到**」而不是「没有」——
    /// 必须能区分，否则界面会把「查不到」显示成「已同步」。
    #[serde(default)]
    pub conflicts_known: bool,
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

    // ------------------------------------------------ ★ M15/T8 同步维度汇总
    /// 已同步的条目数（`in_sync == Some(true)`）。
    #[serde(default)]
    pub in_sync: usize,
    /// 未同步的条目数（`in_sync == Some(false)`）。
    #[serde(default)]
    pub out_of_sync: usize,
    /// 状态未知的条目数（`in_sync == None`，路径没被 `lookup` 过）。
    #[serde(default)]
    pub unknown: usize,
    /// 当前有在传作业的条目数。
    #[serde(default)]
    pub transferring: usize,
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

/// ★ M23：`baloo_status` 的返回 —— Dolphin 集成开关的前置检测。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BalooStatusData {
    /// 找到的 `balooctl`（`balooctl6` 优先）；`None` = 系统没装 Baloo。
    pub balooctl: Option<String>,
    /// `~/.config/baloofilerc` 的路径（诊断用）。
    pub config_path: String,
    /// 配置文件是否存在（不存在 = Baloo 从没跑过，什么都不会被索引）。
    pub config_exists: bool,
    /// Baloo 全局索引开关（`Indexing-Enabled`；缺省视为开）。
    ///
    /// 关着时挂载点都「安全」，但 Dolphin 的 Tags 列也不显示 ——
    /// GUI 要把这一点如实告诉用户，别让人以为开关坏了。
    pub indexing_enabled: bool,
    /// 当前 Dolphin 集成开关（`settings.dolphin_tags`）。
    pub dolphin_tags: bool,
    /// ★ M24：当前 Dolphin **角标**开关（`settings.dolphin_overlay`）。
    pub dolphin_overlay: bool,
    /// ★ M25：角标插件（`qxync-dolphin-overlay`）的检测结果（`None` = 未安装）。
    ///
    /// 老版本 daemon **不发这个字段**（serde default → 反序列化成 `None`）；
    /// 客户端要区分「字段缺失（旧 daemon，未知）」与「值为 null（新 daemon，
    /// 未安装）」—— GUI 对 `undefined` 不显示这一行，JSON 客户端同理。
    pub overlay_plugin: Option<OverlayPluginInfo>,
    /// 每个挂载点的检测结论。
    pub mounts: Vec<BalooMountInfo>,
    /// `true` = 现在可以打开 Dolphin 集成（所有挂载点都不会被索引）。
    ///
    /// 没有挂载点时恒为 `true`（没有要保护的东西；之后挂载时 mount 会再查）。
    pub can_enable: bool,
}

/// ★ M25：`baloo_status` 里的角标插件（`qxync-dolphin-overlay`）检测结果。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OverlayPluginInfo {
    /// `libqxync_overlay.so` 的路径。
    pub path: String,
    /// Dolphin 真的会加载它（系统插件目录 / `QT_PLUGIN_PATH` 命中）。
    ///
    /// `false` = 装在 `~/.local` 但 `QT_PLUGIN_PATH` 没包含该目录 ——
    /// Dolphin 不会加载，角标不会显示。
    pub effective: bool,
}

/// ★ M23：单个挂载点的 Baloo 检测结论。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BalooMountInfo {
    pub mountpoint: String,
    /// 路径（或祖先）在 `exclude folders` 里。
    pub excluded: bool,
    /// **危险**：Baloo 开着且真的会索引这个挂载点（= 会把占位符拉下来）。
    pub indexed: bool,
    /// 该挂载点当前是否真的在暴露 `user.xdg.tags`
    /// （= 设置开了 **且** 这个挂载点安全；两个都满足才有 Tags）。
    pub tags_active: bool,
    /// ★ M24：该挂载点当前是否真的在暴露 `user.qxync.overlay`（角标）。
    pub overlay_active: bool,
    /// 建议执行的排除命令（`balooctl6 config add excludeFolders <挂载点>`）。
    pub exclude_cmd: String,
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
    /// `forget` 删掉的那一条（`clear` 时为 `None`）。
    pub removed_one: Option<DecisionInfo>,
    pub note: Option<String>,
}

/// ★ `action="diff"` 的返回：本地 vs NAS 的文本对比。
///
/// **要么给出 diff，要么明说为什么没给**（`status="skipped"` + 原因）——
/// 静默返回空 diff 会让用户以为「两边一样」，那是危险的假象。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DiffData {
    /// 被对比的远端路径。
    pub path: String,
    pub local_size: u64,
    pub remote_size: u64,
    /// [`crate::decisions::DiffOutcome`]：`{status:"diff",...}` 或 `{status:"skipped",...}`。
    pub outcome: crate::decisions::DiffOutcome,
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

/// ★ M26：读一行，是进度行就取出 [`ProgressData`]，是最终响应就返回 `None`。
///
/// 给 CLI 的进度感知读循环用（见 `qxync-cli/src/ipc_client.rs`）。解析失败
/// 原样上抛 —— 线上只该有两种形状，第三种就是协议坏了。
pub fn conn_line_from_line(line: &[u8]) -> Result<Option<ProgressData>, serde_json::Error> {
    let s = std::str::from_utf8(line).unwrap_or("");
    let parsed: ConnLine = serde_json::from_str(s.trim())?;
    Ok(match parsed {
        ConnLine::Progress(ProgressLine { progress, .. }) => Some(progress),
        ConnLine::Resp(_) => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ M22：`alive_known` 的跨版本兼容 —— 老 daemon 的 payload 没有这个键，
    /// 必须按 `false`（未知）解析，而不是反序列化失败或默认成「探测过」。
    #[test]
    fn session_info_parses_old_daemon_payload_without_alive_known() {
        // 老 daemon（0.6.x）的 status payload 形状
        let old = serde_json::json!({ "sid_masked": "sid-***", "alive": false });
        let si: SessionInfo = serde_json::from_value(old).unwrap();
        assert_eq!(si.sid_masked, "sid-***");
        assert!(!si.alive);
        assert!(!si.alive_known, "缺键 = 未知，绝不粉饰成「探测过」");

        // 新 daemon：离线跳过探测 → alive=false + alive_known=false（界面显示「未知」）
        let offline = serde_json::json!({
            "sid_masked": "sid-***", "alive": false, "alive_known": false
        });
        let si: SessionInfo = serde_json::from_value(offline).unwrap();
        assert!(!si.alive && !si.alive_known);

        // 在线真探测：alive=false + alive_known=true → 界面才能说「失活」
        let dead = serde_json::json!({
            "sid_masked": "sid-***", "alive": false, "alive_known": true
        });
        let si: SessionInfo = serde_json::from_value(dead).unwrap();
        assert!(!si.alive && si.alive_known);

        // 往返无损
        let rt = SessionInfo {
            sid_masked: "sid-***".into(),
            alive: true,
            alive_known: true,
        };
        let back: SessionInfo = serde_json::from_value(serde_json::to_value(&rt).unwrap()).unwrap();
        assert_eq!(back.sid_masked, rt.sid_masked);
        assert_eq!(back.alive, rt.alive);
        assert_eq!(back.alive_known, rt.alive_known);
    }

    // ------------------------------------------------------------ ★ M16 多连接路径

    #[test]
    fn m16_default_endpoint_paths_are_unchanged() {
        assert_eq!(socket_path_for("default"), default_socket_path());
        assert_eq!(pid_path_for("default"), default_pid_path());
        assert_eq!(log_prefix_for("default"), "qxyncd.log");
    }

    #[test]
    fn m16_non_default_endpoints_are_siblings_and_unique() {
        let d = socket_path_for("default");
        let s2 = socket_path_for("nas2");
        let s3 = socket_path_for("nas3");
        assert_eq!(s2.parent(), d.parent(), "必须落在同一个运行目录");
        assert_eq!(
            s2.file_name().and_then(|x| x.to_str()),
            Some("qxyncd-nas2.sock")
        );
        assert_ne!(s2, s3, "不同连接不能共用一个 socket");
        assert_eq!(
            pid_path_for("nas2").file_name().and_then(|x| x.to_str()),
            Some("qxyncd-nas2.pid")
        );
        assert_eq!(log_prefix_for("nas2"), "qxyncd-nas2.log");
        // 非法字符被 sanitize 成安全文件名（防路径穿越）
        assert!(socket_path_for("../x")
            .file_name()
            .and_then(|x| x.to_str())
            .is_some_and(|n| n.contains("qxyncd-") && !n.contains('/')));
    }

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

    /// ★ M26：进度行的线格式与两种兼容场景。
    #[test]
    fn progress_lines_and_envelope_flag() {
        // 1) 进度行 ↔ ConnLine 往返（total 有/无两种形态）
        let p = ProgressLine {
            v: IPC_VERSION,
            progress: ProgressData {
                written: 1024,
                total: Some(4096),
            },
        };
        let line = encode_line(&p).unwrap();
        let back: ConnLine = decode_line(&line).unwrap();
        assert_eq!(back, ConnLine::Progress(p));

        let p2 = ProgressLine {
            v: IPC_VERSION,
            progress: ProgressData {
                written: 7,
                total: None,
            },
        };
        let line2 = encode_line(&p2).unwrap();
        // total 缺失时线格式里就不该有这个字段（GUI/脚本按 key 探测）
        assert!(!String::from_utf8(line2.clone()).unwrap().contains("total"));
        let back2: ConnLine = decode_line(&line2).unwrap();
        assert_eq!(back2, ConnLine::Progress(p2));

        // 2) 最终响应也能从 ConnLine 解出（同一条 socket 上混发不歧义）
        let r = Response::ok(serde_json::json!({ "bytes": 1 }));
        let back3: ConnLine = decode_line(&encode_line(&r).unwrap()).unwrap();
        assert_eq!(back3, ConnLine::Resp(r));

        // 3) 老客户端的线格式（没有 progress 字段）→ 缺省 false
        let old: RequestEnvelope =
            serde_json::from_str(r#"{"v":1,"method":"ls","path":"/home"}"#).unwrap();
        assert!(!old.progress, "老线格式必须解出 progress=false");

        // 4) 新客户端声明进度 → true，且不影响请求参数本身
        let e = RequestEnvelope::with_progress(Request::Get {
            dir: "/home".into(),
            name: "a.bin".into(),
            dest: "/tmp/a.bin".into(),
        });
        assert!(e.progress);
        let back4: RequestEnvelope = decode_line(&encode_line(&e).unwrap()).unwrap();
        assert!(back4.progress);
        assert_eq!(back4.req, e.req);
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
        // ★ `insecure` 是后加的字段：老 payload 没有这个键 → 默认 false，不许解析失败
        assert!(
            !li.insecure,
            "老 daemon 的 LinkInfo 没有 insecure → 默认 false"
        );
        // 新 payload 带 insecure:true 必须能解析出来（GUI 的 same_link 靠它）
        let li_new: LinkInfo = serde_json::from_str(
            r#"{"id":"default","host":"nas","port":9834,"https":true,"insecure":true,
                "user":"u","ipv4_only":false}"#,
        )
        .unwrap();
        assert!(li_new.insecure);
        // 序列化必须带上这个键（daemon → GUI 的契约；字段名定死）
        let v = serde_json::to_value(&li_new).unwrap();
        assert_eq!(v["insecure"], serde_json::json!(true), "{v}");
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

    /// ★ `decisions diff` 要 stat + 读本地缓存 + 从 NAS 拉最多 512 KiB 再跑 LCS，
    /// 比其它 action 慢：只有它给 600s 超时。
    ///
    /// CLI 与 GUI 的超时都取这张表（`qxync-cli/src/ipc_client.rs` 与
    /// `qxync-gui/src/ipc.rs` 都调 `is_long_running`），所以判据只该在这里定义一次。
    #[test]
    fn decisions_diff_is_long_running_other_actions_are_not() {
        let diff = Request::Decisions {
            action: "diff".into(),
            id: Some("home_a.txt".into()),
            resolution: None,
        };
        assert!(diff.is_long_running(), "diff 要拉 NAS 内容，属于长任务");
        for action in ["list", "resolve", "forget", "clear"] {
            let req = Request::Decisions {
                action: action.into(),
                id: None,
                resolution: None,
            };
            assert_eq!(req.method(), "decisions");
            assert!(
                !req.is_long_running(),
                "{action} 是轻操作，不该给 600s 超时"
            );
        }
    }

    #[test]
    fn unknown_method_is_a_parse_error_not_a_panic() {
        let bad = br#"{"v":1,"method":"nope"}"#;
        assert!(decode_line::<RequestEnvelope>(bad).is_err());
    }

    // ================================================================ ★ M15/T8

    /// ★ T8：`file_states` 的 JSON 字段名**固定**（验收脚本按名字断言）。
    ///
    /// 为什么要专门钉住：这些字段是 CLI `--json` 与 GUI 的契约，改名/漏字段
    /// 不会让任何编译失败，只会让脚本静默读到 `null`。
    #[test]
    fn t8_file_states_json_field_names_are_stable() {
        let d = FileStatesData {
            path: "/home".into(),
            mountpoint: Some("/mnt/q".into()),
            root: Some("/home".into()),
            entries: vec![FileStateInfo {
                name: "a.bin".into(),
                remote: "/home/a.bin".into(),
                is_dir: false,
                size: 1000,
                hydrated_bytes: 400,
                state: "local".into(),
                pin: "unspecified".into(),
                dirty: true,
                hidden: false,
                in_sync: Some(false),
                pending_upload: true,
                progress: Some((400, 1000)),
                conflicts: 2,
                conflicts_known: true,
            }],
            online: 1,
            local: 2,
            always: 3,
            note: None,
            in_sync: 4,
            out_of_sync: 5,
            unknown: 6,
            transferring: 7,
        };
        let v = serde_json::to_value(&d).unwrap();
        // 汇总字段（空间维度 + 同步维度）
        for k in [
            "path",
            "mountpoint",
            "root",
            "entries",
            "online",
            "local",
            "always",
            "note",
            "in_sync",
            "out_of_sync",
            "unknown",
            "transferring",
        ] {
            assert!(v.get(k).is_some(), "汇总缺字段 {k}");
        }
        // 单条字段
        let e = v.get("entries").unwrap().as_array().unwrap()[0].clone();
        for k in [
            "name",
            "remote",
            "is_dir",
            "size",
            "hydrated_bytes",
            "state",
            "pin",
            "dirty",
            "hidden",
            "in_sync",
            "pending_upload",
            "progress",
            "conflicts",
            "conflicts_known",
        ] {
            assert!(e.get(k).is_some(), "条目缺字段 {k}");
        }
        // progress 必须是 `[done, total]` 两个数（GUI 直接按下标读）
        let p = e.get("progress").unwrap().as_array().unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].as_u64(), Some(400));
        assert_eq!(p[1].as_u64(), Some(1000));
    }

    /// ★ T8：老 daemon / 老 GUI 的 JSON 缺 T8 字段时必须还能解析（`serde(default)`）。
    #[test]
    fn t8_file_states_json_is_backward_compatible() {
        // 这是 T8 之前的形状（只有空间维度）
        let old = r#"{
            "path": "/home",
            "mountpoint": "/mnt/q",
            "root": "/home",
            "entries": [{"name":"a.bin","remote":"/home/a.bin","is_dir":false,
                         "size":10,"hydrated_bytes":0,"state":"online",
                         "pin":"unspecified","dirty":false,"hidden":false}],
            "online": 1, "local": 0, "always": 0
        }"#;
        let d: FileStatesData = serde_json::from_str(old).expect("老 payload 必须能解析");
        assert_eq!(d.entries.len(), 1);
        let e = &d.entries[0];
        // 缺失的 T8 字段取默认值，且 `in_sync = None`（= 未知，不是「已同步」）
        assert_eq!(e.in_sync, None);
        assert!(!e.pending_upload);
        assert_eq!(e.progress, None);
        assert_eq!(e.conflicts, 0);
        assert!(!e.conflicts_known, "查不到就不能冒充「没有冲突」");
        assert_eq!(d.in_sync, 0);
        assert_eq!(d.out_of_sync, 0);
        assert_eq!(d.unknown, 0);
        assert_eq!(d.transferring, 0);
    }

    /// ★ T8：`status.transfers` 缺字段时为 `None`（老 daemon），不报错。
    #[test]
    fn t8_status_transfers_is_optional() {
        let old = r#"{
            "daemon":{"version":"0.4.3","pid":1,"uptime_secs":1,"socket":"/tmp/s"},
            "logged_in": false,
            "hydro":{"count":0,"bytes":0},
            "mounts": []
        }"#;
        let st: StatusData = serde_json::from_str(old).expect("老 status 必须能解析");
        assert!(st.transfers.is_none(), "没有在途作业时是 None");
        // 新形状：字段齐全
        let t = TransferInfo {
            active: 2,
            downloading: 1,
            uploading: 1,
            done_bytes: 300,
            total_bytes: 1000,
        };
        let v = serde_json::to_value(StatusData {
            daemon: DaemonInfo {
                version: "0.4.3".into(),
                pid: 1,
                uptime_secs: 1,
                socket: "/tmp/s".into(),
            },
            link: None,
            logged_in: false,
            session: None,
            server: None,
            cursors: None,
            hydro: HydroStats::default(),
            uploads: None,
            deletes: None,
            sync: None,
            cache: None,
            transfers: Some(t),
            connectivity: None,
            offline: None,
            mounts: vec![],
        })
        .unwrap();
        let tv = v.get("transfers").unwrap();
        for k in [
            "active",
            "downloading",
            "uploading",
            "done_bytes",
            "total_bytes",
        ] {
            assert!(tv.get(k).is_some(), "transfers 缺字段 {k}");
        }
    }

    // ------------------------------------------------ ★ 未提交改动：diff / forget 的 IPC 契约

    /// `DecisionsData.removed_one` 是新增字段：老 daemon 的 payload（没这个键）必须能解析。
    #[test]
    fn decisions_data_removed_one_is_backward_compatible() {
        let old = r#"{
            "action":"list",
            "decisions":[{"id":"home_a.txt","path":"/home/a.txt","task_id":"",
                          "local_size":1,"local_mtime":2,"remote_size":3,"remote_mtime":4,
                          "is_dir":false,"created_unix":5,"resolution":null}],
            "pending":1,"resolved":0,"removed":0,"note":null
        }"#;
        let d: DecisionsData = serde_json::from_str(old).expect("老 payload 必须能解析");
        assert!(
            d.removed_one.is_none(),
            "没走 forget 时是 None，不是解析失败"
        );
        assert_eq!(d.decisions.len(), 1);
        assert_eq!(d.decisions[0].path, "/home/a.txt");

        // forget 的返回：removed_one 要能经信封往返（CLI 靠它显示「已忽略 <路径>」）
        let forgot = DecisionsData {
            action: "forget".into(),
            decisions: vec![],
            pending: 0,
            resolved: 0,
            removed: 0,
            removed_one: Some(DecisionInfo {
                id: "home_a.txt".into(),
                path: "/home/a.txt".into(),
                task_id: String::new(),
                local_size: 1,
                local_mtime: 2,
                remote_size: 3,
                remote_mtime: 4,
                is_dir: false,
                created_unix: 5,
                resolution: None,
            }),
            note: None,
        };
        let v = serde_json::to_value(&forgot).unwrap();
        assert_eq!(v["removed_one"]["path"], "/home/a.txt");
        let wire = encode_line(&Response::ok(forgot)).unwrap();
        let back: DecisionsData = decode_line::<Response>(&wire)
            .unwrap()
            .into_result()
            .unwrap();
        assert_eq!(
            back.removed_one.expect("removed_one 丢了").path,
            "/home/a.txt"
        );
    }

    /// `DiffData` 必须能经 IPC 信封往返（daemon `to_value` → CLI 解包）。
    /// 两种 outcome 的 JSON 形状一起钉住 —— 这个形状同时是 GUI 的契约。
    #[test]
    fn diff_data_round_trips_through_ipc_envelope() {
        let d = DiffData {
            path: "/home/a.txt".into(),
            local_size: 3,
            remote_size: 3,
            outcome: crate::decisions::DiffOutcome::Diff(crate::decisions::text_diff(
                "a\nb\n", "a\nx\n",
            )),
        };
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["path"], "/home/a.txt");
        let out = &v["outcome"];
        assert_eq!(out["status"], "diff");
        assert_eq!(out["added"], 1);
        assert_eq!(out["removed"], 1);
        // 行字段名/类型是 CLI/GUI 的契约（kind 必须是单字符字符串）
        let line = out["lines"].as_array().unwrap().first().unwrap();
        for k in ["kind", "local_no", "remote_no", "text"] {
            assert!(line.get(k).is_some(), "diff 行缺字段 {k}");
        }
        assert_eq!(line["kind"].as_str().map(str::len), Some(1));
        // daemon → Response 信封 → CLI
        let wire = encode_line(&Response::ok(d)).unwrap();
        let got: DiffData = decode_line::<Response>(&wire)
            .unwrap()
            .into_result()
            .unwrap();
        assert_eq!(got.path, "/home/a.txt");
        match got.outcome {
            crate::decisions::DiffOutcome::Diff(r) => assert_eq!((r.added, r.removed), (1, 1)),
            other => panic!("应解成 Diff，实际 {other:?}"),
        }

        // 跳过分支：必须带 status=skipped + reason（界面要显示「为什么没比」）
        let skipped = DiffData {
            path: "/home/big.bin".into(),
            local_size: 0,
            remote_size: u64::MAX,
            outcome: crate::decisions::DiffOutcome::Skipped(crate::decisions::DiffSkip::TooLarge {
                size: 5,
                limit: 4,
            }),
        };
        let v = serde_json::to_value(&skipped).unwrap();
        assert_eq!(v["outcome"]["status"], "skipped");
        assert_eq!(v["outcome"]["reason"], "too_large");
        assert_eq!(v["outcome"]["size"], 5);
        assert_eq!(v["outcome"]["limit"], 4);
        let wire = encode_line(&Response::ok(skipped)).unwrap();
        let got: DiffData = decode_line::<Response>(&wire)
            .unwrap()
            .into_result()
            .unwrap();
        assert!(matches!(
            got.outcome,
            crate::decisions::DiffOutcome::Skipped(crate::decisions::DiffSkip::TooLarge { .. })
        ));

        // 其余跳过原因的形状（GUI 按 `reason` 分支渲染，名字写错就会落到「未知原因」）
        for (skip, reason) in [
            (crate::decisions::DiffSkip::Binary, "binary"),
            (crate::decisions::DiffSkip::IsDir, "is_dir"),
            (
                crate::decisions::DiffSkip::NoContent {
                    which: "local".into(),
                    detail: "没有缓存".into(),
                },
                "no_content",
            ),
        ] {
            let v = serde_json::to_value(DiffData {
                path: "/home/x".into(),
                local_size: 1,
                remote_size: 2,
                outcome: crate::decisions::DiffOutcome::Skipped(skip),
            })
            .unwrap();
            assert_eq!(v["outcome"]["status"], "skipped");
            assert_eq!(v["outcome"]["reason"], reason);
        }
        // `no_content` 的 side 用哪个字段名也是契约（GUI 读 which/detail）
        let v = serde_json::to_value(DiffData {
            path: "/home/x".into(),
            local_size: 1,
            remote_size: 2,
            outcome: crate::decisions::DiffOutcome::Skipped(
                crate::decisions::DiffSkip::NoContent {
                    which: "remote".into(),
                    detail: "NAS 上的文件已不存在".into(),
                },
            ),
        })
        .unwrap();
        assert_eq!(v["outcome"]["which"], "remote");
        assert_eq!(v["outcome"]["detail"], "NAS 上的文件已不存在");
    }

    /// `DiffData` 缺字段时**不能默认成「零差异」**：默认必须落在 skipped 一侧，
    /// 否则界面会显示「两边一样」，而真相是根本没比。
    #[test]
    fn diff_data_default_is_skipped_not_empty_diff() {
        let d: DiffData = serde_json::from_str("{}").expect("空 payload 也要能解析");
        assert!(d.path.is_empty() && d.local_size == 0 && d.remote_size == 0);
        match d.outcome {
            crate::decisions::DiffOutcome::Skipped(crate::decisions::DiffSkip::NoContent {
                which,
                ..
            }) => assert_eq!(which, "unknown"),
            other => panic!("默认必须是「跳过且原因未知」，实际 {other:?}"),
        }
        let v = serde_json::to_value(DiffData::default()).unwrap();
        assert_eq!(v["outcome"]["status"], "skipped");
    }

    /// `Decisions` 请求的每种 action 都能经信封往返（新增 diff/forget **不用** bump 版本）。
    #[test]
    fn decisions_request_actions_round_trip() {
        for (action, id) in [
            ("list", None),
            ("clear", None),
            ("diff", Some("home_a.txt")),
            ("forget", Some("home_a.txt")),
            ("resolve", Some("home_a.txt")),
        ] {
            let e = RequestEnvelope::new(Request::Decisions {
                action: action.into(),
                id: id.map(str::to_string),
                resolution: None,
            });
            assert_eq!(e.req.method(), "decisions");
            assert_eq!(e.v, IPC_VERSION);
            let back: RequestEnvelope = decode_line(&encode_line(&e).unwrap()).unwrap();
            match back.req {
                Request::Decisions {
                    action: a, id: i, ..
                } => {
                    assert_eq!(a, action);
                    assert_eq!(i.as_deref(), id);
                }
                other => panic!("解成了 {other:?}"),
            }
        }
    }
}
