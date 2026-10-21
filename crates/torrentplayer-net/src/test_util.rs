// SPDX-License-Identifier: MIT OR Apache-2.0

//! Loopback helpers for tests (feature `test-util`): a SOCKS5 proxy that records what it
//! relays and can be stopped and restarted on the same port, and a recording TCP relay to put
//! in front of a seeder to see who connects to it.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::socks5::{self, Reply, ServerAuth, Target};
#[path = "test_udp_proxy.rs"]
mod udp;
pub use udp::TestUdpProxy;

/// One connection relayed by a [`TestProxy`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxiedConnection {
    /// What the client asked for.
    pub target: String,
    /// Local address of the proxy's outgoing connection: what the destination saw as the peer.
    pub outbound_local: SocketAddr,
}

#[derive(Debug, Default)]
struct Shared {
    records: Mutex<Vec<ProxiedConnection>>,
    accepted: Mutex<u64>,
}

/// A SOCKS5 proxy on `127.0.0.1` for tests.
#[derive(Debug)]
pub struct TestProxy {
    addr: SocketAddr,
    credentials: Option<(String, String)>,
    shared: Arc<Shared>,
    running: Mutex<Option<(CancellationToken, JoinHandle<()>)>>,
}

impl TestProxy {
    /// Starts a proxy on a free port, requiring `credentials` if given.
    ///
    /// # Errors
    ///
    /// Returns an error if no port can be bound.
    pub async fn start(credentials: Option<(&str, &str)>) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let proxy = Self {
            addr: listener.local_addr()?,
            credentials: credentials.map(|(u, p)| (u.to_owned(), p.to_owned())),
            shared: Arc::default(),
            running: Mutex::new(None),
        };
        proxy.run(listener);
        Ok(proxy)
    }

    /// The proxy's address.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Every connection relayed so far.
    #[must_use]
    pub fn connections(&self) -> Vec<ProxiedConnection> {
        self.shared
            .records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Client connections accepted so far, including health checks.
    #[must_use]
    pub fn accepted(&self) -> u64 {
        *self
            .shared
            .accepted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Stops listening and cuts every relayed connection.
    pub async fn stop(&self) {
        let running = self
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some((cancel, task)) = running {
            cancel.cancel();
            let _ = task.await;
        }
    }

    /// Listens again on the same port.
    ///
    /// # Errors
    ///
    /// Returns an error if the port cannot be bound again.
    pub async fn restart(&self) -> io::Result<()> {
        self.stop().await;
        let listener = TcpListener::bind(self.addr).await?;
        self.run(listener);
        Ok(())
    }

    fn run(&self, listener: TcpListener) {
        let cancel = CancellationToken::new();
        let shared = Arc::clone(&self.shared);
        let credentials = self.credentials.clone();
        let token = cancel.clone();
        let task = tokio::spawn(async move {
            let mut relays = tokio::task::JoinSet::new();
            loop {
                let accepted = tokio::select! {
                    () = token.cancelled() => break,
                    accepted = listener.accept() => accepted,
                };
                let Ok((stream, _)) = accepted else {
                    continue;
                };
                *shared
                    .accepted
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) += 1;
                let shared = Arc::clone(&shared);
                let credentials = credentials.clone();
                let token = token.clone();
                relays.spawn(async move {
                    tokio::select! {
                        () = token.cancelled() => {}
                        _ = relay(stream, &shared, credentials.as_ref()) => {}
                    }
                });
            }
            drop(listener);
            relays.shutdown().await;
        });
        *self.running.lock().unwrap_or_else(PoisonError::into_inner) = Some((cancel, task));
    }
}

async fn relay(
    mut client: TcpStream,
    shared: &Shared,
    credentials: Option<&(String, String)>,
) -> Result<(), socks5::Socks5Error> {
    let auth = credentials.map_or(ServerAuth::None, |(username, password)| {
        ServerAuth::Password { username, password }
    });
    let target = socks5::accept(&mut client, auth).await?;
    let upstream = match &target {
        Target::Addr(addr) => TcpStream::connect(addr).await,
        Target::Domain(host, port) => TcpStream::connect((host.as_str(), *port)).await,
    };
    let mut upstream = match upstream {
        Ok(s) => s,
        Err(error) => {
            socks5::send_reply(&mut client, Reply::ConnectionRefused).await?;
            return Err(error.into());
        }
    };
    shared
        .records
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(ProxiedConnection {
            target: target.to_string(),
            outbound_local: upstream.local_addr()?,
        });
    socks5::send_reply(&mut client, Reply::Succeeded).await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

/// A TCP relay that records the peer address of every connection it accepts, to check who
/// connected to the service behind it.
#[derive(Debug)]
pub struct RecordingRelay {
    addr: SocketAddr,
    peers: Arc<Mutex<Vec<SocketAddr>>>,
    cancel: CancellationToken,
}

impl RecordingRelay {
    /// Listens on a free loopback port and forwards connections to `upstream`.
    ///
    /// # Errors
    ///
    /// Returns an error if no port can be bound.
    pub async fn start(upstream: SocketAddr) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let addr = listener.local_addr()?;
        let peers = Arc::new(Mutex::new(Vec::new()));
        let cancel = CancellationToken::new();
        let (record, token) = (Arc::clone(&peers), cancel.clone());
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    () = token.cancelled() => return,
                    accepted = listener.accept() => accepted,
                };
                let Ok((mut stream, peer)) = accepted else {
                    continue;
                };
                record
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(peer);
                let token = token.clone();
                tokio::spawn(async move {
                    let Ok(mut server) = TcpStream::connect(upstream).await else {
                        return;
                    };
                    tokio::select! {
                        () = token.cancelled() => {}
                        _ = tokio::io::copy_bidirectional(&mut stream, &mut server) => {}
                    }
                });
            }
        });
        Ok(Self {
            addr,
            peers,
            cancel,
        })
    }

    /// The relay's address.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Peer addresses of every connection accepted so far.
    #[must_use]
    pub fn peers(&self) -> Vec<SocketAddr> {
        self.peers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Drop for RecordingRelay {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
