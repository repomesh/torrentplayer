// SPDX-License-Identifier: MIT OR Apache-2.0

//! UDP tracker announces (BEP 15) over sockets from TorrentPlayer's network manager.
//!
//! librqbit's UDP tracker client opens its own unbound socket, which is fine in direct mode but
//! would bypass a proxy or tunnel. This client takes its socket from [`Network::udp_socket`],
//! which keeps it inside the WireGuard tunnel or the SOCKS5 UDP relay, so torrents that list only
//! UDP trackers still find peers in a protected mode. Responses are
//! untrusted: lengths, transaction ids, and peer counts are checked, and tracker and peer
//! addresses on the user's own network are refused.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use futures_util::{StreamExt as _, stream::FuturesUnordered};
use tokio::net::UdpSocket;
use torrentplayer_net::{Network, is_public_ip};
use url::Url;

/// BEP 15's protocol id, sent with every connect request.
const PROTOCOL_ID: u64 = 0x0417_2710_1980;
const ACTION_CONNECT: u32 = 0;
const ACTION_ANNOUNCE: u32 = 1;
const ACTION_ERROR: u32 = 3;
/// Waits for each attempt; BEP 15 suggests 15 s doubling, which is far too slow for streaming.
const ATTEMPT_TIMEOUTS: [Duration; 2] = [Duration::from_secs(4), Duration::from_secs(8)];
/// Largest response read: the header plus 300 IPv6 peers.
const MAX_RESPONSE: usize = 20 + 300 * 18;
/// Most peers taken from one response.
const MAX_PEERS: usize = 200;
/// Enough candidates to start magnet metadata fetching without waiting for silent trackers.
const BOOTSTRAP_PEERS: usize = 20;
/// Longest error message kept from a tracker.
const MAX_ERROR_CHARS: usize = 200;
/// Re-announce interval bounds, whatever the tracker asks for.
pub(crate) const MIN_INTERVAL: Duration = Duration::from_secs(60);
pub(crate) const MAX_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// What to announce.
#[derive(Debug, Clone, Copy)]
pub struct AnnounceRequest {
    /// The torrent's info hash.
    pub info_hash: [u8; 20],
    /// Our peer id.
    pub peer_id: [u8; 20],
    /// Bytes still missing.
    pub left: u64,
    /// The port announced (TorrentPlayer accepts no connections; trackers want one anyway).
    pub port: u16,
    /// Whether this is the first announce (`started`).
    pub started: bool,
}

/// A tracker's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announced {
    /// Peers, public addresses only.
    pub peers: Vec<SocketAddr>,
    /// When to announce again, clamped to `MIN_INTERVAL..=MAX_INTERVAL`.
    pub interval: Duration,
}

/// Announces to the UDP tracker at `url` (`udp://host:port/...`), through `network` if given.
///
/// # Errors
///
/// Returns a short description if the URL is not a usable UDP tracker, the network refuses
/// UDP, or the tracker does not answer properly.
pub async fn announce(
    network: Option<&Network>,
    url: &Url,
    request: &AnnounceRequest,
) -> Result<Announced, String> {
    if url.scheme() != "udp" {
        return Err("not a UDP tracker".into());
    }
    let host = url.host_str().ok_or("tracker has no host")?;
    let port = url.port().ok_or("tracker has no port")?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if let Some(network) = network {
        if !network.policy().trackers_allowed {
            return Err("trackers are disabled in this mode".into());
        }
        if let Ok(ip) = host.parse::<IpAddr>() {
            if !is_public_ip(ip) {
                return Err("the tracker is on the local network".into());
            }
        } else if host.trim_end_matches('.').eq_ignore_ascii_case("localhost")
            || host
                .trim_end_matches('.')
                .to_ascii_lowercase()
                .ends_with(".localhost")
        {
            return Err("the tracker is on the local network".into());
        }
        // Retain address-family preference and retries for ordinary scoped resolution.
        // Only SOCKS5 remote DNS needs a domain-addressed relay request.
        match network.resolve_host(host, port).await {
            Ok(addresses) => return announce_addresses(Some(network), addresses, request).await,
            Err(torrentplayer_net::NetError::Unsupported(_)) => {}
            Err(error) => return Err(format!("cannot resolve the tracker: {error}")),
        }
        let (socket, target, lease) = network
            .udp_socket_host(host, port)
            .await
            .map_err(|e| e.to_string())?;
        if matches!(target, torrentplayer_net::socks5::Target::Addr(a) if !is_public_ip(a.ip())) {
            return Err("the tracker has no public address".into());
        }
        return announce_socket(&socket, target, request, &|| lease.is_valid()).await;
    }
    let addrs: Vec<SocketAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => tokio::net::lookup_host((host, port))
            .await
            .map_err(|e| format!("cannot resolve the tracker: {e}"))?
            .collect(),
    };
    announce_addresses(None, addrs, request).await
}

async fn announce_addresses(
    network: Option<&Network>,
    addrs: Vec<SocketAddr>,
    request: &AnnounceRequest,
) -> Result<Announced, String> {
    let mut last = String::from("the tracker has no public address");
    // IPv4 first: most trackers and VPN tunnels have it.
    let mut candidates: Vec<SocketAddr> =
        addrs.into_iter().filter(|a| is_public_ip(a.ip())).collect();
    candidates.sort_by_key(SocketAddr::is_ipv6);
    for target in candidates.into_iter().take(2) {
        match announce_to(network, target, request).await {
            Ok(announced) => return Ok(announced),
            Err(error) => last = error,
        }
    }
    Err(last)
}

/// Announces once to up to `MAX_AT_ONCE` of `trackers` (UDP ones only) at the same time and
/// returns the peers they name, without duplicates, within `deadline`. For finding peers of a
/// magnet link when only TorrentPlayer's own UDP client can reach its trackers (in a protected
/// mode); failures are skipped.
pub async fn announce_all(
    network: Option<&Network>,
    trackers: &[Url],
    info_hash: [u8; 20],
    deadline: Duration,
) -> Vec<SocketAddr> {
    const MAX_AT_ONCE: usize = 8;
    let request = AnnounceRequest {
        info_hash,
        peer_id: crate::torrent::generate_peer_id(),
        left: u64::MAX / 2,
        port: 6881,
        started: true,
    };
    let announces = trackers
        .iter()
        .filter(|u| u.scheme() == "udp")
        .take(MAX_AT_ONCE)
        .map(|url| announce(network, url, &request))
        .collect::<FuturesUnordered<_>>();
    collect_peers(announces, deadline).await
}

async fn collect_peers(
    mut announces: impl futures_util::Stream<Item = Result<Announced, String>> + Unpin,
    deadline: Duration,
) -> Vec<SocketAddr> {
    let mut peers: Vec<SocketAddr> = Vec::new();
    let gather = async {
        while let Some(result) = announces.next().await {
            if let Ok(announced) = result {
                for addr in announced.peers {
                    if !peers.contains(&addr) && peers.len() < MAX_PEERS {
                        peers.push(addr);
                    }
                }
                if peers.len() >= BOOTSTRAP_PEERS {
                    break;
                }
            }
        }
    };
    // Keep answers that arrived before the deadline even if another tracker never replies.
    let _ = tokio::time::timeout(deadline, gather).await;
    peers
}

async fn announce_to(
    network: Option<&Network>,
    target: SocketAddr,
    request: &AnnounceRequest,
) -> Result<Announced, String> {
    let (socket, lease) = if let Some(network) = network {
        let (socket, lease) = network
            .udp_socket(target)
            .await
            .map_err(|e| e.to_string())?;
        (socket, Some(lease))
    } else {
        let any = if target.is_ipv4() {
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
        } else {
            SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))
        };
        (
            UdpSocket::bind(any)
                .await
                .map_err(|e| e.to_string())?
                .into(),
            None,
        )
    };
    let still_allowed = || {
        lease
            .as_ref()
            .is_none_or(torrentplayer_net::Lease::is_valid)
    };

    announce_socket(
        &socket,
        torrentplayer_net::socks5::Target::Addr(target),
        request,
        &still_allowed,
    )
    .await
}

async fn announce_socket(
    socket: &torrentplayer_net::DatagramSocket,
    target: torrentplayer_net::socks5::Target,
    request: &AnnounceRequest,
    still_allowed: &(dyn Fn() -> bool + Sync),
) -> Result<Announced, String> {
    let tid = transaction_id()?;
    let connect = connect_request(tid);
    let (reply, peer) = exchange(socket, &target, &connect, still_allowed).await?;
    let connection_id = parse_connect(&reply, tid)?;

    let tid = transaction_id()?;
    let key = transaction_id()?;
    let packet = announce_request(connection_id, tid, key, request);
    let (reply, _) = exchange(
        socket,
        &torrentplayer_net::socks5::Target::Addr(peer),
        &packet,
        still_allowed,
    )
    .await?;
    parse_announce(&reply, tid, peer.is_ipv6())
}

/// Sends `packet` and waits for a reply from `target`, retrying once.
async fn exchange(
    socket: &torrentplayer_net::DatagramSocket,
    target: &torrentplayer_net::socks5::Target,
    packet: &[u8],
    still_allowed: &(dyn Fn() -> bool + Sync),
) -> Result<(Vec<u8>, SocketAddr), String> {
    let mut buf = vec![0u8; MAX_RESPONSE];
    for timeout in ATTEMPT_TIMEOUTS {
        if !still_allowed() {
            return Err("the network mode changed".into());
        }
        socket
            .send_to_target(packet, target)
            .await
            .map_err(|e| e.to_string())?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match tokio::time::timeout_at(deadline, socket.recv_from(&mut buf)).await {
                Err(_) => break,
                Ok(Err(e)) => return Err(e.to_string()),
                // Datagrams from anyone else are ignored.
                Ok(Ok((n, from))) => {
                    let source_ok = match target {
                        torrentplayer_net::socks5::Target::Addr(addr) => from == *addr,
                        torrentplayer_net::socks5::Target::Domain(_, port) => {
                            from.port() == *port && is_public_ip(from.ip())
                        }
                    };
                    if source_ok && buf.get(4..8).filter(|_| n >= 8) == packet.get(12..16) {
                        return Ok((buf[..n].to_vec(), from));
                    }
                }
            }
        }
    }
    Err("the tracker did not answer".into())
}

fn transaction_id() -> Result<u32, String> {
    let mut bytes = [0u8; 4];
    getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
    Ok(u32::from_be_bytes(bytes))
}

fn connect_request(tid: u32) -> [u8; 16] {
    let mut p = [0u8; 16];
    p[..8].copy_from_slice(&PROTOCOL_ID.to_be_bytes());
    p[8..12].copy_from_slice(&ACTION_CONNECT.to_be_bytes());
    p[12..].copy_from_slice(&tid.to_be_bytes());
    p
}

fn announce_request(connection_id: u64, tid: u32, key: u32, r: &AnnounceRequest) -> Vec<u8> {
    let mut p = Vec::with_capacity(98);
    p.extend_from_slice(&connection_id.to_be_bytes());
    p.extend_from_slice(&ACTION_ANNOUNCE.to_be_bytes());
    p.extend_from_slice(&tid.to_be_bytes());
    p.extend_from_slice(&r.info_hash);
    p.extend_from_slice(&r.peer_id);
    p.extend_from_slice(&0u64.to_be_bytes()); // downloaded
    p.extend_from_slice(&r.left.to_be_bytes());
    p.extend_from_slice(&0u64.to_be_bytes()); // uploaded
    p.extend_from_slice(&u32::from(r.started).wrapping_mul(2).to_be_bytes()); // 2 = started
    p.extend_from_slice(&0u32.to_be_bytes()); // IP: the sender's
    p.extend_from_slice(&key.to_be_bytes());
    p.extend_from_slice(&(-1i32).to_be_bytes()); // as many peers as the tracker likes
    p.extend_from_slice(&r.port.to_be_bytes());
    p
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_be_bytes)
}

/// Checks the action and transaction id; a tracker error becomes `Err`.
fn check_header(reply: &[u8], action: u32, tid: u32) -> Result<(), String> {
    let (Some(got_action), Some(got_tid)) = (u32_at(reply, 0), u32_at(reply, 4)) else {
        return Err("the tracker's answer is too short".into());
    };
    if got_tid != tid {
        return Err("the tracker's answer is for another request".into());
    }
    if got_action == ACTION_ERROR {
        let text = String::from_utf8_lossy(&reply[8..]);
        let text: String = text
            .chars()
            .filter(|c| !c.is_control())
            .take(MAX_ERROR_CHARS)
            .collect();
        return Err(format!("the tracker refused: {text}"));
    }
    if got_action != action {
        return Err("the tracker's answer is not what was asked for".into());
    }
    Ok(())
}

fn parse_connect(reply: &[u8], tid: u32) -> Result<u64, String> {
    check_header(reply, ACTION_CONNECT, tid)?;
    reply
        .get(8..16)
        .and_then(|s| s.try_into().ok())
        .map(u64::from_be_bytes)
        .ok_or_else(|| "the tracker's connect answer is too short".into())
}

fn parse_announce(reply: &[u8], tid: u32, ipv6: bool) -> Result<Announced, String> {
    check_header(reply, ACTION_ANNOUNCE, tid)?;
    let interval = u32_at(reply, 8).ok_or("the tracker's announce answer is too short")?;
    if reply.len() < 20 {
        return Err("the tracker's announce answer is too short".into());
    }
    let entry = if ipv6 { 18 } else { 6 };
    let peers = reply[20..]
        .chunks_exact(entry)
        .filter_map(|c| {
            let (ip, port) = c.split_at(entry - 2);
            let ip = if ipv6 {
                IpAddr::from(<[u8; 16]>::try_from(ip).ok()?)
            } else {
                IpAddr::from(<[u8; 4]>::try_from(ip).ok()?)
            };
            let port = u16::from_be_bytes([port[0], port[1]]);
            (port != 0 && is_public_ip(ip)).then_some(SocketAddr::new(ip, port))
        })
        .take(MAX_PEERS)
        .collect();
    Ok(Announced {
        peers,
        interval: Duration::from_secs(u64::from(interval)).clamp(MIN_INTERVAL, MAX_INTERVAL),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(action: u32, tid: u32, rest: &[u8]) -> Vec<u8> {
        let mut r = action.to_be_bytes().to_vec();
        r.extend_from_slice(&tid.to_be_bytes());
        r.extend_from_slice(rest);
        r
    }

    #[tokio::test]
    async fn announces_through_proxy_dns_and_respects_tracker_disable() {
        use torrentplayer_net::{
            NetworkConfig, NetworkMode, NetworkOptions, Socks5Config, socks5::Target,
            test_util::TestUdpProxy,
        };
        let proxy = TestUdpProxy::start(|target, bytes| {
            let source = match target {
                Target::Addr(addr) => *addr,
                Target::Domain(_, port) => SocketAddr::from(([8, 8, 8, 8], *port)),
            };
            let tid = u32_at(bytes, 12)?;
            let answer = match u32_at(bytes, 8)? {
                ACTION_CONNECT => reply(ACTION_CONNECT, tid, &42_u64.to_be_bytes()),
                ACTION_ANNOUNCE => {
                    let mut rest = 60u32.to_be_bytes().to_vec();
                    rest.extend([0; 8]);
                    rest.extend([9, 9, 9, 9, 0x1A, 0xE1]);
                    reply(ACTION_ANNOUNCE, tid, &rest)
                }
                _ => return None,
            };
            Some((source, answer))
        })
        .await
        .unwrap();
        let mut cfg = NetworkConfig {
            mode: NetworkMode::Socks5(Socks5Config {
                host: proxy.addr().ip().to_string(),
                port: proxy.addr().port(),
                username: None,
                password: None,
                remote_dns: true,
                proxy_trackers: true,
                proxy_dht: true,
            }),
            allow_ipv6: false,
            hold: None,
        };
        let net = Network::start(cfg.clone(), NetworkOptions::default())
            .await
            .unwrap();
        net.settled(Duration::from_secs(3)).await;
        let url = Url::parse("udp://tracker.invalid:6969/announce").unwrap();
        let request = AnnounceRequest {
            info_hash: [1; 20],
            peer_id: [2; 20],
            left: 1,
            port: 6881,
            started: true,
        };
        let announced = announce(Some(&net), &url, &request).await.unwrap();
        assert_eq!(announced.peers, vec!["9.9.9.9:6881".parse().unwrap()]);
        assert!(
            matches!(&proxy.targets()[0], Target::Domain(host, 6969) if host == "tracker.invalid")
        );
        assert!(
            matches!(&proxy.targets()[1], Target::Addr(addr) if addr.to_string() == "8.8.8.8:6969")
        );
        if let NetworkMode::Socks5(proxy) = &mut cfg.mode {
            proxy.proxy_trackers = false;
        }
        net.apply(cfg).unwrap();
        net.settled(Duration::from_secs(3)).await;
        assert!(announce(Some(&net), &url, &request).await.is_err());
        assert_eq!(
            proxy.targets().len(),
            2,
            "no disabled tracker request was sent"
        );
        net.shutdown();
    }

    #[test]
    fn connect_round_trip() {
        let p = connect_request(7);
        assert_eq!(&p[..8], &PROTOCOL_ID.to_be_bytes());
        assert_eq!(u32_at(&p, 8), Some(ACTION_CONNECT));
        assert_eq!(u32_at(&p, 12), Some(7));
        let ok = reply(ACTION_CONNECT, 7, &0xDEAD_BEEF_u64.to_be_bytes());
        assert_eq!(parse_connect(&ok, 7), Ok(0xDEAD_BEEF));
        assert!(parse_connect(&ok, 8).is_err(), "wrong transaction id");
        assert!(parse_connect(&ok[..12], 7).is_err(), "truncated");
    }

    #[test]
    fn announce_request_layout() {
        let r = AnnounceRequest {
            info_hash: [1; 20],
            peer_id: [2; 20],
            left: 1000,
            port: 6881,
            started: true,
        };
        let p = announce_request(5, 9, 3, &r);
        assert_eq!(p.len(), 98);
        assert_eq!(u32_at(&p, 8), Some(ACTION_ANNOUNCE));
        assert_eq!(&p[16..36], &[1; 20]);
        assert_eq!(u32_at(&p, 80), Some(2), "started event");
        assert_eq!(&p[96..], &6881u16.to_be_bytes());
    }

    #[test]
    fn announce_keeps_only_public_peers_and_clamps_the_interval() {
        let mut rest = 5u32.to_be_bytes().to_vec(); // interval 5 s
        rest.extend_from_slice(&[0; 8]); // leechers, seeders
        rest.extend_from_slice(&[8, 8, 8, 8, 0x1A, 0xE1]); // 8.8.8.8:6881
        rest.extend_from_slice(&[192, 168, 1, 5, 0x1A, 0xE1]); // LAN: refused
        rest.extend_from_slice(&[1, 1, 1, 1, 0, 0]); // port 0: refused
        rest.extend_from_slice(&[9, 9]); // a trailing partial entry is ignored
        let a = parse_announce(&reply(ACTION_ANNOUNCE, 4, &rest), 4, false).unwrap();
        assert_eq!(a.peers, vec!["8.8.8.8:6881".parse().unwrap()]);
        assert_eq!(a.interval, MIN_INTERVAL);
    }

    #[test]
    fn tracker_errors_are_reported_and_sanitized() {
        let err = parse_announce(
            &reply(ACTION_ERROR, 4, b"torrent not\nregistered\0"),
            4,
            false,
        )
        .unwrap_err();
        assert_eq!(err, "the tracker refused: torrent notregistered");
        let long = vec![b'x'; 5000];
        let err = parse_connect(&reply(ACTION_ERROR, 1, &long), 1).unwrap_err();
        assert!(err.len() < 250);
    }

    #[test]
    fn rejects_malformed_answers() {
        assert!(parse_announce(&[], 1, false).is_err());
        assert!(parse_announce(&reply(ACTION_CONNECT, 1, &[0; 12]), 1, false).is_err());
        assert!(parse_announce(&reply(ACTION_ANNOUNCE, 1, &[0; 4]), 1, false).is_err());
    }

    #[tokio::test]
    async fn enough_peers_start_bootstrap_without_waiting_for_a_silent_tracker() {
        let expected: Vec<SocketAddr> = (1..=20)
            .map(|port| SocketAddr::new("8.8.4.4".parse().unwrap(), port))
            .collect();
        let peers = expected.clone();
        let answers = futures_util::stream::once(async move {
            Ok(Announced {
                peers,
                interval: MIN_INTERVAL,
            })
        })
        .chain(futures_util::stream::pending())
        .boxed();
        let found = tokio::time::timeout(
            Duration::from_secs(1),
            collect_peers(answers, Duration::from_secs(60)),
        )
        .await
        .unwrap();
        assert_eq!(found, expected);
    }

    #[tokio::test]
    async fn a_silent_tracker_does_not_discard_peers_from_other_trackers() {
        let expected: SocketAddr = "8.8.4.4:6882".parse().unwrap();
        let answers = futures_util::stream::iter([true, false])
            .then(|ready| async move {
                if ready {
                    Ok(Announced {
                        peers: vec![expected],
                        interval: MIN_INTERVAL,
                    })
                } else {
                    std::future::pending().await
                }
            })
            .boxed();
        assert_eq!(
            collect_peers(answers, Duration::from_millis(30)).await,
            vec![expected]
        );
    }

    #[tokio::test]
    async fn announces_to_a_local_tracker() {
        // A fake tracker on loopback; loopback is refused for real announces, so call the
        // address-level function directly.
        let tracker = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = tracker.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut buf = [0u8; 200];
            let (n, from) = tracker.recv_from(&mut buf).await.unwrap();
            assert_eq!(n, 16);
            let tid = u32_at(&buf, 12).unwrap();
            tracker
                .send_to(&reply(ACTION_CONNECT, tid, &42u64.to_be_bytes()), from)
                .await
                .unwrap();
            let (n, from) = tracker.recv_from(&mut buf).await.unwrap();
            assert_eq!(n, 98);
            assert_eq!(&buf[..8], &42u64.to_be_bytes());
            let tid = u32_at(&buf, 12).unwrap();
            let mut rest = 1800u32.to_be_bytes().to_vec();
            rest.extend_from_slice(&[0; 8]);
            rest.extend_from_slice(&[8, 8, 4, 4, 0x1A, 0xE2]);
            tracker
                .send_to(&reply(ACTION_ANNOUNCE, tid, &rest), from)
                .await
                .unwrap();
        });
        let request = AnnounceRequest {
            info_hash: [3; 20],
            peer_id: [4; 20],
            left: 1,
            port: 6881,
            started: true,
        };
        let announced = announce_to(None, addr, &request).await.unwrap();
        assert_eq!(announced.peers, vec!["8.8.4.4:6882".parse().unwrap()]);
        assert_eq!(announced.interval, Duration::from_secs(1800));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn refuses_trackers_on_the_local_network() {
        let request = AnnounceRequest {
            info_hash: [0; 20],
            peer_id: [0; 20],
            left: 0,
            port: 1,
            started: false,
        };
        for url in ["udp://127.0.0.1:6969/announce", "udp://192.168.1.1:80"] {
            let err = announce(None, &Url::parse(url).unwrap(), &request)
                .await
                .unwrap_err();
            assert!(err.contains("public address"), "{url}: {err}");
        }
        let http = Url::parse("http://example.org/announce").unwrap();
        assert!(announce(None, &http, &request).await.is_err());
    }
}
