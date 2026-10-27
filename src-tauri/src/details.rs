// SPDX-License-Identifier: MIT OR Apache-2.0

//! A search result's details: what its torrent's metadata says (see
//! `StreamEngine::torrent_details`), fetched on request for one result, and opening an
//! indexer's page in the system browser.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tauri::State;
use torrentplayer_torrent::TorrentSource;
use ts_rs::TS;
use url::Url;

use crate::commands::{FileKindDto, FileSummary};
use crate::core::AppState;
use crate::error::IpcError;
use crate::search::SearchState;

/// How long fetching one result's metadata may take.
const LOAD_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest link opened.
const MAX_LINK_BYTES: usize = 2048;

/// What a torrent's metadata says.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct TorrentDetailsDto {
    /// Lowercase hex info hash.
    pub info_hash: String,
    /// Every file, in torrent order (paths are plain text; never render as HTML).
    pub files: Vec<FileSummary>,
    /// Piece size in bytes.
    pub piece_length: u32,
    /// Total size in bytes.
    pub total_length: u64,
    /// Trackers announced over UDP.
    pub udp_trackers: u32,
    /// Trackers announced over HTTP(S).
    pub http_trackers: u32,
    /// Web seeds.
    pub web_seeds: u32,
    /// Private torrent (peers only from its trackers); `null` if unknown.
    pub private: Option<bool>,
    /// The creator's comment (plain text, at most 500 characters; never render as HTML).
    pub comment: Option<String>,
    /// The program that made the `.torrent` file (plain text).
    pub created_by: Option<String>,
    /// When it was made, in milliseconds since 1970.
    pub created_ms: Option<i64>,
}

fn dto(d: torrentplayer_torrent::details::TorrentDetails) -> TorrentDetailsDto {
    let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    TorrentDetailsDto {
        files: d
            .files
            .into_iter()
            .map(|f| FileSummary {
                index: f.index,
                path: f.path,
                length: f.len,
                kind: FileKindDto::from(f.kind),
            })
            .collect(),
        piece_length: d.piece_len,
        total_length: d.total_len,
        udp_trackers: count(d.udp_trackers),
        http_trackers: count(d.http_trackers),
        web_seeds: count(d.web_seeds),
        private: d.extras.private,
        comment: d.extras.comment,
        created_by: d.extras.created_by,
        created_ms: d.extras.created_at.and_then(|s| s.checked_mul(1000)),
        info_hash: d.info_hash,
    }
}

/// A result's details from its torrent's metadata, if known (`null` otherwise). With `load`,
/// the metadata is fetched first if need be (metadata only, as `prefetch_torrents` does: through
/// the search service's download link, else the magnet link), which starts the engine; without
/// it nothing is sent.
#[tauri::command]
pub(crate) async fn torrent_details(
    app: State<'_, AppState>,
    search: State<'_, SearchState>,
    info_hash: Option<String>,
    magnet: Option<String>,
    download: Option<String>,
    load: bool,
) -> Result<Option<TorrentDetailsDto>, IpcError> {
    let hash = info_hash.filter(|h| h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit()));
    if let (Some(core), Some(hash)) = (app.started(), &hash)
        && let Some(details) = core.engine.torrent_details(hash)
    {
        return Ok(Some(dto(details)));
    }
    if !load {
        return Ok(None);
    }
    let core = Arc::clone(app.core().await?);
    let magnet = magnet
        .as_deref()
        .and_then(|m| TorrentSource::parse_link(m).ok())
        .filter(|s| matches!(s, TorrentSource::Magnet(_)));
    let provider = search.started_provider();
    let download = download
        .filter(|d| provider.as_ref().is_some_and(|p| p.is_download_link(d)))
        .and_then(|d| match TorrentSource::parse_link(&d) {
            Ok(TorrentSource::Url(url)) => Some(url),
            _ => None,
        });
    let fetched = tokio::time::timeout(LOAD_TIMEOUT, async {
        match (download, magnet) {
            (Some(url), fallback) => {
                search.pace(url.as_str()).await;
                core.engine
                    .prefetch_download(url, hash.as_deref(), fallback)
                    .await
            }
            (None, Some(source)) => core.engine.prefetch_metadata(source).await,
            (None, None) => Err(torrentplayer_torrent::EngineError::Unsupported(
                "This result has no link to fetch its details from.",
            )),
        }
    })
    .await
    .map_err(|_| IpcError::from(torrentplayer_torrent::EngineError::MetadataTimeout))??;
    Ok(core.engine.torrent_details(&fetched).map(dto))
}

/// Whether `link` may be opened in the system browser: an http(s) URL with a host, no
/// credentials, and at most 2 KiB, whose host does not name this computer or the local network.
/// Returns it normalized.
///
/// The host check is lexical: IP addresses outside the public ranges, `localhost`, and `.local`
/// names are refused, but a public-looking name is not resolved, so one whose DNS points at a
/// local address passes. The browser, not TorrentPlayer, makes the request.
pub(crate) fn openable(link: &str) -> Option<String> {
    if link.len() > MAX_LINK_BYTES {
        return None;
    }
    let url = Url::parse(link.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let local = match url.host()? {
        url::Host::Domain(name) => {
            let name = name.to_ascii_lowercase();
            // The last label of the name: `localhost` or the `.local` of the local network.
            let last = name
                .trim_end_matches('.')
                .rsplit('.')
                .next()
                .unwrap_or_default();
            last == "localhost" || (last == "local" && name.contains('.'))
        }
        // The same test as for peer addresses: also IPv4-mapped IPv6, fe80::/10, 100.64.0.0/10
        // (carrier-grade NAT and VPN tunnels), and the other reserved ranges.
        url::Host::Ipv4(ip) => !torrentplayer_net::is_public_ip(ip.into()),
        url::Host::Ipv6(ip) => !torrentplayer_net::is_public_ip(ip.into()),
    };
    (!local).then(|| url.into())
}

/// Opens an indexer's page (an http or https link from a search result) in the system
/// browser. The link is checked first (see [`openable`]) and handed to the system as one
/// argument, never through a shell. It is not logged.
#[tauri::command]
#[allow(
    clippy::needless_pass_by_value,
    reason = "Tauri passes arguments by value"
)]
pub(crate) fn open_link(link: String) -> Result<(), IpcError> {
    let link = openable(&link).ok_or_else(|| IpcError::invalid("Only web links can be opened."))?;
    #[cfg(windows)]
    let mut command = {
        let mut c = std::process::Command::new("rundll32.exe");
        c.arg("url.dll,FileProtocolHandler");
        c
    };
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = std::process::Command::new("xdg-open");
    command
        .arg(link)
        .spawn()
        .map_err(|e| IpcError::internal(&e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_only_public_web_links() {
        assert_eq!(
            openable(" https://tracker.example/torrent/1 ").as_deref(),
            Some("https://tracker.example/torrent/1")
        );
        assert!(openable("http://example.org/a?b=c&d=e").is_some());
        assert!(openable("http://93.184.215.14/").is_some());
        assert!(openable("http://[2606:4700::1]/").is_some());
        for bad in [
            "javascript:alert(1)",
            "file:///C:/Windows/system32",
            "magnet:?xt=urn:btih:00",
            "ftp://example.org/",
            "https://user:pw@example.org/",
            "http://localhost:8080/",
            "http://127.0.0.1:9000/s/1?t=secret",
            "http://192.168.1.1/",
            "http://10.0.0.1/",
            "http://169.254.0.1/",
            "http://[::1]/",
            "http://[fd00::1]/",
            "http://[::ffff:127.0.0.1]:9000/s/1",
            "http://[::ffff:192.168.1.1]/",
            "http://[fe80::1]/",
            "http://100.64.0.1/",
            "http://100.127.255.254/",
            "http://0.0.0.0/",
            "http://[::]/",
            "http://224.0.0.1/",
            "http://printer.local/",
            "not a link",
        ] {
            assert_eq!(openable(bad), None, "{bad}");
        }
        assert_eq!(
            openable(&format!("https://example.org/{}", "a".repeat(3000))),
            None
        );
    }
}
