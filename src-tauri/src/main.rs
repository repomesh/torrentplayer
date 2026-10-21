// SPDX-License-Identifier: MIT OR Apache-2.0

//! TorrentPlayer desktop entry point.

#![forbid(unsafe_code)]
// Hide the extra console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> anyhow::Result<()> {
    torrentplayer_app::run()
}
