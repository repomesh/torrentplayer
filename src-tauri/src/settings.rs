// SPDX-License-Identifier: MIT OR Apache-2.0

//! Settings commands: read and update `settings.json`, set or clear secrets in the OS credential
//! store, and report or clear the download cache.
//!
//! Every update is validated by `torrentplayer_settings::apply_patch`, saved atomically, and announced
//! with a [`SETTINGS_CHANGED`] event (to the webview) and a [`watch`] channel (to Rust
//! subsystems). Secrets can be set and cleared here but are never sent back to the webview.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter as _, Manager as _, State};
use tokio::sync::watch;
use torrentplayer_settings::{
    EngineSetting, KEYRING_SERVICE, KeyringStore, LEGACY_KEYRING_SERVICE, LogLevel, NetworkHold,
    PatchError, SecretError, SecretKey, SecretStatus, SecretStore, SecretString, Settings,
    SettingsStore, WireGuardSettings, Zeroize as _, apply_patch,
};
use torrentplayer_torrent::{ENGINE_ENV_VAR, EngineChoice, StreamEngineConfig};
use ts_rs::TS;

use crate::core::AppState;
use crate::error::{ErrorKind, IpcError};

/// Event sent to the webview, with the new [`Settings`], after settings or secrets change.
pub(crate) const SETTINGS_CHANGED: &str = "settings-changed";

/// Marker written by older versions when a cache clear had to wait for the next start.
const CLEAR_PENDING: &str = "clear-cache-pending";

/// Applies a log level to the running subscriber; `None` when `RUST_LOG` overrides it.
pub(crate) type LogApplier = Box<dyn Fn(LogLevel) + Send + Sync>;

/// Managed Tauri state for settings.
pub(crate) struct SettingsState {
    store: SettingsStore,
    /// The latest saved settings; subscribers are told about every change.
    changes: watch::Sender<Settings>,
    /// Serializes updates so concurrent patches cannot overwrite each other.
    updating: tokio::sync::Mutex<()>,
    /// Serializes cache walks and removals.
    maintenance: tokio::sync::Mutex<()>,
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
        let secrets =
            Arc::new(KeyringStore::new(KEYRING_SERVICE).with_legacy(LEGACY_KEYRING_SERVICE));
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
            maintenance: tokio::sync::Mutex::new(()),
            startup: loaded.settings,
            load_warnings: Mutex::new(loaded.warnings),
            secrets,
            default_download_dir,
            log,
        }
    }

    /// At start-up, before anything uses the download folder: points `storage.downloadDir` at
    /// `dir`, where the chosen folder moved with the app's data (see
    /// [`crate::migrate::relocated`]), and saves that, unless the file could not be used (it
    /// stays as it is until the user confirms a connection; the move is found again next time).
    pub(crate) fn move_download_dir(&mut self, dir: &Path) {
        let mut settings = self.current();
        settings.storage.download_dir = Some(dir.to_string_lossy().into_owned());
        if settings.network.hold == Some(NetworkHold::SettingsUnreadable) {
            tracing::debug!("not saving the moved download folder while settings are held");
        } else if let Err(error) = self.store.save(&settings) {
            tracing::warn!(%error, "could not save the moved download folder");
        }
        tracing::info!("the download folder moved with the app's data");
        self.startup = settings.clone();
        self.changes.send_replace(settings);
    }

    /// At start-up: holds traffic as for an unreadable settings file, with `warning` shown on
    /// the Settings page. Used when the settings from before the rename could not be carried
    /// over (see [`crate::migrate::settings_left_behind`]), so the defaults would otherwise
    /// connect directly.
    pub(crate) fn hold_unreadable(&mut self, warning: String) {
        let mut settings = self.current();
        settings.network.hold = Some(NetworkHold::SettingsUnreadable);
        self.load_warnings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(warning);
        self.startup = settings.clone();
        self.changes.send_replace(settings);
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
    /// from the settings, unless `TORRENTPLAYER_ENGINE` overrides the engine.
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
        config.peer_limit = usize::try_from(self.startup.bandwidth.peer_limit).unwrap_or(40);
        config
    }

    /// Applies the storage policy before the engine starts. A clear marker from an older version
    /// is treated as "clear finished downloads" and then retired; queued hashes stay protected.
    /// `queue` is `None` when the saved queue could not be read in full: then nothing is removed
    /// (and the marker is kept), since a download that looks unqueued may belong to it.
    pub(crate) fn run_startup_maintenance(&self, queue: Option<&torrentplayer_library::Queue>) {
        let Some(queue) = queue else {
            tracing::warn!("the queue did not load in full, so no downloads are cleaned up");
            return;
        };
        let marker = self.store.dir().join(CLEAR_PENDING);
        let legacy_clear = fs::read_to_string(&marker).ok().is_some_and(|value| {
            let target = PathBuf::from(value.trim());
            target == self.download_dir(&self.startup) || target == self.default_download_dir
        });
        let mode = if legacy_clear {
            crate::cache::CleanupMode::AllFinished
        } else {
            cleanup_mode(&self.startup)
        };
        let root = self.download_dir(&self.startup);
        let mut removed = 0_u32;
        if crate::cache::queue_hashes_complete(queue) {
            let protected = crate::cache::queue_hashes(queue);
            let plan = crate::cache::plan(&root, &protected, mode, SystemTime::now());
            for candidate in plan.candidates {
                match crate::cache::remove(&root, &candidate.info_hash) {
                    Ok(()) => removed = removed.saturating_add(1),
                    Err(error) => {
                        tracing::warn!(%error, "could not clean up a finished download");
                    }
                }
            }
        } else {
            tracing::debug!("cache cleanup is waiting for queued torrent metadata");
        }
        if removed > 0 {
            tracing::info!(removed, "cleaned up finished downloads at startup");
        }
        if marker.exists()
            && let Err(error) = fs::remove_file(&marker)
        {
            tracing::warn!(%error, "could not remove the old cache-clear marker");
        }
    }

    fn download_dir(&self, settings: &Settings) -> PathBuf {
        settings
            .storage
            .download_dir
            .as_ref()
            .map_or_else(|| self.default_download_dir.clone(), PathBuf::from)
    }

    /// The download folder used by the running engine.
    pub(crate) fn active_download_dir(&self) -> PathBuf {
        self.download_dir(&self.startup)
    }

    /// The FFmpeg folder setting, unless it lies inside a download folder: one saved before
    /// 0.3.1 refused that is ignored (FFmpeg there could come from a torrent), and FFmpeg is
    /// looked for as if no folder were set.
    pub(crate) fn ffmpeg_dir(&self) -> Option<String> {
        let settings = self.current();
        let downloads = [
            self.download_dir(&settings),
            self.active_download_dir(),
            self.default_download_dir.clone(),
        ];
        let dir = settings.advanced.ffmpeg_dir;
        if check_ffmpeg_dir(dir.as_deref(), &downloads).is_err() {
            tracing::warn!("ignoring an FFmpeg folder inside the download folder");
            return None;
        }
        dir
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
        if now.bandwidth.peer_limit != self.startup.bandwidth.peer_limit {
            fields.push("bandwidth.peerLimit".to_owned());
        }
        fields
    }

    /// Applies a partial update, saves it, and announces it. Commands that change settings for
    /// the backend's own reasons (a WireGuard import, a folder picker) go through here; the
    /// webview's [`update_settings`] goes through [`Self::update_from_webview`].
    pub(crate) async fn update(
        &self,
        app: &AppHandle,
        patch: &Value,
    ) -> Result<SettingsView, IpcError> {
        let _guard = self.updating.lock().await;
        self.update_locked(app, patch, Origin::Backend).await
    }

    /// [`Self::update`] for a patch from the webview, which may not choose folders (see
    /// [`check_picked_folders`]).
    async fn update_from_webview(
        &self,
        app: &AppHandle,
        patch: &Value,
    ) -> Result<SettingsView, IpcError> {
        let _guard = self.updating.lock().await;
        self.update_locked(app, patch, Origin::Webview).await
    }

    /// [`Self::update`], with `updating` already held.
    async fn update_locked(
        &self,
        app: &AppHandle,
        patch: &Value,
        origin: Origin,
    ) -> Result<SettingsView, IpcError> {
        let storage_policy_changed = self.save_locked(patch, origin).await?;
        self.announce(app);
        if storage_policy_changed {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                enforce_cache_policy(&app).await;
            });
        }
        Ok(self.view())
    }

    /// Checks, saves, and applies a patch, with `updating` already held; everything
    /// [`Self::update_locked`] does but tell the app. Returns whether the cache policy changed.
    async fn save_locked(&self, patch: &Value, origin: Origin) -> Result<bool, IpcError> {
        let current = self.current();
        let next = apply_patch(&current, patch).map_err(|e| patch_error(&e))?;
        if origin == Origin::Webview {
            check_picked_folders(&current, &next)?;
        }
        if next.advanced.ffmpeg_dir != current.advanced.ffmpeg_dir
            || next.storage.download_dir != current.storage.download_dir
        {
            let downloads = [
                self.download_dir(&next),
                self.active_download_dir(),
                self.default_download_dir.clone(),
            ];
            check_ffmpeg_dir(next.advanced.ffmpeg_dir.as_deref(), &downloads)?;
        }
        let storage_policy_changed = next.storage.cache_limit_gb != current.storage.cache_limit_gb
            || next.storage.cleanup_after_days != current.storage.cleanup_after_days;
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
        Ok(storage_policy_changed)
    }

    /// Saves an imported WireGuard configuration: its keys in the credential store and the rest
    /// in `network.wireguard` (see `import_wireguard_config`). A configuration without a
    /// preshared key removes the previous one. All or nothing: the settings are checked before
    /// any key is stored, and if storing a key or saving the settings fails, the previous keys
    /// are put back. Holds `updating` throughout, so no other change interleaves.
    pub(crate) async fn import_wireguard(
        &self,
        app: &AppHandle,
        imported: &WireGuardSettings,
        private_key: SecretString,
        preshared_key: Option<SecretString>,
    ) -> Result<(SettingsView, SecretStatus), IpcError> {
        let _guard = self.updating.lock().await;
        let status = self
            .import_wireguard_locked(imported, private_key, preshared_key)
            .await?;
        // Announced once, with the keys already stored.
        self.announce(app);
        Ok((self.view(), status))
    }

    /// [`Self::import_wireguard`] without telling the app, with `updating` already held.
    async fn import_wireguard_locked(
        &self,
        imported: &WireGuardSettings,
        private_key: SecretString,
        preshared_key: Option<SecretString>,
    ) -> Result<SecretStatus, IpcError> {
        let patch = serde_json::json!({ "network": { "wireguard": imported } });
        apply_patch(&self.current(), &patch).map_err(|e| patch_error(&e))?;
        let store = self.secrets();
        let previous = blocking(move || {
            let previous = WG_KEYS.map(|key| store.get(key));
            let previous = match previous {
                [Ok(private), Ok(preshared)] => [private, preshared],
                [Err(error), _] | [_, Err(error)] => return Err(error),
            };
            let stored = store
                .set(SecretKey::WireGuardPrivateKey, &private_key)
                .and_then(|()| match &preshared_key {
                    Some(key) => store.set(SecretKey::WireGuardPresharedKey, key),
                    None => store.clear(SecretKey::WireGuardPresharedKey),
                });
            if let Err(error) = stored {
                restore_keys(store.as_ref(), &previous);
                return Err(error);
            }
            Ok(previous)
        })
        .await?
        .map_err(|e| secret_error(&e))?;
        if let Err(error) = self.save_locked(&patch, Origin::Backend).await {
            let store = self.secrets();
            blocking(move || restore_keys(store.as_ref(), &previous)).await?;
            return Err(error);
        }
        let store = self.secrets();
        blocking(move || SecretStatus::of(store.as_ref())).await
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
    bandwidth: &torrentplayer_settings::BandwidthSettings,
) -> (Option<std::num::NonZeroU32>, Option<std::num::NonZeroU32>) {
    let bps = |kib: u32| std::num::NonZeroU32::new(kib.saturating_mul(1024));
    (
        bps(bandwidth.download_limit_kib),
        bps(bandwidth.upload_limit_kib),
    )
}

/// `TORRENTPLAYER_ENGINE`, if set; it overrides the engine setting (for development).
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
    /// The `TORRENTPLAYER_ENGINE` value, if set; it overrides `advanced.engine`.
    pub(crate) engine_override: Option<String>,
    /// Whether `RUST_LOG` overrides `advanced.logLevel`.
    pub(crate) log_level_override: bool,
}

/// How much space TorrentPlayer's download cache uses.
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
    /// Configured maximum in bytes, or `null` when unlimited.
    pub(crate) limit_bytes: Option<u64>,
    /// Finished, unqueued data that "Clear now" can remove.
    pub(crate) reclaimable_bytes: u64,
    /// Number of finished, unqueued downloads that "Clear now" can remove.
    pub(crate) reclaimable_downloads: u32,
}

/// What [`clear_cache`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct ClearCacheOutcome {
    /// Bytes removed, including TorrentPlayer's small completion markers.
    pub(crate) bytes_freed: u64,
    /// Finished downloads removed.
    pub(crate) downloads_removed: u32,
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
    state.update_from_webview(&app, &patch).await
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

/// How much space the download cache uses and how much finished data can be reclaimed safely.
#[tauri::command]
pub(crate) async fn cache_usage(
    app: AppHandle,
    state: State<'_, SettingsState>,
) -> Result<CacheUsage, IpcError> {
    let dir = state.active_download_dir();
    let protected = protected_hashes(&app);
    let limit_bytes = cache_limit_bytes(state.current().storage.cache_limit_gb);
    blocking(move || {
        let Some(protected) = protected else {
            let usage = crate::cache::measure(&dir);
            return CacheUsage {
                bytes: usage.bytes,
                files: usage.files,
                complete: usage.complete,
                limit_bytes,
                reclaimable_bytes: 0,
                reclaimable_downloads: 0,
            };
        };
        let plan = crate::cache::plan(
            &dir,
            &protected,
            crate::cache::CleanupMode::AllFinished,
            SystemTime::now(),
        );
        CacheUsage {
            bytes: plan.usage.bytes,
            files: plan.usage.files,
            complete: plan.usage.complete,
            limit_bytes,
            reclaimable_bytes: plan.reclaimable_bytes,
            reclaimable_downloads: u32::try_from(plan.candidates.len()).unwrap_or(u32::MAX),
        }
    })
    .await
}

/// Deletes every finished download that is neither queued nor open in the player.
#[tauri::command]
pub(crate) async fn clear_cache(app: AppHandle) -> Result<ClearCacheOutcome, IpcError> {
    run_cache_cleanup(&app, crate::cache::CleanupMode::AllFinished).await
}

/// Applies the configured size and age policies. Failures are logged because this runs in the
/// background; the explicit clear command returns infrastructure failures to the caller.
pub(crate) async fn enforce_cache_policy(app: &AppHandle) {
    let settings = app.state::<SettingsState>().current();
    if settings.storage.cache_limit_gb.is_none() && settings.storage.cleanup_after_days.is_none() {
        return;
    }
    // A queue that did not load in full may have lost items whose downloads look unqueued.
    if app.state::<crate::queue::QueueState>().degraded() {
        tracing::debug!("the cache policy waits for the next start: the queue loaded with errors");
        return;
    }
    if let Err(error) = run_cache_cleanup(app, cleanup_mode(&settings)).await {
        tracing::warn!(message = %error.message, "could not enforce the cache policy");
    }
}

async fn run_cache_cleanup(
    app: &AppHandle,
    mode: crate::cache::CleanupMode,
) -> Result<ClearCacheOutcome, IpcError> {
    let settings = app.state::<SettingsState>();
    let _maintenance = settings.maintenance.lock().await;
    let downloads = app.state::<crate::downloads::DownloadsState>();
    // Queue saves and stream starts use the same gate, so the protection snapshot and removals
    // are one transaction from the user's point of view.
    let _cache_gate = downloads.cache_gate().lock().await;
    let root = settings.active_download_dir();
    let Some(protected) = protected_hashes(app) else {
        return Ok(ClearCacheOutcome {
            bytes_freed: 0,
            downloads_removed: 0,
        });
    };
    let plan_root = root.clone();
    let plan =
        blocking(move || crate::cache::plan(&plan_root, &protected, mode, SystemTime::now()))
            .await?;

    let mut outcome = ClearCacheOutcome {
        bytes_freed: 0,
        downloads_removed: 0,
    };
    let mut first_error = None;
    for candidate in plan.candidates {
        // A stream can close without the gate. Re-checking also makes this robust if another
        // backend command acquired the torrent between the scan and this iteration.
        if protected_hashes(app).is_none_or(|hashes| hashes.contains(&candidate.info_hash)) {
            continue;
        }
        if let Some(core) = app.state::<AppState>().started()
            && let Some(id) = core.engine.torrent_with_hash(&candidate.info_hash)
            && let Err(error) = core.engine.remove(id).await
        {
            tracing::warn!(%error, hash = %candidate.info_hash, "could not release a finished download");
            first_error.get_or_insert_with(|| IpcError::from(error));
            continue;
        }
        match remove_cache_entry(root.clone(), candidate.info_hash.clone()).await {
            Ok(()) => {
                outcome.bytes_freed = outcome.bytes_freed.saturating_add(candidate.bytes);
                outcome.downloads_removed = outcome.downloads_removed.saturating_add(1);
            }
            Err(error) => {
                tracing::warn!(%error, hash = %candidate.info_hash, "could not remove a finished download");
                first_error.get_or_insert(error);
            }
        }
    }
    if outcome.downloads_removed > 0 {
        tracing::info!(
            removed = outcome.downloads_removed,
            bytes = outcome.bytes_freed,
            "cleaned up finished downloads"
        );
    } else if let Some(error) = first_error {
        return Err(error);
    }
    Ok(outcome)
}

async fn remove_cache_entry(root: PathBuf, info_hash: String) -> Result<(), IpcError> {
    const ATTEMPTS: usize = 20;
    for attempt in 0..ATTEMPTS {
        let remove_root = root.clone();
        let hash = info_hash.clone();
        let result = blocking(move || crate::cache::remove(&remove_root, &hash)).await?;
        match result {
            Ok(()) => return Ok(()),
            Err(error) if attempt + 1 < ATTEMPTS => {
                tracing::debug!(%error, %info_hash, "cache files are still closing");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) => return Err(IpcError::internal(&error)),
        }
    }
    Ok(())
}

fn protected_hashes(app: &AppHandle) -> Option<HashSet<String>> {
    let downloads = app.state::<crate::downloads::DownloadsState>();
    let queue = downloads.queue();
    if !crate::cache::queue_hashes_complete(&queue) {
        return None;
    }
    let mut hashes = crate::cache::queue_hashes(&queue);
    let app_state = app.state::<AppState>();
    let Some(core) = app_state.started() else {
        return Some(hashes);
    };
    let streams = core.streams.lock().unwrap_or_else(PoisonError::into_inner);
    for stream in streams.values() {
        if let Ok(info) = core.engine.info(stream.torrent) {
            hashes.insert(info.info_hash.to_ascii_lowercase());
        }
    }
    drop(streams);
    if let Some(preload) = core.engine.preload_status() {
        hashes.insert(preload.info_hash.to_ascii_lowercase());
    }
    // The explorer's video preview reads from its torrent while it is open.
    if let Some(previewed) = app.state::<crate::preview::PreviewState>().previewed_hash() {
        hashes.insert(previewed);
    }
    Some(hashes)
}

fn cleanup_mode(settings: &Settings) -> crate::cache::CleanupMode {
    crate::cache::CleanupMode::Policy {
        limit_bytes: cache_limit_bytes(settings.storage.cache_limit_gb),
        max_age: settings
            .storage
            .cleanup_after_days
            .map(|days| Duration::from_secs(u64::from(days) * 24 * 60 * 60)),
    }
}

fn cache_limit_bytes(gigabytes: Option<u32>) -> Option<u64> {
    gigabytes.map(|gb| u64::from(gb) * 1_000_000_000)
}

/// Opens the system folder picker over `window`, starting at the saved download folder, and
/// saves the chosen folder as `storage.downloadDir` (checked like any change). Returns the saved
/// settings, or `None` if the user cancelled. This is the only way to choose the folder (see
/// [`check_picked_folders`]).
#[tauri::command]
pub(crate) async fn choose_download_dir(
    app: AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, SettingsState>,
) -> Result<Option<SettingsView>, IpcError> {
    let Some(folder) = pick_download_dir(&window, &state).await else {
        return Ok(None);
    };
    let patch = serde_json::json!({ "storage": { "downloadDir": folder } });
    state.update(&app, &patch).await.map(Some)
}

async fn pick_download_dir(window: &tauri::WebviewWindow, state: &SettingsState) -> Option<String> {
    let start = state.download_dir(&state.current());
    let mut dialog = rfd::AsyncFileDialog::new()
        .set_title("Choose where TorrentPlayer saves downloaded videos")
        .set_parent(window);
    // The picker cannot start in a folder that does not exist yet.
    if start.is_dir() {
        dialog = dialog.set_directory(&start);
    }
    let chosen = dialog.pick_folder().await;
    chosen.map(|folder| folder.path().to_string_lossy().into_owned())
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

/// The keys of an imported WireGuard configuration.
const WG_KEYS: [SecretKey; 2] = [
    SecretKey::WireGuardPrivateKey,
    SecretKey::WireGuardPresharedKey,
];

/// Puts back the WireGuard keys from before a failed import (`None`: there was none).
fn restore_keys(store: &dyn SecretStore, previous: &[Option<SecretString>; 2]) {
    for (key, value) in WG_KEYS.into_iter().zip(previous) {
        let restored = match value {
            Some(value) => store.set(key, value),
            None => store.clear(key),
        };
        if let Err(error) = restored {
            tracing::warn!(%error, ?key, "could not put the previous WireGuard key back");
        }
    }
}

/// Who asked for a settings change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// The webview, through [`update_settings`].
    Webview,
    /// The backend: a folder picker, a WireGuard import.
    Backend,
}

/// Refuses a new download or FFmpeg folder from the webview. Only the native folder pickers
/// ([`choose_download_dir`], [`crate::media::choose_ffmpeg_dir`]) choose them, and save the
/// choice themselves: FFmpeg from that folder runs on every stream start, and checking a folder
/// touches it, so a compromised webview must not be able to name one. Going back to the default
/// (`null`) is allowed.
fn check_picked_folders(current: &Settings, next: &Settings) -> Result<(), IpcError> {
    if next.storage.download_dir != current.storage.download_dir
        && next.storage.download_dir.is_some()
    {
        return Err(IpcError::invalid(
            "storage.downloadDir: Choose the folder with \"Choose folder…\".",
        ));
    }
    if next.advanced.ffmpeg_dir != current.advanced.ffmpeg_dir && next.advanced.ffmpeg_dir.is_some()
    {
        return Err(IpcError::invalid(
            "advanced.ffmpegDir: Choose the folder with \"Choose folder…\".",
        ));
    }
    Ok(())
}

/// Refuses an FFmpeg folder inside a download folder (which holds the engine caches): files
/// there come from torrents, and FFmpeg from that folder would run.
fn check_ffmpeg_dir(ffmpeg_dir: Option<&str>, downloads: &[PathBuf]) -> Result<(), IpcError> {
    let Some(dir) = ffmpeg_dir else {
        return Ok(());
    };
    if downloads.iter().any(|d| is_within(Path::new(dir), d)) {
        return Err(IpcError::invalid(
            "advanced.ffmpegDir: Choose a folder outside the download folder.",
        ));
    }
    Ok(())
}

/// Whether `path` is `parent` or inside it, comparing names case-insensitively on Windows.
fn is_within(path: &Path, parent: &Path) -> bool {
    let key = |p: &Path| -> Vec<String> {
        p.components()
            .map(|c| {
                let s = c.as_os_str().to_string_lossy();
                if cfg!(windows) {
                    s.to_lowercase()
                } else {
                    s.into_owned()
                }
            })
            .collect()
    };
    let (path, parent) = (key(path), key(parent));
    !parent.is_empty() && path.starts_with(&parent)
}

/// Creates `dir` if needed and checks that files can be written in it.
fn check_writable(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    if !fs::metadata(dir)?.is_dir() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a folder"));
    }
    let probe = dir.join(".torrentplayer-write-test");
    fs::write(&probe, b"")?;
    fs::remove_file(&probe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use torrentplayer_settings::{ExposeSecret as _, MemoryStore};

    #[test]
    fn bandwidth_limits_become_engine_rates() {
        let mut b = torrentplayer_settings::BandwidthSettings::default();
        assert_eq!(rate_limits(&b), (None, None), "0 is unlimited");
        b.download_limit_kib = 2048;
        b.upload_limit_kib = 100;
        let (down, up) = rate_limits(&b);
        assert_eq!(down.map(std::num::NonZeroU32::get), Some(2 * 1024 * 1024));
        assert_eq!(up.map(std::num::NonZeroU32::get), Some(100 * 1024));
    }

    #[test]
    fn cache_limits_use_the_decimal_gigabytes_shown_in_settings() {
        assert_eq!(cache_limit_bytes(None), None);
        assert_eq!(cache_limit_bytes(Some(1)), Some(1_000_000_000));
    }

    fn state(root: &Path) -> SettingsState {
        SettingsState::new(
            SettingsStore::new(root.join("config")),
            Arc::new(MemoryStore::new()),
            root.join("downloads"),
            None,
        )
    }

    fn wireguard_import() -> WireGuardSettings {
        WireGuardSettings {
            name: Some("vpn.conf".into()),
            addresses: vec!["10.0.0.2/32".into()],
            peer_public_key: Some("xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=".into()),
            peer_endpoint: Some("198.51.100.1:51820".into()),
            allowed_ips: vec!["0.0.0.0/0".into()],
            ..WireGuardSettings::default()
        }
    }

    fn secret(text: &str) -> SecretString {
        SecretString::from(text)
    }

    fn key(store: &dyn SecretStore, key: SecretKey) -> Option<String> {
        store
            .get(key)
            .unwrap()
            .map(|s| s.expose_secret().to_owned())
    }

    #[tokio::test]
    async fn a_wireguard_import_stores_keys_and_settings_together() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let secrets = state.secrets();
        let status = state
            .import_wireguard_locked(&wireguard_import(), secret("new-private"), None)
            .await
            .unwrap();
        assert!(status.wire_guard_private_key);
        assert_eq!(
            key(secrets.as_ref(), SecretKey::WireGuardPrivateKey).as_deref(),
            Some("new-private")
        );
        assert_eq!(state.current().network.wireguard, wireguard_import());
    }

    #[tokio::test]
    async fn a_failed_wireguard_import_puts_the_previous_keys_back() {
        let root = tempfile::tempdir().unwrap();
        // The settings cannot be saved: a file stands where the config folder belongs.
        fs::write(root.path().join("config"), "").unwrap();
        let state = state(root.path());
        let secrets = state.secrets();
        secrets
            .set(SecretKey::WireGuardPrivateKey, &secret("old-private"))
            .unwrap();
        secrets
            .set(SecretKey::WireGuardPresharedKey, &secret("old-preshared"))
            .unwrap();
        let error = state
            .import_wireguard_locked(&wireguard_import(), secret("new-private"), None)
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Internal);
        assert_eq!(
            key(secrets.as_ref(), SecretKey::WireGuardPrivateKey).as_deref(),
            Some("old-private")
        );
        assert_eq!(
            key(secrets.as_ref(), SecretKey::WireGuardPresharedKey).as_deref(),
            Some("old-preshared")
        );
        assert_eq!(
            state.current().network.wireguard,
            WireGuardSettings::default()
        );

        // Without earlier keys, nothing is left behind.
        secrets.clear(SecretKey::WireGuardPrivateKey).unwrap();
        secrets.clear(SecretKey::WireGuardPresharedKey).unwrap();
        state
            .import_wireguard_locked(
                &wireguard_import(),
                secret("new-private"),
                Some(secret("new-preshared")),
            )
            .await
            .unwrap_err();
        assert_eq!(key(secrets.as_ref(), SecretKey::WireGuardPrivateKey), None);
        assert_eq!(
            key(secrets.as_ref(), SecretKey::WireGuardPresharedKey),
            None
        );
    }

    #[test]
    fn an_ffmpeg_folder_saved_inside_the_download_folder_is_ignored() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config");
        let inside = root.path().join("downloads").join("ffmpeg");
        let outside = root.path().join("tools");
        for (dir, used) in [(&inside, None), (&outside, outside.to_str())] {
            // Written by 0.3.0, which did not refuse a folder inside the download folder.
            fs::create_dir_all(&config).unwrap();
            fs::write(
                config.join("settings.json"),
                serde_json::json!({ "version": 4, "advanced": { "ffmpegDir": dir } }).to_string(),
            )
            .unwrap();
            assert_eq!(state(root.path()).ffmpeg_dir().as_deref(), used);
        }
        // Held traffic from a failed carry-over: the defaults, and the notice.
        let mut held = state(root.path());
        held.hold_unreadable("left behind".into());
        assert_eq!(
            held.current().network.hold,
            Some(NetworkHold::SettingsUnreadable)
        );
        assert!(held.view().warnings.iter().any(|w| w == "left behind"));
    }

    #[test]
    fn a_moved_download_folder_is_saved_and_used_from_the_start() {
        let root = tempfile::tempdir().unwrap();
        let mut state = state(root.path());
        let moved = root
            .path()
            .join("io.torrentplayer.desktop")
            .join("downloads");
        state.move_download_dir(&moved);
        assert_eq!(state.active_download_dir(), moved);
        assert_eq!(state.engine_config().download_dir, moved);
        assert!(state.restart_required(&state.current()).is_empty());
        let saved = SettingsStore::new(root.path().join("config"))
            .load()
            .settings;
        assert_eq!(
            saved.storage.download_dir.as_deref(),
            moved.to_str(),
            "saved, so the next start uses it too"
        );
    }

    #[test]
    fn engine_config_uses_settings() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path());
        let config = state.engine_config();
        assert_eq!(config.download_dir, dir.path().join("downloads"));
        assert_eq!(config.peer_limit, 40);
        if engine_override().is_none() {
            assert_eq!(config.choice, EngineChoice::Auto);
        }
    }

    #[test]
    fn only_the_folder_pickers_choose_folders() {
        let current = Settings::default();
        let local = if cfg!(windows) {
            r"D:\Videos"
        } else {
            "/home/me/Videos"
        };
        for patch in [
            serde_json::json!({"storage": {"downloadDir": local}}),
            serde_json::json!({"advanced": {"ffmpegDir": local}}),
        ] {
            let next = apply_patch(&current, &patch).unwrap();
            let error = check_picked_folders(&current, &next).unwrap_err();
            assert!(error.message.contains("Choose folder"), "{patch}");
        }
        // UNC paths never get as far as the file system.
        let unc = serde_json::json!({"advanced": {"ffmpegDir": r"\\attacker\share"}});
        assert!(apply_patch(&current, &unc).is_err());

        // Going back to the default, and changes that keep the folders, are fine.
        let mut chosen = current.clone();
        chosen.storage.download_dir = Some(local.into());
        chosen.advanced.ffmpeg_dir = Some(local.into());
        let reset = apply_patch(
            &chosen,
            &serde_json::json!({"storage": {"downloadDir": null}, "advanced": {"ffmpegDir": null}}),
        )
        .unwrap();
        assert_eq!(check_picked_folders(&chosen, &reset), Ok(()));
        let other = apply_patch(
            &chosen,
            &serde_json::json!({"bandwidth": {"peerLimit": 80}}),
        )
        .unwrap();
        assert_eq!(check_picked_folders(&chosen, &other), Ok(()));
    }

    #[test]
    fn ffmpeg_is_never_run_from_the_download_folder() {
        let root = if cfg!(windows) {
            r"C:\Users\me"
        } else {
            "/home/me"
        };
        let downloads = [Path::new(root).join("Downloads").join("TorrentPlayer")];
        let inside = downloads[0].join("torrentplayer-swarm").join("x");
        assert!(check_ffmpeg_dir(inside.to_str(), &downloads).is_err());
        assert!(check_ffmpeg_dir(downloads[0].to_str(), &downloads).is_err());
        if cfg!(windows) {
            let shouting = downloads[0].to_string_lossy().to_uppercase();
            assert!(check_ffmpeg_dir(Some(&shouting), &downloads).is_err());
        }
        let beside = Path::new(root).join("Downloads").join("TorrentPlayer2");
        assert_eq!(check_ffmpeg_dir(beside.to_str(), &downloads), Ok(()));
        assert_eq!(check_ffmpeg_dir(None, &downloads), Ok(()));
    }

    #[test]
    fn restart_is_required_only_for_start_up_settings() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path());
        let mut next = state.current();
        next.bandwidth.peer_limit = 80;
        assert_eq!(state.restart_required(&next), ["bandwidth.peerLimit"]);
        next.storage.download_dir = Some(dir.path().join("elsewhere").display().to_string());
        assert_eq!(
            state.restart_required(&next),
            ["storage.downloadDir", "bandwidth.peerLimit"]
        );
        // Choosing the default folder explicitly is not a change.
        next.storage.download_dir = Some(dir.path().join("downloads").display().to_string());
        assert_eq!(state.restart_required(&next), ["bandwidth.peerLimit"]);
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
        state.run_startup_maintenance(Some(&torrentplayer_library::Queue::default()));
        assert!(other.exists(), "an unknown folder is never cleared");
        assert!(!marker.exists());

        fs::write(&marker, dir.path().join("downloads").display().to_string()).unwrap();
        state.run_startup_maintenance(Some(&torrentplayer_library::Queue::default()));
        // Old unmarked layouts are not identifiable as finished and remain untouched.
        assert!(cache.exists());
        assert!(!marker.exists());
    }

    #[test]
    fn nothing_is_cleaned_up_when_the_queue_did_not_load_in_full() {
        const HASH: &str = "1111111111111111111111111111111111111111";
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path());
        let root = dir.path().join("downloads");
        let finished = root.join(crate::cache::CACHE_DIRS[0]).join(HASH);
        fs::create_dir_all(&finished).unwrap();
        fs::write(finished.join("pieces"), b"data").unwrap();
        crate::cache::mark_finished(&root, torrentplayer_torrent::BackendKind::Swarm, HASH)
            .unwrap();
        fs::create_dir_all(dir.path().join("config")).unwrap();
        let marker = dir.path().join("config").join(CLEAR_PENDING);
        fs::write(&marker, root.display().to_string()).unwrap();

        // A damaged or unreadable queue loads empty: its finished downloads must survive.
        state.run_startup_maintenance(None);
        assert!(finished.exists());
        assert!(marker.exists(), "the clear is still pending");

        state.run_startup_maintenance(Some(&torrentplayer_library::Queue::default()));
        assert!(
            !finished.exists(),
            "the control: an unqueued finished download is cleared"
        );
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
