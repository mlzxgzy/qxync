//! # M20：连通性状态 —— 「现在到底连不连得上 NAS」
//!
//! 离线缓冲区的所有判定都收敛到这一个结构，不要在各处各判一套：
//!
//! * 上传 / 删除 worker 看它决定「现在发不发」—— 离线时不消耗重试次数；
//! * FUSE 写路径看它决定「直接打 NAS 还是先落 outbox」；
//! * 轮询器看它决定「正常轮询还是短探活」—— 离线时每几秒探一次，恢复得快；
//! * `status` / GUI / CLI 看它显示「离线（自 …）N 项待同步」。
//!
//! ## 三种状态，而不是布尔值
//!
//! `Unknown`（刚启动，一次网络都还没试过）与 `Online` 在行为上一样（先试一次），
//! 但**展示上不一样**：刚启动就显示「离线」是假警报，显示「正常」又不诚实。
//! 所以快照里同时给 `online` 与 `known`，界面只在 `known && !online` 时才报离线。
//!
//! ## 判据只有一条：`Error::Transport`
//!
//! 离线 = **网络层**失败（连接失败 / 超时 / TLS / HTTP 传输中断）。业务状态码
//! （不存在、无权限、会话失效、服务端忙）都**不是**离线 —— 它们重试一万次也不会
//! 因为「网又通了」而成功，必须继续走原来的重试/放弃逻辑。
//!
//! ## 为什么用 `Arc` 而不是全局静态
//!
//! 多 NAS（M16）时每个连接一个 daemon 进程，本来互不相干；但单测里多个用例并行，
//! 全局静态会互相污染。做成显式注入的 `Arc<Connectivity>`：谁需要谁持有，
//! 没注入（老测试、只读 FUSE）时行为与 M20 之前完全一致 —— 永远「在线」。

use std::sync::atomic::{AtomicI64, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use crate::error::Error;

/// 一次网络都没试过（刚启动）。
const STATE_UNKNOWN: u8 = 0;
/// 已知在线。
const STATE_ONLINE: u8 = 1;
/// 已知离线。
const STATE_OFFLINE: u8 = 2;

/// 连通性快照（`status` IPC 直接用这个形状）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ConnectivitySnapshot {
    /// 现在能不能连上 NAS。`Unknown` 时是 `true`（乐观：先试一次再说）。
    pub online: bool,
    /// 是否已经有过一次明确判定（成功或失败）。`false` = 刚启动/从没联网过。
    pub known: bool,
    /// 当前状态开始时刻（unix 秒；`Unknown` 时为 0）。
    ///
    /// 界面用 `now - since_unix` 显示「已离线 3 分钟」「已恢复 10 秒」。
    pub since_unix: i64,
    /// 最近一次网络错误原文（恢复后**不清**，留给诊断 —— `since_unix` 表达「现在」）。
    pub last_error: Option<String>,
    /// 累计「在线/未知 → 离线」次数（自 daemon 启动）。
    pub offline_events: u64,
    /// 累计「离线 → 在线」次数（自 daemon 启动）。
    pub online_events: u64,
}

impl Default for ConnectivitySnapshot {
    fn default() -> Self {
        Self {
            online: true,
            known: false,
            since_unix: 0,
            last_error: None,
            offline_events: 0,
            online_events: 0,
        }
    }
}

/// 进程内唯一的连通性状态（每个 NAS 连接一份，由 daemon 注入各组件）。
#[derive(Debug)]
pub struct Connectivity {
    state: AtomicU8,
    since_unix: AtomicI64,
    last_error: Mutex<Option<String>>,
    offline_events: AtomicU64,
    online_events: AtomicU64,
}

impl Default for Connectivity {
    fn default() -> Self {
        Self::new()
    }
}

impl Connectivity {
    pub fn new() -> Self {
        Self {
            state: AtomicU8::new(STATE_UNKNOWN),
            since_unix: AtomicI64::new(0),
            last_error: Mutex::new(None),
            offline_events: AtomicU64::new(0),
            online_events: AtomicU64::new(0),
        }
    }

    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// 现在能不能发网络请求。
    ///
    /// **`Unknown` 返回 `true`**：刚启动时不能凭「没试过」就认定离线 —— 那会让第一次
    /// 上传/轮询永远不发，形成死锁（探活只在离线分支跑）。先乐观地试一次，
    /// 失败自然会置离线。
    pub fn is_online(&self) -> bool {
        self.state.load(Ordering::Relaxed) != STATE_OFFLINE
    }

    /// 是否已经有过明确判定。
    pub fn is_known(&self) -> bool {
        self.state.load(Ordering::Relaxed) != STATE_UNKNOWN
    }

    /// 标记「网络是通的」。返回是否发生了 离线 → 在线 的切换。
    ///
    /// 切换是有意义的事件：调用方据此唤醒上传/删除 worker、触发一次对账。
    pub fn mark_online(&self) -> bool {
        let prev = self.state.swap(STATE_ONLINE, Ordering::Relaxed);
        if prev == STATE_ONLINE {
            return false;
        }
        self.since_unix
            .store(crate::store::now_unix(), Ordering::Relaxed);
        if prev == STATE_OFFLINE {
            self.online_events.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// 标记「网络断了」。`reason` 会原样落到快照里给人看。
    ///
    /// 已经是离线状态时只更新原因，**不重复计数、不重置 since**（否则「已离线 3 分钟」
    /// 会被每一次失败刷新成 0 秒，用户永远看到「刚刚」）。
    pub fn mark_offline(&self, reason: impl Into<String>) -> bool {
        let reason = reason.into();
        if let Ok(mut e) = self.last_error.lock() {
            *e = Some(reason.clone());
        }
        let prev = self.state.swap(STATE_OFFLINE, Ordering::Relaxed);
        if prev == STATE_OFFLINE {
            return false;
        }
        self.since_unix
            .store(crate::store::now_unix(), Ordering::Relaxed);
        self.offline_events.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// 按错误类型判定：只有 [`Error::Transport`] 才置离线，返回是否发生切换。
    ///
    /// 业务 status（4/5/8/…）不是离线 —— 见模块头「判据只有一条」。
    pub fn mark_error(&self, e: &Error) -> bool {
        if e.is_offline() {
            self.mark_offline(e.to_string())
        } else {
            false
        }
    }

    /// 最近一次网络错误原文（恢复后保留，诊断用）。
    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|e| e.clone())
    }

    pub fn snapshot(&self) -> ConnectivitySnapshot {
        let state = self.state.load(Ordering::Relaxed);
        ConnectivitySnapshot {
            online: state != STATE_OFFLINE,
            known: state != STATE_UNKNOWN,
            since_unix: self.since_unix.load(Ordering::Relaxed),
            last_error: self.last_error(),
            offline_events: self.offline_events.load(Ordering::Relaxed),
            online_events: self.online_events.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_is_optimistic_and_reported_unresolved() {
        let c = Connectivity::new();
        assert!(
            c.is_online(),
            "没试过之前必须乐观（否则第一次请求永远不发）"
        );
        assert!(!c.is_known());
        let s = c.snapshot();
        assert!(s.online && !s.known);
        assert_eq!(s.since_unix, 0);
        assert_eq!(s.offline_events, 0);
    }

    #[test]
    fn transitions_count_once_and_keep_since() {
        let c = Connectivity::new();
        assert!(c.mark_offline("连接超时"), "未知 → 离线算一次切换");
        assert!(!c.mark_offline("还是超时"), "已离线时再失败不重复计数");
        assert!(!c.is_online());
        let s = c.snapshot();
        assert!(s.known && !s.online);
        assert_eq!(s.offline_events, 1);
        assert_eq!(
            s.last_error.as_deref(),
            Some("还是超时"),
            "最近一次原因要更新，但 since 不能重置"
        );

        assert!(c.mark_online(), "离线 → 在线算一次切换");
        assert!(!c.mark_online(), "已在线时重复标记不算切换");
        assert!(c.is_online() && c.is_known());
        assert_eq!(c.snapshot().online_events, 1);
    }

    #[test]
    fn only_transport_errors_flip_offline() {
        let c = Connectivity::new();
        // 业务错误：不是离线，不许改状态（否则会无限重试一条注定失败的作业）
        assert!(!c.mark_error(&Error::status(5, "stat")));
        assert!(!c.mark_error(&Error::Auth("口令不对".into())));
        assert!(!c.mark_error(&Error::Parse("坏 JSON".into())));
        assert!(c.is_online());

        // 网络错误：置离线
        assert!(c.mark_error(&Error::Transport("连接被拒绝".into())));
        assert!(!c.is_online());
        assert!(c.last_error().unwrap_or_default().contains("连接被拒绝"));
    }

    /// 快照必须是纯读：反复取快照不能改变状态/计数（`status` 每秒都可能被调）。
    #[test]
    fn snapshot_is_read_only() {
        let c = Connectivity::new();
        c.mark_offline("x");
        let a = c.snapshot();
        let b = c.snapshot();
        assert_eq!(a, b);
    }
}
