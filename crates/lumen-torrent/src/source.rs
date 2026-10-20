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

/// Settings of the HTTP client used to download `.torrent` files.
pub(crate) fn http_client_builder(user_agent: &str) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(user_agent)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        // Only follow redirects to http(s), and not too many of them.
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                attempt.error("too many redirects")
            } else if matches!(attempt.url().scheme(), "http" | "https") {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
}

/// Downloads a `.torrent` file, refusing anything larger than [`MAX_TORRENT_FILE_BYTES`].
///
/// librqbit can fetch URLs itself but reads the whole response without a size limit, so TorrentStream
/// downloads `.torrent` files here instead.
///
/// # Errors
///
/// [`SourceError::MagnetRedirect`] if the link redirects to a magnet link (redirects are
/// followed only to http(s)); otherwise why the download failed or was refused.
pub(crate) async fn fetch_torrent_file(
    client: &reqwest::Client,
    url: &Url,
) -> Result<Bytes, SourceError> {
    let download = |e: reqwest::Error| SourceError::Download(e.without_url().to_string());
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
        let client = http_client_builder("test").build().unwrap();
        assert_eq!(
            fetch_torrent_file(&client, &url).await,
            Err(SourceError::MagnetRedirect(
                "magnet:?xt=urn:btih:7EDD96C56641A3B80DB0C3E607171A99E5ADD1E3&dn=X".into()
            ))
        );

        let url = serve_once(
            "HTTP/1.1 302 Found\r\nLocation: ftp://example.org/a.torrent\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(
            fetch_torrent_file(&client, &url).await,
            Err(SourceError::BadFileSize),
            "other schemes are not followed"
        );

        let url = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/x-bittorrent\r\nContent-Length: 2\r\nConnection: close\r\n\r\nde",
        )
        .await;
        assert_eq!(
            fetch_torrent_file(&client, &url).await,
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
}
