// SPDX-License-Identifier: MIT OR Apache-2.0

//! Thumbnails for search results: a picture from inside each torrent (see
//! `StreamEngine::thumbnail`), and a frame of the preloaded best pick.
//!
//! The webview asks for the results it shows whose metadata is known (prefetched, see
//! [`crate::prefetch`]), at most [`MAX_THUMBNAILS`], two at a time and each for at most 20 s. A
//! newer request, or a new search, stops the running one. Each picture is downloaded piece by
//! piece (checked against the torrent's hashes), checked to really be a JPEG, PNG, or WebP
//! image, and handed over as a `data:` URL, which the content security policy allows for
//! images (`img-src 'self' data:`); nothing else about it reaches the webview. No outside
//! service is asked for posters. The `search.preloadTopResult` setting turns this off too.
//!
//! [`preview_frame`] serves the preloaded best pick's video from the local stream server, so the
//! webview can show a frame from its start, which is already downloaded, without claiming the
//! preload. Like a file explorer preview (see [`crate::preview`]), the webview's read-ahead can
//! download a little more of the file than the preload did.

use std::sync::{Arc, Mutex, PoisonError};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tauri::State;
use tauri::ipc::Channel;
use torrentplayer_stream::StreamId;
use torrentplayer_torrent::Thumbnail;
use ts_rs::TS;

use crate::core::AppState;
use crate::error::IpcError;
use crate::latest::Latest;
use crate::prefetch::run_batch;
use crate::preload::PreloadPolicy as _;
use crate::settings::SettingsState;

/// Most thumbnails fetched per request; the rest of a longer list is ignored.
pub(crate) const MAX_THUMBNAILS: usize = 12;
/// Longest list accepted.
const MAX_REQUESTS: usize = 200;
/// Pictures downloaded at once.
const CONCURRENCY: usize = 2;

/// Managed state: the running thumbnail request, and the best pick's published preview.
#[derive(Debug, Default)]
pub(crate) struct ThumbnailState {
    latest: Latest,
    frame: Mutex<Option<StreamId>>,
}

impl ThumbnailState {
    /// Stops the running request, if any.
    pub(crate) fn cancel(&self) {
        self.latest.cancel();
    }
}

/// A search result to find a thumbnail for.
#[derive(Debug, Clone, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct ThumbnailRequest {
    /// Its key (`SearchResultDto.key`), sent back with the thumbnail.
    pub key: String,
    /// Its info hash (40 hex characters).
    pub info_hash: String,
}

/// The outcome for one result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct ThumbnailDto {
    /// Its key (`SearchResultDto.key`).
    pub key: String,
    /// The picture as a `data:image/…;base64,` URL, or `null` if the torrent has no suitable
    /// picture (or it could not be fetched).
    pub data_url: Option<String>,
}

/// The preloaded best pick's video on the local stream server.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct PreviewFrameDto {
    /// The torrent's info hash (matches `SearchResultDto.infoHash`).
    pub info_hash: String,
    /// The stream URL, with its session token (never log it).
    pub url: String,
}

/// Finds thumbnails for these results (top first), sending each one's outcome to `on_ready`.
/// Stops the previous request; returns when every result is done or a newer request (or
/// search) stops this one. Does nothing while the `search.preloadTopResult` setting is off.
#[tauri::command]
pub(crate) async fn fetch_thumbnails(
    state: State<'_, ThumbnailState>,
    app: State<'_, AppState>,
    settings: State<'_, SettingsState>,
    results: Vec<ThumbnailRequest>,
    on_ready: Channel<ThumbnailDto>,
) -> Result<(), IpcError> {
    let stopped = state.latest.begin();
    if results.len() > MAX_REQUESTS {
        return Err(IpcError::invalid("Too many results for thumbnails."));
    }
    let requests: Vec<ThumbnailRequest> = results
        .into_iter()
        .filter(|r| r.info_hash.len() == 40 && r.info_hash.bytes().all(|b| b.is_ascii_hexdigit()))
        .take(MAX_THUMBNAILS)
        .collect();
    if requests.is_empty() || !settings.preload_top_result() {
        return Ok(());
    }
    let core = Arc::clone(app.core().await?);
    let fetch = move |request: ThumbnailRequest| {
        let core = Arc::clone(&core);
        async move {
            let data_url = match core.engine.thumbnail(&request.info_hash).await {
                Ok(thumbnail) => thumbnail.as_ref().map(data_url),
                Err(error) => {
                    tracing::debug!(%error, "thumbnail failed");
                    None
                }
            };
            Ok(ThumbnailDto {
                key: request.key,
                data_url,
            })
        }
    };
    let ready = |thumbnail: ThumbnailDto| {
        // The window may have moved on; nothing to do then.
        let _ = on_ready.send(thumbnail);
    };
    tokio::select! {
        () = run_batch(requests, CONCURRENCY, fetch, ready) => {}
        () = stopped => {}
    }
    Ok(())
}

/// The preloaded best pick's video, served for a frame of its start (already downloaded), or
/// `null` unless a preload is ready. Replaces the previous one. Never starts the engine and
/// does not claim the preload.
#[tauri::command]
pub(crate) async fn preview_frame(
    state: State<'_, ThumbnailState>,
    app: State<'_, AppState>,
) -> Result<Option<PreviewFrameDto>, IpcError> {
    let Some(core) = app.started() else {
        return Ok(None);
    };
    let Some((info_hash, media)) = core.engine.preloaded_media().await else {
        return Ok(None);
    };
    let published = core.server.publish(media);
    let previous = state
        .frame
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .replace(published.id);
    if let Some(previous) = previous {
        core.server.unpublish(previous);
    }
    Ok(Some(PreviewFrameDto {
        info_hash,
        url: published.url,
    }))
}

/// A picture as a `data:` URL.
fn data_url(thumbnail: &Thumbnail) -> String {
    format!(
        "data:{};base64,{}",
        thumbnail.kind.mime(),
        base64::engine::general_purpose::STANDARD.encode(&thumbnail.bytes)
    )
}

#[cfg(test)]
mod tests {
    use torrentplayer_torrent::thumbnail::ImageType;

    use super::*;

    #[test]
    fn pictures_become_data_urls_of_their_checked_type() {
        let thumbnail = Thumbnail {
            info_hash: "ab".repeat(20),
            kind: ImageType::Png,
            bytes: bytes::Bytes::from_static(b"\x89PNG\r\n\x1a\n"),
        };
        assert_eq!(data_url(&thumbnail), "data:image/png;base64,iVBORw0KGgo=");
    }

    #[test]
    fn thumbnail_dto_serializes_as_camel_case() {
        let json = serde_json::to_value(ThumbnailDto {
            key: "k".into(),
            data_url: None,
        })
        .unwrap();
        assert_eq!(json["dataUrl"], serde_json::Value::Null);
    }
}
