// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end tests of preparing the play queue's next video: the next lecture of a course
//! torrent that is already playing (its start and end at background priority, on both
//! engines), and a preload of a named file rather than the engine's own choice. A librqbit
//! seeder on the loopback interface; no internet access, DHT, or trackers.

// Test code: panicking on failure is the point.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use librqbit::spawn_utils::BlockingSpawner;
use librqbit::{
    AddTorrent, AddTorrentOptions, CreateTorrentOptions, ListenerOptions, Session, SessionOptions,
    create_torrent,
};
use lumen_swarm::Metainfo;
use lumen_torrent::preload::{TAIL_BYTES, preloaded_bytes, targets};
use lumen_torrent::{
    BackendKind, EngineChoice, FileEntry, StreamEngine, StreamEngineConfig, TorrentInfo,
    TorrentSource,
};

const PIECE: u32 = 256 * 1024;
const MIB: usize = 1024 * 1024;
/// The first lecture is short; the second has a 24 MiB head, a 2 MiB tail, and 6 MiB in
/// between that must not download.
const FIRST: usize = 2 * MIB;
const SECOND: usize = 32 * MIB;

fn test_data(len: usize, seed: u64) -> Vec<u8> {
    let mut x: u64 = 0x2545_F491_4F6C_DD1D ^ seed;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x.to_le_bytes()[0]
        })
        .collect()
}

struct Course {
    _seeder: Arc<Session>,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
    seeder_addr: SocketAddr,
    torrent: Bytes,
    hash: String,
    engine: StreamEngine,
}

/// A two-lecture course torrent, seeded on the loopback interface.
async fn course(choice: EngineChoice) -> Course {
    let seed_dir = tempfile::tempdir().unwrap();
    let leech_dir = tempfile::tempdir().unwrap();
    let dir = seed_dir.path().join("course");
    tokio::fs::create_dir_all(&dir).await.unwrap();
    tokio::fs::write(dir.join("Lecture 1.mp4"), test_data(FIRST, 1))
        .await
        .unwrap();
    tokio::fs::write(dir.join("Lecture 2.mp4"), test_data(SECOND, 2))
        .await
        .unwrap();
    let created = create_torrent(
        &dir,
        CreateTorrentOptions {
            name: None,
            trackers: Vec::new(),
            piece_length: Some(PIECE),
        },
        &BlockingSpawner::new(4),
    )
    .await
    .unwrap();
    let torrent: Bytes = created.as_bytes().unwrap();
    let hash = Metainfo::from_bytes(&torrent).unwrap().info_hash_hex();

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
                output_folder: Some(dir.to_string_lossy().into_owned()),
                ..AddTorrentOptions::default()
            }),
        )
        .await
        .unwrap()
        .into_handle()
        .unwrap();
    handle.wait_until_initialized().await.unwrap();
    assert!(handle.stats().finished, "seeder must have both lectures");
    let seeder_addr = seeder.listen_addr().unwrap();

    let engine = StreamEngine::start(StreamEngineConfig {
        download_dir: leech_dir.path().to_path_buf(),
        choice,
        public_discovery: false,
        download_limit_bps: None,
        upload_limit_bps: None,
        peer_limit: 40,
    })
    .await
    .unwrap();
    Course {
        _seeder: seeder,
        _dirs: (seed_dir, leech_dir),
        seeder_addr,
        torrent,
        hash,
        engine,
    }
}

impl Course {
    async fn add(&self) -> TorrentInfo {
        let source = TorrentSource::from_file_bytes(self.torrent.clone()).unwrap();
        self.engine
            .add_with_peers(source, vec![self.seeder_addr])
            .await
            .unwrap()
    }
}

fn lecture(info: &TorrentInfo, name: &str) -> FileEntry {
    info.files
        .iter()
        .find(|f| f.path.ends_with(name))
        .cloned()
        .unwrap()
}

/// Waits until `done` returns true, or fails after 60 s.
async fn eventually(what: &str, mut done: impl AsyncFnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn swarm_prefetches_the_next_lectures_start_and_end_while_one_plays() {
    let c = course(EngineChoice::Auto).await;
    let info = c.add().await;
    assert_eq!(c.engine.backend(info.id).unwrap(), BackendKind::Swarm);
    let (first, second) = (
        lecture(&info, "Lecture 1.mp4"),
        lecture(&info, "Lecture 2.mp4"),
    );

    // Lecture 1 plays; nothing of lecture 2 is wanted yet.
    let playback = c
        .engine
        .start_playback(info.id, first.index, lumen_prioritize::Config::default())
        .await
        .unwrap();
    playback.set_next_file(Some(second.index));
    let have = c.engine.have_pieces(info.id).await.unwrap();
    let before = c
        .engine
        .file_readiness(info.id, Some(second.index), &have)
        .unwrap();
    assert_eq!((before.file, before.len), (second.index, second.len));
    assert!(!before.startable, "nothing of lecture 2 yet");
    let chosen = c
        .engine
        .prefetch_file(info.id, Some(second.index))
        .await
        .unwrap();
    assert_eq!(chosen, second.index);

    let (head, tail) = targets(second.len, TAIL_BYTES);
    assert!(head + tail < second.len, "the test needs a middle part");
    let (geometry, span) = c.engine.file_geometry(info.id, second.index).unwrap();
    let have = async || c.engine.have_pieces(info.id).await.unwrap();
    eventually("the next lecture's head and tail", async || {
        let have = have().await;
        preloaded_bytes(&geometry, span, head, tail, |p| {
            have.get(usize::try_from(p).unwrap())
                .copied()
                .unwrap_or(false)
        }) == head + tail
    })
    .await;

    // Only the start and end: the middle of lecture 2 is not downloaded.
    let have = have().await;
    let piece = |offset: u64| {
        usize::try_from((span.offset_in_torrent + offset) / u64::from(PIECE)).unwrap()
    };
    let middle = piece(head + u64::from(PIECE))..=piece(span.len - tail - u64::from(PIECE));
    assert!(
        middle.clone().all(|p| !have[p]),
        "pieces {middle:?} of the middle were downloaded"
    );

    // The queue's readiness: lecture 2 starts at once without being complete.
    let ready = c
        .engine
        .file_readiness(info.id, Some(second.index), &have)
        .unwrap();
    assert!(ready.startable && !ready.complete());
    assert_eq!(
        c.engine.torrent_with_hash(&info.info_hash.to_uppercase()),
        Some(info.id)
    );
    assert_eq!(c.engine.torrent_with_hash(&"0".repeat(40)), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn librqbit_downloads_the_next_lecture_in_the_background() {
    let c = course(EngineChoice::Rqbit).await;
    let info = c.add().await;
    assert_eq!(c.engine.backend(info.id).unwrap(), BackendKind::Rqbit);
    let second = lecture(&info, "Lecture 2.mp4");
    // `None` picks the file a preload would: here the larger lecture.
    let chosen = c.engine.prefetch_file(info.id, None).await.unwrap();
    assert_eq!(chosen, second.index);
    eventually("the next lecture", async || {
        let stats = c.engine.stats(info.id).await.unwrap();
        stats.file_progress.get(second.index).copied().unwrap_or(0) == second.len
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preloads_the_queued_lecture_not_the_default_choice() {
    let c = course(EngineChoice::Auto).await;
    let magnet =
        TorrentSource::parse_link(&format!("magnet:?xt=urn:btih:{}&dn=course", c.hash)).unwrap();
    let first = Metainfo::from_bytes(&c.torrent)
        .unwrap()
        .files
        .iter()
        .position(|f| f.path.last().is_some_and(|n| n == "Lecture 1.mp4"))
        .unwrap();
    // The engine would choose the larger lecture 2; the queue asks for lecture 1.
    let status = tokio::time::timeout(
        Duration::from_secs(60),
        c.engine
            .preload_file_with_peers(magnet, vec![c.seeder_addr], Some(first)),
    )
    .await
    .expect("preload timed out")
    .expect("preload had nothing to do");
    assert_eq!(status.file, Some(first));
    assert_eq!(status.buffered_bytes, status.target_bytes);
    assert_eq!(
        status.target_bytes, FIRST as u64,
        "a short file is preloaded whole"
    );
}
