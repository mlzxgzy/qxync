//! 上传队列：本地改动 → 服务端。
//!
//! 设计要点（`docs/M2b-写路径.md`；M5 起持久化改走 SQLite）：
//!
//! * **先落盘再改数据**：`enqueue` 先把作业 upsert 进状态库 `uploads` 表（`<marker_dir>/queue.db`），
//!   再改内存队列。中途崩溃 → 重启时 `store.uploads()` 重新入队，绝不丢改动。
//! * **库 = 未完成作业**：上传成功 / 重试超限放弃 / 取消，都会删掉对应的行；
//!   所以重启时表里剩下的就是「还没传完」的作业（对应 M5 之前「删 `.dirty` 标记」的语义）。
//! * 单个 worker 线程串行消费（同一文件天然合并，避免把半成品推上去）；
//!   失败指数退避重试，超过 `max_attempts` 记为 failed 并由 `status` 暴露。
//! * M5 一次性迁移：老版本留在 `marker_dir` 里的 `<hash>.dirty`（JSON）会在 `new()` 时
//!   读出来 → `put_upload` 入库 → 改名成 `*.dirty.migrated`（**保留备份，不删**）。迁移后不再依赖它。
//! * **降级取舍**：状态库打不开时 `new()` 不返回 Err（挂载不该因为队列库坏了就起不来），
//!   而是打 warn 退化成「无持久化」队列：内存语义照旧，但崩溃会丢未完成作业。
//! * 上传成功后必须 `stat&settime=1&mtime=` 对齐时间戳（否则服务端判定「未同步」）。
//! * `qbox_write_log` 尽力而为（服务端不校验 action；未注册同步对时不会落盘，见 client 注释）。

use qxync_client::{write_action, Client};
use qxync_core::store::{Store, UploadRow};
use qxync_core::Error as CoreError;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// 上传队列状态库文件名，放在 `marker_dir` 下。
///
/// 不复用 per-NAS 的 `sync.db`：`UploadQueue::new` 只拿得到队列目录，
/// 队列单独一个库最省事，迁移老 `.dirty` 也只需扫这一个目录。
const QUEUE_DB_FILE: &str = "queue.db";

/// 一个待上传的改动。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadJob {
    pub remote_dir: String,
    pub remote_name: String,
    /// 本地（缓存）文件：内容源。
    pub local: PathBuf,
    /// 期望的服务端 mtime（epoch 秒）。
    pub mtime: i64,
    #[serde(default)]
    pub attempts: u32,
    /// 临时内容（冲突副本的 stash）：上传成功后把本地文件删掉。
    #[serde(default)]
    pub ephemeral: bool,
}

impl UploadJob {
    pub fn remote_path(&self) -> String {
        format!(
            "{}/{}",
            self.remote_dir.trim_end_matches('/'),
            self.remote_name
        )
    }

    /// 转成状态库的行（主键 `remote_path` 由 `UploadRow` 派生，和本类型一致）。
    fn to_row(&self) -> UploadRow {
        UploadRow {
            remote_dir: self.remote_dir.clone(),
            remote_name: self.remote_name.clone(),
            local: self.local.clone(),
            mtime: self.mtime,
            attempts: self.attempts,
            ephemeral: self.ephemeral,
        }
    }

    fn from_row(r: &UploadRow) -> Self {
        Self {
            remote_dir: r.remote_dir.clone(),
            remote_name: r.remote_name.clone(),
            local: r.local.clone(),
            mtime: r.mtime,
            attempts: r.attempts,
            ephemeral: r.ephemeral,
        }
    }
}

#[derive(Debug, Default)]
pub struct UploadStats {
    pub done: AtomicU64,
    pub failed: AtomicU64,
    pub bytes: AtomicU64,
    pub retries: AtomicU64,
    /// 当前排队中的作业数（由 state 派生，冗余存一份方便无锁读）。
    pub pending: AtomicU64,
}

/// 队列快照，给 `status`/xattr 用。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct UploadSnapshot {
    pub pending: u64,
    /// 是否正有作业在上传（在途作业不占 `pending`，但**内容可能还没落地** —
    /// 冲突判定/测试等待都必须看这个，实测踩过：pending=0 时最后一次上传还在飞）。
    pub active: bool,
    pub done: u64,
    pub failed: u64,
    pub retries: u64,
    pub bytes: u64,
}

/// 上传成功回调（M3：让 FUSE 节点清掉 `dirty`，否则脱水永远被 dirty 挡住）。
pub type SuccessHook = Arc<dyn Fn(&str) + Send + Sync>;

/// 调用成功回调 —— **回调 panic 绝不能打死上传 worker**。
///
/// ★ M7 实测踩过：回调跑在**上传 worker 的 OS 线程**里（不是 tokio worker），
/// 里面误用 `tokio::spawn` 会立刻 panic（"must be called from the context of a Tokio
/// runtime"），worker 线程随之死掉 —— 表现是「上传队列永远卡住、drain 超时、
/// 整个 daemon 像挂了」（fuse-matrix 的 M2c 冲突段卡了 3 分钟）。
/// 这里把回调隔离起来：它只能坏它自己，队列必须继续跑。
pub(crate) fn invoke_success_hook(hook: Option<SuccessHook>, remote: &str) {
    let Some(hook) = hook else {
        return;
    };
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook(remote))).is_err() {
        tracing::error!("上传成功回调 panic（已隔离，队列继续）: {remote}");
    }
}

pub struct UploadQueue {
    client: Arc<Client>,
    rt: tokio::runtime::Handle,
    /// 状态库句柄；`None` = 降级模式（库打不开，队列只在内存里）。
    store: Option<Arc<Store>>,
    max_attempts: u32,
    state: Mutex<State>,
    cv: Condvar,
    shutdown: AtomicBool,
    stats: UploadStats,
    success_hook: Mutex<Option<SuccessHook>>,
}

struct State {
    pending: VecDeque<UploadJob>,
    /// 正在上传的作业（用于 drain 等待）
    active: bool,
    /// ★ M8.4：**已被取消**的远端路径。
    ///
    /// 为什么需要它：`cancel()` 原本只能清掉「还没被 worker 取走」的作业，
    /// 已经被取走但**还没发出去**的那个取消不了。M2c 的冲突处理靠
    /// 「cancel + drain + 重新 stat」来避免「在途上传把远端改回本地内容」，
    /// 那个窄窗口就落在这里。worker 在真正发请求前查一次这个集合即可关掉窗口。
    cancelled: std::collections::BTreeSet<String>,
}

impl UploadQueue {
    /// 建队列；状态库里未完成的作业会被**重新入队**（崩溃恢复），
    /// 残留的 `.dirty` 标记会一次性迁移进库（见模块注释）。
    pub fn new(
        client: Arc<Client>,
        rt: tokio::runtime::Handle,
        marker_dir: PathBuf,
    ) -> std::io::Result<Arc<Self>> {
        std::fs::create_dir_all(&marker_dir)?;

        // 取舍：状态库打不开（权限/磁盘/库损坏）时**不返回 Err** —— 挂载不该因为队列库
        // 坏了就起不来。代价是降级成「无持久化」队列：内存语义（串行/退避/重试）照旧，
        // 但崩溃会丢未完成作业；日志显眼提示。
        let db_path = marker_dir.join(QUEUE_DB_FILE);
        let store = match Store::open(&db_path) {
            Ok(s) => Some(Arc::new(s)),
            Err(e) => {
                tracing::warn!(
                    "上传队列状态库 {} 打不开，降级为无持久化队列（崩溃会丢未完成上传）: {e}",
                    db_path.display()
                );
                None
            }
        };

        let mut pending = VecDeque::new();
        match &store {
            Some(s) => {
                // 一次性迁移老版本的 `.dirty` JSON 标记：读 → put_upload → 改名备份。
                let n = migrate_dirty_markers(s, &marker_dir);
                if n > 0 {
                    tracing::info!("已把 {n} 个 .dirty 标记迁入上传队列状态库");
                }
                match s.uploads() {
                    Ok(rows) => {
                        for r in rows {
                            let job = UploadJob::from_row(&r);
                            tracing::info!("恢复未完成的上传: {}", job.remote_path());
                            pending.push_back(job);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("读取上传队列状态库失败（本次从空队列开始，重启再试）: {e}")
                    }
                }
            }
            None => {
                // 降级模式没有库可用：尽力从残留 `.dirty` 恢复一次内存队列。
                // **不改名**：改名等于声称「已持久化」，而内存里的作业崩溃即丢。
                for (path, job) in scan_dirty_markers(&marker_dir) {
                    match job {
                        Some(job) => {
                            tracing::warn!(
                                "降级模式：从 {} 恢复 {}",
                                path.display(),
                                job.remote_path()
                            );
                            pending.push_back(job);
                        }
                        None => tracing::warn!("忽略损坏的标记文件 {}", path.display()),
                    }
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
                cancelled: std::collections::BTreeSet::new(),
            }),
            cv: Condvar::new(),
            shutdown: AtomicBool::new(false),
            success_hook: Mutex::new(None),
            stats: UploadStats {
                pending: AtomicU64::new(n),
                ..Default::default()
            },
        }))
    }

    /// 注册「上传成功」回调（按远端路径调用）。daemon 用它把节点 `dirty` 清掉。
    pub fn set_success_hook(&self, hook: SuccessHook) {
        *self.success_hook.lock().unwrap() = Some(hook);
    }

    /// 入队：**先写状态库，再改内存队列**（崩溃安全）。
    ///
    /// 入库失败不会让调用方失败（写路径不能被队列库拖死），只打 warn 并继续用内存队列 —
    /// 那种情况下这次改动的崩溃安全就没了，日志里能看出来。
    pub fn enqueue(&self, job: UploadJob) -> std::io::Result<()> {
        self.persist_put(&job);
        {
            let mut st = self.state.lock().unwrap();
            // 同一路径已在队列里 → 用新作业替换（后写覆盖先写；库里由 upsert 保证一行）
            st.pending.retain(|j| j.remote_path() != job.remote_path());
            st.pending.push_back(job);
        }
        self.stats
            .pending
            .store(self.pending_len(), Ordering::Relaxed);
        self.cv.notify_all();
        Ok(())
    }

    /// 是否有作业正在上传（在途）。
    pub fn is_active(&self) -> bool {
        self.state.lock().unwrap().active
    }

    /// 取消某路径**还没开始**的上传作业（含状态库里的行）。
    /// 返回是否取消了作业；已经在途的那个取消不了，要用 [`Self::drain`] 等它结束。
    pub fn cancel(&self, remote_path: &str) -> bool {
        {
            // 已取走但未发出的作业也要能取消（见 State::cancelled 的说明）
            let mut st = self.state.lock().unwrap();
            st.cancelled.insert(remote_path.to_string());
        }
        let removed: Vec<UploadJob> = {
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
        if !removed.is_empty() {
            self.persist_delete(remote_path);
            self.stats
                .pending
                .store(self.pending_len(), Ordering::Relaxed);
            tracing::info!("已取消 {remote_path} 的 {} 个待上传作业", removed.len());
        }
        removed.is_empty()
    }

    /// 该远端路径是否还有未完成的上传（含崩溃恢复出来的作业）。
    pub fn has_pending(&self, remote_path: &str) -> bool {
        self.state
            .lock()
            .unwrap()
            .pending
            .iter()
            .any(|j| j.remote_path() == remote_path)
    }

    fn pending_len(&self) -> u64 {
        self.state.lock().unwrap().pending.len() as u64
    }

    pub fn snapshot(&self) -> UploadSnapshot {
        UploadSnapshot {
            pending: self.pending_len(),
            active: self.is_active(),
            done: self.stats.done.load(Ordering::Relaxed),
            failed: self.stats.failed.load(Ordering::Relaxed),
            retries: self.stats.retries.load(Ordering::Relaxed),
            bytes: self.stats.bytes.load(Ordering::Relaxed),
        }
    }

    // ------------------------------------------------------------ 状态库写穿
    //
    // 都是「尽力而为」：失败只 warn，绝不 panic、绝不阻塞 worker ——
    // 内存队列仍然是权威工作副本，持久化只是崩溃恢复的保障。

    /// 入库（`uploads` 表 upsert）。失败仍继续入内存队列。
    fn persist_put(&self, job: &UploadJob) {
        match self.store.as_deref() {
            Some(s) => {
                if let Err(e) = s.put_upload(&job.to_row()) {
                    tracing::warn!(
                        "上传队列落库失败（作业仍在内存队列，重启会丢）: {}: {e}",
                        job.remote_path()
                    );
                }
            }
            None => tracing::warn!(
                "上传队列无持久化（状态库不可用），入队仅存在于内存: {}",
                job.remote_path()
            ),
        }
    }

    /// 重试计数变化落库。
    fn persist_bump(&self, remote_path: &str, attempts: u32) {
        if let Some(s) = self.store.as_deref() {
            if let Err(e) = s.bump_upload_attempts(remote_path, attempts) {
                tracing::warn!("上传重试次数落库失败: {remote_path}: {e}");
            }
        }
    }

    /// 作业结束（成功/放弃/取消）→ 删行，保持「库 = 未完成作业」。
    fn persist_delete(&self, remote_path: &str) {
        if let Some(s) = self.store.as_deref() {
            match s.delete_upload(remote_path) {
                Ok(true) => {}
                Ok(false) => {
                    tracing::debug!("状态库里没有 {remote_path} 的行（可能已被删或被新作业覆盖）")
                }
                Err(e) => tracing::warn!("删除上传队列行失败: {remote_path}: {e}"),
            }
        }
    }

    /// 起 worker 线程（串行消费）。
    pub fn spawn_worker(self: &Arc<Self>) -> std::io::Result<std::thread::JoinHandle<()>> {
        let q = self.clone();
        std::thread::Builder::new()
            .name("qxync-upload".into())
            .spawn(move || q.worker_loop())
    }

    /// 请求退出；worker 会把当前作业做完再退。
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.cv.notify_all();
    }

    /// 等队列清空（测试/卸载时用）。
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

    fn worker_loop(self: Arc<Self>) {
        loop {
            let job = {
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
            let Some(job) = job else {
                tracing::debug!("上传 worker 退出");
                return;
            };
            self.stats
                .pending
                .store(self.pending_len(), Ordering::Relaxed);

            // ★ M8.4：**仅验收用**的注入点 —— 取到作业后先等一会儿再发。
            //
            // 为什么需要它：M2c 的「三向冲突」要求「本地有未上传改动 **且** 远端也变了」。
            // 真实写路径是写穿的（写完几乎立刻上传），这个窗口只有几十毫秒，
            // 端到端没法稳定复现冲突。停在这里 + 上面的 `cancelled` 集合，
            // 就能让 `m84-matrix.sh` 逐个策略跑出确定性的冲突产物。
            // **默认 0（不等待）**，对生产行为没有任何影响。
            let hold_ms: u64 = std::env::var("QXNYC_TEST_UPLOAD_HOLD_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            if hold_ms > 0 {
                std::thread::sleep(Duration::from_millis(hold_ms.min(60_000)));
            }
            let cancelled = {
                let mut st = self.state.lock().unwrap();
                st.cancelled.remove(&job.remote_path())
            };
            if cancelled {
                tracing::info!("上传作业已被取消，跳过: {}", job.remote_path());
                self.persist_delete(&job.remote_path());
                {
                    let mut st = self.state.lock().unwrap();
                    st.active = false;
                }
                self.stats
                    .pending
                    .store(self.pending_len(), Ordering::Relaxed);
                self.cv.notify_all();
                continue;
            }

            match self.rt.block_on(self.upload_one(&job)) {
                Ok(()) => {
                    self.persist_delete(&job.remote_path());
                    if job.ephemeral {
                        // 冲突副本的 stash 是一次性的：传完就删
                        let _ = std::fs::remove_file(&job.local);
                    }
                    invoke_success_hook(
                        self.success_hook.lock().unwrap().clone(),
                        &job.remote_path(),
                    );
                    let n = std::fs::metadata(&job.local).map(|m| m.len()).unwrap_or(0);
                    self.stats.done.fetch_add(1, Ordering::Relaxed);
                    self.stats.bytes.fetch_add(n, Ordering::Relaxed);
                    tracing::info!("已上传 {} ({} 字节)", job.remote_path(), n);
                }
                Err(e) => {
                    let attempts = job.attempts + 1;
                    if attempts >= self.max_attempts {
                        self.stats.failed.fetch_add(1, Ordering::Relaxed);
                        tracing::error!(
                            "上传失败（放弃，已试 {attempts} 次）: {}: {e}",
                            job.remote_path()
                        );
                        // M5：库 = 未完成作业 —— 放弃的作业把行删掉（等价于当年删 `.dirty`），
                        // 否则每次重启都会把注定失败的作业再捞起来。失败计数由 stats/日志留痕。
                        self.persist_delete(&job.remote_path());
                    } else {
                        self.stats.retries.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!("上传失败（第 {attempts} 次）: {}: {e}", job.remote_path());
                        // 先写库再改内存：重启后重试计数不倒退
                        self.persist_bump(&job.remote_path(), attempts);
                        let backoff = Duration::from_millis(300 * (1 << attempts.min(4)));
                        std::thread::sleep(backoff);
                        let mut retry = job.clone();
                        retry.attempts = attempts;
                        let mut st = self.state.lock().unwrap();
                        st.pending.push_back(retry);
                    }
                }
            }
            {
                let mut st = self.state.lock().unwrap();
                st.active = false;
            }
            self.stats
                .pending
                .store(self.pending_len(), Ordering::Relaxed);
            self.cv.notify_all();
        }
    }

    /// 单个作业：上传内容 → 对齐 mtime → 记 write log（尽力而为）。
    async fn upload_one(&self, job: &UploadJob) -> Result<(), CoreError> {
        let bytes = std::fs::read(&job.local)?;
        self.client
            .upload_bytes(&job.remote_dir, &job.remote_name, bytes)
            .await?;
        if job.mtime > 0 {
            self.client
                .set_mtime(&job.remote_dir, &job.remote_name, job.mtime)
                .await?;
        }
        if let Err(e) = self
            .client
            .write_log(&job.remote_path(), write_action::UPSERT_FILE)
            .await
        {
            tracing::debug!("qbox_write_log 失败（不影响上传）: {e}");
        }
        Ok(())
    }

    /// 删除远端条目 + 记 write log（删除同样是「本地改动」）。
    pub async fn delete_remote(&self, dir: &str, name: &str) -> Result<(), CoreError> {
        self.client.delete_entry(dir, name).await?;
        let path = format!("{}/{}", dir.trim_end_matches('/'), name);
        if let Err(e) = self.client.write_log(&path, write_action::DELETE).await {
            tracing::debug!("qbox_write_log(delete) 失败: {e}");
        }
        Ok(())
    }
}

/// 扫老格式标记：`(文件, Some(作业))` 可解析，`(文件, None)` 损坏。
///
/// 只看 `*.dirty`（`*.dirty.migrated` 的扩展名是 `migrated`，不会被重复扫到）。
fn scan_dirty_markers(dir: &Path) -> Vec<(PathBuf, Option<UploadJob>)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e == "dirty").unwrap_or(false) {
            let job = std::fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice::<UploadJob>(&b).ok());
            out.push((path, job));
        }
    }
    out
}

/// M5 一次性迁移：`.dirty` → `uploads` 表，然后把文件改名成 `*.dirty.migrated`
/// （保留备份，不删）。返回成功入库的作业数。
///
/// 入库失败的文件**不改名**，留着下次启动再试（改名了就等于丢掉这份作业）。
fn migrate_dirty_markers(store: &Store, dir: &Path) -> usize {
    let mut migrated = 0;
    for (path, job) in scan_dirty_markers(dir) {
        let archived = path.with_extension("dirty.migrated");
        let Some(job) = job else {
            tracing::warn!("忽略损坏的标记文件 {}（改名备份）", path.display());
            let _ = std::fs::rename(&path, &archived);
            continue;
        };
        match store.put_upload(&job.to_row()) {
            Ok(()) => match std::fs::rename(&path, &archived) {
                Ok(()) => {
                    migrated += 1;
                    tracing::info!(
                        "迁移上传标记 {} → 状态库（备份 {}）",
                        job.remote_path(),
                        archived.display()
                    );
                }
                Err(e) => tracing::warn!(
                    "作业 {} 已入库，但备份 {} 改名失败: {e}",
                    job.remote_path(),
                    path.display()
                ),
            },
            Err(e) => tracing::warn!(
                "迁移 {} 入库失败（保留原文件，下次启动再试）: {e}",
                path.display()
            ),
        }
    }
    migrated
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    static N: AtomicU32 = AtomicU32::new(0);

    /// ★ M7 回归：成功回调 panic 不能打死上传 worker（真 bug：回调里 `tokio::spawn`
    /// 在非 tokio 线程 panic → worker 死 → 队列永久卡住）。
    #[test]
    fn panicking_success_hook_is_isolated() {
        use std::sync::atomic::AtomicU64;
        let called = Arc::new(AtomicU64::new(0));
        let c1 = called.clone();
        invoke_success_hook(
            Some(Arc::new(move |_| {
                c1.fetch_add(1, Ordering::Relaxed);
                panic!("hook 里的 bug（真实场景：tokio::spawn 在非 tokio 线程）");
            })),
            "/home/boom.txt",
        );
        // 第一次 panic 被隔离 → 后续回调照常执行（worker 还活着）
        let c2 = called.clone();
        invoke_success_hook(
            Some(Arc::new(move |_| {
                c2.fetch_add(1, Ordering::Relaxed);
            })),
            "/home/ok.txt",
        );
        assert_eq!(
            called.load(Ordering::Relaxed),
            2,
            "回调 panic 不能中断调用点"
        );
        // 没有回调也不能有事
        invoke_success_hook(None, "/home/none.txt");
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "qxync-upload-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
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

    /// 建一个队列（不 spawn worker，测试只验证持久化语义）。
    /// 返回 Runtime 是为了让 `Handle` 背后的运行时活着。
    fn test_queue(marker_dir: &Path) -> (Arc<UploadQueue>, tokio::runtime::Runtime) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let client = Arc::new(Client::new(&test_link()).unwrap());
        let q = UploadQueue::new(client, rt.handle().clone(), marker_dir.to_path_buf()).unwrap();
        (q, rt)
    }

    fn job(dir: &Path, name: &str) -> UploadJob {
        UploadJob {
            remote_dir: "/home/qxync-test/".into(),
            remote_name: name.into(),
            local: dir.join("cache").join(name),
            mtime: 1234,
            attempts: 0,
            ephemeral: false,
        }
    }

    #[test]
    fn job_remote_path_is_stable_and_row_upserts() {
        let j = UploadJob {
            remote_dir: "/home/qxync-test/".into(),
            remote_name: "a b.txt".into(),
            local: PathBuf::from("/tmp/x"),
            mtime: 1,
            attempts: 0,
            ephemeral: false,
        };
        // remote_path 必须稳定（库里主键、write log、dirty 判定都靠它）
        assert_eq!(j.remote_path(), "/home/qxync-test/a b.txt");
        assert_eq!(
            UploadJob {
                remote_dir: "/home/qxync-test".into(),
                ..j.clone()
            }
            .remote_path(),
            "/home/qxync-test/a b.txt",
            "尾斜杠不能改变 remote_path"
        );
        assert_ne!(
            UploadJob {
                remote_name: "b.txt".into(),
                ..j.clone()
            }
            .remote_path(),
            j.remote_path()
        );

        // 入库：同 remote_path 再写一次是 upsert（一行），字段被新值覆盖
        let store = Store::open_in_memory().unwrap();
        store.put_upload(&j.to_row()).unwrap();
        let updated = UploadJob {
            mtime: 99,
            attempts: 3,
            ephemeral: true,
            ..j.clone()
        };
        store.put_upload(&updated.to_row()).unwrap();
        let rows = store.uploads().unwrap();
        assert_eq!(rows.len(), 1, "同 remote_path 只能有一行");
        assert_eq!(rows[0].remote_path(), j.remote_path());
        assert_eq!(rows[0].mtime, 99);
        assert_eq!(rows[0].attempts, 3);
        assert!(rows[0].ephemeral);
        assert_eq!(UploadJob::from_row(&rows[0]).remote_path(), j.remote_path());
    }

    #[test]
    fn enqueue_persists_row_with_unicode_and_space_name() {
        let dir = tmpdir("enqueue");
        let marker = dir.join("queue");
        let (q, _rt) = test_queue(&marker);
        let j = job(&dir, "中文 名字.txt");
        q.enqueue(j.clone()).unwrap();

        // 入队后库里立刻就有这一行，字段完整（含中文/空格文件名）
        let store = Store::open(marker.join(QUEUE_DB_FILE)).unwrap();
        let rows = store.uploads().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].remote_dir, "/home/qxync-test/");
        assert_eq!(rows[0].remote_name, "中文 名字.txt");
        assert_eq!(rows[0].remote_path(), "/home/qxync-test/中文 名字.txt");
        assert_eq!(rows[0].local, j.local);
        assert_eq!(rows[0].mtime, 1234);
        assert_eq!(rows[0].attempts, 0);
        assert!(!rows[0].ephemeral);

        // 同路径再入队 → 库里仍一行，内容被覆盖（upsert）
        let mut again = j.clone();
        again.mtime = 4321;
        again.attempts = 2;
        q.enqueue(again).unwrap();
        let rows = Store::open(marker.join(QUEUE_DB_FILE))
            .unwrap()
            .uploads()
            .unwrap();
        assert_eq!(rows.len(), 1, "同 remote_path 的重复入队是 upsert");
        assert_eq!(rows[0].mtime, 4321);
        assert_eq!(rows[0].attempts, 2);

        // cancel 也要删掉库里的行（库 = 未完成作业）
        q.cancel("/home/qxync-test/中文 名字.txt");
        assert!(Store::open(marker.join(QUEUE_DB_FILE))
            .unwrap()
            .uploads()
            .unwrap()
            .is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_recovers_pending_jobs_from_store() {
        let dir = tmpdir("recover");
        let marker = dir.join("queue");
        {
            let (q, _rt) = test_queue(&marker);
            q.enqueue(job(&dir, "恢复 一.txt")).unwrap();
            q.enqueue(job(&dir, "恢复二.bin")).unwrap();
            assert!(q.has_pending("/home/qxync-test/恢复 一.txt"));
            // 队列 drop = 模拟进程退出（库连接随之关闭）
        }

        // 新队列指向同一个 queue.db → 未完成作业恢复出来
        let (q2, _rt2) = test_queue(&marker);
        assert_eq!(q2.snapshot().pending, 2);
        assert!(q2.has_pending("/home/qxync-test/恢复 一.txt"));
        assert!(q2.has_pending("/home/qxync-test/恢复二.bin"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_dirty_markers_are_migrated_and_archived() {
        let dir = tmpdir("migrate");
        let marker = dir.join("queue");
        std::fs::create_dir_all(&marker).unwrap();
        let old = job(&dir, "老 标记.txt");
        let dirty = marker.join("00000000deadbeef.dirty");
        let body = serde_json::to_vec(&old).unwrap();
        std::fs::write(&dirty, &body).unwrap();

        let (q, _rt) = test_queue(&marker);
        // 作业进了内存队列（从库里恢复）...
        assert!(q.has_pending("/home/qxync-test/老 标记.txt"));
        // ...也进了库
        let rows = Store::open(marker.join(QUEUE_DB_FILE))
            .unwrap()
            .uploads()
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].remote_name, "老 标记.txt");
        assert_eq!(rows[0].mtime, 1234);
        // 原文件改名为 *.dirty.migrated，内容保留
        assert!(!dirty.exists(), ".dirty 应已改名");
        let archived = marker.join("00000000deadbeef.dirty.migrated");
        assert_eq!(std::fs::read(&archived).unwrap(), body);

        // 二次启动幂等：不再重复迁移、不产生重复行
        let (q2, _rt2) = test_queue(&marker);
        assert_eq!(q2.snapshot().pending, 1);
        assert_eq!(
            Store::open(marker.join(QUEUE_DB_FILE))
                .unwrap()
                .uploads()
                .unwrap()
                .len(),
            1
        );
        assert!(archived.exists(), "备份不能被删");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
