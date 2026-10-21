// SPDX-License-Identifier: MIT OR Apache-2.0

//! The network modes a user can choose.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::error::NetError;
use crate::secret::Secret;
use crate::wireguard::WireGuardConfig;

/// How TorrentPlayer's traffic leaves the machine.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum NetworkMode {
    /// Straight to peers, trackers, and search, with no tunnel. The only mode without a kill
    /// switch.
    #[default]
    Direct,
    /// All traffic is bound to one network interface, typically created by the user's own VPN
    /// app. If that interface disappears or loses its address, traffic stops.
    BindInterface {
        /// Interface name as listed by [`crate::list_interfaces`].
        interface: String,
    },
    /// TCP uses SOCKS5 CONNECT; DHT and UDP trackers use UDP ASSOCIATE when supported.
    Socks5(Socks5Config),
    /// An application-local WireGuard tunnel using an imported configuration.
    WireGuard(WireGuardConfig),
}

/// The kind of a [`NetworkMode`], without its settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, rename = "NetworkModeKind")]
pub enum ModeKind {
    /// [`NetworkMode::Direct`].
    Direct,
    /// [`NetworkMode::BindInterface`].
    BindInterface,
    /// [`NetworkMode::Socks5`].
    Socks5,
    /// [`NetworkMode::WireGuard`].
    WireGuard,
}

/// A SOCKS5 proxy (RFC 1928), with optional username/password authentication (RFC 1929).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Socks5Config {
    /// Proxy host name or IP address.
    pub host: String,
    /// Proxy port.
    pub port: u16,
    /// Username, if the proxy requires authentication.
    pub username: Option<String>,
    /// Password, if the proxy requires authentication.
    pub password: Option<Secret>,
    /// Let the proxy resolve host names (`socks5h`), so no DNS query for trackers or search
    /// leaves this machine. When off, names are resolved locally and only addresses are sent.
    pub remote_dns: bool,
    /// Contact trackers through the proxy. When off, trackers are not contacted at all in this
    /// mode (never directly); DHT and peer exchange remain independent.
    pub proxy_trackers: bool,
    /// Enable DHT through the proxy's UDP relay. Independent of tracker announces.
    pub proxy_dht: bool,
}

/// RFC 1929 limits usernames and passwords to 255 bytes; host names are limited by DNS.
const MAX_FIELD_BYTES: usize = 255;

impl Socks5Config {
    /// Checks the fields before the mode is used.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::InvalidConfig`] for an empty or malformed host, port 0, or overlong
    /// credentials.
    pub fn validate(&self) -> Result<(), NetError> {
        let host = self.host.trim();
        if host.is_empty() {
            return Err(NetError::InvalidConfig(
                "enter the proxy's host name or IP address".into(),
            ));
        }
        if host.len() > MAX_FIELD_BYTES
            || host
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || "/@?#".contains(c))
        {
            return Err(NetError::InvalidConfig(
                "the proxy host is not valid".into(),
            ));
        }
        if self.port == 0 {
            return Err(NetError::InvalidConfig(
                "the proxy port must be 1–65535".into(),
            ));
        }
        let user = self.username.as_deref().unwrap_or_default();
        let pass = self.password.as_ref().map_or("", Secret::expose);
        if user.len() > MAX_FIELD_BYTES || pass.len() > MAX_FIELD_BYTES {
            return Err(NetError::InvalidConfig(
                "proxy usernames and passwords are limited to 255 bytes".into(),
            ));
        }
        if user.is_empty() != pass.is_empty() {
            return Err(NetError::InvalidConfig(
                "give both a proxy username and password, or neither".into(),
            ));
        }
        Ok(())
    }

    /// Username and password, when both are set.
    pub(crate) fn credentials(&self) -> Option<(&str, &str)> {
        match (self.username.as_deref(), self.password.as_ref()) {
            (Some(u), Some(p)) if !u.is_empty() && !p.is_empty() => Some((u, p.expose())),
            _ => None,
        }
    }

    /// `host:port`, with IPv6 literals bracketed.
    #[must_use]
    pub fn endpoint(&self) -> String {
        let host = self.host.trim();
        if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]:{}", self.port)
        } else {
            format!("{host}:{}", self.port)
        }
    }
}

impl NetworkMode {
    /// The mode's kind.
    #[must_use]
    pub fn kind(&self) -> ModeKind {
        match self {
            Self::Direct => ModeKind::Direct,
            Self::BindInterface { .. } => ModeKind::BindInterface,
            Self::Socks5(_) => ModeKind::Socks5,
            Self::WireGuard(_) => ModeKind::WireGuard,
        }
    }

    /// Whether the kill switch applies: every mode except [`NetworkMode::Direct`].
    #[must_use]
    pub fn is_protected(&self) -> bool {
        !matches!(self, Self::Direct)
    }

    /// Where traffic goes, without secrets: the proxy endpoint, the interface name, or the
    /// WireGuard endpoint.
    #[must_use]
    pub fn endpoint(&self) -> Option<String> {
        match self {
            Self::Direct => None,
            Self::BindInterface { interface } => Some(interface.clone()),
            Self::Socks5(c) => Some(c.endpoint()),
            Self::WireGuard(c) => c.peers.first().map(|p| p.endpoint.clone()),
        }
    }

    /// Checks the mode's settings.
    ///
    /// # Errors
    ///
    /// Returns [`NetError::InvalidConfig`] if a setting is malformed.
    pub fn validate(&self) -> Result<(), NetError> {
        match self {
            Self::Direct => Ok(()),
            Self::WireGuard(config) => crate::tunnel::validate(config),
            Self::BindInterface { interface } => {
                let name = interface.trim();
                if name.is_empty() {
                    Err(NetError::InvalidConfig(
                        "choose the network interface to bind to".into(),
                    ))
                } else if name.len() > 256 || name.chars().any(char::is_control) {
                    Err(NetError::InvalidConfig(
                        "the interface name is not valid".into(),
                    ))
                } else {
                    Ok(())
                }
            }
            Self::Socks5(c) => c.validate(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn socks(host: &str, port: u16, user: Option<&str>, pass: Option<&str>) -> Socks5Config {
        Socks5Config {
            host: host.into(),
            port,
            username: user.map(Into::into),
            password: pass.map(Secret::new),
            remote_dns: true,
            proxy_trackers: true,
            proxy_dht: true,
        }
    }

    #[test]
    fn validates_proxy_settings() {
        assert!(socks("127.0.0.1", 1080, None, None).validate().is_ok());
        assert!(
            socks("proxy.example", 1080, Some("u"), Some("p"))
                .validate()
                .is_ok()
        );
        assert!(socks("", 1080, None, None).validate().is_err());
        assert!(socks("a b", 1080, None, None).validate().is_err());
        assert!(socks("user@host", 1080, None, None).validate().is_err());
        assert!(socks("host", 0, None, None).validate().is_err());
        assert!(socks("host", 1, Some("u"), None).validate().is_err());
        assert!(
            socks("host", 1, Some(&"u".repeat(256)), Some("p"))
                .validate()
                .is_err()
        );
    }

    #[test]
    fn endpoints_bracket_ipv6_and_hide_secrets() {
        assert_eq!(socks("::1", 1080, None, None).endpoint(), "[::1]:1080");
        let mode = NetworkMode::Socks5(socks("10.0.0.1", 9050, Some("me"), Some("secret")));
        assert_eq!(mode.endpoint().as_deref(), Some("10.0.0.1:9050"));
        assert!(!format!("{mode:?}").contains("secret"));
        assert!(mode.is_protected());
        assert!(!NetworkMode::Direct.is_protected());
    }
}
