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
use librqbit::storage::{
    BoxStorageFactory, StorageFactory, StorageFactoryExt as _, TorrentStorage,
};
use librqbit::{
    AddTorrent, AddTorrentOptions, CreateTorrentOptions, ListenerOptions, ManagedTorrentShared,
    Session, SessionOptions, TorrentMetadata, create_torrent,
};
use tokio::io::AsyncWriteExt as _;

/// librqbit sessions seeding one file on `127.0.0.1`, which they open read-only.
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
        Self::seed(file, piece_len, upload_bps, count, trackers).await
    }

    /// Seeds an existing file from `count` sessions, like [`LocalSeeder::start`]. For playing
    /// real media through the app (`torrentplayer-bench seed`).
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or librqbit fails to seed it.
    pub async fn seed(
        file: PathBuf,
        piece_len: u32,
        upload_bps: Option<NonZeroU32>,
        count: usize,
        trackers: Vec<String>,
    ) -> anyhow::Result<Self> {
        let dir = file
            .parent()
            .context("the file has no parent directory")?
            .to_path_buf();
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
                dir.clone(),
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
                        storage_factory: Some(ReadOnly { path: file.clone() }.boxed()),
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

/// Storage for a single-file torrent that opens the file read-only. librqbit's own storage
/// opens it read-write and sets its length, which would touch a user's media file.
#[derive(Clone)]
struct ReadOnly {
    path: PathBuf,
}

impl StorageFactory for ReadOnly {
    type Storage = ReadOnlyFile;

    fn create(
        &self,
        _shared: &ManagedTorrentShared,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<ReadOnlyFile> {
        let [info] = metadata.file_infos.as_slice() else {
            anyhow::bail!("the seeder serves single-file torrents only");
        };
        let file = std::fs::File::open(&self.path)
            .with_context(|| format!("opening {} to seed", self.path.display()))?;
        anyhow::ensure!(
            file.metadata()?.len() == info.len,
            "{} changed size since the torrent was created",
            self.path.display()
        );
        Ok(ReadOnlyFile {
            file: Arc::new(file),
            len: info.len,
        })
    }

    fn clone_box(&self) -> BoxStorageFactory {
        self.clone().boxed()
    }
}

/// The seeded file, opened read-only.
#[derive(Clone)]
struct ReadOnlyFile {
    file: Arc<std::fs::File>,
    len: u64,
}

impl TorrentStorage for ReadOnlyFile {
    fn init(
        &mut self,
        _shared: &ManagedTorrentShared,
        _metadata: &TorrentMetadata,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        anyhow::ensure!(file_id == 0, "no file {file_id}");
        read_at(&self.file, offset, buf)?;
        Ok(())
    }

    fn pwrite_all(&self, _file_id: usize, _offset: u64, _buf: &[u8]) -> anyhow::Result<()> {
        anyhow::bail!("the seeded file is read-only")
    }

    fn remove_file(&self, _file_id: usize, _filename: &Path) -> anyhow::Result<()> {
        // Never delete the user's file.
        Ok(())
    }

    fn remove_directory_if_empty(&self, _path: &Path) -> anyhow::Result<()> {
        Ok(())
    }

    fn ensure_file_length(&self, _file_id: usize, length: u64) -> anyhow::Result<()> {
        anyhow::ensure!(length == self.len, "the seeded file has another length");
        Ok(())
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        Ok(Box::new(self.clone()))
    }
}

/// Fills `buf` from `offset` without moving a shared cursor.
#[cfg(unix)]
fn read_at(file: &std::fs::File, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

/// Fills `buf` from `offset` with positioned reads.
#[cfg(windows)]
fn read_at(file: &std::fs::File, mut offset: u64, mut buf: &mut [u8]) -> std::io::Result<()> {
    while !buf.is_empty() {
        match std::os::windows::fs::FileExt::seek_read(file, buf, offset)? {
            0 => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            n => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
        }
    }
    Ok(())
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

    #[tokio::test]
    async fn seeds_a_read_only_file_without_touching_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clip.mp4");
        generate(&path, 3 * 256 * 1024 + 5).await.unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&path, permissions.clone()).unwrap();
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let seeder = LocalSeeder::seed(path.clone(), 256 * 1024, None, 2, Vec::new())
            .await
            .unwrap();
        assert_eq!(seeder.addrs.len(), 2);
        drop(seeder);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before
        );
        // So the temporary folder can be removed on Windows; the file is the test's own.
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        std::fs::set_permissions(&path, permissions).unwrap();
    }
}
