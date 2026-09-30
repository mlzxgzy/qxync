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
//! **M2：128 KiB 区间水合**。缓存文件是「apparent size = 文件大小」的稀疏文件，
//! 只把读到的区间写进去；`head -c 100 big.bin` 只会下载 1 个 128 KiB 区间，
//! 不会再整文件下载（数据面实测支持 `Range` → `206 + Content-Range`）。

use fuser::{
    Config, FileAttr, FileType, Filesystem, FopenFlags, Generation, INodeNo, OpenAccMode,
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyWrite, ReplyXattr, Request, TimeOrNow,
};

pub mod upload;

use qxync_client::Client;
use qxync_core::DirEntry;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use upload::{UploadJob, UploadQueue, UploadSnapshot};

const ENTRY_TTL: Duration = Duration::from_millis(500);
/// 水合超时：对应 Qsync 的 `CANCEL_FETCH_DATA` = 60s（M2 起是**单区间**的超时）。
const HYDRATE_TIMEOUT: Duration = Duration::from_secs(60);
/// 水合粒度：128 KiB（与 Qsync 的 CfAPI `FETCH_DATA` 对齐）。
pub const DEFAULT_CHUNK_SIZE: u64 = 128 * 1024;
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
    /// 稀疏缓存文件：**apparent size = 文件大小**，只有下载过的区间有实际数据。
    cache: Option<PathBuf>,
    /// 每个 128 KiB 区间的完成标记；长度 = 区间数，在创建缓存文件时初始化。
    chunks_done: Vec<bool>,
    /// 本地有未上传的改动。
    dirty: bool,
}

impl Node {
    /// 水合区间数（0 字节文件算 1 个空区间）。
    fn chunk_count(&self, chunk_size: u64) -> usize {
        if self.attr.size == 0 {
            1
        } else {
            self.attr.size.div_ceil(chunk_size) as usize
        }
    }

    fn is_fully_hydrated(&self) -> bool {
        !self.chunks_done.is_empty() && self.chunks_done.iter().all(|d| *d)
    }

    fn is_partially_hydrated(&self) -> bool {
        self.chunks_done.iter().any(|d| *d)
    }

    /// 给 xattr 用的状态串。
    fn state_str(&self) -> &'static str {
        if !self.is_partially_hydrated() {
            "placeholder"
        } else if self.is_fully_hydrated() {
            "hydrated"
        } else {
            "partial"
        }
    }
}

struct Inner {
    nodes: HashMap<INodeNo, Node>,
    by_remote: HashMap<String, INodeNo>,
    next_ino: u64,
    /// 区间水合 single-flight：(ino, 区间下标) → 该次下载的锁。
    inflight_chunks: HashMap<(u64, u64), Arc<Mutex<()>>>,
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
    /// 只读模式（默认）。M2b 起可用 `with_write_mode()` 打开写路径。
    read_only: bool,
    /// 上传队列（写模式下必须提供）。
    upload: Option<Arc<UploadQueue>>,
    /// 水合粒度（M2：128 KiB，与 Qsync 的 CfAPI FETCH_DATA 对齐）。
    chunk_size: u64,
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
            cache: None,
            chunks_done: Vec::new(),
            dirty: false,
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
            read_only: true,
            upload: None,
            chunk_size: DEFAULT_CHUNK_SIZE,
            hydrate_timeout: HYDRATE_TIMEOUT,
            hydro: Arc::new(HydroCounters::default()),
            pins: Arc::new(Mutex::new(HashMap::new())),
            inner: Mutex::new(Inner {
                nodes,
                by_remote,
                next_ino: 2,
                inflight_chunks: HashMap::new(),
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

    /// 打开写路径（读写挂载；写操作需要上传队列）。
    pub fn with_write_mode(mut self) -> Self {
        self.read_only = false;
        self
    }

    /// 注入上传队列（写模式下必须）。
    pub fn with_upload_queue(mut self, queue: Arc<UploadQueue>) -> Self {
        self.upload = Some(queue);
        self
    }

    /// 上传队列快照（daemon `status` 用）。
    pub fn upload_stats(&self) -> Option<UploadSnapshot> {
        self.upload.as_ref().map(|q| q.snapshot())
    }

    /// 覆盖水合粒度（默认 128 KiB）。
    pub fn with_chunk_size(mut self, chunk_size: u64) -> Self {
        assert!(chunk_size > 0, "chunk_size 必须 > 0");
        self.chunk_size = chunk_size;
        self
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

    /// 把 NAS 路径映射成缓存文件名。
    ///
    /// ★ 前缀必须是**远端路径的稳定哈希**，不能用 ino：ino 是按需分配的，
    /// 两次挂载（或 readdir 顺序不同）里同一个 ino 可能对应不同文件，
    /// 那样就会把 A 的内容当成 B 的缓存读到 —— 属于最危险的「静默错数据」。
    fn cache_path(&self, remote: &str, name: &str) -> PathBuf {
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
        self.cache_dir
            .join(format!("{:016x}_{}", fnv1a64(remote.as_bytes()), safe))
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
            cache: None,
            chunks_done: Vec::new(),
            dirty: false,
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

    /// 远端路径的目录部分。
    fn remote_dir_of(&self, remote: &str) -> String {
        remote
            .rsplit_once('/')
            .map(|(d, _)| d.to_string())
            .unwrap_or_else(|| self.remote_root.clone())
    }

    fn epoch_of(t: SystemTime) -> i64 {
        t.duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    /// 把所有区间取回本地。
    ///
    /// ★ **read-modify-write 的前提**：写一个还没取全的占位符文件时，未取回的区间在本地是 0，
    /// 直接写+整文件上传会把远端内容清零。所以写之前必须补齐（`skip` 可用于跳过将被整块覆盖的区间）。
    fn hydrate_all(&self, ino: INodeNo, skip: Option<(u64, u64)>) -> Result<(), fuser::Errno> {
        let (total, chunk_size, local_authoritative) = {
            let g = self.inner.lock().unwrap();
            let n = g.nodes.get(&ino).ok_or(fuser::Errno::ENOENT)?;
            // ★ 本地有未上传改动（或队列里还挂着）时，**本地缓存就是权威内容**：
            //   此时去远端拉取既可能拿到旧内容，也可能 404（本地新建的文件远端还没有）——
            //   实测过的坑：本地新建文件写第二次时报 HTTP 404。
            let pending = self
                .upload
                .as_ref()
                .map(|q| q.has_pending(&n.remote))
                .unwrap_or(false);
            (n.attr.size, self.chunk_size, n.dirty || pending)
        };
        if local_authoritative {
            self.cache_file_for(ino)?;
            return Ok(());
        }
        // 缓存文件必须先存在（ensure_chunk 要往里 pwrite），它同时也是「区间表」的初始化点
        self.cache_file_for(ino)?;
        if total == 0 {
            return Ok(());
        }
        let nchunks = total.div_ceil(chunk_size);
        let write = skip.filter(|(_, len)| *len > 0);
        for idx in 0..nchunks {
            // ★ 只有「写范围**完整覆盖**该区间」时才能跳过。
            //   注意不能只看「写到了这个区间」：区间内只改几个字节时，
            //   其余字节仍是远端原内容，跳过就会把它们当 0 上传（实测过：尾部追加把前 10 KB 清零）。
            let c_start = idx * chunk_size;
            let c_end = ((idx + 1) * chunk_size).min(total);
            if let Some((off, len)) = write {
                if c_start >= off && c_end <= off + len {
                    continue;
                }
            }
            self.ensure_chunk(ino, idx)?;
        }
        Ok(())
    }

    /// 删除文件/目录（`unlink`/`rmdir` 共用）。
    fn remove_entry(&self, parent: INodeNo, name: &OsStr, is_dir: bool, reply: ReplyEmpty) {
        if self.read_only {
            return reply.error(fuser::Errno::EROFS);
        }
        let Some(name) = name.to_str() else {
            return reply.error(fuser::Errno::EINVAL);
        };
        let (parent_remote, _ino, dirty) = {
            let g = self.inner.lock().unwrap();
            let Some(p) = g.nodes.get(&parent) else {
                return reply.error(fuser::Errno::ENOENT);
            };
            let remote = join_path(&p.remote, name);
            let ino = g.by_remote.get(&remote).copied();
            let dirty = ino
                .and_then(|i| g.nodes.get(&i))
                .map(|n| n.dirty)
                .unwrap_or(false);
            (p.remote.clone(), ino, dirty)
        };
        if dirty {
            if let Some(q) = &self.upload {
                if !q.drain(Duration::from_secs(30)) {
                    tracing::warn!("删除前排空上传队列超时: {parent_remote}/{name}");
                    return reply.error(fuser::Errno::EBUSY);
                }
            }
        }
        let client = self.client.clone();
        let (dir, n) = (parent_remote.clone(), name.to_string());
        if let Err(e) = self
            .rt
            .block_on(async move { client.delete_entry(&dir, &n).await })
        {
            tracing::warn!("delete 失败 {parent_remote}/{name}: {e}");
            return reply.error(fuser::Errno::EIO);
        }
        {
            let mut g = self.inner.lock().unwrap();
            let remote = join_path(&parent_remote, name);
            if let Some(i) = g.by_remote.remove(&remote) {
                g.nodes.remove(&i);
            }
        }
        tracing::debug!(
            "{} {}",
            if is_dir { "rmdir" } else { "unlink" },
            join_path(&parent_remote, name)
        );
        reply.ok();
    }

    /// 标记节点为脏并入队上传（写路径的统一出口）。
    fn mark_dirty(&self, ino: INodeNo) -> Result<(), fuser::Errno> {
        let queue = match &self.upload {
            Some(q) => q.clone(),
            None => return Err(fuser::Errno::EROFS),
        };
        let (remote, name, mtime, cache) = {
            let mut g = self.inner.lock().unwrap();
            let n = g.nodes.get_mut(&ino).ok_or(fuser::Errno::ENOENT)?;
            n.dirty = true;
            let cache = n.cache.clone().ok_or(fuser::Errno::EIO)?;
            (
                n.remote.clone(),
                n.name.clone(),
                Self::epoch_of(n.attr.mtime),
                cache,
            )
        };
        let job = UploadJob {
            remote_dir: self.remote_dir_of(&remote),
            remote_name: name,
            local: cache,
            mtime,
            attempts: 0,
        };
        queue.enqueue(job).map_err(|e| {
            tracing::error!("入队上传失败: {e}");
            fuser::Errno::EIO
        })?;
        tracing::debug!("已入队上传: {remote} (mtime={mtime})");
        Ok(())
    }

    /// 缓存文件（懒创建）：**apparent size = 文件大小**，用 `set_len` 造稀疏文件。
    fn cache_file_for(&self, ino: INodeNo) -> Result<PathBuf, fuser::Errno> {
        let mut g = self.inner.lock().unwrap();
        let chunk_size = self.chunk_size;
        let n = g.nodes.get_mut(&ino).ok_or(fuser::Errno::ENOENT)?;
        if let Some(p) = &n.cache {
            return Ok(p.clone());
        }
        let remote = n.remote.clone();
        let path = self.cache_path(&remote, &n.name);
        let f = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| {
                tracing::error!("创建缓存文件失败 {}: {e}", path.display());
                fuser::Errno::EIO
            })?;
        f.set_len(n.attr.size).map_err(|e| {
            tracing::error!("设置缓存文件大小失败 {}: {e}", path.display());
            fuser::Errno::EIO
        })?;
        n.chunks_done = vec![false; n.chunk_count(chunk_size)];
        n.cache = Some(path.clone());
        Ok(path)
    }

    fn chunk_ready(&self, ino: INodeNo, idx: u64) -> bool {
        let g = self.inner.lock().unwrap();
        g.nodes
            .get(&ino)
            .and_then(|n| n.chunks_done.get(idx as usize).copied())
            .unwrap_or(false)
    }

    /// ★ M2：确保 `[offset, offset+len)` 覆盖的区间都已就绪，返回缓存文件路径。
    fn ensure_range(&self, ino: INodeNo, offset: u64, len: u64) -> Result<PathBuf, fuser::Errno> {
        let total = {
            let g = self.inner.lock().unwrap();
            g.nodes.get(&ino).ok_or(fuser::Errno::ENOENT)?.attr.size
        };
        let cache = self.cache_file_for(ino)?;
        if total == 0 || len == 0 {
            return Ok(cache);
        }
        let (first, last) = chunk_indices(offset, len, self.chunk_size);
        for idx in first..=last {
            self.ensure_chunk(ino, idx)?;
        }
        Ok(cache)
    }

    /// 下载单个 128 KiB 区间：single-flight + 超时 + 长度校验 + `pwrite` 到稀疏缓存。
    fn ensure_chunk(&self, ino: INodeNo, idx: u64) -> Result<(), fuser::Errno> {
        if self.chunk_ready(ino, idx) {
            return Ok(());
        }
        let key = (u64::from(ino), idx);
        let cell = {
            let mut g = self.inner.lock().unwrap();
            g.inflight_chunks
                .entry(key)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _guard = cell.lock().unwrap();
        // 拿到锁后再查一次（别人可能刚下完）
        if self.chunk_ready(ino, idx) {
            return Ok(());
        }

        let (remote, total, name, dest) = {
            let g = self.inner.lock().unwrap();
            let n = g.nodes.get(&ino).ok_or(fuser::Errno::ENOENT)?;
            (
                n.remote.clone(),
                n.attr.size,
                n.name.clone(),
                n.cache.clone().ok_or(fuser::Errno::EIO)?,
            )
        };
        let dir = remote
            .rsplit_once('/')
            .map(|(d, _)| d.to_string())
            .unwrap_or_else(|| self.remote_root.clone());

        let start = idx * self.chunk_size;
        let end = (start + self.chunk_size).min(total) - 1; // 闭区间，末块按文件尾截断
        let want = end - start + 1;

        let client = self.client.clone();
        let (d2, n2) = (dir.clone(), name.clone());
        let timeout = self.hydrate_timeout;
        let res = self.rt.block_on(async move {
            tokio::time::timeout(timeout, client.download_range(&d2, &n2, start, end)).await
        });

        let data = match res {
            Err(_) => {
                tracing::warn!("区间水合超时({timeout:?}): {remote} [{start}..={end}]");
                self.inner.lock().unwrap().inflight_chunks.remove(&key);
                return Err(fuser::Errno::EIO);
            }
            Ok(Err(e)) => {
                tracing::warn!("区间水合失败: {remote} [{start}..={end}]: {e}");
                self.inner.lock().unwrap().inflight_chunks.remove(&key);
                return Err(fuser::Errno::EIO);
            }
            Ok(Ok(d)) => d,
        };

        // ★ 长度校验：区间字节数必须与请求一致，否则不写进缓存（铁则 1 的源头把关）
        if data.len() as u64 != want {
            tracing::error!(
                "区间长度不符: {remote} [{start}..={end}] 期望 {want} 实得 {}",
                data.len()
            );
            self.inner.lock().unwrap().inflight_chunks.remove(&key);
            return Err(fuser::Errno::EIO);
        }

        {
            use std::os::unix::fs::FileExt;
            let f = std::fs::OpenOptions::new()
                .write(true)
                .open(&dest)
                .map_err(|_| fuser::Errno::EIO)?;
            f.write_all_at(&data, start).map_err(|e| {
                tracing::error!("写缓存失败 {} @{start}: {e}", dest.display());
                fuser::Errno::EIO
            })?;
        }

        {
            let mut g = self.inner.lock().unwrap();
            if let Some(n) = g.nodes.get_mut(&ino) {
                if let Some(slot) = n.chunks_done.get_mut(idx as usize) {
                    *slot = true;
                }
            }
            g.inflight_chunks.remove(&key);
        }
        self.hydro.record(data.len() as u64);
        tracing::debug!("区间就绪: {remote} [{start}..={end}] ({want} 字节)");
        Ok(())
    }
}

/// 拼远端路径。
fn join_path(dir: &str, name: &str) -> String {
    format!("{}/{}", dir.trim_end_matches('/'), name)
}

/// FNV-1a 64 位：实现简单、跨版本稳定（不像 `DefaultHasher` 那样无保证）。
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// `[offset, offset+len)` 覆盖的区间下标闭区间。
fn chunk_indices(offset: u64, len: u64, chunk_size: u64) -> (u64, u64) {
    let first = offset / chunk_size;
    let last = if len == 0 {
        first
    } else {
        (offset + len - 1) / chunk_size
    };
    (first, last)
}

/// 从稀疏缓存里精确读一段；**不足就报 EIO**（铁则 1）。
fn read_exact_at(path: &Path, offset: u64, size: usize) -> Result<Vec<u8>, fuser::Errno> {
    use std::os::unix::fs::FileExt;
    let f = std::fs::File::open(path).map_err(|_| fuser::Errno::EIO)?;
    let mut buf = vec![0u8; size];
    f.read_exact_at(&mut buf, offset).map_err(|e| {
        // 调用方已确保这段区间在文件范围内，读不满就是错误
        tracing::error!(
            "缓存短读: {} offset={offset} want={size}: {e}",
            path.display()
        );
        fuser::Errno::EIO
    })?;
    Ok(buf)
}

impl Filesystem for QxyncFs {
    fn init(&mut self, _req: &Request, config: &mut fuser::KernelConfig) -> std::io::Result<()> {
        // 与 Qsync 的 128 KiB 水合粒度对齐（M2 已按这个粒度发 Range 请求）
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
        // 只读挂载时拒绝任何写意图；读写挂载下放行（写前的 read-modify-write 在 write/setattr 里做）
        if self.read_only && flags.acc_mode() != OpenAccMode::O_RDONLY {
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
        // 请求范围若越过文件尾，只需返回实际存在的部分（这是 EOF，不是短读）
        let want = (size as u64).min(file_size - offset);
        // ★ M2：只取这段需要的 128 KiB 区间（single-flight + 超时）
        let path = match self.ensure_range(ino, offset, want) {
            Ok(p) => p,
            Err(e) => return reply.error(e),
        };
        match read_exact_at(&path, offset, want as usize) {
            Ok(buf) => reply.data(&buf),
            Err(e) => reply.error(e),
        }
    }

    // ------------------------------------------------------------ M2b 写路径

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: fuser::FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        use std::os::unix::fs::FileExt;
        if self.read_only {
            return reply.error(fuser::Errno::EROFS);
        }
        // ★ read-modify-write：先把会被整块覆盖之外的区间补齐，
        //   否则未取回的区间是 0，整文件上传会把远端内容清零。
        if let Err(e) = self.hydrate_all(ino, Some((offset, data.len() as u64))) {
            return reply.error(e);
        }
        let path = match self.cache_file_for(ino) {
            Ok(p) => p,
            Err(e) => return reply.error(e),
        };
        let f = match std::fs::OpenOptions::new().write(true).open(&path) {
            Ok(f) => f,
            Err(e) => {
                tracing::error!("打开缓存写失败 {}: {e}", path.display());
                return reply.error(fuser::Errno::EIO);
            }
        };
        let end = offset + data.len() as u64;
        let cur = {
            let g = self.inner.lock().unwrap();
            g.nodes.get(&ino).map(|n| n.attr.size).unwrap_or(0)
        };
        if end > cur {
            if f.set_len(end).is_err() {
                return reply.error(fuser::Errno::EIO);
            }
            let mut g = self.inner.lock().unwrap();
            if let Some(n) = g.nodes.get_mut(&ino) {
                n.attr.size = end;
                n.attr.blocks = end.div_ceil(512);
            }
        }
        if f.write_all_at(data, offset).is_err() {
            return reply.error(fuser::Errno::EIO);
        }
        let now = SystemTime::now();
        {
            let mut g = self.inner.lock().unwrap();
            if let Some(n) = g.nodes.get_mut(&ino) {
                n.attr.mtime = now;
                n.attr.ctime = now;
            }
        }
        if let Err(e) = self.mark_dirty(ino) {
            return reply.error(e);
        }
        reply.written(data.len() as u32);
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<fuser::FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        if self.read_only && (size.is_some() || mtime.is_some()) {
            return reply.error(fuser::Errno::EROFS);
        }
        let mut size_changed = false;

        if let Some(new_size) = size {
            let cur = {
                let g = self.inner.lock().unwrap();
                match g.nodes.get(&ino) {
                    Some(n) => n.attr.size,
                    None => return reply.error(fuser::Errno::ENOENT),
                }
            };
            if new_size != cur {
                // 截断到 0 不需要旧内容；否则要先补齐（未取回区间是 0，直接改大小会丢数据）
                if new_size > 0 {
                    if let Err(e) = self.hydrate_all(ino, None) {
                        return reply.error(e);
                    }
                }
                let path = match self.cache_file_for(ino) {
                    Ok(p) => p,
                    Err(e) => return reply.error(e),
                };
                let f = match std::fs::OpenOptions::new().write(true).open(&path) {
                    Ok(f) => f,
                    Err(_) => return reply.error(fuser::Errno::EIO),
                };
                if f.set_len(new_size).is_err() {
                    return reply.error(fuser::Errno::EIO);
                }
                let mut g = self.inner.lock().unwrap();
                if let Some(n) = g.nodes.get_mut(&ino) {
                    n.attr.size = new_size;
                    n.attr.blocks = new_size.div_ceil(512);
                }
                size_changed = true;
            }
        }

        if let Some(t) = mtime {
            let ts = match t {
                TimeOrNow::Now => SystemTime::now(),
                TimeOrNow::SpecificTime(st) => st,
            };
            {
                let mut g = self.inner.lock().unwrap();
                if let Some(n) = g.nodes.get_mut(&ino) {
                    n.attr.mtime = ts;
                }
            }
            // 只改 mtime（例如 `touch`）：不必整文件上传，直接推服务端 mtime 即可
            if !size_changed {
                let (remote, dir, name, epoch) = {
                    let g = self.inner.lock().unwrap();
                    match g.nodes.get(&ino) {
                        Some(n) => (
                            n.remote.clone(),
                            self.remote_dir_of(&n.remote),
                            n.name.clone(),
                            Self::epoch_of(ts),
                        ),
                        None => return reply.error(fuser::Errno::ENOENT),
                    }
                };
                let client = self.client.clone();
                let res = self
                    .rt
                    .block_on(async move { client.set_mtime(&dir, &name, epoch).await });
                if let Err(e) = res {
                    tracing::warn!("set_mtime 远端失败 {remote}: {e}");
                }
            }
        }

        if size_changed {
            if let Err(e) = self.mark_dirty(ino) {
                return reply.error(e);
            }
        }

        let attr = {
            let g = self.inner.lock().unwrap();
            g.nodes.get(&ino).map(|n| n.attr)
        };
        match attr {
            Some(a) => reply.attr(&ENTRY_TTL, &a),
            None => reply.error(fuser::Errno::ENOENT),
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        if self.read_only {
            return reply.error(fuser::Errno::EROFS);
        }
        let Some(name) = name.to_str() else {
            return reply.error(fuser::Errno::EINVAL);
        };
        let parent_remote = {
            let g = self.inner.lock().unwrap();
            match g.nodes.get(&parent) {
                Some(n) => n.remote.clone(),
                None => return reply.error(fuser::Errno::ENOENT),
            }
        };
        let remote = join_path(&parent_remote, name);
        let entry = DirEntry::local(name, false, 0, Self::epoch_of(SystemTime::now()));
        let node = self.insert_node(parent, name, &remote, &entry);
        if let Err(e) = self.cache_file_for(node.ino) {
            return reply.error(e);
        }
        if let Err(e) = self.mark_dirty(node.ino) {
            return reply.error(e);
        }
        tracing::debug!("create {remote}");
        reply.created(
            &ENTRY_TTL,
            &node.attr,
            Generation(0),
            fuser::FileHandle(u64::from(node.ino)),
            FopenFlags::FOPEN_KEEP_CACHE,
        );
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        if self.read_only {
            return reply.error(fuser::Errno::EROFS);
        }
        let Some(name) = name.to_str() else {
            return reply.error(fuser::Errno::EINVAL);
        };
        let parent_remote = {
            let g = self.inner.lock().unwrap();
            match g.nodes.get(&parent) {
                Some(n) => n.remote.clone(),
                None => return reply.error(fuser::Errno::ENOENT),
            }
        };
        let client = self.client.clone();
        let (p, n) = (parent_remote.clone(), name.to_string());
        if let Err(e) = self.rt.block_on(async move { client.mkdir(&p, &n).await }) {
            tracing::warn!("mkdir 失败 {parent_remote}/{name}: {e}");
            return reply.error(fuser::Errno::EIO);
        }
        let remote = join_path(&parent_remote, name);
        let entry = DirEntry::local(name, true, 0, Self::epoch_of(SystemTime::now()));
        let node = self.insert_node(parent, name, &remote, &entry);
        reply.entry(&ENTRY_TTL, &node.attr, Generation(0));
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.remove_entry(parent, name, false, reply);
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.remove_entry(parent, name, true, reply);
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        if self.read_only {
            return reply.error(fuser::Errno::EROFS);
        }
        let (Some(name), Some(newname)) = (name.to_str(), newname.to_str()) else {
            return reply.error(fuser::Errno::EINVAL);
        };
        let (old_remote, new_remote, parent_remote, newparent_remote, ino, dirty) = {
            let g = self.inner.lock().unwrap();
            let (Some(p), Some(np)) = (g.nodes.get(&parent), g.nodes.get(&newparent)) else {
                return reply.error(fuser::Errno::ENOENT);
            };
            let old = join_path(&p.remote, name);
            let new = join_path(&np.remote, newname);
            let ino = g.by_remote.get(&old).copied();
            let dirty = ino
                .and_then(|i| g.nodes.get(&i))
                .map(|n| n.dirty)
                .unwrap_or(false);
            (old, new, p.remote.clone(), np.remote.clone(), ino, dirty)
        };

        // 有未上传的改动时先冲刷：否则队列里的作业还指着旧名字，会和改名打架
        if dirty {
            if let Some(q) = &self.upload {
                if !q.drain(Duration::from_secs(60)) {
                    tracing::warn!("改名前排空上传队列超时: {old_remote}");
                    return reply.error(fuser::Errno::EBUSY);
                }
            }
        }

        let client = self.client.clone();
        let res = if parent_remote == newparent_remote {
            // 同目录：FileStation rename（实测 body: path/source_name/dest_name；大小写改名可直接成功）
            let (dir, from, to) = (parent_remote.clone(), name.to_string(), newname.to_string());
            self.rt
                .block_on(async move { client.rename(&dir, &from, &to).await })
        } else {
            // 跨目录：FileStation move 会**忽略 dest_file**（保持原名），
            // 所以先搬过去，需要改名再在目标目录里 rename 一次。
            let (fd, nn, td, tn) = (
                parent_remote.clone(),
                name.to_string(),
                newparent_remote.clone(),
                newname.to_string(),
            );
            let r = self
                .rt
                .block_on(async { client.move_into(&fd, &nn, &td).await });
            if r.is_ok() && tn != nn {
                let client2 = self.client.clone();
                let (td2, nn2) = (td.clone(), nn.clone());
                self.rt
                    .block_on(async move { client2.rename(&td2, &nn2, &tn).await })
            } else {
                r
            }
        };
        if let Err(e) = res {
            tracing::warn!("rename 失败 {old_remote} -> {new_remote}: {e}");
            return reply.error(fuser::Errno::EIO);
        }

        if let Some(ino) = ino {
            let mut g = self.inner.lock().unwrap();
            g.by_remote.remove(&old_remote);
            g.by_remote.insert(new_remote.clone(), ino);
            if let Some(n) = g.nodes.get_mut(&ino) {
                n.name = newname.to_string();
                n.remote = new_remote.clone();
                n.parent = newparent;
            }
        }
        tracing::debug!("rename {old_remote} -> {new_remote}");
        reply.ok();
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
        let (state, remote, fsize, chunks) = {
            let g = self.inner.lock().unwrap();
            match g.nodes.get(&ino) {
                Some(n) => {
                    let done = n.chunks_done.iter().filter(|d| **d).count();
                    (
                        n.state_str(),
                        n.remote.clone(),
                        n.attr.size,
                        format!("{done}/{}", n.chunks_done.len()),
                    )
                }
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
            // 占位符状态：placeholder（未取任何区间）/ partial（取了一部分）/ hydrated（全取）
            "user.qsync.state" => Some(state.into()),
            // 已就绪区间数 / 总区间数（M2 的可观测性）
            "user.qsync.chunks" => Some(chunks),
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
        let names = "user.qsync.state\0user.qsync.pin\0user.qsync.remote\0user.qsync.vsize\0user.qsync.chunks\0";
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

/// 挂载参数：`-o ro` + 推荐选项（报告 12 §6.3）。
///
/// `auto_unmount` 在 fuser 0.17 里要求 `SessionACL != Owner`（即 `allow_other`），
/// 非特权挂载还需要 `/etc/fuse.conf` 里的 `user_allow_other`；因此做成开关，默认关闭。
pub fn mount_options(auto_unmount: bool, read_only: bool) -> Vec<fuser::MountOption> {
    use fuser::MountOption::*;
    let mut opts = vec![
        if read_only { RO } else { RW },
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
pub fn mount_config(n_threads: usize, auto_unmount: bool, read_only: bool) -> Config {
    // `Config` 是 #[non_exhaustive]，外部 crate 不能写字面量，只能 default + 逐字段赋值
    let mut cfg = Config::default();
    cfg.mount_options = mount_options(auto_unmount, read_only);
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
    read_only: bool,
) -> std::io::Result<()> {
    fuser::mount2(
        fs,
        mountpoint,
        &mount_config(n_threads, auto_unmount, read_only),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_indices_covers_range() {
        let c = 128 * 1024;
        // 文件头 100 字节 → 只碰第 0 块（这就是 head -c 100 只下 128 KiB 的原因）
        assert_eq!(chunk_indices(0, 100, c), (0, 0));
        // 正好一块
        assert_eq!(chunk_indices(0, c, c), (0, 0));
        // 跨块
        assert_eq!(chunk_indices(c - 1, 2, c), (0, 1));
        assert_eq!(chunk_indices(c, 1, c), (1, 1));
        // 尾部
        assert_eq!(chunk_indices(3 * c + 7, 10, c), (3, 3));
    }

    #[test]
    fn node_state_strings() {
        let attr = FileAttr {
            ino: INodeNo(9),
            size: 300 * 1024,
            blocks: 0,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: FileType::RegularFile,
            perm: 0o644,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        };
        let mut n = Node {
            ino: INodeNo(9),
            parent: INodeNo::ROOT,
            name: "a.bin".into(),
            remote: "/home/a.bin".into(),
            attr,
            cache: None,
            chunks_done: vec![false; 3],
            dirty: false,
        };
        assert_eq!(n.state_str(), "placeholder");
        n.chunks_done[0] = true;
        assert_eq!(n.state_str(), "partial");
        n.chunks_done = vec![true; 3];
        assert_eq!(n.state_str(), "hydrated");
        assert_eq!(n.chunk_count(DEFAULT_CHUNK_SIZE), 3);
    }

    #[test]
    fn stable_hash_does_not_collide_for_similar_paths() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_ne!(fnv1a64(b"/home/a.txt"), fnv1a64(b"/home/b.txt"));
        assert_ne!(
            fnv1a64(b"/home/qxync-test/hello.txt"),
            fnv1a64(b"/home/qxync-test/hellp.txt")
        );
        // 稳定：同一输入两次一致
        assert_eq!(fnv1a64(b"/home/x"), fnv1a64(b"/home/x"));
    }

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
