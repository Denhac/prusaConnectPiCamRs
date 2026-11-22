//! Network facts shared by the outputs.

use std::net::{IpAddr, Ipv4Addr};

/// Whether `ip` is a Tailscale address: 100.64.0.0/10 or fd7a:115c:a1e0::/48. Tailscale is for
/// administering the Pis; pictures and video never go over it.
pub fn is_tailscale(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            a == 100 && b & 0xc0 == 64
        }
        IpAddr::V6(v6) => v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

/// Whether `ip` is on a private network rather than the internet: this machine, a LAN (RFC 1918,
/// link-local, IPv6 unique-local), or the shared address space of RFC 6598, which Tailscale uses.
pub fn is_private(ip: IpAddr) -> bool {
    let v4 = |v4: Ipv4Addr| {
        let [a, b, ..] = v4.octets();
        v4.is_loopback() || v4.is_private() || v4.is_link_local() || (a == 100 && b & 0xc0 == 64)
    };
    match ip {
        IpAddr::V4(ip) => v4(ip),
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(mapped) => v4(mapped),
            None => ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_private_networks_from_the_internet() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        for private in [
            "127.0.0.1",
            "10.0.0.36",
            "172.16.0.1",
            "192.168.1.218",
            "169.254.1.1",
            "100.88.1.20",
            "::1",
            "fd7a:115c:a1e0::1",
            "fe80::1",
            "::ffff:10.0.0.36",
        ] {
            assert!(is_private(ip(private)), "{private}");
        }
        for public in [
            "8.8.8.8",
            "203.0.113.48",
            "172.32.0.1",
            "2001:db8::1",
            "::ffff:8.8.8.8",
        ] {
            assert!(!is_private(ip(public)), "{public}");
        }
    }

    #[test]
    fn recognises_tailscale_addresses() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        assert!(is_tailscale(ip("100.88.1.10")));
        assert!(is_tailscale(ip("100.64.0.1")));
        assert!(is_tailscale(ip("100.127.255.255")));
        assert!(!is_tailscale(ip("100.128.0.1")));
        assert!(!is_tailscale(ip("100.63.255.255")));
        assert!(!is_tailscale(ip("10.0.0.36")));
        assert!(is_tailscale(ip("fd7a:115c:a1e0::1")));
        assert!(!is_tailscale(ip("fd7a:115c:a1e1::1")));
    }
}
