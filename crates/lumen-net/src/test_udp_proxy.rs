// SPDX-License-Identifier: MIT OR Apache-2.0

//! A loopback-only scripted UDP relay. It never forwards to the logical destination.

use crate::socks5::{self, Target};
use std::{
    io,
    net::{Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex, PoisonError},
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream, UdpSocket},
};
use tokio_util::sync::CancellationToken;

type Handler = dyn Fn(&Target, &[u8]) -> Option<(SocketAddr, Vec<u8>)> + Send + Sync;

/// A SOCKS5 test relay with scripted replies and recorded logical destinations.
#[derive(Debug)]
pub struct TestUdpProxy {
    addr: SocketAddr,
    targets: Arc<Mutex<Vec<Target>>>,
    cancel: CancellationToken,
}

impl Drop for TestUdpProxy {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl TestUdpProxy {
    /// Starts a no-authentication proxy. The handler supplies every response, so no traffic
    /// is sent to the requested destination, even when it is a public address or DNS name.
    ///
    /// # Errors
    /// Returns an error if the loopback listener cannot be bound.
    pub async fn start(
        handler: impl Fn(&Target, &[u8]) -> Option<(SocketAddr, Vec<u8>)> + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let addr = listener.local_addr()?;
        let targets = Arc::new(Mutex::new(Vec::new()));
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let records = Arc::clone(&targets);
        let handler: Arc<Handler> = Arc::new(handler);
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    () = token.cancelled() => return,
                    accepted = listener.accept() => accepted,
                };
                let Ok((stream, _)) = accepted else { continue };
                let (handler, records, token) =
                    (Arc::clone(&handler), Arc::clone(&records), token.clone());
                tokio::spawn(async move {
                    tokio::select! {
                        () = token.cancelled() => {},
                        _ = relay(stream, &records, &*handler) => {},
                    }
                });
            }
        });
        Ok(Self {
            addr,
            targets,
            cancel,
        })
    }

    /// Proxy TCP address.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
    /// Every UDP destination received, including remotely resolved names.
    #[must_use]
    pub fn targets(&self) -> Vec<Target> {
        self.targets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
    /// Closes control channels and relay sockets.
    pub fn stop(&self) {
        self.cancel.cancel();
    }
}

async fn relay(
    mut control: TcpStream,
    targets: &Mutex<Vec<Target>>,
    handler: &Handler,
) -> io::Result<()> {
    let mut greeting = [0; 2];
    control.read_exact(&mut greeting).await?;
    if greeting[0] != 5 {
        return Ok(());
    }
    let mut methods = vec![0; usize::from(greeting[1])];
    control.read_exact(&mut methods).await?;
    control.write_all(&[5, 0]).await?;
    let mut request = [0; 4];
    control.read_exact(&mut request).await?; // Health checks finish before sending a request.
    let _ = socks5::read_address(&mut control, request[3])
        .await
        .map_err(io::Error::other)?;
    if request[1] != 3 {
        control.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        return Ok(());
    }
    let relay = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let port = relay.local_addr()?.port();
    let mut reply = vec![5, 0, 0, 1, 0, 0, 0, 0];
    reply.extend(port.to_be_bytes());
    control.write_all(&reply).await?;
    let mut bytes = vec![0; 65_535];
    let mut byte = [0];
    loop {
        let (n, from) = tokio::select! {
            _ = control.read(&mut byte) => return Ok(()),
            datagram = relay.recv_from(&mut bytes) => datagram?,
        };
        let packet = &bytes[..n];
        if packet.get(..3) != Some(&[0, 0, 0]) || packet.len() < 4 {
            continue;
        }
        let mut body = &packet[4..];
        let target = socks5::read_address(&mut body, packet[3])
            .await
            .map_err(io::Error::other)?;
        targets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(target.clone());
        if let Some((source, payload)) = handler(&target, body) {
            let reply = crate::socks_udp::encode(&payload, &Target::Addr(source))?;
            relay.send_to(&reply, from).await?;
        }
    }
}
