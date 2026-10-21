// SPDX-License-Identifier: MIT OR Apache-2.0

//! Incoming peer connections. Off by default, so TorrentPlayer opens no listening socket (and
//! causes no firewall prompt) unless the user turns it on.
//!
//! When it is on, [`Network`] listens on one TCP port, but only on the path the selected mode
//! allows:
//!
//! - **Direct:** every interface (IPv4 and IPv6), optionally with a UPnP port mapping on the
//!   router.
//! - **Bound to an interface:** only that interface's addresses, with the socket pinned to the
//!   interface as outgoing connections are (see [`crate::bind`]).
//! - **WireGuard:** only the tunnel's own addresses, inside TorrentPlayer's tunnel. Peers reach
//!   them only if the VPN server forwards the port.
//! - **SOCKS5:** never. A proxy cannot accept arbitrary incoming connections for a client, and
//!   listening anywhere else would bypass it.
//!
//! The listener follows the kill switch: it is closed while traffic is blocked, and reopened
//! for each new epoch. Every accepted connection carries a [`Lease`] for its epoch, exactly like
//! outgoing ones, so it closes when the mode changes or the kill switch engages. Accepted
//! connections go to a single consumer, the torrent engine ([`Network::incoming_peers`]).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Weak};
use std::time::Duration;

use serde::Serialize;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::sync::{CancellationToken, DropGuard};
use ts_rs::TS;

use crate::bind;
use crate::error::NetError;
use crate::mode::NetworkMode;
use crate::network::{EventLevel, Inner, Lease, Network, Route, State};
use crate::tunnel::Tunnel;

/// Ports chosen when the setting asks for a random one: registered ports below the dynamic
/// range that Windows and macOS use for outgoing connections (49152 and up).
pub const RANDOM_PORTS: (u16, u16) = (10_000, 49_151);
/// Other random ports tried when the chosen one is in use.
const RANDOM_ATTEMPTS: usize = 8;
/// Accepted connections waiting for the engine; more are closed at once.
const QUEUE: usize = 64;
/// How long to wait before trying again to open a port that could not be opened.
const RETRY_AFTER: Duration = Duration::from_secs(30);
/// Pause after a failed `accept` (for example, out of file descriptors).
const ACCEPT_BACKOFF: Duration = Duration::from_millis(200);

/// Whether and where to accept incoming peer connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IncomingConfig {
    /// Listen for incoming peer connections. Off by default.
    pub enabled: bool,
    /// TCP port. 0 picks a random port from [`RANDOM_PORTS`] once and keeps it while TorrentPlayer
    /// runs.
    pub port: u16,
    /// In direct mode, ask the router to forward the port (UPnP IGD). Never used in other modes.
    pub upnp: bool,
}

/// Whether incoming peer connections are accepted right now, for the status view.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
#[ts(export, rename = "IncomingPeersStatus")]
pub enum IncomingStatus {
    /// The setting is off: nothing listens.
    #[default]
    Off,
    /// The setting is on, but no torrent engine accepts incoming peers right now.
    Idle {
        /// Why.
        reason: String,
    },
    /// The kill switch blocks traffic, so nothing listens until the mode works again.
    Paused {
        /// Why traffic is blocked.
        reason: String,
    },
    /// The selected mode cannot accept incoming connections (SOCKS5), so nothing listens.
    Unsupported {
        /// Why.
        reason: String,
    },
    /// Listening.
    Listening {
        /// The TCP port.
        port: u16,
        /// The addresses listened on (`0.0.0.0` and `[::]` mean every interface).
        addresses: Vec<String>,
        /// Whether a UPnP port mapping is requested from the router (direct mode only).
        upnp: bool,
    },
    /// The port could not be opened (for example, another program uses it). Retried every
    /// 30 seconds.
    Failed {
        /// Why.
        reason: String,
    },
}

/// A listener that a torrent engine runs itself (librqbit's), reported with
/// [`Network::set_external_listener`] so the status shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalListener {
    /// Listening on this address.
    Listening(SocketAddr),
    /// The port could not be opened.
    Failed(String),
    /// Not listening, for this reason.
    Idle(String),
}

/// An accepted peer connection. Close it when [`Lease::revoked`] completes.
#[derive(Debug)]
pub struct IncomingPeer {
    /// The connection.
    pub stream: TcpStream,
    /// The peer's address as seen by the listener (inside the tunnel for WireGuard).
    pub addr: SocketAddr,
    /// The epoch it was accepted in.
    pub lease: Lease,
}

/// Accepted peer connections, from [`Network::incoming_peers`].
#[derive(Debug)]
pub struct IncomingPeers {
    rx: mpsc::Receiver<IncomingPeer>,
}

impl IncomingPeers {
    /// The next accepted connection; `None` once another consumer took over.
    pub async fn next(&mut self) -> Option<IncomingPeer> {
        self.rx.recv().await
    }
}

/// The incoming-connection part of the network state.
#[derive(Debug, Default)]
pub(crate) struct IncomingState {
    config: IncomingConfig,
    /// The random port in use when `config.port` is 0, kept for the whole run.
    random_port: Option<u16>,
    consumer: Option<mpsc::Sender<IncomingPeer>>,
    /// Increases with each consumer, so a new one gets fresh listeners.
    consumer_id: u64,
    external: Option<ExternalListener>,
    pub(crate) status: IncomingStatus,
}

impl IncomingState {
    /// The port to listen on: the configured one, or the random one.
    fn port(&self) -> Option<u16> {
        match self.config.port {
            0 => self.random_port,
            port => Some(port),
        }
    }
}

/// What the listener should be doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Key {
    epoch: u64,
    port: u16,
    upnp: bool,
    consumer: u64,
}

enum Plan {
    Stop(IncomingStatus),
    Listen(Key, mpsc::Sender<IncomingPeer>),
}

fn plan(st: &State) -> Plan {
    let incoming = &st.incoming;
    if !incoming.config.enabled {
        return Plan::Stop(IncomingStatus::Off);
    }
    let direct = matches!(st.config.mode, NetworkMode::Direct);
    if matches!(st.config.mode, NetworkMode::Socks5(_)) {
        return Plan::Stop(IncomingStatus::Unsupported {
            reason: "a SOCKS5 proxy cannot accept incoming connections, so TorrentPlayer does \
                     not listen in this mode"
                .into(),
        });
    }
    let Some(tx) = incoming.consumer.clone().filter(|tx| !tx.is_closed()) else {
        return Plan::Stop(match &incoming.external {
            Some(ExternalListener::Listening(addr)) => IncomingStatus::Listening {
                port: addr.port(),
                addresses: vec![addr.to_string()],
                upnp: incoming.config.upnp && direct,
            },
            Some(ExternalListener::Failed(reason)) => IncomingStatus::Failed {
                reason: reason.clone(),
            },
            Some(ExternalListener::Idle(reason)) => IncomingStatus::Idle {
                reason: reason.clone(),
            },
            None => IncomingStatus::Idle {
                reason: "the torrent engine is not accepting incoming peers".into(),
            },
        });
    };
    if !st.conn.allows_traffic() {
        return Plan::Stop(IncomingStatus::Paused {
            reason: st.blocked_reason(),
        });
    }
    let Some(port) = incoming.port() else {
        return Plan::Stop(IncomingStatus::Failed {
            reason: "no port was chosen".into(),
        });
    };
    Plan::Listen(
        Key {
            epoch: st.epoch,
            port,
            upnp: incoming.config.upnp && direct,
            consumer: incoming.consumer_id,
        },
        tx,
    )
}

/// A random port from [`RANDOM_PORTS`].
fn random_port() -> Result<u16, NetError> {
    let mut bytes = [0; 2];
    getrandom::fill(&mut bytes)
        .map_err(|e| NetError::Io(std::io::Error::other(format!("no randomness: {e}"))))?;
    let span = RANDOM_PORTS.1 - RANDOM_PORTS.0 + 1;
    Ok(RANDOM_PORTS.0 + u16::from_le_bytes(bytes) % span)
}

impl Network {
    /// Turns incoming peer connections on or off, or changes their port. Existing connections
    /// stay open; the listener is reopened on the new port.
    pub fn set_incoming(&self, config: IncomingConfig) {
        {
            let mut st = self.inner.state();
            if st.incoming.config == config {
                return;
            }
            if config.port == 0 && st.incoming.random_port.is_none() {
                st.incoming.random_port = random_port().ok();
            }
            st.incoming.config = config;
            let message = if config.enabled {
                format!(
                    "Incoming peer connections on, port {}",
                    st.incoming
                        .port()
                        .map_or_else(|| "random".to_owned(), |p| p.to_string())
                )
            } else {
                "Incoming peer connections off".to_owned()
            };
            st.log(EventLevel::Info, message);
        }
        self.inner.incoming_wake.notify_one();
    }

    /// The incoming-connection settings.
    #[must_use]
    pub fn incoming_config(&self) -> IncomingConfig {
        self.inner.state().incoming.config
    }

    /// The port to announce to trackers: the listening port while incoming connections are on
    /// and the mode can accept them (any mode but SOCKS5), whether or not the listener is open
    /// at this moment.
    #[must_use]
    pub fn incoming_port(&self) -> Option<u16> {
        let st = self.inner.state();
        if !st.incoming.config.enabled || matches!(st.config.mode, NetworkMode::Socks5(_)) {
            return None;
        }
        st.incoming.port()
    }

    /// Takes the accepted incoming connections. There is one consumer at a time: a new call
    /// ends the previous one. The listener is open only while a consumer exists, the setting is
    /// on, and the mode allows traffic.
    #[must_use]
    pub fn incoming_peers(&self) -> IncomingPeers {
        let (tx, rx) = mpsc::channel(QUEUE);
        {
            let mut st = self.inner.state();
            st.incoming.consumer = Some(tx);
            st.incoming.consumer_id += 1;
        }
        self.inner.incoming_wake.notify_one();
        IncomingPeers { rx }
    }

    /// Reports a listener that an engine runs itself (librqbit), for the status, while no
    /// consumer takes connections from [`Network::incoming_peers`]. `None` forgets it.
    pub fn set_external_listener(&self, listener: Option<ExternalListener>) {
        self.inner.state().incoming.external = listener;
        self.inner.incoming_wake.notify_one();
    }
}

/// Opens, moves, and closes the listener as the settings, the mode, the kill switch, and the
/// consumer change, for as long as the network runs.
pub(crate) async fn serve(inner: Weak<Inner>, cancel: CancellationToken) {
    let Some((mut policy, wake)) = inner
        .upgrade()
        .map(|net| (net.policy.subscribe(), Arc::clone(&net.incoming_wake)))
    else {
        return;
    };
    let mut active: Option<(Key, DropGuard)> = None;
    loop {
        let Some(net) = inner.upgrade() else {
            return;
        };
        let plan = plan(&net.state());
        let (consumer, retry) = match plan {
            Plan::Stop(status) => {
                active = None;
                set_status(&net, status);
                (None, false)
            }
            Plan::Listen(key, tx) => {
                let mut failed = false;
                if active.as_ref().is_none_or(|(current, _)| *current != key) {
                    // Close the old sockets before opening the new ones on the same port.
                    active = None;
                    match open(&net, key, &tx).await {
                        Ok((port, addresses, guard)) => {
                            active = Some((Key { port, ..key }, guard));
                            set_status(
                                &net,
                                IncomingStatus::Listening {
                                    port,
                                    addresses,
                                    upnp: key.upnp,
                                },
                            );
                        }
                        Err(error) => {
                            failed = true;
                            set_status(
                                &net,
                                IncomingStatus::Failed {
                                    reason: error.to_string(),
                                },
                            );
                        }
                    }
                }
                (Some(tx), failed)
            }
        };
        drop(net);
        tokio::select! {
            () = cancel.cancelled() => return,
            changed = policy.changed() => if changed.is_err() { return },
            () = wake.notified() => {}
            () = async {
                match &consumer {
                    Some(tx) => tx.closed().await,
                    None => std::future::pending().await,
                }
            } => {}
            () = async {
                if retry {
                    tokio::time::sleep(RETRY_AFTER).await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                // Forget the failed attempt so it is made again.
                active = None;
            }
        }
    }
}

fn set_status(net: &Inner, status: IncomingStatus) {
    let mut st = net.state();
    if st.incoming.status == status {
        return;
    }
    match &status {
        IncomingStatus::Listening {
            port,
            addresses,
            upnp,
        } => {
            let upnp = if *upnp { ", UPnP requested" } else { "" };
            st.log(
                EventLevel::Info,
                format!(
                    "Accepting incoming peers on port {port} ({}{upnp})",
                    addresses.join(", ")
                ),
            );
        }
        IncomingStatus::Failed { reason } => {
            st.log(
                EventLevel::Warn,
                format!("Could not listen for incoming peers: {reason}"),
            );
        }
        IncomingStatus::Unsupported { reason } => {
            st.log(
                EventLevel::Info,
                format!("Not accepting incoming peers: {reason}"),
            );
        }
        // Turning the setting off is logged by `set_incoming`, and the kill switch by the
        // monitor.
        IncomingStatus::Off | IncomingStatus::Idle { .. } | IncomingStatus::Paused { .. } => {}
    }
    st.incoming.status = status;
    drop(st);
    net.status_changed();
}

/// Sockets opened for one route.
enum Bound {
    Os(Vec<(TcpListener, SocketAddr)>),
    Tunnel(Vec<(tokio_wireguard::TcpListener, SocketAddr)>, Tunnel),
}

/// Opens the listening sockets for `key` and starts accepting. Returns the port (a random one
/// may have been replaced), the addresses, and a guard that closes everything when dropped.
async fn open(
    net: &Arc<Inner>,
    key: Key,
    tx: &mpsc::Sender<IncomingPeer>,
) -> Result<(u16, Vec<String>, DropGuard), NetError> {
    let network = Network {
        inner: Arc::clone(net),
    };
    let (route, lease, ipv4_only) = network.route()?;
    let random = net.state().incoming.config.port == 0;
    let mut port = key.port;
    let mut attempts = 0;
    let bound = loop {
        match bind_route(&route, port, ipv4_only).await {
            Ok(bound) => break bound,
            Err(NetError::Io(error))
                if random
                    && attempts < RANDOM_ATTEMPTS
                    && matches!(
                        error.kind(),
                        std::io::ErrorKind::AddrInUse | std::io::ErrorKind::AddrNotAvailable
                    ) =>
            {
                attempts += 1;
                port = random_port()?;
                net.state().incoming.random_port = Some(port);
            }
            Err(error) => return Err(error),
        }
    };
    if !lease.is_valid() {
        return Err(NetError::KillSwitch(
            "the network settings changed while opening the port".into(),
        ));
    }
    let cancel = CancellationToken::new();
    let mut addresses = Vec::new();
    match bound {
        Bound::Os(listeners) => {
            for (listener, addr) in listeners {
                addresses.push(addr.to_string());
                tokio::spawn(accept_os(
                    listener,
                    tx.clone(),
                    lease.clone(),
                    cancel.child_token(),
                ));
            }
        }
        Bound::Tunnel(listeners, tunnel) => {
            for (listener, addr) in listeners {
                addresses.push(addr.to_string());
                tokio::spawn(accept_tunnel(
                    listener,
                    tunnel.clone(),
                    tx.clone(),
                    lease.clone(),
                    cancel.child_token(),
                ));
            }
        }
    }
    if key.upnp && matches!(route, Route::Direct) {
        tokio::spawn(forward_port(port, cancel.child_token()));
    }
    Ok((port, addresses, cancel.drop_guard()))
}

/// Opens the sockets for `route`, never anywhere else: every interface in direct mode, the
/// interface's addresses when bound, the tunnel's addresses for WireGuard.
async fn bind_route(route: &Route, port: u16, ipv4_only: bool) -> Result<Bound, NetError> {
    let os = |iface: Option<&bind::BoundInterface>, ip: IpAddr| {
        let addr = SocketAddr::new(ip, port);
        bind::listen(iface, addr)
            .and_then(|l| TcpListener::from_std(l).map_err(NetError::Io))
            .map(|l| (l, addr))
    };
    match route {
        Route::Direct => {
            let mut listeners = vec![os(None, IpAddr::V4(Ipv4Addr::UNSPECIFIED))?];
            // IPv6 is optional: not every machine has it.
            match os(None, IpAddr::V6(Ipv6Addr::UNSPECIFIED)) {
                Ok(v6) => listeners.push(v6),
                Err(error) => {
                    tracing::debug!(target: "torrentplayer_net", %error, "not listening on IPv6");
                }
            }
            Ok(Bound::Os(listeners))
        }
        Route::Bind(iface) => {
            let mut listeners = Vec::new();
            if let Some(v4) = iface.v4 {
                listeners.push(os(Some(iface), IpAddr::V4(v4))?);
            }
            if let Some(v6) = iface.v6.filter(|_| !ipv4_only) {
                listeners.push(os(Some(iface), IpAddr::V6(v6))?);
            }
            if listeners.is_empty() {
                return Err(NetError::InterfaceUnavailable(format!(
                    "{} has no address to listen on",
                    iface.name
                )));
            }
            Ok(Bound::Os(listeners))
        }
        Route::WireGuard(tunnel) => {
            let listeners = tunnel
                .listen(port, ipv4_only)
                .await?
                .into_iter()
                .map(|l| {
                    let addr = l.local_addr()?;
                    Ok((l, addr))
                })
                .collect::<Result<_, std::io::Error>>()?;
            Ok(Bound::Tunnel(listeners, tunnel.clone()))
        }
        Route::Socks(_) => Err(NetError::Unsupported(
            "a SOCKS5 proxy cannot accept incoming connections".into(),
        )),
    }
}

/// Hands an accepted connection to the consumer, if it is still allowed and there is room.
fn deliver(tx: &mpsc::Sender<IncomingPeer>, stream: TcpStream, addr: SocketAddr, lease: &Lease) {
    if !lease.is_valid() {
        return;
    }
    let _ = stream.set_nodelay(true);
    let peer = IncomingPeer {
        stream,
        addr,
        lease: lease.clone(),
    };
    if tx.try_send(peer).is_err() {
        tracing::debug!(target: "torrentplayer_net", "incoming peer dropped: the engine is busy");
    }
}

async fn accept_os(
    listener: TcpListener,
    tx: mpsc::Sender<IncomingPeer>,
    lease: Lease,
    cancel: CancellationToken,
) {
    loop {
        let accepted = tokio::select! {
            () = cancel.cancelled() => return,
            accepted = listener.accept() => accepted,
        };
        match accepted {
            Ok((stream, addr)) => deliver(&tx, stream, addr, &lease),
            Err(error) => {
                tracing::debug!(target: "torrentplayer_net", %error, "accepting a peer failed");
                tokio::time::sleep(ACCEPT_BACKOFF).await;
            }
        }
    }
}

async fn accept_tunnel(
    listener: tokio_wireguard::TcpListener,
    tunnel: Tunnel,
    tx: mpsc::Sender<IncomingPeer>,
    lease: Lease,
    cancel: CancellationToken,
) {
    loop {
        let accepted = tokio::select! {
            () = cancel.cancelled() => return,
            accepted = listener.accept() => accepted,
        };
        match accepted {
            Ok((upstream, addr)) => match tunnel.relay(upstream).await {
                Ok(stream) => deliver(&tx, stream, addr, &lease),
                Err(error) => {
                    tracing::debug!(target: "torrentplayer_net", %error, "relaying a peer failed");
                }
            },
            Err(error) => {
                tracing::debug!(target: "torrentplayer_net", %error, "accepting a peer failed");
                tokio::time::sleep(ACCEPT_BACKOFF).await;
            }
        }
    }
}

/// Keeps a UPnP mapping for `port` on the router (direct mode only) until cancelled. The
/// mapping's lease is renewed every 30 seconds and lapses within a minute after it stops.
async fn forward_port(port: u16, cancel: CancellationToken) {
    let forwarder = match librqbit_upnp::UpnpPortForwarder::new(vec![port], None, None) {
        Ok(forwarder) => forwarder,
        Err(error) => {
            tracing::warn!(target: "torrentplayer_net", error = %format!("{error:#}"), "UPnP port mapping failed");
            return;
        }
    };
    tracing::info!(target: "torrentplayer_net", port, "requesting a UPnP port mapping");
    tokio::select! {
        () = cancel.cancelled() => {}
        _ = forwarder.run_forever() => {}
    }
}
