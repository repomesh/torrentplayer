// SPDX-License-Identifier: MIT OR Apache-2.0

//! Carries the app's folders over from the identifiers used before the rename to TorrentPlayer
//! (`io.torrentstream.desktop`, and `io.github.lumen-app.lumen` before that), so settings, the
//! queue, and the download cache survive it.
//!
//! Runs at start-up, before anything reads the folders. A folder is moved whole when the new
//! one does not exist yet (a rename on the same drive: instant even for a large cache);
//! otherwise each entry the new folder lacks is moved (the webview may already have created its
//! own data there, which is kept). Entries in use, for example by an old copy of the app still
//! running, stay where they are and are tried again at the next start. Secrets move on their own
//! (see `torrentplayer_settings::LEGACY_KEYRING_SERVICE`).

use std::fs;
use std::path::{Path, PathBuf};

/// The identifiers the folders were named after before the renames, newest first.
pub(crate) const OLD_IDENTIFIERS: [&str; 2] =
    ["io.torrentstream.desktop", "io.github.lumen-app.lumen"];

/// The default download folder's name (in the Downloads folder) before the rename.
pub(crate) const OLD_DOWNLOADS_NAME: &str = "TorrentStream";

/// The engine cache folder's name (in the download folder) before the rename.
pub(crate) const OLD_SWARM_DIR: &str = "lumen-swarm";

/// Moves the old-identifier siblings of each of `folders` into it. Returns how many entries
/// moved.
pub(crate) fn from_old_identifier(folders: &[&Path]) -> usize {
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut moved = 0;
    for new in folders {
        if seen.iter().any(|p| p == new) {
            continue;
        }
        seen.push(new.to_path_buf());
        for name in OLD_IDENTIFIERS {
            let old = new.with_file_name(name);
            if old != *new && old.is_dir() {
                moved += migrate(&old, new);
            }
        }
    }
    moved
}

/// Moves `old` to `new` as described in the module documentation. Returns how many entries
/// moved (a whole folder counts as one).
pub(crate) fn migrate(old: &Path, new: &Path) -> usize {
    if !new.exists() {
        if let Some(parent) = new.parent() {
            let _ = fs::create_dir_all(parent);
        }
        return match fs::rename(old, new) {
            Ok(()) => {
                tracing::info!("moved a folder to its new location");
                1
            }
            Err(error) => {
                tracing::warn!(%error, "could not move a folder to its new location");
                0
            }
        };
    }
    let Ok(entries) = fs::read_dir(old) else {
        return 0;
    };
    let mut moved = 0;
    for entry in entries.flatten() {
        let target = new.join(entry.file_name());
        if target.exists() {
            continue;
        }
        match fs::rename(entry.path(), &target) {
            Ok(()) => moved += 1,
            Err(error) => tracing::warn!(%error, "could not move a file to its new folder"),
        }
    }
    // Removed only once empty, so nothing that failed to move is lost.
    let _ = fs::remove_dir(old);
    if moved > 0 {
        tracing::info!(entries = moved, "moved files to the new folder");
    }
    moved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_a_whole_folder_when_the_new_one_is_missing() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join(OLD_IDENTIFIERS[1]);
        fs::create_dir_all(old.join("downloads/torrentplayer-swarm")).unwrap();
        fs::write(old.join("settings.json"), "{}").unwrap();
        let new = root.path().join("io.torrentplayer.desktop");
        assert_eq!(from_old_identifier(&[&new, &new]), 1);
        assert!(new.join("settings.json").is_file());
        assert!(new.join("downloads/torrentplayer-swarm").is_dir());
        assert!(!old.exists());
        // Nothing left to move the next time.
        assert_eq!(from_old_identifier(&[&new]), 0);
    }

    #[test]
    fn moves_both_earlier_identifiers_newest_first() {
        let root = tempfile::tempdir().unwrap();
        for (name, queue) in [(OLD_IDENTIFIERS[0], "newer"), (OLD_IDENTIFIERS[1], "older")] {
            fs::create_dir_all(root.path().join(name)).unwrap();
            fs::write(root.path().join(name).join("queue.json"), queue).unwrap();
        }
        fs::write(
            root.path().join(OLD_IDENTIFIERS[1]).join("settings.json"),
            "{}",
        )
        .unwrap();
        let new = root.path().join("io.torrentplayer.desktop");
        assert_eq!(from_old_identifier(&[&new]), 2);
        assert_eq!(fs::read_to_string(new.join("queue.json")).unwrap(), "newer");
        assert!(new.join("settings.json").is_file());
    }

    #[test]
    fn merges_into_an_existing_folder_without_overwriting() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join(OLD_IDENTIFIERS[1]);
        let new = root.path().join("io.torrentplayer.desktop");
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(new.join("EBWebView")).unwrap();
        fs::write(old.join("queue.json"), "old queue").unwrap();
        fs::create_dir_all(old.join("EBWebView")).unwrap();
        fs::write(old.join("EBWebView/old"), "x").unwrap();
        assert_eq!(migrate(&old, &new), 1);
        assert_eq!(
            fs::read_to_string(new.join("queue.json")).unwrap(),
            "old queue"
        );
        assert!(
            !new.join("EBWebView/old").exists(),
            "the new webview data is kept"
        );
        assert!(
            old.join("EBWebView/old").exists(),
            "what could not move is not lost"
        );
    }
}
