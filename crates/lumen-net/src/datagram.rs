// SPDX-License-Identifier: MIT OR Apache-2.0

//! Datagram I/O through the selected network path.

use std::future::Future;
use std::io;
use std::net::SocketAddr;

/// A UDP socket bound to an OS interface or carried inside TorrentStream's WireGuard tunnel.
pub struct DatagramSocket {
    inner: Datagram,
    lease: Option<crate::Lease>,
}

enum Datagram {
    Os(tokio::net::UdpSocket),
    WireGuard(tokio_wireguard::UdpSocket),
}

impl std::fmt::Debug for DatagramSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatagramSocket")
            .field("local_addr", &self.local_addr().ok())
            .finish()
    }
}

impl DatagramSocket {
    pub(crate) fn wireguard(socket: tokio_wireguard::UdpSocket) -> Self {
        Self {
            inner: Datagram::WireGuard(socket),
            lease: None,
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
        }
    }

    /// Sends one datagram through this socket's network path.
    ///
    /// # Errors
    ///
    /// Returns an error if the lease is revoked, the tunnel closes, or the send fails.
    pub async fn send_to(&self, bytes: &[u8], target: SocketAddr) -> io::Result<usize> {
        self.permitted(async {
            match &self.inner {
                Datagram::Os(socket) => socket.send_to(bytes, target).await,
                Datagram::WireGuard(socket) => socket.send_to(bytes, target).await,
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
            match &self.inner {
                Datagram::Os(socket) => socket.recv_from(bytes).await,
                Datagram::WireGuard(socket) => socket.recv_from(bytes).await,
            }
        })
        .await
    }
}

impl From<tokio::net::UdpSocket> for DatagramSocket {
    fn from(socket: tokio::net::UdpSocket) -> Self {
        Self {
            inner: Datagram::Os(socket),
            lease: None,
        }
    }
}
