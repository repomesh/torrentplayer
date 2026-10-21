// SPDX-License-Identifier: MIT OR Apache-2.0

//! TorrentStream's Tauri application shell.
//!
//! This crate wires the backend crates into a Tauri app: it registers IPC commands,
//! configures logging, and owns application state. Business logic lives in the
//! `lumen-*` crates so it can be tested without a webview.
//!
//! Every IPC command validates its arguments on the Rust side; the frontend is never
//! trusted. The full IPC contract is documented in `docs/ipc.md`.

#![forbid(unsafe_code)]

mod audio;
mod cache;
mod commands;
mod core;
mod details;
mod downloads;
mod error;
mod latest;
mod library;
mod migrate;
mod network;
mod prefetch;
mod preload;
mod preview;
mod queue;
mod search;
mod settings;
mod subtitles;
mod thumbnails;
mod window;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context as _;
use lumen_settings::LogLevel;
use tauri::Manager as _;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

pub use commands::AppInfo;
pub use error::{ErrorKind, IpcError};

/// Environment variable naming a folder that holds a separate profile (settings, queue, and
/// downloads) instead of the usual per-user folders, for tests and portable installs. The
/// webview's own data follows `WEBVIEW2_USER_DATA_FOLDER` on Windows.
pub const PROFILE_ENV_VAR: &str = "LUMEN_PROFILE_DIR";

/// Where the app keeps its files.
struct Folders {
    /// Settings.
    config: PathBuf,
    /// The queue.
    data: PathBuf,
    /// App-local data (the webview's, and downloads before they moved to `downloads`).
    local_data: PathBuf,
    /// The default download folder: `TorrentStream` in the user's Downloads folder.
    downloads: PathBuf,
}

impl Folders {
    /// The per-user folders, or the subfolders of [`PROFILE_ENV_VAR`] when it names an
    /// absolute path.
    fn resolve(app: &tauri::App) -> tauri::Result<Self> {
        if let Some(root) = std::env::var_os(PROFILE_ENV_VAR).map(PathBuf::from) {
            if root.is_absolute() {
                tracing::info!("using the profile folder from {PROFILE_ENV_VAR}");
                return Ok(Self {
                    config: root.join("config"),
                    data: root.join("data"),
                    local_data: root.join("local"),
                    downloads: root.join("local").join("downloads"),
                });
            }
            tracing::warn!("ignoring {PROFILE_ENV_VAR}: not an absolute path");
        }
        let local_data = app.path().app_local_data_dir()?;
        // Systems without a Downloads folder keep downloads with the app's own data.
        let downloads = app.path().download_dir().map_or_else(
            |_| local_data.join("downloads"),
            |dir| dir.join("TorrentStream"),
        );
        let folders = Self {
            config: app.path().app_config_dir()?,
            data: app.path().app_data_dir()?,
            local_data,
            downloads,
        };
        // Settings, the queue, and the cache from before the rename to TorrentStream.
        migrate::from_old_identifier(&[&folders.config, &folders.data, &folders.local_data]);
        Ok(folders)
    }
}

/// Builds and runs the Tauri application until the last window closes.
///
/// # Errors
///
/// Returns an error if the Tauri runtime fails to start, for example when the
/// system webview is missing.
#[allow(
    clippy::too_many_lines,
    reason = "the builder keeps all command and managed-state wiring auditable in one place"
)]
pub fn run() -> anyhow::Result<()> {
    let log = init_tracing();
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "starting TorrentStream"
    );

    tauri::Builder::default()
        .setup(move |app| {
            // Settings live in the config folder; downloads default to Downloads\TorrentStream.
            let folders = Folders::resolve(app)?;
            let earlier_default = folders.local_data.join("downloads");
            let settings =
                settings::SettingsState::load(folders.config, folders.downloads.clone(), log);
            // Downloads used to default to the app's data folder: move them along, unless the
            // user chose a folder of their own.
            if settings.current().storage.download_dir.is_none()
                && earlier_default != folders.downloads
                && earlier_default.is_dir()
            {
                migrate::migrate(&earlier_default, &folders.downloads);
            }
            let library = library::LibraryState::new(folders.data.clone());
            let queue = queue::QueueState::new(folders.data);
            let saved_queue = queue.load();
            settings.run_startup_maintenance(&saved_queue);
            // The network manager starts before anything can connect, and then follows the
            // network settings, so no traffic ever leaves outside the selected mode.
            let network = network::start(&settings.current(), settings.secrets().as_ref())?;
            network::follow(
                app.handle(),
                &network,
                settings.subscribe(),
                settings.secrets(),
            );
            app.manage(core::AppState::new(
                settings.engine_config(),
                network.clone(),
            ));
            app.manage(settings);
            app.manage(search::SearchState::new(network.clone()));
            app.manage(network::NetworkState::new(network));
            network::watch_public_ip(app.handle());
            app.manage(preload::PreloadState::default());
            app.manage(prefetch::PrefetchState::default());
            app.manage(thumbnails::ThumbnailState::default());
            app.manage(downloads::DownloadsState::new(saved_queue));
            app.manage(library);
            app.manage(queue);
            app.manage(preview::PreviewState::default());
            downloads::start(app.handle());
            window::show_eventually(app.handle());
            Ok(())
        })
        .on_page_load(window::on_page_load)
        .invoke_handler(tauri::generate_handler![
            commands::app_info,
            commands::add_torrent_link,
            commands::add_torrent_file,
            commands::start_stream,
            commands::report_playback,
            commands::record_player_metric,
            commands::stream_stats,
            audio::stream_audio,
            subtitles::stream_subtitles,
            subtitles::load_subtitles,
            subtitles::parse_subtitles,
            commands::stop_stream,
            commands::dev_autoplay,
            search::search_torrents,
            search::search_torrents_progressive,
            search::cancel_search,
            prefetch::prefetch_torrents,
            thumbnails::fetch_thumbnails,
            thumbnails::preview_frame,
            details::torrent_details,
            details::open_link,
            preload::preload_status,
            queue::load_queue,
            queue::save_queue,
            queue::preload_next,
            queue::queue_readiness,
            downloads::queue_downloads,
            library::library_items,
            preview::preview_file,
            settings::get_settings,
            settings::update_settings,
            settings::set_secret,
            settings::clear_secret,
            settings::secret_status,
            settings::cache_usage,
            settings::clear_cache,
            settings::choose_download_dir,
            network::network_status,
            network::recheck_network,
            network::list_interfaces,
            network::validate_wireguard_config,
            network::import_wireguard_config,
            network::check_connection,
            network::public_ip,
        ])
        .build(tauri::generate_context!())
        .context("failed to start the Tauri application")?
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                shut_down(app);
            }
        });
    Ok(())
}

/// The longest TorrentStream waits for the engine to stop when it exits.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// Stops the stream engine, if it started, so an unplayed preload's cache is deleted now rather
/// than at the next start. Waits at most [`SHUTDOWN_TIMEOUT`], since exiting must not hang.
fn shut_down(app: &tauri::AppHandle) {
    let Some(core) = app
        .try_state::<core::AppState>()
        .and_then(|state| state.started().cloned())
    else {
        return;
    };
    if let Some(library) = app.try_state::<library::LibraryState>() {
        let progress: Vec<_> = core
            .streams
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter_map(|stream| {
                stream.progress.position_secs.map(|position| {
                    (
                        stream.media.clone(),
                        position,
                        stream.progress.duration_secs,
                        library.revision(),
                    )
                })
            })
            .collect();
        for (media, position, duration, revision) in progress {
            library.record_sync(&media, position, duration, revision);
        }
    }
    let stopped = tauri::async_runtime::block_on(async move {
        tokio::time::timeout(SHUTDOWN_TIMEOUT, core.engine.shutdown()).await
    });
    if stopped.is_ok() {
        tracing::info!("stream engine stopped");
    } else {
        tracing::warn!("the stream engine did not stop in time; exiting anyway");
    }
}

/// Configures `tracing` output. `RUST_LOG` sets the level if present; otherwise the log level
/// setting does, through the returned applier (default `info` until settings load).
fn init_tracing() -> Option<settings::LogApplier> {
    // `try_init` fails only if a subscriber is already set, which is harmless.
    if let Ok(filter) = EnvFilter::try_from_default_env() {
        let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
        return None;
    }
    let (filter, handle) = tracing_subscriber::reload::Layer::new(EnvFilter::new("info"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .try_init();
    Some(Box::new(move |level: LogLevel| {
        if let Err(error) = handle.reload(EnvFilter::new(level.directive())) {
            tracing::warn!(%error, "could not change the log level");
        }
    }))
}
