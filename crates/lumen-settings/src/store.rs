// SPDX-License-Identifier: MIT OR Apache-2.0

//! Loading and saving `settings.json`.
//!
//! Loading never fails: a missing file gives the defaults, and a damaged one is set aside as
//! `settings.invalid.json` before falling back to the defaults. Saving is atomic (write a
//! temporary file, flush it, rename it over the old one), so a crash cannot leave a torn file.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::model::{SCHEMA_VERSION, Settings};

/// The settings file's name inside the config folder.
pub const FILE_NAME: &str = "settings.json";
/// A damaged settings file is renamed to this so the user can recover it.
pub const INVALID_FILE_NAME: &str = "settings.invalid.json";
/// A file written by a newer TorrentStream is copied to this, since saving drops fields it does not know.
pub const NEWER_FILE_NAME: &str = "settings.newer.json";
/// Larger files are not read.
const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// The result of [`SettingsStore::load`].
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    /// The settings to use.
    pub settings: Settings,
    /// Problems found while loading, for the log and the Settings page. Never contains paths.
    pub warnings: Vec<String>,
}

/// Why settings could not be saved.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Writing the file failed.
    #[error("could not write the settings file: {0}")]
    Io(#[from] io::Error),
    /// The settings could not be encoded (should not happen).
    #[error("could not encode the settings: {0}")]
    Encode(#[from] serde_json::Error),
}

/// Reads and writes `settings.json` in one folder (the app config folder).
#[derive(Debug, Clone)]
pub struct SettingsStore {
    dir: PathBuf,
}

impl SettingsStore {
    /// A store for `dir/settings.json`.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The folder holding the settings file.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The settings file's path.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.dir.join(FILE_NAME)
    }

    /// Loads the settings, falling back to defaults (with a warning) on any problem.
    #[must_use]
    pub fn load(&self) -> Loaded {
        let mut warnings = Vec::new();
        let text = match read_limited(&self.path()) {
            Ok(Some(text)) => text,
            Ok(None) => {
                return Loaded {
                    settings: Settings::default(),
                    warnings,
                };
            }
            Err(error) => {
                tracing::warn!(%error, "could not read settings; using defaults");
                warnings.push("Settings could not be read, so defaults are in use.".to_owned());
                return Loaded {
                    settings: Settings::default(),
                    warnings,
                };
            }
        };
        let Ok(Value::Object(value)) = serde_json::from_str::<Value>(&text) else {
            self.set_aside();
            warnings.push(format!(
                "{FILE_NAME} was damaged, so defaults are in use. The old file was kept as \
                 {INVALID_FILE_NAME}."
            ));
            return Loaded {
                settings: Settings::default(),
                warnings,
            };
        };
        if version_of(&value) > SCHEMA_VERSION {
            self.back_up_newer(&text);
            warnings.push(format!(
                "{FILE_NAME} was written by a newer version of TorrentStream; settings this version does \
                 not know about were kept in {NEWER_FILE_NAME}."
            ));
        }
        let value = migrate(value);
        let mut settings = from_sections(value, &mut warnings);
        warnings.extend(settings.sanitize());
        for warning in &warnings {
            tracing::warn!(%warning, "settings loaded with a problem");
        }
        Loaded { settings, warnings }
    }

    /// Saves the settings atomically, readable only by the current user where the OS supports
    /// it (Unix permissions 0600; on Windows the per-user config folder is already private).
    ///
    /// # Errors
    ///
    /// Returns an error if the folder or file cannot be written.
    pub fn save(&self, settings: &Settings) -> Result<(), StoreError> {
        let mut json = serde_json::to_vec_pretty(settings)?;
        json.push(b'\n');
        create_private_dir(&self.dir)?;
        let tmp = self.dir.join(format!("{FILE_NAME}.tmp"));
        let result = write_private(&tmp, &json).and_then(|()| fs::rename(&tmp, self.path()));
        if result.is_err() {
            // Best effort: a stale temporary file is harmless and replaced on the next save.
            let _ = fs::remove_file(&tmp);
        }
        Ok(result?)
    }

    fn set_aside(&self) {
        let target = self.dir.join(INVALID_FILE_NAME);
        if let Err(error) = fs::rename(self.path(), target) {
            tracing::warn!(%error, "could not set the damaged settings file aside");
        }
    }

    /// Keeps a copy of a file written by a newer TorrentStream, since saving would drop its new fields.
    fn back_up_newer(&self, text: &str) {
        let target = self.dir.join(NEWER_FILE_NAME);
        if let Err(error) = write_private(&target, text.as_bytes()) {
            tracing::warn!(%error, "could not back up settings from a newer version");
        }
    }
}

/// Upgrades a settings document to [`SCHEMA_VERSION`]. Add one step per version bump.
///
/// A document without a version is treated as version 1. A document from a newer version is
/// read as well as possible: unknown fields are ignored and known fields are kept.
#[must_use]
pub fn migrate(mut doc: Map<String, Value>) -> Map<String, Value> {
    let version = version_of(&doc);
    if version < SCHEMA_VERSION {
        // Each schema change adds a step here.
        if version < 2 {
            // `network.killSwitch` was removed: every protected mode always fails closed, so
            // the setting never had an effect. Dropping it keeps the file honest.
            if let Some(Value::Object(network)) = doc.get_mut("network") {
                network.remove("killSwitch");
            }
        }
        if version < 3
            && let Some(Value::Object(bandwidth)) = doc.get_mut("bandwidth")
        {
            // Older files predate the optional time target: keep their ratio-only behavior.
            bandwidth
                .entry("seedTimeLimitMinutes")
                .or_insert_with(|| Value::from(0));
        }
        tracing::info!(from = version, to = SCHEMA_VERSION, "migrating settings");
    }
    doc.insert("version".to_owned(), Value::from(SCHEMA_VERSION));
    doc
}

/// The document's schema version; documents without one are version 1.
fn version_of(doc: &Map<String, Value>) -> u32 {
    doc.get("version")
        .and_then(Value::as_u64)
        .map_or(1, |v| u32::try_from(v).unwrap_or(u32::MAX))
}

/// Deserializes section by section, so one bad section resets only itself.
fn from_sections(doc: Map<String, Value>, warnings: &mut Vec<String>) -> Settings {
    let mut settings = Settings::default();
    for (name, value) in doc {
        let ok = match name.as_str() {
            "playback" => section(value, &mut settings.playback),
            "storage" => section(value, &mut settings.storage),
            "bandwidth" => section(value, &mut settings.bandwidth),
            "search" => section(value, &mut settings.search),
            "network" => section(value, &mut settings.network),
            "advanced" => section(value, &mut settings.advanced),
            // `version` was handled by `migrate`; unknown sections (from a newer version) are
            // ignored.
            _ => true,
        };
        if !ok {
            warnings.push(format!(
                "The {name} settings were invalid and were reset to defaults."
            ));
        }
    }
    settings
}

fn section<T: DeserializeOwned>(value: Value, slot: &mut T) -> bool {
    match serde_json::from_value(value) {
        Ok(parsed) => {
            *slot = parsed;
            true
        }
        Err(_) => false,
    }
}

/// Reads a file of at most [`MAX_FILE_BYTES`]; `Ok(None)` if it does not exist.
fn read_limited(path: &Path) -> io::Result<Option<String>> {
    match fs::metadata(path) {
        Ok(meta) if meta.len() > MAX_FILE_BYTES => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "settings file is too large",
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
    use crate::model::NetworkMode;

    fn store() -> (tempfile::TempDir, SettingsStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path().join("config"));
        (dir, store)
    }

    #[test]
    fn missing_file_gives_defaults_without_warnings() {
        let (_dir, store) = store();
        assert_eq!(
            store.load(),
            Loaded {
                settings: Settings::default(),
                warnings: Vec::new()
            }
        );
    }

    #[test]
    fn save_and_load_round_trip() {
        let (_dir, store) = store();
        let mut settings = Settings::default();
        settings.bandwidth.upload_limit_kib = 256;
        settings.network.mode = NetworkMode::Socks5;
        settings.network.socks5.host = "10.64.0.1".into();
        store.save(&settings).unwrap();
        store.save(&settings).unwrap();
        let loaded = store.load();
        assert_eq!(loaded.settings, settings);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        let names: Vec<_> = fs::read_dir(store.dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, [FILE_NAME], "no temporary file left behind");
    }

    #[cfg(unix)]
    #[test]
    fn file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, store) = store();
        store.save(&Settings::default()).unwrap();
        let mode = fs::metadata(store.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn damaged_file_is_set_aside() {
        let (_dir, store) = store();
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(store.path(), "{ not json").unwrap();
        let loaded = store.load();
        assert_eq!(loaded.settings, Settings::default());
        assert_eq!(loaded.warnings.len(), 1);
        assert!(!loaded.warnings[0].contains(&*store.dir().to_string_lossy()));
        assert!(store.dir().join(INVALID_FILE_NAME).exists());
        assert!(!store.path().exists());
    }

    #[test]
    fn bad_section_resets_only_that_section() {
        let (_dir, store) = store();
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(
            store.path(),
            r#"{"version":1,"bandwidth":{"peerLimit":"lots"},"search":{"hideAdult":false},
               "playback":{"defaultSpeed":42}}"#,
        )
        .unwrap();
        let loaded = store.load();
        assert_eq!(loaded.settings.bandwidth, Settings::default().bandwidth);
        assert!(
            !loaded.settings.search.hide_adult,
            "valid sections are kept"
        );
        assert!((loaded.settings.playback.default_speed - 1.0).abs() < f64::EPSILON);
        assert_eq!(loaded.warnings.len(), 2, "{:?}", loaded.warnings);
    }

    #[test]
    fn unversioned_files_migrate_and_newer_ones_are_backed_up() {
        let (_dir, store) = store();
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(store.path(), r#"{"search":{"resultsPerSearch":50}}"#).unwrap();
        let loaded = store.load();
        assert_eq!(loaded.settings.version, SCHEMA_VERSION);
        assert_eq!(loaded.settings.search.results_per_search, 50);
        assert!(loaded.warnings.is_empty());

        fs::write(
            store.path(),
            r#"{"version":99,"future":{"x":1},"network":{"mode":"socks5","newField":true}}"#,
        )
        .unwrap();
        let loaded = store.load();
        assert_eq!(loaded.settings.network.mode, NetworkMode::Socks5);
        assert_eq!(loaded.settings.version, SCHEMA_VERSION);
        assert_eq!(loaded.warnings.len(), 1);
        assert!(store.dir().join(NEWER_FILE_NAME).exists());
    }

    #[test]
    fn version_1_files_lose_the_kill_switch_setting() {
        let (_dir, store) = store();
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(
            store.path(),
            r#"{"version":1,"network":{"mode":"socks5","killSwitch":false,
               "socks5":{"host":"10.0.0.1"}}}"#,
        )
        .unwrap();
        let loaded = store.load();
        assert_eq!(loaded.settings.network.mode, NetworkMode::Socks5);
        assert_eq!(loaded.settings.network.socks5.host, "10.0.0.1");
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        store.save(&loaded.settings).unwrap();
        let saved = fs::read_to_string(store.path()).unwrap();
        assert!(!saved.contains("killSwitch"));
        assert!(saved.contains(&format!("\"version\": {SCHEMA_VERSION}")));
    }

    #[test]
    fn version_2_files_keep_ratio_only_seeding() {
        let (_dir, store) = store();
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(
            store.path(),
            r#"{"version":2,"bandwidth":{"seedAfterWatching":true,"seedRatioTarget":3.5}}"#,
        )
        .unwrap();
        let loaded = store.load();
        assert!((loaded.settings.bandwidth.seed_ratio_target - 3.5).abs() < f64::EPSILON);
        assert_eq!(loaded.settings.bandwidth.seed_time_limit_minutes, 0);
        assert_eq!(loaded.settings.version, SCHEMA_VERSION);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    }

    #[test]
    fn oversized_file_is_not_read() {
        let (_dir, store) = store();
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(store.path(), vec![b' '; 2 * 1024 * 1024]).unwrap();
        let loaded = store.load();
        assert_eq!(loaded.settings, Settings::default());
        assert_eq!(loaded.warnings.len(), 1);
    }
}
