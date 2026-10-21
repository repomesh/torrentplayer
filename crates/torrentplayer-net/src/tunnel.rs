// SPDX-License-Identifier: MIT OR Apache-2.0

//! An application-local WireGuard tunnel. Only encrypted UDP reaches the OS network.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio_wireguard::{Interface, config, x25519};
use zeroize::Zeroizing;

use crate::{NetError, WireGuardConfig};

#[derive(Clone, Debug)]
pub(crate) struct Tunnel(Arc<Session>);

#[derive(Debug)]
struct Session {
    interface: Interface,
    dns: Vec<IpAddr>,
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
    pub(crate) async fn start(config: &WireGuardConfig) -> Result<Self, NetError> {
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
        Ok(Self(Arc::new(Session {
            interface,
            dns: config
                .interface
                .dns
                .iter()
                .map(|ip| {
                    ip.parse()
                        .map_err(|_| invalid("invalid WireGuard DNS server"))
                })
                .collect::<Result<_, _>>()?,
        })))
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

    pub(crate) async fn healthy(&self, timeout: Duration) -> Result<(), NetError> {
        tokio::time::timeout(timeout, async {
            let mut requested = false;
            loop {
                if self.0.interface.handshake_age().await?
                    .is_some_and(|age| age < Duration::from_secs(180)) {
                    return Ok(());
                }
                if !requested {
                    self.0.interface.request_handshake()?;
                    requested = true;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }).await.map_err(|_| NetError::InterfaceUnavailable(
            "the WireGuard server has not completed a recent handshake; check the endpoint and keys".into()
        ))?
    }

    /// Preserve the connector's OS `TcpStream` API using a private loopback pair. The accepting
    /// side checks the exact source port we reserved, so another local process cannot take it.
    pub(crate) async fn connect(&self, target: SocketAddr) -> Result<TcpStream, NetError> {
        let mut upstream = tokio_wireguard::TcpStream::connect(target, &self.0.interface).await?;
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
    let mut id = [0; 2];
    getrandom::fill(&mut id).map_err(|_| invalid("could not generate a DNS query ID"))?;
    // Build the small question directly: dns-parser's builder panics on 63-byte DNS labels.
    let mut query = vec![id[0], id[1], 1, 0, 0, 1, 0, 0, 0, 0, 0, 0];
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
        let reservation = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let server = Interface::new(config::Config {
            interface: config::Interface {
                address: "10.44.0.1/32".parse().unwrap(),
                listen_port: Some(port),
                private_key: server_key,
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
            let tunnel = Tunnel::start(&config).await.unwrap();
            tunnel.healthy(Duration::from_secs(2)).await.unwrap();
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
            let tunnel = Tunnel::start(&config).await.unwrap();
            tunnel.healthy(Duration::from_secs(3)).await.unwrap();
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
        let tunnel = Tunnel::start(&config).await.unwrap();
        assert!(matches!(
            tunnel.resolve("api.ipify.org", 443, true).await,
            Err(NetError::InvalidConfig(_))
        ));
        tunnel.close();
        server.close();
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
