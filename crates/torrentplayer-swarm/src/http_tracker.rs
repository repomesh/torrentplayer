// SPDX-License-Identifier: MIT OR Apache-2.0

//! One-shot HTTP tracker announces (BEP 3, compact peers per BEP 23 and BEP 7) through
//! TorrentPlayer's network manager.
//!
//! librqbit can reach HTTP trackers only through a `socks5://` proxy URL, so its HTTP client
//! resolves tracker host names with the system resolver, outside the protected mode and even
//! while the kill switch is engaged. In a protected mode these announces go through the
//! network's own HTTP client instead ([`Network::configure_http_public`]), which passes host
//! names to the loopback bridge unresolved (`socks5h`): the tunnel or proxy resolves them.
//! Tracker URLs come from untrusted magnet links, so only public hosts are contacted: a
//! `tr=http://192.168.1.1/...` must not make TorrentPlayer send requests to the user's own
//! network, or to the proxy's. Responses are untrusted too: their size is bounded, and peer
//! addresses on the user's own network are refused.

use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use futures_util::{StreamExt as _, stream::FuturesUnordered};
use torrentplayer_net::{Network, is_public_ip};
use url::Url;

use crate::bencode::{self, Value};

/// Largest response read: far more than 200 compact peers.
const MAX_RESPONSE: usize = 64 * 1024;
/// Most peers taken from one response, and in all.
const MAX_PEERS: usize = 200;
/// Enough candidates to start magnet metadata fetching without waiting for silent trackers.
const BOOTSTRAP_PEERS: usize = 20;
/// Most trackers asked at once.
const MAX_AT_ONCE: usize = 8;

/// Announces once to up to 8 of `trackers` (HTTP and HTTPS ones on public hosts only, unless
/// `allow_local`) at the same time, through `network`, and returns the peers they name,
/// without duplicates, within `deadline`.
/// For finding peers of a magnet link in a protected mode, where librqbit must not contact
/// HTTP trackers itself; failures are skipped. Nothing is sent while the network's policy
/// forbids traffic or trackers. `allow_local` also announces to trackers on loopback and
/// private addresses: only for tests and benchmarks with a local tracker.
pub async fn announce_all(
    network: &Network,
    trackers: &[Url],
    info_hash: [u8; 20],
    deadline: Duration,
    allow_local: bool,
) -> Vec<SocketAddr> {
    let policy = network.policy();
    if !policy.traffic_allowed || !policy.trackers_allowed {
        return Vec::new();
    }
    let Ok(client) = client(network, allow_local) else {
        return Vec::new();
    };
    let peer_id = crate::torrent::generate_peer_id();
    let mut announces = trackers
        .iter()
        .filter(|u| matches!(u.scheme(), "http" | "https"))
        // Literal addresses and `localhost` here; other names when the bridge resolves them.
        .filter(|u| allow_local || crate::webseed::url_allowed(u))
        .take(MAX_AT_ONCE)
        .map(|url| announce(&client, announce_url(url, &info_hash, &peer_id)))
        .collect::<FuturesUnordered<_>>();
    let mut peers: Vec<SocketAddr> = Vec::new();
    let gather = async {
        while let Some(result) = announces.next().await {
            match result {
                Ok(found) => {
                    for addr in found {
                        if !peers.contains(&addr) && peers.len() < MAX_PEERS {
                            peers.push(addr);
                        }
                    }
                    if peers.len() >= BOOTSTRAP_PEERS {
                        break;
                    }
                }
                Err(error) => tracing::debug!(%error, "HTTP tracker announce failed"),
            }
        }
    };
    // Keep answers that arrived before the deadline even if another tracker never replies.
    let _ = tokio::time::timeout(deadline, gather).await;
    peers
}

/// The client for announces: through the network's public-only bridge, host names unresolved
/// (the bridge refuses names that resolve to non-public addresses), and no redirects (a
/// tracker that moves is skipped rather than followed somewhere else). With `allow_local`,
/// through the ordinary bridge.
fn client(network: &Network, allow_local: bool) -> Result<reqwest::Client, String> {
    let builder = reqwest::Client::builder()
        .user_agent(concat!("TorrentPlayer/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none());
    let builder = if allow_local {
        network.configure_http(builder)
    } else {
        network.configure_http_public(builder)
    };
    builder
        .map_err(|e| e.to_string())?
        .build()
        .map_err(|e| e.to_string())
}

/// `url` with the announce parameters added to its query.
fn announce_url(url: &Url, info_hash: &[u8; 20], peer_id: &[u8; 20]) -> String {
    let mut out = url.as_str().to_owned();
    if let Some(fragment) = out.find('#') {
        out.truncate(fragment);
    }
    out.push(if url.query().is_some() { '&' } else { '?' });
    out.push_str("info_hash=");
    percent_encode(&mut out, info_hash);
    out.push_str("&peer_id=");
    percent_encode(&mut out, peer_id);
    // Nothing downloaded yet; `left` large, as for UDP trackers, so seeders are returned.
    let _ = write!(
        out,
        "&port=6881&uploaded=0&downloaded=0&left={}&compact=1&numwant=50&event=started",
        u64::MAX / 2
    );
    out
}

fn percent_encode(out: &mut String, bytes: &[u8]) {
    for &b in bytes {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
}

async fn announce(client: &reqwest::Client, url: String) -> Result<Vec<SocketAddr>, String> {
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    if !response.status().is_success() {
        return Err(format!("the tracker answered {}", response.status()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| e.without_url().to_string())?
    {
        if body.len() + chunk.len() > MAX_RESPONSE {
            return Err("the tracker's answer is too large".into());
        }
        body.extend_from_slice(&chunk);
    }
    parse_response(&body)
}

/// The public peers in a tracker's bencoded answer.
fn parse_response(body: &[u8]) -> Result<Vec<SocketAddr>, String> {
    let value = bencode::decode(body).map_err(|e| format!("malformed answer: {e}"))?;
    if !matches!(value, Value::Dict(_)) {
        return Err("malformed answer".into());
    }
    if let Some(reason) = value.get(b"failure reason").and_then(Value::as_bytes) {
        let reason: String = String::from_utf8_lossy(reason).chars().take(200).collect();
        return Err(format!("the tracker refused: {reason}"));
    }
    let mut peers = Vec::new();
    match value.get(b"peers") {
        Some(Value::Bytes(compact)) => {
            for chunk in compact.chunks_exact(6) {
                let ip = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
                peers.push(SocketAddr::new(
                    ip.into(),
                    u16::from_be_bytes([chunk[4], chunk[5]]),
                ));
            }
        }
        Some(Value::List(list)) => {
            for peer in list {
                let ip = peer
                    .get(b"ip")
                    .and_then(Value::as_bytes)
                    .and_then(|ip| std::str::from_utf8(ip).ok())
                    .and_then(|ip| ip.parse::<IpAddr>().ok());
                let port = match peer.get(b"port") {
                    Some(Value::Int(port)) => u16::try_from(*port).ok(),
                    _ => None,
                };
                if let (Some(ip), Some(port)) = (ip, port) {
                    peers.push(SocketAddr::new(ip, port));
                }
            }
        }
        _ => {}
    }
    if let Some(Value::Bytes(compact)) = value.get(b"peers6") {
        for chunk in compact.chunks_exact(18) {
            let mut ip = [0u8; 16];
            ip.copy_from_slice(&chunk[..16]);
            peers.push(SocketAddr::new(
                Ipv6Addr::from(ip).into(),
                u16::from_be_bytes([chunk[16], chunk[17]]),
            ));
        }
    }
    peers.retain(|p| p.port() != 0 && is_public_ip(p.ip()));
    peers.truncate(MAX_PEERS);
    Ok(peers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_announce_urls() {
        let url = Url::parse("http://tracker.example:6969/announce?key=a#frag").unwrap();
        let built = announce_url(&url, &[0x20; 20], b"-TP0100-abcdefghijkl");
        assert!(
            built.starts_with("http://tracker.example:6969/announce?key=a&info_hash=%20%20"),
            "{built}"
        );
        assert!(built.contains("&peer_id=-TP0100-abcdefghijkl&port=6881"));
        assert!(built.contains("&compact=1"));
        assert!(!built.contains('#'));
        let plain = Url::parse("https://t.example/ann").unwrap();
        assert!(
            announce_url(&plain, &[0; 20], &[0; 20])
                .starts_with("https://t.example/ann?info_hash=%00")
        );
    }

    #[test]
    fn parses_compact_and_dictionary_peers_keeping_public_ones() {
        let mut body = b"d8:intervali1800e5:peers12:".to_vec();
        body.extend_from_slice(&[1, 2, 3, 4, 0x1A, 0xE1, 192, 168, 1, 2, 0x1A, 0xE1]);
        body.extend_from_slice(b"6:peers618:");
        let mut v6 = [0u8; 18];
        v6[0] = 0x20;
        v6[1] = 0x01;
        v6[2] = 0x48;
        v6[3] = 0x60;
        v6[15] = 1;
        v6[17] = 80;
        body.extend_from_slice(&v6);
        body.push(b'e');
        let peers = parse_response(&body).unwrap();
        assert_eq!(
            peers,
            vec![
                "1.2.3.4:6881".parse().unwrap(),
                "[2001:4860::1]:80".parse().unwrap()
            ]
        );

        let list = b"d5:peersld2:ip7:8.8.8.84:porti51413eed2:ip9:127.0.0.14:porti1eeee";
        assert_eq!(
            parse_response(list).unwrap(),
            vec!["8.8.8.8:51413".parse::<SocketAddr>().unwrap()]
        );
        assert!(
            parse_response(b"d14:failure reason5:nope!e")
                .unwrap_err()
                .contains("nope!")
        );
        assert!(parse_response(b"not bencode").is_err());
    }
}
