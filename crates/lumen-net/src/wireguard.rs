// SPDX-License-Identifier: MIT OR Apache-2.0

//! WireGuard configuration files (`wg-quick` format): parsing, validation, and a redacted
//! summary that is safe to show and log.
//!
//! Script hooks (`PreUp`, `PostUp`, ...) are never run; they are reported as ignored. The private
//! key and preshared keys are kept as [`Secret`]s and never appear in the summary.

use std::net::IpAddr;

use base64::Engine as _;
use serde::Serialize;
use ts_rs::TS;

use crate::secret::Secret;

/// Largest accepted configuration file.
pub const MAX_CONFIG_BYTES: usize = 64 * 1024;
/// Most `[Peer]` sections accepted.
const MAX_PEERS: usize = 64;
/// Most entries in one list (`Address`, `DNS`, `AllowedIPs`).
const MAX_LIST: usize = 256;

/// A parsed WireGuard configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireGuardConfig {
    /// The local end.
    pub interface: WgInterface,
    /// The servers. At least one.
    pub peers: Vec<WgPeer>,
}

/// The `[Interface]` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WgInterface {
    /// Base64 private key (32 bytes decoded).
    pub private_key: Secret,
    /// Tunnel addresses in CIDR form, for example `10.64.0.2/32`.
    pub addresses: Vec<String>,
    /// DNS servers (IP addresses) and search domains to use inside the tunnel.
    pub dns: Vec<String>,
    /// Tunnel MTU.
    pub mtu: Option<u16>,
    /// Local UDP port; random if unset.
    pub listen_port: Option<u16>,
}

/// One `[Peer]` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WgPeer {
    /// Base64 public key of the server.
    pub public_key: String,
    /// Optional base64 preshared key.
    pub preshared_key: Option<Secret>,
    /// Ranges routed through this peer, in CIDR form.
    pub allowed_ips: Vec<String>,
    /// `host:port` of the server.
    pub endpoint: String,
    /// Keep-alive interval in seconds; `None` when off.
    pub persistent_keepalive: Option<u16>,
}

/// What [`WireGuardConfig::summary`] reports. Contains no private or preshared keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct WireGuardSummary {
    /// Tunnel addresses.
    pub addresses: Vec<String>,
    /// DNS servers and search domains.
    pub dns: Vec<String>,
    /// Tunnel MTU.
    pub mtu: Option<u16>,
    /// Local UDP port.
    pub listen_port: Option<u16>,
    /// Servers.
    pub peers: Vec<WireGuardPeerSummary>,
    /// Whether all IPv4 or IPv6 traffic would be routed through the tunnel (`0.0.0.0/0` or
    /// `::/0` in `AllowedIPs`).
    pub routes_all_traffic: bool,
    /// Lines that were accepted but ignored (for example script hooks, which TorrentStream never runs).
    pub warnings: Vec<String>,
}

/// A server, without its preshared key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct WireGuardPeerSummary {
    /// Base64 public key.
    pub public_key: String,
    /// `host:port`.
    pub endpoint: String,
    /// Routed ranges.
    pub allowed_ips: Vec<String>,
    /// Keep-alive interval in seconds.
    pub persistent_keepalive: Option<u16>,
    /// Whether a preshared key is set (the key itself is never shown).
    pub has_preshared_key: bool,
}

/// Why a configuration was refused. Messages name the line but never repeat key material.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WireGuardError {
    /// The file is larger than [`MAX_CONFIG_BYTES`].
    #[error("the file is too large to be a WireGuard configuration")]
    TooLarge,
    /// A line or value is malformed.
    #[error("line {line}: {message}")]
    Line {
        /// 1-based line number.
        line: usize,
        /// What is wrong.
        message: String,
    },
    /// Something required is missing or the file as a whole is invalid.
    #[error("{0}")]
    Invalid(String),
}

impl WireGuardConfig {
    /// Parses and validates a `wg-quick` configuration.
    ///
    /// Returns the configuration and warnings about lines that were ignored.
    ///
    /// # Errors
    ///
    /// Returns an error if the text is not a complete, valid configuration.
    pub fn parse(text: &str) -> Result<(Self, Vec<String>), WireGuardError> {
        if text.len() > MAX_CONFIG_BYTES {
            return Err(WireGuardError::TooLarge);
        }
        let mut parser = Parser::default();
        for (index, raw) in text.lines().enumerate() {
            parser.line(index + 1, raw)?;
        }
        parser.finish()
    }

    /// The configuration without its secrets.
    #[must_use]
    pub fn summary(&self, warnings: Vec<String>) -> WireGuardSummary {
        let routes_all_traffic = self
            .peers
            .iter()
            .flat_map(|p| &p.allowed_ips)
            .any(|r| r == "0.0.0.0/0" || r == "::/0");
        WireGuardSummary {
            addresses: self.interface.addresses.clone(),
            dns: self.interface.dns.clone(),
            mtu: self.interface.mtu,
            listen_port: self.interface.listen_port,
            peers: self
                .peers
                .iter()
                .map(|p| WireGuardPeerSummary {
                    public_key: p.public_key.clone(),
                    endpoint: p.endpoint.clone(),
                    allowed_ips: p.allowed_ips.clone(),
                    persistent_keepalive: p.persistent_keepalive,
                    has_preshared_key: p.preshared_key.is_some(),
                })
                .collect(),
            routes_all_traffic,
            warnings,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    None,
    Interface,
    Peer,
}

#[derive(Default)]
struct PeerDraft {
    line: usize,
    public_key: Option<String>,
    preshared_key: Option<Secret>,
    allowed_ips: Vec<String>,
    endpoint: Option<String>,
    persistent_keepalive: Option<u16>,
}

struct Parser {
    section: Section,
    seen_interface: bool,
    private_key: Option<Secret>,
    addresses: Vec<String>,
    dns: Vec<String>,
    mtu: Option<u16>,
    listen_port: Option<u16>,
    peers: Vec<PeerDraft>,
    warnings: Vec<String>,
}

impl Default for Parser {
    fn default() -> Self {
        Self {
            section: Section::None,
            seen_interface: false,
            private_key: None,
            addresses: Vec::new(),
            dns: Vec::new(),
            mtu: None,
            listen_port: None,
            peers: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

fn line_error(line: usize, message: impl Into<String>) -> WireGuardError {
    WireGuardError::Line {
        line,
        message: message.into(),
    }
}

impl Parser {
    fn line(&mut self, n: usize, raw: &str) -> Result<(), WireGuardError> {
        // Everything after `#` is a comment; base64 never contains `#`.
        let line = raw.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            return Ok(());
        }
        if line.chars().any(char::is_control) {
            return Err(line_error(n, "contains control characters"));
        }
        if let Some(name) = line.strip_prefix('[') {
            let name = name
                .strip_suffix(']')
                .ok_or_else(|| line_error(n, "a section header must end with ']'"))?
                .trim();
            if name.eq_ignore_ascii_case("interface") {
                if self.seen_interface {
                    return Err(line_error(n, "only one [Interface] section is allowed"));
                }
                self.seen_interface = true;
                self.section = Section::Interface;
            } else if name.eq_ignore_ascii_case("peer") {
                if self.peers.len() == MAX_PEERS {
                    return Err(line_error(n, "too many [Peer] sections"));
                }
                self.peers.push(PeerDraft {
                    line: n,
                    ..PeerDraft::default()
                });
                self.section = Section::Peer;
            } else {
                return Err(line_error(
                    n,
                    "unknown section (expected [Interface] or [Peer])",
                ));
            }
            return Ok(());
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| line_error(n, "expected 'Key = Value'"))?;
        let (key, value) = (key.trim(), value.trim());
        let lower = key.to_ascii_lowercase();
        match self.section {
            Section::None => Err(line_error(n, "setting outside a section")),
            Section::Interface => self.interface_key(n, key, &lower, value),
            Section::Peer => self.peer_key(n, key, &lower, value),
        }
    }

    fn interface_key(
        &mut self,
        n: usize,
        key: &str,
        lower: &str,
        value: &str,
    ) -> Result<(), WireGuardError> {
        match lower {
            "privatekey" => {
                if self.private_key.is_some() {
                    return Err(line_error(n, "PrivateKey is set twice"));
                }
                check_key(n, "PrivateKey", value)?;
                self.private_key = Some(Secret::new(value));
            }
            "address" => {
                for item in list(n, value)? {
                    push_limited(n, &mut self.addresses, parse_cidr(n, item, true)?)?;
                }
            }
            "dns" => {
                for item in list(n, value)? {
                    let entry = if let Ok(ip) = item.parse::<IpAddr>() {
                        ip.to_string()
                    } else if is_host_name(item) {
                        item.to_ascii_lowercase()
                    } else {
                        return Err(line_error(n, "DNS entries must be IP addresses or domains"));
                    };
                    push_limited(n, &mut self.dns, entry)?;
                }
            }
            "mtu" => {
                let mtu = value
                    .parse::<u16>()
                    .ok()
                    .filter(|m| *m >= 576)
                    .ok_or_else(|| line_error(n, "MTU must be a number from 576 to 65535"))?;
                self.mtu = Some(mtu);
            }
            "listenport" => {
                let port = value
                    .parse::<u16>()
                    .map_err(|_| line_error(n, "ListenPort must be a number from 0 to 65535"))?;
                self.listen_port = (port != 0).then_some(port);
            }
            "preup" | "postup" | "predown" | "postdown" => {
                self.warnings.push(format!(
                    "line {n}: {key} was ignored; TorrentStream never runs configuration scripts"
                ));
            }
            "table" | "saveconfig" | "fwmark" => {
                self.warnings.push(format!("line {n}: {key} was ignored"));
            }
            _ => self
                .warnings
                .push(format!("line {n}: unknown setting {key} was ignored")),
        }
        Ok(())
    }

    fn peer_key(
        &mut self,
        n: usize,
        key: &str,
        lower: &str,
        value: &str,
    ) -> Result<(), WireGuardError> {
        let Some(peer) = self.peers.last_mut() else {
            return Err(line_error(n, "setting outside a section"));
        };
        match lower {
            "publickey" => {
                if peer.public_key.is_some() {
                    return Err(line_error(n, "PublicKey is set twice"));
                }
                check_key(n, "PublicKey", value)?;
                peer.public_key = Some(value.to_owned());
            }
            "presharedkey" => {
                if peer.preshared_key.is_some() {
                    return Err(line_error(n, "PresharedKey is set twice"));
                }
                check_key(n, "PresharedKey", value)?;
                peer.preshared_key = Some(Secret::new(value));
            }
            "allowedips" => {
                for item in list(n, value)? {
                    push_limited(n, &mut peer.allowed_ips, parse_cidr(n, item, false)?)?;
                }
            }
            "endpoint" => {
                if peer.endpoint.is_some() {
                    return Err(line_error(n, "Endpoint is set twice"));
                }
                peer.endpoint = Some(parse_endpoint(n, value)?);
            }
            "persistentkeepalive" => {
                peer.persistent_keepalive = if value.eq_ignore_ascii_case("off") {
                    None
                } else {
                    let secs = value.parse::<u16>().map_err(|_| {
                        line_error(n, "PersistentKeepalive must be 'off' or 0 to 65535 seconds")
                    })?;
                    (secs != 0).then_some(secs)
                };
            }
            _ => self
                .warnings
                .push(format!("line {n}: unknown setting {key} was ignored")),
        }
        Ok(())
    }

    fn finish(self) -> Result<(WireGuardConfig, Vec<String>), WireGuardError> {
        if !self.seen_interface {
            return Err(WireGuardError::Invalid(
                "the [Interface] section is missing".into(),
            ));
        }
        let private_key = self
            .private_key
            .ok_or_else(|| WireGuardError::Invalid("[Interface] has no PrivateKey".into()))?;
        if self.addresses.is_empty() {
            return Err(WireGuardError::Invalid("[Interface] has no Address".into()));
        }
        if self.peers.is_empty() {
            return Err(WireGuardError::Invalid("there is no [Peer] section".into()));
        }
        let mut peers = Vec::with_capacity(self.peers.len());
        for draft in self.peers {
            let n = draft.line;
            let public_key = draft
                .public_key
                .ok_or_else(|| line_error(n, "this [Peer] has no PublicKey"))?;
            let endpoint = draft
                .endpoint
                .ok_or_else(|| line_error(n, "this [Peer] has no Endpoint"))?;
            if draft.allowed_ips.is_empty() {
                return Err(line_error(n, "this [Peer] has no AllowedIPs"));
            }
            peers.push(WgPeer {
                public_key,
                preshared_key: draft.preshared_key,
                allowed_ips: draft.allowed_ips,
                endpoint,
                persistent_keepalive: draft.persistent_keepalive,
            });
        }
        let config = WireGuardConfig {
            interface: WgInterface {
                private_key,
                addresses: self.addresses,
                dns: self.dns,
                mtu: self.mtu,
                listen_port: self.listen_port,
            },
            peers,
        };
        Ok((config, self.warnings))
    }
}

/// A comma-separated list with no empty entries.
fn list(n: usize, value: &str) -> Result<Vec<&str>, WireGuardError> {
    let items: Vec<&str> = value.split(',').map(str::trim).collect();
    if items.iter().any(|i| i.is_empty()) {
        return Err(line_error(n, "empty entry in a list"));
    }
    Ok(items)
}

fn push_limited(n: usize, list: &mut Vec<String>, item: String) -> Result<(), WireGuardError> {
    if list.len() == MAX_LIST {
        return Err(line_error(n, "too many entries"));
    }
    list.push(item);
    Ok(())
}

/// A key is standard base64 of exactly 32 bytes (44 characters with padding). An all-zero key is
/// refused. The error never includes the value.
fn check_key(n: usize, name: &str, value: &str) -> Result<(), WireGuardError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|_| line_error(n, format!("{name} is not valid base64")))?;
    let key: [u8; 32] = bytes
        .try_into()
        .map_err(|_| line_error(n, format!("{name} must be 32 bytes")))?;
    if key == [0u8; 32] {
        return Err(line_error(n, format!("{name} is all zeros")));
    }
    Ok(())
}

/// `ip/prefix`, or a bare IP when `bare_ok` (it then means a single host). Returns the
/// normalized form.
fn parse_cidr(n: usize, item: &str, bare_ok: bool) -> Result<String, WireGuardError> {
    let (ip, prefix) = match item.split_once('/') {
        Some((ip, prefix)) => (ip, Some(prefix)),
        None if bare_ok => (item, None),
        None => {
            return Err(line_error(
                n,
                "expected an address range such as 10.0.0.0/24",
            ));
        }
    };
    let ip: IpAddr = ip
        .parse()
        .map_err(|_| line_error(n, "not a valid IP address"))?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    let prefix = match prefix {
        Some(p) => p
            .parse::<u8>()
            .ok()
            .filter(|p| *p <= max)
            .ok_or_else(|| line_error(n, "not a valid prefix length"))?,
        None => max,
    };
    Ok(format!("{ip}/{prefix}"))
}

/// `host:port` with an IPv4 address, a bracketed IPv6 address, or a DNS name.
fn parse_endpoint(n: usize, value: &str) -> Result<String, WireGuardError> {
    let bad = || line_error(n, "Endpoint must be host:port (IPv6 in brackets)");
    let (host, port) = value.rsplit_once(':').ok_or_else(bad)?;
    let port = port
        .parse::<u16>()
        .ok()
        .filter(|p| *p != 0)
        .ok_or_else(bad)?;
    if let Some(v6) = host.strip_prefix('[') {
        let v6 = v6.strip_suffix(']').ok_or_else(bad)?;
        let ip: std::net::Ipv6Addr = v6.parse().map_err(|_| bad())?;
        return Ok(format!("[{ip}]:{port}"));
    }
    if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
        return Ok(format!("{ip}:{port}"));
    }
    if is_host_name(host) {
        return Ok(format!("{}:{port}", host.to_ascii_lowercase()));
    }
    Err(bad())
}

/// A DNS name: dot-separated labels of letters, digits, and hyphens.
fn is_host_name(host: &str) -> bool {
    let host = host.strip_suffix('.').unwrap_or(host);
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRIVATE: &str = "yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=";
    const PUBLIC: &str = "xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=";
    const PSK: &str = "FpCyhws9cxwWoV4xELtfJvjJN+zQVRPISllRWgeopVE=";

    fn sample() -> String {
        format!(
            "# Example provider config
[Interface]
PrivateKey = {PRIVATE}
Address = 10.64.0.2/32, fd00::2/128
DNS = 10.64.0.1, corp.example
MTU = 1420
PostUp = iptables -A FORWARD -j ACCEPT

[Peer]
PublicKey = {PUBLIC}
PresharedKey = {PSK}
AllowedIPs = 0.0.0.0/0, ::/0
Endpoint = vpn.example.com:51820   # Amsterdam
PersistentKeepalive = 25
"
        )
    }

    #[test]
    fn parses_a_typical_provider_config() {
        let (config, warnings) = WireGuardConfig::parse(&sample()).unwrap();
        assert_eq!(config.interface.private_key.expose(), PRIVATE);
        assert_eq!(config.interface.addresses, ["10.64.0.2/32", "fd00::2/128"]);
        assert_eq!(config.interface.dns, ["10.64.0.1", "corp.example"]);
        assert_eq!(config.interface.mtu, Some(1420));
        let peer = &config.peers[0];
        assert_eq!(peer.public_key, PUBLIC);
        assert_eq!(peer.endpoint, "vpn.example.com:51820");
        assert_eq!(peer.allowed_ips, ["0.0.0.0/0", "::/0"]);
        assert_eq!(peer.persistent_keepalive, Some(25));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("PostUp"), "{warnings:?}");
    }

    #[test]
    fn the_summary_never_contains_secrets() {
        let (config, warnings) = WireGuardConfig::parse(&sample()).unwrap();
        let summary = config.summary(warnings);
        assert!(summary.routes_all_traffic);
        assert!(summary.peers[0].has_preshared_key);
        let json = serde_json::to_string(&summary).unwrap();
        assert!(!json.contains(PRIVATE) && !json.contains(PSK), "{json}");
        assert!(json.contains(PUBLIC));
        let debug = format!("{config:?}");
        assert!(!debug.contains(PRIVATE) && !debug.contains(PSK), "{debug}");
    }

    #[test]
    fn accepts_ipv6_endpoints_bare_addresses_and_case_insensitive_keys() {
        let text = format!(
            "[interface]\nprivatekey={PRIVATE}\naddress=10.0.0.2\n[PEER]\npublickey={PUBLIC}\n\
             allowedips=10.0.0.0/8\nendpoint=[2001:db8::1]:51820\npersistentkeepalive=off\n"
        );
        let (config, _) = WireGuardConfig::parse(&text).unwrap();
        assert_eq!(config.interface.addresses, ["10.0.0.2/32"]);
        assert_eq!(config.peers[0].endpoint, "[2001:db8::1]:51820");
        assert_eq!(config.peers[0].persistent_keepalive, None);
        assert!(!config.summary(Vec::new()).routes_all_traffic);
    }

    fn error_for(text: &str) -> WireGuardError {
        WireGuardConfig::parse(text).unwrap_err()
    }

    #[test]
    fn rejects_bad_keys_without_echoing_them() {
        let short = "c2hvcnQ=";
        let text = format!(
            "[Interface]\nPrivateKey = {short}\nAddress = 10.0.0.2/32\n[Peer]\nPublicKey = {PUBLIC}\n\
             AllowedIPs = 0.0.0.0/0\nEndpoint = 1.2.3.4:51820\n"
        );
        let error = error_for(&text);
        assert!(error.to_string().contains("line 2"), "{error}");
        assert!(!error.to_string().contains(short));
        let zeros = base64::engine::general_purpose::STANDARD.encode([0u8; 32]);
        let text = text.replace(short, &zeros);
        assert!(error_for(&text).to_string().contains("zeros"));
        let text = text.replace(&zeros, "not base64!!");
        assert!(error_for(&text).to_string().contains("base64"));
    }

    #[test]
    fn rejects_incomplete_or_malformed_configs() {
        let interface = format!("[Interface]\nPrivateKey = {PRIVATE}\nAddress = 10.0.0.2/32\n");
        let peer =
            |extra: &str| format!("[Peer]\nPublicKey = {PUBLIC}\nAllowedIPs = 0.0.0.0/0\n{extra}");
        let cases = [
            (String::new(), "Interface"),
            (interface.clone(), "[Peer]"),
            (format!("{interface}{}", peer("")), "Endpoint"),
            (
                format!("{interface}{}", peer("Endpoint = host\n")),
                "host:port",
            ),
            (
                format!("{interface}{}", peer("Endpoint = host:0\n")),
                "host:port",
            ),
            (
                format!("{interface}{}", peer("Endpoint = [::1:51820\n")),
                "host:port",
            ),
            (
                format!("{interface}{}", peer("Endpoint = bad_host:1\n")),
                "host:port",
            ),
            (
                format!("{interface}{}", peer("Endpoint = a:1\nEndpoint = b:1\n")),
                "twice",
            ),
            (
                format!(
                    "{interface}{}",
                    peer("Endpoint = a:1\nAllowedIPs = 10.0.0.0\n")
                ),
                "range",
            ),
            (
                format!(
                    "{interface}{}",
                    peer("Endpoint = a:1\nAllowedIPs = 10.0.0.0/33\n")
                ),
                "prefix",
            ),
            (
                format!(
                    "{interface}{}",
                    peer("Endpoint = a:1\nAllowedIPs = 1.1.1.1/32,,\n")
                ),
                "empty",
            ),
            (
                format!("{interface}MTU = 10\n{}", peer("Endpoint = a:1\n")),
                "MTU",
            ),
            (format!("{interface}[Wat]\n"), "unknown section"),
            (format!("Address = 1.2.3.4\n{interface}"), "outside"),
            (format!("{interface}just words\n"), "Key = Value"),
            (format!("{interface}[Interface]\n"), "only one"),
            (
                "[Interface]\nAddress = 10.0.0.2/32\n".to_owned(),
                "PrivateKey",
            ),
        ];
        for (text, needle) in cases {
            let error = error_for(&text).to_string();
            assert!(
                error.contains(needle),
                "expected {needle:?} in {error:?} for:\n{text}"
            );
        }
        assert_eq!(
            error_for(&"#".repeat(MAX_CONFIG_BYTES + 1)),
            WireGuardError::TooLarge
        );
    }
}
