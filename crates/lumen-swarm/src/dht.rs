// SPDX-License-Identifier: MIT OR Apache-2.0

//! Peer discovery through the BitTorrent DHT (BEP 5), using librqbit's DHT node.
//!
//! Many older course torrents list trackers that no longer answer; the DHT still knows their
//! peers. It speaks UDP, which TorrentStream only sends in direct mode, so the node runs only while the
//! network policy allows UDP. When a mode change or the kill switch stops allowing it, the node
//! is shut down (no packet goes out around a VPN or proxy), and started again once allowed.
//! TorrentStream does not announce itself (it accepts no connections), and only public addresses from
//! the DHT are used: anyone can put an address there.

use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use std::time::Duration;

use futures_util::StreamExt as _;
use librqbit_dht::{Dht, DhtBuilder, DhtConfig, Id20};
use lumen_net::Policy;
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::torrent::Shared;

/// Wait before trying again when the node cannot start (for example, no UDP socket).
const RESTART_AFTER: Duration = Duration::from_secs(30);

/// The engine's DHT node, started on first use.
pub(crate) struct DhtNode {
    running: Mutex<Option<(Dht, CancellationToken)>>,
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
    pub(crate) fn new(cancel: CancellationToken) -> Self {
        Self {
            running: Mutex::new(None),
            cancel,
        }
    }

    /// The running node, starting it if needed. `None` if it cannot start.
    async fn get(&self) -> Option<Dht> {
        let mut running = self.running.lock().await;
        if let Some((dht, token)) = running.as_ref()
            && !token.is_cancelled()
        {
            return Some(Arc::clone(dht));
        }
        let token = self.cancel.child_token();
        let config = DhtConfig {
            cancellation_token: Some(token.clone()),
            ..DhtConfig::default()
        };
        match DhtBuilder::with_config(config).await {
            Ok(dht) => {
                debug!("DHT started");
                *running = Some((Arc::clone(&dht), token));
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
        if let Some((_, token)) = self.running.lock().await.take() {
            token.cancel();
            debug!("DHT stopped");
        }
    }
}

/// Whether the policy lets the DHT run. Without a network manager, it may.
pub(crate) fn allowed(policy: Option<&watch::Receiver<Policy>>) -> bool {
    policy.is_none_or(|rx| {
        let p = *rx.borrow();
        p.traffic_allowed && p.udp_allowed
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
        if !allowed(Some(&policy)) {
            node.stop().await;
        }
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
        let Some(dht) = node.get().await else {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(RESTART_AFTER) => continue,
            }
        };
        // No announce: TorrentStream accepts no connections.
        let mut peers = dht.get_peers(Id20::new(info_hash), None);
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
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
                    if !allowed(policy.as_ref()) {
                        break;
                    }
                }
            }
        }
    }
}

/// Only public addresses from the DHT: anyone can store an address there.
fn usable(addr: SocketAddr) -> bool {
    addr.port() != 0 && lumen_net::is_public_ip(addr.ip())
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
    }

    #[test]
    fn uses_only_public_addresses() {
        assert!(usable("8.8.8.8:6881".parse().unwrap()));
        assert!(!usable("8.8.8.8:0".parse().unwrap()));
        assert!(!usable("192.168.0.2:6881".parse().unwrap()));
        assert!(!usable("127.0.0.1:6881".parse().unwrap()));
        assert!(!usable("[fe80::1]:6881".parse().unwrap()));
    }
}
