// SPDX-License-Identifier: MIT OR Apache-2.0

//! An application-local WireGuard tunnel. Only encrypted UDP reaches the OS network.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio_wireguard::{Interface, config, x25519};
use zeroize::Zeroizing;

use crate::{NetError, WireGuardConfig};

/// WireGuard's `REJECT_AFTER_TIME`: a session older than this is unusable.
const REJECT_AFTER_TIME: Duration = Duration::from_secs(180);
/// Why a tunnel without a current session is not connected.
pub(crate) const NO_HANDSHAKE: &str =
    "the WireGuard server has not completed a recent handshake; check the endpoint and keys";
/// Why a tunnel whose server stopped answering traffic is not connected.
const NOT_ANSWERING: &str =
    "the WireGuard server is not answering tunnel traffic; check the endpoint and keys";

#[derive(Clone, Debug)]
pub(crate) struct Tunnel(Arc<Session>);

#[derive(Debug)]
struct Session {
    interface: Interface,
    dns: Vec<IpAddr>,
    /// Where health probes go (see [`Tunnel::probe`]); None without a tunnel address in the
    /// family of any `AllowedIPs` range.
    probe_target: Option<ProbeTarget>,
    probes: Mutex<Probes>,
    /// Started because the server stopped answering the previous tunnel: until the server
    /// answers this one's traffic, it is not connected.
    unproven: bool,
    /// Closed because the server stopped answering it (see [`Tunnel::abandoned`]).
    abandoned: AtomicBool,
}

/// Where a health probe goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProbeTarget {
    /// The tunnel address it is sent from.
    local: IpAddr,
    remote: SocketAddr,
    /// A DNS server inside the tunnel, asked for the root name servers; otherwise an empty UDP
    /// datagram with a hop limit of 1.
    dns: bool,
}

/// The health probes of one tunnel.
#[derive(Debug, Default)]
struct Probes {
    /// One probe at a time.
    in_flight: bool,
    last: Option<Instant>,
    /// Probes sent since the tunnel was last in use or unanswered: the gap between idle probes
    /// doubles with each.
    idle: u32,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.interface.close();
    }
}

fn invalid(message: &str) -> NetError {
    NetError::InvalidConfig(message.into())
}

fn key(text: &str) -> Result<Zeroizing<[u8; 32]>, NetError> {
    let mut bytes = Zeroizing::new([0; 32]);
    let length = base64::engine::general_purpose::STANDARD
        .decode_slice(text, bytes.as_mut())
        .map_err(|_| {
            invalid("the WireGuard key is invalid or missing from the credential store")
        })?;
    if length != 32 || bytes.iter().all(|b| *b == 0) {
        return Err(invalid(
            "the WireGuard key is invalid or missing from the credential store",
        ));
    }
    Ok(bytes)
}

pub(crate) fn validate(config: &WireGuardConfig) -> Result<(), NetError> {
    key(config.interface.private_key.expose())?;
    config
        .interface
        .addresses
        .join(",")
        .parse::<config::Address>()
        .map_err(|_| invalid("WireGuard needs one IPv4 and/or one IPv6 tunnel address"))?;
    let [peer] = config.peers.as_slice() else {
        return Err(invalid(
            "import a WireGuard configuration with exactly one peer",
        ));
    };
    key(&peer.public_key)?;
    if let Some(psk) = &peer.preshared_key {
        key(psk.expose())?;
    }
    if peer.endpoint.is_empty() || peer.allowed_ips.is_empty() {
        return Err(invalid("WireGuard needs a server endpoint and AllowedIPs"));
    }
    for range in &peer.allowed_ips {
        // The stack uses ipnet internally; parsing through the peer builder below enforces
        // this again before any socket is opened.
        let (_, prefix) = range
            .split_once('/')
            .ok_or_else(|| invalid("invalid WireGuard AllowedIPs"))?;
        let ip: IpAddr = range
            .split('/')
            .next()
            .unwrap_or_default()
            .parse()
            .map_err(|_| invalid("invalid WireGuard AllowedIPs"))?;
        let prefix: u8 = prefix
            .parse()
            .map_err(|_| invalid("invalid WireGuard AllowedIPs"))?;
        if prefix > if ip.is_ipv4() { 32 } else { 128 } {
            return Err(invalid("invalid WireGuard AllowedIPs"));
        }
    }
    if config
        .interface
        .dns
        .iter()
        .any(|ip| ip.parse::<IpAddr>().is_err())
    {
        return Err(invalid("WireGuard DNS servers must be IP addresses"));
    }
    if config
        .interface
        .mtu
        .is_some_and(|mtu| !(576..=9000).contains(&mtu))
    {
        return Err(invalid("WireGuard MTU must be between 576 and 9000"));
    }
    Ok(())
}

impl Tunnel {
    /// Starts a tunnel. `unproven`: it replaces one whose server stopped answering (see
    /// [`Tunnel::abandoned`]), so it counts as connected only once the server answers it.
    pub(crate) async fn start(config: &WireGuardConfig, unproven: bool) -> Result<Self, NetError> {
        validate(config)?;
        let peer = &config.peers[0];
        let endpoint = tokio::net::lookup_host(&peer.endpoint)
            .await?
            .next()
            .ok_or_else(|| invalid("the WireGuard endpoint did not resolve"))?;
        let interface = Interface::new(config::Config {
            interface: config::Interface {
                address: config
                    .interface
                    .addresses
                    .join(",")
                    .parse()
                    .map_err(|_| invalid("invalid WireGuard tunnel address"))?,
                private_key: x25519::StaticSecret::from(*key(config
                    .interface
                    .private_key
                    .expose())?),
                listen_port: config.interface.listen_port,
                mtu: config.interface.mtu.map(usize::from),
            },
            peers: vec![config::Peer {
                public_key: x25519::PublicKey::from(*key(&peer.public_key)?),
                preshared_key: peer
                    .preshared_key
                    .as_ref()
                    .map(|psk| key(psk.expose()).map(|key| x25519::StaticSecret::from(*key)))
                    .transpose()?,
                endpoint: Some(endpoint),
                allowed_ips: peer
                    .allowed_ips
                    .iter()
                    .map(|ip| {
                        ip.parse()
                            .map_err(|_| invalid("invalid WireGuard AllowedIPs"))
                    })
                    .collect::<Result<_, _>>()?,
                persistent_keepalive: Some(
                    peer.persistent_keepalive.filter(|n| *n > 0).unwrap_or(25),
                ),
            }],
        })?;
        let dns: Vec<IpAddr> = config
            .interface
            .dns
            .iter()
            .map(|ip| {
                ip.parse()
                    .map_err(|_| invalid("invalid WireGuard DNS server"))
            })
            .collect::<Result<_, _>>()?;
        let allowed = peer
            .allowed_ips
            .iter()
            .map(|range| network(range).ok_or_else(|| invalid("invalid WireGuard AllowedIPs")))
            .collect::<Result<Vec<_>, _>>()?;
        let locals: Vec<IpAddr> = interface.address().addresses().collect();
        Ok(Self(Arc::new(Session {
            probe_target: probe_target(&locals, &allowed, &dns),
            interface,
            dns,
            probes: Mutex::default(),
            unproven,
            abandoned: AtomicBool::new(false),
        })))
    }

    /// Whether the health check closed this tunnel because its server stopped answering, so
    /// its replacement starts unproven.
    pub(crate) fn abandoned(&self) -> bool {
        self.0.abandoned.load(Ordering::Relaxed)
    }

    pub(crate) fn close(&self) {
        self.0.interface.close();
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.0.interface.is_closed()
    }

    pub(crate) async fn udp_socket(
        &self,
        target: SocketAddr,
    ) -> Result<crate::DatagramSocket, NetError> {
        let local = self
            .0
            .interface
            .address()
            .addresses()
            .find(|ip| ip.is_ipv4() == target.is_ipv4())
            .ok_or_else(|| invalid("the WireGuard tunnel has no address for this IP family"))?;
        let socket =
            tokio_wireguard::UdpSocket::bind(SocketAddr::new(local, 0), &self.0.interface).await?;
        Ok(crate::DatagramSocket::wireguard(socket))
    }

    /// Whether the server is answering (see `docs/network.md`, "WireGuard health"):
    ///
    /// - Traffic sent into the tunnel must get an authenticated packet back within
    ///   `reply_timeout`. A WireGuard peer that receives data answers within `KEEPALIVE_TIMEOUT`
    ///   (10 s), with data or an empty keepalive, so silence means the server is not answering,
    ///   even if it still completes handshakes. While it is not answering, the tunnel is probed
    ///   so the check sees the answer as soon as there is one; after `restart_after` without an
    ///   answer, the tunnel is closed so that a new one replaces it.
    /// - Without a current session (none yet, or none for `REJECT_AFTER_TIME`), a handshake must
    ///   complete within `timeout`.
    /// - A tunnel that has heard nothing for half of `reply_timeout` is probed (see
    ///   [`Tunnel::probe`]), since WireGuard is silent while idle. The gap between probes of an
    ///   idle tunnel doubles with each, up to four times `reply_timeout`.
    pub(crate) async fn healthy(
        &self,
        timeout: Duration,
        reply_timeout: Duration,
        restart_after: Duration,
    ) -> Result<(), NetError> {
        let liveness = self.0.interface.liveness().await?;
        let session = liveness
            .handshake_age
            .is_some_and(|age| age < REJECT_AFTER_TIME);
        let unanswered = liveness.unanswered.unwrap_or_default();
        if unanswered >= reply_timeout || (self.0.unproven && liveness.received_age.is_none()) {
            if unanswered >= restart_after {
                // A fresh interface starts over with a new handshake (and source port). Each
                // tunnel must go unanswered this long first, so restarts are as far apart.
                self.0.abandoned.store(true, Ordering::Relaxed);
                self.close();
            } else {
                // Keep asking, so the check sees the server answer again as soon as it does.
                if !session {
                    self.0.interface.request_handshake()?;
                }
                self.probes().idle = 0;
                self.probe(reply_timeout);
            }
            return Err(NetError::InterfaceUnavailable(NOT_ANSWERING.into()));
        }
        if !session {
            return self.handshake(timeout).await;
        }
        let quiet = [liveness.received_age, liveness.handshake_age]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(Duration::MAX);
        if quiet >= reply_timeout / 2 {
            let mut probes = self.probes();
            if let (Some(last), Some(received)) = (probes.last, liveness.received_age) {
                // An answer to the last probe arrives within `reply_timeout`; anything later is
                // the application's traffic.
                if last.elapsed() > received + reply_timeout {
                    probes.idle = 0;
                }
            }
            let gap = reply_timeout / 2 * 2u32.pow(probes.idle.min(IDLE_PROBE_DOUBLINGS));
            if probes.last.is_none_or(|last| last.elapsed() >= gap) {
                probes.idle += 1;
                drop(probes);
                self.probe(reply_timeout);
            }
        }
        Ok(())
    }

    fn probes(&self) -> MutexGuard<'_, Probes> {
        self.0.probes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits up to `timeout` for a new handshake.
    async fn handshake(&self, timeout: Duration) -> Result<(), NetError> {
        tokio::time::timeout(timeout, async {
            let mut requested = false;
            loop {
                if self
                    .0
                    .interface
                    .handshake_age()
                    .await?
                    .is_some_and(|age| age < REJECT_AFTER_TIME)
                {
                    return Ok(());
                }
                if !requested {
                    self.0.interface.request_handshake()?;
                    requested = true;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .map_err(|_| NetError::InterfaceUnavailable(NO_HANDSHAKE.into()))?
    }

    /// Sends one tiny packet into the tunnel in the background, unless a probe is still in
    /// flight, and keeps its socket for up to `wait` so an answer has somewhere to go. The
    /// server's WireGuard answers any data packet within `KEEPALIVE_TIMEOUT`, with a keepalive if
    /// nothing else, wherever the packet is addressed, so the probe only has to be routed to the
    /// server by `AllowedIPs` (see [`probe_target`]): a query for the root name servers (answered
    /// from any resolver's cache) to the tunnel's DNS server, or else an empty UDP datagram with
    /// a hop limit of 1 to the discard port, which the server does not forward.
    fn probe(&self, wait: Duration) {
        let Some(target) = self.0.probe_target else {
            return;
        };
        {
            let mut probes = self.probes();
            if probes.in_flight {
                return;
            }
            probes.in_flight = true;
            probes.last = Some(Instant::now());
        }
        let tunnel = self.clone();
        tokio::spawn(async move {
            let _ = tokio::time::timeout(wait, async {
                let mut socket = tokio_wireguard::UdpSocket::bind(
                    SocketAddr::new(target.local, 0),
                    &tunnel.0.interface,
                )
                .await?;
                if target.dns {
                    socket.send_to(&root_query()?, target.remote).await?;
                } else {
                    socket.set_ttl(1)?;
                    socket.send_to(&[], target.remote).await?;
                }
                let mut response = [0u8; 512];
                socket.recv_from(&mut response[..]).await?;
                Ok::<_, NetError>(())
            })
            .await;
            tunnel.probes().in_flight = false;
        });
    }

    /// Connects to `target` inside the tunnel (see [`Tunnel::relay`]).
    pub(crate) async fn connect(&self, target: SocketAddr) -> Result<TcpStream, NetError> {
        let upstream = tokio_wireguard::TcpStream::connect(target, &self.0.interface).await?;
        self.relay(upstream).await
    }

    /// Listens for incoming connections on the tunnel's own addresses (IPv4, and IPv6 unless
    /// `ipv4_only`), inside the tunnel only. Peers reach them only if the VPN server forwards
    /// the port to this tunnel address.
    pub(crate) async fn listen(
        &self,
        port: u16,
        ipv4_only: bool,
    ) -> Result<Vec<tokio_wireguard::TcpListener>, NetError> {
        let mut listeners = Vec::new();
        for ip in self.0.interface.address().addresses() {
            if ipv4_only && ip.is_ipv6() {
                continue;
            }
            listeners.push(
                tokio_wireguard::TcpListener::bind(SocketAddr::new(ip, port), &self.0.interface)
                    .await?,
            );
        }
        if listeners.is_empty() {
            return Err(invalid("the WireGuard tunnel has no address to listen on"));
        }
        Ok(listeners)
    }

    /// Preserves the connector's OS `TcpStream` API for a tunnel connection using a private
    /// loopback pair, so peer, lease, and HTTP handling stay unchanged. The accepting side
    /// checks the exact source port we reserved, so another local process cannot take it.
    pub(crate) async fn relay(
        &self,
        mut upstream: tokio_wireguard::TcpStream,
    ) -> Result<TcpStream, NetError> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let client = TcpSocket::new_v4()?;
        client.bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
        let expected = client.local_addr()?;
        let accept = async {
            loop {
                let (stream, source) = listener.accept().await?;
                if source == expected {
                    return Ok::<_, std::io::Error>(stream);
                }
            }
        };
        let (client, mut relay) = tokio::try_join!(client.connect(listener.local_addr()?), accept)?;
        let session = self.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = session.0.interface.closed() => {}
                _ = tokio::io::copy_bidirectional(&mut relay, &mut upstream) => {}
            }
        });
        Ok(client)
    }

    pub(crate) async fn resolve(
        &self,
        host: &str,
        port: u16,
        ipv4_only: bool,
    ) -> Result<Vec<SocketAddr>, NetError> {
        if self.0.dns.is_empty() {
            return Err(invalid(
                "this WireGuard configuration needs a DNS server to resolve host names",
            ));
        }
        let mut addresses = Vec::new();
        for &server in &self.0.dns {
            for kind in [dns_parser::QueryType::A, dns_parser::QueryType::AAAA] {
                if ipv4_only && kind == dns_parser::QueryType::AAAA {
                    continue;
                }
                if let Ok(Ok(found)) =
                    tokio::time::timeout(Duration::from_secs(3), self.query(host, server, kind))
                        .await
                {
                    addresses.extend(found.into_iter().map(|ip| SocketAddr::new(ip, port)));
                }
            }
            if !addresses.is_empty() {
                return Ok(addresses);
            }
        }
        Err(NetError::InterfaceUnavailable(
            "DNS lookup through the WireGuard tunnel failed".into(),
        ))
    }

    async fn query(
        &self,
        host: &str,
        server: IpAddr,
        kind: dns_parser::QueryType,
    ) -> Result<Vec<IpAddr>, NetError> {
        let query = dns_query(host, kind)?;
        let local = self
            .0
            .interface
            .address()
            .addresses()
            .find(|ip| ip.is_ipv4() == server.is_ipv4())
            .ok_or_else(|| {
                invalid("the WireGuard DNS server needs a matching tunnel address family")
            })?;
        let socket =
            tokio_wireguard::UdpSocket::bind(SocketAddr::new(local, 0), &self.0.interface).await?;
        socket.send_to(&query, SocketAddr::new(server, 53)).await?;
        let mut response = [0u8; 4096];
        loop {
            let (length, source) = socket.recv_from(&mut response[..]).await?;
            if length < 12 || source != SocketAddr::new(server, 53) || response[..2] != query[..2] {
                continue;
            }
            let mut answer = response[..length].to_vec();
            let header = dns_parser::Packet::parse(&answer)
                .map_err(|_| invalid("the WireGuard DNS server sent an invalid response"))?;
            if header.header.truncated {
                // Large DNS answers retry over TCP inside the same tunnel.
                let mut stream = tokio_wireguard::TcpStream::connect(
                    SocketAddr::new(server, 53),
                    &self.0.interface,
                )
                .await?;
                stream
                    .write_u16(
                        u16::try_from(query.len()).map_err(|_| invalid("DNS query too large"))?,
                    )
                    .await?;
                stream.write_all(&query).await?;
                let size = usize::from(stream.read_u16().await?);
                if !(12..=16 * 1024).contains(&size) {
                    return Err(invalid(
                        "the WireGuard DNS response is too large or incomplete",
                    ));
                }
                answer.resize(size, 0);
                stream.read_exact(&mut answer).await?;
            }
            let packet = dns_parser::Packet::parse(&answer)
                .map_err(|_| invalid("the WireGuard DNS server sent an invalid response"))?;
            if packet.header.query
                || packet.header.truncated
                || answer[..2] != query[..2]
                || packet.header.response_code != dns_parser::ResponseCode::NoError
            {
                return Err(NetError::InterfaceUnavailable(
                    "the WireGuard DNS server could not resolve the host".into(),
                ));
            }
            return Ok(packet
                .answers
                .into_iter()
                .filter_map(|answer| match answer.data {
                    dns_parser::RData::A(record) => Some(IpAddr::V4(record.0)),
                    dns_parser::RData::AAAA(record) => Some(IpAddr::V6(record.0)),
                    _ => None,
                })
                .collect());
        }
    }
}

/// How often the gap between idle probes doubles: from half of the reply timeout to four times
/// the reply timeout.
const IDLE_PROBE_DOUBLINGS: u32 = 3;

/// An `AllowedIPs` range (`address/prefix`) as its network address and prefix length.
fn network(range: &str) -> Option<(IpAddr, u8)> {
    let (ip, prefix) = range.split_once('/')?;
    let ip: IpAddr = ip.parse().ok()?;
    let prefix: u8 = prefix.parse().ok()?;
    let (bits, width) = bits(ip);
    let host = u32::from(width.checked_sub(prefix)?);
    let network = bits
        .checked_shr(host)
        .unwrap_or(0)
        .checked_shl(host)
        .unwrap_or(0);
    Some((from_bits(network, ip.is_ipv4()), prefix))
}

/// An address as a number, and its width in bits.
fn bits(ip: IpAddr) -> (u128, u8) {
    match ip {
        IpAddr::V4(ip) => (u32::from(ip).into(), 32),
        IpAddr::V6(ip) => (ip.into(), 128),
    }
}

fn from_bits(bits: u128, v4: bool) -> IpAddr {
    if v4 {
        IpAddr::V4(Ipv4Addr::from(u32::try_from(bits).unwrap_or(u32::MAX)))
    } else {
        IpAddr::V6(Ipv6Addr::from(bits))
    }
}

fn contains((network, prefix): (IpAddr, u8), ip: IpAddr) -> bool {
    let ((net, width), (bits, family)) = (bits(network), bits(ip));
    let host = u32::from(width.saturating_sub(prefix));
    width == family && net.checked_shr(host).unwrap_or(0) == bits.checked_shr(host).unwrap_or(0)
}

/// Where the health probe goes: the first DNS server that the tunnel routes to the server (in
/// `AllowedIPs`, with a tunnel address in its family). Otherwise an address in `AllowedIPs`: a
/// documentation address (192.0.2.1, `2001:db8::1`) when a range such as `0.0.0.0/0` contains
/// one, since nothing routes those, or else the first host of the first range, which in a
/// split tunnel is often the server itself. Never one of the tunnel's own addresses, which
/// would not leave the tunnel.
fn probe_target(
    locals: &[IpAddr],
    allowed: &[(IpAddr, u8)],
    dns: &[IpAddr],
) -> Option<ProbeTarget> {
    let target = |remote: IpAddr, dns: bool| {
        if locals.contains(&remote) || !allowed.iter().any(|range| contains(*range, remote)) {
            return None;
        }
        let local = *locals.iter().find(|ip| ip.is_ipv4() == remote.is_ipv4())?;
        Some(ProbeTarget {
            local,
            remote: SocketAddr::new(remote, if dns { 53 } else { 9 }),
            dns,
        })
    };
    let documentation = [
        IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
        IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)),
    ];
    dns.iter()
        .find_map(|server| target(*server, true))
        .or_else(|| documentation.into_iter().find_map(|ip| target(ip, false)))
        .or_else(|| {
            allowed.iter().find_map(|&(network, _)| {
                let (bits, width) = bits(network);
                let first = from_bits(bits.saturating_add(1), width == 32);
                [first, network]
                    .into_iter()
                    .find_map(|ip| target(ip, false))
            })
        })
}

/// The header of a recursive query with one question and a random ID.
fn dns_header() -> Result<Vec<u8>, NetError> {
    let mut id = [0; 2];
    getrandom::fill(&mut id).map_err(|_| invalid("could not generate a DNS query ID"))?;
    Ok(vec![id[0], id[1], 1, 0, 0, 1, 0, 0, 0, 0, 0, 0])
}

/// A query for the root zone's name servers (the health probe).
fn root_query() -> Result<Vec<u8>, NetError> {
    let mut query = dns_header()?;
    query.push(0);
    query.extend_from_slice(&(dns_parser::QueryType::NS as u16).to_be_bytes());
    query.extend_from_slice(&1u16.to_be_bytes());
    Ok(query)
}

fn dns_query(host: &str, kind: dns_parser::QueryType) -> Result<Vec<u8>, NetError> {
    let name = host.trim_end_matches('.');
    if name.is_empty()
        || name.len() > 253
        || name
            .split('.')
            .any(|label| label.is_empty() || label.len() > 63 || !label.is_ascii())
    {
        return Err(invalid("the host name is invalid"));
    }
    // Build the small question directly: dns-parser's builder panics on 63-byte DNS labels.
    let mut query = dns_header()?;
    for label in name.split('.') {
        query.push(u8::try_from(label.len()).map_err(|_| invalid("the host name is invalid"))?);
        query.extend_from_slice(label.as_bytes());
    }
    query.push(0);
    query.extend_from_slice(&(kind as u16).to_be_bytes());
    query.extend_from_slice(&1u16.to_be_bytes());
    Ok(query)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wireguard::{WgInterface, WgPeer};
    use crate::{ConnectionState, Network, NetworkConfig, NetworkOptions, Secret};

    fn pair() -> (Interface, WireGuardConfig) {
        let (server_key, server_public) = x25519::keypair();
        let (client_key, client_public) = x25519::keypair();
        let (psk, _) = x25519::keypair();
        // The port is free when reserved, but a socket in a test running alongside can take it
        // before the server binds it (the server's socket is dual-stack): pick another then.
        let (server, port) = (0..20)
            .find_map(|_| {
                let reservation = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
                let port = reservation.local_addr().unwrap().port();
                drop(reservation);
                Interface::new(config::Config {
                    interface: config::Interface {
                        address: "10.44.0.1/32".parse().unwrap(),
                        listen_port: Some(port),
                        private_key: server_key.clone(),
                        mtu: None,
                    },
                    peers: vec![config::Peer {
                        endpoint: None,
                        allowed_ips: vec!["10.44.0.2/32".parse().unwrap()],
                        public_key: client_public,
                        preshared_key: Some(psk.clone()),
                        persistent_keepalive: None,
                    }],
                })
                .ok()
                .map(|server| (server, port))
            })
            .unwrap();
        let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
        let client = WireGuardConfig {
            interface: WgInterface {
                private_key: Secret::new(encode(client_key.as_bytes())),
                addresses: vec!["10.44.0.2/32".into()],
                dns: vec!["10.44.0.1".into()],
                mtu: None,
                listen_port: None,
            },
            peers: vec![WgPeer {
                public_key: encode(server_public.as_bytes()),
                preshared_key: Some(Secret::new(encode(psk.as_bytes()))),
                allowed_ips: vec!["0.0.0.0/0".into()],
                endpoint: format!("127.0.0.1:{port}"),
                persistent_keepalive: Some(1),
            }],
        };
        (server, client)
    }

    #[tokio::test]
    async fn authenticates_with_psk_and_transfers_tcp_then_closes_on_mode_change() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (server, config) = pair();
            let listener = tokio_wireguard::TcpListener::bind("10.44.0.1:8080", &server)
                .await
                .unwrap();
            let net = Network::start(
                NetworkConfig {
                    mode: crate::NetworkMode::WireGuard(config),
                    allow_ipv6: false,
                    hold: None,
                },
                NetworkOptions::default(),
            )
            .await
            .unwrap();
            assert_eq!(
                net.settled(Duration::from_secs(3)).await.state,
                ConnectionState::Connected
            );
            let task = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 4];
                stream.read_exact(&mut request).await.unwrap();
                assert_eq!(&request, b"ping");
                stream.write_all(b"pong").await.unwrap();
                let mut bulk = vec![0; 512 * 1024];
                stream.read_exact(&mut bulk).await.unwrap();
                assert!(bulk.iter().all(|byte| *byte == 0x5a));
                stream.write_all(&bulk).await.unwrap();
                let mut end = [0; 1];
                let _ = stream.read(&mut end).await;
            });
            let (mut stream, mut lease) = net
                .connect("10.44.0.1:8080".parse().unwrap())
                .await
                .unwrap();
            stream.write_all(b"ping").await.unwrap();
            let mut reply = [0; 4];
            stream.read_exact(&mut reply).await.unwrap();
            assert_eq!(&reply, b"pong");
            let bulk = vec![0x5a; 512 * 1024];
            stream.write_all(&bulk).await.unwrap();
            let mut response = vec![0; bulk.len()];
            stream.read_exact(&mut response).await.unwrap();
            assert_eq!(response, bulk);
            net.apply(NetworkConfig::default()).unwrap();
            lease.revoked().await;
            assert_eq!(stream.read(&mut reply).await.unwrap_or(0), 0);
            task.await.unwrap();
            net.shutdown();
            server.close();
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn incoming_connections_arrive_only_inside_the_tunnel() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let (server, config) = pair();
            let net = Network::start(
                NetworkConfig {
                    mode: crate::NetworkMode::WireGuard(config),
                    allow_ipv6: false,
                    hold: None,
                },
                NetworkOptions::default(),
            )
            .await
            .unwrap();
            let mut peers = net.incoming_peers();
            let port = {
                let probe = std::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
                probe.local_addr().unwrap().port()
            };
            net.set_incoming(crate::IncomingConfig {
                enabled: true,
                port,
                upnp: true,
            });
            assert_eq!(
                net.settled(Duration::from_secs(3)).await.state,
                ConnectionState::Connected
            );
            let mut changes = net.status_changes();
            let status = loop {
                let status = net.status().incoming;
                if matches!(status, crate::IncomingStatus::Listening { .. }) {
                    break status;
                }
                changes.changed().await.unwrap();
            };
            assert_eq!(
                status,
                crate::IncomingStatus::Listening {
                    port,
                    addresses: vec![format!("10.44.0.2:{port}")],
                    // UPnP is for direct mode only.
                    upnp: false,
                }
            );
            // Nothing listens on the machine's own interfaces.
            assert!(
                TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                    .await
                    .is_err()
            );
            // The VPN server side connects to the tunnel address.
            let target = SocketAddr::from(([10, 44, 0, 2], port));
            let mut remote = tokio_wireguard::TcpStream::connect(target, &server)
                .await
                .unwrap();
            remote.write_all(b"ping").await.unwrap();
            let mut peer = peers.next().await.unwrap();
            assert_eq!(
                peer.addr,
                SocketAddr::from(([10, 44, 0, 1], peer.addr.port()))
            );
            let mut request = [0; 4];
            peer.stream.read_exact(&mut request).await.unwrap();
            assert_eq!(&request, b"ping");
            peer.stream.write_all(b"pong").await.unwrap();
            let mut reply = [0; 4];
            remote.read_exact(&mut reply).await.unwrap();
            assert_eq!(&reply, b"pong");
            // Leaving the mode ends the connection's lease.
            net.apply(NetworkConfig::default()).unwrap();
            peer.lease.revoked().await;
            net.shutdown();
            server.close();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn resolves_search_hosts_and_checks_public_ip_inside_the_tunnel() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (server, config) = pair();
            let dns = tokio_wireguard::UdpSocket::bind("10.44.0.1:53", &server)
                .await
                .unwrap();
            let dns_task = tokio::spawn(async move {
                let mut bytes = [0; 4096];
                let (length, source) = dns.recv_from(&mut bytes[..]).await.unwrap();
                let mut answer = bytes[..length].to_vec();
                answer[2] = 0x81;
                answer[3] = 0x80;
                answer[7] = 1;
                answer
                    .extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 30, 0, 4, 10, 44, 0, 1]);
                dns.send_to(&answer, source).await.unwrap();
            });
            let http = tokio_wireguard::TcpListener::bind("10.44.0.1:8080", &server)
                .await
                .unwrap();
            let http_task = tokio::spawn(async move {
                let (mut stream, _) = http.accept().await.unwrap();
                let mut request = [0; 4096];
                assert!(stream.read(&mut request).await.unwrap() > 0);
                let body = r#"{"ip":"203.0.113.9"}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                let _ = stream.shutdown().await;
            });
            let net = Network::start(
                NetworkConfig {
                    mode: crate::NetworkMode::WireGuard(config),
                    allow_ipv6: false,
                    hold: None,
                },
                NetworkOptions {
                    ip_check_v4_url: "http://search.torrentplayer.test:8080/".into(),
                    ip_check_v6_url: None,
                    ..NetworkOptions::default()
                },
            )
            .await
            .unwrap();
            assert_eq!(
                net.settled(Duration::from_secs(3)).await.state,
                ConnectionState::Connected
            );
            assert_eq!(net.check_public_ip().await.unwrap().ip, "203.0.113.9");
            dns_task.await.unwrap();
            http_task.await.unwrap();
            net.shutdown();
            server.close();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn closed_connections_do_not_starve_dns_or_other_connections() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (server, config) = pair();
            let dns = tokio_wireguard::UdpSocket::bind("10.44.0.1:53", &server)
                .await
                .unwrap();
            let listener = tokio_wireguard::TcpListener::bind("10.44.0.1:8080", &server)
                .await
                .unwrap();
            let dns_task = tokio::spawn(async move {
                loop {
                    let mut bytes = [0; 4096];
                    let (length, source) = dns.recv_from(&mut bytes[..]).await.unwrap();
                    let mut answer = bytes[..length].to_vec();
                    answer[2] = 0x81;
                    answer[3] = 0x80;
                    answer[7] = 1;
                    answer.extend_from_slice(&[
                        0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 30, 0, 4, 10, 44, 0, 1,
                    ]);
                    dns.send_to(&answer, source).await.unwrap();
                }
            });
            let tcp_task = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    tokio::spawn(async move {
                        let mut bytes = [0; 1024];
                        if let Ok(n) = stream.read(&mut bytes).await {
                            let _ = stream.write_all(&bytes[..n]).await;
                            let _ = stream.shutdown().await;
                        }
                    });
                }
            });
            let tunnel = Tunnel::start(&config, false).await.unwrap();
            tunnel
                .healthy(
                    Duration::from_secs(2),
                    crate::WIREGUARD_REPLY_TIMEOUT,
                    crate::WIREGUARD_RESTART_AFTER,
                )
                .await
                .unwrap();
            for _ in 0..5 {
                let stream = tunnel
                    .connect("10.44.0.1:8080".parse().unwrap())
                    .await
                    .unwrap();
                drop(stream);
            }
            let mut jobs = tokio::task::JoinSet::new();
            for _ in 0..16 {
                let t = tunnel.clone();
                jobs.spawn(async move {
                    let addr = t
                        .resolve("search.torrentplayer.test", 8080, true)
                        .await
                        .unwrap()[0];
                    let mut stream = t.connect(addr).await.unwrap();
                    stream.write_all(b"hello").await.unwrap();
                    let mut buf = [0; 5];
                    stream.read_exact(&mut buf).await.unwrap();
                    assert_eq!(&buf, b"hello");
                });
            }
            while let Some(result) = jobs.join_next().await {
                result.unwrap();
            }
            dns_task.abort();
            tcp_task.abort();
            tunnel.close();
            server.close();
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn handshakes_keep_completing_after_many_rekeys() {
        // Both ends rekey 80 times, more than boringtun's handshake rate limit (64 per second)
        // but slower than it. The limit must count per second: counted over the tunnel's
        // lifetime, the 65th handshake was refused as a flood, as were all later ones, and the
        // tunnel went silent once its last session expired.
        tokio::time::timeout(Duration::from_secs(20), async {
            let (server, config) = pair();
            let dns = dns_server(&server).await;
            let listener = tokio_wireguard::TcpListener::bind("10.44.0.1:8080", &server)
                .await
                .unwrap();
            let echo = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = [0; 5];
                stream.read_exact(&mut bytes).await.unwrap();
                stream.write_all(&bytes).await.unwrap();
                let _ = stream.shutdown().await;
            });
            let tunnel = Tunnel::start(&config, false).await.unwrap();
            tunnel
                .healthy(
                    Duration::from_secs(2),
                    crate::WIREGUARD_REPLY_TIMEOUT,
                    crate::WIREGUARD_RESTART_AFTER,
                )
                .await
                .unwrap();
            for _ in 0..80 {
                tunnel.0.interface.request_handshake().unwrap();
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            // The next handshake still completes at once (a refused one is retried only after
            // REKEY_TIMEOUT, 5 s)...
            tokio::time::sleep(Duration::from_millis(1100)).await;
            tunnel.0.interface.request_handshake().unwrap();
            tokio::time::timeout(Duration::from_secs(2), async {
                while tunnel
                    .0
                    .interface
                    .handshake_age()
                    .await
                    .unwrap()
                    .is_none_or(|age| age >= Duration::from_millis(500))
                {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the handshake after 80 rekeys was refused");
            // ...and DNS and data still flow.
            let address = tunnel
                .resolve("search.torrentplayer.test", 8080, true)
                .await
                .unwrap()[0];
            let mut stream = tunnel.connect(address).await.unwrap();
            stream.write_all(b"hello").await.unwrap();
            let mut reply = [0; 5];
            stream.read_exact(&mut reply).await.unwrap();
            assert_eq!(&reply, b"hello");
            echo.await.unwrap();
            dns.abort();
            tunnel.close();
            server.close();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn udp_stays_inside_wireguard_and_closes_with_its_lease() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (server, config) = pair();
            let remote = tokio_wireguard::UdpSocket::bind("10.44.0.1:6969", &server)
                .await
                .unwrap();
            let task = tokio::spawn(async move {
                let mut packet = [0; 128];
                let (n, source) = remote.recv_from(&mut packet[..]).await.unwrap();
                assert_eq!(source.ip(), "10.44.0.2".parse::<IpAddr>().unwrap());
                assert_eq!(&packet[..n], b"tracker request");
                remote.send_to(b"tracker response", source).await.unwrap();
                tokio::time::sleep(Duration::from_millis(20)).await;
            });
            let net = Network::start(
                NetworkConfig {
                    mode: crate::NetworkMode::WireGuard(config),
                    allow_ipv6: false,
                    hold: None,
                },
                NetworkOptions::default(),
            )
            .await
            .unwrap();
            assert_eq!(
                net.settled(Duration::from_secs(2)).await.state,
                ConnectionState::Connected
            );
            assert!(net.status().udp_allowed);
            assert!(!net.policy().udp_allowed, "unbound UDP/DHT stays disabled");
            let target: SocketAddr = "10.44.0.1:6969".parse().unwrap();
            let (socket, lease) = net.udp_socket(target).await.unwrap();
            assert_eq!(
                socket.local_addr().unwrap().ip(),
                "10.44.0.2".parse::<IpAddr>().unwrap()
            );
            socket.send_to(b"tracker request", target).await.unwrap();
            let mut buf = [0; 128];
            let (n, from) = socket.recv_from(&mut buf).await.unwrap();
            assert_eq!(from, target);
            assert_eq!(&buf[..n], b"tracker response");
            task.await.unwrap();
            net.apply(NetworkConfig::default()).unwrap();
            assert!(!lease.is_valid());
            assert!(socket.send_to(b"must stay blocked", target).await.is_err());
            assert!(socket.recv_from(&mut buf).await.is_err());
            net.shutdown();
            server.close();
        })
        .await
        .unwrap();
    }

    /// What [`relay`] passes between the client and the test server.
    const PASS_ALL: u8 = 0;
    /// Handshake messages only: the server authenticates but never answers traffic.
    const DROP_DATA: u8 = 1;
    const DROP_ALL: u8 = 2;

    /// What [`relay`] saw the client send.
    #[derive(Default)]
    struct Traffic {
        /// Transport data packets other than keepalives (32 bytes): probes, when the
        /// application sends nothing.
        data: std::sync::atomic::AtomicUsize,
        /// When each tunnel (a new sender index) sent its first handshake initiation.
        tunnels: Mutex<Vec<(u32, tokio::time::Instant)>>,
    }

    impl Traffic {
        fn data(&self) -> usize {
            self.data.load(Ordering::SeqCst)
        }

        fn tunnels(&self) -> Vec<tokio::time::Instant> {
            self.tunnels
                .lock()
                .unwrap()
                .iter()
                .map(|(_, at)| *at)
                .collect()
        }
    }

    /// A UDP relay in front of the WireGuard test server at `server`, filtered by `mode`. Returns
    /// the endpoint for the client, and what the client sent.
    async fn relay(
        server: SocketAddr,
        mode: Arc<std::sync::atomic::AtomicU8>,
    ) -> (SocketAddr, Arc<Traffic>) {
        let outside = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let inside = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        inside.connect(server).await.unwrap();
        let endpoint = outside.local_addr().unwrap();
        let traffic = Arc::new(Traffic::default());
        let seen = Arc::clone(&traffic);
        tokio::spawn(async move {
            let passes = |packet: &[u8]| match mode.load(Ordering::SeqCst) {
                PASS_ALL => true,
                // Message type 4 is transport data (including keepalives).
                DROP_DATA => packet.first() != Some(&4),
                _ => false,
            };
            let mut client = None;
            let (mut up, mut down) = ([0u8; 2048], [0u8; 2048]);
            loop {
                tokio::select! {
                    Ok((n, from)) = outside.recv_from(&mut up) => {
                        client = Some(from);
                        match up[0] {
                            // A handshake initiation: the upper 24 bits of the sender index
                            // name the tunnel.
                            1 if n >= 8 => {
                                let index = u32::from_le_bytes(up[4..8].try_into().unwrap()) >> 8;
                                let mut tunnels = seen.tunnels.lock().unwrap();
                                if tunnels.iter().all(|(known, _)| *known != index) {
                                    tunnels.push((index, tokio::time::Instant::now()));
                                }
                            }
                            4 if n > 32 => {
                                seen.data.fetch_add(1, Ordering::SeqCst);
                            }
                            _ => {}
                        }
                        if passes(&up[..n]) {
                            let _ = inside.send(&up[..n]).await;
                        }
                    }
                    Ok(n) = inside.recv(&mut down) => {
                        if let (Some(client), true) = (client, passes(&down[..n])) {
                            let _ = outside.send_to(&down[..n], client).await;
                        }
                    }
                }
            }
        });
        (endpoint, traffic)
    }

    /// Answers every DNS query on the test server with 10.44.0.1.
    async fn dns_server(server: &Interface) -> tokio::task::JoinHandle<()> {
        let dns = tokio_wireguard::UdpSocket::bind("10.44.0.1:53", server)
            .await
            .unwrap();
        tokio::spawn(async move {
            loop {
                let mut bytes = [0; 4096];
                let Ok((length, source)) = dns.recv_from(&mut bytes[..]).await else {
                    return;
                };
                let mut answer = bytes[..length].to_vec();
                answer[2] = 0x81;
                answer[3] = 0x80;
                answer[7] = 1;
                answer
                    .extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 30, 0, 4, 10, 44, 0, 1]);
                let _ = dns.send_to(&answer, source).await;
            }
        })
    }

    /// Waits until the state satisfies `done`.
    async fn wait_for(net: &Network, done: impl Fn(&ConnectionState) -> bool) -> ConnectionState {
        let mut changes = net.status_changes();
        loop {
            let state = net.status().state;
            if done(&state) {
                return state;
            }
            changes.changed().await.unwrap();
        }
    }

    /// A health check's reason as the status shows it.
    fn shown(reason: &str) -> String {
        NetError::InterfaceUnavailable(reason.into()).to_string()
    }

    fn health_options(reply_timeout: Duration) -> NetworkOptions {
        NetworkOptions {
            health_interval: Duration::from_millis(100),
            retry_interval: Duration::from_millis(50),
            check_timeout: Duration::from_millis(500),
            wireguard_reply_timeout: reply_timeout,
            ..NetworkOptions::default()
        }
    }

    /// Starts a network for `config` through a relay in front of its endpoint.
    async fn through_relay(
        mut config: WireGuardConfig,
        options: NetworkOptions,
    ) -> (Network, Arc<std::sync::atomic::AtomicU8>, Arc<Traffic>) {
        let mode = Arc::new(std::sync::atomic::AtomicU8::new(PASS_ALL));
        let target: SocketAddr = config.peers[0].endpoint.parse().unwrap();
        let (endpoint, traffic) = relay(target, Arc::clone(&mode)).await;
        config.peers[0].endpoint = endpoint.to_string();
        let net = Network::start(
            NetworkConfig {
                mode: crate::NetworkMode::WireGuard(config),
                allow_ipv6: false,
                hold: None,
            },
            options,
        )
        .await
        .unwrap();
        (net, mode, traffic)
    }

    /// The tunnel engages the kill switch when the server stops answering and resumes by
    /// itself when it answers again, with `dns` as the configuration's DNS servers, in a split
    /// tunnel (`AllowedIPs` 10.44.0.0/24).
    async fn recovers_without_the_dns_probe(dns: &[&str]) {
        tokio::time::timeout(Duration::from_secs(30), async {
            let (server, mut config) = pair();
            config.interface.dns = dns.iter().map(|ip| (*ip).to_owned()).collect();
            config.peers[0].allowed_ips = vec!["10.44.0.0/24".into()];
            let reply_timeout = Duration::from_secs(1);
            let (net, mode, traffic) = through_relay(config, health_options(reply_timeout)).await;
            assert_eq!(
                net.settled(Duration::from_secs(3)).await.state,
                ConnectionState::Connected
            );
            // Idle: probed, and the probes are answered.
            tokio::time::sleep(reply_timeout * 3).await;
            assert_eq!(net.status().state, ConnectionState::Connected);
            assert!(traffic.data() > 0, "the idle tunnel was never probed");

            // The server stops answering while the application sends something.
            mode.store(DROP_DATA, Ordering::SeqCst);
            let target: SocketAddr = "10.44.0.1:6969".parse().unwrap();
            let (socket, _lease) = net.udp_socket(target).await.unwrap();
            socket.send_to(b"tracker request", target).await.unwrap();
            let state = wait_for(&net, |s| {
                matches!(s, ConnectionState::KillSwitchActive { .. })
            })
            .await;
            assert_eq!(
                state,
                ConnectionState::KillSwitchActive {
                    reason: shown(NOT_ANSWERING)
                }
            );
            tokio::time::sleep(reply_timeout * 2).await;
            assert!(!net.status().traffic_allowed);

            // The kill switch blocks the application: only probes are sent, and they are enough.
            mode.store(PASS_ALL, Ordering::SeqCst);
            tokio::time::timeout(
                reply_timeout * 5,
                wait_for(&net, |s| *s == ConnectionState::Connected),
            )
            .await
            .expect("traffic did not resume after the server answered again");
            assert!(net.status().traffic_allowed);
            net.shutdown();
            server.close();
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recovers_without_a_dns_server() {
        recovers_without_the_dns_probe(&[]).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recovers_with_a_dns_server_outside_allowed_ips() {
        recovers_without_the_dns_probe(&["192.0.2.53"]).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recovers_with_a_dns_server_in_another_ip_family() {
        recovers_without_the_dns_probe(&["fd00::53"]).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_idle_healthy_tunnel_stays_connected_and_probes_less_and_less() {
        tokio::time::timeout(Duration::from_secs(20), async {
            let (server, config) = pair();
            let dns = dns_server(&server).await;
            let reply_timeout = Duration::from_secs(1);
            let (net, _mode, traffic) = through_relay(config, health_options(reply_timeout)).await;
            assert_eq!(
                net.settled(Duration::from_secs(3)).await.state,
                ConnectionState::Connected
            );
            let before = traffic.data();
            // Gaps of 0.5, 1, 2, then 4 s (at most): probes at about 0.5, 1.5, 3.5, and 7.5 s.
            // Checked every 100 ms, the old schedule probed about every 0.6 s.
            tokio::time::sleep(Duration::from_secs(8)).await;
            let probes = traffic.data() - before;
            assert!((2..=6).contains(&probes), "{probes} idle probes in 8 s");
            assert_eq!(net.status().state, ConnectionState::Connected);
            assert!(
                net.status()
                    .events
                    .iter()
                    .all(|e| !e.message.starts_with("Connection lost"))
            );
            dns.abort();
            net.shutdown();
            server.close();
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tunnel_left_unanswered_is_restarted_at_most_once_per_restart_interval() {
        tokio::time::timeout(Duration::from_secs(30), async {
            let (server, config) = pair();
            let dns = dns_server(&server).await;
            let reply_timeout = Duration::from_secs(1);
            let restart_after = Duration::from_secs(3);
            let (net, mode, traffic) = through_relay(
                config,
                NetworkOptions {
                    wireguard_restart_after: restart_after,
                    ..health_options(reply_timeout)
                },
            )
            .await;
            assert_eq!(
                net.settled(Duration::from_secs(3)).await.state,
                ConnectionState::Connected
            );
            assert_eq!(traffic.tunnels().len(), 1);

            mode.store(DROP_DATA, Ordering::SeqCst);
            wait_for(&net, |s| {
                matches!(s, ConnectionState::KillSwitchActive { .. })
            })
            .await;
            // Two restarts, restart_after apart. Each new tunnel completes its handshake but
            // stays blocked: the server still answers no traffic.
            while traffic.tunnels().len() < 3 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                assert_eq!(
                    net.status().state,
                    ConnectionState::KillSwitchActive {
                        reason: shown(NOT_ANSWERING)
                    }
                );
            }
            let tunnels = traffic.tunnels();
            assert!(tunnels[2] - tunnels[1] + Duration::from_millis(100) >= restart_after);
            assert!(
                !net.status()
                    .events
                    .iter()
                    .any(|e| e.message.starts_with("Connection restored"))
            );

            mode.store(PASS_ALL, Ordering::SeqCst);
            tokio::time::timeout(
                reply_timeout * 5,
                wait_for(&net, |s| *s == ConnectionState::Connected),
            )
            .await
            .expect("traffic did not resume after the server answered again");
            assert!(net.status().traffic_allowed);
            dns.abort();
            net.shutdown();
            server.close();
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_probe_that_nothing_routes_is_answered_by_the_servers_keepalive() {
        // No DNS and AllowedIPs 0.0.0.0/0: the probe goes to 192.0.2.1, which the test server
        // drops. Its WireGuard still answers within KEEPALIVE_TIMEOUT (10 s).
        tokio::time::timeout(Duration::from_secs(20), async {
            let (server, mut config) = pair();
            config.interface.dns.clear();
            let tunnel = Tunnel::start(&config, false).await.unwrap();
            assert_eq!(
                tunnel.0.probe_target.unwrap().remote,
                "192.0.2.1:9".parse().unwrap()
            );
            tunnel
                .healthy(
                    Duration::from_secs(2),
                    crate::WIREGUARD_REPLY_TIMEOUT,
                    crate::WIREGUARD_RESTART_AFTER,
                )
                .await
                .unwrap();
            assert_eq!(
                tunnel.0.interface.liveness().await.unwrap().received_age,
                None
            );
            // boringtun's timers count in ticks (100 ms here): a packet received in the tick
            // in which the server sent its handshake response does not count as unanswered.
            // Real probes come at least half a reply timeout after the last handshake.
            tokio::time::sleep(Duration::from_millis(500)).await;
            tunnel.probe(crate::WIREGUARD_REPLY_TIMEOUT);
            loop {
                let liveness = tunnel.0.interface.liveness().await.unwrap();
                if liveness.unanswered.is_none() && liveness.received_age.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            tunnel.close();
            server.close();
        })
        .await
        .unwrap();
    }

    #[test]
    fn probes_go_where_allowed_ips_route_them_to_the_server() {
        let ip = |text: &str| text.parse::<IpAddr>().unwrap();
        let target = |locals: &[&str], allowed: &[&str], dns: &[&str]| {
            probe_target(
                &locals.iter().map(|l| ip(l)).collect::<Vec<_>>(),
                &allowed
                    .iter()
                    .map(|a| network(a).unwrap())
                    .collect::<Vec<_>>(),
                &dns.iter().map(|d| ip(d)).collect::<Vec<_>>(),
            )
            .map(|t| (t.local, t.remote.to_string(), t.dns))
        };
        let v4 = ip("10.64.0.2");
        // The DNS server, when it is routed into the tunnel and the tunnel has its family.
        assert_eq!(
            target(&["10.64.0.2"], &["0.0.0.0/0"], &["10.64.0.1"]),
            Some((v4, "10.64.0.1:53".into(), true))
        );
        assert_eq!(
            target(
                &["10.64.0.2", "fd00::2"],
                &["0.0.0.0/0", "::/0"],
                &["fd00::1"]
            ),
            Some((ip("fd00::2"), "[fd00::1]:53".into(), true))
        );
        // Otherwise a documentation address, which nothing routes...
        assert_eq!(
            target(&["10.64.0.2"], &["0.0.0.0/0"], &[]),
            Some((v4, "192.0.2.1:9".into(), false))
        );
        assert_eq!(
            target(&["10.64.0.2"], &["0.0.0.0/1", "128.0.0.0/1"], &["fd00::1"]),
            Some((v4, "192.0.2.1:9".into(), false))
        );
        assert_eq!(
            target(&["fd00::2"], &["::/0"], &["10.64.0.1"]),
            Some((ip("fd00::2"), "[2001:db8::1]:9".into(), false))
        );
        // ...or the first host of a split tunnel's first range, not the tunnel's own address.
        assert_eq!(
            target(
                &["10.64.0.2"],
                &["10.64.0.0/16", "10.65.0.0/16"],
                &["192.168.1.1"]
            ),
            Some((v4, "10.64.0.1:9".into(), false))
        );
        assert_eq!(
            target(&["10.64.0.1"], &["10.64.0.0/24"], &[]),
            Some((ip("10.64.0.1"), "10.64.0.0:9".into(), false))
        );
        assert_eq!(
            target(&["10.64.0.2"], &["fd00::/64", "10.0.0.7/32"], &[]),
            Some((v4, "10.0.0.7:9".into(), false))
        );
        // Nothing the tunnel could send.
        assert_eq!(target(&["10.64.0.2"], &["fd00::/64"], &["fd00::1"]), None);
        assert_eq!(target(&["10.64.0.2"], &["10.64.0.2/32"], &[]), None);
        // Host bits in an AllowedIPs entry are ignored.
        assert_eq!(network("10.64.3.9/16"), Some((ip("10.64.0.0"), 16)));
        assert_eq!(network("10.64.3.9/33"), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_server_that_never_answers_never_counts_as_connected() {
        // Everything sent to this endpoint is dropped.
        let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let (server, mut config) = pair();
        config.peers[0].endpoint = silent.local_addr().unwrap().to_string();
        let net = Network::start(
            NetworkConfig {
                mode: crate::NetworkMode::WireGuard(config),
                allow_ipv6: false,
                hold: None,
            },
            health_options(Duration::from_secs(1)),
        )
        .await
        .unwrap();
        let status = net.settled(Duration::from_secs(5)).await;
        assert_eq!(
            status.state,
            ConnectionState::KillSwitchActive {
                reason: shown(NO_HANDSHAKE)
            }
        );
        assert!(!status.traffic_allowed && status.kill_switch_engaged);
        // It stays that way, and nothing is allowed through.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let status = net.status();
        assert!(!status.traffic_allowed);
        assert!(status.connected_since_ms.is_none());
        assert!(matches!(
            net.connect("10.44.0.1:80".parse().unwrap()).await,
            Err(NetError::KillSwitch(reason)) if reason == shown(NO_HANDSHAKE)
        ));
        assert!(matches!(
            net.resolve_host("search.torrentplayer.test", 443).await,
            Err(NetError::KillSwitch(_))
        ));
        net.shutdown();
        server.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_server_that_stops_answering_engages_the_kill_switch_until_it_answers_again() {
        tokio::time::timeout(Duration::from_secs(30), async {
            let (server, mut config) = pair();
            let dns = dns_server(&server).await;
            let mode = Arc::new(std::sync::atomic::AtomicU8::new(PASS_ALL));
            let target: SocketAddr = config.peers[0].endpoint.parse().unwrap();
            config.peers[0].endpoint = relay(target, Arc::clone(&mode)).await.0.to_string();
            let reply_timeout = Duration::from_secs(1);
            let net = Network::start(
                NetworkConfig {
                    mode: crate::NetworkMode::WireGuard(config),
                    allow_ipv6: false,
                    hold: None,
                },
                health_options(reply_timeout),
            )
            .await
            .unwrap();
            assert_eq!(
                net.settled(Duration::from_secs(3)).await.state,
                ConnectionState::Connected
            );
            // Idle for several reply timeouts: the probes are answered, so it stays connected.
            let (_, mut lease) = net
                .udp_socket("10.44.0.1:6969".parse().unwrap())
                .await
                .unwrap();
            tokio::time::sleep(reply_timeout * 3).await;
            assert_eq!(net.status().state, ConnectionState::Connected);
            assert!(lease.is_valid());

            // The server keeps completing handshakes but answers no traffic, as in #60.
            mode.store(DROP_DATA, Ordering::SeqCst);
            let lost = tokio::time::Instant::now();
            let state = wait_for(&net, |s| *s != ConnectionState::Connected).await;
            // A probe sent just before the switch may be the one left unanswered.
            assert!(lost.elapsed() + Duration::from_millis(250) >= reply_timeout);
            assert!(matches!(
                state,
                ConnectionState::Reconnecting { reason }
                    | ConnectionState::KillSwitchActive { reason } if reason == shown(NOT_ANSWERING)
            ));
            lease.revoked().await;
            assert!(matches!(
                net.connect("10.44.0.1:80".parse().unwrap()).await,
                Err(NetError::KillSwitch(reason)) if reason == shown(NOT_ANSWERING)
            ));
            // No flapping back while it stays silent.
            let state = wait_for(&net, |s| {
                matches!(s, ConnectionState::KillSwitchActive { .. })
            })
            .await;
            assert_eq!(
                state,
                ConnectionState::KillSwitchActive {
                    reason: shown(NOT_ANSWERING)
                }
            );
            tokio::time::sleep(reply_timeout * 2).await;
            assert!(!net.status().traffic_allowed);

            // Silent altogether, then answering again: traffic resumes by itself.
            mode.store(DROP_ALL, Ordering::SeqCst);
            tokio::time::sleep(reply_timeout).await;
            assert!(!net.status().traffic_allowed);
            mode.store(PASS_ALL, Ordering::SeqCst);
            wait_for(&net, |s| *s == ConnectionState::Connected).await;
            assert!(net.status().traffic_allowed);
            assert!(
                net.status()
                    .events
                    .iter()
                    .any(|e| e.message.starts_with("Connection restored"))
            );
            dns.abort();
            net.shutdown();
            server.close();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn wrong_keys_never_allow_connections() {
        let (server, mut config) = pair();
        let (wrong, _) = x25519::keypair();
        config.peers[0].preshared_key = Some(Secret::new(
            base64::engine::general_purpose::STANDARD.encode(wrong.as_bytes()),
        ));
        let net = Network::start(
            NetworkConfig {
                mode: crate::NetworkMode::WireGuard(config),
                allow_ipv6: false,
                hold: None,
            },
            NetworkOptions {
                check_timeout: Duration::from_millis(150),
                ..NetworkOptions::default()
            },
        )
        .await
        .unwrap();
        let state = net.settled(Duration::from_secs(2)).await;
        assert!(state.kill_switch_engaged);
        assert!(!state.traffic_allowed);
        assert!(matches!(
            net.connect("127.0.0.1:80".parse().unwrap()).await,
            Err(NetError::KillSwitch(_))
        ));
        net.shutdown();
        server.close();
    }

    #[tokio::test]
    async fn truncated_dns_retries_over_tunneled_tcp() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (server, config) = pair();
            let udp = tokio_wireguard::UdpSocket::bind("10.44.0.1:53", &server)
                .await
                .unwrap();
            let tcp = tokio_wireguard::TcpListener::bind("10.44.0.1:53", &server)
                .await
                .unwrap();
            let task = tokio::spawn(async move {
                let mut bytes = [0; 4096];
                let (length, source) = udp.recv_from(&mut bytes[..]).await.unwrap();
                bytes[2] = 0x83; // response with TC set
                bytes[3] = 0x80;
                udp.send_to(&bytes[..length], source).await.unwrap();
                let (mut stream, _) = tcp.accept().await.unwrap();
                let length = usize::from(stream.read_u16().await.unwrap());
                let mut answer = vec![0; length];
                stream.read_exact(&mut answer).await.unwrap();
                answer[2] = 0x81;
                answer[3] = 0x80;
                answer[7] = 1;
                answer
                    .extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 30, 0, 4, 10, 44, 0, 1]);
                stream
                    .write_u16(u16::try_from(answer.len()).unwrap())
                    .await
                    .unwrap();
                stream.write_all(&answer).await.unwrap();
                let _ = stream.shutdown().await;
            });
            let tunnel = Tunnel::start(&config, false).await.unwrap();
            tunnel
                .healthy(
                    Duration::from_secs(3),
                    crate::WIREGUARD_REPLY_TIMEOUT,
                    crate::WIREGUARD_RESTART_AFTER,
                )
                .await
                .unwrap();
            let addresses = tunnel
                .resolve("large.torrentplayer.test", 443, true)
                .await
                .unwrap();
            assert_eq!(
                addresses,
                vec!["10.44.0.1:443".parse::<SocketAddr>().unwrap()]
            );
            task.await.unwrap();
            tunnel.close();
            server.close();
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn missing_dns_never_falls_back_to_the_system_resolver() {
        let (server, mut config) = pair();
        config.interface.dns.clear();
        let tunnel = Tunnel::start(&config, false).await.unwrap();
        assert!(matches!(
            tunnel.resolve("api.ipify.org", 443, true).await,
            Err(NetError::InvalidConfig(_))
        ));
        tunnel.close();
        server.close();
    }

    #[tokio::test]
    async fn a_random_port_that_is_taken_moves_on_to_the_next_free_one() {
        // With 1024 of the 16382 ephemeral ports taken, each random pick has a 1 in 16 chance
        // of starting on a taken port. That used to fail the bind at once ("not a suitable
        // bind address"); 300 binds hit it with near certainty.
        let (key, _) = x25519::keypair();
        let (_, peer) = x25519::keypair();
        let mut options = tokio_wireguard::interface::Options::default();
        options.udp.recv_buffer_size = 1024;
        options.udp.send_buffer_size = 1024;
        let interface = Interface::new_with(
            config::Config {
                interface: config::Interface {
                    address: "10.44.0.2/32".parse().unwrap(),
                    listen_port: None,
                    private_key: key,
                    mtu: None,
                },
                peers: vec![config::Peer {
                    endpoint: None,
                    allowed_ips: vec!["10.44.0.1/32".parse().unwrap()],
                    public_key: peer,
                    preshared_key: None,
                    persistent_keepalive: None,
                }],
            },
            options,
        )
        .unwrap();
        let mut taken = Vec::new();
        for port in (49152u16..).step_by(16).take(1024) {
            taken.push(
                tokio_wireguard::UdpSocket::bind(("10.44.0.2", port), &interface)
                    .await
                    .unwrap(),
            );
        }
        for _ in 0..300 {
            let socket = tokio_wireguard::UdpSocket::bind("10.44.0.2:0", &interface)
                .await
                .expect("a free port was available");
            assert_ne!(socket.local_addr().unwrap().port() % 16, 0);
        }
        drop(taken);
        interface.close();
    }

    #[test]
    fn dns_questions_accept_valid_names_and_reject_invalid_input() {
        for host in [
            "api.ipify.org",
            "search.torrentplayer.test.",
            &format!("{}.test", "a".repeat(63)),
        ] {
            let query = dns_query(host, dns_parser::QueryType::A).unwrap();
            assert_eq!(
                dns_parser::Packet::parse(&query).unwrap().questions.len(),
                1
            );
        }
        for host in ["", "a..test", &format!("{}.test", "a".repeat(64))] {
            assert!(dns_query(host, dns_parser::QueryType::A).is_err());
        }
    }
}
