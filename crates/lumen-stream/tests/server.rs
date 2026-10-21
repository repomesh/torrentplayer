// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end tests of the loopback server over real TCP connections.

// Test code: panicking on failure is the point.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unnecessary_literal_bound
)]

use std::io::{self, Cursor};
use std::net::SocketAddr;
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use lumen_stream::{
    MediaReader, MediaSource, RequestObserver, ServerConfig, StreamId, StreamServer,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

struct Memory(Arc<Vec<u8>>);

impl MediaSource for Memory {
    fn byte_len(&self) -> u64 {
        self.0.len() as u64
    }
    fn content_type(&self) -> &str {
        "video/mp4"
    }
    fn open(&self) -> BoxFuture<'static, io::Result<Box<dyn MediaReader>>> {
        let data = Arc::clone(&self.0);
        Box::pin(async move { Ok(Box::new(Cursor::new(data.to_vec())) as Box<dyn MediaReader>) })
    }
}

#[derive(Default)]
struct Recorder {
    requests: Mutex<Vec<(StreamId, RangeInclusive<u64>)>>,
    first_bytes: Mutex<Vec<(StreamId, u64)>>,
}

impl RequestObserver for Recorder {
    fn on_request(&self, stream: StreamId, range: RangeInclusive<u64>) {
        self.requests.lock().unwrap().push((stream, range));
    }
    fn on_first_byte(&self, stream: StreamId, offset: u64, _waited: Duration) {
        self.first_bytes.lock().unwrap().push((stream, offset));
    }
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

async fn send(addr: SocketAddr, raw: &str) -> Reply {
    let mut tcp = TcpStream::connect(addr).await.unwrap();
    tcp.write_all(raw.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), tcp.read_to_end(&mut buf))
        .await
        .expect("server did not close the connection")
        .unwrap();
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8(buf[..split].to_vec()).unwrap();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .filter_map(|l| l.split_once(": "))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    Reply {
        status,
        headers,
        body: buf[split + 4..].to_vec(),
    }
}

fn get(path_and_query: &str, host: &str, extra: &str) -> String {
    format!("GET {path_and_query} HTTP/1.1\r\nHost: {host}\r\n{extra}Connection: close\r\n\r\n")
}

struct Fixture {
    server: StreamServer,
    data: Arc<Vec<u8>>,
    path: String,
    host: String,
    recorder: Arc<Recorder>,
}

async fn fixture() -> Fixture {
    let recorder = Arc::new(Recorder::default());
    let server = StreamServer::start(ServerConfig {
        observer: Some(recorder.clone()),
        ..ServerConfig::default()
    })
    .await
    .unwrap();
    let data: Arc<Vec<u8>> = Arc::new((0..10_000u32).map(|i| (i % 251) as u8).collect());
    let published = server.publish(Arc::new(Memory(Arc::clone(&data))));
    let url = published.url.strip_prefix("http://").unwrap();
    let (host, path) = url.split_at(url.find('/').unwrap());
    Fixture {
        host: host.to_owned(),
        path: path.to_owned(),
        server,
        data,
        recorder,
    }
}

#[tokio::test]
async fn serves_full_content() {
    let f = fixture().await;
    let r = send(f.server.local_addr(), &get(&f.path, &f.host, "")).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body, *f.data);
    assert_eq!(r.header("accept-ranges"), Some("bytes"));
    assert_eq!(r.header("content-type"), Some("video/mp4"));
    assert_eq!(r.header("cache-control"), Some("no-store"));
    assert!(
        r.header("access-control-allow-origin").is_none(),
        "must not enable CORS"
    );
}

#[tokio::test]
async fn serves_byte_ranges() {
    let f = fixture().await;
    let addr = f.server.local_addr();

    let r = send(addr, &get(&f.path, &f.host, "Range: bytes=100-199\r\n")).await;
    assert_eq!(r.status, 206);
    assert_eq!(r.body, f.data[100..200]);
    assert_eq!(r.header("content-range"), Some("bytes 100-199/10000"));
    assert_eq!(r.header("content-length"), Some("100"));

    let r = send(addr, &get(&f.path, &f.host, "Range: bytes=-10\r\n")).await;
    assert_eq!(r.status, 206);
    assert_eq!(r.body, f.data[9_990..]);

    let r = send(addr, &get(&f.path, &f.host, "Range: bytes=9990-\r\n")).await;
    assert_eq!(r.body, f.data[9_990..]);

    let r = send(addr, &get(&f.path, &f.host, "Range: bytes=10000-\r\n")).await;
    assert_eq!(r.status, 416);
    assert_eq!(r.header("content-range"), Some("bytes */10000"));
    assert!(r.body.is_empty());

    let recorded = f.recorder.requests.lock().unwrap().clone();
    assert_eq!(recorded.len(), 3, "416 responses do not open the source");
    assert_eq!(recorded[0].1, 100..=199);
    assert!(f.recorder.first_bytes.lock().unwrap().contains(&(1, 100)));
}

#[tokio::test]
async fn head_returns_headers_only() {
    let f = fixture().await;
    let raw = format!(
        "HEAD {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        f.path, f.host
    );
    let r = send(f.server.local_addr(), &raw).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-length"), Some("10000"));
    assert!(r.body.is_empty());
}

#[tokio::test]
async fn rejects_missing_or_wrong_token() {
    let f = fixture().await;
    let addr = f.server.local_addr();
    let no_token = f.path.split('?').next().unwrap();
    let wrong = format!("{no_token}?t={}", "0".repeat(64));
    let truncated = &f.path[..f.path.len() - 1];
    for path in [no_token, wrong.as_str(), truncated, "/s/1?x=1"] {
        let r = send(addr, &get(path, &f.host, "")).await;
        assert_eq!(r.status, 403, "{path}");
        assert!(r.body.is_empty());
    }
}

#[tokio::test]
async fn rejects_foreign_host_headers() {
    // DNS rebinding: a website resolving its own name to 127.0.0.1 sends its own Host.
    let f = fixture().await;
    let port = f.server.local_addr().port();
    for host in [
        "evil.example".to_owned(),
        format!("localhost:{port}"),
        format!("127.0.0.1:{}", port.wrapping_add(1)),
        "127.0.0.1".to_owned(),
    ] {
        let r = send(f.server.local_addr(), &get(&f.path, &host, "")).await;
        assert_eq!(r.status, 403, "{host}");
    }
}

#[tokio::test]
async fn unknown_and_unpublished_streams_are_not_found() {
    let f = fixture().await;
    let addr = f.server.local_addr();
    let token_query = &f.path[f.path.find('?').unwrap()..];

    let r = send(addr, &get(&format!("/s/999{token_query}"), &f.host, "")).await;
    assert_eq!(r.status, 404);
    let r = send(addr, &get(&format!("/other{token_query}"), &f.host, "")).await;
    assert_eq!(r.status, 404);

    f.server.unpublish(1);
    let r = send(addr, &get(&f.path, &f.host, "")).await;
    assert_eq!(r.status, 404);
}

#[tokio::test]
async fn only_get_and_head_are_allowed() {
    let f = fixture().await;
    let raw = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        f.path, f.host
    );
    let r = send(f.server.local_addr(), &raw).await;
    assert_eq!(r.status, 405);
    assert_eq!(r.header("allow"), Some("GET, HEAD"));
}

#[tokio::test]
async fn published_url_is_redacted_in_debug_output() {
    let f = fixture().await;
    let published = f.server.publish(Arc::new(Memory(Arc::clone(&f.data))));
    let token = published.url.split("t=").nth(1).unwrap();
    assert!(!format!("{published:?}").contains(token));
    assert!(!format!("{:?}", f.server).contains(token));
}

#[tokio::test]
async fn private_tokens_open_only_their_own_stream() {
    let f = fixture().await;
    let addr = f.server.local_addr();
    let private = f
        .server
        .publish_private(Arc::new(Memory(Arc::clone(&f.data))))
        .unwrap();
    let path = &private.url[private.url.find("/s/").unwrap()..];
    let token = path.split("t=").nth(1).unwrap();
    assert!(!f.path.contains(token), "not the session token");

    let r = send(
        addr,
        &get(
            path,
            &f.host,
            "Range: bytes=0-9
",
        ),
    )
    .await;
    assert_eq!(r.status, 206);
    assert_eq!(r.body, f.data[..10]);
    // The session token opens it too.
    let session = &f.path[f.path.find('?').unwrap()..];
    let r = send(
        addr,
        &get(&format!("/s/{}{session}", private.id), &f.host, ""),
    )
    .await;
    assert_eq!(r.status, 200);

    // Its token opens neither the other stream nor an unknown one.
    for other in ["/s/1", "/s/999", "/other"] {
        let r = send(addr, &get(&format!("{other}?t={token}"), &f.host, "")).await;
        assert_eq!(r.status, 403, "{other}");
    }

    // Unpublished, its token opens nothing (and does not tell that it existed).
    f.server.unpublish(private.id);
    let r = send(addr, &get(path, &f.host, "")).await;
    assert_eq!(r.status, 403);
    assert!(!format!("{private:?}").contains(token));
}
