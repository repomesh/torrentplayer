// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end tests of preloading: a magnet link's metadata is fetched from a librqbit seeder on
//! the loopback interface, the start and end of its video are downloaded into torrentplayer-swarm, and
//! adding the same magnet link afterwards reuses that data. No internet access, DHT, or
//! trackers are involved.

// Test code: panicking on failure is the point.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use librqbit::spawn_utils::BlockingSpawner;
use librqbit::{
    AddTorrent, AddTorrentOptions, CreateTorrentOptions, ListenerOptions, ManagedTorrent, Session,
    SessionOptions, create_torrent,
};
use tokio::io::AsyncReadExt;
use torrentplayer_swarm::Metainfo;
use torrentplayer_torrent::preload::{TAIL_BYTES, targets};
use torrentplayer_torrent::{
    BackendKind, EngineChoice, PreloadPhase, StreamEngine, StreamEngineConfig, TorrentSource,
};

const PIECE: u32 = 256 * 1024;
const MIB: usize = 1024 * 1024;

fn test_data(len: usize, seed: u64) -> Vec<u8> {
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15 ^ seed;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x.to_le_bytes()[0]
        })
        .collect()
}

struct Seeded {
    hash: String,
    data: Vec<u8>,
    handle: Arc<ManagedTorrent>,
}

struct Fixture {
    _seeder: Arc<Session>,
    _seed_dir: tempfile::TempDir,
    leech_dir: tempfile::TempDir,
    seeder_addr: SocketAddr,
    lecture: Seeded,
    other: Seeded,
    engine: Arc<StreamEngine>,
}

async fn seed(session: &Arc<Session>, dir: &std::path::Path, name: &str, data: Vec<u8>) -> Seeded {
    let sub = dir.join(name.replace('.', "_"));
    tokio::fs::create_dir_all(&sub).await.unwrap();
    let file = sub.join(name);
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
    let bytes: Bytes = created.as_bytes().unwrap();
    let hash = Metainfo::from_bytes(&bytes).unwrap().info_hash_hex();
    let handle = session
        .add_torrent(
            AddTorrent::from_bytes(bytes),
            Some(AddTorrentOptions {
                overwrite: true,
                output_folder: Some(sub.to_string_lossy().into_owned()),
                ..AddTorrentOptions::default()
            }),
        )
        .await
        .unwrap()
        .into_handle()
        .unwrap();
    handle.wait_until_initialized().await.unwrap();
    assert!(handle.stats().finished, "seeder must have the whole file");
    Seeded { hash, data, handle }
}

async fn fixture() -> Fixture {
    let seed_dir = tempfile::tempdir().unwrap();
    let leech_dir = tempfile::tempdir().unwrap();
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
    // 40 MiB: a 24 MiB head, a 2 MiB tail, and 14 MiB in between that must not download.
    let lecture = seed(
        &seeder,
        seed_dir.path(),
        "lecture.mp4",
        test_data(40 * MIB, 1),
    )
    .await;
    let other = seed(&seeder, seed_dir.path(), "other.mp4", test_data(4 * MIB, 2)).await;
    let seeder_addr = seeder.listen_addr().unwrap();
    let engine = StreamEngine::start(StreamEngineConfig {
        download_dir: leech_dir.path().to_path_buf(),
        choice: EngineChoice::Auto,
        public_discovery: false,
        download_limit_bps: None,
        upload_limit_bps: None,
        peer_limit: 40,
        dht_state_dir: None,
        allow_local_links: false,
    })
    .await
    .unwrap();
    Fixture {
        _seeder: seeder,
        _seed_dir: seed_dir,
        leech_dir,
        seeder_addr,
        lecture,
        other,
        engine: Arc::new(engine),
    }
}

fn magnet(hash: &str) -> TorrentSource {
    TorrentSource::parse_link(&format!("magnet:?xt=urn:btih:{hash}&dn=lecture")).unwrap()
}

impl Fixture {
    fn cache_dir(&self, hash: &str) -> std::path::PathBuf {
        self.leech_dir.path().join("torrentplayer-swarm").join(hash)
    }

    fn marker(&self, hash: &str) -> std::path::PathBuf {
        self.leech_dir
            .path()
            .join("torrentplayer-swarm")
            .join(format!("{hash}.preload"))
    }

    async fn preload(&self, hash: &str) -> torrentplayer_torrent::PreloadStatus {
        tokio::time::timeout(
            Duration::from_secs(60),
            self.engine
                .preload_with_peers(magnet(hash), vec![self.seeder_addr]),
        )
        .await
        .expect("preload timed out")
        .expect("preload had nothing to do")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preloads_head_and_tail_then_hands_the_torrent_to_add_and_play() {
    let f = fixture().await;
    let len = f.lecture.data.len();
    let (head, tail) = targets(len as u64, TAIL_BYTES);
    assert_eq!((head, tail), (24 * MIB as u64, 2 * MIB as u64));

    let status = f.preload(&f.lecture.hash).await;
    assert_eq!(status.phase, PreloadPhase::Ready, "{status:?}");
    assert_eq!(status.info_hash, f.lecture.hash);
    assert_eq!(status.file, Some(0));
    assert_eq!(status.target_bytes, head + tail);
    assert_eq!(status.buffered_bytes, head + tail);
    assert!(!status.claimed);
    assert_eq!(f.engine.preload_status(), Some(status));
    assert!(
        f.marker(&f.lecture.hash).exists(),
        "unplayed cache is marked"
    );

    // Preloading the same torrent again is a no-op.
    let again = f.preload(&f.lecture.hash).await;
    assert_eq!(again.phase, PreloadPhase::Ready);

    // Adding the magnet link returns the preloaded torrentplayer-swarm torrent, not a librqbit one.
    let info = f.engine.add(magnet(&f.lecture.hash)).await.unwrap();
    assert_eq!(f.engine.backend(info.id).unwrap(), BackendKind::Swarm);
    assert_eq!(info.info_hash, f.lecture.hash);
    assert!(f.engine.preload_status().unwrap().claimed);
    assert!(
        !f.marker(&f.lecture.hash).exists(),
        "claimed caches are kept"
    );

    // Only the head and tail were downloaded, and only by torrentplayer-swarm.
    let have = f.engine.have_pieces(info.id).await.unwrap();
    let pieces = |bytes: u64| usize::try_from(bytes / u64::from(PIECE)).unwrap();
    assert!(have[..pieces(head)].iter().all(|&h| h), "head");
    assert!(have[have.len() - pieces(tail)..].iter().all(|&h| h), "tail");
    let middle = &have[pieces(head)..have.len() - pieces(tail)];
    assert!(middle.iter().all(|&h| !h), "nothing past the head");
    let uploaded = f.lecture.handle.stats().uploaded_bytes;
    assert!(
        uploaded < (head + tail) + 4 * MIB as u64,
        "the seeder sent the preload only once: {uploaded}"
    );

    // Playing it serves the start from the cache at once.
    let _playback = f
        .engine
        .start_playback(info.id, 0, torrentplayer_prioritize::Config::default())
        .await
        .unwrap();
    let media = f.engine.media_source(info.id, 0).await.unwrap();
    let mut reader = media.open().await.unwrap();
    let mut start = vec![0u8; 4 * MIB];
    tokio::time::timeout(Duration::from_millis(500), reader.read_exact(&mut start))
        .await
        .expect("preloaded data should be immediate")
        .unwrap();
    assert_eq!(start, f.lecture.data[..4 * MIB]);
    f.engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_preload_evicts_the_unplayed_one() {
    let f = fixture().await;
    let first = f.preload(&f.lecture.hash).await;
    assert_eq!(first.phase, PreloadPhase::Ready);
    assert!(f.cache_dir(&f.lecture.hash).exists());

    let second = f.preload(&f.other.hash).await;
    assert_eq!(second.phase, PreloadPhase::Ready, "{second:?}");
    assert_eq!(
        second.buffered_bytes,
        f.other.data.len() as u64,
        "small files whole"
    );
    assert_eq!(f.engine.preload_status().unwrap().info_hash, f.other.hash);
    assert!(
        !f.cache_dir(&f.lecture.hash).exists(),
        "evicted cache deleted"
    );
    assert!(!f.marker(&f.lecture.hash).exists());

    // The evicted torrent is gone: adding it again resolves its metadata and starts a new swarm.
    let info = f
        .engine
        .add_with_peers(magnet(&f.lecture.hash), vec![f.seeder_addr])
        .await
        .unwrap();
    assert_eq!(f.engine.backend(info.id).unwrap(), BackendKind::Swarm);

    // Shutting down evicts the unplayed preload too.
    f.engine.shutdown().await;
    assert!(f.engine.preload_status().is_none());
    assert!(!f.marker(&f.other.hash).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_stops_a_preload_that_is_still_resolving() {
    let f = fixture().await;
    // A "peer" that accepts connections and never answers, so the metadata never arrives.
    let silent = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let silent_addr = silent.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((tcp, _)) = silent.accept().await {
            held.push(tcp);
        }
    });
    let unknown = "ab".repeat(20);
    let engine = Arc::clone(&f.engine);
    let task = tokio::spawn({
        let unknown = unknown.clone();
        async move {
            engine
                .preload_with_peers(magnet(&unknown), vec![silent_addr])
                .await
        }
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while f.engine.preload_status().is_none() {
        assert!(!task.is_finished(), "{:?}", task.await);
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Give librqbit time to connect to the silent peer.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!task.is_finished(), "still resolving");
    let status = f.engine.preload_status().unwrap();
    assert_eq!(status.phase, PreloadPhase::Resolving);
    assert_eq!(status.info_hash, unknown);

    f.engine.cancel_preload().await;
    let last = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("cancel stops it promptly")
        .unwrap()
        .unwrap();
    assert_eq!(last.phase, PreloadPhase::Cancelled);
    assert!(f.engine.preload_status().is_none());
    f.engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_preload_caches_are_removed_at_start() {
    let dir = tempfile::tempdir().unwrap();
    let swarm_dir = dir.path().join("torrentplayer-swarm");
    let stale = "cd".repeat(20);
    let kept = "ef".repeat(20);
    for hash in [&stale, &kept] {
        std::fs::create_dir_all(swarm_dir.join(hash)).unwrap();
        std::fs::write(swarm_dir.join(hash).join("pieces.dat"), b"x").unwrap();
    }
    std::fs::write(swarm_dir.join(format!("{stale}.preload")), b"").unwrap();
    let engine = StreamEngine::start(StreamEngineConfig {
        download_dir: dir.path().to_path_buf(),
        choice: EngineChoice::Auto,
        public_discovery: false,
        download_limit_bps: None,
        upload_limit_bps: None,
        peer_limit: 40,
        dht_state_dir: None,
        allow_local_links: false,
    })
    .await
    .unwrap();
    assert!(!swarm_dir.join(&stale).exists());
    assert!(!swarm_dir.join(format!("{stale}.preload")).exists());
    assert!(
        swarm_dir.join(&kept).exists(),
        "caches without a marker stay"
    );
    engine.shutdown().await;
}
