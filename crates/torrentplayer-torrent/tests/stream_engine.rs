// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end tests of the `StreamEngine` facade: playing a file through TorrentPlayer's own engine
//! (torrentplayer-swarm) from a librqbit seeder on the loopback interface, and the same through the
//! loopback stream server. No internet access, DHT, or trackers are involved.

// Test code: panicking on failure is the point.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::SeekFrom;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use librqbit::spawn_utils::BlockingSpawner;
use librqbit::{
    AddTorrent, AddTorrentOptions, CreateTorrentOptions, ListenerOptions, Session, SessionOptions,
    create_torrent,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use torrentplayer_stream::{ServerConfig, StreamServer};
use torrentplayer_torrent::{
    BackendKind, EngineChoice, EngineError, FileKind, SourceError, StreamEngine,
    StreamEngineConfig, TorrentSource,
};

const PIECE: u32 = 256 * 1024;
const LEN: usize = 12 * 1024 * 1024;

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

struct Fixture {
    _seeder: Arc<Session>,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
    seeder_addr: SocketAddr,
    torrent: Bytes,
    data: Vec<u8>,
    engine: StreamEngine,
}

async fn fixture(choice: EngineChoice) -> Fixture {
    let seed_dir = tempfile::tempdir().unwrap();
    let leech_dir = tempfile::tempdir().unwrap();
    let data = test_data();
    let file = seed_dir.path().join("lecture.mp4");
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

    let seeder = Session::new_with_opts(
        seed_dir.path().to_path_buf(),
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
    let handle = seeder
        .add_torrent(
            AddTorrent::from_bytes(torrent.clone()),
            Some(AddTorrentOptions {
                overwrite: true,
                output_folder: Some(seed_dir.path().to_string_lossy().into_owned()),
                ..AddTorrentOptions::default()
            }),
        )
        .await
        .unwrap()
        .into_handle()
        .unwrap();
    handle.wait_until_initialized().await.unwrap();
    let seeder_addr = seeder.listen_addr().unwrap();

    let engine = StreamEngine::start(StreamEngineConfig {
        download_dir: leech_dir.path().to_path_buf(),
        choice,
        public_discovery: false,
        download_limit_bps: None,
        upload_limit_bps: None,
        peer_limit: 40,
        dht_state_dir: None,
    })
    .await
    .unwrap();
    Fixture {
        _seeder: seeder,
        _dirs: (seed_dir, leech_dir),
        seeder_addr,
        torrent,
        data,
        engine,
    }
}

impl Fixture {
    async fn add(&self) -> torrentplayer_torrent::TorrentInfo {
        let source = TorrentSource::from_file_bytes(self.torrent.clone()).unwrap();
        self.engine
            .add_with_peers(source, vec![self.seeder_addr])
            .await
            .unwrap()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_plays_torrent_files_through_torrentplayer_swarm() {
    let f = fixture(EngineChoice::Auto).await;
    let info = f.add().await;
    assert_eq!(f.engine.backend(info.id).unwrap(), BackendKind::Swarm);
    assert_eq!(info.files.len(), 1);
    assert_eq!(info.files[0].kind, FileKind::Video);
    assert_eq!(info.files[0].len, LEN as u64);

    // Adding the same torrent again returns the same entry instead of resetting its cache.
    let again = f.add().await;
    assert_eq!(again.id, info.id);

    let playback = f
        .engine
        .start_playback(info.id, 0, torrentplayer_prioritize::Config::default())
        .await
        .unwrap();
    assert_eq!(playback.backend(), BackendKind::Swarm);

    // Read through the media source, as the stream server does for each request.
    let media = f.engine.media_source(info.id, 0).await.unwrap();
    assert_eq!(media.byte_len(), LEN as u64);
    assert_eq!(media.content_type(), "video/mp4");
    let mut reader = media.open().await.unwrap();
    for offset in [LEN / 2 + 7, 0, LEN - 2048] {
        reader.seek(SeekFrom::Start(offset as u64)).await.unwrap();
        let mut buf = vec![0u8; 2048];
        tokio::time::timeout(Duration::from_secs(20), reader.read_exact(&mut buf))
            .await
            .expect("timed out waiting for data")
            .unwrap();
        assert_eq!(buf, f.data[offset..offset + 2048], "offset {offset}");
    }

    // A reported playhead (with a duration) moves prioritization, and stats see the swarm.
    playback.update(50.0, Some(100.0));
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let have = f.engine.have_pieces(info.id).await.unwrap();
        let status = playback.status(&have);
        assert_eq!(status.position_bytes, LEN as u64 / 2);
        assert_eq!(status.byte_rate, Some(LEN as u64 / 100));
        if status.buffered_bytes > 0 {
            break;
        }
        assert!(Instant::now() < deadline, "playhead never buffered");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let stats = f.engine.stats(info.id).await.unwrap();
    assert_eq!(stats.peers_live, 1, "{stats:?}");
    assert!(stats.file_progress[0] > 0);
    assert!(stats.progress_bytes > 0);
    f.engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serves_torrentplayer_swarm_data_over_the_stream_server() {
    let f = fixture(EngineChoice::Swarm).await;
    let info = f.add().await;
    let _playback = f
        .engine
        .start_playback(info.id, 0, torrentplayer_prioritize::Config::default())
        .await
        .unwrap();
    let server = StreamServer::start(ServerConfig::default()).await.unwrap();
    let published = server.publish(f.engine.media_source(info.id, 0).await.unwrap());

    let url = published.url.strip_prefix("http://").unwrap();
    let (host, path) = url.split_at(url.find('/').unwrap());
    let start = 5 * 1024 * 1024 + 3;
    let mut tcp = tokio::net::TcpStream::connect(server.local_addr())
        .await
        .unwrap();
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nRange: bytes={start}-{}\r\nConnection: close\r\n\r\n",
        start + 8191
    );
    tcp.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), tcp.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    let split = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&response[..split]);
    assert!(head.starts_with("HTTP/1.1 206"), "{head}");
    assert_eq!(&response[split + 4..], &f.data[start..start + 8192]);
    f.engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forced_swarm_plays_magnet_links_and_refuses_unsafe_torrents() {
    let f = fixture(EngineChoice::Swarm).await;
    // librqbit fetches the metadata from the seeder; the torrent then runs in torrentplayer-swarm.
    let hash = torrentplayer_swarm::Metainfo::from_bytes(&f.torrent)
        .unwrap()
        .info_hash_hex();
    let magnet =
        TorrentSource::parse_link(&format!("magnet:?xt=urn:btih:{hash}&dn=lecture")).unwrap();
    let info = tokio::time::timeout(
        Duration::from_secs(30),
        f.engine.add_with_peers(magnet.clone(), vec![f.seeder_addr]),
    )
    .await
    .expect("timed out fetching the metadata")
    .unwrap();
    assert_eq!(info.info_hash, hash);
    assert_eq!(f.engine.backend(info.id).unwrap(), BackendKind::Swarm);
    let media = f.engine.media_source(info.id, 0).await.unwrap();
    let mut reader = media.open().await.unwrap();
    let mut buf = vec![0u8; 4096];
    tokio::time::timeout(Duration::from_secs(20), reader.read_exact(&mut buf))
        .await
        .expect("timed out waiting for data")
        .unwrap();
    assert_eq!(buf, f.data[..4096]);
    // Adding it again returns the same torrent without fetching the metadata again.
    assert_eq!(f.engine.add(magnet).await.unwrap().id, info.id);

    // A single-file torrent named `CON`, a Windows device name.
    let mut bad = b"d4:infod6:lengthi4e4:name3:CON12:piece lengthi16384e6:pieces20:".to_vec();
    bad.extend_from_slice(&[0u8; 20]);
    bad.extend_from_slice(b"ee");
    let result = f
        .engine
        .add(TorrentSource::from_file_bytes(bad).unwrap())
        .await;
    assert!(
        matches!(result, Err(EngineError::UnsafePath(_))),
        "{result:?}"
    );

    let garbage = TorrentSource::from_file_bytes(b"not bencode".to_vec()).unwrap();
    assert!(matches!(
        f.engine.add(garbage).await,
        Err(EngineError::InvalidTorrent(_))
    ));
    f.engine.shutdown().await;
}

/// A magnet link whose metadata was prefetched is added without asking peers for it again,
/// and the peers seen while prefetching are used: the add itself gets no peers here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prefetched_metadata_lets_a_magnet_link_skip_the_metadata_fetch() {
    for choice in [EngineChoice::Swarm, EngineChoice::Auto] {
        let f = fixture(choice).await;
        let hash = torrentplayer_swarm::Metainfo::from_bytes(&f.torrent)
            .unwrap()
            .info_hash_hex();
        let magnet =
            TorrentSource::parse_link(&format!("magnet:?xt=urn:btih:{hash}&dn=lecture")).unwrap();
        let prefetched = tokio::time::timeout(
            Duration::from_secs(30),
            f.engine
                .prefetch_metadata_with_peers(magnet.clone(), vec![f.seeder_addr]),
        )
        .await
        .expect("timed out prefetching the metadata")
        .unwrap();
        assert_eq!(prefetched, hash);
        // A second prefetch is answered from the cache.
        let again = tokio::time::timeout(
            Duration::from_millis(500),
            f.engine.prefetch_metadata(magnet.clone()),
        )
        .await
        .expect("cached");
        assert_eq!(again.unwrap(), hash);

        // Without the cache this would wait for metadata from peers it does not know.
        let info = tokio::time::timeout(Duration::from_secs(10), f.engine.add(magnet))
            .await
            .expect("the add asked peers for the metadata again")
            .unwrap();
        assert_eq!(info.info_hash, hash);
        assert_eq!(
            f.engine.backend(info.id).unwrap(),
            BackendKind::Swarm,
            "{choice:?}"
        );
        let _playback = f
            .engine
            .start_playback(info.id, 0, torrentplayer_prioritize::Config::default())
            .await
            .unwrap();
        let media = f.engine.media_source(info.id, 0).await.unwrap();
        let mut reader = media.open().await.unwrap();
        let mut buf = vec![0u8; 4096];
        tokio::time::timeout(Duration::from_secs(20), reader.read_exact(&mut buf))
            .await
            .expect("timed out waiting for data from the prefetched peers")
            .unwrap();
        assert_eq!(buf, f.data[..4096], "{choice:?}");
        f.engine.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prefetch_refuses_unsafe_metadata() {
    let f = fixture(EngineChoice::Swarm).await;
    let mut bad = b"d4:infod6:lengthi4e4:name3:CON12:piece lengthi16384e6:pieces20:".to_vec();
    bad.extend_from_slice(&[0u8; 20]);
    bad.extend_from_slice(b"ee");
    let result = f
        .engine
        .prefetch_metadata(TorrentSource::from_file_bytes(bad).unwrap())
        .await;
    assert!(
        matches!(result, Err(EngineError::UnsafePath(_))),
        "{result:?}"
    );
    f.engine.shutdown().await;
}

/// A local stand-in for a search site's download links: `/ok` serves the fixture's `.torrent`
/// file, `/other` another torrent, `/big` announces more than 16 MiB, `/missing` is a 404, and
/// `/redirect` redirects to the fixture's magnet link. Counts the requests for each path.
struct LinkServer {
    addr: SocketAddr,
    requests: Arc<std::sync::Mutex<Vec<String>>>,
}

impl LinkServer {
    async fn start(torrent: Bytes, other: Bytes, hash: String) -> Self {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (torrent, other, hash, seen) = (
                    torrent.clone(),
                    other.clone(),
                    hash.clone(),
                    Arc::clone(&seen),
                );
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                    }
                    let path = String::from_utf8_lossy(&head)
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_owned();
                    seen.lock().unwrap().push(path.clone());
                    let (status, extra, body): (&str, String, Bytes) = match path.as_str() {
                        "/ok" => ("200 OK", String::new(), torrent),
                        "/other" => ("200 OK", String::new(), other),
                        "/big" => (
                            "200 OK",
                            format!("Content-Length: {}\r\n", 16 * 1024 * 1024 + 1),
                            Bytes::new(),
                        ),
                        "/redirect" => (
                            "302 Found",
                            format!("Location: magnet:?xt=urn:btih:{hash}&dn=lecture\r\n"),
                            Bytes::new(),
                        ),
                        _ => ("404 Not Found", String::new(), Bytes::new()),
                    };
                    let length = if extra.starts_with("Content-Length") {
                        String::new()
                    } else {
                        format!("Content-Length: {}\r\n", body.len())
                    };
                    let reply = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/x-bittorrent\r\n\
                         {extra}{length}Connection: close\r\n\r\n"
                    );
                    let _ = socket.write_all(reply.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self { addr, requests }
    }

    fn url(&self, path: &str) -> url::Url {
        format!("http://{}{path}", self.addr).parse().unwrap()
    }

    fn count(&self, path: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|p| *p == path)
            .count()
    }
}

/// A valid single-file torrent other than the fixture's.
fn other_torrent() -> Bytes {
    let mut bytes =
        b"d4:infod6:lengthi4e4:name9:other.mp412:piece lengthi16384e6:pieces20:".to_vec();
    bytes.extend_from_slice(&[7u8; 20]);
    bytes.extend_from_slice(b"ee");
    Bytes::from(bytes)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_links_are_checked_remembered_and_fall_back_to_the_magnet_link() {
    let f = fixture(EngineChoice::Swarm).await;
    let hash = torrentplayer_swarm::Metainfo::from_bytes(&f.torrent)
        .unwrap()
        .info_hash_hex();
    let server = LinkServer::start(f.torrent.clone(), other_torrent(), hash.clone()).await;
    let engine = &f.engine;

    // The `.torrent` file is downloaded, checked, and kept: adding the magnet link then needs
    // no metadata from peers (it has none here).
    let fetched = engine
        .prefetch_download(server.url("/ok"), Some(&hash), None)
        .await
        .unwrap();
    assert_eq!(fetched, hash);
    let magnet =
        TorrentSource::parse_link(&format!("magnet:?xt=urn:btih:{hash}&dn=lecture")).unwrap();
    let info = tokio::time::timeout(Duration::from_secs(10), engine.add(magnet))
        .await
        .expect("the add asked peers for the metadata")
        .unwrap();
    assert_eq!(info.info_hash, hash);
    // Following the same link again sends no request.
    engine
        .prefetch_download(server.url("/ok"), None, None)
        .await
        .unwrap();
    assert_eq!(server.count("/ok"), 1);

    let fallback = || Some(TorrentSource::from_file_bytes(f.torrent.clone()).unwrap());
    assert!(matches!(
        engine
            .prefetch_download(server.url("/big"), None, None)
            .await,
        Err(EngineError::Source(SourceError::BadFileSize))
    ));
    // Another torrent than the result's is refused, and the magnet link used instead.
    assert!(matches!(
        engine
            .prefetch_download(server.url("/other"), Some(&hash), None)
            .await,
        Err(EngineError::InvalidTorrent(_))
    ));
    assert_eq!(
        engine
            .prefetch_download(server.url("/other"), Some(&hash), fallback())
            .await
            .unwrap(),
        hash
    );
    // A missing link falls back the same way.
    assert!(matches!(
        engine
            .prefetch_download(server.url("/missing"), None, None)
            .await,
        Err(EngineError::Source(SourceError::Download(_)))
    ));
    assert_eq!(
        engine
            .prefetch_download(server.url("/missing"), Some(&hash), fallback())
            .await
            .unwrap(),
        hash
    );
    f.engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_download_link_that_redirects_to_a_magnet_link_is_added_as_that_magnet_link() {
    for choice in [EngineChoice::Swarm, EngineChoice::Auto] {
        let f = fixture(choice).await;
        let hash = torrentplayer_swarm::Metainfo::from_bytes(&f.torrent)
            .unwrap()
            .info_hash_hex();
        let server = LinkServer::start(f.torrent.clone(), other_torrent(), hash.clone()).await;
        let link = TorrentSource::parse_link(server.url("/redirect").as_str()).unwrap();
        let info = tokio::time::timeout(
            Duration::from_secs(30),
            f.engine.add_with_peers(link.clone(), vec![f.seeder_addr]),
        )
        .await
        .expect("timed out fetching the metadata")
        .unwrap();
        assert_eq!(info.info_hash, hash, "{choice:?}");
        // Adding the link again returns the same torrent without asking the site again.
        assert_eq!(f.engine.add(link).await.unwrap().id, info.id);
        assert_eq!(server.count("/redirect"), 1, "{choice:?}");
        f.engine.shutdown().await;
    }
}

/// Seeds a folder torrent named `Lecture` with these files from librqbit on the loopback
/// interface. Returns the seeder (keep it alive), its folder, its address, and the torrent.
async fn seed_folder(
    files: &[(&str, Vec<u8>)],
) -> (Arc<Session>, tempfile::TempDir, SocketAddr, Bytes) {
    let seed_dir = tempfile::tempdir().unwrap();
    let folder = seed_dir.path().join("Lecture");
    tokio::fs::create_dir_all(&folder).await.unwrap();
    for (name, bytes) in files {
        tokio::fs::write(folder.join(name), bytes).await.unwrap();
    }
    let created = create_torrent(
        &folder,
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
    let seeder = Session::new_with_opts(
        seed_dir.path().to_path_buf(),
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
    let handle = seeder
        .add_torrent(
            AddTorrent::from_bytes(torrent.clone()),
            Some(AddTorrentOptions {
                overwrite: true,
                output_folder: Some(folder.to_string_lossy().into_owned()),
                ..AddTorrentOptions::default()
            }),
        )
        .await
        .unwrap()
        .into_handle()
        .unwrap();
    handle.wait_until_initialized().await.unwrap();
    let addr = seeder.listen_addr().unwrap();
    (seeder, seed_dir, addr, torrent)
}

/// A JPEG-looking file: the format's first bytes, then filler.
fn jpeg(len: usize) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0];
    bytes.extend((0..len - 4).map(|i| u8::try_from(i % 251).unwrap()));
    bytes
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_torrents_poster_becomes_its_thumbnail() {
    let poster = jpeg(40 * 1024);
    let (_seeder, _seed_dir, seeder_addr, torrent) = seed_folder(&[
        ("lecture.mp4", test_data()),
        ("poster.jpg", poster.clone()),
        ("www.YTS.MX.jpg", jpeg(30 * 1024)),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let engine = StreamEngine::start(StreamEngineConfig {
        download_dir: dir.path().to_path_buf(),
        choice: EngineChoice::Swarm,
        public_discovery: false,
        download_limit_bps: None,
        upload_limit_bps: None,
        peer_limit: 40,
        dht_state_dir: None,
    })
    .await
    .unwrap();
    let hash = torrentplayer_swarm::Metainfo::from_bytes(&torrent)
        .unwrap()
        .info_hash_hex();

    // Unknown metadata: nothing is fetched.
    assert!(engine.thumbnail(&hash).await.unwrap().is_none());

    let source = TorrentSource::from_file_bytes(torrent.clone()).unwrap();
    engine
        .prefetch_metadata_with_peers(source, vec![seeder_addr])
        .await
        .unwrap();
    let thumbnail = tokio::time::timeout(Duration::from_secs(20), engine.thumbnail(&hash))
        .await
        .expect("timed out downloading the poster")
        .unwrap()
        .expect("a poster");
    assert_eq!(
        thumbnail.kind,
        torrentplayer_torrent::thumbnail::ImageType::Jpeg
    );
    assert_eq!(thumbnail.bytes, poster, "the poster, not the banner");
    assert_eq!(thumbnail.info_hash, hash);
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stopped_thumbnail_download_leaves_nothing_behind() {
    // The same kind of torrent, but nobody seeds it: the download waits until it is dropped.
    let (seeder, _seed_dir, _addr, torrent) = seed_folder(&[
        ("lecture.mp4", test_data()),
        ("cover.png", {
            let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
            png.resize(20 * 1024, 7);
            png
        }),
    ])
    .await;
    drop(seeder);
    let dir = tempfile::tempdir().unwrap();
    let engine = StreamEngine::start(StreamEngineConfig {
        download_dir: dir.path().to_path_buf(),
        choice: EngineChoice::Swarm,
        public_discovery: false,
        download_limit_bps: None,
        upload_limit_bps: None,
        peer_limit: 40,
        dht_state_dir: None,
    })
    .await
    .unwrap();
    let hash = torrentplayer_swarm::Metainfo::from_bytes(&torrent)
        .unwrap()
        .info_hash_hex();
    engine
        .prefetch_metadata(TorrentSource::from_file_bytes(torrent).unwrap())
        .await
        .unwrap();
    let cache = dir.path().join("thumbnails").join(&hash);
    assert!(
        tokio::time::timeout(Duration::from_millis(500), engine.thumbnail(&hash))
            .await
            .is_err(),
        "no peers, so it is still waiting"
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!cache.exists(), "the stopped download's cache is deleted");
    engine.shutdown().await;
}
