// SPDX-License-Identifier: MIT OR Apache-2.0

//! Thin swarms on the loopback interface: reconnecting to a seeder that went away, finding a
//! seeder through peer exchange (BEP 11), and downloading from web seeds (BEP 19), including
//! servers that send wrong data or ignore ranges, and the refusal of web seeds on the local
//! network.

// Test code: panicking on failure is the point.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::SeekFrom;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use librqbit::spawn_utils::BlockingSpawner;
use librqbit::{
    AddTorrent, AddTorrentOptions, CreateTorrentOptions, ListenerOptions, Session, SessionOptions,
    create_torrent,
};
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use torrentplayer_swarm::{Metainfo, Swarm, SwarmConfig, Torrent};

const PIECE: u32 = 256 * 1024;
const LEN: usize = 8 * 1024 * 1024;

fn test_data() -> Vec<u8> {
    let mut x: u64 = 0xD1B5_4A32_D192_ED03;
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
    _session: Option<Arc<Session>>,
    _dir: tempfile::TempDir,
    addr: Option<SocketAddr>,
    torrent: Vec<u8>,
    data: Vec<u8>,
}

/// Writes the test file and its `.torrent`; with `seed`, also seeds it with librqbit.
async fn seeder(seed: bool) -> Seeder {
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
            trackers: Vec::new(),
            piece_length: Some(PIECE),
        },
        &BlockingSpawner::new(2),
    )
    .await
    .unwrap();
    let torrent = created.as_bytes().unwrap().to_vec();
    if !seed {
        return Seeder {
            _session: None,
            _dir: dir,
            addr: None,
            torrent,
            data,
        };
    }
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
    let addr = session.listen_addr().unwrap();
    Seeder {
        _session: Some(session),
        _dir: dir,
        addr: Some(addr),
        torrent,
        data,
    }
}

async fn swarm(allow_local_web_seeds: bool) -> (Swarm, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = SwarmConfig::new(dir.path().to_path_buf());
    cfg.enable_trackers = false;
    cfg.allow_local_web_seeds = allow_local_web_seeds;
    (Swarm::start(cfg).await.unwrap(), dir)
}

/// Reads `len` bytes at `offset` and checks them.
async fn read_checked(torrent: &Torrent, data: &[u8], offset: usize, len: usize, within: Duration) {
    let mut reader = torrent.reader(0).unwrap();
    reader.seek(SeekFrom::Start(offset as u64)).await.unwrap();
    let mut buf = vec![0u8; len];
    tokio::time::timeout(within, reader.read_exact(&mut buf))
        .await
        .unwrap_or_else(|_| panic!("timed out reading at {offset}: {:?}", torrent.stats()))
        .unwrap();
    assert!(buf == data[offset..offset + len], "wrong bytes at {offset}");
}

/// A TCP relay that can drop its connections and refuse new ones, like a peer going offline.
struct Flaky {
    addr: SocketAddr,
    online: Arc<AtomicBool>,
    generation: Arc<AtomicUsize>,
}

impl Flaky {
    async fn start(upstream: SocketAddr) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let online = Arc::new(AtomicBool::new(true));
        let generation = Arc::new(AtomicUsize::new(0));
        let (on, generation_seen) = (Arc::clone(&online), Arc::clone(&generation));
        tokio::spawn(async move {
            while let Ok((mut client, _)) = listener.accept().await {
                if !on.load(Ordering::SeqCst) {
                    continue;
                }
                let current = Arc::clone(&generation_seen);
                let started = current.load(Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut server = TcpStream::connect(upstream).await.unwrap();
                    let stopped = async {
                        while current.load(Ordering::SeqCst) == started {
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                    };
                    tokio::select! {
                        _ = tokio::io::copy_bidirectional(&mut client, &mut server) => {}
                        () = stopped => {}
                    }
                });
            }
        });
        Self {
            addr,
            online,
            generation,
        }
    }

    /// Closes every connection and refuses new ones until [`Flaky::up`].
    fn down(&self) {
        self.online.store(false, Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    fn up(&self) {
        self.online.store(true, Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnects_to_the_only_seeder_after_it_goes_away() {
    let s = seeder(true).await;
    let relay = Flaky::start(s.addr.unwrap()).await;
    let (swarm, _dir) = swarm(false).await;
    let meta = Metainfo::from_bytes(&s.torrent).unwrap();
    let torrent = swarm.add(meta, vec![relay.addr]).unwrap();
    read_checked(&torrent, &s.data, 0, 512 * 1024, Duration::from_secs(20)).await;

    relay.down();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(torrent.stats().peers_live, 0, "the connection was cut");
    relay.up();
    // Data the engine did not have yet arrives once it reconnects on its own.
    read_checked(
        &torrent,
        &s.data,
        LEN / 2,
        512 * 1024,
        Duration::from_secs(30),
    )
    .await;
    assert_eq!(torrent.stats().peers_live, 1);
    swarm.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retries_a_seeder_that_was_offline_at_first() {
    let s = seeder(true).await;
    let relay = Flaky::start(s.addr.unwrap()).await;
    relay.down();
    let (swarm, _dir) = swarm(false).await;
    let meta = Metainfo::from_bytes(&s.torrent).unwrap();
    let torrent = swarm.add(meta, vec![relay.addr]).unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    relay.up();
    read_checked(
        &torrent,
        &s.data,
        PIECE as usize,
        256 * 1024,
        Duration::from_secs(30),
    )
    .await;
    swarm.shutdown();
}

/// A peer that only shakes hands, advertises peer exchange, and names `others`.
async fn pex_peer(info_hash: [u8; 20], others: Vec<SocketAddr>) -> SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let others = others.clone();
            tokio::spawn(async move {
                let mut theirs = [0u8; 68];
                stream.read_exact(&mut theirs).await.unwrap();
                assert_ne!(
                    theirs[25] & 0x10,
                    0,
                    "TorrentPlayer advertises the extension protocol"
                );
                let mut ours = [0u8; 68];
                ours[0] = 19;
                ours[1..20].copy_from_slice(b"BitTorrent protocol");
                ours[25] = 0x10;
                ours[28..48].copy_from_slice(&info_hash);
                ours[48..68].copy_from_slice(b"-XX0000-pexpeer00000");
                stream.write_all(&ours).await.unwrap();
                let extended = |id: u8, payload: &[u8]| {
                    let mut m = u32::try_from(payload.len() + 2)
                        .unwrap()
                        .to_be_bytes()
                        .to_vec();
                    m.extend_from_slice(&[20, id]);
                    m.extend_from_slice(payload);
                    m
                };
                stream
                    .write_all(&extended(0, b"d1:md6:ut_pexi3eee"))
                    .await
                    .unwrap();
                let mut compact = Vec::new();
                for a in &others {
                    let SocketAddr::V4(v4) = a else { continue };
                    compact.extend_from_slice(&v4.ip().octets());
                    compact.extend_from_slice(&v4.port().to_be_bytes());
                }
                let mut pex = format!("d5:added{}:", compact.len()).into_bytes();
                pex.extend_from_slice(&compact);
                pex.extend_from_slice(b"7:added.f1:\x02e");
                // TorrentPlayer assigned ut_pex the id 1 in its handshake.
                stream.write_all(&extended(1, &pex)).await.unwrap();
                // Stay connected, saying nothing more.
                let mut sink = [0u8; 1024];
                while stream.read(&mut sink).await.is_ok_and(|n| n > 0) {}
            });
        }
    });
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finds_a_seeder_through_peer_exchange() {
    let s = seeder(true).await;
    let (swarm, _dir) = swarm(false).await;
    let meta = Metainfo::from_bytes(&s.torrent).unwrap();
    let gossip = pex_peer(meta.info_hash, vec![s.addr.unwrap()]).await;
    // Only the gossiping peer is known; the seeder is learned from it.
    let torrent = swarm.add(meta, vec![gossip]).unwrap();
    read_checked(&torrent, &s.data, 0, 1024 * 1024, Duration::from_secs(20)).await;
    assert_eq!(torrent.stats().peers_seen, 2);
    swarm.shutdown();
}

/// How the test web server misbehaves.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Server {
    Good,
    /// Flips one byte of every response.
    Corrupt,
    /// Answers every request with the whole file (`200`).
    NoRanges,
}

/// An HTTP server for `data` at any path, counting requests.
async fn web_server(data: Arc<Vec<u8>>, behaviour: Server) -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let (data, counter) = (Arc::clone(&data), Arc::clone(&counter));
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                    }
                    let end = head.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                    let request = String::from_utf8_lossy(&head[..end]).to_string();
                    head.drain(..end);
                    counter.fetch_add(1, Ordering::SeqCst);
                    let range = request.lines().find_map(|l| {
                        let v = l
                            .strip_prefix("range: ")
                            .or_else(|| l.strip_prefix("Range: "))?;
                        let (a, b) = v.strip_prefix("bytes=")?.split_once('-')?;
                        Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?))
                    });
                    let response = if let (Some((a, b)), Server::Good | Server::Corrupt) =
                        (range, behaviour)
                    {
                        let mut body = data[a..=b].to_vec();
                        if behaviour == Server::Corrupt {
                            body[0] ^= 0xff;
                        }
                        let mut r = format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {a}-{b}/{}\r\nContent-Length: {}\r\n\r\n",
                                data.len(),
                                body.len()
                            )
                            .into_bytes();
                        r.extend_from_slice(&body);
                        r
                    } else {
                        let mut r =
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", data.len())
                                .into_bytes();
                        r.extend_from_slice(&data);
                        r
                    };
                    if stream.write_all(&response).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (addr, hits)
}

/// The torrent with a `url-list` entry appended.
fn with_web_seed(torrent: &[u8], url: &str) -> Vec<u8> {
    let mut t = torrent[..torrent.len() - 1].to_vec();
    t.extend_from_slice(format!("8:url-list{}:{url}e", url.len()).as_bytes());
    t
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn downloads_from_a_web_seed_alone() {
    let s = seeder(false).await;
    let data = Arc::new(s.data.clone());
    let (server, hits) = web_server(Arc::clone(&data), Server::Good).await;
    let (swarm, _dir) = swarm(true).await;
    let url = format!("http://{server}/files/lecture.mp4");
    let meta = Metainfo::from_bytes(&with_web_seed(&s.torrent, &url)).unwrap();
    assert_eq!(meta.web_seeds.len(), 1);
    let torrent = swarm.add(meta, Vec::new()).unwrap();
    for offset in [LEN - 300_000, 0, LEN / 3 + 777] {
        read_checked(&torrent, &s.data, offset, 200_000, Duration::from_secs(20)).await;
    }
    let stats = torrent.stats();
    assert_eq!(stats.web_seeds, 1, "{stats:?}");
    assert!(stats.web_seed_bytes > 0);
    assert!(hits.load(Ordering::SeqCst) > 0);
    swarm.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn never_contacts_web_seeds_on_the_local_network() {
    let s = seeder(false).await;
    let (server, hits) = web_server(Arc::new(s.data.clone()), Server::Good).await;
    let (swarm, _dir) = swarm(false).await;
    let by_ip = format!("http://{server}/lecture.mp4");
    let by_name = format!("http://localhost:{}/lecture.mp4", server.port());
    let mut torrent = s.torrent[..s.torrent.len() - 1].to_vec();
    torrent.extend_from_slice(
        format!(
            "8:url-listl{}:{by_ip}{}:{by_name}ee",
            by_ip.len(),
            by_name.len()
        )
        .as_bytes(),
    );
    let meta = Metainfo::from_bytes(&torrent).unwrap();
    assert_eq!(meta.web_seeds.len(), 2);
    let torrent = swarm.add(meta, Vec::new()).unwrap();
    let mut reader = torrent.reader(0).unwrap();
    let mut buf = [0u8; 1024];
    let read = tokio::time::timeout(Duration::from_secs(2), reader.read_exact(&mut buf)).await;
    assert!(read.is_err(), "no data can come from a refused web seed");
    assert_eq!(torrent.stats().web_seeds, 0, "both were given up");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "the server was never contacted"
    );
    swarm.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drops_a_web_seed_with_wrong_data_and_uses_the_peers() {
    let s = seeder(true).await;
    let (server, _) = web_server(Arc::new(s.data.clone()), Server::Corrupt).await;
    let (swarm, _dir) = swarm(true).await;
    let url = format!("http://{server}/lecture.mp4");
    let meta = Metainfo::from_bytes(&with_web_seed(&s.torrent, &url)).unwrap();
    let torrent = swarm.add(meta, vec![s.addr.unwrap()]).unwrap();
    torrent.select_file(0).unwrap();
    read_checked(&torrent, &s.data, 0, LEN, Duration::from_secs(60)).await;
    // Two pieces with bad data and the web seed is dropped.
    assert_eq!(torrent.stats().web_seeds, 0, "{:?}", torrent.stats());
    swarm.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gives_up_on_a_web_seed_that_ignores_ranges() {
    let s = seeder(true).await;
    let (server, hits) = web_server(Arc::new(s.data.clone()), Server::NoRanges).await;
    let (swarm, _dir) = swarm(true).await;
    let url = format!("http://{server}/lecture.mp4");
    let meta = Metainfo::from_bytes(&with_web_seed(&s.torrent, &url)).unwrap();
    let torrent = swarm.add(meta, vec![s.addr.unwrap()]).unwrap();
    // A read in the middle: a whole-file response cannot serve it.
    read_checked(&torrent, &s.data, LEN / 2, 300_000, Duration::from_secs(20)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(torrent.stats().web_seeds, 0);
    let after = hits.load(Ordering::SeqCst);
    read_checked(&torrent, &s.data, LEN / 4, 300_000, Duration::from_secs(20)).await;
    assert_eq!(
        hits.load(Ordering::SeqCst),
        after,
        "no more requests after giving up"
    );
    swarm.shutdown();
}
