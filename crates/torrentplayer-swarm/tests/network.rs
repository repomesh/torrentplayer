// SPDX-License-Identifier: MIT OR Apache-2.0

//! TorrentPlayer's engine behind a SOCKS5 proxy, on the loopback interface: every peer and tracker
//! connection goes through the proxy, the kill switch stops traffic when the proxy goes away
//! (with nothing sent directly), and downloading resumes when it comes back. A recording relay in
//! front of the seeder shows who actually connected to it.

// Test code: panicking on failure is the point.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use librqbit::limits::LimitsConfig;
use librqbit::spawn_utils::BlockingSpawner;
use librqbit::{
    AddTorrent, AddTorrentOptions, CreateTorrentOptions, ListenerOptions, Session, SessionOptions,
    create_torrent,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, UdpSocket};
use torrentplayer_net::test_util::{RecordingRelay, TestProxy};
use torrentplayer_net::{
    ConnectionState, Network, NetworkConfig, NetworkMode, NetworkOptions, Secret, Socks5Config,
};
use torrentplayer_swarm::{Metainfo, Swarm, SwarmConfig, Torrent};

const PIECE: u32 = 256 * 1024;
const LEN: usize = 16 * 1024 * 1024;
/// Example keys from the `wg(8)` manual.
const PRIVATE: &str = "yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=";
const PUBLIC: &str = "xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=";

fn test_data() -> Vec<u8> {
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
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
    torrent: Vec<u8>,
    data: Vec<u8>,
}

/// A librqbit seeder on loopback, uploading at most `limit` bytes per second.
async fn seeder(trackers: Vec<String>, limit: Option<NonZeroU32>) -> Seeder {
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
        &BlockingSpawner::new(4),
    )
    .await
    .unwrap();
    let torrent = created.as_bytes().unwrap().to_vec();
    let session = Session::new_with_opts(
        dir.path().to_path_buf(),
        SessionOptions {
            dht: None,
            disable_trackers: true,
            disable_local_service_discovery: true,
            ratelimits: LimitsConfig {
                upload_bps: limit,
                download_bps: None,
            },
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
    let addr = session.listen_addr().unwrap();
    Seeder {
        _session: session,
        _dir: dir,
        addr,
        torrent,
        data,
    }
}

fn fast_options() -> NetworkOptions {
    NetworkOptions {
        health_interval: Duration::from_millis(200),
        retry_interval: Duration::from_millis(100),
        check_timeout: Duration::from_secs(2),
        connect_timeout: Duration::from_secs(5),
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

async fn swarm(network: &Network, trackers: bool) -> (Swarm, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = SwarmConfig::new(dir.path().to_path_buf());
    cfg.enable_trackers = trackers;
    cfg.network = Some(network.clone());
    (Swarm::start(cfg).await.unwrap(), dir)
}

async fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !ok() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting until {what}"));
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

/// Every connection the seeder's relay accepted came out of the proxy.
fn assert_only_proxied(relay: &RecordingRelay, proxy: &TestProxy) {
    let from_proxy: HashSet<SocketAddr> = proxy
        .connections()
        .iter()
        .map(|c| c.outbound_local)
        .collect();
    for peer in relay.peers() {
        assert!(
            from_proxy.contains(&peer),
            "the seeder saw a connection from {peer} that did not come through the proxy \
             (proxy connections: {from_proxy:?})"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn downloads_only_through_the_proxy_and_the_kill_switch_holds() {
    // 4 MiB/s, so the 16 MiB file takes about 4 s and the proxy can be stopped halfway.
    let seed = seeder(Vec::new(), NonZeroU32::new(4 * 1024 * 1024)).await;
    let relay = RecordingRelay::start(seed.addr).await.unwrap();
    let proxy = TestProxy::start(Some(("torrentplayer-test", "hunter2")))
        .await
        .unwrap();
    let net = Network::start(through(&proxy), fast_options())
        .await
        .unwrap();
    assert_eq!(
        net.settled(Duration::from_secs(5)).await.state,
        ConnectionState::Connected
    );
    let (swarm, _dir) = swarm(&net, false).await;
    let torrent = swarm
        .add(
            Metainfo::from_bytes(&seed.torrent).unwrap(),
            vec![relay.addr()],
        )
        .unwrap();
    torrent.select_file(0).unwrap();

    // Some data arrives, all of it through the proxy.
    wait_until("1 MiB is downloaded", || {
        torrent.stats().downloaded_bytes >= 1024 * 1024
    })
    .await;
    assert!(!proxy.connections().is_empty());
    assert!(
        proxy
            .connections()
            .iter()
            .all(|c| c.target == relay.addr().to_string()),
        "{:?}",
        proxy.connections()
    );
    assert_only_proxied(&relay, &proxy);

    // The proxy goes away: the kill switch engages and traffic stops.
    proxy.stop().await;
    wait_until("the kill switch engages", || {
        matches!(net.status().state, ConnectionState::KillSwitchActive { .. })
    })
    .await;
    wait_until("the peer is disconnected", || {
        torrent.stats().peers_live == 0
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let stalled = torrent.stats().downloaded_bytes;
    let relayed_before = relay.peers().len();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let stats = torrent.stats();
    assert_eq!(
        stats.downloaded_bytes, stalled,
        "no data while blocked: {stats:?}"
    );
    assert!(
        stalled < LEN as u64,
        "the download should not be finished yet"
    );
    assert_eq!(stats.peers_live, 0);
    assert_eq!(
        relay.peers().len(),
        relayed_before,
        "nothing reached the seeder directly while the proxy was down"
    );
    assert_only_proxied(&relay, &proxy);

    // The proxy comes back: the swarm reconnects through it and finishes.
    proxy.restart().await.unwrap();
    wait_until("the network reconnects", || {
        net.status().state == ConnectionState::Connected
    })
    .await;
    let all = read_all(&torrent).await;
    assert!(
        all == seed.data,
        "downloaded bytes differ from the seeded file"
    );
    assert!(
        relay.peers().len() > relayed_before,
        "the peer was reconnected"
    );
    assert_only_proxied(&relay, &proxy);
    swarm.shutdown();
    net.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blocked_mode_makes_no_connections() {
    let seed = seeder(Vec::new(), None).await;
    let relay = RecordingRelay::start(seed.addr).await.unwrap();
    // WireGuard is selected but the built-in tunnel does not exist: everything is blocked.
    let text = format!(
        "[Interface]\nPrivateKey = {PRIVATE}\nAddress = 10.0.0.2/32\n[Peer]\nPublicKey = {PUBLIC}\n\
         AllowedIPs = 0.0.0.0/0\nEndpoint = 198.51.100.1:51820\n"
    );
    let (wg, _) = torrentplayer_net::WireGuardConfig::parse(&text).unwrap();
    let net = Network::start(
        NetworkConfig {
            mode: NetworkMode::WireGuard(wg),
            allow_ipv6: false,
            hold: None,
        },
        fast_options(),
    )
    .await
    .unwrap();
    let (swarm, _dir) = swarm(&net, false).await;
    let torrent = swarm
        .add(
            Metainfo::from_bytes(&seed.torrent).unwrap(),
            vec![relay.addr()],
        )
        .unwrap();
    torrent.select_file(0).unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(relay.peers().is_empty(), "{:?}", relay.peers());
    assert_eq!(torrent.stats().downloaded_bytes, 0);
    swarm.shutdown();
    net.shutdown();
}

/// A minimal HTTP tracker that hands out the peer in `peer` (once it is set) and counts
/// announces.
async fn http_tracker(peer: Arc<Mutex<Option<SocketAddrV4>>>) -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            let peer = *peer.lock().unwrap();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buf = [0u8; 2048];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let mut body = Vec::new();
                if let Some(peer) = peer {
                    body.extend_from_slice(b"d8:intervali60e5:peers6:");
                    body.extend_from_slice(&peer.ip().octets());
                    body.extend_from_slice(&peer.port().to_be_bytes());
                } else {
                    body.extend_from_slice(b"d8:intervali60e5:peers0:");
                }
                body.push(b'e');
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(&body).await;
            });
        }
    });
    (addr, hits)
}

async fn udp_packets_within(socket: &UdpSocket, wait: Duration) -> bool {
    let mut buf = [0u8; 2048];
    tokio::time::timeout(wait, socket.recv_from(&mut buf))
        .await
        .is_ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trackers_go_through_the_proxy_and_udp_trackers_are_skipped() {
    let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let peer = Arc::new(Mutex::new(None));
    let (tracker, announces) = http_tracker(Arc::clone(&peer)).await;
    let trackers = vec![
        format!("http://localhost:{}/announce", tracker.port()),
        format!(
            "udp://127.0.0.1:{}/announce",
            udp.local_addr().unwrap().port()
        ),
    ];
    let seed = seeder(trackers, None).await;
    let relay = RecordingRelay::start(seed.addr).await.unwrap();
    let SocketAddr::V4(relay_v4) = relay.addr() else {
        panic!("the relay listens on IPv4");
    };
    *peer.lock().unwrap() = Some(relay_v4);

    let proxy = TestProxy::start(Some(("torrentplayer-test", "hunter2")))
        .await
        .unwrap();
    let net = Network::start(through(&proxy), fast_options())
        .await
        .unwrap();
    net.settled(Duration::from_secs(5)).await;
    let (swarm, _dir) = swarm(&net, true).await;
    // No initial peers: the only way to find the seeder is the tracker.
    let torrent = swarm
        .add(Metainfo::from_bytes(&seed.torrent).unwrap(), Vec::new())
        .unwrap();
    torrent.select_file(0).unwrap();
    let all = read_all(&torrent).await;
    assert!(all == seed.data);

    assert!(announces.load(Ordering::SeqCst) >= 1);
    let targets: Vec<String> = proxy.connections().into_iter().map(|c| c.target).collect();
    assert!(
        targets.contains(&format!("localhost:{}", tracker.port())),
        "the tracker's host name is resolved by the proxy: {targets:?}"
    );
    assert!(
        !udp_packets_within(&udp, Duration::from_millis(500)).await,
        "a UDP tracker was contacted outside the proxy"
    );
    assert_only_proxied(&relay, &proxy);
    swarm.shutdown();
    net.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_mode_still_uses_udp_trackers() {
    // The control for the test above: the UDP check does detect announces.
    let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let trackers = vec![format!(
        "udp://127.0.0.1:{}/announce",
        udp.local_addr().unwrap().port()
    )];
    let seed = seeder(trackers, None).await;
    let net = Network::start(NetworkConfig::default(), fast_options())
        .await
        .unwrap();
    let (swarm, _dir) = swarm(&net, true).await;
    let _torrent = swarm
        .add(Metainfo::from_bytes(&seed.torrent).unwrap(), Vec::new())
        .unwrap();
    assert!(udp_packets_within(&udp, Duration::from_secs(5)).await);
    swarm.shutdown();
    net.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn magnet_trackers_on_the_users_own_network_are_never_contacted() {
    let (tracker, announces) = http_tracker(Arc::new(Mutex::new(None))).await;
    let port = tracker.port();
    let trackers: Vec<url::Url> = [
        format!("http://127.0.0.1:{port}/announce"),
        format!("http://localhost:{port}/announce"),
        format!("http://LocalHost.:{port}/announce"),
        format!("http://tracker.localhost:{port}/announce"),
        format!("http://[::ffff:127.0.0.1]:{port}/announce"),
        format!("http://192.168.1.1:{port}/announce"),
    ]
    .iter()
    .map(|u| url::Url::parse(u).unwrap())
    .collect();
    let proxy = TestProxy::start(Some(("torrentplayer-test", "hunter2")))
        .await
        .unwrap();
    let net = Network::start(through(&proxy), fast_options())
        .await
        .unwrap();
    net.settled(Duration::from_secs(5)).await;

    let peers = torrentplayer_swarm::http_tracker::announce_all(
        &net,
        &trackers,
        [1; 20],
        Duration::from_secs(3),
    )
    .await;
    assert!(peers.is_empty());
    assert_eq!(announces.load(Ordering::SeqCst), 0);
    let targets: Vec<String> = proxy.connections().into_iter().map(|c| c.target).collect();
    assert!(
        !targets.iter().any(|t| t.ends_with(&format!(":{port}"))),
        "sent through the proxy: {targets:?}"
    );
    net.shutdown();
}
