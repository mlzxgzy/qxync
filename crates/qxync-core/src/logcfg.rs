//! ★ M39：日志轮转与保留策略（daemon / GUI 共用）。
//!
//! ## 为什么要这一层
//!
//! 2026-10-23 的 `~/.local/state/qxync/log/qxyncd.log.2026-10-23` **单个文件 5.2 GB**
//! （日志目录合计 4.9 GB）。当时用的只有 `tracing_appender::rolling::daily`：
//!
//! * **只在跨天切段，同一天内无上限** —— 一次刷屏（每轮轮询都打日志）就能把磁盘吃满；
//! * **从不删除**旧文件 —— 只增不减。
//!
//! 本模块补两件事，且都归 `settings.json` 管（改设置不用重启，见 [`set_policy`]）：
//!
//! 1. **按大小切段**：[`RotatingWriter`] 写满 `log_max_file_mb` 就把当前段改名成
//!    `<前缀>.log.<YYYY-MM-DD>.<n>` 再开新段。文件名与旧实现（`daily`）一致 ——
//!    `qxyncd.log.2026-10-27` 这种跨天命名保持原样，编号段只是同一天内的细分；
//! 2. **按天 / 按总占用删除**：[`prune`] 删最旧的文件，直到满足 `log_keep_days`
//!    与 `log_max_total_mb`。**正在被写的文件绝不删** —— 写入器打开文件时持有
//!    `flock(LOCK_EX|LOCK_NB)`，清理前先探测那把锁（加得上 = 没人在写）。这比
//!    「猜哪个文件是活的」（按 mtime / 文件名排序）可靠：daemon 与 GUI 是两个
//!    进程、各写各的文件，还可能有多个连接各自的 `qxyncd-<id>.log`。
//!
//! ## 边界（如实写）
//!
//! * 日期按 **UTC** 取（与旧 `tracing_appender::rolling::daily` 一致 —— 上游用的是
//!   `OffsetDateTime::now_utc()`，真机可证：`.2026-10-26` 的最后一行是 `23:59:57Z`、
//!   `.2026-10-27` 的第一行是 `00:00:00Z`，即**本地 08:00 换文件**）。所以切段发生在
//!   UTC 零点 = 本地 08:00；时区跳变只影响命名，不影响按 mtime 的删除；
//! * 删除按文件 **mtime** 判新旧，不解析文件名里的日期（旧文件、外部拷贝来的文件也能正确处理）；
//! * 只处理**本目录里以 `qxync` 开头且名字含 `.log` 的普通文件** —— 别的文件一概不碰；
//! * `prune` 是「尽力而为」：删不掉（权限/占用）只跳过，不报错中断。

use std::fs::{File, OpenOptions};
use std::io;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use crate::settings::Settings;

/// 单个日志文件上限（MB）的默认值。
///
/// 32 MB ≈ 正常使用下一整天的量（真机平时每天几百 KB ~ 10 MB）；真出了刷屏 bug，
/// 一天也只切出有限几段，配合总占用上限很快就把旧的删掉。
pub const DEFAULT_MAX_FILE_MB: u32 = 32;
/// 保留天数的默认值（两周，够复盘一次「上周开始不对」的问题）。
pub const DEFAULT_KEEP_DAYS: u32 = 14;
/// 日志目录总占用上限（MB）的默认值。
pub const DEFAULT_MAX_TOTAL_MB: u32 = 256;

/// 字段上限（归一化用；超过就当上限，不报错）。
pub const MAX_FILE_MB_CAP: u32 = 4096;
pub const KEEP_DAYS_CAP: u32 = 3650;
pub const MAX_TOTAL_MB_CAP: u32 = 102_400;

const MIB: u64 = 1024 * 1024;

/// 日志轮转策略。`0` 的语义：`max_file_bytes=0` 不按大小切段、`keep_days=0`
/// 不按天数删、`max_total_bytes=0` 不限总占用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogPolicy {
    pub max_file_bytes: u64,
    pub keep_days: u32,
    pub max_total_bytes: u64,
}

impl Default for LogPolicy {
    fn default() -> Self {
        Self {
            max_file_bytes: DEFAULT_MAX_FILE_MB as u64 * MIB,
            keep_days: DEFAULT_KEEP_DAYS,
            max_total_bytes: DEFAULT_MAX_TOTAL_MB as u64 * MIB,
        }
    }
}

impl LogPolicy {
    /// 从 `settings.json` 的 `log_max_file_mb` / `log_keep_days` / `log_max_total_mb` 构造。
    ///
    /// 字段本身已经在 [`Settings::normalize`] 里夹过范围，这里只做 MB→字节换算。
    pub fn from_settings(s: &Settings) -> Self {
        Self {
            max_file_bytes: s.log_max_file_mb as u64 * MIB,
            keep_days: s.log_keep_days,
            max_total_bytes: s.log_max_total_mb as u64 * MIB,
        }
    }
}

// ---------------------------------------------------------------- 进程内共享策略

struct PolicyCell {
    max_file_bytes: AtomicU64,
    keep_days: AtomicU64,
    max_total_bytes: AtomicU64,
}

fn cell() -> &'static PolicyCell {
    static CELL: OnceLock<PolicyCell> = OnceLock::new();
    CELL.get_or_init(|| {
        let d = LogPolicy::default();
        PolicyCell {
            max_file_bytes: AtomicU64::new(d.max_file_bytes),
            keep_days: AtomicU64::new(d.keep_days as u64),
            max_total_bytes: AtomicU64::new(d.max_total_bytes),
        }
    })
}

/// 当前生效的策略（写路径每次现读 → 改设置立即生效，不用重启）。
pub fn policy() -> LogPolicy {
    let c = cell();
    LogPolicy {
        max_file_bytes: c.max_file_bytes.load(Ordering::Relaxed),
        keep_days: c.keep_days.load(Ordering::Relaxed) as u32,
        max_total_bytes: c.max_total_bytes.load(Ordering::Relaxed),
    }
}

/// 热更新策略（daemon 的 `settings_save` / GUI 保存设置时调）。
pub fn set_policy(p: LogPolicy) {
    let c = cell();
    c.max_file_bytes.store(p.max_file_bytes, Ordering::Relaxed);
    c.keep_days.store(p.keep_days as u64, Ordering::Relaxed);
    c.max_total_bytes
        .store(p.max_total_bytes, Ordering::Relaxed);
}

/// `settings.json` → 策略并立即生效。返回生效后的策略（便于日志里写清楚）。
pub fn apply_settings(s: &Settings) -> LogPolicy {
    let p = LogPolicy::from_settings(s);
    set_policy(p);
    p
}

// ---------------------------------------------------------------- 时间 / 文件名

/// 今天的 **UTC** 日期标签 `YYYY-MM-DD`（与旧 `tracing_appender::rolling::daily`
/// 的命名规则一致；理由见模块文档）。
pub fn today_tag() -> String {
    date_tag(SystemTime::now())
}

/// 某个时刻的 UTC 日期标签。
pub fn date_tag(t: SystemTime) -> String {
    let secs = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// 1970-01-01 起的天数 → `(年, 月, 日)`（Howard Hinnant 的 civil_from_days）。
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    ((y + i64::from(m <= 2)) as i32, m, d)
}

/// 日志目录里的文件名是不是「qxync 的日志文件」—— 只有这些会被清理。
///
/// 判定刻意宽松到能覆盖未来的前缀（`qxyncd.log.2026-10-27`、
/// `qxyncd-<id>.log.2026-10-27`、`qxync-gui.log.2026-10-27`），又窄到
/// 不会碰同目录里别的文件（必须以 `qxync` 开头且含 `.log`）。
pub fn is_qxync_log_name(name: &str) -> bool {
    name.starts_with("qxync") && name.contains(".log")
}

/// 文件名 → 前缀族（`.log` 之前的部分）。扫描/清理时只认这些名字（前缀族也是
/// `qxync` 日志文件的最小识别单位：`qxyncd` / `qxyncd-<id>` / `qxync-gui`）。
fn family_of(name: &str) -> Option<&str> {
    if !is_qxync_log_name(name) {
        return None;
    }
    name.split_once(".log").map(|(fam, _)| fam)
}

/// 尝试给文件加排它锁（写入器用）。失败**不影响写日志**：最坏情况是
/// 「清理时按 mtime 删掉一个还开着的文件」，那只会丢诊断信息，不会丢数据。
#[cfg(unix)]
fn lock_exclusive(file: &File) {
    use std::os::unix::io::AsRawFd;
    unsafe {
        libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB);
    }
}

#[cfg(not(unix))]
fn lock_exclusive(_file: &File) {}

/// 探测「这个文件是不是正被某个进程（含本进程的写入器）持有」。
///
/// `flock` 的语义正好是我们要的：写入器拿 `LOCK_EX|LOCK_NB` 一直持有；这里再拿
/// 一次 —— 拿得到 = 没人写（立刻解锁），`EWOULDBLOCK` = 有人在写。同一个进程用
/// 不同 fd 重入也会被自己的锁挡住，所以 daemon 清理自己正在写的文件也不会误删。
#[cfg(unix)]
fn is_in_use(path: &Path) -> bool {
    use std::os::unix::io::AsRawFd;
    let Ok(f) = std::fs::File::open(path) else {
        // 打不开（权限/已删）→ 不删，交给调用方按失败处理。
        return true;
    };
    let fd = f.as_raw_fd();
    let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        unsafe {
            libc::flock(fd, libc::LOCK_UN);
        }
        false
    } else {
        true
    }
}

#[cfg(not(unix))]
fn is_in_use(_path: &Path) -> bool {
    false
}

// ---------------------------------------------------------------- 扫描 / 清理

/// 一个日志文件（给 `qxync log` / GUI 展示用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFile {
    pub name: String,
    pub size: u64,
    pub mtime: SystemTime,
}

/// 列出目录里的 qxync 日志文件（按 mtime 从新到旧）。
pub fn scan(dir: &Path) -> Vec<LogFile> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        if !is_qxync_log_name(&name) {
            continue;
        }
        let Ok(md) = ent.metadata() else { continue };
        if !md.is_file() {
            continue;
        }
        out.push(LogFile {
            name,
            size: md.len(),
            mtime: md.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        });
    }
    out.sort_by(|a, b| b.mtime.cmp(&a.mtime).then(b.name.cmp(&a.name)));
    out
}

/// 目录里日志文件的数量与总字节数（不含非 qxync 文件）。
pub fn usage(dir: &Path) -> (usize, u64) {
    let files = scan(dir);
    (files.len(), files.iter().map(|f| f.size).sum())
}

/// 清理结果。
#[derive(Debug, Clone, Default)]
pub struct PruneReport {
    /// 删掉的文件（路径 + 字节）。
    pub deleted: Vec<(PathBuf, u64)>,
    pub freed_bytes: u64,
    /// 清理后剩下的 qxync 日志总字节。
    pub kept_bytes: u64,
    /// 扫到的 qxync 日志文件数（清理前）。
    pub scanned: usize,
    /// 想删但没删掉的（权限 / 被占用）—— 如实报出来，不静默吞。
    pub failed: Vec<(PathBuf, String)>,
    /// 因为「正被某个进程持有」而跳过的（`flock` 探测到写入器还开着）。
    pub in_use: Vec<PathBuf>,
}

impl PruneReport {
    pub fn deleted_count(&self) -> usize {
        self.deleted.len()
    }
}

/// 按策略清理 `dir` 里的 qxync 日志。`keep` 里的路径**绝不删**（调用方显式保护）。
///
/// 规则与顺序：
///
/// 1. **正在被写的文件不删**：`flock` 探测（写入器持 `LOCK_EX`，见 [`RotatingWriter`]）。
///    加得上锁 = 没人在写；加不上 = 跳过。daemon / GUI / 多连接各自的活动文件都靠它保护，
///    不依赖 mtime 猜测（GUI 可能几天不写一行日志，光看 mtime 会误判成「死文件」）。
///    旧版本写的文件没有锁 —— 但旧版本进程不会被新版本清理任务撞上（同一时刻只有一个
///    daemon），写路径也不受影响；
/// 2. `keep_days > 0`：mtime 早于 `keep_days` 天前 → 删；
/// 3. `max_total_bytes > 0`：总量超限时**从最旧的删起**，直到不超限或删无可删。
///
/// 删除失败（权限/占用）只跳过并在报告里如实反映（`freed_bytes` 不含它）。
pub fn prune(dir: &Path, pol: &LogPolicy, keep: &[PathBuf]) -> io::Result<PruneReport> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        // 目录不存在 = 没有日志可清，不是错误。
        return Ok(PruneReport::default());
    };
    let mut entries: Vec<(PathBuf, String, u64, SystemTime)> = Vec::new();
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        if family_of(&name).is_none() {
            continue;
        }
        let Ok(md) = ent.metadata() else { continue };
        if !md.is_file() {
            continue;
        }
        entries.push((
            ent.path(),
            name,
            md.len(),
            md.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        ));
    }
    let scanned = entries.len();
    if entries.is_empty() {
        return Ok(PruneReport::default());
    }

    // 保护集：调用方显式 keep + 正在被写的（flock 探测）
    let mut protected: std::collections::HashSet<PathBuf> = keep.iter().cloned().collect();
    let mut in_use: Vec<PathBuf> = Vec::new();
    for (path, _name, _size, _mtime) in &entries {
        if protected.contains(path) {
            continue;
        }
        if is_in_use(path) {
            in_use.push(path.clone());
            protected.insert(path.clone());
        }
    }

    let mut doomed: Vec<(PathBuf, u64)> = Vec::new();
    if pol.keep_days > 0 {
        let cutoff = SystemTime::now()
            .checked_sub(Duration::from_secs(pol.keep_days as u64 * 86_400))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        for (path, _name, size, mtime) in &entries {
            if *mtime < cutoff && !protected.contains(path) {
                doomed.push((path.clone(), *size));
            }
        }
    }

    // 总量从最旧的删起（跳过已判死的与受保护的）
    let total: u64 = entries.iter().map(|(_, _, s, _)| *s).sum();
    let mut remaining = total - doomed.iter().map(|(_, s)| *s).sum::<u64>();
    if pol.max_total_bytes > 0 && remaining > pol.max_total_bytes {
        let mut rest: Vec<&(PathBuf, String, u64, SystemTime)> = entries
            .iter()
            .filter(|(path, _, _, _)| {
                !protected.contains(path) && !doomed.iter().any(|(d, _)| d == path)
            })
            .collect();
        rest.sort_by(|a, b| a.3.cmp(&b.3).then(a.1.cmp(&b.1)));
        for (path, _name, size, _mtime) in rest {
            if remaining <= pol.max_total_bytes {
                break;
            }
            doomed.push((path.clone(), *size));
            remaining -= *size;
        }
    }

    let mut rep = PruneReport {
        scanned,
        kept_bytes: total,
        in_use,
        ..Default::default()
    };
    for (path, size) in doomed {
        match std::fs::remove_file(&path) {
            Ok(()) => {
                rep.deleted.push((path, size));
                rep.freed_bytes += size;
                rep.kept_bytes = rep.kept_bytes.saturating_sub(size);
            }
            Err(e) => rep.failed.push((path, e.to_string())),
        }
    }
    Ok(rep)
}

// ---------------------------------------------------------------- 写路径（切段）

/// 按天 + 按大小切段的日志写入器。
///
/// 用法（daemon / GUI 都一样）：
///
/// ```ignore
/// let writer = RotatingWriter::new(dir, "qxyncd")?;
/// let (nb, guard) = tracing_appender::non_blocking(writer);   // 后台线程落盘
/// // guard 必须活到进程结束（Drop 时 flush）
/// ```
///
/// 文件名：`<前缀>.log.<YYYY-MM-DD>`（当前段）、`<前缀>.log.<YYYY-MM-DD>.<n>`（当天
/// 已切出的旧段，n 从 1 递增）。`<前缀>` 就是旧的 `tracing_appender` 前缀去掉 `.log`。
pub struct RotatingWriter {
    dir: PathBuf,
    prefix: String,
    state: Mutex<WriterState>,
}

struct WriterState {
    file: File,
    path: PathBuf,
    day: String,
    bytes: u64,
    /// 日期检查节流：日志量大时别每写一行都调一次 `localtime_r`。
    last_day_check: Instant,
}

impl RotatingWriter {
    /// 打开（必要时创建）今天这个段。目录不可写会返回错误 —— 调用方自己决定退路
    /// （daemon 退到只写 stderr，见 `init_logging`）。
    pub fn new(dir: impl Into<PathBuf>, prefix: &str) -> io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        let day = today_tag();
        let path = dir.join(format!("{prefix}.log.{day}"));
        let file = open_append(&path)?;
        let bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
        let w = Self {
            dir: dir.clone(),
            prefix: prefix.to_string(),
            state: Mutex::new(WriterState {
                file,
                path: path.clone(),
                day,
                bytes,
                last_day_check: Instant::now(),
            }),
        };
        // 上次进程留下的超限段：开局就切（否则它要等到下一次写才切，而那可能
        // 已经晚了 —— 用户重启 daemon 时正是一个「昨天刷爆 5 GB」的现场）。
        let pol = policy();
        if pol.max_file_bytes > 0 && bytes >= pol.max_file_bytes {
            let mut st = w.lock();
            rotate_size(&dir, &prefix.to_string(), &mut st)?;
            let active = st.path.clone();
            drop(st);
            let _ = prune(&dir, &pol, &[active]);
        }
        Ok(w)
    }

    /// 当前正在写的文件（清理时当 keep 用；测试/诊断也会读）。
    pub fn current_path(&self) -> PathBuf {
        self.lock().path.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, WriterState> {
        // 写路径绝不能因为「别的线程 panic 过」就整个停掉日志。
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    let f = OpenOptions::new().create(true).append(true).open(path)?;
    // ★ M39：写入器一直持有排它锁 —— 清理任务靠它判断「谁在写」。
    lock_exclusive(&f);
    Ok(f)
}

/// 当前段 → `<前缀>.log.<day>.<n>`，再开一个新的当前段（大小超限时）。
fn rotate_size(dir: &Path, prefix: &str, st: &mut WriterState) -> io::Result<()> {
    if st.bytes == 0 {
        return Ok(());
    }
    let base = dir.join(format!("{prefix}.log.{}", st.day));
    let mut n = 1u32;
    while n < 100_000 {
        if !dir.join(format!("{prefix}.log.{}.{}", st.day, n)).exists() {
            break;
        }
        n += 1;
    }
    let target = dir.join(format!("{prefix}.log.{}.{}", st.day, n));
    let _ = st.file.flush();
    // 注意：rename 之后旧文件句柄仍指向被改名的 inode，所以必须先换句柄再写。
    std::fs::rename(&base, &target)?;
    let file = open_append(&base)?;
    st.file = file;
    st.path = base;
    st.bytes = 0;
    Ok(())
}

/// 跨天：换到新的日期段。
fn rotate_day(dir: &Path, prefix: &str, st: &mut WriterState, day: &str) -> io::Result<()> {
    let path = dir.join(format!("{prefix}.log.{day}"));
    let file = open_append(&path)?;
    let bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
    st.file = file;
    st.path = path;
    st.day = day.to_string();
    st.bytes = bytes;
    Ok(())
}

impl io::Write for RotatingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let pol = policy();
        let dir = self.dir.clone();
        let prefix = self.prefix.clone();
        let mut st = self.lock();

        // 跨天检查（每秒最多查一次系统时间）
        if st.last_day_check.elapsed() >= Duration::from_secs(1) {
            st.last_day_check = Instant::now();
            let day = today_tag();
            if day != st.day {
                rotate_day(&dir, &prefix, &mut st, &day)?;
                // 跨天时顺手清一次（新段刚建；旧段只要没人持锁、过了保留期就删）
                let active = st.path.clone();
                drop(st);
                let _ = prune(&dir, &pol, &[active]);
                st = self.lock();
            }
        }

        // 大小检查：写下去会超上限 → 先切段再写（当前段为空时不切，避免空段无限切）
        if pol.max_file_bytes > 0
            && st.bytes > 0
            && st.bytes + buf.len() as u64 > pol.max_file_bytes
        {
            rotate_size(&dir, &prefix, &mut st)?;
            let active = st.path.clone();
            drop(st);
            let _ = prune(&dir, &pol, &[active]);
            st = self.lock();
        }

        let n = st.file.write(buf)?;
        st.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.lock().file.flush()
    }
}

impl Drop for RotatingWriter {
    fn drop(&mut self) {
        let _ = self.lock().file.flush();
    }
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    /// 改全局策略的测试必须串行（cargo test 默认多线程）。
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qxync-m39-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_with_mtime(path: &Path, len: usize, days_ago: u64) {
        std::fs::write(path, vec![b'x'; len]).unwrap();
        let t = SystemTime::now() - Duration::from_secs(days_ago * 86_400);
        let f = OpenOptions::new().write(true).open(path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(t))
            .unwrap();
    }

    #[test]
    fn policy_from_settings_maps_mb() {
        let mut s = Settings::default();
        s.log_max_file_mb = 8;
        s.log_keep_days = 3;
        s.log_max_total_mb = 64;
        let p = LogPolicy::from_settings(&s);
        assert_eq!(p.max_file_bytes, 8 * MIB);
        assert_eq!(p.keep_days, 3);
        assert_eq!(p.max_total_bytes, 64 * MIB);
        assert_eq!(LogPolicy::default().keep_days, DEFAULT_KEEP_DAYS);
    }

    #[test]
    fn civil_from_days_epoch_and_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_000), (2022, 1, 8));
        // 2026-10-27 = 1970-01-01 + 20753 天（与 `datetime.date` 核对过）
        assert_eq!(civil_from_days(20_753), (2026, 10, 27));
    }

    #[test]
    fn only_qxync_log_names_are_touched() {
        assert!(is_qxync_log_name("qxyncd.log.2026-10-27"));
        assert!(is_qxync_log_name("qxyncd-abc.log.2026-10-27.3"));
        assert!(is_qxync_log_name("qxync-gui.log.2026-10-27"));
        for other in ["notes.txt", "qxync.db", "readme.log", ".hidden", "qxyncd"] {
            assert!(!is_qxync_log_name(other), "{other} 不该被当成日志文件");
        }
        assert_eq!(family_of("qxyncd-abc.log.2026-10-27"), Some("qxyncd-abc"));
    }

    /// ★ M39 主回归：同一天内写超上限必须切段，不能再出现 5 GB 单文件。
    #[test]
    fn size_rotation_splits_one_day_into_segments() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tmp("writer-size");
        set_policy(LogPolicy {
            max_file_bytes: 1024,
            keep_days: 0,
            max_total_bytes: 0,
        });
        {
            let mut w = RotatingWriter::new(&dir, "qxyncd").unwrap();
            for _ in 0..10 {
                w.write_all(&[b'x'; 300]).unwrap();
            }
            w.flush().unwrap();
            let today = today_tag();
            assert_eq!(w.current_path(), dir.join(format!("qxyncd.log.{today}")));
        }
        let today = today_tag();
        let files = scan(&dir);
        assert!(
            files.len() >= 3,
            "3000 字节 / 1KiB 上限应切成多段，实际: {files:?}"
        );
        assert!(
            files
                .iter()
                .any(|f| f.name.starts_with(&format!("qxyncd.log.{today}."))),
            "必须出现当天编号段: {files:?}"
        );
        let active = std::fs::metadata(dir.join(format!("qxyncd.log.{today}")))
            .unwrap()
            .len();
        assert!(active <= 1024, "当前段不能超过上限: {active}");
        let total: u64 = files.iter().map(|f| f.size).sum();
        assert_eq!(total, 3000, "切段不能丢内容");
        set_policy(LogPolicy::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 开局就遇到昨天留下的大文件（真机 5.2 GB 现场）→ 直接切段，不等下一次写。
    #[test]
    fn oversized_existing_file_is_rotated_at_open() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tmp("writer-open-rotate");
        std::fs::write(
            dir.join(format!("qxyncd.log.{}", today_tag())),
            vec![b'x'; 4096],
        )
        .unwrap();
        set_policy(LogPolicy {
            max_file_bytes: 1024,
            keep_days: 0,
            max_total_bytes: 0,
        });
        let w = RotatingWriter::new(&dir, "qxyncd").unwrap();
        assert_eq!(
            w.current_path(),
            dir.join(format!("qxyncd.log.{}", today_tag())),
            "开的是新的当前段"
        );
        drop(w);
        let files = scan(&dir);
        assert!(
            files
                .iter()
                .any(|f| f.name == format!("qxyncd.log.{}.1", today_tag())),
            "超限的旧文件应被切段保留: {files:?}"
        );
        set_policy(LogPolicy::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 按天数删：超过 keep_days 的删掉，近期的保留（不管它属于哪个前缀族）。
    #[test]
    fn prune_by_age_keeps_recent_files() {
        let dir = tmp("prune-age");
        write_with_mtime(&dir.join("qxyncd.log.2026-09-01"), 1000, 50);
        write_with_mtime(&dir.join("qxyncd.log.2026-10-01"), 1000, 20);
        write_with_mtime(&dir.join("qxyncd.log.2026-10-27"), 1000, 0);
        write_with_mtime(&dir.join("qxync-gui.log.2026-09-01"), 500, 50);
        write_with_mtime(&dir.join("qxync-gui.log.2026-10-27"), 500, 0);
        // 非日志文件：绝不碰
        std::fs::write(dir.join("notes.txt"), b"keep me").unwrap();

        let rep = prune(
            &dir,
            &LogPolicy {
                max_file_bytes: 0,
                keep_days: 14,
                max_total_bytes: 0,
            },
            &[],
        )
        .unwrap();
        // 删 3 个：qxyncd 的 09-01/10-01（都早于 14 天前）+ qxync-gui 的 09-01
        assert_eq!(rep.deleted_count(), 3, "早于保留期的应被删: {rep:?}");
        assert_eq!(rep.freed_bytes, 2500);
        assert!(dir.join("qxyncd.log.2026-10-27").exists());
        assert!(dir.join("qxync-gui.log.2026-10-27").exists());
        assert!(dir.join("notes.txt").exists(), "非日志文件不能被删");
        assert!(!dir.join("qxyncd.log.2026-09-01").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 按总占用删：从最旧的删起，直到不超限；最新的仍然保留。
    #[test]
    fn prune_by_total_size_deletes_oldest_first() {
        let dir = tmp("prune-total");
        write_with_mtime(&dir.join("qxyncd.log.2026-10-20"), 1000, 7);
        write_with_mtime(&dir.join("qxyncd.log.2026-10-21"), 1000, 6);
        write_with_mtime(&dir.join("qxyncd.log.2026-10-22"), 1000, 5);
        write_with_mtime(&dir.join("qxyncd.log.2026-10-23"), 1000, 4);
        let rep = prune(
            &dir,
            &LogPolicy {
                max_file_bytes: 0,
                keep_days: 0,
                max_total_bytes: 2500,
            },
            &[],
        )
        .unwrap();
        assert_eq!(rep.deleted_count(), 2, "4000 → 2000 应删两个: {rep:?}");
        assert!(dir.join("qxyncd.log.2026-10-22").exists());
        assert!(
            dir.join("qxyncd.log.2026-10-23").exists(),
            "最新一个必须保留"
        );
        assert_eq!(rep.kept_bytes, 2000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 调用方显式 keep 的文件（正在写的）即使很旧也不能删。
    #[test]
    fn prune_keeps_explicit_active_path() {
        let dir = tmp("prune-keep");
        let old = dir.join("qxyncd.log.2026-09-01");
        write_with_mtime(&old, 1000, 90);
        let rep = prune(
            &dir,
            &LogPolicy {
                max_file_bytes: 0,
                keep_days: 1,
                max_total_bytes: 0,
            },
            &[old.clone()],
        )
        .unwrap();
        assert_eq!(rep.deleted_count(), 0);
        assert!(old.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ★ M39 关键回归：**正在被写的文件绝不删** —— 写入器持有 flock，
    /// 清理任务即便觉得它「很旧」也必须跳过。旧设计靠「每族最新一个」猜，
    /// GUI 可能几天不写日志，会被误判成死文件。
    #[test]
    fn prune_skips_file_locked_by_writer() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tmp("prune-locked");
        set_policy(LogPolicy {
            max_file_bytes: 0,
            keep_days: 0,
            max_total_bytes: 0,
        });
        let w = RotatingWriter::new(&dir, "qxyncd").unwrap();
        let active = w.current_path();
        // 造出「这个活动文件很旧」的现场（mtime 90 天前）
        let f = OpenOptions::new().write(true).open(&active).unwrap();
        f.set_times(
            std::fs::FileTimes::new()
                .set_modified(SystemTime::now() - Duration::from_secs(90 * 86_400)),
        )
        .unwrap();
        drop(f);

        let rep = prune(
            &dir,
            &LogPolicy {
                max_file_bytes: 0,
                keep_days: 1,
                max_total_bytes: 0,
            },
            &[],
        )
        .unwrap();
        assert_eq!(rep.deleted_count(), 0, "正在写的文件不能删: {rep:?}");
        assert_eq!(rep.in_use, vec![active.clone()], "要如实报告被谁挡下");
        assert!(active.exists());
        drop(w);
        set_policy(LogPolicy::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 没有被任何进程持有的旧文件（例如 GUI 早就退出、只剩一个 40 天前的日志）
    /// **应该**被按天数删掉 —— 「每族最新一个」的保护不能把死文件也保护起来。
    #[test]
    fn prune_deletes_old_unlocked_lone_file() {
        let dir = tmp("prune-lone");
        let lone = dir.join("qxync-gui.log.2026-09-01");
        write_with_mtime(&lone, 500, 40);
        let rep = prune(
            &dir,
            &LogPolicy {
                max_file_bytes: 0,
                keep_days: 14,
                max_total_bytes: 0,
            },
            &[],
        )
        .unwrap();
        assert_eq!(rep.deleted_count(), 1, "没人写的旧文件该删: {rep:?}");
        assert!(!lone.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 目录不存在 / 空目录 = 无事发生，不报错。
    #[test]
    fn prune_missing_dir_is_ok() {
        let dir = std::env::temp_dir().join(format!("qxync-m39-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let rep = prune(&dir, &LogPolicy::default(), &[]).unwrap();
        assert_eq!(rep.scanned, 0);
    }
}
