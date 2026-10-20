// SPDX-License-Identifier: MIT OR Apache-2.0

//! Build script: generates Tauri context and per-command permissions.

fn main() {
    // Declaring the app's commands here makes Tauri generate an `allow-<command>`
    // permission for each one. Commands are then denied unless a capability in
    // `capabilities/` grants them, which keeps IPC access least-privilege.
    let attributes = tauri_build::Attributes::new()
        .app_manifest(tauri_build::AppManifest::new().commands(&[
            "app_info",
            "add_torrent_link",
            "add_torrent_file",
            "start_stream",
            "report_playback",
            "record_player_metric",
            "stream_stats",
            "stream_audio",
            "stream_subtitles",
            "load_subtitles",
            "parse_subtitles",
            "stop_stream",
            "dev_autoplay",
            "search_torrents",
            "search_torrents_progressive",
            "cancel_search",
            "prefetch_torrents",
            "fetch_thumbnails",
            "preview_frame",
            "torrent_details",
            "open_link",
            "preload_status",
            "load_queue",
            "save_queue",
            "preload_next",
            "queue_readiness",
            "queue_downloads",
            "preview_file",
            "get_settings",
            "update_settings",
            "set_secret",
            "clear_secret",
            "secret_status",
            "cache_usage",
            "clear_cache",
            "choose_download_dir",
            "network_status",
            "recheck_network",
            "list_interfaces",
            "validate_wireguard_config",
            "import_wireguard_config",
            "check_connection",
            "public_ip",
        ]))
        // We embed the Windows manifest ourselves (below) so test binaries get it too.
        .windows_attributes(tauri_build::WindowsAttributes::new_without_app_manifest());
    if let Err(error) = tauri_build::try_build(attributes) {
        fail(&format!("tauri build step failed: {error:#}"));
    }

    #[cfg(windows)]
    embed_windows_manifest();
}

/// Embeds the Common Controls v6 manifest into every linked target, including tests;
/// see the comment in `windows-app-manifest.xml` for why.
#[cfg(windows)]
fn embed_windows_manifest() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    println!("cargo:rerun-if-changed=windows-app-manifest.xml");
    if let Err(error) =
        embed_resource::compile_for_everything("windows-app-manifest.rc", embed_resource::NONE)
            .manifest_required()
    {
        fail(&format!(
            "failed to embed the Windows app manifest: {error}"
        ));
    }
}

/// Build scripts report failure by panicking; there is no other channel.
#[allow(clippy::panic)]
fn fail(message: &str) -> ! {
    panic!("{message}");
}
