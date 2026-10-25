use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

/// A LAN reachable IPv4 address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanAddress {
    pub ip: Ipv4Addr,
    pub iface: String,
}

/// All usable LAN IPv4 addresses, most likely-to-work first.
pub fn lan_addresses() -> Vec<LanAddress> {
    let mut out = Vec::new();
    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for iface in interfaces {
            let if_addrs::IfAddr::V4(v4) = iface.addr else {
                continue;
            };
            let ip = v4.ip;
            if ip.is_loopback() || ip.is_unspecified() || ip.is_link_local() || ip.is_broadcast() {
                continue;
            }
            // Skip common virtual / container adapters unless nothing else exists.
            out.push(LanAddress {
                ip,
                iface: iface.name,
            });
        }
    }
    out.sort_by_key(|a| (interface_rank(&a.iface), ip_rank(a.ip), a.ip.octets()));
    out.dedup_by_key(|a| a.ip);
    out
}

fn interface_rank(name: &str) -> u8 {
    let n = name.to_ascii_lowercase();
    const VIRTUAL: &[&str] = &[
        "vethernet",
        "vmware",
        "virtualbox",
        "vbox",
        "hyper-v",
        "loopback",
        "tailscale",
        "zerotier",
        "wireguard",
        "wintun",
        "docker",
        "br-",
        "veth",
        "bluetooth",
        "npcap",
        "tap",
    ];
    if VIRTUAL.iter().any(|v| n.contains(v)) {
        2
    } else if n.contains("wi-fi") || n.contains("wlan") || n.contains("wlp") {
        0
    } else {
        1
    }
}

fn ip_rank(ip: Ipv4Addr) -> u8 {
    let o = ip.octets();
    match o {
        [192, 168, _, _] => 0,
        [10, _, _, _] => 1,
        [172, b, _, _] if (16..=31).contains(&b) => 2,
        _ => 3,
    }
}

/// Best guess for the address other devices on the LAN should use.
pub fn preferred_lan_ip() -> Option<Ipv4Addr> {
    lan_addresses().first().map(|a| a.ip)
}

pub fn all_lan_ips() -> Vec<IpAddr> {
    let mut ips: Vec<IpAddr> = lan_addresses()
        .into_iter()
        .map(|a| IpAddr::V4(a.ip))
        .collect();
    ips.push(IpAddr::V4(Ipv4Addr::LOCALHOST));
    ips
}

fn host_for_url(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    }
}

/// Build `scheme://host:port` for the given address.
pub fn base_url(tls: bool, ip: IpAddr, port: u16) -> String {
    let scheme = if tls { "https" } else { "http" };
    format!("{}://{}:{}", scheme, host_for_url(ip), port)
}

/// Human-friendly label for the connection banner.
pub fn connect_addresses(tls: bool, port: u16, bind: IpAddr) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let wildcard = bind.is_unspecified();
    if !wildcard {
        out.push(("Local".to_string(), base_url(tls, bind, port)));
        return out;
    }
    out.push((
        "Local".to_string(),
        base_url(tls, IpAddr::V4(Ipv4Addr::LOCALHOST), port),
    ));
    for lan in lan_addresses().into_iter().take(4) {
        out.push((lan.iface.clone(), base_url(tls, IpAddr::V4(lan.ip), port)));
    }
    out
}

pub const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_ranges_rank_first() {
        assert!(ip_rank(Ipv4Addr::new(192, 168, 1, 10)) < ip_rank(Ipv4Addr::new(8, 8, 8, 8)));
        assert!(ip_rank(Ipv4Addr::new(10, 0, 0, 5)) < ip_rank(Ipv4Addr::new(203, 0, 113, 9)));
    }

    #[test]
    fn url_formatting() {
        assert_eq!(
            base_url(true, IpAddr::V4(Ipv4Addr::new(192, 168, 0, 2)), 8443),
            "https://192.168.0.2:8443"
        );
        assert_eq!(
            base_url(false, "fe80::1".parse().unwrap(), 8080),
            "http://[fe80::1]:8080"
        );
    }

    #[test]
    fn virtual_adapters_deprioritised() {
        assert!(interface_rank("Wi-Fi") < interface_rank("vEthernet (WSL)"));
    }
}
