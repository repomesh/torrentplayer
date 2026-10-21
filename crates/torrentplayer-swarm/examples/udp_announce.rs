// SPDX-License-Identifier: MIT OR Apache-2.0

//! Announces to UDP trackers through TorrentPlayer's network manager, to check that UDP trackers work
//! in a mode (direct, or through a SOCKS5 proxy).
//!
//! `cargo run -p torrentplayer-swarm --example udp_announce -- <direct|proxy-host:port> <info-hash-hex> <udp-url>...`

// A diagnostic tool: printing is the point.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::time::Duration;

use torrentplayer_net::{Network, NetworkConfig, NetworkMode, NetworkOptions, Socks5Config};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [mode, hash, trackers @ ..] = args.as_slice() else {
        eprintln!("usage: udp_announce <direct|proxy-host:port> <info-hash-hex> <udp-url>...");
        return Ok(());
    };
    let mode = if mode == "direct" {
        NetworkMode::Direct
    } else {
        let (host, port) = mode
            .rsplit_once(':')
            .ok_or("expected direct or host:port")?;
        NetworkMode::Socks5(Socks5Config {
            host: host.to_owned(),
            port: port.parse()?,
            username: None,
            password: None,
            remote_dns: true,
            proxy_trackers: true,
            proxy_dht: true,
        })
    };
    let net = Network::start(
        NetworkConfig {
            mode,
            allow_ipv6: false,
            hold: None,
        },
        NetworkOptions::default(),
    )
    .await?;
    let status = net.settled(Duration::from_secs(10)).await;
    println!(
        "mode ready: traffic={} udp trackers={}",
        status.traffic_allowed, status.udp_allowed
    );
    let mut info_hash = [0u8; 20];
    for (i, b) in info_hash.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hash[i * 2..i * 2 + 2], 16)?;
    }
    let urls: Vec<url::Url> = trackers
        .iter()
        .filter_map(|t| url::Url::parse(t).ok())
        .collect();
    if let Some(first) = urls.first() {
        let addr =
            tokio::net::lookup_host((first.host_str().unwrap_or(""), first.port().unwrap_or(0)))
                .await?
                .find(std::net::SocketAddr::is_ipv4);
        if let Some(addr) = addr {
            let (socket, _) = net.udp_socket(addr).await?;
            println!("socket bound to {}", socket.local_addr()?);
        }
    }
    let peers = torrentplayer_swarm::udp_tracker::announce_all(
        Some(&net),
        &urls,
        info_hash,
        Duration::from_secs(15),
    )
    .await;
    println!("{} peers", peers.len());
    net.shutdown();
    Ok(())
}
