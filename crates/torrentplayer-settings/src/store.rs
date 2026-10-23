// SPDX-License-Identifier: MIT OR Apache-2.0

//! Loading and saving `settings.json`.
//!
//! Loading never fails. A missing file (the first run) gives the defaults. Any other problem
//! keeps the user's choice of connection from silently becoming a direct connection:
//!
//! - A file that exists but cannot be read (too large, locked by another program, an I/O error)
//!   or is not a JSON object gives the defaults with traffic held
//!   ([`NetworkHold::SettingsUnreadable`]): nothing connects until the user confirms a mode.
//! - A network section that is not an object, or whose connection mode is unknown (a typo, or a
//!   mode from a newer version), holds traffic the same way. Every other network value is read on
//!   its own, so one bad value (say an unknown IPv6 policy) resets only itself.
//!
//! In both cases the original file is copied to `settings.invalid.json` and is left in place:
//! nothing is written while loading, so the next start reads it again. If the copy cannot be
//! made, [`SettingsStore::save`] refuses to replace the file until a copy succeeds, so the user's
//! real settings are never overwritten with defaults.
//!
//! Saving is atomic (write a temporary file, flush it, rename it over the old one), so a crash
//! cannot leave a torn file.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::model::{NetworkHold, NetworkSettings, SCHEMA_VERSION, Settings};

/// The settings file's name inside the config folder.
pub const FILE_NAME: &str = "settings.json";
/// A copy of a settings file that could not be read or used, kept so the user can recover it.
pub const INVALID_FILE_NAME: &str = "settings.invalid.json";
/// A file written by a newer TorrentPlayer is copied to this, since saving drops fields it does not know.
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
    /// The existing file could not be read when TorrentPlayer started and still cannot be copied
    /// to `settings.invalid.json`, so it is not replaced.
    #[error(
        "the existing settings file could not be read or backed up, so it was not replaced \
         (close TorrentPlayer and check the file): {0}"
    )]
    Unread(io::Error),
}

/// Reads and writes `settings.json` in one folder (the app config folder).
#[derive(Debug, Clone)]
pub struct SettingsStore {
    dir: PathBuf,
    /// Set by [`Self::load`] when the file could not be used and could not be copied aside:
    /// [`Self::save`] copies it first, and refuses to save while it cannot. Shared by clones.
    unsaved_original: Arc<AtomicBool>,
}

impl SettingsStore {
    /// A store for `dir/settings.json`.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            unsaved_original: Arc::new(AtomicBool::new(false)),
        }
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

    /// Loads the settings, falling back to defaults (with a warning) on any problem, and
    /// holding traffic when the connection mode the user chose is unknown (see the module docs).
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
                tracing::warn!(%error, "could not read settings; holding traffic");
                let kept = self.keep_original(None);
                warnings.push(unusable_warning(
                    &format!("{FILE_NAME} could not be read"),
                    kept,
                ));
                return held_defaults(warnings);
            }
        };
        let Ok(Value::Object(value)) = serde_json::from_str::<Value>(&text) else {
            let kept = self.keep_original(Some(&text));
            warnings.push(unusable_warning(&format!("{FILE_NAME} was damaged"), kept));
            for warning in &warnings {
                tracing::warn!(%warning, "settings loaded with a problem");
            }
            return held_defaults(warnings);
        };
        if version_of(&value) > SCHEMA_VERSION {
            self.back_up_newer(&text);
            warnings.push(format!(
                "{FILE_NAME} was written by a newer version of TorrentPlayer; settings this version does \
                 not know about were kept in {NEWER_FILE_NAME}."
            ));
        }
        let retired = uses_vpn_adapter_mode(&value);
        let value = migrate(value);
        let mut network_unknown = false;
        let mut settings = from_sections(value, &mut warnings, &mut network_unknown);
        warnings.extend(settings.sanitize());
        if network_unknown {
            settings.network.hold = Some(NetworkHold::SettingsUnreadable);
            let kept = self.keep_original(Some(&text));
            warnings.push(unusable_warning(
                "The connection mode in the network settings was not recognized",
                kept,
            ));
        } else if retired {
            // Saved at once, so the file no longer names the removed mode and the notice stays
            // pending even if TorrentPlayer closes before the user sees it.
            if let Err(error) = self.save(&settings) {
                tracing::warn!(%error, "could not save the migrated settings");
            }
        }
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
    /// Returns an error if the folder or file cannot be written, or [`StoreError::Unread`] if
    /// the file could not be read when loading and still cannot be copied aside.
    pub fn save(&self, settings: &Settings) -> Result<(), StoreError> {
        if self.unsaved_original.load(Ordering::Acquire) {
            self.copy_original().map_err(StoreError::Unread)?;
            self.unsaved_original.store(false, Ordering::Release);
        }
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

    /// Copies a file that could not be used to [`INVALID_FILE_NAME`], from `text` if it was
    /// read. Returns whether the copy was made; if not, saving is refused until it can be.
    fn keep_original(&self, text: Option<&str>) -> bool {
        let result = match text {
            Some(text) => write_private(&self.dir.join(INVALID_FILE_NAME), text.as_bytes()),
            None => self.copy_original(),
        };
        match result {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(%error, "could not copy the unusable settings file aside");
                self.unsaved_original.store(true, Ordering::Release);
                false
            }
        }
    }

    /// Copies `settings.json` to [`INVALID_FILE_NAME`]; a file that is gone needs no copy.
    fn copy_original(&self) -> io::Result<()> {
        match fs::copy(self.path(), self.dir.join(INVALID_FILE_NAME)) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound && !self.path().exists() => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Keeps a copy of a file written by a newer TorrentPlayer, since saving would drop its new fields.
    fn back_up_newer(&self, text: &str) {
        let target = self.dir.join(NEWER_FILE_NAME);
        if let Err(error) = write_private(&target, text.as_bytes()) {
            tracing::warn!(%error, "could not back up settings from a newer version");
        }
    }
}

/// The defaults, with traffic held until the user confirms a connection mode.
fn held_defaults(warnings: Vec<String>) -> Loaded {
    let mut settings = Settings::default();
    settings.network.hold = Some(NetworkHold::SettingsUnreadable);
    Loaded { settings, warnings }
}

/// The warning for a settings file whose connection mode could not be used.
fn unusable_warning(problem: &str, kept: bool) -> String {
    let kept = if kept {
        format!("The file was kept as {INVALID_FILE_NAME}.")
    } else {
        format!("{FILE_NAME} is not replaced until it can be read or backed up.")
    };
    format!(
        "{problem}, so TorrentPlayer is not connecting until you choose a connection in \
         Settings → Network & VPN. {kept}"
    )
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
        // Version 3 added `bandwidth.seedTimeLimitMinutes`; it and the other seeding targets
        // were removed in 0.3.1 without a step: serde ignores fields it does not know, so they
        // are dropped on the next save.
        if version < 4
            && let Some(Value::Object(network)) = doc.get_mut("network")
        {
            // `network.interface` belonged to the VPN adapter mode, removed in 0.3.0.
            network.remove("interface");
        }
        if version < 5
            && let Some(Value::Object(network)) = doc.get_mut("network")
        {
            // `network.vpnAdapterModeRemoved` became one reason of `network.hold`.
            if network.remove("vpnAdapterModeRemoved") == Some(Value::Bool(true)) {
                network.insert("hold".to_owned(), Value::from("vpnAdapterModeRemoved"));
            }
        }
        tracing::info!(from = version, to = SCHEMA_VERSION, "migrating settings");
    }
    // Not tied to a version, so a hand-edited file or a test harness that still selects the
    // removed mode loads too: never as an unknown mode, which would reset every network setting.
    if let Some(Value::Object(network)) = doc.get_mut("network") {
        retire_vpn_adapter_mode(network);
    }
    doc.insert("version".to_owned(), Value::from(SCHEMA_VERSION));
    doc
}

/// Moves settings that used the VPN adapter mode (`"bindInterface"`, removed in 0.3.0) to
/// Direct, and records it in `hold` ([`NetworkHold::VpnAdapterModeRemoved`]): until the user
/// acknowledges the notice, the app holds all traffic rather than silently connecting without the
/// VPN the user chose.
fn retire_vpn_adapter_mode(network: &mut Map<String, Value>) {
    if !is_vpn_adapter_mode(network) {
        return;
    }
    tracing::warn!("the VPN adapter mode was removed; switching to Direct until acknowledged");
    network.insert("mode".to_owned(), Value::from("direct"));
    network.insert("hold".to_owned(), Value::from("vpnAdapterModeRemoved"));
    network.remove("interface");
}

fn is_vpn_adapter_mode(network: &Map<String, Value>) -> bool {
    network.get("mode").and_then(Value::as_str) == Some("bindInterface")
}

/// Whether the document selects the removed VPN adapter mode.
fn uses_vpn_adapter_mode(doc: &Map<String, Value>) -> bool {
    matches!(doc.get("network"), Some(Value::Object(network)) if is_vpn_adapter_mode(network))
}

/// The document's schema version; documents without one are version 1.
fn version_of(doc: &Map<String, Value>) -> u32 {
    doc.get("version")
        .and_then(Value::as_u64)
        .map_or(1, |v| u32::try_from(v).unwrap_or(u32::MAX))
}

/// Deserializes section by section, so one bad section resets only itself. The network section
/// is read value by value (see [`network_section`]); `network_unknown` is set when its
/// connection mode cannot be known.
fn from_sections(
    doc: Map<String, Value>,
    warnings: &mut Vec<String>,
    network_unknown: &mut bool,
) -> Settings {
    let mut settings = Settings::default();
    for (name, value) in doc {
        let ok = match name.as_str() {
            "playback" => section(value, &mut settings.playback),
            "storage" => section(value, &mut settings.storage),
            "bandwidth" => section(value, &mut settings.bandwidth),
            "search" => section(value, &mut settings.search),
            "network" => {
                *network_unknown = !network_section(value, &mut settings.network, warnings);
                true
            }
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

/// Reads the network section value by value, so one bad value resets only itself. Returns
/// `false` if the connection mode cannot be known: the section is not an object, or `mode` or
/// `hold` is invalid (an unknown hold reason, say from a newer version, keeps holding). A missing
/// `mode` is Direct, as in every file from before modes existed.
fn network_section(
    value: Value,
    network: &mut NetworkSettings,
    warnings: &mut Vec<String>,
) -> bool {
    let Value::Object(fields) = value else {
        return false;
    };
    let mut known = true;
    for (name, value) in fields {
        let ok = match name.as_str() {
            // Reported by the caller, with the hold.
            "mode" => {
                known &= section(value, &mut network.mode);
                true
            }
            "hold" => {
                known &= section(value, &mut network.hold);
                true
            }
            "socks5" => section(value, &mut network.socks5),
            "wireguard" => section(value, &mut network.wireguard),
            "encryption" => section(value, &mut network.encryption),
            "ipv6" => section(value, &mut network.ipv6),
            "showPublicIp" => section(value, &mut network.show_public_ip),
            "incomingPeers" => section(value, &mut network.incoming_peers),
            "vpnHintDismissed" => section(value, &mut network.vpn_hint_dismissed),
            // Unknown values (from a newer version) are ignored.
            _ => true,
        };
        if !ok {
            warnings.push(format!(
                "network.{name} was invalid and was reset to its default."
            ));
        }
    }
    known
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
    use crate::model::{EncryptionMode, Ipv6Policy, NetworkMode};

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

    fn held() -> Settings {
        let mut settings = Settings::default();
        settings.network.hold = Some(NetworkHold::SettingsUnreadable);
        settings
    }

    #[test]
    fn damaged_file_holds_traffic_and_is_set_aside() {
        let (_dir, store) = store();
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(store.path(), "{ not json").unwrap();
        let loaded = store.load();
        assert_eq!(loaded.settings, held(), "never Direct without a hold");
        assert_eq!(loaded.warnings.len(), 1);
        assert!(!loaded.warnings[0].contains(&*store.dir().to_string_lossy()));
        assert!(loaded.warnings[0].contains("not connecting"));
        assert_eq!(
            fs::read_to_string(store.dir().join(INVALID_FILE_NAME)).unwrap(),
            "{ not json"
        );
        assert_eq!(
            fs::read_to_string(store.path()).unwrap(),
            "{ not json",
            "nothing is written while loading, so the next start holds again"
        );
        assert_eq!(store.load().settings, held());

        // Confirming a mode saves normally; the copy stays.
        let mut confirmed = loaded.settings;
        confirmed.network.hold = None;
        store.save(&confirmed).unwrap();
        assert_eq!(store.load().settings, Settings::default());
        assert!(store.dir().join(INVALID_FILE_NAME).exists());
    }

    #[test]
    fn a_json_value_that_is_not_an_object_holds_traffic() {
        let (_dir, store) = store();
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(store.path(), "[1, 2]").unwrap();
        assert_eq!(store.load().settings, held());
        assert!(store.dir().join(INVALID_FILE_NAME).exists());
    }

    #[test]
    fn an_unreadable_file_holds_traffic_and_is_never_overwritten() {
        let (_dir, store) = store();
        // A folder in the file's place cannot be read or copied, like a file locked by another
        // program.
        fs::create_dir_all(store.path()).unwrap();
        fs::write(store.path().join("marker"), "the user's settings").unwrap();
        let loaded = store.load();
        assert_eq!(loaded.settings, held());
        assert_eq!(loaded.warnings.len(), 1, "{:?}", loaded.warnings);
        assert!(
            loaded.warnings[0].contains("not replaced"),
            "{:?}",
            loaded.warnings
        );

        let mut confirmed = loaded.settings;
        confirmed.network.hold = None;
        assert!(matches!(store.save(&confirmed), Err(StoreError::Unread(_))));
        assert!(
            store.path().join("marker").exists(),
            "the original is untouched"
        );
        // A clone (the app saves from one) is refused too.
        assert!(store.clone().save(&confirmed).is_err());

        // Once the problem is gone, saving works.
        fs::remove_dir_all(store.path()).unwrap();
        store.save(&confirmed).unwrap();
        assert_eq!(store.load().settings, Settings::default());
    }

    #[test]
    fn an_unreadable_file_that_can_be_copied_is_kept_before_saving() {
        let (_dir, store) = store();
        fs::create_dir_all(store.dir()).unwrap();
        // Too large to read, but still copied aside.
        let mut big = br#"{"network":{"mode":"socks5"}}"#.to_vec();
        big.resize(2 * 1024 * 1024, b' ');
        fs::write(store.path(), &big).unwrap();
        let loaded = store.load();
        assert_eq!(loaded.settings, held());
        assert!(loaded.warnings[0].contains(INVALID_FILE_NAME));
        assert_eq!(fs::read(store.path()).unwrap(), big, "original preserved");
        assert_eq!(fs::read(store.dir().join(INVALID_FILE_NAME)).unwrap(), big);
        store.save(&loaded.settings).unwrap();
        assert_eq!(fs::read(store.dir().join(INVALID_FILE_NAME)).unwrap(), big);
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
    fn removed_seeding_and_autoplay_settings_load_and_are_dropped() {
        let (_dir, store) = store();
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(
            store.path(),
            r#"{"version":5,"playback":{"autoplayNext":false,"defaultSpeed":1.5},
               "bandwidth":{"seedAfterWatching":true,"seedRatioTarget":3.5,
               "seedTimeLimitMinutes":30,"uploadLimitKib":100}}"#,
        )
        .unwrap();
        let loaded = store.load();
        assert!((loaded.settings.playback.default_speed - 1.5).abs() < f64::EPSILON);
        assert_eq!(loaded.settings.bandwidth.upload_limit_kib, 100);
        assert_eq!(loaded.settings.version, SCHEMA_VERSION);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        store.save(&loaded.settings).unwrap();
        let saved = fs::read_to_string(store.path()).unwrap();
        for removed in [
            "autoplayNext",
            "seedAfterWatching",
            "seedRatioTarget",
            "seedTimeLimitMinutes",
        ] {
            assert!(!saved.contains(removed), "{removed}");
        }
    }

    #[test]
    fn version_3_files_load_without_incoming_peers() {
        let (_dir, store) = store();
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(
            store.path(),
            r#"{"version":3,"network":{"mode":"wireGuard","showPublicIp":false}}"#,
        )
        .unwrap();
        let loaded = store.load();
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.settings.network.mode, NetworkMode::WireGuard);
        assert!(
            !loaded.settings.network.incoming_peers.enabled,
            "listening stays off until chosen"
        );
        // A hand-edited, out-of-range port falls back to a random one, keeping the rest.
        fs::write(
            store.path(),
            r#"{"version":3,"network":{"incomingPeers":{"enabled":true,"port":80}}}"#,
        )
        .unwrap();
        let loaded = store.load();
        assert!(loaded.settings.network.incoming_peers.enabled);
        assert_eq!(loaded.settings.network.incoming_peers.port, 0);
        assert_eq!(loaded.warnings.len(), 1, "{:?}", loaded.warnings);
    }

    #[test]
    fn the_vpn_adapter_mode_becomes_direct_with_a_notice() {
        let (_dir, store) = store();
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(
            store.path(),
            r#"{"version":3,"network":{"mode":"bindInterface","interface":"Ethernet",
               "socks5":{"host":"10.0.0.1","port":1081},"vpnHintDismissed":true,
               "incomingPeers":{"enabled":true,"port":51413}}}"#,
        )
        .unwrap();
        let loaded = store.load();
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        let on_disk = fs::read_to_string(store.path()).unwrap();
        assert!(
            !on_disk.contains("bindInterface")
                && on_disk.contains("\"hold\": \"vpnAdapterModeRemoved\""),
            "the migration is saved at once: {on_disk}"
        );
        let network = &loaded.settings.network;
        assert_eq!(network.mode, NetworkMode::Direct);
        assert_eq!(
            network.hold,
            Some(NetworkHold::VpnAdapterModeRemoved),
            "the notice is pending"
        );
        // Nothing else in the section is lost.
        assert_eq!(network.socks5.host, "10.0.0.1");
        assert_eq!(network.socks5.port, 1081);
        assert!(network.vpn_hint_dismissed);
        assert!(network.incoming_peers.enabled);
        assert_eq!(network.incoming_peers.port, 51413);

        // Saved and loaded again: still Direct, the notice still pending, the interface gone.
        store.save(&loaded.settings).unwrap();
        let saved = fs::read_to_string(store.path()).unwrap();
        assert!(!saved.contains("bindInterface") && !saved.contains("\"interface\""));
        assert!(saved.contains("\"hold\": \"vpnAdapterModeRemoved\""));
        assert!(saved.contains(&format!("\"version\": {SCHEMA_VERSION}")));
        let again = store.load();
        assert_eq!(again.settings, loaded.settings);
        assert!(again.warnings.is_empty(), "{:?}", again.warnings);

        // Once dismissed, it stays dismissed.
        let mut dismissed = again.settings;
        dismissed.network.hold = None;
        store.save(&dismissed).unwrap();
        assert_eq!(store.load().settings.network.hold, None);
    }

    #[test]
    fn the_vpn_adapter_mode_is_retired_whatever_the_version() {
        for text in [
            // Unversioned (version 1) and incomplete: no interface name.
            r#"{"network":{"mode":"bindInterface"}}"#,
            // A current-version file edited by hand or by a test harness.
            r#"{"version":4,"network":{"mode":"bindInterface","interface":"wg0"}}"#,
        ] {
            let (_dir, store) = store();
            fs::create_dir_all(store.dir()).unwrap();
            fs::write(store.path(), text).unwrap();
            let loaded = store.load();
            assert!(loaded.warnings.is_empty(), "{text}: {:?}", loaded.warnings);
            assert_eq!(loaded.settings.network.mode, NetworkMode::Direct, "{text}");
            assert_eq!(
                loaded.settings.network.hold,
                Some(NetworkHold::VpnAdapterModeRemoved),
                "{text}"
            );
        }
    }

    #[test]
    fn other_modes_are_not_flagged() {
        for mode in ["direct", "socks5", "wireGuard"] {
            let doc =
                serde_json::json!({"version": 3, "network": {"mode": mode, "interface": "wg0"}});
            let Value::Object(doc) = doc else {
                unreachable!()
            };
            let migrated = migrate(doc);
            assert_eq!(migrated["network"]["mode"], mode);
            assert!(migrated["network"].get("hold").is_none());
            assert!(migrated["network"].get("interface").is_none());
        }
    }

    #[test]
    fn version_4_files_move_the_removed_mode_notice_to_the_hold() {
        for (pending, hold) in [
            (true, Some(NetworkHold::VpnAdapterModeRemoved)),
            (false, None),
        ] {
            let (_dir, store) = store();
            fs::create_dir_all(store.dir()).unwrap();
            let text = serde_json::json!({"version": 4, "network": {"mode": "direct",
                "vpnAdapterModeRemoved": pending}});
            fs::write(store.path(), text.to_string()).unwrap();
            let loaded = store.load();
            assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
            assert_eq!(loaded.settings.network.hold, hold);
            store.save(&loaded.settings).unwrap();
            let saved = fs::read_to_string(store.path()).unwrap();
            assert!(!saved.contains("vpnAdapterModeRemoved\":"), "{saved}");
        }
    }

    #[test]
    fn an_unknown_mode_holds_traffic_and_keeps_the_rest() {
        let (_dir, store) = store();
        fs::create_dir_all(store.dir()).unwrap();
        let text = r#"{"version":5,"network":{"mode":"openvpn","socks5":{"host":"10.0.0.1"},
            "vpnHintDismissed":true},"search":{"hideAdult":false}}"#;
        fs::write(store.path(), text).unwrap();
        let loaded = store.load();
        let network = &loaded.settings.network;
        assert_eq!(network.mode, NetworkMode::Direct);
        assert_eq!(network.hold, Some(NetworkHold::SettingsUnreadable));
        assert_eq!(network.socks5.host, "10.0.0.1");
        assert!(network.vpn_hint_dismissed);
        assert!(!loaded.settings.search.hide_adult);
        assert_eq!(loaded.warnings.len(), 1, "{:?}", loaded.warnings);
        assert!(loaded.warnings[0].contains("not connecting"));
        assert_eq!(
            fs::read_to_string(store.dir().join(INVALID_FILE_NAME)).unwrap(),
            text
        );
        assert_eq!(fs::read_to_string(store.path()).unwrap(), text);
    }

    #[test]
    fn an_invalid_network_section_or_hold_holds_traffic() {
        for text in [
            r#"{"network":"socks5"}"#,
            r#"{"network":{"mode":7}}"#,
            r#"{"network":{"mode":"socks5","hold":"someFutureReason"}}"#,
        ] {
            let (_dir, store) = store();
            fs::create_dir_all(store.dir()).unwrap();
            fs::write(store.path(), text).unwrap();
            let loaded = store.load();
            assert_eq!(
                loaded.settings.network.hold,
                Some(NetworkHold::SettingsUnreadable),
                "{text}"
            );
            assert!(store.dir().join(INVALID_FILE_NAME).exists(), "{text}");
        }
    }

    #[test]
    fn one_bad_network_value_resets_only_itself() {
        let (_dir, store) = store();
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(
            store.path(),
            r#"{"version":5,"network":{"mode":"wireGuard","ipv6":"dualStack",
               "showPublicIp":false,"encryption":"maybe"}}"#,
        )
        .unwrap();
        let loaded = store.load();
        let network = &loaded.settings.network;
        assert_eq!(network.mode, NetworkMode::WireGuard, "the mode is kept");
        assert_eq!(network.hold, None);
        assert_eq!(network.ipv6, Ipv6Policy::Disable);
        assert_eq!(network.encryption, EncryptionMode::Prefer);
        assert!(!network.show_public_ip);
        assert_eq!(loaded.warnings.len(), 2, "{:?}", loaded.warnings);
        assert!(!store.dir().join(INVALID_FILE_NAME).exists());
    }

    #[test]
    fn the_network_section_reads_every_value() {
        // Catches a field added to `NetworkSettings` but not to `network_section`.
        let mut network = NetworkSettings {
            mode: NetworkMode::Socks5,
            encryption: EncryptionMode::Require,
            ipv6: Ipv6Policy::Tunnel,
            show_public_ip: false,
            vpn_hint_dismissed: true,
            hold: Some(NetworkHold::VpnAdapterModeRemoved),
            ..NetworkSettings::default()
        };
        network.socks5.host = "10.0.0.1".into();
        network.wireguard.mtu = Some(1280);
        network.incoming_peers.enabled = true;
        let value = serde_json::to_value(&network).unwrap();
        assert_eq!(
            value.as_object().unwrap().len(),
            9,
            "a new network setting needs a line in network_section"
        );
        let mut read = NetworkSettings::default();
        let mut warnings = Vec::new();
        assert!(network_section(value, &mut read, &mut warnings));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(read, network);
    }

    #[test]
    fn oversized_file_is_not_read() {
        let (_dir, store) = store();
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(store.path(), vec![b' '; 2 * 1024 * 1024]).unwrap();
        let loaded = store.load();
        assert_eq!(loaded.settings, held());
        assert_eq!(loaded.warnings.len(), 1);
    }
}
