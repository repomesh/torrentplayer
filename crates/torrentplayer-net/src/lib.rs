// SPDX-License-Identifier: MIT OR Apache-2.0

//! Network modes for TorrentPlayer, with a kill switch.
//!
//! Every outbound connection TorrentPlayer makes for torrents, trackers, and search goes through a
//! [`Network`]:
//!
//! - [`NetworkMode::Direct`]: no VPN or proxy. Users of a system VPN app keep it connected and
//!   use this mode.
//! - [`NetworkMode::Socks5`]: through a SOCKS5 proxy, with host names resolved by the proxy.
//! - [`NetworkMode::WireGuard`]: an application-local tunnel using an imported configuration.
//!   It requires no system adapter or administrator rights and never falls back to direct.
//!
//! In every mode except direct, the kill switch applies: until the mode is verified, and
//! whenever a health check fails, connections are refused and existing ones are closed. DHT and UDP
//! trackers use sockets scoped to the WireGuard tunnel or the SOCKS5 UDP relay.
//! Unbound native UDP (including uTP) stays disabled in protected modes.
//!
//! Incoming peer connections are off by default. When turned on, the listener opens only on the
//! path the mode allows and never in SOCKS5 mode (see [`Network::set_incoming`]).
//!
//! HTTP clients and librqbit reach the network through a loopback SOCKS5 bridge that applies the
//! same rules ([`Network::configure_http`], [`Network::bridge_proxy_url`]). See
//! `docs/network.md` for the design and its limits.

#![forbid(unsafe_code)]

mod addr;
mod bridge;
mod datagram;
mod error;
mod incoming;
mod mode;
mod network;
mod secret;
pub mod socks5;
mod socks_udp;
#[cfg(any(test, feature = "test-util"))]
pub mod test_util;
#[cfg(test)]
mod tests;
mod tunnel;
pub mod wireguard;

pub use addr::is_public_ip;
pub use datagram::DatagramSocket;
pub use error::NetError;
pub use incoming::{
    ExternalListener, IncomingConfig, IncomingPeer, IncomingPeers, IncomingStatus, RANDOM_PORTS,
};
pub use mode::{ModeKind, NetworkMode, Socks5Config};
pub use network::{
    ConnectionState, EventLevel, IP_CHECK_V4_URL, IP_CHECK_V6_URL, Lease, NetEvent, Network,
    NetworkConfig, NetworkOptions, NetworkStatus, Policy, PublicIp, REACHABLE_AFTER_INBOUND,
    UPNP_LEASE, WIREGUARD_REPLY_TIMEOUT, WIREGUARD_RESTART_AFTER,
};
pub use secret::Secret;
pub use wireguard::{WireGuardConfig, WireGuardError, WireGuardSummary};
