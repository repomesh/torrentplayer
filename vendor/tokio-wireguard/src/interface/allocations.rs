use std::{
    any::type_name,
    fmt, iter,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use rand::Rng;

#[derive(Clone)]
pub struct Allocations {
    pub address: crate::config::Address,
    list: Arc<[Port]>,
}

#[derive(Clone, Copy)]
pub struct Allocation {
    address: SocketAddr,
    proto: Proto,
}

#[derive(Clone, Copy)]
pub enum Proto {
    Tcp,
    Udp,
}

impl Allocations {
    const EPHEMERAL_RANGE: Range<u16> = Range {
        start: 49152,
        end: 65535,
    };

    pub fn new(config: &crate::config::Interface) -> Self {
        Self {
            address: config.address,
            list: iter::repeat_with(Port::new)
                .take(u16::MAX as usize)
                .collect(),
        }
    }

    // TODO: Rework this to allow any src+dst+port for TCP
    pub fn acquire(
        &self,
        address: impl Into<SocketAddr>,
        proto: Proto,
        rng: &mut impl Rng,
    ) -> Option<Allocation> {
        let address = address.into();
        let ip = address.ip();

        match address.port() {
            0 => {
                // Modified for TorrentPlayer: try every ephemeral port once, from a random
                // start. The loop used to stop as soon as the start port was taken (its check
                // matched on the first try), so one collision failed the bind.
                let start = rng.gen_range(Self::EPHEMERAL_RANGE);
                let mut port = start;
                loop {
                    if let Some(allocation) = self.acquire((ip, port), proto, rng) {
                        break Some(allocation);
                    }
                    port = if port + 1 >= Self::EPHEMERAL_RANGE.end {
                        Self::EPHEMERAL_RANGE.start
                    } else {
                        port + 1
                    };
                    if port == start {
                        break None;
                    }
                }
            }

            port => {
                let slot = self.list.get(port as usize)?;

                let ip = match ip {
                    IpAddr::V4(ip) => {
                        let interface = self.address.v4()?;
                        match ip {
                            Ipv4Addr::UNSPECIFIED => IpAddr::V4(interface),
                            _ if ip == interface => IpAddr::V4(interface),
                            _ => return None,
                        }
                    }
                    IpAddr::V6(ip) => {
                        let interface = self.address.v6()?;
                        match ip {
                            Ipv6Addr::UNSPECIFIED => IpAddr::V6(interface),
                            _ if ip == interface => IpAddr::V6(interface),
                            _ => return None,
                        }
                    }
                };

                match (slot, ip, proto) {
                    (Port { tcp_v4: slot, .. }, IpAddr::V4(..), Proto::Tcp)
                    | (Port { tcp_v6: slot, .. }, IpAddr::V6(..), Proto::Tcp)
                    | (Port { udp_v4: slot, .. }, IpAddr::V4(..), Proto::Udp)
                    | (Port { udp_v6: slot, .. }, IpAddr::V6(..), Proto::Udp)
                        if slot
                            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                            .is_ok() =>
                    {
                        Some(Allocation {
                            address: SocketAddr::new(ip, port),
                            proto,
                        })
                    }
                    _ => None,
                }
            }
        }
    }

    pub fn release(&self, allocation: Allocation) {
        if let Some(slot) = self.list.get(allocation.address.port() as usize) {
            match (allocation.address.ip(), allocation.proto) {
                (IpAddr::V4(..), Proto::Tcp) => slot.tcp_v4.store(false, Ordering::Release),
                (IpAddr::V6(..), Proto::Tcp) => slot.tcp_v6.store(false, Ordering::Release),
                (IpAddr::V4(..), Proto::Udp) => slot.udp_v4.store(false, Ordering::Release),
                (IpAddr::V6(..), Proto::Udp) => slot.udp_v6.store(false, Ordering::Release),
            }
        }
    }
}

impl Allocation {
    pub fn address(&self) -> SocketAddr {
        self.address
    }
}

impl fmt::Debug for Allocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple(type_name::<Self>())
            .field(&self.address)
            .finish()
    }
}

struct Port {
    tcp_v4: AtomicBool,
    tcp_v6: AtomicBool,
    udp_v4: AtomicBool,
    udp_v6: AtomicBool,
}

impl Port {
    const fn new() -> Self {
        Self {
            tcp_v4: AtomicBool::new(false),
            tcp_v6: AtomicBool::new(false),
            udp_v4: AtomicBool::new(false),
            udp_v6: AtomicBool::new(false),
        }
    }
}
