// SPDX-License-Identifier: MIT OR Apache-2.0

//! Network modes for TorrentStream, with a kill switch.
//!
//! Every outbound connection TorrentStream makes for torrents, trackers, and search goes through a
//! [`Network`]:
//!
//! - [`NetworkMode::Direct`]: no VPN or proxy.
//! - [`NetworkMode::BindInterface`]: only through one interface, usually the adapter of the
//!   user's own VPN app (see [`bind`] for how each platform pins traffic to it).
//! - [`NetworkMode::Socks5`]: through a SOCKS5 proxy, with host names resolved by the proxy.
//! - [`NetworkMode::WireGuard`]: an application-local tunnel using an imported configuration.
//!   It requires no system adapter or administrator rights and never falls back to direct.
//!
//! In every mode except direct, the kill switch applies: until the mode is verified, and
//! whenever a health check fails, connections are refused and existing ones are closed. UDP
//! trackers can use sockets scoped to the selected interface or WireGuard tunnel. Unbound
//! UDP (including DHT and uTP) stays disabled in protected modes; SOCKS5 refuses UDP.
//!
//! HTTP clients and librqbit reach the network through a loopback SOCKS5 bridge that applies the
//! same rules ([`Network::configure_http`], [`Network::bridge_proxy_url`]). See
//! `docs/network.md` for the design and its limits.

#![forbid(unsafe_code)]

mod addr;
pub mod bind;
mod bridge;
mod datagram;
mod error;
mod mode;
mod network;
mod secret;
pub mod socks5;
#[cfg(any(test, feature = "test-util"))]
pub mod test_util;
#[cfg(test)]
mod tests;
mod tunnel;
pub mod wireguard;

pub use addr::is_public_ip;
pub use bind::{InterfaceInfo, list_interfaces};
pub use datagram::DatagramSocket;
pub use error::NetError;
pub use mode::{ModeKind, NetworkMode, Socks5Config};
pub use network::{
    ConnectionState, EventLevel, IP_CHECK_V4_URL, IP_CHECK_V6_URL, Lease, NetEvent, Network,
    NetworkConfig, NetworkOptions, NetworkStatus, Policy, PublicIp,
};
pub use secret::Secret;
pub use wireguard::{WireGuardConfig, WireGuardError, WireGuardSummary};
