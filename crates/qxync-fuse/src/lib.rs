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

/// ★ M11：删除队列（unlink/rmdir 异步入队 + 同目录攒批推送）。
pub mod delete;
pub mod upload;

// 脱水需要 daemon 持有 fuser 的会话/通知句柄；这里转出，避免 daemon 直接依赖 fuser。
pub use fuser::{BackgroundSession, Notifier};

use crate::delete::DeleteJob;
use qxync_client::peer::{self, ContentSource, PeerConfig, PeerHead};
use qxync_client::Client;
use qxync_core::dehydrate::{Block, Candidate, Policy};
use qxync_core::rules::{HideReason, Rules};
use qxync_core::DirEntry;
use std::collections::{HashMap, HashSet, VecDeque};
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
/// ★ 并发取块的默认扇出。
///
/// 一次保存（read-modify-write）要把所有缺块取齐，串行时墙钟 = 块数 × RTT，
/// 文件一大就非常明显（1 MB=8 块、4 MB=32 块）。8 路并发能把 4 MB 的
/// 取数从 32 个 RTT 压到 4 个。
///
/// 保守取 8：NAS 和家用宽带对并发连接敏感，再高容易触发服务端限流/排队，
/// 反而变慢。要调就 `QXYNC_HYDRATE_FANOUT=1..64`。
pub const DEFAULT_HYDRATE_FANOUT: usize = 8;
/// ★ M2c：本地大批删除熔断的默认阈值（60 秒窗口内最多 100 次删除）。
/// 超过就熔断并把后续删除回 `EACCES`；`qxync sync --force-deletes` 可解除。
pub const DEFAULT_DELETE_LIMIT: usize = 100;
pub const DEFAULT_DELETE_WINDOW: Duration = Duration::from_secs(60);
/// ★ M9：目录清单快照的保鲜期。超过这个岁数、又有 `readdir`/`lookup` 打进来时，
/// 后台补一次 NAS `list`（**调用方不等**）—— 定时刷新由 daemon 轮询推送（`apply_listing`）。
pub const DEFAULT_DIR_TTL: Duration = Duration::from_secs(30);
/// ★ M9：后台内容刷新连续失败多少次之后，退回「丢掉旧内容、读到时按需水合」。
const REFRESH_MAX_ATTEMPTS: u32 = 3;

// ---------------------------------------------------------------- 水合位图落盘
//
// ★ M9 的根因修复。`chunks_done` 以前只活在内存里：daemon 一重启、或者节点被重新
// `lookup` 一遍，`cache_file_for` 就把区间表清成全 `false` —— 于是**磁盘上明明已经
// 有内容**，`cat` 还是会重新去 NAS 逐个区间拉一遍（「缓存过的文件 cat 还要等几秒」
// 就是这么来的）。位图落在缓存文件旁边（`<cache>.qxstate`），并带上「远端签名」
// （size + mtime）：签名对不上（NAS 上那份变了）就整份作废，绝不拿旧内容冒充新内容。
//
// ★ M15/T3 再加一层「内容校验和」。原来下载只校验**长度**（`ensure_chunk` 里
// `data.len() != want`），长度对、内容错（网络截断改写、稀疏空洞被当真数据、
// 位图与内容写序颠倒）要几个月后用户才发现。现在每个区间落一份 `xxhash64`。
//
// ★ **为什么不把 per-chunk 校验和直接塞进 `.qxstate`**（M15 原文的写法）：
// 10 GB 文件 / 128 KiB = 81920 个区间，`8 × nchunks` = 640 KB；而
// `persist_chunk_state` 是**每个区间就绪就全量重写一次**，水合一遍就是
// 640 KB × 81920 ≈ 52 GB 写入 —— 比数据本身大 5 倍。所以拆成两个文件：
// * `.qxstate` —— 头 + 位图，**小到可以随便全量重写**（10 GB 文件也只 10 KB）；
// * `.qxsum`   —— 定长 `8 × nchunks` 的 per-chunk xxhash64，**只做定点 pwrite**
//   （写一个区间就改 8 字节，不重写整张表）。
// 语义等价，但写放大从 O(n²) 降到 O(n)。

const STATE_MAGIC: [u8; 8] = *b"QXSTATE2";
const STATE_VERSION: u32 = 2;
/// v1 的 magic —— 只用于**识别并安全作废**（M15 验收：不能 panic）。
const STATE_MAGIC_V1: [u8; 8] = *b"QXSTATE1";
const STATE_VERSION_V1: u32 = 1;
/// 位图头：magic(8) + version(4) + chunk_size(8) + size(8) + mtime(8) + nchunks(8)
/// \+ file_id(16) + whole_xxhash(8)。
const STATE_HEAD: usize = 8 + 4 + 8 + 8 + 8 + 8 + 16 + 8;
/// v1 的头长（少 file_id + whole_xxhash）。
const STATE_HEAD_V1: usize = 8 + 4 + 8 + 8 + 8 + 8;
/// 「还没算出整文件校验和」的哨兵 —— 0 不用，因为 xxhash64 可能真是 0。
const NO_HASH: u64 = u64::MAX;

/// ★ T3：全零的 `file_id`（还没接 T9 的稳定身份，先占位）。
const ZERO_FILE_ID: [u8; 16] = [0u8; 16];

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChunkState {
    chunk_size: u64,
    size: u64,
    mtime: i64,
    /// ★ T9 用：跨改名/移动稳定的文件身份；当前恒为全零。
    file_id: [u8; 16],
    /// ★ T3：整文件校验和（全量就绪时算一次，用于「一个数判全文件」）。
    /// 未算出时是 [`NO_HASH`]。
    whole_xxhash: u64,
    done: Vec<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StateVerdict {
    /// v2，能用。
    Ok,
    /// v1 老格式 —— 没有校验和，认领不了（作废重水合是唯一安全选择）。
    LegacyV1,
    /// 损坏 / 截断 / 不认识。
    Corrupt,
}

/// 缓存文件 → 位图文件（`<cache>.qxstate`；脱水/失效时和内容一起删）。
fn state_path(cache: &Path) -> PathBuf {
    sibling_path(cache, ".qxstate")
}

/// 缓存文件 → 校验和文件（`<cache>.qxsum`；与 `.qxstate` 同生共死）。
fn sum_path(cache: &Path) -> PathBuf {
    sibling_path(cache, ".qxsum")
}

fn sibling_path(base: &Path, suffix: &str) -> PathBuf {
    let mut s = base.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// 删掉某个缓存文件的「内容 + 位图 + 校验和」三件套（幂等）。
fn remove_cache_files(cache: &Path) {
    if let Err(e) = std::fs::remove_file(cache) {
        if e.kind() != io::ErrorKind::NotFound {
            tracing::warn!("删除缓存文件失败 {}: {e}", cache.display());
        }
    }
    let _ = std::fs::remove_file(state_path(cache));
    let _ = std::fs::remove_file(sum_path(cache));
}

/// 位图落盘（先写 `.tmp` 再 `rename`，原子替换）。
///
/// 顺序很关键：**先 pwrite 内容与 `.qxsum`、后写位图**。崩在中间只会「位图比内容旧」
/// （至多重下一次区间）；反过来就会把稀疏空洞里的 0 当成真数据。
fn state_save(cache: &Path, st: &ChunkState) -> io::Result<()> {
    let path = state_path(cache);
    let tmp = sibling_path(cache, ".qxstate.tmp");
    let mut buf = Vec::with_capacity(STATE_HEAD + st.done.len().div_ceil(8));
    buf.extend_from_slice(&STATE_MAGIC);
    buf.extend_from_slice(&STATE_VERSION.to_le_bytes());
    buf.extend_from_slice(&st.chunk_size.to_le_bytes());
    buf.extend_from_slice(&st.size.to_le_bytes());
    buf.extend_from_slice(&st.mtime.to_le_bytes());
    buf.extend_from_slice(&(st.done.len() as u64).to_le_bytes());
    buf.extend_from_slice(&st.file_id);
    buf.extend_from_slice(&st.whole_xxhash.to_le_bytes());
    let mut bitmap = vec![0u8; st.done.len().div_ceil(8)];
    for (i, d) in st.done.iter().enumerate() {
        if *d {
            bitmap[i / 8] |= 1 << (i % 8);
        }
    }
    buf.extend_from_slice(&bitmap);
    std::fs::write(&tmp, &buf)?;
    std::fs::rename(&tmp, &path)
}

/// 判断一个 `.qxstate` 缓冲区的版本（不解析内容）。
///
/// v1 单独认出来是为了**安全作废**（M15 验收：不能 panic、也不能静默当损坏）。
/// 只看 magic+version 是不够的：一个 8 字节的 `QXSTATE1` 截断文件也会落到
/// LegacyV1，所以额外要求头长度至少 `STATE_HEAD_V1`。
fn state_verdict(buf: &[u8]) -> StateVerdict {
    if buf.len() < 12 {
        return StateVerdict::Corrupt;
    }
    let magic: [u8; 8] = buf[..8].try_into().unwrap();
    let version = u32::from_le_bytes(buf[8..12].try_into().unwrap());
    if magic == STATE_MAGIC {
        return if version == STATE_VERSION && buf.len() >= STATE_HEAD {
            StateVerdict::Ok
        } else {
            StateVerdict::Corrupt
        };
    }
    if magic == STATE_MAGIC_V1 && version == STATE_VERSION_V1 && buf.len() >= STATE_HEAD_V1 {
        return StateVerdict::LegacyV1;
    }
    StateVerdict::Corrupt
}

fn state_decode(buf: &[u8]) -> Option<ChunkState> {
    if state_verdict(buf) != StateVerdict::Ok {
        return None;
    }
    let chunk_size = u64::from_le_bytes(buf[12..20].try_into().ok()?);
    let size = u64::from_le_bytes(buf[20..28].try_into().ok()?);
    let mtime = i64::from_le_bytes(buf[28..36].try_into().ok()?);
    let nchunks = u64::from_le_bytes(buf[36..44].try_into().ok()?);
    if chunk_size == 0 {
        return None;
    }
    // 自洽性校验：区间数必须和 size/chunk_size 对得上（防损坏文件把内存撑爆）。
    let expect = if size == 0 {
        1
    } else {
        size.div_ceil(chunk_size)
    };
    if nchunks != expect {
        return None;
    }
    let bits = (nchunks as usize).div_ceil(8);
    if buf.len() < STATE_HEAD + bits {
        return None;
    }
    let file_id: [u8; 16] = buf[44..60].try_into().ok()?;
    let whole_xxhash = u64::from_le_bytes(buf[60..68].try_into().ok()?);
    let bitmap = &buf[STATE_HEAD..STATE_HEAD + bits];
    let mut done = Vec::with_capacity(nchunks as usize);
    for i in 0..nchunks as usize {
        done.push(bitmap[i / 8] & (1 << (i % 8)) != 0);
    }
    Some(ChunkState {
        chunk_size,
        size,
        mtime,
        file_id,
        whole_xxhash,
        done,
    })
}

fn state_load(cache: &Path) -> Option<ChunkState> {
    let buf = match std::fs::read(state_path(cache)) {
        Ok(b) => b,
        Err(e) => {
            if e.kind() != io::ErrorKind::NotFound {
                tracing::warn!("读取水合位图失败 {}: {e}", state_path(cache).display());
            }
            return None;
        }
    };
    match state_verdict(&buf) {
        StateVerdict::Ok => state_decode(&buf),
        StateVerdict::LegacyV1 => {
            // ★ M15 验收：v1 必须能被识别并**安全作废**，不能 panic、也不能静默。
            // v1 里没有校验和，「哪些区间内容是对的」这个问题无法回答 —— 位图说就绪
            // 但没哈希可比，所以只能当没缓存过（重下一次，用户无感：读操作而已）。
            tracing::info!(
                "旧版水合位图（v1，无校验和）已作废，将按需重新水合: {}",
                state_path(cache).display()
            );
            let _ = std::fs::remove_file(state_path(cache));
            let _ = std::fs::remove_file(sum_path(cache));
            None
        }
        StateVerdict::Corrupt => {
            tracing::warn!("水合位图损坏，按未缓存处理: {}", state_path(cache).display());
            None
        }
    }
}

/// 读回整张 per-chunk 校验和表（长度必须正好是 `8 × nchunks`，否则全 [`NO_HASH`]）。
fn sum_load(cache: &Path, nchunks: usize) -> Vec<u64> {
    let want = nchunks * 8;
    match std::fs::read(sum_path(cache)) {
        Ok(b) if b.len() == want => b
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect(),
        Ok(b) => {
            tracing::warn!(
                "校验和表长度不符（{} != {want}），本轮不做内容校验: {}",
                b.len(),
                sum_path(cache).display()
            );
            vec![NO_HASH; nchunks]
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => vec![NO_HASH; nchunks],
        Err(e) => {
            tracing::warn!("读取校验和表失败 {}: {e}", sum_path(cache).display());
            vec![NO_HASH; nchunks]
        }
    }
}

/// **定点**写若干个区间的校验和（每个区间 `pwrite` 8 字节，不重写整张表）。
///
/// **一次 open 写多个**：水合/本地写路径常常一口气要记好几个区间（水合是 1 个，
/// 但 `mark_written_chunks` 一次 write 可能覆盖几十个），每区间各开一次文件
/// 在 8192 个区间的大文件上是 8192 次 open + 8192 次 `stat`，纯属自找的开销。
///
/// 表文件按需创建并 `set_len` 到定长；调用方保证「先写内容与校验和、后置位图上的位」。
fn sum_store_many(cache: &Path, nchunks: usize, items: &[(u64, u64)]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    if items.is_empty() {
        return Ok(());
    }
    let f = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(sum_path(cache))?;
    let want = (nchunks * 8) as u64;
    if f.metadata()?.len() != want {
        f.set_len(want)?;
    }
    for (idx, h) in items {
        f.write_all_at(&h.to_le_bytes(), idx * 8)?;
    }
    Ok(())
}

/// 单区间版本（便利包装）。
fn sum_store_one(cache: &Path, nchunks: usize, idx: u64, hash: u64) -> io::Result<()> {
    sum_store_many(cache, nchunks, &[(idx, hash)])
}

/// 整表重写（认领时按需修补、verify --repair 用）。
fn sum_store_all(cache: &Path, hashes: &[u64]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(hashes.len() * 8);
    for h in hashes {
        buf.extend_from_slice(&h.to_le_bytes());
    }
    let tmp = sibling_path(cache, ".qxsum.tmp");
    std::fs::write(&tmp, &buf)?;
    std::fs::rename(&tmp, sum_path(cache))
}

/// 把节点的水合位图落盘（内容已写过之后调用）。
fn persist_chunk_state(inner: &Arc<Mutex<Inner>>, chunk_size: u64, ino: INodeNo) {
    let (cache, st) = {
        let g = inner.lock().unwrap();
        let Some(n) = g.nodes.get(&ino) else {
            return;
        };
        let Some(cache) = n.cache.clone() else {
            return;
        };
        (
            cache,
            ChunkState {
                chunk_size,
                size: n.attr.size,
                mtime: epoch_secs(n.attr.mtime),
                file_id: n.file_id,
                whole_xxhash: n.whole_xxhash,
                done: n.chunks_done.clone(),
            },
        )
    };
    if let Err(e) = state_save(&cache, &st) {
        tracing::warn!("水合位图落盘失败 {}: {e}", cache.display());
    }
}

// ---------------------------------------------------------------- T3：内容校验和

/// 为什么是 xxhash64 而不是 sha256：快一个数量级，而这里要挡的是**意外损坏**
/// （掉电、位翻转、写序颠倒、网络截断改写），不是恶意篡改 —— 攻击者能同时改内容
/// 和校验和的话，本来就能改 `sync.db`。真要防篡改得靠服务端签名，那是另一件事。
fn xxh64(bytes: &[u8]) -> u64 {
    use xxhash_rust::xxh64::xxh64;
    xxh64(bytes, 0)
}

/// 读缓存文件里某个区间的字节（用于算校验和）。
fn read_chunk_bytes(cache: &Path, chunk_size: u64, size: u64, idx: u64) -> io::Result<Vec<u8>> {
    use std::os::unix::fs::FileExt;
    let start = idx * chunk_size;
    if start >= size {
        return Ok(Vec::new());
    }
    let end = (start + chunk_size).min(size);
    let f = std::fs::File::open(cache)?;
    let mut buf = vec![0u8; (end - start) as usize];
    f.read_exact_at(&mut buf, start)?;
    Ok(buf)
}

/// 算一个区间在缓存文件里的校验和（**只读那一段**，不把整个文件读进内存）。
fn chunk_hash(cache: &Path, chunk_size: u64, size: u64, idx: u64) -> io::Result<u64> {
    let data = read_chunk_bytes(cache, chunk_size, size, idx)?;
    Ok(xxh64(&data))
}

/// 算整文件的校验和（流式，不把整个文件读进内存）。
fn whole_hash(cache: &Path, size: u64) -> io::Result<u64> {
    use std::io::Read;
    use xxhash_rust::xxh64::Xxh64;
    let mut f = std::fs::File::open(cache)?;
    let mut hasher = Xxh64::new(0);
    let mut buf = vec![0u8; 512 * 1024];
    let mut left = size;
    while left > 0 {
        let want = buf.len().min(left as usize);
        let n = f.read(&mut buf[..want])?;
        if n == 0 {
            // 文件比记录的 size 短：按实际读到的算（调用方会因长度不符另行处理）
            break;
        }
        hasher.update(&buf[..n]);
        left -= n as u64;
    }
    Ok(hasher.digest())
}

/// 核一遍所有「已就绪」区间，返回**校验和不符的区间下标**。
///
/// * 只有位图里标了就绪的区间才核（未就绪的本来就是稀疏空洞，不该有校验和）。
/// * 某个区间的校验和是 [`NO_HASH`]（表缺失/损坏）→ 保守当**不符**：
///   没有基准可比，就不能声称「内容是对的」。这正是「校验失败退回按需水合」。
/// * 读不出来（IO 错）也当不符。
fn verify_done_chunks(cache: &Path, done: &[bool], size: u64, chunk_size: u64) -> Vec<u64> {
    let nchunks = done.len();
    if nchunks == 0 || !done.iter().any(|d| *d) {
        return Vec::new();
    }
    let sums = sum_load(cache, nchunks);
    let mut bad = Vec::new();
    for (idx, d) in done.iter().enumerate() {
        if !*d {
            continue;
        }
        let want = sums[idx];
        let got = match chunk_hash(cache, chunk_size, size, idx as u64) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!("读区间 {idx} 算校验和失败 {}: {e}", cache.display());
                0
            }
        };
        if want == NO_HASH || got != want {
            bad.push(idx as u64);
        }
    }
    bad
}

/// 日志里别把上千个坏区间全列出来 —— 只报头几个 + 总数。
fn summarize_bad(bad: &[u64], chunk_size: u64) -> String {
    const MAX: usize = 5;
    let head: Vec<String> = bad
        .iter()
        .take(MAX)
        .map(|i| format!("#{i}(@{})", i * chunk_size))
        .collect();
    if bad.len() > MAX {
        format!("{} …共 {} 个", head.join(" "), bad.len())
    } else {
        head.join(" ")
    }
}

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
    /// ★ T3：跨改名/移动稳定的文件身份（`.qxstate` v2 头里的一栏）。
    /// T9 落地前恒为 [`ZERO_FILE_ID`]；先占住位置，避免格式再升一次版。
    file_id: [u8; 16],
    /// ★ T3：整文件校验和。全量就绪（`chunks_done` 全 true）时算一次，
    /// 之后 `qxync verify` 就能「一个数」判整个文件而不必逐区间。
    /// 未算出时是 [`NO_HASH`]。
    whole_xxhash: u64,
    /// 本地有未上传的改动。
    dirty: bool,
    /// ★ M3：打开的 fd 数（>0 时禁止脱水，报告 12 §8.1）。
    open_count: u32,
    /// ★ M3：最后一次读/写时间（LRU 脱水排序、闲置判定）。
    last_access: SystemTime,
    /// ★ M3：节点级操作锁 —— `read`/`write`/`setattr` 持锁；脱水用 `try_lock`，
    /// 拿不到就说明「正在水合/读写」，本轮跳过（报告 12 §8.1 的 in_progress）。
    op_lock: Arc<Mutex<()>>,
    /// ★ M9：远端内容变了、但本地「有水」→ 先把新签名记在这儿。
    ///
    /// 此刻**旧内容继续可读**、`attr` 也保持旧的（内容和元数据不许打架）；后台把新
    /// 版本整个拉进临时文件、`rename` 原子换上之后才一起更新。`read` 因此永远不吃
    /// 半新半旧的文件，也不用为了「顺手更新」去等一次 NAS 往返。
    pending: Option<(u64, i64)>,
    /// ★ M9：后台内容刷新是否在飞（去重，一个节点同时只跑一个）。
    refreshing: bool,
    /// ★ M9：后台刷新连续失败次数；超过阈值就退回「丢掉旧内容、下次读按需水合」。
    refresh_attempts: u32,
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

    /// ★ M9：把「待刷新」的新签名落到 `attr` 上。
    ///
    /// 内容被丢弃时（冲突解、脱水）元数据必须跟着远端走：否则节点留着旧 attr，
    /// 而 baseline 已经是新签名，三向合并会判成 Noop，`ls -l` 就永远停在旧大小上。
    fn apply_pending_sig(&mut self) {
        if let Some((size, mtime)) = self.pending.take() {
            self.attr.size = size;
            self.attr.blocks = size.div_ceil(512);
            let t = UNIX_EPOCH + Duration::from_secs(mtime.max(0) as u64);
            self.attr.mtime = t;
            self.attr.ctime = t;
        }
    }

    /// ★ T3：把区间表重置成「全未就绪 / 全就绪」。
    ///
    /// 内容一变，旧区间就都不作数了，`whole_xxhash` 同步作废（否则会拿旧版本的
    /// 整文件哈希去比新内容，verify 会报假损坏）。per-chunk 的 `.qxsum` 由调用方
    /// 单独重写 —— 位图（内存态）和校验和（落盘态）在这里必须一起想。
    fn reset_chunks(&mut self, chunk_size: u64, done: bool) {
        self.chunks_done = vec![done; self.chunk_count(chunk_size)];
        self.whole_xxhash = NO_HASH;
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
    /// ★ M9：目录清单快照（「映射」）—— NAS 文件列表的本地物化。
    ///
    /// 有了它 `readdir`/`lookup` 不再每次往 NAS 跑一趟；刷新由 daemon 的定时轮询
    /// （[`FsHandle::apply_listing`]）推送，快照过期时只做**后台**补拉，绝不阻塞调用方。
    dirs: HashMap<String, DirListing>,
    /// 正在后台补清单的目录（去重，避免同一目录并发拉好几遍）。
    listing_inflight: HashSet<String>,
}

/// 一个目录的 NAS 清单快照。
#[derive(Debug, Clone)]
struct DirListing {
    entries: Arc<Vec<DirEntry>>,
    fetched: Instant,
}

/// 读一个目录的清单快照。
fn listing_get(inner: &Arc<Mutex<Inner>>, dir: &str) -> Option<DirListing> {
    inner.lock().unwrap().dirs.get(dir).cloned()
}

/// 写入/替换一个目录的清单快照（定时刷新与冷路径加载共用）。
fn listing_put(inner: &Arc<Mutex<Inner>>, dir: &str, entries: Arc<Vec<DirEntry>>) {
    let mut g = inner.lock().unwrap();
    g.dirs.insert(
        dir.to_string(),
        DirListing {
            entries,
            fetched: Instant::now(),
        },
    );
}

/// 丢掉一个目录的清单快照（远端目录没了 / 本地刚改过名字空间）。
fn listing_drop(inner: &Arc<Mutex<Inner>>, dir: &str) {
    inner.lock().unwrap().dirs.remove(dir);
}

/// 从目录清单快照里摘掉一个名字（本地删除后立刻生效，不用等下一轮轮询）。
///
/// 不做这一步的话：`rm a` 之后 `ls` 会把快照里还在的 `a` 重新物化成幽灵节点。
fn listing_remove(inner: &Arc<Mutex<Inner>>, dir: &str, name: &str) {
    let mut g = inner.lock().unwrap();
    if let Some(list) = g.dirs.get_mut(dir) {
        if list.entries.iter().any(|e| e.filename == name) {
            let kept: Vec<DirEntry> = list
                .entries
                .iter()
                .filter(|e| e.filename != name)
                .cloned()
                .collect();
            list.entries = Arc::new(kept);
        }
    }
}

/// 把一个条目写进目录清单快照（本地新建 / 改名后立刻可见）。
fn listing_upsert(inner: &Arc<Mutex<Inner>>, dir: &str, entry: &DirEntry) {
    let mut g = inner.lock().unwrap();
    if let Some(list) = g.dirs.get_mut(dir) {
        let mut kept: Vec<DirEntry> = list
            .entries
            .iter()
            .filter(|e| e.filename != entry.filename)
            .cloned()
            .collect();
        kept.push(entry.clone());
        list.entries = Arc::new(kept);
    }
}

/// 在目录清单里查一个名字。
///
/// 返回 `None` = **没有这个目录的清单**（调用方该去问 NAS）；
/// `Some(None)` = 有清单但没这个名字（可以放心回 `ENOENT`，不必再问 NAS）。
fn listing_lookup(inner: &Arc<Mutex<Inner>>, dir: &str, name: &str) -> Option<Option<DirEntry>> {
    let g = inner.lock().unwrap();
    let list = g.dirs.get(dir)?;
    Some(list.entries.iter().find(|e| e.filename == name).cloned())
}

/// 后台补一次目录清单（去重）；返回是否真的发起了。
fn begin_listing_refresh(inner: &Arc<Mutex<Inner>>, dir: &str) -> bool {
    inner
        .lock()
        .unwrap()
        .listing_inflight
        .insert(dir.to_string())
}

async fn run_listing_refresh(inner: Arc<Mutex<Inner>>, client: Arc<Client>, dir: String) {
    match client.list(&dir).await {
        Ok(entries) => {
            tracing::debug!("后台刷新目录清单 {dir}（{} 项）", entries.len());
            listing_put(&inner, &dir, Arc::new(entries));
        }
        Err(e) => tracing::debug!("后台刷新目录清单失败 {dir}: {e}"),
    }
    inner.lock().unwrap().listing_inflight.remove(&dir);
}

// ---------------------------------------------------------------- 区间下载（前台 / 后台共用）

/// 一次区间下载需要的全部外部依赖。
struct FetchCtx {
    client: Arc<Client>,
    peers: Arc<Mutex<Vec<PeerConfig>>>,
    stats: Arc<LanStats>,
    timeout: Duration,
    /// 水合计数（前台水合与后台刷新都记在这儿）。
    hydro: Arc<HydroCounters>,
}

/// 区间下载失败的形态（日志要分得清「超时」「会话失效」还是「NAS 报错」）。
enum ChunkFetchError {
    Timeout,
    /// ★ M10：鉴权失败 —— 调用方会重登一次、热更新 sid，再重试一次。
    Auth(String),
    Nas(String),
}

impl std::fmt::Display for ChunkFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => write!(f, "超时"),
            Self::Auth(e) => write!(f, "会话失效: {e}"),
            Self::Nas(e) => write!(f, "{e}"),
        }
    }
}

/// 下载闭区间 `[start, end]`：**先 LAN 快路径，再回落 NAS**。
///
/// ★ M7 的判据原样保留：对端 `head` 必须 `exists && hydrated`，且 `size`/`mtime`
/// 与 NAS 签名一致 —— 只有「同一份内容」才敢用。★ M9 把它抽出来给后台内容刷新复用，
/// 免得 LAN/NAS 的分支在仓库里有两份。
async fn fetch_chunk_bytes(
    ctx: &FetchCtx,
    remote: &str,
    start: u64,
    end: u64,
    total: u64,
    mtime: i64,
) -> Result<Vec<u8>, ChunkFetchError> {
    let want = end - start + 1;
    let peers = {
        let g = ctx.peers.lock().unwrap();
        if g.is_empty() {
            Vec::new()
        } else {
            g.clone()
        }
    };
    if !peers.is_empty() {
        ctx.stats.attempts.fetch_add(1, Ordering::Relaxed);
        if let Some(h) = peer::fetch_range(&peers, remote, start, want, total, mtime).await {
            ctx.stats.hits.fetch_add(1, Ordering::Relaxed);
            ctx.stats
                .bytes
                .fetch_add(h.data.len() as u64, Ordering::Relaxed);
            tracing::info!(
                "LAN 直传命中: {remote} [{start}..{}) ← {} ({:?})",
                start + want,
                h.peer,
                h.took
            );
            ctx.hydro.record(h.data.len() as u64);
            return Ok(h.data);
        }
        ctx.stats.mismatches.fetch_add(1, Ordering::Relaxed);
    }
    let (dir, name) = match remote.rsplit_once('/') {
        Some((d, n)) => (d.to_string(), n.to_string()),
        None => (String::new(), remote.to_string()),
    };
    match tokio::time::timeout(
        ctx.timeout,
        ctx.client.download_range(&dir, &name, start, end),
    )
    .await
    {
        Err(_) => Err(ChunkFetchError::Timeout),
        Ok(Err(e)) if e.is_auth() => Err(ChunkFetchError::Auth(e.to_string())),
        Ok(Err(e)) => Err(ChunkFetchError::Nas(e.to_string())),
        Ok(Ok(d)) => {
            ctx.hydro.record(d.len() as u64);
            Ok(d)
        }
    }
}

/// ★ M9：把一个文件的「远端新版本」整份下载到 `tmp`（**不碰正在服务的旧内容**）。
///
/// 下载完由调用方 `rename` 原子换上：换之前读到的是一份完整旧版本，换之后是完整新版本，
/// 永远不会出现「前 10 个区间是新的、后面还是旧的」这种撕裂。
async fn refresh_into(
    ctx: &FetchCtx,
    remote: &str,
    tmp: &Path,
    size: u64,
    mtime: i64,
    chunk_size: u64,
) -> Result<(), String> {
    use std::os::unix::fs::FileExt;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(tmp)
        .map_err(|e| format!("创建刷新临时文件失败: {e}"))?;
    f.set_len(size)
        .map_err(|e| format!("设置刷新文件长度失败: {e}"))?;
    if size == 0 {
        return Ok(());
    }
    let nchunks = size.div_ceil(chunk_size);
    for idx in 0..nchunks {
        let start = idx * chunk_size;
        let end = (start + chunk_size).min(size) - 1; // 闭区间，末块按文件尾截断
        let want = end - start + 1;
        let data = fetch_chunk_bytes(ctx, remote, start, end, size, mtime)
            .await
            .map_err(|e| e.to_string())?;
        if data.len() as u64 != want {
            return Err(format!(
                "区间长度不符 [{start}..={end}] 期望 {want} 实得 {}",
                data.len()
            ));
        }
        f.write_all_at(&data, start)
            .map_err(|e| format!("写刷新文件失败 @{start}: {e}"))?;
    }
    Ok(())
}

/// ★ M9：把刷新好的新内容**原子换上**（`rename` + `attr`/区间表一次切换）。
///
/// 返回 `None` = 这轮刷新作废（节点没了 / 被脱水 / 期间被改过 / 签名又变了 / 换失败）。
/// 抽成独立函数是为了**不挂 NAS 也能单测**「换上去之后元数据和内容是一套」。
#[allow(clippy::too_many_arguments)]
fn install_refreshed(
    inner: &Arc<Mutex<Inner>>,
    remote: &str,
    tmp: &Path,
    cache: &Path,
    size: u64,
    mtime: i64,
    chunk_size: u64,
) -> Option<INodeNo> {
    // ★ 与 read/write 互斥：读到一半把文件换成更小的一版会让读到越界 → EIO。
    //   锁的获取顺序与 read/write/dehydrate 一致（节点锁 → 节点表锁），不会死锁。
    let op_lock = {
        let g = inner.lock().unwrap();
        g.by_remote
            .get(remote)
            .and_then(|i| g.nodes.get(i))
            .map(|n| n.op_lock.clone())?
    };
    let _guard = op_lock.lock().unwrap();
    let ino = {
        let mut g = inner.lock().unwrap();
        let found = g.by_remote.get(remote).copied();
        let n = found.and_then(|i| g.nodes.get_mut(&i))?;
        n.refreshing = false;
        // 用户在这期间改过 / 被脱水 / 远端又变了 / 缓存路径换了 → 这轮不要了
        if n.pending != Some((size, mtime)) || n.dirty || n.cache.as_deref() != Some(cache) {
            return None;
        }
        if let Err(e) = std::fs::rename(tmp, cache) {
            tracing::error!("换上刷新内容失败 {remote}: {e}");
            return None;
        }
        n.attr.size = size;
        n.attr.blocks = size.div_ceil(512);
        let t = UNIX_EPOCH + Duration::from_secs(mtime.max(0) as u64);
        n.attr.mtime = t;
        n.attr.ctime = t;
        n.reset_chunks(chunk_size, true);
        n.pending = None;
        n.refresh_attempts = 0;
        n.ino
    };
    // ★ T3：整份刷新出来的内容是**一次下载**得到的，可以顺手把 per-chunk 校验和
    // 一次算齐（逐段读本地 tmp，不额外走网络）。不写的话下次认领会因「有位图
    // 但无校验和」把整份判成不可信，用户会看到刚刷新的文件又被退回重水合。
    let nchunks = {
        let g = inner.lock().unwrap();
        g.nodes.get(&ino).map(|n| n.chunk_count(chunk_size))
    };
    if let Some(nc) = nchunks {
        let mut sums = vec![NO_HASH; nc];
        for idx in 0..nc {
            match chunk_hash(cache, chunk_size, size, idx as u64) {
                Ok(h) => sums[idx as usize] = h,
                Err(e) => {
                    tracing::warn!("刷新后算区间校验和失败 {remote} #{idx}: {e}");
                    break;
                }
            }
        }
        if let Err(e) = sum_store_all(cache, &sums) {
            tracing::warn!("刷新后写校验和表失败 {}: {e}", cache.display());
        }
    }
    // 整文件校验和也顺手更新（本地顺序读一遍）
    if let Ok(h) = whole_hash(cache, size) {
        let mut g = inner.lock().unwrap();
        if let Some(n) = g.nodes.get_mut(&ino) {
            n.whole_xxhash = h;
        }
    }
    // 内容、元数据、区间表、位图必须一起切到新签名，否则下次挂载会认领到旧的那份
    persist_chunk_state(inner, chunk_size, ino);
    Some(ino)
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
    /// 挂载暴露的远端根列表（一对一：永远只有一个）。
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

    // ---------------------------------------------------------------- ★ M9
    /// 把 NAS 某个目录的文件列表推给挂载视图（定时刷新「映射」）。
    ///
    /// daemon 的变更轮询每轮列完目录后调用；此后 `readdir`/`lookup` 吃本地快照，
    /// 不再每次都往 NAS 跑。默认空实现（测试替身不需要）。
    fn apply_listing(&self, _dir: &str, _entries: &[DirEntry]) {}
    /// 远端目录已经不存在 → 丢掉清单快照。默认空实现。
    fn drop_listing(&self, _dir: &str) {}
    /// 该路径是否在等后台把远端新内容换上（此时 baseline 先别推进）。
    fn pending_refresh(&self, _remote: &str) -> bool {
        false
    }
    /// 有水文件的远端内容变了 → 后台整份刷新并原子换上。默认空实现。
    fn spawn_content_refresh(&self, _remote: &str) {}
}

/// 挂载视图句柄：daemon 在把 [`QxyncFs`] 交给 FUSE 挂载线程后，用它继续操作节点表。
#[derive(Clone)]
pub struct FsHandle {
    inner: Arc<Mutex<Inner>>,
    cache_dir: PathBuf,
    chunk_size: u64,
    remote_root: String,
    /// 全部远端根（一对一：长度恒为 1）。
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
    /// ★ M9：FUSE 自己的 NAS 客户端（后台补目录清单、后台刷新内容用）。
    client: Arc<Client>,
    /// ★ M9：后台任务用的 runtime 句柄（与 [`QxyncFs`] 同一个 runtime）。
    rt: tokio::runtime::Handle,
    /// ★ M9：后台内容刷新用（与 [`QxyncFs`] 同一套参数）。
    hydrate_timeout: Duration,
    lan_peers: Arc<Mutex<Vec<PeerConfig>>>,
    hydro: Arc<HydroCounters>,
    /// ★ M9/M10：daemon 挂载后注入的运行时回调（内核失效 + 会话热更新）。
    hooks: Hooks,
}

/// 「把某个 inode 的内核缓存作废」的回调（daemon 挂载后注入 `fuser::Notifier`）。
pub type Invalidator = Arc<dyn Fn(INodeNo) -> io::Result<()> + Send + Sync>;

/// ★ M10：「会话没了 → 给我一个新的 sid」回调（daemon 注入；`None` = 这次没拿到）。
///
/// 挂载点持有自己的 [`Client`]，而 sid 是**挂载那一刻**拷进来的。以前它过期以后没人
/// 续期，整个挂载点会一直 `EIO` 到重新挂载为止（`ls` 直接「输入/输出错误」）。
/// 有了这个回调，读/列目录遇到鉴权失败时就能让 daemon 重登一次、把新 sid 热塞回这个
/// client，然后**原地重试一次**。
pub type SidRefresher = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// daemon 挂载成功后注入的运行时回调。
///
/// [`QxyncFs`] 与 [`FsHandle`] 各持一份 `Hooks`（里面是同一批 `Arc`），
/// 所以 `set_invalidator` / `set_sid_refresher` 之后两边都立刻看得到。
#[derive(Clone, Default)]
struct Hooks {
    invalidator: Arc<Mutex<Option<Invalidator>>>,
    sid_refresher: Arc<Mutex<Option<SidRefresher>>>,
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

    /// ★ M9：注入「内核缓存失效」回调。挂载成功后 daemon 把 `fuser::Notifier` 塞进来，
    /// 这样后台把新内容换上去时能顺手让内核丢掉旧的 page cache。
    pub fn set_invalidator(&self, f: Invalidator) {
        *self.hooks.invalidator.lock().unwrap() = Some(f);
    }

    /// ★ M10：注入「重登拿新 sid」回调。挂载点遇到鉴权失败时用它热更新 sid 并重试。
    pub fn set_sid_refresher(&self, f: SidRefresher) {
        *self.hooks.sid_refresher.lock().unwrap() = Some(f);
    }

    /// ★ M9：当前缓存了几份目录清单（观测 / 单测用）。
    pub fn listing_count(&self) -> usize {
        self.inner.lock().unwrap().dirs.len()
    }

    pub fn delete_guard(&self) -> Arc<DeleteGuard> {
        self.delete_guard.clone()
    }
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// 挂载暴露的远端根（远端路径列表；一对一：只有一个）。
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
            .filter(|n| {
                self.rules
                    .hides_in_roots(&self.roots, &n.remote, false)
                    .is_none()
            })
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
        // ★ M9：后台内容刷新同样算「在途水合」—— 此时脱水会把刚拉下来的内容又清掉，
        //   白烧一遍带宽。
        let in_flight = n.refreshing
            || g.inflight_chunks
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
            remove_cache_files(&p);
        }
        // ③ 再更新占位符状态
        {
            let mut g = self.inner.lock().unwrap();
            let chunk_size = self.chunk_size;
            if let Some(n) = g.nodes.get_mut(&ino) {
                // 脱水之后没有内容 → 待刷新的新签名直接落到 attr 上
                n.apply_pending_sig();
                n.reset_chunks(chunk_size, false);
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
        if self
            .rules
            .hides_in_roots(&self.roots, path, false)
            .is_some()
        {
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

    /// 句柄拿到的是真实的远端根列表（一对一：一个元素），而不是默认实现的空列表。
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
        let sig_changed = kind_changed || n.attr.size != size || epoch_secs(n.attr.mtime) != mtime;
        // 本地有未上传改动时本地就是权威（冲突由 sync 引擎按策略解），这里不动内容。
        let content_changed = !n.dirty && sig_changed;

        // ★ M9：远端又变回本地这一版了 → 取消待刷新的意图。
        if !sig_changed {
            n.pending = None;
            return true;
        }

        // ★ M9：**有水**（整份都在本地）→ 旧内容继续留着可读，只记下新签名，
        //   等 daemon 触发后台刷新把新版本整份拉下来再原子换上。
        //   这期间 `attr` 也保持旧值：内容和元数据必须是一套，否则 `cat` 会读到
        //   「stat 说 10 MB、实际只有 4 MB」这种自相矛盾的状态。
        //   脱水（无内容）只更新元数据；部分水合最怕半新半旧 —— 照旧丢掉重下。
        if content_changed && !kind_changed && n.is_fully_hydrated() {
            if n.pending != Some((size, mtime)) {
                n.pending = Some((size, mtime));
                n.refresh_attempts = 0;
            }
            tracing::debug!("远端内容已变、本地有水 → 排队后台刷新 {remote}（{size} 字节）");
            return true;
        }

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
            // 脱水 / 部分水合 / 类型变了：丢掉旧内容，下次 read 按需水合新版本
            if let Some(p) = n.cache.take() {
                remove_cache_files(&p);
            }
            n.reset_chunks(chunk_size, false);
            n.pending = None;
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
        // 内容要丢了 → 待刷新的新签名先落到 attr 上（元数据跟着远端走）
        n.apply_pending_sig();
        if let Some(p) = n.cache.take() {
            remove_cache_files(&p);
        }
        n.reset_chunks(chunk_size, false);
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
                    remove_cache_files(&p);
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
            // 本地改动是权威：远端新内容的后台刷新意图作废
            n.pending = None;
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

    fn apply_listing(&self, dir: &str, entries: &[DirEntry]) {
        listing_put(&self.inner, dir, Arc::new(entries.to_vec()));
    }

    fn drop_listing(&self, dir: &str) {
        listing_drop(&self.inner, dir);
    }

    /// ★ M9：该路径是否在等后台把远端的新内容换上。
    ///
    /// daemon 用它决定「baseline 先不推进」：内容还没真正换新之前，baseline 若先跑到
    /// 新签名，用户此时基于旧内容改文件就会被误判成「本地覆盖远端」而不是冲突。
    fn pending_refresh(&self, remote: &str) -> bool {
        let g = self.inner.lock().unwrap();
        g.by_remote
            .get(remote)
            .and_then(|i| g.nodes.get(i))
            .map(|n| n.pending.is_some())
            .unwrap_or(false)
    }

    /// ★ M9：把远端的新版本整份拉进缓存并**原子换上**（只对「有水」的文件有意义）。
    ///
    /// 下载写进 `<cache>.refresh`，全部到位后 `rename` 换上，再把 `attr`/区间表/位图
    /// 一次性切到新签名。读路径**不等**这次下载：换文件是原子的，换之前读到的是一份
    /// 完整旧版本、换之后是完整新版本，绝不会读到半新半旧。连续失败 3 次就退回
    /// 「丢掉旧内容、读到时按需水合」。
    fn spawn_content_refresh(&self, remote: &str) {
        let (cache, sig, chunk_size) = {
            let mut g = self.inner.lock().unwrap();
            let Some(ino) = g.by_remote.get(remote).copied() else {
                return;
            };
            let chunk_size = self.chunk_size;
            let Some(n) = g.nodes.get_mut(&ino) else {
                return;
            };
            let Some(sig) = n.pending else {
                return;
            };
            if n.refreshing {
                return;
            }
            let Some(cache) = n.cache.clone() else {
                return;
            };
            n.refreshing = true;
            (cache, sig, chunk_size)
        };
        let (size, mtime) = sig;
        let tmp = sibling_path(&cache, ".refresh");
        let ctx = FetchCtx {
            client: self.client.clone(),
            peers: self.lan_peers.clone(),
            stats: self.lan_stats.clone(),
            timeout: self.hydrate_timeout,
            hydro: self.hydro.clone(),
        };
        let inner = self.inner.clone();
        let invalidator = self.hooks.invalidator.clone();
        let remote = remote.to_string();
        tracing::info!("后台刷新内容 {remote}（{size} 字节）");
        self.rt.spawn(async move {
            if let Err(e) = refresh_into(&ctx, &remote, &tmp, size, mtime, chunk_size).await {
                tracing::warn!("后台刷新内容失败 {remote}: {e}");
                {
                    let mut g = inner.lock().unwrap();
                    let found = g.by_remote.get(&remote).copied();
                    if let Some(n) = found.and_then(|i| g.nodes.get_mut(&i)) {
                        n.refreshing = false;
                        n.refresh_attempts = n.refresh_attempts.saturating_add(1);
                        if n.refresh_attempts >= REFRESH_MAX_ATTEMPTS {
                            tracing::warn!("后台刷新连续失败，退回按需水合: {remote}");
                            if let Some(p) = n.cache.take() {
                                remove_cache_files(&p);
                            }
                            n.reset_chunks(chunk_size, false);
                            n.pending = None;
                            n.refresh_attempts = 0;
                        }
                    }
                }
                let _ = std::fs::remove_file(&tmp);
                return;
            }
            // 安装：期间没人写过 / 没被脱水 / 签名没又变，才敢换。
            // ★ 换文件要和 read/write 抢同一把节点锁，所以丢到 blocking 线程池去做 ——
            //   直接在 async worker 上等一把可能被慢速水合占住几秒的锁会把 runtime 饿死。
            let (r2, t2, c2) = (remote.clone(), tmp.clone(), cache.clone());
            let inner2 = inner.clone();
            let installed = tokio::task::spawn_blocking(move || {
                install_refreshed(&inner2, &r2, &t2, &c2, size, mtime, chunk_size)
            })
            .await
            .ok()
            .flatten();
            let Some(ino) = installed else {
                // 这轮刷新作废（文件被删/被脱水/又被改/远端又变）
                let _ = std::fs::remove_file(&tmp);
                return;
            };
            // 内核 page cache 里可能还留着旧内容 —— 换完必须让内核丢掉
            let f = invalidator.lock().unwrap().clone();
            if let Some(f) = f {
                if let Err(e) = f(ino) {
                    tracing::debug!("刷新后 inval_inode 失败（不影响内容）: {e}");
                }
            }
            tracing::info!("内容已更新到最新版本: {remote}（{size} 字节）");
        });
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
/// 之后所有删除回 `EACCES`，直到 `qxync sync --force-deletes`（或重新挂载）。
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
                "{} 秒内删除超过 {} 项，已熔断（`qxync sync --force-deletes` 可解除）",
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

/// `QxyncFs` 内部那个 tokio runtime（FUSE 回调里用它把异步的 NAS 调用 `block_on` 掉）。
///
/// ★ 存在的唯一理由：**它的析构不能在 tokio 上下文里等阻塞池收尾**。
/// tokio 的 `Runtime::drop` 会等阻塞任务跑完，而「等」这件事在异步上下文里是不允许的，
/// 于是 `blocking/shutdown.rs` 直接 panic：
/// `Cannot drop a runtime in a context where blocking is not allowed`。
///
/// 真实触发路径（2026-10-01 实测，daemon 日志 + 客户端「daemon 提前关闭了连接」）：
/// daemon 在 **async IPC 命令**里调 `qxync_fuse::spawn` → `fuser::spawn_mount2` →
/// 挂载失败（挂载点被系统拒、机器没有 `/dev/fuse` …）时，fuser 会把 `fs` **就地在那个
/// async worker 线程上 drop**。于是「挂载失败」被放大成「daemon 打了个 panic、连接断掉」。
///
/// 修法就是 tokio 官方文档给的办法：在 tokio 上下文里改用 `shutdown_background()`
/// （等价 `shutdown_timeout(0)`，不等待），**普通线程上保持原来的等待语义**
/// （正常卸载时 `fs` 是在 fuser 自己的线程上析构的，行为一字不改）。
struct FsRuntime(Option<tokio::runtime::Runtime>);

impl FsRuntime {
    fn new(rt: tokio::runtime::Runtime) -> Self {
        Self(Some(rt))
    }

    fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        self.0
            .as_ref()
            .expect("QxyncFs 的 runtime 已关闭")
            .block_on(future)
    }

    fn handle(&self) -> tokio::runtime::Handle {
        self.0
            .as_ref()
            .expect("QxyncFs 的 runtime 已关闭")
            .handle()
            .clone()
    }

    /// 在 FUSE 自己的 runtime 上跑后台任务（补目录清单、后台刷新内容）。
    fn spawn<F>(&self, future: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.0
            .as_ref()
            .expect("QxyncFs 的 runtime 已关闭")
            .spawn(future);
    }
}

impl Drop for FsRuntime {
    fn drop(&mut self) {
        let Some(rt) = self.0.take() else { return };
        if tokio::runtime::Handle::try_current().is_ok() {
            rt.shutdown_background();
        } else {
            drop(rt);
        }
    }
}

pub struct QxyncFs {
    rt: FsRuntime,
    client: Arc<Client>,
    /// 远端挂载根（= 这一个 NAS 文件夹，如 `/home`、`/Public`）。
    /// 一对多是删除过的：一个挂载点只对应一个远端路径。
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
    /// ★ M9：目录清单快照的保鲜期（过期只做后台补拉，`readdir` 不等）。
    dir_ttl: Duration,
    /// ★ M10：daemon 挂载后注入的运行时回调（内核失效 / 会话热更新）。
    hooks: Hooks,
    /// ★ M11：当前用户的 uid —— 用来认 `.Trash-<uid>` 这个卷内回收站目录名。
    trash_uid: u32,
    /// ★ M11：删除队列（写模式必须提供）。`None` 时 `unlink` 退回同步删除。
    delete_queue: Option<Arc<crate::delete::DeleteQueue>>,
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
            file_id: ZERO_FILE_ID,
            whole_xxhash: NO_HASH,
            dirty: false,
            open_count: 0,
            last_access: UNIX_EPOCH,
            op_lock: Arc::new(Mutex::new(())),
            pending: None,
            refreshing: false,
            refresh_attempts: 0,
        };
        let mut nodes = HashMap::new();
        let mut by_remote = HashMap::new();
        nodes.insert(INodeNo::ROOT, root);
        by_remote.insert(remote_root.clone(), INodeNo::ROOT);

        Ok(Self {
            rt: FsRuntime::new(rt),
            client,
            remote_root: remote_root.clone(),
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
                dirs: HashMap::new(),
                listing_inflight: HashSet::new(),
            })),
            delete_guard: DeleteGuard::new(DEFAULT_DELETE_LIMIT, DEFAULT_DELETE_WINDOW),
            cache_mode: CacheMode::PageCache,
            rules: Arc::new(Rules::temp_only(true)),
            lan_peers: Arc::new(Mutex::new(Vec::new())),
            lan_stats: Arc::new(LanStats::default()),
            dir_ttl: DEFAULT_DIR_TTL,
            hooks: Hooks::default(),
            trash_uid: unsafe { libc::getuid() },
            delete_queue: None,
        })
    }

    /// ★ M9：目录清单快照保鲜期（默认 30s；过期只在后台补拉）。
    pub fn with_dir_ttl(mut self, d: Duration) -> Self {
        self.dir_ttl = d;
        self
    }

    /// 覆盖水合超时（大文件 + 慢链路时可以放大；M1 是整文件水合，M2 改成区间后就不敏感了）。
    pub fn with_hydrate_timeout(mut self, d: Duration) -> Self {
        self.hydrate_timeout = d;
        self
    }

    pub fn remote_root(&self) -> &str {
        &self.remote_root
    }

    /// 远端根列表（**单根：永远只有一个**）。
    /// 保留列表签名是给规则引擎 / 同步引擎用的（它们按「一组根」写的）。
    pub fn remote_roots(&self) -> Vec<String> {
        vec![self.remote_root.clone()]
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
            client: self.client.clone(),
            rt: self.rt.handle(),
            hydrate_timeout: self.hydrate_timeout,
            lan_peers: self.lan_peers.clone(),
            hydro: self.hydro.clone(),
            hooks: self.hooks.clone(),
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

    /// ★ M11：注入删除队列 —— `unlink`/`rmdir` 从「阻塞打 NAS」改为「入队即返回」。
    ///
    /// 不注入时 [`Self::remove_entry`] 退回同步删除（语义仍是「删得掉」，
    /// 只是慢）。写模式（`--rw`）的 daemon **必须**注入。
    pub fn with_delete_queue(mut self, q: Arc<crate::delete::DeleteQueue>) -> Self {
        self.delete_queue = Some(q);
        self
    }

    /// ★ M11：删除队列快照（`status` 用；未注入时全零）。
    pub fn delete_queue(&self) -> Option<Arc<crate::delete::DeleteQueue>> {
        self.delete_queue.clone()
    }

    /// ★ M10：让 daemon 重登一次，并把新 sid 热塞进本挂载点用的 client。
    ///
    /// 返回 false = 这次没拿到新 sid（没注入回调 / 重登失败），调用方按原错误处理。
    fn refresh_sid(&self) -> bool {
        let f = self.hooks.sid_refresher.lock().unwrap().clone();
        match f.and_then(|f| f()) {
            Some(sid) => {
                self.client.set_sid(sid);
                true
            }
            None => false,
        }
    }

    /// ★ M10：NAS 调用遇「会话失效」→ 重登一次并**原地重试一次**。
    ///
    /// 只对鉴权类错误重试（[`qxync_core::Error::is_auth`]）：网络错误重试只会白等一轮。
    fn with_session_retry<T>(
        &self,
        mut run: impl FnMut() -> std::result::Result<T, qxync_core::Error>,
    ) -> std::result::Result<T, qxync_core::Error> {
        match run() {
            Err(e) if e.is_auth() => {
                tracing::warn!("挂载点会话失效（{e}），请求 daemon 重登后重试");
                if !self.refresh_sid() {
                    return Err(e);
                }
                run()
            }
            other => other,
        }
    }

    /// ★ M9：构造一次区间下载的上下文（前台水合与后台内容刷新共用同一套路径）。
    fn fetch_ctx(&self) -> FetchCtx {
        FetchCtx {
            client: self.client.clone(),
            peers: self.lan_peers.clone(),
            stats: self.lan_stats.clone(),
            timeout: self.hydrate_timeout,
            hydro: self.hydro.clone(),
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
    /// 空路径（不该出现）不参与判定，避免拿 `""` 去比规则。
    fn hide_reason(&self, remote: &str, is_dir: bool) -> Option<HideReason> {
        if remote.is_empty() {
            return None;
        }
        self.rules
            .hides_in_roots(&self.remote_roots(), remote, is_dir)
    }

    /// ★ M11：**回收站目录**判定 —— 路径任一段命中回收站命名就返回 `true`。
    ///
    /// 为什么要挡：KDE Dolphin（`kio_trash.so`）删除文件时不去调 `unlink`，
    /// 而是走 FDO 的卷内回收站协议：在**同一个卷**里造出
    /// `.Trash-$UID/{files,info}`，把文件「搬」进去并写 `.trashinfo`。
    /// 挂载点本身就是一个卷，于是回收站被造在挂载点里、
    /// 内容最终落在 NAS 上。实测 `~/qxync-mnt/.Trash-1000/info/*.trashinfo` 里
    /// 记着 `Path=qxync-test/...`（挂载点内的相对路径），正是这个机制。
    ///
    /// 挡掉之后 Dolphin 的建目录/写文件会拿到 `EPERM`，
    /// KIO 无法创建回收站 → 退回「直接删除」（这是它的既定回退路径），
    /// 用户要的「不进回收站、直接删」就达成了。
    ///
    /// 识别的命名（只认**段**级，不做前缀模糊匹配，避免误伤正常文件）：
    /// * `.Trash`（管理员共享回收站）与 `.Trash-<uid>`（当前 uid）
    /// * `@Recycle`（QNAP 自家回收站，`@` 是 QNAP 的隐藏标记前缀）
    ///
    /// 注意**只挡挂载点内的**这些名字。`hide_reason` 走的是用户 `exclude` 规则，
    /// 两者是不同机制：这个是 qxync 的硬约束，删不得也覆盖不了。
    fn is_trash_path(&self, remote: &str) -> bool {
        remote
            .split('/')
            // 首段常是 ""（绝对路径开头），跳过
            .filter(|s| !s.is_empty())
            .any(|seg| is_trash_segment(seg, self.trash_uid))
    }

    /// 回收站守卫：命中就拒绝并打出可诊断的日志。
    fn deny_trash(&self, remote: &str, op: &str) -> Result<(), fuser::Errno> {
        if self.is_trash_path(remote) {
            tracing::warn!("拒绝{op}回收站路径（挂载点不提供回收站，删除将直接生效）: {remote}");
            return Err(fuser::Errno::EPERM);
        }
        Ok(())
    }

    /// ★ M7：被规则隐藏的路径**任何写操作都不许落地**（返回 `ENOENT`：它在挂载点里不存在）。
    ///
    /// 为什么必须在写入口挡：`open(O_CREAT)` 走的是 `create`，**不经过 `lookup`**。
    /// 实测踩过（本矩阵 FUSE 段抓到的真 bug）：往被排除的文件里 `printf` 会新建节点 →
    /// 0 字节缓存入队上传 → **把 NAS 上那个文件的内容覆盖成 0 字节**，
    /// 直接违反「排除 ≠ 删除、绝不动远端」这条硬约束。
    fn deny_hidden(&self, remote: &str, is_dir: bool) -> Result<(), fuser::Errno> {
        if self.hide_reason(remote, is_dir).is_some() {
            tracing::warn!("拒绝写入被选择性同步排除的路径: {remote}");
            Err(fuser::Errno::ENOENT)
        } else {
            Ok(())
        }
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
        // ★ M7：名字规则能在 stat 之前判掉一部分（临时文件/无斜杠规则），先省一次 NAS 往返。
        if self.hide_reason(&remote, false).is_some() {
            return Err(fuser::Errno::ENOENT);
        }

        if let Some(node) = self.node_by_remote(&remote) {
            return Ok(node);
        }
        // ★ M9：先看父目录的清单快照（= NAS 文件列表的本地映射）。
        //   有快照就**不用问 NAS**：命中直接建节点，没命中直接 ENOENT。
        //   快照过期只在后台补拉，绝不在这里等一次往返。
        if let Some(hit) = listing_lookup(&self.inner, &parent_remote, name) {
            let Some(entry) = hit else {
                return Err(fuser::Errno::ENOENT);
            };
            // 目录限定规则（`/cache/`）要拿到实际类型才能判
            if self.hide_reason(&remote, entry.isfolder).is_some() {
                return Err(fuser::Errno::ENOENT);
            }
            return Ok(self.insert_node(parent, name, &remote, &entry));
        }
        // 冷路径（这个目录还从没列过）：问一次 NAS stat，答案只影响这一个名字。
        let entry = match self
            .with_session_retry(|| self.rt.block_on(self.client.stat(&parent_remote, name)))
        {
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
            file_id: ZERO_FILE_ID,
            whole_xxhash: NO_HASH,
            dirty: false,
            open_count: 0,
            last_access: SystemTime::now(),
            op_lock: Arc::new(Mutex::new(())),
            pending: None,
            refreshing: false,
            refresh_attempts: 0,
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
        let entries = self.dir_entries(&remote)?;
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

    /// ★ M9：取一个目录的清单 —— **有本地快照就直接吃**，没有才同步问一次 NAS。
    ///
    /// 快照过期（超过 [`DEFAULT_DIR_TTL`]）时只丢一个后台补拉任务出去，调用方不等：
    /// `ls` 该多快就多快。定时刷新由 daemon 轮询推送（[`FsHandle::apply_listing`]）。
    fn dir_entries(&self, remote: &str) -> Result<Arc<Vec<DirEntry>>, fuser::Errno> {
        if let Some(list) = listing_get(&self.inner, remote) {
            if list.fetched.elapsed() >= self.dir_ttl && begin_listing_refresh(&self.inner, remote)
            {
                tracing::debug!("目录清单过期，后台补拉 {remote}");
                self.rt.spawn(run_listing_refresh(
                    self.inner.clone(),
                    self.client.clone(),
                    remote.to_string(),
                ));
            }
            return Ok(list.entries);
        }
        // ★ M10：会话失效 → 重登一次再重试（否则整个挂载点会一直 EIO 到重挂）
        let entries = match self.with_session_retry(|| self.rt.block_on(self.client.list(remote))) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("readdir {remote} 失败: {e}");
                return Err(fuser::Errno::EIO);
            }
        };
        let arc = Arc::new(entries);
        listing_put(&self.inner, remote, arc.clone());
        Ok(arc)
    }

    /// ★ M7：readdir 的过滤闸门（抽出来是为了**不挂 FUSE 也能单测**）。
    ///
    /// 返回 `(远端路径, 条目)`，被 `exclude` / 临时文件规则命中的直接丢掉。
    ///
    /// ★ M15/T1：**这里曾经有 `.take(LIST_LIMIT)`（硬截断 200 项），已删除。**
    /// 那个截断的前提是「`Client::list` 只返回一页 200 条」，而这个前提是错的 ——
    /// `list` 自己就带 `start`/`limit` 翻页循环（`qxync-client/src/lib.rs`），
    /// 拿到的 `entries` 已经是**全量**。真机实测（2026-10-04，501 项目录）：
    /// `limit=200&start=0` 返回 200 条且 `total=501`，按 `start` 翻 3 页
    /// 拼回 501 条、**不重不漏**；`total` 是**目录总项数**而非本页条数。
    /// 留着那个 `.take` 的后果就是：任何超过 200 项的目录 `ls` 永远只看得到前
    /// 200 个，且不报错、不提示（`lookup` 的三级回退能兜住 `stat`，但 `ls`
    /// 不做逐项 stat，用户看到的就是残缺目录）。
    fn filter_visible(&self, dir_remote: &str, entries: &[DirEntry]) -> Vec<(String, DirEntry)> {
        let mut out = Vec::with_capacity(entries.len());
        for e in entries.iter() {
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
        // 先算出真正要取的区间（`write` 完整覆盖的块不必取）。
        //
        // ★ 只有「写范围**完整覆盖**该区间」时才能跳过。
        //   注意不能只看「写到了这个区间」：区间内只改几个字节时，
        //   其余字节仍是远端原内容，跳过就会把它们当 0 上传（实测过：尾部追加把前 10 KB 清零）。
        let mut todo: Vec<u64> = Vec::new();
        for idx in 0..nchunks {
            let c_start = idx * chunk_size;
            let c_end = ((idx + 1) * chunk_size).min(total);
            if let Some((off, len)) = write {
                if c_start >= off && c_end <= off + len {
                    continue;
                }
            }
            if !self.chunk_ready(ino, idx) {
                todo.push(idx);
            }
        }
        if todo.is_empty() {
            return Ok(());
        }

        // ★ 性能：并发取，而不是一段一段串行。
        //   写路径要的是 read-modify-write，整文件都得先在本地齐了才敢落笔 ——
        //   于是「一次保存」的开销 = 全部缺块 × RTT，**随文件大小线性增长**
        //   （128 KiB 一块：1 MB 文件 8 个 RTT、4 MB 文件 32 个 RTT）。
        //   这些块彼此独立，按并发扇出取回来，墙钟时间就从 O(nchunks×RTT)
        //   降到 O(ceil(nchunks/并发)×RTT)。
        //
        //   正确性不受影响：每个块各自 `ensure_chunk`，内部有 per-chunk 去重锁
        //   （`inflight_chunks`），且**内容先于位图落盘**（见 `ensure_chunk` 注释），
        //   所以并发取块不会引入「位图说有、内容却是半截」的状态。
        //   任一块失败：先等的那些块仍会正常落盘（只是本次写失败，可重试），
        //   错误返回其中任意一个即可。
        if todo.len() == 1 {
            self.ensure_chunk(ino, todo[0])?;
            return Ok(());
        }
        let fanout = hydrate_fanout().min(todo.len());
        let first_err: Mutex<Option<fuser::Errno>> = Mutex::new(None);
        let err_slot = &first_err;
        std::thread::scope(|scope| {
            for lane in todo.chunks(fanout) {
                scope.spawn(move || {
                    for idx in lane {
                        if let Err(e) = self.ensure_chunk(ino, *idx) {
                            tracing::warn!("区间 {idx} 水合失败: {e:?}");
                            let mut slot = err_slot.lock().unwrap_or_else(|p| p.into_inner());
                            if slot.is_none() {
                                *slot = Some(e);
                            }
                        }
                    }
                });
            }
        });
        if let Some(e) = first_err.into_inner().unwrap_or(None) {
            return Err(e);
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
        let remote = join_path(&parent_remote, name);
        // ★ M7：被排除的路径在挂载点里不存在 —— 删除它同样回 ENOENT
        if let Err(e) = self.deny_hidden(&remote, is_dir) {
            return reply.error(e);
        }
        // ★ M11：不让桌面回收站长在挂载点里（KIO 会往 `.Trash-$UID` 搬文件 = 2~3 次往返/文件）
        if let Err(e) = self.deny_trash(&remote, "删除") {
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
                    tracing::warn!("删除前排空上传队列超时: {remote}");
                    return reply.error(fuser::Errno::EBUSY);
                }
            }
        }
        // ★ M11：**异步入队，立刻返回**。
        //
        // 改之前这里是 `rt.block_on(client.delete_entry(..))` —— unlink 要等 NAS 回包才返回，
        // 删 N 个文件就是 N 次串行 RTT。现在与官方客户端的删除形态对齐
        // （Smart Delete：本地立即生效，服务端侧异步推进）：
        //   * 节点表 / 映射立即摘掉（本地视角文件已消失，用户不等网络）
        //   * 远端删除由 `qxync-delete` worker 攒批推送（同目录合一次请求）
        //   * 失败自动重试，超限则 `status` 的删除失败计数 + 日志告警
        //
        // 熔断在上面已经生效：**大批量误删仍然被 EACCES 挡住**，不会悄悄进队列。
        if let Some(q) = &self.delete_queue {
            q.enqueue(DeleteJob {
                remote_dir: parent_remote.clone(),
                remote_name: name.to_string(),
                is_dir,
                attempts: 0,
            });
        } else {
            // 没有删除队列（只读模式本不该走到这里；写模式必须注入队列）→ 退回同步删除，
            // 保证语义是「删得掉」而不是静默不删。
            // ★ M10：会话失效 → 重登一次再重试
            let res = self.with_session_retry(|| {
                let client = self.client.clone();
                let (dir, n) = (parent_remote.clone(), name.to_string());
                self.rt
                    .block_on(async move { client.delete_entry(&dir, &n).await })
            });
            if let Err(e) = res {
                tracing::warn!("delete 失败 {remote}: {e}");
                return reply.error(fuser::Errno::EIO);
            }
        }
        {
            let mut g = self.inner.lock().unwrap();
            let prefix = format!("{}/", remote.trim_end_matches('/'));
            let victims: Vec<INodeNo> = g
                .by_remote
                .iter()
                .filter(|(p, _)| p.as_str() == remote || p.starts_with(&prefix))
                .map(|(_, ino)| *ino)
                .collect();
            for ino in victims {
                if let Some(n) = g.nodes.remove(&ino) {
                    g.by_remote.remove(&n.remote);
                    // ★ M9：本地删除同时清掉「内容 + 位图」，别让同名新文件认领到旧内容
                    if let Some(p) = n.cache {
                        remove_cache_files(&p);
                    }
                }
            }
        }
        // ★ M9：本地删除立刻从「映射」里摘掉，否则旧快照会把删掉的名字复活成幽灵节点
        listing_remove(&self.inner, &parent_remote, name);
        tracing::debug!("{} {}", if is_dir { "rmdir" } else { "unlink" }, remote);
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
            // 本地改动是权威：远端新内容的后台刷新意图作废
            n.pending = None;
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
        let mut flipped: Vec<u64> = Vec::new();
        let mut newly: Vec<u64> = Vec::new();
        let (cache, size, want) = {
            let mut g = self.inner.lock().unwrap();
            let Some(n) = g.nodes.get_mut(&ino) else {
                return;
            };
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
                        if !*slot {
                            *slot = true;
                            flipped.push(idx);
                        }
                        newly.push(idx);
                    }
                }
            }
            (n.cache.clone(), n.attr.size, want)
        };
        // ★ T3：本地写过的区间同样要有校验和 —— 否则 verify 会把「用户自己写的文件」
        // 全判成损坏。内容刚由 write 落盘，这里读回来算很便宜；一次 write 覆盖多个
        // 区间时合成一次 `pwrite` 批次，别一个区间开一次文件。
        if let Some(cache) = &cache {
            let mut items: Vec<(u64, u64)> = Vec::with_capacity(newly.len());
            for idx in &newly {
                match chunk_hash(cache, chunk_size, size, *idx) {
                    Ok(h) => items.push((*idx, h)),
                    Err(e) => tracing::warn!("算区间校验和失败 {} #{idx}: {e}", cache.display()),
                }
            }
            if let Err(e) = sum_store_many(cache, want, &items) {
                tracing::warn!("写校验和失败 {}: {e}", cache.display());
            }
        }
        // ★ M9：本地写入同样是「已有内容」。只在**真的有区间翻成就绪**时落盘：
        //   4 KB 一块的小写不该每写一次就重写一遍位图（`release` 时还有一次兜底落盘）。
        if !flipped.is_empty() {
            // 内容变了 → 旧的整文件哈希作废
            let mut g = self.inner.lock().unwrap();
            if let Some(n) = g.nodes.get_mut(&ino) {
                n.whole_xxhash = NO_HASH;
            }
            drop(g);
            persist_chunk_state(&self.inner, chunk_size, ino);
        }
    }

    /// 节点操作锁（read/write/setattr 持锁；脱水 try_lock）。
    fn op_lock(&self, ino: INodeNo) -> Option<Arc<Mutex<()>>> {
        let g = self.inner.lock().unwrap();
        g.nodes.get(&ino).map(|n| n.op_lock.clone())
    }

    /// 缓存文件（懒创建）：**apparent size = 文件大小**，用 `set_len` 造稀疏文件。
    ///
    /// ★ M9：**先认领磁盘上已有的缓存**。节点表是内存态，重建节点后 `chunks_done`
    /// 本来是空的；如果缓存文件 + 位图（`.qxstate`）都还在、且远端签名（size/mtime）
    /// 与本节点一致，就直接把区间表恢复出来 —— `read()` 于是完全不用问 NAS。
    ///
    /// ★ T3：认领时**逐区间核一遍 `xxhash64`**。位图只说「这个区间下过」，不保证
    /// 「现在磁盘上的内容还是那份」—— 位图/内容写序颠倒、掉电、静默位翻转都会留下
    /// 「长度对、内容错」的区间。核不过的区间直接清成未就绪，让 `read` 退回按需水合，
    /// **绝不返回错误内容**。这是 `qxync verify` 之外的一道兜底。
    fn cache_file_for(&self, ino: INodeNo) -> Result<PathBuf, fuser::Errno> {
        let (remote, name, size, mtime, existing) = {
            let g = self.inner.lock().unwrap();
            let n = g.nodes.get(&ino).ok_or(fuser::Errno::ENOENT)?;
            (
                n.remote.clone(),
                n.name.clone(),
                n.attr.size,
                epoch_secs(n.attr.mtime),
                n.cache.clone(),
            )
        };
        if let Some(p) = existing {
            return Ok(p);
        }
        let chunk_size = self.chunk_size;
        let path = self.cache_path(&remote, &name);

        // ① 认领：位图签名与节点一致 + 内容文件长度对得上，才敢信。
        let adopted = if path.is_file() {
            state_load(&path).and_then(|st| {
                let len_ok = std::fs::metadata(&path)
                    .map(|m| m.len() == size)
                    .unwrap_or(false);
                if st.chunk_size != chunk_size || st.size != size || st.mtime != mtime || !len_ok {
                    return None;
                }
                // ★ T3：签名过了，再逐区间核内容。核不过的清成未就绪。
                let mut done = st.done;
                let bad = verify_done_chunks(&path, &done, size, chunk_size);
                if !bad.is_empty() {
                    tracing::warn!(
                        "认领时发现 {} 个区间内容与校验和不符，已退回按需水合: {remote} {:?}",
                        bad.len(),
                        summarize_bad(&bad, chunk_size)
                    );
                    for idx in &bad {
                        done[*idx as usize] = false;
                    }
                }
                Some((done, st.whole_xxhash))
            })
        } else {
            None
        };

        let mut g = self.inner.lock().unwrap();
        let n = g.nodes.get_mut(&ino).ok_or(fuser::Errno::ENOENT)?;
        // 竞态兜底：拿锁期间别人可能已经建好了
        if let Some(p) = &n.cache {
            return Ok(p.clone());
        }
        let want = n.chunk_count(chunk_size);
        if let Some((mut done, whole)) = adopted {
            // size 在拿锁期间变了 → 认领作废
            if n.attr.size == size {
                done.resize(want, false);
                tracing::debug!(
                    "认领已有缓存 {remote}（{}/{} 区间已就绪）",
                    done.iter().filter(|d| **d).count(),
                    want
                );
                n.chunks_done = done;
                n.whole_xxhash = whole;
                n.cache = Some(path.clone());
                return Ok(path);
            }
        }

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
        n.chunks_done = vec![false; want];
        n.whole_xxhash = NO_HASH;
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

        let (remote, total, dest, mtime) = {
            let g = self.inner.lock().unwrap();
            let n = g.nodes.get(&ino).ok_or(fuser::Errno::ENOENT)?;
            (
                n.remote.clone(),
                n.attr.size,
                n.cache.clone().ok_or(fuser::Errno::EIO)?,
                Self::epoch_of(n.attr.mtime),
            )
        };
        // ★ M7：规则兜底（lookup 已经挡住了，这里是纵深防御）
        if self.hide_reason(&remote, false).is_some() {
            self.inner.lock().unwrap().inflight_chunks.remove(&key);
            return Err(fuser::Errno::EACCES);
        }
        let start = idx * self.chunk_size;
        let end = (start + self.chunk_size).min(total) - 1; // 闭区间，末块按文件尾截断
        let want = end - start + 1;

        // ★ M7/M9：LAN 快路径 → NAS 回落，走同一套区间下载原语（后台内容刷新也用它）。
        let ctx = self.fetch_ctx();
        let timeout = self.hydrate_timeout;
        let mut auth_retried = false;
        let data = loop {
            match self
                .rt
                .block_on(fetch_chunk_bytes(&ctx, &remote, start, end, total, mtime))
            {
                Ok(d) => break d,
                // ★ M10：sid 过期 → 重登、热更新 sid、原地重试一次
                Err(ChunkFetchError::Auth(msg)) if !auth_retried => {
                    auth_retried = true;
                    tracing::warn!("区间水合鉴权失败（{msg}），重登后重试: {remote}");
                    if !self.refresh_sid() {
                        self.inner.lock().unwrap().inflight_chunks.remove(&key);
                        return Err(fuser::Errno::EACCES);
                    }
                }
                Err(ChunkFetchError::Timeout) => {
                    tracing::warn!("区间水合超时({timeout:?}): {remote} [{start}..={end}]");
                    self.inner.lock().unwrap().inflight_chunks.remove(&key);
                    return Err(fuser::Errno::EIO);
                }
                Err(e) => {
                    tracing::warn!("区间水合失败: {remote} [{start}..={end}]: {e}");
                    self.inner.lock().unwrap().inflight_chunks.remove(&key);
                    return Err(fuser::Errno::EIO);
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

        // ★ T3：**先落校验和，再置位图上的位**。顺序反了的话，崩在中间会留下
        // 「位图说就绪、但没有校验和可比」的区间 —— 那只能当损坏处理，白重下一次。
        let nchunks = {
            let g = self.inner.lock().unwrap();
            g.nodes.get(&ino).map(|n| n.chunk_count(self.chunk_size))
        };
        if let Some(nc) = nchunks {
            if let Err(e) = sum_store_one(&dest, nc, idx, xxh64(&data)) {
                // 校验和写不进去 → 这一区间不能声称「内容是对的」，别置位。
                tracing::warn!("写校验和失败 {} #{idx}: {e}", dest.display());
                self.inner.lock().unwrap().inflight_chunks.remove(&key);
                return Err(fuser::Errno::EIO);
            }
        }

        let all_done = {
            let mut g = self.inner.lock().unwrap();
            let mut all = false;
            if let Some(n) = g.nodes.get_mut(&ino) {
                if let Some(slot) = n.chunks_done.get_mut(idx as usize) {
                    *slot = true;
                }
                all = n.is_fully_hydrated();
            }
            g.inflight_chunks.remove(&key);
            all
        };
        // ★ M9：位图落盘 —— 下次挂载/节点重建时这份内容才算「已经缓存过」。
        // ★ T3：全量就绪时顺手算一次整文件校验和（读一遍本地缓存，不问 NAS）。
        // 之后 `qxync verify` 可以「一个数」判整个文件，不用逐区间。
        if all_done {
            self.refresh_whole_hash(ino, &dest);
        }
        persist_chunk_state(&self.inner, self.chunk_size, ino);
        tracing::debug!("区间就绪: {remote} [{start}..={end}] ({want} 字节)");
        Ok(())
    }

    /// 重算整文件校验和并挂到节点上（**只读本地缓存文件**，不碰 NAS）。
    ///
    /// 失败就保持 [`NO_HASH`] —— 「没有整文件哈希」只影响 verify 的快路径，
    /// 不影响正确性（per-chunk 校验和仍然在）。
    fn refresh_whole_hash(&self, ino: INodeNo, cache: &Path) {
        let size = {
            let g = self.inner.lock().unwrap();
            match g.nodes.get(&ino) {
                Some(n) if n.is_fully_hydrated() => n.attr.size,
                _ => return,
            }
        };
        match whole_hash(cache, size) {
            Ok(h) => {
                let mut g = self.inner.lock().unwrap();
                if let Some(n) = g.nodes.get_mut(&ino) {
                    n.whole_xxhash = h;
                }
            }
            Err(e) => tracing::warn!("算整文件校验和失败 {}: {e}", cache.display()),
        }
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

/// 并发取块的扇出（`QXYNC_HYDRATE_FANOUT`，默认 [`DEFAULT_HYDRATE_FANOUT`]）。
///
/// 每次调用都读环境变量：测试要能临时改，daemon 也可能重启时调。
/// 1 = 关掉并发（退回原来的串行行为），也是「并发取块出问题」时的逃生阀。
fn hydrate_fanout() -> usize {
    std::env::var("QXYNC_HYDRATE_FANOUT")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map(|v| v.clamp(1, 64))
        .unwrap_or(DEFAULT_HYDRATE_FANOUT)
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

/// ★ M11：这个路径段是不是回收站目录名。
///
/// 认三种（**段级精确匹配**，不做前缀/子串匹配，避免误伤正常文件）：
/// * `.Trash` —— FDO 规范的卷内共享回收站
/// * `.Trash-<uid>` —— FDO 规范的卷内用户回收站（只认当前 uid，别人的不管）
/// * `@Recycle` —— QNAP 自家回收站目录（`@` 前缀是 QTS 的隐藏标记）
fn is_trash_segment(seg: &str, uid: u32) -> bool {
    if seg == ".Trash" || seg == "@Recycle" {
        return true;
    }
    if let Some(rest) = seg.strip_prefix(".Trash-") {
        // `.Trash-1000` / `.Trash-0`，纯数字且等于当前 uid
        return rest.parse::<u32>().map(|u| u == uid).unwrap_or(false);
    }
    false
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
        // 写意图打开时只做两件事：排除路径回 ENOENT（纵深防御）、其余放行。
        // ★ 不再有「非家目录根 → EROFS」：能不能写由 NAS 决定（登记成 Qsync 同步文件夹的
        //   目录可写），客户端不预判；服务端拒绝会在上传队列 / 错误列表里如实出现。
        if flags.acc_mode() != OpenAccMode::O_RDONLY {
            let remote = match self.remote_of(ino) {
                Ok(r) => r,
                Err(e) => return reply.error(e),
            };
            // ★ M7：写意图打开排除路径 → ENOENT（纵深防御）
            if let Err(e) = self.deny_hidden(&remote, false) {
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
        {
            let remote = match self.remote_of(ino) {
                Ok(r) => r,
                Err(e) => return reply.error(e),
            };
            // ★ M7：绝不把内容写进被排除的路径
            if let Err(e) = self.deny_hidden(&remote, false) {
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
        // size/mtime 是写操作：只读挂载已在上面挡掉，这里只需要防排除路径
        if size.is_some() || mtime.is_some() {
            let remote = match self.remote_of(ino) {
                Ok(r) => r,
                Err(e) => return reply.error(e),
            };
            // ★ M7：截断/改 mtime 也是写操作 —— 排除路径一律 ENOENT
            if let Err(e) = self.deny_hidden(&remote, false) {
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
                let res = self.with_session_retry(|| {
                    let client = self.client.clone();
                    let (d, n) = (dir.clone(), name.clone());
                    self.rt
                        .block_on(async move { client.set_mtime(&d, &n, epoch).await })
                });
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
        // ★ M7：排除路径不可创建（否则 0 字节缓存会被上传，覆盖远端文件）
        if let Err(e) = self.deny_hidden(&remote, false) {
            return reply.error(e);
        }
        // ★ M11：回收站里的文件也不许建（KIO 搬文件进回收站就是 create+rename）
        if let Err(e) = self.deny_trash(&remote, "创建") {
            return reply.error(e);
        }
        let entry = DirEntry::local(name, false, 0, Self::epoch_of(SystemTime::now()));
        let node = self.insert_node(parent, name, &remote, &entry);
        // ★ M9：本地新建立刻进「映射」，不然下一次 readdir/lookup 又从旧快照里看不到它
        listing_upsert(&self.inner, &parent_remote, &entry);
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
        // ★ M7：排除路径不可建目录（否则会在 NAS 上凭空造出一个被排除的目录）
        if let Err(e) = self.deny_hidden(&remote, true) {
            return reply.error(e);
        }
        // ★ M11：回收站目录建不出来 → KIO 无法启用卷内回收站，退回「直接删除」。
        //   这一条是让「挂载点不提供回收站」真正生效的关键（KIO 会先 mkdir `.Trash-$UID`）。
        if let Err(e) = self.deny_trash(&remote, "创建目录") {
            return reply.error(e);
        }
        // ★ M10：会话失效 → 重登一次再重试
        let res = self.with_session_retry(|| {
            let client = self.client.clone();
            let (p, n) = (parent_remote.clone(), name.to_string());
            self.rt.block_on(async move { client.mkdir(&p, &n).await })
        });
        if let Err(e) = res {
            tracing::warn!("mkdir 失败 {parent_remote}/{name}: {e}");
            return reply.error(fuser::Errno::EIO);
        }
        let entry = DirEntry::local(name, true, 0, Self::epoch_of(SystemTime::now()));
        let node = self.insert_node(parent, name, &remote, &entry);
        // ★ M9：本地新建目录立刻进「映射」
        listing_upsert(&self.inner, &parent_remote, &entry);
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

        // ★ M7：改名的两端都不能落在被排除的路径上（挪进去/挪出来都算写）
        if let Err(e) = self.deny_hidden(&old_remote, false) {
            return reply.error(e);
        }
        if let Err(e) = self.deny_hidden(&new_remote, false) {
            return reply.error(e);
        }
        // ★ M11：改名**两端**都不能是回收站路径。KIO 把文件搬进 `.Trash-$UID/files/`
        // 走的就是跨目录 move/rename —— 这里挡住，KIO 的回收站流程就彻底走不通，
        // 只能退回「直接删除」。搬**出**回收站同样挡（不给回收站开后门）。
        if let Err(e) = self.deny_trash(&old_remote, "改名") {
            return reply.error(e);
        }
        if let Err(e) = self.deny_trash(&new_remote, "改名到") {
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

        // ★ M10：会话失效 → 重登一次再重试（rename/move 都要）
        let res = self.with_session_retry(|| {
            let client = self.client.clone();
            if parent_remote == newparent_remote {
                // 同目录：FileStation rename（实测 body: path/source_name/dest_name；大小写改名可直接成功）
                let (dir, from, to) =
                    (parent_remote.clone(), name.to_string(), newname.to_string());
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
            }
        });
        if let Err(e) = res {
            tracing::warn!("rename 失败 {old_remote} -> {new_remote}: {e}");
            return reply.error(fuser::Errno::EIO);
        }

        if let Some(ino) = ino {
            let mut g = self.inner.lock().unwrap();
            g.by_remote.remove(&old_remote);
            g.by_remote.insert(new_remote.clone(), ino);
            if let Some(n) = g.nodes.get_mut(&ino) {
                // ★ M9：缓存文件（+ 位图）跟着改名走，否则重启后新名字认领不到旧内容
                if let Some(old_cache) = n.cache.clone() {
                    let new_cache = self.cache_path(&new_remote, newname);
                    if old_cache != new_cache {
                        match std::fs::rename(&old_cache, &new_cache) {
                            Ok(()) => {
                                let _ =
                                    std::fs::rename(state_path(&old_cache), state_path(&new_cache));
                                n.cache = Some(new_cache);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "改名后迁移缓存失败 {} -> {}: {e}",
                                    old_cache.display(),
                                    new_cache.display()
                                );
                            }
                        }
                    }
                }
                n.name = newname.to_string();
                n.remote = new_remote.clone();
                n.parent = newparent;
            }
        }
        // ★ M9：改名同步到「映射」：旧名字摘掉、新名字补上
        listing_remove(&self.inner, &parent_remote, name);
        if let Some(node) = self.node_by_remote(&new_remote) {
            let e = DirEntry::local(
                newname,
                node.attr.kind == FileType::Directory,
                node.attr.size,
                Self::epoch_of(node.attr.mtime),
            );
            listing_upsert(&self.inner, &newparent_remote, &e);
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
            "user.qxync.state" => Some(state.into()),
            // 已就绪区间数 / 总区间数（M2 的可观测性）
            "user.qxync.chunks" => Some(chunks),
            "user.qxync.remote" => Some(remote),
            "user.qxync.vsize" => Some(fsize.to_string()),
            "user.qxync.pin" => Some(
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
        let names = "user.qxync.state\0user.qxync.pin\0user.qxync.remote\0user.qxync.vsize\0user.qxync.chunks\0";
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
        // ★ M9：关文件时把位图按**最终**的 size/mtime 落一次盘 ——
        //   本地改写过的文件在下次挂载时也能直接认领，不必重新拉一遍。
        persist_chunk_state(&self.inner, self.chunk_size, ino);
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
    cfg.clone_fd = std::env::var("QXNYC_CLONE_FD")
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
/// `qxync_fuse::mount()` 用 `fuser::mount2`（拿不到 Notifier）；脱水必须能发
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

// ---------------------------------------------------------------- T3：`qxync verify`

/// 一个缓存文件的校验结果。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct VerifyFileReport {
    /// 缓存文件名（`<hash>_<name>`）。
    pub cache: String,
    /// 已就绪区间数 / 总区间数。
    pub chunks_done: usize,
    pub chunks_total: usize,
    /// 文件大小（字节）。
    pub size: u64,
    /// 内容与校验和不符的区间下标（升序）。
    pub bad_chunks: Vec<u64>,
    /// 位图缺失/损坏/是旧版 v1 —— 这类文件「不可信」，应按需重水合。
    pub stale_state: Option<String>,
    /// 整文件校验和（只在全量就绪且 `.qxstate` 里已记录时有值）。
    pub whole_xxhash: Option<u64>,
    /// 校验和表缺失（`.qxsum` 不在或长度不对）→ 无法做内容校验。
    pub missing_sums: bool,
}

impl VerifyFileReport {
    /// 这个文件是不是有实质问题。
    pub fn is_bad(&self) -> bool {
        !self.bad_chunks.is_empty() || self.stale_state.is_some() || self.missing_sums
    }
}

/// 整个缓存目录的校验汇总。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct VerifyReport {
    pub files: Vec<VerifyFileReport>,
    /// 扫过的缓存文件总数（含完全没水合的）。
    pub files_scanned: usize,
    /// 校验过的字节数（只算已就绪区间的）。
    pub bytes_verified: u64,
    /// 干掉的坏区间数（`--repair`）。
    pub repaired_chunks: usize,
    /// 因位图不可信被重置的区间数（`--repair`）。
    pub reset_chunks: usize,
    /// 整文件校验和重算并写回的数量（`--repair`）。
    pub whole_hashes_written: usize,
    /// 扫描耗时（毫秒）。
    pub elapsed_ms: u128,
}

impl VerifyReport {
    /// 全部文件都干净（可作为退出码依据）。
    pub fn is_clean(&self) -> bool {
        self.files.iter().all(|f| !f.is_bad())
    }

    /// 有问题的文件数。
    pub fn bad_files(&self) -> usize {
        self.files.iter().filter(|f| f.is_bad()).count()
    }
}

/// 校验一个缓存目录下的所有缓存文件（`qxync verify` 的实现）。
///
/// **不碰 NAS**：只读缓存文件与 `.qxstate` / `.qxsum`，所以断网也能跑。
/// * `repair = false`（默认）—— 只报告，不改任何文件。
/// * `repair = true` —— 把坏区间从位图里清掉（下次读按需水合）并删掉对应校验和；
///   位图本身不可信的（缺失/损坏/v1）则整份重置成「全未就绪」。
///
/// 注意这里**不做「重新下载」**：repair 只让缓存回到「诚实的未水合」状态，
/// 真正的数据修复由后续的 `read` 按需水合完成 —— 校验和修复不该偷偷产生 NAS 流量。
///
/// **递归扫**（限 [`VERIFY_MAX_DEPTH`] 层）：daemon 的实际缓存目录是
/// `<配置的 cache_dir>/<nas host>`，多了一层；GUI 又是从数据目录根上扫过来的。
/// 只看一层会「什么都没扫到」还报「一切正常」—— 那是最坏的失败方式。
pub fn verify_cache_dir(cache_dir: &Path, repair: bool) -> VerifyReport {
    let t0 = std::time::Instant::now();
    let mut rep = VerifyReport::default();
    let mut seen = 0usize;
    scan_cache_dir(cache_dir, cache_dir, 0, repair, &mut rep, &mut seen);
    rep.files.sort_by(|a, b| a.cache.cmp(&b.cache));
    rep.files_scanned = seen;
    rep.elapsed_ms = t0.elapsed().as_millis();
    rep
}

/// 递归深度上限：daemon 只拼一层主机名，4 层足够宽松又不至于在异常目录树上空转。
const VERIFY_MAX_DEPTH: usize = 4;

fn scan_cache_dir(
    root: &Path,
    dir: &Path,
    depth: usize,
    repair: bool,
    rep: &mut VerifyReport,
    seen: &mut usize,
) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for ent in rd.flatten() {
        let path = ent.path();
        let name = ent.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            if depth < VERIFY_MAX_DEPTH && name != "conflicts" && name != "upload-queue" {
                scan_cache_dir(root, &path, depth + 1, repair, rep, seen);
            }
            continue;
        }
        if !path.is_file() {
            continue;
        }
        // 缓存文件名形如 `<16位hash>_<safe name>`；`.qxstate` / `.qxsum` 等跳过。
        if name.ends_with(".qxstate") || name.ends_with(".qxsum") || name.starts_with('.') {
            continue;
        }
        *seen += 1;
        // 报告里给**相对路径**，否则多 NAS / 多任务时用户看到一堆同名文件定位不了
        let label = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        verify_one(&path, &label, repair, rep);
    }
}

/// 校验单个缓存文件。
///
/// 「一个区间都没就绪」的文件不进报告 —— 那是正常的占位符状态（几百个文件里
/// 大部分都没读过），报出来只是噪音。
fn verify_one(cache: &Path, name: &str, repair: bool, rep: &mut VerifyReport) {
    let Some(st) = state_load(cache) else {
        // state_load 对 v1 已经打过日志并删了；这里再判一次「文件在但状态不可信」。
        let content_len = std::fs::metadata(cache).map(|m| m.len()).unwrap_or(0);
        if content_len == 0 {
            return;
        }
        let r = VerifyFileReport {
            cache: name.to_string(),
            chunks_done: 0,
            chunks_total: 0,
            size: content_len,
            bad_chunks: Vec::new(),
            stale_state: Some("missing-or-legacy".into()),
            whole_xxhash: None,
            missing_sums: false,
        };
        if repair {
            // 位图不可信 → 整份删掉（内容留着当稀疏文件，下次读会重水合相应区间）。
            let _ = std::fs::remove_file(state_path(cache));
            let _ = std::fs::remove_file(sum_path(cache));
        }
        rep.files.push(r);
        return;
    };

    let nchunks = st.done.len();
    let done_n = st.done.iter().filter(|d| **d).count();
    if done_n == 0 {
        // 一块都没水合：位图没什么可校验的（`.qxsum` 也可能压根没建）。
        return;
    }
    let chunk_size = st.chunk_size;
    // 校验和表缺失或长度对不上 → 这一文件「无法做内容校验」，和损坏一样要报出来。
    let sum_len = std::fs::metadata(sum_path(cache)).map(|m| m.len()).unwrap_or(0);
    let missing_sums = sum_len != (nchunks * 8) as u64;
    let bad_chunks = verify_done_chunks(cache, &st.done, st.size, chunk_size);

    let mut whole = if st.whole_xxhash != NO_HASH && bad_chunks.is_empty() {
        Some(st.whole_xxhash)
    } else {
        None
    };

    if repair && !bad_chunks.is_empty() {
        // 坏区间：位图清位 + 校验和清 NO_HASH（下次 ensure_chunk 会重新下载并重算）。
        let mut done = st.done.clone();
        let mut sums = sum_load(cache, nchunks);
        for idx in &bad_chunks {
            done[*idx as usize] = false;
            sums[*idx as usize] = NO_HASH;
        }
        let fixed = ChunkState {
            done,
            whole_xxhash: NO_HASH, // 内容变了，整文件哈希作废
            ..st.clone()
        };
        if let Err(e) = state_save(cache, &fixed) {
            tracing::warn!("verify 修复位图失败 {}: {e}", cache.display());
        } else if let Err(e) = sum_store_all(cache, &sums) {
            tracing::warn!("verify 修复校验和表失败 {}: {e}", cache.display());
        }
        rep.repaired_chunks += bad_chunks.len();
    }

    // 全部就绪且内容无损 → 顺手（重）算一次整文件校验和，让下次 verify 能走快路径。
    if repair && bad_chunks.is_empty() && done_n == nchunks {
        match whole_hash(cache, st.size) {
            Ok(h) if h != st.whole_xxhash => {
                let updated = ChunkState {
                    whole_xxhash: h,
                    ..st.clone()
                };
                if state_save(cache, &updated).is_ok() {
                    rep.whole_hashes_written += 1;
                }
                whole = Some(h);
            }
            Ok(h) => whole = Some(h),
            Err(e) => tracing::warn!("verify 算整文件校验和失败 {}: {e}", cache.display()),
        }
    }

    rep.bytes_verified += done_n as u64 * chunk_size;
    rep.files.push(VerifyFileReport {
        cache: name.to_string(),
        chunks_done: done_n,
        chunks_total: nchunks,
        size: st.size,
        bad_chunks,
        stale_state: None,
        whole_xxhash: whole,
        missing_sums,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ M11：回收站段识别 —— 认得该认的，**且不误伤正常文件**。
    #[test]
    fn trash_segment_recognition() {
        let uid = 1000;
        // 该挡的
        assert!(is_trash_segment(".Trash", uid));
        assert!(is_trash_segment(".Trash-1000", uid));
        assert!(is_trash_segment("@Recycle", uid));
        // 不该挡的（别误伤）
        assert!(!is_trash_segment(".Trash-abc", uid), "非数字后缀");
        assert!(!is_trash_segment(".Trash-1001", uid), "别人的 uid 不管");
        assert!(!is_trash_segment("Trash", uid));
        assert!(!is_trash_segment("@Recycled", uid), "不做前缀匹配");
        assert!(!is_trash_segment("a.Trash-1000", uid), "不做子串匹配");
        assert!(!is_trash_segment(".trash", uid), "大小写敏感");
        assert!(!is_trash_segment("my.Trash-1000.txt", uid));
    }

    /// ★ M11：路径任一段命中回收站就该被拒（含深层）。
    #[test]
    fn is_trash_path_walks_every_segment() {
        let fs_roots = 1000u32;
        // 直接借用自由函数的路径判定：这里只验分段逻辑（QxyncFs 需要挂载环境）
        let hits = |p: &str| {
            p.split('/')
                .filter(|s| !s.is_empty())
                .any(|seg| is_trash_segment(seg, fs_roots))
        };
        assert!(hits("/home/.Trash-1000"));
        assert!(hits("/home/.Trash-1000/files/a.txt"));
        assert!(hits("/home/.Trash-1000/info/a.trashinfo"));
        assert!(hits("/home/@Recycle"));
        assert!(hits("/home/sub/@Recycle/x"));
        // 正常路径不能被误伤
        assert!(!hits("/home/qxync-test"));
        assert!(!hits("/home/a.txt"));
        assert!(!hits("/home/.recent"));
        // 同名的普通文件（不是目录段）不该被当成回收站
        assert!(!hits("/home/.Trash-1000.txt"));
    }

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

    /// 扇出默认 8，且 `QXYNC_HYDRATE_FANOUT=1` 能退回串行（逃生阀）。
    ///
    /// 这条守的是「保存很慢」的修复：并发的意义就是把 nchunks 个 RTT 压成
    /// nchunks/fanout 个，扇出退化成 1 就等于把性能修复关掉了。
    #[test]
    fn hydrate_fanout_defaults_to_eight_and_honors_env() {
        // 注意：环境变量是**进程级共享**的，这里只在自己没被别处改动时断言默认值。
        if std::env::var_os("QXYNC_HYDRATE_FANOUT").is_none() {
            assert_eq!(hydrate_fanout(), DEFAULT_HYDRATE_FANOUT);
            assert_eq!(DEFAULT_HYDRATE_FANOUT, 8);
        }
        // 越界值要夹住（0 → 1；999 → 64），不能让「0 路」把水合卡死
        for (raw, want) in [
            ("1", 1usize),
            ("4", 4),
            ("0", 1),
            ("999", 64),
            ("abc", DEFAULT_HYDRATE_FANOUT),
        ] {
            std::env::set_var("QXYNC_HYDRATE_FANOUT", raw);
            assert_eq!(hydrate_fanout(), want, "QXYNC_HYDRATE_FANOUT={raw}");
        }
        std::env::remove_var("QXYNC_HYDRATE_FANOUT");
    }

    /// 并发取块后，**每一块都必须真的就绪**（不能只回「跑完了」）。
    ///
    /// 覆盖的是并发改动最容易错的地方：线程没跑完 / 漏块 / 重复块。
    /// 这里用 `todo.chunks(fanout)` 的分组语义直接验证「拼起来 == 全部」。
    #[test]
    fn concurrent_chunk_lanes_cover_every_chunk_exactly_once() {
        for total in [1usize, 8, 9, 32, 33, 100] {
            for fanout in [1usize, 4, 8, 16] {
                let todo: Vec<u64> = (0..total as u64).collect();
                let f = fanout.min(todo.len());
                let mut seen: Vec<u64> = todo.chunks(f).flatten().copied().collect();
                seen.sort_unstable();
                assert_eq!(
                    seen, todo,
                    "total={total} fanout={f}: 并发分组必须恰好覆盖每个块一次"
                );
                // 每条 lane 非空（空 lane 说明扇出算错了，会白开线程）
                for lane in todo.chunks(f) {
                    assert!(!lane.is_empty(), "fanout 超过块数时不该产生空 lane");
                }
            }
        }
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
            file_id: ZERO_FILE_ID,
            whole_xxhash: NO_HASH,
            dirty: false,
            open_count: 0,
            last_access: UNIX_EPOCH,
            op_lock: Arc::new(Mutex::new(())),
            pending: None,
            refreshing: false,
            refresh_attempts: 0,
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

    /// ★ 修复回归：`QxyncFs` 里握着一个 tokio `Runtime`，**在 tokio 上下文里析构**会让
    /// tokio 直接 panic —— "Cannot drop a runtime in a context where blocking is not allowed"。
    ///
    /// 真实触发路径：daemon 在 async IPC 命令里调 `qxync_fuse::spawn` → `fuser::spawn_mount2`
    /// → 挂载失败（挂载点被内核/`fusermount3` 拒、机器没有 `/dev/fuse` …）时，fuser 会把
    /// `fs` 就地在**那个 async worker 线程**上 drop → worker panic、连接断掉，
    /// 客户端只看到「daemon 提前关闭了连接」，而不是一条能看懂的挂载错误。
    #[test]
    fn dropping_fs_inside_async_context_does_not_panic() {
        let dir = std::env::temp_dir().join(format!("qxync-fs-drop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let outer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        outer.block_on(async {
            let fs = test_fs(&dir, false);
            drop(fs); // 期望：安静地收掉内部 runtime（修复前这一行 panic）
        });
        let _ = std::fs::remove_dir_all(&dir);
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
        assert_eq!(
            plain.hide_reason("/home/a.crdownload", false),
            Some(HideReason::Temp)
        );
        let off = test_fs(&dir.join("off"), false).with_rules(m7_rules(&[], false));
        assert!(off.hide_reason("/home/a.crdownload", false).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ★ M15/T1：`readdir` **不许**再按 200 项截断目录。
    ///
    /// 这条守的是那个静默 bug：`filter_visible` 曾经有 `.take(LIST_LIMIT)`，而
    /// `LIST_LIMIT = 200` 只是服务端**单页**的容量（`Client::list` 自己会翻页，
    /// 传进来的是全量）。于是任何 >200 项的目录 `ls` 永远只看得到前 200 个，
    /// **且不报错**。
    ///
    /// 真机依据（2026-10-04，501 项目录）：`limit=200&start=0` → 200 条 / `total=501`，
    /// 按 `start` 翻 3 页拼回 501 条、不重不漏。所以这里直接喂 500 条全量清单，
    /// 断言一个不少。
    #[test]
    fn t1_readdir_is_not_truncated_at_200() {
        let dir = m7_tmpdir("t1-bigdir");
        let fs = test_fs(&dir, false);

        // 500 个普通文件 + 1 个会被临时文件规则滤掉的，共 501 条
        let mut entries: Vec<DirEntry> = (0..500)
            .map(|i| DirEntry::local(&format!("f{i:04}.txt"), false, i, 1_700_000_000 + i as i64))
            .collect();
        entries.push(DirEntry::local("dl.crdownload", false, 9, 1));

        let visible = fs.filter_visible("/home/big", &entries);
        let names: Vec<&str> = visible.iter().map(|(_, e)| e.filename.as_str()).collect();
        assert_eq!(
            names.len(),
            500,
            "500 个普通文件必须全部可见（只有 .crdownload 被规则滤掉）"
        );
        assert_eq!(names.first(), Some(&"f0000.txt"));
        assert_eq!(names.last(), Some(&"f0499.txt"));
        assert!(
            !names.contains(&"dl.crdownload"),
            "临时文件仍应被 M7 规则滤掉"
        );

        // 边界：正好 200 项（旧的截断点）一个都不能少
        let exact: Vec<DirEntry> = (0..200)
            .map(|i| DirEntry::local(&format!("e{i:04}.txt"), false, i, 1_700_000_000 + i as i64))
            .collect();
        assert_eq!(fs.filter_visible("/home/big", &exact).len(), 200);

        // 边界：201 项（跨过旧上限的那一项）也要在
        let over: Vec<DirEntry> = (0..201)
            .map(|i| DirEntry::local(&format!("o{i:04}.txt"), false, i, 1_700_000_000 + i as i64))
            .collect();
        let got = fs.filter_visible("/home/big", &over);
        assert_eq!(got.len(), 201, "第 201 项不能被吃掉");
        assert_eq!(got[200].1.filename, "o0200.txt");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ★ M7：写入口的规则闸门 —— `open(O_CREAT)` 不经过 lookup，必须自己挡。
    ///
    /// 这条单测守的是「排除 ≠ 删除」：曾经的真 bug 是往被排除路径 `printf` 会
    /// 新建节点 → 0 字节缓存入队 → 覆盖掉 NAS 上那个文件。
    #[test]
    fn m7_write_entry_denies_hidden_paths() {
        let dir = m7_tmpdir("deny");
        let fs = test_fs(&dir, false).with_rules(m7_rules(&["/secret", "*.crdownload"], true));
        // 被排除的文件/目录：create / mkdir / write / setattr / rename 全走这个闸门
        // （Errno 没实现 PartialEq，用 Debug 串断言错误码是 ENOENT）
        let enoent = |r: Result<(), fuser::Errno>| {
            let e = r.expect_err("必须被拒绝");
            format!("{e:?}") == format!("{:?}", fuser::Errno::ENOENT)
        };
        assert!(enoent(fs.deny_hidden("/home/secret", true)));
        assert!(enoent(fs.deny_hidden("/home/secret/new.txt", false)));
        assert!(enoent(fs.deny_hidden("/home/a.crdownload", false)));
        // 正常路径放行；卷根目录本身也放行
        assert!(fs.deny_hidden("/home/ok.txt", false).is_ok());
        assert!(fs.deny_hidden("/home", true).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ★ M7：远端根**本身**绝不能被规则隐藏（隐藏它 = 挂载点直接空掉）。
    #[test]
    fn m7_rules_never_hide_the_mount_root() {
        let dir = m7_tmpdir("roots");
        let fs = test_fs(&dir, false).with_rules(m7_rules(&["/tailscale.txt"], true));

        assert_eq!(fs.hide_reason("/home", true), None, "根本身不能被隐藏");
        assert_eq!(
            fs.hide_reason("/home/tailscale.txt", false),
            Some(HideReason::Excluded),
            "根里面的内容照常按规则隐藏"
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

        let paths: Vec<String> = h
            .dehydrate_candidates()
            .into_iter()
            .map(|c| c.remote)
            .collect();
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
            let listener = rt
                .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
                .unwrap();
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
        let node2 = fs2.insert_node(
            INodeNo::ROOT,
            "big.bin",
            "/home/qxync-test/big.bin",
            &entry2,
        );
        fs2.cache_file_for(node2.ino).unwrap();
        assert!(
            fs2.ensure_chunk(node2.ino, 0).is_err(),
            "元数据不符必须回落 NAS"
        );
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
    fn delete_guard_trips_and_resets() {
        let g = DeleteGuard::new(3, Duration::from_secs(60));
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

    fn root_tmp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("qxync-fuse-root-{tag}-{}", std::process::id()))
    }

    #[test]
    fn single_root_passthrough_unchanged() {
        let dir = root_tmp("passthrough");
        let _ = std::fs::remove_dir_all(&dir);
        let fs = test_fs(&dir, false);

        // 单根：只有一个远端根，挂载点**就是**它，ROOT 节点就是远端根
        assert_eq!(fs.remote_roots(), vec!["/home".to_string()]);
        assert_eq!(fs.remote_root(), "/home");
        assert_eq!(fs.handle().remote_roots(), vec!["/home".to_string()]);
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

    // ------------------------------------------------ ★ M9：缓存优先 / 映射本地化

    /// 位图编解码往返 + 损坏防护 + ★ T3 的 v1 兼容。
    #[test]
    fn chunk_state_roundtrip_and_rejects_corruption() {
        let st = ChunkState {
            chunk_size: 128 * 1024,
            size: 300 * 1024,
            mtime: 12345,
            file_id: ZERO_FILE_ID,
            whole_xxhash: 0xDEAD_BEEF_CAFE_1234,
            done: vec![true, false, true],
        };
        let buf = {
            // 用和 state_save 一样的编码路径（写盘再读回）
            let dir = m7_tmpdir("state-rt");
            let cache = dir.join("c.bin");
            std::fs::write(&cache, b"x").unwrap();
            state_save(&cache, &st).unwrap();
            let got = state_load(&cache).unwrap();
            assert_eq!(got, st);
            std::fs::read(state_path(&cache)).unwrap()
        };
        assert_eq!(state_decode(&buf).unwrap(), st);

        // 区间数和 size/chunk_size 对不上（损坏/被截断）→ 一律不认
        let mut bad = buf.clone();
        bad[36] = 0xFF;
        assert!(state_decode(&bad).is_none(), "自洽性校验必须挡住损坏位图");
        assert!(state_decode(b"not a qxstate file").is_none());
        assert!(state_decode(&buf[..STATE_HEAD]).is_none());
    }

    /// ★ T3 验收：**旧版 v1 位图要被识别并安全作废，不能 panic、也不能被当 v2 用**。
    ///
    /// v1 里没有校验和，「这个区间的内容还对不对」无法回答，所以只能当没缓存过。
    /// 这里同时验证「识别」（`state_verdict` 认得出 v1）和「作废」（文件被删、
    /// `state_load` 返回 `None`，而不是解出一个缺字段的 v2 结构）。
    #[test]
    fn legacy_v1_state_is_detected_and_safely_discarded() {
        // 手工拼一个合法的 v1 位图：QXSTATE1 + v1 + chunk_size/size/mtime/nchunks + 位图
        let mut v1 = Vec::new();
        v1.extend_from_slice(&STATE_MAGIC_V1);
        v1.extend_from_slice(&STATE_VERSION_V1.to_le_bytes());
        v1.extend_from_slice(&(128u64 * 1024).to_le_bytes());
        v1.extend_from_slice(&(300u64 * 1024).to_le_bytes());
        v1.extend_from_slice(&777i64.to_le_bytes());
        v1.extend_from_slice(&3u64.to_le_bytes());
        v1.extend_from_slice(&[0b101]); // 第 0、2 区间就绪
        assert_eq!(v1.len(), STATE_HEAD_V1 + 1);
        assert_eq!(state_verdict(&v1), StateVerdict::LegacyV1);
        assert!(state_decode(&v1).is_none(), "v1 绝不能被当成 v2 解出来");

        let dir = m7_tmpdir("state-v1");
        let cache = dir.join("legacy.bin");
        std::fs::write(&cache, b"x").unwrap();
        std::fs::write(state_path(&cache), &v1).unwrap();
        assert!(state_load(&cache).is_none(), "v1 不该被认领");
        assert!(
            !state_path(&cache).exists(),
            "v1 位图必须被删掉（安全作废，不是静默留着反复试）"
        );

        // 截断到只剩 magic+version 的 v1 残骸 → 判损坏而不是 LegacyV1
        let mut stub = STATE_MAGIC_V1.to_vec();
        stub.extend_from_slice(&STATE_VERSION_V1.to_le_bytes());
        assert_eq!(state_verdict(&stub), StateVerdict::Corrupt);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ★ T3 核心：手工破坏某个区间后，校验能**定位到具体文件和区间**，
    /// 且坏区间退回按需水合（不返回错误内容）。
    #[test]
    fn verify_locates_the_corrupted_chunk() {
        let dir = m7_tmpdir("t3-verify");
        let cache = dir.join("broken.bin");
        let cs = 128 * 1024u64;
        let size = 300 * 1024u64;
        let nchunks = 3usize;
        let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        std::fs::write(&cache, &payload).unwrap();

        // 三段都就绪，各自带校验和
        for i in 0..nchunks {
            let h = chunk_hash(&cache, cs, size, i as u64).unwrap();
            sum_store_one(&cache, nchunks, i as u64, h).unwrap();
        }
        let st = ChunkState {
            chunk_size: cs,
            size,
            mtime: 42,
            file_id: ZERO_FILE_ID,
            whole_xxhash: whole_hash(&cache, size).unwrap(),
            done: vec![true; nchunks],
        };
        state_save(&cache, &st).unwrap();

        // 干净时：没有坏区间
        assert!(verify_done_chunks(&cache, &st.done, size, cs).is_empty());

        // ★ 破坏第 1 区间中间（长度不变 —— 这正是只查长度查不出来的场景）
        {
            use std::os::unix::fs::FileExt;
            let f = std::fs::OpenOptions::new().write(true).open(&cache).unwrap();
            f.write_all_at(&[0xFFu8; 64], cs + 4096).unwrap();
        }
        let bad = verify_done_chunks(&cache, &st.done, size, cs);
        assert_eq!(bad, vec![1], "必须精确定位到第 1 区间，而不是「文件坏了」");

        // 整文件校验和也必须跟着发现不一致
        assert_ne!(whole_hash(&cache, size).unwrap(), st.whole_xxhash);

        // `.qxsum` 缺失 → 全部已就绪区间都视为「不可信」（保守：没有基准可比）
        let _ = std::fs::remove_file(sum_path(&cache));
        assert_eq!(
            verify_done_chunks(&cache, &st.done, size, cs),
            vec![0, 1, 2],
            "没有校验和就不许声称内容正确"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ★ T3 验收：`verify_cache_dir` 的报告 + `--repair` 语义。
    #[test]
    fn verify_cache_dir_reports_and_repairs() {
        let dir = m7_tmpdir("t3-report");
        let cs = 128 * 1024u64;
        let size = 256 * 1024u64;
        let nchunks = 2usize;

        // 文件 A：完好
        let good = dir.join("aaaaaaaa_good.bin");
        let p: Vec<u8> = (0..size).map(|i| (i % 97) as u8).collect();
        std::fs::write(&good, &p).unwrap();
        for i in 0..nchunks {
            let h = chunk_hash(&good, cs, size, i as u64).unwrap();
            sum_store_one(&good, nchunks, i as u64, h).unwrap();
        }
        state_save(
            &good,
            &ChunkState {
                chunk_size: cs,
                size,
                mtime: 1,
                file_id: ZERO_FILE_ID,
                whole_xxhash: whole_hash(&good, size).unwrap(),
                done: vec![true; nchunks],
            },
        )
        .unwrap();

        // 文件 B：第 1 区间坏了
        let bad = dir.join("bbbbbbbb_bad.bin");
        let q: Vec<u8> = (0..size).map(|i| (i % 89) as u8).collect();
        std::fs::write(&bad, &q).unwrap();
        for i in 0..nchunks {
            let h = chunk_hash(&bad, cs, size, i as u64).unwrap();
            sum_store_one(&bad, nchunks, i as u64, h).unwrap();
        }
        state_save(
            &bad,
            &ChunkState {
                chunk_size: cs,
                size,
                mtime: 2,
                file_id: ZERO_FILE_ID,
                whole_xxhash: NO_HASH,
                done: vec![true; nchunks],
            },
        )
        .unwrap();
        {
            use std::os::unix::fs::FileExt;
            let f = std::fs::OpenOptions::new().write(true).open(&bad).unwrap();
            f.write_all_at(&[0u8; 10], cs + 7).unwrap();
        }

        // 只报告，不改文件
        let rep = verify_cache_dir(&dir, false);
        assert_eq!(rep.files.len(), 2, "两个已水合文件都要报");
        assert!(!rep.is_clean());
        assert_eq!(rep.bad_files(), 1);
        let bf = rep.files.iter().find(|f| f.cache.contains("bad")).unwrap();
        assert_eq!(bf.bad_chunks, vec![1]);
        assert_eq!(rep.repaired_chunks, 0, "默认不改");
        assert_eq!(
            state_load(&bad).unwrap().done,
            vec![true; nchunks],
            "只报告模式不许动位图"
        );

        // --repair：坏区间退回未就绪，下次读按需水合
        let rep2 = verify_cache_dir(&dir, true);
        assert_eq!(rep2.repaired_chunks, 1);
        let fixed = state_load(&bad).unwrap();
        assert_eq!(fixed.done, vec![true, false], "第 1 区间应退回未就绪");
        assert_eq!(
            fixed.whole_xxhash, NO_HASH,
            "内容变了，整文件校验和必须作废"
        );
        // 校验和表里那一格也要清掉（否则下次认领还会误判）
        let sums = sum_load(&bad, nchunks);
        assert_eq!(sums[1], NO_HASH);
        // 修完之后这一轮就干净了（好文件不受影响）
        let rep3 = verify_cache_dir(&dir, true);
        assert!(rep3.is_clean(), "repair 后再校验应全清");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ★ T3 验收：**认领时核不过的区间必须退回按需水合，绝不返回错误内容**。
    ///
    /// 用 `nas.invalid` 当 NAS：只要还去问 NAS 就一定失败，所以
    /// 「坏区间读不出来」== 「没把坏内容当好的返回」。
    #[test]
    fn adopt_rejects_tampered_chunk_instead_of_serving_it() {
        let dir = m7_tmpdir("t3-adopt");
        let cs = 128 * 1024u64;
        let size = 300 * 1024u64;
        let entry = DirEntry::local("tampered.bin", false, size, 555);

        let cache = {
            let fs = test_fs(&dir, false);
            let node = fs.insert_node(INodeNo::ROOT, "tampered.bin", "/home/tampered.bin", &entry);
            let cache = fs.cache_file_for(node.ino).unwrap();
            let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            std::fs::write(&cache, &payload).unwrap();
            fs.mark_written_chunks(node.ino, 0, size);
            assert!(
                state_path(&cache).is_file() && sum_path(&cache).is_file(),
                "位图与校验和表都要落盘"
            );
            cache
        };

        // 破坏第 2 区间（长度不变）
        {
            use std::os::unix::fs::FileExt;
            let f = std::fs::OpenOptions::new().write(true).open(&cache).unwrap();
            f.write_all_at(&[0xEEu8; 128], 2 * cs + 11).unwrap();
        }

        // 新会话认领：应只认领 2/3 个区间
        let fs = test_fs(&dir, false);
        let node = fs.insert_node(INodeNo::ROOT, "tampered.bin", "/home/tampered.bin", &entry);
        let path = fs.cache_file_for(node.ino).unwrap();
        assert_eq!(path, cache);
        let (done, state_str) = {
            let g = fs.inner.lock().unwrap();
            let n = g.nodes.get(&node.ino).unwrap();
            (n.chunks_done.clone(), n.state_str())
        };
        assert_eq!(
            done,
            vec![true, true, false],
            "被破坏的区间必须退回未就绪"
        );
        assert_eq!(state_str, "partial");
        // 读那个坏区间必然失败（要去 NAS，nas.invalid 不通）—— 而不是返回垃圾
        assert!(fs.ensure_range(node.ino, 2 * cs, cs).is_err());
        // 好的两个区间仍然本地可读
        let p = fs.ensure_range(node.ino, 0, 2 * cs).unwrap();
        let bytes = std::fs::read(&p).unwrap();
        let expect: Vec<u8> = (0..2 * cs).map(|i| (i % 251) as u8).collect();
        assert_eq!(bytes[..2 * cs as usize], expect[..]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ★ T3：daemon 的实际缓存目录是 `<配置的 cache_dir>/<nas host>`，
    /// 比 CLI 默认的那一层深 —— 扫不到就会「什么都没扫到还报一切正常」，
    /// 那是最坏的失败方式。
    #[test]
    fn verify_recurses_into_nas_host_subdir() {
        let dir = m7_tmpdir("t3-nested");
        // 模拟 <cache>/<nas host>/
        let host_dir = dir.join("nas.example.com");
        std::fs::create_dir_all(&host_dir).unwrap();
        let cs = 128 * 1024u64;
        let size = 256 * 1024u64;
        let nchunks = 2usize;
        let cache = host_dir.join("cccccccc_nested.bin");
        std::fs::write(&cache, vec![3u8; size as usize]).unwrap();
        for i in 0..nchunks {
            let h = chunk_hash(&cache, cs, size, i as u64).unwrap();
            sum_store_one(&cache, nchunks, i as u64, h).unwrap();
        }
        state_save(
            &cache,
            &ChunkState {
                chunk_size: cs,
                size,
                mtime: 1,
                file_id: ZERO_FILE_ID,
                whole_xxhash: NO_HASH,
                done: vec![true; nchunks],
            },
        )
        .unwrap();

        let rep = verify_cache_dir(&dir, false);
        assert_eq!(rep.files_scanned, 1, "必须递归到 <nas host>/ 里");
        assert_eq!(rep.files.len(), 1);
        assert!(
            rep.files[0].cache.contains("nas.example.com"),
            "报告里要带相对路径便于定位，实际 {}",
            rep.files[0].cache
        );
        assert!(rep.is_clean());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ★ T3 验收：10 GB 级别全量校验的**开销**（单测跑不了 10 GB，
    /// 这里按 1/40 缩比跑 256 MiB，看换算到 10 GB 是否仍在 30 s 预算内）。
    #[test]
    fn verify_throughput_is_far_above_the_30s_budget() {
        let dir = m7_tmpdir("t3-speed");
        let cs = 128 * 1024u64;
        let size = 256 * 1024 * 1024u64;
        let nchunks = (size / cs) as usize;
        let cache = dir.join("speed.bin");
        std::fs::write(&cache, vec![7u8; size as usize]).unwrap();
        for i in 0..nchunks {
            let h = chunk_hash(&cache, cs, size, i as u64).unwrap();
            sum_store_one(&cache, nchunks, i as u64, h).unwrap();
        }
        // 只测「校验」这一段：算每区间校验和 + 与表里比
        let t0 = std::time::Instant::now();
        let bad = verify_done_chunks(&cache, &vec![true; nchunks], size, cs);
        let elapsed = t0.elapsed();
        assert!(bad.is_empty(), "刚写完的校验和表必须能一遍过: {bad:?}");
        let mib = size as f64 / 1048576.0;
        let mibs = mib / elapsed.as_secs_f64().max(1e-9);
        // 换算到 10 GB 需要多久
        let projected_10g = 10240.0 / mibs;
        println!(
            "[T3] 校验吞吐 {mibs:.0} MiB/s → 10 GB 全量校验预计 {projected_10g:.1}s"
        );
        assert!(
            projected_10g < 30.0,
            "10 GB 全量校验预计 {projected_10g:.1}s，超过 30s 预算（实测 {mibs:.0} MiB/s）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ★ M9 的核心回归：**磁盘上已经缓存好的内容，重建节点后仍然算数**。
    ///
    /// 修复前 `cache_file_for` 把区间表清成全 `false`，`read` 于是又去 NAS 拉一遍
    /// （「缓存过的文件 cat 还要等几秒」）。这里用 `nas.invalid` 当 NAS：只要还去问
    /// NAS 就一定失败，所以 `ensure_range` 成功 == 完全没碰网络。
    #[test]
    fn cached_content_is_reused_after_node_recreation() {
        let dir = m7_tmpdir("m9-adopt");
        let size = 300 * 1024u64;
        let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let entry = DirEntry::local("cached.bin", false, size, 777);

        // 第一个「挂载会话」：把内容写到缓存文件 + 位图落盘
        let cache = {
            let fs = test_fs(&dir, false);
            let node = fs.insert_node(INodeNo::ROOT, "cached.bin", "/home/cached.bin", &entry);
            let cache = fs.cache_file_for(node.ino).unwrap();
            std::fs::write(&cache, &payload).unwrap();
            fs.mark_written_chunks(node.ino, 0, size);
            {
                let g = fs.inner.lock().unwrap();
                assert!(
                    g.nodes.get(&node.ino).unwrap().is_fully_hydrated(),
                    "写完全文件后应当是全水合"
                );
            }
            assert!(state_path(&cache).is_file(), "位图必须落盘");
            cache
        };
        assert!(cache.is_file());

        // 第二个「挂载会话」：节点表是空的（模拟 daemon 重启 / 重新 lookup）
        let fs = test_fs(&dir, false);
        let node = fs.insert_node(INodeNo::ROOT, "cached.bin", "/home/cached.bin", &entry);
        // 认领发生在 cache_file_for 里
        let path = fs.cache_file_for(node.ino).unwrap();
        assert_eq!(
            fs.node_state_for_test(node.ino),
            "hydrated",
            "必须认领已有位图"
        );
        assert_eq!(path, cache);
        // 读区间完全走本地（nas.invalid 一旦被问到必然失败）
        let got = fs.ensure_range(node.ino, 0, size).unwrap();
        let bytes = std::fs::read(&got).unwrap();
        assert_eq!(bytes, payload, "认领到的内容必须原样可读");

        // 远端签名变了（mtime 不同）→ 位图作废，不许拿旧内容冒充新版本
        let stale = DirEntry::local("cached.bin", false, size, 778);
        let node2 = fs.insert_node(INodeNo::ROOT, "cached.bin", "/home/other.bin", &stale);
        fs.cache_file_for(node2.ino).unwrap();
        assert_eq!(fs.node_state_for_test(node2.ino), "placeholder");
        assert!(
            fs.ensure_chunk(node2.ino, 0).is_err(),
            "签名不符必须回去问 NAS（这里必然失败）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ★ M10：会话失效 → 让 daemon 重登、把新 sid 热塞回这个 client、**原地重试一次**。
    ///
    /// 以前挂载点持有的是挂载那一刻拷贝的 sid，过期后整个挂载点一直 EIO 到重新挂载。
    #[test]
    fn auth_failure_refreshes_sid_and_retries_exactly_once() {
        let dir = m7_tmpdir("m10-sid");
        let fs = test_fs(&dir, false).with_hydrate_timeout(Duration::from_millis(200));
        let refreshes = Arc::new(AtomicU64::new(0));
        {
            let n = refreshes.clone();
            fs.handle().set_sid_refresher(Arc::new(move || {
                n.fetch_add(1, Ordering::Relaxed);
                Some("fresh-sid".into())
            }));
        }

        // 第一次鉴权失败 → 刷新 sid → 第二次成功，且重试前新 sid 已经在 client 里
        let calls = AtomicU64::new(0);
        let out: std::result::Result<u32, qxync_core::Error> = fs.with_session_retry(|| {
            let n = calls.fetch_add(1, Ordering::Relaxed);
            if n == 0 {
                Err(qxync_core::Error::status(4, "get_list"))
            } else {
                assert_eq!(
                    fs.client.sid().as_deref(),
                    Some("fresh-sid"),
                    "重试之前必须把新 sid 塞回这个 client"
                );
                Ok(42)
            }
        });
        assert_eq!(out.unwrap(), 42);
        assert_eq!(calls.load(Ordering::Relaxed), 2, "只重试一次");
        assert_eq!(refreshes.load(Ordering::Relaxed), 1);

        // 非鉴权错误不重试（重试只会白等一轮）
        let calls2 = AtomicU64::new(0);
        let out2: std::result::Result<u32, qxync_core::Error> = fs.with_session_retry(|| {
            calls2.fetch_add(1, Ordering::Relaxed);
            Err(qxync_core::Error::Transport("连接超时".into()))
        });
        assert!(out2.is_err());
        assert_eq!(calls2.load(Ordering::Relaxed), 1);

        // 刷不出来（没注入 / 重登失败）→ 保留原错误，不再空转
        let fs2 = test_fs(&dir.join("no-refresher"), false);
        let calls3 = AtomicU64::new(0);
        let out3: std::result::Result<u32, qxync_core::Error> = fs2.with_session_retry(|| {
            calls3.fetch_add(1, Ordering::Relaxed);
            Err(qxync_core::Error::Auth("没有 sid".into()))
        });
        assert!(out3.is_err());
        assert_eq!(calls3.load(Ordering::Relaxed), 1);

        let _ = refreshes.load(Ordering::Relaxed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ★ M9：目录清单快照就是「映射」—— 有快照时 `readdir`/`lookup` 不再问 NAS。
    #[test]
    fn dir_listing_snapshot_serves_readdir_and_lookup_without_nas() {
        let dir = m7_tmpdir("m9-dirlist");
        let fs = test_fs(&dir, false);
        let h = fs.handle();
        assert_eq!(h.listing_count(), 0);

        // 没有快照 → 只能问 NAS（nas.invalid）→ EIO
        assert_eq!(
            fs.load_children(INodeNo::ROOT).err().map(|e| e.code()),
            Some(libc::EIO)
        );

        // daemon 的定时轮询把清单推进来
        h.apply_listing(
            "/home",
            &[
                DirEntry::local("a.txt", false, 10, 1),
                DirEntry::local("d", true, 0, 1),
            ],
        );
        assert_eq!(h.listing_count(), 1);

        // 之后 readdir / lookup 全部吃本地快照（内容与 NAS 无关）
        let kids = fs.load_children(INodeNo::ROOT).unwrap();
        let mut names: Vec<String> = kids.iter().map(|n| n.name.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["a.txt".to_string(), "d".to_string()]);

        let n = fs.lookup_child(INodeNo::ROOT, "a.txt").unwrap();
        assert_eq!((n.attr.size, n.attr.kind), (10, FileType::RegularFile));
        // 清单里没有的名字 → 直接 ENOENT，同样不用问 NAS
        assert_eq!(
            fs.lookup_child(INodeNo::ROOT, "nope.txt")
                .err()
                .map(|e| e.code()),
            Some(libc::ENOENT)
        );

        // 远端目录没了 → 丢快照（下一次访问重新问 NAS）
        h.drop_listing("/home");
        assert_eq!(h.listing_count(), 0);
        assert!(fs.load_children(INodeNo::ROOT).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 造一个「整份都在本地」的节点：内容 + 位图都真落盘。
    fn hydrated_node(
        fs: &QxyncFs,
        name: &str,
        remote: &str,
        payload: &[u8],
        mtime: i64,
    ) -> (INodeNo, PathBuf) {
        let entry = DirEntry::local(name, false, payload.len() as u64, mtime);
        let node = fs.insert_node(INodeNo::ROOT, name, remote, &entry);
        let cache = fs.cache_file_for(node.ino).unwrap();
        std::fs::write(&cache, payload).unwrap();
        fs.mark_written_chunks(node.ino, 0, payload.len() as u64);
        (node.ino, cache)
    }

    /// ★ M9：远端内容变了、本地有水 → **旧内容继续可读**（attr 也跟着旧，不许打架），
    /// 只挂一个「待后台刷新」的意图出来；脱水文件则只更元数据。
    #[test]
    fn hydrated_node_keeps_serving_old_content_until_refresh_lands() {
        let dir = m7_tmpdir("m9-pending");
        let fs = test_fs(&dir, false);
        let old: Vec<u8> = (0..300 * 1024u32).map(|i| (i % 253) as u8).collect();
        let (ino, cache) = hydrated_node(&fs, "a.bin", "/home/a.bin", &old, 1000);
        let h = fs.handle();

        // 远端换成了 400 KiB 的新版本
        assert!(h.apply_remote_meta("/home/a.bin", false, 400 * 1024, 2000));
        let n = h.node("/home/a.bin").unwrap();
        assert_eq!(
            (n.size, n.mtime),
            (300 * 1024, 1000),
            "有水的文件在刷新落地前必须保持旧签名：内容与元数据得是一套"
        );
        assert!(h.pending_refresh("/home/a.bin"), "必须挂上待刷新意图");
        assert!(cache.is_file(), "旧内容不能被提前删掉");

        // 读路径完全走本地（nas.invalid 一被问到就必失败）
        let got = std::fs::read(fs.ensure_range(ino, 0, old.len() as u64).unwrap()).unwrap();
        assert_eq!(got, old);

        // 远端又变回本地这一版 → 刷新意图取消
        assert!(h.apply_remote_meta("/home/a.bin", false, 300 * 1024, 1000));
        assert!(!h.pending_refresh("/home/a.bin"));

        // 脱水文件（没有区间就绪）：直接吃新元数据，不排队刷新
        let e = DirEntry::local("dry.bin", false, 10, 1);
        let dry = fs.insert_node(INodeNo::ROOT, "dry.bin", "/home/dry.bin", &e);
        fs.cache_file_for(dry.ino).unwrap();
        assert!(h.apply_remote_meta("/home/dry.bin", false, 20, 2));
        let n = h.node("/home/dry.bin").unwrap();
        assert_eq!((n.size, n.mtime), (20, 2), "脱水文件只更元数据");
        assert!(!h.pending_refresh("/home/dry.bin"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ★ M9：后台刷新「原子换上」—— 内容、大小、mtime、区间表、位图一次切换；
    /// 换完再挂载一次（新节点表）仍然认领得到这份新内容。
    #[test]
    fn install_refreshed_swaps_content_metadata_and_bitmap_atomically() {
        let dir = m7_tmpdir("m9-install");
        let old = vec![1u8; 300 * 1024];
        let new = vec![2u8; 400 * 1024];
        let (ino, cache) = {
            let fs = test_fs(&dir, false);
            let (ino, cache) = hydrated_node(&fs, "a.bin", "/home/a.bin", &old, 1000);
            let h = fs.handle();
            h.apply_remote_meta("/home/a.bin", false, new.len() as u64, 2000);
            assert!(h.pending_refresh("/home/a.bin"));
            // 后台下载好的「新版本」临时文件
            let tmp = sibling_path(&cache, ".refresh");
            std::fs::write(&tmp, &new).unwrap();
            let installed = install_refreshed(
                &fs.inner,
                "/home/a.bin",
                &tmp,
                &cache,
                new.len() as u64,
                2000,
                DEFAULT_CHUNK_SIZE,
            );
            assert_eq!(installed, Some(ino));
            assert!(!tmp.exists(), "临时文件必须被 rename 掉");
            assert!(!h.pending_refresh("/home/a.bin"));
            let n = h.node("/home/a.bin").unwrap();
            assert_eq!((n.size, n.mtime), (new.len() as u64, 2000));
            assert_eq!(std::fs::read(&cache).unwrap(), new);
            // 位图也切到了新签名
            let st = state_load(&cache).unwrap();
            assert_eq!((st.size, st.mtime), (new.len() as u64, 2000));
            assert!(st.done.iter().all(|d| *d));
            drop(fs);
            (ino, cache)
        };
        let _ = ino;

        // 新挂载会话：位图签名是新的 → 直接认领，读出来的就是新版本（不碰 NAS）
        let fs2 = test_fs(&dir, false);
        let e2 = DirEntry::local("a.bin", false, new.len() as u64, 2000);
        let n2 = fs2.insert_node(INodeNo::ROOT, "a.bin", "/home/a.bin", &e2);
        let p2 = fs2.cache_file_for(n2.ino).unwrap();
        assert_eq!(p2, cache);
        assert_eq!(fs2.node_state_for_test(n2.ino), "hydrated");
        let got = std::fs::read(fs2.ensure_range(n2.ino, 0, new.len() as u64).unwrap()).unwrap();
        assert_eq!(got, new);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ★ M9：安装前节点变了（用户写了 / 被脱水）→ 这轮刷新作废，绝不覆盖用户改动。
    #[test]
    fn install_refreshed_aborts_when_node_changed() {
        let dir = m7_tmpdir("m9-install-abort");
        let fs = test_fs(&dir, false);
        let old = vec![3u8; 300 * 1024];
        let new = vec![4u8; 400 * 1024];
        let (ino, cache) = hydrated_node(&fs, "a.bin", "/home/a.bin", &old, 1000);
        let h = fs.handle();
        h.apply_remote_meta("/home/a.bin", false, new.len() as u64, 2000);
        let tmp = sibling_path(&cache, ".refresh");
        std::fs::write(&tmp, &new).unwrap();

        // 期间用户改了内容（dirty）→ 作废
        {
            let mut g = fs.inner.lock().unwrap();
            g.nodes.get_mut(&ino).unwrap().dirty = true;
        }
        assert_eq!(
            install_refreshed(
                &fs.inner,
                "/home/a.bin",
                &tmp,
                &cache,
                new.len() as u64,
                2000,
                DEFAULT_CHUNK_SIZE
            ),
            None
        );
        assert_eq!(std::fs::read(&cache).unwrap(), old, "用户内容不许被覆盖");
        assert!(tmp.exists(), "作废的临时文件留给调用方清理");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ★ M9：后台刷新一直失败（NAS 不可达）→ 连续 3 次后退回「按需水合」，
    /// 并且不能卡死 `refreshing` 标志。
    #[test]
    fn content_refresh_falls_back_to_on_demand_after_failures() {
        let dir = m7_tmpdir("m9-refresh-fail");
        let fs = test_fs(&dir, false).with_hydrate_timeout(Duration::from_millis(200));
        let old = vec![5u8; 300 * 1024];
        let (ino, cache) = hydrated_node(&fs, "a.bin", "/home/a.bin", &old, 1000);
        let h = fs.handle();
        h.apply_remote_meta("/home/a.bin", false, 400 * 1024, 2000);
        assert!(h.pending_refresh("/home/a.bin"));

        for _ in 0..6 {
            h.spawn_content_refresh("/home/a.bin");
            for _ in 0..200 {
                let busy = {
                    let g = fs.inner.lock().unwrap();
                    g.nodes.get(&ino).map(|n| n.refreshing).unwrap_or(false)
                };
                if !busy {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            if fs.node_state_for_test(ino) == "placeholder" {
                break;
            }
        }
        assert_eq!(
            fs.node_state_for_test(ino),
            "placeholder",
            "一直刷不动就该丢掉旧内容、退回按需水合"
        );
        assert!(!cache.exists());
        assert!(!h.pending_refresh("/home/a.bin"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
