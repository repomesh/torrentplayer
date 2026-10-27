// SPDX-License-Identifier: MIT OR Apache-2.0

//! DHT datagrams and bootstrap names through the application's current network lease.

use futures_util::future::BoxFuture;
use librqbit_dht::DhtTransport;
use std::{
    io,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use torrentplayer_net::{DatagramSocket, Network, socks5::Target};

pub(crate) struct ScopedDht {
    v4: Option<DatagramSocket>,
    v6: Option<DatagramSocket>,
    network: Network,
    local: SocketAddr,
    epoch: u64,
    /// Where an IPv6 datagram is received while IPv4 is awaited into the caller's buffer;
    /// kept so that each datagram does not allocate one.
    v6_buffer: tokio::sync::Mutex<Vec<u8>>,
}

impl ScopedDht {
    pub(crate) async fn open(network: Network) -> io::Result<Arc<dyn DhtTransport>> {
        let policy = network.policy();
        let v4 = network
            .udp_socket(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))
            .await
            .ok()
            .map(|(socket, _)| socket);
        let v6 = network
            .udp_socket(SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)))
            .await
            .ok()
            .map(|(socket, _)| socket);
        if network.policy() != policy || !policy.traffic_allowed {
            return Err(io::Error::other(
                "the network mode changed during DHT setup",
            ));
        }
        let local = v4
            .as_ref()
            .or(v6.as_ref())
            .ok_or_else(|| io::Error::other("no scoped UDP address family is available"))?
            .local_addr()?;
        Ok(Arc::new(Self {
            v4,
            v6,
            network,
            local,
            epoch: policy.epoch,
            v6_buffer: tokio::sync::Mutex::new(Vec::new()),
        }))
    }

    fn check_policy(&self) -> io::Result<()> {
        let policy = self.network.policy();
        if policy.epoch != self.epoch || !policy.traffic_allowed || !policy.dht_allowed {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "the DHT network lease ended",
            ));
        }
        Ok(())
    }
}

impl DhtTransport for ScopedDht {
    fn local_addr(&self) -> SocketAddr {
        self.local
    }
    fn send_to<'a>(
        &'a self,
        bytes: &'a [u8],
        target: SocketAddr,
    ) -> BoxFuture<'a, io::Result<usize>> {
        Box::pin(async move {
            if !usable(target) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "DHT addresses must be public",
                ));
            }
            let socket = if target.is_ipv4() {
                self.v4.as_ref()
            } else {
                self.v6.as_ref()
            }
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "DHT address family is unavailable",
                )
            })?;
            socket.send_to(bytes, target).await
        })
    }
    fn recv_from<'a>(
        &'a self,
        bytes: &'a mut [u8],
    ) -> BoxFuture<'a, io::Result<(usize, SocketAddr)>> {
        Box::pin(async move {
            loop {
                let received = if let (Some(v4), Some(v6)) = (&self.v4, &self.v6) {
                    let mut v6_bytes = self.v6_buffer.lock().await;
                    v6_bytes.resize(bytes.len(), 0);
                    tokio::select! {
                        result = v4.recv_from(bytes) => result?,
                        result = v6.recv_from(&mut v6_bytes) => {
                            let (n, from) = result?; bytes[..n].copy_from_slice(&v6_bytes[..n]); (n, from)
                        }
                    }
                } else {
                    let socket = self
                        .v4
                        .as_ref()
                        .or(self.v6.as_ref())
                        .ok_or_else(|| io::Error::other("no scoped UDP socket"))?;
                    socket.recv_from(bytes).await?
                };
                if usable(received.1) {
                    return Ok(received);
                }
            }
        })
    }
    fn resolve<'a>(&'a self, host: &'a str) -> BoxFuture<'a, io::Result<Vec<SocketAddr>>> {
        Box::pin(async move {
            self.check_policy()?;
            let (name, port) = host
                .rsplit_once(':')
                .ok_or_else(|| io::Error::other("invalid DHT bootstrap endpoint"))?;
            let port: u16 = port.parse().map_err(io::Error::other)?;
            match self.network.resolve_host(name, port).await {
                Ok(addresses) => {
                    self.check_policy()?;
                    return Ok(addresses.into_iter().filter(|addr| usable(*addr)).collect());
                }
                // Remote-DNS SOCKS5 has no lookup command; use a domain-addressed ping below.
                Err(torrentplayer_net::NetError::Unsupported(_)) => {}
                Err(error) => return Err(io::Error::other(error)),
            }
            // SOCKS5 does not have a DNS lookup command. A domain-addressed DHT ping lets
            // the proxy resolve the bootstrap name; its authenticated relay reports the IP.
            // Other modes resolve through their normal scoped resolver (including WireGuard DNS).
            let (socket, target, lease) = self
                .network
                .udp_socket_host(name, port)
                .await
                .map_err(io::Error::other)?;
            self.check_policy()?;
            if let Target::Addr(addr) = target {
                return Ok(if usable(addr) { vec![addr] } else { Vec::new() });
            }
            let mut transaction = [0; 8];
            getrandom::fill(&mut transaction).map_err(io::Error::other)?;
            let mut node = [0; 20];
            getrandom::fill(&mut node).map_err(io::Error::other)?;
            let mut ping = b"d1:ad2:id20:".to_vec();
            ping.extend(node);
            ping.extend(b"e1:q4:ping1:t8:");
            ping.extend(transaction);
            ping.extend(b"1:y1:qe");
            tokio::time::timeout(Duration::from_secs(5), async {
                socket.send_to_target(&ping, &target).await?;
                let mut bytes = [0; 2048];
                loop {
                    let (n, from) = socket.recv_from(&mut bytes).await?;
                    if lease.is_valid()
                        && from.port() == port
                        && usable(from)
                        && ping_reply(&bytes[..n], &transaction)
                    {
                        return Ok(vec![from]);
                    }
                }
            })
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "DHT bootstrap ping timed out"))?
        })
    }
}

fn usable(addr: SocketAddr) -> bool {
    addr.port() != 0 && torrentplayer_net::is_public_ip(addr.ip())
}

fn ping_reply(bytes: &[u8], transaction: &[u8]) -> bool {
    let Ok(reply) = crate::bencode::decode(bytes) else {
        return false;
    };
    reply.get(b"y").and_then(crate::bencode::Value::as_bytes) == Some(&b"r"[..])
        && reply.get(b"t").and_then(crate::bencode::Value::as_bytes) == Some(transaction)
        && reply
            .get(b"r")
            .and_then(|r| r.get(b"id"))
            .and_then(crate::bencode::Value::as_bytes)
            .is_some_and(|id| id.len() == 20)
}

#[cfg(test)]
mod tests {
    use super::*;
    use torrentplayer_net::{
        NetworkConfig, NetworkMode, NetworkOptions, Socks5Config, test_util::TestUdpProxy,
    };

    #[tokio::test]
    async fn ipv6_only_transport_uses_the_relay_and_stops_on_mode_change() {
        let destination: SocketAddr = "[2001:4860:4860::8888]:6881".parse().unwrap();
        let proxy = TestUdpProxy::start(move |_, bytes| Some((destination, bytes.to_vec())))
            .await
            .unwrap();
        let network = Network::start(
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
                allow_ipv6: true,
                hold: None,
            },
            NetworkOptions {
                loopback_only: true,
                ..NetworkOptions::default()
            },
        )
        .await
        .unwrap();
        assert!(
            network
                .settled(Duration::from_secs(3))
                .await
                .traffic_allowed
        );
        let (v6, _) = network.udp_socket(destination).await.unwrap();
        let transport = ScopedDht {
            local: v6.local_addr().unwrap(),
            v4: None,
            v6: Some(v6),
            network: network.clone(),
            epoch: network.policy().epoch,
            v6_buffer: tokio::sync::Mutex::new(Vec::new()),
        };
        transport.send_to(b"v6", destination).await.unwrap();
        let mut bytes = [0; 32];
        let (n, from) =
            tokio::time::timeout(Duration::from_secs(3), transport.recv_from(&mut bytes))
                .await
                .unwrap()
                .unwrap();
        assert_eq!((&bytes[..n], from), (&b"v6"[..], destination));
        assert!(
            transport
                .send_to(b"v4", "8.8.8.8:6881".parse().unwrap())
                .await
                .is_err()
        );
        network.apply(NetworkConfig::default()).unwrap();
        assert!(transport.send_to(b"old", destination).await.is_err());
        assert!(transport.recv_from(&mut bytes).await.is_err());
        assert!(
            transport
                .resolve("never-resolves.invalid:6881")
                .await
                .is_err()
        );
        network.shutdown();
    }
}
