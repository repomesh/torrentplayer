// SPDX-License-Identifier: MIT OR Apache-2.0

//! Previews in the file explorer: one file of an added torrent, shown safely.
//!
//! - Pictures (JPEG, PNG, WebP by their bytes, within the thumbnail size limits) as `data:` URLs.
//! - Text and subtitles (`.txt`, `.nfo`, `.md`, `.srt`, …) as plain text, at most 64 KiB.
//! - Videos as a stream URL for a still frame, only when the file's start and end are already
//!   downloaded, so showing it downloads nothing new.
//! - Everything else (PDFs, archives, HTML) as metadata only: nothing is rendered, executed, or
//!   opened.
//!
//! Pictures and text are downloaded at background priority (`read_small_file`), so the video
//! playing keeps priority. One preview at a time: a newer one stops the previous wait.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use serde::Serialize;
use tauri::State;
use torrentplayer_stream::StreamId;
use torrentplayer_torrent::thumbnail::{MAX_IMAGE_BYTES, MIN_IMAGE_BYTES, image_type};
use torrentplayer_torrent::{FileKind, TorrentId};
use ts_rs::TS;

use crate::core::{AppState, Core};
use crate::error::{ErrorKind, IpcError};
use crate::latest::Latest;

/// Text shown, at most.
pub(crate) const MAX_TEXT_BYTES: usize = 64 * 1024;
/// Text files read, at most (longer ones are not previewed).
const MAX_TEXT_FILE_BYTES: u64 = 512 * 1024;
/// How long a preview waits for its file to download.
const WAIT: Duration = Duration::from_secs(20);

const TEXT_EXTENSIONS: &[&str] = &["txt", "nfo", "md", "srt", "vtt", "ass", "ssa", "sub", "log"];
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp"];

/// What a preview shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) enum PreviewKind {
    /// A picture (`image`).
    Image,
    /// Plain text (`text`).
    Text,
    /// A video frame (`video`), or only metadata if its start is not downloaded yet.
    Video,
    /// Metadata only.
    None,
}

/// A file's preview.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct FilePreviewDto {
    /// What was shown, if anything.
    pub kind: Option<PreviewKind>,
    /// A picture as a `data:` URL.
    pub image: Option<String>,
    /// Plain text (render as text, never as HTML).
    pub text: Option<String>,
    /// Whether `text` was cut at 64 KiB.
    pub truncated: bool,
    /// A stream URL for a still frame (it carries a session token: never log it).
    pub video: Option<String>,
    /// Why there is no preview, in words to show.
    pub note: Option<String>,
}

/// Managed state: the running preview and the video published for the last one.
#[derive(Debug, Default)]
pub(crate) struct PreviewState {
    latest: Latest,
    published: Mutex<Option<Published>>,
}

/// A video preview's stream, and the torrent it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Published {
    id: StreamId,
    /// Lowercase info hash.
    info_hash: String,
}

impl PreviewState {
    /// The info hash of the torrent whose video preview is showing, so cache clean-up keeps
    /// it (see `settings::protected_hashes`).
    pub(crate) fn previewed_hash(&self) -> Option<String> {
        self.lock().as_ref().map(|p| p.info_hash.clone())
    }

    /// Records a newly published preview video, returning the stream it replaces.
    fn publish(&self, id: StreamId, info_hash: &str) -> Option<StreamId> {
        let published = Published {
            id,
            info_hash: info_hash.to_ascii_lowercase(),
        };
        self.lock().replace(published).map(|p| p.id)
    }

    /// Forgets the preview video, returning its stream to withdraw.
    fn take(&self) -> Option<StreamId> {
        self.lock().take().map(|p| p.id)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Published>> {
        self.published
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// Ends the explorer's preview when it closes: stops a preview still waiting, and withdraws the
/// preview video's stream, so its torrent is no longer kept from cache clean-up.
#[tauri::command]
#[allow(
    clippy::needless_pass_by_value,
    reason = "Tauri passes managed state by value"
)]
pub(crate) fn stop_preview(state: State<'_, PreviewState>, app: State<'_, AppState>) {
    state.latest.cancel();
    if let Some(id) = state.take()
        && let Some(core) = app.started()
    {
        core.server.unpublish(id);
    }
}

/// What a file can be previewed as, by its kind, name, and size.
pub(crate) fn preview_kind(kind: FileKind, path: &str, len: u64) -> PreviewKind {
    let ext = path
        .rsplit_once('.')
        .map_or("", |(_, e)| e)
        .to_ascii_lowercase();
    if kind == FileKind::Video {
        PreviewKind::Video
    } else if IMAGE_EXTENSIONS.contains(&ext.as_str())
        && (MIN_IMAGE_BYTES..=MAX_IMAGE_BYTES).contains(&len)
    {
        PreviewKind::Image
    } else if TEXT_EXTENSIONS.contains(&ext.as_str()) && len <= MAX_TEXT_FILE_BYTES {
        PreviewKind::Text
    } else {
        PreviewKind::None
    }
}

/// Bytes as text to show: UTF-8 with bad sequences replaced, control characters other than
/// tabs and line breaks removed, and at most [`MAX_TEXT_BYTES`]. Returns the text and whether
/// it was cut.
pub(crate) fn as_text(bytes: &[u8]) -> (String, bool) {
    let truncated = bytes.len() > MAX_TEXT_BYTES;
    let mut text: String = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_TEXT_BYTES)])
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect();
    // Cutting may have split a character; the lossy decode replaced it at the very end.
    if truncated {
        text = text.trim_end_matches('\u{FFFD}').to_owned();
    }
    (text, truncated)
}

/// A preview of one file of an added torrent. Only one runs at a time: a newer call stops the
/// previous one's wait. Never starts the engine.
#[tauri::command]
pub(crate) async fn preview_file(
    state: State<'_, PreviewState>,
    app: State<'_, AppState>,
    torrent_id: usize,
    file_index: usize,
) -> Result<FilePreviewDto, IpcError> {
    let stopped = state.latest.begin();
    let core = Arc::clone(
        app.started()
            .ok_or_else(|| IpcError::new(ErrorKind::NotFound, "Unknown torrent."))?,
    );
    let info = core.engine.info(torrent_id)?;
    let entry = info
        .files
        .iter()
        .find(|f| f.index == file_index)
        .ok_or_else(|| IpcError::new(ErrorKind::NotFound, "Unknown file."))?
        .clone();
    let kind = preview_kind(entry.kind, &entry.path, entry.len);
    // A video previewed before is no longer shown.
    if kind != PreviewKind::Video
        && let Some(previous) = state.take()
    {
        core.server.unpublish(previous);
    }
    if kind == PreviewKind::Video {
        return Ok(video_preview(&state, &core, torrent_id, file_index).await);
    }
    if kind == PreviewKind::None {
        return Ok(FilePreviewDto::default());
    }
    let max = if kind == PreviewKind::Image {
        MAX_IMAGE_BYTES
    } else {
        MAX_TEXT_FILE_BYTES
    };
    let read = core.engine.read_small_file(torrent_id, file_index, max);
    let bytes = tokio::select! {
        result = tokio::time::timeout(WAIT, read) => match result {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(error)) => {
                tracing::debug!(%error, "no preview");
                return Ok(note("This file could not be read."));
            }
            Err(_) => return Ok(note("Still downloading; select it again in a moment.")),
        },
        () = stopped => return Ok(FilePreviewDto::default()),
    };
    if kind == PreviewKind::Image {
        return Ok(match image_type(&bytes) {
            Some(image) => FilePreviewDto {
                kind: Some(PreviewKind::Image),
                image: Some(format!(
                    "data:{};base64,{}",
                    image.mime(),
                    base64::engine::general_purpose::STANDARD.encode(&bytes)
                )),
                ..FilePreviewDto::default()
            },
            None => note("This is not a JPEG, PNG, or WebP picture."),
        });
    }
    let (text, truncated) = as_text(&bytes);
    Ok(FilePreviewDto {
        kind: Some(PreviewKind::Text),
        text: Some(text),
        truncated,
        ..FilePreviewDto::default()
    })
}

/// A still frame of a video whose start and end are downloaded; metadata only otherwise.
async fn video_preview(
    state: &PreviewState,
    core: &Core,
    torrent_id: TorrentId,
    file_index: usize,
) -> FilePreviewDto {
    let ready = match core.engine.have_pieces(torrent_id).await {
        Ok(have) => core
            .engine
            .file_readiness(torrent_id, Some(file_index), &have)
            .is_ok_and(|r| r.file == file_index && r.startable),
        Err(_) => false,
    };
    if !ready {
        return note("Its start is not downloaded yet; play it or queue it to see it.");
    }
    let Ok(media) = core.engine.media_source(torrent_id, file_index).await else {
        return FilePreviewDto::default();
    };
    let Ok(info) = core.engine.info(torrent_id) else {
        return FilePreviewDto::default();
    };
    let published = core.server.publish(media);
    if let Some(previous) = state.publish(published.id, &info.info_hash) {
        core.server.unpublish(previous);
    }
    FilePreviewDto {
        kind: Some(PreviewKind::Video),
        video: Some(published.url),
        ..FilePreviewDto::default()
    }
}

fn note(text: &str) -> FilePreviewDto {
    FilePreviewDto {
        note: Some(text.to_owned()),
        ..FilePreviewDto::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_previewed_torrent_is_known_until_the_preview_ends() {
        let state = PreviewState::default();
        assert_eq!(state.previewed_hash(), None);
        assert_eq!(state.publish(7, "ABCDEF"), None);
        assert_eq!(state.previewed_hash().as_deref(), Some("abcdef"));
        assert_eq!(
            state.publish(8, "123456"),
            Some(7),
            "the older stream is withdrawn"
        );
        assert_eq!(state.previewed_hash().as_deref(), Some("123456"));
        assert_eq!(state.take(), Some(8));
        assert_eq!(state.previewed_hash(), None, "no longer kept from clean-up");
        assert_eq!(state.take(), None);
    }

    #[test]
    fn previews_only_safe_kinds_within_limits() {
        let k = |kind, path: &str, len| preview_kind(kind, path, len);
        assert_eq!(
            k(FileKind::Video, "a/Lecture 1.mkv", 1 << 30),
            PreviewKind::Video
        );
        assert_eq!(k(FileKind::Other, "cover.JPG", 50_000), PreviewKind::Image);
        assert_eq!(k(FileKind::Other, "cover.webp", 50_000), PreviewKind::Image);
        assert_eq!(
            k(FileKind::Other, "huge.png", 5 << 20),
            PreviewKind::None,
            "too large"
        );
        assert_eq!(
            k(FileKind::Other, "tiny.png", 100),
            PreviewKind::None,
            "too small"
        );
        assert_eq!(k(FileKind::Subtitle, "en.srt", 80_000), PreviewKind::Text);
        assert_eq!(k(FileKind::Document, "README.md", 900), PreviewKind::Text);
        assert_eq!(k(FileKind::Other, "release.nfo", 4000), PreviewKind::Text);
        assert_eq!(k(FileKind::Other, "big.txt", 2 << 20), PreviewKind::None);
        for path in [
            "slides.pdf",
            "index.html",
            "setup.exe",
            "notes.docx",
            "noext",
        ] {
            assert_eq!(
                k(FileKind::Document, path, 1000),
                PreviewKind::None,
                "{path}"
            );
        }
    }

    #[test]
    fn text_is_plain_cleaned_and_capped() {
        let (text, truncated) = as_text(b"1\r\n00:00:01 --> 00:00:02\r\nHi\x07\t<b>there</b>\n");
        assert_eq!(text, "1\n00:00:01 --> 00:00:02\nHi\t<b>there</b>\n");
        assert!(!truncated);
        let (text, _) = as_text(b"caf\xe9");
        assert_eq!(text, "caf\u{FFFD}", "bad UTF-8 is replaced");
        let long = "é".repeat(MAX_TEXT_BYTES);
        let (text, truncated) = as_text(long.as_bytes());
        assert!(truncated);
        assert!(text.len() <= MAX_TEXT_BYTES);
        assert!(
            text.chars().all(|c| c == 'é'),
            "no half character at the cut"
        );
    }

    #[test]
    fn previews_serialize_as_camel_case() {
        let json = serde_json::to_value(FilePreviewDto {
            kind: Some(PreviewKind::Text),
            text: Some("hi".into()),
            ..FilePreviewDto::default()
        })
        .unwrap();
        assert_eq!(json["kind"], "text");
        assert_eq!(json["truncated"], false);
        assert_eq!(json["image"], serde_json::Value::Null);
    }
}
