// SPDX-License-Identifier: MIT OR Apache-2.0

//! A minimal HTTP tracker on the loopback interface, so benchmarks can model peers that join a
//! swarm late: each peer is announced only once its [`PeerProfile::announce_after`] has passed.
//!
//! [`PeerProfile::announce_after`]: crate::link::PeerProfile::announce_after

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

use crate::link::Clock;

/// Peers to announce, each with the time (on the clock) from which it is listed.
type Listing = Arc<Mutex<Vec<(SocketAddr, Duration)>>>;

/// A running tracker. Stops when dropped.
#[derive(Debug)]
pub struct LocalTracker {
    /// The announce URL to put in the `.torrent` file.
    pub url: String,
    peers: Listing,
    announces: Arc<AtomicU64>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for LocalTracker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl LocalTracker {
    /// Starts a tracker that asks clients to re-announce every `interval` (also its minimum
    /// interval).
    ///
    /// # Errors
    ///
    /// Returns an error if the loopback listener cannot be bound.
    pub async fn start(interval: Duration, clock: Clock) -> std::io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let url = format!("http://{}/announce", listener.local_addr()?);
        let peers: Listing = Arc::default();
        let announces = Arc::new(AtomicU64::new(0));
        let (listing, count) = (Arc::clone(&peers), Arc::clone(&announces));
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let listing = Arc::clone(&listing);
                let clock = clock.clone();
                count.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(async move {
                    let _ = answer(stream, interval, &listing, &clock).await;
                });
            }
        });
        Ok(Self {
            url,
            peers,
            announces,
            task,
        })
    }

    /// Sets the peers to announce.
    pub fn set_peers(&self, peers: Vec<(SocketAddr, Duration)>) {
        *self.peers.lock().unwrap_or_else(PoisonError::into_inner) = peers;
    }

    /// Announces received so far.
    #[must_use]
    pub fn announces(&self) -> u64 {
        self.announces.load(Ordering::Relaxed)
    }
}

async fn answer(
    mut stream: TcpStream,
    interval: Duration,
    listing: &Listing,
    clock: &Clock,
) -> std::io::Result<()> {
    // Read the request head; its contents do not matter.
    let mut head = Vec::new();
    let mut buf = [0u8; 2048];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 16 * 1024 {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        head.extend_from_slice(&buf[..n]);
    }
    let now = clock.elapsed();
    let compact: Vec<u8> = listing
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter(|(_, after)| *after <= now)
        .filter_map(|(addr, _)| match addr {
            SocketAddr::V4(v4) => {
                Some([v4.ip().octets().as_slice(), &v4.port().to_be_bytes()].concat())
            }
            SocketAddr::V6(_) => None,
        })
        .flatten()
        .collect();
    let secs = interval.as_secs().max(1);
    let mut body = format!(
        "d8:intervali{secs}e12:min intervali{secs}e5:peers{}:",
        compact.len()
    )
    .into_bytes();
    body.extend_from_slice(&compact);
    body.push(b'e');
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(&body);
    stream.write_all(&response).await?;
    stream.shutdown().await
}
