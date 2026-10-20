// SPDX-License-Identifier: MIT OR Apache-2.0

//! The settings model: every user-facing setting, its default, and its valid range.
//!
//! Loading is lenient (out-of-range values are replaced by defaults, never a crash, see
//! [`Settings::sanitize`]); updates are strict (see [`Settings::validate`]).

use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Current schema version. Bump it and add a step to [`crate::migrate`] when the shape changes.
pub const SCHEMA_VERSION: u32 = 2;

/// Slowest and fastest playback speeds.
pub const SPEED_RANGE: (f64, f64) = (0.5, 3.0);
/// Bandwidth limits are in KiB/s; 0 means unlimited. 1 GiB/s is the largest accepted value.
pub const MAX_RATE_KIB: u32 = 1024 * 1024;
/// Peer limit per torrent.
pub const PEER_LIMIT_RANGE: (u32, u32) = (5, 500);
/// Most queued videos downloading at once that can be chosen; 0 means no limit.
pub const MAX_DOWNLOADS: u32 = 100;
/// Seed ratio target; 0 means "keep seeding while TorrentStream runs".
pub const SEED_RATIO_RANGE: (f64, f64) = (0.0, 100.0);
/// Cache size limit in GB.
pub const CACHE_LIMIT_RANGE_GB: (u32, u32) = (1, 100_000);
/// Automatic clean-up age in days.
pub const CLEANUP_RANGE_DAYS: (u32, u32) = (1, 3650);
/// Results requested per search.
pub const RESULTS_RANGE: (u32, u32) = (10, 500);
/// Longest accepted path, host name, interface name, or label.
pub const MAX_TEXT: usize = 1024;

/// All of TorrentStream's settings. Secrets (passwords, private keys) are never part of this struct;
/// they live in the OS credential store (see [`crate::SecretKey`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", default)]
#[ts(export)]
pub struct Settings {
    /// Schema version, for migrations. Not user-editable.
    pub version: u32,
    /// Playback.
    pub playback: PlaybackSettings,
    /// Download folder and cache.
    pub storage: StorageSettings,
    /// Bandwidth limits and seeding.
    pub bandwidth: BandwidthSettings,
    /// Search sources and filters.
    pub search: SearchSettings,
    /// VPN, proxy, and connection settings.
    pub network: NetworkSettings,
    /// Engine choice and diagnostics.
    pub advanced: AdvancedSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            playback: PlaybackSettings::default(),
            storage: StorageSettings::default(),
            bandwidth: BandwidthSettings::default(),
            search: SearchSettings::default(),
            network: NetworkSettings::default(),
            advanced: AdvancedSettings::default(),
        }
    }
}

/// Playback preferences.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", default)]
#[ts(export)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent on/off preferences, each a switch on the Settings page"
)]
pub struct PlaybackSettings {
    /// Speed new lectures start at (0.5–3).
    pub default_speed: f64,
    /// Play the next lecture when one ends.
    pub autoplay_next: bool,
    /// Resume lectures where they were left off.
    pub resume_position: bool,
    /// Hardware video decoding (used by the libmpv player).
    pub hardware_decoding: HardwareDecoding,
    /// Move queued videos that are ready to play (downloaded, or their start and end are)
    /// ahead of those that are not.
    pub optimize_queue: bool,
    /// When optimizing the queue, keep the order of files within one torrent (lectures).
    pub keep_course_order: bool,
    /// Turn on subtitles in the system language when a video has them.
    pub subtitles_by_default: bool,
}

impl Default for PlaybackSettings {
    fn default() -> Self {
        Self {
            default_speed: 1.0,
            autoplay_next: true,
            resume_position: true,
            hardware_decoding: HardwareDecoding::Auto,
            optimize_queue: false,
            keep_course_order: true,
            subtitles_by_default: false,
        }
    }
}

/// Hardware decoding preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum HardwareDecoding {
    /// Use hardware decoding when it is known to be safe (`hwdec=auto-safe`).
    #[default]
    Auto,
    /// Always decode in software.
    Off,
}

/// Where downloads go and how much space they may use.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", default)]
#[ts(export)]
pub struct StorageSettings {
    /// Download folder; `None` uses TorrentStream's default folder.
    pub download_dir: Option<String>,
    /// Cache size limit in GB; `None` means unlimited.
    pub cache_limit_gb: Option<u32>,
    /// Remove watched data after this many days; `None` keeps it.
    pub cleanup_after_days: Option<u32>,
}

/// Bandwidth limits and seeding behavior.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", default)]
#[ts(export)]
pub struct BandwidthSettings {
    /// Download limit in KiB/s; 0 means unlimited.
    pub download_limit_kib: u32,
    /// Upload limit in KiB/s; 0 means unlimited.
    pub upload_limit_kib: u32,
    /// Keep seeding lectures after watching them.
    pub seed_after_watching: bool,
    /// Stop seeding a torrent at this upload/download ratio; 0 keeps seeding while TorrentStream runs.
    pub seed_ratio_target: f64,
    /// Maximum connected peers per torrent.
    pub peer_limit: u32,
    /// Most queued videos downloading in the background at once; 0 means no limit.
    pub max_downloads: u32,
}

impl Default for BandwidthSettings {
    fn default() -> Self {
        Self {
            download_limit_kib: 0,
            upload_limit_kib: 0,
            seed_after_watching: true,
            seed_ratio_target: 2.0,
            peer_limit: 40,
            max_downloads: 0,
        }
    }
}

/// Search sources and filters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", default)]
#[ts(export)]
pub struct SearchSettings {
    /// Which search providers are used.
    pub sources: SearchSources,
    /// Hide results in adult categories.
    pub hide_adult: bool,
    /// Show only results with a known open license.
    pub open_license_only: bool,
    /// Start loading the best result in the background so it plays at once.
    pub preload_top_result: bool,
    /// Results requested per search.
    pub results_per_search: u32,
}

impl Default for SearchSettings {
    fn default() -> Self {
        Self {
            sources: SearchSources::default(),
            hide_adult: true,
            open_license_only: false,
            preload_top_result: true,
            results_per_search: 100,
        }
    }
}

/// Search providers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", default)]
#[ts(export)]
pub struct SearchSources {
    /// search.emberglow.tech, with all of its indexers.
    pub emberglow: bool,
    /// The Internet Archive (license-aware; not available yet).
    pub internet_archive: bool,
    /// Academic Torrents (not available yet).
    pub academic_torrents: bool,
}

impl Default for SearchSources {
    fn default() -> Self {
        Self {
            emberglow: true,
            internet_archive: false,
            academic_torrents: false,
        }
    }
}

/// How torrent traffic reaches the internet. This is the contract `lumen-net` implements.
///
/// Passwords and private keys are not here: read them with [`crate::SecretStore::get`] using
/// [`crate::SecretKey::Socks5Password`], [`crate::SecretKey::WireGuardPrivateKey`], and
/// [`crate::SecretKey::WireGuardPresharedKey`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", default)]
#[ts(export)]
pub struct NetworkSettings {
    /// Connection mode.
    pub mode: NetworkMode,
    /// Interface to bind to in [`NetworkMode::BindInterface`] (OS interface name).
    pub interface: Option<String>,
    /// SOCKS5 proxy, used in [`NetworkMode::Socks5`].
    pub socks5: Socks5Settings,
    /// Imported WireGuard configuration (non-secret parts), used in [`NetworkMode::WireGuard`].
    pub wireguard: WireGuardSettings,
    /// BitTorrent protocol encryption (MSE/PE). Not applied yet: neither engine implements it.
    pub encryption: EncryptionMode,
    /// What to do with IPv6 when traffic is tunneled or proxied.
    pub ipv6: Ipv6Policy,
    /// Show the public IPv4 and IPv6 addresses in the status bar, asked of icanhazip.com
    /// through the current mode when the connection changes and every 10 minutes.
    pub show_public_ip: bool,
}

impl Default for NetworkSettings {
    fn default() -> Self {
        Self {
            mode: NetworkMode::Direct,
            interface: None,
            socks5: Socks5Settings::default(),
            wireguard: WireGuardSettings::default(),
            encryption: EncryptionMode::Prefer,
            ipv6: Ipv6Policy::Disable,
            show_public_ip: true,
        }
    }
}

/// Connection mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum NetworkMode {
    /// No tunnel or proxy.
    #[default]
    Direct,
    /// Bind all torrent traffic to one network interface (for example a VPN app's adapter).
    BindInterface,
    /// Send traffic through a SOCKS5 proxy.
    Socks5,
    /// TorrentStream's built-in userspace WireGuard tunnel.
    WireGuard,
}

/// A SOCKS5 proxy. The password is stored separately under [`crate::SecretKey::Socks5Password`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", default)]
#[ts(export)]
pub struct Socks5Settings {
    /// Host name or IP address.
    pub host: String,
    /// Port (1–65535).
    pub port: u16,
    /// User name, if the proxy requires authentication.
    pub username: Option<String>,
    /// Resolve tracker host names through the proxy.
    pub proxy_dns: bool,
    /// Send tracker requests through the proxy.
    pub proxy_trackers: bool,
    /// Send DHT traffic through the proxy (needs a proxy with UDP support). Reserved: the DHT
    /// is off in every protected mode until SOCKS5 UDP support exists, whatever this says.
    pub proxy_dht: bool,
}

impl Default for Socks5Settings {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 1080,
            username: None,
            proxy_dns: true,
            proxy_trackers: true,
            proxy_dht: true,
        }
    }
}

/// The non-secret parts of an imported WireGuard configuration. The private key (and preshared
/// key, if any) are stored under [`crate::SecretKey`] entries.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", default)]
#[ts(export)]
pub struct WireGuardSettings {
    /// A name for the configuration (for example the file name).
    pub name: Option<String>,
    /// Tunnel addresses (`[Interface] Address`).
    pub addresses: Vec<String>,
    /// DNS servers inside the tunnel (`[Interface] DNS`).
    pub dns: Vec<String>,
    /// The server's public key (`[Peer] PublicKey`).
    pub peer_public_key: Option<String>,
    /// The server's endpoint, `host:port` (`[Peer] Endpoint`).
    pub peer_endpoint: Option<String>,
    /// Routed ranges (`[Peer] AllowedIPs`).
    pub allowed_ips: Vec<String>,
    /// Keep-alive interval in seconds (`[Peer] PersistentKeepalive`).
    pub persistent_keepalive: Option<u16>,
    /// Tunnel MTU (`[Interface] MTU`).
    pub mtu: Option<u16>,
}

impl WireGuardSettings {
    /// Whether a configuration has been imported.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.peer_endpoint.is_some() && self.peer_public_key.is_some()
    }
}

/// BitTorrent protocol encryption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum EncryptionMode {
    /// Encrypt when the peer supports it.
    #[default]
    Prefer,
    /// Only talk to peers that encrypt.
    Require,
    /// Never encrypt.
    Off,
}

/// IPv6 handling when traffic is tunneled or proxied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum Ipv6Policy {
    /// Send IPv6 through the tunnel too (only if the tunnel carries IPv6).
    Tunnel,
    /// Use IPv4 only, so IPv6 cannot bypass the tunnel.
    #[default]
    Disable,
}

/// Engine choice and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", default)]
#[ts(export)]
pub struct AdvancedSettings {
    /// Torrent engine for new torrents. The `LUMEN_ENGINE` environment variable overrides it.
    pub engine: EngineSetting,
    /// Allow the D key to show the debug overlay.
    pub debug_overlay: bool,
    /// Log detail. The `RUST_LOG` environment variable overrides it.
    pub log_level: LogLevel,
}

impl Default for AdvancedSettings {
    fn default() -> Self {
        Self {
            engine: EngineSetting::Auto,
            debug_overlay: true,
            log_level: LogLevel::Info,
        }
    }
}

/// Torrent engine choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum EngineSetting {
    /// TorrentStream's engine for every source. librqbit resolves magnet metadata only.
    #[default]
    Auto,
    /// Always TorrentStream's own engine. For magnet links, librqbit fetches the metadata only.
    Swarm,
    /// Always librqbit.
    Rqbit,
}

/// Log detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum LogLevel {
    /// Errors only.
    Error,
    /// Warnings and errors.
    Warn,
    /// Normal.
    #[default]
    Info,
    /// Detailed.
    Debug,
    /// Everything.
    Trace,
}

impl LogLevel {
    /// The `tracing` filter directive for this level.
    #[must_use]
    pub fn directive(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

/// A setting that failed validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct FieldError {
    /// Dotted path of the setting, for example `bandwidth.peerLimit`.
    pub field: String,
    /// What is wrong, written for users.
    pub message: String,
}

impl FieldError {
    fn new(field: &str, message: impl Into<String>) -> Self {
        Self {
            field: field.to_owned(),
            message: message.into(),
        }
    }
}

impl Settings {
    /// Checks every setting. Returns every problem found, not just the first.
    ///
    /// # Errors
    ///
    /// Returns the list of invalid settings.
    pub fn validate(&self) -> Result<(), Vec<FieldError>> {
        let errors = self.problems();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Replaces invalid values with their defaults, returning a description of each fix. Used
    /// when loading a file from disk, which may have been edited by hand or by a newer version.
    pub fn sanitize(&mut self) -> Vec<String> {
        let defaults = Self::default();
        let mut fixes = Vec::new();
        for problem in self.problems() {
            let reset = match problem.field.as_str() {
                "playback.defaultSpeed" => {
                    self.playback.default_speed = defaults.playback.default_speed;
                    true
                }
                "storage.downloadDir" => {
                    self.storage.download_dir = None;
                    true
                }
                "storage.cacheLimitGb" => {
                    self.storage.cache_limit_gb = None;
                    true
                }
                "storage.cleanupAfterDays" => {
                    self.storage.cleanup_after_days = None;
                    true
                }
                "bandwidth.downloadLimitKib" => {
                    self.bandwidth.download_limit_kib = 0;
                    true
                }
                "bandwidth.uploadLimitKib" => {
                    self.bandwidth.upload_limit_kib = 0;
                    true
                }
                "bandwidth.seedRatioTarget" => {
                    self.bandwidth.seed_ratio_target = defaults.bandwidth.seed_ratio_target;
                    true
                }
                "bandwidth.peerLimit" => {
                    self.bandwidth.peer_limit = defaults.bandwidth.peer_limit;
                    true
                }
                "bandwidth.maxDownloads" => {
                    self.bandwidth.max_downloads = 0;
                    true
                }
                "search.resultsPerSearch" => {
                    self.search.results_per_search = defaults.search.results_per_search;
                    true
                }
                "network.interface" => {
                    self.network.interface = None;
                    true
                }
                "network.socks5.host" => {
                    self.network.socks5.host.clear();
                    true
                }
                "network.socks5.port" => {
                    self.network.socks5.port = defaults.network.socks5.port;
                    true
                }
                "network.socks5.username" => {
                    self.network.socks5.username = None;
                    true
                }
                field if field.starts_with("network.wireguard") => {
                    self.network.wireguard = WireGuardSettings::default();
                    true
                }
                _ => false,
            };
            if reset {
                fixes.push(format!("{} was invalid and was reset", problem.field));
            }
        }
        self.version = SCHEMA_VERSION;
        fixes
    }

    /// Settings that are valid but incomplete or risky, shown to the user as hints.
    #[must_use]
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        let net = &self.network;
        match net.mode {
            NetworkMode::BindInterface if net.interface.is_none() => {
                warnings.push("Choose the network interface to bind to.".to_owned());
            }
            NetworkMode::Socks5 if net.socks5.host.is_empty() => {
                warnings.push("Enter the SOCKS5 proxy's host and port.".to_owned());
            }
            NetworkMode::WireGuard if !net.wireguard.is_configured() => {
                warnings.push("Import a WireGuard configuration.".to_owned());
            }
            _ => {}
        }
        warnings
    }

    fn problems(&self) -> Vec<FieldError> {
        let mut errors = Vec::new();
        self.playback.check(&mut errors);
        self.storage.check(&mut errors);
        self.bandwidth.check(&mut errors);
        if !(RESULTS_RANGE.0..=RESULTS_RANGE.1).contains(&self.search.results_per_search) {
            errors.push(FieldError::new(
                "search.resultsPerSearch",
                "Choose between 10 and 500 results.",
            ));
        }
        self.network.check(&mut errors);
        errors
    }
}

impl PlaybackSettings {
    fn check(&self, errors: &mut Vec<FieldError>) {
        if !in_range(self.default_speed, SPEED_RANGE) {
            errors.push(FieldError::new(
                "playback.defaultSpeed",
                "Choose a speed between 0.5× and 3×.",
            ));
        }
    }
}

impl StorageSettings {
    fn check(&self, errors: &mut Vec<FieldError>) {
        if let Some(dir) = &self.download_dir
            && let Err(message) = check_download_dir(dir)
        {
            errors.push(FieldError::new("storage.downloadDir", message));
        }
        if let Some(gb) = self.cache_limit_gb
            && !(CACHE_LIMIT_RANGE_GB.0..=CACHE_LIMIT_RANGE_GB.1).contains(&gb)
        {
            errors.push(FieldError::new(
                "storage.cacheLimitGb",
                "Choose a limit between 1 GB and 100 000 GB, or unlimited.",
            ));
        }
        if let Some(days) = self.cleanup_after_days
            && !(CLEANUP_RANGE_DAYS.0..=CLEANUP_RANGE_DAYS.1).contains(&days)
        {
            errors.push(FieldError::new(
                "storage.cleanupAfterDays",
                "Choose between 1 and 3650 days, or never.",
            ));
        }
    }
}

impl BandwidthSettings {
    fn check(&self, errors: &mut Vec<FieldError>) {
        for (field, value) in [
            ("bandwidth.downloadLimitKib", self.download_limit_kib),
            ("bandwidth.uploadLimitKib", self.upload_limit_kib),
        ] {
            if value > MAX_RATE_KIB {
                errors.push(FieldError::new(field, "The largest limit is 1 GiB/s."));
            }
        }
        if !in_range(self.seed_ratio_target, SEED_RATIO_RANGE) {
            errors.push(FieldError::new(
                "bandwidth.seedRatioTarget",
                "Choose a ratio between 0 and 100 (0 keeps seeding).",
            ));
        }
        if !(PEER_LIMIT_RANGE.0..=PEER_LIMIT_RANGE.1).contains(&self.peer_limit) {
            errors.push(FieldError::new(
                "bandwidth.peerLimit",
                "Choose between 5 and 500 peers.",
            ));
        }
        if self.max_downloads > MAX_DOWNLOADS {
            errors.push(FieldError::new(
                "bandwidth.maxDownloads",
                "Choose up to 100 downloads, or 0 for no limit.",
            ));
        }
    }
}

impl NetworkSettings {
    fn check(&self, errors: &mut Vec<FieldError>) {
        if let Some(name) = &self.interface
            && !is_label(name)
        {
            errors.push(FieldError::new(
                "network.interface",
                "Interface names cannot be empty or contain control characters.",
            ));
        }
        let proxy = &self.socks5;
        if !proxy.host.is_empty() && !is_host(&proxy.host) {
            errors.push(FieldError::new(
                "network.socks5.host",
                "Enter a host name or IP address without a scheme or port (for example 10.0.0.1).",
            ));
        }
        if proxy.port == 0 {
            errors.push(FieldError::new(
                "network.socks5.port",
                "Choose a port between 1 and 65535.",
            ));
        }
        // RFC 1929 limits user names to 255 bytes.
        if let Some(user) = &proxy.username
            && (!is_label(user) || user.len() > 255)
        {
            errors.push(FieldError::new(
                "network.socks5.username",
                "User names are 1–255 bytes without control characters.",
            ));
        }
        let wg = &self.wireguard;
        let mut texts = wg
            .name
            .iter()
            .chain(&wg.addresses)
            .chain(&wg.dns)
            .chain(&wg.peer_public_key)
            .chain(&wg.peer_endpoint)
            .chain(&wg.allowed_ips);
        if texts.any(|t| !is_label(t))
            || wg.addresses.len() > 16
            || wg.dns.len() > 16
            || wg.allowed_ips.len() > 256
        {
            errors.push(FieldError::new(
                "network.wireguard",
                "The imported WireGuard configuration is invalid; import it again.",
            ));
        }
    }
}

/// A finite number within `range`, inclusive.
fn in_range(value: f64, range: (f64, f64)) -> bool {
    value.is_finite() && value >= range.0 && value <= range.1
}

/// Non-empty, at most [`MAX_TEXT`] bytes, and no control characters.
fn is_label(text: &str) -> bool {
    !text.trim().is_empty() && text.len() <= MAX_TEXT && !text.chars().any(char::is_control)
}

/// A host name or IP address (no scheme, path, port, spaces, or brackets).
fn is_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
}

// Checks a user-chosen download folder.
fn check_download_dir(dir: &str) -> Result<(), String> {
    if dir.is_empty() || dir.len() > MAX_TEXT || dir.chars().any(char::is_control) {
        return Err("Enter a folder path.".to_owned());
    }
    if dir.starts_with(r"\\?\") || dir.starts_with(r"\\.\") {
        return Err("Device paths are not supported; choose a normal folder.".to_owned());
    }
    let path = Path::new(dir);
    if !path.is_absolute() {
        return Err(
            "Enter a full path (for example D:\\Lectures or /home/me/Lectures).".to_owned(),
        );
    }
    let mut normal = 0;
    for component in path.components() {
        match component {
            Component::Normal(_) => normal += 1,
            Component::ParentDir | Component::CurDir => {
                return Err("The path cannot contain \".\" or \"..\".".to_owned());
            }
            Component::RootDir | Component::Prefix(_) => {}
        }
    }
    if normal == 0 {
        return Err("Choose a folder, not the root of a drive.".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_settings_default_for_older_files() {
        // A playback section saved before the queue options existed.
        let old: PlaybackSettings =
            serde_json::from_str(r#"{"defaultSpeed": 1.5, "autoplayNext": false}"#).unwrap();
        assert!(!old.optimize_queue, "off unless chosen");
        assert!(old.keep_course_order);
        assert!((old.default_speed - 1.5).abs() < f64::EPSILON);
        let json = serde_json::to_value(PlaybackSettings::default()).unwrap();
        assert_eq!(json["optimizeQueue"], false);
        assert_eq!(json["keepCourseOrder"], true);
    }

    #[test]
    fn subtitles_are_off_by_default_for_older_files() {
        let old: PlaybackSettings = serde_json::from_str(r#"{"defaultSpeed": 1.25}"#).unwrap();
        assert!(!old.subtitles_by_default);
        let json = serde_json::to_value(PlaybackSettings::default()).unwrap();
        assert_eq!(json["subtitlesByDefault"], false);
    }

    #[test]
    fn defaults_are_valid_and_conservative() {
        let s = Settings::default();
        assert_eq!(s.validate(), Ok(()));
        assert_eq!(s.network.mode, NetworkMode::Direct);
        assert!(s.search.sources.emberglow && s.search.hide_adult);
        assert!(!s.search.open_license_only);
        assert_eq!(s.bandwidth.download_limit_kib, 0);
        assert!(s.warnings().is_empty());
    }

    #[test]
    fn reports_every_invalid_field() {
        let mut s = Settings::default();
        s.playback.default_speed = 9.0;
        s.bandwidth.peer_limit = 1;
        s.bandwidth.seed_ratio_target = f64::NAN;
        s.search.results_per_search = 5;
        s.network.socks5.port = 0;
        let fields: Vec<String> = s
            .validate()
            .unwrap_err()
            .into_iter()
            .map(|e| e.field)
            .collect();
        assert_eq!(
            fields,
            [
                "playback.defaultSpeed",
                "bandwidth.seedRatioTarget",
                "bandwidth.peerLimit",
                "search.resultsPerSearch",
                "network.socks5.port"
            ]
        );
    }

    #[test]
    fn sanitize_resets_only_the_bad_values() {
        let mut s = Settings::default();
        s.bandwidth.peer_limit = 100_000;
        s.bandwidth.download_limit_kib = 512;
        s.storage.download_dir = Some("relative/path".into());
        let fixes = s.sanitize();
        assert_eq!(fixes.len(), 2, "{fixes:?}");
        assert_eq!(s.bandwidth.peer_limit, 40);
        assert_eq!(s.bandwidth.download_limit_kib, 512, "valid values are kept");
        assert_eq!(s.storage.download_dir, None);
        assert_eq!(s.validate(), Ok(()));
    }

    #[test]
    fn download_folder_rules() {
        let root = if cfg!(windows) { r"C:\" } else { "/" };
        let good = if cfg!(windows) {
            r"D:\Lectures"
        } else {
            "/home/me/Lectures"
        };
        let parent = if cfg!(windows) {
            r"C:\a\..\b"
        } else {
            "/a/../b"
        };
        assert!(check_download_dir(good).is_ok());
        assert!(check_download_dir(root).is_err());
        assert!(check_download_dir("Lectures").is_err());
        assert!(check_download_dir(parent).is_err());
        assert!(check_download_dir(r"\\.\PhysicalDrive0").is_err());
        assert!(check_download_dir("").is_err());
    }

    #[test]
    fn hosts_and_labels() {
        for ok in ["10.0.0.1", "proxy.example.org", "::1", "a-b.c"] {
            assert!(is_host(ok), "{ok}");
        }
        for bad in [
            "",
            "socks5://x",
            "host:1080",
            "a b",
            "-a.com",
            "a..b",
            "[::1]",
        ] {
            assert!(!is_host(bad), "{bad}");
        }
        assert!(is_label("wg0"));
        assert!(!is_label("  "));
        assert!(!is_label("a\nb"));
    }

    #[test]
    fn warnings_explain_incomplete_network_modes() {
        let mut s = Settings::default();
        s.network.mode = NetworkMode::Socks5;
        assert_eq!(s.warnings().len(), 1);
        s.network.socks5.host = "10.0.0.1".into();
        assert!(s.warnings().is_empty());
    }

    #[test]
    fn serializes_as_camel_case() {
        let json = serde_json::to_value(Settings::default()).unwrap();
        assert_eq!(json["network"]["mode"], "direct");
        assert_eq!(json["bandwidth"]["peerLimit"], 40);
        assert_eq!(json["bandwidth"]["maxDownloads"], 0, "no limit by default");
        assert_eq!(json["advanced"]["logLevel"], "info");
        let mode: NetworkMode = serde_json::from_value("wireGuard".into()).unwrap();
        assert_eq!(mode, NetworkMode::WireGuard);
    }
}
