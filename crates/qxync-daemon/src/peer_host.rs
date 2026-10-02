//! ★ M7：daemon 侧的对等主机（LAN 直连 / 设备配对 / 事件快路径）。
//!
//! 协议实现在 `qxync_client::peer`；这里负责：
//!
//! * 启动对等 TCP 服务（`link.peer_listen` 配了才启动 —— **默认关闭**）；
//! * 内容源 = 所有挂载视图的句柄（只服务完整水合、未脏、未排除的文件）；
//! * 收到对端事件 → 唤醒轮询线程跑一轮（事件是快路径，对账是主路径，M2c 不变）；
//! * 配对结果 / 对端 `hello` 登记落盘到 `links/<id>.peers.json`（0600）；
//! * 上传成功后把事件广播给所有对端。
//!
//! 安全边界（文档 §2.5）：明文 TCP，token 只授权「读已水合文件 + 提交事件」，
//! 路径必须落在配置的 roots 之内，没有任何写/删远端的能力。

use qxync_client::peer::{
    self, ContentSource, PeerClient, PeerConfig, PeerEvent, PeerHead, PeerIdentity,
    PeerRegistration, PeerServer, PeerStats,
};
use qxync_core::ipc::{mask_token, PeerData, PeerDeviceInfo, PeerEventInfo};
use qxync_core::{ConfigPaths, LinkConfig, PeerRegistry};
use qxync_fuse::FsHandle;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Notify};

use crate::daemon::State;

/// 最近保留多少条对端事件（`qxync peer events`）。
const EVENT_LOG: usize = 200;
/// 事件触发的额外对账的最小间隔（防抖；对端连推多个文件时不要每来一条就跑一轮）。
pub const WAKE_DEBOUNCE: Duration = Duration::from_secs(1);

/// 内容源：把「所有挂载视图」当成本机能提供的文件集合。
pub struct DaemonContent {
    state: Weak<State>,
}

impl DaemonContent {
    /// 收集当前所有挂载句柄（先收集、后调用，避免长时间持有挂载表锁）。
    fn handles(&self) -> Vec<FsHandle> {
        let Some(st) = self.state.upgrade() else {
            return Vec::new();
        };
        let g = st.mounts.lock().unwrap();
        g.values().map(|m| m.handle.clone()).collect()
    }
}

impl ContentSource for DaemonContent {
    fn head(&self, path: &str) -> Option<PeerHead> {
        for h in self.handles() {
            if let Some(hd) = h.head(path) {
                return Some(hd);
            }
        }
        None
    }

    fn read_at(&self, path: &str, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        let handles = self.handles();
        for h in &handles {
            // head 已经把「完整水合 / 未脏 / 未排除」都判过了，这里必须再过一遍：
            // 不能只凭 handle 存在就 open 一个稀疏缓存。
            if h.head(path).is_some() {
                return h.read_at(path, offset, len);
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "本机没有这个文件（未挂载 / 未完整水合）",
        ))
    }

    fn roots(&self) -> Vec<String> {
        self.state
            .upgrade()
            .map(|st| st.link.roots())
            .unwrap_or_default()
    }
}

/// 对等主机（daemon 持有的单例）。
pub struct PeerHost {
    pub link_id: String,
    pub paths: ConfigPaths,
    /// 本机身份名。
    pub name: String,
    /// 实际监听地址（没开监听 = None）。
    pub listen: Option<String>,
    /// 已配对设备（与 FUSE 共享：水合时先试 LAN）。
    pub peers: Arc<StdMutex<Vec<PeerConfig>>>,
    /// 配置的全部远端根（对外声明 + 配对信息）。
    pub roots: Vec<String>,
    server: Option<Arc<PeerServer>>,
    /// 收到的事件（环形，最新在前）；接收任务与 host 共用同一份。
    events_in: Arc<StdMutex<VecDeque<PeerEventInfo>>>,
    /// 发出的事件条数（尽力而为）。
    events_out: Arc<AtomicU64>,
    /// 待广播的事件队列。
    ///
    /// ★ 关键：上传成功回调跑在 **上传 worker 的 OS 线程**里（不是 tokio worker），
    /// 那里 `tokio::spawn` 会直接 panic（"must be called from the context of a Tokio runtime"），
    /// 把上传 worker 打死 —— 表现是「上传队列永远卡住、drain 超时、整个 daemon 像挂了」。
    /// 实测踩过：fuse-matrix 的 M2c 冲突段卡了 3 分钟。所以发送端只做**同步 send**，
    /// 真正的广播在这个常驻 task 里做。
    outbox: mpsc::UnboundedSender<PeerEvent>,
    /// 最近一次操作的提示（诊断用）。
    last_note: StdMutex<Option<String>>,
}

impl PeerHost {
    /// 读取登记的设备并（在配置了 `peer_listen` 时）启动服务。
    pub async fn start(
        paths: ConfigPaths,
        link: &LinkConfig,
        state: &Arc<State>,
        peers: Arc<StdMutex<Vec<PeerConfig>>>,
        wake: Arc<Notify>,
    ) -> std::io::Result<Arc<Self>> {
        let mut registry = PeerRegistry::load(&paths, &link.id).unwrap_or_default();
        registry.peers.retain(|p| !p.token.is_empty());
        if let Ok(mut g) = peers.lock() {
            *g = registry.peers.clone();
        }
        let name = link
            .peer_name
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(crate::sync::hostname);
        let roots = link.roots();
        let events_in: Arc<StdMutex<VecDeque<PeerEventInfo>>> =
            Arc::new(StdMutex::new(VecDeque::new()));

        let bind = link
            .peer_listen
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        // 事件出口：发送端（可能是 FUSE/上传线程）只 send，广播在常驻 task 里做
        let (outbox, mut out_rx) = mpsc::unbounded_channel::<PeerEvent>();
        let events_out = Arc::new(AtomicU64::new(0));
        {
            let peers = peers.clone();
            let out_count = events_out.clone();
            tokio::spawn(async move {
                while let Some(ev) = out_rx.recv().await {
                    let list = peers.lock().map(|g| g.clone()).unwrap_or_default();
                    if list.is_empty() {
                        continue;
                    }
                    let n = peer::broadcast_event(&list, ev).await;
                    if n > 0 {
                        out_count.fetch_add(n as u64, Ordering::Relaxed);
                    }
                }
            });
        }

        let Some(bind) = bind else {
            tracing::info!("LAN 对等服务未开启（link.peer_listen 为空）");
            return Ok(Arc::new(Self {
                link_id: link.id.clone(),
                paths,
                name,
                listen: None,
                peers,
                roots,
                server: None,
                events_in,
                events_out,
                outbox,
                last_note: StdMutex::new(None),
            }));
        };

        let listener = tokio::net::TcpListener::bind(&bind).await?;
        let addr = listener.local_addr()?.to_string();
        let source: Arc<dyn ContentSource> = Arc::new(DaemonContent {
            state: Arc::downgrade(state),
        });
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<PeerEvent>();
        let (reg_tx, mut reg_rx) = mpsc::unbounded_channel::<PeerRegistration>();
        let srv = Arc::new(PeerServer::new(
            name.clone(),
            env!("CARGO_PKG_VERSION"),
            Some(addr.clone()),
            roots.clone(),
            source,
            ev_tx,
            reg_tx,
        ));
        // 重启后恢复入站鉴权：登记的 token 是「这一对设备共用的秘密」。
        for p in &registry.peers {
            srv.add_token(p.name.clone(), p.token.clone());
        }

        // ① 入站事件 → 环形日志 + 唤醒轮询（事件是快路径，对账是主路径）
        {
            let log = events_in.clone();
            let wake = wake.clone();
            tokio::spawn(async move {
                while let Some(ev) = ev_rx.recv().await {
                    tracing::info!("收到对端事件 {}（{} 字节，{}）", ev.path, ev.size, ev.kind);
                    if let Ok(mut g) = log.lock() {
                        g.push_front(PeerEventInfo {
                            path: ev.path.clone(),
                            size: ev.size,
                            mtime: ev.mtime,
                            kind: ev.kind.clone(),
                            ts: ev.ts,
                            from: None,
                        });
                        g.truncate(EVENT_LOG);
                    }
                    wake.notify_one();
                }
            });
        }

        // ② 对端 `hello` 登记 → 落盘 + 更新 live peers（双向信任）
        {
            let paths = paths.clone();
            let link_id = link.id.clone();
            let peers = peers.clone();
            tokio::spawn(async move {
                while let Some(reg) = reg_rx.recv().await {
                    tracing::info!("对端 {} 登记（{}）", reg.name, reg.addr);
                    let mut now = PeerRegistry::load(&paths, &link_id).unwrap_or_default();
                    now.upsert(PeerConfig {
                        name: reg.name.clone(),
                        addr: reg.addr.clone(),
                        token: reg.token.clone(),
                    });
                    if let Err(e) = now.save(&paths, &link_id) {
                        tracing::warn!("保存对等设备失败: {e}");
                    }
                    if let Ok(mut g) = peers.lock() {
                        g.retain(|p| p.name != reg.name);
                        g.push(PeerConfig {
                            name: reg.name,
                            addr: reg.addr,
                            token: reg.token,
                        });
                    }
                }
            });
        }

        tokio::spawn({
            let s = srv.clone();
            async move { peer::serve(listener, s).await }
        });
        tracing::info!("LAN 对等服务已启动: {addr}（身份 {name}）");

        Ok(Arc::new(Self {
            link_id: link.id.clone(),
            paths,
            name,
            listen: Some(addr),
            peers,
            roots,
            server: Some(srv),
            events_in,
            events_out,
            outbox,
            last_note: StdMutex::new(None),
        }))
    }

    /// 已配对的设备快照。
    pub fn peer_list(&self) -> Vec<PeerConfig> {
        self.peers.lock().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn stats(&self) -> Option<Arc<PeerStats>> {
        self.server.as_ref().map(|s| s.stats.clone())
    }

    /// 当前配对码（`qxync peer status` 显示，配对成功后自动轮换）。
    pub fn pairing_code(&self) -> Option<String> {
        self.server.as_ref().and_then(|s| s.pairing_code())
    }

    /// 最近收到的对端事件（最新在前）。
    pub fn events(&self, limit: usize) -> Vec<PeerEventInfo> {
        self.events_in
            .lock()
            .map(|g| g.iter().take(limit).cloned().collect())
            .unwrap_or_default()
    }

    /// `peer status` 数据。
    pub fn status(&self) -> PeerData {
        let devices = self
            .peer_list()
            .into_iter()
            .map(|p| PeerDeviceInfo {
                name: p.name,
                addr: p.addr,
                token_masked: mask_token(&p.token),
            })
            .collect();
        let stats = self.stats();
        PeerData {
            action: "status".into(),
            enabled: self.listen.is_some(),
            listen: self.listen.clone(),
            identity: self.name.clone(),
            peer_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            roots: self.roots.clone(),
            pairing_code: self.pairing_code(),
            devices,
            events_out: self.events_out.load(Ordering::Relaxed),
            events_in: stats
                .as_ref()
                .map(|s| s.events.load(Ordering::Relaxed))
                .unwrap_or(0),
            rejected: stats
                .as_ref()
                .map(|s| s.rejected.load(Ordering::Relaxed))
                .unwrap_or(0),
            note: self.last_note.lock().unwrap().clone().or_else(|| {
                if self.listen.is_none() {
                    Some("未开启 LAN 监听（link.peer_listen 未配置）".to_string())
                } else if self.peer_list().is_empty() {
                    Some("还没有配对的设备：qxync peer pair <addr> --code <配对码>".to_string())
                } else {
                    None
                }
            }),
            ..Default::default()
        }
    }

    /// 按名字或地址找设备。
    fn find(&self, name_or_addr: &str) -> Option<PeerConfig> {
        self.peer_list()
            .into_iter()
            .find(|p| p.name == name_or_addr || p.addr == name_or_addr)
    }

    /// `qxync peer pair <addr> --code <code>`：一次配对建立**双向**信任。
    ///
    /// 1. 向对方换 token（对方把它记成「我们可用」）；
    /// 2. 我们把 token 记进自己的入站表（我们接受对方用它调我们）；
    /// 3. 用 `hello` 把「我是谁 / 怎么连我 / 用哪个 token」告诉对方 →
    ///    对方也能主动连我们（事件快路径需要这一半）。
    pub async fn pair(&self, addr: &str, code: &str) -> Result<PeerData, String> {
        let (token, ident) = PeerClient::pair(addr, code, &self.name, self.roots.clone())
            .await
            .map_err(|e| format!("配对失败: {e}"))?;
        let peer_addr = ident.addr.clone().unwrap_or_else(|| addr.to_string());
        if let Some(s) = &self.server {
            s.add_token(ident.name.clone(), token.clone());
        }
        let pc = PeerConfig {
            name: ident.name.clone(),
            addr: peer_addr.clone(),
            token: token.clone(),
        };
        let mut reg = PeerRegistry::load(&self.paths, &self.link_id).unwrap_or_default();
        reg.upsert(pc.clone());
        reg.save(&self.paths, &self.link_id)
            .map_err(|e| format!("保存对等设备失败: {e}"))?;
        if let Ok(mut g) = self.peers.lock() {
            g.retain(|p| p.name != pc.name);
            g.push(pc.clone());
        }
        let mut note = None;
        if let Some(my_addr) = &self.listen {
            if let Err(e) =
                PeerClient::hello(&peer_addr, &token, &self.name, my_addr, self.roots.clone()).await
            {
                note = Some(format!(
                    "已配对（单向）：对方登记失败（{e}）；对方要主动推事件需要它能连到 {my_addr}"
                ));
            }
        } else {
            note = Some("已配对（单向）：本机未开 LAN 监听，对方无法主动连我们".to_string());
        }
        tracing::info!(
            "已配对设备 {}（{}），token={}",
            pc.name,
            pc.addr,
            mask_token(&pc.token)
        );
        *self.last_note.lock().unwrap() = note.clone();
        Ok(PeerData {
            action: "pair".into(),
            paired_name: Some(pc.name),
            paired_addr: Some(pc.addr),
            paired_token_masked: Some(mask_token(&pc.token)),
            note,
            ..self.status()
        })
    }

    /// 探活（地址或已配对设备名）。
    pub async fn ping(&self, target: &str) -> Result<PeerData, String> {
        let addr = self
            .find(target)
            .map(|p| p.addr)
            .unwrap_or_else(|| target.to_string());
        let started = Instant::now();
        let ident: PeerIdentity = PeerClient::ping(&addr)
            .await
            .map_err(|e| format!("ping {addr} 失败: {e}"))?;
        Ok(PeerData {
            action: "ping".into(),
            peer_name: Some(ident.name),
            peer_version: Some(ident.version),
            peer_roots: ident.roots,
            pairing_open: Some(ident.pairing_open),
            took_ms: Some(started.elapsed().as_millis() as u64),
            ..self.status()
        })
    }

    /// 从对端直传一份内容到本地文件（LAN 直连自检：不经过 NAS）。
    pub async fn fetch(
        &self,
        target: &str,
        path: &str,
        dest: &PathBuf,
    ) -> Result<PeerData, String> {
        let pc = self
            .find(target)
            .ok_or_else(|| format!("没有这台已配对设备: {target}"))?;
        let client = PeerClient::from(&pc);
        let head = client
            .head(path)
            .await
            .map_err(|e| format!("对端 head 失败: {e}"))?;
        if !head.exists || !head.hydrated {
            return Err(format!("对端没有可服务的 {path}（不存在或未完整水合）"));
        }
        let started = Instant::now();
        let mut data = Vec::with_capacity(head.size.min(256 * 1024 * 1024) as usize);
        let mut offset = 0u64;
        while offset < head.size {
            let want = (head.size - offset).min(peer::MAX_GET_LEN);
            let (chunk, _) = client
                .get_range(path, offset, want)
                .await
                .map_err(|e| format!("对端 get 失败 @{offset}: {e}"))?;
            if chunk.len() as u64 != want {
                return Err(format!(
                    "对端短读 @{offset}: 期望 {want} 实得 {}",
                    chunk.len()
                ));
            }
            data.extend_from_slice(&chunk);
            offset += want;
        }
        if data.len() as u64 != head.size {
            return Err(format!(
                "长度不符：head {} 字节，实得 {}",
                head.size,
                data.len()
            ));
        }
        std::fs::write(dest, &data).map_err(|e| format!("写 {}: {e}", dest.display()))?;
        Ok(PeerData {
            action: "fetch".into(),
            fetch_bytes: Some(data.len() as u64),
            fetch_from: Some(pc.name),
            fetch_dest: Some(dest.clone()),
            took_ms: Some(started.elapsed().as_millis() as u64),
            ..self.status()
        })
    }

    /// 广播一个事件给所有对端；返回送达台数。
    pub async fn notify(&self, ev: PeerEvent) -> usize {
        let peers = self.peer_list();
        if peers.is_empty() {
            return 0;
        }
        let n = peer::broadcast_event(&peers, ev).await;
        if n > 0 {
            self.events_out.fetch_add(n as u64, Ordering::Relaxed);
        }
        n
    }

    /// 非阻塞广播（上传成功回调里用：FUSE 的写路径不能被网络拖住）。
    pub fn notify_async(&self, path: String, size: u64, mtime: i64, kind: &str) {
        let ev = PeerEvent::new(path, size, mtime, kind);
        if self.outbox.send(ev).is_err() {
            tracing::warn!("事件队列已关闭，LAN 广播被丢弃（不影响 NAS 同步）");
        }
    }
}

impl std::fmt::Debug for PeerHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerHost")
            .field("name", &self.name)
            .field("listen", &self.listen)
            .field("peers", &self.peer_list().len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ M7 回归：`notify_async` 会被**上传 worker 的 OS 线程**（没有 tokio 上下文）调用。
    /// 曾经它内部用 `tokio::spawn` → panic → 打死上传 worker（队列永久卡住）。
    /// 这里从裸线程调一次：panic 会被 `join().unwrap()` 抓住。
    #[test]
    fn notify_async_is_safe_off_runtime() {
        let (outbox, mut rx) = mpsc::unbounded_channel::<PeerEvent>();
        let host = Arc::new(PeerHost {
            link_id: "unit-test".into(),
            paths: ConfigPaths::discover().unwrap(),
            name: "unit-test".into(),
            listen: None,
            peers: Arc::new(StdMutex::new(Vec::new())),
            roots: vec!["/home".into()],
            server: None,
            events_in: Arc::new(StdMutex::new(VecDeque::new())),
            events_out: Arc::new(AtomicU64::new(0)),
            outbox,
            last_note: StdMutex::new(None),
        });
        let h = host.clone();
        std::thread::spawn(move || {
            // 没有 runtime 也必须能调：只做同步 send
            h.notify_async("/home/qxync-test/x.bin".into(), 3, 4, "modified");
        })
        .join()
        .expect("notify_async 在非 tokio 线程里 panic 了（历史上就是这么把上传 worker 打死的）");
        let ev = rx.try_recv().expect("事件应该已经进队列");
        assert_eq!(ev.path, "/home/qxync-test/x.bin");
        assert_eq!((ev.size, ev.mtime), (3, 4));
    }
}
