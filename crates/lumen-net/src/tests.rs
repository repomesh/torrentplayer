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
    ConnectionState, NetError, Network, NetworkConfig, NetworkMode, NetworkOptions, Secret,
    Socks5Config, socks5,
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
    assert!(!status.udp_allowed, "no UDP through a proxy");
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
                interface: "lumen-test-no-such-interface".into(),
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
    let wrong = socks5::connect(&mut stream, &target, Some(("lumen", "guess"))).await;
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
    socks5::connect(&mut stream, &target, Some(("lumen", &password)))
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
async fn udp_is_refused_through_a_proxy_and_by_the_kill_switch() {
    let target: SocketAddr = "198.51.100.7:6969".parse().unwrap();
    let proxy = TestProxy::start(None).await.unwrap();
    let net = Network::start(socks_mode(&proxy, None), fast_options())
        .await
        .unwrap();
    net.settled(Duration::from_secs(5)).await;
    assert!(!net.policy().bound_udp_allowed);
    assert!(matches!(
        net.udp_socket(target).await,
        Err(NetError::Unsupported(_))
    ));
    net.shutdown();

    let net = Network::start(
        NetworkConfig {
            mode: NetworkMode::BindInterface {
                interface: "lumen-test-no-such-interface".into(),
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
