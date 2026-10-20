// SPDX-License-Identifier: MIT OR Apache-2.0

//! Settings commands: read and update `settings.json`, set or clear secrets in the OS credential
//! store, and report or clear the download cache.
//!
//! Every update is validated by `lumen_settings::apply_patch`, saved atomically, and announced
//! with a [`SETTINGS_CHANGED`] event (to the webview) and a [`watch`] channel (to Rust
//! subsystems). Secrets can be set and cleared here but are never sent back to the webview.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use lumen_settings::{
    EngineSetting, KEYRING_SERVICE, KeyringStore, LogLevel, PatchError, SecretError, SecretKey,
    SecretStatus, SecretStore, SecretString, Settings, SettingsStore, WireGuardSettings,
    Zeroize as _, apply_patch,
};
use lumen_torrent::{ENGINE_ENV_VAR, EngineChoice, StreamEngineConfig};
use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter as _, State};
use tokio::sync::watch;
use ts_rs::TS;

use crate::core::AppState;
use crate::error::{ErrorKind, IpcError};

/// Event sent to the webview, with the new [`Settings`], after settings or secrets change.
pub(crate) const SETTINGS_CHANGED: &str = "settings-changed";

/// TorrentStream's cache folders inside the download folder. Clearing the cache deletes these and
/// nothing else.
const CACHE_DIRS: [&str; 2] = ["lumen-swarm", "librqbit"];
/// Marker in the config folder: clear the cache before the engine starts next time. It holds
/// the download folder it applies to.
const CLEAR_PENDING: &str = "clear-cache-pending";
/// Cache usage stops counting after this many entries and reports a lower bound.
const MAX_WALK_ENTRIES: usize = 200_000;

/// Applies a log level to the running subscriber; `None` when `RUST_LOG` overrides it.
pub(crate) type LogApplier = Box<dyn Fn(LogLevel) + Send + Sync>;

/// Managed Tauri state for settings.
pub(crate) struct SettingsState {
    store: SettingsStore,
    /// The latest saved settings; subscribers are told about every change.
    changes: watch::Sender<Settings>,
    /// Serializes updates so concurrent patches cannot overwrite each other.
    updating: tokio::sync::Mutex<()>,
    /// The settings the running subsystems were started with.
    startup: Settings,
    /// Problems found while loading; cleared by the first successful save.
    load_warnings: Mutex<Vec<String>>,
    secrets: Arc<dyn SecretStore>,
    default_download_dir: PathBuf,
    log: Option<LogApplier>,
}

impl SettingsState {
    /// Loads `settings.json` from `config_dir`, with secrets in the OS credential store.
    pub(crate) fn load(
        config_dir: PathBuf,
        default_download_dir: PathBuf,
        log: Option<LogApplier>,
    ) -> Self {
        let secrets = Arc::new(KeyringStore::new(KEYRING_SERVICE));
        Self::new(
            SettingsStore::new(config_dir),
            secrets,
            default_download_dir,
            log,
        )
    }

    fn new(
        store: SettingsStore,
        secrets: Arc<dyn SecretStore>,
        default_download_dir: PathBuf,
        log: Option<LogApplier>,
    ) -> Self {
        let loaded = store.load();
        if let Some(apply) = &log {
            apply(loaded.settings.advanced.log_level);
        }
        Self {
            store,
            changes: watch::Sender::new(loaded.settings.clone()),
            updating: tokio::sync::Mutex::new(()),
            startup: loaded.settings,
            load_warnings: Mutex::new(loaded.warnings),
            secrets,
            default_download_dir,
            log,
        }
    }

    /// The current settings.
    pub(crate) fn current(&self) -> Settings {
        self.changes.borrow().clone()
    }

    /// Notifies on every change (including secret changes, which leave the value unchanged).
    #[allow(dead_code, reason = "for the network and search subsystems")]
    pub(crate) fn subscribe(&self) -> watch::Receiver<Settings> {
        self.changes.subscribe()
    }

    /// The credential store, for Rust subsystems that need a secret (never the webview).
    #[allow(dead_code, reason = "for the network subsystem")]
    pub(crate) fn secrets(&self) -> Arc<dyn SecretStore> {
        Arc::clone(&self.secrets)
    }

    /// The torrent engine configuration for this run: the download folder and engine choice
    /// from the settings, unless `LUMEN_ENGINE` overrides the engine.
    pub(crate) fn engine_config(&self) -> StreamEngineConfig {
        let mut config = StreamEngineConfig::new(self.download_dir(&self.startup));
        if engine_override().is_none() {
            config.choice = match self.startup.advanced.engine {
                EngineSetting::Auto => EngineChoice::Auto,
                EngineSetting::Swarm => EngineChoice::Swarm,
                EngineSetting::Rqbit => EngineChoice::Rqbit,
            };
        }
        (config.download_limit_bps, config.upload_limit_bps) = rate_limits(&self.startup.bandwidth);
        config
    }

    /// Runs a cache clear scheduled by [`clear_cache`] while the engine was running. Call before
    /// the engine starts.
    pub(crate) fn run_startup_maintenance(&self) {
        let marker = self.store.dir().join(CLEAR_PENDING);
        let Ok(target) = fs::read_to_string(&marker) else {
            return;
        };
        let target = PathBuf::from(target.trim());
        // Only folders TorrentStream itself would use; the marker is never trusted to name others.
        let known = [
            self.download_dir(&self.startup),
            self.default_download_dir.clone(),
        ];
        if known.contains(&target) {
            match clear_dirs(&target) {
                Ok(()) => tracing::info!("cleared the download cache"),
                Err(error) => tracing::warn!(%error, "could not clear the download cache"),
            }
        }
        if let Err(error) = fs::remove_file(&marker) {
            tracing::warn!(%error, "could not remove the cache-clear marker");
        }
    }

    fn download_dir(&self, settings: &Settings) -> PathBuf {
        settings
            .storage
            .download_dir
            .as_ref()
            .map_or_else(|| self.default_download_dir.clone(), PathBuf::from)
    }

    fn view(&self) -> SettingsView {
        let settings = self.current();
        let mut warnings = self
            .load_warnings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        warnings.extend(settings.warnings());
        SettingsView {
            restart_required: self.restart_required(&settings),
            warnings,
            default_download_dir: self.default_download_dir.to_string_lossy().into_owned(),
            active_download_dir: self
                .download_dir(&self.startup)
                .to_string_lossy()
                .into_owned(),
            engine_override: engine_override(),
            log_level_override: self.log.is_none(),
            settings,
        }
    }

    /// Settings changed since start-up that only take effect after a restart.
    fn restart_required(&self, now: &Settings) -> Vec<String> {
        let mut fields = Vec::new();
        if self.download_dir(now) != self.download_dir(&self.startup) {
            fields.push("storage.downloadDir".to_owned());
        }
        if engine_override().is_none() && now.advanced.engine != self.startup.advanced.engine {
            fields.push("advanced.engine".to_owned());
        }
        fields
    }

    /// Applies a partial update (see [`update_settings`]), saves it, and announces it. Other
    /// commands that change settings (for example a WireGuard import) go through here too.
    pub(crate) async fn update(
        &self,
        app: &AppHandle,
        patch: &Value,
    ) -> Result<SettingsView, IpcError> {
        let _guard = self.updating.lock().await;
        let current = self.current();
        let next = apply_patch(&current, patch).map_err(|e| patch_error(&e))?;
        if next.storage.download_dir != current.storage.download_dir
            && let Some(dir) = next.storage.download_dir.clone()
        {
            blocking(move || check_writable(Path::new(&dir)))
                .await?
                .map_err(|error| {
                    tracing::warn!(%error, "download folder is not usable");
                    IpcError::invalid(
                        "storage.downloadDir: That folder could not be created or is not writable.",
                    )
                })?;
        }
        let store = self.store.clone();
        let saved = next.clone();
        blocking(move || store.save(&saved))
            .await?
            .map_err(|e| IpcError::internal(&e))?;
        if next.advanced.log_level != current.advanced.log_level
            && let Some(apply) = &self.log
        {
            apply(next.advanced.log_level);
        }
        self.load_warnings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.changes.send_replace(next);
        self.announce(app);
        Ok(self.view())
    }

    /// Saves an imported WireGuard configuration: its keys in the credential store and the rest
    /// in `network.wireguard` (see `import_wireguard_config`). The settings are checked before
    /// any key is stored, so an invalid import leaves nothing behind. A configuration without a
    /// preshared key removes the previous one.
    pub(crate) async fn import_wireguard(
        &self,
        app: &AppHandle,
        imported: &WireGuardSettings,
        private_key: SecretString,
        preshared_key: Option<SecretString>,
    ) -> Result<(SettingsView, SecretStatus), IpcError> {
        let patch = serde_json::json!({ "network": { "wireguard": imported } });
        apply_patch(&self.current(), &patch).map_err(|e| patch_error(&e))?;
        let store = self.secrets();
        let status = blocking(move || {
            store.set(SecretKey::WireGuardPrivateKey, &private_key)?;
            match &preshared_key {
                Some(key) => store.set(SecretKey::WireGuardPresharedKey, key)?,
                None => store.clear(SecretKey::WireGuardPresharedKey)?,
            }
            Ok(SecretStatus::of(store.as_ref()))
        })
        .await?
        .map_err(|e| secret_error(&e))?;
        // Announces the change once, with the keys already stored.
        let view = self.update(app, &patch).await?;
        Ok((view, status))
    }

    /// Tells watchers that secrets changed (the settings value itself is unchanged).
    fn notify_secrets(&self, app: &AppHandle) {
        self.changes.send_modify(|_| {});
        self.announce(app);
    }

    fn announce(&self, app: &AppHandle) {
        if let Err(error) = app.emit(SETTINGS_CHANGED, self.current()) {
            tracing::warn!(%error, "could not send the settings-changed event");
        }
    }
}

/// The download and upload limits of `bandwidth`, in bytes per second (0 KiB/s: unlimited).
pub(crate) fn rate_limits(
    bandwidth: &lumen_settings::BandwidthSettings,
) -> (Option<std::num::NonZeroU32>, Option<std::num::NonZeroU32>) {
    let bps = |kib: u32| std::num::NonZeroU32::new(kib.saturating_mul(1024));
    (
        bps(bandwidth.download_limit_kib),
        bps(bandwidth.upload_limit_kib),
    )
}

/// `LUMEN_ENGINE`, if set; it overrides the engine setting (for development).
fn engine_override() -> Option<String> {
    std::env::var(ENGINE_ENV_VAR)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Settings plus what the Settings page needs to explain them.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct SettingsView {
    /// The saved settings.
    pub(crate) settings: Settings,
    /// Problems to show: load problems and incomplete settings (for example a proxy without a
    /// host).
    pub(crate) warnings: Vec<String>,
    /// Dotted paths of changed settings that apply after a restart.
    pub(crate) restart_required: Vec<String>,
    /// The download folder used when none is chosen.
    pub(crate) default_download_dir: String,
    /// The download folder in use this run.
    pub(crate) active_download_dir: String,
    /// The `LUMEN_ENGINE` value, if set; it overrides `advanced.engine`.
    pub(crate) engine_override: Option<String>,
    /// Whether `RUST_LOG` overrides `advanced.logLevel`.
    pub(crate) log_level_override: bool,
}

/// How much space TorrentStream's download cache uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct CacheUsage {
    /// Total size of the cached files, in bytes.
    pub(crate) bytes: u64,
    /// Number of cached files.
    pub(crate) files: u64,
    /// False if counting stopped early; `bytes` is then a lower bound.
    pub(crate) complete: bool,
    /// Whether a clear is scheduled for the next start.
    pub(crate) clear_pending: bool,
}

/// What [`clear_cache`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) enum ClearCacheOutcome {
    /// The cache was deleted.
    Cleared,
    /// The engine is using the cache, so it will be deleted the next time TorrentStream starts.
    Scheduled,
}

/// Returns the settings and what the Settings page needs to explain them.
#[tauri::command]
#[allow(
    clippy::needless_pass_by_value,
    reason = "Tauri passes managed state by value"
)]
pub(crate) fn get_settings(state: State<'_, SettingsState>) -> SettingsView {
    state.view()
}

/// Applies a partial update, e.g. `{"bandwidth": {"peerLimit": 80}}`, and returns the saved
/// settings. Nothing is saved unless the whole result is valid.
#[tauri::command]
pub(crate) async fn update_settings(
    app: AppHandle,
    state: State<'_, SettingsState>,
    patch: Value,
) -> Result<SettingsView, IpcError> {
    state.update(&app, &patch).await
}

/// Stores a secret in the OS credential store. The value is never sent back.
#[tauri::command]
pub(crate) async fn set_secret(
    app: AppHandle,
    state: State<'_, SettingsState>,
    key: SecretKey,
    mut value: String,
) -> Result<SecretStatus, IpcError> {
    let prepared = key.prepare(&value);
    value.zeroize();
    let secret: SecretString = prepared.map_err(|e| secret_error(&e))?;
    let store = state.secrets();
    let status = blocking(move || {
        store.set(key, &secret)?;
        Ok(SecretStatus::of(store.as_ref()))
    })
    .await?
    .map_err(|e| secret_error(&e))?;
    state.notify_secrets(&app);
    Ok(status)
}

/// Removes a secret from the OS credential store.
#[tauri::command]
pub(crate) async fn clear_secret(
    app: AppHandle,
    state: State<'_, SettingsState>,
    key: SecretKey,
) -> Result<SecretStatus, IpcError> {
    let store = state.secrets();
    let status = blocking(move || {
        store.clear(key)?;
        Ok(SecretStatus::of(store.as_ref()))
    })
    .await?
    .map_err(|e| secret_error(&e))?;
    state.notify_secrets(&app);
    Ok(status)
}

/// Which secrets are set (never their values).
#[tauri::command]
pub(crate) async fn secret_status(
    state: State<'_, SettingsState>,
) -> Result<SecretStatus, IpcError> {
    let store = state.secrets();
    blocking(move || SecretStatus::of(store.as_ref())).await
}

/// How much space the download cache uses.
#[tauri::command]
pub(crate) async fn cache_usage(state: State<'_, SettingsState>) -> Result<CacheUsage, IpcError> {
    let dir = state.download_dir(&state.startup);
    let marker = state.store.dir().join(CLEAR_PENDING);
    blocking(move || {
        let (bytes, files, complete) = measure(&dir);
        CacheUsage {
            bytes,
            files,
            complete,
            clear_pending: marker.exists(),
        }
    })
    .await
}

/// Deletes TorrentStream's download cache (only its own cache folders), now if the engine has not
/// started, otherwise at the next start.
#[tauri::command]
pub(crate) async fn clear_cache(
    state: State<'_, SettingsState>,
    app_state: State<'_, AppState>,
) -> Result<ClearCacheOutcome, IpcError> {
    let dir = state.download_dir(&state.startup);
    let cleared = app_state
        .unless_started(|| {
            let dir = dir.clone();
            blocking(move || clear_dirs(&dir))
        })
        .await;
    if let Some(result) = cleared {
        result?.map_err(|e| IpcError::internal(&e))?;
        return Ok(ClearCacheOutcome::Cleared);
    }
    let marker = state.store.dir().join(CLEAR_PENDING);
    let text = dir.to_string_lossy().into_owned();
    blocking(move || fs::write(marker, text))
        .await?
        .map_err(|e| IpcError::internal(&e))?;
    Ok(ClearCacheOutcome::Scheduled)
}

/// Opens the system folder picker over `window`, starting at the saved download folder, and
/// returns the chosen folder, or `None` if the user cancelled. Nothing is saved: the Settings
/// page saves the choice with [`update_settings`], which checks it.
#[tauri::command]
pub(crate) async fn choose_download_dir(
    window: tauri::WebviewWindow,
    state: State<'_, SettingsState>,
) -> Result<Option<String>, IpcError> {
    let start = state.download_dir(&state.current());
    let mut dialog = rfd::AsyncFileDialog::new()
        .set_title("Choose where TorrentStream saves downloaded videos")
        .set_parent(&window);
    // The picker cannot start in a folder that does not exist yet.
    if start.is_dir() {
        dialog = dialog.set_directory(&start);
    }
    let chosen = dialog.pick_folder().await;
    Ok(chosen.map(|folder| folder.path().to_string_lossy().into_owned()))
}

/// Runs blocking work (file system, credential store) off the async runtime.
async fn blocking<R: Send + 'static>(
    f: impl FnOnce() -> R + Send + 'static,
) -> Result<R, IpcError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| IpcError::internal(&e))
}

fn patch_error(error: &PatchError) -> IpcError {
    IpcError::invalid(error.to_string())
}

fn secret_error(error: &SecretError) -> IpcError {
    match error {
        SecretError::Invalid(_) => IpcError::invalid(error.to_string()),
        SecretError::Unavailable | SecretError::Refused => {
            IpcError::new(ErrorKind::Internal, error.to_string())
        }
    }
}

/// Creates `dir` if needed and checks that files can be written in it.
fn check_writable(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    if !fs::metadata(dir)?.is_dir() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a folder"));
    }
    let probe = dir.join(".lumen-write-test");
    fs::write(&probe, b"")?;
    fs::remove_file(&probe)
}

/// Size and count of the files in the cache folders under `dir`. Symbolic links are skipped.
fn measure(dir: &Path) -> (u64, u64, bool) {
    let mut stack: Vec<PathBuf> = CACHE_DIRS.iter().map(|d| dir.join(d)).collect();
    let (mut bytes, mut files, mut seen) = (0_u64, 0_u64, 0_usize);
    while let Some(path) = stack.pop() {
        let Ok(entries) = fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > MAX_WALK_ENTRIES {
                return (bytes, files, false);
            }
            // `DirEntry::metadata` does not follow symbolic links.
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                bytes = bytes.saturating_add(meta.len());
                files += 1;
            }
        }
    }
    (bytes, files, true)
}

/// Deletes the cache folders under `dir`. A cache folder that is a symbolic link is left alone.
fn clear_dirs(dir: &Path) -> io::Result<()> {
    for name in CACHE_DIRS {
        let path = dir.join(name);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => fs::remove_dir_all(&path)?,
            Ok(_) => tracing::warn!(folder = name, "cache folder is not a folder; left alone"),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_settings::MemoryStore;

    #[test]
    fn bandwidth_limits_become_engine_rates() {
        let mut b = lumen_settings::BandwidthSettings::default();
        assert_eq!(rate_limits(&b), (None, None), "0 is unlimited");
        b.download_limit_kib = 2048;
        b.upload_limit_kib = 100;
        let (down, up) = rate_limits(&b);
        assert_eq!(down.map(std::num::NonZeroU32::get), Some(2 * 1024 * 1024));
        assert_eq!(up.map(std::num::NonZeroU32::get), Some(100 * 1024));
    }

    fn state(root: &Path) -> SettingsState {
        SettingsState::new(
            SettingsStore::new(root.join("config")),
            Arc::new(MemoryStore::new()),
            root.join("downloads"),
            None,
        )
    }

    #[test]
    fn engine_config_uses_settings() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path());
        let config = state.engine_config();
        assert_eq!(config.download_dir, dir.path().join("downloads"));
        if engine_override().is_none() {
            assert_eq!(config.choice, EngineChoice::Auto);
        }
    }

    #[test]
    fn restart_is_required_only_for_start_up_settings() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path());
        let mut next = state.current();
        next.bandwidth.peer_limit = 80;
        assert!(state.restart_required(&next).is_empty());
        next.storage.download_dir = Some(dir.path().join("elsewhere").display().to_string());
        assert_eq!(state.restart_required(&next), ["storage.downloadDir"]);
        // Choosing the default folder explicitly is not a change.
        next.storage.download_dir = Some(dir.path().join("downloads").display().to_string());
        assert!(state.restart_required(&next).is_empty());
    }

    #[test]
    fn clearing_touches_only_cache_folders() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["lumen-swarm/a/b", "librqbit/c", "mine"] {
            fs::create_dir_all(dir.path().join(name)).unwrap();
        }
        fs::write(dir.path().join("lumen-swarm/a/b/piece"), [0_u8; 100]).unwrap();
        fs::write(dir.path().join("librqbit/c/file"), [0_u8; 50]).unwrap();
        fs::write(dir.path().join("mine/keep"), [0_u8; 7]).unwrap();
        assert_eq!(measure(dir.path()), (150, 2, true));
        clear_dirs(dir.path()).unwrap();
        assert_eq!(measure(dir.path()), (0, 0, true));
        assert!(dir.path().join("mine/keep").exists());
        clear_dirs(dir.path()).unwrap();
    }

    #[test]
    fn pending_clear_runs_only_for_known_folders() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path());
        let cache = dir.path().join("downloads/librqbit");
        let other = dir.path().join("other/librqbit");
        fs::create_dir_all(&cache).unwrap();
        fs::create_dir_all(&other).unwrap();
        fs::create_dir_all(dir.path().join("config")).unwrap();
        let marker = dir.path().join("config").join(CLEAR_PENDING);

        fs::write(&marker, dir.path().join("other").display().to_string()).unwrap();
        state.run_startup_maintenance();
        assert!(other.exists(), "an unknown folder is never cleared");
        assert!(!marker.exists());

        fs::write(&marker, dir.path().join("downloads").display().to_string()).unwrap();
        state.run_startup_maintenance();
        assert!(!cache.exists());
        assert!(!marker.exists());
    }

    #[test]
    fn writable_check_creates_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("new/folder");
        check_writable(&target).unwrap();
        assert!(target.is_dir());
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
        let file = dir.path().join("file");
        fs::write(&file, b"x").unwrap();
        assert!(check_writable(&file).is_err());
    }

    #[test]
    fn errors_name_the_field_without_values() {
        let err = patch_error(
            &apply_patch(
                &Settings::default(),
                &serde_json::json!({"bandwidth": {"peerLimit": 9999}}),
            )
            .unwrap_err(),
        );
        assert_eq!(err.kind, ErrorKind::InvalidArgument);
        assert!(err.message.starts_with("bandwidth.peerLimit: "));
        let err = secret_error(&SecretKey::Socks5Password.prepare("").unwrap_err());
        assert_eq!(err.kind, ErrorKind::InvalidArgument);
    }
}
