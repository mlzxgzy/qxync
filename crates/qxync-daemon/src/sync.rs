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
use qxync_core::rules::{HideReason, Rules};
use qxync_core::store::{Store, DB_FILE};
use qxync_core::sync::{
    apply_renames_to_baseline, conflict_name, conflict_name_with_seq, decide, is_log_missing,
    map_event_path, pair_renames, Baseline, Cursors, Decision, DeleteProtection, LocalSig,
    RenameCandidate, Sig, DEFAULT_LOG_BATCH,
};
use qxync_core::tasks::{
    CONFLICT_ASK, CONFLICT_RENAME_REMOTE, CONFLICT_REPLACE_LOCAL, CONFLICT_REPLACE_REMOTE,
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
    /// ★ M7：选择性同步规则（排除 / 临时文件）。默认「只有临时过滤」，向后兼容。
    pub rules: Arc<Rules>,
    /// ★ M8.4：冲突策略（`tasks::CONFLICTS` 的 5 个取值）。
    /// 默认 `rename_local` = M2c 的硬编码行为（远端占原名、本地另存副本）。
    pub conflict: String,
}

impl MountView {
    /// 该路径是否**不该被同步引擎管理**（对账/事件都要跳过）。
    ///
    /// 两层判据，都在这里收口（调用点只认 `is_some()`）：
    ///
    /// 1. ★ M11+：**回收站子树**（`.Trash` / `.Trash-<uid>` / `@Recycle`）——
    ///    段级精确匹配，与 FUSE 的 `deny_trash` 同一套语义（判定在
    ///    [`qxync_core::trash`]，这里有同样的单测）。不管的后果是真机上
    ///    baseline 71% 被 `@Recycle` 占满、QTS 清回收站时制造批量幽灵删除、
    ///    FDO 的 `.trashinfo` 被反向同步上 NAS。
    /// 2. ★ M7：用户规则（选择性同步排除 / 临时文件）。
    pub fn hidden(&self, remote: &str, is_dir: bool) -> Option<HideReason> {
        // 卷的根目录本身是挂载视图，不参与判定（与 FUSE 层一致）。
        if remote == self.remote_root {
            return None;
        }
        // ★ 回收站：硬约束，优先于用户规则（用户写 `!@Recycle` 也不能把它放回来）。
        if qxync_core::trash::is_trash_path(remote, local_uid()) {
            return Some(HideReason::Trash);
        }
        let roots = self.view.remote_roots();
        self.rules.hides_in_roots(&roots, remote, is_dir)
    }
}

/// 本进程的 uid（与 FUSE 侧 `deny_trash` 的 `trash_uid` 同口径）。
///
/// 回收站目录名里带的是「创建者 uid」（`.Trash-1000`）。FUSE 层用的是
/// `libc::getuid()`；同步引擎必须用同一个数，否则会出现
/// 「FUSE 拒绝操作、引擎却还在同步」的语义分叉。这里**不把它做成配置项**：
/// 它不是策略，只是同一个进程的既定事实。
fn local_uid() -> u32 {
    unsafe { libc::getuid() }
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

/// 一轮的统计（`qxync sync --once` 直接打印）。
#[derive(Debug, Clone, Default)]
pub struct SyncReport {
    pub max_log: u64,
    pub global_notify: u64,
    pub events: u64,
    pub events_skipped: u64,
    pub refreshed: u64,
    /// ★ M9：本地有水、正在后台更新内容（baseline 等刷新落地再推进）的项数。
    pub content_updating: u64,
    pub uploaded: u64,
    pub conflicts: u64,
    /// ★ 待裁决队列巡检清掉的失效条目数（远端已删 / 挂载没了 / 策略已不是 ask）。
    pub decisions_swept: u64,
    pub deleted: u64,
    pub deletes_blocked: u64,
    /// 本轮触发删除熔断的**人话原因**（`deletes_blocked > 0` 时才有值）。
    ///
    /// 与 `deletes_blocked` 一起构成「熔断签名」：数量或原因变了才算「新告警」，
    /// 否则重复轮次只降级成 debug —— 真机实测每 30s 一轮、连续 13 小时刷屏。
    pub delete_block_reason: Option<String>,
    /// ★ 本轮熔断签名是否与上一轮**不同**（新出现 / 数量或原因变了 / 已解除）。
    ///
    /// 只有它为真时才写 WARN 与 journal（避免把日志和历史记录冲掉）；
    /// 解除时也置真，用来记一条「熔断已解除」。
    pub delete_block_changed: bool,
    /// ★ 本轮是否识别到**会话失效**（sid 过期）。
    ///
    /// `poll_once` 把错误塞进 `errors` 后仍返回 `Ok`，所以轮询循环的
    /// `Err(e) if e.is_auth()` 分支永远不触发 —— 真机实测过：sid 过期后每 30s
    /// 报一次 `qbox_get_max_log 失败: status=3`，直到 120s 会话保活重登才恢复。
    /// 有了这个标志，轮询循环看到 `Ok(report)` 也能**立即重登一次**。
    pub auth_failed: bool,
    /// ★ M15/T9：按 `file_id` 认出并迁移成功的远端改名次数。
    pub renamed: u64,
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
    /// 上一轮的**熔断签名**（`数量|原因`）。只有签名变化才重新 WARN / 记 journal，
    /// 否则真机每 30s 一条的重复告警会把 1 万条的 journal 上限几天内冲掉。
    pub delete_block_sig: Mutex<Option<String>>,
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
    /// 本轮轮询开始后，上传 success hook 有没有改过 baseline。
    ///
    /// 用途见 [`persist`]：没有并发写入时 baseline 可以**整份替换**（删除/改名迁移
    /// 才能真正落盘）；有并发写入时退回逐条合并（宁可删除晚一轮落盘，也不能把
    /// hook 刚推进的新签名抹掉）。
    pub hook_touched: bool,
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
            hook_touched: false,
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

    /// ★ 我们自己上传成功后，把 baseline 直接推进到刚写上去的那个签名。
    ///
    /// 为什么必须在这里做（而不是等下一轮轮询的 `AdoptBaseline`）：
    /// 上传落地到下一轮轮询之间有一个**最长 30s（`DEFAULT_POLL_INTERVAL_SECS`）的窗口**。
    /// 窗口内用户再保存一次并等上传完成，节点会被 `clear_dirty` 标成干净，
    /// 于是下一轮对账看到的是「本地 == 远端（都是新版本），baseline 还是老版本」：
    /// `rc = true`（远端变了）、`lc = false`（本地没标脏），走进 `(false, true)` 分支，
    /// 又因为本地签名 ≠ baseline 而被判成 **Conflict** —— 用户的连续两次保存
    /// 凭空多出一个冲突副本。`decide` 的 `(true, true)` 分支里那句
    /// 「远端 == 本地 → AdoptBaseline」要求 `dirty` 还为真才能命中，救不了这里。
    ///
    /// 幂等：重复推进同一个签名是 no-op（`put` 就是覆盖写）。
    pub fn note_uploaded(&mut self, path: &str, size: u64, mtime: i64) -> Result<()> {
        let sig = Sig::file(size, mtime);
        if self.baseline.get(path).same_as(&sig) {
            return Ok(());
        }
        self.baseline.put(path, sig);
        // ★ 告诉正在跑的那一轮「baseline 有并发写入」：它会退回落盘合并，
        //   不把我们刚写的签名覆盖回旧快照（见 [`persist`]）。
        self.hook_touched = true;
        self.save_all()
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
    link_id: &str,
    user: &str,
) -> SyncReport {
    // ★ 克隆快照的同时复位「hook 改过 baseline」标记（同一把锁，没有竞态窗口）：
    //   本轮期间 hook 若再写，标记会重新置起，persist 就退回逐条合并。
    //   本轮之前残留的标记直接忽略 —— hook 那些写入已经包含在这份快照里了。
    let (mut cursors, mut baseline) = {
        let mut g = store.lock().unwrap();
        let snapshot = (g.cursors, g.baseline.clone());
        g.hook_touched = false;
        snapshot
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
            // ★ 服务端未就绪（status=8）：等就好，不是错误，更不该重登。
            // 记进 notes 而不是 errors —— 它不该污染"同步出错"的计数和 last_error。
            if e.is_server_busy() {
                report.note(format!("服务端未就绪，稍后重试：{e}"));
                return report;
            }
            // ★ 会话失效：以前只记一条 error 就 Ok 返回，轮询循环收不到 Err，
            //   于是永远不重登 —— 真机实测每 30s 报一次，直到 120s 保活才恢复。
            if max_log_failure_is_session_loss(&e) {
                report.auth_failed = true;
            }
            report.error(format!("qbox_get_max_log 失败: {e}"));
            // 本轮没跑到对账 → completed=false：只更新「当前错误」，不清任何陈旧告警。
            absorb_round(stats, &mut report, false);
            return report;
        }
    };
    report.max_log = max.max_log;
    report.global_notify = max.global_notify;

    // ---- ①' 服务端在忙（迁移/恢复/备份还原/启动中）→ 本轮什么都不做，等下一轮
    //
    // 这四种状态下服务端数据尚在变动，此时做增量对账容易拿到半成品；
    // 官方客户端此时也是退避轮询而不是重登（逆向 §3.3 的 ERROR_CONNECTION_UNAVAIL → retry）。
    // 顺带避免一个具体误判：这四种状态下 max_log 可能短暂偏小，
    // 过去会被 `should_reset` 当成"游标回退"而白白触发一次全量重扫。
    if let Some(reason) = max.busy_reason() {
        report.note(format!("服务端就绪状态：{reason} → 本轮跳过，稍后重试"));
        return report;
    }

    // ★ `sync_signal == 2` = 服务端要求降速（附 `slowdown_seconds`）。
    // 先记下来给调度层看，别在忙碌状态下还照常轮询。
    if let Some(secs) = max.advised_interval_secs() {
        report.note(format!(
            "服务端要求降速（sync_signal=2）→ 建议下次轮询间隔 {secs}s"
        ));
    }

    if cursors.should_reset(max.max_log) {
        report.note(format!(
            "max_log 回退（{} → {}）→ 游标归零 + 全量重扫",
            cursors.max_log_seen, max.max_log
        ));
        cursors.reset();
    }
    cursors.max_log_seen = max.max_log;

    // ---- ② notify log：文件变更事件（先处理再推进游标）
    // ★ 用服务端自报的 `server_limit`（真机 256）夹住本轮批量，而不是写死 200：
    // 写死会在上限较低的机型上被服务端截断，导致"以为拉完了其实没拉完"。
    let batch_n = match max.server_limit {
        Some(limit) if limit > 0 => cfg.batch.min(limit as usize),
        _ => cfg.batch,
    };
    let mut lower = cursors.notify;
    while (lower.max(0) as u64) < max.max_log {
        match client.sync_log(lower, batch_n, None).await {
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
                    apply_event(
                        client,
                        views,
                        ev,
                        cfg,
                        user,
                        &mut baseline,
                        &mut report,
                        store,
                    )
                    .await;
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
                if e.is_auth() {
                    report.auth_failed = true;
                }
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
            Err(e) => {
                if e.is_auth() {
                    report.auth_failed = true;
                }
                report.error(format!("qbox_get_device_config_list 失败: {e}"));
            }
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
            Err(e) => {
                if e.is_auth() {
                    report.auth_failed = true;
                }
                report.error(format!("qbox_query_notify 失败: {e}"));
            }
        }
    }

    // ---- ④ baseline 对账（兜底：我们自己的写操作不产生事件）
    for view in views {
        reconcile_view(client, view, cfg, &mut baseline, &mut report, store).await;
    }

    // ---- ④' 冲突待裁决队列巡检（把已经不该留着的行清掉）
    sweep_decision_queue(client, views, &baseline, store, &mut report, link_id).await;

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
    // ★ 唯一一处把「一轮的结论」并入长期状态：last_error / last_note / 熔断告警。
    //   这里走到 = 本轮**完整跑完**（拉事件 + 对账 + 巡检），所以陈旧值可以清；
    //   提前 return 的轮次不会经过这里（除 max_log 失败那一支自己调 completed=false）。
    absorb_round(stats, &mut report, true);
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

/// ★ 把一轮的结论并入长期状态（`status` / GUI 告警 / journal 都读这里）。
///
/// ## 为什么必须集中在一处（真机事故）
///
/// 这些字段以前都是「只写不清」：`last_error` 只在有错时写、`delete_block_reason`
/// 只在熔断时写、`last_note` 只在有 note 时写。于是**问题修好之后界面永远显示红字**：
/// 用户机器上 `qxync status` 连续 13 小时显示「删除被挡 692742」，
/// 而实际每轮被挡的一直是同一批 526 项；错误也早就在 08:30 恢复了，
/// 「最近错误」却常驻首页/诊断。
///
/// ## `completed` 的语义（决定敢不敢清）
///
/// * `true` = 本轮真的跑完了全部对账/巡检 → 本轮没发现的问题就是**已经不在了**，
///   陈旧值可以清（`deletes_blocked==0` 时清熔断告警；零错误时清 last_error）。
/// * `false` = 这轮在 max_log 阶段就提前返回（会话失效 / 服务端未就绪），
///   **什么检查都没做** → 只更新「当前错误」，不清任何陈旧告警。
///   没检查过 ≠ 检查通过。
///
/// ## 熔断告警去重（真机事故 2）
///
/// 同一个熔断（数量 + 原因都不变）每 30s 重复一轮，以前每轮一条 WARN + 一条 journal
/// → 实测 journal 3046 条里 3023 条是同一句「526 项删除被熔断挡住」，1 万条上限几天
/// 就被冲掉、真实历史反而没了。现在只有**签名变化**才置
/// [`SyncReport::delete_block_changed`]（WARN / journal 据此只记一次），重复轮次降为 debug。
pub fn absorb_round(stats: &SyncStats, report: &mut SyncReport, completed: bool) {
    // 错误：本轮有错就更新（提前返回的轮次也一样 —— 用户要立刻看到最新的错）。
    if let Some(e) = report.errors.last() {
        *stats.last_error.lock().unwrap() = Some(e.clone());
    } else if completed {
        // 完整跑完且零错误 → 之前那条错误已经过时了。
        *stats.last_error.lock().unwrap() = None;
    }

    if !completed {
        return;
    }

    // note：完整轮次**整体替换**（本轮没有 note 就清空），否则上一轮的提示会常驻。
    *stats.last_note.lock().unwrap() = (!report.notes.is_empty()).then(|| report.notes.join("；"));

    // 删除熔断告警：按签名（数量+原因）去重；本轮检查通过就清掉旧告警。
    let mut slot = stats.delete_block_reason.lock().unwrap();
    if report.deletes_blocked > 0 {
        let reason = report.delete_block_reason.clone().unwrap_or_default();
        let sig = format!("{}|{}", report.deletes_blocked, reason);
        let changed = {
            let mut last = stats.delete_block_sig.lock().unwrap();
            if last.as_deref() == Some(sig.as_str()) {
                false
            } else {
                *last = Some(sig);
                true
            }
        };
        report.delete_block_changed = changed;
        *slot = Some(format!(
            "本轮有 {} 项删除被保护挡住",
            report.deletes_blocked
        ));
        if changed {
            tracing::warn!("删除保护熔断（{} 项）：{}", report.deletes_blocked, reason);
        } else {
            // 同一批删除还在被挡：降为 debug，不再每 30s 刷一条 WARN / journal。
            tracing::debug!(
                "删除保护熔断持续中（{} 项）：{}",
                report.deletes_blocked,
                reason
            );
        }
    } else {
        let had_reason = slot.take().is_some();
        let had_sig = stats.delete_block_sig.lock().unwrap().take().is_some();
        report.delete_block_changed = had_reason || had_sig;
        if report.delete_block_changed {
            tracing::info!("删除保护熔断已解除：本轮删除检查通过");
        }
    }
}

/// ★ 判「`qbox_get_max_log` 这次失败 = 会话没了」。
///
/// `Error::is_auth()` 只认 `Auth` 与 status 4/5；但**真机证据**（`docs/M10-会话热更新.md`
/// §1 现场日志）显示 sid 过期后 `qbox_get_max_log` 会回 `status=3`：
///
/// ```text
/// 变更轮询每一轮都报错（`qbox_get_max_log 失败: status=3`），也不会重登。
/// ```
///
/// status=3 没有码表、也**只在这一个端点上**观测到，所以这里做两件保守的事：
/// 1. 判据只给 `qbox_get_max_log` 用（其他端点不推广 status=3）；
/// 2. 误判的代价只是「多登一次」，漏判的代价是真机实测过的「每 30s 报错，
///    直到 120s 保活才恢复」。
///
/// ⚠️ 关于「多登一次」到底多贵：`login_internal` 自己**不做去重**；唯一的 3s
/// 去重在 `spawn_session_broker` 里（`SESSION_REFRESH_DEDUP`），而那把去重**只服务
/// FUSE 挂载点的重登请求**。轮询循环看到 `auth_failed` 后是直接调 `login_internal`
/// （见 `spawn_poller`）—— 频率上限是「每轮一次」，轮询默认 30s 一轮，量级可接受；
/// 但如果凭据本身失效，就会每轮一次失败登录（长期可能撞上 NAS 的暴力破解封锁）。
/// 所以这里**不要**再把「有 3s 去重」当成放宽判据的理由。
fn max_log_failure_is_session_loss(e: &CoreError) -> bool {
    if e.is_auth() {
        return true;
    }
    matches!(
        e,
        CoreError::Status { status, context, .. }
            if status.0 == 3 && context == "qbox_get_max_log"
    )
}

/// 把内存状态写回 store（**游标 + baseline 同一个事务**；失败只记录不 panic）。
fn persist(store: &Mutex<SyncState>, cursors: &Cursors, baseline: &Baseline) {
    let mut g = store.lock().unwrap();
    g.cursors = *cursors;
    let hook_touched = g.hook_touched;
    merge_round_baseline(&mut g, baseline, hook_touched);
    if let Err(e) = g.save_all() {
        tracing::warn!("状态落盘失败（游标 + baseline 同一事务）: {e}");
    }
}

/// 把一轮结束时的 baseline 快照并入状态库。
///
/// ## 两种模式（`hook_touched` 决定）
///
/// * **本轮没有 hook 并发写入** → **整份替换**。这是默认路径，也是唯一能让
///   **删除/改名迁移落盘**的路径：旧实现只做增量合并，`remove_baseline_tree`
///   从快照里删掉的条目会在下一次克隆时从内存 `g.baseline` 复活，
///   于是「清理回收站 baseline」「删除已确认的基线条目」永远清不掉。
/// * **本轮有 hook 并发写入**（上传成功的 `note_uploaded` 写入了比快照更新的签名）
///   → 逐条合并，只接受 mtime 更新的条目：宁可删除晚一轮落盘，也不能把 hook
///   刚推进的签名覆盖回旧快照（那会让连续两次保存凭空变成冲突副本）。
///
/// mtime 相等时保留内存里那份：epoch 秒粒度下同秒连改两次很常见，快照没有
/// 更权威的理由。合并完成后清掉标记，让下一次 persist 回到整份替换。
fn merge_round_baseline(g: &mut SyncState, baseline: &Baseline, hook_touched: bool) {
    if !hook_touched {
        g.baseline = baseline.clone();
        g.hook_touched = false;
        return;
    }
    for (p, sig) in baseline.entries.iter() {
        let cur = g.baseline.get(p);
        if sig.mtime > cur.mtime {
            g.baseline.put(p.clone(), *sig);
        }
    }
    // 合并完就把标记清掉：本轮更早的 persist 已经消费过它，后续 persist 可以整份替换。
    g.hook_touched = false;
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
    state_store: &Mutex<SyncState>,
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
        // ★ M7：被选择性同步排除的路径连事件都不看（它不在挂载点里，也没有 baseline）
        if view.hidden(&path, false).is_some() {
            report.events_skipped += 1;
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
        apply_decision(
            client,
            view,
            &path,
            d,
            remote,
            baseline,
            report,
            state_store,
        )
        .await;
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
    state_store: &Mutex<SyncState>,
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
        if under_root(&d) && d != root && view.hidden(&d, true).is_none() {
            dirs.insert(d);
        }
    }
    for p in baseline.entries.keys() {
        if under_root(p) && p != &root && view.hidden(p, true).is_none() {
            dirs.insert(parent_dir(p));
        }
    }
    dirs.insert(root.clone());

    // 候选路径：本地节点 + baseline 条目
    let mut candidates: BTreeSet<String> = BTreeSet::new();
    for n in view.view.nodes() {
        if under_root(&n.remote) && n.remote != root && view.hidden(&n.remote, n.is_dir).is_none() {
            candidates.insert(n.remote);
        }
    }
    for p in baseline.entries.keys() {
        if under_root(p) && p != &root && view.hidden(p, false).is_none() {
            candidates.insert(p.clone());
        }
    }

    // 列每个目录 → 远端子项签名表
    let mut remote_map: BTreeMap<String, Sig> = BTreeMap::new();
    let mut scanned = 0usize;
    // ★ 非 404 的列举失败数（网络错误 / 权限 / 服务端忙…）。
    //   见下面「远端视图不完整时暂缓删除判定」。
    let mut list_failures = 0usize;
    for dir in dirs.iter().take(cfg.max_dirs_per_poll) {
        match client.list(dir).await {
            Ok(entries) => {
                scanned += 1;
                // ★ M9：把这份 NAS 文件列表推给挂载视图 —— 「映射」就是它的本地物化。
                //   之后 `readdir`/`lookup` 都吃快照，不再每次往 NAS 跑（ls 不再等网）。
                view.view.apply_listing(dir, &entries);
                for e in &entries {
                    let path = join(dir, &e.filename);
                    // ★ M7：排除 / 临时文件不进远端签名表 —— 既不水合也不登记 baseline
                    if view.hidden(&path, e.isfolder).is_some() {
                        continue;
                    }
                    remote_map.insert(path, Sig::from_entry(e));
                }
            }
            Err(e) if is_not_found(&e) => {
                // 目录本身没了 → 里面的候选在 remote_map 里缺失，自然按「远端不存在」处理
                tracing::debug!("对账：目录已不存在 {dir}");
                view.view.drop_listing(dir);
            }
            Err(e) => {
                // ★ 这不是「远端不存在」，而是**我们没看到**。记错误，并由调用方
                //   暂缓本视图的删除判定（下面 `defer_deletes`）。
                list_failures += 1;
                if e.is_auth() {
                    report.auth_failed = true;
                }
                report.error(format!("对账列举 {dir} 失败: {e}"));
            }
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

    // ★ M7 + M11+：被排除路径（用户规则 / 回收站）的 baseline 条目直接清掉 ——
    //   不留幽灵状态。注意**只清状态，不动本地内容**（排除 ≠ 删除）。
    //   真机取证：某用户 baseline 1991 条里 1425 条在 @Recycle 下、13 条在 .Trash-1000 下；
    //   这些条目会被 QTS 清理回收站时变成批量「远端删除」候选（本轮 526 项被挡删除里
    //   就有 13 条 .Trash-1000 —— 回收站不该进 baseline 的直接证据）。
    let stale: Vec<(String, HideReason)> = baseline
        .entries
        .keys()
        .filter(|p| under_root(p) && *p != &root)
        .filter_map(|p| view.hidden(p, false).map(|r| (p.clone(), r)))
        .collect();
    if !stale.is_empty() {
        let trash = stale
            .iter()
            .filter(|(_, r)| *r == HideReason::Trash)
            .count();
        for (p, _) in &stale {
            remove_baseline_tree(baseline, p);
        }
        // ★ 队列里若已积累回收站路径的**上传**作业，一并撤掉（只撤队列，不碰文件）。
        //   UploadQueue 没有「列出全部待传路径」的公开 API，但这里恰好知道具体路径
        //   （baseline 里被排除的回收站条目）—— 逐个 cancel 是 daemon 侧能做到的全部；
        //   更早版本入队、但 baseline 里没有记录的作业仍可能残留一次（见报告「未修」）。
        if trash > 0 {
            if let Some(q) = &view.upload {
                for (p, reason) in &stale {
                    if *reason == HideReason::Trash && q.cancel(p) {
                        tracing::debug!("撤销回收站路径的待上传作业: {p}");
                    }
                }
            }
        }
        if trash > 0 {
            report.note(format!(
                "回收站排除：清理了 {trash} 条 baseline 记录（回收站不参与同步；只清状态，不动文件）"
            ));
        }
        let by_rules = stale.len() - trash;
        if by_rules > 0 {
            report.note(format!("规则排除：清理了 {by_rules} 条 baseline 记录"));
        }
    }

    tracing::debug!(
        "对账 {}：dirs={} candidates={} remote_items={}",
        view.remote_root,
        dirs.len(),
        candidates.len(),
        remote_map.len()
    );

    // ★ M15/T9：**先把「远端删除 + 远端新增」按 `file_id` 配成改名**，
    // 配上的走 `rename_remote` 迁移本地节点 —— 不生成冲突副本。
    //
    // 必须在删除/决策**之前**做完：配上的路径要同时从「删除清单」和「决策循环」里摘掉，
    // 否则会被后面的删除分支当成「远端没了」把本地节点连缓存一起删掉。
    //
    // 保守性由 `pair_renames` 保证（唯一+ 互斥 + 类型一致才配），这里再做一层：
    // `rename_remote` 返回 false（视图不支持 / 目标已被占）就**不配**，
    // 退回原有的增删判定。
    let renames = collect_renames(view, baseline, &candidates, &remote_map, report);
    let mut renamed_set: BTreeSet<String> = BTreeSet::new();
    for pair in &renames {
        if !view.view.rename_remote(&pair.old_path, &pair.new_path) {
            // 迁移失败 → 这一对不成立，保守退回增删判定。
            tracing::debug!(
                "改名配对迁移失败 {} → {}（退回增删判定）",
                pair.old_path,
                pair.new_path
            );
            continue;
        }
        renamed_set.insert(pair.old_path.clone());
        renamed_set.insert(pair.new_path.clone());
        // baseline 跟着搬：旧路径的签名迁到新路径，下一轮不再当成「本地新增」。
        apply_renames_to_baseline(baseline, std::slice::from_ref(pair));
        if report.renamed < u64::MAX {
            report.renamed += 1;
        }
    }
    if !renamed_set.is_empty() {
        report.note(format!(
            "按文件身份认出 {} 处远端改名（不生成冲突副本）",
            report.renamed
        ));
    }

    // ★ 数据安全阀：远端视图**不完整**时，本轮不做删除判定。
    //
    //   两种来源都算「不完整」，判据在 [`delete_verdict_is_reliable`]：
    //   1. 某个目录**列举失败**（非 404 的网络/权限/服务端错误）—— 其中所有条目
    //      都不在 remote_map 里；
    //   2. **目录数超过单轮扫描上限**（排序在后的目录这轮根本没扫）—— 同样地，
    //      那些目录里的条目全都不在 remote_map 里。这一种**没有错误、没有失败计数**，
    //      比 1 更隐蔽：漏判就会把「没看到」当成「远端已删」，真删本地节点（缓存一起没）
    //      并清掉 baseline。真机 dirs=38 没触发，但目录多的任务会中。
    //
    //   最坏情况：某个目录一次超时/没扫到 → 其中所有文件都不在 remote_map 里 →
    //   `decide` 对「本地干净」的条目判 `DeleteLocal`。所以这一轮**暂缓删除判定**
    //   （其它决策照常），下一轮列全了再说。
    let dirs_truncated = dirs.len() > cfg.max_dirs_per_poll;
    let defer_deletes =
        !delete_verdict_is_reliable(list_failures, dirs.len(), cfg.max_dirs_per_poll);
    if defer_deletes {
        let mut why: Vec<String> = Vec::new();
        if list_failures > 0 {
            why.push(format!("{list_failures} 个目录列举失败"));
        }
        if dirs_truncated {
            why.push(format!(
                "目录数 {} 超过单轮上限 {}（后 {} 个本轮没扫）",
                dirs.len(),
                cfg.max_dirs_per_poll,
                dirs.len() - cfg.max_dirs_per_poll
            ));
        }
        report.note(format!(
            "对账：{} → 本轮跳过删除判定（远端视图不完整，不敢按缺失删本地）",
            why.join("；")
        ));
    }

    let mut deletes: Vec<String> = Vec::new();
    for path in &candidates {
        if renamed_set.contains(path) {
            continue;
        }
        let remote = remote_map.get(path).copied().unwrap_or(Sig::MISSING);
        let local = local_sig(view, path);
        let base = baseline.get(path);
        let d = decide(&local, &base, &remote);
        if d != Decision::Noop {
            tracing::debug!(
                "决策 {path}: local={:?} base={base:?} remote={remote:?} → {d:?}",
                local
            );
        }
        match d {
            Decision::DeleteLocal => {
                if !defer_deletes {
                    deletes.push(path.clone());
                }
            }
            _ => {
                apply_decision(client, view, path, d, remote, baseline, report, state_store).await;
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
                // ★ 原因要带出去：`absorb_round` 用它和数量一起构成「熔断签名」，
                //   同签名只 WARN / 记 journal 一次（真机每 30s 刷屏 13 小时的根因）。
                report.delete_block_reason.get_or_insert(reason.clone());
                report.note(format!("删除保护熔断（{} 项）：{reason}", deletes.len()));
                tracing::debug!(
                    "删除保护熔断（{} 项）：{reason}（重复轮次不重复 WARN，由 absorb_round 决定）",
                    deletes.len()
                );
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

/// 本轮「按远端缺失删本地」的判定是否可信。
///
/// 远端视图**不完整**的两种来源，任何一种都禁止按缺失删本地：
///
/// 1. 本视图有**非 404** 的目录列举失败（网络/权限/服务端错误）——
///    `remote_map` 里缺的项**不代表 NAS 上没有了**，只代表这轮没看到；
///    「远端确实不存在」走 404/2/6（[`is_not_found`]），**不算**列举失败。
/// 2. **目录数超过单轮扫描上限**（[`SyncConfig::max_dirs_per_poll`]）——
///    排序在后的目录这轮**根本没扫**，其中所有条目同样会从 `remote_map` 里缺失。
///    这一种最危险：没有错误、没有失败计数，漏判就会把「没看到」当成
///    「远端已删」，真删本地节点/缓存并清 baseline。
///
/// 判定不可信时调用方**暂缓删除判定**（其它决策照常），下一轮列全了再说。
fn delete_verdict_is_reliable(list_failures: usize, dirs_total: usize, max_dirs: usize) -> bool {
    list_failures == 0 && dirs_total <= max_dirs
}

/// ★ M15/T9：收集本轮可能的改名对 —— 「远端已删」×「远端新增」，按 `file_id` 配。
///
/// 两侧的取法（都是**本轮已观测到的事实**，不猜）：
///
/// * **已删侧** = 在 `candidates` 里、但不在 `remote_map` 里的路径 —— 对账已经认定
///   「本地/基线里有、远端这轮没列出来」。
/// * **新增侧** = 在 `remote_map` 里、但既不在 `candidates` 也不在 `baseline` 里的路径 ——
///   即「远端有了、本地还没见过」。
///
/// 配对本身交给 `pair_renames`（唯一 / 互斥 / 类型一致才配，歧义整组作废）。
/// 这里只负责**把两侧的 `file_id` 算出来并组装好候选**，外加两条本层特有的过滤：
///
/// 1. 身份为 `ZERO_FILE_ID` 的一律不参与 —— 「不知道」不能当成「同一个」；
/// 2. 两侧都必须在同一个挂载视图的远端根之下 —— 别把两个视图的路径凑成一对。
///
/// 返回的候选对**只是候选** —— 调用方还要看 `rename_remote` 是否真的迁移成功，
/// 失败就整对作废（退回增删判定）。
fn collect_renames(
    view: &MountView,
    baseline: &Baseline,
    candidates: &BTreeSet<String>,
    remote_map: &BTreeMap<String, Sig>,
    _report: &mut SyncReport,
) -> Vec<qxync_core::sync::RenamePair> {
    let root = view.remote_root.trim_end_matches('/').to_string();
    let under_root = |p: &str| p == root || p.starts_with(&format!("{root}/"));

    // 新增侧：远端有、本地没见过。
    let mut added: Vec<RenameCandidate> = Vec::new();
    for (path, sig) in remote_map {
        if candidates.contains(path) || baseline.get(path).exists {
            continue;
        }
        let Some(fid) = candidate_file_id(view, path, sig) else {
            continue;
        };
        added.push(RenameCandidate::new(path.clone(), fid, sig.is_dir));
    }
    if added.is_empty() {
        return Vec::new();
    }

    // 已删侧：候选里有、远端这轮没列出来。
    let mut removed: Vec<RenameCandidate> = Vec::new();
    for path in candidates {
        if remote_map.contains_key(path) || !under_root(path) {
            continue;
        }
        let Some(node) = view.view.node(path) else {
            continue;
        };
        // 用**节点自己的身份**（FUSE 侧按内容算好存着）。取不到就不参与 ——
        // 宁可少配一对（退回增删，多个副本），也不要拿一个猜出来的身份去配。
        if node.file_id == qxync_core::file_id::ZERO_FILE_ID {
            continue;
        }
        removed.push(RenameCandidate::new(
            path.clone(),
            node.file_id,
            node.is_dir,
        ));
    }
    if removed.is_empty() {
        return Vec::new();
    }
    pair_renames(&removed, &added)
}

/// 算出「远端新增项」的身份。新增侧本地还没有节点，只能按远端签名算 ——
/// 与本地节点用的是同一个算法（`compute_file_id`），所以两边算出来的 id 可比。
fn candidate_file_id(
    view: &MountView,
    path: &str,
    sig: &Sig,
) -> Option<qxync_core::file_id::FileId> {
    use qxync_core::file_id::{compute_file_id, ZERO_FILE_ID};
    let name = path.rsplit_once('/').map(|(_, n)| n).unwrap_or(path);
    let parent = path.rsplit_once('/').map(|(d, _)| d).unwrap_or("/");
    let fid = compute_file_id(sig.is_dir, sig.size, sig.mtime, name, parent);
    if fid == ZERO_FILE_ID {
        return None;
    }
    // 挂载根之外的路径不参与（防止跨视图配对）
    let root = view.remote_root.trim_end_matches('/');
    if !(path == root || path.starts_with(&format!("{root}/"))) {
        return None;
    }
    Some(fid)
}

/// 这条冲突是不是**第一次**入队？
///
/// 「每个文件都问我」策略下，用户裁决之前**每一轮都会重新看到同一个冲突**
/// （local/remote/baseline 一直没变）。只有第一次入队才算「新冲突」：
/// 计入 `conflicts`、记 note、进 journal。否则 30s 一轮会把计数器和
/// 「更新中心」刷成几百条同一件事（真机 journal 3023/3046 条就是这么来的）。
fn conflict_is_new(existing: &Option<qxync_core::store::DecisionRow>) -> bool {
    existing.is_none()
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
    state_store: &Mutex<SyncState>,
) {
    match d {
        Decision::Noop => {}
        Decision::AdoptBaseline => {
            baseline.put(path, remote);
        }
        Decision::RefreshRemote => {
            let existed =
                view.view
                    .apply_remote_meta(path, remote.is_dir, remote.size, remote.mtime);
            // ★ M9：本地「有水」→ 远端那份先不急着覆盖本地内容：
            //   旧内容继续可读（`cat` 立刻拿得到），同时后台把新版本整份拉下来、
            //   原子换上。**baseline 这轮先不推进** —— 否则用户此刻基于旧内容改文件，
            //   会被三向合并误判成「本地覆盖远端」，而不是该有的冲突。
            if existed && view.view.pending_refresh(path) {
                view.view.spawn_content_refresh(path);
                report.content_updating += 1;
                tracing::debug!("远端变更 {path} → 本地有水，后台更新内容");
            } else {
                if existed {
                    report.refreshed += 1;
                    tracing::info!("远端变更 {path}（{} 字节）→ 元数据已刷新", remote.size);
                }
                baseline.put(path, remote);
            }
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
            resolve_conflict(client, view, path, remote, baseline, report, state_store).await;
        }
        Decision::DeleteLocal => {
            // 调用方统一做删除保护，这里不应到达
        }
    }
}

/// ★ M8.4：冲突分派 —— 按任务上的 `conflict` 策略决定「怎么解」。
///
/// 五个取值逐字对齐 Qsync 6（`docs/M8-向Qsync-Client-6靠拢.md` §1.7）：
///
/// | 策略 | 行为 | 谁丢数据 |
/// |---|---|---|
/// | `ask` | 两边都不动，写待裁决队列等用户裁决 | 都不丢 |
/// | `rename_remote` | NAS 上那份改名成冲突副本，本地内容占原名 | 都不丢 |
/// | `rename_local`（默认） | 本地内容另存冲突副本，远端占原名 | 都不丢 |
/// | `replace_remote` | 本地内容覆盖 NAS 上的文件 | **远端那份丢** |
/// | `replace_local` | NAS 上的文件覆盖本地内容 | **本地那份丢** |
///
/// 前置的「先等在途上传落地再重判」（M2c 踩过的坑）对所有策略都保留。
async fn resolve_conflict(
    client: &Client,
    view: &MountView,
    path: &str,
    mut remote: Sig,
    baseline: &mut Baseline,
    report: &mut SyncReport,
    state_store: &Mutex<SyncState>,
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
                        let existed = view
                            .view
                            .apply_remote_meta(path, r2.is_dir, r2.size, r2.mtime);
                        // ★ M9：本地有水时同样先留着旧内容、后台换新，baseline 等落地
                        if existed && view.view.pending_refresh(path) {
                            view.view.spawn_content_refresh(path);
                            report.content_updating += 1;
                        } else {
                            baseline.put(path, r2);
                            report.refreshed += 1;
                        }
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

    let policy = if view.conflict.is_empty() {
        qxync_core::tasks::DEFAULT_CONFLICT
    } else {
        view.conflict.as_str()
    };
    match policy {
        // ---- 用 NAS 上的文件替换本地（本地那份**丢**，UI 有显式警告）
        CONFLICT_REPLACE_LOCAL => {
            replace_local_with_remote(
                view,
                path,
                remote,
                baseline,
                report,
                "用 NAS 上的文件替换本地文件",
            );
        }
        // ---- 用本地文件替换 NAS 上的（远端那份**丢**）
        CONFLICT_REPLACE_REMOTE => {
            upload_local_over_remote(view, path, report, "用本地文件替换 NAS 上的文件");
        }
        // ---- NAS 上那份改名成冲突副本，本地内容占原名
        CONFLICT_RENAME_REMOTE => {
            let (dir, name) = split_path(path);
            let chosen = match pick_conflict_name(client, &dir, &name, report).await {
                Some(c) => c,
                None => return,
            };
            if let Err(e) = client.rename(&dir, &name, &chosen).await {
                report.error(format!(
                    "{path}: 远端改名成 {chosen} 失败（策略：重命名 NAS 上的文件）: {e}"
                ));
                return;
            }
            let conflict_remote = join(&dir, &chosen);
            // 本地内容占原名 → 入队上传（远端这个名字此刻是空的）
            upload_local_over_remote(
                view,
                path,
                report,
                "重命名 NAS 上的文件（NAS 上的文件已改名，本地内容占原名）",
            );
            report.conflicts += 1;
            tracing::warn!("冲突：{path} → NAS 上的文件改名为 {conflict_remote}，本地内容占原名");
            report.note(format!(
                "冲突：NAS 上的文件已改名为 {conflict_remote}（本地内容占原名，双方都保留）"
            ));
            if let Err(e) = client
                .write_log(&conflict_remote, write_action::UPSERT_FILE)
                .await
            {
                tracing::debug!("冲突副本 write_log 失败（不影响）: {e}");
            }
        }
        // ---- 每个文件都问我：没有裁决 → 入队等待；已有裁决 → 执行
        CONFLICT_ASK => {
            let existing = {
                let g = state_store.lock().unwrap();
                match g.store.decision_by_path(path) {
                    Ok(row) => row,
                    Err(e) => {
                        report.error(format!("读冲突待裁决队列失败 {path}: {e}"));
                        return;
                    }
                }
            };
            let decided = existing.as_ref().and_then(|r| r.resolution.clone());
            match decided.as_deref() {
                Some("keep_local") => {
                    upload_local_over_remote(view, path, report, "按用户裁决：保留本地");
                    finish_decision(state_store, path);
                }
                Some("keep_remote") => {
                    replace_local_with_remote(
                        view,
                        path,
                        remote,
                        baseline,
                        report,
                        "按用户裁决：保留 NAS 上的",
                    );
                    finish_decision(state_store, path);
                }
                Some("keep_both") => {
                    rename_local_copy(client, view, path, remote, baseline, report, local.mtime)
                        .await;
                    finish_decision(state_store, path);
                }
                _ => {
                    // 入队（同路径只保留一条；每轮都冲突也只记一条）
                    //
                    // ★ 只有**新入队**才算一个「冲突」并记 note：同一条冲突在用户裁决前
                    //   每 30s 都会被重新看到，以前每轮都 `conflicts += 1` + 写 journal
                    //   「生成 N 个冲突副本」（其实一个副本都没生成）→ 计数器与
                    //   「更新中心」被同一件事刷屏，而且总数看着像有几百个冲突。
                    let is_new = conflict_is_new(&existing);
                    let (local_size, local_mtime) = (local.size, local.mtime);
                    let row = qxync_core::store::DecisionRow {
                        id: qxync_core::store::DecisionRow::id_for(path),
                        path: path.to_string(),
                        task_id: String::new(),
                        is_dir: false,
                        local_size,
                        local_mtime,
                        remote_size: remote.size,
                        remote_mtime: remote.mtime,
                        created_unix: now_secs() as i64,
                        resolution: None,
                    };
                    match state_store.lock().unwrap().store.decision_upsert(&row) {
                        Ok(()) => {
                            if is_new {
                                report.conflicts += 1;
                                report.note(format!(
                                    "{path}: 冲突等待裁决（策略：每个文件都问我）→ GUI 任务页 / `qxync conflicts`"
                                ));
                            } else {
                                // 已入队过：只更新记录的签名（upsert 已做），不重复计数/刷日志。
                                tracing::debug!(
                                    "{path}: 冲突仍在等待裁决（已入队过，本轮不重复计数）"
                                );
                            }
                        }
                        Err(e) => report.error(format!("{path} 写入待裁决队列失败: {e}")),
                    }
                }
            }
        }
        // ---- 默认：重命名本地文件（M2c 的原始行为）
        _ => {
            rename_local_copy(client, view, path, remote, baseline, report, local.mtime).await;
        }
    }
}

/// 删掉一条已执行的裁决（只在**成功执行**后调用）。
fn finish_decision(state_store: &Mutex<SyncState>, path: &str) {
    let id = qxync_core::store::DecisionRow::id_for(path);
    let g = state_store.lock().unwrap();
    match g.store.decision_delete(&id) {
        Ok(true) => tracing::info!("待裁决冲突已执行并出队: {path}"),
        Ok(false) => {}
        Err(e) => tracing::warn!("待裁决冲突出队失败 {path}: {e}"),
    }
}

/// 找出覆盖 `path` 的挂载视图（路径在远端根之下）。
///
/// 冲突策略挂在**挂载点**上（`MountEntry.conflict`），不是 `decisions` 表的
/// `task_id` 字段上（那个字段目前恒为空串），所以有视图时用视图上的策略。
///
/// ⚠️ **没有视图 ≠ 任务被删**：daemon 刚启动 / 用户卸载 / 恢复挂载失败时视图都会
/// 暂时不在。要判断「任务是不是真没了」必须看磁盘上的任务配置
/// （[`unmounted_row_facts`]），不能拿这个函数的 `None` 当结论。
fn view_for_path<'a>(views: &'a [MountView], path: &str) -> Option<&'a MountView> {
    views.iter().find(|v| {
        let root = v.remote_root.trim_end_matches('/');
        path == root || path.starts_with(&format!("{root}/"))
    })
}

/// 磁盘上「本机到底配了哪些同步任务」的快照 —— 区分「任务被删」与「只是现在没挂载」。
///
/// ## 为什么不能只看挂载视图（真机风险）
///
/// daemon 刚启动、用户卸载、恢复挂载失败时 `views` 是空的，那时**所有**队列行看起来
/// 都「没有任务管」。旧实现据此伪造 `task_alive=false` + `remote_now=MISSING`，
/// 巡检第 1 条（远端已不存在）先命中 → **整队待裁决被静默删光**，理由还显示成
/// 「NAS 上的文件已不存在」—— 用户的裁决意图直接丢失。
///
/// 现在用磁盘上的任务配置做第二判据：**任务存在 → 留**（挂载回来还要执行裁决），
/// 确认没有任何任务覆盖该路径 → 才允许出队。
pub struct ConfiguredTasks {
    pub tasks: Vec<qxync_core::tasks::Task>,
    /// 任务文件是否**全部**解析成功。`false`（有坏文件 / 配置目录读不出）时
    /// 「没有任务覆盖这条路径」这个结论不可信 → 巡检必须保守保留。
    pub complete: bool,
}

impl ConfiguredTasks {
    /// 从磁盘读一次（`~/.config/qxync/tasks/*.json`），**只装本连接的任务**。
    ///
    /// ★ M16：任务文件是全局共享的；不按 link 过滤的话，A 连接的任务会把 B 连接
    /// 待裁决条目的「任务还活着」判据喂成 true（反之亦然），巡检就会误删/误留。
    fn load(link_id: &str) -> Self {
        match qxync_core::config::ConfigPaths::discover() {
            Ok(paths) => {
                let (links, bad_links) = qxync_core::config::LinkConfig::list(&paths);
                if !bad_links.is_empty() {
                    tracing::warn!(
                        "巡检：{} 个连接配置解析失败，本轮不据此判定「任务已删除」",
                        bad_links.len()
                    );
                }
                let mut ids: Vec<String> = links.into_iter().map(|l| l.id).collect();
                if !ids.iter().any(|id| id == link_id) {
                    ids.push(link_id.to_string());
                }
                ids.sort();
                let (tasks, bad) = qxync_core::tasks::Task::list_for_link(&paths, link_id, &ids);
                if !bad.is_empty() {
                    // 解析失败的任务文件里可能就写着覆盖关系 —— 不能据此判定「任务已删」。
                    tracing::warn!(
                        "巡检：{} 个任务文件解析失败，本轮不据此判定「任务已删除」",
                        bad.len()
                    );
                }
                Self {
                    tasks,
                    complete: bad.is_empty() && bad_links.is_empty(),
                }
            }
            Err(e) => {
                tracing::warn!("巡检：读不到配置目录，本轮不据此判定「任务已删除」: {e}");
                Self {
                    tasks: Vec::new(),
                    complete: false,
                }
            }
        }
    }

    /// 覆盖该**远端**路径的任务（一对一：比较任务生效的 NAS 文件夹）。
    fn covering<'a>(&'a self, path: &str) -> Option<&'a qxync_core::tasks::Task> {
        self.tasks
            .iter()
            .find(|t| path_under_root(path, &t.effective_root()))
    }
}

/// `path` 是否在远端根 `root` 之下（含根本身）。
fn path_under_root(path: &str, root: &str) -> bool {
    let root = root.trim_end_matches('/');
    path == root || path.starts_with(&format!("{root}/"))
}

/// 待裁决队列行是否属于**回收站**路径（要直接作废，不许留在队列里）。
///
/// 单独抽出来是为了能离线单测：sweep 的真正取事实路径需要 NAS，但「哪些行算回收站」
/// 是纯判定，不该跟着一起只能靠真机验证。
fn is_trash_decision_row(row: &qxync_core::store::DecisionRow) -> bool {
    qxync_core::trash::is_trash_path(&row.path, local_uid())
}

/// 一行队列条目在**没有挂载视图**时的巡检事实。
///
/// * 任务存在（只是没挂载）→ `observed=false` + `task_alive=true` → 留（按磁盘上的
///   策略判第 3 条；策略不再是 `ask` 时仍然可以出队，因为挂载回来也不会读这张表）；
/// * 确认没有任何任务覆盖 → `task_alive=false` → 出队（没人会执行这条裁决）；
/// * 配置读取不完整（有坏任务文件 / 读不到配置）→ 返回 `None` → 调用方**保留**。
fn unmounted_row_facts(
    row: &qxync_core::store::DecisionRow,
    configured: &ConfiguredTasks,
    baseline: &Baseline,
) -> Option<qxync_core::decisions::DecisionFacts> {
    let recorded_local = Sig::file(row.local_size, row.local_mtime);
    let recorded_remote = Sig::file(row.remote_size, row.remote_mtime);
    let base = baseline.get(&row.path);
    match configured.covering(&row.path) {
        Some(task) => Some(qxync_core::decisions::DecisionFacts {
            // ★ 没有视图 = 没看到远端现状；没有本地视图 = 也没看到本地现状。
            //   两个 flag 都必须是 false，`local_now`/`remote_now` 只是占位值。
            observed: false,
            local_observed: false,
            recorded_local,
            recorded_remote,
            resolved: row.is_resolved(),
            task_alive: true,
            policy: task.conflict.clone(),
            local_now: Sig::MISSING,
            local_dirty: false,
            remote_now: Sig::MISSING,
            baseline: base,
        }),
        None if configured.complete => Some(qxync_core::decisions::DecisionFacts {
            observed: false,
            local_observed: false,
            recorded_local,
            recorded_remote,
            resolved: row.is_resolved(),
            task_alive: false,
            policy: String::new(),
            local_now: Sig::MISSING,
            local_dirty: false,
            remote_now: Sig::MISSING,
            baseline: base,
        }),
        None => None,
    }
}

/// 冲突待裁决队列巡检：把已经不该留着的行清掉。
///
/// ## 为什么需要这一步
///
/// 队列原本**只有一条删除路径**：引擎执行完裁决后调 [`finish_decision`]。于是三个
/// 常见情况会让行永久残留 —— 冲突自己消失了、挂载策略从 `ask` 改成了别的、挂载被
/// 卸载。行留着不丢数据，但用户看到的是「我明明决定好了，怎么还在」，**看着像卡住**。
///
/// ## 判据在哪
///
/// 纯逻辑在 [`qxync_core::decisions::sweep_decision`]（可离线单测）；这里只负责
/// **取事实**（stat 远端、查视图、算本地签名）与**执行删除**。
///
/// ## 保守约束
///
/// * 查不到事实的行（stat 失败）→ **跳过**，绝不当成可删。查不到 ≠ 失效。
/// * **没有挂载视图 ≠ 任务被删**：先用磁盘任务配置确认（见 [`unmounted_row_facts`]）。
/// * 未裁决的行即便此刻不冲突也**不自动删**（理由见 `sweep_decision` 的文档）。
/// * 只删行，**不碰任何文件** —— 与 `decisions_clear` 语义一致。
async fn sweep_decision_queue(
    client: &Client,
    views: &[MountView],
    baseline: &Baseline,
    store: &Mutex<SyncState>,
    report: &mut SyncReport,
    link_id: &str,
) {
    let rows = {
        let g = store.lock().unwrap();
        match g.store.decisions() {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("读待裁决队列失败，跳过本轮巡检: {e}");
                return;
            }
        }
    };
    if rows.is_empty() {
        return;
    }

    // 先把要 stat 的路径去重 —— 同一目录多条冲突时省往返
    let mut remote_cache: BTreeMap<String, Option<Sig>> = BTreeMap::new();
    let mut facts: BTreeMap<String, qxync_core::decisions::DecisionFacts> = BTreeMap::new();
    // ★ 回收站路径的队列行：直接作废（不 stat、不许留在队列里）。
    //   FUSE 侧本就拒绝操作回收站，同步引擎也不再管它 —— 一条「回收站的待裁决」
    //   永远不会被执行，留着只是让用户以为还有事没做。**只删队列行，不碰任何文件**。
    let mut trash_drops: Vec<(String, String)> = Vec::new();
    // ★ 需要判「任务是不是真被删了」时才去读磁盘配置（正常路径下不会读）。
    let mut configured: Option<ConfiguredTasks> = None;

    for r in &rows {
        if is_trash_decision_row(r) {
            trash_drops.push((
                r.id.clone(),
                "回收站路径不参与同步（只清队列，不动文件）".to_string(),
            ));
            continue;
        }
        // 定位挂载视图：找不到 = 这个路径当前没有任何挂载在管它。
        // ★ 但**不等于**任务被删（可能是卸载中 / daemon 刚启动 / 恢复失败）——
        //   必须再用磁盘上的任务配置确认，否则会把整队静默删光。
        let Some(view) = view_for_path(views, &r.path) else {
            let cfg = configured.get_or_insert_with(|| ConfiguredTasks::load(link_id));
            match unmounted_row_facts(r, cfg, baseline) {
                Some(f) => {
                    facts.insert(r.id.clone(), f);
                }
                None => {
                    // 配置读不完整 → 无法判定任务是否还在 → 保留（宁可留，不可误删）。
                    tracing::debug!("巡检：{} 没有挂载视图、任务配置又不完整 → 本轮保留", r.path);
                }
            }
            continue;
        };

        // 远端签名：查一次缓存（stat 失败也缓存「查不到」，本轮不再重试）
        let remote_now = match remote_cache.get(&r.path) {
            Some(v) => *v,
            None => {
                let v = match stat_path(client, &r.path).await {
                    Ok(s) => Some(s),
                    Err(e) => {
                        // 查不到 → 这行的事实不完整，**跳过它**（不删）。
                        if e.is_auth() {
                            report.auth_failed = true;
                        }
                        tracing::debug!("巡检 stat {} 失败，跳过该行: {e}", r.path);
                        None
                    }
                };
                remote_cache.insert(r.path.clone(), v);
                v
            }
        };
        let Some(remote_now) = remote_now else {
            continue;
        };

        let local = local_sig(view, &r.path);
        facts.insert(
            r.id.clone(),
            qxync_core::decisions::DecisionFacts {
                // 「有视图 + stat 成功」只保证**远端**观测到了（observed）。
                // ★ 本地侧必须单独判：本地视图的节点表是进程内的，daemon 重启 /
                //   路径还没被访问过时 node() 是 None，此时 local_now 只是占位
                //   MISSING —— 拿它跑三向决策会把「本进程还没看过」当成「本地没有」，
                //   把已裁决的行误判成「冲突已自行消解」清掉（吞用户裁决意图）。
                observed: true,
                local_observed: view.view.node(&r.path).is_some(),
                recorded_local: Sig::file(r.local_size, r.local_mtime),
                recorded_remote: Sig::file(r.remote_size, r.remote_mtime),
                resolved: r.is_resolved(),
                task_alive: true,
                policy: view.conflict.clone(),
                local_now: local.sig,
                local_dirty: local.dirty,
                remote_now,
                baseline: baseline.get(&r.path),
            },
        );
    }

    // 判据（纯逻辑，在 core 里单测过）。**只跑一次**并把原因带出来 ——
    // 后面记 note 时还要用同一份结论。
    let mut drops: Vec<(String, String)> = facts
        .iter()
        .filter_map(|(id, f)| match qxync_core::decisions::sweep_decision(f) {
            qxync_core::decisions::SweepVerdict::Keep => None,
            qxync_core::decisions::SweepVerdict::Drop(why) => Some((id.clone(), why)),
        })
        .collect();
    drops.extend(trash_drops);
    if drops.is_empty() {
        return;
    }
    let drop_ids: Vec<String> = drops.iter().map(|(id, _)| id.clone()).collect();

    // 执行删除（一个事务）
    let removed = {
        let g = store.lock().unwrap();
        match g.store.decisions_delete_many(&drop_ids) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("巡检出队失败: {e}");
                return;
            }
        }
    };
    if removed == 0 {
        return;
    }

    // 逐条记原因：用户要能看懂「为什么这条自己没了」
    for (id, why) in &drops {
        let path = rows
            .iter()
            .find(|r| r.id == *id)
            .map(|r| r.path.clone())
            .unwrap_or_else(|| id.clone());
        report.note(format!("待裁决队列自动清理：{path}（{why}）"));
    }
    report.decisions_swept = removed as u64;
    tracing::info!("待裁决队列巡检：清掉 {removed} 条失效条目");
}

/// 冲突副本取名（原来内联在 resolve_conflict 里的一段）。
async fn pick_conflict_name(
    client: &Client,
    dir: &str,
    name: &str,
    report: &mut SyncReport,
) -> Option<String> {
    let device = hostname();
    let date = today();
    let mut chosen = conflict_name(name, &device, &date);
    for seq in 2..=20u32 {
        match client.stat(dir, &chosen).await {
            Ok(None) => break,
            Ok(Some(_)) => chosen = conflict_name_with_seq(name, &device, &date, seq),
            Err(e) => {
                report.error(format!("冲突副本取名失败（stat {dir}/{chosen}）: {e}"));
                return None;
            }
        }
    }
    Some(chosen)
}

/// 「用 NAS 上的文件替换本地」：刷新元数据 + 丢掉本地内容。
fn replace_local_with_remote(
    view: &MountView,
    path: &str,
    remote: Sig,
    baseline: &mut Baseline,
    report: &mut SyncReport,
    why: &str,
) {
    view.view
        .apply_remote_meta(path, remote.is_dir, remote.size, remote.mtime);
    view.view.invalidate_content(path);
    baseline.put(path, remote);
    report.conflicts += 1;
    tracing::warn!("冲突：{path} → {why}（NAS 上的内容为准，本地改动被丢弃）");
    report.note(format!("{path}: {why}（本地改动已丢弃）"));
}

/// 「用本地文件替换 NAS 上的」：把本地内容重新入队上传。
fn upload_local_over_remote(view: &MountView, path: &str, report: &mut SyncReport, why: &str) {
    if view.view.has_pending(path) {
        // 已经在队列里了，不重复入队
        report.conflicts += 1;
        return;
    }
    match view.view.mark_dirty(path) {
        Ok(()) => {
            report.conflicts += 1;
            report.uploaded += 1;
            tracing::warn!("冲突：{path} → {why}（本地内容为准，NAS 上的改动将被覆盖）");
            report.note(format!("{path}: {why}（NAS 上的改动将被覆盖）"));
        }
        Err(e) => report.error(format!("{path} 冲突后重新入队上传失败: {e}")),
    }
}

/// 默认策略：本地内容另存冲突副本并上传，远端占原名（双方都不丢）。
async fn rename_local_copy(
    client: &Client,
    view: &MountView,
    path: &str,
    remote: Sig,
    baseline: &mut Baseline,
    report: &mut SyncReport,
    local_mtime: i64,
) {
    let (dir, name) = split_path(path);
    let Some(chosen) = pick_conflict_name(client, &dir, &name, report).await else {
        return;
    };
    // ① 先把本地内容复制到 stash（下一步失效缓存会把它删掉）
    let stash = match view.view.stash_conflict(path, &chosen) {
        Ok(p) => p,
        Err(e) => {
            report.error(format!("{path} 冲突副本落盘失败: {e}"));
            return;
        }
    };
    // ② 冲突副本入队上传
    if let Err(e) = view.view.enqueue_upload(&dir, &chosen, stash, local_mtime) {
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
        .unwrap_or_else(|| "qxync-linux".to_string())
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
                    // ★ M15/T9：测试替身没有真实身份计算 → 留全零 = 「身份未知」。
                    // 全零在 `pair_renames` 里一律不配，行为退回 T9 之前的增删判定。
                    file_id: qxync_core::file_id::ZERO_FILE_ID,
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
                // ★ M15/T9：这里的作业是同步引擎入队的，身份由 FUSE 侧按内容算。
                // 传全零 = 「无身份」→ 上传时不做按身份重定向，退化成写死 remote_name
                // （与 T9 之前的行为一致）。
                file_id: qxync_core::file_id::ZERO_FILE_ID,
                // ★ T8：入队时还没开始传
                bytes_sent: 0,
                bytes_total: 0,
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
        let host = std::env::var("QXNYC_TEST_HOST").ok()?;
        let user = std::env::var("QXNYC_TEST_USER").ok()?;
        let password = std::env::var("QXNYC_TEST_PASSWORD").ok()?;
        let port = std::env::var("QXNYC_TEST_PORT")
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
                ipv4_only: std::env::var("QXNYC_TEST_IPV4").is_ok(),
                exclude: Vec::new(),
                filter_temp: true,
                peer_listen: None,
                peer_name: None,
            },
            password,
        ))
    }

    fn fixture_root() -> String {
        std::env::var("QXNYC_TEST_FIXTURE").unwrap_or_else(|_| "/home/qxync-test".to_string())
    }

    async fn logged_in() -> Option<Client> {
        let (link, pw) = env_creds()?;
        // ★ M17：真机若开了两步验证，测试也走 TOTP（env 里给 Base32 密钥）。
        let otp = std::env::var("QXNYC_TEST_TOTP_SECRET").ok();
        let mut c = Client::new(&link).expect("构造 client");
        c.login_with_otp(&link.user, &pw, otp.as_deref())
            .await
            .expect("登录失败");
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

    /// ★ 回归（用户报的「连删两行就出冲突副本」）：
    /// 自己的上传落地后**立刻**推进 baseline，连续保存就不该被判成冲突。
    ///
    /// 复现的正是修复前的时序：
    ///   t0  base=local=remote=(100,1000)
    ///   ①  保存一次 → local=(90,2000) dirty，上传落地 → remote=(90,2000)，
    ///      节点被 `clear_dirty` 标干净，但 baseline 还停在 (100,1000)
    ///   ②  紧接着再保存一次 → local=(85,2100) dirty，上传落地 → remote=(85,2100)
    ///   ③  下一轮对账：修复前 base=(100,1000)、local/remote 已是 (85,2100)
    ///      → `rc=true, lc=false` 且 `local != base` → **Conflict**
    #[test]
    fn own_upload_advances_baseline_so_rapid_saves_are_not_conflicts() {
        let d = tmpdir("rapid-save");
        let mut st = SyncState::load(&d).unwrap();
        let path = "/home/notes.txt";

        let base = Sig::file(100, 1000);
        st.baseline.put(path, base);
        st.save_all().unwrap();

        // ① 第一版：上传落地 → 走 success hook 的路径推进 baseline
        st.note_uploaded(path, 90, 2000).unwrap();
        assert_eq!(
            st.baseline.get(path),
            Sig::file(90, 2000),
            "上传成功后 baseline 应立刻等于刚落上去的签名"
        );
        // 紧接着第二次保存：本地改了、远端也等于上一版 → 正常上传，不是冲突
        let d2 = decide(
            &LocalSig {
                sig: Sig::file(85, 2100),
                dirty: true,
            },
            &st.baseline.get(path),
            &Sig::file(90, 2000),
        );
        assert_eq!(d2, Decision::UploadLocal, "第二次保存应是增量上传");

        // ② 第二版落地
        st.note_uploaded(path, 85, 2100).unwrap();
        assert_eq!(st.baseline.get(path), Sig::file(85, 2100));

        // ③ 下一轮对账：本地干净 == 远端 == baseline → 无事可做
        let d3 = decide(
            &LocalSig {
                sig: Sig::file(85, 2100),
                dirty: false,
            },
            &st.baseline.get(path),
            &Sig::file(85, 2100),
        );
        assert_eq!(d3, Decision::Noop, "两版都落地后应 Noop");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// ★ 对照组：证明上面那条 `Noop` 靠的是 hook 推进 baseline，而不是 `decide` 本身。
    ///
    /// 同样的时序，**不**推进 baseline 就会判成 Conflict —— 这正是修复前的行为。
    #[test]
    fn without_baseline_advance_rapid_saves_become_a_conflict() {
        let stale = Sig::file(100, 1000); // 修复后仍停在旧签名
        let remote = Sig::file(85, 2100);
        let d = decide(
            &LocalSig {
                sig: Sig::file(85, 2100),
                dirty: false,
            },
            &stale,
            &remote,
        );
        assert_eq!(
            d,
            Decision::Conflict,
            "baseline 不推进就是这个冲突 —— success hook 必须补上这一步"
        );
    }

    /// `note_uploaded` 幂等：同一签名重复推进不报错、不变状态。
    #[test]
    fn note_uploaded_is_idempotent() {
        let d = tmpdir("note-uploaded-idem");
        let mut st = SyncState::load(&d).unwrap();
        let path = "/home/a.txt";
        st.note_uploaded(path, 42, 100).unwrap();
        st.note_uploaded(path, 42, 100).unwrap();
        assert_eq!(st.baseline.get(path), Sig::file(42, 100));
        assert_eq!(st.baseline.len(), 1, "幂等推进不该多出条目");
        // 落盘后重新加载，签名仍在（证明真的写进了状态库）
        let re = SyncState::load(&d).unwrap();
        assert_eq!(re.baseline.get(path), Sig::file(42, 100));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// ★ 回归：一轮轮询落盘时**不能抹掉**期间由 success hook 推上去的 baseline。
    ///
    /// 时序（一轮 poll 几十秒，其间用户一直在保存）：
    ///   ① poll 开头克隆快照：baseline[a]=(100,1000)
    ///   ② 轮询跑网络 IO 期间，a.txt 上传成功 → hook 推进到 (90,2000)
    ///   ③ poll 结束时把**开轮询时的旧快照**落盘 → 旧逻辑整份盖回，
    ///      (90,2000) 被抹成 (100,1000) —— 本次修复被自己撤销
    #[test]
    fn persist_merges_newer_hook_advances_instead_of_clobbering() {
        let d = tmpdir("persist-merge");
        let mut st = SyncState::load(&d).unwrap();
        let path = "/home/a.txt";

        // ① 轮询开头拿到的快照（陈旧）
        let mut snapshot = st.baseline.clone();
        snapshot.put(path, Sig::file(100, 1000));
        st.baseline.put(path, Sig::file(100, 1000));

        // ② 轮询期间上传成功，hook 推进（比快照新）
        st.note_uploaded(path, 90, 2000).unwrap();
        assert_eq!(st.baseline.get(path), Sig::file(90, 2000));

        // ③ 轮询结束落盘那份陈旧快照
        persist_test(&mut st, &snapshot);
        assert_eq!(
            st.baseline.get(path),
            Sig::file(90, 2000),
            "落盘旧快照不能把 hook 刚推进的签名倒退回去"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `persist` 的 baseline 部分的**真实实现**包装（`hook_touched=true` =
    /// 本轮有并发 hook 写入 → 逐条合并模式；这些用例都在验证合并语义）。
    fn persist_test(st: &mut SyncState, baseline: &Baseline) {
        merge_round_baseline(st, baseline, true);
        st.save_all().unwrap();
    }

    /// ★ 关键修复：一轮**没有并发 hook 写入**时，baseline 必须**整份替换** ——
    /// 否则快照里删掉的条目（回收站清理 / 已执行的删除 / 改名旧路径）永远清不掉：
    /// 内存 `g.baseline` 保留旧条目，下一轮克隆又把它带回来，`save_all` 再写回库。
    ///
    /// 这就是任务里那句「清理后下一轮又加回来」的根因，也是 `persist()` 写盘语义
    /// 必须确认的地方。
    #[test]
    fn persist_applies_baseline_deletions_so_purges_really_land_on_disk() {
        let d = tmpdir("persist-purge");
        let mut st = SyncState::load(&d).unwrap();
        let trash = "/home/@Recycle/files/old.txt";
        let keep = "/home/kept.txt";
        st.baseline.put(trash, Sig::file(1, 100));
        st.baseline.put(keep, Sig::file(2, 200));
        st.save_all().unwrap();

        // 本轮快照：回收站条目已被 remove_baseline_tree 从**快照**里删掉（模拟 reconcile 清理）
        let mut snapshot = st.baseline.clone();
        snapshot.remove(trash);

        // 本轮没有 hook 并发写 → 整份替换（这是真实 persist 走的路径）
        merge_round_baseline(&mut st, &snapshot, false);
        assert!(!st.baseline.get(trash).exists, "内存里必须真的删掉");
        assert!(st.baseline.get(keep).exists, "没被清的正常条目要留着");
        st.save_all().unwrap();

        // 落盘后重新打开：清理结果必须还在（不是只在内存里）
        let re = SyncState::load(&d).unwrap();
        assert!(
            !re.baseline.get(trash).exists,
            "★ 回收站 baseline 清理必须落盘，否则下一轮克隆又活过来"
        );
        assert!(re.baseline.get(keep).exists);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 名字改了也一样：旧路径删除 + 新路径写入要在一次整份替换里一起落盘。
    #[test]
    fn persist_applies_baseline_renames_when_no_hook_wrote() {
        let d = tmpdir("persist-rename");
        let mut st = SyncState::load(&d).unwrap();
        let old = "/home/old.txt";
        let new = "/home/new.txt";
        st.baseline.put(old, Sig::file(3, 300));
        st.save_all().unwrap();

        let mut snapshot = st.baseline.clone();
        snapshot.remove(old);
        snapshot.put(new, Sig::file(3, 300));

        merge_round_baseline(&mut st, &snapshot, false);
        st.save_all().unwrap();
        let re = SyncState::load(&d).unwrap();
        assert!(!re.baseline.get(old).exists, "旧路径必须落盘删除");
        assert!(re.baseline.get(new).exists, "新路径必须落盘");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 同一秒内两次保存（mtime 相同、size 不同）时，**较新的那份要赢**。
    ///
    /// epoch 秒的粒度下，同一秒连改两次很常见（用户手快 / 脚本）。
    /// 合并规则要能分辨「谁更新」，否则会退回旧版本。
    #[test]
    fn persist_prefers_newer_size_when_mtimes_collide() {
        let d = tmpdir("persist-same-mtime");
        let mut st = SyncState::load(&d).unwrap();
        let p = "/home/a.txt";
        // 内存里已经是第二版（同一秒）
        st.baseline.put(p, Sig::file(85, 2000));
        st.save_all().unwrap();
        // 轮询快照是第一版（同一秒，但内容更旧）
        let mut snapshot = st.baseline.clone();
        snapshot.put(p, Sig::file(90, 2000));
        persist_test(&mut st, &snapshot);
        assert_eq!(
            st.baseline.get(p),
            Sig::file(85, 2000),
            "同秒时应保留内存里较新的（mtime 相等则不覆盖）"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 合并**不能吞掉**轮询这一轮该落盘的真变化（远端真的变了、mtime 更大）。
    ///
    /// 上一条守「别倒退」，这条守「别什么都不做」—— 只认 mtime 大于的话，
    /// 内存里那条会被永远钉住，正常的远端更新就推不上来了。
    #[test]
    fn persist_still_applies_genuinely_newer_remote_signatures() {
        let d = tmpdir("persist-newer");
        let mut st = SyncState::load(&d).unwrap();
        let p = "/home/a.txt";
        st.baseline.put(p, Sig::file(100, 1000));
        st.save_all().unwrap();
        // 轮询真的看到了远端的新版本
        let mut snapshot = st.baseline.clone();
        snapshot.put(p, Sig::file(120, 3000));
        persist_test(&mut st, &snapshot);
        assert_eq!(
            st.baseline.get(p),
            Sig::file(120, 3000),
            "确实更新的远端签名必须能落进来"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// ★ 真机 M2c 引擎测试：远端刷新 / 冲突副本 / baseline / 删除保护 / 游标落盘。
    ///
    /// ```bash
    /// export QXNYC_TEST_HOST=... QXNYC_TEST_USER=... QXNYC_TEST_PASSWORD=...
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
            rules: Arc::new(Rules::temp_only(true)),
            conflict: qxync_core::tasks::DEFAULT_CONFLICT.to_string(),
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
        let r = poll_once(&client, &views, &store, &cfg, &stats, "default", "test1").await;
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

        let r = poll_once(&client, &views, &store, &cfg, &stats, "default", "test1").await;
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
        let r = poll_once(&client, &views, &store, &strict, &stats, "default", "test1").await;
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
        let r = poll_once(&client, &views, &store, &forced, &stats, "default", "test1").await;
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
            assert!(
                g.db_path().exists(),
                "状态库必须存在: {}",
                g.db_path().display()
            );
            assert_eq!(
                g.store.integrity_check().unwrap(),
                "ok",
                "integrity_check 必须是 ok"
            );
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

    /// ★ M7：引擎侧的规则判定（**不需要 NAS**）—— 排除路径与临时文件必须被判为 hidden，
    /// 而且卷根目录本身永远不算 hidden（它是挂载视图）。
    #[test]
    fn m7_mount_view_hidden_semantics() {
        let work = tmpdir("m7-rules");
        let view = FakeLocal::new(HOME_ROOT, work.join("stash"));
        let rules = Rules::parse(&["/secret".to_string()], true).rules;
        let mv = MountView {
            mountpoint: PathBuf::from("/tmp/m7-mnt"),
            remote_root: HOME_ROOT.to_string(),
            view: view.clone(),
            upload: None,
            read_only: true,
            rules: Arc::new(rules),
            conflict: qxync_core::tasks::DEFAULT_CONFLICT.to_string(),
        };
        assert!(mv.hidden("/home/secret", true).is_some());
        assert!(mv.hidden("/home/secret/deep/x.bin", false).is_some());
        assert!(mv.hidden("/home/a.crdownload", false).is_some(), "临时文件");
        assert!(mv.hidden("/home/qxync-test/hello.txt", false).is_none());
        assert!(
            mv.hidden(HOME_ROOT, true).is_none(),
            "根目录是视图本身，不能被隐藏"
        );

        // 默认规则（只有临时过滤）= M1–M6 行为：普通路径一律不隐藏
        let plain = MountView {
            mountpoint: PathBuf::from("/tmp/m7-mnt2"),
            remote_root: HOME_ROOT.to_string(),
            view,
            upload: None,
            read_only: true,
            rules: Arc::new(Rules::temp_only(true)),
            conflict: qxync_core::tasks::DEFAULT_CONFLICT.to_string(),
        };
        assert!(plain.hidden("/home/secret", true).is_none());
        assert!(plain.hidden("/home/x.crdownload", false).is_some());
        let _ = std::fs::remove_dir_all(&work);
    }

    /// ★ M11+：同步引擎必须把**回收站子树**当「不归我管」。
    ///
    /// FUSE 侧本来就不允许操作回收站（`deny_trash`），引擎却曾把它当用户数据逐目录
    /// 对账：真机 baseline 1991 条里 1425 条在 `@Recycle` 下、13 条在 `.Trash-1000` 下，
    /// QTS 清回收站时这些条目会变成批量「远端删除」候选（= 每轮删除熔断刷屏），
    /// FDO 的 `.trashinfo` 还会被反向同步上 NAS。
    #[test]
    fn sync_engine_ignores_trash_subtrees_but_not_lookalikes() {
        let work = tmpdir("trash-hidden");
        let view = FakeLocal::new(HOME_ROOT, work.join("stash"));
        let mv = MountView {
            mountpoint: PathBuf::from("/tmp/trash-mnt"),
            remote_root: HOME_ROOT.to_string(),
            view: view.clone(),
            upload: None,
            read_only: true,
            rules: Arc::new(Rules::temp_only(true)),
            conflict: qxync_core::tasks::DEFAULT_CONFLICT.to_string(),
        };
        let uid = local_uid();

        // 回收站子树：任何深度、任何类型都隐藏，且原因必须是 Trash（不是用户规则）
        for p in [
            format!("{HOME_ROOT}/@Recycle"),
            format!("{HOME_ROOT}/@Recycle/files/x.txt"),
            format!("{HOME_ROOT}/.Trash-{uid}/files/a"),
            format!("{HOME_ROOT}/.Trash/info/a.trashinfo"),
        ] {
            assert_eq!(
                mv.hidden(&p, false),
                Some(HideReason::Trash),
                "{p} 必须按回收站排除"
            );
        }

        // 正常名字一个都不许误伤（误伤 = 用户的文件从此不再同步）
        for p in [
            format!("{HOME_ROOT}/.recent"),
            format!("{HOME_ROOT}/@Recycled"),
            format!("{HOME_ROOT}/a.Trash-1000.txt"),
            format!("{HOME_ROOT}/.trash"),
            format!("{HOME_ROOT}/qxync-test/hello.txt"),
        ] {
            assert!(mv.hidden(&p, false).is_none(), "{p} 不是回收站，不能被排除");
        }
        // 根目录本身永远不隐藏（它是视图本身）
        assert!(mv.hidden(HOME_ROOT, true).is_none());

        // 用户规则 `!@Recycle` 不能把回收站「放回来」：回收站是硬约束，先于用户规则判。
        let overridden = MountView {
            mountpoint: PathBuf::from("/tmp/trash-mnt2"),
            remote_root: HOME_ROOT.to_string(),
            view,
            upload: None,
            read_only: true,
            rules: Arc::new(Rules::parse(&["!@Recycle".to_string()], false).rules),
            conflict: qxync_core::tasks::DEFAULT_CONFLICT.to_string(),
        };
        assert_eq!(
            overridden.hidden(&format!("{HOME_ROOT}/@Recycle/x"), false),
            Some(HideReason::Trash),
            "回收站排除是硬约束，用户规则不能翻回来"
        );
        let _ = std::fs::remove_dir_all(&work);
    }

    // ------------------------------------------------- ★ 巡检：没挂载 ≠ 任务被删

    fn task_with_root(id: &str, root: &str, conflict: &str) -> qxync_core::tasks::Task {
        qxync_core::tasks::Task {
            id: id.into(),
            root: Some(root.into()),
            conflict: conflict.into(),
            ..qxync_core::tasks::Task::default()
        }
    }

    fn decision_row(
        path: &str,
        local_size: u64,
        remote_size: u64,
    ) -> qxync_core::store::DecisionRow {
        qxync_core::store::DecisionRow {
            id: qxync_core::store::DecisionRow::id_for(path),
            path: path.into(),
            task_id: String::new(),
            is_dir: false,
            local_size,
            local_mtime: 100,
            remote_size,
            remote_mtime: 200,
            created_unix: 1_700_000_000,
            resolution: None,
        }
    }

    /// 回收站路径的待裁决行要能识别出来（巡检会直接作废它们：只清队列、不动文件）。
    #[test]
    fn trash_decision_rows_are_recognized_for_queue_cleanup() {
        let uid = local_uid();
        assert!(is_trash_decision_row(&decision_row(
            &format!("{HOME_ROOT}/@Recycle/files/x.txt"),
            1,
            2
        )));
        assert!(is_trash_decision_row(&decision_row(
            &format!("{HOME_ROOT}/.Trash-{uid}/files/x"),
            1,
            2
        )));
        assert!(
            !is_trash_decision_row(&decision_row(&format!("{HOME_ROOT}/.recent"), 1, 2)),
            ".recent 是正常路径，队列行不能误清"
        );
        assert!(!is_trash_decision_row(&decision_row(
            &format!("{HOME_ROOT}/@Recycled/x"),
            1,
            2
        )));
    }

    /// ★ 真机事故面：daemon 刚启动 / 用户卸载时 `views` 为空。旧实现伪造
    /// `task_alive=false` + `remote_now=MISSING` → 巡检第 1 条（远端已不存在）先命中
    /// → **整队待裁决被静默删光**。现在：任务还在（磁盘配置里有人覆盖它）→ 保留。
    #[test]
    fn sweep_keeps_queue_rows_when_task_exists_but_mount_is_absent() {
        use qxync_core::decisions::{sweep_decision, SweepVerdict};
        let configured = ConfiguredTasks {
            tasks: vec![task_with_root("t1", "/home", CONFLICT_ASK)],
            complete: true,
        };
        let baseline = Baseline::default();
        let row = decision_row("/home/.recent", 10, 20);
        let facts = unmounted_row_facts(&row, &configured, &baseline).expect("任务在 → 能给出事实");
        assert!(
            !facts.observed,
            "没有挂载视图 → 不得声称观测到了本地/远端现状"
        );
        assert_eq!(
            sweep_decision(&facts),
            SweepVerdict::Keep,
            "任务还在、只是现在没挂载 → 绝不能删队列行（误删 = 用户裁决意图丢失）"
        );
    }

    /// 确认**没有任何任务覆盖**这条路径时才允许出队（任务真被删了）。
    #[test]
    fn sweep_drops_queue_rows_only_when_no_task_covers_the_path() {
        use qxync_core::decisions::{sweep_decision, SweepVerdict};
        let configured = ConfiguredTasks {
            tasks: vec![task_with_root("t1", "/Public", CONFLICT_ASK)],
            complete: true,
        };
        let baseline = Baseline::default();
        let row = decision_row("/home/orphan.txt", 10, 20);
        let facts = unmounted_row_facts(&row, &configured, &baseline).expect("配置完整 → 能判定");
        assert!(!facts.observed);
        match sweep_decision(&facts) {
            SweepVerdict::Drop(why) => assert!(why.contains("任务已删除"), "{why}"),
            other => panic!("没有任何任务覆盖时才该出队，实际 {other:?}"),
        }
    }

    /// 任务文件解析失败 / 配置目录读不出 → **不能**据此判定「任务已删」，一律保留。
    #[test]
    fn sweep_keeps_rows_when_task_config_is_unreadable() {
        let configured = ConfiguredTasks {
            tasks: Vec::new(),
            complete: false,
        };
        let baseline = Baseline::default();
        let row = decision_row("/home/maybe.txt", 10, 20);
        assert!(
            unmounted_row_facts(&row, &configured, &baseline).is_none(),
            "配置不完整时不能给出「任务已删」的事实 → 调用方保留该行"
        );
    }

    /// 没挂载时也要看**磁盘上任务当前的策略**：不再是 ask → 挂载回来也不读队列 → 出队。
    #[test]
    fn sweep_uses_disk_policy_when_unmounted() {
        use qxync_core::decisions::{sweep_decision, SweepVerdict};
        let configured = ConfiguredTasks {
            tasks: vec![task_with_root("t1", "/home", "rename_local")],
            complete: true,
        };
        let baseline = Baseline::default();
        let row = decision_row("/home/x.txt", 10, 20);
        let facts = unmounted_row_facts(&row, &configured, &baseline).expect("能判定");
        match sweep_decision(&facts) {
            SweepVerdict::Drop(why) => assert!(why.contains("策略已改为"), "{why}"),
            other => panic!("策略不是 ask 应出队，实际 {other:?}"),
        }
    }

    /// 远端根匹配的边界：根下的路径算覆盖，前缀相似的别误配。
    #[test]
    fn path_under_root_matches_only_real_prefixes() {
        assert!(path_under_root("/home", "/home"));
        assert!(path_under_root("/home/a.txt", "/home"));
        assert!(path_under_root("/home/a/b", "/home/"));
        assert!(
            !path_under_root("/home2/a", "/home"),
            "前缀相似但不是子路径"
        );
        assert!(!path_under_root("/Public/a", "/home"));
        assert!(!path_under_root("/Home/a", "/home"), "大小写不同");
    }

    /// 「每个文件都问我」：同一条冲突在用户裁决前每轮都会被重新看到，
    /// 只有**第一次**入队才算新冲突（否则计数器/journal 每 30s 刷同一件事）。
    #[test]
    fn only_first_queue_write_counts_as_a_new_conflict() {
        let row = decision_row("/home/a.txt", 10, 20);
        assert!(conflict_is_new(&None), "第一次入队 = 新冲突");
        assert!(
            !conflict_is_new(&Some(row)),
            "已入队过（还等裁决）→ 不能重复计数 / 重复写 journal"
        );
    }

    // ------------------------------------------------- ★ 对账：视图不完整时暂缓删除

    /// 判据本身：只有「没有任何非 404 列举失败」**且「目录没被扫描上限截断」**
    /// 才允许按远端缺失删本地。
    #[test]
    fn delete_verdict_requires_a_fully_listed_remote_view() {
        // 正常：全部目录都列出来了、总量没超上限 → 删除判定可信（真机实测 dirs=38）
        assert!(delete_verdict_is_reliable(0, 38, 200));
        for n in [1, 3, 200] {
            assert!(
                !delete_verdict_is_reliable(n, 38, 200),
                "{n} 个目录没列出来时绝不许按「远端缺失」删本地"
            );
        }

        // ★ 目录数被截断 = 排序在后的目录**根本没扫**，里面的条目会全部「缺失」。
        //   这一种没有错误、没有失败计数，最容易漏判成「远端已删」→ 真删本地节点。
        assert!(
            !delete_verdict_is_reliable(0, 201, 200),
            "目录数超上限 → 未扫目录下的 baseline 会被当成「远端已删」"
        );
        assert!(!delete_verdict_is_reliable(0, 2001, 200));
        assert!(
            delete_verdict_is_reliable(0, 200, 200),
            "恰好等于上限 = 全都扫了，不算截断"
        );

        // 两种来源同时发生也不可信（任一成立即不可信）
        assert!(!delete_verdict_is_reliable(1, 201, 200));
    }

    /// ★ 数据安全回归（真机级）：对账时目录列举失败（网络错误 / 超时），
    /// 旧逻辑把「没看到」当成「远端已删」→ 把本地干净节点连缓存一起删掉。
    /// 现在必须**跳过删除判定**：节点与 baseline 都保持不动，只记错误 + 说明。
    ///
    /// 用 127.0.0.1:1 造一个必然 `connection refused` 的客户端 —— 不需要真机 NAS。
    #[tokio::test]
    async fn reconcile_never_deletes_locals_when_listing_failed() {
        let work = tmpdir("reconcile-defer");
        let view = FakeLocal::new(HOME_ROOT, work.join("stash"));
        let path = format!("{HOME_ROOT}/qxync-defer/a.txt");
        // 本地干净节点 + baseline 有签名：正常对账里「远端缺失」→ DeleteLocal
        view.add_file(&path, 3, 100, false, None);
        let mut baseline = Baseline::default();
        baseline.put(path.clone(), Sig::file(3, 100));
        let store = Mutex::new(SyncState::load(work.join("state")).unwrap());
        let mut report = SyncReport::default();

        let link = LinkConfig {
            id: "t".into(),
            host: "127.0.0.1".into(),
            port: 1, // 必然 connection refused → 列举失败（非 404）
            https: false,
            insecure: true,
            user: "test1".into(),
            ipv4_only: false,
            exclude: Vec::new(),
            filter_temp: true,
            peer_listen: None,
            peer_name: None,
        };
        let client = Client::new(&link).expect("构造 client 不该需要联网");
        let mv = MountView {
            mountpoint: PathBuf::from("/tmp/reconcile-defer"),
            remote_root: HOME_ROOT.to_string(),
            view: view.clone(),
            upload: None,
            read_only: false,
            rules: Arc::new(Rules::temp_only(true)),
            conflict: qxync_core::tasks::DEFAULT_CONFLICT.to_string(),
        };
        let cfg = SyncConfig::default();
        reconcile_view(&client, &mv, &cfg, &mut baseline, &mut report, &store).await;

        assert!(
            !report.errors.is_empty(),
            "列举失败必须如实记错误: {report:?}"
        );
        assert_eq!(report.deleted, 0, "列举失败时不许删任何东西");
        assert!(
            view.node(&path).is_some(),
            "★ 本地节点必须还在 —— 修复前这里会被当成「远端已删」删掉"
        );
        assert!(baseline.get(&path).exists, "baseline 也不能清");
        assert!(
            report.notes.iter().any(|n| n.contains("跳过删除判定")),
            "要向用户说明为什么这轮没删: {report:?}"
        );
        let _ = std::fs::remove_dir_all(&work);
    }

    /// ★ 任务 4 核心：回收站 baseline 条目要**真正清掉**（落盘、不是每轮复活），
    /// 而且**只清 baseline，绝不动本地文件/节点**。
    ///
    /// 用必然连不上的 client（127.0.0.1:1）跑真实 `reconcile_view`：目录列举会失败，
    /// 但 baseline 的「排除清理」发生在列举之后、不受影响。清理结果再用真实
    /// `merge_round_baseline`（无 hook 写入 → 整份替换）+ `save_all` 落盘，重开库验证。
    #[tokio::test]
    async fn reconcile_purges_trash_baseline_entries_without_touching_files() {
        let work = tmpdir("reconcile-trash");
        let view = FakeLocal::new(HOME_ROOT, work.join("stash"));
        let uid = local_uid();
        let trash_recycle = format!("{HOME_ROOT}/@Recycle/files/old.txt");
        let trash_fdo = format!("{HOME_ROOT}/.Trash-{uid}/files/x.bin");
        let normal = format!("{HOME_ROOT}/notes.txt");
        for p in [&trash_recycle, &trash_fdo, &normal] {
            view.add_file(p, 3, 100, false, None);
        }
        let mut baseline = Baseline::default();
        for p in [&trash_recycle, &trash_fdo, &normal] {
            baseline.put(p.clone(), Sig::file(3, 100));
        }
        let store = Mutex::new(SyncState::load(work.join("state")).unwrap());
        let mut report = SyncReport::default();

        let link = LinkConfig {
            id: "t".into(),
            host: "127.0.0.1".into(),
            port: 1, // 必然 connection refused（本用例只关心 baseline 排除清理）
            https: false,
            insecure: true,
            user: "test1".into(),
            ipv4_only: false,
            exclude: Vec::new(),
            filter_temp: true,
            peer_listen: None,
            peer_name: None,
        };
        let client = Client::new(&link).expect("构造 client 不该需要联网");
        let mv = MountView {
            mountpoint: PathBuf::from("/tmp/reconcile-trash"),
            remote_root: HOME_ROOT.to_string(),
            view: view.clone(),
            upload: None,
            read_only: false,
            rules: Arc::new(Rules::temp_only(true)),
            conflict: qxync_core::tasks::DEFAULT_CONFLICT.to_string(),
        };
        let cfg = SyncConfig::default();
        reconcile_view(&client, &mv, &cfg, &mut baseline, &mut report, &store).await;

        // ① baseline：回收站条目被清，正常条目保留
        assert!(
            !baseline.get(&trash_recycle).exists,
            "@Recycle 下的 baseline 条目必须清掉"
        );
        assert!(
            !baseline.get(&trash_fdo).exists,
            ".Trash-<uid> 下的 baseline 条目必须清掉"
        );
        assert!(baseline.get(&normal).exists, "正常文件不能误清");
        assert!(
            report.notes.iter().any(|n| n.contains("回收站排除")),
            "要说明清理原因: {report:?}"
        );

        // ② 文件/本地节点一个都不许动（排除 ≠ 删除）
        assert!(
            view.node(&trash_recycle).is_some(),
            "只清 baseline，绝不删文件/节点"
        );
        assert!(view.node(&trash_fdo).is_some());
        assert!(view.node(&normal).is_some());

        // ③ 落盘：模拟 poll_once 结束（本轮没有 hook 写入 → 整份替换），重开库应仍干净
        {
            let mut g = store.lock().unwrap();
            merge_round_baseline(&mut g, &baseline, false);
            g.save_all().unwrap();
        }
        let re = SyncState::load(work.join("state")).unwrap();
        assert!(
            !re.baseline.get(&trash_recycle).exists,
            "★ 清理必须落盘：否则下一轮克隆内存 baseline 时回收站条目又活过来"
        );
        assert!(!re.baseline.get(&trash_fdo).exists);
        assert!(re.baseline.get(&normal).exists);
        let _ = std::fs::remove_dir_all(&work);
    }

    // ------------------------------------------------- ★ absorb_round（陈旧告警清理）

    fn report_with_block(count: u64, reason: &str) -> SyncReport {
        SyncReport {
            deletes_blocked: count,
            delete_block_reason: Some(reason.into()),
            ..Default::default()
        }
    }

    /// ★ 真机事故：同一批 526 项删除每 30s 刷一条 WARN + 一条 journal，连续 13 小时。
    /// 现在只有**签名变化**（数量/原因变了）才算「新告警」。
    #[test]
    fn absorb_round_alerts_once_per_block_signature() {
        let stats = SyncStats::default();
        // ① 熔断出现 → 置告警 + 算新告警
        let mut r = report_with_block(526, "526 项 > 阈值");
        absorb_round(&stats, &mut r, true);
        assert!(r.delete_block_changed, "第一次出现必须记一条");
        assert_eq!(
            stats.snapshot().delete_block_reason.as_deref(),
            Some("本轮有 526 项删除被保护挡住")
        );

        // ② 同一签名重复轮次 → 不再算新告警（WARN / journal 不再刷屏）
        let mut r = report_with_block(526, "526 项 > 阈值");
        absorb_round(&stats, &mut r, true);
        assert!(!r.delete_block_changed, "同一批删除的重复轮次不能当新告警");
        assert!(
            stats.snapshot().delete_block_reason.is_some(),
            "告警本身要保留（用户还要看到问题没解决）"
        );

        // ③ 数量变了 → 又是新告警（用户在清理，要能看到进展）
        let mut r = report_with_block(100, "100 项 > 阈值");
        absorb_round(&stats, &mut r, true);
        assert!(r.delete_block_changed, "数量变化算新告警");
    }

    /// ★ 真机事故：`delete_block_reason` 只写不清 → GUI 的熔断告警永远不消失。
    /// 完整轮次没再被挡时必须清掉，并且记一次「已解除」。
    #[test]
    fn absorb_round_clears_stale_delete_alarm_after_a_clean_round() {
        let stats = SyncStats::default();
        let mut r = report_with_block(3, "3 项 > 阈值");
        absorb_round(&stats, &mut r, true);
        assert!(stats.snapshot().delete_block_reason.is_some());

        let mut r = SyncReport::default();
        absorb_round(&stats, &mut r, true);
        assert!(
            r.delete_block_changed,
            "解除也要记一条（journal「已解除」）"
        );
        assert_eq!(
            stats.snapshot().delete_block_reason,
            None,
            "★ 修复前这里永远不清 → GUI 首页/同步/诊断三处红字常驻"
        );
    }

    /// 提前 return 的轮次（max_log 失败 / 服务端未就绪）**没做任何检查**，不许清旧告警。
    #[test]
    fn absorb_round_never_clears_alarms_on_early_return() {
        let stats = SyncStats::default();
        let mut r = report_with_block(3, "3 项 > 阈值");
        absorb_round(&stats, &mut r, true);

        let mut r = SyncReport {
            errors: vec!["qbox_get_max_log 失败: status=3".into()],
            ..Default::default()
        };
        absorb_round(&stats, &mut r, false);
        assert_eq!(
            stats.snapshot().delete_block_reason.as_deref(),
            Some("本轮有 3 项删除被保护挡住"),
            "没检查过 ≠ 检查通过：提前返回的轮次不能清熔断告警"
        );
        assert!(!r.delete_block_changed);
        // 但当前的错误要立刻反映出来
        assert!(stats
            .snapshot()
            .last_error
            .as_deref()
            .unwrap_or("")
            .contains("status=3"));
    }

    /// ★ 真机事故：`last_error` / `last_note` 同样只写不清 →
    /// 错误早在 08:30 恢复，「⚠️ 最近错误」却常驻首页/诊断。
    #[test]
    fn absorb_round_clears_stale_last_error_and_note_on_clean_round() {
        let stats = SyncStats::default();
        let mut r = SyncReport {
            errors: vec!["旧错误".into()],
            notes: vec!["旧提示".into()],
            ..Default::default()
        };
        absorb_round(&stats, &mut r, true);
        assert_eq!(stats.snapshot().last_error.as_deref(), Some("旧错误"));
        assert_eq!(stats.snapshot().last_note.as_deref(), Some("旧提示"));

        let mut r = SyncReport::default();
        absorb_round(&stats, &mut r, true);
        let snap = stats.snapshot();
        assert_eq!(snap.last_error, None, "完整跑完且零错误 → 清掉陈旧错误");
        assert_eq!(
            snap.last_note, None,
            "陈旧提示也要清掉，否则上次的 note 永远常驻"
        );
    }

    // ------------------------------------------------- ★ 会话失效自愈判据

    /// ★ 真机证据（docs/M10 §1）：sid 过期后 `qbox_get_max_log` 回 `status=3`。
    /// status=3 没有码表、也只在 max_log 上观测到 → 判据只给这个端点，不推广。
    #[test]
    fn status_3_on_max_log_is_session_loss_but_not_elsewhere() {
        let e = CoreError::status_msg(3, "qbox_get_max_log", "会话过期");
        assert!(
            max_log_failure_is_session_loss(&e),
            "真机实测：sid 过期后 max_log 回 status=3，不识别就会每 30s 报错到保活才恢复"
        );
        assert!(
            !max_log_failure_is_session_loss(&CoreError::status(3, "qbox_get_sync_log")),
            "status=3 只在 max_log 观测到，别的端点不推广"
        );
        // 4/5 与 Auth 本来就算会话失效（client/daemon/FUSE 三层同一判据）
        assert!(max_log_failure_is_session_loss(&CoreError::status(
            4,
            "qbox_get_max_log"
        )));
        assert!(max_log_failure_is_session_loss(&CoreError::Auth(
            "口令错".into()
        )));
        // 服务端未就绪（status=8）不是会话问题，重登没用
        assert!(!max_log_failure_is_session_loss(&CoreError::status_msg(
            8,
            "qbox_get_max_log",
            "Qsync Central is initializing."
        )));
        assert!(!max_log_failure_is_session_loss(&CoreError::Transport(
            "断网".into()
        )));
    }
}
