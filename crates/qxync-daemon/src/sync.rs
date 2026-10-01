//! # M2c 同步引擎：三游标轮询 + baseline 对账 + 冲突副本 + 删除保护
//!
//! 这一层把 `qxync-core::sync` 的**纯逻辑**（决策表 / 游标 / baseline）接到真实 NAS 与挂载视图上。
//! 设计取舍（详见 `docs/M2c-变更发现.md`）：
//!
//! * **事件是快路径，对账是兜底**。真机实测：我们自己的 CGI 写操作**不产生** sync log 事件，
//!   删除事件的 `filepath` 还是空的 → 只有「列目录 + baseline 差集」才能可靠地发现变更。
//!   所以每轮都做一次「已知目录对账」，事件只是让变更更快被看到。
//! * **先处理事件、再推进游标**（崩溃宁可重放，不可丢失）；每批处理完落盘一次。
//! * `max_log` 回退 → 游标归零 + 全量重扫（报告 08 §8.3 铁律 3）。
//! * **远端优先，本地不丢**：双方都改 → 远端内容占原名，本地内容存成冲突副本并上传。
//! * **删除保护**：一次对账要删的条目超过阈值 → 整批挡住，`--force-deletes` 才放行。
//! * 引擎只依赖 [`qxync_fuse::LocalView`]，因此**不挂 FUSE 也能测**（见本文件的单测）。

use anyhow::Result;
use qxync_client::{write_action, Client};
use qxync_core::store::{Store, DB_FILE};
use qxync_core::sync::{
    conflict_name, conflict_name_with_seq, decide, is_log_missing, map_event_path, Baseline,
    Cursors, Decision, DeleteProtection, LocalSig, Sig, DEFAULT_LOG_BATCH,
};
use qxync_core::{Error as CoreError, MaxLog};
use qxync_fuse::upload::UploadQueue;
use qxync_fuse::LocalView;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 一个挂载视图（引擎的操作对象）。
pub struct MountView {
    pub mountpoint: PathBuf,
    /// 挂载根（`/home`）。
    pub remote_root: String,
    pub view: Arc<dyn LocalView>,
    /// 读写挂载的上传队列：冲突解决前必须让它停下来（在途上传会把远端重新改成"本地内容"）。
    pub upload: Option<Arc<UploadQueue>>,
    pub read_only: bool,
}

/// 引擎参数。
#[derive(Debug, Clone)]
pub struct SyncConfig {
    /// 一次 `qbox_get_sync_log` 拉多少条。
    pub batch: usize,
    /// 远端批量删除阈值。
    pub delete_protection: DeleteProtection,
    /// 本轮是否跳过删除保护（`--force-deletes`，只生效一轮）。
    pub force_deletes: bool,
    /// 自己的设备 uid（事件回声过滤）；本机未做设备配对时为空。
    pub own_devices: Vec<String>,
    /// 单轮最多对账多少个目录（防止一次扫太多）。
    pub max_dirs_per_poll: usize,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            batch: DEFAULT_LOG_BATCH,
            delete_protection: DeleteProtection::default(),
            force_deletes: false,
            own_devices: Vec::new(),
            max_dirs_per_poll: 200,
        }
    }
}

/// 一轮的统计（`qsync sync --once` 直接打印）。
#[derive(Debug, Clone, Default)]
pub struct SyncReport {
    pub max_log: u64,
    pub global_notify: u64,
    pub events: u64,
    pub events_skipped: u64,
    pub refreshed: u64,
    pub uploaded: u64,
    pub conflicts: u64,
    pub deleted: u64,
    pub deletes_blocked: u64,
    pub baseline_entries: usize,
    pub dirs_scanned: usize,
    pub devices: Vec<String>,
    pub notes: Vec<String>,
    pub errors: Vec<String>,
}

impl SyncReport {
    pub fn note(&mut self, s: impl Into<String>) {
        let s = s.into();
        if !self.notes.iter().any(|x| x == &s) {
            self.notes.push(s);
        }
    }
    pub fn error(&mut self, s: impl Into<String>) {
        let s = s.into();
        tracing::warn!("同步引擎: {s}");
        if self.errors.len() < 20 {
            self.errors.push(s);
        }
    }
}

/// 常驻计数器（`status` 用）。
#[derive(Default)]
pub struct SyncStats {
    pub polls: AtomicU64,
    pub refreshed: AtomicU64,
    pub uploaded: AtomicU64,
    pub conflicts: AtomicU64,
    pub deleted: AtomicU64,
    pub deletes_blocked: AtomicU64,
    pub events: AtomicU64,
    pub last_poll_unix: AtomicU64,
    pub last_error: Mutex<Option<String>>,
    pub delete_block_reason: Mutex<Option<String>>,
    pub last_note: Mutex<Option<String>>,
    pub devices: Mutex<BTreeMap<String, u64>>,
}

/// `SyncStats` 的无锁快照。
#[derive(Debug, Clone, Default)]
pub struct SyncStatsSnapshot {
    pub polls: u64,
    pub refreshed: u64,
    pub uploaded: u64,
    pub conflicts: u64,
    pub deleted: u64,
    pub deletes_blocked: u64,
    pub events: u64,
    pub last_poll_unix: u64,
    pub last_error: Option<String>,
    pub delete_block_reason: Option<String>,
    pub last_note: Option<String>,
    pub devices: Vec<String>,
}

impl SyncStats {
    pub fn snapshot(&self) -> SyncStatsSnapshot {
        SyncStatsSnapshot {
            polls: self.polls.load(Ordering::Relaxed),
            refreshed: self.refreshed.load(Ordering::Relaxed),
            uploaded: self.uploaded.load(Ordering::Relaxed),
            conflicts: self.conflicts.load(Ordering::Relaxed),
            deleted: self.deleted.load(Ordering::Relaxed),
            deletes_blocked: self.deletes_blocked.load(Ordering::Relaxed),
            events: self.events.load(Ordering::Relaxed),
            last_poll_unix: self.last_poll_unix.load(Ordering::Relaxed),
            last_error: self.last_error.lock().unwrap().clone(),
            delete_block_reason: self.delete_block_reason.lock().unwrap().clone(),
            last_note: self.last_note.lock().unwrap().clone(),
            devices: self
                .devices
                .lock()
                .unwrap()
                .iter()
                .map(|(k, v)| format!("{k}:{v}"))
                .collect(),
        }
    }

    fn record_devices(&self, uids: &[String]) {
        let mut g = self.devices.lock().unwrap();
        for u in uids {
            *g.entry(u.clone()).or_insert(0) += 1;
        }
    }
}

/// `<data>/sync/<host>/` 下的持久状态（游标 + baseline）。
/// 同步状态：**内存工作副本**（游标 + baseline）+ **SQLite 持久层**（M5）。
pub struct SyncState {
    pub dir: PathBuf,
    pub store: Store,
    pub cursors: Cursors,
    pub baseline: Baseline,
}

impl SyncState {
    pub fn load(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        // ★ M5：状态落在 <dir>/sync.db。M2c 的 baseline.json / cursors.json 会在首次打开时
        //   迁进库里并归档成 *.json.migrated（保留备份），之后 JSON 不再参与读写。
        let store = Store::open(dir.join(DB_FILE))?;
        let rep = store.migrate_legacy(&dir)?;
        if rep.did_something() {
            tracing::info!(
                "M5 状态迁移：游标={} baseline={} 条；旧 JSON 已归档：{}",
                rep.cursors,
                rep.baseline,
                rep.archived
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let cursors = store.cursors()?;
        let baseline = store.baseline()?;
        Ok(Self {
            dir,
            store,
            cursors,
            baseline,
        })
    }

    /// 状态库路径（诊断 / 验收用）。
    pub fn db_path(&self) -> PathBuf {
        self.dir.join(DB_FILE)
    }

    /// ★ M5 的核心：游标 + baseline **同一个事务**落盘（崩在中间也不会出现
    /// 「游标推了、baseline 没推」这种半新半旧的状态）。
    pub fn save_all(&self) -> Result<()> {
        self.store.save_state(&self.cursors, &self.baseline)?;
        Ok(())
    }
}

// ---------------------------------------------------------------- 主循环

/// 跑一轮：拉事件 + 对账全部挂载视图。
pub async fn poll_once(
    client: &Client,
    views: &[MountView],
    store: &Mutex<SyncState>,
    cfg: &SyncConfig,
    stats: &SyncStats,
    user: &str,
) -> SyncReport {
    let (mut cursors, mut baseline) = {
        let g = store.lock().unwrap();
        (g.cursors, g.baseline.clone())
    };
    let mut report = SyncReport {
        baseline_entries: baseline.len(),
        ..Default::default()
    };
    stats.polls.fetch_add(1, Ordering::Relaxed);
    stats.last_poll_unix.store(now_secs(), Ordering::Relaxed);

    // ---- ① 取 max_log；回退 → 游标归零 + 全量重扫
    let max: MaxLog = match client.max_log().await {
        Ok(m) => m,
        Err(e) => {
            report.error(format!("qbox_get_max_log 失败: {e}"));
            *stats.last_error.lock().unwrap() = report.errors.last().cloned();
            return report;
        }
    };
    report.max_log = max.max_log;
    report.global_notify = max.global_notify;
    if cursors.should_reset(max.max_log) {
        report.note(format!(
            "max_log 回退（{} → {}）→ 游标归零 + 全量重扫",
            cursors.max_log_seen, max.max_log
        ));
        cursors.reset();
    }
    cursors.max_log_seen = max.max_log;

    // ---- ② notify log：文件变更事件（先处理再推进游标）
    let mut lower = cursors.notify;
    while (lower.max(0) as u64) < max.max_log {
        match client.sync_log(lower, cfg.batch, None).await {
            Ok(batch) => {
                if batch.events.is_empty() {
                    report.note(format!("qbox_get_sync_log lower={lower} 没有新事件"));
                    break;
                }
                report.events += batch.events.len() as u64;
                stats
                    .events
                    .fetch_add(batch.events.len() as u64, Ordering::Relaxed);
                let mut uids: Vec<String> = Vec::new();
                for ev in &batch.events {
                    if let Some(u) = &ev.device_uid {
                        if !uids.iter().any(|x| x == u) {
                            uids.push(u.clone());
                        }
                    }
                    apply_event(client, views, ev, cfg, user, &mut baseline, &mut report).await;
                }
                stats.record_devices(&uids);
                for u in uids {
                    if !report.devices.contains(&u) {
                        report.devices.push(u);
                    }
                }
                let next = batch.next_lower(lower);
                if next <= lower {
                    report.note("事件游标无法推进（log_id 异常）");
                    break;
                }
                lower = next;
                cursors.notify = lower;
                // ★ 先处理完这一批，再把游标落盘
                persist(store, &cursors, &baseline);
                if batch.end == 1 || batch.events.len() < cfg.batch {
                    break;
                }
            }
            Err(e) if is_log_missing(&e) => {
                cursors.log_missing_count += 1;
                report.note(format!(
                    "qbox_get_sync_log lower={lower} 返回 -17（区间内没有事件，第 {} 次）",
                    cursors.log_missing_count
                ));
                break;
            }
            Err(e) => {
                report.error(format!("qbox_get_sync_log lower={lower} 失败: {e}"));
                break;
            }
        }
    }

    // ---- ③ config log / global notify log：M2c 只取 + 计数 + 推进游标
    if max.max_log > cursors.config.max(0) as u64 {
        let l = cursors.config.max(0) as u64;
        match client.device_config_list(user, l, max.max_log).await {
            Ok(b) => {
                stats.record_devices(&b.device_uids());
                if !b.is_empty() {
                    report.note(format!(
                        "config log {} 项（M2c 不消费，仅推进游标）",
                        b.len()
                    ));
                }
                cursors.config = max.max_log as i64;
            }
            Err(e) if is_log_missing(&e) => cursors.config = max.max_log as i64,
            Err(e) => report.error(format!("qbox_get_device_config_list 失败: {e}")),
        }
    }
    if max.global_notify > cursors.global_notify.max(0) as u64 {
        let l = cursors.global_notify.max(0) as u64;
        match client.query_notify(l, max.global_notify).await {
            Ok(b) => {
                stats.record_devices(&b.device_uids());
                if !b.is_empty() {
                    report.note(format!(
                        "global notify {} 项（共享/团队事件，M2c 不消费）",
                        b.len()
                    ));
                }
                cursors.global_notify = max.global_notify as i64;
            }
            Err(e) if is_log_missing(&e) => cursors.global_notify = max.global_notify as i64,
            Err(e) => report.error(format!("qbox_query_notify 失败: {e}")),
        }
    }

    // ---- ④ baseline 对账（兜底：我们自己的写操作不产生事件）
    for view in views {
        reconcile_view(client, view, cfg, &mut baseline, &mut report).await;
    }

    report.baseline_entries = baseline.len();
    persist(store, &cursors, &baseline);

    // 计数器
    stats
        .refreshed
        .fetch_add(report.refreshed, Ordering::Relaxed);
    stats.uploaded.fetch_add(report.uploaded, Ordering::Relaxed);
    stats
        .conflicts
        .fetch_add(report.conflicts, Ordering::Relaxed);
    stats.deleted.fetch_add(report.deleted, Ordering::Relaxed);
    stats
        .deletes_blocked
        .fetch_add(report.deletes_blocked, Ordering::Relaxed);
    if let Some(e) = report.errors.last() {
        *stats.last_error.lock().unwrap() = Some(e.clone());
    }
    if !report.notes.is_empty() {
        *stats.last_note.lock().unwrap() = Some(report.notes.join("；"));
    }
    {
        let mut g = stats.delete_block_reason.lock().unwrap();
        if report.deletes_blocked > 0 {
            *g = Some(format!(
                "本轮有 {} 项删除被保护挡住",
                report.deletes_blocked
            ));
        }
    }
    tracing::info!(
        "同步轮询: max_log={} events={} refreshed={} uploaded={} conflicts={} deleted={} blocked={} baseline={} dirs={}",
        report.max_log,
        report.events,
        report.refreshed,
        report.uploaded,
        report.conflicts,
        report.deleted,
        report.deletes_blocked,
        report.baseline_entries,
        report.dirs_scanned
    );
    report
}

/// 把内存状态写回 store（**游标 + baseline 同一个事务**；失败只记录不 panic）。
fn persist(store: &Mutex<SyncState>, cursors: &Cursors, baseline: &Baseline) {
    let mut g = store.lock().unwrap();
    g.cursors = *cursors;
    g.baseline = baseline.clone();
    if let Err(e) = g.save_all() {
        tracing::warn!("状态落盘失败（游标 + baseline 同一事务）: {e}");
    }
}

// ---------------------------------------------------------------- 事件

async fn apply_event(
    client: &Client,
    views: &[MountView],
    ev: &qxync_core::sync::SyncEvent,
    cfg: &SyncConfig,
    user: &str,
    baseline: &mut Baseline,
    report: &mut SyncReport,
) {
    if !ev.has_usable_path() {
        // 真机实测：删除事件的 filepath 是空的 → 只能靠对账发现
        report.events_skipped += 1;
        return;
    }
    if let Some(uid) = &ev.device_uid {
        if cfg.own_devices.iter().any(|u| u == uid) {
            report.events_skipped += 1;
            return;
        }
    }
    // 归属校验：事件带了 user 就必须和本会话一致（报告 11 §5.3）
    if let Some(u) = &ev.user {
        if !u.is_empty() && u != user {
            report.events_skipped += 1;
            return;
        }
    }
    for view in views {
        let Some(path) = map_event_path(&ev.filepath, &user, &view.remote_root) else {
            continue;
        };
        if path == view.remote_root {
            continue;
        }
        let remote = match stat_path(client, &path).await {
            Ok(s) => s,
            Err(e) => {
                report.error(format!("事件 stat {path} 失败: {e}"));
                return;
            }
        };
        let local = local_sig(view, &path);
        let base = baseline.get(&path);
        let d = decide(&local, &base, &remote);
        apply_decision(client, view, &path, d, remote, baseline, report).await;
        return;
    }
    report.events_skipped += 1;
}

/// `stat` 一个完整远端路径 → 签名（不存在 → `Sig::MISSING`）。
async fn stat_path(client: &Client, path: &str) -> Result<Sig, CoreError> {
    let (dir, name) = split_path(path);
    match client.stat(&dir, &name).await? {
        Some(e) => Ok(Sig::from_entry(&e)),
        None => Ok(Sig::MISSING),
    }
}

// ---------------------------------------------------------------- 对账

async fn reconcile_view(
    client: &Client,
    view: &MountView,
    cfg: &SyncConfig,
    baseline: &mut Baseline,
    report: &mut SyncReport,
) {
    tracing::debug!(
        "对账视图 {}（远端根 {}）",
        view.mountpoint.display(),
        view.remote_root
    );
    let root = view.remote_root.trim_end_matches('/').to_string();
    let under_root = |p: &str| p == root || p.starts_with(&format!("{root}/"));

    // 要对账的目录：挂载视图里已知的目录 + baseline 里出现过的目录
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    for d in view.view.known_dirs() {
        if under_root(&d) && d != root {
            dirs.insert(d);
        }
    }
    for p in baseline.entries.keys() {
        if under_root(p) && p != &root {
            dirs.insert(parent_dir(p));
        }
    }
    dirs.insert(root.clone());

    // 候选路径：本地节点 + baseline 条目
    let mut candidates: BTreeSet<String> = BTreeSet::new();
    for n in view.view.nodes() {
        if under_root(&n.remote) && n.remote != root {
            candidates.insert(n.remote);
        }
    }
    for p in baseline.entries.keys() {
        if under_root(p) && p != &root {
            candidates.insert(p.clone());
        }
    }

    // 列每个目录 → 远端子项签名表
    let mut remote_map: BTreeMap<String, Sig> = BTreeMap::new();
    let mut scanned = 0usize;
    for dir in dirs.iter().take(cfg.max_dirs_per_poll) {
        match client.list(dir).await {
            Ok(entries) => {
                scanned += 1;
                for e in &entries {
                    remote_map.insert(join(dir, &e.filename), Sig::from_entry(e));
                }
            }
            Err(e) if is_not_found(&e) => {
                // 目录本身没了 → 里面的候选在 remote_map 里缺失，自然按「远端不存在」处理
                tracing::debug!("对账：目录已不存在 {dir}");
            }
            Err(e) => report.error(format!("对账列举 {dir} 失败: {e}")),
        }
    }
    report.dirs_scanned += scanned;
    if dirs.len() > cfg.max_dirs_per_poll {
        report.note(format!(
            "目录数 {} 超过单轮上限 {}，本轮只对账了前 {}",
            dirs.len(),
            cfg.max_dirs_per_poll,
            cfg.max_dirs_per_poll
        ));
    }

    let mut deletes: Vec<String> = Vec::new();
    for path in &candidates {
        let remote = remote_map.get(path).copied().unwrap_or(Sig::MISSING);
        let local = local_sig(view, path);
        let base = baseline.get(path);
        let d = decide(&local, &base, &remote);
        match d {
            Decision::DeleteLocal => deletes.push(path.clone()),
            _ => {
                apply_decision(client, view, path, d, remote, baseline, report).await;
            }
        }
    }

    // 远端新出现的未知子项：登记 baseline（占位符天然「已同步」），下次不再重复处理
    for (path, sig) in &remote_map {
        if candidates.contains(path) || baseline.get(path).exists {
            continue;
        }
        baseline.put(path.clone(), *sig);
    }
    // ★ 删除保护：一次对账的删除量超过阈值就整批挡住
    if !deletes.is_empty() {
        let check = cfg.delete_protection.check(deletes.len(), baseline.len());
        if let Err(reason) = check {
            if cfg.force_deletes {
                report.note(format!("删除保护被 --force-deletes 放行：{reason}"));
            } else {
                report.deletes_blocked += deletes.len() as u64;
                report.note(format!("删除保护熔断（{} 项）：{reason}", deletes.len()));
                tracing::warn!("删除保护熔断（{} 项）：{reason}", deletes.len());
                return;
            }
        }
        for path in &deletes {
            if view.view.remove_remote(path) {
                report.deleted += 1;
                tracing::info!("远端已删除 {path} → 本地节点移除");
            }
            remove_baseline_tree(baseline, path);
        }
        report.note(format!("删除 {} 项", deletes.len()));
    }
}

/// 执行非删除类决策。
async fn apply_decision(
    client: &Client,
    view: &MountView,
    path: &str,
    d: Decision,
    remote: Sig,
    baseline: &mut Baseline,
    report: &mut SyncReport,
) {
    match d {
        Decision::Noop => {}
        Decision::AdoptBaseline => {
            baseline.put(path, remote);
        }
        Decision::RefreshRemote => {
            if view
                .view
                .apply_remote_meta(path, remote.is_dir, remote.size, remote.mtime)
            {
                report.refreshed += 1;
                tracing::info!(
                    "远端变更 {path}（{} 字节）→ 元数据已刷新、缓存已失效",
                    remote.size
                );
            }
            baseline.put(path, remote);
        }
        Decision::UploadLocal | Decision::RecreateRemote => {
            if view.view.has_pending(path) {
                // 已经在队列里了（每轮重复入队只会白写标记）
                return;
            }
            match view.view.mark_dirty(path) {
                Ok(()) => {
                    report.uploaded += 1;
                    if d == Decision::RecreateRemote {
                        report.note(format!("{path}: 远端已删但本地有改动 → 重新上传"));
                    }
                }
                Err(e) => report.error(format!("{path} 重新入队上传失败: {e}")),
            }
        }
        Decision::Conflict => {
            if view.read_only {
                // 只读挂载没有本地改动可保 → 以远端为准（不产生副本）
                view.view
                    .apply_remote_meta(path, remote.is_dir, remote.size, remote.mtime);
                baseline.put(path, remote);
                return;
            }
            resolve_conflict(client, view, path, remote, baseline, report).await;
        }
        Decision::DeleteLocal => {
            // 调用方统一做删除保护，这里不应到达
        }
    }
}

/// 冲突：远端内容占原名，本地内容另存冲突副本并上传（双方都不丢）。
async fn resolve_conflict(
    client: &Client,
    view: &MountView,
    path: &str,
    mut remote: Sig,
    baseline: &mut Baseline,
    report: &mut SyncReport,
) {
    // ★ 实测踩过：本地 4 MiB 改动还在上传时判定冲突，冲突副本还没传完，
    //   在途的「原名上传」就把远端改回了本地内容 —— 冲突副本白做、远端变更丢失。
    //   所以先取消待上传作业并等在途作业结束，再重新取远端签名重判。
    if let Some(q) = &view.upload {
        if q.has_pending(path) || q.is_active() {
            q.cancel(path);
            if !q.drain(Duration::from_secs(30)) {
                report.error(format!(
                    "{path}: 等待在途上传结束超时，冲突处理推迟到下一轮"
                ));
                return;
            }
            let r2 = match stat_path(client, path).await {
                Ok(s) => s,
                Err(e) => {
                    report.error(format!("冲突前重新 stat {path} 失败: {e}"));
                    return;
                }
            };
            let local2 = local_sig(view, path);
            let base2 = baseline.get(path);
            let d2 = decide(&local2, &base2, &r2);
            if d2 != Decision::Conflict {
                report.note(format!(
                    "{path}: 在途上传已落地/远端又变 → 冲突消解（{d2:?}）"
                ));
                match d2 {
                    Decision::Noop => {}
                    Decision::AdoptBaseline => {
                        baseline.put(path, r2);
                    }
                    Decision::RefreshRemote => {
                        view.view
                            .apply_remote_meta(path, r2.is_dir, r2.size, r2.mtime);
                        baseline.put(path, r2);
                        report.refreshed += 1;
                    }
                    Decision::UploadLocal | Decision::RecreateRemote => {
                        if view.view.mark_dirty(path).is_ok() {
                            report.uploaded += 1;
                        }
                    }
                    // 删除类决策留给调用方的删除保护逻辑，本轮不动
                    Decision::DeleteLocal | Decision::Conflict => {}
                }
                return;
            }
            remote = r2;
        }
    }

    let Some(local) = view.view.node(path) else {
        // 本地已经没有这个节点 → 当成普通远端刷新
        view.view
            .apply_remote_meta(path, remote.is_dir, remote.size, remote.mtime);
        baseline.put(path, remote);
        return;
    };
    if local.is_dir {
        view.view
            .apply_remote_meta(path, remote.is_dir, remote.size, remote.mtime);
        baseline.put(path, remote);
        return;
    }
    let (dir, name) = split_path(path);
    let device = hostname();
    let date = today();
    let mut chosen = conflict_name(&name, &device, &date);
    for seq in 2..=20u32 {
        match client.stat(&dir, &chosen).await {
            Ok(None) => break,
            Ok(Some(_)) => chosen = conflict_name_with_seq(&name, &device, &date, seq),
            Err(e) => {
                report.error(format!("冲突副本取名失败（stat {dir}/{chosen}）: {e}"));
                return;
            }
        }
    }

    // ① 先把本地内容复制到 stash（下一步失效缓存会把它删掉）
    let stash = match view.view.stash_conflict(path, &chosen) {
        Ok(p) => p,
        Err(e) => {
            report.error(format!("{path} 冲突副本落盘失败: {e}"));
            return;
        }
    };
    // ② 冲突副本入队上传
    if let Err(e) = view.view.enqueue_upload(&dir, &chosen, stash, local.mtime) {
        report.error(format!("冲突副本 {dir}/{chosen} 入队失败: {e}"));
        return;
    }
    // ③ 原始路径以远端为准：刷新元数据 + 丢弃本地内容
    view.view
        .apply_remote_meta(path, remote.is_dir, remote.size, remote.mtime);
    view.view.invalidate_content(path);
    baseline.put(path, remote);
    report.conflicts += 1;
    let conflict_remote = join(&dir, &chosen);
    tracing::warn!("冲突：{path} 远端/本地都改了 → 本地内容存为 {conflict_remote}");
    report.note(format!(
        "冲突副本 {conflict_remote}（远端占原名，双方都保留）"
    ));
    if let Err(e) = client
        .write_log(&conflict_remote, write_action::UPSERT_FILE)
        .await
    {
        tracing::debug!("冲突副本 write_log 失败（不影响）: {e}");
    }
}

// ---------------------------------------------------------------- 工具

fn local_sig(view: &MountView, path: &str) -> LocalSig {
    match view.view.node(path) {
        Some(n) => LocalSig {
            sig: Sig {
                exists: true,
                is_dir: n.is_dir,
                size: n.size,
                mtime: n.mtime,
            },
            dirty: n.dirty || view.view.has_pending(path),
        },
        None => LocalSig {
            sig: Sig::MISSING,
            dirty: false,
        },
    }
}

/// 远端条目「不存在」的判定：只有这几个 status 才是真不存在；
/// 其它错误（网络/权限）绝不能当成删除，否则会误删本地。
fn is_not_found(e: &CoreError) -> bool {
    matches!(e, CoreError::Status { status, .. } if matches!(status.0, 4 | 5 | 6))
}

fn remove_baseline_tree(baseline: &mut Baseline, path: &str) {
    baseline.remove(path);
    let prefix = format!("{}/", path.trim_end_matches('/'));
    let victims: Vec<String> = baseline
        .entries
        .keys()
        .filter(|k| k.starts_with(&prefix))
        .cloned()
        .collect();
    for v in victims {
        baseline.remove(&v);
    }
}

fn split_path(path: &str) -> (String, String) {
    match path.rfind('/') {
        Some(i) if i > 0 => (path[..i].to_string(), path[i + 1..].to_string()),
        _ => ("/".to_string(), path.trim_start_matches('/').to_string()),
    }
}

fn parent_dir(path: &str) -> String {
    split_path(path).0
}

fn join(dir: &str, name: &str) -> String {
    format!("{}/{}", dir.trim_end_matches('/'), name)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/proc/sys/kernel/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "qsync-linux".to_string())
}

/// `YYYY-MM-DD`（本地时区，用 libc 拆 tm）。
pub fn today() -> String {
    let secs = now_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    format!(
        "{:04}-{:02}-{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday
    )
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;
    use qxync_core::{LinkConfig, HOME_ROOT};
    use qxync_fuse::upload::UploadQueue;
    use qxync_fuse::LocalNode;
    use std::collections::BTreeMap;
    use std::time::Duration;

    // ---- 假本地视图：把 LocalView 的语义用内存 map 复现，上传走真实的 UploadQueue

    struct FakeLocal {
        root: String,
        nodes: Mutex<BTreeMap<String, LocalNode>>,
        queue: Mutex<Option<Arc<UploadQueue>>>,
        stash_dir: PathBuf,
    }

    impl FakeLocal {
        fn new(root: &str, stash_dir: PathBuf) -> Arc<Self> {
            Arc::new(Self {
                root: root.to_string(),
                nodes: Mutex::new(BTreeMap::new()),
                queue: Mutex::new(None),
                stash_dir,
            })
        }

        fn set_queue(&self, q: Arc<UploadQueue>) {
            *self.queue.lock().unwrap() = Some(q);
        }

        fn add_file(
            &self,
            remote: &str,
            size: u64,
            mtime: i64,
            dirty: bool,
            cache: Option<PathBuf>,
        ) {
            let name = remote.rsplit('/').next().unwrap_or("").to_string();
            self.nodes.lock().unwrap().insert(
                remote.to_string(),
                LocalNode {
                    remote: remote.to_string(),
                    name,
                    is_dir: false,
                    size,
                    mtime,
                    dirty,
                    cache,
                },
            );
        }
    }

    impl LocalView for FakeLocal {
        fn remote_root(&self) -> &str {
            &self.root
        }
        fn node(&self, remote: &str) -> Option<LocalNode> {
            self.nodes.lock().unwrap().get(remote).cloned()
        }
        fn nodes(&self) -> Vec<LocalNode> {
            self.nodes.lock().unwrap().values().cloned().collect()
        }
        fn known_dirs(&self) -> Vec<String> {
            let mut v: Vec<String> = vec![self.root.clone()];
            v.extend(
                self.nodes
                    .lock()
                    .unwrap()
                    .values()
                    .filter(|n| n.is_dir)
                    .map(|n| n.remote.clone()),
            );
            v
        }
        fn has_pending(&self, remote: &str) -> bool {
            self.queue
                .lock()
                .unwrap()
                .as_ref()
                .map(|q| q.has_pending(remote))
                .unwrap_or(false)
        }
        fn apply_remote_meta(&self, remote: &str, is_dir: bool, size: u64, mtime: i64) -> bool {
            let mut g = self.nodes.lock().unwrap();
            let Some(n) = g.get_mut(remote) else {
                return false;
            };
            let changed = n.size != size || n.mtime != mtime || n.is_dir != is_dir;
            n.size = if is_dir { 0 } else { size };
            n.mtime = mtime;
            n.is_dir = is_dir;
            if changed {
                n.cache = None;
            }
            true
        }
        fn invalidate_content(&self, remote: &str) -> bool {
            let mut g = self.nodes.lock().unwrap();
            match g.get_mut(remote) {
                Some(n) => {
                    n.cache = None;
                    n.dirty = false;
                    true
                }
                None => false,
            }
        }
        fn remove_remote(&self, remote: &str) -> bool {
            let prefix = format!("{}/", remote.trim_end_matches('/'));
            let before = self.nodes.lock().unwrap().len();
            self.nodes
                .lock()
                .unwrap()
                .retain(|k, _| k != remote && !k.starts_with(&prefix));
            self.nodes.lock().unwrap().len() != before
        }
        fn mark_dirty(&self, remote: &str) -> std::io::Result<()> {
            let n = self
                .node(remote)
                .ok_or_else(|| std::io::Error::other("no node"))?;
            let (dir, name) = split_path(remote);
            let cache = n
                .cache
                .clone()
                .ok_or_else(|| std::io::Error::other("no cache"))?;
            {
                let mut g = self.nodes.lock().unwrap();
                if let Some(x) = g.get_mut(remote) {
                    x.dirty = true;
                }
            }
            self.enqueue_upload(&dir, &name, cache, n.mtime)
        }
        fn stash_conflict(&self, remote: &str, conflict_name: &str) -> std::io::Result<PathBuf> {
            let n = self
                .node(remote)
                .ok_or_else(|| std::io::Error::other("no node"))?;
            let src = n
                .cache
                .clone()
                .ok_or_else(|| std::io::Error::other("no cache"))?;
            std::fs::create_dir_all(&self.stash_dir)?;
            let dest = self.stash_dir.join(sanitize(conflict_name));
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
                .queue
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| std::io::Error::other("no queue"))?;
            q.enqueue(qxync_fuse::upload::UploadJob {
                remote_dir: remote_dir.to_string(),
                remote_name: remote_name.to_string(),
                local,
                mtime,
                attempts: 0,
                ephemeral: false,
            })
        }
    }

    fn sanitize(s: &str) -> String {
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

    // ---- 真机测试脚手架

    fn env_creds() -> Option<(LinkConfig, String)> {
        let host = std::env::var("QSYNC_TEST_HOST").ok()?;
        let user = std::env::var("QSYNC_TEST_USER").ok()?;
        let password = std::env::var("QSYNC_TEST_PASSWORD").ok()?;
        let port = std::env::var("QSYNC_TEST_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(9834);
        Some((
            LinkConfig {
                id: "m2c-test".into(),
                host,
                port,
                https: true,
                insecure: true,
                user,
                home_root: HOME_ROOT.to_string(),
                roots: Vec::new(),
                ipv4_only: std::env::var("QSYNC_TEST_IPV4").is_ok(),
            },
            password,
        ))
    }

    fn fixture_root() -> String {
        std::env::var("QSYNC_TEST_FIXTURE").unwrap_or_else(|_| "/home/qxync-test".to_string())
    }

    async fn logged_in() -> Option<Client> {
        let (link, pw) = env_creds()?;
        let mut c = Client::new(&link).expect("构造 client");
        c.login(&link.user, &pw).await.expect("登录失败");
        Some(c)
    }

    async fn ensure_dir(client: &Client, parent: &str, name: &str) {
        if client.stat(parent, name).await.ok().flatten().is_none() {
            let _ = client.mkdir(parent, name).await;
        }
    }

    async fn put(client: &Client, dir: &str, name: &str, bytes: &[u8], mtime: i64) {
        client
            .upload_bytes(dir, name, bytes.to_vec())
            .await
            .unwrap_or_else(|e| panic!("上传 {dir}/{name} 失败: {e}"));
        client
            .set_mtime(dir, name, mtime)
            .await
            .unwrap_or_else(|e| panic!("settime {dir}/{name} 失败: {e}"));
    }

    async fn get(client: &Client, dir: &str, name: &str) -> Vec<u8> {
        let dest = std::env::temp_dir().join(format!("qxync-m2c-get-{}", name));
        client
            .download_to_file(dir, name, &dest)
            .await
            .unwrap_or_else(|e| panic!("下载 {dir}/{name} 失败: {e}"));
        let b = std::fs::read(&dest).unwrap();
        let _ = std::fs::remove_file(&dest);
        b
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "qxync-m2c-{tag}-{}-{}",
            std::process::id(),
            now_secs()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// ★ 真机 M2c 引擎测试：远端刷新 / 冲突副本 / baseline / 删除保护 / 游标落盘。
    ///
    /// ```bash
    /// export QSYNC_TEST_HOST=... QSYNC_TEST_USER=... QSYNC_TEST_PASSWORD=...
    /// cargo test -p qxync-daemon -- --ignored --nocapture
    /// ```
    // 必须是多线程运行时：上传 worker 是独立 OS 线程，用 `Handle::block_on` 驱动请求，
    // 单线程（current_thread）运行时会 panic（`Tokio 1.x context ... being shutdown`）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "需要真机 NAS"]
    async fn m2c_engine_refresh_conflict_delete_protection() {
        let Some(client) = logged_in().await else {
            return;
        };
        let fixture = fixture_root();
        let (fx_dir, fx_name) = split_path(&fixture);
        ensure_dir(&client, &fx_dir, &fx_name).await;
        ensure_dir(&client, &fixture, "m2c").await;
        let wd = format!("{fixture}/m2c");

        // 清掉上次残留
        if let Ok(entries) = client.list(&wd).await {
            for e in entries {
                let _ = client.delete_entry(&wd, &e.filename).await;
            }
        }

        let work = tmpdir("engine");
        let stash = work.join("conflicts");
        let marker = work.join("queue");
        let view = FakeLocal::new(HOME_ROOT, stash.clone());
        let client = Arc::new(client);
        let q =
            UploadQueue::new(client.clone(), tokio::runtime::Handle::current(), marker).unwrap();
        q.spawn_worker().unwrap();
        view.set_queue(q.clone());

        let store = Mutex::new(SyncState::load(work.join("state")).unwrap());
        let stats = SyncStats::default();
        let views = vec![MountView {
            mountpoint: PathBuf::from("/tmp/m2c-mnt"),
            remote_root: HOME_ROOT.to_string(),
            view: view.clone(),
            upload: Some(q.clone()),
            read_only: false,
        }];
        let cfg = SyncConfig::default();

        // ---- ① 远端刷新：远端改了、本地没改 → 元数据刷新 + 缓存失效
        let refresh = format!("{wd}/refresh.txt");
        put(&client, &wd, "refresh.txt", b"DDDDDDDD", 1000).await;
        {
            let mut g = store.lock().unwrap();
            g.baseline.put(refresh.clone(), Sig::file(8, 1000));
        }
        view.add_file(&refresh, 8, 1000, false, None);
        put(&client, &wd, "refresh.txt", b"DDDDDDDDDDDD", 2000).await;
        let r = poll_once(&client, &views, &store, &cfg, &stats, "test1").await;
        println!("① 刷新: {r:?}");
        assert!(r.errors.is_empty(), "不该有错误: {:?}", r.errors);
        assert_eq!(r.refreshed, 1, "应识别到 1 个远端刷新");
        let n = view.node(&refresh).unwrap();
        assert_eq!((n.size, n.mtime), (12, 2000), "远端元数据必须刷进视图");
        assert_eq!(
            store.lock().unwrap().baseline.get(&refresh),
            Sig::file(12, 2000),
            "baseline 必须采纳远端签名"
        );

        // ---- ② 冲突副本：远端与本地都改了 → 远端占原名，本地存副本并上传
        let conflict = format!("{wd}/conflict.txt");
        let local_bytes = b"LOCAL-LOCAL-LOCAL".to_vec(); // 17B
        let remote_bytes = b"REMOTE".to_vec(); // 6B
        put(&client, &wd, "conflict.txt", b"BASE-BASE", 1500).await;
        {
            let mut g = store.lock().unwrap();
            g.baseline.put(conflict.clone(), Sig::file(9, 1500));
        }
        // 本地：内容 17B、mtime 2500、脏，缓存文件里是本地内容
        let local_cache = work.join("conflict.local");
        std::fs::write(&local_cache, &local_bytes).unwrap();
        view.add_file(&conflict, 17, 2500, true, Some(local_cache.clone()));
        // 远端：别的设备改成了 6B / mtime 3500
        put(&client, &wd, "conflict.txt", &remote_bytes, 3500).await;

        let r = poll_once(&client, &views, &store, &cfg, &stats, "test1").await;
        println!("② 冲突: {r:?}");
        assert!(r.errors.is_empty(), "不该有错误: {:?}", r.errors);
        assert_eq!(r.conflicts, 1, "应产生 1 个冲突副本");
        assert!(q.drain(Duration::from_secs(30)), "冲突副本应上传完成");

        // 原名保持远端内容
        assert_eq!(get(&client, &wd, "conflict.txt").await, remote_bytes);
        // 副本存在且是本地内容
        let entries = client.list(&wd).await.unwrap();
        let copy = entries
            .iter()
            .find(|e| e.filename.contains("conflicted copy"))
            .map(|e| e.filename.clone())
            .expect("远端应出现冲突副本");
        println!("   冲突副本: {copy}");
        assert!(
            copy.contains("conflicted copy from"),
            "命名应带设备与日期: {copy}"
        );
        assert_eq!(
            get(&client, &wd, &copy).await,
            local_bytes,
            "副本必须是本地内容"
        );
        // 原路径 baseline = 远端；本地视图已失效并以远端为准
        assert_eq!(
            store.lock().unwrap().baseline.get(&conflict),
            Sig::file(6, 3500)
        );
        let n = view.node(&conflict).unwrap();
        assert_eq!((n.size, n.mtime, n.dirty), (6, 3500, false));

        // ---- ③ 删除保护：远端批量删除 > 阈值 → 挡住；--force-deletes 才放行
        ensure_dir(&client, &wd, "del").await;
        let deldir = format!("{wd}/del");
        let mut names = Vec::new();
        for i in 0..6 {
            let name = format!("d{i}.txt");
            put(&client, &deldir, &name, b"x", 1000 + i).await;
            view.add_file(&format!("{deldir}/{name}"), 1, 1000 + i, false, None);
            names.push(name);
        }
        // baseline 记录这 6 个
        {
            let mut g = store.lock().unwrap();
            for (i, name) in names.iter().enumerate() {
                g.baseline
                    .put(format!("{deldir}/{name}"), Sig::file(1, 1000 + i as i64));
            }
        }
        // 远端删掉 5 个
        for name in names.iter().take(5) {
            client.delete_entry(&deldir, name).await.unwrap();
        }
        let strict = SyncConfig {
            delete_protection: DeleteProtection {
                max_entries: 2,
                max_ratio: 1.0,
                min_entries_for_ratio: 1000,
            },
            ..cfg.clone()
        };
        let r = poll_once(&client, &views, &store, &strict, &stats, "test1").await;
        println!("③ 删除保护: {r:?}");
        assert_eq!(r.deletes_blocked, 5, "超阈值必须整批挡住");
        assert_eq!(r.deleted, 0);
        assert!(
            view.node(&format!("{deldir}/d0.txt")).is_some(),
            "被挡住时本地节点不能删"
        );
        assert!(
            store
                .lock()
                .unwrap()
                .baseline
                .get(&format!("{deldir}/d0.txt"))
                .exists,
            "被挡住时 baseline 不能清"
        );

        // --force-deletes 放行
        let forced = SyncConfig {
            force_deletes: true,
            ..strict.clone()
        };
        let r = poll_once(&client, &views, &store, &forced, &stats, "test1").await;
        println!("③' 强制放行: {r:?}");
        assert_eq!(r.deleted, 5, "放行后应删掉 5 项");
        assert!(view.node(&format!("{deldir}/d0.txt")).is_none());
        assert!(
            view.node(&format!("{deldir}/d5.txt")).is_some(),
            "剩下的不能误删"
        );

        // ---- ④ 游标/baseline 落盘（★ M5：SQLite，游标 + baseline 同一事务）+ max_log 观测
        {
            let g = store.lock().unwrap();
            g.save_all().unwrap();
            assert!(g.db_path().exists(), "状态库必须存在: {}", g.db_path().display());
            assert_eq!(g.store.integrity_check().unwrap(), "ok", "integrity_check 必须是 ok");
            assert_eq!(
                g.store.baseline_len().unwrap() as usize,
                g.baseline.len(),
                "库里的 baseline 行数必须和内存一致"
            );
            let back = g.store.cursors().unwrap();
            assert!(back.max_log_seen >= 1, "max_log 必须落盘: {back:?}");
            let max = client.max_log().await.unwrap();
            println!(
                "④ 游标: notify={} config={} global={} max_log_seen={}（服务端 max_log={}, global_notify={}）",
                back.notify, back.config, back.global_notify, back.max_log_seen, max.max_log, max.global_notify
            );
            assert!(
                back.max_log_seen <= max.max_log,
                "游标不能超过服务端 max_log"
            );
        }

        // ---- 清理
        for e in client.list(&wd).await.unwrap_or_default() {
            let _ = client.delete_entry(&wd, &e.filename).await;
        }
        let _ = client.delete_entry(&fixture, "m2c").await;
        q.shutdown();
        let _ = std::fs::remove_dir_all(&work);
    }
}
