// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end tests: TorrentStream's engine downloading from a librqbit seeder on the loopback
//! interface. No internet access, DHT, or trackers are involved.

// Test code: panicking on failure is the point.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::SeekFrom;
use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use librqbit::limits::LimitsConfig;
use librqbit::spawn_utils::BlockingSpawner;
use librqbit::{
    AddTorrent, AddTorrentOptions, CreateTorrentOptions, ListenerOptions, Session, SessionOptions,
    create_torrent,
};
use lumen_swarm::{Metainfo, Swarm, SwarmConfig, Torrent};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::TcpListener;

const PIECE: u32 = 256 * 1024;
const LEN: usize = 24 * 1024 * 1024;

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

struct Fixture {
    _seeder: Arc<Session>,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
    data: Vec<u8>,
    swarm: Swarm,
    torrent: Torrent,
}

async fn fixture(upload_limit: Option<NonZeroU32>) -> Fixture {
    fixture_with_peer(upload_limit, None).await
}

async fn fixture_with_peer(
    upload_limit: Option<NonZeroU32>,
    extra_peer: Option<SocketAddr>,
) -> Fixture {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
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
    let torrent_bytes = created.as_bytes().unwrap();

    let seeder = Session::new_with_opts(
        seed_dir.path().to_path_buf(),
        SessionOptions {
            dht: None,
            disable_trackers: true,
            disable_local_service_discovery: true,
            ratelimits: LimitsConfig {
                upload_bps: upload_limit,
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
            AddTorrent::from_bytes(torrent_bytes.clone()),
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
    let seeder_addr: SocketAddr = seeder.listen_addr().unwrap();

    let mut cfg = SwarmConfig::new(leech_dir.path().to_path_buf());
    cfg.enable_trackers = false;
    let swarm = Swarm::start(cfg).await.unwrap();
    let meta = Metainfo::from_bytes(&torrent_bytes).unwrap();
    let mut peers = vec![seeder_addr];
    peers.extend(extra_peer);
    let torrent = swarm.add(meta, peers).unwrap();
    Fixture {
        _seeder: seeder,
        _dirs: (seed_dir, leech_dir),
        data,
        swarm,
        torrent,
    }
}

fn frame(id: u8, body: &[u8]) -> Vec<u8> {
    let len = u32::try_from(body.len() + 1).unwrap();
    [&len.to_be_bytes()[..], &[id], body].concat()
}

async fn read_frame(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
    let len = stream.read_u32().await.unwrap() as usize;
    let mut frame = vec![0_u8; len];
    stream.read_exact(&mut frame).await.unwrap();
    frame
}

/// Acts as a downloader on a connection lumen-swarm opened: waits until piece zero is verified,
/// requests all of it, and returns its bytes and how long the upload took.
async fn request_first_piece(listener: TcpListener) -> (Vec<u8>, Duration) {
    let (mut stream, _) = listener.accept().await.unwrap();
    let mut handshake = [0_u8; 68];
    stream.read_exact(&mut handshake).await.unwrap();
    handshake[48..].copy_from_slice(b"-TS0001-requester001");
    stream.write_all(&handshake).await.unwrap();

    // This peer has no pieces, but wants anything lumen-swarm can serve.
    stream
        .write_all(&frame(5, &vec![0_u8; (LEN / PIECE as usize).div_ceil(8)]))
        .await
        .unwrap();
    stream.write_all(&frame(2, &[])).await.unwrap();

    let initial = read_frame(&mut stream).await;
    assert_eq!(initial.first(), Some(&5), "the bitfield must be first");
    loop {
        let incoming = read_frame(&mut stream).await;
        if incoming.first() == Some(&4)
            && incoming.get(1..5) == Some(0_u32.to_be_bytes().as_slice())
        {
            break;
        }
    }

    let started = Instant::now();
    for begin in (0..PIECE).step_by(16 * 1024) {
        let body = [
            &0_u32.to_be_bytes()[..],
            &begin.to_be_bytes(),
            &(16_u32 * 1024).to_be_bytes(),
        ]
        .concat();
        stream.write_all(&frame(6, &body)).await.unwrap();
    }
    let mut piece = vec![0_u8; PIECE as usize];
    let mut received = 0;
    while received < piece.len() {
        let incoming = read_frame(&mut stream).await;
        if incoming.first() != Some(&7) || incoming.len() < 9 {
            continue;
        }
        let index = u32::from_be_bytes(incoming[1..5].try_into().unwrap());
        let begin = u32::from_be_bytes(incoming[5..9].try_into().unwrap()) as usize;
        let data = &incoming[9..];
        assert_eq!(index, 0);
        piece[begin..begin + data.len()].copy_from_slice(data);
        received += data.len();
    }
    (piece, started.elapsed())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parses_the_torrent_and_downloads_nothing_unasked() {
    let f = fixture(None).await;
    assert_eq!(f.torrent.meta().files.len(), 1);
    assert_eq!(f.torrent.meta().total_len, LEN as u64);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let stats = f.torrent.stats();
    assert_eq!(stats.peers_live, 1, "{stats:?}");
    assert_eq!(stats.downloaded_bytes, 0);
    f.swarm.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reads_correct_bytes_at_any_offset() {
    let f = fixture(None).await;
    let mut reader = f.torrent.reader(0).unwrap();
    for offset in [
        LEN / 2 + 123,
        0,
        LEN - 1000,
        PIECE as usize - 1,
        7 * PIECE as usize,
    ] {
        reader.seek(SeekFrom::Start(offset as u64)).await.unwrap();
        let mut buf = vec![0u8; 1000];
        tokio::time::timeout(Duration::from_secs(20), reader.read_exact(&mut buf))
            .await
            .expect("timed out waiting for data")
            .unwrap();
        assert_eq!(buf, f.data[offset..offset + 1000], "offset {offset}");
    }
    f.swarm.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stored_readers_read_only_stored_pieces_and_download_nothing() {
    // 1 MiB/s, so the middle of the file is still missing when it is read.
    let f = fixture(NonZeroU32::new(1024 * 1024)).await;
    let mut stored = f.torrent.stored_reader(0).unwrap();
    let mut buf = vec![0u8; 1000];
    let error = stored.read_exact(&mut buf).await.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        f.torrent.stats().downloaded_bytes,
        0,
        "nothing was asked for"
    );

    // Once a normal reader has fetched the start, the stored reader serves it.
    let mut reader = f.torrent.reader(0).unwrap();
    tokio::time::timeout(Duration::from_secs(20), reader.read_exact(&mut buf))
        .await
        .expect("timed out waiting for data")
        .unwrap();
    drop(reader);
    stored.seek(SeekFrom::Start(10)).await.unwrap();
    let mut start = vec![0u8; 500];
    stored.read_exact(&mut start).await.unwrap();
    assert_eq!(start, f.data[10..510]);
    stored.seek(SeekFrom::Start(LEN as u64 / 2)).await.unwrap();
    let error = stored.read_exact(&mut start).await.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    f.swarm.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn downloads_a_whole_selected_file_correctly() {
    let f = fixture(None).await;
    f.torrent.select_file(0).unwrap();
    let mut reader = f.torrent.reader(0).unwrap();
    let mut all = Vec::with_capacity(LEN);
    tokio::time::timeout(Duration::from_secs(60), reader.read_to_end(&mut all))
        .await
        .expect("timed out")
        .unwrap();
    assert_eq!(all.len(), LEN);
    assert!(
        all == f.data,
        "downloaded bytes differ from the seeded file"
    );
    assert!(f.torrent.have_pieces().iter().all(|h| *h));
    f.swarm.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uploads_verified_pieces_and_obeys_the_global_limit() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requester = tokio::spawn(request_first_piece(listener));
    let f = fixture_with_peer(None, Some(addr)).await;
    // A 64 KiB burst is immediate; the remaining 192 KiB takes about three seconds.
    f.swarm.set_upload_limit(NonZeroU32::new(64 * 1024));
    f.torrent.select_file(0).unwrap();

    let (piece, elapsed) = tokio::time::timeout(Duration::from_secs(20), requester)
        .await
        .expect("timed out waiting for the upload")
        .unwrap();
    assert_eq!(piece, f.data[..PIECE as usize]);
    assert!(
        elapsed >= Duration::from_secs(2),
        "the upload limit should be visible, finished in {elapsed:?}"
    );
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let stats = f.torrent.stats();
    assert_eq!(stats.uploaded_bytes, u64::from(PIECE));
    assert!(stats.upload_bps > 0.0, "{stats:?}");
    f.swarm.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seek_target_arrives_before_background_data() {
    // 4 MiB/s, so the 24 MiB file takes about 6 s and ordering is visible.
    let f = fixture(NonZeroU32::new(4 * 1024 * 1024)).await;
    f.torrent.select_file(0).unwrap();
    let target = 16 * 1024 * 1024;
    let mut reader = f.torrent.reader(0).unwrap();
    reader.seek(SeekFrom::Start(target as u64)).await.unwrap();
    let started = Instant::now();
    let mut buf = vec![0u8; 1024 * 1024];
    tokio::time::timeout(Duration::from_secs(20), reader.read_exact(&mut buf))
        .await
        .expect("timed out")
        .unwrap();
    let waited = started.elapsed();
    assert_eq!(buf, f.data[target..target + buf.len()]);

    let have = f.torrent.have_pieces();
    let piece = |offset: usize| offset / PIECE as usize;
    let middle = &have[piece(8 * 1024 * 1024)..piece(12 * 1024 * 1024)];
    let middle_done = middle.iter().filter(|h| **h).count();
    assert!(
        middle_done * 2 < middle.len(),
        "the middle should still be mostly missing ({middle_done}/{} pieces, waited {waited:?})",
        middle.len()
    );
    f.swarm.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pausing_stops_the_download_and_resuming_continues_it() {
    // 4 MiB/s: the 24 MiB file takes about 6 s, so a pause is visible.
    let f = fixture(NonZeroU32::new(4 * 1024 * 1024)).await;
    f.torrent.select_file(0).unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    f.torrent.set_paused(true);
    assert!(f.torrent.is_paused());
    // Requests in flight may still land; then nothing more arrives.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let paused_at = f.torrent.stats().downloaded_bytes;
    assert!(paused_at < LEN as u64, "paused before the end");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        f.torrent.stats().downloaded_bytes,
        paused_at,
        "nothing is downloaded while paused"
    );
    f.torrent.set_paused(false);
    let deadline = Instant::now() + Duration::from_secs(30);
    while !f.torrent.have_pieces().iter().all(|h| *h) {
        assert!(Instant::now() < deadline, "resumed download finished");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    f.swarm.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_torrent_limit_caps_its_rate_and_can_be_lifted() {
    let f = fixture(None).await;
    // 2 MiB/s for 2 s: about 4 MiB plus the burst and requests in flight.
    f.torrent
        .set_download_limit(NonZeroU32::new(2 * 1024 * 1024));
    f.torrent.select_file(0).unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let limited = f.torrent.stats().downloaded_bytes;
    assert!(
        limited < 8 * 1024 * 1024,
        "limited to about 4 MiB, got {limited}"
    );
    f.torrent.set_download_limit(None);
    let deadline = Instant::now() + Duration::from_secs(30);
    while !f.torrent.have_pieces().iter().all(|h| *h) {
        assert!(Instant::now() < deadline, "unlimited download finished");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    f.swarm.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deselected_file_stops_downloading() {
    let f = fixture(NonZeroU32::new(4 * 1024 * 1024)).await;
    f.torrent.select_file(0).unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    f.torrent.deselect_file(0).unwrap();
    // Requests in flight may still land; then background downloading stops.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let at = f.torrent.stats().downloaded_bytes;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(f.torrent.stats().downloaded_bytes, at);
    assert!(at < LEN as u64);
    f.swarm.shutdown();
}
