// SPDX-License-Identifier: MIT OR Apache-2.0

//! What the benchmark knows about a local media file: its container, codecs, duration, and
//! layout, from TorrentPlayer's own container parsers (`torrentplayer-media`).

use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use serde::Serialize;
use torrentplayer_media::{Container, MediaLayout, probe_layout, probe_tracks};

const MIB: f64 = 1024.0 * 1024.0;

/// A local media file as the benchmark sees it.
#[derive(Debug, Clone, Serialize)]
pub struct MediaInfo {
    /// File size in bytes.
    pub size: u64,
    /// `MP4` or `Matroska`; `None` if the container could not be parsed.
    pub container: Option<String>,
    /// The first video track's codec.
    pub video_codec: Option<String>,
    /// Audio codecs, in track order.
    pub audio_codecs: Vec<String>,
    /// Duration from the container header, in seconds.
    pub duration_secs: Option<f64>,
    /// Average bitrate (size over duration), bits per second.
    pub bitrate_bps: Option<u64>,
    /// The index (MP4 `moov`, Matroska `Cues`) comes after the media.
    pub index_at_end: Option<bool>,
    /// Bytes of container header a player reads before the media.
    pub header_bytes: Option<u64>,
    /// Keyframes found (MP4 sync samples or Matroska cue points).
    pub keyframes: Option<usize>,
    /// What the benchmark could not use, and what it does instead.
    pub warnings: Vec<String>,
    /// The layout, for container-aware start-up and seeks.
    #[serde(skip)]
    pub layout: Option<Arc<MediaLayout>>,
}

impl MediaInfo {
    /// Reads the container header, codecs, and layout of the file at `path`.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be opened. Container problems become warnings.
    pub async fn inspect(path: &Path) -> anyhow::Result<Self> {
        let mut file = tokio::fs::File::open(path)
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        let size = file.metadata().await?.len();
        let mut warnings = Vec::new();
        let tracks = match probe_tracks(&mut file, size).await {
            Ok(tracks) => Some(tracks),
            Err(error) => {
                warnings.push(format!("audio tracks not read: {error}"));
                None
            }
        };
        let layout = match probe_layout(&mut file, size).await {
            Ok(layout) => Some(layout),
            Err(error) => {
                warnings.push(format!(
                    "container layout not parsed ({error}): start-up and seeks use the flat \
                     --startup-mib and --seek-mib"
                ));
                None
            }
        };
        if let Some(layout) = &layout
            && layout.seek_points.is_empty()
        {
            warnings.push("no keyframe index (MP4 sync samples or Matroska Cues): seeks use the flat --seek-mib".into());
        }
        let duration_ms = layout
            .as_ref()
            .and_then(|l| l.duration_ms)
            .or_else(|| tracks.as_ref().and_then(|t| t.duration_ms));
        if duration_ms.is_none() {
            warnings.push(
                "no duration in the container header: pass --duration for read-ahead sizing".into(),
            );
        }
        // Durations are far below 2^53 ms.
        #[allow(clippy::cast_precision_loss)]
        let duration_secs = duration_ms.map(|ms| ms as f64 / 1000.0);
        let container = layout
            .as_ref()
            .map(|l| l.container)
            .or_else(|| tracks.as_ref().map(|t| t.container))
            .map(|c| match c {
                Container::Mp4 => "MP4".to_owned(),
                Container::Matroska => "Matroska".to_owned(),
            });
        Ok(Self {
            size,
            container,
            video_codec: layout.as_ref().and_then(|l| l.video_codec.clone()),
            audio_codecs: tracks
                .iter()
                .flat_map(|t| &t.audio)
                .map(|a| a.codec.label().to_owned())
                .collect(),
            duration_secs,
            bitrate_bps: duration_ms
                .filter(|ms| *ms > 0)
                .map(|ms| size.saturating_mul(8000) / ms),
            index_at_end: layout.as_ref().map(|l| l.index_at_end),
            header_bytes: layout
                .as_ref()
                .map(|l| l.header.iter().map(|r| r.end - r.start).sum()),
            keyframes: layout.as_ref().map(|l| l.seek_points.len()),
            warnings,
            layout: layout.map(Arc::new),
        })
    }

    /// One line for reports: container, codecs, duration, bitrate, size, and index position.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut parts = vec![
            self.container
                .clone()
                .unwrap_or_else(|| "unknown container".into()),
        ];
        let mut codecs: Vec<String> = self.video_codec.iter().cloned().collect();
        codecs.extend(self.audio_codecs.iter().cloned());
        if !codecs.is_empty() {
            parts.push(codecs.join(" + "));
        }
        if let Some(secs) = self.duration_secs {
            parts.push(format!("{secs:.1} s"));
        }
        if let Some(bps) = self.bitrate_bps {
            #[allow(clippy::cast_precision_loss)]
            parts.push(format!("{:.2} Mbit/s", bps as f64 / 1e6));
        }
        #[allow(clippy::cast_precision_loss)]
        parts.push(format!("{:.1} MiB", self.size as f64 / MIB));
        if let Some(at_end) = self.index_at_end {
            let index = match (self.container.as_deref(), at_end) {
                (Some("MP4"), true) => "moov at end",
                (Some("MP4"), false) => "moov at start (faststart)",
                (_, true) => "Cues after the clusters",
                (_, false) if self.keyframes.unwrap_or(0) > 0 => "Cues before the clusters",
                (_, false) => "no Cues",
            };
            parts.push(index.to_owned());
        }
        if let Some(header) = self.header_bytes {
            #[allow(clippy::cast_precision_loss)]
            parts.push(format!("header {:.0} KiB", header as f64 / 1024.0));
        }
        if let Some(n) = self.keyframes {
            parts.push(format!("{n} keyframes"));
        }
        parts.join(", ")
    }
}
