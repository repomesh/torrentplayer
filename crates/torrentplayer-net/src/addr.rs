// SPDX-License-Identifier: MIT OR Apache-2.0

//! Telling public addresses from the user's own network.
//!
//! Some destinations come from untrusted sources: a `.torrent` file's web seeds, or addresses
//! other peers pass on. Connecting to whatever they name would let a malicious torrent or peer
//! reach hosts on the user's network (routers, printers, local services) from inside TorrentPlayer, a
//! server-side request forgery. Such destinations must be public.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Whether `ip` is a globally routable unicast address: not unspecified, loopback, private,
/// link-local, shared (carrier-grade NAT), documentation, benchmarking, multicast, broadcast, or
/// otherwise reserved. IPv4-mapped and 6to4 IPv6 addresses are judged by the IPv4 address they
/// carry.
#[must_use]
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [first, second, third, _] = ip.octets();
    let reserved = first == 0 // "this network"
        || first == 10 // private
        || first == 127 // loopback
        || (first == 100 && (64..128).contains(&second)) // shared address space
        || (first == 169 && second == 254) // link-local
        || (first == 172 && (16..32).contains(&second)) // private
        || (first == 192 && second == 0 && (third == 0 || third == 2)) // protocol assignments, documentation
        || (first == 192 && second == 168) // private
        || (first == 198 && (second == 18 || second == 19)) // benchmarking
        || (first == 198 && second == 51 && third == 100) // documentation
        || (first == 203 && second == 0 && third == 113) // documentation
        || first >= 224; // multicast, reserved, broadcast
    !reserved
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let s = ip.segments();
    // Global unicast is 2000::/3. This also excludes the unspecified and loopback addresses,
    // unique local (fc00::/7), link-local (fe80::/10), multicast, and the embedded-IPv4
    // prefixes (::/96, 64:ff9b::/96), which would otherwise reach IPv4 hosts.
    if s[0] & 0xe000 != 0x2000 {
        return false;
    }
    match (s[0], s[1]) {
        // Documentation.
        (0x2001, 0x0db8) => false,
        // 6to4: judged by the IPv4 address it carries.
        (0x2002, _) => {
            let [hi, lo] = [s[1].to_be_bytes(), s[2].to_be_bytes()];
            is_public_v4(Ipv4Addr::new(hi[0], hi[1], lo[0], lo[1]))
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public(s: &str) -> bool {
        is_public_ip(s.parse().unwrap())
    }

    #[test]
    fn classifies_ipv4() {
        for ok in [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "100.63.255.255",
            "172.32.0.1",
        ] {
            assert!(public(ok), "{ok}");
        }
        for bad in [
            "0.0.0.0",
            "10.1.2.3",
            "127.0.0.1",
            "100.64.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.2.1",
            "192.168.1.1",
            "198.18.0.1",
            "203.0.113.9",
            "224.0.0.1",
            "255.255.255.255",
        ] {
            assert!(!public(bad), "{bad}");
        }
    }

    #[test]
    fn classifies_ipv6() {
        for ok in [
            "2606:4700::1111",
            "2a00:1450:4001::200e",
            "2002:0808:0808::1",
        ] {
            assert!(public(ok), "{ok}");
        }
        for bad in [
            "::",
            "::1",
            "fe80::1",
            "fd00::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
            "::ffff:192.168.0.1",
            "64:ff9b::7f00:1",
            "2002:c0a8:0101::1",
        ] {
            assert!(!public(bad), "{bad}");
        }
        assert!(public("::ffff:8.8.8.8"));
    }
}
