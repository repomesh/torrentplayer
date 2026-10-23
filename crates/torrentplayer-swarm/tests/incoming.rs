// SPDX-License-Identifier: MIT OR Apache-2.0

//! Incoming peer connections, on the loopback interface: a seeder that connects to TorrentPlayer's
//! listening port is served like any other peer (and the port is announced to trackers), the
//! peer limit and unknown torrents are refused, and protected modes that cannot listen safely
//! (a SOCKS5 proxy, a blocked tunnel) open no port at all.

// Test code: panicking on failure is the point.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use librqbit::spawn_utils::BlockingSpawner;
use librqbit::{CreateTorrentOptions, create_torrent};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use torrentplayer_net::test_util::TestProxy;
use torrentplayer_net::{
    IncomingConfig, IncomingStatus, Network, NetworkConfig, NetworkMode, NetworkOptions,
    Socks5Config, WireGuardConfig,
};
use torrentplayer_swarm::{Metainfo, Swarm, SwarmConfig, Torrent};

const PIECE: u32 = 64 * 1024;
const LEN: usize = 2 * 1024 * 1024;
/// Example keys from the `wg(8)` manual.
const PRIVATE: &str = "yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=";
const PUBLIC: &str = "xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=";

fn test_data() -> Vec<u8> {
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    (0..LEN)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x.to_le_bytes()[0]
        })
        .collect()
}

/// A torrent of [`test_data`], announcing to `trackers`.
async fn torrent(trackers: Vec<String>) -> (Vec<u8>, Vec<u8>, tempfile::TempDir) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let dir = tempfile::tempdir().unwrap();
    let data = test_data();
    let file = dir.path().join("lecture.mp4");
    tokio::fs::write(&file, &data).await.unwrap();
    let created = create_torrent(
        &file,
        CreateTorrentOptions {
            name: None,
            trackers,
            piece_length: Some(PIECE),
        },
        &BlockingSpawner::new(2),
    )
    .await
    .unwrap();
    (created.as_bytes().unwrap().to_vec(), data, dir)
}

fn fast_options() -> NetworkOptions {
    NetworkOptions {
        health_interval: Duration::from_millis(200),
        retry_interval: Duration::from_millis(100),
        check_timeout: Duration::from_secs(2),
        connect_timeout: Duration::from_secs(5),
        // Sockets stay on loopback, where every test server is.
        loopback_only: true,
        ..NetworkOptions::default()
    }
}

/// A port nothing listens on right now.
fn free_port() -> u16 {
    let probe = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    probe.local_addr().unwrap().port()
}

fn incoming(port: u16) -> IncomingConfig {
    IncomingConfig {
        enabled: true,
        port,
        upnp: false,
    }
}

async fn swarm(network: &Network, peer_limit: usize) -> (Swarm, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = SwarmConfig::new(dir.path().to_path_buf());
    cfg.enable_trackers = true;
    // The test tracker is on loopback.
    cfg.allow_local_trackers = true;
    cfg.network = Some(network.clone());
    cfg.accept_incoming = true;
    cfg.peer_limit = peer_limit;
    (Swarm::start(cfg).await.unwrap(), dir)
}

async fn wait_for_incoming(net: &Network, want: impl Fn(&IncomingStatus) -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !want(&net.status().incoming) {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("incoming status never matched: {:?}", net.status().incoming));
}

/// Whether anything accepts a TCP connection on `127.0.0.1:port`.
async fn accepts_on_loopback(port: u16) -> bool {
    tokio::time::timeout(
        Duration::from_secs(2),
        TcpStream::connect((Ipv4Addr::LOCALHOST, port)),
    )
    .await
    .is_ok_and(|r| r.is_ok())
}

fn handshake(info_hash: [u8; 20]) -> [u8; 68] {
    let mut h = [0u8; 68];
    h[0] = 19;
    h[1..20].copy_from_slice(b"BitTorrent protocol");
    h[28..48].copy_from_slice(&info_hash);
    h[48..68].copy_from_slice(b"-TT0001-incomingtest");
    h
}

/// Connects to `port` as a peer and sends a handshake for `info_hash`. Returns the connection
/// and the answer, or `None` if TorrentPlayer closed the connection instead of answering.
async fn connect_as_peer(port: u16, info_hash: [u8; 20]) -> Option<(TcpStream, [u8; 68])> {
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    stream.write_all(&handshake(info_hash)).await.unwrap();
    let mut theirs = [0u8; 68];
    let read = tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut theirs))
        .await
        .expect("TorrentPlayer neither answered nor closed the connection");
    read.is_ok().then_some((stream, theirs))
}

async fn send(stream: &mut TcpStream, id: u8, payload: &[u8]) {
    let len = u32::try_from(payload.len() + 1).unwrap();
    let mut message = len.to_be_bytes().to_vec();
    message.push(id);
    message.extend_from_slice(payload);
    stream.write_all(&message).await.unwrap();
}

/// Plays a seeder that connected to TorrentPlayer: announces every piece, unchokes, and answers
/// requests until the connection closes.
async fn seed(mut stream: TcpStream, data: Arc<Vec<u8>>) {
    let pieces = LEN.div_ceil(PIECE as usize);
    let mut bits = vec![0u8; pieces.div_ceil(8)];
    for piece in 0..pieces {
        bits[piece / 8] |= 0x80 >> (piece % 8);
    }
    send(&mut stream, 5, &bits).await;
    send(&mut stream, 1, &[]).await;
    loop {
        let mut len = [0u8; 4];
        if stream.read_exact(&mut len).await.is_err() {
            return;
        }
        let len = u32::from_be_bytes(len) as usize;
        if len == 0 {
            continue;
        }
        let mut body = vec![0u8; len];
        if stream.read_exact(&mut body).await.is_err() {
            return;
        }
        if body[0] == 6 {
            let field = |at: usize| u32::from_be_bytes(body[at..at + 4].try_into().unwrap());
            let (piece, begin, length) = (field(1), field(5), field(9));
            let start = piece as usize * PIECE as usize + begin as usize;
            let mut payload = body[1..9].to_vec();
            payload.extend_from_slice(&data[start..start + length as usize]);
            send(&mut stream, 7, &payload).await;
        }
    }
}

fn frame(id: u8, payload: &[u8]) -> Vec<u8> {
    let len = u32::try_from(payload.len() + 1).unwrap();
    [&len.to_be_bytes()[..], &[id], payload].concat()
}

/// Plays a malicious peer that claims every piece and answers every request with garbage. It
/// also sends a garbage block of every piece unasked. Returns once TorrentPlayer closes the
/// connection.
async fn attack(mut stream: TcpStream) {
    let pieces = LEN.div_ceil(PIECE as usize);
    let mut hello = frame(5, &vec![0xFF; pieces.div_ceil(8)]);
    hello.extend(frame(1, &[]));
    for piece in 0..u32::try_from(pieces).unwrap() {
        let mut payload = piece.to_be_bytes().to_vec();
        payload.extend_from_slice(&0u32.to_be_bytes());
        payload.extend(std::iter::repeat_n(0xEE, 16 * 1024));
        hello.extend(frame(7, &payload));
    }
    if stream.write_all(&hello).await.is_err() {
        return;
    }
    loop {
        let mut len = [0u8; 4];
        if stream.read_exact(&mut len).await.is_err() {
            return;
        }
        let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
        if stream.read_exact(&mut body).await.is_err() {
            return;
        }
        if body.first() == Some(&6) {
            let length = u32::from_be_bytes(body[9..13].try_into().unwrap()) as usize;
            let mut payload = body[1..9].to_vec();
            payload.extend(std::iter::repeat_n(0xEE, length));
            if stream.write_all(&frame(7, &payload)).await.is_err() {
                return;
            }
        }
    }
}

async fn read_all(torrent: &Torrent) -> Vec<u8> {
    let mut reader = torrent.reader(0).unwrap();
    let mut all = Vec::with_capacity(LEN);
    tokio::time::timeout(Duration::from_secs(60), reader.read_to_end(&mut all))
        .await
        .expect("timed out reading the file")
        .unwrap();
    all
}

/// An HTTP tracker that records announce request lines and hands out no peers.
async fn http_tracker() -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&requests);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buf = [0u8; 2048];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let line = String::from_utf8_lossy(&request)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                seen.lock().unwrap().push(line);
                let body = b"d8:intervali60e5:peers0:e";
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(body).await;
            });
        }
    });
    (addr, requests)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seeder_that_connects_to_us_is_served_and_the_port_is_announced() {
    let (tracker, announces) = http_tracker().await;
    let (bytes, data, _seed_dir) = torrent(vec![format!(
        "http://127.0.0.1:{}/announce",
        tracker.port()
    )])
    .await;
    let meta = Metainfo::from_bytes(&bytes).unwrap();
    let info_hash = meta.info_hash;

    let net = Network::start(NetworkConfig::default(), fast_options())
        .await
        .unwrap();
    let port = free_port();
    net.set_incoming(incoming(port));
    let (swarm, _dir) = swarm(&net, 40).await;
    wait_for_incoming(
        &net,
        |s| matches!(s, IncomingStatus::Listening { port: p, .. } if *p == port),
    )
    .await;

    // No peers to dial: the only source is a seeder that connects to the listening port.
    let torrent = swarm.add(meta, Vec::new()).unwrap();
    torrent.select_file(0).unwrap();
    let (stream, answer) = connect_as_peer(port, info_hash)
        .await
        .expect("the torrent answers the handshake");
    assert_eq!(&answer[28..48], &info_hash);
    let seeder = tokio::spawn(seed(stream, Arc::new(data.clone())));

    let all = read_all(&torrent).await;
    assert!(all == data, "downloaded bytes differ from the seeded file");
    assert_eq!(torrent.stats().peers_live, 1);

    // Trackers learn the listening port, not the placeholder.
    tokio::time::timeout(Duration::from_secs(10), async {
        while announces.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the tracker was asked");
    let first = announces.lock().unwrap()[0].clone();
    assert!(first.contains(&format!("port={port}")), "{first}");

    swarm.shutdown();
    net.shutdown();
    let _ = seeder.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_sending_bad_data_is_banned_and_refused_when_it_comes_back() {
    let (bytes, data, _seed_dir) = torrent(Vec::new()).await;
    let meta = Metainfo::from_bytes(&bytes).unwrap();
    let info_hash = meta.info_hash;
    let net = Network::start(NetworkConfig::default(), fast_options())
        .await
        .unwrap();
    let port = free_port();
    net.set_incoming(incoming(port));
    let (swarm, _dir) = swarm(&net, 40).await;
    wait_for_incoming(&net, |s| matches!(s, IncomingStatus::Listening { .. })).await;
    let torrent = swarm.add(meta, Vec::new()).unwrap();

    // A thin swarm: one honest seeder and one attacker, both connected before playback.
    let (honest, _) = connect_as_peer(port, info_hash).await.unwrap();
    let honest = tokio::spawn(seed(honest, Arc::new(data.clone())));
    let (attacker, _) = connect_as_peer(port, info_hash).await.unwrap();
    let attacker = tokio::spawn(attack(attacker));
    tokio::time::timeout(Duration::from_secs(5), async {
        while torrent.stats().peers_live < 2 {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("both peers are live");

    // Pieces the attacker spoiled are fetched again and verify, so playback gets every byte.
    let all = read_all(&torrent).await;
    assert!(all == data, "downloaded bytes differ from the seeded file");
    tokio::time::timeout(Duration::from_secs(10), attacker)
        .await
        .expect("the attacker is disconnected")
        .unwrap();
    assert_eq!(torrent.stats().peers_live, 1, "the honest seeder stays");
    // Coming back from another port does not help.
    assert!(connect_as_peer(port, info_hash).await.is_none());

    swarm.shutdown();
    net.shutdown();
    let _ = honest.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_peer_limit_and_unknown_torrents_are_refused() {
    let (bytes, _data, _seed_dir) = torrent(Vec::new()).await;
    let meta = Metainfo::from_bytes(&bytes).unwrap();
    let info_hash = meta.info_hash;
    let net = Network::start(NetworkConfig::default(), fast_options())
        .await
        .unwrap();
    let port = free_port();
    net.set_incoming(incoming(port));
    let (swarm, _dir) = swarm(&net, 1).await;
    let torrent = swarm.add(meta, Vec::new()).unwrap();
    wait_for_incoming(&net, |s| matches!(s, IncomingStatus::Listening { .. })).await;

    // A torrent the engine does not have: closed without an answer.
    assert!(connect_as_peer(port, [0xAB; 20]).await.is_none());

    // The first peer fills the limit of one; the second is refused.
    let (first, _) = connect_as_peer(port, info_hash)
        .await
        .expect("room for one peer");
    tokio::time::timeout(Duration::from_secs(5), async {
        while torrent.stats().peers_live < 1 {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the first peer is live");
    assert!(
        connect_as_peer(port, info_hash).await.is_none(),
        "the peer limit is enforced for incoming peers"
    );
    assert_eq!(torrent.stats().peers_live, 1);

    // A paused torrent accepts nobody.
    drop(first);
    tokio::time::timeout(Duration::from_secs(5), async {
        while torrent.stats().peers_live > 0 {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the first peer is gone");
    torrent.set_paused(true);
    assert!(connect_as_peer(port, info_hash).await.is_none());
    swarm.shutdown();
    net.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn protected_modes_that_cannot_listen_safely_open_no_port() {
    let (bytes, _data, _seed_dir) = torrent(Vec::new()).await;
    let meta = Metainfo::from_bytes(&bytes).unwrap();

    // SOCKS5: a proxy cannot accept incoming connections, so nothing listens anywhere.
    let proxy = TestProxy::start(None).await.unwrap();
    let net = Network::start(
        NetworkConfig {
            mode: NetworkMode::Socks5(Socks5Config {
                host: proxy.addr().ip().to_string(),
                port: proxy.addr().port(),
                username: None,
                password: None,
                remote_dns: true,
                proxy_trackers: true,
                proxy_dht: true,
            }),
            allow_ipv6: false,
            hold: None,
        },
        fast_options(),
    )
    .await
    .unwrap();
    net.settled(Duration::from_secs(5)).await;
    let port = free_port();
    net.set_incoming(incoming(port));
    let (swarm, _dir) = swarm(&net, 40).await;
    let _torrent = swarm.add(meta.clone(), Vec::new()).unwrap();
    wait_for_incoming(&net, |s| matches!(s, IncomingStatus::Unsupported { .. })).await;
    assert!(!accepts_on_loopback(port).await);
    assert_eq!(net.incoming_port(), None);

    // WireGuard whose server never answers: the kill switch holds and nothing listens either.
    let text = format!(
        "[Interface]\nPrivateKey = {PRIVATE}\nAddress = 10.0.0.2/32\n[Peer]\nPublicKey = {PUBLIC}\n\
         AllowedIPs = 0.0.0.0/0\nEndpoint = 198.51.100.1:51820\n"
    );
    let (wg, _) = WireGuardConfig::parse(&text).unwrap();
    net.apply(NetworkConfig {
        mode: NetworkMode::WireGuard(wg),
        allow_ipv6: false,
        hold: None,
    })
    .unwrap();
    wait_for_incoming(&net, |s| matches!(s, IncomingStatus::Paused { .. })).await;
    assert!(!accepts_on_loopback(port).await);

    // Direct mode is the control: the same engine then listens.
    net.apply(NetworkConfig::default()).unwrap();
    wait_for_incoming(&net, |s| matches!(s, IncomingStatus::Listening { .. })).await;
    assert!(connect_as_peer(port, meta.info_hash).await.is_some());
    swarm.shutdown();
    net.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trackers_on_the_users_own_network_are_contacted_only_when_allowed() {
    let (tracker, announces) = http_tracker().await;
    let port = tracker.port();
    let (bytes, _data, _seed_dir) = torrent(vec![
        format!("http://127.0.0.1:{port}/announce"),
        format!("http://localhost:{port}/announce"),
    ])
    .await;
    for allow in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = SwarmConfig::new(dir.path().to_path_buf());
        cfg.enable_trackers = true;
        cfg.allow_local_trackers = allow;
        let swarm = Swarm::start(cfg).await.unwrap();
        let _torrent = swarm.add(Metainfo::from_bytes(&bytes).unwrap(), Vec::new());
        tokio::time::sleep(Duration::from_secs(3)).await;
        let seen = announces.lock().unwrap().len();
        // The control: allowed, the same tracker is announced to.
        assert_eq!(
            seen > 0,
            allow,
            "{seen} announces with allow_local_trackers {allow}"
        );
        swarm.shutdown();
    }
}
