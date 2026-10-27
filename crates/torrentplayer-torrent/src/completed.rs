// SPDX-License-Identifier: MIT OR Apache-2.0

//! Reconstructs fully downloaded media without interrupting playback or replacing user files.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tempfile::NamedTempFile;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use torrentplayer_swarm::Torrent;

use crate::paths::validate_components;
use crate::{FileEntry, TorrentId};

const RETRY_AFTER: Duration = Duration::from_secs(60);
const COPY_BUFFER: usize = 256 * 1024;

/// Whether the normal video or audio file is available on disk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileSaveStatus {
    /// Full path, present only after the complete file has been published.
    pub path: Option<PathBuf>,
    /// Its verified pieces are being combined into the original file.
    pub saving: bool,
    /// The last save failed; it will be retried automatically.
    pub error: Option<String>,
}

#[derive(Debug)]
enum Outcome {
    Saving,
    Saved(PathBuf),
    Failed { at: Instant, error: String },
}

#[derive(Debug)]
struct Job {
    cancel: CancellationToken,
    task: JoinHandle<()>,
    outcome: Arc<Mutex<Outcome>>,
}

#[derive(Debug, Default)]
struct Writers {
    jobs: HashMap<(TorrentId, usize), Job>,
    /// A delayed snapshot must not restart a writer after cache cleanup retired its torrent.
    removed: HashSet<TorrentId>,
    stopped: bool,
}

/// One disk copy at a time for the whole engine; completed files are queued without blocking
/// the download planner, its rate measurements, or the player.
#[derive(Debug)]
pub(crate) struct CompletedFiles {
    writers: Mutex<Writers>,
    slot: Arc<Semaphore>,
}

impl Default for CompletedFiles {
    fn default() -> Self {
        Self {
            writers: Mutex::new(Writers::default()),
            slot: Arc::new(Semaphore::new(1)),
        }
    }
}

impl CompletedFiles {
    pub(crate) fn schedule(&self, id: TorrentId, torrent: &Torrent, root: &Path, file: &FileEntry) {
        let mut writers = self.writers.lock().unwrap_or_else(PoisonError::into_inner);
        if writers.stopped || writers.removed.contains(&id) {
            return;
        }
        let jobs = &mut writers.jobs;
        let key = (id, file.index);
        if let Some(job) = jobs.get(&key) {
            let outcome = job.outcome.lock().unwrap_or_else(PoisonError::into_inner);
            if !matches!(&*outcome, Outcome::Failed { at, .. } if at.elapsed() >= RETRY_AFTER) {
                return;
            }
        }
        let torrent = torrent.clone();
        let root = root.to_path_buf();
        let file = file.clone();
        let slot = self.slot.clone();
        let cancel = CancellationToken::new();
        let stopped = cancel.clone();
        let outcome = Arc::new(Mutex::new(Outcome::Saving));
        let result = outcome.clone();
        let task = tokio::spawn(async move {
            let permit = tokio::select! {
                () = stopped.cancelled() => return,
                permit = slot.acquire() => permit,
            };
            let Ok(_permit) = permit else { return };
            let saved = match torrent.stored_reader(file.index) {
                Ok(reader) => save_file(&root, &file.path, file.len, reader, &stopped).await,
                Err(error) => Err(io::Error::other(error)),
            };
            let next = match saved {
                Ok(path) => {
                    tracing::info!(
                        torrent = id,
                        file = file.index,
                        "completed media file saved"
                    );
                    Outcome::Saved(path)
                }
                Err(error) => {
                    if !stopped.is_cancelled() {
                        tracing::warn!(torrent = id, file = file.index, %error, "could not save completed media file");
                    }
                    Outcome::Failed {
                        at: Instant::now(),
                        error: if error.kind() == io::ErrorKind::AlreadyExists {
                            "A different file already exists at the saved location. Move or rename it; saving will retry automatically."
                        } else {
                            "Could not save the completed file. Check free space and folder permissions; saving will retry automatically."
                        }.to_owned(),
                    }
                }
            };
            *result.lock().unwrap_or_else(PoisonError::into_inner) = next;
        });
        jobs.insert(
            key,
            Job {
                cancel,
                task,
                outcome,
            },
        );
    }

    pub(crate) fn status(&self, id: TorrentId, file: usize) -> FileSaveStatus {
        let writers = self.writers.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(job) = writers.jobs.get(&(id, file)) else {
            return FileSaveStatus::default();
        };
        match &*job.outcome.lock().unwrap_or_else(PoisonError::into_inner) {
            Outcome::Saving => FileSaveStatus {
                saving: true,
                ..FileSaveStatus::default()
            },
            Outcome::Saved(path) => FileSaveStatus {
                path: Some(path.clone()),
                ..FileSaveStatus::default()
            },
            Outcome::Failed { error, .. } => FileSaveStatus {
                error: Some(error.clone()),
                ..FileSaveStatus::default()
            },
        }
    }

    /// Waits for writers to release this torrent before cache cleanup can remove its directory.
    pub(crate) async fn remove(&self, id: Option<TorrentId>) {
        let jobs = {
            let mut writers = self.writers.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(id) = id {
                writers.removed.insert(id);
            } else {
                writers.stopped = true;
            }
            let jobs = &mut writers.jobs;
            let keys: Vec<_> = jobs
                .keys()
                .filter(|(torrent, _)| id.is_none_or(|id| *torrent == id))
                .copied()
                .collect();
            keys.into_iter()
                .filter_map(|key| jobs.remove(&key))
                .collect::<Vec<_>>()
        };
        for job in &jobs {
            job.cancel.cancel();
        }
        for job in jobs {
            if let Err(error) = job.task.await {
                tracing::warn!(%error, "completed file writer stopped unexpectedly");
            }
        }
    }
}

impl Drop for CompletedFiles {
    fn drop(&mut self) {
        for job in self
            .writers
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .jobs
            .values()
        {
            job.cancel.cancel();
        }
    }
}

enum Destination {
    Existing(File),
    New(NamedTempFile),
}

/// Validate before joining, and reject links in the output tree. The separate `files` directory
/// keeps original names away from the piece store and its housekeeping markers.
fn prepare(root: &Path, relative: &str, len: u64) -> io::Result<(PathBuf, Destination)> {
    let components: Vec<_> = relative.split('/').collect();
    validate_components(&components)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    if !fs::symlink_metadata(root)?.file_type().is_dir() {
        return Err(io::Error::other(
            "the torrent download directory is not a regular directory",
        ));
    }
    let mut parent = root.to_path_buf();
    for component in
        std::iter::once("files").chain(components[..components.len() - 1].iter().copied())
    {
        parent.push(component);
        match fs::create_dir(&parent) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        if !fs::symlink_metadata(&parent)?.file_type().is_dir() {
            return Err(io::Error::other(
                "the saved file's directory is not a regular directory",
            ));
        }
    }
    let path = parent.join(components[components.len() - 1]);
    let destination = match fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_file() && meta.len() == len => {
            Destination::Existing(File::open(&path)?)
        }
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a different file already exists at the saved location",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Destination::New(
            tempfile::Builder::new()
                .prefix(".torrentplayer-")
                .suffix(".part")
                .tempfile_in(parent)?,
        ),
        Err(error) => return Err(error),
    };
    Ok((path, destination))
}

async fn save_file(
    root: &Path,
    relative: &str,
    len: u64,
    reader: impl AsyncRead + Unpin,
    cancel: &CancellationToken,
) -> io::Result<PathBuf> {
    let directory = root.to_path_buf();
    let relative = relative.to_owned();
    let (path, destination) =
        tokio::task::spawn_blocking(move || prepare(&directory, &relative, len))
            .await
            .map_err(io::Error::other)??;
    let mut reader = BufReader::with_capacity(COPY_BUFFER, reader.take(len));
    match destination {
        Destination::Existing(file) => {
            let mut existing = tokio::fs::File::from_std(file);
            let mut expected = vec![0; COPY_BUFFER];
            let mut actual = vec![0; COPY_BUFFER];
            let mut read = 0;
            loop {
                let count = tokio::select! {
                    () = cancel.cancelled() => return Err(cancelled()),
                    count = reader.read(&mut expected) => count?,
                };
                if count == 0 {
                    break;
                }
                existing.read_exact(&mut actual[..count]).await?;
                if expected[..count] != actual[..count] {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "a different file already exists at the saved location",
                    ));
                }
                read += count as u64;
            }
            check_length(read, len)?;
        }
        Destination::New(temporary) => {
            let mut output = tokio::fs::File::from_std(temporary.reopen()?);
            let count = tokio::select! {
                () = cancel.cancelled() => return Err(cancelled()),
                count = tokio::io::copy_buf(&mut reader, &mut output) => count?,
            };
            check_length(count, len)?;
            output.flush().await?;
            output.sync_all().await?;
            drop(output);
            if cancel.is_cancelled() {
                return Err(cancelled());
            }
            let saved = path.clone();
            tokio::task::spawn_blocking(move || {
                temporary
                    .persist_noclobber(saved)
                    .map_err(|error| error.error)
            })
            .await
            .map_err(io::Error::other)??;
        }
    }
    Ok(path)
}

fn check_length(actual: u64, expected: u64) -> io::Result<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the media file is not fully downloaded",
        ))
    }
}

fn cancelled() -> io::Error {
    io::Error::new(
        io::ErrorKind::Interrupted,
        "saving the media file was cancelled",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn retired_torrents_and_shutdown_reject_late_save_requests() {
        let root = tempfile::tempdir().unwrap();
        let mut config = torrentplayer_swarm::SwarmConfig::new(root.path().to_path_buf());
        config.enable_trackers = false;
        config.enable_dht = false;
        let swarm = torrentplayer_swarm::Swarm::start(config).await.unwrap();
        let mut bytes =
            b"d4:infod6:lengthi4e4:name9:video.mp412:piece lengthi16384e6:pieces20:".to_vec();
        bytes.extend_from_slice(&[0; 20]);
        bytes.extend_from_slice(b"ee");
        let torrent = swarm
            .add(
                torrentplayer_swarm::Metainfo::from_bytes(&bytes).unwrap(),
                Vec::new(),
            )
            .unwrap();
        let file = FileEntry {
            index: 0,
            path: "video.mp4".into(),
            len: 4,
            offset_in_torrent: 0,
            kind: crate::FileKind::Video,
        };
        for retired in [Some(9), None] {
            let files = CompletedFiles::default();
            files.remove(retired).await;
            files.schedule(9, &torrent, root.path(), &file);
            assert_eq!(files.status(9, 0), FileSaveStatus::default());
        }
        swarm.shutdown_async().await;
    }

    #[tokio::test]
    async fn saves_original_names_formats_and_bytes_and_reuses_them_after_restart() {
        let root = tempfile::tempdir().unwrap();
        let data: Vec<u8> = (0_u8..=250).cycle().take(700_013).collect();
        for name in ["Course/Lecture 01.mkv", "Album/Track 02.flac"] {
            let path = save_file(
                root.path(),
                name,
                data.len() as u64,
                data.as_slice(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(path, root.path().join("files").join(name));
            assert_eq!(fs::read(&path).unwrap(), data);
            let modified = fs::metadata(&path).unwrap().modified().unwrap();
            let again = save_file(
                root.path(),
                name,
                data.len() as u64,
                data.as_slice(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(again, path);
            assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
        }
    }

    #[tokio::test]
    async fn incomplete_reads_never_publish_a_video_or_leave_partial_files() {
        let root = tempfile::tempdir().unwrap();
        let error = save_file(
            root.path(),
            "video.mp4",
            10,
            &b"partial"[..],
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert!(!root.path().join("files/video.mp4").exists());
        assert_eq!(fs::read_dir(root.path().join("files")).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn existing_files_are_not_replaced_even_when_the_length_matches() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("files")).unwrap();
        let path = root.path().join("files/video.mp4");
        fs::write(&path, b"original").unwrap();
        let error = save_file(
            root.path(),
            "video.mp4",
            8,
            &b"new data"[..],
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(path).unwrap(), b"original");
    }

    #[tokio::test]
    async fn cancelled_saves_leave_no_partial_or_final_media() {
        let root = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let (_writer, reader) = tokio::io::duplex(64);
        let error = save_file(root.path(), "audio.flac", 100, reader, &cancel)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(fs::read_dir(root.path().join("files")).unwrap().count(), 0);
    }

    #[test]
    fn original_paths_cannot_escape_the_torrent_directory() {
        let root = tempfile::tempdir().unwrap();
        for path in [
            "../outside.mp4",
            "/outside.mp4",
            "nested/../../outside.mp4",
            "C:\\outside.mp4",
        ] {
            assert_eq!(
                prepare(root.path(), path, 1).err().unwrap().kind(),
                io::ErrorKind::InvalidInput
            );
        }
        assert!(!root.path().join("files").exists());
    }

    #[cfg(unix)]
    #[test]
    fn output_directories_and_files_cannot_be_symbolic_links() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root.path().join("files")).unwrap();
        assert!(prepare(root.path(), "video.mp4", 1).is_err());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}
