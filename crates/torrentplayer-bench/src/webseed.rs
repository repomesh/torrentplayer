// SPDX-License-Identifier: MIT OR Apache-2.0

//! A minimal HTTP/1.1 file server on the loopback interface, standing in for a torrent's web
//! seed (BEP 19): `GET` with single byte ranges and keep-alive, like a static file host.

use std::io::SeekFrom;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

/// A running server. Stops when dropped.
#[derive(Debug)]
pub struct WebSeedServer {
    /// Where it listens.
    pub addr: SocketAddr,
    requests: Arc<AtomicU64>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for WebSeedServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl WebSeedServer {
    /// Serves `file` at every path.
    ///
    /// # Errors
    ///
    /// Returns an error if the loopback listener cannot be bound.
    pub async fn start(file: PathBuf) -> std::io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let addr = listener.local_addr()?;
        let requests = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let file = file.clone();
                let counter = Arc::clone(&counter);
                tokio::spawn(async move {
                    let _ = serve(stream, &file, &counter).await;
                });
            }
        });
        Ok(Self {
            addr,
            requests,
            task,
        })
    }

    /// Requests served so far.
    #[must_use]
    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }
}

/// Appends a `url-list` entry to a `.torrent` file (keys stay sorted: `url-list` sorts after
/// every standard top-level key).
#[must_use]
pub fn with_web_seed(torrent: &[u8], url: &str) -> Vec<u8> {
    let mut out = torrent[..torrent.len().saturating_sub(1)].to_vec();
    out.extend_from_slice(format!("8:url-list{}:{url}e", url.len()).as_bytes());
    out
}

async fn serve(mut stream: TcpStream, path: &PathBuf, counter: &AtomicU64) -> std::io::Result<()> {
    let _ = stream.set_nodelay(true);
    let mut file = tokio::fs::File::open(path).await?;
    let len = file.metadata().await?.len();
    let mut head = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        // One request head at a time (no pipelining needed).
        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
            if head.len() > 16 * 1024 {
                return Ok(());
            }
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                return Ok(());
            }
            head.extend_from_slice(&buf[..n]);
        }
        let end = head.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or(0) + 4;
        let request = String::from_utf8_lossy(&head[..end]).into_owned();
        head.drain(..end);
        counter.fetch_add(1, Ordering::Relaxed);
        let range = request.lines().find_map(|l| {
            let (name, value) = l.split_once(':')?;
            if !name.trim().eq_ignore_ascii_case("range") {
                return None;
            }
            let (a, b) = value.trim().strip_prefix("bytes=")?.split_once('-')?;
            let a: u64 = a.parse().ok()?;
            let b: u64 = if b.is_empty() {
                len - 1
            } else {
                b.parse().ok()?
            };
            (a <= b && b < len).then_some((a, b))
        });
        let (status, first, last) = match range {
            Some((a, b)) => ("206 Partial Content", a, b),
            None => ("200 OK", 0, len.saturating_sub(1)),
        };
        let content_range = if range.is_some() {
            format!("Content-Range: bytes {first}-{last}/{len}\r\n")
        } else {
            String::new()
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\n{content_range}\r\n",
            last - first + 1
        );
        stream.write_all(response.as_bytes()).await?;
        file.seek(SeekFrom::Start(first)).await?;
        let mut left = last - first + 1;
        while left > 0 {
            let n = usize::try_from(left).unwrap_or(usize::MAX).min(buf.len());
            file.read_exact(&mut buf[..n]).await?;
            stream.write_all(&buf[..n]).await?;
            left -= n as u64;
        }
    }
}
