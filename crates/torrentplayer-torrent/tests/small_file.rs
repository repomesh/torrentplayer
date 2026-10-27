// SPDX-License-Identifier: MIT OR Apache-2.0

//! `StreamEngine::read_small_file`: a subtitle file next to a video, read through both engines
//! from a librqbit seeder on the loopback interface.

// Test code: panicking on failure is the point.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use librqbit::spawn_utils::BlockingSpawner;
use librqbit::{
    AddTorrent, AddTorrentOptions, CreateTorrentOptions, ListenerOptions, Session, SessionOptions,
    create_torrent,
};
use torrentplayer_torrent::{
    EngineChoice, EngineError, StreamEngine, StreamEngineConfig, TorrentSource,
};

const PIECE: u32 = 256 * 1024;

fn video() -> Vec<u8> {
    (0..6 * 1024 * 1024u32)
        .map(|i| u8::try_from(i.wrapping_mul(2_654_435_761) >> 24).unwrap())
        .collect()
}

const SUBTITLES: &[u8] = b"1\n00:00:01,000 --> 00:00:02,000\nHello\n";

/// Seeds a folder torrent with a video and its subtitles; returns the seeder (keep it alive),
/// its folder, its address, and the torrent.
async fn seed() -> (Arc<Session>, tempfile::TempDir, SocketAddr, Bytes) {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("Lecture");
    tokio::fs::create_dir_all(&folder).await.unwrap();
    tokio::fs::write(folder.join("lecture.mp4"), video())
        .await
        .unwrap();
    tokio::fs::write(folder.join("lecture.en.srt"), SUBTITLES)
        .await
        .unwrap();
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
    seeder
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
        .unwrap()
        .wait_until_initialized()
        .await
        .unwrap();
    let addr = seeder.listen_addr().unwrap();
    (seeder, dir, addr, torrent)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reads_a_subtitle_file_through_either_engine() {
    let (_seeder, _seed_dir, addr, torrent) = seed().await;
    for choice in [EngineChoice::Swarm, EngineChoice::Rqbit] {
        let dir = tempfile::tempdir().unwrap();
        let engine = StreamEngine::start(StreamEngineConfig {
            download_dir: dir.path().to_path_buf(),
            choice,
            public_discovery: false,
            download_limit_bps: None,
            upload_limit_bps: None,
            peer_limit: 40,
            dht_state_dir: None,
            allow_local_links: false,
        })
        .await
        .unwrap();
        let source = TorrentSource::from_file_bytes(torrent.clone()).unwrap();
        let info = engine.add_with_peers(source, vec![addr]).await.unwrap();
        if choice == EngineChoice::Rqbit {
            assert!(
                dir.path().join("librqbit").join(&info.info_hash).is_dir(),
                "librqbit data should be isolated by info hash"
            );
        }
        let srt = info
            .files
            .iter()
            .find(|f| {
                std::path::Path::new(&f.path)
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("srt"))
            })
            .unwrap()
            .index;

        // Larger than the limit: refused before any download.
        let too_large = engine.read_small_file(info.id, srt, 10).await;
        assert!(
            matches!(too_large, Err(EngineError::Unsupported(_))),
            "{choice:?}: {too_large:?}"
        );
        let bytes = tokio::time::timeout(
            Duration::from_secs(30),
            engine.read_small_file(info.id, srt, 1024 * 1024),
        )
        .await
        .unwrap_or_else(|_| panic!("{choice:?}: timed out reading the subtitle file"))
        .unwrap();
        assert_eq!(bytes, SUBTITLES, "{choice:?}");
        assert!(matches!(
            engine.read_small_file(info.id, 99, 1024).await,
            Err(EngineError::UnknownFile)
        ));
        engine.remove(info.id).await.unwrap();
        assert!(matches!(
            engine.info(info.id),
            Err(EngineError::UnknownTorrent)
        ));
        engine.shutdown().await;
    }
}
