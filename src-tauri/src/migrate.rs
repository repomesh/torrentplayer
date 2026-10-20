// SPDX-License-Identifier: MIT OR Apache-2.0

//! Carries the app's folders over from the identifier used before the rename to TorrentStream
//! (`io.github.lumen-app.lumen`), so settings, the queue, and the download cache survive it.
//!
//! Runs at start-up, before anything reads the folders. A folder is moved whole when the new
//! one does not exist yet (a rename on the same drive: instant even for a large cache);
//! otherwise each entry the new folder lacks is moved (the webview may already have created its
//! own data there, which is kept). Entries in use, for example by an old copy of the app still
//! running, stay where they are and are tried again at the next start. Secrets are unaffected:
//! the credential store keeps its service name (`lumen_settings::KEYRING_SERVICE`).

use std::fs;
use std::path::{Path, PathBuf};

/// The identifier the folders were named after before the rename.
pub(crate) const OLD_IDENTIFIER: &str = "io.github.lumen-app.lumen";

/// Moves the old-identifier sibling of each of `folders` into it. Returns how many entries
/// moved.
pub(crate) fn from_old_identifier(folders: &[&Path]) -> usize {
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut moved = 0;
    for new in folders {
        if seen.iter().any(|p| p == new) {
            continue;
        }
        seen.push(new.to_path_buf());
        let old = new.with_file_name(OLD_IDENTIFIER);
        if old != *new && old.is_dir() {
            moved += migrate(&old, new);
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
        let old = root.path().join(OLD_IDENTIFIER);
        fs::create_dir_all(old.join("downloads/lumen-swarm")).unwrap();
        fs::write(old.join("settings.json"), "{}").unwrap();
        let new = root.path().join("io.torrentstream.desktop");
        assert_eq!(from_old_identifier(&[&new, &new]), 1);
        assert!(new.join("settings.json").is_file());
        assert!(new.join("downloads/lumen-swarm").is_dir());
        assert!(!old.exists());
        // Nothing left to move the next time.
        assert_eq!(from_old_identifier(&[&new]), 0);
    }

    #[test]
    fn merges_into_an_existing_folder_without_overwriting() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join(OLD_IDENTIFIER);
        let new = root.path().join("io.torrentstream.desktop");
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
