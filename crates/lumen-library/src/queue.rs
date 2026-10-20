// SPDX-License-Identifier: MIT OR Apache-2.0

//! The play queue, stored as `queue.json`.
//!
//! An item holds only what is needed to play it again: the torrent's link (a magnet link or
//! an http(s) link to a `.torrent` file), the file index, a title, and a size. Never a stream
//! URL: those carry a session token, and links to loopback hosts are refused for that reason.
//!
//! Loading never fails: a missing file gives an empty queue, invalid items are dropped, and a
//! damaged file is set aside as `queue.invalid.json`. Saving validates the whole queue first
//! and is atomic (write a temporary file, flush it, rename it over the old one).

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

/// The queue file's name inside the app data folder.
pub const QUEUE_FILE_NAME: &str = "queue.json";
/// A damaged queue file is renamed to this so it can be recovered.
pub const QUEUE_INVALID_FILE_NAME: &str = "queue.invalid.json";
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
/// Most trackers (`tr=`) in a magnet link. TorrentStream keeps 20 when it builds one for a torrent
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
    /// Not downloaded in the background until resumed (playing it still works).
    #[serde(default)]
    pub paused: bool,
    /// Kept in the queue but skipped: not played automatically and not downloaded in the
    /// background (unticked in the queue). Playing it directly still works.
    #[serde(default)]
    pub skipped: bool,
    /// The video reached its end during playback. Kept separately from the cursor so jumping
    /// ahead does not make every item before it look watched.
    #[serde(default)]
    pub played: bool,
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
    /// How spare bandwidth is shared between items.
    #[serde(default)]
    pub order: DownloadOrder,
}

/// How spare bandwidth is shared between queued items that download in the background.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum DownloadOrder {
    /// In queue order (for courses and series watched in sequence).
    #[default]
    InOrder,
    /// Torrents that download fastest first; each torrent's own files stay in queue order.
    Fastest,
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
fn validate_source(source: &str) -> Result<(), &'static str> {
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

fn valid_text(text: &str) -> bool {
    let chars = text.chars().count();
    (1..=MAX_TITLE_CHARS).contains(&chars) && !text.chars().any(char::is_control)
}

/// The file format: the queue plus its version.
#[derive(Serialize)]
struct QueueFileRef<'a> {
    version: u32,
    items: &'a [QueueItem],
    current: Option<&'a str>,
    order: DownloadOrder,
}

/// The result of [`QueueStore::load`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    /// The queue to use.
    pub queue: Queue,
    /// Problems found while loading, for the log. Never contains paths.
    pub warnings: Vec<String>,
}

/// Reads and writes `queue.json` in one folder (the app data folder).
#[derive(Debug, Clone)]
pub struct QueueStore {
    dir: PathBuf,
}

impl QueueStore {
    /// A store for `dir/queue.json`.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The queue file's path.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.dir.join(QUEUE_FILE_NAME)
    }

    /// Loads the queue, dropping invalid items and falling back to an empty queue (with a
    /// warning) if the file is damaged.
    #[must_use]
    pub fn load(&self) -> Loaded {
        let mut warnings = Vec::new();
        let text = match read_limited(&self.path()) {
            Ok(Some(text)) => text,
            Ok(None) => {
                return Loaded {
                    queue: Queue::default(),
                    warnings,
                };
            }
            Err(error) => {
                tracing::warn!(%error, "could not read the queue; starting empty");
                warnings.push("The queue could not be read, so it starts empty.".to_owned());
                return Loaded {
                    queue: Queue::default(),
                    warnings,
                };
            }
        };
        let Ok(Value::Object(mut doc)) = serde_json::from_str::<Value>(&text) else {
            self.set_aside();
            warnings.push(format!(
                "{QUEUE_FILE_NAME} was damaged, so the queue starts empty. The old file was kept \
                 as {QUEUE_INVALID_FILE_NAME}."
            ));
            return Loaded {
                queue: Queue::default(),
                warnings,
            };
        };
        let version = doc
            .get("version")
            .and_then(Value::as_u64)
            .map_or(1, |v| u32::try_from(v).unwrap_or(u32::MAX));
        if version > QUEUE_VERSION {
            // Read what this version understands; unknown fields are ignored.
            warnings.push("The queue was saved by a newer version of TorrentStream.".to_owned());
        }
        let raw = match doc.remove("items") {
            Some(Value::Array(items)) => items,
            _ => Vec::new(),
        };
        let mut queue = Queue::default();
        let mut ids = HashSet::new();
        let mut dropped = 0_usize;
        for value in raw {
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
        queue.current = doc
            .get("current")
            .and_then(Value::as_str)
            .filter(|c| ids.contains(*c))
            .map(str::to_owned);
        // An unknown order (from a newer version) falls back to the default.
        queue.order = doc
            .remove("order")
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        for warning in &warnings {
            tracing::warn!(%warning, "queue loaded with a problem");
        }
        Loaded { queue, warnings }
    }

    /// Validates and saves the queue atomically, readable only by the current user where the
    /// OS supports it.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Invalid`] (writing nothing) if the queue is invalid, or an I/O
    /// error if the folder or file cannot be written.
    pub fn save(&self, queue: &Queue) -> Result<(), StoreError> {
        queue.validate()?;
        let mut json = serde_json::to_vec_pretty(&QueueFileRef {
            version: QUEUE_VERSION,
            items: &queue.items,
            current: queue.current.as_deref(),
            order: queue.order,
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

    fn set_aside(&self) {
        let target = self.dir.join(QUEUE_INVALID_FILE_NAME);
        if let Err(error) = fs::rename(self.path(), target) {
            tracing::warn!(%error, "could not set the damaged queue file aside");
        }
    }
}

/// Reads a file of at most [`MAX_FILE_BYTES`]; `Ok(None)` if it does not exist.
fn read_limited(path: &Path) -> io::Result<Option<String>> {
    match fs::metadata(path) {
        Ok(meta) if meta.len() > MAX_FILE_BYTES => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "queue file is too large",
        )),
        Ok(_) => fs::read_to_string(path).map(Some),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
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
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
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
            skipped: false,
            played: false,
        }
    }

    fn queue(ids: &[&str], current: Option<&str>) -> Queue {
        Queue {
            items: ids.iter().map(|id| item(id)).collect(),
            current: current.map(str::to_owned),
            order: DownloadOrder::InOrder,
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
    fn keeps_item_state_and_the_download_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = QueueStore::new(dir.path());
        let mut saved = queue(&["a", "b"], None);
        saved.items[1].paused = true;
        saved.items[0].played = true;
        saved.order = DownloadOrder::Fastest;
        store.save(&saved).unwrap();
        assert_eq!(store.load().queue, saved);
        let json: Value = serde_json::from_str(&fs::read_to_string(store.path()).unwrap()).unwrap();
        assert_eq!(json["order"], "fastest");
        assert_eq!(json["items"][1]["paused"], true);
        assert_eq!(json["items"][0]["played"], true);

        // Files from before these existed: nothing paused, in queue order.
        let mut old = json;
        old["items"][1].as_object_mut().unwrap().remove("paused");
        old["items"][0].as_object_mut().unwrap().remove("played");
        old.as_object_mut().unwrap().remove("order");
        fs::write(store.path(), old.to_string()).unwrap();
        let loaded = store.load().queue;
        assert!(!loaded.items[1].paused);
        assert!(!loaded.items[0].played);
        assert_eq!(loaded.order, DownloadOrder::InOrder);
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

    #[test]
    fn damaged_files_are_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        let store = QueueStore::new(dir.path());
        fs::write(store.path(), "{ not json").unwrap();
        let loaded = store.load();
        assert_eq!(loaded.queue, Queue::default());
        assert_eq!(loaded.warnings.len(), 1);
        assert!(!loaded.warnings[0].contains(&dir.path().display().to_string()));
        assert!(dir.path().join(QUEUE_INVALID_FILE_NAME).exists());
        assert!(!store.path().exists());
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
    }
}
