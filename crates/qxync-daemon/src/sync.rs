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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// `round_hook_advances` 每路径最多保留的推进签名数（见 [`SyncState::round_hook_advances`]）。
///
/// 正常一轮（约 30s）一个文件也就推进几次；这个上限是给
/// `QXNYC_POLL_INTERVAL=0`（禁用轮询 → 表永不清）兜底的，防止无界增长。
/// 保留的是**最近**的（旧的从头丢弃）：`base_for` 关心的是「远端这次列到的
/// 是不是我们自己传上去的某一版」，越新的越可能被列到。
const MAX_ADVANCES_PER_PATH: usize = 16;

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
    /// 单轮对账的**时间预算**（目录列举阶段的墙钟上限）。
    ///
    /// ★ 时间预算驱动（v0.6.8）：一轮能列多少目录由「这轮有多少时间」决定，而不是
    ///   写死的目录数。预算没用完 → 自然列全（每轮全量，删除判定延迟 = 一个间隔）；
    ///   预算耗尽 → 到点即收，游标续扫（退化为按目录数轮转，但每轮耗时可控）。
    ///   * `Duration::ZERO` = 不限时间（只受 `max_dirs_per_poll` 硬顶约束）。
    ///   * daemon 默认按轮询间隔推导（间隔 × [`SCAN_BUDGET_RATIO_OF_INTERVAL`]）；
    ///     `sync --once` 手动跑一轮给足 [`SCAN_BUDGET_ONCE`]——手动场景就是想全量。
    pub scan_time_budget: Duration,
    /// 单轮最多对账多少个目录（**硬顶**：防目录数爆炸把单轮拖死；时间预算先到就按时间走）。
    ///
    /// `0` = 不限。默认 2000：真机 1439 目录在 600s 间隔下时间预算内就能全量，
    /// 硬顶只是兜底；旧默认 200 会把这种场景硬掰成 8 轮轮转（删除延迟 80 分钟）。
    pub max_dirs_per_poll: usize,
}

/// 时间预算占轮询间隔的比例（daemon 默认推导用）。
///
/// 留一半给本轮其它阶段（事件快路径 / 上传 / 巡检）和喘息，列举不至于背靠背。
pub const SCAN_BUDGET_RATIO_OF_INTERVAL: f64 = 0.5;
/// `sync --once` / GUI「立即同步」的时间预算（手动场景就是想全量，给足但不无限）。
pub const SCAN_BUDGET_ONCE: Duration = Duration::from_secs(600);

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            batch: DEFAULT_LOG_BATCH,
            delete_protection: DeleteProtection::default(),
            force_deletes: false,
            own_devices: Vec::new(),
            scan_time_budget: Duration::ZERO,
            max_dirs_per_poll: 2000,
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
    /// ★ 跳过事件的**原因分布**（`只有对账/事件两条路都说不清时才需要`）。
    ///
    /// 用途：事件快路径整体失效时（真机 2026-10-18：79 670 条事件 stat 全部
    /// `status=5`，因为路径被拼成 `/home/.Qsync/.Qsync/…`），以前症状是刷屏 error；
    /// 4/5/6 降级成 debug 之后，`events_skipped` 是唯一痕迹 —— 光看总数分不清
    /// 「路径映射错了」还是「事件都属于别的任务 / 自己的回声」。这张表跟着
    /// `remote_change` 的 journal 详情一起落库，事后能直接归因。
    pub events_skipped_reasons: BTreeMap<String, u64>,
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
    /// 记一条「跳过一个事件」并归类原因（见 `events_skipped_reasons`）。
    fn skip(&mut self, reason: &'static str) {
        self.events_skipped += 1;
        *self
            .events_skipped_reasons
            .entry(reason.to_string())
            .or_insert(0) += 1;
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
    /// ★ 2026-10-19：本轮轮询开始后，上传 success hook **推进过的** baseline 签名
    /// （路径 → 本轮内按时间顺序的签名列表）。判定时用它覆盖开轮快照（见 [`base_for`]）。
    ///
    /// 为什么需要：`poll_once` 开轮时克隆一份 baseline 快照，整轮（几十秒、要列几百个
    /// 目录）都用它判定；而上传成功 hook 在**本轮进行中**就把权威 baseline 推进了。
    /// 于是判某个文件时：快照里还是旧签名、远端已经是「我们自己刚传上去的新版」→
    /// `remote != base`，节点又被 hook 的 `clear_dirty` 标干净（`dirty == false`）→
    /// 走进 `decide` 的 `(false, true)` 分支，且本地签名 ≠ 旧 base → 判 **Conflict**。
    /// 真机 2026-10-19 08:06 就是这样凭空生成了一个字节完全相同的冲突副本
    /// （`防火墙配置 (conflicted copy from <本机名> 2026-10-19).txt`）。
    ///
    /// ★ 为什么是**列表**而不是单个签名：一轮 20–30 秒，用户可能保存两次（真机那轮
    /// 08:06:23 与 08:06:44 各上传一次），而目录列举完全可能落在两次推进**之间** ——
    /// 此时本轮 `remote_map` 里是**较早**的那版。只记最新签名会让 base 比 remote 新，
    /// 判成 `RefreshRemote` 把本地元数据（乃至内容）刷回旧版。`base_for` 因此要求：
    /// 远端这次列到的只要命中「快照 ∪ 本轮任一推进」就以远端为准（`rc = false`）。
    ///
    /// 生命周期：`begin_round` 在**取快照的同一把锁**里清空 —— 清空之前发生的推进
    /// 已经包含在快照里，清空之后发生的推进记进本表。
    pub round_hook_advances: BTreeMap<String, Vec<Sig>>,
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
            round_hook_advances: BTreeMap::new(),
        })
    }

    /// **开轮**：取「游标 + baseline」快照，并复位本轮的两个 hook 标记。
    ///
    /// 三件事必须在**同一把锁**里做，否则会有竞态窗口：
    /// * 快照 —— 本轮判定用的 baseline；
    /// * `hook_touched = false` —— 本轮结束 `persist` 要不要退回落盘合并；
    /// * `round_hook_advances.clear()` —— 清空「本轮 hook 推进」覆盖表。
    ///
    /// 清空之前发生的 hook 推进已经包含在快照里（同一个锁里先进 `note_uploaded` 才会被
    /// 清掉）；清空之后发生的推进记进新表，`base_for` 会优先用它。
    pub fn begin_round(&mut self) -> (Cursors, Baseline) {
        let snapshot = (self.cursors, self.baseline.clone());
        self.hook_touched = false;
        self.round_hook_advances.clear();
        snapshot
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
        // ★ 记进「本轮 hook 推进」覆盖表：正在跑的那一轮判定这个路径时，快照里还是
        //   旧签名，必须用这里的新签名当 base，否则会凭空判成双方都改（见字段注释）。
        //   同一签名重复推进（幂等调用）只记一次；不同签名按时间顺序追加。
        //   每路径保留最近 [`MAX_ADVANCES_PER_PATH`] 个：正常一轮就几次，但
        //   `QXNYC_POLL_INTERVAL=0`（禁用轮询）时上传照常推进、表永不清空，
        //   不设上限就是无界增长。
        let advances = self
            .round_hook_advances
            .entry(path.to_string())
            .or_default();
        if !advances.iter().any(|a| a.same_as(&sig)) {
            advances.push(sig);
            let overflow = advances.len().saturating_sub(MAX_ADVANCES_PER_PATH);
            for _ in 0..overflow {
                advances.remove(0);
            }
        }
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
    //   `begin_round` 同时清空「本轮 hook 推进」覆盖表，理由同上。
    let (mut cursors, mut baseline) = {
        let mut g = store.lock().unwrap();
        g.begin_round()
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
/// 更权威的理由。
///
/// ★ 2026-10-19 修正：`hook_touched` **只由 `begin_round` 复位**，persist 消费后
/// 不再清零。旧实现在第一次 merge 后清标记 → 本轮**下一次** persist 走整份替换，
/// 用「hook 推进之前克隆的旧快照」把 hook 刚写上去的签名在盘上抹掉（事件阶段每批
/// persist 一次 + 轮末再 persist 一次，上传落在事件阶段就必然命中）→ 本轮内存里
/// 看起来修好了，下一轮开轮快照又回到旧签名 → **同一个凭空冲突晚一轮重现**。
/// 代价是「本轮有 hook 写入」时删除/改名迁移晚一轮落盘 —— 注释开头已经声明
/// 接受这个取舍（宁可晚一轮，不能回退）。
fn merge_round_baseline(g: &mut SyncState, baseline: &Baseline, hook_touched: bool) {
    if !hook_touched {
        g.baseline = baseline.clone();
        return;
    }
    for (p, sig) in baseline.entries.iter() {
        let cur = g.baseline.get(p);
        if sig.mtime > cur.mtime {
            g.baseline.put(p.clone(), *sig);
        }
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
    state_store: &Mutex<SyncState>,
) {
    if !ev.has_usable_path() {
        // 真机实测：删除事件的 filepath 是空的 → 只能靠对账发现
        report.skip("删除事件无路径");
        return;
    }
    if let Some(uid) = &ev.device_uid {
        if cfg.own_devices.iter().any(|u| u == uid) {
            report.skip("本机设备回声");
            return;
        }
    }
    // 归属校验：事件带了 user 就必须和本会话一致（报告 11 §5.3）
    if let Some(u) = &ev.user {
        if !u.is_empty() && u != user {
            report.skip("非本账号");
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
            report.skip("被规则隐藏");
            continue;
        }
        let remote = match stat_path(client, &path).await {
            Ok(s) => s,
            Err(e) => {
                if is_not_found(&e) {
                    // ★ 4/5/6 同时是「权限拒绝 / 会话失效」的码，**不是**硬错误：事件指向
                    //   的路径此刻看不到就先跳过，真要删/改由下一轮对账按目录判定（对账侧
                    //   有 stat 正向确认）。以前这里把每条都记成 error：真机 2026-10-18
                    //   有一轮拉到 87 580 条事件，当天日志里 79 670 条 `事件 stat` 失败
                    //   **全部**是双写路径（79 667 条 status=5）——「最近错误」被刷满，
                    //   真正的问题反而被掩盖。
                    //
                    //   ⚠️ 这里**不能**顺手置 `auth_failed`：`status=5` 也是「路径不存在」
                    //   的常规出口，给一个已删文件的 upsert 事件置 auth_failed 会让轮询
                    //   循环每轮重登一次（`is_auth()` 把 4/5 当会话失效），真机会撞上 NAS
                    //   的失败登录封锁。会话真失效时 `max_log` / 对账那条路会报出来。
                    tracing::debug!("事件 stat {path} 返回不存在/不可见（{e}）→ 跳过，等对账兜底");
                    report.skip("远端不可见(4/5/6)");
                } else {
                    if e.is_auth() {
                        report.auth_failed = true;
                    }
                    report.error(format!("事件 stat {path} 失败: {e}"));
                }
                return;
            }
        };
        let local = local_sig(view, &path);
        // ★ 用「开轮快照 ∪ 本轮上传 hook 推进」当 base：本轮进行中自己刚上传成功的文件，
        //   快照里还是旧签名，直接用快照会凭空判成双方都改（真机 2026-10-19 的冲突副本）。
        let d = decide_with_state(state_store, baseline, &path, &local, &remote);
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
    // 所有视图都映射不上：路径不在任何挂载根之下（别的任务 / 别人的家目录），
    // 或者 `map_event_path` 不认识的形态。这条计数是「事件快路径是否在干活」的哨兵。
    report.skip("不在本视图/映射失败");
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

    // 要对账的目录：挂载视图里已知的目录 + baseline 里出现过的目录 + **候选条目的父目录**。
    //
    // ★ 候选的父目录必须进 `dirs`：判定一条候选前要先「看见」它所在的目录。父目录不在
    //   集合里 → 永远不会被列 → 这条候选永远等不到判定（见下面轮转与按目录判定的注释）。
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
    for p in &candidates {
        let d = parent_dir(p);
        if d != root && view.hidden(&d, true).is_none() {
            dirs.insert(d);
        }
    }
    dirs.insert(root.clone());

    // ★ 大目录轮转 + 时间预算（v0.6.8）：一轮能列多少目录由「这轮有多少时间」决定。
    //   * 预算没用完 → 窗口列完即全量（删除判定延迟 = 一个间隔）；
    //   * 预算耗尽 / 目录数超硬顶 → 到点即收，游标 = 最后一个列过的目录，下轮续扫。
    //   旧坑（v0.6.4 修）：每轮 `take(max)` 从排序开头取，排序在后的目录**永远**
    //   轮不到 → 「本轮暂缓删除判定」永久化。游标轮转保证每个目录一个周期内
    //   至少轮到一次；时间预算让「间隔够长」的场景直接退化成每轮全量。
    //   远端根恒定参与 —— 顶层变化与根下的文件必须每轮都看得见。
    let total_dirs = dirs.len();
    let cursor_key = format!("reconcile_scan_cursor:{}", view.mountpoint.display());
    // ★ 游标**恒定读**（一次 meta 查表，便宜）：游标存在 ⟺ 上轮没列全 —— 可能是
    //   目录数超硬顶，也可能是时间预算耗尽。读不到（含首次）就从头开始。
    let start_after = match state_store.lock().unwrap().store.meta(&cursor_key) {
        Ok(v) => v,
        Err(e) => {
            report.error(format!("读取对账轮转游标失败: {e}"));
            None
        }
    };
    let (scan_window, _planned_next) =
        dirs_for_round(&dirs, &root, cfg.max_dirs_per_poll, start_after.as_deref());

    // 列每个目录 → 远端子项签名表
    let mut remote_map: BTreeMap<String, Sig> = BTreeMap::new();
    let mut scanned = 0usize;
    // ★ 非 404 的列举失败数（网络错误 / 权限 / 服务端忙…）。
    let mut list_failures = 0usize;
    // ★ 本轮**已经看见全集**的目录：列举成功（含空目录）或确定不存在（404）。
    //   只有这些目录下的候选才允许判定 —— 其它目录里的条目只是「没看到」，不是
    //   「远端删了」。这条把旧的「整个视图不完整就整轮不判」收窄到**按目录**：
    //   200 个目录里有 1 个超时，另外 199 个目录的删除照常执行。
    let mut observed_dirs: BTreeSet<String> = BTreeSet::new();
    // ★ 存在性**未知**的目录（列举 4/5/6 且 stat 也失败）：目录条目自身本轮也不能判，
    //   否则「父目录列举漏了它 + stat 抖动」会把整棵子树连缓存一起删。
    let mut unresolved_dirs: BTreeSet<String> = BTreeSet::new();
    // ★ 时间预算驱动的列举：预算耗尽即收（远端根除外——顶层变化必须每轮可见，
    //   根是窗口第一项，正常情况下预算开始时必然还没耗）。
    //
    //   ★ 进度保证：**每轮至少列一个轮转目录**（预算检查从第二个轮转目录开始）。
    //   否则「预算 < 一次列举耗时」的极端配置（间隔 1s → 预算 0.5s，单次列举
    //   600ms）会让根吃光预算、轮转目录一个都列不到、游标也存不下 —— v0.6.4
    //   修过的饿死换个马甲回来。至少推进一个 = 游标每轮前进，最慢也能转完一圈。
    let scan_started = Instant::now();
    // 本轮**尝试过**的最后一个轮转目录（游标按它推进；列举失败的目录也算
    // 「到过这里」——下轮从它之后续扫，回绕一圈后自然会再试它）。
    let mut last_listed: Option<String> = None;
    let mut rotating_listed = 0usize;
    let mut budget_exhausted = false;
    let mut attempted = 0usize;
    for dir in &scan_window {
        if dir != &root
            && rotating_listed >= 1
            && cfg.scan_time_budget > Duration::ZERO
            && scan_started.elapsed() >= cfg.scan_time_budget
        {
            budget_exhausted = true;
            tracing::debug!(
                "对账时间预算 {:?} 耗尽：本轮列了 {attempted}/{total_dirs} 个目录，下轮从游标续扫",
                cfg.scan_time_budget
            );
            break;
        }
        attempted += 1;
        match client.list(dir).await {
            Ok(entries) => {
                scanned += 1;
                observed_dirs.insert(dir.clone());
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
                // ★ 数据安全：`4/5/6` 都不能**直接**当成「目录真的没了」——
                //   4/5 同时是「会话失效 / 权限拒绝」的码（真机：存在但当前会话看不到的
                //   /Download、/homes 就回 5/4）。所以先用一次 stat 正向确认：只有 stat
                //   明确说「不存在」（占位条目 exist=0）才敢把该目录当已删除；stat 说存在
                //   或 stat 也报错 → 按「没看到」处理（该目录本轮不判定；4/5 触发重登）。
                //   远端根例外：**挂载根不可能凭空消失**，4/5/6 一律按权限/会话/卸载处理。
                if dir == &root {
                    list_failures += 1;
                    if e.is_auth() {
                        report.auth_failed = true;
                    }
                    report.error(format!(
                        "对账：远端根 {dir} 列举返回不存在（{e}）→ 本轮不判定（按权限/会话/卸载处理）"
                    ));
                } else {
                    match stat_path(client, dir).await {
                        Ok(sig) if !sig.exists => {
                            observed_dirs.insert(dir.clone());
                            tracing::debug!("对账：目录已不存在（stat 已确认）{dir}");
                            view.view.drop_listing(dir);
                        }
                        Ok(sig) => {
                            // stat 确认目录**还存在**：把它记进 remote_map。
                            // 父目录的列举可能漏掉了它（权限过滤 / ACL 隐藏），此时
                            // 「远端缺失」是假象 —— 目录条目自身若不进 remote_map，
                            // 候选循环会把它判 `DeleteLocal` 连整棵子树 + 缓存一起删。
                            // 注意：只证明「目录还在」，内容没列过 → 不进 observed_dirs，
                            // 目录下的条目本轮仍然不判定。
                            list_failures += 1;
                            remote_map.insert(dir.clone(), sig);
                            tracing::debug!("对账：目录 {dir} 列举不可见但 stat 确认存在");
                            report.error(format!(
                                "对账列举 {dir} 说不存在（{e}），但 stat 显示仍存在 → 本轮不删该目录、不判定其内容"
                            ));
                        }
                        Err(e2) => {
                            list_failures += 1;
                            // stat 也失败：目录是否存在**未知** → 目录条目自身也不能判。
                            if e2.is_auth() {
                                report.auth_failed = true;
                            }
                            unresolved_dirs.insert(dir.clone());
                            report.error(format!(
                                "对账列举 {dir} 失败（{e}），stat 确认也失败（{e2}）→ 本轮不判定该目录及其内容"
                            ));
                        }
                    }
                }
            }
            Err(e) => {
                // ★ 这不是「远端不存在」，而是**我们没看到**。记错误；该目录下的候选
                //   本轮不判定（由下面按目录的 `observed_dirs` 决定）。
                list_failures += 1;
                if e.is_auth() {
                    report.auth_failed = true;
                }
                report.error(format!("对账列举 {dir} 失败: {e}"));
            }
        }
        if dir != &root {
            rotating_listed += 1;
            last_listed = Some(dir.clone());
        }
    }
    report.dirs_scanned += scanned;
    // ★ 游标维护（时间预算版）：
    //   本轮把计划窗口列完、且窗口覆盖全部目录 → **清游标**（下轮从头全量）；
    //   否则（硬顶截断 / 预算耗尽）→ 游标 = 本轮尝试过的最后一个目录。
    //   「列完全部」时必须清掉旧游标：不清的话下轮带着游标进轮转模式，白转一圈。
    //   进度保证（每轮至少列一个轮转目录）确保没列全时 `last_listed` 必然有值。
    let covered_all = !budget_exhausted && scan_window.len() >= total_dirs;
    if covered_all {
        if let Err(e) = state_store.lock().unwrap().store.delete_meta(&cursor_key) {
            report.error(format!("清除对账轮转游标失败: {e}"));
        }
    } else if let Some(last) = &last_listed {
        if let Err(e) = state_store
            .lock()
            .unwrap()
            .store
            .set_meta(&cursor_key, last)
        {
            report.error(format!("保存对账轮转游标失败: {e}"));
        }
    }
    // 本轮视图不完整的来源（用于下面「哪些条目本轮暂不判定」的说明）。
    let mut partial_why: Vec<String> = Vec::new();
    if list_failures > 0 {
        partial_why.push(format!(
            "{list_failures} 个目录本轮没列成（失败/不可见/存在性未确认）"
        ));
    }
    if budget_exhausted {
        partial_why.push(format!(
            "对账时间预算 {:?} 耗尽：本轮列了 {attempted}/{total_dirs} 个目录，下轮从游标续扫",
            cfg.scan_time_budget
        ));
    }
    if scan_window.len() < total_dirs {
        partial_why.push(format!(
            "目录数 {total_dirs} 超过单轮硬顶 {}（本轮按轮转列了 {} 个）",
            cfg.max_dirs_per_poll,
            scan_window.len()
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
    let renames = collect_renames(
        view,
        baseline,
        &candidates,
        &remote_map,
        &observed_dirs,
        &unresolved_dirs,
        report,
    );
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

    // ★ 数据安全阀（按目录）：一条候选只有在**它所在的目录本轮被看见**时才允许判定。
    //
    //   旧实现是「整个视图不完整 → 整轮不删」，而「目录数超过单轮上限」又每轮都成立
    //   （固定顺序从开头扫）→ 删除判定被永久推迟。现在视图不完整按目录收窄：
    //   没列到的目录里的条目不参与任何判定（不上传、不刷新、更不删），列到的目录照常
    //   判定与删除。轮转保证每个目录都会轮到，不会再有「永远等不到」的条目。
    let mut deletes: Vec<String> = Vec::new();
    // 本轮因为「所在目录没列到」而推迟的候选数（轮转没轮到 / 列举失败）。
    let mut deferred = 0usize;
    for path in &candidates {
        if renamed_set.contains(path) {
            continue;
        }
        // ★ 存在性未知的目录条目自身也不能判（stat 也失败的 4/5/6 目录）。
        //   否则父目录列举漏掉它时，会把它连整棵子树当「远端已删」。
        if unresolved_dirs.contains(path) {
            deferred += 1;
            tracing::trace!("对账：目录存在性未确认，推迟判定 {path}");
            continue;
        }
        // 「父目录本轮看见全集」= 列举成功或确定 404。两者之外都只有「没看到」这一个事实。
        if !observed_dirs.contains(&parent_dir(path)) {
            // 若它的某个祖先是本轮待删目录（`remove_remote` 会连子树一起删掉这条候选），
            // 就不计入「暂不判定」的条数 —— 否则数字会被即将消失的路径虚高。
            if deletes.iter().any(|d| path.starts_with(&format!("{d}/"))) {
                continue;
            }
            deferred += 1;
            tracing::trace!("对账：父目录本轮没列到，推迟判定 {path}");
            continue;
        }
        let remote = remote_map.get(path).copied().unwrap_or(Sig::MISSING);
        let local = local_sig(view, path);
        // ★ 统一走 `decide_with_state`（快照 ∪ 本轮 hook 推进当 base）：
        //   否则「自己刚传上去的新版」会被开轮快照误判成双方都改（凭空冲突副本）。
        //   这里单独取一次 base 只是为了 debug 日志能打出三方签名。
        let base = base_for(state_store, baseline, path, &remote, local.dirty);
        let d = decide(&local, &base, &remote);
        debug_assert_eq!(
            d,
            decide_with_state(state_store, baseline, path, &local, &remote),
            "对账判定必须与 decide_with_state 一致（防止有人改回 baseline.get）"
        );
        if d != Decision::Noop {
            tracing::debug!(
                "决策 {path}: local={:?} base={base:?} remote={remote:?} → {d:?}",
                local
            );
        }
        match d {
            Decision::DeleteLocal => deletes.push(path.clone()),
            _ => {
                apply_decision(client, view, path, d, remote, baseline, report, state_store).await;
            }
        }
    }
    // 只在「确实有东西被推迟」或「有目录列举失败」时说明；单纯轮转不是告警，
    // 不往 `last_note` 里常驻一条看似出错的提示。
    if !partial_why.is_empty() && (deferred > 0 || list_failures > 0) {
        let tail = if deferred > 0 {
            format!("；未列到的目录下 {deferred} 项本轮暂不判定（不按缺失删本地）")
        } else {
            String::new()
        };
        report.note(format!("对账：{}{tail}", partial_why.join("；")));
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

/// 一轮对账要列的目录（**大目录轮转**的核心，纯函数便于单测）。
///
/// * 无游标 且（`max == 0` 或 `dirs.len() <= max`）→ **全列**（0 是「没配上限」的
///   直觉含义；不能让它退化成「一轮什么都不列」——那就是要消灭的「永久暂不判定」）。
///   返回的游标为 `None`。
/// * 否则 → 轮转模式：**远端根恒定参与**（顶层变化 / 根下文件必须每轮可见），
///   其余目录从 `start_after` 严格之后按排序取（`max-1` 个，`max == 0` 不限），
///   到尾再从头回绕。返回的第二个值是窗口的最后一个目录（计划中的下一轮起点）。
///   `max == 1` 是退化配置：根 + 1 个轮转目录（超上限 1 个）也要保证轮转不饿死。
///
/// ★ 游标存在就进轮转（v0.6.8 起不再要求「目录数 > max」）：时间预算耗尽同样会留
///   游标，那时目录数可能远没超硬顶 —— 若忽略游标从头开始，排序在后的目录在时间
///   预算下永远轮不到（v0.6.4 修过的饿死，换个马甲回来）。
///
/// 为什么必须轮转：旧实现每轮都 `take(max)` 排序最前的目录，目录数一旦超过上限，
/// 排序在后的目录**永远**轮不到；而「未扫到的目录里的条目本轮不判定」这条安全阀
/// 也随之永久生效 → 删除永远不执行。轮转保证每个目录在一个周期内至少被列一次。
fn dirs_for_round(
    dirs: &BTreeSet<String>,
    root: &str,
    max: usize,
    start_after: Option<&str>,
) -> (Vec<String>, Option<String>) {
    let all: Vec<String> = dirs.iter().cloned().collect();
    if start_after.is_none() && (max == 0 || all.len() <= max) {
        // 常规路径：一轮列全（注：root 必然在 dirs 里）；max=0 视为不限。
        return (all, None);
    }
    let mut out: Vec<String> = Vec::with_capacity(max.max(2));
    out.push(root.to_string());
    let rotating: Vec<String> = all.into_iter().filter(|d| d != root).collect();
    let budget = if max == 0 {
        usize::MAX
    } else {
        max.saturating_sub(1).max(1)
    };
    let (mut window, next) = pick_scan_window(&rotating, start_after, budget);
    out.append(&mut window);
    (out, next)
}

/// 从**已排序**的 `items` 里取 `max` 个：先取 `start_after` 严格之后的，不够再从头回绕。
///
/// 返回（窗口，下一轮起点 = 窗口最后一项）。`start_after` 指向的目录可能已经不存在
/// （用户在两次轮询之间删了它）—— 按字符串序定位仍然有效，不会丢一个周期。
/// ★ 回绕补齐**不超过全集**（`max ≥ items.len()` 时窗口 = 全集本身的旋转序）：
///   v0.6.8 起游标存在就进轮转，预算可能大于全集 —— 重复塞目录纯属浪费一轮。
fn pick_scan_window(
    items: &[String],
    start_after: Option<&str>,
    max: usize,
) -> (Vec<String>, Option<String>) {
    if max == 0 || items.is_empty() {
        return (Vec::new(), None);
    }
    let start = match start_after {
        Some(after) => items.partition_point(|d| d.as_str() <= after),
        None => 0,
    };
    let mut out: Vec<String> = items[start..].iter().take(max).cloned().collect();
    if out.len() < max {
        let need = (max - out.len()).min(items.len() - out.len());
        out.extend(items.iter().take(need).cloned());
    }
    let next = out.last().cloned();
    (out, next)
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
///
/// **新增侧永远安全**（`remote_map` 里的条目只来自本轮真正列过的目录）；
/// **已删侧必须过滤**：`candidates` 里可能有「父目录本轮没列到」的条目 —— 那只是
/// 没看到，不是远端已删，拿它去和新增侧配对会凭空造出一次改名。
fn collect_renames(
    view: &MountView,
    baseline: &Baseline,
    candidates: &BTreeSet<String>,
    remote_map: &BTreeMap<String, Sig>,
    observed_dirs: &BTreeSet<String>,
    unresolved_dirs: &BTreeSet<String>,
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
        // ★ 父目录本轮没看见全集 → 「远端已删」不成立，不许拿它去配改名。
        if !observed_dirs.contains(&parent_dir(path)) {
            continue;
        }
        // ★ 目录存在性未确认（stat 也失败的 4/5/6 目录）→ 不能参与改名配对。
        if unresolved_dirs.contains(path) {
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
            // ★ 重判同样统一走 `decide_with_state`：这里的场景正是「在途上传刚落地」，
            //   权威 baseline 刚被 hook 推进，用旧快照重判会二次误判成冲突。
            let d2 = decide_with_state(state_store, baseline, path, &local2, &r2);
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

/// 判定用的 base：**本轮上传 hook 推进过的路径**以 hook 的签名优先，否则用开轮快照。
///
/// 快照落后于权威 baseline 的唯一窗口就是「本轮进行中上传成功」——
/// [`SyncState::round_hook_advances`] 里存着 hook 刚推进的签名。不用它当 base，
/// 就会出现真机那个凭空冲突：远端是自己刚传上去的新版、base 还是开轮前的旧版、
/// 本地已被 `clear_dirty` 标干净 → `decide` 判 `Conflict`。
///
/// 但有一个**反向**的窗口必须让快照赢：本轮列目录可能发生在上传落地**之前**
/// （`remote_map` 是本轮列举结果快照），这时 `remote == 开轮快照里的旧签名`，
/// 若仍用 hook 的新签名当 base，`(lc=false, rc=true)` 会判 `RefreshRemote` 把本地
/// 元数据刷新回**旧版**（内容其实已经是新版）—— 所以「远端这次列到的就是快照里的
/// 那一版」时以快照为准，`Noop`，下一轮再按权威 baseline 收敛。
///
/// ⚠️ `snapshot` 实参是**本轮可变的 baseline 工作副本**（轮内决策会 `put`，如改名迁移 /
/// AdoptBaseline / RefreshRemote 落地），不是冻结的开轮快照。同一路径每轮只判一次，
/// 所以对本函数语义等价；但它不是「历史快照」，别拿它当跨路径/跨时点的一致性基准。
///
/// `local_dirty`：本地节点此刻是否带未上传改动。**「远端比本轮最后一次推进还旧」
/// 这条捷径只在本地干净时才允许**（真机事故正是 clean 场景）：本地一脏，`base=remote`
/// 会把 `decide` 推进 `(lc=true, rc=false) => UploadLocal` —— 别处设备写回的旧版
/// （时钟偏斜 / `rsync -t` / 备份恢复都会造成 mtime 偏小的**新写入**）会被本地覆盖，
/// 既不生成冲突副本也不进待裁决队列，是丢数据。脏时退回 `last` 当 base，让
/// `(true, true)` 走 Conflict 把双方都保下来。
fn base_for(
    state_store: &Mutex<SyncState>,
    snapshot: &Baseline,
    path: &str,
    remote: &Sig,
    local_dirty: bool,
) -> Sig {
    let snap = snapshot.get(path);
    let g = state_store.lock().unwrap();
    let Some(advances) = g.round_hook_advances.get(path) else {
        return snap;
    };
    // 远端这次列到的就是「开轮快照」或「本轮我们自己传上去的某一版」→ 用它当 base：
    // `rc = false`，剩下的只看本地脏不脏。列举可能发生在本轮多次上传之间的任意时刻，
    // 所以这里要匹配**整张列表**，不能只比最新那次（否则会把较早的 in-round 版本
    // 判成 `RefreshRemote`，把本地元数据/内容刷回旧版）。
    if snap.same_as(remote) || advances.iter().any(|a| a.same_as(remote)) {
        return *remote;
    }
    let last = match advances.last() {
        Some(sig) => *sig,
        None => return snap,
    };
    // 列举比「我们本轮最后一次推进」还旧（mtime 更小），说明 `remote_map` 是在那次
    // 上传落地之前列的（别处设备的版本或旧版快照都可能这样进来）—— 拿它刷新本地同样
    // 是回刷旧版，本轮 Noop，下一轮用权威 baseline 收敛。
    //
    // ★ 但这条捷径**只在本地干净时成立**（Noop 的前提就是 local 没有要保的改动）；
    //   本地脏时必须退回 `last`，把判定交给 `(lc=true, rc=true)` 的 Conflict ——
    //   「mtime 更小的远端」也可能是别处设备**刚写**的旧钟版本，覆盖它就是丢数据。
    if !local_dirty
        && !remote.is_dir
        && !last.is_dir
        && (!remote.exists || remote.mtime <= last.mtime)
    {
        return *remote;
    }
    last
}

/// `decide` + 「本轮 hook 推进覆盖开轮快照」的统一入口。
///
/// 对账、事件、冲突重判**都必须**走这里，不要直接 `decide(local, snapshot.get(path), remote)`
/// —— 三个调用点（`apply_event` / `reconcile_view` / `resolve_conflict` 重判）共用
/// 同一套 base 规则，任何一处绕开都会重新引入「快照 vs 权威 baseline」的时序分叉。
fn decide_with_state(
    state_store: &Mutex<SyncState>,
    snapshot: &Baseline,
    path: &str,
    local: &LocalSig,
    remote: &Sig,
) -> Decision {
    decide(
        local,
        &base_for(state_store, snapshot, path, remote, local.dirty),
        remote,
    )
}

/// 服务端「这个路径看不到 / 不存在」的 status：4（权限拒绝）、5（路径不存在/无权限）、
/// 6（不存在）。**它只是候选**，不能单独当删除依据 —— 4/5 同时用于「会话失效」，所以
/// 调用方必须再用 `stat` 正向确认 `exist=0` 才允许按「远端已删」处理（见 `reconcile_view`）。
/// 其它错误（网络/超时/解析）绝不能当成删除。
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

        /// 加一个目录节点（用于「目录条目自身被误删」类回归）。
        fn add_dir(&self, remote: &str) {
            let name = remote.rsplit('/').next().unwrap_or("").to_string();
            self.nodes.lock().unwrap().insert(
                remote.to_string(),
                LocalNode {
                    remote: remote.to_string(),
                    name,
                    is_dir: true,
                    size: 0,
                    mtime: 0,
                    dirty: false,
                    cache: None,
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

    /// ★ 回归（真机 2026-10-19 08:06 的凭空冲突副本）：
    /// **一轮进行中**上传成功、hook 把 baseline 推进到新签名，而本轮判定还拿着开轮快照。
    ///
    /// 真实时序（`防火墙配置.txt`，12218 字节，任务 `root=/home/.Qsync`；时间取自
    /// 10-19 daemon 日志 + sync.db 的 baseline）：
    ///   08:05:56.2  上传 12218 落地 → hook 推进 base=(12218, 1792368356)〔推断：DB 无历史〕
    ///   08:06:21.7  开轮：快照 base = (12218, 1792368356)〔推断〕
    ///   08:06:23.6  本轮内再保存 → 上传落地 → hook 推进〔实测有这次上传，签名推断〕
    ///   08:06:44.6  本轮内再保存 → 上传落地 → hook 推进 (12218, 1792368403)〔实测：DB 终值〕
    ///   08:06:46.9  reconcile 判这个文件：快照 base 还是旧的、远端是刚落的新版、
    ///               本地已干净 → `remote != base`、`local != base` → **Conflict**
    ///
    /// 修法：判定用 `base_for`（快照 ∪ 本轮 hook 推进的**全部**签名），不应再用纯快照。
    #[test]
    fn hook_advance_during_round_prevents_phantom_conflict() {
        let d = tmpdir("round-hook-advance");
        let path = "/home/.Qsync/2026/28-比赛项目/01配置备份/防火墙配置.txt";
        let store = Mutex::new(SyncState::load(&d).unwrap());
        // 开轮前最后一次推进（实测 DB 终值之前的版本；具体 mtime 是推断值）
        let snap_base = Sig::file(12218, 1792368356);
        // 本轮两次保存各自的落点：a2 = DB 实测终值（1792368403 = 08:06:43）；
        // a1 是**合成值**，只用来构造「列举落在两次推进之间」这个窗口
        // （真机那次 remote 到底是哪一版无法从日志反推，日志只证明 08:06:23 有上传）。
        let a1 = Sig::file(12218, 1792368381);
        let a2 = Sig::file(12218, 1792368403);

        // 开轮：快照里是上一版的合法 base
        {
            let mut g = store.lock().unwrap();
            g.baseline.put(path, snap_base);
            g.save_all().unwrap();
        }
        let snapshot = {
            let g = store.lock().unwrap();
            g.baseline.clone()
        };

        // 本轮进行中：两次上传成功 → hook 推进权威 baseline（并记进本轮覆盖表）
        {
            let mut g = store.lock().unwrap();
            g.note_uploaded(path, a1.size, a1.mtime).unwrap();
            g.note_uploaded(path, a2.size, a2.mtime).unwrap();
        }

        // 本地节点此刻是最后一次上传的版本、dirty 已被 hook 清掉
        let local = LocalSig {
            sig: a2,
            dirty: false,
        };

        // ---- ① 远端列表看到最新那版（列举在第二次上传之后）
        assert_eq!(
            decide_with_state(&store, &snapshot, path, &local, &a2),
            Decision::Noop,
            "本轮 hook 已推进到 (12218, {}) 时不能再判冲突",
            a2.mtime
        );
        // 对照：只拿开轮快照判（修复前的行为）就是那次真机冲突
        assert_eq!(
            decide(&local, &snapshot.get(path), &a2),
            Decision::Conflict,
            "旧快照 + 自己刚上传落地的新版 = 凭空冲突副本"
        );

        // ---- ② 远端列表看到**较早**的那次本轮上传（列举落在两次上传之间）
        assert_eq!(
            decide_with_state(&store, &snapshot, path, &local, &a1),
            Decision::Noop,
            "远端是本轮自己传过的某一版 → rc 必须为 false（不能回刷）"
        );
        // 对照：只认「最新一次推进」当 base 就会 RefreshRemote，把本地刷回 a1
        assert_eq!(decide(&local, &a2, &a1), Decision::RefreshRemote);

        // ---- ③ 远端列表还停在开轮前的快照（列举早于本轮第一次上传落地）
        assert_eq!(
            decide_with_state(&store, &snapshot, path, &local, &snap_base),
            Decision::Noop,
            "列举早于上传落地 → 本轮 Noop，下一轮按权威 baseline 收敛"
        );
        assert_eq!(decide(&local, &a2, &snap_base), Decision::RefreshRemote);

        // ---- ④ 远端列表比「本轮最后一次推进」还旧（别处设备写回旧版 / 陈旧列表）
        let older = Sig::file(12218, 1792368340);
        assert_eq!(
            decide_with_state(&store, &snapshot, path, &local, &older),
            Decision::Noop,
            "比本轮推进还旧的远端版本不能拿来刷新本地"
        );
        assert_eq!(decide(&local, &a2, &older), Decision::RefreshRemote);

        // ---- ⑤ 远端确实是别的设备改的新版（mtime 更新）→ 正常刷新
        let other = Sig::file(12218, 1792368500);
        assert_eq!(
            decide_with_state(&store, &snapshot, path, &local, &other),
            Decision::RefreshRemote,
            "别处设备的新版本必须照常刷新"
        );

        // ---- ⑥ ★ 复审补的丢数据用例：本地**脏** + 远端是「mtime 偏小的新写入」
        //   （别处设备时钟慢 / `rsync -t` / 备份恢复）。这条捷径若不限制
        //   `local_dirty`，base=older → `(lc=true, rc=false)` → UploadLocal，
        //   本地直接覆盖别处设备的写入、不出冲突副本 —— 丢数据。
        //   期望：退回 last 当 base → `(true, true)` → Conflict（双方都保留）。
        let dirty_local = LocalSig {
            sig: Sig::file(12218, 1792368410),
            dirty: true,
        };
        assert_eq!(
            decide_with_state(&store, &snapshot, path, &dirty_local, &older),
            Decision::Conflict,
            "本地脏 + 远端旧钟新写入 → 必须判冲突，不能 UploadLocal 覆盖"
        );
        // 对照：本地干净时同一条 remote 仍走 Noop（④ 已锁），说明限制条件只收窄了
        // dirty 场景、没有破坏真机事故（clean）的修复。
        assert_eq!(
            decide_with_state(&store, &snapshot, path, &local, &older),
            Decision::Noop
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `begin_round` 的生命周期：**清空**本轮覆盖表，但上一轮的推进已经折进快照。
    ///
    /// 竞态窗口由「同一把锁」消除：清空前发生的推进在快照里可见；清空后发生的推进
    /// 记进新表，`base_for` 优先用它。列表语义也在这里一起锁住：同一轮多次推进
    /// 按顺序保留、同一签名去重。
    #[test]
    fn begin_round_folds_previous_advances_into_snapshot_and_resets_table() {
        let d = tmpdir("begin-round");
        let path = "/home/a.txt";
        let store = Mutex::new(SyncState::load(&d).unwrap());
        let v1 = Sig::file(1, 100);
        let v2 = Sig::file(2, 200);
        let v3 = Sig::file(3, 300);
        let v4 = Sig::file(4, 400);
        {
            let mut g = store.lock().unwrap();
            g.baseline.put(path, v1);
        }
        // 上一轮进行中 hook 推进 v2（还留在覆盖表里）
        store
            .lock()
            .unwrap()
            .note_uploaded(path, v2.size, v2.mtime)
            .unwrap();

        let (_, snapshot) = store.lock().unwrap().begin_round();
        assert!(
            store.lock().unwrap().round_hook_advances.is_empty(),
            "开轮必须清空覆盖表"
        );
        assert_eq!(snapshot.get(path), v2, "上一轮的推进已经折进快照");
        assert_eq!(base_for(&store, &snapshot, path, &v2, false), v2);

        // 本轮进行中又推进 v3、v4（v3 重复调用幂等）→ 列表按顺序保留
        {
            let mut g = store.lock().unwrap();
            g.note_uploaded(path, v3.size, v3.mtime).unwrap();
            // 第二次推 v3：baseline 已等于它 → `note_uploaded` 的幂等早退分支，
            // 连覆盖表都不进（早退 = 这次没有推进，v3 已在表里）。
            g.note_uploaded(path, v3.size, v3.mtime).unwrap();
            g.note_uploaded(path, v4.size, v4.mtime).unwrap();
            // 再推一次 v3（非最新）：走 Vec 的去重分支（`any(same_as)` 命中），
            // 不追加 —— 这是「同一签名去重」真正生效的唯一路径。
            g.note_uploaded(path, v3.size, v3.mtime).unwrap();
            assert_eq!(
                g.round_hook_advances.get(path).map(Vec::len),
                Some(2),
                "同一签名去重（幂等早退 + Vec 去重两条路都不多记）"
            );
        }
        // 远端列到 v3（两次推进之间）→ base=v3；列到 v4 → base=v4
        assert_eq!(base_for(&store, &snapshot, path, &v3, false), v3);
        assert_eq!(base_for(&store, &snapshot, path, &v4, false), v4);
        // 没被推进过的路径仍然用快照
        assert_eq!(
            base_for(&store, &snapshot, "/home/other.txt", &Sig::MISSING, false),
            Sig::MISSING
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `MAX_ADVANCES_PER_PATH`：`QXNYC_POLL_INTERVAL=0`（禁用轮询 → 覆盖表永不清空）
    /// 时每路径的推进列表不能无界增长 —— 超过上限丢**最旧**的，最新的（最可能被
    /// 下一轮列举看到）保留。
    #[test]
    fn round_hook_advances_are_capped_per_path() {
        let d = tmpdir("advances-cap");
        let mut st = SyncState::load(&d).unwrap();
        let path = "/home/a.txt";
        for i in 0..(MAX_ADVANCES_PER_PATH + 8) {
            st.note_uploaded(path, 10 + i as u64, 1000 + i as i64)
                .unwrap();
        }
        let advances = st.round_hook_advances.get(path).unwrap();
        assert_eq!(
            advances.len(),
            MAX_ADVANCES_PER_PATH,
            "每路径推进列表必须封顶"
        );
        assert_eq!(
            advances.last(),
            Some(&Sig::file(
                10 + (MAX_ADVANCES_PER_PATH + 7) as u64,
                1000 + (MAX_ADVANCES_PER_PATH + 7) as i64
            )),
            "最新的推进必须保留"
        );
        assert_eq!(
            advances.first(),
            Some(&Sig::file(10 + 8_u64, 1008)),
            "超限丢的是最旧的"
        );
        let _ = std::fs::remove_dir_all(&d);
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

    /// ★ 回归（2026-10-19 复审发现的「晚一轮重现」）：一轮里 persist 会发生**多次**
    /// （事件阶段每批一次 + 轮末一次）。旧实现第一次 merge 后把 `hook_touched` 清零，
    /// 第二次 persist 就走整份替换，用「hook 推进之前克隆的旧快照」把 hook 刚写上去的
    /// 签名在盘上抹掉 —— 本轮内存里看着修好了，下一轮开轮快照又回到旧签名，
    /// 同一个凭空冲突晚一轮重现。
    ///
    /// 期望：同一轮内**任意多次** persist 都不能让签名回退；重新 load 后仍是 hook 的值。
    #[test]
    fn persist_keeps_hook_advances_across_repeated_persists_in_one_round() {
        let d = tmpdir("persist-twice");
        let mut st = SyncState::load(&d).unwrap();
        let path = "/home/a.txt";

        // 开轮快照（陈旧）+ 权威 baseline 同步播种
        let mut snapshot = st.baseline.clone();
        snapshot.put(path, Sig::file(100, 1000));
        st.baseline.put(path, Sig::file(100, 1000));

        // 本轮进行中：上传成功 → hook 推进（并置起 hook_touched）
        st.note_uploaded(path, 90, 2000).unwrap();

        // ① 事件阶段的 persist（merge 模式）
        persist_test(&mut st, &snapshot);
        // ② 轮末的 persist —— 旧实现这里 hook_touched 已被 ① 清零 → 整份替换 → 回退
        persist_test(&mut st, &snapshot);
        assert_eq!(
            st.baseline.get(path),
            Sig::file(90, 2000),
            "同一轮第二次 persist 不能把 hook 推进抹回去"
        );
        // ③ 盘上也要是 hook 的值：下一轮开轮快照才不会回到旧签名
        st.save_all().unwrap();
        let re = SyncState::load(&d).unwrap();
        assert_eq!(
            re.baseline.get(path),
            Sig::file(90, 2000),
            "★ 重新 load 后必须仍是 hook 推进的签名（否则冲突晚一轮重现）"
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

    // ------------------------------------------------- ★ 对账：大目录轮转 + 按目录判定

    /// 轮转窗口：目录数超过单轮上限时，远端根每轮都在，其余按游标推进，
    /// 一个周期内覆盖全部目录 —— **不允许有目录永远排在窗口之外**。
    ///
    /// 修前：`dirs.iter().take(200)` 顺序固定，228 个目录里排序在后的 28 个每轮
    /// 都轮不到；「未扫到的目录本轮不判定」因此永久生效，删除永远不执行。
    #[test]
    fn scan_window_rotates_and_covers_every_directory() {
        let root = HOME_ROOT;
        let mut dirs: BTreeSet<String> = BTreeSet::new();
        dirs.insert(root.to_string());
        for i in 0..228 {
            dirs.insert(format!("{root}/d{i:03}"));
        }
        let (w1, c1) = dirs_for_round(&dirs, root, 200, None);
        assert_eq!(w1.len(), 200, "上限 200 = 根 + 199 个轮转目录");
        assert_eq!(w1[0], root, "远端根必须每轮都列");
        let c1 = c1.expect("超过上限必须有下一轮游标");
        let set1: BTreeSet<&String> = w1.iter().collect();
        assert_eq!(set1.len(), w1.len(), "窗口内不能有重复目录");

        let (w2, _) = dirs_for_round(&dirs, root, 200, Some(c1.as_str()));
        assert_eq!(w2.len(), 200);
        let union: BTreeSet<&String> = w1.iter().chain(w2.iter()).collect();
        assert_eq!(
            union.len(),
            dirs.len(),
            "两轮窗口必须覆盖全部 {} 个目录（旧实现排序在后的 28 个永远轮不到）",
            dirs.len()
        );

        // ★ 游标存在（上轮时间预算耗尽 / 硬顶截断留下的）→ 即使目录数没超硬顶也按
        //   游标续扫：预算够大时窗口回绕列全（顺序从游标之后开始），返回下一轮起点。
        //   忽略游标从头开始的话，排序在后的目录在时间预算下永远轮不到。
        let (all, next) = dirs_for_round(&dirs, root, 1000, Some(c1.as_str()));
        assert_eq!(all.len(), dirs.len(), "预算够大时窗口回绕列全");
        assert!(next.is_some(), "带着游标进轮转模式，要返回下一轮起点");

        // 没有游标 + 没超硬顶 → 一轮全列、游标为空（与旧行为一致）
        let (all2, none2) = dirs_for_round(&dirs, root, 1000, None);
        assert_eq!(all2.len(), dirs.len());
        assert_eq!(none2, None, "没游标没超上限就不需要轮转");

        // max=0 = 不限（全列）；不能让退化配置变成「一轮什么目录都不列」
        let (all0, next0) = dirs_for_round(&dirs, root, 0, None);
        assert_eq!(all0.len(), dirs.len());
        assert_eq!(next0, None);

        // max=1 退化配置：根 + 1 个轮转目录（略超上限），也必须能轮转、不永久饿死
        let (one1, c1) = dirs_for_round(&dirs, root, 1, None);
        assert_eq!(one1[0], root);
        assert_eq!(one1.len(), 2);
        let (one2, _) = dirs_for_round(&dirs, root, 1, c1.as_deref());
        assert_ne!(one1[1], one2[1], "上限 1 也不能永远只列同一个目录");
    }

    /// 游标指向的目录在两次轮询之间被删掉也不能让轮转停住；回绕不丢目录。
    #[test]
    fn scan_window_survives_a_deleted_cursor_directory() {
        let items: Vec<String> = (0..10).map(|i| format!("/home/d{i:02}")).collect();
        let (w1, next) = pick_scan_window(&items, Some("/home/d04"), 4);
        assert_eq!(w1, vec!["/home/d05", "/home/d06", "/home/d07", "/home/d08"]);
        assert_eq!(next.as_deref(), Some("/home/d08"));
        // 游标指向一个已经不存在的路径：按字符串序继续，不 panic、不空窗
        let (w2, _) = pick_scan_window(&items, Some("/home/d99"), 4);
        assert_eq!(w2, vec!["/home/d00", "/home/d01", "/home/d02", "/home/d03"]);
        // 到尾回绕
        let (w3, _) = pick_scan_window(&items, Some("/home/d08"), 4);
        assert_eq!(w3, vec!["/home/d09", "/home/d00", "/home/d01", "/home/d02"]);
    }

    /// ★ 数据安全回归（真机级）：对账时目录列举失败（网络错误 / 超时），
    /// 旧逻辑把「没看到」当成「远端已删」→ 把本地干净节点连缓存一起删掉。
    /// 现在该目录下的候选**不做判定**：节点与 baseline 都保持不动，只记错误 + 说明。
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
            report.notes.iter().any(|n| n.contains("暂不判定")),
            "要向用户说明这些条目这轮为什么没判: {report:?}"
        );
        let _ = std::fs::remove_dir_all(&work);
    }

    // ------------------------------------------------- ★ 大目录轮转（真实 reconcile_view）

    /// 用 `host:port` 造一个真实 `Client`，sid 直接塞好（跳过登录）。
    fn client_for(host: &str, port: u16) -> Client {
        let link = LinkConfig {
            id: "t".into(),
            host: host.into(),
            port,
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
        client.set_sid("test-sid");
        client
    }

    fn mount_view(view: Arc<FakeLocal>, root: &str, mountpoint: &str) -> MountView {
        MountView {
            mountpoint: PathBuf::from(mountpoint),
            remote_root: root.to_string(),
            view,
            upload: None,
            read_only: false,
            rules: Arc::new(Rules::temp_only(true)),
            conflict: qxync_core::tasks::DEFAULT_CONFLICT.to_string(),
        }
    }

    /// mock NAS 的行为配置（只覆盖对账用到的两跳）。
    #[derive(Default)]
    struct MockNasSpec {
        /// `get_list path` → 目录条目
        listings: BTreeMap<String, Vec<qxync_core::DirEntry>>,
        /// `get_list` 这些路径 → HTTP 500
        fail: BTreeSet<String>,
        /// `get_list` 这些路径 → HTTP 200 + 非列表 JSON（模拟反代/WAF 的 `{"error":…}`）
        soft_fail: BTreeSet<String>,
        /// `get_list` 这些路径 → JSON `status`（模拟 QTS 的 4/5/6 错误出口）
        force_status: BTreeMap<String, i64>,
        /// `stat` 认为存在的完整路径 → 返回的条目（文件/目录、大小、mtime 由用例定）；
        /// 不在表里 → 占位条目 `exist=0`
        existing_entries: BTreeMap<String, qxync_core::DirEntry>,
        /// `stat` 这些完整路径 → HTTP 500
        stat_fail: BTreeSet<String>,
        /// `stat` 这些完整路径 → JSON `status`
        stat_status: BTreeMap<String, i64>,
    }

    /// 极简 NAS HTTP 服务：实现对账用到的 `get_list` 与 `stat` 两跳。
    ///
    /// 足够在无真机环境下跑**真实** `reconcile_view`：真 Client、真 HTTP、真分页、
    /// 真「列举失败 / 不存在 / stat 确认」路径。
    async fn spawn_listing_server(
        listings: BTreeMap<String, Vec<qxync_core::DirEntry>>,
        fail: BTreeSet<String>,
    ) -> (
        String,
        u16,
        Arc<Mutex<Vec<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        spawn_nas_server(MockNasSpec {
            listings,
            fail,
            ..Default::default()
        })
        .await
    }

    async fn spawn_nas_server(
        spec: MockNasSpec,
    ) -> (
        String,
        u16,
        Arc<Mutex<Vec<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let MockNasSpec {
            listings,
            fail,
            soft_fail,
            force_status,
            existing_entries,
            stat_fail,
            stat_status,
        } = spec;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_task = seen.clone();
        let listings = Arc::new(listings);
        let fail = Arc::new(fail);
        let soft_fail = Arc::new(soft_fail);
        let force_status = Arc::new(force_status);
        let existing_entries = Arc::new(existing_entries);
        let stat_fail = Arc::new(stat_fail);
        let stat_status = Arc::new(stat_status);
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let seen = seen_task.clone();
                let listings = listings.clone();
                let fail = fail.clone();
                let soft_fail = soft_fail.clone();
                let force_status = force_status.clone();
                let existing_entries = existing_entries.clone();
                let stat_fail = stat_fail.clone();
                let stat_status = stat_status.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let mut req = Vec::new();
                    loop {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                        if req.windows(4).any(|w| w == b"\r\n\r\n") || req.len() > 65536 {
                            break;
                        }
                    }
                    let text = String::from_utf8_lossy(&req);
                    let first = text.lines().next().unwrap_or("");
                    let target = first.split_whitespace().nth(1).unwrap_or("/");
                    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
                    let params: BTreeMap<String, String> = query
                        .split('&')
                        .filter_map(|kv| kv.split_once('='))
                        .map(|(k, v)| (k.to_string(), percent_decode(v)))
                        .collect();
                    let func = params.get("func").map(String::as_str).unwrap_or("");
                    let (code, body) = if func == "stat" {
                        let parent = params.get("path").cloned().unwrap_or_default();
                        let name = params.get("file_name").cloned().unwrap_or_default();
                        let full = format!("{parent}/{name}");
                        // `stat:` 前缀只用于事件路径回归断言（列目录的断言只比对目录本身）。
                        seen.lock().unwrap().push(format!("stat:{full}"));
                        if stat_fail.contains(&full) {
                            (
                                "500 Internal Server Error",
                                r#"{"error":"bad gateway"}"#.to_string(),
                            )
                        } else if let Some(status) = stat_status.get(&full) {
                            (
                                "200 OK",
                                serde_json::json!({"status": status, "msg": "forced"}).to_string(),
                            )
                        } else if let Some(entry) = existing_entries.get(&full) {
                            (
                                "200 OK",
                                serde_json::json!({
                                    "total": 1, "exist": 1, "datas": [entry],
                                })
                                .to_string(),
                            )
                        } else {
                            // 不存在的路径：QTS 也会回占位条目，但 exist=0
                            (
                                "200 OK",
                                serde_json::json!({
                                    "total": 0, "exist": 0,
                                    "datas": [{
                                        "filename": name, "isfolder": 1,
                                        "filesize": 0, "epochmt": 0, "exist": 0,
                                    }],
                                })
                                .to_string(),
                            )
                        }
                    } else {
                        let path = params.get("path").cloned().unwrap_or_default();
                        seen.lock().unwrap().push(path.clone());
                        if let Some(status) = force_status.get(&path) {
                            (
                                "200 OK",
                                serde_json::json!({"status": status, "msg": "forced"}).to_string(),
                            )
                        } else if fail.contains(&path) {
                            // 非 404 的传输/服务端错误：对账必须当成「没看到」，不是「已删」。
                            // 刻意回**合法 JSON**：反代的错误体就是这种形状，不能被
                            // `Listing` 的全默认反序列化读成「空目录」（HTTP 状态必须先挡）。
                            (
                                "500 Internal Server Error",
                                r#"{"error":"bad gateway"}"#.to_string(),
                            )
                        } else if soft_fail.contains(&path) {
                            // HTTP 200 + 非列表 JSON：形状校验必须把它当解析失败。
                            ("200 OK", r#"{"error":"soft failure"}"#.to_string())
                        } else if let Some(entries) = listings.get(&path) {
                            let body =
                                serde_json::json!({ "total": entries.len(), "datas": entries });
                            ("200 OK", body.to_string())
                        } else {
                            // 目录确实不存在（QTS 的统一错误出口：status 6 = not found）
                            (
                                "200 OK",
                                serde_json::json!({"status": 6, "msg": "not found"}).to_string(),
                            )
                        }
                    };
                    let resp = format!(
                        "HTTP/1.1 {code}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.flush().await;
                });
            }
        });
        ("127.0.0.1".to_string(), port, seen, handle)
    }

    /// 只解 `%XX`（测试里的路径只有 ASCII）。
    fn percent_decode(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' && i + 2 < b.len() {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
            }
            out.push(b[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// ★ 端到端回归（真 `Client` + 真 HTTP + mock NAS）：任务选的 NAS 文件夹是**家目录的
    /// 子目录**（真机 `root=/home/.Qsync`，NAS 上 `~/.Qsync`）时，事件里的真实路径
    /// `/share/homes/<user>/.Qsync/…` 必须映射成**一个**前缀的客户端路径再去 stat。
    ///
    /// 旧实现会拼出 `/home/.Qsync/.Qsync/…` → 真机每条事件 `status=5`（事件快路径失效）。
    /// 同时回归「事件 stat 的 4/5/6 不再记成硬错误」：指向看不到的路径时只跳过、交对账兜底。
    #[tokio::test]
    async fn apply_event_stats_single_prefixed_path_for_home_subdir_view() {
        let home_view = "/home/.Qsync";
        let path = "/home/.Qsync/2026/28-比赛项目/01配置备份/防火墙配置.txt";
        let ghost = "/home/.Qsync/2026/已删除.txt";
        let (host, port, seen, _srv) = spawn_nas_server(MockNasSpec {
            // ★ stat 必须回**文件**条目（12218 / 1792368403）：旧 mock 一律回目录条目，
            //   「三方一致 → Noop」实际测成的是 RefreshRemote（节点被改成目录）。
            existing_entries: [(
                path.to_string(),
                qxync_core::DirEntry::local(
                    path.rsplit('/').next().unwrap_or(""),
                    false,
                    12218,
                    1792368403,
                ),
            )]
            .into_iter()
            .collect(),
            // 这个路径「看不到」：QTS 用 5（路径不存在 / 无权限）表达
            stat_status: [(ghost.to_string(), 5)].into_iter().collect(),
            ..Default::default()
        })
        .await;

        let work = tmpdir("event-path-subdir");
        let view = FakeLocal::new(home_view, work.join("stash"));
        view.add_file(path, 12218, 1792368403, false, None);
        let mv = mount_view(view.clone(), home_view, "/tmp/event-path-subdir");
        let store = Mutex::new(SyncState::load(work.join("state")).unwrap());
        let mut baseline = Baseline::default();
        baseline.put(path, Sig::file(12218, 1792368403));
        let cfg = SyncConfig::default();
        let client = client_for(&host, port);
        let mk_event = |filepath: &str| qxync_core::sync::SyncEvent {
            log_id: 1,
            action: qxync_core::sync::ACTION_UPSERT_FILE,
            isfolder: 2,
            filepath: filepath.to_string(),
            old_filepath: String::new(),
            exist: true,
            mtime: 1792368403,
            size: 12218,
            device: Some("win-pc".into()),
            device_uid: None,
            user: Some("test1".into()),
        };

        // ---- ① 正常事件：路径映射正确、三方一致 → Noop，且请求里是单前缀路径
        let mut r1 = SyncReport::default();
        let ev = mk_event("/share/homes/test1/.Qsync/2026/28-比赛项目/01配置备份/防火墙配置.txt");
        apply_event(
            &client,
            std::slice::from_ref(&mv),
            &ev,
            &cfg,
            "test1",
            &mut baseline,
            &mut r1,
            &store,
        )
        .await;
        assert!(r1.errors.is_empty(), "不该有错误: {:?}", r1.errors);
        assert_eq!(
            r1.refreshed + r1.conflicts + r1.uploaded + r1.content_updating,
            0,
            "三方一致（local=base=remote）必须是 Noop，不能刷新/冲突/上传: {r1:?}"
        );
        // 节点仍是文件、签名没被动过（旧 mock 回目录条目时这里会被改成目录）
        let node = view.node(path).expect("节点还在");
        assert!(!node.is_dir, "stat 回文件条目，节点不能被改成目录");
        assert_eq!((node.size, node.mtime), (12218, 1792368403));
        let asked = seen.lock().unwrap().clone();
        assert!(
            asked.iter().any(|p| p == &format!("stat:{path}")),
            "必须按客户端路径（单前缀）stat，实际请求: {asked:?}"
        );
        assert!(
            !asked.iter().any(|p| p.contains("/.Qsync/.Qsync/")),
            "★ 绝不能再出现双写路径（真机 status=5 的根因）: {asked:?}"
        );

        // ---- ② 事件指向「看不到」的路径：跳过、不记错误（旧实现会刷一条 error）
        let mut r2 = SyncReport::default();
        let ev2 = mk_event("/share/homes/test1/.Qsync/2026/已删除.txt");
        apply_event(
            &client,
            std::slice::from_ref(&mv),
            &ev2,
            &cfg,
            "test1",
            &mut baseline,
            &mut r2,
            &store,
        )
        .await;
        assert_eq!(r2.events_skipped, 1, "看不到的路径只跳过: {r2:?}");
        assert_eq!(
            r2.events_skipped_reasons.get("远端不可见(4/5/6)"),
            Some(&1),
            "跳过必须归因到 4/5/6（journal 的 remote_change 详情靠它）: {:?}",
            r2.events_skipped_reasons
        );
        assert!(
            r2.errors.is_empty(),
            "4/5/6 不该记成硬错误（交对账兜底）: {:?}",
            r2.errors
        );
        assert!(
            !r2.auth_failed,
            "status=5 是「路径不存在」的常规出口，不能因此重登（否则真机撞失败登录封锁）"
        );
        let _ = std::fs::remove_dir_all(&work);
    }

    /// ★ 复审要求的「穿透 reconcile_view」回归：hook 在**本轮进行中**推进 baseline
    /// （真机 08:06 的事发形态），对账判定必须用「快照 ∪ 本轮推进」—— 只测纯函数
    /// `decide_with_state` 守不住三个调用点，这里直接跑真 `reconcile_view`。
    ///
    /// 时序：开轮快照 base=旧版 → 本轮内上传落地（hook 推进 + 清 dirty）→
    /// 远端列举看到新版 → 旧逻辑判 Conflict（rename_local 出副本 / ask 入队）。
    /// 期望：Noop —— 不出冲突、不进待裁决队列、baseline 不被旧快照回退。
    #[tokio::test]
    async fn reconcile_uses_in_round_hook_advance_not_the_stale_snapshot() {
        use qxync_core::DirEntry;
        let home = "/home/.Qsync";
        let path = "/home/.Qsync/2026/28-比赛项目/01配置备份/防火墙配置.txt";
        let old = Sig::file(12218, 1792368356);
        let new = Sig::file(12218, 1792368403);

        // mock NAS：根下就这一个文件，且就是新版
        let listings: BTreeMap<String, Vec<DirEntry>> = BTreeMap::from([
            (home.to_string(), vec![DirEntry::local("2026", true, 0, 0)]),
            (
                "/home/.Qsync/2026".to_string(),
                vec![DirEntry::local("28-比赛项目", true, 0, 0)],
            ),
            (
                "/home/.Qsync/2026/28-比赛项目".to_string(),
                vec![DirEntry::local("01配置备份", true, 0, 0)],
            ),
            (
                "/home/.Qsync/2026/28-比赛项目/01配置备份".to_string(),
                vec![DirEntry::local(
                    "防火墙配置.txt",
                    false,
                    new.size,
                    new.mtime,
                )],
            ),
        ]);
        let (host, port, _seen, _srv) = spawn_listing_server(listings, BTreeSet::new()).await;

        let work = tmpdir("reconcile-hook-advance");
        let view = FakeLocal::new(home, work.join("stash"));
        // 本地节点 = 新版、干净（hook 的 clear_dirty 已跑过）
        view.add_file(path, new.size, new.mtime, false, None);
        let store = Mutex::new(SyncState::load(work.join("state")).unwrap());

        // 开轮：快照里还是旧版（真机 08:06:21 的形态）
        let mut baseline = Baseline::default();
        baseline.put(path, old);
        {
            let mut g = store.lock().unwrap();
            g.baseline.put(path, old);
            g.save_all().unwrap();
        }
        // 本轮进行中：上传落地 → hook 推进权威 baseline + 记进覆盖表
        store
            .lock()
            .unwrap()
            .note_uploaded(path, new.size, new.mtime)
            .unwrap();

        let mv = mount_view(view.clone(), home, "/tmp/reconcile-hook-advance");
        let client = client_for(&host, port);
        let cfg = SyncConfig::default();
        let mut report = SyncReport::default();
        reconcile_view(&client, &mv, &cfg, &mut baseline, &mut report, &store).await;

        assert_eq!(
            report.conflicts, 0,
            "本轮 hook 已推进到刚落地的签名 → 对账必须 Noop，不能凭空冲突: {report:?}"
        );
        assert_eq!(
            report.refreshed + report.uploaded + report.content_updating,
            0,
            "也不能刷新/上传（三方一致）: {report:?}"
        );
        // 待裁决队列为空（ask 策略下旧 bug 会入队；这里 mount_view 用默认 rename_local，
        // 旧 bug 会直接出副本 —— 两种策略都不该发生，用 decisions 表兜底断言）
        {
            let g = store.lock().unwrap();
            let rows = g.store.decisions().unwrap_or_default();
            assert!(
                rows.iter().all(|r| r.path != path),
                "不能因为这个路径入待裁决队列: {rows:?}"
            );
        }
        // 权威 baseline 仍是 hook 的值（reconcile 不写权威副本；轮末 persist 由
        // poll_once 负责，且 P1-1 修复后 merge 不会把它抹回旧快照 —— 见
        // `persist_keeps_hook_advances_across_repeated_persists_in_one_round`）。
        assert_eq!(store.lock().unwrap().baseline.get(path), new);
        // 判定用的 baseline 工作副本：Noop 路径不 put，仍是开轮的旧值 —— 这是
        // 预期行为（下一轮 begin_round 会用权威值重新克隆），不是污染。
        assert_eq!(baseline.get(path), old);
        let _ = std::fs::remove_dir_all(&work);
    }

    /// ★ 真机症状的回归：228 个目录 > 单轮上限 200。
    ///
    /// 旧行为：每轮都扫排序前 200 个 → 后 28 个目录**永远**轮不到 → 删除判定被永久
    /// 推迟（真机笔记「目录数 228 超过单轮上限 200…本轮跳过删除判定」重复 87 轮）。
    ///
    /// 期望：第 1 轮删掉窗口内 d000 的远端删除；第 2 轮轮转到 d210 再删；
    /// **未扫目录里的条目一次都不许误删**；两轮窗口合起来覆盖全部目录。
    #[tokio::test]
    async fn reconcile_rotates_scan_window_instead_of_starving_deletions() {
        use qxync_core::DirEntry;
        let home = HOME_ROOT;
        let n = 228usize;
        let remote_dirs: Vec<String> = (0..n).map(|i| format!("{home}/d{i:03}")).collect();
        let mut listings: BTreeMap<String, Vec<DirEntry>> = BTreeMap::new();
        // 根：228 个子目录
        listings.insert(
            home.to_string(),
            remote_dirs
                .iter()
                .map(|d| DirEntry::local(d.rsplit('/').next().unwrap(), true, 0, 0))
                .collect(),
        );
        // 每个目录一个文件；d000 / d210 的远端文件已被删（目录还在、清单为空）
        for (i, d) in remote_dirs.iter().enumerate() {
            let mut entries = Vec::new();
            if i != 0 && i != 210 {
                entries.push(DirEntry::local("f.txt", false, 10, 100));
            }
            listings.insert(d.clone(), entries);
        }
        let (host, port, seen, srv) = spawn_listing_server(listings, BTreeSet::new()).await;

        let work = tmpdir("reconcile-rotate");
        let view = FakeLocal::new(home, work.join("stash"));
        let mut baseline = Baseline::default();
        for d in &remote_dirs {
            let p = format!("{d}/f.txt");
            view.add_file(&p, 10, 100, false, None);
            baseline.put(p, Sig::file(10, 100));
        }
        let store = Mutex::new(SyncState::load(work.join("state")).unwrap());
        let mv = mount_view(view.clone(), home, "/tmp/reconcile-rotate");
        let client = client_for(&host, port);
        // 硬顶 200（默认已提到 2000，这里显式压回 200 复刻「目录数超硬顶」场景）
        let cfg = SyncConfig {
            max_dirs_per_poll: 200,
            ..Default::default()
        };

        // ---- 第 1 轮：窗口从排序开头开始
        let mut r1 = SyncReport::default();
        reconcile_view(&client, &mv, &cfg, &mut baseline, &mut r1, &store).await;
        assert_eq!(r1.deleted, 1, "窗口内的 d000 远端删除必须执行: {r1:?}");
        assert!(view.node(&format!("{home}/d000/f.txt")).is_none());
        assert!(
            view.node(&format!("{home}/d210/f.txt")).is_some(),
            "未扫目录里的干净文件不许被当成「远端已删」"
        );
        assert!(baseline.get(&format!("{home}/d210/f.txt")).exists);
        assert!(
            r1.notes.iter().any(|x| x.contains("轮转")),
            "要告诉用户本轮是轮转扫描: {r1:?}"
        );

        // ---- 第 2 轮：从游标接着轮转，d210 这次必须在窗口里
        let mut r2 = SyncReport::default();
        reconcile_view(&client, &mv, &cfg, &mut baseline, &mut r2, &store).await;
        assert_eq!(
            r2.deleted, 1,
            "轮转必须为排序在后的目录补上删除判定: {r2:?}"
        );
        assert!(view.node(&format!("{home}/d210/f.txt")).is_none());

        // 其余 226 个都还在：没有误删
        let still = remote_dirs
            .iter()
            .filter(|d| view.node(&format!("{d}/f.txt")).is_some())
            .count();
        assert_eq!(still, 226, "除 d000 / d210 外一个都不许删");

        // 两轮窗口合起来覆盖全部 228 个目录（旧实现后 28 个永远不出现）
        let seen = seen.lock().unwrap().clone();
        for d in &remote_dirs {
            assert!(seen.iter().any(|p| p == d), "第 1/2 轮合起来必须列过 {d}");
        }
        srv.abort();
        let _ = std::fs::remove_dir_all(&work);
    }

    /// 时间预算充足 → **一轮全量**：排序在后的目录当轮就判定，删除延迟 = 一个间隔。
    ///
    /// 真机场景（v0.6.8 的动机）：1439 目录 × 600s 间隔，旧实现按目录数轮转要
    /// 8 轮 = 80 分钟才轮完一圈；预算内全量后，远端删除最迟一个间隔就落地。
    #[tokio::test]
    async fn generous_time_budget_scans_everything_in_one_round() {
        use qxync_core::DirEntry;
        let home = HOME_ROOT;
        let n = 228usize;
        let remote_dirs: Vec<String> = (0..n).map(|i| format!("{home}/d{i:03}")).collect();
        let mut listings: BTreeMap<String, Vec<DirEntry>> = BTreeMap::new();
        listings.insert(
            home.to_string(),
            remote_dirs
                .iter()
                .map(|d| DirEntry::local(d.rsplit('/').next().unwrap(), true, 0, 0))
                .collect(),
        );
        // d000 / d210 的远端文件已被删（目录还在、清单为空）
        for (i, d) in remote_dirs.iter().enumerate() {
            let mut entries = Vec::new();
            if i != 0 && i != 210 {
                entries.push(DirEntry::local("f.txt", false, 10, 100));
            }
            listings.insert(d.clone(), entries);
        }
        let (host, port, _seen, srv) = spawn_listing_server(listings, BTreeSet::new()).await;

        let work = tmpdir("reconcile-budget-full");
        let view = FakeLocal::new(home, work.join("stash"));
        let mut baseline = Baseline::default();
        for d in &remote_dirs {
            let p = format!("{d}/f.txt");
            view.add_file(&p, 10, 100, false, None);
            baseline.put(p, Sig::file(10, 100));
        }
        let store = Mutex::new(SyncState::load(work.join("state")).unwrap());
        let mv = mount_view(view.clone(), home, "/tmp/reconcile-budget-full");
        let client = client_for(&host, port);
        // 默认硬顶 2000（目录数不超）+ 充足时间预算 → 全量
        let cfg = SyncConfig {
            scan_time_budget: Duration::from_secs(60),
            ..Default::default()
        };

        let mut r1 = SyncReport::default();
        reconcile_view(&client, &mv, &cfg, &mut baseline, &mut r1, &store).await;
        assert_eq!(
            r1.dirs_scanned, 229,
            "预算充足：根 + 228 个目录一轮列完: {r1:?}"
        );
        assert_eq!(
            r1.deleted, 2,
            "d000 和 d210（排序一头一尾）的删除**当轮**都执行: {r1:?}"
        );
        // 全量后游标必须清掉：下轮从头开始（不清的话下轮带着游标白转一圈）
        let cursor_key = format!("reconcile_scan_cursor:{}", mv.mountpoint.display());
        let cursor = store.lock().unwrap().store.meta(&cursor_key).unwrap();
        assert_eq!(cursor, None, "列全了就不该留轮转游标");
        srv.abort();
        let _ = std::fs::remove_dir_all(&work);
    }

    /// 时间预算极小（1ns）→ **进度保证**：每轮至少列一个轮转目录、游标每轮前进，
    /// 不许「根吃光预算 → 轮转目录永远列不到、游标也存不下」——那是时间预算版的
    /// 饿死（v0.6.4 修过的坑换个马甲回来）。
    #[tokio::test]
    async fn tiny_time_budget_still_makes_progress() {
        use qxync_core::DirEntry;
        let home = HOME_ROOT;
        let n = 228usize;
        let remote_dirs: Vec<String> = (0..n).map(|i| format!("{home}/d{i:03}")).collect();
        let mut listings: BTreeMap<String, Vec<DirEntry>> = BTreeMap::new();
        listings.insert(
            home.to_string(),
            remote_dirs
                .iter()
                .map(|d| DirEntry::local(d.rsplit('/').next().unwrap(), true, 0, 0))
                .collect(),
        );
        for (i, d) in remote_dirs.iter().enumerate() {
            let mut entries = Vec::new();
            if i != 0 && i != 210 {
                entries.push(DirEntry::local("f.txt", false, 10, 100));
            }
            listings.insert(d.clone(), entries);
        }
        let (host, port, _seen, srv) = spawn_listing_server(listings, BTreeSet::new()).await;

        let work = tmpdir("reconcile-budget-tiny");
        let view = FakeLocal::new(home, work.join("stash"));
        let mut baseline = Baseline::default();
        for d in &remote_dirs {
            let p = format!("{d}/f.txt");
            view.add_file(&p, 10, 100, false, None);
            baseline.put(p, Sig::file(10, 100));
        }
        let store = Mutex::new(SyncState::load(work.join("state")).unwrap());
        let mv = mount_view(view.clone(), home, "/tmp/reconcile-budget-tiny");
        let client = client_for(&host, port);
        // 硬顶 2000（目录数不超，全靠时间预算截断）+ 1ns 预算
        let cfg = SyncConfig {
            scan_time_budget: Duration::from_nanos(1),
            ..Default::default()
        };
        let cursor_key = format!("reconcile_scan_cursor:{}", mv.mountpoint.display());

        // 第 1 轮：根 + d000（进度保证至少列一个），预算在 d001 前耗尽
        let mut r1 = SyncReport::default();
        reconcile_view(&client, &mv, &cfg, &mut baseline, &mut r1, &store).await;
        assert_eq!(r1.dirs_scanned, 2, "根 + 1 个轮转目录: {r1:?}");
        assert_eq!(
            r1.deleted, 1,
            "d000（第一个轮转目录）的删除当轮执行: {r1:?}"
        );
        let cursor = store.lock().unwrap().store.meta(&cursor_key).unwrap();
        assert_eq!(
            cursor.as_deref(),
            Some(format!("{home}/d000").as_str()),
            "游标 = 本轮列过的最后一个目录"
        );

        // 第 2/3 轮：游标每轮前进一格（饿死的反面：永远在推进）
        for expect in [1usize, 2] {
            let mut r = SyncReport::default();
            reconcile_view(&client, &mv, &cfg, &mut baseline, &mut r, &store).await;
            assert_eq!(r.dirs_scanned, 2, "每轮固定推进一个目录: {r:?}");
            let cursor = store.lock().unwrap().store.meta(&cursor_key).unwrap();
            assert_eq!(
                cursor.as_deref(),
                Some(format!("{home}/d{expect:03}").as_str()),
                "游标必须逐轮前进到 d{expect:03}"
            );
        }
        // d210 还没轮到：不许因为「没看到」被当成远端已删
        assert!(
            view.node(&format!("{home}/d210/f.txt")).is_some(),
            "未轮到的目录里的文件不许被误删"
        );
        srv.abort();
        let _ = std::fs::remove_dir_all(&work);
    }

    /// 单个目录列举失败**只影响它自己**：同一视图里其它目录的远端删除照常执行。
    ///
    /// 旧实现是「整个视图只要有 1 个目录失败 → 整轮不删」，目录一多就会因为一个
    /// 抖动目录把全视图的删除长期挡住。按目录判定后，失败目录里的条目不动，
    /// 其它目录不做「陪绑」。
    #[tokio::test]
    async fn listing_failure_only_defers_its_own_directory() {
        use qxync_core::DirEntry;
        let home = HOME_ROOT;
        let bad = format!("{home}/bad");
        let good = format!("{home}/good");
        let soft = format!("{home}/soft");
        let mut listings: BTreeMap<String, Vec<DirEntry>> = BTreeMap::new();
        listings.insert(
            home.to_string(),
            vec![
                DirEntry::local("bad", true, 0, 0),
                DirEntry::local("good", true, 0, 0),
                DirEntry::local("soft", true, 0, 0),
            ],
        );
        listings.insert(bad.clone(), vec![DirEntry::local("f.txt", false, 10, 100)]);
        // good 的远端文件已被删
        listings.insert(good.clone(), Vec::new());
        listings.insert(soft.clone(), vec![DirEntry::local("f.txt", false, 10, 100)]);
        let mut fail: BTreeSet<String> = BTreeSet::new();
        fail.insert(bad.clone());
        let mut soft_fail: BTreeSet<String> = BTreeSet::new();
        soft_fail.insert(soft.clone());
        let (host, port, _seen, srv) = spawn_nas_server(MockNasSpec {
            listings,
            fail,
            soft_fail,
            ..Default::default()
        })
        .await;

        let work = tmpdir("reconcile-partial-fail");
        let view = FakeLocal::new(home, work.join("stash"));
        let mut baseline = Baseline::default();
        for dir in [&bad, &good, &soft] {
            let p = format!("{dir}/f.txt");
            view.add_file(&p, 10, 100, false, None);
            baseline.put(p, Sig::file(10, 100));
        }
        let store = Mutex::new(SyncState::load(work.join("state")).unwrap());
        let mv = mount_view(view.clone(), home, "/tmp/reconcile-partial-fail");
        let client = client_for(&host, port);
        let cfg = SyncConfig::default();
        let mut report = SyncReport::default();
        reconcile_view(&client, &mv, &cfg, &mut baseline, &mut report, &store).await;

        assert_eq!(
            report.deleted, 1,
            "good 目录的远端删除不能被 bad/soft 的失败陪绑: {report:?}"
        );
        assert!(view.node(&format!("{good}/f.txt")).is_none());
        assert!(
            view.node(&format!("{bad}/f.txt")).is_some(),
            "列举失败的目录里「没看到」不等于「远端已删」"
        );
        assert!(
            view.node(&format!("{soft}/f.txt")).is_some(),
            "HTTP 200 + 非列表 JSON 也必须当失败，不能读成空目录"
        );
        assert!(baseline.get(&format!("{bad}/f.txt")).exists);
        assert!(baseline.get(&format!("{soft}/f.txt")).exists);
        assert!(
            report.notes.iter().any(|x| x.contains("暂不判定")),
            "要说明哪些条目被推迟: {report:?}"
        );
        assert!(
            report.errors.iter().any(|x| x.contains("对账列举")),
            "失败必须如实记错误: {report:?}"
        );
        srv.abort();
        let _ = std::fs::remove_dir_all(&work);
    }

    /// ★ 数据安全红线：`get_list` 回 status 4/5（权限 / 会话）**不能**当成「目录已删」。
    ///
    /// 真机证据：`/Download`（存在但当前会话不可见）回 status 5、`/homes` 回 status 4；
    /// 而 4/5 同时是「会话失效」的码（`Error::is_auth`）。修法：只有 `stat` 正向确认
    /// `exist=0` 才允许按「远端已删」处理；stat 说仍存在 → 该目录不删（目录条目记进
    /// 远端视图），其内容本轮不判定。stat 成功本身证明会话有效，所以**不**置
    /// `auth_failed`（否则永久 ACL 隐藏的目录会每轮重登 + 刷 WARN）。
    #[tokio::test]
    async fn listing_status_4_or_5_never_deletes_without_stat_confirmation() {
        use qxync_core::DirEntry;
        let home = HOME_ROOT;
        let sec = format!("{home}/sec"); // 列举 status 5，但 stat 说存在
        let gone = format!("{home}/gone"); // 列举 status 6，stat 确认不存在
        let mut listings: BTreeMap<String, Vec<DirEntry>> = BTreeMap::new();
        listings.insert(
            home.to_string(),
            vec![
                DirEntry::local("sec", true, 0, 0),
                DirEntry::local("gone", true, 0, 0),
            ],
        );
        let mut force: BTreeMap<String, i64> = BTreeMap::new();
        force.insert(sec.clone(), 5);
        force.insert(gone.clone(), 6);
        let mut existing: BTreeMap<String, qxync_core::DirEntry> = BTreeMap::new();
        existing.insert(
            sec.clone(),
            qxync_core::DirEntry::local(sec.rsplit('/').next().unwrap_or(""), true, 0, 0),
        );
        let (host, port, _seen, srv) = spawn_nas_server(MockNasSpec {
            listings,
            force_status: force,
            existing_entries: existing,
            ..Default::default()
        })
        .await;

        let work = tmpdir("reconcile-status-confirm");
        let view = FakeLocal::new(home, work.join("stash"));
        let mut baseline = Baseline::default();
        for dir in [&sec, &gone] {
            let p = format!("{dir}/f.txt");
            view.add_file(&p, 10, 100, false, None);
            baseline.put(p, Sig::file(10, 100));
        }
        let store = Mutex::new(SyncState::load(work.join("state")).unwrap());
        let mv = mount_view(view.clone(), home, "/tmp/reconcile-status-confirm");
        let client = client_for(&host, port);
        let cfg = SyncConfig::default();
        let mut report = SyncReport::default();
        reconcile_view(&client, &mv, &cfg, &mut baseline, &mut report, &store).await;

        // sec：stat 确认存在 → 绝不删；stat 成功也证明会话有效，不许置 auth_failed
        assert!(
            view.node(&format!("{sec}/f.txt")).is_some(),
            "4/5 不能当删除依据"
        );
        assert!(baseline.get(&format!("{sec}/f.txt")).exists);
        assert!(
            !report.auth_failed,
            "stat 已成功 = 会话有效；置 auth_failed 会让永久 ACL 目录每轮重登/刷 WARN: {report:?}"
        );
        assert!(
            report.errors.iter().any(|x| x.contains("stat 显示仍存在")),
            "要说明为什么这轮没删: {report:?}"
        );
        // gone：stat 确认不存在 → 删除照常执行（正向确认不能把真删除也挡死）
        assert_eq!(
            report.deleted, 1,
            "stat 确认 exist=0 时必须照常删: {report:?}"
        );
        assert!(view.node(&format!("{gone}/f.txt")).is_none());
        assert!(!baseline.get(&format!("{gone}/f.txt")).exists);
        srv.abort();
        let _ = std::fs::remove_dir_all(&work);
    }

    /// 远端根列举回 4/5：按权限 / 会话 / 卸载处理，**绝不**把根下文件当「远端已删」。
    ///
    /// 根不可能凭空消失，所以这一条连 `stat` 正向确认都不做（stat 在这些故障下也可能
    /// 给出不可信的占位结果）。
    #[tokio::test]
    async fn listing_root_status_4_or_5_defers_instead_of_deleting() {
        let home = HOME_ROOT;
        let path = format!("{home}/keep.txt");
        let mut force: BTreeMap<String, i64> = BTreeMap::new();
        force.insert(home.to_string(), 4);
        let (host, port, _seen, srv) = spawn_nas_server(MockNasSpec {
            force_status: force,
            ..Default::default()
        })
        .await;

        let work = tmpdir("reconcile-root-status");
        let view = FakeLocal::new(home, work.join("stash"));
        view.add_file(&path, 10, 100, false, None);
        let mut baseline = Baseline::default();
        baseline.put(path.clone(), Sig::file(10, 100));
        let store = Mutex::new(SyncState::load(work.join("state")).unwrap());
        let mv = mount_view(view.clone(), home, "/tmp/reconcile-root-status");
        let client = client_for(&host, port);
        let cfg = SyncConfig::default();
        let mut report = SyncReport::default();
        reconcile_view(&client, &mv, &cfg, &mut baseline, &mut report, &store).await;

        assert_eq!(report.deleted, 0, "根 4/5 时不许删任何东西: {report:?}");
        assert!(view.node(&path).is_some());
        assert!(baseline.get(&path).exists);
        assert!(report.auth_failed, "4/5 要触发会话重登: {report:?}");
        assert!(
            report.errors.iter().any(|x| x.contains("远端根")),
            "要说明根为什么没判定: {report:?}"
        );
        srv.abort();
        let _ = std::fs::remove_dir_all(&work);
    }

    /// ★ 复核补的回归（F1）：目录被**父目录列举漏掉**（权限过滤 / 服务端隐藏）时，
    /// 目录条目自身不能按「远端缺失」删 —— stat 说它存在就必须把目录记进远端视图，
    /// 否则 `remove_remote` 会连整棵子树 + 缓存 + baseline 一起清掉。
    ///
    /// 触发条件比原 P0 更窄（父目录列举必须漏掉一个实际存在的目录），但后果是整棵子树。
    #[tokio::test]
    async fn directory_missing_from_parent_listing_but_stat_exists_is_not_deleted() {
        let home = HOME_ROOT;
        let hidden = format!("{home}/hidden");
        let child = format!("{hidden}/f.txt");
        // 根列举**没有** hidden 这个条目（模拟权限过滤掉整个目录）
        let mut listings: BTreeMap<String, Vec<qxync_core::DirEntry>> = BTreeMap::new();
        listings.insert(home.to_string(), Vec::new());
        // list(hidden) 回 status 5（存在但不可见）；stat(hidden) 说存在
        let mut force: BTreeMap<String, i64> = BTreeMap::new();
        force.insert(hidden.clone(), 5);
        let mut existing: BTreeMap<String, qxync_core::DirEntry> = BTreeMap::new();
        existing.insert(
            hidden.clone(),
            qxync_core::DirEntry::local(hidden.rsplit('/').next().unwrap_or(""), true, 0, 0),
        );
        let (host, port, _seen, srv) = spawn_nas_server(MockNasSpec {
            listings,
            force_status: force,
            existing_entries: existing,
            ..Default::default()
        })
        .await;

        let work = tmpdir("reconcile-hidden-dir");
        let view = FakeLocal::new(home, work.join("stash"));
        view.add_dir(&hidden);
        view.add_file(&child, 10, 100, false, None);
        let mut baseline = Baseline::default();
        baseline.put(hidden.clone(), Sig::dir());
        baseline.put(child.clone(), Sig::file(10, 100));
        let store = Mutex::new(SyncState::load(work.join("state")).unwrap());
        let mv = mount_view(view.clone(), home, "/tmp/reconcile-hidden-dir");
        let client = client_for(&host, port);
        let cfg = SyncConfig::default();
        let mut report = SyncReport::default();
        reconcile_view(&client, &mv, &cfg, &mut baseline, &mut report, &store).await;

        assert_eq!(
            report.deleted, 0,
            "目录被父目录漏掉但 stat 说存在 → 不许删: {report:?}"
        );
        assert!(
            view.node(&hidden).is_some(),
            "★ 目录条目自身不能被当成「远端已删」"
        );
        assert!(view.node(&child).is_some(), "子树不能被连坐");
        assert!(baseline.get(&hidden).exists);
        assert!(baseline.get(&child).exists);
        srv.abort();
        let _ = std::fs::remove_dir_all(&work);
    }

    /// stat 确认也失败 → 目录存在性未知：目录条目与内容都不判定（保守方向）。
    #[tokio::test]
    async fn stat_failure_keeps_directory_and_subtree() {
        let home = HOME_ROOT;
        // 两种 stat 失败：HTTP 层失败 + status 4/5（会话失效）—— 都必须保守不判定
        let unknown_http = format!("{home}/unknown_http");
        let unknown_auth = format!("{home}/unknown_auth");
        let mut listings: BTreeMap<String, Vec<qxync_core::DirEntry>> = BTreeMap::new();
        listings.insert(home.to_string(), Vec::new());
        let mut force: BTreeMap<String, i64> = BTreeMap::new();
        force.insert(unknown_http.clone(), 6);
        force.insert(unknown_auth.clone(), 5);
        let mut stat_fail: BTreeSet<String> = BTreeSet::new();
        stat_fail.insert(unknown_http.clone());
        let mut stat_status: BTreeMap<String, i64> = BTreeMap::new();
        stat_status.insert(unknown_auth.clone(), 5);
        let (host, port, _seen, srv) = spawn_nas_server(MockNasSpec {
            listings,
            force_status: force,
            stat_fail,
            stat_status,
            ..Default::default()
        })
        .await;

        let work = tmpdir("reconcile-stat-fail");
        let view = FakeLocal::new(home, work.join("stash"));
        let mut baseline = Baseline::default();
        for (dir, child) in [
            (&unknown_http, format!("{unknown_http}/f.txt")),
            (&unknown_auth, format!("{unknown_auth}/f.txt")),
        ] {
            view.add_dir(dir);
            view.add_file(&child, 10, 100, false, None);
            baseline.put(dir.clone(), Sig::dir());
            baseline.put(child.clone(), Sig::file(10, 100));
        }
        let store = Mutex::new(SyncState::load(work.join("state")).unwrap());
        let mv = mount_view(view.clone(), home, "/tmp/reconcile-stat-fail");
        let client = client_for(&host, port);
        let cfg = SyncConfig::default();
        let mut report = SyncReport::default();
        reconcile_view(&client, &mv, &cfg, &mut baseline, &mut report, &store).await;

        assert_eq!(report.deleted, 0, "存在性未知时不许删: {report:?}");
        for dir in [&unknown_http, &unknown_auth] {
            let child = format!("{dir}/f.txt");
            assert!(view.node(dir).is_some(), "目录 {dir} 不能删");
            assert!(view.node(&child).is_some(), "子树不能被连坐");
            assert!(baseline.get(dir).exists);
            assert!(baseline.get(&child).exists);
        }
        assert!(
            report.errors.iter().any(|x| x.contains("stat 确认也失败")),
            "要说明为什么没判定: {report:?}"
        );
        assert!(report.auth_failed, "stat 也回 4/5 时要触发重登: {report:?}");
        srv.abort();
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
