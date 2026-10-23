// SPDX-License-Identifier: MIT OR Apache-2.0

//! Safe housekeeping for downloaded torrent data.
//!
//! New cache entries live in a directory named by their info hash. Playback writes a marker
//! when it reaches the end; only marked, unprotected hash directories are ever selected for
//! automatic removal. Older layouts and unrecognized files still count toward the configured
//! limit, but are left alone because their torrent cannot be identified safely.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use torrentplayer_library::Queue;
use torrentplayer_torrent::{BackendKind, TorrentSource, magnet_info_hash};

/// TorrentPlayer's cache folders inside the download folder.
pub(crate) const CACHE_DIRS: [&str; 2] = ["torrentplayer-swarm", "librqbit"];
/// A timestamp written after playback reaches the end of a file.
const FINISHED_MARKER: &str = ".torrentplayer-finished";
/// Cache usage stops counting after this many entries and reports a lower bound.
const MAX_WALK_ENTRIES: usize = 200_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Usage {
    pub(crate) bytes: u64,
    pub(crate) files: u64,
    pub(crate) complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub(crate) info_hash: String,
    pub(crate) bytes: u64,
    finished_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    pub(crate) usage: Usage,
    pub(crate) candidates: Vec<Candidate>,
    pub(crate) reclaimable_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CleanupMode {
    /// Apply the configured size and age policies.
    Policy {
        limit_bytes: Option<u64>,
        max_age: Option<Duration>,
    },
    /// Remove every finished download that is not protected.
    AllFinished,
}

/// Info hashes that belong to the queue, including hashes present only in magnet links.
pub(crate) fn queue_hashes(queue: &Queue) -> HashSet<String> {
    queue.items.iter().filter_map(queue_item_hash).collect()
}

/// Whether every queued item can be tied to a cache directory. Cleanup pauses when this is false
/// rather than risk removing an unidentified queued torrent.
pub(crate) fn queue_hashes_complete(queue: &Queue) -> bool {
    queue
        .items
        .iter()
        .all(|item| queue_item_hash(item).is_some())
}

fn queue_item_hash(item: &torrentplayer_library::QueueItem) -> Option<String> {
    let hash = item.info_hash.clone().or_else(|| {
        let TorrentSource::Magnet(link) = TorrentSource::parse_link(&item.source).ok()? else {
            return None;
        };
        magnet_info_hash(&link)
    })?;
    valid_hash(&hash).then(|| hash.to_ascii_lowercase())
}

/// Records that playback reached the end. Missing and legacy cache directories are not created.
pub(crate) fn mark_finished(root: &Path, backend: BackendKind, info_hash: &str) -> io::Result<()> {
    if !valid_hash(info_hash) {
        return Ok(());
    }
    let Some(cache_root) = safe_cache_root(root, backend.name()) else {
        return Ok(());
    };
    let dir = cache_root.join(info_hash.to_ascii_lowercase());
    match fs::symlink_metadata(&dir) {
        Ok(meta) if meta.file_type().is_dir() => {
            let seconds = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_secs();
            fs::write(dir.join(FINISHED_MARKER), seconds.to_string())
        }
        Ok(_) | Err(_) => Ok(()),
    }
}

/// A torrent being opened or watched is no longer eligible for cleanup.
pub(crate) fn mark_unfinished(root: &Path, info_hash: &str) {
    if !valid_hash(info_hash) {
        return;
    }
    let hash = info_hash.to_ascii_lowercase();
    for name in CACHE_DIRS {
        if let Some(cache_root) = safe_cache_root(root, name) {
            let _ = fs::remove_file(cache_root.join(&hash).join(FINISHED_MARKER));
        }
    }
}

/// Measures all recognized cache folders without following symbolic links.
pub(crate) fn measure(root: &Path) -> Usage {
    let mut budget = MAX_WALK_ENTRIES;
    let mut usage = Usage {
        complete: true,
        ..Usage::default()
    };
    for name in CACHE_DIRS {
        measure_tree(&root.join(name), &mut budget, &mut usage);
        if !usage.complete {
            break;
        }
    }
    usage
}

/// Chooses finished, unprotected downloads for a policy run or an explicit clear.
pub(crate) fn plan(
    root: &Path,
    protected: &HashSet<String>,
    mode: CleanupMode,
    now: SystemTime,
) -> Plan {
    let usage = measure(root);
    let protected: HashSet<String> = protected
        .iter()
        .map(|hash| hash.to_ascii_lowercase())
        .collect();
    let mut by_hash: HashMap<String, (u64, Option<u64>)> = HashMap::new();
    for name in CACHE_DIRS {
        let Some(cache_root) = safe_cache_root(root, name) else {
            continue;
        };
        let Ok(entries) = fs::read_dir(&cache_root) else {
            continue;
        };
        for entry in entries.flatten() {
            let Some(hash) = entry.file_name().to_str().map(str::to_ascii_lowercase) else {
                continue;
            };
            if !valid_hash(&hash) || protected.contains(&hash) {
                continue;
            }
            let path = entry.path();
            if !fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_dir()) {
                continue;
            }
            let entry_usage = measure_entry(&path);
            let candidate = by_hash.entry(hash).or_default();
            candidate.0 = candidate.0.saturating_add(entry_usage.bytes);
            if let Some(finished_at) = finished_timestamp(&path) {
                candidate.1 = Some(candidate.1.unwrap_or(0).max(finished_at));
            }
        }
    }

    let mut eligible: Vec<Candidate> = by_hash
        .into_iter()
        .filter_map(|(info_hash, (bytes, finished_at))| {
            Some(Candidate {
                info_hash,
                bytes,
                finished_at: finished_at?,
            })
        })
        .collect();
    eligible.sort_by(|a, b| {
        (a.finished_at, a.info_hash.as_str()).cmp(&(b.finished_at, b.info_hash.as_str()))
    });
    let reclaimable_bytes = eligible
        .iter()
        .fold(0_u64, |sum, candidate| sum.saturating_add(candidate.bytes));
    let candidates = match mode {
        CleanupMode::AllFinished => eligible,
        CleanupMode::Policy {
            limit_bytes,
            max_age,
        } => {
            let cutoff = max_age
                .and_then(|age| now.checked_sub(age))
                .map(epoch_seconds);
            let mut remaining = usage.bytes;
            let mut selected = Vec::new();
            for candidate in eligible {
                let expired = cutoff.is_some_and(|cutoff| candidate.finished_at <= cutoff);
                let over_limit = limit_bytes.is_some_and(|limit| remaining > limit);
                if expired || over_limit {
                    remaining = remaining.saturating_sub(candidate.bytes);
                    selected.push(candidate);
                }
            }
            selected
        }
    };
    Plan {
        usage,
        candidates,
        reclaimable_bytes,
    }
}

/// Deletes one validated hash directory from each cache root. Symbolic links are left alone.
pub(crate) fn remove(root: &Path, info_hash: &str) -> io::Result<()> {
    if !valid_hash(info_hash) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid cache info hash",
        ));
    }
    let hash = info_hash.to_ascii_lowercase();
    for name in CACHE_DIRS {
        let Some(cache_root) = safe_cache_root(root, name) else {
            continue;
        };
        let path = cache_root.join(&hash);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_dir() => fs::remove_dir_all(path)?,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn safe_cache_root(root: &Path, name: &str) -> Option<PathBuf> {
    let path = root.join(name);
    fs::symlink_metadata(&path)
        .is_ok_and(|meta| meta.file_type().is_dir())
        .then_some(path)
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 40 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn finished_timestamp(dir: &Path) -> Option<u64> {
    let marker = dir.join(FINISHED_MARKER);
    if !fs::symlink_metadata(&marker).is_ok_and(|m| m.file_type().is_file()) {
        return None;
    }
    fs::read_to_string(marker).ok()?.trim().parse().ok()
}

fn epoch_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

fn measure_entry(path: &Path) -> Usage {
    let mut budget = MAX_WALK_ENTRIES;
    let mut usage = Usage {
        complete: true,
        ..Usage::default()
    };
    measure_tree(path, &mut budget, &mut usage);
    usage
}

fn measure_tree(path: &Path, budget: &mut usize, usage: &mut Usage) {
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if !fs::symlink_metadata(&dir).is_ok_and(|meta| meta.file_type().is_dir()) {
            continue;
        }
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if *budget == 0 {
                usage.complete = false;
                return;
            }
            *budget -= 1;
            let Ok(meta) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if meta.file_type().is_dir() {
                stack.push(entry.path());
            } else if meta.file_type().is_file() {
                usage.bytes = usage.bytes.saturating_add(meta.len());
                usage.files = usage.files.saturating_add(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use torrentplayer_library::QueueItem;

    const OLD: &str = "1111111111111111111111111111111111111111";
    const NEW: &str = "2222222222222222222222222222222222222222";

    fn entry(root: &Path, engine: &str, hash: &str, bytes: usize, finished: u64) {
        let dir = root.join(engine).join(hash);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("pieces"), vec![0_u8; bytes]).unwrap();
        fs::write(dir.join(FINISHED_MARKER), finished.to_string()).unwrap();
    }

    #[test]
    fn policy_removes_oldest_until_below_limit() {
        let root = tempfile::tempdir().unwrap();
        entry(root.path(), "torrentplayer-swarm", OLD, 100, 10);
        entry(root.path(), "torrentplayer-swarm", NEW, 100, 20);
        let plan = plan(
            root.path(),
            &HashSet::new(),
            CleanupMode::Policy {
                limit_bytes: Some(150),
                max_age: None,
            },
            UNIX_EPOCH + Duration::from_secs(30),
        );
        assert_eq!(plan.candidates.len(), 1);
        assert_eq!(plan.candidates[0].info_hash, OLD);
    }

    #[test]
    fn queued_finished_download_is_never_selected() {
        let root = tempfile::tempdir().unwrap();
        entry(root.path(), "librqbit", OLD, 100, 10);
        let protected = HashSet::from([OLD.to_owned()]);
        let plan = plan(
            root.path(),
            &protected,
            CleanupMode::AllFinished,
            UNIX_EPOCH,
        );
        assert!(plan.candidates.is_empty());
        assert_eq!(plan.reclaimable_bytes, 0);
    }

    #[test]
    fn playback_marks_and_unmarks_existing_cache_entries() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("torrentplayer-swarm").join(OLD);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("pieces"), [0_u8; 10]).unwrap();
        mark_finished(root.path(), BackendKind::Swarm, OLD).unwrap();
        let finished = plan(
            root.path(),
            &HashSet::new(),
            CleanupMode::AllFinished,
            SystemTime::now(),
        );
        assert_eq!(finished.candidates.len(), 1);
        mark_unfinished(root.path(), OLD);
        assert!(
            plan(
                root.path(),
                &HashSet::new(),
                CleanupMode::AllFinished,
                SystemTime::now(),
            )
            .candidates
            .is_empty()
        );

        mark_finished(root.path(), BackendKind::Rqbit, NEW).unwrap();
        assert!(!root.path().join("librqbit").join(NEW).exists());
    }

    #[test]
    fn one_marker_reclaims_the_same_hash_from_both_engines() {
        let root = tempfile::tempdir().unwrap();
        entry(root.path(), "torrentplayer-swarm", OLD, 100, 10);
        let other = root.path().join("librqbit").join(OLD);
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("video"), [0_u8; 50]).unwrap();
        let plan = plan(
            root.path(),
            &HashSet::new(),
            CleanupMode::AllFinished,
            UNIX_EPOCH,
        );
        assert_eq!(plan.candidates.len(), 1);
        assert_eq!(plan.candidates[0].bytes, 152);
        remove(root.path(), OLD).unwrap();
        assert!(!root.path().join("torrentplayer-swarm").join(OLD).exists());
        assert!(!other.exists());
    }

    #[test]
    fn age_policy_selects_only_expired_downloads() {
        let root = tempfile::tempdir().unwrap();
        entry(root.path(), "torrentplayer-swarm", OLD, 10, 10);
        entry(root.path(), "torrentplayer-swarm", NEW, 10, 90);
        let plan = plan(
            root.path(),
            &HashSet::new(),
            CleanupMode::Policy {
                limit_bytes: None,
                max_age: Some(Duration::from_secs(50)),
            },
            UNIX_EPOCH + Duration::from_secs(100),
        );
        assert_eq!(plan.candidates.len(), 1);
        assert_eq!(plan.candidates[0].info_hash, OLD);
    }

    #[test]
    fn unknown_and_unfinished_data_is_counted_but_not_removed() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("torrentplayer-swarm/legacy")).unwrap();
        fs::write(
            root.path().join("torrentplayer-swarm/legacy/piece"),
            [0_u8; 9],
        )
        .unwrap();
        fs::create_dir_all(root.path().join("mine")).unwrap();
        fs::write(root.path().join("mine/keep"), [0_u8; 7]).unwrap();
        let plan = plan(
            root.path(),
            &HashSet::new(),
            CleanupMode::AllFinished,
            UNIX_EPOCH,
        );
        assert_eq!(plan.usage.bytes, 9);
        assert!(plan.candidates.is_empty());
        assert!(root.path().join("mine/keep").exists());
    }

    #[test]
    fn queue_hashes_reads_explicit_and_magnet_hashes() {
        let item = |id: &str, info_hash: Option<&str>, source: String| QueueItem {
            id: id.into(),
            source,
            info_hash: info_hash.map(str::to_owned),
            file_index: None,
            title: id.into(),
            torrent_name: None,
            size_bytes: None,
            paused: false,
            played: false,
            added_at: None,
        };
        let queue = Queue {
            items: vec![
                item("a", Some(OLD), "https://example.org/a.torrent".into()),
                item("b", None, format!("magnet:?xt=urn:btih:{NEW}")),
            ],
            ..Queue::default()
        };
        assert_eq!(
            queue_hashes(&queue),
            HashSet::from([OLD.into(), NEW.into()])
        );
        assert!(queue_hashes_complete(&queue));
        let unknown = Queue {
            items: vec![item(
                "c",
                None,
                "https://example.org/unknown.torrent".into(),
            )],
            ..Queue::default()
        };
        assert!(!queue_hashes_complete(&unknown));
    }

    #[test]
    fn the_thumbnail_engines_folder_is_measured_but_never_a_candidate() {
        let root = tempfile::tempdir().unwrap();
        // The thumbnail engine keeps `<hash>` caches in `torrentplayer-swarm/thumbnails`.
        let thumbnails = root.path().join("torrentplayer-swarm/thumbnails");
        entry(&thumbnails, ".", OLD, 10, 1);
        let plan = plan(
            root.path(),
            &HashSet::new(),
            CleanupMode::AllFinished,
            UNIX_EPOCH,
        );
        assert!(plan.candidates.is_empty());
        assert_eq!(plan.usage.bytes, 11);
        remove(root.path(), OLD).unwrap();
        assert!(thumbnails.join(OLD).exists());
    }

    #[test]
    fn removal_is_limited_to_valid_hash_directories() {
        let root = tempfile::tempdir().unwrap();
        entry(root.path(), "torrentplayer-swarm", OLD, 10, 1);
        fs::create_dir_all(root.path().join("mine")).unwrap();
        fs::write(root.path().join("mine/keep"), b"keep").unwrap();
        remove(root.path(), OLD).unwrap();
        assert!(!root.path().join("torrentplayer-swarm").join(OLD).exists());
        assert!(root.path().join("mine/keep").exists());
        assert!(remove(root.path(), "../mine").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn cache_root_symlinks_are_neither_measured_nor_removed() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        entry(outside.path(), "torrentplayer-swarm", OLD, 10, 1);
        std::os::unix::fs::symlink(
            outside.path().join("torrentplayer-swarm"),
            root.path().join("torrentplayer-swarm"),
        )
        .unwrap();
        assert_eq!(measure(root.path()).bytes, 0);
        remove(root.path(), OLD).unwrap();
        assert!(
            outside
                .path()
                .join("torrentplayer-swarm")
                .join(OLD)
                .exists()
        );
    }
}
