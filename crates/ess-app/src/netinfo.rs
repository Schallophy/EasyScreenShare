//! 本机在局域网里的地址——要发给观看端的那个地址，选错就等于「看起来能跑，别人连不上」。
//!
//! 第一版用的是「向公网地址 UDP connect，取 `local_addr()`」（不发包，只让内核按路由表选出口）。
//! **在这台机器上它选错了**：默认路由被代理的虚拟网卡拿走，于是打印给手机看的是
//! `198.18.0.1`（基准测试网段）——对端根本连不通的地址。所以改成枚举网卡 + 规则打分，
//! UDP 那一招只留作非 Windows 的兜底。

use std::net::Ipv4Addr;

/// 候选网卡地址。字段全是纯数据，所以打分规则可以离线测，不依赖本机有几张网卡。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cand {
    pub ip: Ipv4Addr,
    /// Windows `INTERFACE_TYPE`：6=以太 71=802.11 无线，其它多为虚拟/隧道
    pub if_type: u32,
    pub up: bool,
    /// 这张网卡上有没有默认网关。VPN/TUN 这类「代理型」接口通常没有
    pub gateway: bool,
    /// 有没有**与该地址同网段**的网关（on-link）。这一条才把「真 LAN」和「覆盖网」分开：
    /// VPN 也会挂一条网关，但它不在本机那个 /24 里，对局域网对端没有意义。
    pub onlink_gw: bool,
    pub name: String,
}

/// 物理网卡（以太网/无线）。虚拟适配器一律排到最后。
const PHYSICAL: [u32; 2] = [6, 71];

fn usable(c: &Cand) -> bool {
    // 环回、169.254 的 APIPA、以及非私有网段（代理用的 198.18/15、公网地址）都不能发给局域网对端
    c.up && !c.ip.is_loopback() && !c.ip.is_link_local() && c.ip.is_private()
}

/// 打分顺序：物理且带网关 > 带网关 > 物理 > 其余可用私有地址。
pub fn pick(cands: &[Cand]) -> Option<Ipv4Addr> {
    let tier = |f: fn(&Cand) -> bool| {
        cands
            .iter()
            .filter(|c| usable(c) && f(c))
            .map(|c| c.ip)
            .next()
    };
    tier(|c| PHYSICAL.contains(&c.if_type) && c.onlink_gw)
        .or_else(|| tier(|c| PHYSICAL.contains(&c.if_type) && c.gateway))
        .or_else(|| tier(|c| c.onlink_gw))
        .or_else(|| tier(|c| c.gateway))
        .or_else(|| tier(|_| true))
}

/// 用户手动指定优先（选错时的逃生口，也是多网卡机器上「就要这张」的表达）。
fn advertised() -> Option<Ipv4Addr> {
    std::env::var("ESS_ADVERTISE_IP")
        .ok()?
        .trim()
        .parse::<Ipv4Addr>()
        .ok()
}

/// 返回（选中的地址，全部候选的可读列表）。列表要一并打出去：选错时必须看得见。
pub fn lan_ipv4_list() -> (Option<Ipv4Addr>, Vec<String>) {
    if let Some(a) = advertised() {
        return (Some(a), vec![format!("ESS_ADVERTISE_IP={a}")]);
    }
    let cands = enumerate();
    let listed = cands
        .iter()
        .map(|c| {
            format!(
                "{}={}（type {}，{}{}）",
                c.name,
                c.ip,
                c.if_type,
                if c.up { "up" } else { "down" },
                if c.gateway { "，有网关" } else { "" }
            )
        })
        .collect();
    (pick(&cands), listed)
}

pub fn lan_ipv4() -> Option<Ipv4Addr> {
    lan_ipv4_list().0
}

#[cfg(windows)]
fn enumerate() -> Vec<Cand> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_INCLUDE_GATEWAYS, GET_ADAPTERS_ADDRESSES_FLAGS,
        IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::Networking::WinSock::{AF_INET, SOCKET_ADDRESS, SOCKADDR_IN};

    // GAA_FLAG_INCLUDE_GATEWAYS 必须要：不给这个标志，FirstGatewayAddress 恒为空，
    // 而「有没有默认网关」正是把 VPN/TUN 网卡往后排的关键判据。
    let flags: GET_ADAPTERS_ADDRESSES_FLAGS = GAA_FLAG_INCLUDE_GATEWAYS;
    let mut buf = vec![0u8; 64 * 1024];
    let mut size = buf.len() as u32;
    let mut code = unsafe {
        GetAdaptersAddresses(
            AF_INET as u32,
            flags,
            std::ptr::null(),
            buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH,
            &mut size,
        )
    };
    if code == ERROR_INSUFFICIENT_BUFFER && size as usize > buf.len() {
        buf = vec![0u8; size as usize];
        size = buf.len() as u32;
        code = unsafe {
            GetAdaptersAddresses(
                AF_INET as u32,
                flags,
                std::ptr::null(),
                buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH,
                &mut size,
            )
        };
    }
    if code != 0 {
        return Vec::new();
    }

    fn to_v4(addr: &SOCKET_ADDRESS) -> Option<Ipv4Addr> {
        let sa = addr.lpSockaddr;
        if sa.is_null() || unsafe { (*sa).sa_family } != AF_INET {
            return None;
        }
        // sockaddr 与 SOCKADDR_IN 前缀布局一致，确认地址族之后才敢按这个类型读
        let raw = unsafe { (*sa.cast::<SOCKADDR_IN>()).sin_addr.S_un.S_addr };
        Some(Ipv4Addr::from(u32::from_be(raw)))
    }

    fn name_of(p: *const u16) -> String {
        if p.is_null() {
            return "?".into();
        }
        let mut n = 0usize;
        while n < 256 && unsafe { *p.add(n) } != 0 {
            n += 1;
        }
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, n) })
    }

    let mut out = Vec::new();
    let mut p = buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH;
    let mut guard = 0u32;
    while !p.is_null() && guard < 256 {
        guard += 1;
        let a = unsafe { &*p };
        let mut gateways = Vec::new();
        let mut g = a.FirstGatewayAddress;
        while !g.is_null() {
            if let Some(ip) = to_v4(unsafe { &(*g).Address }) {
                gateways.push(ip);
            }
            g = unsafe { (*g).Next };
        }
        let mut u = a.FirstUnicastAddress;
        while !u.is_null() {
            let ua = unsafe { &*u };
            if let Some(ip) = to_v4(&ua.Address) {
                let plen = ua.OnLinkPrefixLength;
                out.push(Cand {
                    ip,
                    if_type: a.IfType,
                    // IF_OPER_STATUS：1 = up
                    up: a.OperStatus == 1,
                    gateway: !gateways.is_empty(),
                    onlink_gw: gateways.iter().any(|gw| same_prefix(ip, *gw, plen)),
                    name: name_of(a.FriendlyName),
                });
            }
            u = ua.Next;
        }
        p = a.Next;
    }
    out
}

#[cfg(windows)]
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

/// `bits` 位前缀内是否同一网段（`bits=0` 视为不匹配，避免把 0.0.0.0 这种当真网关）
fn same_prefix(a: Ipv4Addr, b: Ipv4Addr, bits: u8) -> bool {
    if bits == 0 || bits > 32 {
        return false;
    }
    let mask = u32::MAX << (32 - bits as u32);
    u32::from(a) & mask == u32::from(b) & mask
}

#[cfg(not(windows))]
fn enumerate() -> Vec<Cand> {
    use std::net::UdpSocket;

    // 非 Windows 没有产品路径：保留旧那一招当兜底（不发包，只让路由表选出口）
    for dst in ["8.8.8.8:53", "223.5.5.5:53"] {
        let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else { continue };
        if sock.connect(dst).is_err() {
            continue;
        }
        if let Ok(IpAddr::V4(v4)) = sock.local_addr() {
            return vec![Cand {
                ip: v4,
                if_type: 0,
                up: !v4.is_loopback(),
                gateway: true,
                name: "route".into(),
            }];
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(ip: &str, if_type: u32, up: bool, gateway: bool, name: &str) -> Cand {
        Cand {
            ip: ip.parse::<Ipv4Addr>().unwrap(),
            if_type,
            up,
            gateway,
            onlink_gw: gateway,
            name: name.into(),
        }
    }

    /// 本机踩过的坑：代理虚拟网卡的 198.18 基准网段地址绝不能被选出去。
    #[test]
    fn picks_lan_over_proxy_benchmark_range() {
        let cands = vec![
            c("198.18.0.1", 6, true, true, "Meta"),
            c("192.168.31.224", 6, true, true, "以太网 2"),
        ];
        assert_eq!(pick(&cands), Some(Ipv4Addr::new(192, 168, 31, 224)));
    }

    /// VPN/TUN 有私有地址但没有默认网关，让位给真网卡。
    #[test]
    fn physical_with_gateway_beats_tunnel() {
        let cands = vec![
            c("10.144.63.59", 6, true, false, "ZeroTier One"),
            c("192.168.31.224", 6, true, true, "以太网 2"),
        ];
        assert_eq!(pick(&cands), Some(Ipv4Addr::new(192, 168, 31, 224)));
    }

    /// 只剩 VPN 时也要给出可用地址（覆盖网里的对端确实用它），别退回 127.0.0.1。
    #[test]
    fn tunnel_alone_is_still_usable() {
        let cands = vec![c("10.144.63.59", 6, true, false, "ZeroTier One")];
        assert_eq!(pick(&cands), Some(Ipv4Addr::new(10, 144, 63, 59)));
    }

    #[test]
    fn skips_loopback_apipa_and_down() {
        let cands = vec![
            c("127.0.0.1", 24, true, true, "loop"),
            c("169.254.8.1", 6, true, true, "apipa"),
            c("192.168.99.9", 6, false, true, "down"),
        ];
        assert_eq!(pick(&cands), None);
    }

    /// 真机上 ZeroTier 与以太网同为 type 6 且都「有网关」，只能靠 on-link 判据分开。
    /// 这条把「VPN 排在前面就发覆盖网地址」钉死。
    #[test]
    fn lan_wins_even_when_tunnel_is_enumerated_first() {
        let cands = vec![
            Cand {
                ip: "10.144.63.59".parse().unwrap(),
                if_type: 6,
                up: true,
                gateway: true,
                onlink_gw: false,
                name: "ZeroTier One".into(),
            },
            c("192.168.31.224", 6, true, true, "以太网 2"),
        ];
        assert_eq!(pick(&cands), Some(Ipv4Addr::new(192, 168, 31, 224)));
    }

    #[test]
    fn same_prefix_only_matches_within_the_mask() {
        let a: Ipv4Addr = "192.168.31.224".parse().unwrap();
        assert!(same_prefix(a, "192.168.31.1".parse().unwrap(), 24));
        assert!(!same_prefix(a, "10.144.63.1".parse().unwrap(), 24));
        assert!(!same_prefix(a, "192.168.31.1".parse().unwrap(), 0), "0 位前缀不该算同网段");
    }

    #[test]
    fn wifi_counts_as_physical() {
        let cands = vec![
            c("10.0.0.5", 24, true, true, "其它"),
            c("192.168.1.7", 71, true, true, "WLAN"),
        ];
        assert_eq!(pick(&cands), Some(Ipv4Addr::new(192, 168, 1, 7)));
    }

    /// 真机这条才是关键：这台机器同时挂着代理网卡（198.18/15）与 VPN 网卡，
    /// 选出来的必须是私有地址，且不能是那次 bug 里的 198.18.0.1。
    #[test]
    fn enumerate_choice_is_a_private_address_on_this_machine() {
        let (chosen, listed) = super::lan_ipv4_list();
        let ip = chosen.expect("本机有可用网卡，必须选出一个地址");
        assert!(ip.is_private(), "选出了非私有地址 {ip}：{listed:?}");
        assert_ne!(ip, Ipv4Addr::new(198, 18, 0, 1), "又选中了代理的基准网段地址");
    }
}
