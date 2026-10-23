// SPDX-License-Identifier: MIT OR Apache-2.0

//! The network commands, and the wiring that keeps TorrentPlayer's network manager (`torrentplayer-net`) in line
//! with the saved network settings.
//!
//! The manager starts with the saved settings before anything connects, then follows every
//! change to the network settings or their secrets: saving the settings applies them, so there
//! is no separate "apply" command. Status changes go to the webview as [`NETWORK_STATUS`]
//! events, and the public IPv4 and IPv6 addresses (for the status bar, unless turned off) as
//! [`PUBLIC_IP`] events. See `docs/network.md` and the network section of `docs/ipc.md`.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use serde::Serialize;
use tauri::{AppHandle, Emitter as _, Manager as _, State};
use tokio::sync::watch;
use torrentplayer_net::wireguard::{WgInterface, WgPeer};
use torrentplayer_net::{
    IncomingConfig, NetError, Network, NetworkConfig, NetworkMode, NetworkOptions, NetworkStatus,
    PublicIp, Secret, Socks5Config, WireGuardConfig, WireGuardSummary,
};
use torrentplayer_settings::{
    ExposeSecret as _, Ipv6Policy, NetworkHold, NetworkSettings, SecretError, SecretKey,
    SecretStatus, SecretStore, SecretString, Settings, WireGuardSettings, Zeroize as _,
};
use ts_rs::TS;

use crate::core::AppState;
use crate::error::{ErrorKind, IpcError};
use crate::settings::{SettingsState, SettingsView};

/// Event sent to the webview, with the new [`NetworkStatus`], whenever the status changes.
pub(crate) const NETWORK_STATUS: &str = "network-status";
/// How long [`recheck_network`] waits for the check's result.
const RECHECK_WAIT: Duration = Duration::from_secs(6);
/// Event sent to the webview with the public addresses (`PublicIp`, or `null` while unknown,
/// blocked, or turned off).
pub(crate) const PUBLIC_IP: &str = "network-public-ip";
/// How often the status bar's public addresses are asked again while nothing changes.
const PUBLIC_IP_EVERY: Duration = Duration::from_secs(10 * 60);
/// Least time between two automatic checks, so a flapping connection cannot cause a burst.
const PUBLIC_IP_GAP: Duration = Duration::from_secs(5);
/// Give up on one check after this long.
const PUBLIC_IP_TIMEOUT: Duration = Duration::from_secs(25);
/// Why traffic is held after settings that used the removed VPN adapter mode were moved to
/// Direct, until the user confirms (see [`config_from_settings`]).
pub(crate) const VPN_ADAPTER_HOLD: &str =
    "the VPN adapter mode was removed; confirm the change in Settings → Network";
/// Why traffic is held after `settings.json` (or its network settings) could not be read, so
/// the mode the user chose is unknown, until the user confirms a mode.
pub(crate) const SETTINGS_UNREADABLE_HOLD: &str =
    "the network settings could not be read; choose a connection in Settings → VPN and network";

/// Managed state: the network manager.
#[derive(Clone)]
pub(crate) struct NetworkState {
    network: Network,
    /// The latest public addresses, for the status bar.
    public_ip: Arc<Mutex<Option<PublicIp>>>,
}

impl NetworkState {
    pub(crate) fn new(network: Network) -> Self {
        Self {
            network,
            public_ip: Arc::new(Mutex::new(None)),
        }
    }

    /// Stores the latest public addresses and tells the webview, if they changed.
    fn set_public_ip(&self, app: &AppHandle, ip: Option<PublicIp>) {
        let mut slot = self
            .public_ip
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if *slot == ip {
            return;
        }
        slot.clone_from(&ip);
        drop(slot);
        if let Err(error) = app.emit(PUBLIC_IP, ip) {
            tracing::warn!(%error, "could not send the public-IP event");
        }
    }
}

/// Keeps the status bar's public addresses current, for as long as the app runs: asks again
/// when the connection is allowed again or its mode or epoch changes, and every
/// [`PUBLIC_IP_EVERY`]; forgets them while traffic is blocked or the setting is off, so a stale
/// address is never shown as current. Every request goes through the current mode.
pub(crate) fn watch_public_ip(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<NetworkState>().inner().clone();
        let mut settings = app.state::<SettingsState>().subscribe();
        let mut policy = state.network.subscribe();
        let mut asked_for: Option<(u64, bool)> = None;
        let mut last_check: Option<Instant> = None;
        loop {
            let enabled = settings.borrow_and_update().network.show_public_ip;
            let current = *policy.borrow_and_update();
            if !enabled || !current.traffic_allowed {
                state.set_public_ip(&app, None);
                asked_for = None;
            } else {
                let changed = asked_for != Some((current.epoch, enabled));
                let due = last_check.is_none_or(|t| t.elapsed() >= PUBLIC_IP_EVERY);
                let rested = last_check.is_none_or(|t| t.elapsed() >= PUBLIC_IP_GAP);
                if (changed || due) && rested {
                    asked_for = Some((current.epoch, enabled));
                    last_check = Some(Instant::now());
                    let result =
                        tokio::time::timeout(PUBLIC_IP_TIMEOUT, state.network.check_public_ip())
                            .await;
                    match result {
                        Ok(Ok(ip)) => state.set_public_ip(&app, Some(ip)),
                        Ok(Err(error)) => {
                            tracing::debug!(%error, "public IP check failed");
                            state.set_public_ip(&app, None);
                        }
                        Err(_) => state.set_public_ip(&app, None),
                    }
                    continue;
                }
            }
            // Wake for the next periodic check, the end of the gap, or any change.
            let wait = match last_check {
                Some(t) if asked_for.is_some() => PUBLIC_IP_EVERY.saturating_sub(t.elapsed()),
                Some(t) => PUBLIC_IP_GAP.saturating_sub(t.elapsed()),
                None => PUBLIC_IP_EVERY,
            }
            .max(Duration::from_millis(200));
            tokio::select! {
                changed = settings.changed() => if changed.is_err() { return },
                changed = policy.changed() => if changed.is_err() { return },
                () = tokio::time::sleep(wait) => {}
            }
        }
    });
}

/// The latest public IPv4 and IPv6 addresses for the status bar (`null` while unknown,
/// blocked, or turned off). Updates arrive as [`PUBLIC_IP`] events.
#[tauri::command]
#[allow(
    clippy::needless_pass_by_value,
    reason = "Tauri passes managed state by value"
)]
pub(crate) fn public_ip(state: State<'_, NetworkState>) -> Option<PublicIp> {
    state
        .public_ip
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Starts the network manager with the saved settings. Reads secrets from the OS credential
/// store, so call it once during setup, before anything connects.
pub(crate) fn start(settings: &Settings, secrets: &dyn SecretStore) -> anyhow::Result<Network> {
    let config = config_from_settings(&settings.network, secrets);
    let network = tauri::async_runtime::block_on(Network::start(config, NetworkOptions::default()))
        .context("could not start the network manager")?;
    network.set_incoming(incoming_from_settings(&settings.network));
    Ok(network)
}

/// Applies network settings changes and sends status changes to the webview, for as long as
/// the app runs.
pub(crate) fn follow(
    app: &AppHandle,
    network: &Network,
    mut settings: watch::Receiver<Settings>,
    secrets: Arc<dyn SecretStore>,
) {
    let (handle, net) = (app.clone(), network.clone());
    tauri::async_runtime::spawn(async move {
        // Secret changes notify without changing the value, so every notification rebuilds the
        // configuration; applying an unchanged one does nothing.
        while settings.changed().await.is_ok() {
            let network_settings = settings.borrow_and_update().network.clone();
            let incoming = incoming_from_settings(&network_settings);
            let store = Arc::clone(&secrets);
            let built = tauri::async_runtime::spawn_blocking(move || {
                config_from_settings(&network_settings, store.as_ref())
            })
            .await;
            match built {
                Ok(config) => apply(&handle, &net, config, incoming).await,
                Err(error) => tracing::error!(%error, "could not read the network settings"),
            }
        }
    });
    let (handle, net) = (app.clone(), network.clone());
    tauri::async_runtime::spawn(async move {
        let mut changes = net.status_changes();
        loop {
            if let Err(error) = handle.emit(NETWORK_STATUS, net.status()) {
                tracing::warn!(%error, "could not send the network-status event");
            }
            if changes.changed().await.is_err() {
                return;
            }
        }
    });
}

/// Switches modes, and then the incoming-connection listener, through the stream engine when
/// it is running, so librqbit is replaced before it could send anything under the wrong mode or
/// listen where it should not (see `StreamEngine::apply_network` and
/// `StreamEngine::set_incoming`).
async fn apply(
    app: &AppHandle,
    network: &Network,
    config: NetworkConfig,
    incoming: IncomingConfig,
) {
    let core = app
        .try_state::<AppState>()
        .and_then(|state| state.started().cloned());
    let result = match &core {
        Some(core) => core.engine.apply_network(config).await,
        None => network.apply(config),
    };
    if let Err(error) = result {
        tracing::warn!(%error, "could not apply the network settings");
    }
    match &core {
        Some(core) => core.engine.set_incoming(incoming).await,
        None => network.set_incoming(incoming),
    }
}

/// Whether and where to accept incoming peer connections. The network manager decides which
/// modes can listen (never SOCKS5) and where (see `docs/network.md`).
pub(crate) fn incoming_from_settings(settings: &NetworkSettings) -> IncomingConfig {
    let s = &settings.incoming_peers;
    IncomingConfig {
        enabled: s.enabled,
        port: s.port,
        upnp: s.upnp,
    }
}

/// The network configuration for `settings`, with the proxy password read from the credential
/// store. Settings that cannot work (a missing host, a username without its password) still
/// select their mode, which then blocks traffic and says why: TorrentPlayer never falls back to a
/// direct connection.
///
/// Settings moved from the removed VPN adapter mode to Direct hold all traffic
/// ([`VPN_ADAPTER_HOLD`]) until the user dismisses the notice or picks another mode: the user
/// chose to send nothing outside their VPN, so nothing is sent before they confirm the change.
/// Settings that could not be read ([`NetworkHold::SettingsUnreadable`]) load as Direct and are
/// held the same way ([`SETTINGS_UNREADABLE_HOLD`]): the mode the user chose is unknown.
///
/// There is no kill-switch setting: every mode except direct always fails closed. `proxy_dht`
/// controls DHT through SOCKS5 UDP ASSOCIATE, without a direct fallback. `encryption` is not
/// implemented by either engine.
pub(crate) fn config_from_settings(
    settings: &NetworkSettings,
    secrets: &dyn SecretStore,
) -> NetworkConfig {
    let mode = match settings.mode {
        torrentplayer_settings::NetworkMode::Direct => NetworkMode::Direct,
        torrentplayer_settings::NetworkMode::Socks5 => {
            let s = &settings.socks5;
            let username = s.username.clone().filter(|u| !u.is_empty());
            let password = username
                .as_ref()
                .and_then(|_| read_secret(secrets, SecretKey::Socks5Password));
            NetworkMode::Socks5(Socks5Config {
                host: s.host.clone(),
                port: s.port,
                username,
                password,
                remote_dns: s.proxy_dns,
                proxy_trackers: s.proxy_trackers,
                proxy_dht: s.proxy_dht,
            })
        }
        torrentplayer_settings::NetworkMode::WireGuard => {
            NetworkMode::WireGuard(wireguard(&settings.wireguard, secrets))
        }
    };
    let hold = match settings.hold {
        _ if mode != NetworkMode::Direct => None,
        Some(NetworkHold::VpnAdapterModeRemoved) => Some(VPN_ADAPTER_HOLD.to_owned()),
        Some(NetworkHold::SettingsUnreadable) => Some(SETTINGS_UNREADABLE_HOLD.to_owned()),
        None => None,
    };
    NetworkConfig {
        mode,
        allow_ipv6: settings.ipv6 == Ipv6Policy::Tunnel,
        hold,
    }
}

fn read_secret(secrets: &dyn SecretStore, key: SecretKey) -> Option<Secret> {
    match secrets.get(key) {
        Ok(value) => value.map(|v| Secret::new(v.expose_secret())),
        Err(error) => {
            tracing::warn!(%error, ?key, "could not read a network secret");
            None
        }
    }
}

/// Rebuilds the imported configuration with keys read only inside the backend.
fn wireguard(s: &WireGuardSettings, secrets: &dyn SecretStore) -> WireGuardConfig {
    let peers = match (&s.peer_public_key, &s.peer_endpoint) {
        (Some(public_key), Some(endpoint)) => vec![WgPeer {
            public_key: public_key.clone(),
            preshared_key: read_secret(secrets, SecretKey::WireGuardPresharedKey),
            allowed_ips: s.allowed_ips.clone(),
            endpoint: endpoint.clone(),
            persistent_keepalive: s.persistent_keepalive,
        }],
        _ => Vec::new(),
    };
    WireGuardConfig {
        interface: WgInterface {
            private_key: read_secret(secrets, SecretKey::WireGuardPrivateKey).unwrap_or_default(),
            addresses: s.addresses.clone(),
            dns: s.dns.clone(),
            mtu: s.mtu,
            listen_port: None,
        },
        peers,
    }
}

impl From<NetError> for IpcError {
    fn from(error: NetError) -> Self {
        // torrentplayer-net messages are written for users and contain no secrets.
        let kind = match error {
            NetError::KillSwitch(_) => ErrorKind::KillSwitch,
            NetError::InvalidConfig(_) => ErrorKind::InvalidArgument,
            _ => ErrorKind::NetworkUnavailable,
        };
        Self::new(kind, capitalize(&error.to_string()))
    }
}

fn capitalize(message: &str) -> String {
    let mut chars = message.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect::<String>() + "."
    })
}

/// The network manager's state and recent events.
#[tauri::command]
#[allow(
    clippy::needless_pass_by_value,
    reason = "Tauri passes managed state by value"
)]
pub(crate) fn network_status(state: State<'_, NetworkState>) -> NetworkStatus {
    state.network.status()
}

/// Checks the proxy or WireGuard tunnel again now instead of at the next interval (for a "Try
/// again" button while the kill switch is engaged), and returns the status after the check.
#[tauri::command]
pub(crate) async fn recheck_network(
    state: State<'_, NetworkState>,
) -> Result<NetworkStatus, IpcError> {
    let mut changes = state.network.subscribe();
    changes.borrow_and_update();
    state.network.recheck();
    let _ = tokio::time::timeout(RECHECK_WAIT, changes.changed()).await;
    Ok(state.network.status())
}

/// Parses and checks the text of a WireGuard configuration file, returning what it contains
/// without its keys. Nothing is saved: the settings import stores the parts it needs.
#[tauri::command]
pub(crate) fn validate_wireguard_config(mut text: String) -> Result<WireGuardSummary, IpcError> {
    let parsed = WireGuardConfig::parse(&text);
    // The text contains the private key.
    text.zeroize();
    match parsed {
        Ok((config, warnings)) => {
            NetworkMode::WireGuard(config.clone())
                .validate()
                .map_err(IpcError::from)?;
            Ok(config.summary(warnings))
        }
        Err(error) => Err(IpcError::invalid(format!(
            "This WireGuard configuration is invalid: {error}."
        ))),
    }
}

/// What [`import_wireguard_config`] saved.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct WireGuardImport {
    /// The imported configuration, without its keys.
    pub(crate) summary: WireGuardSummary,
    /// The saved settings.
    pub(crate) view: SettingsView,
    /// Which secrets are set now.
    pub(crate) secrets: SecretStatus,
}

/// A parsed configuration, split into what goes into the settings file and the keys.
struct ParsedImport {
    settings: WireGuardSettings,
    private_key: SecretString,
    preshared_key: Option<SecretString>,
    summary: WireGuardSummary,
}

fn parse_import(text: &str, name: Option<String>) -> Result<ParsedImport, IpcError> {
    let (config, warnings) = WireGuardConfig::parse(text).map_err(|error| {
        IpcError::invalid(format!("This WireGuard configuration is invalid: {error}."))
    })?;
    let [peer] = config.peers.as_slice() else {
        return Err(IpcError::invalid(format!(
            "This configuration has {} [Peer] sections; TorrentPlayer supports exactly one.",
            config.peers.len()
        )));
    };
    NetworkMode::WireGuard(config.clone())
        .validate()
        .map_err(IpcError::from)?;
    let key_error = |e: SecretError| IpcError::invalid(e.to_string());
    let private_key = SecretKey::WireGuardPrivateKey
        .prepare(config.interface.private_key.expose())
        .map_err(key_error)?;
    let preshared_key = peer
        .preshared_key
        .as_ref()
        .map(|key| SecretKey::WireGuardPresharedKey.prepare(key.expose()))
        .transpose()
        .map_err(key_error)?;
    let settings = WireGuardSettings {
        name: name.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()),
        addresses: config.interface.addresses.clone(),
        dns: config.interface.dns.clone(),
        peer_public_key: Some(peer.public_key.clone()),
        peer_endpoint: Some(peer.endpoint.clone()),
        allowed_ips: peer.allowed_ips.clone(),
        persistent_keepalive: peer.persistent_keepalive,
        mtu: config.interface.mtu,
    };
    Ok(ParsedImport {
        settings,
        private_key,
        preshared_key,
        summary: config.summary(warnings),
    })
}

/// Imports a WireGuard configuration file (`text`; `name`, for example the file name, labels
/// it): stores its private key, and preshared key if any, in the OS credential store, and saves
/// the rest in `network.wireguard`. Nothing is saved if the file is invalid. The keys are never
/// sent back.
#[tauri::command]
pub(crate) async fn import_wireguard_config(
    app: AppHandle,
    settings: State<'_, SettingsState>,
    mut text: String,
    name: Option<String>,
) -> Result<WireGuardImport, IpcError> {
    let parsed = parse_import(&text, name);
    // The text contains the private key.
    text.zeroize();
    let parsed = parsed?;
    let (view, secrets) = settings
        .import_wireguard(
            &app,
            &parsed.settings,
            parsed.private_key,
            parsed.preshared_key,
        )
        .await?;
    tracing::info!(
        has_preshared_key = secrets.wire_guard_preshared_key,
        "imported a WireGuard configuration"
    );
    Ok(WireGuardImport {
        summary: parsed.summary,
        view,
        secrets,
    })
}

/// Asks the connection-check service (icanhazip.com, see `torrentplayer_net::IP_CHECK_V4_URL`) for the
/// public IPv4 and IPv6 addresses, through the current mode, now. While the kill switch is
/// engaged it fails without sending anything. The answer also updates the status bar.
#[tauri::command]
pub(crate) async fn check_connection(
    app: AppHandle,
    state: State<'_, NetworkState>,
) -> Result<PublicIp, IpcError> {
    let ip = state
        .network
        .check_public_ip()
        .await
        .map_err(IpcError::from)?;
    if app
        .state::<SettingsState>()
        .current()
        .network
        .show_public_ip
    {
        state.set_public_ip(&app, Some(ip.clone()));
    }
    Ok(ip)
}

#[cfg(test)]
mod tests {
    use torrentplayer_settings::{ExposeSecret, MemoryStore};

    use super::*;

    fn socks_settings(username: Option<&str>) -> NetworkSettings {
        let mut s = NetworkSettings {
            mode: torrentplayer_settings::NetworkMode::Socks5,
            ..NetworkSettings::default()
        };
        s.socks5.host = "10.0.0.1".into();
        s.socks5.port = 9050;
        s.socks5.username = username.map(Into::into);
        s
    }

    #[test]
    fn incoming_peers_map_to_the_listener_settings() {
        assert_eq!(
            incoming_from_settings(&NetworkSettings::default()),
            IncomingConfig::default(),
            "off unless chosen"
        );
        let mut s = NetworkSettings::default();
        s.incoming_peers.enabled = true;
        s.incoming_peers.port = 51413;
        s.incoming_peers.upnp = true;
        assert_eq!(
            incoming_from_settings(&s),
            IncomingConfig {
                enabled: true,
                port: 51413,
                upnp: true,
            }
        );
    }

    #[test]
    fn maps_each_mode() {
        let store = MemoryStore::new();
        let direct = config_from_settings(&NetworkSettings::default(), &store);
        assert_eq!(direct, NetworkConfig::default());

        let socks = NetworkSettings {
            mode: torrentplayer_settings::NetworkMode::Socks5,
            ipv6: Ipv6Policy::Tunnel,
            ..NetworkSettings::default()
        };
        let config = config_from_settings(&socks, &store);
        assert!(matches!(config.mode, NetworkMode::Socks5(_)));
        assert!(config.allow_ipv6);
        assert!(
            config.mode.validate().is_err(),
            "no proxy host blocks traffic"
        );

        let mut wg = NetworkSettings {
            mode: torrentplayer_settings::NetworkMode::WireGuard,
            ..NetworkSettings::default()
        };
        wg.wireguard.peer_endpoint = Some("198.51.100.1:51820".into());
        wg.wireguard.peer_public_key = Some("xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=".into());
        let config = config_from_settings(&wg, &store);
        assert_eq!(
            config.mode.endpoint().as_deref(),
            Some("198.51.100.1:51820")
        );
    }

    #[test]
    fn the_removed_vpn_adapter_mode_holds_direct_traffic_until_confirmed() {
        let store = MemoryStore::new();
        for (reason, message) in [
            (NetworkHold::VpnAdapterModeRemoved, VPN_ADAPTER_HOLD),
            (NetworkHold::SettingsUnreadable, SETTINGS_UNREADABLE_HOLD),
        ] {
            let mut held = NetworkSettings {
                hold: Some(reason),
                ..NetworkSettings::default()
            };
            let config = config_from_settings(&held, &store);
            assert_eq!(config.mode, NetworkMode::Direct);
            assert_eq!(config.hold.as_deref(), Some(message));
            assert!(
                config.is_protected(),
                "held like a mode behind the kill switch"
            );

            // Another mode has its own kill switch, so it does not need the hold.
            held.mode = torrentplayer_settings::NetworkMode::WireGuard;
            assert_eq!(config_from_settings(&held, &store).hold, None);

            // Dismissing the notice lets Direct connect.
            held.mode = torrentplayer_settings::NetworkMode::Direct;
            held.hold = None;
            assert_eq!(
                config_from_settings(&held, &store),
                NetworkConfig::default()
            );
        }
    }

    #[test]
    fn unreadable_settings_hold_traffic_and_a_first_run_connects_directly() {
        let secrets = MemoryStore::new();
        let dir = tempfile::tempdir().unwrap();
        let store = torrentplayer_settings::SettingsStore::new(dir.path());
        let config = config_from_settings(&store.load().settings.network, &secrets);
        assert_eq!(config, NetworkConfig::default(), "no file: Direct, no hold");

        for text in ["{ damaged", r#"{"version":5,"network":{"mode":"openvpn"}}"#] {
            std::fs::write(store.path(), text).unwrap();
            let config = config_from_settings(&store.load().settings.network, &secrets);
            assert_eq!(config.mode, NetworkMode::Direct, "{text}");
            assert_eq!(
                config.hold.as_deref(),
                Some(SETTINGS_UNREADABLE_HOLD),
                "{text}"
            );
        }
    }

    #[test]
    fn wireguard_reads_keys_in_the_backend_and_blocks_missing_credentials() {
        let store = MemoryStore::new();
        let parsed = parse_import(
            "[Interface]\nPrivateKey = yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=\nAddress = 10.0.0.2/32\nDNS = 10.0.0.1\n[Peer]\nPublicKey = xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=\nPresharedKey = FpCyhws9cxwWoV4xELtfJvjJN+zQVRPISllRWgeopVE=\nAllowedIPs = 0.0.0.0/0\nEndpoint = vpn.example.com:51820\n",
            None,
        ).unwrap();
        let settings = NetworkSettings {
            mode: torrentplayer_settings::NetworkMode::WireGuard,
            wireguard: parsed.settings,
            ..NetworkSettings::default()
        };
        assert!(
            config_from_settings(&settings, &store)
                .mode
                .validate()
                .is_err()
        );
        store
            .set(SecretKey::WireGuardPrivateKey, &parsed.private_key)
            .unwrap();
        store
            .set(
                SecretKey::WireGuardPresharedKey,
                parsed.preshared_key.as_ref().unwrap(),
            )
            .unwrap();
        let config = config_from_settings(&settings, &store);
        assert!(config.mode.validate().is_ok());
        let NetworkMode::WireGuard(wg) = config.mode else {
            panic!("expected WireGuard");
        };
        assert_eq!(
            wg.interface.private_key.expose(),
            parsed.private_key.expose_secret()
        );
        assert!(wg.peers[0].preshared_key.is_some());
        assert!(!format!("{wg:?}").contains(parsed.private_key.expose_secret()));
    }

    #[test]
    fn reads_the_proxy_password_from_the_store() {
        let store = MemoryStore::new();
        let config = config_from_settings(&socks_settings(Some("alice")), &store);
        let NetworkMode::Socks5(socks) = &config.mode else {
            panic!("expected SOCKS5");
        };
        assert!(socks.password.is_none());
        assert!(
            config.mode.validate().is_err(),
            "a missing password blocks traffic"
        );

        store
            .set(
                SecretKey::Socks5Password,
                &SecretString::from("pw".to_owned()),
            )
            .unwrap();
        let config = config_from_settings(&socks_settings(Some("alice")), &store);
        let NetworkMode::Socks5(socks) = &config.mode else {
            panic!("expected SOCKS5");
        };
        assert_eq!(socks.password.as_ref().map(Secret::expose), Some("pw"));
        assert!(socks.remote_dns && socks.proxy_trackers);
        assert!(config.mode.validate().is_ok());

        // Without a username the password is not read at all.
        let config = config_from_settings(&socks_settings(None), &store);
        let NetworkMode::Socks5(socks) = &config.mode else {
            panic!("expected SOCKS5");
        };
        assert!(socks.password.is_none());
    }

    #[test]
    fn wireguard_validation_never_echoes_keys() {
        let private = "yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=";
        let text = format!("[Interface]\nPrivateKey = {private}\nAddress = 10.0.0.2/32\n");
        let error = validate_wireguard_config(text).unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidArgument);
        assert!(!error.message.contains(private), "{}", error.message);

        let text = format!(
            "[Interface]\nPrivateKey = {private}\nAddress = 10.0.0.2/32\n[Peer]\n\
             PublicKey = xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=\n\
             AllowedIPs = 0.0.0.0/0\nEndpoint = vpn.example.com:51820\n"
        );
        let summary = validate_wireguard_config(text).unwrap();
        let json = serde_json::to_string(&summary).unwrap();
        assert!(!json.contains(private));
        assert!(json.contains("vpn.example.com:51820"));
    }

    #[test]
    fn imports_split_keys_from_settings() {
        let private = "yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=";
        let preshared = "FpCyhws9cxwWoV4xELtfJvjJN+zQVRPISllRWgeopVE=";
        let peer = "xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=";
        let text = format!(
            "[Interface]\nPrivateKey = {private}\nAddress = 10.0.0.2/32\nDNS = 10.0.0.1\n\
             [Peer]\nPublicKey = {peer}\nPresharedKey = {preshared}\n\
             AllowedIPs = 0.0.0.0/0\nEndpoint = vpn.example.com:51820\n"
        );
        let parsed = parse_import(&text, Some("  home.conf ".into())).unwrap();
        assert_eq!(parsed.private_key.expose_secret(), private);
        assert_eq!(
            parsed
                .preshared_key
                .as_ref()
                .map(ExposeSecret::expose_secret),
            Some(preshared)
        );
        let s = &parsed.settings;
        assert_eq!(s.name.as_deref(), Some("home.conf"));
        assert_eq!(s.peer_endpoint.as_deref(), Some("vpn.example.com:51820"));
        assert_eq!(s.peer_public_key.as_deref(), Some(peer));
        assert_eq!(s.dns, ["10.0.0.1"]);
        assert!(s.is_configured());
        let saved = serde_json::to_string(s).unwrap();
        let summary = serde_json::to_string(&parsed.summary).unwrap();
        for json in [saved, summary] {
            assert!(
                !json.contains(private) && !json.contains(preshared),
                "{json}"
            );
        }
        assert!(parsed.summary.peers[0].has_preshared_key);

        let two_peers = format!(
            "{text}[Peer]\nPublicKey = HIgo9xNzJMWLKASShiTqIybxZ0U3wGLiUeJ1PKf8ykw=\n\
             AllowedIPs = 10.1.0.0/16\n\
             Endpoint = vpn2.example.com:51820\n"
        );
        let error = parse_import(&two_peers, None).err().unwrap();
        assert_eq!(error.kind, ErrorKind::InvalidArgument);
        assert!(error.message.contains("exactly one"), "{}", error.message);
        assert!(!error.message.contains(private));
    }

    #[test]
    fn network_errors_map_to_stable_kinds() {
        let blocked: IpcError = NetError::KillSwitch("the proxy is unreachable".into()).into();
        assert_eq!(blocked.kind, ErrorKind::KillSwitch);
        assert_eq!(
            blocked.message,
            "Traffic is blocked by the kill switch: the proxy is unreachable."
        );
        let failed: IpcError = NetError::Timeout.into();
        assert_eq!(failed.kind, ErrorKind::NetworkUnavailable);
    }
}
