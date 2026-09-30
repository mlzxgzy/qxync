//! 上传队列：本地改动 → 服务端。
//!
//! 设计要点（`docs/M2b-设计.md`）：
//!
//! * **先落标记再改数据**：写操作先把 `<marker_dir>/<hash>.dirty`（JSON）刷盘，再改缓存内容。
//!   中途崩溃 → 重启时扫描标记重新入队，绝不丢改动。
//! * 单个 worker 线程串行消费（同一文件天然合并，避免把半成品推上去）；
//!   失败指数退避重试，超过 `max_attempts` 记为 failed 并由 `status` 暴露。
//! * 上传成功后必须 `stat&settime=1&mtime=` 对齐时间戳（否则服务端判定「未同步」）。
//! * `qbox_write_log` 尽力而为（服务端不校验 action；未注册同步对时不会落盘，见 client 注释）。

use qxync_client::{write_action, Client};
use qxync_core::Error as CoreError;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

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
}

impl UploadJob {
    pub fn remote_path(&self) -> String {
        format!(
            "{}/{}",
            self.remote_dir.trim_end_matches('/'),
            self.remote_name
        )
    }
    fn marker(&self, dir: &std::path::Path) -> PathBuf {
        dir.join(format!(
            "{:016x}.dirty",
            fnv1a64(self.remote_path().as_bytes())
        ))
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
    pub done: u64,
    pub failed: u64,
    pub retries: u64,
    pub bytes: u64,
}

pub struct UploadQueue {
    client: Arc<Client>,
    rt: tokio::runtime::Handle,
    marker_dir: PathBuf,
    max_attempts: u32,
    state: Mutex<State>,
    cv: Condvar,
    shutdown: AtomicBool,
    stats: UploadStats,
}

struct State {
    pending: VecDeque<UploadJob>,
    /// 正在上传的作业（用于 drain 等待）
    active: bool,
}

impl UploadQueue {
    /// 建队列；`marker_dir` 里已有的 `.dirty` 会被**重新入队**（崩溃恢复）。
    pub fn new(
        client: Arc<Client>,
        rt: tokio::runtime::Handle,
        marker_dir: PathBuf,
    ) -> std::io::Result<Arc<Self>> {
        std::fs::create_dir_all(&marker_dir)?;
        let mut pending = VecDeque::new();
        for entry in std::fs::read_dir(&marker_dir)? {
            let path = entry?.path();
            if path.extension().map(|e| e == "dirty").unwrap_or(false) {
                match std::fs::read(&path)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<UploadJob>(&b).ok())
                {
                    Some(job) => {
                        tracing::info!("恢复未完成的上传: {}", job.remote_path());
                        pending.push_back(job);
                    }
                    None => {
                        tracing::warn!("忽略损坏的标记文件 {}", path.display());
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
        }
        let n = pending.len() as u64;
        Ok(Arc::new(Self {
            client,
            rt,
            marker_dir,
            max_attempts: 5,
            state: Mutex::new(State {
                pending,
                active: false,
            }),
            cv: Condvar::new(),
            shutdown: AtomicBool::new(false),
            stats: UploadStats {
                pending: AtomicU64::new(n),
                ..Default::default()
            },
        }))
    }

    /// 入队：**先写盘标记，再改内存队列**（崩溃安全）。
    pub fn enqueue(&self, job: UploadJob) -> std::io::Result<()> {
        let marker = job.marker(&self.marker_dir);
        let body = serde_json::to_vec(&job).map_err(std::io::Error::other)?;
        std::fs::write(&marker, body)?;
        {
            let mut st = self.state.lock().unwrap();
            // 同一路径已在队列里 → 用新作业替换（后写覆盖先写）
            st.pending.retain(|j| j.remote_path() != job.remote_path());
            st.pending.push_back(job);
        }
        self.stats
            .pending
            .store(self.pending_len(), Ordering::Relaxed);
        self.cv.notify_all();
        Ok(())
    }

    /// 该远端路径是否还有未完成的上传（含崩溃恢复出来的标记）。
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
            done: self.stats.done.load(Ordering::Relaxed),
            failed: self.stats.failed.load(Ordering::Relaxed),
            retries: self.stats.retries.load(Ordering::Relaxed),
            bytes: self.stats.bytes.load(Ordering::Relaxed),
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

            match self.rt.block_on(self.upload_one(&job)) {
                Ok(()) => {
                    let _ = std::fs::remove_file(job.marker(&self.marker_dir));
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
                        // 标记留着：重启后还会再试，人工介入也有痕迹
                    } else {
                        self.stats.retries.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!("上传失败（第 {attempts} 次）: {}: {e}", job.remote_path());
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

/// FNV-1a 64：与缓存命名共用同一套稳定哈希。
pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_remote_path_and_marker_are_stable() {
        let j = UploadJob {
            remote_dir: "/home/qxync-test/".into(),
            remote_name: "a b.txt".into(),
            local: PathBuf::from("/tmp/x"),
            mtime: 1,
            attempts: 0,
        };
        assert_eq!(j.remote_path(), "/home/qxync-test/a b.txt");
        let m1 = j.marker(std::path::Path::new("/tmp/q"));
        let m2 = j.marker(std::path::Path::new("/tmp/q"));
        assert_eq!(m1, m2);
        assert_ne!(
            m1,
            UploadJob {
                remote_name: "b.txt".into(),
                ..j.clone()
            }
            .marker(std::path::Path::new("/tmp/q"))
        );
        assert!(m1.to_string_lossy().ends_with(".dirty"));
    }
}
