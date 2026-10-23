// SPDX-License-Identifier: MIT OR Apache-2.0

//! The `StreamEngine` behind a SOCKS5 proxy, on the loopback interface: magnet links through
//! librqbit reach peers and HTTP trackers only through the proxy, UDP trackers are never
//! contacted, and switching between direct and protected modes replaces the librqbit session.

// Test code: panicking on failure is the point.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use librqbit::spawn_utils::BlockingSpawner;
use librqbit::{
    AddTorrent, AddTorrentOptions, CreateTorrentOptions, ListenerOptions, Session, SessionOptions,
    create_torrent,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, UdpSocket};
use torrentplayer_net::test_util::{RecordingRelay, TestProxy};
use torrentplayer_net::{
    IncomingConfig, IncomingStatus, Network, NetworkConfig, NetworkMode, NetworkOptions, Secret,
    Socks5Config,
};
use torrentplayer_torrent::{
    BackendKind, EngineChoice, EngineError, StreamEngine, StreamEngineConfig, TorrentSource,
};

const PIECE: u32 = 256 * 1024;
const LEN: usize = 6 * 1024 * 1024;

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

struct Seeder {
    _session: Arc<Session>,
    _dir: tempfile::TempDir,
    addr: SocketAddr,
    info_hash: String,
    data: Vec<u8>,
}

async fn seeder() -> Seeder {
    let dir = tempfile::tempdir().unwrap();
    let data = test_data();
    let file = dir.path().join("lecture.mp4");
    tokio::fs::write(&file, &data).await.unwrap();
    let created = create_torrent(
        &file,
        CreateTorrentOptions {
            name: None,
            trackers: Vec::new(),
            piece_length: Some(PIECE),
        },
        &BlockingSpawner::new(4),
    )
    .await
    .unwrap();
    let torrent = created.as_bytes().unwrap();
    let session = Session::new_with_opts(
        dir.path().to_path_buf(),
        SessionOptions {
            dht: None,
            disable_trackers: true,
            disable_local_service_discovery: true,
            listen: Some(ListenerOptions {
                listen_addr: (Ipv4Addr::LOCALHOST, 0).into(),
                enable_upnp_port_forwarding: false,
                ..ListenerOptions::default()
            }),
            ..SessionOptions::default()
        },
    )
    .await
    .unwrap();
    let handle = session
        .add_torrent(
            AddTorrent::from_bytes(torrent.clone()),
            Some(AddTorrentOptions {
                overwrite: true,
                output_folder: Some(dir.path().to_string_lossy().into_owned()),
                ..AddTorrentOptions::default()
            }),
        )
        .await
        .unwrap()
        .into_handle()
        .unwrap();
    handle.wait_until_initialized().await.unwrap();
    let info_hash = torrentplayer_swarm::Metainfo::from_bytes(&torrent)
        .unwrap()
        .info_hash_hex();
    Seeder {
        addr: session.listen_addr().unwrap(),
        _session: session,
        _dir: dir,
        info_hash,
        data,
    }
}

fn options() -> NetworkOptions {
    NetworkOptions {
        health_interval: Duration::from_millis(200),
        retry_interval: Duration::from_millis(100),
        ..NetworkOptions::default()
    }
}

fn through(proxy: &TestProxy) -> NetworkConfig {
    NetworkConfig {
        mode: NetworkMode::Socks5(Socks5Config {
            host: proxy.addr().ip().to_string(),
            port: proxy.addr().port(),
            username: Some("torrentplayer-test".into()),
            password: Some(Secret::new("hunter2")),
            remote_dns: true,
            proxy_trackers: true,
            proxy_dht: true,
        }),
        allow_ipv6: false,
        hold: None,
    }
}

/// An HTTP tracker that knows no peers, asks to be announced to again after `interval`
/// seconds, and counts announces.
async fn empty_tracker(interval: u32) -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buf = [0u8; 2048];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let body = format!("d8:intervali{interval}e5:peers0:e");
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(body.as_bytes()).await;
            });
        }
    });
    (addr, hits)
}

async fn read_file(engine: &StreamEngine, id: usize) -> Vec<u8> {
    let _playback = engine
        .start_playback(id, 0, torrentplayer_prioritize::Config::default())
        .await
        .unwrap();
    let media = engine.media_source(id, 0).await.unwrap();
    let mut reader = media.open().await.unwrap();
    let mut all = Vec::with_capacity(LEN);
    tokio::time::timeout(Duration::from_secs(60), reader.read_to_end(&mut all))
        .await
        .expect("timed out reading the file")
        .unwrap();
    all
}

fn assert_only_proxied(relay: &RecordingRelay, proxy: &TestProxy) {
    let from_proxy: HashSet<SocketAddr> = proxy
        .connections()
        .iter()
        .map(|c| c.outbound_local)
        .collect();
    assert!(!relay.peers().is_empty());
    for peer in relay.peers() {
        assert!(
            from_proxy.contains(&peer),
            "the seeder saw {peer}, which did not come through the proxy ({from_proxy:?})"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn magnets_through_librqbit_use_only_the_proxy() {
    let seed = seeder().await;
    let relay = RecordingRelay::start(seed.addr).await.unwrap();
    let proxy = TestProxy::start(Some(("torrentplayer-test", "hunter2")))
        .await
        .unwrap();
    let (tracker, announces) = empty_tracker(60).await;
    let udp_tracker = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let net = Network::start(through(&proxy), options()).await.unwrap();
    net.settled(Duration::from_secs(5)).await;
    let dir = tempfile::tempdir().unwrap();
    let engine = StreamEngine::start_with_network(
        StreamEngineConfig {
            download_dir: dir.path().to_path_buf(),
            choice: EngineChoice::Rqbit,
            // Trackers on, to check which ones are used. The DHT is off in protected modes.
            public_discovery: true,
            download_limit_bps: None,
            upload_limit_bps: None,
            peer_limit: 40,
            dht_state_dir: None,
        },
        Some(net.clone()),
    )
    .await
    .unwrap();

    let magnet = format!(
        "magnet:?xt=urn:btih:{}&dn=lecture&tr=udp%3A%2F%2F127.0.0.1%3A{}%2Fannounce\
         &tr=http%3A%2F%2F{tracker}%2Fannounce",
        seed.info_hash,
        udp_tracker.local_addr().unwrap().port()
    );
    let info = engine
        .add_with_peers(
            TorrentSource::parse_link(&magnet).unwrap(),
            vec![relay.addr()],
        )
        .await
        .unwrap();
    assert_eq!(engine.backend(info.id).unwrap(), BackendKind::Rqbit);
    let all = read_file(&engine, info.id).await;
    assert!(
        all == seed.data,
        "downloaded bytes differ from the seeded file"
    );

    assert_only_proxied(&relay, &proxy);
    let targets: HashSet<String> = proxy.connections().into_iter().map(|c| c.target).collect();
    assert!(targets.contains(&relay.addr().to_string()), "{targets:?}");
    assert!(
        announces.load(Ordering::SeqCst) >= 1,
        "the HTTP tracker was used"
    );
    assert!(
        targets.contains(&tracker.to_string()),
        "the HTTP tracker was reached through the proxy: {targets:?}"
    );
    let mut buf = [0u8; 1024];
    let udp = tokio::time::timeout(Duration::from_millis(500), udp_tracker.recv_from(&mut buf));
    assert!(
        udp.await.is_err(),
        "a UDP tracker was contacted outside the proxy"
    );
    engine.shutdown().await;
    net.shutdown();
}

/// librqbit could reach HTTP trackers only through a `socks5://` proxy URL, whose client
/// resolves host names with the system resolver, and it kept announcing whatever the
/// "proxy trackers" setting later said. In a protected mode TorrentPlayer announces to them
/// itself, through the proxy with the host name unresolved, and librqbit never does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn protected_modes_resolve_tracker_names_through_the_proxy_and_librqbit_never_announces() {
    let seed = seeder().await;
    let relay = RecordingRelay::start(seed.addr).await.unwrap();
    let proxy = TestProxy::start(Some(("torrentplayer-test", "hunter2")))
        .await
        .unwrap();
    // Asks to be announced to every second: librqbit would, if it had the tracker.
    let (tracker, announces) = empty_tracker(1).await;
    let net = Network::start(through(&proxy), options()).await.unwrap();
    net.settled(Duration::from_secs(5)).await;
    let dir = tempfile::tempdir().unwrap();
    let engine = StreamEngine::start_with_network(
        StreamEngineConfig {
            download_dir: dir.path().to_path_buf(),
            choice: EngineChoice::Rqbit,
            public_discovery: true,
            download_limit_bps: None,
            upload_limit_bps: None,
            peer_limit: 40,
            dht_state_dir: None,
        },
        Some(net.clone()),
    )
    .await
    .unwrap();
    // By name: the proxy sees the name only if nothing resolved it here first.
    let named = format!("localhost:{}", tracker.port());
    let magnet = format!(
        "magnet:?xt=urn:btih:{}&dn=lecture&tr=http%3A%2F%2F{named}%2Fannounce",
        seed.info_hash
    );
    engine
        .add_with_peers(
            TorrentSource::parse_link(&magnet).unwrap(),
            vec![relay.addr()],
        )
        .await
        .unwrap();
    let targets: HashSet<String> = proxy.connections().into_iter().map(|c| c.target).collect();
    assert!(
        targets.contains(&named),
        "the tracker's name went to the proxy unresolved: {targets:?}"
    );
    assert!(!targets.contains(&tracker.to_string()), "{targets:?}");
    assert_eq!(announces.load(Ordering::SeqCst), 1, "one announce, ours");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        announces.load(Ordering::SeqCst),
        1,
        "librqbit does not announce on its own"
    );

    // Turning "proxy trackers" off changes nothing for the running torrent: it has no
    // trackers to stop.
    let mut off = through(&proxy);
    if let NetworkMode::Socks5(socks) = &mut off.mode {
        socks.proxy_trackers = false;
    }
    engine.apply_network(off).await.unwrap();
    net.settled(Duration::from_secs(5)).await;
    assert!(!net.policy().trackers_allowed);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(announces.load(Ordering::SeqCst), 1);
    engine.shutdown().await;
    net.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn magnets_without_usable_trackers_are_refused_in_protected_modes() {
    let proxy = TestProxy::start(Some(("torrentplayer-test", "hunter2")))
        .await
        .unwrap();
    let net = Network::start(through(&proxy), options()).await.unwrap();
    net.settled(Duration::from_secs(5)).await;
    let dir = tempfile::tempdir().unwrap();
    let engine = StreamEngine::start_with_network(
        StreamEngineConfig {
            download_dir: dir.path().to_path_buf(),
            choice: EngineChoice::Auto,
            public_discovery: true,
            download_limit_bps: None,
            upload_limit_bps: None,
            peer_limit: 40,
            dht_state_dir: None,
        },
        Some(net.clone()),
    )
    .await
    .unwrap();
    let magnet = "magnet:?xt=urn:btih:dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c\
                  &tr=udp%3A%2F%2Ftracker.example%3A1337%2Fannounce";
    let result = engine.add(TorrentSource::parse_link(magnet).unwrap()).await;
    assert!(
        matches!(result, Err(EngineError::Unsupported(_))),
        "{result:?}"
    );
    assert!(proxy.connections().is_empty());
    engine.shutdown().await;
    net.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turning_protection_on_replaces_the_librqbit_session() {
    let seed = seeder().await;
    let relay = RecordingRelay::start(seed.addr).await.unwrap();
    let proxy = TestProxy::start(Some(("torrentplayer-test", "hunter2")))
        .await
        .unwrap();
    let net = Network::start(NetworkConfig::default(), options())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let engine = StreamEngine::start_with_network(
        StreamEngineConfig {
            download_dir: dir.path().to_path_buf(),
            choice: EngineChoice::Rqbit,
            // No DHT or trackers: nothing may leave the machine in direct mode either.
            public_discovery: false,
            download_limit_bps: None,
            upload_limit_bps: None,
            peer_limit: 40,
            dht_state_dir: None,
        },
        Some(net.clone()),
    )
    .await
    .unwrap();
    let magnet = format!("magnet:?xt=urn:btih:{}&dn=lecture", seed.info_hash);

    // Direct: librqbit connects to the peer itself.
    let direct = engine
        .add_with_peers(
            TorrentSource::parse_link(&magnet).unwrap(),
            vec![relay.addr()],
        )
        .await
        .unwrap();
    assert!(proxy.connections().is_empty());
    let direct_peers = relay.peers().len();
    assert!(direct_peers >= 1);

    // Protected: the old session and its torrents are gone, and the new one uses the proxy.
    engine.apply_network(through(&proxy)).await.unwrap();
    net.settled(Duration::from_secs(5)).await;
    assert!(matches!(
        engine.info(direct.id),
        Err(EngineError::UnknownTorrent)
    ));
    let proxied = engine
        .add_with_peers(
            TorrentSource::parse_link(&magnet).unwrap(),
            vec![relay.addr()],
        )
        .await
        .unwrap();
    assert_ne!(proxied.id, direct.id);
    let all = read_file(&engine, proxied.id).await;
    assert!(all == seed.data);
    let from_proxy: HashSet<SocketAddr> = proxy
        .connections()
        .iter()
        .map(|c| c.outbound_local)
        .collect();
    let later = &relay.peers()[direct_peers..];
    assert!(!later.is_empty());
    assert!(
        later.iter().all(|p| from_proxy.contains(p)),
        "after the switch every connection came through the proxy: {later:?} vs {from_proxy:?}"
    );
    engine.shutdown().await;
    net.shutdown();
}

/// A peer that answers the BitTorrent handshake and then sends only keep-alives, so fetching
/// metadata from it never ends. Counts the connections made to it and those closed since.
async fn stalling_peer() -> (SocketAddr, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let opened = Arc::new(AtomicUsize::new(0));
    let closed = Arc::new(AtomicUsize::new(0));
    let (o, c) = (Arc::clone(&opened), Arc::clone(&closed));
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            o.fetch_add(1, Ordering::SeqCst);
            let closed = Arc::clone(&c);
            tokio::spawn(async move {
                let mut handshake = [0u8; 68];
                if stream.read_exact(&mut handshake).await.is_ok() {
                    // Same protocol and info hash, the extension protocol on, our own peer id.
                    handshake[25] |= 0x10;
                    handshake[48..].copy_from_slice(b"-ST0001-stalling0000");
                    let mut buf = [0u8; 4096];
                    if stream.write_all(&handshake).await.is_ok() {
                        loop {
                            tokio::select! {
                                read = stream.read(&mut buf) => {
                                    if matches!(read, Ok(0) | Err(_)) {
                                        break;
                                    }
                                }
                                () = tokio::time::sleep(Duration::from_millis(500)) => {
                                    if stream.write_all(&[0, 0, 0, 0]).await.is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
                closed.fetch_add(1, Ordering::SeqCst);
            });
        }
    });
    (addr, opened, closed)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turning_protection_on_ends_a_metadata_fetch_in_flight() {
    // Through librqbit for the whole torrent, and through librqbit for metadata only.
    for choice in [EngineChoice::Rqbit, EngineChoice::Auto] {
        let (peer, opened, closed) = stalling_peer().await;
        let proxy = TestProxy::start(Some(("torrentplayer-test", "hunter2")))
            .await
            .unwrap();
        let net = Network::start(NetworkConfig::default(), options())
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let engine = Arc::new(
            StreamEngine::start_with_network(
                StreamEngineConfig {
                    download_dir: dir.path().to_path_buf(),
                    choice,
                    public_discovery: false,
                    download_limit_bps: None,
                    upload_limit_bps: None,
                    peer_limit: 40,
                    dht_state_dir: None,
                },
                Some(net.clone()),
            )
            .await
            .unwrap(),
        );
        let magnet = "magnet:?xt=urn:btih:dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c&dn=lecture";
        let adding = tokio::spawn({
            let engine = Arc::clone(&engine);
            async move {
                engine
                    .add_with_peers(TorrentSource::parse_link(magnet).unwrap(), vec![peer])
                    .await
            }
        });
        // librqbit is asking the peer for the metadata, directly.
        tokio::time::timeout(Duration::from_secs(10), async {
            while opened.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("librqbit never connected to the peer");
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !adding.is_finished(),
            "{choice:?}: the peer never sends the metadata"
        );

        // The switch ends the fetch before the mode changes, and closes its connection.
        let started = std::time::Instant::now();
        tokio::time::timeout(
            Duration::from_secs(10),
            engine.apply_network(through(&proxy)),
        )
        .await
        .expect("the switch waited for the metadata fetch")
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(5), "{choice:?}");
        let result = tokio::time::timeout(Duration::from_secs(1), adding)
            .await
            .expect("the add was still running after the switch")
            .unwrap();
        assert!(
            matches!(result, Err(EngineError::Unsupported(_))),
            "{choice:?}: {result:?}"
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while closed.load(Ordering::SeqCst) < opened.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the connection to the peer stayed open");

        // Nothing tries again, directly or otherwise.
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(opened.load(Ordering::SeqCst), 1, "{choice:?}");
        assert!(proxy.connections().is_empty(), "{choice:?}");
        engine.shutdown().await;
        net.shutdown();
    }
}

/// Whether anything accepts a TCP connection on `127.0.0.1:port`.
async fn accepts_on_loopback(port: u16) -> bool {
    tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)),
    )
    .await
    .is_ok_and(|r| r.is_ok())
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn librqbit_listens_for_incoming_peers_only_in_direct_mode() {
    let seed = seeder().await;
    let proxy = TestProxy::start(Some(("torrentplayer-test", "hunter2")))
        .await
        .unwrap();
    let net = Network::start(NetworkConfig::default(), options())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let engine = StreamEngine::start_with_network(
        StreamEngineConfig {
            download_dir: dir.path().to_path_buf(),
            choice: EngineChoice::Rqbit,
            public_discovery: false,
            download_limit_bps: None,
            upload_limit_bps: None,
            peer_limit: 40,
            dht_state_dir: None,
        },
        Some(net.clone()),
    )
    .await
    .unwrap();
    let port = std::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    engine
        .set_incoming(IncomingConfig {
            enabled: true,
            port,
            upnp: false,
        })
        .await;
    // librqbit opens its port with its session, for the first torrent.
    wait_for_incoming(&net, |s| matches!(s, IncomingStatus::Idle { .. })).await;
    assert!(!accepts_on_loopback(port).await);
    let magnet = format!("magnet:?xt=urn:btih:{}&dn=lecture", seed.info_hash);
    engine
        .add_with_peers(TorrentSource::parse_link(&magnet).unwrap(), vec![seed.addr])
        .await
        .unwrap();
    wait_for_incoming(
        &net,
        |s| matches!(s, IncomingStatus::Listening { port: p, .. } if *p == port),
    )
    .await;
    assert!(accepts_on_loopback(port).await);

    // Protected: the session with its listener is replaced, and SOCKS5 never listens.
    engine.apply_network(through(&proxy)).await.unwrap();
    wait_for_incoming(&net, |s| matches!(s, IncomingStatus::Unsupported { .. })).await;
    assert!(!accepts_on_loopback(port).await);
    engine.shutdown().await;
    net.shutdown();
}
