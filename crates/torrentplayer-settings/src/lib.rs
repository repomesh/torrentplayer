// SPDX-License-Identifier: MIT OR Apache-2.0

//! TorrentPlayer's user settings.
//!
//! - [`Settings`]: every setting with its default and valid range, stored as versioned JSON
//!   (`settings.json` in the app config folder) by [`SettingsStore`]. Loading never fails; bad
//!   files fall back to defaults with a warning.
//! - [`apply_patch`]: validated partial updates from the Settings page.
//! - [`SecretStore`]: proxy passwords and WireGuard keys, kept in the OS credential store and
//!   never in the JSON file.
//!
//! [`NetworkSettings`] is the contract with `torrentplayer-net`: it describes the chosen connection
//! mode, and `torrentplayer-net` turns it (plus the secrets) into a running configuration.

#![forbid(unsafe_code)]

mod model;
mod patch;
mod secrets;
mod store;

pub use model::{
    AdvancedSettings, BandwidthSettings, CACHE_LIMIT_RANGE_GB, CLEANUP_RANGE_DAYS, EncryptionMode,
    EngineSetting, FieldError, HardwareDecoding, IncomingPeersSettings, Ipv6Policy,
    LISTEN_PORT_MIN, LogLevel, MAX_DOWNLOADS, MAX_RATE_KIB, MAX_TEXT, NetworkHold, NetworkMode,
    NetworkSettings, PEER_LIMIT_RANGE, PlaybackSettings, RESULTS_RANGE, SCHEMA_VERSION,
    SPEED_RANGE, SearchSettings, SearchSources, Settings, Socks5Settings, StorageSettings,
    WireGuardSettings, is_network_path,
};
pub use patch::{MAX_PATCH_BYTES, PatchError, apply_patch};
pub use secrets::{
    KEYRING_SERVICE, KeyringStore, LEGACY_KEYRING_SERVICE, MemoryStore, SecretError, SecretKey,
    SecretStatus, SecretStore,
};
pub use store::{
    FILE_NAME, INVALID_FILE_NAME, Loaded, NEWER_FILE_NAME, SettingsStore, StoreError, migrate,
};

/// Re-exported so dependents can hold, read, and wipe secrets without depending on `secrecy`.
pub use secrecy::zeroize::Zeroize;
pub use secrecy::{ExposeSecret, SecretString};
