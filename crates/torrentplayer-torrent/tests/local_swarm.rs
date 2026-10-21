// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end tests against a seeder on the loopback interface. No internet access, DHT, or
//! trackers are involved, so the results are deterministic.

// Test code: panicking on failure is the point.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unnecessary_literal_bound
)]

use std::io::SeekFrom;
use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroU32;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use librqbit::limits::LimitsConfig;
use librqbit::spawn_utils::BlockingSpawner;
use librqbit::{
    AddTorrent, AddTorrentOptions, CreateTorrentOptions, ListenerOptions, Session, SessionOptions,
    create_torrent,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use torrentplayer_stream::{ServerConfig, StreamServer};
use torrentplayer_torrent::{
    Engine, EngineConfig, EngineError, FileKind, PlaybackInput, PlayheadPosition, TorrentSource,
};

const PIECE: u32 = 256 * 1024;
const LEN: usize = 24 * 1024 * 1024;

/// Deterministic, incompressible test data so every piece hash differs.
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

struct Swarm {
    _seeder: Arc<Session>,
    seeder_addr: SocketAddr,
    torrent: Bytes,
    data: Vec<u8>,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
    engine: Engine,
}

async fn swarm() -> Swarm {
    swarm_with_upload_limit(None).await
}

/// A swarm whose seeder uploads at most `limit` bytes per second, so download order is
/// observable.
async fn swarm_with_upload_limit(limit: Option<NonZeroU32>) -> Swarm {
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
    assert!(handle.stats().finished, "seeder must have the whole file");
    let seeder_addr = seeder.listen_addr().unwrap();

    let mut config = EngineConfig::new(leech_dir.path().to_path_buf());
    config.enable_dht = false;
    config.enable_trackers = false;
    let engine = Engine::start(config).await.unwrap();

    Swarm {
        _seeder: seeder,
        seeder_addr,
        torrent,
        data,
        _dirs: (seed_dir, leech_dir),
        engine,
    }
}

async fn add(s: &Swarm) -> usize {
    let source = TorrentSource::from_file_bytes(s.torrent.clone()).unwrap();
    s.engine
        .add_with_peers(source, vec![s.seeder_addr])
        .await
        .unwrap()
        .id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adds_torrent_without_downloading_anything() {
    let s = swarm().await;
    let id = add(&s).await;
    let info = s.engine.info(id).unwrap();
    assert_eq!(info.files.len(), 1);
    assert_eq!(info.files[0].path, "lecture.mp4");
    assert_eq!(info.files[0].kind, FileKind::Video);
    assert_eq!(info.files[0].len, LEN as u64);
    assert_eq!(info.piece_len, PIECE);

    tokio::time::sleep(Duration::from_millis(500)).await;
    let stats = s.engine.stats(id).unwrap();
    assert_eq!(
        stats.progress_bytes, 0,
        "nothing is downloaded until a file is played"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streams_correct_bytes_at_any_offset() {
    let s = swarm().await;
    let id = add(&s).await;
    s.engine.select_file(id, 0).await.unwrap();

    let mut stream = s.engine.open_stream(id, 0).await.unwrap();
    for offset in [LEN / 2 + 123, 0, LEN - 1000, PIECE as usize - 1] {
        stream.seek(SeekFrom::Start(offset as u64)).await.unwrap();
        let mut buf = vec![0u8; 1000];
        tokio::time::timeout(Duration::from_secs(20), stream.read_exact(&mut buf))
            .await
            .expect("timed out waiting for pieces")
            .unwrap();
        assert_eq!(buf, s.data[offset..offset + 1000], "offset {offset}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serves_torrent_data_over_the_loopback_server() {
    let s = swarm().await;
    let id = add(&s).await;
    s.engine.select_file(id, 0).await.unwrap();
    let server = StreamServer::start(ServerConfig::default()).await.unwrap();
    let published = server.publish(Arc::new(s.engine.media_source(id, 0).unwrap()));

    let url = published.url.strip_prefix("http://").unwrap();
    let (host, path) = url.split_at(url.find('/').unwrap());
    let start = 10 * 1024 * 1024 + 7;
    let mut tcp = tokio::net::TcpStream::connect(server.local_addr())
        .await
        .unwrap();
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nRange: bytes={start}-{}\r\nConnection: close\r\n\r\n",
        start + 4095
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
    assert!(head.contains("content-type: video/mp4"), "{head}");
    assert_eq!(&response[split + 4..], &s.data[start..start + 4096]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn playback_session_prioritizes_the_playhead_after_a_seek() {
    // 4 MiB/s: the 24 MiB file takes ~6 s in total, so order is visible.
    let s = swarm_with_upload_limit(NonZeroU32::new(4 * 1024 * 1024)).await;
    let id = add(&s).await;
    let config = torrentplayer_prioritize::Config {
        // Small windows so prioritization is observable on a 24 MiB file.
        engine_lookahead_bytes: 2 * 1024 * 1024,
        min_window_bytes: 4 * 1024 * 1024,
        max_window_bytes: 4 * 1024 * 1024,
        head_bytes: 512 * 1024,
        tail_bytes: 512 * 1024,
        ..torrentplayer_prioritize::Config::default()
    };
    let session = s.engine.start_playback(id, 0, config).await.unwrap();
    let seek_to = 16 * 1024 * 1024;
    session.update(PlaybackInput {
        position: PlayheadPosition::Bytes(seek_to),
        next_file: None,
    });

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let status = session.status();
        if status.buffered_bytes >= 2 * 1024 * 1024 {
            assert_eq!(status.position_bytes, seek_to);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "playhead never buffered: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The playhead region arrived before the middle of the file, which background order
    // (first piece, last piece, then sequential) would only reach later.
    let have = s.engine.have_pieces(id).unwrap();
    let piece = |offset: usize| offset / PIECE as usize;
    let middle = &have[piece(8 * 1024 * 1024)..piece(12 * 1024 * 1024)];
    let middle_done = middle.iter().filter(|h| **h).count();
    assert!(
        middle_done * 2 < middle.len(),
        "the middle of the file should still be mostly missing ({middle_done}/{} pieces)",
        middle.len()
    );

    // The data at the playhead is correct.
    let mut stream = s.engine.open_stream(id, 0).await.unwrap();
    stream.seek(SeekFrom::Start(seek_to)).await.unwrap();
    let mut buf = vec![0u8; 64 * 1024];
    stream.read_exact(&mut buf).await.unwrap();
    let at = usize::try_from(seek_to).unwrap();
    assert_eq!(buf, s.data[at..at + buf.len()]);
}

/// A minimal single-file torrent whose name is `name`.
fn torrent_named(name: &str) -> Vec<u8> {
    let mut t = Vec::new();
    t.extend_from_slice(b"d4:infod6:lengthi4e4:name");
    t.extend_from_slice(format!("{}:{name}", name.len()).as_bytes());
    t.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
    t.extend_from_slice(&[0u8; 20]);
    t.extend_from_slice(b"ee");
    t
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refuses_torrents_with_unsafe_paths() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = EngineConfig::new(dir.path().to_path_buf());
    config.enable_dht = false;
    config.enable_trackers = false;
    let engine = Engine::start(config).await.unwrap();

    for name in ["CON", "a:b", "trailing.", "COM1.mp4"] {
        let source = TorrentSource::from_file_bytes(torrent_named(name)).unwrap();
        let result = engine.add(source).await;
        assert!(
            matches!(result, Err(EngineError::UnsafePath(_))),
            "{name}: {result:?}"
        );
    }
    // A safe name is accepted, proving the fixture itself is valid.
    let ok = engine
        .add(TorrentSource::from_file_bytes(torrent_named("ok.mp4")).unwrap())
        .await;
    assert!(ok.is_ok(), "{ok:?}");
    assert!(
        !Path::new(&dir.path().join("CON")).exists(),
        "nothing was written for refused torrents"
    );
}
