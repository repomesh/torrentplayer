// SPDX-License-Identifier: MIT OR Apache-2.0

//! A minimal HTTP/1.1 range client that stands in for a media player and records timings.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

/// Timings for one range request, measured from when it was sent.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// Until the first body byte arrived.
    pub first_byte: Duration,
    /// Until the bytes the caller needed arrived.
    pub complete: Duration,
}

/// A published stream URL split into its parts.
#[derive(Debug, Clone)]
pub struct Target {
    /// Server address.
    pub addr: SocketAddr,
    /// `Host` header value.
    pub host: String,
    /// Path and query, including the token.
    pub path: String,
}

impl Target {
    /// Splits `http://host/path?query`.
    ///
    /// # Errors
    ///
    /// Returns an error if the URL is not of that form.
    pub fn parse(addr: SocketAddr, url: &str) -> anyhow::Result<Self> {
        let rest = url.strip_prefix("http://").context("not an http URL")?;
        let slash = rest.find('/').context("URL has no path")?;
        Ok(Self {
            addr,
            host: rest[..slash].to_owned(),
            path: rest[slash..].to_owned(),
        })
    }
}

/// Requests exactly `len` bytes starting at `start` and times their arrival.
///
/// # Errors
///
/// Returns an error on connection failures, a non-206 response, or a timeout.
pub async fn timed_range(
    target: &Target,
    start: u64,
    len: u64,
    timeout: Duration,
) -> anyhow::Result<Timing> {
    tokio::time::timeout(timeout, async {
        let began = Instant::now();
        let mut tcp = open(target, start, Some(len)).await?;
        let first_byte = read_body(&mut tcp, began, len).await?;
        Ok(Timing {
            first_byte,
            complete: began.elapsed(),
        })
    })
    .await
    .context("timed out")?
}

/// A simulated media player that reads continuously from a position, like `<video>`.
///
/// It reads as fast as data arrives until `ready_bytes` have been received (reporting that
/// moment), then keeps reading at `pace_bps`, the way a player keeps its buffer topped up. The
/// open connection keeps the engine prioritizing data ahead of it, as a real player's does.
/// Dropping the player closes the connection, as a player does when the user seeks.
#[derive(Debug)]
pub struct Player {
    task: tokio::task::JoinHandle<()>,
    read: Arc<AtomicU64>,
}

impl Player {
    /// Starts reading from `start`. The receiver yields once `ready_bytes` have arrived.
    #[must_use]
    pub fn start(
        target: Target,
        start: u64,
        ready_bytes: u64,
        pace_bps: u64,
    ) -> (Self, oneshot::Receiver<anyhow::Result<Timing>>) {
        let (tx, rx) = oneshot::channel();
        let read = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&read);
        let task = tokio::spawn(async move {
            let began = Instant::now();
            let mut tcp = match open(&target, start, None).await {
                Ok(tcp) => tcp,
                Err(error) => {
                    let _ = tx.send(Err(error));
                    return;
                }
            };
            match read_body(&mut tcp, began, ready_bytes).await {
                Ok(first_byte) => {
                    let _ = tx.send(Ok(Timing {
                        first_byte,
                        complete: began.elapsed(),
                    }));
                    pace(&mut tcp, pace_bps.max(1), &counter).await;
                }
                Err(error) => {
                    let _ = tx.send(Err(error));
                }
            }
        });
        (Self { task, read }, rx)
    }

    /// Bytes read while playing (after the ready point).
    #[must_use]
    pub fn bytes_read(&self) -> u64 {
        self.read.load(Ordering::Relaxed)
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Opens a connection and sends a range request (`bytes=start-` if `len` is `None`).
async fn open(target: &Target, start: u64, len: Option<u64>) -> anyhow::Result<TcpStream> {
    let mut tcp = TcpStream::connect(target.addr).await?;
    tcp.set_nodelay(true)?;
    let end = len.map_or_else(String::new, |len| (start + len - 1).to_string());
    let raw = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nRange: bytes={start}-{end}\r\nConnection: close\r\n\r\n",
        target.path, target.host,
    );
    tcp.write_all(raw.as_bytes()).await?;
    Ok(tcp)
}

/// Reads the response headers (requiring `206`), then body bytes until at least `len` have
/// arrived. Returns the time from `began` to the first body byte.
async fn read_body(tcp: &mut TcpStream, began: Instant, len: u64) -> anyhow::Result<Duration> {
    let mut buf = vec![0u8; 256 * 1024];
    let mut head = Vec::new();
    let mut received: u64;
    let mut first_byte = None;
    loop {
        let n = tcp.read(&mut buf).await?;
        if n == 0 {
            bail!("connection closed before the response headers");
        }
        head.extend_from_slice(&buf[..n]);
        if let Some(end) = head.windows(4).position(|w| w == b"\r\n\r\n") {
            if !head.starts_with(b"HTTP/1.1 206") {
                bail!(
                    "unexpected response: {}",
                    String::from_utf8_lossy(&head[..end.min(64)])
                );
            }
            received = (head.len() - end - 4) as u64;
            if received > 0 {
                first_byte = Some(began.elapsed());
            }
            break;
        }
    }
    while received < len {
        let n = tcp.read(&mut buf).await?;
        if n == 0 {
            bail!("connection closed after {received} of {len} bytes");
        }
        first_byte.get_or_insert_with(|| began.elapsed());
        received += n as u64;
    }
    Ok(first_byte.unwrap_or_else(|| began.elapsed()))
}

/// Keeps reading at roughly `bps` until the connection closes or the task is aborted.
async fn pace(tcp: &mut TcpStream, bps: u64, read: &AtomicU64) {
    let mut buf = vec![0u8; 64 * 1024];
    let started = Instant::now();
    let mut total: u64 = 0;
    while let Ok(n) = tcp.read(&mut buf).await {
        if n == 0 {
            break;
        }
        total += n as u64;
        read.fetch_add(n as u64, Ordering::Relaxed);
        #[allow(clippy::cast_precision_loss)]
        let due = Duration::from_secs_f64(total as f64 / bps as f64);
        if let Some(wait) = due.checked_sub(started.elapsed()) {
            tokio::time::sleep(wait).await;
        }
    }
}
