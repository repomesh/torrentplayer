// SPDX-License-Identifier: MIT OR Apache-2.0

//! The loopback bridge: a SOCKS5 server on `127.0.0.1` that sends every connection it is asked
//! for through [`Network`].
//!
//! HTTP clients (`reqwest`) and librqbit cannot use TorrentStream's connector directly, but both speak
//! SOCKS5. Pointing them at the bridge gives them the same rules as everything else: the
//! current mode, the kill switch, and closing connections when the epoch changes, without
//! rebuilding the clients. A random token (RFC 1929 password) keeps other local programs from
//! using the bridge. Clients that authenticate as [`PUBLIC_USER`] are only connected to public
//! addresses ([`Network::connect_host_public`]), for URLs from untrusted sources.

use std::sync::Weak;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};

use crate::error::NetError;
use crate::network::{Inner, Network};
use crate::secret::Secret;
use crate::socks5::{self, Reply, ServerAuth, Socks5Error, Target};

/// Username clients authenticate with; the password is the per-run token.
pub(crate) const USER: &str = "lumen";
/// Username for connections that may only reach public addresses.
pub(crate) const PUBLIC_USER: &str = "lumen-public";
/// Time allowed for a client to finish the SOCKS5 handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// A random token, as hex.
pub(crate) fn new_token() -> Result<Secret, NetError> {
    let mut bytes = zeroize::Zeroizing::new([0u8; 24]);
    getrandom::fill(bytes.as_mut())
        .map_err(|e| NetError::Unsupported(format!("no secure random numbers: {e}")))?;
    let mut hex = String::with_capacity(48);
    for b in bytes.iter() {
        hex.push(char::from(b"0123456789abcdef"[usize::from(b >> 4)]));
        hex.push(char::from(b"0123456789abcdef"[usize::from(b & 0xf)]));
    }
    Ok(Secret::new(hex))
}

/// Accepts bridge clients until `cancel` fires or the network is dropped.
pub(crate) async fn serve(listener: TcpListener, network: Weak<Inner>, cancel: CancellationToken) {
    loop {
        let accepted = tokio::select! {
            () = cancel.cancelled() => return,
            accepted = listener.accept() => accepted,
        };
        let (stream, peer) = match accepted {
            Ok(ok) => ok,
            Err(error) => {
                debug!(%error, "bridge accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        if !peer.ip().is_loopback() {
            continue;
        }
        let Some(inner) = network.upgrade() else {
            return;
        };
        let network = Network { inner };
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if let Err(error) = handle(stream, network, cancel).await {
                trace!(%error, "bridge connection ended");
            }
        });
    }
}

async fn handle(
    mut client: TcpStream,
    network: Network,
    cancel: CancellationToken,
) -> Result<(), NetError> {
    let token = network.inner.bridge_token.clone();
    let connect_timeout = network.inner.options.connect_timeout;
    let auth = ServerAuth::AnyUser {
        usernames: &[USER, PUBLIC_USER],
        password: token.expose(),
    };
    let (target, user) =
        tokio::time::timeout(HANDSHAKE_TIMEOUT, socks5::accept_as(&mut client, auth))
            .await
            .map_err(|_| NetError::Timeout)??;
    drop(token);
    let public_only = user == 1;
    let connected = tokio::time::timeout(connect_timeout, async {
        match &target {
            Target::Addr(addr) if public_only && !crate::is_public_ip(addr.ip()) => {
                Err(NetError::NotPublic)
            }
            Target::Addr(addr) => network.connect(*addr).await,
            Target::Domain(host, port) if public_only => {
                network.connect_host_public(host, *port).await
            }
            Target::Domain(host, port) => network.connect_host(host, *port).await,
        }
    })
    .await
    .unwrap_or(Err(NetError::Timeout));
    // Relays hold only their lease, so dropping every `Network` handle stops the bridge.
    drop(network);
    let (mut upstream, mut lease) = match connected {
        Ok(ok) => ok,
        Err(error) => {
            let _ = socks5::send_reply(&mut client, reply_for(&error)).await;
            return Err(error);
        }
    };
    socks5::send_reply(&mut client, Reply::Succeeded).await?;
    tokio::select! {
        result = tokio::io::copy_bidirectional(&mut client, &mut upstream) => {
            result?;
        }
        () = lease.revoked() => trace!("bridge connection closed: network changed"),
        () = cancel.cancelled() => {}
    }
    Ok(())
}

/// The SOCKS5 reply for a failed connection.
fn reply_for(error: &NetError) -> Reply {
    match error {
        NetError::KillSwitch(_) | NetError::Ipv6Disabled | NetError::NotPublic => Reply::NotAllowed,
        NetError::Timeout => Reply::TtlExpired,
        NetError::Proxy(Socks5Error::Refused(reply)) => *reply,
        NetError::InterfaceUnavailable(_) | NetError::ProxyUnreachable(_) => {
            Reply::NetworkUnreachable
        }
        NetError::Io(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            Reply::ConnectionRefused
        }
        _ => Reply::GeneralFailure,
    }
}
