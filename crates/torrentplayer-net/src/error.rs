// SPDX-License-Identifier: MIT OR Apache-2.0

//! Errors from the network layer. Messages are written for users and never contain secrets.

/// Why a connection or setting failed.
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    /// A setting is malformed.
    #[error("{0}")]
    InvalidConfig(String),
    /// The kill switch is blocking traffic because the selected tunnel or proxy is not
    /// connected.
    #[error("traffic is blocked by the kill switch: {0}")]
    KillSwitch(String),
    /// IPv6 destinations are refused while traffic is tunneled or proxied.
    #[error("IPv6 is turned off while traffic goes through a VPN or proxy")]
    Ipv6Disabled,
    /// A destination from an untrusted source is on the user's own network (see
    /// [`is_public_ip`](crate::is_public_ip)).
    #[error("refused a connection to an address on the local network")]
    NotPublic,
    /// The selected mode cannot work on this platform.
    #[error("{0}")]
    Unsupported(String),
    /// The chosen network interface is missing, down, or has no usable address.
    #[error("network interface unavailable: {0}")]
    InterfaceUnavailable(String),
    /// The SOCKS5 proxy could not be reached.
    #[error("could not reach the proxy: {0}")]
    ProxyUnreachable(std::io::Error),
    /// The SOCKS5 proxy refused, or broke the protocol.
    #[error("proxy: {0}")]
    Proxy(#[from] crate::socks5::Socks5Error),
    /// A network operation timed out.
    #[error("timed out")]
    Timeout,
    /// An I/O error.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// The connection check failed.
    #[error("{0}")]
    Http(String),
}
