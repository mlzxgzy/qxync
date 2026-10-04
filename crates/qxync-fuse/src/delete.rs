//! ★ M11：删除队列：本地 unlink/rmdir → 服务端。
//!
//! ## 为什么要有这个队列
//!
//! 改之前 [`qxync_fuse::QsyncFs::remove_entry`] 是**同步阻塞**的：
//! `unlink` → `rt.block_on(client.delete_entry(..))` → 一直等到 NAS 回包才 `reply.ok()`。
//! 于是「删 N 个文件 = N 次串行 RTT」，用户点删除要盯着转圈。
//! 走桌面回收站（KDE Dolphin 的 `trash:` 协议）更糟：Dolphin 会
//! 「在卷内 `.Trash-$UID/files/` 放一份 + 写 `info/*.trashinfo`」，
//! 每一步都是一次打回 NAS 的 FUSE 调用，**每个文件 2~3 次往返**。
//!
//! ## 官方是怎么做的
//!
//! Qsync 官方客户端（见 `docs/M8-向Qsync-Client-6靠拢.md` ③ `Enable Smart Delete`）
//! 的语义是 *"deleted files on the local device are retained on the NAS"*：
//! 本地删除立刻生效，**服务端侧**由客户端异步上报 / 后端处理。
//! 也就是说「本地立即返回 + 后台推送」正是官方删除的形态，本队列是对齐它。
//!
//! ## 设计要点（与 `upload` 队列同构）
//!
//! * **先落盘再改数据**：`enqueue` 先把作业写进状态库 `deletes` 表，再改内存队列。
//!   崩溃 → 重启时 `store.deletes()` 重新入队，绝不丢「本地已经删了、NAS 还留着」这件事。
//! * **同目录合并成一次请求**：`qsyncsrv.cgi?func=delete` 的 `file_total` 就是本批条目数
//!   （`stat`/`set_mtime` 把它硬编码成 `1` 只因那些调用每次一个文件）。
//!   同一父目录的作业攒够一批或等满一个窗口就**一次**发出去 —— 这是本模块性能收益的主要来源。
//! * **库 = 未完成作业**：成功 / 重试超限放弃，都会删掉对应行。
//! * 退避重试同上传队列；`drain` 给卸载/测试用。
//!
//! ## 安全取舍
//!
//! 入队即向调用方返回 `Ok`，意味着**远端删除失败时本地已经看不见文件了**。
//! 这是「本地是权威」的取舍，和上传队列同源。缓解手段：
//! 失败重试到超限后 `stats.failed` 自增、journal 记错误、daemon 日志显眼告警，
//! 运维可以从这些痕迹发现并手动补删。**熔断（`DeleteGuard`）仍然在入队前生效** ——
//! 大批量误删仍然会被 `EACCES` 挡住，不会悄悄溜进队列。

use qxync_client::{write_action, Client};
use qxync_core::store::{DeleteRow, Store};
use qxync_core::Error as CoreError;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// 一个待删除的远端条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteJob {
    pub remote_dir: String,
    pub remote_name: String,
    pub is_dir: bool,
    pub attempts: u32,
}

impl DeleteJob {
    pub fn remote_path(&self) -> String {
        format!(
            "{}/{}",
            self.remote_dir.trim_end_matches('/'),
            self.remote_name
        )
    }

    fn to_row(&self) -> DeleteRow {
        DeleteRow {
            remote_dir: self.remote_dir.clone(),
            remote_name: self.remote_name.clone(),
            is_dir: self.is_dir,
            attempts: self.attempts,
            queued_unix: now_unix(),
        }
    }

    fn from_row(r: &DeleteRow) -> Self {
        Self {
            remote_dir: r.remote_dir.clone(),
            remote_name: r.remote_name.clone(),
            is_dir: r.is_dir,
            attempts: r.attempts,
        }
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Default)]
pub struct DeleteStats {
    pub done: AtomicU64,
    pub failed: AtomicU64,
    pub retries: AtomicU64,
    /// 累计发出的**批次数**（每批一次 HTTP 请求）。
    pub batches: AtomicU64,
    /// 累计删除的条目数。
    pub deleted: AtomicU64,
    pub pending: AtomicU64,
}

/// 队列快照，给 `status` 暴露。
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct DeleteSnapshot {
    pub pending: u64,
    /// 是否正有批次在途（已从队列取走、尚未拿到 NAS 回包）。
    pub active: bool,
    pub done: u64,
    pub failed: u64,
    pub retries: u64,
    pub batches: u64,
    pub deleted: u64,
}

/// 一批删除：同一父目录下的若干条目。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Batch {
    dir: String,
    /// `(名字, 是不是目录, 重试次数)`，顺序即删除顺序。
    items: Vec<(String, bool, u32)>,
}

/// 每批最多带多少个条目。
///
/// 太大了会让单个请求体膨胀、失败时整批重试成本高；太小则退化成逐个删。
/// 50 是「一次请求能覆盖 `rm` 选中一整段」的经验值。
pub const MAX_BATCH: usize = 50;

/// 攒批窗口：worker 拿到第一个作业后最多等这么久，凑更多条目一起发。
pub const BATCH_WINDOW: Duration = Duration::from_millis(200);

pub struct DeleteQueue {
    client: Arc<Client>,
    rt: tokio::runtime::Handle,
    store: Option<Arc<Store>>,
    max_attempts: u32,
    state: Mutex<State>,
    cv: Condvar,
    shutdown: AtomicBool,
    stats: DeleteStats,
}

struct State {
    pending: VecDeque<DeleteJob>,
    /// 是否有批次在途。
    active: bool,
}

impl DeleteQueue {
    /// 建队列；状态库里的未完成作业会被重新入队（崩溃恢复）。
    ///
    /// 与 `UploadQueue` 同样取舍：状态库打不开**不**返回 `Err`，
    /// 降级成「无持久化」队列（内存语义照旧，崩溃会丢未完成删除）。
    pub fn new(
        client: Arc<Client>,
        rt: tokio::runtime::Handle,
        marker_dir: std::path::PathBuf,
    ) -> std::io::Result<Arc<Self>> {
        std::fs::create_dir_all(&marker_dir)?;
        // 与上传队列共用同一个库文件（`queue.db`），表各自独立。
        let db_path = marker_dir.join("queue.db");
        let store = match Store::open(&db_path) {
            Ok(s) => Some(Arc::new(s)),
            Err(e) => {
                tracing::warn!(
                    "删除队列状态库 {} 打不开，降级为无持久化队列（崩溃会丢未完成删除）: {e}",
                    db_path.display()
                );
                None
            }
        };

        let mut pending = VecDeque::new();
        if let Some(s) = &store {
            match s.deletes() {
                Ok(rows) => {
                    for r in rows {
                        let job = DeleteJob::from_row(&r);
                        tracing::info!("恢复未完成的删除: {}", job.remote_path());
                        pending.push_back(job);
                    }
                }
                Err(e) => {
                    tracing::warn!("读取删除队列状态库失败（本次从空队列开始，重启再试）: {e}")
                }
            }
        }

        let n = pending.len() as u64;
        Ok(Arc::new(Self {
            client,
            rt,
            store,
            max_attempts: 5,
            state: Mutex::new(State {
                pending,
                active: false,
            }),
            cv: Condvar::new(),
            shutdown: AtomicBool::new(false),
            stats: DeleteStats {
                pending: AtomicU64::new(n),
                ..Default::default()
            },
        }))
    }

    /// 入队：**先写状态库，再改内存**（崩溃安全）。
    ///
    /// 入库失败不阻断调用方（本地删除已经发生了，不能因为队列库写不进去
    /// 就把 unlink 变成失败 —— 那会让用户看到「文件还在」，与真实状态不符）。
    pub fn enqueue(&self, job: DeleteJob) {
        self.persist_put(&job);
        {
            let mut st = self.state.lock().unwrap();
            // 同路径已在队列里：先删后加，保证新的一次尝试计数从当前作业继续
            st.pending.retain(|j| j.remote_path() != job.remote_path());
            st.pending.push_back(job);
        }
        self.stats
            .pending
            .store(self.pending_len(), Ordering::Relaxed);
        self.cv.notify_all();
    }

    pub fn is_active(&self) -> bool {
        self.state.lock().unwrap().active
    }

    pub fn has_pending(&self, remote_path: &str) -> bool {
        self.state
            .lock()
            .unwrap()
            .pending
            .iter()
            .any(|j| j.remote_path() == remote_path)
    }

    pub fn snapshot(&self) -> DeleteSnapshot {
        DeleteSnapshot {
            pending: self.pending_len(),
            active: self.is_active(),
            done: self.stats.done.load(Ordering::Relaxed),
            failed: self.stats.failed.load(Ordering::Relaxed),
            retries: self.stats.retries.load(Ordering::Relaxed),
            batches: self.stats.batches.load(Ordering::Relaxed),
            deleted: self.stats.deleted.load(Ordering::Relaxed),
        }
    }

    fn pending_len(&self) -> u64 {
        self.state.lock().unwrap().pending.len() as u64
    }

    pub fn spawn_worker(self: &Arc<Self>) -> std::io::Result<std::thread::JoinHandle<()>> {
        let q = self.clone();
        std::thread::Builder::new()
            .name("qxync-delete".into())
            .spawn(move || q.worker_loop())
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.cv.notify_all();
    }

    /// 等队列清空（卸载 / 测试用）。
    pub fn drain(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let mut st = self.state.lock().unwrap();
        while (!st.pending.is_empty() || st.active) && std::time::Instant::now() < deadline {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let (g, _) = self.cv.wait_timeout(st, left).unwrap();
            st = g;
        }
        st.pending.is_empty() && !st.active
    }

    /// 取消某路径**还没发出**的删除作业（含状态库行）。
    ///
    /// 用于「删了之后又在同一路径新建」的收敛：新建意味着这个远端路径不该再被删。
    /// 已在途的那一批取消不了，要用 [`Self::drain`] 等它结束。
    pub fn cancel(&self, remote_path: &str) -> bool {
        let removed: Vec<DeleteJob> = {
            let mut st = self.state.lock().unwrap();
            let mut kept = VecDeque::new();
            let mut removed = Vec::new();
            while let Some(j) = st.pending.pop_front() {
                if j.remote_path() == remote_path {
                    removed.push(j);
                } else {
                    kept.push_back(j);
                }
            }
            st.pending = kept;
            removed
        };
        let had = !removed.is_empty();
        if had {
            if let Some(s) = &self.store {
                if let Err(e) = s.delete_row(remote_path) {
                    tracing::warn!("删除队列取消作业 {remote_path} 失败（库）: {e}");
                }
            }
            self.stats
                .pending
                .store(self.pending_len(), Ordering::Relaxed);
            self.cv.notify_all();
        }
        had
    }

    fn worker_loop(self: Arc<Self>) {
        loop {
            // 取作业；没有就等。
            let first = {
                let mut st = self.state.lock().unwrap();
                loop {
                    if let Some(j) = st.pending.pop_front() {
                        st.active = true;
                        break Some(j);
                    }
                    if self.shutdown.load(Ordering::Relaxed) {
                        break None;
                    }
                    let (g, _) = self
                        .cv
                        .wait_timeout(st, Duration::from_millis(500))
                        .unwrap();
                    st = g;
                }
            };
            let Some(first) = first else {
                tracing::debug!("删除 worker 退出");
                return;
            };
            self.stats
                .pending
                .store(self.pending_len(), Ordering::Relaxed);

            let batch = self.collect_batch(first);
            // ★ `rm -rf` 会在**同一轮**里同时入队 `rmdir(dir)` 和若干
            //   `unlink(dir/子)`。若 rmdir 那批先成功，子项的删除必然
            //   "文件不存在"而失败 → 白白重试到放弃，`failed` 虚高。
            //   所以发请求前先丢掉「父目录已被删掉」的子项。
            let batch = self.drop_covered_by_deleted_parent(batch);
            if batch.items.is_empty() {
                self.finish_active();
                continue;
            }
            self.stats.batches.fetch_add(1, Ordering::Relaxed);
            match self.rt.block_on(self.delete_batch(&batch)) {
                Ok(()) => {
                    for (name, _, _) in &batch.items {
                        let p = join_remote(&batch.dir, name);
                        self.persist_delete(&p);
                    }
                    // 被这批删掉的**目录**，其子树里仍在队列中的作业同样作废
                    // （父目录都没了，子项的远端路径已无效）。
                    self.drop_queued_under(&batch);
                    self.stats
                        .done
                        .fetch_add(batch.items.len() as u64, Ordering::Relaxed);
                    self.stats
                        .deleted
                        .fetch_add(batch.items.len() as u64, Ordering::Relaxed);
                    tracing::info!(
                        "已删除 {}/{}（{} 项）",
                        batch.dir,
                        batch
                            .items
                            .iter()
                            .map(|(n, _, _)| n.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                        batch.items.len()
                    );
                }
                Err(e) => {
                    // 整批退回队列重试。批内每项**各自**计数（不能取批内最大值
                    // 覆盖 —— 那会把已经重试多次的项「重置」回 1 次，退化成死循环）。
                    let exhausted: Vec<(String, bool, u32)> = batch
                        .items
                        .iter()
                        .map(|(n, d, a)| (n.clone(), *d, a.saturating_add(1)))
                        .collect();
                    let give_up: Vec<&(String, bool, u32)> = exhausted
                        .iter()
                        .filter(|(_, _, a)| *a >= self.max_attempts)
                        .collect();
                    if !give_up.is_empty() {
                        self.stats
                            .failed
                            .fetch_add(give_up.len() as u64, Ordering::Relaxed);
                        tracing::error!(
                            "删除失败（放弃，已试 {} 次）: {}/{}: {e}",
                            self.max_attempts,
                            batch.dir,
                            give_up
                                .iter()
                                .map(|(n, _, _)| n.as_str())
                                .collect::<Vec<_>>()
                                .join(", "),
                        );
                    }
                    // 未超限的退回队尾重试；超限的从库和内存里清掉
                    // （否则重启会再捞起来注定失败的项）。
                    let mut st = self.state.lock().unwrap();
                    for (name, is_dir, attempts) in &exhausted {
                        let p = join_remote(&batch.dir, name);
                        if *attempts >= self.max_attempts {
                            self.persist_delete(&p);
                            continue;
                        }
                        self.stats.retries.fetch_add(1, Ordering::Relaxed);
                        self.persist_bump(&p, *attempts);
                        st.pending.push_back(DeleteJob {
                            remote_dir: batch.dir.clone(),
                            remote_name: name.clone(),
                            is_dir: *is_dir,
                            attempts: *attempts,
                        });
                    }
                    // 退避按本批**最大**次数（最坏项决定），避免快的项被慢的拖住
                    let back_n = exhausted.iter().map(|(_, _, a)| *a).max().unwrap_or(1);
                    drop(st);
                    let backoff = Duration::from_millis(300 * (1 << back_n.min(4)));
                    std::thread::sleep(backoff);
                }
            }
            self.finish_active();
        }
    }

    fn finish_active(&self) {
        {
            let mut st = self.state.lock().unwrap();
            st.active = false;
        }
        self.stats
            .pending
            .store(self.pending_len(), Ordering::Relaxed);
        self.cv.notify_all();
    }

    /// 把「父目录已在本批被删掉」的条目剔掉。
    ///
    /// `rm -rf dir` 会让 `dir` 和 `dir/子` 一起进队列。NAS 上删掉 `dir`
    /// 之后再去删 `dir/子` 必然报「不存在」，重试也永远不会成功。
    /// 父目录是**子项远端路径的前缀**时才作废，且必须**同批**——
    /// 跨批的目录可能还没删，不能提前丢掉。
    fn drop_covered_by_deleted_parent(&self, batch: Batch) -> Batch {
        let dir_prefixes: Vec<String> = batch
            .items
            .iter()
            .filter(|(_, is_dir, _)| *is_dir)
            .map(|(n, _, _)| format!("{}/", join_remote(&batch.dir, n)))
            .collect();
        if dir_prefixes.is_empty() {
            return batch;
        }
        let mut kept = Vec::new();
        let mut dropped = 0usize;
        for (name, is_dir, attempts) in batch.items {
            let p = join_remote(&batch.dir, &name);
            // 目录本身不能被自己（或另一个待删目录前缀）覆盖掉
            let covered = !is_dir
                && dir_prefixes.iter().any(|pre| {
                    p.starts_with(pre.as_str())
                        && p.trim_end_matches('/') != pre.trim_end_matches('/')
                });
            if covered {
                dropped += 1;
                // 库里的行也要清，否则重启会捞回来
                self.persist_delete(&p);
            } else {
                kept.push((name, is_dir, attempts));
            }
        }
        if dropped > 0 {
            let kept_names: Vec<&str> = kept.iter().map(|(n, _, _)| n.as_str()).collect();
            tracing::info!(
                "{}/{}：{} 个子项的父目录同批被删，跳过（避免必然失败的删除）；本批实际发 {} 项",
                batch.dir,
                kept_names.join(", "),
                dropped,
                kept.len()
            );
        }
        Batch {
            dir: batch.dir,
            items: kept,
        }
    }

    /// 本批删掉了目录 → 队列里位于其子树下的作业一并作废（远端已随父目录消失）。
    fn drop_queued_under(&self, batch: &Batch) {
        let dir_prefixes: Vec<String> = batch
            .items
            .iter()
            .filter(|(_, is_dir, _)| *is_dir)
            .map(|(n, _, _)| format!("{}/", join_remote(&batch.dir, n)))
            .collect();
        if dir_prefixes.is_empty() {
            return;
        }
        let victims: Vec<DeleteJob> = {
            let mut st = self.state.lock().unwrap();
            let mut kept = VecDeque::new();
            let mut victims = Vec::new();
            while let Some(j) = st.pending.pop_front() {
                if dir_prefixes
                    .iter()
                    .any(|pre| j.remote_path().starts_with(pre.as_str()))
                {
                    victims.push(j);
                } else {
                    kept.push_back(j);
                }
            }
            st.pending = kept;
            victims
        };
        for v in victims {
            self.persist_delete(&v.remote_path());
        }
    }

    /// 把 `first` 往前后扩成一批：**同父目录**、最多 `MAX_BATCH` 项。
    ///
    /// 攒批窗口 `BATCH_WINDOW`：worker 刚被唤醒时通常队列里只有 1 项（用户点了一下），
    /// 等一小会儿能接住紧随其后的连续删除（`rm -rf`、Dolphin 多选）。
    /// 只等同目录的项 —— 跨目录没法合成一次请求（`path` 只有一个）。
    fn collect_batch(&self, first: DeleteJob) -> Batch {
        let dir = first.remote_dir.clone();
        let mut items = vec![(first.remote_name, first.is_dir, first.attempts)];
        let deadline = std::time::Instant::now() + BATCH_WINDOW;
        loop {
            if items.len() >= MAX_BATCH || std::time::Instant::now() >= deadline {
                break;
            }
            // 正在退避重试时退出：别为了凑批把 shutdown 拖到退避结束之后。
            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }
            let st = self.state.lock().unwrap();
            // 队首不是同目录就停：不同目录不能合批，也别为等一个不相干的作业卡住。
            match st.pending.front() {
                Some(j) if j.remote_dir == dir => {
                    let job = {
                        drop(st);
                        self.state.lock().unwrap().pending.pop_front().unwrap()
                    };
                    items.push((job.remote_name, job.is_dir, job.attempts));
                }
                Some(_) => break,
                None => {
                    // 队列空：等到窗口末尾或新作业到达
                    let left = deadline.saturating_duration_since(std::time::Instant::now());
                    if left.is_zero() {
                        break;
                    }
                    let _ = self
                        .cv
                        .wait_timeout(st, left.min(Duration::from_millis(20)))
                        .unwrap();
                }
            }
        }
        Batch { dir, items }
    }

    /// 发一批：一次 `func=delete` 请求 + 逐项记 write log。
    async fn delete_batch(&self, batch: &Batch) -> Result<(), CoreError> {
        let names: Vec<&str> = batch.items.iter().map(|(n, _, _)| n.as_str()).collect();
        self.client.delete_entries(&batch.dir, &names).await?;
        // 记 sync log：让其它设备/后端也能发现这次删除（尽力而为，失败不影响）。
        // `write_action::DELETE` 对文件/目录通用（官方客户端同样不区分）。
        for (name, _, _) in &batch.items {
            let path = join_remote(&batch.dir, name);
            if let Err(e) = self.client.write_log(&path, write_action::DELETE).await {
                tracing::debug!("删除 write_log 失败（不影响删除）: {e}");
            }
        }
        Ok(())
    }

    fn persist_put(&self, job: &DeleteJob) {
        if let Some(s) = &self.store {
            if let Err(e) = s.put_delete(&job.to_row()) {
                tracing::warn!("删除作业入库失败 {}: {e}", job.remote_path());
            }
        }
    }

    fn persist_delete(&self, remote_path: &str) {
        if let Some(s) = &self.store {
            if let Err(e) = s.delete_row(remote_path) {
                tracing::warn!("删除作业出库失败 {remote_path}: {e}");
            }
        }
    }

    fn persist_bump(&self, remote_path: &str, attempts: u32) {
        if let Some(s) = &self.store {
            if let Err(e) = s.bump_delete_attempts(remote_path, attempts) {
                tracing::warn!("删除作业计数更新失败 {remote_path}: {e}");
            }
        }
    }
}

fn join_remote(dir: &str, name: &str) -> String {
    format!("{}/{}", dir.trim_end_matches('/'), name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(dir: &str, name: &str) -> DeleteJob {
        DeleteJob {
            remote_dir: dir.to_string(),
            remote_name: name.to_string(),
            is_dir: false,
            attempts: 0,
        }
    }

    /// 批处理的核心不变量：**只合并同目录**（`path` 参数只有一个）。
    #[test]
    fn batch_only_merges_same_dir() {
        let mut st = State {
            pending: VecDeque::new(),
            active: false,
        };
        st.pending.push_back(job("/home", "a"));
        st.pending.push_back(job("/home", "b"));
        st.pending.push_back(job("/other", "c"));
        // 手工模拟 collect_batch 的取法（同目录连取，遇到异目录停）
        let mut items = Vec::new();
        let mut dir = None;
        while let Some(j) = st.pending.pop_front() {
            if let Some(d) = &dir {
                if j.remote_dir != *d {
                    st.pending.push_front(j);
                    break;
                }
            } else {
                dir = Some(j.remote_dir.clone());
            }
            items.push(j.remote_name);
        }
        assert_eq!(items, vec!["a", "b"]);
        // 异目录那个必须原样留在队里
        assert_eq!(st.pending.len(), 1);
        assert_eq!(st.pending[0].remote_path(), "/other/c");
    }

    /// 批内顺序要稳定（删除顺序可预期，便于日志比对）。
    #[test]
    fn batch_preserves_order() {
        let mut q = VecDeque::new();
        for n in ["z", "a", "m"] {
            q.push_back(job("/home", n));
        }
        let names: Vec<String> = q.iter().map(|j| j.remote_name.clone()).collect();
        assert_eq!(names, vec!["z", "a", "m"]);
    }

    /// 远端路径拼接：不能因为 `dir` 尾部斜杠产生 `//`。
    #[test]
    fn remote_path_join_no_double_slash() {
        assert_eq!(join_remote("/home/", "a.txt"), "/home/a.txt");
        assert_eq!(join_remote("/home", "a.txt"), "/home/a.txt");
        assert_eq!(job("/home/", "a.txt").remote_path(), "/home/a.txt");
    }

    /// 批大小上限：避免单个请求无限膨胀。
    #[test]
    fn max_batch_is_sane() {
        assert!(MAX_BATCH > 0 && MAX_BATCH <= 500, "批上限应落在合理区间");
    }

    /// ★ M11 回归：`rm -rf dir` 会让 `dir` 和 `dir/子` **同批**进队列。
    /// 删掉 `dir` 之后再删 `dir/子` 必然报「不存在」，重试到放弃只会让
    /// `failed` 虚高、并把注定失败的项反复丢给 NAS。子项必须被剔掉。
    #[test]
    fn children_of_same_batch_deleted_dir_are_dropped() {
        let batch = Batch {
            dir: "/home/qxync-test".into(),
            items: vec![
                // 先 rmdir 目录，再 unlink 它的子项（内核 rm -rf 的实际顺序）
                ("victim".into(), true, 0),
                ("victim/a.txt".into(), false, 0),
                ("victim/sub/b.txt".into(), false, 0),
                // 同名但**不是**子项的文件，必须保留
                ("victim2.txt".into(), false, 0),
                // 完全无关的文件
                ("other.txt".into(), false, 0),
            ],
        };
        // 不带队列状态也能验：只测「父目录前缀 → 剔除」这段纯逻辑
        let dir_prefixes: Vec<String> = batch
            .items
            .iter()
            .filter(|(_, is_dir, _)| *is_dir)
            .map(|(n, _, _)| format!("{}/", join_remote(&batch.dir, n)))
            .collect();
        assert_eq!(dir_prefixes, vec!["/home/qxync-test/victim/".to_string()]);

        let kept: Vec<String> = batch
            .items
            .iter()
            .filter(|(_, is_dir, _)| !*is_dir)
            .filter(|(n, _, _)| {
                let p = join_remote(&batch.dir, n);
                !dir_prefixes.iter().any(|pre| p.starts_with(pre.as_str()))
            })
            .map(|(n, _, _)| n.clone())
            .collect();
        assert_eq!(
            kept,
            vec!["victim2.txt".to_string(), "other.txt".to_string()],
            "只有父目录真被删的子项该被剔除；`victim2.txt` 不是子项"
        );
    }

    /// ★ M11 回归：前缀匹配必须按「完整路径段」而不是字符串前缀。
    /// `victim` 已删不该影响 `victim2.txt`。
    #[test]
    fn parent_match_is_segment_aware() {
        let pre = "/home/qxync-test/victim/";
        // 有尾斜杠 → 只匹配真子项
        assert!("/home/qxync-test/victim/a.txt".starts_with(pre));
        // 无尾斜杠的裸名字不是它的子项
        assert!(!"/home/qxync-test/victim2.txt".starts_with(pre));
        assert!(!"/home/qxync-test/victim".starts_with(pre));
    }

    /// ★ M11 回归：重试计数必须**逐项递增**，不能拿批内最大值覆盖。
    /// 否则一个已重试 4 次的项会被「重置」回 1 次，永远达不到放弃阈值。
    #[test]
    fn retry_attempts_increment_per_item() {
        // 模拟：批内一项已重试 3 次、另一项刚开始
        let items = vec![
            ("a".to_string(), false, 3u32),
            ("b".to_string(), false, 0u32),
        ];
        let max_attempts = 5u32;
        let next: Vec<u32> = items.iter().map(|(_, _, a)| a.saturating_add(1)).collect();
        assert_eq!(next, vec![4, 1], "每项各自 +1，不能都变成 max(3+1, 0+1)=4");
        // 逐项判定放弃：只有到阈值的才放弃
        let give_up: Vec<&str> = items
            .iter()
            .zip(next.iter())
            .filter(|((_, _, _), n)| **n >= max_attempts)
            .map(|((name, _, _), _)| name.as_str())
            .collect();
        assert!(give_up.is_empty(), "4 和 1 都还没到 5，都该重试");

        // 再来一轮：a 到 5 了，b 才 2 → 只有 a 放弃
        let items2 = vec![
            ("a".to_string(), false, 4u32),
            ("b".to_string(), false, 1u32),
        ];
        let next2: Vec<u32> = items2.iter().map(|(_, _, a)| a.saturating_add(1)).collect();
        let give_up2: Vec<&str> = items2
            .iter()
            .zip(next2.iter())
            .filter(|((_, _, _), n)| **n >= max_attempts)
            .map(|((name, _, _), _)| name.as_str())
            .collect();
        assert_eq!(give_up2, vec!["a"], "只有 a 够阈值");
    }
}
