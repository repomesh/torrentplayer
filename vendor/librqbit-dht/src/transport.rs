// SPDX-License-Identifier: Apache-2.0

//! Application-provided datagram and bootstrap lookup transport. A protected caller can
//! supply already-scoped sockets instead of allowing the DHT to bind or resolve on its own.

use futures::future::BoxFuture;
use std::{io, net::SocketAddr};

/// All DHT I/O, including bootstrap name lookup, through one application's network path.
pub trait DhtTransport: Send + Sync {
    /// Local address used for node diagnostics.
    fn local_addr(&self) -> SocketAddr;
    /// Sends one DHT message through the scoped path.
    ///
    /// # Errors
    /// Returns transport or policy errors without an alternate network path.
    fn send_to<'a>(
        &'a self,
        bytes: &'a [u8],
        target: SocketAddr,
    ) -> BoxFuture<'a, io::Result<usize>>;
    /// Receives one message and its remote address.
    ///
    /// # Errors
    /// Returns transport or lease-revocation errors.
    fn recv_from<'a>(
        &'a self,
        bytes: &'a mut [u8],
    ) -> BoxFuture<'a, io::Result<(usize, SocketAddr)>>;
    /// Resolves a bootstrap endpoint (`host:port`) using the scoped resolver.
    ///
    /// # Errors
    /// Returns resolution or policy errors without a system-DNS fallback.
    fn resolve<'a>(&'a self, host: &'a str) -> BoxFuture<'a, io::Result<Vec<SocketAddr>>>;
}
