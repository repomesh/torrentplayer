// SPDX-License-Identifier: MIT OR Apache-2.0

//! Subtitles for a stream: the subtitle files in its torrent (matched to the playing video),
//! its embedded tracks (from the container probe), and the cues of one file, downloaded and
//! parsed here so the webview only ever receives plain text. See `lumen_media` for the parsers.

use std::time::Duration;

use lumen_media::{MAX_SUBTITLE_BYTES, SubtitleFile, SubtitleTrack, TrackReport, subtitle_files};
use lumen_stream::StreamId;
use lumen_torrent::FileKind;
use serde::Serialize;
use tauri::State;
use tauri::ipc::{InvokeBody, Request};
use ts_rs::TS;

use crate::core::AppState;
use crate::error::{ErrorKind, IpcError};

/// How long loading a subtitle file from the torrent may take.
const LOAD_TIMEOUT: Duration = Duration::from_secs(60);

/// A subtitle file in the stream's torrent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct SubtitleFileDto {
    /// The file's index in the torrent, for `load_subtitles`.
    pub file_index: usize,
    /// For menus: the language and flags (`English (SDH)`), or the file name.
    pub label: String,
    /// BCP 47 language tag from the file name, if it names one.
    pub language: Option<String>,
    /// The file name (plain text; never render as HTML).
    pub file_name: String,
    /// Whether it belongs to the playing video; the others are "other subtitles".
    pub matched: bool,
    /// Marked `forced` (only foreign-language parts).
    pub forced: bool,
    /// Marked for the deaf and hard of hearing.
    pub sdh: bool,
    /// Size in bytes. Files over 5 MiB cannot be loaded.
    pub length: u64,
}

/// A subtitle track embedded in the video file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct EmbeddedSubtitleDto {
    /// For menus: the track's name, or its language, or `Track N`.
    pub label: String,
    /// BCP 47 language tag, if known.
    pub language: Option<String>,
    /// Format: `text`, `webvtt`, `ass`, `ttml`, `cc`, `image`, or `other`.
    pub codec: String,
    /// Whether it holds text (pictures of text, such as PGS and `VobSub`, are `false`).
    pub text: bool,
    /// Marked default.
    pub default: bool,
    /// Marked forced.
    pub forced: bool,
}

/// The subtitles available for a stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct StreamSubtitles {
    /// Subtitle files in the torrent: those matching the video first.
    pub files: Vec<SubtitleFileDto>,
    /// Embedded tracks, or `null` while the container header has not been read yet.
    pub embedded: Option<Vec<EmbeddedSubtitleDto>>,
}

/// One cue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct CueDto {
    /// Start, in milliseconds.
    pub start_ms: u64,
    /// End, in milliseconds.
    pub end_ms: u64,
    /// Plain text with `\n` line breaks (never render as HTML).
    pub text: String,
}

/// A parsed subtitle file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct SubtitleCues {
    /// `srt`, `webvtt`, or `ass`.
    pub format: String,
    /// Cues in time order (at most 20 000).
    pub cues: Vec<CueDto>,
    /// The file had more cues than that; the rest were dropped.
    pub truncated: bool,
}

impl From<&SubtitleFile> for SubtitleFileDto {
    fn from(file: &SubtitleFile) -> Self {
        Self {
            file_index: file.index,
            label: file.label(),
            language: file.language.map(|l| l.code.to_owned()),
            file_name: file.file_name().to_owned(),
            matched: file.matched,
            forced: file.forced,
            sdh: file.sdh,
            length: 0,
        }
    }
}

/// The embedded subtitle tracks of a probed file.
pub(crate) fn embedded(report: &TrackReport) -> Vec<EmbeddedSubtitleDto> {
    report
        .subtitles
        .iter()
        .enumerate()
        .map(|(i, track)| embedded_track(i, track))
        .collect()
}

fn embedded_track(i: usize, track: &SubtitleTrack) -> EmbeddedSubtitleDto {
    let known = track.language.as_deref().and_then(lumen_media::language);
    let label = track
        .name
        .clone()
        .or_else(|| known.map(|l| l.name.to_owned()))
        .or_else(|| track.language.clone())
        .unwrap_or_else(|| format!("Track {}", i + 1));
    EmbeddedSubtitleDto {
        label,
        language: known
            .map(|l| l.code.to_owned())
            .or_else(|| track.language.clone()),
        codec: track.codec.id().to_owned(),
        text: track.codec.is_text(),
        default: track.default,
        forced: track.forced,
    }
}

fn cues_dto(bytes: &[u8]) -> Result<SubtitleCues, IpcError> {
    // The parser's messages are plain sentences without a capital or full stop.
    let parsed = lumen_media::parse_subtitles(bytes)
        .map_err(|error| IpcError::invalid(format!("{}.", capitalize(&error.to_string()))))?;
    Ok(SubtitleCues {
        format: parsed.format.id().to_owned(),
        cues: parsed
            .cues
            .into_iter()
            .map(|c| CueDto {
                start_ms: c.start_ms,
                end_ms: c.end_ms,
                text: c.text,
            })
            .collect(),
        truncated: parsed.truncated,
    })
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// Parses on a blocking thread: up to 5 MiB of text takes milliseconds, but not none.
async fn parse_off_thread(bytes: Vec<u8>) -> Result<SubtitleCues, IpcError> {
    tauri::async_runtime::spawn_blocking(move || cues_dto(&bytes))
        .await
        .map_err(|e| IpcError::internal(&e))?
}

/// Lists the subtitles for a stream: subtitle files in its torrent and embedded tracks.
#[tauri::command]
pub(crate) async fn stream_subtitles(
    state: State<'_, AppState>,
    stream_id: StreamId,
) -> Result<StreamSubtitles, IpcError> {
    let core = state.core().await?;
    let (torrent, file, embedded) = core.with_stream(stream_id, |s| {
        (s.torrent, s.file, s.tracks.embedded_subtitles())
    })?;
    let info = core.engine.info(torrent)?;
    let video = info
        .files
        .iter()
        .find(|f| f.index == file)
        .ok_or_else(|| IpcError::new(ErrorKind::NotFound, "Unknown file."))?;
    let videos = info
        .files
        .iter()
        .filter(|f| f.kind == FileKind::Video)
        .count();
    let files = subtitle_files(
        &video.path,
        videos,
        info.files.iter().map(|f| (f.index, f.path.as_str())),
    )
    .iter()
    .map(|found| SubtitleFileDto {
        length: info
            .files
            .iter()
            .find(|f| f.index == found.index)
            .map_or(0, |f| f.len),
        ..SubtitleFileDto::from(found)
    })
    .collect();
    Ok(StreamSubtitles { files, embedded })
}

/// Downloads one subtitle file of the stream's torrent (in the background, so the video keeps
/// priority) and returns its cues.
#[tauri::command]
pub(crate) async fn load_subtitles(
    state: State<'_, AppState>,
    stream_id: StreamId,
    file_index: usize,
) -> Result<SubtitleCues, IpcError> {
    let core = state.core().await?;
    let torrent = core.with_stream(stream_id, |s| s.torrent)?;
    let info = core.engine.info(torrent)?;
    let entry = info
        .files
        .iter()
        .find(|f| f.index == file_index && is_subtitle_file(&f.path))
        .ok_or_else(|| IpcError::invalid("That is not a subtitle file of this torrent."))?;
    let max = MAX_SUBTITLE_BYTES as u64;
    if entry.len > max {
        return Err(IpcError::invalid(
            "This subtitle file is larger than 5 MiB.",
        ));
    }
    let bytes = tokio::time::timeout(
        LOAD_TIMEOUT,
        core.engine.read_small_file(torrent, file_index, max),
    )
    .await
    .map_err(|_| {
        IpcError::new(
            ErrorKind::Engine,
            "The subtitle file did not download in time. Try again in a moment.",
        )
    })??;
    tracing::info!(
        stream = stream_id,
        file = file_index,
        bytes = bytes.len(),
        "subtitles loaded"
    );
    parse_off_thread(bytes).await
}

/// Parses a subtitle file the user chose (sent as the raw request body).
#[tauri::command]
pub(crate) async fn parse_subtitles(request: Request<'_>) -> Result<SubtitleCues, IpcError> {
    let InvokeBody::Raw(bytes) = request.body() else {
        return Err(IpcError::invalid(
            "Expected the subtitle file as raw bytes.",
        ));
    };
    if bytes.len() > MAX_SUBTITLE_BYTES {
        return Err(IpcError::invalid(
            "This subtitle file is larger than 5 MiB.",
        ));
    }
    parse_off_thread(bytes.clone()).await
}

fn is_subtitle_file(path: &str) -> bool {
    path.rsplit_once('.').is_some_and(|(_, ext)| {
        lumen_media::SUBTITLE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
    })
}

#[cfg(test)]
mod tests {
    use lumen_media::{Container, SubtitleCodec};

    use super::*;

    #[test]
    fn parses_into_plain_cues() {
        let cues = cues_dto(b"1\n00:00:01,000 --> 00:00:02,000\n<b>Hi</b>\n").unwrap();
        assert_eq!(cues.format, "srt");
        assert_eq!(
            cues.cues,
            [CueDto {
                start_ms: 1000,
                end_ms: 2000,
                text: "Hi".to_owned(),
            }]
        );
        let error = cues_dto(b"not subtitles").unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidArgument);
        assert_eq!(
            error.message,
            "This is not an SRT, WebVTT, ASS, or SSA subtitle file."
        );
        let json = serde_json::to_value(&cues).unwrap();
        assert_eq!(json["cues"][0]["startMs"], 1000);
    }

    #[test]
    fn labels_embedded_tracks() {
        let track = |codec, language: Option<&str>, name: Option<&str>| SubtitleTrack {
            codec,
            language: language.map(str::to_owned),
            name: name.map(str::to_owned),
            default: false,
            forced: false,
        };
        let report = TrackReport {
            container: Container::Matroska,
            audio: Vec::new(),
            subtitles: vec![
                track(SubtitleCodec::Text, Some("fre"), None),
                track(SubtitleCodec::Image, Some("xyz"), Some("Commentary")),
                track(SubtitleCodec::Ass, None, None),
            ],
        };
        let got: Vec<(String, Option<String>, bool)> = embedded(&report)
            .into_iter()
            .map(|e| (e.label, e.language, e.text))
            .collect();
        assert_eq!(
            got,
            [
                ("French".to_owned(), Some("fr".to_owned()), true),
                ("Commentary".to_owned(), Some("xyz".to_owned()), false),
                ("Track 3".to_owned(), None, true),
            ]
        );
        assert!(is_subtitle_file("a/b.SRT") && !is_subtitle_file("a/b.sub"));
    }
}
