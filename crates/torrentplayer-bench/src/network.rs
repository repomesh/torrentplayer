// SPDX-License-Identifier: MIT OR Apache-2.0

//! One protected network (WireGuard tunnel or SOCKS5 proxy) for a whole benchmark, as the app
//! keeps one: every engine, run, and suite entry shares it.
//!
//! Starting a tunnel per run reconnected to the VPN server every few seconds with the same key,
//! and after a few such reconnects the server stopped completing handshakes. The network
//! manager reconnects by itself when the mode stops working, so between runs the benchmark only
//! waits (bounded) for traffic to be allowed again, and reports a run that cannot start as
//! "tunnel down".

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::bail;
use torrentplayer_net::{Network, NetworkConfig, NetworkMode, NetworkOptions, NetworkStatus};

/// How long the protected mode may take to come up at first.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a run waits for a protected mode that went down to come back.
pub const RECOVER_TIMEOUT: Duration = Duration::from_secs(90);

/// Network managers started by [`SharedNetwork::start`] in this process (for tests).
static STARTS: AtomicUsize = AtomicUsize::new(0);

/// A started protected network mode, shared by cloning (clones share the network).
#[derive(Clone)]
pub struct SharedNetwork {
    inner: Arc<Inner>,
}

struct Inner {
    network: Network,
}

impl std::fmt::Debug for SharedNetwork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedNetwork")
            .field("status", &self.inner.network.status().state)
            .finish()
    }
}

impl SharedNetwork {
    /// Starts the network manager in `mode`, as the app does (verified, with its kill switch),
    /// and waits up to [`CONNECT_TIMEOUT`] for it to let traffic through.
    ///
    /// # Errors
    ///
    /// Fails if the manager cannot start or the tunnel or proxy does not come up.
    pub async fn start(mode: &NetworkMode) -> anyhow::Result<Self> {
        let n = STARTS.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::info!("starting the protected network (start {n} in this process)");
        let config = NetworkConfig {
            mode: mode.clone(),
            ..NetworkConfig::default()
        };
        let network = Network::start(config, NetworkOptions::default()).await?;
        let status = network.settled(CONNECT_TIMEOUT).await;
        if !status.traffic_allowed {
            network.shutdown();
            bail!("{}; check the configuration", down(&status));
        }
        Ok(Self {
            inner: Arc::new(Inner { network }),
        })
    }

    /// Shares a network manager started elsewhere (tests).
    #[cfg(test)]
    pub(crate) fn wrap(network: Network) -> Self {
        Self {
            inner: Arc::new(Inner { network }),
        }
    }

    /// The network manager, for an engine's configuration.
    #[must_use]
    pub fn network(&self) -> &Network {
        &self.inner.network
    }

    /// Waits up to `timeout` until the mode lets traffic through, as it does again by itself
    /// after a dropped tunnel reconnects.
    ///
    /// # Errors
    ///
    /// "tunnel down", with the mode's state, if it is still blocked after `timeout`.
    pub async fn ready(&self, timeout: Duration) -> anyhow::Result<()> {
        let network = &self.inner.network;
        let mut changes = network.subscribe();
        let up = tokio::time::timeout(timeout, async {
            loop {
                if network.policy().traffic_allowed {
                    return true;
                }
                if changes.changed().await.is_err() {
                    return false;
                }
            }
        })
        .await;
        if up == Ok(true) {
            return Ok(());
        }
        bail!(
            "tunnel down: {} after waiting {} s",
            down(&network.status()),
            timeout.as_secs()
        )
    }

    /// Stops the network manager; every clone stops with it.
    pub fn shutdown(&self) {
        self.inner.network.shutdown();
    }

    /// Network managers started by [`SharedNetwork::start`] so far in this process.
    #[must_use]
    pub fn starts() -> usize {
        STARTS.load(Ordering::Relaxed)
    }
}

/// The reason a mode blocks traffic, from its status.
fn down(status: &NetworkStatus) -> String {
    format!(
        "the {:?} connection is not up ({:?})",
        status.mode, status.state
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn one_network_serves_every_run() {
        let before = SharedNetwork::starts();
        let shared = SharedNetwork::start(&NetworkMode::Direct).await.unwrap();
        for _ in 0..5 {
            let clone = shared.clone();
            clone.ready(Duration::from_secs(1)).await.unwrap();
            assert!(clone.network().policy().traffic_allowed);
        }
        assert_eq!(SharedNetwork::starts() - before, 1);
        shared.shutdown();
        let error = shared
            .ready(Duration::from_millis(50))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("tunnel down"), "{error}");
    }
}
