// SPDX-License-Identifier: MIT OR Apache-2.0

//! End-to-end tests of the network manager on the loopback interface: a recording SOCKS5 proxy,
//! the kill switch, the bridge, and the connection check. Nothing leaves the machine.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

use crate::test_util::TestProxy;
use crate::wireguard::WireGuardConfig;
use crate::{
    ConnectionState, ExternalListener, IncomingConfig, IncomingStatus, NetError, Network,
    NetworkConfig, NetworkMode, NetworkOptions, RANDOM_PORTS, Secret, Socks5Config, socks5,
};

fn fast_options() -> NetworkOptions {
    NetworkOptions {
        health_interval: Duration::from_millis(100),
        retry_interval: Duration::from_millis(50),
        check_timeout: Duration::from_secs(2),
        connect_timeout: Duration::from_secs(5),
        ..NetworkOptions::default()
    }
}

fn socks_mode(proxy: &TestProxy, credentials: Option<(&str, &str)>) -> NetworkConfig {
    NetworkConfig {
        mode: NetworkMode::Socks5(Socks5Config {
            host: proxy.addr().ip().to_string(),
            port: proxy.addr().port(),
            username: credentials.map(|(u, _)| u.to_owned()),
            password: credentials.map(|(_, p)| Secret::new(p)),
            remote_dns: true,
            proxy_trackers: true,
            proxy_dht: true,
        }),
        allow_ipv6: false,
    }
}

/// A server that answers every connection with a fixed HTTP response and counts connections.
async fn http_server(body: &'static str) -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    (addr, hits)
}

/// A server that echoes one line and counts connections.
async fn echo_server() -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buf = [0u8; 64];
                while let Ok(n) = stream.read(&mut buf).await {
                    if n == 0 || stream.write_all(&buf[..n]).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (addr, hits)
}

async fn wait_for_state(net: &Network, want: impl Fn(&ConnectionState) -> bool) -> ConnectionState {
    let mut rx = net.subscribe();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let state = net.status().state;
            if want(&state) {
                return state;
            }
            let _ = tokio::time::timeout(Duration::from_millis(50), rx.changed()).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("state never matched; last: {:?}", net.status()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn socks_mode_sends_everything_through_the_proxy() {
    let proxy = TestProxy::start(Some(("alice", "pw"))).await.unwrap();
    let (echo, echo_hits) = echo_server().await;
    let net = Network::start(socks_mode(&proxy, Some(("alice", "pw"))), fast_options())
        .await
        .unwrap();
    let status = net.settled(Duration::from_secs(5)).await;
    assert_eq!(status.state, ConnectionState::Connected, "{status:?}");
    assert!(
        status.udp_allowed,
        "UDP may use a proxy association, never an unbound destination socket"
    );
    assert!(
        status.connected_since_ms.is_some(),
        "uptime is known once connected"
    );

    let (mut stream, lease) = net.connect(echo).await.unwrap();
    assert!(lease.is_valid());
    stream.write_all(b"ping").await.unwrap();
    let mut back = [0u8; 4];
    stream.read_exact(&mut back).await.unwrap();
    assert_eq!(&back, b"ping");

    let relayed = proxy.connections();
    assert_eq!(relayed.len(), 1, "{relayed:?}");
    assert_eq!(relayed[0].target, echo.to_string());
    assert_eq!(echo_hits.load(Ordering::SeqCst), 1);
    net.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_clients_resolve_names_through_the_proxy() {
    let proxy = TestProxy::start(None).await.unwrap();
    let (server, hits) = http_server(r#"{"ok":true}"#).await;
    let net = Network::start(socks_mode(&proxy, None), fast_options())
        .await
        .unwrap();
    net.settled(Duration::from_secs(5)).await;
    let client = net
        .configure_http(reqwest::Client::builder())
        .unwrap()
        .build()
        .unwrap();
    let url = format!("http://localhost:{}/announce", server.port());
    let body = client.get(url).send().await.unwrap().text().await.unwrap();
    assert_eq!(body, r#"{"ok":true}"#);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    // The bridge handed the name to the proxy unresolved (remote DNS).
    let targets: Vec<String> = proxy.connections().into_iter().map(|c| c.target).collect();
    assert_eq!(targets, [format!("localhost:{}", server.port())]);
    net.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_only_clients_cannot_reach_the_local_network() {
    let (server, hits) = http_server("ok").await;
    let net = Network::start(NetworkConfig::default(), fast_options())
        .await
        .unwrap();
    let open = net
        .configure_http(reqwest::Client::builder())
        .unwrap()
        .build()
        .unwrap();
    let public = net
        .configure_http_public(reqwest::Client::builder())
        .unwrap()
        .build()
        .unwrap();
    let by_ip = format!("http://127.0.0.1:{}/", server.port());
    let by_name = format!("http://localhost:{}/", server.port());
    assert_eq!(
        open.get(&by_ip).send().await.unwrap().text().await.unwrap(),
        "ok"
    );
    assert!(public.get(&by_ip).send().await.is_err());
    assert!(public.get(&by_name).send().await.is_err());
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "only the unrestricted client connected"
    );
    assert!(matches!(
        net.connect_host_public("127.0.0.1", server.port()).await,
        Err(NetError::NotPublic)
    ));
    assert!(matches!(
        net.connect_host_public("LocalHost.", server.port()).await,
        Err(NetError::NotPublic)
    ));
    net.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_kill_switch_blocks_and_recovers() {
    let proxy = TestProxy::start(None).await.unwrap();
    let (echo, echo_hits) = echo_server().await;
    let (server, http_hits) = http_server(r#"{"ip":"203.0.113.9"}"#).await;
    let net = Network::start(socks_mode(&proxy, None), fast_options())
        .await
        .unwrap();
    net.settled(Duration::from_secs(5)).await;
    let (mut stream, mut lease) = net.connect(echo).await.unwrap();
    let epoch = net.policy().epoch;

    proxy.stop().await;
    let state = wait_for_state(&net, |s| {
        matches!(s, ConnectionState::KillSwitchActive { .. })
    })
    .await;
    assert!(matches!(state, ConnectionState::KillSwitchActive { .. }));
    assert!(net.status().kill_switch_engaged);
    assert_eq!(
        net.status().connected_since_ms,
        None,
        "no uptime while blocked"
    );
    assert!(
        net.policy().epoch > epoch,
        "the kill switch starts a new epoch"
    );
    tokio::time::timeout(Duration::from_secs(1), lease.revoked())
        .await
        .expect("leases are revoked when the kill switch engages");
    // The relayed connection is gone too.
    let mut buf = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf)).await;
    assert!(matches!(read, Ok(Ok(0) | Err(_))), "{read:?}");

    // Nothing gets out while blocked: not peers, not HTTP, not the connection check.
    assert!(matches!(
        net.connect(echo).await,
        Err(NetError::KillSwitch(_))
    ));
    let client = net
        .configure_http(reqwest::Client::builder())
        .unwrap()
        .build()
        .unwrap();
    assert!(
        client
            .get(format!("http://{server}/"))
            .send()
            .await
            .is_err()
    );
    assert!(matches!(
        net.check_public_ip().await,
        Err(NetError::KillSwitch(_))
    ));
    assert_eq!(
        echo_hits.load(Ordering::SeqCst),
        1,
        "no direct connection was made"
    );
    assert_eq!(
        http_hits.load(Ordering::SeqCst),
        0,
        "no direct request was made"
    );

    proxy.restart().await.unwrap();
    wait_for_state(&net, |s| *s == ConnectionState::Connected).await;
    let (mut stream, _lease) = net.connect(echo).await.unwrap();
    stream.write_all(b"x").await.unwrap();
    stream.read_exact(&mut buf).await.unwrap();
    assert_eq!(echo_hits.load(Ordering::SeqCst), 2);
    let messages: Vec<String> = net.status().events.into_iter().map(|e| e.message).collect();
    assert!(
        messages.iter().any(|m| m.contains("Kill switch")),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("restored")),
        "{messages:?}"
    );
    net.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_protected_mode_blocks_until_verified_and_never_falls_back() {
    // Nothing listens on this port, so the proxy is down from the start.
    let unused = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let dead = unused.local_addr().unwrap();
    drop(unused);
    let (echo, echo_hits) = echo_server().await;
    let config = NetworkConfig {
        mode: NetworkMode::Socks5(Socks5Config {
            host: dead.ip().to_string(),
            port: dead.port(),
            username: None,
            password: None,
            remote_dns: true,
            proxy_trackers: true,
            proxy_dht: true,
        }),
        allow_ipv6: false,
    };
    let net = Network::start(config, fast_options()).await.unwrap();
    let status = net.settled(Duration::from_secs(5)).await;
    assert!(
        matches!(status.state, ConnectionState::KillSwitchActive { .. }),
        "{status:?}"
    );
    assert!(matches!(
        net.connect(echo).await,
        Err(NetError::KillSwitch(_))
    ));
    assert_eq!(echo_hits.load(Ordering::SeqCst), 0);
    net.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changing_modes_revokes_leases() {
    let proxy = TestProxy::start(None).await.unwrap();
    let (echo, _) = echo_server().await;
    let net = Network::start(NetworkConfig::default(), fast_options())
        .await
        .unwrap();
    assert_eq!(net.status().state, ConnectionState::Direct);
    assert!(net.policy().udp_allowed);
    let (_direct, mut lease) = net.connect(echo).await.unwrap();
    assert!(
        proxy.connections().is_empty(),
        "direct mode does not use the proxy"
    );

    net.apply(socks_mode(&proxy, None)).unwrap();
    tokio::time::timeout(Duration::from_secs(1), lease.revoked())
        .await
        .expect("the direct connection's lease is revoked");
    // Applying the same settings again does not cut connections.
    net.settled(Duration::from_secs(5)).await;
    let (_proxied, lease) = net.connect(echo).await.unwrap();
    net.apply(socks_mode(&proxy, None)).unwrap();
    assert!(lease.is_valid());
    assert_eq!(proxy.connections().len(), 1);
    net.shutdown();
    assert!(!lease.is_valid());
    assert_eq!(net.status().state, ConnectionState::Disconnected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_connection_check_goes_through_the_current_mode() {
    let proxy = TestProxy::start(None).await.unwrap();
    let (server, hits) = http_server(r#"{"ip":"203.0.113.9"}"#).await;
    let options = NetworkOptions {
        ip_check_v4_url: format!("http://{server}/?format=json"),
        ip_check_v6_url: None,
        ..fast_options()
    };
    let net = Network::start(socks_mode(&proxy, None), options)
        .await
        .unwrap();
    net.settled(Duration::from_secs(5)).await;
    let ip = net.check_public_ip().await.unwrap();
    assert_eq!(ip.ip, "203.0.113.9");
    assert_eq!(ip.ipv4.as_deref(), Some("203.0.113.9"));
    assert_eq!(ip.ipv6, None);
    assert_eq!(ip.service, "127.0.0.1");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(proxy.connections()[0].target, server.to_string());
    net.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wireguard_blocks_everything_until_the_server_authenticates() {
    const PRIVATE: &str = "yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=";
    const PUBLIC: &str = "xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=";
    let text = format!(
        "[Interface]\nPrivateKey = {PRIVATE}\nAddress = 10.0.0.2/32\n[Peer]\nPublicKey = {PUBLIC}\n\
         AllowedIPs = 0.0.0.0/0\nEndpoint = 198.51.100.1:51820\n"
    );
    let (config, _) = WireGuardConfig::parse(&text).unwrap();
    let (server, hits) = http_server(r#"{"ip":"203.0.113.9"}"#).await;
    let options = NetworkOptions {
        ip_check_v4_url: format!("http://{server}/"),
        ip_check_v6_url: None,
        ..fast_options()
    };
    let net = Network::start(
        NetworkConfig {
            mode: NetworkMode::WireGuard(config),
            allow_ipv6: false,
        },
        options,
    )
    .await
    .unwrap();
    let status = net.status();
    assert!(
        matches!(status.state, ConnectionState::Connecting),
        "{status:?}"
    );
    assert!(status.kill_switch_engaged);
    assert_eq!(status.endpoint.as_deref(), Some("198.51.100.1:51820"));
    assert!(!format!("{status:?}").contains(PRIVATE));
    assert!(matches!(
        net.connect(server).await,
        Err(NetError::KillSwitch(_))
    ));
    assert!(matches!(
        net.check_public_ip().await,
        Err(NetError::KillSwitch(_))
    ));
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    net.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_interface_engages_the_kill_switch() {
    let (echo, hits) = echo_server().await;
    let net = Network::start(
        NetworkConfig {
            mode: NetworkMode::BindInterface {
                interface: "torrentplayer-test-no-such-interface".into(),
            },
            allow_ipv6: false,
        },
        fast_options(),
    )
    .await
    .unwrap();
    let status = net.settled(Duration::from_secs(5)).await;
    assert!(
        matches!(&status.state, ConnectionState::KillSwitchActive { reason } if reason.contains("not found")),
        "{status:?}"
    );
    assert!(matches!(
        net.connect(echo).await,
        Err(NetError::KillSwitch(_))
    ));
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    net.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ipv6_is_refused_in_protected_modes_unless_allowed() {
    let proxy = TestProxy::start(None).await.unwrap();
    let net = Network::start(socks_mode(&proxy, None), fast_options())
        .await
        .unwrap();
    net.settled(Duration::from_secs(5)).await;
    let v6: SocketAddr = "[2001:db8::1]:6881".parse().unwrap();
    assert!(matches!(net.connect(v6).await, Err(NetError::Ipv6Disabled)));
    assert!(proxy.connections().is_empty());
    net.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_bridge_refuses_clients_without_the_token() {
    let (echo, hits) = echo_server().await;
    let net = Network::start(NetworkConfig::default(), fast_options())
        .await
        .unwrap();
    let url = url::Url::parse(net.bridge_proxy_url().expose()).unwrap();
    let bridge: SocketAddr = format!("{}:{}", url.host_str().unwrap(), url.port().unwrap())
        .parse()
        .unwrap();
    assert!(bridge.ip().is_loopback());

    let target = socks5::Target::Addr(echo);
    let mut stream = tokio::net::TcpStream::connect(bridge).await.unwrap();
    let wrong = socks5::connect(&mut stream, &target, Some(("torrentplayer", "guess"))).await;
    assert!(
        matches!(wrong, Err(socks5::Socks5Error::AuthFailed)),
        "{wrong:?}"
    );
    let mut stream = tokio::net::TcpStream::connect(bridge).await.unwrap();
    let none = socks5::connect(&mut stream, &target, None).await;
    assert!(
        matches!(none, Err(socks5::Socks5Error::NoAcceptableMethod)),
        "{none:?}"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0);

    let mut stream = tokio::net::TcpStream::connect(bridge).await.unwrap();
    let password = url.password().unwrap().to_owned();
    socks5::connect(&mut stream, &target, Some(("torrentplayer", &password)))
        .await
        .unwrap();
    // The bridge has connected; the echo server's task may not have counted it yet.
    tokio::time::timeout(Duration::from_secs(5), async {
        while hits.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    net.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_incomplete_configuration_blocks_traffic() {
    let (echo, hits) = echo_server().await;
    // A username whose password is missing (for example, not in the credential store).
    let config = NetworkConfig {
        mode: NetworkMode::Socks5(Socks5Config {
            host: "127.0.0.1".into(),
            port: 1080,
            username: Some("alice".into()),
            password: None,
            remote_dns: true,
            proxy_trackers: true,
            proxy_dht: true,
        }),
        allow_ipv6: false,
    };
    let net = Network::start(config.clone(), fast_options())
        .await
        .unwrap();
    let status = net.status();
    assert!(
        matches!(&status.state, ConnectionState::Error { reason } if reason.contains("password")),
        "{status:?}"
    );
    assert!(matches!(
        net.connect(echo).await,
        Err(NetError::KillSwitch(_))
    ));
    // Fixing the settings lifts the block.
    net.apply(NetworkConfig::default()).unwrap();
    assert!(net.connect(echo).await.is_ok());
    // Breaking them again blocks again.
    net.apply(config).unwrap();
    assert!(matches!(
        net.connect(echo).await,
        Err(NetError::KillSwitch(_))
    ));
    // The echo server counts the allowed connection when it accepts it, which can lag behind
    // `connect` returning on a busy machine; give it a moment, then check nothing else arrived.
    for _ in 0..100 {
        if hits.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    net.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bound_udp_sockets_use_the_interface_address() {
    // Any up, non-loopback interface with an IPv4 address will do; skip if the machine has none.
    let Some((name, v4)) = crate::list_interfaces()
        .unwrap()
        .into_iter()
        .filter(|i| i.is_up && !i.is_loopback)
        .find_map(|i| {
            let v4 = i
                .addresses
                .iter()
                .filter_map(|a| a.parse::<Ipv4Addr>().ok())
                .find(|a| !a.is_link_local())?;
            Some((i.name, v4))
        })
    else {
        return;
    };
    let net = Network::start(
        NetworkConfig {
            mode: NetworkMode::BindInterface { interface: name },
            allow_ipv6: false,
        },
        fast_options(),
    )
    .await
    .unwrap();
    let status = net.settled(Duration::from_secs(5)).await;
    if !status.traffic_allowed {
        net.shutdown();
        return;
    }
    let policy = net.policy();
    assert!(policy.bound_udp_allowed && !policy.udp_allowed);
    assert!(status.udp_allowed, "UDP trackers are usable when bound");
    let target: SocketAddr = "198.51.100.7:6969".parse().unwrap();
    let (socket, lease) = net.udp_socket(target).await.unwrap();
    assert_eq!(socket.local_addr().unwrap().ip(), std::net::IpAddr::V4(v4));
    assert!(lease.is_valid());
    let v6: SocketAddr = "[2001:db8::1]:6969".parse().unwrap();
    assert!(matches!(
        net.udp_socket(v6).await,
        Err(NetError::Ipv6Disabled)
    ));
    net.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsupported_proxy_udp_is_refused_without_fallback_and_the_kill_switch_blocks_udp() {
    let target: SocketAddr = "198.51.100.7:6969".parse().unwrap();
    let proxy = TestProxy::start(None).await.unwrap();
    let net = Network::start(socks_mode(&proxy, None), fast_options())
        .await
        .unwrap();
    net.settled(Duration::from_secs(5)).await;
    assert!(net.policy().bound_udp_allowed);
    assert!(matches!(
        net.udp_socket(target).await,
        Err(NetError::Proxy(_))
    ));
    net.shutdown();

    let net = Network::start(
        NetworkConfig {
            mode: NetworkMode::BindInterface {
                interface: "torrentplayer-test-no-such-interface".into(),
            },
            allow_ipv6: false,
        },
        fast_options(),
    )
    .await
    .unwrap();
    net.settled(Duration::from_secs(5)).await;
    assert!(matches!(
        net.udp_socket(target).await,
        Err(NetError::KillSwitch(_))
    ));
    net.shutdown();
}

#[tokio::test]
async fn socks_udp_uses_only_the_relay_and_dies_with_its_control_connection() {
    use crate::test_util::TestUdpProxy;
    let sink = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let destination = sink.local_addr().unwrap();
    let proxy = TestUdpProxy::start(move |_, bytes| Some((destination, bytes.to_vec())))
        .await
        .unwrap();
    let cfg = NetworkConfig {
        mode: NetworkMode::Socks5(Socks5Config {
            host: proxy.addr().ip().to_string(),
            port: proxy.addr().port(),
            username: None,
            password: None,
            remote_dns: true,
            proxy_trackers: true,
            proxy_dht: true,
        }),
        allow_ipv6: false,
    };
    let net = Network::start(cfg, fast_options()).await.unwrap();
    assert!(net.settled(Duration::from_secs(3)).await.traffic_allowed);
    let (socket, lease) = net.udp_socket(destination).await.unwrap();
    socket.send_to(b"via proxy", destination).await.unwrap();
    let mut bytes = [0; 64];
    let (n, from) = tokio::time::timeout(Duration::from_secs(2), socket.recv_from(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&bytes[..n], b"via proxy");
    assert_eq!(from, destination);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), sink.recv_from(&mut bytes))
            .await
            .is_err(),
        "no direct UDP reached the destination"
    );
    assert_eq!(proxy.targets(), vec![socks5::Target::Addr(destination)]);
    assert!(
        socket
            .send_to(b"v6", "[::1]:1".parse().unwrap())
            .await
            .is_err()
    );
    proxy.stop();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), socket.recv_from(&mut bytes))
            .await
            .unwrap()
            .is_err()
    );
    assert!(socket.send_to(b"closed", destination).await.is_err());
    net.apply(NetworkConfig::default()).unwrap();
    assert!(!lease.is_valid());
    assert!(socket.send_to(b"changed", destination).await.is_err());
    net.shutdown();
}

#[tokio::test]
async fn socks_udp_domain_targets_do_not_use_local_dns_and_old_leases_fail_closed() {
    use crate::test_util::TestUdpProxy;
    let logical: SocketAddr = "8.8.8.8:6969".parse().unwrap();
    let proxy = TestUdpProxy::start(move |_, bytes| Some((logical, bytes.to_vec())))
        .await
        .unwrap();
    let net = Network::start(
        NetworkConfig {
            mode: NetworkMode::Socks5(Socks5Config {
                host: proxy.addr().ip().to_string(),
                port: proxy.addr().port(),
                username: None,
                password: None,
                remote_dns: true,
                proxy_trackers: true,
                proxy_dht: true,
            }),
            allow_ipv6: false,
        },
        fast_options(),
    )
    .await
    .unwrap();
    net.settled(Duration::from_secs(3)).await;
    let (socket, target, lease) = net
        .udp_socket_host("never-resolves.invalid", 6969)
        .await
        .unwrap();
    assert!(matches!(target, socks5::Target::Domain(_, 6969)));
    socket.send_to_target(b"domain", &target).await.unwrap();
    let mut bytes = [0; 64];
    let (n, from) = tokio::time::timeout(Duration::from_secs(2), socket.recv_from(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&bytes[..n], b"domain");
    assert_eq!(from, logical);
    net.apply(NetworkConfig::default()).unwrap();
    assert!(!lease.is_valid());
    assert!(socket.send_to_target(b"old", &target).await.is_err());
    assert!(socket.recv_from(&mut bytes).await.is_err());
    assert_eq!(proxy.targets().len(), 1);
    net.shutdown();
}

#[test]
fn reads_plain_text_and_json_connection_check_answers() {
    use crate::network::parse_ip_answer;
    let v4: std::net::IpAddr = "203.0.113.7".parse().unwrap();
    let v6: std::net::IpAddr = "2001:db8::7".parse().unwrap();
    assert_eq!(parse_ip_answer(b"203.0.113.7\n"), Some(v4));
    assert_eq!(parse_ip_answer(b"2001:db8::7\n"), Some(v6));
    assert_eq!(parse_ip_answer(br#"{"ip":"203.0.113.7"}"#), Some(v4));
    assert_eq!(parse_ip_answer(b"<html>blocked</html>"), None);
    assert_eq!(parse_ip_answer(b""), None);
}

/// A port nothing listens on right now.
fn free_port() -> u16 {
    let probe = std::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
    probe.local_addr().unwrap().port()
}

async fn wait_for_incoming(
    net: &Network,
    want: impl Fn(&IncomingStatus) -> bool,
) -> IncomingStatus {
    let mut rx = net.status_changes();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = net.status().incoming;
            if want(&status) {
                return status;
            }
            let _ = tokio::time::timeout(Duration::from_millis(50), rx.changed()).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("incoming status never matched; last: {:?}", net.status()))
}

/// Whether anything accepts a TCP connection on `127.0.0.1:port`.
async fn accepts_on_loopback(port: u16) -> bool {
    tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)),
    )
    .await
    .is_ok_and(|r| r.is_ok())
}

fn incoming(port: u16) -> IncomingConfig {
    IncomingConfig {
        enabled: true,
        port,
        upnp: false,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incoming_connections_are_off_until_turned_on_and_follow_the_setting() {
    let net = Network::start(NetworkConfig::default(), fast_options())
        .await
        .unwrap();
    let mut peers = net.incoming_peers();
    assert_eq!(net.status().incoming, IncomingStatus::Off);
    assert_eq!(net.incoming_port(), None);

    let port = free_port();
    net.set_incoming(incoming(port));
    let status = wait_for_incoming(&net, |s| matches!(s, IncomingStatus::Listening { .. })).await;
    assert!(
        matches!(&status, IncomingStatus::Listening { port: p, upnp: false, addresses }
            if *p == port && addresses.contains(&format!("0.0.0.0:{port}"))),
        "{status:?}"
    );
    assert_eq!(net.incoming_port(), Some(port));

    let mut client = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    let mut peer = tokio::time::timeout(Duration::from_secs(5), peers.next())
        .await
        .unwrap()
        .unwrap();
    assert!(peer.lease.is_valid());
    assert_eq!(peer.addr, client.local_addr().unwrap());
    client.write_all(b"hi").await.unwrap();
    let mut hello = [0u8; 2];
    peer.stream.read_exact(&mut hello).await.unwrap();
    assert_eq!(&hello, b"hi");

    // Moving the port reopens the listener but keeps existing connections.
    let moved = free_port();
    net.set_incoming(incoming(moved));
    wait_for_incoming(
        &net,
        |s| matches!(s, IncomingStatus::Listening { port, .. } if *port == moved),
    )
    .await;
    assert!(peer.lease.is_valid(), "a port change closes no connection");
    assert!(!accepts_on_loopback(port).await, "the old port is closed");
    assert!(accepts_on_loopback(moved).await);

    // Turning it off closes the listener.
    net.set_incoming(IncomingConfig::default());
    wait_for_incoming(&net, |s| *s == IncomingStatus::Off).await;
    assert!(!accepts_on_loopback(moved).await);
    assert_eq!(net.incoming_port(), None);
    net.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_random_port_is_chosen_once_and_kept() {
    let net = Network::start(NetworkConfig::default(), fast_options())
        .await
        .unwrap();
    let _peers = net.incoming_peers();
    net.set_incoming(incoming(0));
    let status = wait_for_incoming(&net, |s| matches!(s, IncomingStatus::Listening { .. })).await;
    let IncomingStatus::Listening { port, .. } = status else {
        unreachable!()
    };
    assert!((RANDOM_PORTS.0..=RANDOM_PORTS.1).contains(&port), "{port}");
    assert_eq!(net.incoming_port(), Some(port));
    assert!(accepts_on_loopback(port).await);
    // Turning it off and on again keeps the port, so trackers and port forwards stay valid.
    net.set_incoming(IncomingConfig::default());
    wait_for_incoming(&net, |s| *s == IncomingStatus::Off).await;
    net.set_incoming(incoming(0));
    wait_for_incoming(
        &net,
        |s| matches!(s, IncomingStatus::Listening { port: p, .. } if *p == port),
    )
    .await;
    net.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_port_in_use_is_reported() {
    let taken = std::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
    let port = taken.local_addr().unwrap().port();
    let net = Network::start(NetworkConfig::default(), fast_options())
        .await
        .unwrap();
    let _peers = net.incoming_peers();
    net.set_incoming(incoming(port));
    let status = wait_for_incoming(&net, |s| matches!(s, IncomingStatus::Failed { .. })).await;
    assert!(
        matches!(status, IncomingStatus::Failed { .. }),
        "{status:?}"
    );
    net.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_consumer_nothing_listens() {
    let net = Network::start(NetworkConfig::default(), fast_options())
        .await
        .unwrap();
    let port = free_port();
    net.set_incoming(incoming(port));
    wait_for_incoming(&net, |s| matches!(s, IncomingStatus::Idle { .. })).await;
    assert!(!accepts_on_loopback(port).await);
    // An engine's own listener is reported instead.
    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));
    net.set_external_listener(Some(ExternalListener::Listening(addr)));
    wait_for_incoming(
        &net,
        |s| matches!(s, IncomingStatus::Listening { port: p, .. } if *p == port),
    )
    .await;
    // A consumer that goes away closes the listener again. Wait for the engine's listener to
    // be gone first: otherwise its stale `Listening` status satisfies the wait below before
    // the network manager has bound its own socket.
    net.set_external_listener(None);
    wait_for_incoming(&net, |s| matches!(s, IncomingStatus::Idle { .. })).await;
    let peers = net.incoming_peers();
    wait_for_incoming(&net, |s| matches!(s, IncomingStatus::Listening { .. })).await;
    assert!(accepts_on_loopback(port).await);
    drop(peers);
    wait_for_incoming(&net, |s| matches!(s, IncomingStatus::Idle { .. })).await;
    assert!(!accepts_on_loopback(port).await);
    net.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn socks5_mode_never_listens() {
    let proxy = TestProxy::start(None).await.unwrap();
    let net = Network::start(socks_mode(&proxy, None), fast_options())
        .await
        .unwrap();
    assert_eq!(
        net.settled(Duration::from_secs(5)).await.state,
        ConnectionState::Connected
    );
    let _peers = net.incoming_peers();
    let port = free_port();
    net.set_incoming(incoming(port));
    let status = wait_for_incoming(&net, |s| matches!(s, IncomingStatus::Unsupported { .. })).await;
    assert!(
        matches!(&status, IncomingStatus::Unsupported { reason } if reason.contains("SOCKS5")),
        "{status:?}"
    );
    assert!(!accepts_on_loopback(port).await);
    assert_eq!(net.incoming_port(), None, "no port is announced");

    // Switching to direct opens it; back to the proxy closes it and its connections.
    let mut peers = net.incoming_peers();
    net.apply(NetworkConfig::default()).unwrap();
    wait_for_incoming(&net, |s| matches!(s, IncomingStatus::Listening { .. })).await;
    let _client = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    let mut peer = tokio::time::timeout(Duration::from_secs(5), peers.next())
        .await
        .unwrap()
        .unwrap();
    net.apply(socks_mode(&proxy, None)).unwrap();
    tokio::time::timeout(Duration::from_secs(5), peer.lease.revoked())
        .await
        .unwrap();
    wait_for_incoming(&net, |s| matches!(s, IncomingStatus::Unsupported { .. })).await;
    assert!(!accepts_on_loopback(port).await);
    net.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blocked_protected_mode_does_not_listen() {
    const PRIVATE: &str = "yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=";
    const PUBLIC: &str = "xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=";
    let text = format!(
        "[Interface]\nPrivateKey = {PRIVATE}\nAddress = 10.0.0.2/32\n[Peer]\nPublicKey = {PUBLIC}\n\
         AllowedIPs = 0.0.0.0/0\nEndpoint = 198.51.100.1:51820\n"
    );
    let (config, _) = WireGuardConfig::parse(&text).unwrap();
    let net = Network::start(
        NetworkConfig {
            mode: NetworkMode::WireGuard(config),
            allow_ipv6: false,
        },
        fast_options(),
    )
    .await
    .unwrap();
    let _peers = net.incoming_peers();
    let port = free_port();
    net.set_incoming(incoming(port));
    wait_for_incoming(&net, |s| matches!(s, IncomingStatus::Paused { .. })).await;
    assert!(!accepts_on_loopback(port).await);
    // The same holds for a missing VPN interface.
    net.apply(NetworkConfig {
        mode: NetworkMode::BindInterface {
            interface: "torrentplayer-test-no-such-interface".into(),
        },
        allow_ipv6: false,
    })
    .unwrap();
    wait_for_state(&net, |s| {
        matches!(s, ConnectionState::KillSwitchActive { .. })
    })
    .await;
    wait_for_incoming(&net, |s| matches!(s, IncomingStatus::Paused { .. })).await;
    assert!(!accepts_on_loopback(port).await);
    net.shutdown();
}
