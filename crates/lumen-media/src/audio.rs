// SPDX-License-Identifier: MIT OR Apache-2.0

//! Audio codecs and what the built-in player can decode.

/// Most audio tracks recorded per file; more are ignored.
pub(crate) const MAX_AUDIO_TRACKS: usize = 32;

/// An audio codec, as named by the container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioCodec {
    /// AAC (MPEG-4 or MPEG-2).
    Aac,
    /// MPEG-1/2 Audio Layer III.
    Mp3,
    /// MPEG-1/2 Audio Layer I or II.
    Mp2,
    /// Opus.
    Opus,
    /// Vorbis.
    Vorbis,
    /// FLAC.
    Flac,
    /// Uncompressed PCM.
    Pcm,
    /// Apple Lossless.
    Alac,
    /// Dolby Digital (AC-3).
    Ac3,
    /// Dolby Digital Plus (E-AC-3), which also carries Dolby Atmos in streaming releases.
    Eac3,
    /// Dolby AC-4.
    Ac4,
    /// Dolby `TrueHD` (MLP), which also carries Dolby Atmos on Blu-ray.
    TrueHd,
    /// DTS in any variant (core, DTS-HD, DTS Express, DTS:X).
    Dts,
    /// A codec the probe does not recognize, or encrypted audio.
    Other,
}

impl AudioCodec {
    /// Stable lowercase identifier, for logs and the IPC payload: `aac`, `ac3`, `dts`, ...
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Aac => "aac",
            Self::Mp3 => "mp3",
            Self::Mp2 => "mp2",
            Self::Opus => "opus",
            Self::Vorbis => "vorbis",
            Self::Flac => "flac",
            Self::Pcm => "pcm",
            Self::Alac => "alac",
            Self::Ac3 => "ac3",
            Self::Eac3 => "eac3",
            Self::Ac4 => "ac4",
            Self::TrueHd => "truehd",
            Self::Dts => "dts",
            Self::Other => "other",
        }
    }

    /// Name for display, for example `Dolby Digital (AC-3)`.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Aac => "AAC",
            Self::Mp3 => "MP3",
            Self::Mp2 => "MPEG Layer II",
            Self::Opus => "Opus",
            Self::Vorbis => "Vorbis",
            Self::Flac => "FLAC",
            Self::Pcm => "PCM",
            Self::Alac => "Apple Lossless (ALAC)",
            Self::Ac3 => "Dolby Digital (AC-3)",
            Self::Eac3 => "Dolby Digital Plus (E-AC-3)",
            Self::Ac4 => "Dolby AC-4",
            Self::TrueHd => "Dolby TrueHD",
            Self::Dts => "DTS",
            Self::Other => "an unrecognized format",
        }
    }

    /// Whether the built-in (Chromium-based webview) player decodes it: `Some(true)` for the web
    /// codecs, `Some(false)` for Dolby and DTS audio, and `None` where builds differ or the codec
    /// is unknown (the player then checks its decoder counters instead).
    #[must_use]
    pub const fn webview_decodes(self) -> Option<bool> {
        match self {
            Self::Aac | Self::Mp3 | Self::Opus | Self::Vorbis | Self::Flac => Some(true),
            Self::Ac3 | Self::Eac3 | Self::Ac4 | Self::TrueHd | Self::Dts => Some(false),
            Self::Mp2 | Self::Pcm | Self::Alac | Self::Other => None,
        }
    }
}

/// One audio track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AudioTrack {
    /// Its codec.
    pub codec: AudioCodec,
    /// Whether the container marks it for playback by default: Matroska `FlagDefault`, or the
    /// MP4 track header's enabled flag.
    pub default: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodability_table() {
        for codec in [
            AudioCodec::Ac3,
            AudioCodec::Eac3,
            AudioCodec::TrueHd,
            AudioCodec::Dts,
        ] {
            assert_eq!(codec.webview_decodes(), Some(false), "{codec:?}");
        }
        for codec in [
            AudioCodec::Aac,
            AudioCodec::Mp3,
            AudioCodec::Opus,
            AudioCodec::Vorbis,
            AudioCodec::Flac,
        ] {
            assert_eq!(codec.webview_decodes(), Some(true), "{codec:?}");
        }
        assert_eq!(AudioCodec::Other.webview_decodes(), None);
        assert_eq!(AudioCodec::Ac3.label(), "Dolby Digital (AC-3)");
        assert_eq!(AudioCodec::Eac3.id(), "eac3");
    }
}
