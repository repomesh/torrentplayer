// SPDX-License-Identifier: MIT OR Apache-2.0

//! Datagram I/O through the selected network path.

use std::future::Future;
use std::io;
use std::net::SocketAddr;

/// A leased UDP path through an OS interface, WireGuard tunnel, or SOCKS5 relay.
pub struct DatagramSocket {
    inner: Datagram,
    lease: Option<crate::Lease>,
    ipv4_only: bool,
}

enum Datagram {
    Os(tokio::net::UdpSocket),
    WireGuard(tokio_wireguard::UdpSocket),
    Socks(crate::socks_udp::SocksUdp),
}

impl std::fmt::Debug for DatagramSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatagramSocket")
            .field("local_addr", &self.local_addr().ok())
            .finish()
    }
}

impl DatagramSocket {
    pub(crate) fn socks(socket: crate::socks_udp::SocksUdp) -> Self {
        Self {
            inner: Datagram::Socks(socket),
            lease: None,
            ipv4_only: false,
        }
    }

    pub(crate) fn ipv4_only(mut self, only: bool) -> Self {
        self.ipv4_only = only;
        self
    }

    pub(crate) fn wireguard(socket: tokio_wireguard::UdpSocket) -> Self {
        Self {
            inner: Datagram::WireGuard(socket),
            lease: None,
            ipv4_only: false,
        }
    }

    pub(crate) fn with_lease(mut self, lease: crate::Lease) -> Self {
        self.lease = Some(lease);
        self
    }

    async fn permitted<T>(&self, operation: impl Future<Output = io::Result<T>>) -> io::Result<T> {
        let Some(mut lease) = self.lease.clone() else {
            return operation.await;
        };
        let revoked = || {
            io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "the network settings changed",
            )
        };
        if !lease.is_valid() {
            return Err(revoked());
        }
        tokio::select! {
            biased;
            () = lease.revoked() => Err(revoked()),
            result = operation => result,
        }
    }

    /// The socket's local address, inside the tunnel for WireGuard.
    ///
    /// # Errors
    ///
    /// Returns the socket error if its address is no longer available.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        match &self.inner {
            Datagram::Os(socket) => socket.local_addr(),
            Datagram::WireGuard(socket) => socket.local_addr(),
            Datagram::Socks(socket) => socket.local_addr(),
        }
    }

    /// Sends one datagram through this socket's network path.
    ///
    /// # Errors
    ///
    /// Returns an error if the lease is revoked, the tunnel closes, or the send fails.
    pub async fn send_to(&self, bytes: &[u8], target: SocketAddr) -> io::Result<usize> {
        if self.ipv4_only && target.is_ipv6() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "IPv6 is disabled",
            ));
        }
        self.permitted(async {
            match &self.inner {
                Datagram::Os(socket) => socket.send_to(bytes, target).await,
                Datagram::WireGuard(socket) => socket.send_to(bytes, target).await,
                Datagram::Socks(socket) => {
                    socket
                        .send(bytes, &crate::socks5::Target::Addr(target))
                        .await
                }
            }
        })
        .await
    }

    /// Receives one datagram through this socket's network path.
    ///
    /// # Errors
    ///
    /// Returns an error if the lease is revoked, the tunnel closes, or the receive fails.
    pub async fn recv_from(&self, bytes: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.permitted(async {
            loop {
                let received = match &self.inner {
                    Datagram::Os(socket) => socket.recv_from(bytes).await?,
                    Datagram::WireGuard(socket) => socket.recv_from(&mut *bytes).await?,
                    Datagram::Socks(socket) => socket.recv(bytes).await?,
                };
                if !self.ipv4_only || received.1.is_ipv4() {
                    return Ok(received);
                }
            }
        })
        .await
    }

    /// Sends to an IP address, or passes a host name to a SOCKS5 UDP relay for remote DNS.
    ///
    /// # Errors
    /// Returns an error for a revoked lease, unsupported domain target, or failed send.
    pub async fn send_to_target(
        &self,
        bytes: &[u8],
        target: &crate::socks5::Target,
    ) -> io::Result<usize> {
        match target {
            crate::socks5::Target::Addr(addr) => self.send_to(bytes, *addr).await,
            crate::socks5::Target::Domain(_, _) => {
                self.permitted(async {
                    match &self.inner {
                        Datagram::Socks(socket) => socket.send(bytes, target).await,
                        _ => Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "a domain datagram requires a SOCKS5 relay",
                        )),
                    }
                })
                .await
            }
        }
    }
}

impl From<tokio::net::UdpSocket> for DatagramSocket {
    fn from(socket: tokio::net::UdpSocket) -> Self {
        Self {
            inner: Datagram::Os(socket),
            lease: None,
            ipv4_only: false,
        }
    }
}
