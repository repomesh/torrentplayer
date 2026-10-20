// SPDX-License-Identifier: MIT OR Apache-2.0

//! What a container header says about its tracks.

use crate::audio::AudioTrack;

/// Most subtitle tracks recorded per file; more are ignored.
pub(crate) const MAX_SUBTITLE_TRACKS: usize = 64;
/// Longest track name kept, in characters.
const MAX_NAME_CHARS: usize = 100;
/// Longest language tag kept (`zh-Hant-TW` is 10).
const MAX_LANGUAGE_CHARS: usize = 16;

/// The container format that was probed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Container {
    /// ISO base media (MP4, M4V) or `QuickTime` (MOV).
    Mp4,
    /// Matroska or `WebM`.
    Matroska,
}

/// The format of an embedded subtitle track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SubtitleCodec {
    /// Plain text: Matroska `S_TEXT/UTF8` (SRT), MP4 `tx3g`.
    Text,
    /// `WebVTT`: Matroska `S_TEXT/WEBVTT`, MP4 `wvtt`.
    WebVtt,
    /// ASS or SSA: Matroska `S_TEXT/ASS`, `S_TEXT/SSA`.
    Ass,
    /// TTML: MP4 `stpp`.
    Ttml,
    /// CEA-608/708 closed captions: MP4 `c608`, `c708`.
    ClosedCaptions,
    /// Pictures of text: PGS (Blu-ray), `VobSub` (DVD), DVB.
    Image,
    /// Anything else.
    Other,
}

impl SubtitleCodec {
    /// Stable lowercase identifier: `text`, `webvtt`, `ass`, `ttml`, `cc`, `image`, `other`.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::WebVtt => "webvtt",
            Self::Ass => "ass",
            Self::Ttml => "ttml",
            Self::ClosedCaptions => "cc",
            Self::Image => "image",
            Self::Other => "other",
        }
    }

    /// Whether the track holds text (as opposed to pictures of text).
    #[must_use]
    pub const fn is_text(self) -> bool {
        !matches!(self, Self::Image | Self::Other)
    }
}

/// One embedded subtitle track.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SubtitleTrack {
    /// Its format.
    pub codec: SubtitleCodec,
    /// Language tag as the container gives it (ISO 639-2 like `eng`, or BCP 47), if any. Only
    /// ASCII letters, digits, and `-`.
    pub language: Option<String>,
    /// The track's name, if any: plain text, at most 100 characters.
    pub name: Option<String>,
    /// Marked default.
    pub default: bool,
    /// Marked forced (only for foreign-language parts).
    pub forced: bool,
}

/// The tracks found in a file's container header.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TrackReport {
    /// The container format.
    pub container: Container,
    /// Enabled audio tracks in container order (at most 32). Empty if the file has no audio.
    pub audio: Vec<AudioTrack>,
    /// Enabled subtitle tracks in container order (at most 64).
    pub subtitles: Vec<SubtitleTrack>,
}

impl TrackReport {
    /// The audio track a player picks: the first one marked default, otherwise the first one.
    #[must_use]
    pub fn primary(&self) -> Option<&AudioTrack> {
        self.audio
            .iter()
            .find(|t| t.default)
            .or_else(|| self.audio.first())
    }

    /// When the [`primary`](Self::primary) track is one the built-in player cannot decode,
    /// another audio track it can decode, if there is one.
    #[must_use]
    pub fn playable_alternative(&self) -> Option<&AudioTrack> {
        let primary = self.primary()?;
        if primary.codec.webview_decodes() != Some(false) {
            return None;
        }
        self.audio
            .iter()
            .find(|t| t.codec.webview_decodes() == Some(true))
    }
}

/// A language tag from the container, kept only if it is a plausible tag (`und` is dropped).
pub(crate) fn language_tag(raw: &[u8]) -> Option<String> {
    let tag = std::str::from_utf8(raw).ok()?.trim_end_matches('\0').trim();
    let plausible = !tag.is_empty()
        && tag.len() <= MAX_LANGUAGE_CHARS
        && tag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !tag.eq_ignore_ascii_case("und");
    plausible.then(|| tag.to_owned())
}

/// A track name from the container as plain text: control characters dropped, whitespace
/// collapsed, at most 100 characters.
pub(crate) fn track_name(raw: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(raw);
    let mut name = String::new();
    for word in text
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|w| !w.is_empty())
    {
        if !name.is_empty() {
            name.push(' ');
        }
        name.push_str(word);
    }
    let name: String = name.chars().take(MAX_NAME_CHARS).collect();
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::AudioCodec;

    fn track(codec: AudioCodec, default: bool) -> AudioTrack {
        AudioTrack { codec, default }
    }

    fn report(audio: Vec<AudioTrack>) -> TrackReport {
        TrackReport {
            container: Container::Matroska,
            audio,
            subtitles: Vec::new(),
        }
    }

    #[test]
    fn primary_is_the_first_default_track() {
        let r = report(vec![
            track(AudioCodec::Aac, false),
            track(AudioCodec::Ac3, true),
            track(AudioCodec::Dts, true),
        ]);
        assert_eq!(r.primary().map(|t| t.codec), Some(AudioCodec::Ac3));
        assert_eq!(
            r.playable_alternative().map(|t| t.codec),
            Some(AudioCodec::Aac)
        );
    }

    #[test]
    fn primary_falls_back_to_the_first_track() {
        let r = report(vec![
            track(AudioCodec::Dts, false),
            track(AudioCodec::Ac3, false),
        ]);
        assert_eq!(r.primary().map(|t| t.codec), Some(AudioCodec::Dts));
        assert_eq!(r.playable_alternative(), None, "no web codec");
        let silent = report(Vec::new());
        assert_eq!(silent.primary(), None);
        assert_eq!(silent.playable_alternative(), None);
    }

    #[test]
    fn no_alternative_when_the_primary_track_plays() {
        let r = report(vec![
            track(AudioCodec::Aac, true),
            track(AudioCodec::Opus, false),
        ]);
        assert_eq!(r.playable_alternative(), None);
    }

    #[test]
    fn cleans_language_tags_and_names() {
        assert_eq!(language_tag(b"eng"), Some("eng".to_owned()));
        assert_eq!(language_tag(b"pt-BR\0"), Some("pt-BR".to_owned()));
        assert_eq!(language_tag(b"und"), None);
        assert_eq!(language_tag(b"<script>"), None);
        assert_eq!(language_tag(&[0xFF, 0xFE]), None);
        assert_eq!(language_tag(b"averyveryverylongtag"), None);
        assert_eq!(
            track_name(b"  English\n(SDH)\x07 "),
            Some("English (SDH)".to_owned())
        );
        assert_eq!(track_name(b"\0\0"), None);
        assert_eq!(track_name(&[b'a'; 500]).map(|n| n.len()), Some(100));
        assert_eq!(SubtitleCodec::Image.id(), "image");
        assert!(SubtitleCodec::Ass.is_text() && !SubtitleCodec::Image.is_text());
    }
}
