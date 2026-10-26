// SPDX-License-Identifier: MIT OR Apache-2.0

//! Incoming peer connections. Off by default, so TorrentPlayer opens no listening socket (and
//! causes no firewall prompt) unless the user turns it on.
//!
//! When it is on, [`Network`] listens on one TCP port, but only on the path the selected mode
//! allows:
//!
//! - **Direct:** every interface (IPv4 and IPv6), optionally with a UPnP port mapping on the
//!   router.
//! - **WireGuard:** only the tunnel's own addresses, inside TorrentPlayer's tunnel. Peers reach
//!   them only if the VPN server forwards the port.
//! - **SOCKS5:** never. A proxy cannot accept arbitrary incoming connections for a client, and
//!   listening anywhere else would bypass it.
//!
//! The listener follows the kill switch: it is closed while traffic is blocked, and reopened
//! for each new epoch. Every accepted connection carries a [`Lease`] for its epoch, exactly like
//! outgoing ones, so it closes when the mode changes or the kill switch engages. Accepted
//! connections go to a single consumer, the torrent engine ([`Network::incoming_peers`]).
//!
//! Listening does not mean peers can connect: a router without a port forward drops them.
//! [`Network::reachable_port`] tells only while the router confirms a UPnP mapping, or a peer
//! from the internet connected in recently (which also covers a port forwarded by hand or by a
//! VPN provider).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use serde::Serialize;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use ts_rs::TS;

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
/// How long UPnP discovery waits for routers to answer (they may wait up to 3 seconds).
const UPNP_DISCOVER_TIMEOUT: Duration = Duration::from_secs(5);
/// How often routers are looked for again while no mapping works.
const UPNP_DISCOVER_EVERY: Duration = Duration::from_secs(60);
/// Time limit for one request to the router.
const UPNP_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest reply accepted from a router (a device description or a SOAP reply).
const MAX_UPNP_REPLY: usize = 64 * 1024;
/// The UPnP service that maps ports.
const WAN_IP_CONNECTION: &str = "urn:schemas-upnp-org:service:WANIPConnection:1";

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
    /// The port the router confirms it forwards (UPnP), with the id of the [`forward_port`]
    /// task that keeps the mapping.
    mapped: Option<(u64, u16)>,
    /// Increases with each [`forward_port`] task.
    mapping_id: u64,
    /// Increases each time a listener opens, so peers accepted by a closed one do not count.
    listener_id: u64,
    /// When a peer from the internet last connected in, and on which listener.
    inbound: Option<(u64, Instant)>,
    /// Whether a task waits to report that the last inbound peer is too long ago (see
    /// [`Inbound::record`]).
    inbound_expiry: bool,
}

impl IncomingState {
    /// Whether a peer connected in on the open listener within `within`.
    fn inbound_within(&self, within: Duration) -> bool {
        self.inbound
            .is_some_and(|(listener, at)| listener == self.listener_id && at.elapsed() < within)
    }

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

    /// The listening port that peers on the internet can reach, as far as TorrentPlayer can
    /// tell: while the router confirms a UPnP mapping for it and its own address is public
    /// (direct mode), or while a peer from the internet connected in on the open listener within
    /// [`NetworkOptions::reachable_after_inbound`](crate::NetworkOptions) (direct and WireGuard
    /// modes: that covers a port forwarded by hand or by the VPN provider, once trackers have
    /// sent a peer). `None` otherwise, and never in SOCKS5 mode, which does not listen.
    /// [`Network::status_changes`] fires when this may have changed.
    #[must_use]
    pub fn reachable_port(&self) -> Option<u16> {
        let st = self.inner.state();
        let IncomingStatus::Listening { port, .. } = st.incoming.status else {
            return None;
        };
        let mapped = st.incoming.mapped.is_some_and(|(_, mapped)| mapped == port);
        (mapped
            || st
                .incoming
                .inbound_within(self.inner.options.reachable_after_inbound))
        .then_some(port)
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
    let mut active: Option<(Key, Listening)> = None;
    loop {
        let Some(net) = inner.upgrade() else {
            return;
        };
        let plan = plan(&net.state());
        let (consumer, retry) = match plan {
            Plan::Stop(status) => {
                close(&mut active).await;
                set_status(&net, status);
                (None, false)
            }
            Plan::Listen(key, tx) => {
                let mut failed = false;
                if active.as_ref().is_none_or(|(current, _)| *current != key) {
                    // Close the old sockets before opening the new ones on the same port.
                    close(&mut active).await;
                    match open(&net, key, &tx).await {
                        Ok((port, addresses, listening)) => {
                            active = Some((Key { port, ..key }, listening));
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
                close(&mut active).await;
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

/// The accept loops of one open listener. Dropping it stops them; [`close`] also waits until
/// their sockets are closed.
struct Listening {
    cancel: CancellationToken,
    accepting: Vec<JoinHandle<()>>,
}

impl Drop for Listening {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Closes the active listener and waits until its sockets are closed, so the port neither
/// accepts after the status says it stopped nor is still taken when it is opened again.
async fn close(active: &mut Option<(Key, Listening)>) {
    let Some((_, mut listening)) = active.take() else {
        return;
    };
    listening.cancel.cancel();
    for task in listening.accepting.drain(..) {
        let _ = task.await;
    }
}

/// Sockets opened for one route.
enum Bound {
    Os(Vec<(TcpListener, SocketAddr)>),
    Tunnel(Vec<(tokio_wireguard::TcpListener, SocketAddr)>, Tunnel),
}

/// Opens the listening sockets for `key` and starts accepting. Returns the port (a random one
/// may have been replaced), the addresses, and the accept loops.
async fn open(
    net: &Arc<Inner>,
    key: Key,
    tx: &mpsc::Sender<IncomingPeer>,
) -> Result<(u16, Vec<String>, Listening), NetError> {
    let network = Network {
        inner: Arc::clone(net),
    };
    let (route, lease, ipv4_only) = network.route()?;
    let random = net.state().incoming.config.port == 0;
    let mut port = key.port;
    let mut attempts = 0;
    let bound = loop {
        match bind_route(&route, port, ipv4_only, net.options.loopback_only).await {
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
    let mut accepting = Vec::new();
    let inbound = Inbound {
        net: Arc::downgrade(net),
        listener: {
            let mut st = net.state();
            st.incoming.listener_id += 1;
            st.incoming.listener_id
        },
        // Only the VPN server can send connections into the tunnel; tests' peers are on
        // loopback.
        any_source: matches!(bound, Bound::Tunnel(..)) || net.options.loopback_only,
    };
    match bound {
        Bound::Os(listeners) => {
            for (listener, addr) in listeners {
                addresses.push(addr.to_string());
                accepting.push(tokio::spawn(accept_os(
                    listener,
                    tx.clone(),
                    lease.clone(),
                    inbound.clone(),
                    cancel.child_token(),
                )));
            }
        }
        Bound::Tunnel(listeners, tunnel) => {
            for (listener, addr) in listeners {
                addresses.push(addr.to_string());
                accepting.push(tokio::spawn(accept_tunnel(
                    listener,
                    tunnel.clone(),
                    tx.clone(),
                    lease.clone(),
                    inbound.clone(),
                    cancel.child_token(),
                )));
            }
        }
    }
    if key.upnp
        && matches!(route, Route::Direct)
        && (!net.options.loopback_only || net.options.upnp_control_url.is_some())
    {
        tokio::spawn(forward_port(
            Arc::downgrade(net),
            port,
            cancel.child_token(),
        ));
    }
    Ok((port, addresses, Listening { cancel, accepting }))
}

/// Opens the sockets for `route`, never anywhere else: every interface in direct mode (loopback
/// in tests, see [`direct_listen_ips`]), the tunnel's addresses for WireGuard.
async fn bind_route(
    route: &Route,
    port: u16,
    ipv4_only: bool,
    loopback_only: bool,
) -> Result<Bound, NetError> {
    let os = |ip: IpAddr| {
        let addr = SocketAddr::new(ip, port);
        listen(addr)
            .and_then(|l| TcpListener::from_std(l).map_err(NetError::Io))
            .map(|l| (l, addr))
    };
    match route {
        Route::Direct => {
            let [v4, v6] = direct_listen_ips(loopback_only);
            let mut listeners = vec![os(v4)?];
            // IPv6 is optional: not every machine has it.
            match os(v6) {
                Ok(v6) => listeners.push(v6),
                Err(error) => {
                    tracing::debug!(target: "torrentplayer_net", %error, "not listening on IPv6");
                }
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

/// The addresses the direct-mode listener opens: IPv4 and IPv6 on every interface (loopback
/// with [`NetworkOptions::loopback_only`](crate::NetworkOptions), for tests).
fn direct_listen_ips(loopback_only: bool) -> [IpAddr; 2] {
    [
        crate::addr::local_ip(false, loopback_only),
        crate::addr::local_ip(true, loopback_only),
    ]
}

/// A listening socket on `addr`. An IPv6 socket accepts only IPv6, so the IPv4 one can share
/// the port.
fn listen(addr: SocketAddr) -> Result<std::net::TcpListener, NetError> {
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    if addr.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    // Elsewhere this only allows rebinding over connections in TIME_WAIT; on Windows it would
    // let the socket share a port another program is listening on.
    #[cfg(not(windows))]
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    socket.listen(128)?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

/// Hands an accepted connection to the consumer, if it is still allowed and there is room.
fn deliver(
    tx: &mpsc::Sender<IncomingPeer>,
    stream: TcpStream,
    addr: SocketAddr,
    lease: &Lease,
    inbound: &Inbound,
) {
    if !lease.is_valid() {
        return;
    }
    // Reachable even if the engine is too busy to take the peer.
    inbound.record(addr);
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
    inbound: Inbound,
    cancel: CancellationToken,
) {
    loop {
        let accepted = tokio::select! {
            () = cancel.cancelled() => return,
            accepted = listener.accept() => accepted,
        };
        match accepted {
            Ok((stream, addr)) => deliver(&tx, stream, addr, &lease, &inbound),
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
    inbound: Inbound,
    cancel: CancellationToken,
) {
    loop {
        let accepted = tokio::select! {
            () = cancel.cancelled() => return,
            accepted = listener.accept() => accepted,
        };
        match accepted {
            Ok((upstream, addr)) => match tunnel.relay(upstream, Some(lease.clone())).await {
                Ok(stream) => deliver(&tx, stream, addr, &lease, &inbound),
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

/// Records the peers that connect in on one listener, for [`Network::reachable_port`].
#[derive(Clone)]
struct Inbound {
    net: Weak<Inner>,
    listener: u64,
    /// Every peer proves the port reachable, not only those at public addresses.
    any_source: bool,
}

impl Inbound {
    /// Records that a peer connected in from `addr`. In direct mode only a peer at a public
    /// address shows that the internet reaches the port; a peer on the local network does not.
    fn record(&self, addr: SocketAddr) {
        if !self.any_source && !crate::addr::is_public_ip(addr.ip()) {
            return;
        }
        let Some(net) = self.net.upgrade() else {
            return;
        };
        let mut st = net.state();
        if st.incoming.listener_id != self.listener {
            return;
        }
        let reachable = st
            .incoming
            .inbound_within(net.options.reachable_after_inbound);
        st.incoming.inbound = Some((self.listener, Instant::now()));
        if reachable {
            return;
        }
        st.log(
            EventLevel::Info,
            "A peer connected in from the internet: the listening port is reachable".into(),
        );
        let expiry = !std::mem::replace(&mut st.incoming.inbound_expiry, true);
        drop(st);
        net.status_changed();
        if expiry {
            tokio::spawn(expire_inbound(self.net.clone()));
        }
    }
}

/// Waits until the last peer that connected in is
/// [`NetworkOptions::reachable_after_inbound`](crate::NetworkOptions) ago, then reports that
/// the port may no longer be reachable.
async fn expire_inbound(net: Weak<Inner>) {
    loop {
        let Some(due) = net.upgrade().and_then(|net| {
            let st = net.state();
            st.incoming
                .inbound
                .map(|(_, at)| at + net.options.reachable_after_inbound)
        }) else {
            return;
        };
        tokio::time::sleep_until(due.into()).await;
        let Some(net) = net.upgrade() else {
            return;
        };
        let mut st = net.state();
        if st
            .incoming
            .inbound
            .is_some_and(|(_, at)| at.elapsed() < net.options.reachable_after_inbound)
        {
            // Another peer connected in since.
            continue;
        }
        st.incoming.inbound_expiry = false;
        drop(st);
        tracing::debug!(target: "torrentplayer_net", "no peer connected in for a while: the listening port no longer counts as reachable");
        net.status_changed();
        return;
    }
}

/// Keeps a UPnP mapping for `port` on the router (direct mode only) until cancelled, and
/// records while the router confirms it ([`Network::reachable_port`]). The mapping is leased
/// for [`NetworkOptions::upnp_lease`](crate::NetworkOptions) and renewed at half of it, so it
/// lapses within a lease after it stops, and counts as lost once a lease passes without a
/// renewal. It counts only while the router's own address is public: behind a second NAT
/// (carrier-grade NAT, or a router behind another one) peers cannot reach the port either.
async fn forward_port(net: Weak<Inner>, port: u16, cancel: CancellationToken) {
    let Some((lease, fixed, id)) = net.upgrade().map(|net| {
        let mut st = net.state();
        st.incoming.mapping_id += 1;
        let id = st.incoming.mapping_id;
        drop(st);
        (
            net.options.upnp_lease,
            net.options.upnp_control_url.clone(),
            id,
        )
    }) else {
        return;
    };
    let mapping = Mapping { net, id, port };
    let client = match reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(UPNP_REQUEST_TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!(target: "torrentplayer_net", %error, "UPnP port mapping failed");
            return;
        }
    };
    tracing::info!(target: "torrentplayer_net", port, "requesting a UPnP port mapping");
    let mut gateways = Vec::new();
    let mut discovered: Option<Instant> = None;
    // The public address of the last confirmed mapping, and when that mapping lapses.
    let mut confirmed: Option<(IpAddr, Instant)> = None;
    let mut last_external = None;
    loop {
        let alive = confirmed.is_some_and(|(_, until)| until > Instant::now());
        if !alive && discovered.is_none_or(|at| at.elapsed() >= UPNP_DISCOVER_EVERY) {
            discovered = Some(Instant::now());
            gateways = if let Some(url) = &fixed {
                gateway_at(url).into_iter().collect()
            } else {
                // A mode change or a closed listener stops discovery at once.
                tokio::select! {
                    () = cancel.cancelled() => return,
                    gateways = discover_gateways(&client) => gateways,
                }
            };
        }
        let external = tokio::select! {
            () = cancel.cancelled() => return,
            external = map_any(&gateways, &client, port, lease) => external,
        };
        // Cancelled while the last reply arrived: never record a mapping afterwards.
        if cancel.is_cancelled() {
            return;
        }
        if let Some(ip) = external
            && last_external != external
            && !crate::addr::is_public_ip(ip)
        {
            tracing::info!(target: "torrentplayer_net", port, %ip, "the router maps the port, but its address is not public (another NAT in front of it), so peers cannot reach it");
        }
        if external.is_some() {
            last_external = external;
        }
        let now = Instant::now();
        match external {
            Some(ip) if crate::addr::is_public_ip(ip) => confirmed = Some((ip, now + lease)),
            Some(_) => confirmed = None,
            // A failed renewal: the last mapping holds until its lease ends.
            None => {}
        }
        mapping.report(
            confirmed
                .filter(|(_, until)| *until > now)
                .map(|(ip, _)| ip),
        );
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(lease / 2) => {}
        }
    }
}

/// Maps `port` on the first of `gateways` that accepts, and returns its external address.
async fn map_any(
    gateways: &[Gateway],
    client: &reqwest::Client,
    port: u16,
    lease: Duration,
) -> Option<IpAddr> {
    for gateway in gateways {
        match gateway.map(client, port, lease).await {
            Ok(ip) => return Some(ip),
            Err(error) => {
                tracing::debug!(target: "torrentplayer_net", url = %gateway.control_url, %error, "UPnP port mapping failed");
            }
        }
    }
    None
}

/// The mapping kept by one [`forward_port`] task, forgotten when the task ends.
struct Mapping {
    net: Weak<Inner>,
    id: u64,
    port: u16,
}

impl Mapping {
    /// Records whether the router confirms the mapping, with its public address if it does.
    fn report(&self, external: Option<IpAddr>) {
        let Some(net) = self.net.upgrade() else {
            return;
        };
        let mut st = net.state();
        let recorded = st.incoming.mapped.is_some_and(|(id, _)| id == self.id);
        match external {
            Some(ip) if !recorded => {
                st.incoming.mapped = Some((self.id, self.port));
                st.log(
                    EventLevel::Info,
                    format!(
                        "The router forwards port {} to TorrentPlayer (UPnP, public address {ip})",
                        self.port
                    ),
                );
            }
            None if recorded => {
                st.incoming.mapped = None;
                st.log(
                    EventLevel::Info,
                    format!("The router no longer forwards port {} (UPnP)", self.port),
                );
            }
            _ => return,
        }
        drop(st);
        net.status_changed();
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        let Some(net) = self.net.upgrade() else {
            return;
        };
        let mut st = net.state();
        if st.incoming.mapped.is_some_and(|(id, _)| id == self.id) {
            st.incoming.mapped = None;
            drop(st);
            tracing::debug!(target: "torrentplayer_net", port = self.port, "UPnP port mapping stopped");
            net.status_changed();
        }
    }
}

/// A router's UPnP `WANIPConnection` service, and this computer's address towards it.
struct Gateway {
    control_url: url::Url,
    local_ip: IpAddr,
}

impl Gateway {
    /// Maps TCP `port` on the router to the same port here for `lease`, and returns the
    /// router's own (external) address.
    async fn map(
        &self,
        client: &reqwest::Client,
        port: u16,
        lease: Duration,
    ) -> Result<IpAddr, String> {
        let arguments = format!(
            "<NewRemoteHost></NewRemoteHost><NewExternalPort>{port}</NewExternalPort>\
             <NewProtocol>TCP</NewProtocol><NewInternalPort>{port}</NewInternalPort>\
             <NewInternalClient>{}</NewInternalClient><NewEnabled>1</NewEnabled>\
             <NewPortMappingDescription>TorrentPlayer</NewPortMappingDescription>\
             <NewLeaseDuration>{}</NewLeaseDuration>",
            self.local_ip,
            lease.as_secs().max(1)
        );
        self.call(client, "AddPortMapping", &arguments).await?;
        let reply = self.call(client, "GetExternalIPAddress", "").await?;
        xml_value(&reply, "NewExternalIPAddress")
            .and_then(|ip| ip.trim().parse().ok())
            .ok_or_else(|| "the router did not say its external address".to_owned())
    }

    /// Calls `action` on the service (SOAP) and returns the reply.
    async fn call(
        &self,
        client: &reqwest::Client,
        action: &str,
        arguments: &str,
    ) -> Result<String, String> {
        let body = format!(
            "<?xml version=\"1.0\"?><s:Envelope \
             xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
             s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>\
             <u:{action} xmlns:u=\"{WAN_IP_CONNECTION}\">{arguments}</u:{action}>\
             </s:Body></s:Envelope>"
        );
        let response = client
            .post(self.control_url.clone())
            .header("Content-Type", "text/xml; charset=\"utf-8\"")
            .header("SOAPAction", format!("\"{WAN_IP_CONNECTION}#{action}\""))
            .body(body)
            .send()
            .await
            .map_err(|error| format!("{action}: {error}"))?;
        let status = response.status();
        let reply = read_reply(response)
            .await
            .map_err(|error| format!("{action}: {error}"))?;
        if !status.is_success() {
            let detail = xml_value(&reply, "errorDescription")
                .map(|d| format!(" ({d})"))
                .unwrap_or_default();
            return Err(format!("{action}: HTTP {status}{detail}"));
        }
        Ok(reply)
    }
}

/// A router's reply as text, up to [`MAX_UPNP_REPLY`] bytes.
async fn read_reply(mut response: reqwest::Response) -> Result<String, String> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
        if body.len() + chunk.len() > MAX_UPNP_REPLY {
            return Err("the router sent too much data".into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// The service at `control_url`, given rather than discovered (tests).
fn gateway_at(control_url: &str) -> Option<Gateway> {
    let control_url = url::Url::parse(control_url).ok()?;
    let router = control_url
        .socket_addrs(|| Some(80))
        .ok()?
        .into_iter()
        .next()?;
    Some(Gateway {
        local_ip: local_ip_towards(router).ok()?,
        control_url,
    })
}

/// Looks for routers with a `WANIPConnection` service on the local network (SSDP). Only
/// routers on the local network count: a description or control URL pointing anywhere else
/// is ignored, as anyone on the network can answer the search.
async fn discover_gateways(client: &reqwest::Client) -> Vec<Gateway> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    if let Err(error) = librqbit_upnp::discover_once(
        &tx,
        librqbit_upnp::SSDP_SEARCH_WAN_IPCONNECTION_ST,
        UPNP_DISCOVER_TIMEOUT,
        None,
    )
    .await
    {
        tracing::debug!(target: "torrentplayer_net", error = %format!("{error:#}"), "UPnP discovery failed");
    }
    drop(tx);
    let mut gateways: Vec<Gateway> = Vec::new();
    while let Some(found) = rx.recv().await {
        if !on_local_network(&found.location) {
            continue;
        }
        let description = match client.get(found.location.clone()).send().await {
            Ok(response) if response.status().is_success() => read_reply(response).await,
            Ok(response) => Err(format!("HTTP {}", response.status())),
            Err(error) => Err(error.to_string()),
        };
        let description = match description {
            Ok(description) => description,
            Err(error) => {
                tracing::debug!(target: "torrentplayer_net", location = %found.location, %error, "unusable UPnP router");
                continue;
            }
        };
        let Ok(local_ip) = local_ip_towards(found.received_from) else {
            continue;
        };
        for control_url in wan_ip_control_urls(&description) {
            if let Ok(control_url) = found.location.join(control_url)
                && on_local_network(&control_url)
                && !gateways.iter().any(|g| g.control_url == control_url)
            {
                gateways.push(Gateway {
                    control_url,
                    local_ip,
                });
            }
        }
    }
    tracing::debug!(target: "torrentplayer_net", gateways = gateways.len(), "UPnP discovery");
    gateways
}

/// The control URLs of the `WANIPConnection` services in a router's device description,
/// at any depth of its device tree.
fn wan_ip_control_urls(description: &str) -> Vec<&str> {
    description
        .split("<service>")
        .skip(1)
        .filter_map(|service| {
            let service = service.split("</service>").next()?;
            if xml_value(service, "serviceType")?.trim() != WAN_IP_CONNECTION {
                return None;
            }
            xml_value(service, "controlURL").map(str::trim)
        })
        .collect()
}

/// Whether `url` names a host by a local (not public) address.
fn on_local_network(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => !crate::addr::is_public_ip(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => !crate::addr::is_public_ip(IpAddr::V6(ip)),
        _ => false,
    }
}

/// This computer's address on the route to `addr` (connecting a UDP socket sends nothing).
fn local_ip_towards(addr: SocketAddr) -> std::io::Result<IpAddr> {
    if addr.ip().is_loopback() {
        return Ok(addr.ip());
    }
    let any = if addr.is_ipv4() {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    } else {
        IpAddr::V6(Ipv6Addr::UNSPECIFIED)
    };
    let socket = std::net::UdpSocket::bind((any, 0))?;
    socket.connect(addr)?;
    Ok(socket.local_addr()?.ip())
}

/// The text of the first `<tag>` element in `xml`.
fn xml_value<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let start = xml.find(&format!("<{tag}>"))? + tag.len() + 2;
    let end = start + xml[start..].find(&format!("</{tag}>"))?;
    Some(&xml[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn direct_mode_listens_on_every_interface() {
        assert_eq!(
            direct_listen_ips(false),
            [
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                IpAddr::V6(Ipv6Addr::UNSPECIFIED)
            ]
        );
        assert_eq!(
            direct_listen_ips(true),
            [
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(Ipv6Addr::LOCALHOST)
            ]
        );
    }

    #[tokio::test]
    async fn listeners_bind_only_the_given_address() {
        let listener = listen(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
        let addr = listener.local_addr().unwrap();
        assert_eq!(addr.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        let listener = TcpListener::from_std(listener).unwrap();
        let client = TcpStream::connect(addr).await.unwrap();
        let (_accepted, from) = listener.accept().await.unwrap();
        assert_eq!(from, client.local_addr().unwrap());
        // The same port cannot be opened twice.
        assert!(listen(addr).is_err());
    }

    #[test]
    fn finds_the_port_mapping_service_anywhere_in_a_device_description() {
        let description = format!(
            "<root><device><serviceList><service><serviceType>urn:other</serviceType>\
             <controlURL>/other</controlURL></service></serviceList><deviceList><device>\
             <serviceList><service>\n  <serviceType>{WAN_IP_CONNECTION}</serviceType>\n  \
             <controlURL> /ctl/IPConn </controlURL></service></serviceList></device>\
             </deviceList></device></root>"
        );
        assert_eq!(wan_ip_control_urls(&description), ["/ctl/IPConn"]);
        assert!(wan_ip_control_urls("<root></root>").is_empty());
    }

    /// A router on loopback that answers every request with `response`, then closes.
    async fn router(response: Vec<u8>) -> Gateway {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(&response).await;
                let _ = stream.shutdown().await;
            }
        });
        gateway_at(&format!("http://{addr}/control")).unwrap()
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(UPNP_REQUEST_TIMEOUT)
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn a_router_reply_is_capped_and_redirects_are_not_followed() {
        let body = "x".repeat(MAX_UPNP_REPLY + 1);
        let huge = router(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .into_bytes(),
        )
        .await;
        let error = huge.call(&client(), "GetExternalIPAddress", "").await;
        assert!(error.is_err_and(|e| e.contains("too much")));

        let redirect = router(
            b"HTTP/1.1 302 Found\r\nLocation: http://93.184.216.34/\r\nContent-Length: 0\r\n\
              Connection: close\r\n\r\n"
                .to_vec(),
        )
        .await;
        let error = redirect.call(&client(), "AddPortMapping", "").await;
        assert!(error.is_err_and(|e| e.contains("302")));
    }
}
