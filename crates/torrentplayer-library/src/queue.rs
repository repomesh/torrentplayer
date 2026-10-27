// SPDX-License-Identifier: MIT OR Apache-2.0

//! The play queue, stored as `queue.json`.
//!
//! An item holds only what is needed to play it again: the torrent's link (a magnet link or
//! an http(s) link to a `.torrent` file), the file index, a title, and a size. Never a stream
//! URL: those carry a session token, and links to loopback hosts are refused for that reason.
//!
//! Loading never fails: a missing file gives an empty queue and invalid items are dropped. A
//! file that cannot be read or is damaged is copied aside to `queue.invalid-<time>.json` and left
//! in place, so nothing the user queued is lost; [`Loaded::degraded`] tells the app not to clean
//! up downloads on the strength of such a queue. A file that loaded only in part (items dropped,
//! or written by a newer version) is copied aside the same way before it is first replaced. If a
//! copy cannot be made, [`QueueStore::save`] refuses to replace the file until one succeeds.
//!
//! Saving validates the whole queue first and is atomic (write a temporary file, flush it, rename
//! it over the old one).

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

/// The queue file's name inside the app data folder.
pub const QUEUE_FILE_NAME: &str = "queue.json";
/// A queue file that could not be read or used is copied to `queue.invalid-<time>.json` (Unix
/// seconds; an existing copy is never replaced) so it can be recovered.
pub const QUEUE_INVALID_STEM: &str = "queue.invalid";
/// A queue file written by a newer TorrentPlayer is copied to `queue.newer-<time>.json` before it
/// is first replaced, since saving drops what this version does not know.
pub const QUEUE_NEWER_STEM: &str = "queue.newer";
/// Version of the file format. Bump it, and add a migration to [`QueueStore::load`], when the
/// format changes.
pub const QUEUE_VERSION: u32 = 2;
/// The most items a queue holds.
pub const MAX_QUEUE_ITEMS: usize = 500;
/// Longest title or torrent name, in characters.
pub const MAX_TITLE_CHARS: usize = 1024;
/// Longest link, in bytes (magnet links with many trackers run to a few kilobytes).
pub const MAX_SOURCE_BYTES: usize = 16 * 1024;
/// Longest item id, in bytes.
pub const MAX_ID_BYTES: usize = 64;
/// Most trackers (`tr=`) in a magnet link. TorrentPlayer keeps 20 when it builds one for a torrent
/// opened from a file; search results carry up to about 20 too.
pub const MAX_MAGNET_TRACKERS: usize = 64;
/// Larger files are not read (500 items at the limits above fit well within it).
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// One queued video.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct QueueItem {
    /// Unique within the queue (letters, digits, `-` and `_`), chosen by the UI.
    pub id: String,
    /// Magnet link, or http(s) link to a `.torrent` file.
    pub source: String,
    /// Lowercase hex info hash, if known.
    pub info_hash: Option<String>,
    /// The file to play; `null` plays the torrent's main video.
    pub file_index: Option<u32>,
    /// What to show (plain text from the torrent; never render as HTML).
    pub title: String,
    /// The torrent's name, when the title is one file of it.
    pub torrent_name: Option<String>,
    /// Size in bytes, if known.
    pub size_bytes: Option<u64>,
    /// Paused by the user: not downloaded, and its torrent not uploaded once all of the
    /// torrent's items are paused, until resumed (playing it still works). Items saved as
    /// `skipped` (removed in 0.3.1) load as paused.
    #[serde(default)]
    pub paused: bool,
    /// The video reached its end during playback. Kept separately from the cursor so jumping
    /// ahead does not make every item before it look watched.
    #[serde(default)]
    pub played: bool,
    /// When it was queued, in milliseconds since the Unix epoch, for "Sort by… Recently
    /// added". Items queued before 0.3.1 have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub added_at: Option<u64>,
}

/// The queue: items in play order and a cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct Queue {
    /// Items in play order.
    pub items: Vec<QueueItem>,
    /// Id of the item playing or last played; the next item follows it. `null` starts at the
    /// first item.
    pub current: Option<String>,
}

/// Why a queue was refused. Messages start with the field's path (`items[2]: …`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueueError {
    /// More than [`MAX_QUEUE_ITEMS`] items.
    #[error("items: the queue can hold at most {MAX_QUEUE_ITEMS} items.")]
    TooManyItems,
    /// An item is invalid.
    #[error("items[{index}]: {reason}")]
    InvalidItem {
        /// Position of the item.
        index: usize,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// Two items share an id.
    #[error("items[{index}]: the id is used by an earlier item.")]
    DuplicateId {
        /// Position of the second item.
        index: usize,
    },
    /// `current` names no item.
    #[error("current: no item has this id.")]
    UnknownCurrent,
}

/// Why the queue could not be saved.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The queue is invalid; nothing was written.
    #[error(transparent)]
    Invalid(#[from] QueueError),
    /// Writing the file failed.
    #[error("could not write the queue file: {0}")]
    Io(#[from] io::Error),
    /// The queue could not be encoded (should not happen).
    #[error("could not encode the queue: {0}")]
    Encode(#[from] serde_json::Error),
    /// The existing file loaded only in part and still cannot be copied aside, so it is not
    /// replaced.
    #[error("the existing queue file could not be backed up, so it was not replaced: {0}")]
    Unread(io::Error),
}

impl QueueItem {
    /// Checks the item. The message says which field is wrong.
    ///
    /// # Errors
    ///
    /// Returns what is wrong with the first invalid field.
    pub fn validate(&self) -> Result<(), &'static str> {
        let id_ok = !self.id.is_empty()
            && self.id.len() <= MAX_ID_BYTES
            && self
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !id_ok {
            return Err("id must be 1-64 letters, digits, '-' or '_'.");
        }
        validate_source(&self.source)?;
        if let Some(hash) = &self.info_hash
            && !(hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err("infoHash must be 40 hex digits.");
        }
        if !valid_text(&self.title) {
            return Err("title must be 1-1024 characters without control characters.");
        }
        if let Some(name) = &self.torrent_name
            && !valid_text(name)
        {
            return Err("torrentName must be 1-1024 characters without control characters.");
        }
        Ok(())
    }
}

impl Queue {
    /// Checks every item, the item limit, id uniqueness, and the cursor.
    ///
    /// # Errors
    ///
    /// Returns the first problem found.
    pub fn validate(&self) -> Result<(), QueueError> {
        if self.items.len() > MAX_QUEUE_ITEMS {
            return Err(QueueError::TooManyItems);
        }
        let mut ids = HashSet::new();
        for (index, item) in self.items.iter().enumerate() {
            item.validate()
                .map_err(|reason| QueueError::InvalidItem { index, reason })?;
            if !ids.insert(item.id.as_str()) {
                return Err(QueueError::DuplicateId { index });
            }
        }
        match &self.current {
            Some(current) if !ids.contains(current.as_str()) => Err(QueueError::UnknownCurrent),
            _ => Ok(()),
        }
    }
}

/// A link the queue may store: a magnet link with a BitTorrent info hash, or an http(s) link
/// to a host other than this machine (stream URLs are loopback URLs with a session token).
pub(crate) fn validate_source(source: &str) -> Result<(), &'static str> {
    if source.is_empty() || source.len() > MAX_SOURCE_BYTES {
        return Err("source must be 1-16384 bytes.");
    }
    if source.chars().any(char::is_control) || source.trim() != source {
        return Err("source must not contain spaces at the ends or control characters.");
    }
    if source
        .get(..8)
        .is_some_and(|p| p.eq_ignore_ascii_case("magnet:?"))
    {
        let has_hash = source[8..].split('&').any(|pair| {
            pair.split_once('=').is_some_and(|(key, value)| {
                key == "xt"
                    && value
                        .get(..9)
                        .is_some_and(|urn| urn.eq_ignore_ascii_case("urn:btih:"))
            })
        });
        let trackers = source[8..]
            .split('&')
            .filter(|pair| pair.starts_with("tr="))
            .count();
        return if !has_hash {
            Err("source: a magnet link needs an xt=urn:btih: info hash.")
        } else if trackers > MAX_MAGNET_TRACKERS {
            Err("source: a magnet link may list at most 64 trackers.")
        } else {
            Ok(())
        };
    }
    let url = url::Url::parse(source).map_err(|_| "source must be a magnet or http(s) link.")?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("source must be a magnet or http(s) link.");
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("source must not contain a user name or password.");
    }
    let local = match url.host() {
        None => true,
        Some(url::Host::Domain(domain)) => {
            let domain = domain.trim_end_matches('.').to_ascii_lowercase();
            domain == "localhost" || domain.ends_with(".localhost")
        }
        Some(url::Host::Ipv4(ip)) => ip.is_loopback() || ip.is_unspecified(),
        Some(url::Host::Ipv6(ip)) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    };
    if local {
        return Err("source must not point to this computer.");
    }
    Ok(())
}

pub(crate) fn valid_text(text: &str) -> bool {
    let chars = text.chars().count();
    (1..=MAX_TITLE_CHARS).contains(&chars) && !text.chars().any(char::is_control)
}

/// The file format: the queue plus its version.
#[derive(Serialize)]
struct QueueFileRef<'a> {
    version: u32,
    items: &'a [QueueItem],
    current: Option<&'a str>,
}

/// The result of [`QueueStore::load`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    /// The queue to use.
    pub queue: Queue,
    /// Problems found while loading, for the log. Never contains paths.
    pub warnings: Vec<String>,
    /// Whether the file could not be read in full (unreadable, damaged, invalid items, or a
    /// newer version), so `queue` may miss items the user queued. Nothing should be deleted on
    /// the assumption that it is not queued.
    pub degraded: bool,
}

/// Reads and writes `queue.json` in one folder (the app data folder).
#[derive(Debug, Clone)]
pub struct QueueStore {
    dir: PathBuf,
    /// Set by [`Self::load`] when the file loaded only in part and has no copy yet: the stem of
    /// the copy [`Self::save`] makes before replacing it (refusing to save while it cannot).
    /// Shared by clones.
    unsaved_original: Arc<Mutex<Option<&'static str>>>,
}

/// Reading the queue file is retried this often when another program (an antivirus scanner,
/// a backup tool) briefly holds it open.
const READ_ATTEMPTS: u32 = 5;
const READ_RETRY_DELAY: Duration = Duration::from_millis(50);

impl QueueStore {
    /// A store for `dir/queue.json`.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            unsaved_original: Arc::new(Mutex::new(None)),
        }
    }

    /// The queue file's path.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.dir.join(QUEUE_FILE_NAME)
    }

    /// Loads the queue, dropping invalid items and falling back to an empty queue (with a
    /// warning) if the file cannot be read or is damaged (see the module docs).
    #[must_use]
    pub fn load(&self) -> Loaded {
        self.set_unsaved(None);
        let mut warnings = Vec::new();
        let bytes = match read_limited(&self.path()) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                return Loaded {
                    queue: Queue::default(),
                    warnings,
                    degraded: false,
                };
            }
            Err(error) => {
                tracing::warn!(%error, "could not read the queue; starting empty");
                let kept = self.keep_original(None);
                warnings.push(unusable_warning("could not be read", kept));
                return Loaded {
                    queue: Queue::default(),
                    warnings,
                    degraded: true,
                };
            }
        };
        let Ok(Value::Object(mut doc)) = serde_json::from_slice::<Value>(&bytes) else {
            let kept = self.keep_original(Some(&bytes));
            warnings.push(unusable_warning("was damaged", kept));
            for warning in &warnings {
                tracing::warn!(%warning, "queue loaded with a problem");
            }
            return Loaded {
                queue: Queue::default(),
                warnings,
                degraded: true,
            };
        };
        let version = doc
            .get("version")
            .and_then(Value::as_u64)
            .map_or(1, |v| u32::try_from(v).unwrap_or(u32::MAX));
        let newer = version > QUEUE_VERSION;
        if newer {
            // Read what this version understands; unknown fields are ignored, and the file is
            // copied aside before it is first replaced.
            warnings.push(format!(
                "The queue was saved by a newer version of TorrentPlayer; it is copied to \
                 {QUEUE_NEWER_STEM}-<time>.json before it is next saved."
            ));
        }
        let raw = match doc.remove("items") {
            Some(Value::Array(items)) => items,
            _ => Vec::new(),
        };
        let mut queue = Queue::default();
        let mut ids = HashSet::new();
        let mut dropped = 0_usize;
        for mut value in raw {
            // `skipped` (0.3.0) kept an item without downloading it: it loads as paused.
            if let Some(fields) = value.as_object_mut()
                && fields.remove("skipped") == Some(Value::Bool(true))
            {
                fields.insert("paused".to_owned(), Value::Bool(true));
            }
            match serde_json::from_value::<QueueItem>(value) {
                Ok(item)
                    if queue.items.len() < MAX_QUEUE_ITEMS
                        && item.validate().is_ok()
                        && ids.insert(item.id.clone()) =>
                {
                    queue.items.push(item);
                }
                _ => dropped += 1,
            }
        }
        if dropped > 0 {
            warnings.push(format!(
                "{dropped} invalid queue item{} dropped.",
                if dropped == 1 { " was" } else { "s were" }
            ));
        }
        if newer {
            self.set_unsaved(Some(QUEUE_NEWER_STEM));
        } else if dropped > 0 {
            self.set_unsaved(Some(QUEUE_INVALID_STEM));
        }
        queue.current = doc
            .get("current")
            .and_then(Value::as_str)
            .filter(|c| ids.contains(*c))
            .map(str::to_owned);
        // `order` (in order or fastest first, removed in 0.3.1) is ignored and dropped on the
        // next save: the queue always downloads in queue order.
        for warning in &warnings {
            tracing::warn!(%warning, "queue loaded with a problem");
        }
        let degraded = !warnings.is_empty();
        Loaded {
            queue,
            warnings,
            degraded,
        }
    }

    /// Validates and saves the queue atomically, readable only by the current user where the
    /// OS supports it.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Invalid`] (writing nothing) if the queue is invalid,
    /// [`StoreError::Unread`] if the file loaded only in part and still cannot be copied aside,
    /// or an I/O error if the folder or file cannot be written.
    pub fn save(&self, queue: &Queue) -> Result<(), StoreError> {
        queue.validate()?;
        let mut unsaved = self
            .unsaved_original
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(stem) = *unsaved {
            self.copy_original(stem).map_err(StoreError::Unread)?;
            *unsaved = None;
        }
        drop(unsaved);
        let mut json = serde_json::to_vec_pretty(&QueueFileRef {
            version: QUEUE_VERSION,
            items: &queue.items,
            current: queue.current.as_deref(),
        })?;
        json.push(b'\n');
        create_private_dir(&self.dir)?;
        let tmp = self.dir.join(format!("{QUEUE_FILE_NAME}.tmp"));
        let result = write_private(&tmp, &json).and_then(|()| fs::rename(&tmp, self.path()));
        if result.is_err() {
            // Best effort: a stale temporary file is harmless and replaced on the next save.
            let _ = fs::remove_file(&tmp);
        }
        Ok(result?)
    }

    fn set_unsaved(&self, stem: Option<&'static str>) {
        *self
            .unsaved_original
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = stem;
    }

    /// Copies a file that could not be used aside, from `bytes` if it was read. Returns the
    /// copy's file name; if there is none, saving is refused until a copy can be made.
    fn keep_original(&self, bytes: Option<&[u8]>) -> Option<String> {
        let result = match bytes {
            Some(bytes) => write_aside(&self.dir, QUEUE_INVALID_STEM, bytes).map(Some),
            None => self.copy_original(QUEUE_INVALID_STEM),
        };
        match result {
            Ok(name) => name,
            Err(error) => {
                tracing::warn!(%error, "could not copy the unusable queue file aside");
                self.set_unsaved(Some(QUEUE_INVALID_STEM));
                None
            }
        }
    }

    /// Copies `queue.json` to a new `<stem>-<time>.json` and returns its name; a file that is
    /// gone needs no copy.
    fn copy_original(&self, stem: &str) -> io::Result<Option<String>> {
        let bytes = read_retrying(&self.path(), |path| {
            if fs::metadata(path)?.len() > MAX_FILE_BYTES {
                // Too large to hold in memory: copied as it is, without looking for a twin.
                return Ok(None);
            }
            fs::read(path).map(Some)
        });
        let bytes = match bytes {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if let Some(bytes) = bytes {
            return write_aside(&self.dir, stem, &bytes).map(Some);
        }
        let (target, file) = new_aside_file(&self.dir, stem)?;
        drop(file);
        if let Err(error) = fs::copy(self.path(), &target) {
            let _ = fs::remove_file(&target);
            return Err(error);
        }
        Ok(Some(file_name(&target)))
    }
}

/// The warning for a queue file that could not be used; `kept` is the copy's name, if one was
/// made.
fn unusable_warning(problem: &str, kept: Option<String>) -> String {
    let kept = match kept {
        Some(name) => format!("The old file was kept as {name}."),
        None => format!("{QUEUE_FILE_NAME} is not replaced until it can be read or backed up."),
    };
    format!("{QUEUE_FILE_NAME} {problem}, so the queue starts empty. {kept}")
}

/// Writes `bytes` to a new `<stem>-<time>.json` in `dir`, or reuses an earlier copy with the
/// same contents (so a file that stays damaged is not copied again on every start). Returns the
/// copy's file name.
fn write_aside(dir: &Path, stem: &str, bytes: &[u8]) -> io::Result<String> {
    if let Some(name) = same_copy(dir, stem, bytes) {
        return Ok(name);
    }
    let (path, mut file) = new_aside_file(dir, stem)?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(&path);
        return Err(error);
    }
    Ok(file_name(&path))
}

/// An earlier `<stem>-*.json` copy in `dir` holding exactly `bytes`.
fn same_copy(dir: &Path, stem: &str, bytes: &[u8]) -> Option<String> {
    let prefix = format!("{stem}-");
    fs::read_dir(dir).ok()?.flatten().find_map(|entry| {
        let name = entry.file_name().into_string().ok()?;
        let same = name.starts_with(&prefix)
            && entry
                .metadata()
                .is_ok_and(|m| usize::try_from(m.len()).is_ok_and(|len| len == bytes.len()))
            && fs::read(entry.path()).is_ok_and(|b| b == bytes);
        same.then_some(name)
    })
}

/// Creates a new, empty `<stem>-<time>[-n].json` in `dir`, never replacing an existing file.
fn new_aside_file(dir: &Path, stem: &str) -> io::Result<(PathBuf, fs::File)> {
    create_private_dir(dir)?;
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    for n in 1..=100_u32 {
        let name = if n == 1 {
            format!("{stem}-{secs}.json")
        } else {
            format!("{stem}-{secs}-{n}.json")
        };
        let path = dir.join(name);
        match open_private(&path, true) {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "too many copies of the queue file",
    ))
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Whether a read error is likely to pass soon: on Windows, another program (typically an
/// antivirus scanner) holding the file open without sharing it.
fn is_transient(error: &io::Error) -> bool {
    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ERROR_LOCK_VIOLATION: i32 = 33;
    error.kind() == io::ErrorKind::Interrupted
        || (cfg!(windows)
            && matches!(
                error.raw_os_error(),
                Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
            ))
}

/// Runs `read`, retrying briefly while its error is transient.
fn read_retrying<T>(path: &Path, read: impl Fn(&Path) -> io::Result<T>) -> io::Result<T> {
    let mut attempt = 1;
    loop {
        match read(path) {
            Err(e) if attempt < READ_ATTEMPTS && is_transient(&e) => {
                std::thread::sleep(READ_RETRY_DELAY);
                attempt += 1;
            }
            result => return result,
        }
    }
}

/// Reads a file of at most [`MAX_FILE_BYTES`], retrying transient errors; `Ok(None)` if it
/// does not exist.
fn read_limited(path: &Path) -> io::Result<Option<Vec<u8>>> {
    read_retrying(path, |path| match fs::metadata(path) {
        Ok(meta) if meta.len() > MAX_FILE_BYTES => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "queue file is too large",
        )),
        Ok(_) => fs::read(path).map(Some),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    })
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)
    }
}

/// Writes `bytes` to a new file only the current user can read, and flushes it to disk.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = open_private(path, false)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Opens `path` for writing, readable only by the current user where the OS supports it:
/// truncating it, or failing with `AlreadyExists` if `new` and it exists.
fn open_private(path: &Path, new: bool) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true);
    if new {
        options.create_new(true);
    } else {
        options.create(true).truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c";

    fn item(id: &str) -> QueueItem {
        QueueItem {
            id: id.into(),
            source: format!("magnet:?xt=urn:btih:{HASH}&dn=Sintel"),
            info_hash: Some(HASH.into()),
            file_index: Some(2),
            title: "Sintel.2010.1080p.mp4".into(),
            torrent_name: Some("Sintel".into()),
            size_bytes: Some(1_234_567_890),
            paused: false,
            played: false,
            added_at: None,
        }
    }

    fn queue(ids: &[&str], current: Option<&str>) -> Queue {
        Queue {
            items: ids.iter().map(|id| item(id)).collect(),
            current: current.map(str::to_owned),
        }
    }

    #[test]
    fn round_trips_atomically_with_a_version() {
        let dir = tempfile::tempdir().unwrap();
        let store = QueueStore::new(dir.path().join("nested"));
        assert_eq!(store.load().queue, Queue::default(), "missing file: empty");

        let saved = queue(&["a", "b-2", "c_3"], Some("b-2"));
        store.save(&saved).unwrap();
        let loaded = store.load();
        assert_eq!(loaded.queue, saved);
        assert!(loaded.warnings.is_empty());

        let json: Value = serde_json::from_str(&fs::read_to_string(store.path()).unwrap()).unwrap();
        assert_eq!(json["version"], QUEUE_VERSION);
        assert_eq!(json["items"][0]["fileIndex"], 2);
        assert_eq!(json["current"], "b-2");
        assert!(
            !dir.path().join("nested/queue.json.tmp").exists(),
            "temporary file renamed"
        );
    }

    #[test]
    fn keeps_item_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = QueueStore::new(dir.path());
        let mut saved = queue(&["a", "b"], None);
        saved.items[1].paused = true;
        saved.items[0].played = true;
        store.save(&saved).unwrap();
        assert_eq!(store.load().queue, saved);
        let json: Value = serde_json::from_str(&fs::read_to_string(store.path()).unwrap()).unwrap();
        assert_eq!(json["items"][1]["paused"], true);
        assert_eq!(json["items"][0]["played"], true);

        // Files from before these existed: nothing paused.
        let mut old = json;
        old["items"][1].as_object_mut().unwrap().remove("paused");
        old["items"][0].as_object_mut().unwrap().remove("played");
        fs::write(store.path(), old.to_string()).unwrap();
        let loaded = store.load().queue;
        assert!(!loaded.items[1].paused);
        assert!(!loaded.items[0].played);
    }

    #[test]
    fn keeps_when_items_were_added_and_loads_items_without_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = QueueStore::new(dir.path());
        let mut saved = queue(&["a", "b"], None);
        saved.items[0].added_at = Some(1_792_000_000_000);
        store.save(&saved).unwrap();
        assert_eq!(store.load().queue, saved);
        let json: Value = serde_json::from_str(&fs::read_to_string(store.path()).unwrap()).unwrap();
        assert_eq!(json["items"][0]["addedAt"], 1_792_000_000_000_u64);
        assert!(
            json["items"][1].get("addedAt").is_none(),
            "unknown: left out"
        );

        // A file saved before the field existed.
        let mut old = json;
        old["items"][0].as_object_mut().unwrap().remove("addedAt");
        fs::write(store.path(), old.to_string()).unwrap();
        let loaded = store.load();
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.queue.items.len(), 2);
        assert!(loaded.queue.items.iter().all(|i| i.added_at.is_none()));
    }

    #[test]
    fn skipped_items_from_older_versions_load_as_paused() {
        let dir = tempfile::tempdir().unwrap();
        let store = QueueStore::new(dir.path());
        store.save(&queue(&["a", "b", "c"], None)).unwrap();
        let mut old: Value =
            serde_json::from_str(&fs::read_to_string(store.path()).unwrap()).unwrap();
        old["items"][0]["skipped"] = true.into();
        old["items"][1]["skipped"] = false.into();
        fs::write(store.path(), old.to_string()).unwrap();
        let loaded = store.load();
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        let paused: Vec<bool> = loaded.queue.items.iter().map(|i| i.paused).collect();
        assert_eq!(paused, [true, false, false], "kept, not removed");
        store.save(&loaded.queue).unwrap();
        let saved = fs::read_to_string(store.path()).unwrap();
        assert!(!saved.contains("skipped"), "dropped on save");
    }

    #[test]
    fn ignores_the_removed_download_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = QueueStore::new(dir.path());
        let saved = queue(&["a", "b"], Some("a"));
        store.save(&saved).unwrap();
        // A file saved by 0.3.0 with "Fastest first" chosen.
        let mut old: Value =
            serde_json::from_str(&fs::read_to_string(store.path()).unwrap()).unwrap();
        old["order"] = "fastest".into();
        fs::write(store.path(), old.to_string()).unwrap();
        let loaded = store.load();
        assert_eq!(loaded.queue, saved);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        store.save(&loaded.queue).unwrap();
        let json: Value = serde_json::from_str(&fs::read_to_string(store.path()).unwrap()).unwrap();
        assert!(json.get("order").is_none(), "dropped on save");
        // The frontend's queue with the old field still deserializes.
        let sent: Queue = serde_json::from_value(
            serde_json::json!({"items": [], "current": null, "order": "fastest"}),
        )
        .unwrap();
        assert_eq!(sent, Queue::default());
    }

    #[test]
    fn refuses_invalid_queues_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = QueueStore::new(dir.path());
        let mut bad = queue(&["a", "a"], None);
        assert!(matches!(
            store.save(&bad),
            Err(StoreError::Invalid(QueueError::DuplicateId { index: 1 }))
        ));
        bad = queue(&["a"], Some("zzz"));
        assert!(matches!(
            store.save(&bad),
            Err(StoreError::Invalid(QueueError::UnknownCurrent))
        ));
        let ids: Vec<String> = (0..=MAX_QUEUE_ITEMS).map(|i| format!("i{i}")).collect();
        let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
        assert!(matches!(
            store.save(&queue(&ids, None)),
            Err(StoreError::Invalid(QueueError::TooManyItems))
        ));
        assert!(!store.path().exists());
    }

    #[test]
    fn validates_item_fields() {
        let ok = item("a");
        assert_eq!(ok.validate(), Ok(()));
        let with = |f: fn(&mut QueueItem)| {
            let mut i = ok.clone();
            f(&mut i);
            i.validate()
        };
        assert!(with(|i| i.id = String::new()).is_err());
        assert!(with(|i| i.id = "has space".into()).is_err());
        assert!(with(|i| i.id = "x".repeat(65)).is_err());
        assert!(with(|i| i.info_hash = Some("abc".into())).is_err());
        assert!(with(|i| i.title = String::new()).is_err());
        assert!(with(|i| i.title = "a\nb".into()).is_err());
        assert!(with(|i| i.title = "é".repeat(MAX_TITLE_CHARS)).is_ok());
        assert!(with(|i| i.title = "é".repeat(MAX_TITLE_CHARS + 1)).is_err());
        assert!(with(|i| i.torrent_name = Some(String::new())).is_err());
        assert!(with(|i| i.info_hash = None).is_ok());
        assert!(with(|i| i.file_index = None).is_ok());
    }

    #[test]
    fn stores_torrent_links_but_never_stream_urls() {
        let accepts = |source: &str| validate_source(source).is_ok();
        assert!(accepts(&format!("magnet:?xt=urn:btih:{HASH}")));
        assert!(accepts(&format!(
            "MAGNET:?dn=x&xt=URN:BTIH:{HASH}&tr=udp://t.example:1"
        )));
        assert!(accepts("https://academictorrents.com/download/abc.torrent"));
        assert!(accepts("http://192.168.1.20/course.torrent"));

        let trackers = |n: usize| "&tr=udp%3A%2F%2Ft.example%3A1337".repeat(n);
        assert!(accepts(&format!(
            "magnet:?xt=urn:btih:{HASH}&dn=Course{}",
            trackers(MAX_MAGNET_TRACKERS)
        )));
        assert!(!accepts(&format!(
            "magnet:?xt=urn:btih:{HASH}{}",
            trackers(MAX_MAGNET_TRACKERS + 1)
        )));
        assert!(!accepts("magnet:?dn=no-hash"));
        assert!(!accepts("ftp://example.org/a.torrent"));
        assert!(!accepts("file:///C:/a.torrent"));
        assert!(!accepts("not a link"));
        assert!(!accepts(" https://example.org/a.torrent"));
        assert!(!accepts("https://user:pass@example.org/a.torrent"));
        // Stream URLs: the loopback server, with a token.
        assert!(!accepts("http://127.0.0.1:52100/stream/1?token=secret"));
        assert!(!accepts("http://localhost:52100/stream/1"));
        assert!(!accepts("http://app.localhost./stream/1"));
        assert!(!accepts("http://[::1]:52100/stream/1"));
        assert!(!accepts("http://[::ffff:127.0.0.1]/stream/1"));
        assert!(!accepts("http://0.0.0.0/stream/1"));
        assert!(!accepts(&format!(
            "https://e.org/{}",
            "a".repeat(MAX_SOURCE_BYTES)
        )));
    }

    #[test]
    fn loading_drops_invalid_items_and_dangling_cursors() {
        let dir = tempfile::tempdir().unwrap();
        let store = QueueStore::new(dir.path());
        let good = serde_json::to_value(item("a")).unwrap();
        let mut stream_url = item("b");
        stream_url.source = "http://127.0.0.1:1/stream/1?token=t".into();
        let doc = serde_json::json!({
            "version": 1,
            "items": [good, serde_json::to_value(stream_url).unwrap(), {"id": 5}, good],
            "current": "b",
        });
        fs::write(store.path(), doc.to_string()).unwrap();
        let loaded = store.load();
        assert_eq!(loaded.queue.items, vec![item("a")]);
        assert_eq!(loaded.queue.current, None, "the cursor's item was dropped");
        assert_eq!(loaded.warnings, vec!["3 invalid queue items were dropped."]);
    }

    /// The names of the copies set aside in `dir` with `stem`.
    fn copies(dir: &Path, stem: &str) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.starts_with(&format!("{stem}-")))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn damaged_files_are_copied_aside_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let store = QueueStore::new(dir.path());
        fs::write(store.path(), "{ not json").unwrap();
        let loaded = store.load();
        assert_eq!(loaded.queue, Queue::default());
        assert!(loaded.degraded);
        assert_eq!(loaded.warnings.len(), 1);
        assert!(!loaded.warnings[0].contains(&dir.path().display().to_string()));
        let kept = copies(dir.path(), QUEUE_INVALID_STEM);
        assert_eq!(kept.len(), 1);
        assert!(
            loaded.warnings[0].contains(&kept[0]),
            "{:?}",
            loaded.warnings
        );
        assert_eq!(fs::read(dir.path().join(&kept[0])).unwrap(), b"{ not json");
        assert_eq!(
            fs::read(store.path()).unwrap(),
            b"{ not json",
            "left in place"
        );

        // Still damaged at the next start: degraded again, but not copied twice.
        assert!(store.load().degraded);
        assert_eq!(copies(dir.path(), QUEUE_INVALID_STEM), kept);

        // A different damaged file gets a copy of its own; the first is never replaced.
        fs::write(store.path(), "[1, 2").unwrap();
        assert!(store.load().degraded);
        let both = copies(dir.path(), QUEUE_INVALID_STEM);
        assert_eq!(both.len(), 2);
        assert_eq!(fs::read(dir.path().join(&kept[0])).unwrap(), b"{ not json");

        store.save(&queue(&["a"], None)).unwrap();
        assert_eq!(copies(dir.path(), QUEUE_INVALID_STEM), both);
        assert!(!store.load().degraded);
    }

    #[test]
    fn files_that_are_not_utf8_are_damaged_not_lost() {
        let dir = tempfile::tempdir().unwrap();
        let store = QueueStore::new(dir.path());
        let bytes = b"{\"version\": 2, \"items\": [\"\xff\xfe\"]}";
        fs::write(store.path(), bytes).unwrap();
        let loaded = store.load();
        assert!(loaded.degraded);
        let kept = copies(dir.path(), QUEUE_INVALID_STEM);
        assert_eq!(kept.len(), 1);
        assert_eq!(fs::read(dir.path().join(&kept[0])).unwrap(), bytes);
    }

    #[test]
    fn unreadable_files_are_never_replaced_without_a_copy() {
        let dir = tempfile::tempdir().unwrap();
        let store = QueueStore::new(dir.path());
        // A folder in the file's place cannot be read or copied on any platform.
        fs::create_dir(store.path()).unwrap();
        let loaded = store.load();
        assert_eq!(loaded.queue, Queue::default());
        assert!(loaded.degraded);
        assert_eq!(loaded.warnings.len(), 1);
        assert!(
            loaded.warnings[0].contains("not replaced"),
            "{:?}",
            loaded.warnings
        );
        assert!(copies(dir.path(), QUEUE_INVALID_STEM).is_empty());
        assert!(matches!(
            store.clone().save(&queue(&["a"], None)),
            Err(StoreError::Unread(_))
        ));
        assert!(store.path().is_dir(), "nothing was replaced");
    }

    #[cfg(windows)]
    #[test]
    fn briefly_locked_files_are_read_after_a_retry() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = tempfile::tempdir().unwrap();
        let store = QueueStore::new(dir.path());
        store.save(&queue(&["a"], None)).unwrap();
        // Held without sharing, as antivirus scanners do while they look at a new file.
        let lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(store.path())
            .unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            drop(lock);
        });
        let loaded = store.load();
        release.join().unwrap();
        assert_eq!(loaded.queue, queue(&["a"], None));
        assert!(!loaded.degraded);
    }

    #[cfg(windows)]
    #[test]
    fn locked_files_are_kept_until_they_can_be_copied() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = tempfile::tempdir().unwrap();
        let store = QueueStore::new(dir.path());
        store.save(&queue(&["a", "b"], None)).unwrap();
        let original = fs::read(store.path()).unwrap();
        let lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(store.path())
            .unwrap();
        let loaded = store.load();
        assert!(loaded.degraded);
        assert_eq!(loaded.queue, Queue::default());
        assert!(matches!(
            store.save(&Queue::default()),
            Err(StoreError::Unread(_))
        ));
        drop(lock);
        // Once the file can be read, it is copied before it is replaced.
        store.save(&Queue::default()).unwrap();
        let kept = copies(dir.path(), QUEUE_INVALID_STEM);
        assert_eq!(kept.len(), 1);
        assert_eq!(fs::read(dir.path().join(&kept[0])).unwrap(), original);
    }

    #[test]
    fn files_with_dropped_items_are_copied_before_they_are_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let store = QueueStore::new(dir.path());
        let doc = serde_json::json!({
            "version": 2,
            "items": [serde_json::to_value(item("a")).unwrap(), {"id": 5}],
        });
        fs::write(store.path(), doc.to_string()).unwrap();
        let loaded = store.load();
        assert!(loaded.degraded);
        assert!(
            copies(dir.path(), QUEUE_INVALID_STEM).is_empty(),
            "copied when saved"
        );
        store.save(&loaded.queue).unwrap();
        let kept = copies(dir.path(), QUEUE_INVALID_STEM);
        assert_eq!(kept.len(), 1);
        assert_eq!(
            fs::read_to_string(dir.path().join(&kept[0])).unwrap(),
            doc.to_string()
        );
        store.save(&loaded.queue).unwrap();
        assert_eq!(copies(dir.path(), QUEUE_INVALID_STEM), kept, "copied once");
    }

    #[test]
    fn newer_files_are_read_as_far_as_possible() {
        let dir = tempfile::tempdir().unwrap();
        let store = QueueStore::new(dir.path());
        let mut value = serde_json::to_value(item("a")).unwrap();
        value["rating"] = 5.into();
        let doc = serde_json::json!({ "version": 9, "items": [value], "shuffle": true });
        fs::write(store.path(), doc.to_string()).unwrap();
        let loaded = store.load();
        assert_eq!(loaded.queue.items, vec![item("a")]);
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.degraded);

        // Its unknown fields are kept in a copy before it is first replaced.
        assert!(copies(dir.path(), QUEUE_NEWER_STEM).is_empty());
        store.save(&loaded.queue).unwrap();
        let kept = copies(dir.path(), QUEUE_NEWER_STEM);
        assert_eq!(kept.len(), 1);
        assert_eq!(
            fs::read_to_string(dir.path().join(&kept[0])).unwrap(),
            doc.to_string()
        );
        assert!(!store.load().degraded);
    }
}
