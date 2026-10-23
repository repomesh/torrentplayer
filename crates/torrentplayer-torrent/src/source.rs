// SPDX-License-Identifier: MIT OR Apache-2.0

//! Where a torrent comes from, and safe retrieval of `.torrent` files.

use std::time::Duration;

use bytes::{Bytes, BytesMut};
use url::Url;

/// Largest `.torrent` file accepted. Real course torrents are well under 1 MiB.
pub const MAX_TORRENT_FILE_BYTES: usize = 16 * 1024 * 1024;
/// Longest link accepted (magnet links with many trackers can be a few KiB).
pub const MAX_LINK_BYTES: usize = 16 * 1024;

/// A torrent the user wants to add.
#[derive(Clone, PartialEq, Eq)]
pub enum TorrentSource {
    /// A `magnet:` link.
    Magnet(String),
    /// An `http:` or `https:` URL of a `.torrent` file.
    Url(Url),
    /// The contents of a `.torrent` file.
    File(Bytes),
}

impl std::fmt::Debug for TorrentSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Magnet(_) => f.write_str("Magnet(..)"),
            Self::Url(url) => write!(f, "Url({})", url.host_str().unwrap_or("?")),
            Self::File(bytes) => write!(f, "File({} bytes)", bytes.len()),
        }
    }
}

/// Why a source was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SourceError {
    /// The link is empty or longer than [`MAX_LINK_BYTES`].
    #[error("the link is empty or too long")]
    BadLength,
    /// The link is not a magnet link or an http(s) URL.
    #[error("only magnet links and http(s) links to .torrent files are supported")]
    UnsupportedScheme,
    /// The link could not be parsed.
    #[error("the link is not valid")]
    Invalid,
    /// The `.torrent` file is empty or larger than [`MAX_TORRENT_FILE_BYTES`].
    #[error("the .torrent file is empty or larger than 16 MiB")]
    BadFileSize,
    /// Downloading the `.torrent` file failed.
    #[error("could not download the .torrent file: {0}")]
    Download(String),
    /// The link redirects to this magnet link instead of serving a `.torrent` file, as the
    /// download links of search sites often do. Add the magnet link instead.
    #[error("the link leads to a magnet link, not a .torrent file")]
    MagnetRedirect(String),
    /// The link (or a redirect) leads to the local network (a private, loopback, or
    /// link-local address, or `localhost`): links come from untrusted sites, and must not
    /// reach the user's router, other devices, or local services.
    #[error("the link leads to the local network, which is not allowed")]
    NotPublic,
}

impl TorrentSource {
    /// Parses a link pasted or dropped by the user.
    ///
    /// Only `magnet:`, `http:`, and `https:` are accepted; `file:`, `javascript:`, and every
    /// other scheme are refused.
    ///
    /// # Errors
    ///
    /// Returns an error if the link is too long, malformed, or uses another scheme.
    pub fn parse_link(link: &str) -> Result<Self, SourceError> {
        let link = link.trim();
        if link.is_empty() || link.len() > MAX_LINK_BYTES {
            return Err(SourceError::BadLength);
        }
        let url = Url::parse(link).map_err(|_| SourceError::Invalid)?;
        match url.scheme() {
            "magnet" => {
                let has_hash = url
                    .query_pairs()
                    .any(|(k, v)| k == "xt" && v.starts_with("urn:btih:"));
                if has_hash {
                    Ok(Self::Magnet(link.to_owned()))
                } else {
                    Err(SourceError::Invalid)
                }
            }
            "http" | "https" if url.host_str().is_some() => Ok(Self::Url(url)),
            "http" | "https" => Err(SourceError::Invalid),
            _ => Err(SourceError::UnsupportedScheme),
        }
    }

    /// Wraps the contents of a `.torrent` file.
    ///
    /// # Errors
    ///
    /// Returns an error if the file is empty or larger than [`MAX_TORRENT_FILE_BYTES`].
    pub fn from_file_bytes(bytes: impl Into<Bytes>) -> Result<Self, SourceError> {
        let bytes = bytes.into();
        if bytes.is_empty() || bytes.len() > MAX_TORRENT_FILE_BYTES {
            return Err(SourceError::BadFileSize);
        }
        Ok(Self::File(bytes))
    }
}

/// Trackers kept in a magnet link built by [`magnet_link`].
pub const MAGNET_TRACKERS: usize = 20;
/// Longest magnet link built by [`magnet_link`]; trackers that do not fit are left out.
const MAGNET_BUDGET: usize = 4 * 1024;

/// A magnet link that adds a torrent again: its info hash, name, and up to
/// [`MAGNET_TRACKERS`] of its http(s) and udp trackers, within about 4 KiB. Used to save
/// torrents opened from a `.torrent` file (in the play queue) without keeping the file.
#[must_use]
pub fn magnet_link(info_hash: &str, name: &str, trackers: &[Url]) -> String {
    let encode = |text: &str| url::form_urlencoded::byte_serialize(text.as_bytes()).collect();
    let name: String = encode(&name.chars().take(200).collect::<String>());
    let mut link = format!(
        "magnet:?xt=urn:btih:{}&dn={name}",
        info_hash.to_ascii_lowercase()
    );
    let mut kept = 0;
    for tracker in trackers {
        if kept == MAGNET_TRACKERS {
            break;
        }
        if !matches!(tracker.scheme(), "http" | "https" | "udp") {
            continue;
        }
        let tr: String = encode(tracker.as_str());
        if link.len() + 4 + tr.len() > MAGNET_BUDGET {
            continue;
        }
        link.push_str("&tr=");
        link.push_str(&tr);
        kept += 1;
    }
    link
}

/// Settings of the HTTP client used to download `.torrent` files. Redirects are followed at
/// most 5 times, only to http(s), never from https to http, and (unless `allow_local`) only to
/// public hosts (see [`link_allowed`]). Redirects to anything else (a magnet link) stop there,
/// for [`fetch_torrent_file`] to report.
///
/// Names are checked when they are resolved: route the client through the network's
/// public-only bridge (`Network::configure_http_public`), or without a network manager give it
/// [`PublicResolver`].
pub(crate) fn http_client_builder(user_agent: &str, allow_local: bool) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(user_agent)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::custom(
            move |attempt| match redirect_verdict(attempt.url(), attempt.previous(), allow_local) {
                Redirect::Follow => attempt.follow(),
                Redirect::Stop => attempt.stop(),
                Redirect::Refuse(Some(error)) => attempt.error(error),
                Redirect::Refuse(None) => attempt.error("redirect refused"),
            },
        ))
}

/// What to do with a redirect of a `.torrent` download.
#[derive(Debug, PartialEq, Eq)]
enum Redirect {
    Follow,
    /// Not http(s): hand the redirect itself to the caller (a magnet link, see
    /// [`fetch_torrent_file`]).
    Stop,
    /// Fail the download, with this error if it is one to report as such.
    Refuse(Option<SourceError>),
}

/// The redirect policy of [`http_client_builder`] for a redirect to `next` after `previous`
/// (the first request included).
fn redirect_verdict(next: &Url, previous: &[Url], allow_local: bool) -> Redirect {
    if previous.len() >= 5 {
        Redirect::Refuse(None)
    } else if !matches!(next.scheme(), "http" | "https") {
        Redirect::Stop
    } else if next.scheme() == "http" && previous.iter().any(|u| u.scheme() == "https") {
        // A downgrade would let anyone on the path change the `.torrent` file.
        Redirect::Refuse(None)
    } else if !allow_local && !link_allowed(next) {
        Redirect::Refuse(Some(SourceError::NotPublic))
    } else {
        Redirect::Follow
    }
}

/// Whether a `.torrent` link may be fetched: http(s), and a public host when it is an address
/// (see `torrentplayer_net::is_public_ip`) and not a `localhost` name. Other names are checked
/// when they are resolved (see [`http_client_builder`]).
pub(crate) fn link_allowed(url: &Url) -> bool {
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    match url.host() {
        Some(url::Host::Ipv4(ip)) => torrentplayer_net::is_public_ip(ip.into()),
        Some(url::Host::Ipv6(ip)) => torrentplayer_net::is_public_ip(ip.into()),
        Some(url::Host::Domain(name)) => {
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            name != "localhost" && !name.ends_with(".localhost")
        }
        None => false,
    }
}

/// Resolves names to their public addresses only, for `.torrent` downloads when no network
/// manager is configured (with one, its public-only bridge does the same).
#[derive(Debug, Clone, Copy)]
pub(crate) struct PublicResolver;

impl reqwest::dns::Resolve for PublicResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|a| torrentplayer_net::is_public_ip(a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(SourceError::NotPublic.into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Downloads a `.torrent` file, refusing anything larger than [`MAX_TORRENT_FILE_BYTES`].
///
/// librqbit can fetch URLs itself but reads the whole response without a size limit, so TorrentPlayer
/// downloads `.torrent` files here instead.
///
/// # Errors
///
/// [`SourceError::MagnetRedirect`] if the link redirects to a magnet link (redirects are
/// followed only to http(s)); otherwise why the download failed or was refused.
pub(crate) async fn fetch_torrent_file(
    client: &reqwest::Client,
    url: &Url,
    allow_local: bool,
) -> Result<Bytes, SourceError> {
    if !allow_local && !link_allowed(url) {
        return Err(SourceError::NotPublic);
    }
    let download = |e: reqwest::Error| {
        // Refused by the redirect policy or `PublicResolver`: say why.
        let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&e);
        while let Some(error) = source {
            if let Some(SourceError::NotPublic) = error.downcast_ref::<SourceError>() {
                return SourceError::NotPublic;
            }
            source = error.source();
        }
        SourceError::Download(e.without_url().to_string())
    };
    let response = client.get(url.clone()).send().await.map_err(download)?;
    if response.status().is_redirection()
        && let Some(location) = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
        && let Ok(TorrentSource::Magnet(magnet)) = TorrentSource::parse_link(location)
    {
        return Err(SourceError::MagnetRedirect(magnet));
    }
    let mut response = response.error_for_status().map_err(download)?;
    if response
        .content_length()
        .is_some_and(|len| len > MAX_TORRENT_FILE_BYTES as u64)
    {
        return Err(SourceError::BadFileSize);
    }
    let mut body = BytesMut::new();
    while let Some(chunk) = response.chunk().await.map_err(download)? {
        if body.len() + chunk.len() > MAX_TORRENT_FILE_BYTES {
            return Err(SourceError::BadFileSize);
        }
        body.extend_from_slice(&chunk);
    }
    if body.is_empty() {
        return Err(SourceError::BadFileSize);
    }
    Ok(body.freeze())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c";

    #[test]
    fn accepts_magnets_and_http_links() {
        let magnet = format!("magnet:?xt=urn:btih:{HASH}&dn=Big+Buck+Bunny");
        assert!(matches!(
            TorrentSource::parse_link(&magnet),
            Ok(TorrentSource::Magnet(_))
        ));
        assert!(matches!(
            TorrentSource::parse_link("  https://academictorrents.com/download/abc.torrent "),
            Ok(TorrentSource::Url(_))
        ));
        assert!(matches!(
            TorrentSource::parse_link("http://example.org/a.torrent"),
            Ok(TorrentSource::Url(_))
        ));
    }

    #[test]
    fn refuses_other_schemes_and_bad_links() {
        for (link, err) in [
            ("file:///etc/passwd", SourceError::UnsupportedScheme),
            ("javascript:alert(1)", SourceError::UnsupportedScheme),
            (
                "ftp://example.org/a.torrent",
                SourceError::UnsupportedScheme,
            ),
            ("C:\\Users\\me\\a.torrent", SourceError::UnsupportedScheme),
            ("magnet:?dn=no-hash", SourceError::Invalid),
            ("not a link", SourceError::Invalid),
            ("http://", SourceError::Invalid),
            ("", SourceError::BadLength),
        ] {
            assert_eq!(TorrentSource::parse_link(link), Err(err), "{link}");
        }
        let long = format!(
            "magnet:?xt=urn:btih:{HASH}&tr={}",
            "a".repeat(MAX_LINK_BYTES)
        );
        assert_eq!(
            TorrentSource::parse_link(&long),
            Err(SourceError::BadLength)
        );
    }

    #[test]
    fn file_size_limits() {
        assert_eq!(
            TorrentSource::from_file_bytes(Vec::new()),
            Err(SourceError::BadFileSize)
        );
        assert_eq!(
            TorrentSource::from_file_bytes(vec![0u8; MAX_TORRENT_FILE_BYTES + 1]),
            Err(SourceError::BadFileSize)
        );
        assert!(TorrentSource::from_file_bytes(vec![b'd', b'e']).is_ok());
    }

    /// Serves one HTTP response on a local port; returns its URL.
    async fn serve_once(response: &'static str) -> Url {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let _ = socket.read(&mut request).await;
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        Url::parse(&format!("http://{addr}/download/abc")).unwrap()
    }

    #[tokio::test]
    async fn download_links_that_redirect_to_magnets_say_so() {
        // As search.emberglow.tech answers for indexers that only have magnet links.
        let url = serve_once(
            "HTTP/1.1 302 Found\r\nLocation: magnet:?xt=urn:btih:7EDD96C56641A3B80DB0C3E607171A99E5ADD1E3&dn=X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
        // Local links allowed: the server is on the loopback interface.
        let client = http_client_builder("test", true).build().unwrap();
        assert_eq!(
            fetch_torrent_file(&client, &url, true).await,
            Err(SourceError::MagnetRedirect(
                "magnet:?xt=urn:btih:7EDD96C56641A3B80DB0C3E607171A99E5ADD1E3&dn=X".into()
            ))
        );

        let url = serve_once(
            "HTTP/1.1 302 Found\r\nLocation: ftp://example.org/a.torrent\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(
            fetch_torrent_file(&client, &url, true).await,
            Err(SourceError::BadFileSize),
            "other schemes are not followed"
        );

        let url = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/x-bittorrent\r\nContent-Length: 2\r\nConnection: close\r\n\r\nde",
        )
        .await;
        assert_eq!(
            fetch_torrent_file(&client, &url, true).await,
            Ok(Bytes::from_static(b"de"))
        );
    }

    #[test]
    fn builds_bounded_magnet_links_with_trackers() {
        let trackers: Vec<Url> = [
            "udp://tracker.opentrackr.org:1337/announce",
            "wss://tracker.example/announce",
            "https://t.example/announce?passkey=a&b",
        ]
        .iter()
        .map(|t| Url::parse(t).unwrap())
        .collect();
        let link = magnet_link(&HASH.to_uppercase(), "Lecture 1 & 2", &trackers);
        assert_eq!(
            link,
            format!(
                "magnet:?xt=urn:btih:{HASH}&dn=Lecture+1+%26+2\
                 &tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337%2Fannounce\
                 &tr=https%3A%2F%2Ft.example%2Fannounce%3Fpasskey%3Da%26b"
            ),
            "websocket trackers are left out"
        );
        assert!(matches!(
            TorrentSource::parse_link(&link),
            Ok(TorrentSource::Magnet(_))
        ));

        let many: Vec<Url> = (0..100)
            .map(|i| Url::parse(&format!("udp://t{i}.example:{}/announce", 1000 + i)).unwrap())
            .collect();
        let link = magnet_link(HASH, &"n".repeat(5000), &many);
        assert_eq!(link.matches("&tr=").count(), MAGNET_TRACKERS);
        assert!(link.len() <= MAGNET_BUDGET);
        let long = vec![Url::parse(&format!("https://e.org/{}", "a".repeat(5000))).unwrap()];
        assert!(
            !magnet_link(HASH, "x", &long).contains("&tr="),
            "too long to keep"
        );
    }

    #[test]
    fn debug_output_hides_link_contents() {
        let magnet = format!("magnet:?xt=urn:btih:{HASH}");
        let s = TorrentSource::parse_link(&magnet).unwrap();
        assert!(!format!("{s:?}").contains(HASH));
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
        assert_eq!(
            redirect_verdict(&url("http://93.184.215.14/a.torrent"), &[], false),
            Redirect::Follow
        );
        for local in [
            "https://192.168.1.1/a.torrent",
            "https://10.0.0.8/a.torrent",
            "https://169.254.169.254/latest/meta-data/",
            "https://127.0.0.1:8080/a.torrent",
            "https://[::1]/a.torrent",
            "https://[fe80::1]/a.torrent",
            "https://[::ffff:192.168.0.1]/a.torrent",
            "https://localhost/a.torrent",
            "https://router.localhost./a.torrent",
        ] {
            assert_eq!(
                redirect_verdict(&url(local), &start, false),
                Redirect::Refuse(Some(SourceError::NotPublic)),
                "{local}"
            );
            assert!(!link_allowed(&url(local)), "{local}");
            assert_eq!(
                redirect_verdict(&url(local), &start, true),
                Redirect::Follow,
                "{local} with local links allowed"
            );
        }
        // https to http, even to a public host, and even with local links allowed.
        assert_eq!(
            redirect_verdict(&url("http://cdn.example/a.torrent"), &start, true),
            Redirect::Refuse(None)
        );
        let magnet = url(&format!("magnet:?xt=urn:btih:{HASH}"));
        assert_eq!(redirect_verdict(&magnet, &start, false), Redirect::Stop);
        let many = vec![url("https://a.example/"); 5];
        assert_eq!(
            redirect_verdict(&url("https://b.example/"), &many, false),
            Redirect::Refuse(None)
        );
        assert!(link_allowed(&url("https://indexer.example/dl/1")));
    }

    /// A server on the loopback interface that redirects each path to a location, and records
    /// the paths asked for.
    async fn redirecting_server(
        routes: Vec<(&'static str, String)>,
    ) -> (
        std::net::SocketAddr,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = std::sync::Arc::clone(&seen);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                let path = request.split(' ').nth(1).unwrap_or("").to_owned();
                record.lock().unwrap().push(path.clone());
                let reply = match routes.iter().find(|(p, _)| *p == path) {
                    Some((_, location)) => format!(
                        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\
                         Connection: close\r\n\r\n"
                    ),
                    None => "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nde"
                        .to_owned(),
                };
                let _ = socket.write_all(reply.as_bytes()).await;
            }
        });
        (addr, seen)
    }

    #[tokio::test]
    async fn downloads_refuse_local_links_and_redirects() {
        let (addr, seen) = redirecting_server(Vec::new()).await;
        let local = url(&format!("http://{addr}/ok"));
        let client = http_client_builder("test", false)
            .dns_resolver(PublicResolver)
            .build()
            .unwrap();
        // A local link is refused before anything is sent.
        assert_eq!(
            fetch_torrent_file(&client, &local, false).await,
            Err(SourceError::NotPublic)
        );
        assert!(seen.lock().unwrap().is_empty());
        assert!(fetch_torrent_file(&client, &local, true).await.is_ok());

        // Redirects from a link to the local network are refused before they are followed.
        let (addr, seen) = redirecting_server(vec![
            (
                "/metadata",
                "http://169.254.169.254/latest/meta-data/".to_owned(),
            ),
            ("/router", "http://192.168.1.1/a.torrent".to_owned()),
            ("/loopback", format!("http://{addr}/ok")),
            ("/name", "http://localhost/a.torrent".to_owned()),
        ])
        .await;
        for path in ["/metadata", "/router", "/loopback", "/name"] {
            let link = url(&format!("http://{addr}{path}"));
            // The first request is allowed here, to test the redirect policy alone.
            assert_eq!(
                fetch_torrent_file(&client, &link, true).await,
                Err(SourceError::NotPublic),
                "{path}"
            );
        }
        assert!(!seen.lock().unwrap().iter().any(|p| p == "/ok"));
    }

    #[tokio::test]
    async fn the_public_resolver_refuses_local_names() {
        use reqwest::dns::Resolve as _;
        let name: reqwest::dns::Name = "localhost".parse().unwrap();
        let error = PublicResolver
            .resolve(name)
            .await
            .err()
            .expect("localhost has no public address");
        assert!(error.downcast_ref::<SourceError>().is_some(), "{error}");
    }
}
