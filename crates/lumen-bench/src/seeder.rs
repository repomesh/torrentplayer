// SPDX-License-Identifier: MIT OR Apache-2.0

//! A seeder on the loopback interface, for deterministic benchmarks.

use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use bytes::Bytes;
use librqbit::limits::LimitsConfig;
use librqbit::spawn_utils::BlockingSpawner;
use librqbit::{
    AddTorrent, AddTorrentOptions, CreateTorrentOptions, ListenerOptions, Session, SessionOptions,
    create_torrent,
};
use tokio::io::AsyncWriteExt as _;

/// librqbit sessions seeding one generated file on `127.0.0.1`.
pub struct LocalSeeder {
    _sessions: Vec<Arc<Session>>,
    /// Addresses leechers should connect to, one per seeding session.
    pub addrs: Vec<SocketAddr>,
    /// The `.torrent` file for the seeded data.
    pub torrent: Bytes,
    /// The seeded file.
    pub file: PathBuf,
}

impl std::fmt::Debug for LocalSeeder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalSeeder")
            .field("addrs", &self.addrs)
            .finish_non_exhaustive()
    }
}

impl LocalSeeder {
    /// Writes `len` bytes of incompressible data to `dir/lecture.mp4` and seeds it from
    /// `count` independent sessions (distinct peers). The `.torrent` lists `trackers`; the
    /// seeders themselves never announce.
    ///
    /// `upload_bps` throttles each seeder; benchmarks use the link emulator instead.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be written or librqbit fails to seed it.
    pub async fn start(
        dir: &Path,
        len: usize,
        piece_len: u32,
        upload_bps: Option<NonZeroU32>,
        count: usize,
        trackers: Vec<String>,
    ) -> anyhow::Result<Self> {
        let file = dir.join("lecture.mp4");
        generate(&file, len).await.context("writing seed data")?;
        let created = create_torrent(
            &file,
            CreateTorrentOptions {
                name: None,
                trackers,
                piece_length: Some(piece_len),
            },
            &BlockingSpawner::new(4),
        )
        .await?;
        let torrent = created.as_bytes()?;

        let mut sessions = Vec::new();
        let mut addrs = Vec::new();
        for _ in 0..count.max(1) {
            let session = Session::new_with_opts(
                dir.to_path_buf(),
                SessionOptions {
                    dht: None,
                    disable_trackers: true,
                    disable_local_service_discovery: true,
                    ratelimits: LimitsConfig {
                        upload_bps,
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
            .await?;
            let handle = session
                .add_torrent(
                    AddTorrent::from_bytes(torrent.clone()),
                    Some(AddTorrentOptions {
                        overwrite: true,
                        output_folder: Some(dir.to_string_lossy().into_owned()),
                        ..AddTorrentOptions::default()
                    }),
                )
                .await?
                .into_handle()
                .context("seeder torrent was not added")?;
            handle.wait_until_initialized().await?;
            anyhow::ensure!(
                handle.stats().finished,
                "seeder does not have the whole file"
            );
            addrs.push(session.listen_addr().context("seeder is not listening")?);
            sessions.push(session);
        }
        Ok(Self {
            _sessions: sessions,
            addrs,
            torrent,
            file,
        })
    }
}

/// Deterministic bytes with a bounded working buffer, including multi-gigabyte fixtures.
async fn generate(path: &Path, len: usize) -> std::io::Result<()> {
    let mut file = tokio::fs::File::create(path).await?;
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    let mut block = vec![0; 1024 * 1024];
    let mut remaining = len;
    while remaining > 0 {
        let n = remaining.min(block.len());
        for word in block[..n].chunks_mut(8) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            word.copy_from_slice(&x.to_le_bytes()[..word.len()]);
        }
        file.write_all(&block[..n]).await?;
        remaining -= n;
    }
    file.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fixtures_keep_the_same_bytes_across_buffer_boundaries_and_partial_words() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture");
        generate(&path, 1024 * 1024 + 7).await.unwrap();
        let bytes = tokio::fs::read(path).await.unwrap();
        assert_eq!(bytes.len(), 1024 * 1024 + 7);
        assert_eq!(
            &bytes[..16],
            &[
                231, 227, 168, 234, 11, 40, 108, 127, 224, 171, 249, 28, 135, 25, 113, 228
            ]
        );
        assert_eq!(&bytes[1024 * 1024..], &[25, 159, 39, 181, 192, 176, 189]);
    }
}
