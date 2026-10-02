//! ★ M7：LAN 对等协议（qxync ↔ qxync 的设备配对 / 事件快路径 / 直传）。
//!
//! 设计见 `docs/M7-选择性同步与LAN直连.md` §2。**不是**官方 Qsync 的
//! WebSocket 二进制通道（`Auth1`/`Auth2`/`LANDownloadFile`，线格式未还原），
//! 是自家协议：TCP + 行 JSON 头 + 裸字节体，跟本机 IPC（M1.5）同一套风格，
//! 便于用 `nc` 手工验证。
//!
//! 硬约束（文档 §0）：
//!
//! * **LAN 只是快路径**：任何失败都必须能回落 NAS —— 所以客户端 API 都返回 `Result`，
//!   调用方（FUSE 水合）把错误当「这台 peer 不行」继续试下一个，最后走 NAS。
//! * **只服务完整水合且未脏的文件**：部分水合的稀疏文件里那些 0 不是数据（铁则 1 的 LAN 版）。
//! * **只读能力**：token 只授权「读已水合文件 + 提交事件」，没有任何写/删远端的能力。
//! * 路径必须落在 `roots` 之内（`..`、空路径、越界一律拒绝）。

pub use qxync_core::config::PeerConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// 协议版本（不匹配直接拒绝）。
pub const PEER_VERSION: u32 = 1;
/// 单次 `get` 的字节上限（8 MiB）——超出请分片。
pub const MAX_GET_LEN: u64 = 8 * 1024 * 1024;
/// 单行头长度上限（防粘包/超长行打爆内存）。
pub const MAX_LINE: usize = 64 * 1024;
/// `head` 超时（对端没在 1 秒内答就换下一个）。
pub const HEAD_TIMEOUT: Duration = Duration::from_millis(1000);
/// `get` 超时（128 KiB 区间在局域网上应该是毫秒级）。
pub const GET_TIMEOUT: Duration = Duration::from_secs(5);
/// `ping`/`pair` 超时。
pub const PAIR_TIMEOUT: Duration = Duration::from_secs(3);
/// 配对尝试窗口与上限（防爆破；超了直接拒绝，不告诉对方码对不对）。
pub const PAIR_WINDOW: Duration = Duration::from_secs(60);
pub const PAIR_MAX_ATTEMPTS: usize = 10;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------- 随机数（无 rand 依赖）

/// 32 位十六进制随机串；优先 `/dev/urandom`，不可用时退化为「时间+pid+计数器」混合
/// （只影响不可预测性，功能不受影响；测试环境够用）。
pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
        .is_err()
    {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seed = now_secs()
            ^ (std::process::id() as u64) << 32
            ^ COUNTER.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed);
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (seed.rotate_left((i as u32 % 64) as u32) & 0xff) as u8;
            *b ^= (i as u8).wrapping_mul(31);
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// 6 位数字配对码（配对成功即轮换）。
pub fn random_pairing_code() -> String {
    let hex = random_hex(4);
    let n = u32::from_str_radix(&hex, 16).unwrap_or(0) % 1_000_000;
    format!("{n:06}")
}

/// 常量时间比较（token 校验；避免用 == 泄露前缀信息）。
pub fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

// ---------------------------------------------------------------- 线协议

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerIdentity {
    pub name: String,
    pub version: String,
    /// 本机对等监听地址（`host:port`；没开监听 = None）。配对时对方用它来主动连我们。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<String>,
    #[serde(default)]
    pub roots: Vec<String>,
    /// 当前是否接受配对（true = 有有效配对码）。
    #[serde(default)]
    pub pairing_open: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerEvent {
    /// 远端绝对路径。
    pub path: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub mtime: i64,
    /// `modified` / `created` / `deleted`（目前只是提示，对账不依赖它）。
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub ts: u64,
}

impl PeerEvent {
    pub fn new(path: impl Into<String>, size: u64, mtime: i64, kind: &str) -> Self {
        Self {
            path: path.into(),
            size,
            mtime,
            kind: kind.to_string(),
            ts: now_secs(),
        }
    }
}

/// 请求（`{"v":1,"op":"head","token":"…","path":"…"}`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerRequest {
    #[serde(default = "default_version")]
    pub v: u32,
    pub op: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `hello`：本机的对等监听地址。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roots: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default)]
    pub offset: u64,
    #[serde(default)]
    pub len: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<PeerEvent>,
}

fn default_version() -> u32 {
    PEER_VERSION
}

impl PeerRequest {
    fn new(op: &str) -> Self {
        Self {
            v: PEER_VERSION,
            op: op.to_string(),
            token: None,
            code: None,
            name: None,
            addr: None,
            roots: Vec::new(),
            path: None,
            offset: 0,
            len: 0,
            event: None,
        }
    }
    pub fn ping() -> Self {
        Self::new("ping")
    }
    pub fn pair(code: &str, name: &str, roots: Vec<String>) -> Self {
        let mut r = Self::new("pair");
        r.code = Some(code.to_string());
        r.name = Some(name.to_string());
        r.roots = roots;
        r
    }
    pub fn head(path: &str) -> Self {
        let mut r = Self::new("head");
        r.path = Some(path.to_string());
        r
    }
    pub fn get(path: &str, offset: u64, len: u64) -> Self {
        let mut r = Self::new("get");
        r.path = Some(path.to_string());
        r.offset = offset;
        r.len = len;
        r
    }
    /// 配对后的「我是谁 / 怎么连我 / 用哪个 token」登记（双向信任的关键一步）。
    pub fn hello(name: &str, addr: &str, roots: Vec<String>) -> Self {
        let mut r = Self::new("hello");
        r.name = Some(name.to_string());
        r.addr = Some(addr.to_string());
        r.roots = roots;
        r
    }
    pub fn event(ev: PeerEvent) -> Self {
        let mut r = Self::new("event");
        r.event = Some(ev);
        r
    }
    fn with_token(mut self, token: Option<&str>) -> Self {
        self.token = token.map(|s| s.to_string());
        self
    }
}

/// 回复（`get` 成功时，这一行之后跟 `len` 字节裸数据）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerReply {
    #[serde(default = "default_version")]
    pub v: u32,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<PeerIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exists: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hydrated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub len: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted: Option<bool>,
}

impl PeerReply {
    pub fn ok() -> Self {
        Self {
            v: PEER_VERSION,
            ok: true,
            error: None,
            token: None,
            peer: None,
            exists: None,
            hydrated: None,
            size: None,
            mtime: None,
            len: None,
            accepted: None,
        }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        let mut r = Self::ok();
        r.ok = false;
        r.error = Some(msg.into());
        r
    }
}

/// `head` 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerHead {
    pub exists: bool,
    pub hydrated: bool,
    pub size: u64,
    pub mtime: i64,
}

// ---------------------------------------------------------------- 内容源

/// 对等端能提供的内容（daemon 用挂载视图实现；测试用 [`DirContent`]）。
pub trait ContentSource: Send + Sync {
    /// 远端路径 → 元数据；`None` = 没有 / 不敢给（未完整水合、脏、路径越界）。
    fn head(&self, path: &str) -> Option<PeerHead>;
    /// 精确读一段；长度不足必须报错（铁则 1：绝不短读）。
    fn read_at(&self, path: &str, offset: u64, len: usize) -> io::Result<Vec<u8>>;
    /// 提供的远端根（用于对外声明 / 路径校验）。
    fn roots(&self) -> Vec<String>;
}

/// 「远端根 → 本地目录」的简单内容源（loopback 验收 / 无挂载场景）。
#[derive(Debug, Clone, Default)]
pub struct DirContent {
    pub map: Vec<(String, PathBuf)>,
}

impl DirContent {
    pub fn new(map: Vec<(String, PathBuf)>) -> Self {
        Self { map }
    }

    fn local_of(&self, remote: &str) -> Option<PathBuf> {
        let mut best: Option<(&str, &PathBuf)> = None;
        for (root, dir) in &self.map {
            let root = root.trim_end_matches('/');
            if remote == root || remote.starts_with(&format!("{root}/")) {
                if best.map(|(r, _)| root.len() > r.len()).unwrap_or(true) {
                    best = Some((root, dir));
                }
            }
        }
        let (root, dir) = best?;
        let rel = remote.trim_start_matches(root).trim_start_matches('/');
        Some(dir.join(rel))
    }
}

impl ContentSource for DirContent {
    fn head(&self, path: &str) -> Option<PeerHead> {
        let p = self.local_of(path)?;
        let md = std::fs::metadata(&p).ok()?;
        if !md.is_file() {
            return None;
        }
        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Some(PeerHead {
            exists: true,
            hydrated: true,
            size: md.len(),
            mtime,
        })
    }

    fn read_at(&self, path: &str, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let p = self
            .local_of(path)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "路径不在提供的根内"))?;
        use std::os::unix::fs::FileExt;
        let f = std::fs::File::open(&p)?;
        let mut buf = vec![0u8; len];
        f.read_exact_at(&mut buf, offset)?;
        Ok(buf)
    }

    fn roots(&self) -> Vec<String> {
        self.map.iter().map(|(r, _)| r.clone()).collect()
    }
}

// ---------------------------------------------------------------- 服务端

#[derive(Debug, Default)]
pub struct PeerStats {
    pub connections: AtomicU64,
    pub pairs: AtomicU64,
    pub heads: AtomicU64,
    pub gets: AtomicU64,
    pub bytes_served: AtomicU64,
    pub events: AtomicU64,
    pub rejected: AtomicU64,
}

/// 对端用 `hello` 登记自己：daemon 收到后落盘（双向信任）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerRegistration {
    pub name: String,
    pub addr: String,
    pub roots: Vec<String>,
    /// 双方共用的配对 token（这一对设备之间的秘密）。
    pub token: String,
}

/// 服务端上下文：身份 + 已配对 token + 配对码 + 内容源 + 事件出口。
pub struct PeerServer {
    pub identity: PeerIdentity,
    /// 对端 name → token。
    tokens: StdMutex<HashMap<String, String>>,
    pairing_code: StdMutex<Option<String>>,
    attempts: StdMutex<Vec<Instant>>,
    source: Arc<dyn ContentSource>,
    events: tokio::sync::mpsc::UnboundedSender<PeerEvent>,
    /// 对端 `hello` 登记（daemon 落盘 + 更新 peers 列表）。
    registers: tokio::sync::mpsc::UnboundedSender<PeerRegistration>,
    pub stats: Arc<PeerStats>,
    /// 提供服务的内容源（诊断用）。
    pub mount_roots: Vec<String>,
}

impl PeerServer {
    pub fn new(
        name: impl Into<String>,
        version: impl Into<String>,
        addr: Option<String>,
        roots: Vec<String>,
        source: Arc<dyn ContentSource>,
        events: tokio::sync::mpsc::UnboundedSender<PeerEvent>,
        registers: tokio::sync::mpsc::UnboundedSender<PeerRegistration>,
    ) -> Self {
        Self {
            identity: PeerIdentity {
                name: name.into(),
                version: version.into(),
                addr,
                roots: source
                    .roots()
                    .into_iter()
                    .chain(roots)
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect(),
                pairing_open: true,
            },
            tokens: StdMutex::new(HashMap::new()),
            pairing_code: StdMutex::new(Some(random_pairing_code())),
            attempts: StdMutex::new(Vec::new()),
            source,
            events,
            registers,
            stats: Arc::new(PeerStats::default()),
            mount_roots: Vec::new(),
        }
    }

    /// 预置一个 token（重启后从 `peers.json` 恢复，避免重新配对）。
    pub fn add_token(&self, name: impl Into<String>, token: impl Into<String>) {
        self.tokens
            .lock()
            .unwrap()
            .insert(name.into(), token.into());
    }

    pub fn token_for(&self, name: &str) -> Option<String> {
        self.tokens.lock().unwrap().get(name).cloned()
    }

    pub fn pairing_code(&self) -> Option<String> {
        self.pairing_code.lock().unwrap().clone()
    }

    /// 手动设置/关闭配对码（`None` = 关闭配对）。
    pub fn set_pairing_code(&self, code: Option<String>) {
        *self.pairing_code.lock().unwrap() = code;
    }

    /// 轮换配对码（配对成功后调用）。
    pub fn rotate_pairing_code(&self) -> Option<String> {
        let mut g = self.pairing_code.lock().unwrap();
        *g = g.as_ref().map(|_| random_pairing_code());
        g.clone()
    }

    fn check_pairing(&self, code: &str) -> bool {
        {
            let mut att = self.attempts.lock().unwrap();
            let now = Instant::now();
            att.retain(|t| now.duration_since(*t) < PAIR_WINDOW);
            if att.len() >= PAIR_MAX_ATTEMPTS {
                return false;
            }
            att.push(now);
        }
        let cur = self.pairing_code.lock().unwrap().clone();
        match cur {
            Some(c) => ct_eq(&c, code),
            None => false,
        }
    }

    fn authed(&self, req: &PeerRequest) -> bool {
        let Some(tok) = req.token.as_deref() else {
            return false;
        };
        let g = self.tokens.lock().unwrap();
        // token 不绑定具体名字（配对时交换的就是它），任何一个匹配即通过。
        g.values().any(|t| ct_eq(t, tok))
    }

    /// 路径必须落在某个 root 之内（拒绝 `..` 与空路径）。
    fn path_allowed(&self, path: &str) -> bool {
        if path.is_empty() || !path.starts_with('/') {
            return false;
        }
        if path.split('/').any(|s| s == ".." || s == ".") {
            return false;
        }
        let roots = &self.identity.roots;
        if roots.is_empty() {
            return true;
        }
        roots.iter().any(|r| {
            let r = r.trim_end_matches('/');
            path == r || path.starts_with(&format!("{r}/"))
        })
    }

    /// 处理一条请求；`get` 成功时第二项是**裸数据体**（调用方必须先发头再发它）。
    pub fn handle(&self, req: &PeerRequest) -> (PeerReply, Option<Vec<u8>>) {
        if req.v != PEER_VERSION {
            return (PeerReply::err(format!("协议版本 {} 不受支持", req.v)), None);
        }
        match req.op.as_str() {
            "ping" => {
                let mut r = PeerReply::ok();
                r.peer = Some(PeerIdentity {
                    pairing_open: self.pairing_code().is_some(),
                    ..self.identity.clone()
                });
                (r, None)
            }
            "pair" => {
                let code = req.code.clone().unwrap_or_default();
                if !self.check_pairing(&code) {
                    self.stats.rejected.fetch_add(1, Ordering::Relaxed);
                    return (PeerReply::err("配对码无效或尝试过多"), None);
                }
                let name = req
                    .name
                    .clone()
                    .filter(|n| !n.trim().is_empty())
                    .unwrap_or_else(|| format!("peer-{}", &random_hex(2)));
                let token = random_hex(16);
                self.tokens
                    .lock()
                    .unwrap()
                    .insert(name.clone(), token.clone());
                self.rotate_pairing_code();
                self.stats.pairs.fetch_add(1, Ordering::Relaxed);
                let mut r = PeerReply::ok();
                r.token = Some(token);
                r.peer = Some(self.identity.clone());
                (r, None)
            }
            "event" => {
                if !self.authed(req) {
                    self.stats.rejected.fetch_add(1, Ordering::Relaxed);
                    return (PeerReply::err("token 无效"), None);
                }
                let Some(ev) = &req.event else {
                    return (PeerReply::err("缺少 event"), None);
                };
                if !self.path_allowed(&ev.path) {
                    self.stats.rejected.fetch_add(1, Ordering::Relaxed);
                    return (PeerReply::err("路径越权"), None);
                }
                let accepted = self.events.send(ev.clone()).is_ok();
                self.stats.events.fetch_add(1, Ordering::Relaxed);
                let mut r = PeerReply::ok();
                r.accepted = Some(accepted);
                (r, None)
            }
            "hello" => {
                if !self.authed(req) {
                    self.stats.rejected.fetch_add(1, Ordering::Relaxed);
                    return (PeerReply::err("token 无效"), None);
                }
                let name = req.name.clone().unwrap_or_default();
                let addr = req.addr.clone().unwrap_or_default();
                if name.trim().is_empty() || addr.trim().is_empty() {
                    return (PeerReply::err("hello 需要 name 与 addr"), None);
                }
                let token = req.token.clone().unwrap_or_default();
                let _ = self.registers.send(PeerRegistration {
                    name,
                    addr,
                    roots: req.roots.clone(),
                    token,
                });
                let mut r = PeerReply::ok();
                r.peer = Some(self.identity.clone());
                (r, None)
            }
            "head" => {
                if !self.authed(req) {
                    self.stats.rejected.fetch_add(1, Ordering::Relaxed);
                    return (PeerReply::err("token 无效"), None);
                }
                let path = req.path.clone().unwrap_or_default();
                if !self.path_allowed(&path) {
                    self.stats.rejected.fetch_add(1, Ordering::Relaxed);
                    return (PeerReply::err("路径越权"), None);
                }
                self.stats.heads.fetch_add(1, Ordering::Relaxed);
                let mut r = PeerReply::ok();
                match self.source.head(&path) {
                    Some(h) => {
                        r.exists = Some(h.exists);
                        r.hydrated = Some(h.hydrated);
                        r.size = Some(h.size);
                        r.mtime = Some(h.mtime);
                    }
                    None => {
                        r.exists = Some(false);
                        r.hydrated = Some(false);
                    }
                }
                (r, None)
            }
            "get" => {
                if !self.authed(req) {
                    self.stats.rejected.fetch_add(1, Ordering::Relaxed);
                    return (PeerReply::err("token 无效"), None);
                }
                let path = req.path.clone().unwrap_or_default();
                if !self.path_allowed(&path) {
                    self.stats.rejected.fetch_add(1, Ordering::Relaxed);
                    return (PeerReply::err("路径越权"), None);
                }
                if req.len == 0 || req.len > MAX_GET_LEN {
                    return (
                        PeerReply::err(format!("len 必须在 1..={MAX_GET_LEN} 之间")),
                        None,
                    );
                }
                match self.source.read_at(&path, req.offset, req.len as usize) {
                    Ok(data) if data.len() as u64 == req.len => {
                        self.stats.gets.fetch_add(1, Ordering::Relaxed);
                        self.stats
                            .bytes_served
                            .fetch_add(data.len() as u64, Ordering::Relaxed);
                        let mut r = PeerReply::ok();
                        r.len = Some(data.len() as u64);
                        (r, Some(data))
                    }
                    Ok(data) => (
                        PeerReply::err(format!("短读：要 {} 字节只拿到 {}", req.len, data.len())),
                        None,
                    ),
                    Err(e) => (PeerReply::err(format!("读失败: {e}")), None),
                }
            }
            other => (PeerReply::err(format!("未知 op {other}")), None),
        }
    }
}

/// 读一行（上限 [`MAX_LINE`]）。
async fn read_line_limited<R: AsyncBufReadExt + Unpin>(r: &mut R) -> io::Result<Option<String>> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        let n = r.read(&mut byte).await?;
        if n == 0 {
            return if buf.is_empty() {
                Ok(None)
            } else {
                Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
            };
        }
        if byte[0] == b'\n' {
            return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
        }
        buf.push(byte[0]);
        if buf.len() > MAX_LINE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "请求头过长"));
        }
    }
}

/// 处理一条 TCP 连接（`get` 的裸数据体由调用方在 [`PeerServer::handle`] 之后补发）。
pub async fn handle_conn(server: Arc<PeerServer>, stream: TcpStream) -> io::Result<()> {
    server.stats.connections.fetch_add(1, Ordering::Relaxed);
    stream.set_nodelay(true).ok();
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);
    loop {
        let line = match read_line_limited(&mut rd).await? {
            Some(l) if l.trim().is_empty() => continue,
            Some(l) => l,
            None => return Ok(()),
        };
        let req: PeerRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let mut body = serde_json::to_vec(&PeerReply::err(format!("请求解析失败: {e}")))
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                body.push(b'\n');
                wr.write_all(&body).await?;
                wr.flush().await?;
                continue;
            }
        };
        // `get` 成功 → 先发头，再发裸字节（顺序不能反）
        let (reply, payload) = server.handle(&req);
        let mut body = serde_json::to_vec(&reply)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        body.push(b'\n');
        wr.write_all(&body).await?;
        if let Some(data) = payload {
            wr.write_all(&data).await?;
        }
        wr.flush().await?;
    }
}

/// 接受循环（daemon 里 `tokio::spawn`）。
pub async fn serve(listener: tokio::net::TcpListener, server: Arc<PeerServer>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let s = server.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(s, stream).await {
                        tracing::debug!("peer 连接结束: {e}");
                    }
                });
            }
            Err(e) => {
                tracing::warn!("peer accept 失败: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

// ---------------------------------------------------------------- 客户端

/// 一台对端的客户端（每次请求一条短连接：LAN 上比维护连接池简单且没有状态）。
#[derive(Debug, Clone)]
pub struct PeerClient {
    pub addr: String,
    pub name: String,
    pub token: String,
}

impl From<&PeerConfig> for PeerClient {
    fn from(p: &PeerConfig) -> Self {
        Self {
            addr: p.addr.clone(),
            name: p.name.clone(),
            token: p.token.clone(),
        }
    }
}

async fn roundtrip(
    addr: &str,
    req: &PeerRequest,
    timeout: Duration,
) -> io::Result<(PeerReply, Option<Vec<u8>>, Duration)> {
    let started = Instant::now();
    let fut = async {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true).ok();
        let (rd, mut wr) = stream.into_split();
        let mut rd = BufReader::new(rd);
        let mut line =
            serde_json::to_vec(req).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        line.push(b'\n');
        wr.write_all(&line).await?;
        wr.flush().await?;
        let head = read_line_limited(&mut rd)
            .await?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "对端没有回复"))?;
        let reply: PeerReply = serde_json::from_str(&head).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("回复解析失败: {e}"))
        })?;
        let mut body = None;
        if reply.ok && req.op == "get" {
            let n = reply.len.unwrap_or(0) as usize;
            let mut buf = vec![0u8; n];
            rd.read_exact(&mut buf).await?;
            body = Some(buf);
        }
        Ok::<_, io::Error>((reply, body))
    };
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok((r, b))) => Ok((r, b, started.elapsed())),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "peer 超时")),
    }
}

impl PeerClient {
    pub fn new(addr: impl Into<String>, name: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            addr: addr.into(),
            name: name.into(),
            token: token.into(),
        }
    }

    /// 不需 token 的探活；返回对端身份。
    pub async fn ping(addr: &str) -> io::Result<PeerIdentity> {
        let (r, _, _) = roundtrip(addr, &PeerRequest::ping(), PAIR_TIMEOUT).await?;
        if !r.ok {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                r.error.unwrap_or_else(|| "ping 失败".into()),
            ));
        }
        r.peer
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "缺少身份信息"))
    }

    /// 用配对码换 token。
    pub async fn pair(
        addr: &str,
        code: &str,
        name: &str,
        roots: Vec<String>,
    ) -> io::Result<(String, PeerIdentity)> {
        let (r, _, _) =
            roundtrip(addr, &PeerRequest::pair(code, name, roots), PAIR_TIMEOUT).await?;
        if !r.ok {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                r.error.unwrap_or_else(|| "配对失败".into()),
            ));
        }
        let token = r
            .token
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "配对回复缺少 token"))?;
        let peer = r
            .peer
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "配对回复缺少身份"))?;
        Ok((token, peer))
    }

    /// 配对后的登记：告诉对方「我是谁 / 怎么连我 / 用哪个 token」。
    pub async fn hello(
        addr: &str,
        token: &str,
        my_name: &str,
        my_addr: &str,
        roots: Vec<String>,
    ) -> io::Result<PeerIdentity> {
        let (r, _, _) = roundtrip(
            addr,
            &PeerRequest::hello(my_name, my_addr, roots).with_token(Some(token)),
            PAIR_TIMEOUT,
        )
        .await?;
        if !r.ok {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                r.error.unwrap_or_else(|| "hello 失败".into()),
            ));
        }
        r.peer
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "hello 回复缺少身份"))
    }

    pub async fn head(&self, path: &str) -> io::Result<PeerHead> {
        let (r, _, _) = roundtrip(
            &self.addr,
            &PeerRequest::head(path).with_token(Some(&self.token)),
            HEAD_TIMEOUT,
        )
        .await?;
        if !r.ok {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                r.error.unwrap_or_else(|| "head 失败".into()),
            ));
        }
        Ok(PeerHead {
            exists: r.exists.unwrap_or(false),
            hydrated: r.hydrated.unwrap_or(false),
            size: r.size.unwrap_or(0),
            mtime: r.mtime.unwrap_or(0),
        })
    }

    /// 取一段字节；返回（数据, 对端耗时）。
    pub async fn get_range(
        &self,
        path: &str,
        offset: u64,
        len: u64,
    ) -> io::Result<(Vec<u8>, Duration)> {
        let (r, body, took) = roundtrip(
            &self.addr,
            &PeerRequest::get(path, offset, len).with_token(Some(&self.token)),
            GET_TIMEOUT,
        )
        .await?;
        if !r.ok {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                r.error.unwrap_or_else(|| "get 失败".into()),
            ));
        }
        let data =
            body.ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "get 回复缺少数据体"))?;
        Ok((data, took))
    }

    pub async fn send_event(&self, ev: PeerEvent) -> io::Result<bool> {
        let (r, _, _) = roundtrip(
            &self.addr,
            &PeerRequest::event(ev).with_token(Some(&self.token)),
            HEAD_TIMEOUT,
        )
        .await?;
        if !r.ok {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                r.error.unwrap_or_else(|| "event 失败".into()),
            ));
        }
        Ok(r.accepted.unwrap_or(false))
    }
}

/// LAN 直传的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanFetch {
    pub data: Vec<u8>,
    pub peer: String,
    pub took: Duration,
}

/// 逐个问对端要一段数据；**任何一步不满足就换下一个，全都不行返回 `None`**（调用方回落 NAS）。
///
/// 判据：对端 `head` 必须 `exists && hydrated`，且 `size`/`mtime` 与本地从 NAS 拿到的签名一致。
pub async fn fetch_range(
    peers: &[PeerConfig],
    path: &str,
    offset: u64,
    len: u64,
    expect_size: u64,
    expect_mtime: i64,
) -> Option<LanFetch> {
    for p in peers {
        let c = PeerClient::from(p);
        let head = match c.head(path).await {
            Ok(h) => h,
            Err(e) => {
                tracing::debug!("LAN 快路径: {} head 失败: {e}", p.name);
                continue;
            }
        };
        if !head.exists || !head.hydrated {
            continue;
        }
        if head.size != expect_size || head.mtime != expect_mtime {
            tracing::debug!(
                "LAN 快路径: {} 的 {} 元数据不一致（对端 {}/{} vs NAS {}/{}）",
                p.name,
                path,
                head.size,
                head.mtime,
                expect_size,
                expect_mtime
            );
            continue;
        }
        match c.get_range(path, offset, len).await {
            Ok((data, took)) if data.len() as u64 == len => {
                return Some(LanFetch {
                    data,
                    peer: p.name.clone(),
                    took,
                });
            }
            Ok((data, _)) => tracing::warn!(
                "LAN 快路径: {} 返回长度不符（期望 {len} 实得 {}）",
                p.name,
                data.len()
            ),
            Err(e) => tracing::debug!("LAN 快路径: {} get 失败: {e}", p.name),
        }
    }
    None
}

/// 把事件广播给所有对端；返回成功台数（失败只记日志，不影响主流程）。
pub async fn broadcast_event(peers: &[PeerConfig], ev: PeerEvent) -> usize {
    let mut ok = 0;
    for p in peers {
        let c = PeerClient::from(p);
        match c.send_event(ev.clone()).await {
            Ok(true) => ok += 1,
            Ok(false) => tracing::debug!("LAN 事件被 {} 拒收", p.name),
            Err(e) => tracing::debug!("LAN 事件发给 {} 失败: {e}", p.name),
        }
    }
    ok
}

/// 探测一个地址是不是 qxync 对端（用于 `qsync peer ping <addr>`）。
pub async fn probe(addr: &str) -> io::Result<(PeerIdentity, Duration)> {
    let started = Instant::now();
    let id = PeerClient::ping(addr).await?;
    Ok((id, started.elapsed()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    fn tmpdir(tag: &str) -> PathBuf {
        // 每个测试一个独立目录：测试是并行跑的，共享目录会被另一个测试的清理删掉。
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "qxync-peer-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    async fn spawn_server(
        source: Arc<dyn ContentSource>,
        events: tokio::sync::mpsc::UnboundedSender<PeerEvent>,
    ) -> (String, Arc<PeerServer>) {
        let (reg_tx, _reg_rx) = tokio::sync::mpsc::unbounded_channel();
        let srv = Arc::new(PeerServer::new(
            "srv",
            "0.1.0-test",
            None,
            vec!["/home".into()],
            source,
            events,
            reg_tx,
        ));
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let s = srv.clone();
        tokio::spawn(async move { serve(l, s).await });
        (addr, srv)
    }

    fn dir_source() -> (Arc<dyn ContentSource>, PathBuf) {
        let d = tmpdir("src");
        std::fs::create_dir_all(d.join("qxync-test")).unwrap();
        std::fs::write(d.join("qxync-test/hello.txt"), b"hello lan").unwrap();
        std::fs::write(d.join("qxync-test/big.bin"), vec![7u8; 4096]).unwrap();
        let src: Arc<dyn ContentSource> =
            Arc::new(DirContent::new(vec![("/home".into(), d.clone())]));
        (src, d)
    }

    #[tokio::test]
    async fn pairing_auth_head_get_range_and_event() {
        let (src, dir) = dir_source();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (addr, srv) = spawn_server(src, tx).await;
        let code = srv.pairing_code().unwrap();

        // ping 不需要 token
        let id = PeerClient::ping(&addr).await.unwrap();
        assert_eq!(id.name, "srv");
        assert!(id.pairing_open);

        // 错码 → 拒绝
        assert!(PeerClient::pair(&addr, "000000", "cli", vec![])
            .await
            .is_err());
        // 正确配对码 → 拿 token，且配对码轮换（旧的作废）
        let (token, peer) = PeerClient::pair(&addr, &code, "cli", vec!["/home".into()])
            .await
            .unwrap();
        assert_eq!(token.len(), 32);
        assert_eq!(peer.name, "srv");
        let code2 = srv.pairing_code().unwrap();
        assert_ne!(code, code2, "配对成功后必须轮换配对码");
        assert!(PeerClient::pair(&addr, &code, "cli2", vec![])
            .await
            .is_err());

        // 错 token → 拒绝
        let bogus = PeerClient::new(&addr, "cli", "deadbeef");
        assert!(bogus.head("/home/qxync-test/hello.txt").await.is_err());

        let c = PeerClient::new(&addr, "cli", token.clone());
        let h = c.head("/home/qxync-test/hello.txt").await.unwrap();
        assert!(h.exists && h.hydrated);
        assert_eq!(h.size, 9);

        // 全量 + Range
        let (all, _) = c
            .get_range("/home/qxync-test/big.bin", 0, 4096)
            .await
            .unwrap();
        assert_eq!(all, vec![7u8; 4096]);
        let (part, _) = c
            .get_range("/home/qxync-test/big.bin", 1024, 128)
            .await
            .unwrap();
        assert_eq!(part, vec![7u8; 128]);

        // 路径越权
        let bad = c.head("/etc/passwd").await;
        assert!(bad.is_err(), "root 之外的路径必须拒绝");
        let bad2 = c.get_range("/home/../etc/passwd", 0, 4).await;
        assert!(bad2.is_err(), ".. 必须拒绝");
        // 不存在的文件
        let missing = c.head("/home/qxync-test/nope.bin").await.unwrap();
        assert!(!missing.exists);

        // 事件
        assert!(c
            .send_event(PeerEvent::new(
                "/home/qxync-test/hello.txt",
                9,
                1,
                "modified"
            ))
            .await
            .unwrap());
        let got = rx.recv().await.unwrap();
        assert_eq!(got.path, "/home/qxync-test/hello.txt");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn hello_registers_peer_for_bidirectional_trust() {
        let (src, dir) = dir_source();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (reg_tx, mut reg_rx) = tokio::sync::mpsc::unbounded_channel();
        let srv = Arc::new(PeerServer::new(
            "srv",
            "0.1.0-test",
            Some("10.0.0.2:9840".into()),
            vec!["/home".into()],
            src,
            tx,
            reg_tx,
        ));
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let s = srv.clone();
        tokio::spawn(async move { serve(l, s).await });
        let code = srv.pairing_code().unwrap();
        let (token, _id) = PeerClient::pair(&addr, &code, "cli", vec!["/home".into()])
            .await
            .unwrap();
        // 错 token 不能登记
        assert!(
            PeerClient::hello(&addr, "nope", "cli", "10.0.0.9:9840", vec![])
                .await
                .is_err()
        );
        // 正确 token → 服务端把登记转发给 daemon
        let back = PeerClient::hello(&addr, &token, "cli", "10.0.0.9:9840", vec!["/home".into()])
            .await
            .unwrap();
        assert_eq!(back.name, "srv");
        let reg = reg_rx.recv().await.unwrap();
        assert_eq!(reg.name, "cli");
        assert_eq!(reg.addr, "10.0.0.9:9840");
        assert_eq!(reg.token, token);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn fetch_range_matches_metadata_or_skips() {
        let (src, dir) = dir_source();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (addr, srv) = spawn_server(src, tx).await;
        srv.add_token("cli", "tok");
        let peers = vec![PeerConfig {
            name: "cli".into(),
            addr: addr.clone(),
            token: "tok".into(),
        }];
        let meta = srv.source.head("/home/qxync-test/hello.txt").unwrap();
        // 元数据一致 → 命中
        let hit = fetch_range(
            &peers,
            "/home/qxync-test/hello.txt",
            0,
            meta.size,
            meta.size,
            meta.mtime,
        )
        .await
        .expect("元数据一致时必须命中");
        assert_eq!(hit.data, b"hello lan");
        assert_eq!(hit.peer, "cli");
        // 大小/时间对不上（对端改过还没上传完）→ 跳过，回落 NAS
        assert!(fetch_range(
            &peers,
            "/home/qxync-test/hello.txt",
            0,
            meta.size,
            meta.size + 1,
            meta.mtime
        )
        .await
        .is_none());
        assert!(fetch_range(
            &peers,
            "/home/qxync-test/hello.txt",
            0,
            meta.size,
            meta.size,
            meta.mtime + 1
        )
        .await
        .is_none());
        // token 不对 → 跳过
        let bad = vec![PeerConfig {
            name: "cli".into(),
            addr: addr.clone(),
            token: "nope".into(),
        }];
        assert!(fetch_range(
            &bad,
            "/home/qxync-test/hello.txt",
            0,
            meta.size,
            meta.size,
            meta.mtime
        )
        .await
        .is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn head_reports_not_hydrated_and_len_guard() {
        struct Partial;
        impl ContentSource for Partial {
            fn head(&self, _p: &str) -> Option<PeerHead> {
                None
            }
            fn read_at(&self, _p: &str, _o: u64, _l: usize) -> io::Result<Vec<u8>> {
                Err(io::Error::other("没水合"))
            }
            fn roots(&self) -> Vec<String> {
                vec!["/home".into()]
            }
        }
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (addr, srv) = spawn_server(Arc::new(Partial), tx).await;
        srv.add_token("cli", "tok");
        let c = PeerClient::new(&addr, "cli", "tok");
        let h = c.head("/home/x.bin").await.unwrap();
        assert!(!h.exists && !h.hydrated);
        // len 超上限 / 为 0 都被拒
        assert!(c
            .get_range("/home/x.bin", 0, MAX_GET_LEN + 1)
            .await
            .is_err());
        assert!(c.get_range("/home/x.bin", 0, 0).await.is_err());
    }

    #[test]
    fn tokens_and_codes() {
        assert_eq!(random_hex(16).len(), 32);
        assert_ne!(random_hex(16), random_hex(16));
        let code = random_pairing_code();
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "abcd"));
    }

    #[test]
    fn path_guard_rules() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (reg_tx, _rr) = tokio::sync::mpsc::unbounded_channel();
        let srv = PeerServer::new(
            "s",
            "v",
            None,
            vec!["/home".into(), "/Public".into()],
            Arc::new(DirContent::default()),
            tx,
            reg_tx,
        );
        assert!(srv.path_allowed("/home/a/b"));
        assert!(srv.path_allowed("/Public/x"));
        assert!(!srv.path_allowed("/etc/passwd"));
        assert!(!srv.path_allowed("/home/a/../b"));
        assert!(!srv.path_allowed("home/a"));
        assert!(!srv.path_allowed(""));
    }

    #[test]
    fn pairing_attempts_are_limited() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (reg_tx, _rr) = tokio::sync::mpsc::unbounded_channel();
        let srv = PeerServer::new(
            "s",
            "v",
            None,
            vec!["/home".into()],
            Arc::new(DirContent::default()),
            tx,
            reg_tx,
        );
        let code = srv.pairing_code().unwrap();
        // 先打满错误尝试
        for _ in 0..PAIR_MAX_ATTEMPTS {
            assert!(!srv.check_pairing("000000"));
        }
        // 正确的码也被限流挡住（不告诉对方码是对的还是错的）
        assert!(!srv.check_pairing(&code), "超过尝试上限必须拒绝");
    }
}
