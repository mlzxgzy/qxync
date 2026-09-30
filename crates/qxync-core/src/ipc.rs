//! # 本地 IPC 契约（daemon ↔ CLI/GUI）
//!
//! 传输：unix domain socket，**一行一个 JSON**（见 `docs/M1.5-设计.md` §1）。
//! 这里只放**类型与路径规则**，不做任何 IO，方便 CLI/daemon/测试三方共用。

use crate::config::ConfigPaths;
use crate::model::DirEntry;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 协议版本：请求/响应都带，用于向前兼容。
pub const IPC_VERSION: u32 = 1;

// ---------------------------------------------------------------- 路径规则

/// 默认 socket 路径：`$XDG_RUNTIME_DIR/qxync/qxyncd.sock`，退到 state 目录。
pub fn default_socket_path() -> PathBuf {
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        if !rt.is_empty() {
            return PathBuf::from(rt).join("qsync/qxyncd.sock");
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
    },
    Umount {
        mountpoint: PathBuf,
    },
    Mounts,
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
            Request::Shutdown => "shutdown",
        }
    }

    /// 长任务（下载/上传/挂载）给 CLI 的默认超时更长。
    pub fn is_long_running(&self) -> bool {
        matches!(
            self,
            Request::Get { .. } | Request::Put { .. } | Request::Mount { .. }
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountInfo {
    pub mountpoint: PathBuf,
    pub remote: String,
    pub readonly: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusData {
    pub daemon: DaemonInfo,
    pub link: LinkInfo,
    pub logged_in: bool,
    pub session: Option<SessionInfo>,
    pub server: Option<ServerInfo>,
    pub cursors: Option<CursorInfo>,
    pub hydro: HydroStats,
    pub mounts: Vec<MountInfo>,
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
        });
        let line = encode_line(&e).unwrap();
        let back: RequestEnvelope = decode_line(&line).unwrap();
        match back.req {
            Request::Mount {
                threads,
                hydrate_timeout_secs,
                remote,
                ..
            } => {
                assert_eq!(threads, Some(4));
                assert_eq!(hydrate_timeout_secs, Some(600));
                assert_eq!(remote.as_deref(), Some("/home"));
            }
            other => panic!("解成了 {other:?}"),
        }
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
