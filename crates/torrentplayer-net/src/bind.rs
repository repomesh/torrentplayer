// SPDX-License-Identifier: MIT OR Apache-2.0

//! Network interfaces: listing them, and making TCP connections that can only leave through
//! one of them.
//!
//! How traffic is pinned to an interface, per platform:
//!
//! - **Windows** uses the strong host model: a packet whose source address belongs to an
//!   interface can only be sent through that interface. Binding the socket to the interface's
//!   address is enough.
//! - **Linux and Android** bind the socket to the device (`SO_BINDTODEVICE`), plus its address.
//!   Kernels before 5.7 require `CAP_NET_RAW` for that; without it the mode reports an error
//!   instead of connecting unbound.
//! - **macOS and iOS** bind by interface index (`IP_BOUND_IF` / `IPV6_BOUND_IF`), plus the
//!   address.
//! - Other platforms are not supported and the mode reports an error.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use serde::Serialize;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::{TcpSocket, TcpStream};
use ts_rs::TS;

use crate::error::NetError;

/// Whether binding to an interface is implemented on this platform.
pub const SUPPORTED: bool = cfg!(any(
    windows,
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
));

/// A network interface, for the interface picker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, rename = "NetworkInterface")]
pub struct InterfaceInfo {
    /// Name to select it by (the adapter's friendly name on Windows, for example "Wi-Fi" or a
    /// VPN app's adapter).
    pub name: String,
    /// Its addresses.
    pub addresses: Vec<String>,
    /// Whether it is operationally up.
    pub is_up: bool,
    /// Whether it is the loopback interface (never useful to bind to).
    pub is_loopback: bool,
    /// A guess that it belongs to a VPN: point-to-point, or a name such as `wg0`, `tun0`, or one
    /// containing "VPN".
    pub likely_vpn: bool,
}

/// The interface and addresses that bound connections use, re-resolved by every health check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundInterface {
    pub(crate) name: String,
    pub(crate) v4: Option<Ipv4Addr>,
    pub(crate) v6: Option<Ipv6Addr>,
    #[cfg_attr(not(any(target_os = "macos", target_os = "ios")), allow(dead_code))]
    pub(crate) index: Option<u32>,
}

impl BoundInterface {
    /// A short description for status messages.
    pub(crate) fn describe(&self) -> String {
        let addrs: Vec<String> = self
            .v4
            .map(IpAddr::from)
            .into_iter()
            .chain(self.v6.map(IpAddr::from))
            .map(|ip| ip.to_string())
            .collect();
        format!("{} ({})", self.name, addrs.join(", "))
    }

    fn source_for(&self, target: &SocketAddr) -> Result<IpAddr, NetError> {
        match target {
            SocketAddr::V4(_) => self.v4.map(IpAddr::from).ok_or_else(|| {
                NetError::InterfaceUnavailable(format!("{} has no IPv4 address", self.name))
            }),
            SocketAddr::V6(_) => self.v6.map(IpAddr::from).ok_or_else(|| {
                NetError::InterfaceUnavailable(format!("{} has no IPv6 address", self.name))
            }),
        }
    }

    /// Whether this interface can reach `target`'s address family.
    pub(crate) fn has_family_of(&self, target: &SocketAddr) -> bool {
        self.source_for(target).is_ok()
    }
}

/// Lists the machine's interfaces, grouped by name.
///
/// # Errors
///
/// Returns an error if the operating system cannot list interfaces.
pub fn list_interfaces() -> Result<Vec<InterfaceInfo>, NetError> {
    let mut by_name: BTreeMap<String, InterfaceInfo> = BTreeMap::new();
    for iface in if_addrs::get_if_addrs()? {
        let likely_vpn = iface.is_p2p() || looks_like_vpn(&iface.name);
        let entry = by_name
            .entry(iface.name.clone())
            .or_insert_with(|| InterfaceInfo {
                name: iface.name.clone(),
                addresses: Vec::new(),
                is_up: false,
                is_loopback: false,
                likely_vpn: false,
            });
        entry.addresses.push(iface.ip().to_string());
        entry.is_up |= iface.is_oper_up();
        entry.is_loopback |= iface.is_loopback();
        entry.likely_vpn |= likely_vpn;
    }
    Ok(by_name.into_values().collect())
}

fn looks_like_vpn(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ["wg", "tun", "tap", "ppp", "utun", "ipsec"]
        .iter()
        .any(|p| lower.starts_with(p))
        || [
            "vpn",
            "wireguard",
            "nordlynx",
            "mullvad",
            "proton",
            "tailscale",
            "zerotier",
        ]
        .iter()
        .any(|w| lower.contains(w))
}

/// Finds the interface named `name` (exact match, then case-insensitive) and picks the addresses
/// to bind to. Blocking: it asks the operating system.
pub(crate) fn resolve(name: &str) -> Result<BoundInterface, NetError> {
    if !SUPPORTED {
        return Err(NetError::Unsupported(
            "binding to a network interface is not supported on this platform".into(),
        ));
    }
    let all = if_addrs::get_if_addrs()?;
    let mut matching: Vec<_> = all.iter().filter(|i| i.name == name).collect();
    if matching.is_empty() {
        matching = all
            .iter()
            .filter(|i| i.name.eq_ignore_ascii_case(name))
            .collect();
    }
    if matching.is_empty() {
        return Err(NetError::InterfaceUnavailable(format!(
            "{name} was not found"
        )));
    }
    if matching.iter().any(|i| i.is_loopback()) {
        return Err(NetError::InterfaceUnavailable(format!(
            "{name} is the loopback interface"
        )));
    }
    if !matching.iter().any(|i| i.is_oper_up()) {
        return Err(NetError::InterfaceUnavailable(format!("{name} is down")));
    }
    let usable = |ip: &IpAddr| !ip.is_loopback() && !ip.is_unspecified() && !is_link_local(ip);
    let v4 = matching.iter().find_map(|i| match i.ip() {
        IpAddr::V4(ip) if usable(&IpAddr::V4(ip)) => Some(ip),
        _ => None,
    });
    let v6 = matching.iter().find_map(|i| match i.ip() {
        IpAddr::V6(ip) if usable(&IpAddr::V6(ip)) => Some(ip),
        _ => None,
    });
    if v4.is_none() && v6.is_none() {
        return Err(NetError::InterfaceUnavailable(format!(
            "{name} has no usable address"
        )));
    }
    Ok(BoundInterface {
        name: matching[0].name.clone(),
        v4,
        v6,
        index: matching.iter().find_map(|i| i.index),
    })
}

fn is_link_local(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

/// A UDP socket that sends to `target`'s address family through `iface` only.
pub(crate) fn udp_socket(
    iface: &BoundInterface,
    target: &SocketAddr,
) -> Result<std::net::UdpSocket, NetError> {
    let source = iface.source_for(target)?;
    let socket = Socket::new(
        Domain::for_address(*target),
        Type::DGRAM,
        Some(Protocol::UDP),
    )?;
    pin_to_device(&socket, iface, target)?;
    socket.bind(&SocketAddr::new(source, 0).into())?;
    Ok(socket.into())
}

/// Connects to `target` through `iface` only.
pub(crate) async fn connect(
    iface: &BoundInterface,
    target: SocketAddr,
) -> Result<TcpStream, NetError> {
    let source = iface.source_for(&target)?;
    let socket = Socket::new(
        Domain::for_address(target),
        Type::STREAM,
        Some(Protocol::TCP),
    )?;
    socket.set_nonblocking(true)?;
    pin_to_device(&socket, iface, &target)?;
    socket.bind(&SocketAddr::new(source, 0).into())?;
    let socket = TcpSocket::from_std_stream(socket.into());
    Ok(socket.connect(target).await?)
}

/// A listening socket on `addr`. With `iface`, it is pinned to that interface as [`connect`]
/// pins connections, so it accepts only connections that arrive through it (on Windows, the
/// strong host model drops packets for the interface's address that arrive elsewhere).
pub(crate) fn listen(
    iface: Option<&BoundInterface>,
    addr: SocketAddr,
) -> Result<std::net::TcpListener, NetError> {
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    if addr.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    // Elsewhere this only allows rebinding over connections in TIME_WAIT; on Windows it would
    // let the socket share a port another program is listening on.
    #[cfg(not(windows))]
    socket.set_reuse_address(true)?;
    if let Some(iface) = iface {
        pin_to_device(&socket, iface, &addr)?;
    }
    socket.bind(&addr.into())?;
    socket.listen(128)?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn pin_to_device(
    socket: &Socket,
    iface: &BoundInterface,
    _target: &SocketAddr,
) -> Result<(), NetError> {
    socket
        .bind_device(Some(iface.name.as_bytes()))
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::PermissionDenied => NetError::Unsupported(
                "binding to an interface needs Linux 5.7 or newer, or the CAP_NET_RAW capability"
                    .into(),
            ),
            _ => NetError::Io(e),
        })
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn pin_to_device(
    socket: &Socket,
    iface: &BoundInterface,
    target: &SocketAddr,
) -> Result<(), NetError> {
    let index = iface
        .index
        .and_then(std::num::NonZeroU32::new)
        .ok_or_else(|| NetError::InterfaceUnavailable(format!("{} has no index", iface.name)))?;
    if target.is_ipv4() {
        socket.bind_device_by_index_v4(Some(index))?;
    } else {
        socket.bind_device_by_index_v6(Some(index))?;
    }
    Ok(())
}

/// Windows needs only the source address (strong host model); other platforms never get here
/// because [`resolve`] refuses them.
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios"
)))]
#[allow(clippy::unnecessary_wraps)]
fn pin_to_device(
    _socket: &Socket,
    _iface: &BoundInterface,
    _target: &SocketAddr,
) -> Result<(), NetError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_interfaces_including_loopback() {
        let list = list_interfaces().unwrap();
        assert!(list.iter().any(|i| i.is_loopback), "{list:?}");
    }

    #[test]
    fn refuses_missing_and_loopback_interfaces() {
        let missing = resolve("torrentplayer-test-no-such-interface").unwrap_err();
        assert!(
            matches!(missing, NetError::InterfaceUnavailable(_)),
            "{missing:?}"
        );
        let loopback = list_interfaces()
            .unwrap()
            .into_iter()
            .find(|i| i.is_loopback)
            .unwrap();
        let error = resolve(&loopback.name).unwrap_err();
        assert!(
            matches!(error, NetError::InterfaceUnavailable(_)),
            "{error:?}"
        );
    }

    #[test]
    fn recognizes_vpn_adapter_names() {
        for name in [
            "wg0",
            "tun0",
            "utun3",
            "NordLynx",
            "ProtonVPN",
            "Mullvad",
            "Ethernet 2 (VPN)",
        ] {
            assert!(looks_like_vpn(name), "{name}");
        }
        for name in ["Wi-Fi", "eth0", "en0", "Ethernet"] {
            assert!(!looks_like_vpn(name), "{name}");
        }
    }

    #[tokio::test]
    async fn bound_connections_use_the_interface_address() {
        // Loopback is refused by `resolve`, but `connect` itself works with any address, which
        // lets this test run without a second interface.
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let iface = BoundInterface {
            name: "lo".into(),
            v4: Some(Ipv4Addr::LOCALHOST),
            v6: None,
            index: None,
        };
        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios"
        )))]
        {
            let stream = connect(&iface, listener.local_addr().unwrap())
                .await
                .unwrap();
            assert_eq!(
                stream.local_addr().unwrap().ip(),
                IpAddr::V4(Ipv4Addr::LOCALHOST)
            );
        }
        let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, 1));
        assert!(matches!(
            connect(&iface, v6).await,
            Err(NetError::InterfaceUnavailable(_))
        ));
        drop(listener);
    }

    #[tokio::test]
    async fn listeners_bind_only_the_given_address() {
        let iface = BoundInterface {
            name: "lo".into(),
            v4: Some(Ipv4Addr::LOCALHOST),
            v6: None,
            index: None,
        };
        // As above, pinning to a device needs a real interface outside Windows.
        let pinned = cfg!(not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios"
        )))
        .then_some(&iface);
        let listener = listen(pinned, SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
        let addr = listener.local_addr().unwrap();
        assert_eq!(addr.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let client = TcpStream::connect(addr).await.unwrap();
        let (_accepted, from) = listener.accept().await.unwrap();
        assert_eq!(from, client.local_addr().unwrap());
        // The same port cannot be opened twice.
        assert!(listen(None, addr).is_err());
    }
}
