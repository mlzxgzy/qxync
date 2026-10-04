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
use qxync_core::file_id::{FileId, ZERO_FILE_ID};
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
    /// ★ M15/T9：入队那一刻的稳定身份（[`ZERO_FILE_ID`] = 不知道）。
    ///
    /// 存在的唯一目的是**排队期间远端改名时不写到旧名字上**：worker 真正发之前
    /// 按它反查当前路径（见 [`UploadQueue::set_name_resolver`]），查到新名字就落到
    /// 新名字。没有它的话，排队这几十秒里对方设备把文件改名，NAS 上就会多出一个
    /// 旧名字的**幽灵文件**（内容还是新的）。
    #[serde(default)]
    pub file_id: FileId,
    /// ★ M15/T8：已经**真正发出**的字节数（不是文件大小）。
    ///
    /// 语义与 `client.upload_file` 的返回值严格一致：取自流式读取时的实际计数。
    /// 绝不能拿 `metadata` 预读的大小来填 —— 上传期间用户可能还在改同一个文件，
    /// 那是**下一版**的大小（`docs/M15` T2 验收第 4 条）。
    ///
    /// **纯运行时状态，不落库**（`uploads` 表没有这两列，`to_row`/`from_row` 都
    /// 不带它们）：进程重启后进度从 0 重来才是对的 —— 队列里那些作业本来就
    /// 一个字节都没传出去。
    #[serde(default)]
    pub bytes_sent: u64,
    /// ★ M15/T8：这次传输的总字节数（发之前 `stat` 到的本地大小）。
    ///
    /// 只用来给界面算百分比。与 `bytes_sent` 一样可能被用户后续的改动「作废」，
    /// 所以 `bytes_sent` 会被 `min` 夹住，绝不越界。
    #[serde(default)]
    pub bytes_total: u64,
}

impl UploadJob {
    pub fn remote_path(&self) -> String {
        format!(
            "{}/{}",
            self.remote_dir.trim_end_matches('/'),
            self.remote_name
        )
    }

    /// 改写到新的远端位置（★ T9：排队期间被改名时用）。
    fn retarget(&mut self, remote_path: &str) -> bool {
        let Some((dir, name)) = remote_path.rsplit_once('/') else {
            return false;
        };
        if self.remote_path() == remote_path {
            return false;
        }
        self.remote_dir = dir.to_string();
        self.remote_name = name.to_string();
        true
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
            // 崩溃恢复出来的作业没有身份（`uploads` 表没有这一列）。
            // 那就退回「按排队时的路径发」—— 与 T9 之前完全一样，不会更糟。
            file_id: ZERO_FILE_ID,
            // ★ T8：崩溃恢复的作业一个字节都没传出去，进度必须从 0 起。
            bytes_sent: 0,
            bytes_total: 0,
        }
    }

    /// ★ M15/T8：把这个作业的传输进度记成 `(已发, 总量)` 并返回它。
    ///
    /// ## 为什么要 `min` 夹一下
    /// `bytes_total` 是**发之前** `stat` 到的本地大小，而 `bytes_sent` 是
    /// T2 流式上传里**真正读走**的字节数。上传期间用户可能又在改同一个文件：
    /// 改小了 → `sent < total`（进度条走不到 100%，但作业马上就结束，会被清）；
    /// 改大了 → `sent > total`，不夹的话界面会显示「120%」。
    /// 两种都只是展示问题，绝不能让它反过来影响上传。
    fn note_sent(&mut self, sent: u64, total: u64) -> (u64, u64) {
        self.bytes_total = total;
        self.bytes_sent = sent.min(total);
        (self.bytes_sent, total)
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
///
/// 参数：远端路径 + **这次真正落到 NAS 上的签名**（size, mtime）。
///
/// ★ 为什么要签名（见 `qxync-daemon` 的 success hook）：上传落地后，远端已经是
/// 「我们的新版本」了。baseline 必须在这一刻就跟着推进 —— 否则下一轮对账会看到
/// 「本地 == 远端、但 baseline 还是旧签名」，`decide` 走进
/// `(local 未标脏, remote 变了)` 那一格，又因为本地签名 ≠ baseline 而判成
/// **Conflict**。用户视角就是「我连续改两次同一个文件，凭什么给我冲突副本」。
/// 之前这里只传路径，daemon 只能 `clear_dirty`，baseline 要等下一轮轮询
/// （默认 30s）才被 `AdoptBaseline` 补上 —— 那个窗口就是冲突的来源。
pub type SuccessHook = Arc<dyn Fn(&str, (u64, i64)) + Send + Sync>;

/// ★ M15/T8：上传进度上报回调（远端路径 + `Some((已发, 总量))`，`None` = 作业结束）。
///
/// ## 为什么用 `Option` 而不是两个函数
/// 「传输结束」本身是一条必须上报的信息：完成/失败之后节点上不能留着
/// 「停在 96%」的进度条（会被读成「还有一个作业在跑」）。参数带上 `None`
/// 就让「置进度」与「清进度」共用一个回调，不会出现只实现了一半的注入点。
///
/// 与下载侧的 `ProgressHook` 分开定义：那边传的是「本次收到的增量」（8 路并发
/// 要靠原子累加器合成累计值），这边传的是 T2 已经算好的**累计**字节数 ——
/// 数据源不同，混用两种语义只会写出「进度乱跳」的 bug。
pub type UploadProgressHook = Arc<dyn Fn(&str, Option<(u64, u64)>) + Send + Sync>;

/// ★ M15/T8：调用上传进度回调 —— **panic 绝不能打死上传 worker**。
///
/// 与 [`invoke_success_hook`] 同一套理由：回调跑在上传 worker 的 OS 线程里，
/// 它只能坏它自己。进度是纯展示，丢一次上报没有任何后果。
pub(crate) fn invoke_progress_hook(
    hook: Option<UploadProgressHook>,
    remote: &str,
    progress: Option<(u64, u64)>,
) {
    let Some(hook) = hook else {
        return;
    };
    let shown = match progress {
        Some((d, t)) => format!("{d}/{t}"),
        None => "结束".to_string(),
    };
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook(remote, progress))).is_err() {
        tracing::error!("上传进度回调 panic（已隔离，队列继续）: {remote} {shown}");
    }
}

/// 调用成功回调 —— **回调 panic 绝不能打死上传 worker**。
///
/// ★ M7 实测踩过：回调跑在**上传 worker 的 OS 线程**里（不是 tokio worker），
/// 里面误用 `tokio::spawn` 会立刻 panic（"must be called from the context of a Tokio
/// runtime"），worker 线程随之死掉 —— 表现是「上传队列永远卡住、drain 超时、
/// 整个 daemon 像挂了」（fuse-matrix 的 M2c 冲突段卡了 3 分钟）。
/// 这里把回调隔离起来：它只能坏它自己，队列必须继续跑。
pub(crate) fn invoke_success_hook(hook: Option<SuccessHook>, remote: &str, sig: (u64, i64)) {
    let Some(hook) = hook else {
        return;
    };
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook(remote, sig))).is_err() {
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
    /// ★ M15/T8：上传进度上报（`None` = 不上报）。FUSE 侧注入，写进节点的 `progress`。
    progress_hook: Mutex<Option<UploadProgressHook>>,
    /// ★ M15/T8：按远端路径的**在途**进度（`None` = 当前没有在传的作业）。
    ///
    /// 与节点的 `progress` 分开存：上传队列是**唯一**知道「我现在传的是哪个
    /// 作业、传了多少」的地方，而它在 FUSE 节点表之外。`file_states` 汇总时
    /// 两边取并集（节点侧管下载、这里管上传）。
    inflight: Mutex<std::collections::HashMap<String, (u64, u64)>>,
    /// ★ M15/T9：按 `file_id` 反查「这个东西现在叫什么」；`None` = 没注入。
    name_resolver: Mutex<Option<NameResolver>>,
}

/// ★ M15/T9：身份 → 当前远端路径的解析器（由 FUSE 侧注入，内部查 `nodes` 表）。
///
/// **多条命中 = 有歧义 → 必须返回 `None`**：宁可发到排队时的旧路径（用户看得见
/// 有个多余文件、可自行处理），也不能凭猜测把内容写到另一个文件的位置上。
pub type NameResolver = Arc<dyn Fn(&FileId) -> Option<String> + Send + Sync>;

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
            // ★ T8
            progress_hook: Mutex::new(None),
            inflight: Mutex::new(std::collections::HashMap::new()),
            name_resolver: Mutex::new(None),
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

    /// ★ M15/T8：注册「上传进度」回调（按远端路径 + 真正发出的字节数 + 总字节数）。
    pub fn set_progress_hook(&self, hook: UploadProgressHook) {
        *self.progress_hook.lock().unwrap() = Some(hook);
    }

    /// ★ M15/T8：某个远端路径当前的上传进度（`None` = 没有在传的作业）。
    ///
    /// `file_states` 汇总「传输中」时读它 —— 节点表里那条是下载侧写的，
    /// 上传侧只有队列知道。
    pub fn progress_of(&self, remote_path: &str) -> Option<(u64, u64)> {
        self.inflight.lock().unwrap().get(remote_path).copied()
    }

    /// ★ M15/T8：上传侧在途汇总 —— `(在传作业数, 已发字节, 总字节)`。
    ///
    /// 与 [`crate::FsHandle::download_transfers`] 同一个口径，daemon 把两边加起来
    /// 才是「现在一共有几个文件在传」。
    pub fn upload_transfers(&self) -> (usize, u64, u64) {
        let Ok(m) = self.inflight.lock() else {
            return (0, 0, 0);
        };
        let mut done = 0u64;
        let mut total = 0u64;
        for (d, t) in m.values() {
            done += d;
            total += t;
        }
        (m.len(), done, total)
    }

    /// ★ M15/T8：记一次上传进度并上报。
    ///
    /// 同时做两件事：更新 [`Self::inflight`]（供 `file_states` 读）与调 hook
    /// （让 FUSE 节点也看到）。**都不参与上传正确性** —— 拿不到锁就跳过。
    ///
    /// `None` = 作业结束，把在途记录撤掉并让节点清进度。
    pub(crate) fn note_progress(&self, remote: &str, progress: Option<(u64, u64)>) {
        match self.inflight.lock() {
            Ok(mut m) => match progress {
                Some(p) => {
                    m.insert(remote.to_string(), p);
                }
                None => {
                    m.remove(remote);
                }
            },
            // 锁中毒 = 别的线程 panic 过。进度是纯展示，放弃这一次上报。
            Err(_) => return,
        }
        invoke_progress_hook(
            self.progress_hook.lock().ok().and_then(|h| h.clone()),
            remote,
            progress,
        );
    }

    /// ★ M15/T9：注入「身份 → 当前远端路径」解析器。
    ///
    /// 注入之后，作业**真正发出去之前**会按 `file_id` 复查一次落地位置：
    /// 排队期间对方设备改了名，就落到新名字（不产生旧名字的幽灵文件）。
    /// 没注入 / 解析不出（歧义）→ 按排队时的路径发，与 T9 之前行为一致。
    pub fn set_name_resolver(&self, resolver: NameResolver) {
        *self.name_resolver.lock().unwrap() = Some(resolver);
    }

    /// ★ M15/T9：发之前把作业重定向到「身份现在所在的名字」。
    /// 返回是否真的改了（改了的话调用方要重新落库，主键变了）。
    fn retarget_by_identity(&self, job: &mut UploadJob) -> bool {
        if job.file_id == ZERO_FILE_ID {
            return false;
        }
        let resolver = self.name_resolver.lock().unwrap().clone();
        let Some(resolve) = resolver else {
            return false;
        };
        let Some(current) = resolve(&job.file_id) else {
            return false;
        };
        if !job.retarget(&current) {
            return false;
        }
        tracing::info!(
            "排队期间远端改名，落地到新名字: {}（内容源仍是同一个文件）",
            job.remote_path()
        );
        true
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

            // ★ M15/T9：**发出去之前**按身份复查落地位置。排队这几十秒里对方
            // 设备改了名的话，写到旧名字会在 NAS 上留一个内容是新的「幽灵文件」。
            // 主键（remote_path）变了 → 删旧行、落新行，保持「表 = 未完成作业」。
            let mut job = job;
            let job_pre_retarget_path = job.remote_path();
            if self.retarget_by_identity(&mut job) {
                self.persist_delete(&job_pre_retarget_path);
                self.persist_put(&job);
            }

            // ★ T8：发之前把总量记下来（**只用于展示**）。baseline 推进仍然用
            // T2 返回的「真正发出的字节数」，两者不是一回事：用户在上传期间改了
            // 同一个文件的话，`metadata` 读到的是下一版的大小。
            job.bytes_total = std::fs::metadata(&job.local).map(|m| m.len()).unwrap_or(0);
            if job.bytes_total > 0 {
                self.note_progress(&job.remote_path(), Some((0, job.bytes_total)));
            }
            match self.rt.block_on(self.upload_one(&job)) {
                Ok(n) => {
                    // ★ T8：用 T2 的返回值（真正发出的字节数）结进度，`min` 夹住
                    // 总量 → 越界（用户把文件改大了）也只会显示 100%。
                    let shown = job.note_sent(n, job.bytes_total);
                    self.persist_delete(&job.remote_path());
                    if job.ephemeral {
                        // 冲突副本的 stash 是一次性的：传完就删
                        let _ = std::fs::remove_file(&job.local);
                    }
                    // 签名用**刚发出去**的字节数，不是此刻磁盘上的大小：
                    // 两次保存挨得近时，作业取走之后用户可能又改了缓存文件，
                    // 这时 `metadata(local)` 读到的是「下一版」的大小。
                    // 把它当成这一版的 baseline，就会推进到一个远端并不拥有的签名上。
                    invoke_success_hook(
                        self.success_hook.lock().unwrap().clone(),
                        &job.remote_path(),
                        (n, job.mtime),
                    );
                    // ★ T8：作业结束 → 撤掉在途进度（不留「停在 100%」的假象）。
                    // 先报一次终值是为了让「刚好 100% → 结束」这个转折可被观测。
                    if job.bytes_total > 0 {
                        self.note_progress(&job.remote_path(), Some(shown));
                    }
                    self.note_progress(&job.remote_path(), None);
                    self.stats.done.fetch_add(1, Ordering::Relaxed);
                    self.stats.bytes.fetch_add(n, Ordering::Relaxed);
                    tracing::info!("已上传 {} ({} 字节)", job.remote_path(), n);
                }
                Err(e) => {
                    // ★ T8：失败/放弃之后没有在传的作业了 → 清进度。
                    // 「正在退避重试」由 `pending` 表达（作业已重新入队），
                    // 不在这里假装还在传。
                    self.note_progress(&job.remote_path(), None);
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
    ///
    /// ★ M15/T2：走 [`Client::upload_file`] **流式**上传，不再 `fs::read` 整个文件
    /// 进内存（传 4 GB 文件原来就是 4 GB 内存，并发几个直接 OOM）。现在内存是
    /// O(8 MB)，与文件大小无关。
    ///
    /// 返回**真正发出去的字节数**（success hook 要用它把 baseline 推到准确签名）。
    /// 注意它取自流式读取时的实际计数，**不是** `metadata` 预先读到的大小 ——
    /// 上传期间用户可能还在改同一个文件（这正是「用刚发出的字节数推进 baseline」
    /// 这条正确性关键要防的事，见 M15/T2 验收第 4 条）。
    async fn upload_one(&self, job: &UploadJob) -> Result<u64, CoreError> {
        let n = self
            .client
            .upload_file(&job.remote_dir, &job.local, &job.remote_name)
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
        Ok(n)
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
            Some(Arc::new(move |_, _| {
                c1.fetch_add(1, Ordering::Relaxed);
                panic!("hook 里的 bug（真实场景：tokio::spawn 在非 tokio 线程）");
            })),
            "/home/boom.txt",
            (10, 1000),
        );
        // 第一次 panic 被隔离 → 后续回调照常执行（worker 还活着）
        let c2 = called.clone();
        invoke_success_hook(
            Some(Arc::new(move |_, _| {
                c2.fetch_add(1, Ordering::Relaxed);
            })),
            "/home/ok.txt",
            (20, 2000),
        );
        assert_eq!(
            called.load(Ordering::Relaxed),
            2,
            "回调 panic 不能中断调用点"
        );
        // 没有回调也不能有事
        invoke_success_hook(None, "/home/none.txt", (0, 0));
    }

    /// ★ 回归：回调必须拿到**这一版真正发出去的字节数**。
    ///
    /// daemon 靠它把 baseline 推到准确签名；传错（哪怕只是取上传后磁盘上的
    /// 大小）就会让 baseline 停在一个远端并不拥有的签名上。
    #[test]
    fn success_hook_receives_the_uploaded_signature() {
        use std::sync::Mutex as M;
        let got: Arc<M<Vec<(String, u64, i64)>>> = Arc::new(M::new(Vec::new()));
        let g = got.clone();
        let hook: SuccessHook = Arc::new(move |remote: &str, (size, mtime): (u64, i64)| {
            g.lock().unwrap().push((remote.to_string(), size, mtime));
        });
        invoke_success_hook(Some(hook.clone()), "/home/a.txt", (90, 2000));
        invoke_success_hook(Some(hook.clone()), "/home/b.txt", (1234, 5678));
        invoke_success_hook(Some(hook), "/home/empty.txt", (0, 42));
        assert_eq!(
            *got.lock().unwrap(),
            vec![
                ("/home/a.txt".to_string(), 90, 2000),
                ("/home/b.txt".to_string(), 1234, 5678),
                ("/home/empty.txt".to_string(), 0, 42),
            ],
            "回调应逐次收到 (路径, 本次上传字节数, mtime)"
        );
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
            // ★ T9：老构造路径（无身份）等价于 ZERO —— 队列按原路径发，退化行为一致。
            file_id: ZERO_FILE_ID,
            // ★ T8：还没开始传
            bytes_sent: 0,
            bytes_total: 0,
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
            file_id: ZERO_FILE_ID,
            bytes_sent: 0,
            bytes_total: 0,
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

    // ------------------------------------------------------------ ★ T9 身份重定向

    /// ★ T9：排队期间远端改了名 → 作业**落地到新名字**，不产生旧名字的幽灵文件。
    ///
    /// 断言要点：
    /// 1. `retarget` 只改 `remote_dir`/`remote_name`，**本地内容源一字不动**（内容还是同一个文件）；
    /// 2. 解析不出（`None`）→ **不重定向**（保守退化：宁可传到旧名字让用户看见，
    ///    也不能凭猜测把内容写到别的文件位置上）；
    /// 3. 无身份（崩溃恢复的作业）→ 完全不参与重定向。
    #[test]
    fn t9_job_retargets_to_the_current_name_of_the_same_identity() {
        let mut j = job(Path::new("/tmp"), "old.txt");
        let id =
            qxync_core::file_id::compute_file_id(false, 100, 1_700_000_000, "old.txt", "/home");
        assert_ne!(id, ZERO_FILE_ID);
        j.file_id = id;

        // 1. 解析出改名后的新路径 → 切过去，内容源不变
        assert!(j.retarget("/home/qxync-test/new.txt"));
        assert_eq!(j.remote_path(), "/home/qxync-test/new.txt");
        assert_eq!(
            j.remote_dir, "/home/qxync-test",
            "目录按切分点写回，无尾斜杠"
        );
        assert_eq!(j.remote_name, "new.txt");
        assert_eq!(j.local, Path::new("/tmp").join("cache").join("old.txt"));
        assert_eq!(j.file_id, id, "重定向不改变身份");
        assert_eq!(j.mtime, 1234, "重定向不改变内容特征");

        // 2. 已经是当前名字 → 不动（幂等，不会把主键改来改去）
        assert!(!j.retarget("/home/qxync-test/new.txt"));

        // 3. `retarget` 本身只管切路径，**身份判定在 `retarget_by_identity` 里**（见下一个测试）
        let mut anonymous = job(Path::new("/tmp"), "anon.txt");
        assert_eq!(anonymous.file_id, ZERO_FILE_ID);
        assert!(anonymous.retarget("/home/qxync-test/whatever.txt"));
        assert_eq!(anonymous.remote_path(), "/home/qxync-test/whatever.txt");
    }

    /// ★ T9：`retarget_by_identity` 的四条退化路径 —— 身份为零 / 解析器缺席 /
    /// 解析不出（歧义）/ 已是当前名字，都必须**原样保留排队时的路径**。
    #[test]
    fn t9_retarget_by_identity_falls_back_when_it_cannot_resolve() {
        let dir = tmpdir("retarget");
        let marker = dir.join("queue");
        let (q, _rt) = test_queue(&marker);
        let id = qxync_core::file_id::compute_file_id(false, 100, 1_700_000_000, "a.txt", "/home");

        // 没注入解析器（daemon 还没接线 / 只读挂载）→ 不重定向
        let mut j1 = job(&dir, "a.txt");
        j1.file_id = id;
        assert!(!q.retarget_by_identity(&mut j1));
        assert_eq!(j1.remote_path(), "/home/qxync-test/a.txt");

        // 注入解析器但解析不出（歧义 / 该身份已不存在）→ 不重定向
        q.set_name_resolver(Arc::new(|_| None));
        assert!(!q.retarget_by_identity(&mut j1));
        assert_eq!(j1.remote_path(), "/home/qxync-test/a.txt");

        // 身份为零（崩溃恢复的作业）→ 解析器给什么结果都不动
        let mut anon = job(&dir, "anon.txt");
        assert_eq!(anon.file_id, ZERO_FILE_ID);
        assert!(!q.retarget_by_identity(&mut anon));
        assert_eq!(anon.remote_path(), "/home/qxync-test/anon.txt");

        // 解析器给出新名字 → 重定向
        q.set_name_resolver(Arc::new(|_| {
            Some("/home/qxync-test/改名了.txt".to_string())
        }));
        assert!(q.retarget_by_identity(&mut j1));
        assert_eq!(j1.remote_path(), "/home/qxync-test/改名了.txt");
        // 已经落到新名字后再解析一次 → 幂等，不再改主键
        assert!(!q.retarget_by_identity(&mut j1));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ★ T9：重定向之后**队列状态库的主键要跟着换**（旧路径那行删掉、新路径那行写入）。
    ///
    /// 不换的后果：库里仍留着旧路径 → 崩溃恢复会复活一个已经不存在的路径的作业，
    /// 也就是「幽灵文件」换个方向复发。
    #[test]
    fn t9_retarget_rekeys_the_persisted_row() {
        let dir = tmpdir("rekey");
        let marker = dir.join("queue");
        let (q, _rt) = test_queue(&marker);
        let id = qxync_core::file_id::compute_file_id(false, 7, 1_700_000_001, "x.bin", "/home");

        let mut j = job(&dir, "x.bin");
        j.file_id = id;
        q.enqueue(j.clone()).unwrap();
        assert!(Store::open(marker.join(QUEUE_DB_FILE))
            .unwrap()
            .uploads()
            .unwrap()
            .iter()
            .any(|r| r.remote_path() == "/home/qxync-test/x.bin"));

        // 模拟 worker：按身份查到新名字 → 改主键 → 旧行删掉、新行写入
        q.set_name_resolver(Arc::new(|_| Some("/home/qxync-test/y.bin".to_string())));
        let old_path = j.remote_path();
        let mut job = j.clone();
        assert!(q.retarget_by_identity(&mut job));
        q.persist_delete(&old_path);
        q.persist_put(&job);

        let rows = Store::open(marker.join(QUEUE_DB_FILE))
            .unwrap()
            .uploads()
            .unwrap();
        assert_eq!(rows.len(), 1, "旧路径的行必须删掉，不能留幽灵");
        assert_eq!(rows[0].remote_path(), "/home/qxync-test/y.bin");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ================================================================ ★ M15/T8

    /// ★ T8：上传进度**从 T2 返回的「真正发出的字节数」推进，且不越界**。
    #[test]
    fn t8_upload_progress_uses_sent_bytes_and_never_exceeds_total() {
        let dir = tmpdir("t8-sent");
        let mut j = job(&dir, "a.bin");
        // 正常：发出 400 / 总量 1000
        assert_eq!(j.note_sent(400, 1000), (400, 1000));
        assert_eq!(j.bytes_sent, 400);
        assert_eq!(j.bytes_total, 1000);
        // 上传期间用户把文件**改小**了 → 真正发出的比预读的小：如实报小值
        assert_eq!(j.note_sent(120, 1000), (120, 1000));
        // 上传期间用户把文件**改大**了 → 真正发出的超过预读：必须夹住，
        // 绝不显示 120%
        assert_eq!(j.note_sent(1200, 1000), (1000, 1000), "进度不能越界");
        assert_eq!(j.bytes_sent, 1000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ★ T8：崩溃恢复出来的作业进度必须从 0 起（表里没有这两列）。
    #[test]
    fn t8_progress_is_not_persisted_across_restart() {
        let dir = tmpdir("t8-nopersist");
        let mut j = job(&dir, "a.bin");
        j.note_sent(500, 1000);
        // 往返一次 `uploads` 表（= 崩溃恢复走的路）
        let row = j.to_row();
        let back = UploadJob::from_row(&row);
        assert_eq!(back.bytes_sent, 0, "库里没有进度列，恢复出来必须是 0");
        assert_eq!(back.bytes_total, 0, "总量在发之前才 stat，不该被持久化");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ★ T8：在途表 + 回调一起推进，`None` 表示作业结束。
    #[test]
    fn t8_inflight_tracks_progress_and_clears() {
        let dir = tmpdir("t8-inflight");
        let (q, _rt) = test_queue(&dir.join("queue"));
        let seen: Arc<std::sync::Mutex<Vec<(String, Option<(u64, u64)>)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let s = seen.clone();
        q.set_progress_hook(Arc::new(move |remote: &str, p: Option<(u64, u64)>| {
            s.lock().unwrap().push((remote.to_string(), p));
        }));

        let p = "/home/qxync-test/a.bin";
        assert_eq!(q.progress_of(p), None, "还没开始就没有在途进度");
        assert_eq!(q.upload_transfers().0, 0);

        q.note_progress(p, Some((0, 1000)));
        assert_eq!(q.progress_of(p), Some((0, 1000)));
        q.note_progress(p, Some((600, 1000)));
        assert_eq!(q.progress_of(p), Some((600, 1000)));
        assert_eq!(q.upload_transfers().0, 1);
        assert_eq!(q.upload_transfers(), (1, 600, 1000));

        // 作业结束 → 撤掉，不能停在 60%
        q.note_progress(p, None);
        assert_eq!(q.progress_of(p), None, "结束后不能留着在途进度");
        assert_eq!(q.upload_transfers().0, 0);
        assert_eq!(q.upload_transfers(), (0, 0, 0));

        let v = seen.lock().unwrap();
        assert_eq!(
            *v,
            vec![
                (p.to_string(), Some((0, 1000))),
                (p.to_string(), Some((600, 1000))),
                (p.to_string(), None),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ★ T8：**回调 panic 不能打死上传 worker**（M7 同款坑，上传侧再堵一次）。
    #[test]
    fn t8_panicking_progress_hook_is_isolated() {
        let called = Arc::new(AtomicU64::new(0));
        let c = called.clone();
        let hook: UploadProgressHook = Arc::new(move |_, _| {
            c.fetch_add(1, Ordering::Relaxed);
            panic!("进度回调里的 bug");
        });
        invoke_progress_hook(Some(hook.clone()), "/home/a.bin", Some((1, 10)));
        // 隔离之后后续回调照常执行
        let c2 = called.clone();
        invoke_progress_hook(
            Some(Arc::new(move |_, _| {
                c2.fetch_add(1, Ordering::Relaxed);
            })),
            "/home/a.bin",
            None,
        );
        assert_eq!(
            called.load(Ordering::Relaxed),
            2,
            "回调 panic 不能中断调用点"
        );
        // 没回调也不能有事
        invoke_progress_hook(None, "/home/a.bin", Some((1, 10)));
    }

    /// ★ T8：没注入回调时 `note_progress` 依然不能出错（`file_states` 还要读在途表）。
    #[test]
    fn t8_progress_without_hook_is_silent_and_harmless() {
        let dir = tmpdir("t8-nohook");
        let (q, _rt) = test_queue(&dir.join("queue"));
        let p = "/home/qxync-test/a.bin";
        q.note_progress(p, Some((10, 100)));
        assert_eq!(q.progress_of(p), Some((10, 100)));
        q.note_progress(p, None);
        assert_eq!(q.progress_of(p), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
