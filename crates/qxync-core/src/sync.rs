//! # M2c：变更发现与三向合并（**纯逻辑**，不碰网络/挂载）
//!
//! 模型照报告 08 §8.3 / 11 §5：**服务端多路递增日志 + 客户端持久化游标**。
//! 这一层只做「可以离线单测」的部分，IO 由 client / daemon 负责：
//!
//! * `SyncEvent` / `parse_sync_log`：`qbox_get_sync_log` 的事件解析（真机样本见单测）。
//! * `map_event_path`：真实路径 `/share/homes/<user>/x` → Qsync 视图 `/home/x`。
//! * `decide`：`baseline × local × remote` 三向比较 → 6 种动作（含冲突、删除）。
//! * `conflict_name`：`a.txt` → `a (conflicted copy from <device> 2026-09-30).txt`。
//! * `Cursors` / `Baseline`：**临时文件 + fsync + rename + fsync 父目录** 的原子持久化。
//! * `DeleteProtection`：大批删除熔断（远端大量删除 vs 本地改动）。
//!
//! 真机实测要点（2026-09-30，写进 `docs/M2c-变更发现.md`）：
//! 1. `qbox_get_sync_log` 的 `lower` 是**闭区间下界**（`lower=30` 会返回 `log_id=30`）。
//! 2. 没有任何事件时返回 `status:-17`（旧谜团），**不是协议错**；此时不推进游标即可。
//! 3. 事件里 `isfolder`：`1`=目录、`2`=文件、`0`=删除项。
//! 4. 删除事件的 `filepath` 实测**为空** → 删除只能靠 baseline 对账（`get_list` 差集）发现。
//! 5. 事件 `filepath` 是真实路径 `/share/homes/<user>/...`，必须映射回 `/home/...`。

use crate::error::{Error, Result};
use crate::model::DirEntry;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------- 常量

/// 一次 `qbox_get_sync_log` 的默认条数（真机 `server_limit=256`，取 200 稳妥）。
pub const DEFAULT_LOG_BATCH: usize = 200;
/// 游标状态文件名。
pub const CURSORS_FILE: &str = "cursors.json";
/// baseline 状态文件名。
pub const BASELINE_FILE: &str = "baseline.json";
/// 默认轮询间隔（秒）。
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 30;

// ---------------------------------------------------------------- 事件

/// `qbox_get_sync_log` 的一条事件（真机字段，见模块头注释）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncEvent {
    pub log_id: i64,
    /// action 码：`1`=删除、`12`=新建目录、`14`=文件新增/修改（真机观测值）。
    pub action: i64,
    /// 真机：`1`=目录、`2`=文件、`0`=删除项。
    pub isfolder: i64,
    /// **真实路径** `/share/homes/<user>/...`（删除事件实测为空字符串）。
    pub filepath: String,
    #[serde(default)]
    pub old_filepath: String,
    #[serde(default)]
    pub exist: bool,
    #[serde(default)]
    pub mtime: i64,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub device: Option<String>,
    #[serde(default)]
    pub device_uid: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
}

impl SyncEvent {
    pub fn is_dir(&self) -> bool {
        self.isfolder == 1
    }
    pub fn is_delete(&self) -> bool {
        !self.exist || self.action == ACTION_DELETE
    }
    /// 事件是否可用：删除事件的 filepath 实测为空 → 只能靠对账发现。
    pub fn has_usable_path(&self) -> bool {
        !self.filepath.trim().is_empty()
    }
}

/// 真机观测到的 action 码（报告未确认枚举，与 client::write_action 保持一致）。
pub const ACTION_DELETE: i64 = 1;
pub const ACTION_CREATE_DIR: i64 = 12;
pub const ACTION_UPSERT_FILE: i64 = 14;

/// `qbox_get_sync_log` 的一批返回。
#[derive(Debug, Clone, Default)]
pub struct SyncLogBatch {
    /// 服务端 `end` 字段：1 = 已到日志末尾。
    pub end: i64,
    /// 本批条数（服务端 `number`）。
    pub number: usize,
    pub max_log: Option<u64>,
    pub global_notify: Option<u64>,
    pub events: Vec<SyncEvent>,
}

impl SyncLogBatch {
    /// 下一批的 `lower`：`lower` 是**闭区间下界**，所以推进到「最后一个 log_id + 1」。
    pub fn next_lower(&self, fallback: i64) -> i64 {
        self.events
            .iter()
            .map(|e| e.log_id)
            .max()
            .map(|m| m + 1)
            .unwrap_or(fallback)
    }
}

/// 判断一个错误是不是「日志区间为空 / 日志已滚动」——`status:-17`。
pub fn is_log_missing(e: &Error) -> bool {
    matches!(e, Error::Status { status, .. } if status.0 == -17)
}

/// 解析 `qbox_get_sync_log` 响应。
///
/// * `status` 缺失 → 视为成功（真机旧版行为）。
/// * `status:-17` → `Error::Status`，调用方用 [`is_log_missing`] 判定「没有事件」。
pub fn parse_sync_log(body: &[u8]) -> Result<SyncLogBatch> {
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| {
        Error::Parse(format!(
            "qbox_get_sync_log 解析失败: {e}；原文前 200B: {}",
            String::from_utf8_lossy(&body[..body.len().min(200)])
        ))
    })?;
    let status = v.get("status").and_then(as_i64);
    if let Some(s) = status {
        if s != 0 && s != 1 {
            return Err(Error::status(s, "qbox_get_sync_log"));
        }
    }
    let raw_events = v
        .get("data")
        .or_else(|| v.get("datas"))
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();
    let mut events = Vec::with_capacity(raw_events.len());
    for raw in raw_events {
        events.push(parse_event(&raw)?);
    }
    Ok(SyncLogBatch {
        end: v.get("end").and_then(as_i64).unwrap_or(1),
        number: v
            .get("number")
            .and_then(as_i64)
            .map(|n| n.max(0) as usize)
            .unwrap_or(events.len()),
        max_log: v.get("max_log").and_then(as_u64),
        global_notify: v.get("global_notify").and_then(as_u64),
        events,
    })
}

fn parse_event(raw: &serde_json::Value) -> Result<SyncEvent> {
    let get = |k: &str| raw.get(k).cloned().unwrap_or(serde_json::Value::Null);
    let s = |k: &str| match get(k) {
        serde_json::Value::String(s) => s,
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    };
    let opt = |k: &str| {
        let v = get(k);
        match v {
            serde_json::Value::Null => None,
            serde_json::Value::String(s) if s.is_empty() => None,
            serde_json::Value::String(s) => Some(s),
            other => Some(other.to_string()),
        }
    };
    Ok(SyncEvent {
        log_id: as_i64(&get("log_id")).unwrap_or(0),
        action: as_i64(&get("action")).unwrap_or(0),
        isfolder: as_i64(&get("isfolder")).unwrap_or(0),
        filepath: s("filepath"),
        old_filepath: s("old_filepath"),
        exist: matches!(get("exist"), serde_json::Value::Bool(true))
            || as_i64(&get("exist")).unwrap_or(0) != 0,
        mtime: as_i64(&get("mtime")).unwrap_or(0),
        size: as_u64(&get("size")).unwrap_or(0),
        device: opt("device"),
        device_uid: opt("device_uid"),
        user: opt("user"),
    })
}

fn as_i64(v: &serde_json::Value) -> Option<i64> {
    match v {
        serde_json::Value::Number(n) => n.as_i64(),
        serde_json::Value::String(s) => s.parse().ok(),
        serde_json::Value::Bool(b) => Some(*b as i64),
        _ => None,
    }
}

fn as_u64(v: &serde_json::Value) -> Option<u64> {
    match v {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// 真实路径 → **这个挂载视图**的远端路径。
///
/// 真机事件里是 `/share/homes/<user>/qxync-test/a.txt`（NAS 侧真实路径），而挂载视图用的是
/// Qsync 命名空间的路径（家目录挂载就是 `/home`，共享文件夹挂载就是 `/Public`…）。
/// 已经是视图内路径的原样通过；别的用户 / 归属不明的路径一律丢弃 —— 报告 11 §5.3 明确要求
/// 校验归属，避免处理他人的事件。
///
/// `view_root` 是**调用方的挂载根**（`MountView::remote_root`），不是「家目录」：
/// 挂 `/Public` 时就传 `/Public`。
pub fn map_event_path(real: &str, user: &str, view_root: &str) -> Option<String> {
    let real = real.trim();
    if real.is_empty() {
        return None;
    }
    let view_root = view_root.trim_end_matches('/');
    if let Some(rest) = real.strip_prefix(view_root) {
        // 视图根自身或它下面；注意别把 /homebrew 当成 /home
        if rest.is_empty() || rest.starts_with('/') {
            return Some(if rest.is_empty() {
                view_root.to_string()
            } else {
                format!("{view_root}{rest}")
            });
        }
    }
    let user_home = format!("/share/homes/{user}");
    if let Some(rest) = real.strip_prefix(&user_home) {
        if rest.is_empty() {
            return Some(view_root.to_string());
        }
        if rest.starts_with('/') {
            return Some(format!("{view_root}{rest}"));
        }
    }
    None
}

// ---------------------------------------------------------------- 三向比较

/// 一个路径的签名（base / local / remote 通用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sig {
    pub exists: bool,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
}

impl Sig {
    pub const MISSING: Sig = Sig {
        exists: false,
        is_dir: false,
        size: 0,
        mtime: 0,
    };

    pub fn file(size: u64, mtime: i64) -> Self {
        Self {
            exists: true,
            is_dir: false,
            size,
            mtime,
        }
    }

    pub fn dir() -> Self {
        Self {
            exists: true,
            is_dir: true,
            size: 0,
            mtime: 0,
        }
    }

    /// 由 `stat`/`get_list` 条目构造。
    pub fn from_entry(e: &DirEntry) -> Self {
        if e.isfolder {
            Sig::dir()
        } else {
            Sig::file(e.filesize, e.epochmt)
        }
    }

    /// 目录只比「存在性 + 类型」，文件比大小 + mtime（服务端 epoch 秒，无亚秒）。
    pub fn same_as(&self, other: &Sig) -> bool {
        if self.exists != other.exists {
            return false;
        }
        if !self.exists {
            return true;
        }
        if self.is_dir != other.is_dir {
            return false;
        }
        if self.is_dir {
            return true;
        }
        self.size == other.size && self.mtime == other.mtime
    }
}

/// 本地侧的快照（来自挂载视图）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalSig {
    pub sig: Sig,
    /// 是否有未上传的改动（FUSE dirty 或上传队列里还有该路径）。
    pub dirty: bool,
}

/// 一次三方比较的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// 三方一致，无事可做。
    Noop,
    /// 远端 == 本地，只是 baseline 落后 → 采纳远端签名。
    AdoptBaseline,
    /// 远端变了、本地没改 → 刷新元数据并失效已缓存内容。
    RefreshRemote,
    /// 本地改了、远端没变 → 入队上传。
    UploadLocal,
    /// 远端与本地都改了，且内容不同 → 冲突副本。
    Conflict,
    /// 远端删了、本地没改 → 删本地节点（受删除保护约束）。
    DeleteLocal,
    /// 远端删了、本地改了 → 重新上传（本地为准，远端删除被否决）。
    RecreateRemote,
}

/// 本地是否有「相对 baseline 的改动」。
fn local_changed(local: &LocalSig, base: &Sig) -> bool {
    if !local.sig.exists || !local.dirty {
        return false;
    }
    !local.sig.same_as(base)
}

/// ★ 三向合并决策表（M2c 的核心，全部可单测）。
///
/// ```
/// use qxync_core::sync::{decide, Decision, LocalSig, Sig};
/// // 远端改动、本地没动 → 刷新
/// let d = decide(
///     &LocalSig { sig: Sig::file(10, 100), dirty: false },
///     &Sig::file(10, 100),
///     &Sig::file(20, 200),
/// );
/// assert_eq!(d, Decision::RefreshRemote);
/// ```
pub fn decide(local: &LocalSig, base: &Sig, remote: &Sig) -> Decision {
    if !remote.exists {
        if !base.exists {
            return Decision::Noop;
        }
        return if local_changed(local, base) {
            Decision::RecreateRemote
        } else {
            Decision::DeleteLocal
        };
    }

    // 远端存在但 baseline 里没有：本地新建 / 远端新建 / 上次崩溃
    if !base.exists {
        if local.sig.exists && local.sig.same_as(remote) {
            // 本地内容与远端一致（例如本地上传已落地但 baseline 没写）→ 补 baseline
            return Decision::AdoptBaseline;
        }
        if local_changed(local, &Sig::MISSING) {
            return Decision::UploadLocal;
        }
        return Decision::RefreshRemote;
    }

    let rc = !remote.same_as(base);
    let lc = local_changed(local, base);
    match (lc, rc) {
        (false, false) => Decision::Noop,
        (true, false) => Decision::UploadLocal,
        (false, true) => {
            // 本地内容与 baseline 不同、却没标脏（外部改动/上次崩溃）→ 保守当冲突
            if local.sig.exists && !local.sig.same_as(base) {
                Decision::Conflict
            } else {
                Decision::RefreshRemote
            }
        }
        (true, true) => {
            if local.sig.exists && local.sig.same_as(remote) {
                // 本地上传已经落地（远端 == 本地）
                Decision::AdoptBaseline
            } else {
                Decision::Conflict
            }
        }
    }
}

// ---------------------------------------------------------------- 冲突副本

/// 冲突副本命名：`notes.txt` → `notes (conflicted copy from nas 2026-09-30).txt`。
///
/// 规则照 Qsync 的习惯：保留原名 + 在同一目录里生成副本，绝不覆盖任何一方。
pub fn conflict_name(original: &str, device: &str, date: &str) -> String {
    let (stem, ext) = split_ext(original);
    let device = sanitize_component(device);
    let date = sanitize_component(date);
    match ext {
        Some(ext) => format!("{stem} (conflicted copy from {device} {date}).{ext}"),
        None => format!("{stem} (conflicted copy from {device} {date})"),
    }
}

/// 同名冲突副本已存在时再补一个序号：`... (2).txt`。
pub fn conflict_name_with_seq(original: &str, device: &str, date: &str, seq: u32) -> String {
    if seq <= 1 {
        return conflict_name(original, device, date);
    }
    let (stem, ext) = split_ext(original);
    let device = sanitize_component(device);
    let date = sanitize_component(date);
    match ext {
        Some(ext) => format!("{stem} (conflicted copy from {device} {date} {seq}).{ext}"),
        None => format!("{stem} (conflicted copy from {device} {date} {seq})"),
    }
}

fn split_ext(name: &str) -> (&str, Option<&str>) {
    match name.rfind('.') {
        // `.bashrc` 这种点开头的不算扩展名
        Some(i) if i > 0 && i + 1 < name.len() => (&name[..i], Some(&name[i + 1..])),
        _ => (name, None),
    }
}

fn sanitize_component(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ---------------------------------------------------------------- 删除保护

/// 大批删除熔断参数（远端删除）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeleteProtection {
    /// 一次对账中允许删除的最大条目数。
    pub max_entries: usize,
    /// 允许删除的 baseline 占比（0..=1）；baseline 太小时不启用比例判定。
    pub max_ratio: f64,
    /// 启用比例判定所需的最小 baseline 条目数。
    pub min_entries_for_ratio: usize,
}

impl Default for DeleteProtection {
    fn default() -> Self {
        Self {
            max_entries: 50,
            max_ratio: 0.25,
            min_entries_for_ratio: 20,
        }
    }
}

impl DeleteProtection {
    /// `Ok(())` = 允许执行；`Err(原因)` = 熔断。
    pub fn check(&self, deletes: usize, baseline_len: usize) -> std::result::Result<(), String> {
        if deletes == 0 {
            return Ok(());
        }
        if deletes > self.max_entries {
            return Err(format!(
                "一次对账要删除 {deletes} 项，超过上限 {}（远端可能被清空/卸载）",
                self.max_entries
            ));
        }
        if baseline_len >= self.min_entries_for_ratio {
            let ratio = deletes as f64 / baseline_len as f64;
            if ratio > self.max_ratio {
                return Err(format!(
                    "一次对账要删除 {deletes}/{baseline_len} 项（{:.0}%），超过阈值 {:.0}%",
                    ratio * 100.0,
                    self.max_ratio * 100.0
                ));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- 游标

/// 客户端三个持久化游标 + 观测到的最大日志号。
///
/// 对应 Windows 客户端注册表里的
/// `QSYNC_PROCESSED_MAX_LOG_INDEX_64` / `..._NOTIFY_LOG_INDEX_64` /
/// `..._GLOBAL_NOTIFY_LOG_INDEX_64`（报告 01 §7.3 / 09 §9.5）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursors {
    /// config log 游标（设备/同步文件夹配置变更）。
    #[serde(default)]
    pub config: i64,
    /// notify log 游标（文件变更，**M2c 的主战场**）。
    #[serde(default)]
    pub notify: i64,
    /// global notify log 游标（共享邀请、团队文件夹）。
    #[serde(default)]
    pub global_notify: i64,
    /// 上次看到的 `max_log`：回退说明服务端日志被清空/迁移 → 归零重扫。
    #[serde(default)]
    pub max_log_seen: u64,
    /// `qbox_get_sync_log` 连续返回 `-17`（日志为空）的次数，仅诊断用。
    #[serde(default)]
    pub log_missing_count: u64,
}

impl Cursors {
    /// 服务端日志回退（max_log 变小）→ 必须归零 + 全量重扫（报告 08 §8.3 铁律 3）。
    pub fn should_reset(&self, max_log: u64) -> bool {
        max_log < self.max_log_seen
    }

    pub fn reset(&mut self) {
        *self = Cursors::default();
    }
}

// ---------------------------------------------------------------- 持久化

/// baseline：路径 → 上次同步成功时的**远端签名**。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Baseline {
    #[serde(default = "baseline_version")]
    pub version: u32,
    #[serde(default)]
    pub entries: BTreeMap<String, Sig>,
}

fn baseline_version() -> u32 {
    1
}

impl Baseline {
    pub fn get(&self, path: &str) -> Sig {
        self.entries.get(path).copied().unwrap_or(Sig::MISSING)
    }
    pub fn put(&mut self, path: impl Into<String>, sig: Sig) {
        self.entries.insert(path.into(), sig);
    }
    pub fn remove(&mut self, path: &str) -> Option<Sig> {
        self.entries.remove(path)
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    /// 该目录在 baseline 里是否有条目（用于对账时决定要不要列这个目录）。
    pub fn has_children_of(&self, dir: &str) -> bool {
        let prefix = format!("{}/", dir.trim_end_matches('/'));
        self.entries.keys().any(|k| k.starts_with(&prefix))
    }
    /// 某个目录在 baseline 里的直接子项路径。
    pub fn children_of<'a>(&'a self, dir: &'a str) -> Vec<(&'a str, Sig)> {
        let prefix = format!("{}/", dir.trim_end_matches('/'));
        self.entries
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix))
            .filter(|(k, _)| !k[prefix.len()..].contains('/'))
            .map(|(k, v)| (k.as_str(), *v))
            .collect()
    }
    /// 某个目录下所有后代路径（含深层）。
    pub fn descendants_of<'a>(&'a self, dir: &'a str) -> Vec<&'a str> {
        let prefix = format!("{}/", dir.trim_end_matches('/'));
        self.entries
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .map(|k| k.as_str())
            .collect()
    }
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(raw) => serde_json::from_slice(&raw)
                .map_err(|e| Error::Parse(format!("baseline {} 解析失败: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                version: baseline_version(),
                entries: BTreeMap::new(),
            }),
            Err(e) => Err(Error::Io(format!("读取 {}: {e}", path.display()))),
        }
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        let body = serde_json::to_vec(self)?;
        atomic_write(path, &body)
    }
}

impl Cursors {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(raw) => serde_json::from_slice(&raw)
                .map_err(|e| Error::Parse(format!("游标 {} 解析失败: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(Error::Io(format!("读取 {}: {e}", path.display()))),
        }
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        let body = serde_json::to_vec_pretty(self)?;
        atomic_write(path, &body)
    }
}

/// ★ 原子写：**临时文件 + fsync + rename + fsync 父目录**（报告 08 §8.3 铁律 2）。
///
/// 崩溃时要么看到旧文件、要么看到新文件，绝不会看到半截 JSON。
pub fn atomic_write(path: &Path, body: &[u8]) -> Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(body)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent() {
        // fsync 父目录，保证 rename 本身落盘
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

/// M2c 的状态目录：`<data_dir>/sync/<host>/`（不同 NAS 的 baseline/游标互不污染）。
pub fn sync_state_dir(data_dir: &Path, host: &str) -> PathBuf {
    let host_ns: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    data_dir.join("sync").join(host_ns)
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    /// 真机（2026-09-30，Qsync QPKG 5.0.0.7）`qbox_get_sync_log&lower=30&number=50&get_detail=1`。
    const REAL_SYNC_LOG: &str = r#"{"status": 0, "end": 1, "number": 6,"data":[
      {"log_id": 30, "user": "test1", "old_filepath": "", "filepath": "/share/homes/test1/.Qsync/评分脚本/GB4-3.1.5.strategy.json", "action": 14, "isfolder": 2, "device": "win-pc", "device_uid": "0123456789abcdef0123456789abcdef01234567", "exist": 1, "mtime": 1696426405, "size": "1124"},
      {"log_id": 32, "user": "test1", "old_filepath": "", "filepath": "", "action": 1, "isfolder": 0, "device": "win-pc", "device_uid": "0123456789abcdef0123456789abcdef01234567", "exist": 0, "mtime": 0, "size": "0"},
      {"log_id": 35, "user": "test1", "old_filepath": "", "filepath": "/share/homes/test1/qxync-test/hello.txt", "action": 12, "isfolder": 1, "device": "win-pc", "device_uid": "0123456789abcdef0123456789abcdef01234567", "exist": 1, "mtime": 1790742045, "size": "0"}]}"#;

    const REAL_EMPTY_LOG: &str =
        r#"{"version":"","build":"20260723","status":-17,"success":"true"}"#;

    #[test]
    fn parses_real_sync_log_including_string_numbers() {
        let b = parse_sync_log(REAL_SYNC_LOG.as_bytes()).unwrap();
        assert_eq!(b.events.len(), 3);
        assert_eq!(b.end, 1);
        assert_eq!(b.number, 6);
        assert_eq!(b.events[0].log_id, 30);
        assert_eq!(b.events[0].size, 1124);
        assert!(!b.events[0].is_dir());
        assert_eq!(b.events[0].device.as_deref(), Some("win-pc"));
        // 删除项：filepath 为空、exist=0、action=1
        assert!(b.events[1].is_delete());
        assert!(!b.events[1].has_usable_path());
        // isfolder=1 → 目录
        assert!(b.events[2].is_dir());
        // lower 是闭区间 → 下一批从最后一个 log_id + 1 开始
        assert_eq!(b.next_lower(0), 36);
    }

    #[test]
    fn status_minus_17_is_log_missing_not_a_hard_error() {
        let e = parse_sync_log(REAL_EMPTY_LOG.as_bytes()).unwrap_err();
        assert!(is_log_missing(&e), "{e}");
        assert!(e.to_string().contains("-17"));
    }

    #[test]
    fn maps_real_paths_to_home_view() {
        assert_eq!(
            map_event_path("/share/homes/test1/qxync-test/a.txt", "test1", "/home").as_deref(),
            Some("/home/qxync-test/a.txt")
        );
        assert_eq!(
            map_event_path("/share/homes/test1", "test1", "/home").as_deref(),
            Some("/home")
        );
        // 已经在视图命名空间里
        assert_eq!(
            map_event_path("/home/qxync-test/a.txt", "test1", "/home").as_deref(),
            Some("/home/qxync-test/a.txt")
        );
        // 别的用户 / 别的共享文件夹 / 前缀撞车 → 丢弃
        assert_eq!(
            map_event_path("/share/homes/test2/a.txt", "test1", "/home"),
            None
        );
        assert_eq!(
            map_event_path("/share/Public/a.txt", "test1", "/home"),
            None
        );
        assert_eq!(map_event_path("/homebrew/a.txt", "test1", "/home"), None);
        assert_eq!(map_event_path("", "test1", "/home"), None);
    }

    /// 决策表逐条覆盖（M2c 的正确性核心）。
    #[test]
    fn decision_table() {
        let b10 = Sig::file(10, 100);
        let r10 = Sig::file(10, 100);
        let r20 = Sig::file(20, 200);
        let local = |size, mtime, dirty| LocalSig {
            sig: Sig::file(size, mtime),
            dirty,
        };

        // 三方一致
        assert_eq!(decide(&local(10, 100, false), &b10, &r10), Decision::Noop);
        // 远端改了、本地没改 → 刷新
        assert_eq!(
            decide(&local(10, 100, false), &b10, &r20),
            Decision::RefreshRemote
        );
        // 本地改了、远端没改 → 上传
        assert_eq!(
            decide(&local(15, 150, true), &b10, &r10),
            Decision::UploadLocal
        );
        // 本地上传已落地（远端 == 本地） → 只补 baseline
        assert_eq!(
            decide(&local(20, 200, true), &b10, &r20),
            Decision::AdoptBaseline
        );
        // 双方都改且不同 → 冲突
        assert_eq!(
            decide(&local(15, 150, true), &b10, &r20),
            Decision::Conflict
        );
        // 远端删了、本地没改 → 删本地
        assert_eq!(
            decide(&local(10, 100, false), &b10, &Sig::MISSING),
            Decision::DeleteLocal
        );
        // 远端删了、本地改了 → 重新上传（否决远端删除）
        assert_eq!(
            decide(&local(15, 150, true), &b10, &Sig::MISSING),
            Decision::RecreateRemote
        );
        // baseline 无、远端新建 → 刷新（占位符）
        assert_eq!(
            decide(
                &LocalSig {
                    sig: Sig::MISSING,
                    dirty: false
                },
                &Sig::MISSING,
                &r10
            ),
            Decision::RefreshRemote
        );
        // baseline 无、本地新建（脏） → 上传
        assert_eq!(
            decide(&local(3, 5, true), &Sig::MISSING, &r10),
            Decision::UploadLocal
        );
        // baseline 无、本地新建且已上传落地 → 采纳 baseline
        assert_eq!(
            decide(&local(10, 100, true), &Sig::MISSING, &r10),
            Decision::AdoptBaseline
        );
        // dirty 标志残留但内容 == baseline → 不算本地改动
        assert_eq!(
            decide(&local(10, 100, true), &b10, &r20),
            Decision::RefreshRemote
        );
        // 内容与 baseline 不同又没标脏（崩溃残留） → 保守当冲突
        assert_eq!(
            decide(&local(15, 150, false), &b10, &r20),
            Decision::Conflict
        );

        // 目录：只看存在性
        let bd = Sig::dir();
        let rd = Sig::dir();
        assert_eq!(
            decide(
                &LocalSig {
                    sig: Sig::dir(),
                    dirty: false
                },
                &bd,
                &rd
            ),
            Decision::Noop
        );
        assert_eq!(
            decide(
                &LocalSig {
                    sig: Sig::MISSING,
                    dirty: false
                },
                &bd,
                &Sig::MISSING
            ),
            Decision::DeleteLocal
        );
        assert_eq!(
            decide(
                &LocalSig {
                    sig: Sig::MISSING,
                    dirty: false
                },
                &Sig::MISSING,
                &rd
            ),
            Decision::RefreshRemote
        );
    }

    #[test]
    fn conflict_names_keep_extension_and_are_filesystem_safe() {
        assert_eq!(
            conflict_name("notes.txt", "win-pc", "2026-09-30"),
            "notes (conflicted copy from win-pc 2026-09-30).txt"
        );
        assert_eq!(
            conflict_name("archive.tar.gz", "nas/1", "2026-09-30"),
            "archive.tar (conflicted copy from nas_1 2026-09-30).gz"
        );
        assert_eq!(
            conflict_name("README", "pc", "2026-09-30"),
            "README (conflicted copy from pc 2026-09-30)"
        );
        // .bashrc 这种点开头的没有扩展名
        assert_eq!(
            conflict_name(".bashrc", "pc", "2026-09-30"),
            ".bashrc (conflicted copy from pc 2026-09-30)"
        );
        assert_eq!(
            conflict_name_with_seq("a.txt", "pc", "2026-09-30", 3),
            "a (conflicted copy from pc 2026-09-30 3).txt"
        );
        // 名字里不能出现路径分隔符
        let n = conflict_name("a.txt", "dev/with:weird*chars", "2026-09-30");
        assert!(!n.contains('/') && !n.contains(':') && !n.contains('*'));
    }

    #[test]
    fn delete_protection_trips_on_mass_delete() {
        let p = DeleteProtection::default();
        assert!(p.check(0, 100).is_ok());
        assert!(p.check(10, 100).is_ok()); // 10% < 25%
        assert!(p.check(51, 1000).is_err()); // 超绝对上限
        assert!(p.check(30, 100).is_err()); // 30% > 25%
        assert!(p.check(30, 10).is_ok()); // baseline 太小 → 不做比例判定
        let strict = DeleteProtection {
            max_entries: 5,
            max_ratio: 0.5,
            min_entries_for_ratio: 100,
        };
        assert!(strict.check(5, 3).is_ok());
        assert!(strict.check(6, 3).is_err());
    }

    #[test]
    fn cursors_and_baseline_roundtrip_atomically() {
        let dir = std::env::temp_dir().join(format!("qxync-m2c-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cpath = dir.join(CURSORS_FILE);
        let bpath = dir.join(BASELINE_FILE);

        let mut c = Cursors {
            config: 5,
            notify: 36,
            global_notify: 177,
            max_log_seen: 44,
            log_missing_count: 2,
        };
        c.save(&cpath).unwrap();
        assert_eq!(Cursors::load(&cpath).unwrap(), c);
        assert!(
            !cpath.with_extension("tmp").exists(),
            "临时文件必须被 rename 掉"
        );
        c.notify = 40;
        c.save(&cpath).unwrap();
        assert_eq!(Cursors::load(&cpath).unwrap().notify, 40);

        // 日志回退 → 归零
        assert!(c.should_reset(10));
        assert!(!c.should_reset(44));
        c.reset();
        assert_eq!(c.notify, 0);

        let mut b = Baseline::default();
        b.put("/home/a.txt", Sig::file(3, 9));
        b.put("/home/d", Sig::dir());
        b.put("/home/d/c.txt", Sig::file(1, 1));
        b.save(&bpath).unwrap();
        let back = Baseline::load(&bpath).unwrap();
        assert_eq!(back.get("/home/a.txt"), Sig::file(3, 9));
        assert_eq!(back.get("/home/missing"), Sig::MISSING);
        assert_eq!(back.len(), 3);
        assert!(back.has_children_of("/home/d"));
        assert_eq!(back.children_of("/home/d").len(), 1);
        assert_eq!(back.descendants_of("/home/d").len(), 1);
        assert_eq!(back.children_of("/home").len(), 2); // a.txt + 目录 d 是直接子项
                                                        // 不存在的文件 → 空 baseline（不是错误）
        assert!(Baseline::load(&dir.join("nope.json")).unwrap().is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sync_state_dir_is_host_scoped_and_safe() {
        let p = sync_state_dir(Path::new("/data"), "nas.example:9834");
        assert_eq!(p, PathBuf::from("/data/sync/nas.example_9834"));
    }
}
