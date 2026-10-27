//! ★ M40：网络路径的纯函数 —— IP 作用域分类 + 展示名。
//!
//! 多路径诊断要回答的核心问题之一就是「这个域名解析出来的地址，到底是内网、
//! 公网，还是运营商 NAT 后面的假公网」。这件事不依赖任何网络运行时，做成纯函数
//! 方便单测（client/daemon/CLI/GUI 共用同一套判定，不各写一份）。
//!
//! 分类只用于**展示与提示**，不参与「这条路径通不通」的判定：
//! 一条解析到内网的路径在办公室就是不可达，但客户端不能凭 IP 就拒绝它。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use serde::{Deserialize, Serialize};

/// IP 地址的归属类别（诊断展示用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpScope {
    /// 回环（127.0.0.0/8、::1）。
    Loopback,
    /// RFC1918 内网（10/8、172.16/12、192.168/16）。
    Private,
    /// 运营商级 NAT（100.64.0.0/10）——「看起来像公网，其实外网连不进来」。
    Cgnat,
    /// 链路本地（169.254/16、fe80::/10）。
    LinkLocal,
    /// 未指定地址（0.0.0.0、::）。
    Unspecified,
    /// 其余按公网处理。
    Public,
}

impl IpScope {
    /// 中文短标签（CLI/GUI 直接展示）。
    pub fn label(self) -> &'static str {
        match self {
            IpScope::Loopback => "回环",
            IpScope::Private => "内网",
            IpScope::Cgnat => "运营商 NAT",
            IpScope::LinkLocal => "链路本地",
            IpScope::Unspecified => "未指定",
            IpScope::Public => "公网",
        }
    }

    /// 这条地址是否只可能在某个局域网/本机里可达。
    ///
    /// **不要**拿它做「跳过这条路径」的判据：用户的 NAS 就在那个内网里，
    /// 或者本机通过 VPN 进得去。它只用于诊断文案。
    pub fn is_lan_like(self) -> bool {
        matches!(
            self,
            IpScope::Loopback | IpScope::Private | IpScope::LinkLocal
        )
    }

    /// 给用户的补充提示（没有特殊情况时 `None`）。
    pub fn hint(self) -> Option<&'static str> {
        match self {
            IpScope::Cgnat => Some(
                "这是运营商级 NAT 地址（100.64.0.0/10）：从公网直连通常不可达，\
                 需要 NAS 侧做内网穿透 / VPN，或确认运营商给的是真公网 IP",
            ),
            IpScope::Private => Some(
                "这是内网地址：只有当前网络与该内网互通时可用；\
                 如果人不在这个内网，请把公网/DDNS 路径排在它前面或删除它",
            ),
            IpScope::LinkLocal => Some("这是链路本地地址，通常只在本机直连的网段内可用"),
            IpScope::Loopback => Some("这是本机回环地址，只有跑在本机上的服务可达"),
            IpScope::Unspecified | IpScope::Public => None,
        }
    }
}

/// 判断一个 IP 属于哪一类。
pub fn ip_scope(ip: IpAddr) -> IpScope {
    match ip {
        IpAddr::V4(v4) => ipv4_scope(v4),
        IpAddr::V6(v6) => ipv6_scope(v6),
    }
}

fn ipv4_scope(ip: Ipv4Addr) -> IpScope {
    if ip.is_unspecified() {
        return IpScope::Unspecified;
    }
    if ip.is_loopback() {
        return IpScope::Loopback;
    }
    if ip.is_link_local() {
        return IpScope::LinkLocal;
    }
    // 100.64.0.0/10（RFC 6598，运营商级 NAT）。Rust 的 `is_private()` 不含它。
    if ip.octets()[0] == 100 && (ip.octets()[1] & 0xc0) == 64 {
        return IpScope::Cgnat;
    }
    if ip.is_private() {
        return IpScope::Private;
    }
    IpScope::Public
}

fn ipv6_scope(ip: Ipv6Addr) -> IpScope {
    if ip.is_unspecified() {
        return IpScope::Unspecified;
    }
    if ip.is_loopback() {
        return IpScope::Loopback;
    }
    // fe80::/10
    if (ip.segments()[0] & 0xffc0) == 0xfe80 {
        return IpScope::LinkLocal;
    }
    // fc00::/7（唯一本地地址，内网）
    if (ip.segments()[0] & 0xfe00) == 0xfc00 {
        return IpScope::Private;
    }
    IpScope::Public
}

/// 主机名是不是 IP 字面量（含 IPv6）。
pub fn is_ip_literal(host: &str) -> bool {
    host.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .is_ok()
}

/// 展示用：把 `IpAddr` 变成 `v4` / `v6` 前缀形式（GUI/CLI 表格里窄）。
pub fn format_ip(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => v6.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(s: &str) -> IpScope {
        ip_scope(s.parse().unwrap())
    }

    #[test]
    fn ipv4_scopes_match_rfc_notes() {
        assert_eq!(scope("127.0.0.1"), IpScope::Loopback);
        assert_eq!(scope("10.1.2.3"), IpScope::Private);
        assert_eq!(scope("172.16.0.1"), IpScope::Private);
        assert_eq!(scope("172.31.255.254"), IpScope::Private);
        assert_eq!(scope("192.168.31.100"), IpScope::Private);
        assert_eq!(scope("169.254.1.1"), IpScope::LinkLocal);
        assert_eq!(scope("0.0.0.0"), IpScope::Unspecified);
        // 100.64/10 是 CGNAT；100.63/10 与 100.128/10 都不是
        assert_eq!(scope("100.64.0.1"), IpScope::Cgnat);
        assert_eq!(scope("100.127.255.254"), IpScope::Cgnat);
        assert_eq!(scope("100.63.0.1"), IpScope::Public);
        assert_eq!(scope("100.128.0.1"), IpScope::Public);
        assert_eq!(scope("8.8.8.8"), IpScope::Public);
        assert_eq!(scope("203.0.113.7"), IpScope::Public);
        // 172.32 已经不是 172.16/12
        assert_eq!(scope("172.32.0.1"), IpScope::Public);
    }

    #[test]
    fn ipv6_scopes_match_rfc_notes() {
        assert_eq!(scope("::1"), IpScope::Loopback);
        assert_eq!(scope("::"), IpScope::Unspecified);
        assert_eq!(scope("fe80::1"), IpScope::LinkLocal);
        assert_eq!(scope("fd00::1"), IpScope::Private);
        assert_eq!(scope("fc00::1"), IpScope::Private);
        assert_eq!(scope("2001:4860:4860::8888"), IpScope::Public);
    }

    #[test]
    fn hints_are_only_for_special_scopes() {
        assert!(IpScope::Cgnat.hint().unwrap().contains("100.64"));
        assert!(IpScope::Private.hint().unwrap().contains("内网"));
        assert!(IpScope::Public.hint().is_none());
        assert!(IpScope::Unspecified.hint().is_none());
        assert!(IpScope::Private.is_lan_like());
        assert!(!IpScope::Public.is_lan_like());
    }

    #[test]
    fn ip_literal_detection_handles_brackets_and_names() {
        assert!(is_ip_literal("192.168.1.2"));
        assert!(is_ip_literal("[fe80::1]"));
        assert!(is_ip_literal("::1"));
        assert!(!is_ip_literal("nas.local"));
        assert!(!is_ip_literal("qnap.kdajv.com"));
        assert_eq!(format_ip("::1".parse().unwrap()), "::1");
    }
}
