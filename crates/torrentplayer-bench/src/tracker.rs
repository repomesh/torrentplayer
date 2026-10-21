// SPDX-License-Identifier: MIT OR Apache-2.0

//! A minimal HTTP tracker on the loopback interface, so benchmarks can model peers that join a
//! swarm late: each peer is announced only once its [`PeerProfile::announce_after`] has passed.
//! It also remembers where the viewer said it accepts connections, so seeders can connect to
//! the viewer instead (see [`crate::link::Dialers`]).
//!
//! [`PeerProfile::announce_after`]: crate::link::PeerProfile::announce_after

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

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
    announcer: watch::Sender<Option<SocketAddr>>,
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
        let viewer = watch::Sender::new(None);
        let (listing, count, seen) = (Arc::clone(&peers), Arc::clone(&announces), viewer.clone());
        let task = tokio::spawn(async move {
            while let Ok((stream, from)) = listener.accept().await {
                let listing = Arc::clone(&listing);
                let clock = clock.clone();
                let seen = seen.clone();
                count.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(async move {
                    let _ = answer(stream, from, interval, &listing, &clock, &seen).await;
                });
            }
        });
        Ok(Self {
            url,
            peers,
            announces,
            announcer: viewer,
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

    /// Where the client that last announced (other than to leave) accepts connections: the
    /// address it announced from, with the port it announced. Only the viewer announces; the
    /// seeders never do.
    #[must_use]
    pub fn announcer(&self) -> watch::Receiver<Option<SocketAddr>> {
        self.announcer.subscribe()
    }
}

/// The listening address in an announce request head: `from`'s IP with the `port` parameter.
/// `None` for a `stopped` event or a request without a port.
fn announced_addr(head: &[u8], from: SocketAddr) -> Option<SocketAddr> {
    let line = head.split(|&b| b == b'\r').next()?;
    let target = std::str::from_utf8(line).ok()?.split(' ').nth(1)?;
    let (_, query) = target.split_once('?')?;
    let mut port = None;
    for (key, value) in query.split('&').filter_map(|kv| kv.split_once('=')) {
        match key {
            "port" => port = value.parse::<u16>().ok().filter(|p| *p != 0),
            "event" if value == "stopped" => return None,
            _ => {}
        }
    }
    Some(SocketAddr::new(from.ip(), port?))
}

async fn answer(
    mut stream: TcpStream,
    from: SocketAddr,
    interval: Duration,
    listing: &Listing,
    clock: &Clock,
    announcer: &watch::Sender<Option<SocketAddr>>,
) -> std::io::Result<()> {
    // Read the request head; only the announced port matters.
    let mut head = Vec::new();
    let mut buf = [0u8; 2048];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 16 * 1024 {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        head.extend_from_slice(&buf[..n]);
    }
    if let Some(addr) = announced_addr(&head, from) {
        tracing::debug!(%addr, "announce");
        announcer.send_if_modified(|last| last.replace(addr) != Some(addr));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announces_give_the_listening_address() {
        let from: SocketAddr = "127.0.0.1:50000".parse().unwrap();
        let head = b"GET /announce?info_hash=%01%02&peer_id=x&port=41234&left=0 HTTP/1.1\r\n\r\n";
        assert_eq!(
            announced_addr(head, from),
            Some("127.0.0.1:41234".parse().unwrap())
        );
        let stopped = b"GET /announce?port=41234&event=stopped HTTP/1.1\r\n\r\n";
        assert_eq!(announced_addr(stopped, from), None);
        let portless = b"GET /announce?info_hash=%01 HTTP/1.1\r\n\r\n";
        assert_eq!(announced_addr(portless, from), None);
    }
}
