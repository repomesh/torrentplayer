// SPDX-License-Identifier: MIT OR Apache-2.0

//! The network manager: the selected mode, its health, the kill switch, and every outbound
//! connection TorrentPlayer makes.
//!
//! Every connection asks [`Network`] first. While a protected mode (anything but
//! [`NetworkMode::Direct`]) is not verified as working, connections are refused: there is no
//! fallback to a direct connection. A background monitor re-checks the mode every few seconds,
//! engages the kill switch when it fails, and lifts it when the mode works again.
//!
//! Each change that could leave a connection on the wrong path (a new mode, the kill switch
//! engaging) starts a new *epoch*. Connections carry a [`Lease`] for the epoch they were made in
//! and must close when it is revoked.

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{Notify, watch};
use tokio_util::sync::CancellationToken;
use ts_rs::TS;
use url::Url;
use zeroize::Zeroizing;

use crate::addr::is_public_ip;
use crate::bridge;
use crate::error::NetError;
use crate::incoming::{IncomingState, IncomingStatus};
use crate::mode::{ModeKind, NetworkMode, Socks5Config};
use crate::secret::Secret;
use crate::socks5::{self, Target};
use crate::tunnel::Tunnel;

/// Where the connection check asks for the public IPv4 address (a plain-text answer), always
/// through the current mode. See `docs/network.md`.
pub const IP_CHECK_V4_URL: &str = "https://ipv4.icanhazip.com/";
/// Where it asks for the public IPv6 address.
pub const IP_CHECK_V6_URL: &str = "https://ipv6.icanhazip.com/";
/// Events kept for the status view.
const MAX_EVENTS: usize = 100;
/// Events included in [`NetworkStatus`].
const STATUS_EVENTS: usize = 50;
/// Largest response accepted from the connection-check service.
const MAX_IP_RESPONSE: usize = 1024;

/// What the user selected.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NetworkConfig {
    /// How traffic leaves the machine.
    pub mode: NetworkMode,
    /// Allow IPv6 destinations in protected modes. Off by default, so IPv6 cannot bypass a VPN
    /// or proxy that only carries IPv4.
    pub allow_ipv6: bool,
    /// Hold all traffic, whatever the mode, until this is cleared: the state is
    /// [`ConnectionState::Error`] with this reason, and the policy counts as protected, so the
    /// engines start as they would behind the kill switch. For settings that need the user's
    /// confirmation before anything is sent (the removed VPN adapter mode).
    pub hold: Option<String>,
}

impl NetworkConfig {
    /// Whether the kill switch applies: a protected mode, or a hold.
    #[must_use]
    pub fn is_protected(&self) -> bool {
        self.mode.is_protected() || self.hold.is_some()
    }
}

/// Timing of health checks and connections.
#[derive(Debug, Clone)]
pub struct NetworkOptions {
    /// How often a working mode is re-checked, and a failed one retried.
    pub health_interval: Duration,
    /// How soon a mode is re-checked after it was lost.
    pub retry_interval: Duration,
    /// Time limit for one health check.
    pub check_timeout: Duration,
    /// Time limit for connections made through the loopback bridge and for the connection check.
    pub connect_timeout: Duration,
    /// Where the connection check asks for the public IPv4 address (see [`IP_CHECK_V4_URL`]).
    pub ip_check_v4_url: String,
    /// Where it asks for the public IPv6 address (see [`IP_CHECK_V6_URL`]); `None` skips it.
    pub ip_check_v6_url: Option<String>,
    /// How long traffic sent into a WireGuard tunnel may go unanswered before the server counts
    /// as not responding and the kill switch engages. A tunnel that has heard nothing for half
    /// of this is probed. See [`WIREGUARD_REPLY_TIMEOUT`].
    pub wireguard_reply_timeout: Duration,
    /// How long a WireGuard server may leave traffic unanswered before the tunnel is replaced
    /// by a new one (a fresh handshake and source port). See [`WIREGUARD_RESTART_AFTER`].
    pub wireguard_restart_after: Duration,
}

/// The default [`NetworkOptions::wireguard_reply_timeout`]: WireGuard's `KEEPALIVE_TIMEOUT`
/// (10 s, within which a peer that received data answers, if only with a keepalive) plus two
/// `REKEY_TIMEOUT`s (5 s each, for a handshake retried while the session is renewed).
pub const WIREGUARD_REPLY_TIMEOUT: Duration = Duration::from_secs(20);

/// The default [`NetworkOptions::wireguard_restart_after`]: twice WireGuard's
/// `REJECT_AFTER_TIME` (180 s), long after every session the server could still hold has
/// expired. At most one handshake for a new tunnel every 6 minutes.
pub const WIREGUARD_RESTART_AFTER: Duration = Duration::from_secs(360);

impl Default for NetworkOptions {
    fn default() -> Self {
        Self {
            health_interval: Duration::from_secs(5),
            retry_interval: Duration::from_secs(1),
            check_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(15),
            ip_check_v4_url: IP_CHECK_V4_URL.to_owned(),
            ip_check_v6_url: Some(IP_CHECK_V6_URL.to_owned()),
            wireguard_reply_timeout: WIREGUARD_REPLY_TIMEOUT,
            wireguard_restart_after: WIREGUARD_RESTART_AFTER,
        }
    }
}

/// The connection state shown to the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(tag = "kind", rename_all = "camelCase")]
#[ts(export, rename = "NetworkConnectionState")]
pub enum ConnectionState {
    /// Direct mode: no VPN or proxy, no kill switch.
    Direct,
    /// A protected mode was just selected and is being verified. Traffic waits.
    Connecting,
    /// The protected mode works. Traffic flows.
    Connected,
    /// The protected mode stopped working a moment ago and is being retried. Traffic is
    /// blocked.
    Reconnecting {
        /// What failed.
        reason: String,
    },
    /// TorrentPlayer's network is stopped (the app is closing). Nothing is sent.
    Disconnected,
    /// The protected mode is not working. All traffic is blocked; it resumes automatically when
    /// the mode works again.
    KillSwitchActive {
        /// What failed.
        reason: String,
    },
    /// The selected mode cannot work (unsupported or incomplete settings). All traffic is blocked
    /// until the settings change.
    Error {
        /// Why.
        reason: String,
    },
}

impl ConnectionState {
    /// Whether connections are allowed.
    #[must_use]
    pub fn allows_traffic(&self) -> bool {
        matches!(self, Self::Direct | Self::Connected)
    }
}

/// Severity of an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, rename = "NetworkEventLevel")]
pub enum EventLevel {
    /// Normal operation.
    Info,
    /// Traffic was blocked.
    Warn,
    /// The mode cannot work.
    Error,
}

/// One entry in the network event log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, rename = "NetworkEvent")]
pub struct NetEvent {
    /// When it happened, in milliseconds since the Unix epoch.
    pub at_ms: u64,
    /// Severity.
    pub level: EventLevel,
    /// What happened. Never contains secrets.
    pub message: String,
}

/// A snapshot for the status view.
// Independent flags the UI shows side by side, not a hidden state machine.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct NetworkStatus {
    /// Selected mode.
    pub mode: ModeKind,
    /// Connection state.
    pub state: ConnectionState,
    /// Proxy endpoint or WireGuard endpoint; no secrets.
    pub endpoint: Option<String>,
    /// Whether connections are allowed right now.
    pub traffic_allowed: bool,
    /// Whether the kill switch is blocking traffic (a protected mode that is not connected).
    pub kill_switch_engaged: bool,
    /// Whether scoped UDP is available: direct, through WireGuard, or via a SOCKS5 UDP
    /// association. Proxy UDP support is negotiated per association, not by the health check.
    pub udp_allowed: bool,
    /// Whether trackers may be contacted.
    pub trackers_allowed: bool,
    /// When the mode was last checked, in milliseconds since the Unix epoch.
    pub last_check_ms: Option<u64>,
    /// While connected: since when, in milliseconds since the Unix epoch (for the uptime).
    pub connected_since_ms: Option<u64>,
    /// Recent events, oldest first.
    pub events: Vec<NetEvent>,
    /// Whether incoming peer connections are accepted, and where.
    pub incoming: IncomingStatus,
}

/// What connections may do right now. Published on every change; see [`Network::subscribe`].
// Independent permissions, each checked by a different consumer.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// Increases whenever existing connections must close.
    pub epoch: u64,
    /// Whether new connections are allowed.
    pub traffic_allowed: bool,
    /// Whether a protected mode (proxy or tunnel) is selected, or traffic is held.
    pub protected: bool,
    /// Whether UDP (DHT, UDP trackers, uTP) may be sent from unbound sockets: direct mode only.
    pub udp_allowed: bool,
    /// Whether UDP may use sockets made by [`Network::udp_socket`]: direct, inside WireGuard,
    /// or through SOCKS5 UDP ASSOCIATE. A proxy's UDP support is checked when opening the
    /// association. Unbound OS UDP stays disabled in protected modes.
    pub bound_udp_allowed: bool,
    /// Whether DHT may use scoped datagrams (honors the SOCKS5 DHT preference).
    pub dht_allowed: bool,
    /// Whether trackers may be contacted.
    pub trackers_allowed: bool,
}

/// The public address the connection check saw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct PublicIp {
    /// The address other hosts see: the IPv4 address, or the IPv6 one without IPv4.
    pub ip: String,
    /// The public IPv4 address, if the connection has one.
    pub ipv4: Option<String>,
    /// The public IPv6 address, if the connection has one.
    pub ipv6: Option<String>,
    /// The mode the check went through.
    pub mode: ModeKind,
    /// The service that was asked.
    pub service: String,
    /// When, in milliseconds since the Unix epoch.
    pub checked_at_ms: u64,
}

/// Proof that a connection was allowed in a given epoch. Close the connection when
/// [`Lease::revoked`] completes.
#[derive(Debug, Clone)]
pub struct Lease {
    rx: watch::Receiver<Policy>,
    epoch: u64,
}

impl Lease {
    /// Whether the connection may stay open.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        let policy = *self.rx.borrow();
        policy.epoch == self.epoch && policy.traffic_allowed
    }

    /// Completes when the connection must close: the mode changed, the kill switch engaged, or
    /// the network stopped.
    pub async fn revoked(&mut self) {
        loop {
            let policy = *self.rx.borrow_and_update();
            if policy.epoch != self.epoch || !policy.traffic_allowed {
                return;
            }
            if self.rx.changed().await.is_err() {
                return;
            }
        }
    }
}

/// The network manager. Cheap to clone; clones share state.
#[derive(Clone)]
pub struct Network {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct Inner {
    state: Mutex<State>,
    pub(crate) policy: watch::Sender<Policy>,
    wake: Arc<Notify>,
    /// Wakes the incoming-connection listener (see [`crate::incoming`]).
    pub(crate) incoming_wake: Arc<Notify>,
    /// Increases whenever [`Network::status`] changes.
    status_seq: watch::Sender<u64>,
    pub(crate) options: NetworkOptions,
    bridge_addr: SocketAddr,
    pub(crate) bridge_token: Secret,
    cancel: CancellationToken,
    /// Test seam: how long host name lookups through the system resolver take.
    #[cfg(test)]
    pub(crate) resolve_delay: Mutex<Duration>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(tunnel) = self
            .state
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .tunnel
            .take()
        {
            tunnel.close();
        }
    }
}

pub(crate) struct State {
    pub(crate) config: NetworkConfig,
    pub(crate) conn: ConnectionState,
    tunnel: Option<Tunnel>,
    /// Increases on every `apply`, so results of checks started under old settings are dropped.
    generation: u64,
    pub(crate) epoch: u64,
    last_check: Option<SystemTime>,
    /// When the state last became [`ConnectionState::Connected`]; meaningful only while it is.
    connected_since: Option<SystemTime>,
    events: VecDeque<NetEvent>,
    /// Incoming peer connections.
    pub(crate) incoming: IncomingState,
}

impl State {
    fn policy(&self) -> Policy {
        let protected = self.config.is_protected();
        Policy {
            epoch: self.epoch,
            traffic_allowed: self.conn.allows_traffic(),
            protected,
            udp_allowed: !protected,
            bound_udp_allowed: true,
            dht_allowed: match &self.config.mode {
                NetworkMode::Socks5(cfg) => cfg.proxy_dht,
                _ => true,
            },
            trackers_allowed: match &self.config.mode {
                NetworkMode::Socks5(c) => c.proxy_trackers,
                _ => true,
            },
        }
    }

    pub(crate) fn log(&mut self, level: EventLevel, message: String) {
        match level {
            EventLevel::Info => tracing::info!(target: "torrentplayer_net", "{message}"),
            EventLevel::Warn => tracing::warn!(target: "torrentplayer_net", "{message}"),
            EventLevel::Error => tracing::error!(target: "torrentplayer_net", "{message}"),
        }
        if self.events.len() == MAX_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(NetEvent {
            at_ms: unix_ms(SystemTime::now()),
            level,
            message,
        });
    }

    fn blocked(&self) -> NetError {
        NetError::KillSwitch(self.blocked_reason())
    }

    /// Why traffic is blocked, for users.
    pub(crate) fn blocked_reason(&self) -> String {
        match &self.conn {
            ConnectionState::Connecting => {
                "the connection is being verified; traffic waits until it is".to_owned()
            }
            ConnectionState::Reconnecting { reason }
            | ConnectionState::KillSwitchActive { reason }
            | ConnectionState::Error { reason } => reason.clone(),
            ConnectionState::Disconnected => "the network is stopped".to_owned(),
            ConnectionState::Direct | ConnectionState::Connected => "unknown".to_owned(),
        }
    }
}

/// Where a connection goes.
pub(crate) enum Route {
    Direct,
    Socks(Socks5Config),
    WireGuard(Tunnel),
}

fn unix_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn describe(mode: &NetworkMode) -> String {
    match mode {
        NetworkMode::Direct => "direct (no VPN or proxy)".to_owned(),
        NetworkMode::Socks5(c) => format!("SOCKS5 proxy {}", c.endpoint()),
        NetworkMode::WireGuard(c) => format!(
            "WireGuard ({})",
            c.peers
                .first()
                .map_or("no endpoint", |p| p.endpoint.as_str())
        ),
    }
}

/// The state a newly selected configuration starts in. A hold, or an invalid protected
/// configuration (for example a proxy without a host, or a username without its password),
/// blocks traffic.
fn initial_state(config: &NetworkConfig) -> ConnectionState {
    if let Some(reason) = &config.hold {
        return ConnectionState::Error {
            reason: reason.clone(),
        };
    }
    let mode = &config.mode;
    if let Err(error) = mode.validate() {
        return ConnectionState::Error {
            reason: error.to_string(),
        };
    }
    match mode {
        NetworkMode::Direct => ConnectionState::Direct,
        NetworkMode::Socks5(_) | NetworkMode::WireGuard(_) => ConnectionState::Connecting,
    }
}

impl std::fmt::Debug for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let st = self.inner.state();
        f.debug_struct("Network")
            .field("mode", &st.config.mode.kind())
            .field("state", &st.conn)
            .finish_non_exhaustive()
    }
}

impl Inner {
    pub(crate) fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn publish(&self, st: &State) {
        self.policy.send_replace(st.policy());
        self.status_changed();
        // The listener follows the policy (see `crate::incoming`).
        self.incoming_wake.notify_one();
    }

    /// Tells [`Network::status_changes`] watchers that the status changed.
    pub(crate) fn status_changed(&self) {
        self.status_seq
            .send_modify(|seq| *seq = seq.wrapping_add(1));
    }

    /// Applies the result of a health check started under `generation`; returns when to check
    /// next.
    fn on_check(&self, generation: u64, result: Result<(), NetError>) -> Duration {
        let interval = self.options.health_interval;
        let mut st = self.state();
        if st.generation != generation
            || !matches!(
                st.conn,
                ConnectionState::Connecting
                    | ConnectionState::Connected
                    | ConnectionState::Reconnecting { .. }
                    | ConnectionState::KillSwitchActive { .. }
            )
        {
            return interval;
        }
        st.last_check = Some(SystemTime::now());
        let via = describe(&st.config.mode);
        match result {
            Ok(()) => {
                match &st.conn {
                    ConnectionState::Connected => {}
                    ConnectionState::Connecting => {
                        st.log(EventLevel::Info, format!("Connected: {via}"));
                    }
                    _ => st.log(
                        EventLevel::Info,
                        format!("Connection restored ({via}); traffic resumed"),
                    ),
                }
                if st.conn != ConnectionState::Connected {
                    st.connected_since = st.last_check;
                }
                st.conn = ConnectionState::Connected;
                self.publish(&st);
                interval
            }
            Err(error) => {
                let reason = error.to_string();
                let next = match &st.conn {
                    ConnectionState::Connected => {
                        st.epoch += 1;
                        st.log(
                            EventLevel::Warn,
                            format!(
                                "Connection lost ({reason}). Kill switch: traffic is blocked \
                                 while TorrentPlayer reconnects."
                            ),
                        );
                        st.conn = ConnectionState::Reconnecting { reason };
                        self.options.retry_interval
                    }
                    ConnectionState::KillSwitchActive { reason: old } if *old == reason => {
                        return interval;
                    }
                    _ => {
                        st.log(
                            EventLevel::Warn,
                            format!(
                                "Not connected ({reason}). Kill switch active: traffic is \
                                 blocked until the connection works."
                            ),
                        );
                        st.conn = ConnectionState::KillSwitchActive { reason };
                        interval
                    }
                };
                self.publish(&st);
                next
            }
        }
    }
}

impl Network {
    /// Starts the network manager with `config`, and its loopback bridge for HTTP clients and
    /// librqbit. Must be called inside a Tokio runtime.
    ///
    /// A protected mode starts in [`ConnectionState::Connecting`], with traffic blocked until
    /// the first check succeeds; an invalid one starts in [`ConnectionState::Error`].
    ///
    /// # Errors
    ///
    /// Returns an error if the bridge cannot listen.
    pub async fn start(config: NetworkConfig, options: NetworkOptions) -> Result<Self, NetError> {
        let token = bridge::new_token()?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let bridge_addr = listener.local_addr()?;
        let mut st = State {
            conn: initial_state(&config),
            config,
            tunnel: None,
            generation: 1,
            epoch: 1,
            last_check: None,
            connected_since: None,
            events: VecDeque::new(),
            incoming: IncomingState::default(),
        };
        let message = format!("Network mode: {}", describe(&st.config.mode));
        st.log(EventLevel::Info, message);
        if let ConnectionState::Error { reason } = &st.conn {
            let reason = reason.clone();
            st.log(EventLevel::Error, reason);
        }
        let (policy, _) = watch::channel(st.policy());
        let cancel = CancellationToken::new();
        let wake = Arc::new(Notify::new());
        let inner = Arc::new(Inner {
            state: Mutex::new(st),
            policy,
            wake: Arc::clone(&wake),
            incoming_wake: Arc::new(Notify::new()),
            status_seq: watch::channel(0).0,
            options,
            bridge_addr,
            bridge_token: token,
            cancel: cancel.clone(),
            #[cfg(test)]
            resolve_delay: Mutex::default(),
        });
        tokio::spawn(monitor(Arc::downgrade(&inner), wake, cancel.clone()));
        tokio::spawn(bridge::serve(
            listener,
            Arc::downgrade(&inner),
            cancel.clone(),
        ));
        tokio::spawn(crate::incoming::serve(Arc::downgrade(&inner), cancel));
        Ok(Self { inner })
    }

    /// Switches to `config`. Every existing connection's lease is revoked, and a protected mode
    /// blocks traffic until it is verified; an invalid one blocks it until the settings change.
    /// Applying the current configuration again changes nothing.
    ///
    /// # Errors
    ///
    /// Returns an error if the network was shut down.
    pub fn apply(&self, config: NetworkConfig) -> Result<NetworkStatus, NetError> {
        {
            let mut st = self.inner.state();
            if st.conn == ConnectionState::Disconnected {
                return Err(NetError::KillSwitch("the network is stopped".into()));
            }
            if st.config == config {
                drop(st);
                return Ok(self.status());
            }
            if let Some(tunnel) = st.tunnel.take() {
                tunnel.close();
            }
            st.conn = initial_state(&config);
            st.config = config;
            st.generation += 1;
            st.epoch += 1;
            st.last_check = None;
            let message = format!("Network mode changed: {}", describe(&st.config.mode));
            st.log(EventLevel::Info, message);
            if let ConnectionState::Error { reason } = &st.conn {
                let reason = reason.clone();
                st.log(EventLevel::Error, reason);
            }
            self.inner.publish(&st);
        }
        self.inner.wake.notify_one();
        Ok(self.status())
    }

    /// Re-checks the mode now instead of at the next interval.
    pub fn recheck(&self) {
        self.inner.wake.notify_one();
    }

    /// Stops the monitor and the bridge and blocks all traffic.
    pub fn shutdown(&self) {
        let mut st = self.inner.state();
        if st.conn != ConnectionState::Disconnected {
            if let Some(tunnel) = st.tunnel.take() {
                tunnel.close();
            }
            st.conn = ConnectionState::Disconnected;
            st.epoch += 1;
            st.log(EventLevel::Info, "Network stopped".into());
            self.inner.publish(&st);
        }
        drop(st);
        self.inner.cancel.cancel();
    }

    /// The current state, for the status view.
    #[must_use]
    pub fn status(&self) -> NetworkStatus {
        let st = self.inner.state();
        let policy = st.policy();
        NetworkStatus {
            mode: st.config.mode.kind(),
            state: st.conn.clone(),
            endpoint: st.config.mode.endpoint(),
            traffic_allowed: policy.traffic_allowed,
            kill_switch_engaged: policy.protected && !policy.traffic_allowed,
            udp_allowed: policy.bound_udp_allowed,
            trackers_allowed: policy.trackers_allowed,
            last_check_ms: st.last_check.map(unix_ms),
            connected_since_ms: st
                .connected_since
                .filter(|_| st.conn == ConnectionState::Connected)
                .map(unix_ms),
            events: st
                .events
                .iter()
                .skip(st.events.len().saturating_sub(STATUS_EVENTS))
                .cloned()
                .collect(),
            incoming: st.incoming.status.clone(),
        }
    }

    /// What connections may do right now.
    #[must_use]
    pub fn policy(&self) -> Policy {
        *self.inner.policy.borrow()
    }

    /// Receives every policy change.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Policy> {
        self.inner.policy.subscribe()
    }

    /// Notified whenever [`Network::status`] may have changed: every policy change, and changes
    /// of the incoming-connection listener.
    #[must_use]
    pub fn status_changes(&self) -> watch::Receiver<u64> {
        self.inner.status_seq.subscribe()
    }

    /// Waits until the state is no longer [`ConnectionState::Connecting`], or `timeout` passes,
    /// and returns the status.
    pub async fn settled(&self, timeout: Duration) -> NetworkStatus {
        let mut rx = self.subscribe();
        let _ = tokio::time::timeout(timeout, async {
            loop {
                if self.inner.state().conn != ConnectionState::Connecting {
                    return;
                }
                if rx.changed().await.is_err() {
                    return;
                }
            }
        })
        .await;
        self.status()
    }

    /// Opens a TCP connection to `target` through the current mode.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::KillSwitch`] while traffic is blocked, and connection errors
    /// otherwise. Never connects any other way than the selected mode.
    pub async fn connect(&self, target: SocketAddr) -> Result<(TcpStream, Lease), NetError> {
        let (route, lease, ipv4_only) = self.route()?;
        let stream = self.dial(&route, target, ipv4_only, &lease).await?;
        Self::check_lease(stream, lease)
    }

    /// Opens a TCP connection to `host:port` through the current mode. Through a SOCKS5 proxy
    /// with remote DNS the proxy resolves the name; otherwise the system resolver does.
    ///
    /// # Errors
    ///
    /// As for [`Network::connect`], plus name resolution failures.
    pub async fn connect_host(
        &self,
        host: &str,
        port: u16,
    ) -> Result<(TcpStream, Lease), NetError> {
        self.connect_host_as(host, port, false).await
    }

    /// Like [`Network::connect_host`], but only to public addresses ([`is_public_ip`]), for
    /// destinations from untrusted sources such as a torrent's web seeds: a literal address on
    /// the user's network is refused, `localhost` names are refused, and a name is used only
    /// through the public addresses it resolves to. Through a SOCKS5 proxy that resolves names
    /// itself, other names are passed on unchecked: they resolve on the proxy's network.
    ///
    /// # Errors
    ///
    /// As for [`Network::connect_host`], plus [`NetError::NotPublic`].
    pub async fn connect_host_public(
        &self,
        host: &str,
        port: u16,
    ) -> Result<(TcpStream, Lease), NetError> {
        self.connect_host_as(host, port, true).await
    }

    /// Opens a UDP socket for talking to `target` under the current mode: an ordinary socket in
    /// direct mode. WireGuard sockets stay inside the tunnel; SOCKS5 uses a UDP ASSOCIATE relay
    /// with a live TCP control connection.
    /// The returned lease ends when the mode changes or the kill switch engages; check it
    /// before sending.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::KillSwitch`] while traffic is blocked, relay negotiation failures
    /// for proxies without UDP support, and socket errors otherwise.
    pub async fn udp_socket(
        &self,
        target: SocketAddr,
    ) -> Result<(crate::DatagramSocket, Lease), NetError> {
        let (route, lease, ipv4_only) = self.route()?;
        if ipv4_only && target.is_ipv6() {
            return Err(NetError::Ipv6Disabled);
        }
        let socket = match &route {
            Route::Direct => {
                let any = if target.is_ipv4() {
                    SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
                } else {
                    SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0))
                };
                std::net::UdpSocket::bind(any)?
            }
            Route::WireGuard(tunnel) => {
                let socket = tunnel.udp_socket(target).await?;
                if !lease.is_valid() {
                    return Err(NetError::KillSwitch(
                        "the network settings changed while opening UDP".into(),
                    ));
                }
                return Ok((socket.ipv4_only(ipv4_only).with_lease(lease.clone()), lease));
            }
            Route::Socks(cfg) => {
                let socket = tokio::time::timeout(
                    self.inner.options.connect_timeout,
                    crate::socks_udp::SocksUdp::open(cfg, lease.clone()),
                )
                .await
                .map_err(|_| NetError::Timeout)??;
                if !lease.is_valid() {
                    return Err(NetError::KillSwitch(
                        "the network settings changed while opening UDP".into(),
                    ));
                }
                return Ok((
                    crate::DatagramSocket::socks(socket)
                        .ipv4_only(ipv4_only)
                        .with_lease(lease.clone()),
                    lease,
                ));
            }
        };
        socket.set_nonblocking(true)?;
        if !lease.is_valid() {
            return Err(NetError::KillSwitch(
                "the network settings changed while opening UDP".into(),
            ));
        }
        Ok((
            crate::DatagramSocket::from(UdpSocket::from_std(socket)?)
                .ipv4_only(ipv4_only)
                .with_lease(lease.clone()),
            lease,
        ))
    }

    /// Opens a datagram path to a host, retaining remote DNS in SOCKS5 mode.
    ///
    /// # Errors
    /// Returns route, resolution, or relay errors without falling back to direct UDP or DNS.
    pub async fn udp_socket_host(
        &self,
        host: &str,
        port: u16,
    ) -> Result<(crate::DatagramSocket, Target, Lease), NetError> {
        let (route, original, _) = self.route()?;
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        let target = if let Ok(ip) = bare.parse::<IpAddr>() {
            Target::Addr(SocketAddr::new(ip, port))
        } else if matches!(route, Route::Socks(ref cfg) if cfg.remote_dns) {
            Target::Domain(bare.to_owned(), port)
        } else {
            let addr = self
                .resolve_host(bare, port)
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| {
                    NetError::Io(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "no usable host address",
                    ))
                })?;
            Target::Addr(addr)
        };
        let family = match &target {
            Target::Addr(addr) => *addr,
            Target::Domain(_, _) => SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
        };
        let (socket, lease) = self.udp_socket(family).await?;
        if !original.is_valid() {
            return Err(NetError::KillSwitch(
                "the network settings changed during UDP setup".into(),
            ));
        }
        Ok((socket, target, lease))
    }

    /// Resolves a host through the selected mode, including tunneled DNS in WireGuard.
    ///
    /// # Errors
    ///
    /// Refuses lookups while the kill switch is active. With SOCKS5 remote DNS, use
    /// [`Network::udp_socket_host`] instead: it retains the domain in the relay envelope.
    pub async fn resolve_host(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, NetError> {
        let (route, lease, ipv4_only) = self.route()?;
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        let addresses = match (&route, bare.parse::<IpAddr>()) {
            (_, Ok(ip)) => vec![SocketAddr::new(ip, port)],
            (Route::Socks(cfg), Err(_)) if cfg.remote_dns => {
                return Err(NetError::Unsupported(
                    "use a SOCKS5 domain datagram for remote DNS".into(),
                ));
            }
            (Route::WireGuard(tunnel), Err(_)) => tunnel.resolve(bare, port, ipv4_only).await?,
            (_, Err(_)) => tokio::net::lookup_host((bare, port)).await?.collect(),
        };
        if !lease.is_valid() {
            return Err(NetError::KillSwitch(
                "the network settings changed during DNS lookup".into(),
            ));
        }
        Ok(addresses
            .into_iter()
            .filter(|addr| !ipv4_only || addr.is_ipv4())
            .collect())
    }

    async fn connect_host_as(
        &self,
        host: &str,
        port: u16,
        public_only: bool,
    ) -> Result<(TcpStream, Lease), NetError> {
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        if let Ok(ip) = bare.parse::<IpAddr>() {
            if public_only && !is_public_ip(ip) {
                return Err(NetError::NotPublic);
            }
            return self.connect(SocketAddr::new(ip, port)).await;
        }
        if public_only {
            let name = bare.trim_end_matches('.').to_ascii_lowercase();
            if name == "localhost" || name.ends_with(".localhost") {
                return Err(NetError::NotPublic);
            }
        }
        let (route, lease, ipv4_only) = self.route()?;
        if let Route::Socks(cfg) = &route
            && cfg.remote_dns
        {
            let stream = self
                .socks_dial(cfg, &Target::Domain(host.to_owned(), port))
                .await?;
            return Self::check_lease(stream, lease);
        }
        let mut last = None;
        let addresses = match &route {
            Route::WireGuard(tunnel) => tunnel.resolve(host, port, ipv4_only).await?,
            _ => self.system_lookup(host, port).await?,
        };
        for addr in addresses {
            if ipv4_only && addr.is_ipv6() {
                continue;
            }
            if public_only && !is_public_ip(addr.ip()) {
                last.get_or_insert(NetError::NotPublic);
                continue;
            }
            // The route was chosen before the lookup and the previous attempts, which can
            // take seconds: a direct route must not dial once the mode is protected.
            if !lease.is_valid() {
                return Err(NetError::KillSwitch(
                    "the network settings changed while connecting".into(),
                ));
            }
            match self.dial(&route, addr, ipv4_only, &lease).await {
                Ok(stream) => return Self::check_lease(stream, lease),
                Err(error) => last = Some(error),
            }
        }
        Err(last.unwrap_or_else(|| {
            NetError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no usable address for the host",
            ))
        }))
    }

    /// Resolves `host` with the system resolver.
    async fn system_lookup(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
        #[cfg(test)]
        {
            let delay = *self
                .inner
                .resolve_delay
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            tokio::time::sleep(delay).await;
        }
        Ok(tokio::net::lookup_host((host, port)).await?.collect())
    }

    pub(crate) fn route(&self) -> Result<(Route, Lease, bool), NetError> {
        let st = self.inner.state();
        if !st.conn.allows_traffic() {
            return Err(st.blocked());
        }
        let route = match &st.config.mode {
            NetworkMode::Direct => Route::Direct,
            NetworkMode::Socks5(c) => Route::Socks(c.clone()),
            NetworkMode::WireGuard(_) => {
                Route::WireGuard(st.tunnel.clone().ok_or_else(|| st.blocked())?)
            }
        };
        let lease = Lease {
            rx: self.inner.policy.subscribe(),
            epoch: st.epoch,
        };
        let ipv4_only = st.config.mode.is_protected() && !st.config.allow_ipv6;
        Ok((route, lease, ipv4_only))
    }

    async fn dial(
        &self,
        route: &Route,
        target: SocketAddr,
        ipv4_only: bool,
        lease: &Lease,
    ) -> Result<TcpStream, NetError> {
        if ipv4_only && target.is_ipv6() {
            return Err(NetError::Ipv6Disabled);
        }
        match route {
            Route::Direct => Ok(TcpStream::connect(target).await?),
            Route::Socks(cfg) => self.socks_dial(cfg, &Target::Addr(target)).await,
            Route::WireGuard(tunnel) => tunnel.connect(target, Some(lease.clone())).await,
        }
    }

    async fn socks_dial(&self, cfg: &Socks5Config, target: &Target) -> Result<TcpStream, NetError> {
        let mut stream = match TcpStream::connect((cfg.host.trim(), cfg.port)).await {
            Ok(s) => s,
            Err(error) => {
                // Probably the proxy went down: check now rather than at the next interval.
                self.inner.wake.notify_one();
                return Err(NetError::ProxyUnreachable(error));
            }
        };
        stream.set_nodelay(true)?;
        socks5::connect(&mut stream, target, cfg.credentials()).await?;
        Ok(stream)
    }

    /// A connection that finished after the epoch changed went out under old settings: drop it.
    fn check_lease(stream: TcpStream, lease: Lease) -> Result<(TcpStream, Lease), NetError> {
        if lease.is_valid() {
            Ok((stream, lease))
        } else {
            drop(stream);
            Err(NetError::KillSwitch(
                "the network settings changed while connecting".into(),
            ))
        }
    }

    /// Routes a `reqwest` client through the current mode, via TorrentPlayer's loopback bridge. The
    /// client never needs rebuilding: the bridge follows mode changes, closes its connections
    /// when the epoch changes, and refuses everything while the kill switch is engaged. Host
    /// names are passed to the bridge unresolved (`socks5h`).
    ///
    /// # Errors
    ///
    /// Returns an error if `reqwest` rejects the proxy settings.
    pub fn configure_http(
        &self,
        builder: reqwest::ClientBuilder,
    ) -> Result<reqwest::ClientBuilder, NetError> {
        let url = self.bridge_url(bridge::USER, "socks5h");
        let proxy = reqwest::Proxy::all(url.as_str())
            .map_err(|e| NetError::Http(format!("could not configure the HTTP proxy: {e}")))?;
        Ok(builder.proxy(proxy))
    }

    /// Like [`Network::configure_http`], but the bridge only connects to public addresses (see
    /// [`Network::connect_host_public`]). Use it for URLs from untrusted sources, such as a
    /// torrent's web seeds, and also check each URL (and redirect) with [`is_public_ip`] where
    /// its host is a literal address.
    ///
    /// # Errors
    ///
    /// Returns an error if `reqwest` rejects the proxy settings.
    pub fn configure_http_public(
        &self,
        builder: reqwest::ClientBuilder,
    ) -> Result<reqwest::ClientBuilder, NetError> {
        let url = self.bridge_url(bridge::PUBLIC_USER, "socks5h");
        let proxy = reqwest::Proxy::all(url.as_str())
            .map_err(|e| NetError::Http(format!("could not configure the HTTP proxy: {e}")))?;
        Ok(builder.proxy(proxy))
    }

    /// The bridge as a `socks5://user:token@127.0.0.1:port` URL, for librqbit's
    /// `ConnectionOptions::proxy_url`. The token keeps other local programs from using the
    /// bridge; do not log it.
    #[must_use]
    pub fn bridge_proxy_url(&self) -> Secret {
        Secret::new(self.bridge_url(bridge::USER, "socks5").as_str())
    }

    fn bridge_url(&self, user: &str, scheme: &str) -> Zeroizing<String> {
        Zeroizing::new(format!(
            "{scheme}://{user}:{}@{}",
            self.inner.bridge_token.expose(),
            self.inner.bridge_addr
        ))
    }

    /// Asks [`NetworkOptions::ip_check_v4_url`] and [`NetworkOptions::ip_check_v6_url`] (by
    /// default icanhazip.com) for the public IPv4 and IPv6 addresses, at the same time and
    /// through the current mode. Either may be missing (a connection without IPv6, or a VPN
    /// that carries only IPv4). Never makes a direct request while a protected mode is
    /// selected: if the kill switch is engaged, it fails without sending anything.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::KillSwitch`] while traffic is blocked, and [`NetError::Http`] if
    /// neither address could be found.
    pub async fn check_public_ip(&self) -> Result<PublicIp, NetError> {
        let mode = {
            let st = self.inner.state();
            if !st.conn.allows_traffic() {
                return Err(st.blocked());
            }
            st.config.mode.kind()
        };
        let options = &self.inner.options;
        let parse = |url: &str| {
            Url::parse(url)
                .map_err(|_| NetError::InvalidConfig("the connection-check URL is invalid".into()))
        };
        let v4_url = parse(&options.ip_check_v4_url)?;
        let v6_url = options.ip_check_v6_url.as_deref().map(parse).transpose()?;
        let client = self
            .configure_http(reqwest::Client::builder())?
            .connect_timeout(options.connect_timeout)
            .timeout(options.connect_timeout + Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| NetError::Http(format!("connection check failed: {}", e.without_url())))?;
        let (v4, v6) = tokio::join!(ask_ip(&client, &v4_url), async {
            match &v6_url {
                Some(url) => Some(ask_ip(&client, url).await),
                None => None,
            }
        });
        let ipv4 = v4
            .as_ref()
            .ok()
            .filter(|ip| ip.is_ipv4())
            .map(ToString::to_string);
        let ipv6 = v6
            .and_then(Result::ok)
            .filter(IpAddr::is_ipv6)
            .map(|ip| ip.to_string());
        let Some(ip) = ipv4.clone().or_else(|| ipv6.clone()) else {
            return Err(v4.err().unwrap_or_else(|| {
                NetError::Http("the connection check sent an address of the wrong kind".into())
            }));
        };
        Ok(PublicIp {
            ip,
            ipv4,
            ipv6,
            mode,
            service: v4_url
                .host_str()
                .map(|h| h.strip_prefix("ipv4.").unwrap_or(h))
                .unwrap_or_default()
                .to_owned(),
            checked_at_ms: unix_ms(SystemTime::now()),
        })
    }
}

/// One connection-check request: the address in a plain-text answer (`203.0.113.7\n`) or a
/// JSON one (`{"ip": "203.0.113.7"}`).
async fn ask_ip(client: &reqwest::Client, url: &Url) -> Result<IpAddr, NetError> {
    let http =
        |e: reqwest::Error| NetError::Http(format!("connection check failed: {}", e.without_url()));
    let mut response = client
        .get(url.clone())
        .send()
        .await
        .map_err(http)?
        .error_for_status()
        .map_err(http)?;
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(http)? {
        if body.len() + chunk.len() > MAX_IP_RESPONSE {
            return Err(NetError::Http(
                "the connection check sent too much data".into(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    parse_ip_answer(&body)
        .ok_or_else(|| NetError::Http("the connection check sent an invalid address".into()))
}

/// The address in a connection-check answer, plain text or `{"ip": …}`.
pub(crate) fn parse_ip_answer(body: &[u8]) -> Option<IpAddr> {
    let text = std::str::from_utf8(body).ok()?.trim();
    if text.starts_with('{') {
        let answer: IpAnswer = serde_json::from_str(text).ok()?;
        answer.ip.trim().parse().ok()
    } else {
        text.parse().ok()
    }
}

/// A JSON connection-check answer: `{"ip": "203.0.113.7"}`.
#[derive(serde::Deserialize)]
struct IpAnswer {
    ip: String,
}

/// Checks the selected mode every [`NetworkOptions::health_interval`], and immediately after
/// a change or a failed connection.
async fn monitor(inner: Weak<Inner>, wake: Arc<Notify>, cancel: CancellationToken) {
    loop {
        let Some(net) = inner.upgrade() else {
            return;
        };
        let (generation, mode, idle) = {
            let st = net.state();
            let idle = matches!(
                st.conn,
                ConnectionState::Error { .. } | ConnectionState::Disconnected
            );
            (st.generation, st.config.mode.clone(), idle)
        };
        let timeout = net.options.check_timeout;
        let result = match mode {
            _ if idle => None,
            NetworkMode::Direct => None,
            NetworkMode::Socks5(cfg) => Some(probe(&cfg, timeout).await),
            NetworkMode::WireGuard(cfg) => {
                let check = async {
                    let existing = net.state().tunnel.clone();
                    let tunnel = if let Some(tunnel) =
                        existing.as_ref().filter(|tunnel| !tunnel.is_closed())
                    {
                        tunnel.clone()
                    } else {
                        // A tunnel closed because its server stopped answering is replaced by
                        // one that must hear from the server before traffic resumes.
                        let unproven = existing.as_ref().is_some_and(Tunnel::abandoned);
                        let tunnel = Tunnel::start(&cfg, unproven).await?;
                        let mut st = net.state();
                        if st.generation != generation || st.conn == ConnectionState::Disconnected {
                            tunnel.close();
                            return Err(NetError::KillSwitch("network settings changed".into()));
                        }
                        st.tunnel = Some(tunnel.clone());
                        tunnel
                    };
                    tunnel
                        .healthy(
                            timeout,
                            net.options.wireguard_reply_timeout,
                            net.options.wireguard_restart_after,
                        )
                        .await
                };
                // Mostly a handshake that did not complete in time: one reason for both, so a
                // tunnel that stays down keeps one state instead of logging each alternation.
                Some(
                    tokio::time::timeout(timeout, check)
                        .await
                        .unwrap_or_else(|_| {
                            Err(NetError::InterfaceUnavailable(
                                crate::tunnel::NO_HANDSHAKE.into(),
                            ))
                        }),
                )
            }
        };
        let delay = result.map(|r| net.on_check(generation, r));
        drop(net);
        let sleep = async {
            match delay {
                Some(d) => tokio::time::sleep(d).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            () = cancel.cancelled() => return,
            () = wake.notified() => {}
            () = sleep => {}
        }
    }
}

/// Connects to the proxy and authenticates, without asking it to connect anywhere.
async fn probe(cfg: &Socks5Config, timeout: Duration) -> Result<(), NetError> {
    tokio::time::timeout(timeout, async {
        let mut stream = TcpStream::connect((cfg.host.trim(), cfg.port))
            .await
            .map_err(NetError::ProxyUnreachable)?;
        socks5::negotiate(&mut stream, cfg.credentials()).await?;
        Ok(())
    })
    .await
    .map_err(|_| NetError::Timeout)?
}
