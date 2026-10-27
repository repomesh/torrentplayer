// SPDX-License-Identifier: MIT OR Apache-2.0

//! RFC 1928 UDP relay: only the negotiated relay receives OS datagrams. Its TCP control
//! connection and network lease must both remain alive; fragmentation is unsupported.

use crate::{
    Lease, NetError, Socks5Config,
    socks5::{self, Target},
};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Mutex, PoisonError};
use tokio::io::AsyncReadExt as _;
use tokio::net::{TcpStream, UdpSocket};
use tokio_util::sync::CancellationToken;

pub(crate) struct SocksUdp {
    socket: UdpSocket,
    closed: CancellationToken,
    monitor: tokio::task::JoinHandle<()>,
    /// Receives whole envelopes; kept rather than allocated for every datagram.
    buffer: Mutex<Vec<u8>>,
}

impl Drop for SocksUdp {
    fn drop(&mut self) {
        self.monitor.abort();
    }
}

impl SocksUdp {
    pub(crate) async fn open(
        cfg: &Socks5Config,
        mut lease: Lease,
        loopback_only: bool,
    ) -> Result<Self, NetError> {
        let mut control = TcpStream::connect((cfg.host.trim(), cfg.port))
            .await
            .map_err(NetError::ProxyUnreachable)?;
        let peer = control.peer_addr()?;
        let target = socks5::udp_associate(&mut control, cfg.credentials()).await?;
        // An unspecified relay address means the proxy's own address. Do not resolve a
        // relay name locally when remote DNS was requested; that would add an unscoped lookup.
        let mut relay = match target {
            Target::Addr(addr) => addr,
            Target::Domain(host, port) if host.eq_ignore_ascii_case(cfg.host.trim()) => {
                SocketAddr::new(peer.ip(), port)
            }
            Target::Domain(_, _) => {
                return Err(NetError::Unsupported(
                    "the proxy's UDP relay must supply an IP address".into(),
                ));
            }
        };
        if relay.ip().is_unspecified() {
            relay.set_ip(peer.ip());
        }
        check_relay(peer.ip(), relay)?;
        let local = crate::addr::local_ip(relay.is_ipv6(), loopback_only);
        let socket = UdpSocket::bind(SocketAddr::new(local, 0)).await?;
        socket.connect(relay).await?;
        let closed = CancellationToken::new();
        let stopped = closed.clone();
        let monitor = tokio::spawn(async move {
            let mut byte = [0];
            tokio::select! {
                biased;
                () = lease.revoked() => {},
                _ = control.read(&mut byte) => {},
            }
            stopped.cancel();
        });
        Ok(Self {
            socket,
            closed,
            monitor,
            buffer: Mutex::new(vec![0; 65_535]),
        })
    }

    pub(crate) fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    pub(crate) async fn send(&self, bytes: &[u8], target: &Target) -> io::Result<usize> {
        let packet = encode(bytes, target)?;
        tokio::select! {
            biased;
            () = self.closed.cancelled() => Err(closed()),
            sent = self.socket.send(&packet) => { sent?; Ok(bytes.len()) },
        }
    }

    pub(crate) async fn recv(&self, bytes: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        // Read a whole UDP envelope: a too-small caller buffer must not turn a truncated
        // envelope into a valid packet. The kernel accepts only the connected relay.
        loop {
            tokio::select! {
                biased;
                () = self.closed.cancelled() => return Err(closed()),
                ready = self.socket.readable() => ready?,
            }
            let mut packet = self.buffer.lock().unwrap_or_else(PoisonError::into_inner);
            let n = match self.socket.try_recv(&mut packet) {
                Ok(n) => n,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                Err(error) => return Err(error),
            };
            let Some((payload, source)) = decode(&packet[..n]) else {
                continue;
            };
            if payload.len() > bytes.len() {
                continue;
            }
            bytes[..payload.len()].copy_from_slice(payload);
            return Ok((payload.len(), source));
        }
    }
}

/// Refuses a relay that is not usable, or that a public proxy has no business naming: a relay on
/// this computer or its network (loopback, private, link-local) would point the datagrams at
/// local hosts instead of the proxy. A proxy that is itself local may relay locally.
fn check_relay(proxy: IpAddr, relay: SocketAddr) -> Result<(), NetError> {
    if relay.port() == 0 || relay.ip().is_multicast() {
        return Err(NetError::Unsupported(
            "the proxy supplied an invalid UDP relay".into(),
        ));
    }
    if crate::addr::is_public_ip(proxy) && !crate::addr::is_public_ip(relay.ip()) {
        return Err(NetError::Unsupported(
            "the proxy supplied a UDP relay on the local network".into(),
        ));
    }
    Ok(())
}

fn closed() -> io::Error {
    io::Error::new(
        io::ErrorKind::ConnectionAborted,
        "the proxy UDP association closed",
    )
}

pub(crate) fn encode(bytes: &[u8], target: &Target) -> io::Result<Vec<u8>> {
    let mut packet = socks5::encode_request(target)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    packet[..3].fill(0); // RSV (two bytes), FRAG (zero: no fragmentation).
    if packet.len() + bytes.len() > 65_507 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "UDP packet is too large",
        ));
    }
    packet.extend_from_slice(bytes);
    Ok(packet)
}

fn decode(packet: &[u8]) -> Option<(&[u8], SocketAddr)> {
    if packet.get(..3)? != [0, 0, 0] {
        return None;
    }
    let (ip, end): (IpAddr, usize) = match *packet.get(3)? {
        1 => (
            Ipv4Addr::from(<[u8; 4]>::try_from(packet.get(4..8)?).ok()?).into(),
            8,
        ),
        4 => (
            Ipv6Addr::from(<[u8; 16]>::try_from(packet.get(4..20)?).ok()?).into(),
            20,
        ),
        // A reply must identify the remote IP. Never turn a domain envelope into a local DNS lookup.
        _ => return None,
    };
    let port = u16::from_be_bytes(packet.get(end..end + 2)?.try_into().ok()?);
    if port == 0 {
        return None;
    }
    Some((packet.get(end + 2..)?, SocketAddr::new(ip, port)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[tokio::test]
    async fn receive_ignores_other_senders_invalid_frames_and_oversized_payloads() {
        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let stranger = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        socket.connect(relay.local_addr().unwrap()).await.unwrap();
        let client = SocksUdp {
            socket,
            closed: CancellationToken::new(),
            monitor: tokio::spawn(std::future::pending()),
            buffer: Mutex::new(vec![0; 65_535]),
        };
        let local = client.local_addr().unwrap();
        let source = "8.8.8.8:53".parse().unwrap();
        let envelope = encode(b"ok", &Target::Addr(source)).unwrap();
        stranger.send_to(&envelope, local).await.unwrap();
        let mut fragmented = envelope.clone();
        fragmented[2] = 1;
        relay.send_to(&fragmented, local).await.unwrap();
        let mut reserved = envelope.clone();
        reserved[0] = 1;
        relay.send_to(&reserved, local).await.unwrap();
        relay.send_to(&envelope[..5], local).await.unwrap();
        relay
            .send_to(&encode(b"too long", &Target::Addr(source)).unwrap(), local)
            .await
            .unwrap();
        let mut bytes = [0; 2];
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                client.recv(&mut bytes)
            )
            .await
            .is_err()
        );
        relay.send_to(&envelope, local).await.unwrap();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(2), client.recv(&mut bytes))
                .await
                .unwrap()
                .unwrap(),
            (2, source)
        );
        assert_eq!(&bytes, b"ok");
    }

    proptest! {
        #[test]
        fn arbitrary_envelopes_never_panic(packet in prop::collection::vec(any::<u8>(), 0..2048)) {
            if let Some((payload, addr)) = decode(&packet) {
                prop_assert_ne!(addr.port(), 0);
                prop_assert_eq!(encode(payload, &Target::Addr(addr)).unwrap(), packet);
            }
        }

        #[test]
        fn address_and_payload_round_trip(ip in any::<[u8; 16]>(), port in 1u16..=u16::MAX,
            payload in prop::collection::vec(any::<u8>(), 0..1400), ipv4 in any::<bool>()) {
            let address = if ipv4 { IpAddr::V4(Ipv4Addr::new(ip[0], ip[1], ip[2], ip[3])) }
                else { IpAddr::V6(Ipv6Addr::from(ip)) };
            let addr = SocketAddr::new(address, port);
            let packet = encode(&payload, &Target::Addr(addr)).unwrap();
            prop_assert_eq!(decode(&packet), Some((payload.as_slice(), addr)));
            for offset in 0..3 {
                let mut invalid = packet.clone();
                invalid[offset] = 1;
                prop_assert!(decode(&invalid).is_none());
            }
        }
    }
    #[test]
    fn a_public_proxy_cannot_name_a_local_relay() {
        let relay = |s: &str| s.parse::<SocketAddr>().unwrap();
        let public: IpAddr = "93.184.216.34".parse().unwrap();
        for local in [
            "127.0.0.1:1080",
            "192.168.1.1:1080",
            "10.0.0.1:1080",
            "169.254.169.254:80",
            "[::1]:1080",
            "[fd00::1]:1080",
            "0.0.0.0:1080",
        ] {
            assert!(check_relay(public, relay(local)).is_err(), "{local}");
        }
        assert!(check_relay(public, relay("93.184.216.35:40000")).is_ok());
        // A proxy on this computer or the local network may relay locally, or publicly.
        for proxy in ["127.0.0.1", "192.168.1.2", "::1"] {
            let proxy: IpAddr = proxy.parse().unwrap();
            for ok in ["127.0.0.1:1080", "192.168.1.2:40000", "93.184.216.35:40000"] {
                assert!(check_relay(proxy, relay(ok)).is_ok(), "{proxy} {ok}");
            }
        }
        // Never a multicast relay or port 0.
        let loopback: IpAddr = Ipv4Addr::LOCALHOST.into();
        assert!(check_relay(loopback, relay("224.0.0.1:1080")).is_err());
        assert!(check_relay(loopback, relay("127.0.0.1:0")).is_err());
    }

    #[test]
    fn envelopes_are_strict_and_preserve_addresses() {
        for addr in ["8.8.8.8:53", "[2001:4860:4860::8888]:53"] {
            let addr = addr.parse().unwrap();
            let packet = encode(b"payload", &Target::Addr(addr)).unwrap();
            assert_eq!(decode(&packet), Some((&b"payload"[..], addr)));
            for cut in 0..packet.len() - 7 {
                assert!(decode(&packet[..cut]).is_none());
            }
            let mut fragmented = packet.clone();
            fragmented[2] = 1;
            assert!(decode(&fragmented).is_none());
        }
        let domain = encode(b"x", &Target::Domain("tracker.invalid".into(), 6969)).unwrap();
        assert_eq!(domain[3], 3);
        assert!(decode(&domain).is_none());
        assert!(encode(&vec![0; 65_507], &Target::Domain("x".into(), 1)).is_err());
    }
}
