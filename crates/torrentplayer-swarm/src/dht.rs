// SPDX-License-Identifier: MIT OR Apache-2.0

//! Peer discovery through the BitTorrent DHT (BEP 5), using librqbit's DHT node.
//!
//! Many older course torrents list trackers that no longer answer; the DHT still knows their
//! peers. With a network manager its UDP sockets and bootstrap names use the selected network
//! lease. The node stops on every epoch change, when DHT is disabled, or when the kill switch
//! engages, and starts again once allowed. It never falls back to unbound sockets.
//! TorrentPlayer does not announce itself (it accepts no connections), and only public addresses from
//! the DHT are used: anyone can put an address there.

use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use std::time::Duration;

use futures_util::StreamExt as _;
use librqbit_dht::{Dht, DhtBuilder, DhtConfig, Id20};
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;
use torrentplayer_net::Policy;
use tracing::debug;

use crate::torrent::Shared;

/// Wait before trying again when the node cannot start (for example, no UDP socket).
const RESTART_AFTER: Duration = Duration::from_secs(30);

/// The engine's DHT node, started on first use.
pub(crate) struct DhtNode {
    running: Mutex<Option<(Dht, CancellationToken, u64)>>,
    network: Option<torrentplayer_net::Network>,
    /// The engine's token: the node stops with the engine.
    cancel: CancellationToken,
}

// librqbit's DHT state is not `Debug`.
impl std::fmt::Debug for DhtNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DhtNode").finish_non_exhaustive()
    }
}

impl DhtNode {
    pub(crate) fn new(
        cancel: CancellationToken,
        network: Option<torrentplayer_net::Network>,
    ) -> Self {
        Self {
            running: Mutex::new(None),
            network,
            cancel,
        }
    }

    /// The running node, starting it if needed. `None` if it cannot start, or if `policy` does
    /// not allow it: checked under the same lock as [`DhtNode::stop`], so a mode change that
    /// stops the node cannot be overtaken by a start that checked the policy just before it.
    async fn get(&self, policy: Option<&watch::Receiver<Policy>>) -> Option<Dht> {
        let mut running = self.running.lock().await;
        if !allowed(policy) || self.cancel.is_cancelled() {
            return None;
        }
        if self.network.is_none() && policy.is_some_and(|p| p.borrow().protected) {
            return None;
        }
        let epoch = policy.map_or(0, |p| p.borrow().epoch);
        if let Some((dht, token, started)) = running.as_ref()
            && !token.is_cancelled()
            && *started == epoch
        {
            return Some(Arc::clone(dht));
        }
        if let Some((_, token, _)) = running.take() {
            token.cancel();
        }
        let transport = if let Some(network) = &self.network {
            match crate::dht_transport::ScopedDht::open(network.clone()).await {
                Ok(transport) => Some(transport),
                Err(error) => {
                    debug!(%error, "could not open scoped DHT sockets");
                    return None;
                }
            }
        } else {
            None
        };
        if !allowed(policy) || policy.is_some_and(|p| p.borrow().epoch != epoch) {
            return None;
        }
        let token = self.cancel.child_token();
        let config = DhtConfig {
            transport,
            cancellation_token: Some(token.clone()),
            ..DhtConfig::default()
        };
        match DhtBuilder::with_config(config).await {
            Ok(dht) => {
                debug!("DHT started");
                *running = Some((Arc::clone(&dht), token, epoch));
                Some(dht)
            }
            Err(error) => {
                debug!(%error, "could not start the DHT");
                None
            }
        }
    }

    /// Shuts the node down (it starts again on next use).
    pub(crate) async fn stop(&self) {
        if let Some((_, token, _)) = self.running.lock().await.take() {
            token.cancel();
            debug!("DHT stopped");
        }
    }
}

/// Whether the policy lets the DHT run. Without a network manager, it may.
pub(crate) fn allowed(policy: Option<&watch::Receiver<Policy>>) -> bool {
    policy.is_none_or(|rx| {
        let p = *rx.borrow();
        p.traffic_allowed && p.bound_udp_allowed && p.dht_allowed
    })
}

/// Waits for the next policy change; `false` if the network manager is gone. Never returns
/// without a network manager.
async fn changed(policy: &mut Option<watch::Receiver<Policy>>) -> bool {
    match policy.as_mut() {
        Some(rx) => rx.changed().await.is_ok(),
        None => std::future::pending().await,
    }
}

/// Stops the node whenever the policy stops allowing it. Runs until the engine stops.
pub(crate) async fn follow_policy(node: Arc<DhtNode>, mut policy: watch::Receiver<Policy>) {
    let mut epoch = policy.borrow().epoch;
    loop {
        tokio::select! {
            () = node.cancel.cancelled() => return,
            changed = policy.changed() => {
                if changed.is_err() {
                    node.stop().await;
                    return;
                }
            }
        }
        let next = policy.borrow().epoch;
        if !allowed(Some(&policy)) || epoch != next {
            node.stop().await;
        }
        epoch = next;
    }
}

/// Finds peers for a torrent through the DHT while the policy allows it, until `cancel` fires
/// or the torrent is gone.
pub(crate) async fn discover(
    node: Arc<DhtNode>,
    shared: Weak<Shared>,
    info_hash: [u8; 20],
    mut policy: Option<watch::Receiver<Policy>>,
    cancel: CancellationToken,
) {
    loop {
        while !allowed(policy.as_ref()) {
            tokio::select! {
                () = cancel.cancelled() => return,
                ok = changed(&mut policy) => if !ok { return },
            }
        }
        let Some(dht) = node.get(policy.as_ref()).await else {
            if !allowed(policy.as_ref()) {
                // The policy changed since the check above: wait for it again.
                continue;
            }
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(RESTART_AFTER) => continue,
            }
        };
        // No announce: TorrentPlayer accepts no connections.
        let mut peers = dht.get_peers(Id20::new(info_hash), None);
        let lookup_epoch = policy.as_ref().map_or(0, |p| p.borrow().epoch);
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = dht.cancellation_token().cancelled() => {
                    tokio::select! {
                        () = cancel.cancelled() => return,
                        () = tokio::time::sleep(RESTART_AFTER) => {},
                    }
                    break;
                },
                addr = peers.next() => {
                    let Some(addr) = addr else {
                        // The node was stopped.
                        break;
                    };
                    let Some(shared) = shared.upgrade() else {
                        return;
                    };
                    if usable(addr) {
                        shared.add_candidates([addr]);
                    }
                }
                ok = changed(&mut policy) => {
                    if !ok {
                        return;
                    }
                    if !allowed(policy.as_ref()) || policy.as_ref().is_some_and(|p| p.borrow().epoch != lookup_epoch) {
                        break;
                    }
                }
            }
        }
    }
}

/// Only public addresses from the DHT: anyone can store an address there.
fn usable(addr: SocketAddr) -> bool {
    addr.port() != 0 && torrentplayer_net::is_public_ip(addr.ip())
}

/// Finds a bounded set of DHT peers through the selected network for magnet metadata.
/// The temporary node is cancelled on return or when the caller is cancelled.
pub async fn bootstrap_peers(
    network: torrentplayer_net::Network,
    info_hash: [u8; 20],
    deadline: Duration,
) -> Vec<SocketAddr> {
    let cancel = CancellationToken::new();
    let _stop = cancel.clone().drop_guard();
    let policy = network.subscribe();
    let node = DhtNode::new(cancel, Some(network));
    let mut peers = Vec::new();
    let _ = tokio::time::timeout(deadline, async {
        let Some(dht) = node.get(Some(&policy)).await else {
            return;
        };
        let mut stream = dht.get_peers(Id20::new(info_hash), None);
        loop {
            tokio::select! {
                () = dht.cancellation_token().cancelled() => break,
                peer = stream.next() => match peer {
                    Some(peer) if usable(peer) && !peers.contains(&peer) => {
                        peers.push(peer); if peers.len() >= 20 { break; }
                    },
                    Some(_) => {},
                    None => break,
                },
            }
        }
    })
    .await;
    peers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(traffic: bool, udp: bool) -> Policy {
        Policy {
            epoch: 1,
            traffic_allowed: traffic,
            protected: !udp,
            udp_allowed: udp,
            bound_udp_allowed: udp,
            dht_allowed: true,
            trackers_allowed: true,
        }
    }

    #[test]
    fn runs_only_while_udp_is_allowed() {
        assert!(allowed(None), "no network manager: direct");
        let (tx, rx) = watch::channel(policy(true, true));
        assert!(allowed(Some(&rx)));
        tx.send_replace(policy(true, false));
        assert!(!allowed(Some(&rx)), "a protected mode");
        tx.send_replace(policy(false, true));
        assert!(!allowed(Some(&rx)), "the kill switch");
        let mut disabled = policy(true, true);
        disabled.dht_allowed = false;
        tx.send_replace(disabled);
        assert!(!allowed(Some(&rx)), "the proxy DHT switch");
    }

    #[tokio::test]
    async fn never_starts_while_the_policy_disallows_it() {
        // A mode change that lands between `discover`'s policy check and the start must not
        // leave a node running: the start checks the policy again under the lock.
        let node = DhtNode::new(CancellationToken::new(), None);
        let (tx, rx) = watch::channel(policy(true, false));
        assert!(node.get(Some(&rx)).await.is_none(), "a protected mode");
        tx.send_replace(policy(false, true));
        assert!(node.get(Some(&rx)).await.is_none(), "the kill switch");
        let mut disabled = policy(true, true);
        disabled.dht_allowed = false;
        tx.send_replace(disabled);
        assert!(node.get(Some(&rx)).await.is_none(), "DHT disabled");
        let mut protected = policy(true, true);
        protected.protected = true;
        tx.send_replace(protected);
        assert!(
            node.get(Some(&rx)).await.is_none(),
            "no scoped network transport"
        );
        assert!(node.running.lock().await.is_none(), "nothing was started");
    }

    #[test]
    fn uses_only_public_addresses() {
        assert!(usable("8.8.8.8:6881".parse().unwrap()));
        assert!(!usable("8.8.8.8:0".parse().unwrap()));
        assert!(!usable("192.168.0.2:6881".parse().unwrap()));
        assert!(!usable("127.0.0.1:6881".parse().unwrap()));
        assert!(!usable("[fe80::1]:6881".parse().unwrap()));
    }

    #[tokio::test]
    async fn protected_dht_bootstraps_and_finds_peers_through_a_proxy_without_local_dns() {
        use torrentplayer_net::{
            Network, NetworkConfig, NetworkMode, NetworkOptions, Socks5Config, socks5::Target,
            test_util::TestUdpProxy,
        };
        let proxy = TestUdpProxy::start(|target, bytes| {
            let request = crate::bencode::decode(bytes).ok()?;
            let transaction = request.get(b"t")?.as_bytes()?;
            let query = request.get(b"q")?.as_bytes()?;
            let source = match target {
                Target::Addr(addr) => *addr,
                Target::Domain(_, port) => SocketAddr::from(([8, 8, 8, 8], *port)),
            };
            let mut reply = b"d1:rd2:id20:ABCDEFGHIJKLMNOPQRST".to_vec();
            if query == b"get_peers" {
                reply.extend(b"5:token4:test6:valuesl6:");
                reply.extend([9, 9, 9, 9, 0x1A, 0xE1]);
                reply.extend(b"e");
            } else if query == b"find_node" {
                reply.extend(b"5:nodes26:ABCDEFGHIJKLMNOPQRST");
                reply.extend([8, 8, 8, 8]);
                reply.extend(source.port().to_be_bytes());
            }
            reply.extend(format!("e1:t{}:", transaction.len()).bytes());
            reply.extend(transaction);
            reply.extend(b"1:y1:re");
            Some((source, reply))
        })
        .await
        .unwrap();
        let net = Network::start(
            NetworkConfig {
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
            },
            NetworkOptions::default(),
        )
        .await
        .unwrap();
        assert!(net.settled(Duration::from_secs(3)).await.traffic_allowed);
        let peers = bootstrap_peers(net.clone(), [5; 20], Duration::from_secs(3)).await;
        assert!(
            peers.contains(&"9.9.9.9:6881".parse().unwrap()),
            "{peers:?}"
        );
        assert!(
            proxy
                .targets()
                .iter()
                .any(|target| matches!(target, Target::Domain(_, _))),
            "bootstrap names went to the proxy"
        );
        let transport = crate::dht_transport::ScopedDht::open(net.clone())
            .await
            .unwrap();
        assert!(
            transport
                .send_to(b"blocked", "127.0.0.1:1".parse().unwrap())
                .await
                .is_err()
        );
        let node = DhtNode::new(CancellationToken::new(), Some(net.clone()));
        let policy = net.subscribe();
        let dht = node.get(Some(&policy)).await.unwrap();
        proxy.stop();
        tokio::time::timeout(Duration::from_secs(3), dht.cancellation_token().cancelled())
            .await
            .unwrap();
        assert!(
            node.running.lock().await.as_ref().unwrap().1.is_cancelled(),
            "worker exit marks the cached node dead so the next lookup can restart it"
        );
        net.shutdown();
        assert!(
            transport
                .send_to(b"blocked", "8.8.8.8:6881".parse().unwrap())
                .await
                .is_err()
        );
    }
}
