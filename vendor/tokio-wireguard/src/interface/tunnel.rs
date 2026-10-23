use std::{
    io,
    net::{IpAddr, Ipv6Addr, SocketAddr, SocketAddrV6},
    sync::Arc,
    time::Instant,
};

use boringtun::{
    noise::{
        HandshakeResponse, Packet, PacketCookieReply, PacketData, Tunn, TunnResult,
        rate_limiter::RateLimiter,
    },
    x25519::{PublicKey, StaticSecret},
};
use rand::SeedableRng;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::net::UdpSocket;

use super::random::Lfsr24;

pub struct Tunnel {
    address: crate::config::Address,
    keypair: (StaticSecret, PublicKey),
    peers: Vec<Peer>,
    rng: Lfsr24,
    rate_limiter: Arc<RateLimiter>,
    buffers: (Vec<u8>, Vec<u8>),
    socket: UdpSocket,
}

pub trait IntoPacket<'a> {
    fn into_packet(self, buffer: &'a [u8]) -> &'a [u8];
}

impl Tunnel {
    pub fn new(
        config: &crate::config::Interface,
        peer_configs: impl IntoIterator<Item = crate::config::Peer>,
    ) -> Result<Self, io::Error> {
        let private_key = config.private_key.clone();
        let public_key = PublicKey::from(&private_key);

        let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_nonblocking(true)?;
        socket.set_only_v6(false)?;
        #[cfg(not(windows))]
        socket.set_reuse_address(true)?;

        let address: SockAddr =
            SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, config.listen_port.unwrap_or(0), 0, 0).into();
        socket.bind(&address)?;

        let socket = UdpSocket::from_std(socket.into())?;
        let mut tunnel = Tunnel {
            address: config.address,
            keypair: (private_key, public_key),
            peers: Vec::new(),
            rng: Lfsr24::from_entropy(),
            // Modified for TorrentPlayer: 128, since `recv` counts each handshake initiation
            // twice (boringtun's device uses 64 and counts it once).
            rate_limiter: Arc::new(RateLimiter::new(&public_key, 128)),
            buffers: (vec![0; u16::MAX as usize], vec![0; u16::MAX as usize]),
            socket,
        };

        for peer_config in peer_configs {
            tunnel.add_peer(peer_config)?;
        }
        Ok(tunnel)
    }

    pub fn handshake_age(&self) -> Option<std::time::Duration> {
        self.peers
            .iter()
            .filter(|p| p.active)
            .filter_map(|p| p.tunnel.time_since_last_handshake())
            .min()
    }

    /// Added for TorrentPlayer: see [`crate::interface::Liveness`].
    pub fn liveness(&self) -> crate::interface::Liveness {
        let active = || self.peers.iter().filter(|p| p.active);
        crate::interface::Liveness {
            handshake_age: self.handshake_age(),
            received_age: active()
                .filter_map(|p| p.received.map(|at| at.elapsed()))
                .min(),
            unanswered: active()
                .filter_map(|p| p.waiting_since.map(|at| at.elapsed()))
                .max(),
        }
    }

    pub async fn initiate(&mut self) {
        for peer in self.peers.iter_mut().filter(|p| p.active) {
            if let Some(endpoint) = peer.config.endpoint {
                if let TunnResult::WriteToNetwork(packet) = peer
                    .tunnel
                    .format_handshake_initiation(&mut self.buffers.0, false)
                {
                    self.socket.send_to(packet, mapped(endpoint)).await.ok();
                }
            }
        }
    }

    pub fn socket(&self) -> &UdpSocket {
        &self.socket
    }

    pub async fn recv(&mut self) -> Option<&[u8]> {
        let Self {
            address,
            keypair: (private, public),
            peers,
            buffers,
            socket,
            rate_limiter,
            ..
        } = self;

        let (len, source) = socket.try_recv_from(&mut buffers.0).ok()?;
        let packet = Tunn::parse_incoming_packet(&buffers.0[..len]).ok()?;
        // Modified for TorrentPlayer: a handshake initiation passes the rate limiter (its MAC
        // and, under load, the cookie that proves the source address) before the
        // Diffie-Hellman work below, as in boringtun's own device: before, any 148-byte
        // datagram of type 1 cost a key agreement. `decapsulate` checks it again, so an
        // initiation counts twice toward the limit (see `new`).
        if let Packet::HandshakeInit(..) = packet {
            match rate_limiter.verify_packet(Some(source.ip()), &buffers.0[..len], &mut buffers.1) {
                Ok(_) => {}
                Err(TunnResult::WriteToNetwork(cookie)) => {
                    socket.send_to(cookie, source).await.ok();
                    return None;
                }
                Err(_) => return None,
            }
        }
        let transport = matches!(packet, Packet::PacketData(..));
        // Added for TorrentPlayer: like WireGuard, roam to the source of authenticated
        // handshake and data messages only, never of a cookie reply.
        let roams = !matches!(packet, Packet::PacketCookieReply(..));

        let peer = match packet {
            Packet::HandshakeInit(packet) => {
                let handshake =
                    boringtun::noise::handshake::parse_handshake_anon(private, public, &packet)
                        .ok()?;
                peers.iter_mut().find(|p| {
                    p.active && p.config.public_key.as_bytes() == &handshake.peer_static_public
                })?
            }

            Packet::HandshakeResponse(HandshakeResponse { receiver_idx, .. })
            | Packet::PacketCookieReply(PacketCookieReply { receiver_idx, .. })
            | Packet::PacketData(PacketData { receiver_idx, .. }) => peers
                .iter_mut()
                .find(|p| p.active && p.index == receiver_idx >> 8)?,
        };

        let packet =
            match peer
                .tunnel
                .decapsulate(Some(source.ip()), &buffers.0[..len], &mut buffers.1)
            {
                TunnResult::Err(..) => return None,
                TunnResult::Done => None,
                TunnResult::WriteToNetwork(mut datagram) => loop {
                    socket.send_to(datagram, source).await.ok();
                    match peer.tunnel.decapsulate(None, &[], &mut buffers.0) {
                        TunnResult::WriteToNetwork(d) => datagram = d,
                        _ => break None,
                    }
                },
                TunnResult::WriteToTunnelV4(p, a) if peer.is_allowed_ip(a) => Some(p),
                TunnResult::WriteToTunnelV6(p, a) if peer.is_allowed_ip(a) => Some(p),
                _ => None,
            };

        if roams {
            peer.config.endpoint = Some(source);
        }
        // Added for TorrentPlayer: an authenticated data or keepalive packet shows the peer
        // is answering (see `liveness`). Handshake messages do not: a server can complete
        // handshakes and still drop every data packet.
        if transport {
            peer.received = Some(Instant::now());
            peer.waiting_since = None;
        }
        match packet {
            Some(packet_out) => {
                let len = packet_out.len();
                buffers.0[..len].copy_from_slice(packet_out);

                let address = *address;
                self.send(len, |destination| {
                    address.networks().any(|net| net.contains(&destination))
                })
                .await
            }
            None => None,
        }
    }

    pub async fn send<'s>(
        &'s mut self,
        packet: impl IntoPacket<'s>,
        relay: impl FnOnce(IpAddr) -> bool,
    ) -> Option<&'s [u8]> {
        let Self {
            address,
            peers,
            buffers,
            socket,
            ..
        } = self;

        let packet = packet.into_packet(&buffers.0);
        let destination = Tunn::dst_address(packet)?;

        if address.addresses().any(|ip| ip == destination) {
            return Some(packet);
        }
        if !relay(destination) {
            return None;
        }

        let peer = peers.iter_mut().find(|p| {
            p.active
                && p.config
                    .allowed_ips
                    .iter()
                    .any(|net| net.contains(&destination))
        })?;

        let endpoint = match peer.config.endpoint? {
            SocketAddr::V4(addr) => SocketAddr::V6(SocketAddrV6::new(
                addr.ip().to_ipv6_mapped(),
                addr.port(),
                0,
                0,
            )),
            e => e,
        };
        // Added for TorrentPlayer: traffic now waits for an answer (see `liveness`); a packet
        // queued for a handshake that never completes waits too.
        peer.waiting_since.get_or_insert_with(Instant::now);
        let packet = peer.tunnel.encapsulate(packet, &mut buffers.1);
        if let TunnResult::WriteToNetwork(packet) = packet {
            socket.send_to(packet, mapped(endpoint)).await.ok();
        }

        None
    }

    pub async fn update_timers(&mut self) {
        let Self {
            peers,
            buffers,
            socket,
            rate_limiter,
            ..
        } = self;

        // Modified for TorrentPlayer: the rate limiter the peers share counts handshake messages
        // until its owner resets the count (boringtun's own device does so every second; this
        // resets at most once a second). Unreset, every handshake after the 64th of the
        // interface's lifetime was refused as a flood and answered with a cookie, so each rekey
        // stalled for at least REKEY_TIMEOUT, or failed if the server ignores cookies.
        rate_limiter.reset_count();

        for peer in peers.iter_mut() {
            let endpoint = match (peer.active, peer.config.endpoint) {
                (true, Some(endpoint)) => endpoint,
                _ => continue,
            };
            let packet = peer.tunnel.update_timers(&mut buffers.0);
            if let TunnResult::WriteToNetwork(packet) = packet {
                socket.send_to(packet, mapped(endpoint)).await.ok();
            }
        }
    }

    pub fn add_peer(&mut self, config: crate::config::Peer) -> Result<(), io::Error> {
        let Self {
            keypair: (private, _),
            peers,
            rng,
            rate_limiter,
            ..
        } = self;

        let index = rng.next();
        let tunnel = Tunn::new(
            private.clone(),
            config.public_key,
            config.preshared_key.as_ref().map(StaticSecret::to_bytes),
            config.persistent_keepalive,
            index,
            Some(rate_limiter.clone()),
        );
        let peer = Peer {
            config,
            tunnel,
            index,
            active: true,
            received: None,
            waiting_since: None,
        };

        let slot = peers.iter_mut().find(|p| !p.active);
        if let Some(slot) = slot {
            *slot = peer;
        } else {
            peers.push(peer);
        }
        Ok(())
    }

    pub fn remove_peer(&mut self, key: &PublicKey) -> bool {
        match self.peers.iter_mut().find(|p| &p.config.public_key == key) {
            Some(peer) => {
                peer.active = false;
                true
            }
            None => false,
        }
    }
}

impl<'a> IntoPacket<'a> for &'a [u8] {
    fn into_packet(self, _: &'a [u8]) -> &'a [u8] {
        self
    }
}

impl<'a> IntoPacket<'a> for usize {
    fn into_packet(self, buffer: &'a [u8]) -> &'a [u8] {
        &buffer[..self]
    }
}

struct Peer {
    config: crate::config::Peer,
    tunnel: Tunn,
    index: u32,
    active: bool,
    /// Added for TorrentPlayer: when an authenticated transport packet last arrived.
    received: Option<Instant>,
    /// Added for TorrentPlayer: when the oldest packet still waiting for an answer was sent.
    waiting_since: Option<Instant>,
}

impl Peer {
    fn is_allowed_ip(&self, address: impl Into<IpAddr>) -> bool {
        let address = address.into();
        self.config
            .allowed_ips
            .iter()
            .any(|net| net.contains(&address))
    }
}

fn mapped(endpoint: SocketAddr) -> SocketAddr {
    match endpoint {
        SocketAddr::V4(addr) => SocketAddr::V6(SocketAddrV6::new(
            addr.ip().to_ipv6_mapped(),
            addr.port(),
            0,
            0,
        )),
        addr => addr,
    }
}
