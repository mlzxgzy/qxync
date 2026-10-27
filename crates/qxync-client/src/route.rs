//! ★ M40：多路径路由表。
//!
//! 一条 NAS 连接可以有多条访问路径（家里 LAN、公网 DDNS、备用域名…）。本模块只管
//! **选哪条、什么时候换、失败怎么记**，不碰 HTTP —— 发请求在 [`crate::Client`]，
//! 探测流程也在那里（它需要 `reqwest::Client`）。
//!
//! ## 规则（刻意简单、可解释）
//!
//! * 顺序 = 配置顺序（`LinkConfig::endpoint_list`：额外路径在前、主地址兜底）；
//! * 一次失败只记录当前路径并**换到下一条**（并发失败不会一次跳两条）；
//! * 失败的路径进入冷却：`30s × 2^(连续失败-1)`，上限 5 分钟；成功清零；
//! * 正在用的路径恢复不了时，退而选「冷却最早到期」的那条，不会把自己锁死；
//! * 更靠前的路径**连续 2 次探测成功**才切回（避免 LAN/WAN 抖动来回切）；
//! * 被身份守卫拦下的路径（指向另一台 NAS）标记 `blocked`，永不自动选用。
//!
//! ## 锁
//!
//! 全部状态在一把 `std::sync::RwLock` 后面，**持锁期间绝不 `await`**：探测在锁外
//! 并发跑，拿到结果后再一次性写回。FUSE 的多个线程会同时 `record_failure`，所以
//! 「只有当前 active 失败才换路」这个判断必须在锁内完成。

use std::net::IpAddr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use qxync_core::ipc::{RouteInfo, RouteSummary, RoutesData, SwitchInfo};
use qxync_core::{ip_scope, Endpoint, LinkConfig};

/// 首次失败后的冷却时间。
const COOLDOWN_BASE: Duration = Duration::from_secs(30);
/// 冷却上限（NAS 关机一整夜也不该每 3 秒去敲同一条死路）。
const COOLDOWN_MAX: Duration = Duration::from_secs(300);
/// 切回更靠前路径所需的连续探测成功次数。
const PREFERRED_OK_STREAK: u32 = 2;

/// 一条路径的最近健康状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteState {
    /// 还没试过。
    Unknown,
    /// 最近一次请求/探测成功。
    Ok,
    /// 最近一次请求/探测失败（冷却中或可立即再试）。
    Failed,
    /// 身份守卫拦下（指向另一台 NAS 等）—— 不再使用。
    Blocked,
}

#[derive(Debug, Clone)]
struct Health {
    state: RouteState,
    last_ok_unix: Option<i64>,
    last_fail_unix: Option<i64>,
    last_error: Option<String>,
    latency_ms: Option<u64>,
    fail_streak: u32,
    ok_streak: u32,
    cooldown_until: Option<Instant>,
    blocked_reason: Option<String>,
    resolved: Vec<IpAddr>,
    resolved_at_unix: Option<i64>,
    resolved_changed: bool,
}

impl Default for Health {
    fn default() -> Self {
        Self {
            state: RouteState::Unknown,
            last_ok_unix: None,
            last_fail_unix: None,
            last_error: None,
            latency_ms: None,
            fail_streak: 0,
            ok_streak: 0,
            cooldown_until: None,
            blocked_reason: None,
            resolved: Vec::new(),
            resolved_at_unix: None,
            resolved_changed: false,
        }
    }
}

/// 一次路径切换的记录（诊断页 / 日志用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchRecord {
    pub at_unix: i64,
    pub from: String,
    pub to: String,
    /// `failure` / `probe_recovered` / `preferred_available` / `manual`。
    pub reason: String,
}

impl SwitchRecord {
    fn to_info(&self) -> SwitchInfo {
        SwitchInfo {
            at_unix: self.at_unix,
            from: self.from.clone(),
            to: self.to.clone(),
            reason: self.reason.clone(),
        }
    }
}

struct Inner {
    endpoints: Vec<Endpoint>,
    active: usize,
    health: Vec<Health>,
    switches: u64,
    last_switch: Option<SwitchRecord>,
    /// 第一次启动时是否已经做过一次全路径探测（决定要不要在登录前先探）。
    initial_probed: bool,
}

/// 进程内共享的路由表。每个 `Client` 持有同一个 `Arc<Router>`。
pub struct Router {
    inner: RwLock<Inner>,
}

impl std::fmt::Debug for Router {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        f.debug_struct("Router")
            .field("endpoints", &inner.endpoints.len())
            .field("active", &inner.active)
            .field("base_url", &inner.endpoints[inner.active].base_url())
            .finish()
    }
}

impl Router {
    pub fn new(link: &LinkConfig) -> Self {
        let endpoints = link.endpoint_list();
        let n = endpoints.len().max(1);
        Self {
            inner: RwLock::new(Inner {
                endpoints,
                active: 0,
                health: vec![Health::default(); n],
                switches: 0,
                last_switch: None,
                initial_probed: false,
            }),
        }
    }

    pub fn shared(link: &LinkConfig) -> Arc<Self> {
        Arc::new(Self::new(link))
    }

    pub fn endpoints(&self) -> Vec<Endpoint> {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .endpoints
            .clone()
    }

    pub fn endpoint_count(&self) -> usize {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .endpoints
            .len()
    }

    pub fn active_index(&self) -> usize {
        self.inner.read().unwrap_or_else(|e| e.into_inner()).active
    }

    pub fn active(&self) -> Endpoint {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        inner.endpoints[inner.active].clone()
    }

    /// 当前路径的根 URL。所有请求都从这里拼，**不再直接看 `link.host`**。
    pub fn base_url(&self) -> String {
        self.active().base_url()
    }

    /// 启动后（或第一次登录前）要不要先做一次全路径探测。
    ///
    /// 只有一条路径时返回 `false`：旧版行为不需要任何额外探测。
    pub fn needs_initial_probe(&self) -> bool {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        !inner.initial_probed && inner.endpoints.len() > 1
    }

    /// 一次成功（任意请求或探测）。
    pub fn record_success(&self, idx: usize, latency: Option<Duration>) {
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let Some(h) = inner.health.get_mut(idx) else {
            return;
        };
        if h.state == RouteState::Blocked {
            return; // 身份不符的路径，成功也不启用
        }
        h.state = RouteState::Ok;
        h.last_ok_unix = Some(now_unix());
        h.last_error = None;
        h.latency_ms = latency.map(|d| d.as_millis() as u64);
        h.fail_streak = 0;
        h.ok_streak = h.ok_streak.saturating_add(1);
        h.cooldown_until = None;
    }

    /// 一次传输层失败。
    ///
    /// 返回 `true` = 因为这次失败换了路径。**只有失败的那条正是当前 active 时才换**；
    /// 否则（并发里另一条已经换过了）只记录健康状态，避免一次抖动跳过两条路径。
    pub fn record_failure(&self, idx: usize, error: &str) -> bool {
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let Some(h) = inner.health.get_mut(idx) else {
            return false;
        };
        if h.state == RouteState::Blocked {
            return false;
        }
        h.state = RouteState::Failed;
        h.last_fail_unix = Some(now_unix());
        h.last_error = Some(error.to_string());
        h.fail_streak = h.fail_streak.saturating_add(1);
        h.ok_streak = 0;
        let backoff = cooldown_for(h.fail_streak);
        h.cooldown_until = Some(Instant::now() + backoff);
        if inner.active != idx {
            return false;
        }
        switch_away(&mut inner, "failure")
    }

    /// 身份守卫：这条路径指向另一台 NAS / 不可信，永久不用。
    pub fn block(&self, idx: usize, reason: impl Into<String>) {
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let Some(h) = inner.health.get_mut(idx) else {
            return;
        };
        h.state = RouteState::Blocked;
        h.blocked_reason = Some(reason.into());
        h.ok_streak = 0;
        if inner.active == idx {
            switch_away(&mut inner, "blocked");
        }
    }

    pub fn is_blocked(&self, idx: usize) -> bool {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .health
            .get(idx)
            .map(|h| h.state == RouteState::Blocked)
            .unwrap_or(false)
    }

    /// 写回一轮探测结果：解析记录 + 健康状态 + 选路。
    ///
    /// `probes` 会被就地补上 `resolved_changed`（DDNS 是否换了 IP）。
    pub fn note_probes(&self, probes: &mut [RouteProbe]) {
        let now = Instant::now();
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        inner.initial_probed = true;
        for p in probes.iter_mut() {
            let Some(h) = inner.health.get_mut(p.index) else {
                continue;
            };
            if h.state == RouteState::Blocked {
                continue;
            }
            if !p.resolved.is_empty() {
                p.resolved_changed = !h.resolved.is_empty() && h.resolved != p.resolved;
                h.resolved = p.resolved.clone();
                h.resolved_at_unix = Some(now_unix());
                h.resolved_changed = p.resolved_changed;
            }
            if p.http_ok {
                h.state = RouteState::Ok;
                h.last_ok_unix = Some(now_unix());
                h.last_error = None;
                h.latency_ms = p.latency_ms;
                h.fail_streak = 0;
                h.ok_streak = h.ok_streak.saturating_add(1);
                h.cooldown_until = None;
            } else {
                h.state = RouteState::Failed;
                h.last_fail_unix = Some(now_unix());
                h.last_error = Some(p.error.clone().unwrap_or_else(|| "探测失败".into()));
                h.fail_streak = h.fail_streak.saturating_add(1);
                h.ok_streak = 0;
                h.cooldown_until = Some(now + cooldown_for(h.fail_streak));
            }
        }

        // 选路：当前路径不可用 → 立即换到第一条可用；当前可用 → 更靠前的路径
        // 连续成功 2 次才切回（顺序 = 优先级）。
        let first_ok = probes.iter().find(|p| p.http_ok).map(|p| p.index);
        let Some(first_ok) = first_ok else {
            return;
        };
        let active_ok = probes
            .iter()
            .find(|p| p.index == inner.active)
            .map(|p| p.http_ok)
            .unwrap_or(false);
        let target = if !active_ok {
            Some((first_ok, "probe_recovered"))
        } else if first_ok < inner.active && inner.health[first_ok].ok_streak >= PREFERRED_OK_STREAK
        {
            Some((first_ok, "preferred_available"))
        } else {
            None
        };
        if let Some((idx, reason)) = target {
            switch_to(&mut inner, idx, reason);
        }
    }

    /// 当前路径之外，还有没有「没被拉黑、且不在冷却」的候选。
    pub fn has_alternative(&self) -> bool {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        inner.health.iter().enumerate().any(|(i, h)| {
            i != inner.active && !is_cooling(h, now) && h.state != RouteState::Blocked
        })
    }

    /// 全量快照（`Request::Routes` 的响应）。
    pub fn snapshot(&self) -> RoutesData {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let endpoints = inner
            .endpoints
            .iter()
            .zip(inner.health.iter())
            .enumerate()
            .map(|(i, (ep, h))| {
                let cooldown_secs = h
                    .cooldown_until
                    .map(|t| t.saturating_duration_since(now).as_secs())
                    .unwrap_or(0);
                let state = if h.state == RouteState::Blocked {
                    "blocked"
                } else if is_cooling(h, now) {
                    "cooling"
                } else {
                    match h.state {
                        RouteState::Ok => "ok",
                        RouteState::Failed => "failed",
                        RouteState::Unknown => "unknown",
                        RouteState::Blocked => "blocked",
                    }
                };
                let scopes: Vec<String> = h
                    .resolved
                    .iter()
                    .map(|ip| ip_scope(*ip).label().to_string())
                    .collect();
                RouteInfo {
                    label: ep.label.clone(),
                    host: ep.host.clone(),
                    port: ep.port,
                    https: ep.https,
                    primary: ep.primary,
                    active: i == inner.active,
                    state: state.to_string(),
                    last_ok_unix: h.last_ok_unix,
                    last_fail_unix: h.last_fail_unix,
                    last_error: h.last_error.clone(),
                    latency_ms: h.latency_ms,
                    resolved: h.resolved.iter().map(|ip| ip.to_string()).collect(),
                    resolved_scopes: scopes,
                    resolved_at_unix: h.resolved_at_unix,
                    resolved_changed: h.resolved_changed,
                    cooldown_secs,
                    note: h.blocked_reason.clone(),
                }
            })
            .collect();
        RoutesData {
            active: Some(inner.endpoints[inner.active].addr_label()),
            endpoints,
            switches: inner.switches,
            last_switch: inner.last_switch.as_ref().map(SwitchRecord::to_info),
            probed: inner.initial_probed,
        }
    }

    /// `status` 里的小摘要（不触发网络）。
    pub fn summary(&self) -> RouteSummary {
        let data = self.snapshot();
        let healthy = data.healthy();
        RouteSummary {
            active: data.active,
            healthy,
            total: data.endpoints.len(),
            last_switch: data.last_switch,
        }
    }
}

/// 一次路径探测的原始结果（`Client::probe_all_routes` 填，`Router::note_probes` 消费）。
#[derive(Debug, Clone)]
pub struct RouteProbe {
    pub index: usize,
    pub endpoint: Endpoint,
    /// DNS 解析出的地址（A + AAAA，已去重）。解析失败时为空。
    pub resolved: Vec<IpAddr>,
    pub resolve_error: Option<String>,
    /// 这次解析的结果是否与上一次不同（DDNS 更新的证据）。
    pub resolved_changed: bool,
    pub http_ok: bool,
    pub http_status: Option<u16>,
    pub latency_ms: Option<u64>,
    pub error: Option<String>,
}

impl RouteProbe {
    /// 给日志/诊断用的一句话（**不含 URL 查询串**）。
    pub fn describe(&self) -> String {
        if let Some(err) = &self.error {
            return err.clone();
        }
        if let Some(code) = self.http_status {
            return format!("HTTP {code}");
        }
        "正常".into()
    }
}

fn cooldown_for(fail_streak: u32) -> Duration {
    let shift = fail_streak.saturating_sub(1).min(4);
    let secs = COOLDOWN_BASE.as_secs().saturating_mul(1u64 << shift);
    Duration::from_secs(secs).min(COOLDOWN_MAX)
}

fn is_cooling(h: &Health, now: Instant) -> bool {
    h.cooldown_until.is_some_and(|t| t > now)
}

fn switch_away(inner: &mut Inner, reason: &str) -> bool {
    let now = Instant::now();
    let Some(next) = choose_next(inner, now) else {
        return false;
    };
    switch_to(inner, next, reason)
}

/// 选下一条：先按顺序找「没被拉黑、不在冷却」的；都冷却时选冷却最早到期的。
fn choose_next(inner: &Inner, now: Instant) -> Option<usize> {
    let n = inner.endpoints.len();
    if n <= 1 {
        return None;
    }
    for step in 1..n {
        let i = (inner.active + step) % n;
        let h = &inner.health[i];
        if h.state != RouteState::Blocked && !is_cooling(h, now) {
            return Some(i);
        }
    }
    // 全部在冷却：选最早到期的（排除 blocked）
    (0..n)
        .filter(|i| *i != inner.active)
        .filter(|i| inner.health[*i].state != RouteState::Blocked)
        .min_by_key(|i| inner.health[*i].cooldown_until.unwrap_or(now))
}

fn switch_to(inner: &mut Inner, idx: usize, reason: &str) -> bool {
    if idx >= inner.endpoints.len() || idx == inner.active {
        return false;
    }
    let from = inner.endpoints[inner.active].addr_label();
    let to = inner.endpoints[idx].addr_label();
    inner.active = idx;
    inner.switches = inner.switches.saturating_add(1);
    inner.last_switch = Some(SwitchRecord {
        at_unix: now_unix(),
        from,
        to,
        reason: reason.to_string(),
    });
    true
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use qxync_core::EndpointConfig;

    fn link(primary: &str, extras: &[&str]) -> LinkConfig {
        LinkConfig {
            id: "default".into(),
            host: primary.into(),
            port: 9834,
            https: true,
            insecure: false,
            user: "u".into(),
            ipv4_only: false,
            endpoints: extras
                .iter()
                .map(|h| EndpointConfig {
                    label: None,
                    host: (*h).into(),
                    port: Some(9834),
                    https: Some(true),
                })
                .collect(),
            local_namespace: None,
            exclude: Vec::new(),
            filter_temp: true,
            peer_listen: None,
            peer_name: None,
        }
    }

    fn probe(idx: usize, ok: bool) -> RouteProbe {
        RouteProbe {
            index: idx,
            endpoint: Endpoint {
                label: format!("ep{idx}"),
                host: format!("nas{idx}.local"),
                port: 9834,
                https: true,
                primary: false,
            },
            resolved: vec![],
            resolve_error: None,
            resolved_changed: false,
            http_ok: ok,
            http_status: ok.then_some(200),
            latency_ms: Some(10),
            error: (!ok).then(|| "连接失败".into()),
        }
    }

    #[test]
    fn m40_single_endpoint_never_switches() {
        let r = Router::new(&link("nas.local", &[]));
        assert_eq!(r.endpoint_count(), 1);
        assert!(!r.needs_initial_probe());
        assert!(!r.record_failure(0, "连接超时"));
        assert!(!r.has_alternative());
        assert_eq!(r.base_url(), "https://nas.local:9834");
    }

    #[test]
    fn m40_extra_path_is_tried_before_primary() {
        let r = Router::new(&link("ddns.example", &["192.168.1.10"]));
        assert_eq!(r.endpoint_count(), 2);
        assert!(r.needs_initial_probe());
        assert_eq!(r.base_url(), "https://192.168.1.10:9834");
        // 失败 → 换到主地址
        assert!(r.record_failure(0, "连接超时"));
        assert_eq!(r.active().host, "ddns.example");
        // 再失败 → 全部冷却时仍会选「冷却最早」的那条，不会卡死
        assert!(r.record_failure(1, "连接超时"));
        assert!(r.endpoint_count() == 2);
        assert!(r.snapshot().switches >= 1);
    }

    #[test]
    fn m40_concurrent_failure_does_not_skip_a_path() {
        let r = Router::new(&link("ddns.example", &["a.local", "b.local"]));
        assert_eq!(r.base_url(), "https://a.local:9834");
        // 两个线程都在 a 上失败：只有第一次会换路，第二次只记 a 的健康状态
        assert!(r.record_failure(0, "连接被拒"));
        assert_eq!(r.base_url(), "https://b.local:9834");
        assert!(!r.record_failure(0, "连接被拒"));
        assert_eq!(r.base_url(), "https://b.local:9834", "不能一次跳到第三条");
    }

    #[test]
    fn m40_preferred_path_switches_back_after_two_probes() {
        let r = Router::new(&link("ddns.example", &["lan.local"]));
        // 首次探测：LAN 死、DDNS 活 → 立即切到 DDNS
        let mut probes = vec![probe(0, false), probe(1, true)];
        r.note_probes(&mut probes);
        assert_eq!(r.active().host, "ddns.example");

        // LAN 第一次恢复 → 还不能切（避免抖动）
        let mut probes = vec![probe(0, true), probe(1, true)];
        r.note_probes(&mut probes);
        assert_eq!(r.active().host, "ddns.example");
        // 第二次连续成功 → 切回
        let mut probes = vec![probe(0, true), probe(1, true)];
        r.note_probes(&mut probes);
        assert_eq!(r.active().host, "lan.local");
        assert_eq!(
            r.snapshot().last_switch.unwrap().reason,
            "preferred_available"
        );
    }

    #[test]
    fn m40_probe_records_dns_change_and_cooldown() {
        let r = Router::new(&link("ddns.example", &["lan.local"]));
        let mut p0 = probe(0, true);
        p0.resolved = vec!["192.168.1.10".parse().unwrap()];
        let mut p1 = probe(1, true);
        p1.resolved = vec!["203.0.113.7".parse().unwrap()];
        r.note_probes(&mut [p0, p1]);
        let data = r.snapshot();
        assert_eq!(data.endpoints[0].resolved, vec!["192.168.1.10"]);
        assert_eq!(data.endpoints[0].resolved_scopes, vec!["内网"]);
        assert_eq!(data.endpoints[1].resolved_scopes, vec!["公网"]);
        assert!(!data.endpoints[0].resolved_changed);

        // DDNS 换了 IP → resolved_changed
        let mut p1 = probe(1, true);
        p1.resolved = vec!["198.51.100.9".parse().unwrap()];
        let mut p0 = probe(0, true);
        p0.resolved = vec!["192.168.1.10".parse().unwrap()];
        r.note_probes(&mut [p0, p1]);
        let data = r.snapshot();
        assert!(data.endpoints[1].resolved_changed);

        // 失败 → 冷却秒数 > 0
        let mut p1 = probe(1, false);
        p1.error = Some("连接超时".into());
        let mut p0 = probe(0, true);
        p0.resolved = vec!["192.168.1.10".parse().unwrap()];
        r.note_probes(&mut [p0, p1]);
        let data = r.snapshot();
        assert!(data.endpoints[1].cooldown_secs > 0, "{data:?}");
        assert_eq!(data.endpoints[1].state, "cooling");
    }

    #[test]
    fn m40_blocked_path_is_never_used() {
        let r = Router::new(&link("ddns.example", &["wrong-nas.local"]));
        r.block(0, "NAS 身份不符");
        assert_eq!(r.active().host, "ddns.example");
        let data = r.snapshot();
        assert_eq!(data.endpoints[0].state, "blocked");
        assert!(data.endpoints[0]
            .note
            .as_deref()
            .unwrap()
            .contains("身份不符"));
        // 即使探测说它活，也不会切回去
        let mut probes = vec![probe(0, true), probe(1, true)];
        r.note_probes(&mut probes);
        assert_eq!(r.active().host, "ddns.example");
    }
}
