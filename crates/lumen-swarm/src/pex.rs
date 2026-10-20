// SPDX-License-Identifier: MIT OR Apache-2.0

//! Peer exchange (BEP 11): connected peers tell TorrentStream about other peers of the swarm.
//!
//! TorrentStream only receives peer lists; it advertises `ut_pex` in its extended handshake (BEP 10) but
//! sends none. Everything in a message is untrusted: the number of addresses taken is bounded,
//! and addresses outside the sender's own scope are dropped, so a peer on the internet cannot
//! make TorrentStream connect to hosts on the user's network.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use crate::bencode::{self, BencodeError, Value};

/// Most addresses taken from one message. BEP 11 allows 50 added peers per message; a few more
/// are tolerated.
const MAX_ADDED: usize = 100;
/// `added.f` flag: the peer is a seed (or only uploads).
const FLAG_SEED: u8 = 0x02;
/// `added.f` flag: the peer accepts incoming connections.
const FLAG_CONNECTABLE: u8 = 0x10;

/// A peer learned through peer exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PexPeer {
    pub(crate) addr: SocketAddr,
    /// The sender says this peer has the whole torrent.
    pub(crate) seed: bool,
    /// The sender says this peer accepts incoming connections.
    pub(crate) connectable: bool,
}

/// Parses a `ut_pex` message from the peer at `source`.
pub(crate) fn parse(payload: &[u8], source: SocketAddr) -> Result<Vec<PexPeer>, BencodeError> {
    let message = bencode::decode(payload)?;
    let mut peers = Vec::new();
    for (key, flags_key, entry_len) in [
        (&b"added"[..], &b"added.f"[..], 6),
        (&b"added6"[..], &b"added6.f"[..], 18),
    ] {
        let Some(compact) = message.get(key).and_then(Value::as_bytes) else {
            continue;
        };
        let flags = message
            .get(flags_key)
            .and_then(Value::as_bytes)
            .unwrap_or_default();
        for (i, entry) in compact.chunks_exact(entry_len).enumerate() {
            if peers.len() >= MAX_ADDED {
                return Ok(peers);
            }
            let Some(addr) = compact_addr(entry) else {
                continue;
            };
            if allowed(addr, source) {
                let flag = flags.get(i).copied().unwrap_or_default();
                peers.push(PexPeer {
                    addr,
                    seed: flag & FLAG_SEED != 0,
                    connectable: flag & FLAG_CONNECTABLE != 0,
                });
            }
        }
    }
    Ok(peers)
}

/// A compact address: 4 or 16 address bytes, then a big-endian port.
fn compact_addr(entry: &[u8]) -> Option<SocketAddr> {
    let (ip, port) = entry.split_at_checked(entry.len().checked_sub(2)?)?;
    let port = u16::from_be_bytes([port[0], port[1]]);
    let ip = match ip.len() {
        4 => IpAddr::V4(Ipv4Addr::new(ip[0], ip[1], ip[2], ip[3])),
        16 => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(ip);
            IpAddr::V6(Ipv6Addr::from(octets))
        }
        _ => return None,
    };
    (port != 0).then_some(SocketAddr::new(ip, port))
}

/// How far an address reaches: loopback, the local network, or the internet. `None` for
/// addresses no peer can have.
fn scope(ip: IpAddr) -> Option<u8> {
    let unusable = ip.is_unspecified()
        || ip.is_multicast()
        || matches!(ip, IpAddr::V4(v4) if v4.is_broadcast());
    if unusable {
        None
    } else if ip.is_loopback() {
        Some(0)
    } else if lumen_net::is_public_ip(ip) {
        Some(2)
    } else {
        Some(1)
    }
}

/// Whether to accept `addr` from a peer at `source`: a peer on the internet may only name
/// public addresses, one on the local network may also name local ones, and only a peer on
/// this machine may name this machine.
fn allowed(addr: SocketAddr, source: SocketAddr) -> bool {
    match (scope(addr.ip()), scope(source.ip())) {
        (Some(a), Some(s)) => a >= s,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn message(added: &[u8], flags: &[u8], added6: &[u8]) -> Vec<u8> {
        let mut m = format!("d5:added{}:", added.len()).into_bytes();
        m.extend_from_slice(added);
        m.extend_from_slice(format!("7:added.f{}:", flags.len()).as_bytes());
        m.extend_from_slice(flags);
        m.extend_from_slice(format!("6:added6{}:", added6.len()).as_bytes());
        m.extend_from_slice(added6);
        m.extend_from_slice(b"7:dropped0:e");
        m
    }

    #[test]
    fn parses_ipv4_and_ipv6_peers_with_flags() {
        let added = [[1, 2, 3, 4, 0x1a, 0xe1], [5, 6, 7, 8, 0, 80]].concat();
        let mut v6 = vec![0x26, 0x06, 0x47, 0];
        v6.extend_from_slice(&[0; 11]);
        v6.extend_from_slice(&[1, 0x1f, 0x90]);
        let m = message(&added, &[0x02, 0x10], &v6);
        let peers = parse(&m, addr("9.9.9.9:6881")).unwrap();
        assert_eq!(
            peers,
            [
                PexPeer {
                    addr: addr("1.2.3.4:6881"),
                    seed: true,
                    connectable: false,
                },
                PexPeer {
                    addr: addr("5.6.7.8:80"),
                    seed: false,
                    connectable: true,
                },
                PexPeer {
                    addr: addr("[2606:4700::1]:8080"),
                    seed: false,
                    connectable: false,
                },
            ]
        );
    }

    #[test]
    fn drops_addresses_outside_the_senders_scope() {
        let added = [
            [192, 168, 1, 10, 0x1a, 0xe1], // local network
            [127, 0, 0, 1, 0x1a, 0xe1],    // loopback
            [0, 0, 0, 0, 0x1a, 0xe1],      // unusable
            [8, 8, 8, 8, 0, 0],            // port 0
            [8, 8, 4, 4, 0x1a, 0xe1],      // public: kept
        ]
        .concat();
        let m = message(&added, &[], &[]);
        let from_internet = parse(&m, addr("9.9.9.9:1")).unwrap();
        assert_eq!(from_internet.len(), 1);
        assert_eq!(from_internet[0].addr, addr("8.8.4.4:6881"));
        let from_lan = parse(&m, addr("192.168.1.2:1")).unwrap();
        assert_eq!(from_lan.len(), 2);
        let from_loopback = parse(&m, addr("127.0.0.1:1")).unwrap();
        assert_eq!(from_loopback.len(), 3);
    }

    #[test]
    fn bounds_the_number_of_peers_and_rejects_junk() {
        let added: Vec<u8> = (0..500u32)
            .flat_map(|i| {
                let [_, _, b, c] = i.to_be_bytes();
                [8, 1, b, c, 0x1a, 0xe1]
            })
            .collect();
        let m = message(&added, &[], &[]);
        assert_eq!(parse(&m, addr("9.9.9.9:1")).unwrap().len(), MAX_ADDED);
        assert!(parse(b"not bencode", addr("9.9.9.9:1")).is_err());
        // Wrong types and truncated entries are ignored.
        assert!(
            parse(b"d5:addedi5ee", addr("9.9.9.9:1"))
                .unwrap()
                .is_empty()
        );
        assert!(
            parse(b"d5:added5:abcdee", addr("9.9.9.9:1"))
                .unwrap()
                .is_empty()
        );
    }
}
