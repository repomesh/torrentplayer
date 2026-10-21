// SPDX-License-Identifier: MIT OR Apache-2.0

//! Which audio codec a stream uses, so the player can say when the built-in player will play
//! it without sound (Dolby and DTS audio). Each stream is probed once, when it starts, by
//! reading its container header through the stream's own media source (see `lumen_media`).

use std::sync::Arc;
use std::time::Duration;

use lumen_media::{AudioTrack, Container, TrackReport};
use lumen_stream::{MediaSource, StreamId};
use serde::Serialize;
use tauri::State;
use tokio::sync::watch;
use ts_rs::TS;

use crate::core::AppState;
use crate::error::{ErrorKind, IpcError};
use crate::subtitles::{EmbeddedSubtitleDto, embedded};

/// How long a probe may wait for the container header to download.
const PROBE_TIMEOUT: Duration = Duration::from_secs(45);

/// One audio track.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct AudioTrackDto {
    /// Codec identifier: `aac`, `mp3`, `opus`, `vorbis`, `flac`, `ac3`, `eac3`, `truehd`,
    /// `dts`, `ac4`, `mp2`, `pcm`, `alac`, or `other`.
    pub codec: String,
    /// Codec name for display, for example `Dolby Digital (AC-3)`.
    pub label: String,
    /// Whether the built-in player decodes it; `null` if that varies or the codec is unknown.
    pub decodes: Option<bool>,
}

impl From<&AudioTrack> for AudioTrackDto {
    fn from(track: &AudioTrack) -> Self {
        Self {
            codec: track.codec.id().to_owned(),
            label: track.codec.label().to_owned(),
            decodes: track.codec.webview_decodes(),
        }
    }
}

/// What a stream's container header says about its audio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub(crate) struct StreamAudio {
    /// Whether the header was read. False if the container is not MP4, MOV, Matroska, or `WebM`,
    /// is malformed, or did not download in time; the other fields are then empty.
    pub probed: bool,
    /// Number of audio tracks (0: the video has no sound).
    pub track_count: u32,
    /// The track the player plays: the one marked default, otherwise the first.
    pub primary: Option<AudioTrackDto>,
    /// When the built-in player cannot decode `primary`, another track it can decode.
    pub alternative: Option<AudioTrackDto>,
    /// Whether `primary` can be converted to a format the built-in player decodes (with
    /// `convert_audio`): the player cannot decode it, and this build has the converter.
    pub convertible: bool,
}

impl StreamAudio {
    /// The answer when the header could not be read.
    fn unknown() -> Self {
        Self {
            probed: false,
            track_count: 0,
            primary: None,
            alternative: None,
            convertible: false,
        }
    }
}

impl From<&TrackReport> for StreamAudio {
    fn from(report: &TrackReport) -> Self {
        Self {
            probed: true,
            track_count: u32::try_from(report.audio.len()).unwrap_or(u32::MAX),
            primary: report.primary().map(AudioTrackDto::from),
            alternative: report.playable_alternative().map(AudioTrackDto::from),
            convertible: false,
        }
    }
}

/// What the probe found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Probed {
    pub(crate) audio: StreamAudio,
    /// FFmpeg's stream specifier for the primary audio track, for the audio conversion.
    pub(crate) primary_stream: Option<String>,
    pub(crate) subtitles: Vec<EmbeddedSubtitleDto>,
    /// The duration in the header, in milliseconds, if it gives one.
    pub(crate) duration_ms: Option<u64>,
}

/// A stream's probe of its container's tracks, running or finished. Dropping it (when the
/// stream stops) cancels it, so its reader stops prioritizing the header.
pub(crate) struct TrackProbe {
    result: watch::Receiver<Option<Probed>>,
    task: tauri::async_runtime::JoinHandle<()>,
}

impl TrackProbe {
    /// Starts probing a stream's media.
    pub(crate) fn start(stream: StreamId, media: Arc<dyn MediaSource>) -> Self {
        let (sender, result) = watch::channel(None);
        let task = tauri::async_runtime::spawn(async move {
            sender.send_replace(Some(probe(stream, media.as_ref()).await));
        });
        Self { result, task }
    }

    /// The result, once there is one.
    pub(crate) fn subscribe(&self) -> watch::Receiver<Option<Probed>> {
        self.result.clone()
    }

    /// The embedded subtitle tracks, or `None` while the probe is still running.
    pub(crate) fn embedded_subtitles(&self) -> Option<Vec<EmbeddedSubtitleDto>> {
        self.result.borrow().as_ref().map(|p| p.subtitles.clone())
    }
}

impl Drop for TrackProbe {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Reads the container header of `media`, within [`PROBE_TIMEOUT`].
async fn probe(stream: StreamId, media: &dyn MediaSource) -> Probed {
    let len = media.byte_len();
    let outcome = tokio::time::timeout(PROBE_TIMEOUT, async {
        let mut reader = media.open().await?;
        lumen_media::probe_tracks(&mut reader, len).await
    })
    .await;
    let unknown = Probed {
        audio: StreamAudio::unknown(),
        primary_stream: None,
        subtitles: Vec::new(),
        duration_ms: None,
    };
    match outcome {
        Ok(Ok(report)) => {
            let audio = StreamAudio::from(&report);
            tracing::info!(
                stream,
                tracks = audio.track_count,
                codec = audio.primary.as_ref().map_or("none", |t| t.codec.as_str()),
                alternative = audio.alternative.as_ref().map(|t| t.codec.as_str()),
                subtitles = report.subtitles.len(),
                "audio probed"
            );
            Probed {
                audio,
                primary_stream: report
                    .primary()
                    .map(|t| stream_specifier(report.container, t)),
                subtitles: embedded(&report),
                duration_ms: report.duration_ms,
            }
        }
        Ok(Err(error)) => {
            tracing::info!(stream, %error, "could not probe the audio");
            unknown
        }
        Err(_) => {
            tracing::info!(
                stream,
                "the container header did not arrive in time to probe the audio"
            );
            unknown
        }
    }
}

/// FFmpeg's `-map` specifier for `track`: by track ID in MP4, whose demuxer exposes it, and by
/// position in Matroska, whose demuxer does not.
fn stream_specifier(container: Container, track: &AudioTrack) -> String {
    match (container, track.id) {
        (Container::Mp4, Some(id)) => format!("0:i:{id}"),
        _ => format!("0:a:{}", track.position),
    }
}

/// Returns what a stream's container header says about its audio, waiting for the probe if it
/// is still running (at most 45 s after the stream started).
#[tauri::command]
pub(crate) async fn stream_audio(
    state: State<'_, AppState>,
    stream_id: StreamId,
) -> Result<StreamAudio, IpcError> {
    let core = state.core().await?;
    let mut result = core.with_stream(stream_id, |s| s.tracks.subscribe())?;
    // The probe always finishes; the sender goes away early only if the stream is stopped.
    let probed = result
        .wait_for(Option::is_some)
        .await
        .map_err(|_| IpcError::new(ErrorKind::NotFound, "Unknown stream."))?
        .clone();
    let mut audio = probed.map_or_else(StreamAudio::unknown, |p| p.audio);
    audio.convertible = audio
        .primary
        .as_ref()
        .is_some_and(|t| crate::convert::supported_codec(&t.codec))
        && crate::convert::sidecar().is_some();
    Ok(audio)
}

#[cfg(test)]
mod tests {
    use std::io::{self, Cursor};
    use std::pin::Pin;

    use lumen_media::{AudioCodec, Container};
    use lumen_stream::MediaReader;

    use super::*;

    struct Memory(Vec<u8>);

    type Opened = Pin<Box<dyn Future<Output = io::Result<Box<dyn MediaReader>>> + Send>>;

    impl MediaSource for Memory {
        fn byte_len(&self) -> u64 {
            self.0.len() as u64
        }

        fn content_type(&self) -> &'static str {
            "video/x-matroska"
        }

        fn open(&self) -> Opened {
            let bytes = self.0.clone();
            Box::pin(async move { Ok(Box::new(Cursor::new(bytes)) as Box<dyn MediaReader>) })
        }
    }

    #[test]
    fn reports_primary_and_playable_alternative() {
        let report = TrackReport {
            duration_ms: None,
            container: Container::Matroska,
            subtitles: Vec::new(),
            audio: vec![
                AudioTrack {
                    codec: AudioCodec::Eac3,
                    default: true,
                    id: Some(1),
                    position: 0,
                },
                AudioTrack {
                    codec: AudioCodec::Aac,
                    default: false,
                    id: Some(2),
                    position: 1,
                },
            ],
        };
        let audio = StreamAudio::from(&report);
        assert!(audio.probed);
        assert_eq!(audio.track_count, 2);
        let primary = audio.primary.as_ref().unwrap();
        assert_eq!(primary.codec, "eac3");
        assert_eq!(primary.label, "Dolby Digital Plus (E-AC-3)");
        assert_eq!(primary.decodes, Some(false));
        assert_eq!(audio.alternative.as_ref().unwrap().codec, "aac");

        let json = serde_json::to_value(&audio).unwrap();
        let mut keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "alternative",
                "convertible",
                "primary",
                "probed",
                "trackCount"
            ]
        );
    }

    #[tokio::test]
    async fn probes_a_media_source() {
        // A minimal Matroska file: EBML header, then a Segment with one AC-3 track, number 2.
        let codec = [&[0x86, 0x85][..], b"A_AC3"].concat();
        let fields = [&[0xD7, 0x81, 0x02, 0x83, 0x81, 0x02][..], &codec].concat();
        let entry = [&[0xAE, 0x80 | 0x0D][..], &fields].concat();
        let tracks = [&[0x16, 0x54, 0xAE, 0x6B, 0x80 | 0x0F][..], &entry].concat();
        let segment = [&[0x18, 0x53, 0x80, 0x67, 0xFF][..], &tracks].concat();
        let file = [&[0x1A, 0x45, 0xDF, 0xA3, 0x80][..], &segment].concat();
        let probed = probe(1, &Memory(file)).await;
        assert!(probed.audio.probed);
        assert_eq!(probed.audio.primary.unwrap().codec, "ac3");
        assert_eq!(probed.primary_stream.as_deref(), Some("0:a:0"));
        assert_eq!(probed.subtitles, []);

        let unknown = probe(2, &Memory(b"RIFF....AVI ".to_vec())).await;
        assert_eq!(unknown.audio, StreamAudio::unknown());
    }
}
