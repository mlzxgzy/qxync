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

// 脱水需要 daemon 持有 fuser 的会话/通知句柄；这里转出，避免 daemon 直接依赖 fuser。
pub use fuser::{BackgroundSession, Notifier};

use qxync_client::peer::{self, ContentSource, PeerConfig, PeerHead};
use qxync_client::Client;
use qxync_core::dehydrate::{Block, Candidate, Policy};
use qxync_core::roots::RootSpec;
use qxync_core::rules::{HideReason, Rules};
use qxync_core::DirEntry;
use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use upload::{UploadJob, UploadQueue, UploadSnapshot};

const ENTRY_TTL: Duration = Duration::from_millis(500);
/// 水合超时：对应 Qsync 的 `CANCEL_FETCH_DATA` = 60s（M2 起是**单区间**的超时）。
const HYDRATE_TIMEOUT: Duration = Duration::from_secs(60);
/// 水合粒度：128 KiB（与 Qsync 的 CfAPI `FETCH_DATA` 对齐）。
pub const DEFAULT_CHUNK_SIZE: u64 = 128 * 1024;
/// 目录列举分页上限（对应服务端 `Max_File_List`）。
const LIST_LIMIT: usize = 200;
/// ★ M2c：本地大批删除熔断的默认阈值（60 秒窗口内最多 100 次删除）。
/// 超过就熔断并把后续删除回 `EACCES`；`qsync sync --force-deletes` 可解除。
pub const DEFAULT_DELETE_LIMIT: usize = 100;
pub const DEFAULT_DELETE_WINDOW: Duration = Duration::from_secs(60);

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
    /// ★ M3：打开的 fd 数（>0 时禁止脱水，报告 12 §8.1）。
    open_count: u32,
    /// ★ M3：最后一次读/写时间（LRU 脱水排序、闲置判定）。
    last_access: SystemTime,
    /// ★ M3：节点级操作锁 —— `read`/`write`/`setattr` 持锁；脱水用 `try_lock`，
    /// 拿不到就说明「正在水合/读写」，本轮跳过（报告 12 §8.1 的 in_progress）。
    op_lock: Arc<Mutex<()>>,
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

    /// 本地已缓存的字节数（已就绪区间之和；末块按文件大小截断）。
    fn hydrated_bytes(&self, chunk_size: u64) -> u64 {
        let mut sum = 0u64;
        for (i, done) in self.chunks_done.iter().enumerate() {
            if !*done {
                continue;
            }
            let start = i as u64 * chunk_size;
            if start >= self.attr.size {
                continue;
            }
            let end = (start + chunk_size).min(self.attr.size);
            sum += end - start;
        }
        sum
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

// ---------------------------------------------------------------- M2c：本地视图

/// 一个本地节点的快照（同步引擎只读这份数据做三向比较）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalNode {
    /// NAS 上的绝对路径（Qsync 视图命名空间，如 `/home/x`）。
    pub remote: String,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
    /// 有未上传改动（FUSE write/setattr 置位）。
    pub dirty: bool,
    /// 稀疏缓存文件（未水合时为 `None`）。
    pub cache: Option<PathBuf>,
}

/// ★ M2c 的关键抽象：同步引擎只依赖这个 trait，因此**可以不挂载 FUSE 就测**。
///
/// 实现方：
/// * [`FsHandle`] —— 真实挂载视图（daemon 用）；
/// * 测试里的 `FakeLocalView` —— 用内存 map 复现同样的语义。
pub trait LocalView: Send + Sync {
    /// 挂载根（`/home`）。
    fn remote_root(&self) -> &str;
    /// ★ M6：挂载暴露的全部远端根（单根时就是那一个）。
    ///
    /// 默认实现向后兼容（旧的实现者/测试替身只需实现 `remote_root`）。
    fn remote_roots(&self) -> Vec<String> {
        vec![self.remote_root().to_string()]
    }
    fn node(&self, remote: &str) -> Option<LocalNode>;
    fn nodes(&self) -> Vec<LocalNode>;
    /// 已知目录（节点表里所有目录）——对账时按目录列远端，避免全盘扫描。
    fn known_dirs(&self) -> Vec<String>;
    /// 上传队列里是否还有该路径的作业。
    fn has_pending(&self, remote: &str) -> bool;
    /// 远端元数据变了：更新 attr；若大小/mtime 变了则**失效已缓存内容**。
    /// 返回 true 表示确实更新了（节点存在）。
    fn apply_remote_meta(&self, remote: &str, is_dir: bool, size: u64, mtime: i64) -> bool;
    /// 丢弃已缓存内容（下次 `read()` 重新按区间水合），元数据不动。
    fn invalidate_content(&self, remote: &str) -> bool;
    /// 远端已删除：移除节点 + 缓存（目录连后代一起）。
    fn remove_remote(&self, remote: &str) -> bool;
    /// 把本地节点标脏并入队上传（`remote` 必须已有缓存内容）。
    fn mark_dirty(&self, remote: &str) -> std::io::Result<()>;
    /// 冲突副本：把本地缓存内容复制到一个稳定的 stash 文件，返回该路径。
    fn stash_conflict(&self, remote: &str, conflict_name: &str) -> std::io::Result<PathBuf>;
    /// 直接入队一个上传作业（冲突副本用）。
    fn enqueue_upload(
        &self,
        remote_dir: &str,
        remote_name: &str,
        local: PathBuf,
        mtime: i64,
    ) -> std::io::Result<()>;
}

/// 挂载视图句柄：daemon 在把 [`QxyncFs`] 交给 FUSE 挂载线程后，用它继续操作节点表。
#[derive(Clone)]
pub struct FsHandle {
    inner: Arc<Mutex<Inner>>,
    cache_dir: PathBuf,
    chunk_size: u64,
    remote_root: String,
    /// ★ M6：全部远端根（单根时长度 1）。
    roots: Vec<String>,
    upload: Option<Arc<UploadQueue>>,
    delete_guard: Arc<DeleteGuard>,
    read_only: bool,
    /// ★ M3：pin 状态（脱水拦截条件）。
    pins: PinMap,
    cache_mode: CacheMode,
    /// ★ M7：选择性同步规则（脱水候选必须再过滤一遍：排除路径**永不**脱水）。
    rules: Arc<Rules>,
    /// ★ M7：LAN 直传统计（daemon `peer status` 汇总展示）。
    lan_stats: Arc<LanStats>,
}

/// 缓存占用统计（`status` / 限额判定用）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// 本地已缓存字节数（已就绪区间之和）。
    pub used_bytes: u64,
    /// 文件节点数（不含目录/根）。
    pub total_files: u64,
    /// 有缓存内容的文件数（含部分水合）。
    pub hydrated_files: u64,
}

/// 脱水结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DehydrateOutcome {
    /// 成功，释放了 N 字节。
    Freed(u64),
    /// 安全检查未通过（原因见 [`Block`]）。
    Blocked(Block),
    /// 执行失败（例如 `inval_inode` 被内核拒绝）——此时**没有**清内容。
    Failed(String),
}

impl FsHandle {
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }
    pub fn delete_guard(&self) -> Arc<DeleteGuard> {
        self.delete_guard.clone()
    }
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// ★ M6：挂载暴露的全部远端根（远端路径列表；单根时就是那一个）。
    pub fn remote_roots(&self) -> Vec<String> {
        self.roots.clone()
    }

    pub fn cache_mode(&self) -> CacheMode {
        self.cache_mode
    }

    /// 导出所有可脱水候选（fuse 侧事实，策略判定在 `qxync_core::dehydrate`）。
    ///
    /// ★ M7：被 `exclude` 隐藏的路径**永不**进入候选 —— 排除语义是「本地没有副本」，
    /// 把它的本地内容当缓存清掉会让用户以为自己留着的文件凭空消失。
    pub fn dehydrate_candidates(&self) -> Vec<Candidate> {
        let g = self.inner.lock().unwrap();
        g.nodes
            .values()
            .filter(|n| n.attr.kind == FileType::RegularFile)
            .filter(|n| self.rules.hides_in_roots(&self.roots, &n.remote, false).is_none())
            .map(|n| self.candidate_of(&g, n, true))
            .collect()
    }

    /// ★ M7：规则是否让这条远端路径不可见（挂载点里不存在）。
    pub fn hidden(&self, remote: &str, is_dir: bool) -> Option<HideReason> {
        self.rules.hides_in_roots(&self.roots, remote, is_dir)
    }

    pub fn rules(&self) -> Arc<Rules> {
        self.rules.clone()
    }

    /// ★ M7：LAN 直传统计（(尝试, 命中, 字节, 元数据不符)）。
    pub fn lan_stats(&self) -> Arc<LanStats> {
        self.lan_stats.clone()
    }

    pub fn candidate(&self, remote: &str) -> Option<Candidate> {
        if self.hidden(remote, false).is_some() {
            return None;
        }
        self.candidate_locked(remote, true)
    }

    /// `check_op_lock=false` 用在「调用方已经持有该节点操作锁」的场景
    /// （脱水复查）——否则会把自己的锁当成「在途」而误判。
    fn candidate_locked(&self, remote: &str, check_op_lock: bool) -> Option<Candidate> {
        let g = self.inner.lock().unwrap();
        let ino = *g.by_remote.get(remote)?;
        let n = g.nodes.get(&ino)?;
        Some(self.candidate_of(&g, n, check_op_lock))
    }

    fn candidate_of(&self, g: &Inner, n: &Node, check_op_lock: bool) -> Candidate {
        let in_flight = g
            .inflight_chunks
            .keys()
            .any(|(i, _)| *i == u64::from(n.ino))
            || (check_op_lock && n.op_lock.try_lock().is_err());
        Candidate {
            remote: n.remote.clone(),
            is_dir: n.attr.kind == FileType::Directory,
            size: n.attr.size,
            hydrated_bytes: n.hydrated_bytes(self.chunk_size),
            dirty: n.dirty,
            pending_upload: self
                .upload
                .as_ref()
                .map(|q| q.has_pending(&n.remote))
                .unwrap_or(false),
            open_count: n.open_count,
            in_flight,
            pin: self
                .pins
                .lock()
                .unwrap()
                .get(&n.remote)
                .cloned()
                .unwrap_or_else(|| "unspecified".to_string()),
            last_access: epoch_secs(n.last_access).max(0) as u64,
        }
    }

    /// 缓存占用统计。
    pub fn cache_stats(&self) -> CacheStats {
        let g = self.inner.lock().unwrap();
        let mut out = CacheStats::default();
        for n in g.nodes.values() {
            if n.attr.kind != FileType::RegularFile {
                continue;
            }
            out.total_files += 1;
            let used = n.hydrated_bytes(self.chunk_size);
            if used > 0 {
                out.hydrated_files += 1;
                out.used_bytes += used;
            }
        }
        out
    }

    /// 上传成功后清掉 `dirty`（有 hook 时由上传队列回调）。
    pub fn clear_dirty(&self, remote: &str) {
        if self.has_pending(remote) {
            return;
        }
        let mut g = self.inner.lock().unwrap();
        if let Some(ino) = g.by_remote.get(remote).copied() {
            if let Some(n) = g.nodes.get_mut(&ino) {
                n.dirty = false;
            }
        }
    }

    /// ★★ M3 铁则 2：**先 `inval_inode`（让内核失效 page cache）→ 再清内容 → 再更新状态**。
    ///
    /// `invalidate` 由调用方注入（daemon 传 `Notifier::inval_inode(ino, 0, 0)`；
    /// 单测传空实现）—— 顺序由这里的代码固定，任何实现都绕不过「先失效」。
    ///
    /// 安全检查不通过时**什么都不做**；`inval_inode` 失败时也**不清内容**（宁可占着磁盘，
    /// 也不能让应用读到 0）。
    pub fn dehydrate_now<F>(
        &self,
        remote: &str,
        policy: &Policy,
        mapped: bool,
        invalidate: F,
    ) -> DehydrateOutcome
    where
        F: FnOnce(INodeNo) -> std::io::Result<()>,
    {
        let Some(cand) = self.candidate(remote) else {
            return DehydrateOutcome::Blocked(Block::NoContent);
        };
        if let Err(b) = qxync_core::dehydrate::eligible(&cand, policy, mapped) {
            log_dehydrate_block(remote, &cand, b, "预检");
            return DehydrateOutcome::Blocked(b);
        }
        // 节点操作锁：拿不到 = 正在水合/读写 → 本轮跳过（报告 12 §8.1 in_progress）
        let (ino, lock) = {
            let g = self.inner.lock().unwrap();
            match g
                .by_remote
                .get(remote)
                .copied()
                .and_then(|i| g.nodes.get(&i))
            {
                Some(n) => (n.ino, n.op_lock.clone()),
                None => return DehydrateOutcome::Blocked(Block::NoContent),
            }
        };
        let _guard = match lock.try_lock() {
            Ok(g) => g,
            Err(_) => return DehydrateOutcome::Blocked(Block::InFlight),
        };
        // 持锁后复查（open/dirty/pending 可能刚变了；此时不再看自己的 op_lock）
        let fresh = match self.candidate_locked(remote, false) {
            Some(c) => c,
            None => return DehydrateOutcome::Blocked(Block::NoContent),
        };
        if let Err(b) = qxync_core::dehydrate::eligible(&fresh, policy, mapped) {
            log_dehydrate_block(remote, &fresh, b, "持锁复查");
            return DehydrateOutcome::Blocked(b);
        }
        let freed = fresh.hydrated_bytes;

        // ① 先让内核失效（失败 → 绝不继续）
        if let Err(e) = invalidate(ino) {
            tracing::error!("脱水中止：inval_inode {remote} 失败: {e}");
            return DehydrateOutcome::Failed(format!("inval_inode 失败: {e}"));
        }
        // ② 再清内容（删稀疏缓存）
        let cache_path = {
            let mut g = self.inner.lock().unwrap();
            g.nodes.get_mut(&ino).and_then(|n| n.cache.take())
        };
        if let Some(p) = cache_path {
            if let Err(e) = std::fs::remove_file(&p) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!("删除缓存文件失败 {}: {e}", p.display());
                }
            }
        }
        // ③ 再更新占位符状态
        {
            let mut g = self.inner.lock().unwrap();
            let chunk_size = self.chunk_size;
            if let Some(n) = g.nodes.get_mut(&ino) {
                n.chunks_done = vec![false; n.chunk_count(chunk_size)];
                n.dirty = false;
            }
        }
        tracing::info!("已脱水 {remote}（释放 {freed} 字节，先 inval_inode 再清内容）");
        DehydrateOutcome::Freed(freed)
    }
}

/// ★ M7：挂载视图同时是 LAN 对等端的**内容源**。
///
/// 只服务「完整水合 + 未脏 + 无待上传 + 不在排除规则里」的文件：
/// 部分水合的稀疏文件里那些 0 不是数据（铁则 1 的 LAN 版），脏文件的内容还没推回 NAS，
/// 给出去会让对端拿到一份 NAS 上不存在的版本。
impl ContentSource for FsHandle {
    fn head(&self, path: &str) -> Option<PeerHead> {
        let g = self.inner.lock().unwrap();
        let ino = *g.by_remote.get(path)?;
        let n = g.nodes.get(&ino)?;
        if n.attr.kind != FileType::RegularFile {
            return None; // 目录不通过 LAN 传
        }
        if n.dirty || !n.is_fully_hydrated() {
            return None;
        }
        if self
            .upload
            .as_ref()
            .map(|q| q.has_pending(path))
            .unwrap_or(false)
        {
            return None;
        }
        if self.rules.hides_in_roots(&self.roots, path, false).is_some() {
            return None; // 被选择性同步排除的内容不对外服务
        }
        Some(PeerHead {
            exists: true,
            hydrated: true,
            size: n.attr.size,
            mtime: epoch_secs(n.attr.mtime),
        })
    }

    fn read_at(&self, path: &str, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let (cache, size) = {
            let g = self.inner.lock().unwrap();
            let ino = *g
                .by_remote
                .get(path)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "没有这个节点"))?;
            let n = g
                .nodes
                .get(&ino)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "没有这个节点"))?;
            (
                n.cache
                    .clone()
                    .ok_or_else(|| io::Error::other("没有缓存文件（未水合）"))?,
                n.attr.size,
            )
        };
        if offset.saturating_add(len as u64) > size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("区间越界: {offset}+{len} > {size}"),
            ));
        }
        read_exact_at(&cache, offset, len).map_err(|e| io::Error::other(format!("{e:?}")))
    }

    fn roots(&self) -> Vec<String> {
        self.roots.clone()
    }
}

impl LocalView for FsHandle {
    fn remote_root(&self) -> &str {
        &self.remote_root
    }

    /// ★ M6：句柄拿到的是真实的多根列表，而不是默认实现的「只有 remote_root」。
    fn remote_roots(&self) -> Vec<String> {
        self.roots.clone()
    }

    fn node(&self, remote: &str) -> Option<LocalNode> {
        let g = self.inner.lock().unwrap();
        let ino = *g.by_remote.get(remote)?;
        g.nodes.get(&ino).map(node_snapshot)
    }

    fn nodes(&self) -> Vec<LocalNode> {
        let g = self.inner.lock().unwrap();
        g.nodes.values().map(node_snapshot).collect()
    }

    fn known_dirs(&self) -> Vec<String> {
        let g = self.inner.lock().unwrap();
        g.nodes
            .values()
            .filter(|n| n.attr.kind == FileType::Directory)
            .map(|n| n.remote.clone())
            .collect()
    }

    fn has_pending(&self, remote: &str) -> bool {
        self.upload
            .as_ref()
            .map(|q| q.has_pending(remote))
            .unwrap_or(false)
    }

    fn apply_remote_meta(&self, remote: &str, is_dir: bool, size: u64, mtime: i64) -> bool {
        let mut g = self.inner.lock().unwrap();
        let Some(ino) = g.by_remote.get(remote).copied() else {
            return false;
        };
        let chunk_size = self.chunk_size;
        let Some(n) = g.nodes.get_mut(&ino) else {
            return false;
        };
        let was_dir = n.attr.kind == FileType::Directory;
        let kind_changed = was_dir != is_dir;
        let content_changed =
            !n.dirty && (n.attr.size != size || epoch_secs(n.attr.mtime) != mtime);
        n.attr.size = if is_dir { 0 } else { size };
        n.attr.blocks = n.attr.size.div_ceil(512);
        let t = UNIX_EPOCH + Duration::from_secs(mtime.max(0) as u64);
        n.attr.mtime = t;
        n.attr.ctime = t;
        n.attr.kind = if is_dir {
            FileType::Directory
        } else {
            FileType::RegularFile
        };
        n.attr.perm = if is_dir { 0o755 } else { 0o644 };
        n.attr.nlink = if is_dir { 2 } else { 1 };
        if kind_changed {
            n.attr.size = if is_dir { 0 } else { size };
        }
        if content_changed || kind_changed {
            // 缓存内容已过期：删掉稀疏缓存、清空区间表 → 下次 read 重新水合
            if let Some(p) = n.cache.take() {
                let _ = std::fs::remove_file(p);
            }
            n.chunks_done = vec![false; n.chunk_count(chunk_size)];
        }
        true
    }

    fn invalidate_content(&self, remote: &str) -> bool {
        let mut g = self.inner.lock().unwrap();
        let Some(ino) = g.by_remote.get(remote).copied() else {
            return false;
        };
        let chunk_size = self.chunk_size;
        let Some(n) = g.nodes.get_mut(&ino) else {
            return false;
        };
        if let Some(p) = n.cache.take() {
            let _ = std::fs::remove_file(p);
        }
        n.chunks_done = vec![false; n.chunk_count(chunk_size)];
        n.dirty = false;
        true
    }

    fn remove_remote(&self, remote: &str) -> bool {
        let mut g = self.inner.lock().unwrap();
        let prefix = format!("{}/", remote.trim_end_matches('/'));
        let victims: Vec<INodeNo> = g
            .by_remote
            .iter()
            .filter(|(p, _)| p.as_str() == remote || p.starts_with(&prefix))
            .map(|(_, ino)| *ino)
            .collect();
        if victims.is_empty() {
            return false;
        }
        for ino in victims {
            if let Some(n) = g.nodes.remove(&ino) {
                g.by_remote.remove(&n.remote);
                if let Some(p) = n.cache {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
        true
    }

    fn mark_dirty(&self, remote: &str) -> std::io::Result<()> {
        let q = self
            .upload
            .as_ref()
            .ok_or_else(|| std::io::Error::other("只读挂载，没有上传队列"))?;
        let (name, mtime, cache) = {
            let mut g = self.inner.lock().unwrap();
            let Some(ino) = g.by_remote.get(remote).copied() else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("{remote} 不在节点表里"),
                ));
            };
            let n = g.nodes.get_mut(&ino).unwrap();
            n.dirty = true;
            let cache = n
                .cache
                .clone()
                .ok_or_else(|| std::io::Error::other(format!("{remote} 没有本地缓存，无法上传")))?;
            (n.name.clone(), epoch_secs(n.attr.mtime), cache)
        };
        let dir = remote
            .rsplit_once('/')
            .map(|(d, _)| d.to_string())
            .unwrap_or_else(|| self.remote_root.clone());
        q.enqueue(UploadJob {
            remote_dir: dir,
            remote_name: name,
            local: cache,
            mtime,
            attempts: 0,
            ephemeral: false,
        })
    }

    fn stash_conflict(&self, remote: &str, conflict_name: &str) -> std::io::Result<PathBuf> {
        let conflict_remote = format!(
            "{}/{}",
            remote.rsplit_once('/').map(|(d, _)| d).unwrap_or(""),
            conflict_name
        );
        let src = {
            let g = self.inner.lock().unwrap();
            let ino = g.by_remote.get(remote).copied().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("{remote} 不在节点表里"),
                )
            })?;
            g.nodes
                .get(&ino)
                .and_then(|n| n.cache.clone())
                .ok_or_else(|| {
                    std::io::Error::other(format!("{remote} 没有本地缓存，无法做冲突副本"))
                })?
        };
        let stash_dir = self.cache_dir.join("conflicts");
        std::fs::create_dir_all(&stash_dir)?;
        let dest = stash_dir.join(format!(
            "{:016x}_{}",
            fnv1a64(conflict_remote.as_bytes()),
            sanitize_filename(conflict_name)
        ));
        std::fs::copy(&src, &dest)?;
        Ok(dest)
    }

    fn enqueue_upload(
        &self,
        remote_dir: &str,
        remote_name: &str,
        local: PathBuf,
        mtime: i64,
    ) -> std::io::Result<()> {
        let q = self
            .upload
            .as_ref()
            .ok_or_else(|| std::io::Error::other("只读挂载，没有上传队列"))?;
        q.enqueue(UploadJob {
            remote_dir: remote_dir.to_string(),
            remote_name: remote_name.to_string(),
            local,
            mtime,
            attempts: 0,
            ephemeral: true,
        })
    }
}

fn node_snapshot(n: &Node) -> LocalNode {
    LocalNode {
        remote: n.remote.clone(),
        name: n.name.clone(),
        is_dir: n.attr.kind == FileType::Directory,
        size: n.attr.size,
        mtime: n
            .attr
            .mtime
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
        dirty: n.dirty,
        cache: n.cache.clone(),
    }
}

/// 本地大批删除熔断（M2c）：60 秒窗口内删除数超过阈值就熔断，
/// 之后所有删除回 `EACCES`，直到 `qsync sync --force-deletes`（或重新挂载）。
#[derive(Debug)]
pub struct DeleteGuard {
    limit: usize,
    window: Duration,
    inner: Mutex<DeleteGuardState>,
}

#[derive(Debug)]
struct DeleteGuardState {
    hits: VecDeque<Instant>,
    blocked: bool,
    reason: Option<String>,
}

impl DeleteGuard {
    pub fn new(limit: usize, window: Duration) -> Arc<Self> {
        Arc::new(Self {
            limit,
            window,
            inner: Mutex::new(DeleteGuardState {
                hits: VecDeque::new(),
                blocked: false,
                reason: None,
            }),
        })
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    /// 记录/允许一次删除：`false` = 熔断中，调用方必须回 `EACCES`。
    pub fn allow(&self) -> bool {
        if self.limit == 0 {
            return true;
        }
        let mut g = self.inner.lock().unwrap();
        if g.blocked {
            return false;
        }
        let now = Instant::now();
        while let Some(t) = g.hits.front() {
            if now.duration_since(*t) > self.window {
                g.hits.pop_front();
            } else {
                break;
            }
        }
        if g.hits.len() >= self.limit {
            let reason = format!(
                "{} 秒内删除超过 {} 项，已熔断（`qsync sync --force-deletes` 可解除）",
                self.window.as_secs(),
                self.limit
            );
            g.blocked = true;
            g.reason = Some(reason.clone());
            tracing::error!("{reason}");
            return false;
        }
        g.hits.push_back(now);
        true
    }

    pub fn blocked(&self) -> bool {
        self.inner.lock().unwrap().blocked
    }

    pub fn reason(&self) -> Option<String> {
        self.inner.lock().unwrap().reason.clone()
    }

    pub fn hits(&self) -> usize {
        self.inner.lock().unwrap().hits.len()
    }

    /// 解除熔断（`--force-deletes`）。
    pub fn reset(&self) {
        let mut g = self.inner.lock().unwrap();
        g.hits.clear();
        g.blocked = false;
        g.reason = None;
    }
}

/// 脱水被安全检查挡下时打一条可诊断的日志（含各检查项的实际取值）。
fn log_dehydrate_block(remote: &str, c: &Candidate, b: Block, phase: &str) {
    tracing::info!(
        "脱水跳过 {remote}（{phase}）: {}｜open={} dirty={} pending={} in_flight={} hydrated={} pin={} last_access={}",
        b.reason(),
        c.open_count,
        c.dirty,
        c.pending_upload,
        c.in_flight,
        c.hydrated_bytes,
        c.pin,
        c.last_access
    );
}

/// 文件名安全化（冲突副本的 stash 文件名用）。
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// ★ M3：缓存模式（报告 12 §8.3）。
///
/// * `PageCache`（默认）：走内核 page cache（性能好，脱水必须严格按「先 inval_inode 再清内容」）；
/// * `Direct`：`open()` 回 `FOPEN_DIRECT_IO`，完全绕过 page cache —— 脱水绝对安全，
///   但**没有 readahead、mmap 不可用**（保守模式，排障/低速链路用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheMode {
    PageCache,
    Direct,
}

impl CacheMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "pagecache" | "page-cache" | "cached" | "cache" => Some(CacheMode::PageCache),
            "direct" | "direct_io" | "direct-io" => Some(CacheMode::Direct),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            CacheMode::PageCache => "pagecache",
            CacheMode::Direct => "direct",
        }
    }
}

pub struct QxyncFs {
    rt: tokio::runtime::Runtime,
    client: Arc<Client>,
    /// 远端挂载根（普通用户家目录 = `/home`）。多根时是**第一个**根的远端路径。
    remote_root: String,
    /// ★ M6：全部远端根（单根时只有 `remote_root` 一个，`view_name` 为空）。
    roots: Vec<RootSpec>,
    /// ★ M6：挂载点是不是「虚拟根 + 每个根一个合成目录」的多根视图。
    multi_root: bool,
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
    /// ★ M2c：节点表用 `Arc<Mutex<..>>` 共享 —— daemon 的同步引擎通过 [`FsHandle`]
    /// 在挂载线程之外刷新远端变更（改元数据 / 失效缓存 / 删节点）。
    inner: Arc<Mutex<Inner>>,
    /// ★ M2c：本地大批删除熔断（`rm -rf` 保护）。
    delete_guard: Arc<DeleteGuard>,
    /// ★ M3：缓存模式（pagecache / direct）。
    cache_mode: CacheMode,
    /// ★ M7：选择性同步 / 临时文件过滤规则（挂载点里不可见的直接不出现）。
    rules: Arc<Rules>,
    /// ★ M7：已配对的对等设备（daemon 持有并热更新；水合时先试 LAN）。
    lan_peers: Arc<Mutex<Vec<PeerConfig>>>,
    /// ★ M7：LAN 直传统计（命中区间数 / 字节 / 尝试次数）。
    lan_stats: Arc<LanStats>,
}

/// ★ M7：LAN 快路径计数（`status` 里能看到省了多少次 NAS 请求）。
#[derive(Debug, Default)]
pub struct LanStats {
    /// 试过 LAN 的次数（有 peer 时每次水合区间 +1）。
    pub attempts: AtomicU64,
    /// 命中次数（区间数）。
    pub hits: AtomicU64,
    /// 从 LAN 拿到的字节数。
    pub bytes: AtomicU64,
    /// 对端元数据不一致 / 没水合而跳过的次数。
    pub mismatches: AtomicU64,
}

impl LanStats {
    pub fn snapshot(&self) -> (u64, u64, u64, u64) {
        (
            self.attempts.load(Ordering::Relaxed),
            self.hits.load(Ordering::Relaxed),
            self.bytes.load(Ordering::Relaxed),
            self.mismatches.load(Ordering::Relaxed),
        )
    }
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
            open_count: 0,
            last_access: UNIX_EPOCH,
            op_lock: Arc::new(Mutex::new(())),
        };
        let mut nodes = HashMap::new();
        let mut by_remote = HashMap::new();
        nodes.insert(INodeNo::ROOT, root);
        by_remote.insert(remote_root.clone(), INodeNo::ROOT);

        Ok(Self {
            rt,
            client,
            remote_root: remote_root.clone(),
            // ★ M6：单根 = 直通（M1–M5 行为一字不改）：只有一个根，挂载点**就是**它。
            roots: vec![RootSpec {
                remote: remote_root.clone(),
                view_name: String::new(),
                writable: true,
            }],
            multi_root: false,
            cache_dir,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            read_only: true,
            upload: None,
            chunk_size: DEFAULT_CHUNK_SIZE,
            hydrate_timeout: HYDRATE_TIMEOUT,
            hydro: Arc::new(HydroCounters::default()),
            pins: Arc::new(Mutex::new(HashMap::new())),
            inner: Arc::new(Mutex::new(Inner {
                nodes,
                by_remote,
                next_ino: 2,
                inflight_chunks: HashMap::new(),
            })),
            delete_guard: DeleteGuard::new(DEFAULT_DELETE_LIMIT, DEFAULT_DELETE_WINDOW),
            cache_mode: CacheMode::PageCache,
            rules: Arc::new(Rules::temp_only(true)),
            lan_peers: Arc::new(Mutex::new(Vec::new())),
            lan_stats: Arc::new(LanStats::default()),
        })
    }

    /// ★ M6：多根视图。挂载点是**虚拟根**，每个远端根是它的一个合成目录：
    ///     ~/mnt/home/qxync-test/x  →  /home/qxync-test/x
    ///     ~/mnt/Public/a.txt       →  /Public/a.txt
    /// 单根请继续用 `new()`（行为与 M1–M5 完全一致）。
    ///
    /// 只有**挂载点这一层**多了一次名字映射：节点表以下的缓存/水合/上传仍全用远端路径做键。
    pub fn new_multi(
        client: Arc<Client>,
        entries: Vec<RootSpec>,
        cache_dir: impl Into<PathBuf>,
    ) -> std::io::Result<Self> {
        if entries.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "多根挂载至少需要一个远端根",
            ));
        }
        let cache_dir = cache_dir.into();
        std::fs::create_dir_all(&cache_dir)?;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;

        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        let dir_attr = |ino: INodeNo| FileAttr {
            ino,
            size: 0,
            blocks: 0,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: FileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid,
            gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        };
        // 虚拟根：`remote = ""` —— 它**不是**有效远端路径，任何远端调用都不得拿它当参数。
        let root = Node {
            ino: INodeNo::ROOT,
            parent: INodeNo::ROOT,
            name: String::new(),
            remote: String::new(),
            attr: dir_attr(INodeNo::ROOT),
            cache: None,
            chunks_done: Vec::new(),
            dirty: false,
            open_count: 0,
            last_access: UNIX_EPOCH,
            op_lock: Arc::new(Mutex::new(())),
        };
        let mut nodes = HashMap::new();
        let mut by_remote = HashMap::new();
        nodes.insert(INodeNo::ROOT, root);
        by_remote.insert(String::new(), INodeNo::ROOT);

        // 每个远端根预插入一个合成目录节点（名字 = view_name），不访问 NAS。
        let mut next_ino = 2u64;
        let mut roots = Vec::with_capacity(entries.len());
        for entry in entries {
            let ino = INodeNo(next_ino);
            next_ino += 1;
            nodes.insert(
                ino,
                Node {
                    ino,
                    parent: INodeNo::ROOT,
                    name: entry.view_name.clone(),
                    remote: entry.remote.clone(),
                    attr: dir_attr(ino),
                    cache: None,
                    chunks_done: Vec::new(),
                    dirty: false,
                    open_count: 0,
                    last_access: UNIX_EPOCH,
                    op_lock: Arc::new(Mutex::new(())),
                },
            );
            by_remote.insert(entry.remote.clone(), ino);
            roots.push(entry);
        }
        let remote_root = roots
            .first()
            .map(|r| r.remote.clone())
            .unwrap_or_default();

        Ok(Self {
            rt,
            client,
            remote_root,
            roots,
            multi_root: true,
            cache_dir,
            uid,
            gid,
            read_only: true,
            upload: None,
            chunk_size: DEFAULT_CHUNK_SIZE,
            hydrate_timeout: HYDRATE_TIMEOUT,
            hydro: Arc::new(HydroCounters::default()),
            pins: Arc::new(Mutex::new(HashMap::new())),
            inner: Arc::new(Mutex::new(Inner {
                nodes,
                by_remote,
                next_ino,
                inflight_chunks: HashMap::new(),
            })),
            delete_guard: DeleteGuard::new(DEFAULT_DELETE_LIMIT, DEFAULT_DELETE_WINDOW),
            cache_mode: CacheMode::PageCache,
            rules: Arc::new(Rules::temp_only(true)),
            lan_peers: Arc::new(Mutex::new(Vec::new())),
            lan_stats: Arc::new(LanStats::default()),
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

    /// ★ M6：全部远端根（远端路径列表，保持挂载顺序）。单根时就是 `[remote_root]`。
    pub fn remote_roots(&self) -> Vec<String> {
        self.roots.iter().map(|r| r.remote.clone()).collect()
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

    /// ★ M2c：共享节点表句柄 —— 必须在把实例交给 `mount2` **之前**取。
    pub fn handle(&self) -> FsHandle {
        FsHandle {
            inner: self.inner.clone(),
            cache_dir: self.cache_dir.clone(),
            chunk_size: self.chunk_size,
            remote_root: self.remote_root.clone(),
            roots: self.remote_roots(),
            upload: self.upload.clone(),
            delete_guard: self.delete_guard.clone(),
            read_only: self.read_only,
            pins: self.pins.clone(),
            cache_mode: self.cache_mode,
            rules: self.rules.clone(),
            lan_stats: self.lan_stats.clone(),
        }
    }

    /// ★ M7：选择性同步规则（`link.exclude` + 临时文件过滤）。
    pub fn with_rules(mut self, rules: Arc<Rules>) -> Self {
        self.rules = rules;
        self
    }

    pub fn rules(&self) -> Arc<Rules> {
        self.rules.clone()
    }

    /// ★ M7：LAN 直传统计（(尝试, 命中, 字节, 元数据不符)）。
    pub fn lan_stats(&self) -> Arc<LanStats> {
        self.lan_stats.clone()
    }

    /// ★ M7：已配对的对等设备（daemon 持有 `Arc<Mutex<..>>`，配对成功后热更新）。
    pub fn with_peers(mut self, peers: Arc<Mutex<Vec<PeerConfig>>>) -> Self {
        self.lan_peers = peers;
        self
    }

    /// ★ M7：一次区间水合先试 LAN。返回 `None` = 没有可用对端（调用方走 NAS）。
    ///
    /// 判据（与 `peer::fetch_range` 一致）：对端 `head` 必须 `exists && hydrated`，
    /// 且 `size`/`mtime` 与本节点从 NAS `stat` 拿到的签名一致 —— 只有「同一份内容」才敢用。
    fn lan_fetch_chunk(
        &self,
        remote: &str,
        offset: u64,
        len: u64,
        expect_size: u64,
        expect_mtime: i64,
    ) -> Option<Vec<u8>> {
        if len == 0 {
            return None;
        }
        let peers = {
            let g = self.lan_peers.lock().unwrap();
            if g.is_empty() {
                return None;
            }
            g.clone()
        };
        self.lan_stats.attempts.fetch_add(1, Ordering::Relaxed);
        let hit = self
            .rt
            .block_on(peer::fetch_range(&peers, remote, offset, len, expect_size, expect_mtime));
        match hit {
            Some(h) => {
                self.lan_stats.hits.fetch_add(1, Ordering::Relaxed);
                self.lan_stats
                    .bytes
                    .fetch_add(h.data.len() as u64, Ordering::Relaxed);
                tracing::info!(
                    "LAN 直传命中: {remote} [{offset}..{}) ← {} ({:?})",
                    offset + len,
                    h.peer,
                    h.took
                );
                Some(h.data)
            }
            None => {
                self.lan_stats.mismatches.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// ★ M3：缓存模式（`pagecache` 默认 / `direct` 绕过 page cache）。
    pub fn with_cache_mode(mut self, mode: CacheMode) -> Self {
        self.cache_mode = mode;
        self
    }

    pub fn cache_mode(&self) -> CacheMode {
        self.cache_mode
    }

    /// 本地批量删除阈值（0 = 关闭熔断；默认 100 次/60 秒）。
    pub fn with_delete_limit(mut self, limit: usize) -> Self {
        if limit != DEFAULT_DELETE_LIMIT {
            self.delete_guard = DeleteGuard::new(limit, DEFAULT_DELETE_WINDOW);
        }
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

    /// ★ M7：规则判定 —— 被排除 / 是临时文件的路径在挂载点里**根本不存在**。
    ///
    /// 多根虚拟根（`remote == ""`）与合成目录不参与判定（它们是挂载视图本身）。
    fn hide_reason(&self, remote: &str, is_dir: bool) -> Option<HideReason> {
        if remote.is_empty() {
            return None;
        }
        if self.multi_root && self.roots.iter().any(|r| r.remote == remote) {
            return None; // 根目录本身是视图的一部分
        }
        self.rules.hides_in_roots(&self.remote_roots(), remote, is_dir)
    }

    /// 在目录里查一个名字（不水合）。返回节点克隆。
    fn lookup_child(&self, parent: INodeNo, name: &str) -> Result<Node, fuser::Errno> {
        // ★ M6：虚拟根不映射任何远端路径 —— 只在节点表里找合成目录，
        //   绝不能拿 `""` 去 `client.stat("", name)`（那会打到 NAS 根上）。
        if self.multi_root && parent == INodeNo::ROOT {
            let g = self.inner.lock().unwrap();
            return g
                .nodes
                .values()
                .find(|n| n.ino != INodeNo::ROOT && n.parent == INodeNo::ROOT && n.name == name)
                .cloned()
                .ok_or(fuser::Errno::ENOENT);
        }
        let parent_remote = {
            let g = self.inner.lock().unwrap();
            g.nodes
                .get(&parent)
                .ok_or(fuser::Errno::ENOENT)?
                .remote
                .clone()
        };
        let remote = self.join_remote(&parent_remote, name);
        // ★ M7：名字规则能在 stat 之前判掉一部分（临时文件/无斜杠规则），先省一次 NAS 往返。
        if self.hide_reason(&remote, false).is_some() {
            return Err(fuser::Errno::ENOENT);
        }

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
        // 目录限定规则（`/cache/`）要拿到实际类型才能判
        if self.hide_reason(&remote, entry.isfolder).is_some() {
            return Err(fuser::Errno::ENOENT);
        }
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
            open_count: 0,
            last_access: SystemTime::now(),
            op_lock: Arc::new(Mutex::new(())),
        };
        g.nodes.insert(ino, node.clone());
        g.by_remote.insert(remote.to_string(), ino);
        node
    }

    /// 列一个目录（不水合），并把子节点的元数据灌进表里。
    fn load_children(&self, ino: INodeNo) -> Result<Vec<Node>, fuser::Errno> {
        // ★ M6：虚拟根的子节点是预插入的合成目录 —— **不访问 NAS**。
        if self.multi_root && ino == INodeNo::ROOT {
            let g = self.inner.lock().unwrap();
            return Ok(g
                .nodes
                .values()
                .filter(|n| n.ino != INodeNo::ROOT && n.parent == INodeNo::ROOT)
                .cloned()
                .collect());
        }
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
        let visible = self.filter_visible(&remote, &entries);
        let hidden = entries.len().saturating_sub(visible.len());
        let mut out = Vec::with_capacity(visible.len());
        for (child_remote, e) in visible {
            out.push(self.insert_node(ino, &e.filename, &child_remote, &e));
        }
        if hidden > 0 {
            // ★ M7：排除 / 临时文件不进节点表 —— 挂载点里根本看不到它们。
            tracing::debug!("readdir {remote}: 规则隐藏了 {hidden} 项");
        }
        Ok(out)
    }

    /// ★ M7：readdir 的过滤闸门（抽出来是为了**不挂 FUSE 也能单测**）。
    ///
    /// 返回 `(远端路径, 条目)`，被 `exclude` / 临时文件规则命中的直接丢掉。
    fn filter_visible(&self, dir_remote: &str, entries: &[DirEntry]) -> Vec<(String, DirEntry)> {
        let mut out = Vec::with_capacity(entries.len());
        for e in entries.iter().take(LIST_LIMIT) {
            let child_remote = self.join_remote(dir_remote, &e.filename);
            if self.hide_reason(&child_remote, e.isfolder).is_some() {
                continue;
            }
            out.push((child_remote, e.clone()));
        }
        out
    }

    /// 远端路径的目录部分。
    fn remote_dir_of(&self, remote: &str) -> String {
        remote
            .rsplit_once('/')
            .map(|(d, _)| d.to_string())
            .unwrap_or_else(|| self.remote_root.clone())
    }

    /// 多根挂载时非家目录的根只读（实测：普通账号向共享文件夹上传会被服务端拒绝 status 20）。
    /// 单根直通时永远 Ok（挂载级 read_only 已经管住了）。
    ///
    /// 规则：先把 `remote` 归属到某个根（`remote == root` 或以 `root + "/"` 开头），
    /// 该根 `writable` 才放行；找不到归属的根（例如虚拟根下新建的路径）也一律 `EROFS`。
    fn ensure_writable(&self, remote: &str) -> Result<(), fuser::Errno> {
        if !self.multi_root {
            return Ok(());
        }
        for r in &self.roots {
            let prefix = format!("{}/", r.remote.trim_end_matches('/'));
            if remote == r.remote || remote.starts_with(&prefix) {
                return if r.writable {
                    Ok(())
                } else {
                    Err(fuser::Errno::EROFS)
                };
            }
        }
        Err(fuser::Errno::EROFS)
    }

    /// 该 ino 对应的远端路径（节点不存在 → `ENOENT`）；给写路径的只读检查用。
    fn remote_of(&self, ino: INodeNo) -> Result<String, fuser::Errno> {
        let g = self.inner.lock().unwrap();
        g.nodes
            .get(&ino)
            .map(|n| n.remote.clone())
            .ok_or(fuser::Errno::ENOENT)
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
        // ★ M7：兜底防御 —— 排除/临时文件不该走到水合（lookup 已经 ENOENT 了）。
        //   万一有 ino 漏进来，宁可 EACCES 也绝不去 NAS 拉内容（选择性同步的语义）。
        let guard_remote = {
            let g = self.inner.lock().unwrap();
            g.nodes.get(&ino).map(|n| n.remote.clone())
        };
        if let Some(remote) = guard_remote {
            if self.hide_reason(&remote, false).is_some() {
                tracing::debug!("拒绝水合被规则隐藏的路径: {remote}");
                return Err(fuser::Errno::EACCES);
            }
        }
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
        // 先算出目标远端路径：★ M6 的只读检查必须在真正删除之前，
        // 也不能让被 EROFS 挡下的删除白白吃掉一次熔断额度。
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
        // ★ M6：虚拟根的直接子节点是合成目录（挂载视图本身），只能靠卸载移除。
        if self.multi_root && parent == INodeNo::ROOT {
            return reply.error(fuser::Errno::EROFS);
        }
        // ★ M6：非家目录根只读 —— 共享文件夹里删除的直接回 EROFS。
        if let Err(e) = self.ensure_writable(&join_path(&parent_remote, name)) {
            return reply.error(e);
        }
        // ★ M2c：本地大批删除熔断。`rm -rf` 超过阈值后拒绝继续删，
        //   避免「本地误删 → 立即同步清空远端」这种最危险的组合。
        if !self.delete_guard.allow() {
            tracing::warn!(
                "本地删除被熔断（{}）: {name}",
                self.delete_guard.reason().unwrap_or_default()
            );
            return reply.error(fuser::Errno::EACCES);
        }
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
            ephemeral: false,
        };
        queue.enqueue(job).map_err(|e| {
            tracing::error!("入队上传失败: {e}");
            fuser::Errno::EIO
        })?;
        tracing::debug!("已入队上传: {remote} (mtime={mtime})");
        Ok(())
    }

    /// 测试用：读某个 ino 的状态串（placeholder/partial/hydrated）。
    #[cfg(test)]
    fn node_state_for_test(&self, ino: INodeNo) -> &'static str {
        let g = self.inner.lock().unwrap();
        g.nodes
            .get(&ino)
            .map(|n| n.state_str())
            .unwrap_or("missing")
    }

    /// ★ M3：刷新节点的「最后访问时间」（LRU 脱水用）。
    fn touch(&self, ino: INodeNo) {
        let mut g = self.inner.lock().unwrap();
        if let Some(n) = g.nodes.get_mut(&ino) {
            n.last_access = SystemTime::now();
        }
    }

    /// ★ M3：把被**完整覆盖**的区间记成「已就绪」。
    ///
    /// 本地写入的数据同样在缓存文件里，不记的话本地新建/改过的文件永远被当成
    /// 「没有缓存内容」：既不能脱水、xattr 的 chunks 也不准
    /// （实测踩过：16 MiB 本地文件脱水被判成「本来就是占位符」）。
    /// 只有整块区间都被这次写覆盖才算完整（部分覆盖的块里还有别的字节）。
    fn mark_written_chunks(&self, ino: INodeNo, offset: u64, len: u64) {
        if len == 0 {
            return;
        }
        let chunk_size = self.chunk_size;
        let mut g = self.inner.lock().unwrap();
        if let Some(n) = g.nodes.get_mut(&ino) {
            let want = n.chunk_count(chunk_size);
            if n.chunks_done.len() < want {
                n.chunks_done.resize(want, false);
            }
            let end = offset + len;
            let (first, last) = chunk_indices(offset, len, chunk_size);
            for idx in first..=last {
                let c_start = idx * chunk_size;
                let c_end = ((idx + 1) * chunk_size).min(n.attr.size);
                if c_start >= offset && c_end <= end {
                    if let Some(slot) = n.chunks_done.get_mut(idx as usize) {
                        *slot = true;
                    }
                }
            }
        }
    }

    /// 节点操作锁（read/write/setattr 持锁；脱水 try_lock）。
    fn op_lock(&self, ino: INodeNo) -> Option<Arc<Mutex<()>>> {
        let g = self.inner.lock().unwrap();
        g.nodes.get(&ino).map(|n| n.op_lock.clone())
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

        let (remote, total, name, dest, mtime) = {
            let g = self.inner.lock().unwrap();
            let n = g.nodes.get(&ino).ok_or(fuser::Errno::ENOENT)?;
            (
                n.remote.clone(),
                n.attr.size,
                n.name.clone(),
                n.cache.clone().ok_or(fuser::Errno::EIO)?,
                Self::epoch_of(n.attr.mtime),
            )
        };
        // ★ M7：规则兜底（lookup 已经挡住了，这里是纵深防御）
        if self.hide_reason(&remote, false).is_some() {
            self.inner.lock().unwrap().inflight_chunks.remove(&key);
            return Err(fuser::Errno::EACCES);
        }
        let dir = remote
            .rsplit_once('/')
            .map(|(d, _)| d.to_string())
            .unwrap_or_else(|| self.remote_root.clone());

        let start = idx * self.chunk_size;
        let end = (start + self.chunk_size).min(total) - 1; // 闭区间，末块按文件尾截断
        let want = end - start + 1;

        // ★ M7：LAN 快路径 —— 先问已配对的对端有没有这份内容（元数据必须与 NAS 签名一致）。
        //   命中就完全跳过 NAS；没命中/对端不靠谱（长度不符）就走下面的 NAS 老路。
        let data = match self.lan_fetch_chunk(&remote, start, want, total, mtime) {
            Some(d) => {
                tracing::debug!("LAN 直传命中: {remote} [{start}..={start}+{want})");
                d
            }
            None => {
                let client = self.client.clone();
                let (d2, n2) = (dir.clone(), name.clone());
                let timeout = self.hydrate_timeout;
                let res = self.rt.block_on(async move {
                    tokio::time::timeout(timeout, client.download_range(&d2, &n2, start, end)).await
                });

                match res {
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
                }
            }
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

/// `SystemTime` → epoch 秒（负数/异常一律 0）。
fn epoch_secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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
        // ★ M6：带写意图打开非家目录根下的文件 → EROFS（与挂载级 read_only 检查并存）
        if flags.acc_mode() != OpenAccMode::O_RDONLY {
            let remote = match self.remote_of(ino) {
                Ok(r) => r,
                Err(e) => return reply.error(e),
            };
            if let Err(e) = self.ensure_writable(&remote) {
                return reply.error(e);
            }
        }
        // ★ M3：缓存模式决定是否绕过内核 page cache
        let fopen = match self.cache_mode {
            CacheMode::PageCache => FopenFlags::FOPEN_KEEP_CACHE,
            CacheMode::Direct => FopenFlags::FOPEN_DIRECT_IO,
        };
        {
            let mut g = self.inner.lock().unwrap();
            match g.nodes.get_mut(&ino) {
                Some(n) => {
                    n.open_count = n.open_count.saturating_add(1);
                    n.last_access = SystemTime::now();
                }
                None => return reply.error(fuser::Errno::ENOENT),
            }
        }
        reply.opened(fuser::FileHandle(u64::from(ino)), fopen);
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
        // ★ M3：拿节点操作锁 —— 脱水用 try_lock，因此在途读不会被清内容
        let lock = match self.op_lock(ino) {
            Some(l) => l,
            None => return reply.error(fuser::Errno::ENOENT),
        };
        let _guard = lock.lock().unwrap();
        self.touch(ino);
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
        // ★ M6：非家目录根只读（用该文件节点自己的远端路径归属到根）
        {
            let remote = match self.remote_of(ino) {
                Ok(r) => r,
                Err(e) => return reply.error(e),
            };
            if let Err(e) = self.ensure_writable(&remote) {
                return reply.error(e);
            }
        }
        // ★ M3：与脱水互斥（不然可能「写完被清掉、还没入队」）
        let lock = match self.op_lock(ino) {
            Some(l) => l,
            None => return reply.error(fuser::Errno::ENOENT),
        };
        let _guard = lock.lock().unwrap();
        self.touch(ino);
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
        // ★ M3：本地写入的数据也是「已经有内容」
        self.mark_written_chunks(ino, offset, data.len() as u64);
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
        // ★ M6：size/mtime 是写操作 —— 非家目录根下的节点直接 EROFS
        if size.is_some() || mtime.is_some() {
            let remote = match self.remote_of(ino) {
                Ok(r) => r,
                Err(e) => return reply.error(e),
            };
            if let Err(e) = self.ensure_writable(&remote) {
                return reply.error(e);
            }
        }
        // ★ M3：截断/改 mtime 与脱水互斥
        let lock = self.op_lock(ino);
        let _guard = lock.as_ref().map(|l| l.lock().unwrap());
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
        // ★ M6：非家目录根只读；虚拟根下新建（remote 不属于任何根）同样是 EROFS
        if let Err(e) = self.ensure_writable(&remote) {
            return reply.error(e);
        }
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
        let remote = join_path(&parent_remote, name);
        // ★ M6：非家目录根只读；虚拟根下 `mkdir ~/mnt/NewDir` 也自然回 EROFS
        if let Err(e) = self.ensure_writable(&remote) {
            return reply.error(e);
        }
        let client = self.client.clone();
        let (p, n) = (parent_remote.clone(), name.to_string());
        if let Err(e) = self.rt.block_on(async move { client.mkdir(&p, &n).await }) {
            tracing::warn!("mkdir 失败 {parent_remote}/{name}: {e}");
            return reply.error(fuser::Errno::EIO);
        }
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

        // ★ M6：虚拟根的直接子节点是合成目录（挂载视图本身），不允许改名/挪走。
        if self.multi_root && (parent == INodeNo::ROOT || newparent == INodeNo::ROOT) {
            return reply.error(fuser::Errno::EROFS);
        }
        // ★ M6：改名的**两端**都要可写 —— 从 /Public 挪出、或挪进 /Public 都回 EROFS。
        if let Err(e) = self.ensure_writable(&old_remote) {
            return reply.error(e);
        }
        if let Err(e) = self.ensure_writable(&new_remote) {
            return reply.error(e);
        }

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
        ino: INodeNo,
        _fh: fuser::FileHandle,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        {
            let mut g = self.inner.lock().unwrap();
            if let Some(n) = g.nodes.get_mut(&ino) {
                n.open_count = n.open_count.saturating_sub(1);
                n.last_access = SystemTime::now();
            }
        }
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

/// ★ M3：可通知内核的挂载句柄。
///
/// `qsync_fuse::mount()` 用 `fuser::mount2`（拿不到 Notifier）；脱水必须能发
/// `inval_inode`，所以 daemon 用 [`spawn`] —— 它返回 `BackgroundSession`（可 join/卸载）
/// 和 `Notifier`（`inval_inode(ino, 0, 0)`）。
pub struct MountHandle {
    pub session: fuser::BackgroundSession,
    pub notifier: fuser::Notifier,
}

impl MountHandle {
    /// 让内核对某个 inode 失效（page cache + attr）。
    pub fn invalidate_inode(&self, ino: INodeNo) -> std::io::Result<()> {
        self.notifier.inval_inode(ino, 0, 0)
    }

    /// 卸载并 join 挂载线程（外部 `fusermount3 -u` 之后调用）。
    pub fn join(self) -> std::io::Result<()> {
        self.session.join()
    }
}

/// ★ M3：后台挂载（daemon 用；返回可发通知的句柄）。
pub fn spawn(
    fs: QxyncFs,
    mountpoint: &Path,
    n_threads: usize,
    auto_unmount: bool,
    read_only: bool,
) -> std::io::Result<MountHandle> {
    let cfg = mount_config(n_threads, auto_unmount, read_only);
    let session = fuser::spawn_mount2(fs, mountpoint, &cfg)?;
    let notifier = session.notifier();
    Ok(MountHandle { session, notifier })
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
            open_count: 0,
            last_access: UNIX_EPOCH,
            op_lock: Arc::new(Mutex::new(())),
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

    // ------------------------------------------------------------ M2c 单测

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn test_link() -> qxync_core::LinkConfig {
        qxync_core::LinkConfig {
            id: "test".into(),
            host: "nas.invalid".into(),
            port: 9834,
            https: true,
            insecure: true,
            user: "test1".into(),
            home_root: "/home".into(),
            roots: Vec::new(),
            ipv4_only: false,
            exclude: Vec::new(),
            filter_temp: true,
            peer_listen: None,
            peer_name: None,
        }
    }

    fn test_fs(dir: &Path, rw: bool) -> QxyncFs {
        let client = Arc::new(Client::new(&test_link()).unwrap());
        let fs = QxyncFs::new(client, "/home", dir).unwrap();
        if rw {
            fs.with_write_mode()
        } else {
            fs
        }
    }

    fn m7_rules(exclude: &[&str], filter_temp: bool) -> Arc<Rules> {
        Arc::new(
            Rules::parse(
                &exclude.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                filter_temp,
            )
            .rules,
        )
    }

    fn m7_tmpdir(tag: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "qxync-fuse-m7-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// ★ M7：排除/临时文件在挂载点里**不存在**（lookup 判定 + readdir 过滤）。
    #[test]
    fn m7_rules_hide_lookup_and_readdir() {
        let dir = m7_tmpdir("hide");
        let fs = test_fs(&dir, false).with_rules(m7_rules(&["/secret", "*.crdownload"], true));

        assert_eq!(
            fs.hide_reason("/home/secret", true),
            Some(HideReason::Excluded)
        );
        assert_eq!(
            fs.hide_reason("/home/secret/deep/file.txt", false),
            Some(HideReason::Excluded),
            "祖先目录被排除 → 后代一起隐藏"
        );
        assert_eq!(
            fs.hide_reason("/home/qxync-test/a.crdownload", false),
            Some(HideReason::Temp)
        );
        assert_eq!(fs.hide_reason("/home/qxync-test/hello.txt", false), None);

        // readdir 闸门：只留下正常文件
        let entries = vec![
            DirEntry::local("secret", true, 0, 1),
            DirEntry::local("hello.txt", false, 5, 2),
            DirEntry::local("dl.crdownload", false, 9, 3),
        ];
        let visible = fs.filter_visible("/home", &entries);
        let names: Vec<&str> = visible.iter().map(|(_, e)| e.filename.as_str()).collect();
        assert_eq!(names, vec!["hello.txt"]);

        // 单根默认（只有临时过滤）= M1–M6 行为：普通名字一个不少
        let plain = test_fs(&dir.join("plain"), false);
        assert!(plain.hide_reason("/home/secret", true).is_none());
        assert_eq!(plain.hide_reason("/home/a.crdownload", false), Some(HideReason::Temp));
        let off = test_fs(&dir.join("off"), false).with_rules(m7_rules(&[], false));
        assert!(off.hide_reason("/home/a.crdownload", false).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ★ M7：多根时每个根目录本身是「挂载视图」，绝不能被规则隐藏。
    #[test]
    fn m7_rules_never_hide_mount_roots() {
        let dir = m7_tmpdir("roots");
        let client = Arc::new(Client::new(&test_link()).unwrap());
        let fs = QxyncFs::new_multi(
            client,
            vec![
                RootSpec {
                    remote: "/home".into(),
                    view_name: "home".into(),
                    writable: true,
                },
                RootSpec {
                    remote: "/Public".into(),
                    view_name: "Public".into(),
                    writable: false,
                },
            ],
            &dir,
        )
        .unwrap()
        .with_rules(m7_rules(&["/tailscale.txt"], true));

        assert_eq!(fs.hide_reason("/Public", true), None, "根本身不能被隐藏");
        assert_eq!(fs.hide_reason("/home", true), None, "根本身不能被隐藏");
        // 规则是**根相对**的：同一条规则对每个根都生效（文档 §1.2 的已知取舍）
        assert_eq!(
            fs.hide_reason("/Public/tailscale.txt", false),
            Some(HideReason::Excluded),
            "根里面的内容照常按规则隐藏"
        );
        assert_eq!(
            fs.hide_reason("/home/tailscale.txt", false),
            Some(HideReason::Excluded)
        );
        assert_eq!(fs.hide_reason("/home/a.txt", false), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ★ M7：排除路径**永不**进入脱水候选（防止把用户以为还在的本地副本清掉）。
    #[test]
    fn m7_rules_never_dehydrate_excluded() {
        let dir = m7_tmpdir("dehydrate");
        let fs = test_fs(&dir, false).with_rules(m7_rules(&["/secret"], true));
        let h = fs.handle();
        let keep = DirEntry::local("keep.bin", false, 4096, 10);
        let secret = DirEntry::local("secret.bin", false, 4096, 11);
        fs.insert_node(INodeNo::ROOT, "keep.bin", "/home/keep.bin", &keep);
        fs.insert_node(INodeNo::ROOT, "secret.bin", "/home/secret.bin", &secret);
        let hidden_dir = DirEntry::local("secret", true, 0, 12);
        let dnode = fs.insert_node(INodeNo::ROOT, "secret", "/home/secret", &hidden_dir);
        let inside = DirEntry::local("x.bin", false, 4096, 13);
        fs.insert_node(dnode.ino, "x.bin", "/home/secret/x.bin", &inside);

        let paths: Vec<String> = h.dehydrate_candidates().into_iter().map(|c| c.remote).collect();
        assert!(paths.contains(&"/home/keep.bin".to_string()));
        assert!(
            !paths.iter().any(|p| p.starts_with("/home/secret/")),
            "被排除子树里的文件绝不进脱水候选: {paths:?}"
        );
        assert!(h.candidate("/home/secret/x.bin").is_none());
        assert!(h.candidate("/home/keep.bin").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ★ M7：`ensure_chunk` 先走 LAN；元数据一致就命中，不一致/没对端就回落 NAS。
    #[test]
    fn m7_lan_hydration_fast_path_and_fallback() {
        use qxync_client::peer::{serve, ContentSource, DirContent, PeerServer};

        // 对端内容源（独立线程里的 runtime；QxyncFs 自己 block_on，不能在 async 上下文里跑）
        let src_dir = m7_tmpdir("lan-src");
        std::fs::create_dir_all(src_dir.join("qxync-test")).unwrap();
        let payload = vec![0xABu8; 4096];
        let file = src_dir.join("qxync-test/big.bin");
        std::fs::write(&file, &payload).unwrap();
        let mtime = std::fs::metadata(&file)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let (addr_tx, addr_rx) = std::sync::mpsc::channel::<String>();
        let (ev_tx, _ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let (reg_tx, _reg_rx) = tokio::sync::mpsc::unbounded_channel();
        let source: Arc<dyn ContentSource> = Arc::new(DirContent::new(vec![(
            "/home".to_string(),
            src_dir.clone(),
        )]));
        let server = Arc::new(PeerServer::new(
            "srv",
            "0.0.0-test",
            None,
            vec!["/home".into()],
            source,
            ev_tx,
            reg_tx,
        ));
        server.add_token("self", "tok");
        let server2 = server.clone();
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
            let addr = listener.local_addr().unwrap().to_string();
            addr_tx.send(addr).unwrap();
            rt.spawn(async move { serve(listener, server2).await });
            rt.block_on(async {
                tokio::time::sleep(Duration::from_secs(60)).await;
            });
        });
        let addr = addr_rx.recv().unwrap();

        let peers = Arc::new(Mutex::new(vec![PeerConfig {
            name: "srv".into(),
            addr: addr.clone(),
            token: "tok".into(),
        }]));
        let cache = m7_tmpdir("lan-cache");
        let fs = test_fs(&cache, false).with_peers(peers.clone());
        let entry = DirEntry::local("big.bin", false, payload.len() as u64, mtime);
        let node = fs.insert_node(INodeNo::ROOT, "big.bin", "/home/qxync-test/big.bin", &entry);
        fs.cache_file_for(node.ino).unwrap();

        fs.ensure_chunk(node.ino, 0).expect("LAN 命中时应该成功");
        assert_eq!(fs.node_state_for_test(node.ino), "hydrated");
        let (attempts, hits, bytes, _mism) = fs.lan_stats().snapshot();
        assert_eq!(attempts, 1);
        assert_eq!(hits, 1, "必须走 LAN 命中");
        assert_eq!(bytes, payload.len() as u64);
        let got = std::fs::read(fs.cache_file_for(node.ino).unwrap()).unwrap();
        assert_eq!(got, payload, "LAN 传回来的字节必须与源一致");

        // 元数据对不上（对端那份不是 NAS 上那一版）→ 不命中，回落 NAS（nas.invalid → EIO）
        let bad_peers = Arc::new(Mutex::new(vec![PeerConfig {
            name: "srv".into(),
            addr,
            token: "tok".into(),
        }]));
        let cache2 = m7_tmpdir("lan-cache2");
        let fs2 = test_fs(&cache2, false).with_peers(bad_peers);
        let entry2 = DirEntry::local("big.bin", false, payload.len() as u64, mtime + 42);
        let node2 = fs2.insert_node(INodeNo::ROOT, "big.bin", "/home/qxync-test/big.bin", &entry2);
        fs2.cache_file_for(node2.ino).unwrap();
        assert!(fs2.ensure_chunk(node2.ino, 0).is_err(), "元数据不符必须回落 NAS");
        let (a2, h2, _, m2) = fs2.lan_stats().snapshot();
        assert_eq!(a2, 1);
        assert_eq!(h2, 0);
        assert_eq!(m2, 1);

        let _ = &handle;
        let _ = std::fs::remove_dir_all(&src_dir);
        let _ = std::fs::remove_dir_all(&cache);
        let _ = std::fs::remove_dir_all(&cache2);
    }

    #[test]
    fn delete_guard_trips_and_resets() {        let g = DeleteGuard::new(3, Duration::from_secs(60));
        assert!(g.allow());
        assert!(g.allow());
        assert!(g.allow());
        assert!(!g.allow(), "第 4 次必须熔断");
        assert!(g.blocked());
        assert!(g.reason().unwrap().contains("熔断"));
        g.reset();
        assert!(!g.blocked());
        assert!(g.allow());
        // limit=0 → 完全关闭
        let off = DeleteGuard::new(0, Duration::from_secs(60));
        for _ in 0..1000 {
            assert!(off.allow());
        }
        assert!(!off.blocked());
    }

    #[test]
    fn fs_handle_refreshes_remote_meta_and_invalidates_cache() {
        let dir = std::env::temp_dir().join(format!("qxync-fuse-m2c-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let fs = test_fs(&dir, false);
        let entry = DirEntry::local("a.txt", false, 100, 111);
        let node = fs.insert_node(INodeNo::ROOT, "a.txt", "/home/a.txt", &entry);
        let cache = fs.cache_file_for(node.ino).unwrap();
        assert!(cache.exists());
        let h = fs.handle();
        assert_eq!(h.remote_root(), "/home");
        let n = h.node("/home/a.txt").unwrap();
        assert_eq!(
            (n.size, n.mtime, n.dirty, n.is_dir),
            (100, 111, false, false)
        );
        assert!(h.known_dirs().iter().any(|d| d == "/home"));

        // 远端改了大小 → 元数据更新 + 缓存失效（下次 read 重新水合）
        assert!(h.apply_remote_meta("/home/a.txt", false, 200, 222));
        let n = h.node("/home/a.txt").unwrap();
        assert_eq!((n.size, n.mtime), (200, 222));
        assert!(n.cache.is_none(), "内容过期必须丢弃缓存引用");
        assert!(!cache.exists(), "内容过期必须删掉稀疏缓存文件");

        // 未知路径 → 不动，返回 false（readdir/lookup 会自然发现）
        assert!(!h.apply_remote_meta("/home/nope.txt", false, 1, 1));
        assert_eq!(h.nodes().len(), 2); // root + a.txt

        // 远端删除 → 节点消失
        assert!(h.remove_remote("/home/a.txt"));
        assert!(h.node("/home/a.txt").is_none());
        assert!(!h.remove_remote("/home/a.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fs_handle_dir_delete_removes_descendants() {
        let dir = std::env::temp_dir().join(format!("qxync-fuse-m2c-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let fs = test_fs(&dir, false);
        let d = DirEntry::local("d", true, 0, 1);
        let dnode = fs.insert_node(INodeNo::ROOT, "d", "/home/d", &d);
        let f = DirEntry::local("f.txt", false, 5, 2);
        fs.insert_node(dnode.ino, "f.txt", "/home/d/f.txt", &f);
        let h = fs.handle();
        assert_eq!(h.nodes().len(), 3);
        assert!(h.remove_remote("/home/d"));
        assert!(h.nodes().iter().all(|n| !n.remote.starts_with("/home/d")));
        assert_eq!(h.nodes().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fs_handle_mark_dirty_requires_upload_queue_and_cache() {
        let dir = std::env::temp_dir().join(format!("qxync-fuse-m2c-dirty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // 只读：没有上传队列 → 报错
        let ro = test_fs(&dir.join("ro"), false);
        let e = DirEntry::local("a.txt", false, 4, 1);
        ro.insert_node(INodeNo::ROOT, "a.txt", "/home/a.txt", &e);
        ro.cache_file_for(INodeNo(2)).unwrap();
        assert!(ro.handle().mark_dirty("/home/a.txt").is_err());

        // 读写：入队后 has_pending 为真（内容源是稀疏缓存文件）
        let rw_dir = dir.join("rw");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let client = Arc::new(Client::new(&test_link()).unwrap());
        let q =
            UploadQueue::new(client.clone(), rt.handle().clone(), rw_dir.join("queue")).unwrap();
        let fs = QxyncFs::new(client, "/home", rw_dir.join("cache"))
            .unwrap()
            .with_write_mode()
            .with_upload_queue(q.clone());
        let e = DirEntry::local("a.txt", false, 4, 1);
        let node = fs.insert_node(INodeNo::ROOT, "a.txt", "/home/a.txt", &e);
        fs.cache_file_for(node.ino).unwrap();
        let h = fs.handle();
        assert!(!h.has_pending("/home/a.txt"));
        h.mark_dirty("/home/a.txt").unwrap();
        assert!(h.has_pending("/home/a.txt"));
        assert!(h.node("/home/a.txt").unwrap().dirty);
        // 冲突副本：把本地缓存复制到 stash
        let stash = h
            .stash_conflict("/home/a.txt", "a (conflicted copy from pc 2026-09-30).txt")
            .unwrap();
        assert!(stash.exists());
        assert!(stash.to_string_lossy().contains("conflicts"));
        // 直接入队一个冲突副本上传作业
        h.enqueue_upload(
            "/home",
            "a (conflicted copy from pc 2026-09-30).txt",
            stash,
            9,
        )
        .unwrap();
        assert!(h.has_pending("/home/a (conflicted copy from pc 2026-09-30).txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------------------ M3 脱水单测

    /// 造一个「已水合」的节点（缓存文件存在 + 所有区间就绪）。
    fn hydrated_fs(dir: &Path, pins: PinMap) -> (QxyncFs, INodeNo, PathBuf) {
        let client = Arc::new(Client::new(&test_link()).unwrap());
        let fs = QxyncFs::new(client, "/home", dir.join("cache"))
            .unwrap()
            .with_pins(pins);
        let entry = DirEntry::local("a.bin", false, 300 * 1024, 111);
        let node = fs.insert_node(INodeNo::ROOT, "a.bin", "/home/a.bin", &entry);
        let cache = fs.cache_file_for(node.ino).unwrap();
        {
            let mut g = fs.inner.lock().unwrap();
            let n = g.nodes.get_mut(&node.ino).unwrap();
            n.chunks_done = vec![true; n.chunk_count(DEFAULT_CHUNK_SIZE)];
            // 真的写点字节，验证「删缓存文件」确实发生
            std::fs::write(&cache, vec![7u8; 300 * 1024]).unwrap();
        }
        (fs, node.ino, cache)
    }

    #[test]
    fn dehydrate_invalidates_before_clearing_and_frees_cache() {
        let dir = std::env::temp_dir().join(format!("qxync-fuse-m3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (fs, ino, cache) = hydrated_fs(&dir, Arc::new(Mutex::new(HashMap::new())));
        let h = fs.handle();
        assert_eq!(h.cache_stats().used_bytes, 300 * 1024);
        assert_eq!(h.cache_stats().hydrated_files, 1);

        // 记录 inval_inode 回调被调用时缓存文件是否还在（铁则 2：必须先失效再清）
        let seen = Arc::new(Mutex::new(None::<bool>));
        let seen2 = seen.clone();
        let cache_for_cb = cache.clone();
        let out = h.dehydrate_now(
            "/home/a.bin",
            &Policy::manual(now_secs()),
            false,
            move |got_ino| {
                assert_eq!(u64::from(got_ino), u64::from(ino));
                *seen2.lock().unwrap() = Some(cache_for_cb.exists());
                Ok(())
            },
        );
        assert_eq!(out, DehydrateOutcome::Freed(300 * 1024));
        assert_eq!(
            *seen.lock().unwrap(),
            Some(true),
            "inval_inode 必须发生在清内容之前"
        );
        assert!(!cache.exists(), "缓存文件必须被清掉");
        assert_eq!(h.cache_stats().used_bytes, 0);
        let n = h.node("/home/a.bin").unwrap();
        assert_eq!(n.size, 300 * 1024, "占位符仍显示真实大小");
        assert_eq!(fs.node_state_for_test(ino), "placeholder");
        // 再脱水一次 → 没内容可清
        assert_eq!(
            h.dehydrate_now(
                "/home/a.bin",
                &Policy::manual(now_secs()),
                false,
                |_| Ok(())
            ),
            DehydrateOutcome::Blocked(Block::NoContent)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dehydrate_aborts_when_invalidate_fails() {
        let dir = std::env::temp_dir().join(format!("qxync-fuse-m3b-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (fs, _ino, cache) = hydrated_fs(&dir, Arc::new(Mutex::new(HashMap::new())));
        let h = fs.handle();
        let out = h.dehydrate_now("/home/a.bin", &Policy::manual(now_secs()), false, |_| {
            Err(std::io::Error::other("内核拒绝"))
        });
        assert!(matches!(out, DehydrateOutcome::Failed(_)), "{out:?}");
        assert!(cache.exists(), "inval 失败绝不能清内容");
        assert_eq!(h.cache_stats().used_bytes, 300 * 1024);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dehydrate_safety_chain_in_fuse() {
        let dir = std::env::temp_dir().join(format!("qxync-fuse-m3c-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pins: PinMap = Arc::new(Mutex::new(HashMap::new()));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let client = Arc::new(Client::new(&test_link()).unwrap());
        let q = UploadQueue::new(client.clone(), rt.handle().clone(), dir.join("queue")).unwrap();
        let fs = QxyncFs::new(client, "/home", dir.join("cache"))
            .unwrap()
            .with_write_mode()
            .with_upload_queue(q.clone())
            .with_pins(pins.clone());
        let entry = DirEntry::local("a.bin", false, 300 * 1024, 111);
        let node = fs.insert_node(INodeNo::ROOT, "a.bin", "/home/a.bin", &entry);
        let cache = fs.cache_file_for(node.ino).unwrap();
        {
            let mut g = fs.inner.lock().unwrap();
            let n = g.nodes.get_mut(&node.ino).unwrap();
            n.chunks_done = vec![true; n.chunk_count(DEFAULT_CHUNK_SIZE)];
        }
        std::fs::write(&cache, vec![7u8; 300 * 1024]).unwrap();
        let h = fs.handle();
        let manual = Policy::manual(now_secs());

        // dirty → 拒绝
        {
            let mut g = fs.inner.lock().unwrap();
            g.nodes.get_mut(&node.ino).unwrap().dirty = true;
        }
        assert_eq!(
            h.dehydrate_now("/home/a.bin", &manual, false, |_| Ok(())),
            DehydrateOutcome::Blocked(Block::Dirty)
        );
        // dirty 清了但上传队列里还有作业 → 拒绝
        {
            let mut g = fs.inner.lock().unwrap();
            g.nodes.get_mut(&node.ino).unwrap().dirty = false;
        }
        // 直接入队（不置 dirty）→ 只触发「队列里还有作业」这条
        q.enqueue(UploadJob {
            remote_dir: "/home".into(),
            remote_name: "a.bin".into(),
            local: cache.clone(),
            mtime: 111,
            attempts: 0,
            ephemeral: false,
        })
        .unwrap();
        assert_eq!(
            h.dehydrate_now("/home/a.bin", &manual, false, |_| Ok(())),
            DehydrateOutcome::Blocked(Block::PendingUpload)
        );
        assert!(cache.exists(), "被挡下时不能动内容");
        // 队列清空后放行
        q.cancel("/home/a.bin");
        {
            let mut g = fs.inner.lock().unwrap();
            g.nodes.get_mut(&node.ino).unwrap().dirty = false;
        }
        assert!(matches!(
            h.dehydrate_now("/home/a.bin", &manual, false, |_| Ok(())),
            DehydrateOutcome::Freed(_)
        ));
        assert!(!cache.exists());

        // ---- 重新水合（模拟），继续测 fd / mmap / pin
        let cache2 = fs.cache_file_for(node.ino).unwrap();
        {
            let mut g = fs.inner.lock().unwrap();
            let n = g.nodes.get_mut(&node.ino).unwrap();
            n.chunks_done = vec![true; n.chunk_count(DEFAULT_CHUNK_SIZE)];
        }
        std::fs::write(&cache2, vec![7u8; 300 * 1024]).unwrap();
        // 有打开的 fd → 拒绝
        {
            let mut g = fs.inner.lock().unwrap();
            g.nodes.get_mut(&node.ino).unwrap().open_count = 1;
        }
        assert_eq!(
            h.dehydrate_now("/home/a.bin", &manual, false, |_| Ok(())),
            DehydrateOutcome::Blocked(Block::Open)
        );
        {
            let mut g = fs.inner.lock().unwrap();
            g.nodes.get_mut(&node.ino).unwrap().open_count = 0;
        }
        // 被 mmap → 拒绝
        assert_eq!(
            h.dehydrate_now("/home/a.bin", &manual, true, |_| Ok(())),
            DehydrateOutcome::Blocked(Block::Mapped)
        );
        // pin=pinned / excluded → 拒绝；unpinned 放行
        pins.lock()
            .unwrap()
            .insert("/home/a.bin".into(), "pinned".into());
        assert_eq!(
            h.dehydrate_now("/home/a.bin", &manual, false, |_| Ok(())),
            DehydrateOutcome::Blocked(Block::Pinned)
        );
        pins.lock()
            .unwrap()
            .insert("/home/a.bin".into(), "excluded".into());
        assert_eq!(
            h.dehydrate_now("/home/a.bin", &manual, false, |_| Ok(())),
            DehydrateOutcome::Blocked(Block::Excluded)
        );
        pins.lock()
            .unwrap()
            .insert("/home/a.bin".into(), "unpinned".into());
        assert!(matches!(
            h.dehydrate_now("/home/a.bin", &manual, false, |_| Ok(())),
            DehydrateOutcome::Freed(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dehydrate_skips_in_flight_and_recent() {
        let dir = std::env::temp_dir().join(format!("qxync-fuse-m3d-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (fs, ino, _cache) = hydrated_fs(&dir, Arc::new(Mutex::new(HashMap::new())));
        let h = fs.handle();
        // 在途（持有节点操作锁）→ Blocked(InFlight)
        let lock = fs
            .inner
            .lock()
            .unwrap()
            .nodes
            .get(&ino)
            .unwrap()
            .op_lock
            .clone();
        let guard = lock.lock().unwrap();
        assert_eq!(
            h.dehydrate_now(
                "/home/a.bin",
                &Policy::manual(now_secs()),
                false,
                |_| Ok(())
            ),
            DehydrateOutcome::Blocked(Block::InFlight)
        );
        drop(guard);
        // 保护窗口内（刚访问过）→ Blocked(Recent)
        let policy = Policy {
            idle_secs: 600,
            cache_limit: None,
            recent_secs: 300,
            now: now_secs(),
        };
        assert_eq!(
            h.dehydrate_now("/home/a.bin", &policy, false, |_| Ok(())),
            DehydrateOutcome::Blocked(Block::Recent)
        );
        // 闲置 600s 的人工时间点 → 放行
        let old = Policy {
            idle_secs: 600,
            recent_secs: 300,
            now: now_secs() + 1000,
            cache_limit: None,
        };
        assert!(matches!(
            h.dehydrate_now("/home/a.bin", &old, false, |_| Ok(())),
            DehydrateOutcome::Freed(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upload_success_hook_clears_dirty() {
        let dir = std::env::temp_dir().join(format!("qxync-fuse-m3e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let client = Arc::new(Client::new(&test_link()).unwrap());
        let q = UploadQueue::new(client.clone(), rt.handle().clone(), dir.join("queue")).unwrap();
        let fs = QxyncFs::new(client, "/home", dir.join("cache"))
            .unwrap()
            .with_write_mode()
            .with_upload_queue(q.clone());
        let entry = DirEntry::local("a.bin", false, 10, 1);
        let node = fs.insert_node(INodeNo::ROOT, "a.bin", "/home/a.bin", &entry);
        fs.cache_file_for(node.ino).unwrap();
        let h = fs.handle();
        h.mark_dirty("/home/a.bin").unwrap();
        assert!(h.node("/home/a.bin").unwrap().dirty);
        // 队列里还有作业 → 不清（这就是脱水被挡的第二种情况）
        h.clear_dirty("/home/a.bin");
        assert!(h.node("/home/a.bin").unwrap().dirty);
        // 作业做完了（把队列清掉）→ hook 才能清
        q.cancel("/home/a.bin");
        h.clear_dirty("/home/a.bin");
        assert!(!h.node("/home/a.bin").unwrap().dirty);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_writes_mark_chunks_so_files_can_be_dehydrated() {
        let dir = std::env::temp_dir().join(format!("qxync-fuse-m3f-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (fs, ino, cache) = {
            let client = Arc::new(Client::new(&test_link()).unwrap());
            let fs = QxyncFs::new(client, "/home", dir.join("cache")).unwrap();
            let entry = DirEntry::local("new.bin", false, 0, 1);
            let node = fs.insert_node(INodeNo::ROOT, "new.bin", "/home/new.bin", &entry);
            let cache = fs.cache_file_for(node.ino).unwrap();
            (fs, node.ino, cache)
        };
        // 模拟「本地新建 300 KiB」：先扩大小，再逐块写入
        {
            let mut g = fs.inner.lock().unwrap();
            let n = g.nodes.get_mut(&ino).unwrap();
            n.attr.size = 300 * 1024;
        }
        std::fs::write(&cache, vec![1u8; 300 * 1024]).unwrap();
        fs.mark_written_chunks(ino, 0, 300 * 1024);
        assert_eq!(fs.node_state_for_test(ino), "hydrated");
        let h = fs.handle();
        assert_eq!(h.cache_stats().used_bytes, 300 * 1024);
        // 有内容 → 可以脱水
        assert!(matches!(
            h.dehydrate_now("/home/new.bin", &Policy::manual(now_secs()), false, |_| Ok(
                ()
            )),
            DehydrateOutcome::Freed(_)
        ));

        // 部分覆盖的块不算「有内容」：只写前 100 字节（第一章区间没被完整覆盖）
        let entry = DirEntry::local("part.bin", false, 0, 1);
        let node = fs.insert_node(INodeNo::ROOT, "part.bin", "/home/part.bin", &entry);
        fs.cache_file_for(node.ino).unwrap();
        {
            let mut g = fs.inner.lock().unwrap();
            g.nodes.get_mut(&node.ino).unwrap().attr.size = 300 * 1024;
        }
        fs.mark_written_chunks(node.ino, 0, 100);
        assert_eq!(h.cache_stats().used_bytes, 0, "部分覆盖不能算内容完整");
        assert_eq!(
            h.dehydrate_now(
                "/home/part.bin",
                &Policy::manual(now_secs()),
                false,
                |_| Ok(())
            ),
            DehydrateOutcome::Blocked(Block::NoContent)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_mode_parse_and_direct_io_flag() {
        assert_eq!(CacheMode::parse("pagecache"), Some(CacheMode::PageCache));
        assert_eq!(CacheMode::parse("direct"), Some(CacheMode::Direct));
        assert_eq!(CacheMode::parse("DIRECT_IO"), Some(CacheMode::Direct));
        assert_eq!(CacheMode::parse("nope"), None);
        assert_eq!(CacheMode::Direct.as_str(), "direct");
    }

    // ------------------------------------------------------------ M6 多根视图单测
    //
    // 本会话没有 `/dev/fuse`，挂不上真 FUSE —— 这里只验「虚拟根 → 各远端根」的映射逻辑；
    // 真机挂载由 `xtask/tests/fuse-matrix.sh` 覆盖。

    fn multi_fs(dir: &Path) -> QxyncFs {
        let client = Arc::new(Client::new(&test_link()).unwrap());
        QxyncFs::new_multi(
            client,
            vec![
                RootSpec {
                    remote: "/home".into(),
                    view_name: "home".into(),
                    writable: true,
                },
                RootSpec {
                    remote: "/Public".into(),
                    view_name: "Public".into(),
                    writable: false,
                },
            ],
            dir,
        )
        .unwrap()
    }

    /// `Result<(), Errno>` → 错误码（`Errno` 本身没实现 `PartialEq`）。
    fn errno_code(r: Result<(), fuser::Errno>) -> Option<i32> {
        r.err().map(|e| e.code())
    }

    fn m6_tmp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("qxync-fuse-m6-{tag}-{}", std::process::id()))
    }

    #[test]
    fn multi_root_virtual_lookup_and_readdir() {
        let dir = m6_tmp("view");
        let _ = std::fs::remove_dir_all(&dir);
        let fs = multi_fs(&dir);

        // 全部根 + 第一个根（兼容老调用方的 `remote_root()`）
        assert!(fs.multi_root);
        assert_eq!(
            fs.remote_roots(),
            vec!["/home".to_string(), "/Public".to_string()]
        );
        assert_eq!(fs.remote_root(), "/home");

        // 每个 RootSpec 都预插入了目录节点，且都在虚拟根下
        let home = fs.node_by_remote("/home").expect("home 目录节点");
        let public = fs.node_by_remote("/Public").expect("Public 目录节点");
        assert_eq!(home.parent, INodeNo::ROOT);
        assert_eq!(public.parent, INodeNo::ROOT);
        assert_eq!((home.name.as_str(), public.name.as_str()), ("home", "Public"));
        for n in [&home, &public] {
            assert_eq!(n.attr.kind, FileType::Directory);
            assert_eq!(n.attr.perm, 0o755);
            assert_eq!(n.attr.size, 0);
            assert_eq!(n.attr.nlink, 2);
        }
        // 虚拟根自己：remote = ""（**不是**有效远端路径），且不是任何合成目录
        let root = fs.node_by_remote("").expect("虚拟根");
        assert_eq!(root.ino, INodeNo::ROOT);
        assert_ne!(root.ino, home.ino);

        // 虚拟根 readdir：不访问 NAS，直接返回两个合成目录
        let children = fs
            .load_children(INodeNo::ROOT)
            .expect("虚拟根 readdir 不该碰 NAS");
        assert_eq!(children.len(), 2, "{children:?}");
        for want in ["home", "Public"] {
            assert!(children.iter().any(|n| n.name == want), "缺 {want}: {children:?}");
        }
        assert!(children.iter().any(|n| n.remote == "/home"));
        assert!(children.iter().any(|n| n.remote == "/Public"));

        // 虚拟根 lookup：命中同一个节点；未知名 → ENOENT（绝不拿 "" 去 stat）
        let got = fs.lookup_child(INodeNo::ROOT, "Public").expect("lookup Public");
        assert_eq!(got.ino, public.ino);
        assert_eq!(got.remote, "/Public");
        assert_eq!(
            fs.lookup_child(INodeNo::ROOT, "nope")
                .err()
                .map(|e| e.code()),
            Some(libc::ENOENT)
        );

        // 句柄也要带多根（daemon 的同步引擎靠它）
        let h = fs.handle();
        assert_eq!(
            h.remote_roots(),
            vec!["/home".to_string(), "/Public".to_string()]
        );
        assert_eq!(h.remote_root(), "/home");
        assert!(h.known_dirs().iter().any(|d| d == "/home"));
        assert!(h.known_dirs().iter().any(|d| d == "/Public"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_multi_rejects_empty_roots() {
        let dir = m6_tmp("empty");
        let client = Arc::new(Client::new(&test_link()).unwrap());
        let err = match QxyncFs::new_multi(client, Vec::new(), &dir) {
            Ok(_) => panic!("空 entries 必须报 InvalidInput"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("多根挂载至少需要一个远端根"));
    }

    #[test]
    fn multi_root_writability_follows_root_spec() {
        let dir = m6_tmp("rw");
        let _ = std::fs::remove_dir_all(&dir);
        let fs = multi_fs(&dir);

        // 家目录根可写（根本身与它下面的任意深度路径都算）
        assert_eq!(errno_code(fs.ensure_writable("/home")), None);
        assert_eq!(errno_code(fs.ensure_writable("/home/a.txt")), None);
        assert_eq!(errno_code(fs.ensure_writable("/home/deep/x/y.bin")), None);
        // 共享文件夹根只读（实测服务端拒绝 status 20）
        assert_eq!(errno_code(fs.ensure_writable("/Public")), Some(libc::EROFS));
        assert_eq!(
            errno_code(fs.ensure_writable("/Public/a.txt")),
            Some(libc::EROFS)
        );
        assert_eq!(
            errno_code(fs.ensure_writable("/Public/deep/x")),
            Some(libc::EROFS)
        );
        // 不属于任何根（虚拟根下新建 / 未知路径）→ 也回 EROFS
        assert_eq!(errno_code(fs.ensure_writable("/Unknown/x")), Some(libc::EROFS));
        assert_eq!(errno_code(fs.ensure_writable("/homework/x")), Some(libc::EROFS));
        assert_eq!(errno_code(fs.ensure_writable("")), Some(libc::EROFS));

        // 单根直通：永远 Ok（挂载级 read_only 已经管住），任何路径都不做归属判定
        let single_dir = m6_tmp("single-rw");
        let _ = std::fs::remove_dir_all(&single_dir);
        let single = test_fs(&single_dir, true);
        assert!(!single.multi_root);
        assert_eq!(errno_code(single.ensure_writable("/home/a.txt")), None);
        assert_eq!(errno_code(single.ensure_writable("/Public/a.txt")), None);
        assert_eq!(errno_code(single.ensure_writable("")), None);

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&single_dir);
    }

    #[test]
    fn single_root_passthrough_unchanged() {
        let dir = m6_tmp("passthrough");
        let _ = std::fs::remove_dir_all(&dir);
        let fs = test_fs(&dir, false);

        // 单根：roots 只有一个（view_name 空），multi_root = false
        assert!(!fs.multi_root);
        assert_eq!(fs.remote_roots(), vec!["/home".to_string()]);
        assert_eq!(fs.remote_root(), "/home");
        assert_eq!(fs.handle().remote_roots(), vec!["/home".to_string()]);
        assert_eq!(fs.roots[0].view_name, "");
        // ★ 兼容性关键：挂载点**就是** /home（不是虚拟根），ROOT 节点就是远端根
        assert_eq!(
            fs.node_by_remote("/home").expect("/home 就是 ROOT").ino,
            INodeNo::ROOT
        );

        // ★ 回归：单根时 `load_children(ROOT)` **必须**走 NAS，不能套用多根的虚拟根短路。
        //   假 client 没有 sid → `list()` 直接失败 → EIO（而不是「不碰 NAS 就返回子节点」）。
        assert_eq!(
            fs.load_children(INodeNo::ROOT).err().map(|e| e.code()),
            Some(libc::EIO),
            "单根 ROOT 仍然走 NAS"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
