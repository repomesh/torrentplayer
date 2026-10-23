// SPDX-License-Identifier: MIT OR Apache-2.0

//! Downloads a suite entry's `.torrent` link as the app does: through the protected network
//! when one is configured (never around it), and to public hosts only.
//!
//! The app's own client lives in `torrentplayer-torrent` (`source.rs`); this one applies the
//! same rules: links and redirects to private, loopback, or link-local addresses or to
//! `localhost` are refused, at most 5 redirects are followed, only to http(s), and never from
//! https to http.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::{Context as _, bail};
use reqwest::Url;

use crate::network::{RECOVER_TIMEOUT, SharedNetwork};

/// Largest `.torrent` file downloaded.
pub const MAX_TORRENT_BYTES: usize = 16 * 1024 * 1024;

/// Why a link or redirect was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NotPublic;

impl std::fmt::Display for NotPublic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the link leads to the local network, which is not allowed")
    }
}

impl std::error::Error for NotPublic {}

/// Downloads a `.torrent` file from `url`. With `network` (a protected mode), it goes through
/// the tunnel or proxy, after waiting (bounded) for the mode to let traffic through, and fails
/// rather than connect directly; otherwise it connects directly. Either way only to public
/// hosts.
///
/// # Errors
///
/// Fails if the link is not http(s) or leads to the local network, the protected network
/// stays down, the download fails, or the file is larger than [`MAX_TORRENT_BYTES`].
pub async fn fetch_torrent(url: &str, network: Option<&SharedNetwork>) -> anyhow::Result<Vec<u8>> {
    fetch(url, network, false, RECOVER_TIMEOUT).await
}

/// [`fetch_torrent`], with links to the local network allowed (`allow_local`, for tests with a
/// loopback server) and the wait for the protected network bounded by `ready`.
async fn fetch(
    url: &str,
    network: Option<&SharedNetwork>,
    allow_local: bool,
    ready: Duration,
) -> anyhow::Result<Vec<u8>> {
    let url = Url::parse(url).context("not a valid link")?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("only http:// and https:// links can be downloaded");
    }
    if !allow_local && !link_allowed(&url) {
        bail!(NotPublic);
    }
    let client = client(network, allow_local, ready).await?;
    let response = client.get(url).send().await.map_err(|e| {
        if refused(&e) {
            anyhow::Error::new(NotPublic)
        } else {
            anyhow::Error::new(e.without_url())
        }
    })?;
    if response.status().is_redirection() {
        bail!("the link redirects to something other than a web page (a magnet link?)");
    }
    let mut response = response
        .error_for_status()
        .map_err(reqwest::Error::without_url)?;
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        anyhow::ensure!(
            body.len() + chunk.len() <= MAX_TORRENT_BYTES,
            "the .torrent file is larger than 16 MiB"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// The download client: through the protected network's bridge (its public-only user unless
/// `allow_local`) once the mode lets traffic through, or directly with [`PublicResolver`].
async fn client(
    network: Option<&SharedNetwork>,
    allow_local: bool,
    ready: Duration,
) -> anyhow::Result<reqwest::Client> {
    let builder = reqwest::Client::builder()
        .user_agent(concat!("torrentplayer-bench/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::custom(
            move |attempt| match redirect_verdict(attempt.url(), attempt.previous(), allow_local) {
                Redirect::Follow => attempt.follow(),
                Redirect::Stop => attempt.stop(),
                Redirect::Refuse(Some(error)) => attempt.error(error),
                Redirect::Refuse(None) => attempt.error("redirect refused"),
            },
        ));
    let builder = match (network, allow_local) {
        (Some(shared), _) => {
            // The bridge refuses everything while the kill switch is engaged; wait for the
            // tunnel or proxy as runs do, rather than fail at once.
            shared.ready(ready).await?;
            if allow_local {
                shared.network().configure_http(builder)?
            } else {
                shared.network().configure_http_public(builder)?
            }
        }
        (None, false) => builder.dns_resolver(PublicResolver),
        (None, true) => builder,
    };
    Ok(builder.build()?)
}

/// Whether a request failed because the redirect policy or [`PublicResolver`] refused a host.
fn refused(error: &reqwest::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(e) = source {
        if e.is::<NotPublic>() {
            return true;
        }
        source = e.source();
    }
    false
}

/// What to do with a redirect.
#[derive(Debug, PartialEq, Eq)]
enum Redirect {
    Follow,
    /// Not http(s) (a magnet link): stop, for [`fetch`] to report.
    Stop,
    /// Fail the download, with this error if it is one to report as such.
    Refuse(Option<NotPublic>),
}

/// The redirect policy for a redirect to `next` after `previous` (the first request included),
/// as the app's.
fn redirect_verdict(next: &Url, previous: &[Url], allow_local: bool) -> Redirect {
    if previous.len() >= 5 {
        Redirect::Refuse(None)
    } else if !matches!(next.scheme(), "http" | "https") {
        Redirect::Stop
    } else if next.scheme() == "http" && previous.iter().any(|u| u.scheme() == "https") {
        // A downgrade would let anyone on the path change the `.torrent` file.
        Redirect::Refuse(None)
    } else if !allow_local && !link_allowed(next) {
        Redirect::Refuse(Some(NotPublic))
    } else {
        Redirect::Follow
    }
}

/// Whether `url` may be fetched: a public address, or a name other than `localhost`. Names are
/// checked when they are resolved (by the bridge, or [`PublicResolver`]).
fn link_allowed(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return torrentplayer_net::is_public_ip(ip);
    }
    let name = bare.trim_end_matches('.').to_ascii_lowercase();
    name != "localhost" && !name.ends_with(".localhost")
}

/// Resolves names to their public addresses only, for downloads without a protected network.
#[derive(Debug, Clone, Copy)]
struct PublicResolver;

impl reqwest::dns::Resolve for PublicResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|a| torrentplayer_net::is_public_ip(a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(NotPublic.into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;
    use torrentplayer_net::test_util::TestProxy;
    use torrentplayer_net::{
        Network, NetworkConfig, NetworkMode, NetworkOptions, Secret, Socks5Config,
    };

    use super::*;

    const BODY: &[u8] = b"d8:announce0:e";

    /// An HTTP server on loopback that answers every request with [`BODY`] and records who
    /// connected.
    async fn server() -> (SocketAddr, Arc<Mutex<Vec<SocketAddr>>>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let peers = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&peers);
        tokio::spawn(async move {
            while let Ok((mut stream, peer)) = listener.accept().await {
                seen.lock().unwrap().push(peer);
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0u8; 2048];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => request.extend_from_slice(&buf[..n]),
                        }
                    }
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        BODY.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(BODY).await;
                });
            }
        });
        (addr, peers)
    }

    async fn through(proxy: &TestProxy) -> SharedNetwork {
        let config = NetworkConfig {
            mode: NetworkMode::Socks5(Socks5Config {
                host: proxy.addr().ip().to_string(),
                port: proxy.addr().port(),
                username: Some("bench-test".into()),
                password: Some(Secret::new("hunter2")),
                remote_dns: true,
                proxy_trackers: true,
                proxy_dht: true,
            }),
            allow_ipv6: false,
            hold: None,
        };
        let options = NetworkOptions {
            health_interval: Duration::from_millis(200),
            retry_interval: Duration::from_millis(100),
            ..NetworkOptions::default()
        };
        SharedNetwork::wrap(Network::start(config, options).await.unwrap())
    }

    #[tokio::test]
    async fn with_a_protected_network_the_download_goes_through_it() {
        let proxy = TestProxy::start(Some(("bench-test", "hunter2")))
            .await
            .unwrap();
        let shared = through(&proxy).await;
        let (addr, peers) = server().await;
        let url = format!("http://{addr}/a.torrent");

        // Local links allowed only so the loopback server can stand in for a site.
        let body = fetch(&url, Some(&shared), true, Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(body, BODY);
        let proxied: Vec<SocketAddr> = proxy
            .connections()
            .iter()
            .filter(|c| c.target == addr.to_string())
            .map(|c| c.outbound_local)
            .collect();
        let peers = peers.lock().unwrap().clone();
        assert!(!peers.is_empty());
        for peer in peers {
            assert!(
                proxied.contains(&peer),
                "the server saw {peer}, which did not come through the proxy ({proxied:?})"
            );
        }

        // Down: the download waits for the network and fails, never connecting directly.
        shared.shutdown();
        let error = fetch(&url, Some(&shared), true, Duration::from_millis(50))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("tunnel down"), "{error}");
    }

    #[tokio::test]
    async fn links_to_the_local_network_are_refused_in_every_mode() {
        let proxy = TestProxy::start(Some(("bench-test", "hunter2")))
            .await
            .unwrap();
        let shared = through(&proxy).await;
        let (addr, peers) = server().await;
        for url in [
            format!("http://{addr}/a.torrent"),
            format!("http://localhost:{}/a.torrent", addr.port()),
        ] {
            for network in [None, Some(&shared)] {
                let error = fetch_torrent(&url, network).await.unwrap_err();
                assert!(error.is::<NotPublic>(), "{url}: {error:#}");
            }
        }
        assert!(peers.lock().unwrap().is_empty(), "the server was contacted");
        assert!(fetch_torrent("magnet:?xt=urn:btih:00", None).await.is_err());
        shared.shutdown();
    }

    #[tokio::test]
    async fn the_direct_client_resolves_to_public_addresses_only() {
        use reqwest::dns::Resolve as _;
        let name: reqwest::dns::Name = "localhost".parse().unwrap();
        let error = PublicResolver
            .resolve(name)
            .await
            .err()
            .expect("localhost has no public address");
        assert!(error.downcast_ref::<NotPublic>().is_some(), "{error}");
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn redirects_stay_public_and_never_downgrade() {
        let start = [url("https://indexer.example/dl/1")];
        assert_eq!(
            redirect_verdict(&url("https://cdn.example/a.torrent"), &start, false),
            Redirect::Follow
        );
        for local in [
            "https://192.168.1.1/a.torrent",
            "https://169.254.169.254/latest/meta-data/",
            "https://127.0.0.1:8080/a.torrent",
            "https://[::1]/a.torrent",
            "https://[::ffff:10.0.0.1]/a.torrent",
            "https://localhost/a.torrent",
            "https://router.localhost./a.torrent",
        ] {
            assert_eq!(
                redirect_verdict(&url(local), &start, false),
                Redirect::Refuse(Some(NotPublic)),
                "{local}"
            );
        }
        assert_eq!(
            redirect_verdict(&url("http://cdn.example/a.torrent"), &start, false),
            Redirect::Refuse(None),
            "https to http"
        );
        assert_eq!(
            redirect_verdict(&url("magnet:?xt=urn:btih:00"), &start, false),
            Redirect::Stop
        );
        let many = vec![url("https://indexer.example/"); 5];
        assert_eq!(
            redirect_verdict(&url("https://cdn.example/a.torrent"), &many, false),
            Redirect::Refuse(None)
        );
    }
}
