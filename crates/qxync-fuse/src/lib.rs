//! # qxync-fuse —— M1：只读 FUSE + on-demand 整文件水合
//!
//! 设计原则（照报告 12 篇）：
//!
//! * **FUSE 适配层做薄**：本 crate 只负责「FUSE 回调 → NAS/缓存」的翻译；
//!   占位符语义、水合编排、短读校验都在这里显式表达，方便无挂载场景复用与推理。
//! * **铁则 1**：`read()` 在 `offset + size <= size` 时**必须**返回 `size` 字节，
//!   拿不满就回 `EIO` —— 绝不短读（短读会被内核零填充并缓存 → 数据静默损坏）。
//! * **水合去重**：同一路径并发读只下载一次（single-flight），超时 60s。
//! * 元数据（`getattr`/`readdir`/`lookup`）**不触发下载**：`ls -l` 显示的必须是真实大小。
//!
//! M1 故意用「整文件水合」这种笨办法，先把 FUSE 语义问题一次性暴露出来；
//! M2 再把 `ensure_hydrated` 换成 128 KiB 区间（数据面已实测支持 `Range` → 206）。

use fuser::{
    Config, FileAttr, FileType, Filesystem, FopenFlags, Generation, INodeNo, OpenAccMode,
    ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyXattr, Request,
};
use qxync_client::Client;
use qxync_core::DirEntry;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

const ENTRY_TTL: Duration = Duration::from_millis(500);
/// 水合超时：对应 Qsync 的 `CANCEL_FETCH_DATA` = 60s。
const HYDRATE_TIMEOUT: Duration = Duration::from_secs(60);
/// 目录列举分页上限（对应服务端 `Max_File_List`）。
const LIST_LIMIT: usize = 200;

/// 一个远端节点。
#[derive(Debug, Clone)]
struct Node {
    ino: INodeNo,
    parent: INodeNo,
    name: String,
    /// NAS 上的绝对路径。
    remote: String,
    attr: FileAttr,
    /// 已水合的本地缓存文件。
    hydrated: Option<PathBuf>,
}

struct Inner {
    nodes: HashMap<INodeNo, Node>,
    by_remote: HashMap<String, INodeNo>,
    next_ino: u64,
    /// 水合 single-flight：remote path → 该次水合的锁。
    inflight: HashMap<String, Arc<Mutex<()>>>,
}

/// 远端路径 → pin 状态（`pinned` / `unpinned` / `unspecified` / `excluded`）。
///
/// daemon 持有这份 map 并注入给 FUSE 实例，这样 `setfattr`/`getfattr` 与 IPC 的 `pin` 方法看到同一份状态。
pub type PinMap = Arc<Mutex<HashMap<String, String>>>;

/// 水合统计。挂在 `Arc` 上，**daemon 可以在 mount 之后继续读**（FUSE 实例本身已被移进挂载线程）。
#[derive(Debug, Default)]
pub struct HydroCounters {
    count: AtomicU64,
    bytes: AtomicU64,
}

impl HydroCounters {
    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.count.load(Ordering::Relaxed),
            self.bytes.load(Ordering::Relaxed),
        )
    }
    fn record(&self, bytes: u64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }
}

pub struct QxyncFs {
    rt: tokio::runtime::Runtime,
    client: Arc<Client>,
    /// 远端挂载根（普通用户家目录 = `/home`）。
    remote_root: String,
    cache_dir: PathBuf,
    uid: u32,
    gid: u32,
    hydrate_timeout: Duration,
    hydro: Arc<HydroCounters>,
    /// pin 状态（与 daemon 共享）；为空 map 时一律回 `unspecified`。
    pins: PinMap,
    inner: Mutex<Inner>,
}

impl QxyncFs {
    pub fn new(
        client: Arc<Client>,
        remote_root: impl Into<String>,
        cache_dir: impl Into<PathBuf>,
    ) -> std::io::Result<Self> {
        let remote_root = remote_root.into();
        let cache_dir = cache_dir.into();
        std::fs::create_dir_all(&cache_dir)?;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;

        let root_attr = FileAttr {
            ino: INodeNo::ROOT,
            size: 0,
            blocks: 0,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: FileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: 4096,
            flags: 0,
        };
        let root = Node {
            ino: INodeNo::ROOT,
            parent: INodeNo::ROOT,
            name: String::new(),
            remote: remote_root.clone(),
            attr: root_attr,
            hydrated: None,
        };
        let mut nodes = HashMap::new();
        let mut by_remote = HashMap::new();
        nodes.insert(INodeNo::ROOT, root);
        by_remote.insert(remote_root.clone(), INodeNo::ROOT);

        Ok(Self {
            rt,
            client,
            remote_root,
            cache_dir,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            hydrate_timeout: HYDRATE_TIMEOUT,
            hydro: Arc::new(HydroCounters::default()),
            pins: Arc::new(Mutex::new(HashMap::new())),
            inner: Mutex::new(Inner {
                nodes,
                by_remote,
                next_ino: 2,
                inflight: HashMap::new(),
            }),
        })
    }

    /// 覆盖水合超时（大文件 + 慢链路时可以放大；M1 是整文件水合，M2 改成区间后就不敏感了）。
    pub fn with_hydrate_timeout(mut self, d: Duration) -> Self {
        self.hydrate_timeout = d;
        self
    }

    pub fn remote_root(&self) -> &str {
        &self.remote_root
    }

    /// 观测用：(水合次数, 已水合字节数)。拿到 daemon 后仍可继续读。
    pub fn hydration_stats(&self) -> (u64, u64) {
        self.hydro.snapshot()
    }

    /// 共享计数器句柄（daemon 在把实例移进挂载线程前取一份）。
    pub fn counters(&self) -> Arc<HydroCounters> {
        self.hydro.clone()
    }

    /// 共享 pin 状态（daemon 场景：IPC 的 `pin` 与 xattr 要看到同一份）。
    pub fn with_pins(mut self, pins: PinMap) -> Self {
        self.pins = pins;
        self
    }

    /// 用外部传入的计数器（daemon 场景：挂载后还要读统计）。
    pub fn with_counters(mut self, counters: Arc<HydroCounters>) -> Self {
        self.hydro = counters;
        self
    }

    fn attr_from(&self, ino: INodeNo, e: &DirEntry) -> FileAttr {
        let kind = if e.isfolder {
            FileType::Directory
        } else {
            FileType::RegularFile
        };
        let perm = if e.isfolder { 0o755 } else { 0o644 };
        let secs = e.epochmt.max(0) as u64;
        let mtime = UNIX_EPOCH + Duration::from_secs(secs);
        let size = e.display_size();
        FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: mtime,
            kind,
            perm,
            nlink: if e.isfolder { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    fn join_remote(&self, parent_remote: &str, name: &str) -> String {
        format!("{}/{}", parent_remote.trim_end_matches('/'), name)
    }

    /// 把 NAS 路径映射成缓存文件名（避免任何路径穿越/特殊字符问题）。
    fn cache_path(&self, ino: INodeNo, name: &str) -> PathBuf {
        let safe: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.cache_dir.join(format!("{}_{}", u64::from(ino), safe))
    }

    /// 在目录里查一个名字（不水合）。返回节点克隆。
    fn lookup_child(&self, parent: INodeNo, name: &str) -> Result<Node, fuser::Errno> {
        let parent_remote = {
            let g = self.inner.lock().unwrap();
            g.nodes
                .get(&parent)
                .ok_or(fuser::Errno::ENOENT)?
                .remote
                .clone()
        };
        let remote = self.join_remote(&parent_remote, name);

        if let Some(node) = self.node_by_remote(&remote) {
            return Ok(node);
        }
        let entry = match self.rt.block_on(self.client.stat(&parent_remote, name)) {
            Ok(Some(e)) => e,
            Ok(None) => return Err(fuser::Errno::ENOENT),
            Err(e) => {
                tracing::warn!("lookup {remote} 失败: {e}");
                return Err(fuser::Errno::ENOENT);
            }
        };
        Ok(self.insert_node(parent, name, &remote, &entry))
    }

    fn node_by_remote(&self, remote: &str) -> Option<Node> {
        let g = self.inner.lock().unwrap();
        let ino = *g.by_remote.get(remote)?;
        g.nodes.get(&ino).cloned()
    }

    fn insert_node(&self, parent: INodeNo, name: &str, remote: &str, entry: &DirEntry) -> Node {
        let mut g = self.inner.lock().unwrap();
        if let Some(ino) = g.by_remote.get(remote) {
            if let Some(n) = g.nodes.get(ino) {
                return n.clone();
            }
        }
        let ino = INodeNo(g.next_ino);
        g.next_ino += 1;
        let attr = self.attr_from(ino, entry);
        let node = Node {
            ino,
            parent,
            name: name.to_string(),
            remote: remote.to_string(),
            attr,
            hydrated: None,
        };
        g.nodes.insert(ino, node.clone());
        g.by_remote.insert(remote.to_string(), ino);
        node
    }

    /// 列一个目录（不水合），并把子节点的元数据灌进表里。
    fn load_children(&self, ino: INodeNo) -> Result<Vec<Node>, fuser::Errno> {
        let remote = {
            let g = self.inner.lock().unwrap();
            g.nodes
                .get(&ino)
                .ok_or(fuser::Errno::ENOENT)?
                .remote
                .clone()
        };
        let entries = match self.rt.block_on(self.client.list(&remote)) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("readdir {remote} 失败: {e}");
                return Err(fuser::Errno::EIO);
            }
        };
        let mut out = Vec::with_capacity(entries.len());
        for e in entries.iter().take(LIST_LIMIT) {
            let child_remote = self.join_remote(&remote, &e.filename);
            out.push(self.insert_node(ino, &e.filename, &child_remote, e));
        }
        Ok(out)
    }

    /// ★ 水合：把整个文件下到缓存目录（single-flight + 60s 超时 + 长度校验）。
    fn ensure_hydrated(&self, ino: INodeNo) -> Result<PathBuf, fuser::Errno> {
        let (remote, expected, cached) = {
            let g = self.inner.lock().unwrap();
            let n = g.nodes.get(&ino).ok_or(fuser::Errno::ENOENT)?;
            (n.remote.clone(), n.attr.size, n.hydrated.clone())
        };
        if let Some(p) = cached {
            if p.exists() {
                return Ok(p);
            }
        }

        // single-flight：同一路径只允许一个下载在跑
        let cell = {
            let mut g = self.inner.lock().unwrap();
            g.inflight
                .entry(remote.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _guard = cell.lock().unwrap();

        // 拿到锁后再查一次（可能别人已经下好了）
        {
            let g = self.inner.lock().unwrap();
            if let Some(n) = g.nodes.get(&ino) {
                if let Some(p) = &n.hydrated {
                    if p.exists() {
                        return Ok(p.clone());
                    }
                }
            }
        }

        let (parent, name) = {
            let g = self.inner.lock().unwrap();
            let n = g.nodes.get(&ino).ok_or(fuser::Errno::ENOENT)?;
            (n.parent, n.name.clone())
        };
        let _ = parent;
        let dir = remote
            .rsplit_once('/')
            .map(|(d, _)| d.to_string())
            .unwrap_or_else(|| self.remote_root.clone());
        let dest = self.cache_path(ino, &name);

        let client = self.client.clone();
        let dest2 = dest.clone();
        let dir2 = dir.clone();
        let name2 = name.clone();
        let timeout = self.hydrate_timeout;
        let res = self.rt.block_on(async move {
            tokio::time::timeout(timeout, client.download_to_file(&dir2, &name2, &dest2)).await
        });

        let written = match res {
            Err(_) => {
                tracing::warn!("水合超时({:?}): {remote}", timeout);
                cleanup_partial(&dest);
                self.inner.lock().unwrap().inflight.remove(&remote);
                return Err(fuser::Errno::EIO);
            }
            Ok(Err(e)) => {
                tracing::warn!("水合失败: {remote}: {e}");
                cleanup_partial(&dest);
                self.inner.lock().unwrap().inflight.remove(&remote);
                return Err(fuser::Errno::EIO);
            }
            Ok(Ok(n)) => n,
        };

        // ★ 长度校验：下载到的字节数必须与元数据一致，否则宁可报错也不能让内核看到短文件
        if written != expected {
            tracing::error!("水合长度不符: {remote} 期望 {expected} 实得 {written}");
            let _ = std::fs::remove_file(&dest);
            self.inner.lock().unwrap().inflight.remove(&remote);
            return Err(fuser::Errno::EIO);
        }

        {
            let mut g = self.inner.lock().unwrap();
            if let Some(n) = g.nodes.get_mut(&ino) {
                n.hydrated = Some(dest.clone());
            }
            g.inflight.remove(&remote);
        }
        self.hydro.record(written);
        tracing::info!("水合完成: {remote} ({written} 字节)");
        Ok(dest)
    }

    /// 从缓存文件里读一段；**不足就报 EIO**（铁则 1）。
    fn read_exact_from_cache(
        &self,
        path: &Path,
        offset: u64,
        size: usize,
    ) -> Result<Vec<u8>, fuser::Errno> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(path).map_err(|_| fuser::Errno::EIO)?;
        f.seek(SeekFrom::Start(offset))
            .map_err(|_| fuser::Errno::EIO)?;
        let mut buf = vec![0u8; size];
        let mut filled = 0usize;
        while filled < size {
            match f.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(fuser::Errno::EIO),
            }
        }
        if filled != size {
            // 铁则 1：调用方已确认这段在文件范围内，读不满就是错误
            tracing::error!(
                "缓存短读: {} offset={offset} want={size} got={filled}",
                path.display()
            );
            return Err(fuser::Errno::EIO);
        }
        Ok(buf)
    }
}

impl Filesystem for QxyncFs {
    fn init(&mut self, _req: &Request, config: &mut fuser::KernelConfig) -> std::io::Result<()> {
        // 与 Qsync 的 128 KiB 水合粒度对齐（M2 会按这个粒度发 Range 请求）
        let _ = config.set_max_readahead(128 * 1024);
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let name = match name.to_str() {
            Some(n) => n,
            None => return reply.error(fuser::Errno::ENOENT),
        };
        if name == "." {
            let g = self.inner.lock().unwrap();
            return match g.nodes.get(&parent) {
                Some(n) => reply.entry(&ENTRY_TTL, &n.attr, Generation(0)),
                None => reply.error(fuser::Errno::ENOENT),
            };
        }
        if name == ".." {
            let g = self.inner.lock().unwrap();
            let p = g
                .nodes
                .get(&parent)
                .map(|n| n.parent)
                .unwrap_or(INodeNo::ROOT);
            return match g.nodes.get(&p) {
                Some(n) => reply.entry(&ENTRY_TTL, &n.attr, Generation(0)),
                None => reply.error(fuser::Errno::ENOENT),
            };
        }
        match self.lookup_child(parent, name) {
            Ok(node) => reply.entry(&ENTRY_TTL, &node.attr, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: Option<fuser::FileHandle>,
        reply: ReplyAttr,
    ) {
        // 目录/文件都直接答元数据 —— 不触发任何下载（ls -l 必须是真实大小）
        let attr = {
            let g = self.inner.lock().unwrap();
            g.nodes.get(&ino).map(|n| n.attr)
        };
        match attr {
            Some(a) => reply.attr(&ENTRY_TTL, &a),
            None => reply.error(fuser::Errno::ENOENT),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: fuser::OpenFlags, reply: ReplyOpen) {
        // M1 只读：任何写意图一律 EACCES（M2 再实现写路径）
        if flags.acc_mode() != OpenAccMode::O_RDONLY {
            return reply.error(fuser::Errno::EACCES);
        }
        let exists = { self.inner.lock().unwrap().nodes.contains_key(&ino) };
        if exists {
            reply.opened(
                fuser::FileHandle(u64::from(ino)),
                FopenFlags::FOPEN_KEEP_CACHE,
            );
        } else {
            reply.error(fuser::Errno::ENOENT);
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: fuser::FileHandle,
        offset: u64,
        size: u32,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        let file_size = {
            let g = self.inner.lock().unwrap();
            match g.nodes.get(&ino) {
                Some(n) => n.attr.size,
                None => return reply.error(fuser::Errno::ENOENT),
            }
        };
        if offset >= file_size {
            return reply.data(&[]); // 正常 EOF
        }
        // 水合（single-flight）
        let path = match self.ensure_hydrated(ino) {
            Ok(p) => p,
            Err(e) => return reply.error(e),
        };
        // 请求范围若越过文件尾，只需返回实际存在的部分（这是 EOF，不是短读）
        let want = (size as u64).min(file_size - offset) as usize;
        match self.read_exact_from_cache(&path, offset, want) {
            Ok(buf) => reply.data(&buf),
            Err(e) => reply.error(e),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: fuser::FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let mut entries: Vec<(INodeNo, FileType, String)> = Vec::new();
        {
            let g = self.inner.lock().unwrap();
            let Some(node) = g.nodes.get(&ino) else {
                return reply.error(fuser::Errno::ENOENT);
            };
            if node.attr.kind != FileType::Directory {
                return reply.error(fuser::Errno::ENOTDIR);
            }
            let parent = node.parent;
            entries.push((ino, FileType::Directory, ".".into()));
            entries.push((parent, FileType::Directory, "..".into()));
        }
        let children = match self.load_children(ino) {
            Ok(c) => c,
            Err(e) => return reply.error(e),
        };
        for c in children {
            entries.push((c.ino, c.attr.kind, c.name));
        }
        for (i, (child_ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            // 返回 true 表示缓冲已满
            if reply.add(child_ino, (i + 1) as u64, kind, &name) {
                break;
            }
        }
        reply.ok();
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let (hydrated, remote, fsize) = {
            let g = self.inner.lock().unwrap();
            match g.nodes.get(&ino) {
                Some(n) => (n.hydrated.is_some(), n.remote.clone(), n.attr.size),
                None => return reply.error(fuser::Errno::ENOENT),
            }
        };
        tracing::debug!(
            "getxattr ino={} name={} size={size}",
            u64::from(ino),
            name.to_string_lossy()
        );
        let key = name.to_string_lossy();
        let value: Option<String> = match key.as_ref() {
            // 占位符状态：M1 用「是否已下载」判定
            "user.qsync.state" => Some(if hydrated { "hydrated" } else { "placeholder" }.into()),
            "user.qsync.remote" => Some(remote),
            "user.qsync.vsize" => Some(fsize.to_string()),
            "user.qsync.pin" => Some(
                self.pins
                    .lock()
                    .unwrap()
                    .get(&remote)
                    .cloned()
                    .unwrap_or_else(|| "unspecified".to_string()),
            ),
            _ => None,
        };
        let Some(v) = value else {
            return reply.error(fuser::Errno::ENODATA);
        };
        if size == 0 {
            reply.size(v.len() as u32);
        } else if (size as usize) < v.len() {
            reply.error(fuser::Errno::ERANGE);
        } else {
            reply.data(v.as_bytes());
        }
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        // ★ 末尾必须留一个 NUL：内核 fuse_verify_xattr_list() 会逐项 strnlen，
        // 最后一项没有终止符就直接对整个 listxattr 回 -EIO（实测过：size<66 回 ERANGE，
        // size>=66 反而回 EIO，就是这个校验触发的）。
        let names = "user.qsync.state\0user.qsync.pin\0user.qsync.remote\0user.qsync.vsize\0";
        let exists = { self.inner.lock().unwrap().nodes.contains_key(&ino) };
        if !exists {
            return reply.error(fuser::Errno::ENOENT);
        }
        if size == 0 {
            reply.size(names.len() as u32);
        } else if (size as usize) < names.len() {
            reply.error(fuser::Errno::ERANGE);
        } else {
            reply.data(names.as_bytes());
        }
    }

    // 只读场景下这几个是空操作，但必须显式实现：
    // fuser 的默认实现会回 ENOSYS 并打 WARN（实测每次 close 都刷屏）。
    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: fuser::OpenFlags, reply: ReplyOpen) {
        let exists = { self.inner.lock().unwrap().nodes.contains_key(&ino) };
        if exists {
            reply.opened(fuser::FileHandle(u64::from(ino)), FopenFlags::empty());
        } else {
            reply.error(fuser::Errno::ENOENT);
        }
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: fuser::FileHandle,
        _flags: fuser::OpenFlags,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: fuser::FileHandle,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: fuser::FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: fuser::FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn access(&self, _req: &Request, ino: INodeNo, _mask: fuser::AccessFlags, reply: ReplyEmpty) {
        let exists = { self.inner.lock().unwrap().nodes.contains_key(&ino) };
        if exists {
            reply.ok();
        } else {
            reply.error(fuser::Errno::ENOENT);
        }
    }

    fn destroy(&mut self) {
        let (n, b) = self.hydro.snapshot();
        tracing::info!("unmount: 水合 {n} 次 / {b} 字节");
    }
}

/// 清掉水合失败留下的半截文件：缓存目标 + `.qsync-part` 临时文件。
fn cleanup_partial(dest: &Path) {
    let _ = std::fs::remove_file(dest);
    let tmp = std::path::PathBuf::from(format!("{}.qsync-part", dest.display()));
    let _ = std::fs::remove_file(tmp);
}

/// 挂载参数：`-o ro` + 推荐选项（报告 12 §6.3）。
///
/// `auto_unmount` 在 fuser 0.17 里要求 `SessionACL != Owner`（即 `allow_other`），
/// 非特权挂载还需要 `/etc/fuse.conf` 里的 `user_allow_other`；因此做成开关，默认关闭。
pub fn mount_options(auto_unmount: bool) -> Vec<fuser::MountOption> {
    use fuser::MountOption::*;
    let mut opts = vec![
        RO,
        FSName("qxync".to_string()),
        Subtype("qxync".to_string()),
        DefaultPermissions,
        NoAtime,
        // 注意：attr_timeout / entry_timeout / max_read 不是 fusermount 的挂载选项
        // （传了会报 "unknown option"）。这里改用：
        //   * TTL        → 每次 reply 给 ENTRY_TTL=0.5s（缓存短，避免看不到远端变更）
        //   * 读请求粒度 → init() 里设 max_readahead = 128 KiB
    ];
    if auto_unmount {
        opts.push(AutoUnmount);
    }
    opts
}

/// 会话配置：多线程 + 每线程独立 fd（Linux 4.5+）。
///
/// 单线程会让「并发读同一文件」退化成串行，看不出水合去重是否真的生效；
/// 报告 12 §6.3 也推荐 FUSE 多线程 + 共享状态。
pub fn mount_config(n_threads: usize, auto_unmount: bool) -> Config {
    // `Config` 是 #[non_exhaustive]，外部 crate 不能写字面量，只能 default + 逐字段赋值
    let mut cfg = Config::default();
    cfg.mount_options = mount_options(auto_unmount);
    cfg.n_threads = Some(n_threads.max(1));
    cfg.clone_fd = std::env::var("QSYNC_CLONE_FD")
        .map(|v| v != "0")
        .unwrap_or(false);
    if auto_unmount {
        // fuser 0.17 的硬要求：auto_unmount 必须 acl != Owner
        cfg.acl = fuser::SessionACL::RootAndOwner;
    }
    cfg
}

/// 便捷入口：前台挂载（阻塞）。
pub fn mount(
    fs: QxyncFs,
    mountpoint: &Path,
    n_threads: usize,
    auto_unmount: bool,
) -> std::io::Result<()> {
    fuser::mount2(fs, mountpoint, &mount_config(n_threads, auto_unmount))
}

#[cfg(test)]
mod tests {
    #[test]
    fn cache_path_is_sanitized() {
        // 不能在无挂载的情况下构造 QxyncFs（需要真实 client），
        // 所以这里只验证命名规则本身。
        let name = "空 格 中文名.txt";
        let safe: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        assert_eq!(safe, "_______.txt");
        assert!(!safe.contains('/'));
    }
}
